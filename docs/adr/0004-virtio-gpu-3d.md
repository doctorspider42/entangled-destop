# ADR-0004: 3D acceleration for virtio-gpu — classic VirGL on virglrenderer, loaded at runtime

- Status: accepted
- Date: 2026-08-19
- Extends: [ADR-0001](0001-mvp-architecture.md), constrained by
  [ADR-0002](0002-linux-first-whp-ready.md) (portability rules)
- Backlog: GPU-001…GPU-012 (section 7 of the backlog)

## Context

The MVP's virtio-gpu is 2D-only: the guest's mesa stack finds no
`VIRTIO_GPU_F_VIRGL`, falls back to **llvmpipe**, and GNOME renders a 1080p
desktop on guest CPUs. The goal of this epic is that the guest's GL stack
takes the virgl path instead, so mutter composites (and applications render)
against a host GPU. The device-side surface is fixed by the VirtIO spec
(§5.7): the `VIRGL` feature bit, capability sets, and the 3D command set
(`CTX_CREATE/DESTROY`, `CTX_ATTACH/DETACH_RESOURCE`, `RESOURCE_CREATE_3D`,
`TRANSFER_TO/FROM_HOST_3D`, `SUBMIT_3D`). What is *not* fixed is who executes
the guest's GL command streams on the host.

Constraints that shape the choice:

- **No copyleft in host code** (`cargo deny check` blocks GPL/LGPL/AGPL) —
  this kills several obvious shortcuts (see below).
- **The guest is untrusted.** 3D command streams, resource ids, transfer
  boxes and backing lists are all guest-controlled.
- **Two hosts** (ADR-0002): Linux/KVM today, Windows/WHP as a later phase.
  Protocol decoding and validation must build and test everywhere; only the
  host-GL half may be OS-gated.
- The device already presents through `virtio_gpu::ScanoutSink` into a wgpu
  `Bgra8Unorm` texture; whatever renders must produce BGRA rects for it.

### Host probe (2026-08-19, the actual dev environment)

Measured in the WSL Ubuntu 22.04 that runs all KVM work on this machine:

- `/dev/dri` does **not** exist (no DRM render node), but Mesa's EGL
  advertises `EGL_MESA_platform_surfaceless`, and a surfaceless context comes
  up as **`D3D12 (AMD Radeon PRO Graphics)`, GL 4.2 Compatibility, Mesa
  23.2.1** — WSLg's GPU paravirtualization via `/dev/dxg`, i.e. a real GPU,
  not llvmpipe.
- `libvirglrenderer1` **0.9.1** (MIT) is packaged in jammy.
  `virgl_renderer_init(VIRGL_RENDERER_USE_EGL | VIRGL_RENDERER_USE_SURFACELESS)`
  returns 0, reports `gl_version 42 - core profile enabled` and serves capsets
  VIRGL (1, max_ver 1, 308 bytes) and VIRGL2 (2, max_ver 2, 696 bytes);
  context create/destroy work. The full 3D host path is therefore *available
  and hardware-accelerated* in the primary dev/test environment.

## Options considered

### (a) Classic VirGL with virglrenderer as the host renderer — chosen

The guest's mesa `virgl` driver serializes Gallium command streams; the host
hands them to **virglrenderer** (the reference C library, also used by QEMU,
crosvm and libkrun), which decodes and replays them on a host GL context.

- **License**: virglrenderer is MIT. Its hard runtime dependencies are
  libepoxy (MIT), libEGL/libgbm (Mesa, MIT) and libdrm (MIT/X11) — all
  permissive. The GL *driver* stack it dlopens at runtime is the system's
  (Mesa is MIT; a proprietary driver is the user's choice) — the same position
  wgpu already puts us in. **Linking story**: we do not link it at all — the
  library is `dlopen`ed at runtime (`libloading`, ISC, already in our
  dependency graph via wgpu). `cargo deny` sees only `libloading`; building
  the workspace needs no C headers; a host without the library simply cannot
  enable 3D and says so. This is the same pattern our KVM/WHP split uses:
  capability discovered at runtime, never a build-time fork.
- **Host requirements**: EGL + GL 3.0+ (or GLES). Verified working in WSL
  (above) via surfaceless EGL on D3D12. Coexistence with wgpu presentation is
  clean: virglrenderer owns its own EGL contexts on the GPU worker thread;
  scanout pixels are read back (`virgl_renderer_transfer_read_iov` /
  `get_rect`) into the existing `ScanoutSink` BGRA path. A zero-copy
  dmabuf/EGLimage handoff into wgpu is a later optimization, not a
  correctness requirement — and WSL has no dmabuf anyway.
- **Guest requirements**: every stock Ubuntu/Debian guest ships mesa's virgl
  driver; nothing to install. `glxinfo` reports `virgl` the moment the
  feature bit and capsets appear.
- **Effort**: the device-side decode/validation is ours in any design;
  the renderer behind it is ~30 C entry points wrapped in one module.

### (b) Venus (Vulkan passthrough)

Venus encodes guest *Vulkan* over the same virtio-gpu channel; the host
decodes with virglrenderer's venus context type (or gfxstream). Modern
(this is what crosvm pushes), and long-term the better performance story.
Rejected for this epic:

- Venus requires **blob resources** (`VIRTIO_GPU_F_RESOURCE_BLOB`,
  `HOST_VISIBLE` mappings into a PCI BAR), a per-VM shared-memory region the
  transport does not have yet, timeline fences, and virglrenderer ≥ 0.10 with
  `VIRGL_RENDERER_VENUS` — jammy ships 0.9.1, so we would be building
  virglrenderer from source on every dev/CI host.
- The host needs a real Vulkan ICD. In WSL that means dzn (Vulkan-on-D3D12,
  experimental in Mesa 23) — the probe environment cannot run it credibly.
- The guest desktop story still needs GL: mutter/GTK render GL today, so a
  Venus-only device leaves GNOME on llvmpipe (zink-on-venus exists but adds
  another experimental layer).
- gfxstream: BSD-3-Clause? No — gfxstream is Apache-2.0, license-fine, but it
  is an AOSP component whose host build outside Android/crosvm is not
  packaged anywhere we target.

Venus is the right *second* 3D context type once blob resources exist; the
`Renderer3d` trait deliberately does not preclude it (capsets and context
types are data, not structure).

### (c) Pure-Rust VirGL decoder targeting wgpu

Reimplement virglrenderer in Rust: decode the Gallium stream and execute it
on wgpu. Honest assessment: virglrenderer is ~90k lines of C that encode a
decade of Gallium semantics (shader translation TGSI→GLSL among them);
a wgpu backend additionally has to bridge Gallium's binding model onto
WebGPU's. This is a multi-year project, not an epic — and it would still
carry the same guest-untrusted attack surface, just in Rust. Rejected as the
renderer; *retained as the shape of the test double*: the portable
`NullRenderer` implements the same trait with CPU-side resources so the whole
decode/validate/dispatch path runs (and fuzzes) on hosts with no GPU and no
virglrenderer, Windows included.

### (d) rutabaga_gfx (crosvm's abstraction crate)

BSD-3-Clause, wraps virglrenderer *and* gfxstream behind one Rust API — the
backlog's own suggestion (GPU-001). Rejected in favor of a direct binding:
rutabaga links virglrenderer at **build time** via pkg-config (its
`virgl_renderer` feature), which drags C headers into every workspace build
and breaks `cargo build --workspace` on Windows and on Linux hosts without
the -dev package; its API is shaped around crosvm's fence/display model and
still leaves all the guest-facing validation to us. We take its *architecture
lesson* (a renderer trait with pluggable backends) without the build-time
coupling. Revisit if/when we want gfxstream or Venus, where rutabaga's value
is real.

## Decision

1. **Strategy: classic VirGL.** The device offers `VIRTIO_GPU_F_VIRGL` and
   two capsets (VIRGL, VIRGL2) when — and only when — a working host renderer
   exists at VM start.
2. **Portable core, pluggable renderer.** All wire parsing, validation,
   bounds and the command dispatch live in `virtio-gpu` as portable code
   behind a `Renderer3d` trait. Two implementations:
   - `NullRenderer` (portable, always built): CPU-backed resources, structural
     validation of submit streams, no GL. Powers unit tests, transport tests
     and the fuzz target on every OS.
   - `VirglRenderer` (`#[cfg(target_os = "linux")]`): `dlopen`s
     `libvirglrenderer.so.1` at runtime, initializes EGL surfaceless
     (`USE_EGL | USE_SURFACELESS`), forwards the validated commands. The FFI
     surface is one module with `// SAFETY:` comments on every call.
3. **The validation layer is the trust boundary in front of C.**
   virglrenderer is treated as *inside* the trust boundary (like KVM), but
   nothing guest-controlled reaches it unchecked: command streams are
   length-walked before submit, ids map through bounded tables, transfer
   boxes are checked against the geometry declared at create time, backing
   lists go through the same checked `vm-memory` translation the 2D path
   uses, and every iovec handed across FFI stays alive (owned boxes + the
   `GuestMem` Arc) until explicitly detached. Pinned version: virglrenderer
   0.9.x (jammy's 0.9.1); the wrapper checks no versioned symbols beyond the
   0.9 API.
4. **Fences are synchronous in phase 1.** Every command completes before its
   response is written (exactly the 2D device's model), so `FLAG_FENCE` is
   echoed immediately. This is spec-correct; the cost is no host/guest
   pipelining (a guest may re-use a buffer the host GL driver is still
   consuming, a visual-only hazard). Real fencing
   (`virgl_renderer_create_fence` + poll fd) is phase 2.
5. **Scanout by readback.** `SET_SCANOUT`/`RESOURCE_FLUSH` on a
   renderer-owned resource read the flushed rect back into the existing BGRA
   `ScanoutSink` path (full-frame shadow buffer, dirty-rect reads). Zero-copy
   (dmabuf → wgpu texture import) is an optimization for a host that has
   `/dev/dri`; WSL does not, so it is not phase 1.
6. **Windows/WHP is not painted in.** The portable core builds and tests
   there today (NullRenderer). A future Windows renderer has two credible
   shapes — virglrenderer compiled for Windows (it supports GLES/EGL on
   ANGLE) or a Venus/gfxstream context — both slot behind `Renderer3d`
   without touching the device, exactly like `WhpOptions` vs KVM in ADR-0002.
7. **Config**: `[display] virgl = true` (default `false`). On a host where
   the renderer cannot initialize, `entangled run` fails with a clear error
   rather than silently booting 2D — a desktop profile that asked for 3D and
   got llvmpipe is the bug this epic exists to fix.

## Safety rules (the GPU-specific instance of "guest is untrusted")

- `SUBMIT_3D` streams are structurally validated before dispatch: the
  dword-length walk must cover the buffer exactly; size caps
  (`MAX_SUBMIT_BYTES`) bound staging; the fuzz target
  (`fuzz/fuzz_targets/gpu_3d_commands.rs`) drives the full decode +
  NullRenderer dispatch.
- Resource/context ids are guest-chosen names, never indices: they map
  through `HashMap`s bounded by `MAX_3D_RESOURCES` / `MAX_CONTEXTS`;
  duplicates and id 0 are rejected in-band.
- `TRANSFER_*_3D` boxes are bounds-checked against the geometry recorded at
  `RESOURCE_CREATE_3D` time before the renderer sees them; byte estimates
  are computed in u64 with explicit caps (`MAX_RESOURCE_3D_BYTES`).
- A malformed command fails that command with a `VIRTIO_GPU_RESP_ERR_*`; the
  device never panics and never resets mid-queue on guest input.
- Backing attach/detach across FFI: the iovec array is host-owned, boxed,
  and outlives the attachment; addresses are translated through checked
  `vm-memory` (`get_slice`), so an entry outside guest RAM fails the attach.

## Consequences

- Ubuntu/Debian guests get `glxinfo: virgl` with **zero guest-side setup**;
  GNOME/mutter takes the hardware path (its GL requirements are well inside
  virgl-on-GL-4.2).
- Runtime-only coupling to virglrenderer: `cargo build/test --workspace`
  stays header-free on both hosts; `cargo deny` is untouched except for
  `libloading` (ISC, already in-graph).
- Readback scanout costs one GPU→CPU copy per flushed rect; acceptable at
  1080p (the 2D path already does a CPU copy per flush), and the profile for
  phase-2 zero-copy is clear.
- Synchronous fences cap pipelining; phase 2 (real fences, then dmabuf
  scanout, then Venus-behind-the-trait) has a measured baseline to beat.
- The 8-year-old-package risk: jammy's 0.9.1 predates blob resources — which
  phase 1 does not use — and receives Ubuntu security maintenance. When we
  need newer (Venus), we build virglrenderer ourselves and the dlopen keeps
  that invisible to the crate graph.
