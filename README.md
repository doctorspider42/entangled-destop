# VMHost

A small, self-contained Virtual Machine Monitor (VMM) for Linux x86-64 hosts,
written in Rust directly on KVM using [rust-vmm](https://github.com/rust-vmm)
components. No QEMU — not as a process, not as a library.

MVP target flow:

```bash
vmhost fetch debian --channel stable --arch amd64 --variant gtk-netboot
vmhost disk create debian.raw --size 32G
vmhost install debian --disk debian.raw
vmhost run debian.toml
```

…ending with an installed Debian stable booting into Weston in a 1920×1080
window with working keyboard, mouse and network.

## Status

Early scaffolding. See [docs/adr/0001-mvp-architecture.md](docs/adr/0001-mvp-architecture.md)
for the architecture and [vmhost-mvp-backlog.md](vmhost-mvp-backlog.md) for the
full backlog (Polish).

## Requirements

- Linux x86-64 host with KVM (`/dev/kvm`)
- Rust stable toolchain
- For networking: `CAP_NET_ADMIN` (TAP device setup)

Development on Windows works through WSL2 (Ubuntu), which exposes a real
`/dev/kvm` via nested virtualization, or via `docker/Dockerfile.dev`.

## Layout

- `crates/` — VMM libraries (KVM core, machine model, direct Linux boot,
  virtio transport and devices, display, Debian media handling, control API)
- `apps/vmhost-cli` — the `vmhost` binary
- `guest/` — bootstrap kernel/initramfs configs and test rootfs
- `tests/` — boot, installer and graphical integration tests

## License

Apache-2.0. Host-side dependencies are audited in CI (`cargo deny`) to exclude
copyleft licenses.
