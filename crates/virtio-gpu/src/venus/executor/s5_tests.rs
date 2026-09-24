//! Stage S5 ("GNOME on the GPU"): Vulkan and GL clients present through
//! dma-buf. Against the fake host, every command Mesa 26.0.8's native WSI
//! sends for a swapchain image (`wsi_configure_native_image`,
//! `wsi_create_native_image_mem`, `wsi_common_drm.c:579-840`), its
//! implicit-sync probe (`wsi_drm_check_dma_buf_sync_file_import_export`,
//! `:95-176`), venus's present-time release (`vn_command_buffer.c:202-270`)
//! and the compositor's import of the result through Zink
//! (`zink_resource.c:1317-1379`, `:1650-1653`) — for vkcube's swapchain and
//! for Zink's kopper, whose images are mutable.

use crate::renderer::Renderer3d;
use crate::venus::protocol::*;

use super::fake::FakeVulkan;
use super::harness::*;
use super::host::ResourceMemory;
use super::modifier::{
    DRM_FORMAT_MOD_LINEAR, IGNORED_FLAGS, IMAGE_ASPECT_MEMORY_PLANE_0,
    IMAGE_TILING_DRM_FORMAT_MODIFIER,
};
use super::policy::{self, QUEUE_FAMILY_FOREIGN as FOREIGN};
use super::recording::*;
use super::s1_tests::*;

/// `VK_IMAGE_CREATE_ALIAS_BIT`, on every swapchain image (`wsi_common.c:671`).
const ALIAS: u32 = 0x400;
/// `VK_IMAGE_CREATE_EXTENDED_USAGE_BIT`, with `MUTABLE` for a mutable
/// swapchain (`wsi_common.c:706-707`).
const EXTENDED_USAGE: u32 = 0x100;
/// `VK_IMAGE_USAGE_INPUT_ATTACHMENT_BIT`, which kopper adds when the
/// surface offers it (`zink_kopper.c:296-297`).
const USAGE_INPUT: u32 = 0x80;
/// `VK_IMAGE_LAYOUT_GENERAL`: venus's `VN_PRESENT_SRC_INTERNAL_LAYOUT`
/// (`vn_image.h:19`).
const GENERAL: i32 = 1;
/// `VK_IMAGE_LAYOUT_PREINITIALIZED`: what Zink starts an imported dma-buf in
/// (`zink_resource.c:1653`).
const PREINITIALIZED: i32 = 8;
/// `VK_IMAGE_LAYOUT_SHADER_READ_ONLY_OPTIMAL`.
const SHADER_READ: i32 = 5;

const SWAPCHAIN_IMAGE: u64 = 0xa00;
const SWAPCHAIN_MEM: u64 = 0xa01;
const SWAPCHAIN_RES: u32 = 0xa0;
const PROBE_MEM: u64 = 0xa10;
const PROBE_RES: u32 = 0xa1;
const COMPOSITOR_IMAGE: u64 = 0xa20;
const COMPOSITOR_MEM: u64 = 0xa21;

/// A swapchain as an application asks for one.
struct Swapchain {
    /// What it is called in assertions.
    name: &'static str,
    /// `imageUsage`.
    usage: u32,
    /// The swapchain's `VK_SWAPCHAIN_CREATE_MUTABLE_FORMAT_BIT_KHR`, and so
    /// its format list.
    mutable: bool,
}

/// vkcube's (`cube.c`, colour attachment only) and Zink kopper's
/// (`zink_kopper.c:283-297`: transfers, sampled, colour, input attachment,
/// mutable between UNORM and sRGB).
const SWAPCHAINS: [Swapchain; 2] = [
    Swapchain {
        name: "vkcube",
        usage: USAGE_COLOR,
        mutable: false,
    },
    Swapchain {
        name: "kopper",
        usage: USAGE_TRANSFER | USAGE_SAMPLED | USAGE_COLOR | USAGE_INPUT,
        mutable: true,
    },
];

impl Swapchain {
    /// The image create flags `wsi_configure_image` gives it.
    fn flags(&self) -> u32 {
        if self.mutable {
            ALIAS | MUTABLE | EXTENDED_USAGE
        } else {
            ALIAS
        }
    }

    fn views(&self) -> Option<Vec<i32>> {
        self.mutable.then(|| vec![BGRA, BGRA_SRGB])
    }

    /// The format query `wsi_configure_native_image` makes for every
    /// modifier the driver lists (`wsi_common_drm.c:633-676`): DRM tiling,
    /// the image's flags and usage, the list when mutable, the modifier and
    /// the swapchain's (exclusive) sharing. Mesa's own
    /// `wsi_image_create_info` is chained too, and venus's encoder drops it.
    fn query(&self) -> Command<'static> {
        let mut p_next = vec![
            VkPhysicalDeviceImageFormatInfo2Next::VkPhysicalDeviceImageDrmFormatModifierInfoEXT(
                VkPhysicalDeviceImageDrmFormatModifierInfoEXT {
                    drm_format_modifier: DRM_FORMAT_MOD_LINEAR,
                    sharing_mode: 0,
                    queue_family_index_count: 0,
                    p_queue_family_indices: None,
                },
            ),
        ];
        if let Some(views) = self.views() {
            p_next.insert(
                0,
                VkPhysicalDeviceImageFormatInfo2Next::VkImageFormatListCreateInfo(
                    VkImageFormatListCreateInfo {
                        view_format_count: 2,
                        p_view_formats: Some(views),
                    },
                ),
            );
        }
        Command::GetPhysicalDeviceImageFormatProperties2(
            GetPhysicalDeviceImageFormatProperties2Args {
                physical_device: VkPhysicalDevice(PHYSICAL),
                p_image_format_info: Some(VkPhysicalDeviceImageFormatInfo2 {
                    p_next,
                    format: BGRA,
                    type_: 1,
                    tiling: IMAGE_TILING_DRM_FORMAT_MODIFIER,
                    usage: self.usage,
                    flags: self.flags(),
                }),
                p_image_format_properties: Some(VkImageFormatProperties2::default()),
                ret: 0,
            },
        )
    }

    /// The swapchain image `wsi_create_image` makes: `DMA_BUF` external
    /// memory, the list when mutable, and the modifiers that survived the
    /// query — `[LINEAR]`.
    fn image(&self) -> VkImageCreateInfo<'static> {
        modifier_info(
            BGRA,
            self.usage,
            self.flags(),
            self.views(),
            Named::List(vec![DRM_FORMAT_MOD_LINEAR]),
        )
    }
}

fn query_ret(h: &mut Harness<FakeVulkan>, command: &Command<'_>) -> i32 {
    let Command::GetPhysicalDeviceImageFormatProperties2(q) = h.call(command).unwrap() else {
        panic!()
    };
    q.ret
}

/// `wsi_create_native_image_mem`: the requirements, one dedicated
/// allocation exported as `DMA_BUF` on a device-local type, the blob venus
/// makes of it at once, the bind; then the modifier and the plane the WSI
/// hands the compositor. Answers the blob's size and the plane.
fn swapchain_image(
    h: &mut Harness<FakeVulkan>,
    swapchain: &Swapchain,
) -> (u64, VkSubresourceLayout) {
    let name = swapchain.name;
    assert_eq!(
        query_ret(h, &swapchain.query()),
        VK_SUCCESS,
        "{name}: LINEAR survives the WSI's filter, or the swapchain fails"
    );
    assert_eq!(
        create(h, SWAPCHAIN_IMAGE, swapchain.image()),
        VK_SUCCESS,
        "{name}"
    );
    let req = requirements(h, SWAPCHAIN_IMAGE);
    assert_ne!(req.memory_type_bits & (1 << DEVICE_LOCAL_TYPE), 0, "{name}");
    assert_eq!(
        req.memory_type_bits & (1 << HOST_TYPE),
        0,
        "{name}: never our pages"
    );
    h.send(&allocate(
        DEVICE,
        SWAPCHAIN_MEM,
        req.size,
        DEVICE_LOCAL_TYPE,
        vec![dedicated_to(SWAPCHAIN_IMAGE), dma_buf_export()],
    ))
    .expect("the export allocation");
    let blob = req.size.next_multiple_of(4096);
    h.memory_blob(h.ctx, SWAPCHAIN_RES, SWAPCHAIN_MEM, blob)
        .expect("a handle blob");
    h.send(&bind_image(DEVICE, SWAPCHAIN_IMAGE, SWAPCHAIN_MEM, 0))
        .expect("bound");
    let Command::GetImageDrmFormatModifierPropertiesEXT(m) = h
        .call(&Command::GetImageDrmFormatModifierPropertiesEXT(
            GetImageDrmFormatModifierPropertiesEXTArgs {
                device: VkDevice(DEVICE),
                image: VkImage(SWAPCHAIN_IMAGE),
                p_properties: Some(VkImageDrmFormatModifierPropertiesEXT {
                    drm_format_modifier: 0xdead,
                }),
                ret: 0,
            },
        ))
        .unwrap()
    else {
        panic!()
    };
    assert_eq!(m.ret, VK_SUCCESS);
    assert_eq!(
        m.p_properties.unwrap().drm_format_modifier,
        DRM_FORMAT_MOD_LINEAR
    );
    let Command::GetImageSubresourceLayout(l) = h
        .call(&Command::GetImageSubresourceLayout(
            GetImageSubresourceLayoutArgs {
                device: VkDevice(DEVICE),
                image: VkImage(SWAPCHAIN_IMAGE),
                p_subresource: Some(VkImageSubresource {
                    aspect_mask: IMAGE_ASPECT_MEMORY_PLANE_0,
                    mip_level: 0,
                    array_layer: 0,
                }),
                p_layout: Some(Default::default()),
            },
        ))
        .unwrap()
    else {
        panic!()
    };
    assert!(!h.fatal(), "{name}");
    (blob, l.p_layout.unwrap())
}

#[test]
fn a_native_wsi_swapchain_image_is_the_canonical_image_in_a_handle_blob() {
    assert_eq!(IGNORED_FLAGS, ALIAS | EXTENDED_USAGE);
    for swapchain in &SWAPCHAINS {
        let name = swapchain.name;
        let (mut h, host) = s1();
        let (blob, plane) = swapchain_image(&mut h, swapchain);
        assert_eq!(
            (plane.offset, plane.row_pitch),
            (0, PITCH),
            "{name}: the plane the compositor is told"
        );
        assert!(blob >= plane.size, "{name}");
        // The host image is the canonical one, exactly as an importer's and
        // Zink's GBM export's are: neither ALIAS nor EXTENDED_USAGE reaches
        // it.
        let wsi = host.image_infos().pop().unwrap();
        assert_eq!(create(&mut h, EXPORTER, exporter_image()), VK_SUCCESS);
        let zink = host.image_infos().pop().unwrap();
        assert_eq!(wsi, zink, "{name}: byte-identical host create infos");
        assert_eq!(wsi.flags, MUTABLE, "{name}");
        assert_eq!(wsi.memory, ResourceMemory::Handle, "{name}");
        assert_eq!(wsi.tiling, 0, "{name}: optimal on the host");
        assert_eq!(host.exportable_allocations().len(), 1, "{name}");
        assert!(!h.fatal(), "{name}");
    }
}

#[test]
fn ignored_flags_are_the_only_flags_a_modifier_image_may_add() {
    // Each of the two alone, on a format with no twin too.
    for flags in [ALIAS, EXTENDED_USAGE, ALIAS | EXTENDED_USAGE] {
        let (mut h, _) = s1();
        let mut info = exporter_image();
        info.flags |= flags;
        assert_eq!(create(&mut h, EXPORTER, info), VK_SUCCESS, "{flags:#x}");
        let (mut h, _) = s1();
        let a2r10 = modifier_info(
            58,
            USAGE_COLOR,
            flags,
            None,
            Named::List(vec![DRM_FORMAT_MOD_LINEAR]),
        );
        assert_eq!(create(&mut h, EXPORTER, a2r10), VK_SUCCESS, "{flags:#x}");
    }
    // Anything else still is not: CUBE_COMPATIBLE, 2D_ARRAY_COMPATIBLE,
    // BLOCK_TEXEL_VIEW_COMPATIBLE — a query answers not supported, and a
    // create after that answer is fatal.
    for flag in [0x10, 0x20, 0x80] {
        let swapchain = Swapchain {
            name: "odd",
            usage: USAGE_COLOR,
            mutable: false,
        };
        let Command::GetPhysicalDeviceImageFormatProperties2(mut q) = swapchain.query() else {
            panic!()
        };
        q.p_image_format_info.as_mut().unwrap().flags |= flag;
        let (mut h, _) = s1();
        assert_eq!(
            query_ret(&mut h, &Command::GetPhysicalDeviceImageFormatProperties2(q)),
            VK_ERROR_FORMAT_NOT_SUPPORTED,
            "{flag:#x}"
        );
        let mut info = swapchain.image();
        info.flags |= flag;
        let head = h
            .call(&create_image(DEVICE, SWAPCHAIN_IMAGE, info))
            .expect_err("fatal");
        assert_eq!(head, h.last_start);
        assert!(h.fatal(), "{flag:#x}");
    }
    // A mutable swapchain's usage is still held to the superset of its own
    // format: storage on the sRGB format, which the host will not export,
    // is not supported even with EXTENDED_USAGE.
    let (mut h, _) = s1();
    let Command::GetPhysicalDeviceImageFormatProperties2(mut q) = SWAPCHAINS[1].query() else {
        panic!()
    };
    let info = q.p_image_format_info.as_mut().unwrap();
    info.format = BGRA_SRGB;
    info.usage |= 0x8;
    assert_eq!(
        query_ret(&mut h, &Command::GetPhysicalDeviceImageFormatProperties2(q)),
        VK_ERROR_FORMAT_NOT_SUPPORTED
    );
}

/// The probe `wsi_drm_init_swapchain_implicit_sync` runs once per device
/// before it chooses how a present signals the dma-buf: 4096 bytes of
/// memory type 0, exported as `DMA_BUF`, undedicated. If it fails the WSI
/// asks the driver for implicit sync instead (`implicit_sync = true`), which
/// venus ignores — so a present would carry no fence the compositor could
/// wait for. It must succeed, and make a blob Mesa can export.
#[test]
fn the_implicit_sync_probe_is_an_exportable_allocation_with_a_blob() {
    let (mut h, host) = s1();
    let before = host.exportable_allocations().len();
    h.send(&allocate(
        DEVICE,
        PROBE_MEM,
        4096,
        0,
        vec![dma_buf_export()],
    ))
    .expect("the probe");
    h.memory_blob(h.ctx, PROBE_RES, PROBE_MEM, 4096)
        .expect("its blob, which Mesa exports as a dma-buf");
    assert_eq!(host.exportable_allocations().len(), before + 1);
    h.renderer.destroy_blob(PROBE_RES);
    h.send(&free(DEVICE, PROBE_MEM)).expect("freed");
    assert!(!h.fatal());
}

/// A frame from present to composite: the client renders, venus releases
/// the image to `FOREIGN` in `GENERAL` at the end of the render pass
/// (`vn_cmd_fix_image_memory_barrier_common`, its `PRESENT_SRC` replaced by
/// `VN_PRESENT_SRC_INTERNAL_LAYOUT`); the compositor, another context,
/// attaches the resource, asks its properties, creates the image Zink
/// imports (explicit LINEAR at the plane's pitch, no list, `PREINITIALIZED`),
/// imports the blob, and acquires the image from `FOREIGN` out of
/// `PREINITIALIZED` into a shader-read layout (`zink_resource.c:1650-1653`,
/// `zink_synchronization.cpp:219`). The next frame venus acquires it back
/// out of `GENERAL`.
#[test]
fn the_compositor_imports_a_presented_swapchain_image_and_acquires_it_from_foreign() {
    for swapchain in &SWAPCHAINS {
        let name = swapchain.name;
        let (mut h, host) = s1();
        let (blob, plane) = swapchain_image(&mut h, swapchain);
        let present = h.submit_recording(&[
            begin(CB),
            image_barrier2_families(
                CB,
                SWAPCHAIN_IMAGE,
                (GENERAL, GENERAL),
                (0x400, 0x100),
                (0, 0),
                (0, FOREIGN),
            ),
            end(CB),
        ]);
        assert_eq!(present, Outcome::Consumed, "{name}: the release");
        let client_image = host.image_barriers().last().unwrap().image;

        second_context(&mut h, &[PHYSICAL], PHYSICAL);
        h.renderer.ctx_attach_blob(2, SWAPCHAIN_RES, true);
        let mut import = importer_image(plane.row_pitch);
        import.initial_layout = PREINITIALIZED;
        assert_eq!(
            create(&mut h, COMPOSITOR_IMAGE, import),
            VK_SUCCESS,
            "{name}"
        );
        let req = requirements(&mut h, COMPOSITOR_IMAGE);
        assert!(req.size <= blob, "{name}");
        h.send(&allocate(
            DEVICE,
            COMPOSITOR_MEM,
            req.size,
            DEVICE_LOCAL_TYPE,
            vec![import_of(SWAPCHAIN_RES), dedicated_to(COMPOSITOR_IMAGE)],
        ))
        .expect("the import");
        h.send(&bind_image(DEVICE, COMPOSITOR_IMAGE, COMPOSITOR_MEM, 0))
            .unwrap();
        let composite = h.submit_recording(&[
            begin(CB),
            image_barrier2_families(
                CB,
                COMPOSITOR_IMAGE,
                (PREINITIALIZED, SHADER_READ),
                (0, 0),
                (0x80, 0x20),
                (FOREIGN, 0),
            ),
            // Zink's release at the end of its batch (`zink_batch.c:900-934`).
            image_barrier2_families(
                CB,
                COMPOSITOR_IMAGE,
                (SHADER_READ, SHADER_READ),
                (0x80, 0x20),
                (0x1_0000, 0),
                (0, FOREIGN),
            ),
            end(CB),
        ]);
        assert_eq!(composite, Outcome::Consumed, "{name}: the composite");
        assert!(!h.fatal(), "{name}");
        let barriers = host.image_barriers();
        let acquire = &barriers[barriers.len() - 2];
        assert_ne!(acquire.image, client_image, "{name}: its own host image");
        assert_eq!(
            (acquire.layouts, acquire.families),
            ((PREINITIALIZED, SHADER_READ), (FOREIGN, 0)),
            "{name}: passed through as Zink sent it"
        );

        // The client's next frame: acquired back out of GENERAL.
        h.use_context(CTX);
        let next = h.submit_recording(&[
            begin(CB),
            image_barrier2_families(
                CB,
                SWAPCHAIN_IMAGE,
                (GENERAL, 2),
                (0, 0),
                (0x400, 0x100),
                (FOREIGN, 0),
            ),
            end(CB),
        ]);
        assert_eq!(next, Outcome::Consumed, "{name}: the next acquire");
        assert!(!h.fatal(), "{name}");
    }
}

/// Where the path is chosen: an exporting NVIDIA host shows the guest a
/// driver at venus's dma-buf WSI gate and the modifier extension, which is
/// what `vn_wsi_init` turns native dma-buf WSI on for.
#[test]
fn an_exporting_nvidia_host_puts_the_guest_on_dma_buf_wsi() {
    let (mut h, _) = s1();
    let Command::GetPhysicalDeviceProperties2(p) = h
        .call(&Command::GetPhysicalDeviceProperties2(
            GetPhysicalDeviceProperties2Args {
                physical_device: VkPhysicalDevice(PHYSICAL),
                p_properties: Some(VkPhysicalDeviceProperties2::default()),
            },
        ))
        .unwrap()
    else {
        panic!()
    };
    let p = p.p_properties.unwrap();
    assert_eq!(p.properties.vendor_id, policy::VIRTIO_PCI_VENDOR_ID);
    assert!(p.properties.driver_version >= policy::NVIDIA_DMA_BUF_WSI_DRIVER);
    assert!(!policy::keeps_software_wsi(&p));
}
