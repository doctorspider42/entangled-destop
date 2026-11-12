# ADR-0013: 3D for Windows XP guests — a display driver of our own and OpenGL passed through

Status: **proposed** (2026-11-12); nothing implemented. Builds on ADR-0012
(the BIOS-era machine an XP guest runs on) and reuses nothing of ADR-0009 to
0011, for the reason given first.

## Why the existing 3D path cannot be extended down to XP

Every 3D path this project has goes through virtio-gpu with a guest driver
that speaks Venus (Vulkan on the wire) and a Mesa on top: the WDDM driver,
the Vulkan ICD, Zink, DXVK, Gallium Nine. Windows XP can take none of it:

| The Windows 7+ path needs | Windows XP has |
|---|---|
| WDDM, a KMDF driver built with a current WDK | XPDM: a display driver DLL in `win32k` and a video miniport; no WDDM, and a current WDK builds nothing that loads on XP |
| A Vulkan loader and a Mesa that runs there | No Vulkan loader exists for XP; Mesa dropped XP years ago, and its Venus and Zink drivers never ran on it |
| Direct3D through a user-mode driver (the D3D9/10 DDI) | Direct3D through the DirectDraw/D3D HAL callbacks of the display driver, or through a wrapper |

So this is a second, much smaller 3D stack, not a port.

## Decision

OpenGL first, by **passing the guest's OpenGL calls to the host's OpenGL**,
through a display driver of our own. Three guest pieces and one host piece:

1. **A display driver for XP** (`guest/winxp/display`): a video miniport and
   a display driver DLL for the machine's framebuffer function. The miniport
   sets modes through the DISPI registers; the DLL hands GDI the linear
   framebuffer as an engine-managed surface, so all 2D is GDI's own. This is
   worth having without any 3D: any resolution, no dependence on the BIOS's
   VBE or on `vga.sys`, and it is where an absolute pointer and a hardware
   cursor go. And it is required for 3D, because on XP the display driver is
   what tells `opengl32.dll` which installable client driver to load
   (`OPENGL_GETINFO` through `DrvEscape`).
2. **An OpenGL installable client driver** (`guest/winxp/opengl`, a 32-bit
   DLL): implements the ICD entry points (`DrvCreateContext`,
   `DrvSetPixelFormat`, `DrvSwapBuffers`, …) and the OpenGL API itself, and
   serialises every call into a command buffer.
3. **The channel**: the ICD hands a full command buffer to the display
   driver through `ExtEscape`; the driver copies it into a shared-memory
   window of the device and writes a doorbell register. The call returns
   when the host has executed the buffer, with any results in the same
   window. Synchronous and batched: one exit per few thousand GL calls.
4. **The host executor** (a new crate, portable): owns a real OpenGL context
   per guest context (WGL on Windows, EGL on Linux), decodes the buffer,
   validates it, issues the calls, and gives the finished frame to the
   display at `SwapBuffers`.

Direct3D comes after OpenGL and on top of it: either a D3D-to-OpenGL
wrapper in the guest (WineD3D has builds that run on XP; it is LGPL, guest
side, and the user's to install, like WinFsp in ADR-0008), or later the
DirectDraw/D3D HAL of our own driver. The first is days, the second is a
project; this ADR commits only to the first.

### Why pass OpenGL through, of all the options

- **It is the only API with a driver on both ends.** XP applications speak
  OpenGL 1.1 to 2.1 and Direct3D 7 to 9. The host has a real OpenGL driver.
  Nothing has to translate between APIs for the first one.
- **virgl's wire format would need its host.** A Gallium `virgl` driver in
  the guest (an old Mesa built for XP) would give a tested protocol, but its
  host half is `virglrenderer`, which this project has on Linux only
  (ADR-0004) — and the development host is Windows.
- **Software rendering in the guest** (Mesa `llvmpipe`) is not
  acceleration.

### What makes it safe enough to have

The guest is untrusted, and an OpenGL command stream is a large surface.
The rules, each with a test before the call exists:

- **No guest pointer and no guest length is trusted.** Every command
  carries its payload inline with a length; the executor computes what
  OpenGL will read from the call's own arguments and the pixel-store state
  it tracks (`width × height × components(format) × size(type)`, with the
  unpack row length, alignment and skip) and refuses a payload that is
  shorter. Client-side vertex arrays are sent by the guest as bytes for
  exactly the vertices a draw call names, never as addresses.
- **Object names are the guest context's own.** Every guest context is a
  host context of its own; names are translated through tables, so a name
  the guest never created is an error and not another context's object.
- **Enumerants are allow-listed**, not passed through: an unknown `GLenum`
  is refused before the host driver sees it.
- **Bounded**: buffer sizes, texture dimensions, object counts and the
  command buffer itself have limits a guest cannot raise.
- **The executor can run in a process of its own**, as the virgl renderer
  can (`virgl_isolation = "process"`): a host driver that a stream crashes
  takes the renderer, not the VM. In-process first, because the display
  needs the frame; the seam is there from the start.

This is weaker than Venus's position (Vulkan with validation-shaped
encoding) and stronger than what shipped for XP guests in other hypervisors
a decade ago. It is stated here so nobody mistakes it for more.

## Phases

| Phase | Contents | Acceptance |
|---|---|---|
| **G0 — a desktop of our own** | the miniport and the display DLL; mode list from the host; installed by INF | the XP desktop at 1920×1080×32 on our driver, with no VBE call after boot |
| **G1 — the channel** | the shared window and doorbell on the device, `DrvEscape`, a host echo | a guest program round-trips a megabyte through the driver |
| **G2 — a triangle** | the ICD's context and pixel-format entry points, immediate mode and state for OpenGL 1.1, the executor with its validation | a spinning triangle in a window, drawn by the host GPU |
| **G3 — OpenGL 1.5** | textures, client arrays, display lists, buffer objects, `wglGetProcAddress` extensions | Quake III Arena (or its demo) runs and looks right |
| **G4 — OpenGL 2.1** | shaders, framebuffer objects | an OpenGL 2.x benchmark completes |
| **G5 — Direct3D** | a D3D-to-OpenGL wrapper installed per the user's choice | a Direct3D 9 title starts |

Build: the guest pieces are 32-bit, freestanding C, compiled with clang and
linked with `lld-link` against import libraries generated from `.def`
files (`videoprt.sys`, `win32k.sys`, `gdi32.dll`), with the Windows SDK's
own `winddi.h` and `video.h` for the DDI. No XP-era DDK is needed and none
is redistributed. XP loads unsigned display drivers after one prompt, which
the unattended install answers (`DriverSigningPolicy=Ignore`).

## Out of scope

- Vulkan and Direct3D 10+ in an XP guest: the guest has neither.
- Video overlays, DirectDraw acceleration beyond what GDI gives.
- Windows 2000 and 9x: different driver models again.

## Risks

1. The ICD interface of XP's `opengl32.dll` is documented thinly; the
   entry points and their order have to be established against the real
   DLL, the way ADR-0011 did for `d3d9.dll`.
2. Presenting a host-rendered frame inside a guest window means clipping
   to the window's visible region, which the guest owns. The first cut
   presents full-screen and windowed-unobscured only.
3. Nothing here can be tested until an XP guest is installed, which needs
   the user's product key (ADR-0012, X5).
