/*
 * Copyright 2020 Google LLC
 * SPDX-License-Identifier: MIT
 */

<%def name="vn_sizeof_command(ty)">\
static inline size_t vn_sizeof_${ty.name}(${ty.c_func_params()})
{
    const VkCommandTypeEXT cmd_type = ${ty.attrs['c_type']};
    const VkFlags cmd_flags = 0;
    size_t cmd_size = vn_sizeof_VkCommandTypeEXT(&cmd_type) + vn_sizeof_VkFlags(&cmd_flags);

% for var in ty.variables:
    ${GEN.sizeof_command_arg(ty, var, '', 'cmd_size')}
% endfor

    return cmd_size;
}
</%def>

<%def name="vn_encode_command(ty)">\
static inline void vn_encode_${ty.name}(struct vn_cs_encoder *enc, VkCommandFlagsEXT cmd_flags, ${ty.c_func_params()})
{
    const VkCommandTypeEXT cmd_type = ${ty.attrs['c_type']};

    vn_encode_VkCommandTypeEXT(enc, &cmd_type);
    vn_encode_VkFlags(enc, &cmd_flags);

% for var in ty.variables:
    ${GEN.encode_command_arg(ty, var, '')}
% endfor
}
</%def>

<%def name="vn_decode_command_args_temp(ty)">\
% if 'need_blob_encode' in ty.attrs:
static inline void vn_decode_${ty.name}_args_temp(struct vn_cs_decoder *dec, struct vn_cs_encoder *enc, struct vn_command_${ty.name} *args)
{
    const VkCommandTypeEXT cmd_type = ${ty.attrs['c_type']};
    size_t offset = vn_sizeof_VkCommandTypeEXT(&cmd_type);
%   if ty.ret:

    ${ty.ret.to_c()};
    ${GEN.sizeof_command_reply(ty, ty.ret, '', 'offset')}
% endif
%   for var in ty.variables:
    ${GEN.decode_command_arg(ty, var, 'args->')}
%   endfor
}
% else:
static inline void vn_decode_${ty.name}_args_temp(struct vn_cs_decoder *dec, struct vn_command_${ty.name} *args)
{
%   for var in ty.variables:
    ${GEN.decode_command_arg(ty, var, 'args->')}
%   endfor
}
% endif
</%def>

<%def name="vn_sizeof_command_reply(ty)">\
static inline size_t vn_sizeof_${ty.name}_reply(${ty.c_func_params()})
{
    const VkCommandTypeEXT cmd_type = ${ty.attrs['c_type']};
    size_t cmd_size = vn_sizeof_VkCommandTypeEXT(&cmd_type);

% if ty.ret:
    ${ty.ret.to_c()};
    ${GEN.sizeof_command_reply(ty, ty.ret, '', 'cmd_size')}
% endif
% for var in ty.variables:
    ${GEN.sizeof_command_reply(ty, var, '', 'cmd_size')}
% endfor

    return cmd_size;
}
</%def>

<%def name="vn_encode_command_reply(ty)">\
static inline void vn_encode_${ty.name}_reply(struct vn_cs_encoder *enc, const struct vn_command_${ty.name} *args)
{
    vn_encode_VkCommandTypeEXT(enc, &(VkCommandTypeEXT){${ty.attrs['c_type']}});

% if ty.ret:
    ${GEN.encode_command_reply(ty, ty.ret, 'args->')}
% endif
% for var in ty.variables:
    ${GEN.encode_command_reply(ty, var, 'args->')}
% endfor
}
</%def>

<%def name="vn_decode_command_reply(ty)">\
static inline ${ty.c_func_ret()} vn_decode_${ty.name}_reply(struct vn_cs_decoder *dec, ${ty.c_func_params()})
{
    VkCommandTypeEXT command_type;
    vn_decode_VkCommandTypeEXT(dec, &command_type);
    assert(command_type == ${ty.attrs['c_type']});

% if ty.ret:
    ${ty.ret.to_c()};
    ${GEN.decode_command_reply(ty, ty.ret, '')}
% endif
% for var in ty.variables:
    ${GEN.decode_command_reply(ty, var, '')}
% endfor
% if ty.ret:

    return ${ty.ret.name};
% endif
}
</%def>

<%def name="vn_replace_command_args_handle(ty)">\
static inline void vn_replace_${ty.name}_args_handle(struct vn_command_${ty.name} *args)
{
% for var in ty.variables:
    ${GEN.replace_command_arg_handle(ty, var, 'args->')}
% endfor
}
</%def>
