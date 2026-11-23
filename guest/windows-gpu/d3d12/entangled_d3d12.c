/* SPDX-License-Identifier: Apache-2.0 */
#include <windows.h>
#include <d3d12umddi.h>
#include <dxgi.h>
#include "venus-adapter.h"

static SIZE_T APIENTRY private_device_size(D3D12DDI_HADAPTER adapter,
                                           const D3D12DDIARG_CALCPRIVATEDEVICESIZE *args)
{
    (void)adapter;
    (void)args;
    return 0;
}

static HRESULT APIENTRY create_device(D3D12DDI_HADAPTER adapter, const D3D12DDIARG_CREATEDEVICE_0003 *args)
{
    (void)adapter;
    (void)args;
    return DXGI_ERROR_UNSUPPORTED;
}

static HRESULT APIENTRY get_supported_versions(D3D12DDI_HADAPTER adapter,
                                                UINT32 *count, UINT64 *versions)
{
    (void)versions;
    if (!adapter.pDrvPrivate || !count)
        return E_INVALIDARG;

    /* A version commits us to its device and command-list tables. */
    *count = 0;
    OutputDebugStringA("Entangled D3D12: GetSupportedVersions (no device DDI yet)\n");
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

    return DXGI_ERROR_UNSUPPORTED;
}

static HRESULT APIENTRY get_optional_tables(D3D12DDI_HADAPTER adapter,
                                             UINT32 *count, D3D12DDI_TABLE_REQUEST *tables)
{
    (void)tables;
    if (!adapter.pDrvPrivate || !count)
        return E_INVALIDARG;
    *count = 0;
    return S_OK;
}

static HRESULT APIENTRY fill_table(D3D12DDI_HADAPTER adapter, D3D12DDI_TABLE_TYPE type, void *table,
                                   SIZE_T size, UINT index, D3D12DDI_HRTTABLE runtime_table)
{
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
    (void)device;
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
