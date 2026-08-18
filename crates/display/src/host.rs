//! The window and its event loop (backlog MVP-701, 705, 706).
//!
//! The event loop owns the window, the renderer and the input capture, and runs
//! on the thread that called [`DisplayHost::run`] — which must be the main
//! thread (hard requirement on macOS, good hygiene everywhere). Device threads
//! never touch this state; they go through [`DisplayHandle`] and
//! [`InputQueue`].

use std::sync::{Arc, Mutex};

use winit::application::ApplicationHandler;
use winit::event::{StartCause, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::monitor::MonitorHandle;
use winit::window::{CursorGrabMode, Fullscreen, Window, WindowId};

use crate::handle::{HostEvent, Waker};
use crate::input::{
    ControlEvent, ControlQueue, InputCapture, InputQueue, KeyOutcome, WindowAction,
};
use crate::renderer::{FrameStats, Renderer, StatsReporter};
use crate::scanout::{lock_scanout, Scanout, SharedScanout};
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
}

impl DisplayHost {
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
        })
    }

    /// Overrides the window title (usually `entangled: <vm id>`).
    #[must_use]
    pub fn with_title(mut self, title: impl Into<String>) -> Self {
        self.title = title.into();
        self
    }

    /// The device-facing scanout API. Clone freely; safe to use from any thread.
    pub fn handle(&self) -> DisplayHandle {
        DisplayHandle::new(Arc::clone(&self.scanout), self.waker.clone())
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
        } = self;
        let mut app = App {
            config,
            title,
            scanout,
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
            fullscreen: false,
            mode: ScaleMode::default(),
            reporter: StatsReporter::default(),
            fatal: None,
        };
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
    /// Cursor visibility actually applied to the window (WIN-1501).
    cursor_visible: bool,
    /// Borderless-fullscreen state (WIN-1504).
    fullscreen: bool,
    /// Fit or 1:1 (WIN-1503/1504).
    mode: ScaleMode,
    reporter: StatsReporter,
    fatal: Option<DisplayError>,
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
        let attributes = Window::default_attributes()
            .with_title(self.window_title())
            .with_inner_size(winit::dpi::LogicalSize::new(initial.width, initial.height))
            .with_min_inner_size(winit::dpi::LogicalSize::new(
                ux::MIN_WINDOW_WIDTH,
                ux::MIN_WINDOW_HEIGHT,
            ))
            .with_resizable(true)
            .with_maximized(initial.maximized);
        let window = Arc::new(event_loop.create_window(attributes)?);
        let size = window.inner_size();
        tracing::info!(
            width = size.width,
            height = size.height,
            scale_factor = window.scale_factor(),
            "window created"
        );
        let renderer = Renderer::new(Arc::clone(&window), self.guest_size.0, self.guest_size.1)?;
        self.window = Some(window);
        self.renderer = Some(renderer);
        self.recompute_viewport();
        Ok(())
    }

    /// The title the window should currently carry (WIN-1502): the VM's own
    /// title plus what the user needs to know about the input state.
    fn window_title(&self) -> String {
        ux::window_title(
            &self.title,
            WindowStatus {
                grabbed: self.grabbed,
                mode: self.mode,
            },
        )
    }

    /// Refreshes the title in place. Called only when the state behind it
    /// changes, never per frame — `set_title` is a round trip to the compositor.
    fn update_title(&self) {
        let title = self.window_title();
        if let Some(window) = self.window.as_ref() {
            window.set_title(&title);
        }
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
        self.viewport = match self.window_size() {
            Some((w, h)) => ux::viewport_for(self.mode, self.guest_size.0, self.guest_size.1, w, h),
            None => None,
        };
        self.capture.set_viewport(self.viewport);
        self.sync_cursor();
    }

    /// Uploads the dirty rect and presents (backlog MVP-704/705/706).
    fn draw(&mut self, event_loop: &ActiveEventLoop) {
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
        // is held only for the upload (a `memcpy` into a staging buffer).
        let shared = Arc::clone(&self.scanout);
        let guest_size = {
            let mut scanout = lock_scanout(&shared);
            let size = scanout.size();
            if let Some(renderer) = self.renderer.as_mut() {
                renderer.upload(&mut scanout);
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
        self.reporter.maybe_report(stats, input);
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
            WindowAction::ToggleFullscreen => self.toggle_fullscreen(),
            WindowAction::ToggleScaleMode => self.toggle_scale_mode(),
        }
    }

    fn set_grab(&mut self, grabbed: bool) {
        if self.grabbed == grabbed {
            self.sync_cursor();
            return;
        }
        self.grabbed = grabbed;
        self.update_title();
        self.sync_cursor();
        let Some(window) = self.window.as_ref() else {
            return;
        };
        let mode = if grabbed {
            CursorGrabMode::Confined
        } else {
            CursorGrabMode::None
        };
        if let Err(err) = window.set_cursor_grab(mode) {
            // Wayland compositors may refuse confinement; the guest still gets
            // absolute positions, so this is a warning, not a failure.
            tracing::warn!(%err, grabbed, "compositor refused the pointer grab");
        }
        tracing::info!(grabbed, "pointer grab toggled");
    }

    /// Mirrors the cursor policy onto the window (WIN-1501). Cheap enough to call
    /// after every pointer event: it only talks to winit when the decision
    /// actually flipped.
    fn sync_cursor(&mut self) {
        let visible = self.capture.cursor_visible();
        if visible == self.cursor_visible {
            return;
        }
        self.cursor_visible = visible;
        if let Some(window) = self.window.as_ref() {
            window.set_cursor_visible(visible);
            tracing::trace!(visible, "host cursor visibility changed");
        }
    }

    /// `F11`: borderless fullscreen on the window's current monitor (WIN-1504).
    fn toggle_fullscreen(&mut self) {
        self.fullscreen = !self.fullscreen;
        let fullscreen = self.fullscreen;
        if let Some(window) = self.window.as_ref() {
            // `Borderless(None)` means "the monitor this window is on", which is
            // what the user expects on a multi-head desktop.
            window.set_fullscreen(fullscreen.then(|| Fullscreen::Borderless(None)));
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
            HostEvent::Redraw => {
                if let Some(window) = self.window.as_ref() {
                    window.request_redraw();
                }
            }
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
                self.capture
                    .on_pointer(self.viewport, position.x, position.y);
                self.sync_cursor();
            }
            WindowEvent::CursorEntered { .. } => {
                self.capture.on_pointer_in_window(true);
                self.sync_cursor();
            }
            WindowEvent::CursorLeft { .. } => {
                self.capture.on_pointer_in_window(false);
                self.sync_cursor();
            }
            WindowEvent::MouseInput { state, button, .. } => {
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
                }
                self.sync_cursor();
            }
            _ => {}
        }
    }

    fn exiting(&mut self, _event_loop: &ActiveEventLoop) {
        let stats = self.frame_stats();
        let scanout = lock_scanout(&self.scanout).stats();
        tracing::info!(
            frames = stats.frames,
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
