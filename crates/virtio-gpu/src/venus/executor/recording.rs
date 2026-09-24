//! Stage 5b.2 for the tests: the commands Mesa's venus sends to create
//! pipelines, descriptors, render passes and command buffers, to record and
//! to submit — built as Mesa fills them in — and the two ways a recording
//! reaches the host: directly in the ring, or, past the ring's direct size,
//! through `vkExecuteCommandStreamsMESA` naming a stream blob
//! (`vn_ring.c:500-530`). Used by the fake-host tests and the real-GPU ones
//! alike.

use crate::venus::protocol::*;
use crate::venus::transport::Opcode;
use crate::venus::wire::{CommandHeader, Encoder};

use super::harness::{async_bytes, Harness, Outcome, CTX};
use super::host::HostVulkan;

/// Mesa's primary ring's direct size: 128 KiB >> 4 (`vn_instance.c:149-157`).
pub const DIRECT_SIZE: usize = 8192;
/// The stream blob the harness uploads large submissions into.
pub const STREAM_RES: u32 = 9;
/// Its size: Mesa's `ring->upload` is at least 1 MiB.
pub const STREAM_BYTES: u64 = 4 << 20;

pub const SHADER: u64 = 0xb0;
pub const SET_LAYOUT: u64 = 0xb1;
pub const DESC_POOL: u64 = 0xb2;
pub const SET: u64 = 0xb3;
pub const PIPELINE_LAYOUT: u64 = 0xb4;
pub const PIPELINE: u64 = 0xb5;
pub const RENDER_PASS: u64 = 0xb6;
pub const FRAMEBUFFER: u64 = 0xb7;
pub const CB: u64 = 0xc0;
pub const FENCE: u64 = 0xd0;

/// `VK_STRUCTURE_TYPE`-free helpers for the common values.
pub const STAGE_TRANSFER: u32 = 0x1000;
pub const STAGE_HOST: u32 = 0x4000;
pub const STAGE_COMPUTE: u32 = 0x800;
pub const STAGE_COLOR_OUTPUT: u32 = 0x400;
pub const STAGE_ALL_COMMANDS: u32 = 0x1_0000;
pub const ACCESS_TRANSFER_READ: u32 = 0x800;
pub const ACCESS_TRANSFER_WRITE: u32 = 0x1000;
pub const ACCESS_HOST_READ: u32 = 0x2000;
pub const ACCESS_SHADER_WRITE: u32 = 0x40;
pub const ACCESS_COLOR_WRITE: u32 = 0x100;
pub const QUEUE_FAMILY_IGNORED: u32 = u32::MAX;
pub const WHOLE_SIZE: u64 = u64::MAX;

impl<H: HostVulkan> Harness<H> {
    /// A recording as Mesa sends it: every command encoded without a reply,
    /// back to back, in one submission — straight into the ring when it
    /// fits the direct size, else uploaded into the stream blob and named
    /// by one `vkExecuteCommandStreamsMESA`.
    pub fn submit_recording(&mut self, commands: &[Command<'_>]) -> Outcome {
        let bytes: Vec<u8> = commands.iter().flat_map(|c| async_bytes(c)).collect();
        if bytes.len() <= DIRECT_SIZE {
            self.submit(&bytes)
        } else {
            self.submit_indirect(&[&bytes])
        }
    }

    /// `streams` uploaded one after another into the stream blob, and one
    /// `vkExecuteCommandStreamsMESA` naming each, as `vn_ring_submit_command`
    /// does past the direct size: no reply positions, no dependencies.
    pub fn submit_indirect(&mut self, streams: &[&[u8]]) -> Outcome {
        let pages = self.stream_blob();
        let mut ranges = Vec::new();
        let mut at = 0u64;
        for stream in streams {
            pages
                .write_bytes(at, stream)
                .expect("the stream fits the blob");
            ranges.push((STREAM_RES, at, stream.len() as u64));
            at += (stream.len() as u64).next_multiple_of(64);
        }
        self.submit(&execute_streams(&ranges))
    }

    /// The stream blob's pages, creating it the first time.
    pub fn stream_blob(&mut self) -> std::sync::Arc<crate::venus::shmem::RingPages> {
        if let Some(pages) = self.renderer.blob_pages(STREAM_RES) {
            return pages;
        }
        self.create_blob(CTX, STREAM_RES, STREAM_BYTES);
        self.renderer
            .blob_pages(STREAM_RES)
            .expect("the stream blob was just made")
    }
}

/// `vkExecuteCommandStreamsMESA` naming `(resource, offset, size)` ranges,
/// as `vn_ring.c:494-529` encodes it.
#[must_use]
pub fn execute_streams(ranges: &[(u32, u64, u64)]) -> Vec<u8> {
    let mut enc = Encoder::new();
    enc.command_header(CommandHeader {
        opcode: Opcode::ExecuteCommandStreams.as_u32(),
        flags: 0,
    })
    .expect("encode");
    enc.u32(ranges.len() as u32).expect("encode");
    enc.array_size(ranges.len() as u64).expect("encode");
    for (resource, offset, size) in ranges {
        enc.u32(*resource).expect("encode");
        enc.size(*offset).expect("encode");
        enc.size(*size).expect("encode");
    }
    enc.array_size(0).expect("encode"); // pReplyPositions: null
    enc.u32(0).expect("encode"); // dependencyCount
    enc.array_size(0).expect("encode"); // pDependencies: null
    enc.flags(0).expect("encode");
    enc.finish().expect("encode")
}

// ------------------------------------------------------------ objects

pub fn create_shader_module(device: u64, id: u64, code: &[u32]) -> Command<'static> {
    Command::CreateShaderModule(CreateShaderModuleArgs {
        device: VkDevice(device),
        p_create_info: Some(VkShaderModuleCreateInfo {
            flags: 0,
            code_size: code.len() as u64 * 4,
            p_code: Some(code.to_vec()),
        }),
        p_shader_module: Some(VkShaderModule(id)),
        ret: 0,
    })
}

/// One `STORAGE_BUFFER` binding, visible to `stages`.
pub fn create_storage_set_layout(device: u64, id: u64, stages: u32) -> Command<'static> {
    Command::CreateDescriptorSetLayout(CreateDescriptorSetLayoutArgs {
        device: VkDevice(device),
        p_create_info: Some(VkDescriptorSetLayoutCreateInfo {
            p_next: Vec::new(),
            flags: 0,
            binding_count: 1,
            p_bindings: Some(vec![VkDescriptorSetLayoutBinding {
                binding: 0,
                descriptor_type: 7,
                descriptor_count: 1,
                stage_flags: stages,
                p_immutable_samplers: None,
            }]),
        }),
        p_set_layout: Some(VkDescriptorSetLayout(id)),
        ret: 0,
    })
}

pub fn create_descriptor_pool(device: u64, id: u64, flags: u32) -> Command<'static> {
    Command::CreateDescriptorPool(CreateDescriptorPoolArgs {
        device: VkDevice(device),
        p_create_info: Some(VkDescriptorPoolCreateInfo {
            p_next: Vec::new(),
            flags,
            max_sets: 4,
            pool_size_count: 1,
            p_pool_sizes: Some(vec![VkDescriptorPoolSize {
                type_: 7,
                descriptor_count: 4,
            }]),
        }),
        p_descriptor_pool: Some(VkDescriptorPool(id)),
        ret: 0,
    })
}

pub fn allocate_sets(device: u64, pool: u64, layout: u64, ids: &[u64]) -> Command<'static> {
    Command::AllocateDescriptorSets(AllocateDescriptorSetsArgs {
        device: VkDevice(device),
        p_allocate_info: Some(VkDescriptorSetAllocateInfo {
            p_next: Vec::new(),
            descriptor_pool: VkDescriptorPool(pool),
            descriptor_set_count: ids.len() as u32,
            p_set_layouts: Some(vec![VkDescriptorSetLayout(layout); ids.len()]),
        }),
        p_descriptor_sets: Some(ids.iter().copied().map(VkDescriptorSet).collect()),
        ret: 0,
    })
}

/// One storage-buffer write of `buffer`, whole.
pub fn write_storage(device: u64, set: u64, buffer: u64) -> Command<'static> {
    Command::UpdateDescriptorSets(UpdateDescriptorSetsArgs {
        device: VkDevice(device),
        descriptor_write_count: 1,
        p_descriptor_writes: Some(vec![VkWriteDescriptorSet {
            p_next: Vec::new(),
            dst_set: VkDescriptorSet(set),
            dst_binding: 0,
            dst_array_element: 0,
            descriptor_count: 1,
            descriptor_type: 7,
            p_image_info: None,
            p_buffer_info: Some(vec![VkDescriptorBufferInfo {
                buffer: VkBuffer(buffer),
                offset: 0,
                range: WHOLE_SIZE,
            }]),
            p_texel_buffer_view: None,
        }]),
        descriptor_copy_count: 0,
        p_descriptor_copies: None,
    })
}

pub fn create_pipeline_layout(device: u64, id: u64, set_layouts: &[u64]) -> Command<'static> {
    Command::CreatePipelineLayout(CreatePipelineLayoutArgs {
        device: VkDevice(device),
        p_create_info: Some(VkPipelineLayoutCreateInfo {
            flags: 0,
            set_layout_count: set_layouts.len() as u32,
            p_set_layouts: (!set_layouts.is_empty()).then(|| {
                set_layouts
                    .iter()
                    .copied()
                    .map(VkDescriptorSetLayout)
                    .collect()
            }),
            push_constant_range_count: 0,
            p_push_constant_ranges: None,
        }),
        p_pipeline_layout: Some(VkPipelineLayout(id)),
        ret: 0,
    })
}

fn stage(stage: i32, module: u64, name: &'static [u8]) -> VkPipelineShaderStageCreateInfo<'static> {
    VkPipelineShaderStageCreateInfo {
        p_next: Vec::new(),
        flags: 0,
        stage,
        module: VkShaderModule(module),
        p_name: Some(name),
        p_specialization_info: None,
    }
}

pub fn create_compute_pipeline(device: u64, id: u64, module: u64, layout: u64) -> Command<'static> {
    Command::CreateComputePipelines(CreateComputePipelinesArgs {
        device: VkDevice(device),
        pipeline_cache: VkPipelineCache(0),
        create_info_count: 1,
        p_create_infos: Some(vec![VkComputePipelineCreateInfo {
            p_next: Vec::new(),
            flags: 0,
            stage: stage(0x20, module, b"main"),
            layout: VkPipelineLayout(layout),
            base_pipeline_handle: VkPipeline(0),
            base_pipeline_index: -1,
        }]),
        p_pipelines: Some(vec![VkPipeline(id)]),
        ret: 0,
    })
}

/// vk-smoke's check-6 render pass: one RGBA8 colour attachment, cleared,
/// stored, left in `TRANSFER_SRC_OPTIMAL`.
pub fn create_render_pass(device: u64, id: u64, format: i32) -> Command<'static> {
    Command::CreateRenderPass(CreateRenderPassArgs {
        device: VkDevice(device),
        p_create_info: Some(VkRenderPassCreateInfo {
            p_next: Vec::new(),
            flags: 0,
            attachment_count: 1,
            p_attachments: Some(vec![VkAttachmentDescription {
                flags: 0,
                format,
                samples: 1,
                load_op: 1,  // CLEAR
                store_op: 0, // STORE
                stencil_load_op: 2,
                stencil_store_op: 1,
                initial_layout: 0,
                final_layout: 6, // TRANSFER_SRC_OPTIMAL
            }]),
            subpass_count: 1,
            p_subpasses: Some(vec![VkSubpassDescription {
                flags: 0,
                pipeline_bind_point: 0,
                input_attachment_count: 0,
                p_input_attachments: None,
                color_attachment_count: 1,
                p_color_attachments: Some(vec![VkAttachmentReference {
                    attachment: 0,
                    layout: 2, // COLOR_ATTACHMENT_OPTIMAL
                }]),
                p_resolve_attachments: None,
                p_depth_stencil_attachment: None,
                preserve_attachment_count: 0,
                p_preserve_attachments: None,
            }]),
            dependency_count: 2,
            p_dependencies: Some(vec![
                VkSubpassDependency {
                    src_subpass: u32::MAX,
                    dst_subpass: 0,
                    src_stage_mask: STAGE_COLOR_OUTPUT,
                    dst_stage_mask: STAGE_COLOR_OUTPUT,
                    src_access_mask: 0,
                    dst_access_mask: ACCESS_COLOR_WRITE,
                    dependency_flags: 0,
                },
                VkSubpassDependency {
                    src_subpass: 0,
                    dst_subpass: u32::MAX,
                    src_stage_mask: STAGE_COLOR_OUTPUT,
                    dst_stage_mask: STAGE_TRANSFER,
                    src_access_mask: ACCESS_COLOR_WRITE,
                    dst_access_mask: ACCESS_TRANSFER_READ,
                    dependency_flags: 0,
                },
            ]),
        }),
        p_render_pass: Some(VkRenderPass(id)),
        ret: 0,
    })
}

pub fn create_framebuffer(
    device: u64,
    id: u64,
    render_pass: u64,
    view: u64,
    size: u32,
) -> Command<'static> {
    Command::CreateFramebuffer(CreateFramebufferArgs {
        device: VkDevice(device),
        p_create_info: Some(VkFramebufferCreateInfo {
            p_next: Vec::new(),
            flags: 0,
            render_pass: VkRenderPass(render_pass),
            attachment_count: 1,
            p_attachments: Some(vec![VkImageView(view)]),
            width: size,
            height: size,
            layers: 1,
        }),
        p_framebuffer: Some(VkFramebuffer(id)),
        ret: 0,
    })
}

/// vk-smoke's check-6 pipeline: `vs_main`/`fs_main` of one module, a
/// position (vec2) + barycentric (vec3) vertex, one viewport of `size`.
pub fn create_triangle_pipeline(
    device: u64,
    id: u64,
    module: u64,
    layout: u64,
    render_pass: u64,
    size: u32,
) -> Command<'static> {
    let s = size as f32;
    Command::CreateGraphicsPipelines(CreateGraphicsPipelinesArgs {
        device: VkDevice(device),
        pipeline_cache: VkPipelineCache(0),
        create_info_count: 1,
        p_create_infos: Some(vec![VkGraphicsPipelineCreateInfo {
            p_next: Vec::new(),
            flags: 0,
            stage_count: 2,
            p_stages: Some(vec![
                stage(0x1, module, b"vs_main"),
                stage(0x10, module, b"fs_main"),
            ]),
            p_vertex_input_state: Some(VkPipelineVertexInputStateCreateInfo {
                p_next: Vec::new(),
                flags: 0,
                vertex_binding_description_count: 1,
                p_vertex_binding_descriptions: Some(vec![VkVertexInputBindingDescription {
                    binding: 0,
                    stride: 20,
                    input_rate: 0,
                }]),
                vertex_attribute_description_count: 2,
                p_vertex_attribute_descriptions: Some(vec![
                    VkVertexInputAttributeDescription {
                        location: 0,
                        binding: 0,
                        format: 103, // R32G32_SFLOAT
                        offset: 0,
                    },
                    VkVertexInputAttributeDescription {
                        location: 1,
                        binding: 0,
                        format: 106, // R32G32B32_SFLOAT
                        offset: 8,
                    },
                ]),
            }),
            p_input_assembly_state: Some(VkPipelineInputAssemblyStateCreateInfo {
                flags: 0,
                topology: 3, // TRIANGLE_LIST
                primitive_restart_enable: 0,
            }),
            p_tessellation_state: None,
            p_viewport_state: Some(VkPipelineViewportStateCreateInfo {
                p_next: Vec::new(),
                flags: 0,
                viewport_count: 1,
                p_viewports: Some(vec![VkViewport {
                    x: 0.0,
                    y: 0.0,
                    width: s,
                    height: s,
                    min_depth: 0.0,
                    max_depth: 1.0,
                }]),
                scissor_count: 1,
                p_scissors: Some(vec![VkRect2D {
                    offset: VkOffset2D { x: 0, y: 0 },
                    extent: VkExtent2D {
                        width: size,
                        height: size,
                    },
                }]),
            }),
            p_rasterization_state: Some(VkPipelineRasterizationStateCreateInfo {
                polygon_mode: 0,
                cull_mode: 0,
                front_face: 0,
                line_width: 1.0,
                ..Default::default()
            }),
            p_multisample_state: Some(VkPipelineMultisampleStateCreateInfo {
                rasterization_samples: 1,
                ..Default::default()
            }),
            p_depth_stencil_state: None,
            p_color_blend_state: Some(VkPipelineColorBlendStateCreateInfo {
                p_next: Vec::new(),
                flags: 0,
                logic_op_enable: 0,
                logic_op: 0,
                attachment_count: 1,
                p_attachments: Some(vec![VkPipelineColorBlendAttachmentState {
                    color_write_mask: 0xf,
                    ..Default::default()
                }]),
                blend_constants: [0.0; 4],
            }),
            p_dynamic_state: None,
            layout: VkPipelineLayout(layout),
            render_pass: VkRenderPass(render_pass),
            subpass: 0,
            base_pipeline_handle: VkPipeline(0),
            base_pipeline_index: -1,
        }]),
        p_pipelines: Some(vec![VkPipeline(id)]),
        ret: 0,
    })
}

pub fn create_image_view(device: u64, id: u64, image: u64, format: i32) -> Command<'static> {
    Command::CreateImageView(CreateImageViewArgs {
        device: VkDevice(device),
        p_create_info: Some(VkImageViewCreateInfo {
            p_next: Vec::new(),
            flags: 0,
            image: VkImage(image),
            view_type: 1,
            format,
            components: VkComponentMapping::default(),
            subresource_range: color_range(),
        }),
        p_view: Some(VkImageView(id)),
        ret: 0,
    })
}

pub fn color_range() -> VkImageSubresourceRange {
    VkImageSubresourceRange {
        aspect_mask: 1,
        base_mip_level: 0,
        level_count: 1,
        base_array_layer: 0,
        layer_count: 1,
    }
}

pub fn create_fence(device: u64, id: u64, signalled: bool) -> Command<'static> {
    Command::CreateFence(CreateFenceArgs {
        device: VkDevice(device),
        p_create_info: Some(VkFenceCreateInfo {
            p_next: Vec::new(),
            flags: u32::from(signalled),
        }),
        p_fence: Some(VkFence(id)),
        ret: 0,
    })
}

pub fn destroy_fence(device: u64, id: u64) -> Command<'static> {
    Command::DestroyFence(DestroyFenceArgs {
        device: VkDevice(device),
        fence: VkFence(id),
    })
}

// ----------------------------------------------------- command buffers

pub fn allocate_cbs(device: u64, pool: u64, ids: &[u64], secondary: bool) -> Command<'static> {
    Command::AllocateCommandBuffers(AllocateCommandBuffersArgs {
        device: VkDevice(device),
        p_allocate_info: Some(VkCommandBufferAllocateInfo {
            command_pool: VkCommandPool(pool),
            level: i32::from(secondary),
            command_buffer_count: ids.len() as u32,
        }),
        p_command_buffers: Some(ids.iter().copied().map(VkCommandBuffer).collect()),
        ret: 0,
    })
}

pub fn begin(cb: u64) -> Command<'static> {
    Command::BeginCommandBuffer(BeginCommandBufferArgs {
        command_buffer: VkCommandBuffer(cb),
        p_begin_info: Some(VkCommandBufferBeginInfo {
            p_next: Vec::new(),
            flags: 0x1, // ONE_TIME_SUBMIT
            p_inheritance_info: None,
        }),
        ret: 0,
    })
}

pub fn end(cb: u64) -> Command<'static> {
    Command::EndCommandBuffer(EndCommandBufferArgs {
        command_buffer: VkCommandBuffer(cb),
        ret: 0,
    })
}

pub fn fill(cb: u64, buffer: u64, offset: u64, size: u64, data: u32) -> Command<'static> {
    Command::CmdFillBuffer(CmdFillBufferArgs {
        command_buffer: VkCommandBuffer(cb),
        dst_buffer: VkBuffer(buffer),
        dst_offset: offset,
        size,
        data,
    })
}

pub fn update(cb: u64, buffer: u64, offset: u64, data: &'static [u8]) -> Command<'static> {
    Command::CmdUpdateBuffer(CmdUpdateBufferArgs {
        command_buffer: VkCommandBuffer(cb),
        dst_buffer: VkBuffer(buffer),
        dst_offset: offset,
        data_size: data.len() as u64,
        p_data: Some(data),
    })
}

pub fn copy_buffer(cb: u64, src: u64, dst: u64, regions: &[(u64, u64, u64)]) -> Command<'static> {
    Command::CmdCopyBuffer(CmdCopyBufferArgs {
        command_buffer: VkCommandBuffer(cb),
        src_buffer: VkBuffer(src),
        dst_buffer: VkBuffer(dst),
        region_count: regions.len() as u32,
        p_regions: Some(
            regions
                .iter()
                .map(|(s, d, n)| VkBufferCopy {
                    src_offset: *s,
                    dst_offset: *d,
                    size: *n,
                })
                .collect(),
        ),
    })
}

/// A whole-buffer barrier from `(stage, access)` to `(stage, access)`.
pub fn buffer_barrier(cb: u64, buffer: u64, src: (u32, u32), dst: (u32, u32)) -> Command<'static> {
    Command::CmdPipelineBarrier(CmdPipelineBarrierArgs {
        command_buffer: VkCommandBuffer(cb),
        src_stage_mask: src.0,
        dst_stage_mask: dst.0,
        dependency_flags: 0,
        memory_barrier_count: 0,
        p_memory_barriers: None,
        buffer_memory_barrier_count: 1,
        p_buffer_memory_barriers: Some(vec![VkBufferMemoryBarrier {
            p_next: Vec::new(),
            src_access_mask: src.1,
            dst_access_mask: dst.1,
            src_queue_family_index: QUEUE_FAMILY_IGNORED,
            dst_queue_family_index: QUEUE_FAMILY_IGNORED,
            buffer: VkBuffer(buffer),
            offset: 0,
            size: WHOLE_SIZE,
        }]),
        image_memory_barrier_count: 0,
        p_image_memory_barriers: None,
    })
}

pub fn bind_pipeline(cb: u64, bind_point: i32, pipeline: u64) -> Command<'static> {
    Command::CmdBindPipeline(CmdBindPipelineArgs {
        command_buffer: VkCommandBuffer(cb),
        pipeline_bind_point: bind_point,
        pipeline: VkPipeline(pipeline),
    })
}

pub fn bind_sets(cb: u64, bind_point: i32, layout: u64, sets: &[u64]) -> Command<'static> {
    Command::CmdBindDescriptorSets(CmdBindDescriptorSetsArgs {
        command_buffer: VkCommandBuffer(cb),
        pipeline_bind_point: bind_point,
        layout: VkPipelineLayout(layout),
        first_set: 0,
        descriptor_set_count: sets.len() as u32,
        p_descriptor_sets: Some(sets.iter().copied().map(VkDescriptorSet).collect()),
        dynamic_offset_count: 0,
        p_dynamic_offsets: None,
    })
}

pub fn dispatch(cb: u64, x: u32) -> Command<'static> {
    Command::CmdDispatch(CmdDispatchArgs {
        command_buffer: VkCommandBuffer(cb),
        group_count_x: x,
        group_count_y: 1,
        group_count_z: 1,
    })
}

pub fn begin_render_pass(
    cb: u64,
    render_pass: u64,
    framebuffer: u64,
    size: u32,
    clear: [f32; 4],
) -> Command<'static> {
    Command::CmdBeginRenderPass(CmdBeginRenderPassArgs {
        command_buffer: VkCommandBuffer(cb),
        p_render_pass_begin: Some(VkRenderPassBeginInfo {
            p_next: Vec::new(),
            render_pass: VkRenderPass(render_pass),
            framebuffer: VkFramebuffer(framebuffer),
            render_area: VkRect2D {
                offset: VkOffset2D { x: 0, y: 0 },
                extent: VkExtent2D {
                    width: size,
                    height: size,
                },
            },
            clear_value_count: 1,
            p_clear_values: Some(vec![VkClearValue::Color(VkClearColorValue::Float32(clear))]),
        }),
        contents: 0,
    })
}

pub fn bind_vertex_buffer(cb: u64, buffer: u64) -> Command<'static> {
    Command::CmdBindVertexBuffers(CmdBindVertexBuffersArgs {
        command_buffer: VkCommandBuffer(cb),
        first_binding: 0,
        binding_count: 1,
        p_buffers: Some(vec![VkBuffer(buffer)]),
        p_offsets: Some(vec![0]),
    })
}

pub fn draw(cb: u64, vertices: u32) -> Command<'static> {
    Command::CmdDraw(CmdDrawArgs {
        command_buffer: VkCommandBuffer(cb),
        vertex_count: vertices,
        instance_count: 1,
        first_vertex: 0,
        first_instance: 0,
    })
}

pub fn end_render_pass(cb: u64) -> Command<'static> {
    Command::CmdEndRenderPass(CmdEndRenderPassArgs {
        command_buffer: VkCommandBuffer(cb),
    })
}

/// The whole of a `size`² RGBA8 image, tightly packed into `buffer`.
pub fn copy_image_to_buffer(cb: u64, image: u64, buffer: u64, size: u32) -> Command<'static> {
    Command::CmdCopyImageToBuffer(CmdCopyImageToBufferArgs {
        command_buffer: VkCommandBuffer(cb),
        src_image: VkImage(image),
        src_image_layout: 6,
        dst_buffer: VkBuffer(buffer),
        region_count: 1,
        p_regions: Some(vec![VkBufferImageCopy {
            buffer_offset: 0,
            buffer_row_length: 0,
            buffer_image_height: 0,
            image_subresource: VkImageSubresourceLayers {
                aspect_mask: 1,
                mip_level: 0,
                base_array_layer: 0,
                layer_count: 1,
            },
            image_offset: VkOffset3D { x: 0, y: 0, z: 0 },
            image_extent: VkExtent3D {
                width: size,
                height: size,
                depth: 1,
            },
        }]),
    })
}

// --------------------------------------------------------- submission

/// `vkQueueSubmit` of one batch of `cbs`, fenced by `fence` (0: none), as
/// `vn_queue_submit` sends it: asynchronously.
pub fn queue_submit(queue: u64, cbs: &[u64], fence: u64) -> Command<'static> {
    Command::QueueSubmit(QueueSubmitArgs {
        queue: VkQueue(queue),
        submit_count: 1,
        p_submits: Some(vec![VkSubmitInfo {
            p_next: Vec::new(),
            wait_semaphore_count: 0,
            p_wait_semaphores: None,
            p_wait_dst_stage_mask: None,
            command_buffer_count: cbs.len() as u32,
            p_command_buffers: Some(cbs.iter().copied().map(VkCommandBuffer).collect()),
            signal_semaphore_count: 0,
            p_signal_semaphores: None,
        }]),
        fence: VkFence(fence),
        ret: 0,
    })
}

/// The `vkWaitForFences(1, &fence, VK_TRUE, UINT64_MAX)` Mesa sends,
/// asynchronously, once the feedback slot reads signalled.
pub fn wait_fences(device: u64, fences: &[u64], timeout: u64) -> Command<'static> {
    Command::WaitForFences(WaitForFencesArgs {
        device: VkDevice(device),
        fence_count: fences.len() as u32,
        p_fences: Some(fences.iter().copied().map(VkFence).collect()),
        wait_all: 1,
        timeout,
        ret: 0,
    })
}

pub fn fence_status(device: u64, fence: u64) -> Command<'static> {
    Command::GetFenceStatus(GetFenceStatusArgs {
        device: VkDevice(device),
        fence: VkFence(fence),
        ret: 0,
    })
}

pub fn reset_fences(device: u64, fences: &[u64]) -> Command<'static> {
    Command::ResetFences(ResetFencesArgs {
        device: VkDevice(device),
        fence_count: fences.len() as u32,
        p_fences: Some(fences.iter().copied().map(VkFence).collect()),
        ret: 0,
    })
}
