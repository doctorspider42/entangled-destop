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
touching boot modes or firmware-facing platform devices),
[docs/adr/0005-vm-lifecycle.md](docs/adr/0005-vm-lifecycle.md) (pause, resume
and reboot-in-place: the vCPU stop protocol, what "quiesced" means per device
class, the guest reset matrix, and the gap list for suspend/restore — read
before adding a device, a host thread that touches guest memory, or anything
that ends a run),
[docs/adr/0006-suspend-restore.md](docs/adr/0006-suspend-restore.md) (writing a
running VM to a file and reading it back: the full CPU state per host, the
save/load pair every device owes beside its `reset()`, the snapshot format and
every refusal it makes — read before adding a device, or anything that holds
state a resumed guest would notice missing).

## Build and test

Two supported hosts, and since EPIC 17 phase 5 the same command set on both:
Linux/KVM (the MVP target) and Windows/WHP both run `entangled install`, `run`,
`doctor` and the manager. `install ubuntu` is the portable path (UEFI + verified
ISO, offline); `install debian` additionally needs the bootstrap kernel, which
only builds on Linux. On this Windows machine the Linux side runs in WSL Ubuntu
(has `/dev/kvm` via nested virtualization) or the Dockerfile:

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
| `crates/vmm-core` | Hypervisor backends (KVM, WHP), guest memory, vCPU lifecycle, VM state machine, the neutral whole-CPU state a snapshot carries | EPIC 1/17 |
| `crates/vm-snapshot` | The snapshot container: versioned format, per-section digests, VM/disk fingerprints and every refusal a restore makes (ADR-0006) | — |
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
| `crates/disk-image` | Portable disk-image logic: MBR/GPT/ext4 inspection, create/resize, sparse-preserving relocate, profile-reference guard, `.nvram` sidecar convention | — |
| `crates/control-api` | VM config model (TOML), lifecycle API for CLI/GUI | EPIC 12 |
| `apps/entangled` | `entangled` binary: fetch/disk/install/run/doctor | EPIC 10/12 |
| `apps/manager` | `entangled-manager`: native egui GUI, drives the CLI as child processes | EPIC 16 |

## Skills

Task-focused guides for working in this repo live in `.claude/skills/`:
`kvm-machine`, `whp-backend`, `linux-direct-boot`, `acpi-machine`,
`virtio-device`, `debian-media`, `host-display`, `vm-testing`, `gui-manager`.
Load the matching skill before working on that subsystem. **Every agent loads
`dev-environment` first** — target-dir and disk-space rules for this two-host
machine (the WSL VHDX bloat problem), demo-VM launching, artifact rebuilds;
`scripts/dev-clean.sh` sweeps stray build dirs.

Every skill edit must update its twin in the same change: the Claude copy under
`.claude/skills/` and the project-local Codex copy under `.agents/skills/`. Keep
both versions materially equivalent; a skill change is incomplete until its
twin is updated and validated too.

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
- **A device owes the machine three things, not one.** A `reset()` back to
  power-on (ADR-0005), a `Quiesce` gate before any thread of its own touches
  guest memory (ADR-0005), and a `save`/`load` pair — `queue_positions` if it
  holds queues, `save_device`/`load_device` if it holds anything else
  (ADR-0006). A device that skips the first makes a reboot a haunting, the
  second makes "paused" a lie, and the third makes a resumed guest subtly wrong.
- **Devices are transport-agnostic.** Implement against `VirtioDevice` +
  queues, never against a transport's registers. Both virtio-mmio and virtio-pci
  exist (`transport = "mmio" | "pci"` per VM, default mmio); adding the second
  one touched no device crate, which is the standard to hold. The half the
  transports share lives in `virtio_core::state::TransportState` — a transport
  module is only an address decoder.
- **A VM can be frozen and rebooted, so every device owes two things**
  (ADR-0005): a `reset()` that returns it to power-on, and — if it runs a host
  thread of its own that touches guest memory — a `virtio_core::Quiesce` wait
  before it does, taken *outside* any device lock. A device that skips either
  makes "paused" a lie and a reboot a haunting.
- **No QEMU anywhere** — not as a process, dependency or linked library.
- Errors are typed (`thiserror`) per crate; logging via `tracing` with the VM
  id in the span.
