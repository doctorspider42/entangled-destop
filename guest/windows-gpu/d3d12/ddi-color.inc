/* SPDX-License-Identifier: Apache-2.0 */
static VenusColorImage *resource_color(const VenusDevice *device, D3D12DDI_HRESOURCE resource)
{
    DdiHeap *heap = resource_heap(device, resource);
    return heap && heap->kind == DDI_HEAP_COLOR_IMAGE ? &heap->image : NULL;
}

static void APIENTRY create_color_rtv(D3D12DDI_HDEVICE device, const D3D12DDIARG_CREATE_RENDER_TARGET_VIEW_0002 *args,
                                      D3D12DDI_CPU_DESCRIPTOR_HANDLE destination)
{
    const VenusColorImage *image = args ? resource_color(venus_device(device), args->hDrvResource) : NULL;
    DdiDescriptor *descriptor = (DdiDescriptor *)destination.ptr;
    if (!descriptor || !image || args->ResourceDimension != D3D12DDI_RD_TEXTURE2D ||
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
static void APIENTRY draw_color_target(D3D12DDI_HCOMMANDLIST handle, UINT vertex_count, UINT instance_count,
                                       UINT first_vertex, UINT first_instance)
{
    DdiTransferList *list = recording_command_list(handle);
    if (!list || !vertex_count || !instance_count)
        return;
    const DdiDrawState *draw = &list->draw;
    VenusColorImage *image = draw->target;
    if (!image || list->written_color || !draw->pipeline || draw->pipeline->color_format != image->format ||
        !draw->root_layout || draw->pipeline->layout != draw->root_layout ||
        draw->topology != D3D12DDI_PRIMITIVE_TOPOLOGY_TRIANGLELIST || !draw->has_viewport || !draw->has_scissor) {
        set_command_list_status(list, DXGI_ERROR_UNSUPPORTED);
        return;
    }

    AcquireSRWLockExclusive(&list->pool->lock);
    const HRESULT status = list->generation == list->pool->generation
        ? record_venus_color_draw(list->venus.command, image, &draw->viewport, &draw->scissor,
                                  vertex_count, instance_count, first_vertex, first_instance) : E_INVALIDARG;
    if (SUCCEEDED(status)) {
        list->written_color = image;
        list->color_rendered = TRUE;
    }
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
    if (!list->pool || !image || !color || rect_count || list->written_color) {
        set_command_list_status(list, DXGI_ERROR_UNSUPPORTED);
        return;
    }

    AcquireSRWLockExclusive(&list->pool->lock);
    const HRESULT status = !list->closed && list->generation == list->pool->generation
        ? record_venus_clear_color(list->venus.command, image, color) : E_INVALIDARG;
    if (SUCCEEDED(status))
        list->written_color = image;
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
    return vertex | index | read | write;
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
    if (heap->kind == DDI_HEAP_TRANSFER_BUFFER) {
        const D3D12DDI_RESOURCE_STATES supported = D3D12DDI_RESOURCE_STATE_COPY_SOURCE |
            D3D12DDI_RESOURCE_STATE_COPY_DEST | D3D12DDI_RESOURCE_STATE_VERTEX_AND_CONSTANT_BUFFER |
            D3D12DDI_RESOURCE_STATE_INDEX_BUFFER;
        if ((before | after) & ~supported)
            return DXGI_ERROR_UNSUPPORTED;
        return record_venus_buffer_barrier(list->venus.command, &heap->buffer, buffer_access(before), buffer_access(after));
    }
    if (heap->kind != DDI_HEAP_COLOR_IMAGE)
        return DXGI_ERROR_UNSUPPORTED;
#ifdef ENTANGLED_D3D12_PROBE_PRESENT
    if (heap->present.allocation && before == D3D12DDI_RESOURCE_STATE_COMMON && after == D3D12DDI_RESOURCE_STATE_RENDER_TARGET)
        return S_OK; /* The lab draw and clear discard the previous destination contents. */
#endif
    const VenusColorImage *image = &heap->image;
    if (before != D3D12DDI_RESOURCE_STATE_RENDER_TARGET || image != list->written_color || list->color_copy_source)
        return DXGI_ERROR_UNSUPPORTED;
#ifdef ENTANGLED_D3D12_PROBE_PRESENT
    if (heap->present.allocation && after == D3D12DDI_RESOURCE_STATE_COMMON && !list->present) {
        const HRESULT status = prepare_venus_color_copy(list->venus.command, image, list->color_rendered);
        if (FAILED(status))
            return status;
        const HRESULT copied = copy_venus_color_to_buffer(list->venus.command, image, list->color_rendered,
                                                         &heap->present.pixels, 0, image->extent.width * 4);
        if (SUCCEEDED(copied))
            list->present = &heap->present;
        return copied;
    }
#endif
    if (after != D3D12DDI_RESOURCE_STATE_COPY_SOURCE)
        return DXGI_ERROR_UNSUPPORTED;
    const HRESULT status = prepare_venus_color_copy(list->venus.command, image, list->color_rendered);
    if (SUCCEEDED(status))
        list->color_copy_source = TRUE;
    return status;
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
    for (UINT i = 0; i < count && SUCCEEDED(status); ++i)
        status = transition_resource(list, &barriers[i]);
    ReleaseSRWLockExclusive(&list->pool->lock);
    trace_ddi("ResourceBarrier", (UINT)status, count);
    set_command_list_status(list, status);
}

static UINT32 color_copy_row_pitch(D3D12DDIARG_PLACED_RESOURCE target, const VenusColorImage *image)
{
    if (!target.pLayout)
        return 0;
    if (target.Layout == D3D12DDI_RL_PLACED_PHYSICAL_SUBRESOURCE_PITCHED) {
        const D3D12DDIARG_PHYSICAL_SUBRESOURCE_PITCHED_LAYOUT *p = target.pLayout;
        if (p->PhysicalWidth >= image->extent.width && p->PhysicalHeight >= image->extent.height &&
            p->PhysicalDepth == 1 && color_image_format(p->Format) == image->format)
            return p->Pitch;
    }
    if (target.Layout == D3D12DDI_RL_PLACED_VIRTUAL_SUBRESOURCE_PITCHED) {
        const D3D12DDIARG_VIRTUAL_SUBRESOURCE_PITCHED_LAYOUT *p = target.pLayout;
        if (p->VirtualWidth >= image->extent.width && p->VirtualHeight >= image->extent.height &&
            p->VirtualDepth == 1 && color_image_format(p->Format) == image->format)
            return p->Pitch;
    }
    return 0;
}

static void APIENTRY copy_color_texture_region(
    D3D12DDI_HCOMMANDLIST handle, const D3D12DDIARG_BUFFER_PLACEMENT *dst_placement,
    D3D12DDIARG_PLACED_RESOURCE dst_layout, UINT dst_x, UINT dst_y, UINT dst_z,
    const D3D12DDIARG_BUFFER_PLACEMENT *src_placement, D3D12DDIARG_PLACED_RESOURCE src_layout, const D3D12DDI_BOX *box)
{
    DdiTransferList *list = handle.pDrvPrivate;
    if (!list || FAILED(list->status))
        return;
    const VenusColorImage *image = src_placement && src_layout.Layout == D3D12DDI_RL_SELECT_SUBRESOURCE &&
        !src_placement->BaseAddress.UMD.Offset ? resource_color(list->venus.device, src_placement->BaseAddress.UMD.hResource) : NULL;
    const VenusTransferBuffer *buffer = dst_placement
        ? resource_buffer(list->venus.device, dst_placement->BaseAddress.UMD.hResource) : NULL;
    if (!list->pool || !image || image != list->written_color || !list->color_copy_source || !buffer ||
        dst_x || dst_y || dst_z || (box && (box->Left || box->Top || box->Front ||
        box->Right != (LONG)image->extent.width || box->Bottom != (LONG)image->extent.height || box->Back != 1))) {
        trace_ddi("CopyTextureRegion layouts", src_layout.Layout, dst_layout.Layout);
        set_command_list_status(list, DXGI_ERROR_UNSUPPORTED);
        return;
    }

    const UINT32 row_pitch = color_copy_row_pitch(dst_layout, image);
    AcquireSRWLockExclusive(&list->pool->lock);
    const HRESULT status = !list->closed && list->generation == list->pool->generation
        ? copy_venus_color_to_buffer(list->venus.command, image, list->color_rendered, buffer,
                                    dst_placement->BaseAddress.UMD.Offset, row_pitch)
        : E_INVALIDARG;
    ReleaseSRWLockExclusive(&list->pool->lock);
    trace_ddi("CopyTextureRegion", (UINT)status, row_pitch);
    set_command_list_status(list, status);
}
