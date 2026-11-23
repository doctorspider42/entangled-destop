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
#ifdef ENTANGLED_D3D12_PROBE_DDI
    const UINT32 expected_versions = 1;
    const UINT64 expected_version = D3D12DDI_SUPPORTED_0040;
#else
    const UINT32 expected_versions = 0;
    const UINT64 expected_version = UINT64_MAX;
#endif
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

    size.Interface = D3D12DDI_INTERFACE_VERSION_R4;
    create.Interface = size.Interface;
    const SIZE_T device_size = functions.pfnCalcPrivateDeviceSize(args.hAdapter, &size);
    CHECK(device_size != 0);
    CHECK(functions.pfnCreateDevice(args.hAdapter, &create) == E_INVALIDARG);
    create.hDrvDevice.pDrvPrivate = HeapAlloc(GetProcessHeap(), 0, device_size);
    CHECK(create.hDrvDevice.pDrvPrivate);
    const HRESULT device_status = functions.pfnCreateDevice(args.hAdapter, &create);
    if (SUCCEEDED(device_status))
        functions.pfnDestroyDevice(create.hDrvDevice);
    HeapFree(GetProcessHeap(), 0, create.hDrvDevice.pDrvPrivate);
    CHECK(device_status == S_OK);

    UINT32 count = UINT32_MAX;
    CHECK(functions.pfnGetSupportedVersions(args.hAdapter, &count, NULL) == S_OK);
    CHECK(count == expected_versions);
    UINT64 version = UINT64_MAX;
    count = 1;
    CHECK(functions.pfnGetSupportedVersions(args.hAdapter, &count, &version) == S_OK);
    CHECK(count == expected_versions && version == expected_version);
    CHECK(functions.pfnGetSupportedVersions(args.hAdapter, NULL, NULL) == E_INVALIDARG);
#ifdef ENTANGLED_D3D12_PROBE_DDI
    count = 0;
    version = UINT64_MAX;
    CHECK(functions.pfnGetSupportedVersions(args.hAdapter, &count, &version) ==
          HRESULT_FROM_WIN32(ERROR_INSUFFICIENT_BUFFER));
    CHECK(count == expected_versions && version == UINT64_MAX);
#endif

    D3D12DDI_HRTTABLE runtime_table = {0};
    count = 1;
    CHECK(functions.pfnGetOptionalDDITables(args.hAdapter, &count, NULL) == S_OK && count == 0);
    CHECK(functions.pfnFillDDITable(args.hAdapter, D3D12DDI_TABLE_TYPE_COMMAND_QUEUE_3D,
                                    NULL, 0, 0, runtime_table) == DXGI_ERROR_UNSUPPORTED);

    UINT depth = UINT_MAX;
    D3D12DDIARG_GETCAPS caps = {D3D12DDICAPS_TYPE_3DPIPELINESUPPORT, NULL, &depth, sizeof(depth)};
#ifdef ENTANGLED_D3D12_PROBE_DDI
    CHECK(functions.pfnGetCaps(args.hAdapter, &caps) == S_OK);
    CHECK(depth == D3D12DDI_3DPIPELINELEVEL_11_0);
    D3D12DDI_D3D12_OPTIONS_DATA_0033 options;
    caps = (D3D12DDIARG_GETCAPS){D3D12DDICAPS_TYPE_D3D12_OPTIONS, NULL, &options, sizeof(options) - 1};
    CHECK(functions.pfnGetCaps(args.hAdapter, &caps) == E_INVALIDARG);
    caps.DataSize = sizeof(options);
    CHECK(functions.pfnGetCaps(args.hAdapter, &caps) == S_OK);
    CHECK(options.ResourceBindingTier == D3D12DDI_RESOURCE_BINDING_TIER_1);
    CHECK(options.ResourceHeapTier == D3D12DDI_RESOURCE_HEAP_TIER_1);
#else
    CHECK(functions.pfnGetCaps(args.hAdapter, &caps) == DXGI_ERROR_UNSUPPORTED);
    CHECK(depth == UINT_MAX);
#endif

    D3D12DDIARG_OPENADAPTER second = {0};
    D3D12DDI_ADAPTERFUNCS second_functions = {0};
    second.pAdapterCallbacks = &callbacks;
    second.pAdapterFuncs = &second_functions;
    CHECK(open_adapter(&second) == S_OK);
    CHECK(second.hAdapter.pDrvPrivate && second.hAdapter.pDrvPrivate != args.hAdapter.pDrvPrivate);
    CHECK(functions.pfnCloseAdapter(args.hAdapter) == S_OK);
    CHECK(second_functions.pfnGetSupportedVersions(second.hAdapter, &count, NULL) == S_OK && count == expected_versions);
    CHECK(second_functions.pfnCloseAdapter(second.hAdapter) == S_OK);
    printf("D3D12 DDI PASS: independent Venus adapters, logical device lifecycle, %u DDI versions\n", count);
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
