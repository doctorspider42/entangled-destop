/* SPDX-License-Identifier: Apache-2.0 */
typedef struct DdiBufferResource {
    VenusTransferBuffer *backing;
} DdiBufferResource;

static const VenusTransferBuffer *resource_buffer(D3D12DDI_HDEVICE device, D3D12DDI_HRESOURCE resource)
{
    const DdiBufferResource *object = resource.pDrvPrivate;
    const VenusTransferBuffer *buffer = object ? object->backing : NULL;
    return buffer && buffer->device == venus_device(device) ? buffer : NULL;
}

static D3D12DDI_RESOURCE_ALLOCATION_INFO_0022 buffer_allocation_info(UINT64 size)
{
    const UINT32 alignment = D3D12_DEFAULT_RESOURCE_PLACEMENT_ALIGNMENT;
    if (!size || size > UINT64_MAX - (alignment - 1))
        return (D3D12DDI_RESOURCE_ALLOCATION_INFO_0022){ .ResourceDataSize = UINT64_MAX };
    return (D3D12DDI_RESOURCE_ALLOCATION_INFO_0022){
        .ResourceDataSize = (size + alignment - 1) & ~((UINT64)alignment - 1),
        .ResourceDataAlignment = alignment,
        .Layout = D3D12DDI_TL_ROW_MAJOR,
    };
}

static void APIENTRY check_buffer_allocation(D3D12DDI_HDEVICE device,
                                             const D3D12DDIARG_CREATERESOURCE_0003 *resource,
                                             D3D12DDI_RESOURCE_OPTIMIZATION_FLAGS optimization,
                                             UINT32 alignment_restriction, UINT visible_node_mask,
                                             D3D12DDI_RESOURCE_ALLOCATION_INFO_0022 *info)
{
    (void)optimization;
    trace_ddi("CheckResourceAllocationInfo", resource ? resource->ResourceType : 0, resource ? resource->Width : 0);
    const UINT32 alignment = D3D12_DEFAULT_RESOURCE_PLACEMENT_ALIGNMENT;
    if (!device.pDrvPrivate || !resource || resource->ResourceType != D3D12DDI_RT_BUFFER ||
        (resource->Layout != D3D12DDI_TL_ROW_MAJOR && resource->Layout != D3D12DDI_TL_UNDEFINED) ||
        visible_node_mask > 1 || (alignment_restriction && alignment_restriction < alignment)) {
        *info = buffer_allocation_info(0);
        return;
    }
    *info = buffer_allocation_info(resource->Width);
}

static void APIENTRY check_existing_buffer_allocation(D3D12DDI_HDEVICE device, D3D12DDI_HRESOURCE resource,
                                                      D3D12DDI_RESOURCE_ALLOCATION_INFO_0022 *info)
{
    const VenusTransferBuffer *buffer = resource_buffer(device, resource);
    *info = buffer_allocation_info(buffer ? buffer->size : 0);
    trace_ddi("CheckExistingResourceAllocationInfo", 0, info->ResourceDataSize);
}

static D3D12DDI_GPU_VIRTUAL_ADDRESS APIENTRY check_buffer_address(D3D12DDI_HDEVICE device, D3D12DDI_HRESOURCE resource)
{
    const VenusTransferBuffer *buffer = resource_buffer(device, resource);
    const VkDeviceAddress address = buffer ? buffer->address : 0;
    trace_ddi("CheckResourceVirtualAddress", 0, address != 0);
    return address;
}

static void APIENTRY check_buffer_subresource(D3D12DDI_HDEVICE device, D3D12DDI_HRESOURCE resource, UINT subresource,
                                             D3D12DDI_SUBRESOURCE_INFO *info)
{
    const VenusTransferBuffer *buffer = resource_buffer(device, resource);
    const UINT64 stride = buffer && !subresource ? buffer->size : 0;
    *info = (D3D12DDI_SUBRESOURCE_INFO){ .RowStride = stride, .DepthStride = stride };
    trace_ddi("CheckSubresourceInfo", subresource, stride);
}

static D3D12DDI_HEAP_AND_RESOURCE_SIZES APIENTRY private_buffer_sizes(
    D3D12DDI_HDEVICE device, const D3D12DDIARG_CREATEHEAP_0001 *heap,
    const D3D12DDIARG_CREATERESOURCE_0003 *resource, D3D12DDI_HPROTECTEDRESOURCESESSION_0030 protection)
{
    (void)device;
    (void)protection;
    return (D3D12DDI_HEAP_AND_RESOURCE_SIZES){
        .Heap = heap ? sizeof(VenusTransferBuffer) : 0,
        .Resource = resource ? sizeof(DdiBufferResource) : 0,
    };
}

static HRESULT APIENTRY create_committed_buffer(
    D3D12DDI_HDEVICE device, const D3D12DDIARG_CREATEHEAP_0001 *heap, D3D12DDI_HHEAP driver_heap,
    D3D12DDI_HRTRESOURCE runtime_resource, const D3D12DDIARG_CREATERESOURCE_0003 *resource,
    const D3D12DDI_CLEAR_VALUES *clear, D3D12DDI_HPROTECTEDRESOURCESESSION_0030 protection,
    D3D12DDI_HRESOURCE driver_resource)
{
    (void)runtime_resource;
    const D3D12DDI_RESOURCE_FLAGS_0003 supported_flags = D3D12DDI_RESOURCE_FLAG_0003_SHADER_RESOURCE |
        D3D12DDI_RESOURCE_FLAG_0022_UNORDERED_ACCESS | D3D12DDI_RESOURCE_FLAG_0003_SIMULTANEOUS_ACCESS;
    if (heap && resource) {
        trace_ddi("CreateHeapAndResource flags", resource->Flags, heap->Flags);
        trace_ddi("CreateHeapAndResource memory", heap->MemoryPool, heap->CPUPageProperty);
    }
    if (!heap || !resource || protection.pDrvPrivate || clear ||
        heap->MemoryPool != D3D12DDI_MEMORY_POOL_L0 ||
        (heap->Flags & D3D12DDI_HEAP_FLAG_PRIMARY) ||
        heap->CreationNodeMask > 1 || heap->VisibleNodeMask > 1 ||
        resource->ReuseBufferGPUVA.BaseAddress.UMD.hResource.pDrvPrivate ||
        resource->ResourceType != D3D12DDI_RT_BUFFER || (resource->Flags & ~supported_flags) ||
        resource->Layout != D3D12DDI_TL_ROW_MAJOR)
        return DXGI_ERROR_UNSUPPORTED;

    const BOOL readback = heap->CPUPageProperty == D3D12DDI_CPU_PAGE_PROPERTY_WRITE_BACK;
    if (!readback && heap->CPUPageProperty != D3D12DDI_CPU_PAGE_PROPERTY_WRITE_COMBINE)
        return DXGI_ERROR_UNSUPPORTED;

    VenusTransferBuffer *buffer = driver_heap.pDrvPrivate;
    DdiBufferResource *object = driver_resource.pDrvPrivate;
    if (!device.pDrvPrivate || !buffer || !object || !resource->Width)
        return E_INVALIDARG;
    if (heap->ByteSize && heap->ByteSize < resource->Width)
        return E_INVALIDARG;

    *object = (DdiBufferResource){0};
    VkBufferUsageFlags uses = VK_BUFFER_USAGE_VERTEX_BUFFER_BIT | VK_BUFFER_USAGE_INDEX_BUFFER_BIT |
                             VK_BUFFER_USAGE_UNIFORM_BUFFER_BIT | VK_BUFFER_USAGE_INDIRECT_BUFFER_BIT;
    if (resource->Flags & D3D12DDI_RESOURCE_FLAG_0003_SHADER_RESOURCE)
        uses |= VK_BUFFER_USAGE_UNIFORM_TEXEL_BUFFER_BIT | VK_BUFFER_USAGE_STORAGE_BUFFER_BIT;
    if (resource->Flags & D3D12DDI_RESOURCE_FLAG_0022_UNORDERED_ACCESS)
        uses |= VK_BUFFER_USAGE_STORAGE_TEXEL_BUFFER_BIT | VK_BUFFER_USAGE_STORAGE_BUFFER_BIT;
    HRESULT status = create_venus_transfer_buffer(venus_device(device), resource->Width, readback, uses, buffer);
    if (SUCCEEDED(status))
        object->backing = buffer;
    trace_ddi("CreateHeapAndResource result", (UINT)status, resource->Width);
    return status;
}

static void APIENTRY destroy_committed_buffer(D3D12DDI_HDEVICE device, D3D12DDI_HHEAP heap,
                                              D3D12DDI_HRESOURCE resource)
{
    (void)device;
    DdiBufferResource *object = resource.pDrvPrivate;
    if (object)
        object->backing = NULL;
    if (heap.pDrvPrivate)
        destroy_venus_transfer_buffer(heap.pDrvPrivate);
}

static HRESULT APIENTRY map_buffer_heap(D3D12DDI_HDEVICE device, D3D12DDI_HHEAP heap, void **data)
{
    const VenusTransferBuffer *buffer = heap.pDrvPrivate;
    if (!buffer || buffer->device != venus_device(device) || !buffer->mapped || !data)
        return E_INVALIDARG;
    *data = buffer->mapped;
    trace_ddi("MapHeap", 0, buffer->size);
    return S_OK;
}

static void APIENTRY unmap_buffer_heap(D3D12DDI_HDEVICE device, D3D12DDI_HHEAP heap)
{
    /* The coherent Vulkan mapping lives as long as the backing allocation. */
    (void)device;
    (void)heap;
}

static HRESULT check_backing_resident(const VenusDevice *device, D3D12DDI_HANDLE_AND_TYPE object)
{
    if (!object.Handle)
        return E_INVALIDARG;
#ifdef ENTANGLED_D3D12_PROBE_SHADER
    if (object.Type == D3D12DDI_HT_PIPELINE_STATE) {
        const DdiPipeline *state = object.Handle;
        return state->pipeline && state->device == device ? S_OK : E_INVALIDARG;
    }
#endif
    const VenusTransferBuffer *buffer;
    switch (object.Type) {
    case D3D12DDI_HT_HEAP:
        buffer = object.Handle;
        break;
    case D3D12DDI_HT_0012_RESOURCE:
        buffer = ((const DdiBufferResource *)object.Handle)->backing;
        break;
    default:
        return DXGI_ERROR_UNSUPPORTED;
    }
    return buffer && buffer->memory && buffer->mapped && buffer->device == device ? S_OK : E_INVALIDARG;
}

static HRESULT APIENTRY make_venus_objects_resident(D3D12DDI_HDEVICE handle, D3D12DDIARG_MAKERESIDENT_0001 *args)
{
    const VenusDevice *device = venus_device(handle);
    if (!device || !args || (args->NumObjects && !args->pObjects))
        return E_INVALIDARG;
    if (args->NumAdapters > 1)
        return DXGI_ERROR_UNSUPPORTED;
    for (UINT i = 0; i < args->NumObjects; ++i) {
        const D3D12DDI_HANDLE_AND_TYPE object = args->pObjects[i];
        const HRESULT status = check_backing_resident(device, object);
        trace_ddi("MakeResident object", object.Type, (UINT)status);
        if (FAILED(status))
            return status;
    }

    /* The probe keeps Venus backing allocated throughout each object's lifetime. */
    args->WaitMask = 0;
    if (args->NumAdapters && args->pPagingFenceValue)
        args->pPagingFenceValue[0] = 0;
    trace_ddi("MakeResident", args->NumObjects, args->NumAdapters);
    return S_OK;
}

static void APIENTRY get_debug_allocation_info(D3D12DDI_HDEVICE device, D3D12DDI_HANDLE_AND_TYPE object,
                                              UINT *num_addresses,
                                              D3D12DDI_DEBUG_VIRTUAL_ADDRESS_ALLOCATION_INFO_0012 *addresses,
                                              UINT *num_allocations, D3D12DDI_DEBUG_KMT_ALLOCATION_INFO_0014 *allocations)
{
    (void)device;
    (void)addresses;
    (void)allocations;
    /* The lab's Venus objects do not have KMT allocation handles or WDDM GPUVAs. */
    *num_addresses = 0;
    *num_allocations = 0;
    trace_ddi("GetDebugAllocationInfo", object.Type, 0);
}

static const D3D12DDI_DEVICE_FUNCS_CORE_0040 entangled_device_core_ddi = {
    .pfnCalcPrivateCommandQueueSize = private_command_queue_size,
    .pfnCreateCommandQueue = create_command_queue,
    .pfnDestroyCommandQueue = destroy_command_queue,
    .pfnCalcPrivateCommandListSize = private_command_list_size,
    .pfnCreateCommandList = create_command_list,
    .pfnDestroyCommandList = destroy_command_list,
    .pfnCalcPrivateFenceSize = private_fence_size,
    .pfnCreateFence = create_fence,
    .pfnDestroyFence = destroy_fence,
    .pfnCalcPrivateCommandPoolSize = private_command_pool_size,
    .pfnCreateCommandPool = create_command_pool,
    .pfnDestroyCommandPool = destroy_command_pool,
    .pfnResetCommandPool = reset_command_pool,
    .pfnCalcPrivateCommandRecorderSize = private_command_recorder_size,
    .pfnCreateCommandRecorder = create_command_recorder,
    .pfnDestroyCommandRecorder = destroy_command_recorder,
    .pfnCommandRecorderSetCommandPoolAsTarget = set_command_pool_as_target,
    .pfnCalcPrivateBlendStateSize = private_blend_state_size,
    .pfnCreateBlendState = create_blend_state,
    .pfnDestroyBlendState = destroy_blend_state,
    .pfnCalcPrivateDepthStencilStateSize = private_depth_stencil_state_size,
    .pfnCreateDepthStencilState = create_depth_stencil_state,
    .pfnDestroyDepthStencilState = destroy_depth_stencil_state,
    .pfnCalcPrivateRasterizerStateSize = private_rasterizer_state_size,
    .pfnCreateRasterizerState = create_rasterizer_state,
    .pfnDestroyRasterizerState = destroy_rasterizer_state,
#ifdef ENTANGLED_D3D12_PROBE_SHADER
    .pfnCalcPrivateShaderSize = private_shader_size,
    .pfnCreateVertexShader = create_vertex_shader,
    .pfnCreateComputeShader = create_compute_shader,
    .pfnDestroyShader = destroy_shader,
    .pfnCalcPrivatePipelineStateSize = private_pipeline_size,
    .pfnCreatePipelineState = create_pipeline,
    .pfnDestroyPipelineState = destroy_pipeline,
#endif
    .pfnMakeResident = make_venus_objects_resident,
    .pfnCalcPrivateRootSignatureSize = private_root_signature_size,
    .pfnCreateRootSignature = create_root_signature,
    .pfnDestroyRootSignature = destroy_root_signature,
    .pfnCalcPrivateDescriptorHeapSize = private_descriptor_heap_size,
    .pfnCreateDescriptorHeap = create_descriptor_heap,
    .pfnDestroyDescriptorHeap = destroy_descriptor_heap,
    .pfnGetDescriptorSizeInBytes = descriptor_size,
    .pfnGetCPUDescriptorHandleForHeapStart = cpu_descriptor_heap_start,
    .pfnGetGPUDescriptorHandleForHeapStart = gpu_descriptor_heap_start,
    .pfnCalcPrivateHeapAndResourceSizes = private_buffer_sizes,
    .pfnCreateHeapAndResource = create_committed_buffer,
    .pfnDestroyHeapAndResource = destroy_committed_buffer,
    .pfnMapHeap = map_buffer_heap,
    .pfnUnmapHeap = unmap_buffer_heap,
    .pfnCheckResourceAllocationInfo = check_buffer_allocation,
    .pfnCheckSubresourceInfo = check_buffer_subresource,
    .pfnCheckExistingResourceAllocationInfo = check_existing_buffer_allocation,
    .pfnCheckResourceVirtualAddress = check_buffer_address,
    .pfnGetDebugAllocationInfo = get_debug_allocation_info,
    .pfnGetImplicitPhysicalAdapterMask = implicit_physical_adapter_mask,
    .pfnQueryNodeMap = query_node_map,
};

static void APIENTRY copy_buffer_region(D3D12DDI_HCOMMANDLIST list,
                                       D3D12DDIARG_BUFFER_PLACEMENT dst, D3D12DDIARG_BUFFER_PLACEMENT src,
                                       UINT64 size)
{
    DdiTransferList *transfer = list.pDrvPrivate;
    if (!transfer || FAILED(transfer->status) || !size)
        return;
    const DdiBufferResource *source = src.BaseAddress.UMD.hResource.pDrvPrivate;
    const DdiBufferResource *target = dst.BaseAddress.UMD.hResource.pDrvPrivate;
    if (!source || !target || !source->backing || !target->backing ||
        source->backing->device != transfer->venus.device ||
        target->backing->device != transfer->venus.device) {
        set_command_list_status(transfer, E_INVALIDARG);
        return;
    }
    const VkBufferCopy region = {
        .srcOffset = src.BaseAddress.UMD.Offset,
        .dstOffset = dst.BaseAddress.UMD.Offset,
        .size = size,
    };
    DdiCommandPool *pool = transfer->pool;
    if (pool)
        AcquireSRWLockExclusive(&pool->lock);
    const HRESULT status = !transfer->closed && (!pool || transfer->generation == pool->generation)
        ? record_venus_buffer_copy(transfer->venus.command, target->backing, source->backing, region)
        : E_INVALIDARG;
    if (pool)
        ReleaseSRWLockExclusive(&pool->lock);
    if (transfer->device)
        trace_ddi("CopyBufferRegion", (UINT)status, size);
    set_command_list_status(transfer, status);
}

static void APIENTRY close_transfer_list(D3D12DDI_HCOMMANDLIST list)
{
    DdiTransferList *transfer = list.pDrvPrivate;
    if (!transfer || FAILED(transfer->status))
        return;
    DdiCommandPool *pool = transfer->pool;
    if (pool)
        AcquireSRWLockExclusive(&pool->lock);
    const HRESULT status = !transfer->closed && transfer->venus.command &&
        (!pool || transfer->generation == pool->generation) ? close_venus_transfer_list(&transfer->venus) : E_INVALIDARG;
    transfer->closed = SUCCEEDED(status);
    if (pool)
        ReleaseSRWLockExclusive(&pool->lock);
    if (transfer->device)
        trace_ddi("CloseCommandList", (UINT)status, 0);
    set_command_list_status(transfer, status);
}

static const D3D12DDI_COMMAND_LIST_FUNCS_3D_0040 entangled_transfer_list_ddi = {
    .pfnCloseCommandList = close_transfer_list,
    .pfnResetCommandList = reset_command_list,
    .pfnCopyBufferRegion = copy_buffer_region,
#ifdef ENTANGLED_D3D12_PROBE_SHADER
    .pfnSetPipelineState = set_command_pipeline,
#endif
};

static HRESULT probe_ddi_transfer_list(D3D12DDI_HDEVICE device, D3D12DDI_HRESOURCE dst, D3D12DDI_HRESOURCE src,
                                       VkBufferCopy region)
{
    DdiTransferList transfer = {0};
    HRESULT status = begin_venus_transfer_list(venus_device(device), &transfer.venus);
    if (FAILED(status))
        return status;

    const D3D12DDI_HCOMMANDLIST list = { &transfer };
    const D3D12DDIARG_BUFFER_PLACEMENT src_placement = { .BaseAddress.UMD = {src, region.srcOffset} };
    const D3D12DDIARG_BUFFER_PLACEMENT dst_placement = { .BaseAddress.UMD = {dst, region.dstOffset} };
    entangled_transfer_list_ddi.pfnCopyBufferRegion(list, dst_placement, src_placement, region.size);
    entangled_transfer_list_ddi.pfnCloseCommandList(list);
    status = transfer.status;
    if (SUCCEEDED(status))
        status = submit_venus_transfer_probe(&transfer.venus);
    destroy_venus_transfer_list(&transfer.venus);
    return status;
}

static HRESULT probe_d3d12_buffer_ddi(D3D12DDI_HDEVICE device)
{
    const VkBufferCopy region = { .srcOffset = 16, .dstOffset = 32, .size = 64 * 1024 };
    const UINT64 size = region.dstOffset + region.size + 16;
    const D3D12DDI_HPROTECTEDRESOURCESESSION_0030 protection = {0};
    const D3D12DDI_HRTRESOURCE runtime = {0};
    const D3D12DDI_DEVICE_FUNCS_CORE_0040 *ddi = &entangled_device_core_ddi;
    VenusTransferBuffer upload = {0}, readback = {0};
    DdiBufferResource src = {0}, dst = {0};
    const D3D12DDI_HHEAP upload_heap = { &upload }, readback_heap = { &readback };
    const D3D12DDI_HRESOURCE upload_handle = { &src }, readback_handle = { &dst };

    D3D12DDIARG_CREATEHEAP_0001 heap = {
        .MemoryPool = D3D12DDI_MEMORY_POOL_L0,
        .Flags = D3D12DDI_HEAP_FLAG_BUFFERS,
        .CPUPageProperty = D3D12DDI_CPU_PAGE_PROPERTY_WRITE_COMBINE,
        .CreationNodeMask = 1,
        .VisibleNodeMask = 1,
    };
    D3D12DDIARG_CREATERESOURCE_0003 buffer = {
        .ResourceType = D3D12DDI_RT_BUFFER,
        .Width = size,
        .Height = 1,
        .DepthOrArraySize = 1,
        .MipLevels = 1,
        .SampleDesc.Count = 1,
        .InitialResourceState = D3D12DDI_RESOURCE_STATE_COPY_SOURCE,
    };
    D3D12DDI_RESOURCE_ALLOCATION_INFO_0022 allocation;
    ddi->pfnCheckResourceAllocationInfo(device, &buffer, D3D12DDI_RESOURCE_OPTIMIZATION_FLAG_NONE,
                                        0, heap.VisibleNodeMask, &allocation);
    if (allocation.ResourceDataSize != 2 * D3D12_DEFAULT_RESOURCE_PLACEMENT_ALIGNMENT ||
        allocation.ResourceDataAlignment != D3D12_DEFAULT_RESOURCE_PLACEMENT_ALIGNMENT ||
        allocation.Layout != D3D12DDI_TL_ROW_MAJOR)
        return E_UNEXPECTED;
    heap.ByteSize = allocation.ResourceDataSize;
    heap.Alignment = allocation.ResourceDataAlignment;
    buffer.Layout = allocation.Layout;

    const D3D12DDI_HEAP_AND_RESOURCE_SIZES private_sizes =
        ddi->pfnCalcPrivateHeapAndResourceSizes(device, &heap, &buffer, protection);
    if (private_sizes.Heap != sizeof(upload) || private_sizes.Resource != sizeof(src))
        return E_UNEXPECTED;

    HRESULT status = ddi->pfnCreateHeapAndResource(device, &heap, upload_heap, runtime, &buffer,
                                                   NULL, protection, upload_handle);
    heap.CPUPageProperty = D3D12DDI_CPU_PAGE_PROPERTY_WRITE_BACK;
    buffer.InitialResourceState = D3D12DDI_RESOURCE_STATE_COPY_DEST;
    if (SUCCEEDED(status))
        status = ddi->pfnCreateHeapAndResource(device, &heap, readback_heap, runtime, &buffer,
                                               NULL, protection, readback_handle);
    if (SUCCEEDED(status)) {
        D3D12DDI_RESOURCE_ALLOCATION_INFO_0022 existing;
        ddi->pfnCheckExistingResourceAllocationInfo(device, upload_handle, &existing);
        if (existing.ResourceDataSize != allocation.ResourceDataSize ||
            existing.ResourceDataAlignment != allocation.ResourceDataAlignment || existing.Layout != allocation.Layout)
            status = E_UNEXPECTED;
    }
    void *src_data = NULL, *dst_data = NULL;
    if (SUCCEEDED(status))
        status = ddi->pfnMapHeap(device, upload_heap, &src_data);
    if (SUCCEEDED(status))
        status = ddi->pfnMapHeap(device, readback_heap, &dst_data);
    if (SUCCEEDED(status)) {
        BYTE *source = src_data, *target = dst_data;
        for (UINT64 i = 0; i < size; ++i) {
            source[i] = (BYTE)(i * 31 + (i >> 8));
            target[i] = 0xfa;
        }
        status = probe_ddi_transfer_list(device, readback_handle, upload_handle, region);
        if (SUCCEEDED(status)) {
            for (UINT64 i = 0; i < size; ++i) {
                const BYTE expected = i >= region.dstOffset && i - region.dstOffset < region.size
                    ? source[i - region.dstOffset + region.srcOffset] : 0xfa;
                if (target[i] != expected) {
                    status = HRESULT_FROM_WIN32(ERROR_CRC);
                    break;
                }
            }
        }
    }
    if (dst_data)
        ddi->pfnUnmapHeap(device, readback_heap);
    if (src_data)
        ddi->pfnUnmapHeap(device, upload_heap);
    ddi->pfnDestroyHeapAndResource(device, readback_heap, readback_handle);
    ddi->pfnDestroyHeapAndResource(device, upload_heap, upload_handle);
    return status;
}
