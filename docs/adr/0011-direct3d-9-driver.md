# ADR-0011: A Direct3D 9 driver for Windows guests

Status: **proposed** (2026-11-11). Builds on ADR-0010 (the WDDM driver for
the display function) and needs it: Windows loads a Direct3D user-mode
driver only for a WDDM display adapter.

## Context

A Windows game that is not told otherwise calls Windows' own `d3d9.dll`,
which asks the display adapter's driver for a Direct3D 9 user-mode driver.
Ours names none, so `Direct3DCreate9` gives a device nothing can be created
on and the game leaves without a word. Portal 2 was the report (ADR-0009,
the 32-bit amendment): it runs only with `-vulkan`, which is its bundled
DXVK over our Venus ICD, and only since that ICD exists for x86.

Every way around this is a switch or a prosthesis: a launch option per game,
DXVK's `d3d9.dll` copied beside each `.exe` (undone by a file verification,
an affront to anti-cheat), or DXVK written over a protected system file.
The decision of 2026-11-11 is to do what a graphics card's driver does.

Most Direct3D 9 games are 32-bit, so "a driver" is two builds from the
start, and the 64-bit one alone would not have started Portal 2.

## Decision

Write `entangled_d3d9.dll`, a WDDM Direct3D 9 user-mode driver, for x64 and
x86, named first in the display driver's `UserModeDriverName` and
`UserModeDriverNameWow`.

**What it is made of.** The Direct3D 9 DDI (`d3dumddi.h`: three adapter
functions, about 140 device functions) is the Direct3D 9 API one level
down: render states, texture stage states, transforms and lights, FVF and
declarations, shader bytecode, resources the runtime locks. Mesa had a
Direct3D 9 frontend that turns exactly that into Gallium — Gallium Nine:
the state machine (`nine_state.c`), the fixed-function pipeline as shaders
(`nine_ff.c`), shader model 1–3 translation (`nine_shader.c`), formats and
caps — removed in Mesa 25.2, last in 25.1.9 (MIT). The driver is that code,
carried forward to the Mesa 26.2.6 tree the Direct3D 10 driver already
builds from, with a DDI front in place of Nine's COM objects where the
runtime now owns them (state blocks, swap chains, parameter validation),
over Zink and the Venus ICD, with the WDDM winsys and present path of
ADR-0010 (`wddm_sw_winsys.c`, `d3d10_wddm.c`).

**Rejected.**
- *DXVK inside the driver.* Faster per draw (one translation instead of
  two), but DXVK implements the API, presents to a window through a Vulkan
  swapchain and owns its resources; under a DDI the runtime owns all three.
  It would be the prosthesis again, one level down.
- *D3D9On12.* Windows' own mapping layer needs a Direct3D 12 user-mode
  driver, and Mesa has none (its `d3d12` is the other direction).
- *A translator of our own.* Nine's 31 000 lines are ten years of games.

## Phases

- **N0 — the runtime takes the driver.** A DLL with `OpenAdapter`, caps and
  a device whose functions answer and do nothing, x64 and x86; the INF names
  it. Done when `Direct3DCreate9` + `CreateDevice` succeed in a 32-bit and a
  64-bit test program and the log shows which DDI calls a real game makes
  first.
- **N1 — Nine in the tree.** Nine's sources on Mesa 26.2.6, built with
  clang-cl for x64 and x86 with Zink and the WDDM winsys (the x86 Gallium
  build is new). Done when the DLL links.
- **N2 — a triangle.** Resources, locks, vertex/index buffers, draw, present
  through the kernel driver to DWM. Done when a test program shows a
  textured, lit triangle under the WDDM desktop.
- **N3 — a game.** Shaders, the fixed-function paths, render targets,
  queries, lost-device behaviour. Done when Portal 2 starts with no launch
  option and plays a level.
- **N4 — speed and breadth.** The overlay plane for a full-screen game, a
  second and third game, the 32-bit build of the Direct3D 10 driver beside
  it (the same x86 Gallium build).

The WDDM driver must also become the default before any of this reaches a
user without an opt-in; ADR-0010's "still owed" list is that work.

## Consequences

- Two more DLLs in the 3D package and two more builds in the workflow.
- Nine is unmaintained upstream: every Mesa bump of ours carries it forward
  by hand. The Gallium interface it uses is the one the Direct3D 10
  frontend uses, which bounds the cost.
- Draws go Direct3D 9 → Gallium → Zink → Venus → the host's Vulkan. It will
  not match DXVK's frame rates; the `-vulkan` path stays for whoever wants
  them.

## Record

### N0 (2026-11-11): the runtime takes the driver

`guest/windows-gpu/d3d9/` is the skeleton: `OpenAdapter`, the caps, and a
device whose 142 functions are placeholders generated from the WDK's header
(`gen-stubs.py` reads it preprocessed; the shape of each matters on x86,
where the callee pops its arguments), with real resource handles behind
`pfnCreateResource2`. It builds for x64 and x86, the INF names both
(`UserModeDriverName`, `UserModeDriverNameWow`), and the package, the
installer and the workflow carry them.

In the test guest (Windows 10 19045, WDDM 2.7, d3d9.dll 10.0.19041.7802),
`d3d9-test.exe` and its 32-bit build both get: the adapter named with our
DLL, `CheckDeviceType` and `GetDeviceCaps` for `D3DDEVTYPE_HAL` (vertex and
pixel shader 3.0), `CreateDevice` with hardware vertex processing, `Clear`
and `Present`, all `S_OK`. Nothing is drawn: the device's functions answer
and do nothing. What the runtime does to a new device, in order, is in the
driver's log: `Flush1`, a back buffer (the window's client area, X8R8G8B8),
a D24S8 depth buffer, six vertex and index buffers of its own, the render
states, `Lock`, `SetRenderTarget`, `SetDepthStencil`, `Clear`, `Present1`.

**What it took, which no document says.** For most of a day every call
answered `D3DERR_NOTAVAILABLE` (0x8876086a), `pfnCreateDevice` was never
called, and nothing said why. Three things were between the DLL and a device:

1. *The interface version.* The WDK's default `D3D_UMD_INTERFACE_VERSION` is
   its own newest (WDDM 3.2); the driver is built for and answers WDDM 2.0
   (0x5002, `ddi.h`).
2. *A list of minimum hardware.* d3d9.dll holds a driver that reports
   shader model 2 or later to a fixed list of caps, and one that falls
   short of any of them has its `DevCaps` zeroed and its format list cut
   down to display modes — silently; the adapter then simply has no HAL.
   Ours fell short by one bit, `D3DPMISCCAPS_FOGINFVF` (0x2000), which
   exists only in the DDI's header (`d3dhal.h`), not in the SDK's. The list,
   as the runtime checks it for a shader model 3 driver:
   - `TextureCaps` and `FVFCaps` not zero; `MaxVertexIndex` ≥ 0xfffe;
     `Caps2` has `DYNAMICTEXTURES`; `NumSimultaneousRTs` 1–4; `RasterCaps`
     has `SCISSORTEST`;
   - `PrimitiveMiscCaps` has `MASKZ`, the three `CULL` bits,
     `COLORWRITEENABLE`, `BLENDOP` and `FOGINFVF`, and not `LINEPATTERNREP`;
     with more than one render target also `INDEPENDENTWRITEMASKS` and
     `MRTPOSTPIXELSHADERBLENDING`, and `MaxUserClipPlanes` ≥ 6;
   - `MaxStreams` ≥ 8; no `D3DVTXPCAPS_NO_VSDT_UBYTE4`; `DevCaps2` has
     `STREAMOFFSET` (a logged error without) and
     `VERTEXELEMENTSCANSHARESTREAMOFFSET`; `DeclTypes` has `UBYTE4`,
     `UBYTE4N`, `SHORT2N`, `SHORT4N`, `FLOAT16_2`, `FLOAT16_4`;
   - vertex shader 3.0: `VS20Caps` predication, dynamic flow depth ≥ 24,
     temps ≥ 32, static flow depth ≥ 4; 512–32768 instruction slots;
     `MaxVertexShaderConst` ≥ 256; guard bands of at least ±8192;
     `RasterCaps` has `FOGVERTEX`; `FVFCaps` has `PSIZE`;
     `VertexTextureFilterCaps` has point min and mag;
   - pixel shader 3.0: all five `PS20Caps` bits, dynamic flow depth ≥ 24,
     temps ≥ 32, static flow depth ≥ 4, ≥ 512 slots; 512–32768 instruction
     slots; textures ≥ 4096, `MaxTextureRepeat` ≥ 8192, anisotropy ≥ 16;
     `RasterCaps` has `MIPMAPLODBIAS`, `ANISOTROPY`, `COLORPERSPECTIVE`,
     `SCISSORTEST`, `SLOPESCALEDEPTHBIAS`, `DEPTHBIAS`; every Z and alpha
     compare; every source blend, destination blends 0x23ff; point, linear
     and anisotropic min, point and linear mip and mag (cube and volume:
     point and linear); all six address modes; all nine stencil ops;
     `TextureCaps` has `PERSPECTIVE`, `PROJECTED`, `CUBEMAP`, `VOLUMEMAP`,
     the three `MIP*MAP` bits and `TEXREPEATNOTSCALEDBYSIZE`, and not
     `SQUAREONLY`;
   - in the format list: no `3DACCELERATION` without `DISPLAYMODE`, none on
     A8R8G8B8 or A1R5G5B5 ("an alpha format"), `MEMBEROFGROUP_ARGB` only on
     the RGB display formats — these three are logged errors and disable
     the driver outright.
3. *The newer six functions.* A driver of this interface version must give
   `pfnPresent1`, `pfnFlush1`, `pfnCreateResource2`, `pfnVolBlt1`,
   `pfnTexBlt1` and `pfnBufBlt1` and leave the older form of each NULL, or
   `CreateDevice` fails — this one with a logged error.

**How it was found**, because the next silent refusal will need the same.
d3d9.dll keeps the sentences its debug build would have printed: a table of
its last errors in its own data section, each with the text ("UMD did not
provide non-null pfnPresent1, and null pfnPresent."). `d3d9-test.exe` prints
every run of text in that section after a failure, which is that table. The
caps list has no sentence: it came from reading the function in d3d9.dll
that decides whether an adapter has a HAL, after the test program printed
what the runtime's enumerator held for the adapter (ours, with `DevCaps` 0
and one format). The test still carries both readers; the second is for that
one build of the DLL and checks for it.

Ruled out along the way, and not it: the interface version among 2.0,
Windows 7 and Vista; `D3DCAPS_READ_SCANLINE`, `DDCAPS2_CANRENDERWINDOWED`,
`D3DDEVCAPS_FLOATTLVERTEX`; buffer formats in the list; multisample bits;
the kernel driver (the runtime asks it nothing between the caps and the
decision).

Until a device draws (N2), the driver serves only its test program and any
process with `ENTANGLED_D3D9=1`: everyone else is answered `E_FAIL` from
`OpenAdapter`, which is what they were told before the DLL existed.

VirtualBox's driver answers the same six caps queries (two query types, a
zero gamma answer); read for what it answers, not copied (it is GPL).

### N1 (2026-11-11): Nine in the tree, and in the driver

`entangled_d3d9.dll` is now Nine and Zink behind the DDI front, about 12 MB
for each of x64 and x86, built by Mesa from the tree `prepare-d3d10.ps1`
makes (`build-d3d9.cmd`; `guest/windows-gpu/d3d9/build.cmd` builds only the
test program now).

- **Carrying Nine forward** was smaller than feared: `0003-nine-on-mesa-26.2.patch`
  is under a thousand lines over 25.1.9's sources. What had moved in Gallium
  between the two: the shader stage names (`MESA_SHADER_*`), `UNREACHABLE`,
  surfaces (a description held by value in the framebuffer state, no
  `create_surface`), vertex buffers (set without a reference), the uploader's
  `_ref` forms, `set_constant_buffer` and `clear` (a colour mask per draw
  buffer), the NIR options (a table of the screen's), and a cap that is now
  every driver's (unscaled polygon offset).
- **On Windows** Nine needed three things: its few pthread calls spelled
  with Mesa's C11 threads (`nine_win32.h`), `IUnknown` (Mesa builds with a
  lean `windows.h`), and its Linux-only allocator (memfd) left out.
- **Two sets of headers.** The front sees the WDK's Direct3D 9 headers and
  Nine sees Mesa's own; they declare the same types and cannot meet in one
  translation unit. So the DLL is two sides with a plain-C seam between
  them (`d3d9_nine.h`), and each side translates. Every DDI function that
  becomes real in N2 crosses that seam.
- **x86.** Gallium, Zink and Nine built for x86 at the first attempt, with
  the cross file and `CL=-m32` of the Venus ICD's x86 build. The Direct3D 10
  driver's x86 build stops only in our own crash reporter (`d3d10_wddm.c`
  walks the stack the x64 way) — N4's 32-bit Direct3D 10 driver is that one
  function away from compiling.

In the test guest, on creating a device, the driver now makes a Gallium
screen and a Nine adapter on it and logs what Nine says, in the 64-bit and
the 32-bit test program alike:

    nine: zink Vulkan 1.3(Virtio-GPU Venus (NVIDIA GeForce RTX 2070)
    (MESA_VENUS)), vs 0xfffe0300, ps 0xffff0300, textures to 16384, 4 render
    targets

That is past "the DLL links": Nine, Zink and the 32-bit Venus ICD load and
agree inside a Direct3D 9 program of either width. The device still draws
nothing — its functions are N0's placeholders, and the adapter is closed
with the device. The caps the runtime is given are still the hand-written
table of N0, not Nine's; N2 has to take Nine's and keep them inside the
runtime's minimum list.

Not done here: a run of the workflow (the build step is rewritten and has
not run), and the 12 MB DLLs through `install windows`.

### N2 (2026-11-12): a picture

`d3d9-test.exe` and its 32-bit build draw, in a window DWM composes: a
square turning about its axis, lit by a directional light and textured from
a managed 64×64 texture with mips, out of a vertex buffer; a
`D3DFVF_XYZRHW` triangle with a colour at each corner from `DrawPrimitiveUP`
and another out of a vertex buffer; a clip-space triangle; and the square
four more times with lighting and the texture on and off. The frame is
cleared, drawn by Nine on Zink on Venus, read back and handed to the kernel
driver, which copies it onto DWM's redirection surface when the runtime
presents (`0007-a-present-between-held-frames.patch`, escape 0x904).

What the device's functions do now, across the seam: resources (render
targets, depth, textures, vertex and index buffers, system-memory ones
kept as plain memory), `Lock`/`Unlock` and their asynchronous forms and
rename, `TexBlt1`/`BufBlt1`/`Blt`/`ColorFill`/`GenerateMipSubLevels`, render
target and depth, `Clear`, render, sampler and texture-stage states,
textures, viewport and depth range, scissor, clip planes, transforms,
material and lights, declarations, both kinds of shader and their
constants, streams and indices, the six draws, queries, `Flush1` and
`Present1`.

**What the runtime does, which decides the driver's shape.**

- *It writes the fixed-function shaders itself.* A program that sets no
  shader still reaches the driver with a `vs_2_0` and a `ps_2_0` (their
  comment says `$FF`), remade as the lighting and texture states change.
  Nine's own fixed-function code runs for one case only: vertices the
  declaration calls `POSITIONT`, for which the runtime sets no vertex
  shader.
- *Its `vs_2_0` uses `ifc`*, which the shader model's documents give to 2.x;
  Nine's table said so too and is widened.
- *Vertex and index buffers arrive as system memory* (`pSysMem`). A
  program's own are announced by `Lock` with `NotifyOnly`, and copied into
  Nine's buffer at the unlock; the runtime's own dynamic ones (where it
  puts the vertices of a `Draw…UP`) are written with no word to the driver,
  so a draw from one of those is drawn from memory.
- *`DrawPrimitive2` and `DrawIndexedPrimitive2` are "out of the runtime's
  buffer, addressed in bytes"*, not "already transformed", whatever the DDI's
  document says: a clip-space `DrawPrimitiveUP` with a vertex shader bound
  comes through them. What a vertex is comes from the declaration alone.
- *The DDI's texture-stage numbering includes the sampler states* (13
  `ADDRESSU` to 31 `DMAPOFFSET`); the front sorts them back apart.

**Three things in Nine and Zink.**

- Zink in Mesa 26.2 does not serve a constant buffer given as a pointer
  (`user_buffer`): the shader reads something else, and the process dies
  later on the dangling buffer. Nine gave its fixed-function constants
  that way; they are uploaded now, like a program's. This was the last
  one found — the pre-transformed triangles drew nowhere — by putting the
  viewport into the shader as immediates, then the constants into a real
  buffer, one switch at a time.
- Zink has no `set_clip_state`; Nine's call is guarded. User clip planes
  are therefore not applied yet (N3: lowered into the vertex shader).
- A Nine device without a swap chain needs a software screen for its
  reference paths all the same: softpipe on the null winsys.

**Bring-up switches**, all environment variables read by the DLL:
`ENTANGLED_D3D9=1` (serve this process — still required for anything but
the test program), `ENTANGLED_D3D9_TRACE=1` (the first 400 draw-related
calls), `ENTANGLED_D3D9_SHADERS=1` (shader bytecode), `ENTANGLED_D3D9_LOG`
(a file to log into as well). A fault anywhere in the process is reported
by module and offset with its callers (unwound on x64).

Not done, and N3's: the caps are still N0's table rather than Nine's;
state blocks of a pure device (`StateSet`), `VolBlt1`, `DepthFill`,
`ValidateDevice`, `UpdateWInfo`; blits back to system memory
(`GetRenderTargetData`); streams other than 0 out of memory; user clip
planes; lost devices. `Present1` is a read-back and a copy per frame —
N4's overlay plane replaces it. Seen once and not understood: the 64-bit
test dying on a null pointer in a system DLL when keys were typed at it
mid-run; the report will name the module next time.

### N3 (2026-11-12): a game

Portal 2, started with no launch option and nothing in its environment,
comes up on the driver: the menu with its film behind it, and from
"Continue game" a level, drawn right — lit, textured, the portal's view
through the glass — and walked about in. Every program is served now
(`ENTANGLED_D3D9=0` in a program's environment tells it there is no driver).
It runs at 12–14 frames a second in a 1280×720 window: `Present1` reads the
frame back and copies it through the guest's CPU, and Nine's context is not
threaded. Both are N4's.

What stood between the test program and the game, in the order met:

- **An indexed draw from memory in Nine** ran its state update with stream 0
  unbound; a vertex element took its buffer index from an uninitialised map
  and the array of vertex buffers was cleared past its end. In the 32-bit
  build that is a changed stack cookie, which ends the process with a
  fast-fail no exception handler sees — the DLL carries its own
  `__report_gsfailure` to say where.
- **The runtime's worker thread.** With `pfnLockAsync`, `pfnUnlockAsync` and
  `pfnRename` given, Windows 10's d3d9.dll plays the device's calls from a
  thread of its own (two 768 KB command buffers, swapped) and calls those
  three from the program's thread meanwhile. Nothing under the front may be
  entered by two threads; the threaded context's queue was corrupted and its
  worker spun on a call number that does not exist, while the program's
  thread waited for it a second at a time, for ever. The three are not
  given, and every call arrives in order from one thread at a time
  (`ENTANGLED_D3D9_ASYNC=1` gives them back, for the day they are made safe).
- **The format list.** A game takes formats for granted that N0's list did
  not have and does not look at the refusal (a texture handle of -1, used).
  The list is now what Nine on Zink does on a desktop GPU: volumes, 10- and
  16-bit and float targets, bump maps, the block-compressed ones with ATI1
  and ATI2, depth formats as textures, INTZ and NULL. `VolBlt1` is real.
- **Buffers freed under queued draws.** Since Mesa 26 a draw in a threaded
  context's queue holds no reference to its index or vertex buffers — a
  frontend releases those through the context (`resource_release`) — and
  Nine drops them the old way. Zink's thread drew from a freed index buffer.
  Nine asks for a plain context now; porting its releases is N4's, with the
  thread.
- **An empty render area.** `Clear` and `ColorFill` with a rectangle the
  viewport leaves nothing of are skipped. Twice more, before the runtime's
  worker thread was gone, Zink began a rendering with an empty area from
  somewhere else and our host refused it (the ring is fatal, the game loses
  its device); not seen since, not explained.

**How it was found** — the driver's own instruments, all still in it:
a ring of the last device functions called and (`ENTANGLED_D3D9_RING=1`) of
what they were told, printed by the crash report with the registers, the
faulting instruction's bytes and the stack's words by module and nearest
export; a sampler (`ENTANGLED_D3D9_SAMPLE=1`) that says every five seconds
where each thread that calls the driver is, what handle it waits on, and
once, the runtime's code before its return addresses (the guest's build of
d3d9.dll is not the host's); `ENTANGLED_D3D9_TRACE=2` for where drawing goes.
Windows' Application event log (event 1000) names the module and offset of a
death no handler saw.

Not done: user clip planes (Zink has no `set_clip_state`), state blocks of
a pure device, blits back to system memory, lost devices, `DepthFill`,
`ValidateDevice`; the caps are still N0's table.

### N4, begun (2026-11-12): where the time goes, and the whole screen

Measured in the level, in a 1280×720 window (the driver says it itself every
120 presents, `pace:` in its log): 86 ms a frame, 11.5 a second. Of that:

- **27 ms inside the kernel's present**, whatever the game's vertical sync
  setting. Not the copy (a memcpy of 3.7 MB) and not the read-back from the
  GPU (1.2 ms) or the hand-over to the kernel driver (0.7 ms): a present
  into a window waits for DWM to take the frame and show it, about three of
  the guest's 120 Hz refreshes. The menu's 35 ms is the same wait. A window
  cannot be made faster than DWM here; the answer is the whole screen.
- **57 ms in 318 draws**, 0.18 ms each, every one made in turn on the
  program's thread: Nine's context is not threaded (N3), and neither is the
  runtime's (N3). Not yet profiled further.

**The whole screen does not work yet**, and it is how a game starts when
nobody has told it otherwise: Portal 2 with `-fullscreen`, and the test
program with `full`, both die inside d3d9.dll — the game at its first
`SetRenderTarget` of the back buffer, the test at its first `Present`. What
is known:

- The runtime makes the same resources as for a window: one back buffer of
  one surface (flags `RenderTarget | NotLockable`, no `Primary`, no source
  id), and calls neither `pfnOpenResource` nor `pfnSetDisplayMode`. The
  kernel is asked meanwhile for a shared primary and a shadow surface
  (`GetStandardAllocationDriverData` types 1 and 2) and gives them.
- The present goes down the runtime's flip (`DdFlipLH`, by its own error
  strings), which takes the surface to show from the *second* surface of
  the chain; that surface's link to a driver resource is NULL, and the
  runtime reads through it. The first frame is drawn all the same: the
  device's own render target points at the resource directly.
- Windows' "fullscreen optimizations" are not it
  (`__COMPAT_LAYER=DISABLEDXMAXIMIZEDWINDOWEDMODE` changes nothing), and the
  runtime's table of last errors is empty.
- So the runtime expected a chain of two — a front buffer it would have had
  from somewhere — and what decides that is in its surface-creation code
  ("Failed to create driver primary surface chain", "Failed to create dummy
  primary surface" are its sentences there), not yet read.

- The test program's variants narrow it: with two back buffers the runtime
  makes two resources of one surface each; presented by a copy
  (`fullcopy`) the back buffer is in another pool and the death is
  elsewhere in the runtime. In no variant is a front buffer made through
  the driver. The runtime's own count of the chain for
  `D3DSWAPEFFECT_DISCARD` is two (front and back), made through one of its
  internal functions, and only the backs reach `pfnCreateResource2`.

Until then a game must be started in a window (`-windowed`, or its own
setting), which Portal 2 remembers once set.

Instruments added on the way: `ENTANGLED_D3D9_CALLS=1` (the first thousand
device functions called, placeholders included), the `pace:` line, and the
test program's `full` argument. The guest's d3d9.dll is not the host's
build (functions sit 0xb0 lower); a guest file can be had by copying it
behind a marker (`copy /b marker + file`) and finding the marker in the raw
disk image with the VM off, though a fragmented copy is useless — matching
the faulting instruction's bytes in the host's build finds the function.
