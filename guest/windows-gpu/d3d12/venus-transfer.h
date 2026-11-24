/* SPDX-License-Identifier: Apache-2.0 */
#pragma once

#include "venus-adapter.h"

typedef struct VenusTransferBuffer {
    const VenusDevice *device;
    VkBuffer handle;
    VkDeviceMemory memory;
    VkDeviceSize size;
    VkDeviceSize offset;
    VkDeviceAddress address;
    void *mapped;
    VkBufferUsageFlags usage;
    UINT32 memory_type;
} VenusTransferBuffer;

typedef enum VenusBufferMemory {
    VENUS_BUFFER_UPLOAD,
    VENUS_BUFFER_READBACK,
    VENUS_BUFFER_DEVICE_LOCAL,
} VenusBufferMemory;

HRESULT create_venus_transfer_buffer(const VenusDevice *device, VkDeviceSize size, VenusBufferMemory placement,
                                     VkBufferUsageFlags uses, VenusTransferBuffer *buffer);
void destroy_venus_transfer_buffer(VenusTransferBuffer *buffer);
HRESULT record_venus_buffer_copy(VkCommandBuffer command, const VenusTransferBuffer *dst,
                                 const VenusTransferBuffer *src, VkBufferCopy region);

#ifdef ENTANGLED_D3D12_PROBE_COPY
HRESULT record_venus_buffer_barrier(VkCommandBuffer command, const VenusTransferBuffer *buffer,
                                    VkAccessFlags before, VkAccessFlags after);
HRESULT zero_venus_default_buffer(const VenusTransferBuffer *buffer);

typedef struct VenusColorImage {
    const VenusDevice *device;
    VkImage handle;
    VkDeviceMemory memory;
    VkMemoryRequirements requirements;
    VkExtent3D extent;
    VkFormat format;
    VkImageUsageFlags usage;
    VkImageView view;
    UINT32 row_pitch;
} VenusColorImage;

HRESULT venus_color_image_requirements(const VenusDevice *device, VkFormat format, UINT32 width, UINT32 height,
                                      VkImageUsageFlags usage, VkMemoryRequirements *requirements);
HRESULT create_venus_color_image(const VenusDevice *device, VkFormat format, UINT32 width, UINT32 height,
                                 VkImageUsageFlags usage, VenusColorImage *image);
HRESULT create_venus_placed_color_image(const VenusTransferBuffer *storage, VkDeviceSize offset, VkFormat format,
                                        UINT32 width, UINT32 height, VkImageUsageFlags usage, VenusColorImage *image);
void destroy_venus_color_image(VenusColorImage *image);
HRESULT zero_venus_color_image(const VenusColorImage *image);
HRESULT record_venus_color_barrier(VkCommandBuffer command, const VenusColorImage *image,
                                   VkAccessFlags before, VkAccessFlags after);
HRESULT record_venus_clear_color(VkCommandBuffer command, const VenusColorImage *image, const FLOAT color[4]);
HRESULT record_venus_color_draw(VkCommandBuffer command, const VenusColorImage *image,
                               const VkViewport *viewport, const VkRect2D *scissor,
                               UINT vertex_count, UINT instance_count, UINT first_vertex, UINT first_instance);
HRESULT upload_venus_color_image(VkCommandBuffer command, const VenusTransferBuffer *source,
                                 VkDeviceSize offset, UINT32 row_pitch, const VenusColorImage *destination,
                                 VkOffset3D origin, VkExtent3D extent);
HRESULT copy_venus_color_to_buffer(VkCommandBuffer command, const VenusColorImage *source,
                                  const VenusTransferBuffer *destination, VkDeviceSize offset, UINT32 row_pitch);

HRESULT create_venus_command_pool(const VenusDevice *device, VkCommandPoolCreateFlags flags, VkCommandPool *pool);
HRESULT reset_venus_command_pool(const VenusDevice *device, VkCommandPool pool);
void destroy_venus_command_pool(const VenusDevice *device, VkCommandPool pool);
HRESULT begin_venus_command_buffer(const VenusDevice *device, VkCommandPool pool,
                                   VkCommandBufferUsageFlags flags, VkCommandBuffer *command);

typedef struct VenusTransferList {
    const VenusDevice *device;
    VkCommandPool pool;
    VkCommandBuffer command;
} VenusTransferList;

HRESULT begin_venus_transfer_list(const VenusDevice *device, VenusTransferList *list);
HRESULT close_venus_transfer_list(const VenusTransferList *list);
HRESULT submit_venus_transfer_commands(const VenusDevice *device, UINT count, const VkCommandBuffer *commands);
HRESULT submit_venus_transfer_probe(const VenusTransferList *list);
void destroy_venus_transfer_list(VenusTransferList *list);
#endif
