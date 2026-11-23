/* SPDX-License-Identifier: Apache-2.0 */
#include <windows.h>
#include <d3d12umddi.h>
#include <dxgi.h>
#include <limits.h>
#include <stdint.h>
#include <stdio.h>

#define CHECK(expr) do { \
    if (!(expr)) { fprintf(stderr, "D3D12 DDI FAIL: %s\n", #expr); return 1; } \
} while (0)

static int check_adapter(PFND3D12DDI_OPENADAPTER open_adapter)
{
    D3D12DDIARG_OPENADAPTER args = {0};
    D3D12DDI_ADAPTERFUNCS functions = {0};
    D3DDDI_ADAPTERCALLBACKS callbacks = {0};
    CHECK(open_adapter(NULL) == E_INVALIDARG);
    CHECK(open_adapter(&args) == E_INVALIDARG);
    args.pAdapterFuncs = &functions;
    CHECK(open_adapter(&args) == E_INVALIDARG);
    CHECK(args.hAdapter.pDrvPrivate == NULL);

    args.pAdapterCallbacks = &callbacks;
    CHECK(open_adapter(&args) == S_OK);
    CHECK(args.hAdapter.pDrvPrivate != NULL);
    CHECK(functions.pfnGetSupportedVersions && functions.pfnGetCaps && functions.pfnCloseAdapter);
    CHECK(functions.pfnCalcPrivateDeviceSize && functions.pfnCreateDevice);
    D3D12DDIARG_CALCPRIVATEDEVICESIZE size = {D3D12DDI_INTERFACE_VERSION, 0, 0};
    D3D12DDIARG_CREATEDEVICE_0003 create = {0};
    create.Interface = size.Interface;
    CHECK(functions.pfnCalcPrivateDeviceSize(args.hAdapter, &size) == 0);
    CHECK(functions.pfnCreateDevice(args.hAdapter, &create) == DXGI_ERROR_UNSUPPORTED);

    UINT32 count = UINT32_MAX;
    CHECK(functions.pfnGetSupportedVersions(args.hAdapter, &count, NULL) == S_OK);
    CHECK(count == 0);
    UINT64 version = UINT64_MAX;
    count = 1;
    CHECK(functions.pfnGetSupportedVersions(args.hAdapter, &count, &version) == S_OK);
    CHECK(count == 0 && version == UINT64_MAX);
    CHECK(functions.pfnGetSupportedVersions(args.hAdapter, NULL, NULL) == E_INVALIDARG);

    D3D12DDI_HRTTABLE runtime_table = {0};
    count = 1;
    CHECK(functions.pfnGetOptionalDDITables(args.hAdapter, &count, NULL) == S_OK && count == 0);
    CHECK(functions.pfnFillDDITable(args.hAdapter, D3D12DDI_TABLE_TYPE_COMMAND_QUEUE_3D,
                                    NULL, 0, 0, runtime_table) == DXGI_ERROR_UNSUPPORTED);

    UINT depth = UINT_MAX;
    D3D12DDIARG_GETCAPS caps = {D3D12DDICAPS_TYPE_3DPIPELINESUPPORT, NULL, &depth, sizeof(depth)};
    CHECK(functions.pfnGetCaps(args.hAdapter, &caps) == DXGI_ERROR_UNSUPPORTED);
    CHECK(depth == UINT_MAX);
    CHECK(functions.pfnCloseAdapter(args.hAdapter) == S_OK);
    return 0;
}

int main(void)
{
    const HMODULE module = LoadLibraryExW(L"entangled_d3d12.dll", NULL, LOAD_LIBRARY_SEARCH_APPLICATION_DIR);
    CHECK(module);
    const PFND3D12DDI_OPENADAPTER open_adapter =
        (PFND3D12DDI_OPENADAPTER)(void *)GetProcAddress(module, "OpenAdapter12");
    CHECK(open_adapter);
    const int result = check_adapter(open_adapter);
    FreeLibrary(module);
    if (!result)
        puts("D3D12 DDI PASS (adapter ABI; zero device DDI versions)");
    return result;
}
