/* SPDX-License-Identifier: Apache-2.0 */
#include "venus-adapter.h"
#include <dxgi.h>
#include <stdio.h>
#include <string.h>

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

#ifdef ENTANGLED_D3D12_PROBE_RUNTIME
static void trace_descriptor_indexing(const VenusAdapter *adapter, PFN_vkGetPhysicalDeviceProperties2 get_properties)
{
    const PFN_vkGetPhysicalDeviceFeatures2 get_features = (PFN_vkGetPhysicalDeviceFeatures2)
        adapter->instance_proc_addr(adapter->instance, "vkGetPhysicalDeviceFeatures2");
    if (!get_features)
        return;
    VkPhysicalDeviceDescriptorIndexingFeatures indexing = {
        .sType = VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_DESCRIPTOR_INDEXING_FEATURES,
    };
    VkPhysicalDeviceFeatures2 features = {
        .sType = VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_FEATURES_2, .pNext = &indexing,
    };
    get_features(adapter->physical_device, &features);
    VkPhysicalDeviceDescriptorIndexingProperties capacity = {
        .sType = VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_DESCRIPTOR_INDEXING_PROPERTIES,
    };
    VkPhysicalDeviceProperties2 properties = {
        .sType = VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_PROPERTIES_2, .pNext = &capacity,
    };
    get_properties(adapter->physical_device, &properties);
    char message[576];
    snprintf(message, sizeof(message),
             "Entangled D3D12: Venus descriptors runtime-arrays=%u partially-bound=%u variable-count=%u "
             "sampled-nonuniform=%u uniform-nonuniform=%u storage-nonuniform=%u "
             "sampled-update-after-bind=%u set-samplers=%u stage-samplers=%u set-sampled=%u set-storage=%u pool=%u\n",
             indexing.runtimeDescriptorArray, indexing.descriptorBindingPartiallyBound,
             indexing.descriptorBindingVariableDescriptorCount, indexing.shaderSampledImageArrayNonUniformIndexing,
             indexing.shaderUniformBufferArrayNonUniformIndexing, indexing.shaderStorageBufferArrayNonUniformIndexing,
             indexing.descriptorBindingSampledImageUpdateAfterBind, capacity.maxDescriptorSetUpdateAfterBindSamplers,
             capacity.maxPerStageDescriptorUpdateAfterBindSamplers, capacity.maxDescriptorSetUpdateAfterBindSampledImages,
             capacity.maxDescriptorSetUpdateAfterBindStorageBuffers, capacity.maxUpdateAfterBindDescriptorsInAllPools);
    OutputDebugStringA(message);
}
#endif

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
                if (properties.properties.apiVersion < adapter->api_version)
                    adapter->api_version = properties.properties.apiVersion;
                adapter->subgroup_size = subgroup.subgroupSize;
                char message[384];
                snprintf(message, sizeof(message), "Entangled D3D12: Venus device %04x:%04x %s, graphics queue %u\n",
                         properties.properties.vendorID, properties.properties.deviceID,
                         properties.properties.deviceName, adapter->graphics_queue_family);
                OutputDebugStringA(message);
#ifdef ENTANGLED_D3D12_PROBE_RUNTIME
                trace_descriptor_indexing(adapter, get_properties);
#endif
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

    adapter->api_version = VK_API_VERSION_1_2;
#ifdef ENTANGLED_D3D12_PROBE_SHADER
    const PFN_vkEnumerateInstanceVersion enumerate_version = (PFN_vkEnumerateInstanceVersion)
        adapter->instance_proc_addr(VK_NULL_HANDLE, "vkEnumerateInstanceVersion");
    UINT32 loader_version = 0;
    if (enumerate_version && enumerate_version(&loader_version) == VK_SUCCESS &&
        loader_version >= VK_API_VERSION_1_3)
        adapter->api_version = VK_API_VERSION_1_3;
#endif
    const VkApplicationInfo app = { .sType = VK_STRUCTURE_TYPE_APPLICATION_INFO,
                                    .pEngineName = "Entangled D3D12", .apiVersion = adapter->api_version };
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

#ifdef ENTANGLED_D3D12_PROBE_SHADER
static BOOL supports_maintenance5(const VenusAdapter *adapter, VkPhysicalDeviceMaintenance5FeaturesKHR *feature)
{
    if (adapter->api_version < VK_API_VERSION_1_3)
        return FALSE;
    const PFN_vkEnumerateDeviceExtensionProperties enumerate = (PFN_vkEnumerateDeviceExtensionProperties)
        adapter->instance_proc_addr(adapter->instance, "vkEnumerateDeviceExtensionProperties");
    const PFN_vkGetPhysicalDeviceFeatures2 features = (PFN_vkGetPhysicalDeviceFeatures2)
        adapter->instance_proc_addr(adapter->instance, "vkGetPhysicalDeviceFeatures2");
    UINT32 count = 0;
    if (!enumerate || !features || enumerate(adapter->physical_device, NULL, &count, NULL) != VK_SUCCESS || !count)
        return FALSE;
    VkExtensionProperties *ext = HeapAlloc(GetProcessHeap(), 0, count * sizeof(*ext));
    if (!ext)
        return FALSE;
    BOOL found = FALSE;
    if (enumerate(adapter->physical_device, NULL, &count, ext) == VK_SUCCESS) {
        for (UINT32 i = 0; i < count; ++i) {
            if (strcmp(ext[i].extensionName, VK_KHR_MAINTENANCE_5_EXTENSION_NAME) == 0) {
                found = TRUE;
                break;
            }
        }
    }
    HeapFree(GetProcessHeap(), 0, ext);
    if (!found)
        return FALSE;

    VkPhysicalDeviceFeatures2 requested = {
        .sType = VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_FEATURES_2,
        .pNext = feature,
    };
    features(adapter->physical_device, &requested);
    return feature->maintenance5 != VK_FALSE;
}
#endif

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
    VkDeviceCreateInfo info = {
        .sType = VK_STRUCTURE_TYPE_DEVICE_CREATE_INFO,
        .queueCreateInfoCount = 1,
        .pQueueCreateInfos = &queue_info,
    };
    BOOL has_maintenance5 = FALSE;
    BOOL has_buffer_device_address = FALSE;
    BOOL has_dynamic_rendering = FALSE;
#ifdef ENTANGLED_D3D12_PROBE_SHADER
    VkPhysicalDeviceFeatures core_features = {0};
    const PFN_vkGetPhysicalDeviceProperties2 get_properties = (PFN_vkGetPhysicalDeviceProperties2)
        adapter->instance_proc_addr(adapter->instance, "vkGetPhysicalDeviceProperties2");
    VkPhysicalDeviceProperties2 properties = { .sType = VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_PROPERTIES_2 };
    if (!get_properties)
        return E_FAIL;
    get_properties(adapter->physical_device, &properties);
    info.pEnabledFeatures = &core_features;

    VkPhysicalDeviceMaintenance5FeaturesKHR maintenance5 = {
        .sType = VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_MAINTENANCE_5_FEATURES_KHR,
    };
    const char *extension = VK_KHR_MAINTENANCE_5_EXTENSION_NAME;
    has_maintenance5 = supports_maintenance5(adapter, &maintenance5);
    if (has_maintenance5) {
        info.pNext = &maintenance5;
        info.enabledExtensionCount = 1;
        info.ppEnabledExtensionNames = &extension;
    }
#endif
#ifdef ENTANGLED_D3D12_PROBE_COPY
    VkPhysicalDeviceDynamicRenderingFeatures rendering = {
        .sType = VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_DYNAMIC_RENDERING_FEATURES,
    };
    VkPhysicalDeviceBufferDeviceAddressFeatures address = {
        .sType = VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_BUFFER_DEVICE_ADDRESS_FEATURES,
    };
    const PFN_vkGetPhysicalDeviceFeatures2 get_features = (PFN_vkGetPhysicalDeviceFeatures2)
        adapter->instance_proc_addr(adapter->instance, "vkGetPhysicalDeviceFeatures2");
    if (adapter->api_version >= VK_API_VERSION_1_2 && get_features) {
        if (adapter->api_version >= VK_API_VERSION_1_3)
            address.pNext = &rendering;
        VkPhysicalDeviceFeatures2 features = {
            .sType = VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_FEATURES_2,
            .pNext = &address,
        };
        get_features(adapter->physical_device, &features);
#ifdef ENTANGLED_D3D12_PROBE_SHADER
        core_features.robustBufferAccess = features.features.robustBufferAccess;
#endif
        has_dynamic_rendering = rendering.dynamicRendering != VK_FALSE;
        if (has_dynamic_rendering) {
            rendering.pNext = (void *)info.pNext;
            info.pNext = &rendering;
        }
        has_buffer_device_address = address.bufferDeviceAddress != VK_FALSE;
        if (has_buffer_device_address) {
            address.bufferDeviceAddressCaptureReplay = VK_FALSE;
            address.bufferDeviceAddressMultiDevice = VK_FALSE;
            address.pNext = (void *)info.pNext;
            info.pNext = &address;
        }
    }
#endif
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
        .maintenance5 = has_maintenance5,
        .buffer_device_address = has_buffer_device_address,
        .dynamic_rendering = has_dynamic_rendering,
#ifdef ENTANGLED_D3D12_PROBE_SHADER
        .robust_buffer_access = core_features.robustBufferAccess,
        .limits = properties.properties.limits,
#endif
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
