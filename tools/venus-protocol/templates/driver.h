/*
 * Copyright 2020 Google LLC
 * SPDX-License-Identifier: MIT
 */

#ifndef VN_PROTOCOL_DRIVER_H
#define VN_PROTOCOL_DRIVER_H

% for filename in TEMPLATE_FILENAMES:
#include "vn_protocol_${filename}"
% endfor

#endif /* VN_PROTOCOL_DRIVER_H */
