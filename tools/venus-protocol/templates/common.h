/*
 * Copyright 2020 Google LLC
 * SPDX-License-Identifier: MIT
 */

<%def name="define_typedef(ty)">\
typedef ${ty.typedef.name} ${ty.name};
</%def>

<%def name="define_extended_enum(ty)">\
/* ${ty.name} */
% for key, val in ty.enums.values.items():
%   if key not in ty.enums.vk_xml_values:
#define ${key} ((${ty.name})${val})
%   endif
% endfor
</%def>

<%def name="define_enum(ty)">\
typedef enum ${ty.name} {
% for key, val in ty.enums.values.items():
    ${key} = ${val},
% endfor
} ${ty.name};
</%def>

<%def name="define_bitmask(ty)">\
typedef VkFlags ${ty.name};
</%def>

<%def name="define_struct(ty)">\
typedef struct ${ty.name} {
    ${ty.c_func_params(';\n    ')};
} ${ty.name};
</%def>

<%def name="define_command(ty)">\
struct vn_command_${ty.name} {
    ${ty.c_func_params(';\n    ')};
% if ty.ret:

    ${ty.ret.to_c()};
% endif
};
</%def>

<%def name="define_info(wire_format_ver, vk_xml_ver, exts, max_ext_number)">\
#define VN_INFO_EXTENSION_MAX_NUMBER (${max_ext_number})

struct vn_info_extension {
   const char *name;
   uint32_t number;
   uint32_t spec_version;
};

/* sorted by extension names for bsearch */
static const uint32_t _vn_info_extension_count = ${len(exts)};
static const struct vn_info_extension _vn_info_extensions[${len(exts)}] = {
% for i, ext in enumerate(exts):
   { "${ext.name}", ${ext.number}, ${ext.version} },
% endfor
};

static inline uint32_t
vn_info_wire_format_version(void)
{
    return ${wire_format_ver};
}

static inline uint32_t
vn_info_vk_xml_version(void)
{
    return ${vk_xml_ver};
}

static inline int
vn_info_extension_compare(const void *name, const void *ext)
{
   return strcmp(name, ((const struct vn_info_extension *)ext)->name);
}

static inline int32_t
vn_info_extension_index(const char *name)
{
   const struct vn_info_extension *ext = bsearch(name, _vn_info_extensions,
      _vn_info_extension_count, sizeof(*_vn_info_extensions),
      vn_info_extension_compare);
   return ext ? ext - _vn_info_extensions : -1;
}

static inline const struct vn_info_extension *
vn_info_extension_get(int32_t index)
{
   assert(index >= 0 && (uint32_t)index < _vn_info_extension_count);
   return &_vn_info_extensions[index];
}
</%def>
