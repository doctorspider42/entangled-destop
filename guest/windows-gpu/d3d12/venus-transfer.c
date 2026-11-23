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

HRESULT create_venus_transfer_buffer(const VenusDevice *device, VkDeviceSize size, BOOL readback,
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
    if (!create || !requirements || !allocate || !bind || !map ||
        !DEVICE_FN(device, vkDestroyBuffer) || !DEVICE_FN(device, vkFreeMemory) ||
        !DEVICE_FN(device, vkUnmapMemory))
        return E_FAIL;

    const VkBufferCreateInfo info = {
        .sType = VK_STRUCTURE_TYPE_BUFFER_CREATE_INFO,
        .size = size,
        .usage = VK_BUFFER_USAGE_TRANSFER_SRC_BIT | VK_BUFFER_USAGE_TRANSFER_DST_BIT,
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

    const VkMemoryAllocateInfo allocation = {
        .sType = VK_STRUCTURE_TYPE_MEMORY_ALLOCATE_INFO,
        .allocationSize = req.size,
        .memoryTypeIndex = type,
    };
    VkDeviceMemory memory;
    result = allocate(device->handle, &allocation, NULL, &memory);
    if (result == VK_SUCCESS) {
        buffer->memory = memory;
        result = bind(device->handle, handle, memory, 0);
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
static HRESULT execute_probe_copy(const VenusDevice *device, const VenusTransferBuffer *dst,
                                  const VenusTransferBuffer *src, VkBufferCopy region)
{
    const PFN_vkCreateCommandPool create_pool = DEVICE_FN(device, vkCreateCommandPool);
    const PFN_vkDestroyCommandPool destroy_pool = DEVICE_FN(device, vkDestroyCommandPool);
    const PFN_vkAllocateCommandBuffers allocate = DEVICE_FN(device, vkAllocateCommandBuffers);
    const PFN_vkBeginCommandBuffer begin = DEVICE_FN(device, vkBeginCommandBuffer);
    const PFN_vkEndCommandBuffer end = DEVICE_FN(device, vkEndCommandBuffer);
    const PFN_vkCmdPipelineBarrier barrier = DEVICE_FN(device, vkCmdPipelineBarrier);
    const PFN_vkCreateFence create_fence = DEVICE_FN(device, vkCreateFence);
    const PFN_vkDestroyFence destroy_fence = DEVICE_FN(device, vkDestroyFence);
    const PFN_vkQueueSubmit submit = DEVICE_FN(device, vkQueueSubmit);
    const PFN_vkWaitForFences wait = DEVICE_FN(device, vkWaitForFences);
    const PFN_vkQueueWaitIdle idle = DEVICE_FN(device, vkQueueWaitIdle);
    if (!create_pool || !destroy_pool || !allocate || !begin || !end || !barrier ||
        !create_fence || !destroy_fence || !submit || !wait || !idle)
        return E_FAIL;

    const VkCommandPoolCreateInfo pool_info = {
        .sType = VK_STRUCTURE_TYPE_COMMAND_POOL_CREATE_INFO,
        .flags = VK_COMMAND_POOL_CREATE_TRANSIENT_BIT,
        .queueFamilyIndex = device->graphics_queue_family,
    };
    VkCommandPool pool;
    VkResult result = create_pool(device->handle, &pool_info, NULL, &pool);
    if (result != VK_SUCCESS)
        return venus_hresult(result);

    const VkCommandBufferAllocateInfo command_info = {
        .sType = VK_STRUCTURE_TYPE_COMMAND_BUFFER_ALLOCATE_INFO,
        .commandPool = pool,
        .level = VK_COMMAND_BUFFER_LEVEL_PRIMARY,
        .commandBufferCount = 1,
    };
    const VkCommandBufferBeginInfo begin_info = {
        .sType = VK_STRUCTURE_TYPE_COMMAND_BUFFER_BEGIN_INFO,
        .flags = VK_COMMAND_BUFFER_USAGE_ONE_TIME_SUBMIT_BIT,
    };
    VkCommandBuffer command;
    result = allocate(device->handle, &command_info, &command);
    if (result == VK_SUCCESS)
        result = begin(command, &begin_info);
    HRESULT status = venus_hresult(result);
    if (FAILED(status))
        goto release_pool;

    status = record_venus_buffer_copy(command, dst, src, region);
    if (FAILED(status))
        goto release_pool;

    const VkBufferMemoryBarrier readback = {
        .sType = VK_STRUCTURE_TYPE_BUFFER_MEMORY_BARRIER,
        .srcAccessMask = VK_ACCESS_TRANSFER_WRITE_BIT,
        .dstAccessMask = VK_ACCESS_HOST_READ_BIT,
        .srcQueueFamilyIndex = VK_QUEUE_FAMILY_IGNORED,
        .dstQueueFamilyIndex = VK_QUEUE_FAMILY_IGNORED,
        .buffer = dst->handle,
        .offset = region.dstOffset,
        .size = region.size,
    };
    barrier(command, VK_PIPELINE_STAGE_TRANSFER_BIT, VK_PIPELINE_STAGE_HOST_BIT, 0,
            0, NULL, 1, &readback, 0, NULL);
    status = venus_hresult(end(command));
    if (FAILED(status))
        goto release_pool;

    const VkFenceCreateInfo fence_info = { .sType = VK_STRUCTURE_TYPE_FENCE_CREATE_INFO };
    VkFence fence;
    status = venus_hresult(create_fence(device->handle, &fence_info, NULL, &fence));
    if (FAILED(status))
        goto release_pool;
    const VkSubmitInfo submission = {
        .sType = VK_STRUCTURE_TYPE_SUBMIT_INFO,
        .commandBufferCount = 1,
        .pCommandBuffers = &command,
    };
    /* The probe runs before CreateDevice gives the queue to any other caller. */
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
    status = venus_hresult(result);
    destroy_fence(device->handle, fence, NULL);

release_pool:
    destroy_pool(device->handle, pool, NULL);
    return status;
}

HRESULT probe_venus_buffer_copy(const VenusDevice *device)
{
    const VkBufferCopy region = { .srcOffset = 16, .dstOffset = 32, .size = 64 * 1024 };
    const VkDeviceSize size = region.dstOffset + region.size + 16;
    VenusTransferBuffer upload = {0}, readback = {0};
    HRESULT status = create_venus_transfer_buffer(device, size, FALSE, &upload);
    if (SUCCEEDED(status))
        status = create_venus_transfer_buffer(device, size, TRUE, &readback);
    if (SUCCEEDED(status)) {
        BYTE *src = upload.mapped;
        BYTE *dst = readback.mapped;
        for (VkDeviceSize i = 0; i < size; ++i) {
            src[i] = (BYTE)(i * 31 + (i >> 8));
            dst[i] = 0xfa;
        }
        status = execute_probe_copy(device, &readback, &upload, region);
        if (SUCCEEDED(status)) {
            for (VkDeviceSize i = 0; i < size; ++i) {
                const BYTE expected = i >= region.dstOffset && i - region.dstOffset < region.size
                    ? src[i - region.dstOffset + region.srcOffset] : 0xfa;
                if (dst[i] != expected) {
                    status = HRESULT_FROM_WIN32(ERROR_CRC);
                    break;
                }
            }
        }
    }
    destroy_venus_transfer_buffer(&readback);
    destroy_venus_transfer_buffer(&upload);
    return status;
}
#endif
