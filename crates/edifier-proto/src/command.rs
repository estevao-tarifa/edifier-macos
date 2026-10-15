//! Every command the W820NB family understands, transcribed from the Edifier Connect captures
//! and mEDIFIER's `basedevice.cpp`.

use crate::{AMBIENT_VOLUME_OFFSET, frame};

/// Active noise control mode (`C1 01|02|03`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AncMode {
    /// ANC off, ambient sound off.
    Normal,
    /// Active noise cancellation.
    NoiseCancelling,
    /// Transparency / ambient sound pass-through.
    Ambient,
}

impl AncMode {
    pub fn as_str(self) -> &'static str {
        match self {
            AncMode::Normal => "normal",
            AncMode::NoiseCancelling => "noise-cancelling",
            AncMode::Ambient => "ambient",
        }
    }
}

/// Sound-effect preset (`C4 00..03`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SoundEffect {
    Normal,
    Pop,
    Classical,
    Rock,
}

impl SoundEffect {
    pub fn code(self) -> u8 {
        match self {
            SoundEffect::Normal => 0x00,
            SoundEffect::Pop => 0x01,
            SoundEffect::Classical => 0x02,
            SoundEffect::Rock => 0x03,
        }
    }

    pub fn from_code(code: u8) -> Option<Self> {
        match code {
            0x00 => Some(SoundEffect::Normal),
            0x01 => Some(SoundEffect::Pop),
            0x02 => Some(SoundEffect::Classical),
            0x03 => Some(SoundEffect::Rock),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            SoundEffect::Normal => "normal",
            SoundEffect::Pop => "pop",
            SoundEffect::Classical => "classical",
            SoundEffect::Rock => "rock",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value.to_ascii_lowercase().as_str() {
            "normal" | "0" => Some(SoundEffect::Normal),
            "pop" | "1" => Some(SoundEffect::Pop),
            "classical" | "2" => Some(SoundEffect::Classical),
            "rock" | "3" => Some(SoundEffect::Rock),
            _ => None,
        }
    }
}

/// LDAC transport mode (`49 00|01|02`). Selecting a non-`Off` mode makes the headset expect an
/// LDAC source, so a source without an LDAC encoder (every macOS release to date) loses audio.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LdacMode {
    Off,
    Khz48,
    Khz96,
}

impl LdacMode {
    pub fn code(self) -> u8 {
        match self {
            LdacMode::Off => 0x00,
            LdacMode::Khz48 => 0x01,
            LdacMode::Khz96 => 0x02,
        }
    }

    pub fn from_code(code: u8) -> Option<Self> {
        match code {
            0x00 => Some(LdacMode::Off),
            0x01 => Some(LdacMode::Khz48),
            0x02 => Some(LdacMode::Khz96),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            LdacMode::Off => "off",
            LdacMode::Khz48 => "48k",
            LdacMode::Khz96 => "96k",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value.to_ascii_lowercase().as_str() {
            "off" | "0" => Some(LdacMode::Off),
            "48k" | "48" | "1" => Some(LdacMode::Khz48),
            "96k" | "96" | "2" => Some(LdacMode::Khz96),
            _ => None,
        }
    }
}

/// Ambient-sound volume, exposed by the firmware as `byte = volume + 6`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub struct AmbientVolume(pub i8);

impl AmbientVolume {
    pub fn new(value: i8) -> Result<Self, String> {
        if crate::AMBIENT_VOLUME_RANGE.contains(&value) {
            Ok(AmbientVolume(value))
        } else {
            Err(format!(
                "ambient volume {value} out of range {}..={}",
                crate::AMBIENT_VOLUME_RANGE.start(),
                crate::AMBIENT_VOLUME_RANGE.end()
            ))
        }
    }

    /// Wire byte sent to the device.
    pub fn wire(self) -> u8 {
        (self.0 + AMBIENT_VOLUME_OFFSET) as u8
    }

    /// Convert the wire byte reported in a `CC 03 CC <mode> <byte>` response.
    pub fn from_wire(byte: u8) -> Self {
        AmbientVolume(byte as i8 - AMBIENT_VOLUME_OFFSET)
    }
}

/// Which physical gestures activate each ANC mode (`F1 0A <mask>`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize)]
pub struct ControlSettings {
    /// The "normal" position is cyclable.
    pub normal: bool,
    /// The "noise cancelling" position is cyclable.
    pub noise_reduction: bool,
    /// The "ambient" position is cyclable.
    pub ambient: bool,
}

impl ControlSettings {
    pub fn mask(self) -> u8 {
        (self.normal as u8) | ((self.noise_reduction as u8) << 1) | ((self.ambient as u8) << 2)
    }

    pub fn from_mask(mask: u8) -> Self {
        ControlSettings {
            normal: mask & 0b001 != 0,
            noise_reduction: mask & 0b010 != 0,
            ambient: mask & 0b100 != 0,
        }
    }

    pub fn all() -> Self {
        ControlSettings::from_mask(0b111)
    }
}

/// On-ear playback buttons, forwarded to the host over AVRCP.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PlaybackAction {
    Play,
    Pause,
    VolumeUp,
    VolumeDown,
    Next,
    Previous,
}

impl PlaybackAction {
    pub fn code(self) -> u8 {
        match self {
            PlaybackAction::Play => 0x00,
            PlaybackAction::Pause => 0x01,
            PlaybackAction::VolumeUp => 0x02,
            PlaybackAction::VolumeDown => 0x03,
            PlaybackAction::Next => 0x04,
            PlaybackAction::Previous => 0x05,
        }
    }
}

/// A request sent to the headset.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    /// Read the ANC mode together with the ambient-sound volume.
    QueryAnc,
    /// Set the ANC mode.
    SetAnc(AncMode),
    /// Set the ambient-sound volume; implicitly switches the device to ambient mode.
    SetAmbientVolume(AmbientVolume),
    /// Query the sound-effect preset.
    QuerySoundEffect,
    /// Set the sound-effect preset.
    SetSoundEffect(SoundEffect),
    /// Query the ANC gesture mapping.
    QueryControlSettings,
    /// Set the ANC gesture mapping.
    SetControlSettings(ControlSettings),
    /// Query the LDAC mode.
    QueryLdac,
    /// Set the LDAC mode.
    SetLdac(LdacMode),
    /// Query the game (low-latency) mode.
    QueryGameMode,
    /// Enable or disable the game mode.
    SetGameMode(bool),
    /// Query the voice-prompt volume.
    QueryPromptVolume,
    /// Set the voice-prompt volume.
    SetPromptVolume(u8),
    /// Query the auto-shutdown timer.
    QueryShutdownTimer,
    /// Set the auto-shutdown timer in minutes, or disable it with `None`.
    SetShutdownTimer(Option<u8>),
    /// Query the auto-power-off-when-idle flag (W820NB Double Gold and later).
    QueryAutoPoweroff,
    /// Set the auto-power-off-when-idle flag.
    SetAutoPoweroff(bool),
    /// Read the battery percentage.
    QueryBattery,
    /// Read the headset's Bluetooth MAC address.
    QueryMac,
    /// Read the firmware version.
    QueryFirmware,
    /// Read the advertised device name.
    QueryName,
    /// Write the advertised device name (max length is model dependent).
    SetName(String),
    /// Read the device fingerprint (model specific blob).
    QueryFingerprint,
    /// Query the playback state.
    QueryPlayback,
    /// Forward a transport action to the connected host.
    Playback(PlaybackAction),
    /// Disconnect the current Bluetooth link.
    Disconnect,
    /// Power the headset off.
    PowerOff,
    /// Re-enter pairing mode.
    RePair,
    /// Factory reset.
    FactoryReset,
    /// Escape hatch: send raw payload bytes (a leading `AA` frame header is not included).
    Raw(Vec<u8>),
}

impl Command {
    /// Payload bytes (command byte first) as they appear inside the frame.
    pub fn payload(&self) -> Vec<u8> {
        match self {
            Command::QueryAnc => vec![0xCC],
            Command::SetAnc(mode) => vec![
                0xC1,
                match mode {
                    AncMode::Normal => 0x01,
                    AncMode::NoiseCancelling => 0x02,
                    AncMode::Ambient => 0x03,
                },
            ],
            Command::SetAmbientVolume(volume) => vec![0xC1, 0x03, volume.wire()],
            Command::QuerySoundEffect => vec![0xD5],
            Command::SetSoundEffect(effect) => vec![0xC4, effect.code()],
            Command::QueryControlSettings => vec![0xF0, 0x0A],
            Command::SetControlSettings(settings) => vec![0xF1, 0x0A, settings.mask()],
            Command::QueryLdac => vec![0x48],
            Command::SetLdac(mode) => vec![0x49, mode.code()],
            Command::QueryGameMode => vec![0x08],
            Command::SetGameMode(enabled) => vec![0x09, *enabled as u8],
            Command::QueryPromptVolume => vec![0x05],
            Command::SetPromptVolume(volume) => vec![0x06, *volume],
            Command::QueryShutdownTimer => vec![0xD3],
            // The `00` here is a payload byte, not padding: the firmware expects a 16-bit timer.
            Command::SetShutdownTimer(Some(minutes)) => vec![0xD1, 0x00, *minutes],
            Command::SetShutdownTimer(None) => vec![0xD2],
            Command::QueryAutoPoweroff => vec![0xD7],
            Command::SetAutoPoweroff(enabled) => vec![0xD6, *enabled as u8],
            Command::QueryBattery => vec![0xD0],
            Command::QueryMac => vec![0xC8],
            Command::QueryFirmware => vec![0xC6],
            Command::QueryName => vec![0xC9],
            Command::SetName(name) => {
                let mut payload = vec![0xCA];
                payload.extend_from_slice(name.as_bytes());
                payload
            }
            Command::QueryFingerprint => vec![0xD8],
            Command::QueryPlayback => vec![0xC3],
            Command::Playback(action) => vec![0xC2, action.code()],
            Command::Disconnect => vec![0xCD],
            Command::PowerOff => vec![0xCE],
            Command::RePair => vec![0xCF],
            Command::FactoryReset => vec![0x07],
            Command::Raw(bytes) => bytes.clone(),
        }
    }

    /// Complete frame ready for the transport.
    pub fn encode(&self) -> Result<Vec<u8>, frame::FrameError> {
        frame::encode(&self.payload())
    }

    /// Human readable label used for logs and MCP/CLI output.
    pub fn label(&self) -> String {
        match self {
            Command::QueryAnc => "query ANC".into(),
            Command::SetAnc(mode) => format!("set ANC {}", mode.as_str()),
            Command::SetAmbientVolume(volume) => format!("set ambient volume {}", volume.0),
            Command::QuerySoundEffect => "query sound effect".into(),
            Command::SetSoundEffect(effect) => format!("set sound effect {}", effect.as_str()),
            Command::QueryControlSettings => "query control settings".into(),
            Command::SetControlSettings(settings) => {
                format!("set control settings mask 0b{:03b}", settings.mask())
            }
            Command::QueryLdac => "query LDAC".into(),
            Command::SetLdac(mode) => format!("set LDAC {}", mode.as_str()),
            Command::QueryGameMode => "query game mode".into(),
            Command::SetGameMode(enabled) => {
                format!("set game mode {}", if *enabled { "on" } else { "off" })
            }
            Command::QueryPromptVolume => "query prompt volume".into(),
            Command::SetPromptVolume(volume) => format!("set prompt volume {volume}"),
            Command::QueryShutdownTimer => "query shutdown timer".into(),
            Command::SetShutdownTimer(Some(minutes)) => format!("set shutdown timer {minutes} min"),
            Command::SetShutdownTimer(None) => "disable shutdown timer".into(),
            Command::QueryAutoPoweroff => "query auto power off".into(),
            Command::SetAutoPoweroff(enabled) => {
                format!("set auto power off {}", if *enabled { "on" } else { "off" })
            }
            Command::QueryBattery => "query battery".into(),
            Command::QueryMac => "query MAC".into(),
            Command::QueryFirmware => "query firmware".into(),
            Command::QueryName => "query name".into(),
            Command::SetName(name) => format!("set name {name:?}"),
            Command::QueryFingerprint => "query fingerprint".into(),
            Command::QueryPlayback => "query playback".into(),
            Command::Playback(action) => format!("playback {action:?}"),
            Command::Disconnect => "disconnect".into(),
            Command::PowerOff => "power off".into(),
            Command::RePair => "re-pair".into(),
            Command::FactoryReset => "factory reset".into(),
            Command::Raw(bytes) => format!(
                "raw {}",
                bytes
                    .iter()
                    .map(|b| format!("{b:02X}"))
                    .collect::<Vec<_>>()
                    .join(" ")
            ),
        }
    }

    /// Commands whose effects can leave the audio link in a broken state (LDAC switching is
    /// rejected by macOS's stack, so it must be surfaced to the user before dispatching).
    pub fn is_destructive(&self) -> bool {
        matches!(
            self,
            Command::SetLdac(LdacMode::Khz48)
                | Command::SetLdac(LdacMode::Khz96)
                | Command::FactoryReset
                | Command::PowerOff
                | Command::RePair
        )
    }
}
