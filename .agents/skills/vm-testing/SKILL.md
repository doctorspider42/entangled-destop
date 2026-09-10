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
7. **Fresh-install acceptance** — the only tier that does not build anything.
   It installs a *published release* and runs it with an empty per-user
   profile. Everything in tiers 1-6 runs inside a checkout with artifacts, a
   warm cache and a developer's environment variables; this tier is the one
   that behaves like a stranger. See the section below — three separate
   shipped-product bugs have been invisible to the other six tiers.

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

## The fresh-install acceptance: testing what we ship, not what we build

Every tier above runs in a checkout. `artifacts/` is populated, the cargo cache
is warm, WSL is configured, `GITHUB_TOKEN` is in the environment,
`~/entangled-vms` exists. **A stranger has none of that**, and for a long time
nothing here behaved like one. The cost, in one evening, was two blockers the
1500-test suite could not see:

* `no artifacts/firmware/CLOUDHV.fd under F:\Program Files\Entangled Desktop`
  — every UEFI guest impossible, because the firmware was a gitignored
  artifact that only ever existed in a source checkout;
* `execvpe entangled failed 2` — the WSL backend running a Linux binary the
  Windows installer never shipped.

And a third, found by this harness on the day it was written: `entangled fetch
firmware` failed for **everyone**, because `guest/firmware/pinned.toml` held
the digest of a locally built firmware while the workflow had published a
different one. Nothing caught it, because the only host that fetches is a host
without a copy — and every host in the loop had a copy.

The pattern behind all three: *a check that only runs where the artifact is
already present cannot see the artifact missing.*

### Running it

Two scripts, one per host. Neither needs a hypervisor for its default stages.

```powershell
# Windows, elevated (the installer is PrivilegesRequired=admin).
pwsh -File scripts\fresh-install-acceptance.ps1                 # latest release
pwsh -File scripts\fresh-install-acceptance.ps1 -Tag v0.2.44    # a specific one
```

```bash
# Linux — the published entangled-linux-x86_64, which is also the binary
# `entangled wsl install-engine` puts inside a WSL distribution.
bash scripts/fresh-install-acceptance.sh --tag v0.2.44
```

The Windows script's stages, in order: `download` (the setup .exe off GitHub
Releases, unauthenticated), `install` (`/VERYSILENT /DIR=<scratch>`), `tree`
(what actually landed), `doctor`, `fetch`, `engine`, and the opt-in `guest`:

```powershell
pwsh -File scripts\fresh-install-acceptance.ps1 -Tag v0.2.44 `
    -Stages download,install,tree,doctor,fetch,engine,guest `
    -Iso "$env:LOCALAPPDATA\entangled\ubuntu\26.04\ubuntu-26.04-live-server-amd64.iso"
```

`guest` takes a machine from nothing to a login prompt on the installed
program: ~15 min for the unattended Ubuntu install plus ~2 min to boot it. It
needs a hypervisor and an ISO. There is no `bash scripts/fetch-ubuntu-iso.sh`
on Windows, so `-Iso` is mandatory — which is itself the newcomer's path, and
the reason the parameter exists rather than a download.

### What "stranger" means, mechanically

Copy this list when writing anything that claims to test a shipped artifact:

* the artifact comes off GitHub Releases over plain HTTPS — no `gh`, no token,
  no checkout;
* `USERPROFILE`, `APPDATA`, `LOCALAPPDATA`, `TEMP` (or `HOME`, `XDG_*`) point
  at a throwaway tree, so there is no manager settings file, no verified media
  cache and no `~/entangled-vms`;
* every `ENTANGLED_*`, `CARGO*`, `RUST*` and `*_TOKEN` variable is stripped —
  `ENTANGLED_FIRMWARE_DIR` alone would hand the program the very artifact the
  test is about;
* the directory holding `gh.exe` comes off `PATH`. Scrubbing `APPDATA` hides
  gh's config file, but modern gh keeps tokens in the OS credential store,
  which no environment variable hides;
* the working directory has **no `Cargo.toml` and no `artifacts/` at or above
  it** — asserted, not assumed. This is the one that silently rescues a broken
  installation: `artifacts/firmware/CLOUDHV.fd` relative to the working
  directory is row 5 of the firmware lookup, and a test run from a checkout
  passes whatever the installer did.

### Do not disturb the real installation

`installer/entangled.iss` keeps one `AppId` for the life of the product — it is
the upgrade identity and must never change. So a second install into a scratch
`{app}` **rewrites that AppId's uninstall registration and the shared Start
Menu shortcuts**, and uninstalling it afterwards deletes them: the developer's
own installation silently loses its Add/Remove entry and its Start Menu group.
The script snapshots both before installing (`reg export`, a copy of the group
directory) and restores them in cleanup, then asserts the restored registration
points back at the real install. On a CI runner all of it is a no-op.

The user's VM directory, media cache, manager settings and WSL engine are never
written at all — the scrubbed profile means the installed program cannot see
them — and `/MERGETASKS=!desktopicon,!wslengine` keeps the installer's optional
tasks out of the way, so nothing touches WSL either.

### What CI covers, and what it cannot

`.github/workflows/fresh-install.yml` runs both scripts after every `Release`
run completes, plus daily and on demand. It covers: the release resolving and
downloading without credentials, the setup installing unattended, the shipped
tree (**the firmware regression, directly**), `doctor`'s inventory, `entangled
fetch firmware` with an empty cache, and the `wsl install-engine` surface
existing in the shipped binary.

It cannot cover:

* **a hypervisor.** GitHub's `windows-latest` exposes no WHP and no nested
  virtualization, and `ubuntu-latest` has no `/dev/kvm` for this job. Nothing
  boots there. This is why `entangled doctor` prints the install inventory
  *before* it fails on the hypervisor (`apps/entangled/src/doctor.rs`
  `report()`): on a runner that cannot run a VM, the inventory is the whole
  point of the command, and until that change CI could assert nothing at all
  about a fresh installation. Booting is the developer machine's job:
  `-Stages guest`.
* **WSL.** No distribution on the runner, so "is there a Linux engine in WSL"
  can only be answered as "no, and here is the fix". Whether the install
  *works* needs a real distribution — run the Windows script on a machine that
  has one, or `entangled wsl install-engine` by hand.
* **an upgrade over an existing installation.** The runner has nothing to
  upgrade from, and doing it on a developer machine would mean overwriting
  their real install. Untested; a TODO.

### Reading a failure

Each check prints `[PASS]`/`[FAIL]` with the evidence under it, and the
Windows script writes `-JsonReport` for the workflow summary. The stage logs
(`<root>\logs\`) survive cleanup on purpose — `setup.log` is Inno's own, and
`guest-install.log` / `guest-boot.log` are the serial transcripts, to be read
with the same ANSI-and-not-UTF-8 care as any other transcript here.

A check named `REGRESSION:` is one of the two evening bugs. If one of those
goes red, the shipped product is broken for every new user, not flaky.

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
  `entangled.netprobe=dhcp,<gateway>,<host>:<port>` is the same probe for a
  kernel booted with `ip=dhcp`: it skips the ioctls, reads back the address
  the *lease* gave it and reports that. Use it to exercise the NAT's DHCP
  server with a real client — the portable acceptance is
  `apps/entangled/tests/usernet_guest.rs`, which runs `entangled run` on
  whichever hypervisor the host has and checks the console for both
  `IP-Config: Got DHCP answer` and the probe's OK line. **It needs the
  bootstrap kernel**, for the same class of reason as the gamepad probe:
  virtio-net is a *module* in the Debian-installer kernel, so on the fallback
  there is no `eth0` at all when init runs and the failure reads exactly like
  a broken NAT.
- **usernet's teardown has its own tests now, and they are the ones to extend.**
  Two real bugs came out of that path, both found by a stalled install rather
  than by a test: a flow leak (`is_open()` is true in `CloseWait`, so the
  retirement condition never fired) and a suspected race where a host peer
  closing right after its last write beat those bytes to the guest. The shape
  that settles both lives in `virtio_net::usernet::tcp`'s test module — a host
  peer that writes and closes in the same breath, run thirty times per test run
  and 250 times in the `--ignored` campaign
  (`the_teardown_race_holds_over_a_long_campaign`), asserting the guest has
  every reply byte *before* the FIN and that the flow table returns to zero.
  Three things to copy if you test this area:
  - assert `flow_count()` **after** a workload, never only during it. The leak
    was invisible for exactly as long as nothing did;
  - drive the guest side through a real `smoltcp` handshake, and respect the
    window the NAT advertises — a dumb sender that overruns the receive buffer
    has its segments dropped, never retransmits, and stalls in a way that looks
    like a NAT bug and is not;
  - `measure_the_datapath` (`--ignored`, `--nocapture`) is the before/after
    number for any change to that module.
- `entangled.padprobe=<n>` reports what the guest kernel made of the
  virtio-input gamepad — name, `input_id`, whether `joydev` bound it and its
  `js*` node opens, how many `KEY`/`ABS` codes the input core registered, and
  the `EVIOCGABS` ranges — then echoes **at most** `n` events, stopping after
  1.5 s of silence. That ceiling-plus-silence shape is the point: a host that
  asks for more than it injects gets "and nothing after that" answered, which
  is how `tests/boot/tests/gamepad.rs` proves an unplugged controller stops
  producing events instead of only proving a plugged-in one starts.
  **It needs the bootstrap kernel**: `CONFIG_INPUT_JOYDEV` is a separate symbol
  from `CONFIG_INPUT_EVDEV` and is a module in the Debian-installer kernel, so
  on the fallback the pad has no `js*` for a reason that is nothing to do with
  the device. The test self-skips there — and for the harder case, a
  `artifacts/bootstrap/vmlinuz` built *before* that option was added, the probe
  reports `joydev=` (is the handler registered at all, from
  `/proc/bus/input/handlers`) and the test skips only the `js*` assertions,
  saying so. A guest probe that can distinguish "the device was refused" from
  "the kernel cannot answer" is worth the four extra lines every time.
  **Check which kernel you have before trusting a green run.** That escape
  hatch is doing real work: a `vmlinuz` from before 2026-09-09 has no joydev,
  so every `js*` assertion in `tests/boot/tests/gamepad.rs` passes without
  checking anything and prints `NOT CHECKED` while doing it. `joydev=1` in the
  `padinfo` line is the only proof the js half ran. Rebuild with
  `guest/bootstrap-kernel/build.sh` (it asserts the symbol) if it says 0.
- The same probe prints two more lines. `inputmap` is the whole machine's
  input topology — one `name:uniq:eventnode:jsnode` record per device — and it
  is the only place questions *about the devices next to a device* can be
  settled: which of them `joydev` claimed, and therefore who gets `js0`; and
  whether a two-player VM really has two distinct joysticks (different
  `U: Uniq=`, different nodes). `padinfo` additionally carries `evbits=` (the
  `B: EV=` bitmap verbatim), `ff=` and `ffwrite=`, which together with the
  host-side `InputHandle::ev_bits_probed` and `EventStats::status_ff` are how
  the rumble dead end is asserted as a *negative result* rather than left as a
  TODO. Both patterns generalise: when the claim is about the guest kernel's
  own classification, quote the kernel's bytes rather than paraphrasing them,
  and when the claim is "the guest never asked", record what it *did* ask.
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

## Endurance: the soak (MVP-1404)

`tests/boot/tests/soak.rs` boots **one** VM and leaves it running. Where
`repeat_boot` looks for what a teardown forgets to release, this looks for what
a *running* VM accumulates, which is a different defect class: a leaked irqfd
shows up in the first, a serial IRQ that stops being delivered after the
seventy-thousandth line only in the second. The guest is the test initramfs with
`entangled.heartbeat=<ms>`, whose one line — `VMHOST_HEARTBEAT <n>
uptime_ms=<t> tsc=<n> pm_us=<n>` — carries most of the measurement. The three
counters are the guest's `CLOCK_MONOTONIC`, the raw TSC under it and the host's
own monotonic clock as the guest reads it from the emulated ACPI PM timer; read
the clock finding below before drawing any conclusion from a drift number.

```bash
ENTANGLED_SOAK_LOG=$HOME/soak.tsv \
  cargo test -p boot-tests --test soak -- --ignored --nocapture
```

Default two hours (`ENTANGLED_SOAK_SECS`), 256 MiB, mmio, one vCPU, sampled
every 60 s after a 60 s warm-up. The other knobs are in the file's header table;
two are worth knowing here.

**Always pass `ENTANGLED_SOAK_LOG`.** Every sample is written and flushed as it
is taken and the summary block is appended at the end, so the file is a complete
account of the run up to the moment anything interrupts it. This is not
hypothetical: the first attempt at this run died with the machine at ~50 minutes
on 2026-09-08 and left nothing at all behind, because the numbers only existed
in a `println!` that never happened. Everything the test asserts except the two
whole-transcript checks (gaps, unexpected lines) can be re-derived from the file.

**`ENTANGLED_SOAK_CMDLINE=<words>`** appends to the guest's kernel command line
and **`ENTANGLED_SOAK_POLL_MS=<n>`** changes how often the harness re-reads the
console. Both exist for the control runs below — an experiment that needs the
test edited to repeat is an experiment nobody repeats.

### The measured run: 2 h on KVM in WSL2, 2026-09-09

```text
duration          7200 s (2.00 h) after a 60s warm-up, 121 samples
RSS               85352 -> 85668 KiB (+316 KiB, +158 KiB/h)
file descriptors  11 -> 11
threads           6 -> 6
heartbeats        7066 ticks, 7127 lines on the console, 0 gaps
delivery          worst interval 0.90 of expected (60 expected per 60 s), at 4800 s
guest clock       7109386 ms guest vs 7200131 ms host, drift -12603 ppm (+-104 ppm)
console           309994 bytes total, 43.5 B per heartbeat, 1 unexpected lines
```

(Verbatim from the run, whose summary block has since grown three fields.)

Read that as four verdicts and one apparent failure:

- **Nothing leaks.** Descriptors and threads are *identical* after two hours and
  7 066 heartbeats — no per-interrupt eventfd, no per-kick worker. The 316 KiB
  of RSS is not monotonic (it fell as well as rose, 85 128 KiB at 45 minutes) and
  is dwarfed by the 285 KiB of console text the harness itself accumulated over
  the same period, which is the same memory. Treat +316 KiB / 2 h as noise, and
  the 32 MiB allowance as the thing that would catch a real leak.
- **No interrupt is ever lost.** 7 066 consecutive tick numbers with **zero
  gaps** — every line the guest wrote to the interrupt-driven 8250 arrived. That
  is the assertion this test exists for, and it is the one `repeat_boot` found
  broken before the machine had an IOAPIC.
- **No stalls.** The worst 60-second window carried 0.90 of the heartbeats its
  length implies.
- **The guest says almost nothing.** One benign line in two hours.
- **The guest's clock appeared to lose 90.7 s, and the guest was innocent.**
  The next section is that story, because it is the most expensive lesson this
  test has taught and the shape of it will recur.

### The confirming run: 2 h on the same host, 2026-09-09, green

```text
duration          7200 s (2.00 h) after a 60s warm-up, 121 samples
RSS               88260 -> 89452 KiB (+1192 KiB, +596 KiB/h)
file descriptors  11 -> 11
threads           6 -> 6
heartbeats        7146 ticks, 7207 lines on the console, 0 gaps
delivery          worst interval 0.95 of expected (60 expected per 60 s), at 1140 s
guest clock       7168687 ms guest vs 7200121 ms host, drift -4366 ppm (+-104 ppm), clocksource tsc
cross-check       guest vs the host clock it read itself -4393 ppm, that reading vs
                  the host's own +27 ppm, guest TSC 1888.110 MHz against host time
host reference    monotonic vs wall clock +4484 ppm, guest vs the host's wall clock
                  +99 ppm - reference CLOCK_REALTIME (the host's monotonic clock is
                  the outlier)
console           565905 bytes total, 75.8 B per heartbeat, 1 unexpected line
```

Three numbers to read there, in order of how much they settle:

- **+99 ppm** — the guest against real time over two hours. That is the verdict.
- **+27 ppm** — the guest's own reading of the host clock against the host's
  reading of it, i.e. everything the harness could have got wrong about
  *when* a line arrived, over 7 146 heartbeats. It could not.
- **+4 484 ppm** — the host's monotonic clock against its own wall clock, which
  is the whole of the −4 366 ppm the old assertion would have failed on.

And the per-sample series is the wander caught in the act: the host clock's
error decays smoothly through the run, and the guest's apparent drift follows it
exactly, while the guest's agreement with the wall clock never moves.

| At | apparent drift | host clock error |
|---|---|---|
| 780 s | −9 942 ppm | ~+10 000 ppm |
| 2 940 s | −7 334 ppm | — |
| 4 740 s | −6 038 ppm | — |
| 7 200 s | −4 366 ppm | +4 484 ppm |

Load average over that run went from 0 to 12 and back; the drift did not notice.

### The clock finding: the reference was the broken clock

Three things had to be fixed before the drift number meant anything. The first
two are about measuring honestly; the third is about what you are measuring
*against*, and it is the one that cost a day.

1. **Date the beat you watched arrive, never the one you found.** The baseline
   used to be whichever heartbeat happened to be sitting in the transcript,
   stamped "now" although it had been printed up to one harness poll earlier.
   That fixed ~0.6 s offset is then divided by the run length, so the same
   healthy guest read **+77 356 ppm at 10 s and +10 031 ppm at 60 s**.
   `observe_next_beat` waits for the *transition*, which puts the same small
   latency on both ends of the interval where it cancels.
2. **Print the error bar.** One harness poll plus one observe interval (750 ms)
   is the measurement's whole timing error: ±104 ppm over two hours, ±17 000 ppm
   over 45 seconds. The assertion is widened by exactly that.
3. **Check your reference before you use it.** `Instant` is `CLOCK_MONOTONIC`,
   a free-running count of the host's own making, and it can be wrong about real
   time with nothing to say so.

**On this machine it is wrong, by a lot.** Measured 2026-09-09, three ways that
agree:

| Measurement | Result |
|---|---|
| WSL2 `CLOCK_MONOTONIC` over a 240 s sleep, against Windows QPC | **+33 315 ppm** |
| WSL2 `CLOCK_MONOTONIC` vs WSL2 `CLOCK_REALTIME` (hv-timesync disciplined), 120 s | **+37 889 ppm** |
| hardware TSC over 232 s of Windows QPC time | **1 896 585 kHz** (nominal 1 896 389, +103 ppm) |

So the TSC really does run at the 1 896.389 MHz Hyper-V advertises and
`KVM_GET_TSC_KHZ` hands the guest (verified: a bare `KVM_CREATE_VCPU` +
`KVM_GET_TSC_KHZ` in WSL returns `1896389`), and **WSL2's own
`CLOCK_MONOTONIC` advances as though it were 1 835.4 MHz** — it gains 3.3 %,
48 minutes a day. Our guest was keeping real time; the host's clock was running
fast, and the soak was reporting the difference as the guest's fault.

Every control run makes sense once you see that:

| Control (900 s each, launched together) | Drift |
|---|---|
| default `tsc` clocksource | −32 032 ppm |
| `ENTANGLED_SOAK_CMDLINE=clocksource=kvm-clock` | −32 148 ppm |
| `ENTANGLED_SOAK_CMDLINE=clocksource=acpi_pm` | **−15 ppm** |

- **kvm-clock is not a second opinion.** KVM derives the pvclock scale from the
  same `virtual_tsc_khz`, so it inherits the identical error. The earlier
  reading of "kvm-clock agrees, therefore not a frequency problem" was exactly
  backwards — agreement to 0.4 % is what a shared frequency looks like.
- **`acpi_pm` agrees with the host because it *is* the host.** We synthesise
  the PM timer from host `Instant`, so a guest using it is comparing the host's
  clock with itself. That is what makes it the decisive control, and also why
  it cannot be the reference: a wrong host clock is invisible to it.
- **Not the harness.** The guest now reports the PM timer in its own heartbeat
  (`pm_us=`), so the harness's observation latency can be measured rather than
  argued about: guest `CLOCK_MONOTONIC` against the host clock **the guest read
  itself** is −31 956 ppm, within 400 ppm of the number the host measured from
  outside. Nothing is lost in the observing.
- **Not load — time.** The earlier "it scales with host load, −5 000 ppm quiet
  to −25 000 ppm at load 20" does not survive re-measurement: −32 000 ppm at a
  load average under 3. What actually varies is the host clock's *own* error,
  and it **wanders**. Same machine, same WSL boot, `CLOCK_MONOTONIC` against
  `CLOCK_REALTIME`:

  | Time | Host monotonic error | Guest drift measured against it |
  |---|---|---|
  | 15:24 | +37 889 ppm | −32 032 ppm |
  | 16:57 | +8 064 ppm | −9 621 ppm |

  The TSC against the wall clock was 1 896.6 MHz in both (nominal 1 896.389,
  +119 ppm), and the guest tracked it in both. A reference whose own rate moves
  by 30 000 ppm in ninety minutes is what the old "scales with load" table was
  really watching — the busy period simply happened to be later in the run.
  This is also why the check has to be **per run** rather than a constant
  written into the test.

**And WHP says the same thing from the other side.**
`crates/vmm-core/tests/whp_clock.rs` boots the *same* guest on the *same*
hardware under the other hypervisor, on a host whose clocks are sound:

```text
interval          61005 ms host, 61076 ms guest
drift             +1160 ppm (+-3278 ppm observation error)
cross-check       guest vs the host clock it read itself +10 ppm
host reference    monotonic vs wall clock +170 ppm
guest TSC         1898.571 MHz measured against host time
```

**+10 ppm** between the guest's `CLOCK_MONOTONIC` and our emulated PM timer.
Same machine model, same TSC, same initramfs — the only thing that changed is
whether the host could tell the time. `cargo test -p vmm-core --test whp_clock`
runs it; 60 s by default, `ENTANGLED_WHP_CLOCK_SECS` for longer.

**Verdict: not our bug, and not fixable from our side.** `KVM_SET_TSC_KHZ`
cannot correct it even though `KVM_CAP_TSC_CONTROL` is available: KVM computes
the hardware scaling ratio as `requested / tsc_khz` and the guest divides by
`requested`, so both terms carry the host's own belief about `tsc_khz` and it
cancels out. The error is in that belief, one level below anything a VMM can
reach.

**What the soak does about it:** `HOST_CLOCK_SANITY_PPM` (1 000 ppm). Each beat
now records `SystemTime` beside `Instant`, and if the host's monotonic clock
disagrees with its own wall clock by more than that, the host has disqualified
itself as the reference: the guest is judged against `CLOCK_REALTIME` instead
and the report says which reference it used and why. **The 10 000 ppm gate was
never widened** — a correct guest beats it by orders of magnitude, and a guest
given a wrong TSC frequency blows past it, which is still exactly what this
assertion is for.

Two rules to carry out of this:

- **Any timing measurement taken inside WSL on this machine is 3.3 % fast.**
  It is not confined to VM tests — benchmarks, timeouts and throughput numbers
  are all affected. See the `dev-environment` skill.
- **A measurement is worth no more than its reference.** When a number says the
  thing under test is broken, measure the instrument before believing it. Two
  clocks that disagree are one bug; three clocks name which.

### What it deliberately does not assert

Nothing about absolute performance, and no ratio tighter than 0.5 of the
expected heartbeats in a window: this machine runs other agents' builds and VMs,
and a guest descheduled for a second is not a stalled guest. A lost interrupt
does not produce 0.6 of the heartbeats — it produces none, and then a gap in the
tick numbers, which is the assertion that actually catches it.

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

Manual firmware bring-up (needs a firmware: `entangled fetch firmware`, or
`bash guest/firmware/build-cloudhv.sh` once — ~2.5 min and ~2 GiB of EDK2
checkout in `~/.cache/entangled-edk2` — which is what you want when the
firmware itself is the thing under test):

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
entangled fetch firmware                      # 4 MiB, or build-cloudhv.sh to build it
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
entangled fetch firmware                      # 4 MiB, or build-cloudhv.sh to build it
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

## Installing Debian, on either host (`debian_install`)

`apps/entangled/tests/debian_install.rs`, same shape as `ubuntu_install` and
`fedora_install`, `#[ignore]`d, self-skipping:

```bash
entangled fetch bootstrap-kernel     # or build.sh + build-bootstrap-initramfs.sh
cargo test -p entangled --test debian_install -- --ignored --nocapture
```

It skips on a host with no hypervisor, and on a host with no **bootstrap kernel
+ initramfs** — which it looks for in the three places `entangled` does, in the
same order: `ENTANGLED_BOOTSTRAP_DIR`, `artifacts/bootstrap/` in the checkout,
then any tag directory under `<cache>/bootstrap/`. That last one is what
`entangled fetch bootstrap-kernel` fills, and it is why this test can run on
Windows at all: the kernel is a Linux kernel build with no cross-compile.

What it asserts that the other two do not:

- the install ends in an **ACPI power-off**, not d-i's default reboot. `Power
  down` in the transcript is the assertion. A `reboot=k` triple fault is
  reported as a stop by KVM and *absorbed* by WHP's local APIC, so on Windows
  the installer VM would hang forever after a perfectly good install;
- the generated profile's `kernel` and `initramfs` **exist as the profile spells
  them**. A checkout that built its own keeps the historical relative
  `artifacts/bootstrap/vmlinuz`; a host that downloaded the pair gets absolute
  paths, and a relative one there names nothing. The test resolves relative
  paths against the repo root and absolute ones as-is, which is exactly what
  `entangled run` does;
- `root=UUID=` rather than `root=/dev/vda1`.

### Measured on this machine (Windows/WHP, 2026-09-09)

| What | Number |
|---|---|
| `entangled install debian --auto --headless --network usernet`, d-i trixie, 1536 MiB | **6 min 34 s** by hand, **7 min 36 s** under the test |
| installed system to `<vm name> login:` on ttyS0 | **7.3 s** |
| Weston desktop drawn at 1920x1080 (screenshot at 45 s) | 76 distinct colours sampled, panel and clock legible |
| the whole `debian_install` test, install + boot | 7 min 45 s |

Two things about that boot worth knowing before you write an assertion on it:

- **there is no bootloader and no firmware.** The profile boots the bootstrap
  kernel directly, its initramfs prints `entangled-bootstrap: switching root to
  /dev/vda1` and hands over to systemd. So none of the Ubuntu test's markers
  apply here — no `BdsDxe`, no `shimx64.efi`, no GRUB banner — and 7 seconds
  to a login prompt is normal rather than suspicious;
- **the login prompt carries the VM name**, because `install debian` preseeds it
  through `netcfg/get_hostname`: `e2e-debian login:`, not `debian login:`. Same
  trap the Fedora test documents.

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
# gpu_remote_protocol, gpu_blob, snd_control, snd_device).
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
| `gpu_blob` | the blob-resource surface (VEN-2001/2007): the three wire parsers on raw bytes including the `nr_entries` walk, then arbitrary create/map/unmap/unref against `BlobTable` with the renderer's declared support itself fuzzed. Asserts the invariants, not just absence of panic — the byte budget equals the sum of live blobs, the window holds exactly the mappings the harness believes in, every mapping is inside the window, and **no two mappings overlap**, re-derived from outside after every operation |
| `input_device` | the whole virtio-input device over a real `MmioTransport`: arbitrary chains on both queues, the config-space `(select, subsel)` state machine at any width and offset (a `size` that outran its payload would leak out of the device struct), guest writes to read-only config bytes, and host pushes interleaved with all of it. The status queue is the point — it is the only thing a guest can *write* to this device, and the `EV_FF` request rumble would have used lands there. Asserts no panic, no `DEVICE_NEEDS_RESET`, that the status queue is never written back, and that no used entry claims more than one `virtio_input_event` or more than a chain offered |
| `snd_control` | the virtio-snd parsers and bounds on raw bytes: `QueryInfo`/`ItemHdr`/`RawSetParams` round trips, `stream::validate_params`, `validate_xfer` and the lifecycle. Asserts that an *accepted* SET_PARAMS is inside every advertised set and every named bound, and that a refusal is `BAD_MSG` or `NOT_SUPP` and never `OK`/`IO_ERR` |
| `snd_device` | a brought-up `SoundDevice` with a live pump thread behind a real `MmioTransport`, fed descriptor chains of arbitrary shape (any lengths, any addresses, readable/writable in any order, indirect flags) on all four queues, interleaved with resets. Asserts no panic, no `DEVICE_NEEDS_RESET` from guest input, and no used entry claiming more bytes than the guest offered |
| `gpu_remote_protocol` | the isolated-renderer wire format, both directions, with an exact re-encode check |
| `usernet_frames` | the user-mode NAT's receive path (WHP-1704): arbitrary guest frames — raw bytes, shaped Ethernet, shaped IPv4 with the header and transport checksums fixed so the fuzzer reaches ICMP, DHCP, the DNS relay and the whole smoltcp TCP state machine — through `usernet::offline::OfflineNet`, the real router with **no host sockets** (a fuzzer that could connect would dial arbitrary internet addresses at libFuzzer speed). Asserts the flow table stays inside `MAX_FLOWS`, the guest queue inside `MAX_QUEUED_FRAMES`, and that every emitted frame is one the guest's own device would accept and claims to come from the gateway |
| `snapshot_parse` | the whole snapshot parser (ADR-0006): the container's header and index, every section decoder against raw bytes, arbitrary bytes spliced into a well-formed container so the decoders are reached *through* the digest checks rather than around them, **and** the memory section's compressed block framing — a region header this build agrees with, then block lengths, codec and LZ4 payload from the fuzzer. That last layer asserts something stronger than "no panic": anything that decodes must survive a re-save and a second restore **unchanged**, which is the memory section's version of the exact re-encode the other sections get |

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

**A second suspend has to carry the guest's *later* memory.** The bug that shape
of test exists for is the one a dirty-page scheme would have and a full save
would not: a save that wrote only what some log said had changed, and missed a
page. It does not crash — the guest comes back from the *second* file where it
was at the *first*, or half at each. `a_second_suspend_carries_the_later_memory`
suspends at heartbeat *a*, resumes, runs on, suspends again at *b*, and requires
the second file to continue from `b`; it also asserts the gap is a real one
(`b > a + 2`), because otherwise "it continued" would be true of the earlier
file too, and that the first file still restores to `a`. It is also the only
test that walks the manager's loop — resume, work, suspend again — end to end.

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
- **The hypervisor's dirty log answers a different question than you think.**
  `crates/vmm-core/tests/dirty_log.rs` is four short tests and one of them is the
  whole finding: arm tracking, write two pages of guest RAM *from the VMM*, and
  both hosts report nothing. On this AMD machine KVM also reports pages the guest
  only executed from. Before building anything on a write log, write the test
  that asks what it misses — `ENTANGLED_TRACK_DIRTY=1` makes a real suspend print
  the answer for its own guest.

## Benchmarking a snapshot, and what a reference is

`crates/vm-snapshot/tests/memory_bench.rs` is `#[ignore]`d and allocates
gigabytes. It exists so a change to the memory dump can be argued rather than
asserted:

```bash
cargo test -p vm-snapshot --release --test memory_bench -- --ignored --nocapture
ENTANGLED_SNAPSHOT_THREADS=1 ENTANGLED_SNAPSHOT_COMPRESS=0 cargo test ...   # vary one knob
ENTANGLED_BENCH_MIB=4096 cargo test ...                                     # a bigger guest
```

Four rules it encodes, each of which cost something to learn:

- **Release, always.** The zero scan is the hot loop and `opt-level=0` ruins it.
  The test says so in its own output when it is built wrong.
- **Report the cold pass, not only the warm one.** A real guest's untouched RAM
  is not resident in the host — nothing has ever written it — so the scan takes a
  minor fault per page to read a zero. That is what a suspend pays. A benchmark
  that discards its first run understated the old scanner by a third (5.07 s cold
  against 3.18 s warm on a 2 GiB guest).
- **The synthetic guest's compressibility is a choice, and it is a pessimistic
  one.** Three pseudorandom bytes in every eight give LZ4 1.6×; a real desktop
  guest gives 2.2–2.3×. A page filled with a repeating pattern would give
  twenty, and the number would be a fiction.
- **Compare against a run beside it.** This machine is shared: an installed
  Ubuntu suspend read 2.99 s with a Windows build running alongside and 888 ms
  without. And inside WSL, `CLOCK_MONOTONIC` runs a wandering few thousand ppm
  fast (see "The clock finding"), so an absolute Linux figure is up to ~4 % long
  — a ratio between two runs in the same minute is not.

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
| `crates/vmm-core/tests/dirty_log.rs` | either | what each hypervisor's write log does and does not see (ADR-0006) |
| `crates/vm-snapshot/tests/memory_bench.rs` | either | what a memory dump costs, against itself (`--ignored`, **release**) |

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

### `MpInitLib: Find 1 processors` was a bug in our clock, not a flake

For months this was written up here as an unavoidable timing flake: under
parallel load a UEFI boot test would report `MpInitLib: Find 1 processors`, the
tables it was waiting for would not appear, and a rerun on a quiet machine was
always green. It was a real defect in `machine_x86::acpi::pm`, fixed on
2026-09-09, and it is worth keeping the story because the shape recurs.

The PM timer served a 32-bit `IN` **one byte at a time**, sampling the host
clock again for each byte. A carry out of the low byte between two samples
returns a value up to 255 ticks ahead of the counter, so the next read looks
like it went *backwards*. EDK2 measures its 50 ms AP-detection window by
differencing successive PM-timer reads (`MpLib.c::CheckTimeout`) and treats any
negative difference as the 24-bit counter having wrapped — it adds a whole
4.7-second cycle to its elapsed total and abandons the application processor on
the spot. Idle, the four samples are nanoseconds apart and only an exact carry
tears them; loaded, the exit handler itself can be preempted mid-access, which
is the entire load dependence.

Measured with the firmware boot repeated under CPU spinners on 16 cores
(boots that lost an application processor / boots run):

| | KVM idle | KVM, 48 spinners | KVM, 64 spinners | WHP idle | WHP, 48 spinners |
|---|---|---|---|---|---|
| before | 1/17 | 5/15 | 12/24 | 0/20 | **18/20** |
| after | 0/20 | — | 0/66 | — | 0/50 |

WHP suffers far more because its port-I/O exits are slower, so the four samples
inside one access are further apart.

Three things to take from it:

- **A counter register is sampled once per access, not once per byte.** Anything
  free-running that a guest can read wide — a timer, a cycle counter, a queue
  index — has to be latched for the whole access. The regression test is
  `acpi::pm::tests::a_wide_timer_read_is_one_sample_of_the_counter`, and its
  invariant is the one real hardware offers: the value read lies between the
  counter immediately before the access and the counter immediately after.
- **A test that tolerates either answer cannot catch this.** The UEFI boot tests
  now assert the *configured* processor count (`uefi_acpi`, `uefi_highmem`,
  `uefi_iso`, `whp_uefi`, `whp_highmem`). Widening a tolerance is how a bug
  survives months.
- **Load is a test input.** None of this reproduces on an idle machine.
  `tests/boot/tests/ap_startup.rs::ap_startup_campaign` (ignored;
  `$ENTANGLED_AP_BOOTS`) is the KVM form — it dates the INIT-SIPI, the AP's
  first executed instruction and the firmware's verdict on one timeline — and
  the equivalent on WHP is looping `whp_highmem` while spinners run.

If you *do* see a wrong CPU count again, the run loops now say so themselves:
`vmm_core::VcpuCensus` logs at warn with the configured and started counts and
names the processors still waiting for their SIPI.

### Boot the shape the profiles use, not just the small one

Until 2026-09-08 every UEFI boot test built a 2048 MiB guest, so the whole
high-RAM split — two guest-memory regions, RAM continuing at 4 GiB, a 64-bit
PCI aperture above the top of RAM — had no boot coverage at all, while the
desktop profiles have asked for 4096 MiB since `examples/ubuntu-desktop-live.toml`
was written. `tests/boot/tests/uefi_highmem.rs` and its WHP twin
`crates/vmm-core/tests/whp_highmem.rs` close that: 4096 MiB, both regions
mapped, `PlatformAddHobCB: HighMemory` in the log, `Pci64Base` at the end of
RAM, ACPI installed, no `X64 Exception`, Boot Manager reached. 1.5 s
on KVM, 2.8 s on WHP — a size, not a duration, so there is no excuse for the
next machine-wide constant to be tested only at 2 GiB.

Read the numbers, not the prose: with `--nocapture` this test prints
`PlatformGetFirstNonAddressCB: FirstNonAddress=0x140000000` and
`AddressWidthInitialization: Pci64Base=0x140000000 Pci64Size=0x3FFEC0000000`,
i.e. the firmware's 64-bit aperture starts at **exactly** the top of RAM and
runs to 2^46. EPIC 20's shared-memory window is placed at the same address for
that reason (`machine_x86::layout::pci_mmio64_base`), so if this line ever
moves, `tests/boot/tests/pci_shm.rs` is what will notice.

They also compare the PVH hand-off block byte-for-byte across the run. That is
the assertion that turns "the firmware took a `#GP` in `AcpiPlatformDxe`" into
a sentence: EDK2 re-reads `hvm_start_info.rsdp_paddr` out of guest memory at
the end of DXE, so a scribbled block surfaces half a boot later as a
non-canonical dereference with no hint of what happened (ADR-0003).

Note what they deliberately do **not** assert: the firmware's CPU count. That
is the load flake above, and a regression test must not inherit it.
