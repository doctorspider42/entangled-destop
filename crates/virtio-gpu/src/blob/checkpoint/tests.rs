use super::*;
use std::sync::{Arc, Mutex};
use virtio_core::{ShmAccessError, ShmBacking};

fn support() -> BlobSupport {
    BlobSupport {
        guest: true,
        host3d: true,
        host_visible_bytes: Some(1 << 20),
        host_mapped: false,
    }
}
fn add(table: &mut BlobTable, id: u32, guest: bool) {
    let entries = if guest {
        vec![MemEntry {
            addr: 4096,
            length: 8192,
        }]
    } else {
        vec![]
    };
    let args = ResourceCreateBlob {
        resource_id: id,
        blob_mem: if guest {
            BLOB_MEM_GUEST
        } else {
            BLOB_MEM_HOST3D
        },
        blob_flags: BLOB_FLAG_USE_MAPPABLE,
        blob_id: u64::from(id) + 100,
        size: 8192,
        nr_entries: entries.len() as u32,
    };
    let len = table.validate(&args, support(), &entries).unwrap();
    table.insert(&args, &entries, len).unwrap();
}
fn fixture() -> BlobTable {
    let mut table = BlobTable::new(1 << 20);
    add(&mut table, 5, false);
    add(&mut table, 2, true);
    table.reserve_mapping(5, 16384).unwrap();
    table.commit_mapping(5, 16384);
    table.set_scanout_spec(
        5,
        ScanoutBlobSpec {
            format: crate::FORMAT_B8G8R8X8_UNORM,
            width: 32,
            height: 32,
            stride: 128,
            offset: 1024,
        },
    );
    table
}

struct Backing(Mutex<Vec<u8>>);
impl ShmBacking for Backing {
    fn len(&self) -> u64 {
        1 << 20
    }
    fn read(&self, _: u64, _: &mut [u8]) -> Result<(), ShmAccessError> {
        panic!("metadata reads no backing")
    }
    fn write(&self, _: u64, _: &[u8]) -> Result<(), ShmAccessError> {
        panic!("metadata writes no backing")
    }
    fn fill(&self, offset: u64, len: u64, value: u8) -> Result<(), ShmAccessError> {
        self.0.lock().unwrap()[offset as usize..(offset + len) as usize].fill(value);
        Ok(())
    }
}

#[test]
fn restored_logical_mapping_preserves_bytes_and_still_prevents_collisions() {
    let source = fixture();
    let bytes = source.save_checkpoint().unwrap();
    let checkpoint = BlobCheckpoint::decode(&bytes, support()).unwrap();
    assert_eq!(
        checkpoint.table().get(2).unwrap().backing(),
        &[MemEntry {
            addr: 4096,
            length: 8192
        }]
    );
    let backing = Arc::new(Backing(Mutex::new(vec![0xa7; 1 << 20])));
    let mut target = BlobTable::new(1 << 20);
    let erased: Arc<dyn ShmBacking> = backing.clone();
    assert!(target.window_mut().set_backing(erased.clone()));
    checkpoint.restore(&mut target).unwrap();
    assert!(Arc::ptr_eq(target.window().backing().unwrap(), &erased));
    assert!(
        backing.0.lock().unwrap().iter().all(|byte| *byte == 0xa7),
        "restore must never zero restored content"
    );
    assert_eq!(target.save_checkpoint().unwrap(), bytes);
    assert!(target.reserve_mapping(2, 16384).is_err());
    assert_eq!(target.unmap(5).unwrap(), 16384);
    target.reserve_mapping(2, 16384).unwrap();
    target.commit_mapping(2, 16384);
    assert!(
        backing.0.lock().unwrap()[16384..24576]
            .iter()
            .all(|byte| *byte == 0),
        "a new reservation still clears old contents"
    );
    assert_eq!(source.get(5).unwrap().mapped_at(), Some(16384));
}

#[test]
fn malformed_blob_checkpoint_never_changes_source_or_cold_target() {
    let source = fixture();
    let bytes = source.save_checkpoint().unwrap();
    for len in 0..bytes.len() {
        assert!(
            BlobCheckpoint::decode(&bytes[..len], support()).is_err(),
            "truncation {len}"
        );
    }
    for (offset, word) in [
        (0, 2),
        (12, u32::MAX),
        (16, 0),
        (16 + 8, u32::MAX),
        (16 + 12, MAX_BLOB_ENTRIES + 1),
        (16 + 40, 2),
        (16 + 44, 7), // absent scanout must have canonical zero fields
        (16 + RESOURCE_BYTES + 12 + 40, 2),
        (16 + RESOURCE_BYTES + 12 + 44, 7),
    ] {
        let mut bad = bytes.clone();
        bad[offset..offset + 4].copy_from_slice(&word.to_le_bytes());
        assert!(
            BlobCheckpoint::decode(&bad, support()).is_err(),
            "bad word {offset}"
        );
    }
    let mut trailing = bytes.clone();
    trailing.push(0);
    assert!(BlobCheckpoint::decode(&trailing, support()).is_err());
    let mut bad = bytes.clone();
    bad[16 + RESOURCE_BYTES..16 + RESOURCE_BYTES + 8].copy_from_slice(&u64::MAX.to_le_bytes());
    assert!(
        BlobCheckpoint::decode(&bad, support()).is_err(),
        "backing overflow"
    );
    assert_eq!(source.save_checkpoint().unwrap(), bytes);
    let mut target = BlobTable::new(8192);
    assert!(BlobCheckpoint::decode(&bytes, support())
        .unwrap()
        .restore(&mut target)
        .is_err());
    assert!(target.is_empty());
    assert_eq!(target.window().mapping_count(), 0);
    let mut occupied = fixture();
    assert!(BlobCheckpoint::decode(&bytes, support())
        .unwrap()
        .restore(&mut occupied)
        .is_err());
    assert_eq!(occupied.save_checkpoint().unwrap(), bytes);
}

#[test]
fn duplicate_ids_overlap_changed_policy_and_unmappable_publications_are_refused() {
    let mut source = BlobTable::new(1 << 20);
    add(&mut source, 1, false);
    add(&mut source, 2, false);
    source.reserve_mapping(1, 0).unwrap();
    source.commit_mapping(1, 0);
    source.reserve_mapping(2, 8192).unwrap();
    source.commit_mapping(2, 8192);
    let bytes = source.save_checkpoint().unwrap();
    for (offset, data) in [
        (HEADER_BYTES + RESOURCE_BYTES, 1_u64.to_le_bytes().to_vec()),
        (
            HEADER_BYTES + RESOURCE_BYTES + 32,
            0_u64.to_le_bytes().to_vec(),
        ),
        (HEADER_BYTES + 8, 0_u32.to_le_bytes().to_vec()),
    ] {
        let mut bad = bytes.clone();
        bad[offset..offset + data.len()].copy_from_slice(&data);
        assert!(BlobCheckpoint::decode(&bad, support()).is_err());
    }
    assert!(BlobCheckpoint::decode(
        &bytes,
        BlobSupport {
            host3d: false,
            ..support()
        }
    )
    .is_err());
    assert!(BlobCheckpoint::decode(
        &bytes,
        BlobSupport {
            host_visible_bytes: None,
            ..support()
        }
    )
    .is_err());
}
