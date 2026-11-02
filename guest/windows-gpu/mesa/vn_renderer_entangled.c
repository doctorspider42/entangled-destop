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

#include "vn_renderer.h"
#include "entangled_gpu_public.h"

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
   uint32_t next_res;
   SRWLOCK lock;
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
   if (on < 0)
      on = getenv("ENTANGLED_GPU_TRACE") != NULL;
   return on;
}

static bool
ioctl(struct entangled *gpu, DWORD code, void *in, DWORD in_size, void *out,
      DWORD out_size)
{
   DWORD got = 0;
   return DeviceIoControl(gpu->dev, code, in, in_size, out, out_size, &got,
                          NULL);
}

static uint32_t
next_res(struct entangled *gpu)
{
   return (uint32_t)InterlockedIncrement((volatile LONG *)&gpu->next_res);
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
   AcquireSRWLockExclusive(&gpu->lock);
   const bool ok = ioctl(gpu, IOCTL_ENTANGLED_GPU_SUBMIT_3D, req, (DWORD)size,
                         &resp, sizeof(resp));
   ReleaseSRWLockExclusive(&gpu->lock);
   free(req);
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

static VkResult
entangled_wait(struct vn_renderer *renderer,
               const struct vn_renderer_wait *wait)
{
   (void)renderer;
   const int64_t deadline = os_time_get_absolute_timeout(wait->timeout);
   bool traced = false;
   for (;;) {
      uint32_t reached = 0;
      for (uint32_t i = 0; i < wait->sync_count; i++) {
         const struct entangled_sync *sync =
            (const struct entangled_sync *)wait->syncs[i];
         if (sync->value >= wait->sync_values[i])
            reached++;
      }
      if (wait->wait_any ? reached > 0 : reached == wait->sync_count)
         return VK_SUCCESS;
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
      Sleep(1);
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
   ENTANGLED_GPU_BLOB_IN in = {gpu->ctx_id, next_res(gpu), size, 0};
   ENTANGLED_GPU_BLOB_OUT out = {0};
   AcquireSRWLockExclusive(&gpu->lock);
   const bool ok = ioctl(gpu, IOCTL_ENTANGLED_GPU_BLOB_CREATE, &in, sizeof(in),
                         &out, sizeof(out));
   ReleaseSRWLockExclusive(&gpu->lock);
   if (!ok || !out.Address) {
      free(shmem);
      return NULL;
   }
   shmem->base.refcount = VN_REFCOUNT_INIT(1);
   shmem->base.res_id = in.ResourceId;
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
}

static void
entangled_shmem_destroy(struct vn_renderer *renderer,
                        struct vn_renderer_shmem *shmem)
{
   entangled_release((struct entangled *)renderer, shmem->res_id);
   free(shmem);
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
   ENTANGLED_GPU_BLOB_IN in = {gpu->ctx_id, next_res(gpu),
                               align64(size, 4096), mem_id};
   ENTANGLED_GPU_BLOB_OUT out = {0};
   AcquireSRWLockExclusive(&gpu->lock);
   const bool ok = ioctl(gpu, IOCTL_ENTANGLED_GPU_BLOB_CREATE, &in, sizeof(in),
                         &out, sizeof(out));
   ReleaseSRWLockExclusive(&gpu->lock);
   const bool mappable = flags & VK_MEMORY_PROPERTY_HOST_VISIBLE_BIT;
   if (!ok || (mappable && !out.Address)) {
      free(bo);
      return VK_ERROR_OUT_OF_DEVICE_MEMORY;
   }
   bo->base.refcount = VN_REFCOUNT_INIT(1);
   bo->base.res_id = in.ResourceId;
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
   if (!vn_refcount_dec(&bo->refcount))
      return false;
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
   InitializeSRWLock(&gpu->lock);
   gpu->alloc = alloc;
   gpu->next_res = 0x10000;
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
