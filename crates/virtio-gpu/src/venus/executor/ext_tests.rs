//! Stage 5c against the fake host: the device extensions Zink needs, passed
//! through and bounded; the emulated dma-buf external memory — its queries,
//! export allocations and imports of our own pages, across contexts; the
//! identity the guest is shown. Every command as Mesa 26.0.8 sends it, then
//! every way a guest can abuse it.

use std::sync::Arc;

use crate::renderer::Renderer3d;
use crate::venus::protocol::*;

use super::fake::{self, FakeVulkan};
use super::harness::*;
use super::policy;
use super::recording::*;

/// The fake's memory types: 3 is our pages (`HOST_VISIBLE | HOST_COHERENT`),
/// 1 device-local.
const HOST_TYPE: u32 = 3;
const DEVICE_LOCAL_TYPE: u32 = 1;
const SIZE: u64 = 64 << 10;

/// Everything a Zink device enables that this renderer serves, as the
/// guest's venus sends it: the admitted extensions, the five Zink requires by
/// name, and the two dma-buf names venus adds for any device that wants a
/// swapchain or an fd (`vn_device.c:318-330`).
const ZINK_EXTENSIONS: &[&str] = &[
    "VK_KHR_maintenance1",
    "VK_KHR_maintenance2",
    "VK_KHR_create_renderpass2",
    "VK_KHR_imageless_framebuffer",
    "VK_KHR_dynamic_rendering",
    "VK_KHR_descriptor_update_template",
    "VK_KHR_draw_indirect_count",
    "VK_EXT_robustness2",
    "VK_EXT_transform_feedback",
    "VK_EXT_conditional_rendering",
    "VK_EXT_custom_border_color",
    "VK_EXT_border_color_swizzle",
    "VK_EXT_line_rasterization",
    "VK_EXT_provoking_vertex",
    "VK_EXT_depth_clip_enable",
    "VK_EXT_vertex_attribute_divisor",
    "VK_KHR_external_semaphore_fd",
    "VK_EXT_external_memory_dma_buf",
    "VK_KHR_external_memory_fd",
];

fn device_with(exts: &[&'static str], chain: Vec<VkDeviceCreateInfoNext>) -> Command<'static> {
    let Command::CreateDevice(mut args) = create_device(PHYSICAL, DEVICE, chain) else {
        unreachable!()
    };
    if let Some(info) = args.p_create_info.as_mut() {
        info.enabled_extension_count = exts.len() as u32;
        info.pp_enabled_extension_names = Some(exts.iter().map(|e| e.as_bytes()).collect());
    }
    Command::CreateDevice(args)
}

fn null_descriptor_features() -> VkDeviceCreateInfoNext {
    VkDeviceCreateInfoNext::VkPhysicalDeviceRobustness2FeaturesKHR(
        VkPhysicalDeviceRobustness2FeaturesKHR {
            robust_buffer_access2: 1,
            robust_image_access2: 1,
            null_descriptor: 1,
        },
    )
}

/// A Zink-shaped device on the Zink-shaped fake: `exts` enabled (and
/// `nullDescriptor` when robustness2 is among them), its pool, queue and a
/// command buffer.
fn zink_with(exts: &[&'static str]) -> (Harness<FakeVulkan>, Arc<FakeVulkan>) {
    let host = Arc::new(FakeVulkan::new(vec![fake::zink_gpu(
        "NVIDIA GeForce RTX 2070",
    )]));
    let mut h = Harness::new(Arc::clone(&host));
    boot(&mut h);
    let chain = if exts.contains(&"VK_EXT_robustness2") {
        vec![null_descriptor_features()]
    } else {
        Vec::new()
    };
    let Command::CreateDevice(d) = h.call(&device_with(exts, chain)).expect("device") else {
        panic!()
    };
    assert_eq!(d.ret, VK_SUCCESS);
    h.send(&create_pool(DEVICE, POOL)).expect("pool");
    h.call(&device_queue(DEVICE, QUEUE, 1)).expect("queue");
    h.send(&allocate_cbs(DEVICE, POOL, &[CB], false))
        .expect("command buffer");
    assert!(!h.fatal());
    (h, host)
}

fn zink() -> (Harness<FakeVulkan>, Arc<FakeVulkan>) {
    zink_with(ZINK_EXTENSIONS)
}

fn fatal_on(h: &mut Harness<FakeVulkan>, command: &Command<'_>, what: &str) {
    let head = h.call(command).expect_err(what);
    assert_eq!(
        head, h.last_start,
        "{what}: head stays in front of the command"
    );
    assert!(h.fatal(), "{what}");
}

/// A fresh Zink harness per refusal, the refusal fatal to it.
fn refused(setup: impl Fn(&mut Harness<FakeVulkan>), command: Command<'static>, what: &str) {
    let (mut h, _) = zink();
    setup(&mut h);
    fatal_on(&mut h, &command, what);
}

/// A bound buffer of `size` bytes and `usage`, in device-local memory.
fn bound_buffer(h: &mut Harness<FakeVulkan>, id: u64, size: u64, usage: u32) {
    let Command::CreateBuffer(b) = h
        .call(&create_buffer(DEVICE, id, buffer_info(size, usage)))
        .expect("buffer")
    else {
        panic!()
    };
    assert_eq!(b.ret, VK_SUCCESS);
    let memory = id | 0x1000;
    h.send(&allocate(
        DEVICE,
        memory,
        size.next_multiple_of(256),
        DEVICE_LOCAL_TYPE,
        Vec::new(),
    ))
    .expect("memory");
    h.send(&bind_buffers(DEVICE, &[(id, memory, 0)]))
        .expect("bind");
    assert!(!h.fatal());
}

fn extension_names(h: &mut Harness<FakeVulkan>) -> Vec<String> {
    let count =
        Command::EnumerateDeviceExtensionProperties(EnumerateDeviceExtensionPropertiesArgs {
            physical_device: VkPhysicalDevice(PHYSICAL),
            p_layer_name: None,
            p_property_count: Some(0),
            p_properties: None,
            ret: 0,
        });
    let Command::EnumerateDeviceExtensionProperties(c) = h.call(&count).unwrap() else {
        panic!()
    };
    let n = c.p_property_count.unwrap();
    let Command::EnumerateDeviceExtensionProperties(e) = h
        .call(&Command::EnumerateDeviceExtensionProperties(
            EnumerateDeviceExtensionPropertiesArgs {
                physical_device: VkPhysicalDevice(PHYSICAL),
                p_layer_name: None,
                p_property_count: Some(n),
                p_properties: Some(vec![VkExtensionProperties::default(); n as usize]),
                ret: 0,
            },
        ))
        .unwrap()
    else {
        panic!()
    };
    e.p_properties
        .unwrap()
        .iter()
        .map(|x| String::from_utf8_lossy(policy::c_name(&x.extension_name)).into_owned())
        .collect()
}

const TF_BUFFER: u32 = policy::BUFFER_USAGE_TRANSFORM_FEEDBACK;
const TF_COUNTER: u32 = policy::BUFFER_USAGE_TRANSFORM_FEEDBACK_COUNTER;
const COND: u32 = policy::BUFFER_USAGE_CONDITIONAL_RENDERING;
const TF_BUF: u64 = 0x300;
const COUNTER_BUF: u64 = 0x301;
const PREDICATE_BUF: u64 = 0x302;
const PLAIN_BUF: u64 = 0x303;
const QUERY_POOL: u64 = 0x310;

fn bind_tf(
    buffers: &[u64],
    offsets: &[u64],
    sizes: Option<&[u64]>,
    first: u32,
) -> Command<'static> {
    Command::CmdBindTransformFeedbackBuffersEXT(CmdBindTransformFeedbackBuffersEXTArgs {
        command_buffer: VkCommandBuffer(CB),
        first_binding: first,
        binding_count: buffers.len() as u32,
        p_buffers: Some(buffers.iter().copied().map(VkBuffer).collect()),
        p_offsets: Some(offsets.to_vec()),
        p_sizes: sizes.map(<[u64]>::to_vec),
    })
}

fn begin_tf(
    counters: Option<&[u64]>,
    offsets: Option<&[u64]>,
    first: u32,
    count: u32,
) -> Command<'static> {
    Command::CmdBeginTransformFeedbackEXT(CmdBeginTransformFeedbackEXTArgs {
        command_buffer: VkCommandBuffer(CB),
        first_counter_buffer: first,
        counter_buffer_count: count,
        p_counter_buffers: counters.map(|c| c.iter().copied().map(VkBuffer).collect()),
        p_counter_buffer_offsets: offsets.map(<[u64]>::to_vec),
    })
}

fn end_tf(counters: Option<&[u64]>, offsets: Option<&[u64]>) -> Command<'static> {
    let count = counters.map_or(0, |c| c.len() as u32);
    Command::CmdEndTransformFeedbackEXT(CmdEndTransformFeedbackEXTArgs {
        command_buffer: VkCommandBuffer(CB),
        first_counter_buffer: 0,
        counter_buffer_count: count,
        p_counter_buffers: counters.map(|c| c.iter().copied().map(VkBuffer).collect()),
        p_counter_buffer_offsets: offsets.map(<[u64]>::to_vec),
    })
}

fn query_pool(query_type: i32, count: u32) -> Command<'static> {
    Command::CreateQueryPool(CreateQueryPoolArgs {
        device: VkDevice(DEVICE),
        p_create_info: Some(VkQueryPoolCreateInfo {
            flags: 0,
            query_type,
            query_count: count,
            pipeline_statistics: 0,
        }),
        p_query_pool: Some(VkQueryPool(QUERY_POOL)),
        ret: 0,
    })
}

fn begin_query_indexed(query: u32, index: u32) -> Command<'static> {
    Command::CmdBeginQueryIndexedEXT(CmdBeginQueryIndexedEXTArgs {
        command_buffer: VkCommandBuffer(CB),
        query_pool: VkQueryPool(QUERY_POOL),
        query,
        flags: 0,
        index,
    })
}

fn draw_byte_count(counter: u64, offset: u64, stride: u32) -> Command<'static> {
    Command::CmdDrawIndirectByteCountEXT(CmdDrawIndirectByteCountEXTArgs {
        command_buffer: VkCommandBuffer(CB),
        instance_count: 1,
        first_instance: 0,
        counter_buffer: VkBuffer(counter),
        counter_buffer_offset: offset,
        counter_offset: 0,
        vertex_stride: stride,
    })
}

fn begin_conditional(buffer: u64, offset: u64) -> Command<'static> {
    Command::CmdBeginConditionalRenderingEXT(CmdBeginConditionalRenderingEXTArgs {
        command_buffer: VkCommandBuffer(CB),
        p_conditional_rendering_begin: Some(VkConditionalRenderingBeginInfoEXT {
            buffer: VkBuffer(buffer),
            offset,
            flags: 0,
        }),
    })
}

fn end_conditional() -> Command<'static> {
    Command::CmdEndConditionalRenderingEXT(CmdEndConditionalRenderingEXTArgs {
        command_buffer: VkCommandBuffer(CB),
    })
}

fn line_stipple(factor: u32) -> Command<'static> {
    Command::CmdSetLineStipple(CmdSetLineStippleArgs {
        command_buffer: VkCommandBuffer(CB),
        line_stipple_factor: factor,
        line_stipple_pattern: 0xf0f0,
    })
}

/// The transform feedback and conditional rendering buffers every command
/// test below binds.
fn buffers(h: &mut Harness<FakeVulkan>) {
    bound_buffer(h, TF_BUF, SIZE, TF_BUFFER);
    bound_buffer(h, COUNTER_BUF, 256, TF_COUNTER);
    bound_buffer(h, PREDICATE_BUF, 256, COND);
    bound_buffer(h, PLAIN_BUF, SIZE, 0x2);
}

// ------------------------------------------------- the device and its list

#[test]
fn a_zink_device_is_shown_the_extensions_and_creates_with_them_minus_the_emulated() {
    let (mut h, host) = zink();
    let names = extension_names(&mut h);
    for name in ZINK_EXTENSIONS {
        assert!(names.iter().any(|n| n == name), "{name} is advertised");
    }
    assert!(names.iter().all(|n| n != "VK_KHR_external_memory_win32"));
    assert!(names.iter().all(|n| n != "VK_KHR_swapchain"));
    let request = host
        .device_requests()
        .pop()
        .expect("the device reached the host");
    for name in ZINK_EXTENSIONS {
        let emulated = policy::is_emulated_extension(name);
        assert_eq!(
            request.extensions.iter().any(|e| e == name),
            !emulated,
            "{name}: the host enables it exactly when it is not emulated"
        );
    }
    assert!(request
        .extensions
        .iter()
        .any(|e| e == policy::EXTERNAL_MEMORY_HOST));
    assert!(
        request.chain.iter().any(|l| matches!(l,
            VkDeviceCreateInfoNext::VkPhysicalDeviceRobustness2FeaturesKHR(f) if f.null_descriptor == 1)),
        "nullDescriptor reaches the host"
    );
    assert_eq!(
        request.features.map(|f| f.robust_buffer_access),
        Some(1),
        "robustBufferAccess is on, as robustBufferAccess2 needs"
    );
    assert!(!h.fatal());
}

#[test]
fn the_extension_features_and_properties_are_answered_from_the_host() {
    let (mut h, _) = zink();
    let query = Command::GetPhysicalDeviceFeatures2(GetPhysicalDeviceFeatures2Args {
        physical_device: VkPhysicalDevice(PHYSICAL),
        p_features: Some(VkPhysicalDeviceFeatures2 {
            p_next: vec![
                VkPhysicalDeviceFeatures2Next::VkPhysicalDeviceRobustness2FeaturesKHR(
                    Default::default(),
                ),
                VkPhysicalDeviceFeatures2Next::VkPhysicalDeviceTransformFeedbackFeaturesEXT(
                    Default::default(),
                ),
            ],
            features: Default::default(),
        }),
    });
    let Command::GetPhysicalDeviceFeatures2(f) = h.call(&query).unwrap() else {
        panic!()
    };
    let f = f.p_features.unwrap();
    assert_eq!(
        f.features.robust_buffer_access, 1,
        "robustBufferAccess is reported"
    );
    assert!(f.p_next.iter().any(|l| matches!(l,
        VkPhysicalDeviceFeatures2Next::VkPhysicalDeviceRobustness2FeaturesKHR(r)
            if r.null_descriptor == 1 && r.robust_buffer_access2 == 1 && r.robust_image_access2 == 1)));
    assert!(f.p_next.iter().any(|l| matches!(l,
        VkPhysicalDeviceFeatures2Next::VkPhysicalDeviceTransformFeedbackFeaturesEXT(t)
            if t.transform_feedback == 1)));
    let query = Command::GetPhysicalDeviceProperties2(GetPhysicalDeviceProperties2Args {
        physical_device: VkPhysicalDevice(PHYSICAL),
        p_properties: Some(VkPhysicalDeviceProperties2 {
            p_next: vec![
                VkPhysicalDeviceProperties2Next::VkPhysicalDeviceTransformFeedbackPropertiesEXT(
                    Default::default(),
                ),
            ],
            properties: Default::default(),
        }),
    });
    let Command::GetPhysicalDeviceProperties2(p) = h.call(&query).unwrap() else {
        panic!()
    };
    let p = p.p_properties.unwrap();
    assert_eq!(p.properties.vendor_id, policy::VIRTIO_PCI_VENDOR_ID);
    assert!(p.p_next.iter().any(|l| matches!(l,
        VkPhysicalDeviceProperties2Next::VkPhysicalDeviceTransformFeedbackPropertiesEXT(t)
            if t.max_transform_feedback_buffers == 4)));
}

#[test]
fn a_non_nvidia_host_is_not_shown_the_dma_buf_pair() {
    let mut device = fake::zink_gpu("AMD Radeon");
    device.info.properties.properties.vendor_id = 0x1002;
    for link in &mut device.info.properties.p_next {
        if let VkPhysicalDeviceProperties2Next::VkPhysicalDeviceVulkan12Properties(p) = link {
            p.driver_id = 1; // AMD proprietary
        }
    }
    let host = Arc::new(FakeVulkan::new(vec![device]));
    let mut h = Harness::new(Arc::clone(&host));
    boot(&mut h);
    let names = extension_names(&mut h);
    assert!(names.iter().all(|n| n != policy::EXTERNAL_MEMORY_DMA_BUF));
    assert!(names.iter().all(|n| n != policy::EXTERNAL_MEMORY_FD));
    assert!(names.iter().any(|n| n == policy::EXTERNAL_SEMAPHORE_FD));
    assert!(names.iter().any(|n| n == "VK_EXT_transform_feedback"));
    let Command::GetPhysicalDeviceProperties(p) = h.call(&properties(PHYSICAL)).unwrap() else {
        panic!()
    };
    assert_eq!(
        p.p_properties.unwrap().vendor_id,
        0x1002,
        "only NVIDIA's id is changed"
    );
}

// ------------------------------------------------ extension commands, gated

#[test]
fn an_extension_command_needs_its_extension_enabled_on_the_device() {
    // A device that enabled nothing: every admitted extension's command,
    // value and structure is refused on it.
    let (mut h, _) = zink_with(&[]);
    bound_buffer(&mut h, PREDICATE_BUF, 256, 0x2);
    for (command, what) in [
        (
            begin_conditional(PREDICATE_BUF, 0),
            "vkCmdBeginConditionalRenderingEXT",
        ),
        (end_conditional(), "vkCmdEndConditionalRenderingEXT"),
        (line_stipple(1), "vkCmdSetLineStipple"),
        (
            bind_tf(&[PREDICATE_BUF], &[0], None, 0),
            "vkCmdBindTransformFeedbackBuffersEXT",
        ),
    ] {
        let (mut fresh, _) = zink_with(&[]);
        bound_buffer(&mut fresh, PREDICATE_BUF, 256, 0x2);
        fatal_on(&mut fresh, &command, what);
    }
    // Its buffer usage bits, query type and chained structures too.
    fatal_on(
        &mut h,
        &create_buffer(DEVICE, 0x400, buffer_info(256, TF_BUFFER)),
        "a transform feedback buffer on a device without the extension",
    );
    let (mut h, _) = zink_with(&[]);
    fatal_on(
        &mut h,
        &query_pool(policy::QUERY_TYPE_TRANSFORM_FEEDBACK_STREAM, 4),
        "a transform feedback query pool on a device without the extension",
    );
    let (mut h, _) = zink_with(&[]);
    let Command::CreateGraphicsPipelines(mut p) =
        create_triangle_pipeline(DEVICE, PIPELINE, SHADER, PIPELINE_LAYOUT, RENDER_PASS, 64)
    else {
        panic!()
    };
    p.p_create_infos.as_mut().unwrap()[0]
        .p_rasterization_state
        .as_mut()
        .unwrap()
        .p_next
        .push(
            VkPipelineRasterizationStateCreateInfoNext::VkPipelineRasterizationDepthClipStateCreateInfoEXT(
                VkPipelineRasterizationDepthClipStateCreateInfoEXT {
                    flags: 0,
                    depth_clip_enable: 1,
                },
            ),
        );
    h.send(&create_shader_module(DEVICE, SHADER, &[0x0723_0203; 8]))
        .unwrap();
    h.send(&create_pipeline_layout(DEVICE, PIPELINE_LAYOUT, &[]))
        .unwrap();
    h.send(&create_render_pass(DEVICE, RENDER_PASS, RGBA8))
        .unwrap();
    fatal_on(
        &mut h,
        &Command::CreateGraphicsPipelines(p),
        "a depth-clip structure on a device without VK_EXT_depth_clip_enable",
    );
}

#[test]
fn extension_commands_reach_the_host_on_a_device_that_enabled_them() {
    let (mut h, host) = zink();
    buffers(&mut h);
    h.send(&query_pool(policy::QUERY_TYPE_TRANSFORM_FEEDBACK_STREAM, 4))
        .unwrap();
    for command in [
        bind_tf(&[TF_BUF], &[0], Some(&[SIZE]), 0),
        bind_tf(&[TF_BUF], &[256], Some(&[u64::MAX]), 3),
        begin_tf(Some(&[COUNTER_BUF]), Some(&[4]), 0, 1),
        begin_tf(None, None, 0, 0),
        begin_tf(Some(&[0]), None, 1, 1),
        begin_query_indexed(3, 3),
        Command::CmdEndQueryIndexedEXT(CmdEndQueryIndexedEXTArgs {
            command_buffer: VkCommandBuffer(CB),
            query_pool: VkQueryPool(QUERY_POOL),
            query: 3,
            index: 3,
        }),
        end_tf(Some(&[COUNTER_BUF]), Some(&[4])),
        draw_byte_count(COUNTER_BUF, 4, 16),
        begin_conditional(PREDICATE_BUF, 252),
        end_conditional(),
        line_stipple(1),
        line_stipple(256),
    ] {
        h.send(&command).expect("served");
    }
    assert!(!h.fatal());
    for name in [
        "vkCmdBindTransformFeedbackBuffersEXT",
        "vkCmdBeginTransformFeedbackEXT",
        "vkCmdEndTransformFeedbackEXT",
        "vkCmdBeginQueryIndexedEXT",
        "vkCmdEndQueryIndexedEXT",
        "vkCmdDrawIndirectByteCountEXT",
        "vkCmdBeginConditionalRenderingEXT",
        "vkCmdEndConditionalRenderingEXT",
        "vkCmdSetLineStipple",
    ] {
        assert!(host.called(name) >= 1, "{name} reached the host");
    }
}

#[test]
fn transform_feedback_bindings_are_bounded_by_the_buffers_and_the_limits() {
    for (command, what) in [
        (
            bind_tf(&[TF_BUF], &[0], None, 4),
            "a binding past maxTransformFeedbackBuffers",
        ),
        (
            bind_tf(&[TF_BUF; 2], &[0, 0], None, 3),
            "bindings running past the limit",
        ),
        (
            bind_tf(&[PLAIN_BUF], &[0], None, 0),
            "a buffer without TRANSFORM_FEEDBACK usage",
        ),
        (
            bind_tf(&[TF_BUF], &[2], None, 0),
            "an offset that is not 4-aligned",
        ),
        (
            bind_tf(&[TF_BUF], &[SIZE], None, 0),
            "an offset at the end of the buffer",
        ),
        (
            bind_tf(&[TF_BUF], &[4], Some(&[SIZE]), 0),
            "a range past the buffer",
        ),
        (bind_tf(&[0], &[0], None, 0), "a null buffer"),
        (bind_tf(&[0xdead], &[0], None, 0), "an unknown buffer"),
    ] {
        refused(buffers, command, what);
    }
}

#[test]
fn counter_buffers_are_four_aligned_bytes_inside_a_counter_buffer() {
    for (command, what) in [
        (
            begin_tf(Some(&[COUNTER_BUF]), Some(&[256]), 0, 1),
            "a counter at the end",
        ),
        (
            begin_tf(Some(&[COUNTER_BUF]), Some(&[253]), 0, 1),
            "an unaligned counter",
        ),
        (
            begin_tf(Some(&[COUNTER_BUF]), Some(&[u64::MAX - 1]), 0, 1),
            "an overflowing counter",
        ),
        (
            begin_tf(Some(&[TF_BUF]), Some(&[0]), 0, 1),
            "a buffer without COUNTER usage",
        ),
        (
            begin_tf(None, None, 4, 1),
            "counters past maxTransformFeedbackBuffers",
        ),
        (
            end_tf(Some(&[COUNTER_BUF]), Some(&[256])),
            "an end with a counter at the end",
        ),
    ] {
        refused(buffers, command, what);
    }
}

#[test]
fn indexed_queries_name_a_query_of_the_pool_and_a_stream_the_device_has() {
    let tf_pool = |h: &mut Harness<FakeVulkan>| {
        h.send(&query_pool(policy::QUERY_TYPE_TRANSFORM_FEEDBACK_STREAM, 4))
            .unwrap();
    };
    refused(tf_pool, begin_query_indexed(4, 0), "a query past the pool");
    refused(
        tf_pool,
        begin_query_indexed(0, 4),
        "a stream past maxTransformFeedbackStreams",
    );
    let occlusion = |h: &mut Harness<FakeVulkan>| {
        h.send(&query_pool(0, 4)).unwrap();
    };
    refused(
        occlusion,
        begin_query_indexed(0, 1),
        "an index on a pool with no streams",
    );
    // A transform feedback stream query answers two values a query.
    let (mut h, _) = zink();
    tf_pool(&mut h);
    let results = Command::GetQueryPoolResults(GetQueryPoolResultsArgs {
        device: VkDevice(DEVICE),
        query_pool: VkQueryPool(QUERY_POOL),
        first_query: 0,
        query_count: 1,
        data_size: 4,
        p_data: Some(vec![0; 4]),
        stride: 8,
        flags: 0,
        ret: 0,
    });
    fatal_on(
        &mut h,
        &results,
        "4 bytes are one value short of a stream query's two",
    );
}

#[test]
fn byte_count_draws_conditional_rendering_and_stipples_are_bounded() {
    for (command, what) in [
        (
            draw_byte_count(COUNTER_BUF, 256, 16),
            "a byte counter at the end",
        ),
        (
            draw_byte_count(COUNTER_BUF, 2, 16),
            "an unaligned byte counter",
        ),
        (draw_byte_count(COUNTER_BUF, 0, 0), "a vertex stride of 0"),
        (
            draw_byte_count(COUNTER_BUF, 0, 4096),
            "a stride past the limit",
        ),
        (
            begin_conditional(PREDICATE_BUF, 256),
            "a predicate at the end",
        ),
        (
            begin_conditional(PREDICATE_BUF, 253),
            "an unaligned predicate",
        ),
        (
            begin_conditional(PLAIN_BUF, 0),
            "a buffer without CONDITIONAL_RENDERING usage",
        ),
        (begin_conditional(0, 0), "a null predicate buffer"),
        (line_stipple(0), "a stipple factor of 0"),
        (line_stipple(257), "a stipple factor past 256"),
    ] {
        refused(buffers, command, what);
    }
}

// ------------------------------------------- robustness2 and friends

#[test]
fn null_descriptors_and_vertex_buffers_need_null_descriptor() {
    let null_write = |h: &mut Harness<FakeVulkan>| -> Command<'static> {
        h.send(&create_storage_set_layout(DEVICE, SET_LAYOUT, 0x20))
            .unwrap();
        h.send(&create_descriptor_pool(DEVICE, DESC_POOL, 0))
            .unwrap();
        h.call(&allocate_sets(DEVICE, DESC_POOL, SET_LAYOUT, &[SET]))
            .unwrap();
        write_storage(DEVICE, SET, 0)
    };
    let null_vertex = Command::CmdBindVertexBuffers(CmdBindVertexBuffersArgs {
        command_buffer: VkCommandBuffer(CB),
        first_binding: 0,
        binding_count: 1,
        p_buffers: Some(vec![VkBuffer(0)]),
        p_offsets: Some(vec![0]),
    });
    // With nullDescriptor: both are the guest's to send.
    let (mut h, host) = zink();
    let write = null_write(&mut h);
    h.send(&write).expect("a null buffer descriptor");
    h.send(&null_vertex).expect("a null vertex buffer");
    assert!(!h.fatal());
    assert_eq!(host.called("vkUpdateDescriptorSets"), 1);
    // A null one with an offset is still wrong.
    let (mut h, _) = zink();
    let Command::UpdateDescriptorSets(mut w) = null_write(&mut h) else {
        panic!()
    };
    w.p_descriptor_writes.as_mut().unwrap()[0]
        .p_buffer_info
        .as_mut()
        .unwrap()[0]
        .offset = 16;
    fatal_on(
        &mut h,
        &Command::UpdateDescriptorSets(w),
        "a null buffer with an offset",
    );
    // Without robustness2: both are refused.
    let (mut h, _) = zink_with(&["VK_EXT_transform_feedback"]);
    let write = null_write(&mut h);
    fatal_on(
        &mut h,
        &write,
        "a null buffer descriptor without nullDescriptor",
    );
    let (mut h, _) = zink_with(&["VK_EXT_transform_feedback"]);
    fatal_on(
        &mut h,
        &null_vertex,
        "a null vertex buffer without nullDescriptor",
    );
}

#[test]
fn custom_border_colour_samplers_stop_at_the_device_limit_and_come_back() {
    let sampler = |id: u64, border: i32| {
        Command::CreateSampler(CreateSamplerArgs {
            device: VkDevice(DEVICE),
            p_create_info: Some(VkSamplerCreateInfo {
                p_next: vec![
                    VkSamplerCreateInfoNext::VkSamplerCustomBorderColorCreateInfoEXT(
                        VkSamplerCustomBorderColorCreateInfoEXT {
                            custom_border_color: VkClearColorValue::Float32([1.0, 0.0, 0.0, 1.0]),
                            format: RGBA8,
                        },
                    ),
                ],
                border_color: border,
                max_lod: 1.0,
                ..Default::default()
            }),
            p_sampler: Some(VkSampler(id)),
            ret: 0,
        })
    };
    let destroy = |id: u64| {
        Command::DestroySampler(DestroySamplerArgs {
            device: VkDevice(DEVICE),
            sampler: VkSampler(id),
        })
    };
    let (mut h, host) = zink();
    // The fake allows two.
    h.send(&sampler(0x500, 1_000_287_003)).unwrap();
    h.send(&sampler(0x501, 1_000_287_004)).unwrap();
    assert!(!h.fatal());
    h.send(&destroy(0x500)).unwrap();
    h.send(&sampler(0x502, 1_000_287_003))
        .expect("the destroyed one's entry is back");
    assert_eq!(host.live("VkSampler"), 2);
    fatal_on(
        &mut h,
        &sampler(0x503, 1_000_287_003),
        "a third custom border colour sampler",
    );
    // Without the extension the structure itself is refused.
    let (mut h, _) = zink_with(&["VK_EXT_transform_feedback"]);
    fatal_on(
        &mut h,
        &sampler(0x500, 0),
        "a custom border colour without the extension",
    );
}

#[test]
fn pipeline_state_of_the_extensions_is_bounded_by_the_device() {
    let pipeline = |link: VkPipelineRasterizationStateCreateInfoNext| {
        let Command::CreateGraphicsPipelines(mut p) =
            create_triangle_pipeline(DEVICE, PIPELINE, SHADER, PIPELINE_LAYOUT, RENDER_PASS, 64)
        else {
            panic!()
        };
        p.p_create_infos.as_mut().unwrap()[0]
            .p_rasterization_state
            .as_mut()
            .unwrap()
            .p_next
            .push(link);
        Command::CreateGraphicsPipelines(p)
    };
    let objects = |h: &mut Harness<FakeVulkan>| {
        h.send(&create_shader_module(DEVICE, SHADER, &[0x0723_0203; 8]))
            .unwrap();
        h.send(&create_pipeline_layout(DEVICE, PIPELINE_LAYOUT, &[]))
            .unwrap();
        h.send(&create_render_pass(DEVICE, RENDER_PASS, RGBA8))
            .unwrap();
    };
    let stream = |n| {
        VkPipelineRasterizationStateCreateInfoNext::VkPipelineRasterizationStateStreamCreateInfoEXT(
            VkPipelineRasterizationStateStreamCreateInfoEXT {
                flags: 0,
                rasterization_stream: n,
            },
        )
    };
    let line = |factor| {
        VkPipelineRasterizationStateCreateInfoNext::VkPipelineRasterizationLineStateCreateInfo(
            VkPipelineRasterizationLineStateCreateInfo {
                line_rasterization_mode: 2,
                stippled_line_enable: 1,
                line_stipple_factor: factor,
                line_stipple_pattern: 0xff,
            },
        )
    };
    let (mut h, host) = zink();
    objects(&mut h);
    h.send(&pipeline(stream(3))).expect("stream 3 of 4");
    let (mut fresh, _) = zink();
    objects(&mut fresh);
    fresh
        .send(&pipeline(line(4)))
        .expect("a stipple factor of 4");
    assert!(!h.fatal() && !fresh.fatal());
    assert_eq!(host.called("vkCreateGraphicsPipelines"), 1);
    refused(
        objects,
        pipeline(stream(4)),
        "a rasterization stream past the limit",
    );
    refused(
        objects,
        pipeline(line(0)),
        "a stippled line with a factor of 0",
    );
    let bad_mode =
        VkPipelineRasterizationStateCreateInfoNext::VkPipelineRasterizationLineStateCreateInfo(
            VkPipelineRasterizationLineStateCreateInfo {
                line_rasterization_mode: 7,
                stippled_line_enable: 0,
                line_stipple_factor: 1,
                line_stipple_pattern: 0,
            },
        );
    refused(
        objects,
        pipeline(bad_mode),
        "a line rasterization mode that does not exist",
    );
}

// ------------------------------------------- external memory, emulated

fn buffer_query(handle: u32, usage: u32) -> Command<'static> {
    Command::GetPhysicalDeviceExternalBufferProperties(
        GetPhysicalDeviceExternalBufferPropertiesArgs {
            physical_device: VkPhysicalDevice(PHYSICAL),
            p_external_buffer_info: Some(VkPhysicalDeviceExternalBufferInfo {
                p_next: Vec::new(),
                flags: 0,
                usage,
                handle_type: handle as i32,
            }),
            p_external_buffer_properties: Some(Default::default()),
        },
    )
}

fn answer(h: &mut Harness<FakeVulkan>, handle: u32, usage: u32) -> VkExternalMemoryProperties {
    let Command::GetPhysicalDeviceExternalBufferProperties(q) =
        h.call(&buffer_query(handle, usage)).unwrap()
    else {
        panic!()
    };
    q.p_external_buffer_properties
        .unwrap()
        .external_memory_properties
}

#[test]
fn dma_buf_buffer_queries_answer_for_our_pages_and_nothing_else() {
    let (mut h, host) = zink();
    let dma_buf = policy::MEMORY_HANDLE_DMA_BUF;
    assert_eq!(
        answer(&mut h, dma_buf, 0x3),
        policy::external_memory_properties(true)
    );
    // Stage S1: a buffer our pages cannot hold is still shareable where the
    // host exports device-local memory for it — and nothing where neither.
    host.buffer_imports
        .store(false, std::sync::atomic::Ordering::SeqCst);
    assert_eq!(
        answer(&mut h, dma_buf, 0x3),
        policy::external_memory_properties(true)
    );
    host.buffer_exports
        .store(false, std::sync::atomic::Ordering::SeqCst);
    assert_eq!(
        answer(&mut h, dma_buf, 0x3),
        policy::external_memory_properties(false)
    );
    // Every other handle type: nothing, whatever the host would say.
    let opaque_win32 = 0x2;
    let a = answer(&mut h, opaque_win32, 0x3);
    assert_eq!(
        (a.external_memory_features, a.compatible_handle_types),
        (0, opaque_win32)
    );
    assert!(!h.fatal());
    for (handle, usage, what) in [
        (0x3, 0x3, "two handle bits"),
        (0x80, 0x3, "HOST_ALLOCATION, the renderer's own"),
        (dma_buf, 0, "no usage"),
        (
            dma_buf,
            0x0040_0000,
            "a usage bit no extension here defines",
        ),
    ] {
        let (mut fresh, _) = zink();
        fatal_on(&mut fresh, &buffer_query(handle, usage), what);
    }
}

#[test]
fn dma_buf_image_queries_are_our_pages_or_not_supported() {
    let (mut h, _) = zink();
    let query = |tiling: i32| {
        Command::GetPhysicalDeviceImageFormatProperties2(
            GetPhysicalDeviceImageFormatProperties2Args {
                physical_device: VkPhysicalDevice(PHYSICAL),
                p_image_format_info: Some(VkPhysicalDeviceImageFormatInfo2 {
                    p_next: vec![
                        VkPhysicalDeviceImageFormatInfo2Next::VkPhysicalDeviceExternalImageFormatInfo(
                            VkPhysicalDeviceExternalImageFormatInfo {
                                handle_type: policy::MEMORY_HANDLE_DMA_BUF as i32,
                            },
                        ),
                    ],
                    format: RGBA8,
                    type_: 1,
                    tiling,
                    usage: 0x1,
                    flags: 0,
                }),
                p_image_format_properties: Some(VkImageFormatProperties2 {
                    p_next: vec![
                        VkImageFormatProperties2Next::VkExternalImageFormatProperties(
                            Default::default(),
                        ),
                    ],
                    image_format_properties: Default::default(),
                }),
                ret: 0,
            },
        )
    };
    // The fake imports host memory for linear images only.
    let Command::GetPhysicalDeviceImageFormatProperties2(linear) = h.call(&query(1)).unwrap()
    else {
        panic!()
    };
    assert_eq!(linear.ret, VK_SUCCESS);
    let props = linear.p_image_format_properties.unwrap();
    let VkImageFormatProperties2Next::VkExternalImageFormatProperties(e) = &props.p_next[0] else {
        panic!()
    };
    assert_eq!(
        e.external_memory_properties,
        policy::external_memory_properties(true)
    );
    let Command::GetPhysicalDeviceImageFormatProperties2(optimal) = h.call(&query(0)).unwrap()
    else {
        panic!()
    };
    assert_eq!(optimal.ret, VK_ERROR_FORMAT_NOT_SUPPORTED);
    assert!(!h.fatal());
}

/// `VkExternalMemoryBufferCreateInfo{DMA_BUF}`, as Zink sends it for a
/// shared buffer and venus rewrites it.
fn external_buffer(size: u64, usage: u32) -> VkBufferCreateInfo {
    VkBufferCreateInfo {
        p_next: vec![VkBufferCreateInfoNext::VkExternalMemoryBufferCreateInfo(
            VkExternalMemoryBufferCreateInfo {
                handle_types: policy::MEMORY_HANDLE_DMA_BUF,
            },
        )],
        ..buffer_info(size, usage)
    }
}

fn export_info() -> VkMemoryAllocateInfoNext {
    VkMemoryAllocateInfoNext::VkExportMemoryAllocateInfo(VkExportMemoryAllocateInfo {
        handle_types: policy::MEMORY_HANDLE_DMA_BUF,
    })
}

fn import_info(resource_id: u32) -> VkMemoryAllocateInfoNext {
    VkMemoryAllocateInfoNext::VkImportMemoryResourceInfoMESA(VkImportMemoryResourceInfoMESA {
        resource_id,
    })
}

#[test]
fn a_dma_buf_buffer_asks_for_our_pages_alone() {
    let (mut h, host) = zink();
    let Command::CreateBuffer(b) = h
        .call(&create_buffer(DEVICE, 0x600, external_buffer(SIZE, 0x3)))
        .unwrap()
    else {
        panic!()
    };
    assert_eq!(b.ret, VK_SUCCESS);
    let Command::GetBufferMemoryRequirements2(r) =
        h.call(&buffer_requirements(DEVICE, 0x600)).unwrap()
    else {
        panic!()
    };
    let bits = r
        .p_memory_requirements
        .unwrap()
        .memory_requirements
        .memory_type_bits;
    assert_eq!(bits, 0x18, "types 3 and 4, our pages, and nothing else");
    assert_eq!(
        host.host_memory_resources().last(),
        Some(&("buffer", true)),
        "created for host allocations on the host"
    );
    // On a device that did not enable dma-buf, the handle type is refused.
    let (mut h, _) = zink_with(&["VK_EXT_transform_feedback"]);
    fatal_on(
        &mut h,
        &create_buffer(DEVICE, 0x600, external_buffer(SIZE, 0x3)),
        "DMA_BUF on a device that did not enable it",
    );
    let (mut h, _) = zink();
    let Command::CreateBuffer(mut other) = create_buffer(DEVICE, 0x600, external_buffer(SIZE, 0x3))
    else {
        panic!()
    };
    other.p_create_info.as_mut().unwrap().p_next =
        vec![VkBufferCreateInfoNext::VkExternalMemoryBufferCreateInfo(
            VkExternalMemoryBufferCreateInfo { handle_types: 0x2 },
        )];
    fatal_on(
        &mut h,
        &Command::CreateBuffer(other),
        "a handle type that is not the renderer's",
    );
}

const EXPORTED: u64 = 0x700;
const EXPORTED_RES: u32 = 70;
const MAP_AT: u64 = 0x80_0000;

/// An export allocation of our pages, and the blob Mesa makes of it at
/// once (`vn_device_memory_alloc_export`), mapped.
fn export(h: &mut Harness<FakeVulkan>) -> Arc<crate::venus::shmem::RingPages> {
    h.send(&allocate(
        DEVICE,
        EXPORTED,
        SIZE,
        HOST_TYPE,
        vec![export_info()],
    ))
    .expect("an export allocation");
    h.memory_blob(CTX, EXPORTED_RES, EXPORTED, SIZE)
        .expect("its blob");
    h.renderer
        .map_blob(EXPORTED_RES, MAP_AT, SIZE)
        .expect("mapped");
    h.renderer.blob_pages(EXPORTED_RES).expect("its pages")
}

fn resource_properties(resource_id: u32) -> Command<'static> {
    Command::GetMemoryResourcePropertiesMESA(GetMemoryResourcePropertiesMESAArgs {
        device: VkDevice(DEVICE),
        resource_id,
        p_memory_resource_properties: Some(VkMemoryResourcePropertiesMESA {
            p_next: vec![
                VkMemoryResourcePropertiesMESANext::VkMemoryResourceAllocationSizePropertiesMESA(
                    VkMemoryResourceAllocationSizePropertiesMESA { allocation_size: 0 },
                ),
            ],
            memory_type_bits: 0,
        }),
        ret: 0,
    })
}

fn properties_of(h: &mut Harness<FakeVulkan>, resource_id: u32) -> (i32, u32, u64) {
    let Command::GetMemoryResourcePropertiesMESA(p) =
        h.call(&resource_properties(resource_id)).unwrap()
    else {
        panic!()
    };
    let props = p.p_memory_resource_properties.unwrap();
    let VkMemoryResourcePropertiesMESANext::VkMemoryResourceAllocationSizePropertiesMESA(size) =
        &props.p_next[0];
    (p.ret, props.memory_type_bits, size.allocation_size)
}

#[test]
fn an_export_of_our_pages_is_a_page_blob_and_one_of_device_local_memory_a_handle_blob() {
    let (mut h, host) = zink();
    let pages = export(&mut h);
    assert_eq!(pages.mapped_len(), SIZE);
    assert!(!h.fatal());
    // Device-local: stage 5c refused its blob (no pages to share); on a
    // host that exports device-local memory (this one has
    // VK_KHR_external_memory_win32) it is a handle blob since stage S1 —
    // made, and never mapped. The refusal on a host that cannot export is
    // `s1_tests::a_device_local_export_without_a_handle_capable_host_is_refused_as_before`.
    h.send(&allocate(
        DEVICE,
        0x701,
        SIZE,
        DEVICE_LOCAL_TYPE,
        vec![export_info()],
    ))
    .expect("the allocation itself");
    h.memory_blob(CTX, 71, 0x701, SIZE)
        .expect("a handle blob of device-local memory");
    assert!(h.renderer.map_blob(71, MAP_AT + SIZE, SIZE).is_err());
    assert_eq!(host.live_shared_handles(), 1);
    h.renderer.destroy_blob(71);
    assert_eq!(host.live_shared_handles(), 0);
    h.send(&free(DEVICE, 0x701)).expect("the guest frees it");
    assert!(!h.fatal());
    // Another handle type is VK_ERROR_INVALID_EXTERNAL_HANDLE: the memory is
    // absent, as a refused asynchronous allocation is.
    let before = host.live("memory");
    h.send(&allocate(
        DEVICE,
        0x702,
        SIZE,
        HOST_TYPE,
        vec![VkMemoryAllocateInfoNext::VkExportMemoryAllocateInfo(
            VkExportMemoryAllocateInfo { handle_types: 0x2 },
        )],
    ))
    .unwrap();
    assert_eq!(host.live("memory"), before);
}

#[test]
fn an_import_of_our_own_blob_is_the_same_pages() {
    let (mut h, host) = zink();
    let pages = export(&mut h);
    let (ret, bits, size) = properties_of(&mut h, EXPORTED_RES);
    assert_eq!((ret, bits, size), (VK_SUCCESS, 0x18, SIZE));
    h.send(&allocate(
        DEVICE,
        0x710,
        SIZE,
        HOST_TYPE,
        vec![import_info(EXPORTED_RES)],
    ))
    .expect("the import");
    assert!(!h.fatal());
    let imports = host.imports();
    let last = imports.last().expect("an import reached the host");
    assert_eq!(
        last.addr,
        pages.host_addr(),
        "the same pages, imported again"
    );
    // Its pages are what the exporter's blob shows the guest.
    pages.write_bytes(0, &[0x5a; 16]).unwrap();
    let again = last.pages.upgrade().expect("alive");
    let mut back = [0u8; 16];
    again.read_bytes(0, &mut back).unwrap();
    assert_eq!(back, [0x5a; 16]);
    drop(again);
    // No blob can be made of the import.
    assert!(h.memory_blob(CTX, 72, 0x710, SIZE).is_err());
    // It binds like any memory of that type.
    let Command::CreateBuffer(_) = h
        .call(&create_buffer(DEVICE, 0x711, external_buffer(SIZE, 0x3)))
        .unwrap()
    else {
        panic!()
    };
    h.send(&bind_buffers(DEVICE, &[(0x711, 0x710, 0)])).unwrap();
    assert!(!h.fatal());
}

#[test]
fn imports_of_anything_but_our_memory_are_refused_in_vulkan_terms() {
    let (mut h, host) = zink();
    let _pages = export(&mut h);
    let before = host.live("memory");
    for (resource, ty, size, what) in [
        (0xbeef, HOST_TYPE, SIZE, "an unknown resource"),
        (REPLY_RES, HOST_TYPE, SIZE, "a reply blob"),
        (RING_RES, HOST_TYPE, SIZE, "a ring blob"),
        (
            EXPORTED_RES,
            DEVICE_LOCAL_TYPE,
            SIZE,
            "as a type that is not our pages",
        ),
        (EXPORTED_RES, HOST_TYPE, SIZE * 2, "larger than the blob"),
    ] {
        h.send(&allocate(
            DEVICE,
            0x720,
            size,
            ty,
            vec![import_info(resource)],
        ))
        .unwrap();
        assert_eq!(host.live("memory"), before, "{what}: nothing was made");
        assert!(!h.fatal(), "{what}");
    }
    for resource in [0xbeef, REPLY_RES, RING_RES] {
        assert_eq!(
            properties_of(&mut h, resource).0,
            VK_ERROR_INVALID_EXTERNAL_HANDLE,
            "resource {resource}"
        );
    }
    // A device that did not enable dma-buf imports nothing.
    let (mut h, host) = zink_with(&["VK_EXT_transform_feedback"]);
    h.send(&allocate(DEVICE, EXPORTED, SIZE, HOST_TYPE, Vec::new()))
        .unwrap();
    h.memory_blob(CTX, EXPORTED_RES, EXPORTED, SIZE).unwrap();
    let before = host.live("memory");
    h.send(&allocate(
        DEVICE,
        0x720,
        SIZE,
        HOST_TYPE,
        vec![import_info(EXPORTED_RES)],
    ))
    .unwrap();
    assert_eq!(host.live("memory"), before);
    assert_eq!(
        properties_of(&mut h, EXPORTED_RES).0,
        VK_ERROR_INVALID_EXTERNAL_HANDLE
    );
}

/// A second guest process: context 2, its own instance and Zink device.
fn second_context(h: &mut Harness<FakeVulkan>) {
    h.use_context(2);
    boot(h);
    let Command::CreateDevice(d) = h.call(&device_with(ZINK_EXTENSIONS, Vec::new())).unwrap()
    else {
        panic!()
    };
    assert_eq!(d.ret, VK_SUCCESS);
}

#[test]
fn another_context_imports_the_blob_only_once_it_is_attached_and_the_pages_outlive_everything() {
    let (mut h, host) = zink();
    let pages = export(&mut h);
    second_context(&mut h);
    // Not attached: another process's memory is out of reach.
    assert_eq!(
        properties_of(&mut h, EXPORTED_RES).0,
        VK_ERROR_INVALID_EXTERNAL_HANDLE
    );
    let before = host.live("memory");
    h.send(&allocate(
        DEVICE,
        0x730,
        SIZE,
        HOST_TYPE,
        vec![import_info(EXPORTED_RES)],
    ))
    .unwrap();
    assert_eq!(host.live("memory"), before, "no import without the attach");
    // The guest kernel attaches it when the second process opens the dma-buf.
    h.renderer.ctx_attach_blob(2, EXPORTED_RES, true);
    assert_eq!(
        properties_of(&mut h, EXPORTED_RES),
        (VK_SUCCESS, 0x18, SIZE)
    );
    h.send(&allocate(
        DEVICE,
        0x731,
        SIZE,
        HOST_TYPE,
        vec![import_info(EXPORTED_RES)],
    ))
    .expect("the import");
    assert!(!h.fatal());
    let weak = host.imports().last().expect("imported").pages.clone();
    assert_eq!(
        weak.upgrade().map(|p| p.host_addr()),
        Some(pages.host_addr())
    );
    drop(pages);

    // The exporter frees its memory, its blob goes, its whole context goes:
    // the importer's pages stay, and their budget with them.
    h.use_context(CTX);
    h.send(&free(DEVICE, EXPORTED)).unwrap();
    h.renderer.destroy_blob(EXPORTED_RES);
    h.renderer.ctx_destroy(CTX);
    assert!(weak.upgrade().is_some(), "the import keeps the pages");
    assert!(h.renderer.factory().host_visible_bytes() >= SIZE);
    // Detaching now changes nothing for the import made.
    h.renderer.ctx_attach_blob(2, EXPORTED_RES, false);
    h.use_context(2);
    h.send(&free(DEVICE, 0x731)).unwrap();
    assert!(weak.upgrade().is_none(), "the last holder freed them");
    assert_eq!(h.renderer.factory().host_visible_bytes(), 0);
}

#[test]
fn a_feature_structure_of_an_extension_not_enabled_never_reaches_the_driver() {
    let host = Arc::new(FakeVulkan::new(vec![fake::zink_gpu(
        "NVIDIA GeForce RTX 2070",
    )]));
    let mut h = Harness::new(Arc::clone(&host));
    boot(&mut h);
    let tf = VkDeviceCreateInfoNext::VkPhysicalDeviceTransformFeedbackFeaturesEXT(
        VkPhysicalDeviceTransformFeedbackFeaturesEXT {
            transform_feedback: 1,
            geometry_streams: 0,
        },
    );
    let Command::CreateDevice(d) = h
        .call(&device_with(&["VK_EXT_conditional_rendering"], vec![tf]))
        .unwrap()
    else {
        panic!()
    };
    assert_eq!(d.ret, VK_SUCCESS);
    let request = host.device_requests().pop().unwrap();
    assert!(
        request.chain.iter().all(|l| !matches!(
            l,
            VkDeviceCreateInfoNext::VkPhysicalDeviceTransformFeedbackFeaturesEXT(_)
        )),
        "transform feedback was not enabled, so its features are not forwarded"
    );
    // A feature the guest was not told of is still refused.
    let (mut h2, _) = (Harness::new(Arc::new(FakeVulkan::standard())), ());
    boot(&mut h2);
    let tf = VkDeviceCreateInfoNext::VkPhysicalDeviceTransformFeedbackFeaturesEXT(
        VkPhysicalDeviceTransformFeedbackFeaturesEXT {
            transform_feedback: 1,
            geometry_streams: 0,
        },
    );
    let Command::CreateDevice(d) = h2.call(&device_with(&[], vec![tf])).unwrap() else {
        panic!()
    };
    assert_eq!(d.ret, VK_ERROR_FEATURE_NOT_PRESENT);
}
