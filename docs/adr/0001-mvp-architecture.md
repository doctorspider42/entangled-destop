# ADR-0001: VMHost MVP architecture and scope

- Status: accepted
- Date: 2026-08-18
- Backlog: [vmhost-mvp-backlog.md](../../vmhost-mvp-backlog.md) (MVP-001)

## Context

VMHost is a small, self-contained Virtual Machine Monitor for Linux x86-64 hosts.
The MVP must install and run Debian stable (currently Trixie) in a window with
1920×1080 2D graphics, keyboard/mouse input and networking — without using QEMU
as a process or library, and without shipping copyleft code in the host binary.

## Decisions

1. **Language and hypervisor base.** The host is written in Rust on top of KVM,
   using rust-vmm ecosystem crates (`kvm-ioctls`, `kvm-bindings`, `vm-memory`,
   `linux-loader`, `vmm-sys-util`, `virtio-queue`, `vm-superio`). QEMU is neither
   a runtime dependency nor a linked library.
2. **Control surface.** The first control surface is a CLI (`vmhost`). The
   Avalonia/.NET GUI is explicitly out of MVP scope; the `control-api` crate keeps
   the machine-facing model separate from the CLI so a GUI can attach later.
3. **No BIOS/UEFI in MVP.** Guests boot via the Linux x86 boot protocol (direct
   `bzImage` + initramfs load). Installed systems boot through a project-maintained
   *bootstrap kernel* + initramfs that mounts the installed root from `/dev/vda`
   and `switch_root`s into it. Kernel updates inside the guest do not change the
   boot kernel — an explicit, documented MVP limitation. UEFI/OVMF comes post-MVP.
4. **virtio-mmio, not virtio-pci.** Devices are exposed over `virtio-mmio` and
   announced to the (known) kernel via the command line. This avoids implementing
   a PCI bus, BARs, capability lists and MSI-X in the MVP. virtio-pci is the
   planned successor transport post-MVP.
5. **Device set (all P0):** `virtio-blk` (RAW file backend), `virtio-net` (TAP
   backend), `virtio-gpu` (2D only: scanout, transfer, flush, dirty rects),
   `virtio-input` (keyboard + absolute pointer). Accelerated OpenGL via
   VirGL/Rutabaga is a separate post-MVP milestone, tracked as GPU-0xx.
6. **Presentation.** One `winit` window per VM, rendered via `wgpu` (Vulkan
   preferred). The guest scanout is copied into a host texture; the host GPU only
   presents and scales — no passthrough.
7. **Debian acquisition.** The downloader resolves the *current* release from the
   `stable`/`current` channel metadata instead of pinning version numbers. Trust
   chain: OpenPGP signature over `SHA512SUMS` (Debian keyring) → SHA-512 of the
   artifact → provenance manifest written next to the artifact. Netboot
   (kernel + initrd from `deb.debian.org`) is the recommended install path; a
   user-supplied netinst ISO attached read-only as `/dev/vdb` is the
   compatibility path.
8. **Licensing.** The host binary must not link GPL/AGPL/LGPL code. Enforced in
   CI with `cargo-deny`; `THIRD_PARTY_LICENSES` and an SBOM are generated
   artifacts (MVP-003/006).
9. **Safety posture.** The guest is untrusted. All guest-controlled data
   (descriptor chains, addresses, indices, sizes) is validated before use; a
   malformed descriptor must fail the request or reset the device — never panic
   the host. Bounded chain walks, checked guest-memory access via `vm-memory`
   only.

## Repository shape

Cargo workspace: `crates/{vmm-core, machine-x86, linux-boot, virtio-core,
virtio-block, virtio-net, virtio-gpu, virtio-input, display, debian-media,
control-api}` plus `apps/vmhost-cli`, `guest/` (bootstrap kernel/initramfs,
test rootfs) and `tests/` (boot, installer, graphical).

Crates keep host-OS-specific code behind `#[cfg(target_os = "linux")]` and
target-specific dependency sections so that the platform-independent core
(config model, protocol constants, media resolution, validation logic) builds
and tests on any development OS; the full VMM builds only on Linux.

## Consequences

- Milestone A ("own VMM boots Linux to a serial console") is reachable in
  1–2 weeks because no firmware, PCI or graphics work blocks it.
- Direct boot restricts MVP guests to Linux kernels we can load ourselves;
  arbitrary-ISO boot is deferred by design.
- Choosing virtio-mmio defers PCI complexity but means the post-MVP transport
  migration must keep device implementations transport-agnostic — hence the
  `VirtioDevice` trait in `virtio-core` is defined against queues and config
  space, not against a transport.
