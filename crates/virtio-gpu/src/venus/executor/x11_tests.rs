//! X11 applications on the GPU desktop (ADR-0004, "X11 applications"):
//! Xwayland's glamor on Zink, and the X clients whose buffers it imports,
//! against the fake host — every DRM-modifier create info they send, the
//! ones that must be answered LINEAR, the ones still refused, and the
//! property that holds them together: **whatever the format queries
//! advertise, a create accepts**.
//!
//! The shapes are the captured ones (the fresh-install acceptance of v0.2.49,
//! re-run with the create infos logged). Glamor's first pixmap is a 1×1
//! `B8G8R8A8_UNORM` image, `MUTABLE` between it and its sRGB twin, usage
//! `0x97`, with the list `[LINEAR, DRM_FORMAT_MOD_INVALID]`: Mutter's
//! dma-buf table ends every format with `INVALID`
//! (`meta-wayland-dma-buf.c:1759-1765`), Xwayland 24.1.10 passes a format's
//! modifiers through (`xwayland-dmabuf.c:277-290`,
//! `xwayland-glamor-gbm.c:357-369`), and Zink chains its frontend's whole
//! list (`zink_resource.c:1364-1369`) after asking about LINEAR alone
//! (`find_good_mod`, `set_image_usage`, `:554-610`). That list was fatal to
//! Xwayland's context, and Xwayland crashed.

use crate::renderer::Renderer3d;
use crate::venus::protocol::*;

use super::fake::{self, FakeVulkan};
use super::harness::*;
use super::host::ResourceMemory;
use super::modifier::{self, DRM_FORMAT_MOD_LINEAR, IMAGE_ASPECT_MEMORY_PLANE_0, SCANOUT_FORMATS};
use super::policy;
use super::s1_tests::*;

/// `DRM_FORMAT_MOD_INVALID`, which Mutter lists for every format.
const MOD_INVALID: u64 = 0x00ff_ffff_ffff_ffff;
/// `I915_FORMAT_MOD_X_TILED` and an NVIDIA block-linear modifier: modifiers
/// a compositor on other hardware could list, never offered here.
const MOD_X_TILED: u64 = 0x0100_0000_0000_0001;
const MOD_NV_BLOCK: u64 = 0x0300_0000_0060_1014;
/// `VK_IMAGE_USAGE_INPUT_ATTACHMENT_BIT`.
const USAGE_INPUT: u32 = 0x80;
/// Glamor's usage on Zink: transfers, sampled, colour and input attachment.
const GLAMOR_USAGE: u32 = USAGE_TRANSFER | USAGE_SAMPLED | USAGE_COLOR | USAGE_INPUT;
/// `VK_IMAGE_CREATE_ALIAS_BIT`, `VK_IMAGE_CREATE_EXTENDED_USAGE_BIT`,
/// `VK_IMAGE_CREATE_CUBE_COMPATIBLE_BIT`.
const ALIAS: u32 = 0x400;
const EXTENDED_USAGE: u32 = 0x100;
const CUBE_COMPATIBLE: u32 = 0x10;

/// A DRM-modifier create info in the chain order the capture shows for
/// glamor: the list, the external memory, the view formats.
fn glamor_image(
    format: i32,
    views: Option<Vec<i32>>,
    flags: u32,
    usage: u32,
    extent: (u32, u32),
    mods: Vec<u64>,
) -> VkImageCreateInfo<'static> {
    let mut info = modifier_info(format, usage, flags, views, Named::List(mods));
    // `modifier_info` puts the modifier last; glamor's comes first.
    info.p_next.rotate_right(1);
    info.extent.width = extent.0;
    info.extent.height = extent.1;
    info
}

/// Glamor's first pixmap, exactly as captured.
fn glamor_probe() -> VkImageCreateInfo<'static> {
    glamor_image(
        BGRA,
        Some(vec![BGRA, BGRA_SRGB]),
        MUTABLE,
        GLAMOR_USAGE,
        (1, 1),
        vec![DRM_FORMAT_MOD_LINEAR, MOD_INVALID],
    )
}

/// The query Zink's `check_ici` sends before it (`zink_resource.c:335-395`).
fn glamor_query() -> Command<'static> {
    image_query(
        BGRA,
        GLAMOR_USAGE,
        MUTABLE,
        DRM_FORMAT_MOD_LINEAR,
        vec![
            VkPhysicalDeviceImageFormatInfo2Next::VkImageFormatListCreateInfo(
                VkImageFormatListCreateInfo {
                    view_format_count: 2,
                    p_view_formats: Some(vec![BGRA, BGRA_SRGB]),
                },
            ),
        ],
    )
}

fn modifier_of(h: &mut Harness<FakeVulkan>, image: u64) -> (i32, u64) {
    let Command::GetImageDrmFormatModifierPropertiesEXT(p) = h
        .call(&Command::GetImageDrmFormatModifierPropertiesEXT(
            GetImageDrmFormatModifierPropertiesEXTArgs {
                device: VkDevice(DEVICE),
                image: VkImage(image),
                p_properties: Some(VkImageDrmFormatModifierPropertiesEXT {
                    drm_format_modifier: 0xdead,
                }),
                ret: 0,
            },
        ))
        .expect("the modifier query")
    else {
        panic!()
    };
    (p.ret, p.p_properties.unwrap().drm_format_modifier)
}

fn plane_of(h: &mut Harness<FakeVulkan>, image: u64) -> VkSubresourceLayout {
    let Command::GetImageSubresourceLayout(l) = h
        .call(&Command::GetImageSubresourceLayout(
            GetImageSubresourceLayoutArgs {
                device: VkDevice(DEVICE),
                image: VkImage(image),
                p_subresource: Some(VkImageSubresource {
                    aspect_mask: IMAGE_ASPECT_MEMORY_PLANE_0,
                    mip_level: 0,
                    array_layer: 0,
                }),
                p_layout: Some(Default::default()),
            },
        ))
        .expect("the plane")
    else {
        panic!()
    };
    l.p_layout.unwrap()
}

/// `vkGetDeviceImageMemoryRequirements` of `info`: what an image of it
/// would need, without one.
fn device_requirements(
    h: &mut Harness<FakeVulkan>,
    info: VkImageCreateInfo<'static>,
) -> VkMemoryRequirements {
    let Command::GetDeviceImageMemoryRequirements(r) = h
        .call(&Command::GetDeviceImageMemoryRequirements(
            GetDeviceImageMemoryRequirementsArgs {
                device: VkDevice(DEVICE),
                p_info: Some(VkDeviceImageMemoryRequirements {
                    p_create_info: Some(info),
                    plane_aspect: 0,
                }),
                p_memory_requirements: Some(VkMemoryRequirements2::default()),
            },
        ))
        .expect("the requirements")
    else {
        panic!()
    };
    r.p_memory_requirements.unwrap().memory_requirements
}

// ------------------------------------------- glamor's own pixmaps

#[test]
fn xwaylands_glamor_probe_is_answered_linear_from_mutters_list() {
    let (mut h, host) = s1();
    // What Zink learnt first: LINEAR, and only LINEAR, for XRGB8888.
    let (count, entries) = modifiers_of(&mut h, BGRA, Some(128));
    assert_eq!(count, 1);
    assert_eq!(entries[0].0, DRM_FORMAT_MOD_LINEAR);
    // Its question about LINEAR, as captured: supported.
    assert_eq!(image_answer(&mut h, &glamor_query()).0, VK_SUCCESS);
    // The create with Mutter's whole list: LINEAR, not a fatal context.
    assert_eq!(create(&mut h, EXPORTER, glamor_probe()), VK_SUCCESS);
    assert!(!h.fatal(), "the list [LINEAR, INVALID] was fatal before");
    assert_eq!(
        host.image_infos().pop().unwrap(),
        fake::ImageInfo {
            format: BGRA,
            extent: (1, 1),
            flags: MUTABLE,
            usage: SUPERSET,
            tiling: 0,
            view_formats: vec![BGRA, BGRA_SRGB],
            memory: ResourceMemory::Handle,
        },
        "the canonical optimal image, as for the list [LINEAR]"
    );
    // The guest is only ever shown the modifier it was offered.
    assert_eq!(
        modifier_of(&mut h, EXPORTER),
        (VK_SUCCESS, DRM_FORMAT_MOD_LINEAR)
    );
    let plane = plane_of(&mut h, EXPORTER);
    assert_eq!((plane.offset, plane.row_pitch, plane.size), (0, 256, 256));
    assert!(!h.fatal());
}

#[test]
fn a_list_naming_linear_among_others_is_the_same_image_as_linear_alone() {
    let reference = {
        let (mut h, host) = s1();
        assert_eq!(create(&mut h, EXPORTER, exporter_image()), VK_SUCCESS);
        let req = requirements(&mut h, EXPORTER);
        (host.image_infos().pop().unwrap(), req)
    };
    for mods in [
        vec![DRM_FORMAT_MOD_LINEAR, MOD_INVALID],
        vec![MOD_INVALID, DRM_FORMAT_MOD_LINEAR],
        vec![MOD_X_TILED, DRM_FORMAT_MOD_LINEAR, MOD_NV_BLOCK],
        vec![DRM_FORMAT_MOD_LINEAR, DRM_FORMAT_MOD_LINEAR],
    ] {
        let (mut h, host) = s1();
        let info = modifier_info(
            BGRA,
            USAGE_COLOR | USAGE_SAMPLED | USAGE_TRANSFER,
            MUTABLE,
            Some(vec![BGRA, BGRA_SRGB]),
            Named::List(mods.clone()),
        );
        // What an image of it would need, asked without one.
        let asked = device_requirements(&mut h, info.clone());
        assert_eq!(create(&mut h, EXPORTER, info), VK_SUCCESS, "{mods:x?}");
        assert_eq!(
            host.image_infos().pop().unwrap(),
            reference.0,
            "{mods:x?}: byte-identical host create info"
        );
        let req = requirements(&mut h, EXPORTER);
        assert_eq!(req, reference.1, "{mods:x?}");
        assert_eq!(asked.size, req.size, "{mods:x?}");
        assert_eq!(
            modifier_of(&mut h, EXPORTER),
            (VK_SUCCESS, DRM_FORMAT_MOD_LINEAR),
            "{mods:x?}"
        );
        assert_eq!(plane_of(&mut h, EXPORTER).row_pitch, PITCH, "{mods:x?}");
        assert!(!h.fatal(), "{mods:x?}");
    }
}

#[test]
fn a_modifier_the_device_never_offered_is_still_refused_alone() {
    let list = |mods: Vec<u64>| {
        create_image(
            DEVICE,
            EXPORTER,
            glamor_image(
                BGRA,
                Some(vec![BGRA, BGRA_SRGB]),
                MUTABLE,
                GLAMOR_USAGE,
                (64, 64),
                mods,
            ),
        )
    };
    for (command, what) in [
        (list(vec![MOD_INVALID]), "a list of INVALID alone"),
        (
            list(vec![MOD_X_TILED, MOD_NV_BLOCK]),
            "a list of modifiers never offered",
        ),
        (list(Vec::new()), "an empty list"),
        (
            create_image(
                DEVICE,
                EXPORTER,
                modifier_info(
                    BGRA,
                    GLAMOR_USAGE,
                    0,
                    None,
                    Named::Explicit(
                        MOD_X_TILED,
                        VkSubresourceLayout {
                            row_pitch: PITCH,
                            ..Default::default()
                        },
                    ),
                ),
            ),
            "an explicit modifier never offered",
        ),
    ] {
        refused(command, what);
    }
    // A list naming LINEAR is still judged as the canonical image is: a
    // usage outside the superset, or an optimal image with a list, is not
    // made acceptable by LINEAR's company.
    refused(
        create_image(
            DEVICE,
            EXPORTER,
            glamor_image(
                BGRA,
                None,
                0,
                USAGE_SAMPLED | 0x20,
                (64, 64),
                vec![DRM_FORMAT_MOD_LINEAR, MOD_INVALID],
            ),
        ),
        "depth-stencil usage, LINEAR among others",
    );
    let mut optimal = glamor_probe();
    optimal.tiling = 0;
    refused(
        create_image(DEVICE, EXPORTER, optimal),
        "a modifier list on an optimal image",
    );
}

// ------------------------------------------- a client's buffer, imported

const CLIENT_IMAGE: u64 = 0xb00;
const CLIENT_MEM: u64 = 0xb01;
const CLIENT_RES: u32 = 0xb0;
const XWAYLAND_IMAGE: u64 = 0xb10;
const XWAYLAND_MEM: u64 = 0xb11;

#[test]
fn xwayland_imports_an_x_clients_buffer_as_the_same_canonical_image() {
    // The captured window: 1074×818, whose LINEAR pitch is 1074 × 4 = 4296
    // rounded to 4352.
    const EXTENT: (u32, u32) = (1074, 818);
    const CLIENT_PITCH: u64 = 4352;
    let host = host_with(vec![gpu("NVIDIA GeForce RTX 2070", 7)]);
    let mut h = Harness::new(std::sync::Arc::clone(&host));
    device_on(&mut h, &[PHYSICAL], PHYSICAL, S1_EXTENSIONS);
    // The X client (glxgears on Zink): its back buffer from the DRI3
    // modifiers Xwayland reports, which are Mutter's.
    let exported = glamor_image(
        BGRA,
        Some(vec![BGRA, BGRA_SRGB]),
        MUTABLE,
        GLAMOR_USAGE,
        EXTENT,
        vec![DRM_FORMAT_MOD_LINEAR, MOD_INVALID],
    );
    assert_eq!(create(&mut h, CLIENT_IMAGE, exported), VK_SUCCESS);
    assert_eq!(plane_of(&mut h, CLIENT_IMAGE).row_pitch, CLIENT_PITCH);
    let req = requirements(&mut h, CLIENT_IMAGE);
    h.send(&allocate(
        DEVICE,
        CLIENT_MEM,
        req.size,
        DEVICE_LOCAL_TYPE,
        vec![dma_buf_export(), dedicated_to(CLIENT_IMAGE)],
    ))
    .expect("the export allocation");
    h.send(&bind_image(DEVICE, CLIENT_IMAGE, CLIENT_MEM, 0))
        .expect("bound");
    let blob = req.size.next_multiple_of(4096);
    h.memory_blob(h.ctx, CLIENT_RES, CLIENT_MEM, blob)
        .expect("a handle blob");
    assert!(!h.fatal());

    // Xwayland: `PixmapFromBuffers` with LINEAR at the client's pitch,
    // imported by glamor through EGL into Zink — explicit LINEAR, mutable
    // with the sRGB list, as captured.
    second_context(&mut h, &[PHYSICAL], PHYSICAL);
    h.renderer.ctx_attach_blob(2, CLIENT_RES, true);
    assert_eq!(
        resource_properties(&mut h, CLIENT_RES),
        (VK_SUCCESS, 1 << DEVICE_LOCAL_TYPE, blob)
    );
    let explicit = |pitch: u64| {
        let mut info = modifier_info(
            BGRA,
            GLAMOR_USAGE,
            MUTABLE,
            Some(vec![BGRA, BGRA_SRGB]),
            Named::Explicit(
                DRM_FORMAT_MOD_LINEAR,
                VkSubresourceLayout {
                    row_pitch: pitch,
                    ..Default::default()
                },
            ),
        );
        info.p_next.rotate_right(1);
        info.extent.width = EXTENT.0;
        info.extent.height = EXTENT.1;
        info
    };
    // A pitch that is not the one the client was told: answered, not fatal.
    assert_eq!(
        create(&mut h, XWAYLAND_IMAGE, explicit(1074 * 4)),
        VK_ERROR_INVALID_DRM_FORMAT_MODIFIER_PLANE_LAYOUT_EXT
    );
    assert!(!h.fatal());
    assert_eq!(
        create(&mut h, XWAYLAND_IMAGE, explicit(CLIENT_PITCH)),
        VK_SUCCESS
    );
    let infos = host.image_infos();
    assert_eq!(
        infos[infos.len() - 2],
        infos[infos.len() - 1],
        "the client's host image and Xwayland's are byte-identical"
    );
    let req = requirements(&mut h, XWAYLAND_IMAGE);
    h.send(&allocate(
        DEVICE,
        XWAYLAND_MEM,
        req.size,
        DEVICE_LOCAL_TYPE,
        vec![import_of(CLIENT_RES), dedicated_to(XWAYLAND_IMAGE)],
    ))
    .expect("the import");
    h.send(&bind_image(DEVICE, XWAYLAND_IMAGE, XWAYLAND_MEM, 0))
        .expect("bound");
    assert_eq!(host.handle_imports().len(), 1);
    assert_eq!(
        modifier_of(&mut h, XWAYLAND_IMAGE),
        (VK_SUCCESS, DRM_FORMAT_MOD_LINEAR)
    );
    assert!(!h.fatal(), "imported and bound");
}

// ------------------------------------------- advertised ⇒ accepted

/// The usage Zink derives from a modifier's tiling features for a shared
/// render target it may sample and blit (`get_image_usage_for_feats`,
/// `zink_resource.c:409-477`, without `PIPE_BIND_LINEAR`): each transfer,
/// sampled, colour and input attachment where the feature is, and storage
/// only with `PIPE_BIND_SHADER_IMAGE`.
fn zink_usage(features: u64, shader_image: bool) -> u32 {
    let features = features as u32;
    let mut usage = 0;
    if features & 0x4000 != 0 {
        usage |= 0x1;
    }
    if features & 0x8000 != 0 {
        usage |= 0x2;
    }
    if features & 0x1 != 0 {
        usage |= USAGE_SAMPLED;
    }
    if shader_image && features & 0x2 != 0 {
        usage |= USAGE_STORAGE;
    }
    if features & 0x80 != 0 {
        usage |= USAGE_COLOR | USAGE_INPUT;
    }
    usage
}

/// The flags and view formats a guest may ask a modifier image of `format`
/// for: the canonical ones, the ignored ones, and several no canonical image
/// has.
fn flag_variants(format: i32) -> Vec<(u32, Option<Vec<i32>>)> {
    let twin = modifier::scanout_format(format).flatten();
    let pair = twin.map_or(vec![format], |t| vec![format, t]);
    vec![
        (0, None),
        (MUTABLE, Some(pair.clone())),
        (MUTABLE, Some(vec![format])),
        (MUTABLE, None),
        (ALIAS, None),
        (MUTABLE | EXTENDED_USAGE | ALIAS, Some(pair)),
        (CUBE_COMPATIBLE, None),
        (MUTABLE, Some(vec![format, 100])),
    ]
}

fn view_list(views: &Option<Vec<i32>>) -> Vec<VkPhysicalDeviceImageFormatInfo2Next> {
    views
        .iter()
        .map(|v| {
            VkPhysicalDeviceImageFormatInfo2Next::VkImageFormatListCreateInfo(
                VkImageFormatListCreateInfo {
                    view_format_count: v.len() as u32,
                    p_view_formats: Some(v.clone()),
                },
            )
        })
        .collect()
}

/// The four ways a guest names LINEAR for a `width`-wide image of `format`:
/// the list `[LINEAR]` (Zink's export rebuild, the WSI), Mutter's list
/// (glamor), foreign modifiers with LINEAR among them, and explicitly at the
/// synthesized pitch (every importer).
fn namings(format: i32, width: u32) -> [Named; 4] {
    let pitch = modifier::ModifierLayout::new(format, width, 1).row_pitch;
    [
        Named::List(vec![DRM_FORMAT_MOD_LINEAR]),
        Named::List(vec![DRM_FORMAT_MOD_LINEAR, MOD_INVALID]),
        Named::List(vec![MOD_X_TILED, DRM_FORMAT_MOD_LINEAR, MOD_NV_BLOCK]),
        Named::Explicit(
            DRM_FORMAT_MOD_LINEAR,
            VkSubresourceLayout {
                row_pitch: pitch,
                ..Default::default()
            },
        ),
    ]
}

/// Create a `1074×818` modifier image of the query's parameters named
/// `named` as `id`: it must be created, and reported LINEAR. Answers its
/// host create info; the image is destroyed again.
#[allow(clippy::too_many_arguments)]
fn accepted_as(
    h: &mut Harness<FakeVulkan>,
    host: &FakeVulkan,
    id: u64,
    format: i32,
    usage: u32,
    flags: u32,
    views: &Option<Vec<i32>>,
    named: Named,
) -> fake::ImageInfo {
    let what = format!("format {format} usage {usage:#x} flags {flags:#x} views {views:?}");
    let mut info = modifier_info(format, usage, flags, views.clone(), named);
    info.extent.width = 1074;
    info.extent.height = 818;
    let Ok(Command::CreateImage(c)) = h.call(&create_image(DEVICE, id, info)) else {
        panic!("{what}: advertised, and the create was fatal");
    };
    assert_eq!(c.ret, VK_SUCCESS, "{what}");
    assert_eq!(
        modifier_of(h, id),
        (VK_SUCCESS, DRM_FORMAT_MOD_LINEAR),
        "{what}"
    );
    let created = host.image_infos().pop().unwrap();
    assert_eq!(created.tiling, 0, "{what}: optimal on the host");
    assert_eq!(created.memory, ResourceMemory::Handle, "{what}");
    h.send(&destroy_image(DEVICE, id)).expect("destroyed");
    created
}

/// The property the bug broke: over every scanout format and several
/// others, every core usage bit alone and the combinations clients send, and
/// every kind of flag and view list, a `VK_SUCCESS` from the LINEAR format
/// query means a create of that image **is** accepted — named each of the
/// four ways ([`namings`], rotated through the matrix, all four for the
/// usage Zink derives) — as one canonical host image, reported LINEAR. A
/// format is advertised LINEAR exactly when some such query succeeds, and
/// the usage Zink derives from the advertised features is always accepted.
///
/// Acceptance is monotone in usage (a subset of the canonical superset), so
/// single bits and the maximal combinations sample it; each harness call is
/// a real ring round trip, which is what keeps the matrix to this size.
#[test]
fn everything_the_format_queries_advertise_a_create_accepts() {
    let formats: Vec<i32> = SCANOUT_FORMATS
        .iter()
        .map(|(f, _, _)| *f)
        .chain([100, 126, 23, 109])
        .collect();
    let usages: Vec<u32> = (0..8)
        .map(|bit| 1u32 << bit)
        .chain([
            SUPERSET,
            SUPERSET & !USAGE_STORAGE,
            GLAMOR_USAGE,
            USAGE_TRANSFER | USAGE_SAMPLED,
            policy::IMAGE_USAGE_CORE,
        ])
        .collect();
    let (mut h, host) = s1();
    let mut next_id = 0x1_0000u64;
    let mut accepted_total = 0usize;
    for format in formats {
        let (count, entries) = modifiers_of(&mut h, format, Some(8));
        assert!(count <= 1, "format {format}: only LINEAR is ever listed");
        let mut accepted = 0usize;
        for (flags, views) in flag_variants(format) {
            for &usage in &usages {
                let query = image_query(
                    format,
                    usage,
                    flags,
                    DRM_FORMAT_MOD_LINEAR,
                    view_list(&views),
                );
                let (ret, _) = image_answer(&mut h, &query);
                if ret != VK_SUCCESS {
                    assert_eq!(ret, VK_ERROR_FORMAT_NOT_SUPPORTED);
                    continue;
                }
                let named = namings(format, 1074)
                    .into_iter()
                    .nth(accepted % 4)
                    .expect("four");
                accepted += 1;
                accepted_as(&mut h, &host, next_id, format, usage, flags, &views, named);
                next_id += 1;
            }
        }
        assert_eq!(
            count == 1,
            accepted > 0,
            "format {format}: listed LINEAR exactly when an image of it is supported"
        );
        if let Some(&(_, _, features)) = entries.first() {
            // What Zink asks for with the features it was shown, and the
            // image it then creates, named each way: one host image.
            let pair = modifier::scanout_format(format)
                .flatten()
                .map(|t| vec![format, t]);
            let flags = if pair.is_some() { MUTABLE } else { 0 };
            for shader_image in [false, true] {
                let usage = zink_usage(features, shader_image);
                let query = image_query(
                    format,
                    usage,
                    flags,
                    DRM_FORMAT_MOD_LINEAR,
                    view_list(&pair),
                );
                assert_eq!(
                    image_answer(&mut h, &query).0,
                    VK_SUCCESS,
                    "format {format}: Zink's usage {usage:#x} from the advertised features                      {features:#x}"
                );
                let infos: Vec<_> = namings(format, 1074)
                    .into_iter()
                    .map(|named| {
                        next_id += 1;
                        accepted_as(&mut h, &host, next_id, format, usage, flags, &pair, named)
                    })
                    .collect();
                assert!(
                    infos.windows(2).all(|w| w[0] == w[1]),
                    "format {format} usage {usage:#x}: one canonical image {infos:?}"
                );
            }
        }
        accepted_total += accepted;
    }
    assert!(!h.fatal());
    assert_eq!(host.live("image"), 0, "every image went again");
    // Not vacuous: the 15 scanout formats, each with several flag kinds.
    assert!(accepted_total > 15 * 3 * 4, "{accepted_total}");
}
