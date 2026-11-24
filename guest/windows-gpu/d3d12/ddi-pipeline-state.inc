/* SPDX-License-Identifier: Apache-2.0 */
/* These states are value types. The pipeline consumes them when it is built. */
#define GRAPHICS_STATE_DDI(name, desc_type, handle_type, trace_name)                         \
    static SIZE_T APIENTRY private_##name##_size(D3D12DDI_HDEVICE device,                     \
                                                  const desc_type *desc)                     \
    {                                                                                       \
        return device.pDrvPrivate && desc ? sizeof(*desc) : 0;                               \
    }                                                                                       \
                                                                                            \
    static void APIENTRY create_##name(D3D12DDI_HDEVICE device, const desc_type *desc,        \
                                        handle_type handle)                                  \
    {                                                                                       \
        (void)device;                                                                       \
        desc_type *state = handle.pDrvPrivate;                                               \
        *state = *desc;                                                                      \
        trace_ddi(trace_name, 0, sizeof(*state));                                             \
    }                                                                                       \
                                                                                            \
    static void APIENTRY destroy_##name(D3D12DDI_HDEVICE device, handle_type handle)          \
    {                                                                                       \
        (void)device;                                                                       \
        (void)handle;                                                                       \
    }

GRAPHICS_STATE_DDI(blend_state, D3D12DDI_BLEND_DESC_0010, D3D12DDI_HBLENDSTATE, "CreateBlendState")
GRAPHICS_STATE_DDI(depth_stencil_state, D3D12DDI_DEPTH_STENCIL_DESC_0025, D3D12DDI_HDEPTHSTENCILSTATE,
                   "CreateDepthStencilState")
GRAPHICS_STATE_DDI(rasterizer_state, D3D12DDI_RASTERIZER_DESC_0010, D3D12DDI_HRASTERIZERSTATE,
                   "CreateRasterizerState")

#undef GRAPHICS_STATE_DDI

#ifdef ENTANGLED_D3D12_PROBE_SHADER
typedef struct DdiPipeline {
    const VenusDevice *device;
    VkPipeline pipeline;
    VkRenderPass render_pass;
} DdiPipeline;

static SIZE_T APIENTRY private_pipeline_size(D3D12DDI_HDEVICE device,
                                              const D3D12DDIARG_CREATE_PIPELINE_STATE_0033 *args)
{
    return device.pDrvPrivate && args ? sizeof(DdiPipeline) : 0;
}

static HRESULT create_compute_pipeline(const VenusDevice *venus, const D3D12DDIARG_CREATE_PIPELINE_STATE_0033 *args,
                                        DdiPipeline *state)
{
    const DdiShader *shader = args->hComputeShader.pDrvPrivate;
    const DdiRootSignature *root = args->hRootSignature.pDrvPrivate;
    if (!shader->module || shader->stage != VK_SHADER_STAGE_COMPUTE_BIT ||
        !root || !root->empty_layout || args->NodeMask > 1)
        return DXGI_ERROR_UNSUPPORTED;

    const PFN_vkCreateComputePipelines create = (PFN_vkCreateComputePipelines)
        venus->proc_addr(venus->handle, "vkCreateComputePipelines");
    const PFN_vkDestroyPipeline destroy = (PFN_vkDestroyPipeline)
        venus->proc_addr(venus->handle, "vkDestroyPipeline");
    if (!create || !destroy)
        return E_FAIL;

    const VkComputePipelineCreateInfo info = {
        .sType = VK_STRUCTURE_TYPE_COMPUTE_PIPELINE_CREATE_INFO,
        .stage = {
            .sType = VK_STRUCTURE_TYPE_PIPELINE_SHADER_STAGE_CREATE_INFO,
            .stage = VK_SHADER_STAGE_COMPUTE_BIT,
            .module = shader->module,
            .pName = "main",
        },
        .layout = root->empty_layout,
        .basePipelineIndex = -1,
    };
    VkPipeline pipeline = VK_NULL_HANDLE;
    const VkResult result = create(venus->handle, VK_NULL_HANDLE, 1, &info, NULL, &pipeline);
    if (result == VK_SUCCESS) {
        state->device = venus;
        state->pipeline = pipeline;
    } else if (pipeline) {
        destroy(venus->handle, pipeline, NULL);
    }
    const HRESULT status = venus_hresult(result);
    trace_ddi("CreateComputePipelineState", (UINT)status, state->pipeline != VK_NULL_HANDLE);
    return status;
}

static HRESULT APIENTRY create_pipeline(D3D12DDI_HDEVICE device,
                                         const D3D12DDIARG_CREATE_PIPELINE_STATE_0033 *args,
                                         D3D12DDI_HPIPELINESTATE handle, D3D12DDI_HRTPIPELINESTATE runtime)
{
    (void)runtime;
    DdiPipeline *state = handle.pDrvPrivate;
    if (!state || !private_pipeline_size(device, args))
        return E_INVALIDARG;
    *state = (DdiPipeline){0};
    const VenusDevice *venus = venus_device(device);
    if (args->hComputeShader.pDrvPrivate)
        return create_compute_pipeline(venus, args, state);
    const DdiShader *vertex = args->hVertexShader.pDrvPrivate;
    const DdiRootSignature *root = args->hRootSignature.pDrvPrivate;
    /* Windows builds a point pipeline with no attachment or sample state. */
    if (!vertex || !vertex->module || vertex->stage != VK_SHADER_STAGE_VERTEX_BIT ||
        !root || !root->empty_layout || args->hPixelShader.pDrvPrivate ||
        args->hDomainShader.pDrvPrivate || args->hHullShader.pDrvPrivate ||
        args->hGeometryShader.pDrvPrivate || args->hElementLayout.pDrvPrivate ||
        args->NumRenderTargets || args->DSVFormat != DXGI_FORMAT_UNKNOWN || args->SampleDesc.Count ||
        args->ViewInstancingDesc.ViewInstanceCount || args->NodeMask > 1 ||
        args->PrimitiveTopologyType != D3D12DDI_PRIMITIVE_TOPOLOGY_TYPE_POINT)
        return DXGI_ERROR_UNSUPPORTED;
    trace_ddi("PipelineMaintenance5", 0, venus->maintenance5);
    if (!venus->maintenance5)
        return DXGI_ERROR_UNSUPPORTED;

    const PFN_vkCreateRenderPass create_pass = (PFN_vkCreateRenderPass)
        venus->proc_addr(venus->handle, "vkCreateRenderPass");
    const PFN_vkDestroyRenderPass destroy_pass = (PFN_vkDestroyRenderPass)
        venus->proc_addr(venus->handle, "vkDestroyRenderPass");
    const PFN_vkCreateGraphicsPipelines create_graphics = (PFN_vkCreateGraphicsPipelines)
        venus->proc_addr(venus->handle, "vkCreateGraphicsPipelines");
    const PFN_vkDestroyPipeline destroy_pipeline = (PFN_vkDestroyPipeline)
        venus->proc_addr(venus->handle, "vkDestroyPipeline");
    if (!create_pass || !destroy_pass || !create_graphics || !destroy_pipeline)
        return E_FAIL;

    const VkSubpassDescription subpass = { .pipelineBindPoint = VK_PIPELINE_BIND_POINT_GRAPHICS };
    const VkRenderPassCreateInfo pass_info = {
        .sType = VK_STRUCTURE_TYPE_RENDER_PASS_CREATE_INFO,
        .subpassCount = 1,
        .pSubpasses = &subpass,
    };
    VkRenderPass render_pass;
    VkResult result = create_pass(venus->handle, &pass_info, NULL, &render_pass);
    if (result != VK_SUCCESS)
        return venus_hresult(result);

    const VkPipelineShaderStageCreateInfo stage = {
        .sType = VK_STRUCTURE_TYPE_PIPELINE_SHADER_STAGE_CREATE_INFO,
        .stage = VK_SHADER_STAGE_VERTEX_BIT,
        .module = vertex->module,
        .pName = "main",
    };
    const VkPipelineVertexInputStateCreateInfo vertex_input = {
        .sType = VK_STRUCTURE_TYPE_PIPELINE_VERTEX_INPUT_STATE_CREATE_INFO,
    };
    const VkPipelineInputAssemblyStateCreateInfo assembly = {
        .sType = VK_STRUCTURE_TYPE_PIPELINE_INPUT_ASSEMBLY_STATE_CREATE_INFO,
        .topology = VK_PRIMITIVE_TOPOLOGY_POINT_LIST,
    };
    const VkPipelineRasterizationStateCreateInfo rasterization = {
        .sType = VK_STRUCTURE_TYPE_PIPELINE_RASTERIZATION_STATE_CREATE_INFO,
        .rasterizerDiscardEnable = VK_TRUE,
        .polygonMode = VK_POLYGON_MODE_FILL,
        .lineWidth = 1.0f,
    };
    const VkGraphicsPipelineCreateInfo info = {
        .sType = VK_STRUCTURE_TYPE_GRAPHICS_PIPELINE_CREATE_INFO,
        .stageCount = 1,
        .pStages = &stage,
        .pVertexInputState = &vertex_input,
        .pInputAssemblyState = &assembly,
        .pRasterizationState = &rasterization,
        .layout = root->empty_layout,
        .renderPass = render_pass,
        .basePipelineIndex = -1,
    };
    VkPipeline pipeline = VK_NULL_HANDLE;
    result = create_graphics(venus->handle, VK_NULL_HANDLE, 1, &info, NULL, &pipeline);
    if (result == VK_SUCCESS) {
        state->device = venus;
        state->pipeline = pipeline;
        state->render_pass = render_pass;
    } else {
        if (pipeline)
            destroy_pipeline(venus->handle, pipeline, NULL);
        destroy_pass(venus->handle, render_pass, NULL);
    }
    const HRESULT status = venus_hresult(result);
    trace_ddi("CreatePipelineState", (UINT)status, state->pipeline != VK_NULL_HANDLE);
    return status;
}

static HRESULT APIENTRY make_pipeline_resident(D3D12DDI_HDEVICE device, D3D12DDIARG_MAKERESIDENT_0001 *args)
{
    if (!device.pDrvPrivate || !args || (args->NumObjects && !args->pObjects))
        return E_INVALIDARG;
    if (args->NumAdapters > 1)
        return DXGI_ERROR_UNSUPPORTED;
    for (UINT i = 0; i < args->NumObjects; ++i) {
        const D3D12DDI_HANDLE_AND_TYPE *object = &args->pObjects[i];
        if (object->Type != D3D12DDI_HT_PIPELINE_STATE)
            return DXGI_ERROR_UNSUPPORTED;
        const DdiPipeline *state = object->Handle;
        if (!state || !state->pipeline || state->device != venus_device(device))
            return E_INVALIDARG;
    }

    /* These Vulkan pipelines were allocated at creation and have no WDDM paging fence. */
    args->WaitMask = 0;
    if (args->NumAdapters && args->pPagingFenceValue)
        args->pPagingFenceValue[0] = 0;
    trace_ddi("MakeResident", args->NumObjects, args->NumAdapters);
    return S_OK;
}

static void APIENTRY destroy_pipeline(D3D12DDI_HDEVICE device, D3D12DDI_HPIPELINESTATE handle)
{
    DdiPipeline *state = handle.pDrvPrivate;
    if (!state || !state->pipeline)
        return;
    const VenusDevice *venus = venus_device(device);
    const PFN_vkDestroyPipeline destroy = (PFN_vkDestroyPipeline)
        venus->proc_addr(venus->handle, "vkDestroyPipeline");
    const PFN_vkDestroyRenderPass destroy_pass = (PFN_vkDestroyRenderPass)
        venus->proc_addr(venus->handle, "vkDestroyRenderPass");
    destroy(venus->handle, state->pipeline, NULL);
    if (state->render_pass)
        destroy_pass(venus->handle, state->render_pass, NULL);
    *state = (DdiPipeline){0};
}
#endif
