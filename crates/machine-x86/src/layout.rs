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
/// Not 9, the PC convention: 9 is one of the pins [`VIRTIO_IRQS`] hands to
/// devices, and sharing a level-triggered SCI with an edge-triggered virtio line
/// would be a real bug. 13 is the old coprocessor-error line, which this machine
/// has no use for.
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
// The host assigns each device an *initial* BAR from a fixed window at the
// bottom of the MMIO hole, one 16 KiB slot per device:
//
//   0xc000_0000 .. 0xc002_0000   initial BAR assignment (8 slots × 16 KiB)
//   0xc002_0000 .. 0xd000_0000   room for the guest to re-assign into
//   0xd000_0000 .. 0xd000_8000   virtio-mmio slots (8 × 4 KiB)
//
// The two transports never overlap even though only one is ever active for a
// given VM.
//
// **The initial assignment is a starting point, not the truth.** It was written
// as one when Linux was the only consumer — Linux claims a BAR it finds already
// programmed and leaves it there. EDK2 does not: `PciBusDxe` runs a full
// resource allocation and reassigns every BAR (measured on this machine: it
// hands out these very slots, in reverse device order). So the decode window
// below has to cover everywhere the guest may legitimately put a BAR, and
// anything the host wired to a BAR-relative address has to follow it
// (`crate::notify::DeviceNotifier::rebase`).

/// Base of the host's initial PCI BAR assignment, and of the decoded aperture.
pub const PCI_MMIO_BASE: u64 = MMIO_HOLE_START;

/// Size of one device's BAR window. Must equal
/// `virtio_core::pci::VIRTIO_PCI_BAR_SIZE`; asserted in
/// `crate::virtio_pci::tests::the_bar_slot_size_matches_the_transport`.
///
/// A BAR must be naturally aligned to its own size, which 16 KiB slots starting
/// at a 16 KiB-aligned base are.
pub const PCI_MMIO_SLOT_SIZE: u64 = 0x4000;

/// How many PCI devices the initial assignment covers. Matches
/// `crate::pci::MAX_PCI_DEVICES`.
pub const PCI_MMIO_SLOTS: u64 = 8;

/// One past the end of the *initial* BAR assignment — 8 slots, 128 KiB.
///
/// Not the same thing as [`PCI_MMIO_END`]: this is where the host's own slots
/// stop, and it is what `pci_bar_slot` is bounded by.
pub const PCI_MMIO_SLOTS_END: u64 = PCI_MMIO_BASE + PCI_MMIO_SLOTS * PCI_MMIO_SLOT_SIZE;

/// One past the end of the **decoded** PCI BAR aperture: a BAR anywhere in here
/// is honoured, a BAR outside it decodes nothing.
///
/// This is the same window the DSDT publishes to the guest in
/// `\_SB.PCI0._CRS` ([`PCI_MMIO_HOLE_BASE`] + [`PCI_MMIO_HOLE_SIZE`]), which is
/// the only aperture we have told anyone about, and it is a subset of the
/// `0xc000_0000 + 0x3800_0000` EDK2's CloudHv platform hard-codes. So a guest
/// that re-allocates resources — every UEFI firmware does — has 256 MiB of room
/// and cannot land somewhere the host then silently drops.
///
/// The bound is what keeps a *malicious* guest honest: a BAR parked over guest
/// RAM, over the LAPIC/IOAPIC, or over the virtio-mmio window is refused rather
/// than shadowing them, because [`crate::pci::PciRoot::locate_mmio`] checks this
/// range before it looks at any BAR.
pub const PCI_MMIO_END: u64 = PCI_MMIO_HOLE_BASE + PCI_MMIO_HOLE_SIZE;

/// First IRQ (GSI on the in-kernel IOAPIC) handed to a PCI device's INTx line.
///
/// Deliberately the same pins as the virtio-mmio slots use ([`VIRTIO_IRQS`]): a
/// VM runs one virtio transport, never both, so the pins cannot collide. Sharing
/// the list also means the MP table's ISA IRQ routing (`crate::mptable`) already
/// covers the PCI devices — without a `_PRT` or a `$PIR` table, Linux takes a PCI
/// device's IRQ straight from its `interrupt_line` config register, so the pin it
/// ends up requesting has to be one the MP table routes to the IOAPIC.
pub const PCI_FIRST_IRQ: u32 = VIRTIO_IRQS[0];

/// Guest physical base address of the BAR window for PCI slot `n`.
pub const fn pci_bar_slot(n: u64) -> u64 {
    PCI_MMIO_BASE + n * PCI_MMIO_SLOT_SIZE
}

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

/// First IRQ number handed to virtio devices (GSI on the in-kernel IOAPIC).
/// Legacy devices (serial) use the classic ISA IRQs below this.
///
/// Kept as its own name because it is what a single-device VM gets, which is
/// what most tests assert; the full assignment is [`VIRTIO_IRQS`].
pub const VIRTIO_MMIO_FIRST_IRQ: u32 = VIRTIO_IRQS[0];

/// IOAPIC pins handed to virtio devices, in slot order — **not** a contiguous
/// range, on purpose.
///
/// Both transports take a device's line from here. Assignment used to be
/// `first + slot`, i.e. 5..=12, which quietly collided with two pins this
/// machine's *own* legacy devices already own:
///
/// * **8 — the MC146818 RTC** (`crate::rtc`). Linux registers `rtc_cmos` on IRQ 8
///   and does not share it, so a virtio device on pin 8 gets `-EBUSY` out of
///   `request_irq` (both transports ask for `IRQF_SHARED`) and its probe ends in
///   `VIRTIO_CONFIG_S_FAILED`;
/// * **13 — the ACPI SCI** ([`ACPI_SCI_GSI`]), level-triggered where a virtio line
///   is an edge.
///
/// This was measured, not reasoned about. Booting the Ubuntu installer with five
/// devices left exactly one dead — the keyboard, on pin 8. Adding a third disk to
/// shift every later device up one slot moved the failure to the *GPU*, which had
/// inherited pin 8, and let both input devices bind: the fault followed the pin,
/// not the device.
///
/// 6 (floppy), 7 (LPT), 12 (PS/2 aux) and 14 (IDE) are safe here because this
/// machine emulates none of those controllers, and the FADT's `IAPC_BOOT_ARCH`
/// already tells the guest so (`LEGACY_DEVICES` and `8042` both clear). 4 is the
/// UART, 0/1/2 are the timer, keyboard and cascade.
///
/// Eight entries, matching `virtio::MAX_VIRTIO_SLOTS` and [`PCI_MMIO_SLOTS`].
pub const VIRTIO_IRQS: [u32; PCI_MMIO_SLOTS as usize] = [5, 6, 7, 9, 10, 11, 12, 14];

/// The IOAPIC pin for virtio slot `n`, or `None` when there is no such slot.
pub const fn virtio_irq(n: usize) -> Option<u32> {
    if n < VIRTIO_IRQS.len() {
        Some(VIRTIO_IRQS[n])
    } else {
        None
    }
}

/// Returns the guest physical base address of virtio-mmio slot `n`.
pub const fn virtio_mmio_slot(n: u64) -> u64 {
    VIRTIO_MMIO_BASE + n * VIRTIO_MMIO_SLOT_SIZE
}
