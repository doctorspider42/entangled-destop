/*
 * Copyright 2020 Google LLC
 * SPDX-License-Identifier: MIT
 */

<%def name="vn_sizeof_struct_body(ty, variant='')">\
<% skip_vars = 2 if '_self' in variant and ty.s_type else 0 %>\
    size_t size = 0;
% if skip_vars:
    /* skip val->{${','.join([var.name for var in ty.variables[:skip_vars]])}} */
% endif
% for var in ty.variables[skip_vars:]:
    ${GEN.sizeof_struct_member(ty, var, 'val->', '_partial' in variant, 'size')}
% endfor
    return size;
</%def>

<%def name="vn_encode_struct_body(ty, variant='')">\
<% skip_vars = 2 if '_self' in variant and ty.s_type else 0 %>\
% if skip_vars:
    /* skip val->{${','.join([var.name for var in ty.variables[:skip_vars]])}} */
% endif
% for var in ty.variables[skip_vars:]:
    ${GEN.encode_struct_member(ty, var, 'val->', '_partial' in variant)}
% endfor
</%def>

<%def name="vn_decode_struct_body(ty, variant='')">\
<% skip_vars = 2 if '_self' in variant and ty.s_type else 0 %>\
% if skip_vars:
    /* skip val->{${','.join([var.name for var in ty.variables[:skip_vars]])}} */
% endif
% for var in ty.variables[skip_vars:]:
    ${GEN.decode_struct_member(ty, var, 'val->', '_partial' in variant, '_temp' in variant)}
% endfor
</%def>

<%def name="vn_replace_struct_handle_body(ty, variant='')">\
% for var in ty.variables:
    ${GEN.replace_struct_member_handle(ty, var, 'val->')}
% endfor
</%def>
