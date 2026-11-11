/*
 * d3d9-test.exe: what a Direct3D 9 program does first, with every answer
 * printed - whether Windows hands our adapter a Direct3D 9 driver at all
 * (ADR-0011, phase N0), and how far a device gets.
 *
 * usage: d3d9-test [frames]     (prints to stdout; exit code 0 = a device)
 */
#include <windows.h>

#include <d3d9.h>
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

int
main(int argc, char **argv)
{
   int frames = argc > 1 ? atoi(argv[1]) : 60;
   D3DADAPTER_IDENTIFIER9 id;
   D3DPRESENT_PARAMETERS pp;
   IDirect3DDevice9 *device = NULL;
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
   if (argc > 2)
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
   pp.Windowed = TRUE;
   pp.SwapEffect = D3DSWAPEFFECT_DISCARD;
   pp.BackBufferFormat = D3DFMT_X8R8G8B8;
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
   for (int frame = 0; frame < frames; frame++) {
      MSG msg;

      while (PeekMessageA(&msg, NULL, 0, 0, PM_REMOVE))
         DispatchMessageA(&msg);
      hr = IDirect3DDevice9_Clear(device, 0, NULL, D3DCLEAR_TARGET | D3DCLEAR_ZBUFFER,
                                  D3DCOLOR_XRGB(frame * 4 & 0xff, 96, 160), 1.0f, 0);
      if (frame == 0)
         printf("clear: 0x%08lx\n", (unsigned long)hr);
      IDirect3DDevice9_BeginScene(device);
      IDirect3DDevice9_EndScene(device);
      hr = IDirect3DDevice9_Present(device, NULL, NULL, NULL, NULL);
      if (frame == 0)
         printf("present: 0x%08lx\n", (unsigned long)hr);
      Sleep(16);
   }
   IDirect3DDevice9_Release(device);
   IDirect3D9_Release(d3d);
   printf("done\n");
   return 0;
}
