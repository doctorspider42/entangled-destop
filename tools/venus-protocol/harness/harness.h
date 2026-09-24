/*
 * Copyright 2026 The Entangled Desktop authors
 * SPDX-License-Identifier: MIT
 *
 * Shared bits of the differential harness: a byte buffer, a deterministic
 * RNG and an allocator that never frees (a run is a few MB and then exits).
 */

#ifndef HARNESS_H
#define HARNESS_H

#include <stdbool.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

struct hbuf {
   uint8_t *data;
   size_t len;
   size_t cap;
};

static inline void
hbuf_reserve(struct hbuf *b, size_t extra)
{
   if (b->len + extra <= b->cap)
      return;
   size_t cap = b->cap ? b->cap : 256;
   while (cap < b->len + extra)
      cap *= 2;
   b->data = realloc(b->data, cap);
   if (!b->data) {
      fprintf(stderr, "harness: out of memory\n");
      exit(2);
   }
   b->cap = cap;
}

static inline void
hbuf_put(struct hbuf *b, const void *data, size_t len)
{
   hbuf_reserve(b, len);
   memcpy(b->data + b->len, data, len);
   b->len += len;
}

static inline void
hbuf_put_u32(struct hbuf *b, uint32_t v)
{
   hbuf_put(b, &v, 4);
}

static inline void
hbuf_put_u64(struct hbuf *b, uint64_t v)
{
   hbuf_put(b, &v, 8);
}

struct hrng {
   uint64_t state;
   /* chain-poisoning request, and the link chosen for it: the sType to
    * find in the encoded bytes and the one to write over it */
   int poison;
   int32_t poison_stype;
   int32_t poison_with;
};

static inline uint64_t
hrng_next(struct hrng *r)
{
   /* xorshift64* */
   uint64_t x = r->state;
   x ^= x >> 12;
   x ^= x << 25;
   x ^= x >> 27;
   r->state = x;
   return x * 0x2545F4914F6CDD1DULL;
}

static inline void
hrng_seed(struct hrng *r, uint64_t seed)
{
   r->state = seed * 0x9E3779B97F4A7C15ULL + 0x1234567;
   if (!r->state)
      r->state = 1;
   r->poison = 0;
   r->poison_stype = 0;
   r->poison_with = 0;
   hrng_next(r);
}

static inline uint32_t
hrng_below(struct hrng *r, uint32_t n)
{
   return n ? (uint32_t)(hrng_next(r) % n) : 0;
}

static inline void *
h_calloc(size_t count, size_t size)
{
   void *p = calloc(count ? count : 1, size ? size : 1);
   if (!p) {
      fprintf(stderr, "harness: out of memory\n");
      exit(2);
   }
   return p;
}

/* A short printable NUL-terminated string of 0..12 characters. */
static inline const char *
h_string(struct hrng *r)
{
   uint32_t len = hrng_below(r, 13);
   char *s = h_calloc(len + 1, 1);
   for (uint32_t i = 0; i < len; i++)
      s[i] = (char)('a' + hrng_below(r, 26));
   return s;
}

/* A char[N]: printable up to a random NUL, zero after it, last byte NUL. */
static inline void
h_chars(struct hrng *r, char *dst, size_t n)
{
   size_t len = n ? hrng_below(r, (uint32_t)(n < 40 ? n : 40)) : 0;
   memset(dst, 0, n);
   for (size_t i = 0; i < len && i + 1 < n; i++)
      dst[i] = (char)('A' + hrng_below(r, 26));
}

static inline uint64_t
h_id(struct hrng *r)
{
   return (hrng_next(r) & 0xffffffffffULL) | 1;
}

static inline float
h_float(struct hrng *r)
{
   return (float)(int32_t)hrng_below(r, 20001) / 64.0f - 150.0f;
}

#endif /* HARNESS_H */
