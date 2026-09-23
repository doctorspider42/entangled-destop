/*
 * Copyright 2022 Google LLC
 * Copyright 2022 Collabora Ltd.
 * SPDX-License-Identifier: MIT
 */

#ifndef VN_PROTOCOL_RENDERER_UTIL_H
#define VN_PROTOCOL_RENDERER_UTIL_H

#include "vn_protocol_renderer_info.h"

struct vn_global_proc_table {
% for ty, _, _ in GLOBAL_COMMANDS:
   PFN_${ty.name} ${ty.name[2:]};
% endfor
};

struct vn_instance_proc_table {
% for ty, _, _ in INSTANCE_COMMANDS:
   PFN_${ty.name} ${ty.name[2:]};
% endfor
};

struct vn_physical_device_proc_table {
% for ty, _, _ in PHYSICAL_DEVICE_COMMANDS:
   PFN_${ty.name} ${ty.name[2:]};
% endfor
};

struct vn_device_proc_table {
% for ty, _, _ in DEVICE_COMMANDS:
   PFN_${ty.name} ${ty.name[2:]};
% endfor
};

static inline void
vn_util_init_global_proc_table(PFN_vkGetInstanceProcAddr get_proc_addr,
                               struct vn_global_proc_table *proc_table)
{
#define VN_GIPA(cmd) (PFN_ ## cmd)get_proc_addr(VK_NULL_HANDLE, #cmd)
% for ty, _, _ in GLOBAL_COMMANDS:
   proc_table->${ty.name[2:]} = VN_GIPA(${ty.name});
% endfor
#undef VN_GIPA
}

static inline void
vn_util_init_instance_proc_table(VkInstance instance,
                                 PFN_vkGetInstanceProcAddr get_proc_addr,
                                 struct vn_instance_proc_table *proc_table)
{
#define VN_GIPA(instance, cmd) (PFN_ ## cmd)get_proc_addr(instance, #cmd)
% for ty, _, _ in INSTANCE_COMMANDS:
   proc_table->${ty.name[2:]} = VN_GIPA(instance, ${ty.name});
%   for alias in ty.aliases:
   if (!proc_table->${ty.name[2:]})
      proc_table->${ty.name[2:]} = VN_GIPA(instance, ${alias});
%   endfor
% endfor
#undef VN_GIPA
}

static inline void
vn_util_init_physical_device_proc_table(VkInstance instance,
                                        PFN_vkGetInstanceProcAddr get_proc_addr,
                                        struct vn_physical_device_proc_table *proc_table)
{
#define VN_GIPA(instance, cmd) (PFN_ ## cmd)get_proc_addr(instance, #cmd)
% for ty, _, _ in PHYSICAL_DEVICE_COMMANDS:
   proc_table->${ty.name[2:]} = VN_GIPA(instance, ${ty.name});
%   for alias in ty.aliases:
   if (!proc_table->${ty.name[2:]})
      proc_table->${ty.name[2:]} = VN_GIPA(instance, ${alias});
%   endfor
% endfor
#undef VN_GIPA
}

<%def name="feature_api_version(feat)">VK_API_VERSION_${feat.number.replace('.', '_')}</%def>
static inline void
vn_util_init_device_proc_table(VkDevice dev,
                               PFN_vkGetDeviceProcAddr get_proc_addr,
                               uint32_t api_version,
                               const struct vn_info_extension_table *ext_table,
                               struct vn_device_proc_table *proc_table)
{
#define VN_GDPA(dev, cmd) (PFN_ ## cmd)get_proc_addr(dev, #cmd)
% for ty, feat, exts in DEVICE_COMMANDS:
%   if feat and feat.number == '1.0':
   proc_table->${ty.name[2:]} = VN_GDPA(dev, ${ty.name});
%   else:
   proc_table->${ty.name[2:]} =
%     if feat:
      api_version >= ${feature_api_version(feat)} ? VN_GDPA(dev, ${ty.name}) :
%     endif
%     for ext in exts:
      ext_table->${ext.name[3:]} ? VN_GDPA(dev, ${ty.ext_aliases[ext.name]}) :
%     endfor
      NULL;
%   endif
% endfor
#undef VN_GDPA
}

#endif /* VN_PROTOCOL_RENDERER_UTIL_H */
