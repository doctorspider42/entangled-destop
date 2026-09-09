# Troubleshooting

The failures below are the ones people actually hit, in rough order of how often
they happen on a machine that has never run this before. Each one starts with
the message you will see, because that is what you will be searching for. Every
message quoted here is one this project really prints — the ones carrying
numbers were captured from a passing test on the development machine, not
written from memory.

The first thing to run, always:

```bash
entangled doctor
```

It checks the hypervisor, the artifacts, the VM directory and the default
network backend, and prints `MISSING` next to anything it could not find, with a
one-line fix under it.

- [`cannot read firmware artifacts/firmware/CLOUDHV.fd`](#cannot-read-firmware)
- [`/dev/kvm not found`, or "cannot be used"](#devkvm-not-found-or-cannot-be-used)
- [`WHP is not usable`](#whp-is-not-usable)
- [The VM boots to "no bootable option or device was found"](#no-bootable-option-or-device-was-found)
- [A 16 GiB disk really occupies 16 GiB](#a-16-gib-disk-really-occupies-16-gib)
- [`cannot attach to TAP interface entangled0`](#cannot-attach-to-tap-interface)
- [The guest filesystem is corrupt](#the-guest-filesystem-is-corrupt)
- [The window is black](#the-window-is-black)
- [The VM refuses to start because of 3D or sound](#the-vm-refuses-to-start-because-of-3d-or-sound)
- [The install seems stuck](#the-install-seems-stuck)
- [The installed Ubuntu takes two minutes to reach the login prompt](#the-installed-ubuntu-takes-two-minutes-to-reach-the-login-prompt)
- [Windows says "Windows protected your PC"](#windows-says-windows-protected-your-pc)
- [The guest's clock disagrees with the host's (WSL2)](#the-guests-clock-disagrees-with-the-hosts-wsl2)
- [The guest has no joystick](#the-guest-has-no-joystick)
- [`entangled resume` refuses the snapshot](#entangled-resume-refuses-the-snapshot)
- [The manager says a machine is stopped when it is running](#the-manager-says-a-machine-is-stopped-when-it-is-running)

## `cannot read firmware`

```text
cannot read firmware artifacts/firmware/CLOUDHV.fd: No such file or directory
```

The UEFI firmware is a **build artifact, not a file in the repository**, and a
fresh checkout — or a fresh git worktree, or a fresh Windows installation — does
not have it. Every UEFI boot needs it: `install ubuntu`, `install fedora`,
booting anything installed, `--cdrom`.

On Linux, build it once:

```bash
sudo apt-get install -y build-essential uuid-dev iasl nasm python3 git
bash guest/firmware/build-cloudhv.sh          # ~2.5 min
```

On Windows there is no build — copy `artifacts/firmware/CLOUDHV.fd` in from a
Linux checkout. Note the path is *relative to the working directory*, so a
manager launched from the Start menu looks for it under `Program Files`. Put the
file somewhere writable and set *Settings ▸ Advanced ▸ working directory* to its
parent, or give the machine an absolute `[boot] firmware` path.

The same applies to the two test artifacts (`scripts/fetch-test-kernel.sh`,
`scripts/build-test-initramfs.sh`) — built per checkout, never committed.

## `the Debian installer needs Entangled's own kernel and initramfs`

```text
error: the Debian installer needs Entangled's own kernel and initramfs, and this
host has neither artifacts/bootstrap/ nor a verified copy in the cache.
run `entangled fetch bootstrap-kernel` ...
```

Do what it says:

```console
$ entangled fetch bootstrap-kernel
guest bootstrap artifacts — Linux 6.12.9 (guest-artifacts-6.12.9-1)
  trust         : SHA-256 pinned in this build (guest/bootstrap-kernel/pinned.toml)
```

About 13 MiB, into the same cache the ISOs use, and `entangled install debian`
finds it there with no further configuration. This is the **only** way to get
those two files on Windows: they are a Linux kernel build and there is no cross
build, which is why the release pipeline builds them once and every host
downloads them. On Linux you can also build your own with
`bash guest/bootstrap-kernel/build.sh` (~15 min the first time), and a locally
built `artifacts/bootstrap/` always wins over a download.

Two variations worth knowing:

- **A download that fails the digest check is deleted**, and the error names the
  expected and the found SHA-256. Nothing unverified is ever kept, so the fix is
  to run it again — a truncated transfer cannot linger.
- **`ENTANGLED_BOOTSTRAP_DIR`** points at a directory holding `vmlinuz` and
  `initrd.img` and skips the download entirely: a mounted Linux checkout, a
  shared drive, an internal mirror. `ENTANGLED_BOOTSTRAP_BASE_URL` relocates the
  *download* instead, and the pinned digests are still enforced.

## `/dev/kvm not found`, or "cannot be used"

```text
/dev/kvm not found — KVM is unavailable (kernel module missing or no virtualization support)
/dev/kvm exists but cannot be used: Permission denied (check permissions — user must be in the 'kvm' group)
```

The first message means the kernel has no KVM: virtualization is disabled in the
machine's firmware setup, or you are inside a VM without nested virtualization,
or the `kvm_intel`/`kvm_amd` module is not loaded.

The second is the common one and it is a permissions problem:

```bash
sudo usermod -aG kvm "$USER"
# log out and back in — a new group is not applied to a running session
ls -l /dev/kvm            # expect  crw-rw---- 1 root kvm
```

`newgrp kvm` gets you a shell with the new group without logging out.

## `WHP is not usable`

```text
WHP is not usable — enable the "Windows Hypervisor Platform" optional feature
(Windows Features, or `dism /Online /Enable-Feature /FeatureName:HypervisorPlatform`)
and reboot
```

Exactly what it says. In an **administrator** prompt:

```powershell
dism /Online /Enable-Feature /FeatureName:HypervisorPlatform /All
```

then reboot. Enabling it does not conflict with WSL2 or Hyper-V — all three use
the same hypervisor. If it is already ticked and the message persists, check
that virtualization is enabled in the machine's firmware setup.

## "no bootable option or device was found"

The firmware ran, found the disk, and had no boot entry telling it what to start.
Almost always this means the machine's **`.nvram` file is gone** — deleted,
renamed, left behind when the disk was copied, or never configured in the
profile.

The UEFI boot entry that points at the installed bootloader
(`\EFI\ubuntu\shimx64.efi` and so on) does not live on the disk. It lives in the
variable store, which is the `.nvram` file beside it. Delete that file and you
have a perfectly good disk that nothing knows how to boot.

What to do:

1. Check the profile has an `nvram` line under `[boot]` and that the file it
   names exists. `entangled disk inspect <disk>` reports the sidecar too.
2. If you copied a disk, copy its `.nvram` with it. Always.
3. If the file is genuinely lost, the firmware may still boot the disk through
   its removable-media fallback (`\EFI\BOOT\BOOTX64.EFI`), which most distros
   install. A boot that says `Boot#### "UEFI Misc Device"` instead of
   `Boot0006 "Ubuntu"` is exactly that: it worked, and the variable store did
   not. Booting the guest once and running `sudo grub-install` (or
   `efibootmgr`) inside it writes a fresh entry into the new store.
4. If nothing boots, reinstall. There is no way to reconstruct the variable
   store from outside the guest today.

## A 16 GiB disk really occupies 16 GiB

Disk images are sparse: a 16 GiB image should occupy only what the guest has
actually written. Two situations break that.

**The image is on a Windows drive seen from WSL** (`/mnt/c`, `/mnt/d`, …). The
drvfs filesystem cannot create sparse files, so the whole size is allocated up
front, and it is slower besides. Keep VM images on a native Linux filesystem —
`~/entangled-vms` inside the WSL distribution. The manager warns about this when
it starts a machine whose files are on a Windows drive.

**The guest freed the space but nobody told the host.** Deleting files inside
the guest does not shrink the image. Ask the guest to hand the blocks back:

```bash
sudo fstrim -av           # inside the guest
```

or mount its filesystems with `discard`. `entangled disk inspect` prints
apparent size against real size and reminds you of this whenever the two differ.

## `cannot attach to TAP interface`

```text
cannot attach to TAP interface entangled0: Operation not permitted (os error 1)
cannot attach to TAP interface entangled0: Device or resource busy (os error 16)
cannot attach to TAP interface entangled0: No such device
```

TAP networking is Linux-only and needs the host interface to exist first,
created once by root:

```bash
sudo scripts/setup-tap.sh                    # entangled0 with NAT, owned by you
sudo scripts/setup-tap.sh --down             # remove it again
```

- *Operation not permitted* — the interface exists but is not owned by you, or
  `/dev/net/tun` is not accessible. Re-run the script with `--user <you>`.
- *Device or resource busy* — **another VM already has it.** One TAP interface
  serves one VM; give the second machine its own (`--iface entangled1`, and
  `interface = "entangled1"` in its profile).
- *No such device* — it was never created, or a reboot removed it.

Or sidestep all of it: `backend = "usernet"` in the profile (`--network usernet`
for an install) runs a NAT inside the `entangled` process. No host interface, no
root, works identically on both hosts. It does outbound TCP, UDP, DHCP and DNS;
what it does not do is let anything connect *in*.

## The guest filesystem is corrupt

Ask first whether two VMs were ever running on the same writable image. That is
the one way to destroy a guest filesystem with no bug involved anywhere: two
kernels each believe they own the disk, and each writes metadata over the
other's. Nothing in the tool prevents it.

If you want a second machine from an existing one, copy the image *and* its
NVRAM sidecar first:

```bash
cp --sparse=always ~/entangled-vms/ubuntu.raw ~/entangled-vms/ubuntu2.raw
cp ~/entangled-vms/ubuntu.nvram ~/entangled-vms/ubuntu2.nvram
```

Then run `fsck` inside a rescue boot of the guest. The same rule is why
`entangled resume` refuses a snapshot whose disk has changed size or timestamp:
a restored guest's page cache describes a filesystem that no longer exists, and
letting it write is the same corruption by another route.

## The window is black

- **During a UEFI boot from an ISO, that is expected for a while.** The
  CloudHv firmware has no graphics driver at all, so nothing is drawn until the
  guest's own `virtio_gpu` driver binds — the firmware and GRUB are on the
  *serial console* instead, which is your terminal. Watch there.
- **After the guest is up**, check the run log for a line
  `virtio device activated … device=Gpu`. If the device never activated, the
  guest's driver did not bind.
- To see what the guest thinks it is showing without a window at all, use
  `entangled run --headless --screenshot-after 60 --screenshot shot.png`.

## The VM refuses to start because of 3D or sound

```text
libvirglrenderer.so.1 not found — install libvirglrenderer1 (Debian/Ubuntu) or disable [display] virgl
libasound.so.2 not found — install libasound2 (Debian/Ubuntu) or set [sound] backend = "null"
```

This is deliberate: a machine that was promised a GPU or a named audio output
fails loudly instead of silently running software rendering or silence. Install
the library, or change the profile — `[sound] backend = "auto"` takes whatever
the host has and never blocks a start.

On a Windows host, 3D is not available at all (see the
[limits](user-guide.md#limits-in-one-place)); the manager greys the option out
and says so.

## The install seems stuck

Installs are long: 3–5 minutes for Ubuntu Server, 30–60 for a Fedora
Workstation over the network. Before assuming it is wedged, read the transcript
it is writing next to the disk — `<name>-install.log` — which is the installer's
own console output. Look for, in order:

```text
GRUB menu is up                    the firmware booted the installer media
typing into GRUB                   the host typed the installer's command line
subiquity/load_autoinstall_config  the answer file was found and read
reboot: Power down                 the install finished and powered off
```

- Nothing after "GRUB menu is up" — the keystrokes never landed; the ISO may be
  a variant whose menu differs.
- `Continue with autoinstall?` sitting on screen — the automation flag never
  reached the guest kernel and the installer is waiting for a human.
- No `Power down` — it did not finish; whatever the last step in the log is, is
  where it stopped.

In the manager, the same log is in the Activity pane, and a failed task gets a
one-sentence diagnosis appended.

## The installed Ubuntu takes two minutes to reach the login prompt

Expected, and not your fault. An Ubuntu installed by `entangled install ubuntu`
gets a profile with **no network card**, because the install itself is offline.
The installed system still runs `systemd-networkd-wait-online` and
`cloud-init-network`, and each waits out its timeout before giving up. The boot
log shows both.

Adding `[network] backend = "usernet"` to the profile gives the machine a NIC,
but the installed system also needs a netplan that expects one before the wait
disappears entirely.

## Windows says "Windows protected your PC"

SmartScreen, because the installer is not code-signed. *More info* → *Run
anyway*. The UAC prompt will say **Publisher: Unknown** for the same reason.
There is no published checksum to verify the download against either; if that is
not acceptable, build from source instead — see the
[user guide](user-guide.md#either-host-build-from-source).

## The guest's clock disagrees with the host's (WSL2)

**Resolved 2026-09-09: the guest is right and the WSL2 host's clock is wrong.**
This section used to say the opposite, so if you are here from an older note,
read on.

A guest of ours running on KVM inside WSL2 reports its monotonic clock some
**1–4 % slower** than the WSL host's. The guest is keeping real time. WSL2's own
`CLOCK_MONOTONIC` runs fast — it advances as though the CPU's time-stamp counter
were slower than it is (1 835.4 MHz observed against a real 1 896.4 MHz) — and
the size of the error **wanders**: +3.8 % and +0.8 % were measured ninety
minutes apart in one WSL session. So it is the *host* clock that is fast, and by
an amount that will not be the same when you check.

How to check it yourself, on any WSL distribution, with no VM involved:

```bash
# CLOCK_MONOTONIC against the externally-disciplined wall clock.
python3 -c 'import time; m=time.monotonic(); r=time.time(); time.sleep(120); print("monotonic %.3f  realtime %.3f" % (time.monotonic()-m, time.time()-r))'
```

On an affected WSL host the monotonic figure comes back 1–4 % larger. A second
check from the Windows side: time the same command with PowerShell's
`Measure-Command` — Windows QPC agrees with WSL's wall clock, not with its
monotonic clock.

What this means in practice:

- **The guest's time of day is fine**, on WSL and everywhere else. KVM hands the
  guest the true TSC frequency, and both the `tsc` clocksource and kvm-clock
  derive from it. Measured against the host's *wall* clock, a two-hour guest run
  is within about 100 ppm.
- **Do not benchmark inside WSL** and expect the numbers to be real seconds —
  everything timed there, in a guest or not, reads ~3 % long.
- **Windows/WHP is unaffected.** The same guest on the same machine under WHP
  measures +10 ppm against the host clock it reads through the emulated ACPI PM
  timer.
- Nothing in the VMM can correct a wrong host clock: KVM computes the guest's
  TSC scaling ratio from the same host frequency it advertises to the guest, so
  the error cancels out of any adjustment we could make. If it bothers you,
  restart WSL (`wsl --shutdown`, then start it again) — the frequency is
  established when the WSL VM boots.

## The guest has no joystick

A machine only has a gamepad if its profile asks for one — it is off by default,
like the sound card, because it costs a device slot and because an existing
profile has to keep describing the machine it always described:

```toml
[gamepad]
enabled = true
```

or *Configure ▸ Network & display ▸ Give this machine gamepads* in the manager,
where the number of players sits next to the switch.
Then, inside the guest:

- `cat /proc/bus/input/devices` should list `Entangled Gamepad`. If it does not,
  the device was never attached — check the host's run log for
  `virtio device activated … device=Input`.
- Player 1's pad is `/dev/input/js0`, player 2's is `js1`. The machine's
  absolute pointer used to take `js0` (it matches `joydev`'s id table too, and
  is attached first); it is now shaped so `joydev` refuses it, which is what
  leaves `js0` to the first pad. Enumerating by name (`Entangled Gamepad`) plus
  the `Uniq=` serial (`player-1`, `player-2`) is still the robust way to tell
  two pads apart.
- **No rumble, in any guest.** Linux's virtio-input driver has no
  force-feedback support — it never asks the device about `EV_FF` and never
  creates the kernel plumbing an effect upload needs — and the virtio-input
  specification has no message that could carry an effect anyway. `EVIOCGBIT`
  on the pad reports no `EV_FF`, and that is the device reporting the truth,
  not a missing option.
- No `/dev/input/js*` **at all**, for any device, means the guest kernel has no
  `joydev`: it is a separate config symbol from `evdev`, and a module rather
  than built in on some distribution kernels (`modprobe joydev`). The pad's
  `event*` node exists either way, and SDL uses that one.
- A pad that exists but nothing can open belongs to the udev tag: without
  `ID_INPUT_JOYSTICK` your desktop user gets no ACL on the node. `udevadm info
  /dev/input/eventN` shows the tags.

The host end is chosen by `backend`: `auto` (whatever this host has), `evdev`
(Linux), `xinput` (Windows) or `null` — a pad the guest can see and the host
never moves, which is what a headless run wants. None of them fail because no
controller is plugged in; plugging one in later is enough.

## `entangled resume` refuses the snapshot

A snapshot is bound to the machine, the hypervisor and the disks it was taken
from, and a restore that guessed would corrupt a filesystem. Every refusal names
itself:

```text
not an Entangled snapshot: the file does not start with the "ENTGLSNP" magic
snapshot is truncated: section index needs 78803098 more bytes, 39401549 are left
snapshot format version 8 cannot be restored by this build (it writes and reads version 1)
snapshot header carries unknown flags 0x1; it was written by a newer build
snapshot was taken on Windows/WHP and this is Linux/KVM: a saved CPU carries that
hypervisor's own interrupt-controller and extended-state blobs, which the other one
cannot load
snapshot section memory[0] is corrupt: its contents do not match the recorded digest
disk /home/you/entangled-vms/ubuntu.raw has changed since the snapshot was taken
(size: was 8388608, is 16777216); restoring onto it would corrupt the guest's filesystem
```

There is also a refusal for the machine itself having changed — different
memory, different vCPU count — saying which field, was what, is what.

None of these is recoverable, and none of them should be worked around: boot the
machine normally instead and lose the saved session. The two you can avoid are
the last two: do not edit or resize a machine that has a session saved (the
manager lets you, and warns on the Configure hover), and do not touch its disk
from the host in between. `entangled snapshot <file>` answers all of this
**without** starting anything, and the manager's Snapshots view shows the same
verdict as a sentence under each row, before you click Resume.

## The manager says a machine is stopped when it is running

The manager tracks VMs as child processes and does not adopt orphans. Restart
the manager while a VM it started is running, and the card goes back to
*Stopped* while the machine keeps running; pressing Start then fails with
whatever resource the live VM still holds (a busy TAP interface, most visibly).
For the same reason, Pause, Restart and Suspend are greyed out on a VM this copy
of the manager did not start — those need the control pipe to a child process.

A **suspended** machine is the one state that does survive this: it is the
absence of a process plus the presence of a `<name>.esnap` file, so a manager
that has just started reads it correctly off the disk.

Find the process and stop it the normal way:

```bash
pgrep -a entangled          # Linux
Get-Process entangled       # Windows
```

Prefer `Ctrl+Alt+Q` or an ACPI shutdown from inside the guest over killing the
process; a killed VMM leaves the guest filesystem in whatever state it was in.
