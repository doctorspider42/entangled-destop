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

## Amendment, 2026-10-30: Windows Setup on WHP, and what it actually took

The Windows 10 installation ISO now boots to the first Setup screen
("Windows Setup — Language to install") on the Windows/WHP host, 2 vCPUs, in
the window, with no virtio-win drivers and no Hyper-V enlightenments. §2 was
**not** a blocker for getting this far. What was, in the order it was found:

1. **No display (§1, as predicted).** `machine_x86::framebuffer`: the
   Bochs-DISPI function, VRAM as a 64-bit prefetchable BAR backed by a
   `ShmWindow`, registers in BAR **3**, not 2: EDK2's `PciBusDxe` numbers BARs
   logically, so with a 64-bit pair at 0–1 `QemuVideoDxe`'s `PCI_BAR_IDX2` is
   the register at `0x1c`. The EDID's preferred timing becomes the GOP mode
   through `PcdVideoResolutionSource`, which CloudHv has to declare dynamic.
   `[display] adapter = "framebuffer"` selects it. `framebuffer_present` polls
   VRAM at the refresh rate and sends only the changed rows.
2. **No keyboard in the firmware.** CloudHv ships no keyboard driver, so its
   ConIn was the serial terminal only, and bootmgr's "Press any key to boot
   from CD or DVD" timed out. `VirtioKeyboardDxe` goes in with
   `QemuVideoDxe`. It still did not work, because it never enables
   virtio-input's `statusq`, and our transport refused to activate a device
   with a disabled queue. Virtio 1.2 §4.1.4.3 allows a driver to leave queues
   it does not use disabled. `QueueConfig::build_for_activation` now builds a
   never-ready queue for them, for every device.
3. **`HAL_INITIALIZATION_FAILED` (0x5C, 0x110).** Without an HPET, Windows'
   HAL takes the **RTC's periodic interrupt** as its clock: it sets register
   A's rate and `PIE` and waits for IRQ 8. Our RTC never interrupted. It now
   has `PIE`/`UIE`, register C flags cleared on read, and an `RtcTimer`
   thread, on IOAPIC pin 8 (WHP) or an irqfd on GSI 8 (KVM).
4. **`KMODE_EXCEPTION_NOT_HANDLED` (0x1E, `0xC0000096`).** `#GP` on `wrmsr`
   to AMD's PerfEvtSel0–3 (`0xC0010000–3`), which WHP does not implement.
   Further on, Windows also touches `0xC0011023`, `0xC001102C` (erratum and
   speculation controls) and `0xC0010010` (SYSCFG). `WhpOptions::ignore_msrs`
   (KVM's `ignore_msrs`: read 0, drop writes, log once) gets past them. For
   now it is switched on by `ENTANGLED_WHP_IGNORE_MSRS=1`; a Windows guest
   needs it as a profile setting.
5. **`ACPI_BIOS_ERROR` (0xA5, 0x11, 3: cannot load the RSDT).**
   `InstallCloudHvTables()` installs every table in our XSDT and the DSDT,
   but never the FACS, so `AcpiTableDxe` zeroes `FIRMWARE_CTRL`. Cloud
   Hypervisor's machine is hardware-reduced and needs no FACS. Ours is not,
   and Windows refuses it. `build-cloudhv.sh` patches `CloudHvAcpi.c` to
   install the FACS the FADT names. Linux had only warned.

How these were found is worth keeping, because §3 said the work would be
empirical. `$ENTANGLED_WHP_TRACE_EXITS` now prints the port, GPA or `cpuid`
leaf with each exit. A cancelled run (pause/resume) or a write to `0xcf9`
also dumps the registers, the code and stack around RIP, the nearest export of
the guest PE image it is in, and, for a Windows kernel, the stop code from the
exported `KiBugCheckData`. Every stop above was read that way, before the
display could show anything.

Still open for phase A: Hyper-V enlightenments (performance, and the 0x101
risk under host load); `ignore_msrs` as a guest setting rather than an
environment variable; the unattended path (§4); KVM, which needs the same
RTC/FACS fixes and is untested here; and the boot framebuffer's VRAM in
snapshots (refused for now, §6).

### Correction to §4: Windows PE sees no virtio device at all

§4 put `autounattend.xml` on "a small FAT volume we control". That cannot
work. Before a driver loads, Windows PE sees **no** virtio-blk device: not
the target disk, not the virtio-win ISO, not an answer-file volume, and not
the installation ISO it booted from (the firmware read that, not Windows).
The only thing Setup can read at that point is its own `boot.wim`. So
`guest/windows/remaster-iso.sh` puts the virtio-win drivers into image 2 of
`boot.wim` as `X:\drivers`, and the answer file as `X:\sources\
Autounattend.xml`, which Setup's implicit search reads in the windowsPE pass.
The ISO is rebuilt around `efisys_noprompt.bin`, so there is no "press any key"
either. It is development tooling (wimlib, genisoimage, run in a container),
not a product path. `entangled install windows` needs either the same edit
done in Rust, or a boot medium Windows has an inbox driver for (AHCI/ATAPI
or USB mass storage), and that choice is still open.

Two further things stood between that and a desktop, both found on
2026-10-31:

- **A UDF image on a virtio-blk disk is not installation media.** With
  `viostor` loaded (every virtio function is re-activated by a Windows driver
  after `ExitBootServices`, which is how that was confirmed), Setup still
  said "A media driver your computer needs is missing": the installation ISO
  is a *disk* to Windows, not a CD-ROM. `remaster-iso.sh OUT.img` therefore
  writes the tree to an MBR disk with one FAT32 partition, the shape of a USB
  stick. `install.esd` (over 4 GiB) becomes the chosen edition as a split WIM
  (`install.swm`, `install2.swm`). The firmware boots that disk through
  `\EFI\BOOT\BOOTX64.EFI`, and Setup takes it as its media.
- **The first boot from disk died on a 10.8 MB read.** EDK2's `VirtioBlkDxe`
  never splits a `BlockIo` transfer, and Windows Boot Manager asked for 10.8 MB
  at once. virtio-blk capped one request at 4 MiB because the whole payload
  was staged in host memory. It now stages each segment 4 MiB at a time
  (`STAGING_CHUNK`), and the per-request cap is 128 MiB.

With those, `win10.toml` (target disk first, then the FAT32 installation disk,
usernet, `adapter = "framebuffer"`, `ignore_msrs = true`) installs Windows 10
Pro **with no interaction**: Setup, the copy, the reboot into the installed
system, specialize, OOBE, and auto-logon to a desktop with NetKVM networking.
Consumer media also refuse an answer file with no `<ProductKey>` element; an
empty key with `WillShowUI` `Never` satisfies it, so the system is installed
unactivated.

### Amendment, 2026-10-31: §2 on this host, and where the time goes

The installed Windows 10 desktop, idle, in a 2-vCPU guest on WHP, measured
with `$ENTANGLED_WHP_EXIT_STATS` (exit counts per vCPU, cheap enough not to
change what it measures): **~10 000 exits a second on vCPU 0**, almost all
port I/O. About 4 000/s are the ACPI PM timer (`0x608`, the
performance counter) and about 3 000/s each are `0x70`/`0x71` (the RTC's
periodic interrupt, the system clock, and the register C read that
acknowledges it). Each of those is a full exit served in this process. That
is the most likely cause of the uneven frame pacing and the generally sluggish
feel the first interactive session showed.

§2's answer was the Hyper-V enlightenments. `WhpOptions::hyperv` /
`hyperv_enlightenments = true` now asks WHP for them
(`WHvPartitionPropertyCodeSyntheticProcessorFeaturesBanks`, `HYPERV_FEATURES`
intersected with what the host reports), and the CPUID policy then leaves the
hypervisor leaves to WHP. **This host cannot use it**: on Windows 10 22H2
(19045) `WHvCapabilityCodeSyntheticProcessorFeaturesBanks` fails with
`WHV_E_UNKNOWN_CAPABILITY`, so a profile that asks is refused with that
error. It needs a newer Windows host and is untested.

Reporting ARAT (`0x6.EAX[2]`) and an invariant TSC (`0x8000_0007.EDX[8]`)
in CPUID did not move Windows off the PM timer and the RTC (the same ~11 000
exits/s), so that experiment was not kept. What remains for a Windows 10
host is ours to build. The options are a reference-TSC page served from
userspace (Hyper-V's `HV_X64_MSR_REFERENCE_TSC`, which makes the performance
counter a memory read), and cheaper exits for the two ports.
