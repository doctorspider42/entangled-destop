//! Stage S1 ("GNOME on the GPU") against the fake host: exportable
//! device-local memory and its handle blobs, imports of them by another
//! context, the emulated `VK_EXT_image_drm_format_modifier` (LINEAR only, as
//! a canonical optimal image), `VK_EXT_queue_family_foreign` in barriers, and
//! teardown — every command as Mesa 26.0.8's venus sends it for Zink and GBM,
//! then every way a guest can abuse it.

use std::sync::atomic::Ordering;
use std::sync::Arc;

use crate::error::CommandError;
use crate::renderer::Renderer3d;
use crate::venus::protocol::*;

use super::fake::{self, FakeVulkan, HandleImport};
use super::harness::*;
use super::host::ResourceMemory;
use super::modifier::{
    DRM_FORMAT_MOD_LINEAR, IMAGE_ASPECT_MEMORY_PLANE_0, IMAGE_TILING_DRM_FORMAT_MODIFIER,
};
use super::policy;
use super::recording::*;

/// The fake's device-local type (and 3, 4 are our pages).
const DEVICE_LOCAL_TYPE: u32 = 1;
const HOST_TYPE: u32 = 3;
/// `VK_FORMAT_B8G8R8A8_UNORM` / `_SRGB`: XRGB8888 and its twin.
const BGRA: i32 = 44;
const BGRA_SRGB: i32 = 50;
const W: u32 = 1920;
const H: u32 = 1080;
/// 1920 × 4, already 256-aligned.
const PITCH: u64 = 7680;
/// `rowPitch × height`, which the requirements are raised to.
const PLANE: u64 = PITCH * H as u64;

const MUTABLE: u32 = 0x8;
const USAGE_SAMPLED: u32 = 0x4;
const USAGE_STORAGE: u32 = 0x8;
const USAGE_COLOR: u32 = 0x10;
const USAGE_TRANSFER: u32 = 0x1 | 0x2;
/// What the fake's scanout formats support once they have attachments:
/// transfers, sampled, blits, storage, colour attachment and blend.
const SCANOUT_FEATURES: u32 = 0x1_d401 | 0x80 | 0x100 | 0x2;
/// The superset those features give: transfers, sampled, storage, colour
/// and input attachment.
const SUPERSET: u32 = USAGE_TRANSFER | USAGE_SAMPLED | USAGE_STORAGE | USAGE_COLOR | 0x80;

/// What a Zink device enables for GNOME on the GPU, as venus sends it.
const S1_EXTENSIONS: &[&str] = &[
    "VK_KHR_external_semaphore_fd",
    "VK_EXT_external_memory_dma_buf",
    "VK_KHR_external_memory_fd",
    "VK_EXT_queue_family_foreign",
    "VK_EXT_image_drm_format_modifier",
];

/// An RTX-2070-shaped fake that can export device-local memory, has
/// `VK_EXT_queue_family_foreign`, and attachments on its formats.
fn gpu(name: &str, uuid: u8) -> fake::FakeDevice {
    let mut device = fake::zink_gpu(name);
    device.info.extensions.push(VkExtensionProperties {
        extension_name: policy::name_array(policy::QUEUE_FAMILY_FOREIGN_EXT),
        spec_version: 1,
    });
    for link in &mut device.info.properties.p_next {
        match link {
            VkPhysicalDeviceProperties2Next::VkPhysicalDeviceIDProperties(p) => {
                p.device_uuid = [uuid; 16];
                p.driver_uuid = [0x42; 16];
            }
            VkPhysicalDeviceProperties2Next::VkPhysicalDeviceVulkan11Properties(p) => {
                p.device_uuid = [uuid; 16];
                p.driver_uuid = [0x42; 16];
            }
            _ => {}
        }
    }
    device
}

fn host_with(devices: Vec<fake::FakeDevice>) -> Arc<FakeVulkan> {
    let host = FakeVulkan::new(devices);
    host.format_features
        .store(SCANOUT_FEATURES, Ordering::SeqCst);
    Arc::new(host)
}

fn device_with(physical: u64, exts: &[&'static str]) -> Command<'static> {
    let Command::CreateDevice(mut args) = create_device(physical, DEVICE, Vec::new()) else {
        unreachable!()
    };
    if let Some(info) = args.p_create_info.as_mut() {
        info.enabled_extension_count = exts.len() as u32;
        info.pp_enabled_extension_names = Some(exts.iter().map(|e| e.as_bytes()).collect());
    }
    Command::CreateDevice(args)
}

/// Boot the current context on its `physical` (of `physicals` enumerated),
/// a device with `exts`, its pool, queue and a command buffer.
fn device_on(h: &mut Harness<FakeVulkan>, physicals: &[u64], physical: u64, exts: &[&'static str]) {
    h.call(&enumerate_instance_version()).expect("version");
    h.call(&create_instance(INSTANCE)).expect("instance");
    h.call(&enumerate(INSTANCE, None)).expect("count");
    h.call(&enumerate(INSTANCE, Some(physicals.to_vec())))
        .expect("ids");
    let Command::CreateDevice(d) = h.call(&device_with(physical, exts)).expect("device") else {
        panic!()
    };
    assert_eq!(d.ret, VK_SUCCESS);
    h.send(&create_pool(DEVICE, POOL)).expect("pool");
    h.call(&device_queue(DEVICE, QUEUE, 1)).expect("queue");
    h.send(&allocate_cbs(DEVICE, POOL, &[CB], false))
        .expect("command buffer");
    assert!(!h.fatal());
}

/// Context 1 on the one GPU of an S1 host.
fn s1() -> (Harness<FakeVulkan>, Arc<FakeVulkan>) {
    s1_with(S1_EXTENSIONS)
}

fn s1_with(exts: &[&'static str]) -> (Harness<FakeVulkan>, Arc<FakeVulkan>) {
    let host = host_with(vec![gpu("NVIDIA GeForce RTX 2070", 7)]);
    let mut h = Harness::new(Arc::clone(&host));
    device_on(&mut h, &[PHYSICAL], PHYSICAL, exts);
    (h, host)
}

fn fatal_on(h: &mut Harness<FakeVulkan>, command: &Command<'_>, what: &str) {
    let head = h.call(command).expect_err(what);
    assert_eq!(head, h.last_start, "{what}: head stays before the command");
    assert!(h.fatal(), "{what}");
}

/// A fresh S1 harness per refusal, the refusal fatal to it.
fn refused(command: Command<'static>, what: &str) {
    let (mut h, _) = s1();
    fatal_on(&mut h, &command, what);
}

fn dma_buf_export() -> VkMemoryAllocateInfoNext {
    VkMemoryAllocateInfoNext::VkExportMemoryAllocateInfo(VkExportMemoryAllocateInfo {
        handle_types: policy::MEMORY_HANDLE_DMA_BUF,
    })
}

fn import_of(resource_id: u32) -> VkMemoryAllocateInfoNext {
    VkMemoryAllocateInfoNext::VkImportMemoryResourceInfoMESA(VkImportMemoryResourceInfoMESA {
        resource_id,
    })
}

fn dedicated_to(image: u64) -> VkMemoryAllocateInfoNext {
    VkMemoryAllocateInfoNext::VkMemoryDedicatedAllocateInfo(VkMemoryDedicatedAllocateInfo {
        image: VkImage(image),
        buffer: VkBuffer(0),
    })
}

/// How a modifier image names LINEAR.
enum Named {
    List(Vec<u64>),
    Explicit(u64, VkSubresourceLayout),
}

/// A DRM-modifier image create info as Zink builds it
/// (`zink_resource.c:1317-1379`): `DMA_BUF` external memory (venus's
/// rewrite of `OPAQUE_FD | DMA_BUF`), and the modifier named.
fn modifier_info(
    format: i32,
    usage: u32,
    flags: u32,
    views: Option<Vec<i32>>,
    named: Named,
) -> VkImageCreateInfo<'static> {
    let mut p_next = vec![VkImageCreateInfoNext::VkExternalMemoryImageCreateInfo(
        VkExternalMemoryImageCreateInfo {
            handle_types: policy::MEMORY_HANDLE_DMA_BUF,
        },
    )];
    if let Some(views) = views {
        p_next.push(VkImageCreateInfoNext::VkImageFormatListCreateInfo(
            VkImageFormatListCreateInfo {
                view_format_count: views.len() as u32,
                p_view_formats: Some(views),
            },
        ));
    }
    p_next.push(match named {
        Named::List(mods) => VkImageCreateInfoNext::VkImageDrmFormatModifierListCreateInfoEXT(
            VkImageDrmFormatModifierListCreateInfoEXT {
                drm_format_modifier_count: mods.len() as u32,
                p_drm_format_modifiers: Some(mods),
            },
        ),
        Named::Explicit(modifier, plane) => {
            VkImageCreateInfoNext::VkImageDrmFormatModifierExplicitCreateInfoEXT(
                VkImageDrmFormatModifierExplicitCreateInfoEXT {
                    drm_format_modifier: modifier,
                    drm_format_modifier_plane_count: 1,
                    p_plane_layouts: Some(vec![plane]),
                },
            )
        }
    });
    VkImageCreateInfo {
        p_next,
        flags,
        image_type: 1,
        format,
        extent: VkExtent3D {
            width: W,
            height: H,
            depth: 1,
        },
        mip_levels: 1,
        array_layers: 1,
        samples: 1,
        tiling: IMAGE_TILING_DRM_FORMAT_MODIFIER,
        usage,
        ..Default::default()
    }
}

/// The exporter's image, as Zink rebuilds a GBM scanout buffer at export:
/// mutable between UNORM and sRGB, colour, sampled and transfers, the list
/// `[LINEAR]`.
fn exporter_image() -> VkImageCreateInfo<'static> {
    modifier_info(
        BGRA,
        USAGE_COLOR | USAGE_SAMPLED | USAGE_TRANSFER,
        MUTABLE,
        Some(vec![BGRA, BGRA_SRGB]),
        Named::List(vec![DRM_FORMAT_MOD_LINEAR]),
    )
}

/// The importer's image, as Zink imports a dma-buf
/// (`zink_resource_from_handle`): no srgb list, the explicit LINEAR plane
/// with the stride the exporter reported.
fn importer_image(pitch: u64) -> VkImageCreateInfo<'static> {
    modifier_info(
        BGRA,
        USAGE_SAMPLED | USAGE_TRANSFER,
        0,
        None,
        Named::Explicit(
            DRM_FORMAT_MOD_LINEAR,
            VkSubresourceLayout {
                row_pitch: pitch,
                ..Default::default()
            },
        ),
    )
}

fn create(h: &mut Harness<FakeVulkan>, id: u64, info: VkImageCreateInfo<'static>) -> i32 {
    let Command::CreateImage(c) = h.call(&create_image(DEVICE, id, info)).expect("the create")
    else {
        panic!()
    };
    c.ret
}

fn requirements(h: &mut Harness<FakeVulkan>, image: u64) -> VkMemoryRequirements {
    let Command::GetImageMemoryRequirements2(r) =
        h.call(&memory_requirements(DEVICE, image)).unwrap()
    else {
        panic!()
    };
    r.p_memory_requirements.unwrap().memory_requirements
}

fn resource_properties(h: &mut Harness<FakeVulkan>, resource_id: u32) -> (i32, u32, u64) {
    let Command::GetMemoryResourcePropertiesMESA(p) = h
        .call(&Command::GetMemoryResourcePropertiesMESA(
            GetMemoryResourcePropertiesMESAArgs {
                device: VkDevice(DEVICE),
                resource_id,
                p_memory_resource_properties: Some(VkMemoryResourcePropertiesMESA {
                    p_next: vec![
                        VkMemoryResourcePropertiesMESANext::VkMemoryResourceAllocationSizePropertiesMESA(
                            Default::default(),
                        ),
                    ],
                    memory_type_bits: 0,
                }),
                ret: 0,
            },
        ))
        .unwrap()
    else {
        panic!()
    };
    let props = p.p_memory_resource_properties.unwrap();
    let VkMemoryResourcePropertiesMESANext::VkMemoryResourceAllocationSizePropertiesMESA(size) =
        &props.p_next[0];
    (p.ret, props.memory_type_bits, size.allocation_size)
}

const EXPORTER: u64 = 0x900;
const EXPORTED_MEM: u64 = 0x901;
const EXPORTED_RES: u32 = 90;
const IMPORTER: u64 = 0x910;
const IMPORTED_MEM: u64 = 0x911;

/// Context 1's export, as Zink and venus make it: the modifier image, its
/// requirements, an export allocation dedicated to it on a device-local type
/// (`vn_device_memory_alloc_export`), the bind, and the blob venus makes of
/// it at once. Answers the blob's size.
fn export(h: &mut Harness<FakeVulkan>) -> u64 {
    assert_eq!(create(h, EXPORTER, exporter_image()), VK_SUCCESS);
    let req = requirements(h, EXPORTER);
    assert_ne!(req.memory_type_bits & (1 << DEVICE_LOCAL_TYPE), 0);
    h.send(&allocate(
        DEVICE,
        EXPORTED_MEM,
        req.size,
        DEVICE_LOCAL_TYPE,
        vec![dma_buf_export(), dedicated_to(EXPORTER)],
    ))
    .expect("the export allocation");
    h.send(&bind_image(DEVICE, EXPORTER, EXPORTED_MEM, 0))
        .expect("bound");
    let blob = req.size.next_multiple_of(4096);
    h.memory_blob(h.ctx, EXPORTED_RES, EXPORTED_MEM, blob)
        .expect("a handle blob");
    assert!(!h.fatal());
    blob
}

/// A second guest process: context 2, on `physical` of `physicals`.
fn second_context(h: &mut Harness<FakeVulkan>, physicals: &[u64], physical: u64) {
    h.use_context(2);
    device_on(h, physicals, physical, S1_EXTENSIONS);
}

// ------------------------------------------------- the device it is shown

#[test]
fn an_exporting_host_is_shown_foreign_queues_and_linear_modifiers_and_enables_win32() {
    let (mut h, host) = s1();
    let Command::EnumerateDeviceExtensionProperties(c) = h
        .call(&Command::EnumerateDeviceExtensionProperties(
            EnumerateDeviceExtensionPropertiesArgs {
                physical_device: VkPhysicalDevice(PHYSICAL),
                p_layer_name: None,
                p_property_count: Some(0),
                p_properties: None,
                ret: 0,
            },
        ))
        .unwrap()
    else {
        panic!()
    };
    let n = c.p_property_count.unwrap();
    let Command::EnumerateDeviceExtensionProperties(e) = h
        .call(&Command::EnumerateDeviceExtensionProperties(
            EnumerateDeviceExtensionPropertiesArgs {
                physical_device: VkPhysicalDevice(PHYSICAL),
                p_layer_name: None,
                p_property_count: Some(n),
                p_properties: Some(vec![VkExtensionProperties::default(); n as usize]),
                ret: 0,
            },
        ))
        .unwrap()
    else {
        panic!()
    };
    let shown = e.p_properties.unwrap();
    for name in S1_EXTENSIONS {
        assert!(policy::has_extension(&shown, name), "{name}");
    }
    assert!(!policy::has_extension(
        &shown,
        policy::EXTERNAL_MEMORY_WIN32
    ));
    // The host device: queue_family_foreign and win32 enabled, the
    // emulated ones stripped.
    let request = host.device_requests().pop().unwrap();
    let has = |n: &str| request.extensions.iter().any(|e| e == n);
    assert!(has(policy::QUEUE_FAMILY_FOREIGN_EXT));
    assert!(has(policy::EXTERNAL_MEMORY_WIN32));
    assert!(!has(policy::IMAGE_DRM_FORMAT_MODIFIER));
    assert!(!has(policy::EXTERNAL_MEMORY_DMA_BUF));

    // A host that cannot export is shown neither, and keeps GBM's dumb
    // buffers.
    let mut plain = fake::zink_gpu("NVIDIA GeForce RTX 2070");
    plain
        .info
        .extensions
        .retain(|e| policy::c_name(&e.extension_name) != policy::EXTERNAL_MEMORY_WIN32.as_bytes());
    plain.info.extensions.push(VkExtensionProperties {
        extension_name: policy::name_array(policy::QUEUE_FAMILY_FOREIGN_EXT),
        spec_version: 1,
    });
    let guest = policy::expose(plain.info).expect("shown");
    assert!(!guest.memory_export);
    assert!(!policy::has_extension(
        &guest.extensions,
        policy::QUEUE_FAMILY_FOREIGN_EXT
    ));
    assert!(!policy::has_extension(
        &guest.extensions,
        policy::IMAGE_DRM_FORMAT_MODIFIER
    ));
    assert!(policy::has_extension(
        &guest.extensions,
        policy::EXTERNAL_MEMORY_DMA_BUF
    ));
}

// ------------------------------------------- exportable device-local memory

#[test]
fn a_device_local_export_is_a_handle_blob_that_cannot_be_mapped() {
    let (mut h, host) = s1();
    let blob = export(&mut h);
    assert_eq!(blob, PLANE, "the requirement was raised to the plane");
    // Exportable on the host, undedicated, and the handle held by the blob.
    let exported = host.exportable_allocations();
    assert_eq!(exported.len(), 1);
    assert_eq!(exported[0].1, DEVICE_LOCAL_TYPE);
    assert_eq!(host.live_shared_handles(), 1);
    assert_eq!(h.renderer.handle_blob_count(), 1);
    assert!(h.renderer.blob_pages(EXPORTED_RES).is_none(), "no pages");
    // Never mappable.
    assert!(matches!(
        h.renderer.map_blob(EXPORTED_RES, 0x80_0000, blob),
        Err(CommandError::BlobNotMappable(EXPORTED_RES))
    ));
    // Exported once: a second blob of the same memory is refused.
    assert!(h.memory_blob(CTX, 91, EXPORTED_MEM, blob).is_err());
    // A snapshot is refused while it lives, even with nothing else alive.
    assert!(h
        .renderer
        .snapshot_refusal()
        .is_some_and(|why| why.contains("Vulkan objects") || why.contains("dma-bufs")));
    h.send(&destroy_image(DEVICE, EXPORTER)).unwrap();
    h.send(&free(DEVICE, EXPORTED_MEM)).unwrap();
    h.send(&Command::DestroyInstance(DestroyInstanceArgs {
        instance: VkInstance(INSTANCE),
    }))
    .unwrap();
    assert_eq!(h.renderer.factory().host_objects(), 0);
    assert!(h
        .renderer
        .snapshot_refusal()
        .is_some_and(|why| why.contains("dma-bufs")));
    h.renderer.destroy_blob(EXPORTED_RES);
    assert_eq!(
        host.live_shared_handles(),
        0,
        "the handle is closed with its blob"
    );
    assert_eq!(h.renderer.snapshot_refusal(), None);
}

#[test]
fn a_device_local_export_without_a_handle_capable_host_is_refused_as_before() {
    let mut plain = gpu("NVIDIA GeForce RTX 2070", 7);
    plain
        .info
        .extensions
        .retain(|e| policy::c_name(&e.extension_name) != policy::EXTERNAL_MEMORY_WIN32.as_bytes());
    let host = host_with(vec![plain]);
    let mut h = Harness::new(Arc::clone(&host));
    device_on(
        &mut h,
        &[PHYSICAL],
        PHYSICAL,
        &[
            "VK_EXT_external_memory_dma_buf",
            "VK_KHR_external_memory_fd",
        ],
    );
    h.send(&allocate(
        DEVICE,
        EXPORTED_MEM,
        1 << 20,
        DEVICE_LOCAL_TYPE,
        vec![dma_buf_export()],
    ))
    .unwrap();
    assert!(host.exportable_allocations().is_empty());
    assert!(h
        .memory_blob(CTX, EXPORTED_RES, EXPORTED_MEM, 1 << 20)
        .is_err());
    assert!(!h.fatal());
}

#[test]
fn a_dma_buf_buffer_that_cannot_take_our_pages_is_created_for_a_handle() {
    let (mut h, host) = s1();
    host.buffer_imports.store(false, Ordering::SeqCst);
    let info = VkBufferCreateInfo {
        p_next: vec![VkBufferCreateInfoNext::VkExternalMemoryBufferCreateInfo(
            VkExternalMemoryBufferCreateInfo {
                handle_types: policy::MEMORY_HANDLE_DMA_BUF,
            },
        )],
        ..buffer_info(64 << 10, 0x3)
    };
    let Command::CreateBuffer(b) = h.call(&create_buffer(DEVICE, 0x600, info)).unwrap() else {
        panic!()
    };
    assert_eq!(b.ret, VK_SUCCESS);
    assert_eq!(
        host.resource_memories().last(),
        Some(&("buffer", ResourceMemory::Handle))
    );
    let Command::GetBufferMemoryRequirements2(r) =
        h.call(&buffer_requirements(DEVICE, 0x600)).unwrap()
    else {
        panic!()
    };
    let bits = r
        .p_memory_requirements
        .unwrap()
        .memory_requirements
        .memory_type_bits;
    assert_eq!(bits & 0x18, 0, "never our pages");
    assert_ne!(bits & (1 << DEVICE_LOCAL_TYPE), 0);
    // Its DMA_BUF query: shareable, through the export alone.
    let query = |h: &mut Harness<FakeVulkan>| {
        let Command::GetPhysicalDeviceExternalBufferProperties(q) = h
            .call(&Command::GetPhysicalDeviceExternalBufferProperties(
                GetPhysicalDeviceExternalBufferPropertiesArgs {
                    physical_device: VkPhysicalDevice(PHYSICAL),
                    p_external_buffer_info: Some(VkPhysicalDeviceExternalBufferInfo {
                        p_next: Vec::new(),
                        flags: 0,
                        usage: 0x3,
                        handle_type: policy::MEMORY_HANDLE_DMA_BUF as i32,
                    }),
                    p_external_buffer_properties: Some(Default::default()),
                },
            ))
            .unwrap()
        else {
            panic!()
        };
        q.p_external_buffer_properties
            .unwrap()
            .external_memory_properties
    };
    assert_eq!(query(&mut h), policy::external_memory_properties(true));
    host.buffer_exports.store(false, Ordering::SeqCst);
    assert_eq!(query(&mut h), policy::external_memory_properties(false));
    // Exportable memory binds only to a resource created for its handle.
    h.send(&allocate(
        DEVICE,
        0x601,
        64 << 10,
        DEVICE_LOCAL_TYPE,
        vec![dma_buf_export()],
    ))
    .unwrap();
    h.send(&bind_buffers(DEVICE, &[(0x600, 0x601, 0)])).unwrap();
    assert!(!h.fatal(), "the handle buffer takes the exportable memory");
    let Command::CreateBuffer(_) = h
        .call(&create_buffer(DEVICE, 0x602, buffer_info(64 << 10, 0x3)))
        .unwrap()
    else {
        panic!()
    };
    h.send(&allocate(
        DEVICE,
        0x603,
        64 << 10,
        DEVICE_LOCAL_TYPE,
        vec![dma_buf_export()],
    ))
    .unwrap();
    fatal_on(
        &mut h,
        &bind_buffers(DEVICE, &[(0x602, 0x603, 0)]),
        "exportable memory bound to a plain buffer",
    );
}

// ------------------------------------------------------ imports of a handle

#[test]
fn another_context_imports_the_handle_blob_and_it_outlives_the_exporter() {
    let host = host_with(vec![gpu("NVIDIA GeForce RTX 2070", 7)]);
    let mut h = Harness::new(Arc::clone(&host));
    device_on(&mut h, &[PHYSICAL], PHYSICAL, S1_EXTENSIONS);
    let blob = export(&mut h);
    let exporter_memory = host.exportable_allocations()[0].0;

    // The exporter goes first: its memory, image and whole context.
    h.send(&destroy_image(DEVICE, EXPORTER)).unwrap();
    h.send(&free(DEVICE, EXPORTED_MEM)).unwrap();
    h.renderer.ctx_destroy(CTX);
    assert_eq!(host.live_shared_handles(), 1, "the blob holds the handle");

    second_context(&mut h, &[PHYSICAL], PHYSICAL);
    // Not attached: out of reach.
    assert_eq!(
        resource_properties(&mut h, EXPORTED_RES).0,
        VK_ERROR_INVALID_EXTERNAL_HANDLE
    );
    h.renderer.ctx_attach_blob(2, EXPORTED_RES, true);
    assert_eq!(
        resource_properties(&mut h, EXPORTED_RES),
        (VK_SUCCESS, 1 << DEVICE_LOCAL_TYPE, blob),
        "the export's own type, and the blob's size"
    );
    // The importer creates "the same" image, explicitly LINEAR at the pitch
    // the exporter reported: the host sees the identical canonical image.
    assert_eq!(create(&mut h, IMPORTER, importer_image(PITCH)), VK_SUCCESS);
    let infos = host.image_infos();
    let (exporter, importer) = (&infos[infos.len() - 2], &infos[infos.len() - 1]);
    assert_eq!(exporter, importer, "byte-identical host create infos");
    assert_eq!(importer.tiling, 0, "optimal on the host");
    assert_eq!(importer.memory, ResourceMemory::Handle);
    assert_eq!(importer.flags, MUTABLE);
    assert_eq!(importer.view_formats, vec![BGRA, BGRA_SRGB]);
    assert_eq!(importer.usage, SUPERSET);
    assert_eq!(importer.extent, (W, H));
    let req = requirements(&mut h, IMPORTER);
    h.send(&allocate(
        DEVICE,
        IMPORTED_MEM,
        req.size,
        DEVICE_LOCAL_TYPE,
        vec![import_of(EXPORTED_RES), dedicated_to(IMPORTER)],
    ))
    .expect("the import");
    assert_eq!(
        host.handle_imports(),
        vec![HandleImport {
            payload: exporter_memory,
            size: blob,
            type_index: DEVICE_LOCAL_TYPE,
        }],
        "the exporter's payload, at the export's size and type"
    );
    h.send(&bind_image(DEVICE, IMPORTER, IMPORTED_MEM, 0))
        .unwrap();
    assert!(!h.fatal(), "imported and bound");
    // No second blob of an import, and the blob may go: the import holds
    // the allocation, not the handle.
    assert!(h.memory_blob(2, 92, IMPORTED_MEM, blob).is_err());
    h.renderer.destroy_blob(EXPORTED_RES);
    assert_eq!(host.live_shared_handles(), 0);
    // Imported memory binds only to a handle resource.
    let Command::CreateBuffer(_) = h
        .call(&create_buffer(DEVICE, 0x920, buffer_info(4096, 0x3)))
        .unwrap()
    else {
        panic!()
    };
    fatal_on(
        &mut h,
        &bind_buffers(DEVICE, &[(0x920, IMPORTED_MEM, 0)]),
        "imported device-local memory bound to a plain buffer",
    );
}

#[test]
fn imports_of_a_handle_blob_are_refused_in_vulkan_terms() {
    // Two GPUs of different UUIDs: context 1 exports on the first,
    // context 2 imports on the second.
    let host = host_with(vec![
        gpu("NVIDIA GeForce RTX 2070", 7),
        gpu("NVIDIA GeForce RTX 3070", 9),
    ]);
    let mut h = Harness::new(Arc::clone(&host));
    device_on(&mut h, &[PHYSICAL, PHYSICAL + 1], PHYSICAL, S1_EXTENSIONS);
    let blob = export(&mut h);
    second_context(&mut h, &[PHYSICAL, PHYSICAL + 1], PHYSICAL + 1);
    h.renderer.ctx_attach_blob(2, EXPORTED_RES, true);
    assert_eq!(
        resource_properties(&mut h, EXPORTED_RES).0,
        VK_ERROR_INVALID_EXTERNAL_HANDLE,
        "another GPU"
    );
    let before = host.live("memory");
    let import = |ty: u32, size: u64| {
        allocate(
            DEVICE,
            IMPORTED_MEM,
            size,
            ty,
            vec![import_of(EXPORTED_RES)],
        )
    };
    h.send(&import(DEVICE_LOCAL_TYPE, blob)).unwrap();
    assert_eq!(host.live("memory"), before, "across GPUs: nothing made");

    // The same GPU: a wrong type, a larger size, an unknown blob.
    let host = host_with(vec![gpu("NVIDIA GeForce RTX 2070", 7)]);
    let mut h = Harness::new(Arc::clone(&host));
    device_on(&mut h, &[PHYSICAL], PHYSICAL, S1_EXTENSIONS);
    let blob = export(&mut h);
    second_context(&mut h, &[PHYSICAL], PHYSICAL);
    h.renderer.ctx_attach_blob(2, EXPORTED_RES, true);
    let before = host.live("memory");
    for (ty, size, resource, what) in [
        (0, blob, EXPORTED_RES, "another device-local type"),
        (HOST_TYPE, blob, EXPORTED_RES, "our pages' type"),
        (
            DEVICE_LOCAL_TYPE,
            blob * 2,
            EXPORTED_RES,
            "larger than the blob",
        ),
        (DEVICE_LOCAL_TYPE, blob, 0xbeef, "an unknown blob"),
    ] {
        h.send(&allocate(
            DEVICE,
            IMPORTED_MEM,
            size,
            ty,
            vec![import_of(resource)],
        ))
        .unwrap();
        assert_eq!(host.live("memory"), before, "{what}: nothing made");
        assert!(!h.fatal(), "{what}");
    }
    assert!(host.handle_imports().is_empty());
    // A device that did not enable dma-buf imports nothing either.
    h.use_context(3);
    device_on(
        &mut h,
        &[PHYSICAL],
        PHYSICAL,
        &["VK_KHR_external_semaphore_fd"],
    );
    h.renderer.ctx_attach_blob(3, EXPORTED_RES, true);
    assert_eq!(
        resource_properties(&mut h, EXPORTED_RES).0,
        VK_ERROR_INVALID_EXTERNAL_HANDLE
    );
}

// ------------------------------------------- the modifier, as it is queried

fn format_query(format: i32, list2: bool, capacity: Option<u32>) -> Command<'static> {
    let link = if list2 {
        VkFormatProperties2Next::VkDrmFormatModifierPropertiesList2EXT(
            VkDrmFormatModifierPropertiesList2EXT {
                drm_format_modifier_count: capacity.unwrap_or(0),
                p_drm_format_modifier_properties: capacity
                    .map(|n| vec![Default::default(); n as usize]),
            },
        )
    } else {
        VkFormatProperties2Next::VkDrmFormatModifierPropertiesListEXT(
            VkDrmFormatModifierPropertiesListEXT {
                drm_format_modifier_count: capacity.unwrap_or(0),
                p_drm_format_modifier_properties: capacity
                    .map(|n| vec![Default::default(); n as usize]),
            },
        )
    };
    Command::GetPhysicalDeviceFormatProperties2(GetPhysicalDeviceFormatProperties2Args {
        physical_device: VkPhysicalDevice(PHYSICAL),
        format,
        p_format_properties: Some(VkFormatProperties2 {
            p_next: vec![
                VkFormatProperties2Next::VkFormatProperties3(Default::default()),
                link,
            ],
            ..Default::default()
        }),
    })
}

/// `(count, [(modifier, planes, features)])` of a format's modifier list.
fn modifiers_of(
    h: &mut Harness<FakeVulkan>,
    format: i32,
    capacity: Option<u32>,
) -> (u32, Vec<(u64, u32, u64)>) {
    let Command::GetPhysicalDeviceFormatProperties2(f) =
        h.call(&format_query(format, false, capacity)).unwrap()
    else {
        panic!()
    };
    let props = f.p_format_properties.unwrap();
    let VkFormatProperties2Next::VkDrmFormatModifierPropertiesListEXT(l) = &props.p_next[1] else {
        panic!()
    };
    let entries = l
        .p_drm_format_modifier_properties
        .iter()
        .flatten()
        .map(|p| {
            (
                p.drm_format_modifier,
                p.drm_format_modifier_plane_count,
                u64::from(p.drm_format_modifier_tiling_features),
            )
        })
        .collect();
    (l.drm_format_modifier_count, entries)
}

#[test]
fn only_linear_is_listed_only_for_scanout_formats_with_optimal_features() {
    let (mut h, _) = s1();
    // The count, then the list, as Zink asks (with room for 128).
    assert_eq!(modifiers_of(&mut h, BGRA, None), (1, Vec::new()));
    let (count, entries) = modifiers_of(&mut h, BGRA, Some(128));
    assert_eq!(count, 1);
    assert_eq!(
        entries,
        vec![(DRM_FORMAT_MOD_LINEAR, 1, u64::from(SCANOUT_FEATURES))],
        "the optimal features, one plane"
    );
    // sRGB: no storage with the export, so none in its features either.
    let (_, srgb) = modifiers_of(&mut h, BGRA_SRGB, Some(4));
    assert_eq!(srgb.len(), 1);
    assert_eq!(srgb[0].2 & 0x6, 0, "no STORAGE_IMAGE(_ATOMIC)");
    // Not a scanout format: none. The 10-bit pair are.
    for format in [RGBA8, 43, 58, 64] {
        assert_eq!(
            modifiers_of(&mut h, format, Some(4)).0,
            1,
            "format {format}"
        );
    }
    for format in [100, 126, 97] {
        assert_eq!(
            modifiers_of(&mut h, format, Some(4)),
            (0, Vec::new()),
            "format {format}"
        );
    }
    // The 64-bit list says the same.
    let Command::GetPhysicalDeviceFormatProperties2(f) =
        h.call(&format_query(BGRA, true, Some(2))).unwrap()
    else {
        panic!()
    };
    let props = f.p_format_properties.unwrap();
    let VkFormatProperties2Next::VkDrmFormatModifierPropertiesList2EXT(l) = &props.p_next[1] else {
        panic!()
    };
    assert_eq!(l.drm_format_modifier_count, 1);
    let p = &l.p_drm_format_modifier_properties.as_ref().unwrap()[0];
    assert_eq!(
        (
            p.drm_format_modifier,
            p.drm_format_modifier_plane_count,
            p.drm_format_modifier_tiling_features
        ),
        (DRM_FORMAT_MOD_LINEAR, 1, u64::from(SCANOUT_FEATURES))
    );
    // A host that cannot export: the list stays empty.
    host_cannot_export_lists_nothing();
}

fn host_cannot_export_lists_nothing() {
    let (mut h, host) = s1();
    host.image_exports.store(false, Ordering::SeqCst);
    assert_eq!(modifiers_of(&mut h, BGRA, Some(4)), (0, Vec::new()));
}

fn image_query(
    format: i32,
    usage: u32,
    flags: u32,
    modifier: u64,
    extra: Vec<VkPhysicalDeviceImageFormatInfo2Next>,
) -> Command<'static> {
    let mut p_next = vec![
        VkPhysicalDeviceImageFormatInfo2Next::VkPhysicalDeviceImageDrmFormatModifierInfoEXT(
            VkPhysicalDeviceImageDrmFormatModifierInfoEXT {
                drm_format_modifier: modifier,
                sharing_mode: 0,
                queue_family_index_count: 0,
                p_queue_family_indices: None,
            },
        ),
    ];
    p_next.extend(extra);
    Command::GetPhysicalDeviceImageFormatProperties2(GetPhysicalDeviceImageFormatProperties2Args {
        physical_device: VkPhysicalDevice(PHYSICAL),
        p_image_format_info: Some(VkPhysicalDeviceImageFormatInfo2 {
            p_next,
            format,
            type_: 1,
            tiling: IMAGE_TILING_DRM_FORMAT_MODIFIER,
            usage,
            flags,
        }),
        p_image_format_properties: Some(VkImageFormatProperties2 {
            p_next: vec![
                VkImageFormatProperties2Next::VkExternalImageFormatProperties(Default::default()),
                VkImageFormatProperties2Next::VkSamplerYcbcrConversionImageFormatProperties(
                    Default::default(),
                ),
            ],
            image_format_properties: Default::default(),
        }),
        ret: 0,
    })
}

fn image_answer(
    h: &mut Harness<FakeVulkan>,
    command: &Command<'_>,
) -> (i32, VkImageFormatProperties2) {
    let Command::GetPhysicalDeviceImageFormatProperties2(q) = h.call(command).unwrap() else {
        panic!()
    };
    (q.ret, q.p_image_format_properties.unwrap())
}

fn srgb_list() -> VkPhysicalDeviceImageFormatInfo2Next {
    VkPhysicalDeviceImageFormatInfo2Next::VkImageFormatListCreateInfo(VkImageFormatListCreateInfo {
        view_format_count: 2,
        p_view_formats: Some(vec![BGRA, BGRA_SRGB]),
    })
}

#[test]
fn the_linear_modifier_query_answers_the_canonical_image_and_nothing_else() {
    let (mut h, _) = s1();
    // What Zink's check_ici asks (`zink_resource.c:335-395`).
    let (ret, props) = image_answer(
        &mut h,
        &image_query(
            BGRA,
            USAGE_COLOR | USAGE_SAMPLED | USAGE_TRANSFER,
            MUTABLE,
            0,
            vec![srgb_list()],
        ),
    );
    assert_eq!(ret, VK_SUCCESS);
    let limits = &props.image_format_properties;
    assert_eq!(
        (
            limits.max_mip_levels,
            limits.max_array_layers,
            limits.sample_counts,
            limits.max_extent.depth
        ),
        (1, 1, 1, 1)
    );
    assert!(limits.max_extent.width >= W && limits.max_extent.height >= H);
    let VkImageFormatProperties2Next::VkExternalImageFormatProperties(e) = &props.p_next[0] else {
        panic!()
    };
    assert_eq!(
        e.external_memory_properties,
        policy::external_memory_properties(true)
    );
    let VkImageFormatProperties2Next::VkSamplerYcbcrConversionImageFormatProperties(y) =
        &props.p_next[1]
    else {
        panic!()
    };
    assert_eq!(y.combined_image_sampler_descriptor_count, 1);
    // The whole superset, and a DMA_BUF external query too.
    let dma_buf = VkPhysicalDeviceImageFormatInfo2Next::VkPhysicalDeviceExternalImageFormatInfo(
        VkPhysicalDeviceExternalImageFormatInfo {
            handle_type: policy::MEMORY_HANDLE_DMA_BUF as i32,
        },
    );
    assert_eq!(
        image_answer(&mut h, &image_query(BGRA, SUPERSET, 0, 0, vec![dma_buf])).0,
        VK_SUCCESS
    );
    // Everything else: not supported.
    let not = VK_ERROR_FORMAT_NOT_SUPPORTED;
    let opaque = VkPhysicalDeviceImageFormatInfo2Next::VkPhysicalDeviceExternalImageFormatInfo(
        VkPhysicalDeviceExternalImageFormatInfo { handle_type: 0x2 },
    );
    for (command, what) in [
        (
            image_query(BGRA, USAGE_SAMPLED, 0, 0x0300_0000_0000_0001, Vec::new()),
            "another modifier",
        ),
        (
            image_query(BGRA, 0x20, 0, 0, Vec::new()),
            "a usage outside the superset (depth attachment)",
        ),
        (
            image_query(BGRA_SRGB, USAGE_STORAGE, 0, 0, Vec::new()),
            "storage the sRGB export refuses",
        ),
        (
            image_query(BGRA, USAGE_SAMPLED, MUTABLE, 0, Vec::new()),
            "mutable without a list",
        ),
        (
            image_query(BGRA, USAGE_SAMPLED, 0x10, 0, Vec::new()),
            "a flag outside the canonical ones",
        ),
        (
            image_query(100, USAGE_SAMPLED, 0, 0, Vec::new()),
            "not a scanout format",
        ),
        (
            image_query(BGRA, USAGE_SAMPLED, 0, 0, vec![opaque]),
            "another external handle type",
        ),
    ] {
        assert_eq!(image_answer(&mut h, &command).0, not, "{what}");
    }
    let Command::GetPhysicalDeviceImageFormatProperties2(mut concurrent) =
        image_query(BGRA, USAGE_SAMPLED, 0, 0, Vec::new())
    else {
        panic!()
    };
    if let Some(
        VkPhysicalDeviceImageFormatInfo2Next::VkPhysicalDeviceImageDrmFormatModifierInfoEXT(m),
    ) = concurrent
        .p_image_format_info
        .as_mut()
        .unwrap()
        .p_next
        .first_mut()
    {
        m.sharing_mode = 1;
        m.queue_family_index_count = 2;
        m.p_queue_family_indices = Some(vec![0, 1]);
    }
    assert_eq!(
        image_answer(
            &mut h,
            &Command::GetPhysicalDeviceImageFormatProperties2(concurrent)
        )
        .0,
        not,
        "concurrent sharing"
    );
    assert!(!h.fatal());

    // Fatal: the modifier info without DRM tiling, DRM tiling without it.
    let Command::GetPhysicalDeviceImageFormatProperties2(mut optimal) =
        image_query(BGRA, USAGE_SAMPLED, 0, 0, Vec::new())
    else {
        panic!()
    };
    optimal.p_image_format_info.as_mut().unwrap().tiling = 0;
    refused(
        Command::GetPhysicalDeviceImageFormatProperties2(optimal),
        "a modifier info with optimal tiling",
    );
    let Command::GetPhysicalDeviceImageFormatProperties2(mut bare) =
        image_query(BGRA, USAGE_SAMPLED, 0, 0, Vec::new())
    else {
        panic!()
    };
    bare.p_image_format_info.as_mut().unwrap().p_next.clear();
    refused(
        Command::GetPhysicalDeviceImageFormatProperties2(bare),
        "DRM tiling without a modifier info",
    );
}

// ------------------------------------------- the modifier image, created

#[test]
fn a_modifier_image_is_the_canonical_optimal_image_shown_as_linear() {
    let (mut h, host) = s1();
    assert_eq!(create(&mut h, EXPORTER, exporter_image()), VK_SUCCESS);
    let info = host.image_infos().pop().unwrap();
    assert_eq!(
        info,
        fake::ImageInfo {
            format: BGRA,
            extent: (W, H),
            flags: MUTABLE,
            usage: SUPERSET,
            tiling: 0,
            view_formats: vec![BGRA, BGRA_SRGB],
            memory: ResourceMemory::Handle,
        }
    );
    // Its requirements: device-local only, raised to the plane.
    let req = requirements(&mut h, EXPORTER);
    assert_eq!(req.memory_type_bits & 0x18, 0);
    assert_eq!(req.size, PLANE, "1 MiB on the host, the plane's size shown");
    // vkGetImageDrmFormatModifierPropertiesEXT: LINEAR.
    let props = |image: u64| {
        Command::GetImageDrmFormatModifierPropertiesEXT(
            GetImageDrmFormatModifierPropertiesEXTArgs {
                device: VkDevice(DEVICE),
                image: VkImage(image),
                p_properties: Some(VkImageDrmFormatModifierPropertiesEXT {
                    drm_format_modifier: 0xdead,
                }),
                ret: 0,
            },
        )
    };
    let Command::GetImageDrmFormatModifierPropertiesEXT(p) = h.call(&props(EXPORTER)).unwrap()
    else {
        panic!()
    };
    assert_eq!(p.ret, VK_SUCCESS);
    assert_eq!(
        p.p_properties.unwrap().drm_format_modifier,
        DRM_FORMAT_MOD_LINEAR
    );
    // Its one memory plane: the synthesized LINEAR layout.
    let layout = |aspect: u32| {
        Command::GetImageSubresourceLayout(GetImageSubresourceLayoutArgs {
            device: VkDevice(DEVICE),
            image: VkImage(EXPORTER),
            p_subresource: Some(VkImageSubresource {
                aspect_mask: aspect,
                mip_level: 0,
                array_layer: 0,
            }),
            p_layout: Some(Default::default()),
        })
    };
    let Command::GetImageSubresourceLayout(l) =
        h.call(&layout(IMAGE_ASPECT_MEMORY_PLANE_0)).unwrap()
    else {
        panic!()
    };
    assert_eq!(
        l.p_layout.unwrap(),
        VkSubresourceLayout {
            offset: 0,
            size: PLANE,
            row_pitch: PITCH,
            array_pitch: PLANE,
            depth_pitch: PLANE,
        }
    );
    assert!(!h.fatal());
    // The colour aspect of a modifier image is not a memory plane.
    let (mut h2, _) = s1();
    assert_eq!(create(&mut h2, EXPORTER, exporter_image()), VK_SUCCESS);
    fatal_on(
        &mut h2,
        &layout(0x1),
        "the colour aspect of a modifier image",
    );
    // The modifier of an image of another tiling is no correct guest's to ask.
    let (mut h3, _) = s1();
    assert_eq!(create(&mut h3, IMAGE, image_info()), VK_SUCCESS);
    fatal_on(&mut h3, &props(IMAGE), "the modifier of an optimal image");
    // An odd width: its pitch rounds to 256.
    let (mut h4, host4) = s1();
    let mut odd = exporter_image();
    odd.extent.width = 1366;
    odd.extent.height = 768;
    assert_eq!(create(&mut h4, EXPORTER, odd), VK_SUCCESS);
    assert_eq!(host4.image_infos().pop().unwrap().extent, (1366, 768));
    let Command::GetImageSubresourceLayout(l) =
        h4.call(&layout(IMAGE_ASPECT_MEMORY_PLANE_0)).unwrap()
    else {
        panic!()
    };
    assert_eq!(
        l.p_layout.unwrap().row_pitch,
        5632,
        "1366 × 4 = 5464, rounded to 256"
    );
}

#[test]
fn an_explicit_layout_other_than_the_synthesized_one_is_answered_not_refused() {
    let (mut h, host) = s1();
    let before = host.image_requests();
    assert_eq!(
        create(&mut h, IMPORTER, importer_image(PITCH + 256)),
        VK_ERROR_INVALID_DRM_FORMAT_MODIFIER_PLANE_LAYOUT_EXT
    );
    assert_eq!(host.image_requests(), before, "no host image");
    assert!(!h.fatal());
    let mut offset = importer_image(PITCH);
    if let Some(VkImageCreateInfoNext::VkImageDrmFormatModifierExplicitCreateInfoEXT(e)) =
        offset.p_next.last_mut()
    {
        e.p_plane_layouts.as_mut().unwrap()[0].offset = 4096;
    }
    assert_eq!(
        create(&mut h, IMPORTER, offset),
        VK_ERROR_INVALID_DRM_FORMAT_MODIFIER_PLANE_LAYOUT_EXT
    );
    assert_eq!(create(&mut h, IMPORTER, importer_image(PITCH)), VK_SUCCESS);
}

#[test]
fn every_modifier_image_outside_the_emulation_is_refused() {
    let explicit_plane = VkSubresourceLayout {
        row_pitch: PITCH,
        ..Default::default()
    };
    let with = |f: &dyn Fn(&mut VkImageCreateInfo<'static>)| {
        let mut info = exporter_image();
        f(&mut info);
        create_image(DEVICE, EXPORTER, info)
    };
    for (command, what) in [
        (
            create_image(
                DEVICE,
                EXPORTER,
                modifier_info(BGRA, USAGE_SAMPLED | 0x20, 0, None, Named::List(vec![0])),
            ),
            "usage outside the superset",
        ),
        (
            create_image(
                DEVICE,
                EXPORTER,
                modifier_info(
                    BGRA,
                    USAGE_SAMPLED,
                    0,
                    None,
                    Named::Explicit(0x0300_0000_0000_0001, explicit_plane.clone()),
                ),
            ),
            "an explicit modifier other than LINEAR",
        ),
        (
            create_image(
                DEVICE,
                EXPORTER,
                modifier_info(
                    BGRA,
                    USAGE_SAMPLED,
                    0,
                    None,
                    Named::List(vec![0, 0x0300_0000_0000_0001]),
                ),
            ),
            "a list with a modifier other than LINEAR",
        ),
        (
            create_image(
                DEVICE,
                EXPORTER,
                modifier_info(BGRA, USAGE_SAMPLED, 0, None, Named::List(Vec::new())),
            ),
            "an empty list",
        ),
        (
            create_image(
                DEVICE,
                EXPORTER,
                modifier_info(100, USAGE_SAMPLED, 0, None, Named::List(vec![0])),
            ),
            "a format that is not a scanout format",
        ),
        (with(&|i| i.mip_levels = 2), "two mip levels"),
        (with(&|i| i.array_layers = 2), "two layers"),
        (with(&|i| i.samples = 4), "multisampled"),
        (
            with(&|i| i.flags = MUTABLE | 0x10),
            "a flag outside the canonical ones",
        ),
        (
            with(&|i| {
                i.p_next
                    .retain(|l| !matches!(l, VkImageCreateInfoNext::VkImageFormatListCreateInfo(_)))
            }),
            "mutable without a view format list",
        ),
        (
            with(&|i| {
                i.p_next.push(
                    VkImageCreateInfoNext::VkImageDrmFormatModifierExplicitCreateInfoEXT(
                        VkImageDrmFormatModifierExplicitCreateInfoEXT {
                            drm_format_modifier: 0,
                            drm_format_modifier_plane_count: 1,
                            p_plane_layouts: Some(vec![VkSubresourceLayout {
                                row_pitch: PITCH,
                                ..Default::default()
                            }]),
                        },
                    ),
                )
            }),
            "both a list and an explicit modifier",
        ),
        (
            with(&|i| {
                i.p_next.retain(|l| {
                    !matches!(
                        l,
                        VkImageCreateInfoNext::VkImageDrmFormatModifierListCreateInfoEXT(_)
                    )
                })
            }),
            "DRM tiling naming no modifier",
        ),
        (
            with(&|i| i.tiling = 0),
            "a modifier list on an optimal image",
        ),
        (
            create_image(
                DEVICE,
                EXPORTER,
                modifier_info(
                    BGRA,
                    USAGE_SAMPLED,
                    0,
                    None,
                    Named::Explicit(
                        0,
                        VkSubresourceLayout {
                            row_pitch: PITCH,
                            size: PLANE,
                            ..Default::default()
                        },
                    ),
                ),
            ),
            "an explicit plane with a size",
        ),
    ] {
        refused(command, what);
    }
    // A device that did not enable the extension has no DRM tiling at all.
    let (mut h, _) = s1_with(&[
        "VK_EXT_external_memory_dma_buf",
        "VK_KHR_external_memory_fd",
    ]);
    fatal_on(
        &mut h,
        &create_image(DEVICE, EXPORTER, exporter_image()),
        "DRM tiling without VK_EXT_image_drm_format_modifier",
    );
    // A canonical image the host wants dedicated memory for is answered
    // out of memory: its memory is exported undedicated.
    let (mut h, host) = s1();
    host.image_requires_dedicated.store(true, Ordering::SeqCst);
    assert_eq!(
        create(&mut h, EXPORTER, exporter_image()),
        VK_ERROR_OUT_OF_DEVICE_MEMORY
    );
    assert_eq!(host.live("image"), 0, "the host image went again");
    assert!(!h.fatal());
}

// ------------------------------------------- foreign queue families

fn foreign_barrier(src: u32, dst: u32) -> Command<'static> {
    image_barrier2_families(CB, IMAGE, (0, 6), (0, 0), (0x1000, 0x800), (src, dst))
}

#[test]
fn barriers_name_foreign_only_on_a_device_that_enabled_it() {
    use policy::{
        QUEUE_FAMILY_EXTERNAL as EXTERNAL, QUEUE_FAMILY_FOREIGN as FOREIGN,
        QUEUE_FAMILY_IGNORED as IGNORED,
    };
    let setup = |h: &mut Harness<FakeVulkan>| {
        assert_eq!(create(h, IMAGE, image_info()), VK_SUCCESS);
        h.send(&allocate(
            DEVICE,
            MEMORY,
            1 << 20,
            DEVICE_LOCAL_TYPE,
            Vec::new(),
        ))
        .unwrap();
        h.send(&bind_image(DEVICE, IMAGE, MEMORY, 0)).unwrap();
    };
    // Zink's release to and acquire from FOREIGN around a dma-buf.
    let (mut h, host) = s1();
    setup(&mut h);
    let outcome = h.submit_recording(&[
        begin(CB),
        foreign_barrier(FOREIGN, 0),
        foreign_barrier(0, FOREIGN),
        foreign_barrier(EXTERNAL, 0),
        foreign_barrier(IGNORED, IGNORED),
        foreign_barrier(1, 0),
        end(CB),
    ]);
    assert_eq!(outcome, Outcome::Consumed);
    assert!(!h.fatal());
    assert_eq!(host.called("vkCmdPipelineBarrier2"), 5);

    for (src, dst, what) in [
        (FOREIGN, EXTERNAL, "between the two external families"),
        (2, 0, "a family the device does not have"),
        (0x1234, 0, "a family index out of range"),
    ] {
        let (mut h, _) = s1();
        setup(&mut h);
        let outcome = h.submit_recording(&[begin(CB), foreign_barrier(src, dst), end(CB)]);
        assert!(matches!(outcome, Outcome::Fatal { .. }), "{what}");
    }
    // Without the extension enabled, FOREIGN is refused in every form.
    for command in [
        foreign_barrier(FOREIGN, 0),
        Command::CmdPipelineBarrier(CmdPipelineBarrierArgs {
            command_buffer: VkCommandBuffer(CB),
            src_stage_mask: 0x1,
            dst_stage_mask: 0x1000,
            dependency_flags: 0,
            memory_barrier_count: 0,
            p_memory_barriers: None,
            buffer_memory_barrier_count: 0,
            p_buffer_memory_barriers: None,
            image_memory_barrier_count: 1,
            p_image_memory_barriers: Some(vec![VkImageMemoryBarrier {
                p_next: Vec::new(),
                src_access_mask: 0,
                dst_access_mask: 0x800,
                old_layout: 0,
                new_layout: 6,
                src_queue_family_index: 0,
                dst_queue_family_index: FOREIGN,
                image: VkImage(IMAGE),
                subresource_range: color_range(),
            }]),
        }),
    ] {
        let (mut h, _) = s1_with(&[
            "VK_EXT_external_memory_dma_buf",
            "VK_KHR_external_memory_fd",
        ]);
        setup(&mut h);
        let outcome = h.submit_recording(&[begin(CB), command, end(CB)]);
        assert!(
            matches!(outcome, Outcome::Fatal { .. }),
            "FOREIGN without the extension"
        );
    }
    // A set event transfers nothing.
    let (mut h, _) = s1();
    setup(&mut h);
    h.send(&Command::CreateEvent(CreateEventArgs {
        device: VkDevice(DEVICE),
        p_create_info: Some(VkEventCreateInfo { flags: 0 }),
        p_event: Some(VkEvent(0x7e0)),
        ret: 0,
    }))
    .unwrap();
    let Command::CmdPipelineBarrier2(barrier) = foreign_barrier(0, 1) else {
        panic!()
    };
    let outcome = h.submit_recording(&[
        begin(CB),
        Command::CmdSetEvent2(CmdSetEvent2Args {
            command_buffer: VkCommandBuffer(CB),
            event: VkEvent(0x7e0),
            p_dependency_info: barrier.p_dependency_info,
        }),
        end(CB),
    ]);
    assert!(
        matches!(outcome, Outcome::Fatal { .. }),
        "a set event's transfer"
    );
}

// ------------------------------------------------------------- teardown

#[test]
fn a_reset_leaves_no_handle_no_import_and_no_blob() {
    let host = host_with(vec![gpu("NVIDIA GeForce RTX 2070", 7)]);
    let mut h = Harness::new(Arc::clone(&host));
    device_on(&mut h, &[PHYSICAL], PHYSICAL, S1_EXTENSIONS);
    let blob = export(&mut h);
    second_context(&mut h, &[PHYSICAL], PHYSICAL);
    h.renderer.ctx_attach_blob(2, EXPORTED_RES, true);
    assert_eq!(create(&mut h, IMPORTER, importer_image(PITCH)), VK_SUCCESS);
    h.send(&allocate(
        DEVICE,
        IMPORTED_MEM,
        blob,
        DEVICE_LOCAL_TYPE,
        vec![import_of(EXPORTED_RES)],
    ))
    .unwrap();
    h.send(&bind_image(DEVICE, IMPORTER, IMPORTED_MEM, 0))
        .unwrap();
    assert!(!h.fatal());
    assert_eq!(host.handle_imports().len(), 1);
    assert!(h.renderer.snapshot_refusal().is_some());
    // Destroying the importer's context takes its import with it; the
    // exporter's blob and handle stay, the guest's to unref.
    h.renderer.ctx_destroy(2);
    assert_eq!(host.live_shared_handles(), 1);
    h.renderer.reset();
    assert_eq!(host.live_objects(), 0, "every host object is gone");
    assert_eq!(host.live_shared_handles(), 0, "every handle closed");
    assert_eq!(h.renderer.blob_count(), 0);
    assert_eq!(h.renderer.handle_blob_count(), 0);
    assert_eq!(h.renderer.snapshot_refusal(), None);
}

#[test]
fn a_dma_buf_optimal_image_is_created_for_a_handle_and_a_linear_one_for_our_pages() {
    let (mut h, host) = s1();
    let external = |tiling: i32| VkImageCreateInfo {
        p_next: vec![VkImageCreateInfoNext::VkExternalMemoryImageCreateInfo(
            VkExternalMemoryImageCreateInfo {
                handle_types: policy::MEMORY_HANDLE_DMA_BUF,
            },
        )],
        tiling,
        ..image_info()
    };
    // Zink's shared optimal images (`OPAQUE_FD` rewritten to `DMA_BUF`).
    assert_eq!(create(&mut h, 0x610, external(0)), VK_SUCCESS);
    assert_eq!(
        host.resource_memories().last(),
        Some(&("image", ResourceMemory::Handle))
    );
    // A linear one the guest may map stays in our pages.
    assert_eq!(create(&mut h, 0x611, external(1)), VK_SUCCESS);
    assert_eq!(
        host.resource_memories().last(),
        Some(&("image", ResourceMemory::HostPages))
    );
    // Not external: plain, as ever.
    assert_eq!(create(&mut h, 0x612, image_info()), VK_SUCCESS);
    assert_eq!(
        host.resource_memories().last(),
        Some(&("image", ResourceMemory::Plain))
    );
    // A host that will not export it: plain too.
    host.image_exports.store(false, Ordering::SeqCst);
    assert_eq!(create(&mut h, 0x613, external(0)), VK_SUCCESS);
    assert_eq!(
        host.resource_memories().last(),
        Some(&("image", ResourceMemory::Plain))
    );
    assert!(!h.fatal());
}

struct Wakes(std::sync::atomic::AtomicUsize);

impl virtio_core::HostWaker for Wakes {
    fn wake(&self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

/// Stage S1 flipped `SYNC_FD` exportable, so an application's
/// `vkGetSemaphoreFdKHR` (`vn_queue.c:2440-2495`) now reaches the renderer.
/// On a device-only payload it is exactly: `vn_create_sync_file`
/// (`:1873-1910`) — a virtio-gpu execbuffer on the `ring_idx` of the queue
/// that signalled the semaphore, carrying `vkWaitRingSeqnoMESA` for the
/// submit — then `vkWaitSemaphoreResourceMESA`. The fence retires after the
/// work, and the wait consumes the payload on the host.
#[test]
fn an_application_sync_file_export_is_a_ring_fence_then_the_semaphore_wait() {
    use crate::renderer::{FenceOutcome, FenceTimeline};
    const SEM: u64 = 0x7a0;
    let (mut h, host) = s1();
    h.renderer
        .set_host_waker(Arc::new(Wakes(std::sync::atomic::AtomicUsize::new(0))));
    let Command::GetPhysicalDeviceExternalSemaphoreProperties(p) = h
        .call(&external_semaphore_query(PHYSICAL, 0x10, false))
        .unwrap()
    else {
        panic!()
    };
    assert_eq!(
        p.p_external_semaphore_properties
            .unwrap()
            .external_semaphore_features,
        policy::SEMAPHORE_FEATURE_IMPORTABLE | policy::SEMAPHORE_FEATURE_EXPORTABLE
    );
    // The application: a semaphore exportable as a sync file (the SYNC_FD
    // bit never reaches the host), signalled by a submit on the queue of
    // ring_idx 1.
    h.send(&create_semaphore(DEVICE, SEM, None, 0x10)).unwrap();
    assert_eq!(host.semaphore_exports().last(), Some(&None));
    h.send(&submit_semaphores(QUEUE, &[], &[], &[(SEM, 0)], false, 0))
        .unwrap();
    // vn_create_sync_file: the ring position after that submit, waited for
    // on the context stream, then a fence on ring_idx 1.
    let seqno = u64::from(h.tail());
    h.wait_ring_seqno(seqno).expect("the submit was executed");
    let (timeline, outcome) = h.renderer.create_fence_on(CTX, Some(1), 77).unwrap();
    assert_eq!(
        timeline,
        FenceTimeline::Ring {
            ctx_id: CTX,
            ring_idx: 1
        }
    );
    if outcome == FenceOutcome::Pending {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let mut retired = Vec::new();
        while retired.is_empty() && std::time::Instant::now() < deadline {
            retired.extend(h.renderer.poll_fence_timelines(0));
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        assert_eq!(retired, vec![(timeline, 77)], "the sync file signals");
    }
    // Then the host semaphore's payload is consumed by an empty submit
    // waiting on it.
    h.send(&wait_semaphore_resource(DEVICE, SEM)).unwrap();
    let ops = host.semaphore_ops();
    let last = ops.last().expect("a submit");
    assert_eq!(last.1.len(), 1, "one wait: the exported semaphore");
    assert!(last.2.is_empty(), "and nothing signalled");
    assert!(!h.fatal());
}
