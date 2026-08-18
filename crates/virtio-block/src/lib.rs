//! virtio-blk device (backlog EPIC 4).
//!
//! Three layers:
//!
//! * [`request`] — the wire protocol and all pure validation (portable, the
//!   bulk of the unit tests live here),
//! * [`raw`] — the RAW image backend (`std::fs::File`, positional I/O),
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
    BlockDevice, MAX_DATA_SEGMENTS, MAX_REQUEST_SECTORS, NUM_QUEUES, VIRTIO_BLK_F_FLUSH,
    VIRTIO_BLK_F_RO,
};
pub use raw::RawDisk;
pub use request::{
    sector_offset, total_len, validate_range, BlockError, RequestHeader, RequestType, ID_BYTES,
    MAX_REQUEST_BYTES, REQUEST_HEADER_LEN, SECTOR_SIZE, S_IOERR, S_OK, S_UNSUPP,
};
