/* SPDX-License-Identifier: Apache-2.0 */
typedef struct DdiCommandPool {
    DdiDevice *device;
    VkCommandPool pool;
    SRWLOCK lock;
    VkCommandBuffer *commands;
    SIZE_T capacity;
    SIZE_T used;
    UINT64 generation;
#ifdef ENTANGLED_D3D12_PROBE_SHADER
    VkDescriptorPool descriptor_pool;
    UINT sampler_count;
    VkSampler samplers[64];
#endif
} DdiCommandPool;

typedef struct DdiCommandRecorder {
    DdiDevice *device;
    DdiCommandPool *target;
} DdiCommandRecorder;

typedef struct DdiCommandQueue {
    DdiDevice *device;
    D3D12DDI_HRTCOMMANDQUEUE runtime;
    HANDLE context;
} DdiCommandQueue;

typedef struct DdiCommandSignature {
    DdiDevice *device;
    D3D12DDI_INDIRECT_ARGUMENT_TYPE type;
    UINT stride;
} DdiCommandSignature;

static SIZE_T APIENTRY private_command_signature_size(D3D12DDI_HDEVICE device,
                                                       const D3D12DDIARG_CREATE_COMMAND_SIGNATURE_0001 *args)
{
    return device.pDrvPrivate && args ? sizeof(DdiCommandSignature) : 0;
}

static UINT indirect_argument_size(D3D12DDI_INDIRECT_ARGUMENT_TYPE type)
{
    switch (type) {
    case D3D12DDI_INDIRECT_ARGUMENT_TYPE_DRAW: return sizeof(D3D12DDI_DRAW_ARGUMENTS);
    case D3D12DDI_INDIRECT_ARGUMENT_TYPE_DRAW_INDEXED: return sizeof(D3D12DDI_DRAW_INDEXED_ARGUMENTS);
    case D3D12DDI_INDIRECT_ARGUMENT_TYPE_DISPATCH: return sizeof(D3D12DDI_DISPATCH_ARGUMENTS);
    default: return 0;
    }
}

static HRESULT APIENTRY create_command_signature(D3D12DDI_HDEVICE device,
                                                 const D3D12DDIARG_CREATE_COMMAND_SIGNATURE_0001 *args,
                                                 D3D12DDI_HCOMMANDSIGNATURE handle)
{
    DdiCommandSignature *signature = handle.pDrvPrivate;
    if (!device.pDrvPrivate || !signature || !args || !args->pArgumentDescs || !args->NumArgumentDescs)
        return E_INVALIDARG;
    if (args->NodeMask > 1 || args->NumArgumentDescs != 1 || args->hRootSignature.pDrvPrivate)
        return DXGI_ERROR_UNSUPPORTED;
    const D3D12DDI_INDIRECT_ARGUMENT_TYPE type = args->pArgumentDescs[0].Type;
    const UINT size = indirect_argument_size(type);
    if (!size)
        return DXGI_ERROR_UNSUPPORTED;
    if (args->ByteStride < size || args->ByteStride % sizeof(UINT32))
        return E_INVALIDARG;
    *signature = (DdiCommandSignature){ .device = device.pDrvPrivate, .type = type, .stride = args->ByteStride };
    trace_ddi("CreateCommandSignature", type, signature->stride);
    return S_OK;
}

static void APIENTRY destroy_command_signature(D3D12DDI_HDEVICE device, D3D12DDI_HCOMMANDSIGNATURE handle)
{
    DdiCommandSignature *signature = handle.pDrvPrivate;
    if (signature && signature->device == device.pDrvPrivate)
        *signature = (DdiCommandSignature){0};
}

typedef struct DdiDrawState {
    D3D12DDI_PRIMITIVE_TOPOLOGY topology;
    BOOL has_viewport;
    BOOL has_scissor;
    VkViewport viewport;
    VkRect2D scissor;
    VenusColorImage *target;
    VkPipelineLayout root_layout;
#ifdef ENTANGLED_D3D12_PROBE_SHADER
    const DdiPipeline *pipeline;
    D3D12DDI_VERTEX_BUFFER_VIEW vertex_buffer;
    VkDescriptorSetLayout pixel_set_layout;
    UINT pixel_bindings;
    const DdiDescriptorHeap *resource_heap;
    const DdiDescriptorHeap *sampler_heap;
    const DdiDescriptor *uniform;
    const DdiDescriptor *texture;
    const DdiDescriptor *sampler;
#endif
} DdiDrawState;

#ifdef ENTANGLED_D3D12_PROBE_SHADER
enum { DDI_ROOT_PARAMETERS = 64 };

typedef struct DdiComputeState {
    const DdiPipeline *pipeline;
    const DdiRootSignature *root;
    D3D12DDI_GPU_DESCRIPTOR_HANDLE tables[DDI_ROOT_PARAMETERS];
    UINT32 constants[DDI_COMPUTE_CONSTANT_WORDS];
} DdiComputeState;
#endif

typedef struct DdiTransferList {
    VenusTransferList venus;
    HRESULT status;
    DdiDevice *device;
    D3D12DDI_HRTCOMMANDLIST runtime;
    DdiCommandPool *pool;
    UINT64 generation;
    BOOL closed;
    const VenusColorImage *written_color;
    const VenusColorImage *uploaded_color;
    const VenusColorImage *sampled_color;
    const VenusColorImage *color_copy_source;
    BOOL color_rendered;
#ifdef ENTANGLED_D3D12_PROBE_PRESENT
    DdiPresentSurface *present;
#endif
    DdiDrawState draw;
#ifdef ENTANGLED_D3D12_PROBE_SHADER
    DdiComputeState compute;
#endif
} DdiTransferList;

static void set_command_list_status(DdiTransferList *list, HRESULT status)
{
    list->status = status;
    if (FAILED(status) && list->device && list->device->callbacks->pfnSetCommandListErrorCb)
        list->device->callbacks->pfnSetCommandListErrorCb(list->runtime, status);
}

static DdiTransferList *recording_command_list(D3D12DDI_HCOMMANDLIST handle)
{
    DdiTransferList *list = handle.pDrvPrivate;
    if (!list || FAILED(list->status))
        return NULL;
    if (!list->pool || list->closed) {
        set_command_list_status(list, E_INVALIDARG);
        return NULL;
    }
    return list;
}

static void require_transfer_state(D3D12DDI_HCOMMANDLIST handle, BOOL supported)
{
    DdiTransferList *list = recording_command_list(handle);
    if (list && !supported)
        set_command_list_status(list, DXGI_ERROR_UNSUPPORTED);
}

static void APIENTRY execute_unsupported_indirect(D3D12DDI_HCOMMANDLIST list, D3D12DDI_HCOMMANDSIGNATURE signature,
                                                  UINT count, D3D12DDIARG_BUFFER_PLACEMENT args,
                                                  D3D12DDIARG_BUFFER_PLACEMENT count_buffer)
{
    (void)signature;
    (void)args;
    (void)count_buffer;
    require_transfer_state(list, count == 0);
}

static void APIENTRY set_transfer_topology(D3D12DDI_HCOMMANDLIST list, D3D12DDI_PRIMITIVE_TOPOLOGY topology)
{
    DdiTransferList *commands = recording_command_list(list);
    if (!commands)
        return;
    if (topology != D3D12DDI_PRIMITIVE_TOPOLOGY_UNDEFINED && topology != D3D12DDI_PRIMITIVE_TOPOLOGY_TRIANGLELIST) {
        set_command_list_status(commands, DXGI_ERROR_UNSUPPORTED);
        return;
    }
    commands->draw.topology = topology;
}

static void APIENTRY set_transfer_descriptor_heaps(D3D12DDI_HCOMMANDLIST list, UINT count,
                                                  D3D12DDI_HDESCRIPTORHEAP *heaps)
{
#ifdef ENTANGLED_D3D12_PROBE_SHADER
    DdiTransferList *commands = recording_command_list(list);
    if (!commands)
        return;
    const DdiDescriptorHeap *resource_heap = NULL;
    const DdiDescriptorHeap *sampler_heap = NULL;
    if (count > 2 || (count && !heaps)) {
        set_command_list_status(commands, E_INVALIDARG);
        return;
    }
    for (UINT i = 0; i < count; ++i) {
        const DdiDescriptorHeap *heap = heaps[i].pDrvPrivate;
        if (!heap)
            continue;
        if (heap->device != commands->device ||
            !(heap->args.Flags & D3D12DDI_DESCRIPTOR_HEAP_FLAG_SHADER_VISIBLE)) {
            set_command_list_status(commands, DXGI_ERROR_UNSUPPORTED);
            return;
        }
        if (heap->args.Type == D3D12DDI_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV && !resource_heap)
            resource_heap = heap;
        else if (heap->args.Type == D3D12DDI_DESCRIPTOR_HEAP_TYPE_SAMPLER && !sampler_heap)
            sampler_heap = heap;
        else {
            set_command_list_status(commands, DXGI_ERROR_UNSUPPORTED);
            return;
        }
    }
    if (commands->draw.resource_heap != resource_heap) {
        commands->draw.uniform = NULL;
        commands->draw.texture = NULL;
        memset(commands->compute.tables, 0, sizeof(commands->compute.tables));
    }
    if (commands->draw.sampler_heap != sampler_heap)
        commands->draw.sampler = NULL;
    commands->draw.resource_heap = resource_heap;
    commands->draw.sampler_heap = sampler_heap;
#else
    (void)heaps;
    require_transfer_state(list, count == 0);
#endif
}

static void APIENTRY set_transfer_vertex_buffers(D3D12DDI_HCOMMANDLIST list, UINT first, UINT count,
                                                 const D3D12DDI_VERTEX_BUFFER_VIEW *views)
{
    const UINT max_slots = D3D12_IA_VERTEX_INPUT_RESOURCE_SLOT_COUNT;
    if (first > max_slots || count > max_slots - first) {
        require_transfer_state(list, FALSE);
        return;
    }
    for (UINT i = 0; views && i < count; ++i) {
        if (views[i].BufferLocation || views[i].SizeInBytes) {
#ifdef ENTANGLED_D3D12_PROBE_SHADER
            if (!first && !i && views[i].BufferLocation && views[i].SizeInBytes)
                continue;
#endif
            require_transfer_state(list, FALSE);
            return;
        }
    }
#ifdef ENTANGLED_D3D12_PROBE_SHADER
    DdiTransferList *commands = recording_command_list(list);
    if (commands && !first && count)
        commands->draw.vertex_buffer = views ? views[0] : (D3D12DDI_VERTEX_BUFFER_VIEW){0};
#else
    require_transfer_state(list, TRUE);
#endif
}

static void APIENTRY set_transfer_index_buffer(D3D12DDI_HCOMMANDLIST list, const D3D12DDI_INDEX_BUFFER_VIEW *view)
{
    require_transfer_state(list, !view || (!view->BufferLocation && !view->SizeInBytes));
}

static void APIENTRY set_transfer_stream_output(D3D12DDI_HCOMMANDLIST list, UINT first, UINT count,
                                                const D3D12DDI_STREAM_OUTPUT_BUFFER_VIEW *views)
{
    const UINT max_slots = D3D12_SO_BUFFER_SLOT_COUNT;
    if (first > max_slots || count > max_slots - first) {
        require_transfer_state(list, FALSE);
        return;
    }
    for (UINT i = 0; views && i < count; ++i) {
        if (views[i].BufferLocation || views[i].BufferFilledSizeLocation || views[i].SizeInBytes) {
            require_transfer_state(list, FALSE);
            return;
        }
    }
    require_transfer_state(list, TRUE);
}

static void APIENTRY set_transfer_viewports(D3D12DDI_HCOMMANDLIST list, UINT count, const D3D12DDI_VIEWPORT *viewports)
{
    DdiTransferList *commands = recording_command_list(list);
    if (!commands)
        return;
    commands->draw.has_viewport = FALSE;
    if (!count)
        return;
    if (count != 1 || !viewports || !(viewports->Width > 0) || !(viewports->Height > 0) ||
        !(viewports->MinDepth >= 0 && viewports->MinDepth <= viewports->MaxDepth && viewports->MaxDepth <= 1)) {
        set_command_list_status(commands, DXGI_ERROR_UNSUPPORTED);
        return;
    }
    commands->draw.viewport = (VkViewport){ .x = viewports->TopLeftX, .y = viewports->TopLeftY + viewports->Height,
        .width = viewports->Width, .height = -viewports->Height, .minDepth = viewports->MinDepth, .maxDepth = viewports->MaxDepth };
    commands->draw.has_viewport = TRUE;
}

static void APIENTRY set_transfer_scissors(D3D12DDI_HCOMMANDLIST list, UINT count, const D3D12DDI_RECT *scissors)
{
    DdiTransferList *commands = recording_command_list(list);
    if (!commands)
        return;
    commands->draw.has_scissor = FALSE;
    if (!count)
        return;
    if (count != 1 || !scissors || scissors->left < 0 || scissors->top < 0 ||
        scissors->right < scissors->left || scissors->bottom < scissors->top) {
        set_command_list_status(commands, DXGI_ERROR_UNSUPPORTED);
        return;
    }
    commands->draw.scissor = (VkRect2D){ .offset = {scissors->left, scissors->top},
        .extent = {(UINT32)(scissors->right - scissors->left), (UINT32)(scissors->bottom - scissors->top)} };
    commands->draw.has_scissor = TRUE;
}

static void APIENTRY set_transfer_graphics_root(D3D12DDI_HCOMMANDLIST handle, D3D12DDI_HROOTSIGNATURE signature)
{
    DdiTransferList *commands = recording_command_list(handle);
    if (!commands)
        return;
    const DdiRootSignature *root = signature.pDrvPrivate;
#ifdef ENTANGLED_D3D12_PROBE_SHADER
    commands->draw.pixel_set_layout = root ? root->pixel_set_layout : VK_NULL_HANDLE;
    commands->draw.pixel_bindings = root ? root->pixel_bindings : 0;
    commands->draw.uniform = NULL;
    commands->draw.texture = NULL;
    commands->draw.sampler = NULL;
    if (commands->draw.pixel_set_layout) {
        commands->draw.root_layout = root->pixel_pipeline_layout;
        return;
    }
#endif
    if (root && !root->empty_layout) {
        set_command_list_status(commands, DXGI_ERROR_UNSUPPORTED);
        return;
    }
    commands->draw.root_layout = root ? root->empty_layout : VK_NULL_HANDLE;
}

#ifdef ENTANGLED_D3D12_PROBE_SHADER
static void APIENTRY set_graphics_pixel_table(D3D12DDI_HCOMMANDLIST list, UINT parameter,
                                               D3D12DDI_GPU_DESCRIPTOR_HANDLE base)
{
    DdiTransferList *commands = recording_command_list(list);
    if (!commands)
        return;
    DdiDrawState *draw = &commands->draw;
    const BOOL sampler = parameter == 1 && (draw->pixel_bindings & DDI_PIXEL_SAMPLER);
    const DdiDescriptorHeap *heap = sampler ? draw->sampler_heap : draw->resource_heap;
    const UINT64 start = heap ? (UINT64)(UINT_PTR)(heap + 1) : 0;
    const UINT64 offset = base.ptr - start;
    if ((parameter && !sampler) || !draw->pixel_set_layout || !heap || base.ptr < start ||
        offset % sizeof(DdiDescriptor) || offset / sizeof(DdiDescriptor) >= heap->args.NumDescriptors) {
        set_command_list_status(commands, E_INVALIDARG);
        return;
    }
    const DdiDescriptor *descriptor = (const DdiDescriptor *)(UINT_PTR)base.ptr;
    if (sampler)
        draw->sampler = descriptor;
    else if (draw->pixel_bindings == DDI_PIXEL_CBV)
        draw->uniform = descriptor;
    else
        draw->texture = descriptor;
}
#endif

static void APIENTRY set_transfer_blend_factor(D3D12DDI_HCOMMANDLIST list, const FLOAT values[4])
{
    require_transfer_state(list, values && values[0] == 1.0f && values[1] == 1.0f &&
                           values[2] == 1.0f && values[3] == 1.0f);
}

static void APIENTRY set_transfer_stencil_reference(D3D12DDI_HCOMMANDLIST list, UINT reference)
{
    require_transfer_state(list, reference == 0);
}

static void APIENTRY set_transfer_predication(D3D12DDI_HCOMMANDLIST list, D3D12DDI_HRESOURCE buffer, UINT64 offset,
                                             D3D12DDI_PREDICATION_OP op)
{
    (void)offset;
    (void)op;
    require_transfer_state(list, !buffer.pDrvPrivate);
}

static void APIENTRY clear_transfer_root_arguments(D3D12DDI_HCOMMANDLIST list)
{
#ifdef ENTANGLED_D3D12_PROBE_SHADER
    DdiTransferList *commands = recording_command_list(list);
    if (commands) {
        commands->draw.uniform = NULL;
        commands->draw.texture = NULL;
        commands->draw.sampler = NULL;
        memset(commands->compute.tables, 0, sizeof(commands->compute.tables));
        memset(commands->compute.constants, 0, sizeof(commands->compute.constants));
    }
#else
    require_transfer_state(list, TRUE);
#endif
}

static void set_command_device_error(const DdiDevice *device, HRESULT status)
{
    if (FAILED(status) && device && device->callbacks && device->callbacks->pfnSetErrorCb)
        device->callbacks->pfnSetErrorCb(device->runtime, status);
}

static BOOL supports_command_queue_flags(D3D12DDI_COMMAND_QUEUE_FLAGS flags)
{
    return !(flags & ~(D3D12DDI_COMMAND_QUEUE_FLAG_3D | D3D12DDI_COMMAND_QUEUE_FLAG_COMPUTE |
                      D3D12DDI_COMMAND_QUEUE_FLAG_COPY));
}

static SIZE_T APIENTRY private_command_queue_size(D3D12DDI_HDEVICE handle,
                                                 const D3D12DDIARG_CREATECOMMANDQUEUE_0023 *args)
{
    return handle.pDrvPrivate && args ? sizeof(DdiCommandQueue) : 0;
}

static HRESULT APIENTRY create_command_queue(D3D12DDI_HDEVICE handle,
                                             const D3D12DDIARG_CREATECOMMANDQUEUE_0023 *args,
                                             D3D12DDI_HCOMMANDQUEUE driver_queue, D3D12DDI_HRTCOMMANDQUEUE runtime)
{
    DdiDevice *device = handle.pDrvPrivate;
    DdiCommandQueue *queue = driver_queue.pDrvPrivate;
    if (!device || !args || !queue || !device->callbacks ||
        !device->callbacks->pfnCreateContextVirtualCb || !device->callbacks->pfnDestroyContextCb)
        return E_INVALIDARG;
    if (args->NodeMask > 1 || !supports_command_queue_flags(args->QueueFlags) ||
        args->QueueCreationFlags != D3D12DDI_COMMAND_QUEUE_CREATION_FLAG_NONE)
        return DXGI_ERROR_UNSUPPORTED;

    *queue = (DdiCommandQueue){0};
    /* The single physical adapter is index 0. Node ordinal 1 is its GPUVA engine. */
    D3DDDICB_CREATECONTEXTVIRTUAL context = { .NodeOrdinal = 1 };
    const HRESULT status = device->callbacks->pfnCreateContextVirtualCb(runtime, &context);
    if (SUCCEEDED(status))
        *queue = (DdiCommandQueue){ .device = device, .runtime = runtime, .context = context.hContext };
    trace_ddi("CreateCommandQueue", (UINT)status, args->QueueFlags);
    return status;
}

static void APIENTRY destroy_command_queue(D3D12DDI_HDEVICE handle, D3D12DDI_HCOMMANDQUEUE driver_queue)
{
    DdiCommandQueue *queue = driver_queue.pDrvPrivate;
    if (!queue || !queue->device || queue->device != handle.pDrvPrivate)
        return;
    const D3DDDICB_DESTROYCONTEXT context = { .hContext = queue->context };
    const HRESULT status = queue->device->callbacks->pfnDestroyContextCb(queue->runtime, &context);
    trace_ddi("DestroyCommandQueue", (UINT)status, 0);
    *queue = (DdiCommandQueue){0};
}

static SIZE_T APIENTRY private_command_pool_size(D3D12DDI_HDEVICE device,
                                                 const D3D12DDIARG_CREATE_COMMAND_POOL_0040 *args)
{
    return device.pDrvPrivate && args ? sizeof(DdiCommandPool) : 0;
}

static HRESULT APIENTRY create_command_pool(D3D12DDI_HDEVICE handle,
                                            const D3D12DDIARG_CREATE_COMMAND_POOL_0040 *args,
                                            D3D12DDI_HCOMMANDPOOL_0040 driver_pool)
{
    DdiDevice *device = handle.pDrvPrivate;
    DdiCommandPool *pool = driver_pool.pDrvPrivate;
    if (!device || !args || !pool)
        return E_INVALIDARG;
    if (args->PoolFlags != D3D12DDI_COMMAND_POOL_FLAG_NONE)
        return DXGI_ERROR_UNSUPPORTED;
    *pool = (DdiCommandPool){0};
    const HRESULT status = create_venus_command_pool(&device->venus, 0, &pool->pool);
    if (SUCCEEDED(status))
        pool->device = device;
    trace_ddi("CreateCommandPool", (UINT)status, args->PoolFlags);
    return status;
}

#ifdef ENTANGLED_D3D12_PROBE_SHADER
static void clear_command_samplers(DdiCommandPool *pool)
{
    if (!pool->sampler_count)
        return;
    const VenusDevice *venus = &pool->device->venus;
    const PFN_vkDestroySampler destroy = (PFN_vkDestroySampler)venus->proc_addr(venus->handle, "vkDestroySampler");
    for (UINT i = 0; i < pool->sampler_count; ++i)
        destroy(venus->handle, pool->samplers[i], NULL);
    pool->sampler_count = 0;
}
#endif

static void APIENTRY reset_command_pool(D3D12DDI_HDEVICE handle, D3D12DDI_HCOMMANDPOOL_0040 driver_pool)
{
    DdiDevice *device = handle.pDrvPrivate;
    DdiCommandPool *pool = driver_pool.pDrvPrivate;
    HRESULT status = E_INVALIDARG;
    if (device && pool && pool->device == device) {
        AcquireSRWLockExclusive(&pool->lock);
        status = reset_venus_command_pool(&device->venus, pool->pool);
        if (SUCCEEDED(status)) {
            pool->used = 0;
            ++pool->generation;
#ifdef ENTANGLED_D3D12_PROBE_SHADER
            if (pool->descriptor_pool) {
                const PFN_vkResetDescriptorPool reset = (PFN_vkResetDescriptorPool)
                    device->venus.proc_addr(device->venus.handle, "vkResetDescriptorPool");
                status = venus_hresult(reset(device->venus.handle, pool->descriptor_pool, 0));
                if (SUCCEEDED(status))
                    clear_command_samplers(pool);
            }
#endif
        }
        ReleaseSRWLockExclusive(&pool->lock);
    }
    trace_ddi("ResetCommandPool", (UINT)status, 0);
    set_command_device_error(device, status);
}

static void APIENTRY destroy_command_pool(D3D12DDI_HDEVICE handle, D3D12DDI_HCOMMANDPOOL_0040 driver_pool)
{
    DdiCommandPool *pool = driver_pool.pDrvPrivate;
    if (!pool || !pool->device || pool->device != handle.pDrvPrivate)
        return;
    destroy_venus_command_pool(&pool->device->venus, pool->pool);
#ifdef ENTANGLED_D3D12_PROBE_SHADER
    if (pool->descriptor_pool) {
        const VenusDevice *venus = &pool->device->venus;
        const PFN_vkDestroyDescriptorPool destroy = (PFN_vkDestroyDescriptorPool)
            venus->proc_addr(venus->handle, "vkDestroyDescriptorPool");
        destroy(venus->handle, pool->descriptor_pool, NULL);
        clear_command_samplers(pool);
    }
#endif
    if (pool->commands)
        HeapFree(GetProcessHeap(), 0, pool->commands);
    *pool = (DdiCommandPool){0};
}

static SIZE_T APIENTRY private_command_recorder_size(D3D12DDI_HDEVICE device,
                                                     const D3D12DDIARG_CREATE_COMMAND_RECORDER_0040 *args)
{
    return device.pDrvPrivate && args ? sizeof(DdiCommandRecorder) : 0;
}

static HRESULT APIENTRY create_command_recorder(D3D12DDI_HDEVICE handle,
                                                const D3D12DDIARG_CREATE_COMMAND_RECORDER_0040 *args,
                                                D3D12DDI_HCOMMANDRECORDER_0040 driver_recorder)
{
    DdiCommandRecorder *recorder = driver_recorder.pDrvPrivate;
    if (!handle.pDrvPrivate || !args || !recorder)
        return E_INVALIDARG;
    if (args->RecorderFlags != D3D12DDI_COMMAND_RECORDER_FLAG_NONE || !supports_command_queue_flags(args->QueueFlags))
        return DXGI_ERROR_UNSUPPORTED;
    *recorder = (DdiCommandRecorder){ .device = handle.pDrvPrivate };
    trace_ddi("CreateCommandRecorder", 0, args->QueueFlags);
    return S_OK;
}

static void APIENTRY destroy_command_recorder(D3D12DDI_HDEVICE handle,
                                             D3D12DDI_HCOMMANDRECORDER_0040 driver_recorder)
{
    DdiCommandRecorder *recorder = driver_recorder.pDrvPrivate;
    if (recorder && recorder->device == handle.pDrvPrivate)
        *recorder = (DdiCommandRecorder){0};
}

static void APIENTRY set_command_pool_as_target(D3D12DDI_HDEVICE handle,
                                               D3D12DDI_HCOMMANDRECORDER_0040 driver_recorder,
                                               D3D12DDI_HCOMMANDPOOL_0040 driver_pool)
{
    DdiDevice *device = handle.pDrvPrivate;
    DdiCommandRecorder *recorder = driver_recorder.pDrvPrivate;
    DdiCommandPool *pool = driver_pool.pDrvPrivate;
    if (!device || !recorder || recorder->device != device || (pool && pool->device != device)) {
        set_command_device_error(device, E_INVALIDARG);
        return;
    }
    recorder->target = pool;
    trace_ddi("SetCommandPoolAsTarget", 0, pool != NULL);
}

static SIZE_T APIENTRY private_command_list_size(D3D12DDI_HDEVICE device,
                                                 const D3D12DDIARG_CREATE_COMMAND_LIST_0040 *args)
{
    return device.pDrvPrivate && args ? sizeof(DdiTransferList) : 0;
}

static HRESULT APIENTRY create_command_list(D3D12DDI_HDEVICE handle, const D3D12DDIARG_CREATE_COMMAND_LIST_0040 *args,
                                            D3D12DDI_HCOMMANDLIST driver_list, D3D12DDI_HRTCOMMANDLIST runtime)
{
    DdiDevice *device = handle.pDrvPrivate;
    DdiTransferList *list = driver_list.pDrvPrivate;
    if (!device || !args || !list || !device->callbacks || !device->callbacks->pfnSetCommandListDDITableCb)
        return E_INVALIDARG;
    if (args->NodeMask > 1 || args->CommandListFlags != D3D12DDI_COMMAND_LIST_FLAG_NONE ||
        !supports_command_queue_flags(args->QueueFlags) || args->Type != D3D12DDI_COMMAND_LIST_TYPE_DIRECT)
        return DXGI_ERROR_UNSUPPORTED;

    *list = (DdiTransferList){ .device = device, .runtime = runtime, .closed = TRUE };
    device->callbacks->pfnSetCommandListDDITableCb(runtime, device->adapter->command_list_table);
    trace_ddi("CreateCommandList", args->Type, args->QueueFlags);
    return S_OK;
}

static void APIENTRY destroy_command_list(D3D12DDI_HDEVICE handle, D3D12DDI_HCOMMANDLIST driver_list)
{
    DdiTransferList *list = driver_list.pDrvPrivate;
    if (list && list->device == handle.pDrvPrivate)
        *list = (DdiTransferList){0};
    /* The allocator can retain submitted command buffers after the list is destroyed. */
}

static HRESULT reserve_command_buffer_slot(DdiCommandPool *pool)
{
    if (pool->used < pool->capacity)
        return S_OK;
    if (pool->capacity > SIZE_MAX / sizeof(*pool->commands) / 2)
        return E_OUTOFMEMORY;
    const SIZE_T capacity = pool->capacity ? pool->capacity * 2 : 8;
    const SIZE_T bytes = capacity * sizeof(*pool->commands);
    HANDLE process_heap = GetProcessHeap();
    VkCommandBuffer *commands = pool->commands
        ? HeapReAlloc(process_heap, HEAP_ZERO_MEMORY, pool->commands, bytes)
        : HeapAlloc(process_heap, HEAP_ZERO_MEMORY, bytes);
    if (!commands)
        return E_OUTOFMEMORY;
    pool->commands = commands;
    pool->capacity = capacity;
    return S_OK;
}

static void APIENTRY reset_command_list(D3D12DDI_HCOMMANDLIST driver_list, const D3D12DDIARG_RESETCOMMANDLIST_0040 *args)
{
    DdiTransferList *list = driver_list.pDrvPrivate;
    const DdiCommandRecorder *recorder = args ? args->hDrvCommandRecorder.pDrvPrivate : NULL;
    DdiCommandPool *pool = recorder ? recorder->target : NULL;
    if (!list || !list->device)
        return;
    list->pool = NULL;
    list->closed = FALSE;
    list->venus = (VenusTransferList){0};
    list->written_color = NULL;
    list->uploaded_color = NULL;
    list->sampled_color = NULL;
#ifdef ENTANGLED_D3D12_PROBE_PRESENT
    list->present = NULL;
#endif
    list->color_copy_source = NULL;
    list->color_rendered = FALSE;
    list->draw = (DdiDrawState){0};
#ifdef ENTANGLED_D3D12_PROBE_SHADER
    list->compute = (DdiComputeState){0};
#endif
    if (!pool || pool->device != list->device || recorder->device != list->device ||
        args->CommandListFlags != D3D12DDI_COMMAND_LIST_FLAG_NONE) {
        set_command_list_status(list, E_INVALIDARG);
        return;
    }

    AcquireSRWLockExclusive(&pool->lock);
    HRESULT status = reserve_command_buffer_slot(pool);
    if (SUCCEEDED(status)) {
        VkCommandBuffer *command = &pool->commands[pool->used++];
        const VenusDevice *venus = &list->device->venus;
        status = begin_venus_command_buffer(venus, pool->pool, 0, command);
        if (SUCCEEDED(status)) {
            list->pool = pool;
            list->generation = pool->generation;
            list->venus = (VenusTransferList){ .device = venus, .pool = pool->pool, .command = *command };
        }
    }
    ReleaseSRWLockExclusive(&pool->lock);
    trace_ddi("ResetCommandList", (UINT)status, 0);
    set_command_list_status(list, status);
}

#ifdef ENTANGLED_D3D12_PROBE_SHADER
static void APIENTRY set_command_pipeline(D3D12DDI_HCOMMANDLIST driver_list, D3D12DDI_HPIPELINESTATE driver_pipeline)
{
    DdiTransferList *list = driver_list.pDrvPrivate;
    const DdiPipeline *pipeline = driver_pipeline.pDrvPrivate;
    if (!list || FAILED(list->status))
        return;
    if (!list->pool || !pipeline || !pipeline->pipeline || pipeline->device != list->venus.device) {
        set_command_list_status(list, E_INVALIDARG);
        return;
    }
    const VenusDevice *device = pipeline->device;
    const PFN_vkCmdBindPipeline bind = (PFN_vkCmdBindPipeline)device->proc_addr(device->handle, "vkCmdBindPipeline");
    AcquireSRWLockExclusive(&list->pool->lock);
    HRESULT status = E_INVALIDARG;
    if (!list->closed && list->generation == list->pool->generation) {
        status = bind ? S_OK : E_FAIL;
        if (bind) {
            bind(list->venus.command, pipeline->bind_point, pipeline->pipeline);
            if (pipeline->bind_point == VK_PIPELINE_BIND_POINT_GRAPHICS)
                list->draw.pipeline = pipeline;
            else
                list->compute.pipeline = pipeline;
        }
    }
    ReleaseSRWLockExclusive(&list->pool->lock);
    trace_ddi("SetPipelineState", (UINT)status, pipeline->bind_point);
    set_command_list_status(list, status);
}
#endif

typedef struct DdiFence {
    DdiDevice *device;
    D3D12DDI_FENCE placement;
} DdiFence;

static SIZE_T APIENTRY private_fence_size(D3D12DDI_HDEVICE device, const D3D12DDIARG_CREATE_FENCE *args)
{
    return device.pDrvPrivate && args ? sizeof(DdiFence) : 0;
}

static HRESULT APIENTRY create_fence(D3D12DDI_HDEVICE device, D3D12DDI_HFENCE handle,
                                    const D3D12DDIARG_CREATE_FENCE *args)
{
    DdiFence *fence = handle.pDrvPrivate;
    if (!fence || !device.pDrvPrivate || !args || !args->Fences || args->FenceCount != 1)
        return E_INVALIDARG;
    *fence = (DdiFence){ .device = device.pDrvPrivate, .placement = args->Fences[0] };
    trace_ddi("CreateFence", args->FenceCount, fence->placement.Flags);
    return S_OK;
}

static void APIENTRY destroy_fence(D3D12DDI_HDEVICE device, D3D12DDI_HFENCE handle)
{
    DdiFence *fence = handle.pDrvPrivate;
    if (fence && fence->device == device.pDrvPrivate)
        *fence = (DdiFence){0};
}

static HRESULT gather_transfer_commands(const DdiDevice *device, UINT count, const D3D12DDI_HCOMMANDLIST *lists,
                                        VkCommandBuffer *commands)
{
    for (UINT i = 0; i < count; ++i) {
        const DdiTransferList *list = lists[i].pDrvPrivate;
        if (!list || list->device != device || !list->closed || !list->pool || FAILED(list->status))
            return E_INVALIDARG;
        DdiCommandPool *pool = list->pool;
        AcquireSRWLockShared(&pool->lock);
        const BOOL available = list->generation == pool->generation;
        ReleaseSRWLockShared(&pool->lock);
        if (!available)
            return E_INVALIDARG;
        commands[i] = list->venus.command;
    }
    return S_OK;
}

static void APIENTRY execute_transfer_lists(D3D12DDI_HCOMMANDQUEUE driver_queue, UINT count,
                                           const D3D12DDI_HCOMMANDLIST *lists)
{
    DdiCommandQueue *queue = driver_queue.pDrvPrivate;
    if (!queue || !queue->device || !count)
        return;
    DdiDevice *device = queue->device;
    if (!lists || count > SIZE_MAX / sizeof(VkCommandBuffer)) {
        set_command_device_error(device, E_INVALIDARG);
        return;
    }
    VkCommandBuffer local[16];
    VkCommandBuffer *commands = count <= _countof(local)
        ? local : HeapAlloc(GetProcessHeap(), 0, (SIZE_T)count * sizeof(*commands));
    if (!commands) {
        set_command_device_error(device, E_OUTOFMEMORY);
        return;
    }

    HRESULT status = gather_transfer_commands(device, count, lists, commands);
    if (SUCCEEDED(status)) {
        /* The lab completes Venus work before the runtime can signal its WDDM fence. */
        AcquireSRWLockExclusive(&device->queue_lock);
        status = submit_venus_transfer_commands(&device->venus, count, commands);
#ifdef ENTANGLED_D3D12_PROBE_PRESENT
        if (SUCCEEDED(status)) {
            for (UINT i = 0; i < count; ++i) {
                const DdiTransferList *list = lists[i].pDrvPrivate;
                if (list->present)
                    list->present->ready = TRUE;
            }
        }
#endif
        ReleaseSRWLockExclusive(&device->queue_lock);
    }
    if (commands != local)
        HeapFree(GetProcessHeap(), 0, commands);
    trace_ddi("ExecuteCommandLists retired", (UINT)status, count);
    set_command_device_error(device, status);
}

static const D3D12DDI_COMMAND_QUEUE_FUNCS_CORE_0001 entangled_transfer_queue_ddi = {
    .pfnExecuteCommandLists = execute_transfer_lists,
};
