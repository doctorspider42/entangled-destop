/* SPDX-License-Identifier: Apache-2.0 */
#pragma once

#include <windows.h>
#define VK_NO_PROTOTYPES
#include <vulkan/vulkan_core.h>

typedef struct VenusAdapter {
    HMODULE loader;
    PFN_vkGetInstanceProcAddr instance_proc_addr;
    PFN_vkDestroyInstance destroy_instance;
    VkInstance instance;
    VkPhysicalDevice physical_device;
    UINT32 api_version;
    UINT32 graphics_queue_family;
    UINT32 timestamp_valid_bits;
    UINT32 subgroup_size;
} VenusAdapter;

typedef struct VenusDevice {
    VkDevice handle;
    VkQueue graphics_queue;
    PFN_vkGetDeviceProcAddr proc_addr;
    PFN_vkDestroyDevice destroy;
    UINT32 graphics_queue_family;
    UINT32 timestamp_valid_bits;
    VkPhysicalDeviceMemoryProperties memory;
    BOOL maintenance5;
    BOOL buffer_device_address;
    BOOL dynamic_rendering;
#ifdef ENTANGLED_D3D12_PROBE_SHADER
    BOOL robust_buffer_access;
    BOOL indexed_samplers;
    VkPhysicalDeviceLimits limits;
#endif
} VenusDevice;

HRESULT open_venus_adapter(VenusAdapter *adapter);
void close_venus_adapter(VenusAdapter *adapter);
HRESULT create_venus_device(const VenusAdapter *adapter, VenusDevice *device);
void destroy_venus_device(VenusDevice *device);
HRESULT venus_hresult(VkResult result);
