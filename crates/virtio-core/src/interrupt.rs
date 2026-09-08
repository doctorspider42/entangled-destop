//! Device interrupt plumbing (backlog MVP-306).
//!
//! Two layers, so devices stay transport-agnostic:
//!
//! * [`Interrupt`] is what a device sees — "tell the driver queue *n* has used
//!   buffers" / "tell the driver the config space changed". An MSI-X backed
//!   implementation of the same trait would slot in here without a device
//!   noticing.
//! * [`IrqLine`] is the host mechanism that actually raises the line. On Linux
//!   `machine-x86` implements it with an `EventFd` registered as a KVM irqfd,
//!   so signalling never round-trips through userspace. Tests use counters.
//! * [`MsiSink`] is the same idea one step further along: the host mechanism
//!   that delivers one **MSI message** — an (address, data) pair the device
//!   would have written to memory on real hardware. It is what
//!   [`MsixInterrupt`](crate::pci::MsixInterrupt) signals through, and it is
//!   deliberately neutral: no GSI, no eventfd, no `kvm_msi`, so the KVM
//!   implementation (`machine_x86::msi`) and a future WHP one differ only in
//!   what they do with those two numbers.
//!
//! [`LineInterrupt`] glues the first two together and owns the shared state a
//! single-line (INTx-style) transport exposes: the pending-bit word, its
//! acknowledge semantics and the config generation counter.
//!
//! [`TransportInterrupt`] is the seam between a transport and whichever of the
//! two it was built with. A transport needs more than [`Interrupt`]: it serves
//! the pending-bit word to the guest and acknowledges it. Both
//! [`LineInterrupt`] and [`MsixInterrupt`](crate::pci::MsixInterrupt) implement
//! it, which is what lets `virtio-pci` swap in the MSI-X capable object without
//! `TransportState` — or virtio-mmio — knowing that MSI-X exists.
//!
//! **Both transports use it unchanged.** virtio-mmio serves the word through
//! `INTERRUPT_STATUS` (write-to-ack via `INTERRUPT_ACK`), virtio-pci through
//! the ISR byte in its BAR (read-to-clear); the two bit positions are
//! identical — `INT_VRING`/[`crate::pci::ISR_QUEUE`] is bit 0 and
//! `INT_CONFIG`/[`crate::pci::ISR_CONFIG`] is bit 1 (asserted in
//! `pci::tests::isr_bits_match_the_mmio_interrupt_word`).

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

/// One MSI (message signalled interrupt) message: the address a device would
/// have written on real hardware, and the data it would have written there.
///
/// Both halves are **guest-programmed** — they come straight out of the MSI-X
/// table the driver wrote — so nothing here may be used as a host address. It is
/// a message handed to the host's interrupt controller, which decodes it against
/// the *guest's* local APICs exactly as it would decode a write from a real
/// device.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MsiMessage {
    /// Full 64-bit message address (`address_lo` plus `address_hi` from the
    /// table entry). On x86 the low bits carry the destination APIC id and the
    /// redirection/destination-mode flags.
    pub address: u64,
    /// Message data: vector, delivery mode, trigger mode.
    pub data: u32,
}

/// The host mechanism that delivers an [`MsiMessage`].
///
/// The MSI counterpart of [`IrqLine`], and deliberately just as narrow: an
/// address/data pair in, an interrupt in the guest out. On Linux
/// `machine_x86::msi::KvmMsiSink` hands it to `KVM_SIGNAL_MSI`, which walks the
/// in-kernel local APICs; a WHP host would decode it and call
/// `vmm_core::hv::InterruptDelivery` instead. Neither type appears here
/// (ADR-0002).
///
/// Implementations must be non-blocking: this is called from a device's queue
/// worker thread and, for a synchronous kick, from a vCPU thread.
pub trait MsiSink: Send + Sync {
    fn send(&self, message: MsiMessage) -> Result<(), InterruptError>;
}

/// What a device uses to notify its driver. Transport-agnostic on purpose.
pub trait Interrupt: Send + Sync {
    /// A used buffer was added to `queue_index`.
    fn signal_used_queue(&self, queue_index: u16) -> Result<(), InterruptError>;

    /// The device configuration space changed.
    fn signal_config_change(&self) -> Result<(), InterruptError>;
}

/// What a *transport* needs from its interrupt object, on top of what a device
/// needs ([`Interrupt`]).
///
/// A transport does two things a device never does: it serves the pending-bit
/// word to the guest (virtio-mmio's `INTERRUPT_STATUS`, virtio-pci's ISR byte)
/// and it acknowledges it. Both [`LineInterrupt`] and
/// [`MsixInterrupt`](crate::pci::MsixInterrupt) implement this, which is the
/// whole reason `TransportState` — and therefore virtio-mmio — needs no
/// knowledge of MSI-X: it stores an `Arc<dyn TransportInterrupt>` and cannot
/// tell which one it has.
pub trait TransportInterrupt: Interrupt {
    /// The pending-interrupt word without clearing it.
    fn status(&self) -> u32;

    /// Clears the acknowledged bits (virtio-mmio's `INTERRUPT_ACK`).
    fn ack(&self, bits: u32);

    /// Device reset: nothing is pending any more.
    fn clear(&self);

    /// Returns the pending bits and clears them atomically (virtio-pci's
    /// read-to-clear ISR).
    fn take_status(&self) -> u32;

    /// **Machine** reset, as opposed to the device reset [`Self::clear`] is
    /// (ADR-0005).
    ///
    /// A device reset is a driver writing 0 to `device_status`; a machine reset
    /// is the whole function coming back from power-on, so it also drops the
    /// state a device reset deliberately keeps — `config_generation`, and (for
    /// MSI-X) the table and the message-control register. The default is
    /// [`Self::clear`], which is the whole story for a transport whose only
    /// interrupt state is the pending word.
    fn power_on_reset(&self) {
        self.clear();
    }

    /// The `config_generation` counter.
    fn generation(&self) -> u32;

    /// This interrupt's guest-visible state, for a snapshot (ADR-0006).
    ///
    /// The default covers a transport whose only interrupt state is the
    /// pending word and the generation counter, which is virtio-mmio and
    /// virtio-pci without MSI-X; [`crate::MsixInterrupt`] adds the table.
    fn save_interrupt(&self) -> crate::save::InterruptState {
        crate::save::InterruptState {
            isr: self.status(),
            generation: self.generation(),
            msix: None,
        }
    }

    /// Puts it back.
    fn load_interrupt(
        &self,
        state: &crate::save::InterruptState,
    ) -> Result<(), crate::save::StateError>;

    /// Upcast to the device-facing half, for [`crate::DeviceResources`].
    ///
    /// Written out rather than relying on `Arc<dyn Sub> -> Arc<dyn Super>`
    /// coercion so the crate keeps building on toolchains without trait
    /// upcasting.
    fn as_interrupt(self: Arc<Self>) -> Arc<dyn Interrupt>;
}

/// The virtio-mmio interrupt: the guest-visible `INTERRUPT_STATUS` word and
/// the config generation counter, plus the line that raises the IRQ.
///
/// Shared (`Arc`) between the transport — which serves register reads and the
/// `INTERRUPT_ACK` writes — and the device, which only ever signals. All state
/// is atomic because the two can sit on different vCPU threads.
pub struct LineInterrupt {
    status: AtomicU32,
    generation: AtomicU32,
    line: Arc<dyn IrqLine>,
}

impl LineInterrupt {
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

    /// Returns the pending bits and clears them in one atomic step.
    ///
    /// This is virtio-pci's ISR semantics (spec 1.2 §4.1.4.5: "reading from
    /// this register resets it to 0"); virtio-mmio uses [`Self::status`] plus
    /// [`Self::ack`] instead. Atomic because a device thread may set a bit
    /// between the read and the clear, and that interrupt must not be lost —
    /// with `swap` it stays pending for the next read instead.
    pub fn take_status(&self) -> u32 {
        self.status.swap(0, Ordering::AcqRel)
    }

    /// Restores the pending word and the generation counter from a snapshot.
    ///
    /// No raise: the line's *level* is the interrupt controller's state, saved
    /// with the chip, and re-triggering here would deliver a second copy of an
    /// interrupt the restored guest is already going to see.
    pub fn restore(&self, status: u32, generation: u32) {
        self.status.store(status, Ordering::Release);
        self.generation.store(generation, Ordering::Release);
    }

    /// Bumps `config_generation` without raising anything.
    ///
    /// The generation is a *read protocol* for the config space — a driver reads
    /// it, reads the config, reads it again — not an interrupt mechanism, so it
    /// has to advance on every config change whichever way the driver is told
    /// about it. [`MsixInterrupt`](crate::pci::MsixInterrupt) calls this and then
    /// sends an MSI instead of raising the line.
    pub fn bump_generation(&self) {
        self.generation.fetch_add(1, Ordering::AcqRel);
    }

    fn raise(&self, bit: u32) -> Result<(), InterruptError> {
        self.status.fetch_or(bit, Ordering::AcqRel);
        self.line.trigger()
    }
}

impl TransportInterrupt for LineInterrupt {
    fn status(&self) -> u32 {
        Self::status(self)
    }
    fn ack(&self, bits: u32) {
        Self::ack(self, bits)
    }
    fn clear(&self) {
        Self::clear(self)
    }
    fn take_status(&self) -> u32 {
        Self::take_status(self)
    }
    fn generation(&self) -> u32 {
        Self::generation(self)
    }
    /// The generation is monotonic across a *device* reset — a driver
    /// re-binding must not see it go backwards mid-read — but a rebooted
    /// machine is a fresh device, and a counter that survived would be the one
    /// piece of the previous boot the new guest could observe.
    fn power_on_reset(&self) {
        self.status.store(0, Ordering::Release);
        self.generation.store(0, Ordering::Release);
    }
    fn load_interrupt(
        &self,
        state: &crate::save::InterruptState,
    ) -> Result<(), crate::save::StateError> {
        Self::restore(self, state.isr, state.generation);
        Ok(())
    }
    fn as_interrupt(self: Arc<Self>) -> Arc<dyn Interrupt> {
        self
    }
}

impl Interrupt for LineInterrupt {
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
        let irq = LineInterrupt::new(line.clone());

        assert_eq!(irq.status(), 0);
        assert!(irq.signal_used_queue(0).is_ok());
        assert_eq!(irq.status(), INT_VRING);
        assert_eq!(line.count(), 1);
    }

    #[test]
    fn config_change_bumps_the_generation() {
        let line = Arc::new(TestIrqLine::default());
        let irq = LineInterrupt::new(line);

        assert_eq!(irq.generation(), 0);
        assert!(irq.signal_config_change().is_ok());
        assert_eq!(irq.status(), INT_CONFIG);
        assert_eq!(irq.generation(), 1);
    }

    #[test]
    fn ack_clears_only_acknowledged_known_bits() {
        let line = Arc::new(TestIrqLine::default());
        let irq = LineInterrupt::new(line);
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
        let irq = LineInterrupt::new(line);
        assert!(irq.signal_used_queue(0).is_err());
        assert_eq!(irq.status(), INT_VRING);
    }
}
