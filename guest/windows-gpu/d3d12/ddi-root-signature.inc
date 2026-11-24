/* SPDX-License-Identifier: Apache-2.0 */
typedef struct DdiRootSignature {
    D3D12DDI_ROOT_SIGNATURE_0013 desc;
    UINT node_mask;
    VkPipelineLayout empty_layout;
} DdiRootSignature;

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
        const VenusDevice *venus = device.pDrvPrivate;
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
    trace_ddi("CreateRootSignature", desc->NumParameters, desc->NumStaticSamplers);
    return S_OK;
}

static void APIENTRY destroy_root_signature(D3D12DDI_HDEVICE device, D3D12DDI_HROOTSIGNATURE handle)
{
    DdiRootSignature *root = handle.pDrvPrivate;
    if (!root)
        return;
    if (root->empty_layout) {
        const VenusDevice *venus = device.pDrvPrivate;
        const PFN_vkDestroyPipelineLayout destroy_layout = (PFN_vkDestroyPipelineLayout)
            venus->proc_addr(venus->handle, "vkDestroyPipelineLayout");
        destroy_layout(venus->handle, root->empty_layout, NULL);
    }
    *root = (DdiRootSignature){0};
}
