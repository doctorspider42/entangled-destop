/* SPDX-License-Identifier: Apache-2.0 */
typedef struct DdiDescriptor {
    enum {
        DDI_DESCRIPTOR_EMPTY,
        DDI_DESCRIPTOR_CBV,
        DDI_DESCRIPTOR_SRV,
        DDI_DESCRIPTOR_UAV,
        DDI_DESCRIPTOR_RTV,
        DDI_DESCRIPTOR_DSV,
        DDI_DESCRIPTOR_SAMPLER,
    } kind;
    union {
        D3D12DDI_CONSTANT_BUFFER_VIEW_DESC cbv;
        D3D12DDIARG_CREATE_SHADER_RESOURCE_VIEW_0002 srv;
        D3D12DDIARG_CREATE_UNORDERED_ACCESS_VIEW_0002 uav;
        D3D12DDIARG_CREATE_RENDER_TARGET_VIEW_0002 rtv;
        D3D12DDIARG_CREATE_DEPTH_STENCIL_VIEW dsv;
        D3D12DDIARG_CREATE_SAMPLER sampler;
    } desc;
} DdiDescriptor;

typedef struct DdiDescriptorHeap {
    void *device;
    D3D12DDIARG_CREATE_DESCRIPTOR_HEAP_0001 args;
} DdiDescriptorHeap;

_Static_assert(_Alignof(DdiDescriptorHeap) >= _Alignof(DdiDescriptor), "descriptor storage alignment");

static UINT APIENTRY descriptor_size(D3D12DDI_HDEVICE device, D3D12DDI_DESCRIPTOR_HEAP_TYPE type)
{
    const UINT stride = device.pDrvPrivate && (UINT)type < D3D12DDI_DESCRIPTOR_HEAP_TYPE_NUM_TYPES
        ? sizeof(DdiDescriptor) : 0;
    trace_ddi("GetDescriptorSizeInBytes", type, stride);
    return stride;
}

static SIZE_T APIENTRY private_descriptor_heap_size(D3D12DDI_HDEVICE device,
                                                    const D3D12DDIARG_CREATE_DESCRIPTOR_HEAP_0001 *args)
{
    if (!device.pDrvPrivate || !args ||
        (UINT)args->Type >= D3D12DDI_DESCRIPTOR_HEAP_TYPE_NUM_TYPES || args->NodeMask > 1 ||
        (args->Flags & ~(D3D12DDI_DESCRIPTOR_HEAP_FLAG_CPU_VISIBLE | D3D12DDI_DESCRIPTOR_HEAP_FLAG_SHADER_VISIBLE)) ||
        ((args->Flags & D3D12DDI_DESCRIPTOR_HEAP_FLAG_SHADER_VISIBLE) &&
         args->Type >= D3D12DDI_DESCRIPTOR_HEAP_TYPE_RTV) ||
        !args->NumDescriptors || args->NumDescriptors > (SIZE_MAX - sizeof(DdiDescriptorHeap)) / sizeof(DdiDescriptor))
        return 0;
    return sizeof(DdiDescriptorHeap) + (SIZE_T)args->NumDescriptors * sizeof(DdiDescriptor);
}

static HRESULT APIENTRY create_descriptor_heap(D3D12DDI_HDEVICE device,
                                               const D3D12DDIARG_CREATE_DESCRIPTOR_HEAP_0001 *args,
                                               D3D12DDI_HDESCRIPTORHEAP handle)
{
    DdiDescriptorHeap *heap = handle.pDrvPrivate;
    const SIZE_T bytes = private_descriptor_heap_size(device, args);
    if (!heap || !bytes)
        return E_INVALIDARG;
    memset(heap, 0, bytes);
    heap->device = device.pDrvPrivate;
    heap->args = *args;
    trace_ddi("CreateDescriptorHeap", args->Type, args->NumDescriptors);
    return S_OK;
}

static void APIENTRY destroy_descriptor_heap(D3D12DDI_HDEVICE device, D3D12DDI_HDESCRIPTORHEAP handle)
{
    DdiDescriptorHeap *heap = handle.pDrvPrivate;
    if (heap && heap->device == device.pDrvPrivate)
        heap->device = NULL;
}

static D3D12DDI_CPU_DESCRIPTOR_HANDLE APIENTRY cpu_descriptor_heap_start(D3D12DDI_HDEVICE device,
                                                                       D3D12DDI_HDESCRIPTORHEAP handle)
{
    const DdiDescriptorHeap *heap = handle.pDrvPrivate;
    return (D3D12DDI_CPU_DESCRIPTOR_HANDLE){ heap && heap->device && heap->device == device.pDrvPrivate
        && (heap->args.Flags & D3D12DDI_DESCRIPTOR_HEAP_FLAG_CPU_VISIBLE) ? (SIZE_T)(heap + 1) : 0 };
}

static D3D12DDI_GPU_DESCRIPTOR_HANDLE APIENTRY gpu_descriptor_heap_start(D3D12DDI_HDEVICE device,
                                                                       D3D12DDI_HDESCRIPTORHEAP handle)
{
    const DdiDescriptorHeap *heap = handle.pDrvPrivate;
    /* This is an opaque descriptor table token. Binding must resolve it inside the current heap. */
    return (D3D12DDI_GPU_DESCRIPTOR_HANDLE){ heap && heap->device && heap->device == device.pDrvPrivate
        && (heap->args.Flags & D3D12DDI_DESCRIPTOR_HEAP_FLAG_SHADER_VISIBLE) ? (UINT64)(UINT_PTR)(heap + 1) : 0 };
}
