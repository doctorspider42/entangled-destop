/* SPDX-License-Identifier: Apache-2.0 */
static const DdiDescriptor *graphics_descriptor_range(const DdiDrawState *draw, const DdiGraphicsBinding *binding)
{
    const DdiDescriptorHeap *heap = binding->type == VK_DESCRIPTOR_TYPE_SAMPLER ? draw->sampler_heap : draw->resource_heap;
    if (!heap || binding->parameter >= DDI_ROOT_PARAMETERS)
        return NULL;
    const UINT64 start = (UINT64)(UINT_PTR)(heap + 1);
    const UINT64 base = draw->tables[binding->parameter].ptr;
    if (base < start || (base - start) % sizeof(DdiDescriptor))
        return NULL;
    const UINT64 index = (base - start) / sizeof(DdiDescriptor) + binding->table_offset;
    return index < heap->args.NumDescriptors && binding->count <= heap->args.NumDescriptors - index
        ? (const DdiDescriptor *)(heap + 1) + index : NULL;
}

static HRESULT graphics_buffer_descriptor(DdiTransferList *list, const DdiGraphicsBinding *binding,
                                           const DdiDescriptor *descriptor, VkDescriptorBufferInfo *buffer)
{
    const VenusDevice *device = list->venus.device;
    if (binding->type == VK_DESCRIPTOR_TYPE_STORAGE_BUFFER)
        return descriptor && descriptor->kind == DDI_DESCRIPTOR_SRV && storage_srv_info(device, &descriptor->desc.srv, buffer)
            ? S_OK : E_INVALIDARG;

    D3D12DDI_GPU_VIRTUAL_ADDRESS address;
    UINT64 bytes = 0;
    if (binding->root_address) {
        address = list->draw.addresses[binding->parameter];
    } else {
        if (!descriptor || descriptor->kind != DDI_DESCRIPTOR_CBV)
            return E_INVALIDARG;
        address = descriptor->desc.cbv.BufferLocation;
        bytes = descriptor->desc.cbv.SizeInBytes;
    }
    if (!address || !resolve_buffer_address(list->device, address, bytes, buffer) ||
        buffer->range > device->limits.maxUniformBufferRange ||
        buffer->offset % device->limits.minUniformBufferOffsetAlignment)
        return E_INVALIDARG;
    return S_OK;
}

static HRESULT graphics_image_descriptor(DdiTransferList *list, VkDescriptorType type, const DdiDescriptor *descriptor,
                                          VkDescriptorImageInfo *info)
{
    if (type == VK_DESCRIPTOR_TYPE_SAMPLER)
        return descriptor->kind == DDI_DESCRIPTOR_SAMPLER
            ? create_venus_pixel_sampler(list, &descriptor->desc.sampler, &info->sampler) : E_INVALIDARG;
    if (descriptor->kind != DDI_DESCRIPTOR_SRV || descriptor->desc.srv.ResourceDimension != D3D12DDI_RD_TEXTURE2D)
        return E_INVALIDARG;
    const VenusColorImage *image = resource_color(list->venus.device, descriptor->desc.srv.hDrvResource);
    if (!image || !(image->usage & VK_IMAGE_USAGE_SAMPLED_BIT))
        return E_INVALIDARG;
    *info = (VkDescriptorImageInfo){ .imageView = image->view, .imageLayout = VK_IMAGE_LAYOUT_GENERAL };
    return S_OK;
}

static HRESULT populate_graphics_set(DdiTransferList *list, VkDescriptorSet set, VkWriteDescriptorSet *writes,
                                     VkDescriptorImageInfo *images, VkDescriptorBufferInfo *buffers, UINT *count)
{
    const DdiGraphicsLayout *layout = &list->draw.pipeline->graphics;
    *count = 0;
    for (UINT i = 0; i < layout->count; ++i) {
        const DdiGraphicsBinding *binding = &layout->bindings[i];
        const DdiDescriptor *descriptors = binding->root_address ? NULL : graphics_descriptor_range(&list->draw, binding);
        if (!binding->root_address && !descriptors)
            return E_INVALIDARG;
        const BOOL uses_image = binding->type == VK_DESCRIPTOR_TYPE_SAMPLED_IMAGE || binding->type == VK_DESCRIPTOR_TYPE_SAMPLER;
        for (UINT j = 0; j < binding->count; ++j) {
            const DdiDescriptor *descriptor = descriptors ? &descriptors[j] : NULL;
            if (binding->count > 1 && descriptor->kind == DDI_DESCRIPTOR_EMPTY)
                continue; /* Vulkan partially bound sampler array. */
            VkWriteDescriptorSet *write = &writes[*count];
            *write = (VkWriteDescriptorSet){ .sType = VK_STRUCTURE_TYPE_WRITE_DESCRIPTOR_SET,
                .dstSet = set, .dstBinding = i, .dstArrayElement = j, .descriptorCount = 1, .descriptorType = binding->type };
            HRESULT status;
            if (uses_image) {
                VkDescriptorImageInfo *info = &images[*count];
                *info = (VkDescriptorImageInfo){0};
                status = graphics_image_descriptor(list, binding->type, descriptor, info);
                write->pImageInfo = info;
            } else {
                status = graphics_buffer_descriptor(list, binding, descriptor, &buffers[i]);
                write->pBufferInfo = &buffers[i];
            }
            if (FAILED(status))
                return status;
            ++*count;
        }
    }
    return S_OK;
}

static HRESULT bind_graphics_set(DdiTransferList *list, const DdiPipeline *pipeline)
{
    const VenusDevice *device = list->venus.device;
    const PFN_vkUpdateDescriptorSets update = (PFN_vkUpdateDescriptorSets)
        device->proc_addr(device->handle, "vkUpdateDescriptorSets");
    const PFN_vkCmdBindDescriptorSets bind = (PFN_vkCmdBindDescriptorSets)
        device->proc_addr(device->handle, "vkCmdBindDescriptorSets");
    if (!update || !bind)
        return E_FAIL;

    UINT capacity = 0;
    for (UINT i = 0; i < pipeline->graphics.count; ++i)
        capacity += pipeline->graphics.bindings[i].count;
    VkWriteDescriptorSet small_writes[DDI_GRAPHICS_BINDINGS];
    VkDescriptorImageInfo small_images[DDI_GRAPHICS_BINDINGS];
    VkDescriptorBufferInfo buffers[DDI_GRAPHICS_BINDINGS];
    VkWriteDescriptorSet *writes = small_writes;
    VkDescriptorImageInfo *images = small_images;
    if (capacity > _countof(small_writes)) {
        writes = HeapAlloc(GetProcessHeap(), 0, capacity * (sizeof(*writes) + sizeof(*images)));
        if (!writes)
            return E_OUTOFMEMORY;
        images = (void *)(writes + capacity);
    }
    VkDescriptorSet set;
    HRESULT status = allocate_command_descriptors(list, pipeline->graphics_set, &set);
    UINT count = 0;
    if (SUCCEEDED(status))
        status = populate_graphics_set(list, set, writes, images, buffers, &count);
    if (SUCCEEDED(status)) {
        update(device->handle, count, count ? writes : NULL, 0, NULL);
        bind(list->venus.command, VK_PIPELINE_BIND_POINT_GRAPHICS, pipeline->layout, 0, 1, &set, 0, NULL);
    }
    if (writes != small_writes)
        HeapFree(GetProcessHeap(), 0, writes);
    return status;
}

static HRESULT bind_graphics_resources(DdiTransferList *list)
{
    const DdiPipeline *pipeline = list->draw.pipeline;
    const VenusDevice *device = list->venus.device;
    if (pipeline->graphics.constant_stages) {
        const PFN_vkCmdPushConstants push = (PFN_vkCmdPushConstants)device->proc_addr(device->handle, "vkCmdPushConstants");
        if (!push)
            return E_FAIL;
        push(list->venus.command, pipeline->layout, pipeline->graphics.constant_stages, 0,
             pipeline->graphics.constant_words * sizeof(UINT32), list->draw.constants);
    }
    if (!pipeline->graphics_set)
        return S_OK;
    const HRESULT status = bind_graphics_set(list, pipeline);
    trace_ddi("BindGraphicsResources", (UINT)status, pipeline->graphics.count);
    return status;
}
