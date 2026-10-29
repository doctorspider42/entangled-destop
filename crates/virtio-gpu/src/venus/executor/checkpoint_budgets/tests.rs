use super::*;

fn root(whole: &Arc<PageBudget>) -> BudgetRoot {
    BudgetRoot {
        key: BudgetRootKey::HostPages,
        whole: whole.clone(),
        share_limit: 64,
    }
}

#[test]
fn orphan_and_reused_context_owners_keep_distinct_shares_and_one_whole() {
    let whole = PageBudget::new(128);
    let old = PageBudget::share(&whole, 64);
    let new = PageBudget::share(&whole, 64);
    let old_charge = old.charge(32).unwrap();
    let new_charge = new.charge(16).unwrap();
    let mut table = BudgetTable::new(vec![root(&whole)]).unwrap();
    let old_id = table.register(old_charge.budget().unwrap()).unwrap();
    let new_id = table.register(new_charge.budget().unwrap()).unwrap();
    assert_ne!(
        old_id, new_id,
        "reused context ids must not merge allocation owners"
    );
    assert_eq!(table.register(&old.clone()).unwrap(), old_id);
    assert_eq!(table.root_key(old_id).unwrap(), BudgetRootKey::HostPages);
    let bytes = table.encode().unwrap();
    drop((table, old, new, old_charge, new_charge));
    assert_eq!(whole.used(), 0);
    let whole = PageBudget::new(128);
    let restored = BudgetTable::restore(&bytes, |key| {
        assert_eq!(key, BudgetRootKey::HostPages);
        Ok(root(&whole))
    })
    .unwrap();
    assert!(!Arc::ptr_eq(
        restored.budget(old_id).unwrap(),
        restored.budget(new_id).unwrap()
    ));
    assert!(Arc::ptr_eq(
        restored.budget(old_id).unwrap().whole().unwrap(),
        &whole
    ));
    assert!(Arc::ptr_eq(
        restored.budget(new_id).unwrap().whole().unwrap(),
        &whole
    ));
    assert_eq!(
        whole.used(),
        0,
        "saved used/peak counters are never restored as charges"
    );
    let old_charge = restored.budget(old_id).unwrap().charge(32).unwrap();
    let new_charge = restored.budget(new_id).unwrap().charge(16).unwrap();
    assert_eq!(whole.used(), 48);
    assert!(restored.budget(old_id).unwrap().charge(33).is_err());
    assert!(restored.budget(new_id).unwrap().charge(49).is_err());
    drop((old_charge, new_charge));
    assert_eq!(whole.used(), 0);
    assert_eq!(restored.encode().unwrap(), bytes);
}

#[test]
fn malformed_framing_and_parent_links_fail_before_resolving_any_roots() {
    let whole = PageBudget::new(128);
    let mut table = BudgetTable::new(vec![root(&whole)]).unwrap();
    table.register(&PageBudget::share(&whole, 64)).unwrap();
    let bytes = table.encode().unwrap();
    let mut calls = 0;
    let mut resolver = |_| {
        calls += 1;
        Ok(root(&whole))
    };
    for len in 0..bytes.len() {
        assert!(BudgetTable::restore(&bytes[..len], &mut resolver).is_err());
    }
    for (at, word) in [
        (0, 2_u32),
        (4, u32::MAX),
        (8, u32::MAX),
        (12, 9),
        (16, 1),
        (20, 1),
        (52, 0),
        (52, 2),
    ] {
        let mut bad = bytes.clone();
        bad[at..at + 4].copy_from_slice(&word.to_le_bytes());
        assert!(
            BudgetTable::restore(&bad, &mut resolver).is_err(),
            "corruption at {at}"
        );
    }
    let mut bad = bytes.clone();
    bad[44..52].copy_from_slice(&129_u64.to_le_bytes());
    assert!(BudgetTable::restore(&bad, &mut resolver).is_err());
    let mut bad = bytes.clone();
    bad.push(0);
    assert!(BudgetTable::restore(&bad, &mut resolver).is_err());
    // Duplicate root role, even with otherwise identical valid policies.
    let mut duplicate = bytes[..12].to_vec();
    duplicate[4..8].copy_from_slice(&2_u32.to_le_bytes());
    duplicate[8..12].copy_from_slice(&0_u32.to_le_bytes());
    duplicate.extend_from_slice(&bytes[12..52]);
    duplicate.extend_from_slice(&bytes[12..52]);
    assert!(BudgetTable::restore(&duplicate, &mut resolver).is_err());
    assert_eq!(calls, 0);
}

#[test]
fn file_caps_cannot_enlarge_trusted_factory_policy_or_reuse_live_counters() {
    let whole = PageBudget::new(128);
    let bytes = BudgetTable::new(vec![root(&whole)])
        .unwrap()
        .encode()
        .unwrap();
    for (at, value) in [(36, 256_u64), (44, 65), (44, 63)] {
        let mut bad = bytes.clone();
        bad[at..at + 8].copy_from_slice(&value.to_le_bytes());
        assert!(BudgetTable::restore(&bad, |_| Ok(root(&whole))).is_err());
        assert_eq!(whole.used(), 0);
    }
    let charge = whole.charge(1).unwrap();
    assert!(BudgetTable::restore(&bytes, |_| Ok(root(&whole))).is_err());
    assert_eq!(whole.used(), 1);
    drop(charge);
    assert!(BudgetTable::restore(&bytes, |_| Ok(root(&whole))).is_ok());
}

#[test]
fn unknown_nested_or_differently_capped_owners_are_not_silently_flattened() {
    let whole = PageBudget::new(128);
    let share = PageBudget::share(&whole, 64);
    let mut table = BudgetTable::new(vec![root(&whole)]).unwrap();
    let id = table.register(&share).unwrap();
    assert!(table.register(&PageBudget::share(&share, 64)).is_err());
    assert!(table.register(&PageBudget::share(&whole, 63)).is_err());
    assert!(table.register(&PageBudget::new(64)).is_err());
    assert!(table.budget(0).is_err());
    assert!(table.budget(u32::MAX).is_err());
    assert_eq!(table.register(&share).unwrap(), id);
    let roots = vec![
        root(&whole),
        BudgetRoot {
            key: BudgetRootKey::Class(0),
            whole: whole.clone(),
            share_limit: 64,
        },
    ];
    assert!(
        BudgetTable::new(roots).is_err(),
        "equal-role budgets must not alias across roots"
    );
}

#[test]
fn heap_keys_and_class_roots_roundtrip_and_share_clamping_matches_runtime() {
    let heap = PageBudget::new(32);
    let class = PageBudget::new(16);
    let roots = vec![
        BudgetRoot {
            key: BudgetRootKey::Heap(([7; 16], 2)),
            whole: heap.clone(),
            share_limit: 64,
        },
        BudgetRoot {
            key: BudgetRootKey::Class(0),
            whole: class.clone(),
            share_limit: 8,
        },
    ];
    let mut table = BudgetTable::new(roots.clone()).unwrap();
    let heap_id = table.register(&PageBudget::share(&heap, 64)).unwrap();
    let class_id = table.register(&PageBudget::share(&class, 8)).unwrap();
    let bytes = table.encode().unwrap();
    let restored = BudgetTable::restore(&bytes, |key| {
        Ok(roots.iter().find(|root| root.key == key).unwrap().clone())
    })
    .unwrap();
    assert_eq!(
        restored.root_key(heap_id).unwrap(),
        BudgetRootKey::Heap(([7; 16], 2))
    );
    assert_eq!(restored.budget(heap_id).unwrap().limit(), 32);
    assert_eq!(
        restored.root_key(class_id).unwrap(),
        BudgetRootKey::Class(0)
    );
    assert_eq!(restored.budget(class_id).unwrap().limit(), 8);
    let empty = BudgetTable::new(Vec::new()).unwrap().encode().unwrap();
    assert!(BudgetTable::restore(&empty, |_| panic!("empty table has no roots")).is_ok());
}

#[test]
fn factory_context_links_restore_actual_shares_without_merging_orphan_owners() {
    use super::super::fake::FakeVulkan;
    use crate::venus::renderer::SinkFactory;
    let host = Arc::new(FakeVulkan::standard());
    let mut source = ExecutorFactory::new(host.clone());
    let old = source.context(1);
    let orphan = old.lock().unwrap().budget.charge(4096).unwrap();
    source.context_destroyed(1);
    let current = source.context(1);
    let other = source.context(2);
    let guest = super::super::probe(host.as_ref()).unwrap().remove(0);
    let heap = guest
        .host_memory
        .memory_heaps
        .iter()
        .enumerate()
        .take(guest.host_memory.memory_heap_count as usize)
        .find(|(_, heap)| heap.flags & super::super::policy::MEMORY_HEAP_DEVICE_LOCAL != 0)
        .unwrap();
    let key = (super::super::memory::heap_gpu(&guest), heap.0 as u32);
    let _ = current
        .lock()
        .unwrap()
        .objects
        .limits()
        .device_local(key, heap.1.size);
    let mut budgets = source.checkpoint_budget_table().unwrap();
    let first_links = current
        .lock()
        .unwrap()
        .checkpoint_budget_links(&mut budgets)
        .unwrap();
    let other_links = other
        .lock()
        .unwrap()
        .checkpoint_budget_links(&mut budgets)
        .unwrap();
    let orphan_id = budgets.register(orphan.budget().unwrap()).unwrap();
    assert_ne!(orphan_id, first_links.host_pages);
    assert_ne!(first_links.host_pages, other_links.host_pages);
    let first_bytes = first_links.encode(&budgets).unwrap();
    let other_bytes = other_links.encode(&budgets).unwrap();
    let bytes = budgets.encode().unwrap();
    let restored_factory = ExecutorFactory::new(host);
    let mut restored_budgets = restored_factory.restore_budget_table(&bytes).unwrap();
    let first_links = ContextBudgetLinks::decode(&first_bytes, &restored_budgets).unwrap();
    let other_links = ContextBudgetLinks::decode(&other_bytes, &restored_budgets).unwrap();
    let first = restored_factory
        .new_checkpoint_context(1, &first_links, &restored_budgets)
        .unwrap();
    let second = restored_factory
        .new_checkpoint_context(2, &other_links, &restored_budgets)
        .unwrap();
    assert!(Arc::ptr_eq(
        first.budget.whole().unwrap(),
        &restored_factory.budget
    ));
    assert!(!Arc::ptr_eq(&first.budget, &second.budget));
    assert!(!Arc::ptr_eq(
        &first.budget,
        restored_budgets.budget(orphan_id).unwrap()
    ));
    let old_charge = restored_budgets
        .budget(orphan_id)
        .unwrap()
        .charge(4096)
        .unwrap();
    assert_eq!(first.budget.used(), 0);
    assert_eq!(restored_factory.budget.used(), 4096);
    assert_eq!(
        first
            .checkpoint_budget_links(&mut restored_budgets)
            .unwrap()
            .encode(&restored_budgets)
            .unwrap(),
        first_bytes
    );
    assert_eq!(
        second
            .checkpoint_budget_links(&mut restored_budgets)
            .unwrap()
            .encode(&restored_budgets)
            .unwrap(),
        other_bytes
    );
    let foreign = ExecutorFactory::new(Arc::new(FakeVulkan::standard()));
    assert!(foreign
        .new_checkpoint_context(1, &first_links, &restored_budgets)
        .is_err());
    let owner = first_links.heaps[0].1;
    let payload_limits = restored_factory
        .checkpoint_payload_limits(owner, &restored_budgets)
        .unwrap();
    assert!(Arc::ptr_eq(
        &payload_limits.device_local(key, heap.1.size),
        restored_budgets.budget(owner).unwrap()
    ));
    assert!(restored_factory
        .checkpoint_payload_limits(first_links.host_pages, &restored_budgets)
        .is_err());
    drop(old_charge);
    assert_eq!(restored_factory.budget.used(), 0);
}

#[test]
fn context_link_codec_rejects_wrong_roles_and_non_share_nodes() {
    use super::super::fake::FakeVulkan;
    let mut source = ExecutorFactory::new(Arc::new(FakeVulkan::standard()));
    let context = source.context(1);
    let mut table = source.checkpoint_budget_table().unwrap();
    let links = context
        .lock()
        .unwrap()
        .checkpoint_budget_links(&mut table)
        .unwrap();
    let bytes = links.encode(&table).unwrap();
    for len in 0..bytes.len() {
        assert!(ContextBudgetLinks::decode(&bytes[..len], &table).is_err());
    }
    let mut changed = links.clone();
    changed.classes[0] = links.host_pages;
    assert!(changed.encode(&table).is_err());
    let root_id = table.register(&source.budget).unwrap();
    changed = links.clone();
    changed.host_pages = root_id;
    assert!(changed.encode(&table).is_err());
    for (at, value) in [(0, 2_u32), (4, 0), (8, 0), (12, links.host_pages)] {
        let mut bad = bytes.clone();
        bad[at..at + 4].copy_from_slice(&value.to_le_bytes());
        assert!(ContextBudgetLinks::decode(&bad, &table).is_err());
    }
    let mut bad = bytes;
    bad.push(0);
    assert!(ContextBudgetLinks::decode(&bad, &table).is_err());
}

#[test]
fn cold_context_adoption_and_failures_release_native_objects() {
    use super::super::{fake::FakeVulkan, harness as h};
    use crate::venus::renderer::SinkFactory;
    let host = Arc::new(FakeVulkan::standard());
    let mut factory = ExecutorFactory::new(host.clone());
    let mut table = factory.checkpoint_budget_table().unwrap();
    let links = factory
        .context(1)
        .lock()
        .unwrap()
        .checkpoint_budget_links(&mut table)
        .unwrap();
    factory.context_destroyed(1);
    let create = |id| {
        let mut context = factory.new_checkpoint_context(id, &links, &table).unwrap();
        context
            .execute(&mut h::create_instance(h::INSTANCE))
            .unwrap();
        context
    };
    let context = create(1);
    assert_eq!(host.live_objects(), 1);
    drop(context);
    assert_eq!(
        host.live_objects(),
        0,
        "an abandoned replay destroys its native instance"
    );
    let context = create(1);
    let mut foreign = ExecutorFactory::new(host.clone());
    assert!(foreign.adopt_checkpoint_context(context).is_err());
    assert_eq!(
        host.live_objects(),
        0,
        "foreign adoption failure cleans the candidate"
    );
    let context = create(1);
    let duplicate = create(1);
    assert_eq!(host.live_objects(), 2);
    factory.adopt_checkpoint_context(context).unwrap();
    assert_eq!(
        host.live_objects(),
        2,
        "adoption does not destroy the native context"
    );
    assert!(factory.adopt_checkpoint_context(duplicate).is_err());
    assert_eq!(
        host.live_objects(),
        1,
        "duplicate failure preserves the adopted context"
    );
    factory.context_destroyed(1);
    assert_eq!(host.live_objects(), 0);
}
