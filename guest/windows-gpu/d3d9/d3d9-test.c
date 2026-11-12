/*
 * d3d9-test.exe: what a Direct3D 9 program does first, with every answer
 * printed - whether Windows hands our adapter a Direct3D 9 driver at all
 * (ADR-0011, phase N0), and how far a device gets.
 *
 * usage: d3d9-test [frames]     (prints to stdout; exit code 0 = a device)
 */
#include <windows.h>

#include <d3d9.h>
#include <math.h>
#include <stdio.h>
#include <stdlib.h>

/* Why not: d3d9.dll keeps its last errors, with the sentence it would have
 * told a debugger, in a table in its own data ("HAL Disabled: ...", "UMD
 * failed ..."). The table has no name to look up, but it is text in a
 * section that is otherwise numbers: every run of printable characters in
 * the DLL's writable data is printed, and the errors are among them. */
static void
print_runtime_errors(void)
{
   BYTE *base = (BYTE *)GetModuleHandleA("d3d9.dll");
   IMAGE_NT_HEADERS *nt;
   IMAGE_SECTION_HEADER *section;

   if (!base)
      return;
   nt = (IMAGE_NT_HEADERS *)(base + ((IMAGE_DOS_HEADER *)base)->e_lfanew);
   section = IMAGE_FIRST_SECTION(nt);
   for (unsigned i = 0; i < nt->FileHeader.NumberOfSections; i++, section++) {
      const BYTE *at, *end;
      unsigned printed = 0;

      if (!(section->Characteristics & IMAGE_SCN_MEM_WRITE))
         continue;
      at = base + section->VirtualAddress;
      end = at + section->Misc.VirtualSize;
      while (at < end && printed < 40) {
         const BYTE *run = at;

         while (run < end && *run >= 0x20 && *run < 0x7f)
            run++;
         if (run - at >= 10) {
            printf("  d3d9.dll says: %.*s\n", (int)(run - at), (const char *)at);
            printed++;
         }
         at = run + 1;
      }
   }
}

/* Bring-up only, and only for the d3d9.dll it was read from (10.0.19041.7802,
 * x64; checked by the bytes of one instruction before anything is trusted):
 * what the runtime's enumerator holds for adapter 0 after it has read the
 * driver's caps. IDirect3D9::GetDeviceCaps answers D3DERR_NOTAVAILABLE for a
 * HAL when a "software only" flag is set, or when no format in the list it
 * kept has D3DFORMAT_OP_3DACCELERATION; this prints both. */
static void
print_runtime_state(IDirect3D9 *d3d)
{
#if defined(_WIN64)
   static const BYTE expected[] = {0x83, 0xb9, 0x68, 0x54, 0x00, 0x00, 0x00};
   BYTE *base = (BYTE *)GetModuleHandleA("d3d9.dll");
   BYTE *self = (BYTE *)d3d;
   BYTE *caps, *formats;
   unsigned count;

   if (!base || memcmp(base + 0xf903, expected, sizeof expected) != 0) {
      printf("  (another d3d9.dll build: its state is not read)\n");
      return;
   }
   caps = self + 0x190;
   count = *(unsigned *)(caps + 0x160);
   formats = *(BYTE **)(caps + 0x168);
   printf("  runtime: software-only %u, adapters %u, Caps 0x%08x, DevCaps 0x%08x, "
          "DevCaps2 0x%08x, flags 0x%x, %u formats at %p\n",
          *(unsigned *)(self + 0x5468), *(unsigned *)(self + 0x44), *(unsigned *)(caps + 0x8),
          *(unsigned *)(caps + 0x1c), *(unsigned *)(caps + 0xd4), *(unsigned *)(caps + 0x15c),
          count, (void *)formats);
   printf("           Caps2 0x%08x, Caps3 0x%08x, max texture %u, streams %u, vs 0x%08x, "
          "ps 0x%08x\n",
          *(unsigned *)(caps + 0xc), *(unsigned *)(caps + 0x10), *(unsigned *)(caps + 0x58),
          *(unsigned *)(caps + 0xbc), *(unsigned *)(caps + 0xc4), *(unsigned *)(caps + 0xcc));
   for (unsigned i = 0; formats && i < count && i < 40; i++)
      printf("    format %u ops 0x%08x\n", *(unsigned *)(formats + i * 0x78 + 0x58),
             *(unsigned *)(formats + i * 0x78 + 0x60));
#else
   (void)d3d;
#endif
}

/* ---- the scene (phase N2): what the first frames of any program need -----
 *
 * A triangle from the program's own memory (pre-transformed, coloured: the
 * fixed-function pixel path and DrawPrimitiveUP), and a square from a vertex
 * buffer with a texture, a light and the three transforms (the fixed-function
 * vertex path, a managed texture's upload, a buffer's lock). */

struct scene {
   IDirect3DVertexBuffer9 *square;
   IDirect3DVertexBuffer9 *corner;
   IDirect3DTexture9 *checker;
   HRESULT made;
};

struct lit_vertex {
   float x, y, z;
   float nx, ny, nz;
   float u, v;
};

#define LIT_FVF (D3DFVF_XYZ | D3DFVF_NORMAL | D3DFVF_TEX1)

static void
identity(D3DMATRIX *m)
{
   memset(m, 0, sizeof *m);
   m->_11 = m->_22 = m->_33 = m->_44 = 1.0f;
}

static HRESULT
scene_make(IDirect3DDevice9 *device, struct scene *scene)
{
   static const struct lit_vertex corners[4] = {
      {-0.6f, -0.6f, 0.0f, 0, 0, -1, 0, 1},
      {-0.6f, 0.6f, 0.0f, 0, 0, -1, 0, 0},
      {0.6f, -0.6f, 0.0f, 0, 0, -1, 1, 1},
      {0.6f, 0.6f, 0.0f, 0, 0, -1, 1, 0},
   };
   D3DLOCKED_RECT locked;
   void *data;
   HRESULT hr;

   hr = IDirect3DDevice9_CreateVertexBuffer(device, sizeof corners, D3DUSAGE_WRITEONLY, LIT_FVF,
                                            D3DPOOL_DEFAULT, &scene->square, NULL);
   printf("vertex buffer: 0x%08lx\n", (unsigned long)hr);
   if (FAILED(hr))
      return hr;
   hr = IDirect3DVertexBuffer9_Lock(scene->square, 0, 0, &data, 0);
   printf("vertex buffer lock: 0x%08lx %p\n", (unsigned long)hr, data);
   if (FAILED(hr) || !data)
      return FAILED(hr) ? hr : E_POINTER;
   memcpy(data, corners, sizeof corners);
   IDirect3DVertexBuffer9_Unlock(scene->square);

   {
      /* A pre-transformed triangle that lives in a vertex buffer. */
      static const struct {
         float x, y, z, rhw;
         DWORD color;
      } corner[3] = {
         {60.0f, 300.0f, 0.5f, 1.0f, 0xffffff00},
         {200.0f, 300.0f, 0.5f, 1.0f, 0xffffff00},
         {60.0f, 420.0f, 0.5f, 1.0f, 0xffffff00},
      };

      hr = IDirect3DDevice9_CreateVertexBuffer(device, sizeof corner, D3DUSAGE_WRITEONLY,
                                               D3DFVF_XYZRHW | D3DFVF_DIFFUSE, D3DPOOL_DEFAULT,
                                               &scene->corner, NULL);
      if (SUCCEEDED(hr) && SUCCEEDED(IDirect3DVertexBuffer9_Lock(scene->corner, 0, 0, &data, 0))) {
         memcpy(data, corner, sizeof corner);
         IDirect3DVertexBuffer9_Unlock(scene->corner);
      }
   }

   hr = IDirect3DDevice9_CreateTexture(device, 64, 64, 0, 0, D3DFMT_A8R8G8B8, D3DPOOL_MANAGED,
                                       &scene->checker, NULL);
   printf("texture: 0x%08lx\n", (unsigned long)hr);
   if (FAILED(hr))
      return hr;
   hr = IDirect3DTexture9_LockRect(scene->checker, 0, &locked, NULL, 0);
   printf("texture lock: 0x%08lx %p pitch %d\n", (unsigned long)hr, locked.pBits,
          (int)locked.Pitch);
   if (FAILED(hr) || !locked.pBits)
      return FAILED(hr) ? hr : E_POINTER;
   for (int y = 0; y < 64; y++) {
      DWORD *row = (DWORD *)((char *)locked.pBits + y * locked.Pitch);

      for (int x = 0; x < 64; x++)
         row[x] = ((x >> 3) ^ (y >> 3)) & 1 ? 0xffffffff : 0xffe07020;
   }
   IDirect3DTexture9_UnlockRect(scene->checker, 0);
   return S_OK;
}

static HRESULT
scene_draw(IDirect3DDevice9 *device, struct scene *scene, int frame, int say)
{
   /* x, y, z, rhw, colour: already in window coordinates. */
   struct {
      float x, y, z, rhw;
      DWORD color;
   } triangle[3] = {
      {60.0f, 60.0f, 0.5f, 1.0f, 0xffff0000},
      {260.0f, 60.0f, 0.5f, 1.0f, 0xff00ff00},
      {60.0f, 260.0f, 0.5f, 1.0f, 0xff0000ff},
   };
   D3DMATRIX world, view, projection;
   D3DMATERIAL9 material;
   D3DLIGHT9 light;
   float angle = frame * 0.03f, c = (float)cos(angle), s = (float)sin(angle);
   HRESULT hr;

   IDirect3DDevice9_SetRenderState(device, D3DRS_CULLMODE, D3DCULL_NONE);
   IDirect3DDevice9_SetRenderState(device, D3DRS_ZENABLE, D3DZB_TRUE);

   /* The triangle: no lighting, no texture. */
   IDirect3DDevice9_SetRenderState(device, D3DRS_LIGHTING, FALSE);
   IDirect3DDevice9_SetTexture(device, 0, NULL);
   IDirect3DDevice9_SetFVF(device, D3DFVF_XYZRHW | D3DFVF_DIFFUSE);
   hr = IDirect3DDevice9_DrawPrimitiveUP(device, D3DPT_TRIANGLELIST, 1, triangle,
                                         sizeof triangle[0]);
   if (say)
      printf("triangle: 0x%08lx\n", (unsigned long)hr);

   /* The same kind of triangle from a vertex buffer (yellow, bottom left),
    * and one in clip space from memory (cyan, top right): between the three,
    * which of "from memory" and "pre-transformed" a failure belongs to. */
   if (scene->corner) {
      IDirect3DDevice9_SetStreamSource(device, 0, scene->corner, 0, 20);
      hr = IDirect3DDevice9_DrawPrimitive(device, D3DPT_TRIANGLELIST, 0, 1);
      if (say)
         printf("triangle from a buffer: 0x%08lx\n", (unsigned long)hr);
   }
   {
      struct {
         float x, y, z;
         DWORD color;
      } clip[3] = {
         {0.4f, 0.9f, 0.5f, 0xff00ffff},
         {0.9f, 0.9f, 0.5f, 0xff00ffff},
         {0.9f, 0.4f, 0.5f, 0xff00ffff},
      };
      D3DMATRIX one;

      identity(&one);
      IDirect3DDevice9_SetTransform(device, D3DTS_WORLD, &one);
      IDirect3DDevice9_SetTransform(device, D3DTS_VIEW, &one);
      IDirect3DDevice9_SetTransform(device, D3DTS_PROJECTION, &one);
      IDirect3DDevice9_SetFVF(device, D3DFVF_XYZ | D3DFVF_DIFFUSE);
      hr = IDirect3DDevice9_DrawPrimitiveUP(device, D3DPT_TRIANGLELIST, 1, clip, sizeof clip[0]);
      if (say)
         printf("triangle in clip space: 0x%08lx\n", (unsigned long)hr);
   }

   /* The square: turning about the vertical axis, lit from the front. */
   identity(&world);
   world._11 = c;
   world._13 = -s;
   world._31 = s;
   world._33 = c;
   identity(&view);
   view._43 = 2.5f;
   identity(&projection);
   projection._11 = 1.5f;
   projection._22 = 2.0f;
   projection._33 = 10.0f / 9.9f;
   projection._34 = 1.0f;
   projection._43 = -0.1f * 10.0f / 9.9f;
   projection._44 = 0.0f;
   IDirect3DDevice9_SetTransform(device, D3DTS_WORLD, &world);
   IDirect3DDevice9_SetTransform(device, D3DTS_VIEW, &view);
   IDirect3DDevice9_SetTransform(device, D3DTS_PROJECTION, &projection);

   memset(&material, 0, sizeof material);
   material.Diffuse.r = material.Diffuse.g = material.Diffuse.b = material.Diffuse.a = 1.0f;
   material.Ambient = material.Diffuse;
   IDirect3DDevice9_SetMaterial(device, &material);
   memset(&light, 0, sizeof light);
   light.Type = D3DLIGHT_DIRECTIONAL;
   light.Diffuse.r = light.Diffuse.g = light.Diffuse.b = 1.0f;
   light.Direction.z = 1.0f;
   IDirect3DDevice9_SetLight(device, 0, &light);
   IDirect3DDevice9_LightEnable(device, 0, TRUE);
   IDirect3DDevice9_SetRenderState(device, D3DRS_LIGHTING, TRUE);
   IDirect3DDevice9_SetRenderState(device, D3DRS_AMBIENT, 0x00303030);

   IDirect3DDevice9_SetTexture(device, 0, (IDirect3DBaseTexture9 *)scene->checker);
   IDirect3DDevice9_SetTextureStageState(device, 0, D3DTSS_COLOROP, D3DTOP_MODULATE);
   IDirect3DDevice9_SetTextureStageState(device, 0, D3DTSS_COLORARG1, D3DTA_TEXTURE);
   IDirect3DDevice9_SetTextureStageState(device, 0, D3DTSS_COLORARG2, D3DTA_DIFFUSE);
   IDirect3DDevice9_SetSamplerState(device, 0, D3DSAMP_MINFILTER, D3DTEXF_LINEAR);
   IDirect3DDevice9_SetSamplerState(device, 0, D3DSAMP_MAGFILTER, D3DTEXF_POINT);
   IDirect3DDevice9_SetSamplerState(device, 0, D3DSAMP_MIPFILTER, D3DTEXF_LINEAR);
   IDirect3DDevice9_SetFVF(device, LIT_FVF);
   IDirect3DDevice9_SetStreamSource(device, 0, scene->square, 0, sizeof(struct lit_vertex));
   hr = IDirect3DDevice9_DrawPrimitive(device, D3DPT_TRIANGLESTRIP, 0, 2);
   if (say)
      printf("square: 0x%08lx\n", (unsigned long)hr);

   /* The same square four times along the bottom, small and still: lit and
    * textured, lit only, textured only, neither. Each is a different pair of
    * fixed-function shaders. */
   for (int i = 0; i < 4; i++) {
      identity(&world);
      world._11 = world._22 = 0.25f;
      world._41 = -1.05f + 0.7f * i;
      world._42 = -0.75f;
      IDirect3DDevice9_SetTransform(device, D3DTS_WORLD, &world);
      IDirect3DDevice9_SetRenderState(device, D3DRS_LIGHTING, i < 2);
      IDirect3DDevice9_SetTexture(device, 0,
                                  (i & 1) ? NULL : (IDirect3DBaseTexture9 *)scene->checker);
      IDirect3DDevice9_DrawPrimitive(device, D3DPT_TRIANGLESTRIP, 0, 2);
   }
   return hr;
}

int
main(int argc, char **argv)
{
   int frames = argc > 1 ? atoi(argv[1]) : 60;
   D3DADAPTER_IDENTIFIER9 id;
   D3DPRESENT_PARAMETERS pp;
   IDirect3DDevice9 *device = NULL;
   struct scene scene;
   IDirect3D9 *d3d;
   D3DCAPS9 caps;
   HRESULT hr;
   HWND window;

   printf("d3d9-test, %u-bit\n", (unsigned)(8 * sizeof(void *)));
   d3d = Direct3DCreate9(D3D_SDK_VERSION);
   if (!d3d) {
      printf("Direct3DCreate9: nothing\n");
      return 2;
   }
   /* "d3d9-test <frames> full": the whole screen, exclusively. */
   int full = argc > 2 && strcmp(argv[2], "full") == 0;

   if (argc > 2 && !full)
      print_runtime_state(d3d);
   hr = IDirect3D9_GetAdapterIdentifier(d3d, D3DADAPTER_DEFAULT, 0, &id);
   printf("adapter: 0x%08lx \"%s\" driver \"%s\"\n", (unsigned long)hr,
          SUCCEEDED(hr) ? id.Description : "", SUCCEEDED(hr) ? id.Driver : "");
   {
      D3DDISPLAYMODE mode;

      hr = IDirect3D9_GetAdapterDisplayMode(d3d, D3DADAPTER_DEFAULT, &mode);
      printf("mode: 0x%08lx %ux%u@%u format %u\n", (unsigned long)hr, mode.Width,
             mode.Height, mode.RefreshRate, (unsigned)mode.Format);
      hr = IDirect3D9_CheckDeviceType(d3d, D3DADAPTER_DEFAULT, D3DDEVTYPE_HAL, mode.Format,
                                      D3DFMT_X8R8G8B8, TRUE);
      printf("device type (HAL, windowed, X8R8G8B8 back buffer): 0x%08lx\n",
             (unsigned long)hr);
   }
   hr = IDirect3D9_GetDeviceCaps(d3d, D3DADAPTER_DEFAULT, D3DDEVTYPE_HAL, &caps);
   printf("caps: 0x%08lx", (unsigned long)hr);
   if (SUCCEEDED(hr))
      printf(" vs %lu.%lu ps %lu.%lu, textures up to %lu, devcaps 0x%08lx",
             (caps.VertexShaderVersion >> 8) & 0xff, caps.VertexShaderVersion & 0xff,
             (caps.PixelShaderVersion >> 8) & 0xff, caps.PixelShaderVersion & 0xff,
             caps.MaxTextureWidth, caps.DevCaps);
   printf("\n");

   window = CreateWindowExA(0, "STATIC", "d3d9-test", WS_OVERLAPPEDWINDOW | WS_VISIBLE,
                            100, 100, 640, 480, NULL, NULL, NULL, NULL);
   memset(&pp, 0, sizeof pp);
   pp.Windowed = !full;
   pp.SwapEffect = D3DSWAPEFFECT_DISCARD;
   pp.BackBufferFormat = D3DFMT_X8R8G8B8;
   if (full) {
      D3DDISPLAYMODE mode;

      IDirect3D9_GetAdapterDisplayMode(d3d, D3DADAPTER_DEFAULT, &mode);
      pp.BackBufferWidth = mode.Width;
      pp.BackBufferHeight = mode.Height;
      pp.BackBufferCount = 1;
      pp.FullScreen_RefreshRateInHz = mode.RefreshRate;
      pp.PresentationInterval = D3DPRESENT_INTERVAL_IMMEDIATE;
   }
   pp.EnableAutoDepthStencil = TRUE;
   pp.AutoDepthStencilFormat = D3DFMT_D24S8;
   pp.hDeviceWindow = window;
   hr = IDirect3D9_CreateDevice(d3d, D3DADAPTER_DEFAULT, D3DDEVTYPE_HAL, window,
                                D3DCREATE_HARDWARE_VERTEXPROCESSING, &pp, &device);
   printf("device (hardware vertex processing): 0x%08lx\n", (unsigned long)hr);
   if (FAILED(hr)) {
      hr = IDirect3D9_CreateDevice(d3d, D3DADAPTER_DEFAULT, D3DDEVTYPE_HAL, window,
                                   D3DCREATE_SOFTWARE_VERTEXPROCESSING, &pp, &device);
      printf("device (software vertex processing): 0x%08lx\n", (unsigned long)hr);
   }
   if (FAILED(hr)) {
      print_runtime_errors();
      print_runtime_state(d3d);
      IDirect3D9_Release(d3d);
      return 1;
   }
   memset(&scene, 0, sizeof scene);
   scene.made = scene_make(device, &scene);
   for (int frame = 0; frame < frames; frame++) {
      MSG msg;

      while (PeekMessageA(&msg, NULL, 0, 0, PM_REMOVE))
         DispatchMessageA(&msg);
      hr = IDirect3DDevice9_Clear(device, 0, NULL, D3DCLEAR_TARGET | D3DCLEAR_ZBUFFER,
                                  D3DCOLOR_XRGB(frame * 4 & 0xff, 96, 160), 1.0f, 0);
      if (frame == 0)
         printf("clear: 0x%08lx\n", (unsigned long)hr);
      IDirect3DDevice9_BeginScene(device);
      if (SUCCEEDED(scene.made))
         scene_draw(device, &scene, frame, frame == 0);
      IDirect3DDevice9_EndScene(device);
      if (full && frame == 0) {
         /* What the runtime said to itself while it took the screen. */
         print_runtime_errors();
         fflush(stdout);
      }
      hr = IDirect3DDevice9_Present(device, NULL, NULL, NULL, NULL);
      if (frame == 0)
         printf("present: 0x%08lx\n", (unsigned long)hr);
      Sleep(16);
   }
   if (scene.checker)
      IDirect3DTexture9_Release(scene.checker);
   if (scene.square)
      IDirect3DVertexBuffer9_Release(scene.square);
   if (scene.corner)
      IDirect3DVertexBuffer9_Release(scene.corner);
   IDirect3DDevice9_Release(device);
   IDirect3D9_Release(d3d);
   printf("done\n");
   return 0;
}
