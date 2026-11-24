/* SPDX-License-Identifier: Apache-2.0 */
typedef enum DdiHeapKind {
    DDI_HEAP_NONE,
    DDI_HEAP_TRANSFER_BUFFER,
    DDI_HEAP_COLOR_IMAGE,
} DdiHeapKind;

typedef struct DdiResource DdiResource;

typedef struct DdiHeap {
    const VenusDevice *device;
    DdiHeapKind kind;
    D3D12DDI_HEAP_FLAGS flags;
    SRWLOCK placed_lock;
    DdiResource *placed_images;
    union {
        VenusTransferBuffer buffer;
        VenusColorImage image;
    };
#ifdef ENTANGLED_D3D12_PROBE_PRESENT
    DdiPresentSurface present;
#endif
#ifdef ENTANGLED_D3D12_PROBE_SHADER
    DdiBufferAddress address;
#endif
} DdiHeap;

struct DdiResource {
    DdiHeap *heap;
    VenusTransferBuffer placed_buffer;
    VenusColorImage placed_image;
    VkDeviceSize placed_offset;
    DdiResource *next_placed_image;
};

static DdiHeap *resource_heap(const VenusDevice *device, D3D12DDI_HRESOURCE resource)
{
    const DdiResource *object = resource.pDrvPrivate;
    DdiHeap *heap = object ? object->heap : NULL;
    return device && heap && heap->device == device ? heap : NULL;
}

static VenusColorImage *resource_color(const VenusDevice *device, D3D12DDI_HRESOURCE resource)
{
    DdiHeap *heap = resource_heap(device, resource);
    if (!heap)
        return NULL;
    DdiResource *object = resource.pDrvPrivate;
    if (object->placed_image.handle)
        return &object->placed_image;
    return heap->kind == DDI_HEAP_COLOR_IMAGE ? &heap->image : NULL;
}

#ifdef ENTANGLED_D3D12_PROBE_PRESENT
#include "ddi-present.inc"
#endif

static const VenusTransferBuffer *resource_buffer(const VenusDevice *device, D3D12DDI_HRESOURCE resource)
{
    const DdiHeap *heap = resource_heap(device, resource);
    if (!heap || heap->kind != DDI_HEAP_TRANSFER_BUFFER)
        return NULL;
    const DdiResource *object = resource.pDrvPrivate;
    if (object->placed_image.handle)
        return NULL;
    return object->placed_buffer.handle ? &object->placed_buffer : &heap->buffer;
}

#ifdef ENTANGLED_D3D12_PROBE_SHADER
static void register_buffer_address(DdiDevice *device, DdiHeap *heap)
{
    if (!heap->buffer.address)
        return;
    AcquireSRWLockExclusive(&device->buffer_lock);
    heap->address = (DdiBufferAddress){ .buffer = &heap->buffer, .next = device->buffers };
    device->buffers = &heap->address;
    ReleaseSRWLockExclusive(&device->buffer_lock);
}

static void unregister_buffer_address(DdiDevice *device, const DdiHeap *heap)
{
    if (!heap->address.buffer)
        return;
    AcquireSRWLockExclusive(&device->buffer_lock);
    DdiBufferAddress **entry = &device->buffers;
    while (*entry && *entry != &heap->address)
        entry = &(*entry)->next;
    if (*entry)
        *entry = (*entry)->next;
    ReleaseSRWLockExclusive(&device->buffer_lock);
}

static BOOL resolve_buffer_address(DdiDevice *device, D3D12DDI_GPU_VIRTUAL_ADDRESS address,
                                   UINT64 size, VkDescriptorBufferInfo *binding)
{
    BOOL found = FALSE;
    AcquireSRWLockShared(&device->buffer_lock);
    for (const DdiBufferAddress *entry = device->buffers; entry; entry = entry->next) {
        const VenusTransferBuffer *buffer = entry->buffer;
        if (address < buffer->address)
            continue;
        const UINT64 offset = address - buffer->address;
        if (offset < buffer->size && size <= buffer->size - offset) {
            VkDeviceSize range = size ? size : buffer->size - offset;
            if (!size && range > device->venus.limits.maxUniformBufferRange)
                range = device->venus.limits.maxUniformBufferRange;
            *binding = (VkDescriptorBufferInfo){ .buffer = buffer->handle, .offset = buffer->offset + offset, .range = range };
            found = TRUE;
            break;
        }
    }
    ReleaseSRWLockShared(&device->buffer_lock);
    return found;
}

static void APIENTRY create_uniform_cbv(D3D12DDI_HDEVICE device, const D3D12DDI_CONSTANT_BUFFER_VIEW_DESC *view,
                                        D3D12DDI_CPU_DESCRIPTOR_HANDLE destination)
{
    DdiDescriptor *descriptor = (DdiDescriptor *)destination.ptr;
    if (!descriptor || !view || !view->BufferLocation || !view->SizeInBytes ||
        view->BufferLocation % D3D12_CONSTANT_BUFFER_DATA_PLACEMENT_ALIGNMENT ||
        view->SizeInBytes % D3D12_CONSTANT_BUFFER_DATA_PLACEMENT_ALIGNMENT ||
        view->SizeInBytes > D3D12_REQ_CONSTANT_BUFFER_ELEMENT_COUNT * 16) {
        set_command_device_error(device.pDrvPrivate, DXGI_ERROR_UNSUPPORTED);
        return;
    }
    *descriptor = (DdiDescriptor){ .kind = DDI_DESCRIPTOR_CBV, .desc.cbv = *view };
    trace_ddi("CreateConstantBufferView", 0, view->SizeInBytes);
}
#endif

static VkFormat color_image_format(DXGI_FORMAT format)
{
    switch (format) {
    case DXGI_FORMAT_R8_UNORM: return VK_FORMAT_R8_UNORM;
    case DXGI_FORMAT_R8G8B8A8_UNORM: return VK_FORMAT_R8G8B8A8_UNORM;
    case DXGI_FORMAT_B8G8R8A8_UNORM: return VK_FORMAT_B8G8R8A8_UNORM;
    default: return VK_FORMAT_UNDEFINED;
    }
}

static BOOL supported_color_image(const D3D12DDIARG_CREATERESOURCE_0003 *resource)
{
    const D3D12DDI_RESOURCE_FLAGS_0003 allowed = D3D12DDI_RESOURCE_FLAG_0003_RENDER_TARGET |
                                               D3D12DDI_RESOURCE_FLAG_0003_SHADER_RESOURCE;
    return resource->ResourceType == D3D12DDI_RT_TEXTURE2D && color_image_format(resource->Format) != VK_FORMAT_UNDEFINED &&
           resource->Width && resource->Width <= D3D12_REQ_TEXTURE2D_U_OR_V_DIMENSION &&
           resource->Height && resource->Height <= D3D12_REQ_TEXTURE2D_U_OR_V_DIMENSION &&
           resource->DepthOrArraySize == 1 && resource->MipLevels == 1 && resource->SampleDesc.Count == 1 &&
           !resource->SampleDesc.Quality && (resource->Flags & allowed) &&
           !(resource->Flags & ~allowed) && !resource->pRowMajorLayout;
}

static VkImageUsageFlags color_image_usage(const D3D12DDIARG_CREATERESOURCE_0003 *resource)
{
    VkImageUsageFlags usage = 0;
    if (resource->Flags & D3D12DDI_RESOURCE_FLAG_0003_RENDER_TARGET)
        usage |= VK_IMAGE_USAGE_COLOR_ATTACHMENT_BIT;
    if (resource->Flags & D3D12DDI_RESOURCE_FLAG_0003_SHADER_RESOURCE)
        usage |= VK_IMAGE_USAGE_SAMPLED_BIT;
    return usage;
}

static D3D12DDI_RESOURCE_ALLOCATION_INFO_0022 row_major_allocation(UINT64 size, UINT64 alignment)
{
    if (alignment < D3D12_DEFAULT_RESOURCE_PLACEMENT_ALIGNMENT)
        alignment = D3D12_DEFAULT_RESOURCE_PLACEMENT_ALIGNMENT;
    if (!size || alignment > UINT32_MAX || size > UINT64_MAX - (alignment - 1))
        return (D3D12DDI_RESOURCE_ALLOCATION_INFO_0022){ .ResourceDataSize = UINT64_MAX };
    return (D3D12DDI_RESOURCE_ALLOCATION_INFO_0022){
        .ResourceDataSize = (size + alignment - 1) & ~((UINT64)alignment - 1),
        .ResourceDataAlignment = (UINT32)alignment,
        .AdditionalDataHeaderAlignment = 1,
        .AdditionalDataAlignment = 1,
        .Layout = D3D12DDI_TL_ROW_MAJOR,
    };
}

static void APIENTRY check_resource_allocation(D3D12DDI_HDEVICE device,
                                             const D3D12DDIARG_CREATERESOURCE_0003 *resource,
                                             D3D12DDI_RESOURCE_OPTIMIZATION_FLAGS optimization,
                                             UINT32 alignment_restriction, UINT visible_node_mask,
                                             D3D12DDI_RESOURCE_ALLOCATION_INFO_0022 *info)
{
    (void)optimization;
    trace_ddi("CheckResourceAllocationInfo", resource ? resource->ResourceType : 0, resource ? resource->Width : 0);
    *info = row_major_allocation(0, 0);
    if (!device.pDrvPrivate || !resource || visible_node_mask > 1 ||
        (resource->Layout != D3D12DDI_TL_ROW_MAJOR && resource->Layout != D3D12DDI_TL_UNDEFINED))
        return;
    D3D12DDI_RESOURCE_ALLOCATION_INFO_0022 allocation = *info;
    if (resource->ResourceType == D3D12DDI_RT_BUFFER) {
        allocation = row_major_allocation(resource->Width, 0);
    } else if (supported_color_image(resource)) {
        VkMemoryRequirements backing;
        const HRESULT status = venus_color_image_requirements(venus_device(device), color_image_format(resource->Format),
                                                              (UINT32)resource->Width, resource->Height,
                                                              color_image_usage(resource), &backing);
        if (SUCCEEDED(status))
            allocation = row_major_allocation(backing.size, backing.alignment);
        trace_ddi("Color image allocation info", (UINT)status, allocation.ResourceDataSize);
    }
    if (!alignment_restriction || alignment_restriction >= allocation.ResourceDataAlignment)
        *info = allocation;
    if (resource->ResourceType == D3D12DDI_RT_BUFFER)
        trace_ddi("Buffer allocation alignment", alignment_restriction, info->ResourceDataAlignment);
}

static void APIENTRY check_existing_resource_allocation(D3D12DDI_HDEVICE device, D3D12DDI_HRESOURCE resource,
                                                      D3D12DDI_RESOURCE_ALLOCATION_INFO_0022 *info)
{
    const VenusColorImage *image = resource_color(venus_device(device), resource);
    if (image)
        *info = row_major_allocation(image->requirements.size, image->requirements.alignment);
    else {
        const VenusTransferBuffer *buffer = resource_buffer(venus_device(device), resource);
        *info = row_major_allocation(buffer ? buffer->size : 0, 0);
    }
    trace_ddi("CheckExistingResourceAllocationInfo", 0, info->ResourceDataSize);
}

static D3D12DDI_GPU_VIRTUAL_ADDRESS APIENTRY check_buffer_address(D3D12DDI_HDEVICE device, D3D12DDI_HRESOURCE resource)
{
    const VenusTransferBuffer *buffer = resource_buffer(venus_device(device), resource);
    const VkDeviceAddress address = buffer ? buffer->address : 0;
    trace_ddi("CheckResourceVirtualAddress", 0, address != 0);
    return address;
}

static void APIENTRY check_resource_subresource(D3D12DDI_HDEVICE device, D3D12DDI_HRESOURCE resource, UINT subresource,
                                             D3D12DDI_SUBRESOURCE_INFO *info)
{
    *info = (D3D12DDI_SUBRESOURCE_INFO){0};
    if (!subresource) {
        const VenusTransferBuffer *buffer = resource_buffer(venus_device(device), resource);
        const VenusColorImage *image = resource_color(venus_device(device), resource);
        if (buffer)
            *info = (D3D12DDI_SUBRESOURCE_INFO){ .RowStride = buffer->size, .DepthStride = buffer->size };
        else if (image)
            *info = (D3D12DDI_SUBRESOURCE_INFO){ .RowStride = image->row_pitch,
                .DepthStride = (UINT64)image->row_pitch * image->extent.height };
    }
    trace_ddi("CheckSubresourceInfo", subresource, info->RowStride);
}

static D3D12DDI_HEAP_AND_RESOURCE_SIZES APIENTRY private_resource_sizes(
    D3D12DDI_HDEVICE device, const D3D12DDIARG_CREATEHEAP_0001 *heap,
    const D3D12DDIARG_CREATERESOURCE_0003 *resource, D3D12DDI_HPROTECTEDRESOURCESESSION_0030 protection)
{
    (void)device;
    (void)protection;
    return (D3D12DDI_HEAP_AND_RESOURCE_SIZES){
        .Heap = heap ? sizeof(DdiHeap) : 0,
        .Resource = resource ? sizeof(DdiResource) : 0,
    };
}

static BOOL supported_transfer_buffer(const D3D12DDIARG_CREATERESOURCE_0003 *resource)
{
    const D3D12DDI_RESOURCE_FLAGS_0003 supported_flags = D3D12DDI_RESOURCE_FLAG_0003_SHADER_RESOURCE |
        D3D12DDI_RESOURCE_FLAG_0022_UNORDERED_ACCESS | D3D12DDI_RESOURCE_FLAG_0003_SIMULTANEOUS_ACCESS;
    return resource->ResourceType == D3D12DDI_RT_BUFFER && resource->Width &&
           !(resource->Flags & ~supported_flags) && resource->Layout == D3D12DDI_TL_ROW_MAJOR;
}

static HRESULT create_transfer_backing(DdiDevice *owner, const D3D12DDIARG_CREATEHEAP_0001 *heap,
                                       const D3D12DDIARG_CREATERESOURCE_0003 *resource, VenusTransferBuffer *buffer)
{
    if (!supported_transfer_buffer(resource))
        return DXGI_ERROR_UNSUPPORTED;

    VenusBufferMemory placement;
    if (heap->MemoryPool == D3D12DDI_MEMORY_POOL_L1 &&
        heap->CPUPageProperty == D3D12DDI_CPU_PAGE_PROPERTY_NOT_AVAILABLE)
        placement = VENUS_BUFFER_DEVICE_LOCAL;
    else if (heap->MemoryPool == D3D12DDI_MEMORY_POOL_L0 &&
             heap->CPUPageProperty == D3D12DDI_CPU_PAGE_PROPERTY_WRITE_BACK)
        placement = VENUS_BUFFER_READBACK;
    else if (heap->MemoryPool == D3D12DDI_MEMORY_POOL_L0 &&
             heap->CPUPageProperty == D3D12DDI_CPU_PAGE_PROPERTY_WRITE_COMBINE)
        placement = VENUS_BUFFER_UPLOAD;
    else
        return DXGI_ERROR_UNSUPPORTED;

    if (heap->ByteSize && heap->ByteSize < resource->Width)
        return E_INVALIDARG;

    VkBufferUsageFlags uses = VK_BUFFER_USAGE_VERTEX_BUFFER_BIT | VK_BUFFER_USAGE_INDEX_BUFFER_BIT |
                             VK_BUFFER_USAGE_UNIFORM_BUFFER_BIT | VK_BUFFER_USAGE_INDIRECT_BUFFER_BIT;
    if (resource->Flags & D3D12DDI_RESOURCE_FLAG_0003_SHADER_RESOURCE)
        uses |= VK_BUFFER_USAGE_UNIFORM_TEXEL_BUFFER_BIT | VK_BUFFER_USAGE_STORAGE_BUFFER_BIT;
    if (resource->Flags & D3D12DDI_RESOURCE_FLAG_0022_UNORDERED_ACCESS)
        uses |= VK_BUFFER_USAGE_STORAGE_TEXEL_BUFFER_BIT | VK_BUFFER_USAGE_STORAGE_BUFFER_BIT;
    HRESULT status = create_venus_transfer_buffer(&owner->venus, resource->Width, placement, uses, buffer);
    if (SUCCEEDED(status) && placement == VENUS_BUFFER_DEVICE_LOCAL) {
        /* Vulkan does not apply D3D12 default heap zero-initialization. */
        AcquireSRWLockExclusive(&owner->queue_lock);
        status = zero_venus_default_buffer(buffer);
        ReleaseSRWLockExclusive(&owner->queue_lock);
        if (FAILED(status))
            destroy_venus_transfer_buffer(buffer);
        trace_ddi("ZeroDefaultBuffer", (UINT)status, resource->Width);
    }
    return status;
}

static HRESULT create_placed_buffer(const VenusDevice *venus, const D3D12DDIARG_CREATERESOURCE_0003 *resource,
                                    DdiResource *object)
{
    if (!object || !supported_transfer_buffer(resource))
        return DXGI_ERROR_UNSUPPORTED;
    const D3D12DDIARG_HRESOURCE_PLACEMENT base = resource->ReuseBufferGPUVA.BaseAddress.UMD;
    DdiHeap *heap = resource_heap(venus, base.hResource);
    const VenusTransferBuffer *storage = resource_buffer(venus, base.hResource);
    if (!storage || !storage->address || base.Offset > storage->size || resource->Width > storage->size - base.Offset)
        return E_INVALIDARG;
    *object = (DdiResource){
        .heap = heap,
        .placed_buffer = {
            .device = venus,
            .handle = storage->handle,
            .size = resource->Width,
            .offset = storage->offset + base.Offset,
            .address = storage->address + base.Offset,
            .usage = storage->usage,
        },
    };
    trace_ddi("CreatePlacedBuffer", 0, object->placed_buffer.offset);
    return S_OK;
}

static HRESULT initialize_color_image(DdiDevice *device, VenusColorImage *image)
{
    AcquireSRWLockExclusive(&device->queue_lock);
    const HRESULT status = zero_venus_color_image(image);
    ReleaseSRWLockExclusive(&device->queue_lock);
    if (FAILED(status))
        destroy_venus_color_image(image);
    return status;
}

static BOOL placed_image_overlaps(const DdiHeap *heap, VkDeviceSize offset, VkDeviceSize size)
{
    for (const DdiResource *other = heap->placed_images; other; other = other->next_placed_image) {
        const VkDeviceSize end = other->placed_offset + other->placed_image.requirements.size;
        if (offset < end && offset + size > other->placed_offset)
            return TRUE;
    }
    return FALSE;
}

static HRESULT create_placed_image(DdiDevice *device, const D3D12DDIARG_CREATERESOURCE_0003 *resource,
                                   DdiResource *object)
{
    const VenusDevice *venus = &device->venus;
    const D3D12DDIARG_HRESOURCE_PLACEMENT base = resource->ReuseBufferGPUVA.BaseAddress.UMD;
    trace_ddi("CreatePlacedTexture flags", resource->Flags, resource->InitialResourceState);
    trace_ddi("CreatePlacedTexture layout", resource->Layout, base.Offset);
    if (!object || !supported_color_image(resource) ||
        (resource->Layout != D3D12DDI_TL_ROW_MAJOR && resource->Layout != D3D12DDI_TL_UNDEFINED))
        return DXGI_ERROR_UNSUPPORTED;

    const VkImageUsageFlags usage = color_image_usage(resource);
    const D3D12DDI_HEAP_FLAGS required = usage & VK_IMAGE_USAGE_COLOR_ATTACHMENT_BIT
        ? D3D12DDI_HEAP_FLAG_RT_DS_TEXTURES : D3D12DDI_HEAP_FLAG_NON_RT_DS_TEXTURES;
    DdiHeap *heap = resource_heap(venus, base.hResource);
    const VenusTransferBuffer *storage = resource_buffer(venus, base.hResource);
    if (!storage || storage != &heap->buffer || !storage->memory || storage->mapped ||
        !(heap->flags & required) || (heap->flags & D3D12DDI_HEAP_FLAG_BUFFERS))
        return DXGI_ERROR_UNSUPPORTED;
    if (base.Offset % D3D12_DEFAULT_RESOURCE_PLACEMENT_ALIGNMENT)
        return E_INVALIDARG;

    *object = (DdiResource){0};
    AcquireSRWLockExclusive(&heap->placed_lock);
    HRESULT status = create_venus_placed_color_image(storage, base.Offset, color_image_format(resource->Format),
        (UINT32)resource->Width, resource->Height, usage, &object->placed_image);
    if (SUCCEEDED(status) && placed_image_overlaps(heap, base.Offset, object->placed_image.requirements.size)) {
        destroy_venus_color_image(&object->placed_image);
        status = DXGI_ERROR_UNSUPPORTED;
    }
    if (SUCCEEDED(status))
        status = initialize_color_image(device, &object->placed_image);
    if (SUCCEEDED(status)) {
        object->heap = heap;
        object->placed_offset = base.Offset;
        object->next_placed_image = heap->placed_images;
        heap->placed_images = object;
    }
    ReleaseSRWLockExclusive(&heap->placed_lock);
    trace_ddi("CreatePlacedTexture", (UINT)status, base.Offset);
    return status;
}

static HRESULT APIENTRY create_heap_and_resource(
    D3D12DDI_HDEVICE device, const D3D12DDIARG_CREATEHEAP_0001 *heap, D3D12DDI_HHEAP driver_heap,
    D3D12DDI_HRTRESOURCE runtime_resource, const D3D12DDIARG_CREATERESOURCE_0003 *resource,
    const D3D12DDI_CLEAR_VALUES *clear, D3D12DDI_HPROTECTEDRESOURCESESSION_0030 protection,
    D3D12DDI_HRESOURCE driver_resource)
{
    const VenusDevice *venus = venus_device(device);
    DdiHeap *backing = driver_heap.pDrvPrivate;
    DdiResource *object = driver_resource.pDrvPrivate;
    if (!heap && !backing && resource && venus && !protection.pDrvPrivate && !clear &&
        resource->ReuseBufferGPUVA.BaseAddress.UMD.hResource.pDrvPrivate) {
        return resource->ResourceType == D3D12DDI_RT_BUFFER ? create_placed_buffer(venus, resource, object)
            : create_placed_image(device.pDrvPrivate, resource, object);
    }
    trace_ddi("CreateHeapAndResource arguments", (heap != NULL) | (backing != NULL) << 1 |
              (object != NULL) << 2 | (clear != NULL) << 3, resource ? resource->ResourceType : 0);
    if (heap && resource) {
        trace_ddi("CreateHeapAndResource flags", resource->Flags, heap->Flags);
        trace_ddi("CreateHeapAndResource memory", heap->MemoryPool, heap->CPUPageProperty);
        trace_ddi("CreateHeapAndResource layout", resource->Layout, resource->pRowMajorLayout != NULL);
    }
    if (!heap || !resource || protection.pDrvPrivate || clear ||
        heap->CreationNodeMask > 1 || heap->VisibleNodeMask > 1 ||
        resource->ReuseBufferGPUVA.BaseAddress.UMD.hResource.pDrvPrivate)
        return DXGI_ERROR_UNSUPPORTED;
    if (!venus || !backing || !object || !resource->Width)
        return E_INVALIDARG;

    const BOOL primary = (heap->Flags & D3D12DDI_HEAP_FLAG_PRIMARY) != 0;
#ifndef ENTANGLED_D3D12_PROBE_PRESENT
    (void)runtime_resource;
    if (primary)
        return DXGI_ERROR_UNSUPPORTED;
#endif
    *backing = (DdiHeap){0};
    *object = (DdiResource){0};
    HRESULT status = DXGI_ERROR_UNSUPPORTED;
    if (resource->ResourceType == D3D12DDI_RT_BUFFER && !primary) {
        status = create_transfer_backing(device.pDrvPrivate, heap, resource, &backing->buffer);
        if (SUCCEEDED(status)) {
            backing->kind = DDI_HEAP_TRANSFER_BUFFER;
#ifdef ENTANGLED_D3D12_PROBE_SHADER
            register_buffer_address(device.pDrvPrivate, backing);
#endif
        }
    } else if (supported_color_image(resource) && heap->MemoryPool == D3D12DDI_MEMORY_POOL_L1 &&
               heap->CPUPageProperty == D3D12DDI_CPU_PAGE_PROPERTY_NOT_AVAILABLE &&
               resource->Layout == D3D12DDI_TL_ROW_MAJOR) {
        const VkImageUsageFlags usage = color_image_usage(resource);
        if (!primary || (usage & VK_IMAGE_USAGE_COLOR_ATTACHMENT_BIT))
            status = create_venus_color_image(venus, color_image_format(resource->Format), (UINT32)resource->Width,
                                             resource->Height, usage, &backing->image);
        if (SUCCEEDED(status)) {
            if (heap->ByteSize && heap->ByteSize < backing->image.requirements.size) {
                destroy_venus_color_image(&backing->image);
                status = E_INVALIDARG;
            } else
                status = initialize_color_image(device.pDrvPrivate, &backing->image);
            if (SUCCEEDED(status))
                backing->kind = DDI_HEAP_COLOR_IMAGE;
        }
    }
    if (SUCCEEDED(status)) {
#ifdef ENTANGLED_D3D12_PROBE_PRESENT
        if (primary) {
            status = allocate_present_surface(device.pDrvPrivate, runtime_resource, &backing->image, &backing->present);
            if (FAILED(status)) {
                destroy_venus_color_image(&backing->image);
                *backing = (DdiHeap){0};
                return status;
            }
        }
#endif
        backing->device = venus;
        backing->flags = heap->Flags;
        object->heap = backing;
    }
    trace_ddi("CreateHeapAndResource result", (UINT)status, resource->Width);
    return status;
}

static void APIENTRY destroy_heap_and_resource(D3D12DDI_HDEVICE device, D3D12DDI_HHEAP handle,
                                               D3D12DDI_HRESOURCE resource)
{
    DdiResource *object = resource.pDrvPrivate;
    if (object) {
        const BOOL placed = object->placed_buffer.handle || object->placed_image.handle;
        if (object->placed_image.handle) {
            DdiHeap *backing = object->heap;
            AcquireSRWLockExclusive(&backing->placed_lock);
            DdiResource **entry = &backing->placed_images;
            while (*entry && *entry != object)
                entry = &(*entry)->next_placed_image;
            if (*entry)
                *entry = object->next_placed_image;
            destroy_venus_color_image(&object->placed_image);
            ReleaseSRWLockExclusive(&backing->placed_lock);
        }
        *object = (DdiResource){0};
        if (placed)
            return;
    }
    DdiHeap *heap = handle.pDrvPrivate;
    if (!heap || !heap->device || heap->device != venus_device(device))
        return;
#ifdef ENTANGLED_D3D12_PROBE_PRESENT
    if (heap->present.allocation)
        deallocate_present_surface(device.pDrvPrivate, &heap->present);
#endif
    if (heap->kind == DDI_HEAP_TRANSFER_BUFFER) {
#ifdef ENTANGLED_D3D12_PROBE_SHADER
        unregister_buffer_address(device.pDrvPrivate, heap);
#endif
        while (heap->placed_images) {
            DdiResource *placed = heap->placed_images;
            heap->placed_images = placed->next_placed_image;
            destroy_venus_color_image(&placed->placed_image);
            *placed = (DdiResource){0};
        }
        destroy_venus_transfer_buffer(&heap->buffer);
    }
    else if (heap->kind == DDI_HEAP_COLOR_IMAGE)
        destroy_venus_color_image(&heap->image);
    *heap = (DdiHeap){0};
}

static HRESULT APIENTRY map_buffer_heap(D3D12DDI_HDEVICE device, D3D12DDI_HHEAP handle, void **data)
{
    const DdiHeap *heap = handle.pDrvPrivate;
    if (!heap || heap->device != venus_device(device) || !data)
        return E_INVALIDARG;
    if (heap->kind != DDI_HEAP_TRANSFER_BUFFER || !heap->buffer.mapped)
        return DXGI_ERROR_UNSUPPORTED;
    *data = heap->buffer.mapped;
    trace_ddi("MapHeap", 0, heap->buffer.size);
    return S_OK;
}

static void APIENTRY unmap_buffer_heap(D3D12DDI_HDEVICE device, D3D12DDI_HHEAP heap)
{
    /* The coherent Vulkan mapping lives as long as the backing allocation. */
    (void)device;
    (void)heap;
}

static const DdiHeap *pageable_heap(D3D12DDI_HANDLE_AND_TYPE object)
{
    if (object.Type == D3D12DDI_HT_HEAP)
        return object.Handle;
    if (object.Type == D3D12DDI_HT_0012_RESOURCE && object.Handle)
        return ((const DdiResource *)object.Handle)->heap;
    return NULL;
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
    if (object.Type != D3D12DDI_HT_HEAP && object.Type != D3D12DDI_HT_0012_RESOURCE)
        return DXGI_ERROR_UNSUPPORTED;
    const DdiHeap *heap = pageable_heap(object);
    if (!heap || heap->device != device)
        return E_INVALIDARG;
    if (heap->kind == DDI_HEAP_TRANSFER_BUFFER && heap->buffer.memory && heap->buffer.handle)
        return S_OK;
    if (heap->kind == DDI_HEAP_COLOR_IMAGE && heap->image.memory && heap->image.handle)
        return S_OK;
    return E_INVALIDARG;
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
#ifdef ENTANGLED_D3D12_PROBE_PRESENT
    D3DKMT_HANDLE allocations[8];
    UINT count = 0;
    for (UINT i = 0; i < args->NumObjects; ++i) {
        const DdiHeap *heap = pageable_heap(args->pObjects[i]);
        if (heap && heap->present.allocation) {
            if (count == _countof(allocations))
                return DXGI_ERROR_UNSUPPORTED;
            allocations[count++] = heap->present.allocation;
        }
    }
    if (count)
        return make_present_surfaces_resident(handle.pDrvPrivate, args, allocations, count);
#endif
    return S_OK;
}

static void APIENTRY get_debug_allocation_info(D3D12DDI_HDEVICE device, D3D12DDI_HANDLE_AND_TYPE object,
                                              UINT *num_addresses,
                                              D3D12DDI_DEBUG_VIRTUAL_ADDRESS_ALLOCATION_INFO_0012 *addresses,
                                              UINT *num_allocations, D3D12DDI_DEBUG_KMT_ALLOCATION_INFO_0014 *allocations)
{
    (void)device;
    (void)addresses;
#ifdef ENTANGLED_D3D12_PROBE_PRESENT
    const UINT capacity = allocations ? *num_allocations : 0;
#else
    (void)allocations;
#endif
    *num_addresses = 0;
    *num_allocations = 0;
#ifdef ENTANGLED_D3D12_PROBE_PRESENT
    const DdiHeap *heap = pageable_heap(object);
    if (heap && heap->present.allocation && heap->device == venus_device(device)) {
        if (capacity) {
            *allocations = (D3D12DDI_DEBUG_KMT_ALLOCATION_INFO_0014){ .hAllocation = heap->present.allocation,
                .Size = (UINT64)heap->image.extent.width * heap->image.extent.height * 4 };
        }
        *num_allocations = 1;
    }
#endif
    trace_ddi("GetDebugAllocationInfo", object.Type, 0);
}

#ifdef ENTANGLED_D3D12_PROBE_SHADER
static HRESULT bind_graphics_resources(DdiTransferList *list);
#endif
#include "ddi-color.inc"
#ifdef ENTANGLED_D3D12_PROBE_SHADER
#include "ddi-compute-command.inc"
#include "ddi-graphics-command.inc"
#include "ddi-timestamp.inc"
#endif

#ifdef ENTANGLED_D3D12_PROBE_RUNTIME
static D3D12DDI_HEAP_AND_RESOURCE_SIZES APIENTRY private_opened_transfer_sizes(
    D3D12DDI_HDEVICE device, const D3D12DDIARG_OPENHEAP_0003 *args)
{
    (void)device;
    (void)args;
    return (D3D12DDI_HEAP_AND_RESOURCE_SIZES){0};
}

static HRESULT APIENTRY open_transfer_heap(D3D12DDI_HDEVICE device, const D3D12DDIARG_OPENHEAP_0003 *args,
                                          D3D12DDI_HHEAP heap, D3D12DDI_HRTRESOURCE runtime, D3D12DDI_HRESOURCE resource)
{
    (void)device;
    (void)args;
    (void)heap;
    (void)runtime;
    (void)resource;
    return DXGI_ERROR_UNSUPPORTED;
}

static void APIENTRY copy_transfer_tiles(D3D12DDI_HCOMMANDLIST list, D3D12DDI_HRESOURCE resource,
                                         const D3D12DDI_TILED_RESOURCE_COORDINATE *start,
                                         const D3D12DDI_TILE_REGION_SIZE *region, D3D12DDI_HRESOURCE buffer,
                                         UINT64 offset, D3D12DDI_TILE_COPY_FLAGS flags)
{
    (void)resource;
    (void)start;
    (void)region;
    (void)buffer;
    (void)offset;
    (void)flags;
    require_transfer_state(list, FALSE);
}

static void APIENTRY check_transfer_format(D3D12DDI_HDEVICE device, DXGI_FORMAT format, UINT *support)
{
    (void)device;
    /* The pilot leaves the optional format capabilities disabled. */
    *support = format == DXGI_FORMAT_R10G10B10_XR_BIAS_A2_UNORM ? D3D12DDI_FORMAT_SUPPORT_NOT_SUPPORTED : 0;
}

static void APIENTRY check_transfer_samples(D3D12DDI_HDEVICE device, DXGI_FORMAT format, UINT samples,
                                            D3D12DDI_MULTISAMPLE_QUALITY_LEVEL_FLAGS flags, UINT *levels)
{
    (void)device;
    (void)format;
    (void)flags;
    *levels = samples == 1 ? 1 : 0;
}
#endif

static const D3D12DDI_DEVICE_FUNCS_CORE_0040 entangled_device_core_ddi = {
#ifdef ENTANGLED_D3D12_PROBE_RUNTIME
    .pfnCheckFormatSupport = check_transfer_format,
    .pfnCheckMultisampleQualityLevels = check_transfer_samples,
    .pfnCalcPrivateOpenedHeapAndResourceSizes = private_opened_transfer_sizes,
    .pfnOpenHeapAndResource = open_transfer_heap,
#endif
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
    .pfnCreatePixelShader = create_pixel_shader,
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
    .pfnCreateRenderTargetView = create_color_rtv,
    .pfnCalcPrivateCommandSignatureSize = private_command_signature_size,
    .pfnCreateCommandSignature = create_command_signature,
    .pfnDestroyCommandSignature = destroy_command_signature,
    .pfnCopyDescriptors = copy_descriptors,
    .pfnCopyDescriptorsSimple = copy_descriptors_simple,
#ifdef ENTANGLED_D3D12_PROBE_SHADER
    .pfnCalcPrivateElementLayoutSize = private_vertex_layout_size,
    .pfnCreateElementLayout = create_vertex_layout,
    .pfnDestroyElementLayout = destroy_vertex_layout,
    .pfnCreateConstantBufferView = create_uniform_cbv,
    .pfnCreateShaderResourceView = create_shader_srv,
    .pfnCreateUnorderedAccessView = create_storage_uav,
    .pfnCalcPrivateQueryHeapSize = private_timestamp_heap_size,
    .pfnCreateQueryHeap = create_timestamp_heap,
    .pfnDestroyQueryHeap = destroy_timestamp_heap,
    .pfnCreateSampler = create_pixel_sampler,
#endif
    .pfnCalcPrivateHeapAndResourceSizes = private_resource_sizes,
    .pfnCreateHeapAndResource = create_heap_and_resource,
    .pfnDestroyHeapAndResource = destroy_heap_and_resource,
    .pfnMapHeap = map_buffer_heap,
    .pfnUnmapHeap = unmap_buffer_heap,
    .pfnCheckResourceAllocationInfo = check_resource_allocation,
    .pfnCheckSubresourceInfo = check_resource_subresource,
    .pfnCheckExistingResourceAllocationInfo = check_existing_resource_allocation,
    .pfnCheckResourceVirtualAddress = check_buffer_address,
#ifdef ENTANGLED_D3D12_PROBE_PRESENT
    .pfnGetPresentPrivateDriverDataSize = present_private_data_size,
    .pfnCheckResourceAllocationHandle = check_present_allocation,
#endif
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
    const VenusTransferBuffer *source = resource_buffer(transfer->venus.device, src.BaseAddress.UMD.hResource);
    const VenusTransferBuffer *target = resource_buffer(transfer->venus.device, dst.BaseAddress.UMD.hResource);
    if (!source || !target) {
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
        ? record_venus_buffer_copy(transfer->venus.command, target, source, region)
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
    .pfnClearRenderTargetView = clear_color_rtv,
    .pfnResourceBarrier = transfer_resource_barriers,
    .pfnExecuteIndirect = execute_unsupported_indirect,
    .pfnCopyTextureRegion = copy_color_texture_region,
#ifdef ENTANGLED_D3D12_PROBE_PRESENT
    .pfnPresent = present_surface,
#endif
#ifdef ENTANGLED_D3D12_PROBE_RUNTIME
    .pfnCopyTiles = copy_transfer_tiles,
#endif
    .pfnSetPredication = set_transfer_predication,
    .pfnIaSetTopology = set_transfer_topology,
    .pfnRsSetViewports = set_transfer_viewports,
    .pfnRsSetScissorRects = set_transfer_scissors,
    .pfnOmSetBlendFactor = set_transfer_blend_factor,
    .pfnOmSetStencilRef = set_transfer_stencil_reference,
    .pfnSetDescriptorHeaps = set_transfer_descriptor_heaps,
    .pfnSetGraphicsRootSignature = set_transfer_graphics_root,
#ifdef ENTANGLED_D3D12_PROBE_SHADER
    .pfnSetGraphicsRootDescriptorTable = set_graphics_table,
    .pfnSetGraphicsRootConstantBufferView = set_graphics_cbv,
    .pfnSetGraphicsRoot32BitConstant = set_graphics_constant,
    .pfnSetGraphicsRoot32BitConstants = set_graphics_constants,
    .pfnSetComputeRootSignature = set_compute_root,
    .pfnSetComputeRootDescriptorTable = set_compute_table,
    .pfnSetComputeRoot32BitConstant = set_compute_constant,
    .pfnSetComputeRoot32BitConstants = set_compute_constants,
    .pfnDispatch = dispatch_compute,
    .pfnBeginQuery = begin_unsupported_query,
    .pfnEndQuery = end_timestamp,
    .pfnResolveQueryData = resolve_timestamps,
#endif
    .pfnIASetIndexBuffer = set_transfer_index_buffer,
    .pfnIASetVertexBuffers = set_transfer_vertex_buffers,
    .pfnSOSetTargets = set_transfer_stream_output,
    .pfnOMSetRenderTargets = set_color_render_targets,
    .pfnClearRootArguments = clear_transfer_root_arguments,
#ifdef ENTANGLED_D3D12_PROBE_SHADER
    .pfnSetPipelineState = set_command_pipeline,
    .pfnDrawInstanced = draw_color_target,
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
    DdiHeap upload = {0}, readback = {0};
    DdiResource src = {0}, dst = {0};
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
