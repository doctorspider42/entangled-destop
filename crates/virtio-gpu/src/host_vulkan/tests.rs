//! The executor against the host's real GPU — the RTX 2070 on the Windows
//! development machine. Every test self-skips, with a line saying why, on a
//! host with no Vulkan loader or no device the executor would expose (Linux
//! CI, the WSL side of this machine where the only ICD is lavapipe).

use std::sync::Arc;

use ash::vk;

use super::AshVulkan;
use crate::renderer::Renderer3d;
use crate::venus::executor::harness::*;
use crate::venus::executor::policy::{c_name, MEMORY_PROPERTY_HOST_ANY};
use crate::venus::protocol::*;

const CHECKPOINT_STAGE: &str = "ENTANGLED_TEST_CHECKPOINT_STAGE";
const CHECKPOINT_DIRECTORY: &str = "ENTANGLED_TEST_CHECKPOINT_DIRECTORY";

/// Both halves run in separate processes. Only the on-disk fixture can carry
/// state between them; no Vulkan device, imported mapping or Arc survives.
#[test]
fn checkpoint_buffer_survives_the_saving_process_exiting() {
    const NAME: &str = "host_vulkan::tests::checkpoint_recreates_a_device_and_the_gpu_reads_the_restored_host_memory";
    struct Fixture(std::path::PathBuf);
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let fixture = Fixture(std::env::temp_dir().join(format!(
        "entangled-checkpoint-{}-{nonce}",
        std::process::id()
    )));
    std::fs::create_dir(&fixture.0).unwrap();
    for stage in ["save", "restore"] {
        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .args([NAME, "--exact", "--nocapture", "--test-threads", "1"])
            .env(CHECKPOINT_STAGE, stage)
            .env(CHECKPOINT_DIRECTORY, &fixture.0)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(90);
        let status = loop {
            if let Some(status) = child.try_wait().unwrap() {
                break Some(status);
            }
            if std::time::Instant::now() >= deadline {
                let _ = child.kill();
                break None;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        };
        let output = child.wait_with_output().unwrap();
        let text = format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            status.is_some_and(|status| status.success()),
            "{stage}: {text}"
        );
        if stage == "save" && !fixture.0.join("recipes").exists() {
            assert!(
                text.lines().any(|line| line.starts_with("skipping:")),
                "{text}"
            );
            eprintln!("skipping: the checkpoint child has no usable host GPU");
            return;
        }
        eprintln!("checkpoint {stage} finished in a separate process");
    }
    assert!(fixture.0.join("restored-ok").exists());
}

#[test]
fn checkpoint_recreates_a_device_and_the_gpu_reads_the_restored_host_memory() {
    use crate::venus::executor::recording::*;
    use crate::venus::executor::{checkpoint::MAX_RECIPE_BYTES, VulkanContext};
    use crate::venus::shmem::PageBudget;

    let Some(host) = host() else { return };
    let guest = host.usable_devices().unwrap().remove(0);
    let budget = PageBudget::new(MAX_RECIPE_BYTES);
    let run = |ctx: &mut VulkanContext<AshVulkan>, mut command: Command<'_>| {
        if crate::venus::executor::submit::is_wait(&command) {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
            while !ctx
                .execute_wait(&mut command, std::time::Duration::from_millis(10))
                .expect("wait for checkpoint fixture GPU work")
            {
                assert!(
                    std::time::Instant::now() < deadline,
                    "checkpoint fixture wait timed out"
                );
            }
        } else {
            ctx.execute(&mut command)
                .expect("execute checkpoint fixture");
        }
        assert!(
            crate::venus::executor::context::result_of(&command).is_none_or(|r| r == VK_SUCCESS)
        );
    };
    let stage = std::env::var(CHECKPOINT_STAGE).ok();
    let directory = std::env::var_os(CHECKPOINT_DIRECTORY).map(std::path::PathBuf::from);
    const SIZE: u64 = 64 << 10;
    const VRAM_BUFFER: u64 = BUFFER + 10;
    const VRAM_MEMORY: u64 = MEMORY + 10;
    const VRAM_SIZE: u64 = (2 << 20) + 64;
    let (recipes, memory, sync, vram, images, allocation_size, memory_type) = if stage.as_deref()
        == Some("restore")
    {
        let directory = directory.as_ref().unwrap();
        let metadata = std::fs::read(directory.join("metadata")).unwrap();
        assert_eq!(metadata.len(), 12);
        (
            std::fs::read(directory.join("recipes")).unwrap(),
            std::fs::read(directory.join("memory")).unwrap(),
            std::fs::read(directory.join("sync")).unwrap(),
            std::fs::read(directory.join("vram")).unwrap(),
            std::fs::read(directory.join("images")).unwrap(),
            u64::from_le_bytes(metadata[..8].try_into().unwrap()),
            u32::from_le_bytes(metadata[8..].try_into().unwrap()),
        )
    } else {
        let mut original = VulkanContext::new(1, Arc::clone(&host));
        original.enable_object_recipes(&budget).unwrap();
        run(&mut original, create_instance(INSTANCE));
        run(&mut original, enumerate(INSTANCE, Some(vec![PHYSICAL])));
        let mut create = create_device(
            PHYSICAL,
            DEVICE,
            vec![
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
        );
        let two_queues = guest
            .queue_families
            .first()
            .is_some_and(|family| family.queue_count >= 2);
        if two_queues {
            let Command::CreateDevice(args) = &mut create else {
                unreachable!()
            };
            let queue = &mut args
                .p_create_info
                .as_mut()
                .unwrap()
                .p_queue_create_infos
                .as_mut()
                .unwrap()[0];
            queue.queue_count = 2;
            queue.p_queue_priorities = Some(vec![1.0, 1.0]);
        }
        run(&mut original, create);
        run(&mut original, device_queue(DEVICE, QUEUE, 1));
        if two_queues {
            let mut second = device_queue(DEVICE, QUEUE + 1, 2);
            let Command::GetDeviceQueue2(args) = &mut second else {
                unreachable!()
            };
            args.p_queue_info.as_mut().unwrap().queue_index = 1;
            run(&mut original, second);
            eprintln!("checkpoint fixture uses two fetched queues in family zero");
        }

        run(&mut original, create_pool(DEVICE, POOL));
        run(
            &mut original,
            create_buffer(DEVICE, BUFFER, buffer_info(SIZE, TRANSFER)),
        );
        let mut requirements = buffer_requirements(DEVICE, BUFFER);
        original.execute(&mut requirements).unwrap();
        let Command::GetBufferMemoryRequirements2(requirements) = requirements else {
            panic!()
        };
        let requirements = requirements
            .p_memory_requirements
            .unwrap()
            .memory_requirements;
        let types = requirements.memory_type_bits & guest.host_visible_types();
        assert_ne!(types, 0);
        let memory_type = types.trailing_zeros();
        run(
            &mut original,
            allocate(DEVICE, MEMORY, requirements.size, memory_type, Vec::new()),
        );
        run(&mut original, bind_buffers(DEVICE, &[(BUFFER, MEMORY, 0)]));
        original
            .with_host_buffer(DEVICE, BUFFER, |h, device, queue, family, buffer| {
                h.fill_buffer(device, (queue, family), buffer, 0, SIZE, 0x1357_2468)
            })
            .unwrap()
            .unwrap();
        run(
            &mut original,
            create_buffer(DEVICE, VRAM_BUFFER, buffer_info(VRAM_SIZE, TRANSFER)),
        );
        let mut local_req = buffer_requirements(DEVICE, VRAM_BUFFER);
        original.execute(&mut local_req).unwrap();
        let Command::GetBufferMemoryRequirements2(local_req) = local_req else {
            panic!()
        };
        let local_req = local_req.p_memory_requirements.unwrap().memory_requirements;
        let private_type = guest
            .memory
            .memory_types
            .iter()
            .enumerate()
            .take(guest.memory.memory_type_count as usize)
            .find(|(index, ty)| {
                ty.property_flags & 3 == 1 && local_req.memory_type_bits & (1 << index) != 0
            })
            .map(|(index, _)| index as u32);
        if let Some(index) = private_type {
            eprintln!("checkpoint VRAM fixture uses private device-local memory type {index}");
            run(
                &mut original,
                allocate(DEVICE, VRAM_MEMORY, local_req.size, index, Vec::new()),
            );
            run(
                &mut original,
                bind_buffers(DEVICE, &[(VRAM_BUFFER, VRAM_MEMORY, 0)]),
            );
            if two_queues {
                const COPY_CB: u64 = CB + 99;
                run(&mut original, allocate_cbs(DEVICE, POOL, &[COPY_CB], false));
                run(&mut original, begin(COPY_CB));
                run(
                    &mut original,
                    fill(COPY_CB, VRAM_BUFFER, 0, VRAM_SIZE, 0x2468_1357),
                );
                run(&mut original, end(COPY_CB));
                run(&mut original, queue_submit(QUEUE + 1, &[COPY_CB], 0));
            } else {
                original
                    .with_host_buffer(DEVICE, VRAM_BUFFER, |h, device, queue, family, buffer| {
                        h.fill_buffer(device, (queue, family), buffer, 0, VRAM_SIZE, 0x2468_1357)
                    })
                    .unwrap()
                    .unwrap();
            }
        } else {
            eprintln!("skipping private VRAM subfixture: this GPU has no compatible private device-local type");
        }
        const PIXELS: u64 = IMAGE + 20;
        let mut image_info = image_info();
        image_info.flags = 0x108; // MUTABLE_FORMAT | EXTENDED_USAGE
        image_info.extent.width = 513;
        image_info.extent.height = 513;
        image_info.usage = 7;
        run(&mut original, create_image(DEVICE, PIXELS, image_info));
        let mut image_req = memory_requirements(DEVICE, PIXELS);
        original.execute(&mut image_req).unwrap();
        let Command::GetImageMemoryRequirements2(image_req) = image_req else {
            panic!()
        };
        let image_req = image_req.p_memory_requirements.unwrap().memory_requirements;
        let image_type = guest
            .memory
            .memory_types
            .iter()
            .enumerate()
            .take(guest.memory.memory_type_count as usize)
            .find(|(index, ty)| {
                ty.property_flags & 3 == 1 && image_req.memory_type_bits & (1 << index) != 0
            })
            .map(|(index, _)| index as u32);
        if let Some(index) = image_type {
            run(
                &mut original,
                allocate(DEVICE, MEMORY + 20, image_req.size, index, Vec::new()),
            );
            run(&mut original, bind_image(DEVICE, PIXELS, MEMORY + 20, 0));
            run(&mut original, allocate_cbs(DEVICE, POOL, &[CB + 20], false));
            run(&mut original, begin(CB + 20));
            run(
                &mut original,
                image_barrier2(CB + 20, PIXELS, (0, 7), (0, 0), (0x1000, 0x1000)),
            );
            run(
                &mut original,
                Command::CmdClearColorImage(CmdClearColorImageArgs {
                    command_buffer: VkCommandBuffer(CB + 20),
                    image: VkImage(PIXELS),
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
            );
            run(
                &mut original,
                image_barrier2(CB + 20, PIXELS, (7, 5), (0x1000, 0x1000), (0x1_0000, 0x20)),
            );
            run(&mut original, end(CB + 20));
            run(&mut original, create_fence(DEVICE, FENCE + 20, false));
            run(&mut original, queue_submit(QUEUE, &[CB + 20], FENCE + 20));
            run(
                &mut original,
                wait_fences(DEVICE, &[FENCE + 20], 2_000_000_000),
            );
        } else {
            eprintln!("skipping private image subfixture: no compatible private device-local type");
        }
        const DEPTH_IMAGE: u64 = IMAGE + 21;
        let mut depth_info = crate::venus::executor::harness::image_info();
        depth_info.format = 130; // D32_SFLOAT_S8_UINT
        depth_info.extent.width = 513;
        depth_info.extent.height = 513;
        depth_info.usage = 0x23;
        let mut query = Command::GetPhysicalDeviceImageFormatProperties2(
            GetPhysicalDeviceImageFormatProperties2Args {
                physical_device: VkPhysicalDevice(PHYSICAL),
                p_image_format_info: Some(VkPhysicalDeviceImageFormatInfo2 {
                    format: 130,
                    type_: 1,
                    tiling: 0,
                    usage: 0x23,
                    flags: 0,
                    p_next: Vec::new(),
                }),
                p_image_format_properties: Some(VkImageFormatProperties2::default()),
                ret: VK_ERROR_UNKNOWN,
            },
        );
        original.execute(&mut query).unwrap();
        let depth_type = if crate::venus::executor::context::result_of(&query) == Some(VK_SUCCESS) {
            run(&mut original, create_image(DEVICE, DEPTH_IMAGE, depth_info));
            let mut depth_req = memory_requirements(DEVICE, DEPTH_IMAGE);
            original.execute(&mut depth_req).unwrap();
            let Command::GetImageMemoryRequirements2(depth_req) = depth_req else {
                panic!()
            };
            let depth_req = depth_req.p_memory_requirements.unwrap().memory_requirements;
            let depth_type = guest
                .memory
                .memory_types
                .iter()
                .enumerate()
                .take(guest.memory.memory_type_count as usize)
                .find(|(index, ty)| {
                    ty.property_flags & 3 == 1 && depth_req.memory_type_bits & (1 << index) != 0
                })
                .map(|(index, _)| index as u32);
            if let Some(index) = depth_type {
                run(
                    &mut original,
                    allocate(DEVICE, MEMORY + 21, depth_req.size, index, Vec::new()),
                );
                run(
                    &mut original,
                    bind_image(DEVICE, DEPTH_IMAGE, MEMORY + 21, 0),
                );
                run(&mut original, allocate_cbs(DEVICE, POOL, &[CB + 21], false));
                run(&mut original, begin(CB + 21));
                let depth_barrier = |layouts, stages, access| {
                    let mut command = image_barrier2(CB + 21, DEPTH_IMAGE, layouts, stages, access);
                    let Command::CmdPipelineBarrier2(args) = &mut command else {
                        unreachable!()
                    };
                    args.p_dependency_info
                        .as_mut()
                        .unwrap()
                        .p_image_memory_barriers
                        .as_mut()
                        .unwrap()[0]
                        .subresource_range
                        .aspect_mask = 6;
                    command
                };
                run(
                    &mut original,
                    depth_barrier((0, 7), (0, 0), (0x1000, 0x1000)),
                );
                run(
                    &mut original,
                    Command::CmdClearDepthStencilImage(CmdClearDepthStencilImageArgs {
                        command_buffer: VkCommandBuffer(CB + 21),
                        image: VkImage(DEPTH_IMAGE),
                        image_layout: 7,
                        p_depth_stencil: Some(VkClearDepthStencilValue {
                            depth: 0.375,
                            stencil: 0x5a,
                        }),
                        range_count: 1,
                        p_ranges: Some(vec![VkImageSubresourceRange {
                            aspect_mask: 6,
                            base_mip_level: 0,
                            level_count: 1,
                            base_array_layer: 0,
                            layer_count: 1,
                        }]),
                    }),
                );
                run(
                    &mut original,
                    depth_barrier((7, 3), (0x1000, 0x1000), (0x1_0000, 0x1_8000)),
                );
                run(&mut original, end(CB + 21));
                run(&mut original, create_fence(DEVICE, FENCE + 21, false));
                run(&mut original, queue_submit(QUEUE, &[CB + 21], FENCE + 21));
                run(
                    &mut original,
                    wait_fences(DEVICE, &[FENCE + 21], 2_000_000_000),
                );
                eprintln!(
                    "checkpoint native depth/stencil fixture covers 513x513 pixels in both aspects"
                );
            }
            depth_type
        } else {
            eprintln!("skipping depth/stencil subfixture: D32_SFLOAT_S8_UINT transfer/attachment unsupported");
            None
        };
        crate::venus::executor::checkpoint_sync::tests::populate(&mut original);
        let sync = original
            .save_sync_state(std::time::Duration::from_secs(5))
            .unwrap();
        let recipes = original.save_object_recipes().unwrap();
        let mut memory = Vec::new();
        original.save_host_memory(&mut memory).unwrap();
        let mut vram = Vec::new();
        original
            .save_device_buffers(&mut vram, std::time::Duration::from_secs(5))
            .unwrap();
        if private_type.is_some() {
            let mut check = crate::venus::wire::Decoder::new(&vram);
            assert_eq!(check.u32().unwrap(), 1);
            assert_eq!(check.u32().unwrap(), 1);
            assert_eq!(check.u64().unwrap(), DEVICE);
            assert_eq!(check.u64().unwrap(), VRAM_BUFFER);
            assert_eq!(check.u64().unwrap(), VRAM_SIZE);
            assert_eq!(check.u64().unwrap(), VRAM_MEMORY);
            assert_eq!(check.u64().unwrap(), 0);
            assert!(check
                .blob(VRAM_SIZE as usize)
                .unwrap()
                .chunks_exact(4)
                .all(|word| word == 0x2468_1357u32.to_le_bytes()));
            assert_eq!(check.remaining(), 0);
        }
        let mut images = Vec::new();
        original
            .save_device_images(&mut images, std::time::Duration::from_secs(5))
            .unwrap();
        {
            let mut check = crate::venus::wire::Decoder::new(&images);
            assert_eq!(check.u32().unwrap(), 2);
            let count = check.u32().unwrap();
            assert_eq!(
                count,
                u32::from(image_type.is_some()) + 2 * u32::from(depth_type.is_some())
            );
            for _ in 0..count {
                assert_eq!(check.u64().unwrap(), DEVICE);
                let image = check.u64().unwrap();
                assert_eq!(
                    check.u64().unwrap(),
                    if image == PIXELS {
                        MEMORY + 20
                    } else {
                        MEMORY + 21
                    }
                );
                assert_eq!(check.u64().unwrap(), 0);
                let format = check.u32().unwrap();
                for expected in [513, 513, 1, 1, 0] {
                    assert_eq!(check.u32().unwrap(), expected);
                }
                let aspect = check.u32().unwrap();
                assert_eq!(check.u32().unwrap(), 0); // queue family
                for expected in [if image == PIXELS { 5 } else { 3 }, 2, 0] {
                    assert_eq!(check.u32().unwrap(), expected);
                }
                let len = check.u64().unwrap();
                assert_eq!(len, 513 * 513 * if aspect == 4 { 1 } else { 4 });
                let data = check.blob(len as usize).unwrap();
                match (format, aspect) {
                    (37, 1) => assert!(data.chunks_exact(4).all(|p| p == [255, 0, 255, 255])),
                    (130, 2) => assert!(data.chunks_exact(4).all(|p| p == 0.375f32.to_le_bytes())),
                    (130, 4) => assert!(data.iter().all(|p| *p == 0x5a)),
                    other => panic!("unexpected image aspect {other:?}"),
                }
            }
            assert_eq!(check.remaining(), 0);
        }
        original.destroy_all();
        drop(original);
        assert_eq!(budget.used(), 0);
        if stage.as_deref() == Some("save") {
            let directory = directory.as_ref().unwrap();
            std::fs::write(directory.join("recipes"), &recipes).unwrap();
            std::fs::write(directory.join("memory"), &memory).unwrap();
            std::fs::write(directory.join("sync"), &sync).unwrap();
            std::fs::write(directory.join("vram"), &vram).unwrap();
            std::fs::write(directory.join("images"), &images).unwrap();
            let mut metadata = requirements.size.to_le_bytes().to_vec();
            metadata.extend_from_slice(&memory_type.to_le_bytes());
            std::fs::write(directory.join("metadata"), metadata).unwrap();
            return;
        }
        (
            recipes,
            memory,
            sync,
            vram,
            images,
            requirements.size,
            memory_type,
        )
    };

    let mut restored = VulkanContext::new(1, Arc::clone(&host));
    restored.restore_object_recipes(&recipes, &budget).unwrap();
    restored.restore_host_memory(&memory).unwrap();
    if stage.as_deref() == Some("restore") {
        restored
            .restore_device_buffers_reader(
                &mut std::fs::File::open(directory.as_ref().unwrap().join("vram")).unwrap(),
                std::time::Duration::from_secs(5),
            )
            .unwrap();
    } else {
        restored
            .restore_device_buffers(&vram, std::time::Duration::from_secs(5))
            .unwrap();
    }
    let mut copied_back = Vec::new();
    restored
        .save_device_buffers(&mut copied_back, std::time::Duration::from_secs(5))
        .unwrap();
    assert_eq!(
        copied_back, vram,
        "a new GPU device reads back every restored VRAM chunk exactly"
    );
    if stage.as_deref() == Some("restore") {
        restored
            .restore_device_images_reader(
                &mut std::fs::File::open(directory.as_ref().unwrap().join("images")).unwrap(),
                std::time::Duration::from_secs(5),
            )
            .unwrap();
    } else {
        restored
            .restore_device_images(&images, std::time::Duration::from_secs(5))
            .unwrap();
    }
    let mut image_copy = Vec::new();
    restored
        .save_device_images(&mut image_copy, std::time::Duration::from_secs(5))
        .unwrap();
    assert_eq!(
        image_copy, images,
        "a new process restores every pixel and its executed layout"
    );
    restored
        .restore_sync_state(&sync, std::time::Duration::from_secs(5))
        .unwrap();
    assert_eq!(
        restored
            .save_sync_state(std::time::Duration::from_secs(5))
            .unwrap(),
        sync
    );
    crate::venus::executor::checkpoint_sync::tests::check_after_restore(&mut restored);
    let output = BUFFER + 1;
    let output_memory = MEMORY + 1;
    run(
        &mut restored,
        create_buffer(DEVICE, output, buffer_info(SIZE, TRANSFER)),
    );
    run(
        &mut restored,
        allocate(
            DEVICE,
            output_memory,
            allocation_size,
            memory_type,
            Vec::new(),
        ),
    );
    run(
        &mut restored,
        bind_buffers(DEVICE, &[(output, output_memory, 0)]),
    );
    run(&mut restored, allocate_cbs(DEVICE, POOL, &[CB], false));
    run(&mut restored, begin(CB));
    run(
        &mut restored,
        buffer_barrier(
            CB,
            BUFFER,
            (STAGE_HOST, 0x4000),
            (STAGE_TRANSFER, ACCESS_TRANSFER_READ),
        ),
    );
    run(
        &mut restored,
        copy_buffer(CB, BUFFER, output, &[(0, 0, SIZE)]),
    );
    run(
        &mut restored,
        buffer_barrier(
            CB,
            output,
            (STAGE_TRANSFER, ACCESS_TRANSFER_WRITE),
            (STAGE_HOST, ACCESS_HOST_READ),
        ),
    );
    run(&mut restored, end(CB));
    run(&mut restored, create_fence(DEVICE, FENCE, false));
    run(&mut restored, queue_submit(QUEUE, &[CB], FENCE));
    run(&mut restored, wait_fences(DEVICE, &[FENCE], 2_000_000_000));
    let mut saved = Vec::new();
    restored.save_host_memory(&mut saved).unwrap();
    let mut decoder = crate::venus::wire::Decoder::new(&saved);
    assert_eq!(decoder.u32().unwrap(), 1);
    let count = decoder.u32().unwrap();
    let mut found = false;
    for _ in 0..count {
        let id = decoder.u64().unwrap();
        let len = decoder.u64().unwrap() as usize;
        let data = decoder.blob(len).unwrap();
        if id == output_memory {
            assert!(data[..SIZE as usize]
                .chunks_exact(4)
                .all(|w| w == 0x1357_2468u32.to_le_bytes()));
            found = true;
        }
    }
    assert!(found, "the GPU's destination allocation was checkpointed");
    restored.destroy_all();
    if stage.as_deref() == Some("restore") {
        std::fs::write(directory.as_ref().unwrap().join("restored-ok"), []).unwrap();
    }
}

/// The host, if it has a device the executor would expose.
pub(super) fn host() -> Option<Arc<AshVulkan>> {
    let host = match AshVulkan::load() {
        Ok(host) => host,
        Err(why) => {
            eprintln!("skipping: {why}");
            return None;
        }
    };
    match host.usable_devices() {
        Ok(devices) => {
            for device in &devices {
                eprintln!("host device the guest would see: {}", device.name());
            }
            Some(Arc::new(host))
        }
        Err(why) => {
            eprintln!("skipping: {why}");
            None
        }
    }
}

/// What `ash` says the first non-CPU device is, asked directly.
fn first_gpu_directly() -> (String, u32, u32) {
    // SAFETY: test-only direct use of the loader, mirroring what the
    // renderer does: a local instance, enumerated and destroyed here.
    unsafe {
        let entry = ash::Entry::load().expect("the loader loaded a moment ago");
        let app = vk::ApplicationInfo::default().api_version(vk::API_VERSION_1_3);
        let info = vk::InstanceCreateInfo::default().application_info(&app);
        let instance = entry.create_instance(&info, None).expect("an instance");
        let mut found = None;
        for device in instance.enumerate_physical_devices().expect("devices") {
            let props = instance.get_physical_device_properties(device);
            if props.device_type != vk::PhysicalDeviceType::CPU && found.is_none() {
                let name = props
                    .device_name_as_c_str()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default();
                found = Some((name, props.vendor_id, props.device_id));
            }
        }
        instance.destroy_instance(None);
        found.expect("a GPU")
    }
}

#[test]
fn the_bring_up_runs_on_the_host_gpu_and_names_it_as_ash_does() {
    let Some(host) = host() else { return };
    let mut h = Harness::new(Arc::clone(&host));

    let Command::EnumerateInstanceVersion(v) = h.call(&enumerate_instance_version()).unwrap()
    else {
        panic!()
    };
    assert_eq!(v.ret, VK_SUCCESS);
    let (_, major, minor, _) = crate::venus::capset::vk_api_version_parts(v.p_api_version.unwrap());
    assert!((major, minor) >= (1, 1) && (major, minor) <= (1, 3));

    let Command::CreateInstance(i) = h.call(&create_instance(INSTANCE)).unwrap() else {
        panic!()
    };
    assert_eq!(i.ret, VK_SUCCESS);
    let Command::EnumeratePhysicalDevices(e) = h.call(&enumerate(INSTANCE, None)).unwrap() else {
        panic!()
    };
    let count = e.p_physical_device_count.unwrap();
    assert!(count >= 1);
    let ids: Vec<u64> = (0..u64::from(count)).map(|i| PHYSICAL + i).collect();
    let Command::EnumeratePhysicalDevices(e) = h.call(&enumerate(INSTANCE, Some(ids))).unwrap()
    else {
        panic!()
    };
    assert_eq!(e.ret, VK_SUCCESS);

    let Command::GetPhysicalDeviceProperties(p) = h.call(&properties(PHYSICAL)).unwrap() else {
        panic!()
    };
    let p = p.p_properties.unwrap();
    let (name, vendor, device) = first_gpu_directly();
    assert_eq!(String::from_utf8_lossy(c_name(&p.device_name)), name);
    // Stage 5c: an NVIDIA GPU is shown with the virtio vendor
    // (`policy::shape_identity`); every other vendor as it is.
    let shown = if vendor == crate::venus::executor::policy::NVIDIA_VENDOR_ID {
        crate::venus::executor::policy::VIRTIO_PCI_VENDOR_ID
    } else {
        vendor
    };
    assert_eq!((p.vendor_id, p.device_id), (shown, device));
    let (_, major, minor, _) = crate::venus::capset::vk_api_version_parts(p.api_version);
    assert!((major, minor) <= (1, 3), "apiVersion is capped at 1.3");

    // The memory table, printed for the record, and the policy checked
    // against what the host itself reports.
    let mem = Command::GetPhysicalDeviceMemoryProperties2(GetPhysicalDeviceMemoryProperties2Args {
        physical_device: VkPhysicalDevice(PHYSICAL),
        p_memory_properties: Some(Default::default()),
    });
    let Command::GetPhysicalDeviceMemoryProperties2(m) = h.call(&mem).unwrap() else {
        panic!()
    };
    let guest = m.p_memory_properties.unwrap().memory_properties;
    let (host_memory, importable) = {
        let shown = host.usable_devices().unwrap();
        let first = &shown[0];
        (first.host_memory.clone(), first.importable)
    };
    eprintln!("{name}: memoryTypeBits importable from host allocations = {importable:#x}");
    eprintln!("type | heap | host flags | guest flags");
    // The host's types, and at most one more: device-local memory the guest
    // may map, our pages (`policy::visible_vram`, ADR-0004 2026-09-26).
    let appended = guest.memory_type_count - host_memory.memory_type_count;
    assert!(appended <= 1, "{appended} types appended");
    for i in 0..host_memory.memory_type_count as usize {
        let (h_ty, g_ty) = (&host_memory.memory_types[i], &guest.memory_types[i]);
        eprintln!(
            "{i:>4} | {:>4} | {:#010x} | {:#010x}",
            h_ty.heap_index, h_ty.property_flags, g_ty.property_flags
        );
        assert_eq!(g_ty.heap_index, h_ty.heap_index, "indices unchanged");
        if importable & (1 << i) != 0 {
            assert_eq!(g_ty.property_flags, h_ty.property_flags);
        } else {
            assert_eq!(
                g_ty.property_flags,
                h_ty.property_flags & !MEMORY_PROPERTY_HOST_ANY
            );
        }
    }
    assert!(crate::venus::executor::policy::has_coherent_host_type(
        &guest
    ));
    let visible_vram = |ty: &crate::venus::protocol::VkMemoryType| ty.property_flags & 0x3 == 0x3;
    let shown = guest.memory_types[..guest.memory_type_count as usize]
        .iter()
        .filter(|ty| visible_vram(ty))
        .count();
    assert!(
        shown >= 1,
        "the guest always has DEVICE_LOCAL | HOST_VISIBLE memory"
    );
    if appended == 1 {
        let last = &guest.memory_types[host_memory.memory_type_count as usize];
        assert_eq!(
            last.property_flags, 0x7,
            "DEVICE_LOCAL | HOST_VISIBLE | HOST_COHERENT"
        );
        assert_eq!(
            last.heap_index, host_memory.memory_heap_count,
            "a heap of its own"
        );
        assert_eq!(guest.memory_heap_count, host_memory.memory_heap_count + 1);
        let heap = &guest.memory_heaps[last.heap_index as usize];
        assert_eq!(heap.flags, 0x1);
        assert!(heap.size <= crate::venus::executor::policy::VISIBLE_VRAM_HEAP_BYTES);
        eprintln!(
            "{:>4} | {:>4} | (appended) | {:#010x}, heap of {} MiB",
            host_memory.memory_type_count,
            last.heap_index,
            last.property_flags,
            heap.size >> 20
        );
    }
    if name.contains("RTX 2070") && guest.memory_type_count >= 6 {
        // The 2026-09-23 probe: 3 and 4 import our pages, 5 (the BAR) cannot.
        assert_eq!(importable & 0x38, 0x18);
        assert_ne!(guest.memory_types[3].property_flags & 0x2, 0);
        assert_ne!(guest.memory_types[4].property_flags & 0x2, 0);
        assert_ne!(host_memory.memory_types[5].property_flags & 0x2, 0);
        assert_eq!(
            guest.memory_types[5].property_flags & MEMORY_PROPERTY_HOST_ANY,
            0
        );
        // With the BAR hidden the card has no DEVICE_LOCAL | HOST_VISIBLE
        // type of its own, so the guest is given one: type 6, 3's pages.
        assert_eq!(guest.memory_type_count, 7);
        assert_eq!(guest.memory_types[6].property_flags, 0x7);
    }

    // Features and properties through their chains, then the rest of the
    // bring-up on the real device.
    let features = Command::GetPhysicalDeviceFeatures2(GetPhysicalDeviceFeatures2Args {
        physical_device: VkPhysicalDevice(PHYSICAL),
        p_features: Some(VkPhysicalDeviceFeatures2 {
            p_next: vec![
                VkPhysicalDeviceFeatures2Next::VkPhysicalDeviceVulkan13Features(Default::default()),
                VkPhysicalDeviceFeatures2Next::VkPhysicalDeviceVulkan12Features(Default::default()),
                VkPhysicalDeviceFeatures2Next::VkPhysicalDeviceVulkan11Features(Default::default()),
            ],
            ..Default::default()
        }),
    });
    let Command::GetPhysicalDeviceFeatures2(f) = h.call(&features).unwrap() else {
        panic!()
    };
    assert_eq!(
        f.p_features.unwrap().features.sparse_binding,
        0,
        "sparse is masked"
    );
    let props2 = Command::GetPhysicalDeviceProperties2(GetPhysicalDeviceProperties2Args {
        physical_device: VkPhysicalDevice(PHYSICAL),
        p_properties: Some(VkPhysicalDeviceProperties2 {
            p_next: vec![
                VkPhysicalDeviceProperties2Next::VkPhysicalDeviceVulkan13Properties(
                    Default::default(),
                ),
                VkPhysicalDeviceProperties2Next::VkPhysicalDeviceVulkan12Properties(
                    Default::default(),
                ),
                VkPhysicalDeviceProperties2Next::VkPhysicalDeviceVulkan11Properties(
                    Default::default(),
                ),
            ],
            ..Default::default()
        }),
    });
    let Command::GetPhysicalDeviceProperties2(p2) = h.call(&props2).unwrap() else {
        panic!()
    };
    let p2 = p2.p_properties.unwrap();
    assert_eq!(c_name(&p2.properties.device_name), name.as_bytes());
    if let [_, VkPhysicalDeviceProperties2Next::VkPhysicalDeviceVulkan12Properties(v12), _] =
        p2.p_next.as_slice()
    {
        eprintln!(
            "driver: {} {}",
            String::from_utf8_lossy(c_name(&v12.driver_name)),
            String::from_utf8_lossy(c_name(&v12.driver_info))
        );
    }

    with_device_on(&mut h);
    let fp = Command::GetPhysicalDeviceFormatProperties2(GetPhysicalDeviceFormatProperties2Args {
        physical_device: VkPhysicalDevice(PHYSICAL),
        format: RGBA8,
        p_format_properties: Some(VkFormatProperties2 {
            p_next: vec![VkFormatProperties2Next::VkFormatProperties3(
                Default::default(),
            )],
            ..Default::default()
        }),
    });
    let Command::GetPhysicalDeviceFormatProperties2(fp) = h.call(&fp).unwrap() else {
        panic!()
    };
    assert_ne!(
        fp.p_format_properties
            .unwrap()
            .format_properties
            .optimal_tiling_features,
        0
    );
    let Command::CreateImage(ci) = h.call(&create_image(DEVICE, IMAGE, image_info())).unwrap()
    else {
        panic!()
    };
    assert_eq!(ci.ret, VK_SUCCESS);
    let Command::GetImageMemoryRequirements2(mr) =
        h.call(&memory_requirements(DEVICE, IMAGE)).unwrap()
    else {
        panic!()
    };
    let bits = mr
        .p_memory_requirements
        .unwrap()
        .memory_requirements
        .memory_type_bits;
    assert_ne!(bits, 0);
    assert_eq!(
        bits >> guest.memory_type_count,
        0,
        "every bit names a type the guest sees"
    );
    h.send(&destroy_image(DEVICE, IMAGE)).unwrap();
    h.send(&Command::DestroyInstance(DestroyInstanceArgs {
        instance: VkInstance(INSTANCE),
    }))
    .unwrap();
    assert_eq!(h.renderer.factory().host_objects(), 0);
    assert!(!h.fatal());
}

/// `with_device` without the enumeration `boot` would repeat.
pub(super) fn with_device_on(h: &mut Harness<AshVulkan>) {
    let Command::CreateDevice(d) = h
        .call(&create_device(PHYSICAL, DEVICE, Vec::new()))
        .unwrap()
    else {
        panic!()
    };
    assert_eq!(d.ret, VK_SUCCESS);
    h.send(&create_pool(DEVICE, POOL)).unwrap();
    let Command::GetDeviceQueue2(q) = h.call(&device_queue(DEVICE, QUEUE, 1)).unwrap() else {
        panic!()
    };
    assert_eq!(q.p_queue, Some(VkQueue(QUEUE)));
}

#[test]
fn a_reset_on_the_real_gpu_leaves_nothing_behind() {
    let Some(host) = host() else { return };
    let mut h = Harness::new(host);
    boot(&mut h);
    with_device_on(&mut h);
    h.call(&create_image(DEVICE, IMAGE, image_info())).unwrap();
    assert!(h.renderer.factory().host_objects() > 0);
    crate::renderer::Renderer3d::reset(&mut h.renderer);
    assert_eq!(h.renderer.factory().host_objects(), 0);
}

/// Stage 5b.1 end to end on the host GPU: a buffer in host-visible memory the
/// executor allocated (our pages, imported), the blob the guest makes of that
/// memory mapped into the window, the **host GPU** filling the buffer, and
/// the pattern read back through the blob's pages — the bytes the guest's
/// mapping shows.
#[test]
fn the_host_gpu_fills_a_buffer_and_the_guest_reads_it_through_the_blob() {
    const SIZE: u64 = 64 << 10;
    const MEM_RES: u32 = 20;
    const MAP_AT: u64 = 0x40_0000;
    const PATTERN: u32 = 0xC0FF_EE11;
    let Some(host) = host() else { return };
    let mut h = Harness::new(Arc::clone(&host));
    boot(&mut h);
    with_device_on(&mut h);

    let Command::GetPhysicalDeviceMemoryProperties2(m) =
        h.call(&memory_properties(PHYSICAL)).unwrap()
    else {
        panic!()
    };
    let memory = m.p_memory_properties.unwrap().memory_properties;
    // Every host-visible coherent type the guest may map (types 3 and 4 on
    // the RTX 2070), each through the whole path; Mesa's feedback buffer
    // takes the first of them (`vn_get_memory_type_index`).
    let coherent = 0x2 | 0x4;
    let candidates: Vec<u32> = (0..memory.memory_type_count)
        .filter(|i| memory.memory_types[*i as usize].property_flags & coherent == coherent)
        .collect();
    assert!(!candidates.is_empty(), "a host-visible coherent type");
    // One of them is device local: type 6 on the RTX 2070, which is type 3's
    // pages (`policy::visible_vram`, ADR-0004 2026-09-26), or a type the
    // host itself makes device local and host visible. The GPU writes it and
    // the guest reads it like any other.
    assert!(
        candidates
            .iter()
            .any(|i| memory.memory_types[*i as usize].property_flags & 0x1 != 0),
        "a DEVICE_LOCAL | HOST_VISIBLE | HOST_COHERENT type among {candidates:?}"
    );
    for (round, type_index) in candidates.into_iter().enumerate() {
        let res = MEM_RES + u32::try_from(round).unwrap();
        let Command::CreateBuffer(b) = h
            .call(&create_buffer(DEVICE, BUFFER, buffer_info(SIZE, TRANSFER)))
            .unwrap()
        else {
            panic!()
        };
        assert_eq!(b.ret, VK_SUCCESS);
        let Command::GetBufferMemoryRequirements2(r) =
            h.call(&buffer_requirements(DEVICE, BUFFER)).unwrap()
        else {
            panic!()
        };
        let req = r.p_memory_requirements.unwrap().memory_requirements;
        eprintln!(
            "type {type_index} (guest flags {:#x}): buffer of {SIZE:#x} needs {:#x}, alignment {:#x}, memoryTypeBits {:#x}",
            memory.memory_types[type_index as usize].property_flags,
            req.size,
            req.alignment,
            req.memory_type_bits
        );
        assert_ne!(
            req.memory_type_bits & (1 << type_index),
            0,
            "a transfer buffer may live in host-visible memory"
        );
        h.send(&allocate(DEVICE, MEMORY, req.size, type_index, Vec::new()))
            .unwrap();
        h.send(&bind_buffers(DEVICE, &[(BUFFER, MEMORY, 0)]))
            .unwrap();
        assert!(!h.fatal(), "the allocation and the bind were accepted");
        assert!(h.renderer.factory().host_visible_bytes() >= req.size);

        let blob_size = req.size.next_multiple_of(4096);
        h.memory_blob(CTX, res, MEMORY, blob_size)
            .expect("a blob of the memory");
        h.renderer
            .map_blob(res, MAP_AT, blob_size)
            .expect("mapped into the window");
        let pages = h.renderer.blob_pages(res).expect("the blob's pages");
        assert_eq!(
            h.window.at(MAP_AT),
            Some((pages.host_addr(), blob_size)),
            "the guest's mapping is the imported pages"
        );
        // The guest writes something the GPU must overwrite.
        pages
            .write_bytes(0, &vec![0xaa; usize::try_from(SIZE).unwrap()])
            .unwrap();

        let pattern = PATTERN ^ type_index;
        let filled = h
            .renderer
            .factory()
            .with_context(CTX, |ctx| {
                ctx.with_host_buffer(DEVICE, BUFFER, |host, device, queue, family, buffer| {
                    host.fill_buffer(device, (queue, family), buffer, 0, SIZE, pattern)
                })
            })
            .flatten()
            .expect("the context, its device, a queue and the buffer");
        filled.expect("the host GPU filled the buffer");

        let mut seen = vec![0u8; usize::try_from(SIZE).unwrap()];
        pages.read_bytes(0, &mut seen).unwrap();
        let words: Vec<u32> = seen
            .chunks_exact(4)
            .map(|w| u32::from_le_bytes([w[0], w[1], w[2], w[3]]))
            .collect();
        let wrong = words.iter().filter(|w| **w != pattern).count();
        eprintln!(
            "type {type_index}: read back {} words through the blob: first {:#010x}, last {:#010x}, {wrong} wrong",
            words.len(),
            words[0],
            words[words.len() - 1]
        );
        assert_eq!(wrong, 0, "every word the GPU wrote is what the guest reads");

        // Teardown as Mesa does it: the bo first, then the memory, then the
        // buffer; nothing of the host is left.
        h.renderer.unmap_blob(res, MAP_AT);
        h.renderer.destroy_blob(res);
        drop(pages);
        h.send(&free(DEVICE, MEMORY)).unwrap();
        h.send(&destroy_buffer(DEVICE, BUFFER)).unwrap();
        assert_eq!(h.renderer.factory().host_visible_bytes(), 0);
    }
    h.send(&Command::DestroyInstance(DestroyInstanceArgs {
        instance: VkInstance(INSTANCE),
    }))
    .unwrap();
    assert_eq!(h.renderer.factory().host_objects(), 0);
    assert!(!h.fatal());
}
