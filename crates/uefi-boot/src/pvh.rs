//! The PVH start-of-day structures handed to a firmware in `%ebx`
//! (backlog UEFI-1802).
//!
//! Layout mirrors Xen's `xen/include/public/arch-x86/hvm/start_info.h`
//! verbatim; the encoders below produce those exact byte sequences so the
//! layout is testable without a hypervisor (and on a non-Linux dev machine).
//!
//! Version 1 is the minimum useful version: `memmap_paddr`/`memmap_entries`
//! were added in it, and EDK2's `PlatformScanE820Pvh()` reads exactly those two
//! fields to learn where guest RAM is.

use machine_x86::{layout, E820Type};

use crate::FirmwareError;

/// `XEN_HVM_START_MAGIC_VALUE`.
pub const XEN_HVM_START_MAGIC_VALUE: u32 = 0x336e_c578;

/// The `hvm_start_info` version we emit.
pub const START_INFO_VERSION: u32 = 1;

/// `sizeof(struct hvm_start_info)` for version 1: magic, version, flags,
/// nr_modules (4×4) + modlist_paddr, cmdline_paddr, rsdp_paddr, memmap_paddr
/// (4×8) + memmap_entries, reserved (2×4).
pub const START_INFO_SIZE: usize = 16 + 32 + 8;

/// `sizeof(struct hvm_memmap_table_entry)`: addr, size (2×8) + type, reserved
/// (2×4).
pub const MEMMAP_ENTRY_SIZE: usize = 24;

/// `XEN_HVM_MEMMAP_TYPE_*`.
pub const XEN_HVM_MEMMAP_TYPE_RAM: u32 = 1;
pub const XEN_HVM_MEMMAP_TYPE_RESERVED: u32 = 2;
/// ACPI reclaimable — where the ACPI tables live. EDK2 ignores every entry that
/// is not `XEN_HVM_MEMMAP_TYPE_RAM` (`PlatformScanE820Pvh()` filters on it), so
/// this is documentation for the firmware rather than instruction; the tables
/// stay intact because the range is also outside every RAM entry.
pub const XEN_HVM_MEMMAP_TYPE_ACPI: u32 = 3;

/// One `hvm_memmap_table_entry`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MemmapEntry {
    pub addr: u64,
    pub size: u64,
    pub kind: u32,
}

impl MemmapEntry {
    fn encode(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&self.addr.to_le_bytes());
        out.extend_from_slice(&self.size.to_le_bytes());
        out.extend_from_slice(&self.kind.to_le_bytes());
        out.extend_from_slice(&0u32.to_le_bytes()); // reserved
    }
}

/// The `hvm_start_info` structure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StartInfo {
    pub cmdline_paddr: u64,
    /// Physical address of the ACPI RSDP — `machine_x86::layout::ACPI_RSDP_START`
    /// in practice, since `machine_x86::acpi` puts the tables there.
    ///
    /// EDK2's `InstallCloudHvTables()` dereferences this pointer, walks the XSDT
    /// installing every table it lists, then installs the DSDT from the FADT's
    /// `X_DSDT`. A zero — or an address whose RSDP fails its signature and
    /// checksum check — makes it return `EFI_NOT_FOUND` and install nothing,
    /// which was ADR-0003's phase-2 gap.
    pub rsdp_paddr: u64,
    pub memmap_paddr: u64,
    pub memmap_entries: u32,
}

impl StartInfo {
    /// Encodes the structure exactly as Xen defines it.
    pub fn encode(&self) -> [u8; START_INFO_SIZE] {
        let mut out = [0u8; START_INFO_SIZE];
        out[0x00..0x04].copy_from_slice(&XEN_HVM_START_MAGIC_VALUE.to_le_bytes());
        out[0x04..0x08].copy_from_slice(&START_INFO_VERSION.to_le_bytes());
        out[0x08..0x0c].copy_from_slice(&0u32.to_le_bytes()); // flags
        out[0x0c..0x10].copy_from_slice(&0u32.to_le_bytes()); // nr_modules
        out[0x10..0x18].copy_from_slice(&0u64.to_le_bytes()); // modlist_paddr
        out[0x18..0x20].copy_from_slice(&self.cmdline_paddr.to_le_bytes());
        out[0x20..0x28].copy_from_slice(&self.rsdp_paddr.to_le_bytes());
        out[0x28..0x30].copy_from_slice(&self.memmap_paddr.to_le_bytes());
        out[0x30..0x34].copy_from_slice(&self.memmap_entries.to_le_bytes());
        out[0x34..0x38].copy_from_slice(&0u32.to_le_bytes()); // reserved
        out
    }
}

/// Encodes a memory map table, refusing one that would overflow its page.
pub fn encode_memmap(entries: &[MemmapEntry]) -> Result<Vec<u8>, FirmwareError> {
    if entries.len() > layout::PVH_MEMMAP_MAX_ENTRIES {
        return Err(FirmwareError::MemmapTooLarge {
            needed: entries.len(),
            max: layout::PVH_MEMMAP_MAX_ENTRIES,
        });
    }
    let mut out = Vec::with_capacity(entries.len() * MEMMAP_ENTRY_SIZE);
    for entry in entries {
        entry.encode(&mut out);
    }
    Ok(out)
}

/// Builds the PVH memory map for a VM with `mem_size` bytes of RAM, from the
/// same E820 map the direct-Linux path uses — so the two boot modes cannot
/// disagree about where RAM is. The firmware ROM window is not in it, by
/// construction: `e820_map` describes nothing between the MMIO hole and
/// 4 GiB, and a guest big enough for the high-RAM split continues at exactly
/// 4 GiB — where the ROM window ends.
pub fn memmap_for(mem_size: u64) -> Vec<MemmapEntry> {
    machine_x86::e820_map(mem_size)
        .into_iter()
        .map(|e| MemmapEntry {
            addr: e.addr,
            size: e.size,
            kind: match e.kind {
                E820Type::Ram => XEN_HVM_MEMMAP_TYPE_RAM,
                E820Type::Reserved => XEN_HVM_MEMMAP_TYPE_RESERVED,
                E820Type::AcpiReclaim => XEN_HVM_MEMMAP_TYPE_ACPI,
            },
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn start_info_field_offsets_match_xen() {
        let si = StartInfo {
            cmdline_paddr: 0x1122_3344_5566_7788,
            rsdp_paddr: 0x0102_0304_0506_0708,
            memmap_paddr: 0xdead_beef_0000_1000,
            memmap_entries: 3,
        };
        let b = si.encode();
        assert_eq!(b.len(), 56, "hvm_start_info v1 is 56 bytes");
        assert_eq!(&b[0..4], &0x336e_c578u32.to_le_bytes(), "magic at offset 0");
        assert_eq!(&b[4..8], &1u32.to_le_bytes(), "version 1 at offset 4");
        assert_eq!(&b[8..12], &[0; 4], "flags");
        assert_eq!(&b[12..16], &[0; 4], "nr_modules");
        assert_eq!(&b[16..24], &[0; 8], "modlist_paddr");
        assert_eq!(&b[24..32], &si.cmdline_paddr.to_le_bytes());
        assert_eq!(&b[32..40], &si.rsdp_paddr.to_le_bytes());
        assert_eq!(&b[40..48], &si.memmap_paddr.to_le_bytes());
        assert_eq!(&b[48..52], &3u32.to_le_bytes());
        assert_eq!(&b[52..56], &[0; 4], "reserved");
    }

    #[test]
    fn memmap_entries_are_24_bytes_in_xen_order() {
        let raw = encode_memmap(&[
            MemmapEntry {
                addr: 0,
                size: 0x9fc00,
                kind: XEN_HVM_MEMMAP_TYPE_RAM,
            },
            MemmapEntry {
                addr: 0x10_0000,
                size: 0x1000,
                kind: XEN_HVM_MEMMAP_TYPE_RESERVED,
            },
        ])
        .unwrap();
        assert_eq!(raw.len(), 2 * MEMMAP_ENTRY_SIZE);
        assert_eq!(&raw[0..8], &0u64.to_le_bytes());
        assert_eq!(&raw[8..16], &0x9fc00u64.to_le_bytes());
        assert_eq!(&raw[16..20], &1u32.to_le_bytes());
        assert_eq!(&raw[20..24], &[0; 4]);
        assert_eq!(&raw[24..32], &0x10_0000u64.to_le_bytes());
        assert_eq!(&raw[40..44], &2u32.to_le_bytes());
    }

    /// The whole table plus the start_info must fit in the pages the layout
    /// reserves, and the map must agree with E820 about RAM.
    #[test]
    fn memmap_mirrors_e820_and_fits_its_page() {
        let mem = 2048u64 << 20;
        let entries = memmap_for(mem);
        let e820 = machine_x86::e820_map(mem);
        assert_eq!(entries.len(), e820.len());
        for (m, e) in entries.iter().zip(&e820) {
            assert_eq!(m.addr, e.addr);
            assert_eq!(m.size, e.size);
        }
        let ram: u64 = entries
            .iter()
            .filter(|e| e.kind == XEN_HVM_MEMMAP_TYPE_RAM)
            .map(|e| e.size)
            .sum();
        assert!(ram > 0 && ram < mem);

        let raw = encode_memmap(&entries).unwrap();
        assert!(raw.len() as u64 <= 0x1000, "memmap must fit one page");
        // The three reserved areas must not collide.
        assert_ne!(layout::PVH_START_INFO_START, layout::PVH_MEMMAP_START);
        assert!(layout::PVH_START_INFO_START + START_INFO_SIZE as u64 <= layout::PVH_MEMMAP_START);
        assert!(layout::PVH_MEMMAP_START + raw.len() as u64 <= layout::PVH_CMDLINE_START);
    }

    #[test]
    fn refuses_an_oversized_memmap() {
        let entries = vec![
            MemmapEntry {
                addr: 0,
                size: 0x1000,
                kind: XEN_HVM_MEMMAP_TYPE_RAM,
            };
            layout::PVH_MEMMAP_MAX_ENTRIES + 1
        ];
        assert!(matches!(
            encode_memmap(&entries),
            Err(FirmwareError::MemmapTooLarge { .. })
        ));
    }

    /// The PVH boot data must not land where the firmware itself is loaded
    /// (CloudHv's PT_LOAD starts at 1 MiB) nor on the MP table.
    #[test]
    fn boot_data_is_clear_of_the_firmware_and_mptable() {
        for addr in [
            layout::PVH_START_INFO_START,
            layout::PVH_MEMMAP_START,
            layout::PVH_CMDLINE_START,
        ] {
            assert!(addr >= 0x1000, "must not clobber the real-mode IVT/BDA");
            assert!(addr < layout::HIGH_RAM_START, "must stay in low RAM");
            assert!(addr < layout::MPTABLE_START);
            assert!(addr < layout::EBDA_START);
        }
    }
}
