# ADR-0003: UEFI firmware for Entangled Desktop — EDK2 CloudHv, entered via PVH

- Status: proposed
- Date: 2026-08-19
- Backlog: EPIC 18 (UEFI-1801 pflash/firmware mapping, UEFI-1802 firmware build+boot)
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
   at later.

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

### Still missing — the phase 2/3 gap map

Established from the sources, in the order the firmware will hit them:

| Gap | Why the firmware needs it | Where |
|---|---|---|
| **PCI host bridge at 00:00.0 with device ID `0x0d57`** | `PlatformPei` does `PciRead16 (OVMF_HOSTBRIDGE_DID)` over ports `0xcf8/0xcfc` and switches on the result; `CLOUDHV_DEVICE_ID = 0x0d57` (`OvmfPkg/Include/IndustryStandard/CloudHv.h`). Anything else reaches `default:` → `DEBUG_ERROR "Unknown Host Bridge Device ID"` → `ASSERT (FALSE)` | `OvmfPkg/Library/PlatformInitLib/Platform.c:365` |
| **ACPI tables + `rsdp_paddr`** | `AcpiPlatformDxe` installs the VMM's tables when the host bridge is CloudHv; the MADT is how the firmware learns the CPU topology. In PVH the RSDP address arrives in `hvm_start_info.rsdp_paddr` — we currently pass 0 | `OvmfPkg/AcpiPlatformDxe/AcpiPlatform.c:38` |
| **virtio over PCI** | CloudHv's FDF ships `VirtioPciDeviceDxe`, `Virtio10Dxe`, `VirtioBlkDxe`, `VirtioScsiDxe`, `VirtioNetDxe` — and **no** virtio-MMIO driver at all. Our only transport today is virtio-mmio (ADR-0001 §4). So UEFI-1803 ("boot an Ubuntu ISO from a read-only virtio-blk") is blocked on the post-MVP virtio-pci work, not on firmware | `OvmfPkg/CloudHv/CloudHvX64.fdf:225–235` |
| **Writable pflash / NVRAM** | The variable store sits at `VARS_OFFSET = 0` of the flash device, `VARS_SIZE = 0x84000`. Phase 1 maps the ROM read-only, so `KVM_MEM_READONLY` turns firmware writes into MMIO exits that the bus drops: variables silently do not persist. A real pflash device (status/command state machine, write buffering, a separate NVRAM file per VM) is the follow-up. Note this affects the reset-vector path only — the PVH path loads into RAM and is writable by construction | `OvmfPkg/Include/Fdf/OvmfPkgDefines.fdf.inc`, `CloudHvDefines.fdf.inc` |
| **ACPI PM timer at 0x0608, shutdown at 0x0600** | `AcpiTimerLib` uses `CLOUDHV_ACPI_TIMER_IO_ADDRESS`; the firmware stalls on it during DXE | `OvmfPkg/Include/IndustryStandard/CloudHv.h` |
| **MMIO hole agreement** | The firmware hard-codes the CloudHv 32-bit aperture as `0xc000_0000 + 0x3800_0000`. Our `layout::MMIO_HOLE_START` is already `0xc000_0000` and the virtio window at `0xd000_0000` sits inside it, so this one is free — but it pins the layout | `OvmfPkg/Library/PlatformInitLib/MemDetect.c:61` |

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
  regardless of which of the two firmwares we pick.
- Choosing PVH over the reset vector means we never emulate a CMOS, an RTC
  memory-sizing convention or fw_cfg — the "No QEMU anywhere" rule survives
  contact with UEFI.
