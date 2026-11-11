/*
 * Copyright 2026 Entangled Desktop contributors
 * SPDX-License-Identifier: MIT
 *
 * The screen behind entangled_d3d10.dll, the Direct3D 10 user-mode driver of
 * entangled-wddm.sys (ADR-0010). Replaces Mesa's targets/d3d10umd/d3d10_gdi.c,
 * which presents through GDI.
 *
 * Two rasterisers:
 *
 *  - Zink, when a Vulkan device can be opened (Mesa's Venus through
 *    entangled-gpu.sys): Direct3D draws on the host GPU. What Windows has to
 *    see outside this process - a back buffer it presents, a surface shared
 *    with another process - is a WDDM allocation of the kernel driver, and
 *    this file keeps the two in step by copying: a shared texture is read
 *    back into its allocation when it was drawn to and the context flushes or
 *    presents, and uploaded from it when a texture opened from another
 *    process is about to be sampled. Two copies a frame through guest memory,
 *    the price of a phase that has no shared GPU memory yet (ADR-0010, W1 and
 *    W2 remove them).
 *  - softpipe otherwise, whose display targets are the allocations
 *    themselves (wddm_sw_winsys); nothing to copy, and nothing fast.
 *    ENTANGLED_D3D10_DRIVER=softpipe forces it.
 */

#include <stdarg.h>
#include <stdbool.h>
#include <stdio.h>
#include <string.h>
#include <windows.h>

#include "util/hash_table.h"
#include "util/simple_mtx.h"
#include "util/u_debug.h"
#include "util/u_inlines.h"
#include "util/u_memory.h"
#include "util/os_time.h"
#include "target-helpers/inline_debug_helper.h"
#include "tgsi/tgsi_dump.h"
#include "softpipe/sp_public.h"
#ifdef GALLIUM_ZINK
#include "zink/zink_public.h"
#endif

#include "frontend/sw_winsys.h"
#include "frontend/winsys_handle.h"
#include "gdikmt/gdikmt.h"
#include "pipe/p_context.h"
#include "pipe/p_screen.h"
#include "pipe/p_state.h"
#include "wddm/wddm_sw_winsys.h"

struct pipe_screen *
d3d10_create_screen(struct gdikmt_device *device);

/* ---------------------------------------------------------------------- */
/* Saying something from inside DWM                                        */
/* ---------------------------------------------------------------------- */

/* This driver runs in processes nothing can be read out of - DWM above all.
 * A diagnostic build of the kernel driver takes a line of text through an
 * escape and writes it to COM1, which the VM's log carries; any other build
 * refuses the escape and the line is lost, which is the intent. */
#define D3D10_WDDM_ESCAPE_PRINT 0x900
/* "The host shows this screen's presents itself": see the kernel driver. */
#define D3D10_WDDM_ESCAPE_OVERLAID 0x901

static struct gdikmt_device *d3d10_wddm_log_device;

static void
d3d10_wddm_log(const char *format, ...)
{
   struct gdikmt_device *device = d3d10_wddm_log_device;
   struct {
      unsigned short type;
      unsigned short length;
      char text[236];
   } escape;
   va_list args;

   if (!device)
      return;
   memset(&escape, 0, sizeof escape);
   escape.type = D3D10_WDDM_ESCAPE_PRINT;
   va_start(args, format);
   vsnprintf(escape.text, sizeof escape.text, format, args);
   va_end(args);
   escape.length = (unsigned short)strlen(escape.text);
   device->escape(device, &escape, sizeof escape);
}

/* An access violation or the like in this process: say where, as module and
 * offset for every frame, then let it take its course. The frames are what
 * the linker map of this DLL (build-d3d10.cmd keeps one) turns into names. */
static LONG CALLBACK
d3d10_wddm_exception(EXCEPTION_POINTERS *info)
{
   static volatile LONG reported;
   CONTEXT context;
   DWORD code = info->ExceptionRecord->ExceptionCode;

   /* A debug string (Mesa's log goes out through OutputDebugString, which
    * raises this when no debugger listens): carry the text. */
   if (code == 0x40010006u && info->ExceptionRecord->NumberParameters >= 2 &&
       info->ExceptionRecord->ExceptionInformation[1]) {
      d3d10_wddm_log("%.200s",
                     (const char *)info->ExceptionRecord->ExceptionInformation[1]);
      return EXCEPTION_CONTINUE_SEARCH;
   }
   /* Errors only (not C++ throws or breakpoints), and once: a handler that
    * faults must not report itself for ever. */
   if (code < 0xC0000000u || code == 0xE06D7363u)
      return EXCEPTION_CONTINUE_SEARCH;
   if (InterlockedExchange(&reported, 1) != 0)
      return EXCEPTION_CONTINUE_SEARCH;

   d3d10_wddm_log("exception 0x%08lx at %p, access %p", (unsigned long)code,
                  info->ExceptionRecord->ExceptionAddress,
                  info->ExceptionRecord->NumberParameters > 1
                     ? (void *)info->ExceptionRecord->ExceptionInformation[1]
                     : NULL);

   context = *info->ContextRecord;
   for (unsigned frame = 0; frame < 24 && context.Rip; frame++) {
      DWORD64 image_base = 0;
      PRUNTIME_FUNCTION function;
      HMODULE module = NULL;
      char name[MAX_PATH] = "?";
      const char *base_name = name;

      if (GetModuleHandleExA(GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS |
                                GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
                             (LPCSTR)context.Rip, &module) &&
          module) {
         GetModuleFileNameA(module, name, sizeof name);
         for (const char *p = name; *p; p++) {
            if (*p == '\\' || *p == '/')
               base_name = p + 1;
         }
      }
      d3d10_wddm_log("  #%u %s+0x%llx", frame, base_name,
                     (unsigned long long)(context.Rip - (DWORD64)(uintptr_t)module));

      function = RtlLookupFunctionEntry(context.Rip, &image_base, NULL);
      if (function) {
         PVOID handler_data;
         DWORD64 establisher;

         RtlVirtualUnwind(UNW_FLAG_NHANDLER, image_base, context.Rip, function,
                          &context, &handler_data, &establisher, NULL);
      } else {
         /* A leaf, or not code at all (a jump through a bad pointer): the
          * return address is on top of the stack. */
         context.Rip = *(DWORD64 *)context.Rsp;
         context.Rsp += 8;
      }
   }
   /* When the fault is a jump or a return to somewhere that is not code, the
    * unwind above has nothing to go on. The stack still holds the return
    * addresses of the callers: every word on it that points into a module. */
   {
      NT_TIB *tib = (NT_TIB *)NtCurrentTeb();
      DWORD64 *word = (DWORD64 *)info->ContextRecord->Rsp;
      DWORD64 *end = (DWORD64 *)tib->StackBase;
      unsigned shown = 0;

      /* A return to a bad address leaves that address just below the stack
       * pointer; a call to one leaves the caller on top. */
      d3d10_wddm_log("  rsp[-1] %llx rsp[0] %llx rsp[1] %llx rax %llx rcx %llx",
                     (unsigned long long)word[-1], (unsigned long long)word[0],
                     (unsigned long long)word[1],
                     (unsigned long long)info->ContextRecord->Rax,
                     (unsigned long long)info->ContextRecord->Rcx);
      for (unsigned i = 0; word + i < end && i < 600 && shown < 40; i++) {
         HMODULE module = NULL;
         char name[MAX_PATH] = "?";
         const char *base_name = name;

         if (word[i] < 0x10000 ||
             !GetModuleHandleExA(GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS |
                                    GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
                                 (LPCSTR)word[i], &module) ||
             !module)
            continue;
         GetModuleFileNameA(module, name, sizeof name);
         for (const char *p = name; *p; p++) {
            if (*p == '\\' || *p == '/')
               base_name = p + 1;
         }
         /* Only a word that follows a call instruction is a return address;
          * the rest are pointers to data and leftovers. An indirect call is
          * the one worth reading: it is where a bad pointer was followed. */
         {
            const unsigned char *code_at = (const unsigned char *)word[i];
            const char *kind = NULL;

            if (!IsBadReadPtr(code_at - 7, 7)) {
               if (code_at[-5] == 0xE8)
                  kind = "call";
               else if ((code_at[-2] == 0xFF && (code_at[-1] & 0x38) == 0x10) ||
                        (code_at[-3] == 0xFF && (code_at[-2] & 0x38) == 0x10) ||
                        (code_at[-6] == 0xFF && (code_at[-5] & 0x38) == 0x10) ||
                        (code_at[-7] == 0xFF && (code_at[-6] & 0x38) == 0x10))
                  kind = "INDIRECT";
            }
            if (!kind)
               continue;
            d3d10_wddm_log("  stack[%u] %s+0x%llx %s", i, base_name,
                           (unsigned long long)(word[i] - (DWORD64)(uintptr_t)module),
                           kind);
         }
         shown++;
      }
   }
   InterlockedExchange(&reported, 0);
   return EXCEPTION_CONTINUE_SEARCH;
}

static PVOID d3d10_wddm_exception_handle;

/* The handler leaves with the DLL. Left registered, it is a pointer into
 * whatever is mapped there next, and the next exception anyone raises in the
 * process - a debug string is enough - jumps to it. */
static void
d3d10_wddm_exception_remove(void)
{
   if (d3d10_wddm_exception_handle) {
      RemoveVectoredExceptionHandler(d3d10_wddm_exception_handle);
      d3d10_wddm_exception_handle = NULL;
   }
}

/* ---------------------------------------------------------------------- */
/* softpipe: a template for a shared resource opened without one           */
/* ---------------------------------------------------------------------- */

struct d3d10_wddm_screen {
   struct pipe_screen *screen;
   struct sw_winsys *winsys;
   struct gdikmt_device *device;
   bool gpu;
   /* The host shows this screen's presents (ADR-0010 phase W2); and the
    * device they were shown from, for taking the plane down. */
   bool overlaid;
   void *overlay_device;

   /* The rasteriser's own entry points. */
   void (*destroy)(struct pipe_screen *);
   struct pipe_context *(*context_create)(struct pipe_screen *, void *,
                                          unsigned);
   struct pipe_resource *(*resource_create)(struct pipe_screen *,
                                            const struct pipe_resource *);
   struct pipe_resource *(*resource_from_handle)(struct pipe_screen *,
                                                 const struct pipe_resource *,
                                                 struct winsys_handle *,
                                                 unsigned);
   bool (*resource_get_handle)(struct pipe_screen *, struct pipe_context *,
                               struct pipe_resource *, struct winsys_handle *,
                               unsigned);
   void (*resource_destroy)(struct pipe_screen *, struct pipe_resource *);
   void (*flush_frontbuffer)(struct pipe_screen *, struct pipe_context *,
                             struct pipe_resource *, unsigned, unsigned,
                             void *, unsigned, struct pipe_box *);

   /* GPU only: the textures that have an allocation, by pipe_resource. */
   simple_mtx_t lock;
   struct hash_table *shared;
};

/* A texture on the GPU and the allocation Windows knows it by. */
struct d3d10_wddm_shared {
   struct sw_displaytarget *dt;
   unsigned width;
   unsigned height;
   unsigned stride;
   /* Opened from another process: its pixels arrive in the allocation. */
   bool opened;
   /* Drawn to here since the allocation was last written. */
   bool dirty;
   /* A buffer a screen presents (not one shared between processes): the host
    * shows its frames itself (d3d10_wddm_overlay), so nothing is read back. */
   bool presentable;
   /* The frame as the host reads it: a buffer in memory it can see, filled
    * by a copy on the GPU. */
   struct pipe_resource *frame;
};

struct d3d10_wddm_context {
   struct pipe_context *pipe;
   struct d3d10_wddm_screen *ws;

   void (*destroy)(struct pipe_context *);
   void (*flush)(struct pipe_context *, struct pipe_fence_handle **, unsigned);
   void (*set_framebuffer_state)(struct pipe_context *,
                                 const struct pipe_framebuffer_state *);
   void (*set_sampler_views)(struct pipe_context *, mesa_shader_stage,
                             unsigned, unsigned, unsigned,
                             struct pipe_sampler_view **);
   void (*blit)(struct pipe_context *, const struct pipe_blit_info *);
   void (*resource_copy_region)(struct pipe_context *, struct pipe_resource *,
                                unsigned, unsigned, unsigned, unsigned,
                                struct pipe_resource *, unsigned,
                                const struct pipe_box *);
   void *(*create_shader[3])(struct pipe_context *,
                             const struct pipe_shader_state *);
   /* The shader bound per stage (vertex, fragment, geometry), so that one
    * deleted while bound - which Direct3D allows - is unbound first: zink
    * reads the outgoing shader when the next one is bound. */
   void *bound_shader[3];
   void (*bind_shader[3])(struct pipe_context *, void *);
   void (*delete_shader[3])(struct pipe_context *, void *);
   /* In a flush of ours: the copies it makes flush again. */
   bool flushing;
   void *(*buffer_map)(struct pipe_context *, struct pipe_resource *,
                       unsigned, unsigned, const struct pipe_box *,
                       struct pipe_transfer **);
   void *(*texture_map)(struct pipe_context *, struct pipe_resource *,
                        unsigned, unsigned, const struct pipe_box *,
                        struct pipe_transfer **);
};

#define D3D10_WDDM_MAX_SCREENS 16
#define D3D10_WDDM_MAX_CONTEXTS 64

static struct d3d10_wddm_screen d3d10_wddm_screens[D3D10_WDDM_MAX_SCREENS];
static struct d3d10_wddm_context d3d10_wddm_contexts[D3D10_WDDM_MAX_CONTEXTS];
static simple_mtx_t d3d10_wddm_tables = SIMPLE_MTX_INITIALIZER;

static struct d3d10_wddm_screen *
d3d10_wddm_screen(struct pipe_screen *screen)
{
   for (unsigned i = 0; i < D3D10_WDDM_MAX_SCREENS; i++) {
      if (d3d10_wddm_screens[i].screen == screen)
         return &d3d10_wddm_screens[i];
   }
   return NULL;
}

static struct d3d10_wddm_context *
d3d10_wddm_context(struct pipe_context *pipe)
{
   for (unsigned i = 0; i < D3D10_WDDM_MAX_CONTEXTS; i++) {
      if (d3d10_wddm_contexts[i].pipe == pipe)
         return &d3d10_wddm_contexts[i];
   }
   return NULL;
}

static struct d3d10_wddm_shared *
d3d10_wddm_lookup(struct d3d10_wddm_screen *ws, struct pipe_resource *res)
{
   struct hash_entry *entry;
   struct d3d10_wddm_shared *shared = NULL;

   if (!ws->shared || !res)
      return NULL;
   simple_mtx_lock(&ws->lock);
   entry = _mesa_hash_table_search(ws->shared, res);
   if (entry)
      shared = entry->data;
   simple_mtx_unlock(&ws->lock);
   return shared;
}

/* ---------------------------------------------------------------------- */
/* The copies                                                              */
/* ---------------------------------------------------------------------- */

/* Bring-up: how much copying a second costs, said every few seconds. */
static volatile LONG d3d10_wddm_readbacks, d3d10_wddm_uploads,
   d3d10_wddm_flushes, d3d10_wddm_overlays;
static volatile LONG64 d3d10_wddm_copied;

static void
d3d10_wddm_stats(void)
{
   static volatile LONG last;
   LONG now = (LONG)GetTickCount();
   LONG before = last;

   if ((ULONG)(now - before) < 2000 ||
       InterlockedCompareExchange(&last, now, before) != before)
      return;
   d3d10_wddm_log("copies in %lu ms: %ld overlaid, %ld flushes, %ld readbacks, %ld uploads, %lld MiB",
                  (unsigned long)(now - before),
                  InterlockedExchange(&d3d10_wddm_overlays, 0),
                  InterlockedExchange(&d3d10_wddm_flushes, 0),
                  InterlockedExchange(&d3d10_wddm_readbacks, 0),
                  InterlockedExchange(&d3d10_wddm_uploads, 0),
                  (long long)(InterlockedExchange64(&d3d10_wddm_copied, 0) >> 20));
}

/* The GPU texture into its allocation: what another process, or the kernel
 * driver presenting it, will read. */
static void
d3d10_wddm_readback(struct d3d10_wddm_screen *ws, struct pipe_context *pipe,
                    struct pipe_resource *res,
                    struct d3d10_wddm_shared *shared)
{
   struct pipe_transfer *transfer = NULL;
   struct pipe_box box;
   uint8_t *dst;
   const uint8_t *src;

   /* Before the map, not after: a driver that has to finish rendering to
    * read (zink) flushes from inside the map, and that flush comes back
    * through d3d10_wddm_flush, which must find nothing left to read. */
   shared->dirty = false;
   dst = ws->winsys->displaytarget_map(ws->winsys, shared->dt, PIPE_MAP_WRITE);
   if (!dst)
      return;
   u_box_2d(0, 0, shared->width, shared->height, &box);
   src = pipe->texture_map(pipe, res, 0, PIPE_MAP_READ, &box, &transfer);
   if (!src)
      return;
   for (unsigned y = 0; y < shared->height; y++) {
      memcpy(dst + (size_t)y * shared->stride,
             src + (size_t)y * transfer->stride, (size_t)shared->width * 4);
   }
   pipe->texture_unmap(pipe, transfer);
   InterlockedIncrement(&d3d10_wddm_readbacks);
   InterlockedAdd64(&d3d10_wddm_copied, (LONG64)shared->width * shared->height * 4);
}

/* The allocation into its GPU texture: what another process drew. */
static void
d3d10_wddm_upload(struct d3d10_wddm_screen *ws, struct pipe_context *pipe,
                  struct pipe_resource *res, struct d3d10_wddm_shared *shared)
{
   struct pipe_box box;
   const void *src;

   src = ws->winsys->displaytarget_map(ws->winsys, shared->dt, PIPE_MAP_READ);
   if (!src)
      return;
   u_box_2d(0, 0, shared->width, shared->height, &box);
   pipe->texture_subdata(pipe, res, 0, PIPE_MAP_WRITE, &box, src,
                         shared->stride, 0);
   InterlockedIncrement(&d3d10_wddm_uploads);
   InterlockedAdd64(&d3d10_wddm_copied, (LONG64)shared->width * shared->height * 4);
}

/* ---------------------------------------------------------------------- */
/* Screen                                                                  */
/* ---------------------------------------------------------------------- */

static bool
d3d10_wddm_wants_allocation(const struct pipe_resource *templat)
{
   if (!(templat->bind & (PIPE_BIND_DISPLAY_TARGET | PIPE_BIND_SHARED |
                          PIPE_BIND_SCANOUT)))
      return false;
   if (templat->target != PIPE_TEXTURE_2D || templat->last_level != 0 ||
       templat->array_size > 1 || templat->nr_samples > 1)
      return false;
   switch (templat->format) {
   case PIPE_FORMAT_B8G8R8A8_UNORM:
   case PIPE_FORMAT_B8G8R8A8_SRGB:
   case PIPE_FORMAT_B8G8R8X8_UNORM:
   case PIPE_FORMAT_B8G8R8X8_SRGB:
      return true;
   default:
      return false;
   }
}

static struct pipe_resource *
d3d10_wddm_attach(struct d3d10_wddm_screen *ws,
                  const struct pipe_resource *templat,
                  struct sw_displaytarget *dt, unsigned stride, bool opened)
{
   struct pipe_resource gpu = *templat;
   struct pipe_resource *res;
   struct d3d10_wddm_shared *shared;

   /* The GPU texture is an ordinary one: no window system of Vulkan's is
    * involved, the allocation is how it leaves this process. */
   gpu.bind &= ~(PIPE_BIND_DISPLAY_TARGET | PIPE_BIND_SHARED | PIPE_BIND_SCANOUT);
   gpu.bind |= PIPE_BIND_RENDER_TARGET | PIPE_BIND_SAMPLER_VIEW;
   res = ws->resource_create(ws->screen, &gpu);
   shared = res ? CALLOC_STRUCT(d3d10_wddm_shared) : NULL;
   if (!shared) {
      if (res)
         ws->resource_destroy(ws->screen, res);
      ws->winsys->displaytarget_destroy(ws->winsys, dt);
      return NULL;
   }
   shared->dt = dt;
   shared->width = templat->width0;
   shared->height = templat->height0;
   shared->stride = stride;
   shared->opened = opened;
   shared->presentable = !opened &&
                         (templat->bind & PIPE_BIND_DISPLAY_TARGET) &&
                         !(templat->bind & PIPE_BIND_SHARED);

   simple_mtx_lock(&ws->lock);
   _mesa_hash_table_insert(ws->shared, res, shared);
   simple_mtx_unlock(&ws->lock);
   return res;
}

static struct pipe_resource *
d3d10_wddm_resource_create(struct pipe_screen *screen,
                           const struct pipe_resource *templat)
{
   struct d3d10_wddm_screen *ws = d3d10_wddm_screen(screen);
   struct sw_displaytarget *dt;
   unsigned stride = 0;

   {
      static volatile LONG said;
      uint64_t bytes = templat->target == PIPE_BUFFER
                          ? templat->width0
                          : (uint64_t)templat->width0 * templat->height0 * 4;

      if (bytes >= (4u << 20) && InterlockedIncrement(&said) <= 60)
         d3d10_wddm_log("big resource: target %u format %u %ux%u bind 0x%x usage %u flags 0x%x",
                        templat->target, templat->format, templat->width0,
                        templat->height0, templat->bind, templat->usage,
                        templat->flags);
   }
   if (!ws->gpu || !d3d10_wddm_wants_allocation(templat))
      return ws->resource_create(screen, templat);

   dt = ws->winsys->displaytarget_create(ws->winsys, templat->bind,
                                         templat->format, templat->width0,
                                         templat->height0, 64, NULL, &stride);
   if (!dt)
      return NULL;
   return d3d10_wddm_attach(ws, templat, dt, stride, false);
}

/* The frontend opens a shared resource with no template: the description is
 * the allocation's. */
static struct pipe_resource *
d3d10_wddm_resource_from_handle(struct pipe_screen *screen,
                                const struct pipe_resource *templat,
                                struct winsys_handle *whandle, unsigned usage)
{
   struct d3d10_wddm_screen *ws = d3d10_wddm_screen(screen);
   struct pipe_resource described;
   struct sw_displaytarget *dt;
   unsigned stride = 0;

   if (!templat) {
      if (!wddm_sw_winsys_describe(ws->winsys, whandle, &described))
         return NULL;
      described.screen = screen;
      templat = &described;
   }
   if (!ws->gpu)
      return ws->resource_from_handle(screen, templat, whandle, usage);

   dt = ws->winsys->displaytarget_from_handle(ws->winsys, templat, whandle,
                                              &stride);
   if (!dt)
      return NULL;
   return d3d10_wddm_attach(ws, templat, dt, stride, true);
}

static bool
d3d10_wddm_resource_get_handle(struct pipe_screen *screen,
                               struct pipe_context *pipe,
                               struct pipe_resource *res,
                               struct winsys_handle *whandle, unsigned usage)
{
   struct d3d10_wddm_screen *ws = d3d10_wddm_screen(screen);
   struct d3d10_wddm_shared *shared = d3d10_wddm_lookup(ws, res);

   if (shared)
      return ws->winsys->displaytarget_get_handle(ws->winsys, shared->dt,
                                                  whandle);
   return ws->resource_get_handle
             ? ws->resource_get_handle(screen, pipe, res, whandle, usage)
             : false;
}

static void
d3d10_wddm_resource_destroy(struct pipe_screen *screen,
                            struct pipe_resource *res)
{
   struct d3d10_wddm_screen *ws = d3d10_wddm_screen(screen);

   if (ws->shared) {
      struct hash_entry *entry;
      struct d3d10_wddm_shared *shared = NULL;

      simple_mtx_lock(&ws->lock);
      entry = _mesa_hash_table_search(ws->shared, res);
      if (entry) {
         shared = entry->data;
         _mesa_hash_table_remove(ws->shared, entry);
      }
      simple_mtx_unlock(&ws->lock);
      if (shared) {
         if (shared->frame)
            ws->resource_destroy(screen, shared->frame);
         ws->winsys->displaytarget_destroy(ws->winsys, shared->dt);
         FREE(shared);
      }
   }
   ws->resource_destroy(screen, res);
}

/* ADR-0010 phase W2: a present with no copy through this CPU. The frame is
 * copied, on the GPU, into a buffer whose memory the host can see, and the
 * Venus driver has the host show that memory over the whole screen - the
 * overlay plane a window's swapchain is shown on (ADR-0009 phase E), here for
 * the desktop itself. The primary underneath keeps whatever it last held. */
typedef bool (*d3d10_wddm_present_overlay)(void *device, uint64_t memory,
                                           uint32_t width, uint32_t height,
                                           uint32_t stride, int32_t x,
                                           int32_t y);

static d3d10_wddm_present_overlay
d3d10_wddm_overlay_entry(void)
{
   static d3d10_wddm_present_overlay entry;
   static bool looked;

   if (!looked) {
      HMODULE icd = GetModuleHandleA("vulkan_virtio.dll");

      if (icd)
         entry = (d3d10_wddm_present_overlay)(void *)
            GetProcAddress(icd, "entangled_present_overlay");
      d3d10_wddm_log("overlay entry %p", (void *)entry);
      looked = true;
   }
   return strcmp(debug_get_option("ENTANGLED_D3D10_OVERLAY", "on"), "off") != 0
             ? entry : NULL;
}

static bool
d3d10_wddm_overlay(struct d3d10_wddm_screen *ws, struct pipe_context *pipe,
                   struct pipe_resource *res, struct d3d10_wddm_shared *shared)
{
#ifdef GALLIUM_ZINK
   d3d10_wddm_present_overlay present;
   struct d3d10_wddm_context *wc = pipe ? d3d10_wddm_context(pipe) : NULL;
   struct pipe_fence_handle *fence = NULL;
   void *device = NULL;
   uint64_t memory = 0;

   if (!ws->gpu || !wc || !shared->presentable)
      return false;
   present = d3d10_wddm_overlay_entry();
   if (!present)
      return false;
   if (!shared->frame) {
      struct pipe_resource templat;
      struct pipe_transfer *transfer = NULL;
      struct pipe_box box;

      memset(&templat, 0, sizeof templat);
      templat.target = PIPE_BUFFER;
      templat.format = PIPE_FORMAT_R8_UNORM;
      templat.width0 = shared->width * shared->height * 4;
      templat.height0 = 1;
      templat.depth0 = 1;
      templat.array_size = 1;
      templat.usage = PIPE_USAGE_STAGING;
      shared->frame = ws->resource_create(ws->screen, &templat);
      if (!shared->frame)
         return false;
      /* Mapped once: that is when the Venus driver makes the memory a
       * resource the host can be pointed at. */
      u_box_1d(0, templat.width0, &box);
      if (wc->buffer_map(pipe, shared->frame, 0, PIPE_MAP_READ, &box, &transfer))
         pipe->buffer_unmap(pipe, transfer);
   }
   if (!zink_entangled_frame(pipe, shared->frame, res, &device, &memory))
      return false;
   /* The host reads the buffer when it is told to: the copy must have run. */
   wc->flush(pipe, &fence, 0);
   if (fence) {
      ws->screen->fence_finish(ws->screen, NULL, fence, OS_TIMEOUT_INFINITE);
      ws->screen->fence_reference(ws->screen, &fence, NULL);
   }
   if (!present(device, memory, shared->width, shared->height,
                shared->width * 4, 0, 0))
      return false;
   ws->overlay_device = device;
   {
      /* Said once, for the life of this process: the plane is taken (see
       * wsi_win32_overlay_place in the Venus driver). Anyone in the session
       * may look. */
      static HANDLE taken;

      if (!taken) {
         SECURITY_DESCRIPTOR descriptor;
         SECURITY_ATTRIBUTES attributes = { sizeof attributes, &descriptor, FALSE };

         InitializeSecurityDescriptor(&descriptor, SECURITY_DESCRIPTOR_REVISION);
         SetSecurityDescriptorDacl(&descriptor, TRUE, NULL, FALSE);
         taken = CreateEventA(&attributes, TRUE, FALSE, "Local\\EntangledDesktopOverlay");
      }
   }
   InterlockedIncrement(&d3d10_wddm_overlays);
   return true;
#else
   return false;
#endif
}

static void
d3d10_wddm_flush_frontbuffer(struct pipe_screen *screen,
                             struct pipe_context *pipe,
                             struct pipe_resource *res, unsigned level,
                             unsigned layer, void *winsys_drawable_handle,
                             unsigned nboxes, struct pipe_box *sub_box)
{
   struct d3d10_wddm_screen *ws = d3d10_wddm_screen(screen);
   struct d3d10_wddm_shared *shared = d3d10_wddm_lookup(ws, res);

   if (!shared) {
      ws->flush_frontbuffer(screen, pipe, res, level, layer,
                            winsys_drawable_handle, nboxes, sub_box);
      return;
   }
   /* A present: the host shows the frame from the GPU's own copy of it, or
    * failing that the kernel driver reads the allocation. */
   if (d3d10_wddm_overlay(ws, pipe, res, shared)) {
      static volatile LONG leased, refreshed;
      LONG now = (LONG)GetTickCount();

      shared->dirty = false;
      ws->overlaid = true;
      /* The kernel driver leaves the primary alone while it is told the
       * plane covers it (a lease of two seconds, renewed here), and then
       * shows it once a second - for which the allocation gets its pixels
       * about as often. */
      if ((ULONG)(now - leased) >= 500) {
         struct {
            unsigned short type;
            unsigned short length;
            char text[236];
         } escape;

         leased = now;
         memset(&escape, 0, sizeof escape);
         escape.type = D3D10_WDDM_ESCAPE_OVERLAID;
         ws->device->escape(ws->device, &escape, sizeof escape);
      }
      if ((ULONG)(now - refreshed) >= 900) {
         refreshed = now;
         d3d10_wddm_readback(ws, pipe, res, shared);
      }
   } else {
      ws->overlaid = false;
      d3d10_wddm_readback(ws, pipe, res, shared);
   }
   ws->winsys->displaytarget_display(ws->winsys, shared->dt,
                                     winsys_drawable_handle, nboxes, sub_box);
}

/* ---------------------------------------------------------------------- */
/* Context: noticing draws into, and reads from, a shared texture          */
/* ---------------------------------------------------------------------- */

static void
d3d10_wddm_mark(struct d3d10_wddm_screen *ws, struct pipe_resource *res)
{
   struct d3d10_wddm_shared *shared = d3d10_wddm_lookup(ws, res);

   if (shared)
      shared->dirty = true;
}

static void
d3d10_wddm_set_framebuffer_state(struct pipe_context *pipe,
                                 const struct pipe_framebuffer_state *state)
{
   struct d3d10_wddm_context *wc = d3d10_wddm_context(pipe);

   if (state) {
      for (unsigned i = 0; i < state->nr_cbufs; i++)
         d3d10_wddm_mark(wc->ws, state->cbufs[i].texture);
   }
   wc->set_framebuffer_state(pipe, state);
}

static void
d3d10_wddm_blit(struct pipe_context *pipe, const struct pipe_blit_info *info)
{
   struct d3d10_wddm_context *wc = d3d10_wddm_context(pipe);

   d3d10_wddm_mark(wc->ws, info->dst.resource);
   wc->blit(pipe, info);
}

static void
d3d10_wddm_resource_copy_region(struct pipe_context *pipe,
                                struct pipe_resource *dst, unsigned dst_level,
                                unsigned dstx, unsigned dsty, unsigned dstz,
                                struct pipe_resource *src, unsigned src_level,
                                const struct pipe_box *src_box)
{
   struct d3d10_wddm_context *wc = d3d10_wddm_context(pipe);

   d3d10_wddm_mark(wc->ws, dst);
   wc->resource_copy_region(pipe, dst, dst_level, dstx, dsty, dstz, src,
                            src_level, src_box);
}

/* A texture opened from another process is as old as its last upload: fetch
 * what is in the allocation now, before it is sampled. */
static void
d3d10_wddm_set_sampler_views(struct pipe_context *pipe,
                             mesa_shader_stage shader, unsigned start_slot,
                             unsigned num_views,
                             unsigned unbind_num_trailing_slots,
                             struct pipe_sampler_view **views)
{
   struct d3d10_wddm_context *wc = d3d10_wddm_context(pipe);

   if (views) {
      for (unsigned i = 0; i < num_views; i++) {
         struct d3d10_wddm_shared *shared;

         if (!views[i])
            continue;
         shared = d3d10_wddm_lookup(wc->ws, views[i]->texture);
         if (shared && shared->opened && !shared->dirty)
            d3d10_wddm_upload(wc->ws, pipe, views[i]->texture, shared);
      }
   }
   wc->set_sampler_views(pipe, shader, start_slot, num_views,
                         unbind_num_trailing_slots, views);
}

/* A flush is where a process says "this is drawn": what it drew into shared
 * textures goes into their allocations. */
static void
d3d10_wddm_flush(struct pipe_context *pipe, struct pipe_fence_handle **fence,
                 unsigned flags)
{
   struct d3d10_wddm_context *wc = d3d10_wddm_context(pipe);
   struct d3d10_wddm_screen *ws = wc->ws;
   struct pipe_resource *dirty[16];
   struct d3d10_wddm_shared *shared[16];
   unsigned count = 0;

   wc->flush(pipe, fence, flags);
   /* A screen on its way out flushes the contexts it keeps for itself, after
    * the table of shared textures has gone. */
   if (wc->flushing || !ws->shared)
      return;
   wc->flushing = true;
   InterlockedIncrement(&d3d10_wddm_flushes);
   d3d10_wddm_stats();

   simple_mtx_lock(&ws->lock);
   hash_table_foreach(ws->shared, entry) {
      struct d3d10_wddm_shared *s = entry->data;

      /* What only a present shows is read (or not read at all) there. */
      if (s->presentable && ws->overlaid) {
         s->dirty = false;
         continue;
      }
      if (s->dirty && count < ARRAY_SIZE(dirty)) {
         dirty[count] = (struct pipe_resource *)entry->key;
         shared[count] = s;
         count++;
      }
   }
   simple_mtx_unlock(&ws->lock);
   for (unsigned i = 0; i < count; i++)
      d3d10_wddm_readback(ws, pipe, dirty[i], shared[i]);
   wc->flushing = false;
}

/* Bring-up: the TGSI of every shader the runtime hands over, before the
 * driver translates it - what a translator that stops on one has to be read
 * against. Set to 0 for a quiet driver. */
#ifndef D3D10_WDDM_DUMP_SHADERS
#define D3D10_WDDM_DUMP_SHADERS 0
#endif

static void *
d3d10_wddm_create_shader(struct pipe_context *pipe, unsigned stage,
                         const struct pipe_shader_state *state)
{
   struct d3d10_wddm_context *wc = d3d10_wddm_context(pipe);
   static volatile LONG dumped;

   if (D3D10_WDDM_DUMP_SHADERS && state->type == PIPE_SHADER_IR_TGSI &&
       state->tokens && InterlockedIncrement(&dumped) <= 40) {
      static char text[16384];
      static simple_mtx_t lock = SIMPLE_MTX_INITIALIZER;
      char *line;

      simple_mtx_lock(&lock);
      text[0] = 0;
      tgsi_dump_str(state->tokens, 0, text, sizeof text);
      d3d10_wddm_log("shader, stage %u", stage);
      for (line = text; *line;) {
         char *end = strchr(line, '\n');

         if (end)
            *end = 0;
         d3d10_wddm_log("| %s", line);
         if (!end)
            break;
         line = end + 1;
      }
      simple_mtx_unlock(&lock);
   }
   return wc->create_shader[stage](pipe, state);
}

static void *
d3d10_wddm_create_vs_state(struct pipe_context *pipe,
                           const struct pipe_shader_state *state)
{
   return d3d10_wddm_create_shader(pipe, 0, state);
}

static void *
d3d10_wddm_create_fs_state(struct pipe_context *pipe,
                           const struct pipe_shader_state *state)
{
   return d3d10_wddm_create_shader(pipe, 1, state);
}

static void *
d3d10_wddm_create_gs_state(struct pipe_context *pipe,
                           const struct pipe_shader_state *state)
{
   return d3d10_wddm_create_shader(pipe, 2, state);
}

static void *
d3d10_wddm_buffer_map(struct pipe_context *pipe, struct pipe_resource *res,
                      unsigned level, unsigned usage,
                      const struct pipe_box *box,
                      struct pipe_transfer **transfer)
{
   struct d3d10_wddm_context *wc = d3d10_wddm_context(pipe);
   static volatile LONG said;

   if (res->width0 >= (4u << 20) && InterlockedIncrement(&said) <= 40)
      d3d10_wddm_log("big buffer map: %u bytes, usage 0x%x, box %d+%d",
                     res->width0, usage, box->x, box->width);
   return wc->buffer_map(pipe, res, level, usage, box, transfer);
}

static void *
d3d10_wddm_texture_map(struct pipe_context *pipe, struct pipe_resource *res,
                       unsigned level, unsigned usage,
                       const struct pipe_box *box,
                       struct pipe_transfer **transfer)
{
   struct d3d10_wddm_context *wc = d3d10_wddm_context(pipe);
   static volatile LONG said;

   if ((uint64_t)box->width * box->height * 4 >= (4u << 20) &&
       InterlockedIncrement(&said) <= 40)
      d3d10_wddm_log("big texture map: %ux%u format %u, usage 0x%x, box %dx%d",
                     res->width0, res->height0, res->format, usage,
                     box->width, box->height);
   return wc->texture_map(pipe, res, level, usage, box, transfer);
}

static void
d3d10_wddm_bind_shader(struct pipe_context *pipe, unsigned stage, void *shader)
{
   struct d3d10_wddm_context *wc = d3d10_wddm_context(pipe);

   wc->bound_shader[stage] = shader;
   wc->bind_shader[stage](pipe, shader);
}

static void
d3d10_wddm_delete_shader(struct pipe_context *pipe, unsigned stage, void *shader)
{
   struct d3d10_wddm_context *wc = d3d10_wddm_context(pipe);

   if (shader && wc->bound_shader[stage] == shader)
      d3d10_wddm_bind_shader(pipe, stage, NULL);
   wc->delete_shader[stage](pipe, shader);
}

static void
d3d10_wddm_bind_vs_state(struct pipe_context *pipe, void *shader)
{
   d3d10_wddm_bind_shader(pipe, 0, shader);
}

static void
d3d10_wddm_bind_fs_state(struct pipe_context *pipe, void *shader)
{
   d3d10_wddm_bind_shader(pipe, 1, shader);
}

static void
d3d10_wddm_bind_gs_state(struct pipe_context *pipe, void *shader)
{
   d3d10_wddm_bind_shader(pipe, 2, shader);
}

static void
d3d10_wddm_delete_vs_state(struct pipe_context *pipe, void *shader)
{
   d3d10_wddm_delete_shader(pipe, 0, shader);
}

static void
d3d10_wddm_delete_fs_state(struct pipe_context *pipe, void *shader)
{
   d3d10_wddm_delete_shader(pipe, 1, shader);
}

static void
d3d10_wddm_delete_gs_state(struct pipe_context *pipe, void *shader)
{
   d3d10_wddm_delete_shader(pipe, 2, shader);
}

static void
d3d10_wddm_context_destroy(struct pipe_context *pipe)
{
   struct d3d10_wddm_context *wc = d3d10_wddm_context(pipe);
   void (*destroy)(struct pipe_context *) = wc->destroy;

   /* The driver's own entry points go back first: its destroy calls some of
    * them (zink unbinds its framebuffer and flushes), and by then this
    * context is no longer in the table those hooks look themselves up in. */
   pipe->flush = wc->flush;
   pipe->set_framebuffer_state = wc->set_framebuffer_state;
   pipe->set_sampler_views = wc->set_sampler_views;
   pipe->blit = wc->blit;
   pipe->resource_copy_region = wc->resource_copy_region;
   pipe->create_vs_state = wc->create_shader[0];
   pipe->create_fs_state = wc->create_shader[1];
   pipe->create_gs_state = wc->create_shader[2];
   pipe->bind_vs_state = wc->bind_shader[0];
   pipe->bind_fs_state = wc->bind_shader[1];
   pipe->bind_gs_state = wc->bind_shader[2];
   pipe->delete_vs_state = wc->delete_shader[0];
   pipe->delete_fs_state = wc->delete_shader[1];
   pipe->delete_gs_state = wc->delete_shader[2];
   pipe->buffer_map = wc->buffer_map;
   pipe->texture_map = wc->texture_map;
   simple_mtx_lock(&d3d10_wddm_tables);
   wc->pipe = NULL;
   simple_mtx_unlock(&d3d10_wddm_tables);
   destroy(pipe);
}

static struct pipe_context *
d3d10_wddm_context_create(struct pipe_screen *screen, void *priv,
                          unsigned flags)
{
   struct d3d10_wddm_screen *ws = d3d10_wddm_screen(screen);
   struct pipe_context *pipe = ws->context_create(screen, priv, flags);
   struct d3d10_wddm_context *wc = NULL;

   if (!pipe || !ws->gpu)
      return pipe;

   simple_mtx_lock(&d3d10_wddm_tables);
   for (unsigned i = 0; i < D3D10_WDDM_MAX_CONTEXTS; i++) {
      if (d3d10_wddm_contexts[i].pipe == NULL) {
         wc = &d3d10_wddm_contexts[i];
         wc->pipe = pipe;
         wc->flushing = false;
         break;
      }
   }
   simple_mtx_unlock(&d3d10_wddm_tables);
   if (!wc)
      return pipe;

   wc->ws = ws;
   wc->destroy = pipe->destroy;
   wc->flush = pipe->flush;
   wc->set_framebuffer_state = pipe->set_framebuffer_state;
   wc->set_sampler_views = pipe->set_sampler_views;
   wc->blit = pipe->blit;
   wc->resource_copy_region = pipe->resource_copy_region;
   wc->create_shader[0] = pipe->create_vs_state;
   wc->create_shader[1] = pipe->create_fs_state;
   wc->create_shader[2] = pipe->create_gs_state;
   wc->buffer_map = pipe->buffer_map;
   wc->texture_map = pipe->texture_map;
   pipe->buffer_map = d3d10_wddm_buffer_map;
   pipe->texture_map = d3d10_wddm_texture_map;
   memset(wc->bound_shader, 0, sizeof wc->bound_shader);
   wc->bind_shader[0] = pipe->bind_vs_state;
   wc->bind_shader[1] = pipe->bind_fs_state;
   wc->bind_shader[2] = pipe->bind_gs_state;
   wc->delete_shader[0] = pipe->delete_vs_state;
   wc->delete_shader[1] = pipe->delete_fs_state;
   wc->delete_shader[2] = pipe->delete_gs_state;
   pipe->bind_vs_state = d3d10_wddm_bind_vs_state;
   pipe->bind_fs_state = d3d10_wddm_bind_fs_state;
   pipe->bind_gs_state = d3d10_wddm_bind_gs_state;
   pipe->delete_vs_state = d3d10_wddm_delete_vs_state;
   pipe->delete_fs_state = d3d10_wddm_delete_fs_state;
   pipe->delete_gs_state = d3d10_wddm_delete_gs_state;
   pipe->create_vs_state = d3d10_wddm_create_vs_state;
   pipe->create_fs_state = d3d10_wddm_create_fs_state;
   pipe->create_gs_state = d3d10_wddm_create_gs_state;
   pipe->destroy = d3d10_wddm_context_destroy;
   pipe->flush = d3d10_wddm_flush;
   pipe->set_framebuffer_state = d3d10_wddm_set_framebuffer_state;
   pipe->set_sampler_views = d3d10_wddm_set_sampler_views;
   pipe->blit = d3d10_wddm_blit;
   pipe->resource_copy_region = d3d10_wddm_resource_copy_region;
   return pipe;
}

static void
d3d10_wddm_screen_destroy(struct pipe_screen *screen)
{
   struct d3d10_wddm_screen *ws = d3d10_wddm_screen(screen);
   void (*destroy)(struct pipe_screen *) = ws->destroy;

   /* The plane goes with the screen that put frames on it. */
   if (ws->overlay_device && d3d10_wddm_overlay_entry())
      d3d10_wddm_overlay_entry()(ws->overlay_device, 0, 0, 0, 0, 0, 0);
   /* The device this screen logged through goes with it. */
   if (d3d10_wddm_log_device == ws->device)
      d3d10_wddm_log_device = NULL;
   if (ws->shared) {
      hash_table_foreach(ws->shared, entry)
         FREE(entry->data);
      _mesa_hash_table_destroy(ws->shared, NULL);
      ws->shared = NULL;
   }
   simple_mtx_lock(&d3d10_wddm_tables);
   ws->screen = NULL;
   simple_mtx_unlock(&d3d10_wddm_tables);
   destroy(screen);
}

struct pipe_screen *
d3d10_create_screen(struct gdikmt_device *device)
{
   struct pipe_screen *screen = NULL;
   struct sw_winsys *winsys;
   struct d3d10_wddm_screen *ws = NULL;
   bool gpu = false;

   static volatile LONG handler_installed;

   d3d10_wddm_log_device = device;
   if (InterlockedExchange(&handler_installed, 1) == 0) {
      d3d10_wddm_exception_handle =
         AddVectoredExceptionHandler(1, d3d10_wddm_exception);
      atexit(d3d10_wddm_exception_remove);
   }

   winsys = wddm_sw_winsys_create(device);
   if (!winsys)
      return NULL;

#ifdef GALLIUM_ZINK
   if (strcmp(debug_get_option("ENTANGLED_D3D10_DRIVER", "zink"), "softpipe") != 0) {
      screen = zink_create_screen(NULL, NULL);
      gpu = screen != NULL;
   }
#endif
   if (screen == NULL)
      screen = softpipe_create_screen(winsys);
   if (screen == NULL) {
      winsys->destroy(winsys);
      return NULL;
   }

   simple_mtx_lock(&d3d10_wddm_tables);
   for (unsigned i = 0; i < D3D10_WDDM_MAX_SCREENS; i++) {
      if (d3d10_wddm_screens[i].screen == NULL) {
         ws = &d3d10_wddm_screens[i];
         memset(ws, 0, sizeof *ws);
         ws->screen = screen;
         break;
      }
   }
   simple_mtx_unlock(&d3d10_wddm_tables);
   if (!ws) {
      screen->destroy(screen);
      return NULL;
   }

   d3d10_wddm_log("screen %p: %s", (void *)screen, gpu ? "zink" : "softpipe");

   ws->winsys = winsys;
   ws->device = device;
   ws->gpu = gpu;
   ws->destroy = screen->destroy;
   ws->context_create = screen->context_create;
   ws->resource_create = screen->resource_create;
   ws->resource_from_handle = screen->resource_from_handle;
   ws->resource_get_handle = screen->resource_get_handle;
   ws->resource_destroy = screen->resource_destroy;
   ws->flush_frontbuffer = screen->flush_frontbuffer;
   screen->destroy = d3d10_wddm_screen_destroy;
   screen->resource_from_handle = d3d10_wddm_resource_from_handle;
   if (gpu) {
      simple_mtx_init(&ws->lock, mtx_plain);
      ws->shared = _mesa_pointer_hash_table_create(NULL);
      screen->context_create = d3d10_wddm_context_create;
      screen->resource_create = d3d10_wddm_resource_create;
      screen->resource_get_handle = d3d10_wddm_resource_get_handle;
      screen->resource_destroy = d3d10_wddm_resource_destroy;
      screen->flush_frontbuffer = d3d10_wddm_flush_frontbuffer;
   }

   return screen;
}
