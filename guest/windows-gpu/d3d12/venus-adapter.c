/* SPDX-License-Identifier: Apache-2.0 */
#include "venus-adapter.h"
#include <dxgi.h>
#include <stdio.h>

static HRESULT find_graphics_queue(VkPhysicalDevice device,
                                   PFN_vkGetPhysicalDeviceQueueFamilyProperties get_queue_families,
                                   UINT32 *family)
{
    UINT32 count = 0;
    get_queue_families(device, &count, NULL);
    if (!count)
        return DXGI_ERROR_UNSUPPORTED;

    VkQueueFamilyProperties *queues = HeapAlloc(GetProcessHeap(), 0, sizeof(*queues) * count);
    if (!queues)
        return E_OUTOFMEMORY;
    get_queue_families(device, &count, queues);

    HRESULT result = DXGI_ERROR_UNSUPPORTED;
    for (UINT32 i = 0; i < count; ++i) {
        if (queues[i].queueCount && (queues[i].queueFlags & VK_QUEUE_GRAPHICS_BIT)) {
            *family = i;
            result = S_OK;
            break;
        }
    }
    HeapFree(GetProcessHeap(), 0, queues);
    return result;
}

static HRESULT select_physical_device(VenusAdapter *adapter)
{
    const PFN_vkEnumeratePhysicalDevices enumerate = (PFN_vkEnumeratePhysicalDevices)
        adapter->instance_proc_addr(adapter->instance, "vkEnumeratePhysicalDevices");
    const PFN_vkGetPhysicalDeviceProperties2 get_properties = (PFN_vkGetPhysicalDeviceProperties2)
        adapter->instance_proc_addr(adapter->instance, "vkGetPhysicalDeviceProperties2");
    const PFN_vkGetPhysicalDeviceQueueFamilyProperties get_queue_families =
        (PFN_vkGetPhysicalDeviceQueueFamilyProperties)
        adapter->instance_proc_addr(adapter->instance, "vkGetPhysicalDeviceQueueFamilyProperties");
    if (!enumerate || !get_properties || !get_queue_families)
        return DXGI_ERROR_UNSUPPORTED;

    UINT32 count = 0;
    if (enumerate(adapter->instance, &count, NULL) != VK_SUCCESS || !count)
        return DXGI_ERROR_UNSUPPORTED;
    VkPhysicalDevice *devices = HeapAlloc(GetProcessHeap(), 0, sizeof(*devices) * count);
    if (!devices)
        return E_OUTOFMEMORY;

    HRESULT result = DXGI_ERROR_UNSUPPORTED;
    if (enumerate(adapter->instance, &count, devices) == VK_SUCCESS) {
        for (UINT32 i = 0; i < count; ++i) {
            VkPhysicalDeviceProperties2 properties = { .sType = VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_PROPERTIES_2 };
            get_properties(devices[i], &properties);
            if (properties.properties.apiVersion < VK_API_VERSION_1_2 ||
                properties.properties.deviceType == VK_PHYSICAL_DEVICE_TYPE_CPU)
                continue;

            VkPhysicalDeviceSubgroupProperties subgroup = {
                .sType = VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_SUBGROUP_PROPERTIES,
            };
            VkPhysicalDeviceDriverProperties driver = {
                .sType = VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_DRIVER_PROPERTIES,
                .pNext = &subgroup,
            };
            properties.pNext = &driver;
            get_properties(devices[i], &properties);
            if (driver.driverID != VK_DRIVER_ID_MESA_VENUS)
                continue;

            result = find_graphics_queue(devices[i], get_queue_families, &adapter->graphics_queue_family);
            if (result == DXGI_ERROR_UNSUPPORTED)
                continue;
            if (SUCCEEDED(result)) {
                adapter->physical_device = devices[i];
                adapter->subgroup_size = subgroup.subgroupSize;
                char message[384];
                snprintf(message, sizeof(message), "Entangled D3D12: Venus device %04x:%04x %s, graphics queue %u\n",
                         properties.properties.vendorID, properties.properties.deviceID,
                         properties.properties.deviceName, adapter->graphics_queue_family);
                OutputDebugStringA(message);
            }
            break;
        }
    }
    HeapFree(GetProcessHeap(), 0, devices);
    return result;
}

HRESULT open_venus_adapter(VenusAdapter *adapter)
{
    adapter->loader = LoadLibraryExW(L"vulkan-1.dll", NULL, LOAD_LIBRARY_SEARCH_SYSTEM32);
    if (!adapter->loader)
        return DXGI_ERROR_UNSUPPORTED;

    adapter->instance_proc_addr = (PFN_vkGetInstanceProcAddr)(void *)
        GetProcAddress(adapter->loader, "vkGetInstanceProcAddr");
    adapter->destroy_instance = (PFN_vkDestroyInstance)(void *)
        GetProcAddress(adapter->loader, "vkDestroyInstance");
    if (!adapter->instance_proc_addr || !adapter->destroy_instance)
        return DXGI_ERROR_UNSUPPORTED;

    const PFN_vkCreateInstance create = (PFN_vkCreateInstance)
        adapter->instance_proc_addr(VK_NULL_HANDLE, "vkCreateInstance");
    if (!create)
        return DXGI_ERROR_UNSUPPORTED;

    const VkApplicationInfo app = { .sType = VK_STRUCTURE_TYPE_APPLICATION_INFO,
                                    .pEngineName = "Entangled D3D12", .apiVersion = VK_API_VERSION_1_2 };
    const VkInstanceCreateInfo info = { .sType = VK_STRUCTURE_TYPE_INSTANCE_CREATE_INFO, .pApplicationInfo = &app };
    VkInstance instance = VK_NULL_HANDLE;
    const VkResult result = create(&info, NULL, &instance);
    if (result == VK_ERROR_OUT_OF_HOST_MEMORY)
        return E_OUTOFMEMORY;
    if (result != VK_SUCCESS)
        return DXGI_ERROR_UNSUPPORTED;

    adapter->instance = instance;
    return select_physical_device(adapter);
}

void close_venus_adapter(VenusAdapter *adapter)
{
    if (adapter->instance)
        adapter->destroy_instance(adapter->instance, NULL);
    if (adapter->loader)
        FreeLibrary(adapter->loader);
}

HRESULT venus_hresult(VkResult result)
{
    switch (result) {
    case VK_SUCCESS: return S_OK;
    case VK_TIMEOUT: return HRESULT_FROM_WIN32(WAIT_TIMEOUT);
    case VK_ERROR_OUT_OF_HOST_MEMORY:
    case VK_ERROR_OUT_OF_DEVICE_MEMORY: return E_OUTOFMEMORY;
    case VK_ERROR_DEVICE_LOST: return DXGI_ERROR_DEVICE_REMOVED;
    default: return E_FAIL;
    }
}

HRESULT create_venus_device(const VenusAdapter *adapter, VenusDevice *device)
{
    const PFN_vkCreateDevice create = (PFN_vkCreateDevice)
        adapter->instance_proc_addr(adapter->instance, "vkCreateDevice");
    const PFN_vkGetDeviceProcAddr proc_addr = (PFN_vkGetDeviceProcAddr)
        adapter->instance_proc_addr(adapter->instance, "vkGetDeviceProcAddr");
    const PFN_vkDestroyDevice destroy = (PFN_vkDestroyDevice)
        adapter->instance_proc_addr(adapter->instance, "vkDestroyDevice");
    const PFN_vkGetPhysicalDeviceMemoryProperties get_memory = (PFN_vkGetPhysicalDeviceMemoryProperties)
        adapter->instance_proc_addr(adapter->instance, "vkGetPhysicalDeviceMemoryProperties");
    if (!create || !proc_addr || !destroy || !get_memory)
        return E_FAIL;

    const float priority = 1.0f;
    const VkDeviceQueueCreateInfo queue_info = {
        .sType = VK_STRUCTURE_TYPE_DEVICE_QUEUE_CREATE_INFO,
        .queueFamilyIndex = adapter->graphics_queue_family,
        .queueCount = 1,
        .pQueuePriorities = &priority,
    };
    const VkDeviceCreateInfo info = {
        .sType = VK_STRUCTURE_TYPE_DEVICE_CREATE_INFO,
        .queueCreateInfoCount = 1,
        .pQueueCreateInfos = &queue_info,
    };
    VkDevice handle;
    const VkResult result = create(adapter->physical_device, &info, NULL, &handle);
    if (result != VK_SUCCESS)
        return venus_hresult(result);

    const PFN_vkGetDeviceQueue get_queue = (PFN_vkGetDeviceQueue)proc_addr(handle, "vkGetDeviceQueue");
    if (!get_queue) {
        destroy(handle, NULL);
        return E_FAIL;
    }

    VkQueue queue = VK_NULL_HANDLE;
    get_queue(handle, adapter->graphics_queue_family, 0, &queue);
    if (!queue) {
        destroy(handle, NULL);
        return E_FAIL;
    }

    *device = (VenusDevice){
        .handle = handle,
        .graphics_queue = queue,
        .proc_addr = proc_addr,
        .destroy = destroy,
        .graphics_queue_family = adapter->graphics_queue_family,
    };
    get_memory(adapter->physical_device, &device->memory);
    return S_OK;
}

void destroy_venus_device(VenusDevice *device)
{
    if (device->handle)
        device->destroy(device->handle, NULL);
    *device = (VenusDevice){0};
}
