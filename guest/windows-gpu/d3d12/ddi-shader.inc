/* SPDX-License-Identifier: Apache-2.0 */
#include <wchar.h>
#include <dxil_spirv_c.h>

typedef struct DxbcHeader {
    char tag[4];
    UINT32 checksum[4];
    UINT32 version;
    UINT32 size;
    UINT32 chunk_count;
    UINT32 chunk_offsets[3];
} DxbcHeader;

typedef struct DxbcChunkHeader {
    char tag[4];
    UINT32 size;
} DxbcChunkHeader;

typedef struct DxbcSignatureEntry {
    UINT32 name_offset;
    UINT32 semantic_index;
    UINT32 system_value;
    UINT32 component_type;
    UINT32 register_index;
    UINT32 mask;
} DxbcSignatureEntry;

typedef struct DdiShader {
    VkShaderModule module;
    VkShaderStageFlagBits stage;
    UINT pixel_bindings;
} DdiShader;

typedef enum DxbcStage {
    DXBC_PIXEL = D3D10_SB_PIXEL_SHADER,
    DXBC_VERTEX = D3D10_SB_VERTEX_SHADER,
    DXBC_COMPUTE = 5, /* Token value added for Shader Model 5. */
} DxbcStage;

static const char *shader_semantic(DxbcStage stage, BOOL input, D3D10_SB_NAME value)
{
    switch (value) {
    case D3D10_SB_NAME_UNDEFINED: return stage == DXBC_PIXEL && !input ? "SV_Target" : "TEXCOORD";
    case D3D10_SB_NAME_POSITION: return "SV_Position";
    case D3D10_SB_NAME_VERTEX_ID: return "SV_VertexID";
    case D3D10_SB_NAME_INSTANCE_ID: return "SV_InstanceID";
    default: return NULL;
    }
}

static BOOL can_write_signature(const D3D12DDIARG_SIGNATURE_ENTRY_0012 *entries, UINT count, DxbcStage stage, BOOL input)
{
    if (count > 128 || (count && !entries))
        return FALSE;
    for (UINT i = 0; i < count; ++i) {
        if (entries[i].Stream || entries[i].MinPrecision || entries[i].Mask > 0xf ||
            !shader_semantic(stage, input, entries[i].SystemValue))
            return FALSE;
    }
    return TRUE;
}

static UINT32 signature_chunk_size(UINT count)
{
    return (UINT32)(2 * sizeof(UINT32) + count * (sizeof(DxbcSignatureEntry) + 16));
}

static void write_signature(BYTE *data, const char tag[4], const D3D12DDIARG_SIGNATURE_ENTRY_0012 *entries,
                            UINT count, DxbcStage stage, BOOL input)
{
    DxbcChunkHeader *chunk = (void *)data;
    memcpy(chunk->tag, tag, sizeof(chunk->tag));
    chunk->size = signature_chunk_size(count);
    UINT32 *header = (void *)(chunk + 1);
    header[0] = count;
    header[1] = 2 * sizeof(UINT32);
    DxbcSignatureEntry *params = (void *)(header + 2);
    char *names = (void *)(params + count);
    for (UINT i = 0; i < count; ++i) {
        const D3D12DDIARG_SIGNATURE_ENTRY_0012 *entry = &entries[i];
        const char *semantic = shader_semantic(stage, input, entry->SystemValue);
        params[i] = (DxbcSignatureEntry){
            .name_offset = (UINT32)(names - (char *)header),
            .semantic_index = entry->SystemValue == D3D10_SB_NAME_UNDEFINED ? entry->Register : 0,
            .system_value = entry->SystemValue,
            .component_type = entry->RegisterComponentType,
            .register_index = entry->Register,
            .mask = entry->Mask | (input ? (UINT32)entry->Mask << 8 : 0),
        };
        memcpy(names, semantic, strlen(semantic) + 1);
        names += 16;
    }
}

static HRESULT make_legacy_dxbc(const D3D12DDIARG_CREATE_SHADER_0026 *args, DxbcStage stage, DxbcHeader **output)
{
    const UINT *code = args->pShaderCode;
    if (!code || (UINT)DECODE_D3D10_SB_TOKENIZED_PROGRAM_TYPE(code[0]) != (UINT)stage)
        return DXGI_ERROR_UNSUPPORTED;
    const UINT major = DECODE_D3D10_SB_TOKENIZED_PROGRAM_MAJOR_VERSION(code[0]);
    if (major != 5 && (stage == DXBC_COMPUTE || major != 4))
        return DXGI_ERROR_UNSUPPORTED;
    const UINT words = DECODE_D3D10_SB_TOKENIZED_PROGRAM_LENGTH(code[1]);
    const D3D12DDIARG_STAGE_IO_SIGNATURES empty = {0};
    const D3D12DDIARG_STAGE_IO_SIGNATURES *io = stage == DXBC_COMPUTE ? &empty : args->IOSignatures.Standard;
    if (!io || words < 2 || words > 4 * 1024 * 1024 || args->Flags ||
        !can_write_signature(io->pInputSignature, io->NumInputSignatureEntries, stage, TRUE) ||
        !can_write_signature(io->pOutputSignature, io->NumOutputSignatureEntries, stage, FALSE))
        return DXGI_ERROR_UNSUPPORTED;

    const UINT32 input = sizeof(DxbcHeader);
    const UINT32 chunk_header = sizeof(DxbcChunkHeader);
    const UINT32 output_sig = input + chunk_header + signature_chunk_size(io->NumInputSignatureEntries);
    const UINT32 code_chunk = output_sig + chunk_header + signature_chunk_size(io->NumOutputSignatureEntries);
    const UINT32 code_bytes = words * (UINT32)sizeof(UINT);
    const UINT32 size = code_chunk + chunk_header + code_bytes;
    DxbcHeader *blob = HeapAlloc(GetProcessHeap(), HEAP_ZERO_MEMORY, size);
    if (!blob)
        return E_OUTOFMEMORY;
    *blob = (DxbcHeader){
        .tag = {'D', 'X', 'B', 'C'},
        .version = 1,
        .size = size,
        .chunk_count = 3,
        .chunk_offsets = {input, output_sig, code_chunk},
    };
    BYTE *bytes = (void *)blob;
    write_signature(bytes + input, "ISGN", io->pInputSignature, io->NumInputSignatureEntries, stage, TRUE);
    write_signature(bytes + output_sig, "OSGN", io->pOutputSignature, io->NumOutputSignatureEntries, stage, FALSE);
    DxbcChunkHeader *chunk = (void *)(bytes + code_chunk);
    memcpy(chunk->tag, major == 4 ? "SHDR" : "SHEX", sizeof(chunk->tag));
    chunk->size = code_bytes;
    memcpy(chunk + 1, code, code_bytes);
    *output = blob;
    return S_OK;
}

static HMODULE load_shader_compiler(void)
{
    HMODULE umd;
    if (!GetModuleHandleExW(GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS | GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
                            (LPCWSTR)(const void *)load_shader_compiler, &umd))
        return NULL;
    wchar_t path[MAX_PATH];
    const DWORD length = GetModuleFileNameW(umd, path, ARRAYSIZE(path));
    if (!length || length >= ARRAYSIZE(path))
        return NULL;
    wchar_t *name = wcsrchr(path, L'\\');
    if (!name || wcscpy_s(name + 1, ARRAYSIZE(path) - (size_t)(name + 1 - path), L"dxil-spirv-c-shared.dll"))
        return NULL;
    return LoadLibraryExW(path, NULL, LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR | LOAD_LIBRARY_SEARCH_SYSTEM32);
}

static BOOL single_pixel_register(const dxil_spv_d3d_binding *d3d)
{
    return d3d->stage == DXIL_SPV_STAGE_PIXEL && !d3d->register_space && !d3d->register_index && d3d->range_size == 1;
}

static dxil_spv_bool remap_pixel_cbv(void *context, const dxil_spv_d3d_binding *d3d,
                                     dxil_spv_cbv_vulkan_binding *vulkan)
{
    if (!single_pixel_register(d3d))
        return 0;
    *(UINT *)context |= DDI_PIXEL_CBV;
    *vulkan = (dxil_spv_cbv_vulkan_binding){
        .vulkan.uniform_binding = { .set = 0, .binding = 0, .descriptor_type = DXIL_SPV_VULKAN_DESCRIPTOR_TYPE_UBO },
    };
    return 1;
}

static dxil_spv_bool remap_pixel_srv(void *context, const dxil_spv_d3d_binding *d3d,
                                     dxil_spv_srv_vulkan_binding *vulkan)
{
    if (!single_pixel_register(d3d) || d3d->kind != DXIL_SPV_RESOURCE_KIND_TEXTURE_2D)
        return 0;
    *(UINT *)context |= DDI_PIXEL_SRV;
    *vulkan = (dxil_spv_srv_vulkan_binding){ .buffer_binding = { .set = 0, .binding = 1 } };
    return 1;
}

static dxil_spv_bool remap_pixel_sampler(void *context, const dxil_spv_d3d_binding *d3d,
                                         dxil_spv_vulkan_binding *vulkan)
{
    if (!single_pixel_register(d3d))
        return 0;
    *(UINT *)context |= DDI_PIXEL_SAMPLER;
    *vulkan = (dxil_spv_vulkan_binding){ .set = 0, .binding = 2 };
    return 1;
}

static HRESULT compile_shader_module(const VenusDevice *device, const DxbcHeader *blob, DdiShader *output)
{
    HMODULE compiler = load_shader_compiler();
    if (!compiler) {
        trace_ddi("LoadShaderCompiler", GetLastError(), 0);
        return DXGI_ERROR_UNSUPPORTED;
    }

    dxil_spv_result (*parse)(const void *, size_t, dxil_spv_parsed_blob *) =
        (dxil_spv_result (*)(const void *, size_t, dxil_spv_parsed_blob *))(void *)
        GetProcAddress(compiler, "dxil_spv_parse_dxil_blob");
    void (*free_blob)(dxil_spv_parsed_blob) = (void (*)(dxil_spv_parsed_blob))(void *)
        GetProcAddress(compiler, "dxil_spv_parsed_blob_free");
    dxil_spv_result (*create)(dxil_spv_parsed_blob, dxil_spv_converter *) =
        (dxil_spv_result (*)(dxil_spv_parsed_blob, dxil_spv_converter *))(void *)
        GetProcAddress(compiler, "dxil_spv_create_converter");
    void (*free_converter)(dxil_spv_converter) = (void (*)(dxil_spv_converter))(void *)
        GetProcAddress(compiler, "dxil_spv_converter_free");
    dxil_spv_result (*convert)(dxil_spv_converter) = (dxil_spv_result (*)(dxil_spv_converter))(void *)
        GetProcAddress(compiler, "dxil_spv_converter_run");
    dxil_spv_result (*compiled)(dxil_spv_converter, dxil_spv_compiled_spirv *) =
        (dxil_spv_result (*)(dxil_spv_converter, dxil_spv_compiled_spirv *))(void *)
        GetProcAddress(compiler, "dxil_spv_converter_get_compiled_spirv");
    void (*set_cbv)(dxil_spv_converter, dxil_spv_cbv_remapper_cb, void *) =
        (void (*)(dxil_spv_converter, dxil_spv_cbv_remapper_cb, void *))(void *)
        GetProcAddress(compiler, "dxil_spv_converter_set_cbv_remapper");
    void (*set_srv)(dxil_spv_converter, dxil_spv_srv_remapper_cb, void *) =
        (void (*)(dxil_spv_converter, dxil_spv_srv_remapper_cb, void *))(void *)
        GetProcAddress(compiler, "dxil_spv_converter_set_srv_remapper");
    void (*set_sampler)(dxil_spv_converter, dxil_spv_sampler_remapper_cb, void *) =
        (void (*)(dxil_spv_converter, dxil_spv_sampler_remapper_cb, void *))(void *)
        GetProcAddress(compiler, "dxil_spv_converter_set_sampler_remapper");
    const PFN_vkCreateShaderModule create_module = (PFN_vkCreateShaderModule)
        device->proc_addr(device->handle, "vkCreateShaderModule");
    const PFN_vkDestroyShaderModule destroy_module = (PFN_vkDestroyShaderModule)
        device->proc_addr(device->handle, "vkDestroyShaderModule");
    if (!parse || !free_blob || !create || !free_converter || !convert || !compiled || !set_cbv || !set_srv || !set_sampler ||
        !create_module || !destroy_module) {
        FreeLibrary(compiler);
        return DXGI_ERROR_UNSUPPORTED;
    }

    dxil_spv_parsed_blob parsed = NULL;
    dxil_spv_converter converter = NULL;
    dxil_spv_compiled_spirv code = {0};
    dxil_spv_result result = parse(blob, blob->size, &parsed);
    if (result == DXIL_SPV_SUCCESS)
        result = create(parsed, &converter);
    if (result == DXIL_SPV_SUCCESS) {
        set_cbv(converter, remap_pixel_cbv, &output->pixel_bindings);
        set_srv(converter, remap_pixel_srv, &output->pixel_bindings);
        set_sampler(converter, remap_pixel_sampler, &output->pixel_bindings);
        result = convert(converter);
    }
    if (result == DXIL_SPV_SUCCESS)
        result = compiled(converter, &code);

    HRESULT status = result == DXIL_SPV_ERROR_OUT_OF_MEMORY ? E_OUTOFMEMORY : E_FAIL;
    if (result == DXIL_SPV_SUCCESS && code.data &&
        code.size >= 5 * sizeof(UINT32) && code.size % sizeof(UINT32) == 0) {
        const VkShaderModuleCreateInfo info = {
            .sType = VK_STRUCTURE_TYPE_SHADER_MODULE_CREATE_INFO,
            .codeSize = code.size,
            .pCode = code.data,
        };
        VkShaderModule shader;
        const VkResult created = create_module(device->handle, &info, NULL, &shader);
        status = venus_hresult(created);
        if (created == VK_SUCCESS)
            output->module = shader;
    }
    if (converter)
        free_converter(converter);
    if (parsed)
        free_blob(parsed);
    FreeLibrary(compiler);
    return status;
}

static SIZE_T APIENTRY private_shader_size(D3D12DDI_HDEVICE device, const D3D12DDIARG_CREATE_SHADER_0026 *args)
{
    if (!device.pDrvPrivate || !args)
        return 0;
    if (args->pShaderCode)
        trace_ddi("Shader words", args->pShaderCode[0], args->pShaderCode[1]);
    return sizeof(DdiShader);
}

static HRESULT create_legacy_shader(D3D12DDI_HDEVICE device, const D3D12DDIARG_CREATE_SHADER_0026 *args,
                                     D3D12DDI_HSHADER handle, DxbcStage stage)
{
    DdiShader *shader = handle.pDrvPrivate;
    if (!shader || !device.pDrvPrivate || !args)
        return E_INVALIDARG;
    *shader = (DdiShader){0};

    DxbcHeader *blob = NULL;
    HRESULT status = make_legacy_dxbc(args, stage, &blob);
    if (SUCCEEDED(status)) {
        status = compile_shader_module(venus_device(device), blob, shader);
        HeapFree(GetProcessHeap(), 0, blob);
    }
    if (SUCCEEDED(status))
        shader->stage = stage == DXBC_COMPUTE ? VK_SHADER_STAGE_COMPUTE_BIT :
            stage == DXBC_PIXEL ? VK_SHADER_STAGE_FRAGMENT_BIT : VK_SHADER_STAGE_VERTEX_BIT;
    trace_ddi(stage == DXBC_COMPUTE ? "CreateComputeShader" :
              stage == DXBC_PIXEL ? "CreatePixelShader" : "CreateVertexShader",
              (UINT)status, shader->module != VK_NULL_HANDLE);
    return status;
}

static HRESULT APIENTRY create_vertex_shader(D3D12DDI_HDEVICE device, const D3D12DDIARG_CREATE_SHADER_0026 *args,
                                               D3D12DDI_HSHADER handle)
{
    return create_legacy_shader(device, args, handle, DXBC_VERTEX);
}

static HRESULT APIENTRY create_pixel_shader(D3D12DDI_HDEVICE device, const D3D12DDIARG_CREATE_SHADER_0026 *args,
                                            D3D12DDI_HSHADER handle)
{
    return create_legacy_shader(device, args, handle, DXBC_PIXEL);
}

static HRESULT APIENTRY create_compute_shader(D3D12DDI_HDEVICE device, const D3D12DDIARG_CREATE_SHADER_0026 *args,
                                                D3D12DDI_HSHADER handle)
{
    return create_legacy_shader(device, args, handle, DXBC_COMPUTE);
}

static void APIENTRY destroy_shader(D3D12DDI_HDEVICE device, D3D12DDI_HSHADER handle)
{
    DdiShader *shader = handle.pDrvPrivate;
    if (shader && shader->module) {
        const VenusDevice *venus = venus_device(device);
        const PFN_vkDestroyShaderModule destroy = (PFN_vkDestroyShaderModule)
            venus->proc_addr(venus->handle, "vkDestroyShaderModule");
        destroy(venus->handle, shader->module, NULL);
        shader->module = VK_NULL_HANDLE;
    }
}
