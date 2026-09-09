---
name: acpi-machine
description: ACPI on the Entangled Desktop machine — the RSDP/XSDT/FADT/FACS/MADT/DSDT tables, the AML in the DSDT, the ACPI PM register block and guest power-off, how the MADT and the Intel MP table coexist, and how both boot paths hand the RSDP over. Load before touching machine_x86::acpi, the FADT/MADT/DSDT contents, guest shutdown, or anything that needs the guest to see a platform (Ubuntu, PCI, SMP).
---

# ACPI on this machine

Code: `crates/machine-x86/src/acpi/` (`mod.rs` tables, `aml.rs` AML encoder,
`pm.rs` the PM register block). Published by `machine_x86::acpi::write`, which
the machine calls once next to `machine_x86::mptable::write`. Boot-path
hand-off lives in `crates/linux-boot/src/load.rs` and
`crates/uefi-boot/src/load.rs`.

Related: [ADR-0003](../../../docs/adr/0003-uefi-firmware.md) (the UEFI
contract), `.claude/skills/vm-testing/SKILL.md` (how to run the boot tests).

## Table inventory

One contiguous blob at `layout::ACPI_TABLES_START` = `0x000e_0000`, 64 KiB
reserved (up to `MPTABLE_START`). Every table is 64-byte aligned. Sizes for
`vcpus = 2`:

| Table | Rev | Address | Size | Contents |
|---|---|---|---|---|
| RSDP | 2 | `0x000e_0000` | 36 | XSDT pointer; `RsdtAddress` 0; both checksums |
| XSDT | 1 | `0x000e_0040` | 52 | FADT, MADT — and nothing else |
| FADT | 6 | `0x000e_0080` | 276 | FACS + DSDT pointers, the PM register block, `SCI_INT` 13 |
| FACS | — | `0x000e_01c0` | 64 | version 2, no waking vector (no S3) |
| MADT | 5 | `0x000e_0200` | 104 | LAPIC per vCPU, IOAPIC, 2 source overrides, LAPIC NMI per vCPU |
| DSDT | 2 | `0x000e_0280` | 207 | `\_S5`, `\_SB.PCI0`, one `ACPI0007` per vCPU |

Total 847 bytes for 2 vCPUs; ~4.6 KiB at the 254-vCPU ceiling
(`acpi::MAX_ACPI_CPUS`, same as the MP table's). A unit test asserts the whole
set still fits the region at that ceiling.

Why `0xe0000`:

* the whole `0x9fc00..0x100000` hole is already reserved in the E820 map, so no
  guest allocator can land on it;
* it is inside the legacy RSDP scan window (`0xe0000..0xfffff`), so a guest or
  firmware that ignores the hand-off pointer still finds the tables;
* EDK2 marks `0xa0000..0xfffff` as MMIO rather than system memory
  (`PlatformAddIoMemoryRangeHob`), so DXE never allocates over them.

The region additionally gets its own `E820Type::AcpiReclaim` (type 3) entry —
the one type Linux keeps out of memblock entirely (`e820__memblock_setup` only
adds RAM), and an `XEN_HVM_MEMMAP_TYPE_ACPI` entry in the PVH memmap (which
EDK2 ignores: `PlatformScanE820Pvh()` filters on RAM).

### The PVH hand-off block shares the same window (ADR-0003, 2026-09-08)

`hvm_start_info`, the `hvm_memmap_table_entry` array and the PVH command line
now sit at `layout::PVH_HANDOFF_START = 0xd0000`, three pages immediately below
the ACPI tables, for the second and third reasons above: reserved in E820, and
MMIO as far as EDK2's DXE allocator is concerned.

They have to be that durable because **EDK2 never copies them**. The reset
vector stashes the `%ebx` pointer in `PcdXenPvhStartOfDayStructPtr`, and
`InstallCloudHvTables()` follows it again at the *end* of DXE — after PCI
enumeration — to read `rsdp_paddr` and walk our XSDT. At `0x1000..0x4000`,
where they used to live, they were inside the usable low-RAM E820 entry: host
structures advertised to the guest as free memory. When that read comes back
corrupted the pointer is usually non-canonical, so it faults as **`#GP`, not a
page fault**, and the boot ends with `X64 Exception Type - 0D` in
`QemuFwCfgAcpiPlatform.dll` one line after `OnRootBridgesConnected` — a symptom
that looks nothing like memory corruption. If you ever see that banner, dump
`0xd0000` first.

Guarded by `machine_x86::tests::the_pvh_handoff_block_is_reserved_not_ram`,
`uefi_boot::pvh::tests::the_handoff_block_is_never_usable_ram`, and the two
boot tests `tests/boot/tests/uefi_highmem.rs` /
`crates/vmm-core/tests/whp_highmem.rs`, which boot a 4096 MiB guest (RAM above
the high-RAM split — every other UEFI boot test uses 2048 MiB) and compare the
block byte-for-byte after the run.

**The FACS and the DSDT must never be listed in the XSDT.** EDK2's
`InstallCloudHvTables()` installs every XSDT entry as a table and then installs
the DSDT separately from the FADT's `X_DSDT`; a DSDT in the XSDT would be
installed twice.

## Port map: the ACPI PM register block

16 ports at `layout::ACPI_PM_BASE` = `0x600`, implemented by
`acpi::pm::AcpiPmBlock`, present in **both** boot modes (the FADT names them for
a direct-Linux guest exactly as it does for a firmware). Two addresses are not
ours to choose — EDK2's `OvmfPkg/Include/IndustryStandard/CloudHv.h` fixes
`CLOUDHV_ACPI_SHUTDOWN_IO_ADDRESS` at `0x600` and
`CLOUDHV_ACPI_TIMER_IO_ADDRESS` at `0x608` — and the rest is packed around them.

| Port | Width | Register | FADT field | Semantics |
|---|---|---|---|---|
| `0x600` | 1 | SLEEP_CONTROL | `SLEEP_CONTROL_REG` | `SLP_TYP` bits 4:2, `SLP_EN` bit 5 (write-only) |
| `0x601` | 1 | SLEEP_STATUS | `SLEEP_STATUS_REG` | `WAK_STS` bit 7, write-1-to-clear |
| `0x602` | 2 | PM1a_STS | `PM1a_EVT_BLK` +0 | write-1-to-clear |
| `0x604` | 2 | PM1a_EN | `PM1a_EVT_BLK` +2 | read/write |
| `0x606` | 2 | PM1a_CNT | `PM1a_CNT_BLK` | `SCI_EN` bit 0 (always reads set), `SLP_TYP` 12:10, `SLP_EN` 13 (write-only) |
| `0x608` | 4 | PM timer | `PM_TMR_BLK` | free-running 3.579545 MHz, 24-bit, read-only |
| `0x60c` | 2 | GPE0_STS | `GPE0_BLK` +0 | write-1-to-clear; no source drives it yet |
| `0x60e` | 2 | GPE0_EN | `GPE0_BLK` +2 | read/write |

Four facts that look arbitrary and are not:

* **The PM timer is latched once per access, not once per byte.**
  `AcpiPmBlock::io_read` samples `AcpiPmTimer::ticks()` *before* it walks the
  bytes of the access, because it is the one register in the block that moves
  on its own. Sampling per byte — which it did until 2026-09-09 — lets a carry
  out of the low byte land between byte 0 and byte 1, and the assembled 32-bit
  value is then up to 255 ticks ahead of the counter, so the guest's *next* read
  appears to go backwards. That is not a rounding error to a guest: EDK2's
  `MpInitLib` differences successive reads and reads a negative difference as
  the 24-bit counter wrapping, adding 4.7 s to its elapsed total and abandoning
  an application processor on the spot — the `MpInitLib: Find 1 processors`
  flake, 18 of 20 loaded WHP boots. The invariant to hold, and the one
  `acpi::pm::tests::a_wide_timer_read_is_one_sample_of_the_counter` asserts, is
  the one real hardware offers: **the value a guest reads lies between the
  counter immediately before the access and the counter immediately after it.**
  It applies to every free-running register, here and in any device added
  later.
* **The PM timer is a host clock the guest can read, and the tests use it as
  one.** `AcpiPmTimer` is derived from host `Instant`, so a guest reading
  `0x608` is reading the host's own monotonic clock — which is why the test
  initramfs's heartbeat probe reads it (`pm_us=` beside `uptime_ms=` and
  `tsc=`), and why that reading settled the 2026-09-09 drift finding: guest
  clock against host clock, both sampled inside the guest microseconds apart,
  with none of the harness's observation latency in between. Two consequences.
  The comparison **cannot** detect a host whose own clock is wrong — the WSL2
  host's gains a wandering 0.8–3.8 %, so the PM timer we synthesise there gains
  it too, and a guest firmware's `MicroSecondDelay()` is that much short on that
  host. And any
  device added later that reports host time to the guest inherits both the
  usefulness and the blind spot.
* **`SCI_EN` always reads set.** `FADT.SMI_CMD` is 0 (no SMI on this machine),
  so ACPICA must conclude the platform is *already* in ACPI mode;
  `AcpiHwGetMode()` decides that by reading exactly this bit. Read it back as 0
  and `AcpiEnable()` fails with "No SMI_CMD in FADT, mode transition failed"
  and ACPI never comes up.
* **`SCI_INT` is GSI 13, not the PC-conventional 9.** GSIs 5..=12 belong to the
  virtio-mmio slots (`VIRTIO_MMIO_FIRST_IRQ` + `MAX_VIRTIO_SLOTS`), and a
  level-triggered SCI sharing a line with an edge-triggered virtio interrupt
  would be a real bug. A `const` assertion in `acpi::tests` pins this.

## Guest power-off, and how to wire it

Two writes mean "power off", and both are implemented:

* **Linux** is not in hardware-reduced mode here, so `poweroff` →
  `acpi_power_off()` → ACPICA's `AcpiHwLegacySleep()` writes
  `SLP_TYP` (from `\_S5`) `| SLP_EN` into **PM1a_CNT** (`0x606`);
* **EDK2** keys on the host bridge id, not on the FADT: for CloudHv
  `ResetShutdown()` is literally `IoWrite8 (0x600, 5 << 2 | 1 << 5)` into
  **SLEEP_CONTROL** (`DxeResetShutdown.c`).

Either sets a latch. Anything other than `SLP_TYP == 5` is logged and ignored,
never treated as "some kind of shutdown".

The latch reaches the run loop through one defaulted trait method:

```rust
// vmm_core::ExitHandler
fn shutdown_requested(&self) -> bool { false }
```

Both run loops (`vmm_core::vcpu` and `vmm_core::whp::vcpu`) poll it after every
dispatched exit and return `RunOutcome::Shutdown`. This is the *only* thing that
can end the VM after an S5 write: the guest sits in `CpuDeadLoop()`/`hlt`
afterwards and never exits again.

**Integrator wiring.** `MachineBus` already owns the block and implements the
method, so `apps/entangled/src/run_vm.rs` needs exactly one line — the table
publication, next to the MP table:

```rust
machine_x86::mptable::write(vm.memory(), cfg.vcpus).map_err(|e| e.to_string())?;
machine_x86::acpi::write(vm.memory(), cfg.vcpus).map_err(|e| e.to_string())?;
```

Nothing else changes: `linux_boot::load` and `uefi_boot::load_pvh` advertise
`layout::ACPI_RSDP_START` unconditionally, and a machine that forgets the call
leaves a zeroed region there — the RSDP signature check fails and the guest
falls back to the MP table rather than reading garbage.

For a host-initiated shutdown (window close, SIGINT) the same latch is reachable
as `bus.acpi_pm()`; setting it would let the guest's own vCPU notice on its next
exit, which is a politer stop than `VcpuThreads::stop()`. Not wired yet.

## MADT / MP-table coexistence

Both are published, and they describe the same machine by construction:

| | MP table | MADT |
|---|---|---|
| CPUs | processor entries, LAPIC id == index | type 0 entries, `apic_id` == `acpi_uid` == index |
| IOAPIC | id `vcpu_count`, `0xfec00000` | type 1, same id, same address, GSI base 0 |
| Timer | ISA IRQ 0 → pin 2 | type 2 override, IRQ 0 → GSI 2, bus-conformant flags |
| LINT | LINT0 ExtINT, LINT1 NMI, all CPUs | type 4 NMI on LINT1 per CPU (ExtINT has no MADT equivalent; Linux infers it from `PCAT_COMPAT`) |
| SCI | IRQ 13 → pin 13, edge/high | type 2 override, GSI 13, **level/high** |

The LAPIC ids also have to match CPUID: `vmm_core::Vcpu::new` writes the vCPU
index into leaf 1 `EBX[31:24]`, leaves `0xb`/`0x1f`/`0x8000_0026` `EDX`, and
leaf `0x8000_001e` `EAX`. That last one is AMD's extended APIC id and is the
value Linux *prefers* on an AMD host (`parse_8000_001e()` overwrites what it
took from leaf 1) — without it a 2-vCPU guest logs
`[Firmware Bug]: CPU 1: APIC ID mismatch. CPUID: 0x0000 APIC: 0x0001`.

Linux ignores the MP table completely once it has an MADT
(`ACPI: Using ACPI (MADT) for SMP configuration information`), so the SCI's
trigger-mode disagreement is unobservable — nothing drives GSI 13 either way.
The MP table stays as the `acpi=off` fallback, which
`tests/boot/tests/acpi.rs::acpi_off_falls_back_to_the_mp_table` keeps honest.

## The DSDT and its AML

Hand-rolled in `acpi::aml` — ~150 lines against a 25-year-old grammar, no
dependency, and it compiles and tests on the Windows/WHP host too (where
`vm-memory` is unavailable). rust-vmm's `acpi_tables` crate would pass the
licence gate (Apache-2.0); it was rejected because its value is its AML builder
and this DSDT has five objects in it.

```asl
DefinitionBlock ("", "DSDT", 2, "ENTANG", "EDESKTOP", 1) {
    Name (\_S5, Package (4) { 0x05, 0x05, Zero, Zero })
    Scope (\_SB) {
        Device (PCI0) {
            Name (_HID, EisaId ("PNP0A03"))
            Name (_ADR, Zero)
            Name (_UID, Zero)
            Name (_CRS, ResourceTemplate () {
                WordBusNumber (..., 0x0000, 0x0000, 0x0000, 0x0001)
                IO (Decode16, 0x0CF8, 0x0CF8, 0x01, 0x08)
                DWordMemory (..., 0xC0000000, 0xCFFFFFFF, 0x00000000, 0x10000000)
            })
        }
        Device (C000) { Name (_HID, "ACPI0007") Name (_UID, Zero) }
        Device (C001) { Name (_HID, "ACPI0007") Name (_UID, 0x01) }
    }
}
```

* `\_S5` is not optional: `acpi_sleep_init()` refuses to register a power-off
  handler without it, which is why `poweroff` used to print
  "Power off not available".
* `PCI0._CRS`'s memory window is `layout::PCI_MMIO_HOLE_BASE` ..
  `+ PCI_MMIO_HOLE_SIZE`, which **stops below `VIRTIO_MMIO_BASE` on purpose**. A
  root-bridge window that swallowed the virtio-mmio slots would make Linux
  refuse the platform devices' `request_mem_region`. Those two constants are the
  coordination point with the PCI bus work (EPIC 19): move the window, not the
  DSDT.
* `_UID` matches the MADT's ACPI processor UID; that is how Linux pairs a CPU
  with its ACPI object.

### Checking AML changes with iasl

`iasl` is already a prerequisite of `guest/firmware/build-cloudhv.sh`, so it is
installed. Round-trip any AML change:

```bash
export ENTANGLED_ACPI_DUMP=$HOME/acpi-dump
cargo test -p machine-x86 --test acpi_dump -- --ignored --nocapture
cd $HOME/acpi-dump
iasl -d dsdt.aml     # disassemble to ASL; must match the block above
iasl -d facp.dat     # data tables decode field by field, with warnings
iasl -d apic.dat
```

`iasl -d` printing no error/warning lines *and* producing the ASL you meant is
the acceptance bar for a table change. `ENTANGLED_ACPI_VCPUS=<n>` changes the
CPU count in the dump.

## The 64-bit aperture in `_CRS` (EPIC 20, VEN-2001)

`\_SB.PCI0._CRS` publishes **two** producer windows since the shared-memory
window landed: the 32-bit `DWordMemory` hole, and a `QWordMemory` above RAM for
64-bit prefetchable BARs.

Two details decide whether a guest can use the second one at all:

* **it is marked prefetchable** (type-specific flags `0x07`: read/write, caching
  type 3). Linux' `pci_find_parent_resource` refuses to claim a prefetchable BAR
  inside a window that is not, skips it, and reassigns or disables the device.
  The 32-bit window stays plain read/write, which is correct for what lives in
  it;
* **its base is not a constant.** It is the top of guest RAM —
  `layout::pci_mmio64_base(mem)`, which is also what EDK2 computes as
  `Pci64Base` — so `acpi::write` derives it from the memory object's last
  address (`pci_mmio64_base_above`) rather than taking a number no caller has.
  That is why `AcpiTables::new` takes a second argument now, and why a test
  sweeps every guest size from 1 MiB to 64 GiB asserting the two spellings
  agree.

## Verifying against a real guest

```bash
# direct Linux: tables found, MADT topology, acpi=off fallback, poweroff
cargo test -p boot-tests --test acpi -- --test-threads=1 --nocapture
# UEFI: CloudHv installs our tables and counts the right number of CPUs
cargo test -p boot-tests --test uefi_acpi -- --nocapture
```

The lines that matter in a direct-Linux boot:

```text
BIOS-e820: [mem 0x00000000000e0000-0x00000000000effff] ACPI data
ACPI: RSDP 0x00000000000E0000 000024 (v02 ENTANG)
ACPI: XSDT 0x00000000000E0040 000034 (v01 ENTANG EDESKTOP ...)
ACPI: PM-Timer IO Port: 0x608
IOAPIC[0]: apic_id 2, version 17, address 0xfec00000, GSI 0-23
ACPI: INT_SRC_OVR (bus 0 bus_irq 0 global_irq 2 dfl dfl)
ACPI: Using ACPI (MADT) for SMP configuration information
ACPI: PM: (supports S0 S5)
ACPI: PCI Root Bridge [PCI0] (domain 0000 [bus 00])
```

and in a UEFI boot:

```text
MpInitLib: Find 2 processors in system.
OnRootBridgesConnected: root bridges have been connected, installing ACPI tables
```

`OnRootBridgesConnected` prints `InstallAcpiTables: <status>` **only on
failure**, so the absence of that line is the pass.

## What Ubuntu still needs

ACPI closes ADR-0003's phase-2 table gap, not the whole path to an installer:

* **virtio over PCI** (EPIC 19) is still the phase-3 blocker — CloudHv ships no
  virtio-mmio driver at all. The DSDT's PCI0 node and `layout::PCI_MMIO_HOLE_*`
  are the hooks it needs; when it lands, the `_CRS` should grow the device
  windows and the MADT may need PCI interrupt-link objects (`_PRT`) rather than
  today's bare ISA overrides.
* **No MSI veto.** `IAPC_BOOT_ARCH` deliberately leaves `MSI_NOT_SUPPORTED`
  clear even though nothing supports MSI yet, because setting it makes Linux
  disable MSI machine-wide and would silently defeat the first MSI-X device. A
  unit test pins the bit clear.
* **SSDT/`_PRT` for PCI interrupt routing**, once there are PCI devices with
  interrupts.
* **A power button.** GPE0 registers exist but nothing drives them, so a guest
  cannot be asked to shut down politely; that needs a GPE source plus a
  `\_GPE._Lxx` method and is the natural home for the window-close path.
* **`CMOS_RTC_NOT_PRESENT`.** A direct-Linux guest has no RTC, and setting that
  `IAPC_BOOT_ARCH` bit would skip a ~1.4 s `rtc_cmos` probe timeout — but the
  UEFI machine *does* have one, so the FADT would have to know the boot mode.
* **No reset register.** `FADT.RESET_REG_SUP` is clear; guests keep rebooting
  through their own restart chain (`reboot=k` → triple fault →
  `KVM_EXIT_SHUTDOWN`). An ACPI reset register in the PM block would be the
  tidier answer.
* **SMBIOS** is still missing (`CLOUDHV_SMBIOS_ADDRESS`, `0xf0000`), which is a
  separate table set with the same shape of problem.

## The reset register (ADR-0005)

`FADT.RESET_REG_SUP` is **set**, with `RESET_REG` = I/O 0xCF9 and `RESET_VALUE`
= 0x0E — the same pair QEMU's q35 publishes, and the register
`machine_x86::reset` implements.

It matters more than it looks. Linux's reboot ladder starts at `BOOT_ACPI`:
`acpi_reboot()` writes `RESET_VALUE` to `RESET_REG` if the flag is set, and does
nothing at all if it is not. With the flag clear (as it was before), every guest
reboot silently failed its first attempt and walked down to the keyboard
controller, 0xCF9 and finally a triple fault — which on WHP is absorbed by the
hypervisor and never reaches the host. Naming the register is what makes a
reboot land on the first try, on both hosts.

Nothing else in the FADT changed, and EDK2 does not use the ACPI reset path
(`ResetSystemLib` writes 0xCF9 directly), so the two agree by construction —
they are the same port.

The **ACPI PM timer is pausable** now: `AcpiPmTimer` freezes on pause and its
origin moves forward by the length of the pause, so a firmware spinning in
`MicroSecondDelay()` when the VM was frozen does not come back to find its delay
already over by minutes. On reset it starts from zero, and the S5 shutdown latch
is cleared — a latch left set would end the VM on the next exit after a reboot,
which looks exactly like a guest that powered off during boot.
