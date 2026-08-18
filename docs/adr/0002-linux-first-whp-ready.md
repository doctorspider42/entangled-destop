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
additive, so no manifest edit on our side can subtract it. Options when
this becomes load-bearing (it blocks WHP-1703 onward): upstream a
`default-features = false` PR to rust-vmm, vendor a patched
`virtio-queue`, or use `[patch.crates-io]` against a fork.

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
