/*
 * Copyright 2020 Google LLC
 * SPDX-License-Identifier: MIT
 */

<%namespace name="struct" file="/types_struct.h"/>

<%def name="vn_sizeof_chain_pnext(ty, variant='')">\
static inline size_t
vn_sizeof_${ty.name}_pnext${variant}(const void *val)
{
<%
    next_types, skipped_types = GEN.get_chain(ty)
%>\
% if next_types:
    const VkBaseInStructure *pnext = val;
    size_t size = 0;

    while (pnext) {
        switch ((int32_t)pnext->sType) {
%   for next_ty in next_types:
<%
        ty_cond = GEN.get_type_condition(next_ty)
%>\
        case ${next_ty.s_type}:
%       if ty_cond:
            if (${ty_cond})
                break;
%       endif
            size += vn_sizeof_simple_pointer(pnext);
            size += vn_sizeof_VkStructureType(&pnext->sType);
            size += vn_sizeof_${ty.name}_pnext${variant}(((const ${next_ty.name} *)pnext)->pNext);
            size += vn_sizeof_${next_ty.name}_self${variant}((const ${next_ty.name} *)pnext);
            return size;
%   endfor
%   for skipped_ty in skipped_types:
        case ${skipped_ty.s_type}:
%   endfor
        default:
            /* ignore unknown/unsupported struct */
            break;
        }
        pnext = pnext->pNext;
    }

% else:
    /* no known/supported struct */
% endif
    return vn_sizeof_simple_pointer(NULL);
}
</%def>

<%def name="vn_encode_chain_pnext(ty, variant='')">\
static inline void
vn_encode_${ty.name}_pnext${variant}(struct vn_cs_encoder *enc, const void *val)
{
<%
    next_types, skipped_types = GEN.get_chain(ty)
%>\
% if next_types:
    const VkBaseInStructure *pnext = val;

    while (pnext) {
        switch ((int32_t)pnext->sType) {
%   for next_ty in next_types:
<%
        ty_cond = GEN.get_type_condition(next_ty)
%>\
        case ${next_ty.s_type}:
%       if ty_cond:
            if (${ty_cond})
                break;
%       endif
            vn_encode_simple_pointer(enc, pnext);
            vn_encode_VkStructureType(enc, &pnext->sType);
            vn_encode_${ty.name}_pnext${variant}(enc, ((const ${next_ty.name} *)pnext)->pNext);
            vn_encode_${next_ty.name}_self${variant}(enc, (const ${next_ty.name} *)pnext);
            return;
%   endfor
%   for skipped_ty in skipped_types:
        case ${skipped_ty.s_type}:
%   endfor
        default:
            /* ignore unknown/unsupported struct */
            break;
        }
        pnext = pnext->pNext;
    }

% else:
    /* no known/supported struct */
% endif
    vn_encode_simple_pointer(enc, NULL);
}
</%def>

<%def name="vn_decode_chain_pnext(ty)">\
static inline void
vn_decode_${ty.name}_pnext(struct vn_cs_decoder *dec, const void *val)
{
<%
    next_types, skipped_types = GEN.get_chain(ty)
%>\
% if next_types:
    VkBaseOutStructure *pnext = (VkBaseOutStructure *)val;
    VkStructureType stype;

    if (!vn_decode_simple_pointer(dec))
        return;

    vn_decode_VkStructureType(dec, &stype);
    while (true) {
        assert(pnext);
        if (pnext->sType == stype)
            break;

        pnext = pnext->pNext;
    }

    switch ((int32_t)pnext->sType) {
%   for next_ty in next_types:
    case ${next_ty.s_type}:
        vn_decode_${ty.name}_pnext(dec, ((${next_ty.name} *)pnext)->pNext);
        vn_decode_${next_ty.name}_self(dec, (${next_ty.name} *)pnext);
        break;
%   endfor
%   for skipped_ty in skipped_types:
    case ${skipped_ty.s_type}:
%   endfor
    default:
        assert(false);
        break;
    }
% else:
    /* no known/supported struct */
    if (vn_decode_simple_pointer(dec))
        assert(false);
% endif
}
</%def>

<%def name="vn_decode_chain_pnext_temp(ty, variant='')">\
static inline void *
vn_decode_${ty.name}_pnext${variant}_temp(struct vn_cs_decoder *dec)
{
<%
    next_types, skipped_types = GEN.get_chain(ty)
%>\
% if next_types:
    VkBaseOutStructure *pnext;
    VkStructureType stype;

    if (!vn_decode_simple_pointer(dec))
        return NULL;

    vn_decode_VkStructureType(dec, &stype);
    switch ((int32_t)stype) {
%   for next_ty in next_types:
    case ${next_ty.s_type}:
        pnext = vn_cs_decoder_alloc_temp(dec, sizeof(${next_ty.name}));
        if (pnext) {
            pnext->sType = stype;
            ((${next_ty.name} *)pnext)->pNext = vn_decode_${ty.name}_pnext${variant}_temp(dec);
            vn_decode_${next_ty.name}_self${variant}_temp(dec, (${next_ty.name} *)pnext);
        }
        break;
%   endfor
%   for skipped_ty in skipped_types:
    case ${skipped_ty.s_type}:
%   endfor
    default:
        /* unexpected struct */
        pnext = NULL;
        vn_cs_decoder_set_fatal(dec);
        break;
    }

    return pnext;
% else:
    /* no known/supported struct */
    if (vn_decode_simple_pointer(dec))
        vn_cs_decoder_set_fatal(dec);
    return NULL;
% endif
}
</%def>

<%def name="vn_sizeof_chain_self(ty, variant='')">\
static inline size_t
vn_sizeof_${ty.name}_self${variant}(const ${ty.name} *val)
{
${struct.vn_sizeof_struct_body(ty, '_self' + variant)}\
}
</%def>

<%def name="vn_encode_chain_self(ty, variant='')">\
static inline void
vn_encode_${ty.name}_self${variant}(struct vn_cs_encoder *enc, const ${ty.name} *val)
{
${struct.vn_encode_struct_body(ty, '_self' + variant)}\
}
</%def>

<%def name="vn_decode_chain_self(ty, variant='')">\
static inline void
vn_decode_${ty.name}_self${variant}(struct vn_cs_decoder *dec, ${ty.name} *val)
{
${struct.vn_decode_struct_body(ty, '_self' + variant)}\
}
</%def>

<%def name="vn_replace_chain_handle_self(ty)">\
static inline void
vn_replace_${ty.name}_handle_self(${ty.name} *val)
{
${struct.vn_replace_struct_handle_body(ty, '_self')}\
}
</%def>

<%def name="vn_sizeof_chain_body(ty, variant='')">\
    size_t size = 0;

    size += vn_sizeof_VkStructureType(&val->sType);
    size += vn_sizeof_${ty.name}_pnext${variant}(val->pNext);
    size += vn_sizeof_${ty.name}_self${variant}(val);

    return size;
</%def>

<%def name="vn_encode_chain_body(ty, variant='')">\
    assert(val->sType == ${ty.s_type});
    vn_encode_VkStructureType(enc, &(VkStructureType){ ${ty.s_type} });
    vn_encode_${ty.name}_pnext${variant}(enc, val->pNext);
    vn_encode_${ty.name}_self${variant}(enc, val);
</%def>

<%def name="vn_decode_chain_body(ty, variant='')">\
    VkStructureType stype;
    vn_decode_VkStructureType(dec, &stype);
% if GEN.is_driver:
    assert(stype == ${ty.s_type});
% else:
    if (stype != ${ty.s_type})
        vn_cs_decoder_set_fatal(dec);
% endif

% if '_temp' in variant:
    val->sType = stype;
    val->pNext = vn_decode_${ty.name}_pnext${variant}(dec);
% else:
    assert(val->sType == stype);
    vn_decode_${ty.name}_pnext${variant}(dec, val->pNext);
% endif
    vn_decode_${ty.name}_self${variant}(dec, val);
</%def>

<%def name="vn_replace_chain_handle_body(ty)">\
    struct VkBaseOutStructure *pnext = (struct VkBaseOutStructure *)val;
<%
    next_types, skipped_types = GEN.get_chain(ty)
%>
    do {
        switch ((int32_t)pnext->sType) {
% for next_ty in [ty] + next_types:
        case ${next_ty.s_type}:
            vn_replace_${next_ty.name}_handle_self((${next_ty.name} *)pnext);
            break;
% endfor
% for skipped_ty in skipped_types:
        case ${skipped_ty.s_type}:
% endfor
        default:
            /* ignore unknown/unsupported struct */
            break;
        }
        pnext = pnext->pNext;
    } while (pnext);
</%def>
