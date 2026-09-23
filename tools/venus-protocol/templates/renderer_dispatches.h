/*
 * Copyright 2020 Google LLC
 * SPDX-License-Identifier: MIT
 */

#ifndef VN_PROTOCOL_RENDERER_DISPATCHES_H
#define VN_PROTOCOL_RENDERER_DISPATCHES_H

% for inc in INCLUDES:
#include "vn_protocol_renderer_${inc}.h"
% endfor

static inline const char *vn_dispatch_command_name(VkCommandTypeEXT type)
{
    switch (type) {
% for ty in COMMAND_TYPES:
    case ${ty.attrs['c_type']}: return "${ty.name}";
% endfor
% for ty in COMMAND_SKIPPED:
    case ${ty.attrs['c_type']}: return "${ty.name}";
% endfor
    default: return "unknown";
    }
}

static void (*const vn_dispatch_table[${COMMAND_TABLE_SIZE}])(struct vn_dispatch_context *ctx, VkCommandFlagsEXT flags) = {
% for ty in COMMAND_TYPES:
    [${ty.attrs['c_type']}] = vn_dispatch_${ty.name},
% endfor
};

static inline void vn_dispatch_command(struct vn_dispatch_context *ctx)
{
    VkCommandTypeEXT cmd_type;
    VkCommandFlagsEXT cmd_flags;

    vn_decode_VkCommandTypeEXT(ctx->decoder, &cmd_type);
    vn_decode_VkFlags(ctx->decoder, &cmd_flags);

    {
        if (cmd_type < ${COMMAND_TABLE_SIZE} && vn_dispatch_table[cmd_type])
            vn_dispatch_table[cmd_type](ctx, cmd_flags);
        else
            vn_cs_decoder_set_fatal(ctx->decoder);
    }

    if (vn_cs_decoder_get_fatal(ctx->decoder))
        vn_dispatch_debug_log(ctx, "%s resulted in CS error", vn_dispatch_command_name(cmd_type));
}

#endif /* VN_PROTOCOL_RENDERER_DISPATCHES_H */
