//! Fuzzes the sharpest surface Venus added (VEN-2003, ADR-0004): a
//! **guest-chosen offset turning into a hypervisor mapping of memory the VMM
//! does not own**.
//!
//! Phase 2's window was device-backed — one hypervisor mapping of pages the
//! VMM allocated, and the guest's offsets only ever indexed *inside* it, which
//! `gpu_blob` fuzzes. A Venus window is the other shape: nothing is mapped
//! until a blob is, and then the host asks its hypervisor to put a
//! renderer-owned span at `BAR base + region offset + guest offset`. Three
//! guest-controlled values are multiplied into one address, and a mistake is
//! not an out-of-bounds read but a *mapping* — host memory at an address the
//! guest picked, overlapping whatever else is there.
//!
//! So the invariants are re-derived from **outside**, against what the
//! hypervisor was actually told, never against the bookkeeping that is
//! supposed to maintain it:
//!
//! * every live range lies inside the window's current placement;
//! * no two live ranges overlap;
//! * nothing at all is mapped while the BAR decodes nothing, or while it
//!   decodes somewhere the machine refuses to follow;
//! * the count never passes [`MAX_HOST_RANGES`], because each one is a
//!   hypervisor object (a KVM memory slot) and the pool is finite;
//! * a second region's offsets can never reach the first region's span.
//!
//! The ranges handed in are real host pages (`HostShmRegion`), so a bug that
//! got as far as a real hypervisor would have been given a real address —
//! there is no sentinel here that a checked path could be accidentally
//! passing.

#![no_main]

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use arbitrary::Arbitrary;
use libfuzzer_sys::fuzz_target;
use machine_x86::shm::{plan, ShmWindow};
use virtio_core::{ShmBacking, ShmRegion};
use vmm_core::shm::{GpaMapper, HostRange, HostShmRegion, SharedWindow, MAX_HOST_RANGES};

const PAGE: u64 = 4096;
/// Guest RAM the fuzzed machine claims to have. Fixes the 64-bit aperture,
/// which is what decides whether a BAR base is one the machine will follow.
const GUEST_BYTES: u64 = 2048 << 20;
/// The two regions the fuzzed device declares. Deliberately two, because a
/// second region is the case where a region-relative offset can be mistaken
/// for a window-relative one.
const REGION_ONE: u64 = 64 * PAGE;
const REGION_TWO: u64 = 32 * PAGE;
/// Renderer pages the fuzzer hands out sub-ranges of.
const RENDERER_BYTES: u64 = 16 * PAGE;

#[derive(Debug, Arbitrary)]
enum Op {
    /// The guest programs the BAR — anywhere, including on top of its own RAM.
    Follow { base: Option<u64> },
    /// …or somewhere inside the aperture, which is the interesting half.
    FollowInAperture { page: u16 },
    /// A blob mapping at an arbitrary region offset and length.
    Map {
        region: bool,
        offset: u64,
        pages: u8,
        renderer_page: u8,
    },
    /// A blob mapping at a page-aligned offset, so the fuzzer reaches the
    /// overlap logic instead of bouncing off the alignment check.
    MapAligned {
        region: bool,
        page: u8,
        pages: u8,
        renderer_page: u8,
    },
    Unmap { region: bool, offset: u64 },
    UnmapAligned { region: bool, page: u8 },
    /// Host access to a renderer-mapped window, which must always be refused.
    HostAccess { region: bool, offset: u64, len: u16 },
}

#[derive(Debug, Arbitrary)]
struct Case {
    ops: Vec<Op>,
}

/// A hypervisor that records what it was told, and refuses an overlap exactly
/// as KVM and WHP do.
#[derive(Default)]
struct FuzzMapper {
    live: Mutex<BTreeMap<u64, (u64, u64)>>,
}

impl FuzzMapper {
    fn snapshot(&self) -> BTreeMap<u64, (u64, u64)> {
        self.live.lock().expect("mapper").clone()
    }
}

impl GpaMapper for FuzzMapper {
    fn map_range(&self, gpa: u64, range: HostRange) -> Result<(), vmm_core::hv::HvError> {
        let mut live = self.live.lock().expect("mapper");
        let end = gpa
            .checked_add(range.len())
            .expect("a mapping that wraps the address space must never be asked for");
        for (at, (_, len)) in live.iter() {
            if *at == gpa {
                continue; // a replace, which both hypervisors allow
            }
            let other_end = at.saturating_add(*len);
            assert!(
                end <= *at || other_end <= gpa,
                "the host asked its hypervisor to map {gpa:#x}+{} over the live {at:#x}+{}",
                range.len(),
                len
            );
        }
        live.insert(gpa, (range.addr(), range.len()));
        Ok(())
    }

    fn unmap_range(&self, gpa: u64, _len: u64) -> Result<(), vmm_core::hv::HvError> {
        self.live.lock().expect("mapper").remove(&gpa);
        Ok(())
    }

    fn backend(&self) -> &'static str {
        "fuzz"
    }
}

fuzz_target!(|case: Case| {
    let regions = [
        ShmRegion {
            id: 1,
            len: REGION_ONE,
            host_mapped: true,
        },
        ShmRegion {
            id: 2,
            len: REGION_TWO,
            host_mapped: true,
        },
    ];
    let (bar_size, placements) = plan(&regions).expect("a fixed, valid plan");
    let mapper = Arc::new(FuzzMapper::default());
    let shared = Arc::new(
        SharedWindow::new_host_mapped(bar_size, mapper.clone()).expect("host-mapped window"),
    );
    let aperture_base = machine_x86::layout::pci_mmio64_base(GUEST_BYTES);
    let window = ShmWindow::new(
        shared,
        bar_size,
        placements.clone(),
        aperture_base,
        GUEST_BYTES,
    )
    .expect("window");
    let backings: Vec<Arc<dyn ShmBacking>> = vec![
        window.backing_for(1).expect("region 1"),
        window.backing_for(2).expect("region 2"),
    ];
    // Real host pages, so nothing on the fuzzed path is a sentinel value.
    let renderer = HostShmRegion::new(RENDERER_BYTES).expect("renderer pages");
    let renderer_base = renderer.range().addr();

    // What the harness believes is mapped, independently of the window.
    // (region index, region offset) -> length.
    let mut live: BTreeMap<(usize, u64), u64> = BTreeMap::new();

    let span_of = |index: usize, offset: u64, len: u64| -> Option<(u64, u64)> {
        let placement = placements.get(index)?;
        let end = offset.checked_add(len)?;
        (end <= placement.len).then(|| (placement.offset + offset, len))
    };

    for op in case.ops.into_iter().take(96) {
        match op {
            Op::Follow { base } => {
                window.follow(base);
            }
            Op::FollowInAperture { page } => {
                window.follow(Some(aperture_base + u64::from(page) * PAGE));
            }
            Op::Map {
                region,
                offset,
                pages,
                renderer_page,
            } => {
                let index = usize::from(region);
                let len = u64::from(pages) * PAGE;
                let at = u64::from(renderer_page % 16) * PAGE;
                let addr = renderer_base + at;
                // Only a span that fits the renderer's own pages may be
                // offered: the promise `HostRange::new` carries is the
                // caller's, and a fuzzer that broke it would be fuzzing
                // nothing but its own bug.
                if at + len > RENDERER_BYTES || len == 0 {
                    continue;
                }
                // SAFETY: `addr`..+`len` is inside `renderer`, which outlives
                // every mapping made from it (the window is dropped first).
                if unsafe { backings[index].map_host(offset, addr, len) }.is_ok() {
                    live.insert((index, offset), len);
                }
            }
            Op::MapAligned {
                region,
                page,
                pages,
                renderer_page,
            } => {
                let index = usize::from(region);
                let offset = u64::from(page) * PAGE;
                let len = u64::from(pages) * PAGE;
                let at = u64::from(renderer_page % 16) * PAGE;
                if at + len > RENDERER_BYTES || len == 0 {
                    continue;
                }
                // SAFETY: as above.
                if unsafe { backings[index].map_host(offset, renderer_base + at, len) }.is_ok() {
                    live.insert((index, offset), len);
                }
            }
            Op::Unmap { region, offset } => {
                let index = usize::from(region);
                backings[index].unmap_host(offset);
                live.remove(&(index, offset));
            }
            Op::UnmapAligned { region, page } => {
                let index = usize::from(region);
                let offset = u64::from(page) * PAGE;
                backings[index].unmap_host(offset);
                live.remove(&(index, offset));
            }
            Op::HostAccess {
                region,
                offset,
                len,
            } => {
                let index = usize::from(region);
                let mut buf = vec![0u8; usize::from(len)];
                assert!(
                    backings[index].read(offset, &mut buf).is_err(),
                    "a renderer-mapped window must never hand the device bytes \
                     the guest is not reading"
                );
                assert!(backings[index].write(offset, &buf).is_err());
                assert!(backings[index].fill(offset, u64::from(len), 0).is_err());
                assert!(backings[index].host_mapped());
            }
        }

        // ---- the invariants, re-derived from what the hypervisor was told.
        let hv = mapper.snapshot();
        assert!(
            live.len() <= MAX_HOST_RANGES,
            "{} live ranges is past the {MAX_HOST_RANGES}-slot pool",
            live.len()
        );
        match window.placed_at() {
            None => assert!(
                hv.is_empty(),
                "the BAR decodes nothing but {} ranges are still mapped",
                hv.len()
            ),
            Some(base) => {
                assert_eq!(
                    hv.len(),
                    live.len(),
                    "the hypervisor and the harness disagree about how many ranges are live \
                     (base {base:#x}, hv {hv:#x?}, live {live:#x?})"
                );
                let mut expected: BTreeMap<u64, u64> = BTreeMap::new();
                for ((index, offset), len) in &live {
                    let (window_offset, len) = span_of(*index, *offset, *len)
                        .expect("a mapping the window accepted must fit its region");
                    let gpa = base + window_offset;
                    assert!(
                        gpa >= base && gpa + len <= base + bar_size,
                        "a range at {gpa:#x}+{len} leaves the BAR at {base:#x}+{bar_size}"
                    );
                    assert!(
                        expected.insert(gpa, len).is_none(),
                        "two live mappings claim the same guest address {gpa:#x}"
                    );
                    let (_, mapped_len) = hv
                        .get(&gpa)
                        .unwrap_or_else(|| panic!("nothing is mapped at {gpa:#x}"));
                    assert_eq!(*mapped_len, len);
                }
                // …and nothing the harness does not know about.
                for at in hv.keys() {
                    assert!(
                        expected.contains_key(at),
                        "the hypervisor holds a mapping at {at:#x} nobody asked for"
                    );
                }
            }
        }
    }

    // Teardown order is the invariant that matters most: the window must give
    // every range back before the renderer's pages go away. Every `Arc` on the
    // window has to go, not just the `ShmWindow` — each region's backing holds
    // one too, which is the point of the ownership note in `machine_x86::shm`.
    drop(backings);
    drop(window);
    assert!(
        mapper.snapshot().is_empty(),
        "dropping the window left the hypervisor pointing at renderer memory"
    );
    drop(renderer);
});
