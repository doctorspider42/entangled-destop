//! VirtIO foundation (backlog EPIC 3): the `virtio-mmio` and `virtio-pci`
//! transports, device status handling, virtqueue safety limits and the
//! transport-agnostic [`VirtioDevice`] trait every device crate implements.
//!
//! Design rule: device crates (`virtio-block`, `virtio-net`, …) never touch
//! transport registers — they see queues, feature bits and config space only.
//! That rule is what made adding the second transport a change confined to this
//! crate plus the machine's bus wiring: **not one device crate was touched**.
//!
//! # Layering
//!
//! ```text
//!  guest MMIO exit  ->  MachineBus  ->  MmioTransport ─┐
//!                                       (mmio offsets) │
//!  guest MMIO/PIO   ->  PciBus      ->  PciTransport  ─┤
//!                                       (BAR offsets)  │
//!                                                      v
//!                                       TransportState  ->  VirtioDevice
//!                                (features/status/queues)   (queues, config)
//!                                            |                   |
//!                                    TransportInterrupt <---------+
//!                                     /            \      signal_used_queue
//!                          LineInterrupt         MsixInterrupt
//!                                |                /        \
//!                     IrqLine (irqfd)      IrqLine      MsiSink
//!                                          (INTx fallback)  (KVM_SIGNAL_MSI)
//! ```
//!
//! [`TransportState`] is the single copy of the parts both transports share:
//! feature negotiation, the device-status state machine, per-queue
//! [`QueueConfig`], activation and reset. A transport module is then only an
//! address decoder.
//!
//! `vm-memory` and `virtio-queue` are portable, so everything here builds and
//! tests on any development OS; only the `IrqLine` implementation (eventfd +
//! KVM irqfd, in `machine-x86`) is Linux-specific.

pub mod chain;
pub mod device;
pub mod interrupt;
pub mod mmio;
pub mod msix;
pub mod pci;
pub mod queue;
pub mod state;
pub mod status;
pub mod transport;

#[cfg(any(test, feature = "test-utils"))]
pub mod testing;

pub use chain::{ChainError, ChainWalkGuard, Segment, MAX_DESC_CHAIN_LEN};
pub use device::{DeviceError, DeviceResources, DeviceType, VirtioDevice};
pub use interrupt::{
    Interrupt, InterruptError, IrqLine, LineInterrupt, MsiMessage, MsiSink, TransportInterrupt,
};
pub use msix::{MsixInterrupt, MAX_MSIX_VECTORS};
pub use pci::{PciTransport, VIRTIO_PCI_BAR_SIZE};
pub use queue::{QueueConfig, QueueError};
pub use state::TransportState;
pub use transport::{MmioTransport, TransportError};

/// VirtIO feature bit: the device conforms to the modern (v1.0+) spec.
/// Mandatory for every Entangled Desktop device — we do not implement legacy mode.
pub const VIRTIO_F_VERSION_1: u64 = 1 << 32;

/// Maximum queue size any Entangled Desktop device advertises. Bounded so a guest
/// cannot make the host allocate unbounded ring bookkeeping.
pub const MAX_QUEUE_SIZE: u16 = 256;

/// The concrete guest memory type devices and the transport work against.
///
/// Deliberately concrete rather than generic: `virtio-queue`'s APIs are
/// generic over `Deref<Target: GuestMemory>`, which cannot be used behind the
/// `dyn VirtioDevice` trait object the transport stores.
pub type GuestMem = vm_memory::GuestMemoryMmap;
