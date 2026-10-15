//! Request/response session: framing, correlation, status tracking and shutdown.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use edifier_proto::command::AmbientVolume;
use edifier_proto::event::ShutdownTimer;
use edifier_proto::model::Feature;
use edifier_proto::{
    AncMode, Command, DeviceModel, Event, FrameDecoder, LdacMode, SoundEffect, Status,
};
use tokio::sync::{Mutex, broadcast, mpsc, oneshot};
use tokio::task::JoinHandle;
use tokio::time::timeout;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use crate::ble::{self, Target};
use crate::transport::{LinkError, Result, Transport, TransportReader, TransportWriter};

/// Default wait for the device to answer one request.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(3);
/// Spacing between queries during a refresh, matching mEDIFIER's `readSettings()`.
const REFRESH_SPACING: Duration = Duration::from_millis(150);
/// Idle gap after which a partial frame is discarded (`packetTimeoutMs` in `comm.cpp`).
const FRAME_IDLE_RESET: Duration = Duration::from_secs(5);
/// Depth of the outgoing frame queue.
const WRITE_QUEUE: usize = 16;
/// Depth of the event fan-out buffer.
const EVENT_QUEUE: usize = 64;

/// A live control session over any [`Transport`].
pub struct Session {
    commands: mpsc::Sender<Vec<u8>>,
    pending: Arc<Mutex<HashMap<u8, oneshot::Sender<Event>>>>,
    events: broadcast::Sender<Event>,
    status: Arc<Mutex<Status>>,
    cancel: CancellationToken,
    reader: JoinHandle<()>,
    writer: JoinHandle<()>,
    description: String,
    /// Serialises transactions: the protocol carries no correlation id, so two requests in
    /// flight at once would be ambiguous. Held across awaits on purpose, which is the one case
    /// `async-no-lock-await` allows for `tokio::sync::Mutex`.
    request_lock: Mutex<()>,
}

impl Session {
    /// Wrap a connected transport in a session: a reader task decodes and dispatches frames, a
    /// writer task drains the outgoing queue.
    pub fn spawn<T: Transport>(transport: T) -> Session {
        let description = transport.describe();
        let (reader_half, mut writer_half) = transport.split();

        let (command_tx, mut command_rx) = mpsc::channel::<Vec<u8>>(WRITE_QUEUE);
        let (event_tx, _) = broadcast::channel::<Event>(EVENT_QUEUE);
        let pending: Arc<Mutex<HashMap<u8, oneshot::Sender<Event>>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let status = Arc::new(Mutex::new(Status::new()));
        let cancel = CancellationToken::new();

        let writer_cancel = cancel.clone();
        let writer = tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = writer_cancel.cancelled() => break,
                    outgoing = command_rx.recv() => {
                        let Some(bytes) = outgoing else { break };
                        if let Err(err) = writer_half.write_frame(&bytes).await {
                            warn!(error = %err, "transport write failed");
                            break;
                        }
                    }
                }
            }
            writer_half.close().await;
            debug!("session writer stopped");
        });

        let reader_pending = Arc::clone(&pending);
        let reader_events = event_tx.clone();
        let reader_status = Arc::clone(&status);
        let reader_cancel = cancel.clone();
        let reader = tokio::spawn(async move {
            let mut reader_half = reader_half;
            let mut decoder = FrameDecoder::new();
            // Frame boundaries are abandoned after `FRAME_IDLE_RESET` of silence, matching the
            // buffer cleaner in mEDIFIER's `comm.cpp`.
            let mut last_rx: Option<tokio::time::Instant> = None;
            loop {
                tokio::select! {
                    _ = reader_cancel.cancelled() => break,
                    chunk = reader_half.next_chunk() => {
                        match chunk {
                            Ok(Some(bytes)) => {
                                if last_rx.is_some_and(|at| at.elapsed() > FRAME_IDLE_RESET) {
                                    decoder.reset();
                                }
                                last_rx = Some(tokio::time::Instant::now());
                                decoder.push(&bytes);
                                while let Some(decoded) = decoder.next_frame() {
                                    let frame = match decoded {
                                        Ok(frame) => frame,
                                        Err(err) => {
                                            warn!(error = %err, "dropping malformed frame");
                                            continue;
                                        }
                                    };
                                    let event = Event::parse(&frame);
                                    debug!(event = %event, "device event");
                                    event.apply(&mut *reader_status.lock().await);
                                    let command = frame.command_byte();
                                    if reader_events.send(event.clone()).is_err() {
                                        debug!("no event subscribers");
                                    }
                                    // Correlation: the captures show every setter echoed with its
                                    // own command byte, but firmware variants answer some setters
                                    // with the feature's query byte (`CA` name → `C9` state,
                                    // `49` LDAC → `48`, `09` game mode → `08`, `D1` timer → `D3`).
                                    // Since exactly one request is in flight, fall back to that
                                    // request when this event is typed as its answer.
                                    let mut pending = reader_pending.lock().await;
                                    let key = command
                                        .filter(|byte| pending.contains_key(byte))
                                        .or_else(|| {
                                            let mut keys = pending.keys().copied();
                                            match (keys.next(), keys.next()) {
                                                (Some(only), None) if event.answers(only) => {
                                                    Some(only)
                                                }
                                                _ => None,
                                            }
                                        });
                                    if let Some(sender) = key.and_then(|key| pending.remove(&key)) {
                                        let _ = sender.send(event);
                                    }
                                }
                            }
                            Ok(None) => break,
                            Err(err) => {
                                warn!(error = %err, "transport read failed");
                                break;
                            }
                        }
                    }
                }
            }
            debug!("session reader stopped");
        });

        Session {
            commands: command_tx,
            pending,
            events: event_tx,
            status,
            cancel,
            reader,
            writer,
            description,
            request_lock: Mutex::new(()),
        }
    }

    /// Open a BLE session in one step: the endpoint macOS already connected if there is one,
    /// otherwise a scan.
    ///
    /// # Errors
    ///
    /// Propagates the discovery/connection errors of [`crate::ble::connect`].
    pub async fn connect_ble(target: Target) -> Result<Session> {
        // macOS keeps a paired headset's BLE control link open, and a connected peripheral stops
        // advertising — so scanning alone cannot find the normal case. Ask the system first.
        #[cfg(target_os = "macos")]
        match crate::system_ble::connect(
            &target,
            crate::system_ble::DEFAULT_SERVICE,
            Duration::from_secs(10),
        )
        .await
        {
            Ok(transport) => {
                let model = transport.model();
                let session = Session::spawn(transport);
                session.note_model(model).await;
                return Ok(session);
            }
            Err(err) => {
                debug!(error = %err, "no system-connected endpoint, falling back to a scan")
            }
        }

        let transport = ble::connect(&target, Duration::from_secs(10)).await?;
        let model = transport.model();
        let session = Session::spawn(transport);
        session.note_model(model).await;
        Ok(session)
    }

    /// Record the model the transport guessed, so feature queries skip what the device lacks.
    pub async fn note_model(&self, model: DeviceModel) {
        self.status.lock().await.model = Some(model);
    }

    /// Link description, e.g. `BLE EDIFIER W820NB (EDIFIER W820NB)`.
    pub fn describe(&self) -> &str {
        &self.description
    }

    /// Whether both session tasks are still running.
    pub fn is_alive(&self) -> bool {
        !self.reader.is_finished() && !self.writer.is_finished()
    }

    /// Subscribe to every event the device pushes.
    pub fn subscribe(&self) -> broadcast::Receiver<Event> {
        self.events.subscribe()
    }

    /// Latest known status snapshot.
    pub async fn status(&self) -> Status {
        self.status.lock().await.clone()
    }

    /// Send one already-framed packet (CLI `raw`).
    pub async fn send_frame(&self, frame: Vec<u8>) -> Result<()> {
        self.commands
            .send(frame)
            .await
            .map_err(|_| LinkError::Closed)
    }

    /// Send a command and wait for the device's answer (state frame or acknowledgement).
    ///
    /// # Errors
    ///
    /// [`LinkError::Timeout`] when the device stays silent, [`LinkError::Closed`] when the link
    /// dropped, [`LinkError::Protocol`] for an over-long payload.
    pub async fn request(&self, command: &Command, wait: Duration) -> Result<Event> {
        let frame = command.encode()?;
        let command_byte = *frame
            .get(2)
            .ok_or_else(|| LinkError::Other("empty command payload".into()))?;

        let _guard = self.request_lock.lock().await;

        let (sender, receiver) = oneshot::channel();
        self.pending.lock().await.insert(command_byte, sender);

        if self.commands.send(frame).await.is_err() {
            self.pending.lock().await.remove(&command_byte);
            return Err(LinkError::Closed);
        }

        match timeout(wait, receiver).await {
            Ok(Ok(event)) => Ok(event),
            Ok(Err(_)) => Err(LinkError::Closed),
            Err(_) => {
                self.pending.lock().await.remove(&command_byte);
                Err(LinkError::Timeout(wait))
            }
        }
    }

    /// Send a command without waiting for its answer.
    pub async fn send(&self, command: &Command) -> Result<()> {
        self.send_frame(command.encode()?).await
    }

    /// Query everything the host tracks and merge the answers into the snapshot.
    ///
    /// Features the model lacks are skipped, and a query the firmware does not implement is
    /// logged and skipped rather than failing the refresh.
    pub async fn refresh(&self) -> Status {
        let model = self.status.lock().await.model;
        let mut queries = vec![
            Command::QueryBattery,
            Command::QueryAnc,
            Command::QueryName,
            Command::QueryMac,
            Command::QueryFirmware,
        ];

        let optional = [
            (Feature::LdacGroup, Command::QueryLdac),
            (Feature::GameMode, Command::QueryGameMode),
            (Feature::SoundEffectGroup, Command::QuerySoundEffect),
            (Feature::PromptVolume, Command::QueryPromptVolume),
            (Feature::ShutdownTimer, Command::QueryShutdownTimer),
            (Feature::AutoPoweroff, Command::QueryAutoPoweroff),
            (Feature::ControlSettingsGroup, Command::QueryControlSettings),
        ];
        queries.extend(optional.into_iter().filter_map(|(feature, command)| {
            model
                .is_none_or(|model| model.supports(feature))
                .then_some(command)
        }));

        for query in queries {
            match self.request(&query, DEFAULT_TIMEOUT).await {
                Ok(_) => {}
                Err(LinkError::Timeout(_)) => {
                    debug!(command = %query.label(), "unanswered query (unsupported feature?)");
                }
                Err(err) => {
                    warn!(command = %query.label(), error = %err, "query failed");
                    break;
                }
            }
            tokio::time::sleep(REFRESH_SPACING).await;
        }

        self.status().await
    }

    /// Switch ANC mode.
    ///
    /// # Errors
    ///
    /// See [`Session::request`].
    pub async fn set_anc(&self, mode: AncMode) -> Result<Event> {
        self.request(&Command::SetAnc(mode), DEFAULT_TIMEOUT).await
    }

    /// Set the ambient-sound volume; the firmware switches to ambient mode as a side effect.
    ///
    /// # Errors
    ///
    /// See [`Session::request`].
    pub async fn set_ambient_volume(&self, volume: AmbientVolume) -> Result<Event> {
        self.request(&Command::SetAmbientVolume(volume), DEFAULT_TIMEOUT)
            .await
    }

    /// Set the sound-effect preset.
    ///
    /// # Errors
    ///
    /// See [`Session::request`].
    pub async fn set_sound_effect(&self, effect: SoundEffect) -> Result<Event> {
        self.request(&Command::SetSoundEffect(effect), DEFAULT_TIMEOUT)
            .await
    }

    /// Set the LDAC mode. A non-`Off` mode makes the headset demand an LDAC source.
    ///
    /// # Errors
    ///
    /// See [`Session::request`].
    pub async fn set_ldac(&self, mode: LdacMode) -> Result<Event> {
        self.request(&Command::SetLdac(mode), DEFAULT_TIMEOUT).await
    }

    /// Enable or disable the low-latency game mode.
    ///
    /// # Errors
    ///
    /// See [`Session::request`].
    pub async fn set_game_mode(&self, enabled: bool) -> Result<Event> {
        self.request(&Command::SetGameMode(enabled), DEFAULT_TIMEOUT)
            .await
    }

    /// Rename the headset, truncating on a character boundary to the model's name limit.
    ///
    /// # Errors
    ///
    /// See [`Session::request`].
    pub async fn set_name(&self, name: &str, model: Option<DeviceModel>) -> Result<Event> {
        let limit = model.map_or(24, DeviceModel::max_name_length);
        let mut truncated = String::new();
        for character in name.chars() {
            if truncated.len() + character.len_utf8() > limit {
                break;
            }
            truncated.push(character);
        }
        self.request(&Command::SetName(truncated), DEFAULT_TIMEOUT)
            .await
    }

    /// Set or clear the auto-shutdown timer.
    ///
    /// # Errors
    ///
    /// See [`Session::request`].
    pub async fn set_shutdown_timer(&self, timer: ShutdownTimer) -> Result<Event> {
        let command = match timer {
            ShutdownTimer::Disabled => Command::SetShutdownTimer(None),
            ShutdownTimer::Minutes(minutes) => Command::SetShutdownTimer(Some(minutes)),
        };
        self.request(&command, DEFAULT_TIMEOUT).await
    }

    /// Cancel both tasks and wait for them to release the link.
    pub async fn close(self) {
        self.cancel.cancel();
        drop(self.commands);
        let _ = tokio::join!(self.reader, self.writer);
        info!("session closed");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transport::{TransportReader, TransportWriter};
    use edifier_proto::{Frame, Head};
    use std::sync::Arc;

    /// How the fake device answers a request frame.
    type Answers = Arc<dyn Fn(&[u8]) -> Vec<Vec<u8>> + Send + Sync>;

    struct FakeReader {
        responses: mpsc::Receiver<Vec<u8>>,
    }

    struct FakeWriter {
        answers: Answers,
        responses: mpsc::Sender<Vec<u8>>,
    }

    struct FakeTransport {
        reader: FakeReader,
        writer: FakeWriter,
    }

    impl Transport for FakeTransport {
        type Reader = FakeReader;
        type Writer = FakeWriter;

        fn split(self) -> (FakeReader, FakeWriter) {
            (self.reader, self.writer)
        }

        fn describe(&self) -> String {
            "fake W820NB".into()
        }
    }

    impl TransportReader for FakeReader {
        async fn next_chunk(&mut self) -> Result<Option<Vec<u8>>> {
            Ok(self.responses.recv().await)
        }
    }

    impl TransportWriter for FakeWriter {
        async fn write_frame(&mut self, bytes: &[u8]) -> Result<()> {
            for response in (self.answers)(bytes) {
                self.responses
                    .send(response)
                    .await
                    .map_err(|_| LinkError::Closed)?;
            }
            Ok(())
        }

        async fn close(&mut self) {}
    }

    fn fake_device(answers: Answers) -> FakeTransport {
        let (responses, received) = mpsc::channel(16);
        FakeTransport {
            reader: FakeReader {
                responses: received,
            },
            writer: FakeWriter { answers, responses },
        }
    }

    fn frame(head: Head, payload: &[u8]) -> Vec<u8> {
        Frame {
            head,
            payload: payload.to_vec(),
        }
        .encode()
        .expect("test frames are short enough to encode")
    }

    /// Firmware that echoes every request with an acknowledgement carrying the request's byte.
    fn echo_ack() -> Answers {
        Arc::new(|request: &[u8]| {
            request
                .get(2)
                .map(|command| vec![frame(Head::Ack, &[*command, 0x01])])
                .unwrap_or_default()
        })
    }

    #[tokio::test]
    async fn documented_setters_resolve_on_their_own_command_byte() {
        let session = Session::spawn(fake_device(echo_ack()));
        let event = session
            .request(
                &Command::SetName("Shield".into()),
                Duration::from_millis(500),
            )
            .await
            .expect("an echoed ack resolves the request");
        assert_eq!(
            event,
            Event::Ack {
                command: 0xCA,
                status: vec![0x01]
            }
        );
        session.close().await;
    }

    #[tokio::test]
    async fn a_reply_using_the_query_byte_still_resolves() {
        // Some firmware answers `CA <name>` with the name *state* frame (0xC9) instead of an ack.
        let answers: Answers = Arc::new(|request: &[u8]| {
            if request.get(2) == Some(&0xCA) {
                vec![frame(Head::State, &[0xC9, b'H', b'i'])]
            } else {
                Vec::new()
            }
        });
        let session = Session::spawn(fake_device(answers));
        let event = session
            .request(&Command::SetName("Hi".into()), Duration::from_millis(500))
            .await
            .expect("the state frame answers the single pending request");
        assert_eq!(event, Event::Name("Hi".into()));
        session.close().await;
    }

    #[tokio::test]
    async fn unrelated_notifications_do_not_resolve_a_pending_request() {
        // The device pushes a battery frame but never answers the name query.
        let answers: Answers = Arc::new(|_| vec![frame(Head::State, &[0xD0, 77])]);
        let session = Session::spawn(fake_device(answers));
        let outcome = session
            .request(&Command::QueryName, Duration::from_millis(300))
            .await;
        assert!(
            matches!(outcome, Err(LinkError::Timeout(_))),
            "unrelated events must not satisfy a request: {outcome:?}"
        );
        session.close().await;
    }
}
