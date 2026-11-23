//! The IDE controller as a PCI function (ADR-0012).
//!
//! Only an identity: class `0101`, programming interface `00` — both
//! channels in compatibility mode, no bus master. A controller in that mode
//! decodes the fixed ISA ports and raises IRQ 14 and 15 whatever its BARs
//! say, so the function has no BARs and no interrupt pin, and the
//! controller itself (`ide::Controller`) sits on the machine bus.
//!
//! The vendor and device ids are deliberately ones no operating system has a
//! chipset driver for. An Intel PIIX id would bring Windows' `intelide.sys`,
//! which expects that chip's timing registers and its bus master; with an
//! unknown id Windows binds its standard dual-channel PCI IDE driver by
//! class, and Linux its generic one.

use crate::pci::ConfigSpace;

/// The vendor id QEMU and Bochs use for devices that have no real vendor.
pub const VENDOR_ID: u16 = 0x1234;
pub const DEVICE_ID: u16 = 0x1de0;
/// Mass storage, IDE, compatibility mode on both channels.
pub const CLASS_IDE: u32 = 0x0101_0000;

/// The function's configuration space.
pub fn config_space() -> ConfigSpace {
    ConfigSpace::type0(VENDOR_ID, DEVICE_ID, CLASS_IDE, 1).with_subsystem(VENDOR_ID, DEVICE_ID)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_function_is_a_compatibility_mode_ide_controller_with_no_interrupt_pin() {
        let space = config_space();
        // Class code in the top three bytes of register 0x08.
        assert_eq!(space.read_dword(0x08) >> 8, 0x01_0100);
        // Interrupt pin (bits 8-15 of register 0x3c): none.
        assert_eq!(space.read_dword(0x3c) >> 8 & 0xff, 0);
        for bar in 0..6 {
            assert!(space.bar_window(bar).is_none());
        }
    }
}
