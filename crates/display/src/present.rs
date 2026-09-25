//! The two draws that put a scanout on a target: the image (a full-screen
//! triangle sampling the scanout texture, `present.wgsl`) and — over a frame
//! the CPU mirror does not hold — the cursor plane (`cursor.wgsl`). Shared by
//! the window and by the shared presenter's off-screen screenshots, so both
//! draw exactly the same way.

use crate::scanout::CursorImage;

/// The image pipeline for one target format.
pub(crate) struct ImagePipeline {
    pub(crate) pipeline: wgpu::RenderPipeline,
    layout: wgpu::BindGroupLayout,
    sampler: wgpu::Sampler,
}

fn texture_entry(binding: u32) -> wgpu::BindGroupLayoutEntry {
    wgpu::BindGroupLayoutEntry {
        binding,
        visibility: wgpu::ShaderStages::FRAGMENT,
        ty: wgpu::BindingType::Texture {
            sample_type: wgpu::TextureSampleType::Float { filterable: true },
            view_dimension: wgpu::TextureViewDimension::D2,
            multisampled: false,
        },
        count: None,
    }
}

fn sampler_entry(binding: u32) -> wgpu::BindGroupLayoutEntry {
    wgpu::BindGroupLayoutEntry {
        binding,
        visibility: wgpu::ShaderStages::FRAGMENT,
        ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
        count: None,
    }
}

fn sampler(device: &wgpu::Device, filter: wgpu::FilterMode, label: &str) -> wgpu::Sampler {
    device.create_sampler(&wgpu::SamplerDescriptor {
        label: Some(label),
        address_mode_u: wgpu::AddressMode::ClampToEdge,
        address_mode_v: wgpu::AddressMode::ClampToEdge,
        address_mode_w: wgpu::AddressMode::ClampToEdge,
        mag_filter: filter,
        min_filter: filter,
        mipmap_filter: wgpu::FilterMode::Nearest,
        ..Default::default()
    })
}

impl ImagePipeline {
    /// The pipeline drawing into `format`, sampling with `filter` — linear for
    /// a window that scales (MVP-705), nearest for a 1:1 screenshot.
    pub(crate) fn new(
        device: &wgpu::Device,
        format: wgpu::TextureFormat,
        filter: wgpu::FilterMode,
    ) -> Self {
        let shader = device.create_shader_module(wgpu::include_wgsl!("present.wgsl"));
        let layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("scanout-bind-group-layout"),
            entries: &[texture_entry(0), sampler_entry(1)],
        });
        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("scanout-pipeline-layout"),
            bind_group_layouts: &[&layout],
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
        Self {
            pipeline,
            layout,
            sampler: sampler(device, filter, "scanout-sampler"),
        }
    }

    /// A bind group sampling `view`.
    pub(crate) fn bind(&self, device: &wgpu::Device, view: &wgpu::TextureView) -> wgpu::BindGroup {
        device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("scanout-bind-group"),
            layout: &self.layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(view),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::Sampler(&self.sampler),
                },
            ],
        })
    }
}

/// Where the cursor quad goes, in the clip space of the scanout viewport:
/// left, top, right, bottom, y up. `None` when it lies wholly off the
/// scanout (nothing to draw) or the scanout is empty.
#[must_use]
pub(crate) fn cursor_clip_rect(
    x: i64,
    y: i64,
    width: u32,
    height: u32,
    guest: (u32, u32),
) -> Option<[f32; 4]> {
    let (gw, gh) = (i64::from(guest.0), i64::from(guest.1));
    if gw == 0 || gh == 0 || width == 0 || height == 0 {
        return None;
    }
    let (right, bottom) = (x + i64::from(width), y + i64::from(height));
    if right <= 0 || bottom <= 0 || x >= gw || y >= gh {
        return None;
    }
    let to_x = |px: i64| (px as f64 / gw as f64 * 2.0 - 1.0) as f32;
    let to_y = |py: i64| (1.0 - py as f64 / gh as f64 * 2.0) as f32;
    Some([to_x(x), to_y(y), to_x(right), to_y(bottom)])
}

/// The cursor plane drawn by the GPU (see the module docs).
pub(crate) struct CursorOverlay {
    pipeline: wgpu::RenderPipeline,
    layout: wgpu::BindGroupLayout,
    sampler: wgpu::Sampler,
    placement: wgpu::Buffer,
    /// The uploaded image: its serial, and the bind group sampling it.
    image: Option<(u64, wgpu::BindGroup)>,
    /// Whether the last [`Self::prepare`] left something to draw.
    armed: bool,
}

impl CursorOverlay {
    pub(crate) fn new(device: &wgpu::Device, format: wgpu::TextureFormat) -> Self {
        let shader = device.create_shader_module(wgpu::include_wgsl!("cursor.wgsl"));
        let layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("cursor-bind-group-layout"),
            entries: &[
                texture_entry(0),
                sampler_entry(1),
                wgpu::BindGroupLayoutEntry {
                    binding: 2,
                    visibility: wgpu::ShaderStages::VERTEX,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
            ],
        });
        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("cursor-pipeline-layout"),
            bind_group_layouts: &[&layout],
            push_constant_ranges: &[],
        });
        // Premultiplied source-over, the cursor plane's own convention.
        let over = wgpu::BlendComponent {
            src_factor: wgpu::BlendFactor::One,
            dst_factor: wgpu::BlendFactor::OneMinusSrcAlpha,
            operation: wgpu::BlendOperation::Add,
        };
        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("cursor-overlay"),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs_cursor"),
                compilation_options: wgpu::PipelineCompilationOptions::default(),
                buffers: &[],
            },
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs_cursor"),
                compilation_options: wgpu::PipelineCompilationOptions::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format,
                    blend: Some(wgpu::BlendState {
                        color: over,
                        alpha: over,
                    }),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            multiview: None,
            cache: None,
        });
        let placement = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("cursor-placement"),
            size: 16,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        Self {
            pipeline,
            layout,
            sampler: sampler(device, wgpu::FilterMode::Linear, "cursor-sampler"),
            placement,
            image: None,
            armed: false,
        }
    }

    /// Get ready to draw `cursor` over a `guest`-sized scanout: upload the
    /// image when it changed, place the quad. Nothing is drawn after a
    /// `None`.
    pub(crate) fn prepare(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        cursor: Option<CursorImage<'_>>,
        guest: (u32, u32),
    ) {
        self.armed = false;
        let Some(cursor) = cursor else {
            return;
        };
        let Some(rect) = cursor_clip_rect(cursor.x, cursor.y, cursor.width, cursor.height, guest)
        else {
            return;
        };
        let expected = u64::from(cursor.width) * u64::from(cursor.height) * 4;
        if cursor.pixels.len() as u64 != expected {
            return;
        }
        if self.image.as_ref().map(|(serial, _)| *serial) != Some(cursor.serial) {
            let size = wgpu::Extent3d {
                width: cursor.width,
                height: cursor.height,
                depth_or_array_layers: 1,
            };
            let texture = device.create_texture(&wgpu::TextureDescriptor {
                label: Some("cursor-plane"),
                size,
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: wgpu::TextureFormat::Bgra8Unorm,
                usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
                view_formats: &[],
            });
            queue.write_texture(
                wgpu::TexelCopyTextureInfo {
                    texture: &texture,
                    mip_level: 0,
                    origin: wgpu::Origin3d::ZERO,
                    aspect: wgpu::TextureAspect::All,
                },
                cursor.pixels,
                wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(cursor.width * 4),
                    rows_per_image: Some(cursor.height),
                },
                size,
            );
            let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
            let bind = device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("cursor-bind-group"),
                layout: &self.layout,
                entries: &[
                    wgpu::BindGroupEntry {
                        binding: 0,
                        resource: wgpu::BindingResource::TextureView(&view),
                    },
                    wgpu::BindGroupEntry {
                        binding: 1,
                        resource: wgpu::BindingResource::Sampler(&self.sampler),
                    },
                    wgpu::BindGroupEntry {
                        binding: 2,
                        resource: self.placement.as_entire_binding(),
                    },
                ],
            });
            self.image = Some((cursor.serial, bind));
        }
        let mut bytes = [0u8; 16];
        for (chunk, value) in bytes.chunks_exact_mut(4).zip(rect) {
            chunk.copy_from_slice(&value.to_le_bytes());
        }
        queue.write_buffer(&self.placement, 0, &bytes);
        self.armed = true;
    }

    /// Draws the prepared cursor into `pass` (whose viewport is the
    /// scanout's), if there is one.
    pub(crate) fn draw(&self, pass: &mut wgpu::RenderPass<'_>) {
        let (true, Some((_, bind))) = (self.armed, self.image.as_ref()) else {
            return;
        };
        pass.set_pipeline(&self.pipeline);
        pass.set_bind_group(0, bind, &[]);
        pass.draw(0..6, 0..1);
    }
}

#[cfg(test)]
mod tests {
    use super::cursor_clip_rect;

    #[test]
    fn the_cursor_quad_maps_scanout_pixels_onto_clip_space() {
        // The whole scanout is clip space's whole square.
        assert_eq!(
            cursor_clip_rect(0, 0, 100, 50, (100, 50)),
            Some([-1.0, 1.0, 1.0, -1.0])
        );
        // A 24×24 cursor at the centre of a 1920×1080 scanout.
        let [l, t, r, b] = cursor_clip_rect(960, 540, 24, 24, (1920, 1080)).unwrap();
        assert!((l - 0.0).abs() < 1e-6 && (t - 0.0).abs() < 1e-6);
        assert!((r - 0.025).abs() < 1e-6 && (b + 24.0 / 540.0).abs() < 1e-6);
        // Hanging off the top-left edge: still drawn, clipped by the pass.
        assert!(cursor_clip_rect(-10, -10, 24, 24, (1920, 1080)).is_some());
        // Wholly off screen, or nothing to draw on.
        assert_eq!(cursor_clip_rect(-24, 0, 24, 24, (1920, 1080)), None);
        assert_eq!(cursor_clip_rect(1920, 0, 24, 24, (1920, 1080)), None);
        assert_eq!(cursor_clip_rect(0, 0, 24, 24, (0, 1080)), None);
        assert_eq!(cursor_clip_rect(0, 0, 0, 24, (1920, 1080)), None);
    }
}
