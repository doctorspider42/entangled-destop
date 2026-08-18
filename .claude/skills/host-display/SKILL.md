---
name: host-display
description: The host presentation stack — winit window, wgpu renderer, scanout texture, scaling, and host-side input capture feeding virtio-input (backlog EPIC 7 + rendering side of EPIC 8/9, crate display). Load before working on the window, renderer or input capture.
---

# Host display and input capture

Scope: backlog EPIC 7 (MVP-701…708), the host half of virtio-gpu (texture
updates) and of virtio-input (winit events → guest events). Crate:
`crates/display`. winit/wgpu are added to this crate when the first window
lands — check licenses with `cargo deny check` after adding (both are
permissive; some wgpu backends pull extra deps).

## Existing pieces

- `display::letterbox()` computes the aspect-preserving viewport, returns
  `None` for zero-sized (minimized) windows — renderer skips presenting then
  (MVP-705/706 groundwork, tested).
- `display::DisplayConfig` mirrors the `[display]` section of the VM config.
- Guest side constants live in `virtio-gpu` (`FORMAT_B8G8R8A8_UNORM`) and
  `virtio-input` (`InputEvent::abs_from_window` for pointer scaling).

## Architecture

- winit event loop runs on the **main thread** (hard requirement on macOS,
  good hygiene elsewhere); vCPU and device threads communicate with it via
  channels + `EventLoopProxy` wakeups. Never block the event loop on guest
  state.
- One scanout = one `wgpu::Texture` (format `Bgra8Unorm`, matching the only
  guest format). virtio-gpu `TRANSFER_TO_HOST_2D` writes into a staging
  buffer; `RESOURCE_FLUSH` triggers `queue.write_texture` for the dirty rect
  only (MVP-704/810) and requests a redraw.
- Present: a fullscreen-triangle pipeline sampling the scanout texture into
  the letterboxed viewport; linear filtering when scaled.
- Surface loss/resize (MVP-706): reconfigure the surface on
  `SurfaceError::Lost/Outdated`; on resize recompute `letterbox` — guest
  resolution does not change in MVP (resize-triggered guest mode change is
  MVP-813, P1).

## Input capture (host half of EPIC 9)

- Keyboard: use winit **physical keys** (scancodes), not logical keys, and
  map to Linux `KEY_*` codes — layout interpretation belongs to the guest.
  Keep the map table-driven in `crates/display`.
- Pointer: window position → `InputEvent::abs_from_window` against the
  *viewport* (subtract letterbox offsets, clamp) so the guest cursor matches
  the host cursor exactly over the image.
- Focus loss: emit key-up for every pressed key (track a pressed-set) —
  acceptance criterion MVP-906.
- Reserved shortcuts, never forwarded to the guest: `Ctrl+Alt+G` toggle
  grab, `Ctrl+Alt+Q` request VM shutdown (MVP-907).

## Testing and diagnostics

- Keep all geometry/mapping logic as pure functions (like `letterbox`) with
  unit tests; the windowed path cannot run headless in CI.
- `vmhost` screenshot support (MVP-707): copy the scanout texture to a
  buffer and encode PNG — this is also how graphical acceptance tests
  compare against golden images (see vm-testing skill).
- FPS/copy statistics behind a debug overlay or periodic `tracing` event
  (MVP-708, P1).

## Pitfalls

- Don't create the wgpu device per frame or per transfer; one device+queue
  per window, staging buffers pooled.
- `write_texture` rows must respect 256-byte `bytes_per_row` alignment for
  buffer-to-texture copies — `write_texture` from CPU memory handles padding
  internally, buffer copies do not.
- Wayland vs X11 differences (decorations, scale factors) — trust winit,
  don't special-case; HiDPI: window inner size is physical pixels.
