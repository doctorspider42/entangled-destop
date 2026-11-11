# Copyright © 2026 Entangled Desktop contributors
# SPDX-License-Identifier: MIT

# entangled_d3d9.dll: the Direct3D 9 user-mode driver of a WDDM display
# adapter (Entangled Desktop ADR-0011). The DDI front (entangled_d3d9.c, which
# sees only the WDK's headers) and Nine (which sees only Mesa's Direct3D 9
# headers) are separate translation units on purpose: the two sets of headers
# declare the same types.

_d3d9umd_ddi = cc.preprocess(
  'ddi_pp.c',
  output : 'ddi.i',
)

_d3d9umd_stubs = custom_target(
  'entangled_d3d9_stubs.inc',
  input : ['gen-stubs.py', _d3d9umd_ddi],
  output : 'entangled_d3d9_stubs.inc',
  command : [prog_python, '@INPUT0@', '@INPUT1@', '@OUTPUT@'],
)

# The Nine side: Mesa's Direct3D 9 headers (inc_d3d9) and Nine's own.
_libd3d9umd_nine = static_library(
  'd3d9umd_nine',
  ['d3d9_nine.c'],
  include_directories : [
    inc_d3d9, inc_include, inc_src, inc_gallium, inc_gallium_aux, inc_gallium_drivers,
    include_directories('../../frontends/nine'),
  ],
  dependencies : [idep_nir_headers, idep_mesautil],
)

libgallium_d3d9 = shared_library(
  'entangled_d3d9',
  ['entangled_d3d9.c', _d3d9umd_stubs],
  vs_module_defs : 'entangled_d3d9.def',
  include_directories : [
    inc_include, inc_src, inc_gallium, inc_gallium_aux, inc_gallium_winsys,
    inc_gallium_winsys_sw, inc_gallium_drivers,
  ],
  link_whole : [_libd3d9umd_nine, libnine_st],
  link_with : [
    libgallium, libwswddm
  ],
  dependencies : [
    dep_ws2_32, idep_nir, driver_swrast, driver_zink, idep_xmlconfig, idep_mesautil,
    cc.find_library('gdi32'), cc.find_library('user32'),
  ],
  name_prefix: '',
  install : true,
)
