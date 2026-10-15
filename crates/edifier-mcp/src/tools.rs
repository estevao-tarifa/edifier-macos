//! Tool registry: names, JSON schemas and argument decoding.
//!
//! Nothing here touches hardware, so the mapping from MCP arguments to protocol commands is unit
//! tested without a headset.

use anyhow::{Context, Result, anyhow, bail};
use edifier_proto::command::{AmbientVolume, PlaybackAction};
use edifier_proto::event::ShutdownTimer;
use edifier_proto::{AncMode, Command, ControlSettings, LdacMode, SoundEffect};
use serde_json::{Value, json};

/// One MCP tool.
#[derive(Debug, Clone)]
pub struct ToolSpec {
    /// Tool name as advertised to the client.
    pub name: &'static str,
    /// One-paragraph description shown to the model.
    pub description: &'static str,
    /// JSON Schema for the arguments object.
    pub schema: Value,
}

/// What a tool call asks the server to do.
#[derive(Debug, Clone, PartialEq)]
pub enum Action {
    /// List reachable Edifier BLE endpoints.
    Scan {
        /// Scan window in seconds.
        seconds: u64,
    },
    /// Open (or re-open) the control link, optionally switching to another headset.
    Connect {
        /// Name fragment or CoreBluetooth identifier of the headset to control.
        device: Option<String>,
    },
    /// Close the BLE control link without touching the audio link.
    CloseLink,
    /// Read the full status.
    Status {
        /// Query the device instead of answering from the cached snapshot.
        refresh: bool,
    },
    /// Ask macOS to move the classic audio link to this Mac.
    ConnectAudio {
        /// Name or address fragment of the paired device.
        device: Option<String>,
    },
    /// Send one protocol command.
    Command(Box<Command>),
    /// Send raw payload bytes.
    Raw(Vec<u8>),
}

/// Every tool this server exposes.
#[must_use]
pub fn specs() -> Vec<ToolSpec> {
    vec![
        ToolSpec {
            name: "edifier_scan",
            description: "List Edifier headset control endpoints (BLE) currently reachable from this \
                          Mac, with their CoreBluetooth identifier, name, RSSI and detected model. \
                          Use the identifier with edifier_connect when several headsets are present.",
            schema: json!({
                "type": "object",
                "properties": {
                    "seconds": {"type": "integer", "minimum": 1, "maximum": 30,
                                "description": "Scan window in seconds (default 6)"}
                }
            }),
        },
        ToolSpec {
            name: "edifier_connect",
            description: "Open the BLE control link to the headset (or return the already open one) \
                          and report battery, ANC mode, firmware and every other supported value. \
                          Call this before any other control tool.",
            schema: json!({
                "type": "object",
                "properties": {
                    "device": {"type": "string",
                               "description": "Name fragment or CoreBluetooth identifier (36-character UUID). \
                                               Omit to use the first Edifier endpoint found."}
                }
            }),
        },
        ToolSpec {
            name: "edifier_status",
            description: "Report the headset state. With refresh=true every value is queried from \
                          the device (battery, ANC mode, ambient volume, EQ, game mode, LDAC, prompt \
                          volume, shutdown timer, ANC gestures, name, MAC, firmware); with \
                          refresh=false the cached snapshot from notifications is returned.",
            schema: json!({
                "type": "object",
                "properties": {
                    "refresh": {"type": "boolean", "description": "Query the device (default true)"}
                }
            }),
        },
        ToolSpec {
            name: "edifier_anc",
            description: "Switch noise control: `off` disables ANC and transparency, `on` enables \
                          active noise cancellation, `ambient` enables transparency (ambient sound). \
                          This is the same mode the on-ear button cycles.",
            schema: json!({
                "type": "object",
                "properties": {
                    "mode": {"type": "string", "enum": ["off", "on", "ambient"],
                             "description": "Noise control mode"}
                },
                "required": ["mode"]
            }),
        },
        ToolSpec {
            name: "edifier_ambient_volume",
            description: "Set the transparency (ambient sound) volume from -3 to 3. The headset \
                          switches to ambient mode as a side effect of this command.",
            schema: json!({
                "type": "object",
                "properties": {
                    "level": {"type": "integer", "minimum": -3, "maximum": 3}
                },
                "required": ["level"]
            }),
        },
        ToolSpec {
            name: "edifier_eq",
            description: "Select the sound-effect preset: normal, pop, classical or rock.",
            schema: json!({
                "type": "object",
                "properties": {
                    "preset": {"type": "string", "enum": ["normal", "pop", "classical", "rock"]}
                },
                "required": ["preset"]
            }),
        },
        ToolSpec {
            name: "edifier_game_mode",
            description: "Enable or disable the low-latency game mode, which reduces audio latency \
                          at the cost of range and battery life.",
            schema: json!({
                "type": "object",
                "properties": {"enabled": {"type": "boolean"}},
                "required": ["enabled"]
            }),
        },
        ToolSpec {
            name: "edifier_ldac",
            description: "Select the headset's LDAC mode (`off`, `48k`, `96k`). Warning: macOS has \
                          no LDAC encoder, so any mode other than `off` stops the audio link until it \
                          is set back to `off`; the BLE control link keeps working, so this is \
                          recoverable. Non-off modes require confirm=true.",
            schema: json!({
                "type": "object",
                "properties": {
                    "mode": {"type": "string", "enum": ["off", "48k", "96k"]},
                    "confirm": {"type": "boolean",
                                "description": "Acknowledge the audio interruption (required for 48k/96k)"}
                },
                "required": ["mode"]
            }),
        },
        ToolSpec {
            name: "edifier_prompt_volume",
            description: "Set the volume of the headset's spoken prompts, 0 (silent) to 15.",
            schema: json!({
                "type": "object",
                "properties": {"level": {"type": "integer", "minimum": 0, "maximum": 15}},
                "required": ["level"]
            }),
        },
        ToolSpec {
            name: "edifier_gestures",
            description: "Choose which ANC modes the headset's on-ear gestures are allowed to cycle: \
                          normal, noise_reduction and ambient are independent switches.",
            schema: json!({
                "type": "object",
                "properties": {
                    "normal": {"type": "boolean"},
                    "noise_reduction": {"type": "boolean"},
                    "ambient": {"type": "boolean"}
                }
            }),
        },
        ToolSpec {
            name: "edifier_shutdown_timer",
            description: "Set the auto-shutdown timer in minutes (0..=180), or pass null to disable it.",
            schema: json!({
                "type": "object",
                "properties": {
                    "minutes": {"type": ["integer", "null"], "minimum": 0, "maximum": 180}
                },
                "required": ["minutes"]
            }),
        },
        ToolSpec {
            name: "edifier_rename",
            description: "Rename the headset (the Bluetooth name other devices see). Long names are \
                          truncated to the model's limit (24 characters on a W820NB, 30 on the \
                          Double Gold).",
            schema: json!({
                "type": "object",
                "properties": {"name": {"type": "string", "minLength": 1}},
                "required": ["name"]
            }),
        },
        ToolSpec {
            name: "edifier_playback",
            description: "Forward a transport control action (play, pause, volume_up, volume_down, \
                          next, previous) to the host the headset is currently playing from.",
            schema: json!({
                "type": "object",
                "properties": {
                    "action": {"type": "string",
                               "enum": ["play", "pause", "volume_up", "volume_down", "next", "previous"]}
                },
                "required": ["action"]
            }),
        },
        ToolSpec {
            name: "edifier_raw",
            description: "Send a raw protocol payload over the control link, for experimenting with \
                          commands this server does not model (the frame header and checksum are \
                          added automatically). Example: \"C1 02\" selects noise cancelling.",
            schema: json!({
                "type": "object",
                "properties": {
                    "payload_hex": {"type": "string", "description": "Hex bytes, e.g. \"F0 0A\""}
                },
                "required": ["payload_hex"]
            }),
        },
        ToolSpec {
            name: "edifier_connect_audio",
            description: "Ask macOS to move the classic Bluetooth *audio* link back to this Mac, \
                          which is how playback is taken over from a phone. Requires the headset to \
                          be paired with this Mac; independent of the BLE control link.",
            schema: json!({
                "type": "object",
                "properties": {
                    "device": {"type": "string",
                               "description": "Name or address fragment of the paired device"}
                }
            }),
        },
        ToolSpec {
            name: "edifier_disconnect",
            description: "Close the BLE control link. The audio link, if any, is left untouched.",
            schema: json!({"type": "object", "properties": {}}),
        },
        ToolSpec {
            name: "edifier_destructive",
            description: "Run one of the irreversible headset actions: `power_off` switches the \
                          headset off, `re_pair` puts it into pairing mode (dropping the current \
                          connection), `factory_reset` restores factory settings. All three require \
                          confirm=true.",
            schema: json!({
                "type": "object",
                "properties": {
                    "action": {"type": "string", "enum": ["power_off", "re_pair", "factory_reset"]},
                    "confirm": {"type": "boolean",
                                "description": "Acknowledge that the action interrupts or resets the headset"}
                },
                "required": ["action", "confirm"]
            }),
        },
    ]
}

/// Decode a tool call into an [`Action`].
///
/// # Errors
///
/// Returns a message describing the missing or invalid argument, or an unknown tool name.
pub fn resolve(name: &str, args: &Value) -> Result<Action> {
    let object = args.as_object().cloned().unwrap_or_default();
    let get = |key: &str| object.get(key);

    match name {
        "edifier_scan" => {
            let seconds = match get("seconds") {
                Some(value) => require_u64(Some(value), "seconds")?,
                None => 6,
            };
            Ok(Action::Scan { seconds })
        }
        "edifier_connect" => Ok(Action::Connect {
            device: optional_string(get("device"))?,
        }),
        "edifier_status" => {
            let refresh = match get("refresh") {
                Some(value) => require_bool(Some(value), "refresh")?,
                None => true,
            };
            Ok(Action::Status { refresh })
        }
        "edifier_anc" => {
            let mode = require_string(get("mode"), "mode")?;
            let mode = match mode.to_lowercase().as_str() {
                "off" | "normal" => AncMode::Normal,
                "on" | "noise_cancelling" | "noise-cancelling" | "anc" => AncMode::NoiseCancelling,
                "ambient" | "transparency" => AncMode::Ambient,
                other => bail!("unknown mode {other:?}: expected off, on or ambient"),
            };
            Ok(Action::Command(Box::new(Command::SetAnc(mode))))
        }
        "edifier_ambient_volume" => {
            let level = require_i64(get("level"), "level")?;
            let level = i8::try_from(level).context("level out of range")?;
            let volume = AmbientVolume::new(level).map_err(|err| anyhow!("{err}"))?;
            Ok(Action::Command(Box::new(Command::SetAmbientVolume(volume))))
        }
        "edifier_eq" => {
            let preset = require_string(get("preset"), "preset")?;
            let preset = SoundEffect::parse(&preset).ok_or_else(|| {
                anyhow!("unknown preset {preset:?}: expected normal, pop, classical or rock")
            })?;
            Ok(Action::Command(Box::new(Command::SetSoundEffect(preset))))
        }
        "edifier_game_mode" => {
            let enabled = require_bool(get("enabled"), "enabled")?;
            Ok(Action::Command(Box::new(Command::SetGameMode(enabled))))
        }
        "edifier_ldac" => {
            let mode = require_string(get("mode"), "mode")?;
            let mode = LdacMode::parse(&mode)
                .ok_or_else(|| anyhow!("unknown mode {mode:?}: expected off, 48k or 96k"))?;
            let confirm = match get("confirm") {
                Some(value) => require_bool(Some(value), "confirm")?,
                None => false,
            };
            if mode != LdacMode::Off && !confirm {
                bail!(
                    "refusing to switch LDAC to {} without confirm=true: macOS has no LDAC encoder, \
                     so audio stops until the mode is set back to off",
                    mode.as_str()
                );
            }
            Ok(Action::Command(Box::new(Command::SetLdac(mode))))
        }
        "edifier_prompt_volume" => {
            let level = require_i64(get("level"), "level")?;
            let level = u8::try_from(level).context("level out of range")?;
            if !edifier_proto::PROMPT_VOLUME_RANGE.contains(&level) {
                bail!("prompt volume must be 0..=15, got {level}");
            }
            Ok(Action::Command(Box::new(Command::SetPromptVolume(level))))
        }
        "edifier_gestures" => {
            let mut settings = ControlSettings::default();
            for (key, field) in [
                ("normal", &mut settings.normal),
                ("noise_reduction", &mut settings.noise_reduction),
                ("ambient", &mut settings.ambient),
            ] {
                if let Some(value) = get(key) {
                    *field = require_bool(Some(value), key)?;
                }
            }
            Ok(Action::Command(Box::new(Command::SetControlSettings(
                settings,
            ))))
        }
        "edifier_shutdown_timer" => {
            let Some(value) = get("minutes") else {
                bail!("missing argument `minutes`");
            };
            let timer = if value.is_null() {
                ShutdownTimer::Disabled
            } else {
                let minutes = require_i64(Some(value), "minutes")?;
                let minutes = u8::try_from(minutes).context("minutes out of range")?;
                if !edifier_proto::SHUTDOWN_TIMER_RANGE.contains(&minutes) {
                    bail!("shutdown timer must be 0..=180 minutes, got {minutes}");
                }
                if minutes == 0 {
                    ShutdownTimer::Disabled
                } else {
                    ShutdownTimer::Minutes(minutes)
                }
            };
            let command = match timer {
                ShutdownTimer::Disabled => Command::SetShutdownTimer(None),
                ShutdownTimer::Minutes(minutes) => Command::SetShutdownTimer(Some(minutes)),
            };
            Ok(Action::Command(Box::new(command)))
        }
        "edifier_rename" => {
            let name = require_string(get("name"), "name")?;
            if name.trim().is_empty() {
                bail!("name must not be empty");
            }
            Ok(Action::Command(Box::new(Command::SetName(name))))
        }
        "edifier_playback" => {
            let action = require_string(get("action"), "action")?;
            let action = match action.to_lowercase().as_str() {
                "play" => PlaybackAction::Play,
                "pause" => PlaybackAction::Pause,
                "volume_up" => PlaybackAction::VolumeUp,
                "volume_down" => PlaybackAction::VolumeDown,
                "next" => PlaybackAction::Next,
                "previous" => PlaybackAction::Previous,
                other => bail!("unknown playback action {other:?}"),
            };
            Ok(Action::Command(Box::new(Command::Playback(action))))
        }
        "edifier_raw" => {
            let text = require_string(get("payload_hex"), "payload_hex")?;
            Ok(Action::Raw(parse_hex(&text)?))
        }
        "edifier_connect_audio" => Ok(Action::ConnectAudio {
            device: optional_string(get("device"))?,
        }),
        "edifier_disconnect" => Ok(Action::CloseLink),
        "edifier_destructive" => {
            let action = require_string(get("action"), "action")?;
            let confirm = require_bool(get("confirm"), "confirm")?;
            if !confirm {
                bail!("refusing to run {action} without confirm=true");
            }
            let command = match action.to_lowercase().as_str() {
                "power_off" => Command::PowerOff,
                "re_pair" => Command::RePair,
                "factory_reset" => Command::FactoryReset,
                other => {
                    bail!("unknown action {other:?}: expected power_off, re_pair or factory_reset")
                }
            };
            Ok(Action::Command(Box::new(command)))
        }
        other => bail!("unknown tool {other:?}"),
    }
}

/// Parse a hex byte string such as `C1 02` or `c1,02`.
///
/// # Errors
///
/// Fails when a token is not exactly one byte of hex or when the payload is empty.
pub fn parse_hex(text: &str) -> Result<Vec<u8>> {
    let tokens: Vec<&str> = text
        .split(|character: char| character.is_whitespace() || character == ',' || character == ':')
        .filter(|token| !token.is_empty())
        .collect();
    if tokens.is_empty() {
        bail!("payload must contain at least one hex byte");
    }
    let mut bytes = Vec::with_capacity(tokens.len());
    for token in tokens {
        let cleaned = token.trim_start_matches("0x").trim_start_matches("0X");
        bytes.push(
            u8::from_str_radix(cleaned, 16)
                .with_context(|| format!("{token:?} is not a hex byte"))?,
        );
    }
    Ok(bytes)
}

fn require_string(value: Option<&Value>, key: &str) -> Result<String> {
    match value {
        Some(Value::String(text)) => Ok(text.clone()),
        Some(other) => bail!("argument `{key}` must be a string, got {other}"),
        None => bail!("missing argument `{key}`"),
    }
}

fn optional_string(value: Option<&Value>) -> Result<Option<String>> {
    match value {
        Some(Value::String(text)) if !text.trim().is_empty() => Ok(Some(text.clone())),
        Some(Value::String(_)) | None => Ok(None),
        Some(other) => bail!("expected a string, got {other}"),
    }
}

fn require_bool(value: Option<&Value>, key: &str) -> Result<bool> {
    match value {
        Some(value) => value
            .as_bool()
            .ok_or_else(|| anyhow!("argument `{key}` must be a boolean, got {value}")),
        None => bail!("missing argument `{key}`"),
    }
}

fn require_i64(value: Option<&Value>, key: &str) -> Result<i64> {
    match value {
        Some(value) => value
            .as_i64()
            .ok_or_else(|| anyhow!("argument `{key}` must be an integer, got {value}")),
        None => bail!("missing argument `{key}`"),
    }
}

fn require_u64(value: Option<&Value>, key: &str) -> Result<u64> {
    match value {
        Some(value) => value
            .as_u64()
            .ok_or_else(|| anyhow!("argument `{key}` must be a non-negative integer, got {value}")),
        None => bail!("missing argument `{key}`"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_advertised_tool_resolves_with_plausible_arguments() {
        let calls = [
            ("edifier_scan", json!({})),
            ("edifier_connect", json!({})),
            ("edifier_connect", json!({"device": "W820NB"})),
            ("edifier_status", json!({})),
            ("edifier_anc", json!({"mode": "on"})),
            ("edifier_ambient_volume", json!({"level": -2})),
            ("edifier_eq", json!({"preset": "rock"})),
            ("edifier_game_mode", json!({"enabled": true})),
            ("edifier_ldac", json!({"mode": "off"})),
            ("edifier_prompt_volume", json!({"level": 5})),
            ("edifier_gestures", json!({"normal": true, "ambient": true})),
            ("edifier_shutdown_timer", json!({"minutes": 30})),
            ("edifier_shutdown_timer", json!({"minutes": null})),
            ("edifier_rename", json!({"name": "Shield"})),
            ("edifier_playback", json!({"action": "next"})),
            ("edifier_raw", json!({"payload_hex": "F0 0A"})),
            ("edifier_connect_audio", json!({})),
            ("edifier_disconnect", json!({})),
            (
                "edifier_destructive",
                json!({"action": "power_off", "confirm": true}),
            ),
        ];
        let advertised: Vec<&str> = specs().iter().map(|spec| spec.name).collect();
        for (name, args) in calls {
            assert!(advertised.contains(&name), "{name} is not advertised");
            resolve(name, &args).unwrap_or_else(|err| panic!("{name}/{args}: {err}"));
        }
    }

    #[test]
    fn anc_modes_map_to_protocol_commands() {
        let command = resolve("edifier_anc", &json!({"mode": "ambient"})).unwrap();
        assert_eq!(
            command,
            Action::Command(Box::new(Command::SetAnc(AncMode::Ambient)))
        );
        assert_eq!(
            resolve("edifier_anc", &json!({"mode": "noise-cancelling"})).unwrap(),
            Action::Command(Box::new(Command::SetAnc(AncMode::NoiseCancelling)))
        );
        assert!(resolve("edifier_anc", &json!({"mode": "loud"})).is_err());
        assert!(resolve("edifier_anc", &json!({})).is_err());
    }

    #[test]
    fn destructive_and_lossy_actions_require_confirmation() {
        assert!(resolve("edifier_ldac", &json!({"mode": "96k"})).is_err());
        assert!(resolve("edifier_ldac", &json!({"mode": "96k", "confirm": true})).is_ok());
        assert!(resolve("edifier_ldac", &json!({"mode": "off"})).is_ok());
        assert!(
            resolve(
                "edifier_destructive",
                &json!({"action": "factory_reset", "confirm": false})
            )
            .is_err()
        );
        assert!(
            resolve(
                "edifier_destructive",
                &json!({"action": "factory_reset", "confirm": true})
            )
            .is_ok()
        );
    }

    #[test]
    fn argument_validation_rejects_out_of_range_values() {
        assert!(resolve("edifier_ambient_volume", &json!({"level": 4})).is_err());
        assert!(resolve("edifier_prompt_volume", &json!({"level": 16})).is_err());
        assert!(resolve("edifier_shutdown_timer", &json!({"minutes": 181})).is_err());
        assert!(resolve("edifier_rename", &json!({"name": "   "})).is_err());
        assert!(resolve("edifier_raw", &json!({"payload_hex": "ZZ"})).is_err());
        assert!(resolve("edifier_nope", &json!({})).is_err());
    }

    #[test]
    fn hex_payloads_accept_common_separators() {
        assert_eq!(parse_hex("F0 0A").unwrap(), vec![0xF0, 0x0A]);
        assert_eq!(parse_hex("f0,0a").unwrap(), vec![0xF0, 0x0A]);
        assert_eq!(parse_hex("0xF0:0x0A").unwrap(), vec![0xF0, 0x0A]);
        assert!(parse_hex("").is_err());
    }

    #[test]
    fn tool_schemas_are_valid_objects_with_unique_names() {
        let specs = specs();
        let mut names: Vec<&str> = specs.iter().map(|spec| spec.name).collect();
        let total = names.len();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), total, "tool names must be unique");
        for spec in &specs {
            assert_eq!(
                spec.schema["type"], "object",
                "{} must take an object",
                spec.name
            );
            assert!(
                !spec.description.trim().is_empty(),
                "{} needs a description",
                spec.name
            );
        }
    }
}
