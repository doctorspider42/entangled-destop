# Entangled Desktop — user guide

This guide takes a stranger from an empty machine to a running Linux desktop VM,
on either supported host. It is written for someone who has never seen this
project; if you are working *on* the VMM, read
[CLAUDE.md](../CLAUDE.md) and the [ADRs](adr/) instead.

Every command below was either run on the development machine while writing this
guide or copied verbatim from a test that passes. The
[provenance table](#provenance-of-the-commands-in-this-guide) at the end says
which is which, so you can tell tested instructions from plausible ones.

- [What this is](#what-this-is)
- [What this is not](#what-this-is-not)
- [Hosts and prerequisites](#hosts-and-prerequisites)
- [Getting the software](#getting-the-software)
- [Check the host first: `entangled doctor`](#check-the-host-first-entangled-doctor)
- [Quickstart: Ubuntu Server](#quickstart-ubuntu-server)
- [Quickstart: Fedora Workstation](#quickstart-fedora-workstation)
- [Quickstart: Debian](#quickstart-debian)
- [Living with a VM](#living-with-a-vm)
- [Machines on disk](#machines-on-disk)
- [Pause, reboot, suspend and restore](#pause-reboot-suspend-and-restore)
- [The desktop manager](#the-desktop-manager)
- [Limits, in one place](#limits-in-one-place)
- [Troubleshooting](troubleshooting.md)

## What this is

Entangled Desktop is a virtual machine monitor — the program that creates a
virtual machine, gives it memory, virtual disks, a virtual network card and a
virtual graphics card, and puts its screen in a window on your desktop.

It is written from scratch in Rust on top of the hypervisor your operating
system already has:

- **Linux**, through KVM (`/dev/kvm`);
- **Windows**, through the Windows Hypervisor Platform (WHP) — natively, not
  inside WSL.

There is **no QEMU** anywhere in it: not as a process it launches, not as a
library it links, not as a source of device models. Everything the guest sees —
the virtio devices, the interrupt controllers, the ACPI tables, the PCI bus — is
this project's own code. The UEFI firmware is the one exception and it is
honest about itself: an unmodified EDK2 `CloudHv` build, compiled from pinned
sources by a script in this repository.

What it can do today:

| | |
|---|---|
| Guests | Debian, Ubuntu Server, Ubuntu Desktop, Fedora Workstation — installed by the tool itself, unattended, from media it verified |
| Boot | UEFI firmware with a persistent variable store, or a direct Linux kernel boot with no firmware at all |
| Graphics | 2D scanout in a resizable window; 1920×1080 is the size the project targets and tests. 3D (OpenGL through VirGL) on a Linux host |
| Devices | virtio-blk, virtio-net, virtio-gpu, virtio-input (keyboard, absolute pointer and an Xbox-shaped gamepad), virtio-snd (playback), over virtio-mmio or virtio-pci with MSI-X |
| Network | a host TAP interface (Linux), or a user-mode NAT that needs no administrator and no host setup (both hosts) |
| Lifecycle | pause and resume, reboot in place, suspend to a file and restore it in a new process |
| Disks | sparse RAW images: create, inspect, grow, move between drives, reclaim freed space through discard |
| Interfaces | the `entangled` command-line tool and `entangled-manager`, a native desktop GUI |

## What this is not

- **Not a Windows-guest solution.** Nothing about the machine model is
  Windows-specific and nobody has made a Windows guest boot; assume it does not
  work. Linux guests are what is tested, on every commit.
- **Not a container runtime.** These are full virtual machines with their own
  kernel. If you want process isolation with a shared kernel, use containers.
- **Not a server or cloud product.** There is no daemon, no REST API, no
  clustering, no live migration. One VM is one process you started.
- **Not a QEMU or VirtualBox replacement.** It runs the guests listed above,
  well; it does not have twenty years of device models, USB passthrough, PCI
  passthrough, snapshots-as-a-tree or a disk format of its own.
- **Not signed software.** The Windows binaries are not code-signed, and
  Windows will say so. See [Getting the software](#getting-the-software).

## Hosts and prerequisites

### Linux (x86-64)

- KVM: the file `/dev/kvm` must exist and be readable and writable by you.
  On Debian and Ubuntu that means being in the `kvm` group:

  ```bash
  sudo usermod -aG kvm "$USER"      # then log out and back in
  ls -l /dev/kvm
  ```

- A CPU with hardware virtualization enabled in firmware (Intel VT-x or AMD-V).
  Inside another hypervisor you need nested virtualization; WSL2 on Windows 11
  provides it, which is how this project's Linux side is developed.
- For the window: a Wayland or X11 session, and a GPU with a working Vulkan or
  GL driver (the window is rendered with wgpu; llvmpipe works but is slow).
- Optional, feature by feature:
  - **3D acceleration** — `libvirglrenderer` and a GL driver. It is opened at
    runtime with `dlopen`, never linked, so its absence costs you 3D and
    nothing else ([ADR-0004](adr/0004-virtio-gpu-3d.md)).
  - **Sound** — ALSA (`libasound`), also opened at runtime.
  - **TAP networking** — `CAP_NET_ADMIN` or root, once, to create the host
    interface (`scripts/setup-tap.sh`). The user-mode NAT needs none of this.

### Windows (x86-64)

- Windows 10 or 11 with the **Windows Hypervisor Platform** optional feature
  enabled. It is off by default. In an administrator PowerShell:

  ```powershell
  Enable-WindowsOptionalFeature -Online -FeatureName HypervisorPlatform -All
  ```

  or tick *Windows Hypervisor Platform* in "Turn Windows features on or off".
  Either way, **reboot**. This coexists with WSL2 and Hyper-V; all three ride
  the same hypervisor.
- Virtualization enabled in the BIOS/UEFI setup, as above.
- No administrator rights are needed to *run* VMs once the feature is on, and
  the user-mode network backend needs none either.

## Getting the software

### Windows: the installer from GitHub Releases

Releases live at
<https://github.com/doctorspider42/entangled-destop/releases>. Each one carries
exactly one file, `entangled-desktop-<version>-setup.exe`. It is an Inno Setup
installer; it asks for administrator rights, installs `entangled.exe` and
`entangled-manager.exe` side by side into `C:\Program Files\Entangled Desktop`,
adds a Start-menu entry (and a desktop icon if you tick the box), and offers to
launch the manager when it finishes. Installing over an existing copy upgrades
it in place.

Two things to know before you run it:

- **The binaries are not code-signed, and no checksums are published.** Windows
  SmartScreen shows "Windows protected your PC" and hides the *Run anyway*
  button behind *More info*; the UAC prompt says **Publisher: Unknown**. That is
  accurate — nobody has bought a code-signing certificate for this project, and
  the release workflow publishes no SHA-256 file and no signature. The whole
  chain of trust is "GitHub served it to you over HTTPS from this repository".
  If that is not enough for you, build from source; the build is a single
  `cargo build`.
- **`entangled.exe` is not added to `PATH`.** Call it by its full path, or add
  the install directory to `PATH` yourself. The manager finds it on its own,
  because it sits next to it.

### Either host: build from source

You need the Rust stable toolchain (`rustup` from <https://rustup.rs>; the
channel is pinned by `rust-toolchain.toml`, minimum 1.85) and a checkout of this
repository. On Debian or Ubuntu the build itself needs very little:

```bash
sudo apt-get install -y build-essential pkg-config curl
cargo build --workspace --release
```

That produces `entangled` and `entangled-manager` in `target/release/`. There is
no OpenSSL dependency (TLS is rustls, OpenPGP is a pure-Rust implementation) and
no GTK dependency; the window's Wayland, X11 and GL libraries are opened at
runtime, not linked, so nothing beyond the above is needed to *compile*. To
*run* a VM with a window on a minimal system you also want the libraries a
desktop already has:

```bash
sudo apt-get install -y libwayland-client0 libxkbcommon0 libx11-6 libxcb1 \
    libxkbcommon-x11-0 mesa-vulkan-drivers libgl1
```

Optional, and each one is `dlopen`ed at runtime so its absence costs only its
own feature:

```bash
sudo apt-get install -y libvirglrenderer1   # 3D acceleration (Linux hosts only)
sudo apt-get install -y libasound2          # guest sound
sudo apt-get install -y curl gnupg coreutils  # the ISO fetch scripts
```

Two things are **not** in the repository because they are build artifacts, and a
fresh checkout has neither:

```bash
sudo apt-get install -y build-essential uuid-dev iasl nasm python3 git
bash guest/firmware/build-cloudhv.sh      # the UEFI firmware, ~2.5 min, once
bash guest/bootstrap-kernel/build.sh      # only needed for `install debian`
```

`build-cloudhv.sh` checks out pinned EDK2 sources into
`~/.cache/entangled-edk2` and builds `CLOUDHV.fd` into `artifacts/firmware/`.
**Without it, every UEFI boot fails with `cannot read firmware …`** — the single
most common first-run failure. There is no Windows build of it.

### The firmware on a Windows install

The Windows installer does **not** ship `CLOUDHV.fd` — the firmware is built on
Linux — and `entangled` looks for it at the *relative* path
`artifacts\firmware\CLOUDHV.fd`, resolved against the working directory. Started
from the Start menu, that working directory is the install folder under
`Program Files`, which has no `artifacts` folder and is not writable. So on a
fresh Windows installation `entangled doctor` reports:

```text
    firmware         MISSING at artifacts/firmware/CLOUDHV.fd
```

and UEFI machines cannot start. The fix is two settings:

1. put `CLOUDHV.fd` somewhere writable, e.g.
   `%USERPROFILE%\entangled\artifacts\firmware\CLOUDHV.fd`, copied from a Linux
   checkout;
2. in the manager, *Settings ▸ Advanced ▸ working directory*, point at
   `%USERPROFILE%\entangled` — or give each machine an absolute
   `[boot] firmware` path in its profile, which the machine editor's *Boot &
   media* section does for you.

## Check the host first: `entangled doctor`

Before anything else, ask the tool whether this machine can run VMs at all:

```console
$ entangled doctor
entangled doctor
  KVM API version : 12
  max vCPUs       : 4096
  memory slots    : 32764
  required caps   : all present
  install         : ubuntu — UEFI + verified ISO, offline (no mirror needed)
                    debian — d-i on the bootstrap kernel, needs the network
    firmware         artifacts/firmware/CLOUDHV.fd (4.0 MiB)
    bootstrap kernel artifacts/bootstrap/vmlinuz (12.5 MiB)
    ubuntu ISO      /home/you/.cache/entangled/ubuntu/26.04/ubuntu-26.04-live-server-amd64.iso (2.7 GiB)
    VM directory    /home/you/entangled-vms (781 GiB free of 1007 GiB)
    network         --network tap by default on this host
host looks ready to run VMs
```

It reports the hypervisor it found, which install paths are available on this
host, which artifacts are present, where VMs will be stored and how much room is
left there. If it cannot open the hypervisor at all it says why — that message
is the first thing to quote in a bug report.

On Windows it answers for WHP instead, and the differences it lists are the ones
that matter in practice:

```console
> entangled doctor
entangled doctor
  hypervisor      : Windows Hypervisor Platform present
  processor vendor: AMD
  interrupt chips : 8259/8254/IOAPIC emulated in this process
  …
  networking      : backend = "usernet" — user-mode NAT in this process (DHCP,
                    DNS relay, outbound TCP), no TAP and no administrator
  VMs per process : 1 — WHP maps guest memory for one partition per process,
                    so a second VM needs a second `entangled` process
    firmware         artifacts/firmware/CLOUDHV.fd (4.0 MiB)
    VM directory    C:\Users\you\entangled-vms (108 GiB free of 953 GiB)
    network         --network usernet by default on this host
host looks ready to run VMs
```

(The `install` line lists the Ubuntu and Debian paths only; Fedora works on both
hosts but is not yet reported there.)

## Quickstart: Ubuntu Server

This is the portable path: it works the same on Linux and on Windows, it boots
through UEFI, and the install never touches the network — every package comes
out of the ISO.

**1. Get the firmware and the ISO.** The ISO is verified against Canonical's
signing key, pinned by fingerprint in this repository; nothing is used before
both its OpenPGP signature and its SHA-256 check pass.

```bash
bash guest/firmware/build-cloudhv.sh          # once, ~2.5 min
bash scripts/fetch-ubuntu-iso.sh              # once, ~2.9 GiB, verified
```

On Windows there is no `fetch-ubuntu-iso.sh` — it is a bash script. Either run
it once in WSL (the cache is `%LOCALAPPDATA%\entangled` on Windows and
`~/.cache/entangled` on Linux, so copy the ISO across), or download the ISO
yourself and pass `--iso <path>` to the install command below.

**2. Install.** One command; it creates the disk, generates the unattended
answer file, boots the installer, types the installer's kernel command line into
GRUB over the serial console, and waits for the guest to power itself off:

```bash
entangled install ubuntu --disk ~/entangled-vms/ubuntu.raw --size 20G --auto --headless
```

Drop `--headless` to watch it in a window. Expect **3 to 5 minutes** on a
reasonably fast machine; the acceptance test measures 189 s for the install on
this project's Windows host and 4 min 27 s on a loaded Linux one.

It finishes with a line naming what it produced, for example:

```text
installed: GPT with an ESP on /dev/vda1 (953 MiB) and root on /dev/vda2 (ext4 UUID …)
```

and leaves four files beside the disk:

| File | What it is |
|---|---|
| `ubuntu.raw` | the disk |
| `ubuntu.nvram` | the UEFI variable store, holding the boot entry that points at the installed bootloader — **delete this and the machine boots to "no bootable option" with a perfectly good disk attached** |
| `ubuntu.toml` | the VM profile: what to run |
| `ubuntu-install.log` | the installer's own console output, the only account of what it did |

**3. Run it.**

```bash
entangled run ~/entangled-vms/ubuntu.toml
```

The first boot takes a couple of minutes: cloud-init generates SSH host keys,
and a machine with no network card waits out two systemd timeouts before the
login prompt. Add `--headless` to keep it on the terminal instead of opening a
window.

**For a desktop rather than a server**, fetch the Desktop ISO and point the same
command at it, with room to install into and enough memory for GNOME:

```bash
iso=$(bash scripts/fetch-ubuntu-iso.sh desktop)     # ~6 GiB, same trust chain
entangled install ubuntu --iso "$iso" --disk ~/entangled-vms/desktop.raw \
    --size 40G --memory-mib 4096 --auto --headless
```

An explicit `--memory-mib` carries through to the installed machine's profile,
which matters here: a desktop sized at 4096 for the install must not boot into
the 2048 MiB default afterwards.

## Quickstart: Fedora Workstation

Same shape, one difference that matters: the Fedora netinst image installs *from
the network*, so the installer VM needs one. `--network usernet` gives it a
user-mode NAT that runs inside the `entangled` process — no TAP, no root, no
host configuration.

```bash
bash scripts/fetch-fedora-iso.sh netinst      # once, ~1.2 GiB, verified
entangled install fedora --disk ~/entangled-vms/fedora.raw --size 24G \
    --network usernet --auto --headless
entangled run ~/entangled-vms/fedora.toml
```

Anaconda is driven by a kickstart file on a small ISO9660 volume labelled
`OEMDRV`, which the installer generates. A full Workstation install over the
network takes **30 to 60 minutes** — most of it downloading packages.

To look at Fedora without installing anything, boot the Live image:

```bash
iso=$(bash scripts/fetch-fedora-iso.sh)
entangled run --cdrom "$iso" examples/ubuntu-desktop-live.toml
```

`--cdrom` attaches any ISO read-only as the last virtio-blk device and lets the
firmware boot it. It requires `mode = "uefi"` and `transport = "pci"`, and says
so rather than booting a machine that cannot find the media.

## Quickstart: Debian

The Debian path predates the firmware: it boots the Debian installer's kernel
and initrd directly, with a preseed file appended to the initrd, and the
installed system is then started through a **bootstrap kernel** this project
builds — a small kernel plus initramfs that mounts the installed root from
`/dev/vda` and `switch_root`s into it.

That bootstrap kernel only builds on Linux, so **`install debian` is
Linux-only**. On Windows, install Ubuntu or Fedora instead, or copy an
`artifacts/bootstrap/` directory in from a Linux checkout.

```bash
entangled fetch debian --channel stable --arch amd64 --variant gtk-netboot
entangled install debian --disk ~/entangled-vms/debian.raw --size 32G --auto
entangled run ~/entangled-vms/debian.toml
```

`entangled fetch` resolves the current Debian stable release from signed
metadata, verifies an OpenPGP signature over the checksum file against keys
pinned in this repository, checks the SHA-512 of every artifact, and writes a
provenance manifest next to it. A second run over an intact cache does no
network access at all; `--offline` makes that a requirement rather than an
outcome, and `--refresh` re-checks the signed sums, which is how a new point
release is picked up.

The Debian installer needs a package mirror, so this path needs working
networking: `--network usernet` for the in-process NAT, or `--network tap` with
a host interface created once by `scripts/setup-tap.sh`.

## Living with a VM

### The window

The VM's screen is an ordinary window. Nothing you type or click reaches the
guest until you **grab** input, so the window behaves like every other window
until you ask it not to.

| Action | Result |
|---|---|
| click on the guest image | grab input: keyboard, pointer and wheel now go to the guest |
| `Ctrl+Alt` (pressed and released with nothing in between) | release the grab; held keys are released in the guest too |
| `Ctrl+Alt+G` | toggle the grab explicitly |
| `Ctrl+Alt+Q` | ask the VM to stop |
| `Ctrl+Alt+P` | pause / resume the guest |
| `Ctrl+Alt+R` | reboot the machine in place |
| `Ctrl+Alt+S` | suspend to the snapshot file and stop |
| `Ctrl+Alt+O` | 1:1 pixel mode (no scaling) |
| `F11` | borderless fullscreen |
| any other `Ctrl+Alt+<key>` | forwarded to the guest, so `Ctrl+Alt+F2` still switches its console |

The window title says whether input is grabbed. Losing focus releases the grab
and every key the guest was told is down.

### Gamepads

A machine can have a controller as well as a keyboard and a pointer. It is off
by default — it costs a device slot, and an existing profile has to keep
describing the machine it always described — so ask for it:

```toml
[gamepad]
enabled = true
# backend = "auto"     # auto | null | evdev | xinput
```

or tick *Give this machine a gamepad* in the manager's machine editor, under
*Network & display*.

What the guest gets is an **Xbox 360-shaped pad**: eleven buttons, two sticks,
two analogue triggers and a hat, with exactly the capability set that makes
Linux's `joydev` publish a `/dev/input/js*` node, udev tag the device
`ID_INPUT_JOYSTICK` (which is what gives your desktop user permission to open
it), and SDL — and therefore Steam and most games — derive a complete controller
mapping without any database entry. It does **not** claim Microsoft's USB
vendor and product ids to get one; it says it is a virtual device, and earns the
mapping from its capabilities instead.

`backend` chooses how the host reads a real controller: `evdev` reads
`/dev/input/event*` directly on Linux, `xinput` uses XInput on Windows, `auto`
takes whichever of those this host has, and `null` gives the guest a working
pad that the host never moves — which is what a headless or CI run wants.
`auto` never stops a machine from starting, and in particular does not care
whether a controller is plugged in.

Plugging and unplugging while the VM runs works in both directions, and so does
swapping one pad for another: the host reports the controller's whole state and
the device sends the guest only what changed. A controller that disappears
reads as neutral, so held buttons are released and sticks re-centre rather than
the guest running forward for ever.

Three things it does not do yet: **no rumble** (there is no force-feedback path
back to the host pad), **one pad per machine**, and **no host-side deadzone or
response curve** — the only deadzone is the one the guest's own driver applies
to the `ABS_INFO` the device publishes, which is `xpad`'s. Note also that the
pad is **not necessarily `/dev/input/js0`** — the acceptance run for it found the
machine's absolute pointer claiming `js0` and the pad landing on `js1` — so
enumerate by name (`Entangled Gamepad`) rather than by number.

### Without a window

```bash
entangled run --headless ~/entangled-vms/ubuntu.toml
```

The guest still has a graphics device and still draws; the scanout is simply
off-screen. The serial console is on your terminal, which is where an installed
Ubuntu prints its boot log and offers a login prompt. Two flags make a headless
VM inspectable:

```bash
entangled run --headless --screenshot-after 60 --screenshot boot.png ~/entangled-vms/ubuntu.toml
entangled run --frame-stats frames.json ~/entangled-vms/desktop.toml
```

`--screenshot-after N` writes a PNG of the scanout N seconds in (and refreshes
it periodically), which is how an unattended graphical boot is watched.
`--frame-stats` mirrors the GPU's frame statistics — mean interval and fps, 1 %
and 0.1 % lows, duplicate and dropped frames — into a JSON file rewritten every
120 frames, for comparing two runs with a diff.

### Driving a VM from another program

```bash
entangled run --headless --control-stdin ~/entangled-vms/ubuntu.toml
```

reads one lifecycle command per line on stdin: `pause`, `resume`, `reset`,
`save [path]`, `type <text>` (types on the guest's serial console) and `status`.
This is how the GUI manager's buttons work, and how the end-to-end tests log
into a guest.

Each one is answered on stdout, interleaved with the guest's own console output
and prefixed so a reader can pick it out of the noise:

```text
entangled-control: ok pause
entangled-control: state=Running
entangled-control: saved /home/you/vm.esnap 75.1 MiB written in 3.54s (75.1 MiB of 256 MiB guest RAM in 228 runs, 1 vCPUs)
entangled-control: error save <why it could not be written>
```

`ok <command>` means accepted, not finished. `saved` and `error save …` are the
two ways a suspend ends, and they are the last thing that VM says — it exits
either way. The vocabulary is a wire format between two programs, so it lives in
one place (`control_api::control`) rather than being spelled out twice.

## Machines on disk

A machine is a TOML profile plus the files it names. Nothing is hidden in a
database; the profile an install writes is small enough to read:

```toml
name = "ubuntu"
memory_mib = 2048
vcpus = 2
transport = "pci"

[boot]
mode = "uefi"
firmware = "artifacts/firmware/CLOUDHV.fd"
nvram = "/home/you/entangled-vms/ubuntu.nvram"
cmdline = ""

[[disk]]
path = "/home/you/entangled-vms/ubuntu.raw"
writable = true

[display]
width = 1280
height = 800
scale = 1.0

[sound]
enabled = true

[gamepad]
enabled = true
```

An installed machine gets the sound card and the gamepad switched on for you —
both with `backend = "auto"`, which is the setting that can never be the reason
a VM fails to start. `examples/ubuntu-installed.toml` is the same shape with a
comment on every key.

Things worth knowing about the shape:

- **`transport`** is `"pci"` or `"mmio"`. UEFI boot requires `pci`: the CloudHv
  firmware has no virtio-mmio driver at all, so on mmio it would boot happily
  and then find no disks. The tool refuses that combination instead.
- **`[boot] mode`** is `"uefi"` or `"direct-linux"`. Direct boot takes `kernel`,
  `initramfs` and `cmdline` and skips firmware entirely — it is how the test
  guests and the Debian path start.
- **`nvram`** is the UEFI variable store. It is per machine, it travels with the
  disk, and an installed system needs it to be bootable.
- **`[[disk]]`** repeats; the order is the order the guest sees
  (`/dev/vda`, `/dev/vdb`, …). `writable = false` is enforced twice: the file is
  opened read-only *and* the device advertises itself as read-only and fails
  writes.
- **`[network]`** picks `backend = "tap"` (Linux only) or `"usernet"`; omit the
  section for a machine with no network card.
- **`[display]`** sets the initial scanout size (`width`, `height`) and window
  `scale`; the guest can change modes within it. `virgl = true` asks for 3D
  (Linux hosts only), and `refresh_hz` sets the refresh rate the guest is told
  about, which is the ceiling its compositor paces itself to.
- **`[sound]`** is off unless you add it: `enabled = true` and a `backend` of
  `"auto"` (whatever the host has), `"null"`, `"alsa"` or `"wasapi"`. `auto`
  never stops a machine from starting; a named backend the host cannot open
  does, on purpose.
- **`[gamepad]`** is off unless you add it, the same way: `enabled = true` and a
  `backend` of `"auto"`, `"null"`, `"evdev"` (Linux) or `"xinput"` (Windows).
  See [Gamepads](#gamepads) below.

Every device costs a **slot**, and there are eight of them on either bus. A
machine with one disk spends four (disk, GPU, keyboard, pointer); a network card
is a fifth, a sound card a sixth and a gamepad a seventh, which leaves room for a
CD-ROM *or* a second disk but not both. Going over is refused when the machine
is built, with the count in the message, rather than producing a guest that is
quietly missing a device.

Disk images are sparse RAW files — no proprietary format, `dd` and `losetup`
understand them:

```console
$ entangled disk create --size 8G docs-demo.raw
created docs-demo.raw (8589934592 bytes, sparse)

$ entangled disk inspect ubuntu.raw
ubuntu.raw
  size:   20.0 GiB apparent (21474836480 bytes) · 2.9 GiB on disk
  reclaim: run `fstrim -av` in the guest (or mount it with `discard`) to hand
           unused blocks back to the host: the image then takes less room on
           disk, without ever looking smaller to the guest
  table:  GPT (disk GUID AA4D21DD-0CDD-4963-9E51-72F4772CE2E5)
  nvram:  ubuntu.nvram (UEFI variable store — travels with the disk)
  partitions:
    1  EFI System             953 MiB  LBA 2048+1951744
    2  Linux filesystem      19.1 GiB  LBA 1953792+39987200  ext4 80fab32b-…
```

`entangled disk resize --size 48G <path>` grows an image (shrinking is refused —
it would cut guest data off the end). `entangled disk move` relocates one to
another directory or drive, preserving sparseness, verifying the copy before
deleting the source and updating the profiles that reference it.
`entangled disk rm` refuses while any profile still names the image.

One hard rule the tooling cannot enforce for you: **never run two VMs on the
same writable disk image.** Two guests with the same filesystem mounted
read/write will corrupt it. Copy the image (sparsely, with its `.nvram`) if you
want a second machine.

## Pause, reboot, suspend and restore

- **Pause** (`Ctrl+Alt+P`, or `pause` on the control channel) stops every vCPU
  and quiesces the devices, so nothing touches guest memory while it is frozen.
  Resume continues the same boot.
- **Reset** (`Ctrl+Alt+R`, or `reset`) reboots the machine in place, in the same
  process: the devices are returned to power-on state and the firmware runs
  again. A guest that reboots itself takes the same path.
- **Suspend** (`Ctrl+Alt+S`, or `save [path]`) writes the whole machine — CPU
  state, memory, device state — to a `.esnap` file and stops. Restore it with:

  ```bash
  entangled resume ~/entangled-vms/ubuntu.esnap
  entangled snapshot ~/entangled-vms/ubuntu.esnap   # just describe the file
  ```

**A resume does not delete the snapshot**, on purpose: a restore that fails on
startup has to be retriable, and `entangled resume` points a later bare `save`
back at the same file, which is what makes closing and reopening a machine a
loop rather than a one-way trip. There is one consequence worth knowing before
it surprises you: if you resume a machine and then **stop** it instead of
suspending it again, it goes back to *Suspended* holding the old file — and once
the resumed guest has written to its disk, that file is one the engine will
refuse. The manager says exactly that on the card, and *Start fresh…* is the way
out; it deletes the stale session with you watching.

`entangled snapshot` opens no disk and allocates no memory; it tells you what
the file is of, when it was taken, what it contains and whether *this* machine
could restore it — including for a snapshot taken on the other hypervisor, where
it says so instead of failing.

Restore is deliberately strict, and every refusal names itself: a snapshot from
the other hypervisor, from a build with a different format version, one that is
truncated or has a bad section digest, or **one whose disks have changed size or
timestamp since it was taken**. That last one is not pedantry — a restored guest
believes its page cache still describes the filesystem, and letting it loose on
an image that has moved on writes stale metadata over the new contents.

## The desktop manager

`entangled-manager` is a native GUI for the same machines — egui rendered
through wgpu, no browser and no Electron. It drives the `entangled` CLI as child
processes, so anything it does you can also do by hand, and its log pane shows
the same output you would have seen in a terminal.

Start it with no arguments; `--vm-dir <dir>` points it at a different folder for
one run.

```bash
entangled-manager
```

### The views

**Machines** is where it opens: one card per VM profile found in the VM
directory (`~/entangled-vms`, `%USERPROFILE%\entangled-vms` on Windows,
changeable in Settings). A card shows the machine's memory, vCPUs, screen size,
network interface, how big its disk is and how much of that is really allocated,
and — while it runs — uptime, CPU and a small CPU graph. The buttons follow the
state:

| State | Buttons |
|---|---|
| **Stopped** | **Start** |
| **Running** | **Stop**, **Pause** / **Resume**, **Restart**, **Suspend** |
| **Stopping** | **Kill** (shutdown was asked for; force it) |
| **Installing** | **Abort** |
| **Suspending** | none — it is writing itself to a file and will stop when it has |
| **Suspended** | **Resume**, **Start fresh…** |

plus **Configure** on every card — allowed while a machine is stopped or
suspended, with the warning that changing its hardware is what makes a saved
session unrestorable — and a **More** menu holding *Copy profile path*, *Open
activity log*, *Forget the saved session* (when there is one) and *Delete
machine*. A card whose disk file has gone missing says so and offers no Start
button.

Pause, Restart and Suspend need the control channel, which only exists for a VM
*this manager started*; on a machine someone launched from a terminal they are
greyed out and say so rather than pretending.

**Suspended** is the one state that is a fact on disk rather than something the
manager is remembering: a machine is Suspended when it
has no child process **and** there is a `<name>.esnap` beside its profile. That
is why a suspended machine still reads correctly after the manager has been
closed and reopened, where a *running* one does not (see the limits below). Its
card says when the session was saved and how big it is, **Resume** is only bright
when that session could really go back — the reason it could not is on the hover,
before the click, not in an error afterwards — and **Start fresh…** asks before
throwing the session away, because booting the machine from scratch is exactly
what makes it unrestorable.

**Create machine** opens a four-step wizard — *System*, *Hardware*, *Storage*,
*Review*. You pick Debian or Ubuntu and its installer media, name the machine
and size its memory and vCPUs, either create a new sparse disk or install onto
an existing image, and then read a summary before anything happens. The last
step has an *Advanced: command preview* section showing the exact
`entangled install …` command it is about to run, which is the honest way to
learn the CLI.

**Configure** (the machine editor) edits the profile in four sections, all saved
together: *Hardware* (memory, vCPUs, `mmio`/`pci`), *Boot & media* (direct-Linux
kernel and command line, or UEFI firmware, NVRAM store and disc), *Network &
display* (network backend and interface, MAC, screen size, 3D, sound and its
output backend, and whether the machine gets a gamepad), *Storage* (the ordered
list of disks — the order *is*
`/dev/vda`, `/dev/vdb`, …). Edits go through the same validation `entangled run`
would apply, before the file is written, so a profile the GUI saved always
starts. A machine cannot be edited or deleted while it runs, and it cannot be
renamed: too many file names are derived from it.

**Storage** lists every disk image in the VM directory and every image a profile
references: apparent size against space actually on disk, partition table,
whether an NVRAM sidecar exists, and which machines it is attached to. From here
you can create, attach, detach, grow, move and delete images. Moving copies
sparse-preserving, verifies the copy, updates every profile that referenced the
image and only then deletes the original. Shrinking is not offered at all,
because it can only destroy data.

**Snapshots** — *Saved sessions* — lists every `.esnap` file in the VM
directory: which machine it is of, when it was taken, how big it is on paper and
on disk, and whether it could be resumed **here**. A snapshot is bound to the
hypervisor it was taken on, to the build that wrote it and to the disks it was
pinned to, so a row is often something that cannot be used — and each such row
carries the reason as a plain sentence under it rather than as an error after
the click: *"This was saved on Windows/WHP and the machine is set to run on
Linux/KVM. A saved processor carries its own hypervisor's state, which the other
one cannot load."* Each row offers **Resume**, **Reveal** (open the containing
folder) and **Delete**; the header carries the totals, because "how much is this
costing me" is the usual reason to open a list of very large files.

**Diagnostics** answers "can this computer run machines?" It shows which engine
binary the manager found, which backend new machines will use, the capability
matrix for this host, and the full output of `entangled doctor` run through that
backend — with the exact command line shown so you can repeat it in a terminal.

**Activity** is the console pane at the bottom. Every VM and every install is a
child process whose output is written to `<vm-dir>/<name>-run.log` or
`<name>-install.log` and tailed live here. It opens by itself when an install
starts and when something fails, and a failure gets one plain sentence of
diagnosis appended — "the TAP interface is already held by another VM", "/dev/kvm
is not usable", and so on.

### Where a machine runs, and why it remembers

On Windows there are two ways to run a VM: **Windows (WHP)**, natively, and
**WSL (KVM)**, which runs the Linux build of `entangled` inside your WSL
distribution. The picker is labelled **RUNS ON** and appears in Settings (the
default for new machines), in the wizard, and in each machine's editor.

The choice is stored **per machine, in the manager's own `manager.toml`** — not
in the VM profile. That is deliberate: a profile is meant to boot on either host
([ADR-0002](adr/0002-linux-first-whp-ready.md)), so writing the host into it
would make the file machine-specific. A machine remembers the backend it was
installed on, because that is the one where its files, its firmware and its
capabilities line up — and because the backend decides which other settings are
even legal, so the editor needs a stable answer to grey things against. On a
Linux host, where there is only one backend, the picker degrades to a line of
text.

If a machine's files are somewhere WSL cannot reach — a UNC path, or a path
without a drive letter — the manager refuses to start it there and says what to
do instead. If they are on a Windows drive that WSL *can* reach, it starts and
warns: that filesystem cannot store sparse images, so a 16 GiB disk really
occupies 16 GiB.

### Greyed-out controls

When the manager disables a control it is saying "this would fail at boot", and
it always says why: a short line under the control, and the long explanation as
a tooltip. Four capability gates exist today, all of them the same statement —
*this needs the Linux/KVM backend*:

| Greyed out | Because |
|---|---|
| **Accelerate 3D graphics** | the host renderer speaks EGL, which the Windows host has no equivalent of yet; a 2D machine still gets a desktop |
| **TAP networking** | there is no TAP device on Windows, and the drivers that would provide one are GPL, which this product cannot ship — `usernet` needs no host setup anyway |
| **Debian** in the wizard | the Debian path boots this project's own bootstrap kernel, which builds on Linux only |
| **ALSA / WASAPI** sound output | each is one host's audio system; `auto` takes whatever the host it lands on has and never blocks a start |

The same checks run again when you press Save or Create, so a setting changed
while a form was open cannot slip through. Separately, the manager checks before
launching that the artifacts a profile needs are actually there — the firmware
for a UEFI machine, the bootstrap kernel for a Debian install — and tells you
where to get the missing one rather than letting the VM fail obscurely.

### Updates

If "Check for updates on startup" is on, the manager asks GitHub once, on a
background thread, whether there is a newer release. Being offline is silently
fine — the check never produces a popup. When there is one, a banner appears
above the machine list with the new version number, a **Download & install**
button on Windows (it saves the installer to your Downloads folder and starts
it; the installer upgrades in place and relaunches the manager), a **Release
page** link otherwise, and **Skip** to hide the banner for this session.

## Limits, in one place

Everything here is a real limitation of the current build, not a caveat about
configuration:

1. **No Windows guests.** Untested and unsupported; Linux guests are what CI
   boots.
2. **3D is Linux-host only.** VirGL runs on `libvirglrenderer`, which is opened
   at runtime on Linux. A Windows host gets 2D. Vulkan-through-Venus plumbing
   exists in the protocol and the device, but **nothing on any host decodes a
   Vulkan command stream yet**, so Venus renders nothing.
3. **No zero-copy scanout.** Every presented frame is read back from the
   renderer into host memory and uploaded to the window's texture. On the
   development host that readback is 13 ms of a 20 ms frame — the current cap on
   3D frame rates. A dmabuf path is designed and feature-detected at runtime;
   no host in this project has a DRM node to use it.
4. **Anti-cheat games will not run.** Kernel-level anti-cheat refuses to run in
   a VM by design, and this VMM does not hide itself. Nor is there GPU
   passthrough.
5. **`install debian` needs a Linux host**, because the bootstrap kernel it
   boots the installed system with has no cross build. `install ubuntu` and
   `install fedora` are portable.
6. **Suspend refuses a changed disk.** Resuming onto an image whose size or
   mtime moved is refused by design (see above). Snapshots are also
   uncompressed, unencrypted, and always full — there is no incremental
   suspend — so the file is about the size of the guest's touched RAM.
7. **A resumed guest loses some things by construction**: network connections,
   3D contexts, in-flight audio, and the wall clock (the RTC comes from the host,
   so a guest suspended for an hour resumes an hour behind and corrects itself
   over NTP).
8. **TAP networking is Linux-only.** On Windows the user-mode NAT is the only
   backend — which is also the option that needs no administrator anywhere.
   The NAT does outbound TCP, UDP, DHCP and DNS; it is not a general-purpose
   router, and nothing reaches the guest from outside without port forwarding
   it does not yet have.
9. **The Windows binaries are unsigned, unverifiable and not on `PATH`.**
    SmartScreen warns, the UAC prompt says *Publisher: Unknown*, and the release
    publishes no checksum or signature to check the download against.
10. **The Windows installer does not ship the UEFI firmware**, so a fresh
    Windows install cannot start a UEFI machine until you copy `CLOUDHV.fd` in
    from a Linux checkout (see above).
11. **`entangled fetch` only knows Debian.** Ubuntu and Fedora ISOs come from
    the two bash fetch scripts, which need a Linux shell (`curl`, `gpg`,
    `sha256sum`) — on Windows, run them under WSL or download the ISO yourself
    and pass `--iso`.
12. **No USB, no PCI passthrough, no shared folders, no clipboard sharing, no
    disk format other than RAW, no port forwarding into the user-mode NAT.**
13. **One VM per process, and on Windows one WHP VM per process** — a hypervisor
    limit, not a design choice. Run several `entangled run` processes for
    several machines.
14. **Nothing adopts an orphaned VM.** If the manager is restarted while a VM it
    started is running, its card shows *Stopped* while the machine is very much
    alive — and Pause, Restart and Suspend are greyed out on any VM this copy of
    the manager did not start, because the control channel is a pipe to a child
    process. A *suspended* machine is the exception: that state is a file on
    disk, so it survives a restart of the manager.
15. **The guest's clock runs slow.** Measured at 1.26 % over two hours on the
    development host, and worse the busier the host is; a guest running NTP
    corrects it invisibly, one without NTP will drift. See
    [troubleshooting](troubleshooting.md#the-clock-inside-the-guest-falls-behind)
    for what has and has not been ruled out.
16. **One gamepad per machine, and no rumble.** The pad is an Xbox 360-shaped
    virtio-input device; force feedback has no path back to the host controller,
    there is no second player, and there is no host-side deadzone or response
    curve (deliberately — the guest's own calibration would not be able to see
    it).

## Provenance of the commands in this guide

| Command | How it was verified |
|---|---|
| `entangled doctor` | run on both hosts while writing this guide; both outputs pasted verbatim (the Windows one abridged) |
| `entangled disk create --size 8G …`, `disk inspect` | run while writing this guide; output pasted verbatim |
| `entangled --help`, `disk/install/run/fetch/resume/snapshot --help` | run while writing this guide; every flag named here comes from that output |
| `entangled install ubuntu --disk … --size … --auto --headless` | copied from `apps/entangled/tests/ubuntu_install.rs`, which passes on both hosts |
| `entangled install fedora --disk … --size … --network usernet --auto --headless` | copied from `apps/entangled/tests/fedora_install.rs` |
| `entangled run --headless <profile>` | copied from the same tests |
| `bash guest/firmware/build-cloudhv.sh`, `scripts/fetch-ubuntu-iso.sh`, `scripts/fetch-fedora-iso.sh` | the scripts' own documented invocations; the firmware and the Ubuntu ISO were both present and used on the machine this guide was written on |
| `entangled fetch debian …`, `install debian …` | from the CLI's help output and the Debian install path's documentation; not re-run for this guide |
| `entangled install ubuntu --iso <desktop iso> …` | the server command with the flags the CLI documents; the Desktop variant is what `tests/boot/tests/desktop_gnome.rs` boots, but this exact line was not re-run for the guide |
| `Enable-WindowsOptionalFeature -Online -FeatureName HypervisorPlatform -All` | the standard Windows spelling of the feature this project requires; the feature is enabled on the development host |
| the clock-drift numbers | `cargo test -p boot-tests --test soak -- --ignored --nocapture` for two hours on this machine, plus three shorter control runs; the numbers are this run's own output, and the run **fails** on them |
| the manager's views | rendered while writing this guide with `cargo run -p entangled-manager -- --mock --screenshot <png> --screenshot-view main\|wizard\|diagnostics`, and again for the Snapshots work with `--screenshot-view snapshots\|snapshot-delete\|snapshot-discard\|editor-network`; described from the pictures and the source |
| the control channel's replies | `cargo test -p entangled --test suspend_restore -- --nocapture`, which passes; the `saved …` line is one of its own, with the path shortened |
| `[gamepad] enabled = true`, and what the guest makes of the pad | `cargo test -p boot-tests --test gamepad -- --nocapture` was run here and passes: the guest kernel reports `name=Entangled_Gamepad … keys=11 axes=8 absx=-32768:32767:16:128`. The `js*` half of it self-skipped, because this checkout's bootstrap kernel predates `CONFIG_INPUT_JOYDEV=y` — so **`js0` vs `js1` is quoted from GAME-2104's acceptance run** (recorded in the backlog), not observed here |
