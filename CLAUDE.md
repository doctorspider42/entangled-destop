# Entangled Desktop

A small VMM for Linux x86-64 hosts, written in Rust on KVM + rust-vmm crates.
No QEMU. MVP goal: install and run Debian stable in a 1920×1080 window with
2D graphics (`virtio-gpu`), input and networking. Product backlog (in Polish,
treat as guidance, not contract): [entangled-mvp-backlog.md](entangled-mvp-backlog.md).
Architecture decisions: [docs/adr/0001-mvp-architecture.md](docs/adr/0001-mvp-architecture.md),
[docs/adr/0002-linux-first-whp-ready.md](docs/adr/0002-linux-first-whp-ready.md)
(portability rules that keep a native Windows/WHP port cheap — read before
adding OS-specific code or interrupt plumbing),
[docs/adr/0003-uefi-firmware.md](docs/adr/0003-uefi-firmware.md) (which UEFI
firmware, how it is entered, and what the machine still owes it — read before
touching boot modes or firmware-facing platform devices).

## Build and test

The full VMM only builds on Linux. On this Windows machine use WSL Ubuntu
(has `/dev/kvm` via nested virtualization) or the Dockerfile:

```bash
wsl -d Ubuntu -e bash -lc "cd /mnt/d/entangled-desktop && cargo test --workspace"
```

- `cargo build --workspace` / `cargo test --workspace` — everything
- `cargo clippy --workspace --all-targets -- -D warnings` — must be clean
- `cargo fmt --all` — before finishing any change
- `cargo deny check` — license gate (blocks GPL/AGPL/LGPL); runs in CI

## Workspace map

| Crate | Owns | Backlog |
|---|---|---|
| `crates/vmm-core` | KVM handle, guest memory, vCPU lifecycle, VM state machine | EPIC 1 |
| `crates/machine-x86` | x86-64 machine model: memory layout, E820, CPUID, GDT, IRQ chip | EPIC 1/2 |
| `crates/linux-boot` | Direct bzImage+initramfs boot, boot_params, cmdline | EPIC 2 |
| `crates/uefi-boot` | UEFI firmware boot: PVH entry, reset-vector ROM placement | EPIC 18 |
| `crates/virtio-core` | virtio-mmio transport, virtqueues, `VirtioDevice` trait | EPIC 3 |
| `crates/virtio-block` | virtio-blk device, RAW file backend | EPIC 4 |
| `crates/virtio-net` | virtio-net device, TAP backend | EPIC 5 |
| `crates/virtio-gpu` | virtio-gpu 2D device | EPIC 8 |
| `crates/virtio-input` | keyboard + absolute pointer devices | EPIC 9 |
| `crates/display` | winit window, wgpu renderer, host input capture | EPIC 7 |
| `crates/debian-media` | Debian download, PGP+SHA-512 verification, cache, manifests | EPIC 6 |
| `crates/control-api` | VM config model (TOML), lifecycle API for CLI/GUI | EPIC 12 |
| `apps/entangled` | `entangled` binary: fetch/disk/install/run/doctor | EPIC 10/12 |
| `apps/manager` | `entangled-manager`: native egui GUI, drives the CLI as child processes | EPIC 16 |

## Skills

Task-focused guides for working in this repo live in `.claude/skills/`:
`kvm-machine`, `linux-direct-boot`, `virtio-device`, `debian-media`,
`host-display`, `vm-testing`, `gui-manager`. Load the matching skill before
working on that subsystem.

## Hard rules

- **Guest is untrusted.** Never index host memory with raw guest-supplied
  values; all guest memory access goes through `vm-memory` checked APIs.
  Descriptor chain walks are bounded (`virtio-core::MAX_DESC_CHAIN_LEN`).
  Malformed guest input fails the request or resets the device — never
  `panic!`, `unwrap()` or `expect()` on a guest-controlled path.
- **No copyleft in host code.** New dependencies must pass `cargo deny check`.
  GPL/AGPL/LGPL are blocked (guest-side content is unaffected).
- **Keep the core portable.** Linux-only code (`kvm-*`, TAP, eventfd) stays
  behind `#[cfg(target_os = "linux")]` and
  `[target.'cfg(target_os = "linux")'.dependencies]`. Protocol constants,
  parsing, validation and config logic must build and test everywhere.
- **Devices are transport-agnostic.** Implement against `VirtioDevice` +
  queues, not against virtio-mmio specifics — virtio-pci arrives post-MVP.
- **No QEMU anywhere** — not as a process, dependency or linked library.
- Errors are typed (`thiserror`) per crate; logging via `tracing` with the VM
  id in the span.
