//! Blob resources and the host-visible mapping window (EPIC 20, VEN-2001).
//!
//! A *blob* is a virtio-gpu resource with **no format, no geometry and no
//! pixels** — just a size and a memory type. That is precisely what Venus
//! needs: the guest's mesa `venus` driver puts its command ring, its reply
//! shmem and its `VkDeviceMemory` allocations into blobs, and the device is
//! not supposed to understand any of it.
//!
//! Everything in this module is **portable**: it builds and is unit-tested on
//! every host OS, because it is all bookkeeping and bounds. The half that
//! cannot be portable — actually placing host pages into the guest's
//! shared-memory window — lives behind
//! [`Renderer3d::map_blob`](crate::renderer::Renderer3d::map_blob).
//!
//! # The three memory types, and what each one costs us
//!
//! | `blob_mem` | who owns the memory | what the device does |
//! |---|---|---|
//! | [`BLOB_MEM_GUEST`](crate::protocol::BLOB_MEM_GUEST) | the guest | records the page list; no host allocation at all |
//! | [`BLOB_MEM_HOST3D`](crate::protocol::BLOB_MEM_HOST3D) | the renderer, named by `blob_id` | forwards `blob_id` to the renderer; no guest pages |
//! | [`BLOB_MEM_HOST3D_GUEST`](crate::protocol::BLOB_MEM_HOST3D_GUEST) | both | both of the above; transfers shadow between them |
//!
//! A guest blob is the cheap and important one — it is the venus ring — and it
//! needs no renderer support whatsoever. The two host3d types need a renderer
//! that can name its own allocations, which is [`BlobSupport`].
//!
//! # The untrusted-guest rules this module enforces
//!
//! The guest picks the resource id, the memory type, the flag bits, the entry
//! count, every entry address and length, the blob size, the `blob_id` and —
//! for `RESOURCE_MAP_BLOB` — the *offset inside the host's shared-memory
//! window*. That last one is the sharpest edge in the epic: it is a
//! guest-chosen offset into a host mapping. So:
//!
//! * every count and total is bounded before anything is allocated
//!   ([`MAX_BLOB_RESOURCES`], [`MAX_BLOB_ENTRIES`], [`MAX_BLOB_BYTES`],
//!   [`MAX_TOTAL_BLOB_BYTES`]), with `try_reserve_exact` for the one growable
//!   list;
//! * `size` must be non-zero and a whole number of [`BLOB_PAGE_SIZE`] pages —
//!   the guest is going to `mmap` it, so a partial page is meaningless and an
//!   unaligned one would let a mapping straddle a page the device did not
//!   account for;
//! * a map offset is checked with `u64` arithmetic against the window length
//!   **and** against every live mapping ([`HostVisibleWindow`]), so two blobs
//!   can never overlap in the window and no mapping can run off its end;
//! * unknown flag bits and unknown memory types are refused in band rather
//!   than ignored, because ignoring them hands the guest a resource that
//!   silently cannot do what it asked for.

use std::collections::BTreeMap;
use std::collections::HashMap;

use crate::error::CommandError;
use crate::protocol::{
    MemEntry, BLOB_FLAG_MASK, BLOB_FLAG_USE_CROSS_DEVICE, BLOB_FLAG_USE_MAPPABLE, BLOB_MEM_GUEST,
    BLOB_MEM_HOST3D, BLOB_MEM_HOST3D_GUEST, MAP_CACHE_CACHED, MAP_CACHE_MASK, MAP_CACHE_WC,
};

/// Granularity of everything in the host-visible window. Blob sizes and map
/// offsets are multiples of this, because the guest maps the window with its
/// own page tables and x86-64 Linux' page is 4 KiB.
pub const BLOB_PAGE_SIZE: u64 = 4096;

/// Most live blob resources. Venus allocates one per `VkDeviceMemory` plus a
/// handful of rings per context; a busy Vulkan application holds hundreds.
pub const MAX_BLOB_RESOURCES: usize = 4096;

/// Largest single blob, in bytes. A Vulkan heap allocation for a 4K
/// framebuffer set is tens of MiB; 1 GiB is far past anything legitimate and
/// still expressible in the window.
pub const MAX_BLOB_BYTES: u64 = 1 << 30;

/// Total size of all live blobs. Bounds what one guest can make the host
/// promise across every blob at once.
pub const MAX_TOTAL_BLOB_BYTES: u64 = 4 << 30;

/// Entries in one `RESOURCE_CREATE_BLOB` page list. The same bound the 2D
/// attach-backing path uses, restated here because the command is a different
/// one and a reader should not have to guess which limit applies.
pub const MAX_BLOB_ENTRIES: u32 = 16 * 1024;

/// Live mappings the host-visible window tracks at once. Each is an entry in
/// an ordered map, so this bounds the bookkeeping a guest can force by mapping
/// thousands of tiny blobs.
pub const MAX_HOST_VISIBLE_MAPPINGS: usize = 4096;

/// Which blob memory types a renderer can serve, and whether it has a
/// host-visible window to map them into (VEN-2001).
///
/// The default is "none of them", which is what makes
/// [`VIRTIO_GPU_F_RESOURCE_BLOB`](crate::VIRTIO_GPU_F_RESOURCE_BLOB) an
/// opt-in: a device whose renderer says nothing here does not offer the
/// feature, and a 2D-only device never has one at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct BlobSupport {
    /// `true` when [`BLOB_MEM_GUEST`] blobs are accepted. Costs a renderer
    /// nothing (the pages are the guest's), so any renderer that speaks blob
    /// at all sets it.
    pub guest: bool,
    /// `true` when the renderer can name its own allocations by `blob_id`,
    /// i.e. [`BLOB_MEM_HOST3D`] and [`BLOB_MEM_HOST3D_GUEST`].
    pub host3d: bool,
    /// Length of the shared-memory window this renderer can place mappings
    /// into, or `None` when it has none — and then `RESOURCE_MAP_BLOB` is
    /// refused in band, which is honest rather than silently broken.
    pub host_visible_bytes: Option<u64>,
}

impl BlobSupport {
    /// Nothing supported: no blob feature bit is offered.
    pub const NONE: Self = Self {
        guest: false,
        host3d: false,
        host_visible_bytes: None,
    };

    /// True when the device should offer `VIRTIO_GPU_F_RESOURCE_BLOB` at all.
    pub fn any(&self) -> bool {
        self.guest || self.host3d
    }

    /// Whether `blob_mem` is one this renderer serves.
    pub fn accepts(&self, blob_mem: u32) -> bool {
        match blob_mem {
            BLOB_MEM_GUEST => self.guest,
            BLOB_MEM_HOST3D | BLOB_MEM_HOST3D_GUEST => self.host3d,
            _ => false,
        }
    }
}

/// What a renderer reports back from
/// [`Renderer3d::map_blob`](crate::renderer::Renderer3d::map_blob): the
/// caching the guest must use for the mapping it is about to make.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BlobMapping {
    /// `VIRTIO_GPU_MAP_CACHE_*` — the low nibble of the `map_info` the guest
    /// receives. Anything outside [`MAP_CACHE_MASK`] is masked off before it
    /// reaches the wire.
    pub map_info: u32,
}

impl BlobMapping {
    /// Write-combining, which is what a host GPU's host-visible heap almost
    /// always wants.
    pub const WC: Self = Self {
        map_info: MAP_CACHE_WC,
    };

    /// Ordinary cached memory — correct for a shadow in plain host RAM.
    pub const CACHED: Self = Self {
        map_info: MAP_CACHE_CACHED,
    };

    /// The value that actually goes on the wire.
    pub fn wire(&self) -> u32 {
        self.map_info & MAP_CACHE_MASK
    }
}

/// One live blob resource.
#[derive(Debug)]
pub struct BlobResource {
    id: u32,
    blob_mem: u32,
    blob_flags: u32,
    blob_id: u64,
    size: u64,
    /// Guest pages, for the two types that have them. Empty otherwise.
    backing: Vec<MemEntry>,
    backing_len: u64,
    /// Offset inside the host-visible window while mapped.
    mapped_at: Option<u64>,
}

impl BlobResource {
    pub fn id(&self) -> u32 {
        self.id
    }

    pub fn blob_mem(&self) -> u32 {
        self.blob_mem
    }

    pub fn blob_flags(&self) -> u32 {
        self.blob_flags
    }

    pub fn blob_id(&self) -> u64 {
        self.blob_id
    }

    pub fn size(&self) -> u64 {
        self.size
    }

    /// The guest page list, empty for a pure host3d blob.
    pub fn backing(&self) -> &[MemEntry] {
        &self.backing
    }

    /// Sum of the backing entry lengths (saturating).
    pub fn backing_len(&self) -> u64 {
        self.backing_len
    }

    /// Whether the guest asked for this blob to be mappable.
    pub fn is_mappable(&self) -> bool {
        self.blob_flags & BLOB_FLAG_USE_MAPPABLE != 0
    }

    /// Whether the blob's bytes live on the host (renderer) side.
    pub fn is_host(&self) -> bool {
        matches!(self.blob_mem, BLOB_MEM_HOST3D | BLOB_MEM_HOST3D_GUEST)
    }

    /// Where in the host-visible window this blob is mapped, if it is.
    pub fn mapped_at(&self) -> Option<u64> {
        self.mapped_at
    }
}

/// The host-visible mapping window: a guest-addressable range of the device's
/// shared-memory region, and which blob occupies which part of it.
///
/// The guest names the offset, so this is the allocator's *validator*, not its
/// allocator — placement is the driver's business (Linux' `virtio_gpu` carves
/// the region with its own `drm_mm`), and the device's job is to make sure
/// what it is told is inside the window and does not collide with a live
/// mapping. Getting that wrong would let one guest mapping alias another's
/// host memory.
#[derive(Default)]
pub struct HostVisibleWindow {
    len: u64,
    /// Offset → (length, resource id), ordered so overlap is a neighbour check
    /// rather than a scan.
    mappings: BTreeMap<u64, (u64, u32)>,
    /// The host pages the guest actually maps, once the machine layer has
    /// allocated them (VEN-2001 phase 2).
    ///
    /// `None` is phase 1: the window is a validator with nothing behind it, so
    /// the bookkeeping is exercisable — and fuzzable — on any host, and a
    /// machine that cannot back a window publishes no region at all. `Some` is
    /// what makes a mapping reach a guest.
    backing: Option<std::sync::Arc<dyn virtio_core::ShmBacking>>,
}

impl std::fmt::Debug for HostVisibleWindow {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HostVisibleWindow")
            .field("len", &self.len)
            .field("mappings", &self.mappings.len())
            .field("backed", &self.backing.is_some())
            .finish()
    }
}

impl HostVisibleWindow {
    /// A window of `len` bytes. `len` is a host constant, not guest input.
    pub fn new(len: u64) -> Self {
        Self {
            len,
            mappings: BTreeMap::new(),
            backing: None,
        }
    }

    /// Attaches the host pages behind this window.
    ///
    /// Refused — and the window left unbacked — when the backing is not
    /// exactly as long as the region the device *declared*, because that
    /// length is the one the guest was told and the one every offset is
    /// checked against. A backing shorter than the declaration would turn a
    /// validated offset into an out-of-bounds host access; a longer one would
    /// hide host memory the guest was never promised inside a region it can
    /// map.
    pub fn set_backing(&mut self, backing: std::sync::Arc<dyn virtio_core::ShmBacking>) -> bool {
        if backing.len() != self.len || self.len == 0 {
            tracing::error!(
                declared = self.len,
                backing = backing.len(),
                "refusing a shared-memory backing whose length is not the declared one"
            );
            return false;
        }
        self.backing = Some(backing);
        true
    }

    /// The host pages behind this window, if the machine could back it.
    pub fn backing(&self) -> Option<&std::sync::Arc<dyn virtio_core::ShmBacking>> {
        self.backing.as_ref()
    }

    /// Whether a mapping made in this window can actually reach a guest.
    pub fn is_backed(&self) -> bool {
        self.backing.is_some()
    }

    /// Clears `[offset, size)` so a guest never sees what the previous owner of
    /// that span left there.
    ///
    /// Called on the way *in* rather than on the way out: a blob that is
    /// unmapped and never re-mapped would otherwise leave its bytes in the
    /// window for the next resource, and doing it on entry means a host that
    /// crashed between the two still cannot leak. The span has already been
    /// validated against the window and every live mapping.
    pub fn clear_span(&self, offset: u64, size: u64) -> Result<(), CommandError> {
        let Some(backing) = &self.backing else {
            return Ok(());
        };
        backing.fill(offset, size, 0).map_err(|error| {
            tracing::error!(%error, "cannot clear a shared-memory span for a new mapping");
            CommandError::BadBlobMapping { offset, size }
        })
    }

    /// Length of the window in bytes.
    pub fn len(&self) -> u64 {
        self.len
    }

    /// True when the window has no room at all (no renderer window).
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Live mappings.
    pub fn mapping_count(&self) -> usize {
        self.mappings.len()
    }

    /// Which resource, if any, holds `offset`.
    pub fn resource_at(&self, offset: u64) -> Option<u32> {
        self.mappings
            .range(..=offset)
            .next_back()
            .filter(|(start, (len, _))| offset < start.saturating_add(*len))
            .map(|(_, (_, id))| *id)
    }

    /// Reserves `[offset, offset + size)` for `resource_id`.
    ///
    /// Every bound the guest could push on is checked here, in `u64`: the
    /// alignment of both ends, the sum against the window length (no wrap),
    /// the mapping count, and overlap with the mapping below and the mapping
    /// above.
    pub fn reserve(
        &mut self,
        resource_id: u32,
        offset: u64,
        size: u64,
    ) -> Result<(), CommandError> {
        if self.len == 0 {
            return Err(CommandError::NoHostVisibleWindow);
        }
        if size == 0 || offset % BLOB_PAGE_SIZE != 0 {
            return Err(CommandError::BadBlobMapping { offset, size });
        }
        let end = offset
            .checked_add(size)
            .ok_or(CommandError::BadBlobMapping { offset, size })?;
        if end > self.len {
            return Err(CommandError::BadBlobMapping { offset, size });
        }
        if self.mappings.len() >= MAX_HOST_VISIBLE_MAPPINGS {
            return Err(CommandError::OutOfMemory);
        }
        // The mapping starting at or below `offset` must end at or before it…
        if let Some((start, (len, _))) = self.mappings.range(..=offset).next_back() {
            if start.saturating_add(*len) > offset {
                return Err(CommandError::BlobMappingOverlap { offset, size });
            }
        }
        // …and the next one up must start at or after our end.
        if let Some((start, _)) = self.mappings.range(offset..).next() {
            if *start < end {
                return Err(CommandError::BlobMappingOverlap { offset, size });
            }
        }
        self.mappings.insert(offset, (size, resource_id));
        Ok(())
    }

    /// Releases the mapping at `offset` if it belongs to `resource_id`.
    pub fn release(&mut self, resource_id: u32, offset: u64) {
        if self
            .mappings
            .get(&offset)
            .is_some_and(|(_, id)| *id == resource_id)
        {
            self.mappings.remove(&offset);
        }
    }

    /// Drops every mapping (device reset).
    pub fn clear(&mut self) {
        self.mappings.clear();
    }
}

/// The bounded table of live blob resources.
///
/// Kept separate from [`crate::ResourceTable`] (2D, has pixels) and from
/// [`crate::renderer::Gpu3d`]'s table (3D, has geometry) because a blob has
/// neither, and because the one id namespace is routed *by which table owns
/// the id* — the rule the rest of the device already follows.
#[derive(Debug, Default)]
pub struct BlobTable {
    resources: HashMap<u32, BlobResource>,
    total_bytes: u64,
    window: HostVisibleWindow,
}

impl BlobTable {
    /// A table whose mappings go into a `window_bytes`-long host-visible
    /// window. Zero means the host has none and `RESOURCE_MAP_BLOB` is refused.
    pub fn new(window_bytes: u64) -> Self {
        Self {
            resources: HashMap::new(),
            total_bytes: 0,
            window: HostVisibleWindow::new(window_bytes),
        }
    }

    pub fn len(&self) -> usize {
        self.resources.len()
    }

    pub fn is_empty(&self) -> bool {
        self.resources.is_empty()
    }

    /// Bytes promised across every live blob.
    pub fn total_bytes(&self) -> u64 {
        self.total_bytes
    }

    /// The host-visible window, for diagnostics and tests.
    pub fn window(&self) -> &HostVisibleWindow {
        &self.window
    }

    /// The host-visible window, mutably, so the machine layer's backing can be
    /// attached after construction (VEN-2001 phase 2).
    pub fn window_mut(&mut self) -> &mut HostVisibleWindow {
        &mut self.window
    }

    /// True when `id` names a live blob.
    pub fn owns(&self, id: u32) -> bool {
        self.resources.contains_key(&id)
    }

    pub fn get(&self, id: u32) -> Option<&BlobResource> {
        self.resources.get(&id)
    }

    /// Validates the *shape* of a create request without touching the table —
    /// the checks that must pass before a renderer is asked to allocate
    /// anything.
    ///
    /// Split out from [`Self::insert`] so the device can validate, then call
    /// the renderer, then commit: a renderer failure must not leave a
    /// half-created blob behind.
    pub fn validate(
        &self,
        args: &crate::protocol::ResourceCreateBlob,
        support: BlobSupport,
        entries: &[MemEntry],
    ) -> Result<u64, CommandError> {
        if args.resource_id == 0 {
            return Err(CommandError::ZeroResourceId);
        }
        if self.resources.contains_key(&args.resource_id) {
            return Err(CommandError::DuplicateResource(args.resource_id));
        }
        if self.resources.len() >= MAX_BLOB_RESOURCES {
            return Err(CommandError::OutOfMemory);
        }
        if !support.accepts(args.blob_mem) {
            return Err(CommandError::UnsupportedBlobMem(args.blob_mem));
        }
        if args.blob_flags & !BLOB_FLAG_MASK != 0 {
            return Err(CommandError::BadBlobFlags(args.blob_flags));
        }
        // We have no dmabuf export on either host (ADR-0004 phase 2's probe),
        // so promising cross-device sharing would be a lie the guest acts on.
        if args.blob_flags & BLOB_FLAG_USE_CROSS_DEVICE != 0 {
            return Err(CommandError::BadBlobFlags(args.blob_flags));
        }
        if args.blob_flags & BLOB_FLAG_USE_MAPPABLE != 0 && support.host_visible_bytes.is_none() {
            // A guest asking for a mappable blob on a host with no window gets
            // told now, at create time, rather than at map time when it has
            // already built a Vulkan allocation around it.
            return Err(CommandError::NoHostVisibleWindow);
        }
        if args.size == 0 || args.size % BLOB_PAGE_SIZE != 0 || args.size > MAX_BLOB_BYTES {
            return Err(CommandError::BadBlobSize(args.size));
        }
        if self.total_bytes.saturating_add(args.size) > MAX_TOTAL_BLOB_BYTES {
            return Err(CommandError::OutOfMemory);
        }

        let wants_pages = matches!(args.blob_mem, BLOB_MEM_GUEST | BLOB_MEM_HOST3D_GUEST);
        if wants_pages && entries.is_empty() {
            return Err(CommandError::NoEntries);
        }
        if !wants_pages && !entries.is_empty() {
            // A pure host3d blob has no guest pages by definition; a page list
            // on one means the guest and the device disagree about what was
            // just created.
            return Err(CommandError::BadBlobMem {
                blob_mem: args.blob_mem,
                reason: "a HOST3D blob carries no guest page list",
            });
        }
        let backing_len = entries
            .iter()
            .fold(0u64, |sum, e| sum.saturating_add(u64::from(e.length)));
        if wants_pages && backing_len < args.size {
            return Err(CommandError::ShortBacking {
                need: args.size,
                have: backing_len,
            });
        }
        Ok(backing_len)
    }

    /// Commits a validated create. `backing_len` comes from [`Self::validate`].
    pub fn insert(
        &mut self,
        args: &crate::protocol::ResourceCreateBlob,
        entries: &[MemEntry],
        backing_len: u64,
    ) -> Result<(), CommandError> {
        let mut backing = Vec::new();
        backing
            .try_reserve_exact(entries.len())
            .map_err(|_| CommandError::OutOfMemory)?;
        backing.extend_from_slice(entries);
        self.total_bytes = self.total_bytes.saturating_add(args.size);
        self.resources.insert(
            args.resource_id,
            BlobResource {
                id: args.resource_id,
                blob_mem: args.blob_mem,
                blob_flags: args.blob_flags,
                blob_id: args.blob_id,
                size: args.size,
                backing,
                backing_len,
                mapped_at: None,
            },
        );
        Ok(())
    }

    /// `RESOURCE_UNREF` on a blob: drops it and releases any window mapping.
    /// Returns the window offset it had been mapped at, so the caller can tell
    /// the renderer to tear that host mapping down — a guest is allowed to
    /// unref a blob it never unmapped.
    pub fn remove(&mut self, id: u32) -> Result<Option<u64>, CommandError> {
        let blob = self
            .resources
            .remove(&id)
            .ok_or(CommandError::UnknownResource(id))?;
        self.total_bytes = self.total_bytes.saturating_sub(blob.size);
        if let Some(offset) = blob.mapped_at {
            self.window.release(id, offset);
        }
        Ok(blob.mapped_at)
    }

    /// `RESOURCE_MAP_BLOB`: reserve the guest-named span of the window.
    ///
    /// Returns the size that was reserved, so the caller can hand the renderer
    /// the exact span it must back. Does *not* call the renderer — the caller
    /// does that and then either commits or rolls back with
    /// [`Self::unreserve`], so a renderer failure leaves no phantom mapping.
    pub fn reserve_mapping(&mut self, id: u32, offset: u64) -> Result<u64, CommandError> {
        let blob = self
            .resources
            .get(&id)
            .ok_or(CommandError::UnknownResource(id))?;
        if !blob.is_mappable() {
            return Err(CommandError::BlobNotMappable(id));
        }
        if blob.mapped_at.is_some() {
            return Err(CommandError::BlobAlreadyMapped(id));
        }
        let size = blob.size;
        self.window.reserve(id, offset, size)?;
        // The span is the guest's to read the moment the mapping exists, and
        // the host pages behind it were last some other blob's. Clearing is
        // therefore part of reserving, not something a caller may forget: a
        // `reserve_mapping` that returned `Ok` has handed out a zeroed span.
        // A window with no host memory behind it is a no-op here, which is
        // exactly what phase 1 did.
        if let Err(error) = self.window.clear_span(offset, size) {
            self.window.release(id, offset);
            return Err(error);
        }
        Ok(size)
    }

    /// Records a mapping the renderer accepted.
    pub fn commit_mapping(&mut self, id: u32, offset: u64) {
        if let Some(blob) = self.resources.get_mut(&id) {
            blob.mapped_at = Some(offset);
        }
    }

    /// Rolls a reservation back after the renderer refused it.
    pub fn unreserve(&mut self, id: u32, offset: u64) {
        self.window.release(id, offset);
    }

    /// `RESOURCE_UNMAP_BLOB`: releases the window span. Returns the offset
    /// that was freed so the caller can tell the renderer.
    pub fn unmap(&mut self, id: u32) -> Result<u64, CommandError> {
        let blob = self
            .resources
            .get_mut(&id)
            .ok_or(CommandError::UnknownResource(id))?;
        let offset = blob
            .mapped_at
            .take()
            .ok_or(CommandError::BlobNotMapped(id))?;
        self.window.release(id, offset);
        Ok(offset)
    }

    /// Device reset: forget every blob and every mapping.
    pub fn clear(&mut self) {
        self.resources.clear();
        self.total_bytes = 0;
        self.window.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{ResourceCreateBlob, BLOB_FLAG_USE_SHAREABLE};

    fn support() -> BlobSupport {
        BlobSupport {
            guest: true,
            host3d: true,
            host_visible_bytes: Some(64 << 20),
        }
    }

    fn guest_blob(id: u32, size: u64) -> ResourceCreateBlob {
        ResourceCreateBlob {
            resource_id: id,
            blob_mem: BLOB_MEM_GUEST,
            blob_flags: BLOB_FLAG_USE_SHAREABLE,
            nr_entries: 1,
            blob_id: 0,
            size,
        }
    }

    fn pages(bytes: u64) -> Vec<MemEntry> {
        vec![MemEntry {
            addr: 0x1_0000,
            length: u32::try_from(bytes).unwrap_or(u32::MAX),
        }]
    }

    fn create(
        table: &mut BlobTable,
        args: &ResourceCreateBlob,
        entries: &[MemEntry],
    ) -> Result<(), CommandError> {
        let len = table.validate(args, support(), entries)?;
        table.insert(args, entries, len)
    }

    #[test]
    fn a_guest_blob_needs_pages_that_cover_it() {
        let mut table = BlobTable::new(64 << 20);
        let args = guest_blob(1, 8192);
        // No pages at all.
        assert!(matches!(
            create(&mut table, &args, &[]),
            Err(CommandError::NoEntries)
        ));
        // Pages that do not cover the declared size.
        assert!(matches!(
            create(&mut table, &args, &pages(4096)),
            Err(CommandError::ShortBacking {
                need: 8192,
                have: 4096
            })
        ));
        create(&mut table, &args, &pages(8192)).expect("covered");
        assert_eq!(table.total_bytes(), 8192);
        assert!(table.owns(1));
    }

    #[test]
    fn sizes_must_be_whole_pages_inside_the_budget() {
        let mut table = BlobTable::new(64 << 20);
        for bad in [0, 1, 4095, 4097, MAX_BLOB_BYTES + BLOB_PAGE_SIZE, u64::MAX] {
            let args = guest_blob(1, bad);
            assert!(
                matches!(
                    create(&mut table, &args, &pages(BLOB_PAGE_SIZE)),
                    Err(CommandError::BadBlobSize(_))
                ),
                "size {bad} was accepted"
            );
        }
    }

    #[test]
    fn unknown_memory_types_and_flags_are_refused_not_ignored() {
        let mut table = BlobTable::new(64 << 20);
        for bad_mem in [0u32, 4, 0xffff_ffff] {
            let args = ResourceCreateBlob {
                blob_mem: bad_mem,
                ..guest_blob(1, 4096)
            };
            assert!(matches!(
                create(&mut table, &args, &pages(4096)),
                Err(CommandError::UnsupportedBlobMem(_))
            ));
        }
        let args = ResourceCreateBlob {
            blob_flags: 0x8000_0000,
            ..guest_blob(1, 4096)
        };
        assert!(matches!(
            create(&mut table, &args, &pages(4096)),
            Err(CommandError::BadBlobFlags(_))
        ));
        // Cross-device is a known bit we still refuse: we have no dmabuf.
        let args = ResourceCreateBlob {
            blob_flags: BLOB_FLAG_USE_CROSS_DEVICE,
            ..guest_blob(1, 4096)
        };
        assert!(matches!(
            create(&mut table, &args, &pages(4096)),
            Err(CommandError::BadBlobFlags(_))
        ));
    }

    #[test]
    fn a_host3d_blob_carries_no_guest_pages() {
        let mut table = BlobTable::new(64 << 20);
        let args = ResourceCreateBlob {
            blob_mem: BLOB_MEM_HOST3D,
            nr_entries: 0,
            blob_id: 77,
            ..guest_blob(1, 4096)
        };
        assert!(matches!(
            create(&mut table, &args, &pages(4096)),
            Err(CommandError::BadBlobMem { .. })
        ));
        create(&mut table, &args, &[]).expect("no pages is right");
        assert_eq!(table.get(1).map(|b| b.blob_id()), Some(77));
        assert!(table.get(1).is_some_and(|b| b.is_host()));
    }

    #[test]
    fn a_mappable_blob_needs_a_window_to_map_into() {
        let table = BlobTable::new(0);
        let no_window = BlobSupport {
            host_visible_bytes: None,
            ..support()
        };
        let args = ResourceCreateBlob {
            blob_flags: BLOB_FLAG_USE_MAPPABLE,
            ..guest_blob(1, 4096)
        };
        assert!(matches!(
            table.validate(&args, no_window, &pages(4096)),
            Err(CommandError::NoHostVisibleWindow)
        ));
        // …and a non-mappable blob is fine on the very same host.
        let plain = guest_blob(2, 4096);
        table
            .validate(&plain, no_window, &pages(4096))
            .expect("no map, no window needed");
    }

    #[test]
    fn window_reservations_cannot_overlap_or_run_off_the_end() {
        let mut window = HostVisibleWindow::new(16 * BLOB_PAGE_SIZE);
        window.reserve(1, 0, 4 * BLOB_PAGE_SIZE).expect("first");
        window
            .reserve(2, 8 * BLOB_PAGE_SIZE, 4 * BLOB_PAGE_SIZE)
            .expect("disjoint");
        // Overlaps the tail of the first…
        assert!(matches!(
            window.reserve(3, 2 * BLOB_PAGE_SIZE, BLOB_PAGE_SIZE),
            Err(CommandError::BlobMappingOverlap { .. })
        ));
        // …the head of the second…
        assert!(matches!(
            window.reserve(3, 6 * BLOB_PAGE_SIZE, 4 * BLOB_PAGE_SIZE),
            Err(CommandError::BlobMappingOverlap { .. })
        ));
        // …and swallows the second whole.
        assert!(matches!(
            window.reserve(3, 4 * BLOB_PAGE_SIZE, 12 * BLOB_PAGE_SIZE),
            Err(CommandError::BlobMappingOverlap { .. })
        ));
        // Past the end, and an overflowing end.
        assert!(matches!(
            window.reserve(3, 15 * BLOB_PAGE_SIZE, 2 * BLOB_PAGE_SIZE),
            Err(CommandError::BadBlobMapping { .. })
        ));
        assert!(matches!(
            window.reserve(3, u64::MAX - BLOB_PAGE_SIZE + 1, BLOB_PAGE_SIZE),
            Err(CommandError::BadBlobMapping { .. })
        ));
        // Unaligned offset.
        assert!(matches!(
            window.reserve(3, 1, BLOB_PAGE_SIZE),
            Err(CommandError::BadBlobMapping { .. })
        ));
        // The gap between them fits exactly.
        window
            .reserve(3, 4 * BLOB_PAGE_SIZE, 4 * BLOB_PAGE_SIZE)
            .expect("exact fit");
        assert_eq!(window.mapping_count(), 3);
        assert_eq!(window.resource_at(5 * BLOB_PAGE_SIZE), Some(3));
        window.release(3, 4 * BLOB_PAGE_SIZE);
        assert_eq!(window.resource_at(5 * BLOB_PAGE_SIZE), None);
    }

    #[test]
    fn a_window_of_zero_refuses_every_mapping() {
        let mut window = HostVisibleWindow::new(0);
        assert!(matches!(
            window.reserve(1, 0, BLOB_PAGE_SIZE),
            Err(CommandError::NoHostVisibleWindow)
        ));
    }

    #[test]
    fn mapping_a_blob_twice_is_refused_and_unref_releases_the_window() {
        let mut table = BlobTable::new(16 * BLOB_PAGE_SIZE);
        let args = ResourceCreateBlob {
            blob_flags: BLOB_FLAG_USE_MAPPABLE,
            ..guest_blob(1, 2 * BLOB_PAGE_SIZE)
        };
        create(&mut table, &args, &pages(2 * BLOB_PAGE_SIZE)).expect("create");
        assert!(matches!(
            table.unmap(1),
            Err(CommandError::BlobNotMapped(1))
        ));
        let size = table.reserve_mapping(1, 0).expect("reserve");
        assert_eq!(size, 2 * BLOB_PAGE_SIZE);
        table.commit_mapping(1, 0);
        assert!(matches!(
            table.reserve_mapping(1, 4 * BLOB_PAGE_SIZE),
            Err(CommandError::BlobAlreadyMapped(1))
        ));
        // Unref while mapped frees the window span.
        assert_eq!(table.remove(1).ok(), Some(Some(0)));
        assert_eq!(table.window().mapping_count(), 0);
        assert_eq!(table.total_bytes(), 0);
    }

    #[test]
    fn a_non_mappable_blob_cannot_be_mapped() {
        let mut table = BlobTable::new(16 * BLOB_PAGE_SIZE);
        let args = guest_blob(1, BLOB_PAGE_SIZE);
        create(&mut table, &args, &pages(BLOB_PAGE_SIZE)).expect("create");
        assert!(matches!(
            table.reserve_mapping(1, 0),
            Err(CommandError::BlobNotMappable(1))
        ));
    }

    #[test]
    fn the_resource_count_and_byte_budgets_hold() {
        let mut table = BlobTable::new(0);
        let support = BlobSupport {
            host_visible_bytes: None,
            ..support()
        };
        // The byte budget bites first for big blobs.
        let mut created = 0u32;
        loop {
            let args = guest_blob(created + 1, MAX_BLOB_BYTES);
            let entries = [MemEntry {
                addr: 0x1000,
                length: u32::MAX,
            }; 1];
            match table.validate(&args, support, &entries) {
                Ok(len) => {
                    table.insert(&args, &entries, len).expect("insert");
                    created += 1;
                }
                Err(CommandError::OutOfMemory) => break,
                Err(other) => panic!("unexpected: {other}"),
            }
            assert!(created <= 16, "budget never bit");
        }
        assert_eq!(
            u64::from(created) * MAX_BLOB_BYTES,
            MAX_TOTAL_BLOB_BYTES,
            "the budget is exact"
        );
    }
}
