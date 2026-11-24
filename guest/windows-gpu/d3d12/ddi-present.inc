/* SPDX-License-Identifier: Apache-2.0 */
static HRESULT allocate_present_surface(const DdiDevice *device, D3D12DDI_HRTRESOURCE resource,
                                        const VenusColorImage *image, DdiPresentSurface *surface)
{
    if (!device->callbacks || !device->callbacks->pfnAllocateCb || !device->callbacks->pfnDeallocateCb ||
        !resource.handle || image->format != VK_FORMAT_B8G8R8A8_UNORM)
        return DXGI_ERROR_UNSUPPORTED;

    const UINT64 bytes = (UINT64)image->extent.width * image->extent.height * 4;
    HRESULT status = create_venus_transfer_buffer(image->device, bytes, TRUE, VK_BUFFER_USAGE_TRANSFER_DST_BIT,
                                                  &surface->pixels);
    if (FAILED(status))
        return status;
    const VIOGPU_CREATE_RESOURCE_EXCHANGE resource_data = {0};
    VIOGPU_CREATE_ALLOCATION_EXCHANGE allocation_data = {
        .ResourceOptions = {
            .target = 2, /* PIPE_TEXTURE_2D */
            .format = 1, /* B8G8R8A8_UNORM */
            .width = image->extent.width,
            .height = image->extent.height,
            .depth = 1,
            .array_size = 1,
        },
        .Size = bytes,
    };
    D3D12DDI_ALLOCATION_INFO_0022 info = {
        .pPrivateDriverData = &allocation_data,
        .PrivateDriverDataSize = sizeof(allocation_data),
        .Flags = D3D12DDI_ALLOCATION_INFO_FLAGS_0022_PRIMARY,
    };
    D3D12DDICB_ALLOCATE_0022 allocate = {
        .pPrivateDriverData = &resource_data,
        .PrivateDriverDataSize = sizeof(resource_data),
        .hResource = resource.handle,
        .NumAllocations = 1,
        .pAllocationInfo = &info,
    };
    status = device->callbacks->pfnAllocateCb(device->runtime, &allocate);
    if (SUCCEEDED(status) && !info.hAllocation) {
        const D3D12DDICB_DEALLOCATE_0022 deallocate = { .hResource = resource.handle };
        const HRESULT cleanup = device->callbacks->pfnDeallocateCb(device->runtime, &deallocate);
        trace_ddi("Allocation cleanup", (UINT)cleanup, 0);
        status = E_FAIL;
    }
    if (SUCCEEDED(status)) {
        surface->runtime_resource = resource.handle;
        surface->allocation = info.hAllocation;
    } else
        destroy_venus_transfer_buffer(&surface->pixels);
    trace_ddi("AllocatePresentSurface", (UINT)status, info.hAllocation);
    return status;
}

static void deallocate_present_surface(const DdiDevice *device, DdiPresentSurface *surface)
{
    const D3D12DDICB_DEALLOCATE_0022 deallocate = { .hResource = surface->runtime_resource };
    const HRESULT status = device->callbacks->pfnDeallocateCb(device->runtime, &deallocate);
    trace_ddi("DeallocatePresentSurface", (UINT)status, surface->allocation);
    destroy_venus_transfer_buffer(&surface->pixels);
}

static HRESULT make_present_surfaces_resident(const DdiDevice *device, D3D12DDIARG_MAKERESIDENT_0001 *args,
                                              const D3DKMT_HANDLE *allocations, UINT count)
{
    if (!args->NumAdapters || !args->pRTPagingQueue || !args->pPagingFenceValue ||
        !device->callbacks || !device->callbacks->pfnMakeResidentCb)
        return E_INVALIDARG;
    D3DDDI_MAKERESIDENT resident = { .NumAllocations = count, .AllocationList = allocations, .Flags = args->Flags };
    const HRESULT status = device->callbacks->pfnMakeResidentCb(device->runtime, args->pRTPagingQueue[0], &resident);
    if (status == E_PENDING) {
        args->WaitMask = 1;
        args->pPagingFenceValue[0] = resident.PagingFenceValue;
    }
    trace_ddi("MakePresentSurfacesResident", (UINT)status, resident.NumAllocations);
    return status;
}

static D3DKMT_HANDLE APIENTRY check_present_allocation(D3D12DDI_HDEVICE device, D3D12DDI_HRESOURCE resource)
{
    const DdiHeap *surface = resource_heap(venus_device(device), resource);
    const D3DKMT_HANDLE allocation = surface ? surface->present.allocation : 0;
    trace_ddi("CheckResourceAllocationHandle", 0, allocation);
    return allocation;
}

static HRESULT write_present_allocation(const DdiDevice *device, const DdiPresentSurface *surface)
{
    if (!surface->ready)
        return DXGI_ERROR_INVALID_CALL;
    if (!device->kernel || !device->kernel->pfnEscapeCb || !device->adapter->runtime.handle)
        return DXGI_ERROR_UNSUPPORTED;

    /* Same WDDM pixel handoff as the Mesa UMD. An opened allocation's
       compositor can read these pixels even when VidMm never maps its pages. */
    struct {
        USHORT type;
        USHORT length;
        D3DKMT_HANDLE allocation;
        UINT64 address;
        UINT64 size;
    } pixels = {
        .type = 0x904, /* CopyIn and mark the allocation as a present source. */
        .length = sizeof(pixels) - 2 * sizeof(USHORT),
        .allocation = surface->allocation,
        .address = (UINT64)(UINT_PTR)surface->pixels.mapped,
        .size = surface->pixels.size,
    };
    _Static_assert(sizeof(pixels) == 24, "WDDM pixel escape layout");
    const D3DDDICB_ESCAPE escape = {
        .hDevice = device->runtime.handle,
        .pPrivateDriverData = &pixels,
        .PrivateDriverDataSize = sizeof(pixels),
    };
    return device->kernel->pfnEscapeCb(device->adapter->runtime.handle, &escape);
}

static UINT APIENTRY present_private_data_size(D3D12DDI_HDEVICE device, const D3D12DDIARG_PRESENT_0001 *args)
{
    (void)device;
    (void)args;
    return 0;
}

static void APIENTRY present_surface(D3D12DDI_HCOMMANDLIST commands, D3D12DDI_HCOMMANDQUEUE handle,
                                     const D3D12DDIARG_PRESENT_0001 *args, D3D12DDI_PRESENT_0028 *output)
{
    (void)commands;
    DdiCommandQueue *queue = handle.pDrvPrivate;
    *output = (D3D12DDI_PRESENT_0028){0};
    if (!queue || !queue->device)
        return;
    DdiDevice *device = queue->device;
    const DdiHeap *src = args && args->SurfacesToPresent == 1 && args->phSurfacesToPresent &&
        !args->phSurfacesToPresent->SubResourceIndex
        ? resource_heap(&device->venus, args->phSurfacesToPresent->hSurface) : NULL;
    const DdiHeap *dst = args && args->hDstResource.pDrvPrivate && !args->DstSubResourceIndex
        ? resource_heap(&device->venus, args->hDstResource) : NULL;
    if (!queue->context || !src || !src->present.allocation ||
        (args->hDstResource.pDrvPrivate && (!dst || !dst->present.allocation))) {
        set_command_device_error(device, DXGI_ERROR_UNSUPPORTED);
        return;
    }

    AcquireSRWLockExclusive(&device->queue_lock);
    const HRESULT status = write_present_allocation(device, &src->present);
    ReleaseSRWLockExclusive(&device->queue_lock);
    trace_ddi("Present Windows allocation", (UINT)status, src->present.allocation);
    if (FAILED(status)) {
        set_command_device_error(device, status);
        return;
    }
    output->hContext = queue->context;
    output->hSrcAllocation = src->present.allocation;
    output->hDstAllocation = dst ? dst->present.allocation : 0;
    output->BackBufferMultiplicity = 1;
}
