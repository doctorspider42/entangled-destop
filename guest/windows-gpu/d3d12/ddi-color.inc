/* SPDX-License-Identifier: Apache-2.0 */
static void APIENTRY create_color_rtv(D3D12DDI_HDEVICE device, const D3D12DDIARG_CREATE_RENDER_TARGET_VIEW_0002 *args,
                                      D3D12DDI_CPU_DESCRIPTOR_HANDLE destination)
{
    const VenusColorImage *image = args ? resource_color(venus_device(device), args->hDrvResource) : NULL;
    DdiDescriptor *descriptor = (DdiDescriptor *)destination.ptr;
    if (descriptor && args && !args->hDrvResource.pDrvPrivate && color_image_format(args->Format) != VK_FORMAT_UNDEFINED &&
        args->ResourceDimension == D3D12DDI_RD_TEXTURE2D && !args->Tex2D.MipSlice &&
        !args->Tex2D.FirstArraySlice && !args->Tex2D.PlaneSlice && args->Tex2D.ArraySize == 1) {
        *descriptor = (DdiDescriptor){ .kind = DDI_DESCRIPTOR_RTV, .desc.rtv = *args };
        trace_ddi("CreateNullRenderTargetView", args->Format, 0);
        return;
    }
    if (!descriptor || !image || !(image->usage & VK_IMAGE_USAGE_COLOR_ATTACHMENT_BIT) ||
        args->ResourceDimension != D3D12DDI_RD_TEXTURE2D ||
        (args->Format != DXGI_FORMAT_UNKNOWN && color_image_format(args->Format) != image->format) ||
        args->Tex2D.MipSlice || args->Tex2D.FirstArraySlice || args->Tex2D.PlaneSlice || args->Tex2D.ArraySize != 1) {
        set_command_device_error(device.pDrvPrivate, DXGI_ERROR_UNSUPPORTED);
        return;
    }
    *descriptor = (DdiDescriptor){ .kind = DDI_DESCRIPTOR_RTV, .desc.rtv = *args };
    trace_ddi("CreateRenderTargetView", args->Format, image->extent.width);
}

static VenusColorImage *rtv_color(const VenusDevice *device, D3D12DDI_CPU_DESCRIPTOR_HANDLE rtv)
{
    const DdiDescriptor *descriptor = (const DdiDescriptor *)rtv.ptr;
    return descriptor && descriptor->kind == DDI_DESCRIPTOR_RTV
        ? resource_color(device, descriptor->desc.rtv.hDrvResource) : NULL;
}

static void APIENTRY set_color_render_targets(D3D12DDI_HCOMMANDLIST handle, UINT count,
                                              const D3D12DDI_CPU_DESCRIPTOR_HANDLE *targets, BOOL contiguous,
                                              const D3D12DDI_CPU_DESCRIPTOR_HANDLE *depth)
{
    (void)contiguous;
    DdiTransferList *list = recording_command_list(handle);
    if (!list)
        return;
    list->draw.target = NULL;
    if (depth && depth->ptr) {
        set_command_list_status(list, DXGI_ERROR_UNSUPPORTED);
        return;
    }
    if (!count)
        return;
    VenusColorImage *image = count == 1 && targets ? rtv_color(list->venus.device, targets[0]) : NULL;
    if (!image) {
        set_command_list_status(list, DXGI_ERROR_UNSUPPORTED);
        return;
    }
    list->draw.target = image;
}

#ifdef ENTANGLED_D3D12_PROBE_SHADER
static void APIENTRY create_sampled_srv(D3D12DDI_HDEVICE device, const D3D12DDIARG_CREATE_SHADER_RESOURCE_VIEW_0002 *args,
                                        D3D12DDI_CPU_DESCRIPTOR_HANDLE destination)
{
    const VenusColorImage *image = args ? resource_color(venus_device(device), args->hDrvResource) : NULL;
    DdiDescriptor *descriptor = (DdiDescriptor *)destination.ptr;
    if (!descriptor || !image || !(image->usage & VK_IMAGE_USAGE_SAMPLED_BIT) ||
        args->ResourceDimension != D3D12DDI_RD_TEXTURE2D ||
        (args->Format != DXGI_FORMAT_UNKNOWN && color_image_format(args->Format) != image->format) ||
        args->Shader4ComponentMapping != D3D12DDI_DEFAULT_SHADER_4_COMPONENT_MAPPING ||
        args->Tex2D.MostDetailedMip || args->Tex2D.FirstArraySlice || args->Tex2D.MipLevels != 1 ||
        args->Tex2D.ArraySize != 1 || args->Tex2D.PlaneSlice || args->Tex2D.ResourceMinLODClamp) {
        set_command_device_error(device.pDrvPrivate, DXGI_ERROR_UNSUPPORTED);
        return;
    }
    *descriptor = (DdiDescriptor){ .kind = DDI_DESCRIPTOR_SRV, .desc.srv = *args };
    trace_ddi("CreateShaderResourceView", args->Format, image->extent.width);
}

static void APIENTRY create_pixel_sampler(D3D12DDI_HDEVICE device, const D3D12DDIARG_CREATE_SAMPLER *args,
                                           D3D12DDI_CPU_DESCRIPTOR_HANDLE destination)
{
    const D3D12DDI_SAMPLER_DESC *sampler = args ? args->pSamplerDesc : NULL;
    DdiDescriptor *descriptor = (DdiDescriptor *)destination.ptr;
    if (!descriptor || !sampler || (sampler->Filter & ~D3D12DDI_FILTER_MIN_MAG_MIP_LINEAR) ||
        sampler->AddressU != D3D12DDI_TEXTURE_ADDRESS_MODE_CLAMP ||
        sampler->AddressV != D3D12DDI_TEXTURE_ADDRESS_MODE_CLAMP ||
        sampler->AddressW != D3D12DDI_TEXTURE_ADDRESS_MODE_CLAMP ||
        sampler->MipLODBias || sampler->MinLOD != 0.0f || !(sampler->MaxLOD >= 0.0f)) {
        set_command_device_error(device.pDrvPrivate, DXGI_ERROR_UNSUPPORTED);
        return;
    }
    *descriptor = (DdiDescriptor){ .kind = DDI_DESCRIPTOR_SAMPLER, .desc.sampler = *sampler };
    trace_ddi("CreateSampler", 0, sampler->Filter);
}

static HRESULT create_venus_pixel_sampler(DdiTransferList *list, const D3D12DDI_SAMPLER_DESC *desc, VkSampler *sampler)
{
    const VenusDevice *device = list->venus.device;
    const PFN_vkCreateSampler create = (PFN_vkCreateSampler)device->proc_addr(device->handle, "vkCreateSampler");
    if (!create || !device->proc_addr(device->handle, "vkDestroySampler"))
        return E_FAIL;
    DdiCommandPool *pool = list->pool;
    const FLOAT max_lod = desc->MaxLOD < VK_LOD_CLAMP_NONE ? desc->MaxLOD : VK_LOD_CLAMP_NONE;
    for (UINT i = 0; i < pool->sampler_count; ++i) {
        if (pool->sampler_keys[i].filter == desc->Filter && pool->sampler_keys[i].max_lod == max_lod) {
            *sampler = pool->samplers[i];
            return S_OK;
        }
    }
    if (pool->sampler_count == _countof(pool->samplers))
        return E_OUTOFMEMORY;
    const VkSamplerCreateInfo info = {
        .sType = VK_STRUCTURE_TYPE_SAMPLER_CREATE_INFO,
        .minFilter = desc->Filter & D3D12DDI_FILTER_MIN_LINEAR_MAG_MIP_POINT ? VK_FILTER_LINEAR : VK_FILTER_NEAREST,
        .magFilter = desc->Filter & D3D12DDI_FILTER_MIN_POINT_MAG_LINEAR_MIP_POINT ? VK_FILTER_LINEAR : VK_FILTER_NEAREST,
        .mipmapMode = desc->Filter & D3D12DDI_FILTER_MIN_MAG_POINT_MIP_LINEAR
            ? VK_SAMPLER_MIPMAP_MODE_LINEAR : VK_SAMPLER_MIPMAP_MODE_NEAREST,
        .addressModeU = VK_SAMPLER_ADDRESS_MODE_CLAMP_TO_EDGE,
        .addressModeV = VK_SAMPLER_ADDRESS_MODE_CLAMP_TO_EDGE,
        .addressModeW = VK_SAMPLER_ADDRESS_MODE_CLAMP_TO_EDGE,
        .maxLod = max_lod,
    };
    const HRESULT status = venus_hresult(create(device->handle, &info, NULL, sampler));
    if (SUCCEEDED(status)) {
        const UINT index = pool->sampler_count++;
        pool->samplers[index] = *sampler;
        pool->sampler_keys[index].filter = desc->Filter;
        pool->sampler_keys[index].max_lod = max_lod;
    }
    return status;
}

static HRESULT allocate_command_descriptors(DdiTransferList *list, VkDescriptorSetLayout layout, VkDescriptorSet *set)
{
    const VenusDevice *device = list->venus.device;
    const PFN_vkCreateDescriptorPool create = (PFN_vkCreateDescriptorPool)
        device->proc_addr(device->handle, "vkCreateDescriptorPool");
    const PFN_vkAllocateDescriptorSets allocate = (PFN_vkAllocateDescriptorSets)
        device->proc_addr(device->handle, "vkAllocateDescriptorSets");
    if (!create || !allocate)
        return E_FAIL;
    if (!list->pool->descriptor_pool) {
        if (!device->proc_addr(device->handle, "vkDestroyDescriptorPool") ||
            !device->proc_addr(device->handle, "vkResetDescriptorPool"))
            return E_FAIL;
        const UINT max_sets = _countof(list->pool->samplers);
        const VkDescriptorPoolSize capacity[] = {
            { .type = VK_DESCRIPTOR_TYPE_UNIFORM_BUFFER, .descriptorCount = max_sets * DDI_GRAPHICS_BINDINGS },
            { .type = VK_DESCRIPTOR_TYPE_SAMPLED_IMAGE, .descriptorCount = max_sets * DDI_GRAPHICS_BINDINGS },
            { .type = VK_DESCRIPTOR_TYPE_SAMPLER, .descriptorCount = max_sets *
                (device->indexed_samplers ? 2 * D3D12_MAX_SHADER_VISIBLE_SAMPLER_HEAP_SIZE : DDI_GRAPHICS_BINDINGS) },
            { .type = VK_DESCRIPTOR_TYPE_STORAGE_BUFFER, .descriptorCount = max_sets * DDI_GRAPHICS_BINDINGS },
        };
        const VkDescriptorPoolCreateInfo info = {
            .sType = VK_STRUCTURE_TYPE_DESCRIPTOR_POOL_CREATE_INFO, .maxSets = max_sets,
            .poolSizeCount = _countof(capacity), .pPoolSizes = capacity,
        };
        VkDescriptorPool pool;
        const VkResult result = create(device->handle, &info, NULL, &pool);
        if (result != VK_SUCCESS)
            return venus_hresult(result);
        list->pool->descriptor_pool = pool;
    }
    const VkDescriptorSetAllocateInfo allocation = {
        .sType = VK_STRUCTURE_TYPE_DESCRIPTOR_SET_ALLOCATE_INFO, .descriptorPool = list->pool->descriptor_pool,
        .descriptorSetCount = 1, .pSetLayouts = &layout,
    };
    return venus_hresult(allocate(device->handle, &allocation, set));
}

static HRESULT bind_draw_vertices(const DdiTransferList *list, UINT64 last)
{
    const D3D12DDI_VERTEX_BUFFER_VIEW *view = &list->draw.vertex_buffer;
    const UINT required = list->draw.pipeline->minimum_vertex_stride;
    VkDescriptorBufferInfo buffer;
    if (view->StrideInBytes < required || view->StrideInBytes > D3D12_REQ_MULTI_ELEMENT_STRUCTURE_SIZE_IN_BYTES ||
        view->BufferLocation % 4 || view->StrideInBytes % 4 || view->SizeInBytes < required ||
        last > (view->SizeInBytes - required) / view->StrideInBytes ||
        !resolve_buffer_address(list->device, view->BufferLocation, view->SizeInBytes, &buffer))
        return E_INVALIDARG;
    const VenusDevice *venus = list->venus.device;
    const PFN_vkCmdBindVertexBuffers2 bind = (PFN_vkCmdBindVertexBuffers2)
        venus->proc_addr(venus->handle, "vkCmdBindVertexBuffers2");
    if (!bind)
        return E_FAIL;
    const VkDeviceSize stride = view->StrideInBytes;
    bind(list->venus.command, 0, 1, &buffer.buffer, &buffer.offset, &buffer.range, &stride);
    trace_ddi("BindVertexBuffer", 0, view->StrideInBytes);
    return S_OK;
}

static void APIENTRY draw_color_target(D3D12DDI_HCOMMANDLIST handle, UINT vertex_count, UINT instance_count,
                                       UINT first_vertex, UINT first_instance)
{
    DdiTransferList *list = recording_command_list(handle);
    if (!list || !vertex_count || !instance_count)
        return;
    const DdiDrawState *draw = &list->draw;
    const VenusColorImage *image = draw->target;
    if (!image || !draw->pipeline || draw->pipeline->color_format != image->format ||
        !draw->root || draw->pipeline->graphics_root != draw->root ||
        draw->topology != D3D12DDI_PRIMITIVE_TOPOLOGY_TRIANGLELIST || !draw->has_viewport || !draw->has_scissor) {
        set_command_list_status(list, DXGI_ERROR_UNSUPPORTED);
        return;
    }

    AcquireSRWLockExclusive(&list->pool->lock);
    HRESULT status = list->generation == list->pool->generation ? S_OK : E_INVALIDARG;
    if (SUCCEEDED(status) && draw->pipeline->minimum_vertex_stride) {
        const UINT64 last = draw->pipeline->vertex_rate == VK_VERTEX_INPUT_RATE_INSTANCE
            ? (UINT64)first_instance + instance_count - 1 : (UINT64)first_vertex + vertex_count - 1;
        status = bind_draw_vertices(list, last);
    }
    if (SUCCEEDED(status))
        status = bind_graphics_resources(list);
    if (SUCCEEDED(status))
        status = record_venus_color_draw(list->venus.command, image, &draw->viewport, &draw->scissor,
                                         vertex_count, instance_count, first_vertex, first_instance);
    ReleaseSRWLockExclusive(&list->pool->lock);
    trace_ddi("DrawInstanced", (UINT)status, vertex_count);
    set_command_list_status(list, status);
}
#endif

static void APIENTRY clear_color_rtv(D3D12DDI_HCOMMANDLIST handle, D3D12DDI_CPU_DESCRIPTOR_HANDLE view,
                                    const FLOAT color[4], UINT rect_count, const D3D12DDI_RECT *rects)
{
    (void)rects;
    DdiTransferList *list = handle.pDrvPrivate;
    if (!list || FAILED(list->status))
        return;
    const VenusColorImage *image = rtv_color(list->venus.device, view);
    if (!list->pool || !image || !color || rect_count) {
        set_command_list_status(list, DXGI_ERROR_UNSUPPORTED);
        return;
    }

    AcquireSRWLockExclusive(&list->pool->lock);
    const HRESULT status = !list->closed && list->generation == list->pool->generation
        ? record_venus_clear_color(list->venus.command, image, color) : E_INVALIDARG;
    ReleaseSRWLockExclusive(&list->pool->lock);
    trace_ddi("ClearRenderTargetView", (UINT)status, image->extent.height);
    set_command_list_status(list, status);
}

static VkAccessFlags buffer_access(D3D12DDI_RESOURCE_STATES state)
{
    if (state == D3D12DDI_RESOURCE_STATE_COMMON)
        return VK_ACCESS_MEMORY_READ_BIT | VK_ACCESS_MEMORY_WRITE_BIT;
    const VkAccessFlags vertex = state & D3D12DDI_RESOURCE_STATE_VERTEX_AND_CONSTANT_BUFFER
        ? VK_ACCESS_VERTEX_ATTRIBUTE_READ_BIT | VK_ACCESS_UNIFORM_READ_BIT : 0;
    const VkAccessFlags index = state & D3D12DDI_RESOURCE_STATE_INDEX_BUFFER ? VK_ACCESS_INDEX_READ_BIT : 0;
    const VkAccessFlags read = state & D3D12DDI_RESOURCE_STATE_COPY_SOURCE ? VK_ACCESS_TRANSFER_READ_BIT : 0;
    const VkAccessFlags write = state & D3D12DDI_RESOURCE_STATE_COPY_DEST ? VK_ACCESS_TRANSFER_WRITE_BIT : 0;
    const VkAccessFlags shader_read =
        state & (D3D12DDI_RESOURCE_STATE_NON_PIXEL_SHADER_RESOURCE | D3D12DDI_RESOURCE_STATE_PIXEL_SHADER_RESOURCE |
                 D3D12DDI_RESOURCE_STATE_UNORDERED_ACCESS) ? VK_ACCESS_SHADER_READ_BIT : 0;
    const VkAccessFlags shader_write = state & D3D12DDI_RESOURCE_STATE_UNORDERED_ACCESS ? VK_ACCESS_SHADER_WRITE_BIT : 0;
    return vertex | index | read | write | shader_read | shader_write;
}

static VkAccessFlags color_access(D3D12DDI_RESOURCE_STATES state)
{
    /* ClearRenderTargetView also writes through Vulkan's transfer stage. */
    const VkAccessFlags attachment = state & D3D12DDI_RESOURCE_STATE_RENDER_TARGET
        ? VK_ACCESS_COLOR_ATTACHMENT_READ_BIT | VK_ACCESS_COLOR_ATTACHMENT_WRITE_BIT | VK_ACCESS_TRANSFER_WRITE_BIT : 0;
    return buffer_access(state) | attachment;
}

static HRESULT transition_resource(DdiTransferList *list, const D3D12DDIARG_RESOURCE_BARRIER_0022 *barrier)
{
    if (barrier->Type != D3D12DDI_RESOURCE_BARRIER_TYPE_TRANSITION || barrier->Flags ||
        (barrier->Transition.Subresource && barrier->Transition.Subresource != D3D12DDI_RESOURCE_BARRIER_ALL_SUBRESOURCES))
        return DXGI_ERROR_UNSUPPORTED;
    DdiHeap *heap = resource_heap(list->venus.device, barrier->Transition.hResource);
    if (!heap)
        return DXGI_ERROR_UNSUPPORTED;
    const D3D12DDI_RESOURCE_STATES before = barrier->Transition.StateBefore;
    const D3D12DDI_RESOURCE_STATES after = barrier->Transition.StateAfter;
    const VenusTransferBuffer *buffer = resource_buffer(list->venus.device, barrier->Transition.hResource);
    if (buffer) {
        const D3D12DDI_RESOURCE_STATES supported = D3D12DDI_RESOURCE_STATE_COPY_SOURCE |
            D3D12DDI_RESOURCE_STATE_COPY_DEST | D3D12DDI_RESOURCE_STATE_VERTEX_AND_CONSTANT_BUFFER |
            D3D12DDI_RESOURCE_STATE_INDEX_BUFFER | D3D12DDI_RESOURCE_STATE_NON_PIXEL_SHADER_RESOURCE |
            D3D12DDI_RESOURCE_STATE_PIXEL_SHADER_RESOURCE | D3D12DDI_RESOURCE_STATE_UNORDERED_ACCESS;
        if ((before | after) & ~supported)
            return DXGI_ERROR_UNSUPPORTED;
        return record_venus_buffer_barrier(list->venus.command, buffer, buffer_access(before), buffer_access(after));
    }
    const D3D12DDI_RESOURCE_STATES supported = D3D12DDI_RESOURCE_STATE_COPY_SOURCE |
        D3D12DDI_RESOURCE_STATE_COPY_DEST | D3D12DDI_RESOURCE_STATE_RENDER_TARGET |
        D3D12DDI_RESOURCE_STATE_NON_PIXEL_SHADER_RESOURCE | D3D12DDI_RESOURCE_STATE_PIXEL_SHADER_RESOURCE;
    const VenusColorImage *image = resource_color(list->venus.device, barrier->Transition.hResource);
    if (!image || ((before | after) & ~supported))
        return DXGI_ERROR_UNSUPPORTED;
#ifdef ENTANGLED_D3D12_PROBE_PRESENT
    if (heap->present.allocation && after == D3D12DDI_RESOURCE_STATE_COMMON) {
        if (list->present && list->present != &heap->present)
            return DXGI_ERROR_UNSUPPORTED;
        const BOOL cpu_copy = InterlockedCompareExchange(&heap->present.prefer_gpu, 0, 0) == 0;
        const HRESULT copied = cpu_copy
            ? copy_venus_color_to_buffer(list->venus.command, image, &heap->present.pixels, 0, image->extent.width * 4)
            : record_venus_color_barrier(list->venus.command, image, color_access(before), 0);
        if (SUCCEEDED(copied)) {
            list->present = &heap->present;
            list->present_cpu_copy = cpu_copy;
        }
        return copied;
    }
#endif
    return record_venus_color_barrier(list->venus.command, image, color_access(before), color_access(after));
}

static void APIENTRY transfer_resource_barriers(D3D12DDI_HCOMMANDLIST handle, UINT count,
                                                const D3D12DDIARG_RESOURCE_BARRIER_0022 *barriers)
{
    DdiTransferList *list = handle.pDrvPrivate;
    if (!list || FAILED(list->status) || !count)
        return;
    if (!list->pool || !barriers) {
        set_command_list_status(list, E_INVALIDARG);
        return;
    }

    AcquireSRWLockExclusive(&list->pool->lock);
    HRESULT status = !list->closed && list->generation == list->pool->generation ? S_OK : E_INVALIDARG;
    for (UINT i = 0; i < count && SUCCEEDED(status); ++i) {
        status = transition_resource(list, &barriers[i]);
        if (FAILED(status)) {
            trace_ddi("Unsupported resource barrier", barriers[i].Type, barriers[i].Flags);
            if (barriers[i].Type == D3D12DDI_RESOURCE_BARRIER_TYPE_TRANSITION)
                trace_ddi("Unsupported state transition", barriers[i].Transition.StateBefore,
                          barriers[i].Transition.StateAfter);
        }
    }
    ReleaseSRWLockExclusive(&list->pool->lock);
    trace_ddi("ResourceBarrier", (UINT)status, count);
    set_command_list_status(list, status);
}

typedef struct DdiColorFootprint {
    UINT32 width, height, row_pitch;
} DdiColorFootprint;

static DdiColorFootprint color_copy_footprint(D3D12DDIARG_PLACED_RESOURCE target, VkFormat format)
{
    if (!target.pLayout)
        return (DdiColorFootprint){0};
    if (target.Layout == D3D12DDI_RL_PLACED_PHYSICAL_SUBRESOURCE_PITCHED) {
        const D3D12DDIARG_PHYSICAL_SUBRESOURCE_PITCHED_LAYOUT *p = target.pLayout;
        if (p->PhysicalDepth == 1 && color_image_format(p->Format) == format)
            return (DdiColorFootprint){p->PhysicalWidth, p->PhysicalHeight, p->Pitch};
    }
    if (target.Layout == D3D12DDI_RL_PLACED_VIRTUAL_SUBRESOURCE_PITCHED) {
        const D3D12DDIARG_VIRTUAL_SUBRESOURCE_PITCHED_LAYOUT *p = target.pLayout;
        if (p->VirtualDepth == 1 && color_image_format(p->Format) == format)
            return (DdiColorFootprint){p->VirtualWidth, p->VirtualHeight, p->Pitch};
    }
    return (DdiColorFootprint){0};
}

static BOOL whole_color_copy(const VenusColorImage *image, const D3D12DDI_BOX *box)
{
    return !box || (!box->Left && !box->Top && !box->Front && box->Right == (LONG)image->extent.width &&
                    box->Bottom == (LONG)image->extent.height && box->Back == 1);
}

static HRESULT upload_color_texture_region(DdiTransferList *list, const D3D12DDIARG_BUFFER_PLACEMENT *dst,
                                             UINT dst_x, UINT dst_y, UINT dst_z,
                                             const D3D12DDIARG_BUFFER_PLACEMENT *src,
                                             D3D12DDIARG_PLACED_RESOURCE src_layout, const D3D12DDI_BOX *box)
{
    const VenusColorImage *image = dst && !dst->BaseAddress.UMD.Offset
        ? resource_color(list->venus.device, dst->BaseAddress.UMD.hResource) : NULL;
    const VenusTransferBuffer *buffer = src ? resource_buffer(list->venus.device, src->BaseAddress.UMD.hResource) : NULL;
    if (!list->pool || !image || !buffer || dst_z || dst_x >= image->extent.width || dst_y >= image->extent.height)
        return DXGI_ERROR_UNSUPPORTED;
    const DdiColorFootprint footprint = color_copy_footprint(src_layout, image->format);
    const UINT texel_size = image->format == VK_FORMAT_R8_UNORM ? 1 : 4;
    if (!footprint.width || !footprint.height || footprint.width > footprint.row_pitch / texel_size)
        return E_INVALIDARG;

    VkDeviceSize offset = src->BaseAddress.UMD.Offset;
    VkExtent3D extent = {footprint.width, footprint.height, 1};
    if (box) {
        if (box->Left < 0 || box->Top < 0 || box->Front || box->Back != 1 ||
            box->Right <= box->Left || box->Bottom <= box->Top ||
            (UINT)box->Right > footprint.width || (UINT)box->Bottom > footprint.height)
            return E_INVALIDARG;
        const VkDeviceSize start = (VkDeviceSize)box->Top * footprint.row_pitch + (UINT)box->Left * texel_size;
        if (offset > UINT64_MAX - start)
            return E_INVALIDARG;
        offset += start;
        extent.width = (UINT)(box->Right - box->Left);
        extent.height = (UINT)(box->Bottom - box->Top);
    }
    const VkOffset3D origin = {(LONG)dst_x, (LONG)dst_y, 0};
    AcquireSRWLockExclusive(&list->pool->lock);
    const HRESULT status = !list->closed && list->generation == list->pool->generation
        ? upload_venus_color_image(list->venus.command, buffer, offset, footprint.row_pitch, image, origin, extent)
        : E_INVALIDARG;
    ReleaseSRWLockExclusive(&list->pool->lock);
    trace_ddi("UploadTextureRegion", (UINT)status, ((UINT64)extent.width << 32) | extent.height);
    return status;
}

static void APIENTRY copy_color_texture_region(
    D3D12DDI_HCOMMANDLIST handle, const D3D12DDIARG_BUFFER_PLACEMENT *dst_placement,
    D3D12DDIARG_PLACED_RESOURCE dst_layout, UINT dst_x, UINT dst_y, UINT dst_z,
    const D3D12DDIARG_BUFFER_PLACEMENT *src_placement, D3D12DDIARG_PLACED_RESOURCE src_layout, const D3D12DDI_BOX *box)
{
    DdiTransferList *list = handle.pDrvPrivate;
    if (!list || FAILED(list->status))
        return;
    if (dst_layout.Layout == D3D12DDI_RL_SELECT_SUBRESOURCE && src_layout.Layout != D3D12DDI_RL_SELECT_SUBRESOURCE) {
        const HRESULT status = upload_color_texture_region(list, dst_placement, dst_x, dst_y, dst_z,
                                                            src_placement, src_layout, box);
        set_command_list_status(list, status);
        return;
    }
    const VenusColorImage *image = src_placement && src_layout.Layout == D3D12DDI_RL_SELECT_SUBRESOURCE &&
        !src_placement->BaseAddress.UMD.Offset ? resource_color(list->venus.device, src_placement->BaseAddress.UMD.hResource) : NULL;
    const VenusTransferBuffer *buffer = dst_placement
        ? resource_buffer(list->venus.device, dst_placement->BaseAddress.UMD.hResource) : NULL;
    if (!list->pool || !image || !buffer ||
        dst_x || dst_y || dst_z || !whole_color_copy(image, box)) {
        trace_ddi("CopyTextureRegion layouts", src_layout.Layout, dst_layout.Layout);
        set_command_list_status(list, DXGI_ERROR_UNSUPPORTED);
        return;
    }

    const DdiColorFootprint footprint = color_copy_footprint(dst_layout, image->format);
    if (footprint.width < image->extent.width || footprint.height < image->extent.height) {
        set_command_list_status(list, E_INVALIDARG);
        return;
    }
    AcquireSRWLockExclusive(&list->pool->lock);
    const HRESULT status = !list->closed && list->generation == list->pool->generation
        ? copy_venus_color_to_buffer(list->venus.command, image, buffer, dst_placement->BaseAddress.UMD.Offset,
                                    footprint.row_pitch)
        : E_INVALIDARG;
    ReleaseSRWLockExclusive(&list->pool->lock);
    trace_ddi("CopyTextureRegion", (UINT)status, footprint.row_pitch);
    set_command_list_status(list, status);
}
