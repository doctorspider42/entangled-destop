//! Fuzzes virtio-blk request parsing and validation (backlog MVP-1402, EPIC 4).
//!
//! The 16-byte request header and the geometry checks around it are the first
//! thing a guest's disk request touches, and the only thing standing between a
//! guest-chosen sector number and a `pread` on the host's image file. This target
//! drives header parsing, `validate_range`, `sector_offset` and `total_len` with
//! arbitrary bytes and arbitrary geometry.
//!
//! Properties checked:
//!
//! * parsing never panics and is total over all 16-byte inputs;
//! * an accepted range really is inside the disk, and its byte offset does not
//!   overflow — the two facts the backend then relies on;
//! * `total_len` never overflows however the guest sizes its descriptors.

#![no_main]

use arbitrary::Arbitrary;
use libfuzzer_sys::fuzz_target;
use virtio_block::{
    sector_offset, total_len, validate_range, BlockError, RequestHeader, RequestType,
    MAX_REQUEST_BYTES, REQUEST_HEADER_LEN, SECTOR_SIZE,
};

#[derive(Debug, Arbitrary)]
struct Input {
    header: [u8; REQUEST_HEADER_LEN],
    capacity_sectors: u64,
    len: u64,
    /// Descriptor lengths, as the guest would program them.
    segment_lens: Vec<u32>,
}

fuzz_target!(|input: Input| {
    let header = RequestHeader::parse(&input.header);
    // Parsing is a pure reinterpretation of the bytes; the raw type must survive
    // unmapped so an unknown request can be answered with S_UNSUPP.
    assert_eq!(
        header.raw_type,
        u32::from_le_bytes([
            input.header[0],
            input.header[1],
            input.header[2],
            input.header[3]
        ])
    );
    match header.request_type() {
        Some(t) => assert_eq!(
            header.raw_type,
            match t {
                RequestType::In => 0,
                RequestType::Out => 1,
                RequestType::Flush => 4,
                RequestType::GetId => 8,
            }
        ),
        None => assert!(!matches!(header.raw_type, 0 | 1 | 4 | 8)),
    }

    // Guest-chosen sector plus guest-chosen length against guest-chosen
    // geometry: the interesting three-way interaction.
    match validate_range(input.capacity_sectors, header.sector, input.len) {
        Ok(()) => {
            assert_eq!(
                input.len % SECTOR_SIZE,
                0,
                "accepted an unaligned length {}",
                input.len
            );
            assert!(
                input.len <= MAX_REQUEST_BYTES,
                "accepted {} bytes, above the {MAX_REQUEST_BYTES}-byte cap",
                input.len
            );
            let sectors = input.len / SECTOR_SIZE;
            let end = header
                .sector
                .checked_add(sectors)
                .expect("an accepted range must not overflow the sector count");
            assert!(
                end <= input.capacity_sectors,
                "accepted sectors {}..{end} on a {}-sector disk",
                header.sector,
                input.capacity_sectors
            );
            // The backend turns the accepted sector into a byte offset; that
            // must be representable, which is what makes the pread safe.
            let offset = sector_offset(header.sector)
                .expect("an accepted sector must have a byte offset");
            assert!(offset.checked_add(input.len).is_some());
        }
        Err(
            BlockError::OutOfRange { .. }
            | BlockError::UnalignedLength(_)
            | BlockError::RequestTooLarge(_)
            | BlockError::ReadOnly,
        ) => (),
        Err(other) => panic!("unexpected validation error variant: {other}"),
    }

    // Descriptor-length summation must be overflow-safe for any chain shape, and
    // must refuse anything above the payload cap rather than wrapping.
    let lengths: Vec<u32> = input.segment_lens.iter().copied().take(256).collect();
    match total_len(lengths.iter().copied()) {
        Ok(total) => {
            let expected: u64 = lengths.iter().map(|&l| u64::from(l)).sum();
            assert_eq!(total, expected, "total_len must not lose bytes");
            assert!(
                total <= MAX_REQUEST_BYTES,
                "total_len accepted {total} bytes, above the cap"
            );
        }
        Err(BlockError::RequestTooLarge(_)) => (),
        Err(other) => panic!("unexpected total_len error variant: {other}"),
    }
});
