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

/* Whether the last map of the opened `dt` read pixels not read before. */
bool
wddm_sw_winsys_changed(struct sw_winsys *ws, struct sw_displaytarget *dt);

/* Hands `size` bytes of pixels (rows packed) to whoever opened `dt`. */
bool
wddm_sw_winsys_write(struct sw_winsys *ws, struct sw_displaytarget *dt,
                     const void *pixels, size_t size);

/* Locks of this winsys's display targets end with displaytarget_unmap. */
void
wddm_sw_winsys_lock_per_map(struct sw_winsys *ws);

#ifdef __cplusplus
}
#endif

#endif
