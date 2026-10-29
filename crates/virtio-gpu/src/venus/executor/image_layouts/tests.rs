use super::super::limits::{Caps, Limits};
use super::super::{fake::FakeVulkan, harness as h, recording as r, VulkanContext};
use super::*;

fn run(context: &mut VulkanContext<FakeVulkan>, mut command: Command<'_>) {
    context.execute(&mut command).unwrap();
    assert!(super::super::context::result_of(&command).is_none_or(|ret| ret == VK_SUCCESS));
    assert!(
        context.image_tracking.error.is_none(),
        "{:?}",
        context.image_tracking.error
    );
}

fn setup() -> VulkanContext<FakeVulkan> {
    setup_on(VulkanContext::new(1, Arc::new(FakeVulkan::standard())))
}

fn setup_on(mut context: VulkanContext<FakeVulkan>) -> VulkanContext<FakeVulkan> {
    run(&mut context, h::create_instance(h::INSTANCE));
    run(
        &mut context,
        h::enumerate(h::INSTANCE, Some(vec![h::PHYSICAL])),
    );
    run(
        &mut context,
        h::create_device(
            h::PHYSICAL,
            h::DEVICE,
            vec![
                VkDeviceCreateInfoNext::VkPhysicalDeviceVulkan11Features(
                    VkPhysicalDeviceVulkan11Features {
                        multiview: 1,
                        ..Default::default()
                    },
                ),
                VkDeviceCreateInfoNext::VkPhysicalDeviceVulkan12Features(
                    VkPhysicalDeviceVulkan12Features {
                        timeline_semaphore: 1,
                        ..Default::default()
                    },
                ),
                VkDeviceCreateInfoNext::VkPhysicalDeviceVulkan13Features(
                    VkPhysicalDeviceVulkan13Features {
                        synchronization2: 1,
                        ..Default::default()
                    },
                ),
            ],
        ),
    );
    run(&mut context, h::device_queue(h::DEVICE, h::QUEUE, 1));
    run(&mut context, h::create_pool(h::DEVICE, h::POOL));
    let mut info = h::image_info();
    info.usage = 0x13;
    info.mip_levels = 3;
    info.array_layers = 4;
    run(&mut context, h::create_image(h::DEVICE, h::IMAGE, info));
    run(
        &mut context,
        h::allocate(h::DEVICE, h::MEMORY, 4 << 20, 1, Vec::new()),
    );
    run(
        &mut context,
        h::bind_image(h::DEVICE, h::IMAGE, h::MEMORY, 0),
    );
    run(
        &mut context,
        r::allocate_cbs(h::DEVICE, h::POOL, &[r::CB], false),
    );
    context
}

fn cell(context: &VulkanContext<FakeVulkan>, mip: u32, layer: u32) -> Cell {
    let state = context.image_tracking.images.get(&h::IMAGE).unwrap();
    let matching: Vec<_> = state
        .spans
        .iter()
        .filter(|s| {
            s.range.aspects & 1 != 0
                && (s.range.mip..s.range.mip + s.range.mips).contains(&mip)
                && (s.range.layer..s.range.layer + s.range.layers).contains(&layer)
        })
        .collect();
    assert_eq!(matching.len(), 1);
    matching[0].cell
}

fn transition(
    cb: u64,
    layout: i32,
    mip: u32,
    mips: u32,
    layer: u32,
    layers: u32,
) -> Command<'static> {
    let mut command = r::image_barrier2(cb, h::IMAGE, (0, layout), (0, 0), (0x1_0000, 0));
    if let Command::CmdPipelineBarrier2(args) = &mut command {
        args.p_dependency_info
            .as_mut()
            .unwrap()
            .p_image_memory_barriers
            .as_mut()
            .unwrap()[0]
            .subresource_range = VkImageSubresourceRange {
            aspect_mask: 1,
            base_mip_level: mip,
            level_count: mips,
            base_array_layer: layer,
            layer_count: layers,
        };
    }
    command
}

#[test]
fn recording_and_resetting_barriers_does_not_change_executed_layouts() {
    let mut context = setup();
    run(&mut context, r::begin(r::CB));
    run(&mut context, transition(r::CB, 6, 1, 1, 1, 2));
    assert_eq!(cell(&context, 1, 1).layout, 0);
    run(&mut context, r::end(r::CB));
    run(
        &mut context,
        Command::ResetCommandBuffer(ResetCommandBufferArgs {
            command_buffer: VkCommandBuffer(r::CB),
            flags: 0,
            ret: 0,
        }),
    );
    run(&mut context, r::begin(r::CB));
    run(&mut context, transition(r::CB, 1, 0, 1, 3, 1));
    run(&mut context, r::end(r::CB));
    run(&mut context, r::queue_submit(h::QUEUE, &[r::CB], 0));
    assert_eq!(cell(&context, 1, 1).layout, 0);
    assert_eq!(
        cell(&context, 0, 3),
        Cell {
            layout: 1,
            owner: Owner::Queue(0)
        }
    );
    context.destroy_all();
}

#[test]
fn a_held_submit_does_not_transition_until_its_future_signal_releases_it() {
    let mut context = setup();
    run(
        &mut context,
        r::create_semaphore(h::DEVICE, 0x700, Some(0), 0),
    );
    run(&mut context, r::begin(r::CB));
    run(&mut context, transition(r::CB, 1, 0, u32::MAX, 0, u32::MAX));
    run(&mut context, r::end(r::CB));
    run(
        &mut context,
        r::submit_semaphores(h::QUEUE, &[r::CB], &[(0x700, 5)], &[], true, 0),
    );
    assert!(!context.holds.is_empty());
    assert_eq!(cell(&context, 0, 0).layout, 0);
    run(&mut context, r::signal_semaphore(h::DEVICE, 0x700, 5));
    assert!(context.holds.is_empty());
    assert_eq!(cell(&context, 0, 0).layout, 1);
    assert_eq!(cell(&context, 2, 3).layout, 1);
    context.destroy_all();
}

#[test]
fn equal_layout_sync2_dependencies_preserve_heterogeneous_subresource_layouts() {
    let mut context = setup();
    run(&mut context, r::begin(r::CB));
    run(&mut context, transition(r::CB, 1, 0, u32::MAX, 0, u32::MAX));
    run(&mut context, transition(r::CB, 6, 1, 1, 1, 2));
    let mut dependency = transition(r::CB, 0, 0, u32::MAX, 0, u32::MAX);
    // oldLayout == newLayout == UNDEFINED is legal in sync2: it is a
    // memory dependency, not a discard or a layout transition.
    if let Command::CmdPipelineBarrier2(args) = &mut dependency {
        args.p_dependency_info
            .as_mut()
            .unwrap()
            .p_image_memory_barriers
            .as_mut()
            .unwrap()[0]
            .old_layout = 0;
    }
    run(&mut context, dependency);
    run(&mut context, r::end(r::CB));
    run(&mut context, r::queue_submit(h::QUEUE, &[r::CB], 0));
    assert_eq!(cell(&context, 1, 1).layout, 6);
    assert_eq!(cell(&context, 1, 2).layout, 6);
    assert_eq!(cell(&context, 0, 0).layout, 1);
    assert_eq!(cell(&context, 2, 3).layout, 1);
    context.destroy_all();
}

#[test]
fn secondary_commands_apply_on_primary_submit_and_cannot_poison_a_reused_image_id() {
    let mut context = setup();
    let secondary = r::CB + 1;
    run(
        &mut context,
        r::allocate_cbs(h::DEVICE, h::POOL, &[secondary], true),
    );
    let mut begin = r::begin(secondary);
    if let Command::BeginCommandBuffer(args) = &mut begin {
        args.p_begin_info.as_mut().unwrap().p_inheritance_info =
            Some(VkCommandBufferInheritanceInfo::default());
    }
    run(&mut context, begin);
    run(&mut context, transition(secondary, 6, 0, 1, 0, 1));
    run(&mut context, r::end(secondary));
    run(&mut context, r::begin(r::CB));
    run(
        &mut context,
        Command::CmdExecuteCommands(CmdExecuteCommandsArgs {
            command_buffer: VkCommandBuffer(r::CB),
            command_buffer_count: 1,
            p_command_buffers: Some(vec![VkCommandBuffer(secondary)]),
        }),
    );
    run(&mut context, r::end(r::CB));
    assert_eq!(cell(&context, 0, 0).layout, 0);
    run(&mut context, r::queue_submit(h::QUEUE, &[r::CB], 0));
    assert_eq!(cell(&context, 0, 0).layout, 6);
    run(&mut context, h::destroy_image(h::DEVICE, h::IMAGE));
    run(
        &mut context,
        h::create_image(h::DEVICE, h::IMAGE, h::image_info()),
    );
    context
        .image_tracking
        .submitted(r::CB, 0, &context.objects, 0, None);
    assert_eq!(cell(&context, 0, 0).layout, 0);
    context.destroy_all();
}

fn render_pass_case(mask: u32) {
    let mut context = setup();
    let mut image_view = r::create_image_view(h::DEVICE, h::IMAGE_VIEW, h::IMAGE, h::RGBA8);
    if let Command::CreateImageView(args) = &mut image_view {
        let info = args.p_create_info.as_mut().unwrap();
        info.view_type = 5;
        info.subresource_range.base_array_layer = 1;
        info.subresource_range.layer_count = 3;
    }
    run(&mut context, image_view);
    let mut pass = r::create_render_pass(h::DEVICE, r::RENDER_PASS, h::RGBA8);
    if let Command::CreateRenderPass(args) = &mut pass {
        let info = args.p_create_info.as_mut().unwrap();
        // Even an unused attachment still executes initial/final layout
        // transitions. Multiview covers the union of all subpass view masks.
        info.p_subpasses.as_mut().unwrap()[0]
            .p_color_attachments
            .as_mut()
            .unwrap()[0]
            .attachment = u32::MAX;
        if mask != 0 {
            info.p_next
                .push(VkRenderPassCreateInfoNext::VkRenderPassMultiviewCreateInfo(
                    VkRenderPassMultiviewCreateInfo {
                        subpass_count: 1,
                        p_view_masks: Some(vec![mask]),
                        ..Default::default()
                    },
                ));
        }
    }
    run(&mut context, pass);
    run(
        &mut context,
        r::create_framebuffer(h::DEVICE, r::FRAMEBUFFER, r::RENDER_PASS, h::IMAGE_VIEW, 64),
    );
    run(&mut context, r::begin(r::CB));
    run(
        &mut context,
        r::begin_render_pass(r::CB, r::RENDER_PASS, r::FRAMEBUFFER, 64, [0.; 4]),
    );
    run(&mut context, r::end_render_pass(r::CB));
    run(&mut context, r::end(r::CB));
    assert_eq!(cell(&context, 0, 1).layout, 0);
    run(&mut context, r::queue_submit(h::QUEUE, &[r::CB], 0));
    for layer in 0..4 {
        let changed = if mask == 0 {
            layer == 1
        } else {
            layer >= 1 && mask & (1 << (layer - 1)) != 0
        };
        assert_eq!(cell(&context, 0, layer).layout, if changed { 6 } else { 0 });
        assert_eq!(cell(&context, 1, layer).layout, 0);
    }
    context.destroy_all();
}

#[test]
fn implicit_render_pass_transitions_use_framebuffer_layers_not_the_whole_view() {
    render_pass_case(0);
}

#[test]
fn implicit_multiview_transitions_cover_only_views_used_by_any_subpass() {
    render_pass_case(0b101);
}

#[test]
fn layout_spans_match_an_independent_grid_under_fragmented_depth_stencil_updates() {
    let all = Range {
        aspects: 6,
        mip: 0,
        mips: 3,
        layer: 0,
        layers: 4,
    };
    let initial = Cell {
        layout: 0,
        owner: Owner::Initial,
    };
    let limits = ContextLimits::default();
    let identity = Arc::new(());
    let mut state = ImageState {
        device: h::DEVICE,
        identity: identity.clone(),
        all,
        concurrent: false,
        spans: vec![Span {
            range: all,
            cell: initial,
        }],
        charge: charge(&limits, 1, std::mem::size_of::<Span>()).unwrap(),
    };
    let mut expected = [[[initial; 4]; 3]; 2];
    let mut seed = 0xface_cafeu32;
    let mut random = || {
        seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
        seed
    };
    for _ in 0..400 {
        let mip = random() % 3;
        let layer = random() % 4;
        let range = Range {
            aspects: [2, 4, 6][(random() % 3) as usize],
            mip,
            mips: 1 + random() % (3 - mip),
            layer,
            layers: 1 + random() % (4 - layer),
        };
        let layout = [1, 6, 7][(random() % 3) as usize];
        let op = Op {
            image: View {
                device: h::DEVICE,
                image: h::IMAGE,
                identity: identity.clone(),
                range,
                all_aspects: 6,
                is_3d: false,
            },
            layout,
            old_layout: 0,
            from: IGNORED,
            to: IGNORED,
        };
        state.apply(&op, 0).unwrap();
        for (aspect, grid) in expected.iter_mut().enumerate() {
            if range.aspects & (2 << aspect) == 0 {
                continue;
            }
            for row in grid.iter_mut().skip(mip as usize).take(range.mips as usize) {
                for cell in row
                    .iter_mut()
                    .skip(layer as usize)
                    .take(range.layers as usize)
                {
                    *cell = Cell {
                        layout,
                        owner: Owner::Queue(0),
                    };
                }
            }
        }
        for (aspect, grid) in expected.iter().enumerate() {
            for (mip, row) in grid.iter().enumerate() {
                for (layer, cell) in row.iter().enumerate() {
                    let matches: Vec<_> = state
                        .spans
                        .iter()
                        .filter(|s| {
                            s.range.aspects & (2 << aspect) != 0
                                && (s.range.mip..s.range.mip + s.range.mips).contains(&(mip as u32))
                                && (s.range.layer..s.range.layer + s.range.layers)
                                    .contains(&(layer as u32))
                        })
                        .collect();
                    assert_eq!(matches.len(), 1);
                    assert_eq!(matches[0].cell, *cell);
                }
            }
        }
    }
    state
        .apply(
            &Op {
                image: View {
                    device: h::DEVICE,
                    image: h::IMAGE,
                    identity,
                    range: all,
                    all_aspects: 6,
                    is_3d: false,
                },
                layout: 1,
                old_layout: 0,
                from: IGNORED,
                to: IGNORED,
            },
            0,
        )
        .unwrap();
    assert_eq!(state.spans.len(), 1);
}

#[test]
fn an_exhausted_tracking_budget_refuses_checkpointing_without_killing_the_guest() {
    let host = Arc::new(FakeVulkan::standard());
    let limits = Limits::new(Caps::default().with(Class::RecordingBytes, 1, 1));
    let mut context = VulkanContext::with_caps(
        1,
        host.clone(),
        crate::venus::shmem::PageBudget::new(1 << 20),
        super::super::timeline::FenceThreads::new(8),
        limits.context(),
    );
    for mut command in [
        h::create_instance(h::INSTANCE),
        h::enumerate(h::INSTANCE, Some(vec![h::PHYSICAL])),
        h::create_device(h::PHYSICAL, h::DEVICE, Vec::new()),
        h::create_image(h::DEVICE, h::IMAGE, h::image_info()),
    ] {
        context.execute(&mut command).unwrap();
        assert!(super::super::context::result_of(&command).is_none_or(|ret| ret == VK_SUCCESS));
    }
    assert!(context.image_tracking.error.is_some());
    assert!(!context.is_fatal());
    assert_eq!(host.live("image"), 1);
    context.destroy_all();
    assert_eq!(host.live_objects(), 0);
}

#[test]
fn repeated_secondary_execution_is_budgeted_before_expanding_its_image_commands() {
    let limits = Limits::new(Caps::default().with(Class::RecordingBytes, 128 << 10, 256 << 10));
    let context = VulkanContext::with_caps(
        1,
        Arc::new(FakeVulkan::standard()),
        crate::venus::shmem::PageBudget::new(8 << 20),
        super::super::timeline::FenceThreads::new(8),
        limits.context(),
    );
    let mut context = setup_on(context);
    let secondary = r::CB + 1;
    run(
        &mut context,
        r::allocate_cbs(h::DEVICE, h::POOL, &[secondary], true),
    );
    let mut begin = r::begin(secondary);
    if let Command::BeginCommandBuffer(args) = &mut begin {
        let info = args.p_begin_info.as_mut().unwrap();
        info.flags = 4; // SIMULTANEOUS_USE, including repeated execution.
        info.p_inheritance_info = Some(VkCommandBufferInheritanceInfo::default());
    }
    run(&mut context, begin);
    for _ in 0..100 {
        run(&mut context, transition(secondary, 1, 0, 1, 0, 1));
    }
    run(&mut context, r::end(secondary));
    run(&mut context, r::begin(r::CB));
    let mut execute = Command::CmdExecuteCommands(CmdExecuteCommandsArgs {
        command_buffer: VkCommandBuffer(r::CB),
        command_buffer_count: 300,
        p_command_buffers: Some(vec![VkCommandBuffer(secondary); 300]),
    });
    context.command_bytes = 8 + 8 + 4 + 8 + 300 * 8;
    context.execute(&mut execute).unwrap();
    assert!(!context.is_fatal());
    assert!(context
        .image_tracking
        .error
        .as_ref()
        .is_some_and(|e| e.contains("recording_bytes")));
    assert!(context.image_tracking.recordings.is_empty());
    context.destroy_all();
}
