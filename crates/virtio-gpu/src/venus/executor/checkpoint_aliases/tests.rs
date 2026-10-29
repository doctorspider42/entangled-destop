use super::*;
use crate::venus::executor::{fake::FakeVulkan, harness as h};
use crate::venus::protocol::{Command, VK_SUCCESS};
use std::time::Duration;

#[test]
fn interval_sweep_matches_independent_pairwise_overlap_and_keeps_allocations_separate() {
    for seed in 0..2048_u64 {
        let mut groups: HashMap<(u64, usize), Vec<Span>> = HashMap::new();
        let mut input = Vec::new();
        for id in 1..=6_u64 {
            let bits = seed.wrapping_mul(id * 37 + 19).rotate_left(id as u32);
            let key = ((bits >> 9) & 1, ((bits >> 13) & 1) as usize);
            let start = (bits >> 3) & 31;
            let resource = if bits & 2 == 0 {
                Resource::Image(id)
            } else {
                Resource::Buffer(id)
            };
            let linear = matches!(resource, Resource::Buffer(_)) || bits & 8 != 0;
            let span = span(start, (bits & 7) + 1, 128, 4, resource, linear).unwrap();
            groups.entry(key).or_default().push(span);
            input.push((key, span));
        }
        let result = classify(&mut groups, input.len()).unwrap();
        for (i, (key, span)) in input.iter().enumerate() {
            let unsafe_alias = input.iter().enumerate().any(|(j, (other_key, other))| {
                let (start, end, other_start, other_end) = if span.linear == other.linear {
                    (span.start, span.end, other.start, other.end)
                } else {
                    (
                        span.granule_start,
                        span.granule_end,
                        other.granule_start,
                        other.granule_end,
                    )
                };
                i != j
                    && key == other_key
                    && start < other_end
                    && other_start < end
                    && (matches!(span.resource, Resource::Image(_))
                        || matches!(other.resource, Resource::Image(_)))
            });
            let actual = match span.resource {
                Resource::Image(id) => result.image(id),
                Resource::Buffer(id) => result.buffer(id),
            };
            assert_eq!(actual, unsafe_alias, "seed {seed}, span {span:?}");
        }
    }
}

#[test]
fn padding_and_granularity_count_as_overlap_but_adjacent_pages_do_not() {
    let mut groups = HashMap::new();
    groups.insert(
        (1, 1),
        vec![
            span(0, 65, 512, 64, Resource::Image(1), false).unwrap(),
            span(80, 1, 512, 64, Resource::Buffer(2), true).unwrap(),
            span(128, 1, 512, 64, Resource::Buffer(3), true).unwrap(),
            span(192, 64, 512, 64, Resource::Image(4), false).unwrap(),
        ],
    );
    let result = classify(&mut groups, 4).unwrap();
    assert!(result.image(1));
    assert!(result.buffer(2));
    assert!(!result.buffer(3));
    assert!(!result.image(4));
    for (offset, size, allocation, granularity) in [
        (0, 0, 1, 1),
        (u64::MAX, 1, u64::MAX, 1),
        (1, 2, 2, 1),
        (0, u64::MAX, u64::MAX, 2),
    ] {
        assert!(span(
            offset,
            size,
            allocation,
            granularity,
            Resource::Image(1),
            false
        )
        .is_err());
    }
}

#[test]
fn same_linearity_packing_is_preserved_and_linear_optimal_sharing_is_refused() {
    let mut groups = HashMap::new();
    groups.insert(
        (1, 1),
        vec![
            span(0, 8, 64, 64, Resource::Image(1), false).unwrap(),
            span(16, 8, 64, 64, Resource::Image(2), false).unwrap(),
        ],
    );
    groups.insert(
        (2, 1),
        vec![
            span(0, 8, 64, 64, Resource::Image(3), true).unwrap(),
            span(16, 8, 64, 64, Resource::Buffer(4), true).unwrap(),
        ],
    );
    groups.insert(
        (3, 1),
        vec![
            span(0, 8, 64, 64, Resource::Image(5), false).unwrap(),
            span(16, 8, 64, 64, Resource::Image(6), true).unwrap(),
        ],
    );
    let result = classify(&mut groups, 6).unwrap();
    for id in [1, 2, 3] {
        assert!(!result.image(id));
    }
    assert!(!result.buffer(4));
    assert!(result.image(5));
    assert!(result.image(6));
}

fn run(context: &mut VulkanContext<FakeVulkan>, mut command: Command<'_>) {
    context.execute(&mut command).unwrap();
    assert!(crate::venus::executor::context::result_of(&command)
        .is_none_or(|result| result == VK_SUCCESS));
}
fn fixture(buffer_offset: u64) -> (Arc<FakeVulkan>, VulkanContext<FakeVulkan>) {
    let host = Arc::new(FakeVulkan::standard());
    let mut context = VulkanContext::new(1, host.clone());
    run(&mut context, h::create_instance(h::INSTANCE));
    run(
        &mut context,
        h::enumerate(h::INSTANCE, Some(vec![h::PHYSICAL])),
    );
    run(
        &mut context,
        h::create_device(h::PHYSICAL, h::DEVICE, vec![]),
    );
    run(&mut context, h::device_queue(h::DEVICE, h::QUEUE, 1));
    run(&mut context, h::create_pool(h::DEVICE, h::POOL));
    run(
        &mut context,
        h::allocate(h::DEVICE, h::MEMORY, 4 << 20, 1, vec![]),
    );
    let mut image = h::image_info();
    image.usage = 3;
    run(&mut context, h::create_image(h::DEVICE, h::IMAGE, image));
    run(
        &mut context,
        h::bind_image(h::DEVICE, h::IMAGE, h::MEMORY, 0),
    );
    run(
        &mut context,
        h::create_buffer(h::DEVICE, h::BUFFER, h::buffer_info(4096, h::TRANSFER)),
    );
    run(
        &mut context,
        h::bind_buffers(h::DEVICE, &[(h::BUFFER, h::MEMORY, buffer_offset)]),
    );
    (host, context)
}

#[test]
fn live_shared_allocation_is_capturable_only_for_disjoint_native_ranges() {
    for overlap in [false, true] {
        let (host, mut context) = fixture(if overlap { 0 } else { 2 << 20 });
        let aliases = context.checkpoint_binding_aliases().unwrap();
        assert_eq!(aliases.image(h::IMAGE), overlap);
        assert_eq!(aliases.buffer(h::BUFFER), overlap);
        let mut buffers = Vec::new();
        let mut images = Vec::new();
        assert_eq!(
            context
                .save_device_buffers(&mut buffers, Duration::from_secs(2))
                .is_err(),
            overlap
        );
        assert_eq!(
            context
                .save_device_images(&mut images, Duration::from_secs(2))
                .is_err(),
            overlap
        );
        assert!(!context.is_fatal());
        context.destroy_all();
        assert_eq!(host.live_objects(), 0);
    }
}
