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

static D3D12_RESOURCE_DESC buffer_description(UINT64 size)
{
    D3D12_RESOURCE_DESC resource = {};
    resource.Dimension = D3D12_RESOURCE_DIMENSION_BUFFER;
    resource.Width = size;
    resource.Height = 1;
    resource.DepthOrArraySize = 1;
    resource.MipLevels = 1;
    resource.SampleDesc.Count = 1;
    resource.Layout = D3D12_TEXTURE_LAYOUT_ROW_MAJOR;
    return resource;
}

static ComPtr<ID3D12Resource> committed_buffer(ID3D12Device *device, UINT64 size, const D3D12_HEAP_PROPERTIES &heap,
                                               D3D12_RESOURCE_STATES state)
{
    const auto resource = buffer_description(size);
    ComPtr<ID3D12Resource> result;
    check(device->CreateCommittedResource(&heap, D3D12_HEAP_FLAG_NONE, &resource, state, nullptr,
                                          IID_PPV_ARGS(&result)), "CreateCommittedResource (buffer)");
    return result;
}

static ComPtr<ID3D12Resource> buffer(ID3D12Device *device, UINT64 size, D3D12_HEAP_TYPE type,
                                     D3D12_RESOURCE_STATES state)
{
    const D3D12_HEAP_PROPERTIES heap = {type, D3D12_CPU_PAGE_PROPERTY_UNKNOWN, D3D12_MEMORY_POOL_UNKNOWN, 1, 1};
    return committed_buffer(device, size, heap, state);
}

struct Recording {
    ComPtr<ID3D12CommandQueue> queue;
    ComPtr<ID3D12CommandAllocator> allocator;
    ComPtr<ID3D12GraphicsCommandList> list;
};

static Recording begin_recording(ID3D12Device *device, ID3D12CommandQueue *queue = nullptr)
{
    D3D12_COMMAND_QUEUE_DESC queue_desc = {};
    queue_desc.Type = D3D12_COMMAND_LIST_TYPE_DIRECT;
    Recording commands;
    if (queue)
        commands.queue = queue;
    else
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

static UINT64 queue_clock_sample(ID3D12CommandQueue *queue)
{
    UINT64 gpu = 0, cpu = 0;
    LARGE_INTEGER begin, end;
    QueryPerformanceCounter(&begin);
    check(queue->GetClockCalibration(&gpu, &cpu), "GetClockCalibration");
    QueryPerformanceCounter(&end);
    if (cpu < static_cast<UINT64>(begin.QuadPart) || cpu > static_cast<UINT64>(end.QuadPart))
        check(E_FAIL, "calibrated QPC is outside the guest measurement window");
    return gpu;
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

static void verify_placed_buffers(ID3D12Device *device)
{
    constexpr UINT64 slot = D3D12_DEFAULT_RESOURCE_PLACEMENT_ALIGNMENT;
    constexpr size_t bytes = 4096;
    const auto resource = buffer_description(bytes);
    const auto allocation = device->GetResourceAllocationInfo(0, 1, &resource);
    std::printf("D3D12 placed buffer allocation: alignment=%llu bytes=%llu\n",
                allocation.Alignment, allocation.SizeInBytes);
    const D3D12_HEAP_DESC heap_desc = {
        2 * slot, {D3D12_HEAP_TYPE_DEFAULT, D3D12_CPU_PAGE_PROPERTY_UNKNOWN, D3D12_MEMORY_POOL_UNKNOWN, 1, 1},
        slot, D3D12_HEAP_FLAG_ALLOW_ONLY_BUFFERS,
    };
    ComPtr<ID3D12Heap> heap;
    check(device->CreateHeap(&heap_desc, IID_PPV_ARGS(&heap)), "CreateHeap (placed buffers)");
    std::array<ComPtr<ID3D12Resource>, 2> placed;
    for (size_t i = 0; i < placed.size(); ++i)
        check(device->CreatePlacedResource(heap.Get(), i * slot, &resource, D3D12_RESOURCE_STATE_COPY_DEST, nullptr,
                                           IID_PPV_ARGS(&placed[i])), "CreatePlacedResource (buffer)");
    const auto first_address = placed[0]->GetGPUVirtualAddress();
    if (!first_address || placed[1]->GetGPUVirtualAddress() != first_address + slot)
        check(E_FAIL, "placed buffers have incorrect GPU virtual addresses");

    const auto source = buffer(device, 2 * bytes, D3D12_HEAP_TYPE_UPLOAD, D3D12_RESOURCE_STATE_GENERIC_READ);
    const auto target = buffer(device, 2 * bytes, D3D12_HEAP_TYPE_READBACK, D3D12_RESOURCE_STATE_COPY_DEST);
    std::array<uint8_t, 2 * bytes> pattern;
    for (size_t i = 0; i < pattern.size(); ++i)
        pattern[i] = static_cast<uint8_t>(i * 37 + i / 251 + 1);
    const D3D12_RANGE empty = {}, whole = {0, pattern.size()};
    void *mapped = nullptr;
    check(source->Map(0, &empty, &mapped), "Map placed buffer upload");
    std::memcpy(mapped, pattern.data(), pattern.size());
    source->Unmap(0, &whole);

    const auto commands = begin_recording(device);
    for (size_t i = 0; i < placed.size(); ++i)
        commands.list->CopyBufferRegion(placed[i].Get(), 0, source.Get(), i * bytes, bytes);
    for (size_t i = 0; i < placed.size(); ++i) {
        transition(commands.list.Get(), placed[i].Get(), D3D12_RESOURCE_STATE_COPY_DEST, D3D12_RESOURCE_STATE_COPY_SOURCE);
        commands.list->CopyBufferRegion(target.Get(), i * bytes, placed[i].Get(), 0, bytes);
    }
    const auto elapsed = submit_and_wait(device, commands);
    check(target->Map(0, &whole, &mapped), "Map placed buffer readback");
    const bool identical = std::memcmp(mapped, pattern.data(), pattern.size()) == 0;
    target->Unmap(0, &empty);
    if (!identical)
        check(E_FAIL, "placed buffers alias or do not match the upload");
    if (!allocation.Alignment || slot % allocation.Alignment ||
        allocation.SizeInBytes < bytes || allocation.SizeInBytes > slot)
        check(E_FAIL, "unexpected placed buffer alignment or size");
    std::printf("D3D12 PLACED BUFFER PASS: %zu verified bytes, submit-to-fence %lld us\n", pattern.size(), elapsed);
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

static void verify_compute_dispatch(ID3D12Device *device)
{
    constexpr char source_code[] = R"(
        ByteAddressBuffer input : register(t3, space1);
        RWByteAddressBuffer output : register(u2, space3);
        cbuffer Mask : register(b0) { uint mask; };
        cbuffer Addend : register(b1, space2) { uint addend; };
        [numthreads(8, 1, 1)]
        void main(uint3 id : SV_DispatchThreadID) {
            const uint offset = 4 * id.x;
            output.Store(offset, (input.Load(offset) ^ mask) + addend);
        }
    )";
    const auto shader = compile_shader(source_code, "cs_5_1");
    const D3D12_DESCRIPTOR_RANGE1 ranges[] = {
        {D3D12_DESCRIPTOR_RANGE_TYPE_SRV, 1, 3, 1, D3D12_DESCRIPTOR_RANGE_FLAG_DATA_STATIC, 1},
        {D3D12_DESCRIPTOR_RANGE_TYPE_UAV, 1, 2, 3, D3D12_DESCRIPTOR_RANGE_FLAG_DATA_VOLATILE,
         D3D12_DESCRIPTOR_RANGE_OFFSET_APPEND},
    };
    D3D12_ROOT_PARAMETER1 parameters[3] = {};
    parameters[0].ParameterType = D3D12_ROOT_PARAMETER_TYPE_32BIT_CONSTANTS;
    parameters[0].Constants = {0, 0, 1};
    parameters[1].ParameterType = D3D12_ROOT_PARAMETER_TYPE_DESCRIPTOR_TABLE;
    parameters[1].DescriptorTable = {ARRAYSIZE(ranges), ranges};
    parameters[2].ParameterType = D3D12_ROOT_PARAMETER_TYPE_32BIT_CONSTANTS;
    parameters[2].Constants = {1, 2, 1};
    D3D12_VERSIONED_ROOT_SIGNATURE_DESC signature = {};
    signature.Version = D3D_ROOT_SIGNATURE_VERSION_1_1;
    signature.Desc_1_1.NumParameters = ARRAYSIZE(parameters);
    signature.Desc_1_1.pParameters = parameters;
    ComPtr<ID3DBlob> serialized;
    check(D3D12SerializeVersionedRootSignature(&signature, &serialized, nullptr), "serialize compute root signature");
    ComPtr<ID3D12RootSignature> root;
    check(device->CreateRootSignature(0, serialized->GetBufferPointer(), serialized->GetBufferSize(), IID_PPV_ARGS(&root)),
          "CreateRootSignature (compute)");
    D3D12_COMPUTE_PIPELINE_STATE_DESC pipeline_desc = {};
    pipeline_desc.pRootSignature = root.Get();
    pipeline_desc.CS = {shader->GetBufferPointer(), shader->GetBufferSize()};
    ComPtr<ID3D12PipelineState> pipeline;
    check(device->CreateComputePipelineState(&pipeline_desc, IID_PPV_ARGS(&pipeline)), "CreateComputePipelineState");

    constexpr UINT elements = 64;
    constexpr UINT bytes = elements * sizeof(UINT32);
    constexpr UINT mask = 0x35ce00ff, addend = 17;
    const D3D12_HEAP_PROPERTIES local_heap = {D3D12_HEAP_TYPE_DEFAULT, D3D12_CPU_PAGE_PROPERTY_UNKNOWN,
                                              D3D12_MEMORY_POOL_UNKNOWN, 1, 1};
    const auto input = buffer(device, 2 * bytes, D3D12_HEAP_TYPE_DEFAULT, D3D12_RESOURCE_STATE_COPY_DEST);
    D3D12_RESOURCE_DESC output_desc = buffer_description(2 * bytes);
    output_desc.Flags = D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS;
    ComPtr<ID3D12Resource> output;
    check(device->CreateCommittedResource(&local_heap, D3D12_HEAP_FLAG_NONE, &output_desc,
                                          D3D12_RESOURCE_STATE_UNORDERED_ACCESS, nullptr, IID_PPV_ARGS(&output)),
          "CreateCommittedResource (compute output)");
    const auto upload = buffer(device, bytes, D3D12_HEAP_TYPE_UPLOAD, D3D12_RESOURCE_STATE_GENERIC_READ);
    const auto readback = buffer(device, 2 * bytes, D3D12_HEAP_TYPE_READBACK, D3D12_RESOURCE_STATE_COPY_DEST);
    std::array<UINT32, elements> pattern;
    for (UINT i = 0; i < elements; ++i)
        pattern[i] = i * 851u + 702u;
    const D3D12_RANGE empty = {}, uploaded = {0, bytes}, downloaded = {0, 2 * bytes};
    void *mapped = nullptr;
    check(upload->Map(0, &empty, &mapped), "Map compute upload");
    std::memcpy(mapped, pattern.data(), bytes);
    upload->Unmap(0, &uploaded);

    const D3D12_DESCRIPTOR_HEAP_DESC heap_desc = {D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV, 4,
                                                  D3D12_DESCRIPTOR_HEAP_FLAG_SHADER_VISIBLE, 0};
    ComPtr<ID3D12DescriptorHeap> heap;
    check(device->CreateDescriptorHeap(&heap_desc, IID_PPV_ARGS(&heap)), "CreateDescriptorHeap (compute)");
    const auto cpu = heap->GetCPUDescriptorHandleForHeapStart();
    const auto gpu = heap->GetGPUDescriptorHandleForHeapStart();
    const UINT stride = device->GetDescriptorHandleIncrementSize(heap_desc.Type);
    D3D12_SHADER_RESOURCE_VIEW_DESC srv = {};
    srv.Format = DXGI_FORMAT_R32_TYPELESS;
    srv.ViewDimension = D3D12_SRV_DIMENSION_BUFFER;
    srv.Shader4ComponentMapping = D3D12_DEFAULT_SHADER_4_COMPONENT_MAPPING;
    srv.Buffer = {elements, elements, 0, D3D12_BUFFER_SRV_FLAG_RAW};
    device->CreateShaderResourceView(input.Get(), &srv, {cpu.ptr + 2 * stride});
    D3D12_UNORDERED_ACCESS_VIEW_DESC uav = {};
    uav.Format = DXGI_FORMAT_R32_TYPELESS;
    uav.ViewDimension = D3D12_UAV_DIMENSION_BUFFER;
    uav.Buffer = {elements, elements, 0, 0, D3D12_BUFFER_UAV_FLAG_RAW};
    device->CreateUnorderedAccessView(output.Get(), nullptr, &uav, {cpu.ptr + 3 * stride});
    check(device->GetDeviceRemovedReason(), "create compute descriptors");

    const auto commands = begin_recording(device);
    commands.list->CopyBufferRegion(input.Get(), bytes, upload.Get(), 0, bytes);
    transition(commands.list.Get(), input.Get(), D3D12_RESOURCE_STATE_COPY_DEST, D3D12_RESOURCE_STATE_NON_PIXEL_SHADER_RESOURCE);
    ID3D12DescriptorHeap *const heaps[] = {heap.Get()};
    commands.list->SetDescriptorHeaps(1, heaps);
    commands.list->SetComputeRootSignature(root.Get());
    commands.list->SetPipelineState(pipeline.Get());
    commands.list->SetComputeRoot32BitConstant(0, mask, 0);
    commands.list->SetComputeRootDescriptorTable(1, {gpu.ptr + stride});
    commands.list->SetComputeRoot32BitConstants(2, 1, &addend, 0);
    commands.list->Dispatch(elements / 8, 1, 1);
    transition(commands.list.Get(), output.Get(), D3D12_RESOURCE_STATE_UNORDERED_ACCESS, D3D12_RESOURCE_STATE_COPY_SOURCE);
    commands.list->CopyBufferRegion(readback.Get(), 0, output.Get(), 0, 2 * bytes);
    const auto elapsed = submit_and_wait(device, commands);
    check(readback->Map(0, &downloaded, &mapped), "Map compute readback");
    const auto *words = static_cast<const UINT32 *>(mapped);
    for (UINT i = 0; i < elements; ++i) {
        if (words[i] || words[elements + i] != (pattern[i] ^ mask) + addend)
            check(E_FAIL, "compute output or guard range mismatch");
    }
    readback->Unmap(0, &empty);
    std::wprintf(L"D3D12 COMPUTE PASS: %u results and %u guard bytes verified, submit-to-fence %lld us\n",
                 elements, bytes, elapsed);
}

struct DrawPipeline {
    ComPtr<ID3D12RootSignature> root;
    ComPtr<ID3D12PipelineState> pipeline;
};

enum class ColorWork { Clear, Draw, TimedDraw, CalibratedDraw, Blend, Uniform, Upload, Sample,
                       Atlas, PlacedAtlas, Persistence, Instance, Bindings };

static DrawPipeline create_draw_pipeline(ID3D12Device *device, DXGI_FORMAT target_format, ColorWork work = ColorWork::Draw)
{
    constexpr char vertex_source[] = R"(
        float4 main(uint vertex : SV_VertexID) : SV_Position {
            float2 p = float2(vertex == 1 ? 3 : -1, vertex == 2 ? 3 : -1);
            return float4(p, 0, 1);
        }
    )";
    constexpr char instance_source[] = R"(
        float4 main(uint vertex : SV_VertexID, float4 transform : TEXCOORD0) : SV_Position {
            float2 p = float2(vertex == 1 ? 3 : -1, vertex == 2 ? 3 : -1);
            return float4(p * transform.xy + transform.zw, 0, 1);
        }
    )";
    constexpr char bound_vertex_source[] = R"(
        cbuffer Globals : register(b3, space1) { float4 transform; };
        float4 main(uint vertex : SV_VertexID) : SV_Position {
            float2 p = float2(vertex == 1 ? 3 : -1, vertex == 2 ? 3 : -1);
            return float4(p * transform.xy + transform.zw, 0, 1);
        }
    )";
    constexpr char pixel_source[] = R"(
        float4 main() : SV_Target0 { return float4(0, 1, 0, 1); }
    )";
    constexpr char blend_source[] = R"(
        float4 main() : SV_Target0 { return float4(0.25, 0, 0, 0.5); }
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
    constexpr char bound_pixel_source[] = R"(
        Texture2D<float4> image : register(t2, space2);
        Texture2D<float4> other_view : register(t3, space2);
        StructuredBuffer<uint> sampler_indices : register(t4, space2);
        SamplerState samplers[2] : register(s1, space3);
        float4 main(float4 position : SV_Position) : SV_Target0 {
            uint column = (uint) position.x;
            uint sampler_index = NonUniformResourceIndex(sampler_indices[column & 1]);
            float2 uv = (position.xy + float2(0.25, 0)) / float2(73, 41);
            float4 color = image.SampleLevel(samplers[sampler_index], uv, 0);
            color.g = other_view.Load(int3(int2(position.xy), 0)).g;
            return color;
        }
    )";
    const bool uniform = work == ColorWork::Uniform;
    const bool bindings = work == ColorWork::Bindings;
    const bool sampled = work == ColorWork::Sample || work == ColorWork::Atlas || work == ColorWork::PlacedAtlas ||
                         work == ColorWork::Persistence || bindings;
    const bool instanced = work == ColorWork::Instance;
    const bool blend = work == ColorWork::Blend;
    const auto vertex = compile_shader(instanced ? instance_source : bindings ? bound_vertex_source : vertex_source, "vs_5_1");
    const auto pixel = compile_shader(uniform ? uniform_source : bindings ? bound_pixel_source
                                                             : sampled ? sampled_source : blend ? blend_source : pixel_source,
                                      "ps_5_1");
    D3D12_VERSIONED_ROOT_SIGNATURE_DESC versioned_signature = {};
    versioned_signature.Version = D3D_ROOT_SIGNATURE_VERSION_1_1;
    versioned_signature.Desc_1_1 = {};
    const D3D12_DESCRIPTOR_RANGE1 ranges[] = {
        {uniform ? D3D12_DESCRIPTOR_RANGE_TYPE_CBV : D3D12_DESCRIPTOR_RANGE_TYPE_SRV,
         bindings ? 3u : 1u, bindings ? 2u : 0u, bindings ? 2u : 0u,
         D3D12_DESCRIPTOR_RANGE_FLAG_DATA_STATIC, 0},
        {D3D12_DESCRIPTOR_RANGE_TYPE_SAMPLER, bindings ? 2u : 1u, bindings ? 1u : 0u, bindings ? 3u : 0u,
         D3D12_DESCRIPTOR_RANGE_FLAG_NONE, 0},
    };
    D3D12_ROOT_PARAMETER1 parameters[3] = {};
    for (UINT i = 0; i < ARRAYSIZE(ranges); ++i) {
        parameters[i].ParameterType = D3D12_ROOT_PARAMETER_TYPE_DESCRIPTOR_TABLE;
        parameters[i].DescriptorTable = {1, &ranges[i]};
        parameters[i].ShaderVisibility = D3D12_SHADER_VISIBILITY_PIXEL;
    }
    parameters[2].ParameterType = D3D12_ROOT_PARAMETER_TYPE_CBV;
    parameters[2].ShaderVisibility = D3D12_SHADER_VISIBILITY_VERTEX;
    parameters[2].Descriptor = {3, 1, D3D12_ROOT_DESCRIPTOR_FLAG_DATA_STATIC};
    versioned_signature.Desc_1_1.NumParameters = bindings ? 3 : uniform ? 1 : sampled ? 2 : 0;
    versioned_signature.Desc_1_1.pParameters = versioned_signature.Desc_1_1.NumParameters ? parameters : nullptr;
    if (instanced)
        versioned_signature.Desc_1_1.Flags = D3D12_ROOT_SIGNATURE_FLAG_ALLOW_INPUT_ASSEMBLER_INPUT_LAYOUT;
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
    target.BlendEnable = blend;
    target.SrcBlend = D3D12_BLEND_ONE;
    target.DestBlend = blend ? D3D12_BLEND_INV_SRC_ALPHA : D3D12_BLEND_ZERO;
    target.BlendOp = D3D12_BLEND_OP_ADD;
    target.SrcBlendAlpha = D3D12_BLEND_ONE;
    target.DestBlendAlpha = target.DestBlend;
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
    const D3D12_INPUT_ELEMENT_DESC element = {"TEXCOORD", 0, DXGI_FORMAT_R32G32B32A32_FLOAT,
        0, 0, D3D12_INPUT_CLASSIFICATION_PER_INSTANCE_DATA, 1};
    if (instanced)
        pipeline.InputLayout = {&element, 1};
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

static DrawBindings texture_bindings(ID3D12Device *device, ID3D12Resource *texture,
                                     ID3D12Resource *sampler_indices = nullptr)
{
    const bool indexed_samplers = sampler_indices != nullptr;
    DrawBindings bindings = {};
    D3D12_DESCRIPTOR_HEAP_DESC heap = {D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV, indexed_samplers ? 3u : 2u,
        D3D12_DESCRIPTOR_HEAP_FLAG_SHADER_VISIBLE, 0};
    check(device->CreateDescriptorHeap(&heap, IID_PPV_ARGS(&bindings.heap)), "CreateDescriptorHeap (sampled texture)");
    D3D12_DESCRIPTOR_HEAP_DESC staging_desc = {heap.Type, 3, D3D12_DESCRIPTOR_HEAP_FLAG_NONE, 0};
    ComPtr<ID3D12DescriptorHeap> staging;
    check(device->CreateDescriptorHeap(&staging_desc, IID_PPV_ARGS(&staging)), "CreateDescriptorHeap (texture staging)");
    const auto first = staging->GetCPUDescriptorHandleForHeapStart();
    const UINT stride = device->GetDescriptorHandleIncrementSize(heap.Type);
    const D3D12_CPU_DESCRIPTOR_HANDLE source_ranges[] = {first, {first.ptr + 2 * stride}};
    const D3D12_CPU_DESCRIPTOR_HANDLE destination = bindings.heap->GetCPUDescriptorHandleForHeapStart();
    const UINT destination_count = 2;
    D3D12_SHADER_RESOURCE_VIEW_DESC srv = {};
    srv.Format = texture->GetDesc().Format;
    srv.ViewDimension = D3D12_SRV_DIMENSION_TEXTURE2D;
    srv.Shader4ComponentMapping = D3D12_DEFAULT_SHADER_4_COMPONENT_MAPPING;
    srv.Texture2D.MipLevels = 1;
    for (const auto &source : source_ranges)
        device->CreateShaderResourceView(texture, &srv, source);
    device->CopyDescriptors(1, &destination, &destination_count, ARRAYSIZE(source_ranges), source_ranges, nullptr, heap.Type);
    if (indexed_samplers) {
        D3D12_SHADER_RESOURCE_VIEW_DESC indices = {};
        indices.ViewDimension = D3D12_SRV_DIMENSION_BUFFER;
        indices.Shader4ComponentMapping = D3D12_DEFAULT_SHADER_4_COMPONENT_MAPPING;
        indices.Buffer.NumElements = 2;
        indices.Buffer.StructureByteStride = sizeof(UINT32);
        device->CreateShaderResourceView(sampler_indices, &indices, {destination.ptr + 2 * stride});
    }
    bindings.table = bindings.heap->GetGPUDescriptorHandleForHeapStart();
    if (!indexed_samplers)
        bindings.table.ptr += stride;

    heap.Type = D3D12_DESCRIPTOR_HEAP_TYPE_SAMPLER;
    heap.NumDescriptors = indexed_samplers ? 2 : 1;
    check(device->CreateDescriptorHeap(&heap, IID_PPV_ARGS(&bindings.sampler_heap)), "CreateDescriptorHeap (sampler)");
    staging_desc.Type = heap.Type;
    staging_desc.NumDescriptors = heap.NumDescriptors;
    ComPtr<ID3D12DescriptorHeap> sampler_staging;
    check(device->CreateDescriptorHeap(&staging_desc, IID_PPV_ARGS(&sampler_staging)), "CreateDescriptorHeap (sampler staging)");
    D3D12_SAMPLER_DESC sampler = {};
    sampler.Filter = D3D12_FILTER_MIN_MAG_MIP_POINT;
    sampler.AddressU = sampler.AddressV = sampler.AddressW = D3D12_TEXTURE_ADDRESS_MODE_CLAMP;
    sampler.MaxAnisotropy = 1;
    sampler.ComparisonFunc = D3D12_COMPARISON_FUNC_ALWAYS;
    sampler.MaxLOD = D3D12_FLOAT32_MAX;
    const auto sampler_source = sampler_staging->GetCPUDescriptorHandleForHeapStart();
    device->CreateSampler(&sampler, sampler_source);
    if (indexed_samplers) {
        const UINT increment = device->GetDescriptorHandleIncrementSize(heap.Type);
        sampler.Filter = D3D12_FILTER_MIN_MAG_MIP_LINEAR;
        device->CreateSampler(&sampler, {sampler_source.ptr + increment});
    }
    device->CopyDescriptorsSimple(heap.NumDescriptors, bindings.sampler_heap->GetCPUDescriptorHandleForHeapStart(),
                                  sampler_source, heap.Type);
    bindings.sampler_table = bindings.sampler_heap->GetGPUDescriptorHandleForHeapStart();
    return bindings;
}

static void record_draw(ID3D12GraphicsCommandList *commands, const DrawPipeline &draw,
                         D3D12_CPU_DESCRIPTOR_HANDLE rtv, UINT width, UINT height, const DrawBindings *bindings = nullptr,
                         const D3D12_VERTEX_BUFFER_VIEW *instances = nullptr, D3D12_GPU_VIRTUAL_ADDRESS vertex_cbv = 0)
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
    if (instances)
        commands->IASetVertexBuffers(0, 1, instances);
    if (vertex_cbv)
        commands->SetGraphicsRootConstantBufferView(2, vertex_cbv);
    commands->IASetPrimitiveTopology(D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST);
    commands->RSSetViewports(1, &viewport);
    commands->RSSetScissorRects(1, &scissor);
    commands->OMSetRenderTargets(1, &rtv, FALSE, nullptr);
    commands->DrawInstanced(3, 1, 0, instances ? 1 : 0);
}

static ComPtr<ID3D12Resource> instance_buffer(ID3D12Device *device, D3D12_VERTEX_BUFFER_VIEW &view)
{
    constexpr UINT stride = 32;
    const auto instance = buffer(device, 3 * stride, D3D12_HEAP_TYPE_UPLOAD, D3D12_RESOURCE_STATE_GENERIC_READ);
    const D3D12_RANGE empty = {}, whole = {0, 3 * stride};
    void *mapped = nullptr;
    check(instance->Map(0, &empty, &mapped), "Map instance buffer");
    std::memset(mapped, 0, whole.End);
    const FLOAT transform[] = {1.0f, 1.0f, 0.0f, 0.0f};
    std::memcpy(static_cast<uint8_t *>(mapped) + 2 * stride, transform, sizeof(transform));
    instance->Unmap(0, &whole);
    view = {instance->GetGPUVirtualAddress() + stride, 2 * stride, stride};
    return instance;
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

static void verify_wgpu_initialization(ID3D12Device *device)
{
    struct IndirectCommand {
        D3D12_INDIRECT_ARGUMENT_TYPE type;
        UINT stride;
    };
    constexpr IndirectCommand indirect[] = {
        {D3D12_INDIRECT_ARGUMENT_TYPE_DRAW, sizeof(D3D12_DRAW_ARGUMENTS)},
        {D3D12_INDIRECT_ARGUMENT_TYPE_DRAW_INDEXED, sizeof(D3D12_DRAW_INDEXED_ARGUMENTS)},
        {D3D12_INDIRECT_ARGUMENT_TYPE_DISPATCH, sizeof(D3D12_DISPATCH_ARGUMENTS)},
    };
    ComPtr<ID3D12CommandSignature> signatures[ARRAYSIZE(indirect)];
    ComPtr<ID3D12CommandSignature> constant_signatures[ARRAYSIZE(indirect)];
    D3D12_ROOT_PARAMETER constants = {};
    constants.ParameterType = D3D12_ROOT_PARAMETER_TYPE_32BIT_CONSTANTS;
    constants.Constants = {0, 0, 3};
    const D3D12_ROOT_SIGNATURE_DESC root_desc = {1, &constants, 0, nullptr, D3D12_ROOT_SIGNATURE_FLAG_NONE};
    ComPtr<ID3DBlob> root_bytes;
    check(D3D12SerializeRootSignature(&root_desc, D3D_ROOT_SIGNATURE_VERSION_1, &root_bytes, nullptr),
          "serialize indirect command root signature");
    ComPtr<ID3D12RootSignature> root;
    check(device->CreateRootSignature(0, root_bytes->GetBufferPointer(), root_bytes->GetBufferSize(), IID_PPV_ARGS(&root)),
          "CreateRootSignature (indirect commands)");
    for (UINT i = 0; i < ARRAYSIZE(indirect); ++i) {
        D3D12_INDIRECT_ARGUMENT_DESC argument = {};
        argument.Type = indirect[i].type;
        const D3D12_COMMAND_SIGNATURE_DESC signature = {indirect[i].stride, 1, &argument, 0};
        check(device->CreateCommandSignature(&signature, nullptr, IID_PPV_ARGS(&signatures[i])),
              "CreateCommandSignature");
        D3D12_INDIRECT_ARGUMENT_DESC arguments[2] = {};
        arguments[0].Type = D3D12_INDIRECT_ARGUMENT_TYPE_CONSTANT;
        arguments[0].Constant = {0, 0, 3};
        arguments[1] = argument;
        const D3D12_COMMAND_SIGNATURE_DESC with_constants = {
            indirect[i].stride + 3 * sizeof(UINT32), ARRAYSIZE(arguments), arguments, 0,
        };
        check(device->CreateCommandSignature(&with_constants, root.Get(), IID_PPV_ARGS(&constant_signatures[i])),
              "CreateCommandSignature with root constants");
    }

    const D3D12_DESCRIPTOR_HEAP_DESC sampler_desc = {D3D12_DESCRIPTOR_HEAP_TYPE_SAMPLER,
        D3D12_MAX_SHADER_VISIBLE_SAMPLER_HEAP_SIZE, D3D12_DESCRIPTOR_HEAP_FLAG_SHADER_VISIBLE, 0};
    ComPtr<ID3D12DescriptorHeap> samplers;
    check(device->CreateDescriptorHeap(&sampler_desc, IID_PPV_ARGS(&samplers)), "CreateDescriptorHeap (2048 samplers)");
    if (!samplers->GetGPUDescriptorHandleForHeapStart().ptr)
        check(E_FAIL, "missing shader-visible sampler heap handle");
    const auto rtv_heap = create_rtv_heap(device);
    D3D12_RENDER_TARGET_VIEW_DESC null_rtv = {};
    null_rtv.Format = DXGI_FORMAT_R8G8B8A8_UNORM;
    null_rtv.ViewDimension = D3D12_RTV_DIMENSION_TEXTURE2D;
    device->CreateRenderTargetView(nullptr, &null_rtv, rtv_heap->GetCPUDescriptorHandleForHeapStart());
    check(device->GetDeviceRemovedReason(), "CreateRenderTargetView (null)");

    constexpr SIZE_T size = 64 * 1024 + 1;
    const D3D12_HEAP_PROPERTIES gpu_memory = {D3D12_HEAP_TYPE_CUSTOM, D3D12_CPU_PAGE_PROPERTY_NOT_AVAILABLE,
        D3D12_MEMORY_POOL_L1, 0, 0};
    const auto zero = committed_buffer(device, size, gpu_memory, D3D12_RESOURCE_STATE_COMMON);
    const auto destination = buffer(device, size, D3D12_HEAP_TYPE_READBACK, D3D12_RESOURCE_STATE_COPY_DEST);
    const auto commands = begin_recording(device);
    transition(commands.list.Get(), zero.Get(), D3D12_RESOURCE_STATE_COMMON, D3D12_RESOURCE_STATE_COPY_SOURCE);
    commands.list->CopyBufferRegion(destination.Get(), 0, zero.Get(), 0, size);
    const auto elapsed = submit_and_wait(device, commands);
    const D3D12_RANGE whole = {0, size}, empty = {};
    void *mapped = nullptr;
    check(destination->Map(0, &whole, &mapped), "Map initialized buffer readback");
    const auto *bytes = static_cast<const uint8_t *>(mapped);
    for (SIZE_T i = 0; i < size; ++i)
        if (bytes[i])
            check(E_FAIL, "default buffer was not initialized to zero");
    destination->Unmap(0, &empty);
    std::wprintf(L"D3D12 WGPU INIT PASS: six signatures, sampler heap, null RTV and %zu zeroed bytes, "
                 L"submit-to-fence %lld us\n", size, elapsed);
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
    const bool placed_atlas = work == ColorWork::PlacedAtlas;
    const bool sample_atlas = work == ColorWork::Atlas || placed_atlas;
    const bool persist = work == ColorWork::Persistence;
    const bool general_bindings = work == ColorWork::Bindings;
    const bool sample_texture = work == ColorWork::Sample || sample_atlas || general_bindings || persist;
    const bool use_uniform = work == ColorWork::Uniform;
    const bool use_instances = work == ColorWork::Instance;
    const bool calibrated_draw = work == ColorWork::CalibratedDraw;
    const bool timed_draw = work == ColorWork::TimedDraw || calibrated_draw;
    const bool blend = work == ColorWork::Blend;
    const bool programmable = work == ColorWork::Draw || use_uniform || sample_texture || use_instances || timed_draw || blend;
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

    ComPtr<ID3D12Heap> image_heap;
    ComPtr<ID3D12Resource> target, sampled;
    const D3D12_RESOURCE_STATES write_state = upload_texture
        ? D3D12_RESOURCE_STATE_COPY_DEST : D3D12_RESOURCE_STATE_RENDER_TARGET;
    if (persist) {
        const auto allocation = device->GetResourceAllocationInfo(0, 1, &desc);
        if (!allocation.Alignment || !allocation.SizeInBytes || allocation.SizeInBytes > UINT64_MAX / 2)
            check(E_FAIL, "unexpected render target allocation info");
        const D3D12_HEAP_DESC heap = {2 * allocation.SizeInBytes, video_memory, allocation.Alignment,
            D3D12_HEAP_FLAG_ALLOW_ONLY_RT_DS_TEXTURES};
        check(device->CreateHeap(&heap, IID_PPV_ARGS(&image_heap)), "CreateHeap (two render targets)");
        check(device->CreatePlacedResource(image_heap.Get(), 0, &desc, write_state, nullptr, IID_PPV_ARGS(&sampled)),
              "CreatePlacedResource (first render target)");
        check(device->CreatePlacedResource(image_heap.Get(), allocation.SizeInBytes, &desc, write_state, nullptr,
                                            IID_PPV_ARGS(&target)), "CreatePlacedResource (second render target)");
    } else {
        check(device->CreateCommittedResource(&video_memory, D3D12_HEAP_FLAG_NONE, &desc, write_state, nullptr,
                                               IID_PPV_ARGS(&target)), "CreateCommittedResource (color texture)");
    }
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
    constexpr UINT64 timestamp_offset = 2 * sizeof(UINT64);
    constexpr SIZE_T timestamp_end = timestamp_offset + 2 * sizeof(UINT64);
    ComPtr<ID3D12QueryHeap> timestamps;
    ComPtr<ID3D12Resource> tick_readback;
    if (timed_draw) {
        const D3D12_QUERY_HEAP_DESC heap = {D3D12_QUERY_HEAP_TYPE_TIMESTAMP, 4, 0};
        check(device->CreateQueryHeap(&heap, IID_PPV_ARGS(&timestamps)), "CreateQueryHeap (timestamp)");
        tick_readback = buffer(device, timestamp_end, D3D12_HEAP_TYPE_READBACK, D3D12_RESOURCE_STATE_COPY_DEST);
    }
    DrawPipeline draw;
    DrawBindings bindings = {};
    ComPtr<ID3D12Resource> upload;
    ComPtr<ID3D12Resource> instances;
    ComPtr<ID3D12Resource> transforms;
    ComPtr<ID3D12Resource> indices, index_upload;
    D3D12_GPU_VIRTUAL_ADDRESS vertex_cbv = 0;
    D3D12_VERTEX_BUFFER_VIEW instance_view = {};
    constexpr D3D12_BOX atlas_patch = {3, 5, 0, 11, 12, 1};
    constexpr UINT patch_x = 35, patch_y = 27;
    D3D12_TEXTURE_COPY_LOCATION src = {}, dst = {};
    src.pResource = target.Get();
    src.Type = D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX;
    dst.pResource = readback.Get();
    dst.Type = D3D12_TEXTURE_COPY_TYPE_PLACED_FOOTPRINT;
    dst.PlacedFootprint = footprint;
    const D3D12_RANGE whole = {0, static_cast<SIZE_T>(bytes)}, empty = {};
    void *mapped = nullptr;
    if (persist) {
        const auto prior = begin_recording(device, commands.queue.Get());
        const auto prior_heap = create_rtv_heap(device);
        const auto prior_rtv = prior_heap->GetCPUDescriptorHandleForHeapStart();
        device->CreateRenderTargetView(sampled.Get(), nullptr, prior_rtv);
        const FLOAT blue[] = {0, 0, 1, 1}, green[] = {0, 1, 0, 1};
        prior.list->ClearRenderTargetView(prior_rtv, blue, 0, nullptr);
        prior.list->ClearRenderTargetView(rtv, green, 0, nullptr);
        const auto overlay = create_draw_pipeline(device, desc.Format, ColorWork::Blend);
        record_draw(prior.list.Get(), overlay, prior_rtv, width, height);
        record_draw(prior.list.Get(), overlay, prior_rtv, width, height);
        transition(prior.list.Get(), sampled.Get(), D3D12_RESOURCE_STATE_RENDER_TARGET,
                   D3D12_RESOURCE_STATE_PIXEL_SHADER_RESOURCE);
        std::wprintf(L"D3D12 first render target submit-to-fence %lld us\n", submit_and_wait(device, prior));
    } else if (upload_texture || sample_texture) {
        D3D12_RESOURCE_DESC upload_desc = desc;
        upload_desc.Flags = D3D12_RESOURCE_FLAG_NONE;
        if (sample_atlas)
            upload_desc.Format = DXGI_FORMAT_R8_UNORM;
        D3D12_PLACED_SUBRESOURCE_FOOTPRINT upload_footprint = {};
        UINT64 upload_row = 0, upload_bytes = 0;
        device->GetCopyableFootprints(&upload_desc, 0, 1, 0, &upload_footprint, &rows, &upload_row, &upload_bytes);
        const UINT texel_size = sample_atlas ? 1 : 4;
        if (rows != height || upload_row != width * texel_size || !upload_bytes || upload_bytes > SIZE_MAX)
            check(E_FAIL, "unexpected sampled texture upload footprint");
        upload = buffer(device, upload_bytes, D3D12_HEAP_TYPE_UPLOAD, D3D12_RESOURCE_STATE_GENERIC_READ);
        check(upload->Map(0, &empty, &mapped), "Map texture upload");
        auto *base = static_cast<uint8_t *>(mapped) + upload_footprint.Offset;
        for (UINT y = 0; y < height; ++y) {
            auto *row = base + static_cast<SIZE_T>(y) * upload_footprint.Footprint.RowPitch;
            for (UINT x = 0; x < width; ++x) {
                const auto color = uploaded_pixel(x, y);
                std::memcpy(row + texel_size * x, color.data(), texel_size);
            }
        }
        const D3D12_RANGE upload_range = {0, static_cast<SIZE_T>(upload_bytes)};
        upload->Unmap(0, &upload_range);
        const D3D12_TEXTURE_COPY_LOCATION upload_location = {upload.Get(), D3D12_TEXTURE_COPY_TYPE_PLACED_FOOTPRINT,
                                                               {upload_footprint}};
        D3D12_TEXTURE_COPY_LOCATION upload_target = src;
        if (sample_texture) {
            if (placed_atlas) {
                const auto allocation = device->GetResourceAllocationInfo(0, 1, &upload_desc);
                constexpr UINT64 offset = D3D12_DEFAULT_RESOURCE_PLACEMENT_ALIGNMENT;
                if (allocation.Alignment != offset || !allocation.SizeInBytes || allocation.SizeInBytes > offset)
                    check(E_FAIL, "unexpected placed atlas allocation");
                const D3D12_HEAP_DESC heap = {offset + allocation.SizeInBytes, video_memory,
                    offset, D3D12_HEAP_FLAG_ALLOW_ONLY_NON_RT_DS_TEXTURES};
                check(device->CreateHeap(&heap, IID_PPV_ARGS(&image_heap)), "CreateHeap (placed atlas)");
                check(device->CreatePlacedResource(image_heap.Get(), offset, &upload_desc, D3D12_RESOURCE_STATE_COPY_DEST,
                                                    nullptr, IID_PPV_ARGS(&sampled)), "CreatePlacedResource (atlas)");
            } else {
                check(device->CreateCommittedResource(&video_memory, D3D12_HEAP_FLAG_NONE, &upload_desc,
                                                      D3D12_RESOURCE_STATE_COPY_DEST, nullptr, IID_PPV_ARGS(&sampled)),
                      "CreateCommittedResource (sampled texture)");
            }
            upload_target.pResource = sampled.Get();
        }
        commands.list->CopyTextureRegion(&upload_target, 0, 0, 0, &upload_location, nullptr);
        if (placed_atlas)
            commands.list->CopyTextureRegion(&upload_target, patch_x, patch_y, 0, &upload_location, &atlas_patch);
    }
    if (programmable) {
        draw = create_draw_pipeline(device, desc.Format, work);
        if (use_uniform)
            bindings = tint_bindings(device);
        if (sample_texture && !persist) {
            transition(commands.list.Get(), sampled.Get(), D3D12_RESOURCE_STATE_COPY_DEST,
                       D3D12_RESOURCE_STATE_PIXEL_SHADER_RESOURCE);
        }
        if (general_bindings) {
            constexpr UINT offset = D3D12_CONSTANT_BUFFER_DATA_PLACEMENT_ALIGNMENT;
            const FLOAT transform[] = {1.f, 1.f, 0.f, 0.f};
            transforms = buffer(device, 2 * offset, D3D12_HEAP_TYPE_UPLOAD, D3D12_RESOURCE_STATE_GENERIC_READ);
            check(transforms->Map(0, &empty, &mapped), "Map vertex root CBV");
            std::memcpy(static_cast<uint8_t *>(mapped) + offset, transform, sizeof(transform));
            const D3D12_RANGE written = {offset, offset + sizeof(transform)};
            transforms->Unmap(0, &written);
            vertex_cbv = transforms->GetGPUVirtualAddress() + offset;

            constexpr UINT32 sampler_indices[] = {0, 1};
            indices = buffer(device, sizeof(sampler_indices), D3D12_HEAP_TYPE_DEFAULT, D3D12_RESOURCE_STATE_COPY_DEST);
            index_upload = buffer(device, sizeof(sampler_indices), D3D12_HEAP_TYPE_UPLOAD, D3D12_RESOURCE_STATE_GENERIC_READ);
            check(index_upload->Map(0, &empty, &mapped), "Map sampler index upload");
            std::memcpy(mapped, sampler_indices, sizeof(sampler_indices));
            const D3D12_RANGE index_range = {0, sizeof(sampler_indices)};
            index_upload->Unmap(0, &index_range);
            commands.list->CopyBufferRegion(indices.Get(), 0, index_upload.Get(), 0, sizeof(sampler_indices));
            transition(commands.list.Get(), indices.Get(), D3D12_RESOURCE_STATE_COPY_DEST, D3D12_RESOURCE_STATE_PIXEL_SHADER_RESOURCE);
        }
        if (sample_texture)
            bindings = texture_bindings(device, sampled.Get(), indices.Get());
        if (use_instances)
            instances = instance_buffer(device, instance_view);
        if (timestamps)
            commands.list->EndQuery(timestamps.Get(), D3D12_QUERY_TYPE_TIMESTAMP, 1);
        if (blend) {
            const FLOAT blue[] = {0.0f, 0.0f, 1.0f, 1.0f};
            commands.list->ClearRenderTargetView(rtv, blue, 0, nullptr);
            record_draw(commands.list.Get(), draw, rtv, width, height);
        }
        record_draw(commands.list.Get(), draw, rtv, width, height, bindings.heap ? &bindings : nullptr,
                    use_instances ? &instance_view : nullptr, vertex_cbv);
        if (timestamps) {
            commands.list->EndQuery(timestamps.Get(), D3D12_QUERY_TYPE_TIMESTAMP, 2);
            commands.list->ResolveQueryData(timestamps.Get(), D3D12_QUERY_TYPE_TIMESTAMP, 1, 2,
                                            tick_readback.Get(), timestamp_offset);
        }
    } else if (!upload_texture) {
        const FLOAT red[] = {1.0f, 0.0f, 0.0f, 1.0f};
        commands.list->ClearRenderTargetView(rtv, red, 0, nullptr);
    }
    transition(commands.list.Get(), target.Get(), write_state, D3D12_RESOURCE_STATE_COPY_SOURCE);
    commands.list->CopyTextureRegion(&dst, 0, 0, 0, &src, nullptr);
    UINT64 frequency = 0, before = 0, after = 0;
    if (calibrated_draw) {
        check(commands.queue->GetTimestampFrequency(&frequency), "GetTimestampFrequency");
        if (!frequency)
            check(E_FAIL, "queue clock frequency is zero");
        before = queue_clock_sample(commands.queue.Get());
    }
    const auto elapsed = submit_and_wait(device, commands);
    if (calibrated_draw)
        after = queue_clock_sample(commands.queue.Get());

    check(readback->Map(0, &whole, &mapped), "Map rendered image readback");
    const auto pixels = static_cast<const uint8_t *>(mapped) + footprint.Offset;
    constexpr uint8_t red[] = {255, 0, 0, 255}, green[] = {0, 255, 0, 255}, magenta[] = {255, 0, 255, 255};
    constexpr uint8_t blended[] = {96, 0, 64, 255};
    const uint8_t *expected = use_uniform ? magenta : blend || persist ? blended : programmable ? green : red;
    for (UINT y = 0; y < height; ++y) {
        const auto row = pixels + static_cast<SIZE_T>(y) * footprint.Footprint.RowPitch;
        for (UINT x = 0; x < width; ++x) {
            const bool patched = placed_atlas && x >= patch_x && x < patch_x + atlas_patch.right - atlas_patch.left &&
                                 y >= patch_y && y < patch_y + atlas_patch.bottom - atlas_patch.top;
            auto original = patched ? uploaded_pixel(x - patch_x + atlas_patch.left, y - patch_y + atlas_patch.top)
                                    : uploaded_pixel(x, y);
            if (sample_atlas)
                original[1] = original[2] = 0;
            if (general_bindings && (x & 1)) {
                const auto next = uploaded_pixel(x + 1, y);
                const auto *actual = row + 4 * x;
                if (std::abs(4 * actual[0] - (3 * original[0] + next[0])) > 4 ||
                    std::abs(4 * actual[2] - (3 * original[2] + next[2])) > 4 ||
                    actual[1] != original[1] || actual[3] != 255)
                    check(E_FAIL, "indexed linear sampler output differs from the reference");
                continue;
            }
            const auto *reference = (upload_texture || sample_texture) && !persist ? original.data() : expected;
            if (std::memcmp(row + 4 * x, reference, sizeof(red)) != 0)
                check(E_FAIL, "texture readback differs from expected RGBA pixels");
        }
    }
    readback->Unmap(0, &empty);
    if (timestamps) {
        const D3D12_RANGE tick_range = {timestamp_offset, timestamp_end};
        check(tick_readback->Map(0, &tick_range, &mapped), "Map timestamp results");
        const auto *ticks = reinterpret_cast<const UINT64 *>(static_cast<const uint8_t *>(mapped) + timestamp_offset);
        if (!ticks[0] || ticks[1] <= ticks[0])
            check(E_FAIL, "timestamps did not advance around the draw");
        if (calibrated_draw) {
            // The host clock read extrapolates from a recent calibrated sample.
            const UINT64 tolerance = frequency / 1000;
            std::wprintf(L"D3D12 calibrated GPU clock: %llu ticks/s, before %llu, draw %llu to %llu, after %llu\n",
                         frequency, before, ticks[0], ticks[1], after);
            if (after <= before || (ticks[0] < before && before - ticks[0] > tolerance) ||
                (ticks[1] > after && ticks[1] - after > tolerance))
                check(E_FAIL, "draw queries and queue calibration differ in time domain");
            std::wprintf(L"D3D12 QUEUE CLOCK PASS: draw %.3f us between GPU markers\n",
                         static_cast<double>(ticks[1] - ticks[0]) * 1000000.0 / static_cast<double>(frequency));
        }
        std::wprintf(L"D3D12 draw GPU timestamp delta: %llu ticks\n", ticks[1] - ticks[0]);
        tick_readback->Unmap(0, &empty);
    }
    std::wprintf(L"D3D12 %ls PASS: %ux%u verified pixels, submit-to-fence %lld us\n",
                 timed_draw ? L"TIMESTAMP DRAW" : blend ? L"BLEND" : general_bindings ? L"GRAPHICS BINDINGS" :
                 persist ? L"TEXTURE PERSISTENCE" : placed_atlas ? L"PLACED R8 ATLAS" : sample_atlas ? L"R8 ATLAS SAMPLE" :
                 sample_texture ? L"TEXTURE SAMPLE" : upload_texture ? L"TEXTURE UPLOAD" :
                 use_uniform ? L"CBV DRAW" : use_instances ? L"INSTANCE DRAW" : programmable ? L"DRAW" : L"CLEAR",
                 width, height, elapsed);
}

static void draw_and_present(ID3D12Device *device, bool composition_swapchain, bool wgpu_swapchain)
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
        if (wgpu_swapchain) {
            description.BufferCount = 3;
            description.Flags = DXGI_SWAP_CHAIN_FLAG_FRAME_LATENCY_WAITABLE_OBJECT;
            ComPtr<IDXGIFactory5> feature_factory;
            BOOL tearing = FALSE;
            if (SUCCEEDED(factory.As(&feature_factory)) &&
                SUCCEEDED(feature_factory->CheckFeatureSupport(DXGI_FEATURE_PRESENT_ALLOW_TEARING, &tearing, sizeof(tearing))) &&
                tearing)
                description.Flags |= DXGI_SWAP_CHAIN_FLAG_ALLOW_TEARING;
            std::printf("D3D12 wgpu composition configuration: buffers=%u flags=0x%x\n",
                        description.BufferCount, description.Flags);
            std::fflush(stdout);
        } else {
            description.SwapEffect = DXGI_SWAP_EFFECT_FLIP_SEQUENTIAL;
        }
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
    if (wgpu_swapchain) {
        check(swapchain->SetMaximumFrameLatency(3), "SetMaximumFrameLatency");
        const HANDLE available = swapchain->GetFrameLatencyWaitableObject();
        if (!available)
            check(E_FAIL, "GetFrameLatencyWaitableObject");
        const DWORD signaled = WaitForSingleObject(available, 5000);
        CloseHandle(available);
        if (signaled != WAIT_OBJECT_0)
            check(E_FAIL, "DXGI frame-latency waitable object did not signal");
    }
    std::wprintf(L"D3D12 PRESENT: created %ux%u %ls swap chain\n", width, height,
                 wgpu_swapchain ? L"flip-discard composition" :
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
    enum class Probe { Copy, DeviceLocal, Placed, Compute, Clear, Draw, TimedDraw, Clock, Blend, Cbv, Upload,
                       Sample, Atlas, PlacedAtlas, Persistence, Instance, Bindings, WgpuInit, Present, Composition,
                       WgpuComposition, Fence, RenderFence };
    Probe probe = Probe::Copy;
    for (int i = 1; i < argc; ++i) {
        if (!hardware_control && std::wcscmp(argv[i], L"--hardware") == 0)
            hardware_control = true;
        else if (probe == Probe::Copy && std::wcscmp(argv[i], L"--gpu-buffer") == 0)
            probe = Probe::DeviceLocal;
        else if (probe == Probe::Copy && std::wcscmp(argv[i], L"--placed-buffer") == 0)
            probe = Probe::Placed;
        else if (probe == Probe::Copy && std::wcscmp(argv[i], L"--compute") == 0)
            probe = Probe::Compute;
        else if (probe == Probe::Copy && std::wcscmp(argv[i], L"--clear") == 0)
            probe = Probe::Clear;
        else if (probe == Probe::Copy && std::wcscmp(argv[i], L"--draw") == 0)
            probe = Probe::Draw;
        else if (probe == Probe::Copy && std::wcscmp(argv[i], L"--timestamps") == 0)
            probe = Probe::TimedDraw;
        else if (probe == Probe::Copy && std::wcscmp(argv[i], L"--clock") == 0)
            probe = Probe::Clock;
        else if (probe == Probe::Copy && std::wcscmp(argv[i], L"--blend") == 0)
            probe = Probe::Blend;
        else if (probe == Probe::Copy && std::wcscmp(argv[i], L"--cbv") == 0)
            probe = Probe::Cbv;
        else if (probe == Probe::Copy && std::wcscmp(argv[i], L"--upload-texture") == 0)
            probe = Probe::Upload;
        else if (probe == Probe::Copy && std::wcscmp(argv[i], L"--sample-texture") == 0)
            probe = Probe::Sample;
        else if (probe == Probe::Copy && std::wcscmp(argv[i], L"--sample-atlas") == 0)
            probe = Probe::Atlas;
        else if (probe == Probe::Copy && std::wcscmp(argv[i], L"--placed-texture") == 0)
            probe = Probe::PlacedAtlas;
        else if (probe == Probe::Copy && std::wcscmp(argv[i], L"--texture-persistence") == 0)
            probe = Probe::Persistence;
        else if (probe == Probe::Copy && std::wcscmp(argv[i], L"--graphics-bindings") == 0)
            probe = Probe::Bindings;
        else if (probe == Probe::Copy && std::wcscmp(argv[i], L"--instance") == 0)
            probe = Probe::Instance;
        else if (probe == Probe::Copy && std::wcscmp(argv[i], L"--wgpu-init") == 0)
            probe = Probe::WgpuInit;
        else if (probe == Probe::Copy && std::wcscmp(argv[i], L"--present") == 0)
            probe = Probe::Present;
        else if (probe == Probe::Copy && std::wcscmp(argv[i], L"--composition") == 0)
            probe = Probe::Composition;
        else if (probe == Probe::Copy && std::wcscmp(argv[i], L"--wgpu-composition") == 0)
            probe = Probe::WgpuComposition;
        else if (probe == Probe::Copy && std::wcscmp(argv[i], L"--fence") == 0)
            probe = Probe::Fence;
        else if (probe == Probe::Copy && std::wcscmp(argv[i], L"--fence-render-context") == 0)
            probe = Probe::RenderFence;
        else {
            std::fputs("usage: d3d12-smoke.exe [--hardware] [--gpu-buffer | --placed-buffer | --compute | --clear | --draw | --timestamps | --clock | --blend | --cbv | --upload-texture | "
                       "--sample-texture | --sample-atlas | --placed-texture | --texture-persistence | --instance | --graphics-bindings | --wgpu-init | "
                       "--present | --composition | --wgpu-composition | --fence | --fence-render-context]\n", stderr);
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
        else if (probe == Probe::TimedDraw)
            color_and_verify(device.Get(), ColorWork::TimedDraw);
        else if (probe == Probe::Clock)
            color_and_verify(device.Get(), ColorWork::CalibratedDraw);
        else if (probe == Probe::Blend)
            color_and_verify(device.Get(), ColorWork::Blend);
        else if (probe == Probe::Cbv)
            color_and_verify(device.Get(), ColorWork::Uniform);
        else if (probe == Probe::Upload)
            color_and_verify(device.Get(), ColorWork::Upload);
        else if (probe == Probe::Sample)
            color_and_verify(device.Get(), ColorWork::Sample);
        else if (probe == Probe::Atlas)
            color_and_verify(device.Get(), ColorWork::Atlas);
        else if (probe == Probe::PlacedAtlas)
            color_and_verify(device.Get(), ColorWork::PlacedAtlas);
        else if (probe == Probe::Persistence)
            color_and_verify(device.Get(), ColorWork::Persistence);
        else if (probe == Probe::Bindings)
            color_and_verify(device.Get(), ColorWork::Bindings);
        else if (probe == Probe::Instance)
            color_and_verify(device.Get(), ColorWork::Instance);
        else if (probe == Probe::WgpuInit)
            verify_wgpu_initialization(device.Get());
        else if (probe == Probe::Placed)
            verify_placed_buffers(device.Get());
        else if (probe == Probe::Compute)
            verify_compute_dispatch(device.Get());
        else if (probe == Probe::Present || probe == Probe::Composition || probe == Probe::WgpuComposition)
            draw_and_present(device.Get(), probe != Probe::Present, probe == Probe::WgpuComposition);
        else
            copy_and_verify(device.Get(), probe == Probe::DeviceLocal);
    }
    return 0;
}
