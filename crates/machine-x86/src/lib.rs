//! x86-64 machine model: guest physical memory layout, E820 map and (soon)
//! vCPU register/CPUID/GDT setup (backlog EPIC 1/2).

pub mod layout;

#[cfg(target_os = "linux")]
pub mod boot;

/// E820 memory range types as defined by the BIOS/ACPI interface and consumed
/// by the Linux boot protocol's `boot_params.e820_table`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum E820Type {
    Ram = 1,
    Reserved = 2,
}

/// One guest physical address range for the E820 map.
#[derive(Debug, Clone, Copy)]
pub struct E820Entry {
    pub addr: u64,
    pub size: u64,
    pub kind: E820Type,
}

/// Builds the guest E820 map for a VM with `mem_size` bytes of RAM.
///
/// Layout follows the PC convention: usable low memory below the EBDA, a
/// reserved hole between 640 KiB and 1 MiB (VGA/BIOS shadow), then usable RAM
/// from 1 MiB up to `mem_size`. RAM above 4 GiB (when `mem_size` crosses the
/// 32-bit MMIO hole) is not implemented yet — MVP guests fit below 3 GiB.
pub fn e820_map(mem_size: u64) -> Vec<E820Entry> {
    assert!(
        mem_size <= layout::MMIO_HOLE_START,
        "guests larger than {} bytes need a high-RAM split (post-MVP)",
        layout::MMIO_HOLE_START
    );
    vec![
        E820Entry {
            addr: 0,
            size: layout::EBDA_START,
            kind: E820Type::Ram,
        },
        E820Entry {
            addr: layout::EBDA_START,
            size: layout::HIGH_RAM_START - layout::EBDA_START,
            kind: E820Type::Reserved,
        },
        E820Entry {
            addr: layout::HIGH_RAM_START,
            size: mem_size - layout::HIGH_RAM_START,
            kind: E820Type::Ram,
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn e820_covers_memory_without_overlap() {
        let mem = 2 * 1024 * 1024 * 1024u64; // 2 GiB
        let map = e820_map(mem);
        let mut cursor = 0u64;
        for e in &map {
            assert_eq!(e.addr, cursor, "gap or overlap at {:#x}", e.addr);
            cursor += e.size;
        }
        assert_eq!(cursor, mem);
    }

    #[test]
    fn low_hole_is_reserved() {
        let map = e820_map(512 * 1024 * 1024);
        assert_eq!(map[1].kind, E820Type::Reserved);
        assert_eq!(map[1].addr, layout::EBDA_START);
    }
}
