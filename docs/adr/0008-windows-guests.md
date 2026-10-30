# ADR-0008: Windows guests — what the machine owes a guest that is not Linux

- Status: proposed; nothing implemented
- Date: 2026-10-30
- Extends: ADR-0002 (host portability), ADR-0003 (UEFI firmware), ADR-0005
  (lifecycle), ADR-0006 (suspend/restore)

## Objective

`entangled install windows --iso <user-supplied ISO>` installs Windows 10 or
Windows 11 (with its hardware checks bypassed) unattended, and `entangled run`
boots the installed system to a desktop in the window, with keyboard, absolute
pointer, disk and network, on **both** x86 hosts (Linux/KVM and Windows/WHP).
2D only. Shutdown from inside the guest ends the run the way it does for Linux.

The user guide says today, correctly, that nobody has booted a Windows guest
and that it should be assumed not to work (`docs/user-guide.md`, "Not a
Windows-guest solution"). This ADR states what closing that gap costs, in
which order, and what stays out of reach.

## Context: what is already there

Everything a Windows guest needs from the *core* already exists, because the
UEFI work (ADR-0003, EPIC 18/19) built it for Ubuntu:

| Windows needs | Have | Where |
|---|---|---|
| UEFI with persistent variables (`bootmgfw.efi` is an EFI application; `BootOrder` must survive) | yes | EDK2 CloudHv via PVH, CFI pflash NVRAM — `uefi-boot`, `machine_x86::pflash` |
| A PCI transport the virtio-win drivers understand (they are PCI-only) | yes | virtio-pci with MSI-X — `virtio_core::pci`, `machine_x86::virtio_pci` |
| Boot from an El Torito ISO | yes | `run --cdrom` / `[cdrom]` attaches the ISO read-only as the last virtio-blk device; the firmware's `VirtioBlkDxe` + `PartitionDxe` boot it |
| ACPI with a FADT, MADT, DSDT, PM timer, `\_S5` and an SCI | yes | `machine_x86::acpi` (FADT is *not* HW-reduced) |
| LAPIC/IOAPIC, PIT, RTC, SMP | yes | in-kernel on KVM; WHP xAPIC emulation + `machine_x86::irqchip` on WHP; `machine_x86::rtc` |
| A CPUID policy per host | yes | `vmm_core::vcpu` (KVM), `vmm_core::whp::cpuid` (WHP) |

So this is not a new machine. It is a set of platform pieces that Linux
tolerates the absence of and Windows does not.

## Decision

Windows is reached through UEFI only; there is no direct-boot path and no
legacy BIOS. The installer path uses the **signed virtio-win drivers**, so
the machine stays virtio-first and gains as little legacy emulation as
possible. Each gap below is closed as its own change, in the order given,
with a test that fails before it.

### 1. A firmware display that survives `ExitBootServices` (blocker)

ADR-0003 records that CloudHv ships no `VirtioGpuDxe`: the firmware has no
GOP, so firmware and boot loader output reaches only ttyS0. Linux does not
care. Windows does: `bootmgfw`, Windows Setup and the in-box `BasicDisplay`
driver all draw into the **linear framebuffer that GOP reported**, and they
keep drawing into it after `ExitBootServices`. Even adding `VirtioGpuDxe`
would not help, because it is a `PixelBltOnly` GOP with no `FrameBufferBase`;
Windows would boot to a black window.

Decision:

- Add a minimal PCI display function with the Bochs/DISPI register
  interface: `BAR0` = VRAM (prefetchable, mapped as guest memory, not MMIO
  exits), `BAR2` = the DISPI registers. The register interface is a published
  de-facto standard; no QEMU code is used or linked.
- Add `OvmfPkg/QemuVideoDxe` to `guest/firmware/build-cloudhv.sh`. It is an
  EDK2 driver (BSD-2-Clause-Patent) that binds that device and produces a
  GOP with a real framebuffer. The name is EDK2's; it involves no QEMU.
- The `display` crate gains a second scanout source: the VRAM BAR, read at
  presentation time. The device is a peer of virtio-gpu, not part of it.
- Default mode 1920×1080×32, selected by the firmware, so `BasicDisplay`
  inherits it.

After installation, `viogpudo` (virtio-win's virtio-gpu display-only driver)
is the guest's way to resize. Using it is optional and not part of the
acceptance.

### 2. Hyper-V enlightenments (practically a blocker)

Without them Windows runs, but slowly, and under host contention it is prone
to `CLOCK_WATCHDOG_TIMEOUT` (0x101) bugchecks because an IPI target vCPU did
not run in time. Required set, matching what Windows actually checks:
relaxed timing, VP index, the reference TSC page, synthetic timers (needs
SynIC), APIC EOI/ICR/TPR through MSRs, and the `HV#1` interface signature.

- **KVM**: the kernel implements all of them. The work is setting the
  `0x4000_0000`+ CPUID leaves and enabling `KVM_CAP_HYPERV_SYNIC2`. This is
  pure policy in `vmm_core`.
- **WHP**: today `vmm_core::whp::cpuid` deliberately *zeroes* the Hyper-V
  leaves so that Linux takes the plain path. A Windows guest needs the
  opposite: the partition exposes them through the
  `SyntheticProcessorFeaturesBanks` partition property, and the leaves come
  from Hyper-V itself.

The choice is per guest, so it becomes a VM config switch (`[cpu]
enlightenments = "hyperv"`, implied by `install windows`). That is an
additive option on each backend, as ADR-0002 requires, and no change to
either backend's public API. The Linux default stays exactly as it is.

### 3. ACPI and MSRs that Windows reads and Linux does not

Windows' ACPI driver bugchecks (0xA5) on tables Linux accepts. What we know
already differs from what Windows expects:

- `\_SB.PCI0` is `PNP0A03` with a `_CRS` but no `_PRT`. Windows wants a
  `_PRT` for INTx even if every device we ship uses MSI-X; without it, any
  function that falls back to INTx is left without an interrupt.
- There is no `_OSC`, no MCFG, and no `PNP0A08`. Legacy config ports are
  enough for Windows, but this is the first place to look if `pci.sys`
  refuses the bridge.
- There is no power button device and no RTC device (`PNP0B00`) in the
  DSDT. The FADT describes them, but Windows also enumerates them as ACPI
  devices.

MSRs: the WHP partition enables CPUID exits only (`ExtendedVmExits` = CPUID),
so an MSR WHP does not know raises `#GP` inside the guest. KVM behaves the
same unless `ignore_msrs` is set. Windows probes many model-specific MSRs and
tolerates `#GP` on most of them, but not all of them. The MSRs are discovered
empirically: enable the `X64MsrExit` bit, log the first-boot MSR traffic, and
emulate only what Windows insists on.

This is the least predictable part of the plan; estimates for it are guesses
until the first boot log exists.

### 4. An unattended install path

Windows Setup (WinPE) has no virtio drivers, so it cannot see the target disk
or the network. Decision: **do not emulate AHCI/IDE for this.** Instead:

- `entangled fetch virtio-win` downloads the virtio-win driver ISO with a
  pinned SHA-256, like every other artifact. The drivers are BSD-3-Clause in
  source, but must be the Red Hat-signed binaries, because Windows refuses
  unsigned kernel drivers. They are fetched, never redistributed in the
  installer.
- `entangled install windows` generates an `autounattend.xml` on a small FAT
  volume we control. Its `Microsoft-Windows-PnpCustomizationsWinPE` section
  loads `viostor`, `netkvm` and `vioinput` from the virtio-win ISO; its
  `windowsPE` pass partitions the target disk (GPT, ESP + MSR + system); and
  for Windows 11 it sets the LabConfig bypasses (TPM, Secure Boot, RAM, CPU).
- The Windows ISO is always supplied by the user. We do not download it
  (licence terms and an interactive Microsoft download page).

### 5. A keyboard before drivers exist

Until `vioinput` is loaded, Windows has no input device. An unattended install
never needs one, but recovery screens and `bootmgr`'s "press any key" do. An
8042 controller with a PS/2 keyboard (no mouse) at `0x60`/`0x64`, IRQ 1,
closes this. It also gives the guest's reboot ladder its first rung, the one
the `acpi/mod.rs` comment says is missing today. The FADT's `8042` flag and a
`PNP0303` device in the DSDT are part of the same change. This step is second
priority; the acceptance in phase A does not depend on it.

### 6. What every new device owes

The Bochs display and the 8042 are new devices. Each owes a `reset()`, a
`Quiesce` gate if any host thread of its own touches guest memory (the
display's presenter reads VRAM, so yes), and a `save_device`/`load_device`
pair, according to ADR-0005 and ADR-0006. The VRAM is guest-visible state that
a snapshot must carry. Enabling the enlightenments adds Hyper-V MSR state (the
reference TSC page GPA, SynIC and stimer registers) to the per-host CPU state
in ADR-0006. Until that state is saved, a snapshot of a VM with
enlightenments is **refused**; it is never silently written incomplete.

## Phases

| Phase | Contents | Acceptance |
|---|---|---|
| **A — first desktop** | §1, §2, §3, §4 | `install windows --iso Win10.iso --auto` completes unattended on WHP and on KVM; `run` reaches the desktop; network reaches the internet through user-mode networking; guest shutdown ends the run; 10 consecutive boots without a bugcheck |
| **B — comfort** | §5 i8042; `viogpudo` resize; `[[share]]` through a new `virtio-fs` device (virtio-win has `viofs`, not 9p); virtio-snd through `viosnd` (experimental upstream) | each item demonstrated in a real guest, not just enumerated in Device Manager |
| **C — Windows 11 proper** | TPM 2.0 (a new device + an emulator; no permissive Rust implementation exists today), Secure Boot with enrolled Microsoft keys | Windows 11 installs *without* LabConfig bypasses |

Phase A is the decision this ADR asks for. Phases B and C are recorded so that
nobody mistakes A for the whole job.

Start on WHP. The development host here (Windows 10, AMD) has no `/dev/kvm`
under WSL, and a Windows guest on Hyper-V is the configuration Microsoft
itself tests most. KVM follows with the same profile.

## Out of scope

- **3D acceleration in Windows guests.** No WDDM driver for Venus or VirGL
  exists, and writing one is a multi-year project of its own. ADR-0004 and
  ADR-0007 stay Linux-guest decisions. A Windows guest is 2D only.
- **Windows on ARM on the macOS host (ADR-0007).** That is a different
  machine (`machine-arm64`), a different firmware and a different driver set;
  this ADR is x86 only.
- **Legacy BIOS, AHCI/IDE, emulated NICs (e1000/rtl8139).** Not needed by the
  virtio-win path; added only if a concrete guest cannot do without them.
- **Free page reporting in the guest.** virtio-win's balloon driver does not
  implement it, so `reclaim_memory = true` does nothing for Windows.

## Consequences

- The machine gains its first non-virtio guest-visible devices since the UART
  and RTC: the Bochs display and later the 8042. Both are small, and both
  sit under the same reset/quiesce/save rules as everything else.
- The CPUID policy stops being one-per-host and becomes one-per-host-per-guest-
  family. The Linux policy must remain byte-identical; a test pins it.
- A new external artifact (virtio-win) joins the pinned-digest set, and the
  fresh-install acceptance has to cover it.
- CI cannot install Windows without a licensed or evaluation ISO. The Windows
  acceptance runs on demand against an evaluation image, the same way the KVM
  tests run only where `/dev/kvm` exists.
- `docs/user-guide.md` keeps saying "no Windows guests" until phase A passes
  its acceptance.
