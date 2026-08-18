//! The `wgpu` presentation path (backlog MVP-702…706, 708).
//!
//! One device + queue + scanout texture per window, created once. Each frame
//! uploads only the dirty rect of the scanout mirror and draws it into the
//! letterboxed viewport with a full-screen triangle; the letterbox bars are the
//! attachment's black clear color.

use std::sync::Arc;
use std::time::{Duration, Instant};

use virtio_gpu::BYTES_PER_PIXEL;
use winit::window::Window;

use crate::scanout::Scanout;
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
        tracing::info!(
            fps = format_args!("{:.1}", frames as f64 / secs),
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
    pipeline: wgpu::RenderPipeline,
    bind_group_layout: wgpu::BindGroupLayout,
    sampler: wgpu::Sampler,
    texture: wgpu::Texture,
    bind_group: wgpu::BindGroup,
    /// Size of the currently allocated scanout texture.
    texture_size: (u32, u32),
    /// Scanout generation the texture was allocated for.
    generation: u64,
    stats: FrameStats,
}

impl Renderer {
    /// Creates the GPU state for `window` and a `guest_w`×`guest_h` scanout.
    ///
    /// Blocking on adapter/device creation happens here, at init time, and never
    /// again — the event loop must not block on GPU or guest state.
    pub(crate) fn new(
        window: Arc<Window>,
        guest_w: u32,
        guest_h: u32,
    ) -> Result<Self, DisplayError> {
        let Gpu {
            surface,
            adapter,
            device,
            queue,
        } = init_gpu(&window)?;

        let caps = surface.get_capabilities(&adapter);
        let format = pick_surface_format(&caps).ok_or(DisplayError::UnsupportedSurface)?;
        let alpha_mode = caps
            .alpha_modes
            .first()
            .copied()
            .unwrap_or(wgpu::CompositeAlphaMode::Auto);
        let size = window.inner_size();
        let config = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format,
            width: size.width.max(1),
            height: size.height.max(1),
            present_mode: wgpu::PresentMode::AutoVsync,
            desired_maximum_frame_latency: 2,
            alpha_mode,
            view_formats: Vec::new(),
        };

        let shader = device.create_shader_module(wgpu::include_wgsl!("present.wgsl"));
        let bind_group_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("scanout-bind-group-layout"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
            ],
        });
        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("scanout-pipeline-layout"),
            bind_group_layouts: &[&bind_group_layout],
            push_constant_ranges: &[],
        });
        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("scanout-present"),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs_main"),
                compilation_options: wgpu::PipelineCompilationOptions::default(),
                buffers: &[],
            },
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs_main"),
                compilation_options: wgpu::PipelineCompilationOptions::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format,
                    blend: None,
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            multiview: None,
            cache: None,
        });
        // Linear filtering so a scaled scanout is smooth (MVP-705).
        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("scanout-sampler"),
            address_mode_u: wgpu::AddressMode::ClampToEdge,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            address_mode_w: wgpu::AddressMode::ClampToEdge,
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            mipmap_filter: wgpu::FilterMode::Nearest,
            ..Default::default()
        });

        let texture = create_scanout_texture(&device, guest_w, guest_h);
        let bind_group = create_bind_group(&device, &bind_group_layout, &texture, &sampler);

        let mut renderer = Self {
            surface,
            adapter,
            device,
            queue,
            config,
            configured: false,
            pipeline,
            bind_group_layout,
            sampler,
            texture,
            bind_group,
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

    /// Reconfigures the surface for a new window size. A zero-sized window
    /// (minimized) leaves the surface unconfigured and presenting disabled
    /// (backlog MVP-706).
    pub(crate) fn resize(&mut self, width: u32, height: u32) {
        if width == 0 || height == 0 {
            self.configured = false;
            tracing::debug!("window has zero area; presenting suspended");
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
        self.stats.surface_recoveries += 1;
        self.surface.configure(&self.device, &self.config);
        self.configured = true;
    }

    /// Uploads the scanout's dirty rect into the texture, reallocating first if
    /// the guest changed resolution (backlog MVP-703/704).
    pub(crate) fn upload(&mut self, scanout: &mut Scanout) {
        let (guest_w, guest_h) = scanout.size();
        if scanout.generation() != self.generation || (guest_w, guest_h) != self.texture_size {
            if !self.reallocate_texture(guest_w, guest_h) {
                return;
            }
            self.generation = scanout.generation();
            scanout.mark_all_dirty();
        }
        let Some(rect) = scanout.take_dirty() else {
            return;
        };
        if !rect.fits_within(self.texture_size.0, self.texture_size.1) {
            tracing::warn!(?rect, "dirty rect outside the texture; skipping upload");
            return;
        }
        let stride = scanout.stride();
        let offset = rect.y as u64 * stride as u64 + rect.x as u64 * u64::from(BYTES_PER_PIXEL);
        // `write_texture` from CPU memory pads rows internally, so the 256-byte
        // `bytes_per_row` rule for buffer copies does not apply here — the
        // mirror's own stride is what the data actually has.
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
        self.bind_group = create_bind_group(
            &self.device,
            &self.bind_group_layout,
            &self.texture,
            &self.sampler,
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
        match self.frame(viewport) {
            Ok(()) => {
                self.stats.frames += 1;
                Ok(())
            }
            Err(err @ (wgpu::SurfaceError::Lost | wgpu::SurfaceError::Outdated)) => {
                tracing::debug!(%err, "surface needs reconfiguring");
                self.recover_surface();
                match self.frame(viewport) {
                    Ok(()) => {
                        self.stats.frames += 1;
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
        let frame = self.surface.get_current_texture()?;
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
                pass.set_pipeline(&self.pipeline);
                pass.set_bind_group(0, &self.bind_group, &[]);
                pass.draw(0..3, 0..1);
            }
        }
        self.queue.submit(std::iter::once(encoder.finish()));
        frame.present();
        Ok(())
    }
}

/// The GPU objects one window needs, all tied to the same backend.
struct Gpu {
    surface: wgpu::Surface<'static>,
    adapter: wgpu::Adapter,
    device: wgpu::Device,
    queue: wgpu::Queue,
}

/// Brings up a backend for `window`.
///
/// `WGPU_BACKEND` wins if set. Otherwise Vulkan/DX12/Metal are tried first and
/// OpenGL only as a fallback: under WSLg both are present, Mesa's GL adapter
/// advertises itself first and then loses its device on creation, so "the
/// adapter exists" is not enough — a backend counts as usable only once a
/// device came out of it.
fn init_gpu(window: &Arc<Window>) -> Result<Gpu, DisplayError> {
    let attempts = match wgpu::Backends::from_env() {
        Some(mask) => vec![mask],
        None => vec![wgpu::Backends::PRIMARY, wgpu::Backends::SECONDARY],
    };
    let mut last = None;
    for backends in attempts {
        match init_backend(window, backends) {
            Ok(gpu) => return Ok(gpu),
            Err(err) => {
                tracing::warn!(?backends, %err, "backend unusable; trying the next one");
                last = Some(err);
            }
        }
    }
    Err(last.unwrap_or(DisplayError::UnsupportedSurface))
}

fn init_backend(window: &Arc<Window>, backends: wgpu::Backends) -> Result<Gpu, DisplayError> {
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

    let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
        label: Some("entangled-display"),
        required_features: wgpu::Features::empty(),
        required_limits: adapter.limits(),
        ..Default::default()
    }))?;
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

fn create_bind_group(
    device: &wgpu::Device,
    layout: &wgpu::BindGroupLayout,
    texture: &wgpu::Texture,
    sampler: &wgpu::Sampler,
) -> wgpu::BindGroup {
    let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
    device.create_bind_group(&wgpu::BindGroupDescriptor {
        label: Some("scanout-bind-group"),
        layout,
        entries: &[
            wgpu::BindGroupEntry {
                binding: 0,
                resource: wgpu::BindingResource::TextureView(&view),
            },
            wgpu::BindGroupEntry {
                binding: 1,
                resource: wgpu::BindingResource::Sampler(sampler),
            },
        ],
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
