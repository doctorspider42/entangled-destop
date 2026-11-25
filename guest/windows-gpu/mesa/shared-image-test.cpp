/* SPDX-License-Identifier: MIT */
/* Build with the Mesa Vulkan headers and run in the guest, passing the
 * absolute path of vulkan_virtio.dll. Uses no WSI or Vulkan layers. */
#define VK_NO_PROTOTYPES
#include "entangled_shared_image.h"

#include <cstdio>
#include <cstdlib>

static constexpr VkExtent2D extent = {256, 256};
static constexpr size_t pixel_count = extent.width * extent.height;
static PFN_vkGetInstanceProcAddr gipa;

template<typename T>
static T require(T value, const char *name)
{
    if (!value) {
        std::fprintf(stderr, "%s missing\n", name);
        std::exit(1);
    }
    return value;
}

static void check(VkResult result, const char *name)
{
    if (result != VK_SUCCESS) {
        std::fprintf(stderr, "%s returned %d\n", name, int(result));
        std::exit(1);
    }
}

#define CHECK(call) check((call), #call)
#define MODULE(name) require(reinterpret_cast<PFN_##name>(GetProcAddress(dll, #name)), #name)
#define INSTANCE(name) require(reinterpret_cast<PFN_##name>(gipa(instance, #name)), #name)
#define DEVICE(name) require(reinterpret_cast<PFN_##name>(gdpa(device, #name)), #name)

struct Context {
    VkInstance instance = VK_NULL_HANDLE;
    VkDevice device = VK_NULL_HANDLE;
    VkQueue queue = VK_NULL_HANDLE;
    VkCommandPool pool = VK_NULL_HANDLE;
    VkCommandBuffer cmd = VK_NULL_HANDLE;
    uint32_t family = 0;
    PFN_vkGetDeviceProcAddr gdpa = nullptr;
    VkPhysicalDeviceMemoryProperties memory = {};

    Context()
    {
        VkApplicationInfo app = {VK_STRUCTURE_TYPE_APPLICATION_INFO};
        app.pApplicationName = "entangled-shared-image-test";
        app.apiVersion = VK_API_VERSION_1_1;
        VkInstanceCreateInfo create = {VK_STRUCTURE_TYPE_INSTANCE_CREATE_INFO};
        create.pApplicationInfo = &app;
        CHECK(INSTANCE(vkCreateInstance)(&create, nullptr, &instance));

        uint32_t count = 1;
        VkPhysicalDevice physical = VK_NULL_HANDLE;
        VkResult result = INSTANCE(vkEnumeratePhysicalDevices)(instance, &count, &physical);
        if (result != VK_INCOMPLETE)
            CHECK(result);
        require(physical, "physical device");
        VkPhysicalDeviceProperties properties;
        INSTANCE(vkGetPhysicalDeviceProperties)(physical, &properties);
        std::printf("adapter=%s type=%d\n", properties.deviceName, int(properties.deviceType));
        if (properties.deviceType != VK_PHYSICAL_DEVICE_TYPE_DISCRETE_GPU &&
            properties.deviceType != VK_PHYSICAL_DEVICE_TYPE_INTEGRATED_GPU)
            std::exit(1);
        INSTANCE(vkGetPhysicalDeviceMemoryProperties)(physical, &memory);

        VkQueueFamilyProperties families[32];
        count = 32;
        INSTANCE(vkGetPhysicalDeviceQueueFamilyProperties)(physical, &count, families);
        while (family < count && !(families[family].queueFlags & VK_QUEUE_GRAPHICS_BIT))
            ++family;
        require(family < count, "graphics queue family");

        float priority = 1.0f;
        VkDeviceQueueCreateInfo queue_create = {VK_STRUCTURE_TYPE_DEVICE_QUEUE_CREATE_INFO};
        queue_create.queueFamilyIndex = family;
        queue_create.queueCount = 1;
        queue_create.pQueuePriorities = &priority;
        VkDeviceCreateInfo device_create = {VK_STRUCTURE_TYPE_DEVICE_CREATE_INFO};
        device_create.queueCreateInfoCount = 1;
        device_create.pQueueCreateInfos = &queue_create;
        CHECK(INSTANCE(vkCreateDevice)(physical, &device_create, nullptr, &device));
        gdpa = INSTANCE(vkGetDeviceProcAddr);
        DEVICE(vkGetDeviceQueue)(device, family, 0, &queue);

        VkCommandPoolCreateInfo pool_create = {VK_STRUCTURE_TYPE_COMMAND_POOL_CREATE_INFO};
        pool_create.flags = VK_COMMAND_POOL_CREATE_RESET_COMMAND_BUFFER_BIT;
        pool_create.queueFamilyIndex = family;
        CHECK(DEVICE(vkCreateCommandPool)(device, &pool_create, nullptr, &pool));
        VkCommandBufferAllocateInfo allocate = {VK_STRUCTURE_TYPE_COMMAND_BUFFER_ALLOCATE_INFO};
        allocate.commandPool = pool;
        allocate.commandBufferCount = 1;
        CHECK(DEVICE(vkAllocateCommandBuffers)(device, &allocate, &cmd));
    }

    ~Context()
    {
        DEVICE(vkDestroyCommandPool)(device, pool, nullptr);
        DEVICE(vkDestroyDevice)(device, nullptr);
        INSTANCE(vkDestroyInstance)(instance, nullptr);
    }

    void begin()
    {
        VkCommandBufferBeginInfo begin = {VK_STRUCTURE_TYPE_COMMAND_BUFFER_BEGIN_INFO};
        begin.flags = VK_COMMAND_BUFFER_USAGE_ONE_TIME_SUBMIT_BIT;
        CHECK(DEVICE(vkBeginCommandBuffer)(cmd, &begin));
    }

    void submit()
    {
        CHECK(DEVICE(vkEndCommandBuffer)(cmd));
        VkSubmitInfo submit = {VK_STRUCTURE_TYPE_SUBMIT_INFO};
        submit.commandBufferCount = 1;
        submit.pCommandBuffers = &cmd;
        CHECK(DEVICE(vkQueueSubmit)(queue, 1, &submit, VK_NULL_HANDLE));
        CHECK(DEVICE(vkQueueWaitIdle)(queue));
    }

    void barrier(VkImage image, VkImageLayout old_layout, VkImageLayout layout,
                 uint32_t from, uint32_t to, VkAccessFlags reads, VkAccessFlags writes)
    {
        VkImageMemoryBarrier barrier = {VK_STRUCTURE_TYPE_IMAGE_MEMORY_BARRIER};
        barrier.srcAccessMask = reads;
        barrier.dstAccessMask = writes;
        barrier.oldLayout = old_layout;
        barrier.newLayout = layout;
        barrier.srcQueueFamilyIndex = from;
        barrier.dstQueueFamilyIndex = to;
        barrier.image = image;
        barrier.subresourceRange = {VK_IMAGE_ASPECT_COLOR_BIT, 0, 1, 0, 1};
        DEVICE(vkCmdPipelineBarrier)(cmd, VK_PIPELINE_STAGE_ALL_COMMANDS_BIT,
                                    VK_PIPELINE_STAGE_ALL_COMMANDS_BIT,
                                    0, 0, nullptr, 0, nullptr, 1, &barrier);
    }

    void paint(VkImage image, const VkClearColorValue &color, bool first)
    {
        begin();
        barrier(image, first ? VK_IMAGE_LAYOUT_UNDEFINED : VK_IMAGE_LAYOUT_GENERAL,
                VK_IMAGE_LAYOUT_TRANSFER_DST_OPTIMAL,
                first ? VK_QUEUE_FAMILY_IGNORED : VK_QUEUE_FAMILY_EXTERNAL,
                first ? VK_QUEUE_FAMILY_IGNORED : family, 0, VK_ACCESS_TRANSFER_WRITE_BIT);
        const VkImageSubresourceRange range = {VK_IMAGE_ASPECT_COLOR_BIT, 0, 1, 0, 1};
        DEVICE(vkCmdClearColorImage)(cmd, image, VK_IMAGE_LAYOUT_TRANSFER_DST_OPTIMAL,
                                    &color, 1, &range);
        barrier(image, VK_IMAGE_LAYOUT_TRANSFER_DST_OPTIMAL, VK_IMAGE_LAYOUT_GENERAL,
                family, VK_QUEUE_FAMILY_EXTERNAL, VK_ACCESS_TRANSFER_WRITE_BIT, 0);
        submit();
    }

    void verify(VkImage image, uint32_t expected)
    {
        VkBufferCreateInfo create = {VK_STRUCTURE_TYPE_BUFFER_CREATE_INFO};
        create.size = pixel_count * sizeof(uint32_t);
        create.usage = VK_BUFFER_USAGE_TRANSFER_DST_BIT;
        VkBuffer buffer;
        CHECK(DEVICE(vkCreateBuffer)(device, &create, nullptr, &buffer));
        VkMemoryRequirements reqs;
        DEVICE(vkGetBufferMemoryRequirements)(device, buffer, &reqs);
        constexpr VkMemoryPropertyFlags mapped = VK_MEMORY_PROPERTY_HOST_VISIBLE_BIT |
                                                VK_MEMORY_PROPERTY_HOST_COHERENT_BIT;
        uint32_t type = 0;
        for (; type < memory.memoryTypeCount; ++type) {
            if ((reqs.memoryTypeBits & (1u << type)) &&
                (memory.memoryTypes[type].propertyFlags & mapped) == mapped)
                break;
        }
        require(type < memory.memoryTypeCount, "coherent readback memory");
        VkMemoryAllocateInfo alloc = {VK_STRUCTURE_TYPE_MEMORY_ALLOCATE_INFO};
        alloc.allocationSize = reqs.size;
        alloc.memoryTypeIndex = type;
        VkDeviceMemory readback;
        CHECK(DEVICE(vkAllocateMemory)(device, &alloc, nullptr, &readback));
        CHECK(DEVICE(vkBindBufferMemory)(device, buffer, readback, 0));

        begin();
        barrier(image, VK_IMAGE_LAYOUT_GENERAL, VK_IMAGE_LAYOUT_TRANSFER_SRC_OPTIMAL,
                VK_QUEUE_FAMILY_EXTERNAL, family, 0, VK_ACCESS_TRANSFER_READ_BIT);
        VkBufferImageCopy copy = {};
        copy.imageSubresource = {VK_IMAGE_ASPECT_COLOR_BIT, 0, 0, 1};
        copy.imageExtent = {extent.width, extent.height, 1};
        DEVICE(vkCmdCopyImageToBuffer)(cmd, image, VK_IMAGE_LAYOUT_TRANSFER_SRC_OPTIMAL,
                                      buffer, 1, &copy);
        barrier(image, VK_IMAGE_LAYOUT_TRANSFER_SRC_OPTIMAL, VK_IMAGE_LAYOUT_GENERAL,
                family, VK_QUEUE_FAMILY_EXTERNAL, VK_ACCESS_TRANSFER_READ_BIT, 0);
        VkMemoryBarrier host_barrier = {VK_STRUCTURE_TYPE_MEMORY_BARRIER};
        host_barrier.srcAccessMask = VK_ACCESS_TRANSFER_WRITE_BIT;
        host_barrier.dstAccessMask = VK_ACCESS_HOST_READ_BIT;
        DEVICE(vkCmdPipelineBarrier)(cmd, VK_PIPELINE_STAGE_TRANSFER_BIT,
                                     VK_PIPELINE_STAGE_HOST_BIT, 0,
                                     1, &host_barrier, 0, nullptr, 0, nullptr);
        submit();

        void *address = nullptr;
        CHECK(DEVICE(vkMapMemory)(device, readback, 0, VK_WHOLE_SIZE, 0, &address));
        const auto *pixels = static_cast<const uint32_t *>(address);
        for (size_t i = 0; i < pixel_count; ++i) {
            if (pixels[i] != expected) {
                std::fprintf(stderr, "pixel %zu was %08x, expected %08x\n", i, pixels[i], expected);
                std::exit(1);
            }
        }
        std::printf("verified %zu pixels = %08x\n", pixel_count, expected);
        DEVICE(vkUnmapMemory)(device, readback);
        DEVICE(vkDestroyBuffer)(device, buffer, nullptr);
        DEVICE(vkFreeMemory)(device, readback, nullptr);
    }
};

int wmain(int argc, wchar_t **argv)
{
    if (argc != 2)
        return 2;
    HMODULE dll = require(LoadLibraryW(argv[1]), "Venus ICD");
    gipa = require(reinterpret_cast<PFN_vkGetInstanceProcAddr>(
                       GetProcAddress(dll, "vk_icdGetInstanceProcAddr")), "ICD entry point");
    const auto create = MODULE(entangled_create_shared_image);
    const auto destroy = MODULE(entangled_destroy_shared_image);
    Context writer;
    Context reader;
    entangled_shared_image exported = {}, imported = {}, rejected = {};
    CHECK(create(writer.device, extent, nullptr, &exported));
    if (create(writer.device, extent, exported.token, &rejected) != VK_ERROR_INVALID_EXTERNAL_HANDLE) {
        std::fprintf(stderr, "same-context import should fail without dropping the published blob\n");
        return 1;
    }
    writer.paint(exported.image, {{1.0f, 0.0f, 0.0f, 1.0f}}, true);
    CHECK(create(reader.device, extent, exported.token, &imported));
    reader.verify(imported.image, 0xffff0000);
    writer.paint(exported.image, {{0.0f, 1.0f, 0.0f, 1.0f}}, false);
    destroy(writer.device, &exported);
    reader.verify(imported.image, 0xff00ff00);
    destroy(reader.device, &imported);
    std::puts("VENUS ICD SHARED IMAGE PASS");
    return 0;
}
