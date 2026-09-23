/*
 * Copyright 2020 Google LLC
 * SPDX-License-Identifier: MIT
 */

<%namespace name="types" file="/types.h"/>\
<%namespace name="custom" file="/types_custom.h"/>\
<%namespace name="scalar" file="/types_scalar.h"/>\
\
#ifndef VN_PROTOCOL_RENDERER_TYPES_H
#define VN_PROTOCOL_RENDERER_TYPES_H

#include "vn_protocol_renderer_defines.h"

#pragma GCC diagnostic push
#pragma GCC diagnostic ignored "-Wunused-parameter"

% for ty in EARLY_SCALAR_TYPES:
/* ${types.vn_type_descriptive_name(ty)} */

${types.vn_sizeof_type(ty)}
${types.vn_encode_type(ty)}
${types.vn_decode_type(ty)}
%   if 'need_array' in ty.attrs:
${scalar.vn_encode_scalar_array(ty)}
${scalar.vn_decode_scalar_array(ty)}
%   endif
% endfor
\
${custom.vn_custom_size_t()}
${custom.vn_custom_blob()}
${custom.vn_custom_string()}
${custom.vn_custom_array_size()}
\
% for ty in SCALAR_TYPES:
/* ${types.vn_type_descriptive_name(ty)} */

${types.vn_sizeof_type(ty)}
${types.vn_encode_type(ty)}
${types.vn_decode_type(ty)}
%   if 'need_array' in ty.attrs:
${scalar.vn_encode_scalar_array(ty)}
${scalar.vn_decode_scalar_array(ty)}
%   endif
% endfor
\
#pragma GCC diagnostic pop

#endif /* VN_PROTOCOL_RENDERER_TYPES_H */
