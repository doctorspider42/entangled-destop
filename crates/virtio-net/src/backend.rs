//! The host-side network backend contract (backlog MVP-503/505).
//!
//! [`NetDevice`](crate::NetDevice) never talks to a file descriptor directly;
//! it drives a [`NetBackend`]. The MVP backend is [`TapBackend`](crate::tap)
//! (Linux only), and tests supply in-process fakes, which is what makes the
//! whole device — including the RX worker thread and its shutdown path —
//! testable on any host OS.
//!
//! The trait is deliberately blocking-with-timeout rather than async: one
//! dedicated RX thread per device is enough for a 1 Gbit/s-class MVP, and it
//! keeps the shutdown story simple (see [`NetBackend::wake`]).

use std::time::Duration;

use thiserror::Error;

/// Errors a backend reports. All of them are host-side: guest mistakes never
/// reach this type, they are dropped frames.
#[derive(Debug, Error)]
pub enum NetError {
    #[error("cannot open {path}: {source}")]
    Open {
        path: String,
        #[source]
        source: std::io::Error,
    },

    #[error(
        "interface name {name:?} is invalid: must be 1..{max} bytes, no NUL, no '/' \
         and no whitespace"
    )]
    InterfaceName { name: String, max: usize },

    #[error("cannot attach to TAP interface {ifname}: {source}")]
    Attach {
        ifname: String,
        #[source]
        source: std::io::Error,
    },

    #[error("cannot create the RX wakeup eventfd: {source}")]
    Wakeup {
        #[source]
        source: std::io::Error,
    },

    #[error("reading a frame from {backend} failed: {source}")]
    Read {
        backend: String,
        #[source]
        source: std::io::Error,
    },

    #[error("writing a frame to {backend} failed: {source}")]
    Write {
        backend: String,
        #[source]
        source: std::io::Error,
    },

    #[error("waiting for {backend} to become readable failed: {source}")]
    Poll {
        backend: String,
        #[source]
        source: std::io::Error,
    },

    #[error("this platform has no TAP support")]
    Unsupported,
}

/// Outcome of [`NetBackend::wait_readable`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Readiness {
    /// At least one frame can be read without blocking.
    Readable,
    /// The timeout expired with nothing to read.
    TimedOut,
    /// [`NetBackend::wake`] was called — the RX worker uses this to notice a
    /// device reset immediately instead of waiting out the poll timeout.
    WokenUp,
}

/// A host network endpoint that carries whole Ethernet frames.
///
/// All methods take `&self` because the device's TX path (on the vCPU thread
/// that took the queue-notify exit) and its RX worker thread use the same
/// backend concurrently. Implementations must therefore be internally
/// synchronised or, like a TAP file descriptor, safe to read and write from two
/// threads at once.
pub trait NetBackend: Send + Sync {
    /// Short label for log records, e.g. `tap:vmhost0`.
    fn name(&self) -> &str;

    /// Sends one frame (no virtio-net header) to the host.
    ///
    /// `Ok(0)` means the backend could not accept the frame right now — a full
    /// interface queue, say. That is congestion, not a failure: the caller
    /// drops the frame and counts it, exactly as a real NIC would. `Err` is
    /// reserved for a broken backend.
    fn write_frame(&self, frame: &[u8]) -> Result<usize, NetError>;

    /// Reads one frame into `buf`. `Ok(None)` means "nothing pending right
    /// now"; frames longer than `buf` are truncated to its length, which the
    /// caller detects by sizing `buf` one byte above the frame cap.
    fn read_frame(&self, buf: &mut [u8]) -> Result<Option<usize>, NetError>;

    /// Blocks until a frame is readable, `timeout` expires or [`Self::wake`]
    /// is called.
    fn wait_readable(&self, timeout: Duration) -> Result<Readiness, NetError>;

    /// Makes a concurrent [`Self::wait_readable`] return
    /// [`Readiness::WokenUp`] promptly. Called on device reset and on drop, so
    /// the RX worker can be joined without waiting for a poll timeout.
    ///
    /// The default implementation does nothing, which is correct but leaves
    /// shutdown latency at one poll tick.
    fn wake(&self) -> Result<(), NetError> {
        Ok(())
    }
}
