//! Device-local memory the guest may map (ADR-0004, the 2026-09-26 amendment
//! on Firefox), against the fake host: the `DEVICE_LOCAL | HOST_VISIBLE |
//! HOST_COHERENT` type `policy::visible_vram` appends when the host shows the
//! guest none, because the BAR the host has is hidden. Zink 25.2.8 and
//! 26.0.8 map buffers they asked such memory for, and without the type they
//! got device-local memory with no pages instead.
//!
//! The fake is RTX-2070 shaped: types 0–2 device local, 3 and 4 host visible
//! and importable, 5 the BAR (device local only to the guest); heaps 0
//! (VRAM, holding the BAR too) and 1 (system memory). The appended type is
//! 6, in heap 2, and it is type 3's pages.

use std::sync::Arc;

use crate::renderer::Renderer3d;
use crate::venus::protocol::*;
use crate::venus::renderer::SinkFactory;

use super::fake::FakeVulkan;
use super::harness::*;
use super::ExecutorFactory;

const HOST_COHERENT_TYPE: u32 = 3;
const DEVICE_LOCAL_TYPE: u32 = 1;
const BAR_TYPE: u32 = 5;
/// The appended type, and its heap.
const VRAM_TYPE: u32 = 6;
const VRAM_HEAP: usize = 2;
const MAP_AT: u64 = 0x40_0000;
const MEM_RES: u32 = 20;
const SIZE: u64 = 64 << 10;
/// Zink's slab size: what the Firefox refusal was a blob of.
const SLAB: u64 = 2 << 20;

fn standard() -> (Harness<FakeVulkan>, Arc<FakeVulkan>) {
    let host = Arc::new(FakeVulkan::standard());
    let mut h = Harness::new(Arc::clone(&host));
    with_device(&mut h);
    (h, host)
}

fn told(h: &mut Harness<FakeVulkan>) -> VkPhysicalDeviceMemoryProperties {
    let Command::GetPhysicalDeviceMemoryProperties2(m) =
        h.call(&memory_properties(PHYSICAL)).expect("answered")
    else {
        panic!("wrong reply")
    };
    m.p_memory_properties.expect("filled").memory_properties
}

fn allocate_ret(h: &mut Harness<FakeVulkan>, id: u64, size: u64, ty: u32) -> i32 {
    let Command::AllocateMemory(a) = h
        .call(&allocate(DEVICE, id, size, ty, Vec::new()))
        .expect("answered")
    else {
        panic!("wrong reply")
    };
    a.ret
}

/// A transfer buffer's `memoryTypeBits`, created as [`BUFFER`] + `offset`.
fn buffer_bits(h: &mut Harness<FakeVulkan>, offset: u64) -> u32 {
    let Command::CreateBuffer(b) = h
        .call(&create_buffer(
            DEVICE,
            BUFFER + offset,
            buffer_info(SIZE, TRANSFER),
        ))
        .expect("buffer")
    else {
        panic!("wrong reply")
    };
    assert_eq!(b.ret, VK_SUCCESS);
    let Command::GetBufferMemoryRequirements2(r) = h
        .call(&buffer_requirements(DEVICE, BUFFER + offset))
        .expect("reqs")
    else {
        panic!("wrong reply")
    };
    r.p_memory_requirements
        .expect("filled")
        .memory_requirements
        .memory_type_bits
}

#[test]
fn the_guest_is_shown_device_local_memory_it_may_map_in_a_heap_of_its_own() {
    let (mut h, _) = standard();
    let m = told(&mut h);
    assert_eq!((m.memory_type_count, m.memory_heap_count), (7, 3));
    let flags: Vec<u32> = m.memory_types[..7]
        .iter()
        .map(|t| t.property_flags)
        .collect();
    assert_eq!(
        flags,
        vec![0x1, 0x1, 0x1, 0x6, 0xe, 0x1, 0x7],
        "the host's six as before, and DEVICE_LOCAL | HOST_VISIBLE | HOST_COHERENT last"
    );
    let vram = &m.memory_types[VRAM_TYPE as usize];
    assert_eq!(vram.heap_index as usize, VRAM_HEAP);
    let heap = &m.memory_heaps[VRAM_HEAP];
    assert_eq!(heap.flags, 0x1, "a device-local heap");
    assert_eq!(
        heap.size,
        super::policy::VISIBLE_VRAM_HEAP_BYTES.min(super::MAX_HOST_VISIBLE_BYTES_PER_CONTEXT),
        "512 MiB, never more than the host-visible share it is charged to"
    );
    // Vulkan's ordering rule: a type whose flags are a strict subset of
    // another's comes first. Nothing earlier is a strict superset of a
    // later type.
    for (i, a) in flags.iter().enumerate() {
        for b in &flags[i + 1..] {
            assert!(
                !(a & b == *b && a != b),
                "{a:#x} before {b:#x} breaks the ordering rule"
            );
        }
    }
    // Zink's backstop counts the new device-local heap, the one thing about
    // it the guest's flush arithmetic sees.
    assert!(super::policy::zink_flush_threshold(&m) > 0);
}

#[test]
fn device_local_memory_the_guest_maps_is_our_pages_imported_as_type_3() {
    let (mut h, host) = standard();
    let bits = buffer_bits(&mut h, 0);
    assert_eq!(
        bits, 0x7f,
        "a buffer that may take type 3's pages may take type 6's"
    );
    assert_eq!(allocate_ret(&mut h, MEMORY, SIZE, VRAM_TYPE), VK_SUCCESS);
    assert_eq!(
        host.allocations(),
        vec![(HOST_COHERENT_TYPE, SIZE, true)],
        "the host imported our pages as type 3: it has no type 6"
    );
    h.send(&bind_buffers(DEVICE, &[(BUFFER, MEMORY, 0)]))
        .expect("bind");
    assert!(!h.fatal());
    assert_eq!(host.buffer_binds().len(), 1);
    // Charged to the host-visible share; no device-local byte.
    assert_eq!(h.renderer.factory().host_visible_bytes(), SIZE);
    assert_eq!(h.renderer.factory().usage().limits.device_local_bytes, 0);

    // `vkMapMemory`: the blob is those pages, and the window shows them.
    h.memory_blob(CTX, MEM_RES, MEMORY, SIZE)
        .expect("a blob of device-local memory the guest may map");
    h.renderer
        .map_blob(MEM_RES, MAP_AT, SIZE)
        .expect("mapped into the window");
    let blob = h.renderer.blob_pages(MEM_RES).expect("the blob's pages");
    let import = host.imports().pop().expect("the import");
    assert_eq!(blob.host_addr(), import.addr);
    assert_eq!(h.window.at(MAP_AT), Some((import.addr, SIZE)));
    let pattern: Vec<u8> = (0..4096u32).map(|i| (i * 13 + 1) as u8).collect();
    blob.write_bytes(0x2000, &pattern).expect("inside");
    let mut seen = vec![0u8; pattern.len()];
    import
        .pages
        .upgrade()
        .expect("alive")
        .read_bytes(0x2000, &mut seen)
        .expect("inside");
    assert_eq!(seen, pattern, "what the guest writes, the GPU reads");
    drop(blob);

    // Freed while mapped: the mapping keeps the pages, and the budget with
    // them, until the blob goes.
    h.send(&free(DEVICE, MEMORY)).expect("free");
    assert_eq!(h.renderer.factory().host_visible_bytes(), SIZE);
    h.renderer.destroy_blob(MEM_RES);
    assert_eq!(h.renderer.factory().host_visible_bytes(), 0);
    assert!(!h.fatal());
}

/// The Firefox refusal, kept: a 2 MiB slab of plain device-local memory —
/// or of the BAR, which the guest sees as device local only — has no pages,
/// so its `vkMapMemory` blob is refused, and the ring goes on.
#[test]
fn device_local_memory_that_is_not_host_visible_still_has_no_blob() {
    let (mut h, host) = standard();
    for (id, ty) in [(MEMORY, DEVICE_LOCAL_TYPE), (MEMORY + 1, BAR_TYPE)] {
        assert_eq!(allocate_ret(&mut h, id, SLAB, ty), VK_SUCCESS);
    }
    assert!(host.imports().is_empty());
    for (res, id) in [(MEM_RES, MEMORY), (MEM_RES + 1, MEMORY + 1)] {
        h.memory_blob(CTX, res, id, SLAB)
            .expect_err("no pages to map");
    }
    assert_eq!(h.renderer.factory().host_visible_bytes(), 0);
    assert!(!h.fatal(), "a refused blob is not a ring fault");
}

#[test]
fn a_resource_that_cannot_take_our_pages_never_sees_the_visible_vram_type() {
    let (mut h, host) = standard();
    // An optimal image: the fake driver imports host memory for linear
    // images only.
    h.call(&create_image(DEVICE, IMAGE, image_info()))
        .expect("image");
    let Command::GetImageMemoryRequirements2(r) =
        h.call(&memory_requirements(DEVICE, IMAGE)).expect("reqs")
    else {
        panic!("wrong reply")
    };
    let bits = r
        .p_memory_requirements
        .expect("filled")
        .memory_requirements
        .memory_type_bits;
    assert_eq!(bits & (1 << VRAM_TYPE), 0, "render targets stay in VRAM");
    assert_eq!(allocate_ret(&mut h, MEMORY, 1 << 20, VRAM_TYPE), VK_SUCCESS);
    let head = h
        .call(&bind_image(DEVICE, IMAGE, MEMORY, 0))
        .expect_err("outside its bits");
    assert_eq!(head, h.last_start);
    assert!(h.fatal());
    assert!(host.image_binds().is_empty());

    // A buffer the driver will not import host memory for.
    let (mut h, host) = standard();
    host.buffer_imports
        .store(false, std::sync::atomic::Ordering::SeqCst);
    let bits = buffer_bits(&mut h, 0);
    assert_eq!(bits & 0x58, 0, "neither our pages nor their visible VRAM");
}

#[test]
fn the_visible_vram_heap_is_the_host_visible_share_and_refuses_with_it() {
    const SHARE: u64 = 16 * SIZE;
    let host = Arc::new(FakeVulkan::standard());
    let mut h = Harness::with_factory(ExecutorFactory::with_budgets(
        Arc::clone(&host),
        4 * SHARE,
        SHARE,
    ));
    with_device(&mut h);
    let m = told(&mut h);
    assert_eq!(
        m.memory_heaps[VRAM_HEAP].size, SHARE,
        "a share under 512 MiB is the heap"
    );
    assert_eq!(m.memory_heaps[1].size, SHARE);
    // Past the heap in one allocation: refused by the heap.
    assert_eq!(
        allocate_ret(&mut h, MEMORY, SHARE + SIZE, VRAM_TYPE),
        VK_ERROR_OUT_OF_DEVICE_MEMORY
    );
    // One share for both heaps of our pages: half in each fills it.
    let mut id = MEMORY;
    for i in 0..16 {
        let ty = if i % 2 == 0 {
            VRAM_TYPE
        } else {
            HOST_COHERENT_TYPE
        };
        assert_eq!(allocate_ret(&mut h, id, SIZE, ty), VK_SUCCESS);
        id += 1;
    }
    assert_eq!(h.renderer.factory().host_visible_bytes(), SHARE);
    for ty in [VRAM_TYPE, HOST_COHERENT_TYPE] {
        assert_eq!(
            allocate_ret(&mut h, id, SIZE, ty),
            VK_ERROR_OUT_OF_DEVICE_MEMORY
        );
        id += 1;
    }
    // Never a device-local byte, and a free gives it back.
    assert_eq!(h.renderer.factory().usage().limits.device_local_bytes, 0);
    h.send(&free(DEVICE, MEMORY)).expect("free");
    assert_eq!(h.renderer.factory().host_visible_bytes(), SHARE - SIZE);
    assert_eq!(allocate_ret(&mut h, id, SIZE, VRAM_TYPE), VK_SUCCESS);
    assert!(!h.fatal());
}

/// A host whose pages the driver will only import as type 4 (cached): type 6
/// is backed by type 3, the first coherent type, so it is refused in Vulkan
/// terms — and a resource is never told it may take it.
#[test]
fn the_visible_vram_type_is_refused_when_its_backing_type_will_not_take_the_pages() {
    let mut fake = FakeVulkan::standard();
    fake.host_pointer_bits = 0x10;
    let host = Arc::new(fake);
    let mut h = Harness::new(Arc::clone(&host));
    with_device(&mut h);
    let before = host.live("memory");
    assert_eq!(
        allocate_ret(&mut h, MEMORY, SIZE, VRAM_TYPE),
        VK_ERROR_OUT_OF_DEVICE_MEMORY
    );
    assert_eq!(host.live("memory"), before, "the driver was never asked");
    assert_eq!(h.renderer.factory().host_visible_bytes(), 0);
    assert!(!h.fatal());
}
