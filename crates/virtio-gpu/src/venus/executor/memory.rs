//! Device memory, buffers, images' memory and views (EPIC 20 stage 5b.1):
//! what each of those commands does to a context, following
//! `vkr_device_memory.c`, `vkr_buffer.c` and `vkr_image.c` unless a function
//! says otherwise.
//!
//! # The memory model (ADR-0004, 2026-09-23)
//!
//! A memory type the guest sees as `HOST_VISIBLE` is one whose host type
//! accepts an import of our own pages ([`policy::guest_memory`]). An
//! allocation of it is **our pages** — a [`RingPages::for_memory`] allocation
//! charged to the renderer-wide [`PageBudget`], rounded to the driver's
//! `minImportedHostPointerAlignment` — imported with
//! `VK_EXT_external_memory_host`. The same `Arc` of those pages is kept by the
//! host memory object, by the table, and by the blob the guest later makes of
//! the memory (`RESOURCE_CREATE_BLOB` with `blob_id` = its id), so what the
//! guest maps and what the GPU reads and writes are one set of bytes. Every
//! other type is a plain `vkAllocateMemory`, and no blob can be made of it.
//!
//! This is where the renderer differs most from vkr, which allocates the
//! driver's own memory and exports it as a dma-buf or opaque fd for the VMM
//! to map. There is no fd to hand a WHP partition, and a mapping of the
//! driver's pages is one the VMM cannot keep alive or take back.
//!
//! # What a resource may be bound to
//!
//! Binding imported memory to a buffer or an image is only valid if the
//! resource was created for that handle type
//! (`VUID-vkBindBufferMemory-memory-02985`). vkr ignores this ("we still
//! violate the spec by binding external memory to non-external image or
//! buffer"); here every buffer and image is created with a
//! `VkExternalMemory*CreateInfo` for host allocations **when the driver says
//! it may import them for that resource**, and its `memoryTypeBits` name the
//! host-visible types only if it was. A linear staging buffer can therefore
//! live in memory the guest maps; an optimal-tiling image the driver will not
//! import for simply never sees a host-visible type. The same filtered bits
//! judge every bind.
//!
//! # External memory: `DMA_BUF`, emulated (stage 5c)
//!
//! A Windows host has no dma-buf, and the guest is told it has
//! (`VK_EXT_external_memory_dma_buf`, emulated, `policy`), because Mesa
//! 26.0.8's venus offers `VK_KHR_external_memory_fd` — which Zink's DRM
//! screen needs — only on such a renderer. What a `DMA_BUF` is here is what a
//! blob of this renderer already is: **our pages**. So:
//!
//! * a buffer or an image created for `DMA_BUF` is created as every resource
//!   is — for host allocations when the driver may import them for it — and
//!   one that can take our pages asks for them alone ([`external_type_bits`]),
//!   so memory allocated for it can be exported;
//! * an **export** allocation (`VkExportMemoryAllocateInfo{DMA_BUF}`) is an
//!   ordinary allocation: on a host-visible type it is our pages, and the
//!   blob Mesa makes of it at once (`vn_device_memory_alloc_export`) is those
//!   pages, as for any mapped memory; on any other type there are no pages to
//!   share, the blob is refused, and the guest's `vkAllocateMemory` answers
//!   `VK_ERROR_OUT_OF_DEVICE_MEMORY` (`virtgpu_bo_create_from_device_memory`)
//!   and frees the memory — refused in Vulkan terms;
//! * an **import** (`VkImportMemoryResourceInfoMESA`) must name a blob of
//!   memory of this renderer that this context made or is attached to
//!   ([`ContextBlobs::memory`](crate::venus::renderer::ContextBlobs::memory)),
//!   and becomes a new `VkDeviceMemory` importing **the same pages**
//!   (`VK_EXT_external_memory_host` again): both memories see one set of
//!   bytes. Its lifetime is the pages' `Arc`, which the import holds, so the
//!   exporter's memory, blob or whole context may go first and the pages stay
//!   exactly as long as something imported them; they are charged to the
//!   budget once, when they were first allocated. Anything else is
//!   `VK_ERROR_INVALID_EXTERNAL_HANDLE`, vkr's answer for a resource it
//!   cannot import.
//!
//! # Asynchrony
//!
//! Mesa 26.0.8 allocates memory, creates buffers on a requirements-cache hit,
//! binds and creates views **without asking for a reply**
//! (`vn_device_memory_alloc_simple`, `vn_buffer_init`, `vn_BindBufferMemory2`,
//! `vn_CreateImageView`). A `VkResult` those return reaches nobody: an
//! allocation the budget refused is simply absent, and the next command that
//! names it is fatal as an unknown id — exactly vkr's behaviour when its
//! driver refuses the same allocation.

use std::sync::Arc;

use crate::venus::protocol::{
    AllocateMemoryArgs, BindBufferMemory2Args, BindBufferMemoryArgs, BindImageMemory2Args,
    BindImageMemoryArgs, ChainLink, CreateBufferArgs, CreateBufferViewArgs, CreateImageViewArgs,
    DestroyBufferArgs, DestroyBufferViewArgs, DestroyImageViewArgs, FreeMemoryArgs,
    GetBufferDeviceAddressArgs, GetBufferMemoryRequirements2Args, GetBufferMemoryRequirementsArgs,
    GetDeviceBufferMemoryRequirementsArgs, GetDeviceImageMemoryRequirementsArgs,
    GetDeviceMemoryCommitmentArgs, GetImageMemoryRequirementsArgs, GetImageSubresourceLayoutArgs,
    VkBindBufferMemoryInfo, VkBindBufferMemoryInfoNext, VkBindImageMemoryInfo,
    VkBindImageMemoryInfoNext, VkBufferCreateInfo, VkBufferCreateInfoNext, VkImageCreateInfo,
    VkImageViewCreateInfoNext, VkMemoryAllocateInfoNext, VkMemoryRequirements2,
    VK_ERROR_INVALID_EXTERNAL_HANDLE, VK_ERROR_OUT_OF_DEVICE_MEMORY, VK_ERROR_OUT_OF_HOST_MEMORY,
    VK_ERROR_UNKNOWN, VK_SHARING_MODE_CONCURRENT, VK_SUCCESS,
};
use crate::venus::shmem::{RingPages, ShmemError};

#[cfg(doc)]
use crate::venus::shmem::PageBudget;

use super::context::{
    check_image_create_info, id_error, image_limits_hold, invalid, unimplemented_link, ExecError,
    VulkanContext,
};
use super::host::{Dedicated, HostVulkan, ImageBind, MemoryRequest};
use super::objects::{
    Binding, BufferObject, DedicatedTo, DeviceObject, ImageFacts, Kind, MemoryObject, ViewObject,
};
use super::policy::{self, GuestDevice};

/// `VK_WHOLE_SIZE`.
const WHOLE_SIZE: u64 = u64::MAX;
/// `VK_REMAINING_MIP_LEVELS` / `VK_REMAINING_ARRAY_LAYERS`.
const REMAINING: u32 = u32::MAX;

/// The `memoryTypeBits` a resource may be bound to, from what the host
/// reports: only types the guest sees, and — unless the resource was created
/// able to take our imported pages — none of the host-visible ones.
#[must_use]
pub fn guest_type_bits(guest: &GuestDevice, host_memory: bool, bits: u32) -> u32 {
    let bits = bits & guest.all_types();
    if host_memory {
        bits
    } else {
        bits & !guest.host_visible_types()
    }
}

/// [`guest_type_bits`] for a resource created for `DMA_BUF` export
/// (`external`, stage 5c): when it can take our pages, **only** the types
/// that are our pages, because an export of any other is refused — so what
/// the guest allocates for it is memory it can share. One that cannot take
/// our pages keeps its bits, and an export of its memory is refused.
#[must_use]
pub fn external_type_bits(
    guest: &GuestDevice,
    host_memory: bool,
    external: bool,
    bits: u32,
) -> u32 {
    let bits = guest_type_bits(guest, host_memory, bits);
    let ours = bits & guest.host_visible_types();
    if external && host_memory && ours != 0 {
        ours
    } else {
        bits
    }
}

/// The handle types a `VkExternalMemory{Buffer,Image}CreateInfo` may name on
/// `device`: none, or `DMA_BUF` once the guest enabled the emulated
/// dma-buf extension (Mesa rewrites every handle type an application names
/// to the renderer's, `vn_buffer.c:386-396`, `vn_image.c:687-692`). Answers
/// whether it names `DMA_BUF`.
///
/// # Errors
/// Any other handle type: no correct guest sends one, because no other is
/// the renderer's.
pub(super) fn external_handle_types(
    command: &'static str,
    handle_types: u32,
    device_enabled_dma_buf: bool,
) -> Result<bool, ExecError> {
    match handle_types {
        0 => Ok(false),
        policy::MEMORY_HANDLE_DMA_BUF if device_enabled_dma_buf => Ok(true),
        other => Err(invalid(
            command,
            format!(
                "external memory handle types {other:#x}: DMA_BUF is the only one, and only on a \
                 device that enabled VK_EXT_external_memory_dma_buf"
            ),
        )),
    }
}

impl<H: HostVulkan> DeviceObject<H> {
    /// Whether the guest enabled the emulated dma-buf external memory on
    /// this device (either of the two names Mesa enables together,
    /// `vn_device.c:318-330`).
    #[must_use]
    pub fn dma_buf(&self) -> bool {
        self.enabled(policy::EXTERNAL_MEMORY_DMA_BUF) || self.enabled(policy::EXTERNAL_MEMORY_FD)
    }
}

/// How many memory planes a format has (1 for everything but the
/// multi-planar YCbCr formats of 1.1 and 1.3).
#[must_use]
pub fn plane_count(format: i32) -> u32 {
    match format {
        1_000_156_002 | 1_000_156_004 | 1_000_156_006 | 1_000_156_012 | 1_000_156_014
        | 1_000_156_016 | 1_000_156_022 | 1_000_156_024 | 1_000_156_026 | 1_000_156_029
        | 1_000_156_031 | 1_000_156_033 => 3,
        1_000_156_003
        | 1_000_156_005
        | 1_000_156_013
        | 1_000_156_015
        | 1_000_156_023
        | 1_000_156_025
        | 1_000_156_030
        | 1_000_156_032
        | 1_000_330_000..=1_000_330_003 => 2,
        _ => 1,
    }
}

/// The plane index an `VK_IMAGE_ASPECT_PLANE_n_BIT` names.
fn plane_index(aspect: i32) -> Option<u32> {
    match aspect {
        0x10 => Some(0),
        0x20 => Some(1),
        0x40 => Some(2),
        _ => None,
    }
}

/// Whether `offset` is a multiple of `alignment` (0 counts as 1, as a driver
/// that reports it means "any").
fn aligned(offset: u64, alignment: u64) -> bool {
    alignment == 0 || offset % alignment == 0
}

fn requirements() -> VkMemoryRequirements2 {
    VkMemoryRequirements2::default()
}

impl<H: HostVulkan> VulkanContext<H> {
    // ---------------------------------------------------------- memory

    /// `vkAllocateMemory`. See the module docs for the two paths, and for
    /// the external memory of stage 5c.
    ///
    /// Answered in Vulkan terms, as vkr answers: a type index past the
    /// device's is `VK_ERROR_UNKNOWN`; a size past the heap, or past what is
    /// left of the renderer's host-visible budget, is
    /// `VK_ERROR_OUT_OF_DEVICE_MEMORY` (the answer a driver gives for an
    /// exhausted heap); a host allocator that refuses is
    /// `VK_ERROR_OUT_OF_HOST_MEMORY`; an export of a handle type other than
    /// the emulated `DMA_BUF`, or of `DMA_BUF` on a device that did not enable
    /// it, and an import (`VkImportMemoryResourceInfoMESA`) of anything but a
    /// blob of this renderer's memory this context may reach
    /// ([`Self::import_memory`]) is `VK_ERROR_INVALID_EXTERNAL_HANDLE` —
    /// vkr's answer for a resource it cannot import. Fatal: a zero size, a
    /// flag or feature the guest was not offered, a dedicated resource of
    /// another device or two of them.
    pub(super) fn allocate_memory(
        &mut self,
        args: &mut AllocateMemoryArgs,
    ) -> Result<(), ExecError> {
        const NAME: &str = "vkAllocateMemory";
        let device_id = args.device.0;
        let id = args.p_memory.map(|h| h.0).unwrap_or(0);
        let (device, guest) = self
            .objects
            .device_and_guest(device_id)
            .map_err(id_error(NAME))?;
        self.objects
            .check_new(id, Kind::DeviceMemory)
            .map_err(id_error(NAME))?;
        let Some(info) = &args.p_allocate_info else {
            return Err(invalid(NAME, "pAllocateInfo is null"));
        };
        let index = usize::try_from(info.memory_type_index).unwrap_or(usize::MAX);
        let count = usize::try_from(guest.memory.memory_type_count).unwrap_or(0);
        let Some(ty) = guest
            .memory
            .memory_types
            .get(index)
            .filter(|_| index < count)
        else {
            args.ret = VK_ERROR_UNKNOWN;
            return Ok(());
        };
        if info.allocation_size == 0 {
            return Err(invalid(NAME, "allocationSize is 0"));
        }
        let heap = usize::try_from(ty.heap_index)
            .ok()
            .and_then(|h| guest.memory.memory_heaps.get(h))
            .map_or(0, |h| h.size);
        if info.allocation_size > heap {
            args.ret = VK_ERROR_OUT_OF_DEVICE_MEMORY;
            return Ok(());
        }

        let mut flags = None;
        let mut dedicated = None;
        let mut import = None;
        for link in &info.p_next {
            match link {
                VkMemoryAllocateInfoNext::VkExportMemoryAllocateInfo(e) => {
                    // Mesa rewrites an export to the renderer's handle type
                    // (`vn_device_memory_fix_alloc_info`): `DMA_BUF`, the
                    // emulated one, on a device that enabled it. Nothing
                    // more to do for it here — see the module docs.
                    let ok = e.handle_types == 0
                        || (e.handle_types == policy::MEMORY_HANDLE_DMA_BUF && device.dma_buf());
                    if !ok {
                        args.ret = VK_ERROR_INVALID_EXTERNAL_HANDLE;
                        return Ok(());
                    }
                }
                VkMemoryAllocateInfoNext::VkMemoryAllocateFlagsInfo(f) => {
                    let known = policy::MEMORY_ALLOCATE_DEVICE_MASK
                        | policy::MEMORY_ALLOCATE_DEVICE_ADDRESS
                        | policy::MEMORY_ALLOCATE_CAPTURE_REPLAY;
                    if f.flags & !known != 0
                        || f.flags & policy::MEMORY_ALLOCATE_CAPTURE_REPLAY != 0
                    {
                        return Err(invalid(NAME, format!("allocate flags {:#x}", f.flags)));
                    }
                    if f.flags & policy::MEMORY_ALLOCATE_DEVICE_ADDRESS != 0
                        && !device.buffer_device_address
                    {
                        return Err(invalid(
                            NAME,
                            "DEVICE_ADDRESS without bufferDeviceAddress enabled",
                        ));
                    }
                    let all = 1u32
                        .checked_shl(device.group_size)
                        .map_or(u32::MAX, |b| b - 1);
                    if f.flags & policy::MEMORY_ALLOCATE_DEVICE_MASK != 0
                        && (f.device_mask == 0 || f.device_mask & !all != 0)
                    {
                        return Err(invalid(NAME, format!("device mask {:#x}", f.device_mask)));
                    }
                    flags = Some((f.flags, f.device_mask));
                }
                VkMemoryAllocateInfoNext::VkMemoryDedicatedAllocateInfo(d) => {
                    match (d.image.0, d.buffer.0) {
                        (0, 0) => {}
                        (image, 0) => {
                            let host = self
                                .objects
                                .image(device_id, image)
                                .map_err(id_error(NAME))?
                                .host;
                            dedicated = Some((DedicatedTo::Image(image), Dedicated::Image(host)));
                        }
                        (0, buffer) => {
                            let host = self
                                .objects
                                .buffer(device_id, buffer)
                                .map_err(id_error(NAME))?
                                .host;
                            dedicated =
                                Some((DedicatedTo::Buffer(buffer), Dedicated::Buffer(host)));
                        }
                        _ => {
                            return Err(invalid(
                                NAME,
                                "a dedicated allocation naming both an image and a buffer",
                            ))
                        }
                    }
                }
                VkMemoryAllocateInfoNext::VkMemoryOpaqueCaptureAddressAllocateInfo(c) => {
                    if c.opaque_capture_address != 0 {
                        return Err(invalid(
                            NAME,
                            "an opaque capture address, with capture replay not offered",
                        ));
                    }
                }
                VkMemoryAllocateInfoNext::VkImportMemoryResourceInfoMESA(r) => {
                    import = Some(r.resource_id);
                }
            }
        }
        if !self.objects.has_room() {
            args.ret = VK_ERROR_OUT_OF_HOST_MEMORY;
            return Ok(());
        }
        if let Some(resource_id) = import {
            let property_flags = ty.property_flags;
            return self.import_memory(args, id, resource_id, property_flags, flags);
        }

        let host_visible = ty.property_flags & policy::MEMORY_PROPERTY_HOST_VISIBLE != 0;
        let property_flags = ty.property_flags;
        let type_index = info.memory_type_index;
        let size = info.allocation_size;
        let (pages, request) = if host_visible {
            let pages = match RingPages::for_memory(size, guest.import_alignment, &self.budget) {
                Ok(pages) => Arc::new(pages),
                Err(error) => {
                    tracing::warn!(
                        ctx_id = self.ctx_id,
                        size,
                        %error,
                        "a host-visible allocation was refused"
                    );
                    args.ret = match error {
                        ShmemError::OutOfMemory { .. } => VK_ERROR_OUT_OF_HOST_MEMORY,
                        _ => VK_ERROR_OUT_OF_DEVICE_MEMORY,
                    };
                    return Ok(());
                }
            };
            let bits = match self.host.host_pointer_types(&device.host, &pages) {
                Ok(bits) => bits,
                Err(ret) => {
                    args.ret = ret;
                    return Ok(());
                }
            };
            if bits & 1u32.checked_shl(type_index).unwrap_or(0) == 0 {
                tracing::warn!(
                    ctx_id = self.ctx_id,
                    type_index,
                    importable = format_args!("{bits:#x}"),
                    "the host will not import these pages as the type the guest chose"
                );
                args.ret = VK_ERROR_OUT_OF_DEVICE_MEMORY;
                return Ok(());
            }
            let request = MemoryRequest {
                size: pages.mapped_len(),
                type_index,
                import: Some(Arc::clone(&pages)),
                flags,
                // A dedicated import is one more thing for a driver to
                // refuse, and dedication is a hint for everything the guest
                // could bind here; the table still records it for the bind
                // checks.
                dedicated: None,
            };
            (Some(pages), request)
        } else {
            // Rounded up to a blob page unless dedicated (see
            // `MemoryObject::host_size`); the heap check above was made on
            // the guest's size, and a page more cannot overflow it by more
            // than the rounding.
            let host_size = if dedicated.is_some() {
                Some(size)
            } else {
                size.checked_next_multiple_of(crate::blob::BLOB_PAGE_SIZE)
            };
            let Some(host_size) = host_size else {
                args.ret = VK_ERROR_OUT_OF_DEVICE_MEMORY;
                return Ok(());
            };
            let request = MemoryRequest {
                size: host_size,
                type_index,
                import: None,
                flags,
                dedicated: dedicated.map(|(_, host)| host),
            };
            (None, request)
        };
        let host_size = request.size;
        match self.host.allocate_memory(&device.host, &request) {
            Ok(memory) => {
                self.objects.insert_memory(
                    id,
                    MemoryObject {
                        device: device_id,
                        host: memory,
                        size,
                        host_size,
                        type_index,
                        property_flags,
                        pages,
                        exported: false,
                        dedicated: dedicated.map(|(to, _)| to),
                        allocate_flags: flags.map_or(0, |(f, _)| f),
                    },
                );
                args.ret = VK_SUCCESS;
            }
            Err(ret) => args.ret = ret,
        }
        Ok(())
    }

    /// `vkAllocateMemory` with a `VkImportMemoryResourceInfoMESA` (stage 5c):
    /// a new memory importing the pages of blob `resource_id` — a blob of
    /// memory of this renderer, this context's own or attached to it — as a
    /// host-visible type the host accepts those pages for, no larger than the
    /// blob. The import holds the pages' `Arc` (module docs); no blob can be
    /// made of it again, because the guest already has one. Anything else is
    /// `VK_ERROR_INVALID_EXTERNAL_HANDLE`, logged with why.
    fn import_memory(
        &mut self,
        args: &mut AllocateMemoryArgs,
        id: u64,
        resource_id: u32,
        property_flags: u32,
        flags: Option<(u32, u32)>,
    ) -> Result<(), ExecError> {
        const NAME: &str = "vkAllocateMemory";
        let device_id = args.device.0;
        let Some(info) = &args.p_allocate_info else {
            return Err(invalid(NAME, "pAllocateInfo is null"));
        };
        let (type_index, size) = (info.memory_type_index, info.allocation_size);
        let (device, guest) = self
            .objects
            .device_and_guest(device_id)
            .map_err(id_error(NAME))?;
        let refuse = |why: &str| {
            tracing::info!(
                ctx_id = self.ctx_id,
                resource = resource_id,
                why,
                "vkAllocateMemory import of a virtio-gpu resource refused"
            );
            VK_ERROR_INVALID_EXTERNAL_HANDLE
        };
        let blob = match (device.dma_buf(), self.blobs.as_ref()) {
            (false, _) => Err("the device did not enable VK_EXT_external_memory_dma_buf"),
            (true, None) => Err("the context has no blobs"),
            (true, Some(blobs)) => blobs
                .memory(resource_id)
                .map_err(|_| "not a blob of this renderer's memory this context may reach"),
        };
        let blob = match blob {
            Ok(blob) => blob,
            Err(why) => {
                args.ret = refuse(why);
                return Ok(());
            }
        };
        let pages = blob.pages;
        let align = guest.import_alignment.max(1);
        if property_flags & policy::MEMORY_PROPERTY_HOST_VISIBLE == 0 {
            args.ret = refuse("its type is not one of our pages");
            return Ok(());
        }
        if size > blob.size
            || size > pages.mapped_len()
            || pages.host_addr() % align != 0
            || pages.mapped_len() % align != 0
        {
            args.ret = refuse("its size or its pages do not fit the import");
            return Ok(());
        }
        let bits = match self.host.host_pointer_types(&device.host, &pages) {
            Ok(bits) => bits,
            Err(ret) => {
                args.ret = ret;
                return Ok(());
            }
        };
        if bits & 1u32.checked_shl(type_index).unwrap_or(0) == 0 {
            args.ret = refuse("the host will not import those pages as that type");
            return Ok(());
        }
        let request = MemoryRequest {
            size: pages.mapped_len(),
            type_index,
            import: Some(Arc::clone(&pages)),
            flags,
            dedicated: None,
        };
        match self.host.allocate_memory(&device.host, &request) {
            Ok(memory) => {
                tracing::debug!(
                    ctx_id = self.ctx_id,
                    resource = resource_id,
                    owner = blob.owner,
                    size,
                    "vkAllocateMemory imported a blob of memory: the same pages"
                );
                self.objects.insert_memory(
                    id,
                    MemoryObject {
                        device: device_id,
                        host: memory,
                        size,
                        host_size: request.size,
                        type_index,
                        property_flags,
                        pages: Some(pages),
                        exported: true,
                        dedicated: None,
                        allocate_flags: flags.map_or(0, |(f, _)| f),
                    },
                );
                args.ret = VK_SUCCESS;
            }
            Err(ret) => args.ret = ret,
        }
        Ok(())
    }

    /// `vkGetMemoryResourcePropertiesMESA` (stage 5c), which Mesa sends
    /// before importing a dma-buf (`vn_get_memory_dma_buf_properties`,
    /// `vn_device_memory.c:574-605`): for a blob of this renderer's memory
    /// this context may import ([`Self::import_memory`]'s rule), the types
    /// its pages may be imported as — the host-visible ones the host accepts
    /// them for — and its size; `VK_ERROR_INVALID_EXTERNAL_HANDLE` for
    /// anything else, as vkr answers a resource it cannot import. (vkr is
    /// fatal for a resource the context does not hold at all; here that is
    /// the same answer, because a guest cannot tell a resource that exists
    /// but is not ours from one that is not attached yet.)
    pub(super) fn memory_resource_properties(
        &mut self,
        args: &mut crate::venus::protocol::GetMemoryResourcePropertiesMESAArgs,
    ) -> Result<(), ExecError> {
        use crate::venus::protocol::VkMemoryResourcePropertiesMESANext as N;
        const NAME: &str = "vkGetMemoryResourcePropertiesMESA";
        let (device, guest) = self
            .objects
            .device_and_guest(args.device.0)
            .map_err(id_error(NAME))?;
        let blob = match (device.dma_buf(), self.blobs.as_ref()) {
            (true, Some(blobs)) => blobs.memory(args.resource_id).ok(),
            _ => None,
        };
        let Some(blob) = blob else {
            args.ret = VK_ERROR_INVALID_EXTERNAL_HANDLE;
            return Ok(());
        };
        let bits = match self.host.host_pointer_types(&device.host, &blob.pages) {
            Ok(bits) => bits & guest.host_visible_types(),
            Err(ret) => {
                args.ret = ret;
                return Ok(());
            }
        };
        if let Some(out) = args.p_memory_resource_properties.as_mut() {
            out.memory_type_bits = bits;
            for link in &mut out.p_next {
                let N::VkMemoryResourceAllocationSizePropertiesMESA(size) = link;
                size.allocation_size = blob.size;
            }
        }
        args.ret = VK_SUCCESS;
        Ok(())
    }

    /// `vkFreeMemory`; `VK_NULL_HANDLE` is a no-op. Resources bound to it
    /// stay (Vulkan allows it; they may no longer be used), and so do its
    /// pages if a blob of it is alive.
    pub(super) fn free_memory(&mut self, args: &FreeMemoryArgs) -> Result<(), ExecError> {
        const NAME: &str = "vkFreeMemory";
        self.objects.device(args.device.0).map_err(id_error(NAME))?;
        // Nothing the GPU may still be using is freed under it.
        self.settle(args.device.0);
        if let Some(memory) = self
            .objects
            .take_memory(args.device.0, args.memory.0)
            .map_err(id_error(NAME))?
        {
            let device = self.objects.device(args.device.0).map_err(id_error(NAME))?;
            self.host.free_memory(&device.host, memory.host);
        }
        Ok(())
    }

    /// `vkGetDeviceMemoryCommitment`: forwarded for a lazily allocated type,
    /// the only kind it is defined for; 0 for any other rather than invalid
    /// usage handed to the driver.
    pub(super) fn memory_commitment(
        &mut self,
        args: &mut GetDeviceMemoryCommitmentArgs,
    ) -> Result<(), ExecError> {
        const NAME: &str = "vkGetDeviceMemoryCommitment";
        let device = self.objects.device(args.device.0).map_err(id_error(NAME))?;
        let memory = self
            .objects
            .memory(args.device.0, args.memory.0)
            .map_err(id_error(NAME))?;
        let committed = if memory.property_flags & policy::MEMORY_PROPERTY_LAZILY_ALLOCATED != 0 {
            self.host.memory_commitment(&device.host, &memory.host)
        } else {
            0
        };
        args.p_committed_memory_in_bytes = Some(committed);
        Ok(())
    }

    /// A blob of memory `blob_id` (`RESOURCE_CREATE_BLOB` of this context):
    /// **the same pages** the memory is, if it is host-visible, the blob's
    /// size is the allocation's rounded to a 4 KiB page (what the guest
    /// kernel sends), and no blob was made of it before (vkr's "a memory can
    /// only be exported once").
    ///
    /// # Errors
    /// Why not, as a sentence for the log and the refusal.
    pub fn export_memory(&mut self, blob_id: u64, size: u64) -> Result<Arc<RingPages>, String> {
        if self.fatal {
            return Err(format!("venus context {} is fatal", self.ctx_id));
        }
        let memory = self
            .objects
            .memory_by_id_mut(blob_id)
            .map_err(|error| error.to_string())?;
        let Some(pages) = memory.pages.as_ref() else {
            return Err(format!(
                "memory {blob_id:#x} is of a type the guest cannot map (flags {:#x}), and only \
                 host-visible memory has pages to share",
                memory.property_flags
            ));
        };
        if memory.exported {
            return Err(format!("memory {blob_id:#x} already has a blob"));
        }
        let expected = memory
            .size
            .checked_next_multiple_of(crate::blob::BLOB_PAGE_SIZE)
            .unwrap_or(u64::MAX);
        if size != expected || size > pages.mapped_len() {
            return Err(format!(
                "a {size:#x}-byte blob of memory {blob_id:#x}, whose {:#x} bytes round to {expected:#x}",
                memory.size
            ));
        }
        memory.exported = true;
        Ok(Arc::clone(pages))
    }

    // --------------------------------------------------------- buffers

    /// The checks a `VkBufferCreateInfo` gets before the driver sees it.
    /// Answers whether it is created for `DMA_BUF` export (stage 5c).
    fn check_buffer_info(
        command: &'static str,
        info: &VkBufferCreateInfo,
        device: &DeviceObject<H>,
        guest: &GuestDevice,
    ) -> Result<bool, ExecError> {
        if info.flags & !policy::BUFFER_CREATE_CORE != 0
            || info.flags & policy::BUFFER_CREATE_SPARSE != 0
            || info.flags & policy::BUFFER_CREATE_CAPTURE_REPLAY != 0
        {
            return Err(invalid(command, format!("flags {:#x}", info.flags)));
        }
        if info.flags & policy::BUFFER_CREATE_PROTECTED != 0 && !guest.protected_memory() {
            return Err(invalid(
                command,
                "a protected buffer without protectedMemory",
            ));
        }
        if info.size == 0 {
            return Err(invalid(command, "size is 0"));
        }
        let usage_known = policy::BUFFER_USAGE_CORE
            | policy::BUFFER_USAGE_DEVICE_ADDRESS
            | policy::buffer_usage_of_extensions(|e| device.enabled(e));
        if info.usage == 0 || info.usage & !usage_known != 0 {
            return Err(invalid(command, format!("usage {:#x}", info.usage)));
        }
        if info.usage & policy::BUFFER_USAGE_DEVICE_ADDRESS != 0 && !device.buffer_device_address {
            return Err(invalid(
                command,
                "SHADER_DEVICE_ADDRESS usage without bufferDeviceAddress enabled",
            ));
        }
        if !policy::is_sharing_mode(info.sharing_mode) {
            return Err(invalid(command, "a sharing mode outside Vulkan 1.3"));
        }
        if info.sharing_mode == VK_SHARING_MODE_CONCURRENT {
            let families = info.p_queue_family_indices.as_deref().unwrap_or_default();
            if families.len() < 2
                || families
                    .iter()
                    .any(|f| usize::try_from(*f).map_or(true, |f| f >= guest.queue_families.len()))
            {
                return Err(invalid(
                    command,
                    "concurrent sharing needs two or more real families",
                ));
            }
        }
        let mut external = false;
        for link in &info.p_next {
            match link {
                VkBufferCreateInfoNext::VkExternalMemoryBufferCreateInfo(e) => {
                    external |= external_handle_types(command, e.handle_types, device.dma_buf())?;
                }
                VkBufferCreateInfoNext::VkBufferOpaqueCaptureAddressCreateInfo(c) => {
                    if c.opaque_capture_address != 0 {
                        return Err(invalid(
                            command,
                            "an opaque capture address, with capture replay not offered",
                        ));
                    }
                }
                other => {
                    return Err(unimplemented_link(
                        command,
                        "VkBufferCreateInfo",
                        ChainLink::structure_type(other),
                    ))
                }
            }
        }
        Ok(external)
    }

    /// `vkCreateBuffer`: checked, then created able to take our imported
    /// pages when the driver says it may.
    pub(super) fn create_buffer(&mut self, args: &mut CreateBufferArgs) -> Result<(), ExecError> {
        const NAME: &str = "vkCreateBuffer";
        let device_id = args.device.0;
        let id = args.p_buffer.map(|h| h.0).unwrap_or(0);
        let (device, guest) = self
            .objects
            .device_and_guest(device_id)
            .map_err(id_error(NAME))?;
        self.objects
            .check_new(id, Kind::Buffer)
            .map_err(id_error(NAME))?;
        let Some(info) = &args.p_create_info else {
            return Err(invalid(NAME, "pCreateInfo is null"));
        };
        let external = Self::check_buffer_info(NAME, info, device, guest)?;
        if !self.objects.has_room() {
            args.ret = VK_ERROR_OUT_OF_HOST_MEMORY;
            return Ok(());
        }
        let host_memory =
            self.host
                .buffer_accepts_host_memory(&device.host, info.flags, info.usage);
        match self.host.create_buffer(&device.host, info, host_memory) {
            Ok(buffer) => {
                self.objects.insert_buffer(
                    id,
                    BufferObject {
                        device: device_id,
                        host: buffer,
                        size: info.size,
                        usage: info.usage,
                        flags: info.flags,
                        host_memory,
                        external,
                        bound: None,
                    },
                );
                args.ret = VK_SUCCESS;
            }
            Err(ret) => args.ret = ret,
        }
        Ok(())
    }

    /// `vkDestroyBuffer`; `VK_NULL_HANDLE` is a no-op.
    pub(super) fn destroy_buffer(&mut self, args: &DestroyBufferArgs) -> Result<(), ExecError> {
        const NAME: &str = "vkDestroyBuffer";
        self.objects.device(args.device.0).map_err(id_error(NAME))?;
        // Nothing the GPU may still be using is freed under it.
        self.settle(args.device.0);
        if let Some(buffer) = self
            .objects
            .take_buffer(args.device.0, args.buffer.0)
            .map_err(id_error(NAME))?
        {
            let device = self.objects.device(args.device.0).map_err(id_error(NAME))?;
            self.host.destroy_buffer(&device.host, buffer);
        }
        Ok(())
    }

    /// A buffer's requirements from the host, `memoryTypeBits` filtered
    /// ([`guest_type_bits`]).
    fn buffer_requirements_of(
        &self,
        command: &'static str,
        device_id: u64,
        buffer_id: u64,
        out: &mut VkMemoryRequirements2,
    ) -> Result<(), ExecError> {
        let (device, guest) = self
            .objects
            .device_and_guest(device_id)
            .map_err(id_error(command))?;
        let buffer = self
            .objects
            .buffer(device_id, buffer_id)
            .map_err(id_error(command))?;
        self.host
            .buffer_memory_requirements(&device.host, buffer.host, out);
        out.memory_requirements.memory_type_bits = external_type_bits(
            guest,
            buffer.host_memory,
            buffer.external,
            out.memory_requirements.memory_type_bits,
        );
        Ok(())
    }

    /// `vkGetBufferMemoryRequirements`.
    pub(super) fn buffer_requirements(
        &mut self,
        args: &mut GetBufferMemoryRequirementsArgs,
    ) -> Result<(), ExecError> {
        let mut out = requirements();
        self.buffer_requirements_of(
            "vkGetBufferMemoryRequirements",
            args.device.0,
            args.buffer.0,
            &mut out,
        )?;
        if args.p_memory_requirements.is_some() {
            args.p_memory_requirements = Some(out.memory_requirements);
        }
        Ok(())
    }

    /// `vkGetBufferMemoryRequirements2` (what Mesa asks after every
    /// synchronous `vkCreateBuffer`, with `VkMemoryDedicatedRequirements`).
    pub(super) fn buffer_requirements2(
        &mut self,
        args: &mut GetBufferMemoryRequirements2Args,
    ) -> Result<(), ExecError> {
        const NAME: &str = "vkGetBufferMemoryRequirements2";
        let Some(info) = &args.p_info else {
            return Err(invalid(NAME, "pInfo is null"));
        };
        let buffer = info.buffer.0;
        // A null output is still judged, and stays null in the reply: the
        // guest's decoder writes wherever a present pointer says.
        let mut out = args.p_memory_requirements.take();
        let mut scratch = requirements();
        self.buffer_requirements_of(
            NAME,
            args.device.0,
            buffer,
            out.as_mut().unwrap_or(&mut scratch),
        )?;
        args.p_memory_requirements = out;
        Ok(())
    }

    /// `vkGetDeviceBufferMemoryRequirements`: the create info checked as
    /// `vkCreateBuffer` checks it, answered for the buffer that would be.
    pub(super) fn device_buffer_requirements(
        &mut self,
        args: &mut GetDeviceBufferMemoryRequirementsArgs,
    ) -> Result<(), ExecError> {
        const NAME: &str = "vkGetDeviceBufferMemoryRequirements";
        let (device, guest) = self
            .objects
            .device_and_guest(args.device.0)
            .map_err(id_error(NAME))?;
        let Some(info) = args.p_info.as_ref().and_then(|i| i.p_create_info.as_ref()) else {
            return Err(invalid(NAME, "pInfo or its pCreateInfo is null"));
        };
        require_1_3(NAME, guest)?;
        let external = Self::check_buffer_info(NAME, info, device, guest)?;
        let host_memory =
            self.host
                .buffer_accepts_host_memory(&device.host, info.flags, info.usage);
        if let Some(out) = args.p_memory_requirements.as_mut() {
            self.host
                .device_buffer_memory_requirements(&device.host, info, host_memory, out);
            out.memory_requirements.memory_type_bits = external_type_bits(
                guest,
                host_memory,
                external,
                out.memory_requirements.memory_type_bits,
            );
        }
        Ok(())
    }

    /// Judge one buffer bind, returning what the host is handed: the buffer
    /// of this device and unbound (and not bound earlier in the same call),
    /// the memory of this device, the offset aligned and the requirement
    /// inside the allocation, the memory's type among the buffer's filtered
    /// bits, a dedicated allocation bound only to its own buffer at 0, and
    /// `DEVICE_ADDRESS` memory for a device-address buffer.
    fn check_buffer_bind(
        &self,
        command: &'static str,
        device_id: u64,
        bind: &VkBindBufferMemoryInfo,
        earlier: &[u64],
    ) -> Result<(), ExecError> {
        let (device, guest) = self
            .objects
            .device_and_guest(device_id)
            .map_err(id_error(command))?;
        let buffer = self
            .objects
            .buffer(device_id, bind.buffer.0)
            .map_err(id_error(command))?;
        let memory = self
            .objects
            .memory(device_id, bind.memory.0)
            .map_err(id_error(command))?;
        if buffer.bound.is_some() || earlier.contains(&bind.buffer.0) {
            return Err(invalid(
                command,
                format!("buffer {:#x} is already bound", bind.buffer.0),
            ));
        }
        for link in &bind.p_next {
            match link {
                VkBindBufferMemoryInfoNext::VkBindBufferMemoryDeviceGroupInfo(g) => {
                    let indices = g.p_device_indices.as_deref().unwrap_or_default();
                    // Zero indices is the default; one index of 0 on a
                    // one-device device is the same thing spelled out.
                    if !(indices.is_empty() || (device.group_size == 1 && indices == [0])) {
                        return Err(invalid(command, "a device-group bind across devices"));
                    }
                }
                other => {
                    return Err(unimplemented_link(
                        command,
                        "VkBindBufferMemoryInfo",
                        ChainLink::structure_type(other),
                    ))
                }
            }
        }
        let mut req = requirements();
        self.host
            .buffer_memory_requirements(&device.host, buffer.host, &mut req);
        let req = req.memory_requirements;
        let bits = external_type_bits(
            guest,
            buffer.host_memory,
            buffer.external,
            req.memory_type_bits,
        );
        let fits = bind
            .memory_offset
            .checked_add(req.size)
            .is_some_and(|end| end <= memory.host_size);
        if !aligned(bind.memory_offset, req.alignment) || !fits {
            return Err(invalid(
                command,
                format!(
                    "{:#x} bytes at {:#x} (alignment {:#x}) do not fit the {:#x}-byte host allocation of a {:#x}-byte memory",
                    req.size, bind.memory_offset, req.alignment, memory.host_size, memory.size
                ),
            ));
        }
        if bits & 1u32.checked_shl(memory.type_index).unwrap_or(0) == 0 {
            return Err(invalid(
                command,
                format!(
                    "memory type {} is not among the buffer's {bits:#x}",
                    memory.type_index
                ),
            ));
        }
        match memory.dedicated {
            None => {}
            Some(DedicatedTo::Buffer(b)) if b == bind.buffer.0 && bind.memory_offset == 0 => {}
            Some(_) => {
                return Err(invalid(
                    command,
                    "a dedicated allocation bound to another resource",
                ))
            }
        }
        if buffer.usage & policy::BUFFER_USAGE_DEVICE_ADDRESS != 0
            && memory.allocate_flags & policy::MEMORY_ALLOCATE_DEVICE_ADDRESS == 0
        {
            return Err(invalid(
                command,
                "a device-address buffer bound to memory allocated without DEVICE_ADDRESS",
            ));
        }
        Ok(())
    }

    /// `vkBindBufferMemory2` (and v1, as one bind): every bind judged first,
    /// then one host call; the binds are recorded only if it succeeded.
    fn bind_buffers(
        &mut self,
        command: &'static str,
        device_id: u64,
        binds: &[VkBindBufferMemoryInfo],
    ) -> Result<i32, ExecError> {
        let mut seen = Vec::with_capacity(binds.len());
        for bind in binds {
            self.check_buffer_bind(command, device_id, bind, &seen)?;
            seen.push(bind.buffer.0);
        }
        let device = self.objects.device(device_id).map_err(id_error(command))?;
        let mut host_binds = Vec::with_capacity(binds.len());
        for bind in binds {
            let buffer = self
                .objects
                .buffer(device_id, bind.buffer.0)
                .map_err(id_error(command))?;
            let memory = self
                .objects
                .memory(device_id, bind.memory.0)
                .map_err(id_error(command))?;
            host_binds.push((buffer.host, &memory.host, bind.memory_offset));
        }
        let ret = self.host.bind_buffer_memory(&device.host, &host_binds);
        if ret == VK_SUCCESS {
            for bind in binds {
                let buffer = self
                    .objects
                    .buffer_mut(device_id, bind.buffer.0)
                    .map_err(id_error(command))?;
                buffer.bound = Some(Binding {
                    memory: bind.memory.0,
                    offset: bind.memory_offset,
                });
            }
        }
        Ok(ret)
    }

    /// `vkBindBufferMemory`.
    pub(super) fn bind_buffer_memory(
        &mut self,
        args: &mut BindBufferMemoryArgs,
    ) -> Result<(), ExecError> {
        let bind = VkBindBufferMemoryInfo {
            p_next: Vec::new(),
            buffer: args.buffer,
            memory: args.memory,
            memory_offset: args.memory_offset,
        };
        args.ret = self.bind_buffers("vkBindBufferMemory", args.device.0, &[bind])?;
        Ok(())
    }

    /// `vkBindBufferMemory2`.
    pub(super) fn bind_buffer_memory2(
        &mut self,
        args: &mut BindBufferMemory2Args,
    ) -> Result<(), ExecError> {
        const NAME: &str = "vkBindBufferMemory2";
        let binds = args.p_bind_infos.as_deref().unwrap_or_default();
        if binds.is_empty() {
            return Err(invalid(NAME, "no bind infos"));
        }
        args.ret = self.bind_buffers(NAME, args.device.0, binds)?;
        Ok(())
    }

    /// `vkGetBufferDeviceAddress`: a bound buffer with device-address usage
    /// on a device that enabled `bufferDeviceAddress`.
    pub(super) fn buffer_device_address(
        &mut self,
        args: &mut GetBufferDeviceAddressArgs,
    ) -> Result<(), ExecError> {
        const NAME: &str = "vkGetBufferDeviceAddress";
        let device = self.objects.device(args.device.0).map_err(id_error(NAME))?;
        let Some(info) = &args.p_info else {
            return Err(invalid(NAME, "pInfo is null"));
        };
        let buffer = self
            .objects
            .buffer(args.device.0, info.buffer.0)
            .map_err(id_error(NAME))?;
        if !device.buffer_device_address
            || buffer.usage & policy::BUFFER_USAGE_DEVICE_ADDRESS == 0
            || buffer.bound.is_none()
        {
            return Err(invalid(
                NAME,
                "a buffer that is unbound, or not created for device addresses on a device that \
                 enabled them",
            ));
        }
        args.ret = self.host.buffer_device_address(&device.host, buffer.host);
        Ok(())
    }

    /// `vkCreateBufferView` of a bound texel buffer, its range inside the
    /// buffer and its offset aligned to `minTexelBufferOffsetAlignment`.
    pub(super) fn create_buffer_view(
        &mut self,
        args: &mut CreateBufferViewArgs,
    ) -> Result<(), ExecError> {
        const NAME: &str = "vkCreateBufferView";
        let device_id = args.device.0;
        let id = args.p_view.map(|h| h.0).unwrap_or(0);
        let (device, guest) = self
            .objects
            .device_and_guest(device_id)
            .map_err(id_error(NAME))?;
        self.objects
            .check_new(id, Kind::BufferView)
            .map_err(id_error(NAME))?;
        let Some(info) = &args.p_create_info else {
            return Err(invalid(NAME, "pCreateInfo is null"));
        };
        // Its one link (`VkBufferUsageFlags2CreateInfo`) is 1.4's, refused
        // by the chain policy before a command is dispatched; kept here so
        // no path hands it to the driver.
        if let Some(link) = info.p_next.first() {
            return Err(unimplemented_link(
                NAME,
                "VkBufferViewCreateInfo",
                ChainLink::structure_type(link),
            ));
        }
        let buffer = self
            .objects
            .buffer(device_id, info.buffer.0)
            .map_err(id_error(NAME))?;
        let limit = guest
            .properties
            .properties
            .limits
            .min_texel_buffer_offset_alignment;
        let in_range = info.offset < buffer.size
            && (info.range == WHOLE_SIZE
                || (info.range != 0
                    && info
                        .offset
                        .checked_add(info.range)
                        .is_some_and(|end| end <= buffer.size)));
        if info.flags != 0
            || info.format == 0
            || !policy::is_core_format(info.format)
            || buffer.usage & policy::BUFFER_USAGE_TEXEL == 0
            || buffer.bound.is_none()
            || !in_range
            || !aligned(info.offset, limit)
        {
            return Err(invalid(
                NAME,
                "a view of an unbound or non-texel buffer, or one outside it, misaligned or of \
                 a format outside Vulkan 1.3",
            ));
        }
        if !self.objects.has_room() {
            args.ret = VK_ERROR_OUT_OF_HOST_MEMORY;
            return Ok(());
        }
        match self
            .host
            .create_buffer_view(&device.host, buffer.host, info)
        {
            Ok(view) => {
                self.objects.insert_buffer_view(
                    id,
                    ViewObject {
                        device: device_id,
                        parent: info.buffer.0,
                        host: view,
                    },
                );
                args.ret = VK_SUCCESS;
            }
            Err(ret) => args.ret = ret,
        }
        Ok(())
    }

    /// `vkDestroyBufferView`; `VK_NULL_HANDLE` is a no-op.
    pub(super) fn destroy_buffer_view(
        &mut self,
        args: &DestroyBufferViewArgs,
    ) -> Result<(), ExecError> {
        const NAME: &str = "vkDestroyBufferView";
        self.objects.device(args.device.0).map_err(id_error(NAME))?;
        // Nothing the GPU may still be using is freed under it.
        self.settle(args.device.0);
        if let Some(view) = self
            .objects
            .take_buffer_view(args.device.0, args.buffer_view.0)
            .map_err(id_error(NAME))?
        {
            let device = self.objects.device(args.device.0).map_err(id_error(NAME))?;
            self.host.destroy_buffer_view(&device.host, view);
        }
        Ok(())
    }

    // ---------------------------------------------------------- images

    /// `vkGetImageMemoryRequirements` (v1): not for a disjoint image.
    pub(super) fn image_requirements(
        &mut self,
        args: &mut GetImageMemoryRequirementsArgs,
    ) -> Result<(), ExecError> {
        const NAME: &str = "vkGetImageMemoryRequirements";
        let (device, guest) = self
            .objects
            .device_and_guest(args.device.0)
            .map_err(id_error(NAME))?;
        let image = self
            .objects
            .image(args.device.0, args.image.0)
            .map_err(id_error(NAME))?;
        if image.facts.flags & policy::IMAGE_CREATE_DISJOINT != 0 {
            return Err(invalid(
                NAME,
                "a disjoint image needs the plane-aware query",
            ));
        }
        let mut out = requirements();
        self.host
            .image_memory_requirements(&device.host, image.host, None, &mut out);
        let mut req = out.memory_requirements;
        req.memory_type_bits = external_type_bits(
            guest,
            image.host_memory,
            image.external,
            req.memory_type_bits,
        );
        if args.p_memory_requirements.is_some() {
            args.p_memory_requirements = Some(req);
        }
        Ok(())
    }

    /// `vkGetDeviceImageMemoryRequirements`: the create info checked as
    /// `vkCreateImage` checks it — its limits against the host's included —
    /// and answered for the image that would be.
    pub(super) fn device_image_requirements(
        &mut self,
        args: &mut GetDeviceImageMemoryRequirementsArgs<'_>,
    ) -> Result<(), ExecError> {
        const NAME: &str = "vkGetDeviceImageMemoryRequirements";
        let device_id = args.device.0;
        let Some(info) = &args.p_info else {
            return Err(invalid(NAME, "pInfo is null"));
        };
        let Some(create) = &info.p_create_info else {
            return Err(invalid(NAME, "pCreateInfo is null"));
        };
        let device = self.objects.device(device_id).map_err(id_error(NAME))?;
        let (instance, exposed) = self
            .objects
            .physical(device.physical)
            .map_err(id_error(NAME))?;
        require_1_3(NAME, &exposed.guest)?;
        let external = check_image_create_info(NAME, create, &exposed.guest, device.dma_buf())?;
        if !image_limits_hold(&*self.host, instance, exposed.host, create) {
            return Err(invalid(
                NAME,
                "the image is outside what the host reports for its format, type, tiling, usage and flags",
            ));
        }
        let planes = image_planes(NAME, create)?;
        let plane = if create.flags & policy::IMAGE_CREATE_DISJOINT != 0 {
            match plane_index(info.plane_aspect) {
                Some(p) if p < planes => Some(info.plane_aspect),
                _ => {
                    return Err(invalid(
                        NAME,
                        format!("plane aspect {:#x}", info.plane_aspect),
                    ))
                }
            }
        } else {
            None
        };
        let host_memory = self.host.image_accepts_host_memory(&device.host, create);
        if let Some(out) = args.p_memory_requirements.as_mut() {
            self.host.device_image_memory_requirements(
                &device.host,
                create,
                host_memory,
                plane,
                out,
            );
            out.memory_requirements.memory_type_bits = external_type_bits(
                &exposed.guest,
                host_memory,
                external,
                out.memory_requirements.memory_type_bits,
            );
        }
        Ok(())
    }

    /// Judge one image bind, as [`Self::check_buffer_bind`] judges a buffer
    /// one, per memory plane; answers the plane index it binds.
    fn check_image_bind(
        &self,
        command: &'static str,
        device_id: u64,
        bind: &VkBindImageMemoryInfo,
        earlier: &[(u64, u32)],
    ) -> Result<(u32, Option<i32>), ExecError> {
        let (device, guest) = self
            .objects
            .device_and_guest(device_id)
            .map_err(id_error(command))?;
        let image = self
            .objects
            .image(device_id, bind.image.0)
            .map_err(id_error(command))?;
        let memory = self
            .objects
            .memory(device_id, bind.memory.0)
            .map_err(id_error(command))?;
        let disjoint = image.facts.flags & policy::IMAGE_CREATE_DISJOINT != 0;
        let mut plane = None;
        for link in &bind.p_next {
            match link {
                VkBindImageMemoryInfoNext::VkBindImagePlaneMemoryInfo(p) => {
                    plane = Some(p.plane_aspect);
                }
                VkBindImageMemoryInfoNext::VkBindImageMemoryDeviceGroupInfo(g) => {
                    let indices = g.p_device_indices.as_deref().unwrap_or_default();
                    let regions = g
                        .p_split_instance_bind_regions
                        .as_deref()
                        .unwrap_or_default();
                    if !regions.is_empty()
                        || !(indices.is_empty() || (device.group_size == 1 && indices == [0]))
                    {
                        return Err(invalid(command, "a device-group bind across devices"));
                    }
                }
                other => {
                    return Err(unimplemented_link(
                        command,
                        "VkBindImageMemoryInfo",
                        ChainLink::structure_type(other),
                    ))
                }
            }
        }
        let index = match (disjoint, plane) {
            (false, None) => 0,
            (true, Some(aspect)) => match plane_index(aspect) {
                Some(p) if p < image.facts.planes => p,
                _ => return Err(invalid(command, format!("plane aspect {aspect:#x}"))),
            },
            _ => {
                return Err(invalid(
                    command,
                    "a plane bind names a plane exactly when the image is disjoint",
                ))
            }
        };
        let bit = 1u32 << index;
        if image.bound_planes & bit != 0 || earlier.contains(&(bind.image.0, index)) {
            return Err(invalid(
                command,
                format!("image {:#x} plane {index} is already bound", bind.image.0),
            ));
        }
        let mut req = requirements();
        self.host
            .image_memory_requirements(&device.host, image.host, plane, &mut req);
        let req = req.memory_requirements;
        let bits = external_type_bits(
            guest,
            image.host_memory,
            image.external,
            req.memory_type_bits,
        );
        let fits = bind
            .memory_offset
            .checked_add(req.size)
            .is_some_and(|end| end <= memory.host_size);
        if !aligned(bind.memory_offset, req.alignment) || !fits {
            return Err(invalid(
                command,
                format!(
                    "{:#x} bytes at {:#x} (alignment {:#x}) do not fit the {:#x}-byte host allocation of a {:#x}-byte memory",
                    req.size, bind.memory_offset, req.alignment, memory.host_size, memory.size
                ),
            ));
        }
        if bits & 1u32.checked_shl(memory.type_index).unwrap_or(0) == 0 {
            return Err(invalid(
                command,
                format!(
                    "memory type {} is not among the image's {bits:#x}",
                    memory.type_index
                ),
            ));
        }
        match memory.dedicated {
            None => {}
            Some(DedicatedTo::Image(i)) if i == bind.image.0 && bind.memory_offset == 0 => {}
            Some(_) => {
                return Err(invalid(
                    command,
                    "a dedicated allocation bound to another resource",
                ))
            }
        }
        Ok((index, plane))
    }

    /// `vkBindImageMemory2` (and v1, as one bind).
    fn bind_images(
        &mut self,
        command: &'static str,
        device_id: u64,
        binds: &[VkBindImageMemoryInfo],
    ) -> Result<i32, ExecError> {
        let mut seen = Vec::with_capacity(binds.len());
        let mut planes = Vec::with_capacity(binds.len());
        for bind in binds {
            let (index, plane) = self.check_image_bind(command, device_id, bind, &seen)?;
            seen.push((bind.image.0, index));
            planes.push(plane);
        }
        let device = self.objects.device(device_id).map_err(id_error(command))?;
        let mut host_binds = Vec::with_capacity(binds.len());
        for (bind, plane) in binds.iter().zip(&planes) {
            let image = self
                .objects
                .image(device_id, bind.image.0)
                .map_err(id_error(command))?;
            let memory = self
                .objects
                .memory(device_id, bind.memory.0)
                .map_err(id_error(command))?;
            host_binds.push(ImageBind {
                image: image.host,
                memory: &memory.host,
                offset: bind.memory_offset,
                plane: *plane,
            });
        }
        let ret = self.host.bind_image_memory(&device.host, &host_binds);
        if ret == VK_SUCCESS {
            for (image_id, index) in seen {
                let image = self
                    .objects
                    .image_mut(device_id, image_id)
                    .map_err(id_error(command))?;
                image.bound_planes |= 1 << index;
            }
        }
        Ok(ret)
    }

    /// `vkBindImageMemory`.
    pub(super) fn bind_image_memory(
        &mut self,
        args: &mut BindImageMemoryArgs,
    ) -> Result<(), ExecError> {
        let bind = VkBindImageMemoryInfo {
            p_next: Vec::new(),
            image: args.image,
            memory: args.memory,
            memory_offset: args.memory_offset,
        };
        args.ret = self.bind_images("vkBindImageMemory", args.device.0, &[bind])?;
        Ok(())
    }

    /// `vkBindImageMemory2`. A bind with no memory is Mesa's WSI path
    /// (`vn_image_bind_wsi_memory`), which needs a swapchain this renderer
    /// does not offer: it is an unknown id like any other.
    pub(super) fn bind_image_memory2(
        &mut self,
        args: &mut BindImageMemory2Args,
    ) -> Result<(), ExecError> {
        const NAME: &str = "vkBindImageMemory2";
        let binds = args.p_bind_infos.as_deref().unwrap_or_default();
        if binds.is_empty() {
            return Err(invalid(NAME, "no bind infos"));
        }
        args.ret = self.bind_images(NAME, args.device.0, binds)?;
        Ok(())
    }

    /// `vkGetImageSubresourceLayout`: a linear image, one aspect, a mip level
    /// and a layer it has.
    pub(super) fn image_subresource_layout(
        &mut self,
        args: &mut GetImageSubresourceLayoutArgs,
    ) -> Result<(), ExecError> {
        const NAME: &str = "vkGetImageSubresourceLayout";
        let device = self.objects.device(args.device.0).map_err(id_error(NAME))?;
        let image = self
            .objects
            .image(args.device.0, args.image.0)
            .map_err(id_error(NAME))?;
        let Some(sub) = &args.p_subresource else {
            return Err(invalid(NAME, "pSubresource is null"));
        };
        if image.facts.tiling != policy::IMAGE_TILING_LINEAR
            || !sub.aspect_mask.is_power_of_two()
            || sub.aspect_mask & !policy::IMAGE_ASPECT_VIEW != 0
            || sub.mip_level >= image.facts.mip_levels
            || sub.array_layer >= image.facts.array_layers
        {
            return Err(invalid(
                NAME,
                "a subresource of an optimal image, of several aspects, or past the image",
            ));
        }
        let layout = self
            .host
            .image_subresource_layout(&device.host, image.host, sub);
        if args.p_layout.is_some() {
            args.p_layout = Some(layout);
        }
        Ok(())
    }

    /// `vkCreateImageView` of a fully bound image: the view type one the
    /// image's type and flags allow, the format the image's unless it is
    /// `MUTABLE_FORMAT`, the swizzles and the subresource range inside
    /// Vulkan 1.3 and inside the image.
    pub(super) fn create_image_view(
        &mut self,
        args: &mut CreateImageViewArgs,
    ) -> Result<(), ExecError> {
        const NAME: &str = "vkCreateImageView";
        let device_id = args.device.0;
        let id = args.p_view.map(|h| h.0).unwrap_or(0);
        let device = self.objects.device(device_id).map_err(id_error(NAME))?;
        self.objects
            .check_new(id, Kind::ImageView)
            .map_err(id_error(NAME))?;
        let Some(info) = &args.p_create_info else {
            return Err(invalid(NAME, "pCreateInfo is null"));
        };
        let image = self
            .objects
            .image(device_id, info.image.0)
            .map_err(id_error(NAME))?;
        for link in &info.p_next {
            match link {
                VkImageViewCreateInfoNext::VkImageViewUsageCreateInfo(u) => {
                    if u.usage == 0
                        || u.usage & !policy::IMAGE_USAGE_CORE != 0
                        || u.usage & !image.facts.usage != 0
                    {
                        return Err(invalid(NAME, format!("view usage {:#x}", u.usage)));
                    }
                }
                other => {
                    return Err(unimplemented_link(
                        NAME,
                        "VkImageViewCreateInfo",
                        ChainLink::structure_type(other),
                    ))
                }
            }
        }
        check_view(NAME, info, &image.facts)?;
        if !image.fully_bound() {
            return Err(invalid(NAME, "a view of an image that is not bound"));
        }
        if !self.objects.has_room() {
            args.ret = VK_ERROR_OUT_OF_HOST_MEMORY;
            return Ok(());
        }
        match self.host.create_image_view(&device.host, image.host, info) {
            Ok(view) => {
                self.objects.insert_image_view(
                    id,
                    ViewObject {
                        device: device_id,
                        parent: info.image.0,
                        host: view,
                    },
                );
                args.ret = VK_SUCCESS;
            }
            Err(ret) => args.ret = ret,
        }
        Ok(())
    }

    /// `vkDestroyImageView`; `VK_NULL_HANDLE` is a no-op.
    pub(super) fn destroy_image_view(
        &mut self,
        args: &DestroyImageViewArgs,
    ) -> Result<(), ExecError> {
        const NAME: &str = "vkDestroyImageView";
        self.objects.device(args.device.0).map_err(id_error(NAME))?;
        // Nothing the GPU may still be using is freed under it.
        self.settle(args.device.0);
        if let Some(view) = self
            .objects
            .take_image_view(args.device.0, args.image_view.0)
            .map_err(id_error(NAME))?
        {
            let device = self.objects.device(args.device.0).map_err(id_error(NAME))?;
            self.host.destroy_image_view(&device.host, view);
        }
        Ok(())
    }
}

/// A command Vulkan 1.3 made core (`vkGetDevice*MemoryRequirements`) is
/// refused on a device that reports less: its host entry point may not
/// exist, and `ash` answers a missing one with a panic.
fn require_1_3(command: &'static str, guest: &GuestDevice) -> Result<(), ExecError> {
    if guest.api_version() < policy::MAX_API_VERSION {
        return Err(invalid(
            command,
            "a Vulkan 1.3 command on a device that reports an older version",
        ));
    }
    Ok(())
}

/// The memory planes an image of `info` has: 1, or its format's plane count
/// if it is `DISJOINT` (which a single-plane format may not be).
///
/// # Errors
/// A disjoint image of a single-plane format.
pub(super) fn image_planes(
    command: &'static str,
    info: &VkImageCreateInfo,
) -> Result<u32, ExecError> {
    if info.flags & policy::IMAGE_CREATE_DISJOINT == 0 {
        return Ok(1);
    }
    match plane_count(info.format) {
        1 => Err(invalid(command, "DISJOINT on a single-plane format")),
        n => Ok(n),
    }
}

/// What the table keeps of an image's create info.
pub(super) fn image_facts(info: &VkImageCreateInfo, planes: u32) -> ImageFacts {
    ImageFacts {
        flags: info.flags,
        image_type: info.image_type,
        format: info.format,
        mip_levels: info.mip_levels,
        array_layers: info.array_layers,
        tiling: info.tiling,
        usage: info.usage,
        planes,
        depth: info.extent.depth,
    }
}

/// The checks a `VkImageViewCreateInfo` gets against its image.
fn check_view(
    command: &'static str,
    info: &crate::venus::protocol::VkImageViewCreateInfo,
    image: &ImageFacts,
) -> Result<(), ExecError> {
    let c = &info.components;
    if info.flags != 0
        || !policy::is_image_view_type(info.view_type)
        || info.format == 0
        || !policy::is_core_format(info.format)
        || ![c.r, c.g, c.b, c.a]
            .iter()
            .all(|s| policy::is_component_swizzle(*s))
    {
        return Err(invalid(
            command,
            "flags, view type, format or swizzle outside Vulkan 1.3",
        ));
    }
    if image.flags & policy::IMAGE_CREATE_MUTABLE_FORMAT == 0 && info.format != image.format {
        return Err(invalid(
            command,
            "a view format that differs from an image created without MUTABLE_FORMAT",
        ));
    }
    // VkImageViewType: 1D, 2D, 3D, CUBE, 1D_ARRAY, 2D_ARRAY, CUBE_ARRAY.
    let cube = image.flags & policy::IMAGE_CREATE_CUBE_COMPATIBLE != 0;
    let array2d = image.flags & policy::IMAGE_CREATE_2D_ARRAY_COMPATIBLE != 0;
    let allowed = match (image.image_type, info.view_type) {
        (0, 0 | 4) => true,
        (1, 1 | 5) => true,
        (1, 3 | 6) => cube,
        (2, 2) => true,
        (2, 1 | 5) => array2d,
        _ => false,
    };
    if !allowed {
        return Err(invalid(
            command,
            format!(
                "a view of type {} of an image of type {}",
                info.view_type, image.image_type
            ),
        ));
    }
    let r = &info.subresource_range;
    let levels_ok = r.base_mip_level < image.mip_levels
        && (r.level_count == REMAINING
            || (r.level_count != 0
                && r.base_mip_level
                    .checked_add(r.level_count)
                    .is_some_and(|end| end <= image.mip_levels)));
    // A 2D view of a 3D image (2D_ARRAY_COMPATIBLE) addresses its depth
    // slices as layers; any other view addresses the image's layers.
    let layers = if image.image_type == 2 && info.view_type != 2 {
        image.depth
    } else {
        image.array_layers
    };
    let layers_ok = r.base_array_layer < layers
        && (r.layer_count == REMAINING
            || (r.layer_count != 0
                && r.base_array_layer
                    .checked_add(r.layer_count)
                    .is_some_and(|end| end <= layers)));
    if r.aspect_mask == 0
        || r.aspect_mask & !policy::IMAGE_ASPECT_VIEW != 0
        || !levels_ok
        || !layers_ok
    {
        return Err(invalid(
            command,
            "a subresource range with no aspect, an aspect outside Vulkan 1.3, or past the image",
        ));
    }
    Ok(())
}
