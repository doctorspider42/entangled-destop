use super::*;
use crate::venus::executor::{fake::FakeVulkan, harness as h};
use std::io::Cursor;

fn source() -> (Arc<FakeVulkan>, h::Harness<FakeVulkan>, Arc<Quiesce>) {
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
    gate.pause();
    assert!(gate.wait_until_idle(Duration::from_secs(5)));
    (host, source, gate)
}

fn frontend(
    renderer: &VenusRenderer<ExecutorFactory<FakeVulkan>>,
) -> (
    crate::renderer::checkpoint::ContextCheckpoint,
    crate::blob::BlobTable,
) {
    let mut gpu = crate::renderer::Gpu3d::new(Box::new(crate::NullRenderer::with_venus()));
    for id in renderer.contexts.keys() {
        gpu.ctx_create(*id, crate::CAPSET_VENUS, "fixture").unwrap();
    }
    let mut table = crate::blob::BlobTable::new(renderer.host_visible_bytes);
    let support = renderer.blob_support();
    for (resource, blob) in &renderer.blobs {
        let args = crate::protocol::ResourceCreateBlob {
            resource_id: *resource,
            blob_mem: crate::protocol::BLOB_MEM_HOST3D,
            blob_flags: crate::protocol::BLOB_FLAG_USE_MAPPABLE,
            nr_entries: 0,
            blob_id: blob.blob_id,
            size: blob.size,
        };
        let len = table.validate(&args, support, &[]).unwrap();
        table.insert(&args, &[], len).unwrap();
        if let Some(publication) = &blob.publication {
            table
                .reserve_mapping(*resource, publication.offset())
                .unwrap();
            table.commit_mapping(*resource, publication.offset());
        }
        if let Some(scanout) = &blob.scanout {
            table.set_scanout_spec(*resource, scanout.spec);
        }
    }
    (gpu.checkpoint_contexts().unwrap(), table)
}

#[test]
fn boxed_renderer_seam_streams_checked_native_state_and_adopts_frontend_once() {
    let (host, source, _) = source();
    let (contexts, blobs) = frontend(&source.renderer);
    let mut gpu = crate::renderer::Gpu3d::new(Box::new(source.renderer));
    gpu.restore_checkpoint_contexts(&contexts).unwrap();
    let mut bytes = Vec::new();
    let saved_contexts = gpu
        .save_checkpoint(&mut bytes, &blobs, Duration::from_secs(5))
        .unwrap();
    assert_eq!(saved_contexts.encode().unwrap(), contexts.encode().unwrap());
    assert!(
        gpu.enable_checkpoints(8 << 20).is_err(),
        "late enable must not waive lost history"
    );
    gpu.reset();
    assert_eq!(host.live_objects(), 0);
    let gate = Quiesce::new();
    gate.pause();
    let window = Arc::new(h::RecordingWindow::default());
    let mut native = VenusRenderer::new(ExecutorFactory::new(host.clone()));
    native.set_quiesce(gate);
    native.set_host_visible(window.clone());
    let mut cold = crate::renderer::Gpu3d::new(Box::new(native));
    assert!(cold.enable_checkpoints(0).is_err());
    assert!(cold.enable_checkpoints((64 << 20) + 1).is_err());
    cold.enable_checkpoints(8 << 20).unwrap();
    cold.load_checkpoint(
        &mut Cursor::new(&bytes),
        &contexts,
        &blobs,
        Duration::from_secs(5),
    )
    .unwrap();
    assert_eq!(cold.context_count(), 2);
    assert_eq!(host.live_objects(), 2);
    assert_eq!(window.count(), 2);
    assert!(cold
        .load_checkpoint(
            &mut Cursor::new(&bytes),
            &contexts,
            &blobs,
            Duration::from_secs(5)
        )
        .is_err());
    assert_eq!(
        host.live_objects(),
        2,
        "a non-cold load must leave the running reconstruction intact"
    );
    let mut second = Vec::new();
    cold.save_checkpoint(&mut second, &blobs, Duration::from_secs(5))
        .unwrap();
    cold.reset();
    assert_eq!(host.live_objects(), 0);
    assert_eq!(window.count(), 0);
    let mut unsupported = crate::renderer::Gpu3d::new(Box::new(crate::NullRenderer::with_venus()));
    assert!(unsupported.enable_checkpoints(8 << 20).is_err());
}

#[test]
fn checked_archive_rejects_frontend_disagreement_before_output_or_native_reconstruction() {
    let (host, mut source, gate) = source();
    let (contexts, mut blobs) = frontend(&source.renderer);
    let mut bytes = Vec::new();
    source
        .renderer
        .save_checkpoint_archive_with_frontend(
            &mut bytes,
            Duration::from_secs(5),
            &contexts,
            &blobs,
        )
        .unwrap();
    blobs.unmap(h::REPLY_RES).unwrap();
    let mut failed = Vec::new();
    assert!(source
        .renderer
        .save_checkpoint_archive_with_frontend(
            &mut failed,
            Duration::from_secs(5),
            &contexts,
            &blobs,
        )
        .is_err());
    assert!(failed.is_empty());
    gate.resume();
    source.call(&h::enumerate_instance_version()).unwrap();
    source.renderer.reset();
    assert_eq!(host.live_objects(), 0);
    let gate = Quiesce::new();
    gate.pause();
    let mut cold = VenusRenderer::new(ExecutorFactory::new(host.clone()));
    cold.set_quiesce(gate);
    let window = Arc::new(h::RecordingWindow::default());
    cold.set_host_visible(window.clone());
    let recipes = PageBudget::new(8 << 20);
    assert!(cold
        .load_checkpoint_archive_with_frontend(
            &mut Cursor::new(&bytes),
            &recipes,
            Duration::from_secs(5),
            &contexts,
            &blobs,
        )
        .is_err());
    assert_eq!(recipes.used(), 0);
    assert_eq!(host.live_objects(), 0);
    assert_eq!(window.count(), 0);
    assert_eq!(cold.live_threads(), 0);
    blobs.reserve_mapping(h::REPLY_RES, 0).unwrap();
    blobs.commit_mapping(h::REPLY_RES, 0);
    let mut bad_contexts = contexts.encode().unwrap();
    bad_contexts[8..12].copy_from_slice(&3_u32.to_le_bytes());
    bad_contexts[16..20].copy_from_slice(&4_u32.to_le_bytes());
    let bad_contexts =
        crate::renderer::checkpoint::ContextCheckpoint::decode(&bad_contexts).unwrap();
    assert!(cold
        .load_checkpoint_archive_with_frontend(
            &mut Cursor::new(&bytes),
            &recipes,
            Duration::from_secs(5),
            &bad_contexts,
            &blobs,
        )
        .is_err());
    assert_eq!(host.live_objects(), 0);
    assert_eq!(window.count(), 0);
    cold.load_checkpoint_archive_with_frontend(
        &mut Cursor::new(bytes),
        &recipes,
        Duration::from_secs(5),
        &contexts,
        &blobs,
    )
    .unwrap();
    assert_eq!(host.live_objects(), 2);
    assert_eq!(window.count(), 2);
    cold.reset();
    assert_eq!(host.live_objects(), 0);
    assert_eq!(window.count(), 0);
}

#[test]
fn one_stream_restores_mappings_native_contexts_and_workers_and_can_save_again() {
    let (host, mut source, gate) = source();
    let mut bytes = Vec::new();
    source
        .renderer
        .save_checkpoint_archive(&mut bytes, Duration::from_secs(5))
        .unwrap();
    gate.resume();
    source.call(&h::enumerate_instance_version()).unwrap();
    source.renderer.reset();
    drop(source);
    assert_eq!(host.live_objects(), 0);
    let gate = Quiesce::new();
    gate.pause();
    let window = Arc::new(h::RecordingWindow::default());
    let mut cold = VenusRenderer::new(ExecutorFactory::new(host.clone()));
    cold.set_quiesce(gate);
    cold.set_host_visible(window.clone());
    cold.load_checkpoint_archive(
        &mut Cursor::new(bytes),
        &PageBudget::new(8 << 20),
        Duration::from_secs(5),
    )
    .unwrap();
    assert_eq!(host.live_objects(), 2);
    assert_eq!(window.count(), 2);
    assert_eq!(cold.live_threads(), 2);
    let mut second = Vec::new();
    cold.save_checkpoint_archive(&mut second, Duration::from_secs(5))
        .unwrap();
    assert!(!second.is_empty());
    cold.reset();
    assert_eq!(host.live_objects(), 0);
    assert_eq!(window.count(), 0);
    assert_eq!(cold.live_threads(), 0);
}

#[test]
fn broken_archive_framing_and_missing_parts_never_leave_a_partial_renderer() {
    let (host, mut source, _) = source();
    let mut bytes = Vec::new();
    source
        .renderer
        .save_checkpoint_archive(&mut bytes, Duration::from_secs(5))
        .unwrap();
    source.renderer.reset();
    drop(source);
    let entries = format::read_index(&mut Cursor::new(&bytes)).unwrap();
    let mut bad = (0..48)
        .chain([bytes.len() / 2, bytes.len() - 1])
        .map(|len| bytes[..len].to_vec())
        .collect::<Vec<_>>();
    for at in [0, 8, 12, 16, 20, 24] {
        let mut changed = bytes.clone();
        changed[at..at + 4].fill(0xff);
        bad.push(changed);
    }
    let mut duplicate = bytes.clone();
    let second_header = entries[1].offset as usize - 16;
    duplicate[second_header..second_header + 4]
        .copy_from_slice(&(Kind::Metadata as u32).to_le_bytes());
    bad.push(duplicate);
    let mut trailing = bytes.clone();
    trailing.push(0);
    bad.push(trailing);
    let mut missing = bytes;
    let last = entries.last().unwrap();
    missing.truncate(last.offset as usize - 16);
    missing[12..16].copy_from_slice(&((entries.len() - 1) as u32).to_le_bytes());
    bad.push(missing);
    for bytes in bad {
        let gate = Quiesce::new();
        gate.pause();
        let window = Arc::new(h::RecordingWindow::default());
        let mut cold = VenusRenderer::new(ExecutorFactory::new(host.clone()));
        cold.set_quiesce(gate);
        cold.set_host_visible(window.clone());
        assert!(cold
            .load_checkpoint_archive(
                &mut Cursor::new(bytes),
                &PageBudget::new(8 << 20),
                Duration::from_secs(5)
            )
            .is_err());
        assert_eq!(host.live_objects(), 0);
        assert_eq!(window.count(), 0);
        assert_eq!(cold.live_threads(), 0);
    }
}

#[test]
fn a_stream_write_failure_preserves_the_source_session() {
    struct Broken;
    impl Write for Broken {
        fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
            Err(std::io::Error::other("simulated full snapshot disk"))
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let (host, mut source, gate) = source();
    assert!(source
        .renderer
        .save_checkpoint_archive(&mut Broken, Duration::from_secs(5))
        .is_err());
    assert_eq!(host.live_objects(), 2);
    gate.resume();
    source.call(&h::enumerate_instance_version()).unwrap();
    source.renderer.reset();
    assert_eq!(host.live_objects(), 0);
}

#[test]
fn part_views_cannot_escape_and_writers_cannot_overrun_component_lengths() {
    let parts = [
        Part {
            kind: Kind::Metadata,
            id: 0,
            len: 4,
        },
        Part {
            kind: Kind::Pages,
            id: 0,
            len: 4,
        },
    ];
    let mut bytes = Vec::new();
    format::write_header(&mut bytes, &parts).unwrap();
    format::write_part(&mut bytes, parts[0], |out| {
        out.write_all(b"meta")?;
        Ok(())
    })
    .unwrap();
    format::write_part(&mut bytes, parts[1], |out| {
        out.write_all(b"data")?;
        Ok(())
    })
    .unwrap();
    let mut cursor = Cursor::new(bytes);
    let entries = format::read_index(&mut cursor).unwrap();
    let mut part = PartReader::new(&mut cursor, entries[0]);
    let mut contents = Vec::new();
    part.read_to_end(&mut contents).unwrap();
    assert_eq!(contents, b"meta");
    assert!(part.seek(std::io::SeekFrom::End(1)).is_err());
    assert!(part.seek(std::io::SeekFrom::Current(i64::MIN)).is_err());
    assert_eq!(part.read(&mut [0; 1]).unwrap(), 0);
    let mut out = Vec::new();
    assert!(format::write_part(&mut out, parts[0], |out| {
        out.write_all(b"too long")?;
        Ok(())
    })
    .is_err());
    assert_eq!(out.len(), 16);
    assert!(format::write_part(&mut Vec::new(), parts[0], |out| {
        out.write_all(b"few")?;
        Ok(())
    })
    .is_err());
}
