/*
 * Copyright 2020 Google LLC
 * SPDX-License-Identifier: MIT
 */

<%namespace file="/common.h" import="define_info"/>\
\
#ifndef VN_PROTOCOL_DRIVER_INFO_H
#define VN_PROTOCOL_DRIVER_INFO_H

#include "vn_protocol_driver_defines.h"

${define_info(WIRE_FORMAT_VERSION, VK_XML_VERSION, EXTENSIONS, MAX_EXTENSION_NUMBER)}
\
static inline bool
vn_info_extension_mask_test(const uint32_t *mask, uint32_t ext_number)
{
   return mask[ext_number / 32] & (1 << (ext_number % 32));
}

#endif /* VN_PROTOCOL_DRIVER_INFO_H */
