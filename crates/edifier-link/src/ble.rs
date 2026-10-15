//! BLE GATT transport (the link Android prefers).
//!
//! The headset exposes one primary service in the `4809xxxx-1a48-11e9-ab14-d663bd873d93` family
//! (or `00001000-0000-1000-8991-00805f9b34fb` on the W800K) with a notify characteristic ending
//! in `…01` and a write characteristic ending in `…02`. Notifications carry device frames; writes
//! carry host frames, chunked 20 bytes at a time with a 50 ms gap, matching the captured
//! behaviour of the official app.

use std::time::Duration;

use btleplug::api::{Central, CharPropFlags, Manager as _, Peripheral as _, ScanFilter, WriteType};
use btleplug::platform::{Adapter, Manager, Peripheral};
use edifier_proto::model::{DeviceModel, EDIFIER_SERVICE_SUFFIX};
use futures::StreamExt;
use tokio::sync::mpsc;
use tokio::time::timeout;
use tracing::{debug, info, warn};
use uuid::Uuid;

use crate::transport::{LinkError, Result, Transport, TransportReader, TransportWriter};

/// Chunk size used when writing frames over BLE (`maxPacketLen` in mEDIFIER's `commble.h`).
pub const WRITE_CHUNK: usize = 20;
/// Delay between write chunks (`packetDelayMs` in mEDIFIER's `commble.h`).
pub const WRITE_CHUNK_DELAY: Duration = Duration::from_millis(50);
/// Name fragments that identify an Edifier control endpoint during discovery.
const EDIFIER_NAME_HINTS: [&str; 4] = ["EDIFIER", "W820NB", "W200BT", "W800K"];
/// The older W800K primary service UUID handled by mEDIFIER.
const W800K_SERVICE: &str = "00001000-0000-1000-8991-00805f9b34fb";
/// Depth of the notification queue; a burst larger than this means we cannot keep up.
const NOTIFICATION_QUEUE: usize = 64;
/// How long CoreBluetooth may take to hand over an adapter before we assume the process has no
/// Bluetooth permission (macOS shows a one-time prompt and CoreBluetooth blocks until it is
/// answered, so without this ceiling a first run would hang forever).
const ADAPTER_TIMEOUT: Duration = Duration::from_secs(20);

/// How a peripheral is located.
#[derive(Debug, Clone)]
pub enum Target {
    /// Match the advertised name, case-insensitive substring.
    Name(String),
    /// Match a CoreBluetooth peripheral identifier (the identifier macOS also shows as "UUID").
    Identifier(String),
    /// First peripheral that exposes an Edifier control service or advertises a known name.
    Auto,
}

impl Target {
    /// Interpret a user supplied selector: a 36-character UUID is a CoreBluetooth identifier,
    /// anything else is a name fragment.
    #[must_use]
    pub fn from_selector(selector: &str) -> Target {
        if selector.len() == 36 && selector.matches('-').count() == 4 {
            Target::Identifier(selector.to_string())
        } else {
            Target::Name(selector.to_string())
        }
    }

    fn matches_name(&self, local_name: Option<&str>) -> bool {
        match self {
            Target::Name(wanted) => {
                local_name.is_some_and(|name| name.to_lowercase().contains(&wanted.to_lowercase()))
            }
            _ => local_name.is_some_and(|name| {
                let upper = name.to_uppercase();
                EDIFIER_NAME_HINTS.iter().any(|hint| upper.contains(hint))
            }),
        }
    }
}

/// A discovered candidate peripheral.
#[derive(Debug, Clone, serde::Serialize)]
pub struct Discovered {
    /// CoreBluetooth identifier (stable across runs on macOS).
    pub identifier: String,
    /// Advertised local name.
    pub name: Option<String>,
    /// Signal strength in dBm when the advertisement carried it.
    pub rssi: Option<i16>,
    /// Edifier control service found in the advertisement, when present.
    pub service: Option<String>,
    /// Model guessed from the name and service.
    pub model: DeviceModel,
}

/// Scan for Edifier endpoints. Returns early once `expected` candidates are found.
pub async fn scan(duration: Duration, expected: usize) -> Result<Vec<Discovered>> {
    let adapter = adapter().await?;
    let step = Duration::from_millis(500);
    let mut elapsed = Duration::ZERO;
    let mut seen: Vec<Discovered> = Vec::new();

    adapter
        .start_scan(ScanFilter::default())
        .await
        .map_err(|err| LinkError::Adapter(err.to_string()))?;

    while elapsed < duration && seen.len() < expected {
        tokio::time::sleep(step).await;
        elapsed += step;
        for peripheral in adapter
            .peripherals()
            .await
            .map_err(|err| LinkError::Adapter(err.to_string()))?
        {
            let Ok(Some(properties)) = peripheral.properties().await else {
                continue;
            };
            let service = properties
                .services
                .iter()
                .map(Uuid::to_string)
                .find(|uuid| is_control_service(uuid));
            let name = properties.local_name.clone();
            if service.is_none() && !Target::Auto.matches_name(name.as_deref()) {
                continue;
            }
            let identifier = peripheral.id().to_string();
            if seen.iter().any(|entry| entry.identifier == identifier) {
                continue;
            }
            let model = service.as_deref().map_or_else(
                || {
                    name.as_deref()
                        .map_or(DeviceModel::Generic, DeviceModel::from_name)
                },
                DeviceModel::from_service_uuid,
            );
            seen.push(Discovered {
                identifier,
                name,
                rssi: properties.rssi,
                service,
                model,
            });
        }
    }

    let _ = adapter.stop_scan().await;
    Ok(seen)
}

fn is_control_service(uuid: &str) -> bool {
    uuid.contains(EDIFIER_SERVICE_SUFFIX) || uuid.eq_ignore_ascii_case(W800K_SERVICE)
}

async fn adapter() -> Result<Adapter> {
    let manager = timeout(ADAPTER_TIMEOUT, Manager::new())
        .await
        .map_err(|_| {
            LinkError::Adapter(
                "CoreBluetooth did not answer within 20s — if macOS is showing a Bluetooth \
                 permission prompt, allow it for this app, then retry"
                    .into(),
            )
        })?
        .map_err(|err| LinkError::Adapter(err.to_string()))?;
    let mut adapters = timeout(ADAPTER_TIMEOUT, manager.adapters())
        .await
        .map_err(|_| {
            LinkError::Adapter("CoreBluetooth did not list any adapter within 20s".into())
        })?
        .map_err(|err| LinkError::Adapter(err.to_string()))?;
    adapters.drain(..).next().ok_or_else(|| {
        LinkError::Adapter(
            "no Bluetooth adapter — check System Settings → Privacy & Security → Bluetooth".into(),
        )
    })
}

/// Connected BLE link, split into a notification reader and a characteristic writer.
pub struct BleTransport {
    reader: BleReader,
    writer: BleWriter,
    description: String,
    model: DeviceModel,
}

/// Notification half.
pub struct BleReader {
    chunks: mpsc::Receiver<Vec<u8>>,
}

/// Characteristic half.
pub struct BleWriter {
    peripheral: Peripheral,
    write_char: btleplug::api::Characteristic,
}

impl BleTransport {
    /// Model guessed during discovery.
    pub fn model(&self) -> DeviceModel {
        self.model
    }
}

impl Transport for BleTransport {
    type Reader = BleReader;
    type Writer = BleWriter;

    fn split(self) -> (BleReader, BleWriter) {
        (self.reader, self.writer)
    }

    fn describe(&self) -> String {
        self.description.clone()
    }
}

impl TransportReader for BleReader {
    async fn next_chunk(&mut self) -> Result<Option<Vec<u8>>> {
        Ok(self.chunks.recv().await)
    }
}

impl TransportWriter for BleWriter {
    async fn write_frame(&mut self, bytes: &[u8]) -> Result<()> {
        for (index, chunk) in bytes.chunks(WRITE_CHUNK).enumerate() {
            if index > 0 {
                tokio::time::sleep(WRITE_CHUNK_DELAY).await;
            }
            debug!(payload = %hex(chunk), "ble write");
            self.peripheral
                .write(&self.write_char, chunk, WriteType::WithResponse)
                .await
                .map_err(|err| LinkError::Transport(err.to_string()))?;
        }
        Ok(())
    }

    async fn close(&mut self) {
        if let Err(err) = self.peripheral.disconnect().await {
            debug!(error = %err, "disconnect failed");
        }
    }
}

/// Connect to a peripheral, subscribe to its control notifications and pump them into a queue.
///
/// # Errors
///
/// Returns [`LinkError::NotFound`] when discovery times out, [`LinkError::Connect`] when the
/// peripheral refuses the link and [`LinkError::NoControlService`] when the control
/// characteristics are missing.
pub async fn connect(target: &Target, scan_timeout: Duration) -> Result<BleTransport> {
    let adapter = adapter().await?;
    adapter
        .start_scan(ScanFilter::default())
        .await
        .map_err(|err| LinkError::Adapter(err.to_string()))?;

    let deadline = tokio::time::Instant::now() + scan_timeout;
    let peripheral = loop {
        if let Some(found) = select_peripheral(&adapter, target).await? {
            break found;
        }
        if tokio::time::Instant::now() >= deadline {
            let _ = adapter.stop_scan().await;
            return Err(LinkError::NotFound(match target {
                Target::Name(name) | Target::Identifier(name) => name.clone(),
                Target::Auto => "any Edifier control endpoint".into(),
            }));
        }
        tokio::time::sleep(Duration::from_millis(400)).await;
    };
    let _ = adapter.stop_scan().await;

    let properties = peripheral
        .properties()
        .await
        .ok()
        .flatten()
        .unwrap_or_default();
    info!(device = ?properties.local_name, id = %peripheral.id(), "connecting");
    timeout(
        scan_timeout.max(Duration::from_secs(15)),
        peripheral.connect(),
    )
    .await
    .map_err(|_| LinkError::Timeout(scan_timeout))?
    .map_err(|err| LinkError::Connect(err.to_string()))?;

    peripheral
        .discover_services()
        .await
        .map_err(|err| LinkError::Connect(err.to_string()))?;

    let (rx, tx, service_uuid) =
        find_control_characteristics(&peripheral).ok_or(LinkError::NoControlService)?;

    if let Err(err) = peripheral.subscribe(&rx).await {
        let _ = peripheral.disconnect().await;
        return Err(LinkError::Connect(err.to_string()));
    }
    let mut notifications = peripheral
        .notifications()
        .await
        .map_err(|err| LinkError::Connect(err.to_string()))?;
    let rx_uuid = rx.uuid;

    let (sender, chunks) = mpsc::channel(NOTIFICATION_QUEUE);
    tokio::spawn(async move {
        while let Some(value) = notifications.next().await {
            if value.uuid != rx_uuid {
                continue;
            }
            let len = value.value.len();
            debug!(len, "ble notification");
            if sender.send(value.value).await.is_err() {
                break;
            }
        }
        warn!("ble notification stream ended");
    });

    let model = DeviceModel::from_service_uuid(&service_uuid.to_string());
    let description = format!(
        "BLE {} ({})",
        properties.local_name.as_deref().unwrap_or("Edifier"),
        model.display_name()
    );

    Ok(BleTransport {
        reader: BleReader { chunks },
        writer: BleWriter {
            peripheral,
            write_char: tx,
        },
        description,
        model,
    })
}

/// Pick the best candidate: explicit identifier first, then name, then any Edifier-looking one.
async fn select_peripheral(adapter: &Adapter, target: &Target) -> Result<Option<Peripheral>> {
    let mut by_name = None;
    let mut by_hint = None;
    for candidate in adapter
        .peripherals()
        .await
        .map_err(|err| LinkError::Adapter(err.to_string()))?
    {
        if matches!(target, Target::Identifier(wanted)
            if candidate.id().to_string().eq_ignore_ascii_case(wanted))
        {
            return Ok(Some(candidate));
        }
        let Ok(Some(properties)) = candidate.properties().await else {
            continue;
        };
        if matches!(target, Target::Name(_))
            && target.matches_name(properties.local_name.as_deref())
        {
            by_name.get_or_insert(candidate);
            continue;
        }
        let advertises_service = properties
            .services
            .iter()
            .any(|uuid| is_control_service(&uuid.to_string()));
        if advertises_service || Target::Auto.matches_name(properties.local_name.as_deref()) {
            by_hint.get_or_insert(candidate);
        }
    }
    Ok(by_name.or(by_hint))
}

/// Locate the notify/write pair inside the Edifier control service.
fn find_control_characteristics(
    peripheral: &Peripheral,
) -> Option<(
    btleplug::api::Characteristic,
    btleplug::api::Characteristic,
    Uuid,
)> {
    for service in peripheral.services() {
        if !is_control_service(&service.uuid.to_string()) {
            continue;
        }
        let mut rx = None;
        let mut tx = None;
        for characteristic in &service.characteristics {
            let uuid = characteristic.uuid.to_string().to_lowercase();
            let special_rx =
                uuid.starts_with("48090001") || uuid.starts_with("00001000-0000-1000-8992");
            let special_tx =
                uuid.starts_with("48090002") || uuid.starts_with("00001000-0000-1000-8993");
            if rx.is_none()
                && characteristic.properties.contains(CharPropFlags::NOTIFY)
                && (special_rx || uuid.contains("2-1a48-11e9-ab14-d663bd873d93"))
            {
                rx = Some(characteristic.clone());
            }
            if tx.is_none()
                && characteristic.properties.contains(CharPropFlags::WRITE)
                && (special_tx || uuid.contains("3-1a48-11e9-ab14-d663bd873d93"))
            {
                tx = Some(characteristic.clone());
            }
        }
        if let (Some(rx), Some(tx)) = (rx, tx) {
            return Some((rx, tx, service.uuid));
        }
    }
    None
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    bytes
        .iter()
        .fold(String::with_capacity(bytes.len() * 2), |mut out, byte| {
            let _ = write!(out, "{byte:02X}");
            out
        })
}
