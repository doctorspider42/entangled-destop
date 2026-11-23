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
    UINT32 graphics_queue_family;
} VenusAdapter;

HRESULT open_venus_adapter(VenusAdapter *adapter);
void close_venus_adapter(VenusAdapter *adapter);
