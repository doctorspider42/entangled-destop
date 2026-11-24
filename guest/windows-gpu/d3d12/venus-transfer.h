/* SPDX-License-Identifier: Apache-2.0 */
#pragma once

#include "venus-adapter.h"

typedef struct VenusTransferBuffer {
    const VenusDevice *device;
    VkBuffer handle;
    VkDeviceMemory memory;
    VkDeviceSize size;
    void *mapped;
} VenusTransferBuffer;

HRESULT create_venus_transfer_buffer(const VenusDevice *device, VkDeviceSize size, BOOL readback,
                                     VenusTransferBuffer *buffer);
void destroy_venus_transfer_buffer(VenusTransferBuffer *buffer);
HRESULT record_venus_buffer_copy(VkCommandBuffer command, const VenusTransferBuffer *dst,
                                 const VenusTransferBuffer *src, VkBufferCopy region);

#ifdef ENTANGLED_D3D12_PROBE_COPY
HRESULT create_venus_command_pool(const VenusDevice *device, VkCommandPoolCreateFlags flags, VkCommandPool *pool);
HRESULT reset_venus_command_pool(const VenusDevice *device, VkCommandPool pool);
void destroy_venus_command_pool(const VenusDevice *device, VkCommandPool pool);

typedef struct VenusTransferList {
    const VenusDevice *device;
    VkCommandPool pool;
    VkCommandBuffer command;
} VenusTransferList;

HRESULT begin_venus_transfer_list(const VenusDevice *device, VenusTransferList *list);
HRESULT close_venus_transfer_list(const VenusTransferList *list);
HRESULT submit_venus_transfer_probe(const VenusTransferList *list);
void destroy_venus_transfer_list(VenusTransferList *list);
#endif
