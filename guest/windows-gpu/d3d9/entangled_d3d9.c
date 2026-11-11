/*
 * entangled_d3d9.dll: the Direct3D 9 user-mode driver of entangled-wddm.sys
 * (ADR-0011). Windows' d3d9.dll loads it for the display adapter and calls
 * OpenAdapter; everything else goes through the two tables it fills in.
 *
 * Phase N0: the runtime takes the driver. The adapter answers its caps and
 * makes a device whose every function answers S_OK and says its name once -
 * which is the list of what a program calls, in the order it calls it.
 * Nothing is drawn yet.
 */
#include "ddi.h"
#include "d3d9_nine.h"

/* The kernel driver's contract for an allocation's private data. */
#include "wddm/viogpu_wddm_driver.h"

#include <stdarg.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

/* ------------------------------------------------------------------ log */

/* The kernel driver prints an escape of this type on the VM's serial log
 * (a diagnostic build of it; a quiet one drops it). */
#define ESCAPE_PRINT 0x900

/* The log goes through gdi32's thunks (d3dkmthk.h) and an adapter handle of
 * its own, so that the adapter-level code can speak before there is a device. */
static D3DKMT_HANDLE log_adapter;
static CRITICAL_SECTION log_lock;
static BOOL log_ready;

static void
d3d9_log(const char *format, ...)
{
   struct {
      unsigned short type;
      unsigned short length;
      char text[236];
   } escape;
   D3DKMT_ESCAPE call;
   va_list args;
   int at;

   char line[236];
   NTSTATUS opened = 0, escaped = 0;

   if (!log_ready)
      return;
   va_start(args, format);
   _vsnprintf(line, sizeof line - 1, format, args);
   va_end(args);
   line[sizeof line - 1] = 0;
   EnterCriticalSection(&log_lock);
   if (!log_adapter) {
      D3DKMT_OPENADAPTERFROMHDC open;

      memset(&open, 0, sizeof open);
      open.hDc = GetDC(NULL);
      opened = D3DKMTOpenAdapterFromHdc(&open);
      ReleaseDC(NULL, open.hDc);
      if (opened >= 0)
         log_adapter = open.hAdapter;
   }
   if (log_adapter) {
      memset(&escape, 0, sizeof escape);
      escape.type = ESCAPE_PRINT;
      at = _snprintf(escape.text, sizeof escape.text - 1, "d3d9 %lu: %s",
                     (unsigned long)GetCurrentProcessId(), line);
      (void)at;
      escape.length = (unsigned short)strlen(escape.text);

      memset(&call, 0, sizeof call);
      call.hAdapter = log_adapter;
      call.pPrivateDriverData = &escape;
      call.PrivateDriverDataSize = sizeof escape;
      escaped = D3DKMTEscape(&call);
   }
   /* ENTANGLED_D3D9_LOG names a file to say it all in as well: the serial
    * log is the kernel driver's to give, and a quiet build of it gives none. */
   {
      char path[MAX_PATH];

      if (GetEnvironmentVariableA("ENTANGLED_D3D9_LOG", path, sizeof path) - 1 < sizeof path - 1) {
         FILE *file = fopen(path, "a");

         if (file) {
            fprintf(file, "%lu: %s (open 0x%lx, escape 0x%lx)\n",
                    (unsigned long)GetCurrentProcessId(), line, (unsigned long)opened,
                    (unsigned long)escaped);
            fclose(file);
         }
      }
   }
   LeaveCriticalSection(&log_lock);
}

/* ---------------------------------------------------------------- crash */

/* A fault in the process this DLL is loaded into: say where, once, and let
 * it take its course. A program that dies inside its Direct3D driver says
 * nothing itself, and there is no debugger in the guest. The report is the
 * faulting address and every word of the stack above it that points into
 * this DLL, as offsets its linker map (/MAP) turns into function names:
 * cruder than an unwind, the same for x64 and x86, and enough. */
static HMODULE crash_module;
static PVOID crash_handler;

static LONG CALLBACK
d3d9_crash(EXCEPTION_POINTERS *info)
{
   static volatile LONG reported;
   DWORD code = info->ExceptionRecord->ExceptionCode;
   ULONG_PTR base = (ULONG_PTR)crash_module, top, *word, *limit;
   IMAGE_NT_HEADERS *nt;
   char line[220];
   int at = 0;
   unsigned found = 0;

   /* Errors only (not C++ throws, debug strings or breakpoints). */
   if (code < 0xC0000000u || code == 0xE06D7363u)
      return EXCEPTION_CONTINUE_SEARCH;
   if (InterlockedExchange(&reported, 1) != 0)
      return EXCEPTION_CONTINUE_SEARCH;
   nt = (IMAGE_NT_HEADERS *)(base + ((IMAGE_DOS_HEADER *)base)->e_lfanew);
   top = base + nt->OptionalHeader.SizeOfImage;
   d3d9_log("exception 0x%08lx at %p (this DLL is %p), access %p", (unsigned long)code,
            info->ExceptionRecord->ExceptionAddress, (void *)base,
            info->ExceptionRecord->NumberParameters > 1
               ? (void *)info->ExceptionRecord->ExceptionInformation[1]
               : NULL);
#if defined(_WIN64)
   word = (ULONG_PTR *)info->ContextRecord->Rsp;
#else
   word = (ULONG_PTR *)info->ContextRecord->Esp;
#endif
   limit = (ULONG_PTR *)((NT_TIB *)NtCurrentTeb())->StackBase;
   for (; word < limit && found < 40; word++) {
      if (*word < base || *word >= top)
         continue;
      at += _snprintf(line + at, sizeof line - 1 - at, " 0x%lx", (unsigned long)(*word - base));
      found++;
      if (at > 150) {
         line[at] = 0;
         d3d9_log("  stack:%s", line);
         at = 0;
      }
   }
   if (at) {
      line[at] = 0;
      d3d9_log("  stack:%s", line);
   }
   return EXCEPTION_CONTINUE_SEARCH;
}

/* ---------------------------------------------------------------- stubs */

static volatile LONG stub_said[160];

static HRESULT
d3d9_stub(unsigned index, const char *name)
{
   if (index < 160 && InterlockedExchange(&stub_said[index], 1) == 0)
      d3d9_log("stub: %s", name);
   return S_OK;
}

#include "entangled_d3d9_stubs.inc"

/* ----------------------------------------------------------------- caps */

/* What the adapter says it can do with a format. Phase N0's list is the
 * handful a desktop program asks for; N1 takes it from the Gallium screen. */
#define OPS_COLOR                                                            \
   (FORMATOP_TEXTURE | FORMATOP_CUBETEXTURE | FORMATOP_OFFSCREEN_RENDERTARGET | \
    FORMATOP_SAME_FORMAT_RENDERTARGET | FORMATOP_CONVERT_TO_ARGB |           \
    FORMATOP_OFFSCREENPLAIN | FORMATOP_SRGBREAD | FORMATOP_SRGBWRITE |       \
    FORMATOP_MEMBEROFGROUP_ARGB |                                            \
    FORMATOP_AUTOGENMIPMAP | FORMATOP_VERTEXTEXTURE)
#define OPS_DISPLAY (OPS_COLOR | FORMATOP_DISPLAYMODE | FORMATOP_3DACCELERATION)
#define OPS_DEPTH \
   (FORMATOP_ZSTENCIL | FORMATOP_ZSTENCIL_WITH_ARBITRARY_COLOR_DEPTH)
#define OPS_TEXTURE \
   (FORMATOP_TEXTURE | FORMATOP_CUBETEXTURE | FORMATOP_OFFSCREENPLAIN | \
    FORMATOP_CONVERT_TO_ARGB)

static const FORMATOP d3d9_formats[] = {
   {D3DDDIFMT_X8R8G8B8, OPS_DISPLAY, 0, 0, 0},
   /* Never a display mode: d3d9.dll disables a driver that reports "an alpha
    * format with D3DFORMAT_OP_3DACCELERATION" (its words). */
   {D3DDDIFMT_A8R8G8B8, OPS_COLOR | FORMATOP_SAME_FORMAT_UP_TO_ALPHA_RENDERTARGET, 0, 0, 0},
   {D3DDDIFMT_R5G6B5, OPS_COLOR, 0, 0, 0},
   {D3DDDIFMT_A8, OPS_TEXTURE, 0, 0, 0},
   {D3DDDIFMT_L8, OPS_TEXTURE, 0, 0, 0},
   {D3DDDIFMT_A8L8, OPS_TEXTURE, 0, 0, 0},
   {D3DDDIFMT_DXT1, OPS_TEXTURE | FORMATOP_SRGBREAD, 0, 0, 0},
   {D3DDDIFMT_DXT3, OPS_TEXTURE | FORMATOP_SRGBREAD, 0, 0, 0},
   {D3DDDIFMT_DXT5, OPS_TEXTURE | FORMATOP_SRGBREAD, 0, 0, 0},
   {D3DDDIFMT_D16, OPS_DEPTH, 0, 0, 0},
   {D3DDDIFMT_D24X8, OPS_DEPTH, 0, 0, 0},
   {D3DDDIFMT_D24S8, OPS_DEPTH, 0, 0, 0},
};

static const D3DDDIQUERYTYPE d3d9_queries[] = {
   D3DDDIQUERYTYPE_EVENT,
   D3DDDIQUERYTYPE_OCCLUSION,
};

/* Direct3D 9's caps, as a shader model 3 card of 2008 would give them. */
static void
d3d9_caps9(D3DCAPS9 *caps)
{
   memset(caps, 0, sizeof *caps);
   caps->DeviceType = D3DDEVTYPE_HAL;
   caps->Caps2 = D3DCAPS2_CANAUTOGENMIPMAP | D3DCAPS2_DYNAMICTEXTURES |
                 D3DCAPS2_FULLSCREENGAMMA | D3DCAPS2_CANSHARERESOURCE;
   caps->Caps3 = D3DCAPS3_ALPHA_FULLSCREEN_FLIP_OR_DISCARD |
                 D3DCAPS3_COPY_TO_VIDMEM | D3DCAPS3_COPY_TO_SYSTEMMEM |
                 D3DCAPS3_LINEAR_TO_SRGB_PRESENTATION;
   caps->PresentationIntervals = D3DPRESENT_INTERVAL_ONE |
                                 D3DPRESENT_INTERVAL_IMMEDIATE;
   caps->CursorCaps = D3DCURSORCAPS_COLOR;
   caps->DevCaps = D3DDEVCAPS_CANBLTSYSTONONLOCAL | D3DDEVCAPS_CANRENDERAFTERFLIP |
                   D3DDEVCAPS_DRAWPRIMITIVES2 | D3DDEVCAPS_DRAWPRIMITIVES2EX |
                   D3DDEVCAPS_DRAWPRIMTLVERTEX | D3DDEVCAPS_EXECUTESYSTEMMEMORY |
                   D3DDEVCAPS_EXECUTEVIDEOMEMORY | D3DDEVCAPS_HWRASTERIZATION |
                   D3DDEVCAPS_HWTRANSFORMANDLIGHT | D3DDEVCAPS_PUREDEVICE |
                   D3DDEVCAPS_TEXTURENONLOCALVIDMEM | D3DDEVCAPS_TEXTUREVIDEOMEMORY |
                   D3DDEVCAPS_TLVERTEXSYSTEMMEMORY | D3DDEVCAPS_TLVERTEXVIDEOMEMORY;
   caps->PrimitiveMiscCaps = D3DPMISCCAPS_MASKZ | D3DPMISCCAPS_CULLNONE |
                             D3DPMISCCAPS_CULLCW | D3DPMISCCAPS_CULLCCW |
                             D3DPMISCCAPS_COLORWRITEENABLE |
                             D3DPMISCCAPS_CLIPPLANESCALEDPOINTS |
                             D3DPMISCCAPS_TSSARGTEMP | D3DPMISCCAPS_BLENDOP |
                             D3DPMISCCAPS_INDEPENDENTWRITEMASKS |
                             D3DPMISCCAPS_PERSTAGECONSTANT | D3DPMISCCAPS_FOGANDSPECULARALPHA |
                             D3DPMISCCAPS_SEPARATEALPHABLEND |
                             D3DPMISCCAPS_MRTPOSTPIXELSHADERBLENDING |
                             D3DPMISCCAPS_FOGVERTEXCLAMPED |
                             /* A DDI-only bit (d3dhal.h), and not optional: d3d9.dll
                              * holds a shader model 2+ driver to a list of minimum
                              * caps, this one among them, and one that falls short has
                              * its DevCaps zeroed and its formats cut down to display
                              * modes - without a word, as D3DERR_NOTAVAILABLE from
                              * every call (ADR-0011 has the whole list). */
                             D3DPMISCCAPS_FOGINFVF;
   caps->RasterCaps = D3DPRASTERCAPS_ANISOTROPY | D3DPRASTERCAPS_COLORPERSPECTIVE |
                      D3DPRASTERCAPS_DITHER | D3DPRASTERCAPS_DEPTHBIAS |
                      D3DPRASTERCAPS_FOGRANGE | D3DPRASTERCAPS_FOGTABLE |
                      D3DPRASTERCAPS_FOGVERTEX | D3DPRASTERCAPS_MIPMAPLODBIAS |
                      D3DPRASTERCAPS_SCISSORTEST | D3DPRASTERCAPS_SLOPESCALEDEPTHBIAS |
                      D3DPRASTERCAPS_WFOG | D3DPRASTERCAPS_ZFOG | D3DPRASTERCAPS_ZTEST;
   caps->ZCmpCaps = 0xff;
   caps->AlphaCmpCaps = 0xff;
   caps->SrcBlendCaps = 0x3fff;
   caps->DestBlendCaps = 0x3fff;
   caps->ShadeCaps = D3DPSHADECAPS_COLORGOURAUDRGB | D3DPSHADECAPS_SPECULARGOURAUDRGB |
                     D3DPSHADECAPS_ALPHAGOURAUDBLEND | D3DPSHADECAPS_FOGGOURAUD;
   caps->TextureCaps = D3DPTEXTURECAPS_ALPHA | D3DPTEXTURECAPS_ALPHAPALETTE |
                       D3DPTEXTURECAPS_PERSPECTIVE | D3DPTEXTURECAPS_PROJECTED |
                       D3DPTEXTURECAPS_CUBEMAP | D3DPTEXTURECAPS_VOLUMEMAP |
                       D3DPTEXTURECAPS_MIPMAP | D3DPTEXTURECAPS_MIPVOLUMEMAP |
                       D3DPTEXTURECAPS_MIPCUBEMAP | D3DPTEXTURECAPS_TEXREPEATNOTSCALEDBYSIZE;
   caps->TextureFilterCaps = D3DPTFILTERCAPS_MINFPOINT | D3DPTFILTERCAPS_MINFLINEAR |
                             D3DPTFILTERCAPS_MINFANISOTROPIC | D3DPTFILTERCAPS_MIPFPOINT |
                             D3DPTFILTERCAPS_MIPFLINEAR | D3DPTFILTERCAPS_MAGFPOINT |
                             D3DPTFILTERCAPS_MAGFLINEAR | D3DPTFILTERCAPS_MAGFANISOTROPIC;
   caps->CubeTextureFilterCaps = caps->TextureFilterCaps;
   caps->VolumeTextureFilterCaps = caps->TextureFilterCaps;
   caps->TextureAddressCaps = D3DPTADDRESSCAPS_WRAP | D3DPTADDRESSCAPS_MIRROR |
                              D3DPTADDRESSCAPS_CLAMP | D3DPTADDRESSCAPS_BORDER |
                              D3DPTADDRESSCAPS_INDEPENDENTUV | D3DPTADDRESSCAPS_MIRRORONCE;
   caps->VolumeTextureAddressCaps = caps->TextureAddressCaps;
   caps->LineCaps = D3DLINECAPS_TEXTURE | D3DLINECAPS_ZTEST | D3DLINECAPS_BLEND |
                    D3DLINECAPS_ALPHACMP | D3DLINECAPS_FOG;
   caps->MaxTextureWidth = 8192;
   caps->MaxTextureHeight = 8192;
   caps->MaxVolumeExtent = 2048;
   caps->MaxTextureRepeat = 8192;
   caps->MaxTextureAspectRatio = 8192;
   caps->MaxAnisotropy = 16;
   caps->MaxVertexW = 1e10f;
   caps->GuardBandLeft = -32768.0f;
   caps->GuardBandTop = -32768.0f;
   caps->GuardBandRight = 32768.0f;
   caps->GuardBandBottom = 32768.0f;
   caps->StencilCaps = 0x1ff;
   caps->FVFCaps = D3DFVFCAPS_PSIZE | 8;
   caps->TextureOpCaps = 0x03ffffff;
   caps->MaxTextureBlendStages = 8;
   caps->MaxSimultaneousTextures = 8;
   caps->VertexProcessingCaps = D3DVTXPCAPS_TEXGEN | D3DVTXPCAPS_MATERIALSOURCE7 |
                                D3DVTXPCAPS_DIRECTIONALLIGHTS | D3DVTXPCAPS_POSITIONALLIGHTS |
                                D3DVTXPCAPS_LOCALVIEWER | D3DVTXPCAPS_TEXGEN_SPHEREMAP;
   caps->MaxActiveLights = 8;
   caps->MaxUserClipPlanes = 6;
   caps->MaxVertexBlendMatrices = 4;
   caps->MaxVertexBlendMatrixIndex = 0;
   caps->MaxPointSize = 256.0f;
   caps->MaxPrimitiveCount = 0x555555;
   caps->MaxVertexIndex = 0xffffff;
   caps->MaxStreams = 16;
   caps->MaxStreamStride = 508;
   caps->VertexShaderVersion = D3DVS_VERSION(3, 0);
   caps->MaxVertexShaderConst = 256;
   caps->PixelShaderVersion = D3DPS_VERSION(3, 0);
   caps->PixelShader1xMaxValue = 65504.0f;
   caps->DevCaps2 = D3DDEVCAPS2_STREAMOFFSET | D3DDEVCAPS2_VERTEXELEMENTSCANSHARESTREAMOFFSET |
                    D3DDEVCAPS2_CAN_STRETCHRECT_FROM_TEXTURES;
   caps->MasterAdapterOrdinal = 0;
   caps->AdapterOrdinalInGroup = 0;
   caps->NumberOfAdaptersInGroup = 1;
   caps->DeclTypes = D3DDTCAPS_UBYTE4 | D3DDTCAPS_UBYTE4N | D3DDTCAPS_SHORT2N |
                     D3DDTCAPS_SHORT4N | D3DDTCAPS_USHORT2N | D3DDTCAPS_USHORT4N |
                     D3DDTCAPS_FLOAT16_2 | D3DDTCAPS_FLOAT16_4;
   caps->NumSimultaneousRTs = 4;
   caps->StretchRectFilterCaps = D3DPTFILTERCAPS_MINFPOINT | D3DPTFILTERCAPS_MINFLINEAR |
                                 D3DPTFILTERCAPS_MAGFPOINT | D3DPTFILTERCAPS_MAGFLINEAR;
   caps->VS20Caps.Caps = D3DVS20CAPS_PREDICATION;
   caps->VS20Caps.DynamicFlowControlDepth = 24;
   caps->VS20Caps.NumTemps = 32;
   caps->VS20Caps.StaticFlowControlDepth = 4;
   caps->PS20Caps.Caps = D3DPS20CAPS_ARBITRARYSWIZZLE | D3DPS20CAPS_GRADIENTINSTRUCTIONS |
                         D3DPS20CAPS_PREDICATION | D3DPS20CAPS_NODEPENDENTREADLIMIT |
                         D3DPS20CAPS_NOTEXINSTRUCTIONLIMIT;
   caps->PS20Caps.DynamicFlowControlDepth = 24;
   caps->PS20Caps.NumTemps = 32;
   caps->PS20Caps.StaticFlowControlDepth = 4;
   caps->PS20Caps.NumInstructionSlots = 512;
   caps->VertexTextureFilterCaps = D3DPTFILTERCAPS_MINFPOINT | D3DPTFILTERCAPS_MAGFPOINT;
   caps->MaxVShaderInstructionsExecuted = 65535;
   caps->MaxPShaderInstructionsExecuted = 65535;
   caps->MaxVertexShader30InstructionSlots = 32768;
   caps->MaxPixelShader30InstructionSlots = 32768;
}

static const char *
caps_name(D3DDDICAPS_TYPE type)
{
   switch (type) {
   case D3DDDICAPS_DDRAW: return "DDRAW";
   case D3DDDICAPS_DDRAW_MODE_SPECIFIC: return "DDRAW_MODE_SPECIFIC";
   case D3DDDICAPS_GETFORMATCOUNT: return "FORMATCOUNT";
   case D3DDDICAPS_GETFORMATDATA: return "FORMATDATA";
   case D3DDDICAPS_GETMULTISAMPLEQUALITYLEVELS: return "MULTISAMPLEQUALITYLEVELS";
   case D3DDDICAPS_GETD3DQUERYCOUNT: return "QUERYCOUNT";
   case D3DDDICAPS_GETD3DQUERYDATA: return "QUERYDATA";
   case D3DDDICAPS_GETD3D3CAPS: return "D3D3CAPS";
   case D3DDDICAPS_GETD3D5CAPS: return "D3D5CAPS";
   case D3DDDICAPS_GETD3D6CAPS: return "D3D6CAPS";
   case D3DDDICAPS_GETD3D7CAPS: return "D3D7CAPS";
   case D3DDDICAPS_GETD3D8CAPS: return "D3D8CAPS";
   case D3DDDICAPS_GETD3D9CAPS: return "D3D9CAPS";
   case D3DDDICAPS_GETGAMMARAMPCAPS: return "GAMMARAMPCAPS";
   default: return "?";
   }
}

static HRESULT APIENTRY
d3d9_get_caps(HANDLE adapter, const D3DDDIARG_GETCAPS *arg)
{
   HRESULT result = S_OK;

   (void)adapter;
   switch (arg->Type) {
   case D3DDDICAPS_DDRAW: {
      DDRAW_CAPS *caps = arg->pData;

      if (arg->DataSize < sizeof *caps)
         return E_INVALIDARG;
      memset(caps, 0, sizeof *caps);
      caps->Caps2 = DDRAW_CAPS2_FLIPINTERVAL | DDRAW_CAPS2_FLIPNOVSYNC |
                    DDRAW_CAPS2_DYNAMICTEXTURES;
      break;
   }
   case D3DDDICAPS_DDRAW_MODE_SPECIFIC: {
      DDRAW_MODE_SPECIFIC_CAPS *caps = arg->pData;
      UINT head;

      if (arg->DataSize < sizeof *caps)
         return E_INVALIDARG;
      head = caps->Head;
      memset(caps, 0, sizeof *caps);
      caps->Head = head;
      break;
   }
   case D3DDDICAPS_GETFORMATCOUNT:
      if (arg->DataSize < sizeof(UINT))
         return E_INVALIDARG;
      *(UINT *)arg->pData = ARRAYSIZE(d3d9_formats);
      break;
   case D3DDDICAPS_GETFORMATDATA:
      if (arg->DataSize < sizeof d3d9_formats)
         return E_INVALIDARG;
      memcpy(arg->pData, d3d9_formats, sizeof d3d9_formats);
      break;
   case D3DDDICAPS_GETMULTISAMPLEQUALITYLEVELS: {
      DDIMULTISAMPLEQUALITYLEVELSDATA *levels = arg->pData;

      if (arg->DataSize < sizeof *levels)
         return E_INVALIDARG;
      levels->QualityLevels = levels->MsType == D3DDDIMULTISAMPLE_NONE ? 1 : 0;
      break;
   }
   case D3DDDICAPS_GETD3DQUERYCOUNT:
      if (arg->DataSize < sizeof(UINT))
         return E_INVALIDARG;
      *(UINT *)arg->pData = ARRAYSIZE(d3d9_queries);
      break;
   case D3DDDICAPS_GETD3DQUERYDATA:
      if (arg->DataSize < sizeof d3d9_queries)
         return E_INVALIDARG;
      memcpy(arg->pData, d3d9_queries, sizeof d3d9_queries);
      break;
   case D3DDDICAPS_GETD3D8CAPS:
   case D3DDDICAPS_GETD3D9CAPS: {
      /* Direct3D 8's structure is the head of Direct3D 9's. */
      D3DCAPS9 caps;

      d3d9_caps9(&caps);
      memset(arg->pData, 0, arg->DataSize);
      memcpy(arg->pData, &caps, min(arg->DataSize, sizeof caps));
      break;
   }
   case D3DDDICAPS_GETD3D3CAPS: {
      /* The DirectDraw-era description: a size, and that this is hardware. */
      D3DHAL_GLOBALDRIVERDATA *data = arg->pData;

      memset(arg->pData, 0, arg->DataSize);
      if (arg->DataSize >= sizeof *data) {
         data->dwSize = sizeof *data;
         data->hwCaps.dwSize = sizeof data->hwCaps;
         data->hwCaps.dwFlags = D3DDD_COLORMODEL | D3DDD_DEVCAPS | D3DDD_TRICAPS |
                                D3DDD_LINECAPS | D3DDD_DEVICERENDERBITDEPTH |
                                D3DDD_DEVICEZBUFFERBITDEPTH;
         data->hwCaps.dcmColorModel = D3DCOLOR_RGB;
         data->hwCaps.dwDevCaps = D3DDEVCAPS_HWRASTERIZATION | D3DDEVCAPS_DRAWPRIMITIVES2EX |
                                  D3DDEVCAPS_DRAWPRIMITIVES2 | D3DDEVCAPS_DRAWPRIMTLVERTEX |
                                  D3DDEVCAPS_EXECUTEVIDEOMEMORY | D3DDEVCAPS_TLVERTEXVIDEOMEMORY |
                                  D3DDEVCAPS_TEXTUREVIDEOMEMORY | D3DDEVCAPS_HWTRANSFORMANDLIGHT;
         data->hwCaps.dwDeviceRenderBitDepth = DDBD_16 | DDBD_32;
         data->hwCaps.dwDeviceZBufferBitDepth = DDBD_16 | DDBD_24 | DDBD_32;
         data->dwNumVertices = 0;
         data->dwNumClipVertices = 0;
      }
      break;
   }
   case D3DDDICAPS_GETD3D5CAPS:
   case D3DDDICAPS_GETD3D6CAPS:
   case D3DDDICAPS_GETD3D7CAPS:
   case D3DDDICAPS_GETGAMMARAMPCAPS:
      memset(arg->pData, 0, arg->DataSize);
      break;
   case D3DDDICAPS_GETDECODEGUIDCOUNT:
   case D3DDDICAPS_GETVIDEOPROCESSORDEVICEGUIDCOUNT:
   case D3DDDICAPS_GETEXTENSIONGUIDCOUNT:
      /* No video decoding, processing or extensions. */
      if (arg->DataSize >= sizeof(UINT))
         *(UINT *)arg->pData = 0;
      break;
   default:
      memset(arg->pData, 0, arg->DataSize);
      result = E_NOTIMPL;
      break;
   }
      d3d9_log("caps %u %s, %u bytes: 0x%08lx", (unsigned)arg->Type,
               caps_name(arg->Type), arg->DataSize, (unsigned long)result);
   return result;
}

/* --------------------------------------------------------------- device */

struct d3d9_adapter;

struct d3d9_device {
   HANDLE runtime;
   struct d3d9_adapter *adapter;
   D3DDDI_DEVICECALLBACKS callbacks;
   /* The kernel context presents are submitted on. */
   HANDLE context;
   /* Nine on a Gallium screen (d3d9_nine.c), or NULL where there is none:
    * the device then only answers, as in phase N0. */
   struct entangled_nine *nine;
};

/* What the runtime calls a resource: one or more surfaces of one kind. */
struct d3d9_resource {
   HANDLE runtime;
   D3DDDIFORMAT format;
   D3DDDI_RESOURCEFLAGS flags;
   UINT surfaces;
   UINT width, height;
   /* Nine's object for it (a surface, for a render target or depth buffer),
    * or NULL for a kind this phase does not make yet. */
   void *nine;
   /* A render target the runtime may present has an allocation of the kernel
    * driver's: a present is a copy from it, and it is what DWM is shown. */
   D3DKMT_HANDLE allocation;
   /* The frame on its way from the GPU to that allocation. */
   void *pixels;
};

/* The kernel driver's escape that takes a frame for a present to copy on (0x904;
 * ADR-0010, the second pass): type and length, the allocation, a buffer of
 * ours and its size. */
struct d3d9_escape_write {
   unsigned short type;
   unsigned short length;
   UINT handle;
   unsigned long long buffer;
   unsigned long long size;
   char rest[216];
};

static NTSTATUS
d3d9_escape(struct d3d9_device *device, void *data, UINT size);

/* An allocation the kernel driver will copy from at a present. Its private
 * data is the kernel driver's contract (viogpu_wddm_driver.h). */
static D3DKMT_HANDLE
d3d9_allocate(struct d3d9_device *device, HANDLE runtime_resource, UINT width, UINT height,
              BOOL alpha)
{
   VIOGPU_CREATE_RESOURCE_EXCHANGE resource_exchange;
   VIOGPU_CREATE_ALLOCATION_EXCHANGE allocation_exchange;
   D3DDDI_ALLOCATIONINFO info;
   D3DDDICB_ALLOCATE allocate;
   HRESULT hr;

   memset(&resource_exchange, 0, sizeof resource_exchange);
   memset(&allocation_exchange, 0, sizeof allocation_exchange);
   allocation_exchange.ResourceOptions.target = 2; /* PIPE_TEXTURE_2D */
   allocation_exchange.ResourceOptions.format = alpha ? 1 : 2; /* B8G8R8A8, B8G8R8X8 */
   allocation_exchange.ResourceOptions.width = width;
   allocation_exchange.ResourceOptions.height = height;
   allocation_exchange.ResourceOptions.depth = 1;
   allocation_exchange.ResourceOptions.array_size = 1;
   allocation_exchange.Size = (ULONGLONG)width * 4 * height;

   memset(&info, 0, sizeof info);
   info.pPrivateDriverData = &allocation_exchange;
   info.PrivateDriverDataSize = sizeof allocation_exchange;

   memset(&allocate, 0, sizeof allocate);
   allocate.pPrivateDriverData = &resource_exchange;
   allocate.PrivateDriverDataSize = sizeof resource_exchange;
   allocate.hResource = runtime_resource;
   allocate.NumAllocations = 1;
   allocate.pAllocationInfo = &info;
   hr = device->callbacks.pfnAllocateCb(device->runtime, &allocate);
   if (FAILED(hr) || !info.hAllocation) {
      d3d9_log("allocation %ux%u failed: 0x%08lx", width, height, (unsigned long)hr);
      return 0;
   }
   return info.hAllocation;
}

static HRESULT APIENTRY
d3d9_create_resource2(HANDLE handle, D3DDDIARG_CREATERESOURCE2 *arg)
{
   static volatile LONG said;
   struct d3d9_device *device = handle;
   struct d3d9_resource *resource = calloc(1, sizeof *resource);
   long result = S_OK;

   if (!resource)
      return E_OUTOFMEMORY;
   resource->runtime = arg->hResource;
   resource->format = arg->Format;
   resource->flags = arg->Flags;
   resource->surfaces = arg->SurfCount;
   if (arg->SurfCount && arg->pSurfList) {
      resource->width = arg->pSurfList[0].Width;
      resource->height = arg->pSurfList[0].Height;
   }

   if (device->nine && arg->SurfCount == 1 && !arg->Flags.Texture && !arg->Flags.CubeMap &&
       (arg->Flags.RenderTarget || arg->Flags.ZBuffer)) {
      struct entangled_nine_surface desc;

      memset(&desc, 0, sizeof desc);
      desc.width = resource->width;
      desc.height = resource->height;
      desc.format = (unsigned)arg->Format;
      desc.multisample = (unsigned)arg->MultisampleType;
      desc.quality = arg->MultisampleQuality;
      desc.depth_stencil = arg->Flags.ZBuffer;
      /* Lockable whatever the runtime says: a present reads it back. */
      desc.lockable = 1;
      resource->nine = entangled_nine_surface_create(device->nine, &desc, &result);
      if (!resource->nine) {
         d3d9_log("surface %ux%u format %u failed: 0x%08lx", resource->width, resource->height,
                  (unsigned)arg->Format, (unsigned long)result);
         free(resource);
         return (HRESULT)result;
      }
      if (arg->Flags.RenderTarget &&
          (arg->Format == D3DDDIFMT_X8R8G8B8 || arg->Format == D3DDDIFMT_A8R8G8B8)) {
         resource->allocation = d3d9_allocate(device, arg->hResource, resource->width,
                                              resource->height,
                                              arg->Format == D3DDDIFMT_A8R8G8B8);
         if (resource->allocation)
            resource->pixels = malloc((size_t)resource->width * 4 * resource->height);
      }
   }

   arg->hResource = resource;
   if (InterlockedIncrement(&said) <= 24)
      d3d9_log("resource %p: format %u, %ux%u, %u surfaces, pool %u, flags 0x%x%s%s",
               (void *)resource, (unsigned)arg->Format, resource->width, resource->height,
               arg->SurfCount, (unsigned)arg->Pool, arg->Flags.Value,
               resource->nine ? ", nine" : "", resource->allocation ? ", allocation" : "");
   return S_OK;
}

static HRESULT APIENTRY
d3d9_open_resource(HANDLE device, D3DDDIARG_OPENRESOURCE *arg)
{
   struct d3d9_resource *resource = calloc(1, sizeof *resource);

   (void)device;
   if (!resource)
      return E_OUTOFMEMORY;
   resource->runtime = arg->hResource;
   arg->hResource = resource;
   d3d9_log("resource %p: opened, %u allocations", (void *)resource, arg->NumAllocations);
   return S_OK;
}

static HRESULT APIENTRY
d3d9_destroy_resource(HANDLE handle, HANDLE resource_handle)
{
   struct d3d9_device *device = handle;
   struct d3d9_resource *resource = resource_handle;

   if (!resource)
      return S_OK;
   entangled_nine_release(resource->nine);
   if (resource->allocation) {
      D3DDDICB_DEALLOCATE deallocate;

      memset(&deallocate, 0, sizeof deallocate);
      deallocate.hResource = resource->runtime;
      device->callbacks.pfnDeallocateCb(device->runtime, &deallocate);
   }
   free(resource->pixels);
   free(resource);
   return S_OK;
}

static HRESULT APIENTRY
d3d9_set_render_target(HANDLE handle, const D3DDDIARG_SETRENDERTARGET *arg)
{
   struct d3d9_device *device = handle;
   struct d3d9_resource *resource = arg->hRenderTarget;

   if (!device->nine)
      return S_OK;
   return (HRESULT)entangled_nine_set_render_target(device->nine, arg->RenderTargetIndex,
                                                    resource ? resource->nine : NULL);
}

static HRESULT APIENTRY
d3d9_set_depth_stencil(HANDLE handle, const D3DDDIARG_SETDEPTHSTENCIL *arg)
{
   struct d3d9_device *device = handle;
   struct d3d9_resource *resource = arg->hZBuffer;

   if (!device->nine)
      return S_OK;
   return (HRESULT)entangled_nine_set_depth_stencil(device->nine,
                                                    resource ? resource->nine : NULL);
}

static HRESULT APIENTRY
d3d9_clear(HANDLE handle, const D3DDDIARG_CLEAR *arg, UINT count, const RECT *rects)
{
   struct d3d9_device *device = handle;

   if (!device->nine)
      return S_OK;
   /* RECT and D3DRECT are the same four LONGs. The low three flags are the
    * API's; the fourth (D3DCLEAR_COMPUTERECTS) is the runtime's own. */
   return (HRESULT)entangled_nine_clear(device->nine, count, (const long *)rects,
                                        arg->Flags & 7, arg->FillColor, arg->FillDepth,
                                        arg->FillStencil);
}

static HRESULT APIENTRY
d3d9_flush1(HANDLE handle, UINT flags)
{
   struct d3d9_device *device = handle;

   (void)flags;
   if (device->nine)
      entangled_nine_flush(device->nine);
   return S_OK;
}

/* A present: the frame leaves the GPU for the back buffer's allocation, and
 * the kernel driver copies it on to wherever Windows shows this window (the
 * surface DWM composes from, or the screen). Through the guest's CPU, like
 * every window under this driver so far (ADR-0010); the plane is N4's. */
static HRESULT APIENTRY
d3d9_present1(HANDLE handle, const D3DDDIARG_PRESENT1 *arg)
{
   static volatile LONG said;
   struct d3d9_device *device = handle;
   struct d3d9_resource *resource;
   struct d3d9_escape_write write;
   D3DDDICB_PRESENT present;
   long result;
   HRESULT hr;

   if (!device->nine || !arg->SrcResources || !arg->phSrcResources)
      return S_OK;
   resource = arg->phSrcResources[0].hResource;
   if (!resource || !resource->nine || !resource->allocation || !resource->pixels) {
      if (InterlockedIncrement(&said) <= 4)
         d3d9_log("present of %p: nothing to present from", (void *)resource);
      return S_OK;
   }

   result = entangled_nine_read_surface(device->nine, resource->nine, resource->pixels,
                                        resource->width, resource->height);
   if (result < 0) {
      if (InterlockedIncrement(&said) <= 4)
         d3d9_log("present: reading the frame failed 0x%08lx", (unsigned long)result);
      return (HRESULT)result;
   }
   memset(&write, 0, sizeof write);
   write.type = 0x904;
   write.length = 20;
   write.handle = resource->allocation;
   write.buffer = (unsigned long long)(uintptr_t)resource->pixels;
   write.size = (unsigned long long)resource->width * 4 * resource->height;
   {
      NTSTATUS status = d3d9_escape(device, &write, sizeof write);

      if (status < 0 && InterlockedIncrement(&said) <= 4)
         d3d9_log("present: handing the frame to the kernel driver failed 0x%08lx",
                  (unsigned long)status);
   }

   memset(&present, 0, sizeof present);
   present.hSrcAllocation = resource->allocation;
   present.hContext = device->context;
   hr = device->callbacks.pfnPresentCb(device->runtime, &present);
   if (InterlockedIncrement(&said) <= 4 || FAILED(hr))
      d3d9_log("present %ux%u: 0x%08lx", resource->width, resource->height, (unsigned long)hr);
   return hr;
}

static HRESULT APIENTRY
d3d9_destroy_device(HANDLE handle)
{
   struct d3d9_device *device = handle;

   entangled_nine_close(device->nine);
   if (device->context) {
      D3DDDICB_DESTROYCONTEXT destroy;

      destroy.hContext = device->context;
      device->callbacks.pfnDestroyContextCb(device->runtime, &destroy);
   }
   d3d9_log("device %p destroyed", handle);
   free(device);
   return S_OK;
}

static HRESULT APIENTRY
d3d9_create_device(HANDLE adapter, D3DDDIARG_CREATEDEVICE *arg)
{
   struct d3d9_device *device = calloc(1, sizeof *device);

   if (!device)
      return E_OUTOFMEMORY;
   device->runtime = arg->hDevice;
   device->adapter = adapter;
   device->callbacks = *arg->pCallbacks;
   d3d9_fill_stubs(arg->pDeviceFuncs);
   arg->pDeviceFuncs->pfnDestroyDevice = d3d9_destroy_device;
   arg->pDeviceFuncs->pfnCreateResource2 = d3d9_create_resource2;
   arg->pDeviceFuncs->pfnOpenResource = d3d9_open_resource;
   arg->pDeviceFuncs->pfnDestroyResource = d3d9_destroy_resource;
   arg->pDeviceFuncs->pfnSetRenderTarget = d3d9_set_render_target;
   arg->pDeviceFuncs->pfnSetDepthStencil = d3d9_set_depth_stencil;
   arg->pDeviceFuncs->pfnClear = d3d9_clear;
   arg->pDeviceFuncs->pfnFlush1 = d3d9_flush1;
   arg->pDeviceFuncs->pfnPresent1 = d3d9_present1;
   /* A driver of this interface version has the newer form of six functions
    * and must leave the older one NULL: the runtime fails CreateDevice
    * otherwise ("UMD did not provide non-null pfnPresent1, and null
    * pfnPresent", and so on down the list). */
   arg->pDeviceFuncs->pfnPresent = NULL;
   arg->pDeviceFuncs->pfnFlush = NULL;
   arg->pDeviceFuncs->pfnCreateResource = NULL;
   arg->pDeviceFuncs->pfnVolBlt = NULL;
   arg->pDeviceFuncs->pfnTexBlt = NULL;
   arg->pDeviceFuncs->pfnBufBlt = NULL;
   arg->hDevice = device;
   d3d9_log("device %p: interface %u, version 0x%x, flags 0x%x", (void *)device,
            arg->Interface, arg->Version, arg->Flags.Value);
   {
      D3DDDICB_CREATECONTEXT create;
      HRESULT hr;

      memset(&create, 0, sizeof create);
      hr = device->callbacks.pfnCreateContextCb(device->runtime, &create);
      if (SUCCEEDED(hr))
         device->context = create.hContext;
      else
         d3d9_log("no kernel context (0x%08lx): nothing will be presented", (unsigned long)hr);
   }
   {
      struct entangled_nine_info info;
      long result;

      device->nine = entangled_nine_open(&info, &result);
      if (device->nine)
         d3d9_log("nine: %s, vs 0x%x, ps 0x%x, textures to %u, %u render targets",
                  info.renderer, info.vertex_shader, info.pixel_shader, info.max_texture_width,
                  info.simultaneous_rts);
      else
         d3d9_log("nine: no device (0x%08lx)", (unsigned long)result);
   }
   return S_OK;
}

/* -------------------------------------------------------------- adapter */

struct d3d9_adapter {
   HANDLE runtime;
   D3DDDI_ADAPTERCALLBACKS callbacks;
};

static NTSTATUS
d3d9_escape(struct d3d9_device *device, void *data, UINT size)
{
   D3DDDICB_ESCAPE escape;

   memset(&escape, 0, sizeof escape);
   escape.hDevice = device->runtime;
   escape.pPrivateDriverData = data;
   escape.PrivateDriverDataSize = size;
   escape.hContext = device->context;
   return device->callbacks.pfnEscapeCb(device->adapter->runtime, &escape);
}

static HRESULT APIENTRY
d3d9_close_adapter(HANDLE handle)
{
   d3d9_log("adapter %p closed", handle);
   free(handle);
   return S_OK;
}

HRESULT APIENTRY
OpenAdapter(D3DDDIARG_OPENADAPTER *arg)
{
   struct d3d9_adapter *adapter;
   char exe[MAX_PATH] = "?";
   char want[8] = "";
   const char *base = exe;

   /* Until the device draws (phase N2), a program that got one would be
    * worse off than with none: its locks would return nothing. So only the
    * test program is served, and whoever sets ENTANGLED_D3D9=1 to see what
    * a real program calls; everyone else is told there is no driver, which
    * is what they were told before this DLL existed. */
   GetModuleFileNameA(NULL, exe, sizeof exe);
   for (const char *p = exe; *p; p++) {
      if (*p == '\\' || *p == '/')
         base = p + 1;
   }
   GetEnvironmentVariableA("ENTANGLED_D3D9", want, sizeof want);
   if (_strnicmp(base, "d3d9-test", 9) != 0 && strcmp(want, "1") != 0) {
      d3d9_log("not serving %.150s yet", base);
      return E_FAIL;
   }

   adapter = calloc(1, sizeof *adapter);
   if (!adapter)
      return E_OUTOFMEMORY;
   adapter->runtime = arg->hAdapter;
   adapter->callbacks = *arg->pAdapterCallbacks;
   arg->hAdapter = adapter;
   arg->pAdapterFuncs->pfnGetCaps = d3d9_get_caps;
   arg->pAdapterFuncs->pfnCreateDevice = d3d9_create_device;
   arg->pAdapterFuncs->pfnCloseAdapter = d3d9_close_adapter;
   arg->DriverVersion = D3D_UMD_INTERFACE_VERSION;

   d3d9_log("adapter %p in %.150s (%u-bit): interface %u, version 0x%x", (void *)adapter,
            exe, (unsigned)(8 * sizeof(void *)), arg->Interface, arg->Version);
   d3d9_log("answering driver version 0x%x", arg->DriverVersion);
   return S_OK;
}

BOOL WINAPI
DllMain(HINSTANCE instance, DWORD reason, void *reserved)
{
   (void)reserved;
   if (reason == DLL_PROCESS_ATTACH) {
      DisableThreadLibraryCalls(instance);
      InitializeCriticalSection(&log_lock);
      log_ready = TRUE;
      crash_module = instance;
      crash_handler = AddVectoredExceptionHandler(0, d3d9_crash);
   } else if (reason == DLL_PROCESS_DETACH) {
      /* A handler left behind would be a jump into a DLL that is gone. */
      if (crash_handler)
         RemoveVectoredExceptionHandler(crash_handler);
   }
   return TRUE;
}
