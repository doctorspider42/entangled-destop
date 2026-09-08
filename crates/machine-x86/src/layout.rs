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

/// Base of the PVH hand-off block: three pages holding `hvm_start_info`, the
/// `hvm_memmap_table_entry` array it points at, and the command line.
///
/// **These pages must never be describable as usable RAM.** The firmware does
/// not copy `hvm_start_info` out at entry: EDK2's reset vector stashes the
/// `%ebx` pointer and `AcpiPlatformDxe` dereferences it again *at the end of
/// DXE*, once the PCI bus has been enumerated — `InstallCloudHvTables()` reads
/// `pvh_start_info->rsdp_paddr` there and walks straight into the XSDT. So the
/// structure has to survive the whole of PEI and DXE, and anything that
/// scribbles the page turns that read into a wild pointer. A garbage
/// `rsdp_paddr` is not even a page fault: a non-canonical one faults as
/// `#GP` inside `QemuFwCfgAcpiPlatform.dll`, which is what an ADR-0003 boot
/// looks like when this goes wrong.
///
/// They used to live at 0x1000..0x4000, inside the *usable* low-RAM E820 entry
/// that starts at zero — host structures published to the guest as free memory.
/// Here they sit in the reserved BIOS window instead, immediately below the
/// ACPI tables ([`ACPI_TABLES_START`]), which buys the same two protections the
/// tables get: the whole `0x9fc00..0x100000` range is `E820Type::Reserved`, and
/// EDK2 additionally maps `0xa0000..0xfffff` as MMIO rather than system memory
/// (`PlatformAddIoMemoryRangeHob`), so no DXE allocation can be handed a page
/// of it.
pub const PVH_HANDOFF_START: u64 = 0x000d_0000;

/// Size of the PVH hand-off block: one page each for the start info, the
/// memory map ([`PVH_MEMMAP_MAX_ENTRIES`] × 24 bytes = 3 KiB) and the command
/// line.
pub const PVH_HANDOFF_SIZE: u64 = 0x3000;

/// Guest physical address of the PVH `hvm_start_info` structure handed to a
/// firmware in `%ebx`.
pub const PVH_START_INFO_START: u64 = PVH_HANDOFF_START;

/// Guest physical address of the `hvm_memmap_table_entry` array that
/// `hvm_start_info.memmap_paddr` points at.
pub const PVH_MEMMAP_START: u64 = PVH_HANDOFF_START + 0x1000;

/// Guest physical address of the NUL-terminated PVH command line.
pub const PVH_CMDLINE_START: u64 = PVH_HANDOFF_START + 0x2000;

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

// ---- pflash / NVRAM (UEFI-1804) -------------------------------------------
//
// The window of the emulated CFI flash device (`crate::pflash`) that backs the
// UEFI non-volatile variable store. **These numbers are a contract with the
// firmware build**: `guest/firmware/build-cloudhv.sh` overrides
// `PcdOvmfFdBaseAddress` and `PcdOvmfFlashNvStorageVariableBase` to
// [`PFLASH_BASE`], and if the two disagree the firmware probes an address
// nothing decodes, concludes "FD behaves as RAM", and quietly goes back to
// RAM-only variables — a failure whose only symptom is an installed guest that
// stops booting after its second restart.
//
//   0xffc0_0000 .. 0xffc4_0000   variable store   (PcdFlashNvStorageVariableSize)
//   0xffc4_0000 .. 0xffc4_1000   event log
//   0xffc4_1000 .. 0xffc4_2000   fault-tolerant-write working block
//   0xffc4_2000 .. 0xffc8_4000   fault-tolerant-write spare blocks
//   0xffc8_4000 .. 0x1_0000_0000 decoded, unbacked: reads as erased flash
//
// Why here: the same address `OvmfPkg/OvmfPkgX64` uses for its 4 MiB flash, so
// the window ends exactly at 4 GiB, and it is clear of the IOAPIC
// ([`IOAPIC_ADDR`]), the LAPIC ([`LAPIC_ADDR`]) and the `0xc000_0000 +
// 0x3800_0000` MMIO hole EDK2's CloudHv platform hard-codes.
//
// Note the deliberate overlap with the *reset-vector* ROM placement: a 4 MiB
// flash image mapped by `uefi_boot::rom::place_at_top_of_32bit` lands here too.
// The two modes are mutually exclusive — a reset-vector firmware image *is* its
// own flash, PVH firmware is loaded into RAM and needs this device — and
// `apps/entangled` refuses the combination rather than mapping both.

/// Guest physical base of the pflash window.
pub const PFLASH_BASE: u64 = 0xffc0_0000;

/// Size of the decoded pflash window: `PcdOvmfFirmwareFdSize`, 4 MiB, ending at
/// 4 GiB. The firmware adds exactly this range to the GCD as runtime MMIO, so
/// the device answers reads across all of it.
pub const PFLASH_WINDOW_SIZE: u64 = 0x0040_0000;

/// Live variable store size (`PcdFlashNvStorageVariableSize`, `VARS_LIVE_SIZE`).
pub const PFLASH_VARSTORE_SIZE: u64 = 0x0004_0000;

/// Event log block (`PcdOvmfFlashNvStorageEventLogSize`).
pub const PFLASH_EVENT_LOG_SIZE: u64 = 0x0000_1000;

/// Fault-tolerant-write working block (`PcdFlashNvStorageFtwWorkingSize`).
pub const PFLASH_FTW_WORKING_SIZE: u64 = 0x0000_1000;

/// Fault-tolerant-write spare blocks (`PcdFlashNvStorageFtwSpareSize`,
/// `VARS_SPARE_SIZE`).
pub const PFLASH_FTW_SPARE_SIZE: u64 = 0x0004_2000;

/// Bytes actually persisted per VM: `VARS_SIZE` from `CloudHvDefines.fdf.inc`,
/// i.e. the four regions above. This is the size of the NVRAM file.
pub const PFLASH_NVRAM_SIZE: u64 =
    PFLASH_VARSTORE_SIZE + PFLASH_EVENT_LOG_SIZE + PFLASH_FTW_WORKING_SIZE + PFLASH_FTW_SPARE_SIZE;

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
// bottom of the MMIO hole, one 32 KiB slot per device:
//
//   0xc000_0000 .. 0xc004_0000   initial BAR assignment (8 slots × 32 KiB)
//   0xc004_0000 .. 0xd000_0000   room for the guest to re-assign into
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
/// A BAR must be naturally aligned to its own size, which 32 KiB slots starting
/// at a 32 KiB-aligned base are.
///
/// 32 KiB and not 16 KiB since MSI-X: the table and the PBA take a page each at
/// the top of the same BAR (`virtio_core::pci`), and the BAR sizing protocol can
/// only express a power of two.
pub const PCI_MMIO_SLOT_SIZE: u64 = 0x8000;

/// How many PCI devices the initial assignment covers. Matches
/// `crate::pci::MAX_PCI_DEVICES`.
pub const PCI_MMIO_SLOTS: u64 = 8;

/// One past the end of the *initial* BAR assignment — 8 slots, 256 KiB.
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

// ---- the 64-bit MMIO aperture (EPIC 20, VEN-2001) --------------------------
//
// Everything above lives below 4 GiB, because everything above is a register
// file. A virtio **shared-memory region** is not: it is hundreds of megabytes
// of host RAM the guest maps directly, and it wants a 64-bit prefetchable BAR
// of its own ([`virtio_core::pci::VIRTIO_PCI_SHM_BAR_INDEX`]). That needs an
// aperture nothing else claims, and picking one is not a matter of taste —
// four things are already up there, and a fifth is chosen by the firmware:
//
//   * **high RAM.** A guest bigger than the 32-bit hole gets its remainder at
//     4 GiB, and that region *grows with the guest's memory*
//     (`vmm_core::create_guest_memory`). A constant above 4 GiB is a constant
//     that works until someone boots a bigger VM.
//   * **the pflash window**, `0xffc0_0000..0x1_0000_0000` — below 4 GiB, so it
//     bounds this from underneath along with the whole 32-bit MMIO hole,
//     the LAPIC and the IOAPIC.
//   * **`Pci64Base`, which EDK2 chooses for itself.** Measured on this
//     project's pinned CloudHv build with a 4096 MiB guest:
//     `PlatformGetFirstNonAddressCB: FirstNonAddress=0x140000000` and
//     `AddressWidthInitialization: Pci64Base=0x140000000
//     Pci64Size=0x3FFEC0000000`. So the firmware's 64-bit aperture starts at
//     **exactly** the top of RAM — no page of slack, whatever the vm-testing
//     skill's prose says — and runs to 2^46, the guest's physical address
//     width. `PciBusDxe` then reassigns every BAR, and a 64-bit *prefetchable*
//     one lands in that aperture, aligned up from its base to the BAR's own
//     size.
//
// Hence the rule this machine follows: **our aperture is the firmware's.**
// [`pci_mmio64_base`] returns the same number EDK2 computes, so the host's
// initial assignment, the firmware's reassignment and the DSDT `_CRS` all
// describe one window, and a BAR that moves during enumeration moves *inside*
// a range the host already decodes. The alternative — a fixed high address —
// was rejected precisely because the firmware would move the BAR out of it:
// EDK2 allocates from `Pci64Base` upwards, and `Pci64Base` follows RAM.
//
// The aperture is deliberately larger than what it holds (4 GiB for one
// 256 MiB window today), for the same reason the 32-bit one is: it has to
// cover everywhere a firmware may legitimately re-align a BAR to.

/// Size of the 64-bit MMIO aperture: 4 GiB starting at [`pci_mmio64_base`].
///
/// Room for every [`PCI_MMIO_SLOTS`] function to hold a
/// [`MAX_SHM_BAR_BYTES`]-sized window and still leave slack for a firmware's
/// natural alignment. Nothing but shared-memory regions is allocated from it.
pub const PCI_MMIO64_SIZE: u64 = 4 << 30;

/// Largest single shared-memory BAR this machine will place: 1 GiB.
///
/// A BAR is naturally aligned to its own size, so this also bounds how far
/// into the aperture the first allocation can be pushed by alignment.
pub const MAX_SHM_BAR_BYTES: u64 = 1 << 30;

/// One past the last byte of guest RAM, for a guest of `mem_bytes`.
///
/// The same split `vmm_core::create_guest_memory` makes and `crate::e820_map`
/// publishes: low RAM up to [`MMIO_HOLE_START`], the remainder at 4 GiB. A
/// guest that fits below the hole still ends at 4 GiB as far as the address
/// space is concerned, because the hole is not RAM and nothing may be placed
/// inside it.
pub const fn top_of_ram(mem_bytes: u64) -> u64 {
    if mem_bytes > MMIO_HOLE_START {
        TOP_OF_32BIT + (mem_bytes - MMIO_HOLE_START)
    } else {
        TOP_OF_32BIT
    }
}

/// Base of the 64-bit MMIO aperture for a guest of `mem_bytes`: the top of its
/// RAM, which is exactly what EDK2 publishes as `Pci64Base`.
///
/// Page aligned by construction — `mem_bytes` is a whole number of MiB and
/// both boundaries it is measured against are 1 MiB aligned.
pub const fn pci_mmio64_base(mem_bytes: u64) -> u64 {
    top_of_ram(mem_bytes)
}

/// One past the end of the 64-bit MMIO aperture for a guest of `mem_bytes`.
pub const fn pci_mmio64_end(mem_bytes: u64) -> u64 {
    pci_mmio64_base(mem_bytes) + PCI_MMIO64_SIZE
}

/// Bump allocator for naturally aligned windows inside the 64-bit aperture.
///
/// Not a fixed slot table like [`pci_bar_slot`]: a shared-memory BAR's size is
/// the device's business (it is the renderer's window length rounded up to a
/// power of two), and a PCI memory BAR must be aligned to *its own* size. A
/// table of equal slots would either waste the aperture or misalign a big
/// window; a bump allocator does neither, and it is three lines.
#[derive(Debug, Clone, Copy)]
pub struct Mmio64Allocator {
    next: u64,
    end: u64,
}

impl Mmio64Allocator {
    /// An allocator over the whole aperture of a guest with `mem_bytes` of RAM.
    pub const fn for_guest(mem_bytes: u64) -> Self {
        Self {
            next: pci_mmio64_base(mem_bytes),
            end: pci_mmio64_end(mem_bytes),
        }
    }

    /// Reserves `size` bytes, aligned to `size`. `None` when the aperture is
    /// full or `size` is not a usable BAR size — both host configuration
    /// errors, never anything a guest can provoke.
    pub fn allocate(&mut self, size: u64) -> Option<u64> {
        if size == 0 || !size.is_power_of_two() || size > MAX_SHM_BAR_BYTES {
            return None;
        }
        let base = self.next.checked_add(size - 1)? / size * size;
        let end = base.checked_add(size)?;
        if end > self.end {
            return None;
        }
        self.next = end;
        Some(base)
    }
}

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

#[cfg(test)]
mod tests {
    use super::*;

    const MIB: u64 = 1 << 20;
    /// The largest guest `control_api::MAX_MEMORY_MIB` allows. Restated rather
    /// than imported: `machine-x86` does not depend on the config crate, and
    /// the point of the sweep below is that the aperture is correct for every
    /// size a user can ask for.
    const MAX_GUEST_MIB: u64 = 65536;

    /// The measurement this whole placement rests on
    /// (`tests/boot/tests/uefi_highmem.rs`, and the log line quoted above):
    /// EDK2's `Pci64Base` for a 4096 MiB guest is `0x1_4000_0000`.
    #[test]
    fn the_aperture_starts_where_edk2_puts_pci64base() {
        assert_eq!(pci_mmio64_base(4096 * MIB), 0x1_4000_0000);
        // A guest that fits below the hole has no high RAM at all, and both we
        // and the firmware call the top of its address space 4 GiB.
        assert_eq!(pci_mmio64_base(2048 * MIB), TOP_OF_32BIT);
        assert_eq!(pci_mmio64_base(MMIO_HOLE_START), TOP_OF_32BIT);
    }

    /// The trap this constant exists to avoid: the aperture must clear RAM,
    /// the 32-bit hole, pflash and the reset vector for **every** guest size,
    /// not just the one someone tested with.
    #[test]
    fn the_aperture_never_overlaps_ram_or_anything_below_4_gib() {
        for mib in [1u64, 512, 2048, 3072, 3073, 4096, 8192, 16384, MAX_GUEST_MIB] {
            let bytes = mib * MIB;
            let base = pci_mmio64_base(bytes);
            let top = top_of_ram(bytes);
            assert!(base >= top, "{mib} MiB: aperture {base:#x} overlaps RAM ending {top:#x}");
            assert!(base >= TOP_OF_32BIT, "{mib} MiB: aperture {base:#x} is below 4 GiB");
            assert!(base >= PFLASH_BASE + PFLASH_WINDOW_SIZE, "{mib} MiB: aperture hits pflash");
            assert!(base > u64::from(u32::MAX), "{mib} MiB: aperture is 32-bit addressable");
            assert_eq!(base % 0x1000, 0, "{mib} MiB: aperture base is not page aligned");
            // …and the whole aperture stays inside the 2^46 physical address
            // width the firmware reports (`Pci64Size=0x3FFEC0000000`).
            assert!(pci_mmio64_end(bytes) < 1u64 << 46, "{mib} MiB: aperture past 2^46");
        }
    }

    /// A BAR is only decoded where it is naturally aligned, and the allocator
    /// is the only thing that guarantees that for a window whose size is not
    /// the aperture's alignment.
    #[test]
    fn the_allocator_aligns_every_window_to_its_own_size() {
        // 3073 MiB puts the top of RAM at 4 GiB + 1 MiB, which is page aligned
        // and nothing else — exactly the case a fixed slot table gets wrong.
        let bytes = 3073 * MIB;
        assert_eq!(pci_mmio64_base(bytes), TOP_OF_32BIT + MIB);
        let mut alloc = Mmio64Allocator::for_guest(bytes);
        let first = alloc.allocate(256 << 20).expect("256 MiB window");
        assert_eq!(first % (256 << 20), 0, "{first:#x} is not 256 MiB aligned");
        assert!(first >= pci_mmio64_base(bytes));
        let second = alloc.allocate(1 << 30).expect("1 GiB window");
        assert_eq!(second % (1 << 30), 0);
        assert!(second >= first + (256 << 20), "windows must not overlap");
        assert!(second + (1 << 30) <= pci_mmio64_end(bytes));
    }

    #[test]
    fn the_allocator_refuses_bad_sizes_and_a_full_aperture() {
        let mut alloc = Mmio64Allocator::for_guest(2048 * MIB);
        assert!(alloc.allocate(0).is_none());
        assert!(alloc.allocate(3 << 20).is_none(), "not a power of two");
        assert!(alloc.allocate(MAX_SHM_BAR_BYTES * 2).is_none(), "past the cap");
        for _ in 0..4 {
            alloc.allocate(MAX_SHM_BAR_BYTES).expect("fits");
        }
        assert!(alloc.allocate(MAX_SHM_BAR_BYTES).is_none(), "aperture full");
    }
}
