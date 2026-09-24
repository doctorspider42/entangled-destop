/*
 * Copyright 2020 Google LLC
 * SPDX-License-Identifier: MIT
 */

<%def name="vn_sizeof_union_body(ty, variant='')">\
% if ty.is_valid_union():
    size_t size = vn_sizeof_${ty.sty.name}(&tag);
% else:
<% tag = GEN.UNION_DEFAULT_TAGS[ty.name] %>\
    static const uint32_t tag = ${tag}; /* union with default tag */
    size_t size = vn_sizeof_uint32_t(&tag);
% endif
    switch (tag) {
% for (i, var) in ty.get_union_cases():
    case ${i}:
        ${GEN.sizeof_struct_member(ty, var, 'val->', False, 'size', 2)}
        break;
% endfor
    default:
        assert(false);
        break;
    }
    return size;
</%def>

<%def name="vn_encode_union_body(ty)">\
% if ty.is_valid_union():
    vn_encode_${ty.sty.name}(enc, &tag);
% else:
<% tag = GEN.UNION_DEFAULT_TAGS[ty.name] %>\
    static const uint32_t tag = ${tag}; /* union with default tag */
    vn_encode_uint32_t(enc, &tag);
% endif
    switch (tag) {
% for (i, var) in ty.get_union_cases():
    case ${i}:
        ${GEN.encode_struct_member(ty, var, 'val->', False, 2)}
        break;
% endfor
    default:
        assert(false);
        break;
    }
</%def>

<%def name="vn_decode_union_body(ty, variant='')">\
% if ty.is_valid_union():
    ${ty.sty.name} tag;
    vn_decode_${ty.sty.name}(dec, &tag);
% else:
    uint32_t tag;
    vn_decode_uint32_t(dec, &tag);
% endif
    switch (tag) {
% for (i, var) in ty.get_union_cases():
    case ${i}:
        ${GEN.decode_struct_member(ty, var, 'val->', False, '_temp' in variant, 2)}
        break;
% endfor
    default:
        vn_cs_decoder_set_fatal(dec);
        break;
    }
</%def>

<%def name="vn_replace_union_handle_body(ty, variant='')">\
    switch (tag) {
% for (i, var) in ty.get_union_cases():
    case ${i}:
        ${GEN.replace_struct_member_handle(ty, var, 'val->', 2)}
        break;
% endfor
    default:
        assert(false);
        break;
    }
</%def>
