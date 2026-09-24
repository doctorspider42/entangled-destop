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
//! * **Device extensions are the ones this renderer serves** (stages 5b.3
//!   and 5c, [`advertised_extensions`]): the [`PROMOTED_EXTENSIONS`] and the
//!   [`ADMITTED_EXTENSIONS`] passed through when the host has them, and
//!   `VK_KHR_external_semaphore_fd`, `VK_EXT_external_memory_dma_buf` and
//!   `VK_KHR_external_memory_fd` **emulated** — a Windows host has neither
//!   sync_fd nor dma-buf, and Mesa 26.0.8 exposes Vulkan 1.3 and
//!   `VK_KHR_swapchain` only on a renderer that can import a sync file
//!   ([`external_semaphore_properties`]), and the `KHR_external_memory_fd`
//!   Zink's DRM screen needs only on one that lists dma-buf. vkr advertises
//!   what the host driver has.
//! * **The identity is shaped** ([`shape_identity`], stage 5c): an NVIDIA
//!   device is shown with the virtio PCI vendor, and an NVIDIA driver new
//!   enough for venus's dma-buf WSI is shown just below it. vkr forwards
//!   both.
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
    VkExtensionProperties, VkExternalMemoryProperties, VkExternalSemaphoreProperties,
    VkPhysicalDeviceFeatures, VkPhysicalDeviceFeatures2, VkPhysicalDeviceMemoryProperties,
    VkPhysicalDeviceProperties2, VkQueueFamilyProperties,
};

use super::host::HostDeviceInfo;

/// The newest Vulkan this renderer reports: 1.3, as virglrenderer's
/// `VKR_MAX_API_VERSION` (`vkr_common.h:39`). The capset's `vk_xml_version`
/// (1.3.269) and protocol spec 2 cap the guest there anyway.
pub const MAX_API_VERSION: u32 = vk_make_api_version(0, 1, 3, 0);

/// Vulkan 1.2: what Mesa 26.0.8 clamps a device to when it does not expose
/// `VK_KHR_synchronization2` (`vn_physical_device.c:538-543`).
pub const API_1_2: u32 = vk_make_api_version(0, 1, 2, 0);

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

/// The venus protocol's own two extensions, which no host device reports:
/// their structures are admitted, and their commands are the transport's
/// and `executor::submit::dispatch_extension`'s.
pub const PROTOCOL_EXTENSIONS: &[&str] =
    &["VK_EXT_command_serialization", "VK_MESA_venus_protocol"];

/// Every extension whose **structures and commands** this renderer
/// implements outside core 1.0–1.3: the [`PROTOCOL_EXTENSIONS`], and the
/// device extensions stage 5c passes through for Zink (ADR-0004,
/// 2026-09-24, "OpenGL on the host GPU"). A structure one of them adds is
/// admitted by [`admits_link`] — per device, only once the device enabled an
/// extension that brings it (`executor::generated`) — and gets its bit in the
/// capset ([`admitted_extension_numbers`]); a command one of them adds is
/// classified in `tools/venus-protocol/executor-classes.txt` and served.
///
/// **Read by `scripts/venus-ash-gen.py` and `scripts/venus-exec-gen.py`**,
/// which parse this list out of this file: the generated bridge, the
/// generated translation and the policy are one list.
///
/// What each obliges the renderer to:
///
/// * `VK_EXT_robustness2` / `VK_KHR_robustness2` — feature and property
///   structures passed through; with `nullDescriptor` enabled, a null image
///   view, buffer view or buffer in a descriptor write and a null vertex
///   buffer are the guest's to send (`executor::device_objects`).
/// * `VK_EXT_transform_feedback` — its structures; `vkCmdBindTransformFeedbackBuffersEXT`,
///   `vkCmd{Begin,End}TransformFeedbackEXT`, `vkCmd{Begin,End}QueryIndexedEXT`
///   and `vkCmdDrawIndirectByteCountEXT`, each bounded by hand; its buffer
///   usage bits, query type, stages and accesses once enabled.
/// * `VK_EXT_conditional_rendering` — its structures;
///   `vkCmdBeginConditionalRenderingEXT` (bounded) and
///   `vkCmdEndConditionalRenderingEXT`.
/// * `VK_EXT_line_rasterization` / `VK_KHR_line_rasterization` — its
///   structures and `vkCmdSetLineStipple` (the factor bounded).
/// * `VK_EXT_depth_clip_enable`, `VK_EXT_provoking_vertex`,
///   `VK_EXT_border_color_swizzle` — structures only.
/// * `VK_EXT_vertex_attribute_divisor` / `VK_KHR_vertex_attribute_divisor`
///   — structures only; a divisor's binding inside the device's limits.
/// * `VK_EXT_custom_border_color` — structures and two border colours; no
///   more live samplers with one than `maxCustomBorderColorSamplers`.
pub const ADMITTED_EXTENSIONS: &[&str] = &[
    "VK_EXT_command_serialization",
    "VK_MESA_venus_protocol",
    "VK_EXT_border_color_swizzle",
    "VK_EXT_conditional_rendering",
    "VK_EXT_custom_border_color",
    "VK_EXT_depth_clip_enable",
    "VK_EXT_line_rasterization",
    "VK_EXT_provoking_vertex",
    "VK_EXT_robustness2",
    "VK_EXT_transform_feedback",
    "VK_EXT_vertex_attribute_divisor",
    "VK_KHR_line_rasterization",
    "VK_KHR_robustness2",
    "VK_KHR_vertex_attribute_divisor",
];

/// Extensions promoted into core Vulkan 1.1–1.3 that Mesa 26.0.8's venus
/// passes through (`vn_physical_device_get_passthrough_extensions`,
/// `vn_physical_device.c:1266-1330`), with the core minor version they were
/// promoted to. Every command each adds is core in that version and encoded
/// as the core command, and every structure is a core structure already
/// admitted — which is the rule this list was drawn up by, from vk.xml, and
/// the reason it is **not** every promoted extension of that list:
///
/// * `VK_KHR_device_group` also adds swapchain interactions
///   (`VkImageSwapchainCreateInfoKHR`, `vkAcquireNextImage2KHR`) no stage
///   serves;
/// * `VK_EXT_4444_formats`, `VK_EXT_extended_dynamic_state`,
///   `VK_EXT_extended_dynamic_state2`, `VK_EXT_texel_buffer_alignment` and
///   `VK_EXT_ycbcr_2plane_444_formats` keep a feature structure that was not
///   promoted (and `extended_dynamic_state2` two commands that were not), so
///   advertising them would promise structures the executor refuses;
/// * `VK_KHR_descriptor_update_template` is in, because its one non-core
///   command, `vkCmdPushDescriptorSetWithTemplateKHR`, exists only with
///   `VK_KHR_push_descriptor`, which is not advertised.
///
/// Zink requires five of them by name — core-1.3 promotion does not count
/// for it: `VK_KHR_maintenance1`, `VK_KHR_create_renderpass2`,
/// `VK_KHR_imageless_framebuffer`, `VK_KHR_dynamic_rendering` and
/// `VK_KHR_descriptor_update_template` (`zink_device_info.py:62-63,93-94,
/// 180-183,203-206,313-314`, refused at `:755-758`) — and gates GL 4.0 on
/// the `VK_KHR_maintenance2` string and 4.6 on `VK_KHR_draw_indirect_count`.
/// Each is advertised only on a device of that version or newer, where its
/// entry points exist on the host and the executor's version gate lets them
/// through.
pub const PROMOTED_EXTENSIONS: &[(&str, u32)] = &[
    // promoted to Vulkan 1.1
    ("VK_KHR_16bit_storage", 1),
    ("VK_KHR_bind_memory2", 1),
    ("VK_KHR_dedicated_allocation", 1),
    ("VK_KHR_descriptor_update_template", 1),
    ("VK_KHR_external_fence", 1),
    ("VK_KHR_external_memory", 1),
    ("VK_KHR_external_semaphore", 1),
    ("VK_KHR_get_memory_requirements2", 1),
    ("VK_KHR_maintenance1", 1),
    ("VK_KHR_maintenance2", 1),
    ("VK_KHR_maintenance3", 1),
    ("VK_KHR_multiview", 1),
    ("VK_KHR_relaxed_block_layout", 1),
    ("VK_KHR_sampler_ycbcr_conversion", 1),
    ("VK_KHR_shader_draw_parameters", 1),
    ("VK_KHR_storage_buffer_storage_class", 1),
    ("VK_KHR_variable_pointers", 1),
    // promoted to Vulkan 1.2
    ("VK_KHR_8bit_storage", 2),
    ("VK_KHR_buffer_device_address", 2),
    ("VK_KHR_create_renderpass2", 2),
    ("VK_KHR_depth_stencil_resolve", 2),
    ("VK_KHR_draw_indirect_count", 2),
    ("VK_KHR_driver_properties", 2),
    ("VK_KHR_image_format_list", 2),
    ("VK_KHR_imageless_framebuffer", 2),
    ("VK_KHR_sampler_mirror_clamp_to_edge", 2),
    ("VK_KHR_separate_depth_stencil_layouts", 2),
    ("VK_KHR_shader_atomic_int64", 2),
    ("VK_KHR_shader_float16_int8", 2),
    ("VK_KHR_shader_float_controls", 2),
    ("VK_KHR_shader_subgroup_extended_types", 2),
    ("VK_KHR_spirv_1_4", 2),
    ("VK_KHR_timeline_semaphore", 2),
    ("VK_KHR_uniform_buffer_standard_layout", 2),
    ("VK_KHR_vulkan_memory_model", 2),
    ("VK_EXT_descriptor_indexing", 2),
    ("VK_EXT_host_query_reset", 2),
    ("VK_EXT_sampler_filter_minmax", 2),
    ("VK_EXT_scalar_block_layout", 2),
    ("VK_EXT_separate_stencil_usage", 2),
    ("VK_EXT_shader_viewport_index_layer", 2),
    // promoted to Vulkan 1.3
    ("VK_KHR_copy_commands2", 3),
    ("VK_KHR_dynamic_rendering", 3),
    ("VK_KHR_format_feature_flags2", 3),
    ("VK_KHR_maintenance4", 3),
    ("VK_KHR_shader_integer_dot_product", 3),
    ("VK_KHR_shader_non_semantic_info", 3),
    ("VK_KHR_shader_terminate_invocation", 3),
    ("VK_KHR_synchronization2", 3),
    ("VK_KHR_zero_initialize_workgroup_memory", 3),
    ("VK_EXT_image_robustness", 3),
    ("VK_EXT_inline_uniform_block", 3),
    ("VK_EXT_pipeline_creation_cache_control", 3),
    ("VK_EXT_pipeline_creation_feedback", 3),
    ("VK_EXT_private_data", 3),
    ("VK_EXT_shader_demote_to_helper_invocation", 3),
    ("VK_EXT_subgroup_size_control", 3),
    ("VK_EXT_texture_compression_astc_hdr", 3),
];

/// `VK_KHR_synchronization2`: passed through from the host (stage 5b.3).
/// Everything it adds is core Vulkan 1.3 — `vkQueueSubmit2`,
/// `vkCmdPipelineBarrier2`, `vkCmd*Event2`, `vkCmdWriteTimestamp2` and their
/// structures, whose KHR aliases the protocol encodes as the core commands —
/// so it is advertised only on a device of 1.3 or newer, where every one of
/// those entry points exists on the host and the executor serves it. One of
/// the [`PROMOTED_EXTENSIONS`].
pub const SYNCHRONIZATION_2: &str = "VK_KHR_synchronization2";

/// `VK_KHR_external_semaphore_fd`: **emulated**, advertised on every device
/// whatever the host has (stage 5b.3). The guest never sends its own two
/// commands — Mesa implements `vkImportSemaphoreFdKHR` and
/// `vkGetSemaphoreFdKHR` itself — and it adds no chained structure, so it
/// needs no capset bit. What it obliges the renderer to: the `SYNC_FD`
/// answer of [`external_semaphore_properties`], a `vkCreateDevice` that
/// enables it (stripped before the host sees it), a
/// `VkExportSemaphoreCreateInfo{SYNC_FD}` (stripped likewise), and
/// `vkImportSemaphoreResourceMESA` / `vkWaitSemaphoreResourceMESA`
/// (`executor::submit`).
pub const EXTERNAL_SEMAPHORE_FD: &str = "VK_KHR_external_semaphore_fd";

/// `VK_EXT_external_memory_dma_buf`: **emulated** (stage 5c) — a Windows
/// driver has no dma-buf. Its one effect on the guest is the one that
/// matters: Mesa 26.0.8's venus takes `DMA_BUF` as the renderer's handle type
/// when the renderer lists it (`vn_physical_device.c:1040-1051`) and then
/// exposes `VK_KHR_external_memory_fd` and `VK_EXT_external_memory_dma_buf`
/// of its own (`:1203-1207`), which Zink's DRM screen requires
/// (`zink_screen.c:3862-3866`). What the guest then sends, and what each
/// becomes, is the executor's external-memory emulation
/// (`executor::memory`): `DMA_BUF` external-memory queries answered for what
/// our pages can do ([`external_buffer_properties`]), export allocations
/// whose blob is our pages, and imports (`VkImportMemoryResourceInfoMESA`)
/// of a blob of this renderer's own pages. Its handle-type bit is the only
/// external-memory value outside core 1.3 a device that enabled it may name.
pub const EXTERNAL_MEMORY_DMA_BUF: &str = "VK_EXT_external_memory_dma_buf";

/// `VK_KHR_external_memory_fd`: **emulated**. Never needed by the guest
/// from us — venus implements it itself — but venus enables it (and
/// [`EXTERNAL_MEMORY_DMA_BUF`]) on every device an application wants a
/// swapchain or an fd on, when the renderer's handle type is `DMA_BUF`
/// (`vn_device.c:318-330`), so a device create naming it must be accepted,
/// and it is stripped before the host sees it. It chains no structure the
/// guest sends, and its two commands are Mesa's own.
pub const EXTERNAL_MEMORY_FD: &str = "VK_KHR_external_memory_fd";

/// `VK_EXTERNAL_MEMORY_HANDLE_TYPE_DMA_BUF_BIT_EXT`: the renderer's handle
/// type as the guest sees it.
pub const MEMORY_HANDLE_DMA_BUF: u32 = 0x200;
/// `VK_EXTERNAL_MEMORY_HANDLE_TYPE_HOST_ALLOCATION_BIT_EXT`: what a `DMA_BUF`
/// export or import is on the host — our pages.
pub const MEMORY_HANDLE_HOST_ALLOCATION: u32 = 0x80;
/// `VK_EXTERNAL_MEMORY_FEATURE_DEDICATED_ONLY_BIT`.
pub const MEMORY_FEATURE_DEDICATED_ONLY: u32 = 0x1;
/// `VK_EXTERNAL_MEMORY_FEATURE_EXPORTABLE_BIT`.
pub const MEMORY_FEATURE_EXPORTABLE: u32 = 0x2;
/// `VK_EXTERNAL_MEMORY_FEATURE_IMPORTABLE_BIT`.
pub const MEMORY_FEATURE_IMPORTABLE: u32 = 0x4;

/// `VK_EXTERNAL_SEMAPHORE_HANDLE_TYPE_SYNC_FD_BIT`.
pub const SEMAPHORE_HANDLE_SYNC_FD: u32 = 0x10;
/// Every `VK_EXTERNAL_SEMAPHORE_HANDLE_TYPE_*` bit of Vulkan 1.3 core.
pub const SEMAPHORE_HANDLE_CORE: u32 = 0x1f;
/// `VK_EXTERNAL_SEMAPHORE_FEATURE_EXPORTABLE_BIT`.
pub const SEMAPHORE_FEATURE_EXPORTABLE: u32 = 0x1;
/// `VK_EXTERNAL_SEMAPHORE_FEATURE_IMPORTABLE_BIT`.
pub const SEMAPHORE_FEATURE_IMPORTABLE: u32 = 0x2;
/// `VK_SEMAPHORE_TYPE_BINARY` / `_TIMELINE`.
pub const SEMAPHORE_TYPE_BINARY: i32 = 0;
/// `VK_SEMAPHORE_TYPE_TIMELINE`.
pub const SEMAPHORE_TYPE_TIMELINE: i32 = 1;

/// The extensions a device is shown that the host need not have: their
/// spec version as this renderer implements them.
pub const EMULATED_EXTENSIONS: &[(&str, u32)] = &[
    (EXTERNAL_SEMAPHORE_FD, 1),
    (EXTERNAL_MEMORY_DMA_BUF, 1),
    (EXTERNAL_MEMORY_FD, 1),
];

/// What `vkGetPhysicalDeviceExternalSemaphoreProperties` answers.
///
/// **`SYNC_FD` is synthesized**: `IMPORTABLE` for a binary semaphore, with
/// `SYNC_FD` its one compatible type, and nothing for a timeline one (a sync
/// file is binary). That is exactly what Mesa 26.0.8 needs to set
/// `renderer_sync_fd.semaphore_importable` (`vn_physical_device.c:1124-1141`),
/// which is the gate on `VK_KHR_synchronization2` — and with it Vulkan 1.3 —
/// and on `VK_KHR_swapchain` (`:1212-1224`, `:1262-1271`). A Windows driver
/// answers 0 for it; this renderer emulates the one import Mesa makes
/// (`vkImportSemaphoreResourceMESA` with resource 0, a signalled payload).
///
/// **Not `EXPORTABLE`.** With it the guest would also expose
/// `VK_KHR_external_semaphore_fd` to its own applications (`:1173-1179`),
/// whose `vkGetSemaphoreFdKHR` rests on a virtio-gpu fence per queue and
/// `vkWaitSemaphoreResourceMESA`. Both are implemented, but nothing in the
/// guest's Vulkan 1.3 or its swapchain needs an application to export a
/// sync file, and an export is a promise to a consumer outside Vulkan this
/// renderer has never been tested against; so it is not made.
///
/// Every other handle type gets the host's own answer (`host`).
#[must_use]
pub fn external_semaphore_properties(
    handle_type: u32,
    timeline: bool,
    host: impl FnOnce() -> VkExternalSemaphoreProperties,
) -> VkExternalSemaphoreProperties {
    if handle_type != SEMAPHORE_HANDLE_SYNC_FD {
        return host();
    }
    if timeline {
        return VkExternalSemaphoreProperties::default();
    }
    VkExternalSemaphoreProperties {
        export_from_imported_handle_types: 0,
        compatible_handle_types: SEMAPHORE_HANDLE_SYNC_FD,
        external_semaphore_features: SEMAPHORE_FEATURE_IMPORTABLE,
    }
}

/// What a `DMA_BUF` external-memory query answers (stage 5c), for a buffer
/// or an image whose host twin would take an import of our own pages
/// (`host_importable`: the host answers `HOST_ALLOCATION` `IMPORTABLE` for
/// it).
///
/// A `DMA_BUF` here is a blob of this renderer's pages: an export is memory
/// the guest can map, whose blob is those pages; an import is such a blob,
/// imported again. So the honest answer is **exportable and importable
/// exactly when the resource can live in those pages**, and nothing
/// otherwise — what vkr gets from a Linux driver for a resource it cannot
/// share. The shape is vkr's (the driver's answer passed through): `DMA_BUF`
/// compatible with itself, and exportable from an import of itself. Mesa
/// 26.0.8 then widens the compatible types to its own supported set
/// (`vn_physical_device.c:2950-2953`).
#[must_use]
pub fn external_memory_properties(host_importable: bool) -> VkExternalMemoryProperties {
    if !host_importable {
        return VkExternalMemoryProperties {
            external_memory_features: 0,
            export_from_imported_handle_types: 0,
            compatible_handle_types: MEMORY_HANDLE_DMA_BUF,
        };
    }
    VkExternalMemoryProperties {
        external_memory_features: MEMORY_FEATURE_EXPORTABLE | MEMORY_FEATURE_IMPORTABLE,
        export_from_imported_handle_types: MEMORY_HANDLE_DMA_BUF,
        compatible_handle_types: MEMORY_HANDLE_DMA_BUF,
    }
}

/// The newest core version whose chained structures this stage accepts
/// without an extension.
pub const ADMITTED_CHAIN_API: u32 = vk_make_api_version(0, 1, 3, 0);

/// Whether `name` is advertised when the host has it, on a device the guest
/// is shown as `api`: one of the [`ADMITTED_EXTENSIONS`] that is not the
/// protocol's own, or one of the [`PROMOTED_EXTENSIONS`] on a device of its
/// promotion version or newer.
#[must_use]
pub fn passes_through(name: &str, api: u32) -> bool {
    let (_, major, minor, _) = vk_api_version_parts(api);
    (ADMITTED_EXTENSIONS.contains(&name) && !PROTOCOL_EXTENSIONS.contains(&name))
        || PROMOTED_EXTENSIONS
            .iter()
            .any(|(e, promoted)| *e == name && (major, minor) >= (1, *promoted))
}

/// The device extensions a guest is told about (stages 5b.3 and 5c):
///
/// * the host's, **intersected with what the generated protocol can decode
///   and this renderer serves** ([`passes_through`]; `api` is the device's
///   `apiVersion` as the guest is shown it), spec versions clamped to the
///   protocol's (`vkr_physical_device_init_extensions` does the same with its
///   own table);
/// * and the [`EMULATED_EXTENSIONS`], whatever the host has, at the spec
///   version this renderer implements (clamped to the protocol's too).
///
/// Every structure they bring is inside [`admits_link`]. This is the set
/// for a device whose guest WSI stays on its software path
/// ([`advertised_extensions_on`]).
#[must_use]
pub fn advertised_extensions(
    host: &[VkExtensionProperties],
    api: u32,
) -> Vec<VkExtensionProperties> {
    advertised_extensions_on(host, api, true)
}

/// [`advertised_extensions`] for a device whose identity does
/// (`software_wsi`) or does not keep Mesa 26.0.8's WSI on its software
/// path ([`keeps_software_wsi`]).
///
/// **The emulated dma-buf pair is advertised only where it does.** Listing
/// `VK_EXT_external_memory_dma_buf` is also what moves venus's WSI to its
/// native dma-buf path (`vn_wsi.c:134-139`) — swapchain images exported to
/// the guest's compositor as dma-bufs of device-local, optimal-tiling
/// memory, which this renderer cannot make — for every renderer but an
/// NVIDIA driver older than 590.48.01, which [`shape_identity`] makes every
/// NVIDIA driver look like. On any other host the pair would trade a
/// working swapchain for Zink's DRM screen, so it is not offered there, and
/// Zink there stays on the guest's software GL until dma-buf WSI exists.
#[must_use]
pub fn advertised_extensions_on(
    host: &[VkExtensionProperties],
    api: u32,
    software_wsi: bool,
) -> Vec<VkExtensionProperties> {
    let mut out: Vec<VkExtensionProperties> = host
        .iter()
        .filter_map(|ext| {
            let name = std::str::from_utf8(c_name(&ext.extension_name)).ok()?;
            let known = EXTENSIONS.iter().find(|e| e.name == name)?;
            (known.decodable && passes_through(name, api)).then(|| VkExtensionProperties {
                extension_name: ext.extension_name,
                spec_version: ext.spec_version.min(known.spec_version),
            })
        })
        .collect();
    for (name, spec) in EMULATED_EXTENSIONS {
        let Some(known) = EXTENSIONS.iter().find(|e| e.name == *name && e.decodable) else {
            continue;
        };
        if !software_wsi && (*name == EXTERNAL_MEMORY_DMA_BUF || *name == EXTERNAL_MEMORY_FD) {
            continue;
        }
        out.retain(|e| c_name(&e.extension_name) != name.as_bytes());
        out.push(VkExtensionProperties {
            extension_name: name_array(name),
            spec_version: (*spec).min(known.spec_version),
        });
    }
    out
}

/// Whether `name` is one of the [`EMULATED_EXTENSIONS`]: enabled for the
/// guest, never for the host driver.
#[must_use]
pub fn is_emulated_extension(name: &str) -> bool {
    EMULATED_EXTENSIONS.iter().any(|(e, _)| *e == name)
}

/// Whether a chained structure of type `stype` is one this stage accepts:
/// core Vulkan 1.0 to [`ADMITTED_CHAIN_API`], or added by one of
/// [`ADMITTED_EXTENSIONS`]. Everything else the protocol can chain decodes,
/// and is refused here. (A device-level structure of an extension is further
/// refused on a device that did not enable an extension that brings it —
/// the generated translation's rule, `executor::generated`.)
#[must_use]
pub fn admits_link(stype: i32) -> bool {
    info::structure(stype).is_some_and(|s| {
        s.core.is_some_and(|core| core <= ADMITTED_CHAIN_API)
            || s.extensions.iter().any(|e| ADMITTED_EXTENSIONS.contains(e))
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
    let mut names: Vec<&str> = PROTOCOL_EXTENSIONS.to_vec();
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

    /// The device's Vulkan version **as the guest's driver will show it**:
    /// what [`expose`] reported (the host's, capped at 1.3), clamped to 1.2
    /// when `VK_KHR_synchronization2` is not advertised, because Mesa 26.0.8
    /// clamps it there then (`vn_physical_device.c:538-543`). A command newer
    /// than this is refused: the guest cannot have one of its own to send,
    /// and the host's entry point for it may not exist.
    #[must_use]
    pub fn api_version(&self) -> u32 {
        let reported = self.properties.properties.api_version;
        if has_extension(&self.extensions, SYNCHRONIZATION_2) {
            reported
        } else {
            reported.min(API_1_2)
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

/// The virtio PCI vendor id, `0x1af4`: the vendor of the device the guest's
/// DRM render node really is (Mesa's `VIRTGPU_PCI_VENDOR_ID`,
/// `vn_renderer_virtgpu.c:44`, `:1461`).
pub const VIRTIO_PCI_VENDOR_ID: u32 = 0x1af4;
/// NVIDIA's PCI vendor id.
pub const NVIDIA_VENDOR_ID: u32 = 0x10de;
/// `VK_DRIVER_ID_NVIDIA_PROPRIETARY`.
pub const DRIVER_ID_NVIDIA_PROPRIETARY: i32 = 4;

/// `VN_MAKE_NVIDIA_VERSION` (`vn_common.h:76-78`): NVIDIA's own packing of
/// `driverVersion` — major in bits 22 and up, minor in 14–21, sub-minor in
/// 6–13, patch in 0–5. The RTX 2070's 580.88 is `0x9116_0000`, which is what
/// its `vulkaninfo` prints (2434138112).
#[must_use]
pub const fn nvidia_version(major: u32, minor: u32, sub_minor: u32, patch: u32) -> u32 {
    (major << 22) | (minor << 14) | (sub_minor << 6) | patch
}

/// The first NVIDIA driver Mesa 26.0.8's venus trusts with its dma-buf WSI:
/// below it, venus keeps the software WSI whatever the renderer offers
/// (`vn_wsi.c:134-139`).
pub const NVIDIA_DMA_BUF_WSI_DRIVER: u32 = nvidia_version(590, 48, 1, 0);

/// What a newer NVIDIA driver is reported as: 590.48.0.0, just below
/// [`NVIDIA_DMA_BUF_WSI_DRIVER`].
pub const NVIDIA_SOFTWARE_WSI_DRIVER: u32 = nvidia_version(590, 48, 0, 0);

/// Shape the identity the guest is shown (stage 5c), in `Properties` and so
/// in `Properties2` too (both answer from this one structure):
///
/// * **An NVIDIA device is shown with the virtio PCI vendor**
///   ([`VIRTIO_PCI_VENDOR_ID`]); `deviceID`, `deviceName`, `driverID` and
///   `driverVersion` are the host's. Mesa 26.0.8's `vn_wsi_init` treats
///   `vendorID == 0x10de` as a real NVIDIA GPU visible to the guest's window
///   system and hides its DRM and PCI identity — clearing
///   `VK_EXT_physical_device_drm` and every render-node number
///   (`vn_wsi.c:155-174`) — so that the WSI fails its same-GPU check. That
///   quirk is right for a passed-through GPU and wrong for this one: the
///   guest's GPU is the virtio device, whose render node is what Zink is
///   handed and matches by those numbers (`zink_screen.c:1666-1685`,
///   `:1731-1777`); with them zeroed, Zink finds no device ("failed to
///   choose pdev"). What keys NVIDIA-specific workarounds — Zink's and
///   venus's own (`zink_screen.c:2943`, `vn_query_pool.c:135`) — is
///   `driverID`, which stays the host's.
/// * **While this renderer cannot present through a dma-buf, an NVIDIA
///   driver is shown below the dma-buf WSI gate**: `driverVersion` capped at
///   [`NVIDIA_SOFTWARE_WSI_DRIVER`] when `driverID` is
///   `NVIDIA_PROPRIETARY`. Advertising `VK_EXT_external_memory_dma_buf` (for
///   Zink) would otherwise put venus's WSI on its native dma-buf path for a
///   590.48.01-or-newer host driver (`vn_wsi.c:134-139`), and that path needs
///   exports this renderer cannot make — images with DRM format modifiers,
///   scanned out by the guest's compositor. The guest shows its own
///   `driverVersion` to applications anyway (`vn_physical_device.c:550-554`);
///   the host's is only what venus's workarounds read. **Removing this cap is
///   the switch that turns dma-buf WSI on**, once there is a dma-buf path to
///   turn on. (The RTX 2070's driver today is 580.88, which the cap leaves
///   as it is.)
pub fn shape_identity(properties: &mut VkPhysicalDeviceProperties2) {
    use crate::venus::protocol::VkPhysicalDeviceProperties2Next as N;
    let driver_id = properties.p_next.iter().find_map(|link| match link {
        N::VkPhysicalDeviceVulkan12Properties(p) => Some(p.driver_id),
        N::VkPhysicalDeviceDriverProperties(p) => Some(p.driver_id),
        _ => None,
    });
    let core = &mut properties.properties;
    if core.vendor_id == NVIDIA_VENDOR_ID {
        core.vendor_id = VIRTIO_PCI_VENDOR_ID;
    }
    if driver_id == Some(DRIVER_ID_NVIDIA_PROPRIETARY)
        && core.driver_version >= NVIDIA_DMA_BUF_WSI_DRIVER
    {
        core.driver_version = NVIDIA_SOFTWARE_WSI_DRIVER;
    }
}

/// Whether Mesa 26.0.8's venus keeps its WSI on the software path for a
/// device shown with `properties` even when the renderer lists
/// `VK_EXT_external_memory_dma_buf`: an NVIDIA driver older than 590.48.01
/// (`vn_wsi.c:134-139`), which is every NVIDIA driver after
/// [`shape_identity`].
#[must_use]
pub fn keeps_software_wsi(properties: &VkPhysicalDeviceProperties2) -> bool {
    use crate::venus::protocol::VkPhysicalDeviceProperties2Next as N;
    let driver_id = properties.p_next.iter().find_map(|link| match link {
        N::VkPhysicalDeviceVulkan12Properties(p) => Some(p.driver_id),
        N::VkPhysicalDeviceDriverProperties(p) => Some(p.driver_id),
        _ => None,
    });
    driver_id == Some(DRIVER_ID_NVIDIA_PROPRIETARY)
        && properties.properties.driver_version < NVIDIA_DMA_BUF_WSI_DRIVER
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
    shape_identity(&mut properties);
    let mut features = info.features;
    mask_features(&mut features);
    let extensions = advertised_extensions_on(
        &info.extensions,
        properties.properties.api_version,
        keeps_software_wsi(&properties),
    );
    Ok(GuestDevice {
        properties,
        features,
        queue_families: info.queue_families,
        extensions,
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
// it is enabled, and the two a guest may enable here add none —
// `VK_KHR_synchronization2`'s values are all core 1.3, and
// `VK_KHR_external_semaphore_fd`'s one handle-type bit is core 1.1.

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
/// `VK_BUFFER_USAGE_CONDITIONAL_RENDERING_BIT_EXT`.
pub const BUFFER_USAGE_CONDITIONAL_RENDERING: u32 = 0x200;
/// `VK_BUFFER_USAGE_TRANSFORM_FEEDBACK_BUFFER_BIT_EXT`.
pub const BUFFER_USAGE_TRANSFORM_FEEDBACK: u32 = 0x800;
/// `VK_BUFFER_USAGE_TRANSFORM_FEEDBACK_COUNTER_BUFFER_BIT_EXT`.
pub const BUFFER_USAGE_TRANSFORM_FEEDBACK_COUNTER: u32 = 0x1000;

/// `VK_EXT_transform_feedback`.
pub const TRANSFORM_FEEDBACK: &str = "VK_EXT_transform_feedback";
/// `VK_EXT_conditional_rendering`.
pub const CONDITIONAL_RENDERING: &str = "VK_EXT_conditional_rendering";

/// The `VK_BUFFER_USAGE_*` bits the admitted extensions add, on a device for
/// which `enabled` names the extensions the guest enabled (stage 5c): a
/// transform feedback buffer and counter buffer, and a conditional
/// rendering predicate.
#[must_use]
pub fn buffer_usage_of_extensions(enabled: impl Fn(&str) -> bool) -> u32 {
    let mut bits = 0;
    if enabled(TRANSFORM_FEEDBACK) {
        bits |= BUFFER_USAGE_TRANSFORM_FEEDBACK | BUFFER_USAGE_TRANSFORM_FEEDBACK_COUNTER;
    }
    if enabled(CONDITIONAL_RENDERING) {
        bits |= BUFFER_USAGE_CONDITIONAL_RENDERING;
    }
    bits
}

/// `VK_QUERY_TYPE_TRANSFORM_FEEDBACK_STREAM_EXT`: two values a query.
pub const QUERY_TYPE_TRANSFORM_FEEDBACK_STREAM: i32 = 1_000_028_004;

/// `VK_BORDER_COLOR_FLOAT_CUSTOM_EXT` / `VK_BORDER_COLOR_INT_CUSTOM_EXT`.
#[must_use]
pub fn is_custom_border_color(value: i32) -> bool {
    matches!(value, 1_000_287_003 | 1_000_287_004)
}

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
        let out = advertised_extensions(&host, MAX_API_VERSION);
        let names: Vec<&[u8]> = out.iter().map(|e| c_name(&e.extension_name)).collect();
        assert_eq!(
            names,
            vec![
                EXTERNAL_SEMAPHORE_FD.as_bytes(),
                EXTERNAL_MEMORY_DMA_BUF.as_bytes(),
                EXTERNAL_MEMORY_FD.as_bytes(),
            ],
            "the protocol's own extension is no device's to pass through; the three \
             emulated ones are there whatever the host has"
        );
        assert!(out.iter().all(|e| e.spec_version == 1));
        assert!(has_extension(&host, EXTERNAL_MEMORY_HOST));
    }

    fn named(names: &[&str]) -> Vec<VkExtensionProperties> {
        names
            .iter()
            .map(|n| VkExtensionProperties {
                extension_name: name_array(n),
                spec_version: 1,
            })
            .collect()
    }

    fn advertised_names(host: &[VkExtensionProperties], api: u32) -> Vec<String> {
        let mut names: Vec<String> = advertised_extensions(host, api)
            .iter()
            .map(|e| String::from_utf8_lossy(c_name(&e.extension_name)).into_owned())
            .collect();
        names.sort();
        names
    }

    /// Stage 5c: a host that has every extension the renderer passes
    /// through is shown exactly those — every promoted one of a version the
    /// device has, every admitted one — and the three emulated ones; a host
    /// that lacks some is shown only what it has, plus the emulated ones.
    #[test]
    fn the_advertised_set_is_exactly_what_is_served_given_what_the_host_has() {
        let mut everything: Vec<&str> = PROMOTED_EXTENSIONS.iter().map(|(n, _)| *n).collect();
        everything.extend(
            ADMITTED_EXTENSIONS
                .iter()
                .filter(|n| !PROTOCOL_EXTENSIONS.contains(n)),
        );
        // What must never be passed through, whatever the host has.
        let refused = [
            "VK_KHR_swapchain",
            EXTERNAL_MEMORY_HOST,
            "VK_KHR_device_group",
            "VK_EXT_extended_dynamic_state",
            "VK_EXT_4444_formats",
            "VK_KHR_maintenance5",
            "VK_EXT_image_drm_format_modifier",
            "VK_EXT_queue_family_foreign",
            "VK_KHR_push_descriptor",
            "VK_EXT_calibrated_timestamps",
            "VK_KHR_external_memory_win32",
        ];
        let mut host_names = everything.clone();
        host_names.extend(refused);
        let host = named(&host_names);
        let mut want: Vec<String> = everything.iter().map(|n| (*n).to_owned()).collect();
        for (name, _) in EMULATED_EXTENSIONS {
            want.push((*name).to_owned());
        }
        want.sort();
        assert_eq!(advertised_names(&host, MAX_API_VERSION), want);
        // Zink's five required strings, and its GL 4.x ones, among them.
        for name in [
            "VK_KHR_maintenance1",
            "VK_KHR_maintenance2",
            "VK_KHR_maintenance3",
            "VK_KHR_create_renderpass2",
            "VK_KHR_imageless_framebuffer",
            "VK_KHR_dynamic_rendering",
            "VK_KHR_descriptor_update_template",
            "VK_KHR_draw_indirect_count",
            "VK_EXT_robustness2",
            "VK_EXT_transform_feedback",
            "VK_EXT_depth_clip_enable",
            "VK_EXT_vertex_attribute_divisor",
            "VK_EXT_custom_border_color",
            "VK_EXT_border_color_swizzle",
            "VK_EXT_line_rasterization",
            "VK_EXT_provoking_vertex",
            "VK_EXT_conditional_rendering",
            EXTERNAL_MEMORY_DMA_BUF,
        ] {
            assert!(want.iter().any(|w| w == name), "{name}");
        }

        // A host with only some of them: only those, and the emulated ones.
        let some = named(&[
            "VK_KHR_maintenance1",
            "VK_EXT_transform_feedback",
            "VK_KHR_swapchain",
        ]);
        assert_eq!(
            advertised_names(&some, MAX_API_VERSION),
            vec![
                EXTERNAL_MEMORY_DMA_BUF.to_owned(),
                "VK_EXT_transform_feedback".to_owned(),
                EXTERNAL_MEMORY_FD.to_owned(),
                EXTERNAL_SEMAPHORE_FD.to_owned(),
                "VK_KHR_maintenance1".to_owned(),
            ]
        );

        // A promoted extension waits for its version: on a 1.2 device the
        // 1.3 ones are not shown, the 1.1 and 1.2 ones are.
        let at_1_2 = advertised_names(&host, vk_make_api_version(0, 1, 2, 198));
        assert!(at_1_2.iter().any(|n| n == "VK_KHR_create_renderpass2"));
        assert!(at_1_2.iter().all(|n| n != "VK_KHR_dynamic_rendering"));
        assert!(at_1_2.iter().all(|n| n != SYNCHRONIZATION_2));
        let at_1_1 = advertised_names(&host, vk_make_api_version(0, 1, 1, 0));
        assert!(at_1_1.iter().any(|n| n == "VK_KHR_maintenance1"));
        assert!(at_1_1.iter().all(|n| n != "VK_KHR_draw_indirect_count"));
    }

    /// Every promoted extension advertised was promoted to the version
    /// listed, and every structure it adds is one the executor admits as
    /// core: the rule `PROMOTED_EXTENSIONS` was drawn up by.
    #[test]
    fn every_promoted_extension_advertised_brings_only_admitted_structures() {
        for (name, minor) in PROMOTED_EXTENSIONS {
            assert!(
                info::extension(name).is_some_and(|e| e.decodable),
                "{name} is a protocol extension"
            );
            assert!((1..=3).contains(minor), "{name}");
            // Its own structures (an interaction with another extension,
            // which that one brings, is that one's).
            for s in info::STRUCTURES.iter().filter(|s| s.extensions == [*name]) {
                assert!(
                    admits_link(s.stype),
                    "{name} brings {}, which is not admitted",
                    s.name
                );
            }
        }
        for name in ["VK_EXT_4444_formats", "VK_EXT_extended_dynamic_state"] {
            assert!(
                info::STRUCTURES
                    .iter()
                    .any(|s| s.extensions == [name] && !admits_link(s.stype)),
                "{name} keeps a structure the executor does not admit, which is why it is out"
            );
        }
    }

    /// Stage 5c: an NVIDIA device is shown with the virtio vendor, and a
    /// driver new enough for venus's dma-buf WSI below it; everything else
    /// — device id, name, driver id — is the host's.
    #[test]
    fn the_identity_is_the_virtio_vendor_and_a_software_wsi_nvidia_driver() {
        use crate::venus::protocol::{
            VkPhysicalDeviceProperties2Next as N, VkPhysicalDeviceVulkan12Properties,
        };
        let nvidia = |version: u32, driver_id: i32| {
            let mut p = VkPhysicalDeviceProperties2::default();
            p.properties.vendor_id = NVIDIA_VENDOR_ID;
            p.properties.device_id = 0x1f02;
            p.properties.driver_version = version;
            p.properties.device_name = name_array("NVIDIA GeForce RTX 2070");
            p.p_next = vec![N::VkPhysicalDeviceVulkan12Properties(
                VkPhysicalDeviceVulkan12Properties {
                    driver_id,
                    ..Default::default()
                },
            )];
            p
        };
        // NVIDIA's packing, as the RTX 2070's vulkaninfo prints 580.88.
        assert_eq!(nvidia_version(580, 88, 0, 0), 2_434_138_112);
        assert_eq!(
            NVIDIA_DMA_BUF_WSI_DRIVER,
            (590 << 22) | (48 << 14) | (1 << 6)
        );
        const _: () = assert!(NVIDIA_SOFTWARE_WSI_DRIVER < NVIDIA_DMA_BUF_WSI_DRIVER);

        // Today's driver: the vendor changes, the version does not.
        let mut today = nvidia(nvidia_version(580, 88, 0, 0), DRIVER_ID_NVIDIA_PROPRIETARY);
        shape_identity(&mut today);
        assert_eq!(today.properties.vendor_id, VIRTIO_PCI_VENDOR_ID);
        assert_eq!(today.properties.device_id, 0x1f02);
        assert_eq!(
            today.properties.driver_version,
            nvidia_version(580, 88, 0, 0)
        );
        assert_eq!(
            c_name(&today.properties.device_name),
            b"NVIDIA GeForce RTX 2070"
        );
        assert_eq!(
            today.p_next,
            nvidia(0, DRIVER_ID_NVIDIA_PROPRIETARY).p_next,
            "driverID stays the host's"
        );

        // The gate itself and anything newer: capped just below it.
        for version in [
            NVIDIA_DMA_BUF_WSI_DRIVER,
            nvidia_version(595, 10, 0, 0),
            u32::MAX,
        ] {
            let mut newer = nvidia(version, DRIVER_ID_NVIDIA_PROPRIETARY);
            shape_identity(&mut newer);
            assert_eq!(newer.properties.driver_version, NVIDIA_SOFTWARE_WSI_DRIVER);
        }
        // One below the gate is left alone.
        let mut below = nvidia(NVIDIA_DMA_BUF_WSI_DRIVER - 1, DRIVER_ID_NVIDIA_PROPRIETARY);
        shape_identity(&mut below);
        assert_eq!(
            below.properties.driver_version,
            NVIDIA_DMA_BUF_WSI_DRIVER - 1
        );

        // Another driver's version is never touched, and another vendor
        // keeps its id.
        let mut other = nvidia(u32::MAX, 1);
        other.properties.vendor_id = 0x1002;
        shape_identity(&mut other);
        assert_eq!(other.properties.vendor_id, 0x1002);
        assert_eq!(other.properties.driver_version, u32::MAX);

        // What the shaping buys: venus keeps the software WSI for every
        // NVIDIA driver, so the dma-buf pair can be advertised there; for
        // another driver it cannot, and is not.
        let mut newest = nvidia(u32::MAX, DRIVER_ID_NVIDIA_PROPRIETARY);
        shape_identity(&mut newest);
        assert!(keeps_software_wsi(&newest));
        assert!(keeps_software_wsi(&today));
        assert!(!keeps_software_wsi(&other));
        let host = [VkExtensionProperties {
            extension_name: name_array(TRANSFORM_FEEDBACK),
            spec_version: 1,
        }];
        let on_other = advertised_extensions_on(&host, MAX_API_VERSION, false);
        assert!(has_extension(&on_other, TRANSFORM_FEEDBACK));
        assert!(has_extension(&on_other, EXTERNAL_SEMAPHORE_FD));
        assert!(!has_extension(&on_other, EXTERNAL_MEMORY_DMA_BUF));
        assert!(!has_extension(&on_other, EXTERNAL_MEMORY_FD));
        let on_nvidia = advertised_extensions_on(&host, MAX_API_VERSION, true);
        assert!(has_extension(&on_nvidia, EXTERNAL_MEMORY_DMA_BUF));
    }

    /// Stage 5c: a `DMA_BUF` query answers exportable and importable
    /// exactly for a resource that can live in our pages.
    #[test]
    fn the_dma_buf_answer_is_our_pages_or_nothing() {
        let yes = external_memory_properties(true);
        assert_eq!(
            yes.external_memory_features,
            MEMORY_FEATURE_EXPORTABLE | MEMORY_FEATURE_IMPORTABLE
        );
        assert_eq!(yes.compatible_handle_types, MEMORY_HANDLE_DMA_BUF);
        assert_eq!(yes.export_from_imported_handle_types, MEMORY_HANDLE_DMA_BUF);
        assert_eq!(
            yes.external_memory_features & MEMORY_FEATURE_DEDICATED_ONLY,
            0
        );
        let no = external_memory_properties(false);
        assert_eq!(no.external_memory_features, 0);
        assert_eq!(no.export_from_imported_handle_types, 0);
        assert_eq!(no.compatible_handle_types, MEMORY_HANDLE_DMA_BUF);
        assert_eq!(buffer_usage_of_extensions(|_| false), 0);
        assert_eq!(
            buffer_usage_of_extensions(|e| e == TRANSFORM_FEEDBACK),
            BUFFER_USAGE_TRANSFORM_FEEDBACK | BUFFER_USAGE_TRANSFORM_FEEDBACK_COUNTER
        );
        assert_eq!(
            buffer_usage_of_extensions(|e| e == CONDITIONAL_RENDERING),
            BUFFER_USAGE_CONDITIONAL_RENDERING
        );
    }

    #[test]
    fn synchronization2_is_passed_through_only_on_a_vulkan_1_3_device() {
        let host = [
            VkExtensionProperties {
                extension_name: name_array(SYNCHRONIZATION_2),
                spec_version: 1,
            },
            VkExtensionProperties {
                extension_name: name_array(EXTERNAL_SEMAPHORE_FD),
                spec_version: 7,
            },
        ];
        let at_1_3 = advertised_extensions(&host, vk_make_api_version(0, 1, 3, 309));
        assert!(has_extension(&at_1_3, SYNCHRONIZATION_2));
        // The host's own sync-fd extension is not what is advertised: the
        // emulation's is, once, at its own spec version.
        let fd: Vec<_> = at_1_3
            .iter()
            .filter(|e| c_name(&e.extension_name) == EXTERNAL_SEMAPHORE_FD.as_bytes())
            .collect();
        assert_eq!(fd.len(), 1);
        assert_eq!(fd[0].spec_version, 1);
        let at_1_2 = advertised_extensions(&host, vk_make_api_version(0, 1, 2, 198));
        assert!(!has_extension(&at_1_2, SYNCHRONIZATION_2));
        assert!(has_extension(&at_1_2, EXTERNAL_SEMAPHORE_FD));
        assert!(is_emulated_extension(EXTERNAL_SEMAPHORE_FD));
        assert!(is_emulated_extension(EXTERNAL_MEMORY_DMA_BUF));
        assert!(is_emulated_extension(EXTERNAL_MEMORY_FD));
        assert!(!is_emulated_extension(SYNCHRONIZATION_2));
    }

    #[test]
    fn the_sync_fd_answer_is_importable_for_binary_only_and_other_types_are_the_hosts() {
        let host = || VkExternalSemaphoreProperties {
            export_from_imported_handle_types: 0x2,
            compatible_handle_types: 0x2,
            external_semaphore_features: 0x3,
        };
        let binary = external_semaphore_properties(SEMAPHORE_HANDLE_SYNC_FD, false, host);
        assert_eq!(
            binary.external_semaphore_features,
            SEMAPHORE_FEATURE_IMPORTABLE
        );
        assert_eq!(binary.compatible_handle_types, SEMAPHORE_HANDLE_SYNC_FD);
        assert_eq!(binary.export_from_imported_handle_types, 0);
        let timeline = external_semaphore_properties(SEMAPHORE_HANDLE_SYNC_FD, true, host);
        assert_eq!(timeline, VkExternalSemaphoreProperties::default());
        let opaque = external_semaphore_properties(0x2, false, host);
        assert_eq!(opaque, host());
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
        // Stage 5c: every admitted extension's bit, so the guest sends the
        // structures the executor now takes (its encoders drop the rest).
        for name in ADMITTED_EXTENSIONS {
            let number = info::extension(name).expect("a protocol extension").number;
            assert!(numbers.contains(&number), "{name} ({number})");
        }
        for (number, name) in [
            (288, "VK_EXT_custom_border_color"),
            (29, "VK_EXT_transform_feedback"),
            (82, "VK_EXT_conditional_rendering"),
            (287, "VK_EXT_robustness2"),
            (260, "VK_EXT_line_rasterization"),
            (535, "VK_KHR_line_rasterization"),
        ] {
            assert!(numbers.contains(&number), "{name} ({number})");
        }
        // Nothing whose structures the executor would refuse: not the
        // emulated extensions (they chain nothing the guest sends), not a
        // promoted one whose feature structure was not promoted.
        for (number, name) in [
            (126, "VK_EXT_external_memory_dma_buf"),
            (75, "VK_KHR_external_memory_fd"),
            (268, "VK_EXT_extended_dynamic_state"),
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
            let admitted = PROTOCOL_EXTENSIONS.contains(&ext.name)
                || info::STRUCTURES
                    .iter()
                    .any(|s| admits_link(s.stype) && s.extensions.contains(&ext.name));
            assert!(admitted, "{}", ext.name);
        }
        // 60 until stage 5b.3, and the twelve admitted device extensions.
        assert_eq!(numbers.len(), 72);
        let mask = admitted_extension_mask();
        assert!(mask.is_enumerated());
        let set: u32 = mask.words().iter().map(|w| w.count_ones()).sum();
        assert_eq!(set, 73, "72 extensions and the sentinel");
        for name in ADMITTED_EXTENSIONS {
            let number = info::extension(name).expect("known").number;
            assert!(mask.is_enabled(number), "{name}'s bit is in the capset");
        }
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
