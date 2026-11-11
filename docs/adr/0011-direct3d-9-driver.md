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

### N0, first day (2026-11-11): taken, asked, and turned down

`guest/windows-gpu/d3d9/` is the skeleton: `OpenAdapter`, the caps, and a
device whose 142 functions are placeholders generated from the WDK's header
(`gen-stubs.py`; the shape of each matters on x86, where the callee pops its
arguments). It builds for x64 and x86, the INF names both
(`UserModeDriverName`, `UserModeDriverNameWow`), and the package, the
installer and the workflow carry them.

What works, in the test guest (Windows 10 19045, WDDM 2.7):

- `IDirect3D9::GetAdapterIdentifier` names `entangled_d3d9.dll` to a 64-bit
  program and `entangled_d3d9_x86.dll` to a 32-bit one.
- The runtime loads the DLL into both, calls `OpenAdapter` (interface 9,
  runtime version 0xc001) and asks, every time a program asks it about the
  adapter: `GETD3D9CAPS`, `GETFORMATCOUNT`, `GETD3DQUERYCOUNT`,
  `GETGAMMARAMPCAPS`, then the same four again followed by `GETFORMATDATA`
  and `GETD3DQUERYDATA`, and closes the adapter. Nothing else: no DirectDraw
  caps, no Direct3D 3–8 caps.
- The answers are used: without `D3DDEVCAPS_HWTRANSFORMANDLIGHT`,
  `CreateDevice` with hardware vertex processing changes from
  `D3DERR_NOTAVAILABLE` to `D3DERR_INVALIDCALL`.

What does not: `GetDeviceCaps`, `CheckDeviceType` and `CreateDevice` for
`D3DDEVTYPE_HAL` all answer `D3DERR_NOTAVAILABLE` (0x8876086a), and
`pfnCreateDevice` is never called. **N0 is not done.**

Ruled out, each by a run (`ENTANGLED_D3D9_TRY` is a set of bits that change
one answer each, so a boot tries several):

- the interface version answered: WDDM 2.0 (0x5002), Windows 7 (0x2003) and
  Vista (0x000c) are the same; the WDK's default, WDDM 3.2, was never right
  (newer than the Windows it runs on) and is no longer what is built;
- `D3DCAPS_READ_SCANLINE`, `DDCAPS2_CANRENDERWINDOWED`,
  `D3DDEVCAPS_FLOATTLVERTEX`; shader model 2 instead of 3; `DeviceType` 0;
- the format list: A8R8G8B8 as a display format beside X8R8G8B8 (the kernel
  driver's source modes are A8R8G8B8, the mode Direct3D reports is
  X8R8G8B8), the buffer formats (`VERTEXDATA`, `INDEX16`, `INDEX32`),
  multisample type bits, a display format with only the plain operations.

So the caps are probably not it, and what is left is what the runtime asks
the *kernel* driver between reading the caps and deciding — a display-mode
list, a scan line, a device state — which `entangled-wddm.sys` has only ever
been asked by Direct3D 10 and DWM. The attempt to listen
(`kmt_trace.inc`: d3d9.dll's 64 `D3DKMT` imports pointed at logging
wrappers) heard nothing, so those calls leave by another door (gdi32's
exports looked up at run time, most likely); the next step is to listen
there, or on the kernel driver's side, where a diagnostic build already
prints every DDI it is called with.

Until a device draws, the driver serves only its test program (and any
process with `ENTANGLED_D3D9=1`): everyone else is answered `E_FAIL` from
`OpenAdapter`, which is what they were told before the DLL existed.

A reference that helped: VirtualBox's driver answers the same six queries
the same way (two query types, a zero gamma answer, the caps of its
backend), and `D3D_UMD_INTERFACE_VERSION_WIN7` for its version. Read for
what it answers, not copied (it is GPL).
