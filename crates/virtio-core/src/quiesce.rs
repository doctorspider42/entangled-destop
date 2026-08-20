//! The gate host-side device workers wait on while a VM is paused
//! ([ADR-0005](../../../docs/adr/0005-vm-lifecycle.md)).
//!
//! Parking the vCPUs stops the *guest*, but it does not stop the host. Two
//! kinds of thread keep running and keep touching guest memory:
//!
//! * the queue-notify workers (`machine_x86::notify`), which serve an ioeventfd
//!   a vCPU may already have kicked before it parked, and
//! * a device's own worker — today virtio-net's receive thread, which writes
//!   arriving frames straight into the RX ring whether or not a guest is there
//!   to see them.
//!
//! "Paused" has to mean *nothing writes guest memory*, or a pause is not a
//! point a snapshot could ever be taken at, and a resumed guest finds
//! descriptors it never posted. So both kinds of thread take this gate before
//! they touch a ring, and a pause is not acknowledged until they are through it.
//!
//! # Why every waiter brings its own liveness predicate
//!
//! A worker parked here still has to be able to *stop*. virtio-net's device
//! reset joins its receive thread, and a reset happens while the VM is
//! quiesced — so a gate that only ever released on resume would deadlock the
//! reset that is trying to shut the thread down. [`Quiesce::wait_while_paused`]
//! therefore takes the caller's own "should I keep going?" test and returns as
//! soon as *either* condition changes; the shutting-down side pairs its flag
//! with [`Quiesce::wake`].

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, PoisonError};
use std::time::{Duration, Instant};

/// A pause gate shared by one VM's host-side device workers.
#[derive(Debug, Default)]
pub struct Quiesce {
    /// Read on the hot path without taking the mutex.
    paused: AtomicBool,
    /// How many workers are past the gate and may be touching guest memory
    /// right now. Closing the gate stops *new* work; this is what makes
    /// "paused" mean the work already in hand has finished too.
    in_flight: AtomicUsize,
    /// Only ever a rendezvous point for the condvar; the truth is the atomics.
    lock: Mutex<()>,
    changed: Condvar,
}

/// Permission to touch guest memory, held for one unit of a worker's work.
///
/// The whole reason [`Quiesce::wait_while_paused`] hands one back rather than a
/// plain `bool`: closing the gate only stops work that has not started, and a
/// pause that returned while a receive worker was half-way through writing a
/// frame into the RX ring would not be a point anything could be snapshotted
/// at. Dropping the pass is what lets [`Quiesce::wait_until_idle`] finish.
#[derive(Debug)]
pub struct Pass<'a>(&'a Quiesce);

impl Drop for Pass<'_> {
    fn drop(&mut self) {
        self.0.in_flight.fetch_sub(1, Ordering::AcqRel);
        // Only the pause path is ever waiting on this, so waking is cheap and
        // rare; doing it unconditionally keeps the worker's fast path free of a
        // second atomic read.
        self.0.wake();
    }
}

impl Quiesce {
    /// A gate that is open: the VM is running.
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    pub fn is_paused(&self) -> bool {
        self.paused.load(Ordering::Acquire)
    }

    /// Closes the gate: no worker starts new work from here on.
    ///
    /// Pair it with [`Self::wait_until_idle`] — this alone does not wait for
    /// the work already in hand.
    pub fn pause(&self) {
        self.paused.store(true, Ordering::Release);
    }

    /// Waits until no worker is past the gate, i.e. until "paused" is true of
    /// guest memory and not only of the guest.
    ///
    /// Bounded, and reports whether it got there. A worker that is stuck (a
    /// host disk that has stopped answering) must not be able to wedge a pause;
    /// the honest outcome is to carry on and say so, because the alternative —
    /// a VM that can never be frozen because one device is unwell — is worse.
    pub fn wait_until_idle(&self, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        let mut guard = self.lock.lock().unwrap_or_else(PoisonError::into_inner);
        loop {
            if self.in_flight.load(Ordering::Acquire) == 0 {
                return true;
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                tracing::warn!(
                    workers = self.in_flight.load(Ordering::Acquire),
                    ?timeout,
                    "a device worker did not reach the pause gate; pausing anyway"
                );
                return false;
            }
            guard = self
                .changed
                .wait_timeout(guard, left.min(WAKE_POLL))
                .map(|(guard, _)| guard)
                .unwrap_or_else(|poisoned| poisoned.into_inner().0);
        }
    }

    /// Opens the gate and releases everyone waiting.
    pub fn resume(&self) {
        self.paused.store(false, Ordering::Release);
        self.wake();
    }

    /// Wakes every waiter without changing the gate, so each can re-check its
    /// own liveness predicate. What a device calls when it is tearing a worker
    /// down while the VM is paused.
    pub fn wake(&self) {
        let _guard = self.lock.lock().unwrap_or_else(PoisonError::into_inner);
        self.changed.notify_all();
    }

    /// Blocks while the VM is paused **and** `keep_going` still says so, then
    /// hands out a [`Pass`] for one unit of work.
    ///
    /// `None` means the caller should stop — not that the VM resumed. Called by
    /// a host worker immediately before it touches guest memory and never while
    /// holding a device lock: a reset runs while the VM is quiesced and needs
    /// those same locks. Hold the pass for as long as the work lasts and no
    /// longer; a pause is not acknowledged until every pass is dropped.
    #[must_use]
    pub fn wait_while_paused(&self, keep_going: impl Fn() -> bool) -> Option<Pass<'_>> {
        if !keep_going() {
            return None;
        }
        // Claim first, check second. The other order has a window: a worker
        // that read "not paused" and had not yet counted itself in would be
        // invisible to a `wait_until_idle` running in between, and would then
        // start writing guest memory on a VM the host had already called paused.
        // This way round the worst case is a pass claimed and immediately given
        // back, which only ever makes a pause wait a moment longer.
        let pass = self.pass();
        if !self.is_paused() {
            return Some(pass);
        }
        drop(pass);
        let mut guard = self.lock.lock().unwrap_or_else(PoisonError::into_inner);
        loop {
            if !keep_going() {
                return None;
            }
            if !self.is_paused() {
                // Taken under the lock, so it cannot race a `pause()` that has
                // already decided this worker is idle.
                return Some(self.pass());
            }
            // A timeout rather than a plain wait: `paused` is written outside
            // the mutex (it is on the hot path), so a notification could in
            // principle be missed. Re-checking a few times a second costs
            // nothing while the VM is stopped anyway.
            guard = self
                .changed
                .wait_timeout(guard, WAKE_POLL)
                .map(|(guard, _)| guard)
                .unwrap_or_else(|poisoned| poisoned.into_inner().0);
        }
    }

    /// A [`Pass`] if the VM is running, `None` if it is paused. **Never
    /// blocks.**
    ///
    /// For a producer that would rather drop its input than deliver a burst of
    /// it on resume: the host's input pump, whose events are keystrokes and
    /// pointer motion from a window nobody is looking at while the VM is
    /// frozen. Parking that thread would be worse than dropping, and writing
    /// the guest's event ring is exactly what a pause forbids.
    #[must_use]
    pub fn try_enter(&self) -> Option<Pass<'_>> {
        // Claim first, check second, for the reason `wait_while_paused` does.
        let pass = self.pass();
        if self.is_paused() {
            return None;
        }
        Some(pass)
    }

    fn pass(&self) -> Pass<'_> {
        self.in_flight.fetch_add(1, Ordering::AcqRel);
        Pass(self)
    }
}

/// Upper bound on how long a waiter sleeps before re-reading the gate.
const WAKE_POLL: std::time::Duration = std::time::Duration::from_millis(20);

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicU64;
    use std::time::{Duration, Instant};

    #[test]
    fn an_open_gate_never_blocks() {
        let gate = Quiesce::new();
        assert!(!gate.is_paused());
        let started = Instant::now();
        assert!(gate.wait_while_paused(|| true).is_some());
        assert!(started.elapsed() < Duration::from_millis(50));
        assert!(gate.wait_until_idle(Duration::from_millis(50)));
    }

    /// The reason a pass exists: closing the gate only stops work that has not
    /// started, so a pause that returned while a worker was mid-write would not
    /// be a point anything could be snapshotted at.
    #[test]
    fn pausing_waits_for_the_work_already_in_hand() {
        let gate = Quiesce::new();
        let holding = Arc::new(AtomicBool::new(false));
        let worker = {
            let (gate, holding) = (Arc::clone(&gate), Arc::clone(&holding));
            std::thread::spawn(move || {
                let pass = gate.wait_while_paused(|| true).expect("a pass");
                holding.store(true, Ordering::Release);
                std::thread::sleep(Duration::from_millis(120));
                holding.store(false, Ordering::Release);
                drop(pass);
            })
        };
        while !holding.load(Ordering::Acquire) {
            std::thread::sleep(Duration::from_millis(1));
        }
        gate.pause();
        assert!(
            gate.wait_until_idle(Duration::from_secs(2)),
            "the gate never went idle"
        );
        assert!(
            !holding.load(Ordering::Acquire),
            "pause returned while a worker was still inside its pass"
        );
        worker.join().expect("worker thread");
    }

    /// …and a worker that never comes back cannot wedge a pause.
    #[test]
    fn a_stuck_worker_does_not_wedge_a_pause() {
        let gate = Quiesce::new();
        let stop = Arc::new(AtomicBool::new(false));
        let worker = {
            let (gate, stop) = (Arc::clone(&gate), Arc::clone(&stop));
            std::thread::spawn(move || {
                let _pass = gate.wait_while_paused(|| true).expect("a pass");
                while !stop.load(Ordering::Acquire) {
                    std::thread::sleep(Duration::from_millis(2));
                }
            })
        };
        std::thread::sleep(Duration::from_millis(20));
        gate.pause();
        let started = Instant::now();
        assert!(!gate.wait_until_idle(Duration::from_millis(80)));
        assert!(started.elapsed() < Duration::from_secs(2));
        stop.store(true, Ordering::Release);
        worker.join().expect("worker thread");
    }

    /// The whole point: a worker stops making progress while paused, and picks
    /// up again on resume.
    #[test]
    fn a_worker_parks_while_paused_and_continues_after() {
        let gate = Quiesce::new();
        let work = Arc::new(AtomicU64::new(0));
        let stop = Arc::new(AtomicBool::new(false));
        let worker = {
            let (gate, work, stop) = (Arc::clone(&gate), Arc::clone(&work), Arc::clone(&stop));
            std::thread::spawn(move || {
                while let Some(pass) = gate.wait_while_paused(|| !stop.load(Ordering::Acquire)) {
                    work.fetch_add(1, Ordering::AcqRel);
                    drop(pass);
                    std::thread::sleep(Duration::from_micros(200));
                }
            })
        };

        let deadline = Instant::now() + Duration::from_secs(2);
        while work.load(Ordering::Acquire) == 0 && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(1));
        }
        gate.pause();
        std::thread::sleep(Duration::from_millis(30));
        let frozen = work.load(Ordering::Acquire);
        std::thread::sleep(Duration::from_millis(50));
        assert_eq!(
            work.load(Ordering::Acquire),
            frozen,
            "the worker kept going while the gate was closed"
        );

        gate.resume();
        let deadline = Instant::now() + Duration::from_secs(2);
        while work.load(Ordering::Acquire) == frozen && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(1));
        }
        assert!(
            work.load(Ordering::Acquire) > frozen,
            "the worker did not resume"
        );

        stop.store(true, Ordering::Release);
        gate.wake();
        worker.join().expect("worker thread");
    }

    /// A worker must still be able to shut down while the gate is closed — the
    /// deadlock this design exists to avoid, because device reset happens on a
    /// quiesced VM and joins exactly such a thread.
    #[test]
    fn a_parked_worker_can_still_be_stopped() {
        let gate = Quiesce::new();
        let stop = Arc::new(AtomicBool::new(false));
        gate.pause();
        let worker = {
            let (gate, stop) = (Arc::clone(&gate), Arc::clone(&stop));
            std::thread::spawn(move || {
                while let Some(pass) = gate.wait_while_paused(|| !stop.load(Ordering::Acquire)) {
                    drop(pass);
                    std::thread::sleep(Duration::from_micros(200));
                }
            })
        };
        std::thread::sleep(Duration::from_millis(20));
        stop.store(true, Ordering::Release);
        gate.wake();
        let started = Instant::now();
        worker.join().expect("worker thread");
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "joining a parked worker took {:?}",
            started.elapsed()
        );
        assert!(gate.is_paused(), "the gate was not opened to let it out");
    }
}
