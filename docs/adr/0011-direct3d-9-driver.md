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
