/*
 * Copyright 2020 Google LLC
 * SPDX-License-Identifier: MIT
 */

<%namespace file="/common.h" import="define_info"/>\
\
#ifndef VN_PROTOCOL_RENDERER_INFO_H
#define VN_PROTOCOL_RENDERER_INFO_H

#include "vn_protocol_renderer_defines.h"

struct vn_info_extension_table {
   union {
      bool enabled[${len(EXTENSIONS)}];
      struct {
% for ext in EXTENSIONS:
         bool ${ext.name[3:]};
% endfor
      };
   };
};

${define_info(WIRE_FORMAT_VERSION, VK_XML_VERSION, EXTENSIONS, MAX_EXTENSION_NUMBER)}
\
static inline void
vn_info_extension_mask_init(uint32_t *out_mask)
{
   for (uint32_t i = 0; i < _vn_info_extension_count; i++) {
       out_mask[_vn_info_extensions[i].number / 32] |= 1 << (_vn_info_extensions[i].number % 32);
   }
}

#endif /* VN_PROTOCOL_RENDERER_INFO_H */
