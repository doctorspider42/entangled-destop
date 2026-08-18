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
