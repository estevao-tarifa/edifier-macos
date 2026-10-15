//! Command line front end for Edifier headsets.
//!
//! The binary (`src/main.rs`) only parses arguments and renders a [`Reply`]; every command is
//! implemented here so the behaviour is unit-testable without a headset.

use std::time::Duration;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand, ValueEnum};
use edifier_link::Session;
use edifier_link::ble::{self, Discovered, Target};
use edifier_macos::connect_paired_audio;
use edifier_proto::command::{AmbientVolume, PlaybackAction};
use edifier_proto::event::ShutdownTimer;
use edifier_proto::{
    AncMode, Command, ControlSettings, DeviceModel, LdacMode, SoundEffect, Status,
};
use serde::Serialize;
use tracing::{info, warn};

/// `edifierctl` — control an Edifier headset from the command line.
#[derive(Debug, Parser)]
#[command(
    name = "edifierctl",
    version,
    about = "Control Edifier headsets (W820NB family) over BLE",
    long_about = None
)]
pub struct Cli {
    /// Peripheral to talk to: a CoreBluetooth identifier (36-character UUID) or a name fragment.
    /// Omitted: the first Edifier endpoint found.
    #[arg(long, global = true, env = "EDIFIER_DEVICE")]
    pub device: Option<String>,

    /// Seconds to look for the device before giving up.
    #[arg(long, global = true, default_value_t = 8)]
    pub scan_seconds: u64,

    /// Milliseconds to wait for the device to answer one command. Some commands (LDAC on models
    /// that lack it) are simply never answered, so this bounds the wait.
    #[arg(long, global = true, default_value_t = 3000)]
    pub timeout_ms: u64,

    /// Print machine-readable JSON instead of a table.
    #[arg(long, global = true)]
    pub json: bool,

    /// Increase log verbosity (repeatable).
    #[arg(short, long, global = true, action = clap::ArgAction::Count)]
    pub verbose: u8,

    #[command(subcommand)]
    pub command: CommandLine,
}

/// Every supported operation.
#[derive(Debug, Subcommand)]
pub enum CommandLine {
    /// List Edifier endpoints currently in range.
    Scan,
    /// Read battery, firmware, ANC mode and every other supported value.
    Status {
        /// Skip the initial query burst and report the values already known.
        #[arg(long)]
        cached: bool,
    },
    /// Stream device notifications until interrupted.
    Monitor,
    /// Switch noise control: off, on (noise cancelling) or ambient (transparency).
    Anc {
        /// Target mode.
        mode: AncArg,
    },
    /// Set the ambient-sound (transparency) volume, -3..=3.
    AmbientVolume {
        /// Volume step. Negative steps are accepted without `--` (`ambient-volume -2`).
        #[arg(allow_hyphen_values = true)]
        level: i8,
    },
    /// Choose the sound-effect preset.
    Eq {
        /// Preset name.
        preset: EqArg,
    },
    /// Enable or disable the low-latency game mode.
    GameMode {
        /// Desired state.
        state: OnOff,
    },
    /// Select the LDAC mode. macOS has no LDAC encoder, so any non-off mode silences audio
    /// until it is switched back; pass --force to confirm.
    Ldac {
        /// LDAC mode.
        mode: LdacArg,
        /// Acknowledge that audio will stop until LDAC is turned off again.
        #[arg(long)]
        force: bool,
    },
    /// Set the voice-prompt volume, 0..=15.
    PromptVolume {
        /// Volume step.
        level: u8,
    },
    /// Set the on-headset double-tap / cycle gestures that are allowed to switch ANC modes.
    Gestures {
        /// Allow the "normal" position in the cycle.
        #[arg(long)]
        normal: bool,
        /// Allow the "noise cancelling" position in the cycle.
        #[arg(long)]
        noise_reduction: bool,
        /// Allow the "ambient" position in the cycle.
        #[arg(long)]
        ambient: bool,
    },
    /// Set or clear the auto-shutdown timer.
    ShutdownTimer {
        /// Timer value: `off` or minutes (0..=180).
        #[arg(value_parser = parse_timer)]
        timer: ShutdownTimer,
    },
    /// Read or write the advertised device name.
    Name {
        /// New name; omitted reads the current one.
        name: Option<String>,
    },
    /// Read the battery percentage.
    Battery,
    /// Read the Bluetooth MAC address reported by the headset.
    Mac,
    /// Read the firmware version.
    Firmware,
    /// Forward a transport control action to the connected host.
    Playback {
        /// Action to perform.
        action: PlaybackArg,
    },
    /// Send a raw payload (hex, e.g. `C1 02`) without the frame header and checksum.
    Raw {
        /// Payload bytes.
        #[arg(required = true)]
        bytes: Vec<String>,
    },
    /// Ask macOS to move the *audio* link back to this Mac (requires a paired device).
    ConnectAudio {
        /// Release the audio link instead of taking it over.
        #[arg(long)]
        disconnect: bool,
    },
    /// List the classic-Bluetooth devices this Mac has paired and whether they are connected.
    Paired,
    /// Ask the headset to enter pairing mode.
    RePair {
        /// Skip the confirmation prompt.
        #[arg(long)]
        yes: bool,
    },
    /// Disconnect the control link.
    Disconnect,
    /// Power the headset off.
    PowerOff {
        /// Skip the confirmation prompt.
        #[arg(long)]
        yes: bool,
    },
    /// Factory reset the headset.
    FactoryReset {
        /// Skip the confirmation prompt.
        #[arg(long)]
        yes: bool,
    },
    /// Scan, connect, query everything and report the result — for troubleshooting.
    Doctor,
}

/// ANC modes accepted on the command line.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum AncArg {
    /// ANC and ambient sound off.
    Off,
    /// Active noise cancellation.
    On,
    /// Transparency / ambient sound pass-through.
    Ambient,
}

impl From<AncArg> for AncMode {
    fn from(value: AncArg) -> Self {
        match value {
            AncArg::Off => AncMode::Normal,
            AncArg::On => AncMode::NoiseCancelling,
            AncArg::Ambient => AncMode::Ambient,
        }
    }
}

/// Sound-effect presets.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum EqArg {
    Normal,
    Pop,
    Classical,
    Rock,
}

impl From<EqArg> for SoundEffect {
    fn from(value: EqArg) -> Self {
        match value {
            EqArg::Normal => SoundEffect::Normal,
            EqArg::Pop => SoundEffect::Pop,
            EqArg::Classical => SoundEffect::Classical,
            EqArg::Rock => SoundEffect::Rock,
        }
    }
}

/// LDAC modes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum LdacArg {
    Off,
    #[value(name = "48k")]
    Khz48,
    #[value(name = "96k")]
    Khz96,
}

impl From<LdacArg> for LdacMode {
    fn from(value: LdacArg) -> Self {
        match value {
            LdacArg::Off => LdacMode::Off,
            LdacArg::Khz48 => LdacMode::Khz48,
            LdacArg::Khz96 => LdacMode::Khz96,
        }
    }
}

/// Boolean switches.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum OnOff {
    On,
    Off,
}

impl From<OnOff> for bool {
    fn from(value: OnOff) -> Self {
        matches!(value, OnOff::On)
    }
}

/// Transport actions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum PlaybackArg {
    Play,
    Pause,
    VolumeUp,
    VolumeDown,
    Next,
    Previous,
}

impl From<PlaybackArg> for PlaybackAction {
    fn from(value: PlaybackArg) -> Self {
        match value {
            PlaybackArg::Play => PlaybackAction::Play,
            PlaybackArg::Pause => PlaybackAction::Pause,
            PlaybackArg::VolumeUp => PlaybackAction::VolumeUp,
            PlaybackArg::VolumeDown => PlaybackAction::VolumeDown,
            PlaybackArg::Next => PlaybackAction::Next,
            PlaybackArg::Previous => PlaybackAction::Previous,
        }
    }
}

/// What a command produced.
#[derive(Debug, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Reply {
    /// Scan results.
    Devices { devices: Vec<Discovered> },
    /// A full or partial status snapshot.
    Status { status: Box<Status> },
    /// The device's answer to a single command.
    Event { event: String, status: Box<Status> },
    /// Free-form confirmation.
    Message { message: String },
}

/// Parse the `shutdown-timer` argument.
///
/// # Errors
///
/// Returns a message when the value is neither `off` nor a minute count within 0..=180.
pub fn parse_timer(text: &str) -> Result<ShutdownTimer, String> {
    if text.eq_ignore_ascii_case("off") || text == "0" {
        return Ok(ShutdownTimer::Disabled);
    }
    let minutes: u8 = text
        .parse()
        .map_err(|_| format!("expected `off` or a number of minutes, got {text:?}"))?;
    if edifier_proto::SHUTDOWN_TIMER_RANGE.contains(&minutes) {
        Ok(ShutdownTimer::Minutes(minutes))
    } else {
        Err(format!(
            "shutdown timer must be 0..=180 minutes, got {minutes}"
        ))
    }
}

/// Parse a hex byte string such as `C1` or `0xC1`.
///
/// # Errors
///
/// Returns a message when the token is not exactly one byte of hex.
pub fn parse_hex_byte(token: &str) -> Result<u8, String> {
    let cleaned = token.trim_start_matches("0x").trim_start_matches("0X");
    u8::from_str_radix(cleaned, 16)
        .map_err(|_| format!("expected one hex byte (e.g. C1), got {token:?}"))
}

/// Turn the raw `raw` arguments into payload bytes.
///
/// # Errors
///
/// Propagates [`parse_hex_byte`] failures.
pub fn parse_payload(tokens: &[String]) -> Result<Vec<u8>> {
    tokens
        .iter()
        .map(|token| parse_hex_byte(token))
        .collect::<Result<Vec<u8>, String>>()
        .map_err(|err| anyhow::anyhow!(err))
}

/// Render a status snapshot for interactive output.
#[must_use]
pub fn render_status(status: &Status) -> String {
    let mut lines = Vec::new();
    let mut field = |label: &str, value: Option<String>| {
        if let Some(value) = value {
            lines.push(format!("{label:<16} {value}"));
        }
    };
    field("device", status.name.clone());
    field(
        "model",
        status.model.map(|model| model.display_name().to_string()),
    );
    field(
        "battery",
        status.battery.map(|percent| format!("{percent}%")),
    );
    field("firmware", status.firmware.clone());
    field("mac", status.mac.clone());
    field("anc", status.anc.map(|mode| mode.as_str().to_string()));
    field(
        "ambient volume",
        status.ambient_volume.map(|volume| volume.to_string()),
    );
    field(
        "sound effect",
        status
            .sound_effect
            .map(|effect| effect.as_str().to_string()),
    );
    field("game mode", status.game_mode.map(|on| on.to_string()));
    field("ldac", status.ldac.map(|mode| mode.as_str().to_string()));
    field(
        "prompt volume",
        status.prompt_volume.map(|level| level.to_string()),
    );
    field(
        "shutdown timer",
        status.shutdown_timer.map(|timer| match timer {
            ShutdownTimer::Disabled => "off".into(),
            ShutdownTimer::Minutes(minutes) => format!("{minutes} min"),
        }),
    );
    field(
        "auto power off",
        status.auto_poweroff.map(|on| on.to_string()),
    );
    field(
        "anc gestures",
        status
            .anc_gestures
            .map(|gestures| format!("0b{:03b}", gestures.mask())),
    );
    field(
        "playback state",
        status.playback_state.map(|state| format!("0x{state:02X}")),
    );
    lines.join("\n")
}

/// Run one command.
///
/// # Errors
///
/// Fails when the device cannot be reached, when the headset rejects a command, or when the user
/// asks for something destructive without `--yes`.
pub async fn run(cli: &Cli) -> Result<Reply> {
    let command_timeout = Duration::from_millis(cli.timeout_ms.max(500));
    let target = cli
        .device
        .as_deref()
        .map_or(Target::Auto, Target::from_selector);

    match cli.command {
        CommandLine::Scan => {
            let devices = ble::scan(Duration::from_secs(cli.scan_seconds), usize::MAX).await?;
            return Ok(Reply::Devices { devices });
        }
        CommandLine::Paired => {
            let devices = edifier_macos::paired_devices()?;
            let listed = devices
                .iter()
                .map(|device| {
                    format!(
                        "{} {:<24} {}",
                        if device.connected { "●" } else { "○" },
                        device.name,
                        device.address
                    )
                })
                .collect::<Vec<_>>()
                .join("\n");
            return Ok(Reply::Message {
                message: if listed.is_empty() {
                    "no paired classic Bluetooth devices".into()
                } else {
                    listed
                },
            });
        }
        CommandLine::ConnectAudio { disconnect } => {
            let message = if disconnect {
                let selector = cli
                    .device
                    .clone()
                    .ok_or_else(|| anyhow::anyhow!("--device is required to disconnect"))?;
                edifier_macos::disconnect_paired_audio(&selector)?
            } else {
                connect_paired_audio(cli.device.as_deref())?
            };
            return Ok(Reply::Message { message });
        }
        _ => {}
    }

    info!(target = ?target, "opening control link");
    let requested = format!("connect to {target:?}");
    let session = Session::connect_ble(target)
        .await
        .with_context(|| requested)?;
    info!(link = session.describe(), "connected");

    let reply = dispatch(&session, &cli.command, command_timeout).await;
    session.close().await;
    reply
}

async fn dispatch(session: &Session, command: &CommandLine, wait: Duration) -> Result<Reply> {
    let simple = |command: Command| async move {
        let event = session.request(&command, wait).await?;
        Ok(Reply::Event {
            event: event.to_string(),
            status: Box::new(session.status().await),
        })
    };

    match command {
        CommandLine::Scan | CommandLine::Paired | CommandLine::ConnectAudio { .. } => {
            unreachable!("handled before connecting")
        }
        CommandLine::Status { cached } => {
            let status = if *cached {
                session.status().await
            } else {
                session.refresh().await
            };
            Ok(Reply::Status {
                status: Box::new(status),
            })
        }
        CommandLine::Monitor => {
            let mut events = session.subscribe();
            println!("monitoring {} — press Ctrl-C to stop", session.describe());
            loop {
                tokio::select! {
                    event = events.recv() => match event {
                        Ok(event) => println!("{event}"),
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(skipped)) => {
                            warn!(skipped, "events dropped");
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                    },
                    _ = tokio::signal::ctrl_c() => break,
                }
            }
            Ok(Reply::Message {
                message: "stopped".into(),
            })
        }
        CommandLine::Anc { mode } => simple(Command::SetAnc((*mode).into())).await,
        CommandLine::AmbientVolume { level } => {
            let volume = AmbientVolume::new(*level).map_err(|err| anyhow::anyhow!("{err}"))?;
            simple(Command::SetAmbientVolume(volume)).await
        }
        CommandLine::Eq { preset } => simple(Command::SetSoundEffect((*preset).into())).await,
        CommandLine::GameMode { state } => simple(Command::SetGameMode((*state).into())).await,
        CommandLine::Ldac { mode, force } => {
            let mode: LdacMode = (*mode).into();
            if mode != LdacMode::Off && !force {
                bail!(
                    "refusing to switch LDAC to {} without --force: macOS ships no LDAC encoder, \
                     so audio stops until it is set back to off",
                    mode.as_str()
                );
            }
            if mode != LdacMode::Off {
                warn!("audio will stop on this Mac until LDAC is set back to off");
            }
            simple(Command::SetLdac(mode)).await
        }
        CommandLine::PromptVolume { level } => {
            if !edifier_proto::PROMPT_VOLUME_RANGE.contains(level) {
                bail!("prompt volume must be 0..=15, got {level}");
            }
            simple(Command::SetPromptVolume(*level)).await
        }
        CommandLine::Gestures {
            normal,
            noise_reduction,
            ambient,
        } => {
            let settings = ControlSettings {
                normal: *normal,
                noise_reduction: *noise_reduction,
                ambient: *ambient,
            };
            simple(Command::SetControlSettings(settings)).await
        }
        CommandLine::ShutdownTimer { timer } => {
            simple(match timer {
                ShutdownTimer::Disabled => Command::SetShutdownTimer(None),
                ShutdownTimer::Minutes(minutes) => Command::SetShutdownTimer(Some(*minutes)),
            })
            .await
        }
        CommandLine::Name { name } => match name {
            Some(name) => {
                let model: Option<DeviceModel> = session.status().await.model;
                let event = session.set_name(name, model).await?;
                Ok(Reply::Event {
                    event: event.to_string(),
                    status: Box::new(session.status().await),
                })
            }
            None => simple(Command::QueryName).await,
        },
        CommandLine::Battery => simple(Command::QueryBattery).await,
        CommandLine::Mac => simple(Command::QueryMac).await,
        CommandLine::Firmware => simple(Command::QueryFirmware).await,
        CommandLine::Playback { action } => simple(Command::Playback((*action).into())).await,
        CommandLine::Raw { bytes } => {
            let payload = parse_payload(bytes)?;
            let event = session.request(&Command::Raw(payload), wait).await?;
            Ok(Reply::Event {
                event: event.to_string(),
                status: Box::new(session.status().await),
            })
        }
        CommandLine::RePair { yes } => {
            confirm_and_send(session, *yes, Command::RePair, "re-pair", wait).await
        }
        CommandLine::Disconnect => {
            session.send(&Command::Disconnect).await?;
            Ok(Reply::Message {
                message: "disconnect requested".into(),
            })
        }
        CommandLine::PowerOff { yes } => {
            confirm_and_send(session, *yes, Command::PowerOff, "power off", wait).await
        }
        CommandLine::FactoryReset { yes } => {
            confirm_and_send(session, *yes, Command::FactoryReset, "factory reset", wait).await
        }
        CommandLine::Doctor => {
            let status = session.refresh().await;
            let events = session.status().await;
            let report = format!(
                "link: {}\nquiried fields: {}\n{}",
                session.describe(),
                report_field_count(&status),
                render_status(&events)
            );
            Ok(Reply::Message { message: report })
        }
    }
}

fn report_field_count(status: &Status) -> usize {
    let json = serde_json::to_value(status).unwrap_or(serde_json::Value::Null);
    json.as_object().map_or(0, serde_json::Map::len)
}

async fn confirm_and_send(
    session: &Session,
    confirmed: bool,
    command: Command,
    label: &str,
    wait: Duration,
) -> Result<Reply> {
    if !confirmed {
        bail!("refusing to {label} without --yes");
    }
    let event = session.request(&command, wait).await?;
    Ok(Reply::Event {
        event: format!("{label}: {event}"),
        status: Box::new(session.status().await),
    })
}
