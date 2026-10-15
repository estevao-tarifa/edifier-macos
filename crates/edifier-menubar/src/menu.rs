//! The pure view model: what the menu bar shows and which action every menu item carries.
//!
//! Nothing in this module touches AppKit or BLE, so the state machine, the labels and the
//! tag ↔ action mapping can be unit tested without a headset or a display.

use std::sync::{Arc, Mutex, PoisonError};

use edifier_proto::event::ShutdownTimer;
use edifier_proto::{
    AncMode, ControlSettings, DeviceModel, Event, Feature, LdacMode, SoundEffect, Status,
};

/// Shutdown timers offered in the menu; `None` is the "stay on" entry.
pub const SHUTDOWN_CHOICES: [Option<u8>; 6] =
    [None, Some(15), Some(30), Some(60), Some(120), Some(180)];

/// Highest prompt volume offered in the menu.
pub const PROMPT_VOLUME_MAX: u8 = 15;

/// Lowest ambient-sound volume offered in the menu.
pub const AMBIENT_VOLUME_MIN: i8 = -3;
/// Highest ambient-sound volume offered in the menu.
pub const AMBIENT_VOLUME_MAX: i8 = 3;

/// Tag base of the ANC radio group (`Off` / `Noise Cancelling` / `Transparency`).
const TAG_ANC: isize = 100;
/// Tag base of the ambient-sound volume radio group.
const TAG_AMBIENT: isize = 200;
/// Tag base of the sound-effect radio group.
const TAG_EFFECT: isize = 300;
/// Tag base of the LDAC radio group.
const TAG_LDAC: isize = 400;
/// Tag of the game-mode toggle.
const TAG_GAME_MODE: isize = 500;
/// Tag base of the shutdown-timer radio group.
const TAG_TIMER: isize = 600;
/// Tag base of the ANC-gesture toggles.
const TAG_GESTURE: isize = 700;
/// Tag base of the prompt-volume radio group.
const TAG_PROMPT: isize = 800;
/// Tag of the rename item.
const TAG_RENAME: isize = 900;
/// Tag of the "move audio to this Mac" item.
const TAG_AUDIO_TO_MAC: isize = 901;
/// Tag of the disconnect item.
const TAG_DISCONNECT: isize = 902;
/// Tag of the re-pair item.
const TAG_REPAIR: isize = 903;
/// Tag of the power-off item.
const TAG_POWER_OFF: isize = 904;
/// Tag of the factory-reset item.
const TAG_FACTORY_RESET: isize = 905;
/// Tag of the refresh item.
const TAG_REFRESH: isize = 906;
/// Tag of the quit item.
const TAG_QUIT: isize = 907;

/// How the app is doing right now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Connection {
    /// The runtime has not started yet.
    Idle,
    /// Looking for the headset.
    Scanning,
    /// The headset was found and the GATT link is being opened.
    Connecting,
    /// A live session; the payload is [`edifier_link::Session::describe`].
    Connected {
        /// Link description as reported by the transport.
        description: String,
    },
    /// The last attempt failed; another one is scheduled.
    Waiting {
        /// Seconds until the next attempt.
        seconds: u64,
        /// Why the last attempt failed.
        reason: String,
    },
    /// Connection attempts cannot succeed right now (no adapter, no endpoint).
    Offline {
        /// Human readable reason.
        reason: String,
    },
}

impl Connection {
    /// Whether a session is live.
    #[must_use]
    pub fn is_connected(&self) -> bool {
        matches!(self, Connection::Connected { .. })
    }

    /// One line describing the connection for the menu header.
    #[must_use]
    pub fn line(&self) -> String {
        match self {
            Connection::Idle => "Not started".to_string(),
            Connection::Scanning => "Scanning for the headset…".to_string(),
            Connection::Connecting => "Connecting…".to_string(),
            Connection::Connected { description } => description.clone(),
            Connection::Waiting { seconds, reason } => {
                format!("Not connected: {reason} (retrying in {seconds}s)")
            }
            Connection::Offline { reason } => format!("Not connected: {reason}"),
        }
    }

    /// Short reason for logs and error surfaces.
    #[must_use]
    pub fn reason(&self) -> Option<&str> {
        match self {
            Connection::Waiting { reason, .. } | Connection::Offline { reason } => Some(reason),
            _ => None,
        }
    }
}

/// Everything the menu renders, as one snapshot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UiState {
    /// Connection state.
    pub connection: Connection,
    /// Model guessed during discovery.
    pub model: Option<DeviceModel>,
    /// Latest values reported by the device.
    pub status: Status,
    /// Last failure that should be visible to the user.
    pub error: Option<String>,
}

impl Default for UiState {
    fn default() -> Self {
        Self {
            connection: Connection::Idle,
            model: None,
            status: Status::new(),
            error: None,
        }
    }
}

impl UiState {
    /// Fold one device notification into the snapshot.
    pub fn apply(&mut self, event: &Event) {
        event.apply(&mut self.status);
    }

    /// Overwrite the known values with a fresh snapshot.
    pub fn merge_status(&mut self, status: Status) {
        self.status.merge(status);
    }
}

/// The snapshot shared between the AppKit thread and the BLE runtime.
#[derive(Debug, Clone, Default)]
pub struct SharedState(Arc<Mutex<UiState>>);

impl SharedState {
    /// Start with [`UiState::default`].
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Copy of the current snapshot.
    #[must_use]
    pub fn snapshot(&self) -> UiState {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Edit the snapshot in place under the lock.
    pub fn update(&self, edit: impl FnOnce(&mut UiState)) {
        let mut guard = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        edit(&mut guard);
    }
}

/// SF Symbol shown in the menu bar.
#[must_use]
pub fn icon_symbol(state: &UiState) -> &'static str {
    if state.connection.is_connected() {
        "headphones"
    } else {
        "headphones.slash"
    }
}

/// Battery percentage shown next to the icon, empty when there is nothing trustworthy to show.
#[must_use]
pub fn battery_title(state: &UiState) -> String {
    match (state.connection.is_connected(), state.status.battery) {
        (true, Some(percent)) => format!("{percent}%"),
        _ => String::new(),
    }
}

/// Battery line of the menu.
#[must_use]
pub fn battery_line(state: &UiState) -> String {
    match state.status.battery {
        Some(percent) => format!("Battery: {percent}%"),
        None => "Battery: unknown".to_string(),
    }
}

/// Display name of an ANC mode.
#[must_use]
pub fn anc_label(mode: AncMode) -> &'static str {
    match mode {
        AncMode::Normal => "Off",
        AncMode::NoiseCancelling => "Noise Cancelling",
        AncMode::Ambient => "Transparency",
    }
}

/// Display name of a sound-effect preset.
#[must_use]
pub fn sound_effect_label(effect: SoundEffect) -> &'static str {
    match effect {
        SoundEffect::Normal => "Normal",
        SoundEffect::Pop => "Pop",
        SoundEffect::Classical => "Classical",
        SoundEffect::Rock => "Rock",
    }
}

/// Display name of an LDAC mode.
#[must_use]
pub fn ldac_label(mode: LdacMode) -> &'static str {
    match mode {
        LdacMode::Off => "Off",
        LdacMode::Khz48 => "48 kHz",
        LdacMode::Khz96 => "96 kHz",
    }
}

/// Display name of a shutdown timer.
#[must_use]
pub fn timer_label(timer: ShutdownTimer) -> String {
    match timer {
        ShutdownTimer::Disabled => "Off".to_string(),
        ShutdownTimer::Minutes(minutes) => format!("{minutes} min"),
    }
}

/// Signed ambient-sound volume, e.g. `-3` or `+1`.
#[must_use]
pub fn ambient_volume_label(volume: i8) -> String {
    if volume > 0 {
        format!("+{volume}")
    } else {
        volume.to_string()
    }
}

/// Display name of a prompt volume.
#[must_use]
pub fn prompt_volume_label(volume: u8) -> String {
    if volume == 0 {
        "Silent".to_string()
    } else {
        volume.to_string()
    }
}

/// Warning shown next to the LDAC entries.
#[must_use]
pub fn ldac_warning() -> &'static str {
    "macOS has no LDAC encoder — audio stops until this is switched back to Off"
}

/// One-line summary of the ANC gesture mapping.
#[must_use]
pub fn gesture_label(settings: ControlSettings) -> String {
    let enabled: Vec<&str> = GestureBit::ALL
        .iter()
        .filter(|bit| bit.is_enabled(settings))
        .map(|bit| bit.label())
        .collect();
    if enabled.is_empty() {
        "none".to_string()
    } else {
        enabled.join(", ")
    }
}

/// Whether the (possibly unknown) model exposes a feature.
#[must_use]
pub fn supports(model: Option<DeviceModel>, feature: Feature) -> bool {
    model.is_none_or(|model| model.supports(feature))
}

/// Header rows: device name, firmware, model and the connection description.
#[must_use]
pub fn header_lines(state: &UiState) -> Vec<String> {
    let name = state
        .status
        .name
        .clone()
        .or_else(|| state.model.map(|model| model.display_name().to_string()))
        .unwrap_or_else(|| "EDIFIER headset".to_string());

    let mut lines = vec![name];
    if let Some(firmware) = &state.status.firmware {
        lines.push(format!("Firmware {firmware}"));
    }
    if let Some(model) = state.model {
        lines.push(model.display_name().to_string());
    }
    lines.push(state.connection.line());
    if let Some(mac) = &state.status.mac {
        lines.push(format!("Address {mac}"));
    }
    lines
}

/// One of the three on-ear ANC gesture positions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GestureBit {
    /// The "off" position.
    Normal,
    /// The noise-cancelling position.
    NoiseCancelling,
    /// The transparency position.
    Ambient,
}

impl GestureBit {
    /// Every position, in wire-mask order.
    pub const ALL: [GestureBit; 3] = [
        GestureBit::Normal,
        GestureBit::NoiseCancelling,
        GestureBit::Ambient,
    ];

    /// Position within the `F1 0A` mask.
    #[must_use]
    pub const fn index(self) -> isize {
        match self {
            GestureBit::Normal => 0,
            GestureBit::NoiseCancelling => 1,
            GestureBit::Ambient => 2,
        }
    }

    /// Inverse of [`GestureBit::index`].
    #[must_use]
    pub const fn from_index(index: isize) -> Option<Self> {
        match index {
            0 => Some(GestureBit::Normal),
            1 => Some(GestureBit::NoiseCancelling),
            2 => Some(GestureBit::Ambient),
            _ => None,
        }
    }

    /// Display name of the gesture position.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            GestureBit::Normal => "Off",
            GestureBit::NoiseCancelling => "Noise Cancelling",
            GestureBit::Ambient => "Transparency",
        }
    }

    /// Whether the gesture can currently be cycled to.
    #[must_use]
    pub const fn is_enabled(self, settings: ControlSettings) -> bool {
        match self {
            GestureBit::Normal => settings.normal,
            GestureBit::NoiseCancelling => settings.noise_reduction,
            GestureBit::Ambient => settings.ambient,
        }
    }

    /// The settings with this position flipped.
    #[must_use]
    pub const fn toggled(self, settings: ControlSettings) -> ControlSettings {
        let mut next = settings;
        match self {
            GestureBit::Normal => next.normal = !next.normal,
            GestureBit::NoiseCancelling => next.noise_reduction = !next.noise_reduction,
            GestureBit::Ambient => next.ambient = !next.ambient,
        }
        next
    }
}

/// Everything the user can trigger from the menu.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// Switch the ANC mode.
    SetAnc(AncMode),
    /// Set the ambient-sound volume (implies [`AncMode::Ambient`]).
    SetAmbientVolume(i8),
    /// Set the sound-effect preset.
    SetSoundEffect(SoundEffect),
    /// Set the LDAC mode.
    SetLdac(LdacMode),
    /// Flip the low-latency game mode.
    ToggleGameMode,
    /// Set the auto-shutdown timer.
    SetShutdownTimer(ShutdownTimer),
    /// Add or remove one position from the ANC gesture cycle.
    ToggleGesture(GestureBit),
    /// Set the prompt volume.
    SetPromptVolume(u8),
    /// Ask the user for a new name and rename the headset.
    Rename,
    /// Rename the headset to this name.
    SetName(String),
    /// Open the classic Bluetooth audio link to this Mac.
    AudioToMac,
    /// Drop the control link.
    Disconnect,
    /// Forget the remembered device and re-enter pairing mode.
    RePair,
    /// Power the headset off.
    PowerOff,
    /// Factory reset the headset.
    FactoryReset,
    /// Re-query every value.
    Refresh,
    /// Quit the menu bar app.
    Quit,
}

impl Action {
    /// Numeric identifier attached to the matching `NSMenuItem`.
    #[must_use]
    pub fn tag(&self) -> isize {
        match self {
            Action::SetAnc(mode) => TAG_ANC + anc_index(*mode),
            Action::SetAmbientVolume(volume) => {
                TAG_AMBIENT + isize::from(*volume - AMBIENT_VOLUME_MIN)
            }
            Action::SetSoundEffect(effect) => TAG_EFFECT + isize::from(effect.code()),
            Action::SetLdac(mode) => TAG_LDAC + isize::from(mode.code()),
            Action::ToggleGameMode => TAG_GAME_MODE,
            Action::SetShutdownTimer(timer) => {
                shutdown_index(*timer).map_or(TAG_TIMER, |index| TAG_TIMER + index)
            }
            Action::ToggleGesture(bit) => TAG_GESTURE + bit.index(),
            Action::SetPromptVolume(volume) => TAG_PROMPT + isize::from(*volume),
            Action::Rename | Action::SetName(_) => TAG_RENAME,
            Action::AudioToMac => TAG_AUDIO_TO_MAC,
            Action::Disconnect => TAG_DISCONNECT,
            Action::RePair => TAG_REPAIR,
            Action::PowerOff => TAG_POWER_OFF,
            Action::FactoryReset => TAG_FACTORY_RESET,
            Action::Refresh => TAG_REFRESH,
            Action::Quit => TAG_QUIT,
        }
    }

    /// Inverse of [`Action::tag`]; `None` for a tag we did not create.
    ///
    /// [`Action::SetShutdownTimer`] falls back to "off" for an unknown slot, and a click on the
    /// rename item decodes to [`Action::Rename`], which asks the user for the name.
    #[must_use]
    pub fn from_tag(tag: isize) -> Option<Self> {
        match tag {
            TAG_ANC..=103 => Some(Action::SetAnc(match tag - TAG_ANC {
                0 => AncMode::Normal,
                1 => AncMode::NoiseCancelling,
                _ => AncMode::Ambient,
            })),
            TAG_AMBIENT..=206 => Some(Action::SetAmbientVolume(
                AMBIENT_VOLUME_MIN + i8::try_from(tag - TAG_AMBIENT).ok()?,
            )),
            TAG_EFFECT..=303 => Some(Action::SetSoundEffect(SoundEffect::from_code(
                u8::try_from(tag - TAG_EFFECT).ok()?,
            )?)),
            TAG_LDAC..=402 => Some(Action::SetLdac(LdacMode::from_code(
                u8::try_from(tag - TAG_LDAC).ok()?,
            )?)),
            TAG_GAME_MODE => Some(Action::ToggleGameMode),
            TAG_TIMER..=605 => Some(Action::SetShutdownTimer(shutdown_timer(
                usize::try_from(tag - TAG_TIMER).ok()?,
            )?)),
            TAG_GESTURE..=702 => Some(Action::ToggleGesture(GestureBit::from_index(
                tag - TAG_GESTURE,
            )?)),
            TAG_PROMPT..=816 => Some(Action::SetPromptVolume(
                u8::try_from(tag - TAG_PROMPT).ok()?,
            )),
            TAG_RENAME => Some(Action::Rename),
            TAG_AUDIO_TO_MAC => Some(Action::AudioToMac),
            TAG_DISCONNECT => Some(Action::Disconnect),
            TAG_REPAIR => Some(Action::RePair),
            TAG_POWER_OFF => Some(Action::PowerOff),
            TAG_FACTORY_RESET => Some(Action::FactoryReset),
            TAG_REFRESH => Some(Action::Refresh),
            TAG_QUIT => Some(Action::Quit),
            _ => None,
        }
    }

    /// Human readable label used in log lines and error messages.
    #[must_use]
    pub fn label(&self) -> String {
        match self {
            Action::SetAnc(mode) => format!("set ANC {}", anc_label(*mode)),
            Action::SetAmbientVolume(volume) => {
                format!("set ambient volume {}", ambient_volume_label(*volume))
            }
            Action::SetSoundEffect(effect) => {
                format!("set sound effect {}", sound_effect_label(*effect))
            }
            Action::SetLdac(mode) => format!("set LDAC {}", ldac_label(*mode)),
            Action::ToggleGameMode => "toggle game mode".to_string(),
            Action::SetShutdownTimer(timer) => {
                format!("set shutdown timer {}", timer_label(*timer))
            }
            Action::ToggleGesture(bit) => format!("toggle {} gesture", bit.label()),
            Action::SetPromptVolume(volume) => {
                format!("set prompt volume {}", prompt_volume_label(*volume))
            }
            Action::Rename => "rename the headset".to_string(),
            Action::SetName(name) => format!("rename to {name}"),
            Action::AudioToMac => "move audio to this Mac".to_string(),
            Action::Disconnect => "disconnect".to_string(),
            Action::RePair => "re-pair".to_string(),
            Action::PowerOff => "power off".to_string(),
            Action::FactoryReset => "factory reset".to_string(),
            Action::Refresh => "refresh status".to_string(),
            Action::Quit => "quit".to_string(),
        }
    }
}

/// The timer stored at `index` of [`SHUTDOWN_CHOICES`].
#[must_use]
pub fn shutdown_timer(index: usize) -> Option<ShutdownTimer> {
    SHUTDOWN_CHOICES.get(index).map(|choice| match choice {
        None => ShutdownTimer::Disabled,
        Some(minutes) => ShutdownTimer::Minutes(*minutes),
    })
}

/// Position of `timer` within [`SHUTDOWN_CHOICES`].
#[must_use]
pub fn shutdown_index(timer: ShutdownTimer) -> Option<isize> {
    let minutes = match timer {
        ShutdownTimer::Disabled => None,
        ShutdownTimer::Minutes(minutes) => Some(minutes),
    };
    SHUTDOWN_CHOICES
        .iter()
        .position(|choice| *choice == minutes)
        .and_then(|index| isize::try_from(index).ok())
}

/// Position of `mode` within the ANC radio group.
fn anc_index(mode: AncMode) -> isize {
    match mode {
        AncMode::Normal => 0,
        AncMode::NoiseCancelling => 1,
        AncMode::Ambient => 2,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn connected() -> UiState {
        UiState {
            connection: Connection::Connected {
                description: "BLE EDIFIER W820NB (EDIFIER W820NB)".to_string(),
            },
            model: Some(DeviceModel::W820Nb),
            status: Status::new(),
            error: None,
        }
    }

    #[test]
    fn icon_tracks_the_connection_state() {
        assert_eq!(icon_symbol(&connected()), "headphones");
        for connection in [
            Connection::Idle,
            Connection::Scanning,
            Connection::Connecting,
            Connection::Offline {
                reason: "no adapter".to_string(),
            },
        ] {
            let state = UiState {
                connection,
                ..connected()
            };
            assert_eq!(icon_symbol(&state), "headphones.slash");
        }
    }

    #[test]
    fn battery_title_needs_a_connected_session_and_a_known_level() {
        let mut state = connected();
        state.status.battery = Some(87);
        assert_eq!(battery_title(&state), "87%");

        state.status.battery = None;
        assert_eq!(battery_title(&state), "");

        state.status.battery = Some(87);
        state.connection = Connection::Offline {
            reason: "out of range".to_string(),
        };
        assert_eq!(battery_title(&state), "");
    }

    #[test]
    fn battery_line_reports_unknown_levels() {
        let mut state = connected();
        assert_eq!(battery_line(&state), "Battery: unknown");
        state.status.battery = Some(5);
        assert_eq!(battery_line(&state), "Battery: 5%");
    }

    #[test]
    fn header_lines_include_name_firmware_model_and_connection() {
        let mut state = connected();
        state.status.name = Some("EDIFIER W820NB".to_string());
        state.status.firmware = Some("1.2.3".to_string());
        state.status.mac = Some("0C:AE:BD:BC:8F:A2".to_string());

        assert_eq!(
            header_lines(&state),
            vec![
                "EDIFIER W820NB".to_string(),
                "Firmware 1.2.3".to_string(),
                "EDIFIER W820NB".to_string(),
                "BLE EDIFIER W820NB (EDIFIER W820NB)".to_string(),
                "Address 0C:AE:BD:BC:8F:A2".to_string(),
            ]
        );
    }

    #[test]
    fn header_lines_fall_back_to_the_discovered_model() {
        let state = UiState {
            connection: Connection::Scanning,
            model: Some(DeviceModel::W820NbDoubleGold),
            ..UiState::default()
        };
        assert_eq!(
            header_lines(&state),
            vec![
                "EDIFIER W820NB Double Gold".to_string(),
                "EDIFIER W820NB Double Gold".to_string(),
                "Scanning for the headset…".to_string(),
            ]
        );
    }

    #[test]
    fn connection_lines_describe_every_state() {
        assert_eq!(Connection::Idle.line(), "Not started");
        assert_eq!(Connection::Scanning.line(), "Scanning for the headset…");
        assert_eq!(Connection::Connecting.line(), "Connecting…");
        assert_eq!(
            Connection::Waiting {
                seconds: 4,
                reason: "device not found".to_string(),
            }
            .line(),
            "Not connected: device not found (retrying in 4s)"
        );
        assert_eq!(
            Connection::Offline {
                reason: "no adapter".to_string(),
            }
            .line(),
            "Not connected: no adapter"
        );
        assert!(Connection::Idle.reason().is_none());
    }

    #[test]
    fn labels_match_the_device_vocabulary() {
        assert_eq!(anc_label(AncMode::Normal), "Off");
        assert_eq!(anc_label(AncMode::NoiseCancelling), "Noise Cancelling");
        assert_eq!(anc_label(AncMode::Ambient), "Transparency");
        assert_eq!(ldac_label(LdacMode::Khz96), "96 kHz");
        assert_eq!(timer_label(ShutdownTimer::Disabled), "Off");
        assert_eq!(timer_label(ShutdownTimer::Minutes(30)), "30 min");
        assert_eq!(ambient_volume_label(-3), "-3");
        assert_eq!(ambient_volume_label(0), "0");
        assert_eq!(ambient_volume_label(2), "+2");
        assert_eq!(prompt_volume_label(0), "Silent");
        assert_eq!(prompt_volume_label(7), "7");
    }

    #[test]
    fn gesture_labels_list_the_cycled_positions() {
        assert_eq!(
            gesture_label(ControlSettings::from_mask(0b101)),
            "Off, Transparency"
        );
        assert_eq!(gesture_label(ControlSettings::default()), "none");
        assert_eq!(
            gesture_label(ControlSettings::all()),
            "Off, Noise Cancelling, Transparency"
        );
    }

    #[test]
    fn gesture_bits_round_trip_and_toggle_one_position() {
        let mut mask = ControlSettings::from_mask(0b111);
        for bit in GestureBit::ALL {
            assert_eq!(GestureBit::from_index(bit.index()), Some(bit));
            assert!(bit.is_enabled(mask));
            mask = bit.toggled(mask);
            assert!(!bit.is_enabled(mask));
        }
        assert_eq!(mask.mask(), 0);
        assert_eq!(GestureBit::from_index(3), None);
    }

    #[test]
    fn shutdown_choices_round_trip() {
        for (index, choice) in SHUTDOWN_CHOICES.iter().enumerate() {
            let timer = shutdown_timer(index).expect("choice exists");
            let expected = match choice {
                None => ShutdownTimer::Disabled,
                Some(minutes) => ShutdownTimer::Minutes(*minutes),
            };
            assert_eq!(timer, expected);
            assert_eq!(shutdown_index(timer), Some(index as isize));
        }
        assert_eq!(shutdown_timer(SHUTDOWN_CHOICES.len()), None);
        assert_eq!(shutdown_index(ShutdownTimer::Minutes(7)), None);
    }

    #[test]
    fn every_menu_action_round_trips_through_its_tag() {
        let mut actions = vec![
            Action::ToggleGameMode,
            Action::Rename,
            Action::AudioToMac,
            Action::Disconnect,
            Action::RePair,
            Action::PowerOff,
            Action::FactoryReset,
            Action::Refresh,
            Action::Quit,
        ];
        actions.extend(
            [AncMode::Normal, AncMode::NoiseCancelling, AncMode::Ambient]
                .into_iter()
                .map(Action::SetAnc),
        );
        actions.extend((AMBIENT_VOLUME_MIN..=AMBIENT_VOLUME_MAX).map(Action::SetAmbientVolume));
        actions.extend(
            [
                SoundEffect::Normal,
                SoundEffect::Pop,
                SoundEffect::Classical,
                SoundEffect::Rock,
            ]
            .into_iter()
            .map(Action::SetSoundEffect),
        );
        actions.extend(
            [LdacMode::Off, LdacMode::Khz48, LdacMode::Khz96]
                .into_iter()
                .map(Action::SetLdac),
        );
        actions.extend(SHUTDOWN_CHOICES.iter().enumerate().map(|(index, _)| {
            Action::SetShutdownTimer(shutdown_timer(index).expect("choice exists"))
        }));
        actions.extend(GestureBit::ALL.into_iter().map(Action::ToggleGesture));
        actions.extend((0..=PROMPT_VOLUME_MAX).map(Action::SetPromptVolume));

        let mut tags: Vec<isize> = Vec::with_capacity(actions.len());
        for action in &actions {
            let tag = action.tag();
            assert!(
                !tags.contains(&tag),
                "tag {tag} reused by {:?}",
                action.label()
            );
            tags.push(tag);
            let decoded = Action::from_tag(tag).expect("tag decodes");
            assert_eq!(&decoded, action, "tag {tag} decoded to the wrong action");
        }

        assert_eq!(Action::from_tag(TAG_ANC - 1), None);
        assert_eq!(Action::from_tag(TAG_PROMPT + 17), None);
        assert_eq!(Action::from_tag(9_999), None);
    }

    #[test]
    fn action_labels_name_the_value_being_set() {
        assert_eq!(
            Action::SetAnc(AncMode::Ambient).label(),
            "set ANC Transparency"
        );
        assert_eq!(Action::SetLdac(LdacMode::Off).label(), "set LDAC Off");
        assert_eq!(Action::Refresh.label(), "refresh status");
        assert_eq!(Action::Rename.label(), "rename the headset");
        assert_eq!(Action::SetName("coach".into()).label(), "rename to coach");
        assert_eq!(Action::SetName("coach".into()).tag(), Action::Rename.tag());
    }

    #[test]
    fn shared_state_is_read_and_edited_under_a_lock() {
        let state = SharedState::new();
        assert_eq!(state.snapshot().connection, Connection::Idle);

        state.update(|state| {
            state.connection = Connection::Connecting;
            state.error = Some("boom".to_string());
        });
        let snapshot = state.snapshot();
        assert_eq!(snapshot.connection, Connection::Connecting);
        assert_eq!(snapshot.error.as_deref(), Some("boom"));
    }

    #[test]
    fn events_fold_into_the_snapshot() {
        let mut state = connected();
        state.apply(&Event::Battery(42));
        state.apply(&Event::Anc {
            mode: AncMode::Ambient,
            ambient_volume: edifier_proto::AmbientVolume(2),
        });
        assert_eq!(state.status.battery, Some(42));
        assert_eq!(state.status.anc, Some(AncMode::Ambient));
        assert_eq!(state.status.ambient_volume, Some(2));
    }

    #[test]
    fn supports_follows_the_model_feature_matrix() {
        assert!(supports(None, Feature::LdacGroup));
        assert!(!supports(
            Some(DeviceModel::W820Nb),
            Feature::SoundEffectGroup
        ));
        assert!(supports(Some(DeviceModel::W820Nb), Feature::LdacGroup));
    }
}
