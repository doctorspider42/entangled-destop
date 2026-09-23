/*
 * Copyright 2020 Google LLC
 * SPDX-License-Identifier: MIT
 */

<%namespace name="types" file="/types.h"/>\
\
#ifndef VN_PROTOCOL_DRIVER_HANDLES_H
#define VN_PROTOCOL_DRIVER_HANDLES_H

#include "vn_protocol_driver_types.h"

% for ty in HANDLE_TYPES:
/* ${types.vn_type_descriptive_name(ty)} */

${types.vn_sizeof_type(ty)}
${types.vn_encode_type(ty)}
${types.vn_decode_type(ty)}
% endfor
\
#endif /* VN_PROTOCOL_DRIVER_HANDLES_H */
