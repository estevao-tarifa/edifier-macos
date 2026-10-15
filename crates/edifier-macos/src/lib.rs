//! macOS audio-link helpers.
//!
//! The BLE control link is independent of the Bluetooth *audio* link: the headset can be playing
//! from a phone while the Mac still controls it over BLE (that is also how mEDIFIER's Android-only
//! "Connect Audio" button works). This module asks macOS to bring the audio link back to this Mac
//! by opening the paired device's classic connection, which is what `blueutil` does.

use std::time::{Duration, Instant};

use anyhow::{Result, anyhow, bail};
use objc2_foundation::NSString;
use objc2_io_bluetooth::IOBluetoothDevice;

/// How long to wait for the classic connection to come up after `openConnection`.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// Poll interval while waiting for the connection.
const POLL_INTERVAL: Duration = Duration::from_millis(250);

/// Paired classic Bluetooth device as seen by IOBluetooth.
#[derive(Debug, Clone)]
pub struct PairedDevice {
    /// Device name, e.g. `EDIFIER W820NB`.
    pub name: String,
    /// Colon separated address, e.g. `0C:AE:BD:BC:8F:A2`.
    pub address: String,
    /// Whether the ACL link is up right now.
    pub connected: bool,
}

/// Explain the most common `IOReturn` values IOBluetooth reports, so the user gets an actionable
/// message instead of a raw hex code.
#[must_use]
pub fn describe_ioreturn(code: i32) -> &'static str {
    match code as u32 {
        0xE00002B6 => "the Bluetooth controller is switched off",
        0xE00002C0 => "no such device — pair the headset with this Mac first",
        0xE00002C1 => {
            "not permitted — allow Bluetooth access in System Settings → Privacy & Security"
        }
        0xE00002C7 => "the request is not supported for this device",
        0xE00002D5 => "the device is busy — another host is probably connected",
        0xE00002D6 => "the device did not answer — it is probably switched off or out of range",
        0xE00002D8 => "the device is not ready yet — try again in a moment",
        _ => "unrecognised IOBluetooth error",
    }
}

/// Names that identify the headset when no explicit selector was supplied.
const EDIFIER_NAME_HINTS: [&str; 3] = ["EDIFIER", "W820NB", "W200BT"];

/// Every device macOS has paired over classic Bluetooth.
///
/// # Errors
///
/// Fails when the IOBluetooth framework returns no device list at all (for example when Bluetooth
/// is switched off).
pub fn paired_devices() -> Result<Vec<PairedDevice>> {
    // SAFETY: `pairedDevices` is a class method of IOBluetoothDevice that returns either nil or an
    // autoreleased NSArray of IOBluetoothDevice. We only call it, never take ownership, and only
    // read immutable properties of the returned objects. The process must simply have the
    // IOBluetooth framework loaded, which the linked crate guarantees.
    let devices = unsafe { IOBluetoothDevice::pairedDevices() }
        .ok_or_else(|| anyhow!("Bluetooth is off or no device has been paired"))?;

    let mut out = Vec::with_capacity(devices.count());
    for index in 0..devices.count() {
        let object = devices.objectAtIndex(index);
        let Some(device) = object.downcast_ref::<IOBluetoothDevice>() else {
            continue;
        };
        // SAFETY: `device` was checked to be an IOBluetoothDevice; `name`, `addressString` and
        // `isConnected` are immutable getters with no side effects.
        let (name, address, connected) = unsafe {
            (
                device.name().to_string(),
                device
                    .addressString()
                    .map(|value| value.to_string())
                    .unwrap_or_default(),
                device.isConnected(),
            )
        };
        out.push(PairedDevice {
            name,
            address,
            connected,
        });
    }
    Ok(out)
}

/// Open the classic connection to a paired device, bringing audio back to this Mac.
///
/// `selector` is matched case-insensitively against both the name and the address; when omitted,
/// the first paired Edifier device is used.
///
/// # Errors
///
/// Fails when no paired device matches, when IOBluetooth rejects the request, or when the link does
/// not come up within [`CONNECT_TIMEOUT`].
pub fn connect_paired_audio(selector: Option<&str>) -> Result<String> {
    let devices = paired_devices()?;
    let wanted = selector.map(str::to_lowercase);
    let device = devices
        .iter()
        .find(|device| {
            wanted.as_ref().is_some_and(|wanted| {
                device.name.to_lowercase().contains(wanted)
                    || device.address.to_lowercase().contains(wanted)
            })
        })
        .or_else(|| {
            devices.iter().find(|device| {
                let upper = device.name.to_uppercase();
                EDIFIER_NAME_HINTS.iter().any(|hint| upper.contains(hint))
            })
        })
        .ok_or_else(|| {
            anyhow!(
                "no matching paired device (paired: {})",
                devices
                    .iter()
                    .map(|device| device.name.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        })?;

    if device.connected {
        return Ok(format!(
            "{} ({}) is already connected",
            device.name, device.address
        ));
    }

    let address = NSString::from_str(&device.address);
    // SAFETY: `deviceWithAddressString` is a class method that either returns nil or an
    // autoreleased IOBluetoothDevice on which only immutable getters and the connection calls are
    // used. The NSString is alive for the duration of the call.
    let handle = unsafe { IOBluetoothDevice::deviceWithAddressString(Some(&address)) }
        .ok_or_else(|| anyhow!("IOBluetooth does not know {}", device.address))?;

    // SAFETY: `openConnection` starts an asynchronous HCI connection attempt on a live device
    // object and returns an IOReturn code; no Rust references are involved.
    let result = unsafe { handle.openConnection() };
    if result != 0 {
        bail!(
            "IOBluetooth refused to connect {}: {} (IOReturn 0x{result:08X})",
            device.address,
            describe_ioreturn(result)
        );
    }

    let deadline = Instant::now() + CONNECT_TIMEOUT;
    while Instant::now() < deadline {
        // SAFETY: `isConnected` is a side-effect-free getter on a live device object.
        if unsafe { handle.isConnected() } {
            return Ok(format!(
                "audio link connected to {} ({})",
                device.name, device.address
            ));
        }
        std::thread::sleep(POLL_INTERVAL);
    }
    bail!(
        "{} did not connect within {}s — power the headset on, or pair it again",
        device.name,
        CONNECT_TIMEOUT.as_secs()
    )
}

/// Close the classic connection, releasing the audio link back to the other host.
///
/// # Errors
///
/// Fails when IOBluetooth does not know the device.
pub fn disconnect_paired_audio(selector: &str) -> Result<String> {
    let address = NSString::from_str(selector);
    // SAFETY: see `connect_paired_audio`.
    let handle = unsafe { IOBluetoothDevice::deviceWithAddressString(Some(&address)) }
        .ok_or_else(|| anyhow!("IOBluetooth does not know {selector}"))?;
    // SAFETY: `closeConnection` tears down the ACL link and returns an IOReturn code.
    let result = unsafe { handle.closeConnection() };
    if result != 0 {
        bail!(
            "IOBluetooth refused to disconnect {selector}: {} (IOReturn 0x{result:08X})",
            describe_ioreturn(result)
        );
    }
    Ok(format!("disconnected {selector}"))
}

#[cfg(test)]
mod tests {
    use super::describe_ioreturn;

    #[test]
    fn known_returns_explain_themselves() {
        // kIOReturnTimeout: the headset is off or out of range.
        assert!(describe_ioreturn(0xE00002D6u32 as i32).contains("did not answer"));
        // kIOReturnNotReady.
        assert!(describe_ioreturn(0xE00002D8u32 as i32).contains("not ready"));
        // kIOReturnBusy.
        assert!(describe_ioreturn(0xE00002D5u32 as i32).contains("busy"));
        // Anything unknown must still produce a message rather than an empty string.
        assert!(!describe_ioreturn(0x1234_5678).is_empty());
    }
}
