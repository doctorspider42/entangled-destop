/*
 * Copyright 2026 Entangled Desktop contributors
 * SPDX-License-Identifier: MIT
 */

#ifndef WDDM_SW_WINSYS_H
#define WDDM_SW_WINSYS_H

struct sw_winsys;
struct gdikmt_device;

#ifdef __cplusplus
extern "C" {
#endif

/* A software winsys whose display targets are WDDM allocations of `device`. */
struct sw_winsys *
wddm_sw_winsys_create(struct gdikmt_device *device);

struct winsys_handle;
struct pipe_resource;

/* Fills `templat` for the shared resource `whandle` names. */
bool
wddm_sw_winsys_describe(struct sw_winsys *ws, struct winsys_handle *whandle,
                        struct pipe_resource *templat);

#ifdef __cplusplus
}
#endif

#endif
