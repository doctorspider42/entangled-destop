/* SPDX-License-Identifier: Apache-2.0 */
typedef struct DdiCommandPool {
    DdiDevice *device;
    VkCommandPool pool;
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
    const DdiCommandPool *pool = driver_pool.pDrvPrivate;
    const HRESULT status = device && pool && pool->device == device
        ? reset_venus_command_pool(&device->venus, pool->pool) : E_INVALIDARG;
    trace_ddi("ResetCommandPool", (UINT)status, 0);
    set_command_device_error(device, status);
}

static void APIENTRY destroy_command_pool(D3D12DDI_HDEVICE handle, D3D12DDI_HCOMMANDPOOL_0040 driver_pool)
{
    DdiCommandPool *pool = driver_pool.pDrvPrivate;
    if (!pool || !pool->device || pool->device != handle.pDrvPrivate)
        return;
    destroy_venus_command_pool(&pool->device->venus, pool->pool);
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
