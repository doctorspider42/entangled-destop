use super::*;
use crate::venus::executor::{fake::FakeVulkan, harness as h};

fn captured() -> (Arc<FakeVulkan>, Vec<u8>, Vec<u8>) {
    let host = Arc::new(FakeVulkan::standard());
    let gate = Quiesce::new();
    let mut factory = ExecutorFactory::new(host.clone());
    factory
        .enable_checkpoint_recipes(&PageBudget::new(8 << 20))
        .unwrap();
    let mut source = h::Harness::gated_factory(factory, gate.clone());
    for ctx in [1, 2] {
        source.use_context(ctx);
        source.send(&h::create_instance(h::INSTANCE)).unwrap();
        source.call(&h::enumerate_instance_version()).unwrap();
        let reply = if ctx == 1 {
            h::REPLY_RES
        } else {
            1000 + ctx * 2 + 1
        };
        source
            .renderer
            .map_blob(reply, u64::from(ctx - 1) << 20, h::REPLY_BYTES)
            .unwrap();
    }
    assert!(source
        .renderer
        .checkpoint_object_graph(Duration::from_secs(5))
        .is_err());
    gate.pause();
    assert!(gate.wait_until_idle(Duration::from_secs(5)));
    let captured = source
        .renderer
        .checkpoint_object_graph(Duration::from_secs(5))
        .unwrap();
    assert!(captured.shared_images.is_empty());
    assert!(captured.payloads.handles().is_empty());
    assert_eq!(host.live_objects(), 2);
    let metadata = captured.metadata.encode().unwrap();
    let mut pages = Vec::new();
    captured.pages.save(&mut pages).unwrap();
    source.renderer.reset();
    assert_eq!(host.live_objects(), 0);
    (host, metadata, pages)
}

#[test]
fn renderer_object_graph_restores_native_objects_mappings_and_paused_workers() {
    let (host, bytes, page_bytes) = captured();
    let saved = RendererObjectCheckpoint::decode(&bytes).unwrap();
    assert_eq!(saved.encode().unwrap(), bytes);
    let mut factory = ExecutorFactory::new(host.clone());
    let recipes = PageBudget::new(8 << 20);
    factory.enable_checkpoint_recipes(&recipes).unwrap();
    let budgets = saved.resolve_budgets(&factory).unwrap();
    let pages =
        HostPageTable::restore_owned(&page_bytes, &saved.page_owners(&budgets).unwrap(), &budgets)
            .unwrap();
    let gate = Quiesce::new();
    gate.pause();
    let window = Arc::new(h::RecordingWindow::default());
    let mut renderer = VenusRenderer::new(factory);
    renderer.set_quiesce(gate.clone());
    renderer.set_host_visible(window.clone());
    let mut visited = Vec::new();
    renderer
        .restore_object_graph(
            &saved,
            &budgets,
            &pages,
            &SharedPayloads::default(),
            &recipes,
            |id, context| {
                assert_eq!(context.object_count(), 1);
                visited.push(id);
                Ok(())
            },
        )
        .unwrap();
    assert_eq!(visited, [1, 2]);
    assert_eq!(host.live_objects(), 2);
    assert_eq!(window.count(), saved.blobs.mappings().count());
    assert_eq!(window.count(), 2);
    assert_eq!(renderer.live_threads(), 2);
    assert!(gate.is_paused());
    // Capture again through the same whole-graph entry point. The restored
    // workers and factory retain their journals, owners and reply metadata.
    let second = renderer
        .checkpoint_object_graph(Duration::from_secs(5))
        .unwrap();
    assert_eq!(
        second.metadata.objects.context_ids().collect::<Vec<_>>(),
        [1, 2]
    );
    renderer.reset();
    assert_eq!(host.live_objects(), 0);
    assert_eq!(window.count(), 0);
    assert_eq!(renderer.live_threads(), 0);
}

#[test]
fn later_context_memory_failure_discards_entire_cold_graph_and_all_native_objects() {
    let (host, bytes, page_bytes) = captured();
    let saved = RendererObjectCheckpoint::decode(&bytes).unwrap();
    let factory = ExecutorFactory::new(host.clone());
    let budgets = saved.resolve_budgets(&factory).unwrap();
    let pages =
        HostPageTable::restore_owned(&page_bytes, &saved.page_owners(&budgets).unwrap(), &budgets)
            .unwrap();
    let gate = Quiesce::new();
    gate.pause();
    let window = Arc::new(h::RecordingWindow::default());
    let mut renderer = VenusRenderer::new(factory);
    renderer.set_quiesce(gate);
    renderer.set_host_visible(window.clone());
    let mut visited = Vec::new();
    let result = renderer.restore_object_graph(
        &saved,
        &budgets,
        &pages,
        &SharedPayloads::default(),
        &PageBudget::new(8 << 20),
        |id, _| {
            visited.push(id);
            if id == 2 {
                Err(invalid("simulated later private-memory upload failure"))
            } else {
                Ok(())
            }
        },
    );
    assert!(result.is_err());
    assert_eq!(visited, [1, 2]);
    assert_eq!(host.live_objects(), 0);
    assert_eq!(renderer.factory().host_objects(), 0);
    assert_eq!(renderer.live_threads(), 0);
    assert_eq!(window.count(), 0);
}

#[test]
fn renderer_object_graph_codec_rejects_broken_framing_and_context_mismatch() {
    let (_, bytes, _) = captured();
    for len in 0..bytes.len() {
        assert!(RendererObjectCheckpoint::decode(&bytes[..len]).is_err());
    }
    for (at, value) in [(0, 2_u32), (4, u32::MAX)] {
        let mut bad = bytes.clone();
        bad[at..at + 4].copy_from_slice(&value.to_le_bytes());
        assert!(RendererObjectCheckpoint::decode(&bad).is_err());
    }
    let mut trailing = bytes.clone();
    trailing.push(0);
    assert!(RendererObjectCheckpoint::decode(&trailing).is_err());
    let mut saved = RendererObjectCheckpoint::decode(&bytes).unwrap();
    saved.rings.contexts.pop();
    assert!(saved.encode().is_err());
}

#[test]
fn later_ring_failure_rolls_back_published_ranges_and_adopted_native_contexts() {
    let (host, bytes, page_bytes) = captured();
    let mut saved = RendererObjectCheckpoint::decode(&bytes).unwrap();
    saved.rings.contexts[1].rings[0].seqno += 1;
    let factory = ExecutorFactory::new(host.clone());
    let budgets = saved.resolve_budgets(&factory).unwrap();
    let pages =
        HostPageTable::restore_owned(&page_bytes, &saved.page_owners(&budgets).unwrap(), &budgets)
            .unwrap();
    let gate = Quiesce::new();
    gate.pause();
    let window = Arc::new(h::RecordingWindow::default());
    let mut renderer = VenusRenderer::new(factory);
    renderer.set_quiesce(gate);
    renderer.set_host_visible(window.clone());
    let mut visited = Vec::new();
    assert!(renderer
        .restore_object_graph(
            &saved,
            &budgets,
            &pages,
            &SharedPayloads::default(),
            &PageBudget::new(8 << 20),
            |id, context| {
                assert_eq!(context.object_count(), 1);
                visited.push(id);
                Ok(())
            }
        )
        .is_err());
    assert_eq!(visited, [1, 2]);
    assert_eq!(host.live_objects(), 0);
    assert_eq!(renderer.factory().host_objects(), 0);
    assert_eq!(renderer.live_threads(), 0);
    assert_eq!(
        window.count(),
        0,
        "failed sinks unmap every restored publication"
    );
}
