use super::*;
use crate::protocol::{BLOB_FLAG_USE_MAPPABLE, BLOB_MEM_HOST3D};
use crate::venus::executor::harness::RecordingWindow;
use crate::venus::shmem::PageBudget;
use std::sync::atomic::{AtomicUsize, Ordering};

struct Factory {
    pages: Arc<RingPages>,
    handle: SharedHandle,
    handle_size: u64,
    fail_prepare: bool,
    prepared: Arc<AtomicUsize>,
    forgotten: Arc<AtomicUsize>,
}
impl Factory {
    fn new(value: u64) -> Self {
        Self {
            pages: Arc::new(RingPages::new(4096).unwrap()),
            handle: SharedHandle(Arc::new(value)),
            handle_size: 4096,
            fail_prepare: false,
            prepared: Arc::new(AtomicUsize::new(0)),
            forgotten: Arc::new(AtomicUsize::new(0)),
        }
    }
}
impl SinkFactory for Factory {
    type Sink = CaptureSink;
    fn sink_for(&mut self, _: u32, _: u64) -> io::Result<CaptureSink> {
        Ok(CaptureSink::new())
    }
    fn validate_checkpoint_blob(&self, backing: &ExportedMemory, size: u64) -> Result<(), String> {
        let actual = match backing {
            ExportedMemory::Pages(page) => page.mapped_len(),
            ExportedMemory::Handle(_) => self.handle_size,
        };
        if size != 0 && size <= actual {
            Ok(())
        } else {
            Err("fixture allocation is too small".into())
        }
    }
    fn export_memory(&mut self, _: u32, blob: u64, _: u64) -> Result<ExportedMemory, String> {
        Ok(if blob == 42 {
            ExportedMemory::Pages(self.pages.clone())
        } else {
            ExportedMemory::Handle(self.handle.clone())
        })
    }
    fn prepare_scanout(&mut self, _: &ScanoutTarget) -> Result<(), String> {
        self.prepared.fetch_add(1, Ordering::Relaxed);
        if self.fail_prepare {
            Err("injected scanout preparation failure".into())
        } else {
            Ok(())
        }
    }
    fn forget_scanout(&mut self, _: u32) {
        self.forgotten.fetch_add(1, Ordering::Relaxed);
    }
}

fn renderer(factory: Factory) -> (VenusRenderer<Factory>, Arc<RecordingWindow>, Arc<Quiesce>) {
    let gate = Quiesce::new();
    let window = Arc::new(RecordingWindow::default());
    let mut renderer = VenusRenderer::new(factory);
    renderer.set_quiesce(gate.clone());
    renderer.set_host_visible(window.clone());
    (renderer, window, gate)
}

fn blob(resource_id: u32, blob_id: u64) -> ResourceCreateBlob {
    ResourceCreateBlob {
        resource_id,
        blob_id,
        blob_mem: BLOB_MEM_HOST3D,
        blob_flags: BLOB_FLAG_USE_MAPPABLE,
        nr_entries: 0,
        size: 4096,
    }
}

fn no_image(_: &SavedScanoutImage, _: &SharedHandle) -> Result<ScanoutImage, RecipeError> {
    panic!("page-only checkpoint has no native image associations")
}

fn page_fixture() -> (BlobCheckpointSet, HostPageTable, Vec<u8>, SharedPayloads) {
    let (mut source, _, gate) = renderer(Factory::new(100));
    source.ctx_create(1, crate::CAPSET_VENUS, "owner").unwrap();
    let mem = Arc::new(virtio_core::testing::guest_memory(4096));
    for resource in [1, 2] {
        source
            .create_blob(1, &blob(resource, 42), &mem, &[])
            .unwrap();
        source
            .map_blob(resource, u64::from(resource - 1) * 4096, 4096)
            .unwrap();
    }
    source
        .sinks
        .pages
        .write_bytes(0, &[1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16])
        .unwrap();
    source
        .scanout_blob(
            1,
            &ScanoutBlobSpec {
                format: crate::FORMAT_B8G8R8A8_UNORM,
                width: 2,
                height: 2,
                stride: 8,
                offset: 0,
            },
        )
        .unwrap();
    gate.pause();
    let mut pages = HostPageTable::default();
    let mut payloads = SharedPayloads::default();
    let saved = source.checkpoint_blobs(&mut pages, &mut payloads).unwrap();
    let mut bytes = Vec::new();
    pages.save(&mut bytes).unwrap();
    (saved, pages, bytes, payloads)
}

#[test]
fn frontend_names_publications_and_scanout_must_match_native_manifest() {
    let (saved, _, _, _) = page_fixture();
    let mut table = crate::blob::BlobTable::new(saved.host_visible_bytes);
    let support = crate::blob::BlobSupport {
        guest: true,
        host3d: true,
        host_visible_bytes: Some(saved.host_visible_bytes),
        host_mapped: true,
    };
    for native in &saved.blobs {
        let args = blob(native.resource, native.blob_id);
        let len = table.validate(&args, support, &[]).unwrap();
        table.insert(&args, &[], len).unwrap();
        if let Some((offset, _)) = native.publication {
            table.reserve_mapping(native.resource, offset).unwrap();
            table.commit_mapping(native.resource, offset);
        }
        if let Some((spec, _)) = native.scanout {
            table.set_scanout_spec(native.resource, spec);
        }
    }
    saved.validate_frontend(&table).unwrap();
    let bytes = table.save_checkpoint().unwrap();
    let checkpoint = crate::blob::checkpoint::BlobCheckpoint::decode(&bytes, support).unwrap();
    saved.validate_frontend(checkpoint.table()).unwrap();
    table.unmap(2).unwrap();
    assert!(saved.validate_frontend(&table).is_err());
    table.reserve_mapping(2, 8192).unwrap();
    table.commit_mapping(2, 8192);
    assert!(saved.validate_frontend(&table).is_err());
    table.unmap(2).unwrap();
    table.reserve_mapping(2, 4096).unwrap();
    table.commit_mapping(2, 4096);
    saved.validate_frontend(&table).unwrap();
    let mut wrong = BlobCheckpointSet::decode(&saved.encode().unwrap()).unwrap();
    wrong.blobs[0].blob_id += 1;
    assert!(wrong.validate_frontend(&table).is_err());
    wrong = BlobCheckpointSet::decode(&saved.encode().unwrap()).unwrap();
    wrong.blobs[0].scanout.as_mut().unwrap().0.width += 1;
    assert!(wrong.validate_frontend(&table).is_err());
    table.remove(2).unwrap();
    assert!(saved.validate_frontend(&table).is_err());
}

#[test]
fn blob_maps_keep_guest_offsets_with_new_pages_and_preserve_aliases_and_scanout() {
    let (saved, old_pages, bytes, payloads) = page_fixture();
    let encoded = saved.encode().unwrap();
    let saved = BlobCheckpointSet::decode(&encoded).unwrap();
    let budget = PageBudget::new(4096);
    let pages = HostPageTable::restore(&bytes, &budget).unwrap();
    assert_eq!(pages.pages().len(), 1);
    let (mut restored, window, gate) = renderer(Factory::new(200));
    gate.pause();
    restored
        .restore_blobs(&saved, &pages, &payloads, no_image)
        .unwrap();
    assert_eq!(window.count(), 2);
    for (_, offset, len) in saved.mappings() {
        let (addr, actual_len) = window.at(offset).unwrap();
        assert_eq!(actual_len, len);
        assert_eq!(addr, pages.page(1).unwrap().host_addr());
        assert_ne!(addr, old_pages.page(1).unwrap().host_addr());
    }
    assert_eq!(restored.memory_blobs, 2);
    assert_eq!(restored.memory_blob_use[&1], 2);
    assert_eq!(restored.shm_blobs, 0);
    let mut pixels = Vec::new();
    restored
        .read_scanout(
            1,
            Rect {
                x: 0,
                y: 0,
                width: 2,
                height: 2,
            },
            &mut pixels,
        )
        .unwrap();
    assert_eq!(pixels, (1..=16).collect::<Vec<u8>>());
    let imported = restored.directory.for_context(1).memory(2).unwrap();
    assert!(Arc::ptr_eq(
        imported.backing.pages().unwrap(),
        pages.page(1).unwrap()
    ));
    drop(imported);
    drop(restored);
    assert_eq!(
        window.count(),
        0,
        "all restored GPA publications unmap on teardown"
    );
    drop(pages);
    assert_eq!(budget.used(), 0);
}

#[test]
fn malformed_blob_metadata_is_rejected_before_mapping_any_pages() {
    let (saved, pages, _, payloads) = page_fixture();
    let bytes = saved.encode().unwrap();
    for len in 0..bytes.len() {
        assert!(
            BlobCheckpointSet::decode(&bytes[..len]).is_err(),
            "truncation {len}"
        );
    }
    let dir_len = u32::from_le_bytes(bytes[44..48].try_into().unwrap()) as usize;
    let count_at = 48 + dir_len.next_multiple_of(4);
    let blob_at = count_at + 4;
    for (at, word) in [
        (0, 2_u32),
        (44, u32::MAX),
        (count_at, u32::MAX),
        (blob_at, 0),
        (blob_at + 16, 0),
        (blob_at + 24, 3),
        (blob_at + 28, 2),
        (blob_at + 48, 2),
    ] {
        let mut bad = bytes.clone();
        bad[at..at + 4].copy_from_slice(&word.to_le_bytes());
        assert!(BlobCheckpointSet::decode(&bad).is_err(), "corruption {at}");
    }
    let (mut restored, window, gate) = renderer(Factory::new(200));
    gate.pause();
    let mut saved = BlobCheckpointSet::decode(&bytes).unwrap();
    saved.blobs[0].owner = 3;
    assert!(restored
        .restore_blobs(&saved, &pages, &payloads, no_image)
        .is_err());
    assert_eq!(window.count(), 0);
    assert!(restored.blobs.is_empty());
    saved.blobs[0].owner = 1;
    saved.blobs[1].publication = saved.blobs[0].publication;
    assert!(restored
        .restore_blobs(&saved, &pages, &payloads, no_image)
        .is_err());
    assert_eq!(window.count(), 0);
}

#[test]
fn failed_scanout_preparation_rolls_back_prior_aperture_publications() {
    let (mut source, _, gate) = renderer(Factory::new(100));
    source.ctx_create(1, crate::CAPSET_VENUS, "owner").unwrap();
    let mem = Arc::new(virtio_core::testing::guest_memory(4096));
    source.create_blob(1, &blob(1, 42), &mem, &[]).unwrap();
    source.map_blob(1, 0, 4096).unwrap();
    source.create_blob(1, &blob(2, 43), &mem, &[]).unwrap();
    let canonical = CanonicalImage {
        format: 44,
        flags: 8,
        usage: 3,
        view_formats: vec![44, 50],
        width: 2,
        height: 2,
    };
    let alive = Arc::new(());
    source.directory.for_context(1).record_scanout_image(
        &SharedRef::of(&source.sinks.handle),
        ScanoutImage {
            ctx_id: 1,
            image: 30,
            offset: 0,
            canonical: canonical.clone(),
            alive: Arc::downgrade(&alive),
        },
    );
    source
        .scanout_blob(
            2,
            &ScanoutBlobSpec {
                format: crate::FORMAT_B8G8R8A8_UNORM,
                width: 2,
                height: 2,
                stride: canonical.layout().row_pitch as u32,
                offset: 0,
            },
        )
        .unwrap();
    gate.pause();
    let mut pages = HostPageTable::default();
    let mut payloads = SharedPayloads::default();
    let saved = source.checkpoint_blobs(&mut pages, &mut payloads).unwrap();
    let fresh_payloads =
        SharedPayloads::from_handles(vec![SharedHandle(Arc::new(200_u64))]).unwrap();
    let mut factory = Factory::new(201);
    factory.fail_prepare = true;
    let prepared = factory.prepared.clone();
    let forgotten = factory.forgotten.clone();
    let (mut restored, window, gate) = renderer(factory);
    gate.pause();
    let new_alive = Arc::new(());
    assert!(restored
        .restore_blobs(&saved, &pages, &fresh_payloads, |saved, payload| {
            assert_eq!(*payload.0.downcast_ref::<u64>().unwrap(), 200);
            Ok(ScanoutImage {
                ctx_id: saved.ctx_id,
                image: saved.image,
                offset: saved.offset,
                canonical: saved.canonical.clone(),
                alive: Arc::downgrade(&new_alive),
            })
        })
        .is_err());
    assert_eq!(prepared.load(Ordering::Relaxed), 1);
    assert_eq!(forgotten.load(Ordering::Relaxed), 1);
    assert_eq!(
        window.count(),
        0,
        "earlier publications must be rolled back"
    );
    assert!(restored.blobs.is_empty());
    assert!(restored.directory.with(|state| state.blobs.is_empty()));
    assert_eq!(restored.memory_blobs, 0);
    drop(alive);
    assert!(
        source.checkpoint_blobs(&mut pages, &mut payloads).is_err(),
        "capture must refuse an orphan accepted scanout before committing a file"
    );
    assert!(source.blobs[&1].publication.is_some());
}

#[test]
fn a_valid_manifest_cannot_claim_more_than_the_reconstructed_opaque_allocation() {
    let mut factory = Factory::new(100);
    factory.handle_size = 8192;
    let (mut source, _, gate) = renderer(factory);
    source.ctx_create(1, crate::CAPSET_VENUS, "owner").unwrap();
    let mem = Arc::new(virtio_core::testing::guest_memory(4096));
    source.create_blob(1, &blob(1, 42), &mem, &[]).unwrap();
    source.map_blob(1, 0, 4096).unwrap();
    let mut large = blob(2, 43);
    large.size = 8192;
    source.create_blob(1, &large, &mem, &[]).unwrap();
    gate.pause();
    let mut pages = HostPageTable::default();
    let mut payloads = SharedPayloads::default();
    let bytes = source
        .checkpoint_blobs(&mut pages, &mut payloads)
        .unwrap()
        .encode()
        .unwrap();
    let saved = BlobCheckpointSet::decode(&bytes).unwrap();
    let payloads = SharedPayloads::from_handles(vec![SharedHandle(Arc::new(200_u64))]).unwrap();
    // Directory and blob metadata agree on 8192; the new backend allocation
    // is only 4096. Comparing two file fields cannot establish its real span.
    let (mut restored, window, gate) = renderer(Factory::new(201));
    gate.pause();
    assert!(restored
        .restore_blobs(&saved, &pages, &payloads, no_image)
        .is_err());
    assert_eq!(window.count(), 0);
    assert!(restored.blobs.is_empty());
}
