//! The virtio-net wire format and all of its pure validation (backlog
//! MVP-501/502/508).
//!
//! Everything here is portable and side-effect free: it is the layer the bulk
//! of the unit tests live in, and the layer that decides whether a
//! guest-supplied buffer is a frame VMHost is willing to hand to the host
//! network stack.
//!
//! # Header size
//!
//! `struct virtio_net_hdr` is 10 bytes; `struct virtio_net_hdr_mrg_rxbuf`
//! appends a `num_buffers` field for a total of 12. VMHost does **not** offer
//! `VIRTIO_NET_F_MRG_RXBUF`, but the header is still 12 bytes long, because
//! under `VIRTIO_F_VERSION_1` the driver uses the larger layout regardless —
//! Linux' `virtio_net.c` picks `sizeof(struct virtio_net_hdr_mrg_rxbuf)` when
//! *either* `MRG_RXBUF` or `VERSION_1` is negotiated. The trailing
//! `num_buffers` field is simply unused; VMHost writes it as zero on RX and
//! ignores it on TX.
//!
//! # Frame cap
//!
//! One buffer holds one whole frame: 14 bytes of Ethernet header plus a
//! 1500-byte MTU, [`MAX_FRAME_LEN`]. Anything longer is dropped — with no
//! offloads negotiated the guest may not send a segment the host would have to
//! split, and with no mergeable buffers a frame may not span RX descriptors
//! chains. VLAN-tagged frames (1518 bytes) are out of scope for the MVP.

use thiserror::Error;

/// Size of the virtio-net header in front of every frame, both directions.
pub const VIRTIO_NET_HDR_LEN: usize = 12;

/// Length of an Ethernet II header (destination, source, ethertype).
pub const ETH_HEADER_LEN: usize = 14;

/// Guest MTU VMHost assumes. Not advertised (`VIRTIO_NET_F_MTU` is not
/// offered), so this is purely the host-side cap.
pub const MTU: usize = 1500;

/// Largest frame VMHost moves in either direction.
pub const MAX_FRAME_LEN: usize = ETH_HEADER_LEN + MTU;

/// Largest header+frame buffer, i.e. the cap on one descriptor chain's payload.
pub const MAX_BUFFER_LEN: usize = VIRTIO_NET_HDR_LEN + MAX_FRAME_LEN;

/// `VIRTIO_NET_HDR_GSO_NONE`: no segmentation offload requested. The only
/// value VMHost accepts, since no GSO feature is negotiated.
pub const VIRTIO_NET_HDR_GSO_NONE: u8 = 0;

/// Reasons a guest-supplied TX chain is not a frame we can transmit.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum FrameError {
    #[error(
        "chain holds {len} device-readable bytes, too few for the \
         {VIRTIO_NET_HDR_LEN}-byte virtio-net header"
    )]
    NoHeader { len: usize },

    #[error("chain holds a header but no frame")]
    Empty,

    #[error("frame is {len} bytes, shorter than the {ETH_HEADER_LEN}-byte Ethernet header")]
    TooShort { len: usize },

    #[error("frame is {len} bytes, above the {MAX_FRAME_LEN}-byte cap")]
    TooLong { len: usize },

    #[error(
        "header requests offloading (flags {flags:#x}, gso_type {gso_type:#x}) \
         but no offload feature is negotiated"
    )]
    OffloadRequested { flags: u8, gso_type: u8 },
}

/// The 12-byte virtio-net header (VirtIO spec 1.2, section 5.1.6).
///
/// Parsed for validation only: with no offloads negotiated every field except
/// the layout itself must be zero, so nothing here is ever applied to a frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct NetHeader {
    pub flags: u8,
    pub gso_type: u8,
    pub hdr_len: u16,
    pub gso_size: u16,
    pub csum_start: u16,
    pub csum_offset: u16,
    /// Only meaningful with `VIRTIO_NET_F_MRG_RXBUF`, which VMHost does not
    /// offer. Always written as 0 on RX, ignored on TX.
    pub num_buffers: u16,
}

impl NetHeader {
    /// Parses a header out of exactly [`VIRTIO_NET_HDR_LEN`] guest bytes.
    /// Total function: every bit pattern parses.
    pub fn parse(raw: &[u8; VIRTIO_NET_HDR_LEN]) -> Self {
        let le16 = |a: usize, b: usize| u16::from_le_bytes([raw[a], raw[b]]);
        Self {
            flags: raw[0],
            gso_type: raw[1],
            hdr_len: le16(2, 3),
            gso_size: le16(4, 5),
            csum_start: le16(6, 7),
            csum_offset: le16(8, 9),
            num_buffers: le16(10, 11),
        }
    }

    /// Serialises the header, for RX and for tests.
    pub fn to_bytes(self) -> [u8; VIRTIO_NET_HDR_LEN] {
        let mut raw = [0u8; VIRTIO_NET_HDR_LEN];
        raw[0] = self.flags;
        raw[1] = self.gso_type;
        raw[2..4].copy_from_slice(&self.hdr_len.to_le_bytes());
        raw[4..6].copy_from_slice(&self.gso_size.to_le_bytes());
        raw[6..8].copy_from_slice(&self.csum_start.to_le_bytes());
        raw[8..10].copy_from_slice(&self.csum_offset.to_le_bytes());
        raw[10..12].copy_from_slice(&self.num_buffers.to_le_bytes());
        raw
    }

    /// True when the guest asks for checksum or segmentation offloading.
    /// Neither is negotiated, so such a frame cannot be transmitted as-is.
    pub fn requests_offload(self) -> bool {
        self.flags != 0 || self.gso_type != VIRTIO_NET_HDR_GSO_NONE
    }

    /// The header VMHost prepends to every received frame: all zeroes, so
    /// no offload flag is claimed and `num_buffers` is 0.
    pub const fn rx() -> [u8; VIRTIO_NET_HDR_LEN] {
        [0u8; VIRTIO_NET_HDR_LEN]
    }
}

/// Validates a gathered TX buffer (header followed by frame bytes) and returns
/// the frame length.
///
/// `buffer` is everything the guest made device-readable in one chain, already
/// copied out of guest memory. The checks are exactly the ones the MVP feature
/// set implies: a complete header, a frame at least as long as an Ethernet
/// header, no more than [`MAX_FRAME_LEN`] bytes, and no offload request.
pub fn validate_tx_buffer(buffer: &[u8]) -> Result<usize, FrameError> {
    if buffer.len() < VIRTIO_NET_HDR_LEN {
        return Err(FrameError::NoHeader { len: buffer.len() });
    }
    let (header, frame) = buffer.split_at(VIRTIO_NET_HDR_LEN);
    let mut raw = [0u8; VIRTIO_NET_HDR_LEN];
    // Exactly VIRTIO_NET_HDR_LEN bytes by construction of `split_at`.
    raw.copy_from_slice(header);
    let header = NetHeader::parse(&raw);
    if header.requests_offload() {
        return Err(FrameError::OffloadRequested {
            flags: header.flags,
            gso_type: header.gso_type,
        });
    }
    if frame.is_empty() {
        return Err(FrameError::Empty);
    }
    if frame.len() < ETH_HEADER_LEN {
        return Err(FrameError::TooShort { len: frame.len() });
    }
    if frame.len() > MAX_FRAME_LEN {
        return Err(FrameError::TooLong { len: frame.len() });
    }
    Ok(frame.len())
}

/// Validates a frame the host handed us before it goes into an RX chain.
pub fn validate_rx_frame(frame: &[u8]) -> Result<(), FrameError> {
    if frame.is_empty() {
        return Err(FrameError::Empty);
    }
    if frame.len() < ETH_HEADER_LEN {
        return Err(FrameError::TooShort { len: frame.len() });
    }
    if frame.len() > MAX_FRAME_LEN {
        return Err(FrameError::TooLong { len: frame.len() });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_is_twelve_bytes_under_version_1() {
        // The number Linux' virtio_net.c uses for VERSION_1 devices; getting it
        // wrong shifts every frame by two bytes.
        assert_eq!(VIRTIO_NET_HDR_LEN, 12);
        assert_eq!(NetHeader::rx().len(), 12);
        assert_eq!(MAX_FRAME_LEN, 1514);
        assert_eq!(MAX_BUFFER_LEN, 1526);
    }

    #[test]
    fn header_round_trips_little_endian() {
        let header = NetHeader {
            flags: 0x01,
            gso_type: 0x03,
            hdr_len: 0x1234,
            gso_size: 0x5678,
            csum_start: 0x9abc,
            csum_offset: 0xdef0,
            num_buffers: 0x0102,
        };
        let raw = header.to_bytes();
        assert_eq!(raw[0], 0x01);
        assert_eq!(raw[1], 0x03);
        assert_eq!(&raw[2..4], &[0x34, 0x12]);
        assert_eq!(&raw[10..12], &[0x02, 0x01]);
        assert_eq!(NetHeader::parse(&raw), header);
    }

    #[test]
    fn rx_header_is_all_zero_including_num_buffers() {
        let raw = NetHeader::rx();
        assert_eq!(raw, [0u8; VIRTIO_NET_HDR_LEN]);
        let parsed = NetHeader::parse(&raw);
        assert_eq!(parsed.num_buffers, 0);
        assert!(!parsed.requests_offload());
    }

    #[test]
    fn offload_requests_are_detected() {
        let mut raw = NetHeader::rx();
        raw[0] = 1; // VIRTIO_NET_HDR_F_NEEDS_CSUM
        assert!(NetHeader::parse(&raw).requests_offload());
        let mut raw = NetHeader::rx();
        raw[1] = 1; // VIRTIO_NET_HDR_GSO_TCPV4
        assert!(NetHeader::parse(&raw).requests_offload());
    }

    fn buffer(frame_len: usize) -> Vec<u8> {
        vec![0u8; VIRTIO_NET_HDR_LEN + frame_len]
    }

    #[test]
    fn well_formed_tx_buffer_is_accepted() {
        assert_eq!(validate_tx_buffer(&buffer(ETH_HEADER_LEN)), Ok(14));
        assert_eq!(validate_tx_buffer(&buffer(60)), Ok(60));
        assert_eq!(
            validate_tx_buffer(&buffer(MAX_FRAME_LEN)),
            Ok(MAX_FRAME_LEN)
        );
    }

    #[test]
    fn truncated_header_is_rejected() {
        for len in 0..VIRTIO_NET_HDR_LEN {
            assert_eq!(
                validate_tx_buffer(&vec![0u8; len]),
                Err(FrameError::NoHeader { len }),
                "a {len}-byte chain cannot hold a header"
            );
        }
    }

    #[test]
    fn empty_and_runt_frames_are_rejected() {
        assert_eq!(validate_tx_buffer(&buffer(0)), Err(FrameError::Empty));
        for len in 1..ETH_HEADER_LEN {
            assert_eq!(
                validate_tx_buffer(&buffer(len)),
                Err(FrameError::TooShort { len })
            );
        }
    }

    #[test]
    fn oversized_frames_are_rejected() {
        assert_eq!(
            validate_tx_buffer(&buffer(MAX_FRAME_LEN + 1)),
            Err(FrameError::TooLong {
                len: MAX_FRAME_LEN + 1
            })
        );
        assert_eq!(
            validate_tx_buffer(&buffer(64 * 1024)),
            Err(FrameError::TooLong { len: 64 * 1024 })
        );
    }

    #[test]
    fn offloaded_frames_are_rejected() {
        let mut buf = buffer(64);
        buf[0] = 1;
        assert_eq!(
            validate_tx_buffer(&buf),
            Err(FrameError::OffloadRequested {
                flags: 1,
                gso_type: 0
            })
        );
        let mut buf = buffer(64);
        buf[1] = 3;
        assert_eq!(
            validate_tx_buffer(&buf),
            Err(FrameError::OffloadRequested {
                flags: 0,
                gso_type: 3
            })
        );
    }

    #[test]
    fn rx_frames_are_validated_the_same_way() {
        assert_eq!(validate_rx_frame(&[]), Err(FrameError::Empty));
        assert_eq!(
            validate_rx_frame(&[0u8; 13]),
            Err(FrameError::TooShort { len: 13 })
        );
        assert_eq!(validate_rx_frame(&[0u8; ETH_HEADER_LEN]), Ok(()));
        assert_eq!(validate_rx_frame(&vec![0u8; MAX_FRAME_LEN]), Ok(()));
        assert_eq!(
            validate_rx_frame(&vec![0u8; MAX_FRAME_LEN + 1]),
            Err(FrameError::TooLong {
                len: MAX_FRAME_LEN + 1
            })
        );
    }
}
