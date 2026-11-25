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

### Amendment, 2026-11-20 (evening): what DWM waits for in its flushes

Once the per-frame readback was gone, `surface` still showed only about 2 DWM
presents a second, with DWM spending 30–52% of its time in `pipe->flush`. The
owner asked for that wait to be removed. This section records:

- what the wait turned out to be;
- the half of it that was fixed;
- the half that is out of reach without the kernel driver.

**Measured first.** Two sets of counters were added.

- In the Venus driver (`vn_renderer_entangled.c`): fenced and plain submits
  (count, time, the longest, how many took over 2 ms) and its own waits. Read
  by name through `entangled_renderer_stats`.
- In Venus's polling loop (`vn_relax`, `0001`): time per reason.

The Direct3D driver prints both every two seconds as `venus:` and
`venus waits:` lines. Machine `b`, headless, wintanyl's PoC, 26–27 s windows.
Another agent's VM shared the host throughout.

| | `show` | `hybrid` | `surface` |
|---|---|---|---|
| d3d10 flushes per DWM present | 38–42 | 30 | 90–154 |
| fenced Venus submits per flush | 1 | 1 | 1 |
| DWM time in fenced submits | 8.6 s | 12.6 s | 6.3 s |
| … of which took over 2 ms | 301 | 314 | 226 |
| DWM time waiting on Venus's ring (`ring seqno`) | 7.5 s | not measured | **13.3 s** |
| … waits that reached a sleep | 511 | — | 827 |
| DWM presents a second | 9.6 | 23.8 | 1.9 |

So the wait had two halves, and in `surface` the larger one was not the fence.

**Half 1: Venus's polling sleep.** Venus waits for the host to reach a ring
sequence number by polling. After a short busy wait it sleeps
`base_sleep_us` (160 µs) and doubles from there, through `os_time_sleep()`.
On Windows that is `Sleep(1)`, and a `Sleep(1)` lasts a whole timer tick,
15.6 ms by default. Every synchronous call DWM's Zink makes through the ring
therefore cost a tick or more. `entangled_wait` had the same `Sleep(1)`.

**The fix** (`0001`: `vn_common.c`; `vn_renderer_entangled.c`): `vn_sleep_us`
sleeps on a high-resolution waitable timer instead
(`CREATE_WAITABLE_TIMER_HIGH_RESOLUTION`, Windows 10 1803 and later). The timer
is created once per thread and kept in a fiber-local slot, whose callback
closes it when the thread ends. A system that refuses the flag gets the old
`Sleep`. The global timer resolution is not touched (no `timeBeginPeriod`).

| | `show` | `hybrid` | `surface` | effects (jelly + lamp, 86 s) |
|---|---|---|---|---|
| ring waits, before → after | 7.5 s → 0.66 s | not measured → under 0.2 s | 13.3 s → **2.0 s** | not measured → 0.85 s |
| DWM presents/s, before → after | 9.6 → 12.0 | 23.8 → 20.7 | 1.9 → **3.5** | 10.8 → 10.5 |
| program tick/s after | 60.5 | 61.5 | 61.1 | — |
| CPU readbacks after (8 MiB each) | 28 | 28 | 21 | 82 |

Screenshots of all three modes are clean. The framebuffer profile does not
load this driver; its screenshots are clean at 51.3 / 53.9 / 5.3 tick/s.

**Half 2: the fenced submit, now the whole of what is left.** After the fix,
DWM's fenced submits took this much of its wall time:

| | fenced submits | over 2 ms | time | of wall time |
|---|---|---|---|---|
| `surface` | 11 389 | 659 | 13.8 s | 51% |
| `show` | 11 187 | 587 | 13.5 s | 52% |
| `hybrid` | 16 128 | 332 | 11.8 s | 45% |

Each flush of the d3d10 frontend is a Zink batch. Zink submits each batch
with a `VkFence`. Venus backs fences with renderer syncs, so the batch ends in
a renderer submit that signals one. This backend makes that submit
synchronous: `entangled-gpu.sys` answers the IOCTL only once the host has
retired the fence.

The host retires fences quickly: 0.46–0.65 ms on average, at most one
outstanding. The guest pays more, because of how the kernel driver waits:

1. It has a single sequential IOCTL queue shared by every process.
2. It polls the used ring for about 2 ms.
3. It then waits in `KeDelayExecutionThread(1 ms)`, which, like `Sleep(1)`,
   lasts a timer tick.

So every fenced submit that takes over 2 ms costs at least 15.6 ms. In
`surface` that was 659 of 11 389 submits, about 7 of its 13.8 s. Every other
process's IOCTLs queue behind that wait.

**Why it is not fixed here.** The useful fix is in the kernel driver, which
cannot be rebuilt here because it would have to be signed:

- **A fenced submit that returns at once.** Several commands in flight, and
  the driver records retired fence ids from the used ring.
- **A way to read those ids** (an IOCTL, or a shared page).
- **A wait that blocks on an event**, instead of polling at the tick.

Any one of these removes the 15.6 ms tail. The first and second together
remove the synchronous wait as well.

A user-mode worker thread that issues the fenced IOCTL on the caller's behalf
was considered and not built. Each fenced submit is paired with a plain one
(12 555 plain to 11 389 fenced in `surface`), so DWM's next IOCTL would queue
behind the worker's in the same sequential queue. The gain would be small, and
the risk to ordering, error reporting and teardown is not.

**An observation, not acted on.** The fence per flush comes from Zink: every
batch is submitted with a `VkFence` that Zink reads later, if at all, to
recycle the batch. Making the renderer sync cheaper for a fence nobody waits
on, or having Zink fence only when it must, would take the wait out of most
flushes. Both are changes to Zink or Venus semantics, and this round did not
make them.

**Counters, for next time.** Every two seconds, each process that has the
Direct3D driver loaded prints:

- `venus: N fenced submits T ms (max …, K over 2 ms), … plain submits …, … waits …`
- `venus waits: ring seqno … (… slept), … semaphore …, query …`

Evidence: `F:\entangled-targets\dwm3d\evidence-flush\` (`summary.txt`, logs,
grids).

### Amendment, 2026-11-21: the timer fix above broke UAC until it was fixed again

**Never take the commit of the timer fix (`ecf335c`, "Venus's polling waits
sleep what they ask for") without the one after it.** On its own it makes
every elevation in the guest fail.

What happens:

1. The fiber-local slot that holds each thread's timer has a callback in
   `vulkan_virtio.dll`, which closes the timer when a thread ends.
2. A process that draws through Direct3D and then unloads the Vulkan driver
   (`FreeLibrary`, with the process still running) leaves that callback
   registered, pointing into an unloaded module.
3. When one of its threads that slept on a timer ends, Windows calls the
   callback and the process dies with an access violation.

`consent.exe` (the UAC prompt) does exactly this. It draws its dialog through
Direct3D, unloads the driver, and dies at `vulkan_virtio.dll_unloaded+0x1d790`
with `0xc0000005`. Disassembly confirms that offset is the slot's callback
(`cmp rcx, -2`, the no-timer marker). Every "Yes" in a UAC prompt then shows
"File system error (-1073741819)", and nothing can be elevated. Machine `b`
was recovered by a boot on the framebuffer profile, where the driver is not
loaded.

**The fix** (`0001`, `vn_common.c`):

- When the slot is allocated, it registers an `atexit` handler that calls
  `FlsFree`.
- The driver links the CRT statically (`b_vscrt=mt`). In such a DLL, `atexit`
  runs at `DLL_PROCESS_DETACH`, which covers both `FreeLibrary` and process
  exit.
- `FlsFree` runs the callback for the threads that still hold a timer and
  removes the slot. After that, no thread's exit can reach the module.
- If `atexit` refuses the handler, the slot is freed at once, and every
  thread sleeps the old way.

Checked on machine `b` with the fixed driver: `whoami /groups` from an
elevated prompt reports the High mandatory level, and no `consent.exe`
failure is logged after the install. Before the fix, three failures were
logged in the same session, all at the same offset.

Evidence: `F:\entangled-targets\dwm3d\evidence-lazy\` (`consent-crash-events.txt`,
`uac-after-fix.txt`).

### Amendment, 2026-11-21: who reads DWM's fences — two ideas measured and rejected

The previous amendment ended on an observation: Zink fences every batch, and
almost nobody reads those fences. This amendment measures that claim. It is
wrong, and both changes built on it were rejected.

**Correction.** Zink does not submit with a `VkFence`. It signals its
timeline semaphore (`screen->sem`) with each batch's id. Venus backs that
semaphore with renderer syncs, and that is what turns every batch into a
fenced submit.

**Who waits.** Two new counters in the Venus driver count every wait with a
timeout and every poll. They are printed as `venus reads:`. A third instrument
records who waits, as call chains: set `ENTANGLED_WAIT_CALLERS=1` and the
Direct3D driver prints `waited by` lines. Resolve those against
`entangled_d3d10.map`.

In `show` (machine `b`, headless), 30 seconds saw 19 674 fenced submits,
18 389 waits with a timeout and no polls. In a 20-second `show` run with the
call-chain recorder on, 15 241 of about 15 900 waits had one call chain:

```
ResourceMap → zink_buffer_map → zink_batch_usage_try_wait → zink_wait_on_batch → zink_screen_timeline_wait
```

So the fence of almost every batch is read, by a mapping that waits for it.
The cause is the corruption fix of 2026-11-20: `MAP_WRITE_NOOVERWRITE` maps as
a plain `PIPE_MAP_WRITE`. For a buffer the current batch still uses, Zink then
flushes the batch and waits for the GPU (`try_wait`), on every append. That is
roughly fifteen round trips to the GPU per DWM frame. The comment there, which
says this costs no waiting, is wrong about the cost but right about
correctness.

**Rejected: signalling lazily.** A renderer could hold a batch's signal back
until someone waits for it, sending held-back signals at a limit of batches,
or when a poll finds them idle. This was built (`ENTANGLED_VENUS_FENCE`) and
measured A/B with the same build:

| | signals held back | sent because someone waited | DWM presents/s, `every` → lazy |
|---|---|---|---|
| `show` | 25 827 | 25 129 | 42.8 → 21.5 |
| `hybrid` | 28 776 | 27 819 | 45.1 → 30.2 |
| `surface` | 11 559 | 11 515 | 2.4 → 1.5 |

97–99% of the signals were waited for. Holding them back only moves the wait
from the submit to the waiter, and costs a fenced submit more on top. The
mechanism is not kept in the driver. Its source and the `vn_queue.c` hook are
in the evidence directory.

**Rejected for now: unsynchronized appends (P1).** The plan had two parts:

1. Map `MAP_WRITE_NOOVERWRITE` as `PIPE_MAP_UNSYNCHRONIZED` again.
2. Remove the hazard of 2026-11-20 at its source.

The source was found. Venus reports no resizable BAR, so Zink marks every
buffer of 8 196 bytes or more `PIPE_RESOURCE_FLAG_DONT_MAP_DIRECTLY`, and
answers a discard of one through a temporary buffer whose copy runs later.

The change built for it (`ENTANGLED_D3D10_NOOVERWRITE=unsync`) had two parts:

- Direct3D dynamic buffers no longer get that flag. A discard of one is then
  either a renamed buffer or a direct map of an idle one.
- As a safety net, each buffer object records the batch of the last
  temporary-buffer copy into it. An explicit unsynchronized write that could
  still meet such a copy, flushed or not, is made synchronous.

Writing through a temporary buffer was not an option: a Direct3D map covers
the whole buffer, so copying the temporary buffer back would overwrite the
appends already in place.

It removes the waits:

| | fenced submits per DWM present, sync → unsync | waits with a timeout in 30 s |
|---|---|---|
| `show` | 15.3 → 2.0 | 18 389 → 1 172 |
| `hybrid` | 10.0 → 2.0 | 12 158 → 1 226 |
| `surface` | 91.6 → 7.4 | 6 980 → 126 |

**It also corrupts DWM's frames.** In wintanyl's effects, the "scene equals
desktop" checks differed by 98.4%, 34.7% and 48.2% of pixels; synchronous maps
give about 0.6%, as in every earlier round. A `show` shot had garbage in its
top-left corner and a stray dark triangle, and `hybrid` drew a dark rectangle
over a background window. All of it came with 0–2 temporary-buffer copies and
0 writes made synchronous by the safety net. So the copy found on 2026-11-20
is not the only thing that makes these writes unsafe here. What else is, is not
known yet.

The rule was that a single trace of corruption sends the default back to
synchronous maps, so synchronous maps stay, and the change is not in `0002`.
The patch, the trace shots and the effect results are in the evidence
directory. The next step for anyone who takes it up is to run the frame trace
of 2026-11-20 (`dwmtrace.txt`) with the patch applied.

**What is left of the wait.** With synchronous maps, each frame still pays
for every append with a flush and a GPU round trip. That GPU round trip is
where the kernel driver's 15.6 ms tail comes in. With unsynchronized maps the
batches grow, and half of the remaining fenced submits take over 2 ms
(1 190 of 2 298 in `show`): the same tail, met by fewer and larger submits.
Removing the tail stays a kernel-driver change, as recorded above.

**Measurement caveat.** Across boots, the host's load changed. Another agent's
VM ran during the first arms, and OBS and NVIDIA Broadcast ran during the
last. DWM's present rate moved with it: the final build, identical in its path
to the first arm, presented 18.5 times a second in `show` against 42.8, while
flushing at the same rate (685 against 670 a second). Per-present and
per-second counts of flushes, fenced submits and waits compare across boots;
presents a second only within one boot.

Evidence: `F:\entangled-targets\dwm3d\evidence-lazy\` (shots `A6-*`, `L8-*`,
`C8-*`, `P9-*`, `F10-*` with their logs, `p1-noover-unsync.patch`,
`lazy-fence-vn_queue.patch`, the effects results `fx-*`).

### Amendment, 2026-11-21 (later): the unknown hazard found — unsynchronized appends are the default

The corruption under P1 came from Zink, not from DWM and not from the host. A
discard that **renamed** a buffer left the old memory bound as the index
buffer, and left the new memory unmarked as in use. P1 maps
`MAP_WRITE_NOOVERWRITE` unsynchronized again, now by default, with that fixed
(`0002`).

**Ruled out first.**

- *The host's fence.* It retires only when the host `VkFence` behind it
  signals, that is, after the GPU has finished (`venus/executor/timeline.rs`).
- *The host's view of the memory.* Host-visible memory is the guest's own
  pages, imported with `VK_EXT_external_memory_host`
  (`venus/executor/memory.rs`). The GPU reads exactly what the guest wrote,
  when it executes, as on real hardware.
- *Zink missing that a still-bound buffer is used across batches.* On a new
  batch, `zink_update_descriptor_refs` marks every bound vertex buffer and
  descriptor as used again.

**The oracle.** A diagnostic build added `ENTANGLED_D3D10_MAPCHECK=1`, which
kept a shadow copy of every dynamic buffer the frontend maps. It checks two
things:

- at a no-overwrite map, that the bytes are still what the last unmap left;
- that a no-overwrite write never touches a range written since the last
  discard. To find the range a discard wrote, the discard map is pre-filled
  with `0xcd`.

In one `show` run under P1, with the corruption on screen, it recorded:

- 45 824 no-overwrite maps (23 331 vertex, 22 493 index);
- 339 discards, none of which handed back the previous address;
- 0 changes behind the program;
- 0 overlaps.

So DWM writes correctly, nothing rewrites its data, and every discard renames.
The GPU was reading the wrong memory.

**The cause.** `invalidate_buffer` gives the resource a new object, then calls
`zink_resource_rebind`, which marks the vertex buffers dirty. Two things were
missing:

1. **The index buffer is never bound again.** `zink_draw` binds the index
   buffer only on a new batch, or when the `pipe_resource` or index size
   changes. A rename changes neither, so the batch kept drawing with the old
   object's indices: thin lines, missing parts of windows, stray triangles.
   The threaded context's `replace_buffer` clears `ctx->index_buffer` itself,
   which is why GL, always threaded, never shows this. The D3D10 frontend
   runs Zink unthreaded.
2. **The new object is not marked as used** by the current batch, for an
   index buffer or a vertex buffer, until the next batch begins. The next
   discard of it within the batch therefore found it idle and mapped it
   directly, writing into memory this batch's draws still read.

In the synchronous build neither could happen. Every discard of a dynamic
buffer of 8 196 bytes or more went through Zink's temporary-buffer copy into
the same object, so there was no rename to miss.

**The fix** is in `invalidate_buffer`, unconditional. After the rename it
clears `ctx->index_buffer` when the resource is the bound index buffer, and
marks the new object used by the current batch when it is bound as an index
or vertex buffer.

The rest of P1 as built earlier stays:

- dynamic buffers are mapped directly, not through `DONT_MAP_DIRECTLY`;
- the safety net, which synchronizes an explicit unsynchronized write that
  could still meet one of Zink's own copies. In every run since, it fired 0
  times, with 0–2 such copies.

`ENTANGLED_D3D10_NOOVERWRITE=sync` returns to plain writes. The Direct3D
driver logs the mode and the two counters as `no-overwrite maps …`.

**Confirmed A/B, same build.** Without the fix, `show`, `hybrid` and the
effects were corrupt (above). With it, every check passed:

- `show`, `hybrid` and `surface`: all 18 shots clean.
- wintanyl's effects: the same six FAILs as every synchronous run, at
  0.57–0.61% of pixels (jelly 0.602/0.571%, lamp 0.587–0.606%), none higher.
- Long run: `show` for 300 s, then `hybrid` for 240 s, a shot every 30 s, all
  clean. DWM's private memory went 85 → 89 MiB over the first run and
  89 → 89 MiB over the second; handles went 1 203 → 1 227 → 1 223. A
  first-run settle, no growth.
- UAC elevates.
- CPU readbacks: 32–35 per 30 s, as before (the lease refresh).
- WARP is unchanged: the framebuffer profile loads neither driver, and the
  run on this same host engine gave 52.7 / 54.4 / 7.8 tick/s.

Final build, all switches at their defaults (the effects row comes from the
earlier synchronous build):

| | sync (`F10`) | unsync + fix (`X12`, `D13`) |
|---|---|---|
| fenced Venus submits per DWM present, `show` | 36.3 | 2.0 |
| … `hybrid` | 29.6 | 2.0 |
| … `surface` | 131 | 9.1–9.6 |
| … effects | 24.6 | 2.3 |
| waits with a timeout per present, `show` / `hybrid` / `surface` | 35 / 27 / 130 | 1.0 / 1.0 / 1.3–1.7 |
| DWM's time in its flushes, `show` / `hybrid` / `surface` | 55 / 60 / 66% | 31–34 / 33–36 / 42–48% |
| program tick/s, `show` / `hybrid` / `surface` | 53.8 / 50.6 / 49.7 | 61.9–62.0 / 61.6–61.7 / 59.5–61.1 |

Two caveats on these numbers:

- **Host load.** Presents a second are not compared here. `F10` ran with OBS
  and NVIDIA Broadcast on the host; `X12` and `D13` also ran with another
  agent's VM on it.
- **The remaining fenced submits are larger.** 80–95% of them now take over
  2 ms. Each DWM frame pays the kernel driver's 15.6 ms tail about once,
  instead of round-tripping fifteen times. The total time in fenced submits
  did not fall (`show`: 13.9 s before, 17.3 s after, per ~32 s run). Removing
  that tail remains the kernel-driver change described above.

**Portal 2 and `copilotapphost.exe`.** Portal 2 had died in `ntdll` once on
the new Venus driver, after stray keystrokes had started it. A controlled run
on the same driver started it, showed the menu full screen for 2.5 minutes,
and quit cleanly, logging no failure. The guest's log shows the same `ntdll`
failure of `portal2.exe` on 2026-11-13, long before the timer change, so it
predates this work. `copilotapphost.exe` fails in `ntdll` at most boots since
2026-11-20 16:06, before the timer driver was installed that evening. Neither
is attributed to these changes, and neither was looked into further.

Evidence: `F:\entangled-targets\dwm3d\evidence-lazy\` (`p1-fixed-with-mapcheck.patch`,
the oracle's run `M11-*`, shots `X12-*`, `D13*`, `L12-*-long-*`, effects
`fx-fix12-*`, `fx-def13-*`).

### Amendment, 2026-11-24: the render driver's control-queue wait

In `entangled-gpu.sys`, a command which survived the initial spin would
check for completion again after `KeDelayExecutionThread(1 ms)`. Windows
could wake the waiter on the next ordinary system tick. The waiter holds
the control queue's single command slot, so this wakeup delay can block
subsequent GPU work from both applications and the desktop compositor.

The slow path now waits on a high-resolution one-shot timer before
checking the used ring again. The timer belongs to the outstanding
command and is deleted before returning. The immediate poll path, the
fence retirement condition and the queue serialization are unchanged.
If timer allocation fails, the driver uses its ordinary delay.

In the separate Windows 10 lab, build `22.23.12.781` replaced
`13.32.52.201` on the render-only function. WDDM stayed at
`21.22.46.312`. The 1920×1080 guest ran at 60 Hz on the RTX 2070.
Each row has 16 valid guest QPC-to-GDI samples per action. The desktop
test opened the stock Windows Explorer context menu. The scroll test
used the unchanged Wintanyl F239 and the 80-file directory.

| Render driver | Desktop menu median / max | F239 scroll median / max |
| --- | ---: | ---: |
| Original driver | 131.83 / 764.26 ms | 80.16 / 135.18 ms |
| High-resolution wait, first run | 112.34 / 310.07 ms | 67.24 / 415.21 ms |
| High-resolution wait, repeat | 99.79 / 181.39 ms | 82.85 / 110.13 ms |
| Original driver, restored | 149.41 / 934.71 ms | 106.71 / 418.06 ms |

The menu median improved in both candidate runs and rose after restoring
the original binary. Scroll medians and tails varied. An additional
16-sample menu trace per driver separated the popup's Windows show event
from the menu pixels. Right-click to event changed from 38.85 to 36.73 ms
at the median. Event to pixels changed from 78.89 to 57.52 ms. The latter
interval includes composition, Windows event delivery and GDI sampling
jitter.

The candidate passed the Venus version round trip and a public D3D12
draw on the Entangled WDDM adapter. The draw verified 2,993 pixels after
the fence. A second invocation of the Venus probe in the same boot
received `INVALID_CONTEXT_ID` for its hard-coded context. The successful
first probe leaves that context allocated. The restored original driver
reproduced the same first-pass, second-failure sequence.

The lab package active on the render node was checked against the signed
binary's SHA-256. Probes, installer logs, images and CSVs are under
`F:/VMs/Entangled/d3d12-lab/present-latency-fix/` and
`F:/VMs/Entangled/d3d12-lab/share/present-latency-fix/`.

### Amendment, 2026-11-24: animation cadence and the presentation path

In the 1920×1080, 60 Hz Windows 10 lab with the RTX 2070, Wintanyl's
desktop cube had a 374.9 ms interval without an overlay scanout flush.
Three-dimensional work still reached the host during that interval. A repeat
had a 206.1 ms flush gap and a matching 204.4 ms gap in changed pixels in
the host window. With two windows in the same viewport, Wintanyl's 24-strip
minimize and restore effect logged 2–3 animation ticks and one return from
`DwmFlush` across each of eight periods of 281–307 ms. After receiving a
scanout flush, the host window usually showed changed pixels within 7–10 ms
(the nearest preceding flush, without an end-to-end frame ID).

There are two distinct handoffs in the GPU presentation path. The D3D12 UMD
copies a window's back buffer to mapped memory and uses WDDM allocation escapes so
the guest DWM can import its pixels. Even a scene with no new app pixels
still requires DWM to compose and copy the desktop into a Venus overlay
buffer, wait for that copy, then send the scanout through the render-only
device's serialized control queue. The legacy WDDM flip still runs and
advances Windows' presentation state. This gives us three different things
to measure: publication of app surfaces, composition of the desktop, and
retirement/publication of the resulting overlay. The host window's small
share of the measured stalls makes work on its present loop a lower
priority for this workload.

The [DirectComposition cadence probe](../../tests/windows/dcomp-cadence.cpp)
uploads one 400×384 surface once and varies only how it moves. It runs a
single visual, 24 independently moving clipped strips of that same surface,
and a repeat of the single visual. The default mode performs `Commit` and
`DwmFlush` for each update. `--queued` issues commits on a 60 Hz clock with
no per-update `DwmFlush`. In both modes the CSV reports guest QPC for the
start, commit and completion stages, and stderr reports the adapter and
QPC frequency. Build from an x64 MSVC prompt with the object and executable
outputs on the scratch drive:

```cmd
cl /nologo /std:c++17 /O2 /EHsc /MT /W4 /WX /external:I "%WindowsSdkDir%Include\%WindowsSDKVersion%cppwinrt" /external:W0 tests\windows\dcomp-cadence.cpp /FoF:\scratch\dcomp-cadence.obj /FeF:\scratch\dcomp-cadence.exe d3d11.lib dcomp.lib dwmapi.lib user32.lib gdi32.lib windowsapp.lib
```

Verify changing pixels in the benchmark window as well as its timings. A
hidden run returned from `DwmFlush` about 60 times a second while producing
zero desktop scanouts. A visible probe and a pixel capture of the Entangled
window are required for the cadence numbers below. [DwmFlush timing semantics](https://learn.microsoft.com/en-us/windows/win32/api/dwmapi/nf-dwmapi-dwmflush)

With Wintanyl idle in the same guest, the DXGI adapter reported
`Entangled GPU (virtio-gpu, WDDM)`. Each phase below includes about 4.4 s
after discarding setup. The pixel column counts changes within a fixed
region in the captured Entangled window.

| Mode | Visuals | Updates/s | Scanouts/s | Visible changes/s | Median visible gap |
| --- | ---: | ---: | ---: | ---: | ---: |
| Commit and wait | 1 | 32.6 | 32.6 | 32.1 | 33.34 ms |
| Commit and wait | 24 | 33.3 | 33.3 | 33.1 | 33.34 ms |
| Commit and wait | 1, repeat | 31.4 | 31.4 | 30.7 | 33.37 ms |
| Commit on a 60 Hz clock | 1 | 60.0 | 57.7 | 56.4 | 16.68 ms |
| Commit on a 60 Hz clock | 24 | 60.2 | 48.9 | 48.9 | 20.82 ms |
| Commit on a 60 Hz clock | 1, repeat | 60.2 | 50.6 | 49.5 | 16.70 ms |

`Commit` took 0.28–0.36 ms at the median across the visible phases.
The median `DwmFlush` wait was 30–31 ms in the table. With the final binary,
a second blocking run had 19 visible changes/s and a 50 ms median visible
gap in all three phases. The following queued run reached 46–49 visible
changes/s and a 21 ms median gap. These short runs show variability and a
consistent advantage for decoupling commits from the synchronous wait.

Wintanyl's scene waits for DWM to wake
its animation ticks and can hold a pending tick up to 500 ms while waiting
for an outstanding commit. A controlled pacing comparison in the real
effects is needed to measure its contribution. The probe uses one static
surface and does not exercise live thumbnail acquisition or app-to-DWM
redirection. The 24-strip queued phase falling below 60 also leaves
guest composition and scanout publication as separate work to measure.

The raw guest timings and clock calibration are under
`F:/VMs/Entangled/d3d12-lab/share/animation-etw/` (`dcomp4` and `dcompq`,
then `dcompv` and `dcompqr` on the same binary).
The correlated results, host trace and window capture are under
`F:/VMs/Entangled/d3d12-lab/physical-wheel/`.

#### Comparing Wintanyl's existing effect configurations

On the same lab, the shipping F239 shell alternated default effects with
`[cube] flatten = true` and `[lamp] driver = "expression"`. There were four
samples per action and variant in an ABBA order. The cube rotated between two
viewports with one open window in each. Both windows were on the same viewport
for the minimize and restore tests. The shell log confirmed that two window
visuals were flattened and that the lamp used expressions with all 24 strips.

| Action and variant | Median command-to-first-change | Detected changes per action | Longest internal pixel gap |
| --- | ---: | ---: | ---: |
| Cube, default | 386 ms | 13–16 | 242 ms |
| Cube, flattened | 468 ms | 5–12 | 300 ms |
| Minimize, default lamp | 623 ms | 1–2 | 109 ms |
| Minimize, expression lamp | 368 ms | 1–2 | 192 ms |
| Restore, default lamp | 616 ms | 2–3 | 196 ms |
| Restore, expression lamp | 728 ms | 3–5 | 234 ms |

The onset includes polling the shell's command file. We counted pixel
changes in the same region of the Entangled window over a fixed observation
period (1.39 s for cube, 1.15 s for lamp), including the final transition.
Gaps between changes exclude the wait for the first change. Where only one
change was visible, that sample provides no internal gap. Thresholds of 50,
150 and 500 changed pixels led to the same comparison. The expression lamp
started minimizing sooner, with the same 1–2 detectable changes. The lab
profile was restored. The raw capture, commands and stage comparison share the prefix `fx-arch2` in
`F:/VMs/Entangled/d3d12-lab/physical-wheel/`.

#### Testing independent animation ticks in the same shell

A diagnostic build from Wintanyl `4828531` used the stock effects in both
arms. One arm used the existing `DwmFlush` clock. The other scheduled ticks
every 16.667 ms and left `DwmFlush` running as a background observer. The
diagnostic clock stopped adding work at three outstanding commits. The
same executable ran on the same WDDM/D3D12 adapter with the same Explorer
and Notepad window handles in both arms. Each arm ran four samples per
action. Counts use the same crop and observation windows as the preceding test.

| Action | Median command-to-first-change, stock → clock | Detected changes per action, stock → clock | Longest internal gap, stock → clock |
| --- | ---: | ---: | ---: |
| Cube | 407 → 491 ms | 12–17 → 13–21 | 234 → 221 ms |
| Minimize | 554 → 448 ms | 2 → 1–2 | 142 → 63 ms (one clock sample with two changes) |
| Restore | 673 → 330 ms | 1–3 → 3–4 | 71 → 488 ms |

The median cube gap was 33.4 ms for both arms at the 150-pixel threshold.
The independent clock issued 15–24 commits per cube animation period
versus 15–18 in the stock arm. During the lamp, the clock hit its
outstanding-commit limit on 13–15 ticks per run, and the background
observer recorded zero or one return from `DwmFlush` per period of about
300 ms. An earlier pilot with an extra dialog showed the same low rate of
returns during the lamp. The timing and visual cadence results support
retaining the stock clock while tracing the DWM render and overlay handoff.

Normal shell exit hung in the first diagnostic clock run and later with the
shipping F239 binary on this guest. Both recovered through the guard. The
clean clock run exited normally, and F239 was restored and verified on the
hardware adapter. The diagnostic clock remains a lab experiment.

The benchmark patch, host capture, commands and analysis are under
`F:/VMs/Entangled/d3d12-lab/physical-wheel/` (`wt-pacer-test.patch`,
`fx-pacer`, `fx-pacer3`). Fixture states are under
`F:/VMs/Entangled/d3d12-lab/share/animation-architecture/`.

#### DWM stages during visible animation stalls

The stock F239 shell ran another four cube rotations, three minimizations
and three restores. The guest trace used `Microsoft-Windows-Dwm-Core` and
`Microsoft-Windows-DxgKrnl` (base keywords, verbose level) with a QPC clock
and zero reported lost events or buffers. The preceding detailed trace
also showed one DWM draw lasting 1.32 s. The base-keyword repeat excluded
per-visual tracing and aligned QPC to the host through the lab's UDP
clock bridge on both ends of the run (minimum round trips of 0.586 and
0.588 ms). The [ETW header reader](../../tests/windows/etw-display-qpc.cpp)
uses raw timestamps to avoid the local-time conversion in the XML export.
Build it with the Windows SDK and link `advapi32.lib`.

All 143 observed full-screen virtio-gpu flushes fell within a DWM `Present`
interval or within 4 ms of its boundaries. The longest captured restore
split across stages as follows:

| Stage | Elapsed |
| --- | ---: |
| DWM draw (`MILEVENT_MEDIA_UCE_RENDEREVENT`) | 1,283 ms |
| DWM Present entry after draw exit | 50 ms |
| DWM Present entry to virtio-gpu flush | 65 ms |
| virtio-gpu flush to changed pixels in the host window | 8 ms |

There were 77 guest vertical-blank interrupts and 78 Venus ring notifications
from DWM during that draw, with zero scanout flushes. The longest recorded
deferred fence response in the 1,398 ms scanout gap was 1.16 ms. Separate
minimize samples had 1,072 ms and 1,104 ms inside the same draw stage. The
four cube samples had longest draws of 68–141 ms. Across the 143 DWM
Present calls, median elapsed time was 17.6 ms, with p95 of 58.7 ms and a
maximum of 122.4 ms. Present also contributes to the budget.

The draw scope includes UMD work and waits. Ring notifications can carry
multiple Vulkan operations, and the deferred response timing covers only
the recorded fenced commands. The evidence puts the largest stall before
the overlay handoff, inside the guest DWM draw. A simultaneous UMD and
Venus ring trace can separate CPU-side draw processing, shared-surface
imports and synchronization during that draw.

Raw ETLs are in `F:/VMs/Entangled/d3d12-lab/share/animation-etw/`
(`effects-stages.etl`, `effects-lite.etl`). The joined trace,
`effects-lite-breakdown.txt`, and the matching `fx-lite` host actions and
pixel capture are in `F:/VMs/Entangled/d3d12-lab/physical-wheel/`.

#### Reusing one captured window surface

The detailed trace attributed 22 strip updates and 22 realizations of the
same 1280×820 window source to the 1.32 s draw. Wintanyl builds the lamp
from per-strip `CompositionVisualSurface` objects over a nested window
mirror. A separate 24→4 strip control roughly doubled the visible
minimize updates (1–2 to 4) while leaving first-pixel latency near 0.5 s.
That control changed the effect geometry and was reverted.

The cadence probe's `--snapshot HWND` mode captures one visible window
with Windows Graphics Capture. Pass a hexadecimal guest HWND. It copies
the frame with `CopySubresourceRegion` into a DirectComposition surface
on the same D3D11 device, without CPU pixel readback or an inter-API
shared handle. All visuals sample the resulting surface, and the mode
uses the same 60 Hz commit clock. The probe scales the full texture to
400×384 and moves clipped strips horizontally. It does not reproduce the
lamp's projective transforms or Wintanyl's decoration and reversal.

On the hardware Entangled WDDM adapter, two captures of the actual F239
Explorer measured 79–81 ms from `StartCapture` to the first frame and
another 69–71 ms through copy submission and `EndDraw`. In both 24-strip
phases, after skipping the first 600 ms, 264 commits produced 164 and 178
guest scanouts and the same numbers of changed host frames in 4.38 s.
Host pixel intervals had a 25 ms median, 29–33 ms p95 and 54–58 ms
maximum. The paired one-visual controls had 156–180 changed host frames.
A guest screenshot confirmed that the probe displayed the Explorer file
list.

Starting capture after the same window was already minimized timed out
after three seconds (`0x80070102`). For a real lamp implementation, the
source has to be ready before minimizing, remain available through
restore, and be invalidated when the window changes. An on-demand cold
capture alone would consume about 150 ms on this guest before the first
effect commit. The measured 37–41 visible updates/s also leaves room below
the 60 Hz guest target. The probe tests source reuse independently of
window lifecycle and animation geometry. [Capture a window by HWND](https://learn.microsoft.com/en-us/windows/win32/api/windows.graphics.capture.interop/nf-windows-graphics-capture-interop-igraphicscaptureiteminterop-createforwindow) · [Draw to a DirectComposition surface](https://learn.microsoft.com/en-us/windows/win32/api/dcomp/nf-dcomp-idcompositionsurface-begindraw)

The `dcomp-snap` and `dcomp-snap2` host captures and the guest's matching
results plus `dcomp-minimized` are under the same lab directories as above.

#### Wintanyl lamp trial and pipeline retirement crash

A local Wintanyl F239 variant started a D3D11 window capture off the
scene thread on focus. It kept the most recent frame in the capture pool,
copied it to a drawing surface once at minimize, and reused that surface
on restore. This first trial covered the Explorer's own chrome and kept
the lamp's 24 projective strips. Windows with separate W4 decorations
continued to use the stock lamp. The scene-thread copy, including
composition device binding, ranged from 13 to 129 ms in the lab.

The variant drew both directions with the shared raster, then Wintanyl
exited with `0xC0000005` on the third minimize in the pixel run and on the
fourth minimize under CDB. The latter stopped on the Zink submission
worker. The DLL identities matched the lab build by SHA-256. Its linker
map and the call site in the DLL place the exception under
`submit_queue` → `reset_batch_state_internal` →
`zink_destroy_gfx_program` → Vulkan `DestroyPipeline`. The guest
`vulkan_virtio.dll` faulted while decrementing a reference reached
through the pipeline handle. The available trace does not establish
where that reference became invalid. `ZINK_DEBUG=noopt` in the test
process, with the same candidate binary, still crashed on the fifth
minimize. This rules out that switch as a workaround.

The run under CDB also saw later shells, including the stock build, fail
after `vkAllocateDescriptorSets` referenced a missing descriptor pool
and the host marked their Venus rings fatal. Those host warnings started
after the captured pipeline fault. They are a separate recovery symptom
with unknown cause. A clean restart of the dedicated VM and its renderer
restored the stock F239 shell on Entangled WDDM/D3D12. Keep the raster
variant out of release builds until pipeline retirement is stable and
the result can be benchmarked through repeated full gestures.

The debugger output and minidump are in the lab's `share/animation-architecture`
directory. `fx-snapshot2` has the host pixel log. The post-crash samples
include a shell restart and are unsuitable for comparing animation
cadence. The archived host trace is `run-before-lamp-reset-20261125-004127.log`.

#### Keeping the composition device alive

The next Wintanyl build kept the same bound `CompositionGraphicsDevice`
for the lifetime of the effect. It still produced a separate drawing
surface for each gesture. It completed 16 consecutive minimize/restore
pairs plus eight more in a same-binary ABBA comparison without a shell
restart. This ordinary API lifetime change avoided the previously
reproducible failure in these trials. It does not identify the stale
reference inside the driver. After the first use, GPU copy submission
and `EndDraw` took 13–35 ms in the 16-pair run. The first device bind took
16 ms and the first copy took 80 ms.

The ABBA run used one release binary and the same guest scene and host
capture rectangle for both modes, with eight gestures per direction
and mode. The intervals below are between changes of at least 150 pixels
in the sampled window area. The rectangle did not include the dock, so
its first change is a region-of-interest onset rather than the first
visible pixel anywhere along the animation. Command-file polling is
included in the onset.

| Gesture | Source | First change in region p50 | Pixel gap p50 / max | Max guest scanout gap |
| --- | --- | ---: | ---: | ---: |
| Minimize | Live strip surfaces | 507 ms | 100 / 583 ms | 579 ms |
| Minimize | Shared GPU raster | 420 ms | 33 / 250 ms | 186 ms |
| Restore | Live strip surfaces | 427 ms | 83 / 450 ms | 448 ms |
| Restore | Shared GPU raster | 473 ms | 33 / 50 ms | 383 ms |

The raster improved cadence in the sampled area in both directions. The
restore onset and the remaining scanout gap are still large. The
Wintanyl implementation is opt-in via `WT_BENCH_LAMP_SNAPSHOT=1` in
commit `0b6142d`; separate W4 decorations still use the live source.
The dedicated VM returned to the stock F239 shell after both runs.
Raw results are `fx-snapshot-reused-device` and `fx-snapshot-abba` in
the lab's `physical-wheel` directory.

#### Driving the lamp and cube with the guest keyboard

The dedicated lab accepted `chord win m`, `chord win shift m`, and
`chord ctrl alt left/right` through its control pipe and the virtio
keyboard. The launcher's host timestamp includes its command-file
handoff. The first action used just the Explorer fixture so `Win+M`
minimized one window. The cube used Explorer and Notepad on separate
viewports. The [guest helper](../../tests/windows/animation-input-qpc.cpp)
records only these shortcuts and the specified Explorer HWND's minimize
and restore WinEvents. Build with x64 MSVC and link `user32.lib`. Its CLI
takes the hex HWND and the observation length in seconds.

A QPC clock bridge before and after each run had minimum round trips
of 0.62–0.75 ms. Host Windows Graphics Capture sampled the VM viewport
through the dock. A change means at least 150 pixels crossing the
brightness threshold. The [joiner](../../tests/windows/analyze-animation-input.py)
pairs the first change with the last preceding full-screen flush as a
timing proxy. There is no shared frame ID at those boundaries, and
first visible change can include immediate shell or dock feedback.

| Effect and source | Host command to first visible change p50 / max | Changed frames p50 in 1.2 s |
| --- | ---: | ---: |
| Minimize, live surfaces | 143 / 253 ms | 3 |
| Minimize, shared GPU raster | 118 / 140 ms | 12 |
| Restore, live surfaces | 141 / 270 ms | 2 |
| Restore, shared GPU raster | 193 / 216 ms | 15 |
| Cube, stock build mode | 70 / 226 ms | 19.5 |
| Cube, raster lamp enabled | 85 / 232 ms | 18 |

Each lamp result has five gestures and each cube result has six turns.
In the raster arm, the median time from the guest keyboard marker to
WinEvent delivery was 13 ms for minimize and 27 ms for restore. From the
WinEvent to the preceding flush it was 89 and 144 ms respectively. From
flush to the sampled pixels it was 9 and 10 ms. This locates the major
remaining interval in the guest before the scanout, but does not split
it between Wintanyl's scene work, DWM draw and guest Present. The
elevated ETW launcher failed with `0xC0000005` for this run. The earlier
ETW trace in this ADR is a separate observation of the stock lamp.

The first cube turn took 226–232 ms to the first change. The ten
subsequent turns took 58–92 ms. In the raster-enabled run the first cube
turn spent 35 ms setting up the skybox and 14 ms creating the faces.
Later turns reported 0 ms for both counters. The cold path also took
199 ms from the guest `WM_HOTKEY` log to the first rotation log, which
includes the cube's initial pull phase.

One GPU-raster variant started the restore at the end of the fully
collapsed interval. The geometry from that point onward and its speed
were identical. At the default 320 ms duration it removed 32 ms of
invisible timeline. The five restored windows reached the first sampled
pixel in 193 ms at the median, matching the run with the interval intact.
The candidate was removed. The result points to work on the initial
composition and handoff, rather than a delay in the progress track.

The host records use `input-legacy-direct` and `input-raster-direct` in
the lab's `physical-wheel` directory. The guest QPC markers are in
`share/animation-etw` under the matching prefixes. The collapsed-interval
trial uses `input-raster-no-hold`.

#### Preparing the cube backdrop after startup

Wintanyl `e3ebdab` moves the static skybox upload to a delayed task after
scene setup. It prepares the same cached surface brush used by the cube,
without adding a visual to the scene or requesting animation frames.
The first shortcut still has a lazy fallback if the scheduled task did
not populate the cache.

In one paired fresh-shell run, the control took 146 ms to prepare the
backdrop while handling the first shortcut and another 73 ms to prepare
faces. The delayed task took 48 ms, after `SHELL GOTOWY`. In that shell the
first shortcut spent 0 ms in the backdrop step and 22 ms on faces. Host
command to first visible change was 720 ms in the control and 144 ms
with the cached backdrop. The earlier controls reached their first
changed pixels in 226–232 ms, so the single paired result does not
isolate a 576 ms saving from the backdrop change. The next five turns
on the new build were 67–79 ms. The first-turn result was unchanged
when the pixel threshold was raised from 150 to 500.

The timing artifacts use `input-cube-cold-control` and
`input-cube-prewarmed`. After the check, the lab returned to stock F239
on the same hardware Entangled WDDM adapter.

#### Timing the first restore commit and native Z placement

Wintanyl's `WT_BENCH_SCENE_QPC=1` (commit `04fe598`) writes guest QPC spans around window
model processing, effect entry and frames, and dirty DirectComposition
commits. The input joiner reads these spans with `--scene-log` and
`--scene-pid`. The timestamps use the same guest clock as the existing
input probe. The first pixel remains a thresholded change in the host
window, and the first scene commit can precede the first noncollapsed
lamp geometry.

With the raster lamp, eight restores had a median 53.3 ms from
`EVENT_SYSTEM_MINIMIZEEND` delivery to lamp entry. The entry and first
`Commit` each took less than 1 ms. The first follow-up lamp frame was
39.6 ms after commit return, and the first changed host pixels were
104.5 ms after it. The source of the first delay was the synchronous
native `SetWindowPos` used to raise the restored top-level window above
the viewport floor. In a separate six-restore control, registering the
thumbnail took 0.8 ms and window placement took 53.6 ms at the median.
Reusing the old thumbnail reduced its own step to 0.1 ms but left
window placement at 57.0 ms, so the reuse variant was removed.

The opt-in `WT_BENCH_RESTORE_Z=async` path queues that one raise with
`SWP_ASYNCWINDOWPOS` for an ownerless window restored to the active
viewport. After 200 ms, it checks the window, current viewport and
actual Z order before a synchronous retry if necessary. Group and
viewport reordering continue to use their existing synchronous paths.
With `SWP_NOSENDCHANGING` alone the placement step still took 55.4 ms,
so that variant was removed. [SetWindowPos flags](https://learn.microsoft.com/en-us/windows/win32/api/winuser/nf-winuser-setwindowpos)

| Raster restore test | Samples | Native placement p50 | Event to lamp entry p50 | Host command to first pixel p50 / max |
| --- | ---: | ---: | ---: | ---: |
| Synchronous control | 6 | 53.6 ms | 56.2 ms | 204 / 219 ms |
| Asynchronous trial | 5 | <1 ms | 2.8 ms | 171 / 185 ms |
| Asynchronous with verification | 5 | 0.1 ms | 2.9 ms | 175 / 209 ms |

The native Z-order probe confirmed that Explorer was visible, restored
and above the floor after the final run. The asynchronous path moves
some work after the first commit. In the final run it still took a
median 132 ms from commit return to first changed host pixels, compared
with 106 ms in the synchronous control. The final run also had fewer
thresholded pixel updates in the 1.2 s observation window (median 11
versus 15), so the earlier onset does not establish a cadence gain.
This breakdown does not attribute the time after commit to individual
DWM operations. The one-window tested path remains opt-in.

Raw captures use `input-node-control`, `input-node-reused`,
`input-zorder-no-changing`, `input-zorder-async2` and `input-zorder-final`
in the same lab directories. The additional unprivileged DWM timing
trial, `input-raster-dwmapi`, returned success in 4,739 samples with
zero `qpcFrameDisplayed` and `qpcFrameComplete` values, so it could not
split composition and presentation.

The second dirty scene commit came about 41 ms after the first in the
verified asynchronous run. From the second commit return to the last
full-screen flush before the first visible change, the median was 73 ms
versus 52 ms with synchronous placement. The five asynchronous intervals
contained 7–13 deferred fence responses each, with a maximum recorded
host-side response of 2.30 ms. The matching flush is a timing proxy and
shares no frame identifier with the window capture. These fence timings
leave repeated guest waits and time before submission unmeasured.

The diagnostic shell exited with `0xC0000005` in `ntdll.dll` while
processing `quit` after all five measured restores and the native order
check. The original F239 shell started afterwards on the hardware adapter.
During a later cold jelly drag it logged `DXGI_ERROR_DEVICE_REMOVED`, then
the guard returned to Explorer after three shell failures. In two restart
attempts the host reported `vkAllocateDescriptorSets` on missing pools
and marked the two Venus contexts fatal, as in the earlier recovery
failure above. Those warnings followed the first jelly failure. The
renderer process exited with `0xC0000005` during the subsequent guest
shutdown. A fresh engine and guest boot brought the stock F239 shell back
on Entangled WDDM/D3D12. The first jelly failure and the later missing
descriptor pools still lack a common causal trace.

The archived host log is `run-before-jelly-recovery-20261125-1217.log`.
The adjacent `jelly-device-removed-*.log` extracts and
`input-zorder-final-follow-up.txt` preserve the relevant observations.

#### Render-only context ownership on Windows

The archived host log gives the reason for the two missing descriptor
pools. In both new contexts, the immediately preceding
`vkCreateDescriptorPool` needed 266,816 units with 33,427,552 already held
of a renderer-wide 33,554,432. The following allocation used the pool
that the host had just refused. The same engine recorded 70 context
creates and nine destroys in the full run. In particular, neither of
the two contexts belonging to the diagnostic shell or the two belonging
to stock F239 had a destroy record after their processes exited.

The render-only miniport uses `METHOD_BUFFERED`, so its IOCTL input and
output share storage. It recorded a successful create's context ID from
the input after replacing the first word with the response (`0x1100`).
The cleanup callback then tried to destroy that value. Save the request
ID before writing the reply, and on explicit destroy clear ownership
only after the host acknowledges that exact context. `gpu-test
context-cleanup` opens the render-only device, creates a context, closes
the handle, and verifies that it can create the same context again.
The resource caps are unchanged.

The published `gpu3d-14` render-only miniport (`11.38.1.423`) passed the
same cleanup check on the dedicated guest. The host recorded two creates
and two destroys for the test context. All 31 package assets matched the
published sizes and SHA-256 digests, and `enable-3d --package` accepted
the full directory and built its installer disc. This guest test
installed the render-only miniport from the package.
[Windows GPU package](https://github.com/doctorspider42/entangled-destop/releases/tag/gpu3d-14)

#### Stock Windows animation after the render-only driver fix

The guest showed one Windows 10 Notepad window in the capture region. Six
`Win+M` and `Win+Shift+M` pairs ran through virtio input at 1920 × 1080.
The hardware WDDM adapter and host engine were the same as in the lamp
tests, with the context cleanup fix installed in the render-only miniport.
On the same guest, six more pairs posted `ShowWindowAsync` with
`SW_MINIMIZE` and `SW_RESTORE` directly to the Notepad window and checked
its final iconic state. The API runs start at a guest QPC timestamp.

The [animation analyzer](../../tests/windows/analyze-animation-input.py)
accepts `--dwm-log` and `--dwm-pid` to join Dwm-Core ETW draw and Present
scopes from the same thread with the existing input and pixel data. It
looks for the Present bracketing the last full-screen flush before the
observed change. The flush and host capture have no common frame ID, so
this is a timing attribution. The guest clock bridge round trips were
under 0.7 ms, and the ETW headers reported zero lost events and buffers.
Raising the pixel threshold from 150 to 500 did not materially change the
onset. The region included the window and excluded the taskbar.

| Restore trigger | Samples | Trigger to changed pixels p50 / max | WinEvent to matched draw p50 | Matched draw p50 | Present entry to flush p50 | Flush to pixels p50 |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| Host shortcut through virtio | 6 | 137 / 184 ms | 82 ms | 10 ms | 13 ms | 10 ms |
| Guest `ShowWindowAsync` | 6 | 133 / 145 ms | 80 ms | 0.5 ms | 7 ms | 4 ms |

A separate six-pair keyboard run with no guest key probe gave a 134 ms
median restore onset. In the run with WinEvents, one or two earlier
full-screen flushes commonly occurred after restore and before the first
captured content change. The 80–82 ms is time to the matched draw, and
can include these earlier composition cycles. Posting the operation
directly preserved the wait after the WinEvent. The stock window and
gesture use different visuals from Wintanyl's lamp.

One keyboard minimize took 157 ms from host command to changed pixels.
After the WinEvent, DWM spent about 97 ms inside its DirectComposition
queue scope (`MILEVENT_MEDIA_UCE_PROCESSQUEUEEVENT`), drew for 1 ms,
and handed off the first post-event scanout about 124 ms after the event.
Guest vblank interrupts continued at roughly 16.7 ms throughout the
queue stall. The DWM render thread had a 96 ms gap between its ETW
events within the queue scope. The host recorded two 24-byte submits
from DWM, zero deferred fence responses and zero scanout flushes in
that scope. The other five minimize onsets were 45–62 ms in this run.
The next stage to trace is what the DWM thread waits on inside queue
processing. The older legacy-lamp trace also has a separate long draw
after that stage.

Raw measurements use `stock-dwm-fixed`, `stock-dwm-input` and
`stock-dwm-api` in the lab's `physical-wheel` and `share/animation-etw`
directories.

#### Scheduler trace of a 378 ms stock restore

A 12-pair keyboard repeat with the published render-only miniport and
WPR's CPU profile kept the stock restore median at 137 ms and captured
one 378 ms restore. The QPC clock bridge round trips were below 1 ms, and
both ETL headers reported zero lost events and buffers. The first
84.8 ms elapsed before `MINIMIZEEND`, including 26.9 ms from host send
to the guest key and 57.9 ms from the key to the event. After that event,
150.5 ms elapsed to the matched draw. The draw took 73.2 ms, Present
reached the scanout about 51.9 ms after entry, and the host window
showed changed pixels 17.4 ms after the flush.

The render thread was scheduled to run in the guest for 60.5 ms during
the draw. It was waiting for 46.8 ms of the following 53.6 ms Present
scope. The guest scheduler shows thread state on virtual CPUs. In the
draw, 25 of the resolved samples outside switch and ready timestamps
contained the D3D10 UMD, including 15 in the vectorized memory-copy loop
called by `ResourceUpdateSubResourceUP`. The symbol map was matched to
the installed UMD by SHA-256. This was a CPU texture update on a stock
window restore, separate from the per-strip cost in the legacy lamp.
[WPR wait analysis](https://learn.microsoft.com/en-us/troubleshoot/windows-server/support-tools/support-tools-xperf-wpa-wpr)

The closest resolved user stacks before two Present waits of 26.5 and
20.3 ms were sampled 0.20 and 0.44 ms before the respective switches.
Both passed through `zink_end_batch` and `util_queue_add_job_locked`.
The first was under `d3d10_wddm_flush`, the second under
`d3d10_wddm_flush_frontbuffer`. The exact blocking call was outside
those samples.

The `ReadyThread` stack at each wakeup identifies the same Zink worker
within DWM, calling `futex_wake` from `util_queue_thread_func`. During
the first wait the worker was itself waiting for 17.3 ms. During the
second it was scheduled to run for 19.2 ms, with sampled stacks in the
render-only miniport and the Venus submission code. The host recorded
one deferred fence completion inside each wait. Those completions took
0.32 and 0.97 ms from their recorded deferral. The larger wait spans
therefore include guest submission and worker time outside those
measured host fence intervals.

The first flush also publishes dirty shared surfaces. A trial of
combining the render and overlay submissions needs to preserve that
publication and the fallback present path.

The capture prefix is `stock-dwm-sched`. The matching timestamps, module
identity and scheduler breakdown are in `stock-dwm-sched-breakdown.txt`
in the lab's `physical-wheel` directory.

#### MSI-X completion trials and an earlier Present in a 345 ms restore

Two lab miniports bound the render-only virtio-gpu control queue to one
MSI-X message. The ISR queued a DPC to signal the command's event. The
command verified the used index on every wake and also checked it after a
10 ms relative wait expired. One variant used the event for every control
command. The other reserved it for fenced commands, keeping the short
poll and suppressing the interrupt for ordinary control commands. Both
kept the single shared command slot and passed `gpu-test context-cleanup`
after a reboot with the exact test package installed. The event counter
increased by 6,618 in the first run and by 1,377 in the second between
the readings around each test. These totals include guest background work.

Each run repeated 12 `Win+M` and `Win+Shift+M` pairs in the same stock
Notepad workload at 1920 × 1080, with WPR and the 500-pixel WGC threshold.
Both traces had sub-millisecond clock bridge round trips and zero ETW
events or buffers lost. Times are p50 / maximum in milliseconds.

| Render-only completion | Minimize to pixels | Restore to pixels | Wait in the restore Present associated with the first pixel change |
| --- | ---: | ---: | ---: |
| Published `gpu3d-14`, polling | 63.7 / 81.6 | 137.3 / 378.0 | 13.35 / 46.81 |
| MSI-X on all control commands | 62.8 / 75.8 | 146.9 / 350.5 | 12.87 / 18.70 |
| MSI-X on fenced commands | 62.5 / 97.4 | 145.2 / 345.4 | 11.64 / 17.58 |

Each mode had one 12-pair run. The two event variants shortened waits
inside Presents matched to the first changed pixels, and neither improved
the restore median. The lab was restored to the published miniport. The
MSI-X driver patches were left in the lab for further experiments.

In the 350.5 ms restore, 214.0 ms elapsed from the guest key hook to the
guest `MINIMIZEEND` hook. The Windows message timestamps for the same
events differed by 218 ms. In the 345.4 ms restore the guest received the
key at 21.6 ms and `MINIMIZEEND` at 48.0 ms, relative to host send. DWM
entered Present at 51.9 ms and returned at 273.5 ms. A second Present ran
at 274.3 ms. WGC measured zero changed pixels after those two frames. The
third Present reached the associated scanout at 333.8 ms and the first
changed capture at 345.4 ms. A split based only on that third Present
puts the earlier 221.6 ms Present inside the interval labelled
`WinEvent-draw`. The analyzer also reports the longest complete Present
between the input and the first changed pixels for each gesture.

Inside the 221.6 ms scope, the DWM render thread waited for 194.5 ms,
including one uninterrupted 187.8 ms wait. Its `ReadyThread` stack
identifies the Zink worker at the same `futex_wake` and queue worker RVAs
as in the polling run. The worker was waiting for 186.5 ms during that
interval. Ten stack samples match the return address of the miniport's
`KeWaitForSingleObject` on the control completion event. The only host
fence retirement recorded during the wait came 187.5 ms into it and took
101 us from deferral. About 1.4 ms before the DWM wake the host logged the
completion of physical-device discovery for Venus context `2082817`.
Mesa's context ID encodes guest PID `8136`, identified as `CalculatorApp.exe`
by the WPR image events. A later `ReadyThread` stack ran on a Calculator
thread inside the same miniport. The timestamps alone cannot assign the
preceding gap to discovery. The shared command slot permits one client's
initialization to delay another client's render submission. Separate
enumeration, description and throwaway Vulkan device timings make that
hypothesis measurable.

The capture prefixes are `stock-dwm-msix` and `stock-dwm-fenced-msix` in
`physical-wheel` and `share/animation-etw`. The `-worst.txt`,
`-longest-present.txt` and `-waker.txt` extracts record the 345 ms case.

#### Reuse the startup host-memory capability probe

The timed cold boots used the released `gpu3d-14` render miniport and the
same 4-vCPU Windows lab on the RTX 2070. Among the first 14 guest contexts,
the physical-device description took 124.37 ms at the median and 655.90 ms
at the maximum. Enumerating devices was about 1.6 ms at the median. Reading
the memory types of a sample host pointer took less than 0.13 ms in the
first 14 probes. The cost was in creating and destroying the temporary
logical device that performs the query.

The host now retains a successful `RingPages` capability probe from the
startup adapter check for the lifetime of the Vulkan loader. The cache key
includes device and driver UUIDs, alignment, queue family, and ordered
memory types. A failed probe can be retried. The cache lock is released
before opening a device, and actual memory imports still query each
allocation's pointer. The device and driver identifiers come from Vulkan's
[physical device ID properties](https://docs.vulkan.org/spec/latest/chapters/devsandqueues.html#VkPhysicalDeviceIDProperties).

With this change, the first 14 guest descriptions took 3.42 ms at the
median and 4.62 ms at the maximum. The existing startup check created and
destroyed one probe device for the RTX 2070. The guest contexts reused
that result. These timings measure adapter discovery during a cold boot.
The host logs are `run-init-trace.log` and `run-init-cache.log` in
`physical-wheel`.

#### Trial of waiting on the Windows client's ring before taking the control slot

The host dispatches `vkWaitRingSeqnoMESA` inline on the shared control
queue. While it waits for one context's ring worker, the Windows render
miniport's single command slot remains occupied. Every other Windows
Venus client that needs the control queue waits behind it. This includes
fence submissions and other clients' notifications for idle rings.

The experimental Windows Mesa client checked its own shared ring head
before sending a fenced submission or a blob operation that depended on
an earlier ring allocation. It used Mesa's existing polling and relax
logic before entering the renderer. After roughly 500 ms it fell back to
the original wait command, preserving the host wakeup and timeout path.
A queue submission still used its normal renderer fence after the ring
issued the Vulkan work.

The first 38 seconds after a guest reset were measured on the same 4-vCPU
Windows guest and RTX 2070. Both builds came from the same prepared Mesa
26.2.6 tree and used the same Windows display miniport and Entangled host.
The local test replaced the x64 Venus ICD and verified its SHA-256 after
reboot. Profile windows that began before the reset were excluded.

| Windows x64 ICD | Control submits | Ring waits in the host | Total ring wait | Longest ring wait | GPU ring fences |
| --- | ---: | ---: | ---: | ---: | ---: |
| Rebuilt control | 3,730 | 1,320 | 3,294.97 ms | 157.39 ms | 1,268 |
| Client-side ring wait | 4,226 | 0 | 0 ms | 0 ms | 1,566 |

In a warm 12-pair Notepad run, the DWM context went from 487 waits with
1,721.50 ms on the shared queue to zero. DWM still registered 491 GPU ring
fences in the same profiled windows with the trial, compared with 475
for the control. Input-to-pixels for a restore was 157.8 ms median and
175.8 ms maximum in the control, then 156.0 ms median and 199.5 ms maximum
in the trial. Repeating the control after another boot gave 155.4 ms
median and 184.7 ms maximum.

Six runs of the 300-frame D3D11 smoke on each ICD used the hardware
Entangled adapter and the same copy swap chain. Each verified the final
RGB value `(252, 127, 2)`. The rebuilt control reached a median of
418.4 frames/s (321.5–426.1). The client wait reached 336.8 frames/s
(294.2–345.9). Moving the wait released the shared queue in these samples
at the cost of roughly 20% of this workload's throughput. The trial was
kept as a lab patch. The production Windows ICD keeps renderer-side waits.

An asynchronous transport needs both halves of the dependency. The
renderer must defer an incomplete ring wait without occupying the device
worker, then execute the ordered work and issue its fence. The Windows
miniport must let independent requests make progress while that response
is outstanding. Deferring only the host response leaves the miniport's
single command slot occupied.

The Windows lab report is `venus-guest-ring-wait-results.md` in
`physical-wheel`. Raw A/B captures use `stock-dwm-ring-*`, and the reboot
and per-context records are in `run-init-cache-live.log`.

#### Trial of waking a synchronous host wait from ring progress

The ring worker published its own cursor and signaled a host condition
variable after advancing it or stopping. One host build parked the
control worker on this signal immediately. A second kept the existing
short yield window before parking. Both held the control command and the
Windows miniport's shared slot until the wait completed. The client ICD
and WDDM driver were the same in all runs.

Each build rendered six runs of 300 D3D11 frames on the RTX 2070 and
verified the final readback `(252, 127, 2)`. The workload used a copy
swap chain. Each animation capture sent 12 Notepad minimize/restore
pairs through virtio-input and used the first host window capture with
at least 500 changed pixels. Timings are median / maximum.

| Host wait | D3D11 frames/s, median (range) | Minimize to pixels | Restore to pixels | Restore flush to pixels |
| --- | ---: | ---: | ---: | ---: |
| Polling | 361.2 (296.7–397.2) | 80.3 / 118.3 ms | 165.9 / 232.7 ms | 13.4 / 16.0 ms |
| Immediate park | 316.7 (118.9–371.7) | 70.0 / 150.6 ms | 167.8 / 246.5 ms | 11.6 / 23.9 ms |
| Short yield, then park | 175.3 (137.1–234.7) | 96.2 / 133.4 ms | 198.4 / 274.1 ms | 13.2 / 23.7 ms |
| Polling restored | 325.2 (282.7–355.7) | 73.2 / 114.2 ms | 182.3 / 210.7 ms | 9.8 / 20.0 ms |

The D3D11 logs recorded exactly 302 ring waits and 301 GPU ring fences
per process in all runs. The median sum of ring waits per run was
327.1 ms with polling, 396.9 ms with immediate parking, 624.3 ms with a
short yield followed by parking, and 389.7 ms when polling was restored.
This is time inside the host dependency wait, including worker scheduling
and completion detection. The profiles do not isolate those two costs.
Immediate parking overlapped the repeat control's throughput and showed
no consistent restore latency gain. The short yield variant was
substantially slower than either control. The two park variants were left
in the lab. Polling remains the production behavior.

In the repeat control the dominant restore interval was from the guest
window event to the corresponding scanout flush, at 131.2 ms median.
The 210.7 ms capture spent 5.6 ms delivering the key, 56.3 ms from key to
window event, 136.9 ms from the event to scanout flush, and 12.0 ms after
flush before the pixel threshold. The event-to-flush interval includes
guest rendering and transport and does not by itself assign time to
individual DWM Present calls.

The next transport change should make an unrelated context runnable
while a ring dependency is pending. The Windows Venus queue signal sends
a standalone wait in a fenced `SUBMIT_3D` (`vn_queue.c` and the Windows
renderer backend `vn_renderer_entangled.c`). This is a narrow case to
defer first. Keep the validated response descriptor and the same-context
ordering until the ring advances, then issue the normal GPU fence. A
failed ring dependency must complete its own descriptor with an error
without retiring older GPU fences on that timeline. The guest miniport
needs independently owned slots and must match completions to those
slots. A two-context test with one deliberately parked ring can verify
that independence before measuring animation tails.

The host logs are `run-init-cache-live.log` and `run-venus-ring-*.log`
in `physical-wheel`. The six runs per build are in
`share/venus-guest-ring-wait/d3d11-host-*.txt`. Raw gesture captures
use the prefix `stock-dwm-host-*`.

#### Deferred Venus ring waits on the host

`ENTANGLED_VENUS_DEFER_RING_WAITS=1` enables the host experiment. An
eligible fenced `SUBMIT_3D` consists of a single `vkWaitRingSeqnoMESA`
on a GPU ring timeline. While its cursor is outstanding, the device
retains the response segments and reserves a fence slot. The ring worker
requests a queue wake when its host-owned cursor reaches the target or
the ring exits. A single per-device timer requests a wake at the earliest
timeout, including when a driver call stops the ring worker. Both use the
normal device worker to return the descriptor. That keeps writes behind
the VM's pause gate.

The device submits the GPU fence after ring progress, then waits for the
normal fence completion. A failure returns an error for just that
descriptor. The VirtIO GPU header treats fence IDs on a ring as part of
its context-specific timeline. The split queue can complete independent
descriptors in completion order. See the
[VirtIO 1.3 specification](https://docs.oasis-open.org/virtio/virtio/v1.3/virtio-v1.3.html)
and its GPU control header and split virtqueue sections.

Ordinary submissions keep their existing path. A later request from the
same context settles the earlier dependency before dispatch, using the
synchronous wait when necessary. The Windows ICD already serializes
IOCTLs in each context. The option requires a live host waker and is
disabled by default. Checkpoints refuse outstanding CPU dependencies
and a reset drops their descriptors and stops the deadline timer.

The split-queue test leaves one context parked, completes the second
context's GPU fence, and then steps the first context through cursor
progress and GPU completion separately. The timeout test verifies that a
timer wake returns only the failed wait and leaves an older fence on its
timeline pending. A real ring-worker test verifies that advancing the
cursor requests the device wake. The Windows render miniport still has
one command slot. Independent GPU clients in the guest need multiple
slots and used-ID matching before this path can reduce their contention.

The opt-in host booted the four-vCPU Windows lab with the existing
Windows Venus ICD and WDDM 21.22.46.312. The RTX 2070 log records real
`SUBMIT_3D` descriptors deferred and completed through the new path in
`physical-wheel/run-venus-host-deferred.log`.

#### Parallel fenced waits in the Windows render miniport

The lab driver builds with `CL=/DENTANGLED_GPU_PARALLEL_CONTROL=1`.
Use it with `ENTANGLED_VENUS_DEFER_RING_WAITS=1` on the host. The guest
uses slot zero for ordinary control commands and up to seven additional
slots for standalone fenced ring waits owned by different handles.
The driver identifies completions by the used descriptor ID and validates
the length and fence ID before completing the request asynchronously.
A one-millisecond timer reaps used entries while requests are pending.

Each slot owns its DMA buffers. An abandoned request may return an error
or cancellation to the client before the slot is reusable. The buffer
remains reserved until the used entry arrives or the device resets.
The lab path refuses overlapping IOCTLs on the same handle during one of
these waits. The Mesa Windows backend serializes the relevant IOCTLs per
renderer handle. Default builds retain the sequential dispatcher.

`gpu-test hold-venus-ring` published a wait for a roundtrip that would
never be signaled in its private context. A second process started a
300-frame D3D11 rendering probe during the five-second wait. Both tests
used the same Windows lab, RTX 2070 and host ring deferral.

| Render driver | Successful waits from the D3D11 client while the first context waited | D3D11 probe finished relative to the held reply |
| --- | ---: | ---: |
| Released one-slot driver | 0 | 9,261 ms after |
| Signed lab driver, run 1 | 300 | 213 ms before |
| Signed lab driver, run 2 | 300 | 99 ms after |
| Signed lab driver, run 3 | 300 | 454 ms before |

The host recorded the held request on descriptor 2 and the independent
work on descriptor 4 in all three lab runs. Each D3D11 probe verified the
last frame's RGB value `(252, 127, 2)`. Each held request returned
`ERR_INVALID_PARAMETER` (`0x1205`) at the normal host timeout. Context
cleanup passed after installing and rebooting into render driver
`15.45.52.509`. These runs verify cross-client progress under a held
dependency.

In a warm run without the held client, the lab driver restored Notepad
in 146.4 ms median and 160.2 ms maximum across twelve gestures. The
previous one-slot run with the same host option recorded 164.1 ms median
and 187.0 ms maximum. The D3D11 smoke median fell from 341.5 to 218.1
frames/s across six readback-verified runs per driver. These measurements
come from separate boots and do not measure Wintanyl's own animations.
Keep the feature opt-in. Measure interrupt-driven completion alongside
the polled lab path before deciding on a driver default.

The raw host log is `physical-wheel/run-venus-parallel-control.log` and the
guest QPC samples are `share/parallel-control/*.out`.

#### MSI-X completion for parallel waits

The parallel lab build binds one MSI-X message to the control queue in
D0. It requests notifications while a fenced wait is outstanding. The
interrupt schedules a DPC which matches used entries to the same slots
as the timer. The two paths take the same slot lock and detach each
request once. The timer remains a 16 ms watchdog when MSI-X is active
and uses the 1 ms interval when Windows or the device supplies no usable
message. The [KMDF interrupt lifecycle](https://learn.microsoft.com/windows-hardware/drivers/wdf/enabling-and-disabling-interrupts)
defines the D0 callbacks used to bind and release the queue vector.

The guest service parameter `ControlPollOnly` is a DWORD under
`Services\EntangledGpu\Parameters`. Setting it to 1 and restarting
selects the timer path in the same lab binary. The `gpu-test control-stats`
command reports the negotiated route and the number of requests completed
by each path. Normal packages still build with the parallel dispatcher
disabled.

Two boots of the same signed render driver `16.14.39.639` exercised
both routes on the four-vCPU Windows lab and RTX 2070. The host used
the same deferred-wait engine and each route ran six D3D11 pixel
readback tests and twelve Notepad minimize/restore pairs.

| Measurement | Timer | MSI-X |
| --- | ---: | ---: |
| D3D11 median, frames/s | 361.05 | 457.10 |
| Host fence retirement to next request, median | 1.083 ms | 0.668 ms |
| Minimize to pixels, median / maximum | 62.0 / 107.2 ms | 56.9 / 81.7 ms |
| Restore to pixels, median / maximum | 137.9 / 155.5 ms | 144.0 / 165.7 ms |
| Restore window event to scanout flush, median | 111.3 ms | 105.4 ms |
| Restore scanout flush to pixels, median | 7.0 ms | 10.8 ms |

During the D3D11 sampling window the timer-only boot completed 2,368
requests from the timer. The MSI-X boot counted 2,353 interrupt and 20
timer completions. Median host ring waits were 0.924 and 0.937 ms,
respectively. Median GPU fence retirement was near 0.06 ms in both
boots. The shorter interval after host completion coincided with the
higher D3D11 throughput. The Notepad restore measurements still spend
most of their time before scanout flush.

In all three held-ring runs with MSI-X, the D3D11 client completed 300
waits on descriptor 4 while the private dependency occupied descriptor 2.
The client verified the final RGB value 1.31–1.92 seconds before the
other request returned its expected five-second timeout. The trace is
`physical-wheel/run-venus-parallel-msix.log`. The guest counters and
held-client logs are in `share/parallel-msix/`.

#### Full Wintanyl with MSI-X and a 430 ms first cube transition

The opt-in D3D12 UMD from `d7e31cf` rebuilds the full-shell lab UMD
from the repository. The rebuilt DLL differed from the installed
21.22.46.312 package DLL only in the PE COFF and debug-directory
timestamps. The current Wintanyl shell, built from `04fe598`, loaded the
installed DLL. Its UI and scene devices reported Entangled DX12. Shell
PID 3664 owned Venus contexts 937985 and 937986 on the physical RTX
2070. The render miniport was in MSI-X mode. Windows Defender real-time
protection remained enabled.

The fixture used two Notepad windows and the default Wintanyl effects.
The host script sent the Windows and cube shortcuts through virtio-input.
A guest keyboard hook, the shell's scene QPC markers, full-size 3D
scanout flushes and host-window WGC placed the stages on one timeline.
The minimum clock-bridge round trips were 0.60 and 0.66 ms.

| Action | Samples | Host command to changed pixels, median / maximum | Last flush to pixels, median | Effect ticks in 1.35 s, median |
| --- | ---: | ---: | ---: | ---: |
| Minimize | 6 | 248.8 / 353.6 ms | 12.6 ms | 3 |
| Restore | 6 | 153.6 / 221.0 ms | 12.8 ms | 3.5 |
| Cube | 8 | 78.3 / 430.1 ms | 10.7 ms | 13 |

The first cube shortcut reached the guest keyboard at 8.5 ms. Setup
ended at 18.8 ms, and its first DirectComposition `Commit` returned at
53.4 ms. The first full-size scanout flush after that commit was at
423.1 ms, and WGC recorded changed pixels at 430.1 ms. The shell's first
rotation tick began at 491.4 ms. The first visible change therefore
preceded the first rotation tick. The three observed shell-owned Venus
waits before 430.1 ms completed by 41.4 ms, with a maximum duration of
3.6 ms. These waits cover the shell's contexts. The trace still needs
the guest compositor's work and wait states to attribute the interval
after the first commit. The host flush is a timing match to WGC, with
no shared frame identifier. Thresholds of 150 and 500 changed pixels
gave the same onset timings. Raw probes and state are in
`F:/VMs/Entangled/d3d12-lab/share/repro-shell/` and
`F:/VMs/Entangled/d3d12-lab/physical-wheel/` (prefix `shell-msix-current`).

#### Repeating the animation trace with the release host engine

The 430 ms cube trace used a debug host engine built from `6fbf7e6`.
The published Windows engine in v0.2.176 comes from `1352c24`, which
has identical host code under `apps/` and `crates/`. Only the
standalone engine executable was extracted from the installer. The
lab used the same VM disk, 1920 × 1080 guest mode, display UMD,
MSI-X render miniport, Wintanyl build, two Notepad fixtures and trace
settings. Each series contained six minimize and restore pairs and
eight cube shortcuts through virtio-input. The onset threshold was
500 changed pixels in the Entangled window.

| Host engine | Minimize median / max | Restore median / max | Cube median / max |
| --- | ---: | ---: | ---: |
| Debug, warmed guest | 205.8 / 241.0 ms | 179.8 / 210.8 ms | 75.6 / 103.6 ms |
| Release v0.2.176, fresh guest boot | 122.3 / 197.3 ms | 160.0 / 172.6 ms | 55.1 / 144.0 ms |
| Debug, fresh guest boot | 185.5 / 430.2 ms | 219.7 / 265.2 ms | 116.2 / 315.2 ms |
| Release v0.2.176, another fresh guest boot | 125.4 / 160.2 ms | 159.4 / 181.3 ms | 48.9 / 194.4 ms |

The median summed DWM ring overlap from input to changed pixels during
minimize was 95.5 ms and 103.9 ms in debug, and 13.2 ms in release.
The longest individual wait overlapping any of the 20 actions was
115.7 ms in the warmed debug series, 205.7 ms after the fresh debug
boot, and 9.95 and 12.52 ms in the two release series. The median
shell-context ring reply over the full series was 1.01 and 1.15 ms in
debug, and 0.19 and 0.14 ms in release.
The waits are timeline overlaps and the trace carries no frame ID that
links each of them to WGC.

The DWM command profile averaged 40.8 µs of elapsed time per command in
the sampled two-second windows of the warmed debug run and 11.2 µs in
release. The corresponding averages for `CmdPipelineBarrier2` were
61.1 and 6.5 µs. The number of commands and frames differs between
runs. The comparison covers the full build profiles including debug
assertions and optimization. The reversion to debug reproduced the
long ring delays after another boot. Use a release host engine for
latency baselines, with debug runs for instrumentation and diagnosis.
Raw times are in the same lab directories with prefixes
`shell-hostdbg-ab1`, `shell-hostrel-ab1`, `shell-hostdbg-cold-ab3` and
`shell-hostrel-final-ab4`. A warm release repeat was excluded when the
host capture stopped on a window resize.

#### Restoring a window still uploads its shared texture on the CPU

The release engine also completed a separate run with Dwm-Core, DxgKrnl
and WPR CPU traces. The ETL headers reported no lost events or buffers.
The clock bridge round trips were 0.685 and 0.749 ms. Restore in this
instrumented series had a median of 223.6 ms and a maximum of 269.7 ms.
The two release baseline series above omit WPR and guest ETW recording.

In the 269.7 ms sample, the guest keyboard hook was at 21.3 ms and
`MINIMIZEEND` at 95.1 ms. The Wintanyl lamp started entering at 141.0 ms
and its next DirectComposition commit returned at 146.6 ms. The DWM
draw scope matched to the first changed pixels ran from 206.7 to
212.0 ms, Present began at 256.7 ms, the host flush was at 262.7 ms and
WGC saw the change at 269.7 ms. Earlier flushes within the interval
left the observed region unchanged. The association uses timestamps.

Between draw return and Present entry, the guest scheduler showed the
DWM render thread running for 44.1 of 44.7 ms. In 26 of 33 distinct
user-space sampling timestamps, the instruction pointer was at the same
`YmmLoopNT` offset. The stack led through `util_copy_box`,
`u_default_texture_subdata`, `zink_image_subdata` and
`d3d10_wddm_fetch` from `d3d10_wddm_set_sampler_views`. The
DriverStore D3D10 image hash matched the DLL from the linker map.
The longest observed DWM ring wait between the window event and
this flush was 1.46 ms.

The shared-window path is distinct from the desktop scanout path.
`d3d10_wddm_fetch` reads a changed WDDM allocation into a CPU buffer
and uploads it into a process-local GPU texture before sampling.
The generation check already skips unchanged allocations. Preserving
the producer and consumer's ordering while sharing the GPU backing is
the next surface-sharing milestone. The scratch evidence uses the
prefix `shell-hostrel-final-etw` in the same lab directories.

#### Windows render miniport can create an unmapped Venus blob

`IOCTL_ENTANGLED_GPU_BLOB_CREATE_UNMAPPED` creates a shareable HOST3D
blob and attaches it to the caller's Venus context. It leaves the CPU
address and map information zero and consumes no shared-memory BAR
range. Both creation paths record ownership on the render-device file
handle. Explicit destruction requires the recorded context and resource
pair. File cleanup unmaps the mapped blobs and releases both kinds.

The driver compiled with the default and parallel control queue settings.
The Windows x64 lab VM booted the signed MSI-X variant (17.35.13.751,
`oem21.inf`) on release host 0.2.176. `gpu-test unmapped-blob` created
both blob kinds in one context, checked explicit destruction and a
second destruction of the stale ID, then closed and reopened the same
context. The host log showed three shareable creates and matching unrefs
with no MAP or UNMAP for those IDs. The two mapped controls reused the
same 4 KiB BAR range on consecutive passes. `gpu-test context-cleanup`
and `gpu-test venus` also passed. The system D3D12 runtime verified a
73×41 draw and the 320×240 flip-discard composition probe passed its
DXGI and DWM calls. The Wintanyl release `04fe598` loaded the Entangled
D3D12 UMD after the guest restart. Raw logs are in
`share/unmapped-blob` under the Windows lab directory.

The transport test uses `blob_id=0` (plain renderer memory). A separate
raw Venus probe in the same VM created a 256×256 optimal BGRA8 image on
the RTX 2070 and backed it with 262,144 bytes of device-local memory
(type 1, flags `0x1`). The host exported `blob_id=112` as shareable
resource 65957 and never mapped it. A second Vulkan device in the same
Venus context imported that resource with `VkImportMemoryResourceInfoMESA`
and bound another optimal image. The blob was then released.

The pixel probe repeated that setup with aliased images. The producer
cleared its image on the RTX and released ownership to the external
queue family. The second Vulkan device acquired ownership and copied
the image to a staging buffer. All 65,536 BGRA pixels matched in two
runs (resources 66023 and 66045). The shared texture stayed unmapped.
Only the staging buffer and the command transport were mapped.
The probe waited for each GPU queue to finish before it moved on, which
checks pixel continuity with an explicit ordering on this GPU.
The sources and output are in `share/unmapped-blob`.

The WDDM handoff still needs to attach the producer's blob to the
consumer's Venus context after checking access to the opened WDDM
allocation. Both UMDs must order producer completion and the consumer's
sampling. Until then the display UMD uses the existing CPU pixel path
and these results give no new animation timings.

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
