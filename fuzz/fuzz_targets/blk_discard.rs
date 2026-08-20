//! Fuzzes the virtio-blk DISCARD / WRITE_ZEROES request path (backlog
//! MVP-1402, EPIC 4).
//!
//! `blk_request` covers the 16-byte request header and ordinary I/O geometry.
//! The reclaim commands add a second, richer surface: instead of one sector and
//! one length, the guest hands the device an **array** of
//! `struct virtio_blk_discard_write_zeroes` — sector, num_sectors and a flags
//! word — and every one of those becomes a byte range the host is about to
//! punch out of a real image file. A range that escaped the file here would
//! destroy data outside the disk; a range that overflowed would wrap into one.
//!
//! So this target drives, with arbitrary bytes and arbitrary geometry:
//!
//! * `segment_count` — the array's *shape*: a length that is not a whole number
//!   of segments, an empty payload, more segments than advertised;
//! * `DiscardSegment::parse` — total over all 16-byte inputs;
//! * `DiscardSegment::validate` — the three-way interaction between a
//!   guest-chosen sector, a guest-chosen length and a guest-chosen capacity,
//!   for both commands.
//!
//! Properties checked (the ones the host then relies on):
//!
//! * parsing never panics and preserves the raw flags word, so reserved bits
//!   can be refused rather than masked;
//! * an accepted range is inside the disk, its byte offset and end are
//!   representable, and its length is a whole number of sectors;
//! * an accepted range never exceeds the maximum advertised for its command;
//! * `unmap` is accepted only for write-zeroes, and only from the one defined
//!   flag bit;
//! * an accepted segment count really is at most the advertised maximum, and
//!   the byte length it came from really was that many whole segments.

#![no_main]

use arbitrary::Arbitrary;
use libfuzzer_sys::fuzz_target;
use virtio_block::{
    segment_count, BlockError, DiscardSegment, RequestType, DISCARD_SEGMENT_LEN,
    MAX_DISCARD_ARRAY_BYTES, MAX_DISCARD_SECTORS, MAX_DISCARD_SEG, MAX_WRITE_ZEROES_SECTORS,
    SECTOR_SIZE, WRITE_ZEROES_FLAG_UNMAP,
};

#[derive(Debug, Arbitrary)]
struct Input {
    /// One segment exactly as it arrives on the wire.
    segment: [u8; DISCARD_SEGMENT_LEN],
    /// The disk the segment is checked against.
    capacity_sectors: u64,
    /// Which of the two reclaim commands this is.
    write_zeroes: bool,
    /// Length of the device-readable payload the guest programmed.
    payload_bytes: u64,
    /// A second segment, so a *pair* is exercised too: the device validates the
    /// whole array before acting, and a mixed array is the interesting case.
    other: [u8; DISCARD_SEGMENT_LEN],
}

fuzz_target!(|input: Input| {
    let kind = if input.write_zeroes {
        RequestType::WriteZeroes
    } else {
        RequestType::Discard
    };
    let max_sectors = if input.write_zeroes {
        MAX_WRITE_ZEROES_SECTORS
    } else {
        MAX_DISCARD_SECTORS
    };

    // --- the array's shape -------------------------------------------------
    match segment_count(input.payload_bytes) {
        Ok(count) => {
            assert!(
                count <= MAX_DISCARD_SEG as usize,
                "accepted {count} segments, above the advertised {MAX_DISCARD_SEG}"
            );
            assert_ne!(count, 0, "an empty segment array must never be accepted");
            assert_eq!(
                count as u64 * DISCARD_SEGMENT_LEN as u64,
                input.payload_bytes,
                "an accepted payload must be exactly that many whole segments"
            );
            assert!(input.payload_bytes <= MAX_DISCARD_ARRAY_BYTES);
        }
        Err(BlockError::BadSegmentArray(bytes)) => {
            assert_eq!(bytes, input.payload_bytes);
            assert!(bytes == 0 || bytes % DISCARD_SEGMENT_LEN as u64 != 0);
        }
        Err(BlockError::TooManySegments { count, max }) => {
            assert_eq!(max, MAX_DISCARD_SEG);
            assert!(count > u64::from(max));
            assert!(input.payload_bytes > MAX_DISCARD_ARRAY_BYTES);
        }
        Err(other) => panic!("unexpected segment_count error variant: {other}"),
    }

    // --- one segment -------------------------------------------------------
    for raw in [&input.segment, &input.other] {
        let segment = DiscardSegment::parse(raw);
        // Parsing is a pure reinterpretation; the flags word in particular must
        // survive unmasked so reserved bits can be refused.
        assert_eq!(
            segment.flags,
            u32::from_le_bytes([raw[12], raw[13], raw[14], raw[15]])
        );
        assert_eq!(
            segment.num_sectors,
            u32::from_le_bytes([raw[8], raw[9], raw[10], raw[11]])
        );

        match segment.validate(kind, input.capacity_sectors) {
            Ok(range) => {
                // Only the one defined flag bit may have been set.
                assert_eq!(segment.flags & !WRITE_ZEROES_FLAG_UNMAP, 0);
                assert_eq!(range.unmap, segment.flags & WRITE_ZEROES_FLAG_UNMAP != 0);
                assert!(
                    !range.unmap || input.write_zeroes,
                    "unmap is a write-zeroes flag only"
                );
                assert!(
                    segment.num_sectors <= max_sectors,
                    "accepted {} sectors, above the advertised {max_sectors}",
                    segment.num_sectors
                );
                // The range the host is about to punch must be inside the disk
                // and expressible in bytes — this is the whole point.
                let end_sector = segment
                    .sector
                    .checked_add(u64::from(segment.num_sectors))
                    .expect("an accepted range must not overflow the sector count");
                assert!(
                    end_sector <= input.capacity_sectors,
                    "accepted sectors {}..{end_sector} on a {}-sector disk",
                    segment.sector,
                    input.capacity_sectors
                );
                assert_eq!(range.offset, segment.sector * SECTOR_SIZE);
                assert_eq!(range.len, u64::from(segment.num_sectors) * SECTOR_SIZE);
                assert_eq!(range.len % SECTOR_SIZE, 0);
                assert!(
                    range.offset.checked_add(range.len).is_some(),
                    "an accepted range must have a representable end"
                );
                assert_eq!(
                    range.offset.checked_add(range.len),
                    end_sector.checked_mul(SECTOR_SIZE),
                    "the byte range must be exactly the sector range, in bytes"
                );
            }
            Err(BlockError::ReservedFlags(flags)) => {
                assert_eq!(flags, segment.flags);
                assert_ne!(flags & !WRITE_ZEROES_FLAG_UNMAP, 0);
            }
            Err(BlockError::UnmapNotAllowed) => {
                assert!(!input.write_zeroes);
                assert_ne!(segment.flags & WRITE_ZEROES_FLAG_UNMAP, 0);
            }
            Err(BlockError::ReclaimTooLarge { num_sectors, max }) => {
                assert_eq!(max, max_sectors);
                assert!(num_sectors > max);
            }
            Err(BlockError::OutOfRange { .. }) => (),
            Err(other) => panic!("unexpected validate error variant: {other}"),
        }
    }
});
