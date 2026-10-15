//! Reach a control endpoint that macOS is *already* connected to.
//!
//! Once macOS holds the BLE link to the headset (`EDIFIER BLE` shows as connected in System
//! Settings and in `system_profiler SPBluetoothDataType`), the peripheral stops advertising: a scan
//! — and therefore [`crate::ble`] — can no longer see it, even though the GATT link is up and
//! usable. CoreBluetooth hands such peripherals over through
//! `-[CBCentralManager retrieveConnectedPeripheralsWithServices:]`, which this module uses.
//!
//! # Threading
//!
//! CoreBluetooth objects (`CBCentralManager`, `CBPeripheral`, …) are deliberately `!Send` in these
//! bindings, so they are confined to one dedicated thread that owns them for their whole life. The
//! manager is created with its own serial dispatch queue, so delegate callbacks are serviced by
//! libdispatch and are turned into plain data ([`Callback`]) that is sent to the owning thread over
//! a channel. The async side only exchanges bytes and requests with that thread.

use std::sync::mpsc as sync_mpsc;
use std::time::Duration;

use objc2::rc::Retained;
use objc2::runtime::{NSObject, NSObjectProtocol, ProtocolObject};
use objc2::{AnyThread, DefinedClass, define_class, msg_send};
use objc2_core_bluetooth::{
    CBCentralManager, CBCentralManagerDelegate, CBCharacteristic, CBCharacteristicProperties,
    CBCharacteristicWriteType, CBManagerState, CBPeripheral, CBPeripheralDelegate,
    CBPeripheralState, CBService, CBUUID,
};
use objc2_foundation::{NSArray, NSData, NSError, NSString, NSUUID};
use tokio::sync::{mpsc, oneshot};
use tracing::{debug, info, warn};

use dispatch2::{DispatchQueue, DispatchRetained};

use crate::ble::{Target, WRITE_CHUNK, WRITE_CHUNK_DELAY};
use crate::transport::{LinkError, Result, Transport, TransportReader, TransportWriter};

/// Control service advertised by the W820NB family.
pub const DEFAULT_SERVICE: &str = "48093801-1a48-11e9-ab14-d663bd873d93";

/// Name fragments that identify a headset when no explicit selector was supplied.
const EDIFIER_NAME_HINTS: [&str; 4] = ["EDIFIER", "W820NB", "W200BT", "W800K"];

/// How long the manager may take to report `PoweredOn`.
const STATE_TIMEOUT: Duration = Duration::from_secs(10);
/// How long link/characteristic discovery may take.
const DISCOVERY_TIMEOUT: Duration = Duration::from_secs(15);
/// Poll interval of the owning thread.
const POLL_INTERVAL: Duration = Duration::from_millis(20);

/// A control endpoint the system already connected.
#[derive(Debug, Clone, serde::Serialize)]
pub struct ConnectedEndpoint {
    /// CoreBluetooth identifier reported for the peripheral.
    pub identifier: String,
    /// Name the system kept for it.
    pub name: Option<String>,
    /// Whether the link is up right now.
    pub connected: bool,
}

/// Data extracted from a delegate callback (the only values that cross threads).
#[derive(Debug, Clone)]
enum Callback {
    Connected(String),
    ConnectFailed(String, String),
    ServicesDiscovered(String, Option<String>),
    CharacteristicsDiscovered(String, Option<String>),
    Notified(Vec<u8>),
    Disconnected(String, Option<String>),
}

/// Request from the async side to the owning thread.
enum Request {
    Write(Vec<u8>),
}

struct DelegateIvars {
    events: sync_mpsc::Sender<Callback>,
}

define_class!(
    // SAFETY: NSObject has no subclassing requirements and this class implements no `Drop`.
    #[unsafe(super(NSObject))]
    #[name = "EdifierSystemCentralDelegate"]
    #[ivars = DelegateIvars]
    struct SystemDelegate;

    unsafe impl NSObjectProtocol for SystemDelegate {}

    unsafe impl CBCentralManagerDelegate for SystemDelegate {
        #[unsafe(method(centralManagerDidUpdateState:))]
        fn did_update_state(&self, central: &CBCentralManager) {
            // The owning thread polls `state()` instead; this method only exists to satisfy the
            // protocol and to log the transition.
            let state = unsafe { central.state() };
            debug!(state = state.0, "central manager state changed");
        }

        #[unsafe(method(centralManager:didConnectPeripheral:))]
        fn did_connect(&self, _central: &CBCentralManager, peripheral: &CBPeripheral) {
            emit(self, Callback::Connected(identifier_of(peripheral)));
        }

        #[unsafe(method(centralManager:didFailToConnectPeripheral:error:))]
        fn did_fail_to_connect(
            &self,
            _central: &CBCentralManager,
            peripheral: &CBPeripheral,
            error: Option<&NSError>,
        ) {
            emit(
                self,
                Callback::ConnectFailed(identifier_of(peripheral), describe_error(error)),
            );
        }

        #[unsafe(method(centralManager:didDisconnectPeripheral:error:))]
        fn did_disconnect(
            &self,
            _central: &CBCentralManager,
            peripheral: &CBPeripheral,
            error: Option<&NSError>,
        ) {
            emit(
                self,
                Callback::Disconnected(identifier_of(peripheral), error_text(error)),
            );
        }
    }

    unsafe impl CBPeripheralDelegate for SystemDelegate {
        #[unsafe(method(peripheral:didDiscoverServices:))]
        fn did_discover_services(&self, peripheral: &CBPeripheral, error: Option<&NSError>) {
            emit(
                self,
                Callback::ServicesDiscovered(identifier_of(peripheral), error_text(error)),
            );
        }

        #[unsafe(method(peripheral:didDiscoverCharacteristicsForService:error:))]
        fn did_discover_characteristics(
            &self,
            peripheral: &CBPeripheral,
            _service: &CBService,
            error: Option<&NSError>,
        ) {
            emit(
                self,
                Callback::CharacteristicsDiscovered(identifier_of(peripheral), error_text(error)),
            );
        }

        #[unsafe(method(peripheral:didUpdateValueForCharacteristic:error:))]
        fn did_update_value(
            &self,
            _peripheral: &CBPeripheral,
            characteristic: &CBCharacteristic,
            _error: Option<&NSError>,
        ) {
            // SAFETY: `value` is a getter returning the notification payload.
            if let Some(value) = unsafe { characteristic.value() } {
                emit(self, Callback::Notified(value.to_vec()));
            }
        }
    }
);

fn emit(delegate: &SystemDelegate, callback: Callback) {
    let _ = delegate.ivars().events.send(callback);
}

fn identifier_of(peripheral: &CBPeripheral) -> String {
    // SAFETY: `identifier` is a side-effect-free getter.
    let uuid: Retained<NSUUID> = unsafe { peripheral.identifier() };
    uuid.UUIDString().to_string()
}

fn name_of(peripheral: &CBPeripheral) -> Option<String> {
    // SAFETY: `name` is a side-effect-free getter.
    unsafe { peripheral.name() }.map(|name| name.to_string())
}

fn describe_error(error: Option<&NSError>) -> String {
    error.map_or_else(|| "unknown error".to_string(), ToString::to_string)
}

/// Optional error text for callbacks that only report "something went wrong".
fn error_text(error: Option<&NSError>) -> Option<String> {
    error.map(|error| describe_error(Some(error)))
}

fn text_uuid(uuid: &CBUUID) -> String {
    // SAFETY: `UUIDString` is a getter that formats the UUID.
    unsafe { uuid.UUIDString() }.to_string()
}

fn cb_uuid(text: &str) -> Retained<CBUUID> {
    let text = NSString::from_str(text);
    // SAFETY: `UUIDWithString` parses the string and returns a CBUUID; the input is a valid UUID.
    unsafe { CBUUID::UUIDWithString(&text) }
}

fn ns_data(bytes: &[u8]) -> Retained<NSData> {
    // SAFETY: `initWithBytes:length:` copies `length` bytes from the pointer, so the borrow ends
    // with the call and no reference escapes.
    unsafe {
        NSData::initWithBytes_length(
            NSData::alloc(),
            bytes.as_ptr().cast::<std::ffi::c_void>(),
            bytes.len(),
        )
    }
}

/// Objects owned by the dedicated CoreBluetooth thread.
struct Owner {
    central: Retained<CBCentralManager>,
    peripheral: Retained<CBPeripheral>,
    write: Option<Retained<CBCharacteristic>>,
    #[allow(dead_code, reason = "kept so the delegate outlives the manager")]
    delegate: Retained<SystemDelegate>,
    #[allow(dead_code, reason = "kept so the dispatch queue outlives the manager")]
    queue: DispatchRetained<DispatchQueue>,
    callbacks: sync_mpsc::Receiver<Callback>,
    incoming: mpsc::Sender<Vec<u8>>,
}

impl Owner {
    /// Wait until a callback satisfies `decide`, polling with a deadline.
    fn wait_for<T>(
        &mut self,
        timeout: Duration,
        mut decide: impl FnMut(Callback) -> Option<Result<T>>,
    ) -> Result<T> {
        let deadline = std::time::Instant::now() + timeout;
        loop {
            match self.callbacks.recv_timeout(POLL_INTERVAL) {
                Ok(callback) => {
                    if let Some(outcome) = decide(callback) {
                        return outcome;
                    }
                }
                Err(sync_mpsc::RecvTimeoutError::Timeout) => {}
                Err(sync_mpsc::RecvTimeoutError::Disconnected) => return Err(LinkError::Closed),
            }
            if std::time::Instant::now() >= deadline {
                return Err(LinkError::Timeout(timeout));
            }
        }
    }

    /// Service loop: forward notifications, execute writes, stop when the caller drops the link.
    fn serve(&mut self, requests: sync_mpsc::Receiver<Request>) {
        loop {
            match requests.recv_timeout(POLL_INTERVAL) {
                Ok(Request::Write(chunk)) => {
                    let Some(write) = self.write.as_ref() else {
                        warn!("write requested before the control characteristic was discovered");
                        continue;
                    };
                    let data = ns_data(&chunk);
                    // SAFETY: `writeValue:forCharacteristic:type:` copies the data and returns
                    // nothing; the characteristic is alive for the whole loop.
                    unsafe {
                        self.peripheral.writeValue_forCharacteristic_type(
                            &data,
                            write,
                            CBCharacteristicWriteType::WithResponse,
                        );
                    }
                }
                Err(sync_mpsc::RecvTimeoutError::Timeout) => {}
                Err(sync_mpsc::RecvTimeoutError::Disconnected) => break,
            }

            while let Ok(callback) = self.callbacks.try_recv() {
                match callback {
                    Callback::Notified(bytes) => {
                        if self.incoming.blocking_send(bytes).is_err() {
                            return;
                        }
                    }
                    Callback::Disconnected(identifier, error) => {
                        warn!(%identifier, error = ?error, "system BLE link disconnected");
                        return;
                    }
                    other => debug!(?other, "ignoring late callback"),
                }
            }
        }
        // SAFETY: asking the system to drop the link; objects stay alive until this struct drops.
        unsafe {
            self.central.cancelPeripheralConnection(&self.peripheral);
        }
    }
}

/// Reader half: notifications forwarded from the owning thread.
pub struct SystemReader {
    incoming: mpsc::Receiver<Vec<u8>>,
}

/// Writer half: chunked writes handed to the owning thread.
pub struct SystemWriter {
    requests: sync_mpsc::Sender<Request>,
}

/// Transport over a system-connected peripheral.
pub struct SystemTransport {
    reader: SystemReader,
    writer: SystemWriter,
    description: String,
    model: edifier_proto::DeviceModel,
}

impl SystemTransport {
    /// Model guessed from the peripheral's name.
    #[must_use]
    pub fn model(&self) -> edifier_proto::DeviceModel {
        self.model
    }
}

impl Transport for SystemTransport {
    type Reader = SystemReader;
    type Writer = SystemWriter;

    fn split(self) -> (SystemReader, SystemWriter) {
        (self.reader, self.writer)
    }

    fn describe(&self) -> String {
        self.description.clone()
    }
}

impl TransportReader for SystemReader {
    async fn next_chunk(&mut self) -> Result<Option<Vec<u8>>> {
        Ok(self.incoming.recv().await)
    }
}

impl TransportWriter for SystemWriter {
    async fn write_frame(&mut self, bytes: &[u8]) -> Result<()> {
        for (index, chunk) in bytes.chunks(WRITE_CHUNK).enumerate() {
            if index > 0 {
                tokio::time::sleep(WRITE_CHUNK_DELAY).await;
            }
            self.requests
                .send(Request::Write(chunk.to_vec()))
                .map_err(|_| LinkError::Closed)?;
        }
        Ok(())
    }

    async fn close(&mut self) {
        // Dropping the request channel ends the owner thread's loop, which then releases the link.
        self.requests = sync_mpsc::channel().0;
    }
}

/// Control endpoints macOS already has connected for the headset service.
///
/// # Errors
///
/// See [`connected_endpoints`].
pub async fn edifier_endpoints(timeout: Duration) -> Result<Vec<ConnectedEndpoint>> {
    connected_endpoints(DEFAULT_SERVICE, timeout).await
}

/// Query the peripherals macOS already has connected for `service`.
///
/// # Errors
///
/// Fails when CoreBluetooth never reaches the powered-on state, or when the query cannot run.
pub async fn connected_endpoints(
    service: &str,
    timeout: Duration,
) -> Result<Vec<ConnectedEndpoint>> {
    let service = service.to_string();
    let (ready, waiting) = oneshot::channel();
    std::thread::Builder::new()
        .name("edifier-cb-query".into())
        .spawn(move || {
            let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                query_endpoints(&service, timeout)
            }))
            .unwrap_or_else(|_| Err(LinkError::Other("CoreBluetooth query panicked".into())));
            let _ = ready.send(outcome);
        })
        .map_err(|err| LinkError::Other(format!("cannot start the CoreBluetooth thread: {err}")))?;

    waiting.await.map_err(|_| LinkError::Closed)?
}

fn query_endpoints(service: &str, timeout: Duration) -> Result<Vec<ConnectedEndpoint>> {
    let (callbacks_tx, _callbacks_rx) = sync_mpsc::channel();
    let (delegate, central, _queue) = make_manager(callbacks_tx)?;
    wait_until_powered_on(&central, timeout)?;

    let services = NSArray::from_retained_slice(&[cb_uuid(service)]);
    // SAFETY: `retrieveConnectedPeripheralsWithServices:` returns a snapshot array; both the
    // manager and the query array outlive the call.
    let peripherals: Retained<NSArray<CBPeripheral>> =
        unsafe { central.retrieveConnectedPeripheralsWithServices(&services) };

    let mut out = Vec::new();
    for index in 0..peripherals.count() {
        let peripheral = peripherals.objectAtIndex(index);
        out.push(ConnectedEndpoint {
            identifier: identifier_of(&peripheral),
            name: name_of(&peripheral),
            // SAFETY: `state` is a getter.
            connected: unsafe { peripheral.state() } == CBPeripheralState::Connected,
        });
    }
    debug!(
        count = out.len(),
        service, "system-connected control endpoints"
    );
    drop(delegate);
    Ok(out)
}

type Manager = (
    Retained<SystemDelegate>,
    Retained<CBCentralManager>,
    DispatchRetained<DispatchQueue>,
);

fn make_manager(callbacks: sync_mpsc::Sender<Callback>) -> Result<Manager> {
    let delegate: Retained<SystemDelegate> = {
        let allocated = SystemDelegate::alloc().set_ivars(DelegateIvars { events: callbacks });
        // SAFETY: `init` on a freshly allocated NSObject subclass with its ivars set.
        unsafe { msg_send![super(allocated), init] }
    };
    let queue = DispatchQueue::new("sh.edifier.system-central", None);
    // SAFETY: `initWithDelegate:queue:` is the documented initialiser. CoreBluetooth retains the
    // delegate and the queue for the manager's lifetime; both are also returned to the caller.
    let central: Retained<CBCentralManager> = unsafe {
        CBCentralManager::initWithDelegate_queue(
            CBCentralManager::alloc(),
            Some(ProtocolObject::from_ref(&*delegate)),
            Some(&queue),
        )
    };
    Ok((delegate, central, queue))
}

fn wait_until_powered_on(central: &CBCentralManager, timeout: Duration) -> Result<()> {
    let deadline = std::time::Instant::now() + timeout.max(STATE_TIMEOUT);
    loop {
        // SAFETY: `state` is a getter.
        let state = unsafe { central.state() };
        if state == CBManagerState::PoweredOn {
            return Ok(());
        }
        if std::time::Instant::now() >= deadline {
            return Err(LinkError::Adapter(format!(
                "CoreBluetooth stayed in state {} (powered-on expected): check that Bluetooth is \
                 on and that this app is allowed in System Settings → Privacy & Security → Bluetooth",
                state.0
            )));
        }
        std::thread::sleep(POLL_INTERVAL);
    }
}

/// Connect to an already-connected control endpoint and subscribe to its notifications.
///
/// # Errors
///
/// [`LinkError::NotFound`] when no connected endpoint matches `target`,
/// [`LinkError::Connect`]/[`LinkError::Timeout`] when the handshake fails and
/// [`LinkError::NoControlService`] when the notify/write characteristics are missing.
pub async fn connect(target: &Target, service: &str, timeout: Duration) -> Result<SystemTransport> {
    let service = service.to_string();
    let target = target.clone();
    let (ready, waiting) = oneshot::channel();
    let (incoming_tx, incoming) = mpsc::channel(64);
    let (requests_tx, requests_rx) = sync_mpsc::channel();

    std::thread::Builder::new()
        .name("edifier-cb".into())
        .spawn(move || {
            // CoreBluetooth objects are created on this thread and never leave it.
            let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                handshake(&service, &target, timeout, incoming_tx)
            }));
            match outcome {
                Ok(Ok((mut owner, connection))) => {
                    if ready.send(Ok(connection)).is_err() {
                        debug!("caller dropped before the handshake completed");
                        return;
                    }
                    owner.serve(requests_rx);
                }
                Ok(Err(err)) => {
                    let _ = ready.send(Err(err));
                }
                Err(_) => {
                    let _ = ready.send(Err(LinkError::Other(
                        "CoreBluetooth handshake panicked".into(),
                    )));
                }
            }
        })
        .map_err(|err| LinkError::Other(format!("cannot start the CoreBluetooth thread: {err}")))?;

    let connection = waiting.await.map_err(|_| LinkError::Closed)??;
    let description = format!(
        "BLE {} (system-connected)",
        connection.name.as_deref().unwrap_or("Edifier")
    );
    info!(identifier = %connection.identifier, "system BLE link established");

    let model = connection.name.as_deref().map_or(
        edifier_proto::DeviceModel::Generic,
        edifier_proto::DeviceModel::from_name,
    );

    Ok(SystemTransport {
        reader: SystemReader { incoming },
        writer: SystemWriter {
            requests: requests_tx,
        },
        description,
        model,
    })
}

/// Everything the owning thread needs to hold a live, subscribed link.
struct Connection {
    identifier: String,
    name: Option<String>,
}

fn handshake(
    service: &str,
    target: &Target,
    timeout: Duration,
    incoming: mpsc::Sender<Vec<u8>>,
) -> Result<(Owner, Connection)> {
    let (callbacks_tx, callbacks) = sync_mpsc::channel();
    let (delegate, central, queue) = make_manager(callbacks_tx)?;
    wait_until_powered_on(&central, timeout)?;

    let services = NSArray::from_retained_slice(&[cb_uuid(service)]);
    // SAFETY: `retrieveConnectedPeripheralsWithServices:` returns a snapshot array; manager and
    // query array outlive the call.
    let candidates: Retained<NSArray<CBPeripheral>> =
        unsafe { central.retrieveConnectedPeripheralsWithServices(&services) };

    let peripheral = (0..candidates.count())
        .map(|index| candidates.objectAtIndex(index))
        .find(|peripheral| matches_target(peripheral, target))
        .ok_or_else(|| {
            LinkError::NotFound(if candidates.count() == 0 {
                format!(
                    "macOS holds no connected BLE endpoint for service {service}; power the \
                     headset on, or connect it once in System Settings → Bluetooth"
                )
            } else {
                format!(
                    "{target:?} is not among the {} connected endpoint(s) for {service}",
                    candidates.count()
                )
            })
        })?;

    let identifier = identifier_of(&peripheral);
    let name = name_of(&peripheral);
    info!(%identifier, ?name, "attaching to the system-connected peripheral");

    // SAFETY: `setDelegate:` installs the peripheral callbacks; the delegate lives as long as this
    // thread (kept in `Owner`).
    unsafe { peripheral.setDelegate(Some(ProtocolObject::from_ref(&*delegate))) };
    // SAFETY: requesting the link; already connected peripherals report Connected immediately.
    unsafe { central.connectPeripheral_options(&peripheral, None) };

    let mut owner = Owner {
        central,
        peripheral,
        write: None,
        delegate,
        queue,
        callbacks,
        incoming,
    };
    owner.wait_for(DISCOVERY_TIMEOUT.max(timeout), |callback| match callback {
        Callback::Connected(id) if id == identifier => Some(Ok(())),
        Callback::ConnectFailed(id, error) if id == identifier => {
            Some(Err(LinkError::Connect(format!("{identifier}: {error}"))))
        }
        _ => None,
    })?;

    // SAFETY: starts service discovery; the callback arrives on the dispatch queue.
    unsafe { owner.peripheral.discoverServices(Some(&services)) };
    owner.wait_for(DISCOVERY_TIMEOUT, |callback| match callback {
        Callback::ServicesDiscovered(id, error) if id == identifier => {
            Some(error.map_or(Ok(()), |error| Err(LinkError::Connect(error))))
        }
        _ => None,
    })?;

    // SAFETY: `services` returns the discovered services once discovery completed.
    let discovered = unsafe { owner.peripheral.services() }.ok_or(LinkError::NoControlService)?;
    let control = (0..discovered.count())
        .map(|index| discovered.objectAtIndex(index))
        // SAFETY: `UUID` is a getter.
        .find(|candidate| {
            // SAFETY: `UUID` is a getter.
            let uuid = unsafe { candidate.UUID() };
            text_uuid(&uuid).eq_ignore_ascii_case(service)
        })
        .ok_or(LinkError::NoControlService)?;

    // SAFETY: starts characteristic discovery for the control service.
    unsafe {
        owner
            .peripheral
            .discoverCharacteristics_forService(None, &control);
    }
    owner.wait_for(DISCOVERY_TIMEOUT, |callback| match callback {
        Callback::CharacteristicsDiscovered(id, error) if id == identifier => {
            Some(error.map_or(Ok(()), |error| Err(LinkError::Connect(error))))
        }
        _ => None,
    })?;

    let (notify, write) =
        find_control_characteristics(&control).ok_or(LinkError::NoControlService)?;
    // SAFETY: enables notifications on the control characteristic; delivery is proven by the first
    // `didUpdateValueForCharacteristic` callback.
    unsafe {
        owner
            .peripheral
            .setNotifyValue_forCharacteristic(true, &notify)
    };
    owner.write = Some(write);

    Ok((owner, Connection { identifier, name }))
}

fn matches_target(peripheral: &CBPeripheral, target: &Target) -> bool {
    let identifier = identifier_of(peripheral);
    let name = name_of(peripheral);
    match target {
        Target::Identifier(wanted) => identifier.eq_ignore_ascii_case(wanted),
        Target::Name(wanted) => name
            .as_deref()
            .is_some_and(|name| name.to_lowercase().contains(&wanted.to_lowercase())),
        Target::Auto => name.as_deref().is_some_and(|name| {
            let upper = name.to_uppercase();
            EDIFIER_NAME_HINTS.iter().any(|hint| upper.contains(hint))
        }),
    }
}

fn find_control_characteristics(
    service: &CBService,
) -> Option<(Retained<CBCharacteristic>, Retained<CBCharacteristic>)> {
    // SAFETY: `characteristics` returns the discovered characteristics.
    let characteristics = unsafe { service.characteristics() }?;
    let mut notify = None;
    let mut write = None;
    for index in 0..characteristics.count() {
        let characteristic = characteristics.objectAtIndex(index);
        // SAFETY: `UUID` and `properties` are getters.
        let (uuid, properties) = unsafe { (characteristic.UUID(), characteristic.properties()) };
        let uuid = text_uuid(&uuid).to_lowercase();

        let notifies = properties.0 & CBCharacteristicProperties::Notify.0 != 0;
        let writable = properties.0
            & (CBCharacteristicProperties::Write.0
                | CBCharacteristicProperties::WriteWithoutResponse.0)
            != 0;
        let special_notify =
            uuid.starts_with("48090001") || uuid.starts_with("00001000-0000-1000-8992");
        let special_write =
            uuid.starts_with("48090002") || uuid.starts_with("00001000-0000-1000-8993");
        if notify.is_none()
            && notifies
            && (special_notify || uuid.contains("2-1a48-11e9-ab14-d663bd873d93"))
        {
            notify = Some(characteristic);
            continue;
        }
        if write.is_none()
            && writable
            && (special_write || uuid.contains("3-1a48-11e9-ab14-d663bd873d93"))
        {
            write = Some(characteristic);
        }
    }
    Some((notify?, write?))
}
