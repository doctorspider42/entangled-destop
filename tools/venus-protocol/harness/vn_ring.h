/*
 * Copyright 2026 The Entangled Desktop authors
 * SPDX-License-Identifier: MIT
 *
 * vn_protocol_driver_*.h also emits vn_submit_ and vn_call_ helpers that go
 * through a ring. The harness never calls them; these are the declarations
 * they need to compile (as upstream's tests/vn_ring.h provides).
 */

#ifndef VN_RING_H
#define VN_RING_H

#include "vn_cs.h"

#define VN_TRACE_FUNC()

struct vn_ring;

struct vn_ring_submit_command {
   int dummy;
};

static inline struct vn_cs_encoder *
vn_ring_submit_command_init(struct vn_ring *ring,
                            struct vn_ring_submit_command *submit,
                            void *cmd_data,
                            size_t cmd_size,
                            size_t reply_size)
{
   (void)ring;
   (void)submit;
   (void)cmd_data;
   (void)cmd_size;
   (void)reply_size;
   return NULL;
}

static inline void
vn_ring_submit_command(struct vn_ring *ring, struct vn_ring_submit_command *submit)
{
   (void)ring;
   (void)submit;
}

static inline struct vn_cs_decoder *
vn_ring_get_command_reply(struct vn_ring *ring, struct vn_ring_submit_command *submit)
{
   (void)ring;
   (void)submit;
   return NULL;
}

static inline void
vn_ring_free_command_reply(struct vn_ring *ring, struct vn_ring_submit_command *submit)
{
   (void)ring;
   (void)submit;
}

#endif /* VN_RING_H */
