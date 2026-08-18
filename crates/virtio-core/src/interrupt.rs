//! Device interrupt plumbing (backlog MVP-306).
//!
//! Two layers, so devices stay transport-agnostic:
//!
//! * [`Interrupt`] is what a device sees — "tell the driver queue *n* has used
//!   buffers" / "tell the driver the config space changed". virtio-pci will
//!   provide an MSI-X backed implementation of the same trait.
//! * [`IrqLine`] is the host mechanism that actually raises the line. On Linux
//!   `machine-x86` implements it with an `EventFd` registered as a KVM irqfd,
//!   so signalling never round-trips through userspace. Tests use counters.
//!
//! [`MmioInterrupt`] glues the two together and owns the shared state the
//! `INTERRUPT_STATUS` / `INTERRUPT_ACK` / `CONFIG_GENERATION` registers expose.

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;

use thiserror::Error;

use crate::mmio::{INT_CONFIG, INT_VRING};

#[derive(Debug, Error)]
pub enum InterruptError {
    #[error("failed to raise the device interrupt line: {0}")]
    Signal(String),
}

/// The host side of one device interrupt line.
///
/// Implementations must be cheap and non-blocking: this is called from the
/// vCPU thread that took the queue-notify exit.
pub trait IrqLine: Send + Sync {
    fn trigger(&self) -> Result<(), InterruptError>;
}

/// What a device uses to notify its driver. Transport-agnostic on purpose.
pub trait Interrupt: Send + Sync {
    /// A used buffer was added to `queue_index`.
    fn signal_used_queue(&self, queue_index: u16) -> Result<(), InterruptError>;

    /// The device configuration space changed.
    fn signal_config_change(&self) -> Result<(), InterruptError>;
}

/// The virtio-mmio interrupt: the guest-visible `INTERRUPT_STATUS` word and
/// the config generation counter, plus the line that raises the IRQ.
///
/// Shared (`Arc`) between the transport — which serves register reads and the
/// `INTERRUPT_ACK` writes — and the device, which only ever signals. All state
/// is atomic because the two can sit on different vCPU threads.
pub struct MmioInterrupt {
    status: AtomicU32,
    generation: AtomicU32,
    line: Arc<dyn IrqLine>,
}

impl MmioInterrupt {
    pub fn new(line: Arc<dyn IrqLine>) -> Self {
        Self {
            status: AtomicU32::new(0),
            generation: AtomicU32::new(0),
            line,
        }
    }

    /// Value of the `INTERRUPT_STATUS` register.
    pub fn status(&self) -> u32 {
        self.status.load(Ordering::Acquire)
    }

    /// Value of the `CONFIG_GENERATION` register.
    pub fn generation(&self) -> u32 {
        self.generation.load(Ordering::Acquire)
    }

    /// Guest write to `INTERRUPT_ACK`: clears the acknowledged bits. Bits the
    /// guest has no business setting are ignored.
    pub fn ack(&self, bits: u32) {
        let known = bits & (INT_VRING | INT_CONFIG);
        self.status.fetch_and(!known, Ordering::AcqRel);
    }

    /// Device reset: no interrupt is pending any more.
    pub fn clear(&self) {
        self.status.store(0, Ordering::Release);
    }

    fn raise(&self, bit: u32) -> Result<(), InterruptError> {
        self.status.fetch_or(bit, Ordering::AcqRel);
        self.line.trigger()
    }
}

impl Interrupt for MmioInterrupt {
    fn signal_used_queue(&self, _queue_index: u16) -> Result<(), InterruptError> {
        // virtio-mmio has a single interrupt line shared by all queues; the
        // driver scans every queue after INT_VRING. (virtio-pci with MSI-X
        // will use the per-queue vector instead.)
        self.raise(INT_VRING)
    }

    fn signal_config_change(&self) -> Result<(), InterruptError> {
        self.generation.fetch_add(1, Ordering::AcqRel);
        self.raise(INT_CONFIG)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::TestIrqLine;

    #[test]
    fn used_buffer_sets_vring_bit_and_raises_the_line() {
        let line = Arc::new(TestIrqLine::default());
        let irq = MmioInterrupt::new(line.clone());

        assert_eq!(irq.status(), 0);
        assert!(irq.signal_used_queue(0).is_ok());
        assert_eq!(irq.status(), INT_VRING);
        assert_eq!(line.count(), 1);
    }

    #[test]
    fn config_change_bumps_the_generation() {
        let line = Arc::new(TestIrqLine::default());
        let irq = MmioInterrupt::new(line);

        assert_eq!(irq.generation(), 0);
        assert!(irq.signal_config_change().is_ok());
        assert_eq!(irq.status(), INT_CONFIG);
        assert_eq!(irq.generation(), 1);
    }

    #[test]
    fn ack_clears_only_acknowledged_known_bits() {
        let line = Arc::new(TestIrqLine::default());
        let irq = MmioInterrupt::new(line);
        assert!(irq.signal_used_queue(0).is_ok());
        assert!(irq.signal_config_change().is_ok());
        assert_eq!(irq.status(), INT_VRING | INT_CONFIG);

        irq.ack(INT_VRING);
        assert_eq!(irq.status(), INT_CONFIG);

        // Garbage bits neither clear anything nor get stored.
        irq.ack(0xffff_fff0);
        assert_eq!(irq.status(), INT_CONFIG);

        irq.ack(INT_CONFIG);
        assert_eq!(irq.status(), 0);
    }

    #[test]
    fn signal_failure_propagates_but_status_stays_set() {
        let line = Arc::new(TestIrqLine::failing());
        let irq = MmioInterrupt::new(line);
        assert!(irq.signal_used_queue(0).is_err());
        assert_eq!(irq.status(), INT_VRING);
    }
}
