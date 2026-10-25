//! The window and its event loop (backlog MVP-701, 705, 706).
//!
//! The event loop owns the window, the renderer and the input capture, and runs
//! on the thread that called [`DisplayHost::run`] — which must be the main
//! thread (hard requirement on macOS, good hygiene everywhere). Device threads
//! never touch this state; they go through [`DisplayHandle`] and
//! [`InputQueue`].

use std::sync::{Arc, Mutex};
use std::time::Instant;

use winit::application::ApplicationHandler;
use winit::event::{StartCause, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::monitor::MonitorHandle;
#[cfg(any(target_os = "linux", windows))]
use winit::window::CustomCursor;
use winit::window::{Cursor, CursorGrabMode, Fullscreen, Window, WindowId};

use crate::boost::{BoostGate, BoostPolicy};
use crate::handle::{HostEvent, Waker};
use crate::input::{
    ControlEvent, ControlQueue, InputCapture, InputQueue, KeyOutcome, WindowAction,
};
use crate::renderer::{FrameStats, Renderer, StatsReporter};
use crate::scanout::{lock_scanout, Scanout, SharedScanout};
use crate::shared::SharedSlot;
use crate::ux::{self, ScaleMode, WindowStatus};
use crate::{DisplayConfig, DisplayError, DisplayHandle, Viewport};

/// One window presenting one guest scanout.
///
/// ```no_run
/// # fn main() -> Result<(), display::DisplayError> {
/// let host = display::DisplayHost::new(display::DisplayConfig::default())?;
/// let gpu_side = host.handle();          // hand to the virtio-gpu device
/// let input_side = host.input_queue();   // hand to the virtio-input devices
/// let control = host.control_queue();    // Ctrl+Alt+G / Ctrl+Alt+Q land here
/// std::thread::spawn(move || {
///     let _ = gpu_side.update_scanout(0, 0, 1, 1, &[0, 0, 0, 0xff]);
///     let _ = (input_side.drain(), control.drain());
/// });
/// host.run() // blocks on the main thread until the window closes
/// # }
/// ```
pub struct DisplayHost {
    config: DisplayConfig,
    title: String,
    scanout: SharedScanout,
    events: InputQueue,
    control: ControlQueue,
    waker: Waker,
    event_loop: EventLoop<HostEvent>,
    /// Where the window's shared presenter goes (ADR-0004, zero-copy
    /// presentation), shared with every [`DisplayHandle`].
    shared: SharedSlot,
    /// Whether the window should open a GPU that can present a renderer's
    /// scanout itself ([`Self::with_shared_scanout`]).
    share: bool,
    /// The refresh rate the guest is told (`[display] refresh_hz`), to say
    /// how it sits against the window's monitor ([`Self::with_guest_refresh`]).
    guest_refresh_hz: Option<u32>,
    /// When the window asks the GPU driver for its clocks
    /// ([`Self::with_gpu_boost`]).
    boost: BoostPolicy,
    /// The resolution the window asks the guest for, shared with every
    /// [`DisplayHandle`] ([`crate::resize`]).
    resolution: crate::resize::ResolutionRequest,
    /// Whether the window asks at all ([`Self::with_follow_window`]).
    follow_window: bool,
    /// Where "type the clipboard" sends its text ([`Self::with_typist`]).
    typist: Option<virtio_input::Typist>,
    /// Where the window's geometry is kept between runs
    /// ([`Self::with_placement_file`]).
    placement_file: Option<std::path::PathBuf>,
}

/// Where the toolbar's screenshots go: `Pictures\Entangled` in the user's
/// profile on Windows, `$XDG_PICTURES_DIR/Entangled` or `~/Pictures/Entangled`
/// elsewhere.
fn screenshot_dir() -> Option<std::path::PathBuf> {
    let pictures = if cfg!(windows) {
        std::env::var_os("USERPROFILE").map(|home| std::path::PathBuf::from(home).join("Pictures"))
    } else {
        std::env::var_os("XDG_PICTURES_DIR")
            .map(std::path::PathBuf::from)
            .or_else(|| {
                std::env::var_os("HOME").map(|home| std::path::PathBuf::from(home).join("Pictures"))
            })
    }?;
    Some(pictures.join("Entangled"))
}

/// `20261013-011502`: a UTC timestamp for a file name, from the Unix time.
fn file_timestamp(now: std::time::SystemTime) -> String {
    let secs = now
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let (days, rem) = (secs / 86_400, secs % 86_400);
    // Civil date from days since 1970-01-01 (Howard Hinnant's algorithm).
    let z = days as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}{month:02}{day:02}-{:02}{:02}{:02}",
        rem / 3600,
        (rem / 60) % 60,
        rem % 60
    )
}

impl DisplayHost {
    /// Whether the guest's resolution follows the ordinary window ([`crate::resize`]);
    /// on unless the profile says `follow_window = false`.
    /// F11 preserves the current guest mode and scales its presentation.
    #[must_use]
    pub fn with_follow_window(mut self, follow: bool) -> Self {
        self.follow_window = follow;
        self
    }

    /// Where the toolbar's "type the clipboard" sends its text.
    pub fn with_typist(mut self, typist: virtio_input::Typist) -> Self {
        self.typist = Some(typist);
        self
    }

    /// Opens the window where it was last closed, and remembers where it is
    /// closed this time, in `path` ([`crate::placement`]).
    pub fn with_placement_file(mut self, path: std::path::PathBuf) -> Self {
        self.placement_file = Some(path);
        self
    }
    /// Builds the event loop and allocates the scanout mirror. Call on the main
    /// thread; no window or GPU resource exists until [`DisplayHost::run`].
    pub fn new(config: DisplayConfig) -> Result<Self, DisplayError> {
        config.validate()?;
        let event_loop = EventLoop::<HostEvent>::with_user_event().build()?;
        let waker = Waker::new(event_loop.create_proxy());
        let scanout: SharedScanout =
            Arc::new(Mutex::new(Scanout::new(config.width, config.height)?));
        Ok(Self {
            config,
            title: "Entangled Desktop".to_owned(),
            scanout,
            events: InputQueue::new(),
            control: ControlQueue::new(),
            waker,
            event_loop,
            shared: SharedSlot::default(),
            share: false,
            guest_refresh_hz: None,
            boost: BoostPolicy::Off,
            resolution: crate::resize::ResolutionRequest::default(),
            follow_window: true,
            typist: None,
            placement_file: None,
        })
    }

    /// Tells the window the refresh rate the guest's EDID advertises, so it
    /// can say — once, when it opens — what that rate looks like on the
    /// monitor it opened on (ADR-0004, the high-refresh amendment): a warning
    /// when the guest composites more frames than the monitor can show, a
    /// note on the cadence otherwise ([`crate::refresh::refresh_sentence`]).
    #[must_use]
    pub fn with_guest_refresh(mut self, refresh_hz: u32) -> Self {
        self.guest_refresh_hz = Some(refresh_hz);
        self
    }

    /// Asks the window for **shared presentation** (ADR-0004, zero-copy
    /// presentation of the GPU-composited desktop): a GPU of the display's own
    /// that imports a renderer's scanout buffers and copies each flipped frame
    /// on the GPU, instead of the renderer reading it back and the window
    /// uploading it. It needs wgpu's Vulkan backend, which the window then
    /// tries first — on a VM whose renderer can hand its scanout over (the
    /// Venus renderer on Windows); a 2D-only VM keeps the default backend
    /// order. If the Vulkan device cannot be had, or cannot import, the window
    /// opens as it always has and every frame takes the copy path.
    #[must_use]
    pub fn with_shared_scanout(mut self, share: bool) -> Self {
        self.share = share;
        self
    }

    /// [`Self::with_shared_scanout`] in place, for a caller that learns only
    /// after building the host whether its renderer can share.
    pub fn set_shared_scanout(&mut self, share: bool) {
        self.share = share;
    }

    /// When the window asks the host GPU driver for its clocks
    /// ([`crate::boost`], ADR-0004, the GPU-boost amendment): while the guest
    /// uses the GPU, always, or never (the default of a host built without
    /// this). It needs the shareable Vulkan device of
    /// [`Self::with_shared_scanout`] and a driver with `VK_NV_low_latency2`;
    /// anywhere else it is a no-op. [`crate::boost::GPU_BOOST_ENV`] overrides
    /// it.
    #[must_use]
    pub fn with_gpu_boost(mut self, policy: BoostPolicy) -> Self {
        self.set_gpu_boost(policy);
        self
    }

    /// [`Self::with_gpu_boost`] in place.
    pub fn set_gpu_boost(&mut self, policy: BoostPolicy) {
        self.boost = policy;
    }

    /// Overrides the window title (usually `entangled: <vm id>`).
    #[must_use]
    pub fn with_title(mut self, title: impl Into<String>) -> Self {
        self.title = title.into();
        self
    }

    /// The device-facing scanout API. Clone freely; safe to use from any thread.
    pub fn handle(&self) -> DisplayHandle {
        DisplayHandle::new(
            Arc::clone(&self.scanout),
            self.waker.clone(),
            self.shared.clone(),
            self.resolution.clone(),
        )
    }

    /// The captured guest input stream, for the `virtio-input` devices.
    pub fn input_queue(&self) -> InputQueue {
        self.events.clone()
    }

    /// Host control events (grab toggle, quit request, window close).
    pub fn control_queue(&self) -> ControlQueue {
        self.control.clone()
    }

    /// The configuration this host was built with.
    pub fn config(&self) -> DisplayConfig {
        self.config
    }

    /// Runs the event loop until the window closes, [`DisplayHandle::shutdown`]
    /// is called, or presenting fails fatally. Blocks the calling thread.
    pub fn run(self) -> Result<(), DisplayError> {
        let Self {
            config,
            title,
            scanout,
            events,
            control,
            waker,
            event_loop,
            shared,
            share,
            guest_refresh_hz,
            boost,
            resolution,
            follow_window,
            typist,
            placement_file,
        } = self;
        let shot = DisplayHandle::new(
            Arc::clone(&scanout),
            waker.clone(),
            shared.clone(),
            resolution.clone(),
        );
        let mut app = App {
            config,
            title,
            scanout,
            shared,
            share,
            capture: InputCapture::new(events, control.clone()),
            control,
            waker,
            window: None,
            renderer: None,
            guest_size: (config.width, config.height),
            viewport: None,
            occluded: false,
            grabbed: false,
            cursor_visible: true,
            cursors: None,
            #[cfg(windows)]
            keyboard_hook: None,
            applied_title: String::new(),
            fullscreen: false,
            mode: ScaleMode::default(),
            reporter: StatsReporter::default(),
            fatal: None,
            guest_refresh_hz,
            seen_frames: 0,
            boost: BoostGate::new(boost.with_env()),
            toolbar: crate::toolbar::Toolbar::new((config.width, config.height), 1.0),
            toolbar_press: false,
            resolution,
            follow_window,
            resize_pending: None,
            typist,
            shot,
            note: None,
            placement_file,
        };
        // Nothing here reads `DeviceEvent`s (pointer input is the window's
        // `CursorMoved`), and on Windows winit otherwise registers raw input for
        // every mouse: a 1000 Hz mouse then posts a `WM_INPUT` per report into
        // the queue the redraws have to get through (see `user_event`).
        event_loop.listen_device_events(winit::event_loop::DeviceEvents::Never);
        event_loop.run_app(&mut app)?;
        match app.fatal.take() {
            Some(err) => Err(err),
            None => Ok(()),
        }
    }
}

struct App {
    config: DisplayConfig,
    title: String,
    scanout: SharedScanout,
    /// The shared presenter's slot (ADR-0004, zero-copy presentation).
    shared: SharedSlot,
    /// Whether to open a GPU for it.
    share: bool,
    capture: InputCapture,
    control: ControlQueue,
    waker: Waker,
    window: Option<Arc<Window>>,
    renderer: Option<Renderer>,
    /// Cached guest resolution, refreshed each frame; used by pointer mapping so
    /// a cursor move never has to take the scanout lock.
    guest_size: (u32, u32),
    viewport: Option<Viewport>,
    occluded: bool,
    /// Grab state actually applied to the window, so a repeated request (focus
    /// loss while already ungrabbed) is a no-op.
    grabbed: bool,
    #[cfg(windows)]
    keyboard_hook: Option<crate::windows_keyboard::KeyboardHook>,
    /// Cursor visibility actually applied to the window (WIN-1501).
    cursor_visible: bool,
    /// The two cursors used on Wayland and Windows instead of
    /// `set_cursor_visible`, created once with the window; `None` on X11 and
    /// macOS, where the flag does what it says.
    ///
    /// **Windows** cannot use `set_cursor_visible(false)` while the pointer is
    /// grabbed: winit 0.30 treats "hidden + confined" as a request to pin the
    /// cursor to one pixel in the middle of the client area
    /// (`platform_impl/windows/window_state.rs`, `CursorFlags::refresh`), so
    /// `CursorMoved` stops after one event at the window centre and the guest's
    /// absolute pointer sits there forever — clicks arrive, motion never does
    /// (found 2026-10-12: the tablet was sent the click position, then
    /// 16384/16367, then only buttons). A transparent *image* leaves winit's
    /// `HIDDEN` flag clear, the confinement is the whole client rect, and every
    /// motion reaches the guest. Shown again, Windows' own arrow is fine.
    ///
    /// Hiding the cursor there cannot use `set_cursor_visible(false)`: winit
    /// maps it to `wl_pointer.set_cursor(nil)`, and WSLg's RDP-backed Weston
    /// does not forward the null cursor to the Windows side — the host arrow
    /// keeps hovering over the guest's own pointer (the EPIC 15 demo bug).
    /// Showing a real cursor whose every pixel is transparent takes the
    /// ordinary cursor-image path, which every compositor honours.
    ///
    /// Showing again cannot use `CursorIcon::Default` either: named cursors
    /// come from an XCursor theme, a stock WSL root has none installed, and a
    /// theme that fails to load makes the request a silent no-op — the
    /// transparent image then *sticks* to the pointer, and carrying it onto
    /// the window's own CSD frame (whose themed cursors fail the same way)
    /// leaves the user without a cursor over the decorations. So "visible"
    /// is a bundled arrow image, which takes the same always-honoured path.
    cursors: Option<HostCursors>,
    /// The quick-actions bar at the top edge ([`crate::toolbar`]).
    toolbar: crate::toolbar::Toolbar,
    /// A button press began on the toolbar, so its release is the toolbar's
    /// too and must not reach the guest.
    toolbar_press: bool,
    /// What the window asks the guest's resolution to be ([`crate::resize`]).
    resolution: crate::resize::ResolutionRequest,
    follow_window: bool,
    /// When the window last changed size, until the guest has been asked for
    /// the new one ([`crate::resize::SETTLE`]).
    resize_pending: Option<Instant>,
    typist: Option<virtio_input::Typist>,
    /// A handle on this window's own scanout, for the toolbar's screenshot.
    shot: DisplayHandle,
    /// A short note in the title ("screenshot saved") and when it expires.
    note: Option<(String, Instant)>,
    placement_file: Option<std::path::PathBuf>,
    /// Title text actually applied to the window.
    ///
    /// Every `set_title` is an X11/Wayland round trip, and winit's X11 backend
    /// `expect()`s on it — with `panic = "abort"` in the release profile a
    /// hiccup on that connection would take the *guest* down with the window.
    /// So the title is only ever pushed when the text really changed.
    applied_title: String,
    /// Borderless-fullscreen state (WIN-1504).
    fullscreen: bool,
    /// Fit or 1:1 (WIN-1503/1504).
    mode: ScaleMode,
    reporter: StatsReporter,
    fatal: Option<DisplayError>,
    /// The guest's advertised refresh, for the one sentence at window
    /// creation.
    guest_refresh_hz: Option<u32>,
    /// Guest frames already accounted for ([`Waker::frames`]).
    seen_frames: u64,
    /// When to ask the GPU driver for its clocks ([`crate::boost`]).
    boost: BoostGate,
}

impl App {
    /// Records a fatal error and asks the loop to exit; the error surfaces from
    /// [`DisplayHost::run`].
    fn fail(&mut self, event_loop: &ActiveEventLoop, err: DisplayError) {
        tracing::error!(%err, "display host failed");
        if self.fatal.is_none() {
            self.fatal = Some(err);
        }
        event_loop.exit();
    }

    fn create_window(&mut self, event_loop: &ActiveEventLoop) -> Result<(), DisplayError> {
        let (width, height) = self.config.initial_window_size();
        let monitor = monitor_logical_size(
            event_loop
                .primary_monitor()
                .or_else(|| event_loop.available_monitors().next()),
        );
        let initial = ux::initial_window(width, height, monitor);
        tracing::debug!(
            requested = format_args!("{width}x{height}"),
            monitor = ?monitor,
            opening = format_args!("{}x{}", initial.width, initial.height),
            maximized = initial.maximized,
            "choosing the initial window geometry"
        );
        // WIN-1503: free manual resizing, with a floor that keeps the letterboxed
        // image usable and a maximized start when the guest is as big as the
        // screen.
        self.applied_title = self.window_title();
        let mut attributes = Window::default_attributes()
            .with_title(self.applied_title.clone())
            .with_inner_size(winit::dpi::LogicalSize::new(initial.width, initial.height))
            .with_min_inner_size(winit::dpi::LogicalSize::new(
                ux::MIN_WINDOW_WIDTH,
                ux::MIN_WINDOW_HEIGHT,
            ))
            .with_resizable(true)
            .with_maximized(initial.maximized);
        // Where it was last closed, if that is still on a monitor.
        let monitors: Vec<(i32, i32, u32, u32)> = event_loop
            .available_monitors()
            .map(|m| {
                let (p, s) = (m.position(), m.size());
                (p.x, p.y, s.width, s.height)
            })
            .collect();
        if let Some(saved) = self
            .placement_file
            .as_deref()
            .and_then(crate::placement::Placement::load)
            .filter(|p| p.is_on(&monitors))
        {
            tracing::debug!(?saved, "opening the window where it was last closed");
            attributes = attributes
                .with_position(winit::dpi::PhysicalPosition::new(saved.x, saved.y))
                .with_inner_size(winit::dpi::PhysicalSize::new(saved.width, saved.height))
                .with_maximized(saved.maximized);
        }
        let window = Arc::new(event_loop.create_window(attributes)?);
        #[cfg(windows)]
        match crate::windows_keyboard::KeyboardHook::new(&window) {
            Ok(hook) => self.keyboard_hook = Some(hook),
            Err(error) => tracing::warn!(%error, "cannot capture Windows system shortcuts"),
        }
        self.cursors = HostCursors::new(event_loop);
        if let Some(cursors) = &self.cursors {
            // Establish a known pointer image right away: WSLg's RDP client
            // keeps the last pointer *across windows*, so without this a
            // hidden cursor from a previous run haunts the fresh window until
            // something else sets one.
            window.set_cursor(cursors.shown.clone());
        }
        let size = window.inner_size();
        let monitor_mhz = window
            .current_monitor()
            .or_else(|| event_loop.primary_monitor())
            .and_then(|m| m.refresh_rate_millihertz());
        tracing::info!(
            width = size.width,
            height = size.height,
            scale_factor = window.scale_factor(),
            monitor_hz = monitor_mhz.map(|m| f64::from(m) / 1000.0),
            "window created"
        );
        if let Some(guest_hz) = self.guest_refresh_hz {
            match crate::refresh::refresh_sentence(guest_hz, monitor_mhz) {
                Some((true, sentence)) => tracing::warn!("{sentence}"),
                Some((false, sentence)) => tracing::info!("{sentence}"),
                None => {}
            }
        }
        let renderer = Renderer::new(
            Arc::clone(&window),
            self.guest_size.0,
            self.guest_size.1,
            self.share.then_some(&self.shared),
            self.boost.policy(),
        )?;
        self.window = Some(window);
        self.renderer = Some(renderer);
        self.recompute_viewport();
        Ok(())
    }

    /// The title the window should currently carry (WIN-1502): the VM's own
    /// title plus what the user needs to know about the input state.
    fn window_title(&self) -> String {
        let title = ux::window_title(
            &self.title,
            WindowStatus {
                grabbed: self.grabbed,
                mode: self.mode,
            },
        );
        match &self.note {
            Some((note, _)) => format!("{title} — {note}"),
            None => title,
        }
    }

    /// Refreshes the title in place. Called only when the state behind it
    /// changes, never per frame, and a no-op when the text is already there.
    fn update_title(&mut self) {
        let title = self.window_title();
        if title == self.applied_title {
            return;
        }
        if let Some(window) = self.window.as_ref() {
            window.set_title(&title);
        }
        self.applied_title = title;
    }

    /// Physical window size, or `None` before the window exists.
    fn window_size(&self) -> Option<(u32, u32)> {
        let size = self.window.as_ref()?.inner_size();
        Some((size.width, size.height))
    }

    /// Recomputes the viewport for the current window size and scale mode, and
    /// tells the input capture about it — pointer mapping and the cursor policy
    /// must not lag a resize by one motion event.
    fn recompute_viewport(&mut self) {
        let window_size = self.window_size();
        if let (Some(size), Some(window)) = (window_size, self.window.as_ref()) {
            self.toolbar.set_window(size, window.scale_factor());
        }
        self.viewport = match window_size {
            Some((w, h)) => ux::viewport_for(self.mode, self.guest_size.0, self.guest_size.1, w, h),
            None => None,
        };
        self.capture.set_window_size(window_size);
        self.capture.set_viewport(self.viewport);
        self.sync_cursor();
        // Windows confinement is a screen-space rectangle. winit 0.30 does
        // not refresh it on resize, so F11 would retain the old client area.
        #[cfg(windows)]
        self.sync_cursor_grab();
    }

    /// One redraw, and the guest frames it accounts for: every frame
    /// published since the last draw but the newest was never drawn, and the
    /// newest neither if nothing was presented ([`FrameStats::unshown`]).
    fn draw(&mut self, event_loop: &ActiveEventLoop) {
        let published = self.waker.frames();
        let arrived = published.saturating_sub(self.seen_frames);
        self.seen_frames = published;
        let before = self.frame_stats().frames;
        self.draw_frame(event_loop);
        let presented = self.frame_stats().frames > before;
        if let Some(renderer) = self.renderer.as_mut() {
            renderer.note_guest_frames(arrived, presented);
        }
    }

    /// Uploads the dirty rect and presents (backlog MVP-704/705/706).
    fn draw_frame(&mut self, event_loop: &ActiveEventLoop) {
        self.waker.clear();
        if self.renderer.is_none() {
            return;
        }
        // WIN-1503: during a rapid drag winit can deliver several `Resized`
        // events between two frames, and a surface configured for a size the
        // window no longer has presents a stretched or clipped frame. Asking the
        // renderer to match the *current* size right before drawing costs
        // nothing when it already does.
        if let (Some((win_w, win_h)), Some(renderer)) = (self.window_size(), self.renderer.as_mut())
        {
            renderer.resize(win_w, win_h);
        }
        // The guest may have changed resolution since the last frame. The lock
        // is held only for the upload (a `memcpy` into a staging buffer). A
        // renderer's shared frame, when that is what the scanout shows, is
        // already on the GPU (ADR-0004, zero-copy presentation).
        let shared_frame = self.shared.get().and_then(|p| p.current());
        let mirror = Arc::clone(&self.scanout);
        let guest_size = {
            let mut scanout = lock_scanout(&mirror);
            let size = scanout.size();
            if let Some(renderer) = self.renderer.as_mut() {
                renderer.prepare(&mut scanout, shared_frame);
            }
            size
        };
        self.guest_size = guest_size;
        if self.occluded {
            return;
        }
        let Some((win_w, win_h)) = self.window_size() else {
            return;
        };
        // `viewport_for` returns None for a zero-sized (minimized) window:
        // nothing to present, and the dirty rect stays accumulated for the next
        // frame.
        let Some(viewport) = ux::viewport_for(self.mode, guest_size.0, guest_size.1, win_w, win_h)
        else {
            self.viewport = None;
            self.capture.set_viewport(None);
            return;
        };
        if self.viewport != Some(viewport) {
            self.viewport = Some(viewport);
            self.capture.set_viewport(self.viewport);
            self.sync_cursor();
        }
        if let Some(renderer) = self.renderer.as_mut() {
            renderer.set_toolbar(self.toolbar.overlay());
        }
        let outcome = match self.renderer.as_mut() {
            Some(renderer) => renderer.render(viewport),
            None => return,
        };
        if let Err(err) = outcome {
            self.fail(event_loop, err);
            return;
        }
        let stats = self.frame_stats();
        let input = self.capture.events().stats();
        let boost = self
            .renderer
            .as_ref()
            .map(Renderer::boost_stats)
            .unwrap_or_default();
        self.reporter.maybe_report(stats, input, boost);
    }

    /// Asks the GPU driver for its clocks while the guest uses the GPU, and
    /// withdraws the request once it has not for a while ([`crate::boost`]).
    /// Runs whenever the event loop is about to sleep; while the boost is up
    /// it makes the loop wake at the end of the hold, and while it is down it
    /// arms the activity so the next submit or flip wakes the loop.
    fn steer_boost(&mut self, event_loop: &ActiveEventLoop) {
        let Some(renderer) = self.renderer.as_mut() else {
            return;
        };
        if !renderer.can_boost() {
            return;
        }
        let activity = self.waker.activity();
        let now = Instant::now();
        let mut wants = self.boost.wants(now, activity.last());
        if wants {
            activity.disarm();
        } else {
            activity.arm();
            // An activity between the look above and the arming woke no
            // one: look once more, after arming.
            wants = self.boost.wants(Instant::now(), activity.last());
            if wants {
                activity.disarm();
            }
        }
        renderer.set_boost(wants);
        match self.boost.next_change(Instant::now(), activity.last()) {
            Some(at) if wants => event_loop.set_control_flow(ControlFlow::WaitUntil(at)),
            _ => event_loop.set_control_flow(ControlFlow::Wait),
        }
    }

    /// Applies a reserved shortcut's effect to the window (backlog MVP-907,
    /// WIN-1501/1502/1504). The [`InputCapture`] already updated its own state
    /// and queued whatever the VM supervisor needs to hear about.
    fn apply_action(&mut self, action: WindowAction) {
        match action {
            WindowAction::SetGrab(grabbed) => self.set_grab(grabbed),
            WindowAction::Quit => {
                tracing::info!("Ctrl+Alt+Q: quit requested; the VM supervisor decides");
            }
            WindowAction::Lifecycle => {
                tracing::debug!("lifecycle shortcut queued for the VM supervisor");
            }
            WindowAction::ToggleFullscreen => self.toggle_fullscreen(),
            WindowAction::ToggleScaleMode => self.toggle_scale_mode(),
            WindowAction::TypeClipboard => self.type_clipboard(),
        }
    }

    /// Draws now on Windows, asks for a redraw elsewhere: the same split as
    /// `user_event`, for changes the window makes itself (the toolbar).
    fn redraw(&mut self, event_loop: &ActiveEventLoop) {
        if cfg!(windows) {
            self.draw(event_loop);
        } else if let Some(window) = self.window.as_ref() {
            window.request_redraw();
        }
    }

    /// Carries out a toolbar click. The guest-lifecycle ones go to the VM
    /// supervisor exactly as their shortcuts do, guest-held keys released
    /// first; the window's own ones are applied here.
    fn apply_toolbar(&mut self, action: crate::toolbar::ToolbarAction) {
        use crate::toolbar::ToolbarAction;
        let lifecycle = |app: &mut Self, event: ControlEvent| {
            app.capture.release_all();
            app.control.push(event);
        };
        tracing::info!(?action, "toolbar");
        match action {
            ToolbarAction::PowerButton => lifecycle(self, ControlEvent::PowerButtonRequested),
            ToolbarAction::PowerOff => lifecycle(self, ControlEvent::QuitRequested),
            ToolbarAction::PauseToggle => lifecycle(self, ControlEvent::PauseToggleRequested),
            ToolbarAction::Reset => lifecycle(self, ControlEvent::ResetRequested),
            ToolbarAction::Suspend => lifecycle(self, ControlEvent::SaveRequested),
            ToolbarAction::ToggleFullscreen => self.toggle_fullscreen(),
            ToolbarAction::ToggleScale => self.toggle_scale_mode(),
            ToolbarAction::ReleaseInput => {
                if let Some(action) = self.capture.release_grab() {
                    self.apply_action(action);
                }
            }
            ToolbarAction::TypeClipboard => self.type_clipboard(),
            ToolbarAction::Screenshot => self.save_screenshot(),
            ToolbarAction::CtrlAltDel => {
                // Through the typist, so it is paced and pause-gated like
                // typed text; held keys go back first.
                self.capture.release_all();
                if let Some(typist) = &self.typist {
                    typist.chord(&[29, 56, 111]);
                }
            }
        }
    }

    /// Remembers where the window is, for the next start
    /// ([`crate::placement`]). Not while fullscreen or minimized: neither is
    /// a place to come back to, so the last ordinary placement is kept.
    fn save_placement(&self) {
        let (Some(path), Some(window)) = (self.placement_file.as_deref(), self.window.as_ref())
        else {
            return;
        };
        if self.fullscreen || window.is_minimized() == Some(true) {
            return;
        }
        let maximized = window.is_maximized();
        // A maximized window's own size is the screen's; keep the ordinary
        // size it had, if one was saved, so un-maximizing goes back to it.
        let previous = crate::placement::Placement::load(path);
        let size = window.inner_size();
        let Ok(position) = window.outer_position() else {
            return;
        };
        let placement = match (maximized, previous) {
            (true, Some(previous)) => crate::placement::Placement {
                maximized: true,
                ..previous
            },
            _ => crate::placement::Placement {
                x: position.x,
                y: position.y,
                width: size.width,
                height: size.height,
                maximized,
            },
        };
        if size.width == 0 || size.height == 0 {
            return;
        }
        if let Err(error) = placement.save(path) {
            tracing::debug!(path = %path.display(), %error, "window placement not saved");
        }
    }

    /// Saves the guest's screen (cursor included) as a PNG in
    /// [`screenshot_dir`], on a thread of its own — a shared frame is read
    /// back from the GPU — and says where in the title for a few seconds.
    fn save_screenshot(&mut self) {
        let Some(dir) = screenshot_dir() else {
            tracing::warn!("no Pictures folder to save a screenshot in");
            return;
        };
        let name = self
            .title
            .rsplit('—')
            .next()
            .unwrap_or("guest")
            .trim()
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '-' {
                    c
                } else {
                    '_'
                }
            })
            .collect::<String>();
        let path = dir.join(format!(
            "{name}-{}.png",
            file_timestamp(std::time::SystemTime::now())
        ));
        let handle = self.shot.clone();
        let target = path.clone();
        let spawned = std::thread::Builder::new()
            .name("screenshot".into())
            .spawn(move || {
                let saved = std::fs::create_dir_all(&dir)
                    .map_err(DisplayError::from)
                    .and_then(|()| handle.screenshot(&target));
                if let Err(error) = saved {
                    tracing::warn!(path = %target.display(), %error, "screenshot not saved");
                }
            });
        if spawned.is_ok() {
            let file = path
                .file_name()
                .map(|f| f.to_string_lossy().into_owned())
                .unwrap_or_default();
            self.note = Some((
                format!("screenshot saved: {file}"),
                Instant::now() + std::time::Duration::from_secs(4),
            ));
            self.update_title();
        }
    }

    /// Types the host clipboard's text into the guest. Held keys are released
    /// first, so a Ctrl still down from the click's shortcut cannot turn the
    /// text into a string of shortcuts.
    fn type_clipboard(&mut self) {
        let Some(typist) = self.typist.clone() else {
            return;
        };
        let text = match arboard::Clipboard::new().and_then(|mut c| c.get_text()) {
            Ok(text) => text,
            Err(error) => {
                tracing::warn!(%error, "the clipboard holds no text to type");
                return;
            }
        };
        self.capture.release_all();
        let outcome = typist.type_text(&text);
        tracing::info!(
            typed = outcome.typed,
            skipped = outcome.skipped,
            truncated = outcome.truncated,
            "typing the clipboard into the guest"
        );
    }

    fn set_grab(&mut self, grabbed: bool) {
        if self.grabbed == grabbed {
            self.sync_cursor();
            return;
        }
        self.grabbed = grabbed;
        #[cfg(windows)]
        if let Some(hook) = &self.keyboard_hook {
            hook.set_grab(grabbed);
        }
        self.update_title();
        self.sync_cursor();
        self.sync_cursor_grab();
        tracing::info!(grabbed, "pointer grab toggled");
    }

    /// Reapplies confinement to the current client rectangle, even when the
    /// logical grab state is unchanged after moving or resizing the window.
    fn sync_cursor_grab(&self) {
        let Some(window) = self.window.as_ref() else {
            return;
        };
        // AppKit cannot confine a pointer. Keeping its absolute motion is
        // necessary for the guest tablet; locking it would freeze that input.
        let mode = if self.grabbed && !cfg!(target_os = "macos") {
            CursorGrabMode::Confined
        } else {
            CursorGrabMode::None
        };
        if let Err(err) = window.set_cursor_grab(mode) {
            // Wayland compositors may refuse confinement; the guest still gets
            // absolute positions, so this is a warning, not a failure.
            tracing::warn!(%err, grabbed = self.grabbed, "compositor refused the pointer grab");
        }
    }

    /// Mirrors the cursor policy onto the window (WIN-1501). Cheap enough to call
    /// after every pointer event: it only talks to winit when the decision
    /// actually flipped.
    ///
    /// On Wayland and Windows both states are cursors ([`HostCursors`]) rather
    /// than `set_cursor_visible(false)` / `CursorIcon::Default` — see the
    /// [`App::cursors`] docs for the WSLg story. Everywhere else the plain
    /// visibility flag works and is kept.
    fn sync_cursor(&mut self) {
        // Over the toolbar the user needs the host arrow, grabbed or not.
        let visible = self.capture.cursor_visible() || self.toolbar.captures_pointer();
        if visible == self.cursor_visible {
            return;
        }
        self.cursor_visible = visible;
        let Some(window) = self.window.as_ref() else {
            return;
        };
        match (&self.cursors, visible) {
            (Some(cursors), false) => window.set_cursor(cursors.hidden.clone()),
            (Some(cursors), true) => window.set_cursor(cursors.shown.clone()),
            (None, _) => window.set_cursor_visible(visible),
        }
        // Low frequency (only when crossing the image edge or changing the
        // grab), and the first thing to check when WIN-1501 misbehaves.
        tracing::debug!(visible, "host cursor visibility changed");
    }

    /// `F11`: borderless fullscreen on the window's current monitor (WIN-1504).
    fn toggle_fullscreen(&mut self) {
        self.fullscreen = !self.fullscreen;
        // F11 scales the existing desktop. A display-mode switch underneath
        // an X11 game can desynchronise its mouse and rendering coordinates.
        self.resize_pending = None;
        let fullscreen = self.fullscreen;
        if let Some(window) = self.window.as_ref() {
            // `Borderless(None)` means "the monitor this window is on", which is
            // what the user expects on a multi-head desktop.
            window.set_fullscreen(fullscreen.then_some(Fullscreen::Borderless(None)));
            window.request_redraw();
        }
        tracing::info!(fullscreen, "fullscreen toggled");
        // The compositor answers with a Resized event, which recomputes the
        // viewport; do it now too so nothing depends on that arriving.
        self.recompute_viewport();
    }

    /// `Ctrl+Alt+O`: switch between letterboxed scaling and 1:1 (WIN-1504).
    fn toggle_scale_mode(&mut self) {
        self.mode = self.mode.toggled();
        tracing::info!(mode = ?self.mode, "scale mode toggled");
        self.recompute_viewport();
        self.update_title();
        if let Some(window) = self.window.as_ref() {
            window.request_redraw();
        }
    }

    /// Statistics snapshot, used by the demo and future diagnostics.
    fn frame_stats(&self) -> FrameStats {
        self.renderer
            .as_ref()
            .map(Renderer::stats)
            .unwrap_or_default()
    }
}

/// The cursors Wayland and Windows show instead of toggling
/// `set_cursor_visible` (WIN-1501); see [`App::cursors`] for why both exist.
struct HostCursors {
    /// A fully transparent 8×8 image: "hidden". 8×8 rather than 1×1 because
    /// tiny buffers have historically upset compositors; 256 bytes, once.
    hidden: Cursor,
    /// "Visible". On Wayland a bundled classic arrow *image* rather than
    /// `CursorIcon::Default`, because named cursors need an XCursor theme the
    /// host may not have (stock WSL roots don't), and a failed theme lookup
    /// no-ops silently — leaving the transparent image stuck on the pointer.
    /// On Windows the system arrow, which is always there.
    shown: Cursor,
}

/// The classic left-pointer, one row per line: `B` black, `W` white outline,
/// space transparent. Drawn here so showing the cursor never depends on the
/// host's cursor-theme installation. Only the Wayland path renders it, but the
/// image is checked by the unit tests on every host.
#[cfg(any(target_os = "linux", test))]
const ARROW_PIXELS: [&str; 19] = [
    "W           ",
    "WW          ",
    "WBW         ",
    "WBBW        ",
    "WBBBW       ",
    "WBBBBW      ",
    "WBBBBBW     ",
    "WBBBBBBW    ",
    "WBBBBBBBW   ",
    "WBBBBBBBBW  ",
    "WBBBBBWWWWW ",
    "WBBWBBW     ",
    "WBW WBBW    ",
    "WW  WBBW    ",
    "W    WBBW   ",
    "     WBBW   ",
    "      WBBW  ",
    "      WBBW  ",
    "       WW   ",
];

/// Renders [`ARROW_PIXELS`] as RGBA bytes plus its width and height.
#[cfg(any(target_os = "linux", test))]
fn arrow_rgba() -> (Vec<u8>, u16, u16) {
    let height = ARROW_PIXELS.len() as u16;
    let width = ARROW_PIXELS[0].len() as u16;
    let mut rgba = Vec::with_capacity(usize::from(width) * usize::from(height) * 4);
    for row in ARROW_PIXELS {
        for pixel in row.bytes() {
            rgba.extend_from_slice(match pixel {
                b'B' => &[0x00, 0x00, 0x00, 0xff],
                b'W' => &[0xff, 0xff, 0xff, 0xff],
                _ => &[0x00, 0x00, 0x00, 0x00],
            });
        }
    }
    (rgba, width, height)
}

impl HostCursors {
    /// Builds both cursors, on Wayland only. `None` off Wayland (and if an
    /// image is rejected), which sends [`App::sync_cursor`] back to
    /// `set_cursor_visible` — correct on X11, where the flag works.
    #[cfg(target_os = "linux")]
    fn new(event_loop: &ActiveEventLoop) -> Option<Self> {
        use winit::platform::wayland::ActiveEventLoopExtWayland;
        if !event_loop.is_wayland() {
            return None;
        }
        let hidden = CustomCursor::from_rgba(vec![0u8; 8 * 8 * 4], 8, 8, 0, 0);
        let (rgba, width, height) = arrow_rgba();
        // The hotspot is the arrow's tip, the top-left corner.
        let arrow = CustomCursor::from_rgba(rgba, width, height, 0, 0);
        match (hidden, arrow) {
            (Ok(hidden), Ok(arrow)) => Some(Self {
                hidden: event_loop.create_custom_cursor(hidden).into(),
                shown: event_loop.create_custom_cursor(arrow).into(),
            }),
            (hidden, arrow) => {
                let err = hidden.err().or(arrow.err());
                tracing::warn!(
                    ?err,
                    "cannot build the cursor images; falling back to set_cursor_visible"
                );
                None
            }
        }
    }

    /// Windows: the transparent image, and the system arrow. Without them a
    /// grabbed guest gets no pointer motion at all (see [`App::cursors`]), so
    /// a rejected image is worth a warning.
    #[cfg(windows)]
    fn new(event_loop: &ActiveEventLoop) -> Option<Self> {
        match CustomCursor::from_rgba(vec![0u8; 8 * 8 * 4], 8, 8, 0, 0) {
            Ok(hidden) => Some(Self {
                hidden: event_loop.create_custom_cursor(hidden).into(),
                shown: Cursor::Icon(winit::window::CursorIcon::Default),
            }),
            Err(err) => {
                tracing::warn!(
                    %err,
                    "cannot build the transparent cursor; a grabbed guest will get no pointer motion"
                );
                None
            }
        }
    }

    /// Other hosts (macOS): the plain visibility flag works there.
    #[cfg(not(any(target_os = "linux", windows)))]
    fn new(_event_loop: &ActiveEventLoop) -> Option<Self> {
        None
    }
}

/// A monitor's size in *logical* pixels, to compare against the configured
/// window size (which is also logical). `None` when winit cannot name a monitor,
/// as happens on some remote/headless compositors.
fn monitor_logical_size(monitor: Option<MonitorHandle>) -> Option<(u32, u32)> {
    let monitor = monitor?;
    let size = monitor.size();
    let scale = monitor.scale_factor();
    if !scale.is_finite() || scale <= 0.0 {
        return Some((size.width, size.height));
    }
    let logical: winit::dpi::LogicalSize<f64> = size.to_logical(scale);
    Some((logical.width.round() as u32, logical.height.round() as u32))
}

impl ApplicationHandler<HostEvent> for App {
    fn new_events(&mut self, event_loop: &ActiveEventLoop, cause: StartCause) {
        if matches!(cause, StartCause::Init) {
            // Redraws are guest-driven: wait for events instead of spinning.
            event_loop.set_control_flow(ControlFlow::Wait);
        }
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        self.steer_boost(event_loop);
        if let Some((_, until)) = &self.note {
            let until = *until;
            if Instant::now() >= until {
                self.note = None;
                self.update_title();
            } else {
                event_loop.set_control_flow(ControlFlow::WaitUntil(until));
            }
        }
        if let Some(since) = self.resize_pending {
            let due = since + crate::resize::SETTLE;
            if Instant::now() >= due {
                self.resize_pending = None;
                if let Some((w, h)) = self.window_size() {
                    self.resolution
                        .follow_window(self.follow_window, self.fullscreen, w, h);
                }
            } else {
                event_loop.set_control_flow(ControlFlow::WaitUntil(due));
            }
        }
    }

    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_some() {
            return;
        }
        if let Err(err) = self.create_window(event_loop) {
            self.fail(event_loop, err);
        }
    }

    fn suspended(&mut self, _event_loop: &ActiveEventLoop) {
        // Android-style suspend: drop the surface-bound state. On desktop this
        // never fires, but leaving the renderer alive would be a use-after-free
        // risk if it did.
        tracing::debug!("event loop suspended; dropping the renderer");
        self.renderer = None;
    }

    fn user_event(&mut self, event_loop: &ActiveEventLoop, event: HostEvent) {
        match event {
            // On Windows a guest frame or a cursor move is drawn right here.
            // `request_redraw` there is `WM_PAINT`, which Windows synthesises
            // only once the thread's queue is empty — and a moving 1000 Hz
            // mouse keeps it from being empty: the window then drew 30-50
            // times a second while the guest produced 60-90 frames, and every
            // cursor move made video and desktop stutter (found 2026-10-12).
            // Elsewhere `request_redraw` is kept: on Wayland it is what ties a
            // frame to the compositor's frame callback.
            HostEvent::Redraw => {
                if cfg!(windows) {
                    self.draw(event_loop);
                } else if let Some(window) = self.window.as_ref() {
                    window.request_redraw();
                }
            }
            // `about_to_wait` looks at the activity; the wakeup was the point.
            HostEvent::GpuActive => {}
            HostEvent::Shutdown => {
                tracing::info!("shutdown requested; closing the window");
                event_loop.exit();
            }
        }
    }

    fn window_event(
        &mut self,
        event_loop: &ActiveEventLoop,
        _window_id: WindowId,
        event: WindowEvent,
    ) {
        match event {
            WindowEvent::CloseRequested => {
                self.control.push(ControlEvent::WindowCloseRequested);
                self.capture.release_all();
                event_loop.exit();
            }
            WindowEvent::Resized(size) => {
                if let Some(renderer) = self.renderer.as_mut() {
                    renderer.resize(size.width, size.height);
                }
                if self.follow_window && !self.fullscreen && size.width > 0 && size.height > 0 {
                    self.resize_pending = Some(Instant::now());
                }
                self.recompute_viewport();
                tracing::debug!(
                    width = size.width,
                    height = size.height,
                    viewport = ?self.viewport,
                    "window resized"
                );
                if let Some(window) = self.window.as_ref() {
                    window.request_redraw();
                }
            }
            // Confinement uses desktop coordinates, so moving a grabbed
            // window (including between monitors) must update it as well.
            #[cfg(windows)]
            WindowEvent::Moved(_) => self.sync_cursor_grab(),
            WindowEvent::ScaleFactorChanged { scale_factor, .. } => {
                // winit follows this with a Resized carrying physical pixels.
                tracing::debug!(scale_factor, "window scale factor changed");
            }
            WindowEvent::Occluded(occluded) => {
                self.occluded = occluded;
                if !occluded {
                    if let Some(window) = self.window.as_ref() {
                        window.request_redraw();
                    }
                }
            }
            WindowEvent::RedrawRequested => self.draw(event_loop),
            WindowEvent::KeyboardInput {
                event,
                is_synthetic,
                ..
            } => {
                if is_synthetic {
                    // Focus-change synthetics; our own focus handling covers it.
                    return;
                }
                let outcome = self
                    .capture
                    .on_key(event.physical_key, event.state, event.repeat);
                if let KeyOutcome::Reserved(action) = outcome {
                    self.apply_action(action);
                }
            }
            WindowEvent::CursorMoved { position, .. } => {
                let serial = self.toolbar.serial();
                let eligible = self.fullscreen || self.grabbed;
                let over_bar = self.toolbar.on_pointer(
                    position.x,
                    position.y,
                    eligible,
                    std::time::Instant::now(),
                );
                if !over_bar {
                    self.capture
                        .on_pointer(self.viewport, position.x, position.y);
                }
                self.sync_cursor();
                if self.toolbar.serial() != serial {
                    self.redraw(event_loop);
                }
            }
            WindowEvent::CursorEntered { .. } => {
                self.capture.on_pointer_in_window(true);
                self.sync_cursor();
            }
            WindowEvent::CursorLeft { .. } => {
                self.capture.on_pointer_in_window(false);
                let serial = self.toolbar.serial();
                self.toolbar.hide();
                self.sync_cursor();
                if self.toolbar.serial() != serial {
                    self.redraw(event_loop);
                }
            }
            WindowEvent::MouseInput { state, button, .. } => {
                // The toolbar takes every button over itself, and the release
                // of a press that began there.
                let pressed = state == winit::event::ElementState::Pressed;
                if (pressed && self.toolbar.captures_pointer()) || (!pressed && self.toolbar_press)
                {
                    self.toolbar_press = pressed;
                    if pressed && button == winit::event::MouseButton::Left {
                        let serial = self.toolbar.serial();
                        if let Some(action) = self.toolbar.on_click(std::time::Instant::now()) {
                            self.apply_toolbar(action);
                        }
                        if self.toolbar.serial() != serial {
                            self.redraw(event_loop);
                        }
                    }
                    self.sync_cursor();
                    return;
                }
                // A click on the guest image engages the grab (WIN-1502).
                if let KeyOutcome::Reserved(action) = self.capture.on_button(button, state) {
                    self.apply_action(action);
                }
                self.sync_cursor();
            }
            WindowEvent::MouseWheel { delta, .. } => {
                self.capture.on_wheel(delta);
            }
            WindowEvent::Focused(focused) => {
                self.capture.on_focus(focused);
                if !focused {
                    self.set_grab(false);
                    self.toolbar.hide();
                    self.toolbar_press = false;
                }
                self.sync_cursor();
            }
            _ => {}
        }
    }

    fn exiting(&mut self, _event_loop: &ActiveEventLoop) {
        // Unhook and join before the HWND can be destroyed or reused.
        #[cfg(windows)]
        drop(self.keyboard_hook.take());
        self.save_placement();
        let stats = self.frame_stats();
        let scanout = lock_scanout(&self.scanout).stats();
        tracing::info!(
            frames = stats.frames,
            shared_frames = stats.shared_frames,
            guest_frames = stats.guest_frames,
            unshown = stats.unshown,
            skipped = stats.skipped,
            uploads = stats.uploads,
            upload_mib = format_args!("{:.1}", stats.bytes_uploaded as f64 / (1024.0 * 1024.0)),
            texture_allocations = stats.texture_allocations,
            surface_recoveries = stats.surface_recoveries,
            guest_updates = scanout.updates,
            guest_updates_rejected = scanout.rejected,
            "display host exiting"
        );
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn a_screenshot_file_name_carries_the_utc_date_and_time() {
        let at = std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_760_000_000);
        assert_eq!(super::file_timestamp(at), "20251009-085320");
        let leap = std::time::UNIX_EPOCH + std::time::Duration::from_secs(951_782_400);
        assert_eq!(super::file_timestamp(leap), "20000229-000000");
    }

    use super::*;

    #[test]
    fn the_bundled_arrow_is_a_consistent_rgba_image() {
        let (rgba, width, height) = arrow_rgba();
        assert_eq!(rgba.len(), usize::from(width) * usize::from(height) * 4);
        for row in ARROW_PIXELS {
            assert_eq!(row.len(), usize::from(width), "ragged pixel row: {row:?}");
        }
        // The hotspot pixel (the tip, top-left) must be opaque or the click
        // point would be invisible.
        assert_eq!(rgba[3], 0xff, "hotspot pixel is transparent");
        // The image must contain all three pixel kinds.
        let pixels: Vec<&[u8]> = rgba.chunks_exact(4).collect();
        assert!(pixels.contains(&[0x00u8, 0x00, 0x00, 0xff].as_slice()));
        assert!(pixels.contains(&[0xffu8, 0xff, 0xff, 0xff].as_slice()));
        assert!(pixels.contains(&[0x00u8, 0x00, 0x00, 0x00].as_slice()));
    }
}
