//! Reset-vector firmware ROM placement (backlog UEFI-1801).
//!
//! A flash-image firmware is entered through the architectural reset vector, so
//! its placement is not a choice: the image must end exactly at 4 GiB, which is
//! what puts `0xffff_fff0` inside it. Plain OVMF is *built* for this — its 4 MiB
//! build sets `FW_BASE_ADDRESS = 0xFFC00000` with `FW_SIZE = 0x00400000`
//! (`OvmfPkg/Include/Fdf/OvmfPkgDefines.fdf.inc`), i.e. `0xFFC00000 + 0x400000
//! == 0x1_0000_0000` — so any other placement would also break the firmware's
//! own internal addresses.
//!
//! Pure arithmetic and validation; builds and tests on every platform.

use machine_x86::layout;

use crate::FirmwareError;

/// Host page size assumed for guest memory slots. KVM requires memory regions
/// to be page aligned in both address and size.
pub const PAGE_SIZE: u64 = 0x1000;

/// The lowest address a firmware ROM may start at. The in-kernel IOAPIC lives
/// at [`layout::IOAPIC_ADDR`] and the local APIC just above it; a ROM slot
/// covering either would shadow MMIO the guest needs, and KVM resolves memory
/// slots before MMIO.
pub const ROM_FLOOR: u64 = layout::IOAPIC_ADDR as u64 + PAGE_SIZE;

/// Where a reset-vector firmware image goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RomPlacement {
    pub guest_addr: u64,
    pub len: u64,
}

impl RomPlacement {
    /// One past the last byte of the ROM. Always exactly 4 GiB.
    pub fn end(&self) -> u64 {
        self.guest_addr + self.len
    }

    /// True when `addr` falls inside the ROM.
    pub fn contains(&self, addr: u64) -> bool {
        addr >= self.guest_addr && addr < self.end()
    }
}

/// Places a firmware image of `len` bytes so that its last byte is at
/// 4 GiB − 1, i.e. at `0x1_0000_0000 - len`.
///
/// Refuses images that are not a whole number of pages (KVM would reject the
/// memory slot) and images so large that the ROM would swallow the APIC/MMIO
/// window — a 4 MiB OVMF-style build lands at `0xffc0_0000`, three orders of
/// magnitude clear of the floor.
pub fn place_at_top_of_32bit(len: u64) -> Result<RomPlacement, FirmwareError> {
    if len == 0 || len % PAGE_SIZE != 0 {
        return Err(FirmwareError::NotPageAligned {
            len,
            page: PAGE_SIZE,
        });
    }
    let guest_addr = layout::TOP_OF_32BIT
        .checked_sub(len)
        .ok_or(FirmwareError::RomTooLarge {
            len,
            start: 0,
            limit: ROM_FLOOR,
        })?;
    if guest_addr < ROM_FLOOR {
        return Err(FirmwareError::RomTooLarge {
            len,
            start: guest_addr,
            limit: ROM_FLOOR,
        });
    }
    Ok(RomPlacement { guest_addr, len })
}

#[cfg(test)]
mod tests {
    use super::*;
    use machine_x86::{e820_map, E820Type};

    /// The number that matters: a 4 MiB image lands where OVMF is linked for.
    #[test]
    fn four_mib_rom_ends_at_four_gib() {
        let p = place_at_top_of_32bit(4 << 20).unwrap();
        assert_eq!(
            p.guest_addr, 0xffc0_0000,
            "must match OVMF's FW_BASE_ADDRESS"
        );
        assert_eq!(p.end(), 0x1_0000_0000, "the ROM must end exactly at 4 GiB");
        assert!(
            p.contains(layout::RESET_VECTOR),
            "reset vector must be in ROM"
        );
        assert!(
            p.contains(0xffff_ffff),
            "the last byte must be the last byte"
        );
        assert!(!p.contains(p.guest_addr - 1));
    }

    /// Every legal size, however small, still has to cover the reset vector.
    #[test]
    fn any_size_still_covers_the_reset_vector() {
        for pages in [1u64, 2, 16, 0x100, 0x200, 0x400, 0x1000] {
            let p = place_at_top_of_32bit(pages * PAGE_SIZE).unwrap();
            assert_eq!(p.end(), layout::TOP_OF_32BIT);
            assert!(
                p.contains(layout::RESET_VECTOR),
                "{pages} pages: {:#x} does not cover the reset vector",
                p.guest_addr
            );
        }
    }

    #[test]
    fn rejects_unaligned_and_empty_images() {
        for len in [0u64, 1, 0xfff, 0x1001, (4 << 20) + 1] {
            assert!(
                matches!(
                    place_at_top_of_32bit(len),
                    Err(FirmwareError::NotPageAligned { .. })
                ),
                "len {len:#x} should have been rejected"
            );
        }
    }

    /// A ROM must never shadow the IOAPIC/LAPIC pages: memory slots win over
    /// MMIO in KVM, so an oversized ROM would silently break interrupts.
    #[test]
    fn refuses_to_overlap_the_apic_and_mmio_window() {
        // Largest image that still starts above the floor.
        let ok = layout::TOP_OF_32BIT - ROM_FLOOR;
        let p = place_at_top_of_32bit(ok).unwrap();
        assert_eq!(p.guest_addr, ROM_FLOOR);
        assert!(p.guest_addr > layout::IOAPIC_ADDR as u64);
        assert!(p.guest_addr > layout::LAPIC_ADDR as u64 || p.guest_addr == ROM_FLOOR);

        for len in [ok + PAGE_SIZE, 1 << 30, 3 << 30, layout::TOP_OF_32BIT] {
            assert!(
                matches!(
                    place_at_top_of_32bit(len),
                    Err(FirmwareError::RomTooLarge { .. })
                ),
                "len {len:#x} reaches down to {:#x} and should have been refused",
                layout::TOP_OF_32BIT.wrapping_sub(len)
            );
        }
    }

    /// The ROM is not RAM: no E820 entry may overlap it, and it must sit above
    /// the MMIO hole where RAM is forbidden anyway.
    #[test]
    fn rom_is_never_ram() {
        let p = place_at_top_of_32bit(4 << 20).unwrap();
        assert!(p.guest_addr >= layout::MMIO_HOLE_START);
        for mem_mib in [128u64, 512, 2048, 3072] {
            let mem = mem_mib << 20;
            for e in e820_map(mem) {
                let overlaps = e.addr < p.end() && p.guest_addr < e.addr + e.size;
                assert!(
                    !overlaps,
                    "E820 {:?} entry {:#x}+{:#x} overlaps the ROM at {:#x}",
                    e.kind, e.addr, e.size, p.guest_addr
                );
                if e.kind == E820Type::Ram {
                    assert!(e.addr + e.size <= layout::MMIO_HOLE_START);
                }
            }
        }
    }

    /// The virtio-mmio window must stay reachable: the ROM sits far above it.
    #[test]
    fn rom_does_not_shadow_the_virtio_window() {
        let p = place_at_top_of_32bit(4 << 20).unwrap();
        for slot in 0..8u64 {
            let base = layout::virtio_mmio_slot(slot);
            assert!(!p.contains(base), "ROM shadows virtio slot {slot}");
        }
    }
}
