# ADR-0003: UEFI firmware for Entangled Desktop — EDK2 CloudHv, entered via PVH

- Status: accepted — all four phases have run on hardware (UEFI-1801…1804)
- Date: 2026-08-19
- Backlog: EPIC 18 (UEFI-1801 pflash/firmware mapping, UEFI-1802 firmware
  build+boot, UEFI-1803 boot an installer ISO, UEFI-1804 install and boot
  Ubuntu end to end)
- Extends: [ADR-0001](0001-mvp-architecture.md) (§3 "No BIOS/UEFI in MVP")
- Sources: `edk2-stable202602` (commit `b7a715f7c03c`), checked out and built
  locally; Xen `docs/misc/pvh.pandoc` and
  `xen/include/public/arch-x86/hvm/start_info.h`

## Context

Entangled Desktop boots Linux guests through exactly one path: the direct
`bzImage` protocol (`crates/linux-boot`). The host loads the kernel, builds
`boot_params` + E820, and drops vCPU0 straight into long mode at the kernel's
64-bit entry (`machine_x86::boot::setup_long_mode_sregs` +
`setup_boot_regs`). There is no firmware, no BIOS, no option ROM, and no
bootloader.

That is why installed guests need a project-maintained bootstrap kernel
(ADR-0001 §3): the guest cannot boot *itself*. It also means an arbitrary
distribution ISO — Ubuntu's, which is a UEFI-bootable hybrid image with
shim + GRUB in an ESP — cannot boot at all.

Fixing this needs a firmware that runs *inside* the guest, discovers our
hardware, finds the ESP on a virtual disk and hands off to the distro's
bootloader. This ADR picks that firmware and records the machine contract it
imposes.

## Options considered

### (a) EDK2 `OvmfPkg/CloudHv/CloudHvX64` — chosen

The OVMF variant maintained for Cloud Hypervisor, i.e. for a non-QEMU,
minimal-emulation VMM. Its own README states the design premise:

> the project logically decided to support the PVH boot specification as the
> only way of booting virtual machines. […] PVH allows information like
> location of ACPI tables and location of guest RAM ranges to be shared
> without the need of an extra emulated device like a CMOS.

Verified in the sources rather than assumed:

- **No fw_cfg.** `OvmfPkg/CloudHv/CloudHvX64.dsc` binds
  `QemuFwCfgLib|OvmfPkg/Library/QemuFwCfgLib/QemuFwCfgLibNull.inf` for every
  phase. Nothing in this build talks to QEMU's 0x510/0x511 fw_cfg ports.
- **Not a reset-vector image.** `CloudHvDefines.fdf.inc` sets
  `FW_BASE_ADDRESS = 0x004FFFD0`, `FW_SIZE = 0x00400000`, and
  `CloudHvX64.fdf` reserves the first 4 KiB of the flash device for a PVH ELF
  header ("Leaving 4kiB for the PVH ELF header"). The built image is an ELF:

  ```
  Entry point address:  0x4fffd0
  LOAD  off 0x0        paddr 0x0000000000100000  filesz 0x400000  RWE
  NOTE  off 0xb0       paddr 0x00000000001000b0  filesz 0x14      R
    Owner "Xen"  type 0x12 (XEN_ELFNOTE_PHYS32_ENTRY)  desc: d0 ff 4f 00
  ```

  So the firmware is *loaded into guest RAM at 1 MiB* and entered at
  `0x004FFFD0` in 32-bit protected mode. There is no code at
  `0xFFFF_FFF0`; mapping this image as a ROM below 4 GiB would execute
  nothing but the ELF header.
- **Memory map from PVH, not from CMOS.**
  `OvmfPkg/Library/PlatformInitLib/MemDetect.c` routes
  `PlatformScanE820()` to `PlatformScanE820Pvh()` when the host bridge is
  CloudHv, and `GetPvhMemmapEntries()` reads
  `hvm_start_info->memmap_paddr / memmap_entries` out of the pointer that
  `OvmfPkg/XenResetVector/Ia32/XenPVHMain.asm` stashed from `%ebx`.
- **Chatty on the serial port.** Built `-b DEBUG -D DEBUG_ON_SERIAL_PORT`, the
  DebugLib becomes `BaseDebugLibSerialPort` over
  `PcAtChipsetPkg/Library/SerialIoLib`, whose `gUartBase = 0x3F8` — exactly
  the 16550 `machine_x86::serial` already emulates. Without that define the
  log goes to the Bochs debug I/O port `0x402`
  (`PcdDebugIoPort`), which we do not emulate.
- **License:** BSD-2-Clause-Patent. Buildable from source in ~2.5 min on this
  machine (`guest/firmware/build-cloudhv.sh`), so nothing is vendored.

### (b) `rust-hypervisor-firmware` — kept as a fallback, not the target

Apache-2.0, Rust, "designed to be launched from anything that supports
loading ELF binaries and running them with the PVH booting standard". It can
chain-load "shim + GRUB2 as used by Ubuntu" and implements just enough UEFI
for that.

Rejected as the *target* because it is a deliberately partial UEFI
implementation: no variable services worth the name, no EFI shell, no
network stack, and its own docs scope it to booting cloud images. An Ubuntu
*installer* ISO exercises much more of the UEFI surface than a cloud image
does. It stays interesting as a smoke-test payload: it uses the same PVH
entry contract as (a), so it costs nothing extra to support, and it is far
smaller to debug than a 4 MiB EDK2 build.

Note that it does **not** relax the hardware contract: like (a), its block
support is virtio over **PCI**.

### (c) Plain `OvmfPkg/OvmfPkgX64` — rejected

This is the reset-vector firmware: `OvmfPkg/Include/Fdf/OvmfPkgDefines.fdf.inc`
sets `FW_BASE_ADDRESS = 0xFFC00000` with `FW_SIZE = 0x00400000` for the 4 MiB
build — i.e. the image ends exactly at 4 GiB and the reset vector lands at
`0xFFFF_FFF0`. Attractive shape, wrong dependencies. Verified couplings:

- `OvmfPkgX64.dsc` binds the *real* `QemuFwCfgLib` in SEC, PEI and DXE
  (`QemuFwCfgSecLib.inf`, `QemuFwCfgPeiLib.inf`, `QemuFwCfgDxeLib.inf`).
- `PlatformScanE820()` reads the RAM map from the fw_cfg file `etc/e820`;
  when that is missing, `PlatformGetSystemMemorySizeBelow4gb()` falls back to
  **CMOS 0x34/0x35** and `PlatformGetSystemMemorySizeAbove4gb()` to CMOS
  0x5b–0x5d. So the fallback path is not "no device" but "a different QEMU
  device" — we would have to emulate fw_cfg *or* an RTC/CMOS with QEMU's
  memory-sizing conventions.
- ACPI tables, SMBIOS, boot order, the 64-bit PCI MMIO aperture hint and S3
  state all arrive over fw_cfg in this build.

Adopting (c) means re-implementing a QEMU device contract inside a VMM whose
first hard rule is "No QEMU anywhere". Rejected on principle and on cost.

## Decision

1. **Target firmware: EDK2 `OvmfPkg/CloudHv/CloudHvX64`**, pinned to
   `edk2-stable202602`, built from source by `guest/firmware/build-cloudhv.sh`
   into `artifacts/firmware/CLOUDHV.fd` with a provenance file. Firmware
   binaries are cached artifacts — never committed, never vendored.
2. **Primary entry protocol: PVH.** `BootMode::Uefi` detects an ELF image
   carrying `XEN_ELFNOTE_PHYS32_ENTRY` and boots it per the PVH contract
   below. This is what actually executes CloudHv and rust-hypervisor-firmware.
3. **Secondary entry protocol: reset vector.** A firmware image that is *not*
   a PVH ELF is treated as a flash image, mapped so that its last byte lands
   at `0x1_0000_0000 - 1` (4 GiB), and the vCPU is left in the architectural
   reset state. This is UEFI-1801 as written, it is the only shape a
   plain-OVMF-style build can use, and it is testable without any real
   firmware. It is **not** the path CloudHv takes.
4. **The firmware image is never RAM.** It occupies its own KVM memory slot,
   outside `GuestMem`, and never appears as `E820Type::Ram` or as an
   `XEN_HVM_MEMMAP_TYPE_RAM` entry.
5. **Phase 1 (this change) succeeds when the firmware talks on ttyS0.** Full
   ISO boot is phases 2–3; the gaps are enumerated below rather than guessed
   at later. Both have since landed — phase 2 is the ACPI tables, phase 3 is
   UEFI-1803 — and what each actually cost is recorded under "Measured during
   bring-up".

## What our machine must provide

### For the PVH entry (phase 1, implemented)

From Xen `docs/misc/pvh.pandoc`, the 32-bit entry contract, verbatim on the
points that bind us:

> `ebx`: contains the physical memory address where the loader has placed the
> boot start info structure.
> `cr0`: bit 0 (PE) must be set. All the other writeable bits are cleared.
> `cr4`: all bits are cleared.
> `cs`: must be a 32-bit read/execute code segment with a base of '0' and a
> limit of '0xFFFFFFFF'.
> `ds`, `es`, `ss`: must be a 32-bit read/write data segment with a base of
> '0' and a limit of '0xFFFFFFFF'.
> `tr`: must be a 32-bit TSS (active) with a base of '0' and a limit of
> '0x67'.
> `eflags`: bit 17 (VM) must be cleared. Bit 9 (IF) must be cleared. Bit 8
> (TF) must be cleared.

Paging is **off** — which is why this path deliberately does not call
`setup_page_tables()`/`setup_long_mode_sregs()`.

Plus `hvm_start_info` (version 1, magic `0x336ec578`) in guest RAM, with a
`hvm_memmap_table_entry` array describing RAM as
`XEN_HVM_MEMMAP_TYPE_RAM (1)`.

### For the reset-vector entry (phase 1, implemented)

KVM's post-`KVM_CREATE_VCPU` vCPU state *is* the architectural reset state:
`CS.selector = 0xF000`, `CS.base = 0xFFFF_0000`, `RIP = 0xFFF0`,
`CR0 = 0x6000_0010` (PE clear), paging off — so the first instruction fetch is
at `0xFFFF_FFF0`. The machine therefore does **nothing** to the registers in
this mode; the only requirement is that the ROM covers that address, which
the "end at 4 GiB" placement rule guarantees.
(`crates/uefi-boot/tests/reset_vector.rs` asserts the reset state rather than
trusting this paragraph.)

### Measured during bring-up (2026-08-19)

The gap map below was written from the sources *before* the firmware ran. What
the firmware then actually did, in order, with the machine feature each step
required:

| # | Serial output | Machine feature added |
|---|---|---|
| 1 | `AcpiTimerLibConstructor: Unknown Host Bridge Device ID: 0xFFFF` / `ASSERT [SecMain] BaseRomAcpiTimerLib.c(65)` | PCI configuration space on `0xcf8`/`0xcfc` with a `0x8086:0x0d57` host bridge at `00:00.0` (`machine_x86::platform`) |
| 2 | reached PEI, `PlatformMiscInitialization: Cloud Hypervisor is done.`, RAM sized from our PVH memmap | the ACPI PM timer at `0x0608` (`InternalAcpiGetTimerTick()` is a bare `IoRead32` there, and `MicroSecondDelay()` spins on it) |
| 3 | `ASSERT [CpuDxe] MpLib.c(1971)` in `GetBspNumber()` | **a real bug in our VMM, not a missing device**: `KVM_GET_SUPPORTED_CPUID` reports the *host* CPU's APIC ID in CPUID leaf 1 `EBX[31:24]` and leaves the topology leaves' x2APIC ID alone, so every vCPU claimed APIC id `0x0a` while its local APIC said `0`. `vmm_core::Vcpu::new` now writes the vCPU index into leaf 1 `EBX[31:24]` and leaves `0xB`/`0x1F` `EDX`, as every rust-vmm VMM must |
| 4 | `ASSERT_EFI_ERROR (Status = Device Error)` / `ASSERT [PcRtc] PcRtcEntry.c(181)` | an MC146818 RTC/CMOS at `0x70`/`0x71` (`machine_x86::rtc`) — `EFI_RUNTIME_SERVICES.GetTime()` has no other source, and `RtcWaitToUpdate()` times out without it |
| 5 | **full boot to the UEFI Boot Manager**: DXE dispatch, `PciBus: Discovered PCI @ [00\|00\|00] [VID = 0x8086, DID = 0xD57]`, console terminal modes, `BdsDxe` load-option dump with *BootManagerMenuApp*, *EFI Firmware Setup*, *EFI Internal Shell*, then `BdsDxe: No bootable option or device was found.` | — nothing; this is the expected end of phase 1 |

Two of the predicted gaps showed up verbatim in that run and are worth
recording as confirmed rather than theorised:

- `QEMU Flash: Attempting flash detection at 4FFFD0` → `QemuFlashDetected => FD
  behaves as RAM` → `QEMU flash was not detected. Writable FVB is not being
  installed.` The firmware then falls back to `EmuVariableFvbRuntimeDxe`
  ("EMU Variable FVB: Using pre-reserved block at 7FF7C000"), so UEFI variables
  work but live only in RAM. Persisting them is the pflash/NVRAM item below.
- `QemuFwCfgAcpiPlatform` loads and parks in
  `AcpiPlatformEntryPoint: waiting for root bridges to be connected` — it has
  neither fw_cfg nor a non-zero `rsdp_paddr`, so no ACPI tables are installed.
  `SmbiosPlatformDxe` likewise fails `Not Found` (no SMBIOS entry point at
  `CLOUDHV_SMBIOS_ADDRESS`, `0xf0000`).

  **Closed for ACPI (2026-08-19).** With `machine_x86::acpi` publishing the
  tables and `load_pvh` setting `rsdp_paddr`, the same driver now continues past
  that park to `OnRootBridgesConnected: root bridges have been connected,
  installing ACPI tables` and installs them without a failure status
  (`tests/boot/tests/uefi_acpi.rs` asserts exactly that, alongside
  `MpInitLib: Find 2 processors in system` and a clean run to the Boot Manager).
  SMBIOS is untouched and still `Not Found`.

### The gap map

Established from the sources, in the order the firmware hits them. "Status" is
as of the bring-up run above.

| Gap | Status | Why the firmware needs it | Where |
|---|---|---|---|
| **PCI host bridge at 00:00.0 with device ID `0x0d57`** | **closed** — `machine_x86::platform::PciConfigSpace` | `PlatformPei` does `PciRead16 (OVMF_HOSTBRIDGE_DID)` over ports `0xcf8/0xcfc` and switches on the result; `CLOUDHV_DEVICE_ID = 0x0d57` (`OvmfPkg/Include/IndustryStandard/CloudHv.h`). Anything else reaches `default:` → `DEBUG_ERROR "Unknown Host Bridge Device ID"` → `ASSERT (FALSE)` | `OvmfPkg/Library/PlatformInitLib/Platform.c:365` |
| **ACPI PM timer at 0x0608** | **closed** — `machine_x86::platform::AcpiPmTimer` | `AcpiTimerLib` reads `CLOUDHV_ACPI_TIMER_IO_ADDRESS` directly, and `MicroSecondDelay()` spins on it | `OvmfPkg/Include/IndustryStandard/CloudHv.h` |
| **RTC/CMOS at 0x70/0x71** | **closed** — `machine_x86::rtc` | `PcatRealTimeClockRuntimeDxe` backs `EFI_RUNTIME_SERVICES.GetTime()`; `PcRtcInit()` returns `EFI_DEVICE_ERROR` without a clock whose UIP is clear and VRT set | `PcAtChipsetPkg/PcatRealTimeClockRuntimeDxe/PcRtc.c` |
| **Per-vCPU APIC id in CPUID** | **closed** — `vmm_core::Vcpu::new` | Not a device: `KVM_GET_SUPPORTED_CPUID` leaks the host CPU's APIC id into leaf 1 `EBX[31:24]`, so `GetBspNumber()` cannot match the BSP against the MP hand-off HOB | `UefiCpuPkg/Library/MpInitLib/MpLib.c:1971` |
| **virtio over PCI** | **closed** — `machine_x86::{pci,virtio_pci}` + `virtio_core::pci` | CloudHv's FDF ships `VirtioPciDeviceDxe`, `Virtio10Dxe`, `VirtioBlkDxe`, `VirtioScsiDxe`, `VirtioNetDxe` — and **no** virtio-MMIO driver at all, so UEFI-1803 needed the transport before it needed anything else. It now enumerates all five functions and binds both disks. Three further gaps had to close before that worked; see "Phase 3" below | `OvmfPkg/CloudHv/CloudHvX64.fdf:225–235` |
| **ACPI tables + `rsdp_paddr`** | **closed** — `machine_x86::acpi` + `uefi_boot::load_pvh` | `InstallCloudHvTables()` dereferences `hvm_start_info.rsdp_paddr`, walks the XSDT installing every table it lists, then installs the DSDT from the FADT's `X_DSDT`; a zero (or unsigned) RSDP made it return `EFI_NOT_FOUND`. Now: RSDP/XSDT/FADT/FACS/MADT/DSDT at `0xe0000`, and `OnRootBridgesConnected: … installing ACPI tables` with no failure status. **Correction to the prediction:** the MADT is *not* how this firmware counts CPUs — `PlatformMaxCpuCountInitialization()` reads fw_cfg only, so `boot CPU count unavailable` is still logged and still harmless; the real count comes from `MpInitLib`'s INIT-SIPI sweep (`MpInitLib: Find 2 processors in system`), which needs the per-vCPU APIC id fix, not a table. The tables matter for the guest OS, and for `poweroff` | `OvmfPkg/AcpiPlatformDxe/CloudHvAcpi.c`, `.claude/skills/acpi-machine/SKILL.md` |
| **Writable pflash / NVRAM** | **closed** — `machine_x86::pflash` + a two-line firmware build override | Was: `QemuFlashDetected => No` → `EmuVariableFvbRuntimeDxe`, variables in RAM, `BootOrder`/`Boot####` lost on every stop. That did not block an ISO boot (with no `BootOrder`, `BdsDxe` enumerates removable media and synthesises an entry for the ISO's ESP — exactly what a first boot from installation media needs) and it did block UEFI-1804, where the installed system is reached through the `Boot####` entry `grub-install` writes and nothing else. Now: an emulated CFI-01 device at `0xffc0_0000`, backed by a per-VM NVRAM file. **The prediction in this row was wrong about where the store is**, and the correction is the whole story of phase 4 below: CloudHv declares *no* varstore region at all, and the flash PCDs point at `FW_BASE_ADDRESS = 0x004FFFD0`, which is inside the loaded image and *is* the PVH entry point. Nothing could be emulated there | `OvmfPkg/CloudHv/CloudHvDefines.fdf.inc`, `OvmfPkg/QemuFlashFvbServicesRuntimeDxe/QemuFlash.c`, `guest/firmware/build-cloudhv.sh` |
| **ACPI shutdown port 0x0600** | **closed** — `machine_x86::acpi::pm` | `CLOUDHV_ACPI_SHUTDOWN_IO_ADDRESS` is the ACPI 5.0 `SLEEP_CONTROL_REG`, and for a CloudHv host bridge `ResetShutdown()` is `IoWrite8 (0x600, 5 << 2 \| 1 << 5)`. It is now one register of a 16-byte PM block that also carries PM1a_EVT/CNT (the register Linux uses instead), the PM timer and GPE0; either sleep register latches a request that `ExitHandler::shutdown_requested` turns into `RunOutcome::Shutdown` | `OvmfPkg/Library/ResetSystemLib/DxeResetShutdown.c` |
| **MMIO hole agreement** | **free, but no longer free of consequences** | The firmware hard-codes the CloudHv 32-bit aperture as `0xc000_0000 + 0x3800_0000`. Our `layout::MMIO_HOLE_START` is already `0xc000_0000` and the virtio window at `0xd000_0000` sits inside it — but `PciBusDxe` *allocates* out of that aperture rather than accepting what it finds, so "agreement" turned out to mean the machine must decode the whole advertised window, not just its own slots. See phase 3 gap 2 | `OvmfPkg/Library/PlatformInitLib/MemDetect.c:61` |

### Phase 3 (2026-08-19): the Ubuntu ISO boots to the installer

Where the chain got to, from the same DEBUG serial log the earlier phases were
debugged on:

```
PciBus: Discovered PCI @ [00|01|00]  [VID = 0x1AF4, DID = 0x1042]     (target disk)
PciBus: Discovered PCI @ [00|02|00]  [VID = 0x1AF4, DID = 0x1042]     (installer ISO)
Found Mass Storage device: PciRoot(0x0)/Pci(0x2,0x0)
VirtioBlkInit: LbaSize=0x200[B] NumBlocks=0x56FB24[Lba]               (5700388 sectors)
FSOpen: Open '\EFI\BOOT\BOOTX64.EFI' Success
BdsDxe: starting Boot0003 "UEFI Misc Device 2" from PciRoot(0x0)/Pci(0x2,0x0)
FSOpen: Open '\EFI\BOOT\grubx64.efi' Success
GNU GRUB  version 2.14   →   *Try or Install Ubuntu Server
MpInitChangeApLoopCallback() done!                                    (ExitBootServices)
```

and then, on the virtio-gpu scanout, subiquity's language-selection screen.
`tests/boot/tests/uefi_iso.rs` is the automated form of that log;
`ENTANGLED_UEFI_ISO_LINGER` + `ENTANGLED_UEFI_ISO_SHOT` reproduce the
screenshot.

Four gaps had to close between "the firmware enumerates our disks" and that.
None of them were in the firmware, and — this is the point worth keeping — none
of them were visible to any Linux-only test, because in each case Linux either
does not exercise the register or is more forgiving than EDK2:

| # | Symptom | Gap | Why Linux never caught it |
|---|---|---|---|
| 1 | `PciBusDxe` prints the disks, `VirtioBlkDxe` never binds, no boot media, nothing in the log says why | **PCI subsystem device id was 0.** `Virtio10BindingSupported` requires `Pci.Device.SubsystemID >= 0x40` (spec 1.2 §4.1.2.1 asks a non-transitional device for exactly that; QEMU writes `0x40`) | Linux's `vp_modern_probe` reads the subsystem *vendor* and ignores the device half entirely |
| 2 | `VirtioBlkInit` reports the right capacity, then the first queue kick wakes an *unrelated* device's worker (`queue notify before DRIVER_OK, ignoring  slot=4 device=Input`) and the disk waits forever | **Queue-notify ioeventfds must follow BAR0.** `PciBusDxe` reassigns every BAR during resource allocation — measured: it hands out our own aperture slots *in reverse device order* — and the notification area moves with the BAR. `machine_x86::notify::DeviceNotifier::rebase` plus `VirtioPciBus::reconcile_notify` re-point them; `PciRoot::io_write` now returns a `#[must_use] DecodeChanged`. `PCI_MMIO_END` also widened from the 8 host slots (128 KiB) to the 256 MiB the DSDT already advertises, because a 1 MiB allocation only fitted in 128 KiB by luck of having five devices | Linux claims a BAR it finds already programmed and leaves it where it is |
| 3 | Kernel boots, then **every** virtio probe ends in `driver gave up on this device (FAILED)` — no disks, no GPU, no input | **`interrupt_line` must be read-only.** `PciBusDxe` writes `PCI_INT_LINE_UNKNOWN` (`0xff`) and then `0` to offset `0x3c`, expecting a platform driver to fill in the routed value; there is no such driver here because there is no PIRQ router. Linux reads `0xff` → "not connected" (PCI 3.0 §6.2.4), finds no `_PRT` under `\_SB.PCI0` either, sets `IRQ_NOTCONNECTED`, and `vp_find_vqs_intx`'s `request_irq` fails | Under direct-Linux nothing writes the register, so the host's value was still there to fall back on |
| 4 | Four of five devices bind; one — whichever holds IOAPIC pin 8 — does not | **Pin 8 is the RTC's.** `machine_x86::rtc` exists because UEFI-1802 needed `GetTime()`; Linux registers `rtc_cmos` on IRQ 8 and will not share it, so `request_irq(8, …, IRQF_SHARED)` returns `-EBUSY`. `layout::VIRTIO_IRQS` is now an explicit non-contiguous table, `[5, 6, 7, 9, 10, 11, 12, 14]`, shared by both transports | The virtio-pci acceptance boot attaches one device, which gets pin 5. A five-device mmio boot has the same latent defect and is fixed by the same table |

Diagnosis method for #4 is worth copying: rather than reasoning about which
driver owns IRQ 8, the *pin* was made the variable. Adding a third disk shifted
every later device up one slot, the failure moved to the GPU (which inherited
pin 8) and both input devices bound. The fault followed the pin, not the device.

Two further observations from the same run, recorded so nobody re-derives them:

- **CloudHv ships no `VirtioGpuDxe`**, so there is no GOP for our virtio-gpu and
  the firmware and GRUB are visible *only* on ttyS0. The scanout stays dark until
  Linux's own `virtio_gpu` driver binds. The MVP premise that "the window is the
  display" holds for the guest OS and not for the firmware — which is also why
  the boot test asserts on serial and screenshots separately.
- **The kernel's command line comes from the ISO** and carries no `console=`
  clause, so nothing Linux prints reaches ttyS0. Injecting one needs either an
  interactive GRUB edit (the virtio keyboard now works, so this is possible) or a
  `grub.cfg` on a volume we control; neither is needed to *see* the installer,
  which draws on the scanout.

### Phase 4 (2026-08-19): Ubuntu installs, and the installed system boots

`entangled install ubuntu --disk … --auto --headless` now completes unattended,
and `entangled run` of the profile it writes boots the *installed* system to a
login prompt. Two problems had to be solved, and neither was the one this ADR
predicted.

#### The variable store does not exist where the PCDs say it does

The gap-map row above assumed CloudHv's flash simply needed a device behind it.
It does not have a flash region at all. Read out of the pinned tree:

- `CloudHvX64.fdf` declares three regions — a 4 KiB PVH ELF header at offset 0,
  `FVMAIN_COMPACT` at `0x1000`, `SECFV` at `0x3CC000` — and **no**
  `!include OvmfPkg/Include/Fdf/VarStore.fdf.inc`. The `VARS_*` defines are
  inherited boilerplate; nothing consumes them. Where OvmfPkgX64 keeps a
  pre-formatted variable store, CloudHv has the ELF header and 0x83000 bytes of
  `0xFF` padding.
- `CloudHvDefines.fdf.inc` sets `FW_BASE_ADDRESS = 0x004FFFD0` and derives
  `PcdOvmfFdBaseAddress` and `PcdOvmfFlashNvStorageVariableBase` from it. That
  address is **not** where the image is loaded — the hard-coded PVH ELF header
  says `paddr 0x00100000` — it is not page aligned, and it *is* the PVH entry
  point: `0x100000 + 0x3FFFD0`, the first instruction the firmware executes.
  So `QemuFlashDetected()` probes guest RAM the firmware is running from, reads
  back what it wrote, and reports `FD behaves as RAM`.

Which rules out emulating flash at the address the firmware asks for: an MMIO
region cannot be the instruction-fetch target of the entry point, and the 4 MiB
from `0x004FFFD0` overlaps both the loaded image and the PEI/DXE working memory
at `0x800000`. This is the "(b)" case — the firmware genuinely cannot use a
flash varstore as built — so the build moves the two PCDs that say *where the
flash is* to `0xFFC00000`, the address OvmfPkgX64 uses, where a 4 MiB window
ends exactly at 4 GiB and clears the MMIO hole, the IOAPIC and the LAPIC.
Everything else follows: the event-log, FTW-working and FTW-spare bases are
computed from the variable base inside the FDF.

Deliberately *not* changed: `FW_BASE_ADDRESS`, `FW_SIZE` and the `[FD.CLOUDHV]`
layout (so the image keeps its shape and the hard-coded ELF header still
describes it), `PcdCfvBase`/`PcdBfvBase` (confidential-computing measurement
inputs, which point into the image, not the store), and
`PcdOvmfFirmwareFdSize` (what bounds `QemuFlashWrite` and the GCD range).
`build --pcd` was tried first and is silently ignored for these — the FDF's own
`SET` wins, and the build does not even re-run AutoGen — so the two lines are
edited with an asserted pre-image and the built value is read back out of
`AutoGen.h` afterwards. That check matters because the failure mode is silent:
a firmware probing an address nothing decodes just goes back to RAM variables.

Three device details were measured rather than reasoned about, each one a boot
that ended badly first:

| Symptom | Cause |
|---|---|
| `QemuFlashDetected => FD behaves as RAM` with a device present | clear-status (`0x50`) must return to *read-array* mode, not stay in status mode |
| `QemuFlashDetected` fell through all three verdicts | the cleared status register must read `0x00`. `pflash_cfi01` starts at zero and sets the ready bit only when an operation completes; a device answering `0x80` there is dismissed as none of RAM, ROM or flash |
| `ASSERT [VariableRuntimeDxe] VariableNonVolatile.c(228): VariableStore->Size == VariableStoreLength` | a fresh store cannot be blank flash. `FvbInitialize` rewrites a missing *FV* header, but nothing writes the `VARIABLE_STORE_HEADER` behind it — on QEMU it arrives pre-formatted inside the flash image. The host now generates the same empty-but-formatted store (`pflash::pristine_varstore`) rather than vendoring one |

Evidence, two boots against one NVRAM file
(`tests/boot/tests/uefi_nvram.rs`):

```
QEMU Flash: Attempting flash detection at FFC00010
QemuFlashDetected => FD behaves as FLASH, writable
Installing QEMU flash FVB
Disabling EMU Variable FVB since flash variables appear to be supported.
  Boot0000/0001/0002 + BootOrder   3423 bytes programmed, 0 blocks erased
-- VM stopped, VM restarted --
  the same options, read back              1538 bytes programmed
```

#### `autoinstall` has to reach the kernel command line, which lives on read-only media

subiquity finds a cloud-init NoCloud seed by itself (a volume labelled `CIDATA`
holding `user-data`/`meta-data`), but it will not act on an autoinstall
configuration unattended unless the word `autoinstall` is in `/proc/cmdline`.
That is one unconditional check in the installer; no key in the configuration
file changes it, `interactive-sections: []` included. And the command line comes
from the ISO's own `grub.cfg`.

Both documented ways out were rejected: repacking the ISO throws away the
provenance that is the whole point of the verified fetch, and booting
`casper/vmlinuz` directly (what the upstream quickstart does under QEMU) means
the installer does not see firmware — so subiquity makes a BIOS boot partition
instead of an ESP, and the result is a disk this VMM cannot boot at all.

So the installer's command line is typed into GRUB over the serial console,
which is what the documentation tells a person to do. It works because of a
constraint recorded in phase 3 as a limitation: CloudHv ships no `VirtioGpuDxe`,
so the firmware's console *is* ttyS0 — and GRUB, running on the EFI console,
reads the same UART. `MachineBus::push_serial_input` reaches both.

```
grub> echo entangled: root=$root prefix=$prefix
entangled: root=hd1 prefix=(hd1)/boot/grub
grub> linux /casper/vmlinuz autoinstall console=ttyS0,115200n8 ---
grub> initrd /casper/initrd
grub> boot
```

Each line is sent only after GRUB has printed a fresh prompt, so the exchange is
self-synchronising and legible in the transcript. No `search` is needed: GRUB
read its menu from `($root)/boot/grub/grub.cfg` on the ISO9660 filesystem, so
`$root` already is the installer volume. The same command line carries
`console=ttyS0`, which is the difference between an install that can be
diagnosed and one that runs blind.

#### What the run looks like

```
wrote the cloud-init NoCloud seed volume     bytes=65536 label=CIDATA
UEFI variable store ready                    bytes=540672 fresh=true
GRUB menu is up; opening its command line
typing into GRUB × 4
subiquity/load_autoinstall_config … cmd-install … curtin in-target -- update-grub
[  288.4] reboot: Power down
guest requested ACPI S5 (soft off)           via="PM1a_CNT"
UEFI variable store written by the firmware  programmed_bytes=5328
installation detected                        esp=1 root=2 root_uuid=…
```

and then, from the profile that was written:

```
Boot0006: Ubuntu                             0x0001
FSOpen: Open '\EFI\ubuntu\shimx64.efi' Success
BdsDxe: starting Boot0006 "Ubuntu" from HD(1,GPT,…)/\EFI\ubuntu\shimx64.efi
GNU GRUB  version 2.14 → Booting initrd of Ubuntu 26.04 LTS
Welcome to Ubuntu 26.04 LTS!
[  OK  ] Started serial-getty@ttyS0.service - Serial Getty on ttyS0.
```

`Boot0006 "Ubuntu"` is the whole point of the pflash device: it is the entry
`grub-install` wrote through the CFI device on the previous boot, read back out
of a file.

One more defect surfaced here and is worth keeping: after `reboot: Power down`,
**126 seconds** passed before the run loop noticed. A powered-off guest sits in
HLT and gives KVM no reason to exit, so the vCPU threads — which check
`ExitHandler::shutdown_requested` after each exit — never ran the check.
`apps/entangled` now watches the same latch from the supervisor loop.

### What is still missing after UEFI-1804

- **An ACPI `_PRT`** under `\_SB.PCI0`, with level-triggered `INTA#` (an irqfd
  pair or a resample eventfd) instead of an edge on an ISA pin. The current
  arrangement works because Linux falls back to `interrupt_line` and warns
  `PCI INT A: no GSI - using ISA IRQ 5`; it caps us at one device per pin and
  will not survive a guest that trusts ACPI over the register.
- **MSI-X**, which would retire the whole INTx question — and gaps 3 and 4 with
  it — rather than working around it twice.
- **SMBIOS**, still `Not Found` at `CLOUDHV_SMBIOS_ADDRESS` (`0xf0000`). Harmless
  so far; `dmidecode` and some installer hardware detection want it.

## Consequences

- Two boot modes coexist. `BootMode::DirectLinux` stays the fast path for our
  own bootstrap kernel and every existing test; `BootMode::Uefi` is additive
  and shares the device plumbing unchanged.
- The `[boot]` section grows a `firmware` key and `kernel` becomes optional,
  validated per mode.
- `vmm-core` gains an explicit second kind of memory slot (ROM), which is the
  same shape a future pflash device and a future high-RAM split need.
- **UEFI-1803/1804 depend on virtio-pci**, which was already the top item of
  "next phase after MVP" in the backlog. This ADR upgrades that from
  "logical next step" to a hard prerequisite: no PCI bus, no ISO boot,
  regardless of which of the two firmwares we pick. *Borne out:* the transport
  landed first and UEFI-1803 then needed four further machine fixes on top of
  it, all of them PCI-adjacent.
- **A second, more demanding consumer changes what "the transport works" means.**
  Every one of phase 3's four gaps was a register Linux does not read, does not
  write, or is more forgiving about. Two of them (`interrupt_line`, pin 8) are
  latent in the virtio-mmio path as well and were only found because EDK2 hit
  them first. The lesson for the remaining device work is that "Linux boots" is a
  necessary and distinctly insufficient acceptance criterion.
- Choosing PVH over the reset vector means we never emulate a CMOS, an RTC
  memory-sizing convention or fw_cfg — the "No QEMU anywhere" rule survives
  contact with UEFI.
- **A VM now has state outside its disk.** `[boot] nvram` is a second per-VM
  file, and it is not optional for an installed UEFI guest: delete it and the
  machine boots to "no bootable option" with a perfectly good disk attached.
  Snapshots, cloning and `entangled disk` grew a second thing to copy, and the
  GUI's "delete machine" grew a second thing to remove.
- **The firmware build is no longer stock.** Two `SET` lines in
  `CloudHvDefines.fdf.inc` are rewritten before `build`, with an asserted
  pre-image and a post-build check of the compiled PCD, and the provenance file
  records the flash base. `ENTANGLED_FW_PFLASH=0` builds upstream's own
  firmware, which is what every UEFI-1801…1803 measurement was taken on and what
  `tests/boot/tests/uefi_nvram.rs` correctly fails against. Bumping `EDK2_TAG`
  onto a tree where those lines moved is a loud error rather than a firmware with
  RAM-only variables.
- **"No QEMU anywhere" is about the *process*, not the register conventions.**
  This device is deliberately bug-compatible with `pflash_cfi01`, because that is
  what OVMF's flash driver was written against: programs overwrite rather than
  AND, the cleared status is zero, and `0x50` returns to read-array mode. Where a
  real chip and QEMU differ, QEMU wins. Nothing links against QEMU, no QEMU
  process runs, and no fw_cfg exists — but a firmware written for one VMM carries
  that VMM's conventions with it, and pretending otherwise costs boots.
- **The install path depends on a firmware-visible console.** Typing the
  installer's command line into GRUB works only because the firmware and the
  bootloader share the UART. A future `VirtioGpuDxe` (or any firmware with a GOP)
  would make GRUB draw on the scanout instead, and the serial exchange would have
  to become a virtio-input one. The mechanism is deliberately small and in one
  place (`install_ubuntu::GrubScript`) for exactly that reason.
