/* SPDX-License-Identifier: Apache-2.0 */
typedef struct DdiBufferResource {
    VenusTransferBuffer *backing;
} DdiBufferResource;

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
    if (!heap || !resource || protection.pDrvPrivate || clear ||
        heap->MemoryPool != D3D12DDI_MEMORY_POOL_L0 ||
        (heap->Flags & D3D12DDI_HEAP_FLAG_PRIMARY) ||
        heap->CreationNodeMask > 1 || heap->VisibleNodeMask > 1 ||
        resource->ReuseBufferGPUVA.BaseAddress.UMD.hResource.pDrvPrivate ||
        resource->ResourceType != D3D12DDI_RT_BUFFER || resource->Flags ||
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
    HRESULT status = create_venus_transfer_buffer(device.pDrvPrivate, resource->Width, readback, buffer);
    if (SUCCEEDED(status))
        object->backing = buffer;
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
    if (!buffer || buffer->device != device.pDrvPrivate || !buffer->mapped || !data)
        return E_INVALIDARG;
    *data = buffer->mapped;
    return S_OK;
}

static void APIENTRY unmap_buffer_heap(D3D12DDI_HDEVICE device, D3D12DDI_HHEAP heap)
{
    /* The coherent Vulkan mapping lives as long as the backing allocation. */
    (void)device;
    (void)heap;
}

static const D3D12DDI_DEVICE_FUNCS_CORE_0040 entangled_buffer_ddi = {
    .pfnCalcPrivateHeapAndResourceSizes = private_buffer_sizes,
    .pfnCreateHeapAndResource = create_committed_buffer,
    .pfnDestroyHeapAndResource = destroy_committed_buffer,
    .pfnMapHeap = map_buffer_heap,
    .pfnUnmapHeap = unmap_buffer_heap,
};

static HRESULT probe_d3d12_buffer_ddi(D3D12DDI_HDEVICE device)
{
    const VkBufferCopy region = { .srcOffset = 16, .dstOffset = 32, .size = 64 * 1024 };
    const UINT64 size = region.dstOffset + region.size + 16;
    const D3D12DDI_HPROTECTEDRESOURCESESSION_0030 protection = {0};
    const D3D12DDI_HRTRESOURCE runtime = {0};
    const D3D12DDI_DEVICE_FUNCS_CORE_0040 *ddi = &entangled_buffer_ddi;
    VenusTransferBuffer upload = {0}, readback = {0};
    DdiBufferResource src = {0}, dst = {0};
    const D3D12DDI_HHEAP upload_heap = { &upload }, readback_heap = { &readback };
    const D3D12DDI_HRESOURCE upload_handle = { &src }, readback_handle = { &dst };

    D3D12DDIARG_CREATEHEAP_0001 heap = {
        .ByteSize = (size + D3D12_DEFAULT_RESOURCE_PLACEMENT_ALIGNMENT - 1) &
                    ~((UINT64)D3D12_DEFAULT_RESOURCE_PLACEMENT_ALIGNMENT - 1),
        .Alignment = D3D12_DEFAULT_RESOURCE_PLACEMENT_ALIGNMENT,
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
        .Layout = D3D12DDI_TL_ROW_MAJOR,
        .InitialResourceState = D3D12DDI_RESOURCE_STATE_COPY_SOURCE,
    };
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
        status = execute_venus_probe_copy(dst.backing, src.backing, region);
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
