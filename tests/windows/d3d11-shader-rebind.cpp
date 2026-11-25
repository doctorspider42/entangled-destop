// SPDX-License-Identifier: Apache-2.0
// WDDM regression probe. Build with cl /EHsc /O2 d3d11-shader-rebind.cpp
// d3d11.lib d3dcompiler.lib dxgi.lib (on the same command line).
#define WIN32_LEAN_AND_MEAN
#include <windows.h>
#include <d3d11.h>
#include <d3dcompiler.h>
#include <dxgi.h>
#include <wrl/client.h>

#include <cstdio>
#include <cstdlib>
#include <cstring>

using Microsoft::WRL::ComPtr;

static void check(HRESULT result, const char *operation)
{
    if (FAILED(result)) {
        std::fprintf(stderr, "%s failed (0x%08lx)\n", operation, static_cast<unsigned long>(result));
        std::exit(1);
    }
}

static ComPtr<ID3DBlob> compile(const char *source, const char *profile)
{
    ComPtr<ID3DBlob> shader;
    check(D3DCompile(source, std::strlen(source), nullptr, nullptr, nullptr, "main",
                     profile, 0, 0, &shader, nullptr), "D3DCompile");
    return shader;
}

static void check_pixel(ID3D11DeviceContext *context, ID3D11Texture2D *target,
                        ID3D11Texture2D *staging, UINT expected)
{
    context->CopyResource(staging, target);
    D3D11_MAPPED_SUBRESOURCE map{};
    check(context->Map(staging, 0, D3D11_MAP_READ, 0, &map), "Map");
    UINT pixel = 0;
    std::memcpy(&pixel, static_cast<const BYTE *>(map.pData) + 4 * map.RowPitch + 4 * sizeof(pixel), sizeof(pixel));
    context->Unmap(staging, 0);
    if (pixel != expected) {
        std::fprintf(stderr, "Expected pixel %08x, got %08x\n", expected, pixel);
        std::exit(1);
    }
}

static void run(IDXGIAdapter *adapter, ID3DBlob *vertex, ID3DBlob *red,
                ID3DBlob *green, bool draw_replacement)
{
    ComPtr<ID3D11Device> device;
    ComPtr<ID3D11DeviceContext> context;
    check(D3D11CreateDevice(adapter, D3D_DRIVER_TYPE_UNKNOWN, nullptr, 0, nullptr, 0,
                           D3D11_SDK_VERSION, &device, nullptr, &context), "D3D11CreateDevice");

    ComPtr<ID3D11VertexShader> vs;
    ComPtr<ID3D11PixelShader> first, replacement;
    check(device->CreateVertexShader(vertex->GetBufferPointer(), vertex->GetBufferSize(), nullptr, &vs),
          "CreateVertexShader");
    check(device->CreatePixelShader(red->GetBufferPointer(), red->GetBufferSize(), nullptr, &first),
          "Create first pixel shader");
    check(device->CreatePixelShader(green->GetBufferPointer(), green->GetBufferSize(), nullptr, &replacement),
          "Create replacement pixel shader");

    const D3D11_INPUT_ELEMENT_DESC position{"POSITION", 0, DXGI_FORMAT_R32G32_FLOAT, 0, 0,
                                            D3D11_INPUT_PER_VERTEX_DATA, 0};
    ComPtr<ID3D11InputLayout> layout;
    check(device->CreateInputLayout(&position, 1, vertex->GetBufferPointer(), vertex->GetBufferSize(), &layout),
          "CreateInputLayout");
    const float vertices[][2]{{-1, -1}, {-1, 3}, {3, -1}};
    D3D11_BUFFER_DESC vertices_desc{};
    vertices_desc.ByteWidth = sizeof(vertices);
    vertices_desc.Usage = D3D11_USAGE_IMMUTABLE;
    vertices_desc.BindFlags = D3D11_BIND_VERTEX_BUFFER;
    const D3D11_SUBRESOURCE_DATA vertices_data{vertices, 0, 0};
    ComPtr<ID3D11Buffer> vertex_buffer;
    check(device->CreateBuffer(&vertices_desc, &vertices_data, &vertex_buffer), "Create vertex buffer");

    D3D11_TEXTURE2D_DESC desc{};
    desc.Width = desc.Height = 8;
    desc.MipLevels = desc.ArraySize = desc.SampleDesc.Count = 1;
    desc.Format = DXGI_FORMAT_B8G8R8A8_UNORM;
    desc.BindFlags = D3D11_BIND_RENDER_TARGET;
    ComPtr<ID3D11Texture2D> target, staging;
    check(device->CreateTexture2D(&desc, nullptr, &target), "Create render target texture");
    desc.BindFlags = 0;
    desc.CPUAccessFlags = D3D11_CPU_ACCESS_READ;
    desc.Usage = D3D11_USAGE_STAGING;
    check(device->CreateTexture2D(&desc, nullptr, &staging), "Create readback texture");
    ComPtr<ID3D11RenderTargetView> view;
    check(device->CreateRenderTargetView(target.Get(), nullptr, &view), "CreateRenderTargetView");

    D3D11_RASTERIZER_DESC raster_desc{};
    raster_desc.FillMode = D3D11_FILL_SOLID;
    raster_desc.CullMode = D3D11_CULL_NONE;
    raster_desc.DepthClipEnable = TRUE;
    ComPtr<ID3D11RasterizerState> raster;
    check(device->CreateRasterizerState(&raster_desc, &raster), "CreateRasterizerState");
    const D3D11_VIEWPORT viewport{0, 0, 8, 8, 0, 1};
    context->RSSetState(raster.Get());
    context->RSSetViewports(1, &viewport);
    ID3D11RenderTargetView *views[] = {view.Get()};
    context->OMSetRenderTargets(1, views, nullptr);
    context->IASetPrimitiveTopology(D3D11_PRIMITIVE_TOPOLOGY_TRIANGLELIST);
    context->IASetInputLayout(layout.Get());
    ID3D11Buffer *buffers[] = {vertex_buffer.Get()};
    const UINT stride = sizeof(vertices[0]), offset = 0;
    context->IASetVertexBuffers(0, 1, buffers, &stride, &offset);
    context->VSSetShader(vs.Get(), nullptr, 0);
    context->PSSetShader(first.Get(), nullptr, 0);
    context->Draw(3, 0);
    check_pixel(context.Get(), target.Get(), staging.Get(), 0xffff0000);

    // Release the previously drawn shader after binding its replacement.
    // Complete readbacks without drawing so the replacement has no program yet.
    context->PSSetShader(replacement.Get(), nullptr, 0);
    first.Reset();
    for (int i = 0; i < 8; ++i)
        check_pixel(context.Get(), target.Get(), staging.Get(), 0xffff0000);

    if (draw_replacement) {
        context->Draw(3, 0);
        check_pixel(context.Get(), target.Get(), staging.Get(), 0xff00ff00);
    }
    // Leave the replacement bound through device destruction.
}

int main()
{
    ComPtr<IDXGIFactory1> factory;
    check(CreateDXGIFactory1(IID_PPV_ARGS(&factory)), "CreateDXGIFactory1");
    ComPtr<IDXGIAdapter1> adapter;
    for (UINT index = 0; ; ++index) {
        const HRESULT result = factory->EnumAdapters1(index, adapter.ReleaseAndGetAddressOf());
        if (result == DXGI_ERROR_NOT_FOUND) {
            std::fputs("Entangled WDDM adapter unavailable\n", stderr);
            return 1;
        }
        check(result, "EnumAdapters1");
        DXGI_ADAPTER_DESC1 desc{};
        check(adapter->GetDesc1(&desc), "GetDesc1");
        if (desc.VendorId == 0x1af4 && desc.DeviceId == 0x1050 && !(desc.Flags & DXGI_ADAPTER_FLAG_SOFTWARE))
            break;
    }
    const auto vs = compile("float4 main(float2 p : POSITION) : SV_Position { return float4(p, 0, 1); }", "vs_4_0");
    const auto red = compile("float4 main() : SV_Target { return float4(1, 0, 0, 1); }", "ps_4_0");
    const auto green = compile("float4 main() : SV_Target { return float4(0, 1, 0, 1); }", "ps_4_0");
    for (int i = 0; i < 6; ++i) {
        std::printf("Iteration %d: destroy before drawing the replacement\n", i + 1);
        std::fflush(stdout);
        run(adapter.Get(), vs.Get(), red.Get(), green.Get(), false);
        std::printf("Iteration %d: draw the replacement and verify pixels\n", i + 1);
        std::fflush(stdout);
        run(adapter.Get(), vs.Get(), red.Get(), green.Get(), true);
    }
    std::puts("PASS: 12 shader replacement and teardown trials");
}
