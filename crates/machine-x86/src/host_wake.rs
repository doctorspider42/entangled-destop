//! Host wakeups for machines whose queue kicks are synchronous — the
//! userspace-irqchip attach paths WHP uses ([ADR-0002], the 2026-09-24
//! amendment).
//!
//! A device with asynchronous *host* work (virtio-gpu's fences: a host GPU
//! finishing, observed on a renderer thread) asks to be served through
//! [`virtio_core::HostWaker`]: "call my queue 0 from your ordinary worker
//! context, exactly as if the guest had kicked it". On KVM that context is the
//! ioeventfd worker of `crate::notify`, and a wake is a write to queue 0's
//! eventfd. A synchronous-kick machine has no such worker — every kick runs on
//! the vCPU thread that took the exit — so until this module existed those
//! devices got **no waker at all**, and every path that depended on one fell
//! back to answering at once. For Venus's per-`ring_idx` fences that fallback
//! meant signalling a fence before the GPU work it guards had run.
//!
//! [`HostWakeService`] is the missing worker, and deliberately nothing more:
//!
//! * **One thread per bus**, not per device. Only a device with host work of
//!   its own ever wakes it, which today is one virtio-gpu; the others cost one
//!   idle slot in a bitmap.
//! * **The same notify a kick makes.** A wake of slot *n* ends in
//!   `transport.lock().queue_notify(0)` — the entry point the Linux worker
//!   calls, under the same `Arc<Mutex<_>>` the vCPUs take for a register
//!   access, so the device is serialised against guest kicks exactly as it is
//!   on KVM, and whatever interrupt it raises goes through the same
//!   `Arc<dyn IrqLine>` (the userspace IOAPIC, or MSI-X through
//!   `UserspaceMsiSink`). Raising an interrupt from a host thread is already
//!   what virtio-net's receive worker does on this host.
//! * **Coalesced.** A wake sets its slot's pending bit; only the wake that
//!   *sets* it takes the condvar. However many wakes land before the thread
//!   gets to a slot, the slot is served once — a virtqueue drain serves
//!   everything that is ready, which is the whole reason that is correct.
//!
//! # ADR-0005
//!
//! The thread is a host worker that runs device code, so it owes the machine
//! what the ioeventfd workers owe it:
//!
//! * **pause** — it takes [`Quiesce::wait_while_paused`] before every slot it
//!   serves, *before* the transport lock and outside every device lock, and
//!   holds the [`virtio_core::quiesce::Pass`] across the notify. A paused VM
//!   therefore settles with the thread parked on the gate (holding no pass) or
//!   asleep on its condvar; a wake that arrives while paused is remembered and
//!   served on resume.
//! * **reset** — [`HostWakeService::reset`] stops and joins the thread, drops
//!   every pending wake (they belong to the boot being reset), and starts a
//!   fresh one. It runs on a quiesced VM, which is why stopping pairs the flag
//!   with [`Quiesce::wake`]: a thread parked on a closed gate must be able to
//!   leave it.
//! * **shutdown** — [`HostWakeService::shutdown`] stops and joins, idempotent,
//!   and also runs from `Drop`, so a VM that ends leaves no thread behind.
//!
//! The wakers it hands out hold only the shared pending state, never the
//! thread or the transports, so a renderer thread that outlives the bus keeps
//! a harmless handle rather than keeping the machine alive.
//!
//! Portable: nothing here is host-specific, and every test runs on both hosts.
//!
//! [ADR-0002]: ../../../docs/adr/0002-linux-first-whp-ready.md

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, PoisonError};
use std::thread::JoinHandle;

use virtio_core::{HostWaker, MmioTransport, PciTransport, Quiesce};

/// Name prefix of the service thread (`virtio-wake-mmio`, `virtio-wake-pci`).
///
/// Public so a test can tell a notify the service delivered from one a vCPU
/// delivered; nothing else should depend on it.
pub const HOST_WAKE_THREAD_PREFIX: &str = "virtio-wake";

/// What the service needs from a transport: the queue-notify entry point a
/// kick reaches, and whether a driver is there to be notified.
pub trait WakeTarget: Send + 'static {
    /// `queue_notify(value)` exactly as a guest kick or the KVM worker makes
    /// it.
    fn queue_notify(&mut self, value: u32);
    /// Whether the driver has written `DRIVER_OK` since the last reset.
    fn is_activated(&self) -> bool;
    /// Transport name, for the thread name and log records.
    fn transport_name() -> &'static str;
}

impl WakeTarget for MmioTransport {
    fn queue_notify(&mut self, value: u32) {
        self.queue_notify(value);
    }
    fn is_activated(&self) -> bool {
        self.is_activated()
    }
    fn transport_name() -> &'static str {
        "mmio"
    }
}

impl WakeTarget for PciTransport {
    fn queue_notify(&mut self, value: u32) {
        self.queue_notify(value);
    }
    fn is_activated(&self) -> bool {
        self.is_activated()
    }
    fn transport_name() -> &'static str {
        "pci"
    }
}

/// The state wakers and the thread share.
struct Shared {
    /// One bit per slot: set by a wake, cleared by the thread just before it
    /// serves the slot. Bounded by the bus, never by the guest.
    pending: Vec<AtomicBool>,
    /// "Something is pending", the condvar's predicate.
    signal: Mutex<bool>,
    changed: Condvar,
    /// Asks the thread to leave, from its condvar *and* from the pause gate.
    stopping: AtomicBool,
}

impl Shared {
    fn raise(&self, slot: usize) {
        let Some(bit) = self.pending.get(slot) else {
            return;
        };
        // Only the wake that sets the bit needs the thread: if it was already
        // set, the thread has not taken it yet and will serve it after this
        // wake — which is all a wake asks for.
        if bit.swap(true, Ordering::AcqRel) {
            return;
        }
        self.signal_thread();
    }

    fn signal_thread(&self) {
        let mut signal = self.signal.lock().unwrap_or_else(PoisonError::into_inner);
        *signal = true;
        self.changed.notify_one();
    }

    fn clear(&self) {
        for bit in &self.pending {
            bit.store(false, Ordering::Release);
        }
        *self.signal.lock().unwrap_or_else(PoisonError::into_inner) = false;
    }
}

/// The [`HostWaker`] one device holds.
struct SlotWaker {
    shared: Arc<Shared>,
    slot: usize,
}

impl HostWaker for SlotWaker {
    fn wake(&self) {
        self.shared.raise(self.slot);
    }
}

/// A synchronous-kick bus's host-wake worker. See the module docs.
pub struct HostWakeService<T: WakeTarget> {
    shared: Arc<Shared>,
    /// The transports, by slot; empty until [`Self::start`].
    transports: Mutex<Vec<Arc<Mutex<T>>>>,
    /// The VM's pause gate, installed after the bus is built and re-read for
    /// every slot served (as `crate::notify` does).
    quiesce: Arc<Mutex<Arc<Quiesce>>>,
    worker: Mutex<Option<JoinHandle<()>>>,
    /// Set by the first [`Self::shutdown`]; a reset after it restarts nothing.
    torn_down: AtomicBool,
}

impl<T: WakeTarget> HostWakeService<T> {
    /// A service for a bus of `slots` devices, with no thread yet.
    ///
    /// Built *before* the devices move into their transports, because a
    /// device takes its waker while the machine layer still holds it
    /// ([`virtio_core::VirtioDevice::set_host_waker`]); a wake in the gap is
    /// remembered and served once [`Self::start`] runs.
    pub fn new(slots: usize) -> Self {
        Self {
            shared: Arc::new(Shared {
                pending: (0..slots).map(|_| AtomicBool::new(false)).collect(),
                signal: Mutex::new(false),
                changed: Condvar::new(),
                stopping: AtomicBool::new(false),
            }),
            transports: Mutex::new(Vec::new()),
            quiesce: Arc::new(Mutex::new(Quiesce::new())),
            worker: Mutex::new(None),
            torn_down: AtomicBool::new(false),
        }
    }

    /// The waker for the device in `slot`.
    pub fn waker(&self, slot: usize) -> Arc<dyn HostWaker> {
        Arc::new(SlotWaker {
            shared: Arc::clone(&self.shared),
            slot,
        })
    }

    /// Hands the service its transports (slot order) and starts the thread.
    ///
    /// A thread that cannot be spawned is logged, not fatal: the devices keep
    /// their wakers, and whatever they asked for is served at the next guest
    /// kick instead — slower, never wrong (a waker is a request for service,
    /// not a completion).
    pub fn start(&self, transports: Vec<Arc<Mutex<T>>>) {
        *self
            .transports
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = transports;
        self.spawn();
    }

    fn spawn(&self) {
        if self.torn_down.load(Ordering::Acquire) {
            return;
        }
        let transports = self
            .transports
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        if transports.is_empty() {
            return;
        }
        self.shared.stopping.store(false, Ordering::Release);
        let shared = Arc::clone(&self.shared);
        let quiesce = Arc::clone(&self.quiesce);
        let spawned = std::thread::Builder::new()
            .name(format!("{HOST_WAKE_THREAD_PREFIX}-{}", T::transport_name()))
            .spawn(move || service_loop(&shared, &transports, &quiesce));
        match spawned {
            Ok(handle) => {
                *self.worker.lock().unwrap_or_else(PoisonError::into_inner) = Some(handle);
            }
            Err(error) => tracing::error!(
                transport = T::transport_name(),
                %error,
                "could not start the host-wake thread; host wakeups wait for the next guest kick"
            ),
        }
    }

    /// Stops and joins the thread, if one runs. Safe on a paused VM: the
    /// thread can leave the gate as well as its condvar.
    fn stop(&self) {
        self.shared.stopping.store(true, Ordering::Release);
        self.shared.signal_thread();
        self.quiesce_gate().wake();
        let handle = self
            .worker
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        if let Some(handle) = handle {
            if handle.join().is_err() {
                // A device panic on a guest-controlled path is a bug; report
                // it, never unwind through the machine's reset or teardown.
                tracing::error!(
                    transport = T::transport_name(),
                    "the host-wake thread panicked"
                );
            }
        }
    }

    /// Machine reset (ADR-0005): stop and join the thread, drop the wakes of
    /// the boot being reset, start a fresh thread. Called before the
    /// transports are reset, on a quiesced VM, holding no device lock.
    pub fn reset(&self) {
        if self.torn_down.load(Ordering::Acquire) {
            return;
        }
        self.stop();
        self.shared.clear();
        self.spawn();
    }

    /// Stops and joins the thread for good. Idempotent; also run from `Drop`.
    pub fn shutdown(&self) {
        if self.torn_down.swap(true, Ordering::AcqRel) {
            return;
        }
        self.stop();
        // The transports go too: a stopped bus must be the only owner of its
        // devices, or dropping it would not drop them.
        self.transports
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clear();
    }

    /// Whether the thread is running (tests, diagnostics).
    pub fn is_running(&self) -> bool {
        self.worker
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .as_ref()
            .is_some_and(|handle| !handle.is_finished())
    }

    /// Shares the VM's pause gate with the thread (ADR-0005).
    pub fn set_quiesce(&self, quiesce: Arc<Quiesce>) {
        *self.quiesce.lock().unwrap_or_else(PoisonError::into_inner) = quiesce;
    }

    fn quiesce_gate(&self) -> Arc<Quiesce> {
        Arc::clone(&self.quiesce.lock().unwrap_or_else(PoisonError::into_inner))
    }
}

impl<T: WakeTarget> Drop for HostWakeService<T> {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// The thread: wait for a pending slot, serve each pending slot once, repeat.
///
/// Never panics on a guest-controlled condition: a malformed request is the
/// device's business, and a device that fails makes its transport set
/// `DEVICE_NEEDS_RESET` — the thread keeps serving the other slots.
fn service_loop<T: WakeTarget>(
    shared: &Shared,
    transports: &[Arc<Mutex<T>>],
    quiesce: &Mutex<Arc<Quiesce>>,
) {
    let keep_going = || !shared.stopping.load(Ordering::Acquire);
    loop {
        {
            let mut signal = shared.signal.lock().unwrap_or_else(PoisonError::into_inner);
            while !*signal && keep_going() {
                signal = shared
                    .changed
                    .wait(signal)
                    .unwrap_or_else(PoisonError::into_inner);
            }
            if !keep_going() {
                return;
            }
            *signal = false;
        }
        for (slot, transport) in transports.iter().enumerate() {
            let Some(bit) = shared.pending.get(slot) else {
                continue;
            };
            // Cleared before the notify, so a wake during it is served by the
            // next pass rather than lost.
            if !bit.swap(false, Ordering::AcqRel) {
                continue;
            }
            // The pause gate, before the transport lock (ADR-0005): parking
            // with the lock held would deadlock the reset that runs on a
            // quiesced VM.
            let gate = Arc::clone(&quiesce.lock().unwrap_or_else(PoisonError::into_inner));
            let Some(_pass) = gate.wait_while_paused(keep_going) else {
                return;
            };
            match transport.lock() {
                Ok(mut t) if t.is_activated() => t.queue_notify(0),
                // A wake that raced a device reset: nothing is there to serve,
                // and the next driver's kicks start from a clean ring.
                Ok(_) => tracing::trace!(
                    transport = T::transport_name(),
                    slot,
                    "host wake for a device with no driver; ignored"
                ),
                Err(_) => tracing::error!(
                    transport = T::transport_name(),
                    slot,
                    "transport lock is poisoned; dropping a host wake"
                ),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;
    use std::time::{Duration, Instant};

    /// A transport stand-in: counts notifies, and can hold one open until
    /// released so a burst can be aimed at a notify in progress.
    #[derive(Default)]
    struct Counter {
        notifies: Arc<AtomicUsize>,
        inactive: bool,
        hold: Option<Arc<(Mutex<bool>, Condvar)>>,
        threads: Arc<Mutex<Vec<String>>>,
    }

    impl WakeTarget for Counter {
        fn queue_notify(&mut self, value: u32) {
            assert_eq!(value, 0, "a host wake is a queue-0 notify");
            self.threads.lock().unwrap().push(
                std::thread::current()
                    .name()
                    .unwrap_or_default()
                    .to_string(),
            );
            if let Some(hold) = &self.hold {
                let (lock, cv) = &**hold;
                let mut held = lock.lock().unwrap();
                while *held {
                    held = cv.wait(held).unwrap();
                }
            }
            self.notifies.fetch_add(1, Ordering::SeqCst);
        }
        fn is_activated(&self) -> bool {
            !self.inactive
        }
        fn transport_name() -> &'static str {
            "test"
        }
    }

    fn until(what: &str, within: Duration, done: impl Fn() -> bool) {
        let deadline = Instant::now() + within;
        while !done() {
            assert!(Instant::now() < deadline, "timed out waiting for {what}");
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    fn service_with(counters: Vec<Counter>) -> HostWakeService<Counter> {
        let service = HostWakeService::new(counters.len());
        service.start(
            counters
                .into_iter()
                .map(|c| Arc::new(Mutex::new(c)))
                .collect(),
        );
        service
    }

    #[test]
    fn a_wake_from_a_foreign_thread_is_a_queue_0_notify_on_the_service_thread() {
        let counter = Counter::default();
        let (notifies, threads) = (Arc::clone(&counter.notifies), Arc::clone(&counter.threads));
        let service = service_with(vec![counter]);
        let waker = service.waker(0);
        std::thread::spawn(move || waker.wake()).join().unwrap();
        until("the notify", Duration::from_secs(5), || {
            notifies.load(Ordering::SeqCst) == 1
        });
        assert_eq!(
            threads.lock().unwrap().as_slice(),
            [format!("{HOST_WAKE_THREAD_PREFIX}-test")]
        );
    }

    #[test]
    fn a_wake_reaches_only_its_own_slot() {
        let (a, b) = (Counter::default(), Counter::default());
        let (na, nb) = (Arc::clone(&a.notifies), Arc::clone(&b.notifies));
        let service = service_with(vec![a, b]);
        service.waker(1).wake();
        until("slot 1", Duration::from_secs(5), || {
            nb.load(Ordering::SeqCst) == 1
        });
        std::thread::sleep(Duration::from_millis(30));
        assert_eq!(na.load(Ordering::SeqCst), 0);
        // A slot the bus does not have is ignored rather than trusted.
        service.waker(7).wake();
    }

    /// A burst of wakes while the device is busy is one more notify, not one
    /// per wake: the drain that follows serves everything the burst was for.
    #[test]
    fn a_burst_of_wakes_coalesces_into_one_notify() {
        let hold = Arc::new((Mutex::new(true), Condvar::new()));
        let counter = Counter {
            hold: Some(Arc::clone(&hold)),
            ..Counter::default()
        };
        let (notifies, threads) = (Arc::clone(&counter.notifies), Arc::clone(&counter.threads));
        let service = service_with(vec![counter]);
        let waker = service.waker(0);
        waker.wake();
        until(
            "the first notify to be under way",
            Duration::from_secs(5),
            || threads.lock().unwrap().len() == 1,
        );
        for _ in 0..1000 {
            waker.wake();
        }
        {
            let (lock, cv) = &*hold;
            *lock.lock().unwrap() = false;
            cv.notify_all();
        }
        until("the coalesced notify", Duration::from_secs(5), || {
            notifies.load(Ordering::SeqCst) == 2
        });
        std::thread::sleep(Duration::from_millis(50));
        assert_eq!(notifies.load(Ordering::SeqCst), 2, "1000 wakes, one notify");
    }

    #[test]
    fn a_wake_before_start_is_served_once_the_thread_runs() {
        let counter = Counter::default();
        let notifies = Arc::clone(&counter.notifies);
        let service = HostWakeService::new(1);
        let waker = service.waker(0);
        waker.wake();
        waker.wake();
        std::thread::sleep(Duration::from_millis(20));
        assert_eq!(notifies.load(Ordering::SeqCst), 0);
        service.start(vec![Arc::new(Mutex::new(counter))]);
        until("the notify", Duration::from_secs(5), || {
            notifies.load(Ordering::SeqCst) == 1
        });
    }

    /// ADR-0005: a paused VM gets no notify, the pause settles with the
    /// thread parked, and the wake is served on resume.
    #[test]
    fn a_paused_vm_gets_no_notify_until_resume() {
        let counter = Counter::default();
        let notifies = Arc::clone(&counter.notifies);
        let service = service_with(vec![counter]);
        let gate = Quiesce::new();
        service.set_quiesce(Arc::clone(&gate));
        gate.pause();
        assert!(gate.wait_until_idle(Duration::from_secs(1)));
        service.waker(0).wake();
        std::thread::sleep(Duration::from_millis(100));
        assert_eq!(notifies.load(Ordering::SeqCst), 0, "notified while paused");
        assert!(
            gate.wait_until_idle(Duration::from_millis(100)),
            "the parked thread holds no pass"
        );
        gate.resume();
        until("the notify after resume", Duration::from_secs(5), || {
            notifies.load(Ordering::SeqCst) == 1
        });
    }

    /// A reset runs on a paused VM: it must join a thread parked on the gate,
    /// drop the old boot's wakes, and leave a running thread behind.
    #[test]
    fn a_reset_on_a_paused_vm_joins_drops_old_wakes_and_restarts() {
        let counter = Counter::default();
        let notifies = Arc::clone(&counter.notifies);
        let service = service_with(vec![counter]);
        let gate = Quiesce::new();
        service.set_quiesce(Arc::clone(&gate));
        gate.pause();
        service.waker(0).wake();
        std::thread::sleep(Duration::from_millis(30));
        let started = Instant::now();
        service.reset();
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "the reset joined"
        );
        assert!(service.is_running(), "a fresh thread after the reset");
        gate.resume();
        std::thread::sleep(Duration::from_millis(50));
        assert_eq!(
            notifies.load(Ordering::SeqCst),
            0,
            "the old boot's wake was dropped"
        );
        service.waker(0).wake();
        until("a new boot's wake", Duration::from_secs(5), || {
            notifies.load(Ordering::SeqCst) == 1
        });
    }

    #[test]
    fn shutdown_joins_even_a_parked_thread_and_is_idempotent() {
        let service = service_with(vec![Counter::default()]);
        let gate = Quiesce::new();
        service.set_quiesce(Arc::clone(&gate));
        gate.pause();
        service.waker(0).wake();
        std::thread::sleep(Duration::from_millis(30));
        let started = Instant::now();
        service.shutdown();
        assert!(started.elapsed() < Duration::from_secs(1));
        assert!(!service.is_running());
        service.shutdown();
        service.reset();
        assert!(
            !service.is_running(),
            "a reset after shutdown starts nothing"
        );
        // A waker outliving the service is inert, never a panic.
        service.waker(0).wake();
    }

    #[test]
    fn a_device_with_no_driver_is_not_notified() {
        let counter = Counter {
            inactive: true,
            ..Counter::default()
        };
        let (notifies, threads) = (Arc::clone(&counter.notifies), Arc::clone(&counter.threads));
        let service = service_with(vec![counter]);
        service.waker(0).wake();
        std::thread::sleep(Duration::from_millis(50));
        assert_eq!(notifies.load(Ordering::SeqCst), 0);
        assert!(threads.lock().unwrap().is_empty());
    }
}
