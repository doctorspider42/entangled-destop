/*
 * Venus renderer backend for Windows guests of Entangled Desktop (its
 * ADR-0009): the guest side of virtio-gpu is entangled-gpu.sys, a KMDF
 * driver of the render-only function, spoken to through IOCTLs.
 *
 * Synchronisation is deliberately simple: a submit that signals syncs asks
 * the driver to answer only once the host retired the ring's fence, so a
 * returned submit is a finished one and a sync is a plain counter updated
 * on return.
 *
 * SPDX-License-Identifier: MIT
 */
#include <windows.h>
#include <winioctl.h>

#include "venus-protocol/vn_protocol_driver_device_memory.h"
#include "venus-protocol/vn_protocol_driver_image.h"
#include "vn_device.h"
#include "vn_entrypoints.h"
#include "vn_instance.h"
#include "vn_physical_device.h"
#include "vn_renderer.h"
#include "entangled_gpu_public.h"
#include "entangled_shared_image.h"

#define VENUS_CAPSET_ID 4

struct entangled_shmem {
   struct vn_renderer_shmem base;
};

struct entangled_bo {
   struct vn_renderer_bo base;
};

struct entangled_sync {
   struct vn_renderer_sync base;
   uint64_t value;
};

struct entangled {
   struct vn_renderer base;
   const VkAllocationCallbacks *alloc;
   HANDLE dev;
   SRWLOCK lock;
   SRWLOCK share_lock;
   struct entangled_shared_image *owned_images;
   /* Ours alone: every process (and every instance in it) has a context of
    * its own on the host, so two Vulkan programs can run at once. */
   uint32_t ctx_id;
};

struct venus_capset {
   uint32_t wire_format_version;
   uint32_t vk_xml_version;
   uint32_t vk_ext_command_serialization_spec_version;
   uint32_t vk_mesa_venus_protocol_spec_version;
   uint32_t supports_blob_id_0;
   uint32_t vk_extension_mask1[32];
};

/* ENTANGLED_GPU_TRACE=1: every submit and every wait that does not return
 * at once, on stderr - for finding what a stalled program waits for. */
static bool
trace_on(void)
{
   static int on = -1;
   if (on < 0) {
      const char *to = getenv("ENTANGLED_GPU_TRACE");
      on = to != NULL;
      /* A path rather than "1": stderr - this trace and Mesa's own log
       * (VN_DEBUG=result) - goes to that file, for a process that has no
       * console, such as a browser's GPU process. */
      if (to && (strchr(to, '\\') || strchr(to, '/')))
         freopen(to, "a", stderr);
   }
   return on;
}

/* Where this process's renderer spends its time, for the Direct3D driver's
 * statistics (entangled_renderer_stats, read and reset by name): fenced and
 * plain submits (count, microseconds, the longest), fenced submits over 2 ms,
 * and waits that did not return at once. */
enum {
   STAT_FENCED, STAT_FENCED_US, STAT_FENCED_MAX_US, STAT_FENCED_SLOW,
   STAT_PLAIN, STAT_PLAIN_US, STAT_WAITS, STAT_WAIT_US,
   /* Venus's own polling waits (vn_relax), per reason: waits, microseconds,
    * and the waits that got past busy-waiting into sleeps. */
   STAT_RELAX,
   /* Every wait with a timeout, and every poll (a zero timeout, or a read
    * of a sync): who reads what the fenced submits signal. */
   STAT_BLOCKING_WAITS = STAT_RELAX + 5 * 3, STAT_POLLS, STAT_COUNT
};
static volatile LONG64 entangled_stats[STAT_COUNT];

static int64_t
entangled_now_us(void)
{
   static LARGE_INTEGER frequency;
   LARGE_INTEGER now;
   if (!frequency.QuadPart)
      QueryPerformanceFrequency(&frequency);
   QueryPerformanceCounter(&now);
   return now.QuadPart * 1000000 / frequency.QuadPart;
}

void
entangled_note_relax(unsigned reason, int64_t ns, uint32_t iters, bool slept)
{
   (void)iters;
   if (reason >= 5)
      return;
   InterlockedIncrement64(&entangled_stats[STAT_RELAX + reason * 3]);
   InterlockedAdd64(&entangled_stats[STAT_RELAX + reason * 3 + 1], ns / 1000);
   if (slept)
      InterlockedIncrement64(&entangled_stats[STAT_RELAX + reason * 3 + 2]);
}

__declspec(dllexport) void
entangled_renderer_stats(uint64_t *out, uint32_t count)
{
   for (uint32_t i = 0; i < count && i < STAT_COUNT; i++)
      out[i] = (uint64_t)InterlockedExchange64(&entangled_stats[i], 0);
}

/* ENTANGLED_WAIT_CALLERS=1: who waits with a timeout, by call chain, for
 * entangled_renderer_callers (the Direct3D driver logs it with its
 * statistics). A measuring instrument; off unless asked for. */
#define CALLER_SLOTS 24
#define CALLER_FRAMES 10
static struct {
   uint32_t hash;
   uint32_t frames;
   LONG64 count;
   void *frame[CALLER_FRAMES];
} entangled_callers[CALLER_SLOTS];
static SRWLOCK entangled_callers_lock = SRWLOCK_INIT;

static bool
callers_on(void)
{
   static int on = -1;
   if (on < 0)
      on = getenv("ENTANGLED_WAIT_CALLERS") != NULL;
   return on;
}

static void
entangled_note_caller(void)
{
   void *frame[CALLER_FRAMES];
   const USHORT frames = RtlCaptureStackBackTrace(1, CALLER_FRAMES, frame, NULL);
   uint32_t hash = 2166136261u;
   for (USHORT i = 0; i < frames; i++)
      hash = (hash ^ (uint32_t)(uintptr_t)frame[i]) * 16777619u;
   AcquireSRWLockExclusive(&entangled_callers_lock);
   for (uint32_t i = 0; i < CALLER_SLOTS; i++) {
      if (entangled_callers[i].count && entangled_callers[i].hash != hash)
         continue;
      if (!entangled_callers[i].count) {
         entangled_callers[i].hash = hash;
         entangled_callers[i].frames = frames;
         memcpy(entangled_callers[i].frame, frame, frames * sizeof(void *));
      }
      entangled_callers[i].count++;
      break;
   }
   ReleaseSRWLockExclusive(&entangled_callers_lock);
}

/* One line per call chain seen since the last call: the count, then each
 * frame as module+offset. Returns the lines' total length. */
__declspec(dllexport) size_t
entangled_renderer_callers(char *out, size_t size)
{
   size_t used = 0;
   if (!size)
      return 0;
   out[0] = 0;
   AcquireSRWLockExclusive(&entangled_callers_lock);
   for (uint32_t i = 0; i < CALLER_SLOTS && used + 1 < size; i++) {
      if (!entangled_callers[i].count)
         continue;
      int n = snprintf(out + used, size - used, "%lld:",
                       (long long)entangled_callers[i].count);
      used += n > 0 ? (size_t)n : 0;
      for (uint32_t f = 0; f < entangled_callers[i].frames && used + 1 < size; f++) {
         HMODULE module = NULL;
         char name[MAX_PATH] = "?";
         if (GetModuleHandleExA(GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS |
                                   GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
                                (LPCSTR)entangled_callers[i].frame[f], &module) &&
             module) {
            GetModuleFileNameA(module, name, sizeof(name));
         }
         const char *base = strrchr(name, '\\');
         base = base ? base + 1 : name;
         /* Short, so a chain fits a log line: D the Direct3D driver (Zink
          * inside), V this one, else the module's first letters. */
         if (_strnicmp(base, "entangled_d3d10", 15) == 0)
            base = "D";
         else if (_strnicmp(base, "vulkan_virtio", 13) == 0)
            base = "V";
         n = snprintf(out + used, size - used, " %.6s+%llx", base,
                      (unsigned long long)((uintptr_t)entangled_callers[i].frame[f] -
                                           (uintptr_t)module));
         used += n > 0 ? (size_t)n : 0;
      }
      if (used + 1 < size)
         out[used++] = '\n';
      out[used] = 0;
      entangled_callers[i].count = 0;
   }
   ReleaseSRWLockExclusive(&entangled_callers_lock);
   if (used >= size)
      used = size - 1;
   out[used] = 0;
   return used;
}

static bool
ioctl(struct entangled *gpu, DWORD code, void *in, DWORD in_size, void *out,
      DWORD out_size)
{
   DWORD got = 0;
   return DeviceIoControl(gpu->dev, code, in, in_size, out, out_size, &got,
                          NULL);
}

static void
entangled_destroy(struct vn_renderer *renderer,
                  const VkAllocationCallbacks *alloc)
{
   struct entangled *gpu = (struct entangled *)renderer;
   ENTANGLED_GPU_CTX ctx = {gpu->ctx_id};
   ENTANGLED_GPU_RESPONSE resp;
   ioctl(gpu, IOCTL_ENTANGLED_GPU_CTX_DESTROY, &ctx, sizeof(ctx), &resp,
         sizeof(resp));
   CloseHandle(gpu->dev);
   vk_free(alloc, gpu);
}

static VkResult
entangled_submit(struct vn_renderer *renderer,
                 const struct vn_renderer_submit_batch *batch)
{
   struct entangled *gpu = (struct entangled *)renderer;
   if (batch->cs_size > ENTANGLED_GPU_MAX_SUBMIT)
      return VK_ERROR_OUT_OF_DEVICE_MEMORY;

   const size_t size = sizeof(ENTANGLED_GPU_SUBMIT) + batch->cs_size;
   ENTANGLED_GPU_SUBMIT *req = malloc(size);
   if (!req)
      return VK_ERROR_OUT_OF_HOST_MEMORY;
   memset(req, 0, sizeof(*req));
   req->CtxId = gpu->ctx_id;
   req->Size = (unsigned long)batch->cs_size;
   req->RingIdx = batch->ring_idx;
   req->Flags = batch->sync_count ? ENTANGLED_GPU_SUBMIT_FENCE : 0;
   if (batch->cs_size)
      memcpy(req + 1, batch->cs_data, batch->cs_size);

   ENTANGLED_GPU_RESPONSE resp = {0};
   const int64_t start = entangled_now_us();
   AcquireSRWLockExclusive(&gpu->lock);
   const bool ok = ioctl(gpu, IOCTL_ENTANGLED_GPU_SUBMIT_3D, req, (DWORD)size,
                         &resp, sizeof(resp));
   ReleaseSRWLockExclusive(&gpu->lock);
   free(req);
   {
      const LONG64 took = entangled_now_us() - start;
      if (batch->sync_count) {
         InterlockedIncrement64(&entangled_stats[STAT_FENCED]);
         InterlockedAdd64(&entangled_stats[STAT_FENCED_US], took);
         if (took > 2000)
            InterlockedIncrement64(&entangled_stats[STAT_FENCED_SLOW]);
         for (LONG64 max = entangled_stats[STAT_FENCED_MAX_US]; took > max;
              max = entangled_stats[STAT_FENCED_MAX_US]) {
            if (InterlockedCompareExchange64(&entangled_stats[STAT_FENCED_MAX_US],
                                             took, max) == max)
               break;
         }
      } else {
         InterlockedIncrement64(&entangled_stats[STAT_PLAIN]);
         InterlockedAdd64(&entangled_stats[STAT_PLAIN_US], took);
      }
   }
   if (trace_on()) {
      fprintf(stderr, "entangled: submit tid %lu ring %u size %zu syncs %u ->%s 0x%x\n",
              GetCurrentThreadId(), batch->ring_idx, batch->cs_size,
              batch->sync_count, ok ? "" : " failed", resp.Type);
      for (uint32_t i = 0; i < batch->sync_count; i++)
         fprintf(stderr, "entangled:   sync %p = %llu\n", (void *)batch->syncs[i],
                 (unsigned long long)batch->sync_values[i]);
   }
   if (!ok || resp.Type != 0x1100)
      return VK_ERROR_DEVICE_LOST;

   /* The fence retired before the driver answered: the syncs are reached. */
   for (uint32_t i = 0; i < batch->sync_count; i++) {
      struct entangled_sync *sync = (struct entangled_sync *)batch->syncs[i];
      sync->value = batch->sync_values[i];
   }
   return VK_SUCCESS;
}

/* ADR-0009 phase E: a window's finished frame shown by the host over the
 * desktop, at its place, instead of copied through the window. */
static VkResult
entangled_overlay(struct vn_renderer *renderer,
                  struct vn_renderer_bo *bo,
                  uint32_t width,
                  uint32_t height,
                  uint32_t stride,
                  int32_t x,
                  int32_t y)
{
   struct entangled *gpu = (struct entangled *)renderer;
   ENTANGLED_GPU_OVERLAY in = {bo ? bo->res_id : 0, width, height, stride, x, y};
   ENTANGLED_GPU_RESPONSE resp = {0};
   AcquireSRWLockExclusive(&gpu->lock);
   const bool ok = ioctl(gpu, IOCTL_ENTANGLED_GPU_OVERLAY, &in, sizeof(in),
                         &resp, sizeof(resp));
   ReleaseSRWLockExclusive(&gpu->lock);
   if (trace_on())
      fprintf(stderr, "entangled: overlay %u %ux%u at %d,%d ->%s 0x%x\n",
              in.ResourceId, width, height, x, y, ok ? "" : " failed", resp.Type);
   return ok && resp.Type == 0x1100 ? VK_SUCCESS : VK_ERROR_UNKNOWN;
}

static VkResult
entangled_wait(struct vn_renderer *renderer,
               const struct vn_renderer_wait *wait)
{
   (void)renderer;
   const int64_t deadline = os_time_get_absolute_timeout(wait->timeout);
   const int64_t start = entangled_now_us();
   const bool blocking = wait->timeout != 0;
   InterlockedIncrement64(&entangled_stats[blocking ? STAT_BLOCKING_WAITS : STAT_POLLS]);
   if (blocking && callers_on())
      entangled_note_caller();
   bool traced = false, slept = false;
   for (;;) {
      uint32_t reached = 0;
      for (uint32_t i = 0; i < wait->sync_count; i++) {
         const struct entangled_sync *sync =
            (const struct entangled_sync *)wait->syncs[i];
         if (sync->value >= wait->sync_values[i])
            reached++;
      }
      if (wait->wait_any ? reached > 0 : reached == wait->sync_count) {
         if (slept) {
            InterlockedIncrement64(&entangled_stats[STAT_WAITS]);
            InterlockedAdd64(&entangled_stats[STAT_WAIT_US], entangled_now_us() - start);
         }
         return VK_SUCCESS;
      }
      if (os_time_timeout(0, deadline, os_time_get_nano()))
         return VK_TIMEOUT;
      if (trace_on() && !traced) {
         traced = true;
         fprintf(stderr, "entangled: wait tid %lu any %d timeout %llu\n",
                 GetCurrentThreadId(), wait->wait_any,
                 (unsigned long long)wait->timeout);
         for (uint32_t i = 0; i < wait->sync_count; i++)
            fprintf(stderr, "entangled:   sync %p at %llu for %llu\n",
                    (void *)wait->syncs[i],
                    (unsigned long long)((const struct entangled_sync *)
                                            wait->syncs[i])->value,
                    (unsigned long long)wait->sync_values[i]);
      }
      slept = true;
      /* Not Sleep(1): that is a whole timer tick (15.6 ms) on Windows. */
      vn_sleep_us(200);
   }
}

static struct vn_renderer_shmem *
entangled_shmem_create(struct vn_renderer *renderer, size_t size)
{
   struct entangled *gpu = (struct entangled *)renderer;
   struct entangled_shmem *shmem = calloc(1, sizeof(*shmem));
   if (!shmem)
      return NULL;
   size = align64(size, 4096);
   ENTANGLED_GPU_BLOB_IN in = {gpu->ctx_id, 0, size, 0};
   ENTANGLED_GPU_BLOB_OUT out = {0};
   AcquireSRWLockExclusive(&gpu->lock);
   const bool ok = ioctl(gpu, IOCTL_ENTANGLED_GPU_BLOB_CREATE, &in, sizeof(in),
                         &out, sizeof(out));
   ReleaseSRWLockExclusive(&gpu->lock);
   if (trace_on())
      fprintf(stderr, "entangled: shmem %u size %zu ->%s\n", out.ResourceId,
              size, ok && out.Address ? "" : " failed");
   if (!ok || !out.Address) {
      free(shmem);
      return NULL;
   }
   shmem->base.refcount = VN_REFCOUNT_INIT(1);
   shmem->base.res_id = out.ResourceId;
   shmem->base.mmap_size = size;
   shmem->base.mmap_ptr = (void *)(uintptr_t)out.Address;
   return &shmem->base;
}

static void
entangled_release(struct entangled *gpu, uint32_t res_id)
{
   ENTANGLED_GPU_BLOB_REF ref = {gpu->ctx_id, res_id};
   ENTANGLED_GPU_RESPONSE resp;
   AcquireSRWLockExclusive(&gpu->lock);
   ioctl(gpu, IOCTL_ENTANGLED_GPU_BLOB_DESTROY, &ref, sizeof(ref), &resp,
         sizeof(resp));
   ReleaseSRWLockExclusive(&gpu->lock);
   if (trace_on())
      fprintf(stderr, "entangled: release %u\n", res_id);
}

static void
entangled_shmem_destroy(struct vn_renderer *renderer,
                        struct vn_renderer_shmem *shmem)
{
   entangled_release((struct entangled *)renderer, shmem->res_id);
   free(shmem);
}

static VkResult
entangled_share_image_memory(struct vn_device *dev,
                             struct entangled_shared_image *shared,
                             const VkMemoryRequirements *requirements,
                             HANDLE import_token)
{
   struct entangled *gpu = (struct entangled *)dev->renderer;
   VkDevice device = vn_device_to_handle(dev);
   VkResult result = VK_ERROR_INVALID_EXTERNAL_HANDLE;
   uint32_t types = requirements->memoryTypeBits;
   ENTANGLED_GPU_BLOB_REF ref = {gpu->ctx_id, 0};

   const VkMemoryDedicatedAllocateInfo dedicated = {
      .sType = VK_STRUCTURE_TYPE_MEMORY_DEDICATED_ALLOCATE_INFO,
      .image = shared->image,
   };
   const VkExportMemoryAllocateInfo export = {
      .sType = VK_STRUCTURE_TYPE_EXPORT_MEMORY_ALLOCATE_INFO,
      .pNext = &dedicated,
      .handleTypes = VK_EXTERNAL_MEMORY_HANDLE_TYPE_DMA_BUF_BIT_EXT,
   };
   VkImportMemoryResourceInfoMESA import = {
      .sType = VK_STRUCTURE_TYPE_IMPORT_MEMORY_RESOURCE_INFO_MESA,
      .pNext = &dedicated,
   };

   if (import_token) {
      ENTANGLED_GPU_BLOB_TOKEN token = {(uintptr_t)import_token};
      AcquireSRWLockExclusive(&gpu->lock);
      const bool ok = ioctl(gpu, IOCTL_ENTANGLED_GPU_BLOB_ATTACH, &token,
                            sizeof(token), &ref, sizeof(ref));
      ReleaseSRWLockExclusive(&gpu->lock);
      if (!ok)
         return result;

      /* Serialize attach through release on this context. The IOCTL returns
       * the same resource id for repeated attaches to the same token. */
      for (const struct entangled_shared_image *owner = gpu->owned_images;
           owner; owner = owner->next_owned) {
         if (owner->resource_id == ref.ResourceId)
            return result;
      }
      import.resourceId = ref.ResourceId;
      VkMemoryResourcePropertiesMESA props = {
         .sType = VK_STRUCTURE_TYPE_MEMORY_RESOURCE_PROPERTIES_MESA,
      };
      result = vn_call_vkGetMemoryResourcePropertiesMESA(
         dev->primary_ring, device, ref.ResourceId, &props);
      if (result != VK_SUCCESS)
         goto release;
      types &= props.memoryTypeBits;
   }

   const VkPhysicalDeviceMemoryProperties *props =
      &dev->physical_device->memory_properties;
   uint32_t index;
   for (index = 0; index < props->memoryTypeCount; index++) {
      const VkMemoryPropertyFlags flags = props->memoryTypes[index].propertyFlags;
      if ((types & (1u << index)) &&
          (flags & VK_MEMORY_PROPERTY_DEVICE_LOCAL_BIT) &&
          !(flags & VK_MEMORY_PROPERTY_HOST_VISIBLE_BIT))
         break;
   }
   if (index == props->memoryTypeCount) {
      result = VK_ERROR_FEATURE_NOT_PRESENT;
      goto release;
   }

   VkMemoryAllocateInfo alloc = {
      .sType = VK_STRUCTURE_TYPE_MEMORY_ALLOCATE_INFO,
      .allocationSize = requirements->size,
      .memoryTypeIndex = index,
   };
   struct vn_device_memory *mem = vk_device_memory_create(
      &dev->base.vk, &alloc, NULL, sizeof(*mem));
   if (!mem) {
      result = VK_ERROR_OUT_OF_HOST_MEMORY;
      goto release;
   }
   vn_object_set_id(mem, vn_get_next_obj_id(), VK_OBJECT_TYPE_DEVICE_MEMORY);
   VkDeviceMemory memory = vn_device_memory_to_handle(mem);
   alloc.pNext = import_token ? (const void *)&import : (const void *)&export;
   result = vn_call_vkAllocateMemory(dev->primary_ring, device, &alloc, NULL,
                                    &memory);
   if (result != VK_SUCCESS) {
      vk_device_memory_destroy(&dev->base.vk, NULL, &mem->base.vk);
      goto release;
   }
   shared->memory = memory;

   result = vn_call_vkBindImageMemory(dev->primary_ring, device, shared->image,
                                     shared->memory, 0);
   if (result != VK_SUCCESS || import_token)
      goto release;

   ENTANGLED_GPU_BLOB_IN in = {gpu->ctx_id, 0, align64(requirements->size, 4096),
                              mem->base.id};
   ENTANGLED_GPU_BLOB_OUT out = {0};
   ENTANGLED_GPU_BLOB_TOKEN token = {0};
   AcquireSRWLockExclusive(&gpu->lock);
   bool published = ioctl(gpu, IOCTL_ENTANGLED_GPU_BLOB_CREATE_UNMAPPED, &in,
                          sizeof(in), &out, sizeof(out));
   if (published) {
      ref.ResourceId = out.ResourceId;
      published = ioctl(gpu, IOCTL_ENTANGLED_GPU_BLOB_PUBLISH, &ref,
                        sizeof(ref), &token, sizeof(token));
   }
   ReleaseSRWLockExclusive(&gpu->lock);
   if (published) {
      shared->resource_id = out.ResourceId;
      shared->token = (HANDLE)(uintptr_t)token.Handle;
      shared->next_owned = gpu->owned_images;
      gpu->owned_images = shared;
      return VK_SUCCESS;
   }
   result = VK_ERROR_INVALID_EXTERNAL_HANDLE;

release:
   if (ref.ResourceId)
      entangled_release(gpu, ref.ResourceId);
   return result;
}

__declspec(dllexport) void VKAPI_CALL
entangled_destroy_shared_image(VkDevice device, struct entangled_shared_image *shared)
{
   struct vn_device *dev = vn_device_from_handle(device);
   if (shared->token) {
      struct entangled *gpu = (struct entangled *)dev->renderer;
      AcquireSRWLockExclusive(&gpu->share_lock);
      struct entangled_shared_image **owner = &gpu->owned_images;
      while (*owner != shared)
         owner = &(*owner)->next_owned;
      *owner = shared->next_owned;
      CloseHandle(shared->token);
      entangled_release(gpu, shared->resource_id);
      ReleaseSRWLockExclusive(&gpu->share_lock);
   }
   vn_DestroyImage(device, shared->image, NULL);
   vn_FreeMemory(device, shared->memory, NULL);
   *shared = (struct entangled_shared_image){0};
}

__declspec(dllexport) VkResult VKAPI_CALL
entangled_create_shared_image(VkDevice device, VkExtent2D extent,
                              HANDLE import_token,
                              struct entangled_shared_image *shared)
{
   struct vn_device *dev = vn_device_from_handle(device);
   *shared = (struct entangled_shared_image){0};
   if (dev->renderer->ops.destroy != entangled_destroy ||
       dev->physical_device->external_memory.renderer_handle_type !=
          VK_EXTERNAL_MEMORY_HANDLE_TYPE_DMA_BUF_BIT_EXT)
      return VK_ERROR_FEATURE_NOT_PRESENT;

   if (!extent.width || !extent.height)
      return VK_ERROR_FORMAT_NOT_SUPPORTED;

   const VkExternalMemoryImageCreateInfo external = {
      .sType = VK_STRUCTURE_TYPE_EXTERNAL_MEMORY_IMAGE_CREATE_INFO,
      .handleTypes = VK_EXTERNAL_MEMORY_HANDLE_TYPE_DMA_BUF_BIT_EXT,
   };
   const VkImageCreateInfo create = {
      .sType = VK_STRUCTURE_TYPE_IMAGE_CREATE_INFO,
      .pNext = &external,
      .flags = VK_IMAGE_CREATE_ALIAS_BIT,
      .imageType = VK_IMAGE_TYPE_2D,
      .format = VK_FORMAT_B8G8R8A8_UNORM,
      .extent = {extent.width, extent.height, 1},
      .mipLevels = 1,
      .arrayLayers = 1,
      .samples = VK_SAMPLE_COUNT_1_BIT,
      .tiling = VK_IMAGE_TILING_OPTIMAL,
      .usage = VK_IMAGE_USAGE_TRANSFER_SRC_BIT | VK_IMAGE_USAGE_TRANSFER_DST_BIT |
               VK_IMAGE_USAGE_SAMPLED_BIT | VK_IMAGE_USAGE_COLOR_ATTACHMENT_BIT,
      .sharingMode = VK_SHARING_MODE_EXCLUSIVE,
      .initialLayout = VK_IMAGE_LAYOUT_UNDEFINED,
   };
   VkResult result = vn_CreateImage(device, &create, NULL, &shared->image);
   if (result != VK_SUCCESS)
      return result;

   VkMemoryRequirements requirements;
   dev->base.vk.dispatch_table.GetImageMemoryRequirements(device, shared->image,
                                                          &requirements);
   struct entangled *gpu = (struct entangled *)dev->renderer;
   AcquireSRWLockExclusive(&gpu->share_lock);
   result = entangled_share_image_memory(dev, shared, &requirements, import_token);
   ReleaseSRWLockExclusive(&gpu->share_lock);
   if (result != VK_SUCCESS)
      entangled_destroy_shared_image(device, shared);
   return result;
}

static VkResult
entangled_bo_create_from_device_memory(
   struct vn_renderer *renderer,
   struct vn_renderer_submit_batch *batch,
   VkDeviceSize size,
   vn_object_id mem_id,
   VkMemoryPropertyFlags flags,
   VkExternalMemoryHandleTypeFlags external_handles,
   struct vn_renderer_bo **out_bo)
{
   struct entangled *gpu = (struct entangled *)renderer;
   (void)external_handles;
   /* The allocation the blob names is in this batch: run it first. */
   if (batch && batch->cs_size) {
      VkResult result = entangled_submit(renderer, batch);
      if (result != VK_SUCCESS)
         return result;
      batch->cs_size = 0;
   }
   struct entangled_bo *bo = calloc(1, sizeof(*bo));
   if (!bo)
      return VK_ERROR_OUT_OF_HOST_MEMORY;
   ENTANGLED_GPU_BLOB_IN in = {gpu->ctx_id, 0,
                               align64(size, 4096), mem_id};
   ENTANGLED_GPU_BLOB_OUT out = {0};
   AcquireSRWLockExclusive(&gpu->lock);
   const bool ok = ioctl(gpu, IOCTL_ENTANGLED_GPU_BLOB_CREATE, &in, sizeof(in),
                         &out, sizeof(out));
   ReleaseSRWLockExclusive(&gpu->lock);
   const bool mappable = flags & VK_MEMORY_PROPERTY_HOST_VISIBLE_BIT;
   if (trace_on())
      fprintf(stderr, "entangled: bo %u size %llu flags 0x%x ->%s 0x%x\n",
              out.ResourceId, (unsigned long long)in.Size, flags,
              ok && (!mappable || out.Address) ? "" : " failed", out.Response);
   if (!ok || (mappable && !out.Address)) {
      free(bo);
      return VK_ERROR_OUT_OF_DEVICE_MEMORY;
   }
   bo->base.refcount = VN_REFCOUNT_INIT(1);
   bo->base.res_id = out.ResourceId;
   bo->base.mmap_size = in.Size;
   bo->base.mmap_ptr = (void *)(uintptr_t)out.Address;
   *out_bo = &bo->base;
   return VK_SUCCESS;
}

static VkResult
entangled_bo_create_from_dma_buf(struct vn_renderer *renderer,
                                 VkDeviceSize size,
                                 int fd,
                                 VkMemoryPropertyFlags flags,
                                 struct vn_renderer_bo **out_bo)
{
   (void)renderer;
   (void)size;
   (void)fd;
   (void)flags;
   (void)out_bo;
   return VK_ERROR_INVALID_EXTERNAL_HANDLE;
}

static bool
entangled_bo_destroy(struct vn_renderer *renderer, struct vn_renderer_bo *bo)
{
   /* The last reference is already gone: vn_renderer_bo_unref drops it and
    * calls this only then. Dropping it again here kept every bo, and the
    * host pages behind it, until the process ended. */
   entangled_release((struct entangled *)renderer, bo->res_id);
   free(bo);
   return true;
}

static int
entangled_bo_export(struct vn_renderer *renderer, struct vn_renderer_bo *bo)
{
   (void)renderer;
   (void)bo;
   return -1;
}

static void *
entangled_bo_map(struct vn_renderer *renderer,
                 struct vn_renderer_bo *bo,
                 void *placed_addr)
{
   (void)renderer;
   return placed_addr ? NULL : bo->mmap_ptr;
}

static void
entangled_bo_flush(struct vn_renderer *renderer,
                   struct vn_renderer_bo *bo,
                   VkDeviceSize offset,
                   VkDeviceSize size)
{
   /* Host memory behind a cached mapping: coherent. */
   (void)renderer;
   (void)bo;
   (void)offset;
   (void)size;
}

static VkResult
entangled_sync_create(struct vn_renderer *renderer,
                      uint64_t initial_val,
                      struct vn_renderer_sync **out_sync)
{
   (void)renderer;
   struct entangled_sync *sync = calloc(1, sizeof(*sync));
   if (!sync)
      return VK_ERROR_OUT_OF_HOST_MEMORY;
   sync->value = initial_val;
   *out_sync = &sync->base;
   return VK_SUCCESS;
}

static VkResult
entangled_sync_create_from_syncobj(struct vn_renderer *renderer,
                                   int fd,
                                   bool sync_file,
                                   struct vn_renderer_sync **out_sync)
{
   (void)renderer;
   (void)fd;
   (void)sync_file;
   (void)out_sync;
   return VK_ERROR_INVALID_EXTERNAL_HANDLE;
}

static void
entangled_sync_destroy(struct vn_renderer *renderer,
                       struct vn_renderer_sync *sync)
{
   (void)renderer;
   free(sync);
}

static int
entangled_sync_export(struct vn_renderer *renderer,
                      struct vn_renderer_sync *sync,
                      bool sync_file)
{
   (void)renderer;
   (void)sync;
   (void)sync_file;
   return -1;
}

static VkResult
entangled_sync_reset(struct vn_renderer *renderer,
                     struct vn_renderer_sync *sync)
{
   (void)renderer;
   ((struct entangled_sync *)sync)->value = 0;
   return VK_SUCCESS;
}

static VkResult
entangled_sync_read(struct vn_renderer *renderer,
                    struct vn_renderer_sync *sync,
                    uint64_t *val)
{
   (void)renderer;
   /* A read is a poll (vkGetFenceStatus, vkGetSemaphoreCounterValue). */
   InterlockedIncrement64(&entangled_stats[STAT_POLLS]);
   *val = ((struct entangled_sync *)sync)->value;
   return VK_SUCCESS;
}

static VkResult
entangled_sync_write(struct vn_renderer *renderer,
                     struct vn_renderer_sync *sync,
                     uint64_t val)
{
   (void)renderer;
   ((struct entangled_sync *)sync)->value = val;
   return VK_SUCCESS;
}

VkResult
vn_renderer_create_entangled(struct vn_instance *instance,
                             const VkAllocationCallbacks *alloc,
                             struct vn_renderer **renderer)
{
   (void)instance;
   struct entangled *gpu = vk_zalloc(alloc, sizeof(*gpu), VN_DEFAULT_ALIGN,
                                     VK_SYSTEM_ALLOCATION_SCOPE_INSTANCE);
   if (!gpu)
      return VK_ERROR_OUT_OF_HOST_MEMORY;
   /* Venus's asynchronous present (a thread of its own, vn_wsi.c) stalls on
    * this backend after a few to a few dozen frames - Zink's every
    * SwapBuffers goes through it - with no sync waited on and the host
    * idle. Presenting on the caller's thread draws every frame. */
   vn_env.perf |= VN_PERF_NO_ASYNC_PRESENT;
   /* The feedback pool grows 4 KiB at a time, one blob each, and under
    * ANGLE (a browser's WebGL) it never stops growing: past a thousand
    * blobs the next allocation fails and the GPU process loses its context.
    * Without feedback, fences, semaphores and queries are answered by the
    * host instead. */
   vn_env.perf |= VN_PERF_NO_EVENT_FEEDBACK | VN_PERF_NO_SEMAPHORE_FEEDBACK |
                  VN_PERF_NO_QUERY_FEEDBACK;
   if (trace_on())
      fprintf(stderr, "entangled: renderer for process %lu\n", GetCurrentProcessId());
   InitializeSRWLock(&gpu->lock);
   InitializeSRWLock(&gpu->share_lock);
   gpu->alloc = alloc;
   static volatile LONG instances;
   gpu->ctx_id = (GetCurrentProcessId() & 0xffffff) << 8 |
                 ((uint32_t)InterlockedIncrement(&instances) & 0xff);
   gpu->dev = CreateFileW(ENTANGLED_GPU_DEVICE_NAME, GENERIC_READ | GENERIC_WRITE,
                          FILE_SHARE_READ | FILE_SHARE_WRITE, NULL,
                          OPEN_EXISTING, 0, NULL);
   if (gpu->dev == INVALID_HANDLE_VALUE) {
      vk_free(alloc, gpu);
      return VK_ERROR_INITIALIZATION_FAILED;
   }

   ENTANGLED_GPU_CAPSET_IN cin = {VENUS_CAPSET_ID, 0};
   ENTANGLED_GPU_CAPSET_OUT *cout = calloc(1, sizeof(*cout));
   ENTANGLED_GPU_CTX_CREATE create = {gpu->ctx_id, VENUS_CAPSET_ID};
   ENTANGLED_GPU_RESPONSE created = {0};
   if (!cout ||
       !ioctl(gpu, IOCTL_ENTANGLED_GPU_CAPSET, &cin, sizeof(cin), cout,
              sizeof(*cout)) ||
       cout->Response != 0x1103 ||
       !ioctl(gpu, IOCTL_ENTANGLED_GPU_CTX_CREATE, &create, sizeof(create),
              &created, sizeof(created)) ||
       created.Type != 0x1100) {
      free(cout);
      CloseHandle(gpu->dev);
      vk_free(alloc, gpu);
      return VK_ERROR_INITIALIZATION_FAILED;
   }
   const struct venus_capset *capset = (const struct venus_capset *)cout->Data;
   struct vn_renderer_info *info = &gpu->base.info;
   info->pci.vendor_id = 0x1af4;
   info->pci.device_id = 0x1050;
   info->has_dma_buf_import = false;
   info->has_external_sync = false;
   info->has_timeline_sync = true;
   info->has_implicit_fencing = false;
   info->max_timeline_count = 64;
   info->wire_format_version = capset->wire_format_version;
   info->vk_xml_version = capset->vk_xml_version;
   info->vk_ext_command_serialization_spec_version =
      capset->vk_ext_command_serialization_spec_version;
   info->vk_mesa_venus_protocol_spec_version =
      capset->vk_mesa_venus_protocol_spec_version;
   memcpy(info->vk_extension_mask, capset->vk_extension_mask1,
          sizeof(capset->vk_extension_mask1));
   free(cout);

   gpu->base.ops.destroy = entangled_destroy;
   gpu->base.ops.submit = entangled_submit;
   gpu->base.ops.wait = entangled_wait;
   gpu->base.ops.overlay = entangled_overlay;
   gpu->base.shmem_ops.create = entangled_shmem_create;
   gpu->base.shmem_ops.destroy = entangled_shmem_destroy;
   gpu->base.bo_ops.create_from_device_memory =
      entangled_bo_create_from_device_memory;
   gpu->base.bo_ops.create_from_dma_buf = entangled_bo_create_from_dma_buf;
   gpu->base.bo_ops.destroy = entangled_bo_destroy;
   gpu->base.bo_ops.export_dma_buf = entangled_bo_export;
   gpu->base.bo_ops.export_sync_file = entangled_bo_export;
   gpu->base.bo_ops.map = entangled_bo_map;
   gpu->base.bo_ops.flush = entangled_bo_flush;
   gpu->base.bo_ops.invalidate = entangled_bo_flush;
   gpu->base.sync_ops.create = entangled_sync_create;
   gpu->base.sync_ops.create_from_syncobj = entangled_sync_create_from_syncobj;
   gpu->base.sync_ops.destroy = entangled_sync_destroy;
   gpu->base.sync_ops.export_syncobj = entangled_sync_export;
   gpu->base.sync_ops.reset = entangled_sync_reset;
   gpu->base.sync_ops.read = entangled_sync_read;
   gpu->base.sync_ops.write = entangled_sync_write;

   *renderer = &gpu->base;
   return VK_SUCCESS;
}
