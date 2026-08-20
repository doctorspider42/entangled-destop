//! virtio-blk device (backlog EPIC 4).
//!
//! Three layers:
//!
//! * [`request`] — the wire protocol and all pure validation (portable, the
//!   bulk of the unit tests live here),
//! * [`raw`] — the RAW image backend (`std::fs::File`, positional I/O, plus
//!   the hole punching that makes a guest `fstrim` reclaim host disk space),
//! * [`device`] — [`BlockDevice`], the `virtio_core::VirtioDevice`
//!   implementation that ties guest descriptor chains to the backend.
//!
//! Nothing in this crate knows about virtio-mmio: the device only ever sees
//! queues, features and its config space, so the post-MVP virtio-pci transport
//! can drive it unchanged.

pub mod device;
pub mod raw;
pub mod request;

pub use device::{
    BlockDevice, CHAINS_PER_NOTIFY, MAX_DATA_SEGMENTS, MAX_REQUEST_SECTORS, NUM_QUEUES,
    VIRTIO_BLK_F_DISCARD, VIRTIO_BLK_F_FLUSH, VIRTIO_BLK_F_RO, VIRTIO_BLK_F_WRITE_ZEROES,
};
pub use raw::RawDisk;
pub use request::{
    sector_offset, segment_count, total_len, validate_range, BlockError, DiscardSegment,
    ReclaimRange, RequestHeader, RequestType, DISCARD_SECTOR_ALIGNMENT, DISCARD_SEGMENT_LEN,
    ID_BYTES, MAX_DISCARD_ARRAY_BYTES, MAX_DISCARD_SECTORS, MAX_DISCARD_SEG, MAX_REQUEST_BYTES,
    MAX_WRITE_ZEROES_SECTORS, REQUEST_HEADER_LEN, SECTOR_SIZE, S_IOERR, S_OK, S_UNSUPP,
    WRITE_ZEROES_FLAG_UNMAP, WRITE_ZEROES_MAY_UNMAP,
};
