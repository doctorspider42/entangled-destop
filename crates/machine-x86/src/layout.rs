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

/// ACPI tables (RSDP, XSDT, FADT, FACS, MADT, DSDT), packed from this address
/// upwards. Deliberately inside the classic BIOS ROM window, immediately below
/// the MP table:
///
/// * the whole 0x9fc00..0x100000 range is already reserved in our E820 map, so
///   no guest allocator can land on it (the region itself is additionally
///   published as ACPI-reclaimable — see [`crate::E820Type::AcpiReclaim`]);
/// * `0xe0000..0xfffff` is the legacy RSDP scan window, so a guest or firmware
///   that ignores the hand-off pointer still finds the tables;
/// * EDK2 marks `0xa0000..0xfffff` as MMIO rather than system memory
///   (`PlatformAddIoMemoryRangeHob`), so DXE never allocates over them.
///
/// Both boot paths also hand the RSDP address over explicitly:
/// `boot_params.acpi_rsdp_addr` (direct Linux) and `hvm_start_info.rsdp_paddr`
/// (PVH/UEFI, ADR-0003).
pub const ACPI_TABLES_START: u64 = 0x000e_0000;

/// Size of the ACPI region: everything from [`ACPI_TABLES_START`] up to the MP
/// table. 64 KiB is ~4x what the largest table set (254 vCPUs) needs.
pub const ACPI_TABLES_SIZE: u64 = MPTABLE_START - ACPI_TABLES_START;

/// Guest physical address of the RSDP — the one ACPI address the boot paths
/// need to know, since every other table hangs off the XSDT.
pub const ACPI_RSDP_START: u64 = ACPI_TABLES_START;

// ---- ACPI PM register block (port I/O) -----------------------------------
//
// One 16-byte block of legacy-ACPI fixed-feature registers, described by the
// FADT and implemented by `crate::acpi::pm::AcpiPmBlock`. Two addresses in it
// are not ours to choose: EDK2's CloudHv platform hard-codes the sleep control
// register at 0x600 (`CLOUDHV_ACPI_SHUTDOWN_IO_ADDRESS`) and the PM timer at
// 0x608 (`CLOUDHV_ACPI_TIMER_IO_ADDRESS`); everything else is packed around
// them.
//
// | Port          | Width | Register              | FADT field                |
// |---------------|-------|-----------------------|---------------------------|
// | 0x600         | 1     | SLEEP_CONTROL         | `SLEEP_CONTROL_REG`       |
// | 0x601         | 1     | SLEEP_STATUS          | `SLEEP_STATUS_REG`        |
// | 0x602..0x603  | 2     | PM1a_STS              | `PM1a_EVT_BLK` (+0)       |
// | 0x604..0x605  | 2     | PM1a_EN               | `PM1a_EVT_BLK` (+2)       |
// | 0x606..0x607  | 2     | PM1a_CNT              | `PM1a_CNT_BLK`            |
// | 0x608..0x60b  | 4     | PM timer (24-bit)     | `PM_TMR_BLK`              |
// | 0x60c..0x60f  | 4     | GPE0_STS + GPE0_EN    | `GPE0_BLK`                |

/// Base of the ACPI PM register block. Pinned by EDK2: for a CloudHv host
/// bridge `ResetShutdown()` writes `SLP_TYP=5 | SLP_EN` to exactly this port.
pub const ACPI_PM_BASE: u16 = 0x0600;

/// Size of the ACPI PM register block.
pub const ACPI_PM_SIZE: u16 = 0x10;

/// GSI reserved for the ACPI System Control Interrupt (FADT `SCI_INT`).
///
/// Not 9, the PC convention: GSIs 5..=12 belong to the virtio-mmio slots
/// (`VIRTIO_MMIO_FIRST_IRQ` + `MAX_VIRTIO_SLOTS`), and sharing a level-triggered
/// SCI with an edge-triggered virtio line would be a real bug. 13 is the old
/// coprocessor-error line, which this machine has no use for.
pub const ACPI_SCI_GSI: u32 = 13;

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
/// until high-RAM support lands; virtio-mmio windows and the future PCI hole
/// live here.
pub const MMIO_HOLE_START: u64 = 0xc000_0000;

/// The 32-bit MMIO aperture behind the PCI host bridge, as published in the
/// DSDT's `\_SB.PCI0._CRS` (`crate::acpi`).
///
/// Deliberately stops below [`VIRTIO_MMIO_BASE`]: a PCI root bridge window that
/// swallowed the virtio-mmio slots would make Linux refuse the platform
/// devices' `request_mem_region`. The PCI bus itself (EPIC 19) is being built
/// in parallel — these two constants are the coordination point; move the
/// window, not the DSDT.
pub const PCI_MMIO_HOLE_BASE: u64 = MMIO_HOLE_START;

/// Size of [`PCI_MMIO_HOLE_BASE`]: 256 MiB, ending one byte below
/// [`VIRTIO_MMIO_BASE`].
pub const PCI_MMIO_HOLE_SIZE: u64 = VIRTIO_MMIO_BASE - PCI_MMIO_HOLE_BASE;

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
