# ADR-0010: The Windows desktop on the GPU — a WDDM driver after all

Status: **proposed** (2026-11-11); built as far as "What was built" below
records, and opt-in. Reverses the first sentence of ADR-0009's
decision ("Do not write a WDDM 3D driver") for the display function; keeps
everything else ADR-0009 built.

## Context

ADR-0009 gave Windows *applications* the host GPU without a WDDM driver: a
render-only virtio-gpu function, a KMDF driver, Mesa's Venus ICD, DXVK and
Zink on top, and an overlay plane that shows a foreground Vulkan window with
no CPU copy. The desktop itself stayed on a display-only driver, composed by
DWM on the CPU.

That holds up better than it sounds — with the vsync of ADR-0008's
2026-11-10 amendment the CPU-composed desktop does 79–92 fps at more than a
megapixel of change a frame — but it leaves three things that only a real
display adapter fixes:

- **Every program has to opt in.** A program reaches the GPU only if it
  speaks Vulkan, or is handed DXVK's DLLs, or is started with switches. The
  2026-11-11 amendment is the cost of that: Edge started by Windows itself
  ran a full-screen video at 17.5 fps until two policies stopped it. Chrome,
  Electron and Steam's client are the same story, one by one.
- **The overlay is one window.** It needs the window in front, unobscured and
  entirely on screen. Anything else falls back to a copy through the guest's
  CPU.
- **Windows lists no GPU.** The only Direct3D adapter is the Microsoft Basic
  Render Driver, so software that asks Windows what it has (a browser's
  blocklist, a game's launcher, Media Foundation's decoders) concludes there
  is none.

A Linux guest has none of these problems because its compositor draws on the
GPU through the one virtio-gpu device, and the host scans out the finished
buffer (ADR-0004). This ADR does the same for Windows. Test-signing mode
stays, as for the rest of the 3D package (decided by the user, 2026-11-11).

## What exists, measured or read on 2026-11-11

- **A WDDM kernel driver for virtio-gpu, in source, BSD-3-Clause**: the
  `viogpu3d` of virtio-win PR #943 (`max8rr8/kvm-guest-drivers-windows`,
  commit `9ed3aab1`, 7.5k lines beside the `viogpu/common` we already build).
  It fills the whole WDDM 1.3 table: allocations, contexts, `Render`,
  `Patch`, `SubmitCommand`, `Present`, `SetVidPnSourceAddress`, a flip thread
  that raises `DXGK_INTERRUPT_CRTC_VSYNC`. **It builds with our toolchain**:
  the `cl`/`link` script of `guest/windows-gpu/display` with the file list
  changed and the same no-WPP trace patch produced a 112 KiB `.sys`. Nothing
  beyond the build is verified.
- **It is virgl-shaped.** Commands are virgl's, resources are classic
  virtio-gpu 3D resources moved with `TRANSFER_TO/FROM_HOST`; there is no
  blob resource, no shared-memory mapping and no Venus ring in it. Our
  Windows host serves Venus only (virglrenderer is Linux-only, ADR-0004).
- **A Direct3D 10 user-mode driver, MIT**: Mesa's `d3d10umd` frontend is
  still in the Mesa we pin (26.2.6; removed in 26.3). Mainline it drives
  software rasterisers and presents through GDI. Mesa MR !24223 (draft, open,
  ~2000 lines over 36 files) is the part that makes it a real WDDM UMD:
  allocations and presents through the D3DKMT callbacks, a `gdikmt` helper,
  the private-data contract with the kernel driver
  (`viogpu_wddm_driver.h`), and a virgl winsys.
- **Yttrium's closed package proves the shape runs on our host**: a WDDM
  desktop on the virtio-vga function with Venus contexts (ADR-0008). What it
  does not prove is any of our code.
- **Our own pieces that carry over unchanged**: the host's Venus executor and
  zero-copy scanout of blobs, Mesa's Venus driver with our Windows fixes, Zink,
  DXVK, the build/sign/package/install path, and the vsync timer.

## Decision

One WDDM driver on the display function, built from `viogpu3d`, that carries
**Venus** instead of virgl, with **Zink under `d3d10umd`** as its Direct3D
user-mode driver:

```
DWM, D3D10/10.1 programs ─► d3d10umd ─► Zink ─┐
D3D9/11 programs ─────────► DXVK ─────────────┼─► Venus ICD ─► entangled-wddm.sys ─► virtio-gpu ─► executor ─► host GPU
OpenGL programs ──────────► Zink (WGL) ───────┤      (D3DKMTEscape)   (WDDM, display function)
Vulkan programs ───────────────────────────────┘
                 flip: SetVidPnSourceAddress ─► SET_SCANOUT_BLOB ─► the host's zero-copy scanout
```

1. **The kernel driver** (`entangled-wddm.sys`) is `viogpu3d` at a pinned
   commit plus patches, fetched and patched the way `entangled-display` is.
   It gains what Venus needs and virgl did not: `RESOURCE_CREATE_BLOB` with
   mapping through the shared-memory BAR, `SUBMIT_3D` on a ring index with
   fences, context init with the Venus capset — the same four things
   `entangled-gpu.sys` already does in KMDF, reached through `D3DKMTEscape`
   instead of IOCTLs. A flip scans out the blob behind the primary
   (`SET_SCANOUT_BLOB`), which is the host path Linux guests already use. The
   vsync is our grid timer, reporting `CRTC_VSYNC`.
2. **One device.** Venus moves to the display function (`venus = true` there,
   as for Linux), because a flip names a resource and resource ids are per
   device. The render-only function and `entangled-gpu.sys` stay until this
   replaces them, then go.
3. **The Venus renderer backend** (`vn_renderer_entangled.c`) gets a second
   transport: the escapes of the WDDM driver. Vulkan programs, DXVK and Zink
   keep working through the same ICD, now registered by the display driver's
   INF so Windows lists a Vulkan-capable adapter.
4. **The Direct3D UMD** is `d3d10umd` with MR !24223's WDDM integration,
   creating a Zink screen instead of a software one. It serves what cannot
   take DXVK's DLLs — DWM first — at Direct3D 10. Programs that want
   Direct3D 11 keep DXVK; whether the UMD can be raised to 11 is not part of
   this ADR.

### Why not a thin WDDM driver that leaves rendering on the render-only node

Tempting: keep `entangled-gpu.sys` for all rendering and make the display
driver a WDDM shell whose `Render`/`SubmitCommand` complete at once. It fails
at the flip: the primary DWM flips to would be a resource of the *other*
device, so showing it needs either a copy or a host mechanism that lets one
virtio-gpu device scan out another's resource — a second copy of the overlay
plane, with the same "which one is in front" problem this ADR exists to
remove.

### Why not virgl, which the existing code speaks

It would need virglrenderer on Windows hosts — a second renderer, GL on the
host, and a second trust boundary to harden (ADR-0004's executor exists
because we did not want that).

## Phases

Each ends in something that runs in the test guest. Nothing later starts
before the one before it is measured.

| Phase | Deliverable | Done when |
|---|---|---|
| W0 | `guest/windows-gpu/wddm`: `viogpu3d` at the pinned commit, built, test-signed and installed as it is | the guest boots to a desktop on it (Basic Render Driver still composing — the driver's own UMD is absent); a clean shutdown and reboot; our vsync patch applied and the 60 Hz peak re-measured |
| W1 | Venus through the WDDM driver: blob resources, the shared-memory BAR, `SUBMIT_3D` with ring index and fences, as escapes; the Venus backend's second transport | `vk-smoke` 9/9 and `vk-present` on the display function with the render-only node switched off |
| W2 | Flip to a blob: `SetVidPnSourceAddress` → `SET_SCANOUT_BLOB` | a full-screen Vulkan program reaches the host with no CPU copy and no overlay plane (`shared` in the pacing report) |
| W3 | `d3d10umd` + MR !24223 on Zink, as the driver's UMD | `dxdiag` lists the adapter with Direct3D 10; the UMD's own triangle test draws |
| W4 | DWM on it | the desktop composes on the GPU: the console benchmark and a full-screen video measured against today's 79–92 and 60–75 fps, CPU use beside them |
| W5 | Everything else moves over: DXVK and Zink on the one ICD, Edge without switches or policies, the render-only node and `entangled-gpu.sys` removed, `install windows --gpu3d` installing this | a fresh install passes the checks ADR-0009 lists, with one driver |

### W0, as far as this host takes it (2026-11-11)

`guest/windows-gpu/wddm` fetches `viogpu3d` at the pinned commit, patches and
builds it (`prepare.ps1`, `build.cmd`, `entangled-wddm.inf`), and it was
installed in the Windows 10 test guest over `entangled-display`, live. It does
**not** reach a desktop on this host, and cannot as it is:

- **Windows takes the driver.** `StartDevice` succeeds, then dxgkrnl queries
  the adapter, creates a device and allocations, sets the primary's address
  and presents (eleven `Present` blits logged) — a WDDM adapter, for about
  twenty seconds.
- **The host refuses what it sends.** `CTX_CREATE` with context type 0
  ("classic virgl only"), `RESOURCE_CREATE_3D` ("this renderer serves only
  Venus contexts and has no 3D resources"), and then every command that names
  those: attach, scanout, transfer, flush. The primary is a virgl 3D resource
  and a present is a virgl blit; a Windows host has neither. Windows then
  stops the adapter and falls back to the Basic Display Driver — a desktop,
  no blue screen, no black one.
- **Two things Windows insisted on**, found on the way: with no
  `UserModeDriverName` in the INF dxgkrnl stops the adapter straight after
  `StartDevice` (device code 43, not one query made) — naming a DLL that does
  not exist yet is enough; and the driver needs MSI on (upstream's INF turns
  it off).
- **The way back works**: `gpu-test install` of `entangled-display` on the
  same function, live; the desktop came back at 1920×1016 with both functions
  on Entangled drivers. The two machine-wide registry values the INF sets
  (`EnablePreemption`, `TdrDebugMode`) were deleted by hand afterwards.
- **The fork ships with its debug mode forced on** (`#define DBG 1`,
  messages to a kernel debugger). Our patch turns it off and adds a
  diagnostic build instead: `ENTANGLED_SERIAL_TRACE=<level>` sends the
  driver's messages to COM1, which the VM's log carries. Everything above was
  read from that log; without it code 43 was all there was.

Not done of W0: a desktop on the driver, the restart, and the vsync patch
with its measurement — all behind the primary.

**So the order changes.** The driver has to stop needing virgl before it can
show anything here, which is most of what W1 and W2 were:

| Phase | Deliverable | Done when |
|---|---|---|
| W0′ | the primary without virgl: allocations dxgkrnl presents into are guest memory behind a 2D resource (`RESOURCE_CREATE_2D`, backing, `TRANSFER_TO_HOST_2D`, `FLUSH`), a present from system memory is a copy into it; no context, no 3D resource | the guest boots to a software-composed desktop on `entangled-wddm`, survives a restart, and with the vsync patch repeats the 60 Hz single peak |
| W1 | as above (Venus through the driver), now on a driver that displays | as above |
| W2–W5 | unchanged | unchanged |

W0′ buys nothing a user would see — it is `entangled-display`'s desktop on a
WDDM driver — but it is the first version of this driver that runs on our
host, and every later phase is tested on top of it.

### W0′: the kernel half works; Windows will not keep it without a Direct3D driver (2026-11-11)

`0002-a-primary-without-virgl.patch`: on a host that offers no virgl capset
(`IsVirglHost`, capset ids 1 and 2 — ours offers Venus, id 4, only) the driver
makes no context and no 3D resource. Every allocation is a 2D resource over
the guest pages Windows maps into the aperture, the same pages are mapped
into the kernel, and a present is copied by the driver (`CopyFrom`, on the
command thread, rectangle by rectangle) and sent with
`TRANSFER_TO_HOST_2D` + `RESOURCE_FLUSH`; a flip transfers the whole new
primary once. On a virgl host nothing changes.

In the test guest, for the twenty seconds it lived:

- the host accepted everything but nothing-to-do `CTX_CREATE`/`CTX_DESTROY`
  calls from the device's constructor (since guarded): three 2D resources of
  1920×1016, `scanout set ... source=TwoD` on the driver's primary, and
  transfers and flushes as the desktop changed;
- then **`dwm.exe` crashed** (Application Error 1000, exception `0xe0464645`,
  01:48:23), and two seconds later Windows destroyed the devices and removed
  the adapter, back to the Basic Display Driver. No driver error, no blue
  screen; the System log has nothing about the display.

So the state W0′ aimed at does not exist on Windows 10: DWM does not
compose in software on top of a WDDM adapter whose user-mode driver cannot
be loaded. It needs a Direct3D device from that adapter's driver, and after
it has crashed over it Windows takes the adapter away. (That it is the
missing UMD, and not something else in how this driver answers DWM, is the
reading of one crash record, not a tested fact: the only test is a UMD.)

Not done: a desktop that stays, the restart, the vsync patch and its
measurement.

**What it changes.** The order again: there is no stable driver before a
user-mode driver exists, so the Direct3D UMD comes *first*, in software, and
Venus afterwards:

| Phase | Deliverable | Done when |
|---|---|---|
| W3′ | `d3d10umd` as this driver's UMD with a **software** rasteriser (Mesa's llvmpipe or softpipe, as the frontend runs mainline), plus the WDDM integration of MR !24223 so its allocations and presents go through our kernel driver's 2D path | DWM stays up on `entangled-wddm`; a restart; the vsync patch and the 60 Hz measurement |
| W1, W2 | Venus through the driver, the flip to a blob | as before |
| W3 | the same UMD on Zink instead of the software rasteriser | as before |
| W4, W5 | unchanged | unchanged |

W3′ is slower than today's desktop (a software Direct3D under DWM instead
of DWM's own), and exists to be replaced; what it proves is the part of this
plan most likely to fail — DWM accepting `d3d10umd` at all — before any GPU
work is spent on it.

### What was built: DWM on the host GPU, shown through the overlay plane (2026-11-11)

The phases above were written before any of them ran. This is what running
them made of the plan; where the two disagree, this section is the record.

**The driver stack.** `entangled-wddm.sys` on the display function, with
`entangled_d3d10.dll` as its Direct3D user-mode driver: Mesa's `d3d10umd`
frontend with the WDDM integration of Mesa MR 24223 forward-ported to 26.2.6,
on **Zink**, on the Venus driver ADR-0009 already ships, through the
render-only node. A process that gets no Vulkan device falls back to softpipe
(the secure desktop's processes do: `vkEnumeratePhysicalDevices` answers
`VK_ERROR_INITIALIZATION_FAILED` there, not yet explained). The machine boots
on it, DWM composes with it, UAC's secure desktop, Explorer and Edge run on
it, and a guest restart comes back on it.

**W1 is gone.** The plan had Venus travel through the WDDM driver, as
upstream's `viogpu3d` does for virgl. Nothing needs it: Zink in the user-mode
driver loads the Venus ICD like any Vulkan program, and the ICD talks to
`entangled-gpu.sys`. The WDDM driver stays what W0′ made it — a 2D primary
over the guest's pages, a vsync on a grid, the escapes below — and carries no
3D of its own.

**W2 is the overlay plane, not a flip to a blob.** A present copies DWM's back
buffer, on the GPU, into a buffer in host-visible memory (one per back buffer,
kept), waits for that copy, and has the Venus driver show the buffer's memory
over the whole screen: `entangled_present_overlay`, the overlay hook of
ADR-0009 phase E exported by name from `vulkan_virtio.dll`, called with the
`VkDeviceMemory` Zink allocated (`zink_entangled_frame`, our addition to
Zink). No frame passes through the guest's CPU. The host still uploads the
plane's pixels from those pages to its own texture, as it does for a window's
frames; a flip of the device-local image itself, as Linux guests have
(ADR-0004's handle blobs), would remove that too and is not done.

While the plane covers the screen the primary under it is not what anyone
sees. The user-mode driver says so to the kernel driver through an escape
(`0x901`, a two-second lease it renews); under the lease a flip refreshes the
primary once a second instead of every frame, and the allocation gets its
pixels about as often. That second-old primary is what a screenshot or the
manager's thumbnail reads.

**One plane, one owner.** A window's swapchain and the desktop cannot both be
on the plane. While DWM presents through it, it holds a named event
(`Local\EntangledDesktopOverlay`); the Venus driver's window path
(`wsi_win32_overlay_place`) sees it and presents the ordinary way, which DWM
then composes on the GPU. So under this driver a Vulkan window (Edge with
ANGLE on Vulkan, a DXVK game) loses its own zero-copy path and goes through
the window's GDI surface. Not measured; it is the first thing to measure
before this is anyone's default. `ENTANGLED_D3D10_OVERLAY=off` in DWM's
environment gives the plane back to windows and the desktop its copies.

**Textures shared between processes are still copied by the CPU.** A Direct3D
program that renders through this driver and hands its surface to DWM does so
through the WDDM allocation: the producer reads its GPU texture back into the
allocation at each flush, the consumer uploads it before sampling. Correct,
and slow; a real shared GPU texture (a Venus blob both processes import) is
the remaining piece of the plan's W2/W3 and is not done. DWM's own back
buffers do not take this path any more.

**Measured** (this host, RTX 2070, guest at 2844×1406, the display advertised
at 120 Hz, text scrolling in a console window, frames counted at the host's
virtio-gpu device):

| | frames a second |
|---|---|
| DWM on Zink, every present read back by the CPU (two 16 MiB copies a frame) | 30 |
| the overlay plane, the primary still refreshed every frame | 65 |
| the overlay plane, the primary under the lease | 76 |

The interval histogram of the last has its peaks at one and two refresh
periods (7–8 ms and 16–17 ms). The display-only driver's figures in ADR-0008
(58 at 60 Hz, 73–94 at 120 Hz) were taken at 1920×1080 and are not the same
measurement; the comparison at one resolution has not been made.

**What it took, in the order it bit:**

- *A vectored exception handler outlives its DLL.* The diagnostic handler
  stayed registered after the driver was unloaded, and the next debug string
  in the process jumped to it. Removed at unload.
- *`tgsi_to_nir` has no Shader Model 4 sample opcodes.* Zink translates TGSI
  through it; softpipe never did. `SAMPLE … SVIEW[n], SAMP[m]` reached an
  `UNREACHABLE`, which in a release build is a jump through a table to
  anywhere. The frontend's "old texture operations" (made for virgl) are on
  for Zink too. They assume the texture's index is the sampler's: a shader
  that samples texture 1 with sampler 0 is wrong until `tgsi_to_nir` learns
  the real opcodes.
- *A flush inside a flush.* Zink flushes from inside a read-back map; the
  hooked flush read back again, a new 16 MiB buffer per level, until the
  context's host-visible gigabyte was gone.
- *The Venus backend for Windows never released a buffer object.*
  `vn_renderer_bo_unref` drops the last reference and then calls the backend's
  destroy; ours dropped it a second time and returned. Every mapped
  `VkDeviceMemory` of every Vulkan program in a Windows guest kept its host
  pages until the process ended — ADR-0009's bug, found here because DWM
  frees sixteen megabytes sixty times a second. Fixed in
  `vn_renderer_entangled.c`; **the published `gpu3d` package still has it**.
- *The host refused `vkCreateBufferView` with a usage link.* Zink 26.2 chains
  `VkBufferUsageFlags2CreateInfo` on every buffer view; the executor now takes
  it when `VK_KHR_maintenance5` is enabled and the usage narrows the buffer's
  texel usages.
- *A screen on its way out flushes; a shader deleted while bound.* Two more
  places softpipe forgives and Zink does not, both in the screen factory's
  hooks (`d3d10_wddm.c`).
- *A `StartDevice` that fails before `HWInit` bugchecks in `RemoveDevice`.*
  Upstream's teardown assumes the hardware was opened. The development
  start-once gate sat there for two builds, which is a boot into the recovery
  environment; it now sits after the hardware is up and is unused by the INF.

**Working on it.** A diagnostic build of the kernel driver
(`ENTANGLED_SERIAL_TRACE`) writes to COM1, which the VM's log carries, and
takes text from the user-mode driver through escape `0x900` — Mesa's own log
included, by catching the debug-string exception. The crash report it prints
gives module offsets the linker map resolves. A guest that will not come up
on the driver boots with `adapter = "framebuffer"` (the driver never loads)
and has the driver store's copies replaced there.

**Shipping.** The 3D package's workflow builds the driver and its user-mode
driver and the installer copies them into the guest (`%ProgramFiles%\Entangled\wddm`),
but nothing installs them: `wddm-on.cmd` does, `wddm-off.cmd` goes back to
the display-only driver. It stays opt-in until the list below is short. The
workflow change has not run.

**Still owed before this is a default** (as it stood; the second pass below
replaces this list):

1. the window path under the desktop's plane, measured (Edge video, a DXVK
   game), and a decision about who owns the plane in full screen;
2. shared textures between processes without the CPU;
3. the real sample opcodes in `tgsi_to_nir`, or DXBC straight to NIR;
4. why the secure desktop's processes get no Vulkan device;
5. upstream's machine-wide settings the INF still carries (preemption off,
   TDR debug mode), and its "restart_index … unsupported primitive topology"
   message on every indexed draw;
6. a soak: hours, a resolution change, sleep and resume, a snapshot restore;
7. the comparison with the display-only driver at one resolution.

### The second pass: programs other than DWM (2026-11-11)

The record above ends with a list of what was owed. This is what working down
it found; the list after it replaces that one.

**A Direct3D program's window reaches DWM.** It did not: the Start menu was an
empty panel. Three separate things were wrong.

- *The overlay path ran in every process.* Any buffer with a present bind
  counted as "presentable", so a windowed program's present took the host's
  plane for itself and skipped the copy DWM was waiting for. Only a primary is
  presentable now; the frontend marks one (`PIPE_BIND_SCANOUT` when the
  runtime passes a primary description).
- *DWM does not sample a window's surface, it copies from it.* The fetch of an
  opened texture hung on `set_sampler_views` alone; it is on the source of a
  blit and of a region copy too.
- *An opened allocation cannot be read through WDDM here.* `pfnLockCb` on an
  allocation the caller only opened answers `E_INVALIDARG`, read-only or not,
  and the kernel driver has no mapping either: it maps an allocation's pages
  when Windows puts it in the aperture, which never happens to one nothing
  renders with. A named section would do between ordinary processes and not
  for the ones that matter — the Start menu, Search and every Store app run in
  an app container with a namespace of their own. So the kernel driver holds
  the pixels: the producer writes its frame in at each flush (escape `0x903`),
  whoever opened the allocation reads it out (`0x902`), into paged pool the
  size of the surface, with a write count so that a reader that already has
  the frame is told so and copies nothing (DWM binds every window every time
  it draws). The Start menu, Search and the Calculator render.

  This is two CPU copies per changed frame of each window, and it is the
  honest state of "shared textures": correct, and not the GPU sharing the plan
  wanted. The producer's lock is also held only for the length of a copy now.

**App containers get the GPU.** `StartMenuExperienceHost.exe`, `SearchApp.exe`
and the Store fell back to softpipe — it was they, not the secure desktop
(`consent.exe` has had Zink all along): the render-only node answered
"access denied". Its INF now gives every user and `ALL APPLICATION PACKAGES`
read and write, with a low-integrity label, as a GPU's device object has.

**A mode larger than 16 MiB took the guest down.** Resizing the window to
3836×1952 was a bugcheck: upstream sizes its frame segment at 16 MiB ("FIXME")
and the message for "too small" read the size through a pointer not yet set.
The segment grows to the mode being set (`0005`); 1600×900, 2560×1440,
1280×720 and 3840×2160 in a row, and a guest restart after them, are clean.

**A guest access the host cannot emulate no longer ends the machine.** On one
restart the VM died with "the WHP instruction emulator could not complete
MMIO": the guest had switched the render node's memory decoding off and, 19 ms
later, a program was still copying into a blob with a vector move — an address
that decoded nothing, an instruction the emulator does not know. The WHP
backend now injects `#GP(0)` at that instruction instead of failing the vCPU:
the guest's own kernel ends the program, as hardware's master abort would have
left it to. Not reproduced since, so the injection itself has run only in its
unit test.

**Who owns the plane: the desktop.** `vk-present` (300 frames, a small window,
unthrottled) runs at 212 and 183 frames a second under this driver, presented
through the window and composed by DWM, against 452 and 535 on the
display-only driver with the plane to itself. Both are several times any
display, so the rule stays as built — while the desktop is on the plane, a
window is not. A window the size of the screen has not been measured.

**The same resolution, both drivers** (2844×1406, 120 Hz advertised, text
scrolling in a console):

| | updates a second |
|---|---|
| display-only driver (`entangled-display`), flushing the rectangles that changed | 120 |
| this driver, whole frames on the plane | 72–76 |

So at 120 Hz the display-only driver is ahead on this load, and at 60 Hz the
two are level. A present costs this driver about 2.5 ms (2.3 ms for the GPU's
copy of the frame and the wait for it, 0.2 ms for the host to take it); the
rest of a frame is DWM's own drawing through Zink and the Venus ring, which
fits a 16.7 ms period and misses every other 8.3 ms one. That, not the
present, is what there is to speed up.

**Open, seen once or twice:** `SearchApp.exe` twice died of heap corruption
found in `vn_DestroyPipeline` (the host having refused a `vkDestroyPipeline`
of an id it never made, once), and not in the last runs. Not understood.

**Still owed before this is a default:**

1. the cost of a window the size of the screen under the desktop's plane
   (a video, a DXVK game), and with it whether a full-screen window should
   take the plane (a Direct3D 9 game's does since ADR-0011's N4: it claims
   the plane and this driver stands aside, `d3d10_wddm_plane_claimed`; a
   DXVK or Direct3D 10 one does not yet);
2. shared surfaces on the GPU instead of through the kernel driver's copy,
   and a blt-model present, which still copies between allocations that are
   not mapped (so does nothing);
3. the real sample opcodes in `tgsi_to_nir`, or DXBC straight to NIR;
4. DWM's frame time on Zink over Venus, for displays faster than 60 Hz;
5. the `SearchApp.exe` heap corruption;
6. upstream's machine-wide settings the INF still carries (preemption off,
   TDR debug mode), and its "restart_index … unsupported primitive topology"
   message on every indexed draw;
7. a soak of hours, sleep and resume, a snapshot restore;
8. the workflow and `install windows` with the new package, neither of which
   has run.

### Amendment, 2026-11-19: DWM's 3D composition corrupts — narrowed, not fixed

*Found and fixed the next day: see the amendment of 2026-11-20 below.*

A user prototype ("wintanyl") asks DWM for live window thumbnails as
composition visuals (`dwmapi!#147`) and moves them in 3D: perspective
transforms, the interop compositor, `CompositionVisualSurface` on sprites.
Everything is drawn by `dwm.exe` on this driver stack; the same executable is
clean on a host with a vendor driver. In the guest it is not. What a day of
reproducing it established:

**How it was looked at.** A sparse copy of the Windows 10 test disk, the
engine headless with `--control-stdin`, the program started with the control
channel's `exec`, and bursts of `screenshot` (full-size, a second apart). A
headless run has no window, so the overlay plane declines every present
(`0 overlaid` in the driver's statistics) and DWM's frames go the CPU way:
read back into the primary's allocation at each present, then a **flip**.
The kernel driver's serial log shows three primaries in turn (allocations 1,
2 and 7), and each flip transfers the whole allocation to the host
(`FlushToScreen`), so a screenshot is exactly the back buffer DWM rendered.
The corruption is in that buffer — DWM's drawing through `d3d10umd` on Zink
on Venus — not in the transport behind it.

**What it looks like** (the same in all four of the program's modes,
including the plain DirectComposition one the report called clean — it has no
dimmed backdrop, so stale content is harder to see there):

- axis-aligned rectangles of *old* content — the desktop as it was before
  the program started, un-dimmed, with a window at its own place — inside the
  region that animates, changing from frame to frame (three states, one per
  buffer, is the pattern);
- the program's full-screen backdrop drawn only inside a sub-rectangle
  (different from boot to boot), and the thumbnails outside it missing;
- thin vertical and horizontal lines at the edges of those rectangles, a grey
  polygon now and then, and in the DirectComposition mode a black triangle in
  the lower-left corner that stays.

**Ruled out.** No Venus `FATAL` and no command the host refused in any run;
the only Mesa complaint is the known `restart_index … TRIANGLE_LIST` line. The
DXGI `Blt` entry point, which the frontend leaves unimplemented, is never
called (it would log `_Blt XXX`). The transport, as above.

**Tried and measured as no help.** The frontend's
`_RotateResourceIdentities` (DXGI's flip) swaps the `pipe_resource` pointers
behind the swap chain's resources, and a view refreshes when it is next
bound — but what is *bound* at that moment keeps the old texture, and Direct3D
state survives a present. That is a real gap: after a rotation, a back buffer
left bound is drawn into while another one is shown. A build that re-points
the bound render targets and shader resources at each rotation was installed
in the test guest and compared with the shipped one on fresh boots, same
procedure: no visible difference. It is not in the patch set; the diff is
kept with the session notes, worth carrying once the real cause is found.

**What the fix needs, in order.**

1. The same procedure with a window, so the overlay plane takes DWM's frames
   from the GPU: if the artifacts go, the readback path is implicated after
   all; if they stay, it is DWM's rendering on this stack. (The report's
   pictures and this repro were both headless.)
2. A trace of the DDI calls of one corrupted DWM frame — scissor rectangles,
   viewports, `ResourceCopyRegion` boxes, clears, render-target binds per
   draw — against the region that comes out stale. The shape of the damage
   (rectangles of old content where DWM updates part of a buffer) points at
   partial updates: a scissor or viewport that is not what DWM set, a copy
   region with a box, or a render target that is not the one bound.
3. Zink's handling of DWM's intermediate render targets (`SetRenderTargets`
   onto offscreen textures, then sampling them): the clipped backdrop is what
   a stale scissor would draw.

### Amendment, 2026-11-20: DWM's 3D composition corruption — found and fixed

None of the three leads above was it. The scissors, viewports and copy boxes
DWM sends are what reach Zink, and its intermediate render targets work. The
geometry was wrong: **DWM's vertices were overwritten on the GPU after DWM
wrote them.** A one-line change in the user-mode driver fixes it, and the
three modes of the wintanyl prototype (`show`, `hybrid`, `surface`) are clean
on the 3D stack.

**How it was found.** The Windows 10 test machine `b` of wintanyl, headless,
with the program driven through the control channel and full-size
`screenshot` bursts as before. `d3d10_wddm.c` gained a trace of DWM's frames
(below; it stays in the driver, dormant): every Gallium call that reaches
Zink for a few presents, plus each presented frame written to a file in the
guest. The first trace showed the damage inside the presented buffers and
the partial-update pattern of DWM (per-frame dirty scissors, back-buffer
tiles copied into a 256×256 scratch texture and drawn back for the backdrop
effects), all consistent — so the state was right and the draws' inputs
were not.

Tried on the way and measured as no help: `ZINK_DEBUG=noreorder` in DWM (the
copies into the scratch texture are not reordered past its draws); DWM on
`ENTANGLED_D3D10_DRIVER=softpipe`, which presents black — that path does not
work for DWM at all, so it cannot be an A/B arm.

**The A/B that settled it.** A switch in `ResourceMap`, read from the
environment of DWM, bit 1 dropping `PIPE_MAP_UNSYNCHRONIZED` and bit 2 the
discard flags. Same DLL, same boot procedure, `show` mode, six shots each:
`0` corrupt, both bits clean, **bit 1 alone clean** — so it is the
unsynchronized write of Direct3D's `MAP_WRITE_NOOVERWRITE`.

**The mechanism, from the trace with Zink's side of each map added.** DWM
streams vertices into dynamic buffers of 16 000, 160 000 and 240 012 bytes:
`WRITE_DISCARD` when it wraps, then a series of `WRITE_NOOVERWRITE` appends.
A Direct3D 10 map has no range, so every map is of the whole buffer. Zink
answers the discard with `invalidate_buffer`, which refuses when it thinks
the buffer idle (`!zink_resource_has_usage`); the map then falls back to a
**temporary buffer whose copy into the real one — all 160 000 bytes — is
recorded in the batch** and runs later on the GPU. The appends that follow
are unsynchronized maps of host-visible coherent memory (`hv 1 coherent 1
staging 0`): DWM writes the next vertices straight into the buffer, the
pending copy lands on top of them when the batch executes, and the draws
read whatever the temporary buffer held there. Stale geometry, sprites at
old places, degenerate quads (the thin lines), missing thumbnails, and
regions of a buffer DWM believes it redrew left as they were — every symptom
of the 2026-11-19 amendment. The trace (one frame):

```
DDI Map 30480 buffer 160000x1 sub 0 map 4 (DISCARD)        usage 0x82
zink map 30480 [0+160000] hv 1 coherent 1 staging 1 ... usage 0x2a
DDI Map 30480 buffer 160000x1 sub 0 map 5 (NOOVERWRITE)    usage 0x22
zink map 30480 [0+160000] hv 1 coherent 1 staging 0 ... usage 0x22
... five more direct appends before the batch is flushed
```

Gallium allows an unsynchronized write only where the caller knows nothing
pending touches the range; the frontend cannot know about a copy the driver
made up itself, and Zink does not check its own pending copies for an
explicit `UNSYNCHRONIZED` map (it does when it infers one).

**The fix** (`0002`, `Resource.cpp`): `MAP_WRITE_NOOVERWRITE` maps as a plain
`PIPE_MAP_WRITE`. Zink then sends a write to a buffer the batch still uses
through its own staging copy, in order and without waiting, so the measured
cost is nil. The same driver build, installed in `b`, from fresh boots:

| Mode | before (3D) | after (3D) | WARP (`adapter = "framebuffer"`) |
|---|---|---|---|
| `show` | corrupt, ~58–60 tick/s | clean, 61.9 tick/s | clean, ~52 tick/s |
| `hybrid` | corrupt | clean, 60.9 tick/s | clean, ~47 tick/s |
| `surface` | corrupt | clean, 62.1 tick/s | clean |

Tick/s is the program's own loop (its `Commit` rate), the number wintanyl
quotes. What DWM itself put on the screen in those runs, from the host's
frame-pacing log, was **12–21 frames a second**: a headless run declines the
overlay plane, so each present reads the 1920×1080 frame back through the CPU
(the W2 path), and that, not the GPU, is the ceiling. A run with a window
(the overlay plane) is the measurement still owed. *(Both measured, and the
headless readback removed, in the amendment that follows.)*

**The trace, for next time.** In `dwm.exe` only, every present looks at
`C:\Users\Public\dwmtrace.txt`; writing a number into it (e.g. `cmd /c echo 3
> C:\Users\Public\dwmtrace.txt`) traces that many presents through the
diagnostic kernel driver's COM1 lines (`T …`) and writes each presented frame
to `C:\Users\Public\dwmframe-<n>.raw` (BGRA, screen size). A file older than
the DWM process does not count. Draws are traced at the DDI (Zink swaps its
`draw_vbo` on every state change, so a Gallium hook misses them).

**Left over.**

- The Direct3D 9 driver (Nine) has its own buffer uploads and was not looked
  at; a game streaming vertices with `D3DLOCK_NOOVERWRITE` on Zink deserves
  the same check.
- Zink's fallback itself (a full-buffer temporary copy after a refused
  invalidate, under later explicit unsynchronized maps) is an upstream bug
  worth reporting with this trace; the frontend change does not depend on it
  being fixed.
- The dev agent was quarantined again by Defender in the guest mid-session
  (`Wacatac.H!ml`, deleted from the virtio-fs share it was started from);
  driving fell back to the Run box.

### Amendment, 2026-11-20 (later): DWM's frames stay on the GPU with no window

The owner's rule for this path is that a frame does not go through a CPU, with
a window or without. Headless, it did: every present of DWM's desktop was read
back into the primary's allocation and flipped. That is the path every
wintanyl battery runs on.

**Measured first.** The user-mode driver's statistics gained a breakdown,
printed every two seconds from each process:

- `presents: N of the screen in T ms: … whole, … readback map, … readback memcpy, … flip`
- `flushes: … ms in the driver's flush`

The readback is split into the map (Zink's copy to memory, waited for) and
the copy out of it on the guest's CPU; the flush is Zink's submit, which waits
for the host when the batch carries a fence.

On machine `b`, headless with the engine as it was, wintanyl's PoC on the 3D
adapter gave these numbers. Each window is about 26 s and includes the
program's start-up:

| | `show` | `hybrid` | `surface` | effects (`fx jelly`, `fx lamp`) |
|---|---|---|---|---|
| DWM presents a second | 8.8 | 15.3 | 1.3 | 5.1 |
| readbacks (8 MiB each) | 1 618 | 1 117 | 1 318 | 2 892 (in 129 s) |
| bytes copied by the guest's CPU | 492 MiB/s | 338 MiB/s | 415 MiB/s | 177 MiB/s |
| present time in a failed overlay call | 3.1 s | 6.1 s | 0.4 s | 7.4 s |
| the program's own tick/s | 62.0 | 62.0 | 62.0 | — |

There were about seven readbacks per present. Every flush with a dirty
primary reads it back too, which `d3d10_wddm_flush` skips only while
`ws->overlaid` holds. In `surface` the screen hardly changed between
screenshots a second apart.

**Why the plane declined.** A window installs two presenters on its GPU: one
for the desktop and one for the overlay plane (`renderer.rs`). A headless
display got only the first (`attach_offscreen_gpu`). The render node's
`OverlaySink` therefore never accepted host pages. The device's copy path read
the frame on the host's CPU instead, and the guest saw
`entangled_present_overlay` fail and fell back to the readback.

**The fix** (`display`): `attach_offscreen_gpu` gives the overlay plane its
presenter on the same off-screen device (`GpuScanout::open_offscreen_pair`),
as a window does. The host then logs *"presents a renderer blob's host pages
through the display's GPU: no readback, no CPU copy"*. The guest presents
every frame on the plane (`N overlaid` = presents), and the readback runs only
under W2's lease: once per 0.9 s, about 8 MiB/s. A screenshot from the control
line reads the plane's texture back from the GPU (`gpu_overlay`) when it is
asked, never before. Screenshots in the guest (`BitBlt` of the screen, which
wintanyl's effect tests take) still show the current frame: the lamp's
mid-animation shot catches the trapezoid. The framebuffer profile has no
Venus, so it never calls this; its screenshots are as clean as
`evidence/warp-*.png`, at 54.4 / 51.9 / 5.9 tick/s.

After the fix, same machine, same program:

| | `show` | `hybrid` | `surface` | effects |
|---|---|---|---|---|
| DWM presents a second | 22.9–27.6 (one run 7.0) | 19.6–39.8 | 1.4–2.6 | 10.8 |
| readbacks | 28 | 26–28 | 12–15 | 78 (in 81 s) |
| bytes copied by the guest's CPU | 8 MiB/s | 7–8 MiB/s | 3–4 MiB/s | 7 MiB/s |
| the program's own tick/s | 61.3–61.6 | 61.6 (one run 41.5) | 61.3–61.6 | — |

Two other agents' VMs shared this host's GPU and CPU during these runs. That
explains the spread in frames a second between runs. The readback counts do
not depend on it.

**With a window.** The code path has no CPU copy:

1. The window's overlay presenter imports the blob's host pages.
2. It copies them into its texture on the GPU.
3. The renderer samples that texture over the desktop.

Measured with the engine's window on a separate Windows desktop (zitter's
`windows_test_desktop.py`, so off the owner's desktop and never visible): DWM
made 13.0 / 28.9 / 2.0 presents a second (`show` / `hybrid` / `surface`).
Readbacks were the same lease-paced 8 MiB/s, the program ran 60–62 tick/s,
and the screenshots were clean. A window on an invisible desktop may well be
paced differently from one on screen, so these are not on-screen numbers.

**The next ceiling is the flush, not a copy.** After the fix, DWM spent this
share of its wall time inside `pipe->flush`:

| | flush time | of wall time |
|---|---|---|
| `show` headless | 13.6 s | 26 s |
| `surface` headless | 8.6 s | 26 s |
| `show` windowed | 8.4 s | 26 s |
| `hybrid` windowed | 13.2 s | 26 s |

DWM flushes about 200–800 times a second. `vn_renderer_entangled`'s submit is
an `IOCTL` that returns once the host has retired the batch's fence, so a
flush that carries one waits a host round trip. In `surface` that leaves DWM
at about 2 presents a second while the program commits 62 times. This is not
fixed here; it is the next thing to measure.

**What still goes through a CPU, and why it stays for now:**

- **The primary under the plane**, once per 0.9 s, about 8 MiB/s. It is for
  what reads the primary in the guest, and for the moment the plane goes away.
- **A texture shared between processes.** A program rendering through this
  driver reads its window's surface back at each flush for DWM to upload. In
  these runs that was about 20 readbacks per program run. A Venus blob both
  processes import is the fix, as before.

**Not measured.** wintanyl's full grids (jelly 16×10, lamp 48 strips) never
ran. Its demo opens Direct3D 12 only (`wt-scene/src/gpu.rs`). This driver has
no D3D12, so the demo gets WARP12, which reports a CPU device. The demo then
caps itself at `soft_max_tiles` = 40, which gives jelly 8×5 and lamp 20
strips. The effect numbers above are for those grids. wintanyl's own pass/fail
lines were the same before and after, apart from one lamp check. That check
now counts Paint's colour swatches, because a different window happened to be
behind it in this run.

**Defender, again.** On 2026-11-20 at 16:31 the guest's Defender quarantined
the driver itself (`Trojan:Win32/Bearfoos.B!ml`, a machine-learning verdict)
from the driver store, when a newly started process loaded it. Processes that
already had it mapped kept running, so the previous amendment's evidence
stands. The next boot had no `entangled_d3d10.dll`: Windows stopped the WDDM
device (code 43) and DWM composed on the basic display. Machine `b` was
repaired by reinstalling the driver; no exclusion was added. Expect it again
whenever a build is new to Defender's cloud model.

## Risks, in the order they are likely to bite

- **DWM is the strictest Direct3D client there is.** It needs shared
  surfaces, flip presentation and every format it asks for, and a driver it
  does not like gets replaced by the Basic Render Driver without a word.
  `d3d10umd` is unmaintained upstream and was removed for it; W3 and W4 are
  where this ADR can fail, and the fallback is the stack we have now.
- **A WDDM bug is a blue screen or a black one**, and a TDR that recovers
  badly is worse than either. The display-only driver must stay installable
  as the way back, and W0 exists to learn the driver's failure modes before
  our code is in it.
- **No preemption.** The upstream driver claims `PreemptionAware` and does
  none. One guest program that hangs the queue hangs the desktop until
  Windows resets the adapter.
- **Venus in DWM's process.** DWM runs as its own restricted account; the
  ICD, its files and the device access have to work there, and a crash in
  Mesa is a crash of the desktop.
- **Snapshots.** ADR-0006 refuses nothing about a 2D Windows guest today; a
  guest whose desktop lives in Venus state has the same suspend/restore
  story as a Linux GPU desktop, which is not finished.

## Consequences

- Until W4 passes, nothing a user installs changes: this is built beside the
  working stack, not in place of it.
- Test-signing stays a requirement for 3D, and now for the GPU desktop too.
- If W4 fails, W0–W2 are still worth having: a Venus transport on the display
  function removes the render-only node and its rebinding script, and W2's
  flip is the overlay generalised to a full-screen program.

## Sources

- virtio-win PR #943, `viogpu3d`: https://github.com/virtio-win/kvm-guest-drivers-windows/pull/943
- Mesa MR !24223, the Windows WDDM side of `d3d10umd` and WGL: https://gitlab.freedesktop.org/mesa/mesa/-/merge_requests/24223
- ADR-0009, the stack this builds on; ADR-0008's amendments of 2026-11-10 and
  2026-11-11, the measurements quoted above.
