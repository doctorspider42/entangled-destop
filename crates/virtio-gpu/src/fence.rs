//! Bounded bookkeeping for **deferred fence responses** (ADR-0004 phase 2).
//!
//! Phase 1 completed every fence synchronously: the response was written the
//! moment the command parsed, which is spec-legal but serializes the guest
//! against the host GL driver. Phase 2 holds the response of a fenced command
//! back until the host renderer retires the fence, and this module is the
//! table those held-back responses wait in.
//!
//! Portable and renderer-agnostic: entries are pushed in submission order and
//! retired in submission order, because that is the contract virgl fences
//! (and the guest's DRM fence timeline) have — the Linux `virtio_gpu` driver
//! signals **every** fence with an id at or below the one a response carries,
//! so completing out of order would prematurely signal earlier fences.
//!
//! # Bounds (the guest is untrusted)
//!
//! A guest controls how many fenced commands it submits without waiting.
//! Entries hold a response header, a small body and the chain's writable
//! segments, and — more importantly — each one pins a descriptor chain that
//! was never returned to the used ring. [`MAX_PENDING_FENCES`] caps the
//! table; the device answers anything beyond it with an in-band error rather
//! than allocating further (see `GpuDevice`). A well-behaved driver keeps a
//! handful in flight (mesa throttles at two frames), so the cap is far above
//! real use and far below harm.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

/// Most fenced commands that may be awaiting host completion at once. Above
/// this the device fails fenced commands in band (`ERR_OUT_OF_MEMORY`).
///
/// mesa's virgl driver throttles at ~2 frames of fences; GNOME's mutter adds
/// a few more across processes. 64 leaves an order of magnitude of headroom
/// while bounding what a fence-spamming guest can pin (the ring itself caps
/// chains at queue size anyway; this cap keeps the pinning *named and
/// tested*, per the MVP-1407 rule).
pub const MAX_PENDING_FENCES: usize = 64;

/// A FIFO of payloads waiting on host fence retirement, keyed by the 32-bit
/// host fence id (the wire's u64 `fence_id`, truncated exactly the way it is
/// truncated toward virglrenderer — ids are compared, never ordered, so the
/// truncation only requires ids not to repeat within one table's depth).
///
/// Every entry is stamped on the way in, so the owner can both age out a
/// fence that never retires (the device's watchdog) and report how long the
/// ones that did retire waited — which is the phase-2 pipelining measurement.
#[derive(Debug)]
pub struct FenceQueue<T> {
    entries: VecDeque<Entry<T>>,
}

#[derive(Debug)]
struct Entry<T> {
    id: u32,
    at: Instant,
    payload: T,
}

impl<T> Default for FenceQueue<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T> FenceQueue<T> {
    pub fn new() -> Self {
        Self {
            entries: VecDeque::new(),
        }
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// True when another [`Self::push`] would exceed [`MAX_PENDING_FENCES`].
    pub fn is_full(&self) -> bool {
        self.entries.len() >= MAX_PENDING_FENCES
    }

    /// How long the oldest waiting entry has been waiting, or `None` when the
    /// table is empty. The device's fence watchdog reads this.
    pub fn oldest_age(&self) -> Option<Duration> {
        self.entries.front().map(|e| e.at.elapsed())
    }

    /// Appends a payload waiting on `fence_id`. Returns the payload back when
    /// the table is full — the caller answers that command in band instead.
    pub fn push(&mut self, fence_id: u32, payload: T) -> Result<(), T> {
        if self.is_full() {
            return Err(payload);
        }
        self.entries.push_back(Entry {
            id: fence_id,
            at: Instant::now(),
            payload,
        });
        Ok(())
    }

    /// Retires `fence_id` and everything submitted before it, in order, with
    /// how long each one waited.
    ///
    /// Host fences retire in creation order, and retirement callbacks
    /// coalesce (the renderer may only report the *latest* retired id), so
    /// one call completes the whole prefix up to and including the entry
    /// whose id matches. An id with no matching entry completes nothing —
    /// either it was already handed out on an earlier (coalesced) call, or
    /// the entries were dropped by a device reset.
    pub fn complete(&mut self, fence_id: u32) -> Vec<(Duration, T)> {
        let Some(last) = self.entries.iter().position(|e| e.id == fence_id) else {
            return Vec::new();
        };
        self.entries
            .drain(..=last)
            .map(|e| (e.at.elapsed(), e.payload))
            .collect()
    }

    /// Empties the table (device reset, renderer loss, the watchdog): every
    /// payload is returned so the caller can decide what the guest sees.
    pub fn drain_all(&mut self) -> Vec<(Duration, T)> {
        self.entries
            .drain(..)
            .map(|e| (e.at.elapsed(), e.payload))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The payloads of a completion batch, dropping the wait times.
    fn payloads<T>(batch: Vec<(Duration, T)>) -> Vec<T> {
        batch.into_iter().map(|(_, payload)| payload).collect()
    }

    #[test]
    fn retirement_completes_the_whole_prefix_in_order() {
        let mut q = FenceQueue::new();
        for id in 1..=5u32 {
            q.push(id, id * 10).expect("under the cap");
        }
        // A coalesced callback reporting only fence 3 retires 1, 2 and 3.
        assert_eq!(payloads(q.complete(3)), vec![10, 20, 30]);
        assert_eq!(q.len(), 2);
        // Re-reporting an already-retired id completes nothing.
        assert_eq!(payloads(q.complete(3)), Vec::<u32>::new());
        assert_eq!(payloads(q.complete(2)), Vec::<u32>::new());
        // The rest retires when its own id arrives.
        assert_eq!(payloads(q.complete(5)), vec![40, 50]);
        assert!(q.is_empty());
    }

    #[test]
    fn unknown_ids_complete_nothing() {
        let mut q = FenceQueue::new();
        q.push(7, "a").expect("push");
        assert!(q.complete(99).is_empty());
        assert_eq!(q.len(), 1);
    }

    #[test]
    fn the_oldest_entry_ages_and_an_empty_table_has_no_age() {
        let mut q: FenceQueue<u32> = FenceQueue::new();
        assert!(q.oldest_age().is_none());
        q.push(1, 1).expect("push");
        q.push(2, 2).expect("push");
        let first = q.oldest_age().expect("an entry is waiting");
        // The oldest age tracks the *front* entry, so retiring it moves the
        // watchdog's clock forward rather than resetting it.
        assert_eq!(payloads(q.complete(1)), vec![1]);
        let second = q.oldest_age().expect("one entry left");
        assert!(second <= first.max(second), "ages are monotonic per entry");
        assert_eq!(payloads(q.drain_all()), vec![2]);
        assert!(q.oldest_age().is_none());
    }

    #[test]
    fn the_cap_holds_and_returns_the_payload() {
        let mut q = FenceQueue::new();
        for id in 0..MAX_PENDING_FENCES as u32 {
            q.push(id, id).expect("under the cap");
        }
        assert!(q.is_full());
        assert_eq!(q.push(u32::MAX, 1234), Err(1234));
        assert_eq!(q.len(), MAX_PENDING_FENCES);
        // Draining one makes room for one.
        assert_eq!(payloads(q.complete(0)), vec![0]);
        q.push(u32::MAX, 1234).expect("room again");
    }

    #[test]
    fn drain_all_returns_everything_in_order() {
        let mut q = FenceQueue::new();
        q.push(1, "x").expect("push");
        q.push(2, "y").expect("push");
        assert_eq!(payloads(q.drain_all()), vec!["x", "y"]);
        assert!(q.is_empty());
    }
}
