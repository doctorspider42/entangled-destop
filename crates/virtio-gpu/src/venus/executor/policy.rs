//! What the guest is told about the host, and what it may ask the host for:
//! the pure functions behind every policy of stage 5a.3 (spec §1.2 rows
//! 2–26), each testable without a GPU.
//!
//! Where a policy departs from virglrenderer 1.1.0 (`vkr_*.c`), the function
//! says so and says why. The short list:
//!
//! * **CPU devices are hidden** ([`expose`]). vkr exposes whatever the loader
//!   enumerates; a guest that picks lavapipe through us gets a slower llvmpipe
//!   with a round trip per call, which is never what it wanted.
//! * **`apiVersion` is capped in `Properties2` too**, not only in
//!   `vkGetPhysicalDeviceProperties` (vkr caps only the latter, where it caches
//!   the struct). Mesa reads both.
//! * **Memory types we cannot back are not host-visible** ([`guest_memory`]).
//!   vkr forwards the host's memory properties unchanged and makes any
//!   host-visible allocation exportable; on WHP the VMM must own the pages a
//!   guest maps, and only types that import host allocations can be backed by
//!   them (ADR-0004, 2026-09-23).
//! * **Sparse features are reported false** ([`mask_features`]). vkr masks
//!   nothing; we implement no sparse command, and vulkaninfo enables
//!   `sparseBinding` when it is offered and then creates a sparse image
//!   (spec §1.2). **So is `bufferDeviceAddressCaptureReplay`** (stage 5b.1):
//!   replaying a capture means handing the driver addresses the guest chose,
//!   and neither `vkGetBufferOpaqueCaptureAddress` nor
//!   `vkGetDeviceMemoryOpaqueCaptureAddress` is served.
//! * **Only the chained structures this stage was written for are
//!   accepted** ([`admits_link`]). The generated protocol decodes every
//!   structure the venus protocol can chain, as vkr's does; vkr then hands
//!   them all to the driver. Here a link outside core 1.0–1.3 and the venus
//!   protocol's own is refused as unimplemented, which is where the decoder
//!   used to refuse it before it decoded the whole protocol.
//! * **The capset's extension mask is what [`admits_link`] admits**
//!   ([`admitted_extension_numbers`]), not everything the protocol decodes.
//!   vkr advertises its whole decode table (`vkr_renderer.c:40-48`) because
//!   it hands every decoded structure to the driver; here a decoded structure
//!   outside the admitted set is fatal, so a bit for its extension would be
//!   an invitation to die. And the mask has to name the extensions promoted
//!   into core 1.1–1.3, because the guest gates even a core structure on its
//!   original extension's bit (`VkPhysicalDeviceSynchronization2Features` on
//!   bit 315, `VkPipelineRenderingCreateInfo` on bit 45) and silently drops
//!   it when that bit is clear.

use crate::venus::capset::{vk_api_version_parts, vk_make_api_version, ExtensionMask};
use crate::venus::protocol::info::{self, EXTENSIONS};
use crate::venus::protocol::{
    VkExtensionProperties, VkPhysicalDeviceFeatures, VkPhysicalDeviceFeatures2,
    VkPhysicalDeviceMemoryProperties, VkPhysicalDeviceProperties2, VkQueueFamilyProperties,
};

use super::host::HostDeviceInfo;

/// The newest Vulkan this renderer reports: 1.3, as virglrenderer's
/// `VKR_MAX_API_VERSION` (`vkr_common.h:39`). The capset's `vk_xml_version`
/// (1.3.269) and protocol spec 2 cap the guest there anyway.
pub const MAX_API_VERSION: u32 = vk_make_api_version(0, 1, 3, 0);

/// The oldest device version exposed: 1.1, below which Mesa's venus drops a
/// device on its own (`vn_physical_device.c:1499-1506`) and vkr refuses an
/// instance (`vkr_instance.c`, "require Vulkan 1.1").
pub const MIN_API_VERSION: u32 = vk_make_api_version(0, 1, 1, 0);

/// `VK_PHYSICAL_DEVICE_TYPE_CPU`.
pub const PHYSICAL_DEVICE_TYPE_CPU: i32 = 4;

/// `VK_MEMORY_PROPERTY_HOST_VISIBLE_BIT`.
pub const MEMORY_PROPERTY_HOST_VISIBLE: u32 = 0x2;
/// `VK_MEMORY_PROPERTY_HOST_COHERENT_BIT`.
pub const MEMORY_PROPERTY_HOST_COHERENT: u32 = 0x4;
/// `VK_MEMORY_PROPERTY_HOST_CACHED_BIT`.
pub const MEMORY_PROPERTY_HOST_CACHED: u32 = 0x8;
/// `VK_MEMORY_PROPERTY_LAZILY_ALLOCATED_BIT`.
pub const MEMORY_PROPERTY_LAZILY_ALLOCATED: u32 = 0x10;
/// The three bits that promise a CPU mapping, which [`guest_memory`] takes
/// off every type the VMM cannot back with its own pages.
pub const MEMORY_PROPERTY_HOST_ANY: u32 =
    MEMORY_PROPERTY_HOST_VISIBLE | MEMORY_PROPERTY_HOST_COHERENT | MEMORY_PROPERTY_HOST_CACHED;

/// The extension the renderer enables on every host device it creates, and
/// requires of every device it exposes: guest-visible memory is our pages,
/// imported.
pub const EXTERNAL_MEMORY_HOST: &str = "VK_EXT_external_memory_host";

/// `vkr_api_version_cap_minor` (`vkr_common.h:167-173`): a version newer than
/// `cap` in major.minor becomes `cap`'s major.minor **with its own patch
/// kept**. A host loader of 1.4.309 therefore reports 1.3.309, as vkr does;
/// Mesa then clamps that to the capset's 1.3.269.
#[must_use]
pub fn cap_minor(version: u32, cap: u32) -> u32 {
    let (_, major, minor, patch) = vk_api_version_parts(version);
    let (_, cap_major, cap_minor, _) = vk_api_version_parts(cap);
    if (major, minor) > (cap_major, cap_minor) {
        vk_make_api_version(0, cap_major, cap_minor, patch)
    } else {
        version
    }
}

/// The bytes of a fixed `char[N]` up to its first NUL.
#[must_use]
pub fn c_name(bytes: &[u8]) -> &[u8] {
    let end = bytes.iter().position(|b| *b == 0).unwrap_or(bytes.len());
    bytes.get(..end).unwrap_or_default()
}

/// A fixed `char[256]` holding `name`, NUL-padded (truncated to 255 bytes).
#[must_use]
pub fn name_array(name: &str) -> [u8; 256] {
    let mut out = [0u8; 256];
    for (slot, byte) in out.iter_mut().take(255).zip(name.bytes()) {
        *slot = byte;
    }
    out
}

/// The memory properties a guest sees: the host's, **type indices
/// unchanged**, with `HOST_VISIBLE | HOST_COHERENT | HOST_CACHED` cleared on
/// every type not in `importable` — the `memoryTypeBits` the host answered
/// for an import of our own pages.
///
/// Indices stay put so that every `memoryTypeBits` the host reports later
/// (image and buffer requirements) means the same types to the guest without
/// translation. A type that loses its host bits is still there and still
/// device-usable; the guest simply cannot map it, which is the truth: on WHP
/// a guest mapping must be pages the VMM owns (ADR-0004, 2026-09-23), and the
/// driver's own BAR memory (the 214 MiB `DEVICE_LOCAL|HOST_VISIBLE` type on
/// the RTX 2070) cannot be imported into.
///
/// Nothing is ever *added*: an importable type the host does not call
/// host-visible stays as the host describes it.
#[must_use]
pub fn guest_memory(
    host: &VkPhysicalDeviceMemoryProperties,
    importable: u32,
) -> VkPhysicalDeviceMemoryProperties {
    let mut out = host.clone();
    let count = usize::try_from(host.memory_type_count).unwrap_or(0);
    for (index, ty) in out.memory_types.iter_mut().enumerate().take(count) {
        let bit = u32::try_from(index).ok().and_then(|i| 1u32.checked_shl(i));
        if bit.is_none_or(|bit| importable & bit == 0) {
            ty.property_flags &= !MEMORY_PROPERTY_HOST_ANY;
        }
    }
    out
}

/// Whether any memory type the guest sees is `HOST_VISIBLE | HOST_COHERENT`.
///
/// Mesa's venus assumes one exists and writes past `memoryTypes[32]` when it
/// does not (spec §1.2 row 13), so a device without one is never exposed.
#[must_use]
pub fn has_coherent_host_type(memory: &VkPhysicalDeviceMemoryProperties) -> bool {
    let want = MEMORY_PROPERTY_HOST_VISIBLE | MEMORY_PROPERTY_HOST_COHERENT;
    let count = usize::try_from(memory.memory_type_count).unwrap_or(0);
    memory
        .memory_types
        .iter()
        .take(count)
        .any(|ty| ty.property_flags & want == want)
}

/// Features the executor cannot back reported false: every sparse feature.
/// No sparse command is implemented (`vkQueueBindSparse`,
/// `vkGetPhysicalDeviceSparseImageFormatProperties*`), and vulkaninfo turns
/// `sparseBinding` on when offered and creates a sparse image (spec §1.2).
/// And `bufferDeviceAddressCaptureReplay`, in both structures that carry it:
/// its two commands are not served and its create-time structures would
/// hand the driver addresses the guest chose. vkr masks nothing.
pub fn mask_features(features: &mut VkPhysicalDeviceFeatures2) {
    use crate::venus::protocol::VkPhysicalDeviceFeatures2Next as N;
    mask_core_features(&mut features.features);
    for link in &mut features.p_next {
        match link {
            N::VkPhysicalDeviceVulkan12Features(f) => {
                f.buffer_device_address_capture_replay = 0;
            }
            N::VkPhysicalDeviceBufferDeviceAddressFeatures(f) => {
                f.buffer_device_address_capture_replay = 0;
            }
            _ => {}
        }
    }
}

/// [`mask_features`] on the core structure alone.
pub fn mask_core_features(core: &mut VkPhysicalDeviceFeatures) {
    core.sparse_binding = 0;
    core.sparse_residency_buffer = 0;
    core.sparse_residency_image2d = 0;
    core.sparse_residency_image3d = 0;
    core.sparse_residency2samples = 0;
    core.sparse_residency4samples = 0;
    core.sparse_residency8samples = 0;
    core.sparse_residency16samples = 0;
    core.sparse_residency_aliased = 0;
}

/// The extensions this stage implements: the venus protocol's own two,
/// which no host device reports. Every other extension the protocol can
/// decode waits for the stage that implements its commands and structures.
pub const IMPLEMENTED_EXTENSIONS: &[&str] =
    &["VK_EXT_command_serialization", "VK_MESA_venus_protocol"];

/// The newest core version whose chained structures this stage accepts.
pub const ADMITTED_CHAIN_API: u32 = vk_make_api_version(0, 1, 3, 0);

/// The device extensions a guest is told about: the host's, **intersected
/// with what the generated protocol can decode and this stage implements**
/// ([`IMPLEMENTED_EXTENSIONS`]), spec versions clamped to the protocol's
/// (`vkr_physical_device_init_extensions` does the same with its own table).
/// Today that is only the two private transport extensions, which no host
/// device reports, so the list is empty — which keeps every pNext chain a
/// correct guest sends inside [`admits_link`].
#[must_use]
pub fn advertised_extensions(host: &[VkExtensionProperties]) -> Vec<VkExtensionProperties> {
    host.iter()
        .filter_map(|ext| {
            let name = std::str::from_utf8(c_name(&ext.extension_name)).ok()?;
            let known = EXTENSIONS.iter().find(|e| e.name == name)?;
            (known.decodable && IMPLEMENTED_EXTENSIONS.contains(&name)).then(|| {
                VkExtensionProperties {
                    extension_name: ext.extension_name,
                    spec_version: ext.spec_version.min(known.spec_version),
                }
            })
        })
        .collect()
}

/// Whether a chained structure of type `stype` is one this stage accepts:
/// core Vulkan 1.0 to [`ADMITTED_CHAIN_API`], or added by one of
/// [`IMPLEMENTED_EXTENSIONS`] — exactly the pNext whitelist the bring-up
/// protocol was generated with (`[api] 1.3`, the two venus extensions).
/// Everything else the protocol can chain decodes, and is refused here.
#[must_use]
pub fn admits_link(stype: i32) -> bool {
    info::structure(stype).is_some_and(|s| {
        s.core.is_some_and(|core| core <= ADMITTED_CHAIN_API)
            || s.extensions
                .iter()
                .any(|e| IMPLEMENTED_EXTENSIONS.contains(e))
    })
}

/// The Vulkan extension numbers the capset's `vk_extension_mask1` carries:
/// every extension that adds a structure [`admits_link`] admits, and the
/// venus protocol's own two — **derived from the same table and the same
/// rule** the executor judges a chain by, so the mask and the executor
/// cannot drift apart.
///
/// That is the two protocol extensions plus the extensions promoted into
/// core 1.1–1.3 that brought structures with them (60 in all against the
/// generated protocol, including `VK_KHR_synchronization2` = 315 and
/// `VK_KHR_dynamic_rendering` = 45, on which the guest gates core-1.3
/// structures), and one that was not promoted but adds a core structure
/// too: `VK_EXT_buffer_device_address`, which shares
/// `VkBufferDeviceAddressInfo` with its KHR successor. Its bit lets the guest
/// send that one structure, which the executor admits; its own feature
/// structure is not admitted, and the guest chains it only for a device that
/// enabled the extension, which no device here can.
///
/// A promoted extension that added no structure, or whose structures were
/// not themselves promoted (`VK_EXT_4444_formats`,
/// `VK_EXT_extended_dynamic_state`), has no bit: the mask gates only chained
/// structures (`vn_cs_renderer_protocol_has_extension`), so a bit with
/// nothing admitted behind it would promise nothing but a fatal refusal.
#[must_use]
pub fn admitted_extension_numbers() -> Vec<u32> {
    let mut names: Vec<&str> = IMPLEMENTED_EXTENSIONS.to_vec();
    for s in info::STRUCTURES {
        if admits_link(s.stype) {
            names.extend(s.extensions.iter().copied());
        }
    }
    let mut numbers: Vec<u32> = names
        .into_iter()
        .filter_map(|name| info::extension(name).map(|e| e.number))
        .collect();
    numbers.sort_unstable();
    numbers.dedup();
    numbers
}

/// [`admitted_extension_numbers`] as the capset's enumerated mask, sentinel
/// set.
#[must_use]
pub fn admitted_extension_mask() -> ExtensionMask {
    let mut mask = ExtensionMask::ENUMERATED;
    for number in admitted_extension_numbers() {
        // Every registry number in the protocol's table is below 1024; one
        // that were not would simply stay unadvertised.
        let _ = mask.enable(number);
    }
    mask
}

/// Whether the host reports `name` among `extensions`.
#[must_use]
pub fn has_extension(extensions: &[VkExtensionProperties], name: &str) -> bool {
    extensions
        .iter()
        .any(|ext| c_name(&ext.extension_name) == name.as_bytes())
}

/// Why a host physical device is not shown to the guest.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Hidden {
    /// `VK_PHYSICAL_DEVICE_TYPE_CPU`: lavapipe, SwiftShader and friends.
    Cpu,
    /// Below [`MIN_API_VERSION`].
    TooOld {
        /// What it reports.
        api_version: u32,
    },
    /// No `VK_EXT_external_memory_host`, or its probe failed: nothing it could
    /// map for the guest would be pages the VMM owns.
    NoHostImport,
    /// After [`guest_memory`], no type is `HOST_VISIBLE | HOST_COHERENT`.
    NoCoherentHostMemory,
}

impl std::fmt::Display for Hidden {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Cpu => f.write_str("it is a CPU implementation"),
            Self::TooOld { api_version } => {
                let (_, major, minor, patch) = vk_api_version_parts(*api_version);
                write!(f, "it supports only Vulkan {major}.{minor}.{patch}")
            }
            Self::NoHostImport => write!(
                f,
                "it lacks {EXTERNAL_MEMORY_HOST}, so no guest mapping could be our own pages"
            ),
            Self::NoCoherentHostMemory => f.write_str(
                "none of its memory types that accept our pages is HOST_VISIBLE|HOST_COHERENT",
            ),
        }
    }
}

/// A host physical device as the guest is shown it. Built once per guest
/// instance, by [`expose`], and answered from ever after.
#[derive(Debug, Clone, PartialEq)]
pub struct GuestDevice {
    /// Properties and the full chain, `apiVersion` capped at 1.3.
    pub properties: VkPhysicalDeviceProperties2,
    /// Features and the full chain, [`mask_features`] applied.
    pub features: VkPhysicalDeviceFeatures2,
    /// Queue families, as the host reports them.
    pub queue_families: Vec<VkQueueFamilyProperties>,
    /// [`guest_memory`] of the host's.
    pub memory: VkPhysicalDeviceMemoryProperties,
    /// The host's, for diagnostics and the policy table.
    pub host_memory: VkPhysicalDeviceMemoryProperties,
    /// [`advertised_extensions`].
    pub extensions: Vec<VkExtensionProperties>,
    /// `memoryTypeBits` of host-allocation imports.
    pub importable: u32,
    /// `minImportedHostPointerAlignment`: what the executor rounds and
    /// aligns the pages of a host-visible allocation to.
    pub import_alignment: u64,
}

impl GuestDevice {
    /// `deviceName` up to its NUL, lossily as text.
    #[must_use]
    pub fn name(&self) -> String {
        String::from_utf8_lossy(c_name(&self.properties.properties.device_name)).into_owned()
    }

    /// The memory types the guest sees as `HOST_VISIBLE`: exactly the ones
    /// the executor backs with its own imported pages ([`guest_memory`]
    /// leaves the bit only on importable types).
    #[must_use]
    pub fn host_visible_types(&self) -> u32 {
        let count = usize::try_from(self.memory.memory_type_count).unwrap_or(0);
        self.memory
            .memory_types
            .iter()
            .take(count)
            .enumerate()
            .filter(|(_, ty)| ty.property_flags & MEMORY_PROPERTY_HOST_VISIBLE != 0)
            .fold(0u32, |bits, (index, _)| {
                bits | u32::try_from(index)
                    .ok()
                    .and_then(|i| 1u32.checked_shl(i))
                    .unwrap_or(0)
            })
    }

    /// Every memory type index the guest sees, as a bit mask.
    #[must_use]
    pub fn all_types(&self) -> u32 {
        match self.memory.memory_type_count {
            0 => 0,
            n if n >= 32 => u32::MAX,
            n => (1u32 << n) - 1,
        }
    }

    /// The feature bit `bufferDeviceAddress` as the guest is told it.
    #[must_use]
    pub fn buffer_device_address(&self) -> bool {
        use crate::venus::protocol::VkPhysicalDeviceFeatures2Next as N;
        self.features.p_next.iter().any(|link| match link {
            N::VkPhysicalDeviceVulkan12Features(f) => f.buffer_device_address != 0,
            N::VkPhysicalDeviceBufferDeviceAddressFeatures(f) => f.buffer_device_address != 0,
            _ => false,
        })
    }

    /// The feature bit `protectedMemory` as the guest is told it, from
    /// whichever structure carries it.
    #[must_use]
    pub fn protected_memory(&self) -> bool {
        use crate::venus::protocol::VkPhysicalDeviceFeatures2Next as N;
        self.features.p_next.iter().any(|link| match link {
            N::VkPhysicalDeviceVulkan11Features(f) => f.protected_memory != 0,
            N::VkPhysicalDeviceProtectedMemoryFeatures(f) => f.protected_memory != 0,
            _ => false,
        })
    }
}

/// Decide whether a host device is shown to the guest, and build what it is
/// shown if so. The rules, in the order they are checked: not a CPU device,
/// at least Vulkan 1.1, `VK_EXT_external_memory_host` with a working probe,
/// and at least one coherent host-visible type after [`guest_memory`].
///
/// # Errors
/// The first rule the device fails.
pub fn expose(info: HostDeviceInfo) -> Result<GuestDevice, Hidden> {
    let core = &info.properties.properties;
    if core.device_type == PHYSICAL_DEVICE_TYPE_CPU {
        return Err(Hidden::Cpu);
    }
    if core.api_version < MIN_API_VERSION {
        return Err(Hidden::TooOld {
            api_version: core.api_version,
        });
    }
    let importable = match info.host_import_types {
        Some(bits) if has_extension(&info.extensions, EXTERNAL_MEMORY_HOST) => bits,
        _ => return Err(Hidden::NoHostImport),
    };
    let import_alignment = info.host_import_alignment;
    if !import_alignment.is_power_of_two()
        || import_alignment > crate::venus::shmem::MAX_MEMORY_ALIGNMENT
    {
        return Err(Hidden::NoHostImport);
    }
    let memory = guest_memory(&info.memory, importable);
    if !has_coherent_host_type(&memory) {
        return Err(Hidden::NoCoherentHostMemory);
    }
    let mut properties = info.properties;
    properties.properties.api_version =
        cap_minor(properties.properties.api_version, MAX_API_VERSION);
    let mut features = info.features;
    mask_features(&mut features);
    Ok(GuestDevice {
        properties,
        features,
        queue_families: info.queue_families,
        extensions: advertised_extensions(&info.extensions),
        memory,
        host_memory: info.memory,
        importable,
        import_alignment,
    })
}

// ----------------------------------------------------- enum and flag checks
//
// The generated decoder carries every enum and flag word raw, as the C does
// (`vn_protocol_renderer_types.h`). These are the checks that stand between
// a guest's number and the NVIDIA driver, each against Vulkan 1.3 core plus
// nothing: an extension value is only legal once an extension that defines
// it is enabled, and this stage enables none for the guest.

/// A `VkFormat` Vulkan 1.3 core defines (including `VK_FORMAT_UNDEFINED`).
#[must_use]
pub fn is_core_format(format: i32) -> bool {
    matches!(format,
        0..=184
        | 1_000_156_000..=1_000_156_033 // 1.1: YCbCr
        | 1_000_330_000..=1_000_330_003 // 1.3: 2-plane 444
        | 1_000_340_000..=1_000_340_001 // 1.3: 4444
        | 1_000_066_000..=1_000_066_013 // 1.3: ASTC HDR
    )
}

/// `VkImageType`: 1D, 2D, 3D.
#[must_use]
pub fn is_image_type(value: i32) -> bool {
    (0..=2).contains(&value)
}

/// `VkImageTiling`: optimal, linear (no DRM modifiers without the extension).
#[must_use]
pub fn is_image_tiling(value: i32) -> bool {
    (0..=1).contains(&value)
}

/// `VkSharingMode`: exclusive, concurrent.
#[must_use]
pub fn is_sharing_mode(value: i32) -> bool {
    (0..=1).contains(&value)
}

/// `VK_IMAGE_USAGE_*` bits of Vulkan 1.3 core.
pub const IMAGE_USAGE_CORE: u32 = 0xff;

/// `VK_IMAGE_CREATE_*` bits of Vulkan 1.3 core.
pub const IMAGE_CREATE_CORE: u32 = 0xfff;

/// The three sparse `VK_IMAGE_CREATE_*` bits, never legal here because no
/// sparse feature is reported.
pub const IMAGE_CREATE_SPARSE: u32 = 0x7;

/// `VK_IMAGE_CREATE_PROTECTED_BIT`.
pub const IMAGE_CREATE_PROTECTED: u32 = 0x800;

/// `VK_IMAGE_CREATE_MUTABLE_FORMAT_BIT`.
pub const IMAGE_CREATE_MUTABLE_FORMAT: u32 = 0x8;
/// `VK_IMAGE_CREATE_CUBE_COMPATIBLE_BIT`.
pub const IMAGE_CREATE_CUBE_COMPATIBLE: u32 = 0x10;
/// `VK_IMAGE_CREATE_2D_ARRAY_COMPATIBLE_BIT`.
pub const IMAGE_CREATE_2D_ARRAY_COMPATIBLE: u32 = 0x20;
/// `VK_IMAGE_CREATE_DISJOINT_BIT`.
pub const IMAGE_CREATE_DISJOINT: u32 = 0x200;

/// `VK_IMAGE_TILING_LINEAR`.
pub const IMAGE_TILING_LINEAR: i32 = 1;

/// `VK_BUFFER_CREATE_*` bits of Vulkan 1.3 core: the three sparse ones,
/// `PROTECTED` and `DEVICE_ADDRESS_CAPTURE_REPLAY`.
pub const BUFFER_CREATE_CORE: u32 = 0x1f;
/// The sparse `VK_BUFFER_CREATE_*` bits, never legal here.
pub const BUFFER_CREATE_SPARSE: u32 = 0x7;
/// `VK_BUFFER_CREATE_PROTECTED_BIT`.
pub const BUFFER_CREATE_PROTECTED: u32 = 0x8;
/// `VK_BUFFER_CREATE_DEVICE_ADDRESS_CAPTURE_REPLAY_BIT`, never legal here:
/// capture replay is masked ([`mask_features`]).
pub const BUFFER_CREATE_CAPTURE_REPLAY: u32 = 0x10;

/// `VK_BUFFER_USAGE_*` bits of Vulkan 1.0 core (`TRANSFER_SRC` through
/// `INDIRECT_BUFFER`).
pub const BUFFER_USAGE_CORE: u32 = 0x1ff;
/// `VK_BUFFER_USAGE_SHADER_DEVICE_ADDRESS_BIT` (1.2), legal only on a device
/// that enabled `bufferDeviceAddress`.
pub const BUFFER_USAGE_DEVICE_ADDRESS: u32 = 0x2_0000;
/// `VK_BUFFER_USAGE_UNIFORM_TEXEL_BUFFER_BIT | STORAGE_TEXEL_BUFFER_BIT`:
/// one of them is what a buffer view needs.
pub const BUFFER_USAGE_TEXEL: u32 = 0xc;

/// `VK_MEMORY_ALLOCATE_DEVICE_MASK_BIT`.
pub const MEMORY_ALLOCATE_DEVICE_MASK: u32 = 0x1;
/// `VK_MEMORY_ALLOCATE_DEVICE_ADDRESS_BIT`.
pub const MEMORY_ALLOCATE_DEVICE_ADDRESS: u32 = 0x2;
/// `VK_MEMORY_ALLOCATE_DEVICE_ADDRESS_CAPTURE_REPLAY_BIT`.
pub const MEMORY_ALLOCATE_CAPTURE_REPLAY: u32 = 0x4;

/// `VK_IMAGE_ASPECT_*` bits a view or a subresource may name in 1.3 core:
/// colour, depth, stencil and the three planes (not `METADATA`).
pub const IMAGE_ASPECT_VIEW: u32 = 0x77;

/// `VkImageViewType`: 1D through cube array.
#[must_use]
pub fn is_image_view_type(value: i32) -> bool {
    (0..=6).contains(&value)
}

/// `VkComponentSwizzle`: identity through A.
#[must_use]
pub fn is_component_swizzle(value: i32) -> bool {
    (0..=6).contains(&value)
}

/// `VK_DEVICE_QUEUE_CREATE_PROTECTED_BIT`.
pub const QUEUE_CREATE_PROTECTED: u32 = 0x1;

/// `VK_COMMAND_POOL_CREATE_*` bits of Vulkan 1.3 core.
pub const COMMAND_POOL_CREATE_CORE: u32 = 0x7;

/// `VK_COMMAND_POOL_CREATE_PROTECTED_BIT`.
pub const COMMAND_POOL_CREATE_PROTECTED: u32 = 0x4;

/// `VK_EXTERNAL_MEMORY_HANDLE_TYPE_*` bits of Vulkan 1.3 core.
pub const EXTERNAL_MEMORY_HANDLE_CORE: u32 = 0x7f;

/// `VK_IMAGE_ASPECT_PLANE_{0,1,2}_BIT`.
#[must_use]
pub fn is_plane_aspect(value: i32) -> bool {
    matches!(value, 0x10 | 0x20 | 0x40)
}

/// A `VkSampleCountFlagBits`: exactly one bit, 1 through 64.
#[must_use]
pub fn is_sample_count(value: i32) -> bool {
    matches!(value, 1 | 2 | 4 | 8 | 16 | 32 | 64)
}

/// `VK_IMAGE_LAYOUT_UNDEFINED` or `VK_IMAGE_LAYOUT_PREINITIALIZED`, the only
/// two an image may be created in.
#[must_use]
pub fn is_initial_layout(value: i32) -> bool {
    matches!(value, 0 | 8)
}

/// Zero or a single `VkExternalMemoryHandleTypeFlagBits` of Vulkan 1.3 core.
#[must_use]
pub fn is_handle_type_bit(value: i32) -> bool {
    u32::try_from(value)
        .is_ok_and(|v| v == 0 || (v.is_power_of_two() && v & !EXTERNAL_MEMORY_HANDLE_CORE == 0))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::venus::protocol::VkMemoryType;

    fn memory(flags: &[u32]) -> VkPhysicalDeviceMemoryProperties {
        let mut out = VkPhysicalDeviceMemoryProperties {
            memory_type_count: flags.len() as u32,
            ..Default::default()
        };
        for (slot, flags) in out.memory_types.iter_mut().zip(flags) {
            *slot = VkMemoryType {
                property_flags: *flags,
                heap_index: 0,
            };
        }
        out
    }

    #[test]
    fn cap_minor_keeps_the_patch_as_vkr_does() {
        assert_eq!(
            cap_minor(vk_make_api_version(0, 1, 4, 309), MAX_API_VERSION),
            vk_make_api_version(0, 1, 3, 309)
        );
        let older = vk_make_api_version(0, 1, 2, 198);
        assert_eq!(cap_minor(older, MAX_API_VERSION), older);
        let same = vk_make_api_version(0, 1, 3, 280);
        assert_eq!(cap_minor(same, MAX_API_VERSION), same);
    }

    #[test]
    fn a_type_we_cannot_import_into_loses_every_host_bit_and_keeps_its_index() {
        // The RTX 2070's shape: 0-2 device local, 3/4 host visible and
        // importable, 5 the BAR (device local + host visible, not importable).
        let host = memory(&[0x1, 0x1, 0x1, 0x6, 0xe, 0x7]);
        let guest = guest_memory(&host, 0x18);
        let flags: Vec<u32> = guest.memory_types[..6]
            .iter()
            .map(|t| t.property_flags)
            .collect();
        assert_eq!(flags, vec![0x1, 0x1, 0x1, 0x6, 0xe, 0x1]);
        assert_eq!(guest.memory_type_count, 6);
        assert!(has_coherent_host_type(&guest));
        assert!(!has_coherent_host_type(&guest_memory(&host, 0)));
    }

    #[test]
    fn importability_never_adds_a_bit() {
        let host = memory(&[0x1, 0x2]);
        let guest = guest_memory(&host, 0x3);
        assert_eq!(guest.memory_types[0].property_flags, 0x1);
        assert_eq!(guest.memory_types[1].property_flags, 0x2);
        assert!(!has_coherent_host_type(&guest), "visible but not coherent");
    }

    #[test]
    fn a_memory_type_count_past_32_cannot_escape_the_array() {
        let mut host = memory(&[0x6]);
        host.memory_type_count = 1000;
        let _ = guest_memory(&host, u32::MAX);
        let _ = has_coherent_host_type(&host);
    }

    #[test]
    fn only_decodable_extensions_are_advertised() {
        let host = [
            VkExtensionProperties {
                extension_name: name_array("VK_KHR_swapchain"),
                spec_version: 70,
            },
            VkExtensionProperties {
                extension_name: name_array(EXTERNAL_MEMORY_HOST),
                spec_version: 1,
            },
            VkExtensionProperties {
                extension_name: name_array("VK_MESA_venus_protocol"),
                spec_version: 99,
            },
        ];
        let out = advertised_extensions(&host);
        assert_eq!(out.len(), 1, "only the decodable one survives");
        assert_eq!(c_name(&out[0].extension_name), b"VK_MESA_venus_protocol");
        assert_eq!(out[0].spec_version, 4, "clamped to the protocol's");
        assert!(has_extension(&host, EXTERNAL_MEMORY_HOST));
    }

    #[test]
    fn the_capset_mask_is_what_the_executor_admits_and_names_the_promoted_extensions() {
        let numbers = admitted_extension_numbers();
        // The two protocol extensions, and the ones the guest gates core
        // structures on (ADR-0004, 2026-09-23: sync2 and dynamic rendering).
        for (number, name) in [
            (384, "VK_EXT_command_serialization"),
            (385, "VK_MESA_venus_protocol"),
            (315, "VK_KHR_synchronization2"),
            (45, "VK_KHR_dynamic_rendering"),
            (414, "VK_KHR_maintenance4"),
            (61, "VK_KHR_device_group"),
            (128, "VK_KHR_dedicated_allocation"),
            (158, "VK_KHR_bind_memory2"),
            (147, "VK_KHR_get_memory_requirements2"),
        ] {
            assert!(numbers.contains(&number), "{name} ({number})");
            assert_eq!(info::extension(name).map(|e| e.number), Some(number));
        }
        // Nothing whose structures the executor would refuse.
        for (number, name) in [
            (288, "VK_EXT_custom_border_color"),
            (471, "VK_KHR_maintenance5"),
            (1, "VK_KHR_swapchain"),
            (158 + 1000, "no such extension"),
        ] {
            assert!(!numbers.contains(&number), "{name} ({number})");
        }
        // Every bit is an extension one of whose structures is admitted —
        // or one of the two protocol extensions — so the mask and the chain
        // policy are one rule.
        for number in &numbers {
            let ext = info::EXTENSIONS
                .iter()
                .find(|e| e.number == *number)
                .expect("a known extension");
            let admitted = IMPLEMENTED_EXTENSIONS.contains(&ext.name)
                || info::STRUCTURES
                    .iter()
                    .any(|s| admits_link(s.stype) && s.extensions.contains(&ext.name));
            assert!(admitted, "{}", ext.name);
        }
        assert_eq!(numbers.len(), 60);
        let mask = admitted_extension_mask();
        assert!(mask.is_enumerated());
        let set: u32 = mask.words().iter().map(|w| w.count_ones()).sum();
        assert_eq!(set, 61, "60 extensions and the sentinel");
    }

    #[test]
    fn the_enum_checks_hold_core_1_3_and_nothing_else() {
        assert!(is_core_format(0) && is_core_format(37) && is_core_format(184));
        assert!(!is_core_format(185) && !is_core_format(-1));
        assert!(is_core_format(1_000_156_000) && !is_core_format(1_000_054_000));
        assert!(is_image_type(2) && !is_image_type(3));
        assert!(is_image_tiling(1) && !is_image_tiling(1_000_158_000));
        assert!(is_sample_count(64) && !is_sample_count(3) && !is_sample_count(128));
        assert!(is_initial_layout(8) && !is_initial_layout(1));
        assert!(is_handle_type_bit(0) && is_handle_type_bit(0x40));
        assert!(!is_handle_type_bit(0x3) && !is_handle_type_bit(0x80));
        assert!(is_plane_aspect(0x20) && !is_plane_aspect(0x1));
    }
}
