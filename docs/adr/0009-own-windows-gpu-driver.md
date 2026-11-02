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
| B | Mesa Venus Windows winsys + registry ICD | `vk-smoke` passes 9/9 on our stack with Yttrium uninstalled |
| C | Software WSI for `VK_KHR_win32_surface` | `vkcube` spins in a window |
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
