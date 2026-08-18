//! The transport-agnostic device contract.

/// VirtIO device ids used by the MVP (VirtIO spec 1.2, section 5).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum DeviceType {
    Net = 1,
    Block = 2,
    Gpu = 16,
    Input = 18,
}

/// Contract between the transport (`virtio-mmio` today, virtio-pci later)
/// and a device implementation.
///
/// The transport owns register handling, feature/status negotiation state and
/// queue plumbing; the device sees negotiated features, its config space and
/// activation with ready queues. Implementations must treat all queue content
/// as untrusted (see crate docs and `chain`).
pub trait VirtioDevice: Send {
    /// Device id advertised to the guest.
    fn device_type(&self) -> DeviceType;

    /// Number of virtqueues this device exposes.
    fn num_queues(&self) -> u16;

    /// Feature bits the device offers (must include
    /// [`crate::VIRTIO_F_VERSION_1`]).
    fn device_features(&self) -> u64;

    /// Called when the driver has written FEATURES_OK; the device records
    /// the negotiated subset. Returns false to veto the negotiation (the
    /// transport then refuses FEATURES_OK per spec).
    fn ack_features(&mut self, negotiated: u64) -> bool;

    /// Reads from the device-specific config space at `offset`.
    fn read_config(&self, offset: u64, data: &mut [u8]);

    /// Writes to the device-specific config space at `offset`.
    fn write_config(&mut self, offset: u64, data: &[u8]);

    /// Driver wrote DRIVER_OK: queues are configured and the device may start
    /// processing. Queue handles and interrupt plumbing arrive here once the
    /// mmio transport lands (kept minimal until then).
    fn activate(&mut self);

    /// Device reset: drop in-flight work, release queue state, return to the
    /// pre-ACKNOWLEDGE state. Must be infallible.
    fn reset(&mut self);
}
