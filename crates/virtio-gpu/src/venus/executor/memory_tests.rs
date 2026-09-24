//! Stage 5b.1 against the fake host: device memory, the blob of it the guest
//! maps, buffers, images' memory and views — every command as Mesa 26.0.8
//! sends it (`vn_device_memory.c`, `vn_buffer.c`, `vn_image.c`,
//! `vn_feedback.c`), then every way a guest can abuse them.

use std::sync::Arc;

use crate::renderer::Renderer3d;
use crate::venus::protocol::*;

use super::fake::FakeVulkan;
use super::harness::*;
use super::ExecutorFactory;

/// The RTX-2070-shaped fake's memory types: 3 is `HOST_VISIBLE |
/// HOST_COHERENT` (importable), 1 is `DEVICE_LOCAL`, 5 the BAR the guest sees
/// as device-local only.
const HOST_COHERENT_TYPE: u32 = 3;
const DEVICE_LOCAL_TYPE: u32 = 1;
const BAR_TYPE: u32 = 5;

/// Where the fixtures map their blobs in the window.
const MAP_AT: u64 = 0x40_0000;
/// The fixture memory blob's resource id.
const MEM_RES: u32 = 20;
/// 64 KiB: a Mesa feedback buffer's size class.
const SIZE: u64 = 64 << 10;

fn standard() -> (Harness<FakeVulkan>, Arc<FakeVulkan>) {
    let host = Arc::new(FakeVulkan::standard());
    let mut h = Harness::new(Arc::clone(&host));
    with_device(&mut h);
    (h, host)
}

fn fatal_on(h: &mut Harness<FakeVulkan>, command: &Command<'_>) {
    let head = h.call(command).expect_err("the command must be fatal");
    assert_eq!(head, h.last_start, "head stays in front of the command");
    assert!(h.fatal());
}

/// Mesa's feedback buffer (`vn_feedback_buffer_create`): a synchronous
/// `vkCreateBuffer` and its requirements, an asynchronous allocation of a
/// coherent host-visible type and an asynchronous bind. Answers the
/// requirements.
fn feedback_buffer(h: &mut Harness<FakeVulkan>) -> VkMemoryRequirements {
    let Command::CreateBuffer(b) = h
        .call(&create_buffer(DEVICE, BUFFER, buffer_info(SIZE, TRANSFER)))
        .expect("buffer")
    else {
        panic!("wrong reply")
    };
    assert_eq!(b.ret, VK_SUCCESS);
    let Command::GetBufferMemoryRequirements2(r) =
        h.call(&buffer_requirements(DEVICE, BUFFER)).expect("reqs")
    else {
        panic!("wrong reply")
    };
    let req = r.p_memory_requirements.expect("filled").memory_requirements;
    assert_ne!(req.memory_type_bits & (1 << HOST_COHERENT_TYPE), 0);
    h.send(&allocate(
        DEVICE,
        MEMORY,
        req.size,
        HOST_COHERENT_TYPE,
        Vec::new(),
    ))
    .expect("allocate");
    h.send(&bind_buffers(DEVICE, &[(BUFFER, MEMORY, 0)]))
        .expect("bind");
    req
}

/// The blob of [`MEMORY`] and its mapping, as `vn_MapMemory2` makes them.
fn map_memory(h: &mut Harness<FakeVulkan>, size: u64) {
    h.memory_blob(CTX, MEM_RES, MEMORY, size)
        .expect("a blob of the memory");
    h.renderer
        .map_blob(MEM_RES, MAP_AT, size)
        .expect("mapped into the window");
}

// ------------------------------------------------------------ the path

#[test]
fn host_visible_memory_is_our_pages_and_its_blob_is_the_same_pages() {
    let (mut h, host) = standard();
    let req = feedback_buffer(&mut h);
    assert_eq!(req.size, SIZE);

    // The driver was asked to import pages of ours, of the whole size.
    let imports = host.imports();
    assert_eq!(imports.len(), 1);
    let import = &imports[0];
    assert_eq!(import.len, SIZE);
    assert_eq!(import.addr % 4096, 0);
    assert_eq!(
        host.allocations(),
        vec![(HOST_COHERENT_TYPE, SIZE, true)],
        "a host-visible type is imported, at its type index"
    );
    assert_eq!(host.buffer_binds().len(), 1);
    assert_eq!(h.renderer.factory().host_visible_bytes(), SIZE);

    // The blob wraps those very pages, and the window shows them.
    map_memory(&mut h, SIZE);
    let blob = h.renderer.blob_pages(MEM_RES).expect("the blob's pages");
    assert_eq!(
        blob.host_addr(),
        import.addr,
        "the blob is the imported pages"
    );
    assert_eq!(h.window.at(MAP_AT), Some((import.addr, SIZE)));

    // The guest writes through its mapping; the driver's import sees it.
    let pattern: Vec<u8> = (0..4096u32).map(|i| (i * 7 + 3) as u8).collect();
    blob.write_bytes(0x1000, &pattern).expect("inside");
    let imported = import.pages.upgrade().expect("the import is alive");
    let mut seen = vec![0u8; pattern.len()];
    imported.read_bytes(0x1000, &mut seen).expect("inside");
    assert_eq!(seen, pattern, "one set of bytes, two views");

    // And the other way: what the "GPU" writes, the guest reads.
    imported.write_bytes(0, b"from the gpu").expect("inside");
    let mut back = [0u8; 12];
    blob.read_bytes(0, &mut back).expect("inside");
    assert_eq!(&back, b"from the gpu");
    assert!(!h.fatal());
}

#[test]
fn memory_the_guest_cannot_map_is_plain_and_has_no_blob() {
    let (mut h, host) = standard();
    for (id, ty) in [(MEMORY, DEVICE_LOCAL_TYPE), (MEMORY + 1, BAR_TYPE)] {
        let Command::AllocateMemory(a) = h
            .call(&allocate(DEVICE, id, SIZE, ty, Vec::new()))
            .expect("allocate")
        else {
            panic!("wrong reply")
        };
        assert_eq!(a.ret, VK_SUCCESS);
    }
    assert_eq!(
        host.allocations(),
        vec![(DEVICE_LOCAL_TYPE, SIZE, false), (BAR_TYPE, SIZE, false)],
        "no import: plain driver memory, even of the BAR the guest cannot map"
    );
    assert!(host.imports().is_empty());
    assert_eq!(h.renderer.factory().host_visible_bytes(), 0);
    assert!(h.memory_blob(CTX, MEM_RES, MEMORY, SIZE).is_err());
    assert!(h.memory_blob(CTX, MEM_RES, MEMORY + 1, SIZE).is_err());
    assert_eq!(
        h.renderer.blob_count(),
        2,
        "only the ring and the reply pool"
    );
    assert!(!h.fatal(), "a refused blob is not a ring fault");
}

#[test]
fn a_blob_must_be_the_allocation_rounded_to_a_page_and_only_one() {
    let (mut h, _) = standard();
    // 5000 bytes: the guest kernel rounds the blob to 8 KiB.
    h.send(&allocate(
        DEVICE,
        MEMORY,
        5000,
        HOST_COHERENT_TYPE,
        Vec::new(),
    ))
    .expect("allocate");
    for wrong in [4096, 0x3000, 5000] {
        assert!(
            h.memory_blob(CTX, MEM_RES, MEMORY, wrong).is_err(),
            "{wrong:#x}"
        );
    }
    h.memory_blob(CTX, MEM_RES, MEMORY, 0x2000)
        .expect("the allocation rounded to a page");
    // vkr: a memory is exported once, so two resources never share storage.
    assert!(h.memory_blob(CTX, MEM_RES + 1, MEMORY, 0x2000).is_err());
    // A blob that names no memory, or names a buffer, is no blob either.
    assert!(h.memory_blob(CTX, MEM_RES + 2, 0x7777, 0x2000).is_err());
    assert!(h.memory_blob(CTX, MEM_RES + 3, DEVICE, 0x2000).is_err());
}

#[test]
fn a_blob_of_another_contexts_memory_finds_nothing() {
    let (mut h, _) = standard();
    feedback_buffer(&mut h);
    h.renderer
        .ctx_create(2, crate::CAPSET_VENUS, "venus")
        .expect("a second context");
    // Its own table has no such id, and the kernel's context 0 has none.
    assert!(h.memory_blob(2, MEM_RES, MEMORY, SIZE).is_err());
    assert!(h.memory_blob(0, MEM_RES, MEMORY, SIZE).is_err());
    // The owner can.
    h.memory_blob(CTX, MEM_RES, MEMORY, SIZE)
        .expect("its own memory");
}

#[test]
fn freeing_memory_while_its_blob_is_mapped_leaves_the_pages_to_the_mapping() {
    let (mut h, host) = standard();
    feedback_buffer(&mut h);
    map_memory(&mut h, SIZE);
    let import = host.imports()[0].clone();

    // The guest frees first (Mesa's `vn_FreeMemory` unrefs the bo, but the
    // resource unref and the ring's vkFreeMemory race on the host).
    h.send(&free(DEVICE, MEMORY)).expect("free");
    assert_eq!(host.live("memory"), 0, "the driver's memory is gone");
    let pages = import.pages.upgrade().expect("the mapping keeps the pages");
    assert_eq!(h.window.at(MAP_AT), Some((import.addr, SIZE)));
    drop(pages);
    assert_eq!(
        h.renderer.factory().host_visible_bytes(),
        SIZE,
        "still charged: a guest cannot free its way past the budget"
    );
    let why = h.renderer.snapshot_refusal().expect("refused");
    assert!(why.contains("host Vulkan objects"), "{why}");

    // Unmapping takes the pages out of the guest, and destroying the blob
    // lets them go.
    h.renderer.unmap_blob(MEM_RES, MAP_AT);
    assert_eq!(h.window.at(MAP_AT), None);
    assert!(
        import.pages.upgrade().is_some(),
        "the blob still holds them"
    );
    h.renderer.destroy_blob(MEM_RES);
    assert!(
        import.pages.upgrade().is_none(),
        "freed with the last holder"
    );
    assert_eq!(h.renderer.factory().host_visible_bytes(), 0);
    assert!(!h.fatal());
}

#[test]
fn destroying_the_blob_first_leaves_the_memory_whole() {
    let (mut h, host) = standard();
    feedback_buffer(&mut h);
    map_memory(&mut h, SIZE);
    let import = host.imports()[0].clone();
    h.renderer.destroy_blob(MEM_RES);
    assert_eq!(h.window.count(), 0, "destroying a mapped blob unmaps it");
    assert!(import.pages.upgrade().is_some(), "the memory still is them");
    assert_eq!(host.live("memory"), 1);
    h.send(&free(DEVICE, MEMORY)).expect("free");
    assert!(import.pages.upgrade().is_none());
    assert_eq!(h.renderer.factory().host_visible_bytes(), 0);
}

// ---------------------------------------------------------- the budget

#[test]
fn the_host_visible_budget_is_renderer_wide_and_answered_in_vulkan_terms() {
    let host = Arc::new(FakeVulkan::standard());
    let mut h = Harness::with_factory(ExecutorFactory::with_budget(Arc::clone(&host), 2 * SIZE));
    with_device(&mut h);
    let call = |h: &mut Harness<FakeVulkan>, id: u64, size: u64, ty: u32| {
        let Command::AllocateMemory(a) = h
            .call(&allocate(DEVICE, id, size, ty, Vec::new()))
            .expect("answered")
        else {
            panic!("wrong reply")
        };
        a.ret
    };
    assert_eq!(call(&mut h, MEMORY, SIZE, HOST_COHERENT_TYPE), VK_SUCCESS);
    assert_eq!(
        call(&mut h, MEMORY + 1, 2 * SIZE, HOST_COHERENT_TYPE),
        VK_ERROR_OUT_OF_DEVICE_MEMORY,
        "what is left is too little"
    );
    assert_eq!(
        call(&mut h, MEMORY + 2, 4 * SIZE, HOST_COHERENT_TYPE),
        VK_ERROR_OUT_OF_DEVICE_MEMORY,
        "the whole budget is too little"
    );
    assert_eq!(
        host.allocations().len(),
        1,
        "no refused size reached the host"
    );
    // Device-local memory is the driver's to bound, not the budget's.
    assert_eq!(
        call(&mut h, MEMORY + 3, 16 * SIZE, DEVICE_LOCAL_TYPE),
        VK_SUCCESS
    );
    // Past the heap, and past the type count, as vkr answers.
    assert_eq!(
        call(&mut h, MEMORY + 4, 1 << 40, DEVICE_LOCAL_TYPE),
        VK_ERROR_OUT_OF_DEVICE_MEMORY
    );
    assert_eq!(call(&mut h, MEMORY + 5, SIZE, 6), VK_ERROR_UNKNOWN);
    assert_eq!(call(&mut h, MEMORY + 6, SIZE, 31), VK_ERROR_UNKNOWN);

    // Freeing gives it back; a second context shares the same budget.
    h.send(&free(DEVICE, MEMORY)).expect("free");
    assert_eq!(h.renderer.factory().host_visible_bytes(), 0);
    assert_eq!(
        call(&mut h, MEMORY + 7, 2 * SIZE, HOST_COHERENT_TYPE),
        VK_SUCCESS
    );
    assert!(!h.fatal());
}

#[test]
fn allocations_no_correct_guest_sends_are_refused_before_the_host_sees_them() {
    let (mut h, host) = standard();
    // Size 0.
    fatal_on(
        &mut h,
        &allocate(DEVICE, MEMORY, 0, HOST_COHERENT_TYPE, Vec::new()),
    );
    // Capture replay, which is masked.
    let (mut h2, _) = standard();
    fatal_on(
        &mut h2,
        &allocate(
            DEVICE,
            MEMORY,
            SIZE,
            DEVICE_LOCAL_TYPE,
            vec![VkMemoryAllocateInfoNext::VkMemoryAllocateFlagsInfo(
                VkMemoryAllocateFlagsInfo {
                    flags: 0x4,
                    device_mask: 0,
                },
            )],
        ),
    );
    // DEVICE_ADDRESS on a device that did not enable bufferDeviceAddress.
    let (mut h3, _) = standard();
    fatal_on(
        &mut h3,
        &allocate(
            DEVICE,
            MEMORY,
            SIZE,
            DEVICE_LOCAL_TYPE,
            vec![VkMemoryAllocateInfoNext::VkMemoryAllocateFlagsInfo(
                VkMemoryAllocateFlagsInfo {
                    flags: 0x2,
                    device_mask: 0,
                },
            )],
        ),
    );
    // A dedicated allocation naming both an image and a buffer.
    let (mut h4, _) = standard();
    h4.call(&create_image(DEVICE, IMAGE, image_info()))
        .expect("image");
    h4.call(&create_buffer(DEVICE, BUFFER, buffer_info(SIZE, TRANSFER)))
        .expect("buffer");
    fatal_on(
        &mut h4,
        &allocate(
            DEVICE,
            MEMORY,
            SIZE,
            DEVICE_LOCAL_TYPE,
            vec![VkMemoryAllocateInfoNext::VkMemoryDedicatedAllocateInfo(
                VkMemoryDedicatedAllocateInfo {
                    image: VkImage(IMAGE),
                    buffer: VkBuffer(BUFFER),
                },
            )],
        ),
    );
    assert!(host.allocations().is_empty());

    // Importing a virtio-gpu resource, and exporting a handle: answered, as
    // vkr answers a resource it cannot import.
    let (mut h5, host5) = standard();
    for chain in [
        vec![VkMemoryAllocateInfoNext::VkImportMemoryResourceInfoMESA(
            VkImportMemoryResourceInfoMESA { resource_id: 7 },
        )],
        vec![VkMemoryAllocateInfoNext::VkExportMemoryAllocateInfo(
            VkExportMemoryAllocateInfo { handle_types: 0x10 },
        )],
    ] {
        let Command::AllocateMemory(a) = h5
            .call(&allocate(DEVICE, MEMORY, SIZE, DEVICE_LOCAL_TYPE, chain))
            .expect("answered")
        else {
            panic!("wrong reply")
        };
        assert_eq!(a.ret, VK_ERROR_INVALID_EXTERNAL_HANDLE);
    }
    // Mesa's own rewrite of an export — handle types 0 — is accepted.
    let Command::AllocateMemory(a) = h5
        .call(&allocate(
            DEVICE,
            MEMORY,
            SIZE,
            DEVICE_LOCAL_TYPE,
            vec![VkMemoryAllocateInfoNext::VkExportMemoryAllocateInfo(
                VkExportMemoryAllocateInfo { handle_types: 0 },
            )],
        ))
        .expect("answered")
    else {
        panic!("wrong reply")
    };
    assert_eq!(a.ret, VK_SUCCESS);
    assert_eq!(host5.allocations().len(), 1);
}

// ------------------------------------------------------------- binding

#[test]
fn a_bind_outside_the_memory_or_off_its_alignment_is_refused() {
    // Past the end: a 64 KiB buffer at 256 in 64 KiB of memory.
    let (mut h, host) = standard();
    h.call(&create_buffer(DEVICE, BUFFER, buffer_info(SIZE, TRANSFER)))
        .expect("buffer");
    h.send(&allocate(
        DEVICE,
        MEMORY,
        SIZE,
        HOST_COHERENT_TYPE,
        Vec::new(),
    ))
    .expect("allocate");
    fatal_on(&mut h, &bind_buffers(DEVICE, &[(BUFFER, MEMORY, 256)]));
    assert!(host.buffer_binds().is_empty());

    // Off the 256-byte alignment, with room to spare.
    let (mut h, host) = standard();
    h.call(&create_buffer(DEVICE, BUFFER, buffer_info(SIZE, TRANSFER)))
        .expect("buffer");
    h.send(&allocate(
        DEVICE,
        MEMORY,
        2 * SIZE,
        HOST_COHERENT_TYPE,
        Vec::new(),
    ))
    .expect("allocate");
    fatal_on(&mut h, &bind_buffers(DEVICE, &[(BUFFER, MEMORY, 16)]));
    // An offset whose sum wraps is outside too, not inside.
    let (mut h, _) = standard();
    h.call(&create_buffer(DEVICE, BUFFER, buffer_info(SIZE, TRANSFER)))
        .expect("buffer");
    h.send(&allocate(
        DEVICE,
        MEMORY,
        SIZE,
        HOST_COHERENT_TYPE,
        Vec::new(),
    ))
    .expect("allocate");
    fatal_on(
        &mut h,
        &bind_buffers(DEVICE, &[(BUFFER, MEMORY, u64::MAX - 255)]),
    );
    assert!(host.buffer_binds().is_empty());
}

#[test]
fn a_buffer_binds_once_and_only_to_memory_of_its_own_device() {
    let (mut h, host) = standard();
    feedback_buffer(&mut h);
    h.send(&allocate(
        DEVICE,
        MEMORY + 1,
        SIZE,
        HOST_COHERENT_TYPE,
        Vec::new(),
    ))
    .expect("allocate");
    fatal_on(&mut h, &bind_buffers(DEVICE, &[(BUFFER, MEMORY + 1, 0)]));
    assert_eq!(host.buffer_binds().len(), 1);

    // Twice in one call is twice.
    let (mut h, _) = standard();
    h.call(&create_buffer(DEVICE, BUFFER, buffer_info(SIZE, TRANSFER)))
        .expect("buffer");
    h.send(&allocate(
        DEVICE,
        MEMORY,
        2 * SIZE,
        HOST_COHERENT_TYPE,
        Vec::new(),
    ))
    .expect("allocate");
    fatal_on(
        &mut h,
        &bind_buffers(DEVICE, &[(BUFFER, MEMORY, 0), (BUFFER, MEMORY, SIZE)]),
    );

    // A memory id that is a buffer, and a buffer id that is memory.
    let (mut h, _) = standard();
    feedback_buffer(&mut h);
    fatal_on(&mut h, &bind_buffers(DEVICE, &[(MEMORY, BUFFER, 0)]));
}

#[test]
fn a_dedicated_allocation_binds_only_its_own_resource_at_zero() {
    let (mut h, _) = standard();
    h.call(&create_buffer(DEVICE, BUFFER, buffer_info(SIZE, TRANSFER)))
        .expect("buffer");
    h.call(&create_buffer(
        DEVICE,
        BUFFER + 1,
        buffer_info(SIZE, TRANSFER),
    ))
    .expect("buffer");
    h.send(&allocate(
        DEVICE,
        MEMORY,
        SIZE,
        DEVICE_LOCAL_TYPE,
        vec![VkMemoryAllocateInfoNext::VkMemoryDedicatedAllocateInfo(
            VkMemoryDedicatedAllocateInfo {
                image: VkImage(0),
                buffer: VkBuffer(BUFFER),
            },
        )],
    ))
    .expect("allocate");
    fatal_on(&mut h, &bind_buffers(DEVICE, &[(BUFFER + 1, MEMORY, 0)]));

    let (mut h, host) = standard();
    h.call(&create_buffer(DEVICE, BUFFER, buffer_info(SIZE, TRANSFER)))
        .expect("buffer");
    h.send(&allocate(
        DEVICE,
        MEMORY,
        SIZE,
        DEVICE_LOCAL_TYPE,
        vec![VkMemoryAllocateInfoNext::VkMemoryDedicatedAllocateInfo(
            VkMemoryDedicatedAllocateInfo {
                image: VkImage(0),
                buffer: VkBuffer(BUFFER),
            },
        )],
    ))
    .expect("allocate");
    h.send(&bind_buffers(DEVICE, &[(BUFFER, MEMORY, 0)]))
        .expect("its own buffer");
    assert_eq!(host.buffer_binds().len(), 1);
}

#[test]
fn an_image_the_driver_cannot_import_for_never_sees_a_host_visible_type() {
    let (mut h, host) = standard();
    // Optimal tiling: the fake driver imports host memory for linear only.
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
    assert_eq!(bits, 0x3f & !0x18, "the host said 0x3f; 3 and 4 are ours");
    // A linear one may.
    let linear = VkImageCreateInfo {
        tiling: 1,
        ..image_info()
    };
    h.call(&create_image(DEVICE, IMAGE + 1, linear))
        .expect("image");
    let Command::GetImageMemoryRequirements2(r) = h
        .call(&memory_requirements(DEVICE, IMAGE + 1))
        .expect("reqs")
    else {
        panic!("wrong reply")
    };
    assert_eq!(
        r.p_memory_requirements
            .expect("filled")
            .memory_requirements
            .memory_type_bits,
        0x3f
    );
    assert_eq!(
        host.host_memory_resources(),
        vec![("image", false), ("image", true)],
        "created for host allocations exactly when it may take them"
    );

    // Binding the optimal image to host-visible memory is outside its bits.
    h.send(&allocate(
        DEVICE,
        MEMORY,
        1 << 20,
        HOST_COHERENT_TYPE,
        Vec::new(),
    ))
    .expect("allocate");
    fatal_on(&mut h, &bind_image(DEVICE, IMAGE, MEMORY, 0));
    assert!(host.image_binds().is_empty());
}

#[test]
fn an_image_binds_to_device_memory_and_then_takes_a_view() {
    let (mut h, host) = standard();
    h.call(&create_image(DEVICE, IMAGE, image_info()))
        .expect("image");
    let view = |id: u64| {
        Command::CreateImageView(CreateImageViewArgs {
            device: VkDevice(DEVICE),
            p_create_info: Some(VkImageViewCreateInfo {
                image: VkImage(IMAGE),
                view_type: 1,
                format: RGBA8,
                subresource_range: VkImageSubresourceRange {
                    aspect_mask: 0x1,
                    base_mip_level: 0,
                    level_count: 1,
                    base_array_layer: 0,
                    layer_count: u32::MAX,
                },
                ..Default::default()
            }),
            p_view: Some(VkImageView(id)),
            ret: 0,
        })
    };
    // A view of an unbound image is refused.
    let (mut early, _) = standard();
    early
        .call(&create_image(DEVICE, IMAGE, image_info()))
        .expect("image");
    fatal_on(&mut early, &view(IMAGE_VIEW));

    h.send(&allocate(
        DEVICE,
        MEMORY,
        1 << 20,
        DEVICE_LOCAL_TYPE,
        Vec::new(),
    ))
    .expect("allocate");
    h.send(&bind_image(DEVICE, IMAGE, MEMORY, 0)).expect("bind");
    assert_eq!(host.image_binds().len(), 1);
    // Bound once.
    let Command::CreateImageView(v) = h.call(&view(IMAGE_VIEW)).expect("view") else {
        panic!("wrong reply")
    };
    assert_eq!(v.ret, VK_SUCCESS);
    assert_eq!(host.live("image view"), 1);
    h.send(&Command::DestroyImageView(DestroyImageViewArgs {
        device: VkDevice(DEVICE),
        image_view: VkImageView(IMAGE_VIEW),
    }))
    .expect("destroy");
    assert_eq!(host.live("image view"), 0);

    // A second bind of the same image.
    h.send(&allocate(
        DEVICE,
        MEMORY + 1,
        1 << 20,
        DEVICE_LOCAL_TYPE,
        Vec::new(),
    ))
    .expect("allocate");
    fatal_on(&mut h, &bind_image(DEVICE, IMAGE, MEMORY + 1, 0));
}

#[test]
fn an_image_view_outside_its_image_is_refused() {
    let (mut h, _) = standard();
    h.call(&create_image(DEVICE, IMAGE, image_info()))
        .expect("image");
    h.send(&allocate(
        DEVICE,
        MEMORY,
        1 << 20,
        DEVICE_LOCAL_TYPE,
        Vec::new(),
    ))
    .expect("allocate");
    h.send(&bind_image(DEVICE, IMAGE, MEMORY, 0)).expect("bind");
    for (what, info) in [
        (
            "mip 1 of a 1-level image",
            VkImageViewCreateInfo {
                image: VkImage(IMAGE),
                view_type: 1,
                format: RGBA8,
                subresource_range: VkImageSubresourceRange {
                    aspect_mask: 1,
                    base_mip_level: 1,
                    level_count: 1,
                    base_array_layer: 0,
                    layer_count: 1,
                },
                ..Default::default()
            },
        ),
        (
            "a cube view of a plain 2D image",
            VkImageViewCreateInfo {
                image: VkImage(IMAGE),
                view_type: 3,
                format: RGBA8,
                subresource_range: VkImageSubresourceRange {
                    aspect_mask: 1,
                    base_mip_level: 0,
                    level_count: 1,
                    base_array_layer: 0,
                    layer_count: 1,
                },
                ..Default::default()
            },
        ),
        (
            "another format without MUTABLE_FORMAT",
            VkImageViewCreateInfo {
                image: VkImage(IMAGE),
                view_type: 1,
                format: 44,
                subresource_range: VkImageSubresourceRange {
                    aspect_mask: 1,
                    base_mip_level: 0,
                    level_count: 1,
                    base_array_layer: 0,
                    layer_count: 1,
                },
                ..Default::default()
            },
        ),
    ] {
        let (mut fresh, _) = standard();
        fresh
            .call(&create_image(DEVICE, IMAGE, image_info()))
            .expect("image");
        fresh
            .send(&allocate(
                DEVICE,
                MEMORY,
                1 << 20,
                DEVICE_LOCAL_TYPE,
                Vec::new(),
            ))
            .expect("allocate");
        fresh
            .send(&bind_image(DEVICE, IMAGE, MEMORY, 0))
            .expect("bind");
        let head = fresh
            .call(&Command::CreateImageView(CreateImageViewArgs {
                device: VkDevice(DEVICE),
                p_create_info: Some(info),
                p_view: Some(VkImageView(IMAGE_VIEW)),
                ret: 0,
            }))
            .expect_err(what);
        assert_eq!(head, fresh.last_start, "{what}");
    }
    assert!(!h.fatal());
}

#[test]
fn a_linear_image_reports_its_layout_and_an_optimal_one_is_refused() {
    let (mut h, _) = standard();
    let linear = VkImageCreateInfo {
        tiling: 1,
        ..image_info()
    };
    h.call(&create_image(DEVICE, IMAGE, linear)).expect("image");
    let layout = |image: u64, mip: u32| {
        Command::GetImageSubresourceLayout(GetImageSubresourceLayoutArgs {
            device: VkDevice(DEVICE),
            image: VkImage(image),
            p_subresource: Some(VkImageSubresource {
                aspect_mask: 1,
                mip_level: mip,
                array_layer: 0,
            }),
            p_layout: Some(VkSubresourceLayout::default()),
        })
    };
    let Command::GetImageSubresourceLayout(l) = h.call(&layout(IMAGE, 0)).expect("layout") else {
        panic!("wrong reply")
    };
    assert_eq!(l.p_layout.expect("filled").row_pitch, 256);
    fatal_on(&mut h, &layout(IMAGE, 1));

    let (mut h, _) = standard();
    h.call(&create_image(DEVICE, IMAGE, image_info()))
        .expect("image");
    fatal_on(&mut h, &layout(IMAGE, 0));
}

#[test]
fn a_texel_buffer_view_is_inside_a_bound_buffer() {
    let (mut h, host) = standard();
    // Uniform texel usage.
    h.call(&create_buffer(DEVICE, BUFFER, buffer_info(SIZE, 0x4)))
        .expect("buffer");
    h.send(&allocate(
        DEVICE,
        MEMORY,
        SIZE,
        DEVICE_LOCAL_TYPE,
        Vec::new(),
    ))
    .expect("allocate");
    h.send(&bind_buffers(DEVICE, &[(BUFFER, MEMORY, 0)]))
        .expect("bind");
    let view = |id: u64, offset: u64, range: u64| {
        Command::CreateBufferView(CreateBufferViewArgs {
            device: VkDevice(DEVICE),
            p_create_info: Some(VkBufferViewCreateInfo {
                p_next: Vec::new(),
                flags: 0,
                buffer: VkBuffer(BUFFER),
                format: RGBA8,
                offset,
                range,
            }),
            p_view: Some(VkBufferView(id)),
            ret: 0,
        })
    };
    let Command::CreateBufferView(v) = h.call(&view(BUFFER_VIEW, 0, u64::MAX)).expect("view")
    else {
        panic!("wrong reply")
    };
    assert_eq!(v.ret, VK_SUCCESS);
    assert_eq!(host.live("buffer view"), 1);
    fatal_on(&mut h, &view(BUFFER_VIEW + 1, SIZE - 256, 512));
}

#[test]
fn the_device_level_requirements_are_checked_and_filtered_like_the_objects() {
    let (mut h, _) = standard();
    let reqs = |info: VkBufferCreateInfo| {
        Command::GetDeviceBufferMemoryRequirements(GetDeviceBufferMemoryRequirementsArgs {
            device: VkDevice(DEVICE),
            p_info: Some(VkDeviceBufferMemoryRequirements {
                p_create_info: Some(info),
            }),
            p_memory_requirements: Some(VkMemoryRequirements2::default()),
        })
    };
    let Command::GetDeviceBufferMemoryRequirements(r) =
        h.call(&reqs(buffer_info(1000, TRANSFER))).expect("reqs")
    else {
        panic!("wrong reply")
    };
    let r = r.p_memory_requirements.expect("filled").memory_requirements;
    assert_eq!((r.size, r.alignment, r.memory_type_bits), (1024, 256, 0x3f));

    let image = Command::GetDeviceImageMemoryRequirements(GetDeviceImageMemoryRequirementsArgs {
        device: VkDevice(DEVICE),
        p_info: Some(VkDeviceImageMemoryRequirements {
            p_create_info: Some(image_info()),
            plane_aspect: 0,
        }),
        p_memory_requirements: Some(VkMemoryRequirements2::default()),
    });
    let Command::GetDeviceImageMemoryRequirements(r) = h.call(&image).expect("reqs") else {
        panic!("wrong reply")
    };
    assert_eq!(
        r.p_memory_requirements
            .expect("filled")
            .memory_requirements
            .memory_type_bits,
        0x27
    );
    // A sparse buffer is no buffer here.
    fatal_on(
        &mut h,
        &reqs(VkBufferCreateInfo {
            flags: 0x1,
            ..buffer_info(1000, TRANSFER)
        }),
    );
}

#[test]
fn device_addresses_need_the_feature_the_usage_and_the_memory_flag() {
    use super::fake::gpu;
    let host = Arc::new(FakeVulkan::new(vec![gpu("NVIDIA GeForce RTX 2070")]));
    let mut h = Harness::new(Arc::clone(&host));
    boot(&mut h);
    let features = VkDeviceCreateInfoNext::VkPhysicalDeviceVulkan12Features(
        VkPhysicalDeviceVulkan12Features {
            buffer_device_address: 1,
            ..Default::default()
        },
    );
    let Command::CreateDevice(d) = h
        .call(&create_device(PHYSICAL, DEVICE, vec![features]))
        .expect("device")
    else {
        panic!("wrong reply")
    };
    assert_eq!(d.ret, VK_SUCCESS);
    h.call(&device_queue(DEVICE, QUEUE, 1)).expect("queue");
    h.call(&create_buffer(
        DEVICE,
        BUFFER,
        buffer_info(SIZE, TRANSFER | 0x2_0000),
    ))
    .expect("buffer");
    // Memory without DEVICE_ADDRESS cannot hold a device-address buffer.
    h.send(&allocate(
        DEVICE,
        MEMORY,
        SIZE,
        DEVICE_LOCAL_TYPE,
        Vec::new(),
    ))
    .expect("allocate");
    let flags = vec![VkMemoryAllocateInfoNext::VkMemoryAllocateFlagsInfo(
        VkMemoryAllocateFlagsInfo {
            flags: 0x2,
            device_mask: 0,
        },
    )];
    h.send(&allocate(
        DEVICE,
        MEMORY + 1,
        SIZE,
        DEVICE_LOCAL_TYPE,
        flags,
    ))
    .expect("allocate with DEVICE_ADDRESS");
    h.send(&bind_buffers(DEVICE, &[(BUFFER, MEMORY + 1, 0)]))
        .expect("bind");
    let address = Command::GetBufferDeviceAddress(GetBufferDeviceAddressArgs {
        device: VkDevice(DEVICE),
        p_info: Some(VkBufferDeviceAddressInfo {
            buffer: VkBuffer(BUFFER),
        }),
        ret: 0,
    });
    let Command::GetBufferDeviceAddress(a) = h.call(&address).expect("address") else {
        panic!("wrong reply")
    };
    assert_ne!(a.ret, 0);

    // Capture replay is masked, so its query is not served.
    let capture = Command::GetBufferOpaqueCaptureAddress(GetBufferOpaqueCaptureAddressArgs {
        device: VkDevice(DEVICE),
        p_info: Some(VkBufferDeviceAddressInfo {
            buffer: VkBuffer(BUFFER),
        }),
        ret: 0,
    });
    fatal_on(&mut h, &capture);

    // And without the feature, the usage itself is refused.
    let (mut h, _) = standard();
    fatal_on(
        &mut h,
        &create_buffer(DEVICE, BUFFER, buffer_info(SIZE, TRANSFER | 0x2_0000)),
    );
}

#[test]
fn capture_replay_is_reported_false_whatever_the_host_says() {
    let (mut h, _) = standard();
    let features = Command::GetPhysicalDeviceFeatures2(GetPhysicalDeviceFeatures2Args {
        physical_device: VkPhysicalDevice(PHYSICAL),
        p_features: Some(VkPhysicalDeviceFeatures2 {
            p_next: vec![
                VkPhysicalDeviceFeatures2Next::VkPhysicalDeviceVulkan12Features(Default::default()),
            ],
            ..Default::default()
        }),
    });
    let Command::GetPhysicalDeviceFeatures2(f) = h.call(&features).expect("features") else {
        panic!("wrong reply")
    };
    let Some(VkPhysicalDeviceFeatures2Next::VkPhysicalDeviceVulkan12Features(v12)) =
        f.p_features.expect("filled").p_next.first().cloned()
    else {
        panic!("the link comes back")
    };
    assert_eq!(v12.buffer_device_address, 1, "the host's own answer");
    assert_eq!(v12.buffer_device_address_capture_replay, 0);
}

#[test]
fn the_commitment_of_memory_that_is_not_lazy_is_zero_without_asking_the_driver() {
    let (mut h, _) = standard();
    h.send(&allocate(
        DEVICE,
        MEMORY,
        SIZE,
        DEVICE_LOCAL_TYPE,
        Vec::new(),
    ))
    .expect("allocate");
    let Command::GetDeviceMemoryCommitment(c) = h
        .call(&Command::GetDeviceMemoryCommitment(
            GetDeviceMemoryCommitmentArgs {
                device: VkDevice(DEVICE),
                memory: VkDeviceMemory(MEMORY),
                p_committed_memory_in_bytes: Some(0),
            },
        ))
        .expect("commitment")
    else {
        panic!("wrong reply")
    };
    assert_eq!(c.p_committed_memory_in_bytes, Some(0));
}

// ---------------------------------------------------- rings and seqnos

#[test]
fn a_ring_cannot_live_in_a_blob_of_vulkan_memory() {
    let (mut h, _) = standard();
    feedback_buffer(&mut h);
    h.memory_blob(CTX, MEM_RES, MEMORY, SIZE).expect("blob");
    // vkCreateRingMESA naming the memory blob: refused, the harness ring
    // lives on.
    let err = h.renderer.submit(CTX, &{
        let mut enc = crate::venus::wire::Encoder::new();
        enc.command_header(crate::venus::wire::CommandHeader {
            opcode: crate::venus::transport::Opcode::CreateRing.as_u32(),
            flags: 0,
        })
        .unwrap();
        enc.handle(0x999).unwrap();
        enc.simple_pointer(true).unwrap();
        enc.i32(crate::venus::transport::STYPE_RING_CREATE_INFO_MESA)
            .unwrap();
        enc.simple_pointer(false).unwrap();
        enc.flags(0).unwrap();
        enc.u32(MEM_RES).unwrap();
        for value in [0, SIZE, 1_000, 0, 4, 8, 0x100, 0x1000, 0x1100, 4] {
            enc.u64(value).unwrap();
        }
        enc.finish().unwrap()
    });
    assert!(err.is_err());
    assert_eq!(h.renderer.ring_count(), 1);
}

#[test]
fn a_ring_seqno_wait_returns_once_the_ring_has_executed_that_far() {
    let (mut h, _) = standard();
    // Everything submitted so far has been executed: the wait is immediate.
    let tail = u64::from(h.tail());
    h.wait_ring_seqno(tail).expect("reached");
    // Mesa's order: the allocation into the ring, then a wait for it, then
    // the blob.
    h.produce(&async_bytes(&allocate(
        DEVICE,
        MEMORY,
        SIZE,
        HOST_COHERENT_TYPE,
        Vec::new(),
    )));
    h.wait_ring_seqno(u64::from(h.tail()))
        .expect("the ring got there");
    h.memory_blob(CTX, MEM_RES, MEMORY, SIZE)
        .expect("the memory exists by the time the blob is made");
    // A seqno past the tail can never be reached, and is refused at once.
    assert!(h.wait_ring_seqno(u64::from(h.tail()) + 64).is_err());
    assert!(!h.fatal());
}

// ------------------------------------------------- teardown, snapshot

#[test]
fn a_device_reset_leaves_no_host_object_and_no_page_behind() {
    let (mut h, host) = standard();
    feedback_buffer(&mut h);
    map_memory(&mut h, SIZE);
    h.call(&create_image(DEVICE, IMAGE, image_info()))
        .expect("image");
    let import = host.imports()[0].clone();
    assert!(host.live_objects() >= 5);
    h.renderer.reset();
    assert_eq!(host.live_objects(), 0);
    assert_eq!(h.renderer.factory().host_objects(), 0);
    assert_eq!(h.renderer.factory().host_visible_bytes(), 0);
    assert!(import.pages.upgrade().is_none(), "no page is left");
    assert_eq!(h.window.count(), 0, "nothing is left in front of the guest");
    assert_eq!(h.renderer.blob_count(), 0);
    assert_eq!(h.renderer.snapshot_refusal(), None);
}

#[test]
fn destroying_the_device_frees_memory_after_everything_bound_to_it() {
    let (mut h, host) = standard();
    feedback_buffer(&mut h);
    h.call(&create_image(DEVICE, IMAGE, image_info()))
        .expect("image");
    h.send(&Command::DestroyDevice(DestroyDeviceArgs {
        device: VkDevice(DEVICE),
    }))
    .expect("destroy");
    for kind in ["buffer", "image", "memory", "device", "command pool"] {
        assert_eq!(host.live(kind), 0, "{kind}");
    }
    assert_eq!(h.renderer.factory().host_visible_bytes(), 0);
}

#[test]
fn destroying_the_context_keeps_a_mapped_blob_safe_until_it_goes() {
    let (mut h, host) = standard();
    feedback_buffer(&mut h);
    map_memory(&mut h, SIZE);
    let import = host.imports()[0].clone();
    h.renderer.ctx_destroy(CTX);
    assert_eq!(host.live_objects(), 0, "every host object is gone");
    assert!(
        import.pages.upgrade().is_some(),
        "but the guest may still map the pages"
    );
    let why = h.renderer.snapshot_refusal().expect("refused");
    assert!(why.contains("host-visible Vulkan memory"), "{why}");
    h.renderer.destroy_blob(MEM_RES);
    assert!(import.pages.upgrade().is_none());
    assert_eq!(h.renderer.snapshot_refusal(), None);
}
