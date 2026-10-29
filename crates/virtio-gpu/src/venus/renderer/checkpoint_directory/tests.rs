use super::*;

fn no_image(_: &SavedScanoutImage, _: &SharedHandle) -> Result<ScanoutImage, RecipeError> {
    panic!("page-only directory must not resolve an image")
}

fn handle(value: u64) -> SharedHandle {
    SharedHandle(Arc::new(value))
}

fn image(ctx_id: u32, id: u64, alive: &Arc<()>) -> ScanoutImage {
    ScanoutImage {
        ctx_id,
        image: id,
        offset: 0,
        canonical: CanonicalImage {
            format: 44,
            flags: 8,
            view_formats: vec![44, 50],
            usage: 3,
            width: 2,
            height: 2,
        },
        alive: Arc::downgrade(alive),
    }
}

#[test]
fn directory_file_preserves_page_identity_permissions_and_stale_generations() {
    let source = BlobDirectory::default();
    let page = Arc::new(RingPages::new(4096).unwrap());
    let mapped_bytes = page.mapped_len();
    page.write_bytes(16, b"saved bytes").unwrap();
    source.insert(8, 1, page.clone());
    let stale = source.for_context(1).bind(8, 0, 100).unwrap();
    source.insert(8, 1, page.clone());
    source.insert(9, 0, page.clone());
    source.insert_memory(10, 1, ExportedMemory::Pages(page.clone()), 4096);
    source.attach(10, 2, true);
    let live = source.for_context(1).bind(8, 16, 32).unwrap();
    let next_generation = source.with(|state| state.next_generation);
    let mut pages = HostPageTable::default();
    let mut payloads = SharedPayloads::default();
    let metadata = source
        .checkpoint(&mut pages, &mut payloads)
        .unwrap()
        .encode()
        .unwrap();
    assert_eq!(pages.pages().len(), 1);
    let mut data = Vec::new();
    pages.save(&mut data).unwrap();
    drop((source, pages, page));
    let budget = super::super::super::shmem::PageBudget::new(mapped_bytes);
    let pages = HostPageTable::restore(&data, &budget).unwrap();
    let record = DirectoryCheckpoint::decode(&metadata).unwrap();
    let directory = BlobDirectory::from_checkpoint(&record, &pages, &payloads, no_image).unwrap();
    let blobs = directory.for_context(1);
    assert_eq!(blobs.check_saved_reply(live, 16, 32), Ok(()));
    assert_eq!(
        blobs.check_saved_reply(stale, 0, 100),
        Err(ReplyBlobError::Gone(8))
    );
    assert_eq!(blobs.read(8, 16, 11).unwrap(), b"saved bytes");
    assert!(matches!(
        directory.for_context(2).bind(8, 0, 1),
        Err(ReplyBlobError::Foreign { .. })
    ));
    assert!(directory.for_context(2).bind(9, 0, 1).is_ok());
    assert!(matches!(
        directory.for_context(3).memory(10),
        Err(ReplyBlobError::Foreign { .. })
    ));
    let imported = directory.for_context(2).memory(10).unwrap().backing;
    assert!(Arc::ptr_eq(
        imported.pages().unwrap(),
        pages.page(1).unwrap()
    ));
    directory.with(|state| {
        assert!(Arc::ptr_eq(
            state.blobs[&8].pages().unwrap(),
            state.blobs[&9].pages().unwrap()
        ));
        assert_eq!(state.next_generation, next_generation);
    });
    directory.insert(11, 1, pages.page(1).unwrap().clone());
    assert_eq!(
        blobs.bind(11, 0, 1).unwrap().generation,
        next_generation + 1
    );
    assert_eq!(
        budget.used(),
        4096,
        "three blobs and an import share one charge"
    );
    drop((imported, blobs, directory, pages));
    assert_eq!(budget.used(), 0);
}

#[test]
fn handle_alias_index_live_images_and_pending_bindings_survive_new_payloads() {
    let source = BlobDirectory::default();
    let original = handle(100);
    let pending = handle(101);
    let live = Arc::new(());
    let pending_live = Arc::new(());
    source.insert_memory(10, 1, ExportedMemory::Handle(original.clone()), 4096);
    source.insert_memory(11, 1, ExportedMemory::Handle(original.clone()), 4096);
    source.attach(11, 2, true);
    source
        .for_context(1)
        .record_scanout_image(&SharedRef::of(&original), image(1, 30, &live));
    source
        .for_context(1)
        .record_release(&SharedRef::of(&original), 6, 0xffff_fffd);
    source
        .for_context(1)
        .record_scanout_image(&SharedRef::of(&pending), image(1, 31, &pending_live));
    let mut pages = HostPageTable::default();
    let mut payloads = SharedPayloads::default();
    assert_eq!(payloads.register(&original).unwrap(), 1);
    assert_eq!(payloads.register(&pending).unwrap(), 2);
    let metadata = source
        .checkpoint(&mut pages, &mut payloads)
        .unwrap()
        .encode()
        .unwrap();
    drop((source, payloads, original, pending, live, pending_live));
    let fresh = SharedPayloads::from_handles(vec![handle(200), handle(201)]).unwrap();
    let record = DirectoryCheckpoint::decode(&metadata).unwrap();
    let mut tokens = Vec::new();
    let directory = BlobDirectory::from_checkpoint(&record, &pages, &fresh, |saved, payload| {
        assert_eq!(
            *payload.0.downcast_ref::<u64>().unwrap(),
            if saved.image == 30 { 200 } else { 201 }
        );
        let alive = Arc::new(());
        let restored = image(saved.ctx_id, saved.image, &alive);
        tokens.push(alive);
        Ok(restored)
    })
    .unwrap();
    directory.with(|state| {
        assert_eq!(
            state.by_handle.get(&fresh.handle(1).unwrap().key()),
            Some(&11)
        );
        assert_eq!(state.blobs[&10].memory.as_ref().unwrap().images.len(), 0);
        let memory = state.blobs[&11].memory.as_ref().unwrap();
        assert_eq!(memory.images.len(), 1);
        assert_eq!(
            memory.release,
            Some(ScanoutRelease {
                layout: 6,
                family: 0xffff_fffd
            })
        );
        assert!(memory.images[0].is_alive());
        assert_eq!(state.pending.len(), 1);
    });
    let imported = directory.for_context(2).memory(11).unwrap();
    let ExportedMemory::Handle(imported) = imported.backing else {
        panic!()
    };
    assert!(SharedRef::of(&imported).is(fresh.handle(1).unwrap()));
    assert!(matches!(
        directory.for_context(3).memory(11),
        Err(ReplyBlobError::Foreign { .. })
    ));
    // A delayed blob publication adopts the restored pending image association.
    directory.insert_memory(
        12,
        1,
        ExportedMemory::Handle(fresh.handle(2).unwrap().clone()),
        4096,
    );
    directory.with(|state| {
        assert!(state.pending.is_empty());
        assert_eq!(
            state.blobs[&12].memory.as_ref().unwrap().images[0].image,
            31
        );
    });
    drop(tokens);
    directory
        .with(|state| assert!(!state.blobs[&11].memory.as_ref().unwrap().images[0].is_alive()));
}

#[test]
fn corrupt_directory_bytes_are_rejected_without_resolving_native_objects() {
    let source = BlobDirectory::default();
    source.insert(8, 1, Arc::new(RingPages::new(4096).unwrap()));
    let mut pages = HostPageTable::default();
    let mut payloads = SharedPayloads::default();
    let bytes = source
        .checkpoint(&mut pages, &mut payloads)
        .unwrap()
        .encode()
        .unwrap();
    for length in 0..bytes.len() {
        assert!(
            DirectoryCheckpoint::decode(&bytes[..length]).is_err(),
            "truncated at {length}"
        );
    }
    for (at, word) in [
        (0, 2_u32),
        (12, u32::MAX),
        (16, 0),
        (24, 0),
        (32, 2),
        (36, 0),
        (40, 1),
        (44, 2),
    ] {
        let mut bad = bytes.clone();
        bad[at..at + 4].copy_from_slice(&word.to_le_bytes());
        assert!(
            DirectoryCheckpoint::decode(&bad).is_err(),
            "corruption at {at}"
        );
    }
    let mut trailing = bytes.clone();
    trailing.push(0);
    assert!(DirectoryCheckpoint::decode(&trailing).is_err());
    let mut saved = DirectoryCheckpoint::decode(&bytes).unwrap();
    saved.entries[0].generation += 1;
    assert!(saved.validate().is_err());
    saved.entries[0].generation -= 1;
    saved.entries[0].root = 100;
    assert!(BlobDirectory::from_checkpoint(&saved, &pages, &payloads, no_image).is_err());
    // The original binding is still writable after all failed reconstructions.
    let blobs = source.for_context(1);
    blobs.write(blobs.bind(8, 0, 1).unwrap(), 0, b"x").unwrap();
}

#[test]
fn roots_are_checked_before_image_resolution_and_changed_tokens_are_refused() {
    let source = BlobDirectory::default();
    let payload = handle(100);
    let alive = Arc::new(());
    source.insert_memory(10, 1, ExportedMemory::Handle(payload.clone()), 4096);
    source
        .for_context(1)
        .record_scanout_image(&SharedRef::of(&payload), image(1, 30, &alive));
    source.insert(11, 1, Arc::new(RingPages::new(4096).unwrap()));
    let mut pages = HostPageTable::default();
    let mut payloads = SharedPayloads::default();
    let mut saved = source.checkpoint(&mut pages, &mut payloads).unwrap();
    let mut calls = 0;
    saved.entries[1].root = 1000;
    assert!(
        BlobDirectory::from_checkpoint(&saved, &pages, &payloads, |_, _| {
            calls += 1;
            panic!("an invalid root must be refused before native-context resolution")
        })
        .is_err()
    );
    assert_eq!(calls, 0);
    saved.entries[1].root = 1;
    assert!(
        BlobDirectory::from_checkpoint(&saved, &pages, &payloads, |_, _| {
            Ok(image(2, 30, &alive))
        })
        .is_err()
    );
    assert!(
        BlobDirectory::from_checkpoint(&saved, &pages, &payloads, |_, _| {
            Ok(image(1, 30, &Arc::new(())))
        })
        .is_err(),
        "dead image tokens must not be revived"
    );
}

#[test]
fn empty_directory_roundtrips() {
    let source = BlobDirectory::default();
    let pages = &mut HostPageTable::default();
    let payloads = &mut SharedPayloads::default();
    let bytes = source
        .checkpoint(pages, payloads)
        .unwrap()
        .encode()
        .unwrap();
    let saved = DirectoryCheckpoint::decode(&bytes).unwrap();
    let restored = BlobDirectory::from_checkpoint(&saved, pages, payloads, no_image).unwrap();
    assert_eq!(
        restored
            .checkpoint(pages, payloads)
            .unwrap()
            .encode()
            .unwrap(),
        bytes
    );
}

#[test]
fn duplicate_resources_generations_and_import_permissions_are_refused() {
    let source = BlobDirectory::default();
    let page = Arc::new(RingPages::new(4096).unwrap());
    source.insert(8, 1, page.clone());
    source.insert(9, 1, page.clone());
    let mut pages = HostPageTable::default();
    let mut payloads = SharedPayloads::default();
    let bytes = source
        .checkpoint(&mut pages, &mut payloads)
        .unwrap()
        .encode()
        .unwrap();
    for (at, other, len) in [(48, 16, 4), (56, 24, 8)] {
        let mut bad = bytes.clone();
        bad[at..at + len].copy_from_slice(&bytes[other..other + len]);
        assert!(DirectoryCheckpoint::decode(&bad).is_err());
    }
    source.insert_memory(10, 1, ExportedMemory::Pages(page), 4096);
    source.attach(10, 2, true);
    let mut saved = source.checkpoint(&mut pages, &mut payloads).unwrap();
    let memory = saved.entries.last_mut().unwrap().memory.as_mut().unwrap();
    memory.attached.push(2);
    assert!(saved.encode().is_err());
    saved
        .entries
        .last_mut()
        .unwrap()
        .memory
        .as_mut()
        .unwrap()
        .attached = vec![0];
    assert!(saved.encode().is_err());
}

#[test]
fn directory_aliases_cannot_bypass_the_total_blob_byte_cap() {
    let source = BlobDirectory::default();
    let payload = handle(100);
    for id in 1..=9 {
        source.insert_memory(
            id,
            1,
            ExportedMemory::Handle(payload.clone()),
            MAX_BLOB_BYTES,
        );
    }
    let mut pages = HostPageTable::default();
    let mut payloads = SharedPayloads::default();
    let saved = source.checkpoint(&mut pages, &mut payloads).unwrap();
    assert!(matches!(
        BlobDirectory::from_checkpoint(&saved, &pages, &payloads, no_image),
        Err(RecipeError::Budget)
    ));
}

#[test]
fn an_absent_handle_index_is_preserved_instead_of_guessing_an_alias() {
    let source = BlobDirectory::default();
    let payload = handle(100);
    source.insert_memory(10, 1, ExportedMemory::Handle(payload), 4096);
    // An alias can have lost its reverse index after another alias was removed.
    source.with(|state| state.by_handle.clear());
    let mut pages = HostPageTable::default();
    let mut payloads = SharedPayloads::default();
    let saved = source.checkpoint(&mut pages, &mut payloads).unwrap();
    let restored = BlobDirectory::from_checkpoint(&saved, &pages, &payloads, no_image).unwrap();
    restored.with(|state| {
        assert_eq!(state.blobs.len(), 1);
        assert!(state.by_handle.is_empty());
    });
}
