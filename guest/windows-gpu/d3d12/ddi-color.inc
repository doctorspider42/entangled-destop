/* SPDX-License-Identifier: Apache-2.0 */
static const VenusColorImage *resource_color(const VenusDevice *device, D3D12DDI_HRESOURCE resource)
{
    const DdiHeap *heap = resource_heap(device, resource);
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

static void APIENTRY clear_color_rtv(D3D12DDI_HCOMMANDLIST handle, D3D12DDI_CPU_DESCRIPTOR_HANDLE view,
                                    const FLOAT color[4], UINT rect_count, const D3D12DDI_RECT *rects)
{
    (void)rects;
    DdiTransferList *list = handle.pDrvPrivate;
    if (!list || FAILED(list->status))
        return;
    const DdiDescriptor *descriptor = (const DdiDescriptor *)view.ptr;
    const VenusColorImage *image = descriptor && descriptor->kind == DDI_DESCRIPTOR_RTV
        ? resource_color(list->venus.device, descriptor->desc.rtv.hDrvResource) : NULL;
    if (!list->pool || !image || !color || rect_count || list->cleared_color) {
        set_command_list_status(list, DXGI_ERROR_UNSUPPORTED);
        return;
    }

    AcquireSRWLockExclusive(&list->pool->lock);
    const HRESULT status = !list->closed && list->generation == list->pool->generation
        ? record_venus_clear_color(list->venus.command, image, color) : E_INVALIDARG;
    if (SUCCEEDED(status))
        list->cleared_color = image;
    ReleaseSRWLockExclusive(&list->pool->lock);
    trace_ddi("ClearRenderTargetView", (UINT)status, image->extent.height);
    set_command_list_status(list, status);
}

static HRESULT color_barrier(DdiTransferList *list, const D3D12DDIARG_RESOURCE_BARRIER_0022 *barrier)
{
    if (barrier->Type != D3D12DDI_RESOURCE_BARRIER_TYPE_TRANSITION || barrier->Flags ||
        barrier->Transition.StateBefore != D3D12DDI_RESOURCE_STATE_RENDER_TARGET ||
        barrier->Transition.StateAfter != D3D12DDI_RESOURCE_STATE_COPY_SOURCE ||
        (barrier->Transition.Subresource && barrier->Transition.Subresource != D3D12DDI_RESOURCE_BARRIER_ALL_SUBRESOURCES))
        return DXGI_ERROR_UNSUPPORTED;
    const VenusColorImage *image = resource_color(list->venus.device, barrier->Transition.hResource);
    if (!image || image != list->cleared_color || list->color_copy_source)
        return DXGI_ERROR_UNSUPPORTED;
    const HRESULT status = prepare_venus_color_copy(list->venus.command, image);
    if (SUCCEEDED(status))
        list->color_copy_source = TRUE;
    return status;
}

static void APIENTRY color_resource_barriers(D3D12DDI_HCOMMANDLIST handle, UINT count,
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
        status = color_barrier(list, &barriers[i]);
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
    if (!list->pool || !image || image != list->cleared_color || !list->color_copy_source || !buffer ||
        dst_x || dst_y || dst_z || (box && (box->Left || box->Top || box->Front ||
        box->Right != (LONG)image->extent.width || box->Bottom != (LONG)image->extent.height || box->Back != 1))) {
        trace_ddi("CopyTextureRegion layouts", src_layout.Layout, dst_layout.Layout);
        set_command_list_status(list, DXGI_ERROR_UNSUPPORTED);
        return;
    }

    const UINT32 row_pitch = color_copy_row_pitch(dst_layout, image);
    AcquireSRWLockExclusive(&list->pool->lock);
    const HRESULT status = !list->closed && list->generation == list->pool->generation
        ? copy_venus_color_to_buffer(list->venus.command, image, buffer, dst_placement->BaseAddress.UMD.Offset, row_pitch)
        : E_INVALIDARG;
    ReleaseSRWLockExclusive(&list->pool->lock);
    trace_ddi("CopyTextureRegion", (UINT)status, row_pitch);
    set_command_list_status(list, status);
}
