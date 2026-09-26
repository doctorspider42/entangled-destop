//! The `wgpu` presentation path (backlog MVP-702…706, 708).
//!
//! One device + queue + scanout texture per window, created once. Each frame
//! uploads only the dirty rect of the scanout mirror and draws it into the
//! letterboxed viewport with a full-screen triangle; the letterbox bars are the
//! attachment's black clear color.
//!
//! When the scanout is a renderer's shared image (ADR-0004, zero-copy
//! presentation, [`crate::shared`]) the frame instead samples the shared
//! presenter's texture — already on this device, copied there on the GPU —
//! and the cursor plane is drawn over it by the GPU ([`CursorOverlay`]),
//! since no CPU mirror holds the pixels beneath it. Shared presentation needs
//! wgpu's **Vulkan** backend, so a window asked for it
//! ([`crate::DisplayHost::with_shared_scanout`]) tries that backend first and
//! falls back to the usual order, and the copy path, if it cannot.

use std::sync::Arc;
use std::time::{Duration, Instant};

use virtio_gpu::BYTES_PER_PIXEL;
use winit::window::Window;

use crate::present::{CursorOverlay, ImagePipeline};
use crate::refresh::{choose_present_mode, PresentPreference};
use crate::scanout::Scanout;
use crate::shared::{SharedSlot, SharedTexture};
use crate::{DisplayError, Viewport};

/// Renderer counters for the periodic diagnostics event (backlog MVP-708).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct FrameStats {
    /// Frames actually presented.
    pub frames: u64,
    /// Redraws skipped (minimized, occluded or unconfigured surface).
    pub skipped: u64,
    /// `write_texture` calls (one per coalesced dirty rect).
    pub uploads: u64,
    /// Pixel bytes copied into the GPU texture.
    pub bytes_uploaded: u64,
    /// Scanout texture (re)allocations — one per guest mode change.
    pub texture_allocations: u64,
    /// Surface reconfigurations after `Lost`/`Outdated`.
    pub surface_recoveries: u64,
    /// Frames drawn from a renderer's shared image rather than the mirror
    /// (ADR-0004, zero-copy presentation).
    pub shared_frames: u64,
    /// Guest frames that reached the display: flushes of the scanout, shared
    /// or copied (cursor moves are not frames). Against [`Self::frames`] this
    /// is the window's side of the guest's flip rate.
    pub guest_frames: u64,
    /// Guest frames the window never drew: superseded by a newer one before
    /// the window got to it (the host monitor is slower than the guest, or
    /// the window was blocked), or arriving while it was minimized or
    /// occluded. With a `Mailbox` swapchain the compositor can still drop a
    /// drawn frame at its own vblank, which no counter here sees.
    pub unshown: u64,
    /// Time spent acquiring the next swapchain image, summed, in
    /// microseconds: where a FIFO swapchain makes the window wait for a host
    /// vblank.
    pub acquire_us: u64,
    /// The longest single acquire, in microseconds.
    pub acquire_max_us: u64,
    /// Time spent in the present call itself, summed, in microseconds.
    pub present_us: u64,
    /// The longest single present call, in microseconds.
    pub present_max_us: u64,
}

/// Logs FPS and copy statistics once per second (backlog MVP-708).
#[derive(Debug)]
pub struct StatsReporter {
    last: Instant,
    previous: FrameStats,
    interval: Duration,
}

impl Default for StatsReporter {
    fn default() -> Self {
        Self {
            last: Instant::now(),
            previous: FrameStats::default(),
            interval: Duration::from_secs(1),
        }
    }
}

impl StatsReporter {
    /// Emits a `tracing` event if the reporting interval has elapsed.
    pub fn maybe_report(&mut self, stats: FrameStats, input: crate::input::InputStats) {
        let elapsed = self.last.elapsed();
        if elapsed < self.interval {
            return;
        }
        let secs = elapsed.as_secs_f64();
        let frames = stats.frames.saturating_sub(self.previous.frames);
        let uploads = stats.uploads.saturating_sub(self.previous.uploads);
        let bytes = stats
            .bytes_uploaded
            .saturating_sub(self.previous.bytes_uploaded);
        let shared = stats
            .shared_frames
            .saturating_sub(self.previous.shared_frames);
        let guest = stats
            .guest_frames
            .saturating_sub(self.previous.guest_frames);
        let unshown = stats.unshown.saturating_sub(self.previous.unshown);
        let per_frame = |now: u64, before: u64| {
            if frames == 0 {
                0.0
            } else {
                now.saturating_sub(before) as f64 / frames as f64 / 1000.0
            }
        };
        let acquire_ms = per_frame(stats.acquire_us, self.previous.acquire_us);
        let present_ms = per_frame(stats.present_us, self.previous.present_us);
        tracing::info!(
            fps = format_args!("{:.1}", frames as f64 / secs),
            shared_fps = format_args!("{:.1}", shared as f64 / secs),
            // The guest's frames against the window's presents (ADR-0004,
            // the high-refresh amendment).
            guest_fps = format_args!("{:.1}", guest as f64 / secs),
            unshown_per_s = format_args!("{:.1}", unshown as f64 / secs),
            // Where the window thread waits per frame: the swapchain acquire
            // (a FIFO host vblank) and the present call (ADR-0004, the
            // high-refresh amendment). The maxima are for the whole run.
            acquire_ms = format_args!("{acquire_ms:.3}"),
            acquire_max_ms = format_args!("{:.3}", stats.acquire_max_us as f64 / 1000.0),
            present_ms = format_args!("{present_ms:.3}"),
            present_max_ms = format_args!("{:.3}", stats.present_max_us as f64 / 1000.0),
            uploads_per_s = format_args!("{:.1}", uploads as f64 / secs),
            upload_mib_per_s = format_args!("{:.2}", bytes as f64 / secs / (1024.0 * 1024.0)),
            skipped = stats.skipped.saturating_sub(self.previous.skipped),
            texture_allocations = stats.texture_allocations,
            surface_recoveries = stats.surface_recoveries,
            input_batches = input.batches,
            input_dropped = input.dropped,
            "display statistics"
        );
        self.previous = stats;
        self.last = Instant::now();
    }
}

/// Owns the surface, device and scanout texture for one window.
pub(crate) struct Renderer {
    surface: wgpu::Surface<'static>,
    adapter: wgpu::Adapter,
    device: wgpu::Device,
    queue: wgpu::Queue,
    config: wgpu::SurfaceConfiguration,
    /// False while the window is zero-sized: no surface, nothing to present.
    configured: bool,
    image: ImagePipeline,
    cursor: CursorOverlay,
    texture: wgpu::Texture,
    bind_group: wgpu::BindGroup,
    /// The shared presenter's texture this frame draws, and the bind group
    /// sampling it (rebuilt when its generation changes); `None` while the
    /// mirror is what the scanout shows.
    shared: Option<(SharedTexture, wgpu::BindGroup)>,
    /// Size of the currently allocated scanout texture.
    texture_size: (u32, u32),
    /// Scanout generation the texture was allocated for.
    generation: u64,
    stats: FrameStats,
}

/// The surface configuration of a window: its present mode is chosen
/// ([`crate::refresh::choose_present_mode`]), never `AutoVsync`.
fn surface_config(
    caps: &wgpu::SurfaceCapabilities,
    format: wgpu::TextureFormat,
    size: winit::dpi::PhysicalSize<u32>,
    preference: PresentPreference,
) -> wgpu::SurfaceConfiguration {
    let alpha_mode = caps
        .alpha_modes
        .first()
        .copied()
        .unwrap_or(wgpu::CompositeAlphaMode::Auto);
    let present_mode = choose_present_mode(&caps.present_modes, preference);
    tracing::info!(
        ?present_mode,
        ?preference,
        available = ?caps.present_modes,
        "window present mode (ENTANGLED_PRESENT_MODE to choose)"
    );
    wgpu::SurfaceConfiguration {
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
        format,
        width: size.width.max(1),
        height: size.height.max(1),
        present_mode,
        // Two frames queued at most: with `Mailbox` the newest replaces the
        // queued one, so this bounds latency without ever making the window
        // wait; with `Fifo` it is how far the window may run ahead.
        desired_maximum_frame_latency: 2,
        alpha_mode,
        view_formats: Vec::new(),
    }
}

impl Renderer {
    /// Creates the GPU state for `window` and a `guest_w`×`guest_h` scanout.
    ///
    /// Blocking on adapter/device creation happens here, at init time, and never
    /// again — the event loop must not block on GPU or guest state.
    ///
    /// With `shared`, the window prefers the Vulkan backend and, when it gets
    /// a device that can import a renderer's scanout buffers, installs its
    /// shared presenter in the slot (ADR-0004, zero-copy presentation).
    pub(crate) fn new(
        window: Arc<Window>,
        guest_w: u32,
        guest_h: u32,
        shared: Option<&SharedSlot>,
    ) -> Result<Self, DisplayError> {
        let Gpu {
            surface,
            adapter,
            device,
            queue,
            shareable,
        } = init_gpu(&window, shared.is_some())?;
        if let Some(slot) = shared {
            install_presenter(slot, shareable, &device, &queue);
        }

        let caps = surface.get_capabilities(&adapter);
        let format = pick_surface_format(&caps).ok_or(DisplayError::UnsupportedSurface)?;
        let size = window.inner_size();
        let config = surface_config(&caps, format, size, PresentPreference::from_env());

        // Linear filtering so a scaled scanout is smooth (MVP-705).
        let image = ImagePipeline::new(&device, format, wgpu::FilterMode::Linear);
        let cursor = CursorOverlay::new(&device, format);

        let texture = create_scanout_texture(&device, guest_w, guest_h);
        let bind_group = image.bind(
            &device,
            &texture.create_view(&wgpu::TextureViewDescriptor::default()),
        );

        let mut renderer = Self {
            surface,
            adapter,
            device,
            queue,
            config,
            configured: false,
            image,
            cursor,
            texture,
            bind_group,
            shared: None,
            texture_size: (guest_w, guest_h),
            generation: 0,
            stats: FrameStats {
                texture_allocations: 1,
                ..FrameStats::default()
            },
        };
        renderer.resize(size.width, size.height);
        Ok(renderer)
    }

    /// Current statistics snapshot.
    pub(crate) fn stats(&self) -> FrameStats {
        self.stats
    }

    /// Counts `arrived` guest frames against one draw: all but the newest
    /// were never drawn, and the newest too when nothing was presented.
    pub(crate) fn note_guest_frames(&mut self, arrived: u64, presented: bool) {
        self.stats.guest_frames = self.stats.guest_frames.saturating_add(arrived);
        let shown = u64::from(presented).min(arrived);
        self.stats.unshown = self.stats.unshown.saturating_add(arrived - shown);
    }

    /// Reconfigures the surface for a new window size. A zero-sized window
    /// (minimized) leaves the surface unconfigured and presenting disabled
    /// (backlog MVP-706).
    pub(crate) fn resize(&mut self, width: u32, height: u32) {
        if width == 0 || height == 0 {
            self.configured = false;
            tracing::debug!("window has zero area; presenting suspended");
            return;
        }
        // Idempotent: a rapid resize drag delivers many `Resized` events, and
        // reconfiguring the surface for a size it already has costs a swapchain
        // rebuild for nothing (WIN-1503).
        if self.configured && self.config.width == width && self.config.height == height {
            return;
        }
        self.config.width = width;
        self.config.height = height;
        self.surface.configure(&self.device, &self.config);
        self.configured = true;
    }

    /// Re-queries the surface capabilities and reconfigures after a loss.
    fn recover_surface(&mut self) {
        let caps = self.surface.get_capabilities(&self.adapter);
        if !caps.formats.contains(&self.config.format) {
            if let Some(format) = pick_surface_format(&caps) {
                tracing::warn!(?format, "surface format changed after loss");
                self.config.format = format;
            }
        }
        if !caps.present_modes.contains(&self.config.present_mode) {
            self.config.present_mode =
                choose_present_mode(&caps.present_modes, PresentPreference::from_env());
            tracing::warn!(present_mode = ?self.config.present_mode, "present mode changed after loss");
        }
        self.stats.surface_recoveries += 1;
        self.surface.configure(&self.device, &self.config);
        self.configured = true;
    }

    /// Chooses what the next frame draws: the shared presenter's texture when
    /// it is what the scanout shows (and then the cursor plane is drawn over
    /// it by the GPU), else the mirror. The mirror's texture is kept up to date
    /// either way ([`Self::upload`]), so switching back shows no stale rect.
    pub(crate) fn prepare(&mut self, scanout: &mut Scanout, shared: Option<SharedTexture>) {
        let shared = shared.filter(|s| s.size == scanout.size());
        self.upload(scanout, shared.is_none());
        let Some(texture) = shared else {
            self.shared = None;
            return;
        };
        let rebuilt = match self.shared.take() {
            Some((old, bind)) if old.generation == texture.generation => (texture, bind),
            _ => {
                let bind = self.image.bind(&self.device, &texture.view);
                (texture, bind)
            }
        };
        self.shared = Some(rebuilt);
        self.cursor.prepare(
            &self.device,
            &self.queue,
            scanout.cursor_image(),
            scanout.size(),
        );
    }

    /// Uploads the scanout's dirty rect into the texture, reallocating first if
    /// the guest changed resolution (backlog MVP-703/704), then — when
    /// `with_cursor`, i.e. the mirror is what is drawn — paints the cursor
    /// plane over it (MVP-812).
    pub(crate) fn upload(&mut self, scanout: &mut Scanout, with_cursor: bool) {
        let (guest_w, guest_h) = scanout.size();
        if scanout.generation() != self.generation || (guest_w, guest_h) != self.texture_size {
            if !self.reallocate_texture(guest_w, guest_h) {
                return;
            }
            self.generation = scanout.generation();
            scanout.mark_all_dirty();
        }
        if let Some(rect) = scanout.take_dirty() {
            if !rect.fits_within(self.texture_size.0, self.texture_size.1) {
                tracing::warn!(?rect, "dirty rect outside the texture; skipping upload");
                return;
            }
            let stride = scanout.stride();
            let offset = rect.y as u64 * stride as u64 + rect.x as u64 * u64::from(BYTES_PER_PIXEL);
            // `write_texture` from CPU memory pads rows internally, so the
            // 256-byte `bytes_per_row` rule for buffer copies does not apply
            // here — the mirror's own stride is what the data actually has.
            self.queue.write_texture(
                wgpu::TexelCopyTextureInfo {
                    texture: &self.texture,
                    mip_level: 0,
                    origin: wgpu::Origin3d {
                        x: rect.x,
                        y: rect.y,
                        z: 0,
                    },
                    aspect: wgpu::TextureAspect::All,
                },
                scanout.pixels(),
                wgpu::TexelCopyBufferLayout {
                    offset,
                    bytes_per_row: Some(stride as u32),
                    rows_per_image: Some(rect.height),
                },
                wgpu::Extent3d {
                    width: rect.width,
                    height: rect.height,
                    depth_or_array_layers: 1,
                },
            );
            self.stats.uploads += 1;
            self.stats.bytes_uploaded +=
                u64::from(rect.width) * u64::from(rect.height) * u64::from(BYTES_PER_PIXEL);
        }

        // The cursor plane, composited on the CPU (it is at most 256×256) and
        // written after the base rect so it always ends up on top. Re-written
        // on every upload rather than diffed: a full cursor is 256 KiB worst
        // case and typically 16 KiB, orders below one base frame. The mirror
        // marks the vacated area dirty on every cursor change, which is what
        // restores the base pixels underneath.
        if !with_cursor {
            return;
        }
        if let Some((rect, pixels)) = scanout.cursor_overlay() {
            if !rect.fits_within(self.texture_size.0, self.texture_size.1) {
                return;
            }
            self.queue.write_texture(
                wgpu::TexelCopyTextureInfo {
                    texture: &self.texture,
                    mip_level: 0,
                    origin: wgpu::Origin3d {
                        x: rect.x,
                        y: rect.y,
                        z: 0,
                    },
                    aspect: wgpu::TextureAspect::All,
                },
                &pixels,
                wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(rect.width * BYTES_PER_PIXEL),
                    rows_per_image: Some(rect.height),
                },
                wgpu::Extent3d {
                    width: rect.width,
                    height: rect.height,
                    depth_or_array_layers: 1,
                },
            );
            self.stats.uploads += 1;
            self.stats.bytes_uploaded +=
                u64::from(rect.width) * u64::from(rect.height) * u64::from(BYTES_PER_PIXEL);
        }
    }

    /// Returns false (and keeps the old texture) when the adapter cannot hold a
    /// texture that big — a guest mode change is not allowed to kill the window.
    fn reallocate_texture(&mut self, width: u32, height: u32) -> bool {
        let limit = self.device.limits().max_texture_dimension_2d;
        if width == 0 || height == 0 || width > limit || height > limit {
            tracing::error!(
                width,
                height,
                limit,
                "requested scanout exceeds the device texture limit; keeping the previous mode"
            );
            return false;
        }
        self.texture = create_scanout_texture(&self.device, width, height);
        self.bind_group = self.image.bind(
            &self.device,
            &self
                .texture
                .create_view(&wgpu::TextureViewDescriptor::default()),
        );
        self.texture_size = (width, height);
        self.stats.texture_allocations += 1;
        tracing::debug!(width, height, "scanout texture reallocated");
        true
    }

    /// Presents the scanout into `viewport`, recovering from a lost or outdated
    /// surface (backlog MVP-706).
    pub(crate) fn render(&mut self, viewport: Viewport) -> Result<(), DisplayError> {
        if !self.configured {
            self.stats.skipped += 1;
            return Ok(());
        }
        let shared = u64::from(self.shared.is_some());
        match self.frame(viewport) {
            Ok(()) => {
                self.stats.frames += 1;
                self.stats.shared_frames += shared;
                Ok(())
            }
            Err(err @ (wgpu::SurfaceError::Lost | wgpu::SurfaceError::Outdated)) => {
                tracing::debug!(%err, "surface needs reconfiguring");
                self.recover_surface();
                match self.frame(viewport) {
                    Ok(()) => {
                        self.stats.frames += 1;
                        self.stats.shared_frames += shared;
                        Ok(())
                    }
                    Err(err) => {
                        tracing::warn!(%err, "surface still unusable after reconfiguring");
                        self.stats.skipped += 1;
                        Ok(())
                    }
                }
            }
            Err(wgpu::SurfaceError::Timeout) => {
                tracing::debug!("surface acquisition timed out; dropping the frame");
                self.stats.skipped += 1;
                Ok(())
            }
            // OutOfMemory and any future variant: fatal for this window.
            Err(err) => Err(DisplayError::Surface(err)),
        }
    }

    fn frame(&mut self, viewport: Viewport) -> Result<(), wgpu::SurfaceError> {
        let acquire = Instant::now();
        let frame = self.surface.get_current_texture()?;
        let us = micros(acquire.elapsed());
        self.stats.acquire_us = self.stats.acquire_us.saturating_add(us);
        self.stats.acquire_max_us = self.stats.acquire_max_us.max(us);
        let view = frame
            .texture
            .create_view(&wgpu::TextureViewDescriptor::default());
        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("scanout-present"),
            });
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("scanout-present"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        // Letterbox bars (backlog MVP-705).
                        load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
            });
            // Clamp to the configured surface: winit can report a size the
            // surface has not been reconfigured for yet, and a viewport outside
            // the attachment is a validation error.
            let max_w = self.config.width;
            let max_h = self.config.height;
            if viewport.x < max_w && viewport.y < max_h {
                let width = viewport.width.min(max_w - viewport.x);
                let height = viewport.height.min(max_h - viewport.y);
                pass.set_viewport(
                    viewport.x as f32,
                    viewport.y as f32,
                    width as f32,
                    height as f32,
                    0.0,
                    1.0,
                );
                pass.set_scissor_rect(viewport.x, viewport.y, width, height);
                pass.set_pipeline(&self.image.pipeline);
                match &self.shared {
                    Some((_, bind)) => pass.set_bind_group(0, bind, &[]),
                    None => pass.set_bind_group(0, &self.bind_group, &[]),
                }
                pass.draw(0..3, 0..1);
                if self.shared.is_some() {
                    // No CPU mirror holds the pixels under the cursor: the GPU
                    // draws it over the shared frame.
                    self.cursor.draw(&mut pass);
                }
            }
        }
        self.queue.submit(std::iter::once(encoder.finish()));
        let present = Instant::now();
        frame.present();
        let us = micros(present.elapsed());
        self.stats.present_us = self.stats.present_us.saturating_add(us);
        self.stats.present_max_us = self.stats.present_max_us.max(us);
        Ok(())
    }
}

fn micros(d: Duration) -> u64 {
    u64::try_from(d.as_micros()).unwrap_or(u64::MAX)
}

/// The GPU objects one window needs, all tied to the same backend.
struct Gpu {
    surface: wgpu::Surface<'static>,
    adapter: wgpu::Adapter,
    device: wgpu::Device,
    queue: wgpu::Queue,
    /// Whether the device was opened to import a renderer's scanout buffers
    /// (ADR-0004, zero-copy presentation).
    shareable: bool,
}

/// Installs the window's shared presenter in `slot` when its device can be
/// one, and says either way.
fn install_presenter(
    slot: &SharedSlot,
    shareable: bool,
    device: &wgpu::Device,
    queue: &wgpu::Queue,
) {
    if !shareable {
        tracing::info!(
            "the window's GPU cannot import the renderer's scanout buffers; every frame of \
             the GPU desktop is read back and uploaded (the copy path)"
        );
        return;
    }
    #[cfg(windows)]
    match crate::gpu_scanout::GpuScanout::new(device.clone(), queue.clone()) {
        Ok(presenter) => slot.set(Some(Arc::new(presenter))),
        Err(why) => {
            tracing::warn!(%why, "no shared presenter; the GPU desktop takes the copy path")
        }
    }
    #[cfg(not(windows))]
    let _ = (slot, device, queue);
}

/// Brings up a backend for `window`.
///
/// `WGPU_BACKEND` wins if set. Otherwise Vulkan/DX12/Metal are tried first and
/// OpenGL only as a fallback: under WSLg both are present, Mesa's GL adapter
/// advertises itself first and then loses its device on creation, so "the
/// adapter exists" is not enough — a backend counts as usable only once a
/// device came out of it.
///
/// `shared` (a renderer that can hand the window its scanout buffers, ADR-0004)
/// puts a shareable **Vulkan** device first on Windows — a DX12 device cannot
/// import a Vulkan `OPAQUE_WIN32` allocation — and if that fails, the usual
/// order follows and the GPU desktop takes the copy path. A `WGPU_BACKEND`
/// the user set still wins; a Vulkan one is opened shareable too.
fn init_gpu(window: &Arc<Window>, shared: bool) -> Result<Gpu, DisplayError> {
    let shared = shared && cfg!(windows);
    let attempts = match wgpu::Backends::from_env() {
        Some(mask) => vec![(mask, shared && mask.contains(wgpu::Backends::VULKAN))],
        None if shared => vec![
            (wgpu::Backends::VULKAN, true),
            (wgpu::Backends::PRIMARY, false),
            (wgpu::Backends::SECONDARY, false),
        ],
        None => vec![
            (wgpu::Backends::PRIMARY, false),
            (wgpu::Backends::SECONDARY, false),
        ],
    };
    let mut last = None;
    for (backends, shareable) in attempts {
        match init_backend(window, backends, shareable) {
            Ok(gpu) => return Ok(gpu),
            Err(err) => {
                tracing::warn!(?backends, shareable, %err, "backend unusable; trying the next one");
                last = Some(err);
            }
        }
    }
    Err(last.unwrap_or(DisplayError::UnsupportedSurface))
}

/// The device of `adapter`: a shareable one (see [`init_gpu`]) when asked and
/// possible, else wgpu's ordinary one. `Err` only for a shareable device that
/// could not be had, so [`init_gpu`] moves on to the next backend.
fn request_device(
    adapter: &wgpu::Adapter,
    shareable: bool,
) -> Result<(wgpu::Device, wgpu::Queue), DisplayError> {
    #[cfg(windows)]
    if shareable {
        return crate::gpu_scanout::request_device(adapter, "entangled-display")
            .map_err(DisplayError::SharedDevice);
    }
    let _ = shareable;
    Ok(pollster::block_on(adapter.request_device(
        &wgpu::DeviceDescriptor {
            label: Some("entangled-display"),
            required_features: wgpu::Features::empty(),
            required_limits: adapter.limits(),
            ..Default::default()
        },
    ))?)
}

fn init_backend(
    window: &Arc<Window>,
    backends: wgpu::Backends,
    shareable: bool,
) -> Result<Gpu, DisplayError> {
    let mut descriptor = wgpu::InstanceDescriptor::from_env_or_default();
    descriptor.backends = backends;
    let instance = wgpu::Instance::new(&descriptor);
    let surface = instance.create_surface(Arc::clone(window))?;

    let mut options = wgpu::RequestAdapterOptions {
        power_preference: wgpu::PowerPreference::default(),
        force_fallback_adapter: false,
        compatible_surface: Some(&surface),
    };
    let adapter = match pollster::block_on(instance.request_adapter(&options)) {
        Ok(adapter) => adapter,
        Err(err) => {
            tracing::warn!(%err, "no hardware adapter; retrying with a software fallback");
            options.force_fallback_adapter = true;
            pollster::block_on(instance.request_adapter(&options))?
        }
    };
    let info = adapter.get_info();

    let (device, queue) = request_device(&adapter, shareable)?;
    // A validation error on a guest-driven copy must not abort the host.
    device.on_uncaptured_error(Box::new(|err| {
        tracing::error!(%err, "wgpu reported an uncaptured error");
    }));

    tracing::info!(
        adapter = %info.name,
        backend = ?info.backend,
        device_type = ?info.device_type,
        driver = %info.driver,
        driver_info = %info.driver_info,
        shareable,
        "wgpu adapter selected"
    );
    if info.device_type == wgpu::DeviceType::Cpu {
        tracing::warn!("software rasterizer in use; presentation will be slow");
    }
    Ok(Gpu {
        surface,
        adapter,
        device,
        queue,
        shareable,
    })
}

fn create_scanout_texture(device: &wgpu::Device, width: u32, height: u32) -> wgpu::Texture {
    device.create_texture(&wgpu::TextureDescriptor {
        label: Some("guest-scanout"),
        size: wgpu::Extent3d {
            width,
            height,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        // Bgra8Unorm matches virtio_gpu::FORMAT_B8G8R8A8_UNORM exactly, so
        // guest pixels are copied without any conversion.
        format: wgpu::TextureFormat::Bgra8Unorm,
        usage: wgpu::TextureUsages::TEXTURE_BINDING
            | wgpu::TextureUsages::COPY_DST
            | wgpu::TextureUsages::COPY_SRC,
        view_formats: &[],
    })
}

/// Prefers a non-sRGB surface format: the guest scanout holds display-ready
/// values, so an sRGB surface would re-encode them and wash the image out.
fn pick_surface_format(caps: &wgpu::SurfaceCapabilities) -> Option<wgpu::TextureFormat> {
    if caps.formats.contains(&wgpu::TextureFormat::Bgra8Unorm) {
        return Some(wgpu::TextureFormat::Bgra8Unorm);
    }
    if let Some(format) = caps.formats.iter().copied().find(|f| !f.is_srgb()) {
        return Some(format);
    }
    let fallback = caps.formats.first().copied();
    if let Some(format) = fallback {
        tracing::warn!(
            ?format,
            "no linear surface format; colors may be washed out"
        );
    }
    fallback
}
