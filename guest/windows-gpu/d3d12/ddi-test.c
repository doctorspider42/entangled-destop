/* SPDX-License-Identifier: Apache-2.0 */
#include <windows.h>
#include <d3d12umddi.h>
#include <dxgi.h>
#include <limits.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>

#define CHECK(expr) do { \
    if (!(expr)) { fprintf(stderr, "D3D12 DDI FAIL: %s\n", #expr); return 1; } \
} while (0)

static int check_adapter(PFND3D12DDI_OPENADAPTER open_adapter, BOOL require_venus)
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
    const HRESULT status = open_adapter(&args);
    if (!require_venus && status == DXGI_ERROR_UNSUPPORTED) {
        CHECK(args.hAdapter.pDrvPrivate == NULL);
        CHECK(functions.pfnCloseAdapter == NULL);
        puts("D3D12 DDI PASS (no Venus device, adapter declined)");
        return 0;
    }
    CHECK(status == S_OK);
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

    D3D12DDIARG_OPENADAPTER second = {0};
    D3D12DDI_ADAPTERFUNCS second_functions = {0};
    second.pAdapterCallbacks = &callbacks;
    second.pAdapterFuncs = &second_functions;
    CHECK(open_adapter(&second) == S_OK);
    CHECK(second.hAdapter.pDrvPrivate && second.hAdapter.pDrvPrivate != args.hAdapter.pDrvPrivate);
    CHECK(functions.pfnCloseAdapter(args.hAdapter) == S_OK);
    CHECK(second_functions.pfnGetSupportedVersions(second.hAdapter, &count, NULL) == S_OK && count == 0);
    CHECK(second_functions.pfnCloseAdapter(second.hAdapter) == S_OK);
    puts("D3D12 DDI PASS (independent Venus adapters; zero device DDI versions)");
    return 0;
}

int main(int argc, char **argv)
{
    const BOOL require_venus = argc == 2 && strcmp(argv[1], "--venus") == 0;
    if (argc != 1 && !require_venus) {
        fputs("usage: ddi-test.exe [--venus]\n", stderr);
        return 2;
    }
    const HMODULE module = LoadLibraryExW(L"entangled_d3d12.dll", NULL, LOAD_LIBRARY_SEARCH_APPLICATION_DIR);
    CHECK(module);
    const PFND3D12DDI_OPENADAPTER open_adapter =
        (PFND3D12DDI_OPENADAPTER)(void *)GetProcAddress(module, "OpenAdapter12");
    CHECK(open_adapter);
    const int result = check_adapter(open_adapter, require_venus);
    FreeLibrary(module);
    return result;
}
