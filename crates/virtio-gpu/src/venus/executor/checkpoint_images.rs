//! Checkpoint component for private, single-sample 2D color images.
//! Pixels are copied through bounded staging with their executed layouts.
//! Shared payloads and image aliases still need a separate global checkpoint.

use std::io::Write;
use std::sync::Arc;
use std::time::{Duration, Instant};

use super::checkpoint::RecipeError;
use super::checkpoint_buffers::{TransferRequest, CHUNK_BYTES};
use super::host::{HostVulkan, RawHandle};
use super::image_layouts::{Cell, Owner};
use super::objects::{Binding, ImageFacts, MemoryHandle};
use super::policy::{MEMORY_PROPERTY_DEVICE_LOCAL, MEMORY_PROPERTY_HOST_VISIBLE};
use super::VulkanContext;
use crate::venus::protocol::*;

mod formats;
#[cfg(test)]
mod tests;

const VERSION: u32 = 2;
const MAX_CELLS: u64 = 1 << 16;
const ALL_COMMANDS: u32 = 0x1_0000;
const TRANSFER: u32 = 0x1000;
const MEMORY_READ_WRITE: u32 = 0x1_8000;
// These flags change permissible views, not the base subresource copy geometry.
const COPY_VIEW_FLAGS: u32 = 0x8 | 0x10 | 0x80 | 0x100;

#[derive(Clone, Copy, Debug)]
struct Block {
    width: u32,
    height: u32,
    depth: u32,
    bytes: u32,
}

fn failed(reason: impl Into<String>) -> RecipeError {
    RecipeError::Replay(reason.into())
}

fn aspect_blocks(format: i32) -> Option<Vec<(u32, Block)>> {
    let block = |bytes| Block {
        width: 1,
        height: 1,
        depth: 1,
        bytes,
    };
    Some(match format {
        124 => vec![(2, block(2))],
        125 | 126 => vec![(2, block(4))],
        127 => vec![(4, block(1))],
        128 => vec![(2, block(2)), (4, block(1))],
        129 | 130 => vec![(2, block(4)), (4, block(1))],
        _ => vec![(
            1,
            formats::color_block(format)
                .filter(|b| b.depth == 1 && b.bytes != 0 && b.bytes as usize <= CHUNK_BYTES)?,
        )],
    })
}

struct Target<H: HostVulkan> {
    id: u64,
    device: u64,
    host: H::Image,
    facts: ImageFacts,
    binding: Binding,
    queue: u64,
    family: u32,
    block: Block,
    aspect: u32,
    barrier_aspects: u32,
}

#[derive(Clone, Copy, Debug)]
struct Tile {
    x: u32,
    y: u32,
    width: u32,
    height: u32,
    bytes: usize,
}

/// Iterate in block rows, splitting exceptionally wide rows too. A tile's
/// start is block-aligned; its final edge may reach a non-block-aligned extent.
struct Tiles {
    width: u32,
    height: u32,
    block: Block,
    bx: u32,
    by: u32,
}

impl Tiles {
    fn new(facts: ImageFacts, block: Block, mip: u32) -> Self {
        Self {
            width: facts.width.checked_shr(mip).unwrap_or(0).max(1),
            height: facts.height.checked_shr(mip).unwrap_or(0).max(1),
            block,
            bx: 0,
            by: 0,
        }
    }

    fn byte_len(&self) -> Result<u64, RecipeError> {
        u64::from(self.width.div_ceil(self.block.width))
            .checked_mul(u64::from(self.height.div_ceil(self.block.height)))
            .and_then(|n| n.checked_mul(u64::from(self.block.bytes)))
            .ok_or(RecipeError::Budget)
    }
}

impl Iterator for Tiles {
    type Item = Tile;

    fn next(&mut self) -> Option<Tile> {
        let cols = self.width.div_ceil(self.block.width);
        let rows = self.height.div_ceil(self.block.height);
        if self.by >= rows {
            return None;
        }
        let nx = (cols - self.bx).min(CHUNK_BYTES as u32 / self.block.bytes);
        let ny = if self.bx == 0 && nx == cols {
            (CHUNK_BYTES as u32 / (nx * self.block.bytes)).min(rows - self.by)
        } else {
            1
        };
        let x = self.bx * self.block.width;
        let y = self.by * self.block.height;
        let tile = Tile {
            x,
            y,
            width: (nx * self.block.width).min(self.width - x),
            height: (ny * self.block.height).min(self.height - y),
            bytes: (nx * ny * self.block.bytes) as usize,
        };
        self.bx += nx;
        if self.bx == cols {
            self.bx = 0;
            self.by += ny;
        }
        Some(tile)
    }
}

fn core_layout(layout: i32, aspect: u32) -> Option<i32> {
    match layout {
        0..=8 => Some(layout),
        1_000_117_000 | 1_000_117_001 if aspect != 1 => Some(layout),
        1_000_314_000 => Some(if aspect == 1 { 5 } else { 4 }), // READ_ONLY_OPTIMAL
        1_000_314_001 => Some(if aspect == 1 { 2 } else { 3 }), // ATTACHMENT_OPTIMAL
        _ => None,
    }
}

fn validate_cell(cell: Cell, family: u32, usage: u32, aspect: u32) -> Result<(), RecipeError> {
    if core_layout(cell.layout, aspect).is_none() {
        return Err(failed("unsupported image checkpoint layout"));
    }
    let valid_usage = match core_layout(cell.layout, aspect) {
        Some(2) => aspect == 1 && usage & 0x10 != 0,
        Some(3 | 1_000_117_000 | 1_000_117_001) => aspect != 1 && usage & 0x20 != 0,
        Some(4) => aspect != 1 && usage & (0x20 | 4 | 0x80) != 0,
        Some(5) => usage & (4 | 0x80) != 0,
        _ => true,
    };
    if !valid_usage {
        return Err(RecipeError::Invalid(
            "saved image layout is incompatible with its usage",
        ));
    }
    match cell.owner {
        Owner::Concurrent | Owner::Queue(_)
            if cell.owner == Owner::Queue(family) || cell.owner == Owner::Concurrent =>
        {
            Ok(())
        }
        Owner::Initial if matches!(cell.layout, 0 | 8) => Ok(()),
        _ => Err(failed(
            "image ownership is not available to its checkpoint queue",
        )),
    }
}

fn aspect_barrier(
    cb: VkCommandBuffer,
    image: u64,
    subresource: (u32, u32, u32),
    layouts: (i32, i32),
    upload: bool,
    before: bool,
) -> Command<'static> {
    let (aspect, mip, layer) = subresource;
    let (old_layout, new_layout) = layouts;
    let access = if upload { 0x1000 } else { 0x800 };
    Command::CmdPipelineBarrier(CmdPipelineBarrierArgs {
        command_buffer: cb,
        src_stage_mask: if before { ALL_COMMANDS } else { TRANSFER },
        dst_stage_mask: if before { TRANSFER } else { ALL_COMMANDS },
        image_memory_barrier_count: 1,
        p_image_memory_barriers: Some(vec![VkImageMemoryBarrier {
            p_next: Vec::new(),
            src_access_mask: if before {
                if old_layout == 0 {
                    0
                } else {
                    MEMORY_READ_WRITE
                }
            } else {
                access
            },
            dst_access_mask: if before { access } else { MEMORY_READ_WRITE },
            old_layout,
            new_layout,
            src_queue_family_index: u32::MAX,
            dst_queue_family_index: u32::MAX,
            image: VkImage(image),
            subresource_range: VkImageSubresourceRange {
                aspect_mask: aspect,
                base_mip_level: mip,
                level_count: 1,
                base_array_layer: layer,
                layer_count: 1,
            },
        }]),
        ..Default::default()
    })
}

impl<H: HostVulkan> VulkanContext<H> {
    fn checkpoint_image_queue(
        &self,
        device: u64,
        family: Option<u32>,
        sharing: super::objects::Sharing,
        format: i32,
    ) -> Result<(u64, u32), RecipeError> {
        let (owner, guest) = self
            .objects
            .device_and_guest(device)
            .map_err(|e| failed(e.to_string()))?;
        let supported = |index: u32| {
            guest.queue_families.get(index as usize).is_some_and(|p| {
                let g = &p.min_image_transfer_granularity;
                sharing.allows(index)
                    && p.queue_flags & 7 != 0
                    && (!(124..=130).contains(&format) || p.queue_flags & 1 != 0)
                    && g.width == 1
                    && g.height == 1
                    && g.depth == 1
            })
        };
        let family = family
            .or_else(|| {
                owner
                    .queues
                    .iter()
                    .filter(|q| q.id.is_some() && q.flags == 0 && supported(q.family))
                    .map(|q| q.family)
                    .min()
            })
            .ok_or_else(|| failed("no compatible image checkpoint queue"))?;
        if !supported(family) {
            return Err(failed("image checkpoint queue lacks sharing, graphics/depth-copy or unit-granularity support"));
        }
        Ok((self.checkpoint_family_queue(device, Some(family))?, family))
    }
    /// Exact framed component length without submitting a GPU readback.
    pub fn checkpoint_images_length(&self) -> Result<u64, RecipeError> {
        let targets = self.checkpoint_image_targets()?;
        let mut length = 8u64;
        for target in &targets {
            length = length.checked_add(64).ok_or(RecipeError::Budget)?;
            for mip in 0..target.facts.mip_levels {
                for layer in 0..target.facts.array_layers {
                    let cell = self
                        .image_tracking
                        .images
                        .get(&target.id)
                        .and_then(|state| state.checkpoint_aspect_cell(target.aspect, mip, layer))
                        .ok_or_else(|| failed("missing image layout while sizing checkpoint"))?;
                    let bytes = if matches!(cell.layout, 0 | 8) {
                        0
                    } else {
                        Tiles::new(target.facts, target.block, mip).byte_len()?
                    };
                    length = bytes
                        .checked_next_multiple_of(4)
                        .and_then(|bytes| bytes.checked_add(20))
                        .and_then(|bytes| length.checked_add(bytes))
                        .ok_or(RecipeError::Budget)?;
                }
            }
        }
        Ok(length)
    }
    fn checkpoint_image_targets(&self) -> Result<Vec<Target<H>>, RecipeError> {
        if let Some(error) = &self.image_tracking.error {
            return Err(failed(error));
        }
        let mut targets = Vec::new();
        let aliases = self.checkpoint_binding_aliases()?;
        let mut cells = 0u64;
        for (id, image) in self.objects.checkpoint_images() {
            if !image.fully_bound() {
                continue;
            }
            let mut bindings = image.bindings.iter().zip(&image.binding_identities);
            let Some((binding, identity)) = bindings.next() else {
                continue;
            };
            let Ok(memory) = self.objects.memory(image.device, binding.memory) else {
                continue;
            };
            if !Arc::ptr_eq(identity, &memory.identity) {
                continue;
            }
            if memory.pages.is_some() {
                continue;
            }
            // The global payload graph carries canonical external pixels once,
            // shared by every context. Never upload a private copy over them.
            if memory.shared_handle.is_some()
                && image.canonical.is_some()
                && binding.offset == 0
                && image.bindings.len() == 1
            {
                continue;
            }
            if bindings.next().is_some() || image.facts.planes != 1 {
                return Err(failed(
                    "multi-plane image checkpoint needs per-plane bindings",
                ));
            }
            if image.facts.image_type != 1
                || image.facts.depth != 1
                || image.facts.samples != 1
                || image.facts.flags & !COPY_VIEW_FLAGS != 0
                || image.facts.usage & 3 != 3
            {
                return Err(failed(format!(
                    "image checkpoint needs an unprotected single-sample transfer-capable 2D image: context {}, image {id:#x}, facts {:?}", self.ctx_id, image.facts
                )));
            }
            if image.facts.flags & 0x100 != 0 {
                // EXTENDED_USAGE can obtain usage support from a view format.
                // Our copies use the image's base format, so check it directly.
                let device = self
                    .objects
                    .device(image.device)
                    .map_err(|e| failed(e.to_string()))?;
                let (instance, physical) = self
                    .objects
                    .physical(device.physical)
                    .map_err(|e| failed(e.to_string()))?;
                let mut properties = VkFormatProperties2::default();
                self.host.format_properties(
                    instance,
                    physical.host,
                    image.facts.format,
                    &mut properties,
                );
                let features = match image.facts.tiling {
                    0 => properties.format_properties.optimal_tiling_features,
                    1 => properties.format_properties.linear_tiling_features,
                    _ => return Err(failed("private image copy needs known native tiling")),
                };
                if features & 0xc000 != 0xc000 {
                    return Err(failed(
                        "extended-usage image base format lacks transfer features",
                    ));
                }
            }
            if memory.property_flags & MEMORY_PROPERTY_DEVICE_LOCAL == 0
                || memory.property_flags & MEMORY_PROPERTY_HOST_VISIBLE != 0
                || memory.handle != MemoryHandle::None
                || memory.exported
                || image.handle
                || image.external
            {
                return Err(failed("image checkpoint needs private device-local memory"));
            }
            // Until active aliases are tracked, do not issue a GPU read from
            // an image whose backing bytes may belong to another resource.
            if aliases.image(id) {
                return Err(failed(
                    "image allocation aliases need active-alias checkpointing",
                ));
            }
            let blocks = aspect_blocks(image.facts.format).ok_or_else(|| {
                failed(format!(
                    "unsupported checkpoint image format: context {}, image {id:#x}, facts {:?}",
                    self.ctx_id, image.facts
                ))
            })?;
            let state = self
                .image_tracking
                .images
                .get(&id)
                .filter(|state| Arc::ptr_eq(&state.identity, &image.identity))
                .ok_or_else(|| failed("missing executed image layouts"))?;
            let owner = state.spans.iter().find_map(|span| match span.cell.owner {
                Owner::Queue(family) => Some(family),
                _ => None,
            });
            let (queue, family) = self.checkpoint_image_queue(
                image.device,
                owner,
                image.sharing,
                image.facts.format,
            )?;
            for (aspect, block) in blocks {
                let barrier_aspects = if matches!(image.facts.format, 128..=130) {
                    6
                } else {
                    aspect
                };
                cells = cells
                    .checked_add(
                        u64::from(image.facts.mip_levels) * u64::from(image.facts.array_layers),
                    )
                    .ok_or(RecipeError::Budget)?;
                if cells > MAX_CELLS {
                    return Err(RecipeError::Budget);
                }
                for mip in 0..image.facts.mip_levels {
                    for layer in 0..image.facts.array_layers {
                        let cell = state
                            .checkpoint_aspect_cell(aspect, mip, layer)
                            .ok_or_else(|| failed("missing image aspect layout"))?;
                        validate_cell(cell, family, image.facts.usage, aspect)?;
                        if barrier_aspects == 6
                            && state.checkpoint_aspect_cell(
                                if aspect == 2 { 4 } else { 2 },
                                mip,
                                layer,
                            ) != Some(cell)
                        {
                            return Err(failed("separate depth/stencil layouts or ownership need independent transition support"));
                        }
                    }
                }
                targets.try_reserve(1).map_err(|_| RecipeError::Budget)?;
                targets.push(Target {
                    id,
                    device: image.device,
                    host: image.host,
                    facts: image.facts,
                    binding: *binding,
                    queue,
                    family,
                    block,
                    aspect,
                    barrier_aspects,
                });
            }
        }
        targets.sort_unstable_by_key(|t| (t.id, t.aspect));
        Ok(targets)
    }

    fn checkpoint_image_tile(
        &mut self,
        target: &Target<H>,
        subresource: (u32, u32),
        cell: Cell,
        tile: Tile,
        upload: Option<&[u8]>,
        deadline: Instant,
    ) -> Result<crate::venus::shmem::PrivatePages, RecipeError> {
        let (mip, layer) = subresource;
        let initial = self
            .image_tracking
            .images
            .get(&target.id)
            .and_then(|s| s.checkpoint_aspect_cell(target.aspect, mip, layer))
            .ok_or_else(|| failed("missing image transfer layout"))?;
        let old = core_layout(initial.layout, target.aspect)
            .ok_or_else(|| failed("unsupported initial layout"))?;
        let saved = core_layout(cell.layout, target.aspect)
            .ok_or_else(|| failed("unsupported saved layout"))?;
        let transfer = if upload.is_some() { 7 } else { 6 };
        let pages = self.checkpoint_transfer(
            TransferRequest {
                device: target.device,
                queue: target.queue,
                len: tile.bytes,
                upload,
                deadline,
            },
            |context, cb, staging| {
                context.checkpoint_call(
                    target.device,
                    aspect_barrier(
                        cb,
                        target.host.raw(),
                        (target.barrier_aspects, mip, layer),
                        (old, transfer),
                        upload.is_some(),
                        true,
                    ),
                )?;
                let region = VkBufferImageCopy {
                    buffer_offset: 0,
                    buffer_row_length: 0,
                    buffer_image_height: 0,
                    image_subresource: VkImageSubresourceLayers {
                        aspect_mask: target.aspect,
                        mip_level: mip,
                        base_array_layer: layer,
                        layer_count: 1,
                    },
                    image_offset: VkOffset3D {
                        x: tile.x as i32,
                        y: tile.y as i32,
                        z: 0,
                    },
                    image_extent: VkExtent3D {
                        width: tile.width,
                        height: tile.height,
                        depth: 1,
                    },
                };
                let command = if upload.is_some() {
                    Command::CmdCopyBufferToImage(CmdCopyBufferToImageArgs {
                        command_buffer: cb,
                        src_buffer: VkBuffer(staging.raw()),
                        dst_image: VkImage(target.host.raw()),
                        dst_image_layout: transfer,
                        region_count: 1,
                        p_regions: Some(vec![region]),
                    })
                } else {
                    Command::CmdCopyImageToBuffer(CmdCopyImageToBufferArgs {
                        command_buffer: cb,
                        src_image: VkImage(target.host.raw()),
                        src_image_layout: transfer,
                        dst_buffer: VkBuffer(staging.raw()),
                        region_count: 1,
                        p_regions: Some(vec![region]),
                    })
                };
                context.checkpoint_call(target.device, command)?;
                context.checkpoint_call(
                    target.device,
                    aspect_barrier(
                        cb,
                        target.host.raw(),
                        (target.barrier_aspects, mip, layer),
                        (transfer, saved),
                        upload.is_some(),
                        false,
                    ),
                )?;
                Ok(())
            },
        )?;
        if upload.is_some() {
            self.image_tracking
                .images
                .get_mut(&target.id)
                .ok_or_else(|| failed("missing completed image upload"))?
                .checkpoint_uploaded(
                    target.id,
                    (target.barrier_aspects, mip, layer),
                    cell,
                    target.family,
                )
                .map_err(failed)?;
        }
        Ok(pages)
    }

    /// Stream private color-image bytes. Guest/ring writers must remain parked.
    /// This component alone does not make a Venus renderer snapshot complete.
    pub fn save_device_images(
        &mut self,
        writer: &mut impl Write,
        timeout: Duration,
    ) -> Result<(), RecipeError> {
        let targets = self.checkpoint_image_targets()?;
        let deadline = Instant::now()
            .checked_add(timeout)
            .ok_or_else(|| failed("invalid image checkpoint timeout"))?;
        self.checkpoint_wait_idle(deadline)?;
        writer.write_all(&VERSION.to_le_bytes())?;
        writer.write_all(&(targets.len() as u32).to_le_bytes())?;
        let mut bytes = Vec::new();
        for target in &targets {
            for value in [
                target.device,
                target.id,
                target.binding.memory,
                target.binding.offset,
            ] {
                writer.write_all(&value.to_le_bytes())?;
            }
            for value in [
                target.facts.format as u32,
                target.facts.width,
                target.facts.height,
                target.facts.mip_levels,
                target.facts.array_layers,
                target.facts.tiling as u32,
                target.aspect,
                target.family,
            ] {
                writer.write_all(&value.to_le_bytes())?;
            }
            for mip in 0..target.facts.mip_levels {
                for layer in 0..target.facts.array_layers {
                    let cell = self
                        .image_tracking
                        .images
                        .get(&target.id)
                        .and_then(|s| s.checkpoint_aspect_cell(target.aspect, mip, layer))
                        .ok_or_else(|| failed("missing image capture layout"))?;
                    let (owner, family) = match cell.owner {
                        Owner::Initial => (0, 0),
                        Owner::Concurrent => (1, 0),
                        Owner::Queue(family) => (2, family),
                        _ => return Err(failed("released image ownership")),
                    };
                    for value in [cell.layout as u32, owner, family] {
                        writer.write_all(&value.to_le_bytes())?;
                    }
                    let tiles = Tiles::new(target.facts, target.block, mip);
                    let len = if matches!(cell.layout, 0 | 8) {
                        0
                    } else {
                        tiles.byte_len()?
                    };
                    writer.write_all(&len.to_le_bytes())?;
                    if len != 0 {
                        for tile in tiles {
                            let pages = self.checkpoint_image_tile(
                                target,
                                (mip, layer),
                                cell,
                                tile,
                                None,
                                deadline,
                            )?;
                            pages.read_rows(0, tile.bytes as u64, tile.bytes, 1, &mut bytes)?;
                            writer.write_all(&bytes)?;
                        }
                    }
                    super::checkpoint::write_padding(writer, len)?;
                }
            }
        }
        Ok(())
    }

    /// Validate the whole image component before touching the reconstructed GPU.
    /// A native upload failure retires the partial context instead of resuming it.
    pub fn restore_device_images(
        &mut self,
        bytes: &[u8],
        timeout: Duration,
    ) -> Result<(), RecipeError> {
        self.restore_device_images_reader(&mut std::io::Cursor::new(bytes), timeout)
    }

    /// Restore a seekable, section-bounded image component through one staging
    /// tile. Validate every shape, owner, cell and byte range before GPU upload.
    pub fn restore_device_images_reader(
        &mut self,
        reader: &mut (impl std::io::Read + std::io::Seek),
        timeout: Duration,
    ) -> Result<(), RecipeError> {
        let mut targets = self.checkpoint_image_targets()?;
        let mut input = super::checkpoint_io::ComponentReader::new(reader)?;
        let version = input.u32()?;
        if version != VERSION {
            return Err(RecipeError::Version(version));
        }
        if input.u32()? as usize != targets.len() {
            return Err(RecipeError::Invalid("streamed image count differs"));
        }
        let mut records = Vec::new();
        let mut paired = std::collections::HashMap::new();
        for target in &mut targets {
            for expected in [
                target.device,
                target.id,
                target.binding.memory,
                target.binding.offset,
            ] {
                if input.u64()? != expected {
                    return Err(RecipeError::Invalid("streamed image binding differs"));
                }
            }
            for expected in [
                target.facts.format as u32,
                target.facts.width,
                target.facts.height,
                target.facts.mip_levels,
                target.facts.array_layers,
                target.facts.tiling as u32,
                target.aspect,
            ] {
                if input.u32()? != expected {
                    return Err(RecipeError::Invalid("streamed image shape differs"));
                }
            }
            let saved_family = input.u32()?;
            let sharing = self
                .objects
                .image(target.device, target.id)
                .map_err(|e| failed(e.to_string()))?
                .sharing;
            (target.queue, target.family) = self.checkpoint_image_queue(
                target.device,
                Some(saved_family),
                sharing,
                target.facts.format,
            )?;
            let target = &*target;
            for mip in 0..target.facts.mip_levels {
                for layer in 0..target.facts.array_layers {
                    let layout = input.u32()? as i32;
                    let owner = input.u32()?;
                    let family = input.u32()?;
                    let cell = Cell {
                        layout,
                        owner: match (owner, family) {
                            (0, 0) => Owner::Initial,
                            (1, 0) => Owner::Concurrent,
                            (2, family) => Owner::Queue(family),
                            _ => return Err(RecipeError::Invalid("invalid streamed image owner")),
                        },
                    };
                    validate_cell(cell, target.family, target.facts.usage, target.aspect)?;
                    let initial = self
                        .image_tracking
                        .images
                        .get(&target.id)
                        .and_then(|state| state.checkpoint_aspect_cell(target.aspect, mip, layer))
                        .ok_or_else(|| failed("missing reconstructed image stream layout"))?;
                    if matches!(layout, 0 | 8) && initial.layout != layout {
                        return Err(RecipeError::Invalid(
                            "undefined streamed layout differs from reconstruction",
                        ));
                    }
                    if (initial.owner == Owner::Concurrent) != (cell.owner == Owner::Concurrent) {
                        return Err(RecipeError::Invalid("streamed image sharing mode differs"));
                    }
                    if target.barrier_aspects == 6 {
                        if let Some(previous) = paired.insert((target.id, mip, layer), cell) {
                            if previous != cell {
                                return Err(RecipeError::Invalid(
                                    "paired depth/stencil checkpoint states differ",
                                ));
                            }
                        }
                    }
                    let len = if matches!(layout, 0 | 8) {
                        0
                    } else {
                        Tiles::new(target.facts, target.block, mip).byte_len()?
                    };
                    if input.u64()? != len {
                        return Err(RecipeError::Invalid("streamed image byte count differs"));
                    }
                    let start = input.blob_offset(len)?;
                    records.try_reserve(1).map_err(|_| RecipeError::Budget)?;
                    records.push((target, mip, layer, cell, start, len));
                }
            }
        }
        input.finish_metadata()?;
        let deadline = Instant::now()
            .checked_add(timeout)
            .ok_or_else(|| failed("invalid image stream restore timeout"))?;
        self.checkpoint_wait_idle(deadline)?;
        let mut bytes = Vec::new();
        if records.iter().any(|record| record.5 != 0) {
            bytes
                .try_reserve_exact(CHUNK_BYTES)
                .map_err(|_| RecipeError::Budget)?;
            bytes.resize(CHUNK_BYTES, 0);
        }
        let result = (|| {
            for (target, mip, layer, cell, start, len) in records {
                if len == 0 {
                    continue;
                }
                let mut offset = 0u64;
                for tile in Tiles::new(target.facts, target.block, mip) {
                    let end = offset
                        .checked_add(tile.bytes as u64)
                        .filter(|end| *end <= len)
                        .ok_or(RecipeError::Invalid(
                            "streamed image tile exceeds its payload",
                        ))?;
                    let chunk = bytes
                        .get_mut(..tile.bytes)
                        .ok_or(RecipeError::Invalid("streamed image tile exceeds staging"))?;
                    input.read_at(start.checked_add(offset).ok_or(RecipeError::Budget)?, chunk)?;
                    self.checkpoint_image_tile(
                        target,
                        (mip, layer),
                        cell,
                        tile,
                        Some(chunk),
                        deadline,
                    )?;
                    offset = end;
                }
                if offset != len {
                    return Err(RecipeError::Invalid(
                        "streamed image tiles do not cover their payload",
                    ));
                }
            }
            input.finish()
        })();
        if result.is_err() {
            self.destroy_all();
        }
        result
    }
}
