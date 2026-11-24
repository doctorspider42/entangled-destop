/* SPDX-License-Identifier: Apache-2.0 */
typedef struct DdiTimestampHeap {
    const VenusDevice *device;
    VkQueryPool pool;
    UINT count;
} DdiTimestampHeap;

static SIZE_T APIENTRY private_timestamp_heap_size(D3D12DDI_HDEVICE device,
                                                    const D3D12DDIARG_CREATE_QUERY_HEAP_0001 *args)
{
    return device.pDrvPrivate && args ? sizeof(DdiTimestampHeap) : 0;
}

static HRESULT APIENTRY create_timestamp_heap(D3D12DDI_HDEVICE handle, const D3D12DDIARG_CREATE_QUERY_HEAP_0001 *args,
                                               D3D12DDI_HQUERYHEAP heap_handle)
{
    const VenusDevice *device = venus_device(handle);
    DdiTimestampHeap *heap = heap_handle.pDrvPrivate;
    if (!device || !args || !heap || !args->Count)
        return E_INVALIDARG;
    if (args->Type != D3D12DDI_QUERY_HEAP_TYPE_TIMESTAMP || args->NodeMask > 1 ||
        args->Count > 64 || !device->timestamp_valid_bits || !(device->limits.timestampPeriod > 0.0f))
        return DXGI_ERROR_UNSUPPORTED;
    const PFN_vkCreateQueryPool create = (PFN_vkCreateQueryPool)device->proc_addr(device->handle, "vkCreateQueryPool");
    if (!create || !device->proc_addr(device->handle, "vkDestroyQueryPool"))
        return E_FAIL;
    const VkQueryPoolCreateInfo info = {
        .sType = VK_STRUCTURE_TYPE_QUERY_POOL_CREATE_INFO, .queryType = VK_QUERY_TYPE_TIMESTAMP,
        .queryCount = args->Count,
    };
    VkQueryPool pool = VK_NULL_HANDLE;
    const HRESULT status = venus_hresult(create(device->handle, &info, NULL, &pool));
    if (SUCCEEDED(status))
        *heap = (DdiTimestampHeap){ .device = device, .pool = pool, .count = args->Count };
    trace_ddi("CreateTimestampHeap", (UINT)status, device->timestamp_valid_bits);
    return status;
}

static void APIENTRY destroy_timestamp_heap(D3D12DDI_HDEVICE handle, D3D12DDI_HQUERYHEAP heap_handle)
{
    const VenusDevice *device = venus_device(handle);
    DdiTimestampHeap *heap = heap_handle.pDrvPrivate;
    if (!device || !heap || heap->device != device)
        return;
    const PFN_vkDestroyQueryPool destroy = (PFN_vkDestroyQueryPool)device->proc_addr(device->handle, "vkDestroyQueryPool");
    destroy(device->handle, heap->pool, NULL);
    *heap = (DdiTimestampHeap){0};
}

static void APIENTRY begin_unsupported_query(D3D12DDI_HCOMMANDLIST handle, D3D12DDI_HQUERYHEAP heap,
                                             D3D12DDI_QUERY_TYPE type, UINT index)
{
    (void)heap;
    (void)type;
    (void)index;
    require_transfer_state(handle, FALSE);
}

static void APIENTRY end_timestamp(D3D12DDI_HCOMMANDLIST handle, D3D12DDI_HQUERYHEAP heap_handle,
                                   D3D12DDI_QUERY_TYPE type, UINT index)
{
    DdiTransferList *list = recording_command_list(handle);
    if (!list)
        return;
    const VenusDevice *device = list->venus.device;
    const DdiTimestampHeap *heap = heap_handle.pDrvPrivate;
    if (type != D3D12DDI_QUERY_TYPE_TIMESTAMP || !heap || heap->device != device || index >= heap->count ||
        (list->timestamp_pool && list->timestamp_pool != heap->pool)) {
        set_command_list_status(list, DXGI_ERROR_UNSUPPORTED);
        return;
    }
    const PFN_vkCmdResetQueryPool reset = (PFN_vkCmdResetQueryPool)
        device->proc_addr(device->handle, "vkCmdResetQueryPool");
    const PFN_vkCmdWriteTimestamp write = (PFN_vkCmdWriteTimestamp)
        device->proc_addr(device->handle, "vkCmdWriteTimestamp");
    AcquireSRWLockExclusive(&list->pool->lock);
    HRESULT status = E_INVALIDARG;
    if (list->generation == list->pool->generation) {
        status = reset && write ? S_OK : E_FAIL;
        if (SUCCEEDED(status)) {
            reset(list->venus.command, heap->pool, index, 1);
            write(list->venus.command, VK_PIPELINE_STAGE_BOTTOM_OF_PIPE_BIT, heap->pool, index);
            list->timestamp_pool = heap->pool;
            list->written_timestamps |= UINT64_C(1) << index;
        }
    }
    ReleaseSRWLockExclusive(&list->pool->lock);
    trace_ddi("EndTimestamp", (UINT)status, index);
    set_command_list_status(list, status);
}

static void APIENTRY resolve_timestamps(D3D12DDI_HCOMMANDLIST handle, D3D12DDI_HQUERYHEAP heap_handle,
                                       D3D12DDI_QUERY_TYPE type, UINT first, UINT count,
                                       D3D12DDI_HRESOURCE dst, UINT64 offset)
{
    DdiTransferList *list = recording_command_list(handle);
    if (!list || !count)
        return;
    const VenusDevice *device = list->venus.device;
    const DdiTimestampHeap *heap = heap_handle.pDrvPrivate;
    const VenusTransferBuffer *buffer = resource_buffer(device, dst);
    if (type != D3D12DDI_QUERY_TYPE_TIMESTAMP || !heap || heap->device != device || list->timestamp_pool != heap->pool ||
        first >= heap->count || count > heap->count - first || !buffer || offset % sizeof(UINT64) ||
        offset > buffer->size || count > (buffer->size - offset) / sizeof(UINT64)) {
        set_command_list_status(list, E_INVALIDARG);
        return;
    }
    /* Waiting on an unwritten Vulkan query could stall the physical queue. */
    const UINT64 mask = (UINT64_MAX >> (64 - count)) << first;
    if ((list->written_timestamps & mask) != mask) {
        set_command_list_status(list, E_INVALIDARG);
        return;
    }
    const PFN_vkCmdCopyQueryPoolResults copy = (PFN_vkCmdCopyQueryPoolResults)
        device->proc_addr(device->handle, "vkCmdCopyQueryPoolResults");
    AcquireSRWLockExclusive(&list->pool->lock);
    HRESULT status = E_INVALIDARG;
    if (list->generation == list->pool->generation) {
        status = copy ? S_OK : E_FAIL;
        if (copy)
            copy(list->venus.command, heap->pool, first, count, buffer->handle, buffer->offset + offset,
                 sizeof(UINT64), VK_QUERY_RESULT_64_BIT | VK_QUERY_RESULT_WAIT_BIT);
    }
    ReleaseSRWLockExclusive(&list->pool->lock);
    trace_ddi("ResolveTimestamps", (UINT)status, count);
    set_command_list_status(list, status);
}
