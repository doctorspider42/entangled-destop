/* SPDX-License-Identifier: Apache-2.0 */
enum {
    DDI_COMPUTE_BUFFER_BINDINGS = 16,
    DDI_COMPUTE_CONSTANT_WORDS = 32, /* Vulkan guarantees 128 bytes of push constants. */
};

typedef struct DdiComputeBufferBinding {
    UINT parameter;
    UINT table_offset;
    D3D12DDI_DESCRIPTOR_RANGE_TYPE type;
} DdiComputeBufferBinding;

typedef struct DdiComputeLayout {
    UINT buffer_count;
    DdiComputeBufferBinding buffers[DDI_COMPUTE_BUFFER_BINDINGS];
    UINT constant_words;
} DdiComputeLayout;

typedef struct DdiComputeRemap {
    const DdiRootSignature *root;
    DdiComputeLayout *layout;
} DdiComputeRemap;

static UINT compute_constant_words(const DdiRootSignature *root)
{
    UINT words = 0;
    for (UINT i = 0; i < root->desc.NumParameters; ++i) {
        const D3D12DDI_ROOT_PARAMETER_0013 *param = &root->desc.pRootParameters[i];
        if (param->ShaderVisibility == D3D12DDI_SHADER_VISIBILITY_ALL &&
            param->ParameterType == D3D12DDI_ROOT_PARAMETER_TYPE_32BIT_CONSTANTS)
            words += param->Constants.Num32BitValues;
    }
    return words;
}

static dxil_spv_bool remap_compute_constant(void *context, const dxil_spv_d3d_binding *d3d,
                                            dxil_spv_cbv_vulkan_binding *vulkan)
{
    const DdiComputeRemap *remap = context;
    if (d3d->stage != DXIL_SPV_STAGE_COMPUTE || d3d->range_size != 1)
        return 0;
    UINT offset = 0;
    for (UINT i = 0; i < remap->root->desc.NumParameters; ++i) {
        const D3D12DDI_ROOT_PARAMETER_0013 *param = &remap->root->desc.pRootParameters[i];
        if (param->ShaderVisibility != D3D12DDI_SHADER_VISIBILITY_ALL ||
            param->ParameterType != D3D12DDI_ROOT_PARAMETER_TYPE_32BIT_CONSTANTS)
            continue;
        if (param->Constants.ShaderRegister == d3d->register_index && param->Constants.RegisterSpace == d3d->register_space) {
            *vulkan = (dxil_spv_cbv_vulkan_binding){
                .vulkan.push_constant.offset_in_words = offset,
                .push_constant = TRUE,
            };
            return 1;
        }
        offset += param->Constants.Num32BitValues;
    }
    return 0;
}

static BOOL compute_table_binding(const D3D12DDI_ROOT_DESCRIPTOR_TABLE_0013 *table,
                                  const dxil_spv_d3d_binding *d3d, D3D12DDI_DESCRIPTOR_RANGE_TYPE type,
                                  UINT *table_offset)
{
    UINT64 next_offset = 0;
    for (UINT i = 0; i < table->NumDescriptorRanges; ++i) {
        const D3D12DDI_DESCRIPTOR_RANGE_0013 *range = &table->pDescriptorRanges[i];
        const UINT64 offset = range->OffsetInDescriptorsFromTableStart == UINT32_MAX
            ? next_offset : range->OffsetInDescriptorsFromTableStart;
        if (range->RangeType == type && range->RegisterSpace == d3d->register_space &&
            d3d->register_index >= range->BaseShaderRegister) {
            const UINT register_offset = d3d->register_index - range->BaseShaderRegister;
            if (register_offset < range->NumDescriptors && offset + register_offset <= UINT32_MAX) {
                *table_offset = (UINT)(offset + register_offset);
                return TRUE;
            }
        }
        next_offset = offset + range->NumDescriptors;
    }
    return FALSE;
}

static dxil_spv_bool remap_compute_buffer(DdiComputeRemap *remap, const dxil_spv_d3d_binding *d3d,
                                          D3D12DDI_DESCRIPTOR_RANGE_TYPE type, dxil_spv_vulkan_binding *vulkan)
{
    if (d3d->stage != DXIL_SPV_STAGE_COMPUTE || d3d->range_size != 1 ||
        (d3d->kind != DXIL_SPV_RESOURCE_KIND_RAW_BUFFER && d3d->kind != DXIL_SPV_RESOURCE_KIND_STRUCTURED_BUFFER))
        return 0;
    DdiComputeLayout *layout = remap->layout;
    for (UINT i = 0; i < remap->root->desc.NumParameters; ++i) {
        const D3D12DDI_ROOT_PARAMETER_0013 *param = &remap->root->desc.pRootParameters[i];
        UINT table_offset;
        if (param->ShaderVisibility != D3D12DDI_SHADER_VISIBILITY_ALL ||
            param->ParameterType != D3D12DDI_ROOT_PARAMETER_TYPE_DESCRIPTOR_TABLE ||
            !compute_table_binding(&param->DescriptorTable, d3d, type, &table_offset))
            continue;

        UINT index = 0;
        while (index < layout->buffer_count &&
               (layout->buffers[index].parameter != i || layout->buffers[index].table_offset != table_offset))
            ++index;
        if (index == DDI_COMPUTE_BUFFER_BINDINGS)
            return 0;
        if (index == layout->buffer_count) {
            layout->buffers[index] = (DdiComputeBufferBinding){ i, table_offset, type };
            ++layout->buffer_count;
        }
        *vulkan = (dxil_spv_vulkan_binding){ .binding = index, .descriptor_type = DXIL_SPV_VULKAN_DESCRIPTOR_TYPE_SSBO };
        return 1;
    }
    return 0;
}

static dxil_spv_bool remap_compute_srv(void *context, const dxil_spv_d3d_binding *d3d,
                                       dxil_spv_srv_vulkan_binding *vulkan)
{
    *vulkan = (dxil_spv_srv_vulkan_binding){0};
    return remap_compute_buffer(context, d3d, D3D12DDI_DESCRIPTOR_RANGE_TYPE_SRV, &vulkan->buffer_binding);
}

static dxil_spv_bool remap_compute_uav(void *context, const dxil_spv_uav_d3d_binding *d3d,
                                       dxil_spv_uav_vulkan_binding *vulkan)
{
    *vulkan = (dxil_spv_uav_vulkan_binding){0};
    return !d3d->has_counter &&
        remap_compute_buffer(context, &d3d->d3d_binding, D3D12DDI_DESCRIPTOR_RANGE_TYPE_UAV, &vulkan->buffer_binding);
}
