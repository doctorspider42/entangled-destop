/*
 * Copyright 2020 Google LLC
 * SPDX-License-Identifier: MIT
 */

<%def name="vn_decode_handle_lookup(ty)">\
<% assert not GEN.is_driver %>\
static inline void
vn_decode_${ty.name}_lookup(struct vn_cs_decoder *dec, ${ty.name} *val)
{
    uint64_t id;
    vn_decode_uint64_t(dec, &id);
% if ty.dispatchable:
    *val = (${ty.name})vn_cs_decoder_lookup_object(dec, id, ${ty.attrs['c_objtype']});
% else:
    *val = (${ty.name})(uintptr_t)vn_cs_decoder_lookup_object(dec, id, ${ty.attrs['c_objtype']});
% endif
}
</%def>

<%def name="vn_sizeof_handle_body(ty)">\
    return sizeof(uint64_t);
</%def>

<%def name="vn_encode_handle_body(ty)">\
    const uint64_t id = vn_cs_handle_load_id((const void **)val, ${ty.attrs['c_objtype']});
    vn_encode_uint64_t(enc, &id);
</%def>

<%def name="vn_decode_handle_body(ty, variant='')">\
    uint64_t id;
    vn_decode_uint64_t(dec, &id);
% if '_temp' in variant:
    if (vn_cs_handle_indirect_id(${ty.attrs['c_objtype']})) {
        *val = vn_cs_decoder_alloc_temp(dec, sizeof(vn_object_id));
        if (!*val)
            return;
    }
% endif
    vn_cs_handle_store_id((void **)val, id, ${ty.attrs['c_objtype']});
</%def>

<%def name="vn_replace_handle_handle_body(ty)">\
<% assert not GEN.is_driver %>\
% if ty.dispatchable:
    *val = (${ty.name})(uintptr_t)vn_cs_get_object_handle((const void **)val, ${ty.attrs['c_objtype']});
% else:
    *val = (${ty.name})vn_cs_get_object_handle((const void **)val, ${ty.attrs['c_objtype']});
% endif
</%def>
