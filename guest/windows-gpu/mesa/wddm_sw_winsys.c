/*
 * Copyright 2026 Entangled Desktop contributors
 * SPDX-License-Identifier: MIT
 *
 * A software winsys over a WDDM kernel driver (ADR-0010, phase W3'): the
 * display targets of a software rasteriser are WDDM allocations of
 * entangled-wddm.sys, so what Direct3D draws into a back buffer, a primary or
 * a shared surface is memory the kernel driver can present.
 *
 * Copied by prepare-d3d10.ps1 into Mesa's src/gallium/winsys/sw/wddm. It
 * speaks to the kernel through gdikmt (Mesa MR 24223) and uses the private
 * data of src/virtio/wddm/viogpu_wddm_driver.h, the contract the kernel
 * driver's allocations are created with.
 */

#include <windows.h>

#include "util/format/u_format.h"
#include "util/log.h"
#include "util/u_debug.h"
#include "util/u_math.h"
#include "util/u_memory.h"

#include "frontend/sw_winsys.h"
#include "frontend/winsys_handle.h"
#include "gdikmt/gdikmt.h"
#include "pipe/p_state.h"

#include "wddm/viogpu_wddm_driver.h"
#include "wddm_sw_winsys.h"

/* virtio-gpu's numbers for the two layouts a 2D resource has. */
#define WDDM_SW_FORMAT_B8G8R8A8 1
#define WDDM_SW_FORMAT_B8G8R8X8 2

struct wddm_sw_displaytarget {
   enum pipe_format format;
   unsigned width;
   unsigned height;
   unsigned stride;

   D3DKMT_HANDLE hAllocation;
   HANDLE hResource;
   /* Opened from another device's resource: the runtime owns the handles. */
   bool opened;
   void *ptr;
   /* An opened allocation's pixels, as last read through the driver; the
    * driver's write count they were read at; and whether the last map found
    * them changed. */
   void *copy;
   uint64_t seen;
   bool changed;
};

struct wddm_sw_winsys {
   struct sw_winsys base;
   struct gdikmt_device *device;
   struct gdikmt_context *ctx;
   /* Locks last as long as a map (wddm_sw_winsys_lock_per_map). */
   bool lock_per_map;
};

static inline struct wddm_sw_winsys *
wddm_sw_winsys(struct sw_winsys *ws)
{
   return (struct wddm_sw_winsys *)ws;
}

static inline struct wddm_sw_displaytarget *
wddm_sw_displaytarget(struct sw_displaytarget *dt)
{
   return (struct wddm_sw_displaytarget *)dt;
}

static unsigned
wddm_sw_format(enum pipe_format format)
{
   switch (format) {
   case PIPE_FORMAT_B8G8R8A8_UNORM:
   case PIPE_FORMAT_B8G8R8A8_SRGB:
      return WDDM_SW_FORMAT_B8G8R8A8;
   case PIPE_FORMAT_B8G8R8X8_UNORM:
   case PIPE_FORMAT_B8G8R8X8_SRGB:
      return WDDM_SW_FORMAT_B8G8R8X8;
   default:
      return 0;
   }
}

static bool
wddm_sw_is_displaytarget_format_supported(struct sw_winsys *ws,
                                          unsigned tex_usage,
                                          enum pipe_format format)
{
   return wddm_sw_format(format) != 0;
}

static void
wddm_sw_displaytarget_destroy(struct sw_winsys *ws, struct sw_displaytarget *dt)
{
   struct wddm_sw_winsys *wws = wddm_sw_winsys(ws);
   struct wddm_sw_displaytarget *wdt = wddm_sw_displaytarget(dt);

   if (!wdt->opened && wdt->hAllocation) {
      wws->device->destroyAllocation(wws->device, wdt->hResource,
                                     wdt->hAllocation);
   }
   FREE(wdt->copy);
   FREE(wdt);
}

static struct sw_displaytarget *
wddm_sw_displaytarget_create(struct sw_winsys *ws, unsigned tex_usage,
                             enum pipe_format format, unsigned width,
                             unsigned height, unsigned alignment,
                             const void *front_private, unsigned *stride)
{
   struct wddm_sw_winsys *wws = wddm_sw_winsys(ws);
   struct wddm_sw_displaytarget *wdt;
   VIOGPU_CREATE_RESOURCE_EXCHANGE resource_exchange;
   VIOGPU_CREATE_ALLOCATION_EXCHANGE allocation_exchange;
   D3DDDI_ALLOCATIONINFO allocation_info;
   struct gdikmt_createallocation create;
   NTSTATUS status;

   if (!wddm_sw_format(format))
      return NULL;

   wdt = CALLOC_STRUCT(wddm_sw_displaytarget);
   if (!wdt)
      return NULL;

   wdt->format = format;
   wdt->width = width;
   wdt->height = height;
   /* Four bytes a pixel and no padding: the layout the kernel driver's copy
    * and the host's 2D resource both assume. */
   wdt->stride = width * 4;

   memset(&resource_exchange, 0, sizeof resource_exchange);
   memset(&allocation_exchange, 0, sizeof allocation_exchange);
   allocation_exchange.ResourceOptions.target = PIPE_TEXTURE_2D;
   allocation_exchange.ResourceOptions.format = wddm_sw_format(format);
   allocation_exchange.ResourceOptions.bind = 0;
   allocation_exchange.ResourceOptions.width = width;
   allocation_exchange.ResourceOptions.height = height;
   allocation_exchange.ResourceOptions.depth = 1;
   allocation_exchange.ResourceOptions.array_size = 1;
   allocation_exchange.ResourceOptions.last_level = 0;
   allocation_exchange.ResourceOptions.nr_samples = 0;
   allocation_exchange.ResourceOptions.flags = 0;
   allocation_exchange.Size = (ULONGLONG)wdt->stride * height;

   memset(&allocation_info, 0, sizeof allocation_info);
   allocation_info.pPrivateDriverData = &allocation_exchange;
   allocation_info.PrivateDriverDataSize = sizeof allocation_exchange;

   memset(&create, 0, sizeof create);
   create.NumAllocations = 1;
   create.pAllocationInfo = &allocation_info;
   create.pPrivateDriverData = &resource_exchange;
   create.PrivateDriverDataSize = sizeof resource_exchange;

   status = wws->device->createAllocation(wws->device, &create);
   if (!NT_SUCCESS(status) || !allocation_info.hAllocation) {
      debug_printf("wddm_sw: createAllocation %ux%u failed 0x%lx\n", width,
                   height, (unsigned long)status);
      FREE(wdt);
      return NULL;
   }
   wdt->hAllocation = allocation_info.hAllocation;
   wdt->hResource = create.hResource;

   *stride = wdt->stride;
   return (struct sw_displaytarget *)wdt;
}

static struct sw_displaytarget *
wddm_sw_displaytarget_from_handle(struct sw_winsys *ws,
                                  const struct pipe_resource *templat,
                                  struct winsys_handle *whandle,
                                  unsigned *stride)
{
   struct wddm_sw_winsys *wws = wddm_sw_winsys(ws);
   struct wddm_sw_displaytarget *wdt = NULL;
   struct gdikmt_openallocation open;
   D3DDDI_OPENALLOCATIONINFO *info = NULL;
   VIOGPU_CREATE_ALLOCATION_EXCHANGE *exchange;
   NTSTATUS status;

   memset(&open, 0, sizeof open);
   open.hGlobalHandle = (D3DKMT_HANDLE)(uintptr_t)whandle->handle;
   status = wws->device->queryAllocation(wws->device, &open);
   if (!NT_SUCCESS(status) || open.NumAllocations < 1)
      return NULL;

   info = CALLOC(open.NumAllocations, sizeof *info);
   open.pOpenAllocation = info;
   open.pPrivateDriverData = MALLOC(MAX2(open.PrivateDriverDataSize, 1));
   open.pTotalBuffer = MALLOC(MAX2(open.TotalBufferSize, 1));
   if (!info || !open.pPrivateDriverData || !open.pTotalBuffer)
      goto out;

   status = wws->device->openAllocation(wws->device, &open);
   if (!NT_SUCCESS(status))
      goto out;
   if (info[0].PrivateDriverDataSize < sizeof *exchange ||
       !info[0].pPrivateDriverData)
      goto out;
   exchange = (VIOGPU_CREATE_ALLOCATION_EXCHANGE *)info[0].pPrivateDriverData;

   wdt = CALLOC_STRUCT(wddm_sw_displaytarget);
   if (!wdt)
      goto out;
   wdt->format =
      exchange->ResourceOptions.format == WDDM_SW_FORMAT_B8G8R8A8
         ? PIPE_FORMAT_B8G8R8A8_UNORM
         : PIPE_FORMAT_B8G8R8X8_UNORM;
   wdt->width = exchange->ResourceOptions.width;
   wdt->height = exchange->ResourceOptions.height;
   wdt->stride = wdt->width * 4;
   wdt->hAllocation = info[0].hAllocation;
   wdt->opened = true;
   *stride = wdt->stride;

out:
   FREE(info);
   FREE(open.pPrivateDriverData);
   FREE(open.pTotalBuffer);
   return (struct sw_displaytarget *)wdt;
}

/* What a shared resource opened from `whandle` is: the Direct3D frontend opens
 * one without a template (the kernel driver's private data is the only
 * description there is), and a software rasteriser needs one. */
bool
wddm_sw_winsys_describe(struct sw_winsys *ws, struct winsys_handle *whandle,
                        struct pipe_resource *templat)
{
   struct wddm_sw_displaytarget *wdt;
   unsigned stride;

   wdt = wddm_sw_displaytarget(
      wddm_sw_displaytarget_from_handle(ws, NULL, whandle, &stride));
   if (!wdt)
      return false;

   memset(templat, 0, sizeof *templat);
   templat->target = PIPE_TEXTURE_2D;
   templat->format = wdt->format;
   templat->width0 = wdt->width;
   templat->height0 = wdt->height;
   templat->depth0 = 1;
   templat->array_size = 1;
   templat->last_level = 0;
   templat->nr_samples = 0;
   templat->usage = PIPE_USAGE_DEFAULT;
   templat->bind = PIPE_BIND_RENDER_TARGET | PIPE_BIND_SAMPLER_VIEW |
                   PIPE_BIND_DISPLAY_TARGET | PIPE_BIND_SHARED;
   /* Opened, so nothing to destroy but the description. */
   FREE(wdt);
   return true;
}

static bool
wddm_sw_displaytarget_get_handle(struct sw_winsys *ws,
                                 struct sw_displaytarget *dt,
                                 struct winsys_handle *whandle)
{
   struct wddm_sw_displaytarget *wdt = wddm_sw_displaytarget(dt);

   if (whandle->type == WINSYS_HANDLE_TYPE_D3DKMT_ALLOC) {
      whandle->handle = (HANDLE)(uintptr_t)wdt->hAllocation;
      whandle->stride = wdt->stride;
      return true;
   }
   return false;
}

static void *
wddm_sw_displaytarget_map(struct sw_winsys *ws, struct sw_displaytarget *dt,
                          unsigned flags)
{
   struct wddm_sw_winsys *wws = wddm_sw_winsys(ws);
   struct wddm_sw_displaytarget *wdt = wddm_sw_displaytarget(dt);

   /* Another process's allocation, for a client that copies: Windows will
    * not lock one the caller only opened (0x80070057 whatever the flags), so
    * the kernel driver copies its pixels out (escape 0x902) into a buffer
    * kept for the purpose. Reading only; a writer is the creator. */
   if (wdt->opened && wws->lock_per_map) {
      struct {
         unsigned short type;
         unsigned short length;
         uint32_t handle;
         uint64_t buffer;
         uint64_t size;
         uint64_t seen;
         char rest[208];
      } escape;
      size_t size = (size_t)wdt->stride * wdt->height;
      NTSTATUS status;

      if (flags & PIPE_MAP_WRITE)
         return NULL;
      if (!wdt->copy)
         wdt->copy = MALLOC(size);
      if (!wdt->copy)
         return NULL;
      memset(&escape, 0, sizeof escape);
      escape.type = 0x902;
      escape.length = 20;
      escape.handle = (uint32_t)wdt->hAllocation;
      escape.buffer = (uint64_t)(uintptr_t)wdt->copy;
      escape.size = size;
      escape.seen = wdt->seen;
      status = wws->device->escape(wws->device, &escape, sizeof escape);
      if (!NT_SUCCESS(status)) {
         static int said;

         if (said++ < 8)
            mesa_loge("wddm_sw: reading allocation 0x%x (%ux%u) failed 0x%lx",
                      (unsigned)wdt->hAllocation, wdt->width, wdt->height,
                      (unsigned long)status);
         return NULL;
      }
      /* A count of 0 is "no count" (the pages read directly): always new. */
      wdt->changed = escape.seen == 0 || escape.seen != wdt->seen;
      wdt->seen = escape.seen;
      return wdt->copy;
   }

   /* Locked once and kept: the pages are the allocation's own, and the
    * kernel driver reads them when it presents. */
   if (!wdt->ptr) {
      D3DDDICB_LOCKFLAGS lock_flags;
      NTSTATUS status;

      memset(&lock_flags, 0, sizeof lock_flags);
      /* A reader says so: another process's allocation is locked for
       * reading or not at all. */
      lock_flags.ReadOnly = !(flags & PIPE_MAP_WRITE);
      status = wws->device->lockAllocation(wws->device, wdt->hAllocation,
                                           lock_flags, &wdt->ptr);
      if (!NT_SUCCESS(status)) {
         static int said;

         if (said++ < 8)
            mesa_loge("wddm_sw: lockAllocation 0x%x failed 0x%lx (%s allocation, %ux%u)",
                      (unsigned)wdt->hAllocation, (unsigned long)status,
                      wdt->opened ? "an opened" : "our own", wdt->width, wdt->height);
         wdt->ptr = NULL;
      }
   }
   return wdt->ptr;
}

static void
wddm_sw_displaytarget_unmap(struct sw_winsys *ws, struct sw_displaytarget *dt)
{
   struct wddm_sw_winsys *wws = wddm_sw_winsys(ws);
   struct wddm_sw_displaytarget *wdt = wddm_sw_displaytarget(dt);

   if (wws->lock_per_map && wdt->ptr && wws->device->unlockAllocation) {
      wws->device->unlockAllocation(wws->device, wdt->hAllocation);
      wdt->ptr = NULL;
   }
}

/* Whether the last map of an opened display target read pixels it had not
 * read before; a client that uploads what it maps skips the upload if not. */
bool
wddm_sw_winsys_changed(struct sw_winsys *ws, struct sw_displaytarget *dt)
{
   struct wddm_sw_displaytarget *wdt = wddm_sw_displaytarget(dt);

   (void)ws;
   return !wdt->opened || wdt->changed;
}

/* A frame for whoever opened this display target: `size` bytes of pixels,
 * rows packed, handed to the kernel driver (escape 0x903), which keeps them
 * for the reader's 0x902. */
bool
wddm_sw_winsys_write(struct sw_winsys *ws, struct sw_displaytarget *dt,
                     const void *pixels, size_t size)
{
   struct wddm_sw_winsys *wws = wddm_sw_winsys(ws);
   struct wddm_sw_displaytarget *wdt = wddm_sw_displaytarget(dt);
   struct {
      unsigned short type;
      unsigned short length;
      uint32_t handle;
      uint64_t buffer;
      uint64_t size;
      char rest[216];
   } escape;
   NTSTATUS status;

   memset(&escape, 0, sizeof escape);
   escape.type = 0x903;
   escape.length = 20;
   escape.handle = (uint32_t)wdt->hAllocation;
   escape.buffer = (uint64_t)(uintptr_t)pixels;
   escape.size = size;
   status = wws->device->escape(wws->device, &escape, sizeof escape);
   if (!NT_SUCCESS(status)) {
      static int said;

      if (said++ < 8)
         mesa_loge("wddm_sw: writing allocation 0x%x (%ux%u) failed 0x%lx",
                   (unsigned)wdt->hAllocation, wdt->width, wdt->height,
                   (unsigned long)status);
      return false;
   }
   return true;
}

/* A client that only copies into and out of its display targets (a GPU
 * driver keeping WDDM allocations in step) holds a lock for the copy alone:
 * an allocation shared between processes is locked by one at a time, and one
 * kept locked by its writer is one its reader never sees. A rasteriser that
 * draws into the allocation itself keeps the default, a lock for good. */
void
wddm_sw_winsys_lock_per_map(struct sw_winsys *ws)
{
   wddm_sw_winsys(ws)->lock_per_map = true;
}

static void
wddm_sw_displaytarget_display(struct sw_winsys *ws, struct sw_displaytarget *dt,
                              void *context_private, unsigned nboxes,
                              struct pipe_box *box)
{
   struct wddm_sw_winsys *wws = wddm_sw_winsys(ws);
   struct wddm_sw_displaytarget *wdt = wddm_sw_displaytarget(dt);

   if (!wws->ctx)
      return;
   wws->device->present(wws->ctx, wdt->hAllocation, context_private,
                        nboxes ? box : NULL);
}

static void
wddm_sw_destroy(struct sw_winsys *ws)
{
   struct wddm_sw_winsys *wws = wddm_sw_winsys(ws);

   if (wws->ctx)
      wws->ctx->destroy(wws->ctx);
   FREE(wws);
}

struct sw_winsys *
wddm_sw_winsys_create(struct gdikmt_device *device)
{
   struct wddm_sw_winsys *wws = CALLOC_STRUCT(wddm_sw_winsys);

   if (!wws)
      return NULL;

   wws->device = device;
   if (!NT_SUCCESS(device->createContext(device, &wws->ctx)))
      wws->ctx = NULL;

   wws->base.destroy = wddm_sw_destroy;
   wws->base.is_displaytarget_format_supported =
      wddm_sw_is_displaytarget_format_supported;
   wws->base.displaytarget_create = wddm_sw_displaytarget_create;
   wws->base.displaytarget_from_handle = wddm_sw_displaytarget_from_handle;
   wws->base.displaytarget_get_handle = wddm_sw_displaytarget_get_handle;
   wws->base.displaytarget_map = wddm_sw_displaytarget_map;
   wws->base.displaytarget_unmap = wddm_sw_displaytarget_unmap;
   wws->base.displaytarget_display = wddm_sw_displaytarget_display;
   wws->base.displaytarget_destroy = wddm_sw_displaytarget_destroy;

   return &wws->base;
}
