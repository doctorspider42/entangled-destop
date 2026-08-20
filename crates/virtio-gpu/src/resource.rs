//! Host-side 2D resources: the pixel buffer, the guest backing store and the
//! copy paths between them (backlog MVP-803/804/805/808).
//!
//! A 2D resource is a host-owned `width × height` BGRA image plus a list of
//! guest pages ([`MemEntry`]) the guest promises hold the same image. The guest
//! renders into its pages and asks for `TRANSFER_TO_HOST_2D`, which is the only
//! place guest memory is read.
//!
//! # Untrusted-input rules obeyed here
//!
//! * Every allocation is bounded before it happens: one resource by
//!   [`crate::MAX_RESOURCE_PIXELS`], all resources together by
//!   [`MAX_TOTAL_RESOURCE_PIXELS`] and [`MAX_RESOURCES`], and the backing list
//!   by [`MAX_BACKING_ENTRIES`]. The pixel buffer is allocated with
//!   `try_reserve_exact`, so even a bound the host cannot satisfy becomes
//!   `ERR_OUT_OF_MEMORY` rather than an abort.
//! * Backing addresses are *not* validated at attach time (the guest may attach
//!   pages before they are usable); they are read through checked `vm-memory`
//!   calls at transfer time, so a page outside guest RAM fails that one command.
//! * Every rect is checked with [`Rect::fits_within`] against the resource, and
//!   every host write goes through a checked slice index — there is no
//!   arithmetic path from a guest value to an unchecked host offset.

use std::collections::HashMap;

use vm_memory::{Bytes, GuestAddress, GuestMemory};

use crate::error::CommandError;
use crate::protocol::{MemEntry, Rect};
use crate::{is_supported_format, BYTES_PER_PIXEL, MAX_RESOURCE_PIXELS};

use virtio_core::GuestMem;

/// Bytes per pixel as a `usize`, for slice arithmetic.
const BPP: usize = BYTES_PER_PIXEL as usize;

/// Largest number of live resources. A Linux guest needs a handful (fbdev
/// console plus one dumb buffer per DRM client); a malicious one would create
/// millions of 1×1 resources to grow the host's bookkeeping without ever
/// hitting the per-resource pixel cap.
pub const MAX_RESOURCES: usize = 64;

/// Total pixels across all live resources. Eight full-size resources' worth
/// (~300 MiB of BGRA), enough for a compositor's double buffering at 4K while
/// still bounding what one VM can make the host allocate.
pub const MAX_TOTAL_RESOURCE_PIXELS: u64 = 8 * MAX_RESOURCE_PIXELS;

/// Largest backing page list we accept for one resource. A 4K BGRA framebuffer
/// is ~8100 4 KiB pages, so this leaves room for fragmented scatter lists while
/// bounding the host-side list at 256 KiB per resource.
pub const MAX_BACKING_ENTRIES: u32 = 16 * 1024;

/// One host-side 2D resource.
#[derive(Debug)]
pub struct Resource {
    id: u32,
    width: u32,
    height: u32,
    format: u32,
    /// `width * height * 4` bytes of BGRA, row stride `width * 4`.
    pixels: Vec<u8>,
    /// Guest pages backing this resource, in order. Empty until
    /// `RESOURCE_ATTACH_BACKING`.
    backing: Vec<MemEntry>,
    /// Sum of the backing entry lengths, saturating.
    backing_len: u64,
}

impl Resource {
    /// Allocates a black resource. Geometry and host memory limits must have
    /// been checked by the caller ([`ResourceTable::create`] does).
    fn new(id: u32, format: u32, width: u32, height: u32) -> Result<Self, CommandError> {
        let len = pixel_bytes(width, height).ok_or(CommandError::BadGeometry { width, height })?;
        let mut pixels = Vec::new();
        pixels
            .try_reserve_exact(len)
            .map_err(|_| CommandError::OutOfMemory)?;
        pixels.resize(len, 0);
        Ok(Self {
            id,
            width,
            height,
            format,
            pixels,
            backing: Vec::new(),
            backing_len: 0,
        })
    }

    pub fn id(&self) -> u32 {
        self.id
    }

    pub fn width(&self) -> u32 {
        self.width
    }

    pub fn height(&self) -> u32 {
        self.height
    }

    pub fn format(&self) -> u32 {
        self.format
    }

    /// Row stride in bytes (`width * 4`).
    pub fn stride(&self) -> usize {
        // width is bounded by MAX_RESOURCE_PIXELS, so this cannot overflow.
        self.width as usize * BPP
    }

    /// The host pixel buffer.
    pub fn pixels(&self) -> &[u8] {
        &self.pixels
    }

    /// The attached backing entries, in order.
    ///
    /// Only a snapshot needs the list itself (ADR-0006): it is what lets a
    /// restored resource re-read its pixels out of guest memory instead of
    /// carrying them in the file.
    pub fn backing(&self) -> &[MemEntry] {
        &self.backing
    }

    /// Number of attached backing entries.
    pub fn backing_entries(&self) -> usize {
        self.backing.len()
    }

    /// Total length of the attached backing store in bytes.
    pub fn backing_len(&self) -> u64 {
        self.backing_len
    }

    /// Replaces the backing store. The guest is supposed to detach first; a
    /// second attach silently replaces the list, matching what other VMMs do.
    pub fn attach(&mut self, entries: Vec<MemEntry>) {
        self.backing_len = entries
            .iter()
            .fold(0u64, |sum, e| sum.saturating_add(u64::from(e.length)));
        self.backing = entries;
    }

    /// Drops the backing store (`RESOURCE_DETACH_BACKING`, and implicitly
    /// `RESOURCE_UNREF`). The host pixels survive — the guest may still be
    /// showing them.
    pub fn detach(&mut self) {
        self.backing = Vec::new();
        self.backing_len = 0;
    }

    /// Copies `rect` from the guest backing store into the host pixel buffer
    /// (`TRANSFER_TO_HOST_2D`, MVP-805).
    ///
    /// `offset` is the byte offset of the rect's first pixel inside the backing
    /// store; rows are `stride` bytes apart there, exactly as in the host
    /// buffer.
    pub fn transfer_from_backing(
        &mut self,
        mem: &GuestMem,
        rect: Rect,
        offset: u64,
    ) -> Result<(), CommandError> {
        if !rect.fits_within(self.width, self.height) {
            return Err(CommandError::RectOutOfBounds {
                rect,
                width: self.width,
                height: self.height,
            });
        }
        if self.backing.is_empty() {
            return Err(CommandError::NoBacking(self.id));
        }

        let stride = self.stride();
        let row_bytes = rect.width as usize * BPP;
        // Source span: the last row starts stride*(height-1) bytes after
        // `offset`. All of it must exist in the backing store.
        let stride64 = stride as u64;
        let rows_before_last = u64::from(rect.height - 1);
        let need = stride64
            .checked_mul(rows_before_last)
            .and_then(|span| span.checked_add(offset))
            .and_then(|last_row| last_row.checked_add(row_bytes as u64))
            .ok_or(CommandError::ShortBacking {
                need: u64::MAX,
                have: self.backing_len,
            })?;
        if need > self.backing_len {
            return Err(CommandError::ShortBacking {
                need,
                have: self.backing_len,
            });
        }

        // Full-width transfers (the common case: the guest flushes whole
        // scanlines) are one contiguous span in both the backing store and the
        // host buffer, so they walk the scatter list exactly once.
        if rect.x == 0 && rect.width == self.width {
            let start = rect.y as usize * stride;
            let len = stride * rect.height as usize;
            let dst = self
                .pixels
                .get_mut(start..start.saturating_add(len))
                .ok_or(CommandError::RectOutOfBounds {
                    rect,
                    width: self.width,
                    height: self.height,
                })?;
            return read_backing(mem, &self.backing, offset, dst);
        }

        // Partial-width transfer: one scatter-list walk per row. Rare (guests
        // update whole scanlines) and bounded by the resource height.
        for row in 0..rect.height {
            let src = offset + stride64 * u64::from(row);
            let dst_start = (rect.y as usize + row as usize) * stride + rect.x as usize * BPP;
            let dst = self
                .pixels
                .get_mut(dst_start..dst_start.saturating_add(row_bytes))
                .ok_or(CommandError::RectOutOfBounds {
                    rect,
                    width: self.width,
                    height: self.height,
                })?;
            read_backing(mem, &self.backing, src, dst)?;
        }
        Ok(())
    }

    /// `rect` as tightly packed BGRA rows, ready for
    /// [`crate::ScanoutSink::update_scanout`].
    ///
    /// Returns a borrow of the resource itself when the rect spans whole rows
    /// (no copy at all — the common full-width flush); otherwise the rows are
    /// gathered into `scratch`, which the caller reuses across flushes.
    /// `None` only when the rect does not fit the resource.
    pub fn rect_bytes<'a>(&'a self, rect: Rect, scratch: &'a mut Vec<u8>) -> Option<&'a [u8]> {
        if !rect.fits_within(self.width, self.height) {
            return None;
        }
        let stride = self.stride();
        let row_bytes = rect.width as usize * BPP;
        if rect.width == self.width {
            let start = rect.y as usize * stride;
            return self
                .pixels
                .get(start..start.checked_add(row_bytes * rect.height as usize)?);
        }
        scratch.clear();
        for row in 0..rect.height {
            let start = (rect.y as usize + row as usize) * stride + rect.x as usize * BPP;
            let src = self.pixels.get(start..start.checked_add(row_bytes)?)?;
            scratch.extend_from_slice(src);
        }
        Some(scratch.as_slice())
    }
}

/// Byte size of a `width × height` BGRA image, or `None` when it is zero-sized,
/// beyond [`crate::MAX_RESOURCE_PIXELS`] or beyond the host's address space.
fn pixel_bytes(width: u32, height: u32) -> Option<usize> {
    let pixels = u64::from(width) * u64::from(height);
    if width == 0 || height == 0 || pixels > MAX_RESOURCE_PIXELS {
        return None;
    }
    usize::try_from(pixels * u64::from(BYTES_PER_PIXEL)).ok()
}

/// Walks the chunks of the scattered backing store covering
/// `offset..offset + len` and hands each one to `visit` as
/// `(guest address, chunk length, position inside the destination)`.
///
/// Purely arithmetic — no memory is touched — and every step is checked or
/// saturating, so no guest-supplied length or address can wrap it. A list that
/// runs out before `len` bytes are covered is [`CommandError::ShortBacking`].
fn walk_backing(
    entries: &[MemEntry],
    offset: u64,
    len: usize,
    mut visit: impl FnMut(u64, usize, usize) -> Result<(), CommandError>,
) -> Result<(), CommandError> {
    // Position of the current entry inside the flattened backing store.
    let mut pos = 0u64;
    let mut want = offset;
    let mut covered = 0usize;
    let wanted = offset.saturating_add(len as u64);

    for entry in entries {
        if covered == len {
            return Ok(());
        }
        let entry_len = u64::from(entry.length);
        let end = pos.saturating_add(entry_len);
        if end <= want {
            pos = end;
            continue;
        }
        // want >= pos holds: `want` only ever advances by what we consumed.
        let skip = want - pos;
        let available = entry_len - skip;
        let remaining = (len - covered) as u64;
        let take = available.min(remaining);
        let take_len = usize::try_from(take).unwrap_or(0);
        if take_len > 0 {
            let addr = entry
                .addr
                .checked_add(skip)
                .ok_or(CommandError::Unreadable {
                    addr: entry.addr,
                    reason: "backing entry address overflows".into(),
                })?;
            visit(addr, take_len, covered)?;
            covered += take_len;
            want += take;
        }
        pos = end;
    }
    if covered == len {
        return Ok(());
    }
    Err(CommandError::ShortBacking {
        need: wanted,
        have: pos,
    })
}

/// Reads `dst.len()` bytes starting at linear offset `offset` of the scattered
/// backing store into `dst`.
///
/// The backing list is guest-supplied, so this is done in two passes: the first
/// checks every chunk against guest RAM, the second copies. That matters because
/// `vm-memory`'s `read_slice` copies as much as it can *before* reporting a
/// short read — a one-pass version would leave the resource half-updated from a
/// command that failed. With the pre-check, a rejected transfer changes nothing,
/// and an entry pointing outside guest RAM is [`CommandError::Unreadable`]
/// instead of a host memory access.
pub fn read_backing(
    mem: &GuestMem,
    entries: &[MemEntry],
    offset: u64,
    dst: &mut [u8],
) -> Result<(), CommandError> {
    if dst.is_empty() {
        return Ok(());
    }
    walk_backing(entries, offset, dst.len(), |addr, len, _at| {
        if mem.check_range(GuestAddress(addr), len) {
            Ok(())
        } else {
            Err(CommandError::Unreadable {
                addr,
                reason: format!("{len} bytes from here are not guest RAM"),
            })
        }
    })?;
    walk_backing(entries, offset, dst.len(), |addr, len, at| {
        let slot = dst
            .get_mut(at..at.saturating_add(len))
            .ok_or(CommandError::Unreadable {
                addr,
                reason: "backing chunk does not fit the destination".into(),
            })?;
        mem.read_slice(slot, GuestAddress(addr))
            .map_err(|error| CommandError::Unreadable {
                addr,
                reason: error.to_string(),
            })
    })
}

/// Writes `src` into the scattered backing store starting at linear offset
/// `offset` — the reverse of [`read_backing`], used by `TRANSFER_FROM_HOST_3D`
/// (the guest reading rendered pixels back).
///
/// The same two-pass discipline: every chunk is checked against guest RAM
/// before anything is written, so a rejected transfer writes nothing.
pub fn write_backing(
    mem: &GuestMem,
    entries: &[MemEntry],
    offset: u64,
    src: &[u8],
) -> Result<(), CommandError> {
    if src.is_empty() {
        return Ok(());
    }
    walk_backing(entries, offset, src.len(), |addr, len, _at| {
        if mem.check_range(GuestAddress(addr), len) {
            Ok(())
        } else {
            Err(CommandError::Unreadable {
                addr,
                reason: format!("{len} bytes from here are not guest RAM"),
            })
        }
    })?;
    walk_backing(entries, offset, src.len(), |addr, len, at| {
        let chunk = src
            .get(at..at.saturating_add(len))
            .ok_or(CommandError::Unreadable {
                addr,
                reason: "backing chunk does not fit the source".into(),
            })?;
        mem.write_slice(chunk, GuestAddress(addr))
            .map_err(|error| CommandError::Unreadable {
                addr,
                reason: error.to_string(),
            })
    })
}

/// Every live resource, keyed by the guest-chosen resource id.
#[derive(Debug, Default)]
pub struct ResourceTable {
    resources: HashMap<u32, Resource>,
    /// Sum of `width * height` over all live resources.
    total_pixels: u64,
}

impl ResourceTable {
    pub fn new() -> Self {
        Self::default()
    }

    /// Number of live resources.
    pub fn len(&self) -> usize {
        self.resources.len()
    }

    pub fn is_empty(&self) -> bool {
        self.resources.is_empty()
    }

    /// Total pixels held by all live resources.
    pub fn total_pixels(&self) -> u64 {
        self.total_pixels
    }

    /// Every live resource, in no particular order.
    ///
    /// The order does not matter to a snapshot: each record carries its own id,
    /// and `create` places it back under that id.
    pub fn iter(&self) -> impl Iterator<Item = &Resource> {
        self.resources.values()
    }

    pub fn get(&self, id: u32) -> Option<&Resource> {
        self.resources.get(&id)
    }

    pub fn get_mut(&mut self, id: u32) -> Option<&mut Resource> {
        self.resources.get_mut(&id)
    }

    /// `RESOURCE_CREATE_2D` (MVP-803/809): validates the id, the pixel format
    /// and the geometry, then allocates the host image.
    pub fn create(
        &mut self,
        id: u32,
        format: u32,
        width: u32,
        height: u32,
    ) -> Result<(), CommandError> {
        if id == 0 {
            return Err(CommandError::ZeroResourceId);
        }
        if self.resources.contains_key(&id) {
            return Err(CommandError::DuplicateResource(id));
        }
        if !is_supported_format(format) {
            return Err(CommandError::UnsupportedFormat(format));
        }
        if pixel_bytes(width, height).is_none() {
            return Err(CommandError::BadGeometry { width, height });
        }
        let pixels = u64::from(width) * u64::from(height);
        if self.resources.len() >= MAX_RESOURCES
            || self.total_pixels.saturating_add(pixels) > MAX_TOTAL_RESOURCE_PIXELS
        {
            return Err(CommandError::OutOfMemory);
        }
        let resource = Resource::new(id, format, width, height)?;
        self.resources.insert(id, resource);
        self.total_pixels = self.total_pixels.saturating_add(pixels);
        Ok(())
    }

    /// `RESOURCE_UNREF` (MVP-808): drops the resource and its backing list.
    pub fn remove(&mut self, id: u32) -> Result<(), CommandError> {
        if id == 0 {
            return Err(CommandError::ZeroResourceId);
        }
        let resource = self
            .resources
            .remove(&id)
            .ok_or(CommandError::UnknownResource(id))?;
        let pixels = u64::from(resource.width) * u64::from(resource.height);
        self.total_pixels = self.total_pixels.saturating_sub(pixels);
        Ok(())
    }

    /// Drops every resource (device reset).
    pub fn clear(&mut self) {
        self.resources.clear();
        self.total_pixels = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{FORMAT_B8G8R8A8_UNORM, FORMAT_B8G8R8X8_UNORM};
    use virtio_core::testing::guest_memory;

    const MEM_SIZE: u64 = 1 << 20;

    fn entry(addr: u64, length: u32) -> MemEntry {
        MemEntry { addr, length }
    }

    fn write(mem: &GuestMem, addr: u64, bytes: &[u8]) {
        mem.write_slice(bytes, GuestAddress(addr))
            .expect("test write inside guest memory");
    }

    fn rect(x: u32, y: u32, width: u32, height: u32) -> Rect {
        Rect {
            x,
            y,
            width,
            height,
        }
    }

    #[test]
    fn create_validates_id_format_and_geometry() {
        let mut table = ResourceTable::new();
        assert!(matches!(
            table.create(0, FORMAT_B8G8R8A8_UNORM, 4, 4),
            Err(CommandError::ZeroResourceId)
        ));
        assert!(matches!(
            table.create(1, 67, 4, 4),
            Err(CommandError::UnsupportedFormat(67))
        ));
        assert!(matches!(
            table.create(1, FORMAT_B8G8R8A8_UNORM, 0, 4),
            Err(CommandError::BadGeometry { .. })
        ));
        assert!(matches!(
            table.create(1, FORMAT_B8G8R8A8_UNORM, 4, 0),
            Err(CommandError::BadGeometry { .. })
        ));
        assert!(matches!(
            table.create(1, FORMAT_B8G8R8A8_UNORM, u32::MAX, u32::MAX),
            Err(CommandError::BadGeometry { .. })
        ));
        assert!(table.is_empty(), "nothing was allocated");

        table
            .create(1, FORMAT_B8G8R8A8_UNORM, 4, 2)
            .expect("valid resource");
        assert_eq!(table.len(), 1);
        assert_eq!(table.total_pixels(), 8);
        assert!(matches!(
            table.create(1, FORMAT_B8G8R8A8_UNORM, 4, 2),
            Err(CommandError::DuplicateResource(1))
        ));

        let res = table.get(1).expect("exists");
        assert_eq!((res.width(), res.height()), (4, 2));
        assert_eq!(res.stride(), 16);
        assert_eq!(res.pixels().len(), 32);
        assert_eq!(res.format(), FORMAT_B8G8R8A8_UNORM);
        assert_eq!(res.id(), 1);

        // XRGB8888 — what a real Linux guest sends for its framebuffer — is the
        // same byte layout and must be accepted too.
        table
            .create(2, FORMAT_B8G8R8X8_UNORM, 4, 2)
            .expect("XRGB8888 is supported");
        assert_eq!(
            table.get(2).map(Resource::format),
            Some(FORMAT_B8G8R8X8_UNORM)
        );
    }

    #[test]
    fn resource_count_and_total_pixels_are_capped() {
        let mut table = ResourceTable::new();
        for id in 1..=MAX_RESOURCES as u32 {
            table
                .create(id, FORMAT_B8G8R8A8_UNORM, 2, 2)
                .expect("small resources fit");
        }
        assert!(matches!(
            table.create(9999, FORMAT_B8G8R8A8_UNORM, 2, 2),
            Err(CommandError::OutOfMemory)
        ));

        // The pixel budget bites before the count does for big resources.
        let mut table = ResourceTable::new();
        let side = 4096;
        let tall = u32::try_from(MAX_RESOURCE_PIXELS / u64::from(side)).expect("fits");
        for id in 1..=8u32 {
            table
                .create(id, FORMAT_B8G8R8A8_UNORM, side, tall)
                .expect("eight full-size resources fit the budget");
        }
        assert!(matches!(
            table.create(9, FORMAT_B8G8R8A8_UNORM, side, tall),
            Err(CommandError::OutOfMemory)
        ));
        assert_eq!(table.len(), 8);
    }

    #[test]
    fn remove_frees_the_pixel_budget() {
        let mut table = ResourceTable::new();
        table.create(7, FORMAT_B8G8R8A8_UNORM, 10, 10).expect("ok");
        assert_eq!(table.total_pixels(), 100);
        assert!(matches!(table.remove(0), Err(CommandError::ZeroResourceId)));
        assert!(matches!(
            table.remove(8),
            Err(CommandError::UnknownResource(8))
        ));
        table.remove(7).expect("removes");
        assert_eq!(table.total_pixels(), 0);
        assert!(matches!(
            table.remove(7),
            Err(CommandError::UnknownResource(7))
        ));

        table.create(7, FORMAT_B8G8R8A8_UNORM, 2, 2).expect("ok");
        table.clear();
        assert!(table.is_empty());
        assert_eq!(table.total_pixels(), 0);
    }

    #[test]
    fn attach_sums_the_backing_length_and_detach_clears_it() {
        let mut table = ResourceTable::new();
        table.create(1, FORMAT_B8G8R8A8_UNORM, 4, 4).expect("ok");
        let res = table.get_mut(1).expect("exists");
        assert_eq!(res.backing_entries(), 0);
        assert_eq!(res.backing_len(), 0);

        res.attach(vec![entry(0x1000, 32), entry(0x2000, 32)]);
        assert_eq!(res.backing_entries(), 2);
        assert_eq!(res.backing_len(), 64);

        // A pathological list cannot overflow the sum.
        res.attach(vec![entry(0, u32::MAX); 8]);
        assert_eq!(res.backing_len(), 8 * u64::from(u32::MAX));

        res.detach();
        assert_eq!(res.backing_len(), 0);
        assert_eq!(res.backing_entries(), 0);
    }

    #[test]
    fn read_backing_spans_entries_and_honours_the_offset() {
        let mem = guest_memory(MEM_SIZE);
        write(&mem, 0x1000, &[1, 2, 3, 4]);
        write(&mem, 0x2000, &[5, 6, 7, 8]);
        let entries = [entry(0x1000, 4), entry(0x2000, 4)];

        let mut out = [0u8; 8];
        read_backing(&mem, &entries, 0, &mut out).expect("full read");
        assert_eq!(out, [1, 2, 3, 4, 5, 6, 7, 8]);

        // Straddling the entry boundary.
        let mut out = [0u8; 4];
        read_backing(&mem, &entries, 2, &mut out).expect("straddling read");
        assert_eq!(out, [3, 4, 5, 6]);

        // Entirely inside the second entry.
        let mut out = [0u8; 2];
        read_backing(&mem, &entries, 6, &mut out).expect("tail read");
        assert_eq!(out, [7, 8]);

        // Zero-length destination is a no-op even with no entries at all.
        read_backing(&mem, &[], 0, &mut []).expect("nothing to do");
    }

    #[test]
    fn read_backing_skips_zero_length_entries() {
        let mem = guest_memory(MEM_SIZE);
        write(&mem, 0x3000, &[9, 9]);
        let entries = [entry(0x1000, 0), entry(0x3000, 2), entry(0x4000, 0)];
        let mut out = [0u8; 2];
        read_backing(&mem, &entries, 0, &mut out).expect("read");
        assert_eq!(out, [9, 9]);
    }

    #[test]
    fn read_backing_rejects_short_lists_and_bad_addresses() {
        let mem = guest_memory(MEM_SIZE);
        let entries = [entry(0x1000, 4)];
        let mut out = [0u8; 8];
        assert!(matches!(
            read_backing(&mem, &entries, 0, &mut out),
            Err(CommandError::ShortBacking { need: 8, have: 4 })
        ));
        // Offset past the end of the list.
        assert!(matches!(
            read_backing(&mem, &entries, 99, &mut [0u8; 1]),
            Err(CommandError::ShortBacking { .. })
        ));
        // No list at all.
        assert!(matches!(
            read_backing(&mem, &[], 0, &mut [0u8; 1]),
            Err(CommandError::ShortBacking { .. })
        ));

        // Pages outside guest RAM: a checked-read failure, not a host crash.
        let outside = [entry(MEM_SIZE + 0x1000, 8)];
        assert!(matches!(
            read_backing(&mem, &outside, 0, &mut out),
            Err(CommandError::Unreadable { .. })
        ));
        let wrapping = [entry(u64::MAX - 1, 8)];
        assert!(read_backing(&mem, &wrapping, 0, &mut out).is_err());
        // An entry whose address wraps when the offset is applied.
        let overflowing = [entry(u64::MAX, 8)];
        assert!(matches!(
            read_backing(&mem, &overflowing, 4, &mut [0u8; 2]),
            Err(CommandError::Unreadable { .. })
        ));
    }

    #[test]
    fn a_failed_read_leaves_the_destination_untouched() {
        let mem = guest_memory(MEM_SIZE);
        write(&mem, 0x7000, &[1, 2, 3, 4]);
        // A good first entry followed by one outside guest RAM, and an entry
        // that straddles the end of guest RAM: `read_slice` would copy the
        // valid prefix before failing, so the pre-check has to catch both.
        for entries in [
            vec![entry(0x7000, 4), entry(MEM_SIZE + 0x1000, 4)],
            vec![entry(0x7000, 4), entry(MEM_SIZE - 2, 4)],
        ] {
            let mut out = [0xffu8; 8];
            assert!(matches!(
                read_backing(&mem, &entries, 0, &mut out),
                Err(CommandError::Unreadable { .. })
            ));
            assert_eq!(out, [0xff; 8], "not even the valid prefix is copied");
        }
    }

    #[test]
    fn transfer_copies_the_whole_resource() {
        let mem = guest_memory(MEM_SIZE);
        let mut table = ResourceTable::new();
        table.create(1, FORMAT_B8G8R8A8_UNORM, 2, 2).expect("ok");
        // 2x2 BGRA image in guest memory.
        let image: Vec<u8> = (0..16u8).collect();
        write(&mem, 0x5000, &image);
        let res = table.get_mut(1).expect("exists");
        res.attach(vec![entry(0x5000, 16)]);

        res.transfer_from_backing(&mem, rect(0, 0, 2, 2), 0)
            .expect("transfer");
        assert_eq!(res.pixels(), image.as_slice());
    }

    #[test]
    fn transfer_of_a_sub_rect_respects_the_stride() {
        let mem = guest_memory(MEM_SIZE);
        let mut table = ResourceTable::new();
        table.create(1, FORMAT_B8G8R8A8_UNORM, 4, 4).expect("ok");
        let res = table.get_mut(1).expect("exists");
        // Backing holds a full 4x4 image of 0xAA…; only the 2x2 block at (1,1)
        // is transferred, so the rest of the host image stays black.
        write(&mem, 0x6000, &[0xaa; 4 * 4 * 4]);
        res.attach(vec![entry(0x6000, 4 * 4 * 4)]);

        // The rect's first pixel sits one row down and one pixel in.
        let offset = res.stride() as u64 + u64::from(BYTES_PER_PIXEL);
        res.transfer_from_backing(&mem, rect(1, 1, 2, 2), offset)
            .expect("transfer");

        let px = |x: usize, y: usize| -> [u8; 4] {
            let at = y * 16 + x * 4;
            let mut out = [0u8; 4];
            out.copy_from_slice(&res.pixels()[at..at + 4]);
            out
        };
        assert_eq!(px(1, 1), [0xaa; 4]);
        assert_eq!(px(2, 2), [0xaa; 4]);
        assert_eq!(px(0, 0), [0; 4]);
        assert_eq!(px(3, 3), [0; 4]);
        assert_eq!(px(1, 3), [0; 4]);
    }

    #[test]
    fn transfer_rejects_bad_rects_offsets_and_missing_backing() {
        let mem = guest_memory(MEM_SIZE);
        let mut table = ResourceTable::new();
        table.create(1, FORMAT_B8G8R8A8_UNORM, 4, 4).expect("ok");
        let res = table.get_mut(1).expect("exists");

        // No backing yet.
        assert!(matches!(
            res.transfer_from_backing(&mem, rect(0, 0, 4, 4), 0),
            Err(CommandError::NoBacking(1))
        ));

        res.attach(vec![entry(0x6000, 4 * 4 * 4)]);
        // Rect outside the resource.
        for bad in [
            rect(0, 0, 5, 4),
            rect(1, 0, 4, 4),
            rect(0, 0, 0, 4),
            rect(u32::MAX, 0, 2, 2),
        ] {
            assert!(matches!(
                res.transfer_from_backing(&mem, bad, 0),
                Err(CommandError::RectOutOfBounds { .. })
            ));
        }
        // Offset past the backing store, and an offset that would overflow.
        assert!(matches!(
            res.transfer_from_backing(&mem, rect(0, 0, 4, 4), 1),
            Err(CommandError::ShortBacking { .. })
        ));
        assert!(matches!(
            res.transfer_from_backing(&mem, rect(0, 0, 4, 4), u64::MAX),
            Err(CommandError::ShortBacking { .. })
        ));
        assert!(matches!(
            res.transfer_from_backing(&mem, rect(1, 1, 2, 2), u64::MAX - 4),
            Err(CommandError::ShortBacking { .. })
        ));
        // Nothing was written.
        assert!(res.pixels().iter().all(|b| *b == 0));
    }

    #[test]
    fn transfer_from_pages_outside_guest_ram_leaves_the_resource_untouched() {
        let mem = guest_memory(MEM_SIZE);
        let mut table = ResourceTable::new();
        table.create(1, FORMAT_B8G8R8A8_UNORM, 2, 2).expect("ok");
        let res = table.get_mut(1).expect("exists");
        res.attach(vec![entry(MEM_SIZE - 4, 16)]);
        assert!(matches!(
            res.transfer_from_backing(&mem, rect(0, 0, 2, 2), 0),
            Err(CommandError::Unreadable { .. })
        ));
        assert!(res.pixels().iter().all(|b| *b == 0));
    }

    #[test]
    fn rect_bytes_borrows_full_rows_and_gathers_partial_ones() {
        let mem = guest_memory(MEM_SIZE);
        let mut table = ResourceTable::new();
        table.create(1, FORMAT_B8G8R8A8_UNORM, 2, 2).expect("ok");
        let res = table.get_mut(1).expect("exists");
        let image: Vec<u8> = (0..16u8).collect();
        write(&mem, 0x5000, &image);
        res.attach(vec![entry(0x5000, 16)]);
        res.transfer_from_backing(&mem, rect(0, 0, 2, 2), 0)
            .expect("transfer");

        let mut scratch = Vec::new();
        // Whole resource: a borrow of the resource itself.
        assert_eq!(
            res.rect_bytes(rect(0, 0, 2, 2), &mut scratch),
            Some(image.as_slice())
        );
        // Second row only, still full width.
        assert_eq!(
            res.rect_bytes(rect(0, 1, 2, 1), &mut scratch),
            Some(&image[8..16])
        );
        // One column: gathered row by row into the scratch buffer.
        assert_eq!(
            res.rect_bytes(rect(1, 0, 1, 2), &mut scratch),
            Some([4, 5, 6, 7, 12, 13, 14, 15].as_slice())
        );
        // Bad rects yield None rather than a panic.
        assert_eq!(res.rect_bytes(rect(0, 0, 3, 3), &mut scratch), None);
        assert_eq!(res.rect_bytes(rect(2, 0, 1, 1), &mut scratch), None);
        assert_eq!(res.rect_bytes(rect(0, 0, 0, 0), &mut scratch), None);
    }
}
