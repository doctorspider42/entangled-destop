/*
 * Copyright 2026 The Entangled Desktop authors
 * SPDX-License-Identifier: MIT
 *
 * The guest-side command-stream primitives vn_protocol_driver_*.h expects
 * (see templates/driver_cs.h), implemented over a plain buffer. Semantics
 * follow Mesa's vn_cs.h: a write copies val_size bytes and advances by size
 * (the padding is zero here); a short read is fatal and zero-fills.
 *
 * Handles are carried as their object id: load_id/store_id are identity
 * conversions between the pointer-sized handle and the id.
 */

#ifndef VN_CS_H
#define VN_CS_H

#include <stdbool.h>
#include <stdint.h>
#include <stdlib.h>
#include <string.h>

#include <vulkan/vulkan.h>

#include "harness.h"

typedef uint64_t vn_object_id;

struct vn_cs_encoder {
   struct hbuf buf;
};

struct vn_cs_decoder {
   const uint8_t *data;
   size_t len;
   size_t pos;
   bool fatal;
};

static inline bool
vn_cs_renderer_protocol_has_api_version(uint32_t api_version)
{
   (void)api_version;
   return true;
}

static inline bool
vn_cs_renderer_protocol_has_extension(uint32_t ext_number)
{
   (void)ext_number;
   return true;
}

static inline size_t
vn_cs_encoder_get_len(const struct vn_cs_encoder *enc)
{
   return enc->buf.len;
}

static inline bool
vn_cs_encoder_reserve(struct vn_cs_encoder *enc, size_t size)
{
   hbuf_reserve(&enc->buf, size);
   return true;
}

static inline void
vn_cs_encoder_write(struct vn_cs_encoder *enc, size_t size, const void *val, size_t val_size)
{
   hbuf_reserve(&enc->buf, size);
   memset(enc->buf.data + enc->buf.len, 0, size);
   if (val_size)
      memcpy(enc->buf.data + enc->buf.len, val, val_size);
   enc->buf.len += size;
}

static inline void
vn_cs_decoder_set_fatal(struct vn_cs_decoder *dec)
{
   dec->fatal = true;
}

static inline void
vn_cs_decoder_peek(struct vn_cs_decoder *dec, size_t size, void *val, size_t val_size)
{
   if (dec->fatal || size > dec->len - dec->pos) {
      dec->fatal = true;
      memset(val, 0, val_size);
      return;
   }
   memcpy(val, dec->data + dec->pos, val_size);
}

static inline void
vn_cs_decoder_read(struct vn_cs_decoder *dec, size_t size, void *val, size_t val_size)
{
   vn_cs_decoder_peek(dec, size, val, val_size);
   if (!dec->fatal)
      dec->pos += size;
}

static inline vn_object_id
vn_cs_handle_load_id(const void **handle, VkObjectType type)
{
   (void)type;
   return (vn_object_id)(uintptr_t)*handle;
}

static inline void
vn_cs_handle_store_id(void **handle, vn_object_id id, VkObjectType type)
{
   (void)type;
   *handle = (void *)(uintptr_t)id;
}

#endif /* VN_CS_H */
