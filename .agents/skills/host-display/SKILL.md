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
- **Back and Forward are keyboard codes, not mouse buttons** (`input::nav`).
  winit's `MouseButton::Back`/`Forward` go to the guest as
  `KEY_BACK`/`KEY_FORWARD`, not `BTN_SIDE`/`BTN_EXTRA`, and `split_batch`
  therefore routes them to the keyboard device. The reason is not about the
  buttons at all: `joydev` only leaves an absolute pointer alone if its key set
  is *exactly* the three primary mouse buttons, and a tablet it binds takes
  `/dev/input/js0` away from the gamepad. The full argument is on
  `virtio_input::config::Profile::AbsolutePointer`; the practical rule here is
  that **the pointer profile's key bitmap is not a place to add a button** D
  anything new belongs on the keyboard, in the dense `KEY_*` range it already
  advertises. (`KEY_BACK`/`KEY_FORWARD` are also what a multimedia keyboard
  sends, so browsers and desktops bind them with no configuration.)

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
- **Frame pacing is measured by the GPU device, not by this crate**
  (`virtio_gpu::pacing`, GAME-2105): the window presents asynchronously —
  `update_scanout` is a `memcpy` plus a wake, and the guest never waits for it
  — so the only place that sees every guest frame is the `RESOURCE_FLUSH` path.
  `entangled run --frame-stats <PATH>` writes mean/fps, 1 % and 0.1 % lows,
  duplicate and dropped slots, and the quiet/submit/service split, every 120
  frames. Take a before *and* an after with it, back to back, and record which
  host GL and which build profile produced them (ADR-0004's 2026-09-08
  amendment exists because an earlier measurement did not).

## Pitfalls

- **The EDID's refresh rate is a hard ceiling on the guest's frame rate, and it
  is ours to choose.** The guest's compositor phase-locks to it: with slack in
  the frame it *sleeps out the remainder* and presents on the period exactly
  (measured 16.665 ms at 60 Hz, 8.341 ms at 120 Hz, zero duplicate slots), and
  it will not present faster whatever the host can do. `[display] refresh_hz`
  (24..=240, default 60) is that number; `virtio_gpu::edid` encodes it and the
  pacing counters are defined against it. Past the deadline the guest does
  *not* halve — it stops sleeping and runs work-bound, so raising `refresh_hz`
  buys nothing for a guest that is already flat out. Check `quiet` in the frame
  stats before reaching for the knob.
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
- Wayland vs X11 differences (scale factors) — trust winit, don't
  special-case; HiDPI: window inner size is physical pixels. Decorations and
  the cursor are the exceptions — see the next section.

## Wayland/WSLg: decorations, cursor, resize (learned fixing the EPIC 15 demo bugs)

WSLg is Weston with an RDP/RAIL backend: every Wayland window becomes a real
Windows window (class `RAIL_WINDOW`) mirrored over RDP by `msrdc.exe`. Facts
that shaped the code, all verified against WSLg's protocol stream
(`WAYLAND_DEBUG=1` + a screenshot of the real header):

- **WSLg offers no server-side decorations.** Its Weston does not advertise
  `zxdg_decoration_manager_v1` (neither does GNOME), so winit must draw CSD.
  Without the `wayland-csd-adwaita` winit feature that means sctk's
  `FallbackFrame` — self-described "default ugly frame": grey square buttons
  (the close button has no ✕), no title text, and a 4 px invisible resize
  border. The display crate therefore ships `default = ["wayland-csd"]`; the
  sctk-adwaita → ab_glyph → ttf-parser tree is permissive and deny-clean, and
  winit only pulls it on Linux targets, so Windows builds are untouched.
- **A maximized Wayland window cannot be resized** — no resize edges exist, by
  design. `ux::initial_window` opens maximized whenever the guest is as big as
  the monitor (the 1920×1080 default on a 1920×1080 screen), so the *only* way
  to free the window is the CSD restore button — which the FallbackFrame drew
  as an anonymous grey square. That, plus the 4 px border afterwards, was the
  whole of "the window cannot be resized" from the demo.
- **Resizing the RAIL window from the Windows side does not work**: a
  `MoveWindow`/`SetWindowPos` on the mirrored HWND changes the local window but
  Weston never sends the client a new configure — the content desyncs and
  pointer routing breaks. Do not "fix" window size by poking the Windows HWND.
- **WSLg ignores the null cursor.** `set_cursor_visible(false)` on Wayland is
  `wl_pointer.set_cursor(nil)`; the app emits it correctly on grab, but the RDP
  side never hides the Windows arrow, which then rides on top of the guest's
  own cursor. `host.rs` hides the pointer on Wayland by setting a fully
  transparent 8×8 `CustomCursor` instead (ordinary cursor-image path, honoured
  everywhere); other platforms keep `set_cursor_visible`.
- **Named cursors can be silent no-ops.** `CursorIcon::*` needs an XCursor
  theme; a stock WSL root ships none (`/usr/share/icons/*/cursors` is empty),
  so `set_cursor(CursorIcon::Default)` sends nothing, and the CSD frame's own
  resize/arrow cursors don't work either. Consequences the code handles:
  the *visible* state is a bundled arrow image (`ARROW_PIXELS` in `host.rs`),
  set once at window creation (WSLg's RDP client keeps the last pointer
  *across windows*, so a hidden cursor from a previous run haunts new ones)
  and re-set whenever the policy flips to visible.
- **The cursor image cannot be changed while the pointer is over the CSD
  frame** (winit defers `set_cursor` until the pointer is back over the
  content, and a theme-less frame can't set its own), and Weston carries the
  current image across same-client surface crossings. So the flip back to the
  visible arrow must happen *before* the crossing:
  `ux::CURSOR_EDGE_MARGIN` keeps the outer 16 px of the window a
  "cursor visible" zone (`InputCapture::pointer_over_guest`). A fast flick can
  still skip the margin and carry the transparent cursor onto the titlebar —
  wiggling back over the image repairs it; nothing more can be done app-side.
- **WSLg's pointer confinement is a mirage**: `zwp_pointer_constraints_v1` is
  advertised and `confine_pointer` is accepted, but no `confined` event ever
  arrives — the pointer is never actually confined, so the grab cannot rely
  on it (the code already treats it as best-effort).
- **Do not SIGKILL/SIGTERM a windowed VM under WSLg if avoidable**: abruptly
  disconnecting a client that holds a pending pointer constraint has crashed
  WSLg's Weston in testing, taking every other WSLg window with it (our VM
  supervision survives this: broken pipe → event loop error → clean VM stop
  with NVRAM written). Prefer the window's close button / Ctrl+Alt+Q.
- **Input cannot be injected into WSLg windows from Windows automation**:
  `msrdc.exe` runs with UIAccess, so UIPI silently drops `SendInput` /
  `mouse_event` from normal processes. Manual testing needs a human hand (or a
  nested wlroots compositor with `zwlr_virtual_pointer`, which WSL's stock
  image does not ship). Protocol logging with `WAYLAND_DEBUG=1` is the
  reliable observability tool.
- The CSD frame is subsurfaces *around* the main surface: `GetWindowRect` on
  the RAIL window covers only the content; the header lives above it on
  screen. Coordinate math in host-side tooling must not assume the rect
  includes the titlebar.

## Lifecycle shortcuts (ADR-0005)

Two more reserved `Ctrl+Alt` chords, alongside `G` (grab), `Q` (shut down) and
`O` (1:1):

| Shortcut | `ControlEvent` | Effect |
|---|---|---|
| `Ctrl+Alt+P` | `PauseToggleRequested` | freeze the VM, or let a frozen one continue |
| `Ctrl+Alt+R` | `ResetRequested` | reboot the VM in place |

Both follow the `Ctrl+Alt+Q` pattern exactly: `release_all()` first, so a guest
that survives the request is not left holding the modifiers, then a control
event for the supervisor. Neither is a *window* state, so the window's
`WindowAction` for them is the inert `Lifecycle` — pausing is the VM
supervisor's business and the window only reports the request.

The one thing the display side has to do about a paused VM: **stop pushing
input**. `run_vm`'s pump drains the input queue and drops it while
`lifecycle.is_paused()`, because pushing an event writes the guest's event ring,
which is precisely what a pause forbids. Dropping rather than queueing is
deliberate — the window is not grabbed while frozen, and a burst delivered on
resume would be worse than nothing.
