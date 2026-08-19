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
