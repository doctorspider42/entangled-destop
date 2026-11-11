/*
 * The Nine side of entangled_d3d9.dll (ADR-0011): Mesa's Direct3D 9 state
 * tracker on a Gallium screen, reached by the DDI front through d3d9_nine.h.
 *
 * Phase N1: the screen (Zink over the Venus ICD) and an adapter of Nine's on
 * it, which is enough to say that Nine and a driver under it load and agree
 * inside a Direct3D 9 program. The device comes with N2.
 */
#include "adapter9.h"
#include "iunknown.h"

#include "pipe/p_screen.h"
#include "zink/zink_public.h"

#include "d3d9_nine.h"

#include <stdio.h>
#include <stdlib.h>
#include <string.h>

struct entangled_nine {
   struct d3dadapter9_context context;
   struct NineAdapter9 *adapter;
};

/* Nine calls this last, when its adapter goes: the screen is the context's. */
static void
entangled_nine_context_destroy(struct d3dadapter9_context *context)
{
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
   /* No software screen beside it: Nine's software vertex processing then
    * runs on the hardware one, which is what a driver's device does. */
   nine->context.ref = NULL;
   nine->context.destroy = entangled_nine_context_destroy;
   nine->context.memfd_virtualsizelimit = -1;
   nine->context.override_vram_size = -1;
   nine->context.vblank_mode = 1;
   snprintf(nine->context.identifier.Driver, sizeof nine->context.identifier.Driver,
            "entangled_d3d9.dll");
   snprintf(nine->context.identifier.Description,
            sizeof nine->context.identifier.Description, "%s", screen->get_name(screen));

   hr = NineAdapter9_new(&nine->context, &nine->adapter);
   if (FAILED(hr)) {
      screen->destroy(screen);
      free(nine);
      *result = hr;
      return NULL;
   }

   memset(info, 0, sizeof *info);
   snprintf(info->renderer, sizeof info->renderer, "%s", screen->get_name(screen));
   hr = NineAdapter9_GetDeviceCaps(nine->adapter, D3DDEVTYPE_HAL, &caps);
   if (SUCCEEDED(hr)) {
      info->vertex_shader = caps.VertexShaderVersion;
      info->pixel_shader = caps.PixelShaderVersion;
      info->max_texture_width = caps.MaxTextureWidth;
      info->simultaneous_rts = caps.NumSimultaneousRTs;
   }
   *result = hr;
   return nine;
}

void
entangled_nine_close(struct entangled_nine *nine)
{
   if (!nine)
      return;
   /* The adapter's destructor destroys the screen (the context's destroy). */
   NineUnknown_Destroy(&nine->adapter->base);
   free(nine);
}
