//! Transports and the request/response session for Edifier headsets.
//!
//! The protocol, framing and command table live in [`edifier_proto`]; this crate owns the BLE
//! GATT link (`ble`) and the shared [`Session`] that correlates requests with device
//! notifications.
//!
//! ```no_run
//! use edifier_link::ble::{self, Target};
//! # async fn example() -> edifier_link::Result<()> {
//! if let Some(found) = ble::scan(std::time::Duration::from_secs(3), 1).await?.into_iter().next() {
//!     let session = edifier_link::Session::connect_ble(Target::Identifier(found.identifier)).await?;
//!     println!("{}", session.status().await.summary());
//!     session.close().await;
//! }
//! # Ok(())
//! # }
//! ```

pub mod ble;
pub mod session;
#[cfg(target_os = "macos")]
pub mod system_ble;
pub mod transport;

pub use session::{DEFAULT_TIMEOUT, Session};
pub use transport::{LinkError, Result, Transport, TransportReader, TransportWriter};
