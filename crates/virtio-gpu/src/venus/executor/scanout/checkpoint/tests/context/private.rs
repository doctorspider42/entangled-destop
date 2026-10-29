//! Private buffers and tiled images travel beside the global shared allocation
//! graph. Use real guest commands and real file-backed restore callbacks.
use super::*;

const BUFFER: u64 = 0x7fff_0001;
const BUFFER_MEMORY: u64 = 0x7fff_0002;
const IMAGE: u64 = 0x7fff_0003;
const IMAGE_MEMORY: u64 = 0x7fff_0004;
const PRIVATE_CB: u64 = 0x7fff_0005;
const PRIVATE_FENCE: u64 = 0x7fff_0006;
const SIZE: u64 = (2 << 20) + 64;

fn transition(old: i32, new: i32) -> Command<'static> {
    Command::CmdPipelineBarrier(CmdPipelineBarrierArgs {
        command_buffer: VkCommandBuffer(PRIVATE_CB),
        src_stage_mask: if old == 0 { 1 } else { 0x1000 },
        dst_stage_mask: 0x1000,
        image_memory_barrier_count: 1,
        p_image_memory_barriers: Some(vec![VkImageMemoryBarrier {
            old_layout: old,
            new_layout: new,
            src_access_mask: if old == 0 { 0 } else { 0x1000 },
            dst_access_mask: if new == 7 { 0x1000 } else { 0x800 },
            src_queue_family_index: u32::MAX,
            dst_queue_family_index: u32::MAX,
            image: VkImage(IMAGE),
            subresource_range: VkImageSubresourceRange {
                aspect_mask: 1,
                base_mip_level: 0,
                level_count: 1,
                base_array_layer: 0,
                layer_count: 1,
            },
            ..Default::default()
        }]),
        ..Default::default()
    })
}

pub(super) fn create(harness: &mut h::Harness<AshVulkan>, memory_type: u32) {
    harness.use_context(h::CTX);
    harness
        .send(&h::create_buffer(
            h::DEVICE,
            BUFFER,
            h::buffer_info(SIZE, 3),
        ))
        .unwrap();
    let Command::GetBufferMemoryRequirements2(requirements) = harness
        .call(&h::buffer_requirements(h::DEVICE, BUFFER))
        .unwrap()
    else {
        panic!()
    };
    let requirements = requirements
        .p_memory_requirements
        .unwrap()
        .memory_requirements;
    assert_ne!(requirements.memory_type_bits & (1 << memory_type), 0);
    harness
        .send(&h::allocate(
            h::DEVICE,
            BUFFER_MEMORY,
            requirements.size,
            memory_type,
            Vec::new(),
        ))
        .unwrap();
    harness
        .send(&h::bind_buffers(h::DEVICE, &[(BUFFER, BUFFER_MEMORY, 0)]))
        .unwrap();
    let mut image = h::image_info();
    image.extent.width = 513;
    image.extent.height = 513;
    image.usage = 0x13;
    harness
        .send(&h::create_image(h::DEVICE, IMAGE, image))
        .unwrap();
    let Command::GetImageMemoryRequirements2(requirements) = harness
        .call(&h::memory_requirements(h::DEVICE, IMAGE))
        .unwrap()
    else {
        panic!()
    };
    let requirements = requirements
        .p_memory_requirements
        .unwrap()
        .memory_requirements;
    assert_ne!(requirements.memory_type_bits & (1 << memory_type), 0);
    harness
        .send(&h::allocate(
            h::DEVICE,
            IMAGE_MEMORY,
            requirements.size,
            memory_type,
            Vec::new(),
        ))
        .unwrap();
    harness
        .send(&h::bind_image(h::DEVICE, IMAGE, IMAGE_MEMORY, 0))
        .unwrap();
    for command in [
        r::allocate_cbs(h::DEVICE, h::POOL, &[PRIVATE_CB], false),
        r::begin(PRIVATE_CB),
        r::fill(PRIVATE_CB, BUFFER, 0, SIZE, 0xaabb_ccdd),
        transition(0, 7),
        Command::CmdClearColorImage(CmdClearColorImageArgs {
            command_buffer: VkCommandBuffer(PRIVATE_CB),
            image: VkImage(IMAGE),
            image_layout: 7,
            p_color: Some(VkClearColorValue::Float32([1.0, 0.0, 1.0, 1.0])),
            range_count: 1,
            p_ranges: Some(vec![VkImageSubresourceRange {
                aspect_mask: 1,
                base_mip_level: 0,
                level_count: 1,
                base_array_layer: 0,
                layer_count: 1,
            }]),
        }),
        transition(7, 6),
        r::end(PRIVATE_CB),
        r::create_fence(h::DEVICE, PRIVATE_FENCE, false),
        r::queue_submit(h::QUEUE, &[PRIVATE_CB], PRIVATE_FENCE),
        r::wait_fences(h::DEVICE, &[PRIVATE_FENCE], 2_000_000_000),
    ] {
        harness.call(&command).unwrap();
    }
}

pub(super) fn save(harness: &h::Harness<AshVulkan>, directory: &Path) {
    for ctx in [h::CTX, 2] {
        harness
            .renderer
            .factory()
            .with_context_mut(ctx, |context| {
                let mut buffers =
                    std::fs::File::create(directory.join(format!("vm-private-buffers-{ctx}")))
                        .unwrap();
                context.save_device_buffers(&mut buffers, WAIT).unwrap();
                let mut images =
                    std::fs::File::create(directory.join(format!("vm-private-images-{ctx}")))
                        .unwrap();
                context.save_device_images(&mut images, WAIT).unwrap();
            })
            .unwrap();
    }
    let buffers = std::fs::read(directory.join(format!("vm-private-buffers-{}", h::CTX))).unwrap();
    assert_eq!(buffers.len(), 48 + SIZE as usize);
    assert!(buffers[48..]
        .chunks_exact(4)
        .all(|word| word == [0xdd, 0xcc, 0xbb, 0xaa]));
    let images = std::fs::read(directory.join(format!("vm-private-images-{}", h::CTX))).unwrap();
    assert_eq!(images.len(), 92 + 513 * 513 * 4);
    assert!(images[92..]
        .chunks_exact(4)
        .all(|pixel| pixel == [255, 0, 255, 255]));
}

pub(super) fn restore(
    directory: &Path,
    ctx: u32,
    context: &mut VulkanContext<AshVulkan>,
) -> Result<(), crate::venus::executor::checkpoint::RecipeError> {
    context.restore_device_buffers_reader(
        &mut std::fs::File::open(directory.join(format!("vm-private-buffers-{ctx}")))?,
        WAIT,
    )?;
    context.restore_device_images_reader(
        &mut std::fs::File::open(directory.join(format!("vm-private-images-{ctx}")))?,
        WAIT,
    )
}

pub(super) fn verify(
    directory: &Path,
    renderer: &crate::venus::renderer::VenusRenderer<
        crate::venus::executor::ExecutorFactory<AshVulkan>,
    >,
) {
    renderer
        .factory()
        .with_context_mut(h::CTX, |context| {
            let mut buffers = Vec::new();
            context.save_device_buffers(&mut buffers, WAIT).unwrap();
            let expected =
                std::fs::read(directory.join(format!("vm-private-buffers-{}", h::CTX))).unwrap();
            assert!(
                buffers == expected,
                "restored private buffer differs at {:?}",
                buffers.iter().zip(&expected).position(|(a, b)| a != b)
            );
            let mut images = Vec::new();
            context.save_device_images(&mut images, WAIT).unwrap();
            let expected =
                std::fs::read(directory.join(format!("vm-private-images-{}", h::CTX))).unwrap();
            assert!(
                images == expected,
                "restored private image differs at {:?}",
                images.iter().zip(&expected).position(|(a, b)| a != b)
            );
        })
        .unwrap();
}
