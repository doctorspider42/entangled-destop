// SPDX-License-Identifier: Apache-2.0
#define WIN32_LEAN_AND_MEAN
#include <windows.h>
#include <d3d11.h>
#include <dcomp.h>
#include <dwmapi.h>
#include <wrl/client.h>

#include <cmath>
#include <cstdint>
#include <cstdio>
#include <vector>

using Microsoft::WRL::ComPtr;

namespace {
constexpr int width = 400;
constexpr int height = 384;
constexpr int strips = 24;
constexpr int phase_seconds = 5;

struct Failure {
    const char *call;
    HRESULT status;
};

void check(HRESULT status, const char *call)
{
    if (FAILED(status))
        throw Failure{call, status};
}

long long counter()
{
    LARGE_INTEGER now{};
    QueryPerformanceCounter(&now);
    return now.QuadPart;
}

bool pump_messages()
{
    MSG message{};
    while (PeekMessageW(&message, nullptr, 0, 0, PM_REMOVE)) {
        if (message.message == WM_QUIT)
            return false;
        TranslateMessage(&message);
        DispatchMessageW(&message);
    }
    return true;
}

LRESULT CALLBACK window_proc(HWND window, UINT message, WPARAM wparam, LPARAM lparam)
{
    if (message == WM_DESTROY) {
        PostQuitMessage(0);
        return 0;
    }
    return DefWindowProcW(window, message, wparam, lparam);
}

ComPtr<IDCompositionSurface> make_surface(IDCompositionDevice *composition, ID3D11DeviceContext *context)
{
    ComPtr<IDCompositionSurface> surface;
    check(composition->CreateSurface(width, height, DXGI_FORMAT_B8G8R8A8_UNORM,
                                     DXGI_ALPHA_MODE_IGNORE, &surface), "CreateSurface");
    std::vector<uint32_t> pixels(width * height);
    for (int y = 0; y < height; ++y) {
        for (int x = 0; x < width; ++x) {
            const bool alternate = (x / 32 + y / (height / strips)) % 2 != 0;
            pixels[y * width + x] = alternate ? 0xfff1ba42u : 0xff326c8au;
        }
    }
    POINT offset{};
    {
        ComPtr<IDXGISurface> draw;
        check(surface->BeginDraw(nullptr, IID_PPV_ARGS(&draw), &offset), "BeginDraw");
        ComPtr<ID3D11Texture2D> texture;
        check(draw.As(&texture), "Get draw texture");
        const D3D11_BOX box{static_cast<UINT>(offset.x), static_cast<UINT>(offset.y), 0,
                            static_cast<UINT>(offset.x + width), static_cast<UINT>(offset.y + height), 1};
        context->UpdateSubresource(texture.Get(), 0, &box, pixels.data(), width * sizeof(uint32_t), 0);
        context->Flush();
    }
    check(surface->EndDraw(), "EndDraw");
    return surface;
}

bool phase(IDCompositionDevice *composition, IDCompositionTarget *target, IDCompositionSurface *surface,
           int number, int count, long long frequency)
{
    ComPtr<IDCompositionVisual> root;
    check(composition->CreateVisual(&root), "Create root visual");
    std::vector<ComPtr<IDCompositionVisual>> visuals(count);
    for (int i = 0; i < count; ++i) {
        auto &visual = visuals[i];
        check(composition->CreateVisual(&visual), "Create strip visual");
        check(visual->SetContent(surface), "SetContent");
        const float top = static_cast<float>(i * height / count);
        const float bottom = static_cast<float>((i + 1) * height / count);
        const D2D_RECT_F clip{0, top, width, bottom};
        check(visual->SetClip(clip), "SetClip");
        check(visual->SetOffsetY(50.0f), "SetOffsetY");
        check(root->AddVisual(visual.Get(), FALSE, nullptr), "AddVisual");
    }
    check(target->SetRoot(root.Get()), "SetRoot");
    check(composition->Commit(), "Commit setup");
    check(DwmFlush(), "DwmFlush setup");

    const auto start = counter();
    const auto end = start + frequency * phase_seconds;
    int frame = 0;
    while (pump_messages()) {
        const auto begin = counter();
        if (begin >= end)
            return true;
        const auto seconds = static_cast<float>(begin - start) / static_cast<float>(frequency);
        for (int i = 0; i < count; ++i) {
            const auto offset = 175.0f + 120.0f * std::sin(6.0f * seconds + 0.13f * i);
            check(visuals[i]->SetOffsetX(offset), "SetOffsetX");
        }
        const auto commit = counter();
        check(composition->Commit(), "Commit frame");
        const auto flush = counter();
        const auto status = DwmFlush();
        const auto done = counter();
        printf("%d,%d,%d,%lld,%lld,%lld,%lld,%08lx\n", number, count, ++frame,
               begin, commit, flush, done, static_cast<unsigned long>(status));
        fflush(stdout);
        check(status, "DwmFlush frame");
    }
    return false;
}
} // namespace

int main()
{
    try {
        SetProcessDPIAware();
        if (!SetThreadExecutionState(ES_CONTINUOUS | ES_DISPLAY_REQUIRED))
            fprintf(stderr, "could not keep the display awake\n");
        WNDCLASSW cls{};
        cls.hInstance = GetModuleHandleW(nullptr);
        cls.lpszClassName = L"EntangledDCompCadence";
        cls.lpfnWndProc = window_proc;
        cls.hbrBackground = GetSysColorBrush(COLOR_WINDOW);
        if (!RegisterClassW(&cls))
            check(HRESULT_FROM_WIN32(GetLastError()), "RegisterClassW");
        HWND window = CreateWindowExW(WS_EX_TOPMOST, cls.lpszClassName, L"Entangled DirectComposition cadence",
                                      WS_OVERLAPPEDWINDOW, 340, 195, 800, 550,
                                      nullptr, nullptr, cls.hInstance, nullptr);
        if (!window)
            check(HRESULT_FROM_WIN32(GetLastError()), "CreateWindowExW");

        ComPtr<ID3D11Device> d3d;
        ComPtr<ID3D11DeviceContext> context;
        check(D3D11CreateDevice(nullptr, D3D_DRIVER_TYPE_HARDWARE, nullptr, D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                                nullptr, 0, D3D11_SDK_VERSION, &d3d, nullptr, &context), "D3D11CreateDevice");
        ComPtr<IDXGIDevice> dxgi;
        check(d3d.As(&dxgi), "Get DXGI device");
        ComPtr<IDXGIAdapter> adapter;
        check(dxgi->GetAdapter(&adapter), "Get DXGI adapter");
        DXGI_ADAPTER_DESC adapter_desc{};
        check(adapter->GetDesc(&adapter_desc), "Get DXGI adapter description");
        fprintf(stderr, "adapter=%ls vendor=0x%04x device=0x%04x\n", adapter_desc.Description,
                adapter_desc.VendorId, adapter_desc.DeviceId);
        ComPtr<IDCompositionDevice> composition;
        check(DCompositionCreateDevice(dxgi.Get(), IID_PPV_ARGS(&composition)), "DCompositionCreateDevice");
        ComPtr<IDCompositionTarget> target;
        check(composition->CreateTargetForHwnd(window, TRUE, &target), "CreateTargetForHwnd");
        const auto surface = make_surface(composition.Get(), context.Get());
        if (!SetWindowPos(window, HWND_TOPMOST, 0, 0, 0, 0,
                          SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE | SWP_SHOWWINDOW))
            check(HRESULT_FROM_WIN32(GetLastError()), "Show test window");
        LARGE_INTEGER frequency{};
        QueryPerformanceFrequency(&frequency);
        fprintf(stderr, "qpc_frequency=%lld\n", frequency.QuadPart);
        puts("phase,visuals,frame,qpc_begin,qpc_commit,qpc_flush,qpc_done,hresult");
        for (int i = 0; i < 3; ++i) {
            if (!phase(composition.Get(), target.Get(), surface.Get(), i + 1,
                       i == 1 ? strips : 1, frequency.QuadPart)) {
                fprintf(stderr, "window closed during the measurement\n");
                return 2;
            }
        }
        DestroyWindow(window);
    } catch (const Failure &error) {
        fprintf(stderr, "%s failed (0x%08lx)\n", error.call, static_cast<unsigned long>(error.status));
        return 1;
    }
}
