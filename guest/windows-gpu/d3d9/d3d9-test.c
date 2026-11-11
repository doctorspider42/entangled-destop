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
