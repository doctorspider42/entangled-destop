//! KVM irqfd interrupt lines (backlog MVP-306).
//!
//! The Linux implementation of [`virtio_core::interrupt::IrqLine`]: an
//! `EventFd` registered with KVM as an **irqfd** for one GSI. Triggering it
//! injects the interrupt entirely inside the kernel, so a device raising its
//! interrupt never round-trips through userspace.
//!
//! This is the seam the WHP backend needed a peer for. On Windows the same
//! trait is implemented by `crate::irqchip::ioapic::IoApicLine`, which runs the
//! IOAPIC redirection-table lookup in userspace and asks WHP's local APIC to
//! deliver the message. Every consumer — the serial console, both virtio
//! transports — takes an `Arc<dyn IrqLine>` and cannot tell the two apart.

use kvm_ioctls::VmFd;
use virtio_core::interrupt::{InterruptError, IrqLine};
use vmm_sys_util::eventfd::{EventFd, EFD_NONBLOCK};

/// An `EventFd` registered with KVM as an irqfd for one device's GSI.
///
/// The GSIs live on the in-kernel IOAPIC's ISA-compatible pins, which default
/// to edge triggering — writing the eventfd produces one edge per used-buffer
/// batch, matching what other mmio-based VMMs do. If a guest ever turns out to
/// have configured the pin level-triggered, this is where de-assertion
/// (`KVM_IRQ_LINE` pairs or a resample eventfd) would go.
///
/// # Historical note: interrupts were lost without a MADT/MP table
///
/// Measured while adding the MVP-307 boot benchmark, before this machine
/// published an MP table or ACPI tables: booting with a virtio-blk disk stalled
/// on the *first* disk read in roughly one boot in three. At the stall the
/// device had completed the request and `INTERRUPT_STATUS` still read
/// `INT_VRING`, i.e. the guest never ran its handler — the injection was lost,
/// not the kick. The guest reported "ACPI MADT or MP tables are not detected"
/// and "Switch to virtual wire mode", i.e. it took the GSI through the 8259 as
/// ExtINT instead of through the IOAPIC. `crate::mptable` and `crate::acpi` fixed
/// it; the note stays because the same failure mode is the first thing to
/// suspect if an interrupt ever goes missing again.
pub struct IrqFdLine {
    event: EventFd,
}

impl IrqFdLine {
    /// Creates a non-blocking eventfd and registers it with `vm` as the irqfd
    /// for `gsi`.
    pub fn new(vm: &VmFd, gsi: u32) -> Result<Self, IrqFdError> {
        let event =
            EventFd::new(EFD_NONBLOCK).map_err(|source| IrqFdError::EventFd { gsi, source })?;
        vm.register_irqfd(&event, gsi)
            .map_err(|source| IrqFdError::Register { gsi, source })?;
        Ok(Self { event })
    }

    /// Wraps an eventfd that the caller has already registered (or that a test
    /// wants to observe directly).
    pub fn from_event(event: EventFd) -> Self {
        Self { event }
    }
}

impl IrqLine for IrqFdLine {
    fn trigger(&self) -> Result<(), InterruptError> {
        self.event
            .write(1)
            .map_err(|e| InterruptError::Signal(e.to_string()))
    }
}

#[derive(Debug, thiserror::Error)]
pub enum IrqFdError {
    #[error("failed to create the interrupt eventfd for GSI {gsi}: {source}")]
    EventFd {
        gsi: u32,
        #[source]
        source: std::io::Error,
    },

    #[error("failed to register the irqfd for GSI {gsi}: {source}")]
    Register {
        gsi: u32,
        #[source]
        source: kvm_ioctls::Error,
    },
}
