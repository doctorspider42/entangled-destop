# ADR-0009: Our own Windows GPU driver, instead of Yttrium's

Status: **proposed** (2026-11-02). Follows ADR-0008's 3D amendments.

## Context

3D in a Windows guest works today through a third party's test-signed alpha,
Yttrium's `viogpu3d` (ADR-0008): a WDDM 2 kernel driver plus a user-mode
stack built from Mesa — the Venus Vulkan ICD, WGL, and Gallium's `d3d10umd`
for Direct3D. Three facts make it a poor foundation:

- **We cannot fix it.** The package carries no source and no licence, only
  "Copyright (c) 2026 Hardsoft Swed." The work it descends from is public —
  the `viogpu_win` branch of a `kvm-guest-drivers-windows` fork, proposed
  upstream as virtio-win PR #943 (virtio-win is BSD-3-Clause), with Mesa
  changes in mesa!24223 — but which of that Yttrium ships, and under what
  terms, is not stated.
- **Its Direct3D path is a dead end.** `d3d10umd` emits TGSI, implements
  D3D10 only — no D3D11 — and was removed from Mesa in 26.3 (October 2026)
  as unmaintained. That is why WebGL through ANGLE's D3D11 backend draws
  nothing in our guest while the same page through ANGLE's Vulkan backend
  runs at 46 fps: once our executor stopped refusing its binds, nothing on
  our side was left to fix.
- **A full WDDM 3D driver is the hardest driver Windows has.** Memory
  segments, paging, GPU scheduling, preemption (the upstream branch turns
  preemption off system-wide for lack of it), present paths and TDR
  recovery — before any D3D runtime is satisfied.

What already works and needs none of that: the **Venus protocol end to end**.
Our host executor runs a guest's Vulkan on the host GPU, and in the Windows
guest `vk-smoke` passes 9/9 through the Venus ICD.

## Decision

Do not write a WDDM 3D driver. Build the smallest stack that carries Venus,
and get Direct3D and OpenGL by translating them to Vulkan in user mode:

```
D3D9/10/11 app ──► DXVK ─┐
OpenGL app ─────► Zink ──┼──► Mesa Venus ICD ──► entangled-gpu.sys ──► virtio-gpu ──► our executor ──► host GPU
Vulkan app ──────────────┘   (our Windows winsys)   (KMDF, not WDDM)
desktop (DWM) ──► viogpudo (display-only, as today) ──► virtio-vga scanout
```

0. **A second virtio-gpu function, render-only.** One PCI function has one
   driver, and the virtio-vga function stays `viogpudo`'s (the display). The
   host therefore adds a second virtio-gpu device with no scanout, Venus
   capset and shared-memory BAR only, as PCI class `0x0380` (display, other)
   with its own subsystem ID, so Windows matches our INF to it and never
   offers it to a display driver. (Measured: `viogpudo`'s INF matches any
   `DEV_1050` and takes the function until a driver with a more specific
   hardware ID, `SUBSYS_00401AF4`, outranks it — ours must name it.) The executor serves it exactly as it
   serves the virtio-vga function's contexts today; the host change is a
   `[display] render_node = true` switch and a second `attach`.
1. **`entangled-gpu.sys`, a KMDF driver, not a WDDM one,** bound to that
   function. It owns the virtio-gpu 3D half: contexts (`CTX_CREATE/DESTROY`), blob
   resources (`RESOURCE_CREATE_BLOB`, `MAP/UNMAP` into the process through
   the shared-memory BAR), `SUBMIT_3D` with fences, and ring setup — exposed
   to user mode through a small IOCTL interface, the Windows twin of Linux's
   `virtgpu` DRM uAPI that Mesa already speaks. No paging, no scheduler, no
   preemption: Venus does its own synchronisation through rings and fences.
   The display stays with `viogpudo` (BSD-3-Clause, virtio-win), which we
   already install.
2. **A Windows winsys for Mesa's Venus driver** (`vn_renderer` over our
   IOCTLs instead of DRM), and a software WSI for `VK_KHR_win32_surface`:
   render on the host GPU, copy the finished image to the window through the
   display adapter (as Mesa's lavapipe presents on Windows). The ICD is
   registered under `HKLM\SOFTWARE\Khronos\Vulkan\Drivers`; the LunarG loader
   accepts registry ICDs without a WDDM adapter.
3. **Direct3D through DXVK** (zlib licence) and **OpenGL through Zink**
   (Mesa, MIT) on top of that ICD. Both are maintained, D3D11-complete and
   used by millions of Proton users.

Everything we write is ours to licence (Apache-2.0); everything we take is
BSD-3-Clause, MIT or zlib. The host side does not change: it already is the
Venus renderer this stack talks to.

## Phases

| Phase | Deliverable | Done when |
|---|---|---|
| A0 | the render-only virtio-gpu function on the host (class `0x0380`, Venus only) — **done 2026-11-02**: `[display] render_node = true`; the PCI bus grew to ten slots (pins 3 and 15) to fit it beside a Windows machine's nine functions | a Windows guest enumerates it beside virtio-vga, whose 3D desktop is unaffected (verified); a Linux guest's Venus on it is not yet tried |
| A — **done 2026-11-02** | `entangled-gpu.sys` (KMDF, a virtio transport of our own) with context + blob + `SUBMIT_3D` + fence IOCTLs; a test tool | the tool creates a Venus context and round-trips `vkEnumerateInstanceVersion` through the host executor |
| B — **done 2026-11-02** | Mesa Venus Windows winsys + registry ICD | `vk-smoke` passes 9/9 on our stack with Yttrium uninstalled |
| C — **done 2026-11-02** | Software WSI for `VK_KHR_win32_surface` | `vkcube` spins in a window |
| D | DXVK + Zink in the install (`install windows --gpu3d` with no third-party package) | WebGL Aquarium through ANGLE's D3D11 backend, an OpenGL demo and a D3D11 game run |
| E | Zero-copy present (the guest image shared with the display path instead of copied) | presenting costs no CPU copy per frame |
| F | Signing | Microsoft attestation signing (EV certificate + Partner Center account), so test-signing mode is not needed |

**Phase A, first slice (2026-11-02):** `guest/windows-gpu/driver` —
`entangled-gpu.sys` (KMDF 1.15, plain C, built by `build.cmd` with `cl`/`link`
against the WDK because this machine's build tools have no WDK MSBuild
toolset; test-signed by `sign.ps1`) brings the render-only function up over
its own virtio 1.x transport (capabilities, one-bit-at-a-time status, one
polled split virtqueue) and answers `IOCTL_ENTANGLED_GPU_INFO` and
`_CAPSET_INFO`. `gpu-test.exe` in the guest prints `GPU-TEST PASS`: features
`0x100000019` accepted, one capset, Venus (id 4, 160 bytes). Learned on the
way: the INF needs `MSISupported = 1` (code 12 otherwise, the same `_PRT`
story as viogpu3d); a Microsoft-signed `viogpudo` outranks our test-signed
package even with the more specific hardware ID, so `gpu-test install`
forces it with `UpdateDriverForPlugAndPlayDevices`; and every build needs a
new `DriverVer` (`stampinf`), or Windows keeps the old binary from its
driver store. Second slice: `IOCTL_ENTANGLED_GPU_CTX_CREATE`/`_DESTROY`
and `_SUBMIT_3D` (up to 1 KiB inline). In the guest a Venus context is
created (the host logs `venus context created ... name="entangled-gpu"`), an
empty stream is accepted, an unknown opcode refused (0x1205) and the context
destroyed. Third slice, **phase A done**: `IOCTL_ENTANGLED_GPU_BLOB_CREATE`
creates a HOST3D mappable blob, attaches it to the context, maps it into the
shared-memory BAR (a bump allocator over BAR 2) and from there into the
calling process — handled in the caller's context, the mapping undone in
`EvtFileCleanup`. `gpu-test venus` builds a ring in one blob and a reply
stream in another, writes `vkSetReplyCommandStreamMESA` +
`vkEnumerateInstanceVersion` into the ring, rings `vkNotifyRingMESA`, and
reads `result 0, apiVersion 1.3.341` — the host RTX 2070's Vulkan, through
our driver and our executor, Yttrium nowhere in the path.

**Phase B, first look (2026-11-02):** Mesa 26.2.6 configures for
`-Dplatforms=windows -Dvulkan-drivers=virtio` with meson and MSVC (the Windows
WSI is already in its Venus driver; only the renderer backend is missing —
ours would sit beside `vn_renderer_virtgpu.c` and `vn_renderer_vtest.c`). It
does not compile with MSVC, though: C11 atomics need `/experimental:c11atomics`,
and past that the driver is GNU C — arithmetic on `void *`, variable-length
arrays, `gettid`. The build needs `clang-cl` (LLVM for Windows; LLVM 23.1.4 is
installed through scoop since 2026-11-02). With it, Mesa's own `util` stops
first: under `clang-cl` it takes the MSVC atomics path and calls
`_interlockedexchange64`, which clang does not declare there. Mesa on Windows
with this driver is a port of its own — what mesa!24223 (the Windows Venus
work Yttrium builds on) carries — and is phase B's first real task. It turned out small:
`guest/windows-gpu/mesa/0001-venus-builds-with-clang-cl.patch` (Clang takes the
GCC atomics path; `pid_t`, `windows.h` and `sync_file` stubs for Windows; vtn
linked) and `build.cmd` build the whole Venus driver with clang-cl, and the
link stops at exactly one symbol — `vn_renderer_create_vtest`, the renderer
backend, which on Windows is ours to write over `entangled-gpu.sys`'s IOCTLs.
That backend is now `guest/windows-gpu/mesa/vn_renderer_entangled.c` (copied
into Mesa's `src/virtio/vulkan`, with `public.h` as `entangled_gpu_public.h`,
`vn_renderer_create` calling it on Windows): capset data and a Venus context
at creation, shmem and device-memory blobs through `BLOB_CREATE`/`_DESTROY`,
`SUBMIT_3D` on the batch's ring index, and syncs as plain counters — a submit
that signals asks the driver for a fenced answer (`ENTANGLED_GPU_SUBMIT_FENCE`,
the host answering once the ring's fence retires), so a returned submit is a
finished one. The driver grew the IOCTLs for it and a 64 KiB command area.
Mesa's Venus needed `vs_module_defs : vulkan_api_def` to export its ICD
entry points on Windows (without it the linker drops the whole driver).
**Verified 2026-11-02:** in a Windows 10 guest with the render-only function,
`vk-smoke` run with `VK_ICD_FILENAMES` naming only our ICD passes 9/9 — the
host logs `venus context created ctx_id=1 name="entangled-gpu"` and our ring —
with Yttrium nowhere in the path. (The loader ignores `VK_ICD_FILENAMES` in
an elevated process, which first made Yttrium's ICD answer instead.) It is
slow where the host round-trips: an empty submit costs ~10 ms and 1000 fenced
submits 7 s, because every submit is synchronous and the driver polls; the
driver now spins ~2 ms before sleeping (a 1 ms sleep lasts a whole timer
tick), which removed most of it. **Phase C (2026-11-02):** Venus listed no
`VK_KHR_win32_surface`; advertised under `VK_USE_PLATFORM_WIN32_KHR`, Mesa's
own win32 WSI (a CPU blit of the finished image) works on our stack:
`vk-present` (`guest/windows-gpu/driver`) opens a window, makes a swapchain
(624×441, 2 images) and presents 300 frames in 0.55 s on our ICD, the host
logging the `entangled-gpu` context.

**Installed by `install windows` (2026-11-02):** `--gpu3d DIR` now takes
either package; a folder with `entangled-gpu.inf` is ours (the driver, its
`*.cer`, `gpu-test.exe`, `vulkan_virtio.dll` and the LunarG `VulkanRT-*.exe`),
the profile gets `render_node = true`, and `first-logon.cmd`'s `:owngpu`
trusts the certificate, turns test-signing on, installs the runtime and the
driver, and registers the ICD under `HKLM\SOFTWARE\Khronos\Vulkan\Drivers`
(so elevated programs find it too). Verified on a fresh Windows 10 install:
`vk-smoke` finds "Virtio-GPU Venus (NVIDIA GeForce RTX 2070)" through the
registry, no environment variable, Yttrium nowhere. One trap, the same one
viogpu3d's MSI fell into: the render-only function's device instance follows
its PCI slot, which moves between the installer's machine (answer discs, no
network card) and the installed one, and on the new instance Windows picks
the Microsoft-signed viogpudo. `owngpu-bind.ps1` (a SYSTEM task at startup and
logon) rebinds ours whenever that happens and restarts once — a device
swapped live refuses `CreateFile` with access denied until the restart.

**Phase D, OpenGL (2026-11-02):** `guest/windows-gpu/mesa/build-zink.cmd`
builds Mesa's gallium WGL frontend with Zink only (`opengl32.dll` +
`libgallium_wgl.dll`, zlib static; `win_flex`/`win_bison` at build time).
Mesa 26.2's Zink requires `VK_KHR_maintenance5`, which the executor
deliberately does not advertise (`policy.rs`), so
`0002-zink-without-maintenance5.patch` makes it optional (two uses: buffer-view
usage flags, and dropping an unread PointSize). With it `gl-test` (beside the
DLLs, `GALLIUM_DRIVER=zink`) gets a context whose renderer is "zink Vulkan 1.3
(NVIDIA GeForce RTX 2070 (MESA_VENUS))", OpenGL 4.6 compatibility profile.
It first stalled after 2 to ~90 frames, the host idle and no sync waited on
(`ENTANGLED_GPU_TRACE=1` prints every submit, blob and blocking wait; blobs
did not leak). The stall is Venus's asynchronous present (its own thread,
`vn_wsi.c`): `VN_PERF=no_async_present` alone cures it, so our backend now
sets it. **Verified 2026-11-02:** `gl-test` passes, 300 frames at ~51 fps,
twice in a row, and `vk-present` 300 frames at 1177 fps, no environment
variable. Still open: a second Vulkan program started while Zink runs got
`VK_ERROR_OUT_OF_HOST_MEMORY` (seen with the stalled build; not retried).

Driver fixes found on the way: every process now has a Venus context of its
own (the winsys derives the id from its PID; it was a fixed 1, so a second
Vulkan program could not start), and closing a handle — a process dying
included — destroys its context and releases its blobs on the host. Blobs
take the first free gap of the shared-memory BAR and give it back
(`RESOURCE_UNMAP_BLOB`, then unref); it was a bump allocator that never
reused anything, with 16 mappings per handle where Zink keeps dozens. The
BAR's length is read from its large-memory resource descriptor. With these,
a killed program frees everything and `vk-smoke` passes 9/9 with 1000
submits in 156 ms.

**Phase D, Direct3D (2026-11-02):** DXVK 3.1.1 (zlib, the release's x64
`d3d11.dll`/`dxgi.dll` beside `d3d11-test.exe`) finds "Virtio-GPU Venus
(NVIDIA GeForce RTX 2070)" through our ICD and skips it for two required
extensions. `VK_KHR_load_store_op_none` (enum values only) is now admitted by
the executor — its capset bit comes from the admitted list, since it brings no
structure. `VK_KHR_maintenance5` is now admitted too: its feature and property
structures and flags-2 values through the generated translation,
`vkCmdBindIndexBuffer2` bounded like `vkCmdBindIndexBuffer` (and its size),
`vkGetRenderingAreaGranularity` passed through, a shader module chained into a
pipeline stage accepted on a device that enabled it (bounded like
`vkCreateShaderModule`), and the two subresource-layout queries refused (the
1.0 query carries the stage S1 layout lies). DXVK 3.1.1 then asks for
`VK_KHR_maintenance6`; **DXVK 2.3.1 runs. Verified 2026-11-02:** `d3d11-test`
gets "Virtio-GPU Venus (NVIDIA GeForce RTX 2070)", feature level 11_0, a FIFO
swap chain, and passes — 300 frames in 0.67 s (451 fps), the read-back right.

**WebGL in Edge, and many programs at once (2026-11-02):** Edge's GPU process
brings ANGLE up on our ICD ("ANGLE (VirtIO, Vulkan 1.3.0 (Virtio-GPU Venus
(NVIDIA GeForce RTX 2070)))") but Chromium's GPU blocklist turns WebGL off,
because the only Direct3D adapter Windows lists is the Microsoft Basic Render
Driver: `gpu3d-edge.ps1` adds `--ignore-gpu-blocklist` beside
`--use-angle=vulkan` when our stack is installed. Then the GPU process lost its
context on `VK_ERROR_OUT_OF_HOST_MEMORY`: Venus's feedback pool grows 4 KiB —
one blob — at a time and under ANGLE never stops (over a thousand blobs, past
the per-handle limit); our backend now turns event, semaphore and query
feedback off. And a second Vulkan program failed at its first blob: resource
ids are one namespace for the whole device and every process counted from
0x10000, so the driver now assigns them (`ENTANGLED_GPU_BLOB_OUT::ResourceId`).
**Verified 2026-11-02:** get.webgl.org's cube spins in Edge opened from a
plain link (the sandbox on, no environment variable), and beside it
`vk-smoke` passes 9/9 (1000 submits in 198 ms), `gl-test` 300 frames at 63 fps
and `gpu-test venus` passes.

**The whole stack from `install windows` (2026-11-02):** the package may carry
`opengl/` (Zink) and `dxvk/` (DXVK 2.3's x64 DLLs), installed under
`Program Files\Entangled` for programs to take from beside themselves.
**Verified on a fresh Windows 10 install**, the profile as written: one
automatic restart (`owngpu-bind.ps1` rebinding the moved function), then
get.webgl.org's cube spins in Edge opened from a plain link, and with the
installed DLLs `d3d11-test` passes at 533 fps and `gl-test` at 51.5 fps — no
Yttrium, no environment variable, nothing set by hand.

**DXVK 3 (2026-11-02):** `VK_KHR_maintenance6` is admitted:
`vkCmdBindDescriptorSets2` and `vkCmdPushConstants2` bounded like their 1.0
forms (the layout must exist — a null one, `dynamicPipelineLayout`, is not
served), `vkCmdPushDescriptorSet2` refused (no `VK_KHR_push_descriptor`).
DXVK 3.1.1 then chained every shader into its stage with the module null,
which the generated translation refused: `VkPipelineShaderStageCreateInfo.module`
may now be null, and `check_stages` holds the rule — exactly one of a module
or a chained one, the latter only on a device that *enabled* maintenance5 (the
first cut asked the physical device's advertised list; a test caught it).
**Verified:** DXVK 3.1.1's `d3d11-test` passes at 532 fps. Left: dropping `0002-zink-without-maintenance5.patch` now
that the executor serves maintenance5, async/interrupt submits, zero-copy
present (E) and signing (F).

Phases A0–D are the replacement; this machine has the WDK (10.0.26100) to
build A; E is performance; F is distribution.

## Consequences

- **The desktop is not GPU-composited.** DWM stays on `viogpudo`'s
  display-only path (CPU composition), as it is without `--gpu3d` today.
  Applications get the GPU; the shell does not. Acceptable: what users want
  accelerated is applications, and a display-only desktop at 1080p is fine
  on this machine's CPU.
- **D3D12 is out of scope** (vkd3d-proton over the same ICD is the later
  route, LGPL — guest-side, so `cargo deny` does not apply, but its
  redistribution terms must be checked before we ship it).
- **DXVK is per application** (its DLLs beside the game, or installed into
  `System32` by our installer, which then affects every D3D9–11 program in
  the guest). The installer choice is a phase D decision.
- **Present copies** until phase E: one CPU copy of each finished frame in
  the guest, on top of the host's own copy (ADR-0008's performance notes).
- **Signing is the long pole.** Test-signing is fine for development; a
  driver users install without test mode needs Microsoft's attestation
  signature, which needs an organisation with an EV certificate.
- Yttrium's package stays supported by `--gpu3d` until phase D ships; the
  executor fixes made for it (ADR-0008) benefit any Windows Venus driver.

## Sources

- virtio-win PR #943, the original viogpu3d: https://github.com/virtio-win/kvm-guest-drivers-windows/pull/943
- virglrenderer MR !1185, its host side: https://gitlab.freedesktop.org/virgl/virglrenderer/-/merge_requests/1185
- Mesa drops `d3d10umd` (Mesa 26.3): https://www.phoronix.com/news/Mesa-Drops-Direct3D-10-UMD
- Venus in Mesa: https://docs.mesa3d.org/drivers/venus
