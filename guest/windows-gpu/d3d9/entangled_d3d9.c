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
 * faulting address by module, the callers unwound (x64), and every word of
 * the stack above it that points into this DLL, as offsets its linker map
 * (/MAP) turns into function names: crude, the same for x64 and x86. */
static HMODULE crash_module;
static PVOID crash_handler;

/* "module+0xoffset" for an address in the process. */
static void
d3d9_where(const void *address, char *text, size_t size)
{
   HMODULE module = NULL;
   char path[MAX_PATH];
   const char *name = "?";

   if (GetModuleHandleExA(GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS |
                             GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
                          (LPCSTR)address, &module) &&
       module && GetModuleFileNameA(module, path, sizeof path)) {
      const char *slash = strrchr(path, 92);

      name = slash ? slash + 1 : path;
   }
   _snprintf(text, size - 1, "%s+0x%Ix", name, (size_t)((ULONG_PTR)address - (ULONG_PTR)module));
   text[size - 1] = 0;
}

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

   /* A debug string (Mesa's log goes out through OutputDebugString, which
    * raises this when no debugger listens): carry the text to the log. */
   if (code == 0x40010006u && info->ExceptionRecord->NumberParameters >= 2 &&
       info->ExceptionRecord->ExceptionInformation[1]) {
      static volatile LONG lines;

      if (InterlockedIncrement(&lines) <= 300)
         d3d9_log("%.200s", (const char *)info->ExceptionRecord->ExceptionInformation[1]);
      return EXCEPTION_CONTINUE_SEARCH;
   }
   /* Errors only (not C++ throws or breakpoints). */
   if (code < 0xC0000000u || code == 0xE06D7363u)
      return EXCEPTION_CONTINUE_SEARCH;
   if (InterlockedExchange(&reported, 1) != 0)
      return EXCEPTION_CONTINUE_SEARCH;
   nt = (IMAGE_NT_HEADERS *)(base + ((IMAGE_DOS_HEADER *)base)->e_lfanew);
   top = base + nt->OptionalHeader.SizeOfImage;
   d3d9_where(info->ExceptionRecord->ExceptionAddress, line, sizeof line);
   d3d9_log("exception 0x%08lx at %s in thread %lu, access %p", (unsigned long)code, line,
            (unsigned long)GetCurrentThreadId(),
            info->ExceptionRecord->NumberParameters > 1
               ? (void *)info->ExceptionRecord->ExceptionInformation[1]
               : NULL);
#if defined(_WIN64)
   {
      /* The callers, unwound: whose call it was, in whichever module. */
      CONTEXT context = *info->ContextRecord;

      for (unsigned frame = 0; frame < 28 && context.Rip; frame++) {
         DWORD64 image = 0, establisher = 0;
         PVOID handler_data = NULL;
         PRUNTIME_FUNCTION function = RtlLookupFunctionEntry(context.Rip, &image, NULL);

         if (frame) {
            d3d9_where((const void *)context.Rip, line, sizeof line);
            d3d9_log("  from %s", line);
         }
         if (function) {
            RtlVirtualUnwind(UNW_FLAG_NHANDLER, image, context.Rip, function, &context,
                             &handler_data, &establisher, NULL);
         } else {
            if (context.Rsp < (DWORD64)info->ContextRecord->Rsp ||
                context.Rsp >= (DWORD64)((NT_TIB *)NtCurrentTeb())->StackBase)
               break;
            context.Rip = *(DWORD64 *)context.Rsp;
            context.Rsp += 8;
         }
      }
   }
#endif
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

struct d3d9_adapter {
   HANDLE runtime;
   D3DDDI_ADAPTERCALLBACKS callbacks;
};

struct d3d9_resource;

struct d3d9_device {
   HANDLE runtime;
   struct d3d9_adapter *adapter;
   D3DDDI_DEVICECALLBACKS callbacks;
   /* The kernel context presents are submitted on. */
   HANDLE context;
   /* Nine on a Gallium screen (d3d9_nine.c), or NULL where there is none:
    * the device then only answers, as in phase N0. */
   struct entangled_nine *nine;

   /* Stream 0 and the indices, when the runtime hands them over as memory of
    * the program's (SetStreamSourceUm, SetIndicesUm) and not as buffers. */
   const void *um_vertices;
   UINT stream0_stride;
   const void *um_indices;
   UINT um_index_size;
   /* The index buffer the program has set, to put back after a draw that
    * came with indices of its own. */
   struct d3d9_resource *indices;
   /* The vertex declaration the program has set. */
   struct d3d9_declaration *declaration;
   /* An index buffer of ours for such draws (DrawIndexedPrimitive2 over a
    * vertex buffer: indices in memory, vertices not). */
   void *scratch_indices;
   UINT scratch_size;
};

/* One surface of a resource the runtime keeps in system memory: the driver
 * is told where it is, and reads it when it is copied somewhere. */
struct d3d9_sysmem {
   void *memory;
   UINT pitch, slice_pitch;
   UINT width, height, depth;
};

/* What the runtime calls a resource: one or more surfaces of one kind. */
struct d3d9_resource {
   HANDLE runtime;
   D3DDDIFORMAT format;
   D3DDDI_RESOURCEFLAGS flags;
   UINT surfaces;
   UINT width, height, depth;
   /* Nine's object for it and what it is, or NULL (a resource in system
    * memory that is not a buffer has none). */
   void *nine;
   enum entangled_nine_kind kind;
   UINT levels;
   /* System memory of the runtime's, one entry a surface; NULL for a resource
    * in video memory. A buffer there has a Nine buffer as well, brought up to
    * date when the runtime says it wrote (a lock that only notifies). */
   struct d3d9_sysmem *sysmem;
   UINT notified_offset, notified_size;
   /* A render target the runtime may present has an allocation of the kernel
    * driver's: a present is a copy from it, and it is what DWM is shown. */
   D3DKMT_HANDLE allocation;
   /* The frame on its way from the GPU to that allocation. */
   void *pixels;
};

/* The kernel driver's escape that takes a frame for a present to copy on
 * (0x904; ADR-0010, the second pass): type and length, the allocation, a
 * buffer of ours and its size. */
struct d3d9_escape_write {
   unsigned short type;
   unsigned short length;
   UINT handle;
   unsigned long long buffer;
   unsigned long long size;
   char rest[216];
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

/* Bring-up: with ENTANGLED_D3D9_TRACE=1, the first few hundred calls that
 * decide what a draw is (streams, declaration, shaders, the draws). */
static volatile LONG trace_left = -1;

static BOOL
d3d9_tracing(void)
{
   if (trace_left == -1) {
      char want[4] = "";

      GetEnvironmentVariableA("ENTANGLED_D3D9_TRACE", want, sizeof want);
      InterlockedExchange(&trace_left, want[0] == '1' ? 400 : 0);
   }
   return trace_left > 0 && InterlockedDecrement(&trace_left) >= 0;
}

#define D3D9_TRACE(...)         \
   do {                         \
      if (d3d9_tracing())       \
         d3d9_log(__VA_ARGS__); \
   } while (0)

/* A failure of Nine's, said once a call site (the runtime repeats itself). */
static HRESULT
d3d9_result(const char *what, long result, volatile LONG *said)
{
   if (result < 0 && InterlockedIncrement(said) <= 3)
      d3d9_log("%s failed: 0x%08lx", what, (unsigned long)result);
   return (HRESULT)result;
}

#define D3D9_CHECK(what, call)                                 \
   do {                                                        \
      static volatile LONG said_;                              \
      return d3d9_result(what, (call), &said_);                \
   } while (0)

/* ------------------------------------------------------------ resources */

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

static void
d3d9_resource_free(struct d3d9_device *device, struct d3d9_resource *resource)
{
   entangled_nine_release(resource->nine);
   if (resource->allocation) {
      D3DDDICB_DEALLOCATE deallocate;

      memset(&deallocate, 0, sizeof deallocate);
      deallocate.hResource = resource->runtime;
      device->callbacks.pfnDeallocateCb(device->runtime, &deallocate);
   }
   free(resource->sysmem);
   free(resource->pixels);
   free(resource);
}

static HRESULT APIENTRY
d3d9_create_resource2(HANDLE handle, D3DDDIARG_CREATERESOURCE2 *arg)
{
   static volatile LONG said;
   struct d3d9_device *device = handle;
   struct d3d9_resource *resource = calloc(1, sizeof *resource);
   struct entangled_nine_resource desc;
   BOOL buffer = arg->Flags.VertexBuffer || arg->Flags.IndexBuffer;
   BOOL system = arg->Pool == D3DDDIPOOL_SYSTEMMEM && arg->SurfCount && arg->pSurfList &&
                 arg->pSurfList[0].pSysMem;
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
      resource->depth = arg->pSurfList[0].Depth;
   }
   if (system) {
      resource->sysmem = calloc(arg->SurfCount, sizeof *resource->sysmem);
      if (!resource->sysmem) {
         free(resource);
         return E_OUTOFMEMORY;
      }
      for (UINT i = 0; i < arg->SurfCount; i++) {
         resource->sysmem[i].memory = (void *)arg->pSurfList[i].pSysMem;
         resource->sysmem[i].pitch = arg->pSurfList[i].SysMemPitch;
         resource->sysmem[i].slice_pitch = arg->pSurfList[i].SysMemSlicePitch;
         resource->sysmem[i].width = arg->pSurfList[i].Width;
         resource->sysmem[i].height = arg->pSurfList[i].Height;
         resource->sysmem[i].depth = arg->pSurfList[i].Depth;
      }
   }

   memset(&desc, 0, sizeof desc);
   desc.width = resource->width;
   desc.height = resource->height;
   desc.depth = resource->depth;
   desc.format = (unsigned)arg->Format;
   desc.multisample = (unsigned)arg->MultisampleType;
   desc.quality = arg->MultisampleQuality;
   desc.fvf = arg->Fvf;
   desc.levels = arg->SurfCount;
   if (arg->Flags.RenderTarget)
      desc.usage |= 0x1; /* D3DUSAGE_RENDERTARGET */
   if (arg->Flags.ZBuffer)
      desc.usage |= 0x2; /* D3DUSAGE_DEPTHSTENCIL */
   if (arg->Flags.WriteOnly && buffer)
      desc.usage |= 0x8; /* D3DUSAGE_WRITEONLY */
   if (arg->Flags.Dynamic)
      desc.usage |= 0x200; /* D3DUSAGE_DYNAMIC */
   if (arg->Flags.AutogenMipmap)
      desc.usage |= 0x400; /* D3DUSAGE_AUTOGENMIPMAP */
   if (arg->Flags.VertexBuffer) {
      desc.kind = ENTANGLED_NINE_VERTEX_BUFFER;
   } else if (arg->Flags.IndexBuffer) {
      desc.kind = ENTANGLED_NINE_INDEX_BUFFER;
   } else if (arg->Flags.Volume) {
      desc.kind = ENTANGLED_NINE_VOLUME;
   } else if (arg->Flags.CubeMap) {
      desc.kind = ENTANGLED_NINE_CUBE;
      desc.levels = arg->SurfCount / 6;
   } else if (arg->Flags.Texture) {
      desc.kind = ENTANGLED_NINE_TEXTURE;
   } else {
      desc.kind = ENTANGLED_NINE_SURFACE;
      desc.depth_stencil = arg->Flags.ZBuffer;
      desc.render_target = arg->Flags.RenderTarget;
   }
   resource->kind = desc.kind;
   resource->levels = desc.levels;

   /* In video memory, or a buffer wherever it is (a draw needs one of
    * Nine's): an object of Nine's. System-memory textures and surfaces stay
    * the runtime's memory, read when they are copied from. */
   if (device->nine && (!system || buffer) && arg->SurfCount) {
      resource->nine = entangled_nine_resource_create(device->nine, &desc, &result);
      if (!resource->nine) {
         d3d9_log("resource kind %d format %u %ux%u x%u failed: 0x%08lx", (int)desc.kind,
                  (unsigned)arg->Format, resource->width, resource->height, arg->SurfCount,
                  (unsigned long)result);
         free(resource->sysmem);
         free(resource);
         return (HRESULT)result;
      }
      if (desc.kind == ENTANGLED_NINE_SURFACE && arg->Flags.RenderTarget &&
          (arg->Format == D3DDDIFMT_X8R8G8B8 || arg->Format == D3DDDIFMT_A8R8G8B8)) {
         resource->allocation = d3d9_allocate(device, arg->hResource, resource->width,
                                              resource->height,
                                              arg->Format == D3DDDIFMT_A8R8G8B8);
         if (resource->allocation)
            resource->pixels = malloc((size_t)resource->width * 4 * resource->height);
      }
   }

   arg->hResource = resource;
   if (InterlockedIncrement(&said) <= 40)
      d3d9_log("resource %p: format %u, %ux%u, %u surfaces, pool %u, flags 0x%x%s%s%s",
               (void *)resource, (unsigned)arg->Format, resource->width, resource->height,
               arg->SurfCount, (unsigned)arg->Pool, arg->Flags.Value,
               resource->nine ? ", nine" : "", resource->sysmem ? ", sysmem" : "",
               resource->allocation ? ", allocation" : "");
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
   if (device->indices == resource)
      device->indices = NULL;
   d3d9_resource_free(device, resource);
   return S_OK;
}

static HRESULT APIENTRY
d3d9_lock(HANDLE handle, D3DDDIARG_LOCK *arg)
{
   struct d3d9_device *device = handle;
   struct d3d9_resource *resource = arg->hResource;
   unsigned box[6] = {0, 0, 0, 0, 0, 1}, flags = 0, pitch = 0, slice_pitch = 0;
   BOOL whole = TRUE;
   void *data = NULL;
   long result;

   if (!resource)
      return E_INVALIDARG;
   D3D9_TRACE("lock %p%s sub %u flags 0x%x range %u+%u given %p", (void *)resource,
              resource->sysmem ? " (sysmem)" : "", arg->SubResourceIndex, arg->Flags.Value,
              arg->Range.Offset, arg->Range.Size, arg->pSurfData);
   if (arg->Flags.RangeValid) {
      box[0] = arg->Range.Offset;
      box[2] = arg->Range.Offset + arg->Range.Size;
      whole = FALSE;
   } else if (arg->Flags.AreaValid) {
      box[0] = arg->Area.left;
      box[1] = arg->Area.top;
      box[2] = arg->Area.right;
      box[3] = arg->Area.bottom;
      whole = FALSE;
   } else if (arg->Flags.BoxValid) {
      box[0] = arg->Box.Left;
      box[1] = arg->Box.Top;
      box[2] = arg->Box.Right;
      box[3] = arg->Box.Bottom;
      box[4] = arg->Box.Front;
      box[5] = arg->Box.Back;
      whole = FALSE;
   }
   /* System memory: the runtime has the pointer and only says it will write.
    * Remembered for a buffer, whose copy of Nine's follows at the unlock. */
   if (resource->sysmem || arg->Flags.NotifyOnly) {
      resource->notified_offset = whole ? 0 : box[0];
      resource->notified_size = whole ? resource->width : box[2] - box[0];
      return S_OK;
   }
   if (!device->nine || !resource->nine)
      return S_OK;
   if (arg->Flags.ReadOnly)
      flags |= 0x10; /* D3DLOCK_READONLY */
   if (arg->Flags.NoOverwrite)
      flags |= 0x1000; /* D3DLOCK_NOOVERWRITE */
   if (arg->Flags.Discard)
      flags |= 0x2000; /* D3DLOCK_DISCARD */
   result = entangled_nine_lock(device->nine, resource->nine, resource->kind, resource->levels,
                                arg->SubResourceIndex, whole ? NULL : box, flags, &data, &pitch,
                                &slice_pitch);
   if (result >= 0) {
      arg->pSurfData = data;
      arg->Pitch = pitch;
      arg->SlicePitch = slice_pitch;
   }
   D3D9_CHECK("lock", result);
}

static HRESULT APIENTRY
d3d9_unlock(HANDLE handle, const D3DDDIARG_UNLOCK *arg)
{
   struct d3d9_device *device = handle;
   struct d3d9_resource *resource = arg->hResource;

   if (!resource || !device->nine || !resource->nine)
      return S_OK;
   if (resource->sysmem) {
      /* A buffer in system memory: what the runtime wrote, into Nine's. */
      unsigned box[6] = {0, 0, 0, 0, 0, 1}, pitch, slice_pitch;
      UINT offset = min(resource->notified_offset, resource->width);
      UINT size = min(resource->notified_size, resource->width - offset);
      void *data = NULL;
      long result;

      if (!size)
         return S_OK;
      box[0] = offset;
      box[2] = offset + size;
      result = entangled_nine_lock(device->nine, resource->nine, resource->kind, 1, 0, box, 0,
                                   &data, &pitch, &slice_pitch);
      if (result >= 0 && data) {
         memcpy(data, (const char *)resource->sysmem[0].memory + offset, size);
         result = entangled_nine_unlock(device->nine, resource->nine, resource->kind, 1, 0);
      }
      D3D9_CHECK("buffer update", result);
   }
   if (arg->Flags.NotifyOnly)
      return S_OK;
   D3D9_CHECK("unlock", entangled_nine_unlock(device->nine, resource->nine, resource->kind,
                                              resource->levels, arg->SubResourceIndex));
}

/* The lock the runtime takes for a buffer it will not wait for (a dynamic
 * buffer locked with discard or no-overwrite): the same lock here. Nine
 * gives a discarded buffer new storage itself, so the "rename" the runtime
 * follows a discard with has nothing left to do. */
static HRESULT APIENTRY
d3d9_lock_async(HANDLE handle, D3DDDIARG_LOCKASYNC *arg)
{
   D3DDDIARG_LOCK lock;
   HRESULT hr;

   memset(&lock, 0, sizeof lock);
   lock.hResource = arg->hResource;
   lock.SubResourceIndex = arg->SubResourceIndex;
   lock.Flags.NoOverwrite = arg->Flags.NoOverwrite;
   lock.Flags.Discard = arg->Flags.Discard;
   lock.Flags.RangeValid = arg->Flags.RangeValid;
   lock.Flags.AreaValid = arg->Flags.AreaValid;
   lock.Flags.BoxValid = arg->Flags.BoxValid;
   lock.Flags.NotifyOnly = arg->Flags.NotifyOnly;
   lock.Box = arg->Box;
   hr = d3d9_lock(handle, &lock);
   arg->pSurfData = lock.pSurfData;
   arg->Pitch = lock.Pitch;
   arg->SlicePitch = lock.SlicePitch;
   return hr;
}

static HRESULT APIENTRY
d3d9_unlock_async(HANDLE handle, const D3DDDIARG_UNLOCKASYNC *arg)
{
   D3DDDIARG_UNLOCK unlock;

   memset(&unlock, 0, sizeof unlock);
   unlock.hResource = arg->hResource;
   unlock.SubResourceIndex = arg->SubResourceIndex;
   unlock.Flags.NotifyOnly = arg->Flags.NotifyOnly;
   return d3d9_unlock(handle, &unlock);
}

static HRESULT APIENTRY
d3d9_rename(HANDLE handle, const D3DDDIARG_RENAME *arg)
{
   (void)handle;
   (void)arg;
   return S_OK;
}

/* A dimension of a mip level. */
static UINT
d3d9_level(UINT size, UINT level)
{
   size >>= level;
   return size ? size : 1;
}

/* UpdateTexture and UpdateSurface: a texture in system memory onto one in
 * video memory, the rectangle the runtime names and its image in every
 * level below. */
static HRESULT APIENTRY
d3d9_tex_blt1(HANDLE handle, const D3DDDIARG_TEXBLT1 *arg)
{
   static volatile LONG said;
   struct d3d9_device *device = handle;
   struct d3d9_resource *dst = arg->hDstResource, *src = arg->hSrcResource;
   UINT levels, face = arg->CubeMapFace;
   long result = S_OK;

   if (!device->nine || !dst || !src || !dst->nine)
      return S_OK;
   if (!src->sysmem) {
      if (InterlockedIncrement(&said) <= 3)
         d3d9_log("TexBlt from video memory: not done");
      return S_OK;
   }
   levels = min(max(dst->levels, 1), max(src->levels, 1));
   for (UINT level = 0; level < levels && result >= 0; level++) {
      UINT src_index = src->kind == ENTANGLED_NINE_CUBE ? face * max(src->levels, 1) + level
                                                        : level;
      UINT dst_index = dst->kind == ENTANGLED_NINE_CUBE ? face * max(dst->levels, 1) + level
                                                        : level;
      const struct d3d9_sysmem *from;
      long rect[4];
      UINT x, y;

      if (src_index >= src->surfaces)
         break;
      from = &src->sysmem[src_index];
      /* The rectangle at this level, grown to whole 4x4 blocks so that a
       * compressed format copies whole blocks. */
      rect[0] = (arg->SrcRect.left >> level) & ~3;
      rect[1] = (arg->SrcRect.top >> level) & ~3;
      rect[2] = min((LONG)from->width, (((arg->SrcRect.right - 1) >> level) + 4) & ~3);
      rect[3] = min((LONG)from->height, (((arg->SrcRect.bottom - 1) >> level) + 4) & ~3);
      x = (arg->DstPoint.x >> level) & ~3;
      y = (arg->DstPoint.y >> level) & ~3;
      if (rect[2] <= rect[0] || rect[3] <= rect[1])
         continue;
      if (x + (rect[2] - rect[0]) > d3d9_level(dst->width, level) ||
          y + (rect[3] - rect[1]) > d3d9_level(dst->height, level))
         continue;
      result = entangled_nine_upload(device->nine, dst->nine, dst->kind, dst->levels, dst_index,
                                     x, y, from->memory, from->pitch, rect);
   }
   D3D9_CHECK("texture update", result);
}

/* The same for buffers. */
static HRESULT APIENTRY
d3d9_buf_blt1(HANDLE handle, const D3DDDIARG_BUFFERBLT1 *arg)
{
   struct d3d9_device *device = handle;
   struct d3d9_resource *dst = arg->hDstResource, *src = arg->hSrcResource;
   unsigned box[6] = {0, 0, 0, 0, 0, 1}, pitch, slice_pitch;
   void *data = NULL;
   long result;

   if (!device->nine || !dst || !src || !dst->nine || !src->sysmem)
      return S_OK;
   box[0] = arg->Offset;
   box[2] = arg->Offset + arg->SrcRange.Size;
   result = entangled_nine_lock(device->nine, dst->nine, dst->kind, 1, 0, box, 0, &data, &pitch,
                                &slice_pitch);
   if (result >= 0 && data) {
      memcpy(data, (const char *)src->sysmem[0].memory + arg->SrcRange.Offset,
             arg->SrcRange.Size);
      result = entangled_nine_unlock(device->nine, dst->nine, dst->kind, 1, 0);
   }
   D3D9_CHECK("buffer copy", result);
}

static HRESULT APIENTRY
d3d9_blt(HANDLE handle, const D3DDDIARG_BLT *arg)
{
   static volatile LONG said;
   struct d3d9_device *device = handle;
   struct d3d9_resource *dst = arg->hDstResource, *src = arg->hSrcResource;

   if (!device->nine || !dst || !src)
      return S_OK;
   if (!dst->nine || !src->nine) {
      if (InterlockedIncrement(&said) <= 3)
         d3d9_log("Blt with system memory (%s): not done", src->nine ? "to it" : "from it");
      return S_OK;
   }
   D3D9_CHECK("blt", entangled_nine_blit(device->nine, src->nine, src->kind, src->levels,
                                         arg->SrcSubResourceIndex, (const long *)&arg->SrcRect,
                                         dst->nine, dst->kind, dst->levels,
                                         arg->DstSubResourceIndex, (const long *)&arg->DstRect,
                                         arg->Flags.Linear ? 2 : 1));
}

static HRESULT APIENTRY
d3d9_color_fill(HANDLE handle, const D3DDDIARG_COLORFILL *arg)
{
   struct d3d9_device *device = handle;
   struct d3d9_resource *resource = arg->hResource;

   if (!device->nine || !resource || !resource->nine)
      return S_OK;
   D3D9_CHECK("color fill", entangled_nine_fill(device->nine, resource->nine, resource->kind,
                                                resource->levels, arg->SubResourceIndex,
                                                (const long *)&arg->DstRect, arg->Color));
}

static HRESULT APIENTRY
d3d9_generate_mips(HANDLE handle, const D3DDDIARG_GENERATEMIPSUBLEVELS *arg)
{
   struct d3d9_device *device = handle;
   struct d3d9_resource *resource = arg->hResource;

   if (!device->nine || !resource || !resource->nine ||
       resource->kind == ENTANGLED_NINE_SURFACE)
      return S_OK;
   return (HRESULT)entangled_nine_generate_mips(device->nine, resource->nine);
}

/* ---------------------------------------------------------------- state */

static HRESULT APIENTRY
d3d9_set_render_target(HANDLE handle, const D3DDDIARG_SETRENDERTARGET *arg)
{
   struct d3d9_device *device = handle;
   struct d3d9_resource *resource = arg->hRenderTarget;

   if (!device->nine)
      return S_OK;
   D3D9_CHECK("render target",
              entangled_nine_set_target(device->nine, arg->RenderTargetIndex,
                                        resource ? resource->nine : NULL,
                                        resource ? resource->kind : ENTANGLED_NINE_SURFACE,
                                        resource ? resource->levels : 0, arg->SubResourceIndex));
}

static HRESULT APIENTRY
d3d9_set_depth_stencil(HANDLE handle, const D3DDDIARG_SETDEPTHSTENCIL *arg)
{
   struct d3d9_device *device = handle;
   struct d3d9_resource *resource = arg->hZBuffer;

   if (!device->nine)
      return S_OK;
   D3D9_CHECK("depth buffer",
              entangled_nine_set_depth(device->nine, resource ? resource->nine : NULL,
                                       resource ? resource->kind : ENTANGLED_NINE_SURFACE,
                                       resource ? resource->levels : 0, 0));
}

static HRESULT APIENTRY
d3d9_clear(HANDLE handle, const D3DDDIARG_CLEAR *arg, UINT count, const RECT *rects)
{
   struct d3d9_device *device = handle;

   if (!device->nine)
      return S_OK;
   /* RECT and D3DRECT are the same four LONGs. The low three flags are the
    * API's; the fourth (D3DCLEAR_COMPUTERECTS) is the runtime's own. */
   D3D9_CHECK("clear", entangled_nine_clear(device->nine, count, (const long *)rects,
                                            arg->Flags & 7, arg->FillColor, arg->FillDepth,
                                            arg->FillStencil));
}

static HRESULT APIENTRY
d3d9_set_render_state(HANDLE handle, const D3DDDIARG_RENDERSTATE *arg)
{
   struct d3d9_device *device = handle;

   /* Nine refuses the states it does not know; the runtime sends some of its
    * own, and a refusal here would fail the program's call. */
   if (device->nine)
      entangled_nine_render_state(device->nine, (unsigned)arg->State, arg->Value);
   return S_OK;
}

/* The DDI keeps the numbering of Direct3D 7 and 8, where what became sampler
 * states were texture stage states. */
static HRESULT APIENTRY
d3d9_set_texture_stage_state(HANDLE handle, const D3DDDIARG_TEXTURESTAGESTATE *arg)
{
   struct d3d9_device *device = handle;
   unsigned sampler = 0;

   if (!device->nine)
      return S_OK;
   switch ((unsigned)arg->State) {
   case 13: sampler = 1; break;  /* ADDRESSU */
   case 14: sampler = 2; break;  /* ADDRESSV */
   case 25: sampler = 3; break;  /* ADDRESSW */
   case 15: sampler = 4; break;  /* BORDERCOLOR */
   case 16: sampler = 5; break;  /* MAGFILTER */
   case 17: sampler = 6; break;  /* MINFILTER */
   case 18: sampler = 7; break;  /* MIPFILTER */
   case 19: sampler = 8; break;  /* MIPMAPLODBIAS */
   case 20: sampler = 9; break;  /* MAXMIPLEVEL */
   case 21: sampler = 10; break; /* MAXANISOTROPY */
   case 29: sampler = 11; break; /* SRGBTEXTURE */
   case 30: sampler = 12; break; /* ELEMENTINDEX */
   case 31: sampler = 13; break; /* DMAPOFFSET */
   default: break;
   }
   if (sampler)
      entangled_nine_sampler_state(device->nine, arg->Stage, sampler, arg->Value);
   else if ((unsigned)arg->State >= 1 && (unsigned)arg->State <= 32)
      entangled_nine_texture_stage_state(device->nine, arg->Stage, (unsigned)arg->State,
                                         arg->Value);
   return S_OK;
}

static HRESULT APIENTRY
d3d9_set_texture(HANDLE handle, UINT stage, HANDLE texture)
{
   struct d3d9_device *device = handle;
   struct d3d9_resource *resource = texture;

   if (!device->nine)
      return S_OK;
   D3D9_CHECK("texture", entangled_nine_texture(device->nine, stage,
                                                resource && resource->kind != ENTANGLED_NINE_SURFACE
                                                   ? resource->nine
                                                   : NULL));
}

static HRESULT APIENTRY
d3d9_set_viewport(HANDLE handle, const D3DDDIARG_VIEWPORTINFO *arg)
{
   struct d3d9_device *device = handle;
   unsigned rect[4] = {arg->X, arg->Y, arg->Width, arg->Height};

   D3D9_TRACE("viewport %u,%u %ux%u", arg->X, arg->Y, arg->Width, arg->Height);
   if (!device->nine)
      return S_OK;
   D3D9_CHECK("viewport", entangled_nine_viewport(device->nine, rect, NULL));
}

static HRESULT APIENTRY
d3d9_set_z_range(HANDLE handle, const D3DDDIARG_ZRANGE *arg)
{
   struct d3d9_device *device = handle;
   float range[2] = {arg->MinZ, arg->MaxZ};

   D3D9_TRACE("depth range %g to %g", arg->MinZ, arg->MaxZ);
   if (!device->nine)
      return S_OK;
   D3D9_CHECK("depth range", entangled_nine_viewport(device->nine, NULL, range));
}

static HRESULT APIENTRY
d3d9_set_scissor_rect(HANDLE handle, const RECT *rect)
{
   struct d3d9_device *device = handle;

   if (!device->nine)
      return S_OK;
   D3D9_CHECK("scissor", entangled_nine_scissor(device->nine, (const long *)rect));
}

static HRESULT APIENTRY
d3d9_set_clip_plane(HANDLE handle, const D3DDDIARG_SETCLIPPLANE *arg)
{
   struct d3d9_device *device = handle;

   if (!device->nine)
      return S_OK;
   D3D9_CHECK("clip plane", entangled_nine_clip_plane(device->nine, arg->Index, arg->Plane));
}

static HRESULT APIENTRY
d3d9_set_transform(HANDLE handle, const D3DDDIARG_SETTRANSFORM *arg)
{
   struct d3d9_device *device = handle;

   if (!device->nine)
      return S_OK;
   D3D9_CHECK("transform", entangled_nine_transform(device->nine, (unsigned)arg->TransformType,
                                                    (const float *)&arg->Matrix, 0));
}

static HRESULT APIENTRY
d3d9_multiply_transform(HANDLE handle, const D3DDDIARG_MULTIPLYTRANSFORM *arg)
{
   struct d3d9_device *device = handle;

   if (!device->nine)
      return S_OK;
   D3D9_CHECK("transform", entangled_nine_transform(device->nine, (unsigned)arg->TransformType,
                                                    (const float *)&arg->Matrix, 1));
}

static HRESULT APIENTRY
d3d9_set_material(HANDLE handle, const D3DDDIARG_SETMATERIAL *arg)
{
   struct d3d9_device *device = handle;

   if (!device->nine)
      return S_OK;
   /* The same seventeen floats as a D3DMATERIAL9. */
   D3D9_CHECK("material", entangled_nine_material(device->nine, (const float *)arg));
}

static HRESULT APIENTRY
d3d9_set_light(HANDLE handle, const D3DDDIARG_SETLIGHT *arg, const D3DDDI_LIGHT *light)
{
   struct d3d9_device *device = handle;

   if (!device->nine)
      return S_OK;
   switch (arg->DataType) {
   case D3DDDI_SETLIGHT_ENABLE:
      D3D9_CHECK("light", entangled_nine_light(device->nine, arg->Index, NULL, 1));
   case D3DDDI_SETLIGHT_DISABLE:
      D3D9_CHECK("light", entangled_nine_light(device->nine, arg->Index, NULL, 0));
   default:
      /* D3DDDI_LIGHT is laid out as D3DLIGHT9. */
      D3D9_CHECK("light", entangled_nine_light(device->nine, arg->Index, light, -1));
   }
}

static HRESULT APIENTRY
d3d9_destroy_light(HANDLE handle, const D3DDDIARG_DESTROYLIGHT *arg)
{
   struct d3d9_device *device = handle;

   if (device->nine)
      entangled_nine_light(device->nine, arg->Index, NULL, 0);
   return S_OK;
}

/* -------------------------------------------------------------- shaders */

/* Bring-up: with ENTANGLED_D3D9_SHADERS=1, the bytecode of every shader the
 * runtime hands over, eight tokens a line (the runtime makes shaders of its
 * own for the fixed-function pipeline, and they are not in any file). */
static void
d3d9_dump_shader(const char *what, const UINT *code, UINT bytes)
{
   char want[4] = "";
   UINT count = bytes / 4;

   GetEnvironmentVariableA("ENTANGLED_D3D9_SHADERS", want, sizeof want);
   if (want[0] != '1')
      return;
   d3d9_log("%s: %u tokens", what, count);
   for (UINT i = 0; i < count && i < 512; i += 8) {
      char line[160];
      int at = 0;

      for (UINT j = i; j < i + 8 && j < count; j++)
         at += _snprintf(line + at, sizeof line - 1 - at, " %08x", code[j]);
      line[at] = 0;
      d3d9_log(" %s", line);
   }
}

/* A vertex declaration. What a vertex is comes from here alone: the runtime
 * gives a program's XYZRHW vertices the usage POSITIONT itself, and
 * DrawPrimitive2 is only "from the runtime's own buffer", not "transformed". */
struct d3d9_declaration {
   void *nine;
};

static HRESULT APIENTRY
d3d9_create_vertex_shader_decl(HANDLE handle, D3DDDIARG_CREATEVERTEXSHADERDECL *arg,
                               const D3DDDIVERTEXELEMENT *elements)
{
   struct d3d9_device *device = handle;
   struct d3d9_declaration *declaration;
   long result = S_OK;

   arg->ShaderHandle = NULL;
   if (!device->nine)
      return S_OK;
   declaration = calloc(1, sizeof *declaration);
   if (!declaration)
      return E_OUTOFMEMORY;
   for (UINT i = 0; i < arg->NumVertexElements; i++)
      D3D9_TRACE("declaration element %u: stream %u offset %u type %u method %u usage %u.%u", i,
                 elements[i].Stream, elements[i].Offset, elements[i].Type, elements[i].Method,
                 elements[i].Usage, elements[i].UsageIndex);
   declaration->nine = entangled_nine_declaration_create(device->nine, elements,
                                                         arg->NumVertexElements, &result);
   if (!declaration->nine) {
      free(declaration);
      D3D9_CHECK("vertex declaration", result);
   }
   arg->ShaderHandle = declaration;
   return S_OK;
}

static HRESULT APIENTRY
d3d9_set_vertex_shader_decl(HANDLE handle, HANDLE declaration_handle)
{
   struct d3d9_device *device = handle;
   struct d3d9_declaration *declaration = declaration_handle;

   D3D9_TRACE("declaration %p", declaration_handle);
   device->declaration = declaration;
   if (!device->nine)
      return S_OK;
   D3D9_CHECK("vertex declaration",
              entangled_nine_declaration(device->nine, declaration ? declaration->nine : NULL));
}

static HRESULT APIENTRY
d3d9_delete_vertex_shader_decl(HANDLE handle, HANDLE declaration_handle)
{
   struct d3d9_device *device = handle;
   struct d3d9_declaration *declaration = declaration_handle;

   if (!declaration)
      return S_OK;
   if (device->declaration == declaration)
      device->declaration = NULL;
   entangled_nine_release(declaration->nine);
   free(declaration);
   return S_OK;
}

static HRESULT APIENTRY
d3d9_create_vertex_shader_func(HANDLE handle, D3DDDIARG_CREATEVERTEXSHADERFUNC *arg,
                               const UINT *code)
{
   struct d3d9_device *device = handle;
   long result = S_OK;

   arg->ShaderHandle = NULL;
   if (!device->nine)
      return S_OK;
   d3d9_dump_shader("vertex shader", code, arg->Size);
   arg->ShaderHandle = entangled_nine_vertex_shader_create(device->nine, code, &result);
   D3D9_CHECK("vertex shader", result);
}

static HRESULT APIENTRY
d3d9_set_vertex_shader_func(HANDLE handle, HANDLE shader)
{
   struct d3d9_device *device = handle;

   D3D9_TRACE("vertex shader %p", shader);
   if (!device->nine)
      return S_OK;
   D3D9_CHECK("vertex shader", entangled_nine_vertex_shader(device->nine, shader));
}

static HRESULT APIENTRY
d3d9_create_pixel_shader(HANDLE handle, D3DDDIARG_CREATEPIXELSHADER *arg, const UINT *code)
{
   struct d3d9_device *device = handle;
   long result = S_OK;

   arg->ShaderHandle = NULL;
   if (!device->nine)
      return S_OK;
   d3d9_dump_shader("pixel shader", code, arg->CodeSize);
   arg->ShaderHandle = entangled_nine_pixel_shader_create(device->nine, code, &result);
   D3D9_CHECK("pixel shader", result);
}

static HRESULT APIENTRY
d3d9_set_pixel_shader(HANDLE handle, HANDLE shader)
{
   struct d3d9_device *device = handle;

   D3D9_TRACE("pixel shader %p", shader);
   if (!device->nine)
      return S_OK;
   D3D9_CHECK("pixel shader", entangled_nine_pixel_shader(device->nine, shader));
}

/* A declaration or a shader going away. */
static HRESULT APIENTRY
d3d9_delete_object(HANDLE handle, HANDLE object)
{
   (void)handle;
   entangled_nine_release(object);
   return S_OK;
}

static HRESULT APIENTRY
d3d9_set_vertex_shader_const(HANDLE handle, const D3DDDIARG_SETVERTEXSHADERCONST *arg,
                             const void *data)
{
   struct d3d9_device *device = handle;

   if (!device->nine)
      return S_OK;
   D3D9_CHECK("constants", entangled_nine_vertex_constants(device->nine, 'f', arg->Register,
                                                           data, arg->Count));
}

static HRESULT APIENTRY
d3d9_set_vertex_shader_const_i(HANDLE handle, const D3DDDIARG_SETVERTEXSHADERCONSTI *arg,
                               const INT *data)
{
   struct d3d9_device *device = handle;

   if (!device->nine)
      return S_OK;
   D3D9_CHECK("constants", entangled_nine_vertex_constants(device->nine, 'i', arg->Register,
                                                           data, arg->Count));
}

static HRESULT APIENTRY
d3d9_set_vertex_shader_const_b(HANDLE handle, const D3DDDIARG_SETVERTEXSHADERCONSTB *arg,
                               const BOOL *data)
{
   struct d3d9_device *device = handle;

   if (!device->nine)
      return S_OK;
   D3D9_CHECK("constants", entangled_nine_vertex_constants(device->nine, 'b', arg->Register,
                                                           data, arg->Count));
}

static HRESULT APIENTRY
d3d9_set_pixel_shader_const(HANDLE handle, const D3DDDIARG_SETPIXELSHADERCONST *arg,
                            const FLOAT *data)
{
   struct d3d9_device *device = handle;

   if (!device->nine)
      return S_OK;
   D3D9_CHECK("constants", entangled_nine_pixel_constants(device->nine, 'f', arg->Register,
                                                          data, arg->Count));
}

static HRESULT APIENTRY
d3d9_set_pixel_shader_const_i(HANDLE handle, const D3DDDIARG_SETPIXELSHADERCONSTI *arg,
                              const INT *data)
{
   struct d3d9_device *device = handle;

   if (!device->nine)
      return S_OK;
   D3D9_CHECK("constants", entangled_nine_pixel_constants(device->nine, 'i', arg->Register,
                                                          data, arg->Count));
}

static HRESULT APIENTRY
d3d9_set_pixel_shader_const_b(HANDLE handle, const D3DDDIARG_SETPIXELSHADERCONSTB *arg,
                              const BOOL *data)
{
   struct d3d9_device *device = handle;

   if (!device->nine)
      return S_OK;
   D3D9_CHECK("constants", entangled_nine_pixel_constants(device->nine, 'b', arg->Register,
                                                          data, arg->Count));
}

/* -------------------------------------------------------------- drawing */

static HRESULT APIENTRY
d3d9_set_stream_source(HANDLE handle, const D3DDDIARG_SETSTREAMSOURCE *arg)
{
   struct d3d9_device *device = handle;
   struct d3d9_resource *resource = arg->hVertexBuffer;

   D3D9_TRACE("stream %u: buffer %p%s offset %u stride %u", arg->Stream, (void *)resource,
              resource && resource->sysmem ? " (sysmem)" : "", arg->Offset, arg->Stride);
   if (arg->Stream == 0) {
      device->um_vertices = NULL;
      device->stream0_stride = arg->Stride;
      /* A dynamic buffer in system memory is the runtime's own scratch for
       * vertices it processed or was handed (Draw...UP): it writes there
       * without a word, so the vertices are read at the draw, as memory of
       * the program's is. */
      if (resource && resource->sysmem && resource->flags.Dynamic) {
         device->um_vertices = (const char *)resource->sysmem[0].memory + arg->Offset;
         return S_OK;
      }
   }
   if (!device->nine)
      return S_OK;
   D3D9_CHECK("stream", entangled_nine_stream(device->nine, arg->Stream,
                                              resource ? resource->nine : NULL, arg->Offset,
                                              arg->Stride));
}

static HRESULT APIENTRY
d3d9_set_stream_source_um(HANDLE handle, const D3DDDIARG_SETSTREAMSOURCEUM *arg,
                          const void *memory)
{
   static volatile LONG said;
   struct d3d9_device *device = handle;

   D3D9_TRACE("stream %u: memory %p stride %u", arg->Stream, memory, arg->Stride);
   /* Nine draws from the program's memory in stream 0 only, which is the
    * only stream the API lets a program do it in. */
   if (arg->Stream == 0) {
      device->um_vertices = memory;
      device->stream0_stride = arg->Stride;
   } else if (InterlockedIncrement(&said) <= 3) {
      d3d9_log("stream %u from memory: not done", arg->Stream);
   }
   return S_OK;
}

static HRESULT APIENTRY
d3d9_set_stream_source_freq(HANDLE handle, const D3DDDIARG_SETSTREAMSOURCEFREQ *arg)
{
   struct d3d9_device *device = handle;

   if (!device->nine)
      return S_OK;
   D3D9_CHECK("stream frequency", entangled_nine_stream_frequency(device->nine, arg->Stream,
                                                                  arg->Divider));
}

static HRESULT APIENTRY
d3d9_set_indices(HANDLE handle, const D3DDDIARG_SETINDICES *arg)
{
   struct d3d9_device *device = handle;
   struct d3d9_resource *resource = arg->hIndexBuffer;

   device->um_indices = NULL;
   device->indices = resource;
   /* As for stream 0: the runtime's own scratch for indices. */
   if (resource && resource->sysmem && resource->flags.Dynamic) {
      device->um_indices = resource->sysmem[0].memory;
      device->um_index_size = arg->Stride;
      device->indices = NULL;
      return S_OK;
   }
   if (!device->nine)
      return S_OK;
   D3D9_CHECK("indices", entangled_nine_indices(device->nine, resource ? resource->nine : NULL));
}

static HRESULT APIENTRY
d3d9_set_indices_um(HANDLE handle, UINT index_size, const void *memory)
{
   struct d3d9_device *device = handle;

   device->um_indices = memory;
   device->um_index_size = index_size;
   return S_OK;
}

/* How many vertices `count` primitives of a type are. */
static UINT
d3d9_vertex_count(D3DPRIMITIVETYPE type, UINT count)
{
   switch (type) {
   case D3DPT_POINTLIST: return count;
   case D3DPT_LINELIST: return count * 2;
   case D3DPT_LINESTRIP: return count + 1;
   case D3DPT_TRIANGLELIST: return count * 3;
   default: return count + 2; /* a strip or a fan */
   }
}

/* Indices from memory over vertices in a buffer: Nine draws either all from
 * memory or all from buffers, so the indices go into a buffer of ours. */
static long
d3d9_draw_with_indices(struct d3d9_device *device, D3DPRIMITIVETYPE type, INT base_vertex,
                       UINT min_index, UINT vertices, const void *indices, UINT index_size,
                       UINT count)
{
   UINT bytes = d3d9_vertex_count(type, count) * index_size;
   unsigned box[6] = {0, 0, bytes, 0, 0, 1}, pitch, slice_pitch;
   void *data = NULL;
   long result = S_OK;

   if (!device->scratch_indices || device->scratch_size < bytes ||
       device->um_index_size != index_size) {
      struct entangled_nine_resource desc;

      entangled_nine_release(device->scratch_indices);
      memset(&desc, 0, sizeof desc);
      desc.kind = ENTANGLED_NINE_INDEX_BUFFER;
      desc.width = max(bytes, 64 * 1024);
      desc.format = index_size == 4 ? 102 : 101; /* D3DFMT_INDEX32, D3DFMT_INDEX16 */
      desc.usage = 0x208; /* D3DUSAGE_DYNAMIC | D3DUSAGE_WRITEONLY */
      device->scratch_indices = entangled_nine_resource_create(device->nine, &desc, &result);
      device->scratch_size = device->scratch_indices ? desc.width : 0;
      device->um_index_size = index_size;
      if (!device->scratch_indices)
         return result;
   }
   result = entangled_nine_lock(device->nine, device->scratch_indices,
                                ENTANGLED_NINE_INDEX_BUFFER, 1, 0, box, 0x2000 /* DISCARD */,
                                &data, &pitch, &slice_pitch);
   if (result < 0 || !data)
      return result;
   memcpy(data, indices, bytes);
   entangled_nine_unlock(device->nine, device->scratch_indices, ENTANGLED_NINE_INDEX_BUFFER, 1,
                         0);
   entangled_nine_indices(device->nine, device->scratch_indices);
   result = entangled_nine_draw_indexed(device->nine, type, base_vertex, min_index, vertices, 0,
                                        count);
   entangled_nine_indices(device->nine, device->indices ? device->indices->nine : NULL);
   return result;
}

static HRESULT APIENTRY
d3d9_draw_primitive(HANDLE handle, const D3DDDIARG_DRAWPRIMITIVE *arg, const UINT *flag_buffer)
{
   struct d3d9_device *device = handle;

   (void)flag_buffer;
   D3D9_TRACE("draw: type %u start %u count %u%s", (unsigned)arg->PrimitiveType, arg->VStart,
              arg->PrimitiveCount, device->um_vertices ? " (memory)" : "");
   if (!device->nine)
      return S_OK;
   if (device->um_vertices)
      D3D9_CHECK("draw", entangled_nine_draw_up(device->nine, arg->PrimitiveType,
                                                arg->PrimitiveCount,
                                                (const char *)device->um_vertices +
                                                   (size_t)arg->VStart * device->stream0_stride,
                                                device->stream0_stride));
   D3D9_CHECK("draw", entangled_nine_draw(device->nine, arg->PrimitiveType, arg->VStart,
                                          arg->PrimitiveCount));
}

static HRESULT APIENTRY
d3d9_draw_indexed_primitive(HANDLE handle, const D3DDDIARG_DRAWINDEXEDPRIMITIVE *arg)
{
   struct d3d9_device *device = handle;

   if (!device->nine)
      return S_OK;
   if (device->um_vertices && device->um_indices)
      D3D9_CHECK("draw", entangled_nine_draw_indexed_up(
                            device->nine, arg->PrimitiveType, arg->MinIndex, arg->NumVertices,
                            arg->PrimitiveCount,
                            (const char *)device->um_indices +
                               (size_t)arg->StartIndex * device->um_index_size,
                            device->um_index_size,
                            (const char *)device->um_vertices +
                               (ptrdiff_t)arg->BaseVertexIndex * device->stream0_stride,
                            device->stream0_stride));
   if (device->um_indices)
      D3D9_CHECK("draw", d3d9_draw_with_indices(
                            device, arg->PrimitiveType, arg->BaseVertexIndex, arg->MinIndex,
                            arg->NumVertices,
                            (const char *)device->um_indices +
                               (size_t)arg->StartIndex * device->um_index_size,
                            device->um_index_size, arg->PrimitiveCount));
   D3D9_CHECK("draw", entangled_nine_draw_indexed(device->nine, arg->PrimitiveType,
                                                  arg->BaseVertexIndex, arg->MinIndex,
                                                  arg->NumVertices, arg->StartIndex,
                                                  arg->PrimitiveCount));
}

/* The "2" forms: vertices the runtime has put in stream 0 itself (the
 * program's own from a Draw...UP, whatever they are: the declaration says),
 * addressed in bytes. */
static HRESULT APIENTRY
d3d9_draw_primitive2(HANDLE handle, const D3DDDIARG_DRAWPRIMITIVE2 *arg)
{
   struct d3d9_device *device = handle;
   UINT stride = device->stream0_stride;
   long result;

   D3D9_TRACE("draw2: type %u offset %u count %u stride %u%s", (unsigned)arg->PrimitiveType,
              arg->FirstVertexOffset, arg->PrimitiveCount, stride,
              device->um_vertices ? " (memory)" : "");
   if (!device->nine)
      return S_OK;
   if (device->um_vertices) {
      const float *v = (const float *)((const char *)device->um_vertices + arg->FirstVertexOffset);

      D3D9_TRACE("  first vertex at %p: %g %g %g %g 0x%08x", (const void *)v, v[0], v[1], v[2],
                 v[3], ((const unsigned *)v)[4]);
   }
   if (device->um_vertices)
      result = entangled_nine_draw_up(device->nine, arg->PrimitiveType, arg->PrimitiveCount,
                                      (const char *)device->um_vertices + arg->FirstVertexOffset,
                                      stride);
   else
      result = entangled_nine_draw(device->nine, arg->PrimitiveType,
                                   stride ? arg->FirstVertexOffset / stride : 0,
                                   arg->PrimitiveCount);
   D3D9_CHECK("draw", result);
}

static HRESULT APIENTRY
d3d9_draw_indexed_primitive2(HANDLE handle, const D3DDDIARG_DRAWINDEXEDPRIMITIVE2 *arg,
                             UINT index_size, const void *indices, const UINT *flag_buffer)
{
   struct d3d9_device *device = handle;
   UINT stride = device->stream0_stride;
   const void *first = (const char *)indices + arg->StartIndexOffset;
   long result;

   (void)flag_buffer;
   D3D9_TRACE("draw indexed2: type %u base %d count %u stride %u%s",
              (unsigned)arg->PrimitiveType, arg->BaseVertexOffset, arg->PrimitiveCount, stride,
              device->um_vertices ? " (memory)" : "");
   if (!device->nine)
      return S_OK;
   if (device->um_vertices)
      result = entangled_nine_draw_indexed_up(
         device->nine, arg->PrimitiveType, arg->MinIndex, arg->NumVertices, arg->PrimitiveCount,
         first, index_size, (const char *)device->um_vertices + arg->BaseVertexOffset, stride);
   else
      result = d3d9_draw_with_indices(device, arg->PrimitiveType,
                                      stride ? arg->BaseVertexOffset / (INT)stride : 0,
                                      arg->MinIndex, arg->NumVertices, first, index_size,
                                      arg->PrimitiveCount);
   D3D9_CHECK("draw", result);
}

/* -------------------------------------------------------------- queries */

static HRESULT APIENTRY
d3d9_create_query(HANDLE handle, D3DDDIARG_CREATEQUERY *arg)
{
   struct d3d9_device *device = handle;
   long result = S_OK;

   arg->hQuery = NULL;
   if (!device->nine)
      return S_OK;
   arg->hQuery = entangled_nine_query_create(device->nine, (unsigned)arg->QueryType, &result);
   D3D9_CHECK("query", result);
}

static HRESULT APIENTRY
d3d9_issue_query(HANDLE handle, const D3DDDIARG_ISSUEQUERY *arg)
{
   (void)handle;
   if (!arg->hQuery)
      return S_OK;
   /* D3DISSUE_END is 1 and D3DISSUE_BEGIN 2. */
   D3D9_CHECK("query", entangled_nine_query_issue(arg->hQuery, arg->Flags.Begin ? 2 : 1));
}

static HRESULT APIENTRY
d3d9_get_query_data(HANDLE handle, const D3DDDIARG_GETQUERYDATA *arg)
{
   (void)handle;
   if (!arg->hQuery)
      return S_OK;
   return (HRESULT)entangled_nine_query_data(arg->hQuery, arg->pData,
                                             entangled_nine_query_size(arg->hQuery), 1);
}

/* -------------------------------------------------------------- present */

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
   if (InterlockedIncrement(&said) <= 2 || FAILED(hr))
      d3d9_log("present %ux%u: 0x%08lx", resource->width, resource->height, (unsigned long)hr);
   return hr;
}

/* ------------------------------------------------------- device itself */

static HRESULT APIENTRY
d3d9_destroy_device(HANDLE handle)
{
   struct d3d9_device *device = handle;

   entangled_nine_release(device->scratch_indices);
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
   D3DDDI_DEVICEFUNCS *funcs = arg->pDeviceFuncs;

   if (!device)
      return E_OUTOFMEMORY;
   device->runtime = arg->hDevice;
   device->adapter = adapter;
   device->callbacks = *arg->pCallbacks;
   d3d9_fill_stubs(funcs);
   funcs->pfnDestroyDevice = d3d9_destroy_device;
   funcs->pfnCreateResource2 = d3d9_create_resource2;
   funcs->pfnOpenResource = d3d9_open_resource;
   funcs->pfnDestroyResource = d3d9_destroy_resource;
   funcs->pfnLock = d3d9_lock;
   funcs->pfnUnlock = d3d9_unlock;
   funcs->pfnLockAsync = d3d9_lock_async;
   funcs->pfnUnlockAsync = d3d9_unlock_async;
   funcs->pfnRename = d3d9_rename;
   funcs->pfnTexBlt1 = d3d9_tex_blt1;
   funcs->pfnBufBlt1 = d3d9_buf_blt1;
   funcs->pfnBlt = d3d9_blt;
   funcs->pfnColorFill = d3d9_color_fill;
   funcs->pfnGenerateMipSubLevels = d3d9_generate_mips;
   funcs->pfnSetRenderTarget = d3d9_set_render_target;
   funcs->pfnSetDepthStencil = d3d9_set_depth_stencil;
   funcs->pfnClear = d3d9_clear;
   funcs->pfnSetRenderState = d3d9_set_render_state;
   funcs->pfnSetTextureStageState = d3d9_set_texture_stage_state;
   funcs->pfnSetTexture = d3d9_set_texture;
   funcs->pfnSetViewport = d3d9_set_viewport;
   funcs->pfnSetZRange = d3d9_set_z_range;
   funcs->pfnSetScissorRect = d3d9_set_scissor_rect;
   funcs->pfnSetClipPlane = d3d9_set_clip_plane;
   funcs->pfnSetTransform = d3d9_set_transform;
   funcs->pfnMultiplyTransform = d3d9_multiply_transform;
   funcs->pfnSetMaterial = d3d9_set_material;
   funcs->pfnSetLight = d3d9_set_light;
   funcs->pfnDestroyLight = d3d9_destroy_light;
   funcs->pfnCreateVertexShaderDecl = d3d9_create_vertex_shader_decl;
   funcs->pfnSetVertexShaderDecl = d3d9_set_vertex_shader_decl;
   funcs->pfnDeleteVertexShaderDecl = d3d9_delete_vertex_shader_decl;
   funcs->pfnCreateVertexShaderFunc = d3d9_create_vertex_shader_func;
   funcs->pfnSetVertexShaderFunc = d3d9_set_vertex_shader_func;
   funcs->pfnDeleteVertexShaderFunc = d3d9_delete_object;
   funcs->pfnCreatePixelShader = d3d9_create_pixel_shader;
   funcs->pfnSetPixelShader = d3d9_set_pixel_shader;
   funcs->pfnDeletePixelShader = d3d9_delete_object;
   funcs->pfnSetVertexShaderConst = d3d9_set_vertex_shader_const;
   funcs->pfnSetVertexShaderConstI = d3d9_set_vertex_shader_const_i;
   funcs->pfnSetVertexShaderConstB = d3d9_set_vertex_shader_const_b;
   funcs->pfnSetPixelShaderConst = d3d9_set_pixel_shader_const;
   funcs->pfnSetPixelShaderConstI = d3d9_set_pixel_shader_const_i;
   funcs->pfnSetPixelShaderConstB = d3d9_set_pixel_shader_const_b;
   funcs->pfnSetStreamSource = d3d9_set_stream_source;
   funcs->pfnSetStreamSourceUm = d3d9_set_stream_source_um;
   funcs->pfnSetStreamSourceFreq = d3d9_set_stream_source_freq;
   funcs->pfnSetIndices = d3d9_set_indices;
   funcs->pfnSetIndicesUm = d3d9_set_indices_um;
   funcs->pfnDrawPrimitive = d3d9_draw_primitive;
   funcs->pfnDrawIndexedPrimitive = d3d9_draw_indexed_primitive;
   funcs->pfnDrawPrimitive2 = d3d9_draw_primitive2;
   funcs->pfnDrawIndexedPrimitive2 = d3d9_draw_indexed_primitive2;
   funcs->pfnCreateQuery = d3d9_create_query;
   funcs->pfnDestroyQuery = d3d9_delete_object;
   funcs->pfnIssueQuery = d3d9_issue_query;
   funcs->pfnGetQueryData = d3d9_get_query_data;
   funcs->pfnFlush1 = d3d9_flush1;
   funcs->pfnPresent1 = d3d9_present1;
   /* A driver of this interface version has the newer form of six functions
    * and must leave the older one NULL: the runtime fails CreateDevice
    * otherwise ("UMD did not provide non-null pfnPresent1, and null
    * pfnPresent", and so on down the list). */
   funcs->pfnPresent = NULL;
   funcs->pfnFlush = NULL;
   funcs->pfnCreateResource = NULL;
   funcs->pfnVolBlt = NULL;
   funcs->pfnTexBlt = NULL;
   funcs->pfnBufBlt = NULL;
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
