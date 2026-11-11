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

#include <stdarg.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

/* ------------------------------------------------------------------ log */

/* The kernel driver prints an escape of this type on the VM's serial log
 * (a diagnostic build of it; a quiet one drops it). */
#define ESCAPE_PRINT 0x900

/* gdi32's thunks, as d3dkmthk.h has them: only what the log needs, so that
 * the adapter-level code needs no device. */
typedef struct {
   HDC hDc;
   UINT hAdapter;
   LUID AdapterLuid;
   UINT VidPnSourceId;
} KMT_OPENADAPTERFROMHDC;

typedef struct {
   UINT hAdapter;
   UINT hDevice;
   UINT Type; /* D3DKMT_ESCAPE_DRIVERPRIVATE = 0 */
   UINT Flags;
   VOID *pPrivateDriverData;
   UINT PrivateDriverDataSize;
   UINT hContext;
} KMT_ESCAPE;

typedef struct {
   UINT hAdapter;
} KMT_CLOSEADAPTER;

extern NTSTATUS APIENTRY D3DKMTOpenAdapterFromHdc(KMT_OPENADAPTERFROMHDC *);
extern NTSTATUS APIENTRY D3DKMTEscape(const KMT_ESCAPE *);
extern NTSTATUS APIENTRY D3DKMTCloseAdapter(const KMT_CLOSEADAPTER *);

static UINT log_adapter;
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
   KMT_ESCAPE call;
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
      KMT_OPENADAPTERFROMHDC open;

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

struct d3d9_device {
   HANDLE runtime;
   D3DDDI_DEVICECALLBACKS callbacks;
};

/* Phase N0's resources: a handle and what was asked for, nothing behind it.
 * The runtime will not have a device whose resources have no handle. */
struct d3d9_resource {
   HANDLE runtime;
   D3DDDIFORMAT format;
   UINT surfaces;
   UINT width, height;
};

static HRESULT APIENTRY
d3d9_create_resource2(HANDLE device, D3DDDIARG_CREATERESOURCE2 *arg)
{
   static volatile LONG said;
   struct d3d9_resource *resource = calloc(1, sizeof *resource);

   (void)device;
   if (!resource)
      return E_OUTOFMEMORY;
   resource->runtime = arg->hResource;
   resource->format = arg->Format;
   resource->surfaces = arg->SurfCount;
   if (arg->SurfCount && arg->pSurfList) {
      resource->width = arg->pSurfList[0].Width;
      resource->height = arg->pSurfList[0].Height;
   }
   arg->hResource = resource;
   if (InterlockedIncrement(&said) <= 24)
      d3d9_log("resource %p: format %u, %ux%u, %u surfaces, pool %u, flags 0x%x", (void *)resource,
               (unsigned)arg->Format, resource->width, resource->height, arg->SurfCount,
               (unsigned)arg->Pool, arg->Flags.Value);
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
d3d9_destroy_resource(HANDLE device, HANDLE resource)
{
   (void)device;
   free(resource);
   return S_OK;
}

static HRESULT APIENTRY
d3d9_destroy_device(HANDLE handle)
{
   d3d9_log("device %p destroyed", handle);
   free(handle);
   return S_OK;
}

static HRESULT APIENTRY
d3d9_create_device(HANDLE adapter, D3DDDIARG_CREATEDEVICE *arg)
{
   struct d3d9_device *device = calloc(1, sizeof *device);

   (void)adapter;
   if (!device)
      return E_OUTOFMEMORY;
   device->runtime = arg->hDevice;
   device->callbacks = *arg->pCallbacks;
   d3d9_fill_stubs(arg->pDeviceFuncs);
   arg->pDeviceFuncs->pfnDestroyDevice = d3d9_destroy_device;
   /* A driver of this interface version has the newer form of six functions
    * and must leave the older one NULL: the runtime fails CreateDevice
    * otherwise ("UMD did not provide non-null pfnPresent1, and null
    * pfnPresent", and so on down the list). */
   arg->pDeviceFuncs->pfnCreateResource2 = d3d9_create_resource2;
   arg->pDeviceFuncs->pfnOpenResource = d3d9_open_resource;
   arg->pDeviceFuncs->pfnDestroyResource = d3d9_destroy_resource;
   arg->pDeviceFuncs->pfnPresent = NULL;
   arg->pDeviceFuncs->pfnFlush = NULL;
   arg->pDeviceFuncs->pfnCreateResource = NULL;
   arg->pDeviceFuncs->pfnVolBlt = NULL;
   arg->pDeviceFuncs->pfnTexBlt = NULL;
   arg->pDeviceFuncs->pfnBufBlt = NULL;
   arg->hDevice = device;
   d3d9_log("device %p: interface %u, version 0x%x, flags 0x%x", (void *)device,
            arg->Interface, arg->Version, arg->Flags.Value);
   return S_OK;
}

/* -------------------------------------------------------------- adapter */

struct d3d9_adapter {
   HANDLE runtime;
   D3DDDI_ADAPTERCALLBACKS callbacks;
};

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
   }
   return TRUE;
}
