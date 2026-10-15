//! Device link lifetime management for the server.
//!
//! Tool calls arrive sequentially from the client, so the link is a plain `Option<Session>` that is
//! opened on demand and re-opened after a drop.

use anyhow::{Result, anyhow};
use edifier_link::Session;
use edifier_link::ble::{self, Discovered, Target};
use tracing::{info, warn};

/// Holds the BLE control session for the lifetime of the server process.
#[derive(Default)]
pub struct DeviceLink {
    selector: Option<String>,
    session: Option<Session>,
}

impl DeviceLink {
    /// Create a link that uses `selector` (a name fragment or CoreBluetooth identifier) when it
    /// has to connect.
    #[must_use]
    pub fn new(selector: Option<String>) -> Self {
        DeviceLink {
            selector,
            session: None,
        }
    }

    /// Currently configured selector.
    #[must_use]
    pub fn selector(&self) -> Option<&str> {
        self.selector.as_deref()
    }

    /// Whether a live session is open.
    #[must_use]
    pub fn is_open(&self) -> bool {
        self.session.as_ref().is_some_and(Session::is_alive)
    }

    /// Replace the selector, dropping any open session.
    pub async fn set_selector(&mut self, selector: Option<String>) {
        if self.selector != selector {
            self.selector = selector;
            self.close().await;
        }
    }

    /// The open session, connecting first when necessary.
    ///
    /// # Errors
    ///
    /// Fails when no Edifier endpoint can be found or the control service is missing.
    pub async fn session(&mut self) -> Result<&Session> {
        if !self.is_open() {
            self.reconnect().await?;
        }
        self.session
            .as_ref()
            .ok_or_else(|| anyhow!("control link unavailable"))
    }

    /// Drop a dead session and open a fresh one.
    async fn reconnect(&mut self) -> Result<()> {
        if self.session.is_some() {
            warn!("control link dropped, reconnecting");
        }
        if let Some(dead) = self.session.take() {
            dead.close().await;
        }
        let target = self
            .selector
            .as_deref()
            .map_or(Target::Auto, Target::from_selector);
        info!(?target, "opening control link");
        let session = Session::connect_ble(target).await?;
        info!(link = session.describe(), "control link open");
        self.session = Some(session);
        Ok(())
    }

    /// Close the control link, if open.
    pub async fn close(&mut self) {
        if let Some(session) = self.session.take() {
            session.close().await;
        }
    }

    /// Scan for reachable endpoints.
    ///
    /// # Errors
    ///
    /// Fails when the Bluetooth adapter is unavailable.
    pub async fn scan(seconds: u64) -> Result<Vec<Discovered>> {
        Ok(ble::scan(std::time::Duration::from_secs(seconds), usize::MAX).await?)
    }
}
