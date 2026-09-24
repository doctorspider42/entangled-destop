//! The generated translation ([`super::generated`]) on its own, against a
//! resolver that knows a handful of ids: nested handles, arrays of handles,
//! handles in arrays of structures and in pNext links, every one replaced;
//! a wrong-type id deep inside a structure, a null where one is required, an
//! array that disagrees with its count, an enum or flag outside Vulkan 1.3 —
//! every one refused.

use std::collections::HashMap;

use super::context::ExecError;
use super::generated::{self, Resolve};
use super::objects::{IdError, Kind};
use crate::venus::protocol::*;

/// Ids `1..` of the kinds the test names, each answered with `0x1000 | id`.
struct Mock {
    known: HashMap<u64, Kind>,
    asked: Vec<&'static str>,
    /// The extensions "the device" enabled (stage 5c).
    extensions: Vec<&'static str>,
}

impl Mock {
    fn new(objects: &[(u64, Kind)]) -> Self {
        Self {
            known: objects.iter().copied().collect(),
            asked: Vec::new(),
            extensions: Vec::new(),
        }
    }
}

impl Resolve for Mock {
    fn handle(
        &mut self,
        kind: Kind,
        id: u64,
        may_be_null: bool,
        what: &'static str,
    ) -> Result<u64, ExecError> {
        self.asked.push(what);
        let fail = |error| ExecError::IdIn {
            command: "test",
            field: what,
            error,
        };
        if id == 0 {
            return if may_be_null {
                Ok(0)
            } else {
                Err(fail(IdError::Zero(kind.name())))
            };
        }
        match self.known.get(&id) {
            None => Err(fail(IdError::Unknown {
                id,
                expected: kind.name(),
            })),
            Some(found) if *found != kind => Err(fail(IdError::WrongType {
                id,
                expected: kind.name(),
                found: found.name(),
            })),
            Some(_) => Ok(0x1000 | id),
        }
    }

    fn invalid(&self, what: String) -> ExecError {
        ExecError::Invalid {
            command: "test",
            what,
        }
    }

    fn link(&self, parent: &'static str, stype: i32) -> ExecError {
        super::context::unimplemented_link("test", parent, stype)
    }

    fn enabled(&self, extension: &'static str) -> bool {
        self.extensions.contains(&extension)
    }
}

const CB: u64 = 1;
const RENDER_PASS: u64 = 2;
const FRAMEBUFFER: u64 = 3;
const SET_A: u64 = 4;
const SET_B: u64 = 5;
const LAYOUT: u64 = 6;
const BUFFER: u64 = 7;
const IMAGE: u64 = 8;
const VIEW: u64 = 9;
const DEVICE: u64 = 10;

fn objects() -> Mock {
    Mock::new(&[
        (CB, Kind::CommandBuffer),
        (RENDER_PASS, Kind::RenderPass),
        (FRAMEBUFFER, Kind::Framebuffer),
        (SET_A, Kind::DescriptorSet),
        (SET_B, Kind::DescriptorSet),
        (LAYOUT, Kind::PipelineLayout),
        (BUFFER, Kind::Buffer),
        (IMAGE, Kind::Image),
        (VIEW, Kind::ImageView),
        (DEVICE, Kind::Device),
    ])
}

fn begin_render_pass() -> Command<'static> {
    Command::CmdBeginRenderPass(CmdBeginRenderPassArgs {
        command_buffer: VkCommandBuffer(CB),
        p_render_pass_begin: Some(VkRenderPassBeginInfo {
            p_next: vec![VkRenderPassBeginInfoNext::VkRenderPassAttachmentBeginInfo(
                VkRenderPassAttachmentBeginInfo {
                    attachment_count: 1,
                    p_attachments: Some(vec![VkImageView(VIEW)]),
                },
            )],
            render_pass: VkRenderPass(RENDER_PASS),
            framebuffer: VkFramebuffer(FRAMEBUFFER),
            render_area: VkRect2D::default(),
            clear_value_count: 0,
            p_clear_values: None,
        }),
        contents: 0,
    })
}

fn write(buffer: u64) -> VkWriteDescriptorSet<'static> {
    VkWriteDescriptorSet {
        p_next: Vec::new(),
        dst_set: VkDescriptorSet(SET_A),
        dst_binding: 0,
        dst_array_element: 0,
        descriptor_count: 1,
        descriptor_type: 7,
        p_image_info: None,
        p_buffer_info: Some(vec![VkDescriptorBufferInfo {
            buffer: VkBuffer(buffer),
            offset: 0,
            range: u64::MAX,
        }]),
        p_texel_buffer_view: None,
    }
}

fn update(write: VkWriteDescriptorSet<'static>) -> Command<'static> {
    Command::UpdateDescriptorSets(UpdateDescriptorSetsArgs {
        device: VkDevice(DEVICE),
        descriptor_write_count: 1,
        p_descriptor_writes: Some(vec![write]),
        descriptor_copy_count: 1,
        p_descriptor_copies: Some(vec![VkCopyDescriptorSet {
            src_set: VkDescriptorSet(SET_B),
            src_binding: 0,
            src_array_element: 0,
            dst_set: VkDescriptorSet(SET_A),
            dst_binding: 0,
            dst_array_element: 0,
            descriptor_count: 1,
        }]),
    })
}

#[test]
fn nested_handles_and_handles_in_a_pnext_link_are_all_replaced() {
    let mut r = objects();
    let mut command = begin_render_pass();
    generated::translate(&mut r, &mut command).expect("every id is known");
    let Command::CmdBeginRenderPass(a) = command else {
        panic!()
    };
    assert_eq!(a.command_buffer.0, 0x1000 | CB);
    let begin = a.p_render_pass_begin.unwrap();
    assert_eq!(begin.render_pass.0, 0x1000 | RENDER_PASS);
    assert_eq!(begin.framebuffer.0, 0x1000 | FRAMEBUFFER);
    let VkRenderPassBeginInfoNext::VkRenderPassAttachmentBeginInfo(link) = &begin.p_next[0] else {
        panic!()
    };
    assert_eq!(
        link.p_attachments.as_deref(),
        Some(&[VkImageView(0x1000 | VIEW)][..])
    );
}

#[test]
fn arrays_of_handles_and_handles_in_arrays_of_structures_are_replaced() {
    let mut r = objects();
    let mut bind = Command::CmdBindDescriptorSets(CmdBindDescriptorSetsArgs {
        command_buffer: VkCommandBuffer(CB),
        pipeline_bind_point: 1,
        layout: VkPipelineLayout(LAYOUT),
        first_set: 0,
        descriptor_set_count: 2,
        p_descriptor_sets: Some(vec![VkDescriptorSet(SET_A), VkDescriptorSet(SET_B)]),
        dynamic_offset_count: 0,
        p_dynamic_offsets: None,
    });
    generated::translate(&mut r, &mut bind).unwrap();
    let Command::CmdBindDescriptorSets(a) = bind else {
        panic!()
    };
    assert_eq!(
        a.p_descriptor_sets.unwrap(),
        vec![
            VkDescriptorSet(0x1000 | SET_A),
            VkDescriptorSet(0x1000 | SET_B)
        ]
    );
    assert_eq!(a.layout.0, 0x1000 | LAYOUT);

    let mut u = update(write(BUFFER));
    generated::translate(&mut r, &mut u).unwrap();
    let Command::UpdateDescriptorSets(a) = u else {
        panic!()
    };
    let w = &a.p_descriptor_writes.as_ref().unwrap()[0];
    assert_eq!(w.dst_set.0, 0x1000 | SET_A);
    assert_eq!(
        w.p_buffer_info.as_ref().unwrap()[0].buffer.0,
        0x1000 | BUFFER
    );
    let c = &a.p_descriptor_copies.as_ref().unwrap()[0];
    assert_eq!((c.src_set.0, c.dst_set.0), (0x1000 | SET_B, 0x1000 | SET_A));
    // The device parameter is checked, and left as the guest's id.
    assert_eq!(a.device.0, 0x1000 | DEVICE);
}

#[test]
fn a_wrong_type_id_deep_inside_a_structure_is_refused_by_field() {
    let mut r = objects();
    let mut u = update(write(IMAGE));
    assert_eq!(
        generated::translate(&mut r, &mut u),
        Err(ExecError::IdIn {
            command: "test",
            field: "VkDescriptorBufferInfo.buffer",
            error: IdError::WrongType {
                id: IMAGE,
                expected: "VkBuffer",
                found: "VkImage",
            },
        })
    );
    // An id nobody created, one level down in a link.
    let mut command = begin_render_pass();
    if let Command::CmdBeginRenderPass(a) = &mut command {
        let begin = a.p_render_pass_begin.as_mut().unwrap();
        let VkRenderPassBeginInfoNext::VkRenderPassAttachmentBeginInfo(link) = &mut begin.p_next[0]
        else {
            panic!()
        };
        link.p_attachments = Some(vec![VkImageView(0x777)]);
    }
    assert!(matches!(
        generated::translate(&mut r, &mut command),
        Err(ExecError::IdIn {
            field: "VkRenderPassAttachmentBeginInfo.pAttachments",
            error: IdError::Unknown { id: 0x777, .. },
            ..
        })
    ));
}

#[test]
fn null_is_taken_where_vk_xml_allows_it_and_refused_where_the_executor_requires_it() {
    let mut r = objects();
    // A descriptor write's dstSet: optional in vk.xml (push descriptors),
    // required here.
    let mut w = write(BUFFER);
    w.dst_set = VkDescriptorSet(0);
    assert!(matches!(
        generated::translate(&mut r, &mut update(w)),
        Err(ExecError::IdIn {
            field: "VkWriteDescriptorSet.dstSet",
            error: IdError::Zero(_),
            ..
        })
    ));
    // A render pass begin's null render pass is not something vk.xml allows.
    let mut command = begin_render_pass();
    if let Command::CmdBeginRenderPass(a) = &mut command {
        a.p_render_pass_begin.as_mut().unwrap().render_pass = VkRenderPass(0);
    }
    assert!(generated::translate(&mut r, &mut command).is_err());
    // A graphics pipeline with no render pass (dynamic rendering) is fine.
    let mut pipeline = Command::CreateGraphicsPipelines(CreateGraphicsPipelinesArgs {
        device: VkDevice(DEVICE),
        pipeline_cache: VkPipelineCache(0),
        create_info_count: 1,
        p_create_infos: Some(vec![VkGraphicsPipelineCreateInfo {
            layout: VkPipelineLayout(LAYOUT),
            render_pass: VkRenderPass(0),
            ..Default::default()
        }]),
        p_pipelines: Some(vec![VkPipeline(0x55)]),
        ret: 0,
    });
    generated::translate(&mut r, &mut pipeline).expect("renderPass may be null");
    // The output handle is not an input, and is left for the executor.
    let Some((kind, ids)) = generated::output_handles(&mut pipeline) else {
        panic!()
    };
    assert_eq!(kind, Kind::Pipeline);
    assert_eq!(ids.into_iter().map(|h| *h).collect::<Vec<_>>(), vec![0x55]);
}

#[test]
fn an_array_that_disagrees_with_its_count_is_refused() {
    let mut r = objects();
    let mut bind = Command::CmdBindDescriptorSets(CmdBindDescriptorSetsArgs {
        command_buffer: VkCommandBuffer(CB),
        pipeline_bind_point: 1,
        layout: VkPipelineLayout(LAYOUT),
        first_set: 0,
        descriptor_set_count: 3,
        p_descriptor_sets: Some(vec![VkDescriptorSet(SET_A)]),
        dynamic_offset_count: 0,
        p_dynamic_offsets: None,
    });
    assert!(matches!(
        generated::translate(&mut r, &mut bind),
        Err(ExecError::Invalid { .. })
    ));
}

#[test]
fn enums_and_flags_outside_vulkan_1_3_are_refused() {
    let mut r = objects();
    let mut bind = Command::CmdBindPipeline(CmdBindPipelineArgs {
        command_buffer: VkCommandBuffer(CB),
        pipeline_bind_point: 1_000_165_000, // RAY_TRACING_KHR: an extension's
        pipeline: VkPipeline(0),
    });
    assert!(matches!(
        generated::translate(&mut r, &mut bind),
        Err(ExecError::Invalid { .. })
    ));
    let mut barrier = Command::CmdPipelineBarrier(CmdPipelineBarrierArgs {
        command_buffer: VkCommandBuffer(CB),
        src_stage_mask: 0x1000,
        dst_stage_mask: 0x4000,
        dependency_flags: 0,
        memory_barrier_count: 1,
        p_memory_barriers: Some(vec![VkMemoryBarrier {
            src_access_mask: 0x8000_0000, // no core bit
            dst_access_mask: 0,
        }]),
        buffer_memory_barrier_count: 0,
        p_buffer_memory_barriers: None,
        image_memory_barrier_count: 0,
        p_image_memory_barriers: None,
    });
    assert!(matches!(
        generated::translate(&mut r, &mut barrier),
        Err(ExecError::Invalid { .. })
    ));
    // Every core format is one, and an extension's is not.
    assert!(generated::e_VkFormat(37));
    assert!(generated::e_VkFormat(1_000_156_000)); // G8B8G8R8_422_UNORM, 1.1
    assert!(!generated::e_VkFormat(1_000_054_000)); // PVRTC, an extension's
}

#[test]
fn the_tables_know_each_commands_version_and_dispatchable() {
    let rendering = Command::CmdEndRendering(CmdEndRenderingArgs {
        command_buffer: VkCommandBuffer(CB),
    });
    assert_eq!(generated::min_api(&rendering), (1 << 22) | (3 << 12));
    assert_eq!(
        generated::dispatchable(&rendering),
        Some((Kind::CommandBuffer, CB))
    );
    assert!(generated::is_pass_through(&rendering));
    assert_eq!(generated::min_api(&begin_render_pass()), 1 << 22);
    assert!(
        !generated::is_pass_through(&begin_render_pass()),
        "bounded by hand"
    );
    assert!(generated::TRANSLATED.len() > 130);
}
