/*
 * Copyright 2020 Google LLC
 * SPDX-License-Identifier: MIT
 */

<%def name="vn_sizeof_scalar_array(ty)">\
static inline size_t
vn_sizeof_${ty.name}_array(const ${ty.name} *val, uint32_t count)
{
% if ty.category == ty.DEFAULT:
<% ty_size = GEN.PRIMITIVE_TYPES[ty.name] %>\
    assert(sizeof(*val) == ${ty_size});
    const size_t size = sizeof(*val) * count;
    assert(size >= count);
%   if ty_size >= 4:
    return size;
%   else:
    return (size + 3) & ~3;
%   endif
% elif ty.category == ty.BASETYPE:
    return vn_sizeof_${ty.typedef.name}_array(val, count);
% elif ty.category == ty.ENUM and ty.enums.bitwidth == 32:
    return vn_sizeof_int32_t_array((const int32_t *)val, count);
% elif ty.category == ty.ENUM and ty.enums.bitwidth == 64:
    return vn_sizeof_uint64_t_array((const uint64_t *)val, count);
% else:
<% assert False %>
% endif
}
</%def>

<%def name="vn_encode_scalar_array(ty)">\
static inline void
vn_encode_${ty.name}_array(struct vn_cs_encoder *enc, const ${ty.name} *val, uint32_t count)
{
% if ty.category == ty.DEFAULT:
<% ty_size = GEN.PRIMITIVE_TYPES[ty.name] %>\
    const size_t size = sizeof(*val) * count;
    assert(size >= count);
%   if ty_size >= 4:
    vn_encode(enc, size, val, size);
%   else:
    vn_encode(enc, (size + 3) & ~3, val, size);
%   endif
% elif ty.category == ty.BASETYPE:
    vn_encode_${ty.typedef.name}_array(enc, val, count);
% elif ty.category == ty.ENUM and ty.enums.bitwidth == 32:
    vn_encode_int32_t_array(enc, (const int32_t *)val, count);
% elif ty.category == ty.ENUM and ty.enums.bitwidth == 64:
    vn_encode_uint64_t_array(enc, (const uint64_t *)val, count);
% else:
<% assert False %>
% endif
}
</%def>

<%def name="vn_decode_scalar_array(ty)">\
static inline void
vn_decode_${ty.name}_array(struct vn_cs_decoder *dec, ${ty.name} *val, uint32_t count)
{
% if ty.category == ty.DEFAULT:
<% ty_size = GEN.PRIMITIVE_TYPES[ty.name] %>\
    const size_t size = sizeof(*val) * count;
    assert(size >= count);
%   if ty_size >= 4:
    vn_decode(dec, size, val, size);
%   else:
    vn_decode(dec, (size + 3) & ~3, val, size);
%   endif
% elif ty.category == ty.BASETYPE:
    vn_decode_${ty.typedef.name}_array(dec, val, count);
% elif ty.category == ty.ENUM and ty.enums.bitwidth == 32:
    vn_decode_int32_t_array(dec, (int32_t *)val, count);
% elif ty.category == ty.ENUM and ty.enums.bitwidth == 64:
    vn_decode_uint64_t_array(dec, (uint64_t *)val, count);
% else:
<% assert False %>
% endif
}
</%def>

<%def name="vn_sizeof_scalar_body(ty)">\
% if ty.category == ty.DEFAULT:
<% ty_size = GEN.PRIMITIVE_TYPES[ty.name] %>\
    assert(sizeof(*val) == ${ty_size});
    return ${ty_size if ty_size >= 4 else 4};
% elif ty.category == ty.BASETYPE:
    return vn_sizeof_${ty.typedef.name}(val);
% elif ty.category == ty.ENUM and ty.enums.bitwidth == 32:
    assert(sizeof(*val) == sizeof(int32_t));
    return vn_sizeof_int32_t((const int32_t *)val);
% elif ty.category == ty.ENUM and ty.enums.bitwidth == 64:
    assert(sizeof(*val) == sizeof(uint64_t));
    return vn_sizeof_uint64_t((const uint64_t *)val);
% endif
</%def>

<%def name="vn_encode_scalar_body(ty)">\
% if ty.category == ty.DEFAULT:
<% ty_size = GEN.PRIMITIVE_TYPES[ty.name] %>\
%   if ty_size >= 4:
    vn_encode(enc, ${ty_size}, val, sizeof(*val));
%   else:
    vn_encode(enc, 4, val, sizeof(*val));
%   endif
% elif ty.category == ty.BASETYPE:
    vn_encode_${ty.typedef.name}(enc, val);
% elif ty.category == ty.ENUM and ty.enums.bitwidth == 32:
    vn_encode_int32_t(enc, (const int32_t *)val);
% elif ty.category == ty.ENUM and ty.enums.bitwidth == 64:
    vn_encode_uint64_t(enc, (const uint64_t *)val);
% endif
</%def>

<%def name="vn_decode_scalar_body(ty)">\
% if ty.category == ty.DEFAULT:
<% ty_size = GEN.PRIMITIVE_TYPES[ty.name] %>\
%   if ty_size >= 4:
    vn_decode(dec, ${ty_size}, val, sizeof(*val));
%   else:
    vn_decode(dec, 4, val, sizeof(*val));
%   endif
% elif ty.category == ty.BASETYPE:
    vn_decode_${ty.typedef.name}(dec, val);
% elif ty.category == ty.ENUM and ty.enums.bitwidth == 32:
    vn_decode_int32_t(dec, (int32_t *)val);
% elif ty.category == ty.ENUM and ty.enums.bitwidth == 64:
    vn_decode_uint64_t(dec, (uint64_t *)val);
% endif
</%def>
