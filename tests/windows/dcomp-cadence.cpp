// SPDX-License-Identifier: Apache-2.0
// Interactive display lab probe for the Windows guest and native reference.
#define WIN32_LEAN_AND_MEAN
#include <windows.h>
#include <d3d11.h>
#include <d3d11_4.h>
#include <dcomp.h>
#include <dwmapi.h>
#include <windows.graphics.capture.interop.h>
#include <windows.graphics.directx.direct3d11.interop.h>
#include <winrt/Windows.Foundation.h>
#include <winrt/Windows.Graphics.Capture.h>
#include <winrt/Windows.Graphics.DirectX.Direct3D11.h>
#include <wrl/client.h>

#include <charconv>
#include <cmath>
#include <cstdint>
#include <cstdio>
#include <cstring>
#include <memory>
#include <vector>

using Microsoft::WRL::ComPtr;
namespace Capture = winrt::Windows::Graphics::Capture;
namespace GraphicsDirectX = winrt::Windows::Graphics::DirectX;

namespace {
constexpr int width = 400;
constexpr int height = 384;
constexpr int strips = 24;
constexpr int phase_seconds = 5;
constexpr int queued_hz = 60;

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

struct Surface {
    ComPtr<IDCompositionSurface> content;
    int width;
    int height;
};

Surface make_surface(IDCompositionDevice *composition, ID3D11DeviceContext *context)
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
    return {surface, width, height};
}

Surface capture_window(HWND source, IDCompositionDevice *composition,
                       ID3D11Device *device, ID3D11DeviceContext *context, IDXGIDevice *dxgi)
{
    struct Apartment {
        Apartment() { winrt::init_apartment(); }
        ~Apartment() { winrt::uninit_apartment(); }
    } apartment;
    ComPtr<ID3D11Multithread> multithread;
    check(context->QueryInterface(IID_PPV_ARGS(&multithread)), "Get ID3D11Multithread");
    multithread->SetMultithreadProtected(TRUE);

    winrt::com_ptr<IInspectable> inspectable;
    check(CreateDirect3D11DeviceFromDXGIDevice(dxgi, inspectable.put()), "Create WinRT D3D11 device");
    const auto capture_device = inspectable.as<GraphicsDirectX::Direct3D11::IDirect3DDevice>();
    const auto factory = winrt::get_activation_factory<Capture::GraphicsCaptureItem, IGraphicsCaptureItemInterop>();
    Capture::GraphicsCaptureItem item{nullptr};
    check(factory->CreateForWindow(source, winrt::guid_of<Capture::IGraphicsCaptureItem>(), winrt::put_abi(item)),
          "Create capture item for HWND");
    const auto pool = Capture::Direct3D11CaptureFramePool::CreateFreeThreaded(
        capture_device, GraphicsDirectX::DirectXPixelFormat::B8G8R8A8UIntNormalized, 2, item.Size());
    const auto session = pool.CreateCaptureSession(item);
    session.IsCursorCaptureEnabled(false);
    const auto began = counter();
    session.StartCapture();
    const auto timeout = GetTickCount64() + 3000;
    auto frame = pool.TryGetNextFrame();
    while (!frame && GetTickCount64() < timeout) {
        Sleep(5);
        frame = pool.TryGetNextFrame();
    }
    if (!frame)
        check(HRESULT_FROM_WIN32(WAIT_TIMEOUT), "Wait for captured frame");

    const auto captured = counter();
    const auto size = frame.ContentSize();
    const auto access = frame.Surface().as<::Windows::Graphics::DirectX::Direct3D11::IDirect3DDxgiInterfaceAccess>();
    ComPtr<ID3D11Texture2D> input;
    check(access->GetInterface(IID_PPV_ARGS(&input)), "Get captured D3D11 texture");
    D3D11_TEXTURE2D_DESC desc{};
    input->GetDesc(&desc);
    if (size.Width <= 0 || size.Height <= 0 || static_cast<UINT>(size.Width) > desc.Width ||
        static_cast<UINT>(size.Height) > desc.Height || desc.Format != DXGI_FORMAT_B8G8R8A8_UNORM)
        check(E_INVALIDARG, "Validate captured texture");
    ComPtr<ID3D11Device> input_device;
    input->GetDevice(&input_device);
    if (input_device.Get() != device)
        check(E_FAIL, "Validate same-device capture");

    ComPtr<IDCompositionSurface> surface;
    check(composition->CreateSurface(size.Width, size.Height, desc.Format, DXGI_ALPHA_MODE_IGNORE, &surface),
          "Create snapshot surface");
    POINT offset{};
    {
        ComPtr<IDXGISurface> draw;
        check(surface->BeginDraw(nullptr, IID_PPV_ARGS(&draw), &offset), "Begin snapshot draw");
        ComPtr<ID3D11Texture2D> texture;
        auto status = draw.As(&texture);
        if (SUCCEEDED(status)) {
            D3D11_TEXTURE2D_DESC output{};
            texture->GetDesc(&output);
            if (offset.x < 0 || offset.y < 0 || offset.x + static_cast<int64_t>(size.Width) > output.Width ||
                offset.y + static_cast<int64_t>(size.Height) > output.Height)
                status = E_INVALIDARG;
        }
        if (SUCCEEDED(status)) {
            const D3D11_BOX box{0, 0, 0, static_cast<UINT>(size.Width), static_cast<UINT>(size.Height), 1};
            context->CopySubresourceRegion(texture.Get(), 0, offset.x, offset.y, 0, input.Get(), 0, &box);
            context->Flush();
        }
        const auto end = surface->EndDraw();
        check(status, "Validate snapshot draw texture");
        check(end, "End snapshot draw");
    }
    check(device->GetDeviceRemovedReason(), "Check snapshot device");
    const auto copied = counter();
    fprintf(stderr, "snapshot hwnd=%p size=%dx%d qpc_start=%lld qpc_frame=%lld qpc_submitted=%lld\n",
            source, size.Width, size.Height, began, captured, copied);
    frame.Close();
    session.Close();
    pool.Close();
    return {surface, size.Width, size.Height};
}

void wait_for_next_tick(HANDLE timer, long long start, long long frame, long long frequency)
{
    const auto next = start + (frame + 1) * frequency / queued_hz;
    const auto left = next - counter();
    if (left <= 0)
        return;
    LARGE_INTEGER due{};
    due.QuadPart = -(left * 10'000'000 / frequency + 1);
    if (!SetWaitableTimer(timer, &due, 0, nullptr, nullptr, FALSE))
        check(HRESULT_FROM_WIN32(GetLastError()), "SetWaitableTimer");
    if (WaitForSingleObject(timer, INFINITE) != WAIT_OBJECT_0)
        check(E_FAIL, "Wait for queued tick");
}

bool phase(IDCompositionDevice *composition, IDCompositionTarget *target, const Surface &surface,
           int number, int count, long long frequency, HANDLE queued_timer)
{
    ComPtr<IDCompositionVisual> root;
    check(composition->CreateVisual(&root), "Create root visual");
    std::vector<ComPtr<IDCompositionVisual>> visuals(count);
    for (int i = 0; i < count; ++i) {
        auto &visual = visuals[i];
        check(composition->CreateVisual(&visual), "Create strip visual");
        check(visual->SetContent(surface.content.Get()), "SetContent");
        const float top = static_cast<float>(i * surface.height / count);
        const float bottom = static_cast<float>((i + 1) * surface.height / count);
        const D2D_RECT_F clip{0, top, static_cast<float>(surface.width), bottom};
        check(visual->SetClip(clip), "SetClip");
        const D2D_MATRIX_3X2_F scale{static_cast<float>(width) / static_cast<float>(surface.width), 0, 0,
                                    static_cast<float>(height) / static_cast<float>(surface.height), 0, 0};
        check(visual->SetTransform(scale), "Scale source");
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
        const HRESULT status = queued_timer ? S_OK : DwmFlush();
        const auto done = queued_timer ? flush : counter();
        printf("%d,%d,%d,%lld,%lld,%lld,%lld,%08lx\n", number, count, ++frame,
               begin, commit, flush, done, static_cast<unsigned long>(status));
        fflush(stdout);
        check(status, "DwmFlush frame");
        if (queued_timer)
            wait_for_next_tick(queued_timer, start, (begin - start) * queued_hz / frequency, frequency);
    }
    return false;
}
} // namespace

int main(int argc, char **argv)
{
    HWND source = nullptr;
    if (argc == 3 && std::strcmp(argv[1], "--snapshot") == 0) {
        const char *start = argv[2];
        const char *end = start + std::strlen(start);
        if (std::strncmp(start, "0x", 2) == 0)
            start += 2;
        uintptr_t value = 0;
        const auto parsed = std::from_chars(start, end, value, 16);
        if (parsed.ec == std::errc{} && parsed.ptr == end)
            source = reinterpret_cast<HWND>(value);
        if (!source || !IsWindow(source)) {
            fprintf(stderr, "--snapshot requires a live hexadecimal HWND\n");
            return 2;
        }
    }
    const bool queued = source || (argc == 2 && std::strcmp(argv[1], "--queued") == 0);
    if (argc != 1 && !queued) {
        fprintf(stderr, "usage: dcomp-cadence.exe [--queued | --snapshot HWND]\n");
        return 2;
    }
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
        HWND window = CreateWindowExW(0, cls.lpszClassName, L"Entangled DirectComposition cadence",
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
        DXGI_ADAPTER_DESC desc{};
        check(adapter->GetDesc(&desc), "Get DXGI adapter description");
        fprintf(stderr, "adapter=%ls vendor=0x%04x device=0x%04x\n", desc.Description, desc.VendorId, desc.DeviceId);
        ComPtr<IDCompositionDevice> composition;
        check(DCompositionCreateDevice(dxgi.Get(), IID_PPV_ARGS(&composition)), "DCompositionCreateDevice");
        ComPtr<IDCompositionTarget> target;
        check(composition->CreateTargetForHwnd(window, TRUE, &target), "CreateTargetForHwnd");
        const auto surface = source ? capture_window(source, composition.Get(), d3d.Get(), context.Get(), dxgi.Get())
                                    : make_surface(composition.Get(), context.Get());
        // Show the probe even when the launcher hides its console via STARTUPINFO.
        if (!SetWindowPos(window, HWND_TOPMOST, 0, 0, 0, 0,
                          SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE | SWP_SHOWWINDOW))
            check(HRESULT_FROM_WIN32(GetLastError()), "Show test window");
        LARGE_INTEGER frequency{};
        QueryPerformanceFrequency(&frequency);
        fprintf(stderr, "qpc_frequency=%lld mode=%s\n", frequency.QuadPart, queued ? "queued" : "flush");
        std::unique_ptr<void, decltype(&CloseHandle)> timer(nullptr, CloseHandle);
        if (queued) {
            timer.reset(CreateWaitableTimerExW(nullptr, nullptr, CREATE_WAITABLE_TIMER_HIGH_RESOLUTION,
                                               TIMER_MODIFY_STATE | SYNCHRONIZE));
            if (!timer)
                check(HRESULT_FROM_WIN32(GetLastError()), "CreateWaitableTimerExW");
        }
        puts("phase,visuals,frame,qpc_begin,qpc_commit,qpc_flush,qpc_done,hresult");
        for (int i = 0; i < 3; ++i) {
            if (!phase(composition.Get(), target.Get(), surface, i + 1,
                       i == 1 ? strips : 1, frequency.QuadPart, timer.get())) {
                fprintf(stderr, "window closed during the measurement\n");
                return 2;
            }
        }
        DestroyWindow(window);
    } catch (const Failure &error) {
        fprintf(stderr, "%s failed (0x%08lx)\n", error.call, static_cast<unsigned long>(error.status));
        return 1;
    } catch (const winrt::hresult_error &error) {
        fprintf(stderr, "Windows Graphics Capture failed (0x%08lx): %ls\n",
                static_cast<unsigned long>(error.code().value), error.message().c_str());
        return 1;
    }
}
