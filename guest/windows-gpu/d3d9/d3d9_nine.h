/*
 * What the DDI front (entangled_d3d9.c) asks of the Nine side (d3d9_nine.c).
 *
 * The two are separate translation units because they cannot be one: the
 * front sees the WDK's Direct3D 9 headers and Nine sees Mesa's, and the two
 * declare the same types. So nothing here names a Direct3D type: plain C
 * only, and each side translates. Numbers that are Direct3D's (a D3DFORMAT,
 * a render state) cross as unsigned: the DDI's values are the API's.
 */
#ifndef ENTANGLED_D3D9_NINE_H
#define ENTANGLED_D3D9_NINE_H

struct entangled_nine;

/* What an adapter of Nine's says about itself. */
struct entangled_nine_info {
   char renderer[128];          /* the Gallium screen's name */
   unsigned vertex_shader;      /* D3DCAPS9.VertexShaderVersion */
   unsigned pixel_shader;       /* D3DCAPS9.PixelShaderVersion */
   unsigned max_texture_width;
   unsigned simultaneous_rts;
};

/* A Gallium screen (Zink), a Nine adapter on it and a Nine device with no
 * swap chain of its own; or NULL and, in `result`, the HRESULT that says why
 * (E_FAIL: no screen at all). */
struct entangled_nine *
entangled_nine_open(struct entangled_nine_info *info, long *result);

void
entangled_nine_close(struct entangled_nine *nine);

/*
 * From here on: the device. Every function answers an HRESULT as a long.
 * Objects of Nine's (surfaces, textures, buffers, shaders) cross the seam as
 * void pointers; entangled_nine_release gives one back.
 */

void
entangled_nine_release(void *object);

/* A render target or a depth-stencil surface on its own (a back buffer is
 * one). `format` is a D3DFORMAT, `multisample` a D3DMULTISAMPLE_TYPE. */
struct entangled_nine_surface {
   unsigned width, height;
   unsigned format;
   unsigned multisample, quality;
   int depth_stencil;
   int lockable;
};

void *
entangled_nine_surface_create(struct entangled_nine *nine,
                              const struct entangled_nine_surface *desc, long *result);

long
entangled_nine_set_render_target(struct entangled_nine *nine, unsigned index, void *surface);

long
entangled_nine_set_depth_stencil(struct entangled_nine *nine, void *surface);

/* `rects` is `count` rectangles of four longs (x1, y1, x2, y2), as D3DRECT. */
long
entangled_nine_clear(struct entangled_nine *nine, unsigned count, const long *rects,
                     unsigned flags, unsigned color, float z, unsigned stencil);

/* A surface's pixels, rows of `width` four-byte pixels packed into `pixels`
 * (what a present hands the kernel driver). */
long
entangled_nine_read_surface(struct entangled_nine *nine, void *surface, void *pixels,
                            unsigned width, unsigned height);

/* Everything drawn so far to the host. */
void
entangled_nine_flush(struct entangled_nine *nine);

#endif
