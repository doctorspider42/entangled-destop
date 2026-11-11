/*
 * What the DDI front (entangled_d3d9.c) asks of the Nine side (d3d9_nine.c).
 *
 * The two are separate translation units because they cannot be one: the
 * front sees the WDK's Direct3D 9 headers and Nine sees Mesa's, and the two
 * declare the same types. So nothing here names a Direct3D type: plain C
 * only, and each side translates.
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

/* A Gallium screen (Zink) with a Nine adapter on it, or NULL and, in
 * `result`, the HRESULT that says why (E_FAIL: no screen at all). */
struct entangled_nine *
entangled_nine_open(struct entangled_nine_info *info, long *result);

void
entangled_nine_close(struct entangled_nine *nine);

#endif
