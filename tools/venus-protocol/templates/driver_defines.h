/*
 * Copyright 2020 Google LLC
 * SPDX-License-Identifier: MIT
 */

<%namespace file="/common.h" import="define_typedef, define_extended_enum, define_enum, define_bitmask, define_struct"/>\
\
#ifndef VN_PROTOCOL_DRIVER_DEFINES_H
#define VN_PROTOCOL_DRIVER_DEFINES_H

#include <stdlib.h>
#include <string.h>
#include <vulkan/vulkan.h>

#define VN_SUBMIT_LOCAL_CMD_SIZE 256

% for ty in TYPEDEF_TYPES:
${define_typedef(ty)}
% endfor
\
% for ty in ENUM_EXTENDS:
${define_extended_enum(ty)}
% endfor
\
% for ty in ENUM_TYPES:
${define_enum(ty)}
% endfor
\
% for ty in BITMASK_TYPES:
${define_bitmask(ty)}
% endfor
\
% for ty in STRUCT_TYPES:
${define_struct(ty)}
% endfor
\
#endif /* VN_PROTOCOL_DRIVER_DEFINES_H */
