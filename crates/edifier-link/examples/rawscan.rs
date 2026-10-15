//! Dump every BLE peripheral this Mac can see, with names, services and manufacturer data.
//!
//! Useful when the headset is not being found: it shows whether it is advertising at all, and
//! under which name.
//!
//! ```text
//! cargo run -p edifier-link --example rawscan -- 10
//! ```

use std::time::Duration;

use btleplug::api::{Central, Manager as _, Peripheral as _, ScanFilter};
use btleplug::platform::Manager;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let seconds: u64 = std::env::args()
        .nth(1)
        .and_then(|value| value.parse().ok())
        .unwrap_or(10);

    let manager = Manager::new().await?;
    let adapter = manager
        .adapters()
        .await?
        .into_iter()
        .next()
        .ok_or_else(|| anyhow::anyhow!("no Bluetooth adapter"))?;
    adapter.start_scan(ScanFilter::default()).await?;
    println!("scanning for {seconds}s …");

    let mut seen: Vec<String> = Vec::new();
    for _ in 0..seconds {
        tokio::time::sleep(Duration::from_secs(1)).await;
        for peripheral in adapter.peripherals().await? {
            let id = peripheral.id().to_string();
            if seen.contains(&id) {
                continue;
            }
            let properties = peripheral.properties().await?.unwrap_or_default();
            let services: Vec<String> = properties
                .services
                .iter()
                .map(|uuid| uuid.to_string())
                .collect();
            println!(
                "{:<38} {:<28} rssi={:<7} services=[{}] manufacturer={:?}",
                id,
                properties.local_name.as_deref().unwrap_or("<no name>"),
                properties
                    .rssi
                    .map_or_else(|| "-".to_string(), |rssi| rssi.to_string()),
                services.join(", "),
                properties.manufacturer_data.keys().collect::<Vec<_>>()
            );
            seen.push(id);
        }
    }

    let _ = adapter.stop_scan().await;
    println!("{} peripherals seen", seen.len());
    Ok(())
}
