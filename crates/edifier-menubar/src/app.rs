//! Process entry point: AppKit on the main thread, BLE on a background runtime.

use anyhow::{Context, Result};
use objc2::MainThreadMarker;
use tokio::sync::mpsc;
use tracing::{error, info};

use crate::config::Config;
use crate::link::{self, Ctx};
use crate::menu::{Action, SharedState};
use crate::ui;

/// Depth of the queue that carries menu clicks to the BLE runtime.
const REQUEST_QUEUE: usize = 16;
/// Worker threads of the BLE runtime: one to drive the session, one for blocking helpers.
const RUNTIME_WORKERS: usize = 2;

/// Run the menu bar app until the user quits it.
///
/// `device` overrides the remembered headset for this run: a CoreBluetooth identifier or a name
/// fragment.
///
/// # Errors
///
/// Fails when the app is not started from the main thread, when the settings cannot be read, when
/// the BLE thread cannot be spawned, or when the Tokio runtime cannot be built.
pub fn run(device: Option<&str>) -> Result<()> {
    let mtm = MainThreadMarker::new().context("the menu bar app must start on the main thread")?;

    let mut settings = Config::load()?;
    if let Some(selector) = device {
        link::apply_selector(&mut settings, selector);
    }
    let settings_path = Config::path()?;

    let state = SharedState::new();
    let (requests, queue) = mpsc::channel::<Action>(REQUEST_QUEUE);
    let ctx = Ctx::new(state.clone(), ui::waker(), settings_path);

    ui::install(mtm, state, requests, settings.clone());

    let ble = std::thread::Builder::new()
        .name("edifier-ble".to_string())
        .spawn(move || match runtime() {
            Ok(runtime) => runtime.block_on(link::run(ctx, settings, queue)),
            Err(err) => error!(error = %format!("{err:#}"), "cannot start the bluetooth runtime"),
        })
        .context("cannot spawn the bluetooth thread")?;

    info!("edifier-menubar is running");
    ui::run(mtm);

    // `run` returns once the app terminates, and the process takes the runtime thread with it.
    drop(ble);
    Ok(())
}

/// The runtime the BLE work runs on.
fn runtime() -> Result<tokio::runtime::Runtime> {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(RUNTIME_WORKERS)
        .enable_all()
        .build()
        .context("cannot build the tokio runtime")
}
