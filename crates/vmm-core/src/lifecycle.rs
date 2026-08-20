//! Pause, resume and reset of a *running* VM — the controlled-stop seam
//! ([ADR-0005](../../../../docs/adr/0005-vm-lifecycle.md)).
//!
//! Everything in here is portable: it is a rendezvous protocol between the
//! thread that asks for something and the vCPU threads that have to be
//! somewhere safe before it can happen. KVM and WHP plug into it through two
//! small traits — [`VcpuKick`] (get a vCPU out of the hypervisor's run call)
//! and [`ResettableVcpu`] (put one back into its power-on state) — and neither
//! backend's public API changes shape for the other (ADR-0002).
//!
//! # The stop protocol
//!
//! ```text
//!   requester                          vCPU thread i
//!   ---------                          -------------
//!   phase := Park{Pause|Reset}         .. inside KVM_RUN / WHvRunVirtualProcessor
//!   attention := true
//!   kick(i) for all i, repeatedly  ->  run call returns Interrupted/Canceled
//!                                      loop top: attention()? flush pending exit
//!                                      checkpoint()
//!                                        parked += 1; notify
//!                                        wait until phase == Run | Stop
//!   wait until parked == live vCPUs
//!   -- every vCPU is now OUT of the guest, between two exits --
//!   quiesce()                          (host device workers stop as well)
//!   ... hold (Pause) ...
//!   ... or: reset_machine()            devices + guest memory, once
//!       phase := ResetVcpus        ->  reset_arch_state(); reset_vcpu(i)
//!   wait until every vCPU reported
//!   unquiesce(); phase := Run      ->  back into the guest
//! ```
//!
//! The checkpoint sits at the **top of the run loop**, after the previous exit
//! has been fully dispatched and before the next entry into the hypervisor.
//! That is what makes the stop point safe: no device is half-way through
//! serving an MMIO access, no descriptor chain is half-walked, and the only
//! hypervisor-side leftover — KVM's pending userspace-I/O completion — is
//! flushed explicitly before the vCPU parks. That flush is the *backend's* job
//! rather than this module's, because retiring it can produce one more exit to
//! dispatch and the `ExitHandler` lives in the run loop; [`Lifecycle::attention`]
//! is what tells the loop a park is imminent and the flush is worth doing.
//!
//! # Why the requester coordinates, not a leader vCPU
//!
//! The machine-wide half of a reset (devices back to power-on, boot images
//! reloaded) runs on the thread that asked for it, while every vCPU waits.
//! The per-vCPU half runs on each vCPU's own thread, because KVM requires vCPU
//! ioctls to come from the thread that owns the vCPU fd. A guest-initiated
//! reboot is therefore *latched* by the vCPU that saw it
//! ([`Lifecycle::request_guest_reset`]) and *driven* by the supervisor, which
//! is the same thread that drives a reset asked for from the window.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use crate::hv::{HvError, VcpuRegisters};

/// How long a lifecycle operation waits for the vCPUs to acknowledge before it
/// gives up and puts the VM back the way it found it.
///
/// Generous: a vCPU only has to reach the top of its run loop, but on WHP a
/// halted one may be sitting out the rest of a `HALT_POLL` window, and on a
/// loaded host a debug-build device handler can take a while to return.
pub const ACK_TIMEOUT: Duration = Duration::from_secs(10);

/// How often the requester re-kicks vCPUs that have not parked yet.
///
/// Kicking once is not enough: a vCPU that checked the attention flag just
/// before entering the hypervisor swallows the kick and stays in the guest
/// until its next exit. The same repeated-kick rule the stop path has always
/// used.
const KICK_INTERVAL: Duration = Duration::from_millis(10);

/// A guest reset that arrives within this long of the previous one having
/// finished counts as "the guest did not get anywhere".
///
/// The guest is untrusted, and a firmware or kernel that faults immediately
/// after reset would otherwise have the host rebooting it forever at whatever
/// rate the reset takes. Nothing here is a *fast* path, so the window is
/// generous: a real boot reaches its first device access in milliseconds, but
/// nothing legitimate reboots twice inside a second.
const RESET_STORM_WINDOW: Duration = Duration::from_secs(1);

/// How many back-to-back resets inside [`RESET_STORM_WINDOW`] are tolerated
/// before the VM is stopped instead of rebooted again.
const MAX_RESET_STORM: u32 = 5;

#[derive(Debug, thiserror::Error)]
pub enum LifecycleError {
    #[error("timed out after {waited:?} waiting for the vCPUs to {what} ({done} of {expected})")]
    Timeout {
        what: &'static str,
        done: u32,
        expected: u32,
        waited: Duration,
    },

    #[error("cannot {what}: the VM is {state:?}")]
    WrongState { what: &'static str, state: RunState },

    #[error("no machine is attached to this lifecycle, so it cannot be reset")]
    NoMachine,

    #[error("machine reset failed: {0}")]
    Reset(String),

    #[error("the VM is stopping")]
    Stopping,
}

/// Where a running VM's vCPUs are, as the host sees it.
///
/// Mirrors — and is mapped onto — [`crate::VmState`] by the supervisor:
/// `Running`/`Pausing`/`Resetting` all live inside `VmState::Running`'s
/// lifetime, and `Paused` is `VmState::Paused`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunState {
    /// vCPUs are executing guest code.
    Running,
    /// A pause has been requested; not every vCPU has parked yet.
    Pausing,
    /// Every vCPU is parked and host device workers are quiesced.
    Paused,
    /// A reset is in progress.
    Resetting,
    /// The VM is being torn down; parked vCPUs are released to exit.
    Stopping,
}

/// What a vCPU run loop must do after a [`Lifecycle::checkpoint`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Checkpoint {
    /// Re-enter the guest.
    Continue,
    /// Leave the run loop with [`crate::RunOutcome::Stopped`].
    Stopped,
}

/// Kicks one vCPU out of its hypervisor run call, from another thread.
///
/// KVM sends the vCPU thread an RT signal (`KVM_RUN` returns `EINTR`); WHP
/// calls `WHvCancelRunVirtualProcessor` and bumps the halt gate. Both are
/// non-blocking and callable from any thread; a kick that arrives while the
/// vCPU is not in the run call is harmless.
pub trait VcpuKick: Send + Sync {
    fn kick(&self);
}

/// The vCPU-side capabilities a lifecycle checkpoint needs, on top of plain
/// register access.
///
/// Implemented by `crate::Vcpu` (KVM) and `crate::whp::WhpVcpu`, and used only
/// from the vCPU's own thread — which is what KVM requires of vCPU ioctls, and
/// the reason the per-vCPU half of a reset cannot run on the requester.
pub trait ResettableVcpu: VcpuRegisters {
    /// Puts this vCPU back into the architectural power-on state: registers,
    /// local APIC, and (for an application processor) back to waiting for the
    /// boot CPU's INIT/SIPI.
    ///
    /// Runs *before* the machine's own boot-state setup, so a stale `CR4.LA57`
    /// or an enabled APIC timer from the previous boot cannot survive into the
    /// next one.
    fn reset_arch_state(&mut self, is_boot_cpu: bool) -> Result<(), HvError>;
}

/// What the machine above the hypervisor contributes to a pause or a reset.
///
/// Implemented once, by whoever assembled the VM (`entangled run`, a test
/// harness). Everything here is called with **every vCPU parked**, so an
/// implementation may touch guest memory and device state freely.
pub trait MachineLifecycle: Send + Sync {
    /// Stop host-side device workers from touching guest memory: the
    /// ioeventfd queue workers, the virtio-net receive thread, the PIT's timer.
    /// Called once, after the vCPUs have parked.
    fn quiesce(&self) {}

    /// Let them run again. Called before the vCPUs are released.
    fn unquiesce(&self) {}

    /// Return every device to its power-on state and put the boot images back
    /// into guest memory. Called once, on the requesting thread.
    fn reset_machine(&self) -> Result<(), String>;

    /// Put vCPU `index` into the state it had at the first instruction of the
    /// previous boot. Called on that vCPU's own thread, after
    /// [`ResettableVcpu::reset_arch_state`].
    fn reset_vcpu(&self, index: u32, vcpu: &dyn VcpuRegisters) -> Result<(), String>;
}

/// Internal phase of the rendezvous. `RunState` is the public projection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    Run,
    ParkPause,
    HeldPause,
    ParkReset,
    ResetMachine,
    ResetVcpus,
    Stop,
}

impl Phase {
    fn state(self) -> RunState {
        match self {
            Phase::Run => RunState::Running,
            Phase::ParkPause => RunState::Pausing,
            Phase::HeldPause => RunState::Paused,
            Phase::ParkReset | Phase::ResetMachine | Phase::ResetVcpus => RunState::Resetting,
            Phase::Stop => RunState::Stopping,
        }
    }

    /// True while vCPUs must stay at the barrier.
    fn parks_vcpus(self) -> bool {
        matches!(
            self,
            Phase::ParkPause | Phase::HeldPause | Phase::ParkReset | Phase::ResetMachine
        )
    }
}

#[derive(Debug)]
struct Inner {
    phase: Phase,
    /// One slot per vCPU: has it arrived at the current barrier?
    arrived: Vec<bool>,
    /// One slot per vCPU: has it finished its own half of the current reset?
    reset_done: Vec<bool>,
    /// One slot per vCPU: has its run loop ended for good? A finished vCPU can
    /// never park again, so it is excluded from every barrier — otherwise a
    /// guest that halted one CPU would make pause time out.
    finished: Vec<bool>,
    /// The first error any vCPU hit while resetting itself.
    reset_error: Option<String>,
    /// A guest asked to reboot (reset register, 0xCF9, the keyboard controller
    /// pulse, or a triple fault). Latched by whichever vCPU noticed, consumed
    /// by the supervisor.
    guest_reset: bool,
    /// When the last in-place reset finished, and how many resets in a row have
    /// been asked for within [`RESET_STORM_WINDOW`] of one — the bound that
    /// keeps an untrusted guest from turning "reboot" into an unbounded loop.
    last_reset: Option<Instant>,
    storm: u32,
}

impl Inner {
    fn count(&self, flags: &[bool]) -> u32 {
        flags
            .iter()
            .zip(&self.finished)
            .filter(|(&flag, &done)| flag || done)
            .count() as u32
    }

    fn arrived_count(&self) -> u32 {
        self.count(&self.arrived)
    }

    fn reset_count(&self) -> u32 {
        self.count(&self.reset_done)
    }

    fn clear_barrier(&mut self) {
        self.arrived.fill(false);
        self.reset_done.fill(false);
    }
}

/// The lifecycle control seam of one VM.
///
/// Created before the vCPU threads, shared with every one of them, and held by
/// whatever drives the VM (the supervisor thread, the window, a test).
pub struct Lifecycle {
    vcpus: u32,
    /// Fast path: the run loop reads exactly this on every iteration and takes
    /// no lock unless something is pending.
    attention: AtomicBool,
    inner: Mutex<Inner>,
    changed: Condvar,
    kickers: Mutex<Vec<Arc<dyn VcpuKick>>>,
    machine: Mutex<Option<Arc<dyn MachineLifecycle>>>,
    resets: AtomicU64,
}

impl std::fmt::Debug for Lifecycle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Lifecycle")
            .field("vcpus", &self.vcpus)
            .field("state", &self.state())
            .field("resets", &self.resets())
            .finish()
    }
}

impl Lifecycle {
    /// A lifecycle for a VM with `vcpus` virtual CPUs, running.
    pub fn new(vcpus: u32) -> Arc<Self> {
        let n = vcpus.max(1) as usize;
        Arc::new(Self {
            vcpus: vcpus.max(1),
            attention: AtomicBool::new(false),
            inner: Mutex::new(Inner {
                phase: Phase::Run,
                arrived: vec![false; n],
                reset_done: vec![false; n],
                finished: vec![false; n],
                reset_error: None,
                guest_reset: false,
                last_reset: None,
                storm: 0,
            }),
            changed: Condvar::new(),
            kickers: Mutex::new(Vec::new()),
            machine: Mutex::new(None),
            resets: AtomicU64::new(0),
        })
    }

    /// Attaches the machine that knows how to quiesce and reset itself.
    /// Without one, [`Self::reset`] fails with [`LifecycleError::NoMachine`]
    /// instead of half-resetting a VM.
    pub fn attach_machine(&self, machine: Arc<dyn MachineLifecycle>) {
        *self.lock_machine() = Some(machine);
    }

    /// Registers one vCPU's kick handle. Called by the backend's `spawn_vcpus`.
    pub fn register_kicker(&self, kicker: Arc<dyn VcpuKick>) {
        self.lock_kickers().push(kicker);
    }

    pub fn vcpu_count(&self) -> u32 {
        self.vcpus
    }

    /// How many times this VM has been reset in place.
    pub fn resets(&self) -> u64 {
        self.resets.load(Ordering::Acquire)
    }

    pub fn state(&self) -> RunState {
        self.lock().phase.state()
    }

    pub fn is_paused(&self) -> bool {
        matches!(self.state(), RunState::Paused | RunState::Pausing)
    }

    /// The run loop's fast-path question: is anything pending at all?
    ///
    /// One acquire load. A backend checks this at the top of its loop and, only
    /// when it is true, retires whatever the hypervisor still holds from the
    /// last exit before calling [`Self::checkpoint`] — which is where the
    /// `ExitHandler` a flush may need lives, and why the flush is the run
    /// loop's job rather than this module's.
    pub fn attention(&self) -> bool {
        self.attention.load(Ordering::Acquire)
    }

    // ------------------------------------------------------------ requester

    /// Brings every vCPU to a safe stop and holds it there.
    ///
    /// Returns once every live vCPU has acknowledged and the machine has been
    /// quiesced. Idempotent: pausing an already-paused VM succeeds.
    pub fn pause(&self) -> Result<(), LifecycleError> {
        {
            let mut inner = self.lock();
            match inner.phase {
                Phase::HeldPause | Phase::ParkPause => return Ok(()),
                Phase::Stop => return Err(LifecycleError::Stopping),
                Phase::Run => {}
                other => {
                    return Err(LifecycleError::WrongState {
                        what: "pause",
                        state: other.state(),
                    })
                }
            }
            inner.clear_barrier();
            inner.phase = Phase::ParkPause;
            self.attention.store(true, Ordering::Release);
        }
        self.changed.notify_all();
        self.await_barrier("park", Inner::arrived_count)
            .inspect_err(|_| self.abandon())?;

        if let Some(machine) = self.machine() {
            machine.quiesce();
        }
        let mut inner = self.lock();
        if inner.phase == Phase::ParkPause {
            inner.phase = Phase::HeldPause;
        }
        drop(inner);
        self.changed.notify_all();
        tracing::info!(vcpus = self.vcpus, "VM paused");
        Ok(())
    }

    /// Releases a paused VM. Idempotent: resuming a running VM succeeds.
    pub fn resume(&self) -> Result<(), LifecycleError> {
        {
            let inner = self.lock();
            match inner.phase {
                Phase::Run => return Ok(()),
                Phase::Stop => return Err(LifecycleError::Stopping),
                Phase::HeldPause | Phase::ParkPause => {}
                other => {
                    return Err(LifecycleError::WrongState {
                        what: "resume",
                        state: other.state(),
                    })
                }
            }
        }
        if let Some(machine) = self.machine() {
            machine.unquiesce();
        }
        self.release();
        tracing::info!("VM resumed");
        Ok(())
    }

    /// Resets the VM in place: every vCPU parks, the machine returns to its
    /// power-on state, the boot images go back into guest memory, every vCPU is
    /// put back at the first instruction — and the VM runs again.
    ///
    /// Callable while running or while paused; either way the VM is **running**
    /// afterwards, because a reboot that left the VM frozen would be a
    /// surprising thing for a guest's own `reboot` to do.
    pub fn reset(&self) -> Result<(), LifecycleError> {
        let machine = self.machine().ok_or(LifecycleError::NoMachine)?;
        {
            let mut inner = self.lock();
            match inner.phase {
                Phase::Run | Phase::HeldPause | Phase::ParkPause => {}
                Phase::Stop => return Err(LifecycleError::Stopping),
                other => {
                    return Err(LifecycleError::WrongState {
                        what: "reset",
                        state: other.state(),
                    })
                }
            }
            // A paused VM is already at the barrier; the phase change alone
            // moves it on to the reset barrier, and `arrived` stays valid
            // because both barriers count the same arrival.
            let was_parked = inner.phase.parks_vcpus();
            if !was_parked {
                inner.clear_barrier();
            } else {
                inner.reset_done.fill(false);
            }
            inner.reset_error = None;
            inner.guest_reset = false;
            inner.phase = Phase::ParkReset;
            self.attention.store(true, Ordering::Release);
        }
        self.changed.notify_all();
        self.await_barrier("park for reset", Inner::arrived_count)
            .inspect_err(|_| self.abandon())?;

        machine.quiesce();
        self.set_phase(Phase::ResetMachine);
        let machine_result = machine.reset_machine();
        if let Err(error) = machine_result {
            // Nothing has been resumed and the vCPUs are still parked; leaving
            // them there with the VM in `Stopping` is the honest outcome —
            // re-entering a half-reset machine would be worse.
            self.set_phase(Phase::Stop);
            self.attention.store(true, Ordering::Release);
            self.changed.notify_all();
            return Err(LifecycleError::Reset(error));
        }

        self.set_phase(Phase::ResetVcpus);
        let vcpu_result = self
            .await_barrier("reset themselves", Inner::reset_count)
            .inspect_err(|_| self.abandon());

        let error = {
            let mut inner = self.lock();
            // Consume the latch *here*, not only on the way in. On an SMP guest
            // every vCPU polls the reset registers after its own next exit, so
            // the second one latches a request for the reboot that is already
            // under way — after this call cleared it at the start. Left set, it
            // would be served as a second, spurious reboot the moment the guest
            // came back, and (being inside the storm window) would count
            // against it. Safe to clear now: every vCPU has been parked since
            // the barrier and none of them can have seen anything since.
            inner.guest_reset = false;
            inner.reset_error.clone()
        };
        machine.unquiesce();
        self.release();
        vcpu_result?;
        if let Some(error) = error {
            return Err(LifecycleError::Reset(error));
        }
        self.lock().last_reset = Some(Instant::now());
        let count = self.resets.fetch_add(1, Ordering::AcqRel) + 1;
        tracing::info!(resets = count, "VM reset complete; guest restarted");
        Ok(())
    }

    /// Releases every parked vCPU and marks the VM as stopping, so a paused VM
    /// can still be torn down. Called by the backends' `stop()`.
    pub fn shutdown(&self) {
        self.set_phase(Phase::Stop);
        self.attention.store(true, Ordering::Release);
        self.changed.notify_all();
        if let Some(machine) = self.machine() {
            machine.unquiesce();
        }
    }

    /// Whether a reset would have anything to reset: a machine must be attached
    /// (`entangled run` attaches one; a bare `spawn_vcpus` test does not, and
    /// keeps the old "a triple fault ends the VM" behaviour).
    pub fn can_reset(&self) -> bool {
        self.lock_machine().is_some()
    }

    /// Latches "the guest asked to reboot". Non-blocking and callable from a
    /// vCPU thread or a device: the supervisor turns it into a [`Self::reset`].
    ///
    /// Returns **false** when the guest is resetting faster than it can boot —
    /// see the reset-storm window below. The caller then ends the VM instead, which is
    /// the only bounded answer to a guest that faults its way straight back into
    /// the reset it just came out of.
    #[must_use]
    pub fn request_guest_reset(&self) -> bool {
        let mut inner = self.lock();
        let storming = inner
            .last_reset
            .is_some_and(|at| at.elapsed() < RESET_STORM_WINDOW);
        if storming {
            inner.storm += 1;
            if inner.storm > MAX_RESET_STORM {
                tracing::error!(
                    resets = inner.storm,
                    window = ?RESET_STORM_WINDOW,
                    "the guest is resetting faster than it can boot; stopping the VM instead"
                );
                return false;
            }
        } else {
            inner.storm = 0;
        }
        if !inner.guest_reset {
            inner.guest_reset = true;
            tracing::info!("guest requested a reset; the supervisor will restart the machine");
        }
        true
    }

    /// Consumes a pending guest reset request, if any.
    pub fn take_guest_reset(&self) -> bool {
        let mut inner = self.lock();
        std::mem::replace(&mut inner.guest_reset, false)
    }

    // ---------------------------------------------------------- vCPU thread

    /// The safe stop point, called at the top of a run loop.
    ///
    /// Cheap when nothing is pending: one relaxed-ordering atomic read.
    pub fn checkpoint(&self, index: u32, vcpu: &mut dyn ResettableVcpu) -> Checkpoint {
        if !self.attention.load(Ordering::Acquire) {
            return Checkpoint::Continue;
        }
        let slot = index as usize;
        let mut inner = self.lock();
        loop {
            match inner.phase {
                Phase::Run => return Checkpoint::Continue,
                Phase::Stop => return Checkpoint::Stopped,
                Phase::ParkPause | Phase::ParkReset => {
                    if !inner.arrived.get(slot).copied().unwrap_or(true) {
                        if let Some(flag) = inner.arrived.get_mut(slot) {
                            *flag = true;
                        }
                        self.changed.notify_all();
                    }
                    inner = self.wait(inner);
                }
                Phase::HeldPause | Phase::ResetMachine => inner = self.wait(inner),
                Phase::ResetVcpus => {
                    if !inner.reset_done.get(slot).copied().unwrap_or(true) {
                        let machine = self.machine();
                        drop(inner);
                        let outcome = self.reset_one(index, vcpu, machine.as_deref());
                        inner = self.lock();
                        if let Some(error) = outcome.err() {
                            inner.reset_error.get_or_insert(error);
                        }
                        if let Some(flag) = inner.reset_done.get_mut(slot) {
                            *flag = true;
                        }
                        self.changed.notify_all();
                    }
                    inner = self.wait(inner);
                }
            }
        }
    }

    /// Blocks until something is pending, or `timeout` expires. Returns whether
    /// there is.
    ///
    /// For the one vCPU state that must never re-enter the guest: a vCPU that
    /// has just triple-faulted, or one whose guest has asked to reboot and is
    /// now spinning in its own dead loop. Both have to *wait* for the reset the
    /// supervisor is about to run rather than spin, and the supervisor is on
    /// another thread.
    pub fn wait_for_attention(&self, timeout: Duration) -> bool {
        if self.attention() {
            return true;
        }
        let inner = self.lock();
        let _unused = self
            .changed
            .wait_timeout(inner, timeout)
            .map(|(guard, _)| guard)
            .unwrap_or_else(|poisoned| poisoned.into_inner().0);
        self.attention()
    }

    /// Marks a vCPU's run loop as ended, so no later barrier waits for it.
    pub fn vcpu_finished(&self, index: u32) {
        {
            let mut inner = self.lock();
            if let Some(flag) = inner.finished.get_mut(index as usize) {
                *flag = true;
            }
        }
        self.changed.notify_all();
    }

    fn reset_one(
        &self,
        index: u32,
        vcpu: &mut dyn ResettableVcpu,
        machine: Option<&dyn MachineLifecycle>,
    ) -> Result<(), String> {
        vcpu.reset_arch_state(index == 0)
            .map_err(|e| format!("vCPU {index}: {e}"))?;
        match machine {
            Some(machine) => machine.reset_vcpu(index, vcpu),
            None => Ok(()),
        }
    }

    // ---------------------------------------------------------------- plumbing

    fn lock(&self) -> MutexGuard<'_, Inner> {
        // A poisoned lifecycle means a vCPU thread panicked while holding it.
        // The state is still a structurally valid set of counters, and refusing
        // to pause a VM because of it would be strictly worse than carrying on.
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn lock_kickers(&self) -> MutexGuard<'_, Vec<Arc<dyn VcpuKick>>> {
        self.kickers.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn lock_machine(&self) -> MutexGuard<'_, Option<Arc<dyn MachineLifecycle>>> {
        self.machine.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn machine(&self) -> Option<Arc<dyn MachineLifecycle>> {
        self.lock_machine().clone()
    }

    fn wait<'a>(&'a self, inner: MutexGuard<'a, Inner>) -> MutexGuard<'a, Inner> {
        self.changed
            .wait_timeout(inner, KICK_INTERVAL)
            .map(|(guard, _)| guard)
            .unwrap_or_else(|poisoned| poisoned.into_inner().0)
    }

    fn set_phase(&self, phase: Phase) {
        self.lock().phase = phase;
        self.changed.notify_all();
    }

    /// Puts the VM back into `Run` and lets everyone out of the barrier.
    fn release(&self) {
        {
            let mut inner = self.lock();
            if inner.phase != Phase::Stop {
                inner.phase = Phase::Run;
            }
            inner.clear_barrier();
        }
        self.attention.store(false, Ordering::Release);
        self.changed.notify_all();
    }

    /// A barrier timed out. Put the VM back where it was rather than leaving
    /// half the vCPUs parked forever.
    fn abandon(&self) {
        tracing::error!(
            "a vCPU did not reach the lifecycle checkpoint in {ACK_TIMEOUT:?}; \
             letting the VM run on"
        );
        if let Some(machine) = self.machine() {
            machine.unquiesce();
        }
        self.release();
    }

    fn kick_all(&self) {
        for kicker in self.lock_kickers().iter() {
            kicker.kick();
        }
    }

    /// Waits until `count` reaches the number of live vCPUs, re-kicking every
    /// [`KICK_INTERVAL`] because a kick can be swallowed by a vCPU that was
    /// about to enter the hypervisor.
    fn await_barrier(
        &self,
        what: &'static str,
        count: fn(&Inner) -> u32,
    ) -> Result<(), LifecycleError> {
        let started = Instant::now();
        loop {
            let (done, expected) = {
                let inner = self.lock();
                if inner.phase == Phase::Stop {
                    return Err(LifecycleError::Stopping);
                }
                (count(&inner), self.vcpus)
            };
            if done >= expected {
                return Ok(());
            }
            let waited = started.elapsed();
            if waited >= ACK_TIMEOUT {
                return Err(LifecycleError::Timeout {
                    what,
                    done,
                    expected,
                    waited,
                });
            }
            self.kick_all();
            let inner = self.lock();
            let _unused = self
                .changed
                .wait_timeout(inner, KICK_INTERVAL)
                .map(|(guard, _)| guard)
                .unwrap_or_else(|poisoned| poisoned.into_inner().0);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hv::{X86Registers, X86SpecialRegisters};
    use std::sync::atomic::AtomicU32;

    /// A vCPU stand-in: counts guest "instructions" so a test can prove a
    /// paused VM stops making progress, and records its resets.
    struct FakeVcpu {
        index: u32,
        ticks: Arc<AtomicU64>,
        arch_resets: Arc<AtomicU32>,
    }

    impl VcpuRegisters for FakeVcpu {
        fn get_registers(&self) -> Result<X86Registers, HvError> {
            Ok(X86Registers::default())
        }
        fn set_registers(&self, _regs: &X86Registers) -> Result<(), HvError> {
            Ok(())
        }
        fn get_special_registers(&self) -> Result<X86SpecialRegisters, HvError> {
            Ok(X86SpecialRegisters::default())
        }
        fn set_special_registers(&self, _sregs: &X86SpecialRegisters) -> Result<(), HvError> {
            Ok(())
        }
    }

    impl ResettableVcpu for FakeVcpu {
        fn reset_arch_state(&mut self, _is_boot_cpu: bool) -> Result<(), HvError> {
            self.arch_resets.fetch_add(1, Ordering::AcqRel);
            Ok(())
        }
    }

    struct Kick(Arc<AtomicU32>);
    impl VcpuKick for Kick {
        fn kick(&self) {
            self.0.fetch_add(1, Ordering::AcqRel);
        }
    }

    #[derive(Default)]
    struct RecordingMachine {
        quiesced: AtomicU32,
        unquiesced: AtomicU32,
        machine_resets: AtomicU32,
        vcpu_resets: AtomicU32,
        fail: AtomicBool,
        /// Stands in for the second vCPU of an SMP guest latching a reset
        /// request while the first one's reset is already in flight. Set from
        /// inside `reset_machine`, which is the only deterministic way to be
        /// *inside* a reset from a test.
        latch_during_reset: Mutex<Option<Arc<Lifecycle>>>,
    }

    impl MachineLifecycle for RecordingMachine {
        fn quiesce(&self) {
            self.quiesced.fetch_add(1, Ordering::AcqRel);
        }
        fn unquiesce(&self) {
            self.unquiesced.fetch_add(1, Ordering::AcqRel);
        }
        fn reset_machine(&self) -> Result<(), String> {
            self.machine_resets.fetch_add(1, Ordering::AcqRel);
            if let Ok(latch) = self.latch_during_reset.lock() {
                if let Some(lifecycle) = latch.as_ref() {
                    assert!(lifecycle.request_guest_reset());
                }
            }
            if self.fail.load(Ordering::Acquire) {
                return Err("device reset refused".into());
            }
            Ok(())
        }
        fn reset_vcpu(&self, _index: u32, _vcpu: &dyn VcpuRegisters) -> Result<(), String> {
            self.vcpu_resets.fetch_add(1, Ordering::AcqRel);
            Ok(())
        }
    }

    /// Spawns `n` "vCPU" threads that tick a counter and honour the checkpoint,
    /// exactly as a real run loop does.
    fn spawn_fake_vcpus(
        lifecycle: &Arc<Lifecycle>,
        n: u32,
    ) -> (
        Vec<std::thread::JoinHandle<()>>,
        Vec<Arc<AtomicU64>>,
        Arc<AtomicU32>,
    ) {
        let kicks = Arc::new(AtomicU32::new(0));
        let mut ticks = Vec::new();
        let mut handles = Vec::new();
        for index in 0..n {
            let counter = Arc::new(AtomicU64::new(0));
            ticks.push(Arc::clone(&counter));
            lifecycle.register_kicker(Arc::new(Kick(Arc::clone(&kicks))));
            let lifecycle = Arc::clone(lifecycle);
            let mut vcpu = FakeVcpu {
                index,
                ticks: Arc::clone(&counter),
                arch_resets: Arc::new(AtomicU32::new(0)),
            };
            handles.push(std::thread::spawn(move || loop {
                if lifecycle.checkpoint(vcpu.index, &mut vcpu) == Checkpoint::Stopped {
                    lifecycle.vcpu_finished(vcpu.index);
                    return;
                }
                vcpu.ticks.fetch_add(1, Ordering::AcqRel);
                std::thread::sleep(Duration::from_micros(200));
            }));
        }
        (handles, ticks, kicks)
    }

    fn stop(lifecycle: &Arc<Lifecycle>, handles: Vec<std::thread::JoinHandle<()>>) {
        lifecycle.shutdown();
        for handle in handles {
            handle.join().expect("fake vCPU thread");
        }
    }

    /// The acceptance shape of pause: after it returns, no vCPU makes progress;
    /// after resume, they do again.
    #[test]
    fn a_paused_vm_stops_making_progress_and_resumes() {
        let lifecycle = Lifecycle::new(2);
        let (handles, ticks, _) = spawn_fake_vcpus(&lifecycle, 2);
        std::thread::sleep(Duration::from_millis(20));

        lifecycle.pause().expect("pause");
        assert_eq!(lifecycle.state(), RunState::Paused);
        let frozen: Vec<u64> = ticks.iter().map(|t| t.load(Ordering::Acquire)).collect();
        assert!(frozen.iter().all(|&t| t > 0), "the vCPUs ran at all");
        std::thread::sleep(Duration::from_millis(50));
        for (i, counter) in ticks.iter().enumerate() {
            assert_eq!(
                counter.load(Ordering::Acquire),
                frozen[i],
                "vCPU {i} kept running while paused"
            );
        }

        lifecycle.resume().expect("resume");
        assert_eq!(lifecycle.state(), RunState::Running);
        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline {
            if ticks
                .iter()
                .enumerate()
                .all(|(i, c)| c.load(Ordering::Acquire) > frozen[i])
            {
                stop(&lifecycle, handles);
                return;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        stop(&lifecycle, handles);
        panic!("the vCPUs did not resume");
    }

    #[test]
    fn pause_and_resume_are_idempotent() {
        let lifecycle = Lifecycle::new(1);
        let (handles, _, _) = spawn_fake_vcpus(&lifecycle, 1);
        lifecycle.resume().expect("resume while running is a no-op");
        lifecycle.pause().expect("pause");
        lifecycle.pause().expect("pause again");
        lifecycle.resume().expect("resume");
        lifecycle.resume().expect("resume again");
        stop(&lifecycle, handles);
    }

    /// Reset runs the machine hook exactly once and the per-vCPU hook once per
    /// vCPU, and leaves the VM running.
    #[test]
    fn reset_resets_the_machine_once_and_every_vcpu() {
        let lifecycle = Lifecycle::new(3);
        let machine = Arc::new(RecordingMachine::default());
        lifecycle.attach_machine(Arc::clone(&machine) as Arc<dyn MachineLifecycle>);
        let (handles, ticks, _) = spawn_fake_vcpus(&lifecycle, 3);
        std::thread::sleep(Duration::from_millis(20));

        lifecycle.reset().expect("reset");
        assert_eq!(machine.machine_resets.load(Ordering::Acquire), 1);
        assert_eq!(machine.vcpu_resets.load(Ordering::Acquire), 3);
        assert_eq!(lifecycle.state(), RunState::Running);
        assert_eq!(lifecycle.resets(), 1);

        // And the guest is running again.
        let before: Vec<u64> = ticks.iter().map(|t| t.load(Ordering::Acquire)).collect();
        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline
            && !ticks
                .iter()
                .enumerate()
                .all(|(i, c)| c.load(Ordering::Acquire) > before[i])
        {
            std::thread::sleep(Duration::from_millis(5));
        }
        stop(&lifecycle, handles);
    }

    /// Reset from a paused VM works and ends running: a guest's own reboot must
    /// not leave the VM frozen, and neither should the operator's.
    #[test]
    fn reset_while_paused_ends_running() {
        let lifecycle = Lifecycle::new(2);
        let machine = Arc::new(RecordingMachine::default());
        lifecycle.attach_machine(Arc::clone(&machine) as Arc<dyn MachineLifecycle>);
        let (handles, _, _) = spawn_fake_vcpus(&lifecycle, 2);
        lifecycle.pause().expect("pause");
        lifecycle.reset().expect("reset while paused");
        assert_eq!(lifecycle.state(), RunState::Running);
        assert_eq!(machine.machine_resets.load(Ordering::Acquire), 1);
        assert_eq!(machine.vcpu_resets.load(Ordering::Acquire), 2);
        stop(&lifecycle, handles);
    }

    /// A machine that cannot reset itself must not be re-entered: the VM stops
    /// instead of running on half-reset devices.
    #[test]
    fn a_failed_machine_reset_stops_the_vm_rather_than_resuming_it() {
        let lifecycle = Lifecycle::new(1);
        let machine = Arc::new(RecordingMachine::default());
        machine.fail.store(true, Ordering::Release);
        lifecycle.attach_machine(Arc::clone(&machine) as Arc<dyn MachineLifecycle>);
        let (handles, _, _) = spawn_fake_vcpus(&lifecycle, 1);
        let error = lifecycle.reset().expect_err("the machine refused");
        assert!(matches!(error, LifecycleError::Reset(_)), "{error}");
        assert_eq!(lifecycle.state(), RunState::Stopping);
        for handle in handles {
            handle.join().expect("fake vCPU thread");
        }
    }

    #[test]
    fn reset_without_a_machine_is_refused() {
        let lifecycle = Lifecycle::new(1);
        assert!(matches!(lifecycle.reset(), Err(LifecycleError::NoMachine)));
    }

    /// A vCPU whose run loop already ended must not hold a barrier open — a
    /// halted CPU would otherwise make every pause time out.
    #[test]
    fn a_finished_vcpu_does_not_hold_the_barrier() {
        let lifecycle = Lifecycle::new(2);
        let (handles, _, _) = spawn_fake_vcpus(&lifecycle, 1); // only vCPU 0 runs
        lifecycle.vcpu_finished(1);
        let started = Instant::now();
        lifecycle.pause().expect("pause with one live vCPU");
        assert!(started.elapsed() < ACK_TIMEOUT);
        lifecycle.resume().expect("resume");
        stop(&lifecycle, handles);
    }

    /// The guest-reset latch is a one-shot: the supervisor consumes it once.
    #[test]
    fn the_guest_reset_latch_is_consumed_once() {
        let lifecycle = Lifecycle::new(1);
        assert!(!lifecycle.take_guest_reset());
        assert!(lifecycle.request_guest_reset());
        assert!(lifecycle.request_guest_reset());
        assert!(lifecycle.take_guest_reset());
        assert!(!lifecycle.take_guest_reset());
    }

    /// A guest that reboots straight back into the fault that rebooted it must
    /// not keep the host resetting forever: after a handful of resets inside the
    /// storm window the request is refused, and the run loop ends the VM.
    #[test]
    fn a_reset_storm_is_refused_rather_than_served() {
        let lifecycle = Lifecycle::new(1);
        let machine = Arc::new(RecordingMachine::default());
        lifecycle.attach_machine(Arc::clone(&machine) as Arc<dyn MachineLifecycle>);
        let (handles, _, _) = spawn_fake_vcpus(&lifecycle, 1);
        // The first reset is always served; each one stamps `last_reset`, so
        // every request that follows immediately counts towards the storm.
        lifecycle.reset().expect("first reset");
        let mut refusals = 0;
        for _ in 0..MAX_RESET_STORM + 2 {
            if !lifecycle.request_guest_reset() {
                refusals += 1;
                break;
            }
            assert!(lifecycle.take_guest_reset());
            lifecycle.reset().expect("reset");
        }
        assert_eq!(refusals, 1, "the storm was never refused");
        stop(&lifecycle, handles);
    }

    /// …and a guest that reboots at a human rate is never refused: the storm
    /// counter clears as soon as one boot outlives the window.
    #[test]
    fn a_slow_reboot_never_trips_the_storm_guard() {
        let lifecycle = Lifecycle::new(1);
        let machine = Arc::new(RecordingMachine::default());
        lifecycle.attach_machine(Arc::clone(&machine) as Arc<dyn MachineLifecycle>);
        let (handles, _, _) = spawn_fake_vcpus(&lifecycle, 1);
        for _ in 0..MAX_RESET_STORM + 3 {
            assert!(lifecycle.request_guest_reset());
            assert!(lifecycle.take_guest_reset());
            lifecycle.reset().expect("reset");
            // Pretend the guest booted for longer than the window.
            lifecycle.lock().last_reset = Some(Instant::now() - RESET_STORM_WINDOW * 2);
        }
        stop(&lifecycle, handles);
    }

    /// A reset consumes the guest's request, including one latched *while* it
    /// was running — which on an SMP guest is the normal case, because every
    /// vCPU polls the reset registers after its own next exit.
    #[test]
    fn a_reset_consumes_a_request_latched_while_it_ran() {
        let lifecycle = Lifecycle::new(2);
        let machine = Arc::new(RecordingMachine::default());
        lifecycle.attach_machine(Arc::clone(&machine) as Arc<dyn MachineLifecycle>);
        let (handles, _, _) = spawn_fake_vcpus(&lifecycle, 2);

        assert!(lifecycle.request_guest_reset());
        assert!(lifecycle.take_guest_reset());
        // The second vCPU's latch, landing after `reset` cleared the first one
        // and before it finished.
        if let Ok(mut latch) = machine.latch_during_reset.lock() {
            *latch = Some(Arc::clone(&lifecycle));
        }
        lifecycle.reset().expect("reset");
        assert!(
            !lifecycle.take_guest_reset(),
            "the racing request survived the reset it belonged to, and would be served again"
        );
        stop(&lifecycle, handles);
    }

    #[test]
    fn can_reset_reports_whether_a_machine_is_attached() {
        let lifecycle = Lifecycle::new(1);
        assert!(!lifecycle.can_reset());
        lifecycle.attach_machine(Arc::new(RecordingMachine::default()));
        assert!(lifecycle.can_reset());
    }

    /// Shutdown releases a paused VM, so teardown never deadlocks on a hold.
    #[test]
    fn shutdown_releases_a_paused_vm() {
        let lifecycle = Lifecycle::new(2);
        let (handles, _, _) = spawn_fake_vcpus(&lifecycle, 2);
        lifecycle.pause().expect("pause");
        stop(&lifecycle, handles); // joins: would hang if the hold survived
        assert_eq!(lifecycle.state(), RunState::Stopping);
    }
}
