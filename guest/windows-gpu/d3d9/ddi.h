/* The Direct3D 9 DDI as entangled_d3d9.c sees it, in a header of its own so
 * that build.cmd can hand the same text, preprocessed, to gen-stubs.py. */
#ifndef ENTANGLED_D3D9_DDI_H
#define ENTANGLED_D3D9_DDI_H

/* The interface version this driver is built for and answers OpenAdapter
 * with: WDDM 2.0, the first Windows 10. The WDK's default is its own newest
 * (WDDM 3.2), which is a claim to be newer than the Windows it runs on. */
#ifndef D3D_UMD_INTERFACE_VERSION
#define D3D_UMD_INTERFACE_VERSION 0x5002
#endif

#include <windows.h>
#include <winternl.h> /* NTSTATUS, which a lean windows.h leaves out */

#include <d3d9types.h>
#include <d3d9caps.h>
#include <d3dhal.h>
#include <d3dumddi.h>

#endif
