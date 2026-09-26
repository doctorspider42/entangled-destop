//! `VK_EXT_image_drm_format_modifier`, **emulated** (stage S1, "GNOME on the
//! GPU"): one modifier, `DRM_FORMAT_MOD_LINEAR`, for a small set of scanout
//! formats, realised on the host as a **canonical optimal-tiling image** in
//! exportable device-local memory.
//!
//! # Why an emulation, and why this one
//!
//! Mutter asks GBM for scanout buffers without modifiers (the virtio kernel
//! driver offers no `IN_FORMATS`), so Zink creates them optimal and, at
//! export, rebuilds each as a DRM-modifier image with the list `[LINEAR]` and
//! copies into it (`zink_resource.c:1744-1768`); asking the handle for its
//! stride needs this extension (`:1960-1964`, `zink_resource_get_param`). A
//! Windows host has no DRM modifiers at all, and on the RTX 2070 a linear
//! image cannot be a colour attachment in any scanout format, nor can an
//! optimal one live in our pages (ADR-0004, stage S1). What it can do is
//! export device-local optimal memory as `OPAQUE_WIN32` and import it on
//! another device of the same GPU — which is exactly a dma-buf between two
//! guest processes, provided both create *the same image* over it.
//!
//! # The canonical image
//!
//! For a scanout format `F` ([`SCANOUT_FORMATS`]) and an extent `W×H`, the
//! host image is always ([`CanonicalFormat::create_info`]):
//!
//! * `VK_IMAGE_TYPE_2D`, `W×H×1`, 1 mip level, 1 array layer, 1 sample,
//!   `VK_IMAGE_TILING_OPTIMAL`, `VK_SHARING_MODE_EXCLUSIVE`;
//! * flags `MUTABLE_FORMAT` with a `VkImageFormatListCreateInfo` of `[F, F']`
//!   when `F` has an sRGB/UNORM twin `F'` (Zink makes every shareable image
//!   of such a format mutable between the two, `zink_resource.c:1262-1293`),
//!   and no flags and no list otherwise;
//! * usage the **superset** [`usage_from_features`] derives from `F`'s optimal
//!   tiling features — transfer source and destination, sampled, storage,
//!   colour and input attachment, each where the feature is — narrowed to
//!   what the host accepts for this create info (storage is the one that may
//!   go);
//! * `VkExternalMemoryImageCreateInfo{OPAQUE_WIN32}`, accepted by the host as
//!   exportable and importable and not dedicated-only.
//!
//! Nothing the guest chose beyond `F`, `W`, `H` and the initial layout
//! reaches the host image: its usage must be a subset of the superset, its
//! flags of the canonical flags, its view formats of the canonical list, or
//! the create is refused. Two flags are the exception: `ALIAS` and
//! `EXTENDED_USAGE` ([`IGNORED_FLAGS`]) are accepted and not forwarded,
//! because Mesa's WSI creates every swapchain image with the first and every
//! mutable one with the second (`wsi_configure_image`, `wsi_common.c:671`,
//! `:706-707`) and neither changes what the canonical image already is. So two guest processes that create "the same"
//! LINEAR image — the exporter with `VkImageDrmFormatModifierListCreateInfoEXT`,
//! the importer with `VkImageDrmFormatModifierExplicitCreateInfoEXT` and
//! whatever usage each wants — get byte-identical host create infos, and with
//! them identical layouts, which is what an `OPAQUE_WIN32` import requires.
//!
//! # Every lie, and why no correct guest can observe one
//!
//! 1. **"LINEAR" is optimal.** `vkGetImageDrmFormatModifierPropertiesEXT`
//!    answers `DRM_FORMAT_MOD_LINEAR`, and the bytes are block-linear. The
//!    only way to observe the difference is to read the memory as linear —
//!    and it cannot be read at all: it is device-local, its blob is **not
//!    mappable** (the renderer refuses `RESOURCE_MAP_BLOB` for a handle blob),
//!    and no host-visible memory type is ever offered for these images. A
//!    guest that `mmap`s the "LINEAR" dma-buf gets a failed map, visibly, not
//!    wrong pixels. Every GPU access goes through an image of the same
//!    canonical create info, for which the driver's own layout is the truth.
//! 2. **The features of LINEAR are those of OPTIMAL.**
//!    `VkDrmFormatModifierPropertiesListEXT` (and `…List2EXT`) reports the
//!    host's *optimal* tiling features for the one modifier, less storage
//!    when the superset has no storage and less `DISJOINT` (one plane). They
//!    are the features the host image really has.
//! 3. **The plane layout is synthesized.** `vkGetImageSubresourceLayout` with
//!    `VK_IMAGE_ASPECT_MEMORY_PLANE_0_BIT_EXT` answers offset 0,
//!    `rowPitch = W × bpp` rounded up to [`PITCH_ALIGNMENT`], `size =
//!    rowPitch × H`, and `arrayPitch = depthPitch = size` — a linear layout
//!    that exists nowhere. Nobody can map the bytes, so nobody can index
//!    them with it; its consumers are metadata: GBM's stride, Mutter's
//!    `drmModeAddFB2` pitch, an importer's explicit plane layout (checked
//!    against this same rule, so an import of our own export always
//!    matches), and the host renderer that scans the blob out reads the
//!    image through Vulkan, not through the pitch. The size is **not** the
//!    real allocation's; instead the memory requirements are raised to at
//!    least `rowPitch × H` ([`ModifierLayout::requirement`]), so the blob is
//!    never smaller than the layout claims — which the guest kernel checks
//!    for a framebuffer (`drm_gem_fb_init_with_funcs`).
//! 4. **The image is mutable between `F` and `F'`, and has usage the guest
//!    did not ask for.** A superset: every view and every command the guest
//!    may issue for its own create info is valid on the canonical one.
//! 5. **Only single-plane 2D images of one level, one layer and one sample,
//!    exclusively shared.** The format query says so (`maxMipLevels`,
//!    `maxArrayLayers`, `sampleCounts`), so a correct guest never asks for
//!    more; one that does is refused.
//!
//! Anything but `DRM_FORMAT_MOD_LINEAR` is refused: the format query answers
//! `VK_ERROR_FORMAT_NOT_SUPPORTED`, and a create whose explicit modifier is
//! another, or whose modifier list does not name LINEAR at all, is fatal, as
//! a guest that ignored the query's answer is. A list that names LINEAR
//! *among* others is the implementation's to pick from, and LINEAR is picked
//! (and reported by `vkGetImageDrmFormatModifierPropertiesEXT`): Zink passes
//! its frontend's list through unfiltered, and Xwayland's glamor hands it
//! Mutter's `[LINEAR, DRM_FORMAT_MOD_INVALID]` (ADR-0004, "X11 applications").
//!
//! # The third party: the scanout device (stage S2b)
//!
//! The renderer reads a handle blob a guest compositor flips to through a
//! device of its own ([`super::scanout`]), importing the same handle and
//! creating the same image over it. That image is not re-derived: when a
//! canonical image is bound to handle memory, the executor records its
//! [`CanonicalImage`] on the blob, and the scanout device builds its host
//! create info from that record with [`CanonicalImage::create_info`] — the
//! one function every side's create info comes from.

use crate::venus::protocol::{
    VkExtent3D, VkExternalImageFormatProperties, VkFormatProperties2, VkFormatProperties2Next,
    VkFormatProperties3, VkImageCreateInfo, VkImageCreateInfoNext, VkImageFormatListCreateInfo,
    VkImageFormatProperties, VkImageFormatProperties2, VkImageFormatProperties2Next,
    VkPhysicalDeviceExternalImageFormatInfo, VkPhysicalDeviceImageFormatInfo2,
    VkPhysicalDeviceImageFormatInfo2Next, VkSubresourceLayout, VK_SUCCESS,
};

use super::host::HostVulkan;
use super::policy;

/// `VK_SHARING_MODE_EXCLUSIVE`: the only sharing mode a modifier image has.
pub const SHARING_MODE_EXCLUSIVE: i32 = 0;

/// `DRM_FORMAT_MOD_LINEAR`: the one modifier offered.
pub const DRM_FORMAT_MOD_LINEAR: u64 = 0;
/// `VK_IMAGE_TILING_DRM_FORMAT_MODIFIER_EXT`.
pub const IMAGE_TILING_DRM_FORMAT_MODIFIER: i32 = 1_000_158_000;
/// `VK_IMAGE_ASPECT_MEMORY_PLANE_0_BIT_EXT`.
pub const IMAGE_ASPECT_MEMORY_PLANE_0: u32 = 0x80;
/// The synthesized row pitch's alignment: 256 bytes, what scanout engines
/// and every GBM backend here expect of a linear buffer.
pub const PITCH_ALIGNMENT: u64 = 256;

/// The formats `DRM_FORMAT_MOD_LINEAR` is offered for, each with its
/// sRGB/UNORM twin if it has one and its bytes per pixel: every single-plane
/// colour format a Wayland client's swapchain can be made of
/// (`wsi_wl_display_add_drm_format_modifier`, `wsi_common_wayland.c:406-619`).
///
/// * `VK_FORMAT_R8G8B8A8_UNORM` / `_SRGB`, `VK_FORMAT_B8G8R8A8_UNORM` /
///   `_SRGB` (stage S1): `XRGB8888`/`ARGB8888` are `B8G8R8A8_UNORM` in
///   Vulkan (Zink's `B8G8R8X8` too), `XBGR8888`/`ABGR8888` `R8G8B8A8_UNORM`;
/// * `VK_FORMAT_A2R10G10B10_UNORM_PACK32`, `VK_FORMAT_A2B10G10R10_UNORM_PACK32`:
///   the 30-bit fourccs;
/// * (stage S5) `VK_FORMAT_R16G16B16A16_SFLOAT` and `_UNORM`: the 64-bit
///   fourccs. Under GNOME on the GPU the guest's WSI lists
///   `R16G16B16A16_SFLOAT` as its first surface format (measured), and
///   vkcube takes the first surface format it knows (`cube.c:4608-4625`);
/// * (stage S5) the 16-bit packed formats `R5G6B5`, `B5G6R5`, `A1R5G5B5`,
///   `R5G5B5A1`, `B5G5R5A1`, `R4G4B4A4`, `B4G4R4A4`.
///
/// A format a client can pick *without* LINEAR sends Mesa's WSI down its
/// prime path (`wsi_drm_image_needs_buffer_blit`, `wsi_common_drm.c:893-904`),
/// whose blit buffer must find a device-local type among the `DMA_BUF`
/// buffer's memory types (`wsi_select_device_memory_type`,
/// `wsi_common.c:1922-1959`). A `DMA_BUF` buffer here lives in our pages,
/// which are not device-local, and Mesa's `UNREACHABLE` there is undefined
/// behaviour: a release build spins forever (measured, vkcube on
/// `R16G16B16A16_SFLOAT`). So every format a swapchain can use is here, and
/// the ones the host cannot make canonical are simply not offered.
pub const SCANOUT_FORMATS: &[(i32, Option<i32>, u64)] = &[
    (37, Some(43), 4),
    (43, Some(37), 4),
    (44, Some(50), 4),
    (50, Some(44), 4),
    (58, None, 4),
    (64, None, 4),
    (97, None, 8),
    (91, None, 8),
    (4, None, 2),
    (5, None, 2),
    (8, None, 2),
    (6, None, 2),
    (7, None, 2),
    (2, None, 2),
    (3, None, 2),
];

/// Bytes per pixel of a [`SCANOUT_FORMATS`] format, and of the BGRA pair
/// a handle-blob scanout reads.
#[must_use]
pub fn bytes_per_pixel(format: i32) -> u64 {
    SCANOUT_FORMATS
        .iter()
        .find(|(f, _, _)| *f == format)
        .map_or(4, |(_, _, bpp)| *bpp)
}

/// `VK_IMAGE_CREATE_MUTABLE_FORMAT_BIT`.
const MUTABLE_FORMAT: u32 = 0x8;
/// `VK_IMAGE_CREATE_EXTENDED_USAGE_BIT`.
const EXTENDED_USAGE: u32 = 0x100;
/// `VK_IMAGE_CREATE_ALIAS_BIT`.
const ALIAS: u32 = 0x400;

/// Create flags a guest's modifier image may carry that the canonical image
/// does not, accepted and never forwarded (stage S5):
///
/// * `ALIAS`: Mesa's WSI sets it on every swapchain image
///   (`wsi_common.c:671`), and venus forwards it for every driver but ANV
///   (`vn_physical_device.c:2831-2841`). It promises that two images of
///   identical parameters bound to the same memory read it the same way.
///   Every canonical image of one format and extent has *byte-identical*
///   host create infos, all for `OPAQUE_WIN32` external memory, whose layout
///   is fixed by those parameters alone — the guarantee an import across
///   devices already relies on, and one that covers two images of one device
///   too. Forwarding it would instead make an exporter's host image differ
///   from an importer's that lacks it.
/// * `EXTENDED_USAGE`: set with `MUTABLE_FORMAT` by the WSI for a mutable
///   swapchain (`wsi_common.c:706-707`, Zink's kopper), allowing usage that
///   only a view format supports. The guest's usage is still held to the
///   superset of `F` itself, which the host accepted without the flag, so
///   the flag permits nothing the host image lacks.
pub const IGNORED_FLAGS: u32 = ALIAS | EXTENDED_USAGE;

// VkFormatFeatureFlagBits the usage superset is derived from.
const FEATURE_SAMPLED_IMAGE: u32 = 0x1;
const FEATURE_STORAGE_IMAGE: u32 = 0x2;
const FEATURE_STORAGE_IMAGE_ATOMIC: u32 = 0x4;
const FEATURE_COLOR_ATTACHMENT: u32 = 0x80;
const FEATURE_TRANSFER_SRC: u32 = 0x4000;
const FEATURE_TRANSFER_DST: u32 = 0x8000;
const FEATURE_DISJOINT: u32 = 0x40_0000;
// VkFormatFeatureFlagBits2 that go with storage.
const FEATURE2_STORAGE_READ_WITHOUT_FORMAT: u64 = 1 << 31;
const FEATURE2_STORAGE_WRITE_WITHOUT_FORMAT: u64 = 1 << 32;
const FEATURE2_HOST_IMAGE_TRANSFER: u64 = 1 << 46;

// VkImageUsageFlagBits.
const USAGE_TRANSFER_SRC: u32 = 0x1;
const USAGE_TRANSFER_DST: u32 = 0x2;
const USAGE_SAMPLED: u32 = 0x4;
const USAGE_STORAGE: u32 = 0x8;
const USAGE_COLOR_ATTACHMENT: u32 = 0x10;
const USAGE_INPUT_ATTACHMENT: u32 = 0x80;

/// `VK_IMAGE_TYPE_2D`.
const IMAGE_TYPE_2D: i32 = 1;
/// `VK_IMAGE_TILING_OPTIMAL`.
const TILING_OPTIMAL: i32 = 0;

/// The scanout format `format` and its twin, if it is one.
#[must_use]
pub fn scanout_format(format: i32) -> Option<Option<i32>> {
    SCANOUT_FORMATS
        .iter()
        .find(|(f, _, _)| *f == format)
        .map(|(_, twin, _)| *twin)
}

/// The usage an optimal image of a format with `features` could have, among
/// the ones a scanout buffer is ever given: the canonical superset before
/// the host narrows it.
#[must_use]
pub fn usage_from_features(features: u32) -> u32 {
    let mut usage = 0;
    if features & FEATURE_TRANSFER_SRC != 0 {
        usage |= USAGE_TRANSFER_SRC;
    }
    if features & FEATURE_TRANSFER_DST != 0 {
        usage |= USAGE_TRANSFER_DST;
    }
    if features & FEATURE_SAMPLED_IMAGE != 0 {
        usage |= USAGE_SAMPLED;
    }
    if features & FEATURE_STORAGE_IMAGE != 0 {
        usage |= USAGE_STORAGE;
    }
    if features & FEATURE_COLOR_ATTACHMENT != 0 {
        usage |= USAGE_COLOR_ATTACHMENT | USAGE_INPUT_ATTACHMENT;
    }
    usage
}

/// The synthesized plane of a modifier image (lie 3 of the module docs).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ModifierLayout {
    /// `extent.width`.
    pub width: u32,
    /// `extent.height`.
    pub height: u32,
    /// `width × bpp` rounded up to [`PITCH_ALIGNMENT`].
    pub row_pitch: u64,
}

impl ModifierLayout {
    /// The layout of a `width × height` image of `format`.
    #[must_use]
    pub fn new(format: i32, width: u32, height: u32) -> Self {
        let row = u64::from(width).saturating_mul(bytes_per_pixel(format));
        Self {
            width,
            height,
            row_pitch: row
                .checked_next_multiple_of(PITCH_ALIGNMENT)
                .unwrap_or(u64::MAX),
        }
    }

    /// `rowPitch × height`: the plane's size as the guest is told it.
    #[must_use]
    pub fn size(&self) -> u64 {
        self.row_pitch.saturating_mul(u64::from(self.height))
    }

    /// What `vkGetImageSubresourceLayout(MEMORY_PLANE_0)` answers.
    #[must_use]
    pub fn plane(&self) -> VkSubresourceLayout {
        let size = self.size();
        VkSubresourceLayout {
            offset: 0,
            size,
            row_pitch: self.row_pitch,
            array_pitch: size,
            depth_pitch: size,
        }
    }

    /// A host memory requirement raised so that the blob of memory bound to
    /// the image is at least as large as the synthesized plane: `size` at
    /// least [`Self::size`], still a multiple of `alignment`.
    #[must_use]
    pub fn requirement(&self, size: u64, alignment: u64) -> u64 {
        let size = size.max(self.size());
        size.checked_next_multiple_of(alignment.max(1))
            .unwrap_or(size)
    }
}

/// What the canonical image of one scanout format is on one host device:
/// see the module docs.
#[derive(Debug, Clone, PartialEq)]
pub struct CanonicalFormat {
    /// The format.
    pub format: i32,
    /// The canonical flags: `MUTABLE_FORMAT` with a twin, else 0.
    pub flags: u32,
    /// The canonical view format list: `[format, twin]`, or empty.
    pub view_formats: Vec<i32>,
    /// The usage superset the host accepted.
    pub usage: u32,
    /// What `DRM_FORMAT_MOD_LINEAR` reports as its tiling features: the
    /// host's optimal features, less storage without storage usage, less
    /// `DISJOINT`.
    pub features: u32,
    /// [`Self::features`] as `VkFormatFeatureFlags2`.
    pub features2: u64,
    /// The host's limits for the canonical create info.
    pub limits: VkImageFormatProperties,
}

/// `VK_FORMAT_B8G8R8A8_UNORM` and `VK_FORMAT_B8G8R8A8_SRGB`: the canonical
/// formats whose bytes are already in the order a virtio-gpu
/// `B8G8R8X8`/`B8G8R8A8` scanout names (GBM's `XRGB8888` is Mesa's
/// `BGRX8888_UNORM`, which Zink emulates as `B8G8R8A8_UNORM`,
/// `zink_format.c:176`). The sRGB twin stores the same bytes; only the
/// sampler's decoding differs, and a scanout samples nothing.
pub const SCANOUT_BGRA_FORMATS: [i32; 2] = [44, 50];

/// One canonical host image, **exactly**: the create info both sides of a
/// share build from it — the exporting context, every importing one, and
/// the renderer's own scanout device (stage S2b) — so all of them lay the
/// same payload out the same way. What stage S2b records on a handle blob
/// when such an image is bound to its memory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CanonicalImage {
    /// The format.
    pub format: i32,
    /// The canonical flags.
    pub flags: u32,
    /// The canonical view format list.
    pub view_formats: Vec<i32>,
    /// The usage superset the host accepted.
    pub usage: u32,
    /// `extent.width`.
    pub width: u32,
    /// `extent.height`.
    pub height: u32,
}

impl CanonicalImage {
    /// The host create info, in `initial_layout`. Its external memory
    /// (`OPAQUE_WIN32`) is the host's to add
    /// ([`super::host::ResourceMemory::Handle`]).
    #[must_use]
    pub fn create_info(&self, initial_layout: i32) -> VkImageCreateInfo<'static> {
        let p_next = if self.view_formats.is_empty() {
            Vec::new()
        } else {
            vec![VkImageCreateInfoNext::VkImageFormatListCreateInfo(
                VkImageFormatListCreateInfo {
                    view_format_count: u32::try_from(self.view_formats.len()).unwrap_or(0),
                    p_view_formats: Some(self.view_formats.clone()),
                },
            )]
        };
        VkImageCreateInfo {
            p_next,
            flags: self.flags,
            image_type: IMAGE_TYPE_2D,
            format: self.format,
            extent: VkExtent3D {
                width: self.width,
                height: self.height,
                depth: 1,
            },
            mip_levels: 1,
            array_layers: 1,
            samples: 1,
            tiling: TILING_OPTIMAL,
            usage: self.usage,
            sharing_mode: SHARING_MODE_EXCLUSIVE,
            queue_family_index_count: 0,
            p_queue_family_indices: None,
            initial_layout,
        }
    }

    /// The synthesized plane the guest was told for this image.
    #[must_use]
    pub fn layout(&self) -> ModifierLayout {
        ModifierLayout::new(self.format, self.width, self.height)
    }

    /// Whether its bytes are in BGRA order ([`SCANOUT_BGRA_FORMATS`]).
    #[must_use]
    pub fn is_bgra8(&self) -> bool {
        SCANOUT_BGRA_FORMATS.contains(&self.format)
    }
}

impl CanonicalFormat {
    /// The canonical image of this format at `width × height`.
    #[must_use]
    pub fn image(&self, width: u32, height: u32) -> CanonicalImage {
        CanonicalImage {
            format: self.format,
            flags: self.flags,
            view_formats: self.view_formats.clone(),
            usage: self.usage,
            width,
            height,
        }
    }

    /// The host create info of a `width × height` modifier image, in
    /// `initial_layout` ([`CanonicalImage::create_info`]).
    #[must_use]
    pub fn create_info(
        &self,
        width: u32,
        height: u32,
        initial_layout: i32,
    ) -> VkImageCreateInfo<'static> {
        self.image(width, height).create_info(initial_layout)
    }

    /// The limits a modifier image of this format is shown: the host's for
    /// the canonical image, at one level, one layer, one sample, depth 1.
    #[must_use]
    pub fn shown_limits(&self) -> VkImageFormatProperties {
        let mut limits = self.limits.clone();
        limits.max_extent.depth = 1;
        limits.max_mip_levels = 1;
        limits.max_array_layers = 1;
        limits.sample_counts = 1;
        limits
    }

    /// Whether a guest's `flags` and view formats fit the canonical ones:
    /// flags a subset once [`IGNORED_FLAGS`] are set aside, and — for a
    /// mutable request — a non-empty list inside the canonical one
    /// (`VUID-VkImageCreateInfo-tiling-02353` requires the list of a mutable
    /// modifier image).
    #[must_use]
    pub fn admits(&self, flags: u32, view_formats: Option<&[i32]>) -> bool {
        if flags & !(self.flags | IGNORED_FLAGS) != 0 {
            return false;
        }
        let list = view_formats.unwrap_or_default();
        if flags & MUTABLE_FORMAT != 0 && list.is_empty() {
            return false;
        }
        list.iter()
            .all(|f| *f == self.format || self.view_formats.contains(f))
    }
}

/// The host's question for a canonical create info: `OPAQUE_WIN32`
/// external, the canonical list, optimal, 2D.
fn host_query(
    format: i32,
    flags: u32,
    list: &[i32],
    usage: u32,
) -> VkPhysicalDeviceImageFormatInfo2 {
    let mut p_next = vec![
        VkPhysicalDeviceImageFormatInfo2Next::VkPhysicalDeviceExternalImageFormatInfo(
            VkPhysicalDeviceExternalImageFormatInfo {
                handle_type: policy::MEMORY_HANDLE_OPAQUE_WIN32 as i32,
            },
        ),
    ];
    if !list.is_empty() {
        p_next.push(
            VkPhysicalDeviceImageFormatInfo2Next::VkImageFormatListCreateInfo(
                VkImageFormatListCreateInfo {
                    view_format_count: u32::try_from(list.len()).unwrap_or(0),
                    p_view_formats: Some(list.to_vec()),
                },
            ),
        );
    }
    VkPhysicalDeviceImageFormatInfo2 {
        p_next,
        format,
        type_: IMAGE_TYPE_2D,
        tiling: TILING_OPTIMAL,
        usage,
        flags,
    }
}

/// Whether the host accepts a canonical create info of `usage`, and its
/// limits if so: `VK_SUCCESS`, `EXPORTABLE | IMPORTABLE` for `OPAQUE_WIN32`,
/// not `DEDICATED_ONLY`, and at least one sample.
fn host_accepts<H: HostVulkan>(
    host: &H,
    instance: &H::Instance,
    physical: H::PhysicalDevice,
    query: &VkPhysicalDeviceImageFormatInfo2,
) -> Option<VkImageFormatProperties> {
    let mut out = VkImageFormatProperties2 {
        p_next: vec![
            VkImageFormatProperties2Next::VkExternalImageFormatProperties(
                VkExternalImageFormatProperties::default(),
            ),
        ],
        image_format_properties: VkImageFormatProperties::default(),
    };
    if host.image_format_properties(instance, physical, query, &mut out) != VK_SUCCESS {
        return None;
    }
    let external = out.p_next.iter().find_map(|l| match l {
        VkImageFormatProperties2Next::VkExternalImageFormatProperties(p) => {
            Some(p.external_memory_properties.clone())
        }
        _ => None,
    })?;
    let want = policy::MEMORY_FEATURE_EXPORTABLE | policy::MEMORY_FEATURE_IMPORTABLE;
    let ok = external.external_memory_features & want == want
        && external.external_memory_features & policy::MEMORY_FEATURE_DEDICATED_ONLY == 0
        && external.compatible_handle_types & policy::MEMORY_HANDLE_OPAQUE_WIN32 != 0
        && out.image_format_properties.sample_counts & 1 != 0
        && out.image_format_properties.max_extent.width > 0
        && out.image_format_properties.max_extent.height > 0;
    ok.then_some(out.image_format_properties)
}

/// The canonical image of `format` on host device `physical`, or `None` if
/// `format` is not a scanout format or the host cannot make an exportable
/// canonical image of it (see the module docs).
pub fn canonical_format<H: HostVulkan>(
    host: &H,
    instance: &H::Instance,
    physical: H::PhysicalDevice,
    format: i32,
) -> Option<CanonicalFormat> {
    let twin = scanout_format(format)?;
    let (flags, view_formats) = match twin {
        Some(twin) => (MUTABLE_FORMAT, vec![format, twin]),
        None => (0, Vec::new()),
    };
    let mut props = VkFormatProperties2 {
        p_next: vec![VkFormatProperties2Next::VkFormatProperties3(
            VkFormatProperties3::default(),
        )],
        ..Default::default()
    };
    host.format_properties(instance, physical, format, &mut props);
    let optimal = props.format_properties.optimal_tiling_features;
    let optimal2 = props
        .p_next
        .iter()
        .find_map(|l| match l {
            VkFormatProperties2Next::VkFormatProperties3(p) => Some(p.optimal_tiling_features),
            _ => None,
        })
        .filter(|f| *f != 0)
        .unwrap_or(u64::from(optimal));
    let full = usage_from_features(optimal);
    if full & (USAGE_COLOR_ATTACHMENT | USAGE_SAMPLED) == 0 {
        // Not a format anything scans out of or renders to here.
        return None;
    }
    // The full superset, and — if the host will not export that — the same
    // without storage (the one usage sRGB formats and mutable lists refuse).
    let (usage, limits) = [full, full & !USAGE_STORAGE]
        .into_iter()
        .find_map(|usage| {
            let query = host_query(format, flags, &view_formats, usage);
            host_accepts(host, instance, physical, &query).map(|limits| (usage, limits))
        })?;
    let mut features = optimal & !FEATURE_DISJOINT;
    let mut features2 = optimal2 & !u64::from(FEATURE_DISJOINT) & !FEATURE2_HOST_IMAGE_TRANSFER;
    if usage & USAGE_STORAGE == 0 {
        features &= !(FEATURE_STORAGE_IMAGE | FEATURE_STORAGE_IMAGE_ATOMIC);
        features2 &= !(u64::from(FEATURE_STORAGE_IMAGE | FEATURE_STORAGE_IMAGE_ATOMIC)
            | FEATURE2_STORAGE_READ_WITHOUT_FORMAT
            | FEATURE2_STORAGE_WRITE_WITHOUT_FORMAT);
    }
    Some(CanonicalFormat {
        format,
        flags,
        view_formats,
        usage,
        features,
        features2,
        limits,
    })
}

/// `vkGetPhysicalDeviceImageFormatProperties2` for
/// `VK_IMAGE_TILING_DRM_FORMAT_MODIFIER_EXT`, on a device shown the emulated
/// extension, `info` already checked (`context::check_image_format_info`):
/// `VK_SUCCESS` and the canonical image's limits ([`CanonicalFormat::shown_limits`])
/// exactly when the modifier is `DRM_FORMAT_MOD_LINEAR`, the sharing mode
/// exclusive, the image 2D of a scanout format the host can make canonical,
/// and its usage, flags and view formats inside the canonical ones; a
/// chained `VkExternalImageFormatProperties` is the `DMA_BUF` answer
/// (exportable and importable: the image always lives in exportable
/// memory). `VK_ERROR_FORMAT_NOT_SUPPORTED` for everything else, and for any
/// external handle type but `DMA_BUF`.
pub fn image_format_properties<H: HostVulkan>(
    host: &H,
    instance: &H::Instance,
    physical: H::PhysicalDevice,
    info: &VkPhysicalDeviceImageFormatInfo2,
    out: Option<&mut VkImageFormatProperties2>,
) -> i32 {
    use crate::venus::protocol::VK_ERROR_FORMAT_NOT_SUPPORTED as NOT_SUPPORTED;
    let mut modifier = None;
    let mut list: Option<&[i32]> = None;
    for link in &info.p_next {
        match link {
            VkPhysicalDeviceImageFormatInfo2Next::VkPhysicalDeviceImageDrmFormatModifierInfoEXT(
                m,
            ) => {
                modifier = Some(m);
            }
            VkPhysicalDeviceImageFormatInfo2Next::VkImageFormatListCreateInfo(l) => {
                list = Some(l.p_view_formats.as_deref().unwrap_or_default());
            }
            VkPhysicalDeviceImageFormatInfo2Next::VkPhysicalDeviceExternalImageFormatInfo(e) => {
                if e.handle_type != 0 && e.handle_type != policy::MEMORY_HANDLE_DMA_BUF as i32 {
                    return NOT_SUPPORTED;
                }
            }
            // A stencil usage of a colour format, or anything newer.
            _ => return NOT_SUPPORTED,
        }
    }
    let Some(modifier) = modifier else {
        return NOT_SUPPORTED;
    };
    if modifier.drm_format_modifier != DRM_FORMAT_MOD_LINEAR
        || modifier.sharing_mode != SHARING_MODE_EXCLUSIVE
        || info.type_ != IMAGE_TYPE_2D
    {
        return NOT_SUPPORTED;
    }
    let Some(canonical) = canonical_format(host, instance, physical, info.format) else {
        return NOT_SUPPORTED;
    };
    if !canonical.admits(info.flags, list) || info.usage & !canonical.usage != 0 {
        return NOT_SUPPORTED;
    }
    if let Some(out) = out {
        out.image_format_properties = canonical.shown_limits();
        for link in &mut out.p_next {
            match link {
                VkImageFormatProperties2Next::VkExternalImageFormatProperties(p) => {
                    p.external_memory_properties = policy::external_memory_properties(true);
                }
                VkImageFormatProperties2Next::VkSamplerYcbcrConversionImageFormatProperties(p) => {
                    p.combined_image_sampler_descriptor_count = 1;
                }
                // Not admitted by the executor, so never here.
                _ => {}
            }
        }
    }
    VK_SUCCESS
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_synthesized_plane_is_a_256_aligned_linear_layout() {
        let l = ModifierLayout::new(44, 1920, 1080);
        assert_eq!(l.row_pitch, 7680);
        assert_eq!(l.size(), 7680 * 1080);
        let p = l.plane();
        assert_eq!((p.offset, p.row_pitch, p.size), (0, 7680, 7680 * 1080));
        let odd = ModifierLayout::new(44, 1, 3);
        assert_eq!(odd.row_pitch, 256);
        assert_eq!(odd.size(), 768);
        // The requirement is raised to the plane, and stays aligned.
        assert_eq!(odd.requirement(512, 1024), 1024);
        assert_eq!(l.requirement(1 << 30, 0x1_0000), 1 << 30);
        assert_eq!(l.requirement(4096, 0x1_0000) % 0x1_0000, 0);
        assert!(l.requirement(4096, 0x1_0000) >= l.size());
        // A width no row can hold saturates rather than wraps.
        assert_eq!(
            ModifierLayout::new(44, u32::MAX, 2).row_pitch % PITCH_ALIGNMENT,
            0
        );
    }

    #[test]
    fn usage_follows_the_optimal_features() {
        assert_eq!(usage_from_features(0), 0);
        assert_eq!(
            usage_from_features(0x1_d401 | FEATURE_COLOR_ATTACHMENT | FEATURE_STORAGE_IMAGE),
            USAGE_TRANSFER_SRC
                | USAGE_TRANSFER_DST
                | USAGE_SAMPLED
                | USAGE_STORAGE
                | USAGE_COLOR_ATTACHMENT
                | USAGE_INPUT_ATTACHMENT
        );
        assert_eq!(scanout_format(44), Some(Some(50)));
        assert_eq!(scanout_format(58), Some(None));
        assert_eq!(scanout_format(100), None);
        // Stage S5: the formats a swapchain can pick, at their own sizes.
        assert_eq!(scanout_format(97), Some(None));
        assert_eq!(bytes_per_pixel(97), 8);
        assert_eq!(bytes_per_pixel(4), 2);
        assert_eq!(bytes_per_pixel(44), 4);
        assert_eq!(ModifierLayout::new(97, 500, 500).row_pitch, 4096);
        assert_eq!(ModifierLayout::new(4, 500, 500).row_pitch, 1024);
        assert_eq!(ModifierLayout::new(44, 500, 500).row_pitch, 2048);
        for (format, twin, bpp) in SCANOUT_FORMATS {
            assert!(policy::is_core_format(*format), "{format}");
            assert!(matches!(bpp, 2 | 4 | 8), "{format}");
            if let Some(twin) = twin {
                assert_eq!(scanout_format(*twin), Some(Some(*format)));
            }
        }
    }
}
