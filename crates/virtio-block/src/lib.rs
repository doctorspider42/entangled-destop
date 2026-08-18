//! virtio-blk device (backlog EPIC 4).
//!
//! Current state: request protocol model and sector-range validation
//! (MVP-402/406 groundwork). The queue-driven device and RAW file backend
//! land with the mmio transport.

use thiserror::Error;

/// Sector size fixed by the virtio-blk spec.
pub const SECTOR_SIZE: u64 = 512;

/// Request types from the virtio spec we support in the MVP
/// (`VIRTIO_BLK_T_*`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum RequestType {
    In = 0,
    Out = 1,
    Flush = 4,
    GetId = 8,
}

impl RequestType {
    pub fn from_raw(raw: u32) -> Option<Self> {
        match raw {
            0 => Some(Self::In),
            1 => Some(Self::Out),
            4 => Some(Self::Flush),
            8 => Some(Self::GetId),
            _ => None,
        }
    }
}

/// Status byte written back to the guest (`VIRTIO_BLK_S_*`).
pub const S_OK: u8 = 0;
pub const S_IOERR: u8 = 1;
pub const S_UNSUPP: u8 = 2;

#[derive(Debug, Error, PartialEq, Eq)]
pub enum BlockError {
    #[error("I/O of {len} bytes at sector {sector} exceeds disk of {capacity} sectors")]
    OutOfRange {
        sector: u64,
        len: u64,
        capacity: u64,
    },

    #[error("I/O length {0} is not a multiple of the {SECTOR_SIZE}-byte sector size")]
    UnalignedLength(u64),

    #[error("write request on a read-only device")]
    ReadOnly,
}

/// Validates a guest I/O request against the disk geometry (MVP-406).
///
/// `capacity_sectors` is the disk size in 512-byte sectors, `sector` the
/// requested start, `len` the total data length in bytes.
pub fn validate_range(capacity_sectors: u64, sector: u64, len: u64) -> Result<(), BlockError> {
    if len % SECTOR_SIZE != 0 {
        return Err(BlockError::UnalignedLength(len));
    }
    let sectors = len / SECTOR_SIZE;
    let end = sector.checked_add(sectors).ok_or(BlockError::OutOfRange {
        sector,
        len,
        capacity: capacity_sectors,
    })?;
    if end > capacity_sectors {
        return Err(BlockError::OutOfRange {
            sector,
            len,
            capacity: capacity_sectors,
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const CAP: u64 = 64; // 32 KiB disk

    #[test]
    fn in_range_ok() {
        validate_range(CAP, 0, 512).unwrap();
        validate_range(CAP, 63, 512).unwrap();
        validate_range(CAP, 0, 64 * 512).unwrap();
    }

    #[test]
    fn end_of_disk_rejected() {
        assert!(matches!(
            validate_range(CAP, 64, 512),
            Err(BlockError::OutOfRange { .. })
        ));
        assert!(matches!(
            validate_range(CAP, 1, 64 * 512),
            Err(BlockError::OutOfRange { .. })
        ));
    }

    #[test]
    fn overflow_does_not_wrap() {
        assert!(matches!(
            validate_range(CAP, u64::MAX, 512),
            Err(BlockError::OutOfRange { .. })
        ));
    }

    #[test]
    fn unaligned_rejected() {
        assert!(matches!(
            validate_range(CAP, 0, 100),
            Err(BlockError::UnalignedLength(100))
        ));
    }

    #[test]
    fn unknown_request_type_is_none() {
        assert_eq!(RequestType::from_raw(7), None);
        assert_eq!(RequestType::from_raw(1), Some(RequestType::Out));
    }
}
