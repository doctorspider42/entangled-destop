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

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, PoisonError};

/// A pause gate shared by one VM's host-side device workers.
#[derive(Debug, Default)]
pub struct Quiesce {
    /// Read on the hot path without taking the mutex.
    paused: AtomicBool,
    /// Only ever a rendezvous point for the condvar; the truth is `paused`.
    lock: Mutex<()>,
    changed: Condvar,
}

impl Quiesce {
    /// A gate that is open: the VM is running.
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    pub fn is_paused(&self) -> bool {
        self.paused.load(Ordering::Acquire)
    }

    /// Closes the gate. Workers already past it finish what they were doing;
    /// the lifecycle only calls this once every vCPU has parked, and only
    /// treats the VM as paused once the workers are known to be through.
    pub fn pause(&self) {
        self.paused.store(true, Ordering::Release);
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

    /// Blocks while the VM is paused **and** `keep_going` still says so.
    ///
    /// Returns what `keep_going` last said: `false` means the caller should
    /// stop, not that the VM resumed. Called by a host worker immediately
    /// before it touches guest memory and never while holding a device lock —
    /// a reset runs while the VM is quiesced and needs those same locks.
    pub fn wait_while_paused(&self, keep_going: impl Fn() -> bool) -> bool {
        if !self.is_paused() {
            return keep_going();
        }
        let mut guard = self.lock.lock().unwrap_or_else(PoisonError::into_inner);
        loop {
            if !keep_going() {
                return false;
            }
            if !self.is_paused() {
                return true;
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
        assert!(gate.wait_while_paused(|| true));
        assert!(started.elapsed() < Duration::from_millis(50));
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
                while gate.wait_while_paused(|| !stop.load(Ordering::Acquire)) {
                    work.fetch_add(1, Ordering::AcqRel);
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
                while gate.wait_while_paused(|| !stop.load(Ordering::Acquire)) {
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
