---
name: vm-testing
description: Testing strategy for Entangled Desktop — unit/malicious-guest tests, boot-to-marker integration tests, 100-boot and soak runs, fuzzing, screenshot comparison, and how to run KVM tests in WSL/CI (backlog EPIC 14 + per-epic acceptance criteria). Load when writing or running tests.
---

# VM testing

Scope: backlog EPIC 14 (MVP-1401…1410) and the acceptance criteria sprinkled
through every epic. Integration tests live under `tests/{boot,installer,graphical}`;
unit tests live with their crates.

## Test tiers

1. **Pure unit tests** — validation, parsing, state machines, geometry.
   Run everywhere (`cargo test --workspace`), no KVM needed. This tier must
   stay the majority: extract logic into pure functions precisely so it
   lands here.
2. **Malicious-guest tests** (MVP-1402 baseline): every virtio device gets
   adversarial cases — looped descriptor chains, out-of-range
   indices/addresses/rects, oversized allocations. Assert the typed error,
   assert no panic. `ChainWalkGuard` tests in `virtio-core` are the pattern.
3. **KVM tests** — need `/dev/kvm`. Must self-skip with an `eprintln!` note
   when it is absent or PermissionDenied (pattern:
   `vmm-core/src/hypervisor.rs` tests) so plain CI stays green.
4. **Boot integration tests** (MVP-208): boot a test kernel+initramfs, scan
   captured serial output for `linux_boot::GUEST_READY_MARKER` with a
   deadline; kernel panic string → immediate failure with the full serial
   log in the failure message. No marker within the deadline → failure, not
   a hang.

   **A serial transcript is neither UTF-8 nor plain text**, and both halves of
   that have cost a full acceptance run on this project. Read it through
   `apps/entangled/tests/common::read_transcript`, or the same two lines:

   - `std::fs::read` + `String::from_utf8_lossy`, never `read_to_string`. An
     installed Ubuntu sets up its console font by writing every code point from
     0x00 to 0xFF, so the log stops being valid UTF-8 partway through;
     `read_to_string(..).unwrap_or_default()` turns that into an **empty**
     transcript, and a marker poll over an empty transcript can only time out.
     It did — for the whole six-minute deadline, on a login prompt that had been
     printed at 136 s of guest uptime, and the failure it finally reported named
     an unrelated component.
   - **Strip the ANSI escapes before matching.** systemd colours the
     distribution name, so what is on the wire is
     `ESC[0;1;39mWelcome to ESC[0mESC[1mUbuntu 26.04 LTS`, and
     `contains("Welcome to Ubuntu")` can never match. A marker split by an
     escape sequence is not a marker — pick uncoloured markers where you can,
     and strip where you cannot.
5. **Endurance** (MVP-1403/1404): 100 sequential boots and the 8-hour soak
   are `#[ignore]`d tests invoked explicitly (nightly CI / manual), never in
   the default suite.
6. **Graphical** (MVP-1405/1406): screenshot the scanout (see host-display
   skill), compare against goldens with a small per-pixel tolerance; store
   goldens under `tests/graphical/golden/` as PNG (small resolutions for
   tests, e.g. 640×480, plus one 1920×1080 case).

## Running

- Local (Windows host): the KVM side through WSL —
  `wsl -d Ubuntu -e bash -lc "cd /mnt/d/entangled-desktop && cargo test --workspace"`
  (WSL2 exposes `/dev/kvm`; the dev user must be in the `kvm` group) — and the
  WHP side natively: `cargo test --workspace` in PowerShell runs the whole
  suite including the `whp_*` acceptance boots (`whp_boot`, `whp_virtio_blk`,
  `whp_virtio_pci`, `whp_smp`, `whp_usernet`, `whp_uefi`), each self-skipping
  without the optional feature or the artifacts.
- CI: standard GitHub runners now expose `/dev/kvm` on Linux; tier 3-4 tests
  run there, tiers 5-6 are scheduled jobs. The `windows-latest` job builds,
  lints and tests the workspace and asserts the `whp_*` self-skip path stays a
  loud, working path (no WHP on GitHub's runners).
- Docker: `docker run --device /dev/kvm …` (see `docker/Dockerfile.dev`).

## Guest test images

- Built from `guest/` configs (EPIC 11: reproducible bootstrap kernel +
  initramfs). Binaries are cached build artifacts, **never committed to git**.
- The minimal test initramfs `/init` prints the ready marker, optionally
  runs a scripted probe (mount `/dev/vda`, `evtest`, DHCP check), prints a
  per-check `VMHOST_TEST_OK <name>` / `VMHOST_TEST_FAIL <name>` line, then
  calls `reboot(RESTART)`: with `reboot=k` that ends in a triple fault which
  reaches the host as a clean KVM_EXIT_SHUTDOWN, and it works on every machine
  we boot. Test harnesses parse only these markers — never scrape free-form
  kernel output.
- `entangled.poweroff=1` switches the exit to `reboot(POWER_OFF)`, i.e. the ACPI
  S5 path, and makes the probe report which tables the guest found in
  `/sys/firmware/acpi/tables`. Opt-in on purpose: it exercises the FADT, the
  DSDT's `\_S5` and the host's ACPI PM block, which is a different claim from
  "the guest booted". Pair it with `BootSpec::with_poweroff_probe()`, which
  waits for the *guest* to end the VM (`BootOutcome::ended_by_guest`) instead of
  stopping it from the host — otherwise a broken S5 path looks like a pass.
  **On WHP it is the only guest-initiated ending**: the `reboot=k` triple fault
  that cleanly stops a KVM VM parks a WHP vCPU with no exit at all (see the
  whp-backend skill), so a WHP run that waits for the guest must use S5.
- `entangled.netprobe=<ip>/<prefix>,<gateway>,<host>:<port>` configures eth0
  statically (ioctls — no DHCP client exists in this initramfs), opens a TCP
  connection to `<host>:<port>` and requires its greeting echoed back — the
  guest half of the user-mode-NAT acceptance
  (`crates/vmm-core/tests/whp_usernet.rs`). TX alone is a SYN; only the echo
  proves RX delivery.
- Sources: `guest/test-rootfs/init-rs` (static musl init), built by
  `scripts/build-test-initramfs.sh`; kernel via `scripts/fetch-test-kernel.sh`.

## Endurance: the 100-boot test (MVP-1403)

`tests/boot` is a workspace member holding a shared headless boot harness
(`boot_tests::boot_once`: kernel + initramfs, optional virtio-blk disk, chosen
queue-notify mode, serial capture, time to the ready marker) plus two
`#[ignore]`d tests built on it.

```bash
# 100 sequential boots; ~4 s each, so ~7 minutes.
cargo test -p boot-tests --test repeat_boot -- --ignored --nocapture
```

It asserts that the host process does not grow — file-descriptor count and
thread count *identical* to the settled baseline (taken after five warm-up
boots), RSS within 32 MiB — and that every boot reaches `VMHOST_GUEST_READY`.
The leak assertions run first, so a run with stalls still reports them.

**Original result (100 boots, before any interrupt topology existed): 73/100
reached the marker; fds and threads exactly flat, RSS 4080 → 4124 KiB.** Nothing
leaked, but about a quarter of boots stalled at exactly `Run /init as init
process` — the first userspace write to the interrupt-driven 8250 tty (`printk`
before it uses the polled path) — waiting for a transmitter-empty interrupt on
IRQ 4 that never arrived. With a disk attached the same defect stalled the first
disk read with `INTERRUPT_STATUS` still reading `INT_VRING`. Root cause: no MP
table or MADT, so Linux used virtual-wire ExtINT through the 8259 instead of the
IOAPIC. See the `IrqFdLine` docs in `machine_x86::virtio`. Do not "fix" this by
loosening the test.

**After the MP table and the MADT (25 boots with a virtio-blk disk):
25/25 reached the marker, 3677/4056/5124 ms min/median/max, fds and threads flat,
RSS 4244 → 4272 KiB.** Interrupts now route through the IOAPIC from the MADT
(`ACPI: Using ACPI (MADT) for SMP configuration information`); re-run the full
100 before claiming EPIC 14's acceptance number.

Knobs:

- `ENTANGLED_BOOT_ITERATIONS=<n>` — shorten the run while iterating.
- `ENTANGLED_BOOT_DEADLINE_SECS=<n>` — per-boot deadline (default 30; a healthy
  boot takes ~4 s, so 20 keeps a stall-heavy run quick).
- `ENTANGLED_BOOT_DISK=1` — attach a scratch virtio-blk disk.
- `ENTANGLED_SCRATCH_DIR=<dir>` — where scratch images go. On the Windows
  development host this **must** be a native Linux path (`$HOME/…`): the drvfs
  mount holding the repository cannot create sparse files.
- `ENTANGLED_QUEUE_NOTIFY=sync` — run everything on the pre-MVP-307
  synchronous notify path.

The queue-notify measurement uses the same harness:

```bash
cargo test -p boot-tests --test notify_bench -- --ignored --nocapture
```

## ACPI tests

Details and the expected serial output are in the `acpi-machine` skill; what
matters here is the shape of the coverage.

- **Tier 1** (`machine_x86::acpi`, 30 tests): per-table length/checksum/field
  checks, the two power-off writes, write-1-to-clear register semantics, and
  bounds on guest accesses to the PM block. Runs on Windows too.
- **Tier 1, external** (`crates/machine-x86/tests/acpi_dump.rs`, `#[ignore]`d):
  dumps the tables so `iasl -d` can decode them. An AML change is not reviewed
  until its disassembly has been read.
- **Tier 3** (`vmm-core/tests/smoke.rs`): a real guest writes S5 to `0x600` and
  then spins forever, so only `ExitHandler::shutdown_requested` can end the run
  loop. Bounded with `join_or_stop`, so a regression fails instead of hanging.
- **Tier 4** (`tests/boot/tests/acpi.rs`, 4 tests): the guest kernel must find
  our tables and use the MADT for SMP, a 2-vCPU guest must see 2 CPUs, `acpi=off`
  must still boot through the MP table, and `poweroff` must end the VM through
  ACPI. `--nocapture` prints every ACPI/APIC line the guest produced, which is
  the evidence for any claim about this area.
- **Tier 4** (`tests/boot/tests/uefi_acpi.rs`): EDK2 CloudHv must install our
  tables (`OnRootBridgesConnected` prints `InstallAcpiTables: <status>` only on
  failure) and find the right number of CPUs.

```bash
cargo test -p boot-tests --test acpi -- --test-threads=1 --nocapture
cargo test -p boot-tests --test uefi_acpi -- --nocapture
```

## Thin-provisioning reclaim (`VIRTIO_BLK_F_DISCARD`)

`tests/boot/tests/blk_discard.rs` is the pattern to copy for any feature whose
value is a **host-side effect the guest cannot report**: it boots the same guest
twice over the same image and compares an out-of-band measurement.

- Boot one sets `ENTANGLED_BLK_DISCARD=off`, so the device withholds both
  reclaim features: the guest fills 512 MiB, frees it, asks for the space back
  and is refused. That is the "before", and it is a *measured* before rather
  than a remembered one.
- Boot two runs with reclaim on and the guest's `fstrim` succeeds.
- The verdict is `disk_image::allocated_bytes` either side — the product's own
  helper, not a second implementation — plus an assertion that the guest's
  `/sys/block/vda/queue/discard_*` values are the config-space numbers the device
  published. Without that second check a green test could mean the guest never
  asked for anything.

The guest side is the test init's `entangled.trim=<mib>` probe, which prefers
the `FITRIM` ioctl on a mounted ext4 (what `fstrim(8)` issues) and falls back to
`BLKDISCARD` only when the kernel has no ext4 *at all*. That distinction is
load-bearing: falling back while a filesystem is mounted would write raw over
it, and the second boot would have nothing left to trim. It also means the
**bootstrap kernel** (`CONFIG_EXT4_FS=y`) is what makes the real `fstrim` path
testable — the Debian-installer test kernel has ext4 as a module, so
`artifacts/bootstrap/vmlinuz` is tried first and the test kernel is the fallback.

```bash
bash guest/bootstrap-kernel/build.sh          # or copy an existing artifact
cargo test -p boot-tests --test blk_discard -- --nocapture --test-threads=1
```

The image goes to `~/entangled-vms/discard-test.raw` (WSL-native): on drvfs
(`/mnt/*`) a sparse file allocates everything up front, so the test skips there
rather than failing on the host filesystem's behalf.

## UEFI firmware tests (EPIC 18)

Three layers, matching the tiers above. Boot mode is a config choice, so the
existing device/serial tests are unaffected by any of it.

1. **Portable** (`crates/uefi-boot`, tier 1): firmware-image classification
   (PVH ELF vs flash blob, with truncated/absurd headers), reset-vector ROM
   placement arithmetic, and the `hvm_start_info`/`hvm_memmap_table_entry` byte
   layout. Two of these encode invariants worth keeping honest — a 4 MiB ROM
   must land at `0xffc0_0000` (OVMF's own `FW_BASE_ADDRESS`), and no `e820_map`
   entry may ever overlap the ROM window.
2. **Machine devices** (`machine_x86::platform`, `machine_x86::rtc`, tier 1):
   the host bridge must answer `0x8086:0x0d57` to a 16-bit read at `00:00.0`
   offset 2, the ACPI PM timer must advance, the RTC must report a valid BCD
   date with UIP clear and VRT set, and port `0x70` must read back. Each of
   these was a firmware assert before it was a test — see the bring-up table in
   [ADR-0003](../../../docs/adr/0003-uefi-firmware.md).
3. **KVM** (`crates/uefi-boot/tests/reset_vector.rs`, tier 3): maps a fake ROM
   whose last 16 bytes hold `mov al,0x42; out 0x10,al; mov al,0x43; out 0x10,al;
   jmp $`, then runs the vCPU **without setting a single register** and asserts
   the two port writes arrive. This is the only test of the claim the whole
   reset-vector mode rests on: a fresh KVM vCPU already *is* the architectural
   reset state (`CS.base 0xffff_0000`, `IP 0xfff0`, PE clear), so the first
   fetch lands at `0xffff_fff0` inside the ROM. Use `jmp $`, not `ud2`: an
   exception would depend on the IDT, and this test must not touch the very
   state it is verifying.
   `crates/vmm-core/tests/cpuid.rs` guards the related trap — each vCPU must
   report *its own* index as the initial APIC ID, not the host CPU's, in **every**
   leaf that carries one (1, `0xb`, `0x1f`, `0x8000_001e`, `0x8000_0026`; Linux
   prefers `0x8000_001e` on AMD hosts).
4. **KVM, full firmware boot** (`tests/boot/tests/uefi_acpi.rs`, tier 4): boots
   the real `artifacts/firmware/CLOUDHV.fd` to the Boot Manager and asserts on
   its log — ACPI tables installed, CPU count right, no `ASSERT`. Self-skips
   without the firmware artifact. This is the automated version of the manual
   bring-up below; run it after any change to the machine's firmware-facing
   devices or to the ACPI tables.
5. **KVM, the whole ISO boot chain** (`tests/boot/tests/uefi_iso.rs`, tier 4,
   `#[ignore]`d): see below. This is the one that would have caught all four of
   ADR-0003's phase-3 gaps, and the one to run before claiming any change to
   PCI, interrupts or the block device is safe.
6. **KVM, persistent UEFI variables** (`tests/boot/tests/uefi_nvram.rs`, tier 4,
   *not* ignored — it needs only the firmware and ~10 s): boots the firmware
   **twice against one NVRAM file** and asserts the flash device is accepted
   (`QemuFlashDetected => FD behaves as FLASH, writable`), the RAM-backed store
   stands down (`Disabling EMU Variable FVB …`), `BootOrder`/`Boot0000` end up in
   the *file*, and the second boot **reuses** them (zero blocks erased, fewer
   bytes programmed). Run it after any change to `machine_x86::pflash`, to
   `layout::PFLASH_*`, or to `guest/firmware/build-cloudhv.sh` — those three have
   to agree, and when they do not the only symptom is an installed guest that
   stops booting after its second start. A firmware built with
   `ENTANGLED_FW_PFLASH=0` fails this test, which is the intended behaviour.
7. **The whole install** (`apps/entangled/tests/ubuntu_install.rs`, tier 4,
   `#[ignore]`d): `entangled install ubuntu` and then `entangled run` of the
   profile it wrote. See "Installing Ubuntu" below.

Manual firmware bring-up (needs `bash guest/firmware/build-cloudhv.sh` once,
~2.5 min, ~2 GiB of EDK2 checkout in `~/.cache/entangled-edk2`):

```bash
cargo run -p entangled -- run --headless examples/uefi-firmware.toml
```

A DEBUG-build EDK2 is extremely chatty on ttyS0, and that log *is* the
diagnostic tool: read it forwards, and treat the first `ASSERT [Phase]
File.c(line)` as the next required machine feature rather than as a firmware
bug. That profile has no disks, so a healthy run ends at `BdsDxe: No bootable
option or device was found.` — the firmware works and has nothing to boot.

## Booting an installer ISO (UEFI-1803)

```bash
bash guest/firmware/build-cloudhv.sh          # once, ~2.5 min
bash scripts/fetch-ubuntu-iso.sh              # once, ~2.9 GiB, GPG + SHA-256 verified
cargo test -p boot-tests --test uefi_iso -- --ignored --nocapture
```

`tests/boot/tests/uefi_iso.rs` boots the real firmware with the real ISO on a
read-only virtio-blk over PCI and asserts the *chain*, in the order the log
produces it: no firmware `ASSERT`; `FSOpen: Open '\EFI\BOOT\BOOTX64.EFI'
Success` **and** `BdsDxe: starting Boot…` (opening proves the GPT + FAT ESP of an
isohybrid image were read, starting proves `LoadImage` succeeded — a broken image
logs only the first); the device path pinned to `Pci(0x2,0x0)/HD(2,GPT`, so it
cannot pass on a target disk that happened to be bootable; GRUB's banner *and*
one of the ISO's own menu entries; then `ExitBootServices`. ~35 s.

It self-skips without `/dev/kvm`, without the firmware, or without an ISO — found
via `$ENTANGLED_UBUNTU_ISO` or the newest release in the fetch script's cache.

**Where the assertions stop, and why.** The kernel boots with the *ISO's* command
line, which has no `console=` clause, so nothing Linux prints reaches ttyS0.
Everything after the hand-off is on the virtio-gpu scanout — and CloudHv ships no
`VirtioGpuDxe`, so the scanout is dark until Linux's own driver binds. To see the
installer:

```bash
ENTANGLED_UEFI_ISO_LINGER=120 \
ENTANGLED_UEFI_ISO_SHOT=$HOME/installer.png \
  cargo test -p boot-tests --test uefi_iso -- --ignored --nocapture
```

`LINGER` keeps the guest running that many seconds past the hand-off; `SHOT`
writes the scanout as PNG. A healthy run produces subiquity's language-selection
screen at 1280×800. Asserting on those pixels is screenshot comparison
(MVP-1405) and belongs in the graphical tier, not here.

Or drive it by hand, which is the same machine with a window:

```bash
cargo run -p entangled -- run examples/ubuntu-uefi.toml   # edit the disk paths first
```

## Booting any ISO generically (`--cdrom`), and the GNOME desktop

```bash
iso=$(bash scripts/fetch-ubuntu-iso.sh desktop)     # ~6 GiB, verified
cargo run -p entangled -- run --cdrom "$iso" examples/ubuntu-desktop-live.toml
```

`--cdrom <iso>` (or a `[cdrom] path = "..."` section) attaches the ISO
read-only as the *last* virtio-blk device and lets the firmware boot it —
UEFI-1803's machinery as one flag, no hand-written `[[disk]]` pair. It is
refused outside `mode = "uefi"` + `transport = "pci"` at config time.
The desktop profile is 4096 MiB (the high-RAM split: RAM above the 32-bit MMIO
hole continues at 4 GiB) and reaches the GNOME live session in a few minutes of
llvmpipe; `--screenshot-after N` writes the scanout as PNG after N seconds and
refreshes it every 20 s, which is how an unattended graphical boot is watched.

Two `#[ignore]`d tests pin this path:

- `cargo test -p entangled --test cdrom_boot -- --ignored --nocapture` — the
  CLI plumbing: a diskless UEFI profile + `--cdrom` reaches GRUB (~1 min, uses
  the newest cached ISO, either variant).
- `cargo test -p boot-tests --test desktop_gnome -- --ignored --nocapture` —
  the Desktop ISO to GNOME: types `console=ttyS0` into GRUB (the install
  command's trick) so the *guest kernel's* log is assertable — `smp: Brought
  up 1 node, 4 CPUs` in the UEFI run path, `virtio_gpu` bound, systemd reaching
  the graphical target. `ENTANGLED_DESKTOP_SHOT=<png>` +
  `ENTANGLED_DESKTOP_LINGER=<secs>` capture the desktop itself. GNOME needs the
  cursor plane (mutter's pointer lives on it) and EDID, both in `virtio-gpu`
  since MVP-811/812.

## Installing Ubuntu, and booting what was installed (UEFI-1804)

```bash
bash guest/firmware/build-cloudhv.sh          # once, ~2.5 min
bash scripts/fetch-ubuntu-iso.sh              # once, ~2.9 GiB, verified
cargo run -p entangled -- install ubuntu --disk ~/entangled-vms/ubuntu.raw \
    --size 20G --auto --headless              # unattended
cargo run -p entangled -- run --headless ~/entangled-vms/ubuntu.toml
```

Measured on the development host (16 threads, KVM in WSL2): **4m39s** for the
install, ~2.5 min from `entangled run` to `ubuntu login:` (most of it cloud-init
generating SSH host keys on first boot). The automated form of both halves is
`cargo test -p entangled --test ubuntu_install -- --ignored --nocapture`.

**Three files come out of an install, and all three matter:**

| File | What breaks without it |
|---|---|
| `<name>.toml` | nothing to run |
| `<name>.nvram` | the firmware boots to "no bootable option" with a perfectly good disk attached: the `Boot####` entry pointing at the installed bootloader lives here, not on the disk |
| `<name>-install.log` | the only account of what the installer did — subiquity's own log, captured off ttyS0 |

`<name>-seed.iso` is kept too; it is the cloud-init NoCloud volume (label
`CIDATA`) the install was driven by, and re-running the install regenerates it.

### Reading the install transcript

Four things to look for, in order. Each one failing points somewhere specific:

```
entangled: root=hd1 prefix=(hd1)/boot/grub          GRUB's command line answered
linux /casper/vmlinuz autoinstall console=ttyS0…    the typed command line
subiquity/load_autoinstall_config                   the seed was found and read
reboot: Power down                                  an orderly ACPI S5 finish
```

- **No `entangled: root=…`** — the keystrokes never reached GRUB. The host types
  on the *serial console*, which works only because CloudHv has no GOP and both
  the firmware and GRUB use the UART (ADR-0003 phase 4). Check whether the menu
  marker (`Try or Install Ubuntu Server`) appeared at all.
- **No `subiquity/load_autoinstall_config`** — the seed volume was not found. It
  must be ISO9660, labelled `CIDATA`, with `user-data` *and* `meta-data` at its
  root; cloud-init requires both files and matches only `CIDATA`/`cidata`.
- **`Continue with autoinstall?`** — `autoinstall` did not reach `/proc/cmdline`,
  so the installer is waiting for a human. This is the failure the whole typing
  mechanism exists to prevent; nothing in the autoinstall file can fix it.
- **No `reboot: Power down`** — the install did not finish. Everything before the
  last `start:` line in the transcript did.

The echo of the typed lines is interleaved with cursor-positioning escapes (GRUB
redraws per character), so grep for the *kernel's* view of it — `Command line:`,
or the `autoinstall console=ttyS0` substring — rather than for a clean line.

### Booting the installed system

```
BdsDxe: starting Boot0006 "Ubuntu" from HD(1,GPT,…)/\EFI\ubuntu\shimx64.efi
GNU GRUB  version 2.14
Welcome to Ubuntu 26.04 LTS!
[  OK  ] Started serial-getty@ttyS0.service - Serial Getty on ttyS0.
ubuntu login:
```

`Boot0006 "Ubuntu"` is the evidence that the NVRAM store worked: it is the entry
`grub-install` wrote through the emulated flash device during the install, read
back out of a file by a different VM. If instead you see
`Boot#### "UEFI Misc Device"`, the firmware fell back to enumerating removable
media — the boot may still work, and the variable store did not.

The installed system talks on ttyS0 because the autoinstall profile's
`late-commands` put `console=ttyS0,115200n8` in `/etc/default/grub` and ran
`update-grub`; there is no autoinstall key for the target's kernel command line.
GRUB's own menu is on the serial line for the same reason, which is how a failure
to load the kernel stays visible.

### Reading the host log, not just the guest's

The `entangled run` log is half the diagnostic, because the failures in this area
show up as devices that never come up rather than as errors:

```
attached virtio-pci device slot=3 device=Input address=00:04.0 bar=0xc000c000 irq=9
virtio-input ready device="Entangled Keyboard" profile=Keyboard
virtio device activated transport="virtio-pci" slot=3 device=Input queues=2
```

Every attached device must reach `virtio device activated`. Two lines mean a gap:

- `driver gave up on this device (FAILED)` — the guest's probe failed. If it is
  *some* devices and not all, suspect the interrupt line: compare `irq=` against
  what legacy devices own (`layout::VIRTIO_IRQS` exists because pin 8 is the
  RTC's). The trick that found that one is worth reusing — **make the pin the
  variable**: add a disk to shift every later device up one slot and see whether
  the failure follows the pin or the device.
- `queue notify before DRIVER_OK, ignoring` naming a device that is not the one
  you were watching — a kick reached the wrong device's ioeventfd, i.e. a BAR
  moved and the registration did not follow it (`DeviceNotifier::rebase`).

## Fuzzing (MVP-1402)

`cargo-fuzz` targets live under `fuzz/`, which is its own workspace and is listed
in the root manifest's `exclude`: libfuzzer needs nightly and `-Zsanitizer`, so
`cargo test --workspace` must never try to build it.

```bash
rustup toolchain install nightly
cargo install cargo-fuzz

# Build every target.
cargo +nightly fuzz build --target-dir "$HOME/entangled-fuzz-target"

# Run one, time-boxed (the whole suite: chain_walk, mmio_transport,
# debian_sums, blk_request, blk_discard, gpu_3d_commands,
# gpu_remote_protocol).
cargo +nightly fuzz run chain_walk --target-dir "$HOME/entangled-fuzz-target"     -- -max_total_time=240 -rss_limit_mb=4096

# Reproduce and minimise a finding.
cargo +nightly fuzz run  blk_request fuzz/artifacts/blk_request/crash-<hash>
cargo +nightly fuzz tmin blk_request fuzz/artifacts/blk_request/crash-<hash>
```

`--target-dir` outside the repository matters on the Windows host: D: is nearly
full and the fuzz build is large.

| Target | Covers |
|---|---|
| `chain_walk` | `virtio_core::chain::walk` / `split_rw` over a guest-programmed ring in a small `GuestMemoryMmap` |
| `mmio_transport` | arbitrary register read/write storms of any width against a mock device, with status/interrupt invariants checked after every operation |
| `debian_sums` | `parse_sums`, `Release::parse` and the ISO-name/version helpers |
| `blk_request` | virtio-blk header parsing, `validate_range`, `sector_offset`, `total_len` |
| `blk_discard` | the DISCARD / WRITE_ZEROES segment array: `segment_count` on the array's shape, `DiscardSegment::parse`/`validate` on each range, for both commands. Asserts what the host then relies on — an accepted range is inside the disk, its byte offset *and* end are representable, `unmap` only for write-zeroes, only the one defined flag bit ever accepted |
| `gpu_3d_commands` | `virtio_gpu::renderer::validate_stream` on raw bytes, plus arbitrary 3D command sequences (contexts, creates, backing, transfers, submits, readback) through `Gpu3d` + `NullRenderer` with real guest memory |
| `gpu_remote_protocol` | the isolated-renderer wire format, both directions, with an exact re-encode check |
| `snapshot_parse` | the whole snapshot parser (ADR-0006): the container's header and index, every section decoder against raw bytes, **and** arbitrary bytes spliced into a well-formed container so the decoders are reached *through* the digest checks rather than around them |

Rules that keep the targets useful:

- Seed corpora under `fuzz/corpus/<target>/` come from the malicious-guest unit
  tests (looped chains, `next` past the ring, indirect descriptors, oversized
  lengths, truncated digests, the register bring-up sequence) and are committed;
  libFuzzer's own additions are gitignored.
- Every crash that gets fixed leaves a named regression seed in the corpus
  **and** a unit test in the crate that owns the code — the fuzz target is not
  the regression test.
- Assert invariants, not just absence of panics: an accepted value must satisfy
  what the caller downstream relies on (the payload cap, the sector range, the
  status-bit set). Every finding so far came from such an assertion, not from a
  crash. The strongest one is **exact re-encode**: anything a decoder accepts
  must encode back to the bytes it came from. `snapshot_parse` found a real bug
  with it in under five minutes — an absent `Option` could carry a non-zero
  payload, so `(false, 7)` and `(false, 0)` both decoded to `None` and the
  format had two spellings for one state.
- Seeding a parser's corpus from a **real artifact** beats writing one by hand.
  `fuzz/corpus/snapshot_parse/` is every section of an actual VM's snapshot,
  extracted by reading the container's index; the memory section is kept as a
  64 KiB prefix so the corpus stays small.
- Not part of default CI: a scheduled, time-boxed job.

## Suspend and restore (ADR-0006)

The heartbeat probe pays for itself twice. For a pause its *absence* is the
measurement; for a suspend its **continuation** is:

```text
run:     VMHOST_HEARTBEAT 8      <- suspend here
resume:  VMHOST_HEARTBEAT 9      <- and no second VMHOST_GUEST_READY
```

A counter that carries on with the next number cannot be faked by a machine
that merely booted, and a restored vCPU whose registers, MSRs or local APIC came
back approximately right does not carry on counting — it faults, or it goes
quiet. `suspend_restore.rs` asserts the exact successor, the whole sequence
being consecutive, and zero ready markers in the resumed process.

**Test the refusals from real bytes.** One suspend, then the file corrupted six
ways in memory and offered back to `entangled resume`: wrong magic, truncated,
wrong format version, an unknown header flag, the other hypervisor's host code,
and a flipped bit in the middle of the memory section. Each must fail with a
message that says which. Plus the one that protects a filesystem: the same VM
resumed onto its *unchanged* disk (which must work — the check is about change,
not about having a disk) and then onto one that grew.

Three things that cost time to learn:

- **The GPU can keep working while the guest is dead to the world.** The first
  restored guest drew frames and never printed a line: on KVM the IOAPIC is in
  the kernel, the restore had not carried it, every pin came back masked — and
  MSI-X bypasses the IOAPIC entirely. If a resumed guest looks half-alive, ask
  which interrupt path still works.
- **Count heartbeats, not just their presence.** "A heartbeat appeared" is
  satisfied by a guest that rebooted and started at 0.
- **The two hosts do not lose the same state.** A device inventory is not enough;
  check what the *hypervisor* owns on each host as well.

## Invariants every test run enforces

- No vCPU threads or TAP devices left behind after a VM stops (assert in
  test teardown; acceptance: "closing the VM leaves no vCPU processes or
  graphics contexts").
- Interrupting the VMM must not corrupt a completed disk image (crash-safety
  test writes, kills the process, fsck's the image).
- A failing guest never takes the host process down: any panic in a device
  thread is a test failure by definition.

## Lifecycle: pause, resume, reboot (ADR-0005)

Four properties, on both hosts, all asserted through the **guest** rather than
through host bookkeeping:

| Test | Host | What it proves |
|---|---|---|
| `tests/boot/tests/lifecycle.rs` | KVM | pause / resume / host reset / guest reboot |
| `crates/vmm-core/tests/whp_lifecycle.rs` | WHP | the same four, natively |
| `apps/entangled/tests/guest_reboot.rs` | either | an installed Ubuntu reboots itself through its firmware, twice (`--ignored`) |
| `apps/entangled/tests/suspend_restore.rs` | either | suspend/resume, and every refusal (ADR-0006) |
| `apps/entangled/tests/guest_suspend.rs` | either | an installed Ubuntu is the same session after a suspend (`--ignored`) |

**Measuring a pause needs the guest to be noisy.** The test guest gained
`entangled.heartbeat=<ms>`: it prints `VMHOST_HEARTBEAT <n>` for ever and never
returns. A stalled *console* could mean a stalled device; a stalled heartbeat
means stalled guest code, which is the thing being asserted. It never returns so
that the absence of a line is unambiguous, and the host decides when the VM ends.

**The harness gained a `Driver`.** `boot_once_driven(&spec, Some(driver))` runs a
callback on its own thread while the vCPUs execute — every lifecycle call blocks
until they acknowledge, so it cannot live in the poll predicate. Two things
change when a driver is attached, both deliberate:

- the run is **not** ended by the ready marker (the driver is about to do
  something that happens *after* the guest is ready), and
- the VM gets a lifecycle seam, which turns a guest reset into a reboot instead
  of the end of the run. Every other test in the harness depends on the test
  guest's `reboot=k` ending the run, which is why the seam is opt-in per boot.

The harness also runs a **supervisor** of its own, the same shape `entangled run`
has: a guest reset is latched by whichever vCPU saw it and served by somebody
else, and without that somebody a guest that reboots itself simply waits for
ever. That was the first failure when the test was written.

**The end-to-end one drives the CLI.** `guest_reboot.rs` spawns
`entangled run --headless --control-stdin`, logs in over the serial console with
`type <text>`, and runs `sudo reboot`. It counts EDK2 boot-manager runs
(`BdsDxe: starting Boot`) to prove the *firmware* ran again off a variable store
the reset did not clear — a reset that wiped NVRAM would boot to the EFI shell,
which no login-prompt count would catch. It self-skips without a hypervisor, the
CloudHv firmware or an installed profile (`$ENTANGLED_REBOOT_PROFILE`, else
`~/entangled-vms/{e2e-ubuntu,ubuntu,desktop}.toml`).

Two lessons it cost to learn, both worth keeping:

- **`sudo`'s password prompt is not a stable string.** Ubuntu 26.04 asks
  `[sudo: authenticate] Password:` where older releases said
  `[sudo] password for x:`. Match the one word both contain, and count prompts
  per boot rather than matching once — the console buffer still holds the
  previous boot's.
- **A test that takes twelve minutes must say where it got to.** Print a line per
  step and save the whole console to a file named in the failure message; a tail
  is never enough for a boot log.
