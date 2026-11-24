/* SPDX-License-Identifier: Apache-2.0 */
static BOOL storage_buffer_info(const VenusDevice *device, D3D12DDI_HRESOURCE resource,
                                DXGI_FORMAT format, BOOL raw, UINT stride, UINT64 first, UINT count,
                                VkDescriptorBufferInfo *info)
{
    const VenusTransferBuffer *buffer = resource_buffer(device, resource);
    if (!buffer || !(buffer->usage & VK_BUFFER_USAGE_STORAGE_BUFFER_BIT) || !count)
        return FALSE;
    if (raw) {
        if (format != DXGI_FORMAT_R32_TYPELESS || stride)
            return FALSE;
        stride = 4;
    } else if (format != DXGI_FORMAT_UNKNOWN || !stride || stride % 4) {
        return FALSE;
    }
    if (first > buffer->size / stride)
        return FALSE;
    const VkDeviceSize offset = first * stride;
    const VkDeviceSize size = (UINT64)count * stride;
    const VkDeviceSize alignment = device->limits.minStorageBufferOffsetAlignment;
    if (!alignment || (buffer->offset + offset) % alignment || size > buffer->size - offset ||
        size > device->limits.maxStorageBufferRange)
        return FALSE;
    *info = (VkDescriptorBufferInfo){ .buffer = buffer->handle, .offset = buffer->offset + offset, .range = size };
    return TRUE;
}

static BOOL storage_srv_info(const VenusDevice *device, const D3D12DDIARG_CREATE_SHADER_RESOURCE_VIEW_0002 *args,
                             VkDescriptorBufferInfo *info)
{
    const D3D12DDIARG_BUFFER_SHADER_RESOURCE_VIEW *view = &args->Buffer;
    return args->ResourceDimension == D3D12DDI_RD_BUFFER &&
        (view->Flags == D3D12DDI_BUFFER_SRV_FLAG_NONE || view->Flags == D3D12DDI_BUFFER_SRV_FLAG_RAW) &&
        args->Shader4ComponentMapping == D3D12DDI_DEFAULT_SHADER_4_COMPONENT_MAPPING &&
        storage_buffer_info(device, args->hDrvResource, args->Format, view->Flags == D3D12DDI_BUFFER_SRV_FLAG_RAW,
                            view->StructureByteStride, view->FirstElement, view->NumElements, info);
}

static BOOL storage_uav_info(const VenusDevice *device, const D3D12DDIARG_CREATE_UNORDERED_ACCESS_VIEW_0002 *args,
                             VkDescriptorBufferInfo *info)
{
    const D3D12DDIARG_BUFFER_UNORDERED_ACCESS_VIEW *view = &args->Buffer;
    return args->ResourceDimension == D3D12DDI_RD_BUFFER &&
        (view->Flags == D3D12DDI_BUFFER_UAV_FLAG_NONE || view->Flags == D3D12DDI_BUFFER_UAV_FLAG_RAW) &&
        !view->hDrvCounterResource.pDrvPrivate && !view->CounterOffsetInBytes &&
        storage_buffer_info(device, args->hDrvResource, args->Format, view->Flags == D3D12DDI_BUFFER_UAV_FLAG_RAW,
                            view->StructureByteStride, view->FirstElement, view->NumElements, info);
}

static void APIENTRY create_shader_srv(D3D12DDI_HDEVICE device, const D3D12DDIARG_CREATE_SHADER_RESOURCE_VIEW_0002 *args,
                                       D3D12DDI_CPU_DESCRIPTOR_HANDLE destination)
{
    if (!args || args->ResourceDimension != D3D12DDI_RD_BUFFER) {
        create_sampled_srv(device, args, destination);
        return;
    }
    DdiDescriptor *descriptor = (DdiDescriptor *)destination.ptr;
    VkDescriptorBufferInfo buffer;
    if (!descriptor || !storage_srv_info(venus_device(device), args, &buffer)) {
        set_command_device_error(device.pDrvPrivate, DXGI_ERROR_UNSUPPORTED);
        return;
    }
    *descriptor = (DdiDescriptor){ .kind = DDI_DESCRIPTOR_SRV, .desc.srv = *args };
    trace_ddi("CreateStorageSrv", 0, buffer.range);
}

static void APIENTRY create_storage_uav(D3D12DDI_HDEVICE device,
                                       const D3D12DDIARG_CREATE_UNORDERED_ACCESS_VIEW_0002 *args,
                                       D3D12DDI_CPU_DESCRIPTOR_HANDLE destination)
{
    DdiDescriptor *descriptor = (DdiDescriptor *)destination.ptr;
    VkDescriptorBufferInfo buffer;
    if (!descriptor || !args || !storage_uav_info(venus_device(device), args, &buffer)) {
        set_command_device_error(device.pDrvPrivate, DXGI_ERROR_UNSUPPORTED);
        return;
    }
    *descriptor = (DdiDescriptor){ .kind = DDI_DESCRIPTOR_UAV, .desc.uav = *args };
    trace_ddi("CreateStorageUav", 0, buffer.range);
}

static void APIENTRY set_compute_root(D3D12DDI_HCOMMANDLIST handle, D3D12DDI_HROOTSIGNATURE signature)
{
    DdiTransferList *list = recording_command_list(handle);
    if (!list || list->compute.root == signature.pDrvPrivate)
        return;
    list->compute = (DdiComputeState){ .root = signature.pDrvPrivate, .pipeline = list->compute.pipeline };
}

static void APIENTRY set_compute_table(D3D12DDI_HCOMMANDLIST handle, UINT index, D3D12DDI_GPU_DESCRIPTOR_HANDLE base)
{
    DdiTransferList *list = recording_command_list(handle);
    if (!list)
        return;
    const DdiRootSignature *root = list->compute.root;
    if (!root || index >= root->desc.NumParameters || index >= DDI_ROOT_PARAMETERS ||
        root->desc.pRootParameters[index].ParameterType != D3D12DDI_ROOT_PARAMETER_TYPE_DESCRIPTOR_TABLE) {
        set_command_list_status(list, E_INVALIDARG);
        return;
    }
    list->compute.tables[index] = base;
}

static void APIENTRY set_compute_constants(D3D12DDI_HCOMMANDLIST handle, UINT index, UINT count,
                                           const void *data, UINT offset)
{
    DdiTransferList *list = recording_command_list(handle);
    if (!list || !count)
        return;
    const DdiRootSignature *root = list->compute.root;
    if (!root || index >= root->desc.NumParameters || !data) {
        set_command_list_status(list, E_INVALIDARG);
        return;
    }
    const D3D12DDI_ROOT_PARAMETER_0013 *parameters = root->desc.pRootParameters;
    const D3D12DDI_ROOT_PARAMETER_0013 *param = &parameters[index];
    UINT start = 0;
    for (UINT i = 0; i < index; ++i) {
        if (parameters[i].ParameterType == D3D12DDI_ROOT_PARAMETER_TYPE_32BIT_CONSTANTS &&
            parameters[i].ShaderVisibility == D3D12DDI_SHADER_VISIBILITY_ALL)
            start += parameters[i].Constants.Num32BitValues;
    }
    if (param->ParameterType != D3D12DDI_ROOT_PARAMETER_TYPE_32BIT_CONSTANTS ||
        param->ShaderVisibility != D3D12DDI_SHADER_VISIBILITY_ALL ||
        offset > param->Constants.Num32BitValues || count > param->Constants.Num32BitValues - offset ||
        start + offset > DDI_COMPUTE_CONSTANT_WORDS || count > DDI_COMPUTE_CONSTANT_WORDS - start - offset) {
        set_command_list_status(list, E_INVALIDARG);
        return;
    }
    memcpy(&list->compute.constants[start + offset], data, count * sizeof(UINT32));
}

static void APIENTRY set_compute_constant(D3D12DDI_HCOMMANDLIST handle, UINT index, UINT data, UINT offset)
{
    set_compute_constants(handle, index, 1, &data, offset);
}

static const DdiDescriptor *compute_buffer_descriptor(const DdiTransferList *list, const DdiComputeBufferBinding *binding)
{
    const DdiDescriptorHeap *heap = list->draw.resource_heap;
    if (!heap || binding->parameter >= DDI_ROOT_PARAMETERS)
        return NULL;
    const UINT64 start = (UINT64)(UINT_PTR)(heap + 1);
    const UINT64 base = list->compute.tables[binding->parameter].ptr;
    if (base < start || (base - start) % sizeof(DdiDescriptor))
        return NULL;
    const UINT64 index = (base - start) / sizeof(DdiDescriptor) + binding->table_offset;
    return index < heap->args.NumDescriptors ? (const DdiDescriptor *)(heap + 1) + index : NULL;
}

static HRESULT bind_compute_buffers(DdiTransferList *list, const DdiPipeline *pipeline)
{
    const VenusDevice *device = list->venus.device;
    const PFN_vkUpdateDescriptorSets update = (PFN_vkUpdateDescriptorSets)
        device->proc_addr(device->handle, "vkUpdateDescriptorSets");
    const PFN_vkCmdBindDescriptorSets bind = (PFN_vkCmdBindDescriptorSets)
        device->proc_addr(device->handle, "vkCmdBindDescriptorSets");
    if (!update || !bind)
        return E_FAIL;

    const UINT count = pipeline->compute.buffer_count;
    VkDescriptorBufferInfo buffers[DDI_COMPUTE_BUFFER_BINDINGS];
    for (UINT i = 0; i < count; ++i) {
        const DdiComputeBufferBinding *binding = &pipeline->compute.buffers[i];
        const DdiDescriptor *descriptor = compute_buffer_descriptor(list, binding);
        if (!descriptor)
            return E_INVALIDARG;
        const BOOL valid = binding->type == D3D12DDI_DESCRIPTOR_RANGE_TYPE_SRV
            ? descriptor->kind == DDI_DESCRIPTOR_SRV && storage_srv_info(device, &descriptor->desc.srv, &buffers[i])
            : descriptor->kind == DDI_DESCRIPTOR_UAV && storage_uav_info(device, &descriptor->desc.uav, &buffers[i]);
        if (!valid)
            return E_INVALIDARG;
    }
    VkDescriptorSet set;
    const HRESULT status = allocate_command_descriptors(list, pipeline->compute_set, &set);
    if (FAILED(status))
        return status;
    VkWriteDescriptorSet writes[DDI_COMPUTE_BUFFER_BINDINGS];
    for (UINT i = 0; i < count; ++i) {
        writes[i] = (VkWriteDescriptorSet){ .sType = VK_STRUCTURE_TYPE_WRITE_DESCRIPTOR_SET,
            .dstSet = set, .dstBinding = i, .descriptorCount = 1, .descriptorType = VK_DESCRIPTOR_TYPE_STORAGE_BUFFER,
            .pBufferInfo = &buffers[i] };
    }
    update(device->handle, count, writes, 0, NULL);
    bind(list->venus.command, VK_PIPELINE_BIND_POINT_COMPUTE, pipeline->layout, 0, 1, &set, 0, NULL);
    return S_OK;
}

static void APIENTRY dispatch_compute(D3D12DDI_HCOMMANDLIST handle, UINT x, UINT y, UINT z)
{
    DdiTransferList *list = recording_command_list(handle);
    if (!list || !x || !y || !z)
        return;
    const VenusDevice *device = list->venus.device;
    const DdiPipeline *pipeline = list->compute.pipeline;
    const UINT32 *limit = device->limits.maxComputeWorkGroupCount;
    if (!pipeline || !list->compute.root || x > limit[0] || y > limit[1] || z > limit[2] ||
        compute_constant_words(list->compute.root) != pipeline->compute.constant_words) {
        set_command_list_status(list, E_INVALIDARG);
        return;
    }
    const PFN_vkCmdDispatch dispatch = (PFN_vkCmdDispatch)device->proc_addr(device->handle, "vkCmdDispatch");
    const PFN_vkCmdPushConstants push = (PFN_vkCmdPushConstants)device->proc_addr(device->handle, "vkCmdPushConstants");
    if (!dispatch || (pipeline->compute.constant_words && !push)) {
        set_command_list_status(list, E_FAIL);
        return;
    }
    AcquireSRWLockExclusive(&list->pool->lock);
    HRESULT status = list->generation == list->pool->generation ? S_OK : E_INVALIDARG;
    if (SUCCEEDED(status) && pipeline->compute.buffer_count)
        status = bind_compute_buffers(list, pipeline);
    if (SUCCEEDED(status)) {
        if (pipeline->compute.constant_words)
            push(list->venus.command, pipeline->layout, VK_SHADER_STAGE_COMPUTE_BIT, 0,
                 pipeline->compute.constant_words * sizeof(UINT32), list->compute.constants);
        dispatch(list->venus.command, x, y, z);
    }
    ReleaseSRWLockExclusive(&list->pool->lock);
    trace_ddi("Dispatch", (UINT)status, x);
    set_command_list_status(list, status);
}
