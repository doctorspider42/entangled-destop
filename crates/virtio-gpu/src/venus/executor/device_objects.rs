//! Stage 5b.2: pipelines, descriptors, render passes, queries, events,
//! command buffers and every core `vkCmd*` — what each of those commands
//! does to a context, following vkr (`vkr_pipeline.c`, `vkr_descriptor_set.c`,
//! `vkr_command_buffer.c`, `vkr_query_pool.c`, `vkr_render_pass.c`) unless a
//! function says otherwise. Submission, fences and waits are
//! [`super::submit`]'s.
//!
//! # Generated and hand-written
//!
//! Every command here goes through the same three steps, two of them
//! generated ([`super::generated`], `host_vulkan::calls`):
//!
//! 1. **translate** — every guest id in the command's inputs, however deeply
//!    nested, replaced by the host handle it names (typed, of the command's
//!    device, 0 only where vk.xml allows null), every enum and flag word
//!    checked against core Vulkan 1.3, every array against its count;
//! 2. **call** — [`HostVulkan::call`], which rebuilds the structures as the
//!    driver's and calls it;
//! 3. **answer** — outputs back into the command, which the ring encodes.
//!
//! A `[generated]` command (tools/venus-protocol/executor-classes.txt) is
//! exactly that ([`VulkanContext::pass_through`]). A `[handwritten]` one adds
//! what only the context knows: an id bound to a new object or taken out, a
//! bound against an object's own size or count, against the fixed-size state
//! a driver keeps on the host (viewports, push constants, vertex bindings),
//! or a submission's bookkeeping.
//!
//! # Validation posture (ADR-0004, 2026-09-24)
//!
//! Typed ids everywhere, enum and flag ranges, structural bounds — counts,
//! sizes, offsets into objects whose size the renderer knows, indices into
//! arrays the driver keeps on the host. Past that the driver judges, as in
//! vkr, and `robustBufferAccess` (enabled on every host device that has it)
//! keeps a shader's stray buffer access inside its buffer.

use crate::venus::protocol::{
    Command, VkDescriptorImageInfo, VkDescriptorSetAllocateInfoNext, VkWriteDescriptorSet,
    VkWriteDescriptorSetNext, VK_SUCCESS,
};

use super::context::{id_error, invalid, ExecError, VulkanContext};
use super::generated::{self, Resolve};
use super::host::{HostVulkan, RawHandle};
use super::objects::{Facts, IdError, Kind, Objects, RawObject};

/// `VK_WHOLE_SIZE`.
const WHOLE_SIZE: u64 = u64::MAX;

/// Largest `codeSize` a shader module may have: 64 MiB of SPIR-V, far past
/// any real shader and the size one `vkExecuteCommandStreamsMESA` may carry.
pub const MAX_SHADER_BYTES: u64 = 64 << 20;

/// `vkCmdUpdateBuffer`'s own limit on `dataSize`.
const MAX_UPDATE_BYTES: u64 = 65536;

/// The descriptor types, by value.
mod descriptor {
    pub const SAMPLER: i32 = 0;
    pub const COMBINED_IMAGE_SAMPLER: i32 = 1;
    pub const SAMPLED_IMAGE: i32 = 2;
    pub const STORAGE_IMAGE: i32 = 3;
    pub const UNIFORM_TEXEL_BUFFER: i32 = 4;
    pub const STORAGE_TEXEL_BUFFER: i32 = 5;
    pub const UNIFORM_BUFFER: i32 = 6;
    pub const STORAGE_BUFFER: i32 = 7;
    pub const UNIFORM_BUFFER_DYNAMIC: i32 = 8;
    pub const STORAGE_BUFFER_DYNAMIC: i32 = 9;
    pub const INPUT_ATTACHMENT: i32 = 10;
    pub const INLINE_UNIFORM_BLOCK: i32 = 1_000_138_000;
}

/// `VK_COMMAND_BUFFER_LEVEL_SECONDARY`.
const LEVEL_SECONDARY: i32 = 1;
/// `VK_PIPELINE_BIND_POINT_GRAPHICS` / `_COMPUTE`.
const BIND_GRAPHICS: i32 = 0;
const BIND_COMPUTE: i32 = 1;
/// `VK_DESCRIPTOR_POOL_CREATE_FREE_DESCRIPTOR_SET_BIT`.
const POOL_FREE_DESCRIPTOR_SET: u32 = 0x1;
/// `VK_FRAMEBUFFER_CREATE_IMAGELESS_BIT`.
const FRAMEBUFFER_IMAGELESS: u32 = 0x1;
/// `VK_QUERY_TYPE_PIPELINE_STATISTICS`.
const QUERY_PIPELINE_STATISTICS: i32 = 1;
/// `VK_QUERY_RESULT_64_BIT` / `WITH_AVAILABILITY_BIT`.
const RESULT_64: u32 = 0x1;
const RESULT_WITH_AVAILABILITY: u32 = 0x4;
/// `VK_IMAGE_ASPECT_COLOR_BIT`.
const ASPECT_COLOR: u32 = 0x1;
/// `VkDrawIndirectCommand` / `VkDrawIndexedIndirectCommand` /
/// `VkDispatchIndirectCommand`.
const DRAW_INDIRECT_BYTES: u64 = 16;
const DRAW_INDEXED_INDIRECT_BYTES: u64 = 20;
const DISPATCH_INDIRECT_BYTES: u64 = 12;

/// The context's answer to translation: ids of `device`, as host handles.
pub(super) struct Resolver<'a, H: HostVulkan> {
    pub(super) objects: &'a Objects<H>,
    pub(super) device: u64,
    pub(super) command: &'static str,
}

impl<H: HostVulkan> Resolver<'_, H> {
    fn lookup(&self, kind: Kind, id: u64) -> Result<u64, IdError> {
        let device = self.device;
        let wrong_parent = |child: Kind| IdError::WrongParent {
            child: child.name(),
            id,
            parent: Kind::Device.name(),
            parent_id: device,
        };
        match kind {
            Kind::Device => {
                self.objects.device(id)?;
                if id != device {
                    return Err(IdError::Unknown {
                        id,
                        expected: "VkDevice of this command",
                    });
                }
                Ok(id)
            }
            Kind::Queue => {
                let queue = self.objects.queue(id)?;
                if queue.device != device {
                    return Err(wrong_parent(Kind::Queue));
                }
                Ok(queue.host.raw())
            }
            Kind::Image => Ok(self.objects.image(device, id)?.host.raw()),
            Kind::Buffer => Ok(self.objects.buffer(device, id)?.host.raw()),
            Kind::BufferView => Ok(self.objects.buffer_view(device, id)?.host.raw()),
            Kind::ImageView => Ok(self.objects.image_view(device, id)?.host.raw()),
            Kind::DeviceMemory => Ok(self.objects.memory(device, id)?.host.raw()),
            Kind::CommandPool => Ok(self.objects.pool(device, id)?.host.raw()),
            Kind::Instance | Kind::PhysicalDevice => {
                // Never named by a device-level structure this renderer
                // serves; an id of one is refused as what it is.
                self.objects.check(id, kind)?;
                Err(IdError::Unknown {
                    id,
                    expected: kind.name(),
                })
            }
            _ => Ok(self.objects.raw(kind, device, id)?.host),
        }
    }
}

impl<H: HostVulkan> Resolve for Resolver<'_, H> {
    fn handle(
        &mut self,
        kind: Kind,
        id: u64,
        may_be_null: bool,
        what: &'static str,
    ) -> Result<u64, ExecError> {
        if id == 0 {
            if may_be_null {
                return Ok(0);
            }
            return Err(ExecError::IdIn {
                command: self.command,
                field: what,
                error: IdError::Zero(kind.name()),
            });
        }
        self.lookup(kind, id).map_err(|error| ExecError::IdIn {
            command: self.command,
            field: what,
            error,
        })
    }

    fn invalid(&self, what: String) -> ExecError {
        invalid(self.command, what)
    }

    fn link(&self, parent: &'static str, stype: i32) -> ExecError {
        super::context::unimplemented_link(self.command, parent, stype)
    }

    fn enabled(&self, extension: &'static str) -> bool {
        self.objects
            .device(self.device)
            .is_ok_and(|d| d.enabled(extension))
    }
}

/// The limits of the admitted extensions a command is bounded by (stage
/// 5c), as the guest was told them — zero for an extension the host lacks.
#[derive(Debug, Clone, Copy, Default)]
struct ExtensionLimits {
    /// `VkPhysicalDeviceTransformFeedbackPropertiesEXT`.
    tf_streams: u32,
    tf_buffers: u32,
    tf_buffer_size: u64,
    tf_buffer_data_stride: u32,
    /// `maxVertexAttribDivisor`, of either divisor properties structure.
    max_divisor: u32,
    /// `maxCustomBorderColorSamplers`.
    custom_border_samplers: u32,
}

impl ExtensionLimits {
    fn of(guest: &super::policy::GuestDevice) -> Self {
        use crate::venus::protocol::VkPhysicalDeviceProperties2Next as N;
        let mut out = Self::default();
        for link in &guest.properties.p_next {
            match link {
                N::VkPhysicalDeviceTransformFeedbackPropertiesEXT(p) => {
                    out.tf_streams = p.max_transform_feedback_streams;
                    out.tf_buffers = p.max_transform_feedback_buffers;
                    out.tf_buffer_size = p.max_transform_feedback_buffer_size;
                    out.tf_buffer_data_stride = p.max_transform_feedback_buffer_data_stride;
                }
                N::VkPhysicalDeviceVertexAttributeDivisorPropertiesEXT(p) => {
                    out.max_divisor = out.max_divisor.max(p.max_vertex_attrib_divisor);
                }
                N::VkPhysicalDeviceVertexAttributeDivisorProperties(p) => {
                    out.max_divisor = out.max_divisor.max(p.max_vertex_attrib_divisor);
                }
                N::VkPhysicalDeviceCustomBorderColorPropertiesEXT(p) => {
                    out.custom_border_samplers = p.max_custom_border_color_samplers;
                }
                _ => {}
            }
        }
        out
    }
}

/// `vkCmdSetLineStipple` / `VkPipelineRasterizationLineStateCreateInfo`:
/// `lineStippleFactor` must be in `[1, 256]`
/// (`VUID-vkCmdSetLineStipple-lineStippleFactor-02776`).
fn stipple_factor_ok(factor: u32) -> bool {
    (1..=256).contains(&factor)
}

/// Every `(src, dst)` queue family pair of a 1.0 barrier command's buffer
/// and image barriers.
fn barrier_families(
    buffers: Option<&[crate::venus::protocol::VkBufferMemoryBarrier]>,
    images: Option<&[crate::venus::protocol::VkImageMemoryBarrier]>,
) -> Vec<(u32, u32)> {
    let buffers = buffers
        .unwrap_or_default()
        .iter()
        .map(|b| (b.src_queue_family_index, b.dst_queue_family_index));
    let images = images
        .unwrap_or_default()
        .iter()
        .map(|b| (b.src_queue_family_index, b.dst_queue_family_index));
    buffers.chain(images).collect()
}

/// One image barrier that releases its image out of the instance (stage
/// S2b): to `VK_QUEUE_FAMILY_FOREIGN_EXT` or `VK_QUEUE_FAMILY_EXTERNAL`,
/// from a family of the device.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Release {
    image: u64,
    new_layout: i32,
    dst_family: u32,
}

/// Whether `(src, dst)` releases out of the instance.
fn is_release(src: u32, dst: u32) -> bool {
    use super::policy::{QUEUE_FAMILY_EXTERNAL, QUEUE_FAMILY_FOREIGN, QUEUE_FAMILY_IGNORED};
    let outside = |f: u32| f == QUEUE_FAMILY_EXTERNAL || f == QUEUE_FAMILY_FOREIGN;
    outside(dst) && !outside(src) && src != QUEUE_FAMILY_IGNORED
}

/// The releases among a 1.0 barrier command's image barriers.
fn image_releases(images: Option<&[crate::venus::protocol::VkImageMemoryBarrier]>) -> Vec<Release> {
    images
        .unwrap_or_default()
        .iter()
        .filter(|b| is_release(b.src_queue_family_index, b.dst_queue_family_index))
        .map(|b| Release {
            image: b.image.0,
            new_layout: b.new_layout,
            dst_family: b.dst_queue_family_index,
        })
        .collect()
}

/// [`image_releases`] of sync2 dependency infos.
fn dependency_releases<'a>(
    infos: impl Iterator<Item = &'a crate::venus::protocol::VkDependencyInfo>,
) -> Vec<Release> {
    let mut out = Vec::new();
    for info in infos {
        out.extend(
            info.p_image_memory_barriers
                .iter()
                .flatten()
                .filter(|b| is_release(b.src_queue_family_index, b.dst_queue_family_index))
                .map(|b| Release {
                    image: b.image.0,
                    new_layout: b.new_layout,
                    dst_family: b.dst_queue_family_index,
                }),
        );
    }
    out
}

/// [`barrier_families`] of sync2 dependency infos.
fn dependency_families<'a>(
    infos: impl Iterator<Item = &'a crate::venus::protocol::VkDependencyInfo>,
) -> Vec<(u32, u32)> {
    let mut out = Vec::new();
    for info in infos {
        out.extend(
            info.p_buffer_memory_barriers
                .iter()
                .flatten()
                .map(|b| (b.src_queue_family_index, b.dst_queue_family_index)),
        );
        out.extend(
            info.p_image_memory_barriers
                .iter()
                .flatten()
                .map(|b| (b.src_queue_family_index, b.dst_queue_family_index)),
        );
    }
    out
}

fn ranges_overflow(offset: u64, size: u64, total: u64) -> bool {
    offset.checked_add(size).is_none_or(|end| end > total)
}

impl<H: HostVulkan> VulkanContext<H> {
    /// Stage 5b.2's `[handwritten]` commands, each by name, and the
    /// `[generated]` ones through [`Self::pass_through`]; a refusal for every
    /// other command the protocol decodes. `scripts/venus-exec-gen.py`
    /// refuses to run when these arms and executor-classes.txt disagree.
    pub(super) fn dispatch_objects(&mut self, command: &mut Command<'_>) -> Result<(), ExecError> {
        match command {
            // ------------------------------------------------ objects
            Command::CreateShaderModule(args) => {
                let size = args.p_create_info.as_ref().map_or(0, |i| i.code_size);
                if size == 0 || size % 4 != 0 || size > MAX_SHADER_BYTES {
                    return Err(invalid(
                        "vkCreateShaderModule",
                        format!(
                            "codeSize {size} is not a nonzero multiple of 4 below {MAX_SHADER_BYTES}"
                        ),
                    ));
                }
                self.create(command, Kind::ShaderModule, Facts::None, 0)
            }
            Command::CreatePipelineLayout(args) => {
                let facts = self.pipeline_layout_facts(args)?;
                self.create(command, Kind::PipelineLayout, facts, 0)
            }
            Command::CreateDescriptorSetLayout(args) => {
                let facts = Facts::SetLayout(std::sync::Arc::new(set_layout_info(args)?));
                self.create(command, Kind::DescriptorSetLayout, facts, 0)
            }
            Command::CreateDescriptorPool(args) => {
                let flags = args.p_create_info.as_ref().map_or(0, |i| i.flags);
                let facts = Facts::DescriptorPool {
                    free_individual: flags & POOL_FREE_DESCRIPTOR_SET != 0,
                };
                self.create(command, Kind::DescriptorPool, facts, 0)
            }
            Command::CreateDescriptorUpdateTemplate(_) => {
                self.create(command, Kind::DescriptorUpdateTemplate, Facts::None, 0)
            }
            Command::CreateSampler(args) => self.create_sampler(args.device.0, command),
            Command::CreateSamplerYcbcrConversion(_) => {
                self.create(command, Kind::SamplerYcbcrConversion, Facts::None, 0)
            }
            Command::CreatePipelineCache(_) => {
                self.create(command, Kind::PipelineCache, Facts::None, 0)
            }
            Command::CreateGraphicsPipelines(args) => {
                const NAME: &str = "vkCreateGraphicsPipelines";
                self.check_stages(
                    NAME,
                    args.p_create_infos
                        .iter()
                        .flatten()
                        .flat_map(|i| i.p_stages.iter().flatten()),
                )?;
                let limits = self.limits(NAME, args.device.0)?.clone();
                let ext = self.extension_limits(NAME, args.device.0)?;
                for info in args.p_create_infos.iter().flatten() {
                    check_graphics_state(NAME, info, &limits, &ext)?;
                }
                let facts = Facts::Pipeline {
                    bind_point: BIND_GRAPHICS,
                };
                self.create(command, Kind::Pipeline, facts, 0)
            }
            Command::CreateComputePipelines(args) => {
                self.check_stages(
                    "vkCreateComputePipelines",
                    args.p_create_infos.iter().flatten().map(|i| &i.stage),
                )?;
                let facts = Facts::Pipeline {
                    bind_point: BIND_COMPUTE,
                };
                self.create(command, Kind::Pipeline, facts, 0)
            }
            Command::CreateRenderPass(args) => {
                let limits = self.limits("vkCreateRenderPass", args.device.0)?.clone();
                let facts = render_pass_facts(args, &limits)?;
                self.create(command, Kind::RenderPass, facts, 0)
            }
            Command::CreateRenderPass2(args) => {
                let limits = self.limits("vkCreateRenderPass2", args.device.0)?.clone();
                let facts = render_pass2_facts(args, &limits)?;
                self.create(command, Kind::RenderPass, facts, 0)
            }
            Command::CreateFramebuffer(args) => {
                const NAME: &str = "vkCreateFramebuffer";
                let Some(info) = &args.p_create_info else {
                    return Err(invalid(NAME, "pCreateInfo is null"));
                };
                let imageless = info.flags & FRAMEBUFFER_IMAGELESS != 0;
                if !imageless
                    && info
                        .p_attachments
                        .as_ref()
                        .is_some_and(|a| a.iter().any(|v| v.0 == 0))
                {
                    return Err(invalid(
                        NAME,
                        "a null attachment in a framebuffer that is not imageless",
                    ));
                }
                let (attachments, _) =
                    self.render_pass_facts_of(NAME, args.device.0, info.render_pass.0)?;
                if info.attachment_count != attachments {
                    return Err(invalid(
                        NAME,
                        format!(
                            "{} attachments for a render pass of {attachments}",
                            info.attachment_count
                        ),
                    ));
                }
                let facts = Facts::Framebuffer {
                    imageless,
                    attachments,
                };
                self.create(command, Kind::Framebuffer, facts, 0)
            }
            Command::CreateQueryPool(args) => {
                let Some(info) = &args.p_create_info else {
                    return Err(invalid("vkCreateQueryPool", "pCreateInfo is null"));
                };
                if info.query_count == 0 {
                    return Err(invalid("vkCreateQueryPool", "queryCount is 0"));
                }
                let facts = Facts::QueryPool {
                    query_type: info.query_type,
                    count: info.query_count,
                    values: match info.query_type {
                        QUERY_PIPELINE_STATISTICS => info.pipeline_statistics.count_ones(),
                        // Primitives written and primitives needed (stage 5c).
                        super::policy::QUERY_TYPE_TRANSFORM_FEEDBACK_STREAM => 2,
                        _ => 1,
                    },
                };
                self.create(command, Kind::QueryPool, facts, 0)
            }
            Command::CreateEvent(_) => self.create(command, Kind::Event, Facts::None, 0),
            Command::CreatePrivateDataSlot(_) => {
                self.create(command, Kind::PrivateDataSlot, Facts::None, 0)
            }
            Command::CreateFence(_) => self.create(command, Kind::Fence, Facts::None, 0),
            Command::CreateSemaphore(_) => self.create_semaphore(command),

            Command::DestroyShaderModule(args) => {
                self.destroy(Kind::ShaderModule, args.device.0, args.shader_module.0)
            }
            Command::DestroyPipelineLayout(args) => {
                self.destroy(Kind::PipelineLayout, args.device.0, args.pipeline_layout.0)
            }
            Command::DestroyDescriptorSetLayout(args) => self.destroy(
                Kind::DescriptorSetLayout,
                args.device.0,
                args.descriptor_set_layout.0,
            ),
            Command::DestroyDescriptorPool(args) => {
                self.destroy(Kind::DescriptorPool, args.device.0, args.descriptor_pool.0)
            }
            Command::DestroyDescriptorUpdateTemplate(args) => self.destroy(
                Kind::DescriptorUpdateTemplate,
                args.device.0,
                args.descriptor_update_template.0,
            ),
            Command::DestroySampler(args) => {
                self.destroy(Kind::Sampler, args.device.0, args.sampler.0)
            }
            Command::DestroySamplerYcbcrConversion(args) => self.destroy(
                Kind::SamplerYcbcrConversion,
                args.device.0,
                args.ycbcr_conversion.0,
            ),
            Command::DestroyPipelineCache(args) => {
                self.destroy(Kind::PipelineCache, args.device.0, args.pipeline_cache.0)
            }
            Command::DestroyPipeline(args) => {
                self.destroy(Kind::Pipeline, args.device.0, args.pipeline.0)
            }
            Command::DestroyRenderPass(args) => {
                self.destroy(Kind::RenderPass, args.device.0, args.render_pass.0)
            }
            Command::DestroyFramebuffer(args) => {
                self.destroy(Kind::Framebuffer, args.device.0, args.framebuffer.0)
            }
            Command::DestroyQueryPool(args) => {
                self.destroy(Kind::QueryPool, args.device.0, args.query_pool.0)
            }
            Command::DestroyEvent(args) => self.destroy(Kind::Event, args.device.0, args.event.0),
            Command::DestroyPrivateDataSlot(args) => self.destroy(
                Kind::PrivateDataSlot,
                args.device.0,
                args.private_data_slot.0,
            ),
            // Waits for the queues first, so a fence a queue's record
            // stands on is never destroyed while that work may run.
            Command::DestroyFence(args) => self.destroy(Kind::Fence, args.device.0, args.fence.0),
            // Likewise: nothing submitted that waits on or signals it may
            // still be running when it goes.
            Command::DestroySemaphore(args) => {
                self.destroy(Kind::Semaphore, args.device.0, args.semaphore.0)
            }
            Command::GetSemaphoreCounterValue(args) => {
                self.require_timeline(
                    "vkGetSemaphoreCounterValue",
                    args.device.0,
                    args.semaphore.0,
                )?;
                self.pass_through(command)
            }
            Command::SignalSemaphore(args) => {
                const NAME: &str = "vkSignalSemaphore";
                let Some(info) = &args.p_signal_info else {
                    return Err(invalid(NAME, "pSignalInfo is null"));
                };
                self.require_timeline(NAME, args.device.0, info.semaphore.0)?;
                self.pass_through(command)
            }

            // ------------------------------------------- descriptor sets
            Command::AllocateDescriptorSets(args) => {
                const NAME: &str = "vkAllocateDescriptorSets";
                let Some(info) = &args.p_allocate_info else {
                    return Err(invalid(NAME, "pAllocateInfo is null"));
                };
                let pool = info.descriptor_pool.0;
                self.objects
                    .raw(Kind::DescriptorPool, args.device.0, pool)
                    .map_err(id_error(NAME))?;
                let variable: Option<&[u32]> = info.p_next.iter().find_map(|l| match l {
                    VkDescriptorSetAllocateInfoNext::VkDescriptorSetVariableDescriptorCountAllocateInfo(v) => {
                        Some(v.p_descriptor_counts.as_deref().unwrap_or_default())
                    }
                    #[allow(unreachable_patterns)]
                    _ => None,
                });
                let mut facts = Vec::new();
                for (index, layout) in info.p_set_layouts.iter().flatten().enumerate() {
                    let object = self
                        .objects
                        .raw(Kind::DescriptorSetLayout, args.device.0, layout.0)
                        .map_err(id_error(NAME))?;
                    let Facts::SetLayout(layout) = &object.facts else {
                        return Err(invalid(NAME, "not a descriptor set layout"));
                    };
                    let count = variable.and_then(|v| v.get(index)).copied().unwrap_or(0);
                    let max = layout
                        .variable
                        .and_then(|b| layout.binding(b))
                        .map_or(0, |b| b.count);
                    if count > max {
                        return Err(invalid(
                            NAME,
                            format!(
                                "a variable descriptor count of {count} past the binding's {max}"
                            ),
                        ));
                    }
                    facts.push(Facts::DescriptorSet {
                        layout: std::sync::Arc::clone(layout),
                        variable: count,
                    });
                }
                self.create_each(command, Kind::DescriptorSet, &facts, pool)
            }
            Command::FreeDescriptorSets(args) => {
                const NAME: &str = "vkFreeDescriptorSets";
                let (device, pool) = (args.device.0, args.descriptor_pool.0);
                let facts = self
                    .objects
                    .raw(Kind::DescriptorPool, device, pool)
                    .map_err(id_error(NAME))?
                    .facts
                    .clone();
                if facts
                    != (Facts::DescriptorPool {
                        free_individual: true,
                    })
                {
                    return Err(invalid(
                        NAME,
                        "the pool was not created with FREE_DESCRIPTOR_SET",
                    ));
                }
                let sets: Vec<u64> = args
                    .p_descriptor_sets
                    .iter()
                    .flatten()
                    .map(|s| s.0)
                    .collect();
                self.free_children(command, Kind::DescriptorSet, device, pool, &sets)
            }
            Command::ResetDescriptorPool(args) => {
                let (device, pool) = (args.device.0, args.descriptor_pool.0);
                self.objects
                    .raw(Kind::DescriptorPool, device, pool)
                    .map_err(id_error("vkResetDescriptorPool"))?;
                self.settle(device);
                self.pass_through(command)?;
                // Whatever the driver answered, as vkr: the sets are gone.
                self.objects.forget_pool_children(pool);
                Ok(())
            }
            Command::UpdateDescriptorSets(args) => {
                for write in args.p_descriptor_writes.iter().flatten() {
                    self.check_write(args.device.0, write)?;
                }
                for copy in args.p_descriptor_copies.iter().flatten() {
                    self.check_copy(args.device.0, copy)?;
                }
                self.pass_through(command)
            }

            // ---------------------------------------------- pipeline caches
            Command::GetPipelineCacheData(_) => self.pass_through(command),

            // ------------------------------------------------ queries
            Command::GetQueryPoolResults(args) => {
                const NAME: &str = "vkGetQueryPoolResults";
                let size = self.query_results_size(
                    NAME,
                    args.device.0,
                    args.query_pool.0,
                    (args.first_query, args.query_count),
                    args.stride,
                    args.flags,
                )?;
                if size > args.data_size {
                    return Err(invalid(
                        NAME,
                        format!(
                            "{size} bytes of results do not fit dataSize {}",
                            args.data_size
                        ),
                    ));
                }
                self.pass_through(command)
            }
            Command::ResetQueryPool(args) => {
                self.check_queries(
                    "vkResetQueryPool",
                    args.device.0,
                    args.query_pool.0,
                    args.first_query,
                    args.query_count,
                )?;
                self.pass_through(command)
            }

            // ------------------------------------------------ private data
            Command::SetPrivateData(args) => {
                args.object_handle = self.private_data_object(
                    "vkSetPrivateData",
                    args.device.0,
                    args.object_type,
                    args.object_handle,
                )?;
                self.pass_through(command)
            }
            Command::GetPrivateData(args) => {
                args.object_handle = self.private_data_object(
                    "vkGetPrivateData",
                    args.device.0,
                    args.object_type,
                    args.object_handle,
                )?;
                self.pass_through(command)
            }

            // ------------------------------------------- command buffers
            Command::AllocateCommandBuffers(args) => {
                const NAME: &str = "vkAllocateCommandBuffers";
                let Some(info) = &args.p_allocate_info else {
                    return Err(invalid(NAME, "pAllocateInfo is null"));
                };
                let pool = info.command_pool.0;
                self.objects
                    .pool(args.device.0, pool)
                    .map_err(id_error(NAME))?;
                let facts = Facts::CommandBuffer {
                    secondary: info.level == LEVEL_SECONDARY,
                };
                self.create(command, Kind::CommandBuffer, facts, pool)
            }
            Command::FreeCommandBuffers(args) => {
                let (device, pool) = (args.device.0, args.command_pool.0);
                self.objects
                    .pool(device, pool)
                    .map_err(id_error("vkFreeCommandBuffers"))?;
                let buffers: Vec<u64> = args
                    .p_command_buffers
                    .iter()
                    .flatten()
                    .map(|c| c.0)
                    .collect();
                self.free_children(command, Kind::CommandBuffer, device, pool, &buffers)
            }
            Command::ResetCommandPool(_) | Command::ResetCommandBuffer(_) => {
                self.pass_through(command)
            }
            Command::BeginCommandBuffer(args) => {
                // A secondary recorded for dynamic rendering (stage 5b.3)
                // inherits its colour formats, which a driver keeps in an
                // array of maxColorAttachments.
                use crate::venus::protocol::VkCommandBufferInheritanceInfoNext as N;
                const NAME: &str = "vkBeginCommandBuffer";
                let device = self.cmd_device(NAME, args.command_buffer.0)?;
                let limit = self.limits(NAME, device)?.max_color_attachments;
                let inherited = args
                    .p_begin_info
                    .iter()
                    .filter_map(|b| b.p_inheritance_info.as_ref())
                    .flat_map(|i| i.p_next.iter())
                    .any(|l| {
                        matches!(l, N::VkCommandBufferInheritanceRenderingInfo(r)
                            if r.color_attachment_count > limit)
                    });
                if inherited {
                    return Err(invalid(
                        NAME,
                        "inherited rendering with more colour attachments than maxColorAttachments",
                    ));
                }
                self.pass_through(command)
            }
            Command::CmdBeginRendering(args) => {
                const NAME: &str = "vkCmdBeginRendering";
                let device = self.cmd_device(NAME, args.command_buffer.0)?;
                if let Some(info) = &args.p_rendering_info {
                    self.check_rendering(NAME, device, info)?;
                }
                self.pass_through(command)
            }

            // --------------------------------------- fences and submission
            Command::ResetFences(_) => self.reset_fences(command),
            Command::WaitForFences(_)
            | Command::WaitSemaphores(_)
            | Command::QueueWaitIdle(_)
            | Command::DeviceWaitIdle(_) => {
                // One slice with no time left: the ring worker drives the
                // real, sliced wait (`ExecutingSink`); a caller that reaches
                // past it gets an answer that does not block.
                self.wait_slice(command, std::time::Duration::ZERO)
                    .map(|_| ())
            }
            Command::QueueSubmit(_) | Command::QueueSubmit2(_) => self.queue_submit(command),

            // ----------------------------------------- recording, bounded
            Command::CmdBindPipeline(args) => {
                const NAME: &str = "vkCmdBindPipeline";
                let device = self.cmd_device(NAME, args.command_buffer.0)?;
                let facts = self
                    .objects
                    .raw(Kind::Pipeline, device, args.pipeline.0)
                    .map_err(id_error(NAME))?
                    .facts
                    .clone();
                if facts
                    != (Facts::Pipeline {
                        bind_point: args.pipeline_bind_point,
                    })
                {
                    return Err(invalid(
                        NAME,
                        "the pipeline was not created for that bind point",
                    ));
                }
                self.pass_through(command)
            }
            Command::CmdBindDescriptorSets(args) => {
                const NAME: &str = "vkCmdBindDescriptorSets";
                let device = self.cmd_device(NAME, args.command_buffer.0)?;
                let sets = match self
                    .objects
                    .raw(Kind::PipelineLayout, device, args.layout.0)
                    .map_err(id_error(NAME))?
                    .facts
                {
                    Facts::PipelineLayout { sets } => sets,
                    _ => 0,
                };
                if ranges_overflow(
                    u64::from(args.first_set),
                    u64::from(args.descriptor_set_count),
                    u64::from(sets),
                ) {
                    return Err(invalid(
                        NAME,
                        format!(
                            "sets {}+{} of a layout of {sets}",
                            args.first_set, args.descriptor_set_count
                        ),
                    ));
                }
                // The driver reads one dynamic offset per dynamic descriptor
                // of the sets bound: exactly that many must be there.
                let mut dynamic = 0u32;
                for set in args.p_descriptor_sets.iter().flatten() {
                    let object = self
                        .objects
                        .raw(Kind::DescriptorSet, device, set.0)
                        .map_err(id_error(NAME))?;
                    if let Facts::DescriptorSet { layout, .. } = &object.facts {
                        dynamic = dynamic.saturating_add(layout.dynamic);
                    }
                }
                if args.dynamic_offset_count != dynamic {
                    return Err(invalid(
                        NAME,
                        format!(
                            "{} dynamic offsets for sets with {dynamic} dynamic descriptors",
                            args.dynamic_offset_count
                        ),
                    ));
                }
                self.pass_through(command)
            }
            Command::CmdBeginRenderPass(args) => {
                const NAME: &str = "vkCmdBeginRenderPass";
                let device = self.cmd_device(NAME, args.command_buffer.0)?;
                if let Some(begin) = &args.p_render_pass_begin {
                    self.check_render_pass_begin(NAME, device, begin)?;
                }
                self.pass_through(command)
            }
            Command::CmdBeginRenderPass2(args) => {
                const NAME: &str = "vkCmdBeginRenderPass2";
                let device = self.cmd_device(NAME, args.command_buffer.0)?;
                if let Some(begin) = &args.p_render_pass_begin {
                    self.check_render_pass_begin(NAME, device, begin)?;
                }
                self.pass_through(command)
            }
            Command::CmdBindIndexBuffer(args) => {
                const NAME: &str = "vkCmdBindIndexBuffer";
                let device = self.cmd_device(NAME, args.command_buffer.0)?;
                let size = self.buffer_size(NAME, device, args.buffer.0)?;
                if args.offset >= size {
                    return Err(invalid(NAME, "offset is outside the buffer"));
                }
                self.pass_through(command)
            }
            Command::CmdBindVertexBuffers(args) => {
                const NAME: &str = "vkCmdBindVertexBuffers";
                let device = self.cmd_device(NAME, args.command_buffer.0)?;
                self.check_vertex_buffers(
                    NAME,
                    device,
                    (args.first_binding, args.binding_count),
                    args.p_buffers.as_deref(),
                    args.p_offsets.as_deref(),
                    None,
                )?;
                self.pass_through(command)
            }
            Command::CmdBindVertexBuffers2(args) => {
                const NAME: &str = "vkCmdBindVertexBuffers2";
                let device = self.cmd_device(NAME, args.command_buffer.0)?;
                self.check_vertex_buffers(
                    NAME,
                    device,
                    (args.first_binding, args.binding_count),
                    args.p_buffers.as_deref(),
                    args.p_offsets.as_deref(),
                    args.p_sizes.as_deref(),
                )?;
                self.pass_through(command)
            }
            Command::CmdSetViewport(args) => {
                self.check_viewports(
                    "vkCmdSetViewport",
                    args.command_buffer.0,
                    args.first_viewport,
                    args.viewport_count,
                )?;
                self.pass_through(command)
            }
            Command::CmdSetScissor(args) => {
                self.check_viewports(
                    "vkCmdSetScissor",
                    args.command_buffer.0,
                    args.first_scissor,
                    args.scissor_count,
                )?;
                self.pass_through(command)
            }
            Command::CmdSetViewportWithCount(args) => {
                self.check_viewports(
                    "vkCmdSetViewportWithCount",
                    args.command_buffer.0,
                    0,
                    args.viewport_count,
                )?;
                self.pass_through(command)
            }
            Command::CmdSetScissorWithCount(args) => {
                self.check_viewports(
                    "vkCmdSetScissorWithCount",
                    args.command_buffer.0,
                    0,
                    args.scissor_count,
                )?;
                self.pass_through(command)
            }
            Command::CmdPushConstants(args) => {
                const NAME: &str = "vkCmdPushConstants";
                let device = self.cmd_device(NAME, args.command_buffer.0)?;
                let limit = self.limits(NAME, device)?.max_push_constants_size;
                if args.size == 0
                    || args.offset % 4 != 0
                    || args.size % 4 != 0
                    || ranges_overflow(
                        u64::from(args.offset),
                        u64::from(args.size),
                        u64::from(limit),
                    )
                {
                    return Err(invalid(
                        NAME,
                        format!(
                            "{} bytes at {} are not a 4-aligned range inside maxPushConstantsSize {limit}",
                            args.size, args.offset
                        ),
                    ));
                }
                self.pass_through(command)
            }
            Command::CmdFillBuffer(args) => {
                const NAME: &str = "vkCmdFillBuffer";
                let device = self.cmd_device(NAME, args.command_buffer.0)?;
                let size = self.buffer_size(NAME, device, args.dst_buffer.0)?;
                let bad = args.dst_offset % 4 != 0
                    || args.dst_offset >= size
                    || (args.size != WHOLE_SIZE
                        && (args.size == 0
                            || args.size % 4 != 0
                            || ranges_overflow(args.dst_offset, args.size, size)));
                if bad {
                    return Err(invalid(
                        NAME,
                        "the range is not a 4-aligned one inside the buffer",
                    ));
                }
                self.pass_through(command)
            }
            Command::CmdUpdateBuffer(args) => {
                const NAME: &str = "vkCmdUpdateBuffer";
                let device = self.cmd_device(NAME, args.command_buffer.0)?;
                let size = self.buffer_size(NAME, device, args.dst_buffer.0)?;
                if args.dst_offset % 4 != 0
                    || args.data_size == 0
                    || args.data_size % 4 != 0
                    || args.data_size > MAX_UPDATE_BYTES
                    || ranges_overflow(args.dst_offset, args.data_size, size)
                {
                    return Err(invalid(
                        NAME,
                        "the update is not a 4-aligned range of at most 64 KiB inside the buffer",
                    ));
                }
                self.pass_through(command)
            }
            Command::CmdCopyBuffer(args) => {
                const NAME: &str = "vkCmdCopyBuffer";
                let device = self.cmd_device(NAME, args.command_buffer.0)?;
                let regions: Vec<(u64, u64, u64)> = args
                    .p_regions
                    .iter()
                    .flatten()
                    .map(|r| (r.src_offset, r.dst_offset, r.size))
                    .collect();
                self.check_buffer_copies(
                    NAME,
                    device,
                    args.src_buffer.0,
                    args.dst_buffer.0,
                    &regions,
                )?;
                self.pass_through(command)
            }
            Command::CmdCopyBuffer2(args) => {
                const NAME: &str = "vkCmdCopyBuffer2";
                let device = self.cmd_device(NAME, args.command_buffer.0)?;
                if let Some(info) = &args.p_copy_buffer_info {
                    let regions: Vec<(u64, u64, u64)> = info
                        .p_regions
                        .iter()
                        .flatten()
                        .map(|r| (r.src_offset, r.dst_offset, r.size))
                        .collect();
                    self.check_buffer_copies(
                        NAME,
                        device,
                        info.src_buffer.0,
                        info.dst_buffer.0,
                        &regions,
                    )?;
                }
                self.pass_through(command)
            }
            Command::CmdCopyBufferToImage(args) => {
                const NAME: &str = "vkCmdCopyBufferToImage";
                let device = self.cmd_device(NAME, args.command_buffer.0)?;
                let offsets: Vec<u64> = args
                    .p_regions
                    .iter()
                    .flatten()
                    .map(|r| r.buffer_offset)
                    .collect();
                self.check_image_copy_offsets(NAME, device, args.src_buffer.0, &offsets)?;
                self.pass_through(command)
            }
            Command::CmdCopyBufferToImage2(args) => {
                const NAME: &str = "vkCmdCopyBufferToImage2";
                let device = self.cmd_device(NAME, args.command_buffer.0)?;
                if let Some(info) = &args.p_copy_buffer_to_image_info {
                    let offsets: Vec<u64> = info
                        .p_regions
                        .iter()
                        .flatten()
                        .map(|r| r.buffer_offset)
                        .collect();
                    self.check_image_copy_offsets(NAME, device, info.src_buffer.0, &offsets)?;
                }
                self.pass_through(command)
            }
            Command::CmdCopyImageToBuffer(args) => {
                const NAME: &str = "vkCmdCopyImageToBuffer";
                let device = self.cmd_device(NAME, args.command_buffer.0)?;
                let offsets: Vec<u64> = args
                    .p_regions
                    .iter()
                    .flatten()
                    .map(|r| r.buffer_offset)
                    .collect();
                self.check_image_copy_offsets(NAME, device, args.dst_buffer.0, &offsets)?;
                self.pass_through(command)
            }
            Command::CmdCopyImageToBuffer2(args) => {
                const NAME: &str = "vkCmdCopyImageToBuffer2";
                let device = self.cmd_device(NAME, args.command_buffer.0)?;
                if let Some(info) = &args.p_copy_image_to_buffer_info {
                    let offsets: Vec<u64> = info
                        .p_regions
                        .iter()
                        .flatten()
                        .map(|r| r.buffer_offset)
                        .collect();
                    self.check_image_copy_offsets(NAME, device, info.dst_buffer.0, &offsets)?;
                }
                self.pass_through(command)
            }
            Command::CmdDrawIndirect(args) => {
                const NAME: &str = "vkCmdDrawIndirect";
                let device = self.cmd_device(NAME, args.command_buffer.0)?;
                self.check_indirect(
                    NAME,
                    device,
                    args.buffer.0,
                    args.offset,
                    (args.draw_count, args.stride),
                    DRAW_INDIRECT_BYTES,
                )?;
                self.pass_through(command)
            }
            Command::CmdDrawIndexedIndirect(args) => {
                const NAME: &str = "vkCmdDrawIndexedIndirect";
                let device = self.cmd_device(NAME, args.command_buffer.0)?;
                self.check_indirect(
                    NAME,
                    device,
                    args.buffer.0,
                    args.offset,
                    (args.draw_count, args.stride),
                    DRAW_INDEXED_INDIRECT_BYTES,
                )?;
                self.pass_through(command)
            }
            Command::CmdDrawIndirectCount(args) => {
                const NAME: &str = "vkCmdDrawIndirectCount";
                let device = self.cmd_device(NAME, args.command_buffer.0)?;
                self.check_indirect(
                    NAME,
                    device,
                    args.buffer.0,
                    args.offset,
                    (args.max_draw_count, args.stride),
                    DRAW_INDIRECT_BYTES,
                )?;
                self.check_indirect(
                    NAME,
                    device,
                    args.count_buffer.0,
                    args.count_buffer_offset,
                    (1, 4),
                    4,
                )?;
                self.pass_through(command)
            }
            Command::CmdDrawIndexedIndirectCount(args) => {
                const NAME: &str = "vkCmdDrawIndexedIndirectCount";
                let device = self.cmd_device(NAME, args.command_buffer.0)?;
                self.check_indirect(
                    NAME,
                    device,
                    args.buffer.0,
                    args.offset,
                    (args.max_draw_count, args.stride),
                    DRAW_INDEXED_INDIRECT_BYTES,
                )?;
                self.check_indirect(
                    NAME,
                    device,
                    args.count_buffer.0,
                    args.count_buffer_offset,
                    (1, 4),
                    4,
                )?;
                self.pass_through(command)
            }
            Command::CmdDispatchIndirect(args) => {
                const NAME: &str = "vkCmdDispatchIndirect";
                let device = self.cmd_device(NAME, args.command_buffer.0)?;
                self.check_indirect(
                    NAME,
                    device,
                    args.buffer.0,
                    args.offset,
                    (1, DISPATCH_INDIRECT_BYTES as u32),
                    DISPATCH_INDIRECT_BYTES,
                )?;
                self.pass_through(command)
            }
            Command::CmdBeginQuery(args) => {
                self.check_cmd_queries(
                    "vkCmdBeginQuery",
                    args.command_buffer.0,
                    args.query_pool.0,
                    args.query,
                    1,
                )?;
                self.pass_through(command)
            }
            Command::CmdEndQuery(args) => {
                self.check_cmd_queries(
                    "vkCmdEndQuery",
                    args.command_buffer.0,
                    args.query_pool.0,
                    args.query,
                    1,
                )?;
                self.pass_through(command)
            }
            Command::CmdResetQueryPool(args) => {
                self.check_cmd_queries(
                    "vkCmdResetQueryPool",
                    args.command_buffer.0,
                    args.query_pool.0,
                    args.first_query,
                    args.query_count,
                )?;
                self.pass_through(command)
            }
            Command::CmdWriteTimestamp(args) => {
                self.check_cmd_queries(
                    "vkCmdWriteTimestamp",
                    args.command_buffer.0,
                    args.query_pool.0,
                    args.query,
                    1,
                )?;
                self.pass_through(command)
            }
            Command::CmdWriteTimestamp2(args) => {
                self.check_cmd_queries(
                    "vkCmdWriteTimestamp2",
                    args.command_buffer.0,
                    args.query_pool.0,
                    args.query,
                    1,
                )?;
                self.pass_through(command)
            }
            Command::CmdCopyQueryPoolResults(args) => {
                const NAME: &str = "vkCmdCopyQueryPoolResults";
                let device = self.cmd_device(NAME, args.command_buffer.0)?;
                let size = self.query_results_size(
                    NAME,
                    device,
                    args.query_pool.0,
                    (args.first_query, args.query_count),
                    args.stride,
                    args.flags,
                )?;
                let buffer = self.buffer_size(NAME, device, args.dst_buffer.0)?;
                if args.dst_offset % 4 != 0 || ranges_overflow(args.dst_offset, size, buffer) {
                    return Err(invalid(NAME, "the results do not fit the buffer"));
                }
                self.pass_through(command)
            }
            Command::CmdClearAttachments(args) => {
                const NAME: &str = "vkCmdClearAttachments";
                let device = self.cmd_device(NAME, args.command_buffer.0)?;
                let limit = self.limits(NAME, device)?.max_color_attachments;
                if args
                    .p_attachments
                    .iter()
                    .flatten()
                    .any(|a| a.aspect_mask & ASPECT_COLOR != 0 && a.color_attachment >= limit)
                {
                    return Err(invalid(
                        NAME,
                        "a colour attachment index past maxColorAttachments",
                    ));
                }
                self.pass_through(command)
            }
            Command::CmdSetDeviceMask(args) => {
                const NAME: &str = "vkCmdSetDeviceMask";
                let device = self.cmd_device(NAME, args.command_buffer.0)?;
                let group = self
                    .objects
                    .device(device)
                    .map_err(id_error(NAME))?
                    .group_size;
                let all = 1u32.checked_shl(group).map_or(u32::MAX, |b| b - 1);
                if args.device_mask == 0 || args.device_mask & !all != 0 {
                    return Err(invalid(NAME, "a device mask outside the device group"));
                }
                self.pass_through(command)
            }
            Command::CmdExecuteCommands(args) => {
                const NAME: &str = "vkCmdExecuteCommands";
                let device = self.cmd_device(NAME, args.command_buffer.0)?;
                for cb in args.p_command_buffers.iter().flatten() {
                    let object = self
                        .objects
                        .raw(Kind::CommandBuffer, device, cb.0)
                        .map_err(id_error(NAME))?;
                    if object.facts != (Facts::CommandBuffer { secondary: true }) {
                        return Err(invalid(
                            NAME,
                            format!("{:#x} is not a secondary command buffer", cb.0),
                        ));
                    }
                }
                self.pass_through(command)
            }

            // ------------------------ stage 5c: the admitted extensions
            Command::CmdBindTransformFeedbackBuffersEXT(args) => {
                const NAME: &str = "vkCmdBindTransformFeedbackBuffersEXT";
                let device = self.cmd_device(NAME, args.command_buffer.0)?;
                let ext = self.extension_limits(NAME, device)?;
                if ranges_overflow(
                    u64::from(args.first_binding),
                    u64::from(args.binding_count),
                    u64::from(ext.tf_buffers),
                ) {
                    return Err(invalid(
                        NAME,
                        format!(
                            "bindings {}+{} past maxTransformFeedbackBuffers {}",
                            args.first_binding, args.binding_count, ext.tf_buffers
                        ),
                    ));
                }
                let offsets = args.p_offsets.as_deref().unwrap_or_default();
                for (index, buffer) in args.p_buffers.iter().flatten().enumerate() {
                    let (size, usage) = self.buffer_size_usage(NAME, device, buffer.0)?;
                    let offset = offsets.get(index).copied().unwrap_or(0);
                    let range = args
                        .p_sizes
                        .as_deref()
                        .and_then(|s| s.get(index))
                        .copied()
                        .unwrap_or(WHOLE_SIZE);
                    let bad = usage & super::policy::BUFFER_USAGE_TRANSFORM_FEEDBACK == 0
                        || offset % 4 != 0
                        || offset >= size
                        || (range != WHOLE_SIZE
                            && (range > ext.tf_buffer_size
                                || ranges_overflow(offset, range, size)));
                    if bad {
                        return Err(invalid(
                            NAME,
                            "a transform feedback binding that is not a 4-aligned range inside a \
                             TRANSFORM_FEEDBACK buffer, or past maxTransformFeedbackBufferSize",
                        ));
                    }
                }
                self.pass_through(command)
            }
            Command::CmdBeginTransformFeedbackEXT(args) => {
                const NAME: &str = "vkCmdBeginTransformFeedbackEXT";
                let device = self.cmd_device(NAME, args.command_buffer.0)?;
                self.check_counter_buffers(
                    NAME,
                    device,
                    (args.first_counter_buffer, args.counter_buffer_count),
                    args.p_counter_buffers.as_deref(),
                    args.p_counter_buffer_offsets.as_deref(),
                )?;
                self.pass_through(command)
            }
            Command::CmdEndTransformFeedbackEXT(args) => {
                const NAME: &str = "vkCmdEndTransformFeedbackEXT";
                let device = self.cmd_device(NAME, args.command_buffer.0)?;
                self.check_counter_buffers(
                    NAME,
                    device,
                    (args.first_counter_buffer, args.counter_buffer_count),
                    args.p_counter_buffers.as_deref(),
                    args.p_counter_buffer_offsets.as_deref(),
                )?;
                self.pass_through(command)
            }
            Command::CmdBeginQueryIndexedEXT(args) => {
                self.check_indexed_query(
                    "vkCmdBeginQueryIndexedEXT",
                    args.command_buffer.0,
                    args.query_pool.0,
                    args.query,
                    args.index,
                )?;
                self.pass_through(command)
            }
            Command::CmdEndQueryIndexedEXT(args) => {
                self.check_indexed_query(
                    "vkCmdEndQueryIndexedEXT",
                    args.command_buffer.0,
                    args.query_pool.0,
                    args.query,
                    args.index,
                )?;
                self.pass_through(command)
            }
            Command::CmdDrawIndirectByteCountEXT(args) => {
                const NAME: &str = "vkCmdDrawIndirectByteCountEXT";
                let device = self.cmd_device(NAME, args.command_buffer.0)?;
                let ext = self.extension_limits(NAME, device)?;
                let (size, _) = self.buffer_size_usage(NAME, device, args.counter_buffer.0)?;
                if args.counter_buffer_offset % 4 != 0
                    || ranges_overflow(args.counter_buffer_offset, 4, size)
                    || args.vertex_stride == 0
                    || args.vertex_stride > ext.tf_buffer_data_stride
                {
                    return Err(invalid(
                        NAME,
                        "a byte counter outside its buffer, or a vertex stride of 0 or past \
                         maxTransformFeedbackBufferDataStride",
                    ));
                }
                self.pass_through(command)
            }
            Command::CmdBeginConditionalRenderingEXT(args) => {
                const NAME: &str = "vkCmdBeginConditionalRenderingEXT";
                let device = self.cmd_device(NAME, args.command_buffer.0)?;
                let Some(begin) = &args.p_conditional_rendering_begin else {
                    return Err(invalid(NAME, "pConditionalRenderingBegin is null"));
                };
                let (size, usage) = self.buffer_size_usage(NAME, device, begin.buffer.0)?;
                if usage & super::policy::BUFFER_USAGE_CONDITIONAL_RENDERING == 0
                    || begin.offset % 4 != 0
                    || ranges_overflow(begin.offset, 4, size)
                {
                    return Err(invalid(
                        NAME,
                        "a predicate that is not 4 aligned bytes inside a CONDITIONAL_RENDERING \
                         buffer",
                    ));
                }
                self.pass_through(command)
            }
            Command::CmdSetLineStipple(args) => {
                if !stipple_factor_ok(args.line_stipple_factor) {
                    return Err(invalid(
                        "vkCmdSetLineStipple",
                        format!(
                            "lineStippleFactor {} is outside [1, 256]",
                            args.line_stipple_factor
                        ),
                    ));
                }
                self.pass_through(command)
            }

            // ------------- barriers: queue family ownership (stage S1)
            Command::CmdPipelineBarrier(args) => {
                const NAME: &str = "vkCmdPipelineBarrier";
                let device = self.cmd_device(NAME, args.command_buffer.0)?;
                let pairs = barrier_families(
                    args.p_buffer_memory_barriers.as_deref(),
                    args.p_image_memory_barriers.as_deref(),
                );
                let releases = image_releases(args.p_image_memory_barriers.as_deref());
                self.check_families(NAME, device, &pairs, false)?;
                self.pass_through(command)?;
                self.note_releases(device, &releases);
                Ok(())
            }
            Command::CmdWaitEvents(args) => {
                const NAME: &str = "vkCmdWaitEvents";
                let device = self.cmd_device(NAME, args.command_buffer.0)?;
                let pairs = barrier_families(
                    args.p_buffer_memory_barriers.as_deref(),
                    args.p_image_memory_barriers.as_deref(),
                );
                let releases = image_releases(args.p_image_memory_barriers.as_deref());
                self.check_families(NAME, device, &pairs, false)?;
                self.pass_through(command)?;
                self.note_releases(device, &releases);
                Ok(())
            }
            Command::CmdPipelineBarrier2(args) => {
                const NAME: &str = "vkCmdPipelineBarrier2";
                let device = self.cmd_device(NAME, args.command_buffer.0)?;
                let pairs = dependency_families(args.p_dependency_info.iter());
                let releases = dependency_releases(args.p_dependency_info.iter());
                self.check_families(NAME, device, &pairs, false)?;
                self.pass_through(command)?;
                self.note_releases(device, &releases);
                Ok(())
            }
            Command::CmdWaitEvents2(args) => {
                const NAME: &str = "vkCmdWaitEvents2";
                let device = self.cmd_device(NAME, args.command_buffer.0)?;
                let pairs = dependency_families(args.p_dependency_infos.iter().flatten());
                let releases = dependency_releases(args.p_dependency_infos.iter().flatten());
                self.check_families(NAME, device, &pairs, false)?;
                self.pass_through(command)?;
                self.note_releases(device, &releases);
                Ok(())
            }
            Command::CmdSetEvent2(args) => {
                const NAME: &str = "vkCmdSetEvent2";
                let device = self.cmd_device(NAME, args.command_buffer.0)?;
                let pairs = dependency_families(args.p_dependency_info.iter());
                // A set transfers nothing: its barriers' two families are
                // equal (`VUID-vkCmdSetEvent2-srcQueueFamilyIndex-03842`).
                self.check_families(NAME, device, &pairs, true)?;
                self.pass_through(command)
            }

            other if generated::is_pass_through(other) => self.pass_through(other),
            other => self.dispatch_extension(other),
        }
    }

    /// The queue family indices of a command's buffer and image barriers
    /// (stage S1), each `(src, dst)` pair judged before the driver indexes
    /// its per-family state by them: a family of the device, or
    /// `VK_QUEUE_FAMILY_IGNORED`, or `VK_QUEUE_FAMILY_EXTERNAL` (core 1.1), or
    /// `VK_QUEUE_FAMILY_FOREIGN_EXT` on a device that enabled
    /// `VK_EXT_queue_family_foreign` (`VUID-VkImageMemoryBarrier-srcQueueFamilyIndex-09100`
    /// and its twins) — never a transfer between the two external ones
    /// (`-04065`), and with `equal`, no transfer at all. Before stage S1 the
    /// barriers were passed through unjudged: an out-of-range family was the
    /// driver's to survive.
    ///
    /// # Errors
    /// Fatal, as a value no correct guest sends.
    fn check_families(
        &self,
        command: &'static str,
        device: u64,
        pairs: &[(u32, u32)],
        equal: bool,
    ) -> Result<(), ExecError> {
        use super::policy::{QUEUE_FAMILY_EXTERNAL, QUEUE_FAMILY_FOREIGN, QUEUE_FAMILY_IGNORED};
        let (object, guest) = self
            .objects
            .device_and_guest(device)
            .map_err(id_error(command))?;
        let foreign = object.enabled(super::policy::QUEUE_FAMILY_FOREIGN_EXT);
        let named = |f: u32| {
            f == QUEUE_FAMILY_IGNORED
                || f == QUEUE_FAMILY_EXTERNAL
                || (foreign && f == QUEUE_FAMILY_FOREIGN)
                || guest.has_family(f)
        };
        let external = |f: u32| f == QUEUE_FAMILY_EXTERNAL || f == QUEUE_FAMILY_FOREIGN;
        for &(src, dst) in pairs {
            if !named(src) || !named(dst) {
                return Err(invalid(
                    command,
                    format!(
                        "queue family indices {src:#x} -> {dst:#x}: each must be one of the \
                         device's families, IGNORED, EXTERNAL, or FOREIGN on a device that \
                         enabled VK_EXT_queue_family_foreign"
                    ),
                ));
            }
            if src != dst && (external(src) && external(dst) || equal) {
                return Err(invalid(
                    command,
                    format!("an ownership transfer {src:#x} -> {dst:#x} the command cannot make"),
                ));
            }
        }
        Ok(())
    }

    /// Stage S2b: every image barrier that releases an image out of the
    /// instance, on an image recorded as a handle blob's canonical image,
    /// sets that blob's release — the layout and family the renderer's
    /// scanout device acquires it from and hands it back to. Zink releases
    /// every exported image this way at the end of each batch
    /// (`zink_batch.c:900-934`: `oldLayout == newLayout == res->layout`,
    /// `srcQueueFamilyIndex` its own queue's family, `dst` `FOREIGN`).
    /// Recorded when the barrier is recorded, which for a frame Mutter flips
    /// is before the flip: Mutter commits a frame only once its fence has
    /// signalled (`meta-kms-impl-device.c:2089-2116`).
    fn note_releases(&self, device: u64, releases: &[Release]) {
        let Some(blobs) = self.blobs.as_ref() else {
            return;
        };
        for release in releases {
            let Some(scanout) = self
                .objects
                .image(device, release.image)
                .ok()
                .and_then(|image| image.scanout.as_ref())
            else {
                continue;
            };
            blobs.record_release(&scanout.shared, release.new_layout, release.dst_family);
        }
    }

    // ------------------------------------------------------ the three steps

    /// The device a command is dispatched on, by its first parameter.
    pub(super) fn device_of(&self, command: &Command<'_>) -> Result<u64, ExecError> {
        let name = command.name();
        let Some((kind, id)) = generated::dispatchable(command) else {
            return Err(ExecError::NotImplemented { command: name });
        };
        match kind {
            Kind::Device => {
                self.objects.device(id).map_err(id_error(name))?;
                Ok(id)
            }
            Kind::Queue => Ok(self.objects.queue(id).map_err(id_error(name))?.device),
            Kind::CommandBuffer => self.cmd_device(name, id),
            _ => Err(ExecError::NotImplemented { command: name }),
        }
    }

    /// The device of command buffer `id`.
    pub(super) fn cmd_device(&self, command: &'static str, id: u64) -> Result<u64, ExecError> {
        Ok(self
            .objects
            .raw_any(Kind::CommandBuffer, id)
            .map_err(id_error(command))?
            .device)
    }

    /// Refuse a command newer than the device's version as the guest sees
    /// it: the host's entry point for it may not exist.
    fn require_version(&self, command: &Command<'_>, device: u64) -> Result<(), ExecError> {
        let name = command.name();
        let (_, guest) = self
            .objects
            .device_and_guest(device)
            .map_err(id_error(name))?;
        let wanted = generated::min_api(command);
        if wanted == 0 {
            // An admitted extension's command (stage 5c): served on a device
            // the guest enabled one of the extensions that bring it on — the
            // host device was created with it, so its entry point exists.
            let brought = generated::extensions_of(command);
            let device_object = self.objects.device(device).map_err(id_error(name))?;
            if !brought.is_empty() && brought.iter().any(|e| device_object.enabled(e)) {
                return Ok(());
            }
            return Err(ExecError::NotEnabled {
                command: name,
                extensions: brought,
            });
        }
        let have = guest.api_version();
        let version = |v: u32| (v >> 22, (v >> 12) & 0x3ff);
        if version(wanted) > version(have) {
            let (major, minor) = version(wanted);
            return Err(ExecError::TooNew {
                command: name,
                major,
                minor,
            });
        }
        Ok(())
    }

    /// Translate `command`'s inputs against `device`.
    pub(super) fn translate(
        &self,
        device: u64,
        command: &mut Command<'_>,
    ) -> Result<(), ExecError> {
        let mut resolver = Resolver {
            objects: &self.objects,
            device,
            command: command.name(),
        };
        generated::translate(&mut resolver, command)
    }

    /// The generated host call, and a lost device noticed.
    pub(super) fn host_call(
        &mut self,
        device: u64,
        command: &mut Command<'_>,
    ) -> Result<(), ExecError> {
        let name = command.name();
        let host = self.objects.device(device).map_err(id_error(name))?;
        self.host
            .call(&host.host, command)
            .map_err(|error| ExecError::HostCall {
                command: name,
                error,
            })?;
        self.note_result(name, generated::result_of(command));
        Ok(())
    }

    /// Remember a `VK_ERROR_DEVICE_LOST`: the command is answered, and the
    /// context is fatal from the next one on (see the module docs of
    /// [`super`]).
    pub(super) fn note_result(&mut self, command: &'static str, ret: Option<i32>) {
        if ret == Some(crate::venus::protocol::VK_ERROR_DEVICE_LOST) {
            tracing::warn!(
                ctx_id = self.ctx_id,
                command,
                "the host device is lost; this Venus context ends after answering"
            );
            self.lost = true;
        }
    }

    /// A `[generated]` command: translate, call, answer.
    pub(super) fn pass_through(&mut self, command: &mut Command<'_>) -> Result<(), ExecError> {
        let device = self.device_of(command)?;
        self.require_version(command, device)?;
        self.translate(device, command)?;
        self.host_call(device, command)
    }

    // --------------------------------------------------- create, destroy

    /// Every create and allocate: the guest's ids (all new, all distinct,
    /// room for all), the call, then each id bound to the handle the host
    /// made — or, where it made none, answered with id 0, as vkr answers a
    /// failed element (`vkr_device_object.py`). Anything a failing call did
    /// make is destroyed rather than leaked.
    pub(super) fn create(
        &mut self,
        command: &mut Command<'_>,
        kind: Kind,
        facts: Facts,
        pool: u64,
    ) -> Result<(), ExecError> {
        self.create_each(command, kind, &[facts], pool)
    }

    /// [`Self::create`] with facts of their own for each object (one entry
    /// for all of them, or one per object).
    pub(super) fn create_each(
        &mut self,
        command: &mut Command<'_>,
        kind: Kind,
        facts: &[Facts],
        pool: u64,
    ) -> Result<(), ExecError> {
        let name = command.name();
        let device = self.device_of(command)?;
        self.require_version(command, device)?;
        let ids: Vec<u64> = generated::output_handles(command)
            .map(|(_, slots)| slots.iter().map(|h| **h).collect())
            .unwrap_or_default();
        if ids.is_empty() {
            return Err(invalid(name, "no handle to create"));
        }
        let mut seen = std::collections::HashSet::new();
        for id in &ids {
            self.objects.check_new(*id, kind).map_err(id_error(name))?;
            if !seen.insert(*id) {
                return Err(invalid(name, format!("id {id:#x} is named twice")));
            }
        }
        if self.objects.len().saturating_add(ids.len()) > super::objects::MAX_OBJECTS_PER_CONTEXT {
            // What a driver out of room says; nothing is created.
            generated::set_result(command, crate::venus::protocol::VK_ERROR_OUT_OF_HOST_MEMORY);
            zero_outputs(command);
            return Ok(());
        }
        self.translate(device, command)?;
        self.host_call(device, command)?;
        let ret = generated::result_of(command).unwrap_or(VK_SUCCESS);
        let raws: Vec<u64> = generated::output_handles(command)
            .map(|(_, slots)| slots.iter().map(|h| **h).collect())
            .unwrap_or_default();
        let host = &self.host;
        let device_host = &self.objects.device(device).map_err(id_error(name))?.host;
        let mut bound = Vec::with_capacity(ids.len());
        for (index, id) in ids.iter().enumerate() {
            let raw = raws.get(index).copied().unwrap_or(0);
            if raw == 0 {
                bound.push(0);
            } else if ret < 0 {
                // A failed call that made some objects anyway (pipelines):
                // the guest is told none were made, so none may stay.
                if !matches!(kind, Kind::CommandBuffer | Kind::DescriptorSet) {
                    host.destroy_object(device_host, kind, raw);
                }
                bound.push(0);
            } else {
                bound.push(*id);
            }
        }
        for (index, id) in bound.iter().enumerate() {
            if *id != 0 {
                let raw = raws.get(index).copied().unwrap_or(0);
                let facts = facts
                    .get(index)
                    .or_else(|| facts.first())
                    .cloned()
                    .unwrap_or(Facts::None);
                self.objects.insert_raw(
                    *id,
                    RawObject {
                        kind,
                        device,
                        host: raw,
                        pool,
                        facts,
                    },
                );
            }
        }
        if let Some((_, slots)) = generated::output_handles(command) {
            for (slot, id) in slots.into_iter().zip(&bound) {
                *slot = *id;
            }
        }
        Ok(())
    }

    /// Every simple `vkDestroy*`: id 0 is a no-op; otherwise the object must
    /// be a `kind` of `device`, and it is destroyed after any work that may
    /// still use it (vkr destroys at once; see [`Self::settle`]).
    fn destroy(&mut self, kind: Kind, device: u64, id: u64) -> Result<(), ExecError> {
        let name = kind.name();
        self.objects.device(device).map_err(id_error(name))?;
        if id == 0 {
            return Ok(());
        }
        self.objects.raw(kind, device, id).map_err(id_error(name))?;
        self.settle(device);
        let Some(object) = self
            .objects
            .take_raw(kind, device, id)
            .map_err(id_error(name))?
        else {
            return Ok(());
        };
        if kind == Kind::DescriptorPool {
            self.objects.forget_pool_children(id);
        }
        if object.facts == Facts::CustomBorderSampler {
            let device = self.objects.device_mut(device).map_err(id_error(name))?;
            device.custom_border_samplers = device.custom_border_samplers.saturating_sub(1);
        }
        let host = self.objects.device(device).map_err(id_error(name))?;
        self.host.destroy_object(&host.host, kind, object.host);
        Ok(())
    }

    /// `vkFreeCommandBuffers` / `vkFreeDescriptorSets`: every non-null id a
    /// `kind` of `pool` (vkr does not check; a set freed through another
    /// pool is a driver's heap corrupted), work that may use them done, the
    /// driver's free, and the ids gone.
    fn free_children(
        &mut self,
        command: &mut Command<'_>,
        kind: Kind,
        device: u64,
        pool: u64,
        ids: &[u64],
    ) -> Result<(), ExecError> {
        let name = command.name();
        for id in ids.iter().filter(|id| **id != 0) {
            let object = self
                .objects
                .raw(kind, device, *id)
                .map_err(id_error(name))?;
            if object.pool != pool {
                return Err(invalid(
                    name,
                    format!("{id:#x} was not allocated from that pool"),
                ));
            }
        }
        self.settle(device);
        self.pass_through(command)?;
        for id in ids.iter().filter(|id| **id != 0) {
            let _ = self.objects.take_raw(kind, device, *id);
        }
        Ok(())
    }

    // ---------------------------------------------------------- checks

    fn limits(
        &self,
        command: &'static str,
        device: u64,
    ) -> Result<&crate::venus::protocol::VkPhysicalDeviceLimits, ExecError> {
        let (_, guest) = self
            .objects
            .device_and_guest(device)
            .map_err(id_error(command))?;
        Ok(&guest.properties.properties.limits)
    }

    /// The admitted extensions' limits of `device`, as the guest was told
    /// them (stage 5c).
    fn extension_limits(
        &self,
        command: &'static str,
        device: u64,
    ) -> Result<ExtensionLimits, ExecError> {
        let (_, guest) = self
            .objects
            .device_and_guest(device)
            .map_err(id_error(command))?;
        Ok(ExtensionLimits::of(guest))
    }

    /// Whether `device` was created with robustness2's `nullDescriptor`.
    fn null_descriptor(&self, command: &'static str, device: u64) -> Result<bool, ExecError> {
        Ok(self
            .objects
            .device(device)
            .map_err(id_error(command))?
            .null_descriptor)
    }

    /// Buffer `id`'s size and usage.
    fn buffer_size_usage(
        &self,
        command: &'static str,
        device: u64,
        id: u64,
    ) -> Result<(u64, u32), ExecError> {
        let buffer = self.objects.buffer(device, id).map_err(id_error(command))?;
        Ok((buffer.size, buffer.usage))
    }

    /// `vkCreateSampler` (stage 5c: custom border colours). A sampler with
    /// one — a custom border colour value, or a chained
    /// `VkSamplerCustomBorderColorCreateInfoEXT` — takes one of the device's
    /// `maxCustomBorderColorSamplers` entries, which a driver keeps in a
    /// fixed table (`VUID-VkSamplerCreateInfo-None-04012`); past them it is
    /// refused, and the entry is given back when the sampler goes.
    fn create_sampler(&mut self, device: u64, command: &mut Command<'_>) -> Result<(), ExecError> {
        use crate::venus::protocol::VkSamplerCreateInfoNext as N;
        const NAME: &str = "vkCreateSampler";
        let Command::CreateSampler(args) = &*command else {
            return Err(invalid(NAME, "not a vkCreateSampler"));
        };
        let custom = args.p_create_info.as_ref().is_some_and(|info| {
            super::policy::is_custom_border_color(info.border_color)
                || info
                    .p_next
                    .iter()
                    .any(|l| matches!(l, N::VkSamplerCustomBorderColorCreateInfoEXT(_)))
        });
        let id = args.p_sampler.map_or(0, |h| h.0);
        if !custom {
            return self.create(command, Kind::Sampler, Facts::None, 0);
        }
        let limit = self.extension_limits(NAME, device)?.custom_border_samplers;
        let live = self
            .objects
            .device(device)
            .map_err(id_error(NAME))?
            .custom_border_samplers;
        if live >= limit {
            return Err(invalid(
                NAME,
                format!("a custom border colour sampler past maxCustomBorderColorSamplers {limit}"),
            ));
        }
        self.create(command, Kind::Sampler, Facts::CustomBorderSampler, 0)?;
        if self.objects.raw(Kind::Sampler, device, id).is_ok() {
            let device = self.objects.device_mut(device).map_err(id_error(NAME))?;
            device.custom_border_samplers = device.custom_border_samplers.saturating_add(1);
        }
        Ok(())
    }

    /// Counter buffers of `vkCmd{Begin,End}TransformFeedbackEXT`: inside
    /// `maxTransformFeedbackBuffers`, and each one named 4 aligned bytes
    /// inside a `TRANSFORM_FEEDBACK_COUNTER` buffer (a null one means none).
    fn check_counter_buffers(
        &self,
        command: &'static str,
        device: u64,
        (first, count): (u32, u32),
        buffers: Option<&[crate::venus::protocol::VkBuffer]>,
        offsets: Option<&[u64]>,
    ) -> Result<(), ExecError> {
        let ext = self.extension_limits(command, device)?;
        if ranges_overflow(
            u64::from(first),
            u64::from(count),
            u64::from(ext.tf_buffers),
        ) {
            return Err(invalid(
                command,
                format!(
                    "counter buffers {first}+{count} past maxTransformFeedbackBuffers {}",
                    ext.tf_buffers
                ),
            ));
        }
        let offsets = offsets.unwrap_or_default();
        for (index, buffer) in buffers.unwrap_or_default().iter().enumerate() {
            if buffer.0 == 0 {
                continue;
            }
            let (size, usage) = self.buffer_size_usage(command, device, buffer.0)?;
            let offset = offsets.get(index).copied().unwrap_or(0);
            if usage & super::policy::BUFFER_USAGE_TRANSFORM_FEEDBACK_COUNTER == 0
                || offset % 4 != 0
                || ranges_overflow(offset, 4, size)
            {
                return Err(invalid(
                    command,
                    "a counter that is not 4 aligned bytes inside a TRANSFORM_FEEDBACK_COUNTER \
                     buffer",
                ));
            }
        }
        Ok(())
    }

    /// `vkCmd{Begin,End}QueryIndexedEXT`: the query inside its pool, and the
    /// index a transform feedback stream the device has — 0 for any other
    /// query type (`VUID-vkCmdBeginQueryIndexedEXT-queryType-06692`).
    fn check_indexed_query(
        &self,
        command: &'static str,
        cb: u64,
        pool: u64,
        query: u32,
        index: u32,
    ) -> Result<(), ExecError> {
        let device = self.cmd_device(command, cb)?;
        self.check_queries(command, device, pool, query, 1)?;
        let query_type = match self
            .objects
            .raw(Kind::QueryPool, device, pool)
            .map_err(id_error(command))?
            .facts
        {
            Facts::QueryPool { query_type, .. } => query_type,
            _ => return Err(invalid(command, "not a query pool")),
        };
        let streams = if query_type == super::policy::QUERY_TYPE_TRANSFORM_FEEDBACK_STREAM {
            self.extension_limits(command, device)?.tf_streams
        } else {
            1
        };
        if index >= streams {
            return Err(invalid(
                command,
                format!("index {index} of a query pool of {streams} streams"),
            ));
        }
        Ok(())
    }

    fn buffer_size(&self, command: &'static str, device: u64, id: u64) -> Result<u64, ExecError> {
        Ok(self
            .objects
            .buffer(device, id)
            .map_err(id_error(command))?
            .size)
    }

    /// No shader stage may carry its module in its chain (maintenance5, not
    /// enabled on any host device here); the module itself must be named,
    /// which translation checks.
    fn check_stages<'s>(
        &self,
        command: &'static str,
        stages: impl Iterator<Item = &'s crate::venus::protocol::VkPipelineShaderStageCreateInfo<'s>>,
    ) -> Result<(), ExecError> {
        use crate::venus::protocol::VkPipelineShaderStageCreateInfoNext as N;
        for stage in stages {
            if stage
                .p_next
                .iter()
                .any(|l| matches!(l, N::VkShaderModuleCreateInfo(_)))
            {
                return Err(invalid(
                    command,
                    "a shader stage chains its module (VK_KHR_maintenance5), which no device here enables",
                ));
            }
            // The driver reads every map entry's bytes out of pData: each
            // must be inside it (the wire already made pData dataSize long).
            if let Some(spec) = &stage.p_specialization_info {
                let data = spec.data_size;
                if spec
                    .p_map_entries
                    .iter()
                    .flatten()
                    .any(|e| ranges_overflow(u64::from(e.offset), e.size, data))
                {
                    return Err(invalid(command, "a specialization constant outside pData"));
                }
            }
        }
        Ok(())
    }

    /// A pipeline layout's facts, and its bounds: sets within
    /// `maxBoundDescriptorSets`, push constant ranges within
    /// `maxPushConstantsSize`.
    fn pipeline_layout_facts(
        &self,
        args: &crate::venus::protocol::CreatePipelineLayoutArgs,
    ) -> Result<Facts, ExecError> {
        const NAME: &str = "vkCreatePipelineLayout";
        let Some(info) = &args.p_create_info else {
            return Err(invalid(NAME, "pCreateInfo is null"));
        };
        let limits = self.limits(NAME, args.device.0)?;
        if info.set_layout_count > limits.max_bound_descriptor_sets {
            return Err(invalid(
                NAME,
                "more set layouts than maxBoundDescriptorSets",
            ));
        }
        let push = u64::from(limits.max_push_constants_size);
        if info
            .p_push_constant_ranges
            .iter()
            .flatten()
            .any(|r| ranges_overflow(u64::from(r.offset), u64::from(r.size), push))
        {
            return Err(invalid(
                NAME,
                "a push constant range past maxPushConstantsSize",
            ));
        }
        Ok(Facts::PipelineLayout {
            sets: info.set_layout_count,
        })
    }

    /// The attachment count and clear count of render pass `id`.
    fn render_pass_facts_of(
        &self,
        command: &'static str,
        device: u64,
        id: u64,
    ) -> Result<(u32, u32), ExecError> {
        match self
            .objects
            .raw(Kind::RenderPass, device, id)
            .map_err(id_error(command))?
            .facts
        {
            Facts::RenderPass {
                attachments,
                clears,
            } => Ok((attachments, clears)),
            _ => Err(invalid(command, "not a render pass")),
        }
    }

    /// A render pass begin: its framebuffer made for as many attachments as
    /// the pass has (a driver indexes one by the other), and a clear value
    /// for every attachment the pass clears.
    fn check_render_pass_begin(
        &self,
        command: &'static str,
        device: u64,
        begin: &crate::venus::protocol::VkRenderPassBeginInfo,
    ) -> Result<(), ExecError> {
        use crate::venus::protocol::VkRenderPassBeginInfoNext as N;
        let (attachments, clears) =
            self.render_pass_facts_of(command, device, begin.render_pass.0)?;
        let framebuffer = self
            .objects
            .raw(Kind::Framebuffer, device, begin.framebuffer.0)
            .map_err(id_error(command))?;
        let Facts::Framebuffer {
            imageless,
            attachments: fb_attachments,
        } = framebuffer.facts
        else {
            return Err(invalid(command, "not a framebuffer"));
        };
        if fb_attachments != attachments {
            return Err(invalid(
                command,
                "a framebuffer of another attachment count than the render pass",
            ));
        }
        if imageless {
            let given = begin.p_next.iter().find_map(|l| match l {
                N::VkRenderPassAttachmentBeginInfo(a) => Some(a.attachment_count),
                #[allow(unreachable_patterns)]
                _ => None,
            });
            if given != Some(attachments) {
                return Err(invalid(
                    command,
                    "an imageless framebuffer begun without one view per attachment",
                ));
            }
        }
        if begin.clear_value_count < clears {
            return Err(invalid(
                command,
                format!(
                    "{} clear values for a pass that clears {clears} attachments",
                    begin.clear_value_count
                ),
            ));
        }
        Ok(())
    }

    /// A dynamic-rendering scope (stage 5b.3), bounded as a render pass's
    /// attachments are: colour attachments inside `maxColorAttachments` (the
    /// driver keeps them in an array of that size), a layer count, a view
    /// mask inside `maxMultiviewViewCount`, and a nonzero render area. Every
    /// view is translated like any other id (of this device, or null where
    /// Vulkan lets an attachment be absent), and every layout, load and
    /// store op and resolve mode is range-checked by the generated walk.
    fn check_rendering(
        &self,
        command: &'static str,
        device: u64,
        info: &crate::venus::protocol::VkRenderingInfo,
    ) -> Result<(), ExecError> {
        let limits = self.limits(command, device)?;
        if info.color_attachment_count > limits.max_color_attachments {
            return Err(invalid(
                command,
                format!(
                    "{} colour attachments, past maxColorAttachments {}",
                    info.color_attachment_count, limits.max_color_attachments
                ),
            ));
        }
        let views = self.max_multiview_views(command, device)?;
        if info.view_mask != 0 && 32 - info.view_mask.leading_zeros() > views {
            return Err(invalid(
                command,
                format!(
                    "view mask {:#x} past maxMultiviewViewCount {views}",
                    info.view_mask
                ),
            ));
        }
        if info.view_mask == 0 && info.layer_count == 0 {
            return Err(invalid(command, "a layer count of 0 without multiview"));
        }
        if info.render_area.extent.width == 0 || info.render_area.extent.height == 0 {
            return Err(invalid(command, "an empty render area"));
        }
        Ok(())
    }

    /// `maxMultiviewViewCount` as the guest was told it (0 when the host's
    /// chain did not carry it: no multiview).
    fn max_multiview_views(&self, command: &'static str, device: u64) -> Result<u32, ExecError> {
        use crate::venus::protocol::VkPhysicalDeviceProperties2Next as N;
        let (_, guest) = self
            .objects
            .device_and_guest(device)
            .map_err(id_error(command))?;
        Ok(guest
            .properties
            .p_next
            .iter()
            .find_map(|l| match l {
                N::VkPhysicalDeviceVulkan11Properties(p) => Some(p.max_multiview_view_count),
                N::VkPhysicalDeviceMultiviewProperties(p) => Some(p.max_multiview_view_count),
                _ => None,
            })
            .unwrap_or(0)
            .min(32))
    }

    /// The layout facts of descriptor set `set`.
    fn set_facts(
        &self,
        command: &'static str,
        device: u64,
        set: u64,
    ) -> Result<(std::sync::Arc<super::objects::SetLayoutInfo>, u32), ExecError> {
        match &self
            .objects
            .raw(Kind::DescriptorSet, device, set)
            .map_err(id_error(command))?
            .facts
        {
            Facts::DescriptorSet { layout, variable } => {
                Ok((std::sync::Arc::clone(layout), *variable))
            }
            _ => Err(invalid(command, "not a descriptor set")),
        }
    }

    /// `vkCopyDescriptorSet`: both ranges inside their sets' bindings.
    fn check_copy(
        &self,
        device: u64,
        copy: &crate::venus::protocol::VkCopyDescriptorSet,
    ) -> Result<(), ExecError> {
        const NAME: &str = "vkUpdateDescriptorSets";
        let (src, src_var) = self.set_facts(NAME, device, copy.src_set.0)?;
        let (dst, dst_var) = self.set_facts(NAME, device, copy.dst_set.0)?;
        let src_type = binding_span(
            &src,
            src_var,
            copy.src_binding,
            copy.src_array_element,
            copy.descriptor_count,
        )
        .ok_or_else(|| invalid(NAME, "a copy's source range is outside its set"))?;
        let dst_type = binding_span(
            &dst,
            dst_var,
            copy.dst_binding,
            copy.dst_array_element,
            copy.descriptor_count,
        )
        .ok_or_else(|| invalid(NAME, "a copy's destination range is outside its set"))?;
        if src_type != dst_type {
            return Err(invalid(NAME, "a copy between bindings of different types"));
        }
        Ok(())
    }

    /// The array a write's descriptor type reads is there and whole, every
    /// handle in it named, and every buffer range inside its buffer.
    fn check_write(&self, device: u64, write: &VkWriteDescriptorSet<'_>) -> Result<(), ExecError> {
        use descriptor as d;
        const NAME: &str = "vkUpdateDescriptorSets";
        // Where the write lands: a driver writes descriptor memory at the
        // binding's offset plus the element, so the whole range must be the
        // set's own, in bindings of the write's type (a write runs on into
        // the next binding, as Vulkan allows).
        let (layout, variable) = self.set_facts(NAME, device, write.dst_set.0)?;
        let landed = binding_span(
            &layout,
            variable,
            write.dst_binding,
            write.dst_array_element,
            write.descriptor_count,
        );
        if landed != Some(write.descriptor_type) {
            return Err(invalid(
                NAME,
                format!(
                    "{} descriptors of type {} at binding {} element {} are not the set's own",
                    write.descriptor_count,
                    write.descriptor_type,
                    write.dst_binding,
                    write.dst_array_element
                ),
            ));
        }
        let count = usize::try_from(write.descriptor_count).unwrap_or(usize::MAX);
        let whole = |len: Option<usize>| len == Some(count);
        // robustness2's `nullDescriptor` (stage 5c): a null view or buffer is
        // a descriptor that reads zero, which Zink binds for every unbound
        // slot.
        let null_ok = self.null_descriptor(NAME, device)?;
        match write.descriptor_type {
            d::SAMPLER
            | d::COMBINED_IMAGE_SAMPLER
            | d::SAMPLED_IMAGE
            | d::STORAGE_IMAGE
            | d::INPUT_ATTACHMENT => {
                let infos: &[VkDescriptorImageInfo] =
                    write.p_image_info.as_deref().unwrap_or_default();
                if !whole(write.p_image_info.as_ref().map(Vec::len)) {
                    return Err(invalid(NAME, "an image write without its pImageInfo"));
                }
                if write.descriptor_type != d::SAMPLER
                    && !null_ok
                    && infos.iter().any(|i| i.image_view.0 == 0)
                {
                    return Err(invalid(NAME, "an image descriptor with no image view"));
                }
            }
            d::UNIFORM_TEXEL_BUFFER | d::STORAGE_TEXEL_BUFFER => {
                let views = write.p_texel_buffer_view.as_deref().unwrap_or_default();
                if !whole(write.p_texel_buffer_view.as_ref().map(Vec::len))
                    || (!null_ok && views.iter().any(|v| v.0 == 0))
                {
                    return Err(invalid(NAME, "a texel buffer write without its views"));
                }
            }
            d::UNIFORM_BUFFER
            | d::STORAGE_BUFFER
            | d::UNIFORM_BUFFER_DYNAMIC
            | d::STORAGE_BUFFER_DYNAMIC => {
                if !whole(write.p_buffer_info.as_ref().map(Vec::len)) {
                    return Err(invalid(NAME, "a buffer write without its pBufferInfo"));
                }
                for info in write.p_buffer_info.iter().flatten() {
                    if info.buffer.0 == 0 && null_ok {
                        // VUID-VkDescriptorBufferInfo-buffer-02999.
                        if info.offset != 0 || info.range != WHOLE_SIZE {
                            return Err(invalid(
                                NAME,
                                "a null buffer descriptor with an offset or a range",
                            ));
                        }
                        continue;
                    }
                    let size = self.buffer_size(NAME, device, info.buffer.0)?;
                    let range_bad = info.range != WHOLE_SIZE
                        && (info.range == 0 || ranges_overflow(info.offset, info.range, size));
                    if info.offset >= size || range_bad {
                        return Err(invalid(
                            NAME,
                            "a buffer descriptor's range is outside its buffer",
                        ));
                    }
                }
            }
            d::INLINE_UNIFORM_BLOCK => {
                let block = write.p_next.iter().find_map(|l| match l {
                    VkWriteDescriptorSetNext::VkWriteDescriptorSetInlineUniformBlock(b) => Some(b),
                    _ => None,
                });
                if block.is_none_or(|b| b.data_size != write.descriptor_count) {
                    return Err(invalid(
                        NAME,
                        "an inline uniform block write whose data is not descriptorCount bytes",
                    ));
                }
            }
            // Translation refuses every value outside core 1.3.
            _ => {}
        }
        Ok(())
    }

    fn check_vertex_buffers(
        &self,
        command: &'static str,
        device: u64,
        (first, count): (u32, u32),
        buffers: Option<&[crate::venus::protocol::VkBuffer]>,
        offsets: Option<&[u64]>,
        sizes: Option<&[u64]>,
    ) -> Result<(), ExecError> {
        let limit = self.limits(command, device)?.max_vertex_input_bindings;
        if ranges_overflow(u64::from(first), u64::from(count), u64::from(limit)) {
            return Err(invalid(
                command,
                "a binding range past maxVertexInputBindings",
            ));
        }
        let buffers = buffers.unwrap_or_default();
        let offsets = offsets.unwrap_or_default();
        let null_ok = self.null_descriptor(command, device)?;
        for (index, buffer) in buffers.iter().enumerate() {
            let offset = offsets.get(index).copied().unwrap_or(0);
            if buffer.0 == 0 {
                // A null vertex buffer reads zero with robustness2's
                // `nullDescriptor`, and its offset must be 0
                // (`VUID-vkCmdBindVertexBuffers-pBuffers-04001/04002`).
                if !null_ok || offset != 0 {
                    return Err(invalid(
                        command,
                        "a null vertex buffer without nullDescriptor, or with an offset",
                    ));
                }
                continue;
            }
            let size = self.buffer_size(command, device, buffer.0)?;
            let range = sizes
                .and_then(|s| s.get(index))
                .copied()
                .unwrap_or(WHOLE_SIZE);
            if offset >= size || (range != WHOLE_SIZE && ranges_overflow(offset, range, size)) {
                return Err(invalid(command, "a vertex buffer range outside its buffer"));
            }
        }
        Ok(())
    }

    fn check_viewports(
        &self,
        command: &'static str,
        cb: u64,
        first: u32,
        count: u32,
    ) -> Result<(), ExecError> {
        let device = self.cmd_device(command, cb)?;
        let limit = self.limits(command, device)?.max_viewports;
        if count == 0 || ranges_overflow(u64::from(first), u64::from(count), u64::from(limit)) {
            return Err(invalid(
                command,
                format!("{count} at {first} is past maxViewports {limit}"),
            ));
        }
        Ok(())
    }

    fn check_buffer_copies(
        &self,
        command: &'static str,
        device: u64,
        src: u64,
        dst: u64,
        regions: &[(u64, u64, u64)],
    ) -> Result<(), ExecError> {
        let src_size = self.buffer_size(command, device, src)?;
        let dst_size = self.buffer_size(command, device, dst)?;
        for (src_offset, dst_offset, size) in regions {
            if *size == 0
                || ranges_overflow(*src_offset, *size, src_size)
                || ranges_overflow(*dst_offset, *size, dst_size)
            {
                return Err(invalid(command, "a copy region outside its buffers"));
            }
        }
        Ok(())
    }

    /// Where a buffer-image copy starts in its buffer. The footprint behind
    /// that offset depends on the image's format and the region's extent
    /// and is the driver's to judge, as in vkr; `robustBufferAccess` does
    /// not cover transfers, so this is where the posture stops short.
    fn check_image_copy_offsets(
        &self,
        command: &'static str,
        device: u64,
        buffer: u64,
        offsets: &[u64],
    ) -> Result<(), ExecError> {
        let size = self.buffer_size(command, device, buffer)?;
        if offsets.iter().any(|o| *o >= size) {
            return Err(invalid(command, "a region starts outside its buffer"));
        }
        Ok(())
    }

    fn check_indirect(
        &self,
        command: &'static str,
        device: u64,
        buffer: u64,
        offset: u64,
        (count, stride): (u32, u32),
        record: u64,
    ) -> Result<(), ExecError> {
        let size = self.buffer_size(command, device, buffer)?;
        if offset % 4 != 0 {
            return Err(invalid(command, "an indirect offset that is not 4-aligned"));
        }
        if count == 0 {
            return Ok(());
        }
        if count > 1 && (u64::from(stride) < record || stride % 4 != 0) {
            return Err(invalid(
                command,
                format!("stride {stride} for {record}-byte records"),
            ));
        }
        let span = u64::from(count - 1)
            .checked_mul(u64::from(stride))
            .and_then(|s| s.checked_add(record));
        if span.is_none_or(|span| ranges_overflow(offset, span, size)) {
            return Err(invalid(command, "the indirect records run past the buffer"));
        }
        Ok(())
    }

    fn query_pool_facts(
        &self,
        command: &'static str,
        device: u64,
        pool: u64,
    ) -> Result<(u32, u32), ExecError> {
        match self
            .objects
            .raw(Kind::QueryPool, device, pool)
            .map_err(id_error(command))?
            .facts
        {
            Facts::QueryPool { count, values, .. } => Ok((count, values)),
            _ => Err(invalid(command, "not a query pool")),
        }
    }

    fn check_queries(
        &self,
        command: &'static str,
        device: u64,
        pool: u64,
        first: u32,
        count: u32,
    ) -> Result<(), ExecError> {
        let (queries, _) = self.query_pool_facts(command, device, pool)?;
        if ranges_overflow(u64::from(first), u64::from(count), u64::from(queries)) {
            return Err(invalid(
                command,
                format!("queries {first}+{count} of a pool of {queries}"),
            ));
        }
        Ok(())
    }

    fn check_cmd_queries(
        &self,
        command: &'static str,
        cb: u64,
        pool: u64,
        first: u32,
        count: u32,
    ) -> Result<(), ExecError> {
        let device = self.cmd_device(command, cb)?;
        self.check_queries(command, device, pool, first, count)
    }

    /// The bytes `count` results from `first` take at `stride` with
    /// `flags`: the last one's end. Refuses a range outside the pool and a
    /// stride the results do not fit.
    fn query_results_size(
        &self,
        command: &'static str,
        device: u64,
        pool: u64,
        (first, count): (u32, u32),
        stride: u64,
        flags: u32,
    ) -> Result<u64, ExecError> {
        self.check_queries(command, device, pool, first, count)?;
        let (_, values) = self.query_pool_facts(command, device, pool)?;
        let word: u64 = if flags & RESULT_64 != 0 { 8 } else { 4 };
        let per = u64::from(values + u32::from(flags & RESULT_WITH_AVAILABILITY != 0)) * word;
        if count == 0 {
            return Ok(0);
        }
        if stride % word != 0 || (count > 1 && stride < per) {
            return Err(invalid(
                command,
                format!("stride {stride} for {per}-byte results"),
            ));
        }
        u64::from(count - 1)
            .checked_mul(stride)
            .and_then(|s| s.checked_add(per))
            .ok_or_else(|| invalid(command, "the results overflow"))
    }

    /// `vkSetPrivateData`/`vkGetPrivateData`'s `objectHandle`, an id of
    /// `objectType`, as the host handle; types whose host object is not a
    /// plain handle here are refused.
    fn private_data_object(
        &self,
        command: &'static str,
        device: u64,
        object_type: i32,
        id: u64,
    ) -> Result<u64, ExecError> {
        let kind = match object_type {
            4 => Kind::Queue,
            6 => Kind::CommandBuffer,
            7 => Kind::Fence,
            8 => Kind::DeviceMemory,
            9 => Kind::Buffer,
            10 => Kind::Image,
            11 => Kind::Event,
            12 => Kind::QueryPool,
            13 => Kind::BufferView,
            14 => Kind::ImageView,
            15 => Kind::ShaderModule,
            16 => Kind::PipelineCache,
            17 => Kind::PipelineLayout,
            18 => Kind::RenderPass,
            19 => Kind::Pipeline,
            20 => Kind::DescriptorSetLayout,
            21 => Kind::Sampler,
            22 => Kind::DescriptorPool,
            23 => Kind::DescriptorSet,
            24 => Kind::Framebuffer,
            25 => Kind::CommandPool,
            1_000_156_000 => Kind::SamplerYcbcrConversion,
            1_000_085_000 => Kind::DescriptorUpdateTemplate,
            1_000_295_000 => Kind::PrivateDataSlot,
            _ => {
                return Err(invalid(
                    command,
                    format!("private data on object type {object_type} is not served"),
                ))
            }
        };
        let mut resolver = Resolver {
            objects: &self.objects,
            device,
            command,
        };
        resolver.handle(kind, id, false, "objectHandle")
    }
}

/// `VK_ATTACHMENT_UNUSED` / `VK_SUBPASS_EXTERNAL`.
const UNUSED: u32 = u32::MAX;
/// `VK_ATTACHMENT_LOAD_OP_CLEAR`.
const LOAD_OP_CLEAR: i32 = 1;
/// `VK_DESCRIPTOR_BINDING_VARIABLE_DESCRIPTOR_COUNT_BIT`.
const BINDING_VARIABLE_COUNT: u32 = 0x8;

/// The type of the descriptors `count` elements from `element` of `binding`
/// on land in — running on into the following bindings as Vulkan's
/// consecutive-binding rule allows, each of the same type — or `None` when
/// the range leaves the set. An inline uniform block counts bytes and never
/// runs on.
fn binding_span(
    layout: &super::objects::SetLayoutInfo,
    variable: u32,
    binding: u32,
    element: u32,
    count: u32,
) -> Option<i32> {
    let size = |b: &super::objects::LayoutBinding| {
        if Some(b.binding) == layout.variable {
            variable
        } else {
            b.count
        }
    };
    let first = layout.binding(binding)?;
    let ty = first.descriptor_type;
    if ty == descriptor::INLINE_UNIFORM_BLOCK {
        return (u64::from(element) + u64::from(count) <= u64::from(size(first))).then_some(ty);
    }
    let mut left = u64::from(count);
    let mut element = u64::from(element);
    let mut current = *first;
    loop {
        if current.descriptor_type != ty {
            return None;
        }
        let avail = u64::from(size(&current)).checked_sub(element)?;
        let take = avail.min(left);
        left -= take;
        if left == 0 {
            return Some(ty);
        }
        element = 0;
        // The next binding by number; bindings of no descriptors are
        // passed over, as Vulkan says.
        let mut next = current.binding.checked_add(1)?;
        loop {
            let candidate = layout.binding(next)?;
            if candidate.count != 0 {
                current = *candidate;
                break;
            }
            next = next.checked_add(1)?;
        }
    }
}

/// What a descriptor set layout's sets will be judged by: its bindings
/// (numbers unique), its dynamic descriptors, its variable-count binding.
fn set_layout_info(
    args: &crate::venus::protocol::CreateDescriptorSetLayoutArgs,
) -> Result<super::objects::SetLayoutInfo, ExecError> {
    use crate::venus::protocol::VkDescriptorSetLayoutCreateInfoNext as N;
    const NAME: &str = "vkCreateDescriptorSetLayout";
    let Some(info) = &args.p_create_info else {
        return Err(invalid(NAME, "pCreateInfo is null"));
    };
    let given = info.p_bindings.as_deref().unwrap_or_default();
    let flags: &[u32] = info
        .p_next
        .iter()
        .find_map(|l| match l {
            N::VkDescriptorSetLayoutBindingFlagsCreateInfo(f) => {
                Some(f.p_binding_flags.as_deref().unwrap_or_default())
            }
            #[allow(unreachable_patterns)]
            _ => None,
        })
        .unwrap_or_default();
    if !flags.is_empty() && flags.len() != given.len() {
        return Err(invalid(
            NAME,
            "binding flags for a different number of bindings",
        ));
    }
    let mut out = super::objects::SetLayoutInfo::default();
    for (index, b) in given.iter().enumerate() {
        if matches!(
            b.descriptor_type,
            descriptor::UNIFORM_BUFFER_DYNAMIC | descriptor::STORAGE_BUFFER_DYNAMIC
        ) {
            out.dynamic = out.dynamic.saturating_add(b.descriptor_count);
        }
        if flags
            .get(index)
            .is_some_and(|f| f & BINDING_VARIABLE_COUNT != 0)
        {
            if out.variable.is_some() {
                return Err(invalid(NAME, "two variable-count bindings"));
            }
            out.variable = Some(b.binding);
        }
        out.bindings.push(super::objects::LayoutBinding {
            binding: b.binding,
            descriptor_type: b.descriptor_type,
            count: b.descriptor_count,
        });
    }
    out.bindings.sort_unstable_by_key(|b| b.binding);
    if out
        .bindings
        .windows(2)
        .any(|w| w[0].binding == w[1].binding)
    {
        return Err(invalid(NAME, "a binding number used twice"));
    }
    if let Some(variable) = out.variable {
        if out.bindings.last().map(|b| b.binding) != Some(variable) {
            return Err(invalid(
                NAME,
                "the variable-count binding is not the last one",
            ));
        }
    }
    Ok(out)
}

/// A graphics pipeline's fixed-function counts against what a driver keeps
/// in arrays of the device's limits: vertex bindings and attributes,
/// viewports and scissors, colour attachments; and one stage of each kind.
fn check_graphics_state(
    command: &'static str,
    info: &crate::venus::protocol::VkGraphicsPipelineCreateInfo<'_>,
    limits: &crate::venus::protocol::VkPhysicalDeviceLimits,
    ext: &ExtensionLimits,
) -> Result<(), ExecError> {
    use crate::venus::protocol::VkPipelineRasterizationStateCreateInfoNext as R;
    use crate::venus::protocol::VkPipelineVertexInputStateCreateInfoNext as V;
    let mut stages = 0u32;
    for stage in info.p_stages.iter().flatten() {
        let bit = u32::try_from(stage.stage).unwrap_or(0);
        if stages & bit != 0 {
            return Err(invalid(command, "a shader stage named twice"));
        }
        stages |= bit;
    }
    if let Some(vi) = &info.p_vertex_input_state {
        let bindings = limits.max_vertex_input_bindings;
        let attributes = limits.max_vertex_input_attributes;
        if vi.vertex_binding_description_count > bindings
            || vi.vertex_attribute_description_count > attributes
            || vi
                .p_vertex_binding_descriptions
                .iter()
                .flatten()
                .any(|b| b.binding >= bindings)
            || vi
                .p_vertex_attribute_descriptions
                .iter()
                .flatten()
                .any(|a| a.location >= attributes || a.binding >= bindings)
        {
            return Err(invalid(
                command,
                "vertex input past the device's binding or attribute limits",
            ));
        }
        // Stage 5c: divisors name bindings a driver indexes by.
        for link in &vi.p_next {
            let V::VkPipelineVertexInputDivisorStateCreateInfo(d) = link;
            if d.vertex_binding_divisor_count > bindings
                || d.p_vertex_binding_divisors
                    .iter()
                    .flatten()
                    .any(|b| b.binding >= bindings || b.divisor > ext.max_divisor)
            {
                return Err(invalid(
                    command,
                    "a vertex divisor past maxVertexInputBindings or maxVertexAttribDivisor",
                ));
            }
        }
    }
    // Stage 5c: the rasterization state's extension structures.
    for link in info
        .p_rasterization_state
        .iter()
        .flat_map(|r| r.p_next.iter())
    {
        match link {
            R::VkPipelineRasterizationStateStreamCreateInfoEXT(s)
                if s.rasterization_stream >= ext.tf_streams =>
            {
                return Err(invalid(
                    command,
                    format!(
                        "rasterization stream {} past maxTransformFeedbackStreams {}",
                        s.rasterization_stream, ext.tf_streams
                    ),
                ));
            }
            R::VkPipelineRasterizationLineStateCreateInfo(l)
                if l.stippled_line_enable != 0 && !stipple_factor_ok(l.line_stipple_factor) =>
            {
                return Err(invalid(
                    command,
                    format!(
                        "lineStippleFactor {} is outside [1, 256]",
                        l.line_stipple_factor
                    ),
                ));
            }
            _ => {}
        }
    }
    if let Some(vp) = &info.p_viewport_state {
        if vp.viewport_count > limits.max_viewports || vp.scissor_count > limits.max_viewports {
            return Err(invalid(command, "more viewports than maxViewports"));
        }
    }
    if let Some(cb) = &info.p_color_blend_state {
        if cb.attachment_count > limits.max_color_attachments {
            return Err(invalid(
                command,
                "more blend attachments than maxColorAttachments",
            ));
        }
    }
    // A pipeline for dynamic rendering (stage 5b.3) names its colour
    // formats instead of a render pass, as many as a begin may bind.
    for link in &info.p_next {
        if let crate::venus::protocol::VkGraphicsPipelineCreateInfoNext::VkPipelineRenderingCreateInfo(r) =
            link
        {
            if r.color_attachment_count > limits.max_color_attachments {
                return Err(invalid(
                    command,
                    "more rendering colour formats than maxColorAttachments",
                ));
            }
        }
    }
    Ok(())
}

fn attachment_ok(index: u32, attachments: u32) -> bool {
    index == UNUSED || index < attachments
}

fn subpass_ok(index: u32, subpasses: u32) -> bool {
    index == UNUSED || index < subpasses
}

/// A render pass's facts, and the indices a driver resolves inside its own
/// create info: every attachment reference, preserve index and dependency
/// inside the pass, colour attachments within the limit, multiview arrays
/// one per subpass and per dependency.
fn render_pass_facts(
    args: &crate::venus::protocol::CreateRenderPassArgs,
    limits: &crate::venus::protocol::VkPhysicalDeviceLimits,
) -> Result<Facts, ExecError> {
    use crate::venus::protocol::VkRenderPassCreateInfoNext as N;
    const NAME: &str = "vkCreateRenderPass";
    let Some(info) = &args.p_create_info else {
        return Err(invalid(NAME, "pCreateInfo is null"));
    };
    let n = info.attachment_count;
    let subpasses = info.subpass_count;
    for s in info.p_subpasses.iter().flatten() {
        if s.color_attachment_count > limits.max_color_attachments {
            return Err(invalid(
                NAME,
                "more colour attachments than maxColorAttachments",
            ));
        }
        let refs = s
            .p_input_attachments
            .iter()
            .flatten()
            .chain(s.p_color_attachments.iter().flatten())
            .chain(s.p_resolve_attachments.iter().flatten())
            .chain(s.p_depth_stencil_attachment.iter());
        if refs.map(|r| r.attachment).any(|a| !attachment_ok(a, n))
            || s.p_preserve_attachments.iter().flatten().any(|a| *a >= n)
        {
            return Err(invalid(NAME, "an attachment reference outside the pass"));
        }
    }
    if info
        .p_dependencies
        .iter()
        .flatten()
        .any(|d| !subpass_ok(d.src_subpass, subpasses) || !subpass_ok(d.dst_subpass, subpasses))
    {
        return Err(invalid(
            NAME,
            "a dependency on a subpass the pass does not have",
        ));
    }
    for link in &info.p_next {
        match link {
            N::VkRenderPassMultiviewCreateInfo(m) => {
                if (m.subpass_count != 0 && m.subpass_count != subpasses)
                    || (m.dependency_count != 0 && m.dependency_count != info.dependency_count)
                {
                    return Err(invalid(
                        NAME,
                        "multiview masks for another number of subpasses",
                    ));
                }
            }
            N::VkRenderPassInputAttachmentAspectCreateInfo(a) => {
                let subs = info.p_subpasses.as_deref().unwrap_or_default();
                for r in a.p_aspect_references.iter().flatten() {
                    let inputs = usize::try_from(r.subpass)
                        .ok()
                        .and_then(|i| subs.get(i))
                        .map(|s| s.input_attachment_count);
                    if inputs.is_none_or(|count| r.input_attachment_index >= count) {
                        return Err(invalid(NAME, "an input aspect reference outside the pass"));
                    }
                }
            }
            #[allow(unreachable_patterns)]
            _ => {}
        }
    }
    let clears = info
        .p_attachments
        .iter()
        .flatten()
        .enumerate()
        .filter(|(_, a)| a.load_op == LOAD_OP_CLEAR || a.stencil_load_op == LOAD_OP_CLEAR)
        .map(|(i, _)| u32::try_from(i + 1).unwrap_or(u32::MAX))
        .max()
        .unwrap_or(0);
    Ok(Facts::RenderPass {
        attachments: n,
        clears,
    })
}

/// [`render_pass_facts`] for `vkCreateRenderPass2`.
fn render_pass2_facts(
    args: &crate::venus::protocol::CreateRenderPass2Args,
    limits: &crate::venus::protocol::VkPhysicalDeviceLimits,
) -> Result<Facts, ExecError> {
    use crate::venus::protocol::VkSubpassDescription2Next as S;
    const NAME: &str = "vkCreateRenderPass2";
    let Some(info) = &args.p_create_info else {
        return Err(invalid(NAME, "pCreateInfo is null"));
    };
    let n = info.attachment_count;
    let subpasses = info.subpass_count;
    for s in info.p_subpasses.iter().flatten() {
        if s.color_attachment_count > limits.max_color_attachments {
            return Err(invalid(
                NAME,
                "more colour attachments than maxColorAttachments",
            ));
        }
        let resolve = s.p_next.iter().filter_map(|l| match l {
            S::VkSubpassDescriptionDepthStencilResolve(r) => {
                r.p_depth_stencil_resolve_attachment.as_ref()
            }
            #[allow(unreachable_patterns)]
            _ => None,
        });
        let refs = s
            .p_input_attachments
            .iter()
            .flatten()
            .chain(s.p_color_attachments.iter().flatten())
            .chain(s.p_resolve_attachments.iter().flatten())
            .chain(s.p_depth_stencil_attachment.iter())
            .chain(resolve);
        if refs.map(|r| r.attachment).any(|a| !attachment_ok(a, n))
            || s.p_preserve_attachments.iter().flatten().any(|a| *a >= n)
        {
            return Err(invalid(NAME, "an attachment reference outside the pass"));
        }
    }
    if info
        .p_dependencies
        .iter()
        .flatten()
        .any(|d| !subpass_ok(d.src_subpass, subpasses) || !subpass_ok(d.dst_subpass, subpasses))
    {
        return Err(invalid(
            NAME,
            "a dependency on a subpass the pass does not have",
        ));
    }
    let clears = info
        .p_attachments
        .iter()
        .flatten()
        .enumerate()
        .filter(|(_, a)| a.load_op == LOAD_OP_CLEAR || a.stencil_load_op == LOAD_OP_CLEAR)
        .map(|(i, _)| u32::try_from(i + 1).unwrap_or(u32::MAX))
        .max()
        .unwrap_or(0);
    Ok(Facts::RenderPass {
        attachments: n,
        clears,
    })
}

/// Answer every output handle of `command` with 0.
fn zero_outputs(command: &mut Command<'_>) {
    if let Some((_, slots)) = generated::output_handles(command) {
        for slot in slots {
            *slot = 0;
        }
    }
}
