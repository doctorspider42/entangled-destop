/* vk-present: a win32 window, a Vulkan surface and swapchain, and 300
 * frames cleared to a changing colour and presented (ADR-0009 phase C).
 * Prints "PRESENT PASS <fps>" when every frame was acquired and presented. */
#define VK_USE_PLATFORM_WIN32_KHR
#define VK_NO_PROTOTYPES
#include <windows.h>
#include <stdio.h>
#include <vulkan/vulkan.h>

#define FRAMES 300
#define CHECK(x, what)                                                        \
    do {                                                                      \
        VkResult r_ = (x);                                                    \
        if (r_ != VK_SUCCESS && r_ != VK_SUBOPTIMAL_KHR) {                    \
            printf("PRESENT FAIL %s: %d\n", what, (int)r_);                   \
            return 1;                                                         \
        }                                                                     \
    } while (0)

static PFN_vkGetInstanceProcAddr gipa;
#define LOAD(inst, name) PFN_##name name = (PFN_##name)gipa(inst, #name)

static LRESULT CALLBACK proc(HWND w, UINT m, WPARAM a, LPARAM b)
{
    return DefWindowProcW(w, m, a, b);
}

int main(void)
{
    HMODULE loader = LoadLibraryW(L"vulkan-1.dll");
    if (!loader) {
        printf("PRESENT FAIL no vulkan-1.dll\n");
        return 1;
    }
    gipa = (PFN_vkGetInstanceProcAddr)(void *)GetProcAddress(loader, "vkGetInstanceProcAddr");
    LOAD(NULL, vkCreateInstance);

    const char *iext[] = {VK_KHR_SURFACE_EXTENSION_NAME, VK_KHR_WIN32_SURFACE_EXTENSION_NAME};
    VkApplicationInfo app = {VK_STRUCTURE_TYPE_APPLICATION_INFO, NULL, "vk-present", 1, NULL, 0,
                             VK_API_VERSION_1_1};
    VkInstanceCreateInfo ici = {VK_STRUCTURE_TYPE_INSTANCE_CREATE_INFO, NULL, 0, &app, 0, NULL,
                                2, iext};
    VkInstance inst;
    CHECK(vkCreateInstance(&ici, NULL, &inst), "vkCreateInstance");
    LOAD(inst, vkEnumeratePhysicalDevices);
    LOAD(inst, vkGetPhysicalDeviceQueueFamilyProperties);
    LOAD(inst, vkGetPhysicalDeviceSurfaceSupportKHR);
    LOAD(inst, vkGetPhysicalDeviceSurfaceCapabilitiesKHR);
    LOAD(inst, vkGetPhysicalDeviceSurfaceFormatsKHR);
    LOAD(inst, vkCreateWin32SurfaceKHR);
    LOAD(inst, vkCreateDevice);
    LOAD(inst, vkGetDeviceProcAddr);

    WNDCLASSW wc = {0};
    wc.lpfnWndProc = proc;
    wc.hInstance = GetModuleHandleW(NULL);
    wc.lpszClassName = L"vkpresent";
    RegisterClassW(&wc);
    HWND hwnd = CreateWindowW(L"vkpresent", L"vk-present", WS_OVERLAPPEDWINDOW | WS_VISIBLE,
                              100, 100, 640, 480, NULL, NULL, wc.hInstance, NULL);
    VkWin32SurfaceCreateInfoKHR sci = {VK_STRUCTURE_TYPE_WIN32_SURFACE_CREATE_INFO_KHR, NULL, 0,
                                       wc.hInstance, hwnd};
    VkSurfaceKHR surface;
    CHECK(vkCreateWin32SurfaceKHR(inst, &sci, NULL, &surface), "vkCreateWin32SurfaceKHR");

    uint32_t n = 1;
    VkPhysicalDevice pd;
    VkResult er = vkEnumeratePhysicalDevices(inst, &n, &pd);
    if ((er != VK_SUCCESS && er != VK_INCOMPLETE) || n == 0) {
        printf("PRESENT FAIL no physical device\n");
        return 1;
    }
    uint32_t qn = 0;
    VkQueueFamilyProperties qp[16];
    vkGetPhysicalDeviceQueueFamilyProperties(pd, &qn, NULL);
    if (qn > 16)
        qn = 16;
    vkGetPhysicalDeviceQueueFamilyProperties(pd, &qn, qp);
    uint32_t family = UINT32_MAX;
    for (uint32_t i = 0; i < qn; i++) {
        VkBool32 ok = VK_FALSE;
        vkGetPhysicalDeviceSurfaceSupportKHR(pd, i, surface, &ok);
        if (ok && (qp[i].queueFlags & VK_QUEUE_GRAPHICS_BIT)) {
            family = i;
            break;
        }
    }
    if (family == UINT32_MAX) {
        printf("PRESENT FAIL no queue family presents to the surface\n");
        return 1;
    }
    float prio = 1.0f;
    VkDeviceQueueCreateInfo qci = {VK_STRUCTURE_TYPE_DEVICE_QUEUE_CREATE_INFO, NULL, 0, family, 1,
                                   &prio};
    const char *dext[] = {VK_KHR_SWAPCHAIN_EXTENSION_NAME};
    VkDeviceCreateInfo dci = {VK_STRUCTURE_TYPE_DEVICE_CREATE_INFO, NULL, 0, 1, &qci, 0, NULL, 1,
                              dext, NULL};
    VkDevice dev;
    CHECK(vkCreateDevice(pd, &dci, NULL, &dev), "vkCreateDevice");
#define DLOAD(name) PFN_##name name = (PFN_##name)vkGetDeviceProcAddr(dev, #name)
    DLOAD(vkGetDeviceQueue);
    DLOAD(vkCreateSwapchainKHR);
    DLOAD(vkGetSwapchainImagesKHR);
    DLOAD(vkAcquireNextImageKHR);
    DLOAD(vkQueuePresentKHR);
    DLOAD(vkCreateCommandPool);
    DLOAD(vkAllocateCommandBuffers);
    DLOAD(vkBeginCommandBuffer);
    DLOAD(vkEndCommandBuffer);
    DLOAD(vkCmdPipelineBarrier);
    DLOAD(vkCmdClearColorImage);
    DLOAD(vkQueueSubmit);
    DLOAD(vkCreateSemaphore);
    DLOAD(vkCreateFence);
    DLOAD(vkWaitForFences);
    DLOAD(vkResetFences);
    DLOAD(vkResetCommandBuffer);
    DLOAD(vkDeviceWaitIdle);
    VkQueue queue;
    vkGetDeviceQueue(dev, family, 0, &queue);

    VkSurfaceCapabilitiesKHR caps;
    CHECK(vkGetPhysicalDeviceSurfaceCapabilitiesKHR(pd, surface, &caps), "surface caps");
    uint32_t fn = 1;
    VkSurfaceFormatKHR fmt;
    VkResult fr = vkGetPhysicalDeviceSurfaceFormatsKHR(pd, surface, &fn, &fmt);
    if ((fr != VK_SUCCESS && fr != VK_INCOMPLETE) || fn == 0) {
        printf("PRESENT FAIL no surface format\n");
        return 1;
    }
    VkSwapchainCreateInfoKHR swci = {VK_STRUCTURE_TYPE_SWAPCHAIN_CREATE_INFO_KHR};
    swci.surface = surface;
    swci.minImageCount = caps.minImageCount < 2 ? 2 : caps.minImageCount;
    swci.imageFormat = fmt.format;
    swci.imageColorSpace = fmt.colorSpace;
    swci.imageExtent = caps.currentExtent;
    swci.imageArrayLayers = 1;
    swci.imageUsage = VK_IMAGE_USAGE_TRANSFER_DST_BIT | VK_IMAGE_USAGE_COLOR_ATTACHMENT_BIT;
    swci.preTransform = caps.currentTransform;
    swci.compositeAlpha = VK_COMPOSITE_ALPHA_OPAQUE_BIT_KHR;
    swci.presentMode = VK_PRESENT_MODE_FIFO_KHR;
    swci.clipped = VK_TRUE;
    VkSwapchainKHR sc;
    CHECK(vkCreateSwapchainKHR(dev, &swci, NULL, &sc), "vkCreateSwapchainKHR");
    uint32_t in = 0;
    vkGetSwapchainImagesKHR(dev, sc, &in, NULL);
    VkImage images[8];
    if (in > 8)
        in = 8;
    vkGetSwapchainImagesKHR(dev, sc, &in, images);
    printf("swapchain %ux%u, %u images, format %d\n", swci.imageExtent.width,
           swci.imageExtent.height, in, (int)fmt.format);

    VkCommandPoolCreateInfo pci = {VK_STRUCTURE_TYPE_COMMAND_POOL_CREATE_INFO, NULL,
                                   VK_COMMAND_POOL_CREATE_RESET_COMMAND_BUFFER_BIT, family};
    VkCommandPool pool;
    CHECK(vkCreateCommandPool(dev, &pci, NULL, &pool), "command pool");
    VkCommandBufferAllocateInfo cai = {VK_STRUCTURE_TYPE_COMMAND_BUFFER_ALLOCATE_INFO, NULL, pool,
                                       VK_COMMAND_BUFFER_LEVEL_PRIMARY, 1};
    VkCommandBuffer cb;
    CHECK(vkAllocateCommandBuffers(dev, &cai, &cb), "command buffer");
    VkSemaphoreCreateInfo semi = {VK_STRUCTURE_TYPE_SEMAPHORE_CREATE_INFO};
    VkSemaphore acquired, rendered;
    CHECK(vkCreateSemaphore(dev, &semi, NULL, &acquired), "semaphore");
    CHECK(vkCreateSemaphore(dev, &semi, NULL, &rendered), "semaphore");
    VkFenceCreateInfo fci = {VK_STRUCTURE_TYPE_FENCE_CREATE_INFO, NULL, 0};
    VkFence fence;
    CHECK(vkCreateFence(dev, &fci, NULL, &fence), "fence");

    LARGE_INTEGER f, t0, t1;
    QueryPerformanceFrequency(&f);
    QueryPerformanceCounter(&t0);
    for (int frame = 0; frame < FRAMES; frame++) {
        MSG msg;
        while (PeekMessageW(&msg, NULL, 0, 0, PM_REMOVE))
            DispatchMessageW(&msg);
        uint32_t idx;
        CHECK(vkAcquireNextImageKHR(dev, sc, UINT64_MAX, acquired, VK_NULL_HANDLE, &idx),
              "acquire");
        VkCommandBufferBeginInfo bi = {VK_STRUCTURE_TYPE_COMMAND_BUFFER_BEGIN_INFO, NULL,
                                       VK_COMMAND_BUFFER_USAGE_ONE_TIME_SUBMIT_BIT};
        vkResetCommandBuffer(cb, 0);
        vkBeginCommandBuffer(cb, &bi);
        VkImageSubresourceRange range = {VK_IMAGE_ASPECT_COLOR_BIT, 0, 1, 0, 1};
        VkImageMemoryBarrier toDst = {VK_STRUCTURE_TYPE_IMAGE_MEMORY_BARRIER, NULL, 0,
                                      VK_ACCESS_TRANSFER_WRITE_BIT, VK_IMAGE_LAYOUT_UNDEFINED,
                                      VK_IMAGE_LAYOUT_TRANSFER_DST_OPTIMAL,
                                      VK_QUEUE_FAMILY_IGNORED, VK_QUEUE_FAMILY_IGNORED,
                                      images[idx], range};
        vkCmdPipelineBarrier(cb, VK_PIPELINE_STAGE_TOP_OF_PIPE_BIT,
                             VK_PIPELINE_STAGE_TRANSFER_BIT, 0, 0, NULL, 0, NULL, 1, &toDst);
        float t = (float)frame / FRAMES;
        VkClearColorValue color = {{t, 0.3f, 1.0f - t, 1.0f}};
        vkCmdClearColorImage(cb, images[idx], VK_IMAGE_LAYOUT_TRANSFER_DST_OPTIMAL, &color, 1,
                             &range);
        VkImageMemoryBarrier toPresent = toDst;
        toPresent.srcAccessMask = VK_ACCESS_TRANSFER_WRITE_BIT;
        toPresent.dstAccessMask = 0;
        toPresent.oldLayout = VK_IMAGE_LAYOUT_TRANSFER_DST_OPTIMAL;
        toPresent.newLayout = VK_IMAGE_LAYOUT_PRESENT_SRC_KHR;
        vkCmdPipelineBarrier(cb, VK_PIPELINE_STAGE_TRANSFER_BIT,
                             VK_PIPELINE_STAGE_BOTTOM_OF_PIPE_BIT, 0, 0, NULL, 0, NULL, 1,
                             &toPresent);
        vkEndCommandBuffer(cb);
        VkPipelineStageFlags stage = VK_PIPELINE_STAGE_TRANSFER_BIT;
        VkSubmitInfo si = {VK_STRUCTURE_TYPE_SUBMIT_INFO, NULL, 1, &acquired, &stage, 1, &cb, 1,
                           &rendered};
        CHECK(vkQueueSubmit(queue, 1, &si, fence), "submit");
        VkPresentInfoKHR pi = {VK_STRUCTURE_TYPE_PRESENT_INFO_KHR, NULL, 1, &rendered, 1, &sc,
                               &idx, NULL};
        CHECK(vkQueuePresentKHR(queue, &pi), "present");
        CHECK(vkWaitForFences(dev, 1, &fence, VK_TRUE, UINT64_MAX), "fence wait");
        vkResetFences(dev, 1, &fence);
    }
    QueryPerformanceCounter(&t1);
    vkDeviceWaitIdle(dev);
    double secs = (double)(t1.QuadPart - t0.QuadPart) / (double)f.QuadPart;
    printf("PRESENT PASS %d frames in %.2f s (%.1f fps)\n", FRAMES, secs, FRAMES / secs);
    return 0;
}
