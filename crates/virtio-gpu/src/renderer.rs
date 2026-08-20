//! The 3D seam (ADR-0004, GPU-002…GPU-008): what the device needs from a host
//! renderer, and the guest-facing validation that stands in front of it.
//!
//! Everything in this module is **portable** — it builds and tests on every
//! host OS. The two implementations of [`Renderer3d`] are
//! [`crate::null_renderer::NullRenderer`] (portable, CPU-backed, used by unit
//! tests and the fuzzer) and `crate::virgl::VirglRenderer` (Linux, dlopen'd
//! libvirglrenderer).
//!
//! # The trust boundary
//!
//! The renderer behind the trait is *trusted* (for virglrenderer: like KVM —
//! a C component we hand validated input to). Everything in front of it is
//! not. [`Gpu3d`] is that front: it owns the id tables, the bounds and the
//! structural checks, so that by the time a trait method is called
//!
//! * context and resource ids exist in bounded, host-chosen tables — a guest
//!   id is a *name*, never an index;
//! * geometry was validated at create time and every transfer box was checked
//!   against it (mip level included) with u64 arithmetic;
//! * a `SUBMIT_3D` stream was length-walked ([`validate_stream`]) so its
//!   command headers cover the buffer exactly;
//! * every count and byte total sits under a named constant from the
//!   MVP-1407 table.
//!
//! A rule violation fails that one command in band (see
//! [`crate::CommandError`]); nothing here panics on guest input.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use virtio_core::GuestMem;

use crate::error::CommandError;
use crate::protocol::{Box3d, MemEntry, Rect, ResourceCreate3d, Transfer3d};

/// Most rendering contexts a guest may hold open at once. Each mesa process
/// in the guest costs one; GNOME plus a busy desktop is a few dozen.
pub const MAX_CONTEXTS: usize = 128;

/// Most live 3D resources across all contexts. mesa allocates one per
/// buffer/texture; a composited desktop holds thousands, not tens of
/// thousands.
pub const MAX_3D_RESOURCES: usize = 16384;

/// Largest `SUBMIT_3D` command stream accepted, in bytes. mesa's virgl driver
/// flushes at 16k dwords (64 KiB) by default; 1 MiB leaves room for enlarged
/// cmdbufs while bounding what one submit can stage.
pub const MAX_SUBMIT_BYTES: usize = 1 << 20;

/// Per-axis limit for texture height/depth, and the array-size limit —
/// matches the largest GL texture any plausible host offers.
pub const MAX_3D_DIM: u32 = 16384;

/// Width limit. Buffers travel as `width` bytes × 1 × 1, so this is also the
/// largest single buffer (256 MiB) a guest can ask the renderer to allocate.
pub const MAX_3D_WIDTH: u32 = 1 << 28;

/// Array-layer limit (`array_size`).
pub const MAX_3D_ARRAY: u32 = 2048;

/// Mip-level limit (`last_level`); 15 covers a 16384-wide chain.
pub const MAX_3D_LAST_LEVEL: u32 = 15;

/// MSAA sample-count limit (`nr_samples`).
pub const MAX_3D_SAMPLES: u32 = 32;

/// Total *elements* (width × height × depth × layers) across all live 3D
/// resources. The device cannot know bytes-per-element (formats are Gallium
/// enums the renderer owns), so this bounds the driving factor; at common
/// 4-byte formats it is ~4 GiB of host allocations. True VRAM accounting is
/// the renderer's (GPU-012 recovery is phase 2).
pub const MAX_TOTAL_3D_ELEMENTS: u64 = 1 << 30;

/// One capability set the renderer serves (`GET_CAPSET_INFO`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CapsetInfo {
    /// `VIRTIO_GPU_CAPSET_*`: 1 = VIRGL, 2 = VIRGL2.
    pub id: u32,
    pub max_version: u32,
    pub max_size: u32,
}

/// What the device records about a live 3D resource — enough to validate
/// transfer boxes and to feed the scanout/cursor readback path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Resource3dDesc {
    pub target: u32,
    pub format: u32,
    pub width: u32,
    pub height: u32,
    pub depth: u32,
    pub array_size: u32,
    pub last_level: u32,
}

impl Resource3dDesc {
    /// Element count (width × height × depth × layers) as a u64.
    fn elements(&self) -> u64 {
        u64::from(self.width)
            * u64::from(self.height)
            * u64::from(self.depth)
            * u64::from(self.array_size.max(1))
    }

    /// Extent of mip `level` along one axis (the standard GL halving).
    fn level_extent(base: u32, level: u32) -> u32 {
        base.checked_shr(level).unwrap_or(0).max(1)
    }

    /// True when `b` lies fully inside mip `level` of this resource.
    ///
    /// Array textures keep their layer count across levels; the wire encodes
    /// layers in either `depth` (3D textures shrink) or `array_size` (arrays
    /// do not), so the depth bound is the more permissive of the two — the
    /// renderer applies the exact per-target rule, this check only has to be
    /// sound as a host-side bound.
    pub fn box_fits(&self, b: &Box3d, level: u32) -> bool {
        if b.w == 0 || b.h == 0 || b.d == 0 || level > self.last_level {
            return false;
        }
        let w = u64::from(Self::level_extent(self.width, level));
        let h = u64::from(Self::level_extent(self.height, level));
        let d = u64::from(Self::level_extent(self.depth, level)).max(u64::from(self.array_size));
        u64::from(b.x) + u64::from(b.w) <= w
            && u64::from(b.y) + u64::from(b.h) <= h
            && u64::from(b.z) + u64::from(b.d) <= d
    }
}

/// What the virtio-gpu device needs from a host 3D renderer.
///
/// Callers (i.e. [`Gpu3d`]) have already validated ids, geometry, boxes and
/// stream structure; implementations own execution and may still fail any
/// call (host GL errors, renderer-side limits), which the device answers in
/// band.
pub trait Renderer3d: Send {
    /// The capability sets this renderer serves, in `capset_index` order.
    fn capsets(&self) -> &[CapsetInfo];

    /// One capset blob, already bounded by the matching `max_size`.
    fn capset(&mut self, id: u32, version: u32) -> Result<Vec<u8>, CommandError>;

    fn ctx_create(&mut self, ctx_id: u32, name: &str) -> Result<(), CommandError>;

    fn ctx_destroy(&mut self, ctx_id: u32);

    fn resource_create_3d(&mut self, args: &ResourceCreate3d) -> Result<(), CommandError>;

    fn resource_unref(&mut self, resource_id: u32);

    fn ctx_attach_resource(&mut self, ctx_id: u32, resource_id: u32);

    fn ctx_detach_resource(&mut self, ctx_id: u32, resource_id: u32);

    /// Hands the resource its guest backing pages. `mem` is the live guest
    /// memory; implementations that keep host pointers into it (virgl iovecs)
    /// must clone the `Arc` and hold it until [`Self::detach_backing`] /
    /// [`Self::resource_unref`] / [`Self::reset`].
    fn attach_backing(
        &mut self,
        resource_id: u32,
        mem: &Arc<GuestMem>,
        entries: &[MemEntry],
    ) -> Result<(), CommandError>;

    fn detach_backing(&mut self, resource_id: u32);

    fn transfer_to_host(&mut self, ctx_id: u32, xfer: &Transfer3d) -> Result<(), CommandError>;

    fn transfer_from_host(&mut self, ctx_id: u32, xfer: &Transfer3d) -> Result<(), CommandError>;

    /// Executes one validated command stream in `ctx_id`.
    fn submit(&mut self, ctx_id: u32, stream: &[u8]) -> Result<(), CommandError>;

    /// Reads `rect` (level 0, layer 0) of a resource as tightly packed BGRA
    /// rows into `out` (cleared and resized by the implementation) — the
    /// scanout/cursor readback path (GPU-010).
    fn read_rect_bgra(
        &mut self,
        resource_id: u32,
        rect: Rect,
        out: &mut Vec<u8>,
    ) -> Result<(), CommandError>;

    /// Drops every context and resource (device reset). Must be infallible
    /// and leave the renderer usable for a fresh driver.
    fn reset(&mut self);
}

/// Structural validation of a `SUBMIT_3D` stream (GPU-007).
///
/// The virgl wire format is a sequence of 32-bit headers, each followed by
/// `header >> 16` payload dwords. The walk must land exactly on the end of
/// the buffer: a stream whose last command overhangs, or whose byte length is
/// not dword-aligned, is malformed and never reaches the renderer. Command
/// *semantics* stay the renderer's business — this check makes "the guest
/// controls a length field" safe, not "the guest draws correctly".
pub fn validate_stream(stream: &[u8]) -> Result<(), CommandError> {
    if stream.len() % 4 != 0 {
        return Err(CommandError::InvalidStream(
            "stream length is not a whole number of dwords",
        ));
    }
    let mut at = 0usize;
    while at < stream.len() {
        let header = match stream.get(at..at + 4) {
            Some(raw) => u32::from_le_bytes([raw[0], raw[1], raw[2], raw[3]]),
            None => return Err(CommandError::InvalidStream("truncated command header")),
        };
        let payload_dwords = (header >> 16) as usize;
        // 4 header bytes + payload; cannot overflow usize (payload < 2^18).
        at += 4 + payload_dwords * 4;
    }
    if at != stream.len() {
        return Err(CommandError::InvalidStream(
            "last command overhangs the stream",
        ));
    }
    Ok(())
}

/// Per-resource state [`Gpu3d`] tracks alongside the renderer's own.
#[derive(Debug)]
struct TrackedResource {
    desc: Resource3dDesc,
    backing_len: u64,
}

/// The validation layer in front of a [`Renderer3d`] — see the module docs.
///
/// Owned by the device when 3D is enabled; every 3D command goes through
/// here, never straight to the trait.
pub struct Gpu3d {
    renderer: Box<dyn Renderer3d>,
    contexts: HashSet<u32>,
    resources: HashMap<u32, TrackedResource>,
    total_elements: u64,
}

impl std::fmt::Debug for Gpu3d {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Gpu3d")
            .field("contexts", &self.contexts.len())
            .field("resources", &self.resources.len())
            .field("total_elements", &self.total_elements)
            .finish()
    }
}

impl Gpu3d {
    pub fn new(renderer: Box<dyn Renderer3d>) -> Self {
        Self {
            renderer,
            contexts: HashSet::new(),
            resources: HashMap::new(),
            total_elements: 0,
        }
    }

    /// Number of capsets, for `struct virtio_gpu_config`.
    pub fn num_capsets(&self) -> u32 {
        u32::try_from(self.renderer.capsets().len()).unwrap_or(0)
    }

    /// `GET_CAPSET_INFO`: capset at `index`, or `None` past the end (the
    /// device answers that with zeroes, matching QEMU).
    pub fn capset_info(&self, index: u32) -> Option<CapsetInfo> {
        usize::try_from(index)
            .ok()
            .and_then(|i| self.renderer.capsets().get(i))
            .copied()
    }

    /// `GET_CAPSET`: the blob for `(id, version)` after checking the pair is
    /// one the renderer advertised.
    pub fn capset(&mut self, id: u32, version: u32) -> Result<Vec<u8>, CommandError> {
        let info = self
            .renderer
            .capsets()
            .iter()
            .find(|c| c.id == id && version <= c.max_version)
            .copied()
            .ok_or(CommandError::UnknownCapset { id, version })?;
        let blob = self.renderer.capset(id, version)?;
        if blob.len() > info.max_size as usize {
            // A renderer bug, not a guest error — but never ship the guest
            // more bytes than GET_CAPSET_INFO promised.
            return Err(CommandError::Renderer(format!(
                "capset {id} v{version} is {} bytes, advertised max {}",
                blob.len(),
                info.max_size
            )));
        }
        Ok(blob)
    }

    /// `CTX_CREATE` (GPU-004).
    pub fn ctx_create(
        &mut self,
        ctx_id: u32,
        context_init: u32,
        name: &str,
    ) -> Result<(), CommandError> {
        if ctx_id == 0 || self.contexts.contains(&ctx_id) {
            return Err(CommandError::BadContextId(ctx_id));
        }
        if self.contexts.len() >= MAX_CONTEXTS {
            return Err(CommandError::TooManyContexts);
        }
        // `context_init` selects a context type (Venus, drm) in newer specs;
        // classic virgl is type 0 and the only one phase 1 speaks.
        if context_init != 0 {
            return Err(CommandError::UnsupportedContextType(context_init));
        }
        self.renderer.ctx_create(ctx_id, name)?;
        self.contexts.insert(ctx_id);
        Ok(())
    }

    /// `CTX_DESTROY` (GPU-004).
    pub fn ctx_destroy(&mut self, ctx_id: u32) -> Result<(), CommandError> {
        if !self.contexts.remove(&ctx_id) {
            return Err(CommandError::UnknownContext(ctx_id));
        }
        self.renderer.ctx_destroy(ctx_id);
        Ok(())
    }

    /// True when `ctx_id` names a live rendering context.
    ///
    /// The device needs this on its own for the ids the *2D* table owns: the
    /// guest legally attaches a `RESOURCE_CREATE_2D` resource to a 3D context
    /// (the kernel does it for its console framebuffer), and that command still
    /// has to be refused for a context that does not exist.
    pub fn has_context(&self, ctx_id: u32) -> bool {
        self.contexts.contains(&ctx_id)
    }

    /// `CTX_ATTACH_RESOURCE` / `CTX_DETACH_RESOURCE` (GPU-006) for the ids this
    /// front owns. Ids owned by the device's 2D table never get here — the
    /// device answers those itself (see `GpuDevice::ctx_resource`).
    pub fn ctx_resource(
        &mut self,
        ctx_id: u32,
        resource_id: u32,
        attach: bool,
    ) -> Result<(), CommandError> {
        if !self.contexts.contains(&ctx_id) {
            return Err(CommandError::UnknownContext(ctx_id));
        }
        if !self.resources.contains_key(&resource_id) {
            return Err(CommandError::UnknownResource(resource_id));
        }
        if attach {
            self.renderer.ctx_attach_resource(ctx_id, resource_id);
        } else {
            self.renderer.ctx_detach_resource(ctx_id, resource_id);
        }
        Ok(())
    }

    /// `RESOURCE_CREATE_3D` (GPU-005). The caller has already ruled out a
    /// clash with a 2D resource id (one id namespace per device).
    pub fn resource_create(&mut self, args: &ResourceCreate3d) -> Result<(), CommandError> {
        if args.resource_id == 0 {
            return Err(CommandError::ZeroResourceId);
        }
        if self.resources.contains_key(&args.resource_id) {
            return Err(CommandError::DuplicateResource(args.resource_id));
        }
        if self.resources.len() >= MAX_3D_RESOURCES {
            return Err(CommandError::OutOfMemory);
        }
        if args.width == 0
            || args.height == 0
            || args.depth == 0
            || args.width > MAX_3D_WIDTH
            || args.height > MAX_3D_DIM
            || args.depth > MAX_3D_DIM
            || args.array_size > MAX_3D_ARRAY
            || args.last_level > MAX_3D_LAST_LEVEL
            || args.nr_samples > MAX_3D_SAMPLES
        {
            return Err(CommandError::BadGeometry3d);
        }
        let desc = Resource3dDesc {
            target: args.target,
            format: args.format,
            width: args.width,
            height: args.height,
            depth: args.depth,
            array_size: args.array_size,
            last_level: args.last_level,
        };
        let elements = desc.elements();
        if self.total_elements.saturating_add(elements) > MAX_TOTAL_3D_ELEMENTS {
            return Err(CommandError::OutOfMemory);
        }
        self.renderer.resource_create_3d(args)?;
        self.total_elements = self.total_elements.saturating_add(elements);
        self.resources.insert(
            args.resource_id,
            TrackedResource {
                desc,
                backing_len: 0,
            },
        );
        Ok(())
    }

    /// `RESOURCE_UNREF` for a 3D resource.
    pub fn resource_unref(&mut self, resource_id: u32) -> Result<(), CommandError> {
        let tracked = self
            .resources
            .remove(&resource_id)
            .ok_or(CommandError::UnknownResource(resource_id))?;
        self.total_elements = self.total_elements.saturating_sub(tracked.desc.elements());
        self.renderer.resource_unref(resource_id);
        Ok(())
    }

    /// True when `resource_id` names a live 3D resource.
    pub fn owns(&self, resource_id: u32) -> bool {
        self.resources.contains_key(&resource_id)
    }

    /// Geometry of a live 3D resource (scanout binding, cursor size checks).
    pub fn desc(&self, resource_id: u32) -> Option<Resource3dDesc> {
        self.resources.get(&resource_id).map(|r| r.desc)
    }

    /// `RESOURCE_ATTACH_BACKING` routed to a 3D resource. Entry count and
    /// command length were validated by the device (shared with the 2D path).
    pub fn attach_backing(
        &mut self,
        resource_id: u32,
        mem: &Arc<GuestMem>,
        entries: &[MemEntry],
    ) -> Result<(), CommandError> {
        let tracked = self
            .resources
            .get_mut(&resource_id)
            .ok_or(CommandError::UnknownResource(resource_id))?;
        self.renderer.attach_backing(resource_id, mem, entries)?;
        tracked.backing_len = entries
            .iter()
            .fold(0u64, |sum, e| sum.saturating_add(u64::from(e.length)));
        Ok(())
    }

    /// `RESOURCE_DETACH_BACKING` routed to a 3D resource.
    pub fn detach_backing(&mut self, resource_id: u32) -> Result<(), CommandError> {
        let tracked = self
            .resources
            .get_mut(&resource_id)
            .ok_or(CommandError::UnknownResource(resource_id))?;
        self.renderer.detach_backing(resource_id);
        tracked.backing_len = 0;
        Ok(())
    }

    /// `TRANSFER_TO_HOST_3D` / `TRANSFER_FROM_HOST_3D` (GPU-008).
    pub fn transfer(
        &mut self,
        ctx_id: u32,
        xfer: &Transfer3d,
        to_host: bool,
    ) -> Result<(), CommandError> {
        // ctx 0 is the kernel's own context and always valid (dumb-buffer
        // uploads arrive on it before any GL client exists).
        if ctx_id != 0 && !self.contexts.contains(&ctx_id) {
            return Err(CommandError::UnknownContext(ctx_id));
        }
        let tracked = self
            .resources
            .get(&xfer.resource_id)
            .ok_or(CommandError::UnknownResource(xfer.resource_id))?;
        if !tracked.desc.box_fits(&xfer.region, xfer.level) {
            return Err(CommandError::BoxOutOfBounds {
                b: xfer.region,
                level: xfer.level,
            });
        }
        if tracked.backing_len == 0 {
            return Err(CommandError::NoBacking(xfer.resource_id));
        }
        if xfer.offset >= tracked.backing_len {
            return Err(CommandError::ShortBacking {
                need: xfer.offset,
                have: tracked.backing_len,
            });
        }
        if to_host {
            self.renderer.transfer_to_host(ctx_id, xfer)
        } else {
            self.renderer.transfer_from_host(ctx_id, xfer)
        }
    }

    /// `SUBMIT_3D` (GPU-007): bound, structurally validate, dispatch.
    pub fn submit(&mut self, ctx_id: u32, stream: &[u8]) -> Result<(), CommandError> {
        if !self.contexts.contains(&ctx_id) {
            return Err(CommandError::UnknownContext(ctx_id));
        }
        if stream.len() > MAX_SUBMIT_BYTES {
            return Err(CommandError::StreamTooLarge(stream.len()));
        }
        validate_stream(stream)?;
        self.renderer.submit(ctx_id, stream)
    }

    /// Scanout/cursor readback (GPU-010): `rect` of level 0 as packed BGRA.
    pub fn read_rect_bgra(
        &mut self,
        resource_id: u32,
        rect: Rect,
        out: &mut Vec<u8>,
    ) -> Result<(), CommandError> {
        let tracked = self
            .resources
            .get(&resource_id)
            .ok_or(CommandError::UnknownResource(resource_id))?;
        if !rect.fits_within(tracked.desc.width, tracked.desc.height) {
            return Err(CommandError::RectOutOfBounds {
                rect,
                width: tracked.desc.width,
                height: tracked.desc.height,
            });
        }
        self.renderer.read_rect_bgra(resource_id, rect, out)
    }

    /// Device reset: drop every context and resource, renderer included.
    pub fn reset(&mut self) {
        self.renderer.reset();
        self.contexts.clear();
        self.resources.clear();
        self.total_elements = 0;
    }

    /// Live contexts (diagnostics).
    pub fn context_count(&self) -> usize {
        self.contexts.len()
    }

    /// Live 3D resources (diagnostics).
    pub fn resource_count(&self) -> usize {
        self.resources.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stream_of(commands: &[(u16, &[u32])]) -> Vec<u8> {
        let mut out = Vec::new();
        for (id, payload) in commands {
            let header = (payload.len() as u32) << 16 | u32::from(*id);
            out.extend_from_slice(&header.to_le_bytes());
            for dw in *payload {
                out.extend_from_slice(&dw.to_le_bytes());
            }
        }
        out
    }

    #[test]
    fn valid_streams_walk_to_exactly_the_end() {
        validate_stream(&[]).expect("empty stream is fine");
        let s = stream_of(&[(1, &[7, 8, 9]), (2, &[]), (3, &[0])]);
        validate_stream(&s).expect("well-formed stream");
    }

    #[test]
    fn malformed_streams_are_rejected_not_panicked_on() {
        // Not dword-aligned.
        assert!(matches!(
            validate_stream(&[0, 1, 2]),
            Err(CommandError::InvalidStream(_))
        ));
        // Header claims more payload than the buffer holds.
        let mut s = stream_of(&[(1, &[1, 2])]);
        s.truncate(8);
        assert!(matches!(
            validate_stream(&s),
            Err(CommandError::InvalidStream(_))
        ));
        // Maximum length field on a tiny buffer must not wrap.
        let evil = u32::MAX.to_le_bytes();
        assert!(matches!(
            validate_stream(&evil),
            Err(CommandError::InvalidStream(_))
        ));
    }

    #[test]
    fn box_bounds_respect_mip_levels_and_reject_overflow() {
        let desc = Resource3dDesc {
            target: 2,
            format: 1,
            width: 1024,
            height: 512,
            depth: 1,
            array_size: 1,
            last_level: 2,
        };
        let full = Box3d {
            x: 0,
            y: 0,
            z: 0,
            w: 1024,
            h: 512,
            d: 1,
        };
        assert!(desc.box_fits(&full, 0));
        assert!(!desc.box_fits(&full, 1), "level 1 is 512x256");
        let level1 = Box3d {
            w: 512,
            h: 256,
            ..full
        };
        assert!(desc.box_fits(&level1, 1));
        assert!(!desc.box_fits(&level1, 3), "past last_level");
        // Empty and overflowing boxes.
        assert!(!desc.box_fits(&Box3d { w: 0, ..full }, 0));
        let evil = Box3d {
            x: u32::MAX,
            w: 2,
            ..full
        };
        assert!(!desc.box_fits(&evil, 0));
        // Array layers survive mipping.
        let array = Resource3dDesc {
            array_size: 6,
            last_level: 1,
            ..desc
        };
        let layered = Box3d {
            w: 512,
            h: 256,
            d: 6,
            ..full
        };
        assert!(array.box_fits(&layered, 1));
    }
}
