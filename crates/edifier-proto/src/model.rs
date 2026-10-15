//! Known Edifier headsets and the feature set each one exposes (from mEDIFIER's
//! `deviceinfo.json`).

/// BLE control-service UUIDs are `4809<model>01-1a48-11e9-ab14-d663bd873d93`; the control
/// characteristics are the same prefix with `0001`/`0002` (notify/write).
pub const EDIFIER_SERVICE_SUFFIX: &str = "-1a48-11e9-ab14-d663bd873d93";

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Feature {
    NoiseGroup,
    AmbientSoundGroup,
    SoundEffectGroup,
    ControlSettingsGroup,
    LdacGroup,
    GameMode,
    PromptVolume,
    ShutdownTimer,
    AutoPoweroff,
    Rename,
    Playback,
    /// Switch the *audio* link to another paired host (Android-only in mEDIFIER; on macOS this
    /// maps to an IOBluetooth profile connect).
    HostSwitch,
}

/// A known headset model.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DeviceModel {
    /// W820NB (single gold / regular).
    W820Nb,
    /// W820NB Double Gold.
    W820NbDoubleGold,
    /// W820NB Plus — untested by mEDIFIER; treated as a W820NB.
    W820NbPlus,
    /// W200BT Plus.
    W200BtPlus,
    /// Anything else that answers the protocol.
    Generic,
}

impl DeviceModel {
    /// BLE control service UUID advertised by this model.
    pub fn service_uuid(self) -> &'static str {
        match self {
            DeviceModel::W820Nb | DeviceModel::W820NbPlus => "48093801-1a48-11e9-ab14-d663bd873d93",
            DeviceModel::W820NbDoubleGold => "48097901-1a48-11e9-ab14-d663bd873d93",
            DeviceModel::W200BtPlus => "48092801-1a48-11e9-ab14-d663bd873d93",
            DeviceModel::Generic => "48093801-1a48-11e9-ab14-d663bd873d93",
        }
    }

    pub fn max_name_length(self) -> usize {
        match self {
            DeviceModel::W820NbDoubleGold => 30,
            _ => 24,
        }
    }

    pub fn display_name(self) -> &'static str {
        match self {
            DeviceModel::W820Nb => "EDIFIER W820NB",
            DeviceModel::W820NbPlus => "EDIFIER W820NB Plus",
            DeviceModel::W820NbDoubleGold => "EDIFIER W820NB Double Gold",
            DeviceModel::W200BtPlus => "EDIFIER W200BT Plus",
            DeviceModel::Generic => "Edifier device",
        }
    }

    /// Features hidden by the app for this model; everything else is supported.
    pub fn hidden_features(self) -> &'static [Feature] {
        match self {
            DeviceModel::W820Nb | DeviceModel::W820NbPlus => {
                &[Feature::SoundEffectGroup, Feature::ControlSettingsGroup]
            }
            DeviceModel::W820NbDoubleGold => &[Feature::AutoPoweroff],
            DeviceModel::W200BtPlus => &[
                Feature::NoiseGroup,
                Feature::AmbientSoundGroup,
                Feature::ControlSettingsGroup,
                Feature::LdacGroup,
                Feature::GameMode,
            ],
            DeviceModel::Generic => &[],
        }
    }

    pub fn supports(self, feature: Feature) -> bool {
        !self.hidden_features().contains(&feature)
    }

    /// Best-effort model detection from an advertised BLE service UUID.
    pub fn from_service_uuid(uuid: &str) -> DeviceModel {
        let uuid = uuid.to_ascii_lowercase();
        if uuid.starts_with("48093801") {
            DeviceModel::W820Nb
        } else if uuid.starts_with("48097901") {
            DeviceModel::W820NbDoubleGold
        } else if uuid.starts_with("48092801") {
            DeviceModel::W200BtPlus
        } else {
            // Every other `…-1a48-11e9-ab14-d663bd873d93` service (and anything unrecognised) is
            // controlled by the generic command set.
            DeviceModel::Generic
        }
    }

    /// Model detection from the Bluetooth advertised name.
    pub fn from_name(name: &str) -> DeviceModel {
        let upper = name.to_ascii_uppercase();
        if upper.contains("W200BT") {
            DeviceModel::W200BtPlus
        } else if upper.contains("DOUBLE GOLD") {
            DeviceModel::W820NbDoubleGold
        } else if upper.contains("W820NB PLUS") {
            DeviceModel::W820NbPlus
        } else if upper.contains("W820NB") {
            DeviceModel::W820Nb
        } else {
            DeviceModel::Generic
        }
    }
}

impl Feature {
    pub fn as_str(self) -> &'static str {
        match self {
            Feature::NoiseGroup => "anc",
            Feature::AmbientSoundGroup => "ambient-volume",
            Feature::SoundEffectGroup => "sound-effect",
            Feature::ControlSettingsGroup => "anc-gestures",
            Feature::LdacGroup => "ldac",
            Feature::GameMode => "game-mode",
            Feature::PromptVolume => "prompt-volume",
            Feature::ShutdownTimer => "shutdown-timer",
            Feature::AutoPoweroff => "auto-poweroff",
            Feature::Rename => "rename",
            Feature::Playback => "playback",
            Feature::HostSwitch => "host-switch",
        }
    }
}
