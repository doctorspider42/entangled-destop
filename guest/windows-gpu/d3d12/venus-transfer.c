/* SPDX-License-Identifier: Apache-2.0 */
#include "venus-transfer.h"
#include <d3d12.h>
#include <dxgi.h>

#define DEVICE_FN(device, name) ((PFN_##name)(device)->proc_addr((device)->handle, #name))

static UINT32 buffer_memory_type(const VkPhysicalDeviceMemoryProperties *memory, UINT32 type_bits,
                                  VenusBufferMemory placement)
{
    const VkMemoryPropertyFlags required = placement == VENUS_BUFFER_DEVICE_LOCAL
        ? VK_MEMORY_PROPERTY_DEVICE_LOCAL_BIT
        : VK_MEMORY_PROPERTY_HOST_VISIBLE_BIT | VK_MEMORY_PROPERTY_HOST_COHERENT_BIT;
    UINT32 fallback = UINT32_MAX;
    for (UINT32 i = 0; i < memory->memoryTypeCount; ++i) {
        const VkMemoryPropertyFlags flags = memory->memoryTypes[i].propertyFlags;
        if (!(type_bits & (1u << i)) || (flags & required) != required)
            continue;
        if (placement == VENUS_BUFFER_UPLOAD ||
            (placement == VENUS_BUFFER_READBACK && (flags & VK_MEMORY_PROPERTY_HOST_CACHED_BIT)) ||
            (placement == VENUS_BUFFER_DEVICE_LOCAL && !(flags & VK_MEMORY_PROPERTY_HOST_VISIBLE_BIT)))
            return i;
        if (fallback == UINT32_MAX)
            fallback = i;
    }
    return fallback;
}

HRESULT create_venus_transfer_buffer(const VenusDevice *device, VkDeviceSize size, VenusBufferMemory placement,
                                     VkBufferUsageFlags uses, VenusTransferBuffer *buffer)
{
    *buffer = (VenusTransferBuffer){0};
    if (!size || size > UINT64_MAX - 3)
        return E_INVALIDARG;

    const PFN_vkCreateBuffer create = DEVICE_FN(device, vkCreateBuffer);
    const PFN_vkGetBufferMemoryRequirements requirements = DEVICE_FN(device, vkGetBufferMemoryRequirements);
    const PFN_vkAllocateMemory allocate = DEVICE_FN(device, vkAllocateMemory);
    const PFN_vkBindBufferMemory bind = DEVICE_FN(device, vkBindBufferMemory);
    const BOOL cpu_visible = placement != VENUS_BUFFER_DEVICE_LOCAL;
    const PFN_vkMapMemory map = cpu_visible ? DEVICE_FN(device, vkMapMemory) : NULL;
    const PFN_vkGetBufferDeviceAddress address = device->buffer_device_address
        ? DEVICE_FN(device, vkGetBufferDeviceAddress) : NULL;
    if (!create || !requirements || !allocate || !bind ||
        (device->buffer_device_address && !address) ||
        !DEVICE_FN(device, vkDestroyBuffer) || !DEVICE_FN(device, vkFreeMemory) ||
        (cpu_visible && (!map || !DEVICE_FN(device, vkUnmapMemory))))
        return E_FAIL;

    const VkBufferCreateInfo info = {
        .sType = VK_STRUCTURE_TYPE_BUFFER_CREATE_INFO,
        .size = cpu_visible ? size : (size + 3) & ~UINT64_C(3),
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
    buffer->usage = info.usage;

    VkMemoryRequirements req;
    requirements(device->handle, handle, &req);
    const UINT32 type = buffer_memory_type(&device->memory, req.memoryTypeBits, placement);
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
    if (result == VK_SUCCESS && map) {
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

    region.srcOffset += src->offset;
    region.dstOffset += dst->offset;
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
HRESULT record_venus_buffer_barrier(VkCommandBuffer command, const VenusTransferBuffer *buffer,
                                    VkAccessFlags before, VkAccessFlags after)
{
    const PFN_vkCmdPipelineBarrier barrier = DEVICE_FN(buffer->device, vkCmdPipelineBarrier);
    if (!barrier)
        return E_FAIL;
    const VkBufferMemoryBarrier transition = {
        .sType = VK_STRUCTURE_TYPE_BUFFER_MEMORY_BARRIER,
        .srcAccessMask = before,
        .dstAccessMask = after,
        .srcQueueFamilyIndex = VK_QUEUE_FAMILY_IGNORED,
        .dstQueueFamilyIndex = VK_QUEUE_FAMILY_IGNORED,
        .buffer = buffer->handle,
        .offset = buffer->offset,
        .size = buffer->size,
    };
    barrier(command, VK_PIPELINE_STAGE_ALL_COMMANDS_BIT, VK_PIPELINE_STAGE_ALL_COMMANDS_BIT, 0,
            0, NULL, 1, &transition, 0, NULL);
    return S_OK;
}

static UINT32 color_texel_size(VkFormat format)
{
    return format == VK_FORMAT_R8_UNORM ? 1 : 4;
}

static UINT32 color_row_pitch(VkFormat format, UINT32 width)
{
    const UINT32 mask = D3D12_TEXTURE_DATA_PITCH_ALIGNMENT - 1;
    return (width * color_texel_size(format) + mask) & ~mask;
}

static VkImageCreateInfo color_image_info(VkFormat format, UINT32 width, UINT32 height, BOOL shader_resource)
{
    const UINT32 padded_width = color_row_pitch(format, width) / color_texel_size(format);
    return (VkImageCreateInfo){
        .sType = VK_STRUCTURE_TYPE_IMAGE_CREATE_INFO,
        .imageType = VK_IMAGE_TYPE_2D,
        .format = format,
        .extent = { shader_resource ? width : padded_width, height, 1 },
        .mipLevels = 1,
        .arrayLayers = 1,
        .samples = VK_SAMPLE_COUNT_1_BIT,
        .tiling = shader_resource ? VK_IMAGE_TILING_OPTIMAL : VK_IMAGE_TILING_LINEAR,
        .usage = VK_IMAGE_USAGE_TRANSFER_SRC_BIT | VK_IMAGE_USAGE_TRANSFER_DST_BIT |
            (shader_resource ? VK_IMAGE_USAGE_SAMPLED_BIT : 0),
        .sharingMode = VK_SHARING_MODE_EXCLUSIVE,
        .initialLayout = VK_IMAGE_LAYOUT_UNDEFINED,
    };
}

HRESULT venus_color_image_requirements(const VenusDevice *device, VkFormat format, UINT32 width, UINT32 height,
                                      BOOL shader_resource, VkMemoryRequirements *requirements)
{
    const PFN_vkGetDeviceImageMemoryRequirements query = DEVICE_FN(device, vkGetDeviceImageMemoryRequirements);
    if (!query)
        return DXGI_ERROR_UNSUPPORTED;
    const VkImageCreateInfo image = color_image_info(format, width, height, shader_resource);
    const VkDeviceImageMemoryRequirements info = {
        .sType = VK_STRUCTURE_TYPE_DEVICE_IMAGE_MEMORY_REQUIREMENTS,
        .pCreateInfo = &image,
    };
    VkMemoryRequirements2 output = { .sType = VK_STRUCTURE_TYPE_MEMORY_REQUIREMENTS_2 };
    query(device->handle, &info, &output);
    *requirements = output.memoryRequirements;
    return requirements->size && requirements->alignment && requirements->memoryTypeBits ? S_OK : E_FAIL;
}

static UINT32 local_image_memory(const VenusDevice *device, UINT32 types)
{
    for (UINT32 i = 0; i < device->memory.memoryTypeCount; ++i) {
        if ((types & (1u << i)) && (device->memory.memoryTypes[i].propertyFlags & VK_MEMORY_PROPERTY_DEVICE_LOCAL_BIT))
            return i;
    }
    return UINT32_MAX;
}

static HRESULT create_local_image(const VenusDevice *device, const VkImageCreateInfo *info,
                                  VkImage *out_image, VkDeviceMemory *out_memory, VkMemoryRequirements *out_requirements)
{
    const PFN_vkCreateImage create = DEVICE_FN(device, vkCreateImage);
    const PFN_vkGetImageMemoryRequirements requirements = DEVICE_FN(device, vkGetImageMemoryRequirements);
    const PFN_vkAllocateMemory allocate = DEVICE_FN(device, vkAllocateMemory);
    const PFN_vkBindImageMemory bind = DEVICE_FN(device, vkBindImageMemory);
    const PFN_vkDestroyImage destroy = DEVICE_FN(device, vkDestroyImage);
    const PFN_vkFreeMemory free_memory = DEVICE_FN(device, vkFreeMemory);
    if (!create || !requirements || !allocate || !bind || !destroy || !free_memory)
        return DXGI_ERROR_UNSUPPORTED;

    VkImage handle;
    VkResult result = create(device->handle, info, NULL, &handle);
    if (result != VK_SUCCESS)
        return venus_hresult(result);
    requirements(device->handle, handle, out_requirements);
    const UINT32 type = local_image_memory(device, out_requirements->memoryTypeBits);
    if (type == UINT32_MAX) {
        destroy(device->handle, handle, NULL);
        return DXGI_ERROR_UNSUPPORTED;
    }

    const VkMemoryAllocateInfo allocation = {
        .sType = VK_STRUCTURE_TYPE_MEMORY_ALLOCATE_INFO,
        .allocationSize = out_requirements->size,
        .memoryTypeIndex = type,
    };
    VkDeviceMemory memory;
    result = allocate(device->handle, &allocation, NULL, &memory);
    if (result == VK_SUCCESS) {
        result = bind(device->handle, handle, memory, 0);
        if (result == VK_SUCCESS) {
            *out_image = handle;
            *out_memory = memory;
            return S_OK;
        }
        free_memory(device->handle, memory, NULL);
    }
    destroy(device->handle, handle, NULL);
    return venus_hresult(result);
}

static const VkImageSubresourceRange color_subresource = {
    .aspectMask = VK_IMAGE_ASPECT_COLOR_BIT,
    .levelCount = 1,
    .layerCount = 1,
};

HRESULT create_venus_color_image(const VenusDevice *device, VkFormat format, UINT32 width, UINT32 height,
                                 BOOL shader_resource, VenusColorImage *image)
{
    *image = (VenusColorImage){0};
    const PFN_vkGetImageSubresourceLayout layout = DEVICE_FN(device, vkGetImageSubresourceLayout);
    const PFN_vkCreateImageView create_view = DEVICE_FN(device, vkCreateImageView);
    if ((!shader_resource && !layout) ||
        (shader_resource && (!create_view || !DEVICE_FN(device, vkDestroyImageView))))
        return DXGI_ERROR_UNSUPPORTED;
    const VkImageCreateInfo info = color_image_info(format, width, height, shader_resource);
    HRESULT status = create_local_image(device, &info, &image->handle, &image->memory, &image->requirements);
    if (FAILED(status))
        return status;
    image->device = device;
    image->format = format;
    image->extent = (VkExtent3D){width, height, 1};

    if (shader_resource) {
        const VkImageViewCreateInfo view = {
            .sType = VK_STRUCTURE_TYPE_IMAGE_VIEW_CREATE_INFO, .image = image->handle,
            .viewType = VK_IMAGE_VIEW_TYPE_2D, .format = format, .subresourceRange = color_subresource,
        };
        status = venus_hresult(create_view(device->handle, &view, NULL, &image->sampled_view));
        if (FAILED(status)) {
            destroy_venus_color_image(image);
            return status;
        }
        /* The D3D12 copy footprint is independent of the opaque Vulkan layout. */
        image->layout.rowPitch = color_row_pitch(format, width);
        return S_OK;
    }
    const VkImageSubresource subresource = { .aspectMask = VK_IMAGE_ASPECT_COLOR_BIT };
    layout(device->handle, image->handle, &subresource, &image->layout);
    if (image->layout.rowPitch >= (UINT64)width * color_texel_size(format) &&
        image->layout.rowPitch % D3D12_TEXTURE_DATA_PITCH_ALIGNMENT == 0)
        return S_OK;
    destroy_venus_color_image(image);
    return DXGI_ERROR_UNSUPPORTED;
}

void destroy_venus_color_image(VenusColorImage *image)
{
    const VenusDevice *device = image->device;
    if (!device)
        return;
    if (image->sampled_view)
        DEVICE_FN(device, vkDestroyImageView)(device->handle, image->sampled_view, NULL);
    if (image->render_view) {
        DEVICE_FN(device, vkDestroyImageView)(device->handle, image->render_view, NULL);
        DEVICE_FN(device, vkDestroyImage)(device->handle, image->render_handle, NULL);
        DEVICE_FN(device, vkFreeMemory)(device->handle, image->render_memory, NULL);
    }
    DEVICE_FN(device, vkDestroyImage)(device->handle, image->handle, NULL);
    if (image->memory)
        DEVICE_FN(device, vkFreeMemory)(device->handle, image->memory, NULL);
    *image = (VenusColorImage){0};
}

static HRESULT ensure_color_attachment(VenusColorImage *image)
{
    const VenusDevice *device = image->device;
    const PFN_vkCreateImageView create_view = DEVICE_FN(device, vkCreateImageView);
    if (!create_view || !DEVICE_FN(device, vkDestroyImageView))
        return E_FAIL;

    AcquireSRWLockExclusive(&image->render_lock);
    HRESULT status = S_OK;
    if (!image->render_view) {
        VkImageCreateInfo info = color_image_info(image->format, image->extent.width, image->extent.height, FALSE);
        info.extent = image->extent;
        info.tiling = VK_IMAGE_TILING_OPTIMAL;
        info.usage = VK_IMAGE_USAGE_COLOR_ATTACHMENT_BIT | VK_IMAGE_USAGE_TRANSFER_SRC_BIT;
        VkMemoryRequirements requirements;
        status = create_local_image(device, &info, &image->render_handle, &image->render_memory, &requirements);
        if (SUCCEEDED(status)) {
            const VkImageViewCreateInfo view = {
                .sType = VK_STRUCTURE_TYPE_IMAGE_VIEW_CREATE_INFO, .image = image->render_handle,
                .viewType = VK_IMAGE_VIEW_TYPE_2D, .format = image->format, .subresourceRange = color_subresource,
            };
            status = venus_hresult(create_view(device->handle, &view, NULL, &image->render_view));
            if (FAILED(status)) {
                DEVICE_FN(device, vkDestroyImage)(device->handle, image->render_handle, NULL);
                DEVICE_FN(device, vkFreeMemory)(device->handle, image->render_memory, NULL);
                image->render_handle = VK_NULL_HANDLE;
                image->render_memory = VK_NULL_HANDLE;
                image->render_view = VK_NULL_HANDLE;
            }
        }
    }
    ReleaseSRWLockExclusive(&image->render_lock);
    return status;
}

HRESULT record_venus_color_draw(VkCommandBuffer command, VenusColorImage *image,
                               const VkViewport *viewport, const VkRect2D *scissor,
                               UINT vertex_count, UINT instance_count, UINT first_vertex, UINT first_instance)
{
    const VenusDevice *device = image->device;
    const PFN_vkCmdPipelineBarrier barrier = DEVICE_FN(device, vkCmdPipelineBarrier);
    const PFN_vkCmdBeginRendering begin = DEVICE_FN(device, vkCmdBeginRendering);
    const PFN_vkCmdEndRendering end = DEVICE_FN(device, vkCmdEndRendering);
    const PFN_vkCmdSetViewport set_viewport = DEVICE_FN(device, vkCmdSetViewport);
    const PFN_vkCmdSetScissor set_scissor = DEVICE_FN(device, vkCmdSetScissor);
    const PFN_vkCmdDraw draw = DEVICE_FN(device, vkCmdDraw);
    if (!barrier || !begin || !end || !set_viewport || !set_scissor || !draw)
        return E_FAIL;
    HRESULT status = ensure_color_attachment(image);
    if (FAILED(status))
        return status;

    const VkImageMemoryBarrier target = {
        .sType = VK_STRUCTURE_TYPE_IMAGE_MEMORY_BARRIER,
        .dstAccessMask = VK_ACCESS_COLOR_ATTACHMENT_WRITE_BIT,
        .oldLayout = VK_IMAGE_LAYOUT_UNDEFINED,
        .newLayout = VK_IMAGE_LAYOUT_COLOR_ATTACHMENT_OPTIMAL,
        .srcQueueFamilyIndex = VK_QUEUE_FAMILY_IGNORED,
        .dstQueueFamilyIndex = VK_QUEUE_FAMILY_IGNORED,
        .image = image->render_handle,
        .subresourceRange = color_subresource,
    };
    barrier(command, VK_PIPELINE_STAGE_TOP_OF_PIPE_BIT, VK_PIPELINE_STAGE_COLOR_ATTACHMENT_OUTPUT_BIT,
            0, 0, NULL, 0, NULL, 1, &target);
    const VkRenderingAttachmentInfo attachment = {
        .sType = VK_STRUCTURE_TYPE_RENDERING_ATTACHMENT_INFO, .imageView = image->render_view,
        .imageLayout = target.newLayout, .loadOp = VK_ATTACHMENT_LOAD_OP_DONT_CARE, .storeOp = VK_ATTACHMENT_STORE_OP_STORE,
    };
    const VkRenderingInfo rendering = {
        .sType = VK_STRUCTURE_TYPE_RENDERING_INFO, .renderArea.extent = {image->extent.width, image->extent.height},
        .layerCount = 1, .colorAttachmentCount = 1, .pColorAttachments = &attachment,
    };
    begin(command, &rendering);
    set_viewport(command, 0, 1, viewport);
    set_scissor(command, 0, 1, scissor);
    draw(command, vertex_count, instance_count, first_vertex, first_instance);
    end(command);
    return S_OK;
}

HRESULT record_venus_clear_color(VkCommandBuffer command, const VenusColorImage *image, const FLOAT color[4])
{
    const PFN_vkCmdPipelineBarrier barrier = DEVICE_FN(image->device, vkCmdPipelineBarrier);
    const PFN_vkCmdClearColorImage clear = DEVICE_FN(image->device, vkCmdClearColorImage);
    if (!barrier || !clear)
        return E_FAIL;

    const VkImageMemoryBarrier target = {
        .sType = VK_STRUCTURE_TYPE_IMAGE_MEMORY_BARRIER,
        .dstAccessMask = VK_ACCESS_TRANSFER_WRITE_BIT,
        .oldLayout = VK_IMAGE_LAYOUT_UNDEFINED,
        .newLayout = VK_IMAGE_LAYOUT_TRANSFER_DST_OPTIMAL,
        .srcQueueFamilyIndex = VK_QUEUE_FAMILY_IGNORED,
        .dstQueueFamilyIndex = VK_QUEUE_FAMILY_IGNORED,
        .image = image->handle,
        .subresourceRange = color_subresource,
    };
    const VkClearColorValue value = { .float32 = {color[0], color[1], color[2], color[3]} };
    barrier(command, VK_PIPELINE_STAGE_TOP_OF_PIPE_BIT, VK_PIPELINE_STAGE_TRANSFER_BIT, 0, 0, NULL, 0, NULL, 1, &target);
    clear(command, image->handle, target.newLayout, &value, 1, &color_subresource);
    return S_OK;
}

HRESULT prepare_venus_color_copy(VkCommandBuffer command, const VenusColorImage *image, BOOL rendered)
{
    const PFN_vkCmdPipelineBarrier barrier = DEVICE_FN(image->device, vkCmdPipelineBarrier);
    if (!barrier)
        return E_FAIL;
    const VkImageMemoryBarrier source = {
        .sType = VK_STRUCTURE_TYPE_IMAGE_MEMORY_BARRIER,
        .srcAccessMask = rendered ? VK_ACCESS_COLOR_ATTACHMENT_WRITE_BIT : VK_ACCESS_TRANSFER_WRITE_BIT,
        .dstAccessMask = VK_ACCESS_TRANSFER_READ_BIT,
        .oldLayout = rendered ? VK_IMAGE_LAYOUT_COLOR_ATTACHMENT_OPTIMAL : VK_IMAGE_LAYOUT_TRANSFER_DST_OPTIMAL,
        .newLayout = VK_IMAGE_LAYOUT_TRANSFER_SRC_OPTIMAL,
        .srcQueueFamilyIndex = VK_QUEUE_FAMILY_IGNORED,
        .dstQueueFamilyIndex = VK_QUEUE_FAMILY_IGNORED,
        .image = rendered ? image->render_handle : image->handle,
        .subresourceRange = color_subresource,
    };
    const VkPipelineStageFlags previous = rendered ? VK_PIPELINE_STAGE_COLOR_ATTACHMENT_OUTPUT_BIT : VK_PIPELINE_STAGE_TRANSFER_BIT;
    barrier(command, previous, VK_PIPELINE_STAGE_TRANSFER_BIT, 0, 0, NULL, 0, NULL, 1, &source);
    return S_OK;
}

HRESULT prepare_venus_shader_read(VkCommandBuffer command, const VenusColorImage *image)
{
    const PFN_vkCmdPipelineBarrier barrier = DEVICE_FN(image->device, vkCmdPipelineBarrier);
    if (!barrier || !image->sampled_view)
        return DXGI_ERROR_UNSUPPORTED;
    const VkImageMemoryBarrier source = {
        .sType = VK_STRUCTURE_TYPE_IMAGE_MEMORY_BARRIER,
        .srcAccessMask = VK_ACCESS_TRANSFER_WRITE_BIT,
        .dstAccessMask = VK_ACCESS_SHADER_READ_BIT,
        .oldLayout = VK_IMAGE_LAYOUT_TRANSFER_DST_OPTIMAL,
        .newLayout = VK_IMAGE_LAYOUT_SHADER_READ_ONLY_OPTIMAL,
        .srcQueueFamilyIndex = VK_QUEUE_FAMILY_IGNORED,
        .dstQueueFamilyIndex = VK_QUEUE_FAMILY_IGNORED,
        .image = image->handle,
        .subresourceRange = color_subresource,
    };
    barrier(command, VK_PIPELINE_STAGE_TRANSFER_BIT, VK_PIPELINE_STAGE_FRAGMENT_SHADER_BIT, 0,
            0, NULL, 0, NULL, 1, &source);
    return S_OK;
}

static BOOL color_image_buffer_region(const VenusColorImage *image, const VenusTransferBuffer *buffer,
                                      VkDeviceSize offset, UINT32 row_pitch, VkBufferImageCopy *region)
{
    const UINT32 texel_size = color_texel_size(image->format);
    if (image->device != buffer->device || !row_pitch || row_pitch % texel_size ||
        (offset + buffer->offset) % texel_size ||
        row_pitch / texel_size < image->extent.width || offset >= buffer->size)
        return FALSE;
    const UINT64 span = (UINT64)(image->extent.height - 1) * row_pitch + (UINT64)image->extent.width * texel_size;
    if (span > buffer->size - offset)
        return FALSE;
    *region = (VkBufferImageCopy){
        .bufferOffset = buffer->offset + offset,
        .bufferRowLength = row_pitch / texel_size,
        .imageSubresource = { .aspectMask = VK_IMAGE_ASPECT_COLOR_BIT, .layerCount = 1 },
        .imageExtent = image->extent,
    };
    return TRUE;
}

HRESULT upload_venus_color_image(VkCommandBuffer command, const VenusTransferBuffer *source,
                                 VkDeviceSize offset, UINT32 row_pitch, const VenusColorImage *destination)
{
    VkBufferImageCopy region;
    if (!color_image_buffer_region(destination, source, offset, row_pitch, &region))
        return E_INVALIDARG;
    const PFN_vkCmdPipelineBarrier barrier = DEVICE_FN(source->device, vkCmdPipelineBarrier);
    const PFN_vkCmdCopyBufferToImage copy = DEVICE_FN(source->device, vkCmdCopyBufferToImage);
    if (!barrier || !copy)
        return E_FAIL;
    const VkImageMemoryBarrier target = {
        .sType = VK_STRUCTURE_TYPE_IMAGE_MEMORY_BARRIER,
        .dstAccessMask = VK_ACCESS_TRANSFER_WRITE_BIT,
        .oldLayout = VK_IMAGE_LAYOUT_UNDEFINED,
        .newLayout = VK_IMAGE_LAYOUT_TRANSFER_DST_OPTIMAL,
        .srcQueueFamilyIndex = VK_QUEUE_FAMILY_IGNORED,
        .dstQueueFamilyIndex = VK_QUEUE_FAMILY_IGNORED,
        .image = destination->handle,
        .subresourceRange = color_subresource,
    };
    barrier(command, VK_PIPELINE_STAGE_TOP_OF_PIPE_BIT, VK_PIPELINE_STAGE_TRANSFER_BIT, 0,
            0, NULL, 0, NULL, 1, &target);
    copy(command, source->handle, destination->handle, target.newLayout, 1, &region);
    return S_OK;
}

HRESULT copy_venus_color_to_buffer(VkCommandBuffer command, const VenusColorImage *source,
                                  BOOL rendered, const VenusTransferBuffer *destination, VkDeviceSize offset,
                                  UINT32 row_pitch)
{
    VkBufferImageCopy region;
    if (!color_image_buffer_region(source, destination, offset, row_pitch, &region))
        return E_INVALIDARG;
    const PFN_vkCmdCopyImageToBuffer copy = DEVICE_FN(source->device, vkCmdCopyImageToBuffer);
    if (!copy)
        return E_FAIL;
    const VkImage handle = rendered ? source->render_handle : source->handle;
    copy(command, handle, VK_IMAGE_LAYOUT_TRANSFER_SRC_OPTIMAL, destination->handle, 1, &region);
    return S_OK;
}

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

HRESULT zero_venus_default_buffer(const VenusTransferBuffer *buffer)
{
    const PFN_vkCmdFillBuffer fill = DEVICE_FN(buffer->device, vkCmdFillBuffer);
    if (!fill)
        return DXGI_ERROR_UNSUPPORTED;
    VenusTransferList list;
    HRESULT status = begin_venus_transfer_list(buffer->device, &list);
    if (FAILED(status))
        return status;
    fill(list.command, buffer->handle, 0, VK_WHOLE_SIZE, 0);
    status = record_venus_buffer_barrier(list.command, buffer, VK_ACCESS_TRANSFER_WRITE_BIT,
                                        VK_ACCESS_MEMORY_READ_BIT | VK_ACCESS_MEMORY_WRITE_BIT);
    if (SUCCEEDED(status))
        status = close_venus_transfer_list(&list);
    if (SUCCEEDED(status))
        status = submit_venus_transfer_probe(&list);
    destroy_venus_transfer_list(&list);
    return status;
}

void destroy_venus_transfer_list(VenusTransferList *list)
{
    const VenusDevice *device = list->device;
    if (device)
        destroy_venus_command_pool(device, list->pool);
    *list = (VenusTransferList){0};
}
#endif
