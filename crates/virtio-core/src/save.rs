//! Device state beside every `reset()`
//! ([ADR-0006](../../../docs/adr/0006-suspend-restore.md)).
//!
//! ADR-0005 made every device own a `reset()`. Suspend needs the other half:
//! for each of those pieces of state, a way to write it down and put it back.
//! The shapes here are **plain data** — no I/O, no encoding, no hypervisor —
//! so they build and are tested on both hosts, and the crate that writes them
//! to a file (`vm-snapshot`) is the only one that knows what the bytes look
//! like.
//!
//! # What has to be in a virtqueue's saved state
//!
//! The geometry (`size`, `ready`, the three ring addresses) is the *driver's*
//! programming and comes back from the registers. What does not is the pair of
//! positions the **device** keeps: `next_avail`, how far it has read into the
//! available ring, and `next_used`, how far it has written into the used one.
//!
//! Those two could *almost* be recomputed from guest memory — at a quiesced
//! device, `next_used` is the used ring's index, and a device that has drained
//! its ring has `next_avail == avail.idx`. "Almost" is the problem: a device
//! holding a descriptor chain across a host fence (virtio-gpu, ADR-0004
//! phase 2) has advanced `next_avail` past what it has completed, and a
//! recomputed position would hand the restored guest's buffers out twice.
//! Saving the number the device actually holds costs four bytes and has no
//! such caveat.

/// Where a device has got to in one virtqueue.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct QueuePosition {
    /// The next entry of the available ring the device will read.
    pub next_avail: u16,
    /// The next slot of the used ring the device will write.
    pub next_used: u16,
}

/// One virtqueue: what the driver programmed, and where the device is.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct QueueState {
    pub size: u16,
    pub ready: bool,
    pub desc_table: u64,
    pub driver_area: u64,
    pub device_area: u64,
    pub position: QueuePosition,
}

/// One MSI-X table entry, as the guest programmed it.
///
/// A copy of [`crate::msix::MsixEntry`]'s fields rather than the type itself,
/// so this module stays free of the interrupt machinery and a snapshot cannot
/// be broken by a change to how the table is *served*.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct MsixEntryState {
    pub address_lo: u32,
    pub address_hi: u32,
    pub data: u32,
    pub vector_control: u32,
}

/// The MSI-X half of a virtio-pci function's interrupt state.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MsixState {
    /// The capability's first dword, as the configuration space mirrors it
    /// (enable and function-mask live in its upper half).
    pub control: u32,
    /// `config_msix_vector` from the common configuration structure.
    pub config_vector: u16,
    /// `queue_msix_vector`, one per virtqueue.
    pub queue_vectors: Vec<u16>,
    pub entries: Vec<MsixEntryState>,
    /// The pending-bit array, as 64-bit words.
    pub pending: Vec<u64>,
}

/// A transport's interrupt state: the pending word every transport has, the
/// config generation, and the MSI-X table where there is one.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct InterruptState {
    /// `INTERRUPT_STATUS` (virtio-mmio) / the ISR byte (virtio-pci).
    pub isr: u32,
    /// `config_generation`.
    ///
    /// Restored rather than reset: a driver that read the counter, read the
    /// config and was suspended before reading the counter again must see the
    /// same value, or it retries a read that never needed retrying.
    pub generation: u32,
    pub msix: Option<MsixState>,
}

/// Everything one virtio slot is.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TransportSaveState {
    /// `DeviceType::id()`, checked on load: a snapshot restored onto a
    /// different device is a guest whose driver is talking to a stranger.
    pub device_type: u32,
    /// The features the *device* offers, checked on load for the same reason:
    /// a rebuilt host that offers a different set would let the guest go on
    /// using one it no longer has.
    pub device_features: u64,
    pub device_features_sel: u32,
    pub driver_features: u64,
    pub driver_features_sel: u32,
    pub queue_sel: u32,
    pub status: u32,
    pub activated: bool,
    pub queues: Vec<QueueState>,
    pub interrupt: InterruptState,
    /// Whatever the device itself owns beyond its queues, in its own encoding
    /// (`VirtioDevice::save_device`).
    pub device: Vec<u8>,
}

/// Why a saved state could not be put back.
///
/// Separate from `TransportError` because these are not guest-caused: they are
/// a snapshot and a machine that do not describe the same VM, and the honest
/// answer to every one of them is to refuse the restore.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum StateError {
    #[error("snapshot slot holds a device of type {snapshot}, this machine has type {current}")]
    DeviceType { snapshot: u32, current: u32 },

    #[error(
        "snapshot slot offers features {snapshot:#x}, this build offers {current:#x}; the guest \
         negotiated against the first set"
    )]
    DeviceFeatures { snapshot: u64, current: u64 },

    #[error("snapshot slot has {snapshot} queues, this device has {current}")]
    QueueCount { snapshot: usize, current: usize },

    #[error("snapshot slot has {snapshot} MSI-X vectors, this function has {current}")]
    MsixTableSize { snapshot: usize, current: usize },

    #[error("the device refused its saved state: {0}")]
    Device(String),

    #[error("the driver had activated this device, but its saved queues cannot be rebuilt: {0}")]
    Queue(String),

    #[error("the device refused to activate from its saved state: {0}")]
    Activate(String),
}
