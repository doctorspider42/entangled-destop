/*
 * Copyright 2020 Google LLC
 * SPDX-License-Identifier: MIT
 */

<%namespace name="types" file="/types.h"/>\
<%namespace name="union" file="/types_union.h"/>\
<%namespace name="command" file="/types_command.h"/>\
\
<%def name="submit_command(ty)">\
static inline void vn_submit_${ty.name}(struct vn_ring *vn_ring, VkCommandFlagsEXT cmd_flags, ${ty.c_func_params()}, struct vn_ring_submit_command *submit)
{
    uint8_t local_cmd_data[VN_SUBMIT_LOCAL_CMD_SIZE];
    void *cmd_data = local_cmd_data;
    size_t cmd_size = vn_sizeof_${ty.name}(${ty.c_func_args()});
    if (cmd_size > sizeof(local_cmd_data)) {
        cmd_data = malloc(cmd_size);
        if (!cmd_data)
            cmd_size = 0;
    }
    const size_t reply_size = cmd_flags & VK_COMMAND_GENERATE_REPLY_BIT_EXT ? vn_sizeof_${ty.name}_reply(${ty.c_func_args()}) : 0;

    struct vn_cs_encoder *enc = vn_ring_submit_command_init(vn_ring, submit, cmd_data, cmd_size, reply_size);
    if (cmd_size) {
        vn_encode_${ty.name}(enc, cmd_flags, ${ty.c_func_args()});
        vn_ring_submit_command(vn_ring, submit);
        if (cmd_data != local_cmd_data)
            free(cmd_data);
    }
}
</%def>\
\
<%def name="call_command(ty)">\
static inline ${ty.c_func_ret()} vn_call_${ty.name}(struct vn_ring *vn_ring, ${ty.c_func_params()})
{
    VN_TRACE_FUNC();

    struct vn_ring_submit_command submit;
    vn_submit_${ty.name}(vn_ring, VK_COMMAND_GENERATE_REPLY_BIT_EXT, ${ty.c_func_args()}, &submit);
    struct vn_cs_decoder *dec = vn_ring_get_command_reply(vn_ring, &submit);
%   if ty.ret:
    if (dec) {
        const ${ty.ret.to_c()} = vn_decode_${ty.name}_reply(dec, ${ty.c_func_args()});
        vn_ring_free_command_reply(vn_ring, &submit);
        return ${ty.ret.name};
    } else {
        return VK_ERROR_OUT_OF_HOST_MEMORY;
    }
%   else:
    if (dec) {
        vn_decode_${ty.name}_reply(dec, ${ty.c_func_args()});
        vn_ring_free_command_reply(vn_ring, &submit);
    }
%   endif
}
</%def>\
\
<%def name="async_command(ty)">\
static inline void vn_async_${ty.name}(struct vn_ring *vn_ring, ${ty.c_func_params()})
{
    struct vn_ring_submit_command submit;
    vn_submit_${ty.name}(vn_ring, 0, ${ty.c_func_args()}, &submit);
}
</%def>\
\
#ifndef VN_PROTOCOL_DRIVER_${GUARD}_H
#define VN_PROTOCOL_DRIVER_${GUARD}_H

% if GUARD == 'STRUCTS':
#include "vn_protocol_driver_handles.h"
% else:
#include "vn_ring.h"
#include "vn_protocol_driver_structs.h"
% endif

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

${types.vn_sizeof_type_helpers(ty)}\
${types.vn_sizeof_type(ty)}
\
%   if 'need_encode' in ty.attrs:
${types.vn_encode_type_helpers(ty)}\
${types.vn_encode_type(ty)}
%   endif
\
%   if 'need_decode' in ty.attrs:
${types.vn_decode_type_helpers(ty)}\
${types.vn_decode_type(ty)}
%   endif
\
%   if 'need_partial' in ty.attrs and ty.category == ty.STRUCT:
${types.vn_sizeof_type_helpers(ty, '_partial')}\
${types.vn_sizeof_type_partial(ty)}
\
${types.vn_encode_type_helpers(ty, '_partial')}\
${types.vn_encode_type_partial(ty)}
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

${union.vn_sizeof_union_tag(ty)}
%   if 'need_encode' in ty.attrs:
${union.vn_encode_union_tag(ty)}
%   endif
%   if 'need_decode' in ty.attrs:
${types.vn_decode_type(ty)}
%   endif
% endfor
\
% for ty in COMMAND_TYPES:
${command.vn_sizeof_command(ty)}
${command.vn_encode_command(ty)}
${command.vn_sizeof_command_reply(ty)}
${command.vn_decode_command_reply(ty)}
% endfor
\
% for ty in COMMAND_TYPES:
${submit_command(ty)}
% endfor
% for ty in COMMAND_TYPES:
%   if ty.has_out_ty:
${call_command(ty)}
%   endif
${async_command(ty)}
% endfor
\
#endif /* VN_PROTOCOL_DRIVER_${GUARD}_H */
