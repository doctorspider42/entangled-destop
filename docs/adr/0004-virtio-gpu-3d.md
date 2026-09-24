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

## Amendment (2026-08-20): GPU-012 — renderer-crash containment

### The failure model, precisely

One reproducible failure drives this: after 1–3 minutes of sustained GNOME
compositing, WSLg's jammy mesa 23.2 D3D12 megadriver dereferences a NULL
gallium hook inside `virgl_renderer_submit_cmd` (`vrend` → `swrast_dri.so` →
call to `0x0`). The stream is structurally valid GL work; our validation front
is not implicated. The consequence is what matters:

- it is a **SIGSEGV on the device worker thread**, inside a `dlopen`ed C
  library, in *our* process;
- a SIGSEGV is process-fatal, so it takes the whole VMM down — every vCPU, the
  disk, the network, the guest's unsaved work;
- it is *not* recoverable in place: the faulting instruction cannot be retried,
  and the only in-process escape is a `siglongjmp` out of a signal handler,
  which (a) is UB by Rust's rules, (b) leaves mesa's own worker threads holding
  locks nobody will release, and (c) has to be right on the first try because
  it only ever runs when we are already dying.

The requirement therefore is not "survive the fault" but "**do not share a
process with the thing that faults**". The renderer must be able to die without
taking the VM with it: at worst the VM degrades to 2D/llvmpipe, the session
lives, and the user gets a warning.

### Decision: the renderer runs in a subprocess

`Renderer3d` is implemented by `remote::RemoteRenderer`, which spawns a helper
process holding the real `VirglRenderer` and talks to it over a `SocketPair`.
Everything the C library and the host GL stack can do — segfault, abort,
deadlock, leak an EGL display, get OOM-killed — is now confined to a process
whose death the VMM *observes* instead of sharing.

Why a subprocess rather than the alternatives:

| option | verdict |
|---|---|
| in-process signal trap (`sigaction` + `siglongjmp` out of the FFI frame) | rejected: UB across Rust frames, leaves mesa's threads holding locks, and the recovery path is only exercised while already crashing |
| renderer on a dedicated thread | does not help at all — a SIGSEGV is process-wide |
| `vhost-user-gpu`-style external GPU process (crosvm/QEMU) | the right long-term shape, and this is a subset of it; the full protocol needs shared guest memory and dmabuf handoff, neither of which this host can do (see the dmabuf probe above) |

**Guest memory never leaves the VMM.** The helper gets *bytes*, never guest
physical addresses: the client reads the guest pages it needs through the
checked `vm-memory` API and sends the span; the helper keeps a host-side
**shadow backing** per resource and attaches *that* as the resource's iovec.
This is a deliberate security property — the isolated process that runs
untrusted-guest-derived GL work has no window onto guest RAM — and it is also
what lets the design work without memfd-backed guest memory, i.e. without
touching `vmm_core::create_guest_memory` or either hypervisor backend.

The costs, stated plainly:

- one extra copy per `TRANSFER_*_3D` (guest pages → staging → socket → shadow),
  bounded by `REMOTE_XFER_WINDOW`;
- one extra copy per scanout flush (the packed rect travels the socket);
- ~50 µs of round-trip latency per command batch on a warm socket.

For the failure it prevents — the VM dying every 1–3 minutes on this host —
that is a trade worth making, so **process isolation is the default** on Linux
(`[display] virgl_isolation = "process"`), with `"in-process"` available for a
host whose GL stack is trusted and which wants the last copy back.

### What the device does when the renderer dies

The seam is portable and is the part that matters architecturally
(ADR-0002: a future Windows/ANGLE renderer wants exactly the same isolation,
with `CreateProcess` + a handle pair in place of `fork`/`socketpair`, and the
protocol types already build and test on Windows):

1. any I/O error on the socket, or the helper exiting, makes
   `Renderer3d::is_alive()` false;
2. the device notices on its next queue notification and **degrades to 2D
   once**: every held fenced response is released as `ERR_UNSPEC` (a chain the
   guest never gets back is a hang), the renderer is dropped, and a 3D-owned
   scanout binding is released so the window keeps its last frame;
3. later 3D commands are refused in band (`ERR_UNSPEC`), while the **2D half of
   the device keeps working** — which is what "degrades to 2D" means in
   practice: the guest can still put a console framebuffer on screen;
4. the driver is told `DEVICE_NEEDS_RESET`, but only *after* the guest has its
   completions and its interrupt;
5. the host log says exactly what happened, at `error`, naming GPU-012.

The demonstration is a test, not a story: `gpu_remote.rs` spawns a real helper
process, drives 3D through it, `SIGKILL`s it mid-flight, and asserts the device
survives, releases what it held, refuses 3D in band and still serves 2D.

**Demonstrated end to end** (2026-08-20, WSLg): the Ubuntu Desktop live session
booted with `virgl_isolation = "process"`, composited on virgl at ~30 fps for a
minute, and then its renderer process was `kill -9`'d under load. The log:

```
ERROR virtio_gpu::remote::client: the isolated 3D renderer died; the VM keeps
      running and virtio-gpu degrades to 2D (ADR-0004 GPU-012)
      pid=14214 status=Some(ExitStatus(unix_wait_status(9)))
      error=the renderer process closed the connection
ERROR virtio_gpu::device: the host 3D renderer is gone; virtio-gpu has degraded
      to 2D for this VM … The VM itself is unaffected
ERROR virtio_core::state: device failed to process a queue notification
      … error=the host 3D renderer was lost; virtio-gpu degraded to 2D
```

Thirty-seven seconds later the guest had rebuilt its scanout out of
`RESOURCE_CREATE_2D` resources (`scanout set … three_d=false`) and kept
presenting; the VM ran on for another two and a half minutes and shut down
cleanly on its own. A screenshot taken ~170 s *after* the kill shows a live,
fully composited GNOME session. In-process, this same event is a SIGSEGV that
ends the VM.

One bound isolation *adds*, and therefore has to name: a shadow backing is host
memory the in-process renderer would never have allocated (there, a backing is
guest RAM the guest already paid for). `REMOTE_MAX_BACKING` (64 MiB) bounds one
resource and `REMOTE_MAX_TOTAL_SHADOW` (512 MiB) bounds all of them together;
both are checked on *both* sides — the client so the guest gets a clean
`ERR_OUT_OF_MEMORY`, the helper because it does not trust the VMM either.

## Amendment (2026-08-20): phase 2 — real fences

Phase 1's decision 4 (synchronous fences) is replaced. A fenced command whose
work lands on the host GL timeline — `SUBMIT_3D`, `TRANSFER_TO_HOST_3D`,
`TRANSFER_FROM_HOST_3D` — now keeps its descriptor chain **out of the used
ring** until `virgl_renderer_create_fence`'s fence retires. Everything else
(capset queries, resource creation, scanout binding, and `RESOURCE_FLUSH`,
whose readback is synchronous by construction) still answers immediately even
when fenced: deferring those would add latency and no correctness.

The mechanism, and why it is shaped this way:

- **Completion runs on the device's own worker thread.** GL is thread-affine,
  so `virgl_renderer_poll` may only be called where EGL is current — the queue
  worker. A renderer therefore cannot complete anything itself; it can only ask
  to be *called*. That ask is `virtio_core::HostWaker`, and the Linux machine
  layer implements it as a write to **queue 0's existing eventfd** — the same fd
  KVM writes for a guest kick. A wake is thus an ordinary `queue_notify(0)`
  from the worker: no new thread, no new epoll slot, no new state machine, and a
  spurious queue-0 notify is a no-op for every device by construction.
  `DeferredWaker` covers the ordering (a device is inside its transport before
  the worker exists) by remembering a wake that arrives in the gap.
- **A monitor thread ticks only while fences are outstanding**, at 1 ms.
  QEMU's equivalent timer is 10 ms, a quarter of a 60 Hz frame; that is visible
  as jitter. `virgl_renderer_get_poll_fd()` returns **-1** on WSLg's D3D12 GL
  (no fence fd), which is why the tick — not the fd — is the load-bearing path;
  the fd is recorded for diagnostics and for hosts that have one.
- **Retirement completes a prefix.** Host fences retire in creation order and
  callbacks coalesce, so reporting fence *n* completes everything submitted up
  to *n*, oldest first — which is also what the guest's DRM fence timeline
  requires.
- **The guest cannot wedge the device.** `MAX_PENDING_FENCES` (64) bounds held
  chains and is checked *before* the renderer is asked for a fence (an unwaited
  host fence is pure waste). Past the cap a fenced command completes
  synchronously — deliberately phase 1's behaviour rather than an in-band
  error: it pins nothing, it cannot break a legitimately busy guest, and the
  hazard it re-admits (a guest reusing a buffer the host GL driver is still
  reading) is the one phase 1 shipped with. A fence that never retires is
  completed by a **watchdog** (`FENCE_TIMEOUT`, 2 s) with a loud host warning:
  a stalled host GL stack must not become a guest that never wakes up. A
  *failed* fenced command answers at once — an error has nothing to wait for —
  and a device reset releases everything held.

Measured on WSLg (D3D12 / AMD Radeon PRO), `virgl_fence_host.rs`: a real
`virgl_renderer_create_fence` on an empty submit retires in **≈2 ms** end to
end (one monitor tick plus the poll), against a 16.7 ms frame budget.

### Scanout readback, and why dmabuf zero-copy is *not* in phase 2

The intended phase-2 optimization was exporting the scanout resource as a
dmabuf and importing it into the wgpu presentation path. It is not
implementable on either half of this host, and the probe is worth recording:

- **No DRM node.** WSL's Ubuntu has no `/dev/dri` at all — only `/dev/dxg`, the
  paravirtualized D3D12 channel. There is nothing for a dmabuf to come from.
- **No export extension.** The surfaceless EGL display advertises
  `EGL_KHR_fence_sync`, `EGL_KHR_wait_sync`, `EGL_MESA_drm_image` — and
  **neither `EGL_MESA_image_dma_buf_export` nor `EGL_EXT_image_dma_buf_import`**.
  `eglExportDMABUFImageMESA` does not exist to call.
- **No import path either.** wgpu 26 has no safe external-memory API; importing
  a dmabuf means `wgpu_hal` Vulkan interop (`VK_EXT_external_memory_dma_buf` +
  `Device::texture_from_raw`), and our presentation here is wgpu-on-D3D12
  through `/dev/dxg`, not Vulkan on a DRM node.

So the phase-2 scanout work is the part that *is* available to a host like
this: `read_rect_bgra` now reads the dirty rect **straight into the caller's
packed buffer** with an explicit row stride, instead of phase 1's full-frame
shadow plus a row-by-row repack. That removes one CPU copy of every flushed
rect and a 7.9 MiB (1080p) host allocation from the present path. It also
**fixes a latent phase-1 bug**: `virgl_renderer_transfer_read_iov` writes the
box's rows *packed* at the given offset, so phase 1's full-frame-strided
repack produced wrong pixels for any rect narrower than the resource — a
partial-rect test against the real library now pins this down.

The seam for a host that *does* have a DRM node is in place and feature-detected
at runtime, never at compile time: `Renderer3d::export_scanout` returns
`Option<ScanoutExport>` (dmabuf fd, stride, fourcc, modifier) and the default is
`None`. Phase 3 fills in both ends — the EGL export in `virgl.rs` and a
`wgpu_hal` import in `display` — behind that same probe.

### Frame pacing is measured by the device

A `RESOURCE_FLUSH` on the scanout resource *is* a guest present, which makes the
device the only place in the system that sees every frame. `virtio_gpu::pacing`
turns those intervals into a host-side frame clock (mean/min/max, late frames
past a 20 ms budget, idle gaps excluded) and logs it every 120 frames next to
the fence statistics **and the device's own service time** — how long the flush
path itself took. The interval alone cannot attribute a slow frame; the pair
can. No guest agent, no instrumented mutter, and the numbers are comparable
across a 2D device, a synchronous-fence virgl device and an asynchronous-fence
one.

### Measured: the Ubuntu 26.04 Desktop live session, 1920×1080, 4 vCPUs

Five headless runs on WSLg (D3D12 / AMD Radeon PRO), ~4 minutes each, GNOME
compositing throughout; each figure is the mean of the per-120-frame windows
(3 000–3 700 frames per run).

| run | fences | renderer | frame interval | fps | worst window | device service time |
|---|---|---|---:|---:|---:|---:|
| A | synchronous (phase 1) | in-process | 33.6 ms | 29.8 | 36.8 ms (98.7 ms max) | — |
| B | deferred (phase 2) | in-process | 33.5 ms | 29.9 | 34.5 ms (51.6 ms max) | — |
| C | deferred (phase 2) | **isolated process** | 33.4 ms | 29.9 | 34.6 ms (51.3 ms max) | — |
| D | deferred | in-process | 33.4 ms | 29.9 | — | **1.9 ms** (max 5–8 ms) |
| E | — (2D device) | none | 33.5 ms | 29.9 | — | **1.8 ms** (max 3–5 ms) |

Boot to the first scanout flip: 36–41 s, run-to-run noise larger than the
difference between modes. Three conclusions, and the third is the useful one:

1. **Process isolation is free on this workload.** 33.4 ms isolated versus
   33.5 ms in-process — below the run-to-run spread. The extra copy per flush
   and per transfer does not show up against a 33 ms frame, which is what
   justifies making isolation the default rather than an opt-in.
2. **Real fences change nothing here, and that is not a bug — it is what the
   guest asks for.** `fence_deferred` is **0** across every run: the only
   command GNOME-on-virgl fences is `RESOURCE_FLUSH` (the kernel's
   `virtio_gpu_primary_plane_update` attaches the plane's out-fence there),
   and mesa's virgl driver does not request out-fences on its submits in this
   session. `RESOURCE_FLUSH` is deliberately *not* deferred: its readback is
   synchronous, so the work really is finished when the response is written,
   and holding it would add latency for nothing. The tail did improve
   (worst-window 36.8 → 34.5 ms, worst single interval 98.7 → 51.6 ms), which
   is the deferral machinery not being in the way rather than it being used.
   The deferral path is exercised instead by `gpu_fence.rs` and by a real host
   fence in `virgl_fence_host.rs`.
3. **The host presentation path is 6 % of a frame.** 1.9 ms of 33.4 ms, and a
   2D device with no GPU at all measures the same 1.8 ms / 33.5 ms — so the
   readback is *not* what caps this guest at 30 fps, and neither is virgl. That
   retires the urgency of dmabuf zero-copy on this host (which, per the probe
   above, is not implementable here anyway) and points the next investigation
   at the guest: a compositor that lands on exactly half of the EDID's 60 Hz
   is missing a deadline of its own, not ours.

Where phase 2's fences *will* matter is phase 3: the moment a scanout flush
stops being a synchronous readback (zero-copy, where the frame is done when the
GPU says so) the flush fence becomes the right thing to defer — and the
machinery for it is now in place and tested.

## Amendment (2026-08-20): the id namespace stays mixed in virgl mode

Phase 1 assumed the guest kernel creates *every* object through
`RESOURCE_CREATE_3D` once `VIRGL` is negotiated. The boot log says otherwise:
Ubuntu 26.04's `virtio_gpu` still creates its own dumb/console framebuffer with
**`RESOURCE_CREATE_2D`** (observed: resource 2, `B8G8R8X8_UNORM`, 1920×1080,
right before `fb0: virtio_gpudrmfb frame buffer device`), and then —
`virtio_gpu_gem_object_open`/`_close` — **attaches it to the DRM client's 3D
context and detaches it again**. So on a virgl device both halves of the one id
namespace are live at the same time.

Consequences, all of them "route by which table owns the id" (the rule the rest
of the device already followed):

- `CTX_ATTACH_RESOURCE` / `CTX_DETACH_RESOURCE` must accept a **2D-owned** id.
  QEMU and crosvm accept it because they keep a single table — QEMU's virgl path
  even re-creates 2D resources inside virglrenderer
  (`virgl_cmd_create_resource_2d`: target 2, depth 1, array 1,
  `BIND_RENDER_TARGET`). We keep the 2D table as it is and answer the attach
  after validating the context and the id, without calling the renderer: it has
  no handle for that resource, and the guest never names a 2D resource inside a
  `SUBMIT_3D` stream — it reaches it through `TRANSFER_TO_HOST_2D` /
  `SET_SCANOUT` / `RESOURCE_FLUSH`, which route by ownership too.
- Refusing it (which phase 1 did, with `ERR_INVALID_RESOURCE_ID`) is *visible*:
  the guest's `virtio_gpu_dequeue_ctrl_func` logs
  `*ERROR* response 0x1203 (command 0x202)` and again for `0x203` on every
  virgl boot. Nothing else broke — the console framebuffer works because it
  never needed the renderer — but a spec-faithful device answers OK, and
  `boot-tests/virgl_gnome.rs` now asserts the pair is absent.

## Amendment (2026-08-19): what the implementation found

Phase 1 landed the same day; three FFI facts worth keeping:

1. **virglrenderer stores the pointers it is given at init.** Both the cookie
   and the `virgl_renderer_callbacks` struct are retained by address (QEMU
   keeps a `static` for the same reason). A stack-local callbacks struct
   SIGSEGVs later; ours are boxed inside `VirglRenderer` and leaked on drop,
   because of the next point.
2. **Never `virgl_renderer_cleanup`, never dlclose.** Cleanup terminates EGL,
   which unloads mesa's driver while mesa worker threads still hold TLS
   destructors — an observed SIGSEGV in `__nptl_deallocate_tsd` at thread
   exit under WSLg's d3d12 driver. The library handle is `ManuallyDrop`, the
   initialized state lives for the process (exactly what QEMU/crosvm do), and
   one process gets at most one initialized renderer, ever.
3. **Thread affinity is real but manageable.** EGL contexts bind to the
   calling thread, so `virgl_renderer_init` runs lazily on the device worker
   thread's first command, and a guest device reset (which arrives on a vCPU
   thread as an MMIO status write) only *marks* the renderer; the actual
   `virgl_renderer_reset` runs before the next worker-thread command, with
   freed iovec arrays parked in a graveyard until then.

Measured in WSL (D3D12 / AMD Radeon PRO): init reports GL 4.2 core, capsets
VIRGL v1 (308 B) and VIRGL2 v2 (696 B); the `virgl_host.rs` integration test
round-trips guest pages → iovec → GL texture → BGRA readback in ~0.5 s
including EGL bring-up. The `gpu_3d_commands` fuzz target ran clean
(4.65M executions).

**Guest acceptance** (`boot-tests/virgl_gnome.rs`): the Ubuntu 26.04 Desktop
live session negotiates `+virgl`, reads both capsets, reaches
graphical.target in 67–98 s (llvmpipe needed several minutes), and an EGL
probe typed into a `systemd.debug_shell` reports `GL_RENDERER = virgl` from
inside the guest.

**Known limitation of the WSLg host GL (not of this design):** after 1–3
minutes of sustained GNOME compositing on the D3D12-backed host GL, a submit
dereferences a NULL gallium hook inside jammy's mesa 23.2 megadriver
(backtraced: `virgl_renderer_submit_cmd` → vrend → `swrast_dri.so` → call to
0x0) and takes the process down. Our validation front is not implicated —
the stream is structurally valid GL work the host driver mishandles. On this
host, `LIBGL_ALWAYS_SOFTWARE=1` moves the renderer onto host llvmpipe, which
is stable (still reported as `virgl` in the guest, still off the guest's
CPUs' critical path, but no GPU win). Native Linux hosts with real DRI
drivers do not share this failure mode; renderer-crash *containment*
(GPU-012) is phase-2 work either way.

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

## Amendment, 2026-08-20 — Venus is the Windows answer, and it is an epic

Phase 1 rejected Venus because it needed blob resources and a shared-memory
region the transport does not have, plus a host Vulkan ICD that WSL's dzn was
not. That reasoning stands for *phase 1*; it is not a verdict on Windows.

Recorded now because the question came up as "can WHP do 3D at all": yes, and
WHP has nothing to do with it. The hypervisor virtualises CPU and memory. What
blocks 3D on Windows is the host renderer — `virglrenderer` speaks EGL, which
Windows does not provide. Everything on our side of that line is already
portable: the command decoder, the `Renderer3d` trait and phase 2's
out-of-process renderer protocol all build and test on Windows.

Three ways to fill the gap, all licence-clean:

1. **virglrenderer on ANGLE** (BSD) — the short path, no changes to our code,
   prior art in crosvm; but it stacks a GL→D3D translation under our VIRGL→GL
   translation, and requires building a Linux-centric C library for Windows.
2. **Venus** — the guest's Mesa venus driver against the host's native Vulkan.
   First-class on Windows, and on Linux hosts with a real DRM node; one
   implementation serves both hosts, which is ADR-0002's rule applied to
   graphics. Cost: `VIRTIO_GPU_F_RESOURCE_BLOB` and a shared-memory region on
   both transports.
3. **gfxstream** (Apache-2.0) — supports Windows, does GLES and Vulkan; slots
   behind the same trait if Venus proves harder than expected.

Decision: **Venus is the target** (backlog EPIC 20), ANGLE stays as the
fallback if blob resources turn out to be the wrong hill. Either way the C
library is loaded at runtime, never linked, exactly as phase 1 established.

## Amendment (2026-08-21): EPIC 20 phase 1 — blob resources and the shared-memory window

Phase 1 rejected Venus for two concrete missing pieces (option (b) above):
blob resources, and a shared-memory region the transports did not have. This
amendment records what those cost, what they look like on each transport, and
**exactly** where Venus stands when this phase stops.

### What a blob is, and why the device has to be *less* clever

A blob resource has no format, no geometry and no pixels — a size, a memory
type, and (for two of the three types) a page list. That is not an omission in
the spec: it is the point. Venus does not want a device that understands
textures, because the thing being moved is a serialized Vulkan command stream
and a `VkDeviceMemory` allocation. Everything the 2D and 3D halves do — bound a
rect against a resource's width, size a transfer against a mip level — has no
counterpart here.

So `virtio_gpu::blob` bounds the only things it *can* bound, and refuses
everything ambiguous:

| guest value | rule |
|---|---|
| `blob_mem` | must be one of the three, **and** one the renderer declared |
| `blob_flags` | unknown bits refused, not masked; `USE_CROSS_DEVICE` refused outright (no dmabuf on either host) |
| `size` | non-zero, a whole number of 4 KiB pages, ≤ `MAX_BLOB_BYTES` (1 GiB), and the sum ≤ `MAX_TOTAL_BLOB_BYTES` (4 GiB) |
| `nr_entries` | ≤ `MAX_BLOB_ENTRIES` (16384), and the command must actually carry that many (the multiplication is checked) |
| page list | required for `GUEST`/`HOST3D_GUEST` and must cover `size`; **forbidden** for `HOST3D` |
| `RESOURCE_MAP_BLOB` offset | page-aligned, `offset + size` inside the window in u64, and disjoint from every live mapping |

The last row is the one that matters. It is the first time in this device that
a guest names an offset into a **host** mapping. `HostVisibleWindow` is
therefore a validator, not an allocator — placement is the driver's business
(Linux' `virtio_gpu` carves the region with its own `drm_mm`) and the device's
job is to refuse anything that would let one guest mapping alias another's host
memory. The check is a two-neighbour test in a `BTreeMap`, and the fuzz target
re-derives the no-overlap invariant from *outside* after every operation, so a
bug in that test cannot hide behind itself.

One deliberate asymmetry: **a guest-memory blob never reaches the renderer.**
Its bytes are guest pages the device already tracks, and handing them to a
`dlopen`ed C library buys nothing and widens the trust boundary. Only the two
host3d types cross the `Renderer3d` seam, and they cross it by `blob_id` — so
the isolated helper (GPU-012) still has no window onto guest RAM, unchanged.

The blob table is a **third owner in the one id namespace**, which is the rule
the mixed-namespace amendment above already established: route by which table
owns the id. Attach/detach-backing and `TRANSFER_*_3D` are refused for a blob
(a blob's pages are fixed at create time and it has no geometry to transfer
against); `CTX_ATTACH_RESOURCE` is accepted, because Venus attaches its ring
blob to its context exactly as the kernel attaches its 2D console framebuffer;
`SET_SCANOUT` is refused in favour of the blob variant; and `RESOURCE_UNREF`
releases a window span the guest forgot to unmap, because a guest is allowed to
forget and the host must not leak it.

`SET_SCANOUT_BLOB` is implemented for a guest-memory blob with a single-plane
BGRA layout: the flush gathers rows out of the guest pages through the same
checked `vm-memory` path everything else uses, at the stride the guest
declared. A **host** blob is refused, because presenting one needs the
zero-copy export this host does not have (the dmabuf probe in the phase-2
amendment above) and a black screen is a worse answer than an error.

### The shared-memory region, per transport

| | virtio-mmio | virtio-pci |
|---|---|---|
| how the driver finds it | `SHM_SEL` selects by `shmid`, then `SHM_LEN_LOW/HIGH` + `SHM_BASE_LOW/HIGH` | `VIRTIO_PCI_CAP_SHARED_MEMORY_CFG` (`cfg_type` 8) as a `virtio_pci_cap64` |
| record | four 32-bit registers, spec 4.2.2 | 24 bytes; offset and length split across two field pairs, spec 4.1.4.7 |
| where the window lives | a guest-physical range the machine layer picks | **BAR 2**, a window of its own |
| a region that does not exist | reads all-ones (unchanged) | no capability published at all (unchanged) |

Two decisions worth their own sentences.

**The all-ones answer is load-bearing, and was preserved byte for byte.** A read
of `SHM_LEN` for a region that does not exist must return all-ones; returning
zero makes Linux' `virtio_gpu` see a *present*, zero-length region at address 0,
try to reserve it, and fail its probe. That was a real bug fix once, and it now
has a test of its own (`a_device_without_shm_regions_still_reads_all_ones`)
standing next to the new behaviour, so the two cannot drift apart. A
zero-length region is refused at construction for the same reason.

**On PCI it has to be a second BAR.** BAR 0 is 32 KiB, 32-bit, and sized to the
register file; the "why one BAR and not two" argument in `pci.rs` is about the
*register* structures and still holds. A host-visible blob window is hundreds of
megabytes and wants to be 64-bit prefetchable, so it cannot share — and
`virtio_pci_cap64` exists precisely because such a region's length does not fit
a 32-bit field.

**And the window is declared but not yet backed.** A region is reported to the
guest only when the device declares it *and* the machine layer has said where it
landed (`set_shm_base`). Nothing calls that yet, because allocating the second
BAR — a 64-bit prefetchable aperture, a KVM memory slot / `WHvMapGpaRange` for
the host pages behind it, the DSDT `_CRS` that publishes it, and the BAR-rebase
machinery EDK2 forces — is `machine-x86` work, and that crate was another
agent's this session. Until it exists, `RESOURCE_MAP_BLOB` is refused in band
with a message saying exactly that. This ordering is the honest one: an
unbacked window advertised to a driver is a guest that faults on its first
`mmap`, which is strictly worse than a device that says "not here".

Consequently every feature bit follows capability rather than hope.
`VIRTIO_GPU_F_RESOURCE_BLOB` and `VIRTIO_GPU_F_CONTEXT_INIT` are offered only
when the attached renderer's `BlobSupport` and capsets justify them, and a
device with no region produces byte-identical registers and byte-identical PCI
configuration space to the one it produced before any of this existed.

### Where Venus actually stands

Done, and tested on both hosts with no GPU at all:

- `VIRTIO_GPU_F_RESOURCE_BLOB`: `RESOURCE_CREATE_BLOB` for all three memory
  types, `RESOURCE_MAP_BLOB` / `RESOURCE_UNMAP_BLOB` against the window,
  `SET_SCANOUT_BLOB`, and every bound in the table above;
- `VIRTIO_GPU_F_CONTEXT_INIT` and `VIRTIO_GPU_CAPSET_VENUS`. `context_init`'s
  low byte is a capset id naming the context *type*, honoured only for a capset
  the renderer actually advertises — otherwise the guest would get a context
  whose encoding nothing on the host can decode;
- the shared-memory region on both transports;
- `NullRenderer::with_venus()`, a portable loopback that declares the venus
  capset and all three blob types and validates real window offsets, so the
  whole path is exercisable — and fuzzable — on Windows;
- the isolated-renderer protocol carries all of it (version 2): `CtxCreate` grew
  a capset id, and there are `CreateBlob` / `DestroyBlob` / `MapBlob` /
  `UnmapBlob` / `BlobSupport` messages.

Not done, stated plainly: **Venus renders nothing.** No host in this project
decodes a Vulkan command stream yet.

### The host probe, 2026-08-21, and what it means

Measured in the WSL Ubuntu 22.04 that runs all KVM work on this machine:

- `/dev/dri` still does not exist; `/dev/dxg` does. No DRM render node.
- `libvirglrenderer1` is **0.9.1** (jammy). The four Venus-era entry points —
  `virgl_renderer_context_create_with_flags`, `..._resource_create_blob`,
  `..._resource_map`, `..._resource_unmap` — do not resolve, and
  `virgl_renderer_get_cap_set(VIRTIO_GPU_CAPSET_VENUS)` reports **0 bytes**.
  Recorded by a test rather than by a note: `virgl_host.rs` prints
  `capsets=[VIRGL v1 308 B, VIRGL2 v2 696 B]`, `blob_support={guest: false,
  host3d: false, host_visible_bytes: None}`, and asserts those two answers stay
  consistent with one another.
- A host Vulkan ICD **does** exist, and it is not the one phase 1 assumed. Not
  dzn: **lavapipe** (`/usr/share/vulkan/icd.d/lvp_icd.x86_64.json`).
  `vulkaninfo --summary` enumerates exactly one device — `llvmpipe (LLVM
  15.0.7)`, `PHYSICAL_DEVICE_TYPE_CPU`, Vulkan 1.3, Mesa 23.2.1, conformance
  1.3.1.1. The radeon and intel ICDs are installed and find nothing, because
  there is no DRM node for them to open.
- Building virglrenderer ≥ 0.10 from source *here* is blocked by the
  environment, not by the plan: `meson`, `ninja`, `libepoxy-dev` and the Vulkan
  headers are all absent, and **`sudo` requires a password**, so no agent can
  install them.

That changes what "get Venus working" would mean on this machine, so it is
worth saying out loud: even with a self-built virglrenderer, the host Vulkan
device would be **llvmpipe on the CPU**, behind a Venus decode, inside a VM —
software Vulkan under a translation layer. That can prove *correctness* (a
guest `vulkaninfo` enumerating a device is a real result) but it cannot produce
a number worth putting beside the phase-2 frame table, and it would be no
evidence at all for the claim that motivated the epic: that Venus is the right
answer on Windows. A native Linux host with a DRM node, or the Windows/WHP host
against its native Vulkan ICD, is where that measurement belongs.

What was built for it anyway, because being ready costs nothing: the
`VirglRenderer` wrapper **probes for the Venus entry points at `dlopen` time**
and advertises the venus capset if and only if all four resolve *and* the
library reports a non-zero capset size. Drop a newer `libvirglrenderer.so.1` on
a host and Venus lights up with no rebuild — the same runtime-detection
discipline this ADR established for the library itself, applied one level down.
`ENTANGLED_GPU_VENUS=off` suppresses the probe for a host whose newer library
has Venus compiled in but whose Vulkan ICD nobody should be rendering against.

### Next agent starts here

1. ~~**`machine-x86`: back the window.**~~ Done, 2026-09-09 — see the phase-2
   amendment below.
2. **A real Venus renderer (VEN-2003).** Build virglrenderer ≥ 1.0 with
   `-Dvenus=true` on a host where `apt` is available, then fill in
   `VirglRenderer::map_blob` with `virgl_renderer_resource_map` against the
   window from step 1, and add `VIRGL_RENDERER_VENUS` (very likely with
   `USE_EXTERNAL_BLOB`) to the init flags. The typed-context and create-blob FFI
   halves are already written and behind the runtime probe.
3. ~~**VEN-2004 (Windows isolation).**~~ Done, 2026-09-08:
   `remote::pipe_windows::DuplexPipe` is the handle pair (a duplex named pipe,
   installed as the child's stdin exactly as the socketpair end is), and
   `gpu_remote.rs` runs its whole containment proof on both hosts. What Windows
   still lacks is a *renderer* to isolate, not the isolation.
4. **Guest acceptance (VEN-2006)** is only meaningful after 1 and 2, and only on
   a host with a real Vulkan device. `vulkaninfo` inside the guest is the first
   milestone, `vkcube` the second — the same shape as `GLPROBE_RENDERER=virgl`
   was for phase 1.
5. The GUI's capability gate (`Backend::virgl_block`) does not know about any of
   this. Nothing changes for the user today — blob resources are offered only
   when a renderer declares them, and none does on this host — but when step 2
   lands, the manager wants a "3D: Venus / VirGL / none" line rather than a
   boolean.

## Amendment (2026-09-09): EPIC 20 phase 2 — the shared-memory window is real

Phase 1 built everything about a shared-memory region except the part that
needs host memory: a device could declare an `ShmRegion`, both transports could
answer for one, and `virtio_gpu::blob` could validate a mapping inside one — but
nothing allocated or mapped the window, so `RESOURCE_MAP_BLOB` could not succeed
anywhere and the region was never published to a guest at all. This amendment
records what filling that in cost, and the two decisions inside it that were
measured rather than reasoned about.

### Where the window goes, and why that is not a matter of taste

Four things are already above 4 GiB or bound it from below, and a fifth is
chosen by the firmware:

| what | where | why it constrains us |
|---|---|---|
| the 32-bit MMIO hole | `0xc000_0000..0x1_0000_0000` | PCI BARs, virtio-mmio slots, LAPIC, IOAPIC |
| the pflash window | `0xffc0_0000..0x1_0000_0000` | ADR-0003; the UEFI variable store |
| **high RAM** | `0x1_0000_0000 ..` **and it grows** | a guest bigger than 3 GiB gets its remainder at 4 GiB, so *any fixed constant above 4 GiB is a constant that works until someone boots a bigger VM* |
| `Pci64Base` | chosen by EDK2 | `PciBusDxe` reassigns **every** BAR during enumeration, and a 64-bit prefetchable one lands in the firmware's own 64-bit aperture |

The third row is the trap a sibling agent paid for in `fix/uefi-high-ram`, and
the fourth is the one that decides the answer. So the fourth was measured rather
than assumed. `tests/boot/tests/uefi_highmem.rs` on this project's pinned
CloudHv build, 4096 MiB guest:

```text
PlatformGetFirstNonAddressCB: FirstNonAddress=0x140000000
AddressWidthInitialization: Pci64Base=0x140000000 Pci64Size=0x3FFEC0000000
PlatformAddHobCB: HighMemory [0x100000000, 0x140000000)
```

`Pci64Base` is **exactly** the top of RAM — no page of slack, whatever the
vm-testing skill's prose said — and the aperture runs from there to 2^46, the
guest's physical address width.

Decision: **our aperture is the firmware's.** `layout::pci_mmio64_base(mem)`
returns the same number EDK2 computes — `TOP_OF_32BIT + (mem - MMIO_HOLE_START)`
for a guest above the hole, `TOP_OF_32BIT` for one below it — and the aperture
is 16 GiB long from there. The host's initial BAR assignment, the firmware's
reassignment and the DSDT `_CRS` then all describe one range, and a BAR that
moves during enumeration moves *inside* something the host already decodes.

The alternative, a fixed high address, was rejected for a concrete reason
rather than a stylistic one: EDK2 allocates from `Pci64Base` upwards, and
`Pci64Base` follows RAM, so the firmware would move the BAR straight back out
of any fixed window we picked. Arithmetic, for the two sizes the tests cover:

* 2048 MiB — no high RAM at all, top of address space 4 GiB, window at
  `0x1_0000_0000`, aperture to `0x5_0000_0000`;
* 4096 MiB — high RAM `0x1_0000_0000..0x1_4000_0000`, window at
  `0x1_4000_0000`, aperture to `0x5_4000_0000`;
* 65536 MiB (the config maximum) — window at `0x10_4000_0000`, aperture ending
  at `0x14_4000_0000`, comfortably inside 2^46.

The length is 16 GiB rather than a smaller round number because it is *derived*
from what it has to hold: `PCI_MMIO_SLOTS` functions × `MAX_SHM_BAR_BYTES`,
plus one more window's worth of gap that natural alignment can push the first
allocation by — nine gigabytes, rounded up to the next power of two and held
there by a `const` assertion. It landed at 4 GiB first, under a comment making
exactly that claim, which eight 1 GiB windows do not fit into. One device
declares a region today, so nothing hit it.

Ours is also **not** as long as the firmware's, and that asymmetry is worth
naming: EDK2 publishes `Pci64Size=0x3FFEC0000000` and `PciBusDxe` allocates out
of *that*, not out of the DSDT `_CRS`. If it ever placed a window past
`pci_mmio64_end`, `ShmWindow::follow` would refuse it and the guest would get no
`resource2` — quietly. The sizing rule above is what keeps that hypothetical.

BARs are allocated out of the aperture by a bump allocator rather than from a
slot table (`layout::Mmio64Allocator`), because a memory BAR must be aligned to
*its own* size and a window's size is the device's business. A 3073 MiB guest —
top of RAM at 4 GiB + 1 MiB, page aligned and nothing else — is the case a slot
table gets wrong, and it has a test.

### Prefetchable is not decoration

The BAR is 64-bit **and prefetchable**, and both halves are load-bearing at a
different layer:

* EDK2's `PciBusDxe` puts a *non*-prefetchable 64-bit BAR in the **32-bit**
  aperture whenever it fits there. For a 256 MiB window it does not fit, and
  enumeration fails;
* Linux' `pci_find_parent_resource` refuses to claim a prefetchable BAR inside
  a host-bridge window that is not itself prefetchable — it skips the window and
  reassigns or gives up. So the DSDT's `QWordMemory` producer descriptor carries
  caching type 3 (cacheable prefetchable), not the plain read/write the 32-bit
  `DWordMemory` window uses.

Neither of those is visible in a unit test, and both are the difference between
a guest with a `resource2` and a guest without one.

### Who owns the pages, and how they are freed

`vmm_core::shm::HostShmRegion` owns one anonymous host allocation through
`vm_memory::MmapRegion` — `mmap` on Linux, `VirtualAlloc` on Windows — so there
is no new `unsafe` in the allocation and no second guest-memory type.
`SharedWindow` pairs it with the hypervisor mapping and owns both: it holds at
most one live placement, moving means unmap-then-map, and `Drop` unmaps
**before** the pages are released. The reverse order would leave a hypervisor
slot pointing at memory this process had returned to the allocator, which is the
worst bug this module could have.

The hypervisor half is a four-line trait, `GpaMapper`, so `machine-x86` never
names a `kvm_bindings` or `WHV_*` type (ADR-0002):

| | KVM | WHP |
|---|---|---|
| map | `KVM_SET_USER_MEMORY_REGION` on a slot reserved for the life of the VM | `WHvMapGpaRange` |
| move | the same call on the same slot number, which replaces it | `WHvUnmapGpaRange` then `WHvMapGpaRange` |
| unmap | the same call with `memory_size = 0` | `WHvUnmapGpaRange` |
| execute | **no per-slot control** — the guest's own page tables decide, as for guest RAM | `Read \| Write`, so an instruction fetch faults |

The last row is a real asymmetry and is recorded where it is true rather than
in a note somewhere else. It is not a security difference that matters — the
pages are the guest's own writable memory either way — but a reader comparing
the two backends deserves to be told.

`vmm_core::shm::UnmappedGpaMapper` is the third implementation: real host pages,
no guest behind them. It is what makes the whole placement path unit-testable on
a machine with no hypervisor, which is why `crates/machine-x86/tests/shm_bus.rs`
runs on both hosts in a second rather than only where `/dev/kvm` exists.

### The guest chooses the address, so the machine gets a veto

A BAR is guest-writable. A guest can therefore point a 256 MiB prefetchable
window at its own page tables, at the LAPIC, at the pflash window or at another
device — and KVM and WHP would both map it there if asked. So every placement
goes through `machine_x86::shm::ShmWindow` — `follow`, or its two halves
`release_for` and `claim` where a sweep needs them apart — which maps only
inside the aperture published in the DSDT and otherwise leaves the window
**unmapped**.
Because the aperture starts at the top of RAM, one containment test rules out
every collision that matters at once.

"Decodes nothing" always means *unmap*, never "leave it and lose something".
That is the difference from `crate::notify`'s ioeventfd sweep, which this one
otherwise copies: a stale ioeventfd costs a kick, a stale memory mapping is host
pages sitting where the guest has put something else. The sweep therefore runs
on **both** hosts, unlike the ioeventfd one, and a machine reset takes every
window down (ADR-0005).

What it copies exactly, and had to be corrected to copy, is the **two passes**:
release every window whose BAR moved, then claim them all. `PciBusDxe` hands out
addresses in reverse device order, so a permutation of two functions' windows
asks the host to map A where B still is — and both hypervisors refuse an
overlapping range, KVM as an overlapping memory slot and WHP as a failed
`WHvMapGpaRange`. A one-pass sweep leaves A unmapped with nothing to schedule
the retry: a stale ioeventfd is picked up by the next sweep, but an unmapped
window is not, and the guest's `mmap` of the region reads nothing, silently.
`shm_bus::two_windows_that_swap_addresses_both_end_up_mapped` is the regression
test, with a `GpaMapper` that refuses overlaps the way a real one does; it fails
against the one-pass version. Latent today — one device declares a region — and
cheap to get right before a second does.

### What the device does with it

`virtio_core::ShmBacking` is the device's whole view: `len`, `read`, `write`,
`fill`, every offset bounded in `u64`, no pointers and no hypervisor. It arrives
through `VirtioDevice::set_shm_backing`, defaulted to a no-op, and is scoped to
one region's span inside the BAR rather than to the BAR — so a second region can
never be reached through the first one's offsets.

Two rules follow, and both are fuzzed:

* **a span handed to a guest is zeroed.** `BlobTable::reserve_mapping` clears
  before it returns, so it is a property of the table rather than of one caller.
  The pages were last some other blob's, and a guest must not find them;
* **and only that span.** Clearing past either end would wipe a neighbouring
  mapping's bytes under a guest that is using them.

The `gpu_blob` target grew a real `ShmBacking` for this: it scribbles a canary
over the whole window before every operation and reads every live mapping back,
so the property is checked against *memory* rather than against the bookkeeping
that is supposed to maintain it, and it aims arbitrary `(offset, len)` pairs —
wrapping ones included — straight at the backing. Campaign on the tree as
merged: **1 197 656 executions in 603 s** (1986 exec/s, 927 edges, 668-case
corpus), no crash and no invariant violation; an earlier 1 085 637-execution run
before the review fixes was equally clean.

The loopback Venus renderer writes a 32-byte signature at the mapping offset —
magic, resource id, size, `blob_id`. A real Venus renderer will map its own
`VkDeviceMemory` there instead (`Renderer3d::set_host_visible` is the seam), and
until one exists the signature is what makes "the guest reads what the host
wrote" provable on a machine whose only Vulkan ICD is lavapipe. An **isolated**
renderer (GPU-012) takes the default no-op: an `Arc` does not cross a pipe, the
helper never sees the window, and the device's own clear-on-map is what keeps
that case honest.

### The evidence

`tests/boot/tests/pci_shm.rs`, KVM, bootstrap kernel, no virtio-gpu driver
interface involved — the probe goes through sysfs:

```text
VMHOST_TEST_OK shmprobe device=0000:00:01.0 driver=virtio-pci bar=0x100000000
  size=268435456 prefetch=1 sixtyfour=1 wrote=20 magic=HOST-WROTE-THIS-FIRST
```

Both directions, and neither rests on the host's own bookkeeping: the host
stamps a marker into the window before the vCPUs start, the guest `mmap`s
`/sys/bus/pci/devices/0000:00:01.0/resource2` and reads it back, writes a reply
one page in, and the test reads that reply out of the host pages after the guest
has stopped. In between, a real Linux kernel found a BAR nothing told it about
and claimed it inside the DSDT's prefetchable 64-bit window.

`echo 1 > enable` is the step that matters and is easy to miss: it is
`pci_enable_device`, it sets the memory-space bit, and *that* is what makes the
host map the window. `EBUSY` there means the bound driver already did it, which
is better evidence than success.

Two boots, one number apart — 2048 MiB and 4096 MiB — which is the regression
test for the high-RAM trap above.

### Suspend and restore (ADR-0006)

A host-visible mapping cannot survive a restore, and the format already had the
hook for saying so: `TransportSaveState::shm_bases` was written in phase 1 with
the note "empty on every VM today, because nothing in `machine-x86` has an
address to hand out yet". It is not empty any more, on either transport — on PCI
the driver derives the address from the BAR and never reads that field, so it is
recorded purely for the file — and `virtio_core::StateError::ShmBase` is
therefore a live refusal rather than dead code.

The order at restore is the whole trick: configuration space first (which puts
the BAR back), then `reconcile_shm` (which puts the host pages back at that
address), then the transports' own `load`, which compares. A machine that would
place the window somewhere else — a different memory size, above all, since the
aperture follows RAM — refuses by name instead of restoring a guest whose blob
mappings point at nothing. A window the guest had unmapped when the snapshot was
taken records nothing at all, so it does not insist on an address the guest was
not using.

Blob *resources* are unchanged and still counted rather than described: a guest
that had one open when it was suspended is told `DEVICE_NEEDS_RESET`, the same
signal a crashed renderer produces.

### What Venus still lacks

Exactly one thing now, and it is the same one as before: **a renderer**. The
window it was missing exists, is mapped, is bounded and is provable from inside
a guest. Nothing on either host decodes a Vulkan command stream.

*(Superseded on 2026-09-10 by the phase-3 amendment below: a Linux host with a
self-built virglrenderer 1.1 does decode one now. Left as written, because the
sentence dates the moment and the phase-3 section is where the answer is.)*

### Next agent starts here

1. ~~**A real Venus renderer (VEN-2003).**~~ Done, 2026-09-10 — see the
   phase-3 amendment below. Two of the guesses in this item were wrong and are
   worth leaving visible: `USE_EXTERNAL_BLOB` turned out to be exactly the flag
   *not* to set, and `apt` was never the obstacle (the build needed newer
   Vulkan headers than the distribution has, which is a pin rather than a
   package).
2. ~~**Per-blob host mappings.**~~ Done, 2026-09-10. The shape predicted here
   is the shape it took, with one correction: it is a **mode**, not an
   addition. The whole-window mapping and a renderer range inside it overlap,
   and no hypervisor allows that, so a window is one or the other. "Nothing
   above the seam changes" was also not quite right — the renderer declares the
   mode and the device carries it, because only the renderer knows.
3. ~~**Guest acceptance (VEN-2006).**~~ Done, 2026-09-10:
   `tests/boot/tests/venus_vulkan.rs`. `vulkaninfo` was not needed — a
   python-ctypes probe needs nothing on the ISO but `libvulkan.so.1`. `vkcube`
   is still open, and still meaningless for speed on a host whose only ICD is
   lavapipe.
4. **Zero-copy scanout (VEN-2005/GPU phase 3)** is still where the frame rate
   is: GAME-2105 measured the readback at 13 ms of a 20 ms frame. Unrelated to
   this window, and unblocked by nothing in it.
5. The GUI's capability gate (`Backend::virgl_block`) still knows nothing about
   any of this. Step 1 has now landed, so it has something to say — see the
   phase-3 list.
6. **One restore direction is still unguarded**, and closing it needs a format
   field rather than a check. `TransportSaveState::shm_bases` records the bases
   a window *was placed at*, so the dangerous direction is refused: a snapshot
   that names one, restored onto a machine that would place it elsewhere or
   nowhere, is `StateError::ShmBase`. The reverse — a snapshot taken with the
   window unmapped, or on a build with no shm support at all, restored onto a
   machine that has one — is accepted, because an empty `shm_bases` is
   indistinguishable from "the guest had it unmapped", which is legitimate and
   must stay accepted. Telling the two apart means recording the region ids the
   transport *declares* alongside the bases it placed, which is a snapshot
   format change and belongs with the next one, not bolted onto a landing pass.
   Nothing a guest can do reaches it: it needs two different host builds.

## Amendment (2026-09-08): GAME-2105 — why a guest sits at 30 fps, and what it cost

Phase 2 left one number unexplained: five configurations, all landing within
0.2 ms of a 33.4 ms frame interval, with a device that reported spending
1.9 ms of it. This amendment answers that, and it retracts phase 2's
conclusion — the sentence "the readback is *not* what caps this guest at
30 fps" was drawn from a run whose host GL was not the one that ships.

### The instrument first: what a frame interval is made of

The device sees every present (a `RESOURCE_FLUSH` on the scanout resource),
which makes it the only honest frame clock in the system — but an interval on
its own cannot say *whose* millisecond it is. `virtio_gpu::pacing` now cuts
each interval at the two boundaries the control queue can actually see:

```text
 present N-1 answered                                     present N answered
         |---- quiet ----+-------- submit --------+---- service ----|
                    first command of        RESOURCE_FLUSH     readback +
                    frame N                 of frame N         sink push
```

* **quiet** — the guest asked this device for nothing. A compositor waiting on
  a clock of its own spends its frame here.
* **submit** — the guest was feeding the device: transfers, 3D submits, flush.
* **service** — the device's own cost.

The three sum to the interval exactly, which is what turns the attribution
into an argument. Alongside them the report carries the 1 % and 0.1 % lows
(mean of the slowest samples, out of a 100 µs histogram — no sample retention),
and two counters defined against the *advertised* refresh period: `duplicate`
(refresh slots the guest put nothing into) and `dropped` (presents superseded
inside one slot). `entangled run --frame-stats <PATH>` mirrors the whole report
to JSON every 120 frames, so two runs are compared with a diff rather than an
impression.

### The measurement

WSLg (D3D12 / AMD Radeon PRO), an **installed** Ubuntu Desktop guest,
1920×1080, 4 vCPUs, headless, `es2gears` animating in a small window so the
compositor never idles. ~4.3 minutes per run. `window` is one steady
120-frame report; `run` covers everything including boot and idle stretches.
The machine was shared with two other agents' VMs throughout (load average 3
to 12), so absolute numbers carry host noise — every comparison below is
between runs taken back to back.

| # | device | `refresh_hz` | run fps | window fps | window interval | quiet | submit | service | frames /4 min |
|---|---|---:|---:|---:|---:|---:|---:|---:|---:|
| 1 | 2D | 60 | 54.0 | **60.0** | **16.665 ms** | 13.3 | 2.3 | 1.1 | 12 951 |
| 2 | 2D | 120 | 81.5 | **119.9** | **8.341 ms** | 4.4 | 2.4 | 1.5 | 21 223 |
| 3 | virgl, isolated | 60 | 26.0 | **29.7** | **33.70 ms** | 0.1 | 4.8 | **28.8** | 6 232 |
| 4 | virgl, in-process | 60 | 54.5 | 52.2 | 19.16 ms | 2.2 | 7.1 | 9.9 | 14 875 |
| 5 | virgl, isolated, *after the fix below* | 60 | **50.3** | 40.6 | 24.62 ms | 0.6 | 8.4 | **15.7** | **12 468** |
| 6 | virgl, isolated, after, 120 Hz | 120 | 52.9 | 60.4 | 16.55 ms | 0.0 | 5.3 | 11.2 | 13 194 |

(Run means for the phase columns; the run-level service means are 1.3, 1.7,
30.0, 9.2, 13.1 and 12.4 ms in the same order.)

### Mechanism one: the advertised refresh is a ceiling we choose

Runs 1 and 2 are the same guest, the same workload and the same device; only
the number in the EDID differs. The guest presents at **exactly** the
advertised period — 16.665 ms against a 16.667 ms slot, 8.341 ms against
8.333 — with `duplicate` and `dropped` both zero, and it spends the slack
*asleep*: 13.3 ms of quiet at 60 Hz, 4.4 ms at 120 Hz, with the work unchanged
at ~3.5 ms. The compositor is phase-locked to the mode we advertise and will
not present faster than it, whatever the host can do.

That is a mechanism nobody had looked at: **60 Hz was a hard ceiling on the
guest's frame rate, chosen by a constant in `edid.rs`.** Nothing here scans out
a cable, so the honest thing is to make it a policy: `[display] refresh_hz`
(24..=240, default 60). The default stays at what a physical monitor would
report — a guest is entitled to be told something plausible, and 60 Hz is what
every other VMM says — but a profile that wants a finer quantum can now have
one, and the pacing counters follow the same value so the report stays honest.

### Mechanism two: past the deadline the guest does not halve, it runs flat out

The tempting story — "the guest misses a vblank and takes the next slot,
hence exactly half" — is **wrong**, and runs 3 to 6 refute it. When the frame's
work does not fit in a period the guest stops sleeping (quiet 0.1 ms in run 3,
0.0 in run 6) and presents as fast as it finishes. The cadence is then the
work, not a fraction of the refresh: 33.70, 19.16, 24.62, 16.55 ms — none of
them a multiple of anything. And raising the advertised refresh does nothing
for such a guest: runs 5 and 6 differ by 5 % on a doubled quantum.

So phase 2's "exactly half of the EDID's 60 Hz" was a coincidence of cost, not
a mechanism. Run 3 reproduces it — 29.7 fps, 33.70 ms — and the decomposition
says where every millisecond went: **28.8 ms of it, 85 % of the frame, was the
device's own scanout readback**.

### What phase 2 measured, and why it read 1.9 ms

Phase 2 recorded the host presentation path at 1.9 ms, 6 % of the frame, and
concluded the readback was not the problem. `virgl_readback_host.rs` — a test
binary that times a bare full-screen readback with no guest and no VM — shows
why that cannot have been the shipping path:

| host GL | 1920×1080 readback | 960×540 |
|---|---:|---:|
| WSLg D3D12 (AMD Radeon PRO), release | **16.8 ms** median (14.7 min) | 8.6 ms |
| llvmpipe (`LIBGL_ALWAYS_SOFTWARE=1`), release | 4.8 ms | 0.9 ms |
| WSLg D3D12, **debug build** | 351.7 ms | 88.5 ms |

A 1.9 ms mean is not reachable on the D3D12 path at any rect size worth
flushing; it is a host-llvmpipe run with small damage. That is a plausible
thing for those runs to have been — this host's D3D12 mesa segfaults after
1–3 minutes of compositing, and `LIBGL_ALWAYS_SOFTWARE=1` is the documented way
to survive a four-minute measurement. The lesson is procedural and worth more
than the number: **a performance figure has to record which renderer produced
it**, or the next reader draws exactly the wrong conclusion from it. (The third
row is the other half of that rule: `read_rect_bgra` is 20× slower unoptimised,
so a debug-build measurement of this path means nothing at all.) The
`virgl_readback_host` timings are printed, never asserted — a shared developer
machine under another agent's build would fail a threshold for reasons that
have nothing to do with this code.

There is no partial-rect saving hiding here either: `pixels_mean` is 2 073 600
in *every* run, 2D and 3D alike. The guest double-buffers, so the framebuffer
changes every frame, so `drm_atomic_helper_damage_merged` widens the damage to
the whole plane. Every present is a full-screen flush by construction.

### The fix: the isolated readback was paying for four copies of the screen

Run 3 against run 4 says process isolation cost **20.8 ms per frame** — not the
"free" of phase 2's table, which was measured when a flush moved 1.9 ms of
work. At 1080p the reply is 7.9 MiB, once per present, and the old path spent
it like this: the helper allocated a fresh `Vec` for the pixels, `Reply::encode`
copied it into a second, `frame()` copied *that* into a third, the client
`memset` its receive buffer before reading, and `Reply::decode` copied the
payload out into a fourth `Vec` that replaced the caller's reusable one. Four
allocations of ~8 MiB, three copies and a `memset`, every frame.

None of that is inherent to the isolation. `write_bytes_reply` now puts the
header and the pixels on the wire straight out of the renderer's own reusable
buffer, and the client reads the payload directly into the caller's buffer via
`read_frame_header` + `read_payload`, which reuse an allocation rather than
clearing it. Same bytes on the wire — a unit test asserts the two framings are
byte-identical — and in the steady state no allocation, no `memset` and no
second copy at all.

Measured, run 3 → run 5, back to back on the same guest:

* device service time **30.0 ms → 13.1 ms** (run means)
* frames delivered in one 4-minute run **6 232 → 12 468** (exactly double)
* run frame rate **26.0 → 50.3 fps**
* the isolation premium over in-process **20.8 ms → 3.9 ms**

— and run 5 carried a *higher* host load than run 3 (average 11 against 3), so
the figure is if anything conservative. Process isolation is nearly free again,
which is what lets it stay the default: the answer to GPU-012 does not have to
cost half the frame rate.

### What is left

* **The readback itself is now the cap** — 13 ms of a 20 ms frame on this host.
  That is phase 3's zero-copy scanout, which this host cannot do: no
  `/dev/dri`, no dmabuf export extension (the phase-2 probe above). A host with
  a DRM node is where that number moves next.
* **`refresh_hz` helps only a guest with slack**, which today means a 2D or
  cheap-workload guest. Once the readback is cheap enough that a 3D guest has
  slack again, the 60 Hz ceiling becomes the binding constraint for it too.
* The phase-2 table is left in place above rather than rewritten: it is what
  was measured, and the interesting part of this amendment is *why* it read the
  way it did.

## Amendment (2026-09-10): EPIC 20 phase 3 — Venus renders

Phase 2 ended with one sentence: "What Venus still lacks is exactly one thing,
and it is the same one as before: **a renderer**." This amendment is that
renderer, what it cost, and — the part that matters more than the code — what
this host can and cannot be used to claim about it.

### The library, and why it had to be built

`libvirglrenderer1` on jammy is **0.9.1**, which predates blob resources and
Venus entirely. Every `dlopen`-time probe this project has added since EPIC 20
phase 1 came back negative against it, correctly.

`guest/virglrenderer/build-virglrenderer.sh` builds the library instead, the
way `guest/firmware/build-cloudhv.sh` builds the firmware: pinned tag, pinned
commit, verified after the clone, installed into
`~/.cache/entangled-virglrenderer/<tag>/` (shared machine state, never inside a
worktree). It is **not** linked — `ENTANGLED_VIRGL_LIB` names it and
`VirglRenderer::load` `dlopen`s it, so `cargo build --workspace` stays
header-free on both hosts and `cargo deny` still governs only the Cargo graph.

- **virglrenderer `virglrenderer-1.1.0`**, commit
  `1aeaf5e10a9c89096e96d09599aa419d5c50712f`, MIT.
- `meson --buildtype release -Dvenus=true -Dplatforms=egl`.
- **Vulkan-Headers `v1.3.269`**, commit
  `374f9fd97520f6dd1b80745de09208d878ab4a52`, Apache-2.0, headers only.
  Needed because the bundled `venus-protocol` headers are generated against
  `VK_HEADER_VERSION 269` while jammy's `libvulkan-dev` is 1.3.204: the build
  dies on `StdVideoH264LevelIdc`, which lives in the newer `vk_video/` headers.
  The Vulkan *loader* stays the distribution's.
- Runtime dependencies of the result, verified with `readelf -d`: `libm`,
  `libepoxy.so.0` (MIT), `libdrm.so.2` (MIT), `libgbm.so.1` (Mesa, MIT),
  `libvulkan.so.1` (Apache-2.0 loader), `libc`. All permissive.
- The script asserts the four Venus entry points are exported rather than
  trusting `-Dvenus=true`, because a library without them degrades to classic
  virgl *silently*, which is the failure mode hardest to notice.

### Three things the library taught us, all load-bearing

**1. Venus in 1.1 exists only behind the render server.** `VIRGL_RENDERER_VENUS`
alone does nothing: `virgl_renderer_init` only reaches `proxy_renderer_init`
when `VIRGL_RENDERER_RENDER_SERVER` is also set, and every Venus entry point
after that begins with `if (!state.proxy_initialized) return EINVAL` — the
capset, `context_create_with_flags`, the blob allocation. The init flags are
therefore `USE_EGL | USE_SURFACELESS | VENUS | RENDER_SERVER`, which brings up
classic virgl in-process *and* a Venus decoder in a subprocess the library
spawns (found at the path compiled into it, overridable with
`RENDER_SERVER_EXEC_PATH`).

That is a security property arriving for free, and it deserves saying out loud:
**the untrusted Vulkan command stream is decoded in a process of its own
regardless of `[display] virgl_isolation`.** GPU-012 containment covers the GL
half; virglrenderer's own render server covers the Vulkan half.

**2. The venus command ring is not a guest blob.** Phase 1's blob amendment
guessed it would be, and built `BLOB_MEM_GUEST` around that guess. It is wrong,
and the reason is the render server: `proxy_context_attach_resource` refuses any
resource it cannot receive as a *file descriptor*, and an iovec list over
anonymous guest RAM never is one. Mesa's venus driver knows this and allocates
its rings and reply shmem as `HOST3D` + `MAPPABLE` blobs — the path this phase
implements. So the phase-1 asymmetry (a guest-memory blob never reaches the
renderer) survives untouched, and now rests on a measurement instead of a
preference. The one code change it forced is smaller than the guess would have
been: `Renderer3d::create_blob` grew a `ctx_id`, because
`virgl_renderer_resource_create_blob` resolves a host blob *through the context
that asked for it*, and for Venus that context is the Vulkan connection the
`blob_id` was minted on.

**3. `virgl_renderer_resource_map` hands back a plain host pointer**, page
aligned in practice, together with a length and (through
`virgl_renderer_resource_get_map_info`) a caching type — exactly the shape the
phase-2 "next agent starts here" list predicted, and the reason
`VIRGL_RENDERER_USE_EXTERNAL_BLOB` is **not** set. With external blobs a host
blob comes back as an fd the VMM must `mmap` itself; without it the library does
the `mmap` and the VMM gets an address it can hand straight to a hypervisor.
Page alignment is still *checked*, not assumed: `vkMapMemory` promises only
`minMemoryMapAlignment`, which the spec allows to be 64 bytes.

### The window had to become a different object

Phase 2 mapped the whole window as one hypervisor slot and let the device write
into it (the loopback signature, and the `pci_shm.rs` proof). Venus wants the
opposite: a host pointer per blob, at the offset the guest named. The two cannot
both be live — they overlap, and KVM refuses an overlapping memory slot while
WHP fails the `WHvMapGpaRange` — so this is a **mode**, not a layering:

| | device-backed (phase 2) | renderer-mapped (Venus) |
|---|---|---|
| what the guest reads | pages the VMM allocated | `VkDeviceMemory` the host Vulkan driver allocated |
| hypervisor objects | one, covering the BAR | one per live blob mapping |
| before any blob is mapped | the whole window decodes | **nothing** decodes |
| device host access (`read`/`write`/`fill`) | works | **refused** |
| who clears a span before the guest sees it | the device (`reserve_mapping`) | nobody has to: the span *is* a fresh allocation |

The mode is declared by the renderer (`BlobSupport::host_mapped`), carried by
the device (`ShmRegion::host_mapped`) and honoured by the machine
(`SharedWindow::new_host_mapped`). Refusing host reads and writes on such a
window rather than silently doing nothing is the point of the middle rows: a
device writing bytes no guest can see is a bug that looks like a working
guarantee.

**`GpaMapper` is addressed by range now, not by region.** One pair,
`map_range`/`unmap_range`, taking a `HostRange` — a plain (address, length)
whose only general constructor is `unsafe` and carries the whole contract:
*these pages stay mapped, at this address, until the matching unmap returns*.
`HostShmRegion` discharges it safely for a window's own pages; the renderer
discharges it for a blob mapping, and every path that calls
`virgl_renderer_resource_unmap` takes the guest mapping down **first**
(`VirglRenderer::take_mapping_down`, which `unmap_blob`, `destroy_blob`,
`reset` and `Drop` all go through). The reverse order is a guest reading a
`VkDeviceMemory` the host has recycled.

KVM needed the only real bookkeeping: a memory slot is identified by number, so
`Vm::create_shm_window` reserves `1 + MAX_HOST_RANGES` of them per window and
the mapper allocates out of that pool. `MAX_HOST_RANGES` (64) is therefore not a
bookkeeping bound like the device's `MAX_HOST_VISIBLE_MAPPINGS` (4096) — it is
the number of *hypervisor objects* a guest can make the host create, and past it
a map fails in band. WHP addresses a range by its address and needed nothing; it
gets the mode anyway, because it costs four lines and Windows will want it the
day it has a renderer.

One consequence for the isolated renderer (GPU-012), decided rather than
discovered: `RemoteRenderer` now **withholds the window entirely** instead of
advertising one it cannot fill. An `Arc` does not cross a pipe, and a host
pointer in the helper's address space names nothing in the VMM's — so a guest
running against an isolated renderer is told at `RESOURCE_CREATE_BLOB` time
that there is no mappable memory here, rather than at map time when it has
already built a Vulkan allocation around the promise.

### What is fuzzed, and why it is a new target rather than a wider old one

`gpu_blob` fuzzes a guest offset indexing *inside* a mapping. The new surface is
a guest offset **becoming** a mapping, out of three guest-controlled values
multiplied together (the BAR base, the region's offset in the BAR, and the
offset inside the region). `fuzz/fuzz_targets/venus_window.rs` drives the real
`machine_x86::shm::ShmWindow` with a recording `GpaMapper` and real host pages,
and re-derives the invariants from **outside** — against what the hypervisor was
actually told, never against the bookkeeping meant to maintain it:

* every live range lies inside the window's current placement;
* no two live ranges overlap (the mapper asserts it the way KVM would refuse
  it);
* nothing at all is mapped while the BAR decodes nothing, or decodes somewhere
  the machine will not follow;
* the count never passes `MAX_HOST_RANGES`;
* a second region's offsets can never reach the first region's span;
* dropping the window leaves the hypervisor holding nothing.

**It paid for itself in two minutes.** `ShmBackingHandle::unmap_host` bounded
its offset with `at(offset, 0)`, and a zero-length span is "inside" a region at
`offset == len` too — which is the *next* region's offset 0. So region 1 could
unmap region 2's mapping, and the guest would go on reading host memory the
renderer was about to free. An unmap names a byte, so the byte has to be one of
ours: the check is `offset < len` now, with a unit test beside the existing
`a_region_backing_cannot_reach_another_region`. (The target's *first* finding
was in the harness rather than the product — dropping the `ShmWindow` while the
region backings still held `Arc`s on the window, so nothing was unmapped. Worth
recording, because the ownership note in `machine_x86::shm` says exactly that
and it was still got wrong.)

Campaign on the tree as merged: **1 050 426 executions in 902 s** (1164
exec/s, 1539-case corpus, peak RSS 528 MiB), no crash and no invariant
violation; a 630 426-execution run immediately after the fix was equally clean.
`gpu_blob` and `gpu_3d_commands` are unchanged and still cover what they
covered.

### Honesty about performance, and what was refused

The only Vulkan ICD on this project's development host is **lavapipe** —
`llvmpipe`, `PHYSICAL_DEVICE_TYPE_CPU`. A working Venus path here therefore
proves *correctness*: that the protocol, the blob allocation, the window
mapping and the guest's driver all agree. It proves nothing whatsoever about
speed, and a frame number taken from it would be a number about llvmpipe
running under a translation layer inside a VM.

So **no figure was added beside the phase-2 or GAME-2105 frame tables**, and
none should be until a host with a real Vulkan device runs this. That is not
caution for its own sake: those tables already produced one wrong conclusion
(the 1.9 ms readback that turned out to be a host-llvmpipe run), and the
procedural lesson recorded there — *a performance figure has to record which
renderer produced it* — applies twice over to a figure whose renderer is a CPU.

What was measured instead is the part that is about this code rather than about
the host GPU: `crates/virtio-gpu/tests/venus_host.rs` asserts the address
`virgl_renderer_resource_map` returns is page aligned, at least as long as the
blob, lands at the window offset the guest named, and is host memory the test
can write and read back — and that a device reset takes it out of the window
before the library frees it.

### The guest evidence

`tests/boot/tests/venus_vulkan.rs`, KVM, Ubuntu 26.04 Desktop live session,
4 vCPUs, 4096 MiB, UEFI, the Venus-capable library above. The guest kernel,
verbatim:

```text
[    0.818843] pci 0000:00:02.0: BAR 2 [mem 0x140000000-0x14fffffff 64bit pref]
[    1.565292] [drm] Host memory window: 0x140000000 +0x10000000
[    1.565816] [drm] features: +virgl +edid +resource_blob +host_visible
[    1.565818] [drm] features: +context_init
[    1.572425] [drm] number of cap sets: 3
[    1.573205] [drm] cap set 0: id 1, max-version 1, max-size 308
[    1.573940] [drm] cap set 1: id 2, max-version 2, max-size 1384
[    1.574538] [drm] cap set 2: id 4, max-version 0, max-size 160
```

`+host_visible` is the line phase 2 could not produce: the kernel found the
shared-memory region, claimed the 64-bit prefetchable BAR at the top of RAM,
and is willing to map blobs into it. Cap set 2 is `VIRTIO_GPU_CAPSET_VENUS`.

Then, typed into a `systemd.debug_shell` root shell once the desktop was up —
a python-ctypes `vkCreateInstance` + `vkEnumeratePhysicalDevices` +
`vkGetPhysicalDeviceProperties`, needing nothing on the ISO but
`libvulkan.so.1`:

```text
VKPROBE_ICDS=asahi_icd.json gfxstream_vk_icd.json intel_hasvk_icd.json
  intel_icd.json lvp_icd.json nouveau_icd.json radeon_icd.json virtio_icd.json
VKPROBE_COUNT=2 rc=0
VKPROBE_DEVICE=Virtio-GPU Venus (llvmpipe (LLVM 15.0.7, 256 bits))
VKPROBE_DEVICE=llvmpipe (LLVM 21.1.8, 256 bits)
```

Two Vulkan devices, and the part that cannot be faked is the pair of LLVM
versions. **15.0.7** is jammy's mesa 23.2.1 — the *host's* lavapipe, reached
through Venus. **21.1.8** is Ubuntu 26.04's own, the guest's software fallback
sitting next to it. A guest process enumerated the host's Vulkan stack over
virtio-gpu, and the string `Virtio-GPU Venus (...)` is mesa's venus driver
naming itself.

The desktop reached `graphical.target` in 62 s on the same device, so the GL
half is unaffected: one virtio-gpu serves classic virgl to mutter and Venus to
Vulkan clients at the same time, which is the whole point of putting the capset
beside the others rather than instead of them.

**Two runs before this one failed, and both failures were real.**

The first died with `SIGSEGV` after 90 seconds — WSLg's D3D12 mesa dereferencing
a NULL gallium hook under sustained GNOME compositing, the exact GPU-012 failure
model this ADR recorded in 2026-08-20, this time provoked by `gst-plugin-scan`
probing video buffers. `LIBGL_ALWAYS_SOFTWARE=1` moves the *GL* half onto host
llvmpipe and is what lets a multi-minute desktop run finish here; it does not
touch the Venus half, which is lavapipe either way. That is now in the
`dev-environment` skill, because every future 3D acceptance on this machine will
need it.

The second reached the probe and the guest said:

```text
[drm:virtio_gpu_dequeue_ctrl_func [virtio_gpu]] *ERROR* response 0x1205 (command 0x207)
Aborted                    (core dumped) python3 -c 'import ctypes; ...'
```

`ERR_INVALID_PARAMETER` on `SUBMIT_3D`, then mesa's venus driver aborting
because it could not build its ring — and the cause was ours.
`renderer::validate_stream` walks the **virgl** encoding (32-bit headers whose
top half is a payload dword count), and that encoding belongs to the classic
virgl *context type*, not to the command that carries it. `Gpu3d` now remembers
each context's capset and walks only a virgl one's stream. What every context
type still owes is the part that is about this device rather than about the
encoding: the size cap, and dword alignment, because
`virgl_renderer_submit_cmd` takes a dword count and a length that is not a
multiple of four cannot be passed on at all. Beyond that a typed context's
bytes are opaque here by design — validating them would mean implementing Venus
in the VMM, and the component that does understand them decodes them in a
process of its own.

Two smaller things the runs turned up, both now fixed:

* **`CTX_DETACH_RESOURCE` from a destroyed context.** Once per boot, right
  after `fb0`: the guest's DRM client destroys its context and *then* closes
  the objects that were attached to it. Refusing that made the guest log
  `*ERROR* response 0x1204 (command 0x203)` about an operation the device was
  going to do nothing about; it answers OK now, as QEMU and crosvm do. An
  attach still requires a live context.
* **`ERR_UNSPEC` on ten `SUBMIT_3D`s** around the 100-second mark, from the
  guest's gstreamer probing hardware video decode. Our library is built without
  `-Dvideo=true`, so `CREATE_VIDEO_BUFFER` is rejected by virglrenderer and the
  guest falls back. Left alone deliberately: a feature we do not offer being
  refused in band is the system working.

### Can the installer set this up for the user?

Asked, and worth answering here because the answer is mostly yes and the
constraints are specific.

**Is the build redistributable as it stands?** Yes. virglrenderer is MIT and
the four things the built `.so` needs at runtime are `libepoxy.so.0` (MIT),
`libdrm.so.2` (MIT), `libgbm.so.1` (Mesa, MIT), `libvulkan.so.1` (the
Khronos loader, Apache-2.0) and libc — all permissive, none copyleft, so the
`cargo deny` rule that governs our Cargo graph is not even engaged and the
attribution owed is a THIRD-PARTY-NOTICES entry. The `virgl_render_server`
binary beside it is part of the same MIT project and has to ship too, because
Venus does not work without it.

**What it links against at runtime, and therefore the minimum distro.** The
build on this machine is against jammy (Ubuntu 22.04): glibc 2.35, and the
sonames above. A binary built there runs on 22.04 and newer, not on older —
glibc is forward compatible only. The runtime packages a user needs are small:
`libepoxy0`, `libdrm2`, `libgbm1`, `libvulkan1` and a Vulkan ICD
(`mesa-vulkan-drivers` covers lavapipe and the open-source GPU drivers). Note
what is *not* in that list: no `-dev` packages, no meson, no compiler. The
build dependencies are a build-machine concern only.

**The shape that fits this project.** Publish the `.so` plus
`virgl_render_server` as a release asset (a `virglrenderer-<tag>-<distro>`
tarball with its own `SHA256SUMS` and provenance, exactly like `CLOUDHV.fd`),
pin the digest in a `guest/virglrenderer/pinned.toml` the binary
`include_str!`s, and have the installer's existing optional WSL task place it
beside the Linux engine and `apt install` the five runtime packages. `wsl -u
root` needs no password, which is what makes the apt half possible at all —
the same property that made the WSL engine task possible. `RENDER_SERVER_EXEC_PATH`
is how the server is found once it is not at its build-time prefix, and
`ENTANGLED_VIRGL_LIB` is how the engine finds the library; both already exist.

Two caveats to state on that page rather than discover later. First, the user's
WSL still has **no GPU Vulkan device** — WSLg exposes `/dev/dxg`, not a DRM
node, so the ICD that gets used is lavapipe and Venus there is correctness, not
speed. Second, a distro-shipped virglrenderer will eventually be new enough
(Ubuntu 24.04 packages 1.0.0, 26.04 will be newer still), at which point the
right answer is to stop shipping ours and let the runtime probe find theirs —
which it already would, because `ENTANGLED_VIRGL_LIB` is only consulted when it
is set.

### Next agent starts here

1. **A host with a real Vulkan device.** Everything above is correctness; none
   of it is a performance claim, and this host cannot make one. A native Linux
   box with a DRM node, or Windows against its own ICD, is where the number
   that motivated EPIC 20 actually lives. Until then, do not put a Venus figure
   beside the frame tables.
2. **Windows.** WHP already has the window, the mode and the sub-range mapping;
   what it lacks is a renderer. Two shapes, unchanged from the 2026-08-20
   amendment: virglrenderer built for Windows on ANGLE (GL only, no Venus —
   the render server is `fork`/`socketpair`), or a native Venus decoder. The
   render-server dependency discovered here makes the first cheaper and the
   second harder than the phase-1 note assumed, and that is worth re-costing
   before anyone starts.
3. **Zero-copy scanout (VEN-2005 / GPU phase 3)** is still where the frame rate
   is — GAME-2105 measured the readback at 13 ms of a 20 ms frame — and it is
   still blocked on a host with a DRM node. Unrelated to Venus, and unblocked
   by nothing in this phase.
4. **`SET_SCANOUT_BLOB` on a host blob** is still refused. With Venus that is
   now a real gap rather than a theoretical one: a Vulkan application rendering
   into a `VkImage` and presenting it wants exactly that path, and today it has
   to round-trip through a guest-memory blob. It needs the same export the
   zero-copy scanout needs, so it belongs with item 3.
5. **The GUI capability gate** (`Backend::virgl_block`) still knows nothing
   about any of this. Now that a host *can* serve Venus, the "3D: Venus /
   VirGL / none" line the phase-1 list asked for has something to say — and it
   should say which library it found and where, because on a machine with both
   0.9.1 and a self-built 1.1.0 the difference is invisible otherwise.
6. **`MAX_HOST_RANGES` is 64 and untested against a demanding guest.** A
   Vulkan application with many host-visible `VkDeviceMemory` allocations will
   find it. Raising it is a one-constant change (KVM's slot limit is far
   higher), but the honest move is to measure a real application first and set
   it from that rather than from a guess.

## Amendment, 2026-09-16 — the renderer becomes an artifact we publish

Every amendment before this one asked what Venus *could* do on a host that had
built virglrenderer by hand. This one is about the hosts that have not, which is
all of them: `entangled fetch virglrenderer` now downloads a Venus-capable
renderer the way `fetch firmware` and `fetch bootstrap-kernel` already download
theirs, and `doctor` reports what a host actually has.

### What was missing, stated plainly

Nothing shipped a renderer, on any surface. `entangled fetch` knew three
targets and none was this; `installer/entangled.iss` carried two executables,
an icon, three text files and `CLOUDHV.fd`; the WSL engine installer
(`control_api::wsl::install_script`) copies exactly one file, the `entangled`
binary, so the "WSL (KVM)" backend the manager advertises as *the one that can
do 3D* installed nothing that makes 3D possible. `doctor` said nothing about 3D
at all, the manager's 3D checkbox was gated only on `is_linux_kvm()`, and the
error a user finally got at VM start named `guest/virglrenderer/build-virglrenderer.sh`
— a path inside a source checkout that an installed user does not have.

That is the same shape as the three shipped-product bugs this project already
records: a firmware the installer never carried, a Linux engine it never
shipped, a pinned digest no release served.

### The two things that had to change in the loader first

Publishing a prebuilt `.so` is not just a matter of uploading one.

**The render server's path is compiled in, and absolute.** Venus in
virglrenderer 1.1 exists only behind `VIRGL_RENDERER_RENDER_SERVER`, and the
library `fork`/`exec`s `virgl_render_server` from a path fixed at build time
under its own `--prefix`. For a downloaded artifact that is a directory on a CI
runner, so Venus would degrade — quietly, to classic virgl with a warning,
which is the failure mode that survives a release because nothing crashes.
`VirglRenderer` now derives the path from the library it actually opened and
exports `RENDER_SERVER_EXEC_PATH`, unless the operator set it. Measured rather
than assumed: with the build's original prefix moved away so the compiled-in
path is dead, `venus_host.rs` still passes from a relocated tree — and the
negative control, the same tree with no `virgl_render_server` in it, skips with
"Venus is advertised but a venus context will not start".

**A library we chose is a preference, not an instruction.** There are now two
variables. `ENTANGLED_VIRGL_LIB` is a person's, and a path that will not open
is still a hard error, because silently loading a different library is how a
Venus run becomes a classic-virgl run nobody notices.
`ENTANGLED_VIRGL_LIB_DEFAULT` is the one `entangled run` sets from the cache,
and a path that will not open falls through to the system library. The host
that makes the difference real has no `libvulkan.so.1`: a Venus build lists it
in `DT_NEEDED` and will not `dlopen` at all there, while the distribution's own
0.9.x does not link Vulkan and works fine. Preferring our download must not
take 3D away from somebody who had it.

### The ABI question, and why it turned out to be smaller than it looked

A published binary carries a glibc floor, which the firmware and the kernel do
not have to think about — both are flat images that link nothing. Measured on
the 1.1.0 build: the library and the render server each need at most
**GLIBC_2.34**, and their only *versioned* symbol requirements come from `libc`
and `libm` — nothing versioned from libepoxy, libdrm, libgbm or libvulkan,
which are plain soname matches.

The floor is therefore one number, and it is a number this project had already
chosen: `release.yml` builds the Linux engine on `ubuntu-22.04`, so
`.github/workflows/virglrenderer.yml` does too. GLIBC_2.34 covers Ubuntu 22.04+,
Debian 12+ and Fedora 35+, and excludes Ubuntu 20.04 and Debian 11 — which the
Linux engine already excluded. The renderer narrows nothing.

### What a user needs on their own host

Only runtime libraries, all present on any desktop install: `libepoxy.so.0`,
`libdrm.so.2`, `libgbm.so.1`, `libvulkan.so.1`, `libm`, `libc`. The `-dev`
packages and meson/ninja that `build-virglrenderer.sh` wants are a developer's
problem and stay one.

### Still not done

- **The pin has no release behind it.** `guest/virglrenderer/pinned.toml`
  carries placeholder digests and a note saying so; a fetch answers 404 with
  the workflow's name in it. Running `.github/workflows/virglrenderer.yml` once
  and committing the block it prints is what turns this on, and only a push can
  do that.
- **Windows gets nothing from this.** The artifact is a Linux `.so` and only
  the WSL engine can load it; `fetch virglrenderer` on Windows downloads bytes
  that host cannot use. Driving the fetch *inside* WSL from the manager is the
  obvious follow-up, and is not done.
- **The manager still gates its 3D checkbox on the backend alone**, not on
  whether a renderer exists. `doctor` now knows; the GUI does not.

## Amendment, 2026-09-16 — what a Windows host can actually share with a renderer

The Windows 3D plan in this ADR has always been one sentence ("virglrenderer
built for Windows on ANGLE"). Two reconnaissance passes replaced it with
measurements, and the conclusion is different from the sentence.

### ANGLE is not the way in

virglrenderer *does* have a Windows host target — `with_host_windows` in
`meson.build`, a `mman_win32.c` shim, and `have_egl = true` forced for Windows
and Darwin. It looks supported until you read how upstream builds it:

```
--cross-file=.gitlab-ci/x86_64-w64-mingw32 -Dplatforms= -Dtests=false
                                            -Drender-server=false -Dvenus=false
```

`-Dplatforms=` is **empty**: no EGL, no GLX, no renderer backend at all. It is a
compile smoke test, and the job carrying it is marked `FIXME: ... turned off`.
Meanwhile `vrend_winsys_egl.c` — the only EGL winsys there is — is written
around gbm (ten `#ifdef ENABLE_GBM` sites) and `ENABLE_GBM` is never set on
Windows, and its surfaceless path asks for `EGL_PLATFORM_SURFACELESS_MESA`,
which ANGLE does not implement. So "build virglrenderer for Windows against
ANGLE" is not a configuration; it is writing a winsys backend for a shape
nobody has ever run, in C we do not own, for classic VirGL only.

### The renderer cannot be remote, for a reason that is not about transport

`RemoteRenderer` already runs the renderer in another process and its transport
is abstracted (a `socketpair` on Unix, a duplex named pipe on Windows), its
protocol is portable and tested, and guest memory never crosses it — only
bytes. Moving that transport to TCP, and the renderer to a Linux box or WSL,
therefore looks cheap. It is cheap. It also cannot serve Venus, and the reason
is structural rather than incidental: the host-visible window is *host pages in
the VMM's address space*, and a pointer another process hands back names
nothing there. `remote::client` already withholds the window for exactly this,
which is why an isolated renderer reports `host_visible_bytes=0` while still
advertising the venus capset.

Measured anyway, because the transport question was worth pricing
(Windows → WSL, this machine): **684 µs** per call/reply over the VM's own
address, **29.4 ms** to move a 1080p frame, a ~34 fps ceiling from the readback
alone. Over `127.0.0.1` it is **50 ms** per call — WSL2's localhost forwarding
is a userspace relay and is 73× worse than the direct address, which is a trap
worth knowing before anybody benchmarks anything across that boundary.

### What Windows *can* do, measured

The interesting design is therefore not a port of virglrenderer at all: it is a
native Venus decoder against the host's own Vulkan, in a sandboxed process,
sharing one image with the VMM. Its riskiest assumption — that a frame can go
from the decoder to the presenter without passing through the CPU — is now
tested on this machine (NVIDIA RTX 2070, driver `0x91160000`, Vulkan 1.4.312).
The probe is kept with the VM directory; its four findings:

1. Every extension the design needs is present: `VK_KHR_external_memory`,
   `..._win32`, `VK_KHR_external_semaphore`, `..._win32`,
   `VK_KHR_dedicated_allocation`, `VK_KHR_timeline_semaphore`.
2. `B8G8R8A8_UNORM`, `OPTIMAL`, as `OPAQUE_WIN32`: exportable **and**
   importable, and not dedicated-only.
3. An image exported on one device and imported on another reads back the
   producer's pixels exactly.
4. **Across a real process boundary** — a second process, its own instance,
   device and queue, opening the memory by name — the same. `SHARED ACROSS
   PROCESSES`.

Costs, 1080p:

| | per frame | ceiling |
|---|---|---|
| produce into the shared image (no CPU copy) | 0.070 ms | ~14 300 fps |
| copy the same frame out to host memory | 0.740 ms | ~1 350 fps |

Sharing is 10.6× cheaper — **and both numbers are irrelevant**, which is the
finding that matters. The guest desktop measured on this host runs at 5.4 fps,
a 185 ms frame; a local readback is 0.4 % of that. Zero-copy scanout (VEN-2005)
is therefore *not* the thing to build first and was never the risk. The frames
go somewhere else entirely, and on a Windows host that somewhere is the
decoder that does not exist yet.

Two traps this probe cost, recorded so the next one does not pay them:

- A named export with `dwAccess = 0` produces a handle nothing may open, and
  the import fails as `ERROR_OUT_OF_DEVICE_MEMORY` — an error that says nothing
  about access rights. `GENERIC_ALL` is what a shared render target wants, and a
  sandboxed decoder will want a deliberate ACL rather than the default.
- The first run of the probe reported "NOT shared" because it cleared to 0.5 and
  expected 128 where the GPU produced 127. A one-bit rounding disagreement reads
  exactly like a failed mechanism; clear to an exact `k/255`.

### The presentation layer closes the path

wgpu consumes the shared image, measured the same day. The route is entirely
below wgpu's public API and every step of it exists:

1. `Features::VULKAN_EXTERNAL_MEMORY_WIN32` — wgpu-hal already enables
   `VK_KHR_external_memory_win32` when the adapter has it, and exposes the fact
   as a feature, so no bring-your-own-device is needed;
2. `Device::as_hal::<Vulkan>()` for the raw `ash::Device`;
3. import the named memory and bind an image, exactly as the raw probe does;
4. `wgpu_hal::vulkan::Device::texture_from_raw` — with a **drop callback**, or
   wgpu-hal takes ownership of the image and destroys it without knowing about
   the memory imported behind it;
5. `Device::create_texture_from_hal::<Vulkan>()` for a real `wgpu::Texture`.

In a separate process from the producer, wgpu's own command encoder then read
back the exact pixels the producer cleared:

```
WGPU: adapter NVIDIA GeForce RTX 2070 (Vulkan)
WGPU: device opened with VULKAN_EXTERNAL_MEMORY_WIN32
WGPU: imported the shared memory onto wgpu's Vulkan device
WGPU: it is now a wgpu::Texture
WGPU: read BGRA [224, 160, 64, 255] back through wgpu ->
      THE PRESENTATION LAYER SEES THE PRODUCER'S PIXELS
```

The `copy_texture_to_buffer` in that last line is the *proof*, not the design:
it exists so a CPU-side assertion is possible. A real presenter samples the
texture into the surface and never touches host memory at all.

So the whole presentation path for a Windows host with an out-of-process
renderer is now demonstrated end to end, and the `Backends::VULKAN` hint is
load-bearing — wgpu prefers DX12 on this host, and a DX12 device cannot import
a Vulkan `OPAQUE_WIN32` handle.

### Still untested

- **External semaphores in anger.** The extension is present; the decoder/
  presenter handshake is a design item, not a measured one.
- Everything above the boundary: the decoder itself, which is the whole cost.

## Amendment, 2026-09-17 — EPIC 20 phase 4: the transport, and the first bytes a real guest sent

Phase 3 made Venus render on a Linux host with virglrenderer doing the Vulkan.
This phase builds the half we cannot borrow: **our own** implementation of
everything a Venus guest touches before a single Vulkan command is executed —
the capset it reads to decide whether to load at all, the byte primitives, the
ten transport commands, the command ring's layout and the pump that drains it.
It executes no Vulkan. That seam is the point: the parts a malicious guest can
reach are pure logic over bytes, so they are tested on every host, including
hosts with no GPU.

### What is here

`crates/virtio-gpu/src/venus/`, seven modules, ~300 tests:

| Module | What it owns |
|---|---|
| `capset` | The 160 bytes a guest reads before it will speak to us |
| `wire` | Decoder/encoder primitives: little-endian, 4-byte granular, no length field anywhere |
| `transport` | The ten commands that arrive on the context stream and create the ring |
| `ring` | The five regions of a ring the guest proposes and we validate |
| `shmem` | The host pages the ring lives in — the only `unsafe` in the family |
| `pump` | The head/tail protocol, and the shadow copy that makes decoding safe while the guest writes |
| `renderer` | The `Renderer3d` that ties them together, behind `ENTANGLED_VENUS_CAPTURE` |

The renderer is a diagnostic stage behind an environment variable rather than a
profile key, for the same reason `ENTANGLED_GPU_FENCES` is: a `[display]` key
would be inherited by every profile, the manager and the installer's tests, for
something the next phase deletes.

`capset` and `ring` were additionally mutation-tested, because a test suite
over a byte layout is the easiest kind to write vacuously — every assertion
passes and every field is off by four.

### The bytes

All of those tests encode what we believe the protocol to be and decode it
again. That proves self-consistency and nothing else: a field misread the same
way twice round-trips perfectly. So the acceptance for this phase was to make
a *real* Mesa venus driver, in a real guest, say something to us.

It did. Seventy-two bytes, on an Ubuntu guest under WHP:

```
vkSetReplyCommandStreamMESA { resourceId:  8, offset: 0, size: 20 }
vkSetReplyCommandStreamMESA { resourceId: 10, offset: 0, size: 20 }
```

— the driver pointing our reply encoder at a window before it asks anything,
twice, the second time with a fresh blob after the first got no answer. Then
Mesa called `abort()`, which is exactly what this stage promised: a renderer
that replies to nothing gets a guest that gives up.

Those bytes are now `the_first_bytes_a_real_mesa_venus_driver_sent_us` in
`transport.rs`, and they pin down three things a round trip cannot: that the
command header is `{ opcode, flags }` and not the reverse, that a
`simple_pointer` is a **64-bit** presence marker rather than the 32-bit one it
is natural to write, and that `VkCommandStreamDescriptionMESA` packs a
`uint32_t` and two `size_t`s with no padding — so `offset` lands unaligned. Our
decoder read them correctly on the first attempt.

### Four days of the wrong question

Getting there took seven guest boots, and six of them asked the wrong question,
which is worth recording because the failure mode is general.

The guest kernel read our capset perfectly from the first run — `cap set 0: id
4, max-version 0, max-size 160`, every feature negotiated, the host-visible
window mapped at `0x140000000` — and yet Mesa yielded `llvmpipe` and the
capture file stayed empty. With `VN_DEBUG=init` producing **no output at all**,
the natural reading was "the driver looked at our capset and declined", and
four hypotheses were eliminated against that reading: an empty extension mask,
a `max_version` mismatch, `supports_multiple_timelines = 0`, a missing driver
library. Two capset fields were even falsified to coax the driver past a gate
it was never standing at.

The reading was unfounded, and one control run killed it: `VN_DEBUG=vtest`
forces venus onto a renderer that cannot exist, and it printed nothing either.
Silence was never evidence.

Asking the kernel directly — the same three ioctls venus makes, from python,
with Mesa out of the middle — gave the answer in one line:

```
OPEN=errno13(Permission denied)      /dev/dri/renderD128, as the login user
ROOT OPEN=ok
ROOT CAPS=ok       vk_xml_version 1.3.269, our 160 bytes, verbatim
ROOT CTXINIT=ok    a Venus context, on our renderer
```

We log in on `ttyS0`. `systemd-logind` grants the DRM render node to the user
of a *graphical* seat by ACL, and a serial login is not one. Every silent run
was a driver that failed `open(2)` before it had anything to say. Our capset
was never the question.

`supports_multiple_timelines` has been reverted to false, which is what
`VenusCapset::new()` says truthfully: `virtio_gpu::fence` is one FIFO, and a
renderer that promises per-queue timelines and then retires in submission order
does not fail loudly — it returns the wrong fence to the wrong queue. The
permissive extension mask stays, on two grounds that the other field had
neither: it is what virglrenderer effectively advertises (`venus_hw.h`, with
the sentinel clear "all the extensions are assumed to be supported by the
renderer side protocol"), and it is the configuration the captured bytes were
produced under, so removing it would cost the golden vector its provenance.

The general lesson: **a diagnostic's silence is only evidence once you have
seen that diagnostic speak.** Establish that first, or every hypothesis you
eliminate is eliminated against nothing.

### What phase 4 does not have

- **Replies.** The renderer decodes and captures; it encodes nothing back.
  That is the whole of phase 5, and Mesa's `abort()` is the measurement of it.
- **`save`/`load` for rings** (ADR-0006). A snapshot taken with a live Venus
  context will refuse.
- **One sink for all rings**, so a multi-ring capture interleaves into soup.
- **`allow_vk_wait_syncs`**, which belongs with the threading model rather than
  with the capset that advertises it.

## Amendment, 2026-09-23 — the guest can see NVIDIA Vulkan memory under WHP

Venus maps host `VkDeviceMemory` straight into guest-physical space. On KVM
that works because KVM maps any host VA; on WHP nobody had measured whether
memory the NVIDIA driver owns, or has imported, survives `WHvMapGpaRange` with
coherent data both ways. Everything in phase 5 that touches memory rested on
that, so it was measured before anything was built on it
(`F:\VMs\Entangled\probes\host-visible-memory\`, RTX 2070, driver 580.88,
Windows 10 Home).

### It works, in every case tried

Two candidates, 2 MiB each, one row per memory type and order:

- **A — the driver's memory:** `vkAllocateMemory` + `vkMapMemory`, then
  `WHvMapGpaRange` of that pointer, for every `HOST_VISIBLE` type (3, 4, and
  the 214 MiB `DEVICE_LOCAL|HOST_VISIBLE` type 5).
- **B — our pages, imported:** `VirtualAlloc`, then
  `VK_EXT_external_memory_host`. Only types 3 and 4 accept a host pointer
  (`memoryTypeBits = 0x18`, alignment 4 KiB). Both orders were tried: import
  then map, and map then import.

Every row: allocation `VK_SUCCESS`, mapping `S_OK`, and host-CPU → guest,
GPU → guest, guest → GPU and unmap/remap (a different allocation at the same
GPA, never stale) all correct. Each step used its own pattern, and a stale
value would have been named as such. This held over six full runs, with the
guest's caches both disabled and enabled, and I reproduced it independently.

### What differs is speed, and the type decides it

Guest access, in TSC ticks per dword (plain guest RAM ≈ 2.2):

| backing | guest read | guest store | note |
|---|---|---|---|
| A, type 4 (cached) | ≈ 2–3 | ≈ 2–3 | full speed |
| B, type 3 or 4 (our pages) | ≈ 2–6 | ≈ 2–5 | full speed, even as type 3 |
| A, type 3 (driver WC) | ≈ 300–500 | ≈ 180–380 | host stores 3–5: **write-combining is lost in the guest** |
| A, type 5 (BAR) | ≈ 2000 | ≈ 30–200 | reads as slow as the host's own; one run landed in system memory |

The guest's own cache settings changed nothing. On this host the host-side
mapping decides the memory type.

`WHvMapGpaRange` of 2 MiB takes a median of 24–39 µs, and `WHvUnmapGpaRange`
55–116 µs.

### Consequences for the renderer

- **Guest-visible memory of types 3 and 4 is backed by our own pages,
  imported.** That is the fast row, and the VMM owns the pages and their
  lifetime — which is also what save/restore (ADR-0006) and the guest-untrusted
  rules want: freeing a guest's allocation can never leave the partition
  mapping a page the driver has recycled.
- **Type 5 is the open question.** It cannot be imported. Mapped from the
  driver it is correct but slow, and its placement is not stable. Either the
  renderer hides it from the guest (advertising a subset of memory properties
  is legitimate), or it forwards it for write-only uploads. That is decided
  with a real workload, not now.
- **Mappings are not free.** At tens of µs each, the renderer maps whole
  `VkDeviceMemory` objects, never sub-ranges per access. A guest allocator that
  suballocates, as every serious one does, keeps the count low.

Untested: the driver migrating type 5 memory under pressure while it is
mapped, allocations much larger than 2 MiB, and a Linux guest's own PAT
choices.

## Correction, 2026-09-23 — what the phase-4 bytes actually were

A reading of Mesa 26.2.3 and virglrenderer 1.1.0 against the phase-4 run
(`spec-phase5-bringup.md`, kept with the session's research notes) shows that
three things written in the phase-4 amendment were wrong. They are kept above
as written, as this ADR does with its other wrong guesses, and corrected here.

- **The two `SetReply`s were not a retry.** A reply window comes from a
  sequential pool created per `VkInstance` (`vn_renderer_util.c:94-116`,
  `vn_instance.c:315-316`), and each `VkInstance` is its own virtio-gpu
  context with its own ring. Two windows, both at offset 0, therefore mean two
  instances in two contexts. They landed in one file because the capture sink
  fed every ring into it.
- **The abort was not "no reply"; it was our doorbell model.** Mesa submits
  `SetReply` and the command it precedes as two ring submissions with nothing
  between them. It rings the doorbell at most once per millisecond
  (`vn_ring.c:478-489`) and relies on the host polling for the `idleTimeout` it
  passed at ring creation. Our renderer drained once on the doorbell, published
  `IDLE` and never looked again, so `vkEnumerateInstanceVersion` (opcode 137,
  16 bytes) sat unread in each ring. About 3.5 s later Mesa's watchdog found
  `VK_RING_STATUS_ALIVE_BIT_MESA` never set and aborted
  (`vn_common.c:229-283`). Stage 5a.1 fixes both: a ring worker that polls for
  `idleTimeout`, and an `ALIVE` monitor.
- **virglrenderer does not leave the extension-mask sentinel clear.** It sets
  it, over an enumerated mask of exactly what its protocol decodes
  (`vkr_renderer.c:40-48`). The permissive mask in `run_vm.rs` therefore has no
  reference precedent, and it goes once the generated protocol (stage 5a.2)
  provides the table to enumerate from.

And one refinement of "a diagnostic's silence is only evidence once you have
seen it speak". `VN_DEBUG` was silent because it *could not* speak: every
`vn_log` is `MESA_LOG_DEBUG` (`vn_common.c:92-99`), and a release Mesa defaults
to `MESA_LOG_INFO` (`util/log.c:134-137`). Guest probes need
`MESA_LOG_LEVEL=debug` alongside `VN_DEBUG`. The lesson stands. The
`EACCES` on `/dev/dri/renderD128` stands too — it was measured with raw ioctls,
not inferred from silence.

## Amendment, 2026-09-23 — stage 5a.1, the ring service

The two faults the correction above names are fixed in
`crates/virtio-gpu/src/venus/service.rs`, and one debt from "What phase 4 does
not have" is paid.

- **Every ring has a worker thread**, faithful to `vkr_ring_thread`
  (`vkr_ring.c:241-335`): pump while there is progress; with none, keep
  polling `tail` (sixteen yields, then `vkr_ring_relax`'s doubling sleeps,
  never past the deadline) until `idleTimeout` has passed since the last
  progress; publish `IDLE`, re-read `tail`, and take `IDLE` back down if work
  arrived; otherwise park until the doorbell, and take `IDLE` down on waking.
  `vkNotifyRingMESA` now only wakes the worker; the device's queue worker never
  touches ring pages. The decisions are a pure state machine (`RingService`)
  over an injected clock, tested deterministically, including the exact Mesa
  shape: `SetReply` rings, the command a few microseconds later does not, both
  are consumed.
- **Every context with a monitored ring has an `ALIVE` monitor**
  (`vkr_context.c:507-545`), at the shortest period any of its rings asked
  for, floored at 1 ms (`MIN_MONITOR_PERIOD`) so a guest cannot make it spin; a
  period of zero refuses the ring, as the reference does. It is a separate
  thread because a worker stuck in one long command cannot report on itself,
  and a test holds a worker inside its sink to prove `ALIVE` keeps coming.
- **`status` is only ever read-modify-written.** The guest clears `ALIVE`
  with its own atomic AND (`vn_common.c:229-243`), so the pump's old
  whole-word store from a host-side mirror would have raced it. `RingBacking`
  now offers `store_head` and `set_status_bits`/`clear_status_bits`
  (`fetch_or`/`fetch_and`, `SeqCst`) and no whole-word store of `status`.
- **A sink decides how far `head` moves, and can end the ring.**
  `Batch::fatal_after(n)` advances `head` over `n` bytes and no further,
  publishes `FATAL` and stops the worker — never past a command whose reply was
  not written (spec §5 item 4). The capture sinks use it: they consume and
  record `SetReply`/`SeekReply`, and at the first command they would have to
  answer they record it and the rest of the batch and declare the ring fatal.
  Against a real guest the capture should be `SetReply` (36 B) +
  `vkEnumerateInstanceVersion` (16 B), and the guest should abort on "ring
  fatal error" at once rather than on its 3.5 s watchdog.

  **Measured on 2026-09-23** (Ubuntu guest, root, `MESA_LOG_LEVEL=debug
  VN_DEBUG=init,result`): the capture is exactly those 52 bytes, the second
  command byte-for-byte `89 00 00 00 01 00 00 00 01 00 00 00 00 00 00 00`, and
  the renderer logs FATAL after `0x24` of `0x34` bytes. Mesa, now able to
  speak, reports `connected to renderer`, wire format 1, vk.xml 1.3.269 and
  protocol spec 2, and then `aborting on ring fatal error at iter 4096`.
  The abort reason is the one predicted; its timing is not. Mesa reads the
  status word only at the same iteration where the watchdog would check
  `ALIVE`, so it still comes about 3.5 s in. What changed is *why* it aborts.
- **One sink per ring.** `VenusRenderer` takes a `SinkFactory`
  (`(ctx_id, ring) -> io::Result<S>`), and `ENTANGLED_VENUS_CAPTURE` is now a
  prefix: each ring writes `<prefix>.ctx<N>.ring<M>.bin`, `M` counting the
  run's rings from 0. The "one sink for all rings" debt above is gone.
- **ADR-0005 is honoured by both threads**: a `Quiesce` pass before every
  pass, outside every lock, and stop-and-join on `vkDestroyRingMESA`,
  `ctx_destroy`, blob destruction and `reset` — including on a paused VM.

Still owed: replies (5a.2 onwards), and `save`/`load` for rings (ADR-0006).

One risk the reference shares and this stage does not fix: the idle
handshake assumes the host's `idleTimeout` is at least as long, in real time,
as the guest's one-millisecond doorbell rate limit. A host clock that runs
fast — WSL's does, by up to 3.8 % — can publish `IDLE` a few tens of
microseconds before the guest is allowed to ring again; a submission in that
gap is announced by no doorbell, and the ring parks on it while the monitor
keeps the watchdog quiet. The wake-up latency of a real doorbell normally
covers the gap. If a guest is ever seen hanging with `IDLE` up and
`tail != head`, a bounded park (re-check `tail` every few milliseconds) is the
cheap fix.

## Amendment, 2026-09-23 — stage 5a.3, the executor

The renderer stops capturing and answers. `crates/virtio-gpu/src/venus/executor/`
is a `SinkFactory` whose per-ring sink decodes each command with the generated
protocol, executes it, writes the reply into the guest's reply window and only
then lets `head` move; `crates/virtio-gpu/src/host_vulkan/` is the host side,
over `ash` 0.38 loaded at run time (`Entry::load`, nothing linked). It is
attached with `ENTANGLED_VENUS=vulkan` — diagnostic, an environment variable,
for the capture's reason — and the run is refused before the guest boots if
the host has no device the executor would expose.

- **The host is a trait.** `HostVulkan` covers exactly this stage's calls and
  speaks the generated protocol structures, so the object table, the id rules,
  every policy and every reply shape are tested against a fake on every host;
  the `ash` side is the only code that turns a validated value into a driver
  structure, and it is outside `venus` so that family's only `unsafe` stays in
  `shmem`. The ~1500-field bridge between the two is generated
  (`scripts/venus-ash-gen.py`, from the protocol and ash's own definitions).
- **Object ids** are virglrenderer's rules plus the ones it leaves to the
  driver: unique per context whatever the type, typed lookups, parents
  recorded and checked, destruction in dependency order on `vkDestroy*`,
  context destruction and device reset. Anything wrong is fatal to the context
  and to the ring, with `head` left on the offending command.
- **Replies** go into a `HOST3D` blob of the same context, bounded by the
  window (stricter than vkr, which bounds by the resource), under the blob
  directory's lock, so a blob destroyed while it is the window is never
  written again.
- **What the guest is shown, where it differs from vkr:** CPU devices hidden;
  `apiVersion` capped at 1.3 in `Properties2` as well as in `Properties`;
  sparse features reported false (no sparse command exists here, and
  vulkaninfo enables what it is offered); device extensions limited to what the
  protocol decodes, which today is none; and the memory policy below.
- **Memory.** Type indices are the host's. Every type that does not accept an
  import of our own pages (`vkGetMemoryHostPointerPropertiesEXT` on a
  `RingPages` allocation, through a throwaway device) loses
  `HOST_VISIBLE|HOST_COHERENT|HOST_CACHED`; a device without
  `VK_EXT_external_memory_host`, or left with no coherent host-visible type, is
  not exposed. On the RTX 2070 (driver 580.88), from the real-GPU test:

  | type | heap | host flags | guest flags |
  |---|---|---|---|
  | 0 | 1 | — | — |
  | 1 | 0 | `DEVICE_LOCAL` | `DEVICE_LOCAL` |
  | 2 | 0 | `DEVICE_LOCAL` | `DEVICE_LOCAL` |
  | 3 | 1 | `HOST_VISIBLE\|HOST_COHERENT` | same |
  | 4 | 1 | `HOST_VISIBLE\|HOST_COHERENT\|HOST_CACHED` | same |
  | 5 | 2 | `DEVICE_LOCAL\|HOST_VISIBLE\|HOST_COHERENT` (the BAR) | `DEVICE_LOCAL` |

  `memoryTypeBits` importable from host allocations: `0x18`, as the
  2026-09-23 probe measured by hand.
- **The capset** now carries the enumerated mask of what the protocol decodes,
  sentinel set, as virglrenderer does; the "assume everything" override is
  gone from `run_vm.rs`.
- **Snapshots** are refused by name while a Venus context holds host Vulkan
  objects: `VirtioDevice::snapshot_refusal` (default `None`), asked by
  `MachineBus::snapshot_refusals` before anything is written.

Owed by the next stage: `supports_multiple_timelines` is still false, and
release Mesa binds every queue to a fence timeline in 1..63 regardless; the
executor records each queue's `ring_idx`, and `virtio_gpu::fence` needs one FIFO
per `ring_idx` before `vkQueueSubmit` can retire a guest fence and the capset
bit can flip. `vkExecuteCommandStreamsMESA` (commands over 8 KiB) is refused.

## Amendment, 2026-09-23 — a Linux guest on WHP sees the RTX 2070 through our own renderer

Stage 5a's milestone, measured in the Ubuntu guest (Mesa 26.0.8, root,
`VK_DRIVER_FILES` = venus only, `MESA_LOG_LEVEL=debug VN_DEBUG=init,result`),
against the executing renderer (`ENTANGLED_VENUS=vulkan`, commit `f3cf3ae`):

```
$ vulkaninfo --summary                       # exit 0
GPU0:
    apiVersion   = 1.2.0
    vendorID     = 0x10de
    deviceType   = PHYSICAL_DEVICE_TYPE_DISCRETE_GPU
    deviceName   = Virtio-GPU Venus (NVIDIA GeForce RTX 2070)
    driverName   = venus
    driverInfo   = Mesa 26.0.8-1ubuntu0.3
```

With every ICD visible, the loader orders the venus device first and llvmpipe
second. The renderer logged no refusal and no FATAL. Mesa logged
`renderer instance version 1.3.309`.

So every piece written for this stage has now run against the real thing:
the capset, the transport, the ring worker and its monitor, the generated
protocol, the object table and the host-Vulkan executor, on WHP and on the
host's own GPU. No other process and no C renderer sits in the path.

### Open

- **`apiVersion 1.2.0`, not 1.3.** We answer with the host's version capped at
  1.3, and the Mesa 26.2.3 source clamps only to 1.3 at the lowest
  (`vn_physical_device.c:528-541`). The guest runs 26.0.8, whose clamps may
  differ; the exact `.0` patch suggests a deliberate clamp rather than our
  number passed through. The prime suspect is the enumerated extension mask
  (sentinel set, only the two protocol extensions). Mesa checks the renderer's
  protocol knowledge of an extension before encoding its structs, and every
  1.3-core struct belongs to an extension that was promoted into 1.3. The
  answer is to read the guest's own `vn_physical_device.c` for 26.0.8 and a
  full, non-summary `vulkaninfo` dump.
- **No device extensions are advertised.** That is enough for this milestone
  and not enough for anything that presents: WSI, and Zink's GL on top of
  Vulkan, both need extensions, and the protocol has to be able to decode them
  before we may say so.
- **`vkGetPhysicalDeviceImageFormatProperties2` returned
  `VK_ERROR_FORMAT_NOT_SUPPORTED`** once during vulkaninfo's probing. That is a
  legitimate answer to a probe, but it should be checked against the host's own
  answer for the same query.

### Resolved — why the guest said 1.2, and why that is also the swapchain

Read from the guest's exact Mesa, 26.0.8 (Ubuntu's `-1ubuntu0.3` patches
nothing in venus):

- **The clamp.** `vn_physical_device_sanitize_properties` clamps the device to
  1.2 whenever `VK_KHR_synchronization2` is not exposed
  (`vn_physical_device.c:538-543`).
- **Why sync2 was missing.** It is a pass-through extension, so it needs our
  `vkEnumerateDeviceExtensionProperties` to list it, and ours is empty. On WSI
  builds, which Ubuntu's is, it is additionally gated on
  `renderer_sync_fd.semaphore_importable` (`:1262-1271`, `:1328`).
- **What that needs from us.** The guest sets `semaphore_importable` only when
  we list `VK_KHR_external_semaphore_fd` and answer
  `vkGetPhysicalDeviceExternalSemaphoreProperties(SYNC_FD)` with `IMPORTABLE`
  (`:1124-1141`).
- **The same gate hides `VK_KHR_swapchain`.** In 26.0.8,
  `semaphore_importable` also decides whether the guest exposes
  `VK_KHR_swapchain` at all (`:1212-1224`).

So the guest's Vulkan version and its ability to present rest on one thing:
**sync_fd semaphore import, which a Windows host does not have and our
renderer must emulate.** virglrenderer does it with a real sync_fd. We need:

- `vkImportSemaphoreResourceMESA(resourceId 0)` = "signal the semaphore now"
  (`vn_queue.c:398-414`), emulated by an empty signalling submit;
- `vkWaitSemaphoreResourceMESA` = "consume the pending payload"
  (`vn_queue.c:2489-2490`);
- a synthesized `SYNC_FD` properties reply;
- `VK_KHR_external_semaphore_fd` stripped from `vkCreateDevice`, and
  `VkExportSemaphoreCreateInfo{SYNC_FD}` stripped from `vkCreateSemaphore`,
  before they reach the host driver.

My hypothesis above, that the extension mask caused this, was wrong. The mask
is read in exactly one place, `vn_cs_renderer_protocol_has_extension`
(`vn_cs.h:102-105`). The generated guest encoders use it to decide whether to
*send* an extension's `pNext` structs, and silently drop the ones whose bit is
clear. A mask bit therefore obliges the renderer to decode, never to support,
and the real risk runs the other way: an extension we enumerate without its bit
has its structs dropped on the floor.

Two more 26.0.8 facts that shape the next stage:

- **`supports_multiple_timelines` is only asserted, and asserts are compiled
  out.** The guest always creates 64 rings and binds every queue to a
  `ring_idx` (`vn_renderer_virtgpu.c:1497-1500`, `vn_device.c:83-99`). Per-ring
  fences are owed whatever we advertise.
- **26.0.8 still uses fence feedback**: extra command buffers writing into
  `HOST_VISIBLE|HOST_COHERENT` memory (`vn_feedback.c:74-77`), plus async
  `vkWaitForFences`/`vkWaitSemaphores` that the renderer must truly block on
  (`vn_queue.c:1759`, `:2243`).

## Amendment, 2026-09-24 — stage 5b.1, device memory, buffers and images

The executor now allocates and frees device memory, creates buffers, binds
buffers and images, answers every memory-requirements query and creates buffer
and image views (`venus/executor/memory.rs`), and a `HOST3D` blob whose
`blob_id` names a `VkDeviceMemory` is **that memory's pages**. This is the
memory model the 2026-09-23 measurement chose, built.

- **Guest-visible memory is our pages, imported.** An allocation of a type the
  guest sees as `HOST_VISIBLE` (types 3 and 4 on the RTX 2070) is a
  `RingPages::for_memory` allocation — zeroed, aligned to the driver's
  `minImportedHostPointerAlignment`, rounded up to a multiple of it — checked
  against `vkGetMemoryHostPointerPropertiesEXT` for the guest's type index and
  imported with `VK_EXT_external_memory_host` (`HOST_ALLOCATION`). Every other
  type is a plain `vkAllocateMemory`, and no blob can be made of it.
- **The blob is the same `Arc`.** `RESOURCE_CREATE_BLOB` with a `blob_id`
  asks the factory (`SinkFactory::export_memory`) for that context's memory;
  it must be host-visible, the blob's size must be the allocation rounded to
  4 KiB (what the guest kernel sends), and — as in vkr — a memory is exported
  once. Mapping publishes exactly the blob's span of those pages
  (`RingPages::publish_len`), so the guest's mapping and the GPU's view are one
  set of bytes. `blob_id` 0 stays plain shared memory for rings and replies.
- **Lifetime.** The pages have three holders — the host memory object
  (`host_vulkan::HostMemory`, which releases its `Arc` only after
  `vkFreeMemory` has returned, and after `vkDeviceWaitIdle` for an import),
  the blob, and the publication — and are freed when the last goes, whichever
  order the guest frees in. Mesa's own order (unref the bo, then
  `vkFreeMemory` into the ring) races on the host by construction, and both
  orders are tested. A partition can never map pages the allocator has
  reused, and a GPU can never write pages the executor has let go of.
- **Budget.** 1 GiB of imported pages renderer-wide
  (`executor::MAX_HOST_VISIBLE_BYTES`, a `shmem::PageBudget` shared by every
  context), charged at allocation and refunded only when the pages are freed,
  so freeing memory while its blob stays mapped does not free budget. Past it
  an allocation answers `VK_ERROR_OUT_OF_DEVICE_MEMORY`. Device-local memory
  is the driver's to bound; a size past its heap gets the same answer before
  the driver is asked.
- **What a resource may be bound to.** Binding imported memory is valid only
  for a resource created for that handle type. Every buffer and image is
  created with a `VkExternalMemory*CreateInfo{HOST_ALLOCATION}` when the driver
  reports the handle type `IMPORTABLE` for it, and its `memoryTypeBits` name
  the host-visible types only then. On the RTX 2070 a transfer buffer may live
  in types 3 and 4 (`memoryTypeBits = 0x1b`); an image the driver will not
  import for never sees them. Every bind is judged against the same filtered
  bits, the offset against the alignment and the requirement against the
  guest's allocation size, before the driver is asked.
- **The capset's extension mask is now what the executor admits** — the two
  venus extensions plus every extension that adds a structure the chain
  policy admits (core 1.1–1.3), 60 in all, derived from the same table and
  rule (`policy::admitted_extension_numbers`). That includes
  `VK_KHR_synchronization2` (315) and `VK_KHR_dynamic_rendering` (45), on which
  the guest gates core-1.3 structures, and was checked against every
  `vn_cs_renderer_protocol_has_extension` gate in Mesa 26.0.8's driver
  headers: each gated structure the executor admits has its bit. It is
  narrower than virglrenderer's (everything its protocol decodes), because
  here a decoded structure outside the admitted set is fatal.
- **`vkWaitRingSeqnoMESA` is served** on the context stream: the device waits
  (bounded, 5 s) until the ring's `head` passes the seqno. Mesa sends it
  before creating the blob of memory it allocated without a reply
  (`vn_device_memory_wait_alloc`); without it the blob could arrive before the
  ring worker had executed the allocation.

Where this differs from vkr, beyond the memory model: every bind, view and
requirements query is validated before the driver sees it (vkr trusts the
driver); `bufferDeviceAddressCaptureReplay` is reported false, and so
`vkGetBufferOpaqueCaptureAddress`, `vkGetDeviceMemoryOpaqueCaptureAddress` and
any nonzero opaque capture address are refused; `vkGetDeviceMemoryCommitment`
is forwarded only for a lazily allocated type and answers 0 otherwise; a
ring cannot be built on a blob of Vulkan memory.

What Mesa 26.0.8 sends, and what it gets: `vkAllocateMemory` without a reply,
`VkMemoryAllocateFlagsInfo`, `VkMemoryDedicatedAllocateInfo` and an
`VkExportMemoryAllocateInfo` it has rewritten to handle types 0 (all served);
`VkImportMemoryResourceInfoMESA` only for a dma-buf import or guest vram,
neither of which exists here (`VK_ERROR_INVALID_EXTERNAL_HANDLE`, as vkr
answers a resource it cannot import); the blob lazily, at the first
`vkMapMemory`, as `HOST3D`/`MAPPABLE` with `blob_id` = the memory's id;
`vkCreateBuffer` + `vkGetBufferMemoryRequirements2` (with
`VkMemoryDedicatedRequirements`) or, on its requirements-cache hit, the create
alone; `vkBindBufferMemory2`, `vkBindImageMemory2`, `vkCreateImageView` and
`vkCreateBufferView` without replies; `vkGetDevice{Buffer,Image}MemoryRequirements`
(refused on a device below 1.3, where the entry point may not exist);
`vkGetImageSubresourceLayout` for linear images. A `vkBindImageMemory2` with no
memory is its WSI path and needs a swapchain nothing here offers yet.

**Measured on the RTX 2070** (driver 580.88, Windows,
`host_vulkan::tests::the_host_gpu_fills_a_buffer_and_the_guest_reads_it_through_the_blob`):
for each of types 3 and 4, a 64 KiB transfer buffer allocated and bound through
the executor, its memory's blob mapped into a window, the guest's bytes
overwritten by the **host GPU** (`vkCmdFillBuffer` through a test-only submit
path, `AshVulkan::fill_buffer`), and all 16384 words read back through the
blob's pages as the guest sees them, 0 wrong.

Owed by the next stages: queue submission (5b.3), whose command buffers are
what will use this memory, and the sync_fd semaphore emulation that unlocks
sync2 and the swapchain (the 2026-09-23 finding); `save`/`load` for memory,
which a snapshot still refuses by name while any host Vulkan object or any
imported page is alive.

## Amendment, 2026-09-24 — stage 5b.2, pipelines, command buffers and fenced submission

The executor now serves every core Vulkan 1.0–1.3 command a guest needs to
build pipelines, descriptors, render passes, framebuffers, query pools and
events, to record command buffers — every core `vkCmd*`, secondaries included —
and to submit them with an optional binary fence and wait for it. Command
streams too large for the ring arrive through `vkExecuteCommandStreamsMESA`,
which is served. Semaphores remain stage 5b.3.

### The mechanical majority is generated

`scripts/venus-exec-gen.py` (a sibling of `venus-ash-gen.py`, reading the same
`rust_protocol.py` model as the protocol, `vk.xml`, and ash's sources) writes
two files from a checked-in classification of all 211 core ≤ 1.3 commands the
protocol decodes (`tools/venus-protocol/executor-classes.txt`: 41 *bespoke* —
stages 5a.3/5b.1 —, 85 *hand-written*, 56 *generated*, 29 *refused*):

- `venus/executor/generated.rs`, portable and without `unsafe`: for the 141
  commands served by translation, the walk that replaces every guest id in the
  inputs — nested in structures, in arrays, in admitted pNext links — by the
  host handle, through a `Resolve` the context implements (typed, of the
  command's device, 0 only where vk.xml says `optional`/`noautovalidity`, with
  a short list of handles vk.xml lets be null that a driver would dereference:
  a stage's module, a pipeline's layout, `vkUpdateDescriptorSets`' `dstSet`,
  set layouts, bound sets, index and vertex buffers); every enum checked
  against the values core 1.0–1.3 defines and every flag word against the core
  bits (from `vk.xml`'s `<feature>` blocks, not from the extensions); every
  array against the count it travels with. Plus the tables the executor asks:
  a command's dispatchable, its core version, its `VkResult`, its output
  handles.
- `host_vulkan/calls.rs`: the one place a translated command becomes a driver
  call. Each structure is rebuilt as its `ash` twin in an arena
  (`host_vulkan/arena.rs`) — every pointer into an arena-owned copy that
  outlives the call, pNext links in the guest's order — and the entry point is
  called through `ash`'s function table; outputs are written back. The one
  `unsafe` contract is the translation's.

The *generated* commands are exactly that; the *hand-written* ones add what
only the context knows (`venus/executor/device_objects.rs`, `submit.rs`):
binding the guest's ids to what a create made and taking them out on destroy,
facts about objects later commands are judged by, bounds, submission's
bookkeeping. `--check` runs in CI beside the other two generators; the script
also refuses a classification that disagrees with the executor's `match` arms.

### Validation posture

1. **Typed ids everywhere**: an unknown id, one of another type, or one of
   another device is fatal to the context, however deep in a structure it is.
2. **Enum and flag ranges**: core Vulkan 1.3 values only — no device extension
   is advertised, so an extension's value is no correct guest's.
3. **Structural bounds** — counts, sizes, offsets into objects the renderer
   knows, and indices into the fixed-size state a driver keeps on the host:
   buffer ranges of fills, updates, copies, indirect draws and dispatches,
   vertex and index bindings, descriptor buffer ranges and query-result copies
   against the buffer's size; query ranges against the pool; descriptor writes
   and copies against the set layout's bindings (the consecutive-binding rule
   included, variable-count bindings at the count allocated); one dynamic
   offset per dynamic descriptor bound; sets against the pipeline layout;
   attachment references, preserve indices, dependencies and multiview arrays
   inside their render pass; a framebuffer made for as many attachments as its
   pass, and a clear value for every attachment a begin clears; viewports,
   scissors, vertex bindings and attributes, colour attachments and push
   constants within the device's limits; specialization entries inside their
   data; one shader stage of each kind; a command no newer than the device's
   version as the guest sees it (its entry point may not exist).
4. **Past that, the driver**, as in vkr: full valid-usage checking is not the
   goal. Two known edges: a buffer–image copy's footprint beyond its first
   byte (it depends on format and extent; a transfer is not covered by
   robustness either), and "ignored-if" pointers a driver reads anyway.
5. **Containment**: every host device is created with `robustBufferAccess`
   when it supports it, whatever the guest enabled, so a shader's stray buffer
   access stays inside its buffer. The guest is not told: it enabled what it
   enabled, and a robust device only behaves better. vkr enables none.

### Submission, fences and waits

What Mesa 26.0.8 sends (read from `vn_queue.c`, `vn_feedback.c`,
`vn_command_buffer.c`, `vn_ring.c`): a recording is encoded locally and sent at
`vkEndCommandBuffer` as one submission, through `vkExecuteCommandStreamsMESA`
when it is over the ring's 8 KiB direct size (as is any large single command);
`vkQueueSubmit` asynchronously (`vkQueueSubmit2` only with sync2, which the
guest does not have yet); **no semaphore on any plain submit** (only the
sparse path adds one); a fenced submit carries the fence's **feedback command
buffer**, recorded once at `vkCreateFence` (barrier, `vkCmdFillBuffer` of
`VK_SUCCESS` into the fence's slot of a host-visible feedback buffer, barrier
to `HOST`) and resubmitted unchanged; the guest polls that slot and, once it
reads signalled, sends an **asynchronous** `vkWaitForFences(1, fence, VK_TRUE,
UINT64_MAX)`; `vkGetFenceStatus` only without feedback; `vkQueueWaitIdle` and
`vkDeviceWaitIdle` never (vkr refuses both). All of it is served: the
feedback command buffers are just more command buffers writing memory the guest
maps (5b.1), and tested so, with the real GPU writing the slot the guest reads.

- **Waits are real, and sliced.** `vkWaitForFences` is waited for on the ring
  worker, in 20 ms slices with the context lock released between them, the
  guest's timeout honoured to the slice (`UINT64_MAX` is forever); a ring being
  torn down stops within a slice. The context's `ALIVE` monitor runs apart from
  the worker and keeps the guest's watchdog fed meanwhile (tested with a GPU
  that never finishes). `vkQueueWaitIdle`/`vkDeviceWaitIdle` are served from
  each queue's record of pending work.
- **Nothing is freed under the GPU.** Each queue records the fence of its
  newest fenced submit (which covers every earlier batch) and whether an
  unfenced submit followed. Every destroy, free and pool reset waits for that
  record to clear first — the fence, or the queue going idle — so a guest that
  destroys what a pending submission uses gets a wait, not a host driver
  freeing memory under the GPU; Mesa's own order (wait, then destroy) never
  waits. Device and context teardown wait for the device to go idle, as vkr.
- **A lost device** (`VK_ERROR_DEVICE_LOST` from any call) is answered — the
  command that met it is replied to and consumed — and the context is then
  fatal. The VMM stays up; the objects are destroyed as ever.
- **Pausing**: a slice-waiting ring holds its ADR-0005 pass for the whole wait,
  so a pause during one is the bounded "pausing anyway" of
  `Quiesce::wait_until_idle`. GPU work already submitted cannot be paused in
  any case; a snapshot still refuses while any host Vulkan object is alive.

### Where this differs from vkr

`vkExecuteCommandStreamsMESA` copies each stream before decoding (vkr decodes
in place while the guest may write), bounds the bytes one call names (64 MiB)
and refuses a dependency that does not point forward (vkr ignores them); waits
are sliced and `vkQueueWaitIdle`/`vkDeviceWaitIdle` are served; a lost device
ends the context; destroys wait for pending work; every check in the posture
above is ours; `vkFree{CommandBuffers,DescriptorSets}` must name the pool the
objects came from; a pipeline call that fails partway destroys what it did make
(vkr leaks it); `robustBufferAccess` is on; semaphores are refused (5b.3).

### Measured on the RTX 2070

Driven through a real ring with the generated driver-side encoder, exactly as
the guest would (`host_vulkan::pipeline_tests`, driver 580.88, Windows):
vk-smoke check 4 plus a fence feedback buffer — 16384 words through the blob,
0 wrong, the feedback slot reads `VK_SUCCESS`; check 5 — vk-smoke's compute
shader over 1 Mi elements, 0 wrong; check 6 — vk-smoke's triangle through a
classic render pass, **256×256 exact, `fnv1a=0x2678f2a0e39fba1b`**, the
checksum vk-smoke's README gives for the bare RTX 2070; a 293 872-byte shader
module through `vkExecuteCommandStreamsMESA`, run over 4096 elements, 0 wrong;
200 fenced submits, each waited for as Mesa waits, no word lost.

### vk-smoke in the guest

Checks 4, 5, 6 and 9 need nothing past this stage; 7 skips (the guest reports
1.2). **Check 8 creates a timeline semaphore** — asynchronously, so its refusal
makes the context fatal and the guest's next ring wait finds the ring dead —
so run `--checks 4,5,6,7,9` to see 9; in the default order 8 ends the run.
Nothing before check 8 names a semaphore.

## Amendment, 2026-09-24 — a Linux guest renders on the host GPU, with exact pixels

The guest acceptance for 5b.1 and 5b.2 was `guest/vk-smoke` inside the Ubuntu
guest (Mesa 26.0.8 venus, root, fetched over usernet) against the executing
renderer (commit `3f68f67`), run with `--checks 1,2,3,4,5,6,7,9`. Check 8
needs semaphores, which are stage 5b.3, and refusing them ends the context:

```
SMOKE 1 instance     PASS  "Virtio-GPU Venus (NVIDIA GeForce RTX 2070)" apiVersion=1.2.0
SMOKE 2 device       PASS
SMOKE 3 host-memory  PASS  1 MiB, type 3, write/unmap/remap/read back
SMOKE 4 transfer     PASS  fill x3 + update + 2-region copy of 64 KiB verified
SMOKE 5 compute      PASS  1048576 elements, all f(i) correct
SMOKE 6 graphics     PASS  256x256 exact, fnv1a=0x2678f2a0e39fba1b
SMOKE 7 dynamic-rendering  SKIP  (guest reports 1.2 — stage 5b.3)
SMOKE 9 many-submits PASS  1000 submits, 1000 fences, none lost
SMOKE DONE pass=7 fail=0 skip=2
```

After 5b.1 alone, checks 1–3 passed and the renderer refused
`vkAllocateCommandBuffers` by name, exactly where that stage ended.

The triangle's checksum is **the one the same binary produces on the bare RTX
2070 on the host**, so the guest's pixels are bit-identical to native. The
renderer logged no refusal.

### Observed, not yet understood: GPU time

GPU time from the smoke test's own timestamps is far higher in the guest than
native: 24 ms against 0.64 ms for compute, and 12.9 ms against 0.20 ms for the
triangle. The prime suspect is placement. The 5a.3 memory policy hides
`HOST_VISIBLE` on the BAR type, so every buffer the guest wants mapped lands in
system memory (type 3), and the GPU reads and writes it across PCIe. The fix,
if that is it, is a real workload decision: which types to expose, and whether
a guest's storage buffers belong in memory the guest never maps. This is a
performance item, not a correctness one.

## Amendment, 2026-09-24 — stage 5b.3, semaphores, sync-file emulation, queue timelines and Vulkan 1.3

The executor now serves semaphores, the sync-file semaphore import Mesa's WSI
rests on, and virtio-gpu fences on every queue's `ring_idx` timeline, and it
advertises `VK_KHR_synchronization2`. Against Mesa 26.0.8 that is what turns
the guest's device into **Vulkan 1.3 with `VK_KHR_swapchain`**: the three
gates of "Resolved — why the guest said 1.2" are now open.

### What the guest is shown

| extension | how | what it obliges the renderer to |
|---|---|---|
| `VK_KHR_synchronization2` | passed through, on a device of 1.3 or newer | every command it adds is core 1.3 (`vkQueueSubmit2`, `vkCmdPipelineBarrier2`, `vkCmd{Set,Reset,Wait}Event{s}2`, `vkCmdWriteTimestamp2`, the KHR aliases encoded as the core commands), all served; its structures are core 1.3 (`VkDependencyInfo`, `Vk*MemoryBarrier2`, `VkSubmitInfo2`, `VkSemaphoreSubmitInfo`, `VkCommandBufferSubmitInfo`, `VkPhysicalDeviceSynchronization2Features`) and admitted; capset bit 315, already set by 5b.1 |
| `VK_KHR_external_semaphore_fd` | **emulated**, advertised whatever the host has | the `SYNC_FD` answer below; stripped from `vkCreateDevice` (Mesa adds it for every device an application wants a swapchain on, `vn_device.c:333-337`) and `VkExportSemaphoreCreateInfo{SYNC_FD}` stripped from `vkCreateSemaphore`, before the driver sees either; `vkImportSemaphoreResourceMESA` and `vkWaitSemaphoreResourceMESA`. Its own two commands are never sent (Mesa implements them itself) and it chains no structure, so it needs no capset bit |

`vkGetPhysicalDeviceExternalSemaphoreProperties(SYNC_FD)` answers
**`IMPORTABLE` only** for a binary semaphore (compatible type `SYNC_FD`,
nothing exportable), and nothing for a timeline one; every other handle type
gets the host driver's own answer. `IMPORTABLE` is what sets
`renderer_sync_fd.semaphore_importable` (`vn_physical_device.c:1124-1141`),
and that one flag is the gate on sync2 (`:1262-1271`, and with it 1.3,
`:538-543`) and on the swapchain (`:1212-1224`). `EXPORTABLE` would also make
the guest offer `VK_KHR_external_semaphore_fd` to its applications
(`:1173-1179`), whose `vkGetSemaphoreFdKHR` exports through a fence on the
queue's timeline and `vkWaitSemaphoreResourceMESA`; both are implemented and
tested, but nothing in 1.3 or the swapchain needs an application to export a
sync file, so the promise is not made. It is one constant to flip.

The executor's version gate is now the guest's version as Mesa 26.0.8 will
show it — the host's capped at 1.3, and 1.2 when sync2 is not advertised — so
a core-1.3 command passes exactly when the guest can have one of its own to
send. On the RTX 2070 that is 1.3 (`apiVersion 1.3.312` from the host's 1.4;
the guest clamps it to 1.3.0 because protocol spec 2 cannot carry host image
copy, `:535-536`). Features2 and Properties2 answer every 1.3 structure from
the host's chain (`VkPhysicalDeviceVulkan13Features`/`Properties`, and the
individual sync2, dynamic-rendering and maintenance4 structures).

### What the guest sends on the WSI and sync-file paths, and what each becomes

Read from 26.0.8's `vn_queue.c` and `vn_wsi.c`. Without a dma-buf export the
guest's common WSI runs in its software mode (`vn_wsi.c:134-139`): images are
copied by the CPU, and the renderer sees ordinary submits and fences.

| guest | when | here |
|---|---|---|
| `vkCreateSemaphore` (binary; timeline with `VkSemaphoreTypeCreateInfo`; `VkExportSemaphoreCreateInfo` if the application asked) | async | created; `SYNC_FD` stripped from the export, any other type must be one the host exports |
| `vkQueueSubmit`/`vkQueueSubmit2` with waits and signals | async; `Submit2` once the device is 1.3 (`vn_device.c:554`) | translated; timeline values and device-group indices checked against the counts a driver indexes by; each binary semaphore's state tracked (below) |
| `vkImportSemaphoreResourceMESA`, resource 0 | before a submit that waits on a semaphore whose temporary payload was an imported sync file the guest already waited for itself (`vn_queue.c:387-417`) — every acquired swapchain image | **recorded, not performed**: the semaphore has a signalled temporary payload, and the next wait consumes it — a submit's wait on it is taken out of the batch before the host sees it |
| `vkWaitSemaphoreResourceMESA` | `vkGetSemaphoreFdKHR` (`:2440-2495`), which needs an exportable renderer (not offered) | the temporary payload consumed if there is one, otherwise the permanent one with an empty submit that waits on it |
| execbuffer with a fence on the queue's `ring_idx`, carrying `vkWaitRingSeqnoMESA` | `vn_create_sync_file`, the same exports | a host fence on the bound queue, retired by the queue's fence thread |
| `vkWaitSemaphores(UINT64_MAX)` async, `vkGetSemaphoreCounterValue`, `vkSignalSemaphore` | timeline feedback read signalled; feedback off; host signal | a real wait in 20 ms slices off the context lock, like `vkWaitForFences`; passed through; timeline semaphores only |
| `vkImportFenceResourceMESA`, `vkResetFenceResourceMESA` | never; only with an exportable sync-file fence, not advertised | refused |

Why the import is bookkeeping and not the "empty signalling submit" the
2026-09-23 finding suggested: vkr imports a sync file of `-1` as a
*temporary* payload, and a signalling submit gets that wrong three ways — it
waits behind the queue's earlier work, it changes the *permanent* payload a
temporary import must leave alone, and when the permanent payload is already
signalled it is a signal of a signalled binary semaphore, which a driver need
not survive. Dropping the consuming wait is exactly what waiting on a
signalled temporary payload means. The same per-semaphore record refuses a
binary wait with no signal submitted before it (a GPU that waits forever) and
a second signal of a signalled binary semaphore, before the driver sees them.

### Fences on a queue's timeline, and the waiter model

`virtio_gpu::fence` keeps one FIFO per timeline — the device's
(`VIRTIO_GPU_FLAG_INFO_RING_IDX` clear: every virgl fence, unchanged) and one
per `(context, ring_idx)` — in one bounded table, so a retirement completes
only its own timeline's prefix and the cap, the watchdog and a reset's drain
stay what they were. A renderer names the timeline through additive
`Renderer3d` methods whose defaults are the old behaviour, so virgl's fences
are untouched. `ring_idx` 0 is the context's CPU timeline and, as in vkr, is
signalled at once (the context commands before it have run). Every other
`ring_idx` goes to the executor: an empty `vkQueueSubmit` with a host fence
on the queue bound to it (a fence on a timeline no queue is bound to is
refused, as vkr refuses it, and the device answers it at once), handed to
**that queue's fence thread**, started by its first fence, which waits for
the FIFO's head in 50 ms slices, destroys the host fence and records the
retirement for the device, then wakes it. As vkr: one thread per queue,
because the ring workers must not block and one `vkWaitForFences` covers one
device and one fence at a time; a single poller would add latency to every
fence or leave a signalled one waiting behind another queue.

- **ADR-0005.** The fence thread touches no guest memory and takes no lock
  the executor or the device holds; the guest sees a retirement only when the
  device's (gated) queue worker writes the held response, so it takes no pass,
  and a pause neither waits for it nor breaks it. `vkDestroyDevice`, context
  destruction and reset stop and join every fence thread of the device (one
  slice), wait for the device to go idle, then destroy the fences still queued
  and retire them in order, as vkr does when a queue goes; a reset then drops
  every retirement of the old boot, so none can complete a new boot's fence of
  the same id. Tested on a paused VM.
- **ADR-0006.** A snapshot is refused by name while any ring fence is queued
  or retired and not yet collected, before the executor's own refusal of live
  host objects.
- The capset's `supports_multiple_timelines` is now true for the executing
  renderer (false for a capture, which has no queue); Mesa only asserts it.

### Core 1.3 coverage

Every core 1.3 command the protocol decodes is served except two:
`vkGetDeviceImageSparseMemoryRequirements` (every sparse feature is reported
false) and `vkGetPhysicalDeviceToolProperties` (answered by the guest driver
itself). Of the rest, `vkQueueSubmit2`, `vkCmdBeginRendering`,
`vkCmdBindVertexBuffers2`, `vkCmdCopy{Buffer,BufferToImage,ImageToBuffer}2`,
`vkCmdSet{Viewport,Scissor}WithCount`, `vkCmdWriteTimestamp2` and the private
data commands are hand-written with bounds; `vkGetDevice{Buffer,Image}MemoryRequirements`
are bespoke (5b.1); the rest pass through the generated translation. Dynamic
rendering is bounded as a render pass is: colour attachments inside
`maxColorAttachments` in `vkCmdBeginRendering`, `VkPipelineRenderingCreateInfo`
and an inherited `VkCommandBufferInheritanceRenderingInfo`
(`vkBeginCommandBuffer` is hand-written for it), a view mask inside
`maxMultiviewViewCount`, a layer count and a render area; every view is a
typed id of the device.

### Measured on the RTX 2070

Driven through a real ring with the generated driver-side encoder, as the
guest would (`host_vulkan::pipeline_tests`, driver 580.88, Windows): the
device shown as `apiVersion 1.3.312` with `VK_KHR_synchronization2` and
`VK_KHR_external_semaphore_fd`, `SYNC_FD` features `0x2`; vk-smoke check 7 —
the triangle through `vkCmdBeginRendering`, its transitions through
`vkCmdPipelineBarrier2`, submitted with `vkQueueSubmit2` — **256×256 exact,
`fnv1a=0xd79d631c4d62403b`**, the checksum of the bare RTX 2070; check 8 — a
timeline semaphore across two submits, the host's wait for 2 in 7.1 ms,
counter 2, 0 words wrong, then `vkQueueWaitIdle` and `vkDeviceWaitIdle`; the
WSI sequence (three frames of import, render signalling a binary semaphore,
the present's `vkQueueSubmit2` waiting on it with a fence, then a sync-file
export) with no refusal; and a fence on timeline 1 retired 82 ms after a
submit of eight 32 MiB fills, with all 8 388 608 words already written.

Owed: the guest acceptance (vk-smoke all nine checks with the guest at 1.3,
and `vulkaninfo` listing `VK_KHR_swapchain` and `VK_KHR_synchronization2`),
and presenting through a real swapchain, which in the guest's software WSI
is CPU copies — correct, and slow.

## Amendment, 2026-09-24 — Vulkan 1.3 in the guest, all nine checks, and a swapchain

Guest acceptance for 5b.3, run the same way as the one above (commit
`06b9754`, full `vk-smoke`, no `--checks`):

```
SMOKE 1 instance          PASS  "Virtio-GPU Venus (NVIDIA GeForce RTX 2070)" apiVersion=1.3.0
SMOKE 2 device            PASS  timeline_semaphore=core, dynamic_rendering=core
SMOKE 3 host-memory       PASS
SMOKE 4 transfer          PASS
SMOKE 5 compute           PASS  1048576 elements, all f(i) correct
SMOKE 6 graphics          PASS  fnv1a=0x2678f2a0e39fba1b
SMOKE 7 dynamic-rendering PASS  fnv1a=0xd79d631c4d62403b
SMOKE 8 timeline-sync     PASS  A signals 1, B waits 1 and signals 2, counter=2
SMOKE 9 many-submits      PASS  1000 submits, 1000 fences, none lost
SMOKE DONE pass=9 fail=0 skip=0
```

The guest's own `vulkaninfo` now reports `apiVersion = 1.3.0` for the venus
device and lists **`VK_KHR_swapchain`** (revision 70) and
`VK_KHR_synchronization2`. That is the gate diagnosed above, opened by the
sync_fd emulation. Both triangle checksums match the bare RTX 2070 on the
host. The renderer logged no refusal.

`1.3.0`, not `1.3.x`, is Mesa's own clamp for venus protocol spec version 2
(`vn_physical_device.c:535-536`). Going past it needs protocol v3 and its
host-image-copy obligations, which nothing needs yet.

With a swapchain exposed, a Vulkan application in the guest can now present.
Without dma-buf, Mesa's WSI takes its software path (`vn_wsi.c:134`): it renders
on the GPU and copies the result into shared memory for the guest's display
server. That is the next thing to measure.

## Amendment, 2026-09-24 — vkcube on the guest's desktop, rendered by the host GPU

The first real Vulkan *application* in the guest. Ubuntu 26.04 GNOME, logged
in automatically on `seat0` under Wayland; `vkcube --wsi wayland` run as the
session's user:

```
Selected GPU 0: Virtio-GPU Venus (NVIDIA GeForce RTX 2070), type: DiscreteGpu
```

It stayed up for the whole run (several minutes), the renderer logged no
refusal, and the VMM's own screenshot shows the textured LunarG cube
spinning in a window on the GNOME desktop. The path is this ADR's whole
stack on WHP: the guest's Mesa venus driver, our transport and ring, the
generated protocol, the executor on the RTX 2070, and our imported pages
mapped through WHP. Because we export no dma-buf, Mesa's WSI takes its
software path (`vn_wsi.c:134`): the GPU renders, the frame is copied into
`wl_shm`, and the guest's own compositor puts it on the existing 2D scanout.

### The bug the first run found

The first attempt died at once. We refused `vkCreateCommandPool` on queue
family 1, because the device had been created with a queue in family 0 only.
That check was stricter than the spec: `queueFamilyIndex` need only name one of
the *physical* device's families
(`VUID-vkCreateCommandPool-queueFamilyIndex-01937`). Mesa's WSI relies on
exactly that — `wsi_swapchain_init` makes a blit pool for every family,
whether a queue exists on it or not. The check now follows the spec, and a
protected pool still needs a protected queue on its family. The case is pinned
by `a_command_pool_may_name_any_family_of_the_physical_device_as_mesa_wsi_does`.

It is the first bug a real *application* found that none of our tests,
the guest-side smoke test included, could have found. That is the argument
for running real applications next, rather than growing the smoke test.

### What this does not yet show

- **GNOME itself is not on the GPU.** `gnome-shell` maps only `dri_gbm.so`;
  its GL runs in software. A composited desktop on the GPU means GL on
  Vulkan (Zink) over venus, and Zink with GBM/KMS needs dma-buf-shaped
  exports that this renderer does not make.
- **Every frame crosses the CPU twice**: guest WSI copy into `wl_shm`, then
  the compositor's scanout upload on the host. It is correct, and far from
  the zero-copy path the 2026-09-16 amendments measured.
- **Frame rate** has not been measured.

## Amendment, 2026-09-24 — OpenGL is not on the GPU yet, and why

`glmark2-wayland` with `MESA_LOADER_DRIVER_OVERRIDE=zink` ran for minutes at
roughly 280 FPS, which looked like success. It was not: its own
`GL_RENDERER` line says `llvmpipe (LLVM 21.1.8, 256 bits)`, and so does
`eglinfo` for every profile. The override failed silently and GL fell back
to software. The lesson is the one ADR-0004 recorded for `VN_DEBUG`: read
the renderer string before believing a frame rate.

Mesa said why when asked (`MESA_LOG_LEVEL=debug`, surfaceless EGL):
`ZINK: failed to choose pdev`. Read from Mesa 26.0.8:

- **The DRM identity.** On the EGL/GBM path, zink is handed the render
  node and picks the Vulkan device whose `VkPhysicalDeviceDrmPropertiesEXT`
  names that node (`zink_screen.c:1660-1685`, `:1736-1777`). Venus normally
  reports the virtgpu node, but `vn_wsi_init` zeroes the DRM and PCI identity
  and hides `EXT_physical_device_drm` whenever `vendorID == 0x10de`
  (`vn_wsi.c:155-174`). That quirk exists for a real NVIDIA GPU visible to a
  guest's WSI. We pass the host's vendor ID through, so zink never finds its
  device.
- **Then extension *strings*.** zink requires `VK_KHR_maintenance1`,
  `create_renderpass2`, `imageless_framebuffer`, `dynamic_rendering` and
  `descriptor_update_template` by name. Core 1.3 promotion does not count
  (`zink_device_info.py:506-525`). It also requires `nullDescriptor` from
  robustness2 (`zink_screen.c:3458-3461`).
- **Then `VK_KHR_external_memory_fd`** (`zink_screen.c:3862-3866`), which
  venus exposes only if the renderer advertises
  `VK_EXT_external_memory_dma_buf` (`vn_physical_device.c:1040-1051`).
- **GL version gates** beyond that: transform feedback, depth clip and
  vertex divisor for 3.3; the `maintenance2` string for 4.0; a *reported*
  `robustBufferAccess` for 4.3; the `draw_indirect_count` string for 4.6.

Separately, venus decides software WSI (the `wl_shm` path vkcube uses) from
`driverID`/`driverVersion`, not from `vendorID`: NVIDIA below 590.48.1, or no
dma-buf advertised (`vn_wsi.c:134-139`). Advertising an emulated dma-buf would
therefore flip WSI onto a path this renderer cannot serve the day the host
driver reaches 590.48, unless the reported version is held below it.

Stage 5c takes these in order: report the virtio vendor ID (`0x1af4`) for an
NVIDIA host, hold the reported driver version under the dma-buf-WSI line,
advertise and serve the extensions above, and emulate
`VK_EXT_external_memory_dma_buf` over our own pages.

## Amendment, 2026-09-24 — stage 5c: the identity, Zink's extensions, and dma-buf over our pages

Stage 5c is the list above, built. None of it is guest acceptance yet: the
RTX 2070 runs every new path through a real ring (below), and `eglinfo` and
`glmark2-wayland` under `MESA_LOADER_DRIVER_OVERRIDE=zink` are the next
measurement. Two citations above, corrected from 26.0.8: the five extensions
Zink requires by name are declared at `zink_device_info.py:62-63, 93-94,
180-183, 203-206, 313-314` and refused at `:755-758`, and
`zink_get_display_device` is `zink_screen.c:1666-1685`.

### The identity (`policy::shape_identity`)

- **`vendorID` `0x10de` is shown as `0x1af4`**, the virtio PCI vendor —
  Mesa's `VIRTGPU_PCI_VENDOR_ID`, what the guest's render node really is
  (`vn_renderer_virtgpu.c:44`, `:1461`). `deviceID`, `deviceName`,
  `driverID` and `driverVersion` stay the host's. The `vn_wsi_init` quirk is
  for a real NVIDIA GPU visible to the guest's window system; ours is a
  virtual device, and what the NVIDIA workarounds of Zink and of venus itself
  key on is `driverID` (`zink_screen.c:2943`, `vn_query_pool.c:135`), which
  is kept. With the quirk not triggered, venus reports the virtgpu node's DRM
  numbers and `EXT_physical_device_drm`, and Zink's match has something to
  match.
- **An NVIDIA `driverVersion` at or past 590.48.01 is shown as 590.48.0.0**
  (`VN_MAKE_NVIDIA_VERSION`, `vn_common.h:76-78`: 10, 8, 8 and 6 bits; the
  RTX 2070's 580.88 is `0x91160000`, which its `vulkaninfo` prints as
  2434138112). Venus keeps its software WSI for an NVIDIA driver below that
  line even when the renderer lists dma-buf (`vn_wsi.c:134-139`), and the
  guest shows applications its own `driverVersion` anyway
  (`vn_physical_device.c:550-554`). Today's 580.88 is untouched. **Removing
  the cap is the switch that turns dma-buf WSI on**, once this renderer has a
  dma-buf path to present through.
- **A non-NVIDIA host is not offered the dma-buf pair**
  (`policy::keeps_software_wsi`, `advertised_extensions_on`). For any other
  driver the same line of `vn_wsi.c` puts venus's WSI on its native dma-buf
  path the moment dma-buf is listed, and that path exports device-local,
  optimal-tiling swapchain images this renderer cannot make: advertising it
  there would trade a working swapchain for Zink's DRM screen. On such a host
  Zink stays on the guest's software GL until dma-buf WSI exists. This
  refines the stage-5c design, which named only the NVIDIA case.

### What the guest is shown (`policy`)

On the RTX 2070 (Windows, 580.88), from
`host_vulkan::pipeline_tests::the_host_gpu_is_shown_what_zink_needs`: **71
device extensions**, `apiVersion 1.3.312`, vendor `0x1af4`, device `0x1f02`,
`driverVersion 0x91160000`.

| kind | extensions | what it obliges the renderer to |
|---|---|---|
| promoted to 1.1–1.3, passed through (`PROMOTED_EXTENSIONS`: 58, of which the RTX 2070 has 57 — not `EXT_texture_compression_astc_hdr`) | 1.1: `16bit_storage`, `bind_memory2`, `dedicated_allocation`, `descriptor_update_template`, `external_{fence,memory,semaphore}`, `get_memory_requirements2`, `maintenance1/2/3`, `multiview`, `relaxed_block_layout`, `sampler_ycbcr_conversion`, `shader_draw_parameters`, `storage_buffer_storage_class`, `variable_pointers`; 1.2: `8bit_storage`, `buffer_device_address`, `create_renderpass2`, `depth_stencil_resolve`, `draw_indirect_count`, `driver_properties`, `image_format_list`, `imageless_framebuffer`, `sampler_mirror_clamp_to_edge`, `separate_depth_stencil_layouts`, `shader_atomic_int64`, `shader_float16_int8`, `shader_float_controls`, `shader_subgroup_extended_types`, `spirv_1_4`, `timeline_semaphore`, `uniform_buffer_standard_layout`, `vulkan_memory_model`, `EXT_descriptor_indexing`, `EXT_host_query_reset`, `EXT_sampler_filter_minmax`, `EXT_scalar_block_layout`, `EXT_separate_stencil_usage`, `EXT_shader_viewport_index_layer`; 1.3: `copy_commands2`, `dynamic_rendering`, `format_feature_flags2`, `maintenance4`, `shader_integer_dot_product`, `shader_non_semantic_info`, `shader_terminate_invocation`, `synchronization2`, `zero_initialize_workgroup_memory`, `EXT_image_robustness`, `EXT_inline_uniform_block`, `EXT_pipeline_creation_cache_control`, `EXT_pipeline_creation_feedback`, `EXT_private_data`, `EXT_shader_demote_to_helper_invocation`, `EXT_subgroup_size_control`, `EXT_texture_compression_astc_hdr` | nothing new: every command is core in that version and encoded as the core command, every own structure is core and admitted — the rule the list is drawn up by, from vk.xml. Out by the same rule: `KHR_device_group` (swapchain interactions), and `EXT_4444_formats`, `EXT_extended_dynamic_state`, `EXT_extended_dynamic_state2`, `EXT_texel_buffer_alignment`, `EXT_ycbcr_2plane_444_formats` (feature structures that were not promoted). `descriptor_update_template` is in because its one non-core command needs `KHR_push_descriptor`, not offered. Each only on a device of its version or newer |
| admitted, passed through (`ADMITTED_EXTENSIONS`) | `EXT_robustness2` (`KHR_` where the host has it; the RTX 2070 has not), `EXT_transform_feedback`, `EXT_conditional_rendering`, `EXT_line_rasterization` and `KHR_`, `EXT_vertex_attribute_divisor` and `KHR_`, `EXT_depth_clip_enable`, `EXT_provoking_vertex`, `EXT_custom_border_color`, `EXT_border_color_swizzle` | their 25 structures, each admitted only on a device that enabled an extension bringing it; their feature and property structures queried from the host (gated on the host reporting the extension — the host instance is 1.3, and several are 1.4 names); their commands served (below); their enum values and flag bits legal once enabled |
| emulated | `KHR_external_semaphore_fd` (5b.3), `EXT_external_memory_dma_buf`, `KHR_external_memory_fd` | the emulation below; all three stripped from a device create before the host sees it. `KHR_external_memory_fd` is listed because venus adds it with dma-buf to every device create that wants a swapchain or an fd (`vn_device.c:318-330`) |

The capset's enumerated mask is still derived from the admitted structures:
72 extensions (60, and the twelve admitted device extensions). The generators
read `ADMITTED_EXTENSIONS` out of `policy.rs`, so the bridge, the translation
and the policy are one list. `robustBufferAccess` is *reported* as the host
reports it (Zink's GL 4.3 gate), and forced on every host device as before,
which `robustBufferAccess2` needs.

### Commands and bounds

The nine new commands are classified in `executor-classes.txt` and go
through the generated translation and host call like every other; the host
call reaches the driver through `calls::ExtTables`, the `ash` tables of the
admitted extensions the host device was created with. An extension command
on a device that enabled none of its extensions is refused
(`ExecError::NotEnabled`), and so are its structures (the generated chain
walk) and its values (generated `x_`/`m_` helpers over `Resolve::enabled`,
from vk.xml's extension `<require>` blocks and their `depends` — so sync2's
transform-feedback stage and access bits are legal exactly with transform
feedback).

| command | bounded by hand |
|---|---|
| `vkCmdBindTransformFeedbackBuffersEXT` | bindings inside `maxTransformFeedbackBuffers`; each buffer `TRANSFORM_FEEDBACK`, its offset 4-aligned inside it, its range inside it and inside `maxTransformFeedbackBufferSize` |
| `vkCmd{Begin,End}TransformFeedbackEXT` | counter slots inside the limit; each non-null counter 4 aligned bytes inside a `TRANSFORM_FEEDBACK_COUNTER` buffer |
| `vkCmd{Begin,End}QueryIndexedEXT` | the query inside its pool; the index a stream the device has for a stream query, 0 otherwise (a stream pool's results are two values a query) |
| `vkCmdDrawIndirectByteCountEXT` | the counter 4 aligned bytes inside its buffer; a stride in `1..=maxTransformFeedbackBufferDataStride` |
| `vkCmdBeginConditionalRenderingEXT` | the predicate 4 aligned bytes inside a `CONDITIONAL_RENDERING` buffer (`End` is generated) |
| `vkCmdSetLineStipple` | a factor in `[1, 256]`, as in `VkPipelineRasterizationLineStateCreateInfo` |

Beyond the commands: a rasterization stream inside
`maxTransformFeedbackStreams`; vertex divisors naming bindings inside
`maxVertexInputBindings`, divisors inside `maxVertexAttribDivisor`; no more
live custom-border-colour samplers than `maxCustomBorderColorSamplers`, a
driver's fixed table; with robustness2's `nullDescriptor`, a null image
view, texel view, buffer (offset 0, whole range) or vertex buffer (offset 0)
is the guest's to send — Zink binds one for every unbound slot — and without
it they are refused as before. A feature structure of an extension the guest
did not enable is judged, and never forwarded.

### `VK_EXT_external_memory_dma_buf`, emulated over our pages

A dma-buf here is what a blob of this renderer already is: our pages.

- **Queries.** `vkGetPhysicalDeviceExternalBufferProperties(DMA_BUF)`
  answers `EXPORTABLE | IMPORTABLE`, compatible with and exportable from
  `DMA_BUF`, exactly when the host would import our pages for such a buffer
  (`HOST_ALLOCATION` `IMPORTABLE`), and nothing otherwise; every other handle
  type is answered nothing. An image query with `DMA_BUF` — which 26.0.8
  answers itself as unsupported for every tiling but DRM modifiers
  (`vn_physical_device.c:2812-2817`), so only another guest driver would send
  it — is asked of the host as `HOST_ALLOCATION` and answered the same way,
  or `VK_ERROR_FORMAT_NOT_SUPPORTED`.
- **Resources.** `VkExternalMemory{Buffer,Image}CreateInfo{DMA_BUF}` is
  accepted on a device that enabled dma-buf (any other handle type is fatal).
  The host resource is created as every resource is, for host allocations
  when the host allows, and one that can take our pages then asks for them
  alone in its `memoryTypeBits`. Zink creates every shared image this way,
  with the `OPAQUE_FD` venus rewrites to `DMA_BUF`, and asks no format query
  first (`zink_resource.c:1336-1340`, `:1504`).
- **Exports.** `VkExportMemoryAllocateInfo{DMA_BUF}` is an ordinary
  allocation. On a host-visible type it is our pages, and the blob Mesa makes
  of it at once (`vn_device_memory_alloc_export`) is those pages, as for any
  mapped memory. On any other type there is nothing to share: the memory is
  made, its blob refused, and the guest's `vkAllocateMemory` answers
  `VK_ERROR_OUT_OF_DEVICE_MEMORY` and frees it — refused in Vulkan terms. (Not
  making the memory would make the guest's following free fatal.)
- **Imports.** `vkGetMemoryResourcePropertiesMESA` and
  `VkImportMemoryResourceInfoMESA` take a blob of `VkDeviceMemory` of this
  renderer that the context made or is **attached** to, and nothing else
  (`VK_ERROR_INVALID_EXTERNAL_HANDLE`, vkr's answer for a resource it cannot
  import; vkr is fatal for a resource the context does not hold, which a
  guest cannot tell from one not attached yet). The import is a new
  `VkDeviceMemory` importing **the same pages** with
  `VK_EXT_external_memory_host`, as a host-visible type the host accepts them
  for, no larger than the blob; no blob is made of it again. Cross-context
  sharing reaches the renderer as `CTX_ATTACH_RESOURCE`, which the guest
  kernel sends when another process opens a GEM handle of the dma-buf: the
  device used to answer a blob's attach alone, and now tells the renderer too
  (`Renderer3d::ctx_attach_blob`, a no-op by default). Memory blobs live in
  the blob directory beside rings and reply windows, and can never be bound
  as either.
- **Lifetime.** The import holds the pages' `Arc`, taken under the
  directory lock, like the exporting memory, its blob and its publication;
  the pages go when the last holder does, and the budget is charged once, at
  the first allocation. The exporter may free its memory, destroy its blob or
  its whole context first: the importer's GPU keeps writing pages that exist,
  and a partition never maps pages the allocator reused (tested in that
  order). A detach afterwards stops new imports and changes nothing for one
  made.
- **Left out**: `EXT_image_drm_format_modifier` and `EXT_queue_family_foreign`
  (GNOME on the GPU is a later stage). Without the first Zink has no dma-buf
  modifier queries and cannot export an image it did not create exportable
  (`zink_screen.c:3613`, `zink_resource.c:1960-1962`); without the second its
  `dmabuf` capability is 0 (`zink_screen.c:1128-1136`), so it offers no PRIME
  import or export — EGL dma-buf image import and GBM buffer sharing are off
  — while rendering into its own images and presenting through kopper's
  (software) Vulkan swapchain are not affected.

### Measured on the RTX 2070

Driven through real rings with the generated driver-side encoder
(`host_vulkan::pipeline_tests`, driver 580.88, Windows):

- **Transform feedback.** A hand-assembled SPIR-V vertex shader (`Xfb`,
  `XfbBuffer 0`, stride 16) writing `(i, 2i, 7, 1)`, one triangle with
  rasterizer discard under dynamic rendering with no attachment, captured
  into host-visible memory: `[0,0,7,1, 1,2,7,1, 2,4,7,1]`, nothing past it,
  and the end counter at 48 bytes.
- **Conditional rendering.** vk-smoke's compute shader over 4096 elements
  inside `vkCmdBeginConditionalRenderingEXT`, the predicate written through
  the blob: 0 — all 4096 untouched; 1 — all 4096 written.
- **dma-buf across two contexts.** Context 1 exports 64 KiB for a `DMA_BUF`
  buffer and writes a pattern through its blob; context 2, attached, imports
  it on its own device, copies it into a buffer of its own and fills the
  import: 16384 words of context 1's read by context 2, 0 wrong, and context
  2's fill read back through context 1's blob, 0 wrong. One set of pages, two
  `VkDevice`s importing it.

### What the guest should report

Zink's own gates for GL 4.6 are met on this device as the guest will see it
(`draw_indirect_count`, transform feedback with 4 streams and 4 buffers,
`robustBufferAccess` with `robustImageAccess2`, the `maintenance2` string,
depth clip, vertex divisors). The expectation for the guest acceptance:
`eglinfo` names `zink Vulkan 1.3(Virtio-GPU Venus (NVIDIA GeForce RTX 2070)
(MESA_VENUS))` with OpenGL 4.6 core and compatibility profiles and OpenGL ES
3.2 — to be read from the run, not assumed.

## Amendment, 2026-09-24 — OpenGL runs on the host GPU; a window does not yet

After stage 5c and two fixes that only real zink traffic could find, the
guest's GL runs on the RTX 2070. With `MESA_LOADER_DRIVER_OVERRIDE=zink`,
surfaceless EGL gives:

```
OpenGL core profile renderer: zink Vulkan 1.3(Virtio-GPU Venus (NVIDIA GeForce RTX 2070) (MESA_VENUS))
```

— core, compatibility and ES, all on our device.

### The two fixes

- **A bind is judged against what the host allocated.** Venus computes a
  buffer's memory requirements from a per-usage cache as
  `align(size, cached.alignment)` (`vn_buffer.c:136-146`), an
  implementation-defined rule. On this host a 4-byte buffer really needs
  16 bytes at alignment 16, so the guest allocated 4 bytes and bound 16.
  vkr does not check and the driver allocates in pages, so nobody else
  notices. The check exists to protect the host allocation, so it now
  measures that: a non-dedicated allocation is rounded up to a blob page on
  the host, host-visible memory already is whole pages, and binds are
  judged against `MemoryObject::host_size`.
- **A size query ignores the size it is handed.** The generated host call for
  `vkGetPipelineCacheData` bounded `*pDataSize` even when `pData` was NULL —
  the size query, whose input the spec says is ignored, and which carries
  whatever the app's variable held. `vkGetQueryPoolResults` shared the
  pattern. The fix is in `scripts/venus-exec-gen.py`, so it covers both.

Each fix has a test that fails without it. The pipeline-cache one runs on the
real RTX 2070, where the cache header is 36 bytes.

### What still falls back

The Wayland EGL platform creates the zink screen, then fails at
`dri2_setup_device` ("DRI2: failed to setup EGLDevice",
`platform_wayland.c:2737-2740`) and drops to llvmpipe. libdrm is not the
cause: a guest probe shows `drmGetDevice2` on both `card0` and `renderD128`
equal to the only `drmGetDevices2` entry. So the fd EGL holds at that point
is not the node libdrm describes. The next question is which device the
software-rendered GNOME compositor hands its clients through `linux-dmabuf`.
The durable answer is GNOME itself on the GPU, which is the next stage in any
case.

## Amendment, 2026-09-24 — stage S1 of "GNOME on the GPU": shared device-local memory, LINEAR modifiers, foreign queues, exportable sync files

GNOME's compositor, Mutter, draws through GBM on `card0`. With Zink forced
in its place (a driconf `dri_driver=zink` entry), four things stood between
this renderer and a GPU-composited desktop. Each was read from Mesa 26.0.8,
Mutter 50.1 and Linux 7.0, and each citation below was checked:

1. **No `VK_EXT_queue_family_foreign`.** Zink's dma-buf capability needs it
   (`zink_screen.c:1128-1133`). Without that capability GBM has no export
   (`gbm_dri.c:1242-1247`), so it makes every `gbm_bo` a dumb buffer
   (`:902-903`). The host has the extension; policy did not pass it through.
2. **No `VK_EXT_image_drm_format_modifier`.** Mutter asks for scanout
   surfaces without modifiers, because the virtio kernel driver offers no
   `IN_FORMATS`. Zink therefore makes them optimal and not exportable. At
   export it rebuilds each one as a DRM-modifier image with the list
   `[LINEAR]` and copies into it (`zink_resource.c:1744-1768`). Asking the
   exported handle for its stride needs the extension too (`:1958-1964`,
   `zink_resource_get_param`).
3. **Memory.** Zink places every non-staging image in device-local memory
   (`zink_resource.c:1443-1446`) and fails when no type matches
   (`:1047-1076`). Stage 5c's dma-buf existed only in our pages. Those are not
   device-local. On the RTX 2070 an optimal image cannot live in them, and a
   linear one cannot be a colour attachment in any scanout format (measured,
   below). So every export was refused.
4. **Fences.** `EGL_ANDROID_native_fence_sync` needs the guest's
   `VK_KHR_external_semaphore_fd`. Venus offers that extension only when a
   `SYNC_FD` semaphore is also exportable (`vn_physical_device.c:1173-1179`).
   Stage 5b.3 answered "importable" only.

The design that removes all four: **device-local memory that can be exported
is the dma-buf.** On the host it is `OPAQUE_WIN32` memory, backed by a blob
with no pages that the guest cannot map. Another context imports it through
the NT handle. A LINEAR modifier is emulated as one **canonical optimal
image**, so the exporter's image and the importer's image have the same
layout. The code is `venus/executor/{memory,modifier}.rs`,
`host_vulkan/mod.rs` and `venus/renderer.rs`.

### What the guest is shown

| extension | how | when |
|---|---|---|
| `VK_EXT_queue_family_foreign` | passed through, and enabled on the host device | the host has it, **and** device-local memory can be exported, **and** the dma-buf pair is shown |
| `VK_EXT_image_drm_format_modifier` (spec 2) | **emulated**: stripped from the host's device create, answered by the executor | the same condition |
| `VK_KHR_external_semaphore_fd` | emulated as before, and now `EXPORTABLE` (`policy::SYNC_FD_EXPORTABLE`) | always |
| `VK_KHR_external_memory_win32` | host-only: enabled on every host device that has it, never shown to the guest | — |

"Device-local memory can be exported" is `GuestDevice::memory_export`. It
means the host lists `VK_KHR_external_memory_win32` and reports the
`deviceUUID`/`driverUUID` that an import is checked against. A Linux host
never meets this condition: its drivers have no `OPAQUE_WIN32`, and
`AshVulkan` resolves the Win32 entry point only under `cfg!(windows)`. A
Linux host therefore gets neither extension, and GBM keeps its dumb
buffers. Showing the extensions there would turn every GBM allocation into
a failing export. The capset mask gains bit 159: without that bit the
guest's encoder drops the modifier structures. It gains no bit for
queue-family-foreign, which chains no structure. That brings the mask to 73
extensions.

### Handle blobs

An export allocation (`VkExportMemoryAllocateInfo{DMA_BUF}`) on a type that
is not our pages is allocated on the host with
`VkExportMemoryAllocateInfo{OPAQUE_WIN32}`. Its host size is rounded to a
blob page. It is **never dedicated on the host**; the guest's own dedication
is still recorded for its binds. An import of opaque memory must match the
export's dedication, and neither side can always name the other's image.
Leaving dedication off both sides makes them agree by construction. The
canonical image requires a type that is not dedicated-only. A canonical
image that the host would still want dedicated memory for is refused with
`VK_ERROR_OUT_OF_DEVICE_MEMORY`.

The blob Mesa makes of that memory straight away (`vn_device_memory_alloc_export`:
`HOST3D`, `SHAREABLE`, not `MAPPABLE`) is a **handle blob**
(`ExportedMemory::Handle`). At blob creation the executor calls
`vkGetMemoryWin32HandleKHR` and keeps the NT handle together with the
export's type, host size and UUIDs (`memory::HandleExport`), type-erased
inside the renderer's blob directory. The blob has no pages. The renderer
refuses `RESOURCE_MAP_BLOB` of it (`VenusError::HandleBlobNotMappable`,
answered `BlobNotMappable`), and the device layer already refuses to map a
blob without `MAPPABLE`. Ownership and attachment follow stage 5c's rules
for page blobs.

**Lifetime.** An NT handle to exported memory references the allocation's
payload by itself. The exporting memory, its device and its whole context
may therefore go first, and the blob can still be imported, exactly as a
dma-buf outlives the process that exported it. An import references the
payload too, so the blob may go after it. The handle is closed
(`SharedMemoryHandle`'s `Drop`, `CloseHandle`) when the last `Arc` of it
goes: the blob's, or an import's that is in progress. Neither closing the
handle nor an import needs the exporter's device to still exist. That order
is the real-GPU test below.

**Two deviations from the brief**, both deliberate:

- *The handle is taken when the blob is created, not when it is imported.*
  If it were taken at import, the exporter's memory would have to outlive
  every import still to come, and a dma-buf promises the opposite.
- *A handle blob outlives its context until the guest unrefs the resource*,
  as a page blob does. The guest kernel frees the resource when the last GEM
  reference goes. Another process — Mutter, holding a client's buffer — may
  keep it longer than the client lives. Imported memories go with their
  context (`destroy_all`). A reset drops every blob and closes every handle.

### Imports (`VkImportMemoryResourceInfoMESA` of a handle blob)

The import must be on a device that enabled dma-buf and can export at all,
from a context the blob belongs to or is attached to. The importer's
`deviceUUID` and `driverUUID` must be the exporter's. The type must be the
export's own; `vkGetMemoryResourcePropertiesMESA` answers exactly that bit,
with the blob's size. The size must be no more than the blob. The host is
then handed `VkImportMemoryWin32HandleInfoKHR`, with the export's own
allocation size and type (an opaque handle type requires both) and without
dedication. Anything else is `VK_ERROR_INVALID_EXTERNAL_HANDLE`, logged with
the reason, as vkr answers. On an import, venus passes the application's
export info through unrewritten (Zink names `OPAQUE_FD | DMA_BUF`). That is
accepted and ignored: no blob is ever made of an import. Exportable memory
and imported memory each bind only to a resource created for a host handle
(`VUID-vkBindImageMemory-memory-02728`, `-02989`, and the buffer
equivalents). Otherwise the bind is fatal.

### Resources

A buffer or image created for `DMA_BUF` still takes our pages whenever the
host will import them for it. Those are resources the guest may map, and the
page-type restriction of `memory::external_type_bits` stays. A `DMA_BUF`
resource our pages cannot hold is now created for `OPAQUE_WIN32`, provided
the host answers `EXPORTABLE | IMPORTABLE` and not dedicated-only for it
(`ResourceMemory::Handle`). Such a resource sees only device-local types.
Zink's shared optimal images are one example: venus rewrites their
`OPAQUE_FD` to `DMA_BUF`. `vkGetPhysicalDeviceExternalBufferProperties(DMA_BUF)`
answers shareable when either kind of memory can hold the buffer.

### The canonical image, and every lie it tells

The module docs of `executor::modifier` are the full account. In short, for
a scanout format `F` — `B8G8R8A8_UNORM`/`_SRGB`, `R8G8B8A8_UNORM`/`_SRGB`,
`A2R10G10B10`, `A2B10G10R10` — and an extent `W×H`, the host image is always
the same:

- 2D, `W×H×1`, one level, one layer, one sample, `OPTIMAL`, `EXCLUSIVE`;
- `MUTABLE_FORMAT` with the list `[F, F']` when `F` has an sRGB/UNORM twin
  `F'` (as Zink creates every shareable image of such a format), otherwise
  no flags;
- the usage **superset** derived from `F`'s optimal features: transfers,
  sampled, storage, colour and input attachment. The host is asked for it
  with `OPAQUE_WIN32` and the list, and storage is dropped if refused (as it
  is for sRGB on the RTX 2070);
- `OPAQUE_WIN32` external memory.

The guest's own usage, flags and view formats must fit inside these, or the
create is fatal. Nothing else it chose reaches the host. An exporter creating
with `…ListCreateInfoEXT` and an importer creating with
`…ExplicitCreateInfoEXT` therefore produce byte-identical host create infos.
The lies:

1. **LINEAR is optimal.** It cannot be observed: nothing can map the memory.
   A guest that `mmap`s the "LINEAR" dma-buf fails visibly and does not see
   wrong pixels.
2. **LINEAR's features are the optimal ones.** They are reported less storage
   when the superset lost it, and less `DISJOINT`. They are the features the
   host image really has.
3. **The plane layout is synthesized.** `MEMORY_PLANE_0` gives offset 0 and
   `rowPitch = W × 4` rounded up to 256. The size is `rowPitch × H`, which is
   not the real allocation's size. Instead, memory requirements are raised
   to at least that size, so the blob is never smaller than the pitch implies,
   as the guest kernel checks for a framebuffer. An importer's explicit plane
   must be offset 0 at that same pitch; any other layout is answered
   `VK_ERROR_INVALID_DRM_FORMAT_MODIFIER_PLANE_LAYOUT_EXT`.
4. **The image is mutable, and has usage the guest did not ask for.** Both are
   supersets.
5. **Only single-level, single-layer, single-sample, exclusive 2D images.**
   The format query says so; any other request is refused.

Any other modifier is `VK_ERROR_FORMAT_NOT_SUPPORTED` in the query and fatal
in a create.

### Barriers with foreign queue families

`vkCmdPipelineBarrier`, `vkCmdPipelineBarrier2`, `vkCmdWaitEvents`,
`vkCmdWaitEvents2` and `vkCmdSetEvent2` moved from *generated* to
*hand-written*. Each queue family pair of each buffer or image barrier must
now be one of: a family of the device, `IGNORED`, `EXTERNAL`, or `FOREIGN`
on a device that enabled `VK_EXT_queue_family_foreign`. A pair must never be
a transfer between the two external families (`-04065`). `vkCmdSetEvent2`
may make no transfer at all (`VUID-vkCmdSetEvent2-srcQueueFamilyIndex-03842`).
**Before this stage the indices reached the driver unjudged**, and an
out-of-range family was left for the driver to survive.

### `SYNC_FD`, exportable

The flip is one constant. An application's `vkGetSemaphoreFdKHR` on a
device-only payload (`vn_queue.c:2440-2495`) becomes `vn_create_sync_file`
(`:1873-1910`): a virtio-gpu execbuffer on the `ring_idx` of the queue that
last signalled the semaphore, carrying `vkWaitRingSeqnoMESA`. This renderer
executes it as a wait for the ring position, then a host fence on that
queue, retired by the queue's fence thread. `vkWaitSemaphoreResourceMESA`
follows, which the executor serves as an empty submit that consumes the
payload. On an imported payload, `vkImportSemaphoreResourceMESA(0)` comes
first. All of these were served since 5b.3. A fake test now runs them in
the order Mesa does.

### What Mesa, Zink and GBM will send, and what each becomes

| guest | here |
|---|---|
| `vkGetPhysicalDeviceFormatProperties2` + `VkDrmFormatModifierPropertiesListEXT` (capacity 128, `zink_init_format_props`) | one entry, LINEAR, one plane, the optimal features; none for a format that is not a scanout format |
| `vkGetPhysicalDeviceImageFormatProperties2` + `VkPhysicalDeviceImageDrmFormatModifierInfoEXT` (`check_ici`, `zink_resource.c:335-395`) | the canonical image's host limits at 1/1/1, or `FORMAT_NOT_SUPPORTED` |
| `vkCreateImage`, DRM tiling, `DMA_BUF` external, `[LINEAR]` list, `MUTABLE` + `[F, F']` (the export rebuild) | the canonical optimal image, created for `OPAQUE_WIN32` |
| `vkGetImageDrmFormatModifierPropertiesEXT` | LINEAR |
| `vkGetImageSubresourceLayout(MEMORY_PLANE_0)` (stride and offset) | the synthesized plane |
| `vkAllocateMemory` + export `DMA_BUF` + dedicated, device-local type; then `RESOURCE_CREATE_BLOB` `SHAREABLE` | exportable host memory; a handle blob |
| `vkGetMemoryFdKHR` / `drmPrimeFDToHandle` (Mutter's `gbm_bo_get_handle`) | guest-side only: the same GEM resource |
| another process: `CTX_ATTACH_RESOURCE`, `vkGetMemoryResourcePropertiesMESA`, `vkCreateImage` explicit LINEAR, `vkAllocateMemory` + `VkImportMemoryResourceInfoMESA` | attach; the export's type bit; the same canonical image; a Win32 import |
| barriers to and from `VK_QUEUE_FAMILY_FOREIGN_EXT` | passed through, judged |
| `vkGetSemaphoreFdKHR` (`EGL_ANDROID_native_fence_sync`) | a ring fence, then `vkWaitSemaphoreResourceMESA` |

### Measured on the RTX 2070

The test is
`host_vulkan::pipeline_tests::a_linear_modifier_image_rendered_by_one_context_is_read_back_exactly_by_another`
(driver 580.88, Windows), driven through real rings with the generated
driver-side encoder.

Context 1 is shown LINEAR for RGBA8 with tiling features `0x1dd83`, which
includes the colour attachment linear tiling lacks. It creates the
canonical image, gets an exportable allocation of `0x40000` bytes (256 ×
1024-byte pitch), and makes the handle blob; mapping the blob is refused.
It then renders vk-smoke's check-6 triangle through a render pass and
releases the image to `FOREIGN`.

Context 2 attaches and is answered exactly the export's type. It creates the
same image explicitly LINEAR at pitch 1024, gets the same requirements, and
imports and binds. **Context 1 then destroys its whole instance, and the
blob is destroyed**, so the import alone holds the allocation. Context 2
acquires the image from `FOREIGN` and copies it into our pages. The result
is `256x256 exact, 6 probes ok, px red/green/blue/clear=8362/8363/8363/40448,
fnv1a=0x2678f2a0e39fba1b`: vk-smoke's checksum for the bare RTX 2070,
rendered by one guest process and read back by another through device-local
memory neither can map.

### Owed

- **Guest acceptance.** GNOME with `dri_driver=zink`: `gbm_bo`s from Zink,
  and Mutter's framebuffers made of handle blobs.
- **Scanout of a handle blob.** Mutter's `drmModeAddFB2` of such a blob
  reaches the device as `SET_SCANOUT_BLOB`. The host renderer must read the
  image through Vulkan: the handle is exactly what it imports. That is the
  scanout-blob hook being built beside this stage; the pitch the guest
  passes is the synthesized one and says nothing about the bytes.
- A Linux host's equivalent (`OPAQUE_FD` from a real GPU) is not built.
  There this stage advertises nothing.
- `save`/`load`: a snapshot is refused by name while a handle blob lives,
  even after every Vulkan object has gone.

## Amendment, 2026-09-24 — GNOME on the GPU, S2a: the device scans out renderer blobs

A GPU-composited guest desktop (Mutter over GBM → zink → venus) puts its
frames on screen through **renderer blobs**. Each scanout buffer is a guest
`VkDeviceMemory` that the kernel wraps in a `RESOURCE_CREATE_BLOB` with
`BLOB_MEM_HOST3D` and `SHAREABLE`, never `MAPPABLE`. Every page flip is then
`SET_SCANOUT_BLOB` (`B8G8R8X8`, the framebuffer's width and height,
`strides[0]`, `offsets[0]`) followed by an unfenced `RESOURCE_FLUSH`, with no
transfer (Linux 7.0 `virtgpu_plane.c:267-305`, `virtgpu_vq.c:1459-1493`). Until
now the device refused `SET_SCANOUT_BLOB` for anything but a guest-memory blob.
S2a is the device and trait half. The Venus renderer's implementation is a
later stage.

- **The hook is additive.** `Renderer3d::scanout_blob(resource_id,
  &ScanoutBlobSpec) -> Result<(), CommandError>`, where `ScanoutBlobSpec` is
  `{format, width, height, stride, offset}`. The default refuses with the
  error the device answered before (`ERR_INVALID_PARAMETER`), so virgl, the
  loopback and the isolated renderer are unchanged. The isolated renderer does
  not forward the hook: that would need a new request in its protocol, and it
  has no host blob memory worth reading back.
- **The pixels come through `read_rect_bgra`**, with no new read call. An
  accepted `scanout_blob` promises that `read_rect_bgra(resource_id, rect)`
  now reads that layout, until the next accepted spec for that resource,
  `destroy_blob` or `reset`. The flush path is the one 3D scanouts already
  use: clip to the scanout, read into the device's reused buffer, then
  `update_scanout`. `display` did not change. A second read call would only
  pass the spec again on every frame, and the renderer is already required to
  keep it.
- **One spec per buffer.** A compositor alternates between two or three
  buffers on every flip. The device therefore stores the accepted spec on the
  blob, not on the scanout. Flipping to a buffer whose layout is already
  accepted makes no renderer call. A changed layout is asked again. A refusal
  keeps the previous acceptance and the previous binding. After the first
  renderer-blob scanout, which is logged once at info, re-binds log at debug.
- **Bounds before the renderer.** The device checks the rect against the
  framebuffer, the framebuffer against `MAX_RESOURCE_PIXELS`, the stride
  against a row (`width × 4`), and `offset + stride × height` against the
  blob's declared size, all in u64. Flush damage is checked against the
  declared framebuffer. The renderer's answer must be exactly
  `rect.width × rect.height × 4` bytes, or the flush fails in band.
- **Lifetimes follow the other sources.** `RESOURCE_UNREF` of the blob on
  screen disables the scanout. `SET_SCANOUT`/`SET_SCANOUT_BLOB` with resource
  0 disables it. A device reset drops the binding, the blobs and every accepted
  layout. A lost renderer (GPU-012) drops a renderer-blob scanout, as it drops
  a 3D one. `HOST3D_GUEST` blobs take the renderer path too: their pixels are
  the renderer's as well.
- **Snapshots** (ADR-0006) record the binding as a blob scanout, which is
  host-owned. A restore rebinds nothing and reads nothing, the window keeps its
  initial frame, and the blob counts in `live_blobs`, so the driver is told to
  start again. This matches a 3D-resource scanout. The readback runs on the
  gated queue worker inside a trait call, so it needs no `Quiesce` of its own.

Pinned by the `renderer_blob_scanout` tests in `tests/gpu_blob.rs`. They use a
fake renderer whose readback encodes each pixel's coordinates and resource.

**What the Venus renderer owes for this to work:**

1. `scanout_blob`: accept a blob that is one of its `VkDeviceMemory` blobs
   and is at least `offset + stride × height` bytes on the host, then record
   the spec per resource.
2. `read_rect_bgra` for such a resource: copy the rows of `rect` out of that
   memory (`offset + y × stride + x × 4`) as packed BGRA.
3. `destroy_blob` and `reset`: forget the spec.

## Amendment, 2026-09-24 — GNOME on the GPU, S2b: the Venus renderer serves the scanout

S2a gave the device a way to ask a renderer for a renderer blob's pixels.
S2b is the Venus renderer's answer, for both kinds of blob its memory
makes. The code is `venus/renderer.rs` (the judgement and the page path),
`venus/executor/scanout.rs` (the handle path), and the recording in
`venus/executor/{memory,device_objects,modifier}.rs`.

### What the guest does, verified

- **The release.** At the end of every batch Zink releases each exported
  image out of its instance (`zink_batch.c:900-934`): one image barrier with
  `oldLayout == newLayout == res->layout`, so no layout change — whatever
  layout the frame left the image in. `srcAccessMask` is its last access and
  `dstAccessMask` 0. `dstStageMask` is `ALL_COMMANDS`. `srcQueueFamilyIndex`
  is its queue's family and `dstQueueFamilyIndex` is
  `VK_QUEUE_FAMILY_FOREIGN_EXT`. The layout is therefore not a constant.
  After a render pass it is the pass's final layout. After a blit it is
  `TRANSFER_DST`. On a driver Zink runs with `general_layout` it is
  `GENERAL` (`zink_screen.c:3134-3143`).
- **The fence.** Mutter 50.1 commits a KMS update only once the update's
  `sync_fd` — the frame's `EGL_ANDROID_native_fence_sync` fence
  (`meta-onscreen-native.c:1812-1822`) — is readable
  (`meta-kms-impl-device.c:2089-2116`). By the time `SET_SCANOUT_BLOB` and
  `RESOURCE_FLUSH` arrive, the release has executed on the host GPU. The
  host's copy needs no semaphore of its own.
- **The format.** GBM's `XRGB8888` is Mesa's `BGRX8888_UNORM`
  (`dri_helpers.c:434`), which Zink emulates as `B8G8R8A8_UNORM`
  (`zink_format.c:176`). The kernel sends it as `B8G8R8X8_UNORM`.

### Page blobs

A blob of host-visible memory is our pages. It is accepted when
`offset + stride × (height − 1) + width × 4 <= pages.mapped_len()`, computed
in u64. That is the last byte the image touches, so the check is exact at
the end rather than `stride × height`. It is read row by row through
`RingPages::read_bytes` into the device's buffer, packed. A ring or reply
blob (`blob_id` 0) is not an image and is refused.

### Handle blobs: the canonical image, recorded

The guest's pitch for a handle blob is synthesized and says nothing about
the bytes (S1). The only authority on the layout is the canonical image
bound to the memory, so the blob must know which image that is.

- **What is recorded.** `modifier::CanonicalImage`: format, flags, view
  formats, usage superset, width and height. That is exactly what the host
  create info is built from (`CanonicalImage::create_info`, which the
  exporter, every importer and the scanout device all call). The record
  also holds the bind's `memoryOffset`, the context and image ids, and a
  `Weak` of a token the image object owns.
- **When.** At every successful bind of a canonical image to handle memory:
  the exporting memory, or an import of a handle blob. Memory keeps a `Weak`
  of the handle its blob holds (`SharedRef`). The directory finds the blob
  by that handle's identity, through an index keyed by the handle's address,
  never by a resource id a guest could reuse. Mesa makes the blob inside
  `vkAllocateMemory`, before the bind. An application that binds first gets
  its images recorded when the blob is made: `export_memory` records the
  images already bound, pending in the directory until the renderer inserts
  the blob straight after, where they are adopted.
- **Lifetime.** The record is updated under the directory lock. It lives
  exactly as long as the image object: the token dies with it, whichever
  path destroys it (`vkDestroyImage`, device teardown, context teardown, a
  reset), and dead records are pruned. At most `MAX_SCANOUT_IMAGES` (8) are
  kept per blob, oldest first out. Pending records are capped at 64.
  `forget_context` drops a context's records. Removing the blob drops its
  records and its index entry.
- **The release, recorded too.** Every image barrier of a recorded canonical
  image that releases it out of the instance (to `FOREIGN` or `EXTERNAL`,
  from a family of the device) sets the blob's last release: the layout and
  the family. It is recorded when the barrier is recorded, which for a
  flipped frame is before the flip. It is kept per blob, not per image,
  because it describes the payload.

`scanout_blob` of a handle blob is accepted only when a live record
matches the spec: the format is BGRA-ordered (`B8G8R8A8_UNORM` or `_SRGB` —
the same bytes; a scanout samples nothing), the extent is the
framebuffer's, and both the plane offset and the bind offset are 0. The
stride must be exactly the synthesized pitch. An RGBA-ordered canonical
image under a BGRA scanout is **refused, not swizzled**: the device accepts
only the two BGRA formats, so it would be a guest naming the wrong fourcc.
Anything else is `ERR_INVALID_PARAMETER` (`CommandError::ScanoutLayout`),
logged with the reason ("no canonical DRM-modifier image is bound to its
memory", "stride 7936 is not the 7680-byte pitch…"). A refusal keeps the
previous acceptance.

### The scanout device

One host `VkDevice` owned by the renderer's factory (`ExecutorFactory`),
not by any guest context, so nothing a guest does to its own objects
reaches it. It is created lazily by the first handle-blob scanout, on the
physical device whose `deviceUUID`/`driverUUID` are the export's. It has
one queue (the first graphics family, else the first with transfers), one
command pool, one command buffer, one fence, and three extensions:
`VK_KHR_external_memory_win32`, `VK_EXT_external_memory_host` and
`VK_EXT_queue_family_foreign`.

For each blob it keeps an import: the export's NT handle imported
(`VkImportMemoryWin32HandleInfoKHR`, the export's own size and type,
undedicated). **Exactly the canonical image** on record is created over it
and bound at 0, and it has a staging buffer of `width × height × 4` bytes of
our own pages, charged to the renderer's 1 GiB host-visible budget. At most
`MAX_SCANOUT_TARGETS` (4) are kept, the least recently read evicted first. A
compositor flips between two or three; an evicted one is simply made again
by the next read.

**One read**, on the one command buffer, fenced:

| | `src` family | `dst` family | `oldLayout` | `newLayout` | stages | access |
|---|---|---|---|---|---|---|
| acquire | the release's (`FOREIGN`) | ours | the release's layout `L` | `L` if `GENERAL` or `TRANSFER_SRC`, else `TRANSFER_SRC` | `TOP_OF_PIPE` → `TRANSFER` | 0 → `TRANSFER_READ` |
| copy | `vkCmdCopyImageToBuffer` of the rect, packed | | | | | |
| release | ours | the release's | the copy layout | `L` | `TRANSFER` → `BOTTOM_OF_PIPE` | 0 → 0 |

A buffer barrier beside the release (`TRANSFER_WRITE` → `HOST_READ`) makes
the copy visible to the host. The old layout is never `UNDEFINED`, which
would discard the frame. Releasing back in `L` makes the guest's next
acquire (`oldLayout = res->layout`, from `FOREIGN`) exactly consistent. A
release in a layout the device will not acquire from is refused before
anything is recorded: `UNDEFINED`, `PREINITIALIZED`, the two
`synchronization2` layouts (not enabled here), or any extension layout. So
is a read before any release has been recorded.

The fence is waited for `SCANOUT_WAIT` (100 ms) at most. On a timeout the
flush fails in band, the window keeps its frame, and the next read first
waits (bounded) for that work. A lost device fails the flush and drops the
whole scanout device; the next scanout makes a new one. Only then are the
rows read out of the staging pages through the bounded
`RingPages::read_bytes`.

### Lifecycle

- **No thread of its own.** The readback runs inside `read_rect_bgra` on the
  device's gated queue worker, as S2a said, so there is no new `Quiesce`
  obligation.
- **Unref.** `destroy_blob` of a handle blob drops its import, image and
  staging buffer (`SinkFactory::forget_scanout`). A changed spec replaces
  the import once the new one is complete.
- **Reset.** `reset()` drops every import. **The `VkDevice` itself is
  kept.** After that it holds nothing of any guest, and a rebooted desktop
  scans out again within seconds; re-creating a device on every reboot
  would only be a stall. It goes when the renderer does, when it is lost,
  or when a blob of another GPU is scanned out.
- **Snapshots.** Unchanged. A handle blob already refuses a snapshot by
  name. A page-blob scanout's memory is a live executor object, which
  refuses one too; the device-side record is S2a's.

### Measured on the RTX 2070

The test is
`host_vulkan::pipeline_tests::a_handle_blob_flip_reads_back_the_frame_through_the_renderers_scanout_device`,
driver 580.88 on Windows, through the renderer's `scanout_blob` and
`read_rect_bgra`. That is the whole Venus path the device calls. A real
`GpuDevice` needs a guest ring the test harness does not reach, so the
device half stays S2a's fake-renderer tests. Context 1 exports a 256×256
LINEAR BGRA8 buffer, blob before bind as Mesa does, renders vk-smoke's
check-6 triangle and releases it as Zink does. The renderer reads back
`256x256 exact, 6 probes ok, px red/green/blue/clear=8362/8363/8363/40448`,
BGRA `fnv1a=0x9a880db295ee1483`. Swizzled back to RGBA that is
`0x2678f2a0e39fba1b`, vk-smoke's checksum. The guest then re-acquires the
buffer from `FOREIGN`, draws check 7's clear and releases it again. The
second flush reads the new frame (BGRA `0x4c1a0a49e1373503`, RGBA
`0xd79d631c4d62403b`), so the acquire/release cycle repeats.

Per flush:

| | 256×256 | 1920×1080 |
|---|---:|---:|
| release build, median (min–max) | 0.19 ms (0.18–0.34) | 4.1 ms (3.8–6.4) |
| debug build, median | 9.6 ms | 278 ms |
| first read (import, image, staging), release | 0.52 ms | — |

At 1080p, 2.65 ms of the 4.1 ms is the relaxed byte-at-a-time copy out of
the staging pages (measured alone). The GPU copy, the submit and the fence
are the other ~1.5 ms. This is the copy path, option (a) of the research.

### Queries are questions (found by kmscube on this stage's parent)

Zink's format table probes formats outside core 1.3. It asked
`vkGetPhysicalDeviceFormatProperties2` about `VK_FORMAT_A1B5G5R5_UNORM_PACK16_KHR`
(1000470000, maintenance5), and the executor killed the context: every GL
client on Zink died at startup. A query about a value some extension
defines but this device does not serve is now answered, and never sent to
the driver:

- `vkGetPhysicalDeviceFormatProperties2`: no features in the base structure
  and in every chained one; modifier lists come back empty.
- `vkGetPhysicalDeviceImageFormatProperties2`: `VK_ERROR_FORMAT_NOT_SUPPORTED`
  for a non-core format, DRM-modifier tiling without the extension shown,
  an extension's usage, flag, view format, stencil usage or handle type.
- `vkGetPhysicalDeviceExternalSemaphoreProperties` of an extension's handle
  type, and `vkGetPhysicalDeviceExternalBufferProperties` of an extension's
  handle type, usage or flag: "nothing".
- `vkGetPhysicalDeviceSparseImageFormatProperties(2)`: no entries, since no
  sparse feature is shown. `vkGetPhysicalDeviceExternalFenceProperties`:
  nothing, since no external fence is served. All three used to be refused
  as unimplemented.

What no Vulkan defines (an image type of 7), or what valid usage forbids
outright (usage 0, two handle bits, a modifier structure without its
tiling), stays fatal. So does **creating** an image or view of any value
outside what is served. Pinned by `venus::executor::query_tests`.

### Owed

- **Guest acceptance**: kmscube, then GNOME with `dri_driver=zink` (the
  coordinator's run).
- **Zero-copy** (a later stage). Short of that, a word-wise copy out of the
  staging pages, which have no concurrent host writer, would take about
  2 ms off a 1080p flush.
- The release layout is the one **recorded** last. A command buffer
  recorded once and submitted many times with different releases is not
  followed; Zink records one per batch.
- A Linux host (`OPAQUE_FD`) has no handle blobs, so no handle-blob scanout
  either.

## Amendment, 2026-09-24 — GNOME on the GPU: measured in the guest

After S1, S2a and S2b (commit `96815a2`), in the Ubuntu 26.04 guest on WHP
with `ENTANGLED_VENUS=vulkan`:

**`kmscube -D /dev/dri/card0`** (GBM → zink → our Venus → KMS, no Mutter),
zink forced by `MESA_LOADER_DRIVER_OVERRIDE`:

```
renderer: "zink Vulkan 1.3(Virtio-GPU Venus (NVIDIA GeForce RTX 2070) (MESA_VENUS))"
display extensions: ... EGL_ANDROID_native_fence_sync ...
Rendered 5052 frames in 86.48 sec (58.4 fps)          # 1920x1080
```

The host logged `virtio-gpu is scanning out a renderer blob: the guest
composites on the GPU resource=31 width=1920 height=1080 stride=7680`, with
flips alternating between resources 31 and 32, and frame pacing at 60 fps.
The VMM's screenshot shows the cube. A temporary pixel probe on the flush path
read back the grey clear (`0xFF7F7F7F`) and a centre pixel that changed every
sample.

**GNOME Shell on zink**, enabled by a driconf entry scoped to the compositor
alone:

```xml
<!-- /etc/drirc in the guest -->
<driconf>
  <device driver="loader" kernel_driver="virtio_gpu">
    <application name="gnome-shell on zink" executable="gnome-shell">
      <option name="dri_driver" value="zink" />
    </application>
  </device>
</driconf>
```

After a `gdm3` restart:

- `gnome-shell` maps `libvulkan_virtio.so`, and its journal still says
  `Created gbm renderer for '/dev/dri/card0'` — now on zink instead of
  kms_swrast.
- Mutter advertises **`zwp_linux_dmabuf_v1` version 5, main device `0xE280`**
  (226:128, the render node). It advertised version 3 with no device while it
  rendered in software. That was the reason Wayland GL clients could not find
  their EGL device.
- **`glmark2-wayland`** (zink via `MESA_LOADER_DRIVER_OVERRIDE`) under it:
  `GL_RENDERER: zink Vulkan 1.3(Virtio-GPU Venus (NVIDIA GeForce RTX 2070))`,
  **`GL_VERSION: 4.6 (Compatibility Profile)`**, 110–131 FPS in `build`, in a
  window on the GPU-composited desktop. The VMM screenshot shows it.
- The host scanned out a renderer blob. Desktop frame pacing was about 30 fps
  while glmark2 ran.

### What this took, and what it does not yet do

The whole path is ours. Mesa's zink turns GL into Vulkan in the guest, and
venus serialises it. On the host, our transport, generated protocol and
executor run it on the RTX 2070. The scanout buffers are device-local memory
shared between contexts through Win32 handles, dressed as LINEAR dma-bufs. The
renderer's own scanout device reads each flipped frame back, and the existing
display path shows it.

Not yet:

- **Clients still need the override.** Only `gnome-shell` is on zink by
  driconf. Dropping `executable=` from the entry should give every GL app zink,
  but that has not been measured.
- **Every frame is copied twice.** The GPU readback is ~4 ms at 1080p (2.65 ms
  of it a byte-wise copy out of the staging pages), then the CPU mirror feeds
  the texture upload. Vulkan clients add their own software-WSI copy into
  `wl_shm`. Zero-copy presentation (option (c): the shared handle straight
  into wgpu) is the next performance step.
- **The guest must be configured**: the driconf file, and render-node access
  for serial-console tools (ADR above). Nothing installs either yet.
