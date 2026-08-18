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
    sector_offset, total_len, validate_range, BlockError, RequestHeader, RequestType, ID_BYTES,
    MAX_REQUEST_BYTES, REQUEST_HEADER_LEN, SECTOR_SIZE, S_IOERR, S_OK, S_UNSUPP,
};

/// `VIRTIO_BLK_F_RO`: the disk is read-only.
pub const VIRTIO_BLK_F_RO: u64 = 1 << 5;
/// `VIRTIO_BLK_F_FLUSH`: the device honours `VIRTIO_BLK_T_FLUSH`.
pub const VIRTIO_BLK_F_FLUSH: u64 = 1 << 9;

/// virtio-blk has exactly one request queue in the MVP (no `VIRTIO_BLK_F_MQ`).
pub const NUM_QUEUES: usize = 1;
const REQUEST_QUEUE: u16 = 0;
static QUEUE_MAX_SIZES: [u16; NUM_QUEUES] = [MAX_QUEUE_SIZE];

/// Size of the guest-visible config space: just the `capacity` field, in
/// 512-byte sectors, little-endian. The later fields of
/// `struct virtio_blk_config` (`size_max`, `seg_max`, geometry, topology…) all
/// belong to features we do not offer, so the driver never reads them.
const CONFIG_LEN: usize = 8;

/// Hard bound on how many chains one notification may process, so a guest that
/// keeps refilling the available ring from another vCPU cannot pin this thread
/// forever. Leftover work is picked up by the next notification.
const CHAINS_PER_NOTIFY: usize = 4 * MAX_QUEUE_SIZE as usize;

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
            features |= VIRTIO_BLK_F_RO;
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
        let config = self.disk.capacity_sectors().to_le_bytes();
        debug_assert_eq!(config.len(), CONFIG_LEN);
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
