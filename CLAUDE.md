# Entangled Desktop

A small VMM for Linux x86-64 hosts, written in Rust on KVM + rust-vmm crates.
No QEMU. MVP goal: install and run Debian stable in a 1920×1080 window with
2D graphics (`virtio-gpu`), input and networking. Product backlog (in Polish,
treat as guidance, not contract): [entangled-mvp-backlog.md](entangled-mvp-backlog.md).
Architecture decisions: [docs/adr/0001-mvp-architecture.md](docs/adr/0001-mvp-architecture.md),
[docs/adr/0002-linux-first-whp-ready.md](docs/adr/0002-linux-first-whp-ready.md)
(portability rules that keep the native Windows/WHP port cheap, plus its
amendments recording what the port has actually delivered — read before adding
OS-specific code or interrupt plumbing),
[docs/adr/0003-uefi-firmware.md](docs/adr/0003-uefi-firmware.md) (which UEFI
firmware, how it is entered, and what the machine still owes it — read before
touching boot modes or firmware-facing platform devices).

## Build and test

Two supported hosts since EPIC 17 phase 4: Linux/KVM (the MVP target, incl.
`entangled install`) and Windows/WHP (`entangled run`, `doctor`, the manager).
On this Windows machine the Linux side runs in WSL Ubuntu (has `/dev/kvm` via
nested virtualization) or the Dockerfile:

```bash
wsl -d Ubuntu -e bash -lc "cd /mnt/d/entangled-desktop && cargo test --workspace"
```

- `cargo build --workspace` / `cargo test --workspace` — everything
- `cargo clippy --workspace --all-targets -- -D warnings` — must be clean
- `cargo fmt --all` — before finishing any change
- `cargo deny check` — license gate (blocks GPL/AGPL/LGPL); runs in CI

The **whole workspace builds and tests natively on Windows** too — that is
where the WHP backend is exercised (EPIC 17), including `entangled run` of a
real Linux guest with the window, virtio-pci + MSI-X, user-mode networking and
UEFI with persistent NVRAM. Run both hosts for any change:

```powershell
$env:CARGO_TARGET_DIR = "$env:LOCALAPPDATA\entangled-target-whp"
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo run -p entangled -- run --headless examples\windows-whp.toml
```

Two vendored, minimally patched crates in `third_party/` (`virtio-queue`,
`linux-loader` — see their VENDORED.md and ADR-0002) take the unix-only
`vm-memory` `rawfd` feature back out of the graph; cargo features are additive,
so nothing else could.

WHP tests need the "Windows Hypervisor Platform" optional feature (admin +
reboot); without it they self-skip with a hint, like the KVM tests without
`/dev/kvm`. `cargo test -p vmm-core --test whp_boot` additionally needs the
kernel and initramfs artifacts, and self-skips without them.

## Workspace map

| Crate | Owns | Backlog |
|---|---|---|
| `crates/vmm-core` | Hypervisor backends (KVM, WHP), guest memory, vCPU lifecycle, VM state machine | EPIC 1/17 |
| `crates/machine-x86` | x86-64 machine model: memory layout, E820, CPUID, GDT, ACPI/MP tables, PCI root bus, and the userspace 8259/8254/IOAPIC for hosts whose hypervisor has none | EPIC 1/2/17/19 |
| `crates/linux-boot` | Direct bzImage+initramfs boot, boot_params, cmdline | EPIC 2 |
| `crates/uefi-boot` | UEFI firmware boot: PVH entry, reset-vector ROM placement | EPIC 18 |
| `crates/virtio-core` | virtio-mmio **and** virtio-pci transports, virtqueues, `VirtioDevice` trait | EPIC 3/19 |
| `crates/virtio-block` | virtio-blk device, RAW file backend | EPIC 4 |
| `crates/virtio-net` | virtio-net device, TAP backend | EPIC 5 |
| `crates/virtio-gpu` | virtio-gpu device: 2D scanout + VirGL 3D (`Renderer3d` trait, null renderer everywhere, virglrenderer dlopen'd on Linux — ADR-0004) | EPIC 8, GPU-001..012 |
| `crates/virtio-input` | keyboard + absolute pointer devices | EPIC 9 |
| `crates/display` | winit window, wgpu renderer, host input capture | EPIC 7 |
| `crates/debian-media` | Debian download, PGP+SHA-512 verification, cache, manifests | EPIC 6 |
| `crates/control-api` | VM config model (TOML), lifecycle API for CLI/GUI | EPIC 12 |
| `apps/entangled` | `entangled` binary: fetch/disk/install/run/doctor | EPIC 10/12 |
| `apps/manager` | `entangled-manager`: native egui GUI, drives the CLI as child processes | EPIC 16 |

## Skills

Task-focused guides for working in this repo live in `.claude/skills/`:
`kvm-machine`, `whp-backend`, `linux-direct-boot`, `acpi-machine`,
`virtio-device`, `debian-media`, `host-display`, `vm-testing`, `gui-manager`.
Load the matching skill before working on that subsystem.

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
  `[target.'cfg(target_os = "linux")'.dependencies]`; the WHP backend and the
  `windows` crate stay behind `#[cfg(windows)]` and
  `[target.'cfg(windows)'.dependencies]`. Protocol constants, parsing,
  validation and config logic must build and test everywhere. Guest memory is
  portable — never add a second guest-memory type.
- **Hypervisors talk through `vmm_core::hv`.** Machine and device code uses the
  neutral register structs, `ExitHandler`, `RunOutcome` and `InterruptDelivery`;
  a `kvm_bindings` or `WHV_*` type outside `vmm-core` is a bug. Neither backend's
  public API may change to suit the other — a capability only one host needs is
  an *additive* option (`WhpOptions`) or a trait the other simply does not
  implement, never a changed signature.
- **Every `unsafe` block carries a `// SAFETY:` comment** saying why the
  pointer is valid and which union arm is live (`undocumented_unsafe_blocks` is
  `deny`). FFI-ness alone is not a justification.
- **Devices are transport-agnostic.** Implement against `VirtioDevice` +
  queues, never against a transport's registers. Both virtio-mmio and virtio-pci
  exist (`transport = "mmio" | "pci"` per VM, default mmio); adding the second
  one touched no device crate, which is the standard to hold. The half the
  transports share lives in `virtio_core::state::TransportState` — a transport
  module is only an address decoder.
- **No QEMU anywhere** — not as a process, dependency or linked library.
- Errors are typed (`thiserror`) per crate; logging via `tracing` with the VM
  id in the span.
