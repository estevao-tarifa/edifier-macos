//! The Tokio side of the app: discovery, the BLE session, reconnection with backoff, and the
//! protocol traffic behind every menu item.
//!
//! The AppKit thread never talks to the headset. It pushes an [`Action`] into a bounded channel and
//! reads the [`SharedState`] snapshot; everything here runs on a background runtime.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use edifier_link::ble::{self, Discovered, Target};
use edifier_link::{DEFAULT_TIMEOUT, LinkError, Session};
use edifier_proto::event::ShutdownTimer;
use edifier_proto::{AmbientVolume, Command, DeviceModel, Event, Status};
use tokio::sync::{broadcast, mpsc};
use tracing::{debug, info, warn};

use crate::config::Config;
use crate::menu::{Action, Connection, SharedState, UiState};

/// How long one discovery pass runs before it is treated as "not in range".
const SCAN_WINDOW: Duration = Duration::from_secs(5);
/// How many candidates one discovery pass collects before it stops early.
const SCAN_CANDIDATES: usize = 4;
/// Delay before the first reconnection attempt; doubles on every further failure.
const INITIAL_BACKOFF: Duration = Duration::from_secs(2);
/// Upper bound of the reconnection backoff.
const MAX_BACKOFF: Duration = Duration::from_secs(60);
/// Pause after the user drops or restarts the link on purpose.
const MANUAL_RECONNECT: Duration = Duration::from_secs(10);
/// How often the battery level is polled while a session is live.
const BATTERY_POLL: Duration = Duration::from_secs(300);
/// How long a setter waits for its confirmation before being reported as "written, unconfirmed".
const SETTLE_TIMEOUT: Duration = Duration::from_millis(1500);

/// Something that can ask the AppKit thread to redraw the status item.
pub trait Waker: Send + Sync + 'static {
    /// Redraw as soon as the main thread is free.
    fn wake(&self);
}

/// Shared plumbing for the BLE runtime: the snapshot every redraw reads, the waker that nudges
/// AppKit, and where the settings live.
#[derive(Clone)]
pub struct Ctx {
    state: SharedState,
    waker: Arc<dyn Waker>,
    settings_path: PathBuf,
}

impl Ctx {
    /// Build the runtime context.
    #[must_use]
    pub fn new(state: SharedState, waker: Arc<dyn Waker>, settings_path: PathBuf) -> Self {
        Self {
            state,
            waker,
            settings_path,
        }
    }

    /// Copy of the current snapshot.
    #[must_use]
    pub fn snapshot(&self) -> UiState {
        self.state.snapshot()
    }

    /// Publish a new connection state and redraw.
    fn set_connection(&self, connection: Connection) {
        self.state.update(|state| state.connection = connection);
        self.wake();
    }

    /// Fold a device notification into the snapshot and redraw.
    fn fold(&self, event: &Event) {
        self.state.update(|state| state.apply(event));
        self.wake();
    }

    /// Merge a fresh status snapshot and redraw.
    fn merge(&self, status: Status) {
        self.state.update(|state| state.merge_status(status));
        self.wake();
    }

    /// Surface a failed action in the menu.
    fn report(&self, err: &anyhow::Error) {
        let message = format!("{err:#}");
        warn!(error = %message, "menu action failed");
        self.state.update(|state| state.error = Some(message));
        self.wake();
    }

    /// Forget the last failure, once something worked again.
    fn clear_error(&self) {
        self.state.update(|state| state.error = None);
    }

    /// Write the remembered device to disk.
    fn persist(&self, settings: &Config) -> Result<()> {
        settings
            .save_to(&self.settings_path)
            .with_context(|| format!("cannot save {}", self.settings_path.display()))
    }

    /// Nudge the main thread.
    fn wake(&self) {
        self.waker.wake();
    }
}

/// Find the headset, serve it, reconnect with backoff — until the request channel closes.
///
/// Runs on the BLE runtime thread; every exit path leaves the snapshot in a state the menu can
/// render, so the app never claims to be connected while the link is down.
pub async fn run(ctx: Ctx, mut settings: Config, mut requests: mpsc::Receiver<Action>) {
    let mut backoff = INITIAL_BACKOFF;
    loop {
        ctx.set_connection(Connection::Scanning);
        let outcome = match connect(&ctx, &mut settings).await {
            Ok(session) => {
                backoff = INITIAL_BACKOFF;
                serve(&ctx, &mut settings, &mut requests, session).await
            }
            Err(err) => {
                let reason = format!("{err:#}");
                warn!(error = %reason, "cannot reach the headset");
                Outcome::Failed { reason }
            }
        };

        let (delay, reason) = match outcome {
            Outcome::Failed { reason } => {
                let delay = backoff;
                backoff = (backoff * 2).min(MAX_BACKOFF);
                (delay, reason)
            }
            Outcome::Retry => {
                let delay = backoff;
                backoff = (backoff * 2).min(MAX_BACKOFF);
                (delay, "the control link dropped".to_string())
            }
            Outcome::Reconnect => {
                backoff = INITIAL_BACKOFF;
                (MANUAL_RECONNECT, "restarting the control link".to_string())
            }
            Outcome::Quit => break,
        };

        ctx.set_connection(Connection::Waiting {
            seconds: delay.as_secs(),
            reason,
        });
        if !wait_for_retry(&ctx, &mut settings, &mut requests, delay).await {
            break;
        }
    }
    info!("ble runtime stopped");
}

/// Why a serving loop ended.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Outcome {
    /// The link dropped on its own; reconnect after the backoff.
    Retry,
    /// The user asked for the link to be restarted; reconnect after a visible pause.
    Reconnect,
    /// The headset could not be reached at all.
    Failed {
        /// Why the attempt failed.
        reason: String,
    },
    /// The app is quitting.
    Quit,
}

/// Apply a `--device` selector to the remembered settings.
///
/// A 36-character UUID is taken as a CoreBluetooth identifier, anything else as a name fragment.
pub fn apply_selector(settings: &mut Config, selector: &str) {
    match Target::from_selector(selector) {
        Target::Identifier(identifier) => settings.device_identifier = Some(identifier),
        Target::Name(name) => settings.device_name = Some(name),
        Target::Auto => {}
    }
}

/// Endpoints macOS already holds a BLE link to, presented like scan results.
///
/// A connected peripheral stops advertising, so a scan cannot see the normal macOS case; asking the
/// system returns it immediately.
#[cfg(target_os = "macos")]
async fn system_endpoints() -> Vec<Discovered> {
    match edifier_link::system_ble::edifier_endpoints(Duration::from_secs(10)).await {
        Ok(endpoints) => endpoints
            .into_iter()
            .map(|endpoint| Discovered {
                model: endpoint
                    .name
                    .as_deref()
                    .map_or(DeviceModel::Generic, DeviceModel::from_name),
                identifier: endpoint.identifier,
                name: endpoint.name,
                rssi: None,
                service: None,
            })
            .collect(),
        Err(err) => {
            debug!(error = %err, "no system-connected endpoint");
            Vec::new()
        }
    }
}

#[cfg(not(target_os = "macos"))]
async fn system_endpoints() -> Vec<Discovered> {
    Vec::new()
}

/// Find the headset and open a control session on it.
async fn connect(ctx: &Ctx, settings: &mut Config) -> Result<Session> {
    // Prefer the endpoint the system already connected; only scan when there is none.
    let mut found = system_endpoints().await;
    if found.is_empty() {
        found = ble::scan(SCAN_WINDOW, SCAN_CANDIDATES)
            .await
            .context("bluetooth scan failed")?;
    }
    let index = select_endpoint(
        settings.device_identifier.as_deref(),
        settings.device_name.as_deref(),
        &found,
    )
    .ok_or_else(|| anyhow!("no Edifier endpoint found"))?;
    let candidate = found.get(index).context("the scanned endpoint vanished")?;

    info!(
        device = candidate.name.as_deref().unwrap_or("unknown"),
        id = %candidate.identifier,
        model = %candidate.model.display_name(),
        "found the headset",
    );
    if settings.remember(&candidate.identifier, candidate.name.as_deref())
        && let Err(err) = ctx.persist(settings)
    {
        warn!(error = %format!("{err:#}"), "cannot remember the headset");
    }
    ctx.state
        .update(|state| state.model = Some(candidate.model));

    ctx.set_connection(Connection::Connecting);
    let session = Session::connect_ble(Target::Identifier(candidate.identifier.clone()))
        .await
        .context("cannot open the control link")?;
    ctx.set_connection(Connection::Connected {
        description: session.describe().to_string(),
    });
    info!(link = session.describe(), "control link established");
    Ok(session)
}

/// Pick which discovered endpoint to use: the remembered identifier first, then the remembered
/// name, and only then any Edifier endpoint that answered. A headset that was re-paired gets a new
/// CoreBluetooth identifier, so the remembered name is the fallback for that case.
#[must_use]
pub fn select_endpoint(
    identifier: Option<&str>,
    name: Option<&str>,
    found: &[Discovered],
) -> Option<usize> {
    if let Some(identifier) = identifier
        && let Some(index) = found
            .iter()
            .position(|candidate| candidate.identifier == identifier)
    {
        return Some(index);
    }
    if let Some(name) = name {
        let wanted = name.to_lowercase();
        if let Some(index) = found.iter().position(|candidate| {
            candidate
                .name
                .as_deref()
                .is_some_and(|found| found.to_lowercase().contains(&wanted))
        }) {
            return Some(index);
        }
    }
    if found.is_empty() { None } else { Some(0) }
}

/// Service one live session until it drops or the user quits.
async fn serve(
    ctx: &Ctx,
    settings: &mut Config,
    requests: &mut mpsc::Receiver<Action>,
    session: Session,
) -> Outcome {
    let mut events = session.subscribe();

    let status = session.refresh().await;
    ctx.merge(status);
    debug!("initial status refresh done");

    let mut battery =
        tokio::time::interval_at(tokio::time::Instant::now() + BATTERY_POLL, BATTERY_POLL);

    let outcome = 'serve: loop {
        if !session.is_alive() {
            break Outcome::Retry;
        }
        tokio::select! {
            event = events.recv() => match event {
                Ok(event) => ctx.fold(&event),
                Err(broadcast::error::RecvError::Lagged(skipped)) => {
                    warn!(skipped, "device events were dropped");
                }
                Err(broadcast::error::RecvError::Closed) => break Outcome::Retry,
            },
            request = requests.recv() => match request {
                None => break Outcome::Quit,
                Some(action) => match perform(ctx, settings, &session, action).await {
                    Ok(FollowUp::Keep) => ctx.clear_error(),
                    Ok(FollowUp::Reconnect) => break 'serve Outcome::Reconnect,
                    Ok(FollowUp::Quit) => break 'serve Outcome::Quit,
                    Err(err) => ctx.report(&err),
                },
            },
            _ = battery.tick() => {
                if let Err(err) = session.request(&Command::QueryBattery, DEFAULT_TIMEOUT).await {
                    debug!(error = %err, "battery poll did not answer");
                }
            }
        }
    };

    session.close().await;
    info!("control link closed");
    outcome
}

/// What the serving loop should do after an action.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FollowUp {
    /// Stay on the current session.
    Keep,
    /// Drop the link and reconnect.
    Reconnect,
    /// Quit the app.
    Quit,
}

/// Send the traffic one menu action stands for.
async fn perform(
    ctx: &Ctx,
    settings: &mut Config,
    session: &Session,
    action: Action,
) -> Result<FollowUp> {
    let snapshot = ctx.snapshot();
    let follow_up = match &action {
        Action::Quit => FollowUp::Quit,
        Action::Refresh => {
            let status = session.refresh().await;
            ctx.merge(status);
            FollowUp::Keep
        }
        Action::AudioToMac => {
            let selector = settings.device_name.clone();
            let device = tokio::task::spawn_blocking(move || {
                edifier_macos::connect_paired_audio(selector.as_deref())
            })
            .await
            .context("the audio link task panicked")?
            .context("cannot move the audio link to this Mac")?;
            info!(device = %device, "audio link moved to this Mac");
            FollowUp::Keep
        }
        // Handled by the menu target, which prompts and comes back as `SetName`.
        Action::Rename => FollowUp::Keep,
        Action::SetName(name) => {
            confirm(ctx, session.set_name(name, snapshot.model).await)?;
            FollowUp::Keep
        }
        Action::SetAnc(mode) => {
            settle(ctx, session, &Command::SetAnc(*mode)).await?;
            FollowUp::Keep
        }
        Action::SetAmbientVolume(volume) => {
            let volume = AmbientVolume::new(*volume).map_err(anyhow::Error::msg)?;
            settle(ctx, session, &Command::SetAmbientVolume(volume)).await?;
            FollowUp::Keep
        }
        Action::SetSoundEffect(effect) => {
            settle(ctx, session, &Command::SetSoundEffect(*effect)).await?;
            FollowUp::Keep
        }
        Action::SetLdac(mode) => {
            settle(ctx, session, &Command::SetLdac(*mode)).await?;
            FollowUp::Keep
        }
        Action::ToggleGameMode => {
            let enabled = !snapshot.status.game_mode.unwrap_or(false);
            settle(ctx, session, &Command::SetGameMode(enabled)).await?;
            FollowUp::Keep
        }
        Action::SetShutdownTimer(timer) => {
            let command = match timer {
                ShutdownTimer::Disabled => Command::SetShutdownTimer(None),
                ShutdownTimer::Minutes(minutes) => Command::SetShutdownTimer(Some(*minutes)),
            };
            settle(ctx, session, &command).await?;
            FollowUp::Keep
        }
        Action::ToggleGesture(bit) => {
            let next = bit.toggled(snapshot.status.anc_gestures.unwrap_or_default());
            settle(ctx, session, &Command::SetControlSettings(next)).await?;
            FollowUp::Keep
        }
        Action::SetPromptVolume(volume) => {
            settle(ctx, session, &Command::SetPromptVolume(*volume)).await?;
            FollowUp::Keep
        }
        Action::Disconnect => {
            session.send(&Command::Disconnect).await?;
            FollowUp::Reconnect
        }
        Action::RePair => {
            session.send(&Command::RePair).await?;
            if settings.forget() {
                ctx.persist(settings)?;
            }
            FollowUp::Reconnect
        }
        Action::PowerOff => {
            session.send(&Command::PowerOff).await?;
            FollowUp::Reconnect
        }
        Action::FactoryReset => {
            session.send(&Command::FactoryReset).await?;
            FollowUp::Reconnect
        }
    };
    info!(action = %action.label(), "menu action handled");
    Ok(follow_up)
}

/// Write a setter and fold the device's answer when one arrives.
///
/// Several W820NB setters echo the *query* command byte of their feature (`48` for LDAC, `08` for
/// the game mode, `D5` for the sound effect, `C9` for the name, `D3` for the timer), and
/// [`edifier_link`] correlates answers by command byte, so those setters are never acknowledged.
/// Silence is therefore reported as "written, unconfirmed" rather than as a failure: the device
/// still publishes the new state, and that event stream is what the menu renders.
async fn settle(ctx: &Ctx, session: &Session, command: &Command) -> Result<()> {
    confirm(ctx, session.request(command, SETTLE_TIMEOUT).await)
}

/// Fold a setter's answer, tolerating the commands the headset never acknowledges.
fn confirm(ctx: &Ctx, answer: edifier_link::Result<Event>) -> Result<()> {
    match answer {
        Ok(event) => {
            ctx.fold(&event);
            Ok(())
        }
        Err(LinkError::Timeout(wait)) => {
            debug!(
                ?wait,
                "command went out unconfirmed; waiting for the device's next state frame"
            );
            Ok(())
        }
        Err(err) => Err(err.into()),
    }
}

/// Wait out the reconnection delay, letting the user cut it short.
///
/// Returns `false` when the app is quitting.
async fn wait_for_retry(
    ctx: &Ctx,
    settings: &mut Config,
    requests: &mut mpsc::Receiver<Action>,
    delay: Duration,
) -> bool {
    let deadline = tokio::time::Instant::now() + delay;
    loop {
        tokio::select! {
            _ = tokio::time::sleep_until(deadline) => return true,
            request = requests.recv() => match request {
                None | Some(Action::Quit) => return false,
                Some(Action::Refresh) => return true,
                Some(Action::RePair) => {
                    if settings.forget()
                        && let Err(err) = ctx.persist(settings)
                    {
                        ctx.report(&err);
                    }
                    return true;
                }
                Some(action) => {
                    debug!(action = %action.label(), "ignored while disconnected");
                }
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use edifier_link::transport::{
        LinkError as TransportError, Result as LinkResult, Transport, TransportReader,
        TransportWriter,
    };
    use edifier_proto::model::DeviceModel;
    use edifier_proto::{AncMode, ControlSettings, Frame, Head, LdacMode, SoundEffect};
    use std::collections::HashMap;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use crate::menu::GestureBit;
    use crate::test_support::TempDir;

    fn candidate(identifier: &str, name: Option<&str>) -> Discovered {
        Discovered {
            identifier: identifier.to_string(),
            name: name.map(str::to_string),
            rssi: Some(-42),
            service: None,
            model: DeviceModel::from_name(name.unwrap_or_default()),
        }
    }

    #[test]
    fn select_endpoint_prefers_the_remembered_identifier() {
        let found = [
            candidate("uuid-1", Some("EDIFIER W820NB")),
            candidate("uuid-2", Some("EDIFIER W200BT Plus")),
        ];
        assert_eq!(select_endpoint(Some("uuid-2"), None, &found), Some(1));
        assert_eq!(
            select_endpoint(Some("uuid-9"), Some("w820nb"), &found),
            Some(0)
        );
        assert_eq!(
            select_endpoint(Some("uuid-9"), Some("w200bt"), &found),
            Some(1)
        );
        assert_eq!(select_endpoint(Some("uuid-9"), None, &found), Some(0));
        assert_eq!(select_endpoint(None, None, &found), Some(0));
    }

    #[test]
    fn select_endpoint_reports_nothing_when_nothing_was_found() {
        assert_eq!(select_endpoint(Some("uuid-1"), Some("EDIFIER"), &[]), None);
    }

    #[test]
    fn apply_selector_splits_identifiers_from_name_fragments() {
        let mut settings = Config::default();
        apply_selector(&mut settings, "B1E4E6C2-1365-C7BE-6B22-4B0F0FDD7C4A");
        assert_eq!(
            settings.device_identifier.as_deref(),
            Some("B1E4E6C2-1365-C7BE-6B22-4B0F0FDD7C4A")
        );
        assert_eq!(settings.device_name, None);

        let mut settings = Config::default();
        apply_selector(&mut settings, "EDIFIER W820NB");
        assert_eq!(settings.device_identifier, None);
        assert_eq!(settings.device_name.as_deref(), Some("EDIFIER W820NB"));
    }

    /// A waker that counts the nudges it received.
    #[derive(Default)]
    struct CountingWaker(AtomicUsize);

    impl Waker for CountingWaker {
        fn wake(&self) {
            self.0.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Frames the fake headset received, in order.
    type Sent = Arc<Mutex<Vec<Vec<u8>>>>;

    /// Notification half of the loopback transport.
    struct LoopbackReader {
        replies: mpsc::Receiver<Vec<u8>>,
    }

    /// Command half of the loopback transport: records requests and answers them from a script.
    struct LoopbackWriter {
        sent: Sent,
        replies: mpsc::Sender<Vec<u8>>,
        script: HashMap<u8, Vec<u8>>,
    }

    /// A fake headset reachable over a loopback transport.
    struct Loopback {
        reader: LoopbackReader,
        writer: LoopbackWriter,
    }

    impl TransportReader for LoopbackReader {
        async fn next_chunk(&mut self) -> LinkResult<Option<Vec<u8>>> {
            Ok(self.replies.recv().await)
        }
    }

    impl TransportWriter for LoopbackWriter {
        async fn write_frame(&mut self, bytes: &[u8]) -> LinkResult<()> {
            self.sent.lock().expect("test lock").push(bytes.to_vec());
            let request = Frame::parse(bytes).map_err(TransportError::Protocol)?;
            if let Some(command) = request.command_byte()
                && let Some(payload) = self.script.get(&command)
            {
                let reply = Frame {
                    head: Head::State,
                    payload: payload.clone(),
                }
                .encode()
                .map_err(TransportError::Protocol)?;
                self.replies
                    .send(reply)
                    .await
                    .map_err(|_| TransportError::Closed)?;
            }
            Ok(())
        }

        async fn close(&mut self) {}
    }

    impl Transport for Loopback {
        type Reader = LoopbackReader;
        type Writer = LoopbackWriter;

        fn split(self) -> (LoopbackReader, LoopbackWriter) {
            (self.reader, self.writer)
        }

        fn describe(&self) -> String {
            "loopback EDIFIER W820NB (EDIFIER W820NB)".to_string()
        }
    }

    /// Build a session whose fake device answers `script`, keyed by request command byte.
    fn loopback(script: &[(u8, Vec<u8>)]) -> (Session, Sent) {
        let sent: Sent = Arc::new(Mutex::new(Vec::new()));
        let (tx, rx) = mpsc::channel(16);
        let transport = Loopback {
            reader: LoopbackReader { replies: rx },
            writer: LoopbackWriter {
                sent: Arc::clone(&sent),
                replies: tx,
                script: script.iter().cloned().collect(),
            },
        };
        (Session::spawn(transport), sent)
    }

    /// Everything [`perform`] needs, plus the temp directory keeping the settings file alive.
    struct Harness {
        /// Kept alive for the lifetime of the test.
        #[allow(dead_code)]
        dir: TempDir,
        ctx: Ctx,
        settings: Config,
        waker: Arc<CountingWaker>,
    }

    fn harness() -> Harness {
        let dir = TempDir::new("link-harness");
        let waker = Arc::new(CountingWaker::default());
        let ctx = Ctx::new(
            SharedState::new(),
            Arc::clone(&waker) as Arc<dyn Waker>,
            dir.path().join("config.json"),
        );
        Harness {
            dir,
            ctx,
            settings: Config::default(),
            waker,
        }
    }

    /// Let the session's writer task drain the outgoing queue.
    async fn drain(sent: &Sent) -> Vec<Vec<u8>> {
        for _ in 0..50 {
            let frames = sent.lock().expect("test lock").clone();
            if !frames.is_empty() {
                return frames;
            }
            tokio::task::yield_now().await;
        }
        Vec::new()
    }

    /// Payload of the single frame the fake headset received.
    async fn only_request(sent: &Sent) -> Vec<u8> {
        let frames = drain(sent).await;
        assert_eq!(frames.len(), 1, "expected exactly one request");
        Frame::parse(&frames[0])
            .expect("the recorded frame is a valid host frame")
            .payload
    }

    #[tokio::test]
    async fn anc_action_sends_the_matching_command_and_updates_the_snapshot() {
        let (session, sent) = loopback(&[(0xC1, vec![0xC1, 0x02, 0x08])]);
        let harness = harness();

        let follow_up = perform(
            &harness.ctx,
            &mut harness.settings.clone(),
            &session,
            Action::SetAnc(AncMode::NoiseCancelling),
        )
        .await
        .expect("action succeeds");

        assert_eq!(follow_up, FollowUp::Keep);
        assert_eq!(only_request(&sent).await, vec![0xC1, 0x02]);
        let snapshot = harness.ctx.snapshot();
        assert_eq!(snapshot.status.anc, Some(AncMode::NoiseCancelling));
        assert_eq!(snapshot.status.ambient_volume, Some(2));
    }

    #[tokio::test]
    async fn gesture_action_flips_one_bit_of_the_current_mask() {
        let (session, sent) = loopback(&[(0xF1, vec![0xF1, 0x0A, 0b110])]);
        let harness = harness();
        harness.ctx.state.update(|state| {
            state.status.anc_gestures = Some(ControlSettings::all());
        });

        perform(
            &harness.ctx,
            &mut harness.settings.clone(),
            &session,
            Action::ToggleGesture(GestureBit::Ambient),
        )
        .await
        .expect("action succeeds");

        assert_eq!(only_request(&sent).await, vec![0xF1, 0x0A, 0b011]);
        assert_eq!(
            harness
                .ctx
                .snapshot()
                .status
                .anc_gestures
                .map(|settings| settings.mask()),
            Some(0b110)
        );
    }

    #[tokio::test]
    async fn ldac_action_reports_the_device_state_verbatim() {
        let (session, sent) = loopback(&[(0x49, vec![0x49, 0x02])]);
        let harness = harness();

        perform(
            &harness.ctx,
            &mut harness.settings.clone(),
            &session,
            Action::SetLdac(LdacMode::Khz96),
        )
        .await
        .expect("action succeeds");

        assert_eq!(only_request(&sent).await, vec![0x49, 0x02]);
        assert_eq!(harness.ctx.snapshot().status.ldac, Some(LdacMode::Khz96));
    }

    #[tokio::test]
    async fn sound_effect_action_sends_the_preset_code() {
        let (session, sent) = loopback(&[(0xC4, vec![0xD5, 0x03])]);
        let harness = harness();

        perform(
            &harness.ctx,
            &mut harness.settings.clone(),
            &session,
            Action::SetSoundEffect(SoundEffect::Rock),
        )
        .await
        .expect("action succeeds");

        assert_eq!(only_request(&sent).await, vec![0xC4, 0x03]);
        // Presets are reported in `D5` frames. `Session::request` accepts that state frame as the
        // answer to the `C4` setter (firmware answers some setters with the feature's query byte),
        // so the new preset is reflected immediately instead of waiting for the next push.
        assert_eq!(
            harness.ctx.snapshot().status.sound_effect,
            Some(SoundEffect::Rock)
        );
    }

    #[tokio::test]
    async fn game_mode_toggles_the_opposite_of_the_known_state() {
        let (session, sent) = loopback(&[(0x09, vec![0x09, 0x00])]);
        let harness = harness();
        harness
            .ctx
            .state
            .update(|state| state.status.game_mode = Some(true));

        perform(
            &harness.ctx,
            &mut harness.settings.clone(),
            &session,
            Action::ToggleGameMode,
        )
        .await
        .expect("action succeeds");

        assert_eq!(only_request(&sent).await, vec![0x09, 0x00]);
        assert_eq!(harness.ctx.snapshot().status.game_mode, Some(false));
    }

    #[tokio::test]
    async fn a_silent_setter_is_not_reported_as_a_failure() {
        // No scripted reply at all: the request never correlates, exactly like a real rename.
        let (session, sent) = loopback(&[]);
        let harness = harness();

        let follow_up = perform(
            &harness.ctx,
            &mut harness.settings.clone(),
            &session,
            Action::SetShutdownTimer(ShutdownTimer::Minutes(30)),
        )
        .await
        .expect("an unconfirmed setter is still a success");

        assert_eq!(follow_up, FollowUp::Keep);
        assert_eq!(only_request(&sent).await, vec![0xD1, 0x00, 0x1E]);
        assert_eq!(harness.ctx.snapshot().error, None);
    }

    #[tokio::test]
    async fn rename_action_truncates_to_the_model_limit() {
        let (session, sent) = loopback(&[(0xCA, {
            let mut payload = vec![0xC9];
            payload.extend_from_slice(b"coach");
            payload
        })]);
        let mut events = session.subscribe();
        let harness = harness();
        harness
            .ctx
            .state
            .update(|state| state.model = Some(DeviceModel::W820Nb));

        perform(
            &harness.ctx,
            &mut harness.settings.clone(),
            &session,
            Action::SetName("coach".repeat(10)),
        )
        .await
        .expect("action succeeds");

        let payload = only_request(&sent).await;
        assert_eq!(payload[0], 0xCA);
        assert_eq!(payload.len() - 1, 24, "W820NB names are capped at 24 bytes");
        assert!(payload[1..].starts_with(b"coach"));

        // The headset confirms a rename with a `C9` frame, which `Session` correlates with
        // `QueryName` instead of `SetName`: the new name reaches the menu through the event stream.
        let event = events.recv().await.expect("the rename is echoed");
        harness.ctx.fold(&event);
        assert_eq!(harness.ctx.snapshot().status.name.as_deref(), Some("coach"));
        assert_eq!(harness.ctx.snapshot().error, None);
    }

    #[tokio::test]
    async fn destructive_actions_ask_for_a_link_restart() {
        for (action, payload) in [
            (Action::Disconnect, vec![0xCD]),
            (Action::RePair, vec![0xCF]),
            (Action::PowerOff, vec![0xCE]),
            (Action::FactoryReset, vec![0x07]),
        ] {
            let (session, sent) = loopback(&[]);
            let harness = harness();
            let follow_up = perform(
                &harness.ctx,
                &mut harness.settings.clone(),
                &session,
                action,
            )
            .await
            .expect("action succeeds");
            assert_eq!(follow_up, FollowUp::Reconnect);
            assert_eq!(only_request(&sent).await, payload);
        }
    }

    #[tokio::test]
    async fn re_pair_forgets_the_remembered_device() {
        let (session, _sent) = loopback(&[]);
        let mut harness = harness();
        harness.settings.device_identifier = Some("uuid-1".to_string());
        harness.settings.device_name = Some("EDIFIER W820NB".to_string());

        perform(
            &harness.ctx,
            &mut harness.settings,
            &session,
            Action::RePair,
        )
        .await
        .expect("action succeeds");

        assert_eq!(harness.settings, Config::default());
        assert!(
            harness.ctx.settings_path.exists(),
            "the forgotten device was written back to disk"
        );
    }

    #[tokio::test]
    async fn a_rejected_action_is_published_instead_of_panicking() {
        let (session, _sent) = loopback(&[]);
        let harness = harness();

        let error = perform(
            &harness.ctx,
            &mut harness.settings.clone(),
            &session,
            Action::SetAmbientVolume(9),
        )
        .await
        .expect_err("out-of-range volume is rejected");
        harness.ctx.report(&error);

        let visible = harness.ctx.snapshot().error.expect("error is visible");
        assert!(visible.contains("out of range"), "{visible}");
        assert!(
            harness.waker.0.load(Ordering::Relaxed) > 0,
            "AppKit was nudged"
        );
    }

    #[tokio::test]
    async fn waiting_for_a_retry_can_be_cut_short_by_the_user() {
        let harness = harness();
        let (tx, mut rx) = mpsc::channel(4);
        tx.send(Action::Refresh).await.expect("queued");
        assert!(
            wait_for_retry(
                &harness.ctx,
                &mut harness.settings.clone(),
                &mut rx,
                Duration::from_secs(300)
            )
            .await,
            "a refresh request retries immediately"
        );

        let (tx, mut rx) = mpsc::channel(4);
        drop(tx);
        assert!(
            !wait_for_retry(
                &harness.ctx,
                &mut harness.settings.clone(),
                &mut rx,
                Duration::from_secs(300)
            )
            .await,
            "a closed channel stops the runtime"
        );
    }
}
