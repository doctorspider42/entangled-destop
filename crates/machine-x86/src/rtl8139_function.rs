//! The RTL8139 network card as a PCI function (ADR-0012).
//!
//! The card's 256 registers twice, as the real one has them: in an I/O BAR,
//! which is how Windows XP's in-box driver reaches them, and in a memory
//! BAR. One `INTA#`, wired to the interrupt `\_SB.PCI0._PRT` names for the
//! function's device number.
//!
//! The line is level-triggered on the card and this machine's interrupt
//! lines are edges, so two things stand in for the level: the card raises
//! an edge whenever a cause becomes pending or is left pending by a write
//! (`rtl8139::Rtl8139`), and [`EoiRelay`] raises one more when the guest
//! ends an interrupt with a cause still there — the case an edge alone
//! loses, a frame arriving while the handler is running.

use std::sync::{Arc, Weak};

use crate::irqchip::ioapic::EoiListener;
use crate::pci::{ConfigSpace, PciError};

/// The BAR with the registers in I/O space.
pub const IO_BAR: u8 = 0;
/// The BAR with the same registers in memory space.
pub const MEMORY_BAR: u8 = 1;

/// The function's configuration space: its ports at `ports`, its memory
/// window at `memory`, and `INTA#` on `irq`.
pub fn config_space(ports: u16, memory: u32, irq: u8) -> Result<ConfigSpace, PciError> {
    Ok(ConfigSpace::type0(
        rtl8139::VENDOR_ID,
        rtl8139::DEVICE_ID,
        rtl8139::CLASS_ETHERNET,
        rtl8139::REVISION,
    )
    .with_subsystem(rtl8139::VENDOR_ID, rtl8139::DEVICE_ID)
    .with_io_bar(IO_BAR, ports, rtl8139::REGISTERS)?
    .with_memory_bar(MEMORY_BAR, memory, u32::from(rtl8139::REGISTERS))?
    .with_interrupt(1, irq))
}

/// The card on the bus.
pub struct NicSlot {
    pub card: Arc<rtl8139::Rtl8139>,
    /// The token `PciRoot` knows this function by.
    pub(crate) owner: usize,
    /// Whatever has to live as long as the card does (its [`EoiRelay`]).
    pub(crate) _keep: Option<Arc<dyn std::any::Any + Send + Sync>>,
}

/// Tells the card when the guest has ended its interrupt, so a cause that is
/// still pending raises the line again.
pub struct EoiRelay {
    card: Weak<rtl8139::Rtl8139>,
}

impl EoiRelay {
    pub fn new(card: &Arc<rtl8139::Rtl8139>) -> Arc<Self> {
        Arc::new(Self {
            card: Arc::downgrade(card),
        })
    }

    /// What `IoApic::listen` takes; the relay itself must be kept alive.
    pub fn listener(self: &Arc<Self>) -> Weak<dyn EoiListener> {
        let listener: Arc<dyn EoiListener> = self.clone();
        Arc::downgrade(&listener)
    }
}

impl EoiListener for EoiRelay {
    fn eoi(&self) {
        if let Some(card) = self.card.upgrade() {
            card.interrupt_acknowledged();
        }
    }

    fn unmasked(&self) {
        // A pin unmasked with a cause pending is a level the guest has not
        // seen yet.
        self.eoi();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_function_has_an_io_bar_a_memory_bar_and_inta() {
        let space = config_space(0xc100, 0xc000_8000, 6).unwrap();
        assert_eq!(space.read_dword(0x00), 0x8139_10ec);
        assert_eq!(space.read_dword(0x08) >> 8, 0x02_0000);
        // BAR 0: I/O at 0xc100. BAR 1: memory.
        assert_eq!(space.read_dword(0x10), 0xc101);
        assert_eq!(space.read_dword(0x14), 0xc000_8000);
        // Interrupt line 6, pin INTA#.
        assert_eq!(space.read_dword(0x3c) & 0xffff, 0x0106);
        assert!(
            config_space(0xc180, 0xc000_8000, 6).is_err(),
            "misaligned ports"
        );
    }
}
