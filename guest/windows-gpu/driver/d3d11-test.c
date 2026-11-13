/* Direct3D 11 through DXVK on our Vulkan stack (ADR-0009 phase D): a device
 * and a flip-model swap chain in a window, 300 frames cleared to a changing
 * colour and presented, and a read-back of the last frame through a staging
 * texture. Run beside DXVK's d3d11.dll and dxgi.dll.
 * Prints "D3D11-TEST PASS ..." or "D3D11-TEST FAIL ...". */
#define COBJMACROS
#include <windows.h>
#include <d3d11.h>
#include <dxgi.h>
#include <stdio.h>

static LRESULT CALLBACK proc(HWND w, UINT m, WPARAM a, LPARAM b)
{
    return DefWindowProcA(w, m, a, b);
}

#define CHECK(what, hr)                                                   \
    do {                                                                  \
        HRESULT check_ = (hr);                                            \
        if (FAILED(check_)) {                                             \
            printf("D3D11-TEST FAIL %s 0x%08lx\n", what, (unsigned long)check_); \
            return 1;                                                     \
        }                                                                 \
    } while (0)

int main(void)
{
    WNDCLASSA wc = {0};
    wc.lpfnWndProc = proc;
    wc.hInstance = GetModuleHandleA(NULL);
    wc.lpszClassName = "d3d11test";
    RegisterClassA(&wc);
    HWND win = CreateWindowA("d3d11test", "d3d11-test", WS_OVERLAPPEDWINDOW | WS_VISIBLE, 100,
                             100, 640, 480, NULL, NULL, wc.hInstance, NULL);

    DXGI_SWAP_CHAIN_DESC sd = {0};
    sd.BufferCount = 2;
    sd.BufferDesc.Format = DXGI_FORMAT_R8G8B8A8_UNORM;
    sd.BufferUsage = DXGI_USAGE_RENDER_TARGET_OUTPUT;
    sd.OutputWindow = win;
    sd.SampleDesc.Count = 1;
    sd.Windowed = TRUE;
    sd.SwapEffect = DXGI_SWAP_EFFECT_FLIP_DISCARD;
    ID3D11Device *dev = NULL;
    ID3D11DeviceContext *ctx = NULL;
    IDXGISwapChain *swap = NULL;
    D3D_FEATURE_LEVEL level = 0;
    /* The device and the swap chain apart, so that a refusal names its step. */
    CHECK("D3D11CreateDevice",
          D3D11CreateDevice(NULL, D3D_DRIVER_TYPE_HARDWARE, NULL, 0, NULL, 0, D3D11_SDK_VERSION,
                            &dev, &level, &ctx));

    IDXGIDevice *dxgi = NULL;
    IDXGIAdapter *adapter = NULL;
    IDXGIFactory *factory = NULL;
    DXGI_ADAPTER_DESC ad = {0};
    CHECK("IDXGIDevice", ID3D11Device_QueryInterface(dev, &IID_IDXGIDevice, (void **)&dxgi));
    CHECK("GetAdapter", IDXGIDevice_GetAdapter(dxgi, &adapter));
    if (SUCCEEDED(IDXGIAdapter_GetDesc(adapter, &ad)))
        printf("adapter  %ls\nfeature level 0x%x\n", ad.Description, level);
    CHECK("GetParent", IDXGIAdapter_GetParent(adapter, &IID_IDXGIFactory, (void **)&factory));
    {
        HRESULT hr = IDXGIFactory_CreateSwapChain(factory, (IUnknown *)dev, &sd, &swap);

        if (FAILED(hr)) {
            /* A driver without the flip model: the copying swap chain. */
            printf("flip-model swap chain: 0x%08lx; the copying one\n", (unsigned long)hr);
            sd.BufferCount = 1;
            sd.SwapEffect = DXGI_SWAP_EFFECT_DISCARD;
            hr = IDXGIFactory_CreateSwapChain(factory, (IUnknown *)dev, &sd, &swap);
        }
        CHECK("CreateSwapChain", hr);
    }

    ID3D11Texture2D *back = NULL;
    ID3D11RenderTargetView *rtv = NULL;
    CHECK("GetBuffer", IDXGISwapChain_GetBuffer(swap, 0, &IID_ID3D11Texture2D, (void **)&back));
    CHECK("CreateRenderTargetView",
          ID3D11Device_CreateRenderTargetView(dev, (ID3D11Resource *)back, NULL, &rtv));

    LARGE_INTEGER f, t0, t1;
    QueryPerformanceFrequency(&f);
    QueryPerformanceCounter(&t0);
    const int frames = 300;
    float last[4] = {0};
    for (int i = 0; i < frames; i++) {
        MSG msg;
        while (PeekMessageA(&msg, NULL, 0, 0, PM_REMOVE))
            DispatchMessageA(&msg);
        float colour[4] = {(i % 100) / 100.0f, 0.5f, 1.0f - (i % 100) / 100.0f, 1.0f};
        ID3D11DeviceContext_OMSetRenderTargets(ctx, 1, &rtv, NULL);
        ID3D11DeviceContext_ClearRenderTargetView(ctx, rtv, colour);
        if (i == frames - 1)
            memcpy(last, colour, sizeof(last));
        if (i < frames - 1)
            CHECK("Present", IDXGISwapChain_Present(swap, 0, 0));
    }

    /* The last frame, before it is presented: read it back. */
    D3D11_TEXTURE2D_DESC td;
    ID3D11Texture2D_GetDesc(back, &td);
    td.Usage = D3D11_USAGE_STAGING;
    td.BindFlags = 0;
    td.CPUAccessFlags = D3D11_CPU_ACCESS_READ;
    td.MiscFlags = 0;
    ID3D11Texture2D *staging = NULL;
    CHECK("CreateTexture2D", ID3D11Device_CreateTexture2D(dev, &td, NULL, &staging));
    ID3D11DeviceContext_CopyResource(ctx, (ID3D11Resource *)staging, (ID3D11Resource *)back);
    D3D11_MAPPED_SUBRESOURCE map;
    CHECK("Map", ID3D11DeviceContext_Map(ctx, (ID3D11Resource *)staging, 0, D3D11_MAP_READ, 0,
                                         &map));
    const unsigned char *px = (const unsigned char *)map.pData;
    unsigned r = px[0], g = px[1], b = px[2];
    ID3D11DeviceContext_Unmap(ctx, (ID3D11Resource *)staging, 0);
    CHECK("Present", IDXGISwapChain_Present(swap, 0, 0));
    QueryPerformanceCounter(&t1);
    double s = (double)(t1.QuadPart - t0.QuadPart) / f.QuadPart;

    unsigned want_r = (unsigned)(last[0] * 255.0f + 0.5f), want_b = (unsigned)(last[2] * 255.0f + 0.5f);
    if (abs((int)r - (int)want_r) > 2 || abs((int)g - 128) > 2 || abs((int)b - (int)want_b) > 2) {
        printf("D3D11-TEST FAIL read back %u,%u,%u, wanted %u,128,%u\n", r, g, b, want_r, want_b);
        return 1;
    }
    printf("D3D11-TEST PASS %d frames in %.2f s (%.1f fps), read back %u,%u,%u\n", frames, s,
           frames / s, r, g, b);
    return 0;
}
