/* SPDX-License-Identifier: Apache-2.0 */
enum { DDI_GRAPHICS_BINDINGS = 24, DDI_GRAPHICS_ROOT_WORDS = 64 };

typedef struct DdiGraphicsBinding {
    UINT parameter;
    UINT table_offset;
    UINT count;
    VkDescriptorType type;
    VkShaderStageFlags stages;
    BOOL root_address;
} DdiGraphicsBinding;

typedef struct DdiGraphicsLayout {
    UINT count;
    DdiGraphicsBinding bindings[DDI_GRAPHICS_BINDINGS];
    UINT constant_words;
    VkShaderStageFlags constant_stages;
} DdiGraphicsLayout;

typedef struct DdiGraphicsRemap {
    const DdiRootSignature *root;
    DdiGraphicsLayout *layout;
} DdiGraphicsRemap;

static VkShaderStageFlags graphics_visibility(D3D12DDI_SHADER_VISIBILITY visibility)
{
    switch (visibility) {
    case D3D12DDI_SHADER_VISIBILITY_ALL: return VK_SHADER_STAGE_VERTEX_BIT | VK_SHADER_STAGE_FRAGMENT_BIT;
    case D3D12DDI_SHADER_VISIBILITY_VERTEX: return VK_SHADER_STAGE_VERTEX_BIT;
    case D3D12DDI_SHADER_VISIBILITY_PIXEL: return VK_SHADER_STAGE_FRAGMENT_BIT;
    default: return 0;
    }
}

static VkShaderStageFlags graphics_shader_stage(dxil_spv_shader_stage stage)
{
    return stage == DXIL_SPV_STAGE_VERTEX ? VK_SHADER_STAGE_VERTEX_BIT :
           stage == DXIL_SPV_STAGE_PIXEL ? VK_SHADER_STAGE_FRAGMENT_BIT : 0;
}

static UINT graphics_constant_words(const DdiRootSignature *root, UINT parameters)
{
    UINT words = 0;
    for (UINT i = 0; i < parameters; ++i) {
        const D3D12DDI_ROOT_PARAMETER_0013 *param = &root->desc.pRootParameters[i];
        if (param->ParameterType == D3D12DDI_ROOT_PARAMETER_TYPE_32BIT_CONSTANTS)
            words += param->Constants.Num32BitValues;
    }
    return words;
}

static BOOL graphics_table_slot(const D3D12DDI_ROOT_DESCRIPTOR_TABLE_0013 *table, const dxil_spv_d3d_binding *d3d,
                                D3D12DDI_DESCRIPTOR_RANGE_TYPE type, UINT *slot, UINT *count)
{
    UINT64 next = 0;
    for (UINT i = 0; i < table->NumDescriptorRanges; ++i) {
        const D3D12DDI_DESCRIPTOR_RANGE_0013 *range = &table->pDescriptorRanges[i];
        const UINT64 offset = range->OffsetInDescriptorsFromTableStart == UINT32_MAX
            ? next : range->OffsetInDescriptorsFromTableStart;
        next = offset + range->NumDescriptors;
        if (range->RangeType != type || range->RegisterSpace != d3d->register_space ||
            d3d->register_index < range->BaseShaderRegister)
            continue;
        const UINT index = d3d->register_index - range->BaseShaderRegister;
        if (index >= range->NumDescriptors || offset + index > UINT32_MAX)
            continue;
        const UINT available = range->NumDescriptors - index;
        const UINT elements = d3d->range_size == UINT32_MAX ? available : d3d->range_size;
        if (!elements || elements > available)
            return FALSE;
        *slot = (UINT)(offset + index);
        *count = elements;
        return TRUE;
    }
    return FALSE;
}

static BOOL add_graphics_binding(DdiGraphicsLayout *layout, DdiGraphicsBinding binding, UINT *index)
{
    for (UINT i = 0; i < layout->count; ++i) {
        DdiGraphicsBinding *current = &layout->bindings[i];
        if (current->parameter == binding.parameter && current->table_offset == binding.table_offset &&
            current->count == binding.count && current->type == binding.type && current->root_address == binding.root_address) {
            current->stages |= binding.stages;
            *index = i;
            return TRUE;
        }
    }
    if (layout->count == _countof(layout->bindings))
        return FALSE;
    *index = layout->count++;
    layout->bindings[*index] = binding;
    return TRUE;
}

static BOOL graphics_descriptor(DdiGraphicsRemap *remap, const dxil_spv_d3d_binding *d3d, VkDescriptorType type,
                                D3D12DDI_DESCRIPTOR_RANGE_TYPE range_type, UINT *index)
{
    const VkShaderStageFlags stage = graphics_shader_stage(d3d->stage);
    for (UINT i = 0; i < remap->root->desc.NumParameters; ++i) {
        const D3D12DDI_ROOT_PARAMETER_0013 *param = &remap->root->desc.pRootParameters[i];
        if (!(graphics_visibility(param->ShaderVisibility) & stage))
            continue;
        DdiGraphicsBinding binding = { .parameter = i, .type = type, .stages = stage, .count = 1 };
        if (param->ParameterType == D3D12DDI_ROOT_PARAMETER_TYPE_DESCRIPTOR_TABLE) {
            if (!graphics_table_slot(&param->DescriptorTable, d3d, range_type, &binding.table_offset, &binding.count) ||
                (binding.count > 1 && (type != VK_DESCRIPTOR_TYPE_SAMPLER ||
                                      binding.count > D3D12_MAX_SHADER_VISIBLE_SAMPLER_HEAP_SIZE)))
                continue;
        } else if (param->ParameterType == D3D12DDI_ROOT_PARAMETER_TYPE_CBV &&
                   type == VK_DESCRIPTOR_TYPE_UNIFORM_BUFFER && d3d->range_size == 1 &&
                   param->Descriptor.ShaderRegister == d3d->register_index &&
                   param->Descriptor.RegisterSpace == d3d->register_space) {
            binding.root_address = TRUE;
        } else {
            continue;
        }
        return add_graphics_binding(remap->layout, binding, index);
    }
    trace_ddi("MissingGraphicsBinding", type, ((UINT64)d3d->register_space << 32) | d3d->register_index);
    return FALSE;
}

static dxil_spv_bool remap_graphics_cbv(void *context, const dxil_spv_d3d_binding *d3d,
                                        dxil_spv_cbv_vulkan_binding *vulkan)
{
    DdiGraphicsRemap *remap = context;
    const VkShaderStageFlags stage = graphics_shader_stage(d3d->stage);
    for (UINT i = 0; i < remap->root->desc.NumParameters; ++i) {
        const D3D12DDI_ROOT_PARAMETER_0013 *param = &remap->root->desc.pRootParameters[i];
        if (param->ParameterType == D3D12DDI_ROOT_PARAMETER_TYPE_32BIT_CONSTANTS && d3d->range_size == 1 &&
            (graphics_visibility(param->ShaderVisibility) & stage) && param->Constants.ShaderRegister == d3d->register_index &&
            param->Constants.RegisterSpace == d3d->register_space) {
            *vulkan = (dxil_spv_cbv_vulkan_binding){
                .vulkan.push_constant.offset_in_words = graphics_constant_words(remap->root, i), .push_constant = TRUE,
            };
            remap->layout->constant_stages |= stage;
            return 1;
        }
    }
    UINT index;
    if (!graphics_descriptor(remap, d3d, VK_DESCRIPTOR_TYPE_UNIFORM_BUFFER, D3D12DDI_DESCRIPTOR_RANGE_TYPE_CBV, &index))
        return 0;
    *vulkan = (dxil_spv_cbv_vulkan_binding){
        .vulkan.uniform_binding = { .binding = index, .descriptor_type = DXIL_SPV_VULKAN_DESCRIPTOR_TYPE_UBO },
    };
    return 1;
}

static dxil_spv_bool remap_graphics_srv(void *context, const dxil_spv_d3d_binding *d3d,
                                        dxil_spv_srv_vulkan_binding *vulkan)
{
    const BOOL buffer = d3d->kind == DXIL_SPV_RESOURCE_KIND_RAW_BUFFER || d3d->kind == DXIL_SPV_RESOURCE_KIND_STRUCTURED_BUFFER;
    if (!buffer && d3d->kind != DXIL_SPV_RESOURCE_KIND_TEXTURE_2D)
        return 0;
    const VkDescriptorType type = buffer ? VK_DESCRIPTOR_TYPE_STORAGE_BUFFER : VK_DESCRIPTOR_TYPE_SAMPLED_IMAGE;
    UINT index;
    if (!graphics_descriptor(context, d3d, type, D3D12DDI_DESCRIPTOR_RANGE_TYPE_SRV, &index))
        return 0;
    *vulkan = (dxil_spv_srv_vulkan_binding){ .buffer_binding = { .binding = index,
        .descriptor_type = buffer ? DXIL_SPV_VULKAN_DESCRIPTOR_TYPE_SSBO_SCALAR_ALIGNED
                                  : DXIL_SPV_VULKAN_DESCRIPTOR_TYPE_IDENTITY } };
    return 1;
}

static dxil_spv_bool remap_graphics_sampler(void *context, const dxil_spv_d3d_binding *d3d,
                                            dxil_spv_vulkan_binding *vulkan)
{
    UINT index;
    if (!graphics_descriptor(context, d3d, VK_DESCRIPTOR_TYPE_SAMPLER, D3D12DDI_DESCRIPTOR_RANGE_TYPE_SAMPLER, &index))
        return 0;
    *vulkan = (dxil_spv_vulkan_binding){ .binding = index };
    return 1;
}
