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
#include "basetexture9.h"
#include "cubetexture9.h"
#include "indexbuffer9.h"
#include "pixelshader9.h"
#include "query9.h"
#include "surface9.h"
#include "texture9.h"
#include "vertexbuffer9.h"
#include "vertexdeclaration9.h"
#include "vertexshader9.h"
#include "volumetexture9.h"

#include "pipe/p_context.h"
#include "pipe/p_screen.h"
#include "util/u_math.h"
#include "util/u_surface.h"
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
   /* The viewport, which the DDI sets in two halves. */
   D3DVIEWPORT9 viewport;
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
      nine->viewport.MaxZ = 1.0f;
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

/* ---- resources --------------------------------------------------------- */

void *
entangled_nine_resource_create(struct entangled_nine *nine,
                               const struct entangled_nine_resource *desc, long *result)
{
   struct NineDevice9 *device = nine->device;
   void *object = NULL;

   switch (desc->kind) {
   case ENTANGLED_NINE_SURFACE:
      if (desc->depth_stencil)
         *result = NineDevice9_CreateDepthStencilSurface(device, desc->width, desc->height,
                                                         desc->format, desc->multisample,
                                                         desc->quality, FALSE,
                                                         (IDirect3DSurface9 **)&object, NULL);
      else if (desc->render_target)
         /* Lockable whatever the runtime says: a present reads it back. */
         *result = NineDevice9_CreateRenderTarget(device, desc->width, desc->height, desc->format,
                                                  desc->multisample, desc->quality, TRUE,
                                                  (IDirect3DSurface9 **)&object, NULL);
      else
         *result = NineDevice9_CreateOffscreenPlainSurface(device, desc->width, desc->height,
                                                           desc->format, D3DPOOL_DEFAULT,
                                                           (IDirect3DSurface9 **)&object, NULL);
      break;
   case ENTANGLED_NINE_TEXTURE:
      *result = NineDevice9_CreateTexture(device, desc->width, desc->height, desc->levels,
                                          desc->usage, desc->format, D3DPOOL_DEFAULT,
                                          (IDirect3DTexture9 **)&object, NULL);
      break;
   case ENTANGLED_NINE_CUBE:
      *result = NineDevice9_CreateCubeTexture(device, desc->width, desc->levels, desc->usage,
                                              desc->format, D3DPOOL_DEFAULT,
                                              (IDirect3DCubeTexture9 **)&object, NULL);
      break;
   case ENTANGLED_NINE_VOLUME:
      *result = NineDevice9_CreateVolumeTexture(device, desc->width, desc->height, desc->depth,
                                                desc->levels, desc->usage, desc->format,
                                                D3DPOOL_DEFAULT,
                                                (IDirect3DVolumeTexture9 **)&object, NULL);
      break;
   case ENTANGLED_NINE_VERTEX_BUFFER:
      *result = NineDevice9_CreateVertexBuffer(device, desc->width, desc->usage, desc->fvf,
                                               D3DPOOL_DEFAULT,
                                               (IDirect3DVertexBuffer9 **)&object, NULL);
      break;
   case ENTANGLED_NINE_INDEX_BUFFER:
      *result = NineDevice9_CreateIndexBuffer(device, desc->width, desc->usage, desc->format,
                                              D3DPOOL_DEFAULT,
                                              (IDirect3DIndexBuffer9 **)&object, NULL);
      break;
   default:
      *result = E_INVALIDARG;
      break;
   }
   return SUCCEEDED(*result) ? object : NULL;
}

/* The surface a part of a resource is, with a reference the caller gives
 * back (entangled_nine_release); NULL for a part that is not a surface. */
static struct NineSurface9 *
entangled_nine_part(void *object, enum entangled_nine_kind kind, unsigned levels, unsigned sub)
{
   IDirect3DSurface9 *surface = NULL;

   if (!object)
      return NULL;
   switch (kind) {
   case ENTANGLED_NINE_SURFACE:
      NineUnknown_AddRef(NineUnknown(object));
      return NineSurface9(object);
   case ENTANGLED_NINE_TEXTURE:
      if (FAILED(NineTexture9_GetSurfaceLevel(object, sub, &surface)))
         return NULL;
      return NineSurface9(surface);
   case ENTANGLED_NINE_CUBE:
      if (!levels ||
          FAILED(NineCubeTexture9_GetCubeMapSurface(object, sub / levels, sub % levels, &surface)))
         return NULL;
      return NineSurface9(surface);
   default:
      return NULL;
   }
}

long
entangled_nine_lock(struct entangled_nine *nine, void *object, enum entangled_nine_kind kind,
                    unsigned levels, unsigned sub, const unsigned *box, unsigned flags,
                    void **data, unsigned *pitch, unsigned *slice_pitch)
{
   HRESULT hr;

   (void)nine;
   *pitch = 0;
   *slice_pitch = 0;
   switch (kind) {
   case ENTANGLED_NINE_VERTEX_BUFFER:
      return NineVertexBuffer9_Lock(object, box ? box[0] : 0, box ? box[2] - box[0] : 0, data,
                                    flags);
   case ENTANGLED_NINE_INDEX_BUFFER:
      return NineIndexBuffer9_Lock(object, box ? box[0] : 0, box ? box[2] - box[0] : 0, data,
                                   flags);
   case ENTANGLED_NINE_VOLUME: {
      D3DLOCKED_BOX locked;
      D3DBOX d3d_box;

      if (box) {
         d3d_box.Left = box[0];
         d3d_box.Top = box[1];
         d3d_box.Right = box[2];
         d3d_box.Bottom = box[3];
         d3d_box.Front = box[4];
         d3d_box.Back = box[5];
      }
      hr = NineVolumeTexture9_LockBox(object, sub, &locked, box ? &d3d_box : NULL, flags);
      if (SUCCEEDED(hr)) {
         *data = locked.pBits;
         *pitch = locked.RowPitch;
         *slice_pitch = locked.SlicePitch;
      }
      return hr;
   }
   default: {
      struct NineSurface9 *surface = entangled_nine_part(object, kind, levels, sub);
      D3DLOCKED_RECT locked;
      RECT rect;

      if (!surface)
         return E_INVALIDARG;
      if (box) {
         rect.left = box[0];
         rect.top = box[1];
         rect.right = box[2];
         rect.bottom = box[3];
      }
      hr = NineSurface9_LockRect(surface, &locked, box ? &rect : NULL, flags);
      if (SUCCEEDED(hr)) {
         *data = locked.pBits;
         *pitch = locked.Pitch;
      }
      NineUnknown_Release(NineUnknown(surface));
      return hr;
   }
   }
}

long
entangled_nine_unlock(struct entangled_nine *nine, void *object, enum entangled_nine_kind kind,
                      unsigned levels, unsigned sub)
{
   (void)nine;
   switch (kind) {
   case ENTANGLED_NINE_VERTEX_BUFFER:
      return NineVertexBuffer9_Unlock(object);
   case ENTANGLED_NINE_INDEX_BUFFER:
      return NineIndexBuffer9_Unlock(object);
   case ENTANGLED_NINE_VOLUME:
      return NineVolumeTexture9_UnlockBox(object, sub);
   default: {
      struct NineSurface9 *surface = entangled_nine_part(object, kind, levels, sub);
      HRESULT hr;

      if (!surface)
         return E_INVALIDARG;
      hr = NineSurface9_UnlockRect(surface);
      NineUnknown_Release(NineUnknown(surface));
      return hr;
   }
   }
}

long
entangled_nine_upload(struct entangled_nine *nine, void *object, enum entangled_nine_kind kind,
                      unsigned levels, unsigned sub, unsigned x, unsigned y, const void *pixels,
                      unsigned pitch, const long *rect)
{
   struct NineSurface9 *surface = entangled_nine_part(object, kind, levels, sub);
   D3DLOCKED_RECT locked;
   RECT dst;
   HRESULT hr;

   (void)nine;
   if (!surface)
      return E_INVALIDARG;
   dst.left = x;
   dst.top = y;
   dst.right = x + (rect[2] - rect[0]);
   dst.bottom = y + (rect[3] - rect[1]);
   hr = NineSurface9_LockRect(surface, &locked, &dst, 0);
   if (SUCCEEDED(hr)) {
      /* By blocks, so that a compressed format copies as what it is. */
      util_copy_rect(locked.pBits, surface->base.info.format, locked.Pitch, 0, 0,
                     rect[2] - rect[0], rect[3] - rect[1], pixels, pitch, rect[0], rect[1]);
      hr = NineSurface9_UnlockRect(surface);
   }
   NineUnknown_Release(NineUnknown(surface));
   return hr;
}

long
entangled_nine_set_target(struct entangled_nine *nine, unsigned index, void *object,
                          enum entangled_nine_kind kind, unsigned levels, unsigned sub)
{
   struct NineSurface9 *surface = entangled_nine_part(object, kind, levels, sub);
   HRESULT hr = NineDevice9_SetRenderTarget(nine->device, index, (IDirect3DSurface9 *)surface);

   if (surface)
      NineUnknown_Release(NineUnknown(surface));
   return hr;
}

long
entangled_nine_set_depth(struct entangled_nine *nine, void *object, enum entangled_nine_kind kind,
                         unsigned levels, unsigned sub)
{
   struct NineSurface9 *surface = entangled_nine_part(object, kind, levels, sub);
   HRESULT hr = NineDevice9_SetDepthStencilSurface(nine->device, (IDirect3DSurface9 *)surface);

   if (surface)
      NineUnknown_Release(NineUnknown(surface));
   return hr;
}

long
entangled_nine_blit(struct entangled_nine *nine, void *src, enum entangled_nine_kind src_kind,
                    unsigned src_levels, unsigned src_sub, const long *src_rect, void *dst,
                    enum entangled_nine_kind dst_kind, unsigned dst_levels, unsigned dst_sub,
                    const long *dst_rect, unsigned filter)
{
   struct NineSurface9 *from = entangled_nine_part(src, src_kind, src_levels, src_sub);
   struct NineSurface9 *to = entangled_nine_part(dst, dst_kind, dst_levels, dst_sub);
   HRESULT hr = E_INVALIDARG;

   if (from && to)
      hr = NineDevice9_StretchRect(nine->device, (IDirect3DSurface9 *)from,
                                   (const RECT *)src_rect, (IDirect3DSurface9 *)to,
                                   (const RECT *)dst_rect, filter);
   if (from)
      NineUnknown_Release(NineUnknown(from));
   if (to)
      NineUnknown_Release(NineUnknown(to));
   return hr;
}

long
entangled_nine_fill(struct entangled_nine *nine, void *object, enum entangled_nine_kind kind,
                    unsigned levels, unsigned sub, const long *rect, unsigned color)
{
   struct NineSurface9 *surface = entangled_nine_part(object, kind, levels, sub);
   HRESULT hr;

   if (!surface)
      return E_INVALIDARG;
   hr = NineDevice9_ColorFill(nine->device, (IDirect3DSurface9 *)surface, (const RECT *)rect,
                              color);
   NineUnknown_Release(NineUnknown(surface));
   return hr;
}

long
entangled_nine_generate_mips(struct entangled_nine *nine, void *texture)
{
   (void)nine;
   NineBaseTexture9_GenerateMipSubLevels(NineBaseTexture9(texture));
   return S_OK;
}

/* ---- state ------------------------------------------------------------- */

long
entangled_nine_render_state(struct entangled_nine *nine, unsigned state, unsigned value)
{
   return NineDevice9_SetRenderState(nine->device, state, value);
}

long
entangled_nine_texture_stage_state(struct entangled_nine *nine, unsigned stage, unsigned type,
                                   unsigned value)
{
   return NineDevice9_SetTextureStageState(nine->device, stage, type, value);
}

long
entangled_nine_sampler_state(struct entangled_nine *nine, unsigned sampler, unsigned type,
                             unsigned value)
{
   return NineDevice9_SetSamplerState(nine->device, sampler, type, value);
}

long
entangled_nine_texture(struct entangled_nine *nine, unsigned stage, void *texture)
{
   return NineDevice9_SetTexture(nine->device, stage, texture);
}

long
entangled_nine_viewport(struct entangled_nine *nine, const unsigned *rect, const float *range)
{
   D3DVIEWPORT9 *viewport = &nine->viewport;

   if (rect) {
      viewport->X = rect[0];
      viewport->Y = rect[1];
      viewport->Width = rect[2];
      viewport->Height = rect[3];
   }
   if (range) {
      viewport->MinZ = range[0];
      viewport->MaxZ = range[1];
   }
   return NineDevice9_SetViewport(nine->device, viewport);
}

long
entangled_nine_scissor(struct entangled_nine *nine, const long *rect)
{
   return NineDevice9_SetScissorRect(nine->device, (const RECT *)rect);
}

long
entangled_nine_clip_plane(struct entangled_nine *nine, unsigned index, const float *plane)
{
   return NineDevice9_SetClipPlane(nine->device, index, plane);
}

long
entangled_nine_transform(struct entangled_nine *nine, unsigned state, const float *matrix,
                         int multiply)
{
   if (multiply)
      return NineDevice9_MultiplyTransform(nine->device, state, (const D3DMATRIX *)matrix);
   return NineDevice9_SetTransform(nine->device, state, (const D3DMATRIX *)matrix);
}

long
entangled_nine_material(struct entangled_nine *nine, const float *material)
{
   return NineDevice9_SetMaterial(nine->device, (const D3DMATERIAL9 *)material);
}

long
entangled_nine_light(struct entangled_nine *nine, unsigned index, const void *light, int enable)
{
   HRESULT hr = S_OK;

   if (light)
      hr = NineDevice9_SetLight(nine->device, index, light);
   if (SUCCEEDED(hr) && enable >= 0)
      hr = NineDevice9_LightEnable(nine->device, index, enable);
   return hr;
}

/* ---- shaders and vertex layout ------------------------------------------ */

void *
entangled_nine_declaration_create(struct entangled_nine *nine, const void *elements,
                                  unsigned count, long *result)
{
   static const D3DVERTEXELEMENT9 end = D3DDECL_END();
   const D3DVERTEXELEMENT9 *given = elements;
   IDirect3DVertexDeclaration9 *declaration = NULL;
   D3DVERTEXELEMENT9 *copy;

   /* Nine reads up to the end marker, which the runtime may or may not send. */
   if (count && given[count - 1].Stream == 0xff) {
      *result = NineDevice9_CreateVertexDeclaration(nine->device, given, &declaration);
      return SUCCEEDED(*result) ? declaration : NULL;
   }
   copy = malloc((count + 1) * sizeof *copy);
   if (!copy) {
      *result = E_OUTOFMEMORY;
      return NULL;
   }
   memcpy(copy, given, count * sizeof *copy);
   copy[count] = end;
   *result = NineDevice9_CreateVertexDeclaration(nine->device, copy, &declaration);
   free(copy);
   return SUCCEEDED(*result) ? declaration : NULL;
}

long
entangled_nine_declaration(struct entangled_nine *nine, void *declaration)
{
   return NineDevice9_SetVertexDeclaration(nine->device, declaration);
}

void *
entangled_nine_vertex_shader_create(struct entangled_nine *nine, const void *code, long *result)
{
   IDirect3DVertexShader9 *shader = NULL;

   *result = NineDevice9_CreateVertexShader(nine->device, code, &shader);
   return SUCCEEDED(*result) ? shader : NULL;
}

long
entangled_nine_vertex_shader(struct entangled_nine *nine, void *shader)
{
   return NineDevice9_SetVertexShader(nine->device, shader);
}

void *
entangled_nine_pixel_shader_create(struct entangled_nine *nine, const void *code, long *result)
{
   IDirect3DPixelShader9 *shader = NULL;

   *result = NineDevice9_CreatePixelShader(nine->device, code, &shader);
   return SUCCEEDED(*result) ? shader : NULL;
}

long
entangled_nine_pixel_shader(struct entangled_nine *nine, void *shader)
{
   return NineDevice9_SetPixelShader(nine->device, shader);
}

long
entangled_nine_vertex_constants(struct entangled_nine *nine, int type, unsigned start,
                                const void *data, unsigned count)
{
   switch (type) {
   case 'f':
      return NineDevice9_SetVertexShaderConstantF(nine->device, start, data, count);
   case 'i':
      return NineDevice9_SetVertexShaderConstantI(nine->device, start, data, count);
   default:
      return NineDevice9_SetVertexShaderConstantB(nine->device, start, data, count);
   }
}

long
entangled_nine_pixel_constants(struct entangled_nine *nine, int type, unsigned start,
                               const void *data, unsigned count)
{
   switch (type) {
   case 'f':
      return NineDevice9_SetPixelShaderConstantF(nine->device, start, data, count);
   case 'i':
      return NineDevice9_SetPixelShaderConstantI(nine->device, start, data, count);
   default:
      return NineDevice9_SetPixelShaderConstantB(nine->device, start, data, count);
   }
}

/* ---- drawing ------------------------------------------------------------ */

long
entangled_nine_stream(struct entangled_nine *nine, unsigned stream, void *vertex_buffer,
                      unsigned offset, unsigned stride)
{
   return NineDevice9_SetStreamSource(nine->device, stream, vertex_buffer, offset, stride);
}

long
entangled_nine_stream_frequency(struct entangled_nine *nine, unsigned stream, unsigned divider)
{
   return NineDevice9_SetStreamSourceFreq(nine->device, stream, divider);
}

long
entangled_nine_indices(struct entangled_nine *nine, void *index_buffer)
{
   return NineDevice9_SetIndices(nine->device, index_buffer);
}

long
entangled_nine_draw(struct entangled_nine *nine, unsigned primitive, unsigned start,
                    unsigned count)
{
   return NineDevice9_DrawPrimitive(nine->device, primitive, start, count);
}

long
entangled_nine_draw_indexed(struct entangled_nine *nine, unsigned primitive, int base_vertex,
                            unsigned min_index, unsigned vertices, unsigned start_index,
                            unsigned count)
{
   return NineDevice9_DrawIndexedPrimitive(nine->device, primitive, base_vertex, min_index,
                                           vertices, start_index, count);
}

long
entangled_nine_draw_up(struct entangled_nine *nine, unsigned primitive, unsigned count,
                       const void *vertices, unsigned stride)
{
   return NineDevice9_DrawPrimitiveUP(nine->device, primitive, count, vertices, stride);
}

long
entangled_nine_draw_indexed_up(struct entangled_nine *nine, unsigned primitive,
                               unsigned min_index, unsigned vertices, unsigned count,
                               const void *indices, unsigned index_size,
                               const void *vertex_data, unsigned stride)
{
   return NineDevice9_DrawIndexedPrimitiveUP(nine->device, primitive, min_index, vertices, count,
                                             indices,
                                             index_size == 4 ? D3DFMT_INDEX32 : D3DFMT_INDEX16,
                                             vertex_data, stride);
}

/* ---- queries ------------------------------------------------------------ */

void *
entangled_nine_query_create(struct entangled_nine *nine, unsigned type, long *result)
{
   IDirect3DQuery9 *query = NULL;

   *result = NineDevice9_CreateQuery(nine->device, type, &query);
   return SUCCEEDED(*result) ? query : NULL;
}

long
entangled_nine_query_issue(void *query, unsigned flags)
{
   return NineQuery9_Issue(query, flags);
}

long
entangled_nine_query_data(void *query, void *data, unsigned size, int flush)
{
   return NineQuery9_GetData(query, data, size, flush ? D3DGETDATA_FLUSH : 0);
}

unsigned
entangled_nine_query_size(void *query)
{
   return NineQuery9_GetDataSize(query);
}
