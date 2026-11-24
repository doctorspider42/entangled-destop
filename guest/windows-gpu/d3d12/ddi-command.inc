/* SPDX-License-Identifier: Apache-2.0 */
typedef struct DdiCommandPool {
    DdiDevice *device;
    VkCommandPool pool;
    SRWLOCK lock;
    VkCommandBuffer *commands;
    SIZE_T capacity;
    SIZE_T used;
    UINT64 generation;
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

typedef struct DdiTransferList {
    VenusTransferList venus;
    HRESULT status;
    DdiDevice *device;
    D3D12DDI_HRTCOMMANDLIST runtime;
    DdiCommandPool *pool;
    UINT64 generation;
    BOOL closed;
} DdiTransferList;

static void set_command_list_status(DdiTransferList *list, HRESULT status)
{
    list->status = status;
    if (FAILED(status) && list->device && list->device->callbacks->pfnSetCommandListErrorCb)
        list->device->callbacks->pfnSetCommandListErrorCb(list->runtime, status);
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
        if (bind)
            bind(list->venus.command, pipeline->bind_point, pipeline->pipeline);
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
