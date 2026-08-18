//! Virtqueue descriptor-chain safety (backlog MVP-304/308/309).
//!
//! The wire format comes from the `virtio-queue` crate (`RawDescriptor` /
//! `desc::split::Descriptor`); this module owns the Entangled Desktop policy on top of
//! it:
//!
//! * every chain walk is bounded ([`MAX_DESC_CHAIN_LEN`]) so a malicious guest
//!   cannot loop the host,
//! * every descriptor index is checked against the ring size,
//! * every descriptor-table read goes through `vm-memory`'s checked API, so a
//!   descriptor table pointing outside guest RAM fails the request instead of
//!   dereferencing a bad host address,
//! * indirect descriptors are rejected outright — Entangled Desktop does not offer
//!   `VIRTIO_F_INDIRECT_DESC`, so a chain using them is a protocol violation.

use thiserror::Error;
use virtio_queue::desc::split::Descriptor as SplitDescriptor;
use virtio_queue::desc::RawDescriptor;
use vm_memory::{Bytes, GuestAddress};

use crate::GuestMem;

/// Upper bound on descriptors per chain. The spec allows up to queue size;
/// we cap harder because no MVP device legitimately needs longer chains.
pub const MAX_DESC_CHAIN_LEN: u16 = 128;

/// Size of one split-ring descriptor (`struct virtq_desc`).
pub const DESC_SIZE: u64 = 16;

/// Split-ring descriptor flags (VirtIO spec 1.2, section 2.7.5).
pub const VIRTQ_DESC_F_NEXT: u16 = 1;
/// The buffer is device-writable (driver-readable) rather than device-readable.
pub const VIRTQ_DESC_F_WRITE: u16 = 2;
/// The buffer contains an indirect descriptor table — never negotiated here.
pub const VIRTQ_DESC_F_INDIRECT: u16 = 4;

#[derive(Debug, Error, PartialEq, Eq)]
pub enum ChainError {
    #[error("descriptor chain exceeds {MAX_DESC_CHAIN_LEN} entries (looped or malicious)")]
    TooLong,

    #[error("descriptor index {index} out of range for queue size {queue_size}")]
    IndexOutOfRange { index: u16, queue_size: u16 },

    #[error("device-writable descriptor precedes driver-readable one")]
    WriteBeforeRead,

    #[error("descriptor table entry {index} at {addr:#x} is not readable guest memory: {reason}")]
    DescriptorUnreadable {
        index: u16,
        addr: u64,
        reason: String,
    },

    #[error("descriptor table address {table:#x} overflows with index {index}")]
    DescriptorTableOverflow { table: u64, index: u16 },

    #[error("indirect descriptors are not supported (VIRTIO_F_INDIRECT_DESC is not offered)")]
    IndirectNotSupported,
}

/// Guards one walk over a descriptor chain. Call [`step`](Self::step) for
/// every descriptor visited; the guard errors out instead of letting the
/// walk run away.
#[derive(Debug)]
pub struct ChainWalkGuard {
    queue_size: u16,
    steps: u16,
}

impl ChainWalkGuard {
    pub fn new(queue_size: u16) -> Self {
        Self {
            queue_size,
            steps: 0,
        }
    }

    /// Records a visit to descriptor `index`. Errors if the walk exceeds
    /// [`MAX_DESC_CHAIN_LEN`] or the index is outside the ring.
    pub fn step(&mut self, index: u16) -> Result<(), ChainError> {
        if index >= self.queue_size {
            return Err(ChainError::IndexOutOfRange {
                index,
                queue_size: self.queue_size,
            });
        }
        self.steps += 1;
        if self.steps > MAX_DESC_CHAIN_LEN.min(self.queue_size) {
            return Err(ChainError::TooLong);
        }
        Ok(())
    }

    /// Number of descriptors visited so far.
    pub fn steps(&self) -> u16 {
        self.steps
    }
}

/// One buffer of a guest descriptor chain, as handed to a device.
///
/// `addr`/`len` are still *guest supplied*: they have not been range-checked
/// against guest RAM. Devices must only reach them through the checked
/// `vm-memory` APIs, which is what makes an out-of-range buffer an I/O error
/// rather than a host memory-safety problem.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Segment {
    /// Guest physical address of the buffer.
    pub addr: u64,
    /// Buffer length in bytes.
    pub len: u32,
    /// True when the buffer is device-writable (`VIRTQ_DESC_F_WRITE`).
    pub writable: bool,
}

/// Walks the descriptor chain starting at `head` and returns its buffers.
///
/// The walk is bounded by [`MAX_DESC_CHAIN_LEN`] and the ring size, every
/// index is validated, and every descriptor-table read is a checked guest
/// memory access — a looped chain, an index past the ring or a descriptor
/// table outside guest RAM all produce a [`ChainError`] instead of touching
/// host memory.
pub fn walk(
    mem: &GuestMem,
    desc_table: u64,
    queue_size: u16,
    head: u16,
) -> Result<Vec<Segment>, ChainError> {
    let mut guard = ChainWalkGuard::new(queue_size);
    let capacity = usize::from(MAX_DESC_CHAIN_LEN.min(queue_size)).min(16);
    let mut segments = Vec::with_capacity(capacity);
    let mut index = head;
    loop {
        guard.step(index)?;
        let addr = desc_table
            .checked_add(u64::from(index).saturating_mul(DESC_SIZE))
            .ok_or(ChainError::DescriptorTableOverflow {
                table: desc_table,
                index,
            })?;
        let raw: RawDescriptor =
            mem.read_obj(GuestAddress(addr))
                .map_err(|e| ChainError::DescriptorUnreadable {
                    index,
                    addr,
                    reason: e.to_string(),
                })?;
        let desc = SplitDescriptor::from(raw);
        if desc.flags() & VIRTQ_DESC_F_INDIRECT != 0 {
            return Err(ChainError::IndirectNotSupported);
        }
        segments.push(Segment {
            addr: desc.addr().0,
            len: desc.len(),
            writable: desc.flags() & VIRTQ_DESC_F_WRITE != 0,
        });
        if desc.flags() & VIRTQ_DESC_F_NEXT == 0 {
            return Ok(segments);
        }
        index = desc.next();
    }
}

/// Splits a chain into its device-readable prefix and device-writable suffix.
///
/// The spec requires all device-readable descriptors to come first; a chain
/// that interleaves them is rejected with [`ChainError::WriteBeforeRead`].
pub fn split_rw(segments: &[Segment]) -> Result<(&[Segment], &[Segment]), ChainError> {
    let split = segments
        .iter()
        .position(|s| s.writable)
        .unwrap_or(segments.len());
    let (readable, writable) = segments.split_at(split);
    if writable.iter().any(|s| !s.writable) {
        return Err(ChainError::WriteBeforeRead);
    }
    Ok((readable, writable))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing;

    #[test]
    fn bounded_walk_passes() {
        let mut g = ChainWalkGuard::new(256);
        for i in 0..MAX_DESC_CHAIN_LEN {
            assert!(g.step(i).is_ok());
        }
        assert_eq!(g.steps(), MAX_DESC_CHAIN_LEN);
    }

    #[test]
    fn looped_chain_is_cut_off() {
        let mut g = ChainWalkGuard::new(256);
        let mut result = Ok(());
        for _ in 0..u16::MAX {
            result = g.step(0);
            if result.is_err() {
                break;
            }
        }
        assert_eq!(result, Err(ChainError::TooLong));
    }

    #[test]
    fn index_beyond_ring_rejected() {
        let mut g = ChainWalkGuard::new(8);
        assert_eq!(
            g.step(8),
            Err(ChainError::IndexOutOfRange {
                index: 8,
                queue_size: 8
            })
        );
    }

    #[test]
    fn small_queue_caps_at_queue_size() {
        let mut g = ChainWalkGuard::new(4);
        for i in 0..4 {
            assert!(g.step(i).is_ok());
        }
        assert_eq!(g.step(0), Err(ChainError::TooLong));
    }

    #[test]
    fn walks_a_two_buffer_chain() {
        let mem = testing::guest_memory(0x2_0000);
        let ring = testing::SplitRing::layout(0x1000, 16);
        ring.write_desc(&mem, 0, 0x8000, 16, VIRTQ_DESC_F_NEXT, 1);
        ring.write_desc(&mem, 1, 0x9000, 1, VIRTQ_DESC_F_WRITE, 0);

        let segments = walk(&mem, ring.desc_table(), 16, 0).expect("chain walks");
        assert_eq!(
            segments,
            vec![
                Segment {
                    addr: 0x8000,
                    len: 16,
                    writable: false
                },
                Segment {
                    addr: 0x9000,
                    len: 1,
                    writable: true
                },
            ]
        );
    }

    #[test]
    fn self_referencing_chain_is_rejected() {
        let mem = testing::guest_memory(0x2_0000);
        let ring = testing::SplitRing::layout(0x1000, 16);
        // Descriptor 0 points at itself: an infinite chain.
        ring.write_desc(&mem, 0, 0x8000, 16, VIRTQ_DESC_F_NEXT, 0);

        assert_eq!(
            walk(&mem, ring.desc_table(), 16, 0),
            Err(ChainError::TooLong)
        );
    }

    #[test]
    fn next_index_past_ring_is_rejected() {
        let mem = testing::guest_memory(0x2_0000);
        let ring = testing::SplitRing::layout(0x1000, 16);
        ring.write_desc(&mem, 0, 0x8000, 16, VIRTQ_DESC_F_NEXT, 99);

        assert_eq!(
            walk(&mem, ring.desc_table(), 16, 0),
            Err(ChainError::IndexOutOfRange {
                index: 99,
                queue_size: 16
            })
        );
    }

    #[test]
    fn descriptor_table_outside_guest_memory_is_an_error() {
        let mem = testing::guest_memory(0x2_0000);
        let err = walk(&mem, 0xdead_0000, 16, 0).expect_err("must not read host memory");
        assert!(matches!(err, ChainError::DescriptorUnreadable { .. }));
    }

    #[test]
    fn descriptor_table_address_overflow_is_an_error() {
        let mem = testing::guest_memory(0x2_0000);
        let err = walk(&mem, u64::MAX - 4, 16, 3).expect_err("must not wrap");
        assert!(matches!(err, ChainError::DescriptorTableOverflow { .. }));
    }

    #[test]
    fn indirect_descriptors_are_rejected() {
        let mem = testing::guest_memory(0x2_0000);
        let ring = testing::SplitRing::layout(0x1000, 16);
        ring.write_desc(&mem, 0, 0x8000, 64, VIRTQ_DESC_F_INDIRECT, 0);

        assert_eq!(
            walk(&mem, ring.desc_table(), 16, 0),
            Err(ChainError::IndirectNotSupported)
        );
    }

    #[test]
    fn split_rw_separates_prefix_and_suffix() {
        let segs = [
            Segment {
                addr: 1,
                len: 16,
                writable: false,
            },
            Segment {
                addr: 2,
                len: 512,
                writable: true,
            },
            Segment {
                addr: 3,
                len: 1,
                writable: true,
            },
        ];
        let (r, w) = split_rw(&segs).expect("well formed");
        assert_eq!(r.len(), 1);
        assert_eq!(w.len(), 2);
    }

    #[test]
    fn split_rw_rejects_interleaved_chains() {
        let segs = [
            Segment {
                addr: 1,
                len: 1,
                writable: true,
            },
            Segment {
                addr: 2,
                len: 16,
                writable: false,
            },
        ];
        assert_eq!(split_rw(&segs), Err(ChainError::WriteBeforeRead));
    }
}
