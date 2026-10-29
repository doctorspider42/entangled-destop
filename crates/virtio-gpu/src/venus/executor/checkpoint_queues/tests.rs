use super::*;
use crate::venus::executor::{fake::FakeVulkan, harness as h, recording as r};
use crate::venus::shmem::PageBudget;
use std::sync::Arc;
use std::time::Duration;

fn run(context: &mut VulkanContext<FakeVulkan>, mut command: Command<'_>) {
    context.execute(&mut command).unwrap();
    assert!(crate::venus::executor::context::result_of(&command)
        .is_none_or(|result| result == VK_SUCCESS));
}
fn fixture(
    families: Option<Vec<u32>>,
) -> (Arc<FakeVulkan>, VulkanContext<FakeVulkan>, Arc<PageBudget>) {
    fixture_queues(families, false, false)
}

fn fixture_queues(
    families: Option<Vec<u32>>,
    two_same: bool,
    other_family: bool,
) -> (Arc<FakeVulkan>, VulkanContext<FakeVulkan>, Arc<PageBudget>) {
    let mut physical = crate::venus::executor::fake::gpu("checkpoint queue GPU");
    physical
        .info
        .queue_families
        .push(physical.info.queue_families[1].clone());
    let host = Arc::new(FakeVulkan::new(vec![physical]));
    let budget = PageBudget::new(64 << 20);
    let mut context = VulkanContext::new(1, host.clone());
    context.enable_object_recipes(&budget).unwrap();
    run(&mut context, h::create_instance(h::INSTANCE));
    run(
        &mut context,
        h::enumerate(h::INSTANCE, Some(vec![h::PHYSICAL])),
    );
    let mut device = h::create_device(h::PHYSICAL, h::DEVICE, vec![]);
    let Command::CreateDevice(args) = &mut device else {
        unreachable!()
    };
    let queue = &mut args
        .p_create_info
        .as_mut()
        .unwrap()
        .p_queue_create_infos
        .as_mut()
        .unwrap()[0];
    if two_same {
        queue.queue_count = 2;
        queue.p_queue_priorities = Some(vec![1.0, 1.0]);
    }
    if other_family {
        args.p_create_info
            .as_mut()
            .unwrap()
            .p_queue_create_infos
            .as_mut()
            .unwrap()
            .push(VkDeviceQueueCreateInfo {
                queue_family_index: 1,
                queue_count: 1,
                p_queue_priorities: Some(vec![1.0]),
                ..Default::default()
            });
        args.p_create_info.as_mut().unwrap().queue_create_info_count = 2;
    }
    run(&mut context, device);
    run(&mut context, h::device_queue(h::DEVICE, h::QUEUE, 1));
    run(&mut context, h::create_pool(h::DEVICE, h::POOL));
    let mut buffer = h::buffer_info(4096, h::TRANSFER);
    if let Some(families) = families {
        buffer.sharing_mode = 1;
        buffer.queue_family_index_count = families.len() as u32;
        buffer.p_queue_family_indices = Some(families);
    }
    run(&mut context, h::create_buffer(h::DEVICE, h::BUFFER, buffer));
    run(
        &mut context,
        h::allocate(h::DEVICE, h::MEMORY, 64 << 10, 1, vec![]),
    );
    run(
        &mut context,
        h::bind_buffers(h::DEVICE, &[(h::BUFFER, h::MEMORY, 0)]),
    );
    (host, context, budget)
}

#[test]
fn same_family_queues_roundtrip_with_private_semaphore_dependencies() {
    let (host, mut context, budget) = fixture_queues(None, true, false);
    let before = host.submitted().len();
    let error = context
        .save_device_buffers(&mut Vec::new(), Duration::from_secs(2))
        .unwrap_err();
    assert!(error.to_string().contains("all declared queues fetched"));
    assert_eq!(host.submitted().len(), before);
    assert!(!context.is_fatal());
    let mut second = h::device_queue(h::DEVICE, h::QUEUE + 1, 2);
    let Command::GetDeviceQueue2(args) = &mut second else {
        unreachable!()
    };
    args.p_queue_info.as_mut().unwrap().queue_index = 1;
    run(&mut context, second);
    run(&mut context, r::queue_submit(h::QUEUE, &[], 0));
    run(&mut context, r::queue_submit(h::QUEUE + 1, &[], 0));
    let start = host.semaphore_ops().len();
    let mut bytes = Vec::new();
    context
        .save_device_buffers(&mut bytes, Duration::from_secs(2))
        .unwrap();
    let ops = host.semaphore_ops();
    let ops = &ops[start..];
    assert_eq!(ops.len(), 3);
    assert!(ops[0].1.is_empty());
    assert_eq!(ops[0].2.len(), 1);
    assert_eq!(ops[1].1, ops[0].2);
    assert_eq!(ops[1].2.len(), 1);
    assert_eq!(ops[2].1, ops[1].2);
    assert!(ops[2].2.is_empty());
    assert_eq!(ops[0].0, ops[2].0);
    assert_ne!(ops[0].0, ops[1].0);
    assert!(host.unsatisfiable_waits().is_empty());
    assert_eq!(host.live("VkSemaphore"), 0);
    let recipes = context.save_object_recipes().unwrap();
    let mut cold = VulkanContext::new(1, host.clone());
    cold.restore_object_recipes(&recipes, &budget).unwrap();
    cold.restore_device_buffers(&bytes, Duration::from_secs(2))
        .unwrap();
    let mut again = Vec::new();
    cold.save_device_buffers(&mut again, Duration::from_secs(2))
        .unwrap();
    assert_eq!(bytes, again);
    cold.destroy_all();
    assert!(!context.is_fatal());
    context.destroy_all();
    assert_eq!(host.live_objects(), 0);
}

#[test]
fn different_fetched_families_still_refuse_before_copy() {
    let (host, mut context, _) = fixture_queues(None, false, true);
    let mut second = h::device_queue(h::DEVICE, h::QUEUE + 2, 3);
    let Command::GetDeviceQueue2(args) = &mut second else {
        unreachable!()
    };
    args.p_queue_info.as_mut().unwrap().queue_family_index = 1;
    run(&mut context, second);
    let before = host.submitted().len();
    let error = context
        .save_device_buffers(&mut Vec::new(), Duration::from_secs(2))
        .unwrap_err();
    assert!(
        error.to_string().contains("executed queue-family proof"),
        "{error}"
    );
    assert_eq!(host.submitted().len(), before);
    context.destroy_all();
    assert_eq!(host.live_objects(), 0);
}

fn fetch_other_family(context: &mut VulkanContext<FakeVulkan>) {
    let mut second = h::device_queue(h::DEVICE, h::QUEUE + 2, 3);
    let Command::GetDeviceQueue2(args) = &mut second else {
        unreachable!()
    };
    args.p_queue_info.as_mut().unwrap().queue_family_index = 1;
    run(context, second);
}

#[test]
fn executed_buffer_owner_survives_reconstruction_with_another_fetched_family() {
    let (host, mut context, budget) = fixture_queues(None, false, true);
    fetch_other_family(&mut context);
    run(
        &mut context,
        r::allocate_cbs(h::DEVICE, h::POOL, &[r::CB], false),
    );
    run(&mut context, r::begin(r::CB));
    run(&mut context, r::fill(r::CB, h::BUFFER, 0, 4096, 0x12345678));
    run(&mut context, r::end(r::CB));
    assert!(
        context
            .checkpoint_buffer_family(h::DEVICE, h::BUFFER)
            .is_err(),
        "recording is not execution"
    );
    run(&mut context, r::queue_submit(h::QUEUE, &[r::CB], 0));
    assert_eq!(
        context
            .checkpoint_buffer_family(h::DEVICE, h::BUFFER)
            .unwrap(),
        Some(0)
    );
    let owners = context.save_buffer_ownership().unwrap();
    let recipes = context.save_object_recipes().unwrap();
    let mut bytes = Vec::new();
    context
        .save_device_buffers(&mut bytes, Duration::from_secs(2))
        .unwrap();
    let mut cold = VulkanContext::new(1, host.clone());
    cold.restore_object_recipes(&recipes, &budget).unwrap();
    assert!(cold.checkpoint_buffer_family(h::DEVICE, h::BUFFER).is_err());
    let before = cold.save_buffer_ownership().unwrap();
    let calls = host.calls();
    for offset in [0usize, 4, 8, 16, 24, 32, 36] {
        let mut bad = owners.clone();
        bad[offset..offset + 4].copy_from_slice(&u32::MAX.to_le_bytes());
        assert!(
            cold.restore_buffer_ownership(&bad).is_err(),
            "offset {offset}"
        );
        assert_eq!(before, cold.save_buffer_ownership().unwrap());
        assert_eq!(calls, host.calls());
    }
    cold.restore_buffer_ownership(&owners).unwrap();
    assert_eq!(owners, cold.save_buffer_ownership().unwrap());
    cold.restore_device_buffers(&bytes, Duration::from_secs(2))
        .unwrap();
    let mut again = Vec::new();
    cold.save_device_buffers(&mut again, Duration::from_secs(2))
        .unwrap();
    assert_eq!(bytes, again);
    assert!(host.unsatisfiable_waits().is_empty());
    cold.destroy_all();
    context.destroy_all();
    assert_eq!(host.live_objects(), 0);
}

#[test]
fn partial_access_does_not_prove_whole_buffer_ownership() {
    let (host, mut context, _) = fixture_queues(None, false, true);
    fetch_other_family(&mut context);
    run(
        &mut context,
        r::allocate_cbs(h::DEVICE, h::POOL, &[r::CB], false),
    );
    run(&mut context, r::begin(r::CB));
    run(&mut context, r::fill(r::CB, h::BUFFER, 0, 16, 0));
    run(&mut context, r::end(r::CB));
    run(&mut context, r::queue_submit(h::QUEUE, &[r::CB], 0));
    assert!(context
        .checkpoint_buffer_family(h::DEVICE, h::BUFFER)
        .is_err());
    context.destroy_all();
    assert_eq!(host.live_objects(), 0);
}

#[test]
fn opaque_work_in_another_family_cannot_claim_the_buffer_proof() {
    let (host, mut context, _) = fixture_queues(None, false, true);
    fetch_other_family(&mut context);
    run(
        &mut context,
        r::allocate_cbs(h::DEVICE, h::POOL, &[r::CB], false),
    );
    run(&mut context, r::begin(r::CB));
    run(&mut context, r::fill(r::CB, h::BUFFER, 0, 4096, 0));
    run(&mut context, r::end(r::CB));
    run(&mut context, r::queue_submit(h::QUEUE, &[r::CB], 0));
    let mut pool = h::create_pool(h::DEVICE, h::POOL + 1);
    let Command::CreateCommandPool(args) = &mut pool else {
        unreachable!()
    };
    args.p_create_info.as_mut().unwrap().queue_family_index = 1;
    run(&mut context, pool);
    run(
        &mut context,
        r::allocate_cbs(h::DEVICE, h::POOL + 1, &[r::CB + 1], false),
    );
    run(&mut context, r::begin(r::CB + 1));
    run(
        &mut context,
        Command::CmdSetLineWidth(CmdSetLineWidthArgs {
            command_buffer: VkCommandBuffer(r::CB + 1),
            line_width: 1.0,
        }),
    );
    run(&mut context, r::end(r::CB + 1));
    assert!(context
        .checkpoint_buffer_family(h::DEVICE, h::BUFFER)
        .is_ok());
    run(&mut context, r::queue_submit(h::QUEUE + 2, &[r::CB + 1], 0));
    assert!(context
        .checkpoint_buffer_family(h::DEVICE, h::BUFFER)
        .unwrap_err()
        .to_string()
        .contains("opaque"));
    context.destroy_all();
    assert_eq!(host.live_objects(), 0);
}

#[test]
fn a_reused_buffer_id_does_not_inherit_an_old_recordings_owner() {
    let (host, mut context, _) = fixture_queues(None, false, true);
    fetch_other_family(&mut context);
    run(
        &mut context,
        r::allocate_cbs(h::DEVICE, h::POOL, &[r::CB], false),
    );
    run(&mut context, r::begin(r::CB));
    run(&mut context, r::fill(r::CB, h::BUFFER, 0, 4096, 0));
    run(&mut context, r::end(r::CB));
    run(&mut context, h::destroy_buffer(h::DEVICE, h::BUFFER));
    run(
        &mut context,
        h::create_buffer(h::DEVICE, h::BUFFER, h::buffer_info(4096, h::TRANSFER)),
    );
    // Exercise the stale guest input through the fake, never a native driver.
    context
        .buffer_tracking
        .submitted(r::CB, h::DEVICE, 0, &mut context.objects);
    assert_eq!(
        context
            .objects
            .buffer(h::DEVICE, h::BUFFER)
            .unwrap()
            .checkpoint_owner,
        None
    );
    context.destroy_all();
    assert_eq!(host.live_objects(), 0);
}

#[test]
fn a_single_opaque_family_bounds_all_possible_uses_of_an_otherwise_unseen_buffer() {
    let (host, mut context, budget) = fixture_queues(None, false, true);
    fetch_other_family(&mut context);
    assert!(context
        .checkpoint_buffer_family(h::DEVICE, h::BUFFER)
        .is_err());
    run(
        &mut context,
        r::allocate_cbs(h::DEVICE, h::POOL, &[r::CB], false),
    );
    run(&mut context, r::begin(r::CB));
    run(
        &mut context,
        Command::CmdSetLineWidth(CmdSetLineWidthArgs {
            command_buffer: VkCommandBuffer(r::CB),
            line_width: 1.0,
        }),
    );
    run(&mut context, r::end(r::CB));
    run(&mut context, r::queue_submit(h::QUEUE, &[r::CB], 0));
    assert_eq!(
        context
            .objects
            .buffer(h::DEVICE, h::BUFFER)
            .unwrap()
            .checkpoint_owner,
        None
    );
    assert_eq!(
        context
            .checkpoint_buffer_family(h::DEVICE, h::BUFFER)
            .unwrap(),
        Some(0)
    );
    let owners = context.save_buffer_ownership().unwrap();
    let recipes = context.save_object_recipes().unwrap();
    let mut bytes = Vec::new();
    context
        .save_device_buffers(&mut bytes, Duration::from_secs(2))
        .unwrap();
    let mut cold = VulkanContext::new(1, host.clone());
    cold.restore_object_recipes(&recipes, &budget).unwrap();
    cold.restore_buffer_ownership(&owners).unwrap();
    assert_eq!(
        cold.checkpoint_buffer_family(h::DEVICE, h::BUFFER).unwrap(),
        Some(0)
    );
    cold.restore_device_buffers(&bytes, Duration::from_secs(2))
        .unwrap();
    assert_eq!(
        cold.objects
            .buffer(h::DEVICE, h::BUFFER)
            .unwrap()
            .checkpoint_owner,
        Some((0, true))
    );
    cold.destroy_all();
    context.destroy_all();
    assert_eq!(host.live_objects(), 0);
}

#[test]
fn copy_scratch_outlives_a_timed_out_wait_on_the_other_queue() {
    let (host, mut context, _) = fixture_queues(None, true, false);
    let mut second = h::device_queue(h::DEVICE, h::QUEUE + 1, 2);
    let Command::GetDeviceQueue2(args) = &mut second else {
        unreachable!()
    };
    args.p_queue_info.as_mut().unwrap().queue_index = 1;
    run(&mut context, second);
    let other = context.objects.queue(h::QUEUE + 1).unwrap().host;
    host.stick_queue(other);
    assert!(context
        .save_device_buffers(&mut Vec::new(), Duration::from_secs(2))
        .is_err());
    context.poll_clocks(h::DEVICE);
    assert_eq!(
        host.live("VkSemaphore"),
        2,
        "the other queue still holds both dependencies"
    );
    assert_eq!(
        host.live("buffer"),
        2,
        "staging survives the other queue's wait"
    );
    assert!(!context.is_fatal());
    host.release_queue(other);
    context.poll_clocks(h::DEVICE);
    assert_eq!(host.live("VkSemaphore"), 0);
    assert_eq!(host.live("buffer"), 1);
    context
        .save_device_buffers(&mut Vec::new(), Duration::from_secs(2))
        .unwrap();
    context.destroy_all();
    assert_eq!(host.live_objects(), 0);
}

#[test]
fn copy_queue_must_be_in_concurrent_resource_family_set() {
    for allowed in [false, true] {
        let (host, mut context, _) = fixture(Some(if allowed { vec![0, 1] } else { vec![1, 2] }));
        let result = context.save_device_buffers(&mut Vec::new(), Duration::from_secs(2));
        assert_eq!(result.is_ok(), allowed);
        if let Err(error) = result {
            assert!(error.to_string().contains("concurrent sharing"));
        }
        context.destroy_all();
        assert_eq!(host.live_objects(), 0);
    }
    use crate::venus::executor::objects::Sharing;
    assert!(Sharing::new(0, &[]).allows(300));
    assert!(!Sharing::new(1, &[64, 65]).allows(0));
    assert!(!Sharing::new(1, &[0, 64]).allows(0));
    assert!(!Sharing::new(7, &[]).allows(0));
}

#[test]
fn recorded_buffer_family_transfer_refuses_capture_and_survives_recipe_reconstruction() {
    let event = Command::CmdSetEvent2(CmdSetEvent2Args {
        command_buffer: VkCommandBuffer(r::CB),
        event: VkEvent(0),
        p_dependency_info: Some(VkDependencyInfo {
            buffer_memory_barrier_count: 1,
            p_buffer_memory_barriers: Some(vec![VkBufferMemoryBarrier2 {
                src_queue_family_index: 0,
                dst_queue_family_index: 1,
                ..Default::default()
            }]),
            ..Default::default()
        }),
    });
    assert!(buffer_transfer(&event));
    let (host, mut context, budget) = fixture(None);
    run(
        &mut context,
        r::allocate_cbs(h::DEVICE, h::POOL, &[r::CB], false),
    );
    run(&mut context, r::begin(r::CB));
    let mut barrier = r::buffer_barrier(r::CB, h::BUFFER, (0x1000, 0x1000), (0x1000, 0x800));
    let Command::CmdPipelineBarrier(args) = &mut barrier else {
        unreachable!()
    };
    let barrier_info = &mut args.p_buffer_memory_barriers.as_mut().unwrap()[0];
    barrier_info.src_queue_family_index = 0;
    barrier_info.dst_queue_family_index = 1;
    assert!(buffer_transfer(&barrier));
    run(&mut context, barrier);
    run(&mut context, r::end(r::CB));
    assert!(context
        .checkpoint_data_refusal()
        .unwrap()
        .contains("ownership transfers"));
    assert!(context
        .save_device_buffers(&mut Vec::new(), Duration::from_secs(2))
        .is_err());
    let recipes = context.save_object_recipes().unwrap();
    let mut cold = VulkanContext::new(1, host.clone());
    cold.restore_object_recipes(&recipes, &budget).unwrap();
    assert!(cold
        .checkpoint_data_refusal()
        .unwrap()
        .contains("ownership transfers"));
    cold.destroy_all();
    context.destroy_all();
    assert_eq!(host.live_objects(), 0);
}
