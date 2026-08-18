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
5. **Endurance** (MVP-1403/1404): 100 sequential boots and the 8-hour soak
   are `#[ignore]`d tests invoked explicitly (nightly CI / manual), never in
   the default suite.
6. **Graphical** (MVP-1405/1406): screenshot the scanout (see host-display
   skill), compare against goldens with a small per-pixel tolerance; store
   goldens under `tests/graphical/golden/` as PNG (small resolutions for
   tests, e.g. 640×480, plus one 1920×1080 case).

## Running

- Local (Windows host): everything through WSL —
  `wsl -d Ubuntu -e bash -lc "cd /mnt/d/entangled-desktop && cargo test --workspace"`.
  WSL2 exposes `/dev/kvm`; the dev user must be in the `kvm` group.
- CI: standard GitHub runners now expose `/dev/kvm` on Linux; tier 3-4 tests
  run there, tiers 5-6 are scheduled jobs.
- Docker: `docker run --device /dev/kvm …` (see `docker/Dockerfile.dev`).

## Guest test images

- Built from `guest/` configs (EPIC 11: reproducible bootstrap kernel +
  initramfs). Binaries are cached build artifacts, **never committed to git**.
- The minimal test initramfs `/init` prints the ready marker, optionally
  runs a scripted probe (mount `/dev/vda`, `evtest`, DHCP check), prints a
  per-check `VMHOST_TEST_OK <name>` / `VMHOST_TEST_FAIL <name>` line, then
  calls `reboot(RESTART)` — NOT power-off: the machine has no ACPI, so
  power-off halts forever, while restart (with `reboot=k`) ends in a triple
  fault that reaches the host as a clean KVM_EXIT_SHUTDOWN. Test harnesses
  parse only these markers — never scrape free-form kernel output.
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

**Current result (100 boots, no virtio devices): 73/100 reached the marker; fds
and threads exactly flat, RSS 4080 → 4124 KiB.** So nothing leaks, but the
100/100 acceptance criterion is not met: about a quarter of boots stall at
exactly `Run /init as init process` — the first userspace write to the
interrupt-driven 8250 tty (`printk` before it uses the polled path) — waiting for
a transmitter-empty interrupt on IRQ 4 that never arrives. With a disk attached
the same defect stalls the first disk read with `INTERRUPT_STATUS` still reading
`INT_VRING`. Root cause: no MP table or MADT, so Linux uses virtual-wire ExtINT
through the 8259 instead of the IOAPIC. See the `IrqFdLine` docs in
`machine_x86::virtio`. Do not "fix" this by loosening the test.

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

## Fuzzing (MVP-1402)

`cargo-fuzz` targets live under `fuzz/`, which is its own workspace and is listed
in the root manifest's `exclude`: libfuzzer needs nightly and `-Zsanitizer`, so
`cargo test --workspace` must never try to build it.

```bash
rustup toolchain install nightly
cargo install cargo-fuzz

# Build all four targets.
cargo +nightly fuzz build --target-dir "$HOME/entangled-fuzz-target"

# Run one, time-boxed (the whole suite: chain_walk, mmio_transport,
# debian_sums, blk_request).
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
  status-bit set). Both findings so far came from such an assertion, not from a
  crash.
- Not part of default CI: a scheduled, time-boxed job.

## Invariants every test run enforces

- No vCPU threads or TAP devices left behind after a VM stops (assert in
  test teardown; acceptance: "closing the VM leaves no vCPU processes or
  graphics contexts").
- Interrupting the VMM must not corrupt a completed disk image (crash-safety
  test writes, kills the process, fsck's the image).
- A failing guest never takes the host process down: any panic in a device
  thread is a test failure by definition.
