# Entangled Desktop

Run Linux in a window, on Linux or Windows, with an experimental native Apple
Silicon backend, using a virtual machine monitor written from scratch in Rust.

Entangled Desktop is a VMM: it builds a virtual machine on the hypervisor your
system already has — **KVM** on Linux, the **Windows Hypervisor Platform** on
Windows, or **Hypervisor.framework** on macOS — gives it disks, a network card and a virtual GPU, and puts its screen
in a window. It installs Debian, Ubuntu and Fedora for you, unattended, from
media it has verified against pinned signing keys.

**There is no QEMU anywhere in it** — not as a process, not as a library, not as
a source of device models. The virtio devices, the interrupt controllers, the
ACPI tables and the PCI bus are this project's own code. The one piece of
outside firmware is an unmodified EDK2 `CloudHv` build, compiled from pinned
sources by a script in this repository.

- **[User guide](docs/user-guide.md)** — install, first VM, the GUI, the limits
- **[Troubleshooting](docs/troubleshooting.md)** — the failures people actually hit
- **[Architecture decisions](docs/adr/)** — why it is built this way

## What it can do

| | |
|---|---|
| Guests | Debian, Ubuntu Server, Ubuntu Desktop, Fedora Workstation — installed unattended from verified media |
| Hosts | Linux with `/dev/kvm`; native Windows/WHP; experimental Apple Silicon/macOS (limits below) |
| Boot | UEFI with a persistent variable store, or a direct Linux kernel boot with no firmware |
| Graphics | 2D scanout in a resizable window (1920×1080 is the tested target); a GPU desktop on either host — Vulkan, and OpenGL through Zink, on the host GPU (`[display] venus = true`, `install ubuntu --venus`); OpenGL through VirGL on a Linux host |
| Devices | virtio-blk, -net, -gpu, -input (keyboard, pointer, gamepad), -snd over virtio-mmio or virtio-pci with MSI-X |
| Network | a host TAP interface (Linux), or a user-mode NAT needing no administrator (both hosts) |
| Lifecycle | pause and resume, reboot in place, suspend to a file and restore it later |
| Interfaces | the `entangled` CLI and `entangled-manager`, a native desktop GUI |

## What it is not

- **Not a Windows-guest solution.** Linux guests are what is tested; assume
  Windows guests do not work.
- **Not a container runtime.** These are full VMs with their own kernel.
- **Not a server product.** No daemon, no API, no clustering, no live migration.
- **Windows releases are unsigned.** The Windows binaries are unsigned and no checksums are
  published; SmartScreen will warn, and it is right to.
- The GPU desktop needs a host Vulkan driver and a guest set up for it (the
  installer does that), there is no USB or PCI passthrough, and anti-cheat
  games will not run. The full list is in the
  [user guide](docs/user-guide.md#limits-in-one-place).

## Install

**macOS preview** — Apple Silicon and macOS 26 or later. The native app bundle
includes its Vulkan runtime; no global Vulkan SDK installation is needed.
The manager's Create machine wizard downloads verified Ubuntu ARM64 media and
installs GNOME with hardware graphics automatically. The equivalent CLI command
is `entangled install ubuntu --auto --disk ubuntu.raw --size 8G --memory-mib 4096`.
The current backend supports SMP (1–64 vCPUs within the host hypervisor limit),
user-mode networking and direct Linux boot. New machines use all available
Apple Silicon cores by default; choose fewer in the wizard or with `--vcpus`. Vulkan 1.3 and Zink OpenGL 2.1 are verified on Apple M3 Pro; presentation
currently copies pixels. Audio, pause, reset, suspend and ARM ISO/UEFI boot are
not connected. Longer stability testing is still in progress.

The drag-to-Applications DMG is published alongside the Windows installer and
Linux engine in [Releases](https://github.com/doctorspider42/entangled-destop/releases).
The release workflow builds a fresh ARM64 app with the same release version,
signs the app and DMG with Developer ID, submits the DMG to Apple, staples the
accepted notarization ticket and verifies Gatekeeper before publication.
A SHA-256 file accompanies the DMG. Earlier manually attached preview assets
may be signed without notarization; their release notes identify this.
Packaging and signing configuration are in
[installer/macos/README.md](installer/macos/README.md), with implementation and
acceptance evidence in [ADR-0007](docs/adr/0007-macos-arm64.md). The remaining
table and installation paths describe the Linux and Windows implementations.

**Windows** — download `entangled-desktop-<version>-setup.exe` from
[Releases](https://github.com/doctorspider42/entangled-destop/releases) and run
it. It is unsigned, so SmartScreen shows "Windows protected your PC": *More
info* → *Run anyway*, or build from source instead. You also need the *Windows
Hypervisor Platform* optional feature (admin, one reboot).

**Linux** — build it:

```bash
sudo apt-get install -y build-essential pkg-config curl
cargo build --workspace --release
entangled fetch firmware                  # the UEFI firmware, 4 MiB, digest checked
sudo usermod -aG kvm "$USER"              # then log out and back in
```

`bash guest/firmware/build-cloudhv.sh` builds the same firmware locally instead
(~2.5 min, Linux only). The Windows installer ships a copy next to the
program, so a Windows installation needs neither. The
[user guide](docs/user-guide.md#getting-the-software) has the optional packages
(3D, sound, TAP networking) and the five places the firmware is looked for.

## First VM

```bash
entangled doctor                              # can this machine run VMs?
bash scripts/fetch-ubuntu-iso.sh              # ~2.9 GiB, signature-checked
entangled install ubuntu --disk ~/entangled-vms/ubuntu.raw --size 20G --auto --headless
entangled run ~/entangled-vms/ubuntu.toml
```

Three to five minutes for the install, which is unattended: it creates the disk,
generates the answer file, drives the installer over the serial console and
waits for the guest to power itself off. It leaves an `ubuntu.toml` profile, an
`ubuntu.nvram` UEFI variable store (keep it — without it the machine has no boot
entry) and the installer's own transcript.

Or press **+ Create machine** in `entangled-manager` and let the wizard do it.

Debian is the same two commands with one extra in front, on either host:

```bash
entangled fetch bootstrap-kernel   # ~13 MiB: the Debian installer cannot run on
                                   # Debian's own kernel here, so we ship one
entangled install debian --disk ~/entangled-vms/debian.raw --size 32G --auto --headless
```

That kernel is upstream Linux, **GPL-2.0-only**, published with its corresponding
source; it is checked against a SHA-256 pinned in the binary, which is a weaker
guarantee than the pinned OpenPGP keys the ISOs get. Both facts are spelled out
in the [user guide](docs/user-guide.md#the-guest-bootstrap-artifacts).

## Development

This is also a working repository. [CLAUDE.md](CLAUDE.md) is the map: the
workspace layout, the build and test commands for both hosts, and the rules that
keep the core portable and the guest untrusted. The
[ADRs](docs/adr/) record the decisions and what each one cost, and
`.claude/skills/` holds a task-focused guide per subsystem.

```bash
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all
cargo deny check                     # no GPL/AGPL/LGPL in host code
```

The product backlog, in Polish, is
[vmhost-mvp-backlog.md](vmhost-mvp-backlog.md).

## License

Apache-2.0. Host-side dependencies are audited in CI (`cargo deny`) to exclude
copyleft licenses; guest-side content is unaffected.
