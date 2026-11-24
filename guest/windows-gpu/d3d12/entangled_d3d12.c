/* SPDX-License-Identifier: Apache-2.0 */
#include <windows.h>
#include <d3d12umddi.h>
#include <dxgi.h>
#include <stdio.h>
#include <string.h>
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

static HRESULT write_probe_shader_models(const D3D12DDIARG_GETCAPS *args)
{
    const D3D12DDI_D3D12_SHADER_MODELS_DATA_0011 *models = args->pData;
    if (!models || args->DataSize != sizeof(*models) || !models->pNumShaderModelsSupported)
        return E_INVALIDARG;
    UINT *count = models->pNumShaderModelsSupported;
    const UINT capacity = models->pShaderModelsSupported ? *count : 0;
    *count = 1;
    if (models->pShaderModelsSupported) {
        if (capacity < *count)
            return HRESULT_FROM_WIN32(ERROR_INSUFFICIENT_BUFFER);
        models->pShaderModelsSupported[0] = D3D12DDI_SHADER_MODEL_5_1_RELEASE_0011;
    }
    return S_OK;
}

static HRESULT APIENTRY get_supported_extended_features(D3D12DDI_HDEVICE device, UINT32 *count,
                                                         D3D12DDI_FEATURE_0020 *features)
{
    (void)features;
    if (!device.pDrvPrivate || !count)
        return E_INVALIDARG;
    *count = 0;
    trace_ddi("GetSupportedExtendedFeatures", 0, 0);
    return S_OK;
}

static HRESULT APIENTRY get_extended_feature_versions(D3D12DDI_HDEVICE device, D3D12DDI_FEATURE_0020 feature,
                                                        UINT32 *count, UINT32 *versions)
{
    (void)feature;
    (void)versions;
    if (!device.pDrvPrivate || !count)
        return E_INVALIDARG;
    *count = 0;
    return DXGI_ERROR_UNSUPPORTED;
}

static HRESULT APIENTRY enable_extended_feature(D3D12DDI_HDEVICE device, D3D12DDI_FEATURE_0020 feature,
                                                UINT32 version)
{
    (void)device;
    (void)feature;
    (void)version;
    return DXGI_ERROR_UNSUPPORTED;
}

static HRESULT APIENTRY set_extended_feature_callbacks(D3D12DDI_HDEVICE device, D3D12DDI_TABLE_TYPE type,
                                                       const void *table, SIZE_T size)
{
    (void)device;
    (void)table;
    trace_ddi("SetExtendedFeatureCallbacks", type, size);
    return DXGI_ERROR_UNSUPPORTED;
}

static const D3D12DDI_EXTENDED_FEATURES_FUNCS_0021 extended_features = {
    .pfnGetSupportedExtendedFeatures = get_supported_extended_features,
    .pfnGetSupportedExtendedFeatureVersions = get_extended_feature_versions,
    .pfnEnableExtendedFeature = enable_extended_feature,
    .pfnSetExtendedFeatureCallbacks = set_extended_feature_callbacks,
};
#endif

#ifdef ENTANGLED_D3D12_PROBE_COPY
static UINT APIENTRY implicit_physical_adapter_mask(D3D12DDI_HDEVICE device)
{
    return device.pDrvPrivate ? 1 : 0;
}

static void APIENTRY query_node_map(D3D12DDI_HDEVICE device, UINT physical_adapters, UINT *map)
{
    /* The render engine ordinal is separate from the physical adapter index. */
    trace_ddi("QueryNodeMap", physical_adapters, implicit_physical_adapter_mask(device));
    for (UINT i = 0; i < physical_adapters; ++i)
        map[i] = i == 0 && device.pDrvPrivate ? 0 : D3D12DDI_NODE_MAP_HIDE_NODE;
}

/* The WDK DDI headers define external C constants. Keep the callbacks in this translation unit. */
#include "ddi-root-signature.inc"
#ifdef ENTANGLED_D3D12_PROBE_SHADER
#include "ddi-shader.inc"
#endif
#include "ddi-pipeline-state.inc"
#include "ddi-buffer.inc"
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
        result = probe_d3d12_buffer_ddi(args->hDrvDevice);
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
    if (args->Type == D3D12DDICAPS_TYPE_SHADER) {
        const VenusAdapter *venus = adapter.pDrvPrivate;
        if (!venus->subgroup_size)
            return DXGI_ERROR_UNSUPPORTED;
        /* Windows 10 requires nonzero lane counts even with WaveOps disabled.
           Use one Vulkan subgroup as a conservative floor for the lab only. */
        const D3D12DDI_SHADER_CAPS_0015 shader = {
            .WaveLaneCountMin = venus->subgroup_size,
            .WaveLaneCountMax = venus->subgroup_size,
            .TotalLaneCount = venus->subgroup_size,
        };
        trace_ddi("Shader subgroup size", 0, venus->subgroup_size);
        return write_probe_caps(args, &shader, sizeof(shader));
    }
    if (args->Type == D3D12DDICAPS_TYPE_0011_SHADER_MODELS)
        return write_probe_shader_models(args);
    if (args->Type == D3D12DDICAPS_TYPE_ARCHITECTURE_INFO) {
        /* Provisional RTX lab capability. */
        const D3D12DDI_ARCHITECTURE_INFO_DATA architecture = {0};
        return write_probe_caps(args, &architecture, sizeof(architecture));
    }
    if (args->Type == D3D12DDICAPS_TYPE_MEMORY_ARCHITECTURE) {
        /* The lab targets a single discrete adapter. */
        if (!args->pInfo || *(const UINT *)args->pInfo != 0)
            return E_INVALIDARG;
        const D3D12DDI_MEMORY_ARCHITECTURE_CAPS memory = { .IOCoherent = TRUE };
        return write_probe_caps(args, &memory, sizeof(memory));
    }
    if (args->Type == D3D12DDICAPS_TYPE_0023_UMD_BASED_COMMAND_QUEUE_PRIORITY) {
        const D3D12DDICAPS_UMD_BASED_COMMAND_QUEUE_PRIORITY_DATA_0023 priority = {
            .SupportedQueueFlagsForGlobalRealtimeQueues = D3D12DDI_COMMAND_QUEUE_FLAG_NONE,
        };
        return write_probe_caps(args, &priority, sizeof(priority));
    }
    if (args->Type == D3D12DDICAPS_TYPE_0022_TEXTURE_LAYOUT && !args->pInfo) {
        const D3D12DDI_TEXTURE_LAYOUT_CAPS_0026 layout = {0};
        return write_probe_caps(args, &layout, sizeof(layout));
    }
    if (args->Type == D3D12DDICAPS_TYPE_TEXTURE_LAYOUT_SETS) {
        const UINT *query = args->pInfo;
        if (!query || query[0] != D3D12DDI_TL_ROW_MAJOR || query[1] > D3D12DDI_FUNCUNIT_COPY_DST)
            return E_INVALIDARG;
        trace_ddi("Row-major layout function", query[1], args->DataSize);
        const UINT16 placement = (UINT16)D3D12_TEXTURE_DATA_PLACEMENT_ALIGNMENT;
        const UINT16 pitch = (UINT16)D3D12_TEXTURE_DATA_PITCH_ALIGNMENT;
        const D3D12DDI_ROW_MAJOR_LAYOUT_CAPS layout = {
            .SubCaps = {
                {4, placement, pitch, pitch},
                {16, placement, pitch, pitch},
            },
        };
        return write_probe_caps(args, &layout, sizeof(layout));
    }
    if (args->Type == D3D12DDICAPS_TYPE_0022_CPU_PAGE_TABLE_FALSE_POSITIVES) {
        if (!args->pInfo || *(const UINT *)args->pInfo != 0)
            return E_INVALIDARG;
        const D3D12DDI_COMMAND_QUEUE_FLAGS queues = D3D12DDI_COMMAND_QUEUE_FLAG_NONE;
        return write_probe_caps(args, &queues, sizeof(queues));
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
    trace_ddi("FillDDITable index", type, index);
    if (type == D3D12DDI_TABLE_TYPE_0020_EXTENDED_FEATURES) {
        if (!adapter.pDrvPrivate || !table || size != sizeof(extended_features) || index)
            return E_INVALIDARG;
        memcpy(table, &extended_features, sizeof(extended_features));
        return S_OK;
    }
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
