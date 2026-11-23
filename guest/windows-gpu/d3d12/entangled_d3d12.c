/* SPDX-License-Identifier: Apache-2.0 */
#include <windows.h>
#include <d3d12umddi.h>
#include <dxgi.h>
#include <stdio.h>
#include <string.h>
#include "venus-adapter.h"
#include "venus-transfer.h"

#ifdef ENTANGLED_D3D12_PROBE_DDI
static void trace_ddi(const char *operation, UINT code, SIZE_T value)
{
    char message[160];
    snprintf(message, sizeof(message), "Entangled D3D12: %s code=%u value=%zu\n", operation, code, value);
    OutputDebugStringA(message);
}

static HRESULT write_probe_caps(const D3D12DDIARG_GETCAPS *args, const void *data, SIZE_T size)
{
    if (!args->pData || args->DataSize != size)
        return E_INVALIDARG;
    memcpy(args->pData, data, size);
    return S_OK;
}
#endif

static SIZE_T APIENTRY private_device_size(D3D12DDI_HADAPTER adapter,
                                           const D3D12DDIARG_CALCPRIVATEDEVICESIZE *args)
{
    if (!adapter.pDrvPrivate || !args)
        return 0;
#ifdef ENTANGLED_D3D12_PROBE_DDI
    trace_ddi("CalcPrivateDeviceSize", args->Interface, args->Version);
#endif
    return args->Interface == D3D12DDI_INTERFACE_VERSION_R4 ? sizeof(VenusDevice) : 0;
}

static HRESULT APIENTRY create_device(D3D12DDI_HADAPTER adapter, const D3D12DDIARG_CREATEDEVICE_0003 *args)
{
    const VenusAdapter *venus = adapter.pDrvPrivate;
    if (!venus || !args)
        return E_INVALIDARG;
#ifdef ENTANGLED_D3D12_PROBE_DDI
    trace_ddi("CreateDevice", args->Interface, args->Version);
#endif
    if (args->Interface != D3D12DDI_INTERFACE_VERSION_R4)
        return DXGI_ERROR_UNSUPPORTED;
    VenusDevice *device = args->hDrvDevice.pDrvPrivate;
    if (!device)
        return E_INVALIDARG;
    *device = (VenusDevice){0};
    HRESULT result = create_venus_device(venus, device);
#ifdef ENTANGLED_D3D12_PROBE_COPY
    if (SUCCEEDED(result)) {
        trace_ddi("Venus copy begin", 0, 64 * 1024);
        result = probe_venus_buffer_copy(device);
        trace_ddi("Venus copy result", (UINT)result, 64 * 1024);
        if (FAILED(result))
            destroy_venus_device(device);
    }
#endif
#ifdef ENTANGLED_D3D12_PROBE_DDI
    trace_ddi("CreateDevice result", (UINT)result, sizeof(*device));
#endif
    return result;
}

static HRESULT APIENTRY get_supported_versions(D3D12DDI_HADAPTER adapter,
                                                UINT32 *count, UINT64 *versions)
{
    (void)versions;
    if (!adapter.pDrvPrivate || !count)
        return E_INVALIDARG;

#ifdef ENTANGLED_D3D12_PROBE_DDI
    const UINT32 capacity = versions ? *count : 0;
    *count = 1;
    if (versions) {
        if (capacity < *count)
            return HRESULT_FROM_WIN32(ERROR_INSUFFICIENT_BUFFER);
        versions[0] = D3D12DDI_SUPPORTED_0040;
    }
    trace_ddi("GetSupportedVersions", 40, versions != NULL);
#else
    /* A version commits us to its device and command-list tables. */
    *count = 0;
    OutputDebugStringA("Entangled D3D12: GetSupportedVersions (no device DDI yet)\n");
#endif
    return S_OK;
}

static HRESULT APIENTRY close_adapter(D3D12DDI_HADAPTER adapter)
{
    VenusAdapter *venus = adapter.pDrvPrivate;
    if (!venus)
        return E_INVALIDARG;
    close_venus_adapter(venus);
    HeapFree(GetProcessHeap(), 0, venus);
    return S_OK;
}

static HRESULT APIENTRY get_caps(D3D12DDI_HADAPTER adapter, const D3D12DDIARG_GETCAPS *args)
{
    if (!adapter.pDrvPrivate || !args)
        return E_INVALIDARG;

#ifdef ENTANGLED_D3D12_PROBE_DDI
    trace_ddi("GetCaps", args->Type, args->DataSize);
    if (args->Type == D3D12DDICAPS_TYPE_3DPIPELINESUPPORT) {
        /* Let the lab runtime reach device initialization. Function tables still gate creation. */
        const D3D12DDI_3DPIPELINELEVEL level = D3D12DDI_3DPIPELINELEVEL_11_0;
        return write_probe_caps(args, &level, sizeof(level));
    }
    if (args->Type == D3D12DDICAPS_TYPE_D3D12_OPTIONS) {
        const D3D12DDI_D3D12_OPTIONS_DATA_0033 options = {
            .ResourceBindingTier = D3D12DDI_RESOURCE_BINDING_TIER_1,
            .ResourceHeapTier = D3D12DDI_RESOURCE_HEAP_TIER_1,
        };
        return write_probe_caps(args, &options, sizeof(options));
    }
#endif
    return DXGI_ERROR_UNSUPPORTED;
}

static HRESULT APIENTRY get_optional_tables(D3D12DDI_HADAPTER adapter,
                                             UINT32 *count, D3D12DDI_TABLE_REQUEST *tables)
{
    (void)tables;
    if (!adapter.pDrvPrivate || !count)
        return E_INVALIDARG;
#ifdef ENTANGLED_D3D12_PROBE_DDI
    trace_ddi("GetOptionalDDITables", 0, tables != NULL);
#endif
    *count = 0;
    return S_OK;
}

static HRESULT APIENTRY fill_table(D3D12DDI_HADAPTER adapter, D3D12DDI_TABLE_TYPE type, void *table,
                                   SIZE_T size, UINT index, D3D12DDI_HRTTABLE runtime_table)
{
#ifdef ENTANGLED_D3D12_PROBE_DDI
    trace_ddi("FillDDITable", type, size);
#endif
    (void)adapter;
    (void)type;
    (void)table;
    (void)size;
    (void)index;
    (void)runtime_table;
    return DXGI_ERROR_UNSUPPORTED;
}

static void APIENTRY destroy_device(D3D12DDI_HDEVICE device)
{
    if (device.pDrvPrivate)
        destroy_venus_device(device.pDrvPrivate);
#ifdef ENTANGLED_D3D12_PROBE_DDI
    trace_ddi("DestroyDevice", 0, 0);
#endif
}

static const D3D12DDI_ADAPTERFUNCS adapter_functions = {
    .pfnCalcPrivateDeviceSize = private_device_size,
    .pfnCreateDevice = create_device,
    .pfnCloseAdapter = close_adapter,
    .pfnGetSupportedVersions = get_supported_versions,
    .pfnGetCaps = get_caps,
    .pfnGetOptionalDDITables = get_optional_tables,
    .pfnFillDDITable = fill_table,
    .pfnDestroyDevice = destroy_device,
};

__declspec(dllexport) HRESULT APIENTRY OpenAdapter12(D3D12DDIARG_OPENADAPTER *args)
{
    if (!args || !args->pAdapterFuncs || !args->pAdapterCallbacks)
        return E_INVALIDARG;

    OutputDebugStringA("Entangled D3D12: OpenAdapter12\n");
    VenusAdapter *venus = HeapAlloc(GetProcessHeap(), HEAP_ZERO_MEMORY, sizeof(*venus));
    if (!venus)
        return E_OUTOFMEMORY;

    const HRESULT result = open_venus_adapter(venus);
    if (FAILED(result)) {
        close_venus_adapter(venus);
        HeapFree(GetProcessHeap(), 0, venus);
        OutputDebugStringA("Entangled D3D12: failed to select a Venus graphics device\n");
        return result;
    }

    args->hAdapter.pDrvPrivate = venus;
    *args->pAdapterFuncs = adapter_functions;
    return S_OK;
}
