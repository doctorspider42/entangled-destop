//! Fuzzes the virtio-gpu **blob resource** surface (EPIC 20, VEN-2001/2007).
//!
//! Blob resources are the largest new attack surface since 3D itself, and the
//! reason is one field: `RESOURCE_MAP_BLOB` lets the guest name the **offset
//! inside a host mapping** that its blob lands at. Everything else on the path
//! is guest-chosen too — the resource id, the memory type, the flag bits, the
//! page count, every page address and length, the blob size and the `blob_id`.
//!
//! Four layers are on the fuzzed path:
//!
//! * [`ResourceCreateBlob::parse`] and [`ResourceMapBlob::parse`] on raw bytes,
//!   including the trailing-entry walk — the guest-controlled `nr_entries`
//!   multiplication that must never wrap;
//! * [`SetScanoutBlob::parse`], whose strides and offsets are the layout the
//!   scanout path trusts;
//! * the whole [`BlobTable`] bookkeeping under an arbitrary sequence of
//!   create / map / unmap / unref, with the renderer's declared support itself
//!   fuzzed (a host with no window, a host with a tiny one, a host that serves
//!   only guest blobs);
//! * [`HostVisibleWindow`] directly, with arbitrary `(offset, size)` pairs.
//!
//! Properties asserted, not merely "does not crash":
//!
//! * no panic and no abort — allocations are bounded *before* they happen, and
//!   the target runs with overflow checks and debug assertions on;
//! * the byte budget never exceeds [`MAX_TOTAL_BLOB_BYTES`] and the resource
//!   count never exceeds [`MAX_BLOB_RESOURCES`];
//! * **no two live mappings overlap**, re-derived from scratch after every
//!   operation — the invariant that stops one guest mapping aliasing another's
//!   host memory;
//! * every live mapping lies entirely inside the window.

#![no_main]

use arbitrary::Arbitrary;
use libfuzzer_sys::fuzz_target;
use virtio_gpu::blob::{
    BlobSupport, BlobTable, HostVisibleWindow, BLOB_PAGE_SIZE, MAX_BLOB_RESOURCES,
    MAX_TOTAL_BLOB_BYTES,
};
use virtio_gpu::protocol::{
    MemEntry, ResourceCreateBlob, ResourceMapBlob, SetScanoutBlob, BLOB_MEM_GUEST,
    BLOB_MEM_HOST3D, BLOB_MEM_HOST3D_GUEST,
};

/// Window sizes the fuzzer picks between: none, one page, and something with
/// room for a few blobs.
const WINDOWS: [u64; 4] = [0, BLOB_PAGE_SIZE, 64 * BLOB_PAGE_SIZE, 1 << 30];

#[derive(Debug, Arbitrary)]
enum Op {
    Create {
        resource_id: u32,
        blob_mem: u32,
        blob_flags: u32,
        blob_id: u64,
        size: u64,
        entries: Vec<(u64, u32)>,
    },
    /// A create whose memory type is one of the three real ones, so the
    /// fuzzer spends most of its time past the first rejection.
    CreatePlausible {
        resource_id: u8,
        which_mem: u8,
        mappable: bool,
        pages: u8,
        entry_pages: u8,
    },
    Map {
        resource_id: u32,
        offset: u64,
    },
    /// A map at a page-aligned offset, again to get past the cheap rejection.
    MapAligned {
        resource_id: u8,
        page: u16,
    },
    Unmap {
        resource_id: u32,
    },
    Unref {
        resource_id: u32,
    },
    Clear,
}

#[derive(Debug, Arbitrary)]
struct Case {
    /// Raw bytes for the three wire parsers.
    raw_create: Vec<u8>,
    raw_map: Vec<u8>,
    raw_scanout: Vec<u8>,
    /// Which window size the "host" has.
    window: u8,
    /// What the renderer claims it can serve.
    guest_blobs: bool,
    host3d_blobs: bool,
    ops: Vec<Op>,
    /// Direct window reservations, unmediated by the table.
    reservations: Vec<(u32, u64, u64)>,
}

/// Recomputes the overlap invariant from the outside, so a bug in the window's
/// own neighbour check cannot hide behind itself.
fn assert_no_overlaps(window: &HostVisibleWindow, live: &[(u64, u64)]) {
    for (i, (a_start, a_len)) in live.iter().enumerate() {
        let a_end = a_start.saturating_add(*a_len);
        assert!(
            a_end <= window.len(),
            "mapping {a_start:#x}+{a_len} runs past the {} byte window",
            window.len()
        );
        for (b_start, b_len) in live.iter().skip(i + 1) {
            let b_end = b_start.saturating_add(*b_len);
            assert!(
                a_end <= *b_start || b_end <= *a_start,
                "mappings {a_start:#x}+{a_len} and {b_start:#x}+{b_len} overlap"
            );
        }
    }
}

fuzz_target!(|case: Case| {
    // ---- layer 1: the wire parsers on arbitrary bytes.
    if let Some(args) = ResourceCreateBlob::parse(&case.raw_create) {
        // The guest-controlled entry count drives two multiplications; neither
        // may wrap, and walking the entries must never index out of bounds.
        let _ = ResourceCreateBlob::total_len(args.nr_entries);
        for index in 0..args.nr_entries.min(4096) {
            let _ = ResourceCreateBlob::entry_at(&case.raw_create, index);
        }
        // The extremes of the index space, which is where a wrap would live.
        for index in [args.nr_entries, u32::MAX, u32::MAX - 1, 0] {
            let _ = ResourceCreateBlob::entry_at(&case.raw_create, index);
        }
    }
    let _ = ResourceMapBlob::parse(&case.raw_map);
    let _ = SetScanoutBlob::parse(&case.raw_scanout);

    // ---- layer 2: the window on its own.
    let window_len = WINDOWS[usize::from(case.window) % WINDOWS.len()];
    let mut window = HostVisibleWindow::new(window_len);
    let mut live: Vec<(u64, u64)> = Vec::new();
    for (id, offset, size) in case.reservations.into_iter().take(64) {
        if window.reserve(id, offset, size).is_ok() {
            live.push((offset, size));
        }
        assert_no_overlaps(&window, &live);
    }

    // ---- layers 3 and 4: the table, with the renderer's support fuzzed.
    let support = BlobSupport {
        guest: case.guest_blobs,
        host3d: case.host3d_blobs,
        host_visible_bytes: (window_len != 0).then_some(window_len),
    };
    let mut table = BlobTable::new(window_len);
    // (resource_id, size) of every blob the table accepted, so the harness can
    // recompute the window invariant without reaching inside it.
    let mut sizes: Vec<(u32, u64)> = Vec::new();
    let mut mapped: Vec<(u32, u64, u64)> = Vec::new();

    let apply_create = |table: &mut BlobTable,
                            sizes: &mut Vec<(u32, u64)>,
                            args: ResourceCreateBlob,
                            entries: Vec<MemEntry>| {
        if let Ok(backing_len) = table.validate(&args, support, &entries) {
            if table.insert(&args, &entries, backing_len).is_ok() {
                sizes.push((args.resource_id, args.size));
            }
        }
        assert!(table.len() <= MAX_BLOB_RESOURCES);
        assert!(table.total_bytes() <= MAX_TOTAL_BLOB_BYTES);
    };

    for op in case.ops.into_iter().take(64) {
        match op {
            Op::Create {
                resource_id,
                blob_mem,
                blob_flags,
                blob_id,
                size,
                entries,
            } => {
                let entries: Vec<MemEntry> = entries
                    .into_iter()
                    .take(64)
                    .map(|(addr, length)| MemEntry { addr, length })
                    .collect();
                let args = ResourceCreateBlob {
                    resource_id,
                    blob_mem,
                    blob_flags,
                    nr_entries: entries.len() as u32,
                    blob_id,
                    size,
                };
                apply_create(&mut table, &mut sizes, args, entries);
            }
            Op::CreatePlausible {
                resource_id,
                which_mem,
                mappable,
                pages,
                entry_pages,
            } => {
                let blob_mem = match which_mem % 3 {
                    0 => BLOB_MEM_GUEST,
                    1 => BLOB_MEM_HOST3D,
                    _ => BLOB_MEM_HOST3D_GUEST,
                };
                let entries: Vec<MemEntry> = if blob_mem == BLOB_MEM_HOST3D {
                    Vec::new()
                } else {
                    vec![MemEntry {
                        addr: 0x1_0000,
                        length: u32::from(entry_pages) * BLOB_PAGE_SIZE as u32,
                    }]
                };
                let args = ResourceCreateBlob {
                    resource_id: u32::from(resource_id),
                    blob_mem,
                    blob_flags: u32::from(mappable),
                    nr_entries: entries.len() as u32,
                    blob_id: u64::from(resource_id),
                    size: u64::from(pages) * BLOB_PAGE_SIZE,
                };
                apply_create(&mut table, &mut sizes, args, entries);
            }
            Op::Map {
                resource_id,
                offset,
            } => {
                if let Ok(size) = table.reserve_mapping(resource_id, offset) {
                    table.commit_mapping(resource_id, offset);
                    mapped.push((resource_id, offset, size));
                }
            }
            Op::MapAligned { resource_id, page } => {
                let id = u32::from(resource_id);
                let offset = u64::from(page) * BLOB_PAGE_SIZE;
                if let Ok(size) = table.reserve_mapping(id, offset) {
                    table.commit_mapping(id, offset);
                    mapped.push((id, offset, size));
                }
            }
            Op::Unmap { resource_id } => {
                if let Ok(offset) = table.unmap(resource_id) {
                    mapped.retain(|(id, at, _)| !(*id == resource_id && *at == offset));
                }
            }
            Op::Unref { resource_id } => {
                if let Ok(was_mapped_at) = table.remove(resource_id) {
                    sizes.retain(|(id, _)| *id != resource_id);
                    if let Some(offset) = was_mapped_at {
                        mapped.retain(|(id, at, _)| !(*id == resource_id && *at == offset));
                    }
                }
            }
            Op::Clear => {
                table.clear();
                sizes.clear();
                mapped.clear();
                assert_eq!(table.total_bytes(), 0);
                assert_eq!(table.window().mapping_count(), 0);
            }
        }

        // The bounds, after every single operation.
        assert!(table.len() <= MAX_BLOB_RESOURCES);
        assert!(table.total_bytes() <= MAX_TOTAL_BLOB_BYTES);
        assert_eq!(
            table.total_bytes(),
            sizes.iter().map(|(_, size)| *size).sum::<u64>(),
            "the byte budget must equal the sum of live blobs"
        );
        assert_eq!(
            table.window().mapping_count(),
            mapped.len(),
            "the window must hold exactly the mappings the harness believes in"
        );
        let spans: Vec<(u64, u64)> = mapped.iter().map(|(_, at, len)| (*at, *len)).collect();
        assert_no_overlaps(table.window(), &spans);
        for (id, at, _) in &mapped {
            assert_eq!(
                table.window().resource_at(*at),
                Some(*id),
                "the window must attribute each mapped page to its blob"
            );
        }
    }
});
