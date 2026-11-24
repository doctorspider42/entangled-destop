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
        D3D12DDI_SAMPLER_DESC sampler;
    } desc;
} DdiDescriptor;

typedef struct DdiDescriptorHeap {
    DdiDevice *device;
    struct DdiDescriptorHeap *next;
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
    AcquireSRWLockExclusive(&heap->device->descriptor_lock);
    heap->next = heap->device->descriptor_heaps;
    heap->device->descriptor_heaps = heap;
    ReleaseSRWLockExclusive(&heap->device->descriptor_lock);
    trace_ddi("CreateDescriptorHeap", args->Type, args->NumDescriptors);
    return S_OK;
}

static void APIENTRY destroy_descriptor_heap(D3D12DDI_HDEVICE device, D3D12DDI_HDESCRIPTORHEAP handle)
{
    DdiDescriptorHeap *heap = handle.pDrvPrivate;
    DdiDevice *owner = device.pDrvPrivate;
    if (!heap || !owner || heap->device != owner)
        return;
    AcquireSRWLockExclusive(&owner->descriptor_lock);
    DdiDescriptorHeap **entry = &owner->descriptor_heaps;
    while (*entry && *entry != heap)
        entry = &(*entry)->next;
    if (*entry)
        *entry = heap->next;
    heap->device = NULL;
    ReleaseSRWLockExclusive(&owner->descriptor_lock);
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

static BOOL valid_descriptor_range(const DdiDevice *device, SIZE_T address, UINT count,
                                   D3D12DDI_DESCRIPTOR_HEAP_TYPE type, BOOL source)
{
    for (const DdiDescriptorHeap *heap = device->descriptor_heaps; heap; heap = heap->next) {
        const SIZE_T start = (SIZE_T)(heap + 1);
        if (heap->args.Type != type || !(heap->args.Flags & D3D12DDI_DESCRIPTOR_HEAP_FLAG_CPU_VISIBLE) ||
            (source && (heap->args.Flags & D3D12DDI_DESCRIPTOR_HEAP_FLAG_SHADER_VISIBLE)) || address < start)
            continue;
        const SIZE_T offset = address - start;
        const SIZE_T index = offset / sizeof(DdiDescriptor);
        if (!(offset % sizeof(DdiDescriptor)) && index < heap->args.NumDescriptors &&
            count <= heap->args.NumDescriptors - index)
            return TRUE;
    }
    return FALSE;
}

typedef struct DdiDescriptorRanges {
    UINT count;
    const D3D12DDI_CPU_DESCRIPTOR_HANDLE *starts;
    const UINT *sizes;
} DdiDescriptorRanges;

static UINT descriptor_range_size(DdiDescriptorRanges ranges, UINT i)
{
    return ranges.sizes ? ranges.sizes[i] : 1;
}

static BOOL count_descriptor_ranges(const DdiDevice *device, DdiDescriptorRanges ranges,
                                    D3D12DDI_DESCRIPTOR_HEAP_TYPE type, BOOL source, UINT64 *total)
{
    *total = 0;
    if (ranges.count && !ranges.starts)
        return FALSE;
    for (UINT i = 0; i < ranges.count; ++i) {
        const UINT size = descriptor_range_size(ranges, i);
        if (size && !valid_descriptor_range(device, ranges.starts[i].ptr, size, type, source))
            return FALSE;
        *total += size;
    }
    return TRUE;
}

static void copy_descriptor_ranges(DdiDescriptorRanges dst, DdiDescriptorRanges src, UINT64 total)
{
    UINT d = 0, s = 0, dst_offset = 0, src_offset = 0;
    while (total) {
        const UINT dst_count = descriptor_range_size(dst, d) - dst_offset;
        const UINT src_count = descriptor_range_size(src, s) - src_offset;
        if (!dst_count) {
            ++d;
            dst_offset = 0;
            continue;
        }
        if (!src_count) {
            ++s;
            src_offset = 0;
            continue;
        }
        const UINT count = dst_count < src_count ? dst_count : src_count;
        DdiDescriptor *target = (DdiDescriptor *)dst.starts[d].ptr + dst_offset;
        const DdiDescriptor *original = (const DdiDescriptor *)src.starts[s].ptr + src_offset;
        memmove(target, original, (SIZE_T)count * sizeof(*target));
        dst_offset += count;
        src_offset += count;
        total -= count;
    }
}

static void APIENTRY copy_descriptors(D3D12DDI_HDEVICE handle,
                                      UINT dst_count, const D3D12DDI_CPU_DESCRIPTOR_HANDLE *dst_starts, const UINT *dst_sizes,
                                      UINT src_count, const D3D12DDI_CPU_DESCRIPTOR_HANDLE *src_starts, const UINT *src_sizes,
                                      D3D12DDI_DESCRIPTOR_HEAP_TYPE type)
{
    DdiDevice *device = handle.pDrvPrivate;
    if (!device)
        return;
    const DdiDescriptorRanges dst = { dst_count, dst_starts, dst_sizes }, src = { src_count, src_starts, src_sizes };
    UINT64 dst_total = 0, src_total = 0;
    AcquireSRWLockShared(&device->descriptor_lock);
    const BOOL valid = count_descriptor_ranges(device, dst, type, FALSE, &dst_total) &&
                       count_descriptor_ranges(device, src, type, TRUE, &src_total) && dst_total == src_total;
    if (valid)
        copy_descriptor_ranges(dst, src, dst_total);
    ReleaseSRWLockShared(&device->descriptor_lock);
    trace_ddi("CopyDescriptors", valid ? 0 : (UINT)E_INVALIDARG, (SIZE_T)dst_total);
    if (!valid)
        set_command_device_error(device, E_INVALIDARG);
}

static void APIENTRY copy_descriptors_simple(D3D12DDI_HDEVICE device, UINT count,
                                             D3D12DDI_CPU_DESCRIPTOR_HANDLE dst, D3D12DDI_CPU_DESCRIPTOR_HANDLE src,
                                             D3D12DDI_DESCRIPTOR_HEAP_TYPE type)
{
    copy_descriptors(device, 1, &dst, &count, 1, &src, &count, type);
}
