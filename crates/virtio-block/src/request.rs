//! virtio-blk request protocol and validation (backlog MVP-402/406).
//!
//! Pure logic: no guest memory, no file I/O, no transport. Everything here
//! runs on any host OS and is unit-testable in isolation, which is where the
//! bulk of the untrusted-input checking lives.

use thiserror::Error;

/// Sector size fixed by the virtio-blk spec.
pub const SECTOR_SIZE: u64 = 512;

/// Length of the device-readable request header: `type` (le32), `reserved`
/// (le32), `sector` (le64).
pub const REQUEST_HEADER_LEN: usize = 16;

/// Length of the `GET_ID` reply buffer (`VIRTIO_BLK_ID_BYTES`).
pub const ID_BYTES: usize = 20;

/// Largest total data payload we accept for one request. A guest can chain up
/// to `virtio_core::MAX_DESC_CHAIN_LEN` descriptors of up to 4 GiB each, so
/// without this cap a single request could ask the host to stage hundreds of
/// gigabytes.
pub const MAX_REQUEST_BYTES: u64 = 4 << 20;

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

#[derive(Debug, Error)]
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

    #[error("request payload of {0} bytes exceeds the {MAX_REQUEST_BYTES}-byte limit")]
    RequestTooLarge(u64),

    #[error("disk image {path} could not be opened: {source}")]
    Open {
        path: String,
        #[source]
        source: std::io::Error,
    },

    #[error("disk image {path} is {len} bytes, which is not a whole number of sectors")]
    UnalignedImage { path: String, len: u64 },

    #[error("disk image is empty")]
    EmptyImage,

    #[error("disk I/O at offset {offset} failed: {source}")]
    Io {
        offset: u64,
        #[source]
        source: std::io::Error,
    },

    #[error("flushing the disk image failed: {source}")]
    Flush {
        #[source]
        source: std::io::Error,
    },
}

/// The 16-byte device-readable request header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RequestHeader {
    /// Raw `type` field — kept raw so an unknown value can be answered with
    /// `S_UNSUPP` and logged rather than silently remapped.
    pub raw_type: u32,
    /// Start sector for `IN`/`OUT`; meaningless for `FLUSH`/`GET_ID`.
    pub sector: u64,
}

impl RequestHeader {
    /// Parses the header from its little-endian wire form. Bytes 4..8 are the
    /// spec's `reserved` field and are ignored.
    pub fn parse(bytes: &[u8; REQUEST_HEADER_LEN]) -> Self {
        Self {
            raw_type: u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]),
            sector: u64::from_le_bytes([
                bytes[8], bytes[9], bytes[10], bytes[11], bytes[12], bytes[13], bytes[14],
                bytes[15],
            ]),
        }
    }

    pub fn request_type(&self) -> Option<RequestType> {
        RequestType::from_raw(self.raw_type)
    }
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

/// Byte offset of `sector` in the backing image, checked against overflow.
pub fn sector_offset(sector: u64) -> Result<u64, BlockError> {
    sector
        .checked_mul(SECTOR_SIZE)
        .ok_or(BlockError::OutOfRange {
            sector,
            len: 0,
            capacity: 0,
        })
}

/// Total length of a list of buffer lengths, capped at [`MAX_REQUEST_BYTES`].
pub fn total_len(lengths: impl IntoIterator<Item = u32>) -> Result<u64, BlockError> {
    let mut total: u64 = 0;
    for len in lengths {
        total = total
            .checked_add(u64::from(len))
            .ok_or(BlockError::RequestTooLarge(u64::MAX))?;
        if total > MAX_REQUEST_BYTES {
            return Err(BlockError::RequestTooLarge(total));
        }
    }
    Ok(total)
}

#[cfg(test)]
mod tests {
    use super::*;

    const CAP: u64 = 64; // 32 KiB disk

    #[test]
    fn in_range_ok() {
        assert!(validate_range(CAP, 0, 512).is_ok());
        assert!(validate_range(CAP, 63, 512).is_ok());
        assert!(validate_range(CAP, 0, 64 * 512).is_ok());
        assert!(validate_range(CAP, 0, 0).is_ok());
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
        assert!(matches!(
            sector_offset(u64::MAX),
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
        // DISCARD (11) and WRITE_ZEROES (13) exist in the spec but are not
        // negotiated, so they must read back as unknown.
        assert_eq!(RequestType::from_raw(11), None);
        assert_eq!(RequestType::from_raw(13), None);
    }

    #[test]
    fn header_parses_little_endian_fields() {
        let mut raw = [0u8; REQUEST_HEADER_LEN];
        raw[0..4].copy_from_slice(&1u32.to_le_bytes());
        raw[4..8].copy_from_slice(&0xdead_beefu32.to_le_bytes()); // reserved
        raw[8..16].copy_from_slice(&0x1234_5678_9abcu64.to_le_bytes());

        let header = RequestHeader::parse(&raw);
        assert_eq!(header.raw_type, 1);
        assert_eq!(header.request_type(), Some(RequestType::Out));
        assert_eq!(header.sector, 0x1234_5678_9abc);
    }

    #[test]
    fn total_len_caps_oversized_requests() {
        assert_eq!(total_len([512u32, 512]).ok(), Some(1024));
        assert!(matches!(
            total_len([u32::MAX, u32::MAX]),
            Err(BlockError::RequestTooLarge(_))
        ));
        // Exactly at the limit is fine, one byte over is not.
        let cap = u32::try_from(MAX_REQUEST_BYTES).expect("cap fits in u32");
        assert_eq!(total_len([cap]).ok(), Some(MAX_REQUEST_BYTES));
        assert!(matches!(
            total_len([cap, 1]),
            Err(BlockError::RequestTooLarge(_))
        ));
    }
}
