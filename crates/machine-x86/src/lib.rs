//! x86-64 machine model: guest physical memory layout, E820 map and (soon)
//! vCPU register/CPUID/GDT setup (backlog EPIC 1/2).

pub mod acpi;
pub mod boot;
pub mod bus;
pub mod irqchip;
pub mod layout;
pub mod mptable;
pub mod pci;
pub mod pflash;
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

/// KVM-specific device plumbing: irqfd interrupt lines and the ioeventfd
/// queue-notify offload.
#[cfg(target_os = "linux")]
pub mod irqfd;
#[cfg(target_os = "linux")]
pub mod notify;

/// MSI delivery — **portable** since EPIC 17 phase 4: the architectural
/// address/data decode and the [`msi::UserspaceMsiSink`] built on it are pure
/// machine code; only [`msi::KvmMsiSink`] (`KVM_SIGNAL_MSI`) stays Linux-gated
/// inside the module.
pub mod msi;

/// The PCI bus carrying virtio functions — **portable** since EPIC 17 phase 4,
/// by the same split [`virtio`] got in phase 3: `attach` keeps the KVM wiring
/// (irqfds, `KVM_SIGNAL_MSI`, ioeventfds), `attach_userspace` wires the same
/// functions through [`irqchip::UserspaceIrqChip`] and [`msi::UserspaceMsiSink`]
/// with synchronous queue kicks.
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
/// reserved hole between 640 KiB and 1 MiB (VGA/BIOS shadow), usable RAM from
/// 1 MiB up to at most the 32-bit MMIO hole ([`layout::MMIO_HOLE_START`]) —
/// and, for guests bigger than that, the remainder as high RAM starting at
/// 4 GiB ([`layout::TOP_OF_32BIT`]). The hole itself is never RAM: the PCI
/// aperture, the virtio-mmio window, LAPIC/IOAPIC and the pflash window live
/// there, and `vmm_core::create_guest_memory` allocates the same two-region
/// shape (the cross-check test is below).
///
/// The reserved hole is split so the ACPI tables
/// ([`layout::ACPI_TABLES_START`]) get their own ACPI-reclaimable entry. They
/// would already be protected by the surrounding reserved range; the separate
/// entry is what tells a guest OS *why* the range is special.
pub fn e820_map(mem_size: u64) -> Vec<E820Entry> {
    let acpi_end = layout::ACPI_TABLES_START + layout::ACPI_TABLES_SIZE;
    let mut map = vec![
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
            size: mem_size.min(layout::MMIO_HOLE_START) - layout::HIGH_RAM_START,
            kind: E820Type::Ram,
        },
    ];
    if mem_size > layout::MMIO_HOLE_START {
        map.push(E820Entry {
            addr: layout::TOP_OF_32BIT,
            size: mem_size - layout::MMIO_HOLE_START,
            kind: E820Type::Ram,
        });
    }
    map
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

    /// A guest bigger than the hole splits: low RAM stops exactly at the hole,
    /// high RAM starts exactly at 4 GiB, nothing is described in between, and
    /// the total mapped bytes still equal the requested size.
    #[test]
    fn big_guests_get_a_high_ram_entry_above_4_gib() {
        let mem = 6 * 1024 * 1024 * 1024u64; // 6 GiB
        let map = e820_map(mem);
        let mut cursor = 0u64;
        for e in &map {
            assert!(e.addr >= cursor, "overlap at {:#x}", e.addr);
            // The only permitted gap is the MMIO hole itself.
            if e.addr != cursor {
                assert_eq!(cursor, layout::MMIO_HOLE_START, "gap below the hole");
                assert_eq!(e.addr, layout::TOP_OF_32BIT, "high RAM must start at 4 GiB");
            }
            cursor = e.addr + e.size;
        }
        assert_eq!(
            map.iter().map(|e| e.size).sum::<u64>(),
            mem,
            "every requested byte must be described"
        );
        let high = map.last().expect("entries");
        assert_eq!(high.kind, E820Type::Ram);
        assert_eq!(high.addr, layout::TOP_OF_32BIT);
        assert_eq!(high.size, mem - layout::MMIO_HOLE_START);
        // And nothing — RAM or otherwise — is described inside the hole.
        for e in &map {
            let end = e.addr + e.size;
            assert!(
                end <= layout::MMIO_HOLE_START || e.addr >= layout::TOP_OF_32BIT,
                "{:#x}..{end:#x} intrudes into the MMIO hole",
                e.addr
            );
        }
    }

    /// A guest exactly at the hole stays below it — no empty high entry.
    #[test]
    fn a_guest_exactly_at_the_hole_has_no_high_entry() {
        let map = e820_map(layout::MMIO_HOLE_START);
        assert!(map
            .iter()
            .all(|e| e.addr + e.size <= layout::MMIO_HOLE_START));
        assert_eq!(
            map.iter().map(|e| e.size).sum::<u64>(),
            layout::MMIO_HOLE_START
        );
    }

    /// vmm-core allocates guest memory in the same two-region shape this map
    /// describes, from its own copies of the two boundary constants (it must
    /// not depend on this crate). This is the test that keeps them equal.
    #[test]
    fn layout_agrees_with_vmm_core_memory_split() {
        assert_eq!(vmm_core::LOW_RAM_END, layout::MMIO_HOLE_START);
        assert_eq!(vmm_core::HIGH_RAM_START, layout::TOP_OF_32BIT);
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
