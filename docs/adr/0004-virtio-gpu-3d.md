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
