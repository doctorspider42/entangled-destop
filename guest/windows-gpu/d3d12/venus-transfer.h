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
HRESULT probe_venus_buffer_copy(const VenusDevice *device);
#endif
