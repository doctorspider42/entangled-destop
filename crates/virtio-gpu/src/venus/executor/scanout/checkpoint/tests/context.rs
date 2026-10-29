//! Native context reconstruction, using the same on-disk shared-payload fixture.
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use super::super::*;
use crate::host_vulkan::AshVulkan;
use crate::renderer::Renderer3d;
use crate::venus::executor::checkpoint::MAX_RECIPE_BYTES;
use crate::venus::executor::checkpoint_factory::FactoryObjectCheckpoint;
use crate::venus::executor::checkpoint_pages::{HostPageTable, PageBudgetOwners};
use crate::venus::executor::checkpoint_shared::{PayloadBudgetOwners, SharedPayloads};
use crate::venus::executor::harness as h;
use crate::venus::executor::recording as r;
use crate::venus::executor::{s1_tests as s1, VulkanContext};
use crate::venus::protocol::*;

pub(super) mod initial;
mod private;
mod rings;
pub(super) mod transition;

const RESOURCE: u32 = 88;
const PAGE_RESOURCE: u32 = RESOURCE + 1;
const WAIT: Duration = Duration::from_secs(5);
const CB: u64 = r::CB + 1000;
const FENCE: u64 = r::FENCE + 1000;

fn barrier(layouts: (i32, i32), families: (u32, u32), accesses: (u32, u32)) -> Command<'static> {
    Command::CmdPipelineBarrier(CmdPipelineBarrierArgs {
        command_buffer: VkCommandBuffer(CB),
        src_stage_mask: 0x1_0000,
        dst_stage_mask: 0x1_0000,
        image_memory_barrier_count: 1,
        p_image_memory_barriers: Some(vec![VkImageMemoryBarrier {
            p_next: Vec::new(),
            src_access_mask: accesses.0,
            dst_access_mask: accesses.1,
            old_layout: layouts.0,
            new_layout: layouts.1,
            src_queue_family_index: families.0,
            dst_queue_family_index: families.1,
            image: VkImage(h::IMAGE),
            subresource_range: VkImageSubresourceRange {
                aspect_mask: 1,
                base_mip_level: 0,
                level_count: 1,
                base_array_layer: 0,
                layer_count: 1,
            },
        }]),
        ..Default::default()
    })
}

fn clear(color: [f32; 4]) -> Command<'static> {
    Command::CmdClearColorImage(CmdClearColorImageArgs {
        command_buffer: VkCommandBuffer(CB),
        image: VkImage(h::IMAGE),
        image_layout: 7,
        p_color: Some(VkClearColorValue::Float32(color)),
        range_count: 1,
        p_ranges: Some(vec![VkImageSubresourceRange {
            aspect_mask: 1,
            base_mip_level: 0,
            level_count: 1,
            base_array_layer: 0,
            layer_count: 1,
        }]),
    })
}

fn run(context: &mut VulkanContext<AshVulkan>, mut command: Command<'_>) {
    if crate::venus::executor::submit::is_wait(&command) {
        let deadline = std::time::Instant::now() + WAIT;
        while !context
            .execute_wait(&mut command, Duration::from_millis(10))
            .unwrap()
        {
            assert!(
                std::time::Instant::now() < deadline,
                "{} did not finish",
                command.name()
            );
        }
    } else {
        context.execute(&mut command).unwrap();
    }
    assert!(
        crate::venus::executor::context::result_of(&command).is_none_or(|ret| ret == VK_SUCCESS)
    );
}

fn start(harness: &mut h::Harness<AshVulkan>) {
    for command in [
        h::create_instance(h::INSTANCE),
        h::enumerate(h::INSTANCE, Some(vec![h::PHYSICAL])),
        s1::device_with(h::PHYSICAL, s1::S1_EXTENSIONS),
        h::device_queue(h::DEVICE, h::QUEUE, 1),
        h::create_pool(h::DEVICE, h::POOL),
    ] {
        let command = harness.call(&command).unwrap();
        assert!(crate::venus::executor::context::result_of(&command)
            .is_none_or(|ret| ret == VK_SUCCESS));
    }
}

pub(super) fn save(host: Arc<AshVulkan>, arena: &mut ScanoutDevice<AshVulkan>, directory: &Path) {
    let template = super::seed(arena, 6);
    let record = parse(&template).unwrap();
    let budget = PageBudget::new(MAX_RECIPE_BYTES);
    let host_type = host.usable_devices().unwrap()[0]
        .host_visible_types()
        .trailing_zeros();
    let quiesce = virtio_core::Quiesce::new();
    let mut factory = crate::venus::executor::ExecutorFactory::new(host);
    factory.enable_checkpoint_recipes(&budget).unwrap();
    let mut harness = h::Harness::gated_factory(factory, quiesce.clone());
    start(&mut harness);
    let mut image = s1::exporter_image();
    image.extent.width = record.image.width;
    image.extent.height = record.image.height;
    harness
        .send(&h::create_image(h::DEVICE, h::IMAGE, image.clone()))
        .unwrap();
    harness
        .send(&h::allocate(
            h::DEVICE,
            h::MEMORY,
            record.size,
            record.type_index,
            vec![s1::dma_buf_export()],
        ))
        .unwrap();
    harness
        .send(&h::bind_image(h::DEVICE, h::IMAGE, h::MEMORY, 0))
        .unwrap();
    harness
        .memory_blob(harness.ctx, RESOURCE, h::MEMORY, record.size)
        .unwrap();
    let ignored = (u32::MAX, u32::MAX);
    for command in [
        r::allocate_cbs(h::DEVICE, h::POOL, &[CB], false),
        r::begin(CB),
        barrier((0, 7), ignored, (0, 0x1000)),
        clear([0.0, 1.0, 0.0, 1.0]),
        barrier((7, 6), ignored, (0x1000, 0x800)),
        barrier((6, 6), (0, policy::QUEUE_FAMILY_FOREIGN), (0x800, 0)),
        r::end(CB),
        r::create_fence(h::DEVICE, FENCE, false),
        r::queue_submit(h::QUEUE, &[CB], FENCE),
        r::wait_fences(h::DEVICE, &[FENCE], 2_000_000_000),
    ] {
        let command = harness.call(&command).unwrap();
        assert!(crate::venus::executor::context::result_of(&command)
            .is_none_or(|ret| ret == VK_SUCCESS));
    }
    const PAGE_MEMORY: u64 = h::MEMORY + 2000;
    harness
        .send(&h::allocate(
            h::DEVICE,
            PAGE_MEMORY,
            1 << 20,
            host_type,
            Vec::new(),
        ))
        .unwrap();
    harness
        .memory_blob(h::CTX, PAGE_RESOURCE, PAGE_MEMORY, 1 << 20)
        .unwrap();
    harness.use_context(2);
    start(&mut harness);
    harness.renderer.ctx_attach_blob(2, RESOURCE, true);
    let properties = harness
        .call(&Command::GetMemoryResourcePropertiesMESA(
            GetMemoryResourcePropertiesMESAArgs {
                device: VkDevice(h::DEVICE),
                resource_id: RESOURCE,
                p_memory_resource_properties: Some(VkMemoryResourcePropertiesMESA::default()),
                ret: VK_ERROR_UNKNOWN,
            },
        ))
        .unwrap();
    let Command::GetMemoryResourcePropertiesMESA(properties) = properties else {
        panic!()
    };
    assert_eq!(properties.ret, VK_SUCCESS);
    assert_ne!(
        properties
            .p_memory_resource_properties
            .unwrap()
            .memory_type_bits
            & (1 << record.type_index),
        0
    );
    // Two retired imports precede the live allocation: one into VRAM, one into
    // host pages. Their original blobs will not exist during reconstruction.
    const OLD_IMAGE: u64 = h::IMAGE + 2000;
    harness
        .send(&h::create_image(h::DEVICE, OLD_IMAGE, image.clone()))
        .unwrap();
    harness
        .send(&h::allocate(
            h::DEVICE,
            h::MEMORY,
            4096,
            record.type_index,
            vec![s1::import_of(RESOURCE)],
        ))
        .unwrap();
    harness
        .send(&h::bind_image(h::DEVICE, OLD_IMAGE, h::MEMORY, 0))
        .unwrap();
    harness
        .send(&h::destroy_image(h::DEVICE, OLD_IMAGE))
        .unwrap();
    harness.send(&h::free(h::DEVICE, h::MEMORY)).unwrap();
    harness.renderer.ctx_attach_blob(2, PAGE_RESOURCE, true);
    harness
        .send(&h::create_buffer(
            h::DEVICE,
            h::BUFFER,
            h::buffer_info(1 << 20, 3),
        ))
        .unwrap();
    harness
        .send(&h::allocate(
            h::DEVICE,
            PAGE_MEMORY,
            4096,
            host_type,
            vec![s1::import_of(PAGE_RESOURCE)],
        ))
        .unwrap();
    harness
        .send(&h::bind_buffers(h::DEVICE, &[(h::BUFFER, PAGE_MEMORY, 0)]))
        .unwrap();
    harness
        .send(&h::destroy_buffer(h::DEVICE, h::BUFFER))
        .unwrap();
    harness.send(&h::free(h::DEVICE, PAGE_MEMORY)).unwrap();
    harness.use_context(h::CTX);
    harness
        .send(&h::create_buffer(
            h::DEVICE,
            h::BUFFER,
            h::buffer_info(1 << 20, 3),
        ))
        .unwrap();
    harness
        .send(&h::bind_buffers(h::DEVICE, &[(h::BUFFER, PAGE_MEMORY, 0)]))
        .unwrap();
    harness.use_context(2);
    harness
        .send(&h::create_buffer(
            h::DEVICE,
            h::BUFFER,
            h::buffer_info(1 << 20, 3),
        ))
        .unwrap();
    harness
        .send(&h::allocate(
            h::DEVICE,
            PAGE_MEMORY,
            4096,
            host_type,
            vec![s1::import_of(PAGE_RESOURCE)],
        ))
        .unwrap();
    harness
        .send(&h::bind_buffers(h::DEVICE, &[(h::BUFFER, PAGE_MEMORY, 0)]))
        .unwrap();
    harness
        .send(&h::create_image(h::DEVICE, h::IMAGE, image))
        .unwrap();
    harness
        .send(&h::allocate(
            h::DEVICE,
            h::MEMORY,
            record.size,
            record.type_index,
            vec![s1::import_of(RESOURCE)],
        ))
        .unwrap();
    harness
        .send(&h::bind_image(h::DEVICE, h::IMAGE, h::MEMORY, 0))
        .unwrap();
    private::create(&mut harness, record.type_index);
    let mut payloads = SharedPayloads::default();
    let mut page_table = HostPageTable::default();
    // Record a newer release but never submit it. A checkpoint must continue
    // to use layout 6 from actual executed work, not this directory hint.
    harness.use_context(h::CTX);
    for command in [
        Command::ResetCommandBuffer(ResetCommandBufferArgs {
            command_buffer: VkCommandBuffer(CB),
            flags: 0,
            ret: VK_ERROR_UNKNOWN,
        }),
        r::begin(CB),
        barrier((6, 7), (0, policy::QUEUE_FAMILY_FOREIGN), (0, 0)),
        r::end(CB),
    ] {
        harness.call(&command).unwrap();
    }
    for ctx in [h::CTX, 2] {
        harness.use_context(ctx);
        harness.call(&h::enumerate_instance_version()).unwrap();
        let reply_resource = if ctx == h::CTX {
            h::REPLY_RES
        } else {
            1000 + ctx * 2 + 1
        };
        harness
            .renderer
            .map_blob(reply_resource, u64::from(ctx - 1) << 20, h::REPLY_BYTES)
            .unwrap();
        harness
            .submit_virtqueue_seqno(100 + u64::from(ctx))
            .unwrap();
    }
    harness
        .renderer
        .map_blob(PAGE_RESOURCE, 2 << 20, 1 << 20)
        .unwrap();
    quiesce.pause();
    assert!(quiesce.wait_until_idle(WAIT));
    let graph = harness.renderer.checkpoint_object_graph(WAIT).unwrap();
    private::save(&harness, directory);
    std::fs::write(
        directory.join("vm-object-graph"),
        graph.metadata.encode().unwrap(),
    )
    .unwrap();
    let mut budgets = harness
        .renderer
        .factory()
        .checkpoint_budget_table()
        .unwrap();
    let objects = harness
        .renderer
        .factory()
        .checkpoint_objects(&mut budgets, &mut page_table, &mut payloads, WAIT)
        .unwrap();
    std::fs::write(directory.join("vm-factory"), objects.encode().unwrap()).unwrap();
    assert_eq!(
        payloads.handles().len(),
        1,
        "native exporter and importer share one payload"
    );
    assert_eq!(page_table.pages().len(), 1);
    let blob_state = harness
        .renderer
        .checkpoint_blobs(&mut page_table, &mut payloads)
        .unwrap();
    std::fs::write(directory.join("vm-blobs"), blob_state.encode().unwrap()).unwrap();
    let rings = harness
        .renderer
        .checkpoint_rings(&mut page_table, WAIT)
        .unwrap();
    std::fs::write(directory.join("vm-rings"), rings.encode().unwrap()).unwrap();
    let page_owners = page_table.checkpoint_budget_owners(&mut budgets).unwrap();
    blob_state.validate_page_owners(&page_owners).unwrap();
    std::fs::write(
        directory.join("vm-page-owners"),
        page_owners.encode(&budgets).unwrap(),
    )
    .unwrap();
    let payload_owners = payloads
        .checkpoint_budget_owners::<AshVulkan>(&mut budgets)
        .unwrap();
    std::fs::write(
        directory.join("vm-payload-owners"),
        payload_owners.encode(&budgets).unwrap(),
    )
    .unwrap();
    std::fs::write(directory.join("vm-budgets"), budgets.encode().unwrap()).unwrap();
    page_table
        .page(1)
        .unwrap()
        .write_bytes(0, &vec![0x17; 1 << 20])
        .unwrap();
    let mut page_bytes = Vec::new();
    page_table.save(&mut page_bytes).unwrap();
    std::fs::write(directory.join("vm-pages"), page_bytes).unwrap();
    harness
        .renderer
        .save_checkpoint_archive(
            &mut std::fs::File::create(directory.join("vm-renderer-archive")).unwrap(),
            WAIT,
        )
        .unwrap();
    let (target, executed_release) = harness
        .renderer
        .factory()
        .checkpoint_shared_target(RESOURCE, &payloads.handles()[0])
        .unwrap();
    assert_eq!(
        executed_release, record.state,
        "an unsubmitted barrier cannot change captured GPU ownership"
    );
    harness.renderer.ctx_destroy(h::CTX);
    harness.renderer.ctx_destroy(2);
    let (_, orphan_release) = harness
        .renderer
        .factory()
        .checkpoint_shared_target(RESOURCE, &target.handle)
        .unwrap();
    assert_eq!(
        orphan_release, executed_release,
        "executed ownership survives exporter teardown while the payload lives"
    );
    drop(harness);
    let mut pixels = Vec::new();
    arena
        .save_shared_image(&target, executed_release, &mut pixels)
        .unwrap();
    assert!(parse(&pixels)
        .unwrap()
        .pixels
        .chunks_exact(4)
        .all(|pixel| pixel == [0, 255, 0, 255]));
    std::fs::write(directory.join("vm-image"), pixels).unwrap();
}

pub(super) fn restore(
    host: Arc<AshVulkan>,
    arena: &mut ScanoutDevice<AshVulkan>,
    directory: &Path,
) {
    let bytes = std::fs::read(directory.join("vm-image")).unwrap();
    let record = parse(&bytes).unwrap();
    let factory = crate::venus::executor::ExecutorFactory::new(host.clone());
    let budgets = factory
        .restore_budget_table(&std::fs::read(directory.join("vm-budgets")).unwrap())
        .unwrap();
    let payload_owners = PayloadBudgetOwners::decode(
        &std::fs::read(directory.join("vm-payload-owners")).unwrap(),
        &budgets,
    )
    .unwrap();
    let payload_owner = payload_owners.owner(1).unwrap();
    let limits = factory
        .checkpoint_payload_limits(payload_owner, &budgets)
        .unwrap();
    let restored_image = arena
        .restore_shared_image_checkpoint(
            RESOURCE,
            &bytes,
            &limits,
            budgets.budget(payload_owner).unwrap(),
        )
        .unwrap();
    factory
        .adopt_checkpoint_shared_image(&restored_image, payload_owner, &budgets)
        .unwrap();
    let foreign = crate::venus::executor::ExecutorFactory::new(host.clone());
    let foreign_budgets = foreign
        .restore_budget_table(&std::fs::read(directory.join("vm-budgets")).unwrap())
        .unwrap();
    assert!(
        foreign
            .adopt_checkpoint_shared_image(&restored_image, payload_owner, &foreign_budgets)
            .is_err(),
        "identical numeric owner ids cannot move a native upload between factories"
    );
    let target = restored_image.target().clone();
    let (_, captured_release) = factory
        .checkpoint_shared_target(RESOURCE, &target.handle)
        .unwrap();
    assert_eq!(
        captured_release,
        restored_image.state(),
        "a second save knows the actual uploaded ownership"
    );
    let payloads = SharedPayloads::from_handles(vec![target.handle.clone()]).unwrap();
    payload_owners
        .validate_handles::<AshVulkan>(&payloads, &budgets)
        .unwrap();
    let budget = PageBudget::new(MAX_RECIPE_BYTES);
    let page_bytes = std::fs::read(directory.join("vm-pages")).unwrap();
    let page_owners = PageBudgetOwners::decode(
        &std::fs::read(directory.join("vm-page-owners")).unwrap(),
        &budgets,
    )
    .unwrap();
    let blob_state = crate::venus::renderer::checkpoint_blobs::BlobCheckpointSet::decode(
        &std::fs::read(directory.join("vm-blobs")).unwrap(),
    )
    .unwrap();
    blob_state.validate_page_owners(&page_owners).unwrap();
    let page_table = HostPageTable::restore_owned(&page_bytes, &page_owners, &budgets).unwrap();
    let objects =
        FactoryObjectCheckpoint::decode(&std::fs::read(directory.join("vm-factory")).unwrap())
            .unwrap();
    let mut contexts = factory
        .restore_checkpoint_objects(&objects, &budgets, &page_table, &payloads, &budget, WAIT)
        .unwrap();
    for context in &mut contexts {
        private::restore(directory, context.context_id(), context).unwrap();
    }
    let gate = virtio_core::Quiesce::new();
    gate.pause();
    let window = Arc::new(h::RecordingWindow::default());
    let mut restored_renderer = crate::venus::renderer::VenusRenderer::new(factory);
    restored_renderer.set_quiesce(gate.clone());
    restored_renderer.set_host_visible(window.clone());
    restored_renderer
        .restore_blobs(&blob_state, &page_table, &payloads, |saved, payload| {
            let context = contexts
                .iter_mut()
                .find(|context| context.ctx_id == saved.ctx_id)
                .expect("the directory's image context was reconstructed");
            context.restore_directory_image(saved, payload)
        })
        .unwrap();
    let restored_directory = restored_renderer.blob_directory().clone();
    assert_eq!(window.count(), blob_state.mappings().count());
    assert_eq!(
        window.count(),
        3,
        "reply windows and the native page allocation were actually published"
    );
    assert_eq!(
        window.at(2 << 20).unwrap().0,
        page_table.page(1).unwrap().host_addr()
    );
    for (_, offset, len) in blob_state.mappings() {
        assert_eq!(
            window.at(offset).unwrap().1,
            len,
            "the guest aperture span survives"
        );
    }
    for context in &mut contexts {
        context.blobs = Some(restored_directory.for_context(context.ctx_id));
    }
    let imported_blob = restored_directory.for_context(2).memory(RESOURCE).unwrap();
    let crate::venus::renderer::ExportedMemory::Handle(imported_blob) = imported_blob.backing
    else {
        panic!()
    };
    assert!(crate::venus::renderer::SharedRef::of(&imported_blob).is(&target.handle));
    assert!(restored_directory.for_context(3).memory(RESOURCE).is_err());
    let page_blob = restored_directory
        .for_context(2)
        .memory(PAGE_RESOURCE)
        .unwrap();
    assert!(Arc::ptr_eq(
        page_blob.backing.pages().unwrap(),
        page_table.page(1).unwrap()
    ));
    assert!(Arc::ptr_eq(
        &contexts[0].shared_allocation_links().unwrap()[0].handle.0,
        &contexts[1].shared_allocation_links().unwrap()[0].handle.0
    ));
    let first_pages = contexts[0].host_allocation_links().unwrap()[0]
        .pages
        .clone();
    let second_pages = contexts[1].host_allocation_links().unwrap()[0]
        .pages
        .clone();
    assert!(Arc::ptr_eq(&first_pages, &second_pages));
    assert!(Arc::ptr_eq(&first_pages, page_table.page(1).unwrap()));
    let mut original = [0; 16];
    second_pages.read_bytes(0, &mut original).unwrap();
    assert_eq!(original, [0x17; 16]);
    first_pages.write_bytes(0, &vec![0xb6; 1 << 20]).unwrap();
    let ignored = (u32::MAX, u32::MAX);
    // Write through the reconstructed exporter's image, then read through the
    // separately reconstructed importer's image on another native VkDevice.
    let context = &mut contexts[0];
    run(
        context,
        Command::ResetCommandBuffer(ResetCommandBufferArgs {
            command_buffer: VkCommandBuffer(CB),
            flags: 0,
            ret: VK_ERROR_UNKNOWN,
        }),
    );
    for command in [
        r::begin(CB),
        barrier((6, 6), (policy::QUEUE_FAMILY_FOREIGN, 0), (0, 0x1000)),
        barrier((6, 7), ignored, (0, 0x1000)),
        clear([0.0, 0.0, 1.0, 1.0]),
        barrier((7, 6), ignored, (0x1000, 0x800)),
        barrier((6, 6), (0, policy::QUEUE_FAMILY_FOREIGN), (0x800, 0)),
        r::end(CB),
        r::reset_fences(h::DEVICE, &[FENCE]),
        r::queue_submit(h::QUEUE, &[CB], FENCE),
        r::wait_fences(h::DEVICE, &[FENCE], 2_000_000_000),
    ] {
        run(context, command);
    }
    assert_eq!(context.held_items(), 0);
    let context = &mut contexts[1];
    let size = record.pixels.len() as u64;
    const OUT_BUFFER: u64 = h::BUFFER + 1000;
    run(
        context,
        h::create_buffer(h::DEVICE, OUT_BUFFER, h::buffer_info(size + (1 << 20), 3)),
    );
    let mut requirements = h::buffer_requirements(h::DEVICE, OUT_BUFFER);
    context.execute(&mut requirements).unwrap();
    let Command::GetBufferMemoryRequirements2(requirements) = requirements else {
        panic!()
    };
    let requirements = requirements
        .p_memory_requirements
        .unwrap()
        .memory_requirements;
    let types =
        host.usable_devices().unwrap()[0].host_visible_types() & requirements.memory_type_bits;
    assert_ne!(types, 0);
    const OUTPUT: u64 = h::MEMORY + 1000;
    run(
        context,
        h::allocate(
            h::DEVICE,
            OUTPUT,
            requirements.size,
            types.trailing_zeros(),
            Vec::new(),
        ),
    );
    run(
        context,
        h::bind_buffers(h::DEVICE, &[(OUT_BUFFER, OUTPUT, 0)]),
    );
    for command in [
        r::allocate_cbs(h::DEVICE, h::POOL, &[CB], false),
        r::begin(CB),
        barrier((6, 6), (policy::QUEUE_FAMILY_FOREIGN, 0), (0, 0x800)),
        Command::CmdCopyImageToBuffer(CmdCopyImageToBufferArgs {
            command_buffer: VkCommandBuffer(CB),
            src_image: VkImage(h::IMAGE),
            src_image_layout: 6,
            dst_buffer: VkBuffer(OUT_BUFFER),
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
                    width: record.image.width,
                    height: record.image.height,
                    depth: 1,
                },
            }]),
        }),
        r::buffer_barrier(CB, h::BUFFER, (0x4000, 0x4000), (0x1000, 0x800)),
        r::copy_buffer(CB, h::BUFFER, OUT_BUFFER, &[(0, size, 1 << 20)]),
        r::buffer_barrier(CB, OUT_BUFFER, (0x1000, 0x1000), (0x4000, 0x2000)),
        barrier((6, 6), (0, policy::QUEUE_FAMILY_FOREIGN), (0x800, 0)),
        r::end(CB),
        r::create_fence(h::DEVICE, FENCE, false),
        r::queue_submit(h::QUEUE, &[CB], FENCE),
        r::wait_fences(h::DEVICE, &[FENCE], 2_000_000_000),
    ] {
        run(context, command);
    }
    let mut output = Vec::new();
    assert_eq!(context.held_items(), 0);
    context.save_host_memory(&mut output).unwrap();
    let mut decoder = Decoder::new(&output);
    assert_eq!(decoder.u32().unwrap(), 1);
    let count = decoder.u32().unwrap();
    let mut result = None;
    for _ in 0..count {
        let id = decoder.u64().unwrap();
        let len = decoder.u64().unwrap() as usize;
        let data = decoder.blob(len).unwrap();
        if id == OUTPUT {
            result = Some(data);
        }
    }
    let result = result.expect("GPU output allocation was saved");
    assert!(
        result[size as usize..size as usize + (1 << 20)]
            .iter()
            .all(|byte| *byte == 0xb6),
        "the importer's GPU reads CPU writes through the exporter's restored page root"
    );
    let pixels = &result[..size as usize];
    let wrong = pixels
        .chunks_exact(4)
        .enumerate()
        .find(|(_, pixel)| *pixel != [255, 0, 0, 255]);
    assert!(
        wrong.is_none(),
        "the importer sees the exporter's new pixels; first mismatch {wrong:?}, first pixels {:?}",
        &pixels[..16]
    );
    for context in contexts {
        restored_renderer
            .checkpoint_factory_mut()
            .unwrap()
            .adopt_checkpoint_context(context)
            .unwrap();
    }
    rings::exercise(directory, &mut restored_renderer, &page_table, &gate, false);
    restore_whole_graph(host, arena, directory);
}

fn restore_whole_graph(
    host: Arc<AshVulkan>,
    arena: &mut ScanoutDevice<AshVulkan>,
    directory: &Path,
) {
    use crate::venus::renderer::checkpoint_objects::RendererObjectCheckpoint;
    let saved = RendererObjectCheckpoint::decode(
        &std::fs::read(directory.join("vm-object-graph")).unwrap(),
    )
    .unwrap();
    let mut factory = crate::venus::executor::ExecutorFactory::new(host.clone());
    let recipes = PageBudget::new(MAX_RECIPE_BYTES);
    factory.enable_checkpoint_recipes(&recipes).unwrap();
    let budgets = saved.resolve_budgets(&factory).unwrap();
    let owners = saved.payload_owners(&budgets).unwrap();
    let owner = owners.owner(1).unwrap();
    let image = arena
        .restore_shared_image_checkpoint(
            RESOURCE,
            &std::fs::read(directory.join("vm-image")).unwrap(),
            &factory.checkpoint_payload_limits(owner, &budgets).unwrap(),
            budgets.budget(owner).unwrap(),
        )
        .unwrap();
    factory
        .adopt_checkpoint_shared_image(&image, owner, &budgets)
        .unwrap();
    let payloads = SharedPayloads::from_handles(vec![image.target().handle.clone()]).unwrap();
    let pages = HostPageTable::restore_owned_reader(
        &mut std::fs::File::open(directory.join("vm-pages")).unwrap(),
        &saved.page_owners(&budgets).unwrap(),
        &budgets,
    )
    .unwrap();
    let gate = virtio_core::Quiesce::new();
    gate.pause();
    let window = Arc::new(h::RecordingWindow::default());
    let mut renderer = crate::venus::renderer::VenusRenderer::new(factory);
    renderer.set_quiesce(gate.clone());
    renderer.set_host_visible(window.clone());
    let mut visited = Vec::new();
    renderer
        .restore_object_graph(
            &saved,
            &budgets,
            &pages,
            &payloads,
            &recipes,
            |ctx, context| {
                assert_eq!(context.shared_allocation_links().unwrap().len(), 1);
                assert_eq!(context.host_allocation_links().unwrap().len(), 1);
                visited.push(ctx);
                private::restore(directory, ctx, context)
            },
        )
        .unwrap();
    assert_eq!(visited, [h::CTX, 2]);
    private::verify(directory, &renderer);
    assert!(window.count() > 0);
    let (target, release) = renderer
        .factory()
        .checkpoint_shared_target(RESOURCE, &image.target().handle)
        .unwrap();
    let mut copied = Vec::new();
    arena
        .save_shared_image(&target, release, &mut copied)
        .unwrap();
    assert!(parse(&copied)
        .unwrap()
        .pixels
        .chunks_exact(4)
        .all(|pixel| pixel == [0, 255, 0, 255]));
    rings::exercise(directory, &mut renderer, &pages, &gate, true);
    assert_eq!(window.count(), 0);
    drop(renderer);
    restore_archive(host, directory);
}

fn restore_archive(host: Arc<AshVulkan>, directory: &Path) {
    let gate = virtio_core::Quiesce::new();
    gate.pause();
    let window = Arc::new(h::RecordingWindow::default());
    let mut renderer = crate::venus::renderer::VenusRenderer::new(
        crate::venus::executor::ExecutorFactory::new(host),
    );
    renderer.set_quiesce(gate.clone());
    renderer.set_host_visible(window.clone());
    renderer
        .load_checkpoint_archive(
            &mut std::fs::File::open(directory.join("vm-renderer-archive")).unwrap(),
            &PageBudget::new(MAX_RECIPE_BYTES),
            WAIT,
        )
        .unwrap();
    assert_eq!(window.count(), 3);
    private::verify(directory, &renderer);
    let graph = renderer.checkpoint_object_graph(WAIT).unwrap();
    assert_eq!(graph.shared_images.len(), 1);
    let mut second = Vec::new();
    renderer.save_checkpoint_archive(&mut second, WAIT).unwrap();
    assert!(second.len() > 3 << 20);
    let mut ring_pages = HostPageTable::default();
    let saved_rings = renderer.checkpoint_rings(&mut ring_pages, WAIT).unwrap();
    rings::exercise_saved(saved_rings, &mut renderer, &ring_pages, &gate, true);
    assert_eq!(window.count(), 0);
}
