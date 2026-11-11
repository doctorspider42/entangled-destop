/*
 * The Nine side of entangled_d3d9.dll (ADR-0011): Mesa's Direct3D 9 state
 * tracker on a Gallium screen, reached by the DDI front through d3d9_nine.h.
 *
 * Nine implements the Direct3D 9 API; the DDI is that API one level down,
 * with the runtime keeping what it keeps for every driver (swap chains,
 * state blocks, validation). So each function here is a call of Nine's
 * device, on objects the front holds as void pointers.
 */
#include "adapter9.h"
#include "device9.h"
#include "iunknown.h"
#include "surface9.h"

#include "pipe/p_context.h"
#include "pipe/p_screen.h"
#include "util/u_math.h"
#include "softpipe/sp_public.h"
#include "frontend/sw_winsys.h"
#include "sw/null/null_sw_winsys.h"
#include "zink/zink_public.h"

#include "d3d9_nine.h"

#include <stdio.h>
#include <stdlib.h>
#include <string.h>

struct entangled_nine {
   struct d3dadapter9_context context;
   struct NineAdapter9 *adapter;
   /* Nine's device, with no swap chain of its own: under the DDI the runtime
    * has those, and hands the driver its back buffers as resources. */
   struct NineDevice9 *device;
};

/* Nine calls this last, when its adapter goes: the screen is the context's. */
static void
entangled_nine_context_destroy(struct d3dadapter9_context *context)
{
   if (context->ref)
      context->ref->destroy(context->ref);
   context->ref = NULL;
   if (context->hal)
      context->hal->destroy(context->hal);
   context->hal = NULL;
}

struct entangled_nine *
entangled_nine_open(struct entangled_nine_info *info, long *result)
{
   struct entangled_nine *nine = calloc(1, sizeof *nine);
   struct pipe_screen *screen;
   D3DCAPS9 caps;
   HRESULT hr;

   *result = E_FAIL;
   if (!nine) {
      *result = E_OUTOFMEMORY;
      return NULL;
   }
   screen = zink_create_screen(NULL, NULL);
   if (!screen) {
      free(nine);
      return NULL;
   }

   nine->context.hal = screen;
   /* Nine keeps a software screen beside the hardware one, for vertices a
    * program asks to have processed in software (ProcessVertices, software
    * vertex processing): softpipe, drawing nowhere. */
   {
      struct sw_winsys *winsys = null_sw_create();

      nine->context.ref = winsys ? softpipe_create_screen(winsys) : NULL;
      if (!nine->context.ref) {
         if (winsys)
            winsys->destroy(winsys);
         screen->destroy(screen);
         free(nine);
         return NULL;
      }
   }
   nine->context.destroy = entangled_nine_context_destroy;
   nine->context.memfd_virtualsizelimit = -1;
   nine->context.override_vram_size = -1;
   nine->context.vblank_mode = 1;
   /* Synchronous: the runtime calls from the program's thread and expects
    * each call done when it returns. */
   nine->context.csmt_force = 0;
   snprintf(nine->context.identifier.Driver, sizeof nine->context.identifier.Driver,
            "entangled_d3d9.dll");
   snprintf(nine->context.identifier.Description,
            sizeof nine->context.identifier.Description, "%s", screen->get_name(screen));

   hr = NineAdapter9_new(&nine->context, &nine->adapter);
   if (FAILED(hr)) {
      nine->context.ref->destroy(nine->context.ref);
      screen->destroy(screen);
      free(nine);
      *result = hr;
      return NULL;
   }

   memset(info, 0, sizeof *info);
   snprintf(info->renderer, sizeof info->renderer, "%s", screen->get_name(screen));
   hr = NineAdapter9_GetDeviceCaps(nine->adapter, D3DDEVTYPE_HAL, &caps);
   if (SUCCEEDED(hr)) {
      D3DDEVICE_CREATION_PARAMETERS creation;
      D3DPRESENT_PARAMETERS present;

      info->vertex_shader = caps.VertexShaderVersion;
      info->pixel_shader = caps.PixelShaderVersion;
      info->max_texture_width = caps.MaxTextureWidth;
      info->simultaneous_rts = caps.NumSimultaneousRTs;

      memset(&creation, 0, sizeof creation);
      creation.DeviceType = D3DDEVTYPE_HAL;
      /* The runtime has already set the FPU as the program asked. */
      creation.BehaviorFlags = D3DCREATE_HARDWARE_VERTEXPROCESSING | D3DCREATE_FPU_PRESERVE;
      memset(&present, 0, sizeof present);
      present.Windowed = TRUE;
      /* No IDirect3D9 and no presentation group: 0003's swap-chain-less
       * device. */
      hr = NineDevice9_new(screen, &creation, &caps, &present, NULL, NULL, &nine->context,
                           false, NULL, &nine->device, 0);
   }
   if (FAILED(hr)) {
      NineUnknown_Destroy(&nine->adapter->base);
      free(nine);
      *result = hr;
      return NULL;
   }
   *result = hr;
   return nine;
}

void
entangled_nine_close(struct entangled_nine *nine)
{
   if (!nine)
      return;
   if (nine->device)
      NineUnknown_Destroy(&nine->device->base);
   /* The adapter's destructor destroys the screen (the context's destroy). */
   NineUnknown_Destroy(&nine->adapter->base);
   free(nine);
}

void
entangled_nine_release(void *object)
{
   if (object)
      NineUnknown_Release(NineUnknown(object));
}

void *
entangled_nine_surface_create(struct entangled_nine *nine,
                              const struct entangled_nine_surface *desc, long *result)
{
   IDirect3DSurface9 *surface = NULL;

   if (desc->depth_stencil)
      *result = NineDevice9_CreateDepthStencilSurface(nine->device, desc->width, desc->height,
                                                      desc->format, desc->multisample,
                                                      desc->quality, FALSE, &surface, NULL);
   else
      *result = NineDevice9_CreateRenderTarget(nine->device, desc->width, desc->height,
                                               desc->format, desc->multisample, desc->quality,
                                               desc->lockable, &surface, NULL);
   return SUCCEEDED(*result) ? surface : NULL;
}

long
entangled_nine_set_render_target(struct entangled_nine *nine, unsigned index, void *surface)
{
   return NineDevice9_SetRenderTarget(nine->device, index, surface);
}

long
entangled_nine_set_depth_stencil(struct entangled_nine *nine, void *surface)
{
   return NineDevice9_SetDepthStencilSurface(nine->device, surface);
}

long
entangled_nine_clear(struct entangled_nine *nine, unsigned count, const long *rects,
                     unsigned flags, unsigned color, float z, unsigned stencil)
{
   /* D3DRECT is four LONGs. */
   return NineDevice9_Clear(nine->device, count, (const D3DRECT *)rects, flags, color, z,
                            stencil);
}

long
entangled_nine_read_surface(struct entangled_nine *nine, void *surface, void *pixels,
                            unsigned width, unsigned height)
{
   struct NineSurface9 *nine_surface = NineSurface9(surface);
   D3DLOCKED_RECT locked;
   HRESULT hr;

   (void)nine;
   hr = NineSurface9_LockRect(nine_surface, &locked, NULL, D3DLOCK_READONLY);
   if (FAILED(hr))
      return hr;
   width = MIN2(width, nine_surface->desc.Width);
   height = MIN2(height, nine_surface->desc.Height);
   for (unsigned y = 0; y < height; y++)
      memcpy((char *)pixels + (size_t)y * width * 4,
             (char *)locked.pBits + (size_t)y * locked.Pitch, (size_t)width * 4);
   NineSurface9_UnlockRect(nine_surface);
   return S_OK;
}

void
entangled_nine_flush(struct entangled_nine *nine)
{
   struct pipe_context *pipe = NineDevice9_GetPipe(nine->device);

   pipe->flush(pipe, NULL, 0);
}
