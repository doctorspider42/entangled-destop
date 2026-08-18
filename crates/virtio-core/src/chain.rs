//! Virtqueue descriptor-chain safety (backlog MVP-304/309).
//!
//! Ring parsing itself comes from the `virtio-queue` crate; this module owns
//! the VMHost policy on top of it: every chain walk must be bounded so a
//! malicious guest cannot loop the host, and every buffer must stay within
//! guest memory (enforced by `vm-memory` checked access at the call sites).

use thiserror::Error;

/// Upper bound on descriptors per chain. The spec allows up to queue size;
/// we cap harder because no MVP device legitimately needs longer chains.
pub const MAX_DESC_CHAIN_LEN: u16 = 128;

#[derive(Debug, Error, PartialEq, Eq)]
pub enum ChainError {
    #[error("descriptor chain exceeds {MAX_DESC_CHAIN_LEN} entries (looped or malicious)")]
    TooLong,

    #[error("descriptor index {index} out of range for queue size {queue_size}")]
    IndexOutOfRange { index: u16, queue_size: u16 },

    #[error("device-writable descriptor precedes driver-readable one")]
    WriteBeforeRead,
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
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bounded_walk_passes() {
        let mut g = ChainWalkGuard::new(256);
        for i in 0..MAX_DESC_CHAIN_LEN {
            g.step(i).unwrap();
        }
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
            g.step(i).unwrap();
        }
        assert_eq!(g.step(0), Err(ChainError::TooLong));
    }
}
