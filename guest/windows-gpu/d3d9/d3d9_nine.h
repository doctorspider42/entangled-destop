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

/* ---- resources ---------------------------------------------------------
 *
 * A resource of the DDI's is one of these; the front keeps the kind beside
 * the object and names a part of it by the DDI's sub-resource index (a mip
 * level; for a cube, face * levels + level).
 */
enum entangled_nine_kind {
   ENTANGLED_NINE_SURFACE,       /* a render target, depth buffer or plain surface */
   ENTANGLED_NINE_TEXTURE,
   ENTANGLED_NINE_CUBE,
   ENTANGLED_NINE_VOLUME,
   ENTANGLED_NINE_VERTEX_BUFFER,
   ENTANGLED_NINE_INDEX_BUFFER,
};

struct entangled_nine_resource {
   enum entangled_nine_kind kind;
   unsigned width, height, depth;   /* of the top level; a buffer's length is width */
   unsigned levels;
   unsigned format;                 /* D3DFORMAT */
   unsigned usage;                  /* D3DUSAGE_* */
   unsigned multisample, quality;   /* a surface's */
   int depth_stencil;               /* a surface that is a depth buffer */
   int render_target;               /* a surface that is a render target */
   unsigned fvf;                    /* a vertex buffer's */
};

void *
entangled_nine_resource_create(struct entangled_nine *nine,
                               const struct entangled_nine_resource *desc, long *result);

/* The memory of a part of a resource. `box` is six unsigneds (left, top,
 * right, bottom, front, back) or NULL for all of it; for a buffer, left and
 * right are a byte range. `flags` are D3DLOCK_*. */
long
entangled_nine_lock(struct entangled_nine *nine, void *object, enum entangled_nine_kind kind,
                    unsigned levels, unsigned sub, const unsigned *box, unsigned flags,
                    void **data, unsigned *pitch, unsigned *slice_pitch);

long
entangled_nine_unlock(struct entangled_nine *nine, void *object, enum entangled_nine_kind kind,
                      unsigned levels, unsigned sub);

/* Pixels from memory of the runtime's (a system-memory texture) into a part
 * of a resource: `rect` (left, top, right, bottom) of the source, to
 * (`x`, `y`) of the destination. */
long
entangled_nine_upload(struct entangled_nine *nine, void *object, enum entangled_nine_kind kind,
                      unsigned levels, unsigned sub, unsigned x, unsigned y, const void *pixels,
                      unsigned pitch, const long *rect);

/* Render targets by resource part (a texture level can be one). */
long
entangled_nine_set_target(struct entangled_nine *nine, unsigned index, void *object,
                          enum entangled_nine_kind kind, unsigned levels, unsigned sub);

long
entangled_nine_set_depth(struct entangled_nine *nine, void *object, enum entangled_nine_kind kind,
                         unsigned levels, unsigned sub);

/* StretchRect and ColorFill between parts; rects are four longs or NULL.
 * `filter` is a D3DTEXTUREFILTERTYPE. */
long
entangled_nine_blit(struct entangled_nine *nine, void *src, enum entangled_nine_kind src_kind,
                    unsigned src_levels, unsigned src_sub, const long *src_rect, void *dst,
                    enum entangled_nine_kind dst_kind, unsigned dst_levels, unsigned dst_sub,
                    const long *dst_rect, unsigned filter);

long
entangled_nine_fill(struct entangled_nine *nine, void *object, enum entangled_nine_kind kind,
                    unsigned levels, unsigned sub, const long *rect, unsigned color);

long
entangled_nine_generate_mips(struct entangled_nine *nine, void *texture);

/* ---- state ------------------------------------------------------------- */

long
entangled_nine_render_state(struct entangled_nine *nine, unsigned state, unsigned value);

long
entangled_nine_texture_stage_state(struct entangled_nine *nine, unsigned stage, unsigned type,
                                   unsigned value);

long
entangled_nine_sampler_state(struct entangled_nine *nine, unsigned sampler, unsigned type,
                             unsigned value);

long
entangled_nine_texture(struct entangled_nine *nine, unsigned stage, void *texture);

/* The viewport is set in two calls by the DDI (the rectangle, the depth
 * range); `rect` and `range` may each be NULL to keep what was set. */
long
entangled_nine_viewport(struct entangled_nine *nine, const unsigned *rect, const float *range);

long
entangled_nine_scissor(struct entangled_nine *nine, const long *rect);

long
entangled_nine_clip_plane(struct entangled_nine *nine, unsigned index, const float *plane);

/* `matrix` is sixteen floats (a D3DMATRIX). */
long
entangled_nine_transform(struct entangled_nine *nine, unsigned state, const float *matrix,
                         int multiply);

/* `material` is seventeen floats (a D3DMATERIAL9). */
long
entangled_nine_material(struct entangled_nine *nine, const float *material);

/* `light` is a D3DLIGHT9 (the DDI's D3DDDI_LIGHT has its layout), or NULL to
 * leave the light as it is; `enable` < 0 leaves that as it is. */
long
entangled_nine_light(struct entangled_nine *nine, unsigned index, const void *light,
                     int enable);

/* ---- shaders and vertex layout ------------------------------------------ */

/* `elements` are D3DVERTEXELEMENT9 (eight bytes each), `count` of them, with
 * or without the end marker. */
void *
entangled_nine_declaration_create(struct entangled_nine *nine, const void *elements,
                                  unsigned count, long *result);

long
entangled_nine_declaration(struct entangled_nine *nine, void *declaration);

void *
entangled_nine_vertex_shader_create(struct entangled_nine *nine, const void *code, long *result);

long
entangled_nine_vertex_shader(struct entangled_nine *nine, void *shader);

void *
entangled_nine_pixel_shader_create(struct entangled_nine *nine, const void *code, long *result);

long
entangled_nine_pixel_shader(struct entangled_nine *nine, void *shader);

/* `type` is 'f', 'i' or 'b'; `count` registers from `start`. */
long
entangled_nine_vertex_constants(struct entangled_nine *nine, int type, unsigned start,
                                const void *data, unsigned count);

long
entangled_nine_pixel_constants(struct entangled_nine *nine, int type, unsigned start,
                               const void *data, unsigned count);

/* ---- drawing ------------------------------------------------------------ */

long
entangled_nine_stream(struct entangled_nine *nine, unsigned stream, void *vertex_buffer,
                      unsigned offset, unsigned stride);

long
entangled_nine_stream_frequency(struct entangled_nine *nine, unsigned stream, unsigned divider);

long
entangled_nine_indices(struct entangled_nine *nine, void *index_buffer);

long
entangled_nine_draw(struct entangled_nine *nine, unsigned primitive, unsigned start,
                    unsigned count);

long
entangled_nine_draw_indexed(struct entangled_nine *nine, unsigned primitive, int base_vertex,
                            unsigned min_index, unsigned vertices, unsigned start_index,
                            unsigned count);

/* Vertices (and indices) from memory of the program's. */
long
entangled_nine_draw_up(struct entangled_nine *nine, unsigned primitive, unsigned count,
                       const void *vertices, unsigned stride);

long
entangled_nine_draw_indexed_up(struct entangled_nine *nine, unsigned primitive,
                               unsigned min_index, unsigned vertices, unsigned count,
                               const void *indices, unsigned index_size,
                               const void *vertex_data, unsigned stride);

/* ---- queries ------------------------------------------------------------ */

void *
entangled_nine_query_create(struct entangled_nine *nine, unsigned type, long *result);

long
entangled_nine_query_issue(void *query, unsigned flags);

/* S_OK with the data, S_FALSE while it is not there yet. */
long
entangled_nine_query_data(void *query, void *data, unsigned size, int flush);

unsigned
entangled_nine_query_size(void *query);

#endif
