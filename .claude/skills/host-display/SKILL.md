---
name: host-display
description: The host presentation stack — winit window, wgpu renderer, scanout texture, scaling, window UX (grab, cursor, fullscreen, 1:1) and host-side input capture feeding virtio-input (backlog EPIC 7 + EPIC 15 + rendering side of EPIC 8/9, crate display). Load before working on the window, renderer or input capture.
---

# Host display and input capture

Scope: backlog EPIC 7 (MVP-701…708), EPIC 15 (WIN-1501…1504, window UX), the
host half of virtio-gpu (texture updates) and of virtio-input (winit events →
guest events). Crate: `crates/display`. winit/wgpu licenses are already cleared
by `cargo deny check` (both permissive; some wgpu backends pull extra deps, so
re-check after touching features).

## Existing pieces

- `display::letterbox()` computes the aspect-preserving viewport, returns
  `None` for zero-sized (minimized) windows — renderer skips presenting then
  (MVP-705/706, tested).
- `display::ux` is the window-UX policy, all pure and unit-tested:
  `viewport_for(ScaleMode, …)` (fit vs 1:1), `cursor_visible(grabbed, over)`,
  `window_title(base, WindowStatus)`, `initial_window(w, h, monitor)` and the
  `MIN_WINDOW_WIDTH/HEIGHT` floor. `host.rs` only asks the policy and tells
  winit — put new window behaviour here, not in the event loop.
- `display::DisplayConfig` mirrors the `[display]` section of the VM config.
- `DisplayHandle` implements `virtio_gpu::ScanoutSink`, which is the whole of
  what the GPU device sees (`resolution`, `set_resolution`, `update_scanout`);
  `DisplayHandle::detached(w, h)` is the windowless version used by tests.
- Guest side constants live in `virtio-gpu` (`FORMAT_B8G8R8A8_UNORM` /
  `FORMAT_B8G8R8X8_UNORM`, both 4-byte BGRA) and
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
- `InputCapture` keeps **two** held-key sets and the distinction is load-bearing:
  `held` is what the user physically holds (maintained even while ungrabbed, so
  shortcuts stay recognisable), `guest_held` is what the guest was told is down
  (only these ever get a key-up, which is what prevents stuck modifiers).

## Window UX and the input grab (EPIC 15)

Nothing reaches the guest unless the **grab** is active — keyboard, pointer and
wheel alike. The grab starts inactive so the window behaves like any other
window on the desktop.

| Action | Result |
|---|---|
| click on the guest image | grab input, host cursor hidden over the image, guest pointer re-synced to the host position |
| click on a letterbox bar | nothing (no grab, nothing forwarded) |
| `Ctrl+Alt`, released with no other key/button pressed in between | release the grab, cursor back, held keys handed to the guest as key-ups |
| `Ctrl+Alt+G` | explicit grab toggle |
| `Ctrl+Alt+Q` | `ControlEvent::QuitRequested` — the supervisor decides |
| `Ctrl+Alt+O` | 1:1 pixel mode toggle (`ScaleMode`) |
| `F11` | borderless fullscreen toggle; reserved even without modifiers |
| `Ctrl+Alt+<anything else>` | forwarded to the guest verbatim (`Ctrl+Alt+F2` must reach Weston) |
| focus loss | grab released, every guest-held key released |
| window resize | letterbox recomputed; ≥ 640×360 enforced by winit |

Rules to keep when extending this:

- Reserved shortcuts are recognised on **press**, before the key joins `held`,
  and the modifier state comes from our own `held` set (winit's
  `ModifiersChanged` goes stale across focus changes).
- The `Ctrl+Alt` release only counts while `modifiers_clean` — set when both
  modifiers go down, cleared by any other key or button press. That single flag
  is what keeps the release gesture from swallowing `Ctrl+Alt+F2`.
- `WindowAction` (returned synchronously from `on_key`/`on_button`) is for the
  window; `ControlEvent` (queued) is for the VM supervisor. Fullscreen and
  scaling never leave the event loop.
- The host cursor is hidden only when `grabbed && pointer_over_guest`; the guest
  draws its own cursor, and over the bars the user needs the host one back.
- The title always states the input state, updated from the event loop — never
  through a channel and never per frame (`set_title` is a compositor round trip).

## Testing and diagnostics

- Keep all geometry/mapping logic as pure functions (like `letterbox`) with
  unit tests; the windowed path cannot run headless in CI.
- `entangled` screenshot support (MVP-707): copy the scanout texture to a
  buffer and encode PNG — this is also how graphical acceptance tests
  compare against golden images (see vm-testing skill).
- FPS/copy statistics behind a debug overlay or periodic `tracing` event
  (MVP-708, P1).

## Pitfalls

- Don't create the wgpu device per frame or per transfer; one device+queue
  per window, staging buffers pooled.
- A rapid resize drag delivers many `Resized` events between frames.
  `Renderer::resize` is idempotent and `draw()` re-asserts the current window
  size before presenting — keep both, or resizing shows stretched frames.
- Reserved shortcuts must also work while ungrabbed (`Ctrl+Alt+Q` on a window
  the user has not clicked into yet), which is why key tracking cannot be gated
  on the grab — only *forwarding* is.
- **winit's X11 backend `expect()`s on most requests** (`set_title` among them)
  and the release profile is `panic = "abort"`, so a broken X connection kills
  the process — and with it the guest. Talk to the window as little as possible:
  title and cursor updates are deduplicated against the applied state, and
  nothing window-related runs per frame. Do not add per-frame `set_*` calls.
- `write_texture` rows must respect 256-byte `bytes_per_row` alignment for
  buffer-to-texture copies — `write_texture` from CPU memory handles padding
  internally, buffer copies do not.
- Wayland vs X11 differences (decorations, scale factors) — trust winit,
  don't special-case; HiDPI: window inner size is physical pixels.
