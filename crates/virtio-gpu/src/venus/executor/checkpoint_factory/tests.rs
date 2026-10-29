use super::super::checkpoint::MAX_RECIPE_BYTES;
use super::super::{fake::FakeVulkan, harness as h};
use super::*;
use crate::renderer::Renderer3d;

fn captured() -> (
    Arc<FakeVulkan>,
    FactoryObjectCheckpoint,
    Vec<u8>,
    HostPageTable,
    SharedPayloads,
) {
    let host = Arc::new(FakeVulkan::standard());
    let gate = virtio_core::Quiesce::new();
    let recipes = PageBudget::new(2 * MAX_RECIPE_BYTES);
    let mut factory = ExecutorFactory::new(host.clone());
    factory.enable_checkpoint_recipes(&recipes).unwrap();
    let mut source = h::Harness::gated_factory(factory, gate.clone());
    for ctx in [1, 2] {
        source.use_context(ctx);
        source.send(&h::create_instance(h::INSTANCE)).unwrap();
    }
    gate.pause();
    assert!(gate.wait_until_idle(Duration::from_secs(5)));
    let mut budgets = source.renderer.factory().checkpoint_budget_table().unwrap();
    let mut pages = HostPageTable::default();
    let mut payloads = SharedPayloads::default();
    let saved = source
        .renderer
        .factory()
        .checkpoint_objects(
            &mut budgets,
            &mut pages,
            &mut payloads,
            Duration::from_secs(5),
        )
        .unwrap();
    assert_eq!(host.live_objects(), 2, "capture preserves the source");
    let bytes = saved.encode().unwrap();
    assert_eq!(
        FactoryObjectCheckpoint::decode(&bytes)
            .unwrap()
            .encode()
            .unwrap(),
        bytes
    );
    source.renderer.reset();
    drop(source);
    assert_eq!(host.live_objects(), 0);
    (host, saved, budgets.encode().unwrap(), pages, payloads)
}

#[test]
fn whole_factory_objects_use_recorded_owners_and_adopt_without_replaying_gpu_work() {
    use crate::venus::renderer::SinkFactory;
    let (host, saved, bytes, pages, payloads) = captured();
    assert_eq!(saved.context_ids().collect::<Vec<_>>(), [1, 2]);
    let mut factory = ExecutorFactory::new(host.clone());
    let budgets = factory.restore_budget_table(&bytes).unwrap();
    let recipes = PageBudget::new(2 * MAX_RECIPE_BYTES);
    let contexts = factory
        .restore_checkpoint_objects(
            &saved,
            &budgets,
            &pages,
            &payloads,
            &recipes,
            Duration::from_secs(5),
        )
        .unwrap();
    assert_eq!(
        factory.host_objects(),
        0,
        "preparation does not partly adopt contexts"
    );
    assert_eq!(host.live_objects(), 2);
    for context in contexts {
        factory.adopt_checkpoint_context(context).unwrap();
    }
    assert_eq!(factory.host_objects(), 2);
    assert!(factory
        .restore_checkpoint_objects(
            &saved,
            &budgets,
            &pages,
            &payloads,
            &recipes,
            Duration::from_secs(5)
        )
        .is_err());
    for ctx in [1, 2] {
        factory.context_destroyed(ctx);
    }
    assert_eq!(host.live_objects(), 0);
    assert_eq!(recipes.used(), 0);
}

#[test]
fn failure_in_later_context_cleans_earlier_native_replay_and_leaves_factory_empty() {
    let (host, mut saved, bytes, pages, payloads) = captured();
    saved.contexts[1].parts[1][..4].copy_from_slice(&u32::MAX.to_le_bytes());
    let factory = ExecutorFactory::new(host.clone());
    let budgets = factory.restore_budget_table(&bytes).unwrap();
    let recipes = PageBudget::new(2 * MAX_RECIPE_BYTES);
    assert!(factory
        .restore_checkpoint_objects(
            &saved,
            &budgets,
            &pages,
            &payloads,
            &recipes,
            Duration::from_secs(5)
        )
        .is_err());
    assert_eq!(factory.host_objects(), 0);
    assert_eq!(host.live_objects(), 0);
    assert_eq!(recipes.used(), 0);
}

#[test]
fn factory_object_file_refuses_truncation_duplicate_ids_and_oversized_parts() {
    let (_, mut saved, _, _, _) = captured();
    let bytes = saved.encode().unwrap();
    for len in 0..bytes.len() {
        assert!(FactoryObjectCheckpoint::decode(&bytes[..len]).is_err());
    }
    for (at, value) in [(0, VERSION + 1), (4, u32::MAX), (8, 0), (12, u32::MAX)] {
        let mut bad = bytes.clone();
        bad[at..at + 4].copy_from_slice(&value.to_le_bytes());
        assert!(FactoryObjectCheckpoint::decode(&bad).is_err());
    }
    let mut trailing = bytes;
    trailing.push(0);
    assert!(FactoryObjectCheckpoint::decode(&trailing).is_err());
    saved.contexts[1].id = saved.contexts[0].id;
    assert!(saved.encode().is_err());
}

#[test]
fn retention_covers_empty_and_reused_context_ids_and_late_enable_preserves_source() {
    use crate::venus::renderer::SinkFactory;
    let host = Arc::new(FakeVulkan::standard());
    let recipes = PageBudget::new(2 * MAX_RECIPE_BYTES);
    let mut factory = ExecutorFactory::new(host.clone());
    factory.enable_checkpoint_recipes(&recipes).unwrap();
    assert!(factory.enable_checkpoint_recipes(&recipes).is_err());
    for ctx in [1, 2] {
        factory.context_created(ctx);
    }
    factory.context_destroyed(1);
    factory.context_created(1);
    let mut budgets = factory.checkpoint_budget_table().unwrap();
    let saved = factory
        .checkpoint_objects(
            &mut budgets,
            &mut HostPageTable::default(),
            &mut SharedPayloads::default(),
            Duration::from_secs(5),
        )
        .unwrap();
    assert_eq!(saved.context_ids().collect::<Vec<_>>(), [1, 2]);
    let mut late = ExecutorFactory::new(host.clone());
    late.context_created(1);
    late.with_context_mut(1, |context| {
        context
            .execute(&mut h::create_instance(h::INSTANCE))
            .unwrap()
    })
    .unwrap();
    assert!(late.enable_checkpoint_recipes(&recipes).is_err());
    assert_eq!(
        late.host_objects(),
        1,
        "rejecting late retention leaves the source intact"
    );
    for ctx in [1, 2] {
        factory.context_destroyed(ctx);
    }
    late.context_destroyed(1);
    assert!(
        late.enable_checkpoint_recipes(&recipes).is_err(),
        "destroying a context cannot retrospectively enable complete retention"
    );
    assert_eq!(host.live_objects(), 0);
}

#[test]
fn uncaptured_query_results_refuse_save_without_discarding_the_live_context() {
    use crate::venus::protocol::*;
    use crate::venus::renderer::SinkFactory;
    let host = Arc::new(FakeVulkan::standard());
    let mut factory = ExecutorFactory::new(host.clone());
    factory
        .enable_checkpoint_recipes(&PageBudget::new(MAX_RECIPE_BYTES))
        .unwrap();
    factory.context_created(1);
    factory
        .with_context_mut(1, |context| {
            for mut command in [
                h::create_instance(h::INSTANCE),
                h::enumerate(h::INSTANCE, Some(vec![h::PHYSICAL])),
                h::create_device(h::PHYSICAL, h::DEVICE, Vec::new()),
                Command::CreateQueryPool(CreateQueryPoolArgs {
                    device: VkDevice(h::DEVICE),
                    p_create_info: Some(VkQueryPoolCreateInfo {
                        query_type: 0,
                        query_count: 1,
                        ..Default::default()
                    }),
                    p_query_pool: Some(VkQueryPool(0x70001)),
                    ret: VK_ERROR_UNKNOWN,
                }),
            ] {
                context.execute(&mut command).unwrap();
                assert!(
                    super::super::context::result_of(&command).is_none_or(|ret| ret == VK_SUCCESS)
                );
            }
            assert!(context
                .checkpoint_data_refusal()
                .unwrap()
                .contains("query-pool"));
        })
        .unwrap();
    let alive = host.live_objects();
    let mut budgets = factory.checkpoint_budget_table().unwrap();
    assert!(factory
        .checkpoint_objects(
            &mut budgets,
            &mut HostPageTable::default(),
            &mut SharedPayloads::default(),
            Duration::from_secs(5)
        )
        .is_err());
    assert_eq!(host.live_objects(), alive);
    factory.context_destroyed(1);
    assert_eq!(host.live_objects(), 0);
}
