//! A disk representation of one released canonical shared image. Only pixels
//! and allocation/create metadata cross the file boundary; native handles do not.
//! This is a payload component, not a renderer/VM checkpoint. The caller must
//! drain all owners and prove that this is the payload's active canonical image.

use super::super::checkpoint_ownership::SharedImageState;
use std::io::Write;

use super::*;
use crate::venus::executor::limits::ContextLimits;
use crate::venus::protocol::CmdCopyBufferToImageArgs;
use crate::venus::renderer::SharedHandle;
use crate::venus::wire::Decoder;

const VERSION: u32 = 1;
// Version 2 is exclusively an Initial image: no pixel bytes and no release.
const INITIAL_VERSION: u32 = 2;
// A pending external ownership transfer with distinct old/new image layouts.
const TRANSITION_VERSION: u32 = 3;
const MAX_ALLOCATION: u64 = 1 << 30;

#[cfg(test)]
mod tests;

struct Record<'a> {
    uuids: ([u8; 16], [u8; 16]),
    size: u64,
    type_index: u32,
    image: CanonicalImage,
    state: SharedImageState,
    pixels: &'a [u8],
}

fn validate(image: &CanonicalImage, state: SharedImageState) -> Result<usize, ScanoutError> {
    let transfer = state.transfer();
    let release = transfer.map(|(_, release)| release);
    if !image.is_bgra8()
        || image.width == 0
        || image.height == 0
        || u64::from(image.width) * u64::from(image.height) > crate::MAX_RESOURCE_PIXELS
        || image.flags != 8
        || image.usage & 3 != 3
        || image.usage & !0x9f != 0
        || image.view_formats != [image.format, if image.format == 44 { 50 } else { 44 }]
        || release.is_some_and(|release| {
            copy_layout(release.layout).is_none()
                || !matches!(
                    release.family,
                    policy::QUEUE_FAMILY_EXTERNAL | policy::QUEUE_FAMILY_FOREIGN
                )
        })
    {
        return Err(refused(
            "invalid canonical shared-image checkpoint metadata",
        ));
    }
    if transfer.is_some_and(|(old, release)| {
        copy_layout(old).is_none()
            || matches!(state, SharedImageState::ReleasedTransition { .. }) && old == release.layout
            || old == LAYOUT_COLOR_ATTACHMENT && image.usage & 0x10 == 0
            || old == LAYOUT_SHADER_READ_ONLY && image.usage & (4 | 0x80) == 0
    }) {
        return Err(refused("invalid shared image checkpoint transfer layout"));
    }
    if release.is_some_and(|release| {
        release.layout == LAYOUT_COLOR_ATTACHMENT && image.usage & 0x10 == 0
            || release.layout == LAYOUT_SHADER_READ_ONLY && image.usage & (4 | 0x80) == 0
    }) {
        return Err(refused(
            "shared image checkpoint layout is incompatible with its usage",
        ));
    }
    if state == SharedImageState::Initial {
        return Ok(0);
    }
    usize::try_from(u64::from(image.width) * u64::from(image.height) * 4)
        .map_err(|_| refused("shared image checkpoint exceeds the host address space"))
}

/// Exact framed length; this validates geometry without a native GPU read.
pub fn encoded_len(
    image: &CanonicalImage,
    state: impl Into<SharedImageState>,
) -> Result<u64, ScanoutError> {
    let state = state.into();
    (validate(image, state)? as u64)
        .checked_add(
            if matches!(state, SharedImageState::ReleasedTransition { .. }) {
                100
            } else {
                96
            },
        )
        .ok_or_else(|| refused("shared image checkpoint length overflow"))
}

/// The actual validated payload selects its native uploader device.
pub fn image_uuids(bytes: &[u8]) -> Result<([u8; 16], [u8; 16]), ScanoutError> {
    Ok(parse(bytes)?.uuids)
}

fn parse(bytes: &[u8]) -> Result<Record<'_>, ScanoutError> {
    let parse = || -> Result<Record<'_>, String> {
        let mut decoder = Decoder::new(bytes);
        let version = decoder.u32().map_err(|e| e.to_string())?;
        if !matches!(version, VERSION | INITIAL_VERSION | TRANSITION_VERSION) {
            return Err("shared image checkpoint version differs".into());
        }
        let mut uuid = || -> Result<[u8; 16], String> {
            let mut out = [0; 16];
            for word in out.chunks_exact_mut(4) {
                word.copy_from_slice(&decoder.u32().map_err(|e| e.to_string())?.to_le_bytes());
            }
            Ok(out)
        };
        let uuids = (uuid()?, uuid()?);
        let size = decoder.u64().map_err(|e| e.to_string())?;
        let type_index = decoder.u32().map_err(|e| e.to_string())?;
        if size == 0 || size > MAX_ALLOCATION || type_index >= 32 {
            return Err("invalid shared image allocation metadata".into());
        }
        let format = decoder.u32().map_err(|e| e.to_string())? as i32;
        let flags = decoder.u32().map_err(|e| e.to_string())?;
        let usage = decoder.u32().map_err(|e| e.to_string())?;
        let width = decoder.u32().map_err(|e| e.to_string())?;
        let height = decoder.u32().map_err(|e| e.to_string())?;
        let count = decoder.u32().map_err(|e| e.to_string())?;
        if count != 2 {
            return Err("invalid canonical image view-format count".into());
        }
        let view_formats = vec![
            decoder.u32().map_err(|e| e.to_string())? as i32,
            decoder.u32().map_err(|e| e.to_string())? as i32,
        ];
        let image = CanonicalImage {
            format,
            flags,
            usage,
            width,
            height,
            view_formats,
        };
        let release = ScanoutRelease {
            layout: decoder.u32().map_err(|e| e.to_string())? as i32,
            family: decoder.u32().map_err(|e| e.to_string())?,
        };
        let state = if version == INITIAL_VERSION {
            if release.layout != 0 || release.family != u32::MAX {
                return Err("Initial shared image carries release metadata".into());
            }
            SharedImageState::Initial
        } else if version == TRANSITION_VERSION {
            SharedImageState::ReleasedTransition {
                old_layout: decoder.u32().map_err(|e| e.to_string())? as i32,
                release,
            }
        } else {
            SharedImageState::Released(release)
        };
        let expected = validate(&image, state).map_err(|e| e.to_string())?;
        if decoder.u64().map_err(|e| e.to_string())? != expected as u64 {
            return Err("shared image checkpoint byte count differs".into());
        }
        let pixels = decoder.blob(expected).map_err(|e| e.to_string())?;
        if decoder.remaining() != 0 {
            return Err("trailing shared image checkpoint bytes".into());
        }
        Ok(Record {
            uuids,
            size,
            type_index,
            image,
            state,
            pixels,
        })
    };
    parse().map_err(refused)
}

impl<H: HostVulkan> ScanoutDevice<H> {
    /// Save one externally released canonical payload. All guest, presenter and
    /// scanout writers must be stopped/drained. The operation restores ownership
    /// and layout before returning, including before any disk write can fail.
    /// The target may outlive its exporting Vulkan context.
    pub fn save_shared_image(
        &mut self,
        target: &ScanoutTarget,
        release: impl Into<SharedImageState>,
        writer: &mut impl Write,
    ) -> Result<(), ScanoutError> {
        self.save_shared_image_state(target, release.into(), writer)
    }

    pub(super) fn save_shared_image_state(
        &mut self,
        target: &ScanoutTarget,
        state: SharedImageState,
        writer: &mut impl Write,
    ) -> Result<(), ScanoutError> {
        let expected = validate(&target.image, state)?;
        let export = target
            .handle
            .0
            .downcast_ref::<HandleExport<H>>()
            .ok_or_else(|| refused("shared image handle belongs to another host backend"))?;
        if export.uuids != self.uuids
            || export.size == 0
            || export.size > MAX_ALLOCATION
            || export.type_index >= 32
        {
            return Err(refused("shared image allocation or GPU is incompatible"));
        }
        let mut pixels = Vec::new();
        if let Some((old_layout, release)) = state.transfer() {
            self.copy_with_transition(
                target,
                old_layout,
                release,
                Rect {
                    x: 0,
                    y: 0,
                    width: target.image.width,
                    height: target.image.height,
                },
            )?
            .collect(&mut pixels)?;
        }
        if pixels.len() != expected {
            return Err(refused("incomplete shared image readback"));
        }
        let mut write = || -> std::io::Result<()> {
            let version = match state {
                SharedImageState::Initial => INITIAL_VERSION,
                SharedImageState::Released(_) => VERSION,
                SharedImageState::ReleasedTransition { .. } => TRANSITION_VERSION,
            };
            writer.write_all(&version.to_le_bytes())?;
            writer.write_all(&export.uuids.0)?;
            writer.write_all(&export.uuids.1)?;
            writer.write_all(&export.size.to_le_bytes())?;
            for value in [
                export.type_index,
                target.image.format as u32,
                target.image.flags,
                target.image.usage,
                target.image.width,
                target.image.height,
                2,
            ] {
                writer.write_all(&value.to_le_bytes())?;
            }
            for format in &target.image.view_formats {
                writer.write_all(&format.to_le_bytes())?;
            }
            // Reserved fields in the Initial variant are canonical zeros/IGNORED,
            // not an invented external release.
            let (layout, family) = match state {
                SharedImageState::Initial => (0i32, u32::MAX),
                SharedImageState::Released(release)
                | SharedImageState::ReleasedTransition { release, .. } => {
                    (release.layout, release.family)
                }
            };
            writer.write_all(&layout.to_le_bytes())?;
            writer.write_all(&family.to_le_bytes())?;
            if let SharedImageState::ReleasedTransition { old_layout, .. } = state {
                writer.write_all(&old_layout.to_le_bytes())?;
            }
            writer.write_all(&(pixels.len() as u64).to_le_bytes())?;
            writer.write_all(&pixels)
        };
        write().map_err(|e| refused(format!("writing shared image checkpoint: {e}")))
    }

    /// Recreate a payload with a new exportable allocation and native handle.
    /// Entire framing is validated first. The returned handle owns the restored
    /// allocation even after this private device is dropped. `limits` must be
    /// from the restoring renderer, so exports/imports share its heap accounting.
    /// This component currently covers BGRA canonical images at offset zero.
    pub fn restore_shared_image(
        &mut self,
        resource_id: u32,
        bytes: &[u8],
        limits: &ContextLimits,
    ) -> Result<(ScanoutTarget, ScanoutRelease), ScanoutError> {
        self.restore_released_shared_image(resource_id, bytes, limits, None)
    }

    /// Require the recorded allocation owner, including an orphan context's
    /// share, before charging or allocating native GPU memory.
    pub fn restore_shared_image_owned(
        &mut self,
        resource_id: u32,
        bytes: &[u8],
        limits: &ContextLimits,
        owner: &Arc<PageBudget>,
    ) -> Result<(ScanoutTarget, ScanoutRelease), ScanoutError> {
        self.restore_released_shared_image(resource_id, bytes, limits, Some(owner))
    }

    /// Return an ownership proof only after the actual native pixel upload and
    /// release completed. The cold factory can use it for a later checkpoint.
    pub fn restore_shared_image_checkpoint(
        &mut self,
        resource_id: u32,
        bytes: &[u8],
        limits: &ContextLimits,
        owner: &Arc<PageBudget>,
    ) -> Result<super::super::checkpoint_ownership::RestoredSharedImage, ScanoutError> {
        let (target, state) =
            self.restore_shared_image_with_owner(resource_id, bytes, limits, Some(owner))?;
        Ok(super::super::checkpoint_ownership::RestoredSharedImage {
            target,
            state,
            owner: owner.clone(),
        })
    }

    fn restore_released_shared_image(
        &mut self,
        resource_id: u32,
        bytes: &[u8],
        limits: &ContextLimits,
        owner: Option<&Arc<PageBudget>>,
    ) -> Result<(ScanoutTarget, ScanoutRelease), ScanoutError> {
        if !matches!(parse(bytes)?.state, SharedImageState::Released(_)) {
            return Err(refused(
                "Initial payload requires checkpoint ownership adoption",
            ));
        }
        let (target, state) =
            self.restore_shared_image_with_owner(resource_id, bytes, limits, owner)?;
        match state {
            SharedImageState::Released(release) => Ok((target, release)),
            _ => Err(refused(
                "restored payload requires full checkpoint ownership adoption",
            )),
        }
    }

    fn restore_shared_image_with_owner(
        &mut self,
        resource_id: u32,
        bytes: &[u8],
        limits: &ContextLimits,
        owner: Option<&Arc<PageBudget>>,
    ) -> Result<(ScanoutTarget, SharedImageState), ScanoutError> {
        let record = parse(bytes)?;
        if record.uuids != self.uuids {
            return Err(refused("shared image GPU or driver UUID differs"));
        }
        self.settle()?;
        let instance = self
            .instance
            .as_ref()
            .ok_or_else(|| refused("no instance"))?;
        let physicals = self
            .host
            .enumerate_physical_devices(instance)
            .map_err(|ret| refused(format!("checkpoint physical devices: {ret}")))?;
        let found = physicals.into_iter().find_map(|physical| {
            let info = self.host.describe_physical_device(instance, physical);
            let guest = policy::expose(info).ok()?;
            (guest.uuids() == Some(record.uuids)).then_some((physical, guest))
        });
        let (physical, guest) = found.ok_or_else(|| refused("checkpoint GPU is unavailable"))?;
        let canonical = super::super::modifier::canonical_format(
            &*self.host,
            instance,
            physical,
            record.image.format,
        )
        .ok_or_else(|| refused("checkpoint canonical image is unsupported"))?;
        if canonical.image(record.image.width, record.image.height) != record.image
            || record.image.width > canonical.limits.max_extent.width
            || record.image.height > canonical.limits.max_extent.height
        {
            return Err(refused(
                "checkpoint canonical create parameters differ from this driver",
            ));
        }
        let ty = guest
            .memory
            .memory_types
            .get(record.type_index as usize)
            .filter(|_| record.type_index < guest.memory.memory_type_count)
            .ok_or_else(|| refused("checkpoint memory type is unavailable"))?;
        if ty.property_flags & 3 != 1 {
            return Err(refused(
                "checkpoint payload requires private device-local memory",
            ));
        }
        let heap = guest
            .memory
            .memory_heaps
            .get(ty.heap_index as usize)
            .filter(|_| ty.heap_index < guest.memory.memory_heap_count)
            .ok_or_else(|| refused("checkpoint memory heap is unavailable"))?;
        let heap_budget = limits.device_local((record.uuids.0, ty.heap_index), heap.size);
        if owner.is_some_and(|owner| !Arc::ptr_eq(owner, &heap_budget)) {
            return Err(refused(
                "shared image allocation owner differs from its saved heap share",
            ));
        }
        let charge = heap_budget
            .charge(record.size)
            .map_err(|_| refused("shared image checkpoint exceeds its GPU heap budget"))?;
        let device = self.device.as_ref().ok_or_else(|| refused("no device"))?;
        let memory = self
            .host
            .allocate_memory(
                device,
                &MemoryRequest {
                    size: record.size,
                    type_index: record.type_index,
                    import: None,
                    flags: None,
                    dedicated: None,
                    export_handle: true,
                    import_handle: None,
                },
            )
            .map_err(|ret| refused(format!("allocating checkpoint shared payload: {ret}")))?;
        let exported = self.host.export_memory_handle(device, &memory);
        self.host.free_memory(device, memory);
        let shared = exported
            .map_err(|ret| refused(format!("exporting checkpoint shared payload: {ret}")))?;
        let handle = SharedHandle(Arc::new(HandleExport::<H> {
            shared: Arc::new(shared),
            type_index: record.type_index,
            size: record.size,
            uuids: record.uuids,
            charge: Some(Arc::new(charge)),
        }));
        let spec = crate::renderer::ScanoutBlobSpec {
            format: crate::FORMAT_B8G8R8A8_UNORM,
            width: record.image.width,
            height: record.image.height,
            stride: u32::try_from(record.image.layout().row_pitch)
                .map_err(|_| refused("checkpoint image pitch overflows"))?,
            offset: 0,
        };
        let target = ScanoutTarget {
            resource_id,
            handle,
            image: record.image,
            spec,
        };
        let at = self.target_index(&target)?;
        // Native allocation, memory type, geometry and binding were validated.
        // Initial images must remain UNDEFINED: no upload or GPU submission.
        let Some((old_layout, release)) = record.state.transfer() else {
            return Ok((target, SharedImageState::Initial));
        };
        let t = self
            .targets
            .get(at)
            .ok_or_else(|| refused("missing restored shared target"))?;
        let (Some(image), Some(buffer), Some(pages)) = (t.image, t.buffer, t.pages.clone()) else {
            return Err(refused("incomplete restored shared target"));
        };
        pages
            .import_pages()
            .write_bytes(0, record.pixels)
            .map_err(|e| refused(format!("checkpoint upload pages: {e}")))?;
        let (cb, fence, family) = (self.cb, self.fence, self.family);
        let range = VkImageSubresourceRange {
            aspect_mask: 1,
            base_mip_level: 0,
            level_count: 1,
            base_array_layer: 0,
            layer_count: 1,
        };
        let transition =
            |old_layout, new_layout, src_access_mask, dst_access_mask, families: (u32, u32)| {
                VkImageMemoryBarrier {
                    p_next: Vec::new(),
                    old_layout,
                    new_layout,
                    src_access_mask,
                    dst_access_mask,
                    src_queue_family_index: families.0,
                    dst_queue_family_index: families.1,
                    image: VkImage(image.raw()),
                    subresource_range: range.clone(),
                }
            };
        let barrier = |src_stage_mask,
                       dst_stage_mask,
                       images: Vec<VkImageMemoryBarrier>,
                       buffers: Vec<VkBufferMemoryBarrier>| {
            Command::CmdPipelineBarrier(CmdPipelineBarrierArgs {
                command_buffer: VkCommandBuffer(cb),
                src_stage_mask,
                dst_stage_mask,
                image_memory_barrier_count: images.len() as u32,
                p_image_memory_barriers: Some(images),
                buffer_memory_barrier_count: buffers.len() as u32,
                p_buffer_memory_barriers: Some(buffers),
                ..Default::default()
            })
        };
        let ignored = (u32::MAX, u32::MAX);
        let commands = vec![
            Command::ResetFences(ResetFencesArgs {
                device: VkDevice(0),
                fence_count: 1,
                p_fences: Some(vec![VkFence(fence)]),
                ret: VK_ERROR_UNKNOWN,
            }),
            Command::ResetCommandBuffer(ResetCommandBufferArgs {
                command_buffer: VkCommandBuffer(cb),
                flags: 0,
                ret: VK_ERROR_UNKNOWN,
            }),
            Command::BeginCommandBuffer(BeginCommandBufferArgs {
                command_buffer: VkCommandBuffer(cb),
                p_begin_info: Some(VkCommandBufferBeginInfo {
                    flags: 1,
                    ..Default::default()
                }),
                ret: VK_ERROR_UNKNOWN,
            }),
            barrier(
                STAGE_HOST | STAGE_TOP_OF_PIPE,
                STAGE_TRANSFER,
                vec![transition(
                    0,
                    LAYOUT_TRANSFER_DST,
                    0,
                    ACCESS_TRANSFER_WRITE,
                    ignored,
                )],
                vec![VkBufferMemoryBarrier {
                    p_next: Vec::new(),
                    src_access_mask: 0x4000,
                    dst_access_mask: ACCESS_TRANSFER_READ,
                    src_queue_family_index: u32::MAX,
                    dst_queue_family_index: u32::MAX,
                    buffer: VkBuffer(buffer.raw()),
                    offset: 0,
                    size: record.pixels.len() as u64,
                }],
            ),
            Command::CmdCopyBufferToImage(CmdCopyBufferToImageArgs {
                command_buffer: VkCommandBuffer(cb),
                src_buffer: VkBuffer(buffer.raw()),
                dst_image: VkImage(image.raw()),
                dst_image_layout: LAYOUT_TRANSFER_DST,
                region_count: 1,
                p_regions: Some(vec![VkBufferImageCopy {
                    buffer_offset: 0,
                    buffer_row_length: 0,
                    buffer_image_height: 0,
                    image_subresource: VkImageSubresourceLayers {
                        aspect_mask: 1,
                        mip_level: 0,
                        base_array_layer: 0,
                        layer_count: 1,
                    },
                    image_offset: VkOffset3D { x: 0, y: 0, z: 0 },
                    image_extent: VkExtent3D {
                        width: target.image.width,
                        height: target.image.height,
                        depth: 1,
                    },
                }]),
            }),
            // Recreate the exact pending transfer pair. A guest acquire must
            // match both layouts, even when the release also changes layout.
            barrier(
                STAGE_TRANSFER,
                STAGE_TRANSFER,
                vec![transition(
                    LAYOUT_TRANSFER_DST,
                    old_layout,
                    ACCESS_TRANSFER_WRITE,
                    0,
                    ignored,
                )],
                vec![],
            ),
            barrier(
                STAGE_TRANSFER,
                STAGE_BOTTOM_OF_PIPE,
                vec![transition(
                    old_layout,
                    release.layout,
                    0,
                    0,
                    (family, release.family),
                )],
                vec![],
            ),
            Command::EndCommandBuffer(EndCommandBufferArgs {
                command_buffer: VkCommandBuffer(cb),
                ret: VK_ERROR_UNKNOWN,
            }),
        ];
        self.submit_copy_commands(commands)?;
        Ok((target, record.state))
    }
}
