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
  what the GPU device sees (`resolution`, `set_resolution`, `update_scanout`,
  the cursor plane, and — for zero-copy presentation — `accepts_shared_scanout`,
  `present_shared`, `forget_shared`; see the section below);
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

## Zero-copy presentation of a renderer's scanout (ADR-0004, 2026-09-25)

When the guest composites on the GPU (GNOME on Zink over Venus), its scanout
buffers are **handle blobs**: exportable device-local memory on the host GPU.
On Windows the display takes them on its own GPU instead of the CPU mirror:
`present_shared(frame, lease)` imports the renderer's duplicated NT handle once
per image (`SharedScanoutImage::serial`), and each flip copies the damage **on
the GPU** into a texture of the display's own (`gpu_scanout::GpuScanout`).
The window samples that texture, and nothing goes through the CPU. The code is
`crates/display/src/{shared,gpu_scanout,present}.rs`; the portable types are
`virtio_gpu::shared`.

- **It needs the Vulkan backend.** A DX12 device cannot import a Vulkan
  `OPAQUE_WIN32` allocation. `DisplayHost::with_shared_scanout(true)` (set by
  `entangled run` for the Venus executor on Windows) tries
  `Backends::VULKAN` first, opened through wgpu-hal's `open_with_callback`
  with `Features::VULKAN_EXTERNAL_MEMORY_WIN32` **plus**
  `VK_EXT_queue_family_foreign`: the guest releases every scanout buffer to
  `FOREIGN`, wgpu does not enable that extension, and acquiring from `FOREIGN`
  without it is invalid. Failure falls back to the old backend order, and so
  to the copy path. 2D-only VMs keep the default order. A headless VM gets an
  off-screen presenter (`DisplayHandle::attach_offscreen_gpu`) that does
  exactly what the window's does, so `--headless` runs measure the real path.
  `ENTANGLED_SCANOUT_PATH=copy` turns it all off: that is the A/B switch.
- **Never make a guest image a wgpu texture.** `create_texture_from_hal`
  starts the texture in wgpu's `UNINITIALIZED` state. wgpu's first use then
  records a barrier from `VK_IMAGE_LAYOUT_UNDEFINED`, which lets the driver
  discard the guest's pixels, on an image the guest's queue family still
  owns. That is why the display **copies** into a texture it owns rather than
  sampling the import. The raw acquire/copy/release goes into a wgpu encoder
  through `CommandEncoder::as_hal_mut`, so wgpu's submission order holds.
- **The texture's layout invariant.** The display's shared texture is cleared
  and moved to `RESOURCE` once, when it is made. wgpu only ever uses it as
  `RESOURCE` (sampling, and the screenshot's draw), and every raw copy leaves
  it in `SHADER_READ_ONLY_OPTIMAL`. So wgpu's tracker and the real layout
  never disagree. A new wgpu operation on that texture must keep this, or end
  with `transition_resources(... RESOURCE)`.
- **Never block in `device.poll(Wait)` on a hot path.** wgpu-core holds its
  fence lock across the wait, and `Queue::submit` takes it, so a blocking wait
  stalls every submit, the window's frame included. The presenter's retirer
  polls (`PollType::Poll`) every 250 µs while a copy is outstanding. The only
  blocking wait is before destroying an import, which is rare.
- **The device's worker never waits for the GPU.** `present_shared` returns
  once the copy is submitted. The `lease` (the renderer's claim on the
  payload, `executor::writes`, `Owner::Presenter`) goes to the retirer, which
  drops it once the copy has run. The synchronous first version cost 1.2–2.9 ms
  per flip under load: the copy queued behind the desktop's own GPU work at
  idle clocks.
- **The cursor plane over a shared frame is drawn by the GPU**
  (`present::CursorOverlay`, `cursor.wgsl`, premultiplied source-over). No CPU
  mirror holds the pixels under it. Over the mirror it is still composited on
  the CPU.
- **Which source is shown**: the last update decides. A mirror update or a mode
  change deactivates the shared texture. A shared present makes it current,
  and its first copy after the mirror is of the whole visible region.
- **A flush's damage is frame damage, and the texture is a per-plane target.**
  The rect is what changed since the frame shown before, whichever buffer that
  was in, and the display's texture (and the CPU mirror) holds exactly that
  frame. So a compositor flipping A, B, A needs no per-buffer bookkeeping:
  copying the damage from the buffer flushed is right. Do not "fix" this with
  per-resource tracking. In practice Linux's virtio-gpu driver sends the whole
  plane whenever the framebuffer changes (`ignore_damage_clips`, since 6.8),
  and GNOME flips every frame. The shared statistics line shows it: `mpix`
  over `presented` is exactly 1920×1080. Pinned by
  `shared::tests::a_flip_sequence_with_frame_damage_leaves_the_texture_equal_to_the_buffer_shown`.
- **A "leftover" beside a window: check the wallpaper first.** Ubuntu's logo
  is 436 px wide and the calculator 359 px, so its rays show at both edges
  (72 px on the left, 5 on the right). This was reported as stale pixels, and
  it was the live wallpaper under a correct shadow (ADR-0004, 2026-09-26). To
  tell the two apart, diff against the empty desktop from the same boot. Stale
  pixels differ from it, and the wallpaper is bit-identical outside the
  shadow. Then A/B with `ENTANGLED_SCANOUT_PATH=copy`, whose screenshot is the
  renderer's readback of the guest's own buffer.
- **Screenshots of a shared frame** draw the texture into an off-screen target
  through the window's own `ImagePipeline` (nearest, 1:1), read it back, and
  composite the mirror's cursor plane on the CPU. `DisplayHandle::screenshot_bgra`
  gives the pixels without PNG.
- **Diagnostics**: the window's `display statistics` line carries
  `shared_fps` (and `uploads_per_s` drops to 0), `guest_fps` (every flush the
  display received, shared or copied) against `fps` (what the window
  presented), `unshown_per_s` (guest frames superseded before a draw) and the
  window thread's `acquire_ms`/`present_ms` per frame. `display: shared scanout
  statistics` every 600 presents gives submit time, submit-to-retired GPU time
  (`gpu_mean_ms`), full copies, imports, refusals and abandoned copies. The
  device's pacing report carries `shared` per window.
- **virtio-gpu's own lib tests driving a `DisplayHandle`** hit the dev-dependency
  cycle: inside that test build, `crate::shared::*` is a *second* copy of the
  types `display` links. `display` re-exports its `virtio_gpu` (`#[doc(hidden)]`)
  so the real-GPU test can build the display's frame and lease
  (`pipeline_tests::for_the_display`).

## The GPU boost (ADR-0004, the GPU-boost amendment of 2026-09-26)

The guest's GPU work runs on the renderer's own Vulkan devices in bursts a
frame apart, and NVIDIA's driver reads that as a mostly idle GPU (P5/P8). The
one request an application may make for more is `VK_NV_low_latency2`'s
`vkSetLatencySleepModeNV` with `lowLatencyBoost`, and it is per swapchain:
only the window has one. On the RTX 2070 (580.88) it holds the **whole GPU**
at P0 while it is set — the renderer's devices included, and even with
nothing submitted — so the window asks for it only while the guest uses the
GPU (`display::boost`, `display::latency`):

- The shareable Vulkan device (`gpu_scanout::request_device(.., latency)`)
  enables `VK_NV_low_latency2` + `VK_KHR_present_id` when the adapter has
  both; `latency::LatencyBoost` makes the call on wgpu's own swapchain
  (`Surface::as_hal::<Vulkan>()` → `raw_swapchain()`), and the renderer
  applies it again after **every configure** — a new swapchain starts off.
- **Activity** is a flip reaching the display (`Waker::frame`) or a
  `SUBMIT_3D` reaching the device (`ScanoutSink::gpu_work`, an atomic store).
  `App::about_to_wait` asks `BoostGate::wants`; while boosting it sleeps with
  `ControlFlow::WaitUntil(end of hold)`, and while not it **arms** the
  activity so the next submit sends one `HostEvent::GpuActive` — one wakeup
  per idle-to-active transition, never one per submit. Arm, then look at the
  activity again: a submit between the two would otherwise wake no one.
- `boost::ACTIVE_HOLD` is 1 s; the driver keeps P0 about 1.8 s longer by
  itself after the request is withdrawn, and the next request reaches P0
  within one 50 ms NVML sample.
- **Never pass NULL** for `pSleepModeInfo`: the spec says it switches
  everything off, and NVIDIA 580.88 crashes inside the call. Off is an explicit
  structure with every member false.
- `[display] gpu_boost = false` or `ENTANGLED_GPU_BOOST=off` turn it off;
  `=always` sets it once and leaves it (measurement only: an idle desktop
  then sits at P0). A headless VM has no swapchain and no boost, and neither
  has the copy path's non-shareable device or a non-NVIDIA driver.
- The `display statistics` line carries `gpu_boost`, `gpu_boost_raised`,
  `gpu_boost_on_s` and `gpu_boost_failed`; `RUST_LOG=display::renderer=debug`
  logs each transition.
- The measurement tool is `F:\VMs\Entangled\probes\boost\` — `gpu-boost-probe`
  (two devices in one process, NVML sampled every 50 ms) for the host alone,
  and `runboost.sh`/`drive.ps1`/`summ.py` for guest A/B boots.
- **A flicker report: grab the screen before blaming the present mode.**
  `F:\VMs\Entangled\probes\boost\grab.ps1` grabs the VM window's client area ~20 times a
  second and logs luma, black fraction and frame-to-frame change (`fl.py`
  finds alternations). On 2026-09-26 Mailbox, Fifo, FifoRelaxed and the copy
  path all grabbed clean; the probe's strobing content (the CSS page, the
  overview toggled every 0.8 s) and a VRR panel are what remained (ADR-0004,
  the GPU-boost amendment). Keep windowed probe content calm while the user
  is at the screen.

## Input capture (host half of EPIC 9)

- Keyboard: use winit **physical keys** (scancodes), not logical keys, and
  map to Linux `KEY_*` codes — layout interpretation belongs to the guest.
  Keep the map table-driven in `crates/display`.
- Pointer: window position → `InputEvent::abs_from_window` against the
  *viewport* (subtract letterbox offsets, clamp) so the guest cursor matches
  the host cursor exactly over the image.
- Pointer rate: the host reports the mouse at its polling rate (250–1000 Hz on
  Windows), several times per guest frame. The run loop's input pump passes
  each pass's batches through `virtio_input::coalesce_motion` (a run of
  motion-only batches keeps only its last position; any key, button or wheel
  event ends the run) and pushes each device **once** per pass — one
  interrupt, not one per mouse report.
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

### Windows shell shortcuts and F11 (2026-10-25)

`windows_keyboard` installs a fresh `WH_KEYBOARD_LL` hook on its own message
thread at each new grab and drops/disarms/joins it on release or focus loss.
Do not reuse a hook left idle since window creation: Windows can silently remove
a timed-out low-level hook. A failed install shows a window notice.
It intercepts Alt+Tab/Esc/Space/F4, Ctrl+Esc and Windows-key chords only while
the input grab is active AND this VM's HWND is the foreground window. Ordinary
keys retain winit's AltGr handling. Intercepted keys are posted as normal
WM_KEYDOWN/UP to the window's own queue, preserving order with modifiers and
avoiding DefWindowProc's SYSKEY actions. `system_keys` tests ownership of key-up
after a modifier is released, repeats, focus loss and the message scan-code bits.
Ctrl+Alt remains the grab escape; Windows' secure attention sequence is outside
this hook. Drop disables capture, posts WM_QUIT, unhooks and joins the worker.

F11 preserves the guest's current display mode and only scales presentation.
Switching Ubuntu from 1920x1016 to 1920x1080 under Portal 2 desynchronised its
cursor; restoring 1920x1016 while the host stayed fullscreen aligned it again.
Ordinary window resizing still follows `follow_window`; pending resize requests
are cancelled on F11 and checked again at debounce expiry. Never reintroduce a
fullscreen mode switch without checking in-game mouse alignment.

### Relative gaming mouse (2026-10-25)

The accelerated PCI desktop includes a separate `RelativePointer` device.
`[display] relative_mouse = true` additionally selects gaming capture on startup;
absent keeps desktop tablet integration initially, with Ctrl+Alt+M switching
modes during the session. `false` omits the relative device. Legacy MMIO guests
only gain one when explicitly requested, preserving their existing slot budget.
The preference survives focus/grab loss, but actual raw capture requires both;
Ctrl+Alt always returns the pointer and keyboard to the host. Guest cursor hiding
is not a reliable request for relative input and must not automatically lock it.

Gaming uses raw `DeviceEvent::MouseMotion`, enabled with `WhenFocused` only
while active; desktop mode keeps `DeviceEvents::Never`. On Windows the real
hidden+confined winit state intentionally pins the host pointer to the client
centre in this mode ONLY. Desktop mode still hides with a transparent image.
Linux/macOS use `Locked`; refusal restores desktop mode. Window exit releases
capture and restores the host cursor before disposing the window.

Relative reports, buttons and wheels use a dedicated queue/device, never the
tablet's last-position-wins coalescer. Fractional raw counts accumulate, signed
deltas are bounded, mode/focus changes release buttons on their original device,
and a gaming capture's initial click sends no absolute recenter. The paused pump
drains both queues under the existing quiesce gate. For a borderless 16:9 game,
set 1920x1080 and `follow_window = false` before launch: F11 then fills a 1920x1080
monitor without changing the game's display mode, stretching it or cropping it.

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
| `Ctrl+Alt+V` | type the host clipboard into the guest (`virtio_input::typing`); modifiers handed back first |
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
  the intervals' standard deviation and frame-to-frame jitter, duplicate and
  dropped slots, and the quiet/submit/service split, every two seconds of the
  advertised refresh (120 frames at 60 Hz, 480 at 240). `late` is 1.2 refresh
  periods, not a fixed 20 ms, and the file names its `refresh_us`. Those
  counters are the guest's view; whether the host monitor showed a frame is
  the window's `guest_fps`/`unshown_per_s`. Take a before *and* an after with it, back to back, and record which
  host GL and which build profile produced them (ADR-0004's 2026-09-08
  amendment exists because an earlier measurement did not).

## Pitfalls

### Windows: the pointer and the redraws (found 2026-10-12)

Two bugs that only show on a Windows host, both invisible in any test that does
not move a real mouse over a grabbed window:

- **Hidden + confined pins the cursor to the window centre.** winit 0.30's
  `CursorFlags::refresh` (`platform_impl/windows/window_state.rs`) treats a
  grabbed (`Confined`) *and* hidden (`set_cursor_visible(false)`) cursor as a
  request to clip it to one pixel in the middle of the client area. The guest
  then gets the click position, then the centre (`ABS` 16384/16367), and no
  motion ever after: the guest cursor sits mid-screen and "the mouse does not
  work", while clicks and the keyboard do. `HostCursors` hides it with the
  transparent image instead, so winit's `HIDDEN` flag stays clear and the clip
  is the whole client rect. This had been broken since the grab existed.
- **`request_redraw` starves under a moving mouse.** On Windows it is
  `WM_PAINT`, synthesised only when the thread's queue is empty; a 1000 Hz
  mouse (plus the raw-input `WM_INPUT` winit registers by default) keeps it
  from being empty. Measured on the GPU desktop with a video playing: the
  window drew 30–50 times a second and dropped 20–60 guest frames a second
  (`unshown_per_s`) while the cursor moved, each draw costing well under a
  millisecond — it simply was not asked. `user_event(HostEvent::Redraw)` now
  draws directly on Windows (Linux keeps `request_redraw`, which on Wayland is
  tied to the frame callback), and `listen_device_events(DeviceEvents::Never)`
  drops raw input outside gaming capture (which enables WhenFocused only while
  active). After: 0.1 unshown frames a second
  while moving, the window drawing 100+ times a second.

Diagnose either with `RUST_LOG=entangled::run_vm=trace,virtio_gpu::device=trace`:
`pointer events to the tablet` shows what the host sent, `virtio-gpu cursor
moved` what the guest made of it; the window's own `display statistics` line
(`fps` against `guest_fps`, `unshown_per_s`, `input_batches`) shows the
redraw side. Both traces are per event — never leave them on while judging
smoothness, they cost enough to stutter by themselves.

- **The EDID's refresh rate is a hard ceiling on the guest's frame rate, and it
  is ours to choose.** The guest's compositor phase-locks to it: with slack in
  the frame it *sleeps out the remainder* and presents on the period exactly
  (measured 16.665 ms at 60 Hz, 8.341 ms at 120 Hz, zero duplicate slots), and
  it will not present faster whatever the host can do. `[display] refresh_hz`
  (24..=240) is that number; `virtio_gpu::edid` encodes it and the pacing
  counters are defined against it. Past the deadline the guest does *not*
  halve — it stops sleeping and runs work-bound, so raising `refresh_hz` buys
  nothing for a guest that is already flat out. Check `quiet` in the frame
  stats before reaching for the knob.
- **A new machine's refresh follows the host monitor, not 60** (ADR-0004, the
  high-refresh amendment): `control_api::refresh::default_refresh_hz` — the
  monitor's rate, snapped to a standard one, up to the 144 Hz the GPU desktop
  keeps up with, and above that the largest whole fraction of it (240 → 120).
  Mutter takes whatever the EDID says at every rate up to 240 (the KMS CRTC
  and `DisplayConfig.GetCurrentState` agree), but GNOME on the GPU fills only
  47–76 % of 240 Hz slots, at random — interval sd 2–2.8 ms, visible judder —
  and 97–100 % at 60, 120 and 144 (sd 0.2–1.3 ms). A guest frame rate that is
  not a whole fraction of the monitor's judders by construction (144 on 240
  shows frames for 2, 2, 1 refreshes). The window says which case it is in
  when it opens (`display::refresh::refresh_sentence`), and warns when the
  profile asks for more than the monitor shows.
- **The window never presents with `AutoVsync`** (`display::refresh`). wgpu
  resolves it to `FifoRelaxed` on NVIDIA Vulkan, which tears whenever the
  window presents slower than the monitor — always, for a guest slower than
  the host. The default is `Mailbox` (never waits, never tears), else `Fifo`;
  `ENTANGLED_PRESENT_MODE=mailbox|fifo|relaxed|immediate|auto` is the A/B
  switch. At 240 Hz on the RTX 2070 the three modes gave the same spread of
  guest rates (112–183), so the choice is correctness, not speed. A FIFO
  surface that fills makes the acquire wait while wgpu-core holds the device's
  fence lock, which every `Queue::submit` — the shared presenter's copy on the
  virtio-gpu worker included — needs; do not make the window block on vblank.
- **Measure windowed runs twice, and note what else runs.** On this desktop a
  VMware guest (25–40 % of the GPU) and PCSX2 (1–2 cores) ran through every
  2026-09-26 measurement, and 240 Hz runs fell into a slow state — GNOME at
  half the rate, every host call (the guest's `QueueSubmit`, the presenter's
  submit) twice as slow — in five of fifteen boots, windowed or not, whatever
  the present mode. The probe
  (`F:\VMs\Entangled\probes\hz\`) records the foreground process and both
  tenants' CPU per phase.
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
- **Grabbing the Windows screen from PowerShell to check what the window shows**
  needs `SetProcessDPIAware()` first. This machine's monitor is 3840×2160 at
  scale 3, and a DPI-unaware `CopyFromScreen` returns the top-left 1280×720
  physical pixels without saying so.

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
  everywhere) — and on **Windows** too, for a different reason (Pitfalls,
  "hidden + confined"); X11 and macOS keep `set_cursor_visible`.
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

## The guest's resolution follows the window (`resize.rs`, 2026-10-12)

`App` marks `resize_pending` on every `Resized`; `about_to_wait` asks once the
size has held for `resize::SETTLE` (350 ms) — a drag delivers dozens of sizes
a second, the guest must see one. `ResolutionRequest` is shared between the
window and every `DisplayHandle`: the handle reports it as
`ScanoutSink::preferred_resolution` (what `GET_DISPLAY_INFO` and the EDID
say) and holds the device's `DisplayListener`, which sets
`VIRTIO_GPU_EVENT_DISPLAY` and raises the config-change interrupt. Sizes are
physical pixels, clamped to 640×480..=4096×2304 (the scanout resource bound)
and even. `DisplayHost::with_follow_window(false)` / `[display] follow_window =
false` turns it off; `DisplayHandle::request_guest_resolution` (the control
channel's `display WxH`) is the headless way to exercise it. Note for HiDPI:
a window on a 300 % 4K monitor asks for a 4K desktop — sharp, and heavy for
the GPU desktop.

## The window's placement (`placement.rs`, 2026-10-13)

`DisplayHost::with_placement_file`: `run_vm` passes
`control_api::control::window_path` (`.window/<name>.toml` beside the
profile). `App::save_placement` runs in `exiting`. It skips a fullscreen or
minimized window, and while maximized it keeps the ordinary size saved before
it. `create_window` applies a saved placement only if `Placement::is_on` puts
its title bar on a current monitor. The minimum inner size is logical, so on
a 300 % monitor a saved 1280×720 opens at 1920×1080 physical. That is the
floor working, not a restore bug (measured: the position came back exact).

## Typing text into the guest (`virtio_input::typing`, 2026-10-13)

*Type the clipboard* (`Button::TypeClipboard`) reads the host clipboard with
`arboard` (text only, no default features; eframe already links it). It
releases the guest-held keys and queues the text on a `virtio_input::Typist`.
`run_vm::spawn_typist` paces the queue into the keyboard at about 200
characters a second, under a pause-gate pass. Three things to keep:

- **The pacing.** The device bounds pending events at 1024, and one burst of
  a long paste would drop keys.
- **The layout maps.** US by default: `key_for` covers printable ASCII,
  tab and newline. `[display] typing_layout = "pl"` (`Layout::Pl`,
  `stroke_for`) adds `ąćęłńóśźż` and the capitals as `KEY_RIGHTALT` + the
  letter (`ź` is AltGr+X, as on the layout). The modifiers go down before
  the key and come up after it, each in its own `SYN` batch. Anything else
  is skipped and counted, never guessed.
- **The caps.** `MAX_TYPED_CHARS` per request, and `MAX_QUEUED_BATCHES`
  overall, refused whole rather than half-typed.

The control channel's `paste <text>` (`
` is Enter) reaches the same queue,
which is also how a headless test verifies it. The control channel now reads
bytes and decodes them lossily: `lines()` used to end the whole channel at the
first line that was not UTF-8.

## The quick-actions bar (`toolbar.rs`, 2026-10-12)

A VMware-style bar at the top edge: Shut down (ACPI power button), Suspend,
Restart | Fullscreen, Fit/1:1, Release input | Power off. Pause has no button or
reserved keyboard shortcut; Ctrl+Alt+P belongs to the guest.

- **When it shows** is policy in `Toolbar::on_pointer`: whenever the window is
  focused, including an ungrabbed normal window, when the pointer is within
  `REVEAL_EDGE` of the top; it hides once the pointer leaves the bar, its label
  and `HIDE_MARGIN`. While the pointer is over the bar `captures_pointer()` is
  true: `host.rs` withholds the motion and the clicks from the guest and
  `sync_cursor` shows the host arrow. Relative gaming input pins the host
  pointer centrally; Ctrl+Alt releases capture so the user can reach the top
  edge. Never turn camera movement into an automatic menu trigger.
- **Restart and Power off ask twice** (`CONFIRM_WINDOW`, 3 s); the armed label
  says "CLICK AGAIN TO …" in the warning colour.
- **Drawing**: a premultiplied BGRA image rendered on the CPU whenever
  `Toolbar::serial` changes (SDF icons, a 5×7 bitmap font scaled by a whole
  factor: 2× at 100 %, 6× at 300 %) and drawn by a second
  `present::CursorOverlay` over the *whole surface* after the frame
  (`Renderer::set_toolbar`), so it ignores the letterbox. `CursorOverlay::place`
  is the generic "image at a clip rect" both the cursor plane and the bar use.
- **Reviewing it**: `ENTANGLED_TOOLBAR_PNG_DIR=<dir> cargo test -p display
  --lib dump_toolbar_pngs -- --ignored` writes every hover state, at 1× and 2×,
  over a grey background. There is no other way to look at it without a
  human: the window cannot be driven from automation on WSLg, and on Windows
  only by real mouse input.
- Actions go where the shortcuts go: lifecycle ones as `ControlEvent`s
  (`PowerButtonRequested` is new and has no shortcut), window ones applied in
  place, Release through `InputCapture::release_grab`.

## Lifecycle shortcuts (ADR-0005)

Two more reserved `Ctrl+Alt` chords, alongside `G` (grab), `Q` (shut down) and
`O` (1:1):

| Shortcut | `ControlEvent` | Effect |
|---|---|---|
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

### Windows fullscreen confinement (2026-10-24)

A grabbed pointer must be confined to the current client rectangle in desktop
coordinates. winit 0.30.13 refreshes that rectangle in `set_cursor_grab`, but
its Windows `WM_SIZE` / `WM_WINDOWPOSCHANGED` handlers do not refresh it. Keeping
only `App::grabbed` current leaves the old window's bounds active after F11:
the guest image fills the screen but the pointer cannot reach its top/bottom.
`App::sync_cursor_grab` reapplies the existing mode after viewport recomputation
on Windows and on `WindowEvent::Moved`. Do not route this through `set_grab`'s
unchanged-state early return, and do not release the logical grab or guest-held
keys just to update geometry. Keep the transparent custom cursor: using winit's
hidden flag with confinement would pin the pointer to the centre again.

Manual regression: grab the image, F11 into fullscreen, reach all four edges,
F11 back, and repeat after moving/resizing the window. `GetClipCursor` should
match `GetClientRect` translated by `ClientToScreen` while the window is focused
and grabbed. The display demo can check this without booting a guest.
