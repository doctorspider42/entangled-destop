/* SPDX-License-Identifier: Apache-2.0 */
#include "venus-transfer.h"
#include <dxgi.h>

#define DEVICE_FN(device, name) ((PFN_##name)(device)->proc_addr((device)->handle, #name))

static UINT32 transfer_memory_type(const VkPhysicalDeviceMemoryProperties *memory, UINT32 type_bits,
                                   BOOL readback)
{
    const VkMemoryPropertyFlags required = VK_MEMORY_PROPERTY_HOST_VISIBLE_BIT |
                                           VK_MEMORY_PROPERTY_HOST_COHERENT_BIT;
    UINT32 fallback = UINT32_MAX;
    for (UINT32 i = 0; i < memory->memoryTypeCount; ++i) {
        const VkMemoryPropertyFlags flags = memory->memoryTypes[i].propertyFlags;
        if (!(type_bits & (1u << i)) || (flags & required) != required)
            continue;
        if (!readback || (flags & VK_MEMORY_PROPERTY_HOST_CACHED_BIT))
            return i;
        if (fallback == UINT32_MAX)
            fallback = i;
    }
    return fallback;
}

HRESULT create_venus_transfer_buffer(const VenusDevice *device, VkDeviceSize size, BOOL readback, VkBufferUsageFlags uses,
                                     VenusTransferBuffer *buffer)
{
    *buffer = (VenusTransferBuffer){0};
    if (!size)
        return E_INVALIDARG;

    const PFN_vkCreateBuffer create = DEVICE_FN(device, vkCreateBuffer);
    const PFN_vkGetBufferMemoryRequirements requirements = DEVICE_FN(device, vkGetBufferMemoryRequirements);
    const PFN_vkAllocateMemory allocate = DEVICE_FN(device, vkAllocateMemory);
    const PFN_vkBindBufferMemory bind = DEVICE_FN(device, vkBindBufferMemory);
    const PFN_vkMapMemory map = DEVICE_FN(device, vkMapMemory);
    const PFN_vkGetBufferDeviceAddress address = device->buffer_device_address
        ? DEVICE_FN(device, vkGetBufferDeviceAddress) : NULL;
    if (!create || !requirements || !allocate || !bind || !map ||
        (device->buffer_device_address && !address) ||
        !DEVICE_FN(device, vkDestroyBuffer) || !DEVICE_FN(device, vkFreeMemory) ||
        !DEVICE_FN(device, vkUnmapMemory))
        return E_FAIL;

    const VkBufferCreateInfo info = {
        .sType = VK_STRUCTURE_TYPE_BUFFER_CREATE_INFO,
        .size = size,
        .usage = uses | VK_BUFFER_USAGE_TRANSFER_SRC_BIT | VK_BUFFER_USAGE_TRANSFER_DST_BIT |
            (address ? VK_BUFFER_USAGE_SHADER_DEVICE_ADDRESS_BIT : 0),
        .sharingMode = VK_SHARING_MODE_EXCLUSIVE,
    };
    VkBuffer handle;
    VkResult result = create(device->handle, &info, NULL, &handle);
    if (result != VK_SUCCESS)
        return venus_hresult(result);
    buffer->device = device;
    buffer->handle = handle;
    buffer->size = size;

    VkMemoryRequirements req;
    requirements(device->handle, handle, &req);
    const UINT32 type = transfer_memory_type(&device->memory, req.memoryTypeBits, readback);
    if (type == UINT32_MAX) {
        destroy_venus_transfer_buffer(buffer);
        return DXGI_ERROR_UNSUPPORTED;
    }

    const VkMemoryAllocateFlagsInfo flags = {
        .sType = VK_STRUCTURE_TYPE_MEMORY_ALLOCATE_FLAGS_INFO,
        .flags = VK_MEMORY_ALLOCATE_DEVICE_ADDRESS_BIT,
    };
    const VkMemoryAllocateInfo allocation = {
        .sType = VK_STRUCTURE_TYPE_MEMORY_ALLOCATE_INFO,
        .pNext = address ? &flags : NULL,
        .allocationSize = req.size,
        .memoryTypeIndex = type,
    };
    VkDeviceMemory memory;
    result = allocate(device->handle, &allocation, NULL, &memory);
    if (result == VK_SUCCESS) {
        buffer->memory = memory;
        result = bind(device->handle, handle, memory, 0);
    }
    if (result == VK_SUCCESS && address) {
        const VkBufferDeviceAddressInfo address_info = {
            .sType = VK_STRUCTURE_TYPE_BUFFER_DEVICE_ADDRESS_INFO,
            .buffer = handle,
        };
        buffer->address = address(device->handle, &address_info);
        if (!buffer->address) {
            destroy_venus_transfer_buffer(buffer);
            return E_FAIL;
        }
    }
    if (result == VK_SUCCESS) {
        void *mapped;
        result = map(device->handle, memory, 0, VK_WHOLE_SIZE, 0, &mapped);
        if (result == VK_SUCCESS)
            buffer->mapped = mapped;
    }
    if (result != VK_SUCCESS)
        destroy_venus_transfer_buffer(buffer);
    return venus_hresult(result);
}

void destroy_venus_transfer_buffer(VenusTransferBuffer *buffer)
{
    const VenusDevice *device = buffer->device;
    if (!device)
        return;
    if (buffer->mapped)
        DEVICE_FN(device, vkUnmapMemory)(device->handle, buffer->memory);
    DEVICE_FN(device, vkDestroyBuffer)(device->handle, buffer->handle, NULL);
    if (buffer->memory)
        DEVICE_FN(device, vkFreeMemory)(device->handle, buffer->memory, NULL);
    *buffer = (VenusTransferBuffer){0};
}

HRESULT record_venus_buffer_copy(VkCommandBuffer command, const VenusTransferBuffer *dst,
                                 const VenusTransferBuffer *src, VkBufferCopy region)
{
    if (!command || !dst || !src || !dst->handle || !src->handle ||
        dst->device != src->device || !region.size ||
        region.srcOffset >= src->size || region.size > src->size - region.srcOffset ||
        region.dstOffset >= dst->size || region.size > dst->size - region.dstOffset)
        return E_INVALIDARG;

    if (src->handle == dst->handle && region.srcOffset < region.dstOffset + region.size &&
        region.dstOffset < region.srcOffset + region.size)
        return E_INVALIDARG;
    const PFN_vkCmdCopyBuffer copy = DEVICE_FN(src->device, vkCmdCopyBuffer);
    if (!copy)
        return E_FAIL;
    copy(command, src->handle, dst->handle, 1, &region);
    return S_OK;
}

#ifdef ENTANGLED_D3D12_PROBE_COPY
HRESULT create_venus_command_pool(const VenusDevice *device, VkCommandPoolCreateFlags flags, VkCommandPool *pool)
{
    const PFN_vkCreateCommandPool create = DEVICE_FN(device, vkCreateCommandPool);
    if (!create || !DEVICE_FN(device, vkDestroyCommandPool) || !DEVICE_FN(device, vkResetCommandPool))
        return E_FAIL;
    const VkCommandPoolCreateInfo info = {
        .sType = VK_STRUCTURE_TYPE_COMMAND_POOL_CREATE_INFO,
        .flags = flags,
        .queueFamilyIndex = device->graphics_queue_family,
    };
    return venus_hresult(create(device->handle, &info, NULL, pool));
}

HRESULT reset_venus_command_pool(const VenusDevice *device, VkCommandPool pool)
{
    return venus_hresult(DEVICE_FN(device, vkResetCommandPool)(device->handle, pool, 0));
}

void destroy_venus_command_pool(const VenusDevice *device, VkCommandPool pool)
{
    DEVICE_FN(device, vkDestroyCommandPool)(device->handle, pool, NULL);
}

HRESULT begin_venus_command_buffer(const VenusDevice *device, VkCommandPool pool,
                                   VkCommandBufferUsageFlags flags, VkCommandBuffer *command)
{
    const PFN_vkAllocateCommandBuffers allocate = DEVICE_FN(device, vkAllocateCommandBuffers);
    const PFN_vkBeginCommandBuffer begin = DEVICE_FN(device, vkBeginCommandBuffer);
    if (!allocate || !begin)
        return E_FAIL;

    if (!*command) {
        const VkCommandBufferAllocateInfo info = {
            .sType = VK_STRUCTURE_TYPE_COMMAND_BUFFER_ALLOCATE_INFO,
            .commandPool = pool,
            .level = VK_COMMAND_BUFFER_LEVEL_PRIMARY,
            .commandBufferCount = 1,
        };
        VkCommandBuffer allocated;
        VkResult result = allocate(device->handle, &info, &allocated);
        if (result != VK_SUCCESS)
            return venus_hresult(result);
        *command = allocated;
    }
    const VkCommandBufferBeginInfo info = {
        .sType = VK_STRUCTURE_TYPE_COMMAND_BUFFER_BEGIN_INFO,
        .flags = flags,
    };
    return venus_hresult(begin(*command, &info));
}

HRESULT begin_venus_transfer_list(const VenusDevice *device, VenusTransferList *list)
{
    *list = (VenusTransferList){0};
    VkCommandPool pool;
    HRESULT status = create_venus_command_pool(device, VK_COMMAND_POOL_CREATE_TRANSIENT_BIT, &pool);
    if (FAILED(status))
        return status;
    list->device = device;
    list->pool = pool;
    status = begin_venus_command_buffer(device, pool, VK_COMMAND_BUFFER_USAGE_ONE_TIME_SUBMIT_BIT, &list->command);
    if (FAILED(status))
        destroy_venus_transfer_list(list);
    return status;
}

HRESULT close_venus_transfer_list(const VenusTransferList *list)
{
    const VenusDevice *device = list->device;
    const PFN_vkEndCommandBuffer end = DEVICE_FN(device, vkEndCommandBuffer);
    const PFN_vkCmdPipelineBarrier barrier = DEVICE_FN(device, vkCmdPipelineBarrier);
    if (!end || !barrier)
        return E_FAIL;
    const VkMemoryBarrier readback = {
        .sType = VK_STRUCTURE_TYPE_MEMORY_BARRIER,
        .srcAccessMask = VK_ACCESS_TRANSFER_WRITE_BIT,
        .dstAccessMask = VK_ACCESS_HOST_READ_BIT,
    };
    barrier(list->command, VK_PIPELINE_STAGE_TRANSFER_BIT, VK_PIPELINE_STAGE_HOST_BIT, 0,
            1, &readback, 0, NULL, 0, NULL);
    return venus_hresult(end(list->command));
}

HRESULT submit_venus_transfer_commands(const VenusDevice *device, UINT count, const VkCommandBuffer *commands)
{
    const PFN_vkCreateFence create_fence = DEVICE_FN(device, vkCreateFence);
    const PFN_vkDestroyFence destroy_fence = DEVICE_FN(device, vkDestroyFence);
    const PFN_vkQueueSubmit submit = DEVICE_FN(device, vkQueueSubmit);
    const PFN_vkWaitForFences wait = DEVICE_FN(device, vkWaitForFences);
    const PFN_vkQueueWaitIdle idle = DEVICE_FN(device, vkQueueWaitIdle);
    if (!create_fence || !destroy_fence || !submit || !wait || !idle)
        return E_FAIL;
    const VkFenceCreateInfo fence_info = { .sType = VK_STRUCTURE_TYPE_FENCE_CREATE_INFO };
    VkFence fence;
    VkResult result = create_fence(device->handle, &fence_info, NULL, &fence);
    if (result != VK_SUCCESS)
        return venus_hresult(result);
    const VkSubmitInfo submission = {
        .sType = VK_STRUCTURE_TYPE_SUBMIT_INFO,
        .commandBufferCount = count,
        .pCommandBuffers = commands,
    };
    /* The caller serializes access to the queue. */
    result = submit(device->graphics_queue, 1, &submission, fence);
    if (result == VK_SUCCESS) {
        result = wait(device->handle, 1, &fence, VK_TRUE, 5 * UINT64_C(1000000000));
        if (result != VK_SUCCESS && result != VK_ERROR_DEVICE_LOST) {
            /* The queue can still own the fence, command pool and buffers. */
            VkResult drained;
            do {
                drained = idle(device->graphics_queue);
            } while (drained != VK_SUCCESS && drained != VK_ERROR_DEVICE_LOST);
        }
    }
    destroy_fence(device->handle, fence, NULL);
    return venus_hresult(result);
}

HRESULT submit_venus_transfer_probe(const VenusTransferList *list)
{
    /* Device initialization has not yet shared the queue with other callers. */
    return submit_venus_transfer_commands(list->device, 1, &list->command);
}

void destroy_venus_transfer_list(VenusTransferList *list)
{
    const VenusDevice *device = list->device;
    if (device)
        destroy_venus_command_pool(device, list->pool);
    *list = (VenusTransferList){0};
}
#endif
