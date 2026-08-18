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
- `guest/` — bootstrap kernel/initramfs configs and test rootfs
- `tests/` — boot, installer and graphical integration tests

## License

Apache-2.0. Host-side dependencies are audited in CI (`cargo deny`) to exclude
copyleft licenses.
