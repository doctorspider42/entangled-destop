//! VirtIO foundation (backlog EPIC 3): the `virtio-mmio` transport, device
//! status handling, virtqueue safety limits and the transport-agnostic
//! [`VirtioDevice`] trait every device crate implements.
//!
//! Design rule: device crates (`virtio-block`, `virtio-net`, …) never touch
//! transport registers — they see queues, feature bits and config space only.
//! That keeps the post-MVP virtio-pci migration confined to this crate.

pub mod chain;
pub mod device;
pub mod mmio;
pub mod status;

pub use chain::{ChainError, ChainWalkGuard, MAX_DESC_CHAIN_LEN};
pub use device::{DeviceType, VirtioDevice};

/// VirtIO feature bit: the device conforms to the modern (v1.0+) spec.
/// Mandatory for every VMHost device — we do not implement legacy mode.
pub const VIRTIO_F_VERSION_1: u64 = 1 << 32;

/// Maximum queue size any VMHost device advertises. Bounded so a guest
/// cannot make the host allocate unbounded ring bookkeeping.
pub const MAX_QUEUE_SIZE: u16 = 256;
