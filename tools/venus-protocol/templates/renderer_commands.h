/*
 * Copyright 2020 Google LLC
 * SPDX-License-Identifier: MIT
 */

<%namespace name="command" file="/types_command.h"/>\
<%namespace name="types" file="/types.h"/>\
<%namespace name="union" file="/types_union.h"/>\
\
<%def name="dispatch_command(ty)">\
static inline void vn_dispatch_${ty.name}(struct vn_dispatch_context *ctx, VkCommandFlagsEXT flags)
{
    struct vn_command_${ty.name} args;

    if (!ctx->dispatch_${ty.name}) {
        vn_cs_decoder_set_fatal(ctx->decoder);
        return;
    }

% if 'need_blob_encode' in ty.attrs:
    if (flags & VK_COMMAND_GENERATE_REPLY_BIT_EXT) {
        if (!vn_cs_encoder_acquire(ctx->encoder))
           return;
    }

    vn_decode_${ty.name}_args_temp(ctx->decoder, ctx->encoder, &args);
% else:
    vn_decode_${ty.name}_args_temp(ctx->decoder, &args);
% endif
% if ty.variables and ty.variables[0].ty.base.dispatchable:
    if (!args.${ty.variables[0].name}) {
        vn_cs_decoder_set_fatal(ctx->decoder);
        return;
    }
% endif

    if (!vn_cs_decoder_get_fatal(ctx->decoder))
        ctx->dispatch_${ty.name}(ctx, &args);

% if ty.ret and ty.ret.ty.name == 'VkResult':
#ifdef DEBUG
    if (!vn_cs_decoder_get_fatal(ctx->decoder) && vn_dispatch_should_log_result(args.${ty.ret.name}))
        vn_dispatch_debug_log(ctx, "${ty.name} returned %d", args.${ty.ret.name});
#endif

% endif
% if ty.can_device_lost:
    if (flags & VK_COMMAND_GENERATE_REPLY_BIT_EXT) {
        if (!vn_cs_decoder_get_fatal(ctx->decoder)) {
%   if 'need_blob_encode' in ty.attrs:
            vn_encode_${ty.name}_reply(ctx->encoder, &args);
            vn_cs_encoder_release(ctx->encoder);
%   else:
            if (vn_cs_encoder_acquire(ctx->encoder)) {
                vn_encode_${ty.name}_reply(ctx->encoder, &args);
                vn_cs_encoder_release(ctx->encoder);
            }
%   endif
        }
    } else if (args.${ty.ret.name} == VK_ERROR_DEVICE_LOST) {
        vn_cs_decoder_set_fatal(ctx->decoder);
    }
% else:
    if ((flags & VK_COMMAND_GENERATE_REPLY_BIT_EXT) && !vn_cs_decoder_get_fatal(ctx->decoder)) {
%   if 'need_blob_encode' in ty.attrs:
        vn_encode_${ty.name}_reply(ctx->encoder, &args);
        vn_cs_encoder_release(ctx->encoder);
%   else:
        if (vn_cs_encoder_acquire(ctx->encoder)) {
            vn_encode_${ty.name}_reply(ctx->encoder, &args);
            vn_cs_encoder_release(ctx->encoder);
        }
%   endif
    }
% endif

    vn_cs_decoder_reset_temp_pool(ctx->decoder);
}
</%def>\
\
#ifndef VN_PROTOCOL_RENDERER_${GUARD}_H
#define VN_PROTOCOL_RENDERER_${GUARD}_H

% if GUARD == 'STRUCTS':
#include "vn_protocol_renderer_handles.h"
% else:
#include "vn_protocol_renderer_structs.h"
% endif

#pragma GCC diagnostic push
#if !defined(__clang__) && defined(__GNUC__) && __GNUC__ >= 12
#pragma GCC diagnostic ignored "-Wdangling-pointer"
#endif
#pragma GCC diagnostic ignored "-Wpointer-arith"
#pragma GCC diagnostic ignored "-Wunused-parameter"

<% all_skipped = STRUCT_SKIPPED + MANUAL_UNION_TYPES + COMMAND_SKIPPED %>\
% if all_skipped:
/*
 * These structs/unions/commands are not included
 *
% for ty in all_skipped:
 *   ${ty.name}
% endfor
 */

% endif
% for ty in STRUCT_TYPES:
/* ${types.vn_type_descriptive_name(ty)} */

%   if 'need_encode' in ty.attrs:
${types.vn_encode_type_helpers(ty)}\
${types.vn_encode_type(ty)}
%   endif
\
%   if 'need_decode' in ty.attrs:
${types.vn_decode_type_helpers(ty, '_temp')}\
${types.vn_decode_type_temp(ty)}
%   endif
\
%   if 'need_partial' in ty.attrs and ty.might_contain_handle():
${types.vn_decode_type_helpers(ty, '_partial_temp')}\
${types.vn_decode_type_partial_temp(ty)}
%   endif
\
%   if 'need_decode' in ty.attrs and ty.might_contain_handle():
${types.vn_replace_type_handle_helpers(ty)}\
${types.vn_replace_type_handle(ty)}
%   endif
% endfor
\
% if MANUAL_UNION_TYPES:
/*
 * Helpers for manual serialization
 */

% endif
\
% for ty in MANUAL_UNION_TYPES:
/* ${types.vn_type_descriptive_name(ty)} */

%   if 'need_encode' in ty.attrs:
${union.vn_encode_union_tag(ty)}
%   endif
%   if 'need_decode' in ty.attrs:
${types.vn_decode_type_temp(ty)}
%   endif
% endfor
\
% for ty in COMMAND_TYPES:
${command.vn_decode_command_args_temp(ty)}
${command.vn_replace_command_args_handle(ty)}
${command.vn_encode_command_reply(ty)}
% endfor
\
% for ty in COMMAND_TYPES:
${dispatch_command(ty)}
% endfor
#pragma GCC diagnostic pop

#endif /* VN_PROTOCOL_RENDERER_${GUARD}_H */
