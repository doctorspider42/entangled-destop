/* SPDX-License-Identifier: Apache-2.0 */
typedef struct DdiRootSignature {
    D3D12DDI_ROOT_SIGNATURE_0013 desc;
    UINT node_mask;
    VkPipelineLayout empty_layout;
#ifdef ENTANGLED_D3D12_PROBE_SHADER
    VkPipelineLayout uniform_pipeline_layout;
    VkDescriptorSetLayout uniform_set_layout;
#endif
} DdiRootSignature;

#ifdef ENTANGLED_D3D12_PROBE_SHADER
static BOOL single_pixel_cbv_table(const D3D12DDI_ROOT_SIGNATURE_0013 *desc)
{
    if (desc->NumParameters != 1 || desc->NumStaticSamplers)
        return FALSE;
    const D3D12DDI_ROOT_PARAMETER_0013 *parameter = desc->pRootParameters;
    if (parameter->ParameterType != D3D12DDI_ROOT_PARAMETER_TYPE_DESCRIPTOR_TABLE ||
        parameter->ShaderVisibility != D3D12DDI_SHADER_VISIBILITY_PIXEL ||
        parameter->DescriptorTable.NumDescriptorRanges != 1)
        return FALSE;
    const D3D12DDI_DESCRIPTOR_RANGE_0013 *range = parameter->DescriptorTable.pDescriptorRanges;
    return range->RangeType == D3D12DDI_DESCRIPTOR_RANGE_TYPE_CBV && range->NumDescriptors == 1 &&
           !range->BaseShaderRegister && !range->RegisterSpace &&
           (!range->OffsetInDescriptorsFromTableStart || range->OffsetInDescriptorsFromTableStart == UINT32_MAX);
}

static HRESULT create_uniform_root_layout(const VenusDevice *venus, DdiRootSignature *root)
{
    const PFN_vkCreateDescriptorSetLayout create_set = (PFN_vkCreateDescriptorSetLayout)
        venus->proc_addr(venus->handle, "vkCreateDescriptorSetLayout");
    const PFN_vkDestroyDescriptorSetLayout destroy_set = (PFN_vkDestroyDescriptorSetLayout)
        venus->proc_addr(venus->handle, "vkDestroyDescriptorSetLayout");
    const PFN_vkCreatePipelineLayout create_layout = (PFN_vkCreatePipelineLayout)
        venus->proc_addr(venus->handle, "vkCreatePipelineLayout");
    if (!create_set || !destroy_set || !create_layout || !venus->proc_addr(venus->handle, "vkDestroyPipelineLayout"))
        return E_FAIL;
    const VkDescriptorSetLayoutBinding binding = {
        .binding = 0, .descriptorType = VK_DESCRIPTOR_TYPE_UNIFORM_BUFFER,
        .descriptorCount = 1, .stageFlags = VK_SHADER_STAGE_FRAGMENT_BIT,
    };
    const VkDescriptorSetLayoutCreateInfo set = {
        .sType = VK_STRUCTURE_TYPE_DESCRIPTOR_SET_LAYOUT_CREATE_INFO, .bindingCount = 1, .pBindings = &binding,
    };
    VkDescriptorSetLayout set_layout;
    const VkResult result = create_set(venus->handle, &set, NULL, &set_layout);
    if (result != VK_SUCCESS)
        return venus_hresult(result);
    const VkPipelineLayoutCreateInfo pipeline_info = {
        .sType = VK_STRUCTURE_TYPE_PIPELINE_LAYOUT_CREATE_INFO, .setLayoutCount = 1, .pSetLayouts = &set_layout,
    };
    VkPipelineLayout pipeline_layout;
    const VkResult pipeline = create_layout(venus->handle, &pipeline_info, NULL, &pipeline_layout);
    if (pipeline != VK_SUCCESS) {
        destroy_set(venus->handle, set_layout, NULL);
        return venus_hresult(pipeline);
    }
    root->uniform_set_layout = set_layout;
    root->uniform_pipeline_layout = pipeline_layout;
    return S_OK;
}
#endif

static BOOL add_root_array_size(SIZE_T *size, UINT count, SIZE_T element_size, const void *data)
{
    if ((count && !data) || count > (SIZE_MAX - *size) / element_size)
        return FALSE;
    *size += count * element_size;
    return TRUE;
}

static SIZE_T APIENTRY private_root_signature_size(D3D12DDI_HDEVICE device,
                                                    const D3D12DDIARG_CREATE_ROOT_SIGNATURE_0013 *args)
{
    if (!device.pDrvPrivate || !args || args->Version != D3D12DDI_ROOT_SIGNATURE_VERSION_1_1 ||
        args->NodeMask > 1 || !args->pRootSignature_1_1)
        return 0;

    const D3D12DDI_ROOT_SIGNATURE_0013 *desc = args->pRootSignature_1_1;
    SIZE_T size = sizeof(DdiRootSignature);
    if (!add_root_array_size(&size, desc->NumParameters, sizeof(*desc->pRootParameters), desc->pRootParameters) ||
        !add_root_array_size(&size, desc->NumStaticSamplers, sizeof(*desc->pStaticSamplers), desc->pStaticSamplers))
        return 0;

    for (UINT i = 0; i < desc->NumParameters; ++i) {
        const D3D12DDI_ROOT_PARAMETER_0013 *parameter = &desc->pRootParameters[i];
        if (parameter->ParameterType == D3D12DDI_ROOT_PARAMETER_TYPE_DESCRIPTOR_TABLE) {
            const D3D12DDI_ROOT_DESCRIPTOR_TABLE_0013 *table = &parameter->DescriptorTable;
            if (!add_root_array_size(&size, table->NumDescriptorRanges,
                                     sizeof(*table->pDescriptorRanges), table->pDescriptorRanges))
                return 0;
        }
    }
    return size;
}

static HRESULT APIENTRY create_root_signature(D3D12DDI_HDEVICE device,
                                               const D3D12DDIARG_CREATE_ROOT_SIGNATURE_0013 *args,
                                               D3D12DDI_HROOTSIGNATURE handle)
{
    DdiRootSignature *root = handle.pDrvPrivate;
    if (!root || !private_root_signature_size(device, args))
        return E_INVALIDARG;

    const D3D12DDI_ROOT_SIGNATURE_0013 *desc = args->pRootSignature_1_1;
    D3D12DDI_ROOT_PARAMETER_0013 *parameters = (void *)(root + 1);
    D3D12DDI_STATIC_SAMPLER *samplers = (void *)(parameters + desc->NumParameters);
    D3D12DDI_DESCRIPTOR_RANGE_0013 *ranges = (void *)(samplers + desc->NumStaticSamplers);

    *root = (DdiRootSignature){ .desc = *desc, .node_mask = args->NodeMask };
    root->desc.pRootParameters = desc->NumParameters ? parameters : NULL;
    root->desc.pStaticSamplers = desc->NumStaticSamplers ? samplers : NULL;
    if (desc->NumStaticSamplers)
        memcpy(samplers, desc->pStaticSamplers, desc->NumStaticSamplers * sizeof(*samplers));

    for (UINT i = 0; i < desc->NumParameters; ++i) {
        parameters[i] = desc->pRootParameters[i];
        if (parameters[i].ParameterType != D3D12DDI_ROOT_PARAMETER_TYPE_DESCRIPTOR_TABLE)
            continue;

        D3D12DDI_ROOT_DESCRIPTOR_TABLE_0013 *table = &parameters[i].DescriptorTable;
        if (table->NumDescriptorRanges) {
            memcpy(ranges, table->pDescriptorRanges, table->NumDescriptorRanges * sizeof(*ranges));
            table->pDescriptorRanges = ranges;
            ranges += table->NumDescriptorRanges;
        } else {
            table->pDescriptorRanges = NULL;
        }
    }
    if (!desc->NumParameters && !desc->NumStaticSamplers) {
        const VenusDevice *venus = venus_device(device);
        const PFN_vkCreatePipelineLayout create_layout = (PFN_vkCreatePipelineLayout)
            venus->proc_addr(venus->handle, "vkCreatePipelineLayout");
        const PFN_vkDestroyPipelineLayout destroy_layout = (PFN_vkDestroyPipelineLayout)
            venus->proc_addr(venus->handle, "vkDestroyPipelineLayout");
        if (!create_layout || !destroy_layout)
            return E_FAIL;
        const VkPipelineLayoutCreateInfo info = { .sType = VK_STRUCTURE_TYPE_PIPELINE_LAYOUT_CREATE_INFO };
        VkPipelineLayout layout;
        const VkResult result = create_layout(venus->handle, &info, NULL, &layout);
        if (result != VK_SUCCESS)
            return venus_hresult(result);
        root->empty_layout = layout;
    }
#ifdef ENTANGLED_D3D12_PROBE_SHADER
    if (single_pixel_cbv_table(desc)) {
        const HRESULT status = create_uniform_root_layout(venus_device(device), root);
        trace_ddi("CreateUniformRootLayout", (UINT)status, root->uniform_set_layout != VK_NULL_HANDLE);
        if (FAILED(status))
            return status;
    }
#endif
    trace_ddi("CreateRootSignature", desc->NumParameters, desc->NumStaticSamplers);
    return S_OK;
}

static void APIENTRY destroy_root_signature(D3D12DDI_HDEVICE device, D3D12DDI_HROOTSIGNATURE handle)
{
    DdiRootSignature *root = handle.pDrvPrivate;
    if (!root)
        return;
    if (root->empty_layout) {
        const VenusDevice *venus = venus_device(device);
        const PFN_vkDestroyPipelineLayout destroy_layout = (PFN_vkDestroyPipelineLayout)
            venus->proc_addr(venus->handle, "vkDestroyPipelineLayout");
        destroy_layout(venus->handle, root->empty_layout, NULL);
    }
#ifdef ENTANGLED_D3D12_PROBE_SHADER
    if (root->uniform_set_layout) {
        const VenusDevice *venus = venus_device(device);
        const PFN_vkDestroyPipelineLayout destroy_layout = (PFN_vkDestroyPipelineLayout)
            venus->proc_addr(venus->handle, "vkDestroyPipelineLayout");
        const PFN_vkDestroyDescriptorSetLayout destroy_set = (PFN_vkDestroyDescriptorSetLayout)
            venus->proc_addr(venus->handle, "vkDestroyDescriptorSetLayout");
        destroy_layout(venus->handle, root->uniform_pipeline_layout, NULL);
        destroy_set(venus->handle, root->uniform_set_layout, NULL);
    }
#endif
    *root = (DdiRootSignature){0};
}
