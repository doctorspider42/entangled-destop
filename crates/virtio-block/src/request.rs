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

/// Request types from the virtio spec we implement (`VIRTIO_BLK_T_*`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum RequestType {
    In = 0,
    Out = 1,
    Flush = 4,
    GetId = 8,
    /// `VIRTIO_BLK_T_DISCARD` — the guest no longer needs these sectors; the
    /// host may deallocate them (`VIRTIO_BLK_F_DISCARD`).
    Discard = 11,
    /// `VIRTIO_BLK_T_WRITE_ZEROES` — these sectors must read back as zeros,
    /// and may be deallocated if the segment sets `unmap`
    /// (`VIRTIO_BLK_F_WRITE_ZEROES`).
    WriteZeroes = 13,
}

impl RequestType {
    pub fn from_raw(raw: u32) -> Option<Self> {
        match raw {
            0 => Some(Self::In),
            1 => Some(Self::Out),
            4 => Some(Self::Flush),
            8 => Some(Self::GetId),
            11 => Some(Self::Discard),
            13 => Some(Self::WriteZeroes),
            _ => None,
        }
    }

    /// True for the two commands that carry a segment array instead of a data
    /// payload.
    pub fn is_reclaim(self) -> bool {
        matches!(self, Self::Discard | Self::WriteZeroes)
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

    #[error("discard/write-zeroes flags {0:#x} set reserved bits")]
    ReservedFlags(u32),

    #[error("the unmap flag is defined for write-zeroes only, not for discard")]
    UnmapNotAllowed,

    #[error("reclaim segment of {num_sectors} sectors exceeds the advertised maximum of {max}")]
    ReclaimTooLarge { num_sectors: u32, max: u32 },

    #[error(
        "discard/write-zeroes payload of {0} bytes is not a whole number of          {DISCARD_SEGMENT_LEN}-byte segments"
    )]
    BadSegmentArray(u64),

    #[error("{count} reclaim segments exceed the advertised maximum of {max}")]
    TooManySegments { count: u64, max: u32 },

    #[error("request type {0} is not a discard or write-zeroes request")]
    NotAReclaimRequest(u32),

    #[error("discarding {len} bytes at offset {offset} failed: {source}")]
    Discard {
        offset: u64,
        len: u64,
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
///
/// Self-sufficient on purpose: it checks the payload cap as well as the
/// geometry, so a caller that reaches it without going through [`total_len`]
/// first still cannot be talked into an oversized transfer. The MVP-1402 fuzz
/// target found that gap — the device happened to call `total_len` first, so the
/// cap was enforced by call order rather than by this helper, which the
/// virtio-device skill points every new device at.
pub fn validate_range(capacity_sectors: u64, sector: u64, len: u64) -> Result<(), BlockError> {
    if len % SECTOR_SIZE != 0 {
        return Err(BlockError::UnalignedLength(len));
    }
    if len > MAX_REQUEST_BYTES {
        return Err(BlockError::RequestTooLarge(len));
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
    // The backend turns the accepted sector into a byte offset and reads
    // `len` bytes from there, so both must be representable. A real
    // `capacity_sectors` comes from a file size and can never be large enough
    // for this to trigger — but the check belongs here rather than in the
    // caller's head (MVP-1402 fuzz finding).
    sector_offset(sector)?
        .checked_add(len)
        .ok_or(BlockError::OutOfRange {
            sector,
            len,
            capacity: capacity_sectors,
        })?;
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

// ---------------------------------------------------------------------------
// DISCARD / WRITE_ZEROES (thin-provisioning reclaim)
// ---------------------------------------------------------------------------

/// Length of one `struct virtio_blk_discard_write_zeroes` segment: `sector`
/// (le64), `num_sectors` (le32), `flags` (le32).
pub const DISCARD_SEGMENT_LEN: usize = 16;

/// `VIRTIO_BLK_WRITE_ZEROES_FLAG_UNMAP` — the only defined flag bit. Every
/// other bit of the segment's `flags` word is reserved and must be zero.
pub const WRITE_ZEROES_FLAG_UNMAP: u32 = 1;

/// `discard_sector_alignment`: the alignment, in sectors, the driver should use
/// for discard ranges.
///
/// 8 sectors = 4 KiB, which is the block size of every filesystem that will
/// hold one of our images (ext4's default block, NTFS's default cluster). A
/// discard the guest aligns to that is one the host can actually deallocate;
/// a smaller granularity would only produce ranges `fallocate` has to zero
/// instead of punch.
pub const DISCARD_SECTOR_ALIGNMENT: u32 = 8;

/// `max_discard_sectors`: largest range one discard segment may cover — 1 GiB.
///
/// The host serves a discard with a single `fallocate`/`FSCTL_SET_ZERO_DATA`
/// call whose cost does not depend on the length, so a generous limit costs
/// nothing and keeps a guest `fstrim` of a big filesystem down to a handful of
/// segments. It is still a limit, because "unbounded" is not a number we can
/// reason about: at 1 GiB a segment's byte length stays far inside `u64` even
/// when multiplied out over a full request.
pub const MAX_DISCARD_SECTORS: u32 = 1 << 21;

/// `max_write_zeroes_sectors`: largest range one write-zeroes segment may
/// cover — 32 MiB, far smaller than the discard limit and deliberately so.
///
/// Write-zeroes must *always* leave zeros, so on a filesystem that cannot
/// punch holes the host has to write every one of those bytes. That work is
/// proportional to the range, which makes the limit a bound on how long one
/// guest request can occupy the device thread. A guest that wants to zero
/// more simply sends more requests, exactly as it already does for writes.
pub const MAX_WRITE_ZEROES_SECTORS: u32 = 1 << 16;

/// `max_discard_seg` / `max_write_zeroes_seg`: segments in one request.
///
/// 256 × 16 bytes is one 4 KiB page of segment array — and it is also Linux's
/// own `MAX_DISCARD_SEGMENTS`, so advertising more could never be used.
pub const MAX_DISCARD_SEG: u32 = 256;

/// Bytes of segment array one request may carry. Derived, not chosen: the
/// device refuses a longer device-readable payload before staging it.
pub const MAX_DISCARD_ARRAY_BYTES: u64 = MAX_DISCARD_SEG as u64 * DISCARD_SEGMENT_LEN as u64;

/// `write_zeroes_may_unmap`: we do deallocate when the driver sets `unmap`,
/// wherever the host filesystem can — that is the whole point of the feature
/// here, so saying so is honest and lets the guest prefer write-zeroes over a
/// full-length write of zero pages.
pub const WRITE_ZEROES_MAY_UNMAP: u8 = 1;

/// One `struct virtio_blk_discard_write_zeroes` segment, straight off the wire
/// and therefore entirely guest-controlled.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DiscardSegment {
    pub sector: u64,
    pub num_sectors: u32,
    /// Raw flags word — kept raw so reserved bits can be *refused* rather than
    /// masked away.
    pub flags: u32,
}

/// A validated reclaim range, in host-file terms.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReclaimRange {
    /// Byte offset in the backing image.
    pub offset: u64,
    /// Byte length. May be zero: a zero-sector segment is a legal no-op.
    pub len: u64,
    /// The driver set `VIRTIO_BLK_WRITE_ZEROES_FLAG_UNMAP`.
    pub unmap: bool,
}

impl DiscardSegment {
    /// Parses one segment from its little-endian wire form.
    pub fn parse(bytes: &[u8; DISCARD_SEGMENT_LEN]) -> Self {
        Self {
            sector: u64::from_le_bytes([
                bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7],
            ]),
            num_sectors: u32::from_le_bytes([bytes[8], bytes[9], bytes[10], bytes[11]]),
            flags: u32::from_le_bytes([bytes[12], bytes[13], bytes[14], bytes[15]]),
        }
    }

    /// Validates the segment for `kind` against the disk geometry and turns it
    /// into the byte range the host will act on.
    ///
    /// Everything a guest can choose is checked here, overflow-safely and
    /// without reference to the caller's order of operations:
    ///
    /// * reserved flag bits are refused ([`BlockError::ReservedFlags`]);
    /// * `unmap` is refused on a discard ([`BlockError::UnmapNotAllowed`]) —
    ///   the spec defines the bit for write-zeroes only;
    /// * the range is refused past the per-segment maximum for its command
    ///   ([`BlockError::ReclaimTooLarge`]);
    /// * `sector + num_sectors` is refused when it wraps or leaves the disk,
    ///   and so is a byte offset or end that does not fit a `u64`
    ///   ([`BlockError::OutOfRange`]).
    pub fn validate(
        &self,
        kind: RequestType,
        capacity_sectors: u64,
    ) -> Result<ReclaimRange, BlockError> {
        let max_sectors = match kind {
            RequestType::Discard => MAX_DISCARD_SECTORS,
            RequestType::WriteZeroes => MAX_WRITE_ZEROES_SECTORS,
            _ => return Err(BlockError::NotAReclaimRequest(kind as u32)),
        };
        if self.flags & !WRITE_ZEROES_FLAG_UNMAP != 0 {
            return Err(BlockError::ReservedFlags(self.flags));
        }
        let unmap = self.flags & WRITE_ZEROES_FLAG_UNMAP != 0;
        if unmap && kind == RequestType::Discard {
            return Err(BlockError::UnmapNotAllowed);
        }
        if self.num_sectors > max_sectors {
            return Err(BlockError::ReclaimTooLarge {
                num_sectors: self.num_sectors,
                max: max_sectors,
            });
        }
        let out_of_range = || BlockError::OutOfRange {
            sector: self.sector,
            len: u64::from(self.num_sectors).saturating_mul(SECTOR_SIZE),
            capacity: capacity_sectors,
        };
        let end = self
            .sector
            .checked_add(u64::from(self.num_sectors))
            .ok_or_else(out_of_range)?;
        if end > capacity_sectors {
            return Err(out_of_range());
        }
        // The backend works in bytes: both ends must be representable there
        // too, independently of how large a capacity the caller claimed.
        let offset = sector_offset(self.sector)?;
        let len = u64::from(self.num_sectors)
            .checked_mul(SECTOR_SIZE)
            .ok_or_else(out_of_range)?;
        offset.checked_add(len).ok_or_else(out_of_range)?;
        Ok(ReclaimRange { offset, len, unmap })
    }
}

/// How many segments a device-readable payload of `bytes` carries, refusing a
/// length that is not a whole number of segments, is empty, or is longer than
/// [`MAX_DISCARD_ARRAY_BYTES`].
///
/// The two failure modes are deliberately different errors: a payload that is
/// not a multiple of the segment size is a *malformed* request (the device
/// answers `S_UNSUPP`), while too many segments is a limit the driver was told
/// about and ignored (`S_IOERR`).
pub fn segment_count(bytes: u64) -> Result<usize, BlockError> {
    if bytes == 0 || bytes % DISCARD_SEGMENT_LEN as u64 != 0 {
        return Err(BlockError::BadSegmentArray(bytes));
    }
    if bytes > MAX_DISCARD_ARRAY_BYTES {
        let count = bytes / DISCARD_SEGMENT_LEN as u64;
        return Err(BlockError::TooManySegments {
            count,
            max: MAX_DISCARD_SEG,
        });
    }
    // Bounded by MAX_DISCARD_SEG above, so the cast cannot truncate.
    Ok((bytes / DISCARD_SEGMENT_LEN as u64) as usize)
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
        assert_eq!(RequestType::from_raw(11), Some(RequestType::Discard));
        assert_eq!(RequestType::from_raw(13), Some(RequestType::WriteZeroes));
        // 12 is SECURE_ERASE and 14 is GET_LIFETIME: real spec commands whose
        // features we do not offer, so they must still read back as unknown.
        assert_eq!(RequestType::from_raw(12), None);
        assert_eq!(RequestType::from_raw(14), None);
        assert!(RequestType::Discard.is_reclaim());
        assert!(RequestType::WriteZeroes.is_reclaim());
        assert!(!RequestType::Out.is_reclaim());
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

    /// Regression for the MVP-1402 fuzz finding: `validate_range` used to check
    /// only the geometry, so a payload above [`MAX_REQUEST_BYTES`] passed it as
    /// long as it fitted on the disk. The cap was enforced only because the
    /// device happened to call [`total_len`] first.
    #[test]
    fn validate_range_enforces_the_payload_cap_on_its_own() {
        // A disk big enough that geometry alone would accept the request.
        let capacity = (MAX_REQUEST_BYTES / SECTOR_SIZE) * 16;
        assert!(matches!(
            validate_range(capacity, 0, MAX_REQUEST_BYTES + SECTOR_SIZE),
            Err(BlockError::RequestTooLarge(_))
        ));
        assert!(matches!(
            validate_range(capacity, 0, 32 << 20),
            Err(BlockError::RequestTooLarge(_))
        ));
        // Exactly at the cap is still a legal request.
        assert!(validate_range(capacity, 0, MAX_REQUEST_BYTES).is_ok());
        // And the other checks are unchanged.
        assert!(matches!(
            validate_range(capacity, 0, SECTOR_SIZE + 1),
            Err(BlockError::UnalignedLength(_))
        ));
        assert!(matches!(
            validate_range(8, 4, 8 * SECTOR_SIZE),
            Err(BlockError::OutOfRange { .. })
        ));
    }

    /// Second MVP-1402 fuzz finding of the same shape: a sector whose *byte*
    /// offset overflows used to pass `validate_range` whenever the caller also
    /// claimed a capacity large enough to contain it, leaving `sector_offset` to
    /// fail afterwards. No real image can be that big, but the helper must not
    /// depend on its caller for that.
    #[test]
    fn validate_range_rejects_sectors_whose_byte_offset_overflows() {
        let absurd = u64::MAX / 8;
        assert!(matches!(
            validate_range(u64::MAX, absurd, 0),
            Err(BlockError::OutOfRange { .. })
        ));
        assert!(matches!(
            validate_range(u64::MAX, u64::MAX, SECTOR_SIZE),
            Err(BlockError::OutOfRange { .. })
        ));
        // The largest sector whose offset still fits is accepted, and its offset
        // is exactly what the backend will use.
        let last = u64::MAX / SECTOR_SIZE;
        assert!(validate_range(u64::MAX, last, 0).is_ok());
        assert!(sector_offset(last).is_ok());
    }

    // ------------------------------------------- discard / write-zeroes

    fn segment(sector: u64, num_sectors: u32, flags: u32) -> DiscardSegment {
        DiscardSegment {
            sector,
            num_sectors,
            flags,
        }
    }

    #[test]
    fn a_segment_parses_its_little_endian_fields() {
        let mut raw = [0u8; DISCARD_SEGMENT_LEN];
        raw[0..8].copy_from_slice(&0x1122_3344_5566u64.to_le_bytes());
        raw[8..12].copy_from_slice(&4096u32.to_le_bytes());
        raw[12..16].copy_from_slice(&1u32.to_le_bytes());
        let parsed = DiscardSegment::parse(&raw);
        assert_eq!(parsed, segment(0x1122_3344_5566, 4096, 1));
    }

    #[test]
    fn a_legal_segment_becomes_a_byte_range() {
        let range = segment(8, 8, 0)
            .validate(RequestType::Discard, CAP)
            .expect("in range");
        assert_eq!(
            range,
            ReclaimRange {
                offset: 8 * SECTOR_SIZE,
                len: 8 * SECTOR_SIZE,
                unmap: false,
            }
        );
        // Write-zeroes may ask for the unmap bit, and it is reported through.
        let range = segment(0, 64, WRITE_ZEROES_FLAG_UNMAP)
            .validate(RequestType::WriteZeroes, CAP)
            .expect("whole disk");
        assert!(range.unmap);
        assert_eq!(range.len, CAP * SECTOR_SIZE);
        // A zero-sector segment is a legal no-op, not an error.
        let range = segment(CAP, 0, 0)
            .validate(RequestType::Discard, CAP)
            .expect("empty segment at the end of the disk");
        assert_eq!(range.len, 0);
    }

    #[test]
    fn reserved_flag_bits_are_refused_never_masked() {
        for flags in [2u32, 4, 0x8000_0000, u32::MAX, WRITE_ZEROES_FLAG_UNMAP | 2] {
            assert!(
                matches!(
                    segment(0, 8, flags).validate(RequestType::WriteZeroes, CAP),
                    Err(BlockError::ReservedFlags(f)) if f == flags
                ),
                "flags {flags:#x} must be refused"
            );
            assert!(matches!(
                segment(0, 8, flags).validate(RequestType::Discard, CAP),
                Err(BlockError::ReservedFlags(_))
            ));
        }
    }

    #[test]
    fn unmap_is_a_write_zeroes_flag_only() {
        assert!(matches!(
            segment(0, 8, WRITE_ZEROES_FLAG_UNMAP).validate(RequestType::Discard, CAP),
            Err(BlockError::UnmapNotAllowed)
        ));
        assert!(segment(0, 8, WRITE_ZEROES_FLAG_UNMAP)
            .validate(RequestType::WriteZeroes, CAP)
            .is_ok());
    }

    #[test]
    fn a_segment_may_not_leave_the_disk_however_it_is_phrased() {
        let big = 1 << 20; // a 512 MiB disk in sectors
        for (sector, num_sectors) in [
            (big, 1),                    // starts at the end
            (big - 1, 2),                // ends one sector past it
            (0, u32::MAX),               // absurd length
            (u64::MAX, 8),               // start overflows the addition
            (u64::MAX - 4, 8),           // end wraps
            (u64::MAX / SECTOR_SIZE, 8), // byte offset would overflow
        ] {
            assert!(
                matches!(
                    segment(sector, num_sectors, 0).validate(RequestType::Discard, big),
                    Err(BlockError::OutOfRange { .. } | BlockError::ReclaimTooLarge { .. })
                ),
                "sector {sector} + {num_sectors} must be refused"
            );
        }
        // Right up to the last sector is fine.
        assert!(segment(big - 1, 1, 0)
            .validate(RequestType::Discard, big)
            .is_ok());
    }

    #[test]
    fn the_per_command_maximum_is_enforced_and_differs_by_command() {
        // A disk far larger than either limit, so geometry cannot be what
        // refuses these.
        let huge = 1u64 << 40;
        assert!(segment(0, MAX_DISCARD_SECTORS, 0)
            .validate(RequestType::Discard, huge)
            .is_ok());
        assert!(matches!(
            segment(0, MAX_DISCARD_SECTORS + 1, 0).validate(RequestType::Discard, huge),
            Err(BlockError::ReclaimTooLarge { max, .. }) if max == MAX_DISCARD_SECTORS
        ));
        assert!(segment(0, MAX_WRITE_ZEROES_SECTORS, 0)
            .validate(RequestType::WriteZeroes, huge)
            .is_ok());
        assert!(matches!(
            segment(0, MAX_WRITE_ZEROES_SECTORS + 1, 0).validate(RequestType::WriteZeroes, huge),
            Err(BlockError::ReclaimTooLarge { max, .. }) if max == MAX_WRITE_ZEROES_SECTORS
        ));
        // The write-zeroes limit really is the tighter one: its fallback has to
        // write every byte.
        const { assert!(MAX_WRITE_ZEROES_SECTORS < MAX_DISCARD_SECTORS) };
        // Neither command accepts a segment belonging to some other request.
        assert!(matches!(
            segment(0, 8, 0).validate(RequestType::Out, huge),
            Err(BlockError::NotAReclaimRequest(1))
        ));
    }

    #[test]
    fn the_segment_array_length_must_describe_whole_segments_and_not_too_many() {
        assert_eq!(segment_count(DISCARD_SEGMENT_LEN as u64).unwrap(), 1);
        assert_eq!(
            segment_count(MAX_DISCARD_ARRAY_BYTES).unwrap(),
            MAX_DISCARD_SEG as usize
        );
        for bad in [0u64, 1, 15, 17, 4095] {
            assert!(
                matches!(segment_count(bad), Err(BlockError::BadSegmentArray(b)) if b == bad),
                "{bad} bytes is not a segment array"
            );
        }
        assert!(matches!(
            segment_count(MAX_DISCARD_ARRAY_BYTES + DISCARD_SEGMENT_LEN as u64),
            Err(BlockError::TooManySegments { count, max })
                if count == u64::from(MAX_DISCARD_SEG) + 1 && max == MAX_DISCARD_SEG
        ));
        // A payload the device could never have staged in the first place is
        // still refused arithmetically rather than truncated.
        assert!(matches!(
            segment_count(u64::MAX - 15),
            Err(BlockError::TooManySegments { .. })
        ));
    }

    /// The config values the guest is told about have to be the ones validation
    /// actually enforces, or a well-behaved driver gets `S_IOERR` for a request
    /// we invited.
    #[test]
    fn the_advertised_limits_are_the_enforced_limits() {
        assert_eq!(MAX_DISCARD_ARRAY_BYTES, 4096);
        assert_eq!(DISCARD_SECTOR_ALIGNMENT, 8);
        assert_eq!(WRITE_ZEROES_MAY_UNMAP, 1);
        let huge = 1u64 << 40;
        // Exactly max_discard_seg segments of exactly max_discard_sectors each
        // is a legal request in full.
        let count = segment_count(MAX_DISCARD_ARRAY_BYTES).unwrap();
        assert_eq!(count, MAX_DISCARD_SEG as usize);
        for i in 0..count as u64 {
            assert!(
                segment(i * u64::from(MAX_DISCARD_SECTORS), MAX_DISCARD_SECTORS, 0)
                    .validate(RequestType::Discard, huge)
                    .is_ok()
            );
        }
    }
}
