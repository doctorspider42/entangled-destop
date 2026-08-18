//! The transport-agnostic device contract.

use std::sync::Arc;

use thiserror::Error;
use virtio_queue::Queue;

use crate::chain::ChainError;
use crate::interrupt::{Interrupt, InterruptError};
use crate::GuestMem;

/// VirtIO device ids used by the MVP (VirtIO spec 1.2, section 5).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum DeviceType {
    Net = 1,
    Block = 2,
    Gpu = 16,
    Input = 18,
}

impl DeviceType {
    /// The value the transport reports in its `DEVICE_ID` register.
    pub const fn id(self) -> u32 {
        self as u32
    }
}

/// Everything a device needs to start serving its queues, handed over by the
/// transport when the driver sets `DRIVER_OK`.
///
/// This is the crux of the trait redesign for EPIC 3: before the transport
/// existed, `activate()` took no arguments and a device had no way to reach a
/// queue, guest memory or the interrupt line. Now the transport — the only
/// component that knows the register layout — validates the guest-programmed
/// geometry, builds real virtqueues and hands them over together with the
/// guest memory handle and an [`Interrupt`]. Nothing mmio-specific crosses the
/// boundary, so virtio-pci can construct the same struct post-MVP.
pub struct DeviceResources {
    /// Guest RAM, shared with the VM. Devices must only access it through the
    /// checked `vm-memory` APIs.
    pub mem: Arc<GuestMem>,

    /// One configured and validated virtqueue per entry of
    /// [`VirtioDevice::queue_max_sizes`], in the same order.
    pub queues: Vec<Queue>,

    /// Used-buffer and config-change notifications.
    pub interrupt: Arc<dyn Interrupt>,
}

impl std::fmt::Debug for DeviceResources {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DeviceResources")
            .field("queues", &self.queues.len())
            .finish_non_exhaustive()
    }
}

/// Errors a device reports to the transport.
///
/// Everything here is recoverable from the host's point of view: the transport
/// logs the error and sets `DEVICE_NEEDS_RESET`. Guest-caused failures of a
/// *single request* never reach this type — devices answer those in-band (a
/// virtio-blk error status byte, for instance), because failing one request
/// must not take the whole device down.
#[derive(Debug, Error)]
pub enum DeviceError {
    #[error("device has no queue with index {0}")]
    UnknownQueue(u16),

    #[error("device is not activated")]
    NotActivated,

    #[error("device expects {expected} queues, transport offered {actual}")]
    QueueCount { expected: usize, actual: usize },

    #[error("descriptor chain rejected: {0}")]
    Chain(#[from] ChainError),

    #[error("guest memory access failed: {0}")]
    Memory(String),

    #[error("virtqueue error: {0}")]
    Queue(String),

    #[error("interrupt delivery failed: {0}")]
    Interrupt(#[from] InterruptError),

    #[error("device backend failed: {0}")]
    Backend(String),
}

/// A queue the driver programmed badly is a device-level failure. Available so
/// devices that build queues themselves can propagate it.
impl From<crate::queue::QueueError> for DeviceError {
    fn from(value: crate::queue::QueueError) -> Self {
        DeviceError::Queue(value.to_string())
    }
}

/// Contract between the transport (`virtio-mmio` today, virtio-pci later)
/// and a device implementation.
///
/// The transport owns register handling, feature/status negotiation state and
/// queue plumbing; the device sees negotiated features, its config space and
/// activation with ready queues. Implementations must treat all queue content
/// as untrusted (see crate docs and [`crate::chain`]).
pub trait VirtioDevice: Send {
    /// Device id advertised to the guest.
    fn device_type(&self) -> DeviceType;

    /// Maximum size of each virtqueue this device exposes, in queue order.
    /// The length of the slice is the number of queues.
    fn queue_max_sizes(&self) -> &[u16];

    /// Number of virtqueues this device exposes.
    fn num_queues(&self) -> u16 {
        u16::try_from(self.queue_max_sizes().len()).unwrap_or(u16::MAX)
    }

    /// Feature bits the device offers (must include
    /// [`crate::VIRTIO_F_VERSION_1`]).
    fn device_features(&self) -> u64;

    /// Called when the driver has written FEATURES_OK; the device records
    /// the negotiated subset. Returns false to veto the negotiation (the
    /// transport then refuses FEATURES_OK per spec).
    fn ack_features(&mut self, negotiated: u64) -> bool;

    /// Reads from the device-specific config space at `offset`. Reads past the
    /// end of the config space must produce zeroes, never a panic.
    fn read_config(&self, offset: u64, data: &mut [u8]);

    /// Writes to the device-specific config space at `offset`. Devices with a
    /// read-only config space ignore this.
    fn write_config(&mut self, offset: u64, data: &[u8]);

    /// Driver wrote DRIVER_OK: take the validated queues, guest memory handle
    /// and interrupt, and get ready to process requests.
    fn activate(&mut self, resources: DeviceResources) -> Result<(), DeviceError>;

    /// The driver kicked `queue_index`. Process everything available on it and
    /// signal the interrupt if used buffers were added.
    ///
    /// Malformed guest requests must be failed in-band (status byte / dropped
    /// chain) and reported as `Ok`; only host-level trouble returns `Err`,
    /// which makes the transport set `DEVICE_NEEDS_RESET`.
    fn notify(&mut self, queue_index: u16) -> Result<(), DeviceError>;

    /// Device reset: drop in-flight work, release queue state, return to the
    /// pre-ACKNOWLEDGE state. Must be infallible.
    fn reset(&mut self);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn device_ids_match_the_spec() {
        assert_eq!(DeviceType::Net.id(), 1);
        assert_eq!(DeviceType::Block.id(), 2);
        assert_eq!(DeviceType::Gpu.id(), 16);
        assert_eq!(DeviceType::Input.id(), 18);
    }
}
