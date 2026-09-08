//! The virtio-blk device (backlog MVP-401…408).
//!
//! Request layout on the wire (VirtIO spec 1.2, section 5.2.6): one chain of
//!
//! * a device-readable 16-byte header (`type`, `reserved`, `sector`),
//! * zero or more data buffers — device-readable for `OUT`, device-writable
//!   for `IN`/`GET_ID`,
//! * a device-writable status byte, last.
//!
//! Every value in that chain is guest-controlled. The rules this module obeys:
//! the chain walk is bounded and index-checked (`virtio_core::chain`), buffer
//! addresses are only ever used through checked `vm-memory` calls, sector
//! ranges go through [`validate_range`], total payloads are capped at
//! [`MAX_REQUEST_BYTES`], and any malformed request is answered with an error
//! status byte instead of taking the device — or the host — down.

use std::path::Path;
use std::sync::Arc;

use virtio_core::chain::{self, Segment};
use virtio_core::device::{DeviceError, DeviceResources, DeviceType, VirtioDevice};
use virtio_core::interrupt::Interrupt;
use virtio_core::{GuestMem, MAX_DESC_CHAIN_LEN, MAX_QUEUE_SIZE, VIRTIO_F_VERSION_1};
use virtio_queue::{Queue, QueueT};
use vm_memory::{Bytes, GuestAddress};

use crate::raw::RawDisk;
use crate::request::{
    sector_offset, segment_count, total_len, validate_range, BlockError, DiscardSegment,
    ReclaimRange, RequestHeader, RequestType, DISCARD_SECTOR_ALIGNMENT, DISCARD_SEGMENT_LEN,
    ID_BYTES, MAX_DISCARD_SECTORS, MAX_DISCARD_SEG, MAX_REQUEST_BYTES, MAX_WRITE_ZEROES_SECTORS,
    REQUEST_HEADER_LEN, SECTOR_SIZE, S_IOERR, S_OK, S_UNSUPP, WRITE_ZEROES_MAY_UNMAP,
};

/// `VIRTIO_BLK_F_RO`: the disk is read-only.
pub const VIRTIO_BLK_F_RO: u64 = 1 << 5;
/// `VIRTIO_BLK_F_FLUSH`: the device honours `VIRTIO_BLK_T_FLUSH`.
pub const VIRTIO_BLK_F_FLUSH: u64 = 1 << 9;
/// `VIRTIO_BLK_F_DISCARD`: the device honours `VIRTIO_BLK_T_DISCARD`, so a
/// guest `fstrim` gives host disk space back.
pub const VIRTIO_BLK_F_DISCARD: u64 = 1 << 13;
/// `VIRTIO_BLK_F_WRITE_ZEROES`: the device honours
/// `VIRTIO_BLK_T_WRITE_ZEROES`.
pub const VIRTIO_BLK_F_WRITE_ZEROES: u64 = 1 << 14;

/// Set `ENTANGLED_BLK_DISCARD=off` to withhold both reclaim features from every
/// disk of this VM: the before/after switch for measuring reclaim, and the
/// escape hatch if a host filesystem ever turns out to punch holes badly. Any
/// other value (or none) leaves them on, which is the right default -- without
/// them a sparse image only ever grows.
fn reclaim_enabled() -> bool {
    !std::env::var("ENTANGLED_BLK_DISCARD").is_ok_and(|v| {
        matches!(
            v.trim().to_ascii_lowercase().as_str(),
            "off" | "0" | "false"
        )
    })
}

/// virtio-blk has exactly one request queue in the MVP (no `VIRTIO_BLK_F_MQ`).
pub const NUM_QUEUES: usize = 1;
const REQUEST_QUEUE: u16 = 0;
static QUEUE_MAX_SIZES: [u16; NUM_QUEUES] = [MAX_QUEUE_SIZE];

/// Size of the guest-visible config space.
///
/// `struct virtio_blk_config` is a fixed layout, so publishing the discard and
/// write-zeroes fields means publishing everything in front of them too. The
/// fields in between (`size_max`, `seg_max`, geometry, `blk_size`, topology,
/// `writeback`, `num_queues`) belong to features we do not offer and read back
/// as zero, which is exactly what a driver that has not negotiated them
/// expects: it never looks. 60 bytes covers `write_zeroes_may_unmap` at offset
/// 56 plus its three padding bytes, and stops short of the secure-erase fields
/// (`VIRTIO_BLK_F_SECURE_ERASE`, not offered).
const CONFIG_LEN: usize = 60;

/// Offsets inside `struct virtio_blk_config` (VirtIO spec 1.2, section 5.2.4).
mod config {
    pub const CAPACITY: usize = 0;
    pub const MAX_DISCARD_SECTORS: usize = 36;
    pub const MAX_DISCARD_SEG: usize = 40;
    pub const DISCARD_SECTOR_ALIGNMENT: usize = 44;
    pub const MAX_WRITE_ZEROES_SECTORS: usize = 48;
    pub const MAX_WRITE_ZEROES_SEG: usize = 52;
    pub const WRITE_ZEROES_MAY_UNMAP: usize = 56;
}

/// Hard bound on how many chains one notification may process, so a guest that
/// keeps refilling the available ring from another vCPU cannot pin this thread
/// forever. Leftover work is picked up by the next notification.
pub const CHAINS_PER_NOTIFY: usize = 4 * MAX_QUEUE_SIZE as usize;

/// A virtio-blk device backed by a RAW image file.
pub struct BlockDevice {
    /// Label for log records — the image file name.
    name: String,
    disk: RawDisk,
    /// `VIRTIO_BLK_T_GET_ID` reply, zero-padded to [`ID_BYTES`].
    device_id: [u8; ID_BYTES],
    features: u64,
    acked_features: u64,
    /// Reusable staging buffer, so steady-state I/O does not allocate. Bounded
    /// by [`MAX_REQUEST_BYTES`] because that caps any single buffer.
    io_buf: Vec<u8>,

    // Set on activate(), cleared on reset().
    mem: Option<Arc<GuestMem>>,
    queue: Option<Queue>,
    interrupt: Option<Arc<dyn Interrupt>>,
}

impl std::fmt::Debug for BlockDevice {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BlockDevice")
            .field("name", &self.name)
            .field("capacity_sectors", &self.disk.capacity_sectors())
            .field("read_only", &self.disk.is_read_only())
            .field("activated", &self.queue.is_some())
            .finish_non_exhaustive()
    }
}

impl BlockDevice {
    /// Opens `path` as a RAW image and builds the device. `writable` false
    /// yields a read-only device (MVP-408).
    pub fn open(path: &Path, writable: bool) -> Result<Self, BlockError> {
        let disk = RawDisk::open(path, writable)?;
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| path.display().to_string());
        Ok(Self::with_disk(name, disk))
    }

    /// Builds a device around an already opened backend.
    pub fn with_disk(name: String, disk: RawDisk) -> Self {
        let mut features = VIRTIO_F_VERSION_1 | VIRTIO_BLK_F_FLUSH;
        if disk.is_read_only() {
            // Both reclaim commands modify the image, so a read-only disk (an
            // installer ISO) must not even offer them.
            features |= VIRTIO_BLK_F_RO;
        } else if reclaim_enabled() {
            features |= VIRTIO_BLK_F_DISCARD | VIRTIO_BLK_F_WRITE_ZEROES;
        }
        let mut device_id = [0u8; ID_BYTES];
        for (slot, byte) in device_id.iter_mut().zip(name.as_bytes()) {
            *slot = *byte;
        }
        Self {
            name,
            disk,
            device_id,
            features,
            acked_features: 0,
            io_buf: Vec::new(),
            mem: None,
            queue: None,
            interrupt: None,
        }
    }

    pub fn capacity_sectors(&self) -> u64 {
        self.disk.capacity_sectors()
    }

    pub fn is_read_only(&self) -> bool {
        self.disk.is_read_only()
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    // ------------------------------------------------------ queue draining

    /// Processes every available chain, then notifies the driver once.
    fn drain(
        &mut self,
        queue: &mut Queue,
        mem: &Arc<GuestMem>,
        interrupt: &dyn Interrupt,
    ) -> Result<(), DeviceError> {
        let desc_table = queue.desc_table();
        let queue_size = queue.size();
        let mut served = 0usize;

        while let Some(head) = queue
            .pop_descriptor_chain(Arc::clone(mem))
            .map(|chain| chain.head_index())
        {
            let written = self.handle_request(mem, desc_table, queue_size, head);
            queue
                .add_used(mem.as_ref(), head, written)
                .map_err(|e| DeviceError::Queue(e.to_string()))?;
            served += 1;
            if served >= CHAINS_PER_NOTIFY {
                tracing::warn!(
                    disk = %self.name,
                    served,
                    "virtio-blk notification budget exhausted; deferring the rest"
                );
                break;
            }
        }

        if served > 0
            && queue
                .needs_notification(mem.as_ref())
                .map_err(|e| DeviceError::Queue(e.to_string()))?
        {
            interrupt.signal_used_queue(REQUEST_QUEUE)?;
        }
        Ok(())
    }

    /// Handles one descriptor chain. Returns the number of bytes written into
    /// device-writable buffers, which is what goes into the used ring.
    ///
    /// Never returns an error: a request the guest malformed badly enough that
    /// even the status byte is unreachable is dropped (used length 0), because
    /// the alternative — failing the whole device — would let a buggy driver
    /// take the disk offline.
    fn handle_request(
        &mut self,
        mem: &GuestMem,
        desc_table: u64,
        queue_size: u16,
        head: u16,
    ) -> u32 {
        let segments = match chain::walk(mem, desc_table, queue_size, head) {
            Ok(segments) => segments,
            Err(error) => {
                tracing::warn!(
                    disk = %self.name,
                    head,
                    %error,
                    "dropping malformed virtio-blk descriptor chain"
                );
                return 0;
            }
        };
        let (readable, writable) = match chain::split_rw(&segments) {
            Ok(split) => split,
            Err(error) => {
                tracing::warn!(disk = %self.name, head, %error, "dropping virtio-blk chain");
                return 0;
            }
        };

        let Some(header_seg) = readable.first() else {
            tracing::warn!(
                disk = %self.name,
                head,
                "virtio-blk chain has no device-readable header"
            );
            return 0;
        };
        if (header_seg.len as usize) < REQUEST_HEADER_LEN {
            tracing::warn!(
                disk = %self.name,
                head,
                len = header_seg.len,
                "virtio-blk request header is too short"
            );
            return 0;
        }
        let mut raw_header = [0u8; REQUEST_HEADER_LEN];
        if let Err(error) = mem.read_slice(&mut raw_header, GuestAddress(header_seg.addr)) {
            tracing::warn!(
                disk = %self.name,
                head,
                addr = format_args!("{:#x}", header_seg.addr),
                %error,
                "virtio-blk request header is not readable guest memory"
            );
            return 0;
        }
        let header = RequestHeader::parse(&raw_header);

        // The status byte is always the last device-writable buffer; without it
        // there is nowhere to report a result, so the chain is unusable.
        let Some((status_seg, data_in)) = writable.split_last() else {
            tracing::warn!(
                disk = %self.name,
                head,
                "virtio-blk chain has no device-writable status byte"
            );
            return 0;
        };
        if status_seg.len < 1 {
            tracing::warn!(
                disk = %self.name,
                head,
                "virtio-blk status descriptor is zero-length"
            );
            return 0;
        }
        // Everything readable after the header is write payload.
        let data_out = readable.get(1..).unwrap_or(&[]);

        let (status, written) = self.execute(mem, &header, data_out, data_in);

        if let Err(error) = mem.write_obj(status, GuestAddress(status_seg.addr)) {
            tracing::warn!(
                disk = %self.name,
                head,
                addr = format_args!("{:#x}", status_seg.addr),
                %error,
                "cannot write the virtio-blk status byte"
            );
            return written;
        }
        written.saturating_add(1)
    }

    fn execute(
        &mut self,
        mem: &GuestMem,
        header: &RequestHeader,
        data_out: &[Segment],
        data_in: &[Segment],
    ) -> (u8, u32) {
        let Some(kind) = header.request_type() else {
            tracing::debug!(
                disk = %self.name,
                raw_type = header.raw_type,
                "unsupported virtio-blk request type"
            );
            return (S_UNSUPP, 0);
        };
        match kind {
            RequestType::In => self.do_in(mem, header.sector, data_in),
            RequestType::Out => self.do_out(mem, header.sector, data_out),
            RequestType::Flush => (self.do_flush(), 0),
            RequestType::GetId => self.do_get_id(mem, data_in),
            RequestType::Discard | RequestType::WriteZeroes => {
                self.do_reclaim(mem, kind, data_out, data_in)
            }
        }
    }

    fn do_in(&mut self, mem: &GuestMem, sector: u64, data_in: &[Segment]) -> (u8, u32) {
        let len = match total_len(data_in.iter().map(|s| s.len)) {
            Ok(len) => len,
            Err(error) => {
                tracing::warn!(disk = %self.name, %error, "rejecting virtio-blk read");
                return (S_IOERR, 0);
            }
        };
        if let Err(error) = validate_range(self.disk.capacity_sectors(), sector, len) {
            tracing::warn!(disk = %self.name, sector, len, %error, "rejecting virtio-blk read");
            return (S_IOERR, 0);
        }
        let mut offset = match sector_offset(sector) {
            Ok(offset) => offset,
            Err(error) => {
                tracing::warn!(disk = %self.name, sector, %error, "rejecting virtio-blk read");
                return (S_IOERR, 0);
            }
        };

        let mut written = 0u32;
        for segment in data_in {
            let chunk = segment.len as usize;
            if chunk == 0 {
                continue;
            }
            self.reserve(chunk);
            if let Err(error) = self.disk.read_at(&mut self.io_buf[..chunk], offset) {
                tracing::warn!(disk = %self.name, offset, %error, "virtio-blk read failed");
                return (S_IOERR, written);
            }
            if let Err(error) = mem.write_slice(&self.io_buf[..chunk], GuestAddress(segment.addr)) {
                tracing::warn!(
                    disk = %self.name,
                    addr = format_args!("{:#x}", segment.addr),
                    len = segment.len,
                    %error,
                    "virtio-blk read target is not writable guest memory"
                );
                return (S_IOERR, written);
            }
            offset = offset.saturating_add(chunk as u64);
            written = written.saturating_add(segment.len);
        }
        (S_OK, written)
    }

    fn do_out(&mut self, mem: &GuestMem, sector: u64, data_out: &[Segment]) -> (u8, u32) {
        // Belt and braces: a read-only image must reject writes even if
        // feature negotiation somehow let VIRTIO_BLK_F_RO slip.
        if self.disk.is_read_only() {
            tracing::warn!(
                disk = %self.name,
                sector,
                "rejecting write to a read-only virtio-blk device"
            );
            return (S_IOERR, 0);
        }
        let len = match total_len(data_out.iter().map(|s| s.len)) {
            Ok(len) => len,
            Err(error) => {
                tracing::warn!(disk = %self.name, %error, "rejecting virtio-blk write");
                return (S_IOERR, 0);
            }
        };
        if let Err(error) = validate_range(self.disk.capacity_sectors(), sector, len) {
            tracing::warn!(disk = %self.name, sector, len, %error, "rejecting virtio-blk write");
            return (S_IOERR, 0);
        }
        let mut offset = match sector_offset(sector) {
            Ok(offset) => offset,
            Err(error) => {
                tracing::warn!(disk = %self.name, sector, %error, "rejecting virtio-blk write");
                return (S_IOERR, 0);
            }
        };

        for segment in data_out {
            let chunk = segment.len as usize;
            if chunk == 0 {
                continue;
            }
            self.reserve(chunk);
            if let Err(error) =
                mem.read_slice(&mut self.io_buf[..chunk], GuestAddress(segment.addr))
            {
                tracing::warn!(
                    disk = %self.name,
                    addr = format_args!("{:#x}", segment.addr),
                    len = segment.len,
                    %error,
                    "virtio-blk write source is not readable guest memory"
                );
                return (S_IOERR, 0);
            }
            if let Err(error) = self.disk.write_at(&self.io_buf[..chunk], offset) {
                tracing::warn!(disk = %self.name, offset, %error, "virtio-blk write failed");
                return (S_IOERR, 0);
            }
            offset = offset.saturating_add(chunk as u64);
        }
        (S_OK, 0)
    }

    /// `VIRTIO_BLK_T_DISCARD` / `VIRTIO_BLK_T_WRITE_ZEROES`: the guest hands
    /// back sectors it no longer needs.
    ///
    /// Both commands carry a **segment array** in place of a data payload:
    /// `n` × `struct virtio_blk_discard_write_zeroes`, each 16 bytes of
    /// entirely guest-chosen sector, length and flags. The order here is
    /// deliberate — stage, parse, validate *every* segment, and only then touch
    /// the image — so a request with one bad segment changes nothing at all
    /// rather than half of what it asked for.
    fn do_reclaim(
        &mut self,
        mem: &GuestMem,
        kind: RequestType,
        data_out: &[Segment],
        data_in: &[Segment],
    ) -> (u8, u32) {
        let feature = match kind {
            RequestType::Discard => VIRTIO_BLK_F_DISCARD,
            RequestType::WriteZeroes => VIRTIO_BLK_F_WRITE_ZEROES,
            _ => return (S_UNSUPP, 0),
        };
        // A command for a feature the driver never negotiated is not ours to
        // serve, whether or not we offered it.
        if self.acked_features & feature == 0 {
            tracing::debug!(
                disk = %self.name,
                command = %reclaim_name(kind),
                "refusing a reclaim command the driver did not negotiate"
            );
            return (S_UNSUPP, 0);
        }
        // Belt and braces, exactly as in `do_out`: these commands modify the
        // image, so a read-only disk refuses them even if negotiation slipped.
        if self.disk.is_read_only() {
            tracing::warn!(
                disk = %self.name,
                command = %reclaim_name(kind),
                "rejecting a reclaim command on a read-only virtio-blk device"
            );
            return (S_IOERR, 0);
        }
        // Nothing is written back except the status byte; a driver that offered
        // device-writable buffers here gets them left alone.
        if !data_in.is_empty() {
            tracing::debug!(
                disk = %self.name,
                command = %reclaim_name(kind),
                segments = data_in.len(),
                "ignoring device-writable buffers on a reclaim request"
            );
        }

        // 1. How long is the array? Two guest-controlled numbers have to agree:
        //    the descriptor lengths and the segment size.
        let bytes = match total_len(data_out.iter().map(|s| s.len)) {
            Ok(bytes) => bytes,
            Err(error) => {
                tracing::warn!(disk = %self.name, %error, "rejecting reclaim request");
                return (S_IOERR, 0);
            }
        };
        let count = match segment_count(bytes) {
            Ok(count) => count,
            // A payload that is not a whole number of segments is a malformed
            // request, not a limit violation: the driver got the wire format
            // wrong, so S_UNSUPP.
            Err(error @ BlockError::BadSegmentArray(_)) => {
                tracing::warn!(disk = %self.name, %error, "malformed reclaim request");
                return (S_UNSUPP, 0);
            }
            Err(error) => {
                tracing::warn!(disk = %self.name, %error, "rejecting reclaim request");
                return (S_IOERR, 0);
            }
        };

        // 2. Stage the array. `segment_count` has bounded it at
        //    MAX_DISCARD_ARRAY_BYTES (4 KiB), so this cast and this buffer are
        //    both bounded by a constant.
        let total = match usize::try_from(bytes) {
            Ok(total) => total,
            Err(_) => return (S_IOERR, 0),
        };
        self.reserve(total);
        let mut at = 0usize;
        for segment in data_out {
            let chunk = segment.len as usize;
            if chunk == 0 {
                continue;
            }
            let Some(slot) = self.io_buf.get_mut(at..at + chunk) else {
                // Unreachable: `total` is the sum of these lengths. Refused
                // rather than indexed, because "unreachable" is not a proof.
                return (S_IOERR, 0);
            };
            if let Err(error) = mem.read_slice(slot, GuestAddress(segment.addr)) {
                tracing::warn!(
                    disk = %self.name,
                    addr = format_args!("{:#x}", segment.addr),
                    len = segment.len,
                    %error,
                    "reclaim segment array is not readable guest memory"
                );
                return (S_IOERR, 0);
            }
            at += chunk;
        }

        // 3. Validate every segment before acting on any of them.
        let capacity = self.disk.capacity_sectors();
        let mut ranges: Vec<ReclaimRange> = Vec::with_capacity(count);
        for index in 0..count {
            let start = index * DISCARD_SEGMENT_LEN;
            let mut raw = [0u8; DISCARD_SEGMENT_LEN];
            match self.io_buf.get(start..start + DISCARD_SEGMENT_LEN) {
                Some(slice) => raw.copy_from_slice(slice),
                None => return (S_IOERR, 0),
            }
            let segment = DiscardSegment::parse(&raw);
            match segment.validate(kind, capacity) {
                Ok(range) => ranges.push(range),
                // Reserved flag bits and a misplaced unmap bit are the driver
                // using the protocol wrongly — S_UNSUPP, as the spec's own
                // "unsupported request" answer.
                Err(error @ (BlockError::ReservedFlags(_) | BlockError::UnmapNotAllowed)) => {
                    tracing::warn!(
                        disk = %self.name,
                        command = %reclaim_name(kind),
                        index,
                        %error,
                        "refusing reclaim segment"
                    );
                    return (S_UNSUPP, 0);
                }
                Err(error) => {
                    tracing::warn!(
                        disk = %self.name,
                        command = %reclaim_name(kind),
                        index,
                        sector = segment.sector,
                        num_sectors = segment.num_sectors,
                        %error,
                        "refusing reclaim segment"
                    );
                    return (S_IOERR, 0);
                }
            }
        }

        // 4. Act. A host I/O failure part way through is reported as it
        //    happened: the spec lets a failed request have applied some of its
        //    segments, and both commands are idempotent, so a retry is safe.
        for range in ranges {
            let result = match kind {
                RequestType::Discard => self.disk.discard(range.offset, range.len),
                _ => self.disk.write_zeroes(range.offset, range.len, range.unmap),
            };
            if let Err(error) = result {
                tracing::warn!(
                    disk = %self.name,
                    command = %reclaim_name(kind),
                    offset = range.offset,
                    len = range.len,
                    %error,
                    "host reclaim failed"
                );
                return (S_IOERR, 0);
            }
        }
        (S_OK, 0)
    }

    fn do_flush(&mut self) -> u8 {
        match self.disk.flush() {
            Ok(()) => S_OK,
            Err(error) => {
                tracing::warn!(disk = %self.name, %error, "virtio-blk flush failed");
                S_IOERR
            }
        }
    }

    fn do_get_id(&mut self, mem: &GuestMem, data_in: &[Segment]) -> (u8, u32) {
        let Some(segment) = data_in.first() else {
            tracing::warn!(disk = %self.name, "GET_ID without a reply buffer");
            return (S_IOERR, 0);
        };
        let len = (segment.len as usize).min(ID_BYTES);
        if len == 0 {
            return (S_OK, 0);
        }
        match mem.write_slice(&self.device_id[..len], GuestAddress(segment.addr)) {
            // `len` is at most ID_BYTES (20), so the cast cannot truncate.
            Ok(()) => (S_OK, len as u32),
            Err(error) => {
                tracing::warn!(
                    disk = %self.name,
                    addr = format_args!("{:#x}", segment.addr),
                    %error,
                    "GET_ID reply buffer is not writable guest memory"
                );
                (S_IOERR, 0)
            }
        }
    }

    /// The guest-visible `struct virtio_blk_config`.
    ///
    /// Only the fields belonging to features we offer are ever non-zero. The
    /// reclaim limits are the constants validation enforces: publishing a
    /// number we would then refuse would make a well-behaved driver look
    /// malicious.
    fn config_space(&self) -> [u8; CONFIG_LEN] {
        let mut raw = [0u8; CONFIG_LEN];
        raw[config::CAPACITY..config::CAPACITY + 8]
            .copy_from_slice(&self.disk.capacity_sectors().to_le_bytes());
        if self.features & VIRTIO_BLK_F_DISCARD != 0 {
            raw[config::MAX_DISCARD_SECTORS..config::MAX_DISCARD_SECTORS + 4]
                .copy_from_slice(&MAX_DISCARD_SECTORS.to_le_bytes());
            raw[config::MAX_DISCARD_SEG..config::MAX_DISCARD_SEG + 4]
                .copy_from_slice(&MAX_DISCARD_SEG.to_le_bytes());
            raw[config::DISCARD_SECTOR_ALIGNMENT..config::DISCARD_SECTOR_ALIGNMENT + 4]
                .copy_from_slice(&DISCARD_SECTOR_ALIGNMENT.to_le_bytes());
        }
        if self.features & VIRTIO_BLK_F_WRITE_ZEROES != 0 {
            raw[config::MAX_WRITE_ZEROES_SECTORS..config::MAX_WRITE_ZEROES_SECTORS + 4]
                .copy_from_slice(&MAX_WRITE_ZEROES_SECTORS.to_le_bytes());
            raw[config::MAX_WRITE_ZEROES_SEG..config::MAX_WRITE_ZEROES_SEG + 4]
                .copy_from_slice(&MAX_DISCARD_SEG.to_le_bytes());
            raw[config::WRITE_ZEROES_MAY_UNMAP] = WRITE_ZEROES_MAY_UNMAP;
        }
        raw
    }

    /// Grows the staging buffer to at least `len` bytes. `len` is bounded by
    /// [`MAX_REQUEST_BYTES`] because [`total_len`] already vetted the request.
    fn reserve(&mut self, len: usize) {
        if self.io_buf.len() < len {
            self.io_buf.resize(len, 0);
        }
    }
}

impl VirtioDevice for BlockDevice {
    fn device_type(&self) -> DeviceType {
        DeviceType::Block
    }

    fn queue_max_sizes(&self) -> &[u16] {
        &QUEUE_MAX_SIZES
    }

    fn device_features(&self) -> u64 {
        self.features
    }

    fn ack_features(&mut self, negotiated: u64) -> bool {
        if negotiated & VIRTIO_F_VERSION_1 == 0 {
            return false;
        }
        if negotiated & !self.features != 0 {
            tracing::warn!(
                disk = %self.name,
                negotiated = format_args!("{negotiated:#x}"),
                offered = format_args!("{:#x}", self.features),
                "driver accepted features the device never offered"
            );
            return false;
        }
        self.acked_features = negotiated;
        true
    }

    fn read_config(&self, offset: u64, data: &mut [u8]) {
        let config = self.config_space();
        for (i, byte) in data.iter_mut().enumerate() {
            let index = offset.saturating_add(i as u64);
            *byte = usize::try_from(index)
                .ok()
                .and_then(|i| config.get(i))
                .copied()
                .unwrap_or(0);
        }
    }

    fn write_config(&mut self, offset: u64, data: &[u8]) {
        tracing::warn!(
            disk = %self.name,
            offset,
            len = data.len(),
            "ignoring guest write to the read-only virtio-blk config space"
        );
    }

    fn activate(&mut self, resources: DeviceResources) -> Result<(), DeviceError> {
        let mut queues = resources.queues;
        if queues.len() != NUM_QUEUES {
            return Err(DeviceError::QueueCount {
                expected: NUM_QUEUES,
                actual: queues.len(),
            });
        }
        self.queue = queues.pop();
        self.mem = Some(resources.mem);
        self.interrupt = Some(resources.interrupt);
        tracing::info!(
            disk = %self.name,
            capacity_sectors = self.disk.capacity_sectors(),
            read_only = self.disk.is_read_only(),
            "virtio-blk ready"
        );
        Ok(())
    }

    fn notify(&mut self, queue_index: u16) -> Result<(), DeviceError> {
        if queue_index != REQUEST_QUEUE {
            return Err(DeviceError::UnknownQueue(queue_index));
        }
        let mem = self.mem.clone().ok_or(DeviceError::NotActivated)?;
        let interrupt = self.interrupt.clone().ok_or(DeviceError::NotActivated)?;
        // Taken out for the duration so `self` stays mutably usable inside the
        // drain loop; always put back, even on error.
        let mut queue = self.queue.take().ok_or(DeviceError::NotActivated)?;
        let result = self.drain(&mut queue, &mem, interrupt.as_ref());
        self.queue = Some(queue);
        result
    }

    /// virtio-blk has **no state beyond its queue** once the VM is quiesced
    /// (ADR-0006).
    ///
    /// Every request is served synchronously inside `notify`: the chain is
    /// walked, the file read or written, the used entry added and the interrupt
    /// signalled before `notify` returns. So a paused device holds nothing —
    /// there is no in-flight list to drain, because there is nowhere for a
    /// request to be in flight. `crates/virtio-block/tests/blk_queue.rs`
    /// asserts that as a property rather than as a comment.
    ///
    /// What still has to be carried is the *position*: the guest may have
    /// posted requests the device has not been kicked for yet, and a restored
    /// device that started at zero would serve every completed request again.
    fn queue_positions(&self) -> Vec<virtio_core::QueuePosition> {
        use virtio_queue::QueueT as _;
        self.queue
            .as_ref()
            .map(|q| {
                vec![virtio_core::QueuePosition {
                    next_avail: q.next_avail(),
                    next_used: q.next_used(),
                }]
            })
            .unwrap_or_default()
    }

    fn reset(&mut self) {
        // Data the guest believed was flushed must not be lost across a reset.
        if let Err(error) = self.disk.flush() {
            tracing::warn!(disk = %self.name, %error, "flush during virtio-blk reset failed");
        }
        self.queue = None;
        self.mem = None;
        self.interrupt = None;
        self.acked_features = 0;
        self.io_buf = Vec::new();
    }
}

/// Chain-shape limits the guest must respect, exported so the CLI and tests can
/// state them: header + status take two descriptors out of the chain budget.
pub const MAX_DATA_SEGMENTS: u16 = MAX_DESC_CHAIN_LEN - 2;

/// Sectors per [`MAX_REQUEST_BYTES`], for callers reporting device limits.
pub const MAX_REQUEST_SECTORS: u64 = MAX_REQUEST_BYTES / SECTOR_SIZE;

/// Log label for the two reclaim commands.
fn reclaim_name(kind: RequestType) -> &'static str {
    match kind {
        RequestType::Discard => "discard",
        RequestType::WriteZeroes => "write-zeroes",
        _ => "not-a-reclaim-command",
    }
}
