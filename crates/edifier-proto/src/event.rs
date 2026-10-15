//! Parsing of device -> host frames into typed events, and the aggregated status snapshot the
//! CLI, menu bar app and MCP server all render.

use crate::command::{AmbientVolume, AncMode, ControlSettings, LdacMode, SoundEffect};
use crate::frame::{Frame, HEAD_ACK, HEAD_STATE, Head};
use crate::model::DeviceModel;

/// Auto-shutdown timer state as reported by `D3`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ShutdownTimer {
    Disabled,
    Minutes(u8),
}

/// A decoded device notification.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    /// `BB 03 CC <mode> <volume>`
    Anc {
        mode: AncMode,
        ambient_volume: AmbientVolume,
    },
    /// `BB 02 D0 <percent>`
    Battery(u8),
    /// `BB 07 C8 <6 bytes>`
    Mac([u8; 6]),
    /// `BB 04 C6 <3 bytes>`
    Firmware(String),
    /// `BB .. C9 <utf8>`
    Name(String),
    /// `BB 02 D5 <preset>`
    SoundEffect(SoundEffect),
    /// `BB 02 08 <flag>`
    GameMode(bool),
    /// `BB 02 48 <mode>`
    Ldac(LdacMode),
    /// `BB 02 05 <volume>`
    PromptVolume(u8),
    /// `BB 02 D3 <flag>` / `BB 03 D3 <minutes>`
    ShutdownTimer(ShutdownTimer),
    /// `BB 02 D7 <flag>`
    AutoPoweroff(bool),
    /// `BB 03 F0 0A <mask>`
    ControlSettings(ControlSettings),
    /// `BB 02 C3 <state>`
    PlaybackState(u8),
    /// `BB 16 D8 <20 bytes>`
    Fingerprint(String),
    /// `CC <len> <cmd> <status>` — acknowledgement, the rest of the payload echoes the request.
    Ack { command: u8, status: Vec<u8> },
    /// Anything not covered above.
    Unknown { head: Head, payload: Vec<u8> },
}

impl Event {
    /// Decode a validated frame.
    pub fn parse(frame: &Frame) -> Event {
        let payload = frame.payload.as_slice();
        let Some(&command) = payload.first() else {
            return Event::Unknown {
                head: frame.head,
                payload: payload.to_vec(),
            };
        };
        let params = &payload[1..];

        match frame.head {
            Head::Ack => Event::Ack {
                command,
                status: params.to_vec(),
            },
            Head::Host => Event::Unknown {
                head: frame.head,
                payload: payload.to_vec(),
            },
            Head::State => match (command, params) {
                (0xD0, [percent]) => Event::Battery(*percent),
                (0xD5, [preset]) => SoundEffect::from_code(*preset)
                    .map(Event::SoundEffect)
                    .unwrap_or(Event::Unknown {
                        head: frame.head,
                        payload: payload.to_vec(),
                    }),
                (0x08 | 0x09, [flag]) => Event::GameMode(*flag != 0),
                (0x48 | 0x49, [mode]) => {
                    LdacMode::from_code(*mode)
                        .map(Event::Ldac)
                        .unwrap_or(Event::Unknown {
                            head: frame.head,
                            payload: payload.to_vec(),
                        })
                }
                (0x05 | 0x06, [volume]) => Event::PromptVolume(*volume),
                (0xD7 | 0xD6, [flag]) => Event::AutoPoweroff(*flag != 0),
                (0xD3, [flag]) => Event::ShutdownTimer(if *flag == 0 {
                    ShutdownTimer::Disabled
                } else {
                    ShutdownTimer::Minutes(*flag)
                }),
                (0xD3, [0x00, minutes]) => Event::ShutdownTimer(ShutdownTimer::Minutes(*minutes)),
                // Replies to `C1 0x` echo the mode and the ambient-sound volume back, so both a
                // query (`CC`) and a set (`C1`) settle the ANC state.
                (0xC1, [mode, volume]) | (0xCC, [mode, volume]) => match mode {
                    0x01 => Event::Anc {
                        mode: AncMode::Normal,
                        ambient_volume: AmbientVolume::from_wire(*volume),
                    },
                    0x02 => Event::Anc {
                        mode: AncMode::NoiseCancelling,
                        ambient_volume: AmbientVolume::from_wire(*volume),
                    },
                    0x03 => Event::Anc {
                        mode: AncMode::Ambient,
                        ambient_volume: AmbientVolume::from_wire(*volume),
                    },
                    _ => Event::Unknown {
                        head: frame.head,
                        payload: payload.to_vec(),
                    },
                },
                (0xC8, mac @ [_, _, _, _, _, _]) => {
                    let mut bytes = [0u8; 6];
                    bytes.copy_from_slice(mac);
                    Event::Mac(bytes)
                }
                (0xC6, [_, _, _]) => Event::Firmware(hex_dotted(params)),
                (0xC9, name) if !name.is_empty() => Event::Name(
                    String::from_utf8_lossy(name)
                        .trim_end_matches('\0')
                        .to_string(),
                ),
                (0xF0 | 0xF1, [0x0A, mask]) => {
                    Event::ControlSettings(ControlSettings::from_mask(*mask))
                }
                (0xC3, [state, ..]) => Event::PlaybackState(*state),
                (0xD8, rest) if !rest.is_empty() => Event::Fingerprint(hex(rest)),
                _ => Event::Unknown {
                    head: frame.head,
                    payload: payload.to_vec(),
                },
            },
        }
    }

    /// Fold this event into a status snapshot.
    pub fn apply(&self, status: &mut Status) {
        match self {
            Event::Anc {
                mode,
                ambient_volume,
            } => {
                status.anc = Some(*mode);
                status.ambient_volume = Some(ambient_volume.0);
            }
            Event::Battery(percent) => status.battery = Some(*percent),
            Event::Mac(bytes) => status.mac = Some(format_mac(bytes)),
            Event::Firmware(version) => status.firmware = Some(version.clone()),
            Event::Name(name) => status.name = Some(name.clone()),
            Event::SoundEffect(effect) => status.sound_effect = Some(*effect),
            Event::GameMode(enabled) => status.game_mode = Some(*enabled),
            Event::Ldac(mode) => status.ldac = Some(*mode),
            Event::PromptVolume(volume) => status.prompt_volume = Some(*volume),
            Event::ShutdownTimer(timer) => status.shutdown_timer = Some(*timer),
            Event::AutoPoweroff(enabled) => status.auto_poweroff = Some(*enabled),
            Event::ControlSettings(settings) => status.anc_gestures = Some(*settings),
            Event::PlaybackState(state) => status.playback_state = Some(*state),
            Event::Fingerprint(blob) => status.fingerprint = Some(blob.clone()),
            Event::Ack { .. } | Event::Unknown { .. } => {}
        }
    }

    /// Whether this event settles a pending request for `command`.
    ///
    /// A request is settled either by an acknowledgement echoing its own command byte (the shape
    /// seen in the captures, e.g. `sendData:AA02C4002189 → readData:CC02C40021AB`) or by the
    /// feature's state frame, which firmware answers with the *query* byte of that feature
    /// (`C4` sound effect → `D5`, `CA` name → `C9`, `49` LDAC → `48`, `09` game mode → `08`,
    /// `D1`/`D2` shutdown timer → `D3`, `F1` gestures → `F0`). Both spellings are accepted so a
    /// setter cannot hang on firmware that replies with the query byte.
    pub fn answers(&self, command: u8) -> bool {
        match self {
            Event::Anc { .. } => matches!(command, 0xCC | 0xC1),
            Event::Battery(_) => command == 0xD0,
            Event::Mac(_) => command == 0xC8,
            Event::Firmware(_) => command == 0xC6,
            Event::Name(_) => matches!(command, 0xC9 | 0xCA),
            Event::SoundEffect(_) => matches!(command, 0xD5 | 0xC4),
            Event::GameMode(_) => matches!(command, 0x08 | 0x09),
            Event::Ldac(_) => matches!(command, 0x48 | 0x49),
            Event::PromptVolume(_) => matches!(command, 0x05 | 0x06),
            Event::ShutdownTimer(_) => matches!(command, 0xD1..=0xD3),
            Event::AutoPoweroff(_) => matches!(command, 0xD7 | 0xD6),
            Event::ControlSettings(_) => matches!(command, 0xF0 | 0xF1),
            Event::PlaybackState(_) => matches!(command, 0xC3 | 0xC2),
            Event::Fingerprint(_) => command == 0xD8,
            Event::Ack { command: ack, .. } => *ack == command,
            Event::Unknown { .. } => false,
        }
    }
}

impl std::fmt::Display for Event {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Event::Anc {
                mode,
                ambient_volume,
            } => write!(
                f,
                "anc={} ambient_volume={}",
                mode.as_str(),
                ambient_volume.0
            ),
            Event::Battery(percent) => write!(f, "battery={percent}%"),
            Event::Mac(bytes) => write!(f, "mac={}", format_mac(bytes)),
            Event::Firmware(version) => write!(f, "firmware={version}"),
            Event::Name(name) => write!(f, "name={name}"),
            Event::SoundEffect(effect) => write!(f, "sound_effect={}", effect.as_str()),
            Event::GameMode(enabled) => write!(f, "game_mode={enabled}"),
            Event::Ldac(mode) => write!(f, "ldac={}", mode.as_str()),
            Event::PromptVolume(volume) => write!(f, "prompt_volume={volume}"),
            Event::ShutdownTimer(ShutdownTimer::Disabled) => write!(f, "shutdown_timer=disabled"),
            Event::ShutdownTimer(ShutdownTimer::Minutes(minutes)) => {
                write!(f, "shutdown_timer={minutes}min")
            }
            Event::AutoPoweroff(enabled) => write!(f, "auto_poweroff={enabled}"),
            Event::ControlSettings(settings) => write!(f, "anc_gestures=0b{:03b}", settings.mask()),
            Event::PlaybackState(state) => write!(f, "playback_state=0x{state:02X}"),
            Event::Fingerprint(blob) => write!(f, "fingerprint={blob}"),
            Event::Ack { command, status } => {
                write!(f, "ack cmd=0x{command:02X} status={}", hex(status))
            }
            Event::Unknown { head, payload } => {
                write!(
                    f,
                    "unknown head=0x{:02X} payload={}",
                    head.as_byte(),
                    hex(payload)
                )
            }
        }
    }
}

/// Everything we know about a headset at a point in time. Unqueried fields stay `None` so a
/// snapshot can be diffed before and after a refresh.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize)]
pub struct Status {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<DeviceModel>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub battery: Option<u8>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mac: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub firmware: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub anc: Option<AncMode>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ambient_volume: Option<i8>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sound_effect: Option<SoundEffect>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub game_mode: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ldac: Option<LdacMode>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prompt_volume: Option<u8>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub shutdown_timer: Option<ShutdownTimer>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub auto_poweroff: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub anc_gestures: Option<ControlSettings>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub playback_state: Option<u8>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fingerprint: Option<String>,
}

impl Status {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn merge(&mut self, other: Status) {
        macro_rules! take {
            ($($field:ident),* $(,)?) => {
                $(if other.$field.is_some() { self.$field = other.$field; })*
            };
        }
        take!(
            model,
            name,
            battery,
            mac,
            firmware,
            anc,
            ambient_volume,
            sound_effect,
            game_mode,
            ldac,
            prompt_volume,
            shutdown_timer,
            auto_poweroff,
            anc_gestures,
            playback_state,
            fingerprint,
        );
    }

    /// One-line rendering used by the menu bar title and CLI tables.
    pub fn summary(&self) -> String {
        let mut parts = Vec::new();
        if let Some(battery) = self.battery {
            parts.push(format!("{battery}%"));
        }
        if let Some(anc) = self.anc {
            parts.push(
                match anc {
                    AncMode::Normal => "ANC off",
                    AncMode::NoiseCancelling => "ANC on",
                    AncMode::Ambient => "Ambient",
                }
                .to_string(),
            );
        }
        if let Some(ldac) = self.ldac
            && ldac != LdacMode::Off
        {
            parts.push(format!("LDAC {}", ldac.as_str()));
        }
        if self.game_mode == Some(true) {
            parts.push("game mode".into());
        }
        parts.join(" · ")
    }
}

pub fn format_mac(bytes: &[u8; 6]) -> String {
    bytes
        .iter()
        .map(|b| format!("{b:02X}"))
        .collect::<Vec<_>>()
        .join(":")
}

fn hex(bytes: &[u8]) -> String {
    bytes
        .iter()
        .map(|b| format!("{b:02X}"))
        .collect::<Vec<_>>()
        .join("")
}

fn hex_dotted(bytes: &[u8]) -> String {
    bytes
        .iter()
        .map(|b| format!("{b:02X}"))
        .collect::<Vec<_>>()
        .join(".")
}

/// Frame header bytes re-exported for tests and transports.
pub const STATE_HEAD: u8 = HEAD_STATE;
pub const ACK_HEAD: u8 = HEAD_ACK;
