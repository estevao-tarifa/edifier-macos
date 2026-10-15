//! Advertise this Mac as an EDIFIER W820NB and answer the control protocol over real BLE.
//!
//! The headset itself is a Bluetooth Low Energy peripheral: it publishes one primary service in
//! the `4809xxxx-1a48-11e9-ab14-d663bd873d93` family with a notify characteristic (`…0001`) and
//! a write characteristic (`…0002`). [`crate::ble`] (central role) scans for that pair, subscribes
//! to notifications and writes host frames to it. This example implements the *device* side of
//! that exchange with CoreBluetooth's peripheral role, so the whole stack can be exercised without
//! the physical headset.
//!
//! # Threading
//!
//! Everything runs on the main thread: the [`CBPeripheralManager`] is created with the main
//! dispatch queue, the delegate callbacks therefore arrive on the main thread, and `main` blocks in
//! `CFRunLoopRun`, which is what CoreBluetooth requires to deliver those callbacks. No locking is
//! needed because no callback ever runs concurrently with another. The only background thread is
//! the discovery thread that locates this very peripheral with a *central*-role scan (see
//! [`spawn_self_discovery`]) — that thread owns its own single-threaded Tokio runtime.
//!
//! # Output
//!
//! - `emulator-ready <identifier>` once on stdout: the CoreBluetooth identifier a central scan
//!   reports for this peripheral, i.e. the value `edifierctl --device <identifier>` needs.
//! - `emulator-state <json>` on stdout after every handled frame, so a test can observe the state
//!   machine. Diagnostics go to stderr through `tracing`, driven by `RUST_LOG`.
//!
//! Everything the emulator answers is built from the documented command table (see
//! `docs/protocol.md`) using `edifier-proto`'s framing; no reply is invented.
//!
//! # Reachability (measured on macOS 15.7.2, Apple Silicon)
//!
//! The advertisement really is handed to the system: `bluetoothd` logs
//! `Received 'start advertising' request … with advertised UUID(s) [48093801-…], name "EDIFIER
//! W820NB"` followed by `Started advertising successfully status=0` and
//! `Advertising state change: Starting(1) --> Advertising(2)`. However, a `CBCentralManager` scan
//! running on *this same host* never reports it — neither from another process
//! (`edifierctl scan`, `examples/rawscan.rs`) nor from this process — while the same scans do list
//! the 14–15 remote BLE devices in range. A local scan sees only peripherals received over the
//! air, and a controller cannot receive its own transmission, so a client on the same Mac cannot
//! discover or connect to this peripheral.
//!
//! Consequence: `emulator-ready <identifier>` is printed as soon as a *local* scan reports the
//! peripheral, which `spawn_self_discovery` retries every few seconds. On a host that loops local
//! advertisements back (a second device running the client, or a stack that relays them) the line
//! appears immediately; on this Mac it does not, and a warning on stderr says so after 20 s. The
//! protocol half of the emulator is verified without a radio by the tests at the end of this file
//! (`cargo test -p edifier-link --example emulate`), which assert the recorded frames byte for
//! byte.

use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use edifier_proto::command::{AmbientVolume, AncMode, LdacMode, SoundEffect};
use edifier_proto::event::ShutdownTimer;
use edifier_proto::frame::{Frame, FrameDecoder, FrameError, Head};
use objc2::runtime::{AnyObject, NSObject, NSObjectProtocol, ProtocolObject};
use objc2::{DefinedClass, MainThreadMarker, MainThreadOnly, define_class, msg_send};
use objc2_core_bluetooth::{
    CBATTError, CBATTRequest, CBAdvertisementDataLocalNameKey, CBAdvertisementDataServiceUUIDsKey,
    CBAttributePermissions, CBCentral, CBCharacteristic, CBCharacteristicProperties,
    CBManagerState, CBMutableCharacteristic, CBMutableService, CBPeripheralManager,
    CBPeripheralManagerDelegate, CBService, CBUUID,
};
use objc2_core_foundation::{CFRunLoop, kCFRunLoopDefaultMode};
use objc2_foundation::{NSArray, NSData, NSError, NSMutableDictionary, NSString};
use tracing::{debug, info, warn};

/// Control service the W820NB advertises.
const SERVICE_UUID: &str = "48093801-1a48-11e9-ab14-d663bd873d93";
/// Characteristic the device notifies on (`…0001`).
const NOTIFY_CHARACTERISTIC_UUID: &str = "48090001-1a48-11e9-ab14-d663bd873d93";
/// Characteristic the host writes to (`…0002`).
const WRITE_CHARACTERISTIC_UUID: &str = "48090002-1a48-11e9-ab14-d663bd873d93";

/// Advertised name of the emulated headset.
const DEFAULT_NAME: &str = "EDIFIER W820NB";
/// Bluetooth address reported by `C8` (the real W820NB's address, overridable with `--mac`).
const DEFAULT_MAC: [u8; 6] = [0x0C, 0xAE, 0xBD, 0xBC, 0x8F, 0xA2];
/// Battery percentage reported by `D0`.
const DEFAULT_BATTERY: u8 = 77;
/// Firmware version reported by `C6` (`03.00.02`).
const DEFAULT_FIRMWARE: [u8; 3] = [0x03, 0x00, 0x02];
/// Fingerprint blob reported by `D8`. The brief and `docs/protocol.md` both give the reply as
/// `BB 16 D8 …`, i.e. a 22-byte payload, so the blob is 21 bytes.
const DEFAULT_FINGERPRINT: [u8; 21] = [
    0xAA, 0xBB, 0xCC, 0xDD, 0xEE, 0xFF, 0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99,
    0xAA, 0xBB, 0xCC, 0xDD, 0xEE,
];
/// Voice-prompt volume reported by `05` (the value in the captured traffic).
const DEFAULT_PROMPT_VOLUME: u8 = 0x07;

/// Silence after which a partial frame is dropped, matching `comm.cpp`'s `packetTimeoutMs`.
const FRAME_IDLE_RESET: Duration = Duration::from_secs(5);

/// One notification queue entry waiting for the transmit queue to drain.
const PENDING_NOTIFICATIONS: usize = 32;

// -------------------------------------------------------------------------------------------------
// Arguments
// -------------------------------------------------------------------------------------------------

/// Command-line configuration.
#[derive(Debug, Clone)]
struct Args {
    /// Bluetooth address the emulated device reports for `C8`.
    mac: [u8; 6],
    /// Advertised (and protocol) name.
    name: String,
    /// Battery percentage reported for `D0`.
    battery: u8,
    /// Stop after this many seconds instead of running until interrupted.
    seconds: Option<u64>,
}

impl Args {
    /// Parse `--mac`, `--name`, `--battery` and `--seconds`.
    fn parse<I: IntoIterator<Item = String>>(arguments: I) -> Result<Self> {
        let mut args = Args {
            mac: DEFAULT_MAC,
            name: DEFAULT_NAME.to_string(),
            battery: DEFAULT_BATTERY,
            seconds: None,
        };
        let mut arguments = arguments.into_iter();
        while let Some(flag) = arguments.next() {
            let mut value = || {
                arguments
                    .next()
                    .ok_or_else(|| anyhow!("{flag} needs a value"))
            };
            match flag.as_str() {
                "--mac" => args.mac = parse_mac(&value()?)?,
                "--name" => args.name = value()?,
                "--battery" => {
                    let text = value()?;
                    let percent: u8 = text
                        .parse()
                        .with_context(|| format!("--battery expects 0..=100, got {text:?}"))?;
                    if percent > 100 {
                        bail!("--battery expects 0..=100, got {percent}");
                    }
                    args.battery = percent;
                }
                "--seconds" => {
                    let text = value()?;
                    args.seconds =
                        Some(text.parse().with_context(|| {
                            format!("--seconds expects a number, got {text:?}")
                        })?);
                }
                other => bail!("unknown argument {other:?} (try --help)"),
            }
        }
        if args.name.is_empty() {
            bail!("--name must not be empty");
        }
        Ok(args)
    }
}

/// Parse `AA:BB:CC:DD:EE:FF` (colons or dashes).
fn parse_mac(text: &str) -> Result<[u8; 6]> {
    let parts: Vec<&str> = text.split([':', '-']).collect();
    if parts.len() != 6 {
        bail!("--mac expects six hex pairs separated by `:` or `-`, got {text:?}");
    }
    let mut mac = [0u8; 6];
    for (slot, part) in mac.iter_mut().zip(parts) {
        *slot = u8::from_str_radix(part, 16)
            .with_context(|| format!("--mac expects hex bytes, got {part:?}"))?;
    }
    Ok(mac)
}

const USAGE: &str = "\
Emulate an EDIFIER W820NB over real Bluetooth Low Energy.

Usage: emulate [OPTIONS]

Options:
      --mac <AA:BB:CC:DD:EE:FF>  Bluetooth address reported for the `C8` query
      --name <NAME>              Advertised device name (default: EDIFIER W820NB)
      --battery <0..=100>        Battery percentage reported for `D0` (default: 77)
      --seconds <N>              Exit after N seconds (default: run until interrupted)
  -h, --help                     Print this help
";

// -------------------------------------------------------------------------------------------------
// Device state machine
// -------------------------------------------------------------------------------------------------

/// The state the emulated firmware keeps, one field per documented query.
#[derive(Debug, Clone)]
struct Emulator {
    name: String,
    battery: u8,
    mac: [u8; 6],
    firmware: [u8; 3],
    fingerprint: [u8; 21],
    anc: AncMode,
    ambient_volume: AmbientVolume,
    sound_effect: SoundEffect,
    game_mode: bool,
    ldac: LdacMode,
    prompt_volume: u8,
    shutdown: ShutdownTimer,
    auto_poweroff: bool,
    gestures: u8,
    playback_state: u8,
}

impl Emulator {
    fn new(args: &Args) -> Self {
        Emulator {
            name: args.name.clone(),
            battery: args.battery,
            mac: args.mac,
            firmware: DEFAULT_FIRMWARE,
            fingerprint: DEFAULT_FINGERPRINT,
            anc: AncMode::Normal,
            ambient_volume: AmbientVolume(0),
            sound_effect: SoundEffect::Normal,
            game_mode: false,
            ldac: LdacMode::Off,
            prompt_volume: DEFAULT_PROMPT_VOLUME,
            shutdown: ShutdownTimer::Disabled,
            auto_poweroff: false,
            gestures: 0b111,
            playback_state: 0x00,
        }
    }

    /// The ambient-sound byte as it appears on the wire (`volume + 6`).
    fn ambient_wire(&self) -> u8 {
        self.ambient_volume.wire()
    }

    /// The mode byte used by `C1`/`CC`.
    fn anc_wire(&self) -> u8 {
        match self.anc {
            AncMode::Normal => 0x01,
            AncMode::NoiseCancelling => 0x02,
            AncMode::Ambient => 0x03,
        }
    }

    /// Answer one host frame, returning the encoded replies and whether the name changed.
    ///
    /// Replies follow the documented command table exactly: state/value frames (`BB`) echo the
    /// question, acknowledgements (`CC`) echo the request. Unknown commands are acknowledged the
    /// way the firmware acknowledges a request it does not implement.
    fn handle(&mut self, frame: &Frame) -> Result<(Vec<Vec<u8>>, bool), FrameError> {
        let Some(&command) = frame.payload.first() else {
            return Ok((Vec::new(), false));
        };
        let params = frame.params();
        let mut renamed = false;

        let replies: Vec<(Head, Vec<u8>)> = match command {
            // Query ANC mode + ambient volume.
            0xCC => vec![(
                Head::State,
                vec![0xCC, self.anc_wire(), self.ambient_wire()],
            )],
            // Set ANC mode, or set the ambient volume (`C1 03 <byte>` also selects ambient).
            0xC1 => match params {
                [0x03, wire] => {
                    self.anc = AncMode::Ambient;
                    self.ambient_volume = AmbientVolume::from_wire(*wire);
                    vec![(Head::State, vec![0xC1, 0x03, *wire])]
                }
                [0x03] => {
                    self.anc = AncMode::Ambient;
                    vec![(Head::State, vec![0xC1, 0x03, self.ambient_wire()])]
                }
                [mode @ (0x01 | 0x02)] => {
                    self.anc = if *mode == 0x01 {
                        AncMode::Normal
                    } else {
                        AncMode::NoiseCancelling
                    };
                    vec![(Head::State, vec![0xC1, *mode, self.ambient_wire()])]
                }
                _ => vec![(Head::Ack, frame.payload.clone())],
            },
            // Query sound effect / set sound effect (acknowledged, the firmware reports the
            // setting through `D5`).
            0xD5 => vec![(Head::State, vec![0xD5, self.sound_effect.code()])],
            0xC4 => match params {
                [code] if SoundEffect::from_code(*code).is_some() => {
                    self.sound_effect =
                        SoundEffect::from_code(*code).unwrap_or(SoundEffect::Normal);
                    vec![(Head::Ack, vec![0xC4, 0x01])]
                }
                _ => vec![(Head::Ack, frame.payload.clone())],
            },
            // Query/set the ANC gesture mask.
            0xF0 if params == [0x0A] => vec![(Head::State, vec![0xF0, 0x0A, self.gestures])],
            0xF1 => match params {
                [0x0A, mask] => {
                    self.gestures = *mask;
                    vec![(Head::State, vec![0xF1, 0x0A, *mask])]
                }
                _ => vec![(Head::Ack, frame.payload.clone())],
            },
            // Query/set LDAC.
            0x48 => vec![(Head::State, vec![0x48, self.ldac.code()])],
            0x49 => match params {
                [mode] => match LdacMode::from_code(*mode) {
                    Some(ldac) => {
                        self.ldac = ldac;
                        vec![(Head::State, vec![0x49, *mode])]
                    }
                    None => vec![(Head::Ack, frame.payload.clone())],
                },
                _ => vec![(Head::Ack, frame.payload.clone())],
            },
            // Query/set the low-latency game mode.
            0x08 => vec![(Head::State, vec![0x08, self.game_mode as u8])],
            0x09 => match params {
                [flag] => {
                    self.game_mode = *flag != 0;
                    vec![(Head::State, vec![0x09, *flag])]
                }
                _ => vec![(Head::Ack, frame.payload.clone())],
            },
            // Query/set the voice-prompt volume.
            0x05 => vec![(Head::State, vec![0x05, self.prompt_volume])],
            0x06 => match params {
                [volume] => {
                    self.prompt_volume = *volume;
                    vec![(Head::State, vec![0x06, *volume])]
                }
                _ => vec![(Head::Ack, frame.payload.clone())],
            },
            // Query the shutdown timer (single-byte "disabled" form, 16-bit minute form when set).
            0xD3 => match self.shutdown {
                ShutdownTimer::Disabled => vec![(Head::State, vec![0xD3, 0x00])],
                ShutdownTimer::Minutes(minutes) => {
                    vec![(Head::State, vec![0xD3, 0x00, minutes])]
                }
            },
            // Set (`D1 00 <minutes>`) / clear (`D2`) the shutdown timer, both acknowledged.
            0xD1 => match params {
                [0x00, minutes] | [minutes] => {
                    self.shutdown = ShutdownTimer::Minutes(*minutes);
                    vec![(Head::Ack, vec![0xD1, 0x01])]
                }
                _ => vec![(Head::Ack, frame.payload.clone())],
            },
            0xD2 => {
                self.shutdown = ShutdownTimer::Disabled;
                vec![(Head::Ack, vec![0xD2, 0x01])]
            }
            // Query/set auto power off.
            0xD7 => vec![(Head::State, vec![0xD7, self.auto_poweroff as u8])],
            0xD6 => match params {
                [flag] => {
                    self.auto_poweroff = *flag != 0;
                    vec![(Head::State, vec![0xD6, *flag])]
                }
                _ => vec![(Head::Ack, frame.payload.clone())],
            },
            // Battery, MAC, firmware, name, fingerprint, playback state.
            0xD0 => vec![(Head::State, vec![0xD0, self.battery])],
            0xC8 => {
                let mut payload = vec![0xC8];
                payload.extend_from_slice(&self.mac);
                vec![(Head::State, payload)]
            }
            0xC6 => {
                let mut payload = vec![0xC6];
                payload.extend_from_slice(&self.firmware);
                vec![(Head::State, payload)]
            }
            0xC9 => {
                let mut payload = vec![0xC9];
                payload.extend_from_slice(self.name.as_bytes());
                vec![(Head::State, payload)]
            }
            // Rename: applied, acknowledged, and reflected in the advertisement.
            0xCA => {
                if !params.is_empty() {
                    let requested = String::from_utf8_lossy(params);
                    let requested = requested.trim_matches(char::from(0));
                    if !requested.is_empty() {
                        self.name = requested.to_string();
                        renamed = true;
                    }
                }
                vec![(Head::Ack, vec![0xCA, 0x01])]
            }
            0xD8 => {
                let mut payload = vec![0xD8];
                payload.extend_from_slice(&self.fingerprint);
                vec![(Head::State, payload)]
            }
            0xC3 => vec![(Head::State, vec![0xC3, self.playback_state])],
            // Playback action, disconnect, power off, re-pair, factory reset: acknowledged.
            0xC2 => vec![(Head::Ack, vec![0xC2, 0x01])],
            0xCD => vec![(Head::Ack, vec![0xCD, 0x01])],
            0xCE => vec![(Head::Ack, vec![0xCE, 0x01])],
            0xCF => vec![(Head::Ack, vec![0xCF, 0x01])],
            0x07 => vec![(Head::Ack, vec![0x07, 0x01])],
            // Anything else: acknowledge, echoing the request, like the firmware does.
            _ => vec![(Head::Ack, frame.payload.clone())],
        };

        let encoded = replies
            .into_iter()
            .map(|(head, payload)| Frame { head, payload }.encode())
            .collect::<Result<Vec<Vec<u8>>, FrameError>>()?;
        Ok((encoded, renamed))
    }

    /// A JSON snapshot of the state, printed as an `emulator-state` line.
    fn snapshot(&self) -> StateSnapshot<'_> {
        StateSnapshot {
            name: &self.name,
            battery: self.battery,
            mac: format_mac(&self.mac),
            firmware: format_bytes(&self.firmware, "."),
            anc: self.anc.as_str(),
            ambient_volume: self.ambient_volume.0,
            sound_effect: self.sound_effect.as_str(),
            game_mode: self.game_mode,
            ldac: self.ldac.as_str(),
            prompt_volume: self.prompt_volume,
            shutdown_minutes: match self.shutdown {
                ShutdownTimer::Disabled => None,
                ShutdownTimer::Minutes(minutes) => Some(minutes),
            },
            auto_poweroff: self.auto_poweroff,
            anc_gestures: format!("0b{:03b}", self.gestures),
            playback_state: format!("0x{:02X}", self.playback_state),
            fingerprint: format_bytes(&self.fingerprint, ""),
        }
    }
}

/// Serializable view of [`Emulator`] used for the `emulator-state` lines.
#[derive(serde::Serialize)]
struct StateSnapshot<'a> {
    name: &'a str,
    battery: u8,
    mac: String,
    firmware: String,
    anc: &'static str,
    ambient_volume: i8,
    sound_effect: &'static str,
    game_mode: bool,
    ldac: &'static str,
    prompt_volume: u8,
    shutdown_minutes: Option<u8>,
    auto_poweroff: bool,
    anc_gestures: String,
    playback_state: String,
    fingerprint: String,
}

fn format_mac(bytes: &[u8]) -> String {
    format_bytes(bytes, ":")
}

fn format_bytes(bytes: &[u8], separator: &str) -> String {
    bytes
        .iter()
        .map(|byte| format!("{byte:02X}"))
        .collect::<Vec<_>>()
        .join(separator)
}

// -------------------------------------------------------------------------------------------------
// CoreBluetooth peripheral
// -------------------------------------------------------------------------------------------------

/// Feed one write chunk through the decoder and answer every complete frame.
///
/// A host frame longer than the 20-byte BLE write chunk arrives as several write requests; the
/// decoder buffers the partial frame until the final chunk (exactly what the real firmware does).
/// Returns the encoded replies and whether any of them renamed the device.
fn handle_chunk(
    state: &mut Emulator,
    decoder: &mut FrameDecoder,
    chunk: &[u8],
) -> (Vec<Vec<u8>>, bool) {
    let mut replies = Vec::new();
    let mut renamed = false;
    decoder.push(chunk);
    while let Some(decoded) = decoder.next_frame() {
        let frame = match decoded {
            Ok(frame) => frame,
            Err(err) => {
                warn!(error = %err, "dropping malformed host frame");
                continue;
            }
        };
        match state.handle(&frame) {
            Ok((encoded, renamed_by_command)) => {
                renamed |= renamed_by_command;
                replies.extend(encoded);
            }
            Err(err) => warn!(error = %err, "cannot encode a reply"),
        }
    }
    (replies, renamed)
}

/// Instance variables of the peripheral-manager delegate.
///
/// Every field is interior-mutable because CoreBluetooth hands out `&self`; nothing here is shared
/// across threads, so `RefCell`/`Cell` are enough (see the module docs on threading).
struct EmulatorIvars {
    state: RefCell<Emulator>,
    decoder: RefCell<FrameDecoder>,
    last_write: Cell<Option<Instant>>,
    notify: RefCell<Option<objc2::rc::Retained<CBMutableCharacteristic>>>,
    /// Frames whose delivery was refused because the transmit queue was full.
    pending: RefCell<VecDeque<Vec<u8>>>,
    /// The most recent notification, served to read requests on the notify characteristic.
    last_notification: RefCell<Vec<u8>>,
    advertising: Cell<bool>,
    published: Cell<bool>,
}

define_class!(
    // SAFETY: `NSObject` has no subclassing requirements and the delegate implements no `Drop`.
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[ivars = EmulatorIvars]
    struct EmulatorDelegate;

    // SAFETY: `NSObjectProtocol` has no additional requirements.
    unsafe impl NSObjectProtocol for EmulatorDelegate {}

    // SAFETY: `CBPeripheralManagerDelegate` has no additional requirements; all selectors below
    // match the framework's signatures.
    unsafe impl CBPeripheralManagerDelegate for EmulatorDelegate {
        #[unsafe(method(peripheralManagerDidUpdateState:))]
        fn did_update_state(&self, peripheral: &CBPeripheralManager) {
            let state = unsafe { peripheral.state() };
            if state != CBManagerState::PoweredOn {
                warn!(?state, "peripheral manager is not powered on");
                return;
            }
            info!("peripheral manager powered on");
            self.publish_service(peripheral);
            self.start_advertising(peripheral);
        }

        #[unsafe(method(peripheralManagerDidStartAdvertising:error:))]
        fn did_start_advertising(
            &self,
            _peripheral: &CBPeripheralManager,
            error: Option<&NSError>,
        ) {
            match error {
                Some(error) => warn!(error = %error, "advertising failed"),
                None => {
                    self.ivars().advertising.set(true);
                    info!(name = %self.ivars().state.borrow().name, "advertising");
                    self.print_state();
                }
            }
        }

        #[unsafe(method(peripheralManager:didAddService:error:))]
        fn did_add_service(
            &self,
            _peripheral: &CBPeripheralManager,
            service: &CBService,
            error: Option<&NSError>,
        ) {
            match error {
                Some(error) => warn!(error = %error, "adding the service failed"),
                None => {
                    self.ivars().published.set(true);
                    let uuid = unsafe { service.UUID().UUIDString() }.to_string();
                    info!(uuid = %uuid, "control service published");
                }
            }
        }

        #[unsafe(method(peripheralManager:central:didSubscribeToCharacteristic:))]
        fn did_subscribe(
            &self,
            _peripheral: &CBPeripheralManager,
            central: &CBCentral,
            _characteristic: &CBCharacteristic,
        ) {
            let identifier = unsafe { central.identifier().UUIDString() }.to_string();
            info!(central = %identifier, "central subscribed to notifications");
        }

        #[unsafe(method(peripheralManager:didReceiveReadRequest:))]
        fn did_receive_read(&self, peripheral: &CBPeripheralManager, request: &CBATTRequest) {
            // The client only reads what it needs to subscribe; answer with the last notification
            // so a read of the notify characteristic is never empty.
            let value = self.ivars().last_notification.borrow().clone();
            let data = NSData::with_bytes(&value);
            unsafe { request.setValue(Some(&data)) };
            unsafe { peripheral.respondToRequest_withResult(request, CBATTError::Success) };
        }

        #[unsafe(method(peripheralManager:didReceiveWriteRequests:))]
        fn did_receive_write(
            &self,
            peripheral: &CBPeripheralManager,
            requests: &NSArray<CBATTRequest>,
        ) {
            let mut replies: Vec<Vec<u8>> = Vec::new();
            let mut renamed = false;
            {
                let ivars = self.ivars();
                let mut decoder = ivars.decoder.borrow_mut();
                let now = Instant::now();
                if ivars
                    .last_write
                    .get()
                    .is_some_and(|at| now.duration_since(at) > FRAME_IDLE_RESET)
                {
                    // 5 s of silence abandons a partial frame, matching the host side.
                    decoder.reset();
                }
                ivars.last_write.set(Some(now));

                let mut state = ivars.state.borrow_mut();
                for request in requests.to_vec() {
                    let value = unsafe { request.value() };
                    unsafe {
                        peripheral.respondToRequest_withResult(&request, CBATTError::Success)
                    };
                    let Some(value) = value else { continue };
                    let bytes = value.to_vec();
                    debug!(len = bytes.len(), "write request");
                    let (chunk_replies, chunk_renamed) =
                        handle_chunk(&mut state, &mut decoder, &bytes);
                    replies.extend(chunk_replies);
                    renamed |= chunk_renamed;
                }
            }

            let handled = !replies.is_empty();
            for reply in replies {
                self.notify(peripheral, reply);
            }
            if handled {
                self.print_state();
            }
            if renamed {
                let name = self.ivars().state.borrow().name.clone();
                info!(name = %name, "renamed, restarting advertising");
                self.start_advertising(peripheral);
            }
        }

        #[unsafe(method(peripheralManagerIsReadyToUpdateSubscribers:))]
        fn ready_to_update(&self, peripheral: &CBPeripheralManager) {
            let pending: Vec<Vec<u8>> = self.ivars().pending.borrow_mut().drain(..).collect();
            for bytes in pending {
                self.notify(peripheral, bytes);
            }
        }
    }
);

impl EmulatorDelegate {
    /// Create the delegate with its initial device state.
    fn new(mtm: MainThreadMarker, args: &Args) -> objc2::rc::Retained<Self> {
        let ivars = EmulatorIvars {
            state: RefCell::new(Emulator::new(args)),
            decoder: RefCell::new(FrameDecoder::new()),
            last_write: Cell::new(None),
            notify: RefCell::new(None),
            pending: RefCell::new(VecDeque::new()),
            last_notification: RefCell::new(Vec::new()),
            advertising: Cell::new(false),
            published: Cell::new(false),
        };
        let this = Self::alloc(mtm).set_ivars(ivars);
        // SAFETY: `NSObject`'s `init` takes no arguments and returns the same object.
        unsafe { msg_send![super(this), init] }
    }

    /// Publish the control service with its notify/write characteristic pair.
    fn publish_service(&self, peripheral: &CBPeripheralManager) {
        if self.ivars().published.get() {
            return;
        }
        let mtm = self.mtm();
        // SAFETY: the UUID strings are valid Bluetooth UUIDs.
        let service_uuid = unsafe { CBUUID::UUIDWithString(&NSString::from_str(SERVICE_UUID)) };
        let notify_uuid =
            unsafe { CBUUID::UUIDWithString(&NSString::from_str(NOTIFY_CHARACTERISTIC_UUID)) };
        let write_uuid =
            unsafe { CBUUID::UUIDWithString(&NSString::from_str(WRITE_CHARACTERISTIC_UUID)) };

        // SAFETY: the property/permission combinations are the ones the headset uses. CoreBluetooth
        // adds the client-characteristic-configuration descriptor itself for notify
        // characteristics.
        let notify = unsafe {
            CBMutableCharacteristic::initWithType_properties_value_permissions(
                mtm.alloc::<CBMutableCharacteristic>(),
                &notify_uuid,
                CBCharacteristicProperties::Notify,
                None,
                CBAttributePermissions::Readable,
            )
        };
        // SAFETY: see above; a dynamic (nil) value makes the characteristic writeable.
        let write = unsafe {
            CBMutableCharacteristic::initWithType_properties_value_permissions(
                mtm.alloc::<CBMutableCharacteristic>(),
                &write_uuid,
                CBCharacteristicProperties::Write
                    | CBCharacteristicProperties::WriteWithoutResponse,
                None,
                CBAttributePermissions::Writeable,
            )
        };

        // SAFETY: the UUID string is a valid Bluetooth UUID.
        let service = unsafe {
            CBMutableService::initWithType_primary(
                mtm.alloc::<CBMutableService>(),
                &service_uuid,
                true,
            )
        };
        let characteristics =
            NSArray::from_slice(&[&*notify as &CBCharacteristic, &*write as &CBCharacteristic]);
        unsafe { service.setCharacteristics(Some(&characteristics)) };
        *self.ivars().notify.borrow_mut() = Some(notify);
        unsafe { peripheral.addService(&service) };
        info!(uuid = SERVICE_UUID, "publishing control service");
    }

    /// Start (or restart) advertising the control service under the current name.
    fn start_advertising(&self, peripheral: &CBPeripheralManager) {
        let ivars = self.ivars();
        let name = ivars.state.borrow().name.clone();

        // SAFETY: the advertisement dictionary holds only the types CoreBluetooth documents.
        let service_uuid = unsafe { CBUUID::UUIDWithString(&NSString::from_str(SERVICE_UUID)) };
        let uuid_list = NSArray::from_slice(&[&*service_uuid]);
        let advertised_name = NSString::from_str(&name);

        let advertisement: objc2::rc::Retained<NSMutableDictionary<NSString, AnyObject>> =
            NSMutableDictionary::new();
        advertisement.insert(
            unsafe { CBAdvertisementDataLocalNameKey },
            &*advertised_name,
        );
        advertisement.insert(unsafe { CBAdvertisementDataServiceUUIDsKey }, &*uuid_list);

        if ivars.advertising.get() {
            unsafe { peripheral.stopAdvertising() };
            ivars.advertising.set(false);
        }
        // NOTE: 128-bit service UUID + local name do not both fit the 28-byte advertisement, so
        // CoreBluetooth moves the name into the scan response. A scan therefore still sees both.
        unsafe { peripheral.startAdvertising(Some(&advertisement)) };
    }

    /// Send one reply frame over the notify characteristic.
    fn notify(&self, peripheral: &CBPeripheralManager, bytes: Vec<u8>) {
        *self.ivars().last_notification.borrow_mut() = bytes.clone();
        let data = NSData::with_bytes(&bytes);
        let Some(characteristic) = self.ivars().notify.borrow().clone() else {
            warn!("notification dropped: the notify characteristic is not published yet");
            return;
        };
        let sent = unsafe {
            peripheral.updateValue_forCharacteristic_onSubscribedCentrals(
                &data,
                &characteristic,
                None,
            )
        };
        if !sent {
            let mut pending = self.ivars().pending.borrow_mut();
            if pending.len() >= PENDING_NOTIFICATIONS {
                warn!("notification queue full, dropping the oldest entry");
                pending.pop_front();
            }
            pending.push_back(bytes);
            debug!(
                queued = pending.len(),
                "transmit queue full, notification queued"
            );
        }
    }

    /// Print the current state as one `emulator-state` line.
    fn print_state(&self) {
        match serde_json::to_string(&self.ivars().state.borrow().snapshot()) {
            Ok(json) => println!("emulator-state {json}"),
            Err(err) => warn!(error = %err, "cannot serialise the emulator state"),
        }
    }
}

// -------------------------------------------------------------------------------------------------
// Self discovery
// -------------------------------------------------------------------------------------------------

/// Find this very peripheral with a central scan and print its CoreBluetooth identifier.
///
/// CoreBluetooth exposes no API for a peripheral's own identifier, so the emulator runs a central
/// role scan (the same one `edifierctl scan` uses) and reports the identifier of the peripheral
/// that advertises the control service. This runs on its own thread with its own single-threaded
/// Tokio runtime; the main thread keeps running the CoreBluetooth run loop.
fn spawn_self_discovery(service_uuid: &'static str, name: String) {
    let spawned = std::thread::Builder::new()
        .name("self-discovery".to_string())
        .spawn(move || {
            let runtime = match tokio::runtime::Builder::new_current_thread()
                .enable_time()
                .build()
            {
                Ok(runtime) => runtime,
                Err(err) => {
                    warn!(error = %err, "cannot start the self-discovery runtime");
                    return;
                }
            };
            runtime.block_on(async move {
                info!("locating this peripheral with a central scan");
                let started = Instant::now();
                let mut warned = false;
                loop {
                    match edifier_link::ble::scan(Duration::from_secs(3), usize::MAX).await {
                        Ok(devices) => {
                            let found = devices.into_iter().find(|device| {
                                device
                                    .service
                                    .as_deref()
                                    .is_some_and(|uuid| uuid.eq_ignore_ascii_case(service_uuid))
                                    || device.name.as_deref() == Some(name.as_str())
                            });
                            if let Some(device) = found {
                                info!(identifier = %device.identifier, "peripheral visible to a local scan");
                                println!("emulator-ready {}", device.identifier);
                                return;
                            }
                        }
                        Err(err) => warn!(error = %err, "self-discovery scan failed"),
                    }
                    if !warned && started.elapsed() > Duration::from_secs(20) {
                        warned = true;
                        warn!(
                            "a local scan has not reported this peripheral yet; macOS CoreBluetooth \
                             does not deliver an advertisement created on this host back to a scan \
                             on the same host, so `emulator-ready` can only be printed where local \
                             advertisements are looped back (keep the emulator running: it retries)"
                        );
                    }
                    if warned {
                        // Nothing to find on this host; back off instead of scanning continuously.
                        tokio::time::sleep(Duration::from_secs(30)).await;
                    }
                }
            });
        });
    if let Err(err) = spawned {
        warn!(error = %err, "cannot spawn the self-discovery thread");
    }
}

// -------------------------------------------------------------------------------------------------
// Entry point
// -------------------------------------------------------------------------------------------------

fn init_tracing() {
    let filter = std::env::var("RUST_LOG").unwrap_or_else(|_| "emulate=info".to_string());
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::new(filter))
        .with_writer(std::io::stderr)
        .try_init();
}

fn main() -> Result<()> {
    let arguments: Vec<String> = std::env::args().skip(1).collect();
    if arguments.iter().any(|arg| arg == "--help" || arg == "-h") {
        print!("{USAGE}");
        return Ok(());
    }
    let args = Args::parse(arguments)?;
    init_tracing();

    let mtm = MainThreadMarker::new()
        .ok_or_else(|| anyhow!("the emulator must be created on the main thread"))?;
    let delegate = EmulatorDelegate::new(mtm, &args);

    // SAFETY: `mtm.alloc` allocates `CBPeripheralManager`; the delegate is a live object that
    // adopts `CBPeripheralManagerDelegate`, and `None` selects the main queue, which `main`
    // drains with `CFRunLoopRun`.
    //
    // `_manager` is deliberately a named binding: the manager owns the GATT database and must
    // outlive the run loop, while its delegate reference is weak, so `delegate` must too.
    let _manager = unsafe {
        CBPeripheralManager::initWithDelegate_queue(
            mtm.alloc::<CBPeripheralManager>(),
            Some(ProtocolObject::from_ref(&*delegate)),
            None,
        )
    };

    info!(
        name = %args.name,
        mac = %format_mac(&args.mac),
        battery = args.battery,
        "emulator starting — waiting for the peripheral manager to power on"
    );

    spawn_self_discovery(SERVICE_UUID, args.name.clone());

    run_loop(args.seconds);
    info!("emulator stopped");
    Ok(())
}

/// Run the main-thread run loop, which is what CoreBluetooth needs to deliver callbacks.
fn run_loop(seconds: Option<u64>) {
    match seconds {
        None => CFRunLoop::run(),
        Some(seconds) => {
            // `CFRunLoopRunInMode` returns once the timeout elapses, so loop until the whole
            // interval has passed while still servicing Bluetooth events. The default mode is a
            // common mode, which is where the main dispatch queue's source is registered.
            let mode = unsafe { kCFRunLoopDefaultMode };
            let deadline = Instant::now() + Duration::from_secs(seconds);
            loop {
                let remaining = deadline.saturating_duration_since(Instant::now());
                if remaining.is_zero() {
                    break;
                }
                CFRunLoop::run_in_mode(mode, remaining.as_secs_f64(), false);
            }
        }
    }
}

// -------------------------------------------------------------------------------------------------
// Protocol tests
// -------------------------------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    use edifier_link::{Session, Transport, TransportReader, TransportWriter};
    use edifier_proto::Command;
    use tokio::sync::Mutex as AsyncMutex;
    use tokio::sync::mpsc;

    fn emulator() -> Emulator {
        Emulator::new(&Args {
            mac: DEFAULT_MAC,
            name: DEFAULT_NAME.to_string(),
            battery: DEFAULT_BATTERY,
            seconds: None,
        })
    }

    fn hex(text: &str) -> Vec<u8> {
        let clean: String = text.chars().filter(char::is_ascii_hexdigit).collect();
        assert_eq!(clean.len() % 2, 0, "odd hex length in {text:?}");
        (0..clean.len() / 2)
            .map(|index| u8::from_str_radix(&clean[index * 2..index * 2 + 2], 16).expect("hex"))
            .collect()
    }

    /// Send one host payload and return the single reply, checking its checksum and header.
    fn reply(state: &mut Emulator, payload: &[u8], head: Head) -> Vec<u8> {
        let frame = Frame {
            head: Head::Host,
            payload: payload.to_vec(),
        };
        let (replies, _) = state.handle(&frame).expect("reply encodes");
        assert_eq!(replies.len(), 1, "expected exactly one reply");
        let encoded = replies.into_iter().next().expect("one reply");
        // `Frame::parse` rejects a wrong checksum, so this also proves the frame is valid.
        let parsed = Frame::parse(&encoded).expect("reply checksum");
        assert_eq!(parsed.head, head, "reply header");
        parsed.payload
    }

    #[test]
    fn captured_queries_reproduce_the_recorded_frames() {
        let mut state = emulator();
        // Every expectation below is a captured `readData:` frame from
        // `crates/edifier-proto/tests/vectors.rs`.
        let cases: &[(&[u8], &str)] = &[
            (&[0xD0], "BB02D04D21F3"),
            (&[0xCC], "BB03CC010621AA"),
            (&[0xC1, 0x01], "BB03C10106219F"),
            (&[0xC1, 0x02], "BB03C1020621A0"),
            (&[0xC1, 0x03, 0x09], "BB03C1030921A4"),
            (&[0xD5], "BB02D50021AB"),
            (&[0x08], "BB02080020DE"),
            (&[0x48], "BB024800211E"),
            (&[0x05], "BB02050720E2"),
            (&[0xF0, 0x0A], "BB03F00A0721D8"),
            (&[0xD3], "BB02D30021A9"),
            (&[0xD2], "CC02D20121BA"),
        ];
        for (payload, expected) in cases {
            let frame = Frame {
                head: Head::Host,
                payload: payload.to_vec(),
            };
            let (replies, _) = state.handle(&frame).expect("encode");
            assert_eq!(replies.len(), 1, "payload {payload:02X?}");
            assert_eq!(replies[0], hex(expected), "payload {payload:02X?}");
        }
    }

    #[test]
    fn every_documented_query_answers_with_the_documented_payload() {
        let mut state = emulator();
        assert_eq!(reply(&mut state, &[0xD0], Head::State), vec![0xD0, 77]);
        assert_eq!(
            reply(&mut state, &[0xC8], Head::State),
            [vec![0xC8], DEFAULT_MAC.to_vec()].concat()
        );
        assert_eq!(
            reply(&mut state, &[0xC6], Head::State),
            vec![0xC6, 0x03, 0x00, 0x02]
        );
        assert_eq!(
            reply(&mut state, &[0xC9], Head::State),
            [vec![0xC9], DEFAULT_NAME.as_bytes().to_vec()].concat()
        );
        assert_eq!(reply(&mut state, &[0xC3], Head::State), vec![0xC3, 0x00]);

        let fingerprint = reply(&mut state, &[0xD8], Head::State);
        // `docs/protocol.md` and the brief both give the reply as `BB 16 D8 …`, i.e. 22 payload
        // bytes: the command byte plus a 21-byte blob.
        assert_eq!(fingerprint.len(), 0x16);
        assert_eq!(fingerprint[0], 0xD8);
        assert_eq!(&fingerprint[1..], &DEFAULT_FINGERPRINT[..]);
    }

    #[test]
    fn setters_mutate_the_state_and_echo_the_value() {
        let mut state = emulator();
        assert_eq!(
            reply(&mut state, &[0x49, 0x02], Head::State),
            vec![0x49, 0x02]
        );
        assert_eq!(state.ldac.code(), 0x02);
        assert_eq!(reply(&mut state, &[0x48], Head::State), vec![0x48, 0x02]);

        assert_eq!(
            reply(&mut state, &[0x09, 0x01], Head::State),
            vec![0x09, 0x01]
        );
        assert_eq!(reply(&mut state, &[0x08], Head::State), vec![0x08, 0x01]);

        assert_eq!(
            reply(&mut state, &[0x06, 0x0F], Head::State),
            vec![0x06, 0x0F]
        );
        assert_eq!(reply(&mut state, &[0x05], Head::State), vec![0x05, 0x0F]);

        assert_eq!(
            reply(&mut state, &[0xF1, 0x0A, 0x05], Head::State),
            vec![0xF1, 0x0A, 0x05]
        );
        assert_eq!(
            reply(&mut state, &[0xF0, 0x0A], Head::State),
            vec![0xF0, 0x0A, 0x05]
        );

        assert_eq!(
            reply(&mut state, &[0xD6, 0x01], Head::State),
            vec![0xD6, 0x01]
        );
        assert_eq!(reply(&mut state, &[0xD7], Head::State), vec![0xD7, 0x01]);
    }

    #[test]
    fn ambient_volume_switches_to_ambient_mode() {
        let mut state = emulator();
        // `C1 03 03` is the captured ambient-volume -3 request.
        assert_eq!(
            reply(&mut state, &[0xC1, 0x03, 0x03], Head::State),
            vec![0xC1, 0x03, 0x03]
        );
        assert_eq!(state.anc, AncMode::Ambient);
        assert_eq!(state.ambient_volume.0, -3);
    }

    #[test]
    fn shutdown_timer_set_and_clear_use_both_documented_forms() {
        let mut state = emulator();
        assert_eq!(
            reply(&mut state, &[0xD1, 0x00, 0x05], Head::Ack),
            vec![0xD1, 0x01]
        );
        // The query then answers with the 16-bit `BB 03 D3 00 <minutes>` form.
        assert_eq!(
            reply(&mut state, &[0xD3], Head::State),
            vec![0xD3, 0x00, 0x05]
        );
        assert_eq!(reply(&mut state, &[0xD2], Head::Ack), vec![0xD2, 0x01]);
        assert_eq!(reply(&mut state, &[0xD3], Head::State), vec![0xD3, 0x00]);
    }

    #[test]
    fn set_commands_are_acknowledged_like_the_firmware_does() {
        let mut state = emulator();
        for (payload, expected) in [
            (vec![0xC4, 0x03], vec![0xC4, 0x01]),
            (vec![0xC2, 0x00], vec![0xC2, 0x01]),
            (vec![0xCD], vec![0xCD, 0x01]),
            (vec![0xCE], vec![0xCE, 0x01]),
            (vec![0xCF], vec![0xCF, 0x01]),
            (vec![0x07], vec![0x07, 0x01]),
        ] {
            assert_eq!(reply(&mut state, &payload, Head::Ack), expected);
        }
        assert_eq!(state.sound_effect, SoundEffect::Rock);
    }

    #[test]
    fn unknown_commands_are_acknowledged_with_the_request_echoed() {
        let mut state = emulator();
        assert_eq!(
            reply(&mut state, &[0x42, 0x01, 0x02], Head::Ack),
            vec![0x42, 0x01, 0x02]
        );
    }

    #[test]
    fn a_write_split_across_chunks_is_reassembled() {
        // A 24-byte name (the W820NB limit) makes the host frame 27 bytes, so the client sends it
        // as two 20-byte writes; the emulator must answer exactly once, after the last chunk.
        let rename = Command::SetName("ABCDEFGHIJKLMNOPQRSTUVWX".to_string())
            .encode()
            .expect("encode");
        assert!(rename.len() > 20, "rename must span chunks");

        let mut state = emulator();
        let mut decoder = FrameDecoder::new();
        let (first, _) = handle_chunk(&mut state, &mut decoder, &rename[..20]);
        assert!(first.is_empty(), "no reply before the last chunk");
        let (second, renamed) = handle_chunk(&mut state, &mut decoder, &rename[20..]);
        assert!(renamed);
        assert_eq!(second.len(), 1);
        assert_eq!(
            Frame::parse(&second[0]).expect("reply checksum").payload,
            vec![0xCA, 0x01]
        );
        assert_eq!(state.name, "ABCDEFGHIJKLMNOPQRSTUVWX");
        // The name query then reports the new name.
        assert_eq!(
            reply(&mut state, &[0xC9], Head::State),
            [vec![0xC9], state.name.clone().into_bytes()].concat()
        );
    }

    #[test]
    fn a_malformed_tail_is_dropped_and_the_next_frame_still_answers() {
        let mut state = emulator();
        let mut decoder = FrameDecoder::new();
        let (replies, _) = handle_chunk(&mut state, &mut decoder, &[0x00, 0xFF]);
        assert!(replies.is_empty());
        let good = Command::QueryBattery.encode().expect("encode");
        let (replies, _) = handle_chunk(&mut state, &mut decoder, &good);
        assert_eq!(replies.len(), 1);
        assert_eq!(
            Frame::parse(&replies[0]).expect("reply checksum").payload,
            vec![0xD0, 77]
        );
    }

    // ---------------------------------------------------------------------------------------------
    // Client-stack verification
    // ---------------------------------------------------------------------------------------------

    /// Wires the real `edifier-link` session to the emulator's state machine in memory.
    ///
    /// The radio is the only part of the stack that is not exercised: host frames reach
    /// [`handle_chunk`] exactly as a `CBATTRequest` would deliver them, and the replies go back
    /// through the client's own `FrameDecoder` and request correlation.
    struct LoopbackTransport {
        state: Arc<AsyncMutex<Emulator>>,
        decoder: Arc<AsyncMutex<FrameDecoder>>,
        device_to_client: mpsc::Sender<Vec<u8>>,
        client_from_device: mpsc::Receiver<Vec<u8>>,
    }

    struct LoopbackReader {
        client_from_device: mpsc::Receiver<Vec<u8>>,
    }

    struct LoopbackWriter {
        state: Arc<AsyncMutex<Emulator>>,
        decoder: Arc<AsyncMutex<FrameDecoder>>,
        device_to_client: mpsc::Sender<Vec<u8>>,
    }

    impl TransportReader for LoopbackReader {
        async fn next_chunk(&mut self) -> edifier_link::Result<Option<Vec<u8>>> {
            Ok(self.client_from_device.recv().await)
        }
    }

    impl TransportWriter for LoopbackWriter {
        async fn write_frame(&mut self, bytes: &[u8]) -> edifier_link::Result<()> {
            let (replies, _) = {
                let mut state = self.state.lock().await;
                let mut decoder = self.decoder.lock().await;
                handle_chunk(&mut state, &mut decoder, bytes)
            };
            for reply in replies {
                let _ = self.device_to_client.send(reply).await;
            }
            Ok(())
        }

        async fn close(&mut self) {}
    }

    impl Transport for LoopbackTransport {
        type Reader = LoopbackReader;
        type Writer = LoopbackWriter;

        fn split(self) -> (LoopbackReader, LoopbackWriter) {
            (
                LoopbackReader {
                    client_from_device: self.client_from_device,
                },
                LoopbackWriter {
                    state: self.state,
                    decoder: self.decoder,
                    device_to_client: self.device_to_client,
                },
            )
        }

        fn describe(&self) -> String {
            "loopback emulator".to_string()
        }
    }

    #[tokio::test]
    async fn the_client_session_drives_the_emulator_end_to_end() {
        let (device_to_client, client_from_device) = mpsc::channel(16);
        let transport = LoopbackTransport {
            state: Arc::new(AsyncMutex::new(emulator())),
            decoder: Arc::new(AsyncMutex::new(FrameDecoder::new())),
            device_to_client,
            client_from_device,
        };
        let session = Session::spawn(transport);

        // Battery (`D0`).
        let event = session
            .request(&Command::QueryBattery, Duration::from_secs(1))
            .await
            .expect("battery");
        assert_eq!(event.to_string(), "battery=77%");

        // ANC switch (`C1 02`) then the query (`CC`).
        let event = session
            .set_anc(AncMode::NoiseCancelling)
            .await
            .expect("set anc");
        assert_eq!(event.to_string(), "anc=noise-cancelling ambient_volume=0");
        let event = session
            .request(&Command::QueryAnc, Duration::from_secs(1))
            .await
            .expect("query anc");
        assert_eq!(event.to_string(), "anc=noise-cancelling ambient_volume=0");

        // Rename (`CA`) and read it back (`C9`).
        let event = session.set_name("Tested", None).await.expect("rename");
        assert_eq!(event.to_string(), "ack cmd=0xCA status=01");
        let event = session
            .request(&Command::QueryName, Duration::from_secs(1))
            .await
            .expect("query name");
        assert_eq!(event.to_string(), "name=Tested");

        // Raw escape hatch (`D0`).
        let event = session
            .request(&Command::Raw(vec![0xD0]), Duration::from_secs(1))
            .await
            .expect("raw battery");
        assert_eq!(event.to_string(), "battery=77%");

        // The full refresh burst the `status`/`doctor` commands use.
        let status = session.refresh().await;
        assert_eq!(status.battery, Some(77));
        assert_eq!(status.anc, Some(AncMode::NoiseCancelling));
        assert_eq!(status.name.as_deref(), Some("Tested"));
        assert_eq!(status.mac.as_deref(), Some("0C:AE:BD:BC:8F:A2"));
        assert_eq!(status.firmware.as_deref(), Some("03.00.02"));
        assert_eq!(status.ldac, Some(LdacMode::Off));
        assert_eq!(status.game_mode, Some(false));
        assert_eq!(status.prompt_volume, Some(7));
        assert_eq!(status.shutdown_timer, Some(ShutdownTimer::Disabled));
        assert_eq!(status.sound_effect, Some(SoundEffect::Normal));

        session.close().await;
    }
}
