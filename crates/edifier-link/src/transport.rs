//! Transport traits.
//!
//! A transport splits into a reader and a writer so each half can live in its own task: the
//! reader never blocks writes, and `tokio::select!` never has to poll a partially consumed
//! transport read (which would not be cancellation safe).
//!
//! Traits use return-position `impl Future` rather than `#[async_trait]`, so impls are
//! statically dispatched with no per-call boxing, and the `+ Send` bound lets the halves run on
//! the multi-threaded Tokio runtime.

use thiserror::Error;

/// Alias that keeps the error type overridable while defaulting to [`LinkError`].
pub type Result<T, E = LinkError> = std::result::Result<T, E>;

#[derive(Debug, Error)]
pub enum LinkError {
    /// No Bluetooth adapter is usable (hardware off, or the process was denied permission).
    #[error("bluetooth adapter unavailable: {0}")]
    Adapter(String),
    /// The requested device did not show up during discovery.
    #[error("device not found: {0}")]
    NotFound(String),
    /// The link could not be established.
    #[error("connection failed: {0}")]
    Connect(String),
    /// The peripheral answered, but exposes no Edifier control service.
    #[error("the device does not expose the Edifier control service")]
    NoControlService,
    /// The link was closed by the peer or by us.
    #[error("link closed")]
    Closed,
    /// The device did not answer in time.
    #[error("timed out after {0:?}")]
    Timeout(std::time::Duration),
    /// Transport level failure (IO, CoreBluetooth, RFCOMM channel).
    #[error("transport error: {0}")]
    Transport(String),
    /// Invalid protocol data.
    #[error("protocol error: {0}")]
    Protocol(#[from] edifier_proto::FrameError),
    /// Anything else.
    #[error("{0}")]
    Other(String),
}

/// Reads device frames from a link.
pub trait TransportReader: Send + 'static {
    /// Next chunk of bytes, or `None` once the peer closed the link.
    fn next_chunk(&mut self) -> impl Future<Output = Result<Option<Vec<u8>>>> + Send;
}

/// Writes host frames to a link.
pub trait TransportWriter: Send + 'static {
    /// Send one already-framed packet.
    fn write_frame(&mut self, bytes: &[u8]) -> impl Future<Output = Result<()>> + Send;

    /// Tear the link down. Must be idempotent.
    fn close(&mut self) -> impl Future<Output = ()> + Send;
}

/// A link that can be split into a reader and a writer.
pub trait Transport: Send + 'static {
    /// Read half type.
    type Reader: TransportReader;
    /// Write half type.
    type Writer: TransportWriter;

    /// Consume the transport and hand out both halves.
    fn split(self) -> (Self::Reader, Self::Writer);

    /// Human readable description for logs and CLI output.
    fn describe(&self) -> String;
}
