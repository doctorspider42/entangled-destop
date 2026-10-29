use super::*;
use crate::venus::executor::fake::FakeVulkan;
use crate::venus::executor::harness as h;

fn captured() -> (RingCheckpointSet, HostPageTable) {
    let gate = Quiesce::new();
    let mut harness = h::Harness::gated(Arc::new(FakeVulkan::standard()), gate.clone());
    h::boot(&mut harness);
    harness.submit_virtqueue_seqno(19).unwrap();
    harness
        .renderer
        .ctx_create(2, crate::CAPSET_VENUS, "empty connection")
        .unwrap();
    gate.pause();
    assert!(gate.wait_until_idle(Duration::from_secs(5)));
    let mut pages = HostPageTable::default();
    let saved = harness
        .renderer
        .checkpoint_rings(&mut pages, Duration::from_secs(5))
        .unwrap();
    assert_eq!(saved.contexts[0].rings[0].seqno, 19);
    assert_eq!(saved.contexts[0].rings[0].worker.sink.len(), 64);
    let ring = &saved.contexts[0].rings[0];
    assert_eq!(ring.info.resource_id, h::RING_RES);
    let original = harness.renderer.blob_pages(h::RING_RES).unwrap();
    assert!(Arc::ptr_eq(&original, pages.page(ring.page).unwrap()));
    let pump = crate::venus::pump::RingPump::from_checkpoint(
        RingLayout::new(ring.info, original.resource_len()).unwrap(),
        original.as_ref(),
        &ring.worker.pump,
    )
    .unwrap();
    assert_eq!(pump.cursor(), harness.head());
    let mut data = Vec::new();
    pages.save(&mut data).unwrap();
    // Teardown may reset the original control words. Reload independent pages
    // from the file so subsequent checks cannot rely on the saving process.
    drop(harness);
    let pages =
        HostPageTable::restore(&data, &crate::venus::shmem::PageBudget::new(2 << 20)).unwrap();
    let ring = &saved.contexts[0].rings[0];
    let page = pages.page(ring.page).unwrap();
    assert!(crate::venus::pump::RingPump::from_checkpoint(
        RingLayout::new(ring.info, page.resource_len()).unwrap(),
        page.as_ref(),
        &ring.worker.pump,
    )
    .is_ok());
    (saved, pages)
}

#[test]
fn renderer_captures_live_workers_and_contexts_without_rings() {
    let (saved, _) = captured();
    assert_eq!(saved.contexts.len(), 2);
    assert_eq!(saved.contexts[0].ctx_id, 1);
    assert_eq!(saved.contexts[1].ctx_id, 2);
    assert!(saved.contexts[1].rings.is_empty());
    let bytes = saved.encode().unwrap();
    let decoded = RingCheckpointSet::decode(&bytes).unwrap();
    assert_eq!(decoded.encode().unwrap(), bytes);
}

#[test]
fn malformed_ring_manifests_are_refused_before_any_worker_is_started() {
    let (saved, _) = captured();
    let bytes = saved.encode().unwrap();
    for len in 0..bytes.len() {
        assert!(
            RingCheckpointSet::decode(&bytes[..len]).is_err(),
            "truncation {len}"
        );
    }
    for (at, word) in [
        (0, 2_u32),
        (4, u32::MAX),
        (8, 0),
        (12, u32::MAX),
        (16, 2),
        (28, u32::MAX),
        (40, 0),
        (44, 2),
        (60, 0),
        (120, 3),
        (168, u32::MAX),
    ] {
        let mut bad = bytes.clone();
        bad[at..at + 4].copy_from_slice(&word.to_le_bytes());
        assert!(RingCheckpointSet::decode(&bad).is_err(), "corruption {at}");
    }
    let mut bad = bytes.clone();
    bad[20..28].copy_from_slice(&u64::MAX.to_le_bytes());
    assert!(RingCheckpointSet::decode(&bad).is_err());
    let mut bad = bytes;
    bad.push(0);
    assert!(RingCheckpointSet::decode(&bad).is_err());
}

#[test]
fn relative_layouts_keep_their_base_flags_and_nonempty_extra_region() {
    let info = RingCreateInfo {
        flags: 0xdead_beef,
        resource_id: 7,
        offset: 64,
        size: 512,
        idle_timeout_ns: u64::MAX,
        head_offset: 0,
        tail_offset: 4,
        status_offset: 8,
        buffer_offset: 64,
        buffer_size: 128,
        extra_offset: 256,
        extra_size: 8,
    };
    let layout = RingLayout::new(info, 4096).unwrap();
    assert_eq!(layout.create_info(), info);
    let restored = RingLayout::new(layout.create_info(), 4096).unwrap();
    assert_eq!(restored, layout);
}

#[test]
fn renderer_restores_mapped_workers_reply_cursors_monitor_and_context_poison() {
    use crate::venus::executor::checkpoint_shared::SharedPayloads;
    use crate::venus::executor::ExecutorFactory;
    use crate::venus::protocol::{Command, VK_SUCCESS};
    use crate::venus::pump::{STATUS_ALIVE, STATUS_FATAL};
    use crate::venus::renderer::checkpoint_blobs::BlobCheckpointSet;
    use crate::venus::shmem::PageBudget;
    let mut source = h::Harness::monitored(Arc::new(FakeVulkan::standard()), 10_000);
    for ctx in [1, 2] {
        source.use_context(ctx);
        // No native objects are needed for this metadata fixture's version query.
        source.call(&h::enumerate_instance_version()).unwrap();
        source.submit_virtqueue_seqno(100 + u64::from(ctx)).unwrap();
    }
    source
        .renderer
        .ctx_create(3, crate::CAPSET_VENUS, "empty poisoned context")
        .unwrap();
    source.renderer.contexts.get_mut(&3).unwrap().poisoned = true;
    source.renderer.quiesce.pause();
    assert!(source
        .renderer
        .quiesce
        .wait_until_idle(Duration::from_secs(5)));
    let mut pages = HostPageTable::default();
    let mut payloads = SharedPayloads::default();
    let blob_bytes = source
        .renderer
        .checkpoint_blobs(&mut pages, &mut payloads)
        .unwrap()
        .encode()
        .unwrap();
    let ring_bytes = source
        .renderer
        .checkpoint_rings(&mut pages, Duration::from_secs(5))
        .unwrap()
        .encode()
        .unwrap();
    let mut page_bytes = Vec::new();
    pages.save(&mut page_bytes).unwrap();
    drop((source, pages));
    let pages = HostPageTable::restore(&page_bytes, &PageBudget::new(4 << 20)).unwrap();
    let blob_state = BlobCheckpointSet::decode(&blob_bytes).unwrap();
    let new_renderer = || {
        let gate = Quiesce::new();
        gate.pause();
        let window = Arc::new(h::RecordingWindow::default());
        let mut renderer =
            VenusRenderer::new(ExecutorFactory::new(Arc::new(FakeVulkan::standard())));
        renderer.set_quiesce(gate.clone());
        renderer.set_host_visible(window.clone());
        renderer
            .restore_blobs(&blob_state, &pages, &payloads, |_, _| {
                panic!("no canonical images in this metadata fixture")
            })
            .unwrap();
        (renderer, gate, window)
    };
    // A later sink with inconsistent seqno must fail before any earlier sink
    // clears shared IDLE or starts a worker.
    let mut bad = RingCheckpointSet::decode(&ring_bytes).unwrap();
    bad.contexts[1].rings[0].seqno += 1;
    let mut controls = Vec::new();
    for context in &bad.contexts {
        for ring in &context.rings {
            let page = pages.page(ring.page).unwrap();
            let layout = RingLayout::new(ring.info, page.resource_len()).unwrap();
            controls.push((ring.page, layout, page.load_host_word(&layout.status())));
        }
    }
    let (mut rejected, _, _) = new_renderer();
    assert!(rejected.restore_rings(&bad, &pages).is_err());
    assert_eq!(rejected.live.count(), 0);
    assert!(rejected.contexts.is_empty());
    for (id, layout, status) in controls {
        assert_eq!(
            pages.page(id).unwrap().load_host_word(&layout.status()),
            status
        );
    }
    drop(rejected);
    let saved = RingCheckpointSet::decode(&ring_bytes).unwrap();
    let (mut restored, gate, window) = new_renderer();
    restored.restore_rings(&saved, &pages).unwrap();
    assert!(restored.contexts[&3].poisoned);
    assert!(restored.contexts[&3].rings.is_empty());
    assert_eq!(
        restored.contexts[&1].monitor.as_ref().unwrap().period(),
        Duration::from_millis(10)
    );
    let mut replies = Vec::new();
    for context in &saved.contexts {
        for ring in &context.rings {
            let mut decoder = Decoder::new(&ring.worker.sink);
            assert_eq!(decoder.u32().unwrap(), 1);
            assert_eq!(decoder.u32().unwrap(), context.ctx_id);
            assert_eq!(decoder.u64().unwrap(), ring.ring);
            assert_eq!(decoder.u32().unwrap(), 1);
            let resource = decoder.u32().unwrap();
            let _generation = decoder.u64().unwrap();
            let offset = decoder.u64().unwrap();
            let _size = decoder.u64().unwrap();
            let cursor = decoder.u64().unwrap();
            let prefix = restored
                .directory
                .for_context(context.ctx_id)
                .read(resource, offset, cursor)
                .unwrap();
            let page = pages.page(ring.page).unwrap();
            let layout = RingLayout::new(ring.info, page.resource_len()).unwrap();
            page.clear_status_bits(&layout.status(), STATUS_ALIVE);
            let head = page.load_host_word(&layout.head());
            let command = h::call_bytes(&h::enumerate_instance_version());
            for (i, byte) in command.iter().enumerate() {
                let at = head.wrapping_add(i as u32) % layout.buffer().len() as u32;
                page.write_bytes(layout.buffer().start() + u64::from(at), &[*byte])
                    .unwrap();
            }
            let tail = head.wrapping_add(command.len() as u32);
            assert!(page.guest_store_word(layout.tail().offset(), tail));
            replies.push((
                context.ctx_id,
                ring.page,
                layout,
                tail,
                resource,
                offset,
                cursor,
                prefix,
            ));
        }
    }
    gate.resume();
    let deadline = Instant::now() + Duration::from_secs(10);
    for (ctx, id, layout, tail, resource, offset, cursor, prefix) in replies {
        let page = pages.page(id).unwrap();
        while page.load_host_word(&layout.head()) != tail {
            assert_eq!(page.load_host_word(&layout.status()) & STATUS_FATAL, 0);
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(1));
        }
        let reply = restored
            .directory
            .for_context(ctx)
            .read(resource, offset, cursor + 64)
            .unwrap();
        assert_eq!(&reply[..cursor as usize], prefix);
        let encoded = h::call_bytes(&h::enumerate_instance_version());
        let mut command = Command::decode_next(&mut Decoder::new(&encoded)).unwrap().1;
        command
            .decode_reply(&mut Decoder::new(&reply[cursor as usize..]))
            .unwrap();
        let Command::EnumerateInstanceVersion(args) = command else {
            panic!()
        };
        assert_eq!(args.ret, VK_SUCCESS);
        assert!(args.p_api_version.is_some_and(|version| version > 0));
    }
    gate.pause();
    assert!(gate.wait_until_idle(Duration::from_secs(5)));
    assert_eq!(
        restored.contexts[&1].rings[&h::RING]
            .virtqueue_seqno
            .current(),
        101
    );
    drop(restored);
    assert_eq!(window.count(), 0);
}
