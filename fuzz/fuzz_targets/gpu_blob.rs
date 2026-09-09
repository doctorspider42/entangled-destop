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
//! * every live mapping lies entirely inside the window;
//! * **a span handed back by `reserve_mapping` is zeroed** (VEN-2001 phase 2).
//!   Since the window has real host memory behind it, a guest that maps a blob
//!   where another blob used to be must not find the other blob's bytes. The
//!   harness scribbles a canary over the whole window before every operation
//!   and then reads every live mapping back, so the property is checked against
//!   memory rather than against the bookkeeping that is supposed to maintain it;
//! * every host-side access through the backing is bounded — the fuzzer aims
//!   arbitrary `(offset, len)` pairs at it directly, including ones that would
//!   wrap in `u64`.

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
///
/// The last one is deliberately *not* backed by real memory: a gigabyte of
/// host RAM per fuzz case would make the campaign about the allocator instead
/// of about the bounds. Everything below `MAX_BACKED_BYTES` gets real pages.
const WINDOWS: [u64; 4] = [0, BLOB_PAGE_SIZE, 64 * BLOB_PAGE_SIZE, 1 << 30];

/// Largest window this target puts real host memory behind.
const MAX_BACKED_BYTES: u64 = 64 * BLOB_PAGE_SIZE;

/// The byte the harness scribbles over the whole window before each operation,
/// so a span that comes back *not* zeroed is a leak of a previous mapping.
const CANARY: u8 = 0xa5;

/// Host memory behind the window, the same shape `machine_x86::shm` supplies
/// but without a hypervisor: a plain buffer, every access bounded in `u64`
/// before it becomes an index.
struct FuzzBacking {
    bytes: std::sync::Mutex<Vec<u8>>,
}

impl FuzzBacking {
    fn new(len: u64) -> std::sync::Arc<Self> {
        std::sync::Arc::new(Self {
            bytes: std::sync::Mutex::new(vec![0u8; len as usize]),
        })
    }

    /// `[offset, offset + len)` as a `usize` range, or `None` when it leaves
    /// the buffer. The whole point of the type: no arithmetic on a
    /// guest-derived offset happens anywhere else.
    fn range(&self, len_total: usize, offset: u64, len: u64) -> Option<(usize, usize)> {
        let end = offset.checked_add(len)?;
        if end > len_total as u64 {
            return None;
        }
        Some((usize::try_from(offset).ok()?, usize::try_from(end).ok()?))
    }
}

impl virtio_core::ShmBacking for FuzzBacking {
    fn len(&self) -> u64 {
        self.bytes.lock().unwrap().len() as u64
    }

    fn read(&self, offset: u64, buf: &mut [u8]) -> Result<(), virtio_core::ShmAccessError> {
        let bytes = self.bytes.lock().unwrap();
        let (start, end) = self
            .range(bytes.len(), offset, buf.len() as u64)
            .ok_or(virtio_core::ShmAccessError {
                offset,
                len: buf.len() as u64,
                window: bytes.len() as u64,
            })?;
        buf.copy_from_slice(&bytes[start..end]);
        Ok(())
    }

    fn write(&self, offset: u64, data: &[u8]) -> Result<(), virtio_core::ShmAccessError> {
        let mut bytes = self.bytes.lock().unwrap();
        let total = bytes.len();
        let (start, end) =
            self.range(total, offset, data.len() as u64)
                .ok_or(virtio_core::ShmAccessError {
                    offset,
                    len: data.len() as u64,
                    window: total as u64,
                })?;
        bytes[start..end].copy_from_slice(data);
        Ok(())
    }

    fn fill(&self, offset: u64, len: u64, byte: u8) -> Result<(), virtio_core::ShmAccessError> {
        let mut bytes = self.bytes.lock().unwrap();
        let total = bytes.len();
        let (start, end) = self
            .range(total, offset, len)
            .ok_or(virtio_core::ShmAccessError {
                offset,
                len,
                window: total as u64,
            })?;
        bytes[start..end].fill(byte);
        Ok(())
    }
}

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
    /// Host-side accesses aimed straight at the window's backing, so the
    /// bounds are fuzzed without a device in the way.
    accesses: Vec<(u64, u16, bool)>,
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
    let backed = window_len != 0 && window_len <= MAX_BACKED_BYTES;
    let mut window = HostVisibleWindow::new(window_len);
    let bare_backing = backed.then(|| FuzzBacking::new(window_len));
    if let Some(backing) = &bare_backing {
        assert!(
            window.set_backing(backing.clone()),
            "a backing of exactly the declared length must be accepted"
        );
        assert!(window.is_backed());
    }
    // A backing of the wrong length must be refused outright: the declared
    // length is what every guest offset is checked against.
    let mut mismatched = HostVisibleWindow::new(BLOB_PAGE_SIZE);
    assert!(!mismatched.set_backing(FuzzBacking::new(2 * BLOB_PAGE_SIZE)));
    assert!(!mismatched.is_backed());

    let mut live: Vec<(u64, u64)> = Vec::new();
    for (id, offset, size) in case.reservations.into_iter().take(64) {
        if window.reserve(id, offset, size).is_ok() {
            live.push((offset, size));
        }
        assert_no_overlaps(&window, &live);
        // Clearing a span is the one host write the device makes on a guest's
        // behalf; it must be bounded exactly like the reservation was.
        let cleared = window.clear_span(offset, size).is_ok();
        if backed && size > 0 {
            let fits = offset.checked_add(size).is_some_and(|end| end <= window_len);
            assert_eq!(cleared, fits, "clear_span disagreed with the window bounds");
        }
    }

    // Host-side accesses straight at the backing, with arbitrary offsets.
    if let Some(backing) = &bare_backing {
        use virtio_core::ShmBacking as _;
        for (offset, len, write) in case.accesses.iter().copied().take(64) {
            let len = u64::from(len);
            let inside = offset.checked_add(len).is_some_and(|end| end <= window_len);
            let mut buf = vec![0u8; len as usize];
            let result = if write {
                backing.write(offset, &buf)
            } else {
                backing.read(offset, &mut buf)
            };
            assert_eq!(
                result.is_ok(),
                inside,
                "a {}-byte access at {offset:#x} of a {window_len}-byte window",
                len
            );
            assert_eq!(backing.fill(offset, len, 0).is_ok(), inside);
        }
    }

    // ---- layers 3 and 4: the table, with the renderer's support fuzzed.
    let support = BlobSupport {
        guest: case.guest_blobs,
        host3d: case.host3d_blobs,
        host_visible_bytes: (window_len != 0).then_some(window_len),
    };
    let mut table = BlobTable::new(window_len);
    let table_backing = backed.then(|| FuzzBacking::new(window_len));
    if let Some(backing) = &table_backing {
        assert!(table.window_mut().set_backing(backing.clone()));
    }
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
        // Poison every byte the guest could reach *before* the operation. A
        // mapping created below must come back zeroed anyway; anything else
        // means a span reached a guest with the previous owner's bytes in it.
        if let Some(backing) = &table_backing {
            use virtio_core::ShmBacking as _;
            backing.fill(0, window_len, CANARY).expect("the whole window");
        }
        // The span this operation created, if it created one. Only *that* span
        // is guaranteed zero: the poison above lands on every other live
        // mapping too, and those were cleared in an earlier iteration.
        let mut created: Option<(u64, u64)> = None;
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
                    created = Some((offset, size));
                }
            }
            Op::MapAligned { resource_id, page } => {
                let id = u32::from(resource_id);
                let offset = u64::from(page) * BLOB_PAGE_SIZE;
                if let Ok(size) = table.reserve_mapping(id, offset) {
                    table.commit_mapping(id, offset);
                    mapped.push((id, offset, size));
                    created = Some((offset, size));
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
        // A span this operation created must be all zeroes: `reserve_mapping`
        // clears before it returns, and the canary written at the top of the
        // iteration is what makes "still zero" mean something. Both ends of the
        // span are read, because an off-by-one in the clear would show at
        // exactly one of them.
        if let (Some(backing), Some((at, len))) = (&table_backing, created) {
            use virtio_core::ShmBacking as _;
            let window = usize::try_from(len.min(4 * BLOB_PAGE_SIZE)).unwrap();
            let mut head = vec![CANARY; window];
            backing
                .read(at, &mut head)
                .expect("a live mapping is inside the window");
            let mut tail = vec![CANARY; window];
            backing
                .read(at + len - window as u64, &mut tail)
                .expect("a live mapping is inside the window");
            assert!(
                head.iter().chain(tail.iter()).all(|b| *b == 0),
                "mapping {at:#x}+{len} was handed to the guest without being cleared"
            );
            // …and nothing outside it: clearing more than was asked for would
            // wipe a neighbouring mapping's bytes under a guest that is using
            // them.
            if at > 0 {
                let mut before = [0u8; 1];
                backing.read(at - 1, &mut before).expect("in the window");
                assert_eq!(
                    before[0], CANARY,
                    "the clear ran past the start of {at:#x}+{len}"
                );
            }
            if at + len < window_len {
                let mut after = [0u8; 1];
                backing.read(at + len, &mut after).expect("in the window");
                assert_eq!(
                    after[0], CANARY,
                    "the clear ran past the end of {at:#x}+{len}"
                );
            }
        }
        for (id, at, _) in &mapped {
            assert_eq!(
                table.window().resource_at(*at),
                Some(*id),
                "the window must attribute each mapped page to its blob"
            );
        }
    }
});
