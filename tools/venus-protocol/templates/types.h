/*
 * Copyright 2020 Google LLC
 * SPDX-License-Identifier: MIT
 */

<%namespace name="chain" file="/types_chain.h"/>
<%namespace name="handle" file="/types_handle.h"/>
<%namespace name="scalar" file="/types_scalar.h"/>
<%namespace name="struct" file="/types_struct.h"/>
<%namespace name="union" file="/types_union.h"/>

<%def name="vn_type_descriptive_name(ty)">\
% if ty.category == ty.DEFAULT:
${ty.name}\
% elif ty.category == ty.BASETYPE:
typedef ${ty.typedef.name} ${ty.name}\
% elif ty.category == ty.ENUM and ty.enums.bitwidth == 32:
enum ${ty.name}\
% elif ty.category == ty.ENUM and ty.enums.bitwidth == 64:
typedef VkFlags64 ${ty.name}\
% elif ty.category == ty.HANDLE:
VK_DEFINE_${"" if ty.dispatchable else "NON_DISPATCHABLE_"}HANDLE(${ty.name})\
% elif ty.category == ty.UNION:
union ${ty.name}\
% elif ty.category == ty.STRUCT:
struct ${ty.name}${" chain" if ty.s_type else ""}\
% else:
<% assert False %>
% endif
</%def>

<%def name="vn_sizeof_type(ty)">\
static inline size_t
% if ty.is_valid_union():
vn_sizeof_${ty.name}(const ${ty.name} *val, ${ty.sty.name} tag)
% else:
vn_sizeof_${ty.name}(const ${ty.name} *val)
% endif
{
% if ty.category in [ty.DEFAULT, ty.BASETYPE, ty.ENUM]:
${scalar.vn_sizeof_scalar_body(ty)}\
% elif ty.category == ty.HANDLE:
${handle.vn_sizeof_handle_body(ty)}\
% elif ty.category == ty.UNION:
${union.vn_sizeof_union_body(ty)}\
% elif ty.category == ty.STRUCT and not ty.s_type:
${struct.vn_sizeof_struct_body(ty)}\
% elif ty.category == ty.STRUCT and ty.s_type:
${chain.vn_sizeof_chain_body(ty)}\
% else:
<% assert False %>
% endif
}
</%def>

<%def name="vn_sizeof_type_helpers(ty, variant='')">\
% if ty.category == ty.STRUCT and ty.s_type:
${chain.vn_sizeof_chain_pnext(ty, variant)}
${chain.vn_sizeof_chain_self(ty, variant)}
% endif
</%def>

<%def name="vn_encode_type(ty)">\
static inline void
% if ty.is_valid_union():
vn_encode_${ty.name}(struct vn_cs_encoder *enc, const ${ty.name} *val, ${ty.sty.name} tag)
% else:
vn_encode_${ty.name}(struct vn_cs_encoder *enc, const ${ty.name} *val)
% endif
{
% if ty.category in [ty.DEFAULT, ty.BASETYPE, ty.ENUM]:
${scalar.vn_encode_scalar_body(ty)}\
% elif ty.category == ty.HANDLE:
${handle.vn_encode_handle_body(ty)}\
% elif ty.category == ty.UNION:
${union.vn_encode_union_body(ty)}\
% elif ty.category == ty.STRUCT and not ty.s_type:
${struct.vn_encode_struct_body(ty)}\
% elif ty.category == ty.STRUCT and ty.s_type:
${chain.vn_encode_chain_body(ty)}\
% else:
<% assert False %>
% endif
}
</%def>

<%def name="vn_encode_type_helpers(ty, variant='')">\
% if ty.category == ty.STRUCT and ty.s_type:
${chain.vn_encode_chain_pnext(ty, variant)}
${chain.vn_encode_chain_self(ty, variant)}
% endif
</%def>

<%def name="vn_decode_type(ty)">\
static inline void
vn_decode_${ty.name}(struct vn_cs_decoder *dec, ${ty.name} *val)
{
% if ty.category in [ty.DEFAULT, ty.BASETYPE, ty.ENUM]:
${scalar.vn_decode_scalar_body(ty)}\
% elif ty.category == ty.HANDLE:
${handle.vn_decode_handle_body(ty)}\
% elif ty.category == ty.UNION:
${union.vn_decode_union_body(ty)}\
% elif ty.category == ty.STRUCT and not ty.s_type:
${struct.vn_decode_struct_body(ty)}\
% elif ty.category == ty.STRUCT and ty.s_type:
${chain.vn_decode_chain_body(ty)}\
% else:
<% assert False %>
% endif
}
</%def>

<%def name="vn_decode_type_helpers(ty, variant='')">\
% if ty.category == ty.STRUCT and ty.s_type:
%   if '_temp' in variant:
${chain.vn_decode_chain_pnext_temp(ty, variant.replace('_temp', ''))}
%   else:
${chain.vn_decode_chain_pnext(ty)}
%   endif
${chain.vn_decode_chain_self(ty, variant)}
% endif
</%def>

<%def name="vn_decode_type_temp(ty)">\
static inline void
vn_decode_${ty.name}_temp(struct vn_cs_decoder *dec, ${ty.name} *val)
{
% if ty.category in [ty.DEFAULT, ty.BASETYPE, ty.ENUM]:
${scalar.vn_decode_scalar_body(ty)}\
% elif ty.category == ty.HANDLE and not GEN.is_driver:
${handle.vn_decode_handle_body(ty, '_temp')}\
% elif ty.category == ty.UNION:
${union.vn_decode_union_body(ty, '_temp')}\
% elif ty.category == ty.STRUCT and not ty.s_type:
${struct.vn_decode_struct_body(ty, '_temp')}\
% elif ty.category == ty.STRUCT and ty.s_type:
${chain.vn_decode_chain_body(ty, '_temp')}\
% else:
<% assert False %>
% endif
}
</%def>

<%def name="vn_sizeof_type_partial(ty)">\
static inline size_t
vn_sizeof_${ty.name}_partial(const ${ty.name} *val)
{
% if ty.category in [ty.DEFAULT, ty.BASETYPE, ty.ENUM]:
${scalar.vn_sizeof_scalar_body(ty)}\
% elif ty.category == ty.HANDLE:
${handle.vn_sizeof_handle_body(ty)}\
% elif ty.category == ty.UNION:
    assert(false); /* no user? */
% elif ty.category == ty.STRUCT and not ty.s_type:
${struct.vn_sizeof_struct_body(ty, '_partial')}\
% elif ty.category == ty.STRUCT and ty.s_type:
${chain.vn_sizeof_chain_body(ty, '_partial')}\
% else:
<% assert False %>
% endif
}
</%def>

<%def name="vn_encode_type_partial(ty)">\
static inline void
vn_encode_${ty.name}_partial(struct vn_cs_encoder *enc, const ${ty.name} *val)
{
% if ty.category in [ty.DEFAULT, ty.BASETYPE, ty.ENUM]:
${scalar.vn_encode_scalar_body(ty)}\
% elif ty.category == ty.HANDLE:
${handle.vn_encode_handle_body(ty)}\
% elif ty.category == ty.UNION:
    assert(false); /* no user? */
% elif ty.category == ty.STRUCT and not ty.s_type:
${struct.vn_encode_struct_body(ty, '_partial')}\
% elif ty.category == ty.STRUCT and ty.s_type:
${chain.vn_encode_chain_body(ty, '_partial')}\
% else:
<% assert False %>
% endif
}
</%def>

<%def name="vn_decode_type_partial_temp(ty)">\
static inline void
vn_decode_${ty.name}_partial_temp(struct vn_cs_decoder *dec, ${ty.name} *val)
{
% if ty.category in [ty.DEFAULT, ty.BASETYPE, ty.ENUM]:
${scalar.vn_decode_scalar_body(ty)}\
% elif ty.category == ty.HANDLE:
${handle.vn_decode_handle_body(ty, '_temp')}\
% elif ty.category == ty.UNION:
    assert(false); /* no user? */
% elif ty.category == ty.STRUCT and not ty.s_type:
${struct.vn_decode_struct_body(ty, '_partial_temp')}\
% elif ty.category == ty.STRUCT and ty.s_type:
${chain.vn_decode_chain_body(ty, '_partial_temp')}\
% else:
<% assert False %>
% endif
}
</%def>

<%def name="vn_replace_type_handle(ty)">\
static inline void
% if ty.is_valid_union():
vn_replace_${ty.name}_handle(${ty.name} *val, ${ty.sty.name} tag)
% else:
vn_replace_${ty.name}_handle(${ty.name} *val)
% endif
{
% if ty.category == ty.HANDLE:
${handle.vn_replace_handle_handle_body(ty)}\
% elif ty.is_valid_union():
${union.vn_replace_union_handle_body(ty)}\
% elif ty.category == ty.STRUCT and not ty.s_type:
${struct.vn_replace_struct_handle_body(ty)}\
% elif ty.category == ty.STRUCT and ty.s_type:
${chain.vn_replace_chain_handle_body(ty)}\
% else:
<% assert False %>
% endif
}
</%def>

<%def name="vn_replace_type_handle_helpers(ty)">\
% if ty.category == ty.STRUCT and ty.s_type:
${chain.vn_replace_chain_handle_self(ty)}
% endif
</%def>
