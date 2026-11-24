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
    VkPipelineBindPoint bind_point;
    VkRenderPass render_pass;
    VkFormat color_format;
    VkPipelineLayout layout;
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
        state->bind_point = VK_PIPELINE_BIND_POINT_COMPUTE;
    } else if (pipeline) {
        destroy(venus->handle, pipeline, NULL);
    }
    const HRESULT status = venus_hresult(result);
    trace_ddi("CreateComputePipelineState", (UINT)status, state->pipeline != VK_NULL_HANDLE);
    return status;
}

static HRESULT create_color_pipeline(const VenusDevice *venus, const D3D12DDIARG_CREATE_PIPELINE_STATE_0033 *args,
                                     const DdiShader *vertex, const DdiRootSignature *root, DdiPipeline *state)
{
    const DdiShader *pixel = args->hPixelShader.pDrvPrivate;
    const D3D12DDI_BLEND_DESC_0010 *blend = args->hBlendState.pDrvPrivate;
    const D3D12DDI_RASTERIZER_DESC_0010 *raster = args->hRasterizerState.pDrvPrivate;
    const D3D12DDI_DEPTH_STENCIL_DESC_0025 *depth = args->hDepthStencilState.pDrvPrivate;
    const VkFormat format = color_image_format(args->RTVFormats[0]);
    if (!venus->dynamic_rendering || !vertex || !vertex->module || vertex->stage != VK_SHADER_STAGE_VERTEX_BIT ||
        !pixel || !pixel->module || pixel->stage != VK_SHADER_STAGE_FRAGMENT_BIT || !root || !root->empty_layout ||
        args->hDomainShader.pDrvPrivate || args->hHullShader.pDrvPrivate || args->hGeometryShader.pDrvPrivate ||
        args->hElementLayout.pDrvPrivate || args->NumRenderTargets != 1 || format == VK_FORMAT_UNDEFINED ||
        args->DSVFormat != DXGI_FORMAT_UNKNOWN || args->SampleDesc.Count != 1 || args->SampleDesc.Quality ||
        args->ViewInstancingDesc.ViewInstanceCount || args->NodeMask > 1 ||
        args->PrimitiveTopologyType != D3D12DDI_PRIMITIVE_TOPOLOGY_TYPE_TRIANGLE)
        return DXGI_ERROR_UNSUPPORTED;
    if (!blend || blend->AlphaToCoverageEnable || blend->RenderTarget[0].BlendEnable ||
        blend->RenderTarget[0].LogicOpEnable || !raster || raster->FillMode != D3D12DDI_FILL_MODE_SOLID ||
        raster->CullMode != D3D12DDI_CULL_MODE_NONE || !raster->DepthClipEnable || raster->DepthBias ||
        raster->SlopeScaledDepthBias || raster->ForcedSampleCount || (depth && (depth->DepthEnable || depth->StencilEnable)))
        return DXGI_ERROR_UNSUPPORTED;

    const PFN_vkCreateGraphicsPipelines create = (PFN_vkCreateGraphicsPipelines)
        venus->proc_addr(venus->handle, "vkCreateGraphicsPipelines");
    const PFN_vkDestroyPipeline destroy = (PFN_vkDestroyPipeline)venus->proc_addr(venus->handle, "vkDestroyPipeline");
    if (!create || !destroy)
        return E_FAIL;

    const VkPipelineShaderStageCreateInfo shaders[] = {
        { .sType = VK_STRUCTURE_TYPE_PIPELINE_SHADER_STAGE_CREATE_INFO, .stage = VK_SHADER_STAGE_VERTEX_BIT,
          .module = vertex->module, .pName = "main" },
        { .sType = VK_STRUCTURE_TYPE_PIPELINE_SHADER_STAGE_CREATE_INFO, .stage = VK_SHADER_STAGE_FRAGMENT_BIT,
          .module = pixel->module, .pName = "main" },
    };
    const VkPipelineVertexInputStateCreateInfo vertices = { .sType = VK_STRUCTURE_TYPE_PIPELINE_VERTEX_INPUT_STATE_CREATE_INFO };
    const VkPipelineInputAssemblyStateCreateInfo assembly = {
        .sType = VK_STRUCTURE_TYPE_PIPELINE_INPUT_ASSEMBLY_STATE_CREATE_INFO, .topology = VK_PRIMITIVE_TOPOLOGY_TRIANGLE_LIST,
    };
    const VkPipelineViewportStateCreateInfo viewport = {
        .sType = VK_STRUCTURE_TYPE_PIPELINE_VIEWPORT_STATE_CREATE_INFO, .viewportCount = 1, .scissorCount = 1,
    };
    const VkPipelineRasterizationStateCreateInfo rasterization = {
        .sType = VK_STRUCTURE_TYPE_PIPELINE_RASTERIZATION_STATE_CREATE_INFO,
        .polygonMode = VK_POLYGON_MODE_FILL, .cullMode = VK_CULL_MODE_NONE, .lineWidth = 1.0f,
        .frontFace = raster->FrontCounterClockwise ? VK_FRONT_FACE_COUNTER_CLOCKWISE : VK_FRONT_FACE_CLOCKWISE,
    };
    const VkSampleMask sample_mask = args->SampleMask;
    const VkPipelineMultisampleStateCreateInfo samples = {
        .sType = VK_STRUCTURE_TYPE_PIPELINE_MULTISAMPLE_STATE_CREATE_INFO, .rasterizationSamples = VK_SAMPLE_COUNT_1_BIT,
        .pSampleMask = &sample_mask,
    };
    const VkPipelineColorBlendAttachmentState attachment = { .colorWriteMask = blend->RenderTarget[0].RenderTargetWriteMask };
    const VkPipelineColorBlendStateCreateInfo blending = {
        .sType = VK_STRUCTURE_TYPE_PIPELINE_COLOR_BLEND_STATE_CREATE_INFO, .attachmentCount = 1, .pAttachments = &attachment,
    };
    const VkDynamicState dynamic_states[] = {VK_DYNAMIC_STATE_VIEWPORT, VK_DYNAMIC_STATE_SCISSOR};
    const VkPipelineDynamicStateCreateInfo dynamic = {
        .sType = VK_STRUCTURE_TYPE_PIPELINE_DYNAMIC_STATE_CREATE_INFO, .dynamicStateCount = _countof(dynamic_states),
        .pDynamicStates = dynamic_states,
    };
    const VkPipelineRenderingCreateInfo rendering = {
        .sType = VK_STRUCTURE_TYPE_PIPELINE_RENDERING_CREATE_INFO, .colorAttachmentCount = 1, .pColorAttachmentFormats = &format,
    };
    const VkGraphicsPipelineCreateInfo info = {
        .sType = VK_STRUCTURE_TYPE_GRAPHICS_PIPELINE_CREATE_INFO, .pNext = &rendering,
        .stageCount = _countof(shaders), .pStages = shaders, .pVertexInputState = &vertices,
        .pInputAssemblyState = &assembly, .pViewportState = &viewport, .pRasterizationState = &rasterization,
        .pMultisampleState = &samples, .pColorBlendState = &blending, .pDynamicState = &dynamic,
        .layout = root->empty_layout, .basePipelineIndex = -1,
    };
    VkPipeline pipeline = VK_NULL_HANDLE;
    const VkResult result = create(venus->handle, VK_NULL_HANDLE, 1, &info, NULL, &pipeline);
    if (result == VK_SUCCESS) {
        *state = (DdiPipeline){ .device = venus, .pipeline = pipeline, .bind_point = VK_PIPELINE_BIND_POINT_GRAPHICS,
            .color_format = format, .layout = root->empty_layout };
    } else if (pipeline) {
        destroy(venus->handle, pipeline, NULL);
    }
    const HRESULT status = venus_hresult(result);
    trace_ddi("CreateColorPipeline", (UINT)status, format);
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
    if (args->NumRenderTargets)
        return create_color_pipeline(venus, args, vertex, root, state);
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
        state->bind_point = VK_PIPELINE_BIND_POINT_GRAPHICS;
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
