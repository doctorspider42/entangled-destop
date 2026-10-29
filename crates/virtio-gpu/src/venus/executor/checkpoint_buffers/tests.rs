use std::sync::atomic::Ordering;

use super::super::checkpoint::MAX_RECIPE_BYTES;
use super::super::fake::FakeVulkan;
use super::super::{harness as h, recording as r};
use super::*;
use crate::venus::shmem::PageBudget;

const SIZE: u64 = 8195;
const WAIT: Duration = Duration::from_secs(2);

#[test]
fn address_capable_allocations_roundtrip_until_a_real_device_address_reaches_the_guest() {
    let budget = PageBudget::new(MAX_RECIPE_BYTES);
    let host = Arc::new(FakeVulkan::standard());
    let mut context = VulkanContext::new(1, host.clone());
    context.enable_object_recipes(&budget).unwrap();
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
            vec![VkDeviceCreateInfoNext::VkPhysicalDeviceVulkan12Features(
                VkPhysicalDeviceVulkan12Features {
                    buffer_device_address: 1,
                    ..Default::default()
                },
            )],
        ),
    );
    run(&mut context, h::device_queue(h::DEVICE, h::QUEUE, 1));
    run(&mut context, h::create_pool(h::DEVICE, h::POOL));
    run(
        &mut context,
        h::create_buffer(
            h::DEVICE,
            h::BUFFER,
            h::buffer_info(
                SIZE,
                h::TRANSFER | super::super::policy::BUFFER_USAGE_DEVICE_ADDRESS,
            ),
        ),
    );
    run(
        &mut context,
        h::allocate(
            h::DEVICE,
            h::MEMORY,
            64 << 10,
            1,
            vec![VkMemoryAllocateInfoNext::VkMemoryAllocateFlagsInfo(
                VkMemoryAllocateFlagsInfo {
                    flags: 2,
                    device_mask: 0,
                },
            )],
        ),
    );
    run(
        &mut context,
        h::bind_buffers(h::DEVICE, &[(h::BUFFER, h::MEMORY, 0)]),
    );
    assert_eq!(context.checkpoint_data_refusal(), None);
    let mut bytes = Vec::new();
    context.save_device_buffers(&mut bytes, WAIT).unwrap();
    let recipes = context.save_object_recipes().unwrap();
    let mut cold = VulkanContext::new(1, host.clone());
    cold.restore_object_recipes(&recipes, &budget).unwrap();
    cold.restore_device_buffers(&bytes, WAIT).unwrap();
    assert_eq!(cold.checkpoint_data_refusal(), None);
    let mut second = Vec::new();
    cold.save_device_buffers(&mut second, WAIT).unwrap();
    assert_eq!(bytes, second);
    cold.destroy_all();

    let mut query = Command::GetBufferDeviceAddress(GetBufferDeviceAddressArgs {
        device: VkDevice(h::DEVICE),
        p_info: Some(VkBufferDeviceAddressInfo {
            buffer: VkBuffer(h::BUFFER),
        }),
        ret: 0,
    });
    context.execute(&mut query).unwrap();
    let Command::GetBufferDeviceAddress(query) = query else {
        unreachable!()
    };
    assert_ne!(query.ret, 0);
    assert!(context
        .checkpoint_data_refusal()
        .unwrap()
        .contains("exposed to the guest"));
    let mut refused = Vec::new();
    assert!(context.save_device_buffers(&mut refused, WAIT).is_err());
    assert!(refused.is_empty());
    run(&mut context, h::destroy_buffer(h::DEVICE, h::BUFFER));
    run(&mut context, h::free(h::DEVICE, h::MEMORY));
    assert!(
        context.checkpoint_data_refusal().is_some(),
        "freeing the buffer cannot erase address copies in guest RAM"
    );
    context.destroy_all();
    assert_eq!(host.live_objects(), 0);
}

#[test]
fn freed_or_reused_memory_ids_never_trigger_a_read_of_dangling_buffers() {
    let budget = PageBudget::new(MAX_RECIPE_BYTES);
    let host = Arc::new(FakeVulkan::standard());
    let mut context = fixture(host.clone(), &budget);
    run(
        &mut context,
        Command::FreeMemory(FreeMemoryArgs {
            device: VkDevice(h::DEVICE),
            memory: VkDeviceMemory(h::MEMORY),
        }),
    );
    for reused in [false, true] {
        if reused {
            run(
                &mut context,
                h::allocate(h::DEVICE, h::MEMORY, 64 << 10, 1, Vec::new()),
            );
        }
        let mut bytes = Vec::new();
        context.save_device_buffers(&mut bytes, WAIT).unwrap();
        assert_eq!(bytes, [1, 0, 0, 0, 0, 0, 0, 0]);
        assert_eq!(host.live("buffer"), 1, "no staging buffer was created");
        assert!(!context.is_fatal());
    }
    context.destroy_all();
    assert_eq!(host.live_objects(), 0);
}

fn run(context: &mut VulkanContext<FakeVulkan>, mut command: Command<'_>) {
    context.execute(&mut command).unwrap();
    assert!(super::super::context::result_of(&command).is_none_or(|ret| ret == VK_SUCCESS));
}

fn fixture(host: Arc<FakeVulkan>, budget: &Arc<PageBudget>) -> VulkanContext<FakeVulkan> {
    fixture_size(host, budget, SIZE)
}

fn fixture_size(
    host: Arc<FakeVulkan>,
    budget: &Arc<PageBudget>,
    size: u64,
) -> VulkanContext<FakeVulkan> {
    let mut context = VulkanContext::new(1, host);
    context.enable_object_recipes(budget).unwrap();
    run(&mut context, h::create_instance(h::INSTANCE));
    run(
        &mut context,
        h::enumerate(h::INSTANCE, Some(vec![h::PHYSICAL])),
    );
    run(
        &mut context,
        h::create_device(h::PHYSICAL, h::DEVICE, Vec::new()),
    );
    run(&mut context, h::device_queue(h::DEVICE, h::QUEUE, 1));
    run(&mut context, h::create_pool(h::DEVICE, h::POOL));
    run(
        &mut context,
        h::create_buffer(h::DEVICE, h::BUFFER, h::buffer_info(size, h::TRANSFER)),
    );
    run(
        &mut context,
        h::allocate(
            h::DEVICE,
            h::MEMORY,
            size.next_multiple_of(64 << 10),
            1,
            Vec::new(),
        ),
    );
    run(
        &mut context,
        h::bind_buffers(h::DEVICE, &[(h::BUFFER, h::MEMORY, 0)]),
    );
    context
}

#[test]
fn malformed_buffer_files_are_rejected_before_the_first_gpu_upload() {
    let budget = PageBudget::new(MAX_RECIPE_BYTES);
    let host = Arc::new(FakeVulkan::standard());
    let mut context = fixture(host.clone(), &budget);
    let mut bytes = Vec::new();
    context.save_device_buffers(&mut bytes, WAIT).unwrap();
    assert_eq!(bytes.len(), 48 + (SIZE as usize).next_multiple_of(4));
    let calls = host.calls();
    let mut bad: Vec<_> = (0..48)
        .chain([bytes.len() / 2, bytes.len() - 1])
        .map(|end| bytes[..end].to_vec())
        .collect();
    for (offset, value) in [(0, 99u32), (4, u32::MAX)] {
        let mut changed = bytes.clone();
        changed[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
        bad.push(changed);
    }
    for offset in [8, 16, 24, 32, 40] {
        let mut changed = bytes.clone();
        changed[offset..offset + 8].copy_from_slice(&u64::MAX.to_le_bytes());
        bad.push(changed);
    }
    let mut trailing = bytes.clone();
    trailing.push(0);
    bad.push(trailing);
    for (index, malformed) in bad.into_iter().enumerate() {
        assert!(
            context.restore_device_buffers(&malformed, WAIT).is_err(),
            "case {index}, len {}",
            malformed.len()
        );
        assert_eq!(host.calls(), calls);
        assert!(!context.is_fatal());
    }
    context.restore_device_buffers(&bytes, WAIT).unwrap();
    assert_eq!(context.budget.used(), 0);
    context.destroy_all();
    assert_eq!(host.live_objects(), 0);
}

#[test]
fn a_timed_out_readback_keeps_staging_and_the_guests_destroyed_source_alive() {
    let budget = PageBudget::new(MAX_RECIPE_BYTES);
    let host = Arc::new(FakeVulkan::standard());
    let mut context = fixture(host.clone(), &budget);
    host.stuck.store(true, Ordering::SeqCst);
    assert!(context.save_device_buffers(&mut Vec::new(), WAIT).is_err());
    assert!(!context.is_fatal());
    assert!(context.budget.used() >= SIZE);
    assert_eq!(host.live("buffer"), 2);
    run(&mut context, h::destroy_buffer(h::DEVICE, h::BUFFER));
    assert_eq!(
        host.live("buffer"),
        2,
        "the GPU still holds both source and staging"
    );
    assert_eq!(context.objects.doomed(), 2);
    host.stuck.store(false, Ordering::SeqCst);
    context.checkpoint_wait_idle(Instant::now() + WAIT).unwrap();
    assert_eq!(host.live("buffer"), 0);
    assert_eq!(context.budget.used(), 0);
    assert_eq!(context.objects.doomed(), 0);
    context.destroy_all();
    assert_eq!(host.live_objects(), 0);
}

#[test]
fn a_disk_write_failure_leaves_the_source_and_the_running_context_usable() {
    struct BrokenDisk(usize);
    impl Write for BrokenDisk {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            if self.0 == 0 {
                return Err(std::io::Error::other("injected disk failure"));
            }
            let take = bytes.len().min(self.0);
            self.0 -= take;
            Ok(take)
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let budget = PageBudget::new(MAX_RECIPE_BYTES);
    let host = Arc::new(FakeVulkan::standard());
    let mut context = fixture(host.clone(), &budget);
    let objects = context.object_count();
    assert!(context
        .save_device_buffers(&mut BrokenDisk(48 + 100), WAIT)
        .is_err());
    assert_eq!(context.object_count(), objects);
    assert!(!context.is_fatal());
    assert_eq!(context.budget.used(), 0);
    assert_eq!(host.live("buffer"), 1);
    run(&mut context, r::queue_submit(h::QUEUE, &[], 0));
    context.save_device_buffers(&mut Vec::new(), WAIT).unwrap();
    context.destroy_all();
    assert_eq!(host.live_objects(), 0);
}

#[test]
fn a_device_lost_during_upload_destroys_the_partial_reconstruction() {
    let budget = PageBudget::new(MAX_RECIPE_BYTES);
    let host = Arc::new(FakeVulkan::standard());
    let mut original = fixture(host, &budget);
    let mut bytes = Vec::new();
    original.save_device_buffers(&mut bytes, WAIT).unwrap();
    let recipes = original.save_object_recipes().unwrap();
    original.destroy_all();
    drop(original);
    let host = Arc::new(FakeVulkan::standard());
    let mut restored = VulkanContext::new(1, host.clone());
    restored.restore_object_recipes(&recipes, &budget).unwrap();
    host.lose_at("vkQueueSubmit");
    assert!(restored.restore_device_buffers(&bytes, WAIT).is_err());
    assert_eq!(restored.object_count(), 0);
    assert_eq!(restored.budget.used(), 0);
    assert_eq!(host.live_objects(), 0);
}

#[test]
fn streamed_buffer_io_failure_discards_the_partial_native_context() {
    use std::io::{Cursor, Read, Seek, SeekFrom};
    struct FailAfterChunk(Cursor<Vec<u8>>);
    impl Read for FailAfterChunk {
        fn read(&mut self, bytes: &mut [u8]) -> std::io::Result<usize> {
            assert!(bytes.len() <= CHUNK_BYTES);
            if self.0.position() >= 48 + CHUNK_BYTES as u64 {
                return Err(std::io::Error::other("simulated VRAM file read failure"));
            }
            self.0.read(bytes)
        }
    }
    impl Seek for FailAfterChunk {
        fn seek(&mut self, at: SeekFrom) -> std::io::Result<u64> {
            self.0.seek(at)
        }
    }
    let budget = PageBudget::new(MAX_RECIPE_BYTES);
    let host = Arc::new(FakeVulkan::standard());
    let mut source = fixture_size(host.clone(), &budget, 2 * CHUNK_BYTES as u64 + 3);
    let mut bytes = Vec::new();
    source.save_device_buffers(&mut bytes, WAIT).unwrap();
    let recipes = source.save_object_recipes().unwrap();
    source.destroy_all();
    let mut cold = VulkanContext::new(1, host.clone());
    cold.restore_object_recipes(&recipes, &budget).unwrap();
    assert!(cold
        .restore_device_buffers_reader(&mut FailAfterChunk(Cursor::new(bytes)), WAIT)
        .is_err());
    assert_eq!(cold.object_count(), 0);
    assert_eq!(host.live_objects(), 0);
    assert_eq!(cold.budget.used(), 0);
}
