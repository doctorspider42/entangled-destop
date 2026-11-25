/* SPDX-License-Identifier: Apache-2.0 */
static HRESULT allocate_present_surface(const DdiDevice *device, D3D12DDI_HRTRESOURCE resource,
                                        const VenusColorImage *image, DdiPresentSurface *surface)
{
    if (!device->callbacks || !device->callbacks->pfnAllocateCb || !device->callbacks->pfnDeallocateCb ||
        !resource.handle || image->format != VK_FORMAT_B8G8R8A8_UNORM)
        return DXGI_ERROR_UNSUPPORTED;

    const UINT64 bytes = (UINT64)image->extent.width * image->extent.height * 4;
    HRESULT status = create_venus_transfer_buffer(image->device, bytes, VENUS_BUFFER_READBACK,
                                                  VK_BUFFER_USAGE_TRANSFER_DST_BIT, &surface->pixels);
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
        surface->prefer_gpu = enabled_environment_flag("ENTANGLED_D3D12_GPU_SHARE_ONLY");
    } else
        destroy_venus_transfer_buffer(&surface->pixels);
    trace_ddi("AllocatePresentSurface", (UINT)status, info.hAllocation);
    return status;
}

static void deallocate_present_surface(const DdiDevice *device, DdiPresentSurface *surface)
{
    if (surface->mirror.image)
        surface->destroy_mirror(device->venus.handle, &surface->mirror);
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

static HRESULT present_escape(const DdiDevice *device, void *data, UINT size)
{
    if (!device->kernel || !device->kernel->pfnEscapeCb || !device->adapter->runtime.handle)
        return DXGI_ERROR_UNSUPPORTED;
    const D3DDDICB_ESCAPE escape = {
        .hDevice = device->runtime.handle, .pPrivateDriverData = data, .PrivateDriverDataSize = size,
    };
    return device->kernel->pfnEscapeCb(device->adapter->runtime.handle, &escape);
}

static HRESULT write_present_allocation(const DdiDevice *device, DdiHeap *source)
{
    DdiPresentSurface *surface = &source->present;
    if (!surface->ready)
        return DXGI_ERROR_INVALID_CALL;
    if (!surface->cpu_ready) {
        VenusTransferList list;
        HRESULT status = begin_venus_transfer_list(&device->venus, &list);
        if (FAILED(status))
            return status;
        status = copy_venus_color_to_buffer(list.command, &source->image, &surface->pixels, 0,
                                           source->image.extent.width * 4);
        if (SUCCEEDED(status))
            status = close_venus_transfer_list(&list);
        if (SUCCEEDED(status))
            status = submit_venus_transfer_commands(&device->venus, 1, &list.command);
        destroy_venus_transfer_list(&list);
        if (FAILED(status))
            return status;
        surface->cpu_ready = TRUE;
    }

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
    return present_escape(device, &pixels, sizeof(pixels));
}

static HRESULT create_present_mirror(const DdiDevice *device, const VenusColorImage *image, DdiPresentSurface *surface)
{
    const HMODULE icd = GetModuleHandleW(L"vulkan_virtio.dll");
    if (!icd)
        return DXGI_ERROR_UNSUPPORTED;
    const PFN_entangled_create_shared_image create = (PFN_entangled_create_shared_image)(void *)
        GetProcAddress(icd, "entangled_create_shared_image");
    surface->destroy_mirror = (PFN_entangled_destroy_shared_image)(void *)
        GetProcAddress(icd, "entangled_destroy_shared_image");
    if (!create || !surface->destroy_mirror)
        return DXGI_ERROR_UNSUPPORTED;

    const VkExtent2D extent = {image->extent.width, image->extent.height};
    HRESULT status = venus_hresult(create(device->venus.handle, extent, NULL, &surface->mirror));
    if (FAILED(status))
        return status;
    ENTANGLED_WDDM_BLOB_ESCAPE bind = {
        .Type = ENTANGLED_WDDM_BIND_BLOB,
        .Version = ENTANGLED_WDDM_SHARE_VERSION,
        .Allocation = surface->allocation,
        .Token = (UINT64)(UINT_PTR)surface->mirror.token,
    };
    status = present_escape(device, &bind, sizeof(bind));
    if (status != S_OK)
        surface->destroy_mirror(device->venus.handle, &surface->mirror);
    return status;
}

static HRESULT copy_present_mirror(const VenusColorImage *source, VkImage mirror, BOOL first)
{
    const VenusDevice *device = source->device;
    const PFN_vkCmdCopyImage copy = (PFN_vkCmdCopyImage)device->proc_addr(device->handle, "vkCmdCopyImage");
    const PFN_vkCmdPipelineBarrier barrier = (PFN_vkCmdPipelineBarrier)device->proc_addr(device->handle,
                                                                                       "vkCmdPipelineBarrier");
    if (!copy || !barrier)
        return DXGI_ERROR_UNSUPPORTED;
    VenusTransferList list;
    HRESULT status = begin_venus_transfer_list(device, &list);
    if (FAILED(status))
        return status;
    status = record_venus_color_barrier(list.command, source, VK_ACCESS_MEMORY_WRITE_BIT, VK_ACCESS_TRANSFER_READ_BIT);
    if (SUCCEEDED(status)) {
        const VkImageMemoryBarrier acquire = {
            .sType = VK_STRUCTURE_TYPE_IMAGE_MEMORY_BARRIER,
            .dstAccessMask = VK_ACCESS_TRANSFER_WRITE_BIT,
            .oldLayout = first ? VK_IMAGE_LAYOUT_UNDEFINED : VK_IMAGE_LAYOUT_GENERAL,
            .newLayout = VK_IMAGE_LAYOUT_TRANSFER_DST_OPTIMAL,
            .srcQueueFamilyIndex = first ? VK_QUEUE_FAMILY_IGNORED : VK_QUEUE_FAMILY_EXTERNAL,
            .dstQueueFamilyIndex = first ? VK_QUEUE_FAMILY_IGNORED : device->graphics_queue_family,
            .image = mirror,
            .subresourceRange = {VK_IMAGE_ASPECT_COLOR_BIT, 0, 1, 0, 1},
        };
        barrier(list.command, VK_PIPELINE_STAGE_TOP_OF_PIPE_BIT, VK_PIPELINE_STAGE_TRANSFER_BIT, 0,
                0, NULL, 0, NULL, 1, &acquire);
        const VkImageCopy region = {
            .srcSubresource = {VK_IMAGE_ASPECT_COLOR_BIT, 0, 0, 1},
            .dstSubresource = {VK_IMAGE_ASPECT_COLOR_BIT, 0, 0, 1},
            .extent = source->extent,
        };
        copy(list.command, source->handle, VK_IMAGE_LAYOUT_GENERAL, mirror, VK_IMAGE_LAYOUT_TRANSFER_DST_OPTIMAL,
             1, &region);
        VkImageMemoryBarrier release = acquire;
        release.srcAccessMask = VK_ACCESS_TRANSFER_WRITE_BIT;
        release.dstAccessMask = 0;
        release.oldLayout = VK_IMAGE_LAYOUT_TRANSFER_DST_OPTIMAL;
        release.newLayout = VK_IMAGE_LAYOUT_GENERAL;
        release.srcQueueFamilyIndex = device->graphics_queue_family;
        release.dstQueueFamilyIndex = VK_QUEUE_FAMILY_EXTERNAL;
        barrier(list.command, VK_PIPELINE_STAGE_TRANSFER_BIT, VK_PIPELINE_STAGE_BOTTOM_OF_PIPE_BIT, 0,
                0, NULL, 0, NULL, 1, &release);
        status = close_venus_transfer_list(&list);
    }
    if (SUCCEEDED(status))
        status = submit_venus_transfer_commands(device, 1, &list.command);
    destroy_venus_transfer_list(&list);
    return status;
}

static HRESULT mirror_present_allocation(const DdiDevice *device, DdiHeap *source,
                                        D3DKMT_HANDLE destination, BOOL require_reader)
{
    DdiPresentSurface *surface = &source->present;
    if (surface->mirror_unavailable || (!InterlockedCompareExchange(&surface->prefer_gpu, 0, 0) &&
        !enabled_environment_flag("ENTANGLED_D3D12_GPU_SHARE")))
        return DXGI_ERROR_UNSUPPORTED;
    if (!surface->mirror.image) {
        const HRESULT opened = create_present_mirror(device, &source->image, surface);
        trace_ddi("Bind present GPU image", (UINT)opened, surface->allocation);
        if (opened != S_OK) {
            surface->mirror_unavailable = TRUE;
            InterlockedExchange(&surface->prefer_gpu, 0);
            return opened;
        }
    }
    ENTANGLED_WDDM_BLOB_ACCESS_ESCAPE access = {
        .Type = ENTANGLED_WDDM_ACCESS_BLOB,
        .Version = ENTANGLED_WDDM_SHARE_VERSION,
        .Allocation = surface->allocation,
        .Operation = require_reader ? ENTANGLED_WDDM_BEGIN_WRITE_AFTER_READ : ENTANGLED_WDDM_BEGIN_WRITE,
    };
    const HRESULT begun = present_escape(device, &access, sizeof(access));
    if (begun != S_OK) {
        trace_ddi("Begin present GPU image", (UINT)begun, surface->allocation);
        return begun;
    }
    const HRESULT copied = copy_present_mirror(&source->image, surface->mirror.image, access.Generation == 0);
    access.Operation = SUCCEEDED(copied) ? ENTANGLED_WDDM_END_ACCESS : ENTANGLED_WDDM_ABORT_ACCESS;
    const HRESULT finished = present_escape(device, &access, sizeof(access));
    trace_ddi("Publish GPU present source", (UINT)copied, surface->allocation);
    trace_ddi("Publish GPU present destination", (UINT)finished, destination);
    if (FAILED(copied) || finished != S_OK) {
        surface->mirror_unavailable = TRUE;
        if (!surface->gpu_only_used)
            InterlockedExchange(&surface->prefer_gpu, 0);
    }
    return FAILED(copied) ? copied : finished;
}

static void disable_gpu_present(const DdiDevice *device, DdiPresentSurface *surface)
{
    ENTANGLED_WDDM_BLOB_ACCESS_ESCAPE access = {
        .Type = ENTANGLED_WDDM_ACCESS_BLOB,
        .Version = ENTANGLED_WDDM_SHARE_VERSION,
        .Allocation = surface->allocation,
        .Operation = ENTANGLED_WDDM_DISABLE_GPU_ACCESS,
    };
    const HRESULT status = present_escape(device, &access, sizeof(access));
    trace_ddi("Fall back to CPU present", (UINT)status, surface->allocation);
    surface->mirror_unavailable = TRUE;
    InterlockedExchange(&surface->prefer_gpu, 0);
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
    DdiHeap *src = args && args->SurfacesToPresent == 1 && args->phSurfacesToPresent &&
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
    const LONGLONG start = queue_timing_start();
    DdiPresentSurface *surface = &src->present;
    HRESULT status = DXGI_ERROR_UNSUPPORTED;
    if (surface->ready && !dst && surface->mirror.image &&
        InterlockedCompareExchange(&surface->prefer_gpu, 0, 0))
        status = mirror_present_allocation(device, src, 0, TRUE);
    if (status == S_OK) {
        surface->gpu_only_used = TRUE;
        trace_queue_duration("present-gpu-handoff", start, 1);
        trace_ddi("GPU-only Present", 0, surface->allocation);
    } else {
        /* Keep the CPU path for blits and for allocations whose GPU read
         * has yet to be acknowledged. A failed GPU-only surface switches
         * back permanently so the opener can see new CPU writes. */
        if (surface->gpu_only_used && InterlockedCompareExchange(&surface->prefer_gpu, 0, 0))
            disable_gpu_present(device, surface);
        status = write_present_allocation(device, src);
        if (SUCCEEDED(status)) {
            trace_queue_duration("present-copy-to-wddm", start, 1);
            const HRESULT mirrored = mirror_present_allocation(device, src, dst ? dst->present.allocation : 0, FALSE);
            if (mirrored != S_OK && surface->mirror.image && !surface->gpu_only_used)
                disable_gpu_present(device, surface);
        }
    }
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
