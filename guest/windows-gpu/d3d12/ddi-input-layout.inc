/* SPDX-License-Identifier: Apache-2.0 */
enum { DDI_VERTEX_ELEMENTS = 32 };

typedef struct DdiElementLayout {
    UINT count;
    UINT minimum_stride;
    VkVertexInputRate rate;
    VkVertexInputAttributeDescription attributes[DDI_VERTEX_ELEMENTS];
} DdiElementLayout;

static VkFormat vertex_float_format(DXGI_FORMAT format, UINT *size)
{
    switch (format) {
    case DXGI_FORMAT_R32_FLOAT: *size = 4; return VK_FORMAT_R32_SFLOAT;
    case DXGI_FORMAT_R32G32_FLOAT: *size = 8; return VK_FORMAT_R32G32_SFLOAT;
    case DXGI_FORMAT_R32G32B32_FLOAT: *size = 12; return VK_FORMAT_R32G32B32_SFLOAT;
    case DXGI_FORMAT_R32G32B32A32_FLOAT: *size = 16; return VK_FORMAT_R32G32B32A32_SFLOAT;
    default: return VK_FORMAT_UNDEFINED;
    }
}

static SIZE_T APIENTRY private_vertex_layout_size(D3D12DDI_HDEVICE device,
                                                   const D3D12DDIARG_CREATEELEMENTLAYOUT_0010 *args)
{
    return device.pDrvPrivate && args && args->NumElements && args->pVertexElements &&
        args->NumElements <= DDI_VERTEX_ELEMENTS ? sizeof(DdiElementLayout) : 0;
}

static HRESULT build_vertex_layout(const D3D12DDIARG_CREATEELEMENTLAYOUT_0010 *args, DdiElementLayout *layout)
{
    const D3D12DDI_INPUT_CLASSIFICATION classification = args->pVertexElements[0].InputSlotClass;
    if (classification != D3D12DDI_INPUT_CLASSIFICIATION_PER_VERTEX_DATA &&
        classification != D3D12DDI_INPUT_CLASSIFICIATION_PER_INSTANCE_DATA)
        return E_INVALIDARG;
    const BOOL instanced = classification == D3D12DDI_INPUT_CLASSIFICIATION_PER_INSTANCE_DATA;
    layout->rate = instanced ? VK_VERTEX_INPUT_RATE_INSTANCE : VK_VERTEX_INPUT_RATE_VERTEX;
    UINT next_offset = 0;
    UINT32 locations = 0;
    for (UINT i = 0; i < args->NumElements; ++i) {
        const D3D12DDIARG_INPUT_ELEMENT_DESC *element = &args->pVertexElements[i];
        UINT bytes = 0;
        const VkFormat format = vertex_float_format(element->Format, &bytes);
        const UINT offset = element->AlignedByteOffset == UINT_MAX ? next_offset : element->AlignedByteOffset;
        if (element->InputSlot || element->InputSlotClass != classification ||
            element->InstanceDataStepRate != (UINT)instanced || format == VK_FORMAT_UNDEFINED ||
            element->InputRegister >= DDI_VERTEX_ELEMENTS ||
            (locations & (UINT32_C(1) << element->InputRegister)) || offset % 4 ||
            offset > D3D12_REQ_MULTI_ELEMENT_STRUCTURE_SIZE_IN_BYTES - bytes)
            return DXGI_ERROR_UNSUPPORTED;

        locations |= UINT32_C(1) << element->InputRegister;
        layout->attributes[i] = (VkVertexInputAttributeDescription){
            .location = element->InputRegister, .binding = 0, .format = format, .offset = offset,
        };
        next_offset = offset + bytes;
        if (layout->minimum_stride < next_offset)
            layout->minimum_stride = next_offset;
    }
    layout->count = args->NumElements;
    return S_OK;
}

static void APIENTRY create_vertex_layout(D3D12DDI_HDEVICE device, const D3D12DDIARG_CREATEELEMENTLAYOUT_0010 *args,
                                          D3D12DDI_HELEMENTLAYOUT handle)
{
    DdiElementLayout *layout = handle.pDrvPrivate;
    if (!layout || !private_vertex_layout_size(device, args)) {
        set_command_device_error(device.pDrvPrivate, E_INVALIDARG);
        return;
    }
    *layout = (DdiElementLayout){0};
    const HRESULT status = build_vertex_layout(args, layout);
    trace_ddi("CreateElementLayout", (UINT)status, layout->count);
    set_command_device_error(device.pDrvPrivate, status);
}

static void APIENTRY destroy_vertex_layout(D3D12DDI_HDEVICE device, D3D12DDI_HELEMENTLAYOUT handle)
{
    (void)device;
    DdiElementLayout *layout = handle.pDrvPrivate;
    if (layout)
        *layout = (DdiElementLayout){0};
}
