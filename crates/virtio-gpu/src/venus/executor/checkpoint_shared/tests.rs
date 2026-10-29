use super::super::{fake::FakeVulkan, harness::*, s1_tests as s1};
use super::*;
use crate::renderer::Renderer3d;

fn enable(h: &Harness<FakeVulkan>, budget: &Arc<PageBudget>) {
    h.renderer
        .factory()
        .with_context_mut(h.ctx, |ctx| ctx.enable_object_recipes(budget).unwrap())
        .unwrap();
}

fn capture(h: &Harness<FakeVulkan>) -> (Vec<u8>, Vec<SharedAllocationLink>) {
    h.renderer
        .factory()
        .with_context(h.ctx, |ctx| {
            (
                ctx.save_object_recipes().unwrap(),
                ctx.shared_allocation_links().unwrap(),
            )
        })
        .unwrap()
}

fn fixture() -> (Harness<FakeVulkan>, Arc<FakeVulkan>, Arc<PageBudget>) {
    let host = s1::host_with(vec![s1::gpu("checkpoint GPU", 7)]);
    let h = Harness::new(host.clone());
    let budget = PageBudget::new(crate::venus::executor::checkpoint::MAX_RECIPE_BYTES);
    enable(&h, &budget);
    (h, host, budget)
}

#[test]
fn exporter_and_importer_reconstruct_the_same_payload_without_the_old_directory() {
    let (mut h, host, budget) = fixture();
    s1::device_on(&mut h, &[PHYSICAL], PHYSICAL, s1::S1_EXTENSIONS);
    let size = s1::export(&mut h);
    let (export_recipes, exporter) = capture(&h);
    assert_eq!(exporter.len(), 1);
    let original_payload = host.exportable_allocations()[0].0;
    h.use_context(2);
    enable(&h, &budget);
    s1::device_on(&mut h, &[PHYSICAL], PHYSICAL, s1::S1_EXTENSIONS);
    h.renderer.ctx_attach_blob(2, s1::EXPORTED_RES, true);
    assert_eq!(
        s1::resource_properties(&mut h, s1::EXPORTED_RES).0,
        VK_SUCCESS
    );
    h.send(&allocate(
        DEVICE,
        s1::IMPORTED_MEM,
        size,
        s1::DEVICE_LOCAL_TYPE,
        vec![s1::import_of(s1::EXPORTED_RES)],
    ))
    .unwrap();
    let (import_recipes, importer) = capture(&h);
    assert_eq!(importer.len(), 1);
    assert!(Arc::ptr_eq(&exporter[0].handle.0, &importer[0].handle.0));
    // Imports retain checkpoint identity after the guest destroys its blob.
    h.renderer.destroy_blob(s1::EXPORTED_RES);
    assert_eq!(
        s1::resource_properties(&mut h, s1::EXPORTED_RES).0,
        VK_ERROR_INVALID_EXTERNAL_HANDLE
    );
    let (after_query, after_destroy) = capture(&h);
    assert_eq!(
        after_query, import_recipes,
        "pure blob queries never enter reconstruction history"
    );
    assert!(Arc::ptr_eq(
        &importer[0].handle.0,
        &after_destroy[0].handle.0
    ));
    drop(after_destroy);
    let mut captured = SharedPayloads::default();
    let export_link_file = captured.encode_links(&exporter).unwrap();
    let import_link_file = captured.encode_links(&importer).unwrap();
    assert_eq!(
        captured.handles().len(),
        1,
        "two contexts share one file payload id"
    );
    h.use_context(3);
    enable(&h, &budget);
    s1::device_on(&mut h, &[PHYSICAL], PHYSICAL, s1::S1_EXTENSIONS);
    s1::export(&mut h);
    let (_, replacement) = capture(&h);
    let payload = host.exportable_allocations().last().unwrap().0;
    assert_ne!(payload, original_payload);
    let loaded = SharedPayloads::from_handles(vec![replacement[0].handle.clone()]).unwrap();
    let restored_exporter = loaded.resolve_links(&export_link_file).unwrap();
    let restored_importer = loaded.resolve_links(&import_link_file).unwrap();
    assert!(Arc::ptr_eq(
        &restored_exporter[0].handle.0,
        &restored_importer[0].handle.0
    ));
    assert!(!Arc::ptr_eq(
        &restored_exporter[0].handle.0,
        &exporter[0].handle.0
    ));
    h.renderer.ctx_destroy(3);
    h.renderer.ctx_destroy(CTX);
    h.renderer.ctx_destroy(2);
    drop(h);
    let before = host.handle_imports().len();
    let mut first = VulkanContext::new(CTX, host.clone());
    first
        .restore_object_recipes_with_shared(&export_recipes, &budget, &restored_exporter)
        .unwrap();
    let mut second = VulkanContext::new(2, host.clone());
    second
        .restore_object_recipes_with_shared(&import_recipes, &budget, &restored_importer)
        .unwrap();
    let imports = host.handle_imports();
    assert_eq!(imports.len(), before + 2);
    assert!(imports[before..]
        .iter()
        .all(|import| import.payload == payload));
    assert_eq!(
        first.shared_allocation_links().unwrap()[0].allocation_serial,
        exporter[0].allocation_serial
    );
    assert_eq!(
        second.shared_allocation_links().unwrap()[0].allocation_serial,
        importer[0].allocation_serial
    );
    first.destroy_all();
    second.destroy_all();
    drop(first);
    drop(second);
    drop((
        exporter,
        importer,
        replacement,
        captured,
        loaded,
        restored_exporter,
        restored_importer,
    ));
    assert_eq!(host.live_objects(), 0);
    assert_eq!(host.live_shared_handles(), 0);
}

#[test]
fn a_reused_memory_id_cannot_redirect_a_dead_allocation_generation() {
    let (mut h, host, budget) = fixture();
    s1::device_on(&mut h, &[PHYSICAL], PHYSICAL, s1::S1_EXTENSIONS);
    s1::export(&mut h);
    let (_, stale) = capture(&h);
    h.send(&destroy_image(DEVICE, s1::EXPORTER)).unwrap();
    h.send(&free(DEVICE, s1::EXPORTED_MEM)).unwrap();
    h.renderer.destroy_blob(s1::EXPORTED_RES);
    s1::export(&mut h);
    let (recipes, current) = capture(&h);
    assert_eq!(current[0].memory, stale[0].memory);
    assert_ne!(current[0].allocation_serial, stale[0].allocation_serial);
    h.renderer.ctx_destroy(CTX);
    drop(h);
    let mut wrong = VulkanContext::new(CTX, host.clone());
    assert!(wrong
        .restore_object_recipes_with_shared(&recipes, &budget, &stale)
        .is_err());
    assert_eq!(wrong.object_count(), 0);
    drop(wrong);
    let mut correct = VulkanContext::new(CTX, host.clone());
    correct
        .restore_object_recipes_with_shared(&recipes, &budget, &current)
        .unwrap();
    assert_eq!(
        correct.shared_allocation_links().unwrap()[0].allocation_serial,
        current[0].allocation_serial
    );
    correct.destroy_all();
    drop(correct);
    drop(current);
    drop(stale);
    assert_eq!(host.live_objects(), 0);
    assert_eq!(host.live_shared_handles(), 0);
}

#[test]
fn malformed_link_tables_are_rejected_before_creating_any_objects() {
    let (mut h, host, budget) = fixture();
    s1::device_on(&mut h, &[PHYSICAL], PHYSICAL, s1::S1_EXTENSIONS);
    s1::export(&mut h);
    let (recipes, links) = capture(&h);
    let mut bad = vec![vec![links[0].clone(), links[0].clone()]];
    for field in 0..3 {
        let mut changed = links[0].clone();
        match field {
            0 => changed.memory = 0,
            1 => changed.device = 0,
            _ => changed.allocation_serial = 0,
        }
        bad.push(vec![changed]);
    }
    let calls = host.calls();
    for malformed in bad {
        let mut context = VulkanContext::new(CTX, host.clone());
        assert!(context
            .restore_object_recipes_with_shared(&recipes, &budget, &malformed)
            .is_err());
        assert_eq!(host.calls(), calls);
        assert_eq!(context.object_count(), 0);
    }
}

#[test]
fn file_payload_ids_are_dense_and_malformed_links_never_touch_the_host() {
    let (mut h, host, _) = fixture();
    s1::device_on(&mut h, &[PHYSICAL], PHYSICAL, s1::S1_EXTENSIONS);
    s1::export(&mut h);
    let (_, links) = capture(&h);
    let mut table = SharedPayloads::default();
    let bytes = table.encode_links(&links).unwrap();
    assert_eq!(bytes.len(), 36);
    assert_eq!(&bytes[32..36], &1u32.to_le_bytes());
    assert!(
        SharedPayloads::from_handles(vec![links[0].handle.clone(), links[0].handle.clone()])
            .is_err()
    );
    let mut bad: Vec<_> = (0..bytes.len()).map(|end| bytes[..end].to_vec()).collect();
    for offset in [0, 4, 32] {
        let mut changed = bytes.clone();
        changed[offset..offset + 4].copy_from_slice(&u32::MAX.to_le_bytes());
        bad.push(changed);
    }
    for offset in [8, 16, 24, 32] {
        let mut changed = bytes.clone();
        changed[offset..offset + 4].fill(0);
        bad.push(changed);
    }
    let mut trailing = bytes.clone();
    trailing.push(0);
    bad.push(trailing);
    let mut duplicate = bytes.clone();
    duplicate[4..8].copy_from_slice(&2u32.to_le_bytes());
    duplicate.extend_from_slice(&bytes[8..]);
    bad.push(duplicate);
    let calls = host.calls();
    for malformed in bad {
        assert!(table.resolve_links(&malformed).is_err());
    }
    assert_eq!(host.calls(), calls);
    let decoded = table.resolve_links(&bytes).unwrap();
    assert_eq!(decoded[0].allocation_serial, links[0].allocation_serial);
    assert!(Arc::ptr_eq(&decoded[0].handle.0, &links[0].handle.0));
    h.renderer.ctx_destroy(CTX);
    drop(h);
    drop(table);
    drop(decoded);
    drop(links);
    assert_eq!(host.live_objects(), 0);
    assert_eq!(host.live_shared_handles(), 0);
}

#[test]
fn retired_imports_rebuild_after_their_blobs_are_gone() {
    for pages in [false, true] {
        let (mut h, host, budget) = fixture();
        s1::device_on(&mut h, &[PHYSICAL], PHYSICAL, s1::S1_EXTENSIONS);
        let size = if pages {
            let size = 1 << 20;
            h.send(&allocate(DEVICE, MEMORY, size, s1::HOST_TYPE, Vec::new()))
                .unwrap();
            h.memory_blob(CTX, s1::EXPORTED_RES, MEMORY, size).unwrap();
            size
        } else {
            s1::export(&mut h)
        };
        h.use_context(2);
        enable(&h, &budget);
        s1::device_on(&mut h, &[PHYSICAL], PHYSICAL, s1::S1_EXTENSIONS);
        h.renderer.ctx_attach_blob(2, s1::EXPORTED_RES, true);
        if pages {
            h.send(&create_buffer(DEVICE, BUFFER, buffer_info(size, 3)))
                .unwrap();
        } else {
            h.send(&create_image(DEVICE, IMAGE, s1::exporter_image()))
                .unwrap();
        }
        h.send(&allocate(
            DEVICE,
            MEMORY,
            4096,
            if pages {
                s1::HOST_TYPE
            } else {
                s1::DEVICE_LOCAL_TYPE
            },
            vec![s1::import_of(s1::EXPORTED_RES)],
        ))
        .unwrap();
        if pages {
            h.send(&bind_buffers(DEVICE, &[(BUFFER, MEMORY, 0)]))
                .unwrap();
        } else {
            h.send(&bind_image(DEVICE, IMAGE, MEMORY, 0)).unwrap();
        }
        let (live_recipes, _) = capture(&h);
        let mut unlinked = VulkanContext::new(2, host.clone());
        assert!(
            unlinked
                .restore_object_recipes(&live_recipes, &budget)
                .is_err(),
            "a live import must never silently become private scratch memory"
        );
        assert_eq!(unlinked.object_count(), 0);
        drop(unlinked);
        if pages {
            h.send(&destroy_buffer(DEVICE, BUFFER)).unwrap();
        } else {
            h.send(&destroy_image(DEVICE, IMAGE)).unwrap();
        }
        h.send(&free(DEVICE, MEMORY)).unwrap();
        let (recipes, links) = capture(&h);
        assert!(links.is_empty());
        h.renderer.destroy_blob(s1::EXPORTED_RES);
        h.renderer.ctx_destroy(CTX);
        h.renderer.ctx_destroy(2);
        drop(h);
        let mut restored = VulkanContext::new(2, host.clone());
        restored.restore_object_recipes(&recipes, &budget).unwrap();
        assert!(restored.objects.checkpoint_memories().next().is_none());
        restored.destroy_all();
        drop(restored);
        assert_eq!(host.live_objects(), 0);
        assert_eq!(host.live_shared_handles(), 0);
    }
}

#[test]
fn corrupt_import_geometry_is_rejected_before_the_first_native_object() {
    let (mut h, host, budget) = fixture();
    s1::device_on(&mut h, &[PHYSICAL], PHYSICAL, s1::S1_EXTENSIONS);
    let size = s1::export(&mut h);
    h.use_context(2);
    enable(&h, &budget);
    s1::device_on(&mut h, &[PHYSICAL], PHYSICAL, s1::S1_EXTENSIONS);
    h.renderer.ctx_attach_blob(2, s1::EXPORTED_RES, true);
    h.send(&allocate(
        DEVICE,
        MEMORY,
        size,
        s1::DEVICE_LOCAL_TYPE,
        vec![s1::import_of(s1::EXPORTED_RES)],
    ))
    .unwrap();
    h.send(&free(DEVICE, MEMORY)).unwrap();
    let (bytes, _) = capture(&h);
    let meta = bytes.len() - 36;
    let mut bad = Vec::new();
    for (offset, value) in [
        (meta - 4, 0u32),
        (meta - 4, 2),
        (meta + 32, 0),
        (meta + 32, 3),
    ] {
        let mut changed = bytes.clone();
        changed[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
        bad.push(changed);
    }
    for offset in [meta, meta + 8, meta + 16, meta + 24] {
        let mut changed = bytes.clone();
        changed[offset..offset + 8].fill(0);
        bad.push(changed);
    }
    for end in meta - 4..bytes.len() {
        bad.push(bytes[..end].to_vec());
    }
    let calls = host.calls();
    for malformed in bad {
        let mut restored = VulkanContext::new(2, host.clone());
        assert!(restored
            .restore_object_recipes(&malformed, &budget)
            .is_err());
        assert_eq!(host.calls(), calls);
        assert_eq!(restored.object_count(), 0);
    }
    h.renderer.ctx_destroy(CTX);
    h.renderer.ctx_destroy(2);
}
