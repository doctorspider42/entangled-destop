//! x86-64 machine model: guest physical memory layout, E820 map and (soon)
//! vCPU register/CPUID/GDT setup (backlog EPIC 1/2).

pub mod acpi;
pub mod boot;
pub mod bus;
pub mod irqchip;
pub mod layout;
pub mod mptable;
pub mod pci;
pub mod platform;
pub mod rtc;
pub mod serial;

/// The virtio-mmio window: address decoding, slot placement and the guest
/// cmdline clauses that announce it — **portable** since EPIC 17 phase 3.
///
/// A device's *wiring* used to be what pinned this to Linux: an irqfd and an
/// ioeventfd are KVM concepts. The interrupt half is solved by [`irqchip`], whose
/// `IoApicLine` is the same `virtio_core::interrupt::IrqLine` an irqfd is, so
/// [`virtio::VirtioMmioBus::attach_userspace`] attaches the same devices on a host
/// with no in-kernel irqchip. The kick half has no WHP equivalent yet, so those
/// machines run every queue notification inline on the vCPU thread (see
/// [`virtio::VirtioMmioBus::attach_userspace`] for the measured cost).
pub mod virtio;

/// KVM-specific device plumbing: irqfd interrupt lines, ioeventfd queue-notify
/// offload and the PCI bus built on them.
#[cfg(target_os = "linux")]
pub mod irqfd;
#[cfg(target_os = "linux")]
pub mod msi;
#[cfg(target_os = "linux")]
pub mod notify;
#[cfg(target_os = "linux")]
pub mod virtio_pci;

/// E820 memory range types as defined by the BIOS/ACPI interface and consumed
/// by the Linux boot protocol's `boot_params.e820_table`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum E820Type {
    Ram = 1,
    Reserved = 2,
    /// ACPI reclaimable: the ACPI tables live here. Linux keeps this out of
    /// memblock (`e820__memblock_setup` only adds RAM), so nothing allocates
    /// over the tables before ACPICA has copied them, and userspace can still
    /// see the range as reclaimable.
    AcpiReclaim = 3,
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
///
/// The reserved hole is split so the ACPI tables
/// ([`layout::ACPI_TABLES_START`]) get their own ACPI-reclaimable entry. They
/// would already be protected by the surrounding reserved range; the separate
/// entry is what tells a guest OS *why* the range is special.
pub fn e820_map(mem_size: u64) -> Vec<E820Entry> {
    assert!(
        mem_size <= layout::MMIO_HOLE_START,
        "guests larger than {} bytes need a high-RAM split (post-MVP)",
        layout::MMIO_HOLE_START
    );
    let acpi_end = layout::ACPI_TABLES_START + layout::ACPI_TABLES_SIZE;
    vec![
        E820Entry {
            addr: 0,
            size: layout::EBDA_START,
            kind: E820Type::Ram,
        },
        E820Entry {
            addr: layout::EBDA_START,
            size: layout::ACPI_TABLES_START - layout::EBDA_START,
            kind: E820Type::Reserved,
        },
        E820Entry {
            addr: layout::ACPI_TABLES_START,
            size: layout::ACPI_TABLES_SIZE,
            kind: E820Type::AcpiReclaim,
        },
        E820Entry {
            addr: acpi_end,
            size: layout::HIGH_RAM_START - acpi_end,
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

    /// The ACPI tables must be described as ACPI-reclaimable, not as RAM: on
    /// the reclaimable type Linux keeps the range out of memblock entirely.
    #[test]
    fn acpi_region_is_reclaimable_and_covers_the_tables() {
        let map = e820_map(512 * 1024 * 1024);
        let acpi = map
            .iter()
            .find(|e| e.kind == E820Type::AcpiReclaim)
            .expect("no ACPI entry in the E820 map");
        assert_eq!(acpi.addr, layout::ACPI_TABLES_START);
        assert_eq!(acpi.size, layout::ACPI_TABLES_SIZE);
        assert!(
            acpi.addr + acpi.size <= layout::MPTABLE_START,
            "the ACPI region must not swallow the MP table"
        );
        assert!(
            acpi.addr >= layout::EBDA_START && acpi.addr + acpi.size <= layout::HIGH_RAM_START,
            "the ACPI region must stay inside the low reserved hole"
        );
        // Exactly one RAM entry below the EBDA and one above 1 MiB; the ACPI
        // split must not have produced a RAM hole.
        assert_eq!(map.iter().filter(|e| e.kind == E820Type::Ram).count(), 2);
    }

    /// A DSDT `_CRS` that overlapped the virtio-mmio window would make Linux
    /// refuse the platform devices; the two windows are disjoint by definition.
    #[test]
    fn pci_hole_does_not_overlap_the_virtio_window() {
        const {
            assert!(
                layout::PCI_MMIO_HOLE_BASE + layout::PCI_MMIO_HOLE_SIZE <= layout::VIRTIO_MMIO_BASE
            );
            assert!(layout::PCI_MMIO_HOLE_BASE >= layout::MMIO_HOLE_START);
            assert!(layout::PCI_MMIO_HOLE_SIZE > 0);
        }
    }
}
