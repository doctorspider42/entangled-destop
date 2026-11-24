/* SPDX-License-Identifier: Apache-2.0 */
#include <windows.h>
#include <dcomp.h>
#include <d3d12.h>
#include <d3dcompiler.h>
#include <d3dkmthk.h>
#include <dwmapi.h>
#include <dxgi1_4.h>
#include <wrl/client.h>

#include <array>
#include <chrono>
#include <cstdint>
#include <cstdio>
#include <cstdlib>
#include <cstring>
#include <cwchar>

using Microsoft::WRL::ComPtr;

void verify_wddm_fence(LUID luid, bool synchronization_only);

static void check(HRESULT result, const char *operation)
{
    if (FAILED(result)) {
        std::fprintf(stderr, "D3D12 FAIL: %s (0x%08lX)\n", operation,
                     static_cast<unsigned long>(result));
        std::exit(1);
    }
}

static void print_wddm_info(LUID luid)
{
    D3DKMT_OPENADAPTERFROMLUID open = {};
    open.AdapterLuid = luid;
    const NTSTATUS open_status = D3DKMTOpenAdapterFromLuid(&open);
    if (open_status < 0) {
        std::wprintf(L"WDDM lookup: open adapter returned 0x%08lX\n",
                     static_cast<unsigned long>(open_status));
        return;
    }

    D3DKMT_QUERYADAPTERINFO query = {};
    query.hAdapter = open.hAdapter;
    D3DKMT_DRIVERVERSION version = {};
    query.Type = KMTQAITYPE_DRIVERVERSION;
    query.pPrivateDriverData = &version;
    query.PrivateDriverDataSize = sizeof(version);
    const NTSTATUS version_status = D3DKMTQueryAdapterInfo(&query);
    if (version_status < 0) {
        std::wprintf(L"WDDM version query returned 0x%08lX\n", static_cast<unsigned long>(version_status));
    } else {
        std::wprintf(L"WDDM version: %u\n", static_cast<unsigned>(version));
    }

    D3DKMT_UMDFILENAMEINFO name = {};
    name.Version = KMTUMDVERSION_DX12;
    query.Type = KMTQAITYPE_UMDRIVERNAME;
    query.pPrivateDriverData = &name;
    query.PrivateDriverDataSize = sizeof(name);
    const NTSTATUS query_status = D3DKMTQueryAdapterInfo(&query);
    const D3DKMT_CLOSEADAPTER close = {open.hAdapter};
    D3DKMTCloseAdapter(&close);
    if (query_status < 0) {
        std::wprintf(L"D3D12 UMD lookup: query returned 0x%08lX\n",
                     static_cast<unsigned long>(query_status));
    } else {
        std::wprintf(L"D3D12 UMD: %.*ls\n", MAX_PATH, name.UmdFileName);
    }
}

struct Adapter {
    ComPtr<IDXGIAdapter1> dxgi;
    LUID luid;
};

static Adapter select_adapter(bool hardware_control)
{
    ComPtr<IDXGIFactory1> factory;
    check(CreateDXGIFactory1(IID_PPV_ARGS(&factory)), "CreateDXGIFactory1");
    constexpr wchar_t entangled_name[] = L"Entangled GPU (virtio-gpu, WDDM)";
    for (UINT index = 0; ; ++index) {
        ComPtr<IDXGIAdapter1> adapter;
        const HRESULT result = factory->EnumAdapters1(index, &adapter);
        if (result == DXGI_ERROR_NOT_FOUND)
            break;
        check(result, "EnumAdapters1");

        DXGI_ADAPTER_DESC1 desc = {};
        check(adapter->GetDesc1(&desc), "GetDesc1");
        if (desc.Flags & DXGI_ADAPTER_FLAG_SOFTWARE)
            continue;
        if (!hardware_control && std::wcscmp(desc.Description, entangled_name) != 0)
            continue;
        if (hardware_control && FAILED(D3D12CreateDevice(adapter.Get(), D3D_FEATURE_LEVEL_11_0,
                                                         __uuidof(ID3D12Device), nullptr)))
            continue;

        std::wprintf(L"DXGI adapter: %ls (PCI %04X:%04X, LUID %08lX:%08lX)\n", desc.Description,
                     desc.VendorId, desc.DeviceId, static_cast<unsigned long>(desc.AdapterLuid.HighPart),
                     desc.AdapterLuid.LowPart);
        print_wddm_info(desc.AdapterLuid);
        std::fflush(stdout);
        return {adapter, desc.AdapterLuid};
    }
    check(DXGI_ERROR_NOT_FOUND, hardware_control ? "find a hardware D3D12 adapter" :
                                                 "find the Entangled WDDM adapter");
    return {};
}

static ComPtr<ID3D12Device> create_device(const Adapter &adapter, bool hardware_control)
{
    ComPtr<ID3D12Device> device;
    check(D3D12CreateDevice(adapter.dxgi.Get(), D3D_FEATURE_LEVEL_11_0, IID_PPV_ARGS(&device)),
          "D3D12CreateDevice (feature level 11_0)");
    const auto actual_luid = device->GetAdapterLuid();
    if (actual_luid.LowPart != adapter.luid.LowPart || actual_luid.HighPart != adapter.luid.HighPart)
        check(E_FAIL, "device LUID differs from the explicit DXGI adapter");
    if (!hardware_control && !GetModuleHandleW(L"entangled_d3d12.dll"))
        check(E_FAIL, "Entangled D3D12 UMD is missing from the runtime process");
    return device;
}

static ComPtr<ID3D12Resource> buffer(ID3D12Device *device, UINT64 size, D3D12_HEAP_TYPE type,
                                     D3D12_RESOURCE_STATES state)
{
    D3D12_HEAP_PROPERTIES heap = {type, D3D12_CPU_PAGE_PROPERTY_UNKNOWN,
                                  D3D12_MEMORY_POOL_UNKNOWN, 1, 1};
    D3D12_RESOURCE_DESC resource = {};
    resource.Dimension = D3D12_RESOURCE_DIMENSION_BUFFER;
    resource.Width = size;
    resource.Height = 1;
    resource.DepthOrArraySize = 1;
    resource.MipLevels = 1;
    resource.SampleDesc.Count = 1;
    resource.Layout = D3D12_TEXTURE_LAYOUT_ROW_MAJOR;
    ComPtr<ID3D12Resource> result;
    check(device->CreateCommittedResource(&heap, D3D12_HEAP_FLAG_NONE, &resource, state, nullptr,
                                          IID_PPV_ARGS(&result)), "CreateCommittedResource (buffer)");
    return result;
}

struct Recording {
    ComPtr<ID3D12CommandQueue> queue;
    ComPtr<ID3D12CommandAllocator> allocator;
    ComPtr<ID3D12GraphicsCommandList> list;
};

static Recording begin_recording(ID3D12Device *device)
{
    D3D12_COMMAND_QUEUE_DESC queue_desc = {};
    queue_desc.Type = D3D12_COMMAND_LIST_TYPE_DIRECT;
    Recording commands;
    check(device->CreateCommandQueue(&queue_desc, IID_PPV_ARGS(&commands.queue)), "CreateCommandQueue");
    check(device->CreateCommandAllocator(queue_desc.Type, IID_PPV_ARGS(&commands.allocator)), "CreateCommandAllocator");
    check(device->CreateCommandList(0, queue_desc.Type, commands.allocator.Get(), nullptr, IID_PPV_ARGS(&commands.list)),
          "CreateCommandList");
    return commands;
}

static long long submit_and_wait(ID3D12Device *device, const Recording &commands)
{
    ComPtr<ID3D12Fence> fence;
    check(device->CreateFence(0, D3D12_FENCE_FLAG_NONE, IID_PPV_ARGS(&fence)), "CreateFence");
    check(commands.list->Close(), "Close command list");

    const HANDLE completed = CreateEventW(nullptr, FALSE, FALSE, nullptr);
    if (!completed)
        check(HRESULT_FROM_WIN32(GetLastError()), "CreateEvent");
    check(fence->SetEventOnCompletion(1, completed), "SetEventOnCompletion");
    ID3D12CommandList *const lists[] = {commands.list.Get()};
    const auto start = std::chrono::steady_clock::now();
    commands.queue->ExecuteCommandLists(1, lists);
    check(commands.queue->Signal(fence.Get(), 1), "Signal fence");
    const DWORD wait = WaitForSingleObject(completed, 10000);
    const auto elapsed = std::chrono::steady_clock::now() - start;
    if (wait == WAIT_FAILED)
        check(HRESULT_FROM_WIN32(GetLastError()), "wait for queue fence");
    if (wait != WAIT_OBJECT_0)
        check(HRESULT_FROM_WIN32(ERROR_TIMEOUT), "wait for queue fence");
    CloseHandle(completed);
    check(device->GetDeviceRemovedReason(), "device after fence completion");
    if (fence->GetCompletedValue() != 1)
        check(E_FAIL, "fence value differs from the queued signal");
    return static_cast<long long>(std::chrono::duration_cast<std::chrono::microseconds>(elapsed).count());
}

static void transition(ID3D12GraphicsCommandList *commands, ID3D12Resource *resource,
                       D3D12_RESOURCE_STATES before, D3D12_RESOURCE_STATES after)
{
    D3D12_RESOURCE_BARRIER barrier = {};
    barrier.Type = D3D12_RESOURCE_BARRIER_TYPE_TRANSITION;
    barrier.Transition = {resource, D3D12_RESOURCE_BARRIER_ALL_SUBRESOURCES, before, after};
    commands->ResourceBarrier(1, &barrier);
}

static void copy_and_verify(ID3D12Device *device, bool device_local)
{
    std::array<uint8_t, 64 * 1024> pattern;
    for (size_t i = 0; i < pattern.size(); ++i)
        pattern[i] = static_cast<uint8_t>(i * 37 + i / 251 + 1);
    const auto source = buffer(device, pattern.size(), D3D12_HEAP_TYPE_UPLOAD, D3D12_RESOURCE_STATE_GENERIC_READ);
    const auto target = buffer(device, pattern.size(), D3D12_HEAP_TYPE_READBACK, D3D12_RESOURCE_STATE_COPY_DEST);
    const D3D12_RANGE empty = {};
    const D3D12_RANGE whole = {0, pattern.size()};
    void *mapped = nullptr;
    check(source->Map(0, &empty, &mapped), "Map upload buffer");
    std::memcpy(mapped, pattern.data(), pattern.size());
    source->Unmap(0, &whole);

    const auto commands = begin_recording(device);
    ComPtr<ID3D12Resource> intermediate;
    if (device_local) {
        intermediate = buffer(device, pattern.size(), D3D12_HEAP_TYPE_DEFAULT, D3D12_RESOURCE_STATE_COPY_DEST);
        if (!intermediate->GetGPUVirtualAddress())
            check(E_FAIL, "device-local buffer has no GPU virtual address");
        commands.list->CopyBufferRegion(intermediate.Get(), 0, source.Get(), 0, pattern.size());
        transition(commands.list.Get(), intermediate.Get(), D3D12_RESOURCE_STATE_COPY_DEST, D3D12_RESOURCE_STATE_COPY_SOURCE);
    }
    commands.list->CopyBufferRegion(target.Get(), 0, intermediate ? intermediate.Get() : source.Get(), 0, pattern.size());
    const auto elapsed = submit_and_wait(device, commands);
    check(target->Map(0, &whole, &mapped), "Map readback buffer");
    const bool identical = std::memcmp(mapped, pattern.data(), pattern.size()) == 0;
    target->Unmap(0, &empty);
    if (!identical)
        check(E_FAIL, "readback differs from upload");
    std::wprintf(L"D3D12 %lsCOPY PASS: %zu verified bytes, submit-to-fence %lld us\n",
                 device_local ? L"DEVICE-LOCAL " : L"", pattern.size(), elapsed);
}

static ComPtr<ID3DBlob> compile_shader(const char *source, const char *target)
{
    ComPtr<ID3DBlob> bytecode, messages;
    const HRESULT status = D3DCompile(source, std::strlen(source), nullptr, nullptr, nullptr, "main", target,
                                      D3DCOMPILE_ENABLE_STRICTNESS, 0, &bytecode, &messages);
    if (FAILED(status) && messages)
        std::fputs(static_cast<const char *>(messages->GetBufferPointer()), stderr);
    check(status, "D3DCompile");
    return bytecode;
}

struct DrawPipeline {
    ComPtr<ID3D12RootSignature> root;
    ComPtr<ID3D12PipelineState> pipeline;
};

enum class ColorWork { Clear, Draw, Uniform, Upload, Sample };

static DrawPipeline create_draw_pipeline(ID3D12Device *device, DXGI_FORMAT target_format, ColorWork work = ColorWork::Draw)
{
    constexpr char vertex_source[] = R"(
        float4 main(uint vertex : SV_VertexID) : SV_Position {
            float2 p = float2(vertex == 1 ? 3 : -1, vertex == 2 ? 3 : -1);
            return float4(p, 0, 1);
        }
    )";
    constexpr char pixel_source[] = R"(
        float4 main() : SV_Target0 { return float4(0, 1, 0, 1); }
    )";
    constexpr char uniform_source[] = R"(
        cbuffer Tint : register(b0) { float4 tint; };
        float4 main() : SV_Target0 { return tint; }
    )";
    constexpr char sampled_source[] = R"(
        Texture2D<float4> image : register(t0);
        SamplerState point_sampler : register(s0);
        float4 main(float4 position : SV_Position) : SV_Target0 {
            return image.SampleLevel(point_sampler, position.xy / float2(73, 41), 0);
        }
    )";
    const bool uniform = work == ColorWork::Uniform;
    const bool sampled = work == ColorWork::Sample;
    const auto vertex = compile_shader(vertex_source, "vs_5_1");
    const auto pixel = compile_shader(uniform ? uniform_source : sampled ? sampled_source : pixel_source, "ps_5_1");
    D3D12_VERSIONED_ROOT_SIGNATURE_DESC versioned_signature = {};
    versioned_signature.Version = D3D_ROOT_SIGNATURE_VERSION_1_1;
    versioned_signature.Desc_1_1 = {};
    const D3D12_DESCRIPTOR_RANGE1 ranges[] = {
        {uniform ? D3D12_DESCRIPTOR_RANGE_TYPE_CBV : D3D12_DESCRIPTOR_RANGE_TYPE_SRV, 1, 0, 0,
         D3D12_DESCRIPTOR_RANGE_FLAG_DATA_STATIC, 0},
        {D3D12_DESCRIPTOR_RANGE_TYPE_SAMPLER, 1, 0, 0, D3D12_DESCRIPTOR_RANGE_FLAG_NONE, 0},
    };
    D3D12_ROOT_PARAMETER1 parameters[2] = {};
    for (UINT i = 0; i < ARRAYSIZE(parameters); ++i) {
        parameters[i].ParameterType = D3D12_ROOT_PARAMETER_TYPE_DESCRIPTOR_TABLE;
        parameters[i].DescriptorTable = {1, &ranges[i]};
        parameters[i].ShaderVisibility = D3D12_SHADER_VISIBILITY_PIXEL;
    }
    versioned_signature.Desc_1_1.NumParameters = uniform ? 1 : sampled ? 2 : 0;
    versioned_signature.Desc_1_1.pParameters = versioned_signature.Desc_1_1.NumParameters ? parameters : nullptr;
    ComPtr<ID3DBlob> root_bytes;
    check(D3D12SerializeVersionedRootSignature(&versioned_signature, &root_bytes, nullptr),
          "D3D12SerializeVersionedRootSignature");
    DrawPipeline draw;
    check(device->CreateRootSignature(0, root_bytes->GetBufferPointer(), root_bytes->GetBufferSize(),
                                      IID_PPV_ARGS(&draw.root)), "CreateRootSignature");

    D3D12_GRAPHICS_PIPELINE_STATE_DESC pipeline = {};
    pipeline.pRootSignature = draw.root.Get();
    pipeline.VS = {vertex->GetBufferPointer(), vertex->GetBufferSize()};
    pipeline.PS = {pixel->GetBufferPointer(), pixel->GetBufferSize()};
    auto &target = pipeline.BlendState.RenderTarget[0];
    target.SrcBlend = D3D12_BLEND_ONE;
    target.DestBlend = D3D12_BLEND_ZERO;
    target.BlendOp = D3D12_BLEND_OP_ADD;
    target.SrcBlendAlpha = D3D12_BLEND_ONE;
    target.DestBlendAlpha = D3D12_BLEND_ZERO;
    target.BlendOpAlpha = D3D12_BLEND_OP_ADD;
    target.LogicOp = D3D12_LOGIC_OP_NOOP;
    target.RenderTargetWriteMask = D3D12_COLOR_WRITE_ENABLE_ALL;
    pipeline.SampleMask = UINT_MAX;
    pipeline.RasterizerState.FillMode = D3D12_FILL_MODE_SOLID;
    pipeline.RasterizerState.CullMode = D3D12_CULL_MODE_NONE;
    pipeline.RasterizerState.DepthClipEnable = TRUE;
    pipeline.DepthStencilState.DepthFunc = D3D12_COMPARISON_FUNC_ALWAYS;
    pipeline.PrimitiveTopologyType = D3D12_PRIMITIVE_TOPOLOGY_TYPE_TRIANGLE;
    pipeline.NumRenderTargets = 1;
    pipeline.RTVFormats[0] = target_format;
    pipeline.SampleDesc.Count = 1;
    check(device->CreateGraphicsPipelineState(&pipeline, IID_PPV_ARGS(&draw.pipeline)), "CreateGraphicsPipelineState");
    return draw;
}

struct DrawBindings {
    ComPtr<ID3D12Resource> constant_buffer;
    ComPtr<ID3D12DescriptorHeap> heap;
    D3D12_GPU_DESCRIPTOR_HANDLE table;
    ComPtr<ID3D12DescriptorHeap> sampler_heap;
    D3D12_GPU_DESCRIPTOR_HANDLE sampler_table;
};

static DrawBindings tint_bindings(ID3D12Device *device)
{
    constexpr UINT block = D3D12_CONSTANT_BUFFER_DATA_PLACEMENT_ALIGNMENT;
    const FLOAT tint[] = {1.0f, 0.0f, 1.0f, 1.0f};
    DrawBindings uniform = {};
    uniform.constant_buffer = buffer(device, 2 * block, D3D12_HEAP_TYPE_UPLOAD, D3D12_RESOURCE_STATE_GENERIC_READ);
    const D3D12_RANGE empty = {}, tint_range = {block, block + sizeof(tint)};
    void *mapped = nullptr;
    check(uniform.constant_buffer->Map(0, &empty, &mapped), "Map shader constants");
    std::memcpy(static_cast<uint8_t *>(mapped) + block, tint, sizeof(tint));
    uniform.constant_buffer->Unmap(0, &tint_range);

    const D3D12_DESCRIPTOR_HEAP_DESC heap = {D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV, 2,
        D3D12_DESCRIPTOR_HEAP_FLAG_SHADER_VISIBLE, 0};
    check(device->CreateDescriptorHeap(&heap, IID_PPV_ARGS(&uniform.heap)), "CreateDescriptorHeap (shader-visible CBV)");
    const UINT stride = device->GetDescriptorHandleIncrementSize(heap.Type);
    auto cpu = uniform.heap->GetCPUDescriptorHandleForHeapStart();
    uniform.table = uniform.heap->GetGPUDescriptorHandleForHeapStart();
    cpu.ptr += stride;
    uniform.table.ptr += stride;
    const D3D12_CONSTANT_BUFFER_VIEW_DESC descriptor = {uniform.constant_buffer->GetGPUVirtualAddress() + block, block};
    device->CreateConstantBufferView(&descriptor, cpu);
    return uniform;
}

static DrawBindings texture_bindings(ID3D12Device *device, ID3D12Resource *texture)
{
    DrawBindings bindings = {};
    D3D12_DESCRIPTOR_HEAP_DESC heap = {D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV, 1,
        D3D12_DESCRIPTOR_HEAP_FLAG_SHADER_VISIBLE, 0};
    check(device->CreateDescriptorHeap(&heap, IID_PPV_ARGS(&bindings.heap)), "CreateDescriptorHeap (sampled texture)");
    D3D12_SHADER_RESOURCE_VIEW_DESC srv = {};
    srv.Format = DXGI_FORMAT_R8G8B8A8_UNORM;
    srv.ViewDimension = D3D12_SRV_DIMENSION_TEXTURE2D;
    srv.Shader4ComponentMapping = D3D12_DEFAULT_SHADER_4_COMPONENT_MAPPING;
    srv.Texture2D.MipLevels = 1;
    device->CreateShaderResourceView(texture, &srv, bindings.heap->GetCPUDescriptorHandleForHeapStart());
    bindings.table = bindings.heap->GetGPUDescriptorHandleForHeapStart();

    heap.Type = D3D12_DESCRIPTOR_HEAP_TYPE_SAMPLER;
    check(device->CreateDescriptorHeap(&heap, IID_PPV_ARGS(&bindings.sampler_heap)), "CreateDescriptorHeap (sampler)");
    D3D12_SAMPLER_DESC sampler = {};
    sampler.Filter = D3D12_FILTER_MIN_MAG_MIP_POINT;
    sampler.AddressU = sampler.AddressV = sampler.AddressW = D3D12_TEXTURE_ADDRESS_MODE_CLAMP;
    sampler.MaxAnisotropy = 1;
    sampler.ComparisonFunc = D3D12_COMPARISON_FUNC_ALWAYS;
    sampler.MaxLOD = D3D12_FLOAT32_MAX;
    device->CreateSampler(&sampler, bindings.sampler_heap->GetCPUDescriptorHandleForHeapStart());
    bindings.sampler_table = bindings.sampler_heap->GetGPUDescriptorHandleForHeapStart();
    return bindings;
}

static void record_draw(ID3D12GraphicsCommandList *commands, const DrawPipeline &draw,
                         D3D12_CPU_DESCRIPTOR_HANDLE rtv, UINT width, UINT height, const DrawBindings *bindings = nullptr)
{
    const D3D12_VIEWPORT viewport = {0, 0, static_cast<FLOAT>(width), static_cast<FLOAT>(height), 0, 1};
    const D3D12_RECT scissor = {0, 0, static_cast<LONG>(width), static_cast<LONG>(height)};
    commands->SetGraphicsRootSignature(draw.root.Get());
    commands->SetPipelineState(draw.pipeline.Get());
    if (bindings) {
        ID3D12DescriptorHeap *const heaps[] = {bindings->heap.Get(), bindings->sampler_heap.Get()};
        commands->SetDescriptorHeaps(bindings->sampler_heap ? 2 : 1, heaps);
        commands->SetGraphicsRootDescriptorTable(0, bindings->table);
        if (bindings->sampler_heap)
            commands->SetGraphicsRootDescriptorTable(1, bindings->sampler_table);
    }
    commands->IASetPrimitiveTopology(D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST);
    commands->RSSetViewports(1, &viewport);
    commands->RSSetScissorRects(1, &scissor);
    commands->OMSetRenderTargets(1, &rtv, FALSE, nullptr);
    commands->DrawInstanced(3, 1, 0, 0);
}

static ComPtr<ID3D12DescriptorHeap> create_rtv_heap(ID3D12Device *device)
{
    D3D12_DESCRIPTOR_HEAP_DESC desc = {};
    desc.Type = D3D12_DESCRIPTOR_HEAP_TYPE_RTV;
    desc.NumDescriptors = 1;
    ComPtr<ID3D12DescriptorHeap> heap;
    check(device->CreateDescriptorHeap(&desc, IID_PPV_ARGS(&heap)), "CreateDescriptorHeap (RTV)");
    return heap;
}

static std::array<uint8_t, 4> uploaded_pixel(UINT x, UINT y)
{
    return {static_cast<uint8_t>(x * 13 + y * 7), static_cast<uint8_t>(y * 17),
            static_cast<uint8_t>(x ^ y), 255};
}

static void color_and_verify(ID3D12Device *device, ColorWork work)
{
    constexpr UINT width = 73, height = 41;
    const bool upload_texture = work == ColorWork::Upload;
    const bool sample_texture = work == ColorWork::Sample;
    const bool use_uniform = work == ColorWork::Uniform;
    const bool programmable = work == ColorWork::Draw || use_uniform || sample_texture;
    const D3D12_HEAP_PROPERTIES video_memory = {D3D12_HEAP_TYPE_DEFAULT, D3D12_CPU_PAGE_PROPERTY_UNKNOWN,
                                                D3D12_MEMORY_POOL_UNKNOWN, 1, 1};
    D3D12_RESOURCE_DESC desc = {};
    desc.Dimension = D3D12_RESOURCE_DIMENSION_TEXTURE2D;
    desc.Width = width;
    desc.Height = height;
    desc.DepthOrArraySize = 1;
    desc.MipLevels = 1;
    desc.Format = DXGI_FORMAT_R8G8B8A8_UNORM;
    desc.SampleDesc.Count = 1;
    desc.Flags = upload_texture ? D3D12_RESOURCE_FLAG_NONE : D3D12_RESOURCE_FLAG_ALLOW_RENDER_TARGET;

    ComPtr<ID3D12Resource> target;
    const D3D12_RESOURCE_STATES write_state = upload_texture
        ? D3D12_RESOURCE_STATE_COPY_DEST : D3D12_RESOURCE_STATE_RENDER_TARGET;
    check(device->CreateCommittedResource(&video_memory, D3D12_HEAP_FLAG_NONE, &desc,
                                          write_state, nullptr, IID_PPV_ARGS(&target)), "CreateCommittedResource (color texture)");
    ComPtr<ID3D12DescriptorHeap> rtv_heap;
    D3D12_CPU_DESCRIPTOR_HANDLE rtv = {};
    if (!upload_texture) {
        rtv_heap = create_rtv_heap(device);
        rtv = rtv_heap->GetCPUDescriptorHandleForHeapStart();
        device->CreateRenderTargetView(target.Get(), nullptr, rtv);
    }

    D3D12_PLACED_SUBRESOURCE_FOOTPRINT footprint = {};
    UINT rows = 0;
    UINT64 row_size = 0, bytes = 0;
    device->GetCopyableFootprints(&desc, 0, 1, 0, &footprint, &rows, &row_size, &bytes);
    if (rows != height || row_size != width * 4 || bytes > SIZE_MAX || !bytes)
        check(E_FAIL, "unexpected render-target copy footprint");
    const auto readback = buffer(device, bytes, D3D12_HEAP_TYPE_READBACK, D3D12_RESOURCE_STATE_COPY_DEST);

    const auto commands = begin_recording(device);
    DrawPipeline draw;
    DrawBindings bindings = {};
    ComPtr<ID3D12Resource> upload;
    ComPtr<ID3D12Resource> sampled;
    D3D12_TEXTURE_COPY_LOCATION src = {}, dst = {};
    src.pResource = target.Get();
    src.Type = D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX;
    dst.pResource = readback.Get();
    dst.Type = D3D12_TEXTURE_COPY_TYPE_PLACED_FOOTPRINT;
    dst.PlacedFootprint = footprint;
    const D3D12_RANGE whole = {0, static_cast<SIZE_T>(bytes)}, empty = {};
    void *mapped = nullptr;
    if (upload_texture || sample_texture) {
        upload = buffer(device, bytes, D3D12_HEAP_TYPE_UPLOAD, D3D12_RESOURCE_STATE_GENERIC_READ);
        check(upload->Map(0, &empty, &mapped), "Map texture upload");
        auto *base = static_cast<uint8_t *>(mapped) + footprint.Offset;
        for (UINT y = 0; y < height; ++y) {
            auto *row = base + static_cast<SIZE_T>(y) * footprint.Footprint.RowPitch;
            for (UINT x = 0; x < width; ++x) {
                const auto color = uploaded_pixel(x, y);
                std::memcpy(row + 4 * x, color.data(), color.size());
            }
        }
        upload->Unmap(0, &whole);
        D3D12_TEXTURE_COPY_LOCATION upload_location = dst;
        upload_location.pResource = upload.Get();
        D3D12_TEXTURE_COPY_LOCATION upload_target = src;
        if (sample_texture) {
            D3D12_RESOURCE_DESC sampled_desc = desc;
            sampled_desc.Flags = D3D12_RESOURCE_FLAG_NONE;
            check(device->CreateCommittedResource(&video_memory, D3D12_HEAP_FLAG_NONE, &sampled_desc,
                                                  D3D12_RESOURCE_STATE_COPY_DEST, nullptr, IID_PPV_ARGS(&sampled)),
                  "CreateCommittedResource (sampled texture)");
            upload_target.pResource = sampled.Get();
        }
        commands.list->CopyTextureRegion(&upload_target, 0, 0, 0, &upload_location, nullptr);
    }
    if (programmable) {
        draw = create_draw_pipeline(device, desc.Format, work);
        if (use_uniform)
            bindings = tint_bindings(device);
        if (sample_texture) {
            transition(commands.list.Get(), sampled.Get(), D3D12_RESOURCE_STATE_COPY_DEST,
                       D3D12_RESOURCE_STATE_PIXEL_SHADER_RESOURCE);
            bindings = texture_bindings(device, sampled.Get());
        }
        record_draw(commands.list.Get(), draw, rtv, width, height, bindings.heap ? &bindings : nullptr);
    } else if (!upload_texture) {
        const FLOAT red[] = {1.0f, 0.0f, 0.0f, 1.0f};
        commands.list->ClearRenderTargetView(rtv, red, 0, nullptr);
    }
    transition(commands.list.Get(), target.Get(), write_state, D3D12_RESOURCE_STATE_COPY_SOURCE);
    commands.list->CopyTextureRegion(&dst, 0, 0, 0, &src, nullptr);
    const auto elapsed = submit_and_wait(device, commands);

    check(readback->Map(0, &whole, &mapped), "Map rendered image readback");
    const auto pixels = static_cast<const uint8_t *>(mapped) + footprint.Offset;
    constexpr uint8_t red[] = {255, 0, 0, 255}, green[] = {0, 255, 0, 255}, magenta[] = {255, 0, 255, 255};
    const uint8_t *expected = use_uniform ? magenta : programmable ? green : red;
    for (UINT y = 0; y < height; ++y) {
        const auto row = pixels + static_cast<SIZE_T>(y) * footprint.Footprint.RowPitch;
        for (UINT x = 0; x < width; ++x) {
            const auto original = uploaded_pixel(x, y);
            if (std::memcmp(row + 4 * x, (upload_texture || sample_texture) ? original.data() : expected, sizeof(red)) != 0)
                check(E_FAIL, "texture readback differs from expected RGBA pixels");
        }
    }
    readback->Unmap(0, &empty);
    std::wprintf(L"D3D12 %ls PASS: %ux%u verified pixels, submit-to-fence %lld us\n",
                 sample_texture ? L"TEXTURE SAMPLE" : upload_texture ? L"TEXTURE UPLOAD" :
                 use_uniform ? L"CBV DRAW" : programmable ? L"DRAW" : L"CLEAR",
                 width, height, elapsed);
}

static void draw_and_present(ID3D12Device *device, bool composition_swapchain)
{
    constexpr UINT width = 320, height = 240;
    constexpr wchar_t window_class[] = L"EntangledD3D12Smoke";
    const HINSTANCE instance = GetModuleHandleW(nullptr);
    WNDCLASSW window_description = {};
    window_description.lpfnWndProc = DefWindowProcW;
    window_description.hInstance = instance;
    window_description.lpszClassName = window_class;
    if (!RegisterClassW(&window_description))
        check(HRESULT_FROM_WIN32(GetLastError()), "RegisterClassW");
    RECT bounds = {0, 0, width, height};
    if (!AdjustWindowRect(&bounds, WS_OVERLAPPEDWINDOW, FALSE))
        check(HRESULT_FROM_WIN32(GetLastError()), "AdjustWindowRect");
    HWND window = CreateWindowExW(0, window_class, L"Entangled D3D12 presentation probe", WS_OVERLAPPEDWINDOW,
                                  240, 180, bounds.right - bounds.left, bounds.bottom - bounds.top,
                                  nullptr, nullptr, instance, nullptr);
    if (!window)
        check(HRESULT_FROM_WIN32(GetLastError()), "CreateWindowExW");
    ShowWindow(window, SW_SHOWNORMAL);

    const auto commands = begin_recording(device);
    ComPtr<IDXGIFactory2> factory;
    check(CreateDXGIFactory2(0, IID_PPV_ARGS(&factory)), "CreateDXGIFactory2");
    check(factory->MakeWindowAssociation(window, DXGI_MWA_NO_ALT_ENTER), "MakeWindowAssociation");
    DXGI_SWAP_CHAIN_DESC1 description = {};
    description.Width = width;
    description.Height = height;
    description.Format = DXGI_FORMAT_B8G8R8A8_UNORM;
    description.SampleDesc.Count = 1;
    description.BufferUsage = DXGI_USAGE_RENDER_TARGET_OUTPUT;
    description.BufferCount = 2;
    description.Scaling = DXGI_SCALING_STRETCH;
    description.SwapEffect = DXGI_SWAP_EFFECT_FLIP_DISCARD;
    ComPtr<IDXGISwapChain1> created;
    ComPtr<IDCompositionDevice> compositor;
    ComPtr<IDCompositionTarget> target;
    ComPtr<IDCompositionVisual> visual;
    if (composition_swapchain) {
        description.SwapEffect = DXGI_SWAP_EFFECT_FLIP_SEQUENTIAL;
        description.AlphaMode = DXGI_ALPHA_MODE_PREMULTIPLIED;
        check(factory->CreateSwapChainForComposition(commands.queue.Get(), &description, nullptr, &created),
              "CreateSwapChainForComposition");
        check(DCompositionCreateDevice(nullptr, IID_PPV_ARGS(&compositor)), "DCompositionCreateDevice");
        check(compositor->CreateTargetForHwnd(window, TRUE, &target), "CreateTargetForHwnd");
        check(compositor->CreateVisual(&visual), "CreateVisual");
        check(visual->SetContent(created.Get()), "SetContent");
        check(target->SetRoot(visual.Get()), "SetRoot");
        check(compositor->Commit(), "Commit (composition)");
    } else {
        check(factory->CreateSwapChainForHwnd(commands.queue.Get(), window, &description, nullptr, nullptr, &created),
              "CreateSwapChainForHwnd");
    }
    ComPtr<IDXGISwapChain3> swapchain;
    check(created.As(&swapchain), "Query IDXGISwapChain3");
    std::wprintf(L"D3D12 PRESENT: created %ux%u %ls swap chain\n", width, height,
                 composition_swapchain ? L"flip-sequential composition" : L"flip-discard HWND");
    std::fflush(stdout);

    ComPtr<ID3D12Resource> image;
    check(swapchain->GetBuffer(swapchain->GetCurrentBackBufferIndex(), IID_PPV_ARGS(&image)), "GetBuffer (swap chain)");
    const auto descriptors = create_rtv_heap(device);
    const auto rtv = descriptors->GetCPUDescriptorHandleForHeapStart();
    device->CreateRenderTargetView(image.Get(), nullptr, rtv);
    const auto draw = create_draw_pipeline(device, description.Format);
    transition(commands.list.Get(), image.Get(), D3D12_RESOURCE_STATE_PRESENT, D3D12_RESOURCE_STATE_RENDER_TARGET);
    record_draw(commands.list.Get(), draw, rtv, width, height);
    transition(commands.list.Get(), image.Get(), D3D12_RESOURCE_STATE_RENDER_TARGET, D3D12_RESOURCE_STATE_PRESENT);
    const auto render_us = submit_and_wait(device, commands);

    const auto present_start = std::chrono::steady_clock::now();
    const HRESULT presented = swapchain->Present(1, 0);
    check(presented, "Present (swap chain)");
    const auto present_us = std::chrono::duration_cast<std::chrono::microseconds>(
        std::chrono::steady_clock::now() - present_start).count();
    const auto composition_start = std::chrono::steady_clock::now();
    const HRESULT composition = DwmFlush();
    const auto composition_us = std::chrono::duration_cast<std::chrono::microseconds>(
        std::chrono::steady_clock::now() - composition_start).count();
    std::wprintf(L"D3D12 PRESENT: draw-to-fence %lld us, Present %lld us, DwmFlush %lld us (0x%08lX)\n",
                 render_us, static_cast<long long>(present_us), static_cast<long long>(composition_us),
                 static_cast<unsigned long>(composition));
    std::fflush(stdout);

    const auto deadline = std::chrono::steady_clock::now() + std::chrono::seconds(4);
    while (std::chrono::steady_clock::now() < deadline) {
        MSG msg;
        while (PeekMessageW(&msg, nullptr, 0, 0, PM_REMOVE)) {
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
        MsgWaitForMultipleObjectsEx(0, nullptr, 50, QS_ALLINPUT, MWMO_INPUTAVAILABLE);
    }
    DestroyWindow(window);
    UnregisterClassW(window_class, instance);
    if (presented != S_OK)
        check(DXGI_ERROR_INVALID_CALL, "Present did not submit a frame");
    check(composition, "DwmFlush");
    std::puts("D3D12 PRESENT: DXGI and DWM calls passed");
}

int wmain(int argc, wchar_t **argv)
{
    bool hardware_control = false;
    enum class Probe { Copy, DeviceLocal, Clear, Draw, Cbv, Upload, Sample, Present, Composition, Fence, RenderFence };
    Probe probe = Probe::Copy;
    for (int i = 1; i < argc; ++i) {
        if (!hardware_control && std::wcscmp(argv[i], L"--hardware") == 0)
            hardware_control = true;
        else if (probe == Probe::Copy && std::wcscmp(argv[i], L"--gpu-buffer") == 0)
            probe = Probe::DeviceLocal;
        else if (probe == Probe::Copy && std::wcscmp(argv[i], L"--clear") == 0)
            probe = Probe::Clear;
        else if (probe == Probe::Copy && std::wcscmp(argv[i], L"--draw") == 0)
            probe = Probe::Draw;
        else if (probe == Probe::Copy && std::wcscmp(argv[i], L"--cbv") == 0)
            probe = Probe::Cbv;
        else if (probe == Probe::Copy && std::wcscmp(argv[i], L"--upload-texture") == 0)
            probe = Probe::Upload;
        else if (probe == Probe::Copy && std::wcscmp(argv[i], L"--sample-texture") == 0)
            probe = Probe::Sample;
        else if (probe == Probe::Copy && std::wcscmp(argv[i], L"--present") == 0)
            probe = Probe::Present;
        else if (probe == Probe::Copy && std::wcscmp(argv[i], L"--composition") == 0)
            probe = Probe::Composition;
        else if (probe == Probe::Copy && std::wcscmp(argv[i], L"--fence") == 0)
            probe = Probe::Fence;
        else if (probe == Probe::Copy && std::wcscmp(argv[i], L"--fence-render-context") == 0)
            probe = Probe::RenderFence;
        else {
            std::fputs("usage: d3d12-smoke.exe [--hardware] [--gpu-buffer | --clear | --draw | --cbv | --upload-texture | "
                       "--sample-texture | --present | --composition | --fence | --fence-render-context]\n", stderr);
            return 2;
        }
    }
    const auto adapter = select_adapter(hardware_control);
    if (probe == Probe::Fence || probe == Probe::RenderFence)
        verify_wddm_fence(adapter.luid, probe == Probe::Fence);
    else {
        const auto device = create_device(adapter, hardware_control);
        if (probe == Probe::Clear)
            color_and_verify(device.Get(), ColorWork::Clear);
        else if (probe == Probe::Draw)
            color_and_verify(device.Get(), ColorWork::Draw);
        else if (probe == Probe::Cbv)
            color_and_verify(device.Get(), ColorWork::Uniform);
        else if (probe == Probe::Upload)
            color_and_verify(device.Get(), ColorWork::Upload);
        else if (probe == Probe::Sample)
            color_and_verify(device.Get(), ColorWork::Sample);
        else if (probe == Probe::Present || probe == Probe::Composition)
            draw_and_present(device.Get(), probe == Probe::Composition);
        else
            copy_and_verify(device.Get(), probe == Probe::DeviceLocal);
    }
    return 0;
}
