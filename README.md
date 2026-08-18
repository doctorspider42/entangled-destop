# Entangled Desktop

A small, self-contained Virtual Machine Monitor (VMM) for Linux x86-64 hosts,
written in Rust directly on KVM using [rust-vmm](https://github.com/rust-vmm)
components. No QEMU — not as a process, not as a library.

MVP target flow:

```bash
entangled fetch debian --channel stable --arch amd64 --variant gtk-netboot
entangled disk create debian.raw --size 32G
entangled install debian --disk debian.raw
entangled run debian.toml
```

…ending with an installed Debian stable booting into Weston in a 1920×1080
window with working keyboard, mouse and network.

## Status

Early scaffolding. See [docs/adr/0001-mvp-architecture.md](docs/adr/0001-mvp-architecture.md)
for the architecture and [entangled-mvp-backlog.md](entangled-mvp-backlog.md) for the
full backlog (Polish).

`entangled fetch` works today (EPIC 6): it resolves the current Debian stable
release from signed metadata, verifies it against OpenPGP keys pinned in
`crates/debian-media/keys/`, streams the artifacts with resumable downloads and
writes a provenance manifest next to each one.

```bash
entangled fetch debian --channel stable --arch amd64 --variant gtk-netboot
entangled fetch debian --variant netinst-iso --refresh   # re-check the signed sums
entangled fetch debian --variant text-netboot --offline  # verified cache only
```

Media lands in `$XDG_CACHE_HOME/entangled/media/<version>/<arch>-<variant>/`. A
second run over an intact cache performs no network access at all; nothing is
marked ready before both its OpenPGP signature and its digest check pass, and a
failed check removes the partial file.

## Desktop manager (GUI)

`entangled-manager` is a native desktop front end for the same flows — no
Electron, no browser: egui/eframe rendering through wgpu, the stack the VM
window already uses.

```bash
cargo build --workspace     # puts entangled and entangled-manager side by side
entangled-manager           # or: entangled-manager --vm-dir ~/vms
```

It scans a VM directory (default `~/entangled-vms`, changeable in Settings and
persisted to `~/.config/entangled/manager.toml`) and shows a card per profile
with its memory, vCPUs, resolution, network interface, image size and how much
of it is actually allocated on disk. From there:

- **Start / Stop** — `entangled run <profile>` as a tracked child process; the
  VM opens its own window, Stop sends a termination signal so the guest shuts
  down cleanly. Closing the manager leaves running VMs alone; they are not its
  children's keeper, only their launcher.
- **New machine** — a wizard for name, memory, vCPUs, disk size and installer
  variant that runs `entangled install debian --auto` and streams the installer
  console into the log pane while a card tracks it as *Installing*.
- **Delete** — refuses while the machine is busy, then asks for the name to be
  typed out; disks that live outside the VM directory are left alone.
- **Console** — the child's stdout/stderr is written to
  `<vm-dir>/<name>-{run,install}.log` and tailed live in the UI, so a failure
  leaves both an on-screen explanation and a file to inspect. Common causes
  (a TAP already held by another VM, no `/dev/kvm`, a missing bootstrap kernel)
  are recognised and explained in one sentence.

The manager itself is host-agnostic: it builds on Windows as well, where the
stop signal falls back to terminating the process.

## Requirements

- Linux x86-64 host with KVM (`/dev/kvm`)
- Rust stable toolchain
- For networking: `CAP_NET_ADMIN` (TAP device setup)

Development on Windows works through WSL2 (Ubuntu), which exposes a real
`/dev/kvm` via nested virtualization, or via `docker/Dockerfile.dev`.

## Layout

- `crates/` — VMM libraries (KVM core, machine model, direct Linux boot,
  virtio transport and devices, display, Debian media handling, control API)
- `apps/entangled` — the `entangled` binary
- `apps/manager` — `entangled-manager`, the native desktop GUI
- `guest/` — bootstrap kernel/initramfs configs and test rootfs
- `tests/` — boot, installer and graphical integration tests

## License

Apache-2.0. Host-side dependencies are audited in CI (`cargo deny`) to exclude
copyleft licenses.
