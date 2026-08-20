//! A CPU-backed [`Renderer3d`] with no GPU, no GL and no unsafe code
//! (ADR-0004's test double).
//!
//! It models each 3D resource as a linear host byte buffer (4 bytes per
//! element, level 0 / layer 0 only — enough to round-trip scanout pixels) and
//! executes `SUBMIT_3D` by counting the already-validated commands. That makes
//! the *entire* device path — negotiation, capsets, contexts, create/attach,
//! transfers, submits, scanout readback — exercisable on any host, with no
//! virglrenderer and no window, which is exactly what the unit tests, the
//! transport tests and the fuzz target need.
//!
//! It is deliberately **not** wired into `entangled run`: a guest offered
//! `VIRTIO_GPU_F_VIRGL` that lands here would negotiate mesa's virgl driver
//! against a renderer that never draws. Hosts without a real renderer must
//! not offer the feature at all.

use std::collections::HashMap;
use std::sync::Arc;

use virtio_core::GuestMem;

use crate::blob::{BlobMapping, BlobSupport};
use crate::error::CommandError;
use crate::protocol::{MemEntry, Rect, ResourceCreate3d, ResourceCreateBlob, Transfer3d};
use crate::renderer::{CapsetInfo, Renderer3d};
use crate::resource::{read_backing, write_backing};

/// Bytes per element the model assumes (BGRA scanout pixels, which is the
/// only content anything reads back out of it).
const BPE: usize = 4;

/// Largest host buffer one modeled resource may allocate (64 MiB — a 4K BGRA
/// frame is 33 MiB). Anything bigger is created *unbacked*: metadata is
/// tracked, transfers to it fail cleanly with `ERR_OUT_OF_MEMORY`.
const MAX_MODEL_BYTES: u64 = 64 << 20;

/// Capsets served: the same ids a virglrenderer host offers, with a blob of
/// zeroes — parseable shape, no capabilities claimed.
const CAPSETS: [CapsetInfo; 2] = [
    CapsetInfo {
        id: crate::CAPSET_VIRGL,
        max_version: 1,
        max_size: 308,
    },
    CapsetInfo {
        id: crate::CAPSET_VIRGL2,
        max_version: 2,
        max_size: 696,
    },
];

/// Capsets served in Venus mode ([`NullRenderer::with_venus`]): the virgl pair
/// plus [`crate::CAPSET_VENUS`].
///
/// The venus capset blob is `struct virgl_renderer_capset_venus`, which is
/// `{ u32 wire_format_version; u32 vk_xml_version; u32 vk_ext_command_serialization_spec_version; u32 vk_ms_command_serialization_spec_version; ... }`
/// — 32 bytes in the versions that matter. We serve zeroes of that length: a
/// guest that reads it sees wire format 0 and declines to use the context,
/// which is exactly right for a renderer that cannot execute Vulkan. The point
/// of the loopback is to prove the *plumbing* end to end, not to pretend.
const CAPSETS_VENUS: [CapsetInfo; 3] = [
    CAPSETS[0],
    CAPSETS[1],
    CapsetInfo {
        id: crate::CAPSET_VENUS,
        max_version: 0,
        max_size: VENUS_CAPSET_BYTES,
    },
];

/// Size of the venus capset blob (`struct virgl_renderer_capset_venus`).
const VENUS_CAPSET_BYTES: u32 = 32;

/// Size of the loopback host-visible window ([`NullRenderer::with_venus`]):
/// enough that the mapping bookkeeping is exercised with realistic offsets,
/// small enough that nothing is tempted to actually allocate it.
pub const NULL_HOST_VISIBLE_BYTES: u64 = 256 << 20;

#[derive(Debug, Default)]
struct ModelResource {
    width: u32,
    height: u32,
    /// Linear model storage, `width * height * depth * layers * 4` bytes, or
    /// empty when the resource was too large to model.
    data: Vec<u8>,
    backing: Vec<MemEntry>,
    mem: Option<Arc<GuestMem>>,
}

/// A blob the loopback renderer is holding on the host side.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ModelBlob {
    blob_mem: u32,
    blob_id: u64,
    size: u64,
    mapped_at: Option<u64>,
}

/// The portable no-op renderer. See the module docs.
#[derive(Debug, Default)]
pub struct NullRenderer {
    resources: HashMap<u32, ModelResource>,
    /// `SUBMIT_3D` streams executed, total commands seen — lets tests assert
    /// dispatch happened without pretending to rasterize.
    submits: u64,
    commands: u64,
    /// Venus/blob loopback mode ([`Self::with_venus`]).
    venus: bool,
    /// Host-side blobs, tracked so map/unmap bookkeeping is real.
    blobs: HashMap<u32, ModelBlob>,
    /// Contexts and the capset id each was created with, so a test can assert
    /// a venus context really arrived as a venus context.
    context_types: HashMap<u32, u32>,
}

impl NullRenderer {
    pub fn new() -> Self {
        Self::default()
    }

    /// The loopback **Venus** renderer (VEN-2002/VEN-2003): advertises the
    /// venus capset, accepts venus-typed contexts, serves all three blob
    /// memory types and owns a host-visible window it validates mappings
    /// against.
    ///
    /// It executes no Vulkan — nothing portable could. What it proves is that
    /// every byte of the path from a guest `RESOURCE_CREATE_BLOB` /
    /// `CTX_CREATE(venus)` / `RESOURCE_MAP_BLOB` to the renderer seam is
    /// wired, bounded and testable on a host with no GPU at all, Windows
    /// included.
    pub fn with_venus() -> Self {
        Self {
            venus: true,
            ..Self::default()
        }
    }

    /// Streams executed so far.
    pub fn submit_count(&self) -> u64 {
        self.submits
    }

    /// Commands decoded across all streams.
    pub fn command_count(&self) -> u64 {
        self.commands
    }

    /// Capset id context `ctx_id` was created with (0 = classic virgl).
    pub fn context_type(&self, ctx_id: u32) -> Option<u32> {
        self.context_types.get(&ctx_id).copied()
    }

    /// Live host-side blobs.
    pub fn blob_count(&self) -> usize {
        self.blobs.len()
    }

    /// `(blob_mem, blob_id, size, mapped_at)` of a host-side blob — what a
    /// test needs to assert the create/map path carried the guest's values all
    /// the way through the seam.
    pub fn blob_state(&self, resource_id: u32) -> Option<(u32, u64, u64, Option<u64>)> {
        self.blobs
            .get(&resource_id)
            .map(|b| (b.blob_mem, b.blob_id, b.size, b.mapped_at))
    }

    fn resource_mut(&mut self, id: u32) -> Result<&mut ModelResource, CommandError> {
        self.resources
            .get_mut(&id)
            .ok_or(CommandError::UnknownResource(id))
    }

    /// Linear byte range for a transfer touching `xfer.region` at level 0 of
    /// the model (whole rows of the region, laid out at the resource stride).
    ///
    /// Levels above 0 and volumetric slices are accepted but not modeled —
    /// they read/write nothing and succeed, matching "the renderer owns
    /// semantics".
    fn model_span(res: &ModelResource, xfer: &Transfer3d) -> Option<(usize, usize)> {
        if xfer.level != 0 || xfer.region.z != 0 {
            return None;
        }
        let stride = res.width as usize * BPE;
        let start = xfer.region.y as usize * stride + xfer.region.x as usize * BPE;
        let len = if xfer.region.x == 0 && xfer.region.w == res.width {
            // Whole rows: one contiguous span.
            stride * xfer.region.h as usize
        } else {
            // Partial rows: the model copies row 0 only (keeps the double —
            // and the fuzzer — simple; the box was already bounds-checked).
            xfer.region.w as usize * BPE
        };
        let end = start.checked_add(len)?;
        (end <= res.data.len()).then_some((start, end))
    }
}

impl Renderer3d for NullRenderer {
    fn capsets(&self) -> &[CapsetInfo] {
        if self.venus {
            &CAPSETS_VENUS
        } else {
            &CAPSETS
        }
    }

    fn capset(&mut self, id: u32, version: u32) -> Result<Vec<u8>, CommandError> {
        let info = self
            .capsets()
            .iter()
            .find(|c| c.id == id)
            .ok_or(CommandError::UnknownCapset { id, version })?;
        Ok(vec![0u8; info.max_size as usize])
    }

    fn ctx_create(&mut self, ctx_id: u32, capset_id: u32, _name: &str) -> Result<(), CommandError> {
        self.context_types.insert(ctx_id, capset_id);
        Ok(())
    }

    fn ctx_destroy(&mut self, ctx_id: u32) {
        self.context_types.remove(&ctx_id);
    }

    fn resource_create_3d(&mut self, args: &ResourceCreate3d) -> Result<(), CommandError> {
        let elements = u64::from(args.width)
            * u64::from(args.height)
            * u64::from(args.depth)
            * u64::from(args.array_size.max(1));
        let bytes = elements.saturating_mul(BPE as u64);
        let mut data = Vec::new();
        if bytes <= MAX_MODEL_BYTES {
            let len = usize::try_from(bytes).map_err(|_| CommandError::OutOfMemory)?;
            data.try_reserve_exact(len)
                .map_err(|_| CommandError::OutOfMemory)?;
            data.resize(len, 0);
        }
        self.resources.insert(
            args.resource_id,
            ModelResource {
                width: args.width,
                height: args.height,
                data,
                backing: Vec::new(),
                mem: None,
            },
        );
        Ok(())
    }

    fn resource_unref(&mut self, resource_id: u32) {
        self.resources.remove(&resource_id);
    }

    fn ctx_attach_resource(&mut self, _ctx_id: u32, _resource_id: u32) {}

    fn ctx_detach_resource(&mut self, _ctx_id: u32, _resource_id: u32) {}

    fn attach_backing(
        &mut self,
        resource_id: u32,
        mem: &Arc<GuestMem>,
        entries: &[MemEntry],
    ) -> Result<(), CommandError> {
        let res = self.resource_mut(resource_id)?;
        res.backing = entries.to_vec();
        res.mem = Some(Arc::clone(mem));
        Ok(())
    }

    fn detach_backing(&mut self, resource_id: u32) {
        if let Some(res) = self.resources.get_mut(&resource_id) {
            res.backing = Vec::new();
            res.mem = None;
        }
    }

    fn transfer_to_host(&mut self, _ctx_id: u32, xfer: &Transfer3d) -> Result<(), CommandError> {
        let res = self.resource_mut(xfer.resource_id)?;
        let Some((start, end)) = Self::model_span(res, xfer) else {
            return Ok(()); // Unmodeled shape: accepted, nothing stored.
        };
        let mem = Arc::clone(
            res.mem
                .as_ref()
                .ok_or(CommandError::NoBacking(xfer.resource_id))?,
        );
        let backing = std::mem::take(&mut res.backing);
        let dst = res
            .data
            .get_mut(start..end)
            .ok_or(CommandError::OutOfMemory)?;
        let outcome = read_backing(&mem, &backing, xfer.offset, dst);
        self.resource_mut(xfer.resource_id)?.backing = backing;
        outcome
    }

    fn transfer_from_host(&mut self, _ctx_id: u32, xfer: &Transfer3d) -> Result<(), CommandError> {
        let res = self.resource_mut(xfer.resource_id)?;
        let Some((start, end)) = Self::model_span(res, xfer) else {
            return Ok(());
        };
        let mem = Arc::clone(
            res.mem
                .as_ref()
                .ok_or(CommandError::NoBacking(xfer.resource_id))?,
        );
        let backing = std::mem::take(&mut res.backing);
        let src = res.data.get(start..end).ok_or(CommandError::OutOfMemory)?;
        let outcome = write_backing(&mem, &backing, xfer.offset, src);
        self.resource_mut(xfer.resource_id)?.backing = backing;
        outcome
    }

    fn submit(&mut self, _ctx_id: u32, stream: &[u8]) -> Result<(), CommandError> {
        // The stream was structurally validated by `Gpu3d`; count its
        // commands the same way the walk did.
        let mut at = 0usize;
        let mut seen = 0u64;
        while at + 4 <= stream.len() {
            let header =
                u32::from_le_bytes([stream[at], stream[at + 1], stream[at + 2], stream[at + 3]]);
            at += 4 + (header >> 16) as usize * 4;
            seen += 1;
        }
        self.submits += 1;
        self.commands = self.commands.saturating_add(seen);
        Ok(())
    }

    fn read_rect_bgra(
        &mut self,
        resource_id: u32,
        rect: Rect,
        out: &mut Vec<u8>,
    ) -> Result<(), CommandError> {
        let res = self
            .resources
            .get(&resource_id)
            .ok_or(CommandError::UnknownResource(resource_id))?;
        let stride = res.width as usize * BPE;
        let row_bytes = rect.width as usize * BPE;
        out.clear();
        out.try_reserve_exact(row_bytes * rect.height as usize)
            .map_err(|_| CommandError::OutOfMemory)?;
        for row in 0..rect.height {
            let start = (rect.y as usize + row as usize) * stride + rect.x as usize * BPE;
            let src = res.data.get(start..start.saturating_add(row_bytes)).ok_or(
                CommandError::RectOutOfBounds {
                    rect,
                    width: res.width,
                    height: res.height,
                },
            )?;
            out.extend_from_slice(src);
        }
        Ok(())
    }

    fn reset(&mut self) {
        self.resources.clear();
        self.blobs.clear();
        self.context_types.clear();
    }

    // ------------------------------------- blob resources (EPIC 20/VEN-2001)

    fn blob_support(&self) -> BlobSupport {
        if self.venus {
            BlobSupport {
                guest: true,
                host3d: true,
                host_visible_bytes: Some(NULL_HOST_VISIBLE_BYTES),
            }
        } else {
            BlobSupport::NONE
        }
    }

    fn create_blob(
        &mut self,
        args: &ResourceCreateBlob,
        _mem: &Arc<GuestMem>,
        _entries: &[MemEntry],
    ) -> Result<(), CommandError> {
        if !self.venus {
            return Err(CommandError::UnsupportedBlobMem(args.blob_mem));
        }
        // No host allocation: the loopback models a host blob as bookkeeping
        // only, which is what keeps this testable on a host with no GPU and
        // bounded by the device's own limits rather than by real memory.
        self.blobs.insert(
            args.resource_id,
            ModelBlob {
                blob_mem: args.blob_mem,
                blob_id: args.blob_id,
                size: args.size,
                mapped_at: None,
            },
        );
        Ok(())
    }

    fn destroy_blob(&mut self, resource_id: u32) {
        self.blobs.remove(&resource_id);
    }

    fn map_blob(
        &mut self,
        resource_id: u32,
        offset: u64,
        size: u64,
    ) -> Result<BlobMapping, CommandError> {
        if !self.venus {
            return Err(CommandError::NoHostVisibleWindow);
        }
        let blob = self
            .blobs
            .get_mut(&resource_id)
            .ok_or(CommandError::UnknownResource(resource_id))?;
        if size != blob.size {
            // The device computes the span from the blob's own size, so a
            // mismatch is a host bug, not a guest one — refuse rather than
            // map something the two sides disagree about.
            return Err(CommandError::Renderer(format!(
                "map of blob {resource_id} asked for {size} bytes, blob is {}",
                blob.size
            )));
        }
        blob.mapped_at = Some(offset);
        // Plain host RAM would be cached; a real host-visible GPU heap is
        // write-combining. The loopback holds no memory at all, so cached is
        // the truthful answer.
        Ok(BlobMapping::CACHED)
    }

    fn unmap_blob(&mut self, resource_id: u32, _offset: u64) {
        if let Some(blob) = self.blobs.get_mut(&resource_id) {
            blob.mapped_at = None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::Box3d;
    use crate::renderer::Gpu3d;
    use vm_memory::{Bytes, GuestAddress};

    fn create_args(id: u32, width: u32, height: u32) -> ResourceCreate3d {
        ResourceCreate3d {
            resource_id: id,
            target: 2,
            format: 2,
            bind: 1 << 18,
            width,
            height,
            depth: 1,
            array_size: 1,
            last_level: 0,
            nr_samples: 0,
            flags: 0,
        }
    }

    fn xfer(id: u32, w: u32, h: u32, offset: u64) -> Transfer3d {
        Transfer3d {
            region: Box3d {
                x: 0,
                y: 0,
                z: 0,
                w,
                h,
                d: 1,
            },
            offset,
            resource_id: id,
            level: 0,
            stride: 0,
            layer_stride: 0,
        }
    }

    #[test]
    fn pixels_round_trip_guest_to_host_and_back() {
        let mem = Arc::new(virtio_core::testing::guest_memory(1 << 20));
        let mut gpu = Gpu3d::new(Box::new(NullRenderer::new()));
        gpu.ctx_create(1, 0, "test").expect("ctx");
        gpu.resource_create(&create_args(5, 2, 2)).expect("create");
        let image: Vec<u8> = (0..16u8).collect();
        mem.write_slice(&image, GuestAddress(0x4000)).expect("seed");
        gpu.attach_backing(
            5,
            &mem,
            &[MemEntry {
                addr: 0x4000,
                length: 16,
            }],
        )
        .expect("attach");
        gpu.transfer(0, &xfer(5, 2, 2, 0), true).expect("to host");

        let mut out = Vec::new();
        gpu.read_rect_bgra(
            5,
            Rect {
                x: 0,
                y: 0,
                width: 2,
                height: 2,
            },
            &mut out,
        )
        .expect("readback");
        assert_eq!(out, image);

        // …and back into different guest pages.
        mem.write_slice(&[0u8; 16], GuestAddress(0x8000))
            .expect("clear");
        gpu.detach_backing(5).expect("detach");
        gpu.attach_backing(
            5,
            &mem,
            &[MemEntry {
                addr: 0x8000,
                length: 16,
            }],
        )
        .expect("re-attach");
        gpu.transfer(1, &xfer(5, 2, 2, 0), false)
            .expect("from host");
        let mut round = [0u8; 16];
        mem.read_slice(&mut round, GuestAddress(0x8000))
            .expect("read");
        assert_eq!(&round[..], image.as_slice());
    }

    #[test]
    fn the_validation_front_rejects_what_the_spec_says_it_must() {
        let mem = Arc::new(virtio_core::testing::guest_memory(1 << 20));
        let mut gpu = Gpu3d::new(Box::new(NullRenderer::new()));

        // Contexts: id 0, duplicates, unknown, the count cap.
        assert!(matches!(
            gpu.ctx_create(0, 0, ""),
            Err(CommandError::BadContextId(0))
        ));
        gpu.ctx_create(7, 0, "x").expect("ok");
        assert!(matches!(
            gpu.ctx_create(7, 0, "x"),
            Err(CommandError::BadContextId(7))
        ));
        // `context_init` names a context type by capset id (VEN-2002). This
        // renderer serves only virgl, so a venus context is refused — and so
        // is any reserved bit outside the capset-id mask.
        assert!(matches!(
            gpu.ctx_create(8, crate::CAPSET_VENUS, "venus"),
            Err(CommandError::UnsupportedContextType(_))
        ));
        assert!(matches!(
            gpu.ctx_create(8, 0x0100, ""),
            Err(CommandError::UnsupportedContextType(0x0100))
        ));
        // …while capset 1 (virgl) *is* advertised, so it is a legal type.
        gpu.ctx_create(8, crate::CAPSET_VIRGL, "virgl")
            .expect("virgl context");
        gpu.ctx_destroy(8).expect("destroy");
        assert!(matches!(
            gpu.ctx_destroy(9),
            Err(CommandError::UnknownContext(9))
        ));

        // Resources: zero id, duplicate, absurd geometry, elements budget.
        assert!(matches!(
            gpu.resource_create(&create_args(0, 2, 2)),
            Err(CommandError::ZeroResourceId)
        ));
        gpu.resource_create(&create_args(1, 4, 4)).expect("ok");
        assert!(matches!(
            gpu.resource_create(&create_args(1, 4, 4)),
            Err(CommandError::DuplicateResource(1))
        ));
        assert!(matches!(
            gpu.resource_create(&create_args(2, 0, 4)),
            Err(CommandError::BadGeometry3d)
        ));
        assert!(matches!(
            gpu.resource_create(&create_args(2, u32::MAX, u32::MAX)),
            Err(CommandError::BadGeometry3d)
        ));

        // Transfers: unknown ctx, unknown resource, no backing, bad box.
        assert!(matches!(
            gpu.transfer(99, &xfer(1, 2, 2, 0), true),
            Err(CommandError::UnknownContext(99))
        ));
        assert!(matches!(
            gpu.transfer(0, &xfer(66, 2, 2, 0), true),
            Err(CommandError::UnknownResource(66))
        ));
        assert!(matches!(
            gpu.transfer(0, &xfer(1, 2, 2, 0), true),
            Err(CommandError::NoBacking(1))
        ));
        gpu.attach_backing(
            1,
            &mem,
            &[MemEntry {
                addr: 0x4000,
                length: 64,
            }],
        )
        .expect("attach");
        assert!(matches!(
            gpu.transfer(0, &xfer(1, 5, 4, 0), true),
            Err(CommandError::BoxOutOfBounds { .. })
        ));
        assert!(matches!(
            gpu.transfer(0, &xfer(1, 4, 4, u64::MAX), true),
            Err(CommandError::ShortBacking { .. })
        ));

        // Submits: unknown ctx, oversized, malformed.
        assert!(matches!(
            gpu.submit(99, &[]),
            Err(CommandError::UnknownContext(99))
        ));
        let huge = vec![0u8; crate::renderer::MAX_SUBMIT_BYTES + 4];
        assert!(matches!(
            gpu.submit(7, &huge),
            Err(CommandError::StreamTooLarge(_))
        ));
        assert!(matches!(
            gpu.submit(7, &u32::MAX.to_le_bytes()),
            Err(CommandError::InvalidStream(_))
        ));
        gpu.submit(7, &[]).expect("empty stream is legal");

        // Capsets: both advertised ones resolve, garbage does not.
        assert_eq!(gpu.num_capsets(), 2);
        assert_eq!(gpu.capset_info(0).map(|c| c.id), Some(1));
        assert_eq!(gpu.capset_info(1).map(|c| c.id), Some(2));
        assert_eq!(gpu.capset_info(2), None);
        assert!(gpu.capset(1, 1).is_ok());
        assert!(matches!(
            gpu.capset(3, 0),
            Err(CommandError::UnknownCapset { .. })
        ));
        assert!(matches!(
            gpu.capset(1, 9),
            Err(CommandError::UnknownCapset { .. }),
        ));

        // ctx_resource needs both halves live.
        gpu.ctx_resource(7, 1, true).expect("attach ok");
        assert!(matches!(
            gpu.ctx_resource(7, 66, true),
            Err(CommandError::UnknownResource(66))
        ));
        assert!(matches!(
            gpu.ctx_resource(99, 1, true),
            Err(CommandError::UnknownContext(99))
        ));

        // Reset drops everything.
        gpu.reset();
        assert_eq!(gpu.context_count(), 0);
        assert_eq!(gpu.resource_count(), 0);
        assert!(matches!(
            gpu.submit(7, &[]),
            Err(CommandError::UnknownContext(7))
        ));
    }

    #[test]
    fn the_context_and_resource_caps_hold() {
        let mut gpu = Gpu3d::new(Box::new(NullRenderer::new()));
        for id in 1..=crate::renderer::MAX_CONTEXTS as u32 {
            gpu.ctx_create(id, 0, "").expect("under the cap");
        }
        assert!(matches!(
            gpu.ctx_create(u32::MAX, 0, ""),
            Err(CommandError::TooManyContexts)
        ));

        // The elements budget bites before the count cap for big resources.
        let mut gpu = Gpu3d::new(Box::new(NullRenderer::new()));
        let mut created = 0u32;
        loop {
            // 16M elements each (64 MiB model): the budget allows 64.
            match gpu.resource_create(&create_args(created + 1, 4096, 4096)) {
                Ok(()) => created += 1,
                Err(CommandError::OutOfMemory) => break,
                Err(other) => panic!("unexpected: {other}"),
            }
            assert!(created <= 4096, "budget never bit");
        }
        assert_eq!(
            u64::from(created) * 4096 * 4096,
            crate::renderer::MAX_TOTAL_3D_ELEMENTS,
            "the budget is exact"
        );
        // Freeing one makes room for one.
        gpu.resource_unref(1).expect("unref");
        gpu.resource_create(&create_args(9999, 4096, 4096))
            .expect("budget freed");
    }
}
