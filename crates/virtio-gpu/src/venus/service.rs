//! The Venus ring **service**: the thread that keeps consuming a ring for as
//! long as the guest's contract says it must, and the thread that keeps
//! telling the guest the host is still there (EPIC 20, ADR-0004 stage 5a.1).
//!
//! [`super::pump`] is the head/tail protocol as pure synchronous logic. This
//! module decides *when* to run it, which turns out to be the half a guest can
//! actually see go wrong.
//!
//! # Why a thread, and not the doorbell
//!
//! Mesa rings the doorbell (`vkNotifyRingMESA`) only when the ring's status
//! says `IDLE` **and** at least a millisecond has passed since the last
//! doorbell it sent (`vn_ring.c:478-489`). A reply-bearing command is two ring
//! submissions — `vkSetReplyCommandStreamMESA`, then the command — written
//! back to back with nothing between them (`vn_ring.c:698-727`). So the first
//! rings the doorbell and the second, a few microseconds later, does not. The
//! guest is entitled to that: it passed `idleTimeout` at ring creation, and the
//! contract is that the host keeps polling for that long after the last work it
//! saw before it publishes `IDLE` and waits (`vkr_ring.c:260-287`).
//!
//! A renderer that drains synchronously on the doorbell and republishes `IDLE`
//! at once breaks that contract on the very first command a guest sends, and
//! that is exactly how a real Mesa guest got stuck on the previous design: the
//! command sat in the ring, unread, until the guest's watchdog aborted.
//!
//! So each adopted ring gets a [`RingWorker`], faithful to virglrenderer's
//! `vkr_ring_thread` (`vkr_ring.c:241-335`):
//!
//! 1. pump; while that makes progress, keep going;
//! 2. with no progress, keep polling `tail` — yielding, then sleeping with a
//!    growing backoff — until `idleTimeout` has passed since the last
//!    progress (on a host whose sleeps are coarse, the first
//!    [`HOST_SPIN`] of that is yields alone: see below);
//! 3. then publish `IDLE`, **re-read `tail`**, and if work arrived in between,
//!    take `IDLE` back down and go to 1;
//! 4. otherwise park until a doorbell, a stop or a reset; on waking, take
//!    `IDLE` down and go to 1.
//!
//! The decisions are [`RingService`], a pure state machine over an injected
//! clock, so every one of them is tested deterministically; [`RingWorker`] is
//! the thin thread around it.
//!
//! # Why Windows polls by yielding (ADR-0004, 2026-09-25)
//!
//! vkr's backoff assumes a sleep lasts about what it asked for: on Linux a
//! 10 µs `usleep` is over in well under 0.1 ms. On Windows the shortest
//! sleep there is lasts 0.35–0.6 ms whatever is asked (a high-resolution
//! waitable timer, which `std::thread::sleep` uses; measured on the RTX 2070
//! machine, Windows 10), and a condition-variable timeout rounds up to the
//! 15.6 ms system tick. Mesa's `idleTimeout` is 1 ms (`vn_ring.c:18`), so
//! a Windows worker spent most of that window asleep, and each of the
//! several ring submissions a guest makes around one `vkQueueSubmit`
//! (destroys, allocations, the recording, the submit) waited up to half a
//! millisecond to be seen: an empty submit's round trip was 0.7–2.0 ms in
//! the guest against 0.4 ms with a yielding worker, and glmark2 ran at a
//! third of the frame rate. So on Windows the worker polls by yielding for
//! [`HOST_SPIN`] after its last progress — the whole of Mesa's window — and
//! only then starts vkr's backoff. The price is a core kept busy for at
//! most that long after each burst of guest work; an idle guest costs
//! nothing, because a ring with nothing to do parks.
//!
//! # Why a second thread for `ALIVE`
//!
//! Mesa's ring waits have a watchdog. The waiter clears
//! `VK_RING_STATUS_ALIVE_BIT_MESA` when it takes the role (`vn_common.c:229-243`)
//! and aborts ~3.5 s later if the host has not set it again
//! (`vn_common.c:274-283`). virglrenderer answers from a per-context monitor
//! thread that sets `ALIVE` on every monitored ring at least every
//! `maxReportingPeriodMicroseconds` (`vkr_context.c:507-545`) — Mesa asks for
//! 3 s (`vn_ring.c:354-361`). It must be a separate thread: a ring worker busy
//! executing one long command is exactly the case in which the guest is
//! waiting longest, and it cannot report on itself while it is busy.
//! [`RingMonitor`] is that thread.
//!
//! # The status word is shared, so it is only ever read-modify-written
//!
//! The worker sets and clears `IDLE` and `FATAL`, the monitor sets `ALIVE`, and
//! the guest clears `ALIVE` with its own atomic AND. Every write any of them
//! makes is a `fetch_or`/`fetch_and` of named bits
//! ([`RingPages::set_status_bits`], [`RingPages::clear_status_bits`]); nothing
//! stores the word whole.
//!
//! # Both threads owe ADR-0005 a `Quiesce` pass
//!
//! Both write guest-visible pages, so both take
//! [`Quiesce::wait_while_paused`] before every pass and hold the [`Pass`] for
//! exactly the work — never while holding any device lock, and never while
//! parked. Stopping is `stop flag → wake the condvar → Quiesce::wake → join`,
//! which works while the VM is paused: a worker waiting at the gate re-checks
//! its liveness predicate and leaves. A ring whose sink is waiting on the
//! context stream ([`Step::Blocked`]) waits between passes, on its doorbell,
//! so it holds no pass while the thing it waits for is behind the gated
//! device worker. Neither thread ever takes a lock that the
//! device side holds while joining it, which is what makes joining from inside
//! a device callback (`vkDestroyRingMESA`, `ctx_destroy`, `reset`) safe.
//!
//! [`Pass`]: virtio_core::quiesce::Pass

use std::io;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use thiserror::Error;
use virtio_core::Quiesce;

use super::pump::{Idle, Pass, PumpError, RingBacking, RingPump, RingSink, STATUS_ALIVE};
use super::ring::HostWord;
use super::shmem::RingPages;

/// The shortest `ALIVE` reporting period this host honours.
///
/// `VkRingMonitorInfoMESA::maxReportingPeriodMicroseconds` is a guest `u32`,
/// and a period of one microsecond would turn the monitor into a thread that
/// takes a `Quiesce` pass and writes guest memory a million times a second. A
/// millisecond is three thousand times faster than Mesa ever asks for (3 s,
/// `vn_common.h:64`) — so it cannot starve a real guest's watchdog — and still
/// a rate at which the monitor costs nothing measurable. A shorter request is
/// raised to it rather than refused: the guest asked for *at least* that often,
/// and more often than it needs is not a violation of anything.
///
/// Zero is different, and is refused by [`monitor_period`]: the reference
/// treats it as a fatal protocol error (`vkr_transport.c:228-232`).
pub const MIN_MONITOR_PERIOD: Duration = Duration::from_millis(1);

/// How many polls without progress are answered with a bare `yield` before the
/// backoff starts sleeping: `vkr_ring_relax`'s `busy_wait_order` (2⁴ = 16).
const BUSY_WAIT_ORDER: u32 = 4;

/// The first backoff sleep, doubled every time the poll count doubles:
/// `vkr_ring_relax`'s `base_sleep_us`.
const BASE_SLEEP: Duration = Duration::from_micros(10);

/// How long after its last progress a ring is polled by yielding alone,
/// before [`relax_backoff`]'s sleeps begin (module docs, "Why Windows polls
/// by yielding").
///
/// Zero on Linux, which keeps vkr's backoff exactly. On Windows 2 ms: all of
/// Mesa's 1 ms `idleTimeout`, with room to spare, and nothing near the
/// [`super::pump::MAX_IDLE_TIMEOUT`] a guest may ask for — past this the
/// backoff sleeps as vkr's does, however long the guest asked the host to
/// keep polling.
pub const HOST_SPIN: Duration = if cfg!(windows) {
    Duration::from_millis(2)
} else {
    Duration::ZERO
};

/// Below this, a poll sleeps with `thread::sleep` (high resolution on both
/// hosts); at or above it, on the worker's condition variable, so that a stop
/// does not have to wait the sleep out. Condition-variable timeouts are
/// millisecond-granular on Windows, which is why short sleeps do not use them.
const INTERRUPTIBLE_SLEEP: Duration = Duration::from_millis(1);

/// The guest's `maxReportingPeriodMicroseconds` as the period the monitor will
/// actually use: `None` for the zero the reference refuses, otherwise the
/// request raised to [`MIN_MONITOR_PERIOD`].
#[must_use]
pub fn monitor_period(us: u32) -> Option<Duration> {
    (us != 0).then(|| Duration::from_micros(u64::from(us)).max(MIN_MONITOR_PERIOD))
}

/// The backoff after `iter` consecutive polls that found nothing:
/// `vkr_ring_relax` (`vkr_ring.c:187-205`) exactly — a `yield` for the first
/// sixteen, then 10 µs doubling every time `iter` doubles.
///
/// The caller caps it at the time left before the ring should go idle, so the
/// growth only matters for a guest that asked for a long `idleTimeout`, and
/// [`super::pump::MAX_IDLE_TIMEOUT`] bounds that.
#[must_use]
pub fn relax_backoff(iter: u32) -> Duration {
    if iter < (1 << BUSY_WAIT_ORDER) {
        return Duration::ZERO;
    }
    // `util_last_bit(iter) - busy_wait_order - 1`; `iter >= 16` makes the
    // subtraction safe and `iter < 2^32` keeps the shift at most 27.
    let last_bit = u32::BITS - iter.leading_zeros();
    let shift = last_bit - BUSY_WAIT_ORDER - 1;
    BASE_SLEEP.saturating_mul(1u32 << shift)
}

/// The longest a blocked ring waits between two looks at what it is waiting
/// for ([`Step::Blocked`]).
///
/// Only a safety net: the renderer rings the worker's doorbell when it
/// records the value a blocked sink waits for, and a stop rings the same
/// condition variable, so neither waits this out. What it does bound is the
/// cost of a guest that blocks its ring and never sends the seqno — one pass
/// over the ring's bytes this often — and, were a doorbell ever missed, the
/// latency that would cost.
pub const MAX_BLOCKED_WAIT: Duration = Duration::from_millis(100);

/// The wait after `iter` consecutive blocked passes: from one millisecond,
/// doubling, up to [`MAX_BLOCKED_WAIT`]. Always long enough to be a
/// condition-variable wait ([`INTERRUPTIBLE_SLEEP`]), because the doorbell is
/// what normally ends it.
#[must_use]
pub fn blocked_backoff(iter: u32) -> Duration {
    let shift = iter.saturating_sub(1).min(16);
    INTERRUPTIBLE_SLEEP
        .saturating_mul(1u32 << shift)
        .min(MAX_BLOCKED_WAIT)
}

// ------------------------------------------------------------ the decisions

/// Why a ring stopped for good. Every one of these has published
/// [`STATUS_FATAL`](super::pump::STATUS_FATAL) by the time it is reported, so
/// the guest's driver aborts rather than waiting on a `head` that will not move.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum RingStop {
    /// The protocol refused a pass: an impossible `tail`, or a sink that could
    /// not answer the command at `head`.
    #[error(transparent)]
    Pump(#[from] PumpError),

    /// The sink took nothing from a full ring, so nothing can ever arrive to
    /// unstick it ([`Pass::Deadlocked`]).
    #[error("the ring is full and its sink consumed none of the {offered:#x} bytes in it")]
    Deadlocked {
        /// How many bytes were on offer — always the buffer's full length.
        offered: u32,
    },
}

/// What the worker should do after one [`RingService::step`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    /// Progress was made, or the ring is due to go idle: step again at once.
    Again,
    /// Nothing new yet, and the ring is not due to go idle: poll again after
    /// this long. [`Duration::ZERO`] means yield the CPU and come straight
    /// back. Never longer than the time left before the ring is due to go idle.
    Poll(Duration),
    /// `IDLE` is published and a re-read of `tail` confirmed there is nothing
    /// to do. Block until a doorbell, then call [`RingService::wake`].
    Park,
    /// The sink is waiting on the context stream ([`Pass::Blocked`]): wait
    /// for a doorbell — the renderer rings one when it records what the sink
    /// waits for — or at most this long, **holding no pass**, then step
    /// again. `IDLE` stays down meanwhile, as virglrenderer's ring thread
    /// keeps it down while it waits (`vkr_ring_wait_virtqueue_seqno`).
    Blocked(Duration),
    /// The ring is dead and `FATAL` is published. The worker ends.
    Stopped(RingStop),
}

/// The ring worker's decisions, as a pure state machine over an injected clock.
///
/// Owns the [`RingPump`] and the ring's [`RingSink`], and nothing else: no
/// thread, no lock, no clock of its own. `now` is any monotonic [`Duration`]
/// since an origin of the caller's choosing — the worker uses the time since
/// it started, a test uses whatever numbers make its point.
#[derive(Debug)]
pub struct RingService<S> {
    pump: RingPump,
    sink: S,
    /// When the pump last made progress, or the ring last woke.
    last_progress: Duration,
    /// Polls in a row that found nothing, for [`relax_backoff`].
    relax_iter: u32,
    /// How long after the last progress a poll only yields, without
    /// advancing `relax_iter` ([`HOST_SPIN`] for a live worker).
    spin: Duration,
    /// `IDLE` is up and the ring is waiting for a doorbell.
    parked: bool,
    /// Set once the ring is dead; every later step answers it.
    stopped: Option<RingStop>,
}

impl<S: RingSink> RingService<S> {
    /// Start serving a freshly adopted ring at `now`.
    ///
    /// The ring starts **polling**, not idle — virglrenderer's thread starts
    /// with `last_submit = now` — so work the guest queued before we looked,
    /// or right after creating the ring, is picked up without a doorbell.
    pub fn new(pump: RingPump, sink: S, now: Duration) -> Self {
        Self {
            pump,
            sink,
            last_progress: now,
            relax_iter: 0,
            spin: Duration::ZERO,
            parked: false,
            stopped: None,
        }
    }

    /// Poll by yielding alone for `spin` after every progress (and every
    /// wake), and only then start [`relax_backoff`]. [`new`](Self::new)
    /// starts with none, which is vkr's backoff exactly; the renderer's
    /// workers use [`HOST_SPIN`].
    #[must_use]
    pub fn with_spin(mut self, spin: Duration) -> Self {
        self.spin = spin;
        self
    }

    /// The pump, for its cursor, status and layout.
    #[must_use]
    pub fn pump(&self) -> &RingPump {
        &self.pump
    }

    /// The sink.
    #[must_use]
    pub fn sink(&self) -> &S {
        &self.sink
    }

    /// The sink, mutably.
    pub fn sink_mut(&mut self) -> &mut S {
        &mut self.sink
    }

    /// Whether the ring is parked waiting for a doorbell.
    #[must_use]
    pub fn is_parked(&self) -> bool {
        self.parked
    }

    /// Why the ring stopped, if it has.
    #[must_use]
    pub fn stopped(&self) -> Option<RingStop> {
        self.stopped
    }

    /// Whether the next [`step`](Self::step) at `now` will publish `IDLE`:
    /// polling, and `idleTimeout` has passed since the last progress.
    ///
    /// The worker uses this to forget a doorbell that arrived while it was
    /// polling — virglrenderer's `pending_notify = false` just before it sets
    /// `IDLE`. That is safe because `IDLE` is followed by a re-read of `tail`,
    /// and every doorbell announces a `tail` the guest stored before ringing.
    #[must_use]
    pub fn idle_due(&self, now: Duration) -> bool {
        !self.parked
            && self.stopped.is_none()
            && now >= self.last_progress.saturating_add(self.pump.idle_timeout())
    }

    /// Make one decision at `now`.
    ///
    /// Parked or stopped, this touches no memory at all and repeats itself;
    /// otherwise it may publish `IDLE` (and re-read `tail`), and then makes at
    /// most one pass of the pump.
    pub fn step(&mut self, now: Duration, backing: &impl RingBacking) -> Step {
        if let Some(stop) = self.stopped {
            return Step::Stopped(stop);
        }
        if self.parked {
            return Step::Park;
        }

        if self.idle_due(now) {
            match self.pump.enter_idle(backing) {
                Idle::Park => {
                    self.parked = true;
                    return Step::Park;
                }
                // `IDLE` went up and straight back down: the guest stored a
                // `tail` between our last pass and the publication. This is
                // the lost-wakeup case the re-read exists for, and the only
                // correct answer is to pump it now.
                Idle::WorkArrived => {}
            }
        }

        match self.pump.pump(backing, &mut self.sink) {
            Ok(Pass::Progress { .. }) => {
                self.last_progress = now;
                self.relax_iter = 0;
                Step::Again
            }
            Ok(Pass::Idle | Pass::Stalled { .. }) => self.relax(now),
            Ok(Pass::Blocked { consumed, .. }) => {
                // Not idle: the ring has work, it just cannot run it yet. The
                // idle clock restarts so `IDLE` is not published under a
                // command the ring is about to run.
                self.last_progress = now;
                if consumed != 0 {
                    self.relax_iter = 0;
                }
                self.relax_iter = self.relax_iter.saturating_add(1);
                Step::Blocked(blocked_backoff(self.relax_iter))
            }
            Ok(Pass::Deadlocked { offered }) => {
                // The pump reports this and leaves the policy to its driver;
                // the policy is that a guest waiting on a `head` that cannot
                // move should be told, not hung.
                self.pump.mark_fatal(backing);
                self.stop(RingStop::Deadlocked { offered })
            }
            Err(error) => self.stop(RingStop::Pump(error)),
        }
    }

    /// Wake from a park at `now`: take `IDLE` down and restart the idle clock.
    /// Does nothing unless parked.
    pub fn wake(&mut self, now: Duration, backing: &impl RingBacking) {
        if self.parked {
            self.parked = false;
            self.pump.leave_idle(backing);
            self.last_progress = now;
            self.relax_iter = 0;
        }
    }

    /// Hand the pump and the sink back — to reset the ring's words on
    /// `vkDestroyRingMESA`, or to look at what a sink collected.
    pub fn into_parts(self) -> (RingPump, S) {
        (self.pump, self.sink)
    }

    fn relax(&mut self, now: Duration) -> Step {
        let deadline = self.last_progress.saturating_add(self.pump.idle_timeout());
        let left = deadline.saturating_sub(now);
        if left.is_zero() {
            // Due to go idle; the next step publishes `IDLE`.
            return Step::Again;
        }
        if now.saturating_sub(self.last_progress) < self.spin {
            // Still in the hot window: yield, and leave the backoff where it
            // is, so it starts from the beginning once the window is over.
            return Step::Poll(Duration::ZERO);
        }
        self.relax_iter = self.relax_iter.saturating_add(1);
        Step::Poll(relax_backoff(self.relax_iter).min(left))
    }

    fn stop(&mut self, why: RingStop) -> Step {
        self.stopped = Some(why);
        Step::Stopped(why)
    }
}

// ------------------------------------------------------------- live threads

/// A count of this renderer's live service threads.
///
/// A diagnostic, and the thing a test asserts to show that a reset really
/// joined everything: the count is taken *before* a thread is spawned and
/// given back by the thread itself as its last act, so once a join has
/// returned the count already reflects it.
#[derive(Debug, Clone, Default)]
pub struct LiveThreads(Arc<AtomicUsize>);

impl LiveThreads {
    /// A fresh count of zero.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Service threads currently running.
    #[must_use]
    pub fn count(&self) -> usize {
        self.0.load(Ordering::Acquire)
    }

    fn enter(&self) -> LiveGuard {
        self.0.fetch_add(1, Ordering::AcqRel);
        LiveGuard(Arc::clone(&self.0))
    }
}

/// One counted thread; dropping it gives the count back.
#[derive(Debug)]
struct LiveGuard(Arc<AtomicUsize>);

impl Drop for LiveGuard {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

/// Take a lock whose holder panicked rather than panicking in turn: these are
/// guest-steered paths, and the state behind every one of these mutexes is a
/// few flags that are valid whatever a panicking holder left half-done.
fn relock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

// ------------------------------------------------------------ the ring worker

/// What the worker and the renderer share: the stop flag and the doorbell.
#[derive(Debug, Default)]
struct WorkerShared {
    /// Written outside the mutex so the hot path can read it without one, and
    /// always followed by a notify under the mutex so no waiter misses it.
    stop: AtomicBool,
    /// Set by the thread as it leaves, whatever the reason.
    ended: AtomicBool,
    /// `true` while a doorbell has arrived that the worker has not acted on.
    doorbell: Mutex<bool>,
    changed: Condvar,
}

impl WorkerShared {
    fn stopping(&self) -> bool {
        self.stop.load(Ordering::Acquire)
    }

    fn ring(&self) {
        *relock(&self.doorbell) = true;
        self.changed.notify_all();
    }

    fn forget_doorbell(&self) {
        *relock(&self.doorbell) = false;
    }

    fn request_stop(&self) {
        self.stop.store(true, Ordering::Release);
        // Under the lock, so a waiter that has checked `stop` and is about to
        // wait cannot miss this.
        let _guard = relock(&self.doorbell);
        self.changed.notify_all();
    }

    /// Block until a doorbell (consumed, `true`) or a stop (`false`).
    fn wait_for_doorbell(&self) -> bool {
        let mut rung = relock(&self.doorbell);
        loop {
            if self.stopping() {
                return false;
            }
            if *rung {
                *rung = false;
                return true;
            }
            rung = self
                .changed
                .wait(rung)
                .unwrap_or_else(PoisonError::into_inner);
        }
    }

    /// Wait out one [`Step::Blocked`]: until a doorbell (consumed), a stop,
    /// or `limit`. A doorbell that arrived before this is called ends it at
    /// once — it is checked under the lock the ringer takes — so a value
    /// recorded between the sink's look and this wait is never slept past.
    fn wait_blocked(&self, limit: Duration) {
        let mut rung = relock(&self.doorbell);
        if self.stopping() {
            return;
        }
        if !*rung {
            rung = self
                .changed
                .wait_timeout(rung, limit)
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        }
        *rung = false;
    }

    /// Sit out one poll backoff. Short ones sleep (high resolution on both
    /// hosts); longer ones wait on the condition variable so a stop ends them.
    fn pause_for(&self, backoff: Duration) {
        if backoff.is_zero() {
            std::thread::yield_now();
        } else if backoff < INTERRUPTIBLE_SLEEP {
            std::thread::sleep(backoff);
        } else {
            let rung = relock(&self.doorbell);
            if self.stopping() {
                return;
            }
            let _ = self
                .changed
                .wait_timeout(rung, backoff)
                .unwrap_or_else(PoisonError::into_inner);
        }
    }
}

/// A ring worker's stop request, as its sink sees it
/// ([`RingSink::attach_stop`](super::pump::RingSink::attach_stop)): a sink
/// that blocks inside `consume` — a Vulkan wait, in slices — polls it between
/// slices, so tearing the ring down never waits on the guest's timeout.
#[derive(Debug, Clone)]
pub struct StopSignal(Arc<WorkerShared>);

impl StopSignal {
    /// Whether the worker has been asked to stop.
    #[must_use]
    pub fn is_stopping(&self) -> bool {
        self.0.stopping()
    }

    /// A signal nobody will ever raise, for a sink run outside a worker.
    #[must_use]
    pub fn never() -> Self {
        Self(Arc::new(WorkerShared::default()))
    }
}

/// One adopted ring's worker thread. See the module docs.
///
/// Dropping it stops and joins the thread; [`stop`](Self::stop) does the same
/// and hands the [`RingPump`] back.
#[derive(Debug)]
pub struct RingWorker {
    shared: Arc<WorkerShared>,
    quiesce: Arc<Quiesce>,
    thread: Option<JoinHandle<RingPump>>,
}

impl RingWorker {
    /// Start serving `service` over `pages`, taking `quiesce` before every
    /// pass.
    ///
    /// `pages` must be the pages the service's pump was adopted over; the
    /// worker holds this `Arc` for as long as it runs, so the memory cannot go
    /// away under it.
    ///
    /// # Errors
    ///
    /// Whatever the OS says when it will not start a thread.
    pub fn spawn<S>(
        name: String,
        mut service: RingService<S>,
        pages: Arc<RingPages>,
        quiesce: Arc<Quiesce>,
        live: &LiveThreads,
    ) -> io::Result<Self>
    where
        S: RingSink + Send + 'static,
    {
        let shared = Arc::new(WorkerShared::default());
        service
            .sink_mut()
            .attach_stop(StopSignal(Arc::clone(&shared)));
        let guard = live.enter();
        let thread = {
            let shared = Arc::clone(&shared);
            let quiesce = Arc::clone(&quiesce);
            std::thread::Builder::new().name(name).spawn(move || {
                let pump = serve(service, &pages, &quiesce, &shared);
                shared.ended.store(true, Ordering::Release);
                drop(guard);
                pump
            })?
        };
        Ok(Self {
            shared,
            quiesce,
            thread: Some(thread),
        })
    }

    /// The doorbell: wake the worker if it is parked. Touches no ring memory —
    /// this runs on the device's queue worker, and the ring is the worker's.
    pub fn notify(&self) {
        self.shared.ring();
    }

    /// Whether the thread has finished — a dead ring, or a stop.
    #[must_use]
    pub fn has_ended(&self) -> bool {
        self.shared.ended.load(Ordering::Acquire)
    }

    /// Ask the thread to stop without waiting for it. Lets a caller stopping
    /// many rings signal them all before joining any.
    pub fn signal_stop(&self) {
        self.shared.request_stop();
        // A worker waiting at the pause gate re-checks its liveness predicate
        // only when woken; a reset runs on a paused VM.
        self.quiesce.wake();
    }

    /// Stop the thread, join it, and hand back the pump — `None` only if the
    /// thread panicked, which nothing in it should be able to do.
    pub fn stop(mut self) -> Option<RingPump> {
        self.join()
    }

    fn join(&mut self) -> Option<RingPump> {
        let thread = self.thread.take()?;
        self.signal_stop();
        match thread.join() {
            Ok(pump) => Some(pump),
            Err(_) => {
                tracing::error!("a Venus ring worker panicked; its ring is abandoned");
                None
            }
        }
    }
}

impl Drop for RingWorker {
    fn drop(&mut self) {
        let _ = self.join();
    }
}

/// The worker's loop. Returns the pump so the owner can reset the ring's words.
fn serve<S: RingSink>(
    mut service: RingService<S>,
    pages: &RingPages,
    quiesce: &Quiesce,
    shared: &WorkerShared,
) -> RingPump {
    let origin = Instant::now();
    let keep_going = || !shared.stopping();
    loop {
        // ADR-0005: the pass is taken before the ring is touched, held for
        // exactly one step, and never while parked or sleeping.
        let Some(pass) = quiesce.wait_while_paused(keep_going) else {
            break;
        };
        let now = origin.elapsed();
        if service.idle_due(now) {
            shared.forget_doorbell();
        }
        let step = service.step(now, pages);
        drop(pass);

        match step {
            Step::Again => {}
            Step::Poll(backoff) => shared.pause_for(backoff),
            // The pass was dropped above: a blocked ring holds none while it
            // waits, so a pause settles and a reset joins while the value it
            // waits for is still behind the (gated) device worker.
            Step::Blocked(limit) => shared.wait_blocked(limit),
            Step::Park => {
                if !shared.wait_for_doorbell() {
                    break;
                }
                // Waking writes `status`, so it is a pass of its own.
                let Some(pass) = quiesce.wait_while_paused(keep_going) else {
                    break;
                };
                service.wake(origin.elapsed(), pages);
                drop(pass);
            }
            Step::Stopped(why) => {
                tracing::warn!(
                    %why,
                    cursor = service.pump().cursor(),
                    "a Venus ring stopped for good; FATAL is published"
                );
                break;
            }
        }
    }
    service.into_parts().0
}

// ---------------------------------------------------------------- the monitor

/// One ring the monitor keeps alive.
#[derive(Debug)]
struct Watched {
    ring: u64,
    pages: Arc<RingPages>,
    status: HostWord,
}

#[derive(Debug)]
struct MonitorState {
    rings: Vec<Watched>,
    /// The shortest period any watched ring asked for, as the reference
    /// keeps it (`vkr_transport.c:234-250`). Never raised again.
    period: Duration,
    /// A ring was added or the period shortened: report now, not at the end
    /// of the current wait.
    changed: bool,
}

#[derive(Debug)]
struct MonitorShared {
    stop: AtomicBool,
    state: Mutex<MonitorState>,
    changed: Condvar,
}

/// A context's `ALIVE` monitor thread. See the module docs.
///
/// Dropping it stops and joins the thread.
#[derive(Debug)]
pub struct RingMonitor {
    shared: Arc<MonitorShared>,
    quiesce: Arc<Quiesce>,
    thread: Option<JoinHandle<()>>,
}

impl RingMonitor {
    /// Start a monitor that reports every `period` (already clamped by
    /// [`monitor_period`]) on whatever rings it is later told to
    /// [`watch`](Self::watch).
    ///
    /// # Errors
    ///
    /// Whatever the OS says when it will not start a thread.
    pub fn spawn(
        name: String,
        period: Duration,
        quiesce: Arc<Quiesce>,
        live: &LiveThreads,
    ) -> io::Result<Self> {
        let shared = Arc::new(MonitorShared {
            stop: AtomicBool::new(false),
            state: Mutex::new(MonitorState {
                rings: Vec::new(),
                period: period.max(MIN_MONITOR_PERIOD),
                changed: false,
            }),
            changed: Condvar::new(),
        });
        let guard = live.enter();
        let thread = {
            let shared = Arc::clone(&shared);
            let quiesce = Arc::clone(&quiesce);
            std::thread::Builder::new().name(name).spawn(move || {
                monitor(&shared, &quiesce);
                drop(guard);
            })?
        };
        Ok(Self {
            shared,
            quiesce,
            thread: Some(thread),
        })
    }

    /// Start keeping `ring` alive: set `ALIVE` in its `status` word at least
    /// every `period`, and at least as often as any other ring on this
    /// monitor asked for. Reports once straight away.
    pub fn watch(&self, ring: u64, pages: Arc<RingPages>, status: HostWord, period: Duration) {
        let mut state = relock(&self.shared.state);
        state.rings.push(Watched {
            ring,
            pages,
            status,
        });
        state.period = state.period.min(period.max(MIN_MONITOR_PERIOD));
        state.changed = true;
        drop(state);
        self.shared.changed.notify_all();
    }

    /// Stop keeping `ring` alive. **Synchronous**: the monitor sets `ALIVE`
    /// only while holding the lock this takes, so once this returns it will
    /// never write that ring's `status` again — which is what lets the owner
    /// zero the word afterwards and have it stay zero.
    ///
    /// Answers whether the ring was being watched.
    pub fn unwatch(&self, ring: u64) -> bool {
        let mut state = relock(&self.shared.state);
        let before = state.rings.len();
        state.rings.retain(|watched| watched.ring != ring);
        before != state.rings.len()
    }

    /// The period in force.
    #[must_use]
    pub fn period(&self) -> Duration {
        relock(&self.shared.state).period
    }

    /// How many rings are watched.
    #[must_use]
    pub fn watched(&self) -> usize {
        relock(&self.shared.state).rings.len()
    }

    fn join(&mut self) {
        let Some(thread) = self.thread.take() else {
            return;
        };
        self.shared.stop.store(true, Ordering::Release);
        {
            let _guard = relock(&self.shared.state);
            self.shared.changed.notify_all();
        }
        self.quiesce.wake();
        if thread.join().is_err() {
            tracing::error!("a Venus ring monitor panicked");
        }
    }
}

impl Drop for RingMonitor {
    fn drop(&mut self) {
        self.join();
    }
}

/// The monitor's loop: `vkr_context_ring_monitor_thread`.
fn monitor(shared: &MonitorShared, quiesce: &Quiesce) {
    let stopping = || shared.stop.load(Ordering::Acquire);
    loop {
        // Pass first, lock second, and never the other way round: `unwatch`
        // takes the lock from the device side, and a paused VM must not be
        // able to hold it hostage.
        let Some(pass) = quiesce.wait_while_paused(|| !stopping()) else {
            return;
        };
        let period = {
            let mut state = relock(&shared.state);
            for watched in &state.rings {
                watched.pages.set_status_bits(&watched.status, STATUS_ALIVE);
            }
            state.changed = false;
            state.period
        };
        drop(pass);

        let deadline = Instant::now() + period;
        let mut state = relock(&shared.state);
        loop {
            if stopping() {
                return;
            }
            if state.changed {
                break;
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                break;
            }
            state = shared
                .changed
                .wait_timeout(state, left)
                .map(|(state, _)| state)
                .unwrap_or_else(|poisoned| poisoned.into_inner().0);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::venus::pump::{Batch, Consumed, MAX_IDLE_TIMEOUT, STATUS_FATAL, STATUS_IDLE};
    use crate::venus::ring::{GuestWord, Region, RingCreateInfo, RingLayout};

    /// The resource every fixture lives in.
    const RESOURCE: u64 = 0x1000;
    /// A small command buffer.
    const BUFFER: u32 = 256;
    /// The idle timeout the deterministic tests use: Mesa's one millisecond.
    const TIMEOUT: Duration = Duration::from_millis(1);

    const fn us(n: u64) -> Duration {
        Duration::from_micros(n)
    }

    /// Every write the service made to the status word, in order.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum StatusWrite {
        Set(u32),
        Clear(u32),
    }

    /// A guest, played by hand, over a plain byte vector: no `unsafe`, no
    /// threads, and a hook for the race the idle re-read exists for.
    #[derive(Debug, Default)]
    struct Inner {
        mem: Vec<u8>,
        writes: Vec<StatusWrite>,
        /// Bytes the guest stores (and publishes with a new `tail`) the moment
        /// the host sets `IDLE` — between the publication and the re-read.
        on_idle: Option<Vec<u8>>,
    }

    #[derive(Debug)]
    struct Guest {
        inner: Mutex<Inner>,
        layout: RingLayout,
    }

    impl Guest {
        fn new(idle_timeout: Duration) -> Self {
            let info = RingCreateInfo {
                flags: 0,
                resource_id: 1,
                offset: 0,
                size: RESOURCE,
                idle_timeout_ns: u64::try_from(idle_timeout.as_nanos()).expect("fits"),
                head_offset: 0,
                tail_offset: 4,
                status_offset: 8,
                buffer_offset: 16,
                buffer_size: u64::from(BUFFER),
                extra_offset: 0,
                extra_size: 0,
            };
            Self {
                inner: Mutex::new(Inner {
                    mem: vec![0; RESOURCE as usize],
                    ..Inner::default()
                }),
                layout: RingLayout::new(info, RESOURCE).expect("fixture layout"),
            }
        }

        fn service<S: RingSink>(&self, sink: S, now: Duration) -> RingService<S> {
            let pump = RingPump::new(self.layout, self).expect("a zeroed ring");
            RingService::new(pump, sink, now)
        }

        fn word(&self, at: u64) -> u32 {
            let inner = relock(&self.inner);
            let at = at as usize;
            u32::from_le_bytes(inner.mem[at..at + 4].try_into().expect("four bytes"))
        }

        fn poke(inner: &mut Inner, at: u64, value: u32) {
            let at = at as usize;
            inner.mem[at..at + 4].copy_from_slice(&value.to_le_bytes());
        }

        fn head(&self) -> u32 {
            self.word(self.layout.head().offset())
        }

        fn tail(&self) -> u32 {
            self.word(self.layout.tail().offset())
        }

        fn status(&self) -> u32 {
            self.word(self.layout.status().offset())
        }

        /// Write `bytes` at the current tail and publish the new tail — one
        /// Mesa ring submission. Rings no doorbell; the test decides that.
        fn submit(&self, bytes: &[u8]) {
            let mut inner = relock(&self.inner);
            Self::submit_locked(&mut inner, self.layout, bytes);
        }

        fn submit_locked(inner: &mut Inner, layout: RingLayout, bytes: &[u8]) {
            let tail_at = layout.tail().offset() as usize;
            let tail = u32::from_le_bytes(inner.mem[tail_at..tail_at + 4].try_into().expect("4"));
            let base = layout.buffer().start() as usize;
            for (i, byte) in bytes.iter().enumerate() {
                let at = (tail as usize + i) % BUFFER as usize;
                inner.mem[base + at] = *byte;
            }
            let tail = tail.wrapping_add(u32::try_from(bytes.len()).expect("fits"));
            Self::poke(inner, layout.tail().offset(), tail);
        }

        /// The guest's watchdog arming: its own atomic AND on `status`.
        fn clear_alive(&self) {
            let mut inner = relock(&self.inner);
            let at = self.layout.status().offset();
            let value = {
                let at = at as usize;
                u32::from_le_bytes(inner.mem[at..at + 4].try_into().expect("4"))
            };
            Self::poke(&mut inner, at, value & !STATUS_ALIVE);
        }

        fn writes(&self) -> Vec<StatusWrite> {
            relock(&self.inner).writes.clone()
        }
    }

    impl RingBacking for Guest {
        fn load_host_word(&self, word: &HostWord) -> u32 {
            self.word(word.offset())
        }

        fn load_guest_word(&self, word: &GuestWord) -> u32 {
            self.word(word.offset())
        }

        fn store_head(&self, head: &HostWord, value: u32) {
            Self::poke(&mut relock(&self.inner), head.store_offset(), value);
        }

        fn set_status_bits(&self, status: &HostWord, bits: u32) {
            let mut inner = relock(&self.inner);
            let at = status.store_offset();
            let value = {
                let at = at as usize;
                u32::from_le_bytes(inner.mem[at..at + 4].try_into().expect("4"))
            };
            Self::poke(&mut inner, at, value | bits);
            inner.writes.push(StatusWrite::Set(bits));
            if bits & STATUS_IDLE != 0 {
                if let Some(bytes) = inner.on_idle.take() {
                    Self::submit_locked(&mut inner, self.layout, &bytes);
                }
            }
        }

        fn clear_status_bits(&self, status: &HostWord, bits: u32) {
            let mut inner = relock(&self.inner);
            let at = status.store_offset();
            let value = {
                let at = at as usize;
                u32::from_le_bytes(inner.mem[at..at + 4].try_into().expect("4"))
            };
            Self::poke(&mut inner, at, value & !bits);
            inner.writes.push(StatusWrite::Clear(bits));
        }

        fn read_buffer(&self, buffer: &Region, offset: u64, dst: &mut [u8]) {
            let inner = relock(&self.inner);
            let from = (buffer.start() + offset) as usize;
            dst.copy_from_slice(&inner.mem[from..from + dst.len()]);
        }
    }

    /// A sink that takes everything and keeps it.
    #[derive(Debug, Default)]
    struct Swallow(Vec<u8>);

    impl RingSink for Swallow {
        fn consume(&mut self, batch: Batch<'_>) -> Consumed {
            self.0.extend_from_slice(batch.bytes());
            batch.all()
        }
    }

    /// Step until the service parks, stops, or asks to wait; returns the step
    /// it ended on. Bounded, so a regression is a failure rather than a hang.
    fn settle<S: RingSink>(svc: &mut RingService<S>, now: Duration, ring: &Guest) -> Step {
        for _ in 0..64 {
            match svc.step(now, ring) {
                Step::Again => continue,
                other => return other,
            }
        }
        panic!("the service never settled at {now:?}");
    }

    // --------------------------------------------------------- the backoff

    #[test]
    fn the_backoff_yields_sixteen_times_then_sleeps_doubling_like_the_reference() {
        // `iter` is incremented before it is judged, as in `vkr_ring_relax`,
        // so the first sixteen *values* are yields and a fresh ring yields
        // fifteen times before its first sleep.
        for iter in 0..16 {
            assert_eq!(relax_backoff(iter), Duration::ZERO, "iter {iter}");
        }
        assert_eq!(relax_backoff(16), us(10));
        assert_eq!(relax_backoff(31), us(10));
        assert_eq!(relax_backoff(32), us(20));
        assert_eq!(relax_backoff(64), us(40));
        assert_eq!(relax_backoff(1 << 20), us(10 << 16));
        // The top of the range neither overflows nor panics.
        assert_eq!(relax_backoff(u32::MAX), us(10 << 27));
    }

    #[test]
    fn a_poll_never_sleeps_past_the_moment_the_ring_is_due_to_go_idle() {
        let ring = Guest::new(MAX_IDLE_TIMEOUT);
        let mut svc = ring.service(Swallow::default(), Duration::ZERO);
        let mut now = Duration::ZERO;
        let mut yields = 0;
        loop {
            match svc.step(now, &ring) {
                Step::Poll(backoff) => {
                    if backoff.is_zero() {
                        yields += 1;
                    }
                    assert!(now + backoff <= MAX_IDLE_TIMEOUT, "slept past the deadline");
                    // Advance by the backoff, or a little for a yield.
                    now += backoff.max(us(1));
                }
                Step::Again => {}
                Step::Park => break,
                Step::Blocked(limit) => panic!("blocked for {limit:?}"),
                Step::Stopped(why) => panic!("stopped: {why}"),
            }
        }
        assert_eq!(yields, 15, "the busy-wait phase is fifteen yields");
        assert!(
            now >= MAX_IDLE_TIMEOUT,
            "parked at {now:?}, before the timeout"
        );
        assert_eq!(ring.status(), STATUS_IDLE);
    }

    // ------------------------------------------------ the spin window

    /// Step `svc` every 10 µs from `from` until it parks or `until`: the time
    /// and length of the first poll that sleeps, and how many yields came
    /// before it.
    fn first_sleep<S: RingSink>(
        svc: &mut RingService<S>,
        ring: &Guest,
        from: Duration,
        until: Duration,
    ) -> (Option<(Duration, Duration)>, u32) {
        let mut now = from;
        let mut yields = 0;
        while now < until {
            match svc.step(now, ring) {
                Step::Poll(backoff) if backoff.is_zero() => yields += 1,
                Step::Poll(backoff) => return (Some((now, backoff)), yields),
                Step::Again => {}
                Step::Park => break,
                other => panic!("unexpected {other:?}"),
            }
            now += us(10);
        }
        (None, yields)
    }

    #[test]
    fn a_spinning_ring_yields_for_its_window_and_then_starts_vkrs_backoff_from_the_beginning() {
        let ring = Guest::new(MAX_IDLE_TIMEOUT);
        let spin = Duration::from_millis(2);
        let mut svc = ring
            .service(Swallow::default(), Duration::ZERO)
            .with_spin(spin);
        let (first, yields) = first_sleep(&mut svc, &ring, Duration::ZERO, MAX_IDLE_TIMEOUT);
        let (at, backoff) = first.expect("it sleeps once the window is over");
        // 200 polls in the window, then vkr's fifteen yields, then its first
        // sleep — 10 µs, not whatever the window's polls would have grown it to.
        assert_eq!(yields, 200 + 15);
        assert!(at >= spin, "slept at {at:?}, inside the window");
        assert_eq!(backoff, us(10));
        assert_eq!(ring.status(), 0, "not idle");
    }

    #[test]
    fn progress_opens_the_spin_window_again() {
        let ring = Guest::new(MAX_IDLE_TIMEOUT);
        let spin = Duration::from_millis(2);
        let mut svc = ring
            .service(Swallow::default(), Duration::ZERO)
            .with_spin(spin);
        let (first, _) = first_sleep(&mut svc, &ring, Duration::ZERO, MAX_IDLE_TIMEOUT);
        let (at, _) = first.expect("a sleep after the first window");
        // Work five milliseconds in: the next polls yield again, for a whole
        // window from that progress.
        let work = at + Duration::from_millis(3);
        ring.submit(b"work");
        assert_eq!(svc.step(work, &ring), Step::Again);
        let (again, yields) = first_sleep(&mut svc, &ring, work + us(10), MAX_IDLE_TIMEOUT);
        let (at, backoff) = again.expect("a sleep after the second window");
        assert!(at >= work + spin, "slept at {at:?}, inside the new window");
        assert_eq!(backoff, us(10));
        assert!(yields >= 199, "{yields} yields");
    }

    #[test]
    fn with_mesas_timeout_a_spinning_ring_never_sleeps_and_still_parks_on_time() {
        let ring = Guest::new(TIMEOUT);
        let mut svc = ring
            .service(Swallow::default(), Duration::ZERO)
            .with_spin(Duration::from_millis(2));
        let (first, yields) = first_sleep(&mut svc, &ring, Duration::ZERO, us(5000));
        assert_eq!(first, None, "no poll inside Mesa's 1 ms window sleeps");
        assert_eq!(yields, 100);
        assert_eq!(ring.status(), STATUS_IDLE, "parked at the timeout, as ever");
        assert!(svc.is_parked());
    }

    #[test]
    fn the_host_spin_covers_mesas_window_only_where_sleeps_are_coarse() {
        if cfg!(windows) {
            assert!(HOST_SPIN >= TIMEOUT, "all of Mesa's 1 ms idleTimeout");
            assert!(HOST_SPIN < MAX_IDLE_TIMEOUT);
        } else {
            assert_eq!(HOST_SPIN, Duration::ZERO, "vkr's backoff, exactly");
        }
    }

    // -------------------------------------------------- the idle contract

    #[test]
    fn idle_is_published_only_once_the_timeout_has_passed_without_progress() {
        let ring = Guest::new(TIMEOUT);
        let mut svc = ring.service(Swallow::default(), Duration::ZERO);

        // A freshly adopted ring polls: it is not idle, so the guest's first
        // submission is picked up without a doorbell.
        for now in [0, 1, 500, 999] {
            assert!(
                matches!(svc.step(us(now), &ring), Step::Poll(_)),
                "at {now} µs"
            );
            assert_eq!(ring.status(), 0, "IDLE at {now} µs, before the timeout");
        }
        assert!(ring.writes().is_empty());

        // Work at 600 µs pushes the deadline out to 1600 µs.
        ring.submit(b"work");
        assert_eq!(svc.step(us(600), &ring), Step::Again);
        assert_eq!(svc.sink().0, b"work");
        assert!(matches!(svc.step(us(1500), &ring), Step::Poll(_)));
        assert_eq!(
            ring.status(),
            0,
            "IDLE before the timeout since the last progress"
        );

        // At the deadline: IDLE, then park.
        assert_eq!(settle(&mut svc, us(1600), &ring), Step::Park);
        assert_eq!(ring.status(), STATUS_IDLE);
        assert_eq!(ring.writes(), vec![StatusWrite::Set(STATUS_IDLE)]);
        assert!(svc.is_parked());

        // Parked means parked: stepping again touches nothing.
        let writes = ring.writes().len();
        ring.submit(b"no doorbell");
        assert_eq!(svc.step(us(5000), &ring), Step::Park);
        assert_eq!(ring.writes().len(), writes);
        assert_eq!(ring.head(), 4, "a parked ring consumed without a doorbell");
    }

    #[test]
    fn a_zero_idle_timeout_parks_at_once_as_the_guest_asked() {
        let ring = Guest::new(Duration::ZERO);
        let mut svc = ring.service(Swallow::default(), Duration::ZERO);
        assert_eq!(svc.step(Duration::ZERO, &ring), Step::Park);
        assert_eq!(ring.status(), STATUS_IDLE);
    }

    /// The bug this module exists for, in the shape a real Mesa guest makes
    /// it: `SetReply` rings the doorbell, the command written a few
    /// microseconds later does not (Mesa's one-per-millisecond rate limit),
    /// and both must be consumed.
    #[test]
    fn two_submissions_within_the_idle_window_and_one_doorbell_are_both_consumed() {
        let ring = Guest::new(TIMEOUT);
        let mut svc = ring.service(Swallow::default(), Duration::ZERO);
        // Nothing for a while: the ring parks.
        assert_eq!(settle(&mut svc, us(1000), &ring), Step::Park);
        assert_eq!(ring.status() & STATUS_IDLE, STATUS_IDLE);

        // t = 10 ms: submission one. The guest sees IDLE and rings.
        let set_reply = [0xb2u8; 36];
        ring.submit(&set_reply);
        assert_eq!(ring.status() & STATUS_IDLE, STATUS_IDLE, "the guest rings");
        // The doorbell wakes the worker a little later.
        svc.wake(us(10_050), &ring);
        assert_eq!(ring.status() & STATUS_IDLE, 0, "waking takes IDLE down");
        assert_eq!(
            settle(&mut svc, us(10_060), &ring),
            Step::Poll(Duration::ZERO)
        );
        assert_eq!(ring.head(), 36);

        // t = 10.07 ms: submission two. The status is not IDLE — and even if it
        // were, the guest's rate limit forbids a second doorbell this soon.
        let command = [0x89u8, 0, 0, 0, 1, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0];
        ring.submit(&command);

        // The worker is still polling inside its window, and picks it up.
        assert_eq!(
            settle(&mut svc, us(10_200), &ring),
            Step::Poll(Duration::ZERO)
        );
        assert_eq!(ring.head(), 52, "the second submission was consumed");
        let mut want = set_reply.to_vec();
        want.extend_from_slice(&command);
        assert_eq!(svc.sink().0, want);
        // IDLE never went up between the two.
        assert_eq!(
            ring.writes(),
            vec![
                StatusWrite::Set(STATUS_IDLE),
                StatusWrite::Clear(STATUS_IDLE)
            ]
        );

        // Only after a full window with nothing does it go idle again.
        assert!(matches!(svc.step(us(11_100), &ring), Step::Poll(_)));
        assert_eq!(settle(&mut svc, us(11_200), &ring), Step::Park);
    }

    #[test]
    fn work_stored_between_the_idle_publication_and_the_re_read_is_not_lost() {
        let ring = Guest::new(TIMEOUT);
        let mut svc = ring.service(Swallow::default(), Duration::ZERO);
        // The guest stores its tail exactly between our IDLE and our re-read:
        // it read a not-yet-idle status, so it rings no doorbell.
        relock(&ring.inner).on_idle = Some(b"late".to_vec());

        assert_eq!(svc.step(us(1000), &ring), Step::Again);
        assert!(!svc.is_parked(), "parked on work that was already there");
        assert_eq!(ring.head(), 4);
        assert_eq!(svc.sink().0, b"late");
        // IDLE went up and came straight back down.
        assert_eq!(
            ring.writes(),
            vec![
                StatusWrite::Set(STATUS_IDLE),
                StatusWrite::Clear(STATUS_IDLE)
            ]
        );
        assert_eq!(ring.status(), 0);
        // The progress restarted the window.
        assert!(matches!(svc.step(us(1500), &ring), Step::Poll(_)));
        assert_eq!(settle(&mut svc, us(2000), &ring), Step::Park);
    }

    #[test]
    fn waking_takes_idle_down_and_restarts_the_window() {
        let ring = Guest::new(TIMEOUT);
        let mut svc = ring.service(Swallow::default(), Duration::ZERO);
        assert_eq!(settle(&mut svc, us(1000), &ring), Step::Park);
        assert_eq!(ring.status(), STATUS_IDLE);

        // Waking when not parked is a no-op; waking parked clears IDLE.
        svc.wake(us(4000), &ring);
        assert_eq!(ring.status(), 0);
        assert!(!svc.is_parked());
        let writes = ring.writes().len();
        svc.wake(us(4001), &ring);
        assert_eq!(ring.writes().len(), writes, "a second wake wrote again");

        // A doorbell with nothing behind it: the ring polls for a full window
        // from the wake, then parks again.
        assert!(matches!(svc.step(us(4999), &ring), Step::Poll(_)));
        assert_eq!(ring.status(), 0);
        assert_eq!(settle(&mut svc, us(5000), &ring), Step::Park);
        assert_eq!(ring.status(), STATUS_IDLE);
    }

    #[test]
    fn the_pumps_idle_bit_never_disturbs_the_monitors_alive_bit() {
        let ring = Guest::new(TIMEOUT);
        let mut svc = ring.service(Swallow::default(), Duration::ZERO);
        ring.set_status_bits(&ring.layout.status(), STATUS_ALIVE);
        assert_eq!(settle(&mut svc, us(1000), &ring), Step::Park);
        assert_eq!(ring.status(), STATUS_ALIVE | STATUS_IDLE);
        ring.clear_alive();
        svc.wake(us(2000), &ring);
        assert_eq!(ring.status(), 0, "waking resurrected a cleared ALIVE");
    }

    // ------------------------------------------------------------ the end

    #[test]
    fn a_sink_that_cannot_answer_stops_the_ring_and_head_stays_on_the_command() {
        /// Answers the first 36 bytes, cannot answer what follows.
        struct Honest;
        impl RingSink for Honest {
            fn consume(&mut self, batch: Batch<'_>) -> Consumed {
                if batch.len() > 36 {
                    batch.fatal_after(36)
                } else {
                    batch.all()
                }
            }
        }
        let ring = Guest::new(TIMEOUT);
        let mut svc = ring.service(Honest, Duration::ZERO);
        ring.submit(&[0u8; 36]);
        ring.submit(&[0u8; 16]);

        let step = svc.step(us(10), &ring);
        assert_eq!(
            step,
            Step::Stopped(RingStop::Pump(PumpError::SinkFatal {
                offered: 52,
                consumed: 36
            }))
        );
        assert_eq!(ring.head(), 36, "head moved past a command nobody answered");
        assert_eq!(ring.status() & STATUS_FATAL, STATUS_FATAL);

        // Stopped is stopped: the same answer, and no memory is touched.
        let writes = ring.writes();
        ring.submit(&[0u8; 8]);
        assert_eq!(svc.step(us(5000), &ring), step);
        assert_eq!(ring.writes(), writes);
        assert_eq!(ring.head(), 36);
        assert!(!svc.idle_due(us(5000)));
    }

    #[test]
    fn an_impossible_tail_and_a_deadlocked_ring_both_stop_it_with_fatal() {
        let ring = Guest::new(TIMEOUT);
        let mut svc = ring.service(Swallow::default(), Duration::ZERO);
        Guest::poke(
            &mut relock(&ring.inner),
            ring.layout.tail().offset(),
            0x7fff_ffff,
        );
        assert!(matches!(
            svc.step(us(1), &ring),
            Step::Stopped(RingStop::Pump(PumpError::TailOutOfRange { .. }))
        ));
        assert_eq!(ring.status() & STATUS_FATAL, STATUS_FATAL);
        assert_eq!(ring.head(), 0);

        /// Takes nothing, ever.
        struct Refuses;
        impl RingSink for Refuses {
            fn consume(&mut self, batch: Batch<'_>) -> Consumed {
                batch.nothing()
            }
        }
        let ring = Guest::new(TIMEOUT);
        let mut svc = ring.service(Refuses, Duration::ZERO);
        // A partial batch is a stall: the ring polls, then parks on it.
        ring.submit(&[1u8; 8]);
        assert!(matches!(svc.step(us(1), &ring), Step::Poll(_)));
        assert_eq!(settle(&mut svc, us(1000), &ring), Step::Park);
        // A full one can never resolve itself.
        svc.wake(us(2000), &ring);
        ring.submit(&vec![1u8; BUFFER as usize - 8]);
        assert_eq!(
            svc.step(us(2001), &ring),
            Step::Stopped(RingStop::Deadlocked { offered: BUFFER })
        );
        assert_eq!(ring.status() & STATUS_FATAL, STATUS_FATAL);
        assert_eq!(ring.head(), 0);
        assert_eq!(ring.tail(), BUFFER);
    }

    #[test]
    fn a_blocked_sink_is_offered_the_same_bytes_again_never_parks_and_never_deadlocks() {
        /// Takes four-byte commands; a zero word is "wait for `open`".
        struct Gate {
            open: Arc<AtomicBool>,
            offers: usize,
        }
        impl RingSink for Gate {
            fn consume(&mut self, batch: Batch<'_>) -> Consumed {
                self.offers += 1;
                let mut done = 0;
                for word in batch.bytes().chunks_exact(4) {
                    if word == [0; 4] && !self.open.load(Ordering::SeqCst) {
                        return batch.blocked(done);
                    }
                    done += 4;
                }
                batch.consumed(done)
            }
        }
        let open = Arc::new(AtomicBool::new(false));
        let ring = Guest::new(TIMEOUT);
        let gate = Gate {
            open: Arc::clone(&open),
            offers: 0,
        };
        let mut svc = ring.service(gate, Duration::ZERO);
        ring.submit(&[1, 1, 1, 1, 0, 0, 0, 0, 2, 2, 2, 2]);
        // What came before the wait is released; the wait is not.
        let Step::Blocked(first) = svc.step(us(1), &ring) else {
            panic!("not blocked")
        };
        assert_eq!(first, INTERRUPTIBLE_SLEEP);
        assert_eq!(ring.head(), 4);
        // Long past the idle timeout, with `tail` unmoved, the same bytes are
        // offered again, `IDLE` never goes up, and the wait grows to its cap.
        let mut last = first;
        for i in 1..20u32 {
            let Step::Blocked(limit) = svc.step(us(1) + TIMEOUT * 10 * i, &ring) else {
                panic!("not blocked")
            };
            assert!(limit >= last && limit <= MAX_BLOCKED_WAIT);
            last = limit;
        }
        assert_eq!(last, MAX_BLOCKED_WAIT);
        assert_eq!(svc.sink().offers, 20);
        assert_eq!(ring.status() & STATUS_IDLE, 0);
        assert_eq!(ring.head(), 4);
        // Blocked on a full ring is still not a deadlock.
        ring.submit(&vec![3u8; BUFFER as usize - 12]);
        assert!(matches!(svc.step(us(500_000), &ring), Step::Blocked(_)));
        assert_eq!(ring.status() & STATUS_FATAL, 0);
        // Once it opens, the rest runs, and the ring idles as usual.
        open.store(true, Ordering::SeqCst);
        assert_eq!(svc.step(us(500_001), &ring), Step::Again);
        assert_eq!(ring.head(), ring.tail());
        assert_eq!(settle(&mut svc, us(600_000), &ring), Step::Park);
    }

    // --------------------------------------------------------- the monitor

    #[test]
    fn the_monitor_period_is_refused_at_zero_and_raised_to_the_floor() {
        assert_eq!(monitor_period(0), None, "the reference refuses zero");
        assert_eq!(monitor_period(1), Some(MIN_MONITOR_PERIOD));
        assert_eq!(monitor_period(999), Some(MIN_MONITOR_PERIOD));
        assert_eq!(monitor_period(1_000), Some(MIN_MONITOR_PERIOD));
        assert_eq!(monitor_period(1_001), Some(us(1_001)));
        // What Mesa actually sends.
        assert_eq!(monitor_period(3_000_000), Some(Duration::from_secs(3)));
        assert_eq!(monitor_period(u32::MAX), Some(us(u64::from(u32::MAX))));
    }

    /// Wait (generously) for `cond`, failing with `what` if it never holds.
    fn eventually(what: &str, cond: impl Fn() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while !cond() {
            assert!(Instant::now() < deadline, "timed out waiting for {what}");
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    fn real_pages() -> (Arc<RingPages>, RingLayout) {
        let info = RingCreateInfo {
            flags: 0,
            resource_id: 1,
            offset: 0,
            size: RESOURCE,
            idle_timeout_ns: 1_000,
            head_offset: 0,
            tail_offset: 4,
            status_offset: 8,
            buffer_offset: 16,
            buffer_size: u64::from(BUFFER),
            extra_offset: 0,
            extra_size: 0,
        };
        let layout = RingLayout::new(info, RESOURCE).expect("fixture layout");
        (Arc::new(RingPages::new(RESOURCE).expect("a page")), layout)
    }

    #[test]
    fn the_monitor_sets_alive_again_after_the_guest_clears_it() {
        let (pages, layout) = real_pages();
        let live = LiveThreads::new();
        let quiesce = Quiesce::new();
        let status = layout.status();
        let alive = || pages.load_host_word(&status) & STATUS_ALIVE != 0;

        let monitor = RingMonitor::spawn(
            "venus-mon-test".into(),
            MIN_MONITOR_PERIOD,
            Arc::clone(&quiesce),
            &live,
        )
        .expect("a monitor thread");
        assert_eq!(live.count(), 1);
        monitor.watch(7, Arc::clone(&pages), status, MIN_MONITOR_PERIOD);
        eventually("the first ALIVE", alive);

        // The guest's watchdog arms, several times over; each time the monitor
        // answers within a period or so.
        for _ in 0..5 {
            pages.clear_status_bits(&status, STATUS_ALIVE);
            eventually("ALIVE after the guest cleared it", alive);
        }

        // Paused, it writes nothing: ADR-0005.
        quiesce.pause();
        assert!(quiesce.wait_until_idle(Duration::from_secs(5)));
        pages.clear_status_bits(&status, STATUS_ALIVE);
        std::thread::sleep(Duration::from_millis(30));
        assert!(!alive(), "the monitor wrote guest memory while paused");
        quiesce.resume();
        eventually("ALIVE after resume", alive);

        // Unwatched, it never writes that word again — synchronously.
        assert!(monitor.unwatch(7));
        assert!(!monitor.unwatch(7));
        pages.clear_status_bits(&status, u32::MAX);
        std::thread::sleep(Duration::from_millis(30));
        assert_eq!(pages.load_host_word(&status), 0);

        // And it stops while paused, which is when a reset runs.
        quiesce.pause();
        drop(monitor);
        assert_eq!(live.count(), 0);
    }

    #[test]
    fn the_monitor_keeps_the_shortest_period_it_was_asked_for() {
        let live = LiveThreads::new();
        let (pages, layout) = real_pages();
        let monitor = RingMonitor::spawn(
            "venus-mon-test".into(),
            Duration::from_secs(3),
            Quiesce::new(),
            &live,
        )
        .expect("a monitor thread");
        assert_eq!(monitor.period(), Duration::from_secs(3));
        monitor.watch(
            1,
            Arc::clone(&pages),
            layout.status(),
            Duration::from_secs(5),
        );
        assert_eq!(monitor.period(), Duration::from_secs(3), "never raised");
        monitor.watch(
            2,
            Arc::clone(&pages),
            layout.status(),
            Duration::from_millis(40),
        );
        assert_eq!(monitor.period(), Duration::from_millis(40));
        monitor.watch(3, pages, layout.status(), Duration::ZERO);
        assert_eq!(monitor.period(), MIN_MONITOR_PERIOD, "floored");
        assert_eq!(monitor.watched(), 3);
    }
}
