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
    UINT pixel_bindings;
    BOOL blends;
    UINT minimum_vertex_stride;
    VkVertexInputRate vertex_rate;
    DdiComputeLayout compute;
    VkDescriptorSetLayout compute_set;
} DdiPipeline;

static SIZE_T APIENTRY private_pipeline_size(D3D12DDI_HDEVICE device,
                                              const D3D12DDIARG_CREATE_PIPELINE_STATE_0033 *args)
{
    return device.pDrvPrivate && args ? sizeof(DdiPipeline) : 0;
}

static void destroy_compute_layout(const VenusDevice *venus, DdiPipeline *state)
{
    const PFN_vkDestroyPipelineLayout destroy_layout = (PFN_vkDestroyPipelineLayout)
        venus->proc_addr(venus->handle, "vkDestroyPipelineLayout");
    const PFN_vkDestroyDescriptorSetLayout destroy_set = (PFN_vkDestroyDescriptorSetLayout)
        venus->proc_addr(venus->handle, "vkDestroyDescriptorSetLayout");
    if (state->layout)
        destroy_layout(venus->handle, state->layout, NULL);
    if (state->compute_set)
        destroy_set(venus->handle, state->compute_set, NULL);
}

static HRESULT create_compute_layout(const VenusDevice *venus, DdiPipeline *state)
{
    const PFN_vkCreatePipelineLayout create_layout = (PFN_vkCreatePipelineLayout)
        venus->proc_addr(venus->handle, "vkCreatePipelineLayout");
    const PFN_vkCreateDescriptorSetLayout create_set = (PFN_vkCreateDescriptorSetLayout)
        venus->proc_addr(venus->handle, "vkCreateDescriptorSetLayout");
    if (!create_layout || !create_set || !venus->proc_addr(venus->handle, "vkDestroyPipelineLayout") ||
        !venus->proc_addr(venus->handle, "vkDestroyDescriptorSetLayout"))
        return E_FAIL;

    const UINT bindings = state->compute.buffer_count;
    if (bindings) {
        VkDescriptorSetLayoutBinding buffers[DDI_COMPUTE_BUFFER_BINDINGS];
        for (UINT i = 0; i < bindings; ++i) {
            buffers[i] = (VkDescriptorSetLayoutBinding){ .binding = i, .descriptorType = VK_DESCRIPTOR_TYPE_STORAGE_BUFFER,
                .descriptorCount = 1, .stageFlags = VK_SHADER_STAGE_COMPUTE_BIT };
        }
        const VkDescriptorSetLayoutCreateInfo info = {
            .sType = VK_STRUCTURE_TYPE_DESCRIPTOR_SET_LAYOUT_CREATE_INFO, .bindingCount = bindings, .pBindings = buffers,
        };
        const HRESULT status = venus_hresult(create_set(venus->handle, &info, NULL, &state->compute_set));
        if (FAILED(status))
            return status;
    }
    const VkPushConstantRange constants = {
        .stageFlags = VK_SHADER_STAGE_COMPUTE_BIT, .size = state->compute.constant_words * sizeof(UINT32),
    };
    const VkPipelineLayoutCreateInfo info = {
        .sType = VK_STRUCTURE_TYPE_PIPELINE_LAYOUT_CREATE_INFO,
        .setLayoutCount = bindings ? 1 : 0,
        .pSetLayouts = bindings ? &state->compute_set : NULL,
        .pushConstantRangeCount = constants.size ? 1 : 0,
        .pPushConstantRanges = constants.size ? &constants : NULL,
    };
    return venus_hresult(create_layout(venus->handle, &info, NULL, &state->layout));
}

static HRESULT create_compute_pipeline(const VenusDevice *venus, const D3D12DDIARG_CREATE_PIPELINE_STATE_0033 *args,
                                        DdiPipeline *state)
{
    const DdiShader *shader = args->hComputeShader.pDrvPrivate;
    const DdiRootSignature *root = args->hRootSignature.pDrvPrivate;
    if (!shader || !shader->compute_code || shader->stage != VK_SHADER_STAGE_COMPUTE_BIT ||
        !root || root->desc.NumStaticSamplers || args->NodeMask > 1)
        return DXGI_ERROR_UNSUPPORTED;

    DdiPipeline pipeline = { .device = venus, .bind_point = VK_PIPELINE_BIND_POINT_COMPUTE };
    pipeline.compute.constant_words = compute_constant_words(root);
    if (pipeline.compute.constant_words > DDI_COMPUTE_CONSTANT_WORDS)
        return DXGI_ERROR_UNSUPPORTED;

    const PFN_vkCreateComputePipelines create = (PFN_vkCreateComputePipelines)
        venus->proc_addr(venus->handle, "vkCreateComputePipelines");
    const PFN_vkDestroyPipeline destroy = (PFN_vkDestroyPipeline)
        venus->proc_addr(venus->handle, "vkDestroyPipeline");
    if (!create || !destroy)
        return E_FAIL;

    DdiShader compiled = {0};
    DdiComputeRemap remap = { .root = root, .layout = &pipeline.compute };
    HRESULT status = compile_shader_module(venus, shader->compute_code, &remap, &compiled);
    if (SUCCEEDED(status) && pipeline.compute.buffer_count &&
        (!venus->robust_buffer_access || pipeline.compute.buffer_count > venus->limits.maxPerStageDescriptorStorageBuffers))
        status = DXGI_ERROR_UNSUPPORTED;
    if (SUCCEEDED(status))
        status = create_compute_layout(venus, &pipeline);
    const VkComputePipelineCreateInfo info = {
        .sType = VK_STRUCTURE_TYPE_COMPUTE_PIPELINE_CREATE_INFO,
        .stage = {
            .sType = VK_STRUCTURE_TYPE_PIPELINE_SHADER_STAGE_CREATE_INFO,
            .stage = VK_SHADER_STAGE_COMPUTE_BIT,
            .module = compiled.module,
            .pName = "main",
        },
        .layout = pipeline.layout,
        .basePipelineIndex = -1,
    };
    if (SUCCEEDED(status))
        status = venus_hresult(create(venus->handle, VK_NULL_HANDLE, 1, &info, NULL, &pipeline.pipeline));
    if (compiled.module)
        destroy_shader_module(venus, compiled.module);
    if (SUCCEEDED(status))
        *state = pipeline;
    else {
        if (pipeline.pipeline)
            destroy(venus->handle, pipeline.pipeline, NULL);
        destroy_compute_layout(venus, &pipeline);
    }
    trace_ddi("CreateComputePipelineState", (UINT)status, pipeline.compute.buffer_count);
    return status;
}

static HRESULT create_color_pipeline(const VenusDevice *venus, const D3D12DDIARG_CREATE_PIPELINE_STATE_0033 *args,
                                     const DdiShader *vertex, const DdiRootSignature *root, DdiPipeline *state)
{
    const DdiShader *pixel = args->hPixelShader.pDrvPrivate;
    const D3D12DDI_BLEND_DESC_0010 *blend = args->hBlendState.pDrvPrivate;
    const D3D12DDI_RASTERIZER_DESC_0010 *raster = args->hRasterizerState.pDrvPrivate;
    const D3D12DDI_DEPTH_STENCIL_DESC_0025 *depth = args->hDepthStencilState.pDrvPrivate;
    const DdiElementLayout *elements = args->hElementLayout.pDrvPrivate;
    const VkFormat format = color_image_format(args->RTVFormats[0]);
    const VkPipelineLayout layout = !root || !pixel || pixel->pixel_bindings != root->pixel_bindings ? VK_NULL_HANDLE
        : pixel->pixel_bindings ? root->pixel_pipeline_layout : root->empty_layout;
    if (!venus->dynamic_rendering || !vertex || !vertex->module || vertex->stage != VK_SHADER_STAGE_VERTEX_BIT ||
        !pixel || !pixel->module || pixel->stage != VK_SHADER_STAGE_FRAGMENT_BIT || !layout ||
        args->hDomainShader.pDrvPrivate || args->hHullShader.pDrvPrivate || args->hGeometryShader.pDrvPrivate ||
        (elements && (!elements->count || !venus->proc_addr(venus->handle, "vkCmdBindVertexBuffers2"))) ||
        args->NumRenderTargets != 1 || format == VK_FORMAT_UNDEFINED ||
        args->DSVFormat != DXGI_FORMAT_UNKNOWN || args->SampleDesc.Count != 1 || args->SampleDesc.Quality ||
        args->ViewInstancingDesc.ViewInstanceCount || args->NodeMask > 1 ||
        args->PrimitiveTopologyType != D3D12DDI_PRIMITIVE_TOPOLOGY_TYPE_TRIANGLE)
        return DXGI_ERROR_UNSUPPORTED;
    if (!blend || blend->AlphaToCoverageEnable ||
        blend->RenderTarget[0].LogicOpEnable || !raster || raster->FillMode != D3D12DDI_FILL_MODE_SOLID ||
        raster->CullMode != D3D12DDI_CULL_MODE_NONE || !raster->DepthClipEnable || raster->DepthBias ||
        raster->SlopeScaledDepthBias || raster->ForcedSampleCount || (depth && (depth->DepthEnable || depth->StencilEnable)))
        return DXGI_ERROR_UNSUPPORTED;
    const D3D12DDI_RENDER_TARGET_BLEND_DESC *target = &blend->RenderTarget[0];
    if (target->BlendEnable &&
        (target->SrcBlend != D3D12DDI_BLEND_ONE || target->DestBlend != D3D12DDI_BLEND_INV_SRC_ALPHA ||
         target->BlendOp != D3D12DDI_BLEND_OP_ADD || target->SrcBlendAlpha != D3D12DDI_BLEND_ONE ||
         target->DestBlendAlpha != D3D12DDI_BLEND_INV_SRC_ALPHA || target->BlendOpAlpha != D3D12DDI_BLEND_OP_ADD))
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
    VkVertexInputBindingDescription vertex_binding = {0};
    VkPipelineVertexInputStateCreateInfo vertices = { .sType = VK_STRUCTURE_TYPE_PIPELINE_VERTEX_INPUT_STATE_CREATE_INFO };
    if (elements) {
        vertex_binding = (VkVertexInputBindingDescription){
            .binding = 0, .stride = elements->minimum_stride, .inputRate = elements->rate,
        };
        vertices.vertexBindingDescriptionCount = 1;
        vertices.pVertexBindingDescriptions = &vertex_binding;
        vertices.vertexAttributeDescriptionCount = elements->count;
        vertices.pVertexAttributeDescriptions = elements->attributes;
    }
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
    const VkPipelineColorBlendAttachmentState attachment = {
        .blendEnable = target->BlendEnable, .colorWriteMask = target->RenderTargetWriteMask,
        .srcColorBlendFactor = VK_BLEND_FACTOR_ONE, .dstColorBlendFactor = VK_BLEND_FACTOR_ONE_MINUS_SRC_ALPHA,
        .colorBlendOp = VK_BLEND_OP_ADD,
        .srcAlphaBlendFactor = VK_BLEND_FACTOR_ONE, .dstAlphaBlendFactor = VK_BLEND_FACTOR_ONE_MINUS_SRC_ALPHA,
        .alphaBlendOp = VK_BLEND_OP_ADD,
    };
    const VkPipelineColorBlendStateCreateInfo blending = {
        .sType = VK_STRUCTURE_TYPE_PIPELINE_COLOR_BLEND_STATE_CREATE_INFO, .attachmentCount = 1, .pAttachments = &attachment,
    };
    const VkDynamicState dynamic_states[] = {
        VK_DYNAMIC_STATE_VIEWPORT, VK_DYNAMIC_STATE_SCISSOR, VK_DYNAMIC_STATE_VERTEX_INPUT_BINDING_STRIDE,
    };
    const VkPipelineDynamicStateCreateInfo dynamic = {
        .sType = VK_STRUCTURE_TYPE_PIPELINE_DYNAMIC_STATE_CREATE_INFO, .dynamicStateCount = elements ? 3 : 2,
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
        .layout = layout, .basePipelineIndex = -1,
    };
    VkPipeline pipeline = VK_NULL_HANDLE;
    const VkResult result = create(venus->handle, VK_NULL_HANDLE, 1, &info, NULL, &pipeline);
    if (result == VK_SUCCESS) {
        *state = (DdiPipeline){ .device = venus, .pipeline = pipeline, .bind_point = VK_PIPELINE_BIND_POINT_GRAPHICS,
            .color_format = format, .layout = layout, .pixel_bindings = pixel->pixel_bindings,
            .blends = target->BlendEnable,
            .minimum_vertex_stride = elements ? elements->minimum_stride : 0,
            .vertex_rate = elements ? elements->rate : VK_VERTEX_INPUT_RATE_VERTEX };
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
    if (state->bind_point == VK_PIPELINE_BIND_POINT_COMPUTE)
        destroy_compute_layout(venus, state);
    if (state->render_pass)
        destroy_pass(venus->handle, state->render_pass, NULL);
    *state = (DdiPipeline){0};
}
#endif
