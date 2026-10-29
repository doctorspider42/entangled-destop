use super::*;

#[test]
fn address_capable_host_pages_reconstruct_without_exposing_a_device_address() {
    use crate::venus::executor::{fake::FakeVulkan, harness as h};
    use crate::venus::protocol::*;
    let host = Arc::new(FakeVulkan::standard());
    let recipes_budget = PageBudget::new(super::super::checkpoint::MAX_RECIPE_BYTES);
    let mut source = VulkanContext::new(1, host.clone());
    source.enable_object_recipes(&recipes_budget).unwrap();
    for mut command in [
        h::create_instance(h::INSTANCE),
        h::enumerate(h::INSTANCE, Some(vec![h::PHYSICAL])),
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
        h::allocate(
            h::DEVICE,
            h::MEMORY,
            4097,
            3,
            vec![VkMemoryAllocateInfoNext::VkMemoryAllocateFlagsInfo(
                VkMemoryAllocateFlagsInfo {
                    flags: 2,
                    device_mask: 0,
                },
            )],
        ),
    ] {
        source.execute(&mut command).unwrap();
        assert!(super::super::context::result_of(&command).is_none_or(|value| value == VK_SUCCESS));
    }
    let links = source.host_allocation_links().unwrap();
    links[0].pages.write_bytes(4000, &[0xa7; 97]).unwrap();
    let mut pages = HostPageTable::default();
    let encoded_links = pages.encode_links(&links).unwrap();
    let mut data = Vec::new();
    pages.save(&mut data).unwrap();
    let recipes = source.save_object_recipes().unwrap();
    source.destroy_all();
    drop(source);
    drop(links);
    drop(pages);
    let loaded = HostPageTable::restore(&data, &PageBudget::new(8192)).unwrap();
    let links = loaded.resolve_links(&encoded_links).unwrap();
    let mut cold = VulkanContext::new(1, host.clone());
    cold.restore_object_recipes_with_memory(&recipes, &recipes_budget, &[], &links)
        .unwrap();
    assert_eq!(
        cold.objects
            .memory(h::DEVICE, h::MEMORY)
            .unwrap()
            .allocate_flags,
        2
    );
    assert!(cold.checkpoint_data_refusal().is_none());
    let mut bytes = [0; 97];
    cold.host_allocation_links().unwrap()[0]
        .pages
        .read_bytes(4000, &mut bytes)
        .unwrap();
    assert_eq!(bytes, [0xa7; 97]);
    cold.destroy_all();
    assert_eq!(host.live_objects(), 0);
}

#[test]
fn one_page_id_preserves_a_ring_mapping_including_its_padding() {
    let pages = Arc::new(RingPages::new(17).unwrap());
    let mapped = pages.mapped_len();
    pages.write_bytes(0, &[7; 17]).unwrap();
    pages.write_mapped_bytes(mapped - 4, &[1, 2, 3, 4]).unwrap();
    let mut table = HostPageTable::default();
    assert_eq!(table.register(&pages).unwrap(), 1);
    assert_eq!(table.register(&pages.clone()).unwrap(), 1);
    let mut bytes = Vec::new();
    table.save(&mut bytes).unwrap();
    drop(table);
    drop(pages);
    let budget = PageBudget::new(mapped);
    let restored = HostPageTable::restore(&bytes, &budget).unwrap();
    let page = restored.page(1).unwrap();
    assert_eq!(page.resource_len(), 17);
    assert_eq!(page.mapped_len(), mapped);
    let mut padding = [0; 4];
    page.read_mapped_bytes(mapped - 4, &mut padding).unwrap();
    assert_eq!(padding, [1, 2, 3, 4]);
    assert!(page.read_bytes(mapped - 4, &mut padding).is_err());
    assert!(page.write_mapped_bytes(mapped - 1, &[0; 2]).is_err());
    assert!(page.read_mapped_bytes(u64::MAX, &mut padding).is_err());
    assert_eq!(budget.used(), mapped);
    drop(restored);
    assert_eq!(budget.used(), 0);
}

#[test]
fn malformed_page_files_and_failed_budgets_leave_no_partial_allocations() {
    let source = PageBudget::new(128 << 10);
    let pages = Arc::new(RingPages::for_memory(65537, 65536, &source).unwrap());
    let mut table = HostPageTable::default();
    table.register(&pages).unwrap();
    let mut bytes = Vec::new();
    table.save(&mut bytes).unwrap();
    let budget = PageBudget::new(128 << 10);
    let mut bad: Vec<_> = (0..32)
        .chain([bytes.len() - 1])
        .map(|end| bytes[..end].to_vec())
        .collect();
    for offset in [0, 4, 8, 16, 24] {
        let mut changed = bytes.clone();
        changed[offset..offset + 4].fill(0xff);
        bad.push(changed);
    }
    for offset in [8, 16, 24] {
        let mut changed = bytes.clone();
        changed[offset..offset + 8].fill(0);
        bad.push(changed);
    }
    let mut trailing = bytes.clone();
    trailing.push(0);
    bad.push(trailing);
    for malformed in bad {
        assert!(HostPageTable::restore(&malformed, &budget).is_err());
        assert_eq!(budget.used(), 0);
    }
    assert!(HostPageTable::restore(&bytes, &PageBudget::new(65536)).is_err());
    let another = Arc::new(RingPages::new(4096).unwrap());
    table.register(&another).unwrap();
    let mut bytes = Vec::new();
    table.save(&mut bytes).unwrap();
    assert!(
        HostPageTable::restore(&bytes, &budget).is_err(),
        "second allocation exceeds the remaining budget"
    );
    assert_eq!(budget.used(), 0, "first allocation was released too");
}

#[test]
fn reconstructed_contexts_and_their_blob_reuse_one_loaded_page_allocation() {
    use super::super::{harness::*, s1_tests as s1};
    use crate::renderer::Renderer3d;
    let host = s1::host_with(vec![s1::gpu("shared pages", 7)]);
    let mut h = Harness::new(host.clone());
    let journal_budget = PageBudget::new(super::super::checkpoint::MAX_RECIPE_BYTES);
    for ctx in [CTX, 2] {
        h.use_context(ctx);
        h.renderer
            .factory()
            .with_context_mut(ctx, |context| {
                context.enable_object_recipes(&journal_budget).unwrap()
            })
            .unwrap();
        s1::device_on(&mut h, &[PHYSICAL], PHYSICAL, s1::S1_EXTENSIONS);
        if ctx == CTX {
            h.send(&allocate(DEVICE, MEMORY, 65537, s1::HOST_TYPE, Vec::new()))
                .unwrap();
            h.memory_blob(CTX, 91, MEMORY, 65537u64.next_multiple_of(4096))
                .unwrap();
        } else {
            h.renderer.ctx_attach_blob(ctx, 91, true);
            h.send(&allocate(
                DEVICE,
                MEMORY,
                4096,
                s1::HOST_TYPE,
                vec![s1::import_of(91)],
            ))
            .unwrap();
        }
    }
    let mut table = HostPageTable::default();
    let mut records = Vec::new();
    for ctx in [CTX, 2] {
        let (recipes, links) = h
            .renderer
            .factory()
            .with_context(ctx, |context| {
                (
                    context.save_object_recipes().unwrap(),
                    context.host_allocation_links().unwrap(),
                )
            })
            .unwrap();
        let encoded = table.encode_links(&links).unwrap();
        records.push((recipes, encoded));
    }
    assert_eq!(table.pages().len(), 1);
    let old = table.page(1).unwrap().clone();
    old.write_bytes(65000, &[1, 2, 3, 4]).unwrap();
    let mut data = Vec::new();
    table.save(&mut data).unwrap();
    h.renderer.ctx_destroy(CTX);
    h.renderer.ctx_destroy(2);
    drop(h);
    drop(table);
    let pages_budget = PageBudget::new(old.mapped_len());
    let loaded = HostPageTable::restore(&data, &pages_budget).unwrap();
    assert!(!Arc::ptr_eq(loaded.page(1).unwrap(), &old));
    drop(old);
    let mut restored = Vec::new();
    for (index, (recipes, encoded)) in records.into_iter().enumerate() {
        let links = loaded.resolve_links(&encoded).unwrap();
        let mut missing = VulkanContext::new(index as u32 + 1, host.clone());
        assert!(missing
            .restore_object_recipes_with_memory(&recipes, &journal_budget, &[], &[])
            .is_err());
        assert_eq!(missing.object_count(), 0);
        let mut context = VulkanContext::new(index as u32 + 1, host.clone());
        context
            .restore_object_recipes_with_memory(&recipes, &journal_budget, &[], &links)
            .unwrap();
        assert!(
            context.host_allocation_links().unwrap()[0].exported,
            "one-blob export state survives too"
        );
        restored.push(context);
    }
    let first = restored[0].host_allocation_links().unwrap()[0]
        .pages
        .clone();
    let second = restored[1].host_allocation_links().unwrap()[0]
        .pages
        .clone();
    assert!(Arc::ptr_eq(&first, &second));
    assert!(
        Arc::ptr_eq(&first, loaded.page(1).unwrap()),
        "the blob uses the same loaded root"
    );
    first.write_bytes(65000, &[9, 8, 7, 6]).unwrap();
    let mut actual = [0; 4];
    second.read_bytes(65000, &mut actual).unwrap();
    assert_eq!(actual, [9, 8, 7, 6]);
    assert_eq!(
        pages_budget.used(),
        first.mapped_len(),
        "each import adds zero page charges"
    );
    for context in &mut restored {
        context.destroy_all();
    }
    drop(restored);
    drop(first);
    drop(second);
    drop(loaded);
    assert_eq!(pages_budget.used(), 0);
    assert_eq!(host.live_objects(), 0);
}

#[test]
fn hostile_page_links_and_private_staging_cannot_become_published_memory() {
    let budget = PageBudget::new(65536);
    let pages = Arc::new(RingPages::for_memory(64, 65536, &budget).unwrap());
    let mut table = HostPageTable::default();
    let link = HostAllocationLink {
        device: 1,
        memory: 2,
        allocation_serial: 3,
        exported: true,
        pages,
    };
    let bytes = table.encode_links(std::slice::from_ref(&link)).unwrap();
    assert_eq!(bytes.len(), 40);
    let mut bad: Vec<_> = (0..bytes.len()).map(|end| bytes[..end].to_vec()).collect();
    for offset in [0, 4, 32, 36] {
        let mut changed = bytes.clone();
        changed[offset..offset + 4].fill(0xff);
        bad.push(changed);
    }
    for offset in [8, 16, 24, 32] {
        let mut changed = bytes.clone();
        changed[offset..offset + 4].fill(0);
        bad.push(changed);
    }
    let mut duplicate = bytes.clone();
    duplicate[4..8].copy_from_slice(&2u32.to_le_bytes());
    duplicate.extend_from_slice(&bytes[8..]);
    bad.push(duplicate);
    let mut trailing = bytes.clone();
    trailing.push(0);
    bad.push(trailing);
    for malformed in bad {
        assert!(table.resolve_links(&malformed).is_err());
    }
    assert!(table.encode_links(&[link.clone(), link]).is_err());
    let private_budget = PageBudget::new(65536);
    let private =
        crate::venus::shmem::PrivatePages::for_memory(64, 65536, &private_budget).unwrap();
    assert!(table.register(private.import_pages()).is_err());
}
