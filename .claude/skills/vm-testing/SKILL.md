---
name: vm-testing
description: Testing strategy for VMHost — unit/malicious-guest tests, boot-to-marker integration tests, 100-boot and soak runs, fuzzing, screenshot comparison, and how to run KVM tests in WSL/CI (backlog EPIC 14 + per-epic acceptance criteria). Load when writing or running tests.
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

## Fuzzing (MVP-1402)

- `cargo fuzz` targets under `fuzz/fuzz_targets/`, starting with the
  descriptor-chain parser and `debian-media::parse_sums`. Fuzz targets build
  the corpus from the malicious-guest unit tests. Not part of default CI —
  scheduled job with a time box.

## Invariants every test run enforces

- No vCPU threads or TAP devices left behind after a VM stops (assert in
  test teardown; acceptance: "closing the VM leaves no vCPU processes or
  graphics contexts").
- Interrupting the VMM must not corrupt a completed disk image (crash-safety
  test writes, kills the process, fsck's the image).
- A failing guest never takes the host process down: any panic in a device
  thread is a test failure by definition.
