/* SPDX-License-Identifier: Apache-2.0 */
#include <windows.h>
#include <d3d12.h>
#include <d3dkmthk.h>
#include <dxgi1_2.h>
#include <wrl/client.h>

#include <array>
#include <chrono>
#include <cstdint>
#include <cstdio>
#include <cstdlib>
#include <cstring>
#include <cwchar>

using Microsoft::WRL::ComPtr;

static void check(HRESULT result, const char *operation)
{
    if (FAILED(result)) {
        std::fprintf(stderr, "D3D12 COPY FAIL: %s (0x%08lX)\n", operation,
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

static ComPtr<ID3D12Device> create_device(bool hardware_control)
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
        ComPtr<ID3D12Device> device;
        check(D3D12CreateDevice(adapter.Get(), D3D_FEATURE_LEVEL_11_0, IID_PPV_ARGS(&device)),
              "D3D12CreateDevice (feature level 11_0)");
        const auto actual_luid = device->GetAdapterLuid();
        if (actual_luid.LowPart != desc.AdapterLuid.LowPart ||
            actual_luid.HighPart != desc.AdapterLuid.HighPart) {
            check(E_FAIL, "device LUID differs from the explicit DXGI adapter");
        }
        if (!hardware_control && !GetModuleHandleW(L"entangled_d3d12.dll"))
            check(E_FAIL, "Entangled D3D12 UMD is missing from the runtime process");
        return device;
    }
    check(DXGI_ERROR_NOT_FOUND, hardware_control ? "find a hardware D3D12 adapter" :
                                                 "find the Entangled WDDM adapter");
    return {};
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

static void copy_and_verify(ID3D12Device *device)
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

    D3D12_COMMAND_QUEUE_DESC queue_desc = {};
    queue_desc.Type = D3D12_COMMAND_LIST_TYPE_DIRECT;
    ComPtr<ID3D12CommandQueue> queue;
    ComPtr<ID3D12CommandAllocator> allocator;
    ComPtr<ID3D12GraphicsCommandList> list;
    ComPtr<ID3D12Fence> fence;
    check(device->CreateCommandQueue(&queue_desc, IID_PPV_ARGS(&queue)), "CreateCommandQueue");
    check(device->CreateCommandAllocator(queue_desc.Type, IID_PPV_ARGS(&allocator)), "CreateCommandAllocator");
    check(device->CreateCommandList(0, queue_desc.Type, allocator.Get(), nullptr, IID_PPV_ARGS(&list)),
          "CreateCommandList");
    check(device->CreateFence(0, D3D12_FENCE_FLAG_NONE, IID_PPV_ARGS(&fence)), "CreateFence");
    list->CopyBufferRegion(target.Get(), 0, source.Get(), 0, pattern.size());
    check(list->Close(), "Close command list");

    const HANDLE completed = CreateEventW(nullptr, FALSE, FALSE, nullptr);
    if (!completed)
        check(HRESULT_FROM_WIN32(GetLastError()), "CreateEvent");
    check(fence->SetEventOnCompletion(1, completed), "SetEventOnCompletion");
    ID3D12CommandList *const lists[] = {list.Get()};
    const auto start = std::chrono::steady_clock::now();
    queue->ExecuteCommandLists(1, lists);
    check(queue->Signal(fence.Get(), 1), "Signal fence");
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

    check(target->Map(0, &whole, &mapped), "Map readback buffer");
    const bool identical = std::memcmp(mapped, pattern.data(), pattern.size()) == 0;
    target->Unmap(0, &empty);
    if (!identical)
        check(E_FAIL, "readback differs from upload");
    std::wprintf(L"D3D12 COPY PASS: %zu verified bytes, submit-to-fence %lld us\n", pattern.size(),
                 static_cast<long long>(std::chrono::duration_cast<std::chrono::microseconds>(elapsed).count()));
}

int wmain(int argc, wchar_t **argv)
{
    const bool hardware_control = argc == 2 && std::wcscmp(argv[1], L"--hardware") == 0;
    if (argc != 1 && !hardware_control) {
        std::fputs("usage: d3d12-smoke.exe [--hardware]\n", stderr);
        return 2;
    }
    copy_and_verify(create_device(hardware_control).Get());
    return 0;
}
