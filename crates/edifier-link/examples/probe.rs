//! Hardware smoke test: scan, connect, read the full status, exercise the ANC switch and
//! restore the original mode.
//!
//! Run with the headset powered on and in range:
//!
//! ```text
//! cargo run -p edifier-link --example probe
//! ```

use std::time::Duration;

use edifier_link::Session;
use edifier_link::ble::{self, Target};
use edifier_proto::AncMode;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            std::env::var("RUST_LOG").unwrap_or_else(|_| "edifier_link=debug,probe=info".into()),
        )
        .init();

    println!("== scanning for Edifier endpoints ==");
    let found = ble::scan(Duration::from_secs(8), 1).await?;
    if found.is_empty() {
        println!("no Edifier endpoint found (power the headset on and keep it in range)");
        return Ok(());
    }
    for device in &found {
        println!(
            "  {:<38} {:<20} rssi={:?} model={:?} service={:?}",
            device.identifier,
            device.name.as_deref().unwrap_or("?"),
            device.rssi,
            device.model,
            device.service
        );
    }

    let target = Target::Identifier(found[0].identifier.clone());
    println!("== connecting ==");
    let session = Session::connect_ble(target).await?;
    println!("connected: {}", session.describe());

    let mut events = session.subscribe();
    tokio::spawn(async move {
        while let Ok(event) = events.recv().await {
            println!("  event: {event}");
        }
    });

    println!("== refreshing status ==");
    let before = session.refresh().await;
    println!("{}", serde_json::to_string_pretty(&before)?);
    println!("summary: {}", before.summary());

    let original = before.anc.unwrap_or(AncMode::Normal);
    let probe = if original == AncMode::NoiseCancelling {
        AncMode::Ambient
    } else {
        AncMode::NoiseCancelling
    };

    println!("== switching ANC to {:?} ==", probe);
    session.set_anc(probe).await?;
    tokio::time::sleep(Duration::from_millis(400)).await;
    let after = session.status().await;
    println!("anc now: {:?}", after.anc);

    println!("== restoring ANC to {:?} ==", original);
    session.set_anc(original).await?;
    tokio::time::sleep(Duration::from_millis(400)).await;
    println!("anc restored: {:?}", session.status().await.anc);

    println!("== final status ==");
    println!(
        "{}",
        serde_json::to_string_pretty(&session.refresh().await)?
    );

    session.close().await;
    println!("done");
    Ok(())
}
