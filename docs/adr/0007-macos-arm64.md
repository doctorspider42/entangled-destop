# ADR-0007: native macOS on Apple Silicon, Ubuntu ARM64 and accelerated 3D

- Status: accepted direction; implementation in progress
- Date: 2026-10-14
- Extends: ADR-0002 (host portability), ADR-0004 (Venus)

## Objective

The product must start natively on macOS and run Ubuntu with hardware-backed
3D acceleration. A compiling manager, a CPU smoke guest, or a host Vulkan
device alone does not satisfy this objective. Acceptance requires a real
Ubuntu ARM64 desktop, guest Vulkan and OpenGL rendering on the Apple GPU,
input, disk and network, and orderly shutdown from the native application.

## Decision

Use Hypervisor.framework for ARM64 CPU execution and memory mappings. Reuse
the existing checked GuestMem and transport-independent virtio devices. The
ARM machine must be separate from machine-x86: x86 boot parameters, PVH,
APIC and CPU snapshot structures are not an ARM platform contract.

Use the existing Venus protocol/executor with a host Vulkan-on-Metal driver.
KosmicKrisp is the first driver to validate on Apple Silicon. OpenGL is served
by guest Mesa Zink over Venus. Native wgpu windows use Metal. This retains
the project's own VMM and device implementation and its no-QEMU rule.

The fastest presentation path remains a requirement to implement and measure,
not something implied by Apple's unified memory. The current shared scanout
export/import path is Windows-specific and cannot be passed to Metal.

## Implemented and measured

On this Mac (Apple M3 Pro, macOS 27.0.1):

- `vmm_core::hvf` maps checked GuestMem regions, creates thread-owned ARM64
  vCPUs, accesses GPRs/PC/PSTATE and reports exception/timer/cancellation exits.
  Machine-facing register/exit contracts are portable `vmm_core::hv::arm64`;
  Apple FFI identifiers remain inside the backend.
- VM ownership is process-wide. Arc retains mappings until the last vCPU is
  destroyed. Each vCPU is !Send/!Sync; a separate cancellation handle can be
  used on another thread and is retired before its ID can be reused.
- Rust hardware acceptance passes 100 create/run/destroy cycles, an HVC
  result of 42, VM retention after its public handle is dropped, cancellation
  of a busy guest, a late harmless kick and two live vCPUs on separate threads.
- The HVF shared-memory mapper retains its VM and checks ownership across RAM,
  GIC and every shared window. Shared buffers are read/write and non-executable.
  `HvfOptions::small_page_mappings` selects the macOS 26+ 4 KiB IPA granule
  through a runtime-resolved API, so Linux's neighboring 4 KiB blob mappings
  can share the original host allocation despite macOS's 16 KiB host pages.
  The real hardware test maps a 4 KiB subrange of a 16 KiB allocation: the
  guest reads 41 and writes 42, the host sees 42, overlaps with RAM/GIC/another
  window are refused, instruction fetch is denied, and access faults after
  unmapping. An unexpected native unmap failure terminates the VM process
  before backing can be freed; returning would permit a guest use-after-free.
  Ubuntu rootfs/disk/network/shutdown acceptance also passes with 4 KiB IPA.
- Venus ring blobs now publish only their reserved guest span, like memory
  blobs. Publishing a whole host allocation rounded to 16 KiB would overlap
  an adjacent guest 4 KiB blob. A portable regression injects two 16 KiB
  allocations behind neighboring 4 KiB reservations and verifies exact spans.
- `machine-arm64` describes RAM, PL011, PSCI, timers, GIC v3 and virtio-MMIO
  in a Device Tree. Direct Image loading checks the kernel's runtime span
  (including BSS), initrd placement and malformed/overflowing ranges before
  writing. Six boundary/firmware tests pass on this host.
- Apple's native GIC v3 owns the interrupt controller, including virtual
  timer delivery. It requires macOS 15+. An actual Ubuntu 6.8.0-142-generic
  kernel reaches a static ARM64 initramfs marker and powers off via PSCI.
  HVC exits already report the next PC; data aborts require advancing it.
  OS debug lock registers are modeled as not implemented; unknown system
  register accesses remain errors. PSCI FEATURES refuses unimplemented
  CPU_ON, CPU_SUSPEND and SYSTEM_RESET.
- The official Ubuntu Noble ARM64 cloud rootfs boots through its own initrd
  on the existing read-only virtio-block device. Its `/etc/os-release` reports
  Ubuntu 24.04.5 LTS. A shell acting as init reads that file, configures the
  existing usernet virtio-net device and successfully pings its router.
  Guest `/proc/interrupts` shows serviced GIC interrupts for disk and network
  (271 and 5 in the initial run), followed by orderly PSCI power-off.
  This initial acceptance is real Ubuntu userspace, but not systemd or a desktop.
- The ARM64 MMIO bus now attaches the actual Venus GPU, with its 4 GiB
  host-visible window at 4 GiB and a shared host-completion worker. The worker
  moved from machine-x86 to virtio-core; the old public module re-exports it
  for API compatibility. Its eight wake/pause/reset/shutdown tests pass.
  Ubuntu's kernel identifies capset 4 (Venus), resource blobs, context-init,
  the host window and `/dev/dri/renderD128`. Legacy kernel framebuffer
  context creation still logs unsupported-context responses; 2D scanout and
  explicitly selected Venus contexts work, and the legacy path needs cleanup.
- Usernet carried DNS and external TCP to Ubuntu's signed package repository,
  installing Mesa 25.2.8 and vulkan-tools into an APFS-cloned development disk.
  Writable acceptance syncs and remounts the filesystem read-only before
  power-off. The machine still needs an RTC; this development init sets wall
  time from the host before APT date checks instead of disabling those checks.
- Mesa 25.2 requires renderer-side `VK_KHR_external_memory_fd` even for
  device enumeration with no dma-buf WSI. The executor already serves that
  emulated declaration; it is now listed independently of dma-buf support.
  This does not enable guest fd exports or native image sharing. Actual
  guest `vulkaninfo` identifies `Virtio-GPU Venus (Apple M3 Pro)` as an
  integrated GPU, Vulkan 1.3, driver `venus`.
- The existing guest `vk-smoke` builds as ARM64 Linux using Rust, clang,
  rust-lld and a small linker sysroot extracted read-only from Ubuntu. It is
  passed to the guest over the existing read-only virtio-9p device. All nine
  selected guest checks pass: instance/device, mapped memory, transfers,
  a compute hash over 1,048,576 elements, exact 256x256 classic and dynamic
  triangle images, timeline synchronization and 1000 submissions with 1000
  signalled fences (none lost). Image hashes are
  `2678f2a0e39fba1b` and `d79d631c4d62403b`, matching the CPU reference.
  This proves hardware 3D through the running Ubuntu guest on Apple GPU;
  it is still a shell/init acceptance, not a full desktop or product launch.
- Native `entangled run` now launches explicitly selected `architecture =
  "aarch64"` profiles with HVF, direct Linux boot, virtio-MMIO disk, usernet,
  Venus, keyboard, absolute pointer and read-only 9P shares. Legacy profiles
  keep x86-64 and their serialized representation; unsupported ARM snapshots,
  SMP, ISO boot and audio are refused before host setup. The native window
  presents through Metal. The same nine Vulkan checks also pass through the
  ordinary native CLI, both headless and windowed.
- Full Ubuntu systemd and the GNOME desktop boot from a writable development
  rootfs. The real window displays the desktop and an 800x600 glmark2 model.
  Forced guest Zink reports `Accelerated: yes` and Apple M3 Pro through Venus;
  glmark2's selected ten-second build scene exits successfully, score 281
  (282 FPS). This is one workload, not a performance or graphics-conformance
  claim. The driver lacks fillModeNonSolid and EXT_custom_border_color;
  Zink warns and currently exposes OpenGL 2.1 / GLES 2.0. Default GNOME still
  uses llvmpipe, so an accelerated desktop compositor remains unproven.
- Full desktop acceleration now works in an explicitly configured Ubuntu
  X11 session. Selecting Zink globally initially caused Xorg Glamor to fail
  `AddScreen/ScreenInit` repeatedly. Disabling Glamor for the modesetting
  device lets Xorg display the software-WSI scanout, while GNOME Shell and
  GL clients render through Zink/Venus. The desktop is visually verified;
  the actual GNOME Shell process inherits the Zink environment, maps the
  virtio Vulkan driver, and its own journal identifies Apple M3 Pro through
  Venus. `glxinfo` reports `Accelerated: yes`. No driver capability override
  is used. `guest/ubuntu-arm64/configure-venus-desktop.sh` records this guest
  configuration and preserves unrelated environment entries. Executing the
  repository script twice inside the guest and booting again reproduces the
  accelerated GNOME desktop; diagnostic shutdown reaches PSCI power down and
  the native CLI exits 0. The previous
  unconfigured image still uses llvmpipe. This is experimental acceleration:
  OpenGL feature limits/warnings, software WSI and native pixel copies remain.
- Screenshot encoding runs separately from the input monitor. A runtime test
  opens GNOME Terminal through virtio keyboard input and types exactly
  `echo INPUT_OK`, with no repeated keys during screenshot bursts. Typing
  `systemctl poweroff` then reaches systemd's poweroff target and PSCI power
  down; the native CLI exits 0. Application Stop's power-key/grace fallback
  previously forced termination after twenty seconds. The manager protocol's
  separate `shutdown` now sends the power key without that forced deadline;
  a second native desktop run shows GNOME's sixty-second power-off countdown,
  then systemd/PSCI shutdown, window closure and process exit 0. `status` and
  keyboard `paste` replies also work; `echo MANAGER_PASTE_OK` is verified in
  the guest terminal. This exercises the engine's manager protocol, not a
  real manager button click. Stop/SIGTERM's forced fallback is still distinct.
- A native Metal compute shader writes 41 into an mmap allocation. An ARM64
  guest mapped to those same pages reads it and returns 42. No copy is made
  between Metal and the guest in this probe. This is not a texture-sharing
  or concurrent GPU/guest coherency test.
- The actual Venus admission policy accepts KosmicKrisp from Vulkan SDK
  1.4.363.0: Apple M3 Pro, host pointer import alignment 16384 bytes,
  host-visible type mask initially 0x1. Shared scanout export is currently false.
- The initial real triangle tests failed because the single unified host
  memory type was exposed only as guest-mappable. Optimal images cannot
  import host pages on this driver, leaving them no memory type. The policy
  now exposes index 0 as native device-local allocation and an appended
  mappable alias backed by the same host type and heap. Both actual Venus
  triangle tests pass, checking every pixel of a 256x256 image: classic
  render pass and dynamic rendering. This proves the protocol/executor and
  Metal driver path, not a running guest kernel or desktop presentation.
- The manager builds and starts natively with Metal enabled. Its screenshot
  mode renders and exits successfully using mock data; it has not booted a VM.
  It names macOS correctly and refuses current x86 saved sessions on this host.
- The native release application now bundles the manager, ARM64 engine,
  a signed native adapter, Vulkan Loader and KosmicKrisp. The adapter selects
  its own runtime with no developer SDK environment. The complete bundle is
  Developer ID signed with hardened runtime/library validation; only the
  engine has hypervisor and DYLD environment entitlements. Packaged `doctor`
  admits Apple M3 Pro and the packaged release engine passes all nine actual
  Ubuntu Vulkan checks. The signed manager starts and renders natively.
- A Finder drag-to-Applications DMG has its own procedural Retina background
  and app icon. The actual mounted Finder window is visually inspected, not
  merely inferred from the generated artwork. Apple accepts the final
  notarization request. Its ticket is stapled and validated; Gatekeeper
  accepts both the final DMG and the app copied out of it as
  `Notarized Developer ID`. Reproducible packaging, installer and notarization
  tools are under `scripts/`; instructions are in `installer/macos/README.md`.
  The installer contains the application/runtime, not an Ubuntu disk.
  This earlier package predates the automatic ARM64 provisioning flow below.
- macOS sparse disk relocation uses SEEK_DATA/SEEK_HOLE; APFS discard and
  write-zeroes use F_PUNCHHOLE. Allocation tests flush through virtio before
  measuring: APFS can commit speculative allocation during a punch, so a
  dirty file's st_blocks is not a stable baseline. Both MMIO and PCI reclaim
  tests check actual zeros and reclaimed allocation.
- Free-page reporting uses MADV_FREE and 16 KiB alignment on Apple Silicon.
  Unit tests verify that partial edge pages are preserved. Its behavior with
  hypervisor-mapped RAM still needs guest acceptance; Darwin may retain
  contents until reclamation.
- `cargo check --workspace --locked` passes on macOS ARM64. The existing
  x86-only bzImage/PVH loader implementations are architecture-gated; config,
  firmware inspection and other portable logic remain available.
- 55 vmm-core unit tests and Clippy for vmm-core, display, manager,
  linux-boot and uefi-boot pass. These are not Linux/KVM or Windows/WHP
  hardware acceptance results. Full workspace Clippy with warnings denied
  also passes on this platform.
- The 24 Venus policy tests, including the unified-memory regression, and
  Clippy for virtio-gpu pass. `cargo deny check` passes after the existing
  rustls 0.23.45 lock entry was updated to security patch 0.23.46 (the audit
  reported RUSTSEC-2026-0346 through -0349). New Metal dependencies satisfy
  the same license gate as the other hosts.
- `cargo test --workspace --locked` passes on macOS. Hypervisor-specific
  Linux/WHP tests are gated out, and GPU tests that lack a loader self-skip;
  the two Metal triangle tests above were run separately with KosmicKrisp.
  Shared-memory fixtures use the actual host page size. Snapshot byte-codec
  tests use a theoretical x86 machine on unsupported hosts while the public
  restore entry point still refuses before changing RAM.

## Reproducing the probes

Use a Rust toolchain with Cargo and Xcode command-line tools. Keep build
outputs outside the checkout, as the dev-environment skill requires:

```sh
export CARGO_TARGET_DIR=/private/tmp/entangled-target-macos
sh scripts/macos-hvf-smoke.sh
sh scripts/macos-hvf-smoke.sh --shared-memory # macOS 26+ for the 4 KiB mapping probe
sh scripts/macos-probe.sh
```

The shell scripts sign only their generated probe binary with an ad-hoc
signature and the hypervisor entitlement. The Metal probe cleans its own
temporary binary on exit. A sandbox that hides the GPU cannot run these
hardware checks; there is no silent self-skip.

For the kernel/rootfs acceptance, supply an ARM64 Ubuntu Image (gzip is
accepted) and its matching initrd. The optional disk must be the raw ext4
rootfs, not a partitioned QCOW2 image:

```sh
python3 scripts/build-arm64-smoke.py /private/tmp/entangled-arm64-smoke
sh scripts/macos-linux-smoke.sh /path/to/vmlinuz /private/tmp/entangled-arm64-smoke/smoke.cpio
cargo run -p machine-arm64 --example extract_boot --locked -- /path/to/rootfs.img /private/tmp/entangled-arm64-boot
sh scripts/macos-linux-smoke.sh /private/tmp/entangled-arm64-boot/vmlinuz /private/tmp/entangled-arm64-boot/initrd.img /path/to/rootfs.img
# Repeat with the mapping granule used by the Venus path (macOS 26+):
ENTANGLED_HVF_SMALL_PAGES=1 sh scripts/macos-linux-smoke.sh /private/tmp/entangled-arm64-boot/vmlinuz /private/tmp/entangled-arm64-boot/initrd.img /path/to/rootfs.img
```

The rootfs is opened read-only. Acceptance has a 30-second watchdog, requires
whole userspace marker lines (not strings echoed in the kernel command line),
Ubuntu identity, the ping result and nonzero guest-serviced disk/network
interrupt counts. The temporary static-init test is independent of the disk.
The development media is the immutable official
`noble/20260926/noble-server-cloudimg-arm64.tar.gz`, SHA-256
`1800eb56b6b839a08c020551e16fbd8c95b2772452ecce3737a9ad25fe594126`.
Initially it was checked against the official HTTPS checksum file. The CLI
now also verifies Canonical's detached signature with the pinned cloud-image
key `D2EB44626FDDC30B513D5BB71A5D6C4C7DB87C81`, independently checked against
Canonical's public-image documentation. `entangled fetch ubuntu-cloud --arch
arm64` downloads the 512 MiB archive, validates that signature before using
its digest, and writes a signed provenance manifest. The actual empty-cache
online fetch and the zero-network offline cache path both pass. This reuses
the Debian downloader's bounded resumable stream transfer; existing flow tests
still cover interruption/Range and digest failure.

`entangled install ubuntu --auto` now connects verified media to sparse RAW
extraction, read-only ext4 kernel/initrd extraction and native HVF provisioning.
It installs Ubuntu Desktop, creates the guest account and configures Zink/Venus
without mounting the guest filesystem on the host. A profile is published only
after an exact bounded serial success marker and actual PSCI SYSTEM_OFF. The
real empty-target installation completed in 151 seconds, and its subsequent
normal boot reached GNOME. Malformed archive paths/types/checksums, damaged
gzip trailers, existing targets and oversized files are covered by tests.
The initial macOS manager defaulted to Ubuntu, one CPU and 4 GiB RAM and exposes this
automatic download path. Its 130 tests pass on the native host; interactive
creation through the manager and repackaging remain to be verified.

A longer desktop run reported two guest watchdog soft lockups in
update-notifier. Host sampling still shows the vCPU inside Hypervisor.framework,
live input/control handling and continuing screenshot output. This is an open
stability finding, not a successful soak test. The earlier signed DMG does
not yet contain the new fetch, installation and wizard changes.

The native runner now retains an NSProcessInfo user-initiated activity for the
entire VM lifetime to prevent App Nap when the window is occluded. Host idle
sleep remains allowed. A subsequent 333-second boot/desktop/terminal/OpenGL/
orderly-poweroff run had no new soft lockups: `glxinfo` reported Accelerated=yes
on Zink/Venus Apple M3 Pro, and glmark2's single off-screen build scene ran at
416 FPS (score 415). This is a short regression check, not evidence that the
longer watchdog finding is fully resolved. The full workspace passed 2241
tests with 10 environment/artifact-dependent ignores. The lost-device test was
made to wait for the separate FATAL publication after the answered head, fixing
an observed race in its assertion without changing the wire ordering.

The updated drag-to-Applications DMG includes the verified installer, native
wizard, activity guard and corrected diagnostics. Both application and DMG
are signed with Developer ID Application (team 7L5KF39DAR); strict signature
verification and DMG checksum verification pass. Per the user's explicit
choice this updated preview is not submitted to Apple for notarization. It is
saved as `artifacts/macos/Entangled-0.2.0-macos-arm64-preview.dmg`; the earlier
notarized package is kept separately and predates these changes.
The signed bundle, with SDK environment variables removed, also completed a
fresh Ubuntu installation in 145 seconds using the wizard's actual command
arguments (including the absent optional vCPU/headless flags). The exact
success marker, read-only remount, PSCI power-off and published profile were
observed; the process exited successfully. Final bundled diagnostics report
HVF and Venus ready. Interactive wizard clicks remain unverified because the
native UI automation timed out while selecting the running app.

Native control now supports the manager's bare `thumbnail` command, using the
same default `.thumbnails/<name>.png` location and atomic 480-pixel PNG writer
as the other backends; explicit paths, including spaces, also work. Unsupported
CPU pause/reset/suspend buttons are disabled on native ARM64, with matching
guards in the action handlers. Hardware power-button shutdown remains enabled.
An opt-in native manager acceptance test launches the signed bundle through
the actual Supervisor control pipe, waits for a decoded, predominantly coloured
Ubuntu desktop thumbnail, then requests shutdown. This passed in 92 seconds,
with GNOME's actual wallpaper visible, guest PSCI power-off and successful child
exit, without a forced cancellation or SDK runtime environment. The test is
ignored by default and requires an exclusively owned Ubuntu profile:

```sh
ENTANGLED_NATIVE_ACCEPTANCE_ENGINE='/path/Entangled Desktop.app/Contents/MacOS/entangled' \
ENTANGLED_NATIVE_ACCEPTANCE_PROFILE=/path/to/owned/ubuntu.toml \
cargo test -p entangled-manager native_arm64_engine_thumbnail_and_orderly_shutdown -- --ignored --nocapture
```

The signed-bundle regression run stayed alive for 25 minutes, including the
GNOME lock screen and guest display sleep, without new soft lockups, watchdog
timeouts or device loss. Unlock restored the actual desktop; a newly opened
terminal again reported Accelerated=yes on Zink/Venus Apple M3 Pro. A subsequent
10-second glmark2 build scene completed at 99 FPS (score 98), followed by a clean
guest power-off at 1788 seconds and host process exit 0. The lower score than
the earlier short run is recorded without claiming maximum throughput or a
long production soak. This covers the previously failing time window; App Nap
is a supported explanation, not a proven exclusive root cause.

Final native gates pass: workspace tests (2241 passed, 11 ignored), workspace
clippy with warnings denied, formatting, cargo-deny and strict signatures. The
native ignored manager test was separately run and passed. The signed preview
DMG was updated to include thumbnails and honest lifecycle controls, with
SHA-256 `64fabe5e7f0af462e25cab567ca381539c5528fb42fcd59bea519d41a36238cd`.
Acceptance logs and a post-unlock screenshot are retained under
`artifacts/macos/acceptance/`. These tests were performed on macOS; Linux/KVM
and Windows/WHP hardware were not available for this acceptance run.

With the Vulkan SDK installed in a task-specific directory:

```sh
export DYLD_LIBRARY_PATH="$VULKAN_SDK/lib"
export VK_DRIVER_FILES="$VULKAN_SDK/share/vulkan/icd.d/libkosmickrisp_icd.json"
cargo run -p virtio-gpu --example venus_host_probe --locked
```

The development SDK was downloaded from LunarG and its SHA-256 matched the
official manifest: `163431e0203c8215286a17cf393643ce6b951f77d1b4f74ab30821a1f0beb09b`.
It lives in `/private/tmp/entangled-vulkan-sdk/macOS`, with no global driver
installation. Do not commit the SDK or guest images.

To enable the GPU in the real Ubuntu acceptance, set `ENTANGLED_HVF_GPU=1`
alongside the Vulkan driver variables above. `ENTANGLED_HVF_PROBES_DIR` exposes
only the specified directory as read-only tag `probes`; a development command
in `ENTANGLED_HVF_INIT_COMMAND` can mount it under `/run/probes` and run
`vk-smoke --checks 3,4,5,6,7,8,9 --repeat 2`. Custom commands must emit the exact
line `VMHOST_CUSTOM_READY` on success; their mere execution is not a pass.
`ENTANGLED_HVF_WRITABLE=1` is for a disposable copied disk only, and
`ENTANGLED_HVF_TIMEOUT_SECS` is bounded to 1..900 seconds (default 30).

## Native SMP acceptance (2026-10-15)

The runtime now creates one owning host thread per vCPU, with native GICv3
redistributors and flat MPIDR affinities matching the device tree. PSCI CPU_ON,
CPU_OFF and AFFINITY_INFO use bounded shared power state; powered-off threads
park on a condition variable. A CPU_ON supplies its entry address and x0
context and resets EL1 translation/timer state before re-entry. Shutdown wakes,
kicks and joins every vCPU before device reset or releasing RAM. CPU execution
is parallel; only actual shared device accesses take their device locks.

Profiles accept 1–64 CPUs, additionally checked against the framework's actual
limit before allocating RAM. The manager's CPU choice reaches the native
installer and its saved profile. New Apple Silicon profiles use all available
host cores, bounded to 64; Linux and Windows keep their existing half-host
policy. Existing profiles retain their explicit CPU count: set `vcpus = 12`
for the tested 12-core M3 Pro, with the VM stopped.

Real hardware boots passed at 1, 2, 4, 6 and 12 vCPUs. Installed Ubuntu 24.04
with Linux 6.8 also passed CPU-count checks, two offline/online rounds for each
secondary CPU and an additional CPU0 offline/online cycle, then clean PSCI
power-off. `guest/ubuntu-arm64/smp-acceptance.sh` is the opt-in guest probe for
an exclusively owned test disk; it is not part of normal guest startup.

Three interleaved OpenSSL SHA-256 runs per configuration, with no simultaneous
host build, gave these medians (16 KiB blocks, three seconds, one worker per
vCPU):

| vCPUs | SHA-256 kB/s |
|---|---:|
| 1 | 2,477,015 |
| 6 | 14,991,644 |
| 12 | 18,501,378 |

This workload scales 7.47× from one to twelve CPUs, and 1.23× from six to
twelve. It supports the full-core default for parallel work; it does not
predict every application or imply dedicated host cores. macOS schedules the
threads across the M3 Pro's six performance and six efficiency cores.

A full GNOME desktop reported `nproc = 12`, online CPUs `0-11`, and hardware
Zink/Venus rendering on Apple M3 Pro (`Accelerated: yes`). A ten-second
off-screen glmark2 build scene completed at 918 FPS (score 917), followed by
orderly guest shutdown and host exit 0. This is a selected graphics smoke test;
the already recorded Zink feature warnings and OpenGL 2.1 limits still apply.
The signed bundle also passed the real manager Supervisor's desktop thumbnail
and orderly shutdown test at 12 vCPUs. Fresh Ubuntu provisioning with
`--vcpus 12` reached its unique ready marker and actual power-off at 108.4 guest
seconds; the resulting profile carries twelve CPUs. Workspace tests pass
(2245 passed, 11 ignored); the native ignored test passed separately, as did
clippy, formatting, cargo-deny, strict signatures and DMG verification.
Linux/KVM and Windows/WHP hardware were unavailable for this run.

The updated drag-to-Applications installer is
`artifacts/macos/Entangled-0.2.0-macos-arm64-smp.dmg`, signed with Developer ID,
without notarization per the user's choice. Its SHA-256 is
`644101a3e36f6c0c7e229e259dd9c250177adfde154f7620638bc20dbe293cab`.
Logs and repeated benchmark results are retained under
`artifacts/macos/acceptance/smp/`.

## Remaining work

1. Complete the remaining ARM platform: an RTC,
   interactive serial input and architecture-specific CPU lifecycle.
2. Broaden verification of the now-working Zink/Venus GNOME compositor.
   Resolve missing feature requirements without inventing host GPU capabilities,
   and extend OpenGL workloads, image correctness and interactive acceptance.
3. Implement native image sharing and explicit lifetime/synchronization for
   the fast presentation path; measure current copies and frame latency.
4. Complete manager control integration, ARM64 Ubuntu provisioning/media
   verification and release integration for the signed application/installer,
   and enforced OS minimums (15 for CPU/GIC, 26 for current Venus mappings).
5. Verify shutdown through actual manager controls and further repeated
   full desktop startup/shutdown and filesystem integrity. Add audio if used
   by the shipped default profile. ARM ISO/UEFI installation is not connected.
6. Record runtime and latency measurements on real workloads. Native CLI,
   full desktop scanout, selected guest Vulkan and Zink OpenGL, disk/network,
   keyboard typing and guest-initiated orderly shutdown have evidence; these
   alone do not prove the full product objective.

ARM CPU snapshots and lifecycle reset cannot use X86CpuState. Existing
snapshot formats must refuse unsupported ARM state until a complete new
architecture contract exists; successful CPU bring-up does not imply
pause/reset/suspend support.

## Native suspend amendment (2026-10-29)

The earlier snapshot/lifecycle limitation is superseded for suspend and
restore by the ARM64 contract in [ADR-0006](0006-suspend-restore.md#amendment-2026-10-29-native-apple-silicon-suspendrestore).
The manager's Suspend action, native window toolbar/Ctrl+Alt+S, `save` control
command and `entangled resume` now use it. Restart/reset remains unavailable.
`scripts/macos-suspend-smoke.sh` is the signed native CLI regression, covering
four active CPUs and PSCI-off CPUs across two snapshot generations. It uses
an embedded guest probe. Installed GNOME/Venus evidence and remaining limits
are recorded below.

## Installed GNOME GPU suspend acceptance (2026-10-29)

A fresh, signature/digest-verified Ubuntu 24.04.5 ARM64 installation ran in a
native window with 4 vCPUs, 4 GiB RAM and a 2048x1536 guest desktop. Mesa 25.2.8
reported `Accelerated: yes` and Zink/Venus on Apple M3 Pro; Vulkan enumeration
identified the same GPU. The workload cycled glmark2 build, texture and Phong
shading scenes. These are functional tests on an unoptimized development engine,
not release performance measurements.

The first longer capture failed at the 64 MiB reconstruction-history budget.
The running guest continued drawing after refusal. A fresh short capture then
exposed a separate overly strict image-flag check: the compositor used a private
BGRA image with flags 0x508. The flag permits aliasing but does not establish it.
The corrected copy path accepts that capability while retaining the existing
native requirement-span checks that reject actual overlapping live bindings.

After that correction, two short disk-save/fresh-process-restore cycles passed:

| Witness | Before save | First restore | Second restore |
|---|---|---|---|
| Boot ID | `6daa9f14-d94a-41cb-adf3-e025ed97d069` | same | same |
| GNOME Shell PID | 942 | 942 | 942 |
| Existing glmark2 PID | 1622 | 1622 | 1622 |
| Completed scene results | 3 | 12 | 23 |

A new `glxinfo`, Vulkan enumeration and a fresh off-screen glmark2 build scene
also succeeded after each restore. The final guest reached systemd poweroff and
PSCI SYSTEM_OFF, and all three engine processes exited 0. Saves wrote 707 and
717 MiB in 23.73 and 38.93 seconds respectively in this development build.
Native checkpoint regressions passed (135 tests, including actual separate-
process GPU pixel reconstruction), as did virtio-gpu Clippy with warnings denied
and formatting. Native shared-image-export tests report a capability skip on
this driver. No KVM/WHP hardware or host reboot was part of this acceptance.

The follow-up compactor described in ADR-0006 removes superseded primary
recordings, overwritten/freed descriptor writes and repeated fence-reset and
resource-0 semaphore-import recipes. Object lifetimes and import generations
stay ordered; secondary references and descriptor-copy sources are conservative
retention barriers. The 64 MiB hard/shared budgets remain as safety limits for
irreducible reconstruction state. A journal which already failed cannot be
repaired by closing an application: its dropped records require a fresh session.
Durable local logs, structured witnesses and images are under
`artifacts/macos/acceptance/suspend-desktop/`. The guest witness script is
`guest/ubuntu-arm64/gpu-suspend-acceptance.sh`.


### History-compaction duration acceptance

The follow-up ran five minutes of continuous glmark2 load before the first save,
then three more minutes of the same workload after a fresh-process restore,
followed by another save and fresh-process restore. Both cycles passed on M3 Pro.
The cold writer used primary/descriptor compaction; both resumes and the second
writer used the final compactor including fence resets and temporary imports,
also checking compatibility of the unchanged recipe format.

| Witness | First save | First restored probe | Second save | Second restored probe |
|---|---|---|---|---|
| Boot ID | `ba8cb59d-a4b2-4dc0-b2df-0d15991fcfba` | same | same | same |
| GNOME / existing glmark2 PID | 948 / 1653 | same | same | same |
| glmark2 start time | 4700 | same | same | same |
| Completed scene results | 249 | 259 | 407 | 417 |

New GL/Vulkan queries and a fresh off-screen GL scene passed after both restores.
The first two running phases discarded more than 630 MiB of redundant recipes,
while preserving the unchanged 64 MiB retained-state budget. The guest shut down
normally; all three engine processes exited 0. The first save took 263.67 seconds
under host memory pressure (the controller's 180-second timeout was extended and
the same save completed); the second took 27.46 seconds. Both files were about
740 MiB. These are functional results, not a performance comparison.

The final change passed 2463 workspace tests (13 ignored), full-workspace Clippy
with warnings denied, and 143 checkpoint tests with native Vulkan available.
The broader native Vulkan suite had seven failures outside recipe retention:
four unsupported-extension fixtures, two memory-policy assertions and a retired
buffer-copy fixture. Their logs are preserved; the full native suite is not green.
`history-final-cycles-result.json` is the final desktop verdict, superseding the
initial controller timeout. The persistent test VM is powered off with its local
signed engine updated; its now-stale test snapshot was removed after shutdown.


### Native reliability follow-up

The recipe reader now accepts v2 and v3; v3 persists explicit allocation
ordinals and a high-water counter, allowing dead allocation/object lifetimes to
be reclaimed without changing live imported-page/shared-payload ownership.
Core recipes/imports retain the 64 MiB shared limit, with separately bounded
eight-byte generation metadata. ADR-0006 describes the compatibility and
reachability rules.

The seven native fixture failures above were resolved or gated by the actual
advertised hardware capability. UMA tests now exercise split guest memory
heaps; optional Zink extensions are enabled only when advertised. The native
transform-feedback execution fixture skips when the driver lacks the extension.
The retired-buffer fixture re-records its ONE_TIME_SUBMIT command buffer before
a second submission. Conditional rendering, triangle fan pixel rendering and
cross-context content tests run on this driver. The resulting native suite
passed 894 tests, including the native pool and generation/page-link edge cases.
The full workspace passed 2478 tests (13 ignored), Clippy, formatting and cargo
deny; the two native macOS CLI suspend tests passed too.

Longer load exposed a separate issue beyond recipe retention. One old-engine
run was killed before capture; another grew from approximately 3 GiB to
92.6 GiB physical-footprint accounting near 19 minutes. vmmap attributed about
89.9 GiB to IOAccelerator graphics and 266 MiB to malloc allocations. The latter
run was powered off through the guest before completing acceptance; neither
run is a successful long-duration suspend result. A native autorelease scope
was added around Vulkan operations on long-lived renderer/vCPU/restore threads;
its sustained effect must be checked by endurance acceptance, independently of
its passing native nested-pool regression.

Evidence for this follow-up is under
`artifacts/macos/acceptance/suspend-reliability/`. An abandoned controller left
one test VM alive; a subsequent boot briefly reused its writable disk. That
invalidated those runs. Both VMs were stopped, the test harness gained a process
lock and an open-disk check before touching its control socket, and the guest
was booted read-only for forced filesystem repair. The first check requested a
reboot; the next forced check finished successfully and reached graphical.target.
Those preparation runs are separate from the endurance verdict.


The pooled-lifetime extension subsequently passed 896 native Vulkan tests and
2480 workspace tests (13 ignored), Clippy and formatting. It reduced a live
restored context's retained recipes from about 4 MiB to 80 KiB while preserving
its original glmark2 process and passing new GL/Vulkan work. However, that
windowed follow-up crossed the test harness's 8 GiB footprint guard shortly
after restoration/fresh GPU work; the controller powered the guest off normally.
The planned 30-minute final load did not complete. Autorelease scopes alone
therefore do not establish that the native memory-growth finding is resolved.
Further isolated presentation and headless guest measurements are in progress.


The isolated host-presentation diagnostic completed five minutes of full-plane
2048x1536 uploads (about 700 MiB/s), with an observed peak physical footprint of
356271208 bytes (about 340 MiB) and exit 0. This does not reproduce the windowed
VM's growth on its own. The headless GPU workload on an exclusively owned APFS clone at the same
2048x1536 resolution passed 30 minutes of continuous glmark2, a fresh-process
restore, three further minutes of GPU work, and a second fresh-process restore.
The boot UUID, compositor and glmark2 PID/start time remained identical;
scene counters advanced and fresh GLX/Vulkan/offscreen GL work passed after
both restores. All three engine processes exited 0. The source save took
24.05 s and wrote 779 MiB; observed peak footprints were 7233358264,
5506582008 and 2821053928 bytes (capture included). See
`headless-cycles-result.json`; this isolates guest GPU execution from the
native window's uploads.
The installed runtime identifies itself as `vulkan-sdk-1.4.363 (git-269a955c89)`.
A published command-memory patch for a different Mesa branch is a diagnostic
lead, not an applied or verified fix for this runtime. No SDK/app installation
was changed.


### Hidden-window uploads and lifecycle feedback (2026-10-30)

The next windowed run crossed the 8 GiB test guard during its source load
(11110278752 bytes observed); it powered off normally and is not a passing
suspend acceptance. Command-pool cache release alone did not fix that growth.

A separate native-window reproduction identified a host presentation bug:
`draw_frame` prepared `queue.write_texture` uploads before checking occlusion,
then returned without `queue.submit`. Continuous frames therefore accumulated
unsubmitted staging allocations while the native app was hidden. A bounded,
VM-free old-engine reproduction grew from about 330 MiB to 4417933392 bytes
within seconds after Cmd+H, then its 4 GiB guard stopped the disposable probe
cleanly (`hidden-before-memory.json`). Check visibility and nonzero window size
before preparation; preserve dirty damage in the CPU mirror while hidden. The
existing unocclusion event requests a redraw and uploads the latest image.
The identical fixed-window probe passed 120 seconds, including more than a
minute hidden, with 4966 accepted full-plane updates, 851 rendered frames,
356271232 bytes peak footprint and exit 0 (`hidden-after-memory.json`). No
uploads were queued during the hidden interval; the CPU mirror kept receiving
frames. The full windowed acceptance subsequently passed; see the final verdict below.

Native VM windows show persistent Saving/Restoring feedback using the Deep Field
panel and linked ice rings, plus an indeterminate activity rail. Completion is
explicit, with no notice timeout or guessed percentage; save refusal clears the
indicator and resumes the original guest. The manager has a distinct Resuming
state, polls the existing status command with at most one outstanding request,
and clears that state only when the engine reports Running/Paused or exits.
The native ARM64 engine reports Starting until CPU/GPU restore is ready, and
prints control replies on a fresh line even during PL011 output. Both loading
surfaces respect disabled interface motion. Mock manager previews and a native
window preview were visually inspected; process tests cover slow readiness,
readiness completion and failed-restore exit.

The real native manager was then exercised on the exclusively owned clone:
Start → Suspend (644 MiB, 25.82 s) → Resume → Suspend again (635 MiB, 20.39 s).
Both the manager and native VM window showed the live Saving/Restoring panels;
both cleared them after the engine's Running reply. The restored GNOME desktop
was visible, and the second save succeeded. Manager and children closed normally
(`manager-live-result.json`, `manager-live-engine.log`; the manager replaces its
run log on Resume, so the first save was recorded before that replacement).
The unrelated x86 bootstrap-kernel warning is now omitted for native ARM64
profiles, which name their Ubuntu kernel/initramfs directly.

The presentation fix also acquires/retries the native surface before staging
uploads, and validates the viewport before consuming CPU damage. A surface
timeout or failed recovery therefore cannot accumulate unsubmitted writes
either. Acquisition remains outside the scanout mutex and adds no blocking GPU
Wait. Endurance restores use the optimized, signed local app bundle, with its
own copied Vulkan runtime and notices; hashes are in final-package-hashes.json.

### Final native acceptance verdict (2026-10-30)

`occlusion-cycles-result.json` is PASS: 30 minutes of continuous accelerated
Ubuntu/GNOME/glmark2 load with the native window hidden, save → fresh-process
restore → fresh GLX/Vulkan/offscreen GL verification → three more minutes of
GPU load → save → second fresh-process restore and fresh GPU verification.
Boot UUID 1412d393-1be0-4f02-9133-153c98e2729e, compositor PID 955 and glmark2
PID 1672/start 4658/nonce witness-1444-1793318384 stayed identical. Validated
scene counters advanced 1564 → 1575 → 1761. Every engine exited 0; no footprint
guard fired. Source and the two resumed processes peaked at 6769666056,
2951319040 and 2919276128 observed bytes respectively, including capture.
The source debug save wrote 731 MiB in 33.71 s; the optimized resumed save
wrote 731 MiB in 2.79 s. Retained recipe storage compacted from about 4.3 MiB
to 146336 bytes near the end of the source load.

The cold process used the debug engine with the hidden-window upload gate;
both restores used the final optimized .app bundle, including the additional
acquire-before-upload ordering and its own Vulkan runtime. The mapped engine
and both Vulkan libraries were verified inside that bundle
(`package-runtime-mappings.log`, `final-package-hashes.json`). The restored
window visibly rendered glmark2 correctly. The final presentation preview
also passed 90 s of visible/hidden full-plane updates, peaking at 380863616
bytes and exiting 0.

Final checks: 2488 workspace tests passed, 13 ignored; the real-supported-GPU
fixtures ran within the 899-test virtio-gpu suite. Display passed 145 tests
plus its doc test; manager passed 144 tests. The signed packaged engine passed
both native CLI suspend/SMP acceptance tests. Workspace Clippy, formatting and
cargo-deny's advisories/bans/licenses/sources gates passed. This is evidence
for the M3 Pro/runtime/workload tested, not unlimited support for every Vulkan
state or an increase to the retained-state safety limit.

The persistent test VM is powered off normally and has a self-contained local
release bundle under its .engine directory, with run.sh and manager.sh launchers.
Its stale test snapshots and the exclusively owned diagnostic disk clone are
removed during task cleanup; no user media/kernel cache or installed app is
replaced. A copied signed .app must preserve Frameworks symlinks; deep/strict
signature verification of the persistent copy and its doctor probe passed.

### macOS SDK compatibility during publication

The macOS 26 release runner exposed a link-time regression hidden by the local
macOS 27 SDK: hv_vcpu_get_serror and hv_vcpu_set_serror are declared
API_AVAILABLE(macos(27.0)). Rust extern declarations do not inherit that
availability annotation. The backend now resolves both symbols at runtime,
caching the pair only when both exist. Older SDKs therefore link normally.
Full CPU save/load returns SnapshotUnsupported when either entry point is
missing, before reading or mutating CPU state; it never substitutes an empty
pending-SError state. Normal macOS 26 VM execution retains its existing support.
Unit fixtures cover each missing symbol, both missing, and preservation of the
resolved functions' pending-state values and native result codes. Native CLI/SMP
acceptance verifies the resolved path on macOS 27.
