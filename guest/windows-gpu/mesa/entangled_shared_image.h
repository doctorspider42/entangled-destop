/* SPDX-License-Identifier: MIT */
#ifndef ENTANGLED_SHARED_IMAGE_H
#define ENTANGLED_SHARED_IMAGE_H

#include <windows.h>
#include <vulkan/vulkan.h>

/* Private Venus ICD interface. Keep this object at the same address until it
 * is destroyed. The image is optimal BGRA8 with one mip, layer and sample.
 * The caller owns queue synchronization and external image layout transitions.
 * An import borrows the token during the call. An export owns its returned
 * token until destroy. Duplicate it or pin it with a WDDM allocation to share.
 */
struct entangled_shared_image {
   VkImage image;
   VkDeviceMemory memory;
   HANDLE token;
   uint32_t resource_id;
   struct entangled_shared_image *next_owned;
};

typedef VkResult (VKAPI_PTR *PFN_entangled_create_shared_image)(
   VkDevice device, VkExtent2D extent, HANDLE import_token,
   struct entangled_shared_image *shared);
typedef void (VKAPI_PTR *PFN_entangled_destroy_shared_image)(
   VkDevice device, struct entangled_shared_image *shared);

#endif
