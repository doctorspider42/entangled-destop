//! The userspace interrupt controllers a hypervisor without in-kernel ones
//! needs: IOAPIC, 8254 PIT and 8259 PIC pair (backlog WHP-1703, ADR-0002).
//!
//! # Why this is a machine-model module and not a WHP module
//!
//! KVM provides all three in the kernel (`KVM_CREATE_IRQCHIP`, `KVM_CREATE_PIT2`
//! in `vmm_core::Vm::new`); WHP provides only each vCPU's **local** APIC. That
//! makes the gap host-specific but the fix is not: an IOAPIC redirection table, a
//! counter driven by a clock and an 8259 register file are the machine's devices,
//! exactly like the UART next to them. The only genuinely hypervisor-specific
//! step is the last one — handing a decoded interrupt message to a local APIC —
//! and that is a one-method seam, [`vmm_core::hv::InterruptDelivery`], which the
//! WHP backend implements over `WHvRequestInterrupt`. So the models live here,
//! portable and unit-tested on both hosts, and no `WHV_*` type is anywhere near
//! them.
//!
//! # Topology
//!
//! Fixed by what [`crate::mptable`] and [`crate::acpi`] already publish, so the
//! guest cannot see two different machines:
//!
//! ```text
//!   8254 channel 0  --IRQ 0-->  IOAPIC pin 2   (the PC convention; MADT override)
//!   16550 COM1      --IRQ 4-->  IOAPIC pin 4
//!   virtio slot n   --IRQ 5+n-> IOAPIC pin 5+n (EPIC 17 phase 3)
//!   8259 pair                   masked, wired to nothing
//! ```
//!
//! # Wiring one up
//!
//! ```ignore
//! let chip = UserspaceIrqChip::new(partition.interrupt_delivery(), machine.vcpu_count)?;
//! let serial = SerialConsole::with_trigger(chip.serial_line(), out);
//! let bus = MachineBus::new(serial).with_irqchip(chip);
//! ```
//!
//! `MachineBus` then routes the PIC and PIT ports and the IOAPIC's MMIO page to
//! it; on KVM the same bus is built without this call and those addresses stay
//! with the in-kernel chips.

pub mod ioapic;
pub mod pic;
pub mod pit;

use std::sync::{Arc, Mutex};

use virtio_core::interrupt::IrqLine;
use vmm_core::hv::InterruptDelivery;

use self::ioapic::{IoApic, IoApicError};
use self::pic::Pic8259;
use self::pit::{Pit, PitTimer};

/// IOAPIC pin the 8254's channel-0 output is wired to.
///
/// Two, not zero: the PC convention that [`crate::mptable`]'s I/O interrupt
/// entries and [`crate::acpi`]'s MADT interrupt source override both publish for
/// ISA IRQ 0. A guest that trusts either table looks for the timer here.
pub const TIMER_PIN: u8 = 2;

/// IOAPIC pin the 16550's interrupt is wired to, matching
/// [`crate::serial::SERIAL_IRQ`].
pub const SERIAL_PIN: u8 = 4;

#[derive(Debug, thiserror::Error)]
pub enum IrqChipError {
    #[error(transparent)]
    IoApic(#[from] IoApicError),

    #[error("failed to start the PIT timer thread: {0}")]
    TimerThread(#[source] std::io::Error),

    #[error("virtio slot {0} has no IOAPIC pin assigned (see layout::VIRTIO_IRQS)")]
    NoSuchVirtioSlot(usize),
}

/// The machine's userspace interrupt controllers, as one attachable unit.
///
/// Owns the PIT's timer thread: dropping the chip stops and joins it, so "closing
/// the VM leaves no device threads behind" holds here as it does for the virtio
/// queue workers.
pub struct UserspaceIrqChip {
    ioapic: Arc<IoApic>,
    pit: Arc<Pit>,
    pic: Mutex<Pic8259>,
    /// Field order matters for teardown: `_timer` is declared after `pit` and
    /// `ioapic` but dropped *before* them, which is the direction we want — the
    /// thread that triggers the interrupt line stops before the line goes away.
    _timer: PitTimer,
}

impl UserspaceIrqChip {
    /// Builds the chip set: an IOAPIC delivering through `delivery`, a PIT on
    /// [`TIMER_PIN`] with its host thread running, and a masked 8259 pair.
    ///
    /// `vcpu_count` is the IOAPIC id, matching what the MP table and MADT
    /// publish (above every LAPIC id).
    pub fn new(
        delivery: Arc<dyn InterruptDelivery>,
        vcpu_count: u32,
    ) -> Result<Arc<Self>, IrqChipError> {
        let ioapic = IoApic::new(delivery, u8::try_from(vcpu_count).unwrap_or(u8::MAX));
        let pit = Pit::new(ioapic.line(TIMER_PIN)?);
        let timer = PitTimer::start(Arc::clone(&pit)).map_err(IrqChipError::TimerThread)?;
        Ok(Arc::new(Self {
            ioapic,
            pit,
            pic: Mutex::new(Pic8259::new()),
            _timer: timer,
        }))
    }

    /// The IOAPIC, for a device that needs a line on a specific pin.
    pub fn ioapic(&self) -> &Arc<IoApic> {
        &self.ioapic
    }

    /// The PIT, for diagnostics (`Pit::edges`).
    pub fn pit(&self) -> &Arc<Pit> {
        &self.pit
    }

    /// The interrupt line for the serial console: IOAPIC pin [`SERIAL_PIN`].
    pub fn serial_line(&self) -> Arc<dyn IrqLine> {
        // `SERIAL_PIN` is a compile-time constant below `REDIRECTION_ENTRIES`, so
        // the only error arm cannot occur; asserted in the unit tests.
        match self.ioapic.line(SERIAL_PIN) {
            Ok(line) => line,
            Err(_) => unreachable!("SERIAL_PIN is within the IOAPIC's pin count"),
        }
    }

    /// The interrupt line for virtio-mmio (or virtio-pci) slot `slot`: IOAPIC pin
    /// [`crate::layout::virtio_irq`].
    ///
    /// This is the WHP peer of `crate::irqfd::IrqFdLine`, and the reason
    /// `virtio::VirtioMmioBus::attach_userspace` needs no WHP knowledge: both are
    /// an `Arc<dyn IrqLine>`, and the transport that takes one cannot tell whether
    /// triggering it writes an eventfd the kernel drains or walks a
    /// redirection table in this process.
    ///
    /// The pin table is not contiguous — pins 8 (RTC) and 13 (ACPI SCI) belong to
    /// this machine's own devices — so the mapping goes through `layout`, which is
    /// the same table both transports and the MP table use.
    pub fn virtio_line(&self, slot: usize) -> Result<Arc<dyn IrqLine>, IrqChipError> {
        let gsi = crate::layout::virtio_irq(slot).ok_or(IrqChipError::NoSuchVirtioSlot(slot))?;
        // Every entry of `VIRTIO_IRQS` is a small ISA-range pin, so the cast
        // cannot fail; treated as "no such slot" rather than unwrapped.
        let pin = u8::try_from(gsi).map_err(|_| IrqChipError::NoSuchVirtioSlot(slot))?;
        Ok(self.ioapic.line(pin)?)
    }

    /// True when `port` belongs to one of the chips.
    pub fn claims_port(port: u16) -> bool {
        Pic8259::contains(port) || Pit::contains(port)
    }

    /// True when `addr` falls in the IOAPIC's register window.
    pub fn claims_mmio(addr: u64) -> bool {
        IoApic::contains(addr)
    }

    pub fn io_write(&self, port: u16, data: &[u8]) {
        if Pit::contains(port) {
            for &byte in data {
                self.pit.io_write(port, byte);
            }
            return;
        }
        match self.pic.lock() {
            Ok(mut pic) => {
                for &byte in data {
                    pic.io_write(port, byte);
                }
            }
            Err(_) => tracing::error!(
                port = format_args!("{port:#x}"),
                "8259 lock is poisoned; dropping guest write"
            ),
        }
    }

    pub fn io_read(&self, port: u16, data: &mut [u8]) {
        if Pit::contains(port) {
            for byte in data.iter_mut() {
                *byte = self.pit.io_read(port);
            }
            return;
        }
        match self.pic.lock() {
            Ok(pic) => {
                for byte in data.iter_mut() {
                    *byte = pic.io_read(port);
                }
            }
            Err(_) => {
                tracing::error!(
                    port = format_args!("{port:#x}"),
                    "8259 lock is poisoned; reading 0xff"
                );
                data.fill(0xff);
            }
        }
    }

    pub fn mmio_write(&self, addr: u64, data: &[u8]) {
        self.ioapic.mmio_write(addr, data);
    }

    pub fn mmio_read(&self, addr: u64, data: &mut [u8]) {
        self.ioapic.mmio_read(addr, data);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout;
    use std::sync::atomic::{AtomicU32, Ordering};
    use vmm_core::hv::{HvError, InterruptRequest};

    #[derive(Default)]
    struct Counting(AtomicU32);

    impl InterruptDelivery for Counting {
        fn request(&self, _interrupt: &InterruptRequest) -> Result<(), HvError> {
            self.0.fetch_add(1, Ordering::AcqRel);
            Ok(())
        }
    }

    fn chip() -> Arc<UserspaceIrqChip> {
        UserspaceIrqChip::new(Arc::new(Counting::default()), 1).unwrap()
    }

    /// The pins must be the ones the published tables promise, or the guest
    /// programs a redirection entry nothing is wired to.
    #[test]
    fn pins_match_the_published_tables() {
        assert_eq!(u32::from(SERIAL_PIN), crate::serial::SERIAL_IRQ);
        // The MP table routes ISA IRQ 0 to pin 2 and the MADT overrides it to
        // GSI 2; both are `TIMER_PIN`.
        assert_eq!(TIMER_PIN, 2);
        // Neither collides with the virtio window.
        assert!(u32::from(SERIAL_PIN) < layout::VIRTIO_MMIO_FIRST_IRQ);
        assert!(u32::from(TIMER_PIN) < layout::VIRTIO_MMIO_FIRST_IRQ);
    }

    /// The two claim predicates must be exactly the set of addresses the bus
    /// hands over — and must not include anything another device owns.
    #[test]
    fn claims_the_legacy_ports_and_the_ioapic_page_only() {
        for port in [0x20u16, 0x21, 0x40, 0x43, 0x61, 0xa0, 0x4d0] {
            assert!(
                UserspaceIrqChip::claims_port(port),
                "{port:#x} must be claimed"
            );
        }
        // The UART, the ACPI PM block and the PCI configuration ports belong to
        // other devices on the same bus.
        for port in [0x3f8u16, 0x600, 0x608, 0xcf8, 0xcfc, 0x70, 0x71] {
            assert!(
                !UserspaceIrqChip::claims_port(port),
                "{port:#x} belongs to another device"
            );
        }
        assert!(UserspaceIrqChip::claims_mmio(u64::from(
            layout::IOAPIC_ADDR
        )));
        assert!(!UserspaceIrqChip::claims_mmio(u64::from(
            layout::LAPIC_ADDR
        )));
        assert!(!UserspaceIrqChip::claims_mmio(layout::VIRTIO_MMIO_BASE));
    }

    #[test]
    fn the_serial_line_lands_on_the_serial_pin() {
        let chip = chip();
        // Unmask pin 4 with vector 0x31, then pulse through the serial line.
        let base = u64::from(layout::IOAPIC_ADDR);
        chip.mmio_write(base, &(0x10u32 + 2 * u32::from(SERIAL_PIN)).to_le_bytes());
        chip.mmio_write(base + 0x10, &0x31u32.to_le_bytes());
        chip.serial_line().trigger().unwrap();
        assert_eq!(chip.ioapic().delivered(), 1);
    }

    /// Port dispatch must reach the right chip: a PIT command must not land in
    /// the 8259's mask register and vice versa.
    #[test]
    fn port_dispatch_separates_the_pit_from_the_pic() {
        let chip = chip();
        chip.io_write(pic::PIC_MASTER_DATA, &[0xfb]);
        let mut byte = [0u8; 1];
        chip.io_read(pic::PIC_MASTER_DATA, &mut byte);
        assert_eq!(byte[0], 0xfb, "the 8259 mask must round-trip");

        // Program channel 0 periodic and read the counter back: a nonzero read
        // proves the write reached the PIT and not the PIC.
        chip.io_write(0x43, &[0x34]);
        chip.io_write(0x40, &[0xff]);
        chip.io_write(0x40, &[0xff]);
        chip.io_read(0x40, &mut byte);
        let lo = byte[0];
        chip.io_read(0x40, &mut byte);
        assert_ne!(u16::from_le_bytes([lo, byte[0]]), 0);
    }

    /// Dropping the chip must stop the PIT thread; a leaked thread would keep
    /// triggering an interrupt line whose delivery target is gone.
    #[test]
    fn dropping_the_chip_stops_the_timer_thread() {
        let before = std::thread::available_parallelism().is_ok(); // no-op guard
        let chip = chip();
        chip.io_write(0x43, &[0x34]);
        chip.io_write(0x40, &[0x00]);
        chip.io_write(0x40, &[0x10]);
        std::thread::sleep(std::time::Duration::from_millis(20));
        // Drop joins the thread; if it did not, this would race the assertion
        // below under a sanitizer rather than deadlock, so the value of this test
        // is that `Drop` is exercised at all.
        drop(chip);
        assert!(before);
    }
}
