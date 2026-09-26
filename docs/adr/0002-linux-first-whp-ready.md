# ADR-0002: Linux-first, WHP-ready — the native Windows port path

- Status: accepted
- Date: 2026-08-18
- Extends: [ADR-0001](0001-mvp-architecture.md)

## Context

The MVP targets Linux x86-64 hosts on KVM. A native Windows host build is a
plausible future direction: Windows exposes a KVM-like userspace hypervisor
API — **WHP (Windows Hypervisor Platform)** on top of Hyper-V, used by QEMU's
WHPX accelerator, VirtualBox and the Android emulators, and available on both
Windows Pro and Home. Its execution model maps closely onto ours: a partition
per VM, `WHvRunVirtualProcessor` as the run loop, exit records for port I/O
and MMIO that translate almost 1:1 to `vmm_core::ExitHandler`.

This ADR records the decision to keep the codebase portable enough that the
port stays a **bounded backend project (~4–8 engineering weeks), not a
rewrite**, and pins down the rules that keep it that way.

## Decision

1. **Linux/KVM remains the only supported host through the MVP.** No WHP code
   lands before the MVP ships; this ADR only constrains design so the option
   stays cheap.
2. **Everything above the hypervisor stays host-agnostic.** The expected port
   surface, kept current as the code evolves:

   | Layer | Port work |
   |---|---|
   | `virtio-core` + device crates (blk, gpu, input, net protocol) | None — transport- and OS-agnostic by rule; `Interrupt` already abstracts the delivery mechanism |
   | `display` (winit/wgpu) | None — builds on Windows today (native DX12/Vulkan instead of WSLg's llvmpipe) |
   | `debian-media`, `control-api`, `entangled` | None — pure Rust + rustls, no OS gates |
   | `vmm-core` | A second hypervisor backend behind a trait: partition/vCPU creation, `WHvMapGpaRange`, register/CPUID setup, run-loop exit translation (~1–2 weeks) |
   | Interrupt chip + PIT | The real gap: KVM gives us an in-kernel PIC/IOAPIC/PIT; WHP provides only the local APIC, so PIC/IOAPIC routing and the PIT must be emulated in userspace, as QEMU/WHPX does (~1–2 weeks) |
   | Networking | TAP does not exist on Windows and wintun/tap-windows6 are GPL (blocked). The plan is a user-mode NAT (slirp-like) backend on `smoltcp` (0BSD) — which also gives rootless networking on Linux (~2–3 weeks) |
   | Bootstrap kernel/initramfs | None — build-once Linux artifacts distributed as files |

3. **Rules that protect the port** (additions to CLAUDE.md hard rules in
   spirit; enforced in review):
   - Interrupt delivery is always behind a trait (`virtio_core::Interrupt`,
     the serial trigger); no `EventFd`/irqfd types in device logic.
   - No new `#[cfg(target_os = "linux")]` outside `vmm-core`, TAP code and
     eventfd plumbing; protocol/validation/config logic must build everywhere.
   - Host networking backends are pluggable; nothing may assume TAP is the
     only backend (the config format already names `backend = "tap"`).
   - No dependency that is Linux-only *by license or design* may become
     load-bearing above `vmm-core`.

## Amendment (2026-08-19): first native Windows build findings

Measured with the x86_64-pc-windows-gnu toolchain on the dev machine:

- `control-api` and `debian-media` compile natively today, unchanged.
- Everything touching guest memory fails on one dependency: **`vm-memory`'s
  mmap backend is unix-only** in current releases (0.17/0.18). The vm-memory
  *traits* are portable; only the backing implementation is not. The WHP
  port therefore includes a `GuestMemoryWindows` region type
  (VirtualAlloc-backed, implementing `GuestMemory`/`GuestMemoryRegion`) plus
  a small alias switch in vmm-core (`GuestMem` becomes per-OS).
- `display` is blocked only transitively (it imports two format constants
  from virtio-gpu); if needed it can be decoupled in minutes.
- The register seam from WHP-1701 (`vmm_core::hv`) is in place: machine
  setup code no longer touches kvm_bindings.

## Amendment (2026-08-19, later): EPIC 17 phase 1 delivered

WHP-1701 and WHP-1702 are done and verified on a host with the "Windows
Hypervisor Platform" optional feature enabled. A real-mode guest runs
machine code under WHP, its port write reaches `ExitHandler`, and it
terminates; 100 partition create/destroy cycles leak nothing. Details and
the full phase-2 list live in `.claude/skills/whp-backend/SKILL.md`.

**Correction to the amendment above.** The "mmap backend is unix-only"
finding was wrong, and the `GuestMemoryWindows` it prescribed was not
written. vm-memory's mmap backend *does* ship a Windows implementation
(`VirtualAlloc(MEM_COMMIT, PAGE_READWRITE)` / `VirtualFree`, with
`get_host_address()` yielding exactly the pointer `WHvMapGpaRange` wants).
What is unix-only is vm-memory's **default `rawfd` feature**, whose
fd-based `ReadVolatile`/`WriteVolatile` impls upstream refuses to build on
Windows with a `compile_error!`. Nothing in this workspace uses those
impls, so the workspace manifest turns `rawfd` off everywhere and
`vmm_core::GuestMem` stays *one type on both hosts* — no per-OS region
type, no new `unsafe`, no divergence for guest-memory consumers. The alias
remains so a future divergence (huge pages on Linux, `MEM_WRITE_WATCH`
dirty tracking on Windows) is a one-line change.

Native Windows build status measured after the change: `vmm-core`,
`machine-x86`, `linux-boot`, `control-api` and `debian-media` compile and
test (25 tests in `vmm-core` alone). The `virtio-*` crates and `display`
still do not, for a reason worth recording because we cannot fix it
locally: **`virtio-queue` 0.17 depends on `vm-memory` without
`default-features = false`**, so it re-enables `rawfd` for the whole
dependency graph and the `compile_error!` fires again. Cargo features are
additive, so no manifest edit on our side can subtract it. Resolved on
2026-08-19 (user's call): a vendored, minimally-patched copy lives in
third_party/virtio-queue (see its VENDORED.md), wired via [patch.crates-io].
With it, the whole virtio stack and `display` compile natively on Windows;
an upstream rust-vmm PR remains the endgame.

What phase 1 delivered, against the port surface table above:

| Layer | Status |
|---|---|
| `vmm-core` second backend | **Done** — `whp` module: partition lifecycle, `WHvMapGpaRange`, `VcpuRegisters` over `WHvGet`/`SetVirtualProcessorRegisters`, run loop with port-I/O emulation, stop via `WHvCancelRunVirtualProcessor` |
| Interrupt chip + PIT | Still the real gap (WHP-1703). Unchanged assessment |
| MMIO | New finding: WHP's `MemoryAccess` exit carries neither access width nor data, so virtio-mmio needs WHP's own instruction emulator (`WHvEmulatorTryMmioEmulation`). Structured and reported as a typed error today |
| Networking | Unchanged (WHP-1704, smoltcp NAT) |

Two platform facts found by measurement, both with design consequences:

- **`WHV_UINT128` is `DECLSPEC_ALIGN(16)` in the WHP headers, but the
  `windows` crate's generated binding drops the alignment.** A register-value
  buffer that is only 8-byte aligned makes WHP fault with
  `STATUS_ACCESS_VIOLATION` *inside* the call. The backend forces alignment
  and rejects misaligned buffers with a typed error.
- **WHP allows only one partition per host process to have guest memory
  mapped at a time** (`WHvMapGpaRange` → `0xC0370008` for the second one).
  KVM has no such limit, so this is a genuine behavioural difference: a
  multi-VM `entangled` on Windows needs one process per VM. Worth settling
  before the control API grows a multi-VM surface (EPIC 12).

Dependency added: `windows` 0.62 (Microsoft, MIT OR Apache-2.0), feature
`Win32_System_Hypervisor`, only under `[target.'cfg(windows)'.dependencies]`.
`cargo deny check` passes.

## Amendment (2026-08-19, phase 2): a Linux guest boots on WHP

WHP-1703 is done. `cargo test -p vmm-core --test whp_boot` boots
`artifacts/bootstrap/vmlinuz` with the marker initramfs to
`VMHOST_GUEST_READY` on Windows, headless, in ~3.5 s (debug build), and tears
down cleanly. Design detail lives in `.claude/skills/whp-backend/SKILL.md`; this
records what the ADR itself has to change its mind about.

**The userspace irqchip is machine code, not backend code.** The phase-1
assessment called PIC/IOAPIC/PIT "the WHP gap", which invited putting them in
`vmm-core::whp`. They are in `machine-x86::irqchip` instead, portable and
unit-tested on both hosts, because a redirection table, a counter driven by a
clock and an 8259 register file are the machine's devices — the same category as
the 16550 beside them. Exactly one step is genuinely hypervisor-specific: handing
a decoded interrupt message to a local APIC. That is now a one-method seam,
`vmm_core::hv::InterruptDelivery`, which **KVM deliberately does not implement**
(its IOAPIC is in the kernel and irqfds reach it without userspace). The rule in
the Decision section — "interrupt delivery is always behind a trait" — therefore
gains a second trait, at the other end of the same path.

**The seam that was declared unnecessary was necessary after all.** `hv.rs` said
the seam needed nothing for interrupts because `virtio_core::Interrupt`/`IrqLine`
and the serial trigger already abstracted delivery. Half true: those cover the
*device* side. The serial console's trigger was not abstract at all — it held an
`EventFd` and registered its own irqfd — so it now holds an `Arc<dyn IrqLine>`,
the trait virtio devices already used, with the eventfd path moved intact into
`machine_x86::irqfd::IrqFdLine`. The Linux behaviour is unchanged;
`IrqLine::trigger` is the non-blocking edge `EventFd::write(1)` was.

**`hlt` means something different on the two backends.** KVM with an in-kernel
irqchip emulates `hlt` in the kernel — the vCPU blocks inside `KVM_RUN` and
userspace never sees it. WHP always reports it, and a Linux guest executes `hlt`
on every trip through `default_idle()`. So the WHP run loop treats `hlt` as an
idle wait on a `HaltGate` bumped by every injection, not as an outcome. It
reports `RunOutcome::Halted` only when local APIC emulation is off, where nothing
could ever wake the CPU — which is the phase-1 behaviour the real-mode smoke
guests depend on, and why the new capabilities are opt-in through `WhpOptions`
rather than switched on for everyone.

**A guest must not be told it is on Hyper-V.** A WHP partition runs on Hyper-V,
and the hypervisor CPUID leaves can carry the `"Microsoft Hv"` signature. A Linux
guest that sees it starts using Hyper-V synthetic MSRs and enlightenments a WHP
exo-partition does not implement. The backend zeroes leaf `0x4000_0000` while
keeping the hypervisor-present bit (parity with KVM), so the guest finds no
hypervisor interface and takes the architectural paths — TSC and the 8254, no
kvmclock, no Hyper-V clocksource. This is the one place the two backends'
guest-visible CPUID legitimately differs, and it is deliberate.

Two further platform facts, both measured:

- **`CpuidResultList` is the wrong API for editing a leaf.** It takes *complete*
  results and there is no call that reports what WHP would otherwise have
  returned, so using it means inventing every bit of a leaf including the feature
  bits WHP masks for its own reasons. `CpuidExitList` +
  `ExtendedVmExits.X64CpuidExit` produces an exit whose context carries
  `DefaultResultRax..Rdx`, which makes the policy a *diff* — the same shape as the
  KVM path's edit of `KVM_GET_SUPPORTED_CPUID`.
- **`WHvEmulatorTryMmioEmulation` refuses 16-bit real mode**, failing with
  `internal emulation failure` (status `0x2`) without ever invoking a callback.
  Long mode works. Not a problem for a Linux or UEFI guest, but it does mean the
  emulator cannot be smoke-tested with a real-mode guest the way the port-I/O
  path is.

The 16-byte alignment rule from phase 1 extends further than stated: it also
applies to the register-value buffer **WHP hands us** in the emulator's register
callbacks, which has no alignment guarantee of its own. Both callbacks stage
through an over-aligned buffer.

`third_party/linux-loader` joins `third_party/virtio-queue`: the same
`default-features = false` on `vm-memory` one crate further along the graph
(see its VENDORED.md). With `rawfd` off, `File` has no `ReadVolatile` impl, so
`linux_boot::load` reads the image through a `Cursor<Vec<u8>>` — portable, and one
transient copy the initramfs path already paid. One upstream rust-vmm PR would
retire both vendored copies.

Native Windows status after phase 2: `vmm-core`, `machine-x86`, `linux-boot`,
`control-api`, `debian-media`, the `virtio-*` crates and `display` all build and
test. What is still Linux-only above `vmm-core` is the *device wiring* —
`machine_x86::irqfd`, `notify`, `virtio`, `virtio_pci` — because irqfds and
ioeventfds are KVM concepts; attaching the same devices through the userspace
irqchip is phase 3.

## Amendment (2026-08-19, phase 4): `entangled run` is native on Windows

EPIC 17 is product-complete: `entangled run <profile>` works natively on a
Windows host — window, virtio-gpu scanout, input, disks on either transport,
user-mode networking, UEFI firmware with persistent NVRAM, ACPI S5 shutdown.
The measured evidence and the phase list live in
`.claude/skills/whp-backend/SKILL.md`; this records what the ADR has to change
its mind about, and the two platform facts that cost the time.

**The run path splits per host exactly once.** `apps/entangled`'s `run_vm`
keeps one shared body — config, devices, presentation, supervision, reporting —
and one per-OS `host::start()` doing machine assembly. The differences inside
it are the ones this ADR already predicted (in-kernel chips vs.
`machine_x86::irqchip`, irqfd/ioeventfd vs. synchronous kicks, `sigaction` vs.
`SetConsoleCtrlHandler`) plus one it did not: **register setup is BSP-only on
WHP** in every boot mode, because WHP has no INIT of its own to discard host
writes and an AP touched by the host never leaves wait-for-startup.

**virtio-pci needed no ioeventfd substitute, and MSI-X needed no WHP MSI API.**
The phase-3 assessment ("its notification area follows a guest-programmable BAR,
which needs the ioeventfd rebasing KVM has") dissolved rather than got solved:
with synchronous kicks every notify is decoded against the BAR's *current* base
by `PciRoot::locate_mmio`, so there is nothing registered at an absolute address
and nothing to rebase. MSI-X delivery is `machine_x86::msi::decode_msi_message`
— the architectural address/data decode, portable and unit-tested on both hosts
— feeding the same `InterruptDelivery` seam the IOAPIC uses. `virtio_core::msix`
still only knows how to produce a message (the phase-3 rule held).

**The instruction emulator serves guest RAM, not just devices.** WHP's emulator
routes *every* memory operand of a faulting instruction through the memory
callback — not only the device window that faulted. The firmware's `CopyMem`
out of the pflash window is `rep movs` from MMIO into RAM, and a callback that
only knew devices silently dropped the RAM half: EDK2 copied its own variable
store as zeroes and reported the NVRAM volume corrupt. The callback now serves
RAM-backed GPAs from `GuestMem` (checked accessors) and falls through to the
device bus for the rest. No Linux boot could have caught this — a kernel never
points a memory-to-memory instruction at a device window.

**`reboot=k` is not a way to end a WHP VM.** The test guests' triple-fault
ending (`reboot(RESTART)` with `reboot=k`) reaches KVM as `KVM_EXIT_SHUTDOWN`;
WHP with local APIC emulation absorbs the reset instead of reporting an exit,
and the vCPU parks. Guest-initiated shutdown on WHP is the ACPI S5 path, which
both hosts already turn into a clean stop through the PM-block latch. Related
and fixed on both backends: `join_or_stop` now stops the remaining vCPUs as
soon as *any* vCPU thread finishes — no run loop returns while its guest is
healthy, and waiting for a triple-faulted BSP's parked APs hung the supervisor
on a machine that was already dead.

What phase 4 delivered, against this ADR's original port-surface table: every
row is closed. The one behavioural difference that remains product-visible is
the one-mapped-partition-per-process limit, which `entangled-manager` already
respects by driving one CLI process per VM.

## Amendment (2026-08-20, phase 5): `entangled install` is native on Windows

`entangled install ubuntu --auto --headless` completes on a plain Windows host
and the disk it produces boots — measured on this machine, debug build:
**4 min 27 s** from the command to `installed: GPT with an ESP on /dev/vda1
(953 MiB) and root on /dev/vda2 (ext4 UUID …)`, ending in the guest's own ACPI
S5 and a written profile whose paths are Windows paths. The port surface table
at the top of this ADR is now closed for the *whole* product surface, not just
`run`.

**Almost nothing had to be replaced, because almost nothing was Linux-specific.**
The gate said "requires a Linux host with KVM (on Windows use WSL2)"; behind it
were three files whose content is *logic over bytes* — the newc cpio the d-i
preseed rides in, the ISO9660 NoCloud seed, the GRUB-over-serial typing, the
partition-table inspection, the profile writer. None of it mounts anything: the
installer writes the disk from inside the guest, and the host only ever *reads*
partition tables (`crates/disk-image`, portable since the disk-management work).
So the change is a `#[cfg]` widening plus two honest per-host decisions, in the
same shape `run_vm` uses — one shared body, the differences named in one place:

| | Linux (KVM) | Windows (WHP) |
|---|---|---|
| `install --network` default | `tap` (`usernet` since the 2026-09-26 installed-network amendment) | `usernet` |
| `--network tap` | the host interface | typed refusal naming `--network usernet` |
| Debian bootstrap kernel | `guest/bootstrap-kernel/build.sh`, or the same download | `entangled fetch bootstrap-kernel` — no cross build, so the release pipeline builds it once on Linux and every host downloads it |
| Everything else | identical | identical |

### Amendment: the last Linux-only row became a download

That third row said "copy `artifacts/bootstrap/` in from a Linux checkout, or
install Ubuntu", and it was the only remaining thing a Windows user could not
do at all. The constraint underneath it is real and unchanged — a Linux kernel
build does not cross-compile, and Debian's own installer kernel cannot drive
virtio-mmio because they build the module without
`CONFIG_VIRTIO_MMIO_CMDLINE_DEVICES` — but it is a constraint on *building*,
not on *having*. So the artifact is built once on Linux by CI
(`.github/workflows/guest-artifacts.yml`), published under an immutable release
tag with the kernel's corresponding source beside it, and fetched by
`entangled fetch bootstrap-kernel` into the same cache the ISOs use.

Two portability notes this leaves behind, both in the spirit of the rules above:

- **the resolver is portable and the build is not.** `apps/entangled/src/bootstrap.rs`
  has no `cfg(target_os)` at all: the pin, the digest check, the cache layout and
  the download are byte logic, and the only per-host difference is one sentence
  of advice in a failure message. That is the same shape as `net_plan` — the
  hosts differ in *what they can do locally*, never in the code path;
- **a fetched artifact cannot be named by a relative path.** Profiles written on
  a checkout still say `artifacts/bootstrap/vmlinuz`, because that is what the
  manager's working-directory setting exists to make work; profiles written on a
  host that downloaded the pair name it absolutely. One resolver decides, and
  the profile records what it decided, rather than a constant being written into
  a file that might be read from anywhere.

**The riskiest thing about the port turned out not to exist.** The plan named
the installer's networking as the highest-risk item — d-i and subiquity both
want a mirror, and on Windows that means the smoltcp NAT rather than TAP. But
the Ubuntu path is *deliberately offline* (`assets/autoinstall/ubuntu-server.yaml`:
one mirror candidate, no geoip, `fallback: offline-install`, everything installed
out of the ISO's own pool), so the install that matters on Windows never touches
the network at all. Networking is on the Debian d-i path's critical path only,
where the preseed is given a static address — and where usernet's numbers now
come from `virtio_net::UserNetConfig` itself rather than being repeated in the
installer, so the `[network]` section and the `netcfg/get_ipaddress=` clause
cannot drift apart.

**But it did hide a real bug, and only a real installer could find it.** The
Debian netboot path is the first thing this project has ever run over usernet
that opens *hundreds* of connections, and it stalled at "Loading additional
components" after exactly 64 udebs with `refusing a guest connection: the NAT is
at its flow limit`. The NAT was leaking a flow per completed download: a guest
that closes its half first — every HTTP client — leaves smoltcp in `CloseWait`,
where `is_open()` is still true, so the retirement test never fired and nothing
shut the host stream's write half, so the remote never sent the EOF that would
have finished the close. `MAX_FLOWS` was doing its job; what it bounded was a
leak. With the half-close propagated, the same install walks straight past
"Detecting hardware" into `debootstrap`. Two lessons worth keeping: a
user-mode NAT's *close* path needs a workload that closes thousands of
connections before it can be called done, and the unit tests that covered this
module all closed both halves at once, which is the one case that never leaks.

**And the d-i path needed two endings fixed, both of them "the installer stops
and nobody is typing".** d-i ends by *rebooting*, which on WHP is not an ending
at all — the triple fault is absorbed and the vCPU parks — so the automated
profile now sets `debian-installer/exit/poweroff`, the ACPI S5 ending both
backends already latch. And `apt-setup` selects the security suite by default,
scans `security.debian.org`, and on a failed scan raises a *critical-priority
note* with a `<Continue>` button that `priority=critical` does not skip and no
other preseed key answers; the Ubuntu path has a serial automation script that
could press Enter, the Debian path does not. Selecting no apt services removes
the scan. Neither is Windows-specific in principle — both are "the host must be
able to tell a finished install from a waiting one", which is the same
requirement the Ubuntu path met with `shutdown: poweroff` from the start.

Kernel-level DHCP through the NAT is now evidenced too — `ip=dhcp` on a real
guest, answered by `granted the guest a DHCP lease ip=192.168.74.15` on the
host and `IP-Config: Got DHCP answer from 192.168.74.1` in the guest — which
closes the phase-4 open item that DHCP had only ever been unit-tested. It took
a two-line change to make askable: a profile's own `ip=` clause now wins over
the backend's appended one, because the kernel honours the last one it is given.

**Two path facts that a Linux-first codebase gets wrong, both now tested.**
The cache root was resolved as `$HOME/.cache/entangled` with no Windows
fallback, so a Windows host with a full 2.9 GiB ISO cache reported "no Ubuntu
ISO in the cache"; `debian_media::cache_root` is now the one resolution both the
media cache and the ISO lookup use (`%LOCALAPPDATA%\entangled` when there is no
`HOME`), asserted for both hosts' environments on both hosts. And `--disk` is
now optional, defaulting into the *manager's* VM directory
(`disk_image::refs::manager_vm_dir`, `%USERPROFILE%\entangled-vms`), because two
defaults would have meant two halves of one VM collection.

One product decision recorded rather than made silently: an Ubuntu install still
writes a profile with **no `[network]` section**, matching the offline install it
came from. The cost is visible in the boot log — `systemd-networkd-wait-online`
and `cloud-init-network` each wait out their timeout before the login prompt,
on both hosts. Attaching a NIC means also giving the installed system a netplan
that expects one, which is a change to the autoinstall profile both hosts share,
so it is left as a follow-up rather than smuggled into the port.

### Addendum (same day, after merging main): the acceptance runs itself now

The phase-5 numbers above were measured by hand, because
`tests/ubuntu_install.rs` — the acceptance criterion as a test — could not pass
on Windows for a reason that had nothing to do with the port: it read the serial
transcript with `read_to_string`, which fails on the byte range an installed
Ubuntu writes while setting up its console font, and matched a marker systemd
splits with a colour escape. Both are fixed (see the vm-testing skill), and the
test now passes unattended on this Windows host:

| | |
|---|---|
| install | **189 s** (`--auto --headless`, 12 GiB target, debug build), ending `installed: GPT with an ESP on /dev/vda1 (572 MiB) and root on /dev/vda2 (ext4 UUID 40589fc3-…)` |
| the ending | `guest requested ACPI S5 (soft off) via="PM1a_CNT"` → `UEFI variable store written by the firmware programmed_bytes=5328` |
| the installed system | `BdsDxe: starting Boot0006 "Ubuntu"` off the persisted NVRAM entry → shim → `GNU GRUB version 2.14` → `Welcome to Ubuntu 26.04 LTS!` → `Started serial-getty@ttyS0.service` → `e2e-ubuntu login:` |
| total | 332 s for install *and* boot-what-was-installed |

So the acceptance is a command on either host rather than a procedure:
`cargo test -p entangled --test ubuntu_install -- --ignored`. The hand-measured
4 min 27 s and this 189 s are the same install on the same machine; the
difference is load, not a change.

The `[network]`-less profile's cost is still there and still visible in that
transcript: `systemd-networkd-wait-online` and `cloud-init-network` each wait out
about two minutes before the login prompt, which is most of the boot half.

## Consequences

- The MVP pays a small ongoing tax (trait indirection for interrupts, target
  gates) that we are already paying for testability anyway.
- WHP's exit latency is higher than KVM's; acceptable for a desktop VMM, but
  performance-sensitive paths (virtio notify) should keep batching-friendly
  shapes rather than assuming cheap exits.
- The userspace PIC/IOAPIC/PIT needed for WHP is also the first step toward
  reducing reliance on KVM's in-kernel devices, should that ever be wanted.
- Requires the "Windows Hypervisor Platform" optional feature on the host;
  coexists with WSL2 (both ride Hyper-V).

## Amendment, 2026-09-09 — two usernet items closed

The WHP phase-4 amendment left two open questions about the user-mode NAT that
is Windows' only network. Both are now answered, by tests rather than by
reasoning:

- **The teardown race is gone.** A host peer that wrote and closed in the same
  breath could beat its own bytes to the guest (~1 in 4 under WHP). It no
  longer reproduces: 250/250 clean unit exchanges and 15/15 clean real-guest
  runs on WHP, and there is a structural reason — `close()` is gated on
  `send_queue() == 0`, smoltcp orders the FIN behind buffered data, and a flow
  retires only in `Closed`/`TimeWait`. The remaining loss path is an *abortive*
  peer close, which is TCP semantics and now reaches the guest as an RST rather
  than as silence. The test has teeth: swapping the graceful close for
  `abort()` makes it fail.
- **`CONFIG_IP_PNP_DHCP` was never missing.** The TODO assumed the bootstrap
  kernel lacked it; `make defconfig` supplies it, the kernel `ip=dhcp` path is
  now proven guest-visible on both hosts, and `build.sh` asserts the symbol so
  it cannot quietly go.

One limit is worth stating plainly because it is easy to assume otherwise:
**usernet forwards UDP only for DHCP and DNS.** QUIC, NTP, mDNS and game
traffic do not cross it, and a test says so.

## Amendment, 2026-09-10 — WHP's version skew is a portability seam too

This ADR's portability rules are about the two *hosts*. One finding says they
are not enough: two Windows versions of the same host can disagree about an API
this backend depends on.

`WHvDeleteVirtualProcessor` followed by `WHvCreateVirtualProcessor` at the same
index works on the Windows the WHP backend was developed against, and does not
work at all on Windows 10 19045 (AMD Threadripper 1920X, WHP feature on): the
delete succeeds, and every create for that index afterwards returns
`E_INVALIDARG (0x80070057)` for the life of the partition. That pattern was how
the reset returned a virtual processor to power-on state, so on that host a
reboot ended the VM. ADR-0005's 2026-09-10 amendment has the measurement, the
replacement, and the reason the replacement is more portable rather than merely
different: it asks WHP for nothing beyond reading and writing processor state,
which is what suspend/restore already needs.

Two rules follow, both cheap:

- **Prefer state a host will hand back over state a host will recreate.** Where
  the choice exists, reading the pristine state once and writing it again asks
  less of the platform than any "make me a new one of these" call, and it is
  the same code the snapshot already owes (ADR-0006).
- **Phase 4's BSP-only rule stands and gets simpler.** Register setup is still
  BSP-only, and an AP still must not be touched by the host — but the reset no
  longer needs to *know* that: the state it writes back is per index, so the
  AP's wait-for-startup activity word returns to an AP and nothing branches on
  `is_boot_cpu`.

A third point is about the tests rather than the API. Both WHP reset tests
self-skip without the guest artifacts (`artifacts/bootstrap/vmlinuz`,
`artifacts/tests/test-initramfs.cpio.gz`), which are gitignored and built by a
bash script — so on a Windows-only checkout they had always skipped, and a
platform difference this basic went unseen. The script is Linux-only but the
work is not: the `init` cross-builds from Windows with
`rustup target add x86_64-unknown-linux-musl` and
`RUSTFLAGS="-Clinker=rust-lld -Clink-self-contained=yes"`, and the `cpio`/`gzip`
packing is one WSL command. The `whp-backend` skill carries the exact recipe.

## Amendment, 2026-09-24 — a host waker on WHP: the machine layer's last missing worker

Phase 3 and phase 4 closed the kick half of the port by *not* porting it:
with no ioeventfd, every queue notify runs on the vCPU thread that took the
exit, and nothing registered at an absolute address means nothing to rebase.
That was complete for work a *guest* starts. It was not complete for work the
*host* finishes. `virtio_core::HostWaker` is how a device with asynchronous
host work (a GPU fence retiring on a renderer thread) asks to be called back
"from its ordinary worker context, exactly as if the guest had kicked it" —
and on KVM that context is the ioeventfd worker, whose queue-0 eventfd a wake
simply writes. The synchronous-kick attach paths had no worker, so they
installed no waker, and every device on WHP ran on its no-waker fallback. For
the Venus renderer that fallback signalled each queue-timeline fence before the
GPU work it guards had run: 77 004 times in one GNOME desktop run (ADR-0004,
the 2026-09-24 capacity amendment).

**What the machine layer now provides.** `machine_x86::host_wake::HostWakeService`,
portable, one per synchronous-kick bus (`VirtioMmioBus::attach_userspace*` and
`VirtioPciBus::attach_userspace*`):

- every device gets a waker before it moves into its transport, as the KVM
  path's `DeferredWaker` is handed over; a wake before the thread starts is
  kept;
- a wake of slot *n* makes one machine-owned thread (`virtio-wake-mmio` /
  `virtio-wake-pci`) call `transport.lock().queue_notify(0)` — the entry point
  the KVM worker calls, under the same mutex a vCPU takes, so the device is
  serialised against guest kicks as it is on KVM, and the interrupt it raises
  goes through the same userspace IOAPIC / `UserspaceMsiSink` line a
  vCPU-thread notify would use. Off-vCPU interrupt injection was already
  proven by virtio-net's receive worker on this host;
- wakes coalesce: a per-slot pending bit, and only the wake that sets it
  touches the condvar, so a burst is one notify (two at most, if one lands
  while the first is being served);
- ADR-0005's obligations, as the ioeventfd workers meet them: the pause gate
  before the transport lock and outside every device lock, with the pass held
  across the notify; `reset()` stops and joins the thread, drops the old
  boot's wakes and starts a fresh one (safe on a paused VM, because stopping
  pairs the flag with `Quiesce::wake`); `shutdown()` joins and lets go of the
  transports, idempotent, and also runs from `Drop`. A wake is a request for
  service, not state, so a snapshot carries none (ADR-0006).

**Neither backend's API changed.** The service is additive to both buses, the
KVM constructors are untouched (`host_wake: None` there), and nothing in
`vmm_core` knows it exists. The one KVM configuration that still has no
worker, `ENTANGLED_QUEUE_NOTIFY=sync`, keeps its inert `DeferredWaker`, and
that is now a *documented* state rather than a fallback anyone may lean on:
`HostWaker`'s contract says a device must be correct when its waker never
fires — whatever it holds is served at the guest's next kick, later but never
wrongly. The Venus renderer no longer signals a ring fence because no waker
was installed (ADR-0004).

Evidence: `machine_x86::host_wake` unit tests and `tests/host_wake_bus.rs`
(both transports, both hosts: foreign-thread wake → queue-0 notify on the
service thread with the interrupt delivered through the userspace irqchip,
coalescing, no notify while paused, a paused reset that joins, a shutdown that
leaves the bus the transports' only owner), and on WHP itself
`vmm-core --test whp_virtio_blk`'s second boot, whose disk serves *none* of its
requests on the kicking vCPU — every kick becomes a wake, and the guest still
reads its 8 MiB, interrupt-completed (66 kicks, 66 notifies from the host-wake
thread, 64 interrupts).

## Amendment, 2026-09-26 — installed machines are networked by default: usernet on both hosts

ADR-0004's installed-profile amendment listed it as owed: *the installed
Ubuntu profile has no `[network]` section*. It was worse than one missing
section, and it was a per-host question, which is why it is answered here.

### What each path wrote

| | Installer VM | Installed profile | Default `--network` |
|---|---|---|---|
| `install debian` | the `--network` backend, static netcfg on the kernel command line | the same section, no MAC | `tap` on Linux, `usernet` on Windows |
| `install fedora` | ditto, static in the kickstart | ditto | ditto |
| `install ubuntu` | **no NIC** (offline by design) | **no `[network]` at all**; `--network` was accepted and ignored | ditto, ignored |
| manager's New machine | drives the CLI; never passed `--network` | whatever the engine wrote | the engine's |

A profile without `[network]` means no virtio-net device at `run`: the guest
has no NIC. So every Ubuntu machine — the GPU desktop included — booted with
no network, and on Linux the Debian and Fedora defaults were a TAP that
`scripts/setup-tap.sh` has to create as root, so a fresh Linux host failed
those installs half a second in. The hand-added `backend = "usernet"` in the
comparison profile (`venus-ubuntu-net-profile.toml`) was how every probe that
needed packages got them.

### The decision

**Every installed machine gets `[network] backend = "usernet"` and a MAC of
its own, on both hosts, unless `--network` says otherwise.**

- *usernet on Linux too.* It is the only backend that needs nothing from the
  host: no interface made as root, no DHCP server beside it, no
  administrator. TAP on Linux needs `setup-tap.sh` once *and*, for a guest
  that asks DHCP for its address (Ubuntu, and Fedora after install), a DHCP
  server on the segment that the project does not ship. A default that fails
  on a fresh host, or boots a guest with no address, is not a default. TAP
  stays one flag away: `--network tap [--interface <if>]`, Linux only, as
  before. `main::DEFAULT_NETWORK` is now one string, and it is
  `control_api::DEFAULT_NEW_MACHINE_NETWORK` spelled for clap (a test holds
  them equal).
- *A MAC in the profile.* `control_api::new_machine_mac(name, disk)`: FNV-1a
  over the name and the disk path, `52:…` (locally administered, unicast —
  the prefix `virtio_net::MacAddr::derive` uses). Written once, so it survives
  renaming the machine; unique per machine, because no two machines share a
  disk, so two `ubuntu` profiles in two VM directories still differ (which
  matters on a TAP bridge); deterministic, so a reinstall onto the same disk
  keeps the address a guest configuration may be keyed to. The installer VM
  and the installed machine share it (`NetPlan::machine_section`) — the
  installer's NIC *is* the machine's. `mac` is now validated at load
  (`control_api::parse_mac`: six hex octets, unicast, not zero) rather than at
  `run`, so the manager's editor shows the same refusal; a profile without
  one keeps the name-derived address it always had.
- *The Ubuntu installer stays offline.* Everything it installs comes from the
  verified ISO, so the install is reproducible and takes the time it was
  measured at, and no mirror, `updates: security` download or language-pack
  fetch can change what was tested. The machine updates the way any Ubuntu
  does, with `apt`, once it runs. So `--network` means "the machine's
  network" for all three installers; Debian and Fedora also install over it.
- *The guest configures the NIC itself.* The Desktop install needs nothing:
  NetworkManager brings up any wired NIC by DHCP (`Wired connection 1`, with
  subiquity's own `01-network-manager-all.yaml`). A *server* install that saw
  no NIC configures none, so the server profile's late-commands write
  `/etc/netplan/90-entangled.yaml`: DHCP on `match: name: "en*"` — by pattern,
  because the NIC's PCI slot and so its `enp0sN` name follow the disks, and
  the installer has two more of those — `optional: true`, mode 0600. The
  Debian and Fedora guests keep the static address their installer was
  preseeded with, which is the usernet guest address the DHCP server hands
  out anyway.
- *Existing profiles are untouched.* A profile without `[network]` still means
  no NIC; nothing rewrites it. The one exception is the manager's own install
  flow, which now passes `--network usernet` explicitly (a WSL engine may be
  an older release whose Linux default was TAP) and, if the profile the
  engine wrote has no `[network]` (an Ubuntu install by such an engine),
  stamps the section a current engine writes, in the same
  `discovery::apply_resources` pass that already stamps memory, vCPUs and
  refresh. The editor already offers backend, interface and MAC, so the
  default is one the UI can show.

### What the guest can reach, and what it cannot

Unchanged by design, and now written down in the user guide:

- One segment, `192.168.74.0/24`: the guest is `.15` by DHCP (one-hour
  lease), the process is `.1` — router, DHCP server and DNS server. Each VM
  has its own segment.
- **Reachable:** outbound TCP anywhere; DNS over UDP and TCP, relayed to
  `1.1.1.1`; a ping of `.1`. Also the host's *non-loopback* addresses and its
  LAN, exactly as any LAN machine reaches them — a host service bound to a LAN
  address or `0.0.0.0` is reachable at that address.
- **Refused by the NAT:** anything on the guest's own segment (the process
  itself — except DNS, below), loopback `127.0.0.0/8`, and now also
  `0.0.0.0` (Linux connects a socket aimed at it to the local host, so it was
  a loopback bypass on a Linux host), broadcast and multicast. A host service
  bound to `127.0.0.1` only is unreachable from the guest by every address.
- **Not carried:** UDP other than DHCP and DNS (no NTP — the guest reports
  `NTPSynchronized=no` — no QUIC, no games), ICMP past the gateway, IPv6 (the
  guest has only a link-local address and no route, so programs use IPv4;
  `curl -6` fails in 1 ms rather than hanging), anything inbound (no port
  forwarding).

### Two DNS bugs, found reading the relay before the acceptance

- **The relay read upstream answers into 512 bytes.** Every stub resolver asks
  for more through EDNS0 (systemd-resolved advertises 1232), and a longer
  answer is *discarded* on Windows (`recv_from` fails with `WSAEMSGSIZE` and
  the datagram is gone) and cut short on Linux, so an answer past 512 bytes
  never arrived and the lookup timed out. The relay now reads the whole datagram; one
  too long for a frame (1472 bytes of DNS) goes back as the header and
  question with TC set, no records — RFC 1035's truncation, walked with every
  read bounds-checked because the upstream is as untrusted as the guest.
- **The resolver's answer to TC is TCP to its DNS server** — `192.168.74.1:53`
  — which the NAT refused as host-local. That one flow is now opened, and
  carried to the configured upstream: an address the host chose, never one
  the guest named. Every other gateway port stays refused.

### Measured on this Windows host (2026-09-26, release build)

A fresh `install ubuntu --venus --auto --headless` from the 26.04.1 Desktop
ISO into `F:\VMs\Entangled\netdef`, no `--network`: **13 min 39 s**, and the
summary says `network: usernet — user-mode NAT, the guest takes its address
by DHCP; MAC 52:ad:f5:d7:2e:58`. The written profile has
`[network] backend = "usernet"`, `mac = "52:ad:f5:d7:2e:58"`. Then `run` of
that profile, unchanged, driven over the serial console
(`F:\VMs\Entangled\probes\net\`). Every row but the throughput is from the
boot before the throughput fix below; the throughput row is the final build
on the same disk:

| Check | Result |
|---|---|
| address | host: `granted the guest a DHCP lease mac=52-ad-f5-d7-2e-58 ip=192.168.74.15`; guest: `enp0s2=192.168.74.15/24`, `default via 192.168.74.1 proto dhcp`, NetworkManager's `Wired connection 1`, lease 3600 s, DNS `192.168.74.1` |
| DNS | resolved's link DNS `192.168.74.1`; `deb.debian.org`, `archive.ubuntu.com`, `www.mozilla.org` resolve (A and AAAA) |
| big DNS | `dig +ignore TXT microsoft.com`: `flags: qr tc`, 42 bytes — the relay's truncation; without `+ignore`: `Truncated, retrying in TCP mode`, 58 answers, 4575 bytes; `dig +tcp @192.168.74.1` answers |
| HTTPS | `curl -I` of `https://archive.ubuntu.com`, `https://deb.debian.org/debian/`, `https://www.mozilla.org/`: 200 |
| apt | `apt-get update` rc 0 (`resolute`, `-updates`, `-security`), `apt-get install curl bind9-dnsutils` rc 0; GNOME's Software Updater found 198 updates on its own |
| Firefox | `https://www.debian.org/` rendered on the GPU desktop (screenshot `netdef\firefox-3.png`), 22 established HTTPS flows |
| isolation | host listener on `127.0.0.1:8765`: from the guest `192.168.74.1:8765` refused (rc 7), `192.168.233.1:8765` reset (rc 56, nothing listens there), `127.0.0.1:8765` is the guest's own loopback (rc 7); the listener's log shows no guest request. A listener bound to the host-only `192.168.233.1:8000` is reachable (200) |
| not carried | ping `1.1.1.1` 100 % loss, ping `.1` answered, `NTPSynchronized=no`, `curl -6` fails at once |
| throughput | final build: 1 GiB over HTTPS from `proof.ovh.net` at **53.2 MB/s** (20.2 s), 100 MB from Hetzner at 51.0 MB/s; the host itself, minutes earlier: 83.8 and 90.0 MB/s. `entangled.exe` used 13.8 s of CPU over the 21.2 s download — 65 % of one core of 24, the guest's vCPU time (its TLS included) counted in — and 2-4 % idle. Before the fix below: **1.4-1.9 MB/s**, 10-13 % of a core. The same boot re-ran DNS (a 1187-byte TXT answer over UDP, the TC/TCP round trip) and `curl -I` on the final build |

### A third bug the measurement found: two segments a tick

The first acceptance run downloaded at 1.4-1.8 MB/s against a host line doing
51 MB/s. The guest's own TCP said why (`ss -ti` 15 s into a download): about
1300 segments a second, whatever the file — a packet-rate ceiling, not a
bandwidth one. smoltcp's `Interface::poll` dispatches each socket once, one
TCP segment per flow, and `TcpNat::poll` called it twice a pump tick (~1.4 ms
on this host, measured). `TcpNat::poll_with_budget` now runs `poll_egress`
again while it produces, up to `MAX_SEGMENTS_PER_POLL` (64) and never past
the room left in the guest's queue, so a burst is paced rather than dropped
at `MAX_QUEUED_FRAMES`. The flow buffer then bounds a tick, so it went from
16 KiB to 256 KiB (32 MiB at the 64-flow cap). Each step measured on its own
boot of the installed profile, 1 GiB download:

| `FLOW_BUFFER` | segments per tick | guest MB/s | VMM CPU |
|---|---|---|---|
| 16 KiB | 2 | 1.8 | 10-12 % of a core |
| 256 KiB | 2 | 1.9 | 13 % |
| 16 KiB | ≤ 64 | 9.9 | 20 % |
| 256 KiB | ≤ 64 | **52.3** | 46 % |
| 256 KiB, read straight into smoltcp's ring (final) | ≤ 64 | **53.2** | 65 % |

`a_bulk_download_is_not_held_to_one_segment_a_poll` pins it: a full 16 KiB
window now leaves in one poll, and with the loop taken out the test sees two.

### Boots that did not reach the network

Two of the eight boots of the installed 8-vCPU profile that day panicked
before any NIC was up: `..MP-BIOS bug: 8254 timer not connected to IO-APIC`,
then `Kernel panic - not syncing: IO-APIC + timer doesn't work!` at
0.001-0.002 s, on an idle host. Even the six good boots print `tsc: Unable to
calibrate against PIT`, as the 4-vCPU refresh runs of the same day did in
about half their boots without ever panicking. It is not this amendment's —
the profiles that panicked and the ones that booted are the same file — but
it is the first thing a user of an 8-vCPU installed machine on WHP would hit,
so it is recorded here and owed below. It is not only a many-vCPU matter:
the 1-vCPU direct-boot guest of `usernet_guest` panicked the same way in one
gate run, with a WSL workspace build loading the host beside it (three
reruns passed).

### Tests

- `control_api`: `a_new_machines_network_round_trips_for_each_backend`,
  `a_new_machines_mac_is_stable_unique_and_valid` (1000 machines, no repeat),
  `a_profiles_mac_is_validated_at_load`.
- `entangled`: `the_default_network_is_usernet_on_every_host`,
  `each_network_choice_becomes_the_machines_section`,
  `the_installed_machine_is_networked_by_default` (Ubuntu profile, TOML round
  trip, `none`, and `tap` on Linux),
  `the_installed_server_configures_its_nic_by_dhcp` (the folded late-command,
  server only, the installer still offline); the ignored `ubuntu_install`
  end-to-end now asserts the written `[network]`.
- `entangled-manager`: the install command line carries `--network usernet`;
  `apply_resources_networks_a_machine_an_older_engine_left_offline`.
- `virtio-net`: `host_local_destinations_are_refused` (now with `0.0.0.0`,
  `127.1.2.3`, broadcast, multicast, three ports each),
  `dns_over_tcp_to_the_gateway_reaches_the_upstream_resolver`,
  `a_dns_answer_longer_than_512_bytes_is_delivered_whole`,
  `a_dns_answer_too_long_for_a_frame_is_truncated_with_tc`,
  `a_malformed_long_dns_answer_is_dropped_not_misread`,
  `a_bulk_download_is_not_held_to_one_segment_a_poll`.

### Owed

- **The 8254 under WHP** (above): `check_timer()` failed in two of eight
  8-vCPU boots and one loaded 1-vCPU test boot, and PIT calibration fails in
  most boots. The userspace PIT's catch-up (`MAX_CATCHUP_EDGES`) and its
  thread's scheduling against the vCPU threads and host load are where to
  look first (whp-backend skill, 8254 notes).
- **DNS goes to `1.1.1.1`, not the host's resolver.** A network that blocks
  outside DNS, a VPN's internal zone, `/etc/hosts` and `.local` names do not
  resolve in the guest. Reading the host's resolvers (`/etc/resolv.conf`,
  `GetAdaptersAddresses`) is a contained change to `UserNetConfig`; so is a
  `[network] dns` key, which the manager would then have to show.
- **No port forwarding, no NTP, no IPv6, no ICMP past the gateway** — each a
  separate feature of the NAT, none needed to reach the internet.
- **The server path is not acceptance-tested here**: this host has only the
  Desktop ISO. The netplan late-command is asserted byte for byte and its
  folding modelled, but no installed server has DHCPed with it yet; the
  ignored `ubuntu_install` end-to-end is the test that would.
- **TAP is unmeasured on this machine**: WSL has no `/dev/kvm` here. The TAP
  path's code did not change beyond the MAC it is now given.
- **Upload throughput** (guest → host) was not measured; it rides the same
  loop and buffer, so it should have moved with the download.
