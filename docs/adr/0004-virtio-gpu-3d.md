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

### Next agent starts here

1. **A real Venus renderer (VEN-2003).** Build virglrenderer ≥ 1.0 with
   `-Dvenus=true` on a host where `apt` is available, then fill in
   `VirglRenderer::map_blob` with `virgl_renderer_resource_map` and
   `Renderer3d::set_host_visible` with the window it should map into, and add
   `VIRGL_RENDERER_VENUS` (very likely with `USE_EXTERNAL_BLOB`) to the init
   flags. The typed-context and create-blob FFI halves are already written and
   behind the runtime probe; the window is now waiting for them.
2. **Per-blob host mappings.** This phase maps the *whole* window as one
   hypervisor slot and lets the device write into it. A real Venus renderer
   wants the opposite: `virgl_renderer_resource_map` hands back a host pointer
   per blob, and the VMM maps *that* at the guest-named offset — one slot per
   live mapping, torn down on unmap. `GpaMapper` takes a `&HostShmRegion` today
   and would grow a sub-range form; `HostVisibleWindow` already tracks exactly
   the spans that would need one. Nothing above the seam changes.
3. **Guest acceptance (VEN-2006)** is meaningful after 1, and only on a host
   with a real Vulkan device. `vulkaninfo` inside the guest is the first
   milestone, `vkcube` the second.
4. **Zero-copy scanout (VEN-2005/GPU phase 3)** is still where the frame rate
   is: GAME-2105 measured the readback at 13 ms of a 20 ms frame. Unrelated to
   this window, and unblocked by nothing in it.
5. The GUI's capability gate (`Backend::virgl_block`) still knows nothing about
   any of this, and still should not until step 1 lands — blob resources are
   offered only when a renderer declares them, and none does on this host.
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
