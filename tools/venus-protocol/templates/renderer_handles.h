/*
 * Copyright 2020 Google LLC
 * SPDX-License-Identifier: MIT
 */

<%namespace name="types" file="/types.h"/>\
<%namespace name="handle" file="/types_handle.h"/>\
\
#ifndef VN_PROTOCOL_RENDERER_HANDLES_H
#define VN_PROTOCOL_RENDERER_HANDLES_H

#include "vn_protocol_renderer_types.h"

% for ty in HANDLE_TYPES:
/* ${types.vn_type_descriptive_name(ty)} */

${types.vn_encode_type(ty)}
%   if ty.dispatchable:
${types.vn_decode_type_temp(ty)}
%   else:
${types.vn_decode_type(ty)}
%   endif
${handle.vn_decode_handle_lookup(ty)}
${types.vn_replace_type_handle(ty)}
% endfor
\
#endif /* VN_PROTOCOL_RENDERER_HANDLES_H */
