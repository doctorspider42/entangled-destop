//! Guest physical memory layout for the Entangled Desktop x86-64 machine.
//!
//! Addresses follow the Linux x86 boot protocol conventions used by other
//! rust-vmm based VMMs so a stock `bzImage` boots without firmware.

/// Start of the Extended BIOS Data Area; usable low RAM ends here.
pub const EBDA_START: u64 = 0x0009_fc00;

/// Boot GDT written by the host before starting vCPU0 in long mode.
pub const BOOT_GDT_START: u64 = 0x0000_0500;

/// Boot IDT (a single zeroed entry — interrupts are off until the kernel
/// installs its own).
pub const BOOT_IDT_START: u64 = 0x0000_0520;

/// Initial stack pointer for the 64-bit kernel entry.
pub const BOOT_STACK_POINTER: u64 = 0x0000_8ff0;

/// Identity-map page tables built by the host: PML4, one PDPT and four page
/// directories (2 MiB pages covering the first 4 GiB).
pub const PML4_START: u64 = 0x0000_9000;
pub const PDPTE_START: u64 = 0x0000_a000;
pub const PD_START: u64 = 0x0000_b000;

/// Intel MP floating pointer + configuration table, in the classic BIOS scan
/// window (Linux probes 0xF0000..0xFFFFF for "_MP_"). Gives the guest a real
/// interrupt topology so device IRQs route through the IOAPIC instead of the
/// 8259 virtual-wire fallback, where irqfd edge injections are intermittently
/// lost.
pub const MPTABLE_START: u64 = 0x000f_0000;

/// Physical address of the local APIC (architectural default).
pub const LAPIC_ADDR: u32 = 0xfee0_0000;

/// Physical address of the IOAPIC (architectural default; KVM's in-kernel
/// IOAPIC lives here).
pub const IOAPIC_ADDR: u32 = 0xfec0_0000;

/// Conventional "high memory" start (1 MiB); the kernel is loaded above this.
pub const HIGH_RAM_START: u64 = 0x0010_0000;

/// Guest physical address where `boot_params` (the "zero page") is written.
pub const ZERO_PAGE_START: u64 = 0x0000_7000;

/// Guest physical address of the kernel command line.
pub const CMDLINE_START: u64 = 0x0002_0000;

/// Maximum command line length we allow (boot protocol supports more, but
/// this is plenty and keeps the layout simple).
pub const CMDLINE_MAX_LEN: usize = 2048;

// ---- UEFI boot (EPIC 18, ADR-0003) ---------------------------------------

/// Guest physical address of the PVH `hvm_start_info` structure handed to a
/// firmware in `%ebx`. Lives in low RAM that neither the firmware image
/// (loaded at 1 MiB and up) nor the SEC/PEI temporary RAM (inside the
/// firmware's own MEMFD, at 8 MiB and up) touches.
pub const PVH_START_INFO_START: u64 = 0x0000_1000;

/// Guest physical address of the `hvm_memmap_table_entry` array that
/// `hvm_start_info.memmap_paddr` points at.
pub const PVH_MEMMAP_START: u64 = 0x0000_2000;

/// Guest physical address of the NUL-terminated PVH command line.
pub const PVH_CMDLINE_START: u64 = 0x0000_3000;

/// Cap on the PVH memory map, so the array cannot run out of its page.
/// One `hvm_memmap_table_entry` is 24 bytes: 128 entries is 3 KiB.
pub const PVH_MEMMAP_MAX_ENTRIES: usize = 128;

/// One past the end of the 32-bit physical address space. A reset-vector
/// firmware ROM is placed so that its last byte is at `TOP_OF_32BIT - 1`,
/// which puts the architectural reset vector (`0xffff_fff0`) inside it.
pub const TOP_OF_32BIT: u64 = 0x1_0000_0000;

/// Where a reset-mode vCPU takes its first instruction fetch: `CS.base`
/// `0xffff_0000` + `IP` `0xfff0`. Firmware ROM placement must cover it.
pub const RESET_VECTOR: u64 = 0xffff_fff0;

/// Start of the 32-bit MMIO hole. RAM must not be mapped at or above this
/// until high-RAM support lands; the PCI BAR aperture and the virtio-mmio
/// windows live here.
///
/// The value is also pinned from the outside: EDK2's CloudHv platform
/// hard-codes its 32-bit aperture as `0xc000_0000 + 0x3800_0000`
/// (ADR-0003, "MMIO hole agreement"), so everything below must stay inside
/// `0xc000_0000..0xf800_0000`.
pub const MMIO_HOLE_START: u64 = 0xc000_0000;

// ---- PCI BAR aperture (EPIC 19, virtio-pci) -------------------------------
//
// The host assigns BARs itself, from a fixed window at the bottom of the MMIO
// hole. Fixed rather than dynamically allocated because there is no ACPI `_CRS`
// and no MCFG to publish a movable aperture through: a guest that re-assigns
// these BARs would have nothing to re-assign them *from*, so both Linux (which
// keeps a BIOS-assigned BAR it can claim) and EDK2 (whose `PciBusDxe` finds the
// BARs already programmed) simply use what they find.
//
//   0xc000_0000 .. 0xc002_0000   PCI BAR aperture (8 slots × 16 KiB)
//   0xc002_0000 .. 0xd000_0000   free
//   0xd000_0000 .. 0xd000_8000   virtio-mmio slots (8 × 4 KiB)
//
// The two transports therefore never overlap even though only one is ever
// active for a given VM.

/// Base of the host-assigned PCI BAR aperture.
pub const PCI_MMIO_BASE: u64 = MMIO_HOLE_START;

/// Size of one device's BAR window. Must equal
/// `virtio_core::pci::VIRTIO_PCI_BAR_SIZE`; asserted in
/// `crate::virtio_pci::tests::the_bar_slot_size_matches_the_transport`.
///
/// A BAR must be naturally aligned to its own size, which 16 KiB slots starting
/// at a 16 KiB-aligned base are.
pub const PCI_MMIO_SLOT_SIZE: u64 = 0x4000;

/// How many PCI devices the aperture holds. Matches
/// `crate::pci::MAX_PCI_DEVICES`.
pub const PCI_MMIO_SLOTS: u64 = 8;

/// One past the end of the PCI BAR aperture.
pub const PCI_MMIO_END: u64 = PCI_MMIO_BASE + PCI_MMIO_SLOTS * PCI_MMIO_SLOT_SIZE;

/// First IRQ (GSI on the in-kernel IOAPIC) handed to a PCI device's INTx line.
///
/// Deliberately the same pins as [`VIRTIO_MMIO_FIRST_IRQ`]: a VM runs one virtio
/// transport, never both, so the pins cannot collide. Keeping them identical
/// also means the MP table's ISA IRQ routing (`crate::mptable`) already covers
/// the PCI devices — without ACPI or a `$PIR` table, Linux takes a PCI device's
/// IRQ straight from its `interrupt_line` config register, so the pin it ends up
/// requesting has to be one the MP table routes to the IOAPIC.
pub const PCI_FIRST_IRQ: u32 = VIRTIO_MMIO_FIRST_IRQ;

/// Guest physical base address of the BAR window for PCI slot `n`.
pub const fn pci_bar_slot(n: u64) -> u64 {
    PCI_MMIO_BASE + n * PCI_MMIO_SLOT_SIZE
}

/// Base of the virtio-mmio device window region.
pub const VIRTIO_MMIO_BASE: u64 = 0xd000_0000;

/// Size of each virtio-mmio device slot (one 4 KiB page).
pub const VIRTIO_MMIO_SLOT_SIZE: u64 = 0x1000;

/// First IRQ number handed to virtio-mmio devices (GSI on the in-kernel
/// IOAPIC). Legacy devices (serial) use the classic ISA IRQs below this.
pub const VIRTIO_MMIO_FIRST_IRQ: u32 = 5;

/// Returns the guest physical base address of virtio-mmio slot `n`.
pub const fn virtio_mmio_slot(n: u64) -> u64 {
    VIRTIO_MMIO_BASE + n * VIRTIO_MMIO_SLOT_SIZE
}
