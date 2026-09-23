/*
 * Copyright 2026 The Entangled Desktop authors
 * SPDX-License-Identifier: MIT
 *
 * The host-side command-stream primitives vn_protocol_renderer_*.h expects
 * (see templates/renderer_cs.h), implemented over a plain buffer, modelled on
 * virglrenderer's vkr_cs.h: a short read is fatal and zero-fills, temporary
 * allocations come from a pool that is never freed, and object lookup hands
 * back a stand-in object whose handle is the id itself.
 */

#ifndef VKR_CS_H
#define VKR_CS_H

#include <stdbool.h>
#include <stdint.h>
#include <stdlib.h>
#include <string.h>

#include <vulkan/vulkan.h>

#include "harness.h"

typedef uint64_t vkr_object_id;

struct vkr_object {
   union {
      uint64_t u64;
   } handle;
};

struct vkr_cs_encoder {
   struct hbuf buf;
};

struct vkr_cs_decoder {
   const uint8_t *data;
   size_t len;
   size_t pos;
   bool fatal;
};

static inline bool
vkr_cs_encoder_acquire(struct vkr_cs_encoder *enc)
{
   (void)enc;
   return true;
}

static inline void
vkr_cs_encoder_release(struct vkr_cs_encoder *enc)
{
   (void)enc;
}

static inline void
vkr_cs_encoder_write(struct vkr_cs_encoder *enc, size_t size, const void *val, size_t val_size)
{
   hbuf_reserve(&enc->buf, size);
   memset(enc->buf.data + enc->buf.len, 0, size);
   if (val_size)
      memcpy(enc->buf.data + enc->buf.len, val, val_size);
   enc->buf.len += size;
}

static inline void
vkr_cs_decoder_set_fatal(const struct vkr_cs_decoder *dec)
{
   ((struct vkr_cs_decoder *)dec)->fatal = true;
}

static inline bool
vkr_cs_decoder_get_fatal(const struct vkr_cs_decoder *dec)
{
   return dec->fatal;
}

static inline void
vkr_cs_decoder_peek(const struct vkr_cs_decoder *dec, size_t size, void *val, size_t val_size)
{
   if (dec->fatal || size > dec->len - dec->pos) {
      vkr_cs_decoder_set_fatal(dec);
      memset(val, 0, val_size);
      return;
   }
   memcpy(val, dec->data + dec->pos, val_size);
}

static inline void
vkr_cs_decoder_read(struct vkr_cs_decoder *dec, size_t size, void *val, size_t val_size)
{
   vkr_cs_decoder_peek(dec, size, val, val_size);
   if (!dec->fatal)
      dec->pos += size;
}

static inline struct vkr_object *
vkr_cs_decoder_lookup_object(const struct vkr_cs_decoder *dec, vkr_object_id id, VkObjectType type)
{
   (void)dec;
   (void)type;
   if (!id)
      return NULL;
   struct vkr_object *obj = h_calloc(1, sizeof(*obj));
   obj->handle.u64 = id;
   return obj;
}

static inline void
vkr_cs_decoder_reset_temp_pool(struct vkr_cs_decoder *dec)
{
   (void)dec;
}

static inline void *
vkr_cs_decoder_alloc_temp(struct vkr_cs_decoder *dec, size_t size)
{
   if (size > ((size_t)1 << 30)) {
      vkr_cs_decoder_set_fatal(dec);
      return NULL;
   }
   return h_calloc(1, size);
}

static inline void *
vkr_cs_decoder_alloc_temp_array(struct vkr_cs_decoder *dec, size_t size, size_t count)
{
   if (size && count > (((size_t)1 << 30) / size)) {
      vkr_cs_decoder_set_fatal(dec);
      return NULL;
   }
   return h_calloc(count, size);
}

static inline void *
vkr_cs_decoder_get_blob_storage(struct vkr_cs_decoder *dec, size_t size)
{
   return vkr_cs_decoder_alloc_temp(dec, size);
}

static inline void *
vkr_cs_encoder_get_blob_storage(struct vkr_cs_encoder *enc, size_t offset, size_t size)
{
   (void)enc;
   (void)offset;
   return h_calloc(1, size);
}

static inline bool
vkr_cs_handle_indirect_id(VkObjectType type)
{
   (void)type;
   return false;
}

static inline vkr_object_id
vkr_cs_handle_load_id(const void **handle, VkObjectType type)
{
   (void)type;
   return (vkr_object_id)(uintptr_t)*handle;
}

static inline void
vkr_cs_handle_store_id(void **handle, vkr_object_id id, VkObjectType type)
{
   (void)type;
   *handle = (void *)(uintptr_t)id;
}

#endif /* VKR_CS_H */
