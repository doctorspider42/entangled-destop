//! The driver never sees a wait it cannot meet (ADR-0004, the
//! wait-before-signal amendment of 2026-09-26).
//!
//! # The problem
//!
//! Vulkan lets a queue wait on a timeline value nothing has been asked to
//! signal yet: the signal may come later from another queue, from
//! `vkSignalSemaphore` on the host, or from an imported payload. A guest can
//! therefore hand the host a queue wait that nothing will ever satisfy, in one
//! call. On the RTX 2070 (driver 580.88) such a wait, pending on one
//! `VkDevice` while another device of the process did real GPU work, wedged
//! the driver process-wide twice in seven runs: both threads stuck inside it,
//! the process unkillable until reboot. In the VMM the process is the host's
//! whole GPU presence — the other contexts' rings, the scanout device, the
//! display's presenter.
//!
//! # The rule
//!
//! **A submit reaches the driver only once every wait in it is covered by a
//! signal that has already reached the driver.** Anything else is *held*
//! here, host-side, and released — in order — the moment a covering signal
//! is submitted. By induction every submit the driver holds depends only on
//! work submitted before it, so the driver's dependency graph is acyclic and
//! every queue drains (bounded by GPU time, which Windows' TDR bounds). This
//! is what Mesa's own `vk_queue` does for drivers that cannot wait before a
//! signal (`VK_QUEUE_SUBMIT_MODE_THREADED`); here it runs inline, on the
//! thread that submits the covering signal.
//!
//! What *covers* a wait, per object, as the host has been asked (never as
//! the guest has been told — while submits are held the two orders differ):
//!
//! * a **timeline** value `v`: [`SemaphoreState::host_value`] `>= v`, the
//!   highest of the semaphore's initial value, every signal of a submit that
//!   reached the driver, and every `vkSignalSemaphore`;
//! * a **binary** semaphore: [`SemaphoreState::host_pending`] — a signal
//!   reached the driver and no wait that reached it has consumed it. A
//!   binary wait with no signal submitted at all, in the guest's own order,
//!   is invalid usage and is refused before anything else ([`super::submit`]);
//!   a temporary imported payload (the sync-file emulation) is consumed
//!   before the host sees the wait, so it needs no cover;
//! * a **`vkCmdWaitEvents`** on an event the command buffer did not set
//!   itself ([`EventUse::needs_set`]): the event is set, as a host
//!   `vkSetEvent` or the last set of a submitted command buffer left it
//!   ([`EventState::set`]).
//!
//! Semaphores are never shared across devices or contexts here: a device's
//! semaphore is a child of it in the object table, and nothing imports one
//! (Mesa's venus imports only a sync file, which is the emulated payload
//! above; every other handle type is exported at most, never imported). The
//! renderer's own submits — ring fences, the marks of shared-image ordering
//! ([`super::writes`]) — wait on nothing, and the scanout device and the
//! display's presenter are devices of their own whose waits are bounded CPU
//! waits on fences of work the driver already holds.
//!
//! # Order
//!
//! A held submit holds every later submit of its queue, and every
//! virtio-gpu fence on the queue's timeline, behind it — also host-side, so
//! the guest's per-queue order and its fence order are what the driver sees.
//! Releasing runs the front of every queue whose front is covered, repeatedly
//! until nothing more moves; releasing one may cover another queue's.
//!
//! # Waits on the host's side
//!
//! A CPU wait whose answer is behind a held submit never goes to the driver:
//! `vkWaitForFences` on a held submit's fence, `vkWaitSemaphores` on an
//! uncovered value, `vkQueueWaitIdle` and `vkDeviceWaitIdle` of a queue with
//! held work, and `vkGetQueryPoolResults(WAIT)` (emulated by polling without
//! `WAIT`, which the driver bounds). Each of these *naps* instead: the ring
//! worker sleeps [`NAP`] with the context lock released and asks again,
//! honouring the guest's timeout, and a ring being torn down stops within one.
//!
//! # Bounds and teardown
//!
//! Every held item is charged to [`Class::HeldSubmits`] and its wire bytes to
//! [`Class::HeldBytes`]; past either the context ends, as for any command
//! with no `VkResult` to refuse with. A context that goes fatal, is
//! destroyed or reset, and a `vkDestroyDevice`, drop their held submits
//! (nothing of them reached the driver, so nothing waits for them) and retire
//! their held virtio-gpu fences at once, as ADR-0005 wants a queue's fences
//! answered when it goes. There is no deadline: a guest waiting on a value
//! its own CPU signals later may wait as long as it likes, and one waiting
//! forever holds only its own charges and its own ring.
//!
//! # Cost
//!
//! With nothing held, a submit pays one pass over its waits (a table lookup
//! each, already done once by the planning) and a map lookup per command
//! buffer for events. Released submits pay the ordinary submit.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use crate::renderer::FenceOutcome;
use crate::venus::protocol::{Command, VkSubmitInfoNext, VK_ERROR_UNKNOWN, VK_SUCCESS};
use crate::venus::renderer::{FenceRetirer, RingFence};
use crate::venus::shmem::Charge;

use super::context::{invalid, ExecError, VulkanContext};
use super::generated;
use super::host::HostVulkan;
use super::limits::Class;
use super::objects::{EventState, Facts, Kind, SemaphoreState};

/// How long a ring worker sleeps, with the context lock released, before it
/// asks again about a wait whose answer is behind held work.
pub const NAP: Duration = Duration::from_millis(1);

/// The least a held submit is charged to [`Class::HeldBytes`], whatever its
/// wire size: what the bookkeeping around it costs.
const MIN_HELD_BYTES: u64 = 64;

/// One batch of a submit, by guest id: its waits `(semaphore, value)`, its
/// command buffers, its signals `(semaphore, value)`. A binary semaphore's
/// value is 0.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Batch {
    /// Waits, `(semaphore, value)`.
    pub waits: Vec<(u64, u64)>,
    /// Command buffers, in order.
    pub cbs: Vec<u64>,
    /// Signals, `(semaphore, value)`.
    pub signals: Vec<(u64, u64)>,
}

/// A submit's batches, in order.
pub type SyncOps = Vec<Batch>;

/// What a `vkQueueSubmit`/`vkQueueSubmit2` synchronises on, by guest id,
/// after [`super::submit`] has planned it (the waits a temporary payload
/// satisfies are gone by then).
#[must_use]
pub fn sync_ops(command: &Command<'_>) -> SyncOps {
    match command {
        Command::QueueSubmit(a) => a
            .p_submits
            .iter()
            .flatten()
            .map(|s| {
                let (waits, signals) = s
                    .p_next
                    .iter()
                    .find_map(|l| match l {
                        VkSubmitInfoNext::VkTimelineSemaphoreSubmitInfo(t) => Some((
                            t.p_wait_semaphore_values.as_deref().unwrap_or_default(),
                            t.p_signal_semaphore_values.as_deref().unwrap_or_default(),
                        )),
                        _ => None,
                    })
                    .unwrap_or_default();
                let value = |values: &[u64], i: usize| values.get(i).copied().unwrap_or(0);
                Batch {
                    waits: s
                        .p_wait_semaphores
                        .iter()
                        .flatten()
                        .enumerate()
                        .map(|(i, x)| (x.0, value(waits, i)))
                        .collect(),
                    cbs: s.p_command_buffers.iter().flatten().map(|c| c.0).collect(),
                    signals: s
                        .p_signal_semaphores
                        .iter()
                        .flatten()
                        .enumerate()
                        .map(|(i, x)| (x.0, value(signals, i)))
                        .collect(),
                }
            })
            .collect(),
        Command::QueueSubmit2(a) => a
            .p_submits
            .iter()
            .flatten()
            .map(|s| Batch {
                waits: s
                    .p_wait_semaphore_infos
                    .iter()
                    .flatten()
                    .map(|x| (x.semaphore.0, x.value))
                    .collect(),
                cbs: s
                    .p_command_buffer_infos
                    .iter()
                    .flatten()
                    .map(|c| c.command_buffer.0)
                    .collect(),
                signals: s
                    .p_signal_semaphore_infos
                    .iter()
                    .flatten()
                    .map(|x| (x.semaphore.0, x.value))
                    .collect(),
            })
            .collect(),
        _ => Vec::new(),
    }
}

/// A submit command owned by the executor, for holding: the same arguments,
/// no borrow of the ring's bytes.
fn owned(command: &Command<'_>) -> Option<Command<'static>> {
    match command {
        Command::QueueSubmit(a) => Some(Command::QueueSubmit(a.clone())),
        Command::QueueSubmit2(a) => Some(Command::QueueSubmit2(a.clone())),
        _ => None,
    }
}

// ------------------------------------------------------------------ events

/// One operation a command buffer records on an event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventOp {
    /// `vkCmdSetEvent`, `vkCmdSetEvent2`.
    Set,
    /// `vkCmdResetEvent`, `vkCmdResetEvent2`.
    Reset,
    /// One event of `vkCmdWaitEvents`, `vkCmdWaitEvents2`.
    Wait,
}

/// What one command buffer does to one event, summed over its recording.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct EventUse {
    /// It waits on the event before setting it itself: the event must be
    /// set when the command buffer runs, by something outside it.
    pub needs_set: bool,
    /// It resets the event somewhere.
    pub resets: bool,
    /// The event's state after it: set, reset, or untouched.
    pub exit: Option<bool>,
}

impl EventUse {
    /// `op`, recorded after everything so far.
    ///
    /// # Errors
    /// A wait on an event this command buffer reset with no set after it:
    /// only a host `vkSetEvent` racing the GPU could release it.
    pub fn record(&mut self, op: EventOp) -> Result<(), &'static str> {
        match op {
            EventOp::Set => self.exit = Some(true),
            EventOp::Reset => {
                self.resets = true;
                self.exit = Some(false);
            }
            EventOp::Wait => match self.exit {
                Some(true) => {}
                None => self.needs_set = true,
                Some(false) => {
                    return Err(
                        "a wait on an event the same command buffer reset, with no set \
                                between: only a host set racing the GPU could release it",
                    );
                }
            },
        }
        Ok(())
    }

    /// `then`, a secondary's use, executed after everything so far.
    ///
    /// # Errors
    /// As [`Self::record`].
    pub fn then(&mut self, then: EventUse) -> Result<(), &'static str> {
        if then.needs_set {
            self.record(EventOp::Wait)?;
        }
        self.resets |= then.resets;
        if then.exit.is_some() {
            self.exit = then.exit;
        }
        Ok(())
    }
}

// ------------------------------------------------------------------- holds

/// One thing held back from a queue.
enum Held {
    /// A guest submit, by guest id, and what it synchronises on.
    Submit {
        command: Box<Command<'static>>,
        ops: SyncOps,
        since: Instant,
        _charges: (Charge, Charge),
    },
    /// A virtio-gpu fence on the queue's timeline, behind held submits.
    RingFence {
        fence: RingFence,
        retire: FenceRetirer,
        _charge: Charge,
    },
}

struct HoldQueue {
    device: u64,
    items: VecDeque<Held>,
}

/// One context's held work, per queue (by guest id).
#[derive(Default)]
pub struct Holds {
    queues: BTreeMap<u64, HoldQueue>,
    /// Whether the first hold of this context has been logged at `info`.
    logged: bool,
}

impl std::fmt::Debug for Holds {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Holds")
            .field("queues", &self.queues.len())
            .field("items", &self.len())
            .finish()
    }
}

impl Holds {
    /// Whether nothing is held.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.queues.is_empty()
    }

    /// Items held, every queue together.
    #[must_use]
    pub fn len(&self) -> usize {
        self.queues.values().map(|q| q.items.len()).sum()
    }

    /// Whether queue `queue` has anything held.
    #[must_use]
    pub fn holds_queue(&self, queue: u64) -> bool {
        self.queues.contains_key(&queue)
    }

    /// Whether any queue of device `device` has anything held.
    #[must_use]
    pub fn holds_device(&self, device: u64) -> bool {
        self.queues.values().any(|q| q.device == device)
    }

    /// Virtio-gpu fences held.
    #[must_use]
    pub fn ring_fences(&self) -> usize {
        self.queues
            .values()
            .flat_map(|q| q.items.iter())
            .filter(|h| matches!(h, Held::RingFence { .. }))
            .count()
    }

    /// Whether a held submit carries fence `fence` (a guest id).
    #[must_use]
    pub fn holds_fence(&self, fence: u64) -> bool {
        fence != 0
            && self.queues.values().flat_map(|q| q.items.iter()).any(|h| {
                matches!(h, Held::Submit { command, .. } if match &**command {
                    Command::QueueSubmit(a) => a.fence.0 == fence,
                    Command::QueueSubmit2(a) => a.fence.0 == fence,
                    _ => false,
                })
            })
    }

    fn push(&mut self, queue: u64, device: u64, item: Held) {
        self.queues
            .entry(queue)
            .or_insert_with(|| HoldQueue {
                device,
                items: VecDeque::new(),
            })
            .items
            .push_back(item);
    }

    fn pop(&mut self, queue: u64) -> Option<Held> {
        let q = self.queues.get_mut(&queue)?;
        let item = q.items.pop_front();
        if q.items.is_empty() {
            self.queues.remove(&queue);
        }
        item
    }
}

/// How the executor's holds went, every context together since it was
/// made: the usage log's `holds_*` fields.
#[derive(Debug, Default)]
pub struct HoldStats {
    held: AtomicU64,
    ring_fences: AtomicU64,
    released: AtomicU64,
    dropped: AtomicU64,
    longest_us: AtomicU64,
    naps: AtomicU64,
}

/// A copy of [`HoldStats`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct HoldCounts {
    /// Submits held because a wait was not covered, or their queue had held
    /// work before them.
    pub held: u64,
    /// Virtio-gpu fences held behind them.
    pub ring_fences: u64,
    /// Held submits that later reached the driver.
    pub released: u64,
    /// Held submits dropped by a teardown, a reset or a fatal context.
    pub dropped: u64,
    /// The longest any released submit was held, in microseconds.
    pub longest_us: u64,
    /// Naps ring workers took instead of a driver wait: a wait whose answer
    /// was behind held work, or a query result not ready yet.
    pub naps: u64,
}

impl HoldCounts {
    /// Field by field, the larger of the two.
    #[must_use]
    pub fn max(self, other: Self) -> Self {
        Self {
            held: self.held.max(other.held),
            ring_fences: self.ring_fences.max(other.ring_fences),
            released: self.released.max(other.released),
            dropped: self.dropped.max(other.dropped),
            longest_us: self.longest_us.max(other.longest_us),
            naps: self.naps.max(other.naps),
        }
    }
}

impl HoldStats {
    /// A copy of the counts.
    #[must_use]
    pub fn counts(&self) -> HoldCounts {
        HoldCounts {
            held: self.held.load(Ordering::Relaxed),
            ring_fences: self.ring_fences.load(Ordering::Relaxed),
            released: self.released.load(Ordering::Relaxed),
            dropped: self.dropped.load(Ordering::Relaxed),
            longest_us: self.longest_us.load(Ordering::Relaxed),
            naps: self.naps.load(Ordering::Relaxed),
        }
    }
}

/// What [`VulkanContext::enqueue`] did with a submit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Enqueued {
    /// It reached the driver, which answered this.
    Submitted(i32),
    /// It is held until its waits are covered.
    Held,
}

impl<H: HostVulkan> VulkanContext<H> {
    /// What is known of semaphore `id`, if it is one of `device`'s.
    fn semaphore_facts(&self, device: u64, id: u64) -> Option<SemaphoreState> {
        match self.objects.raw(Kind::Semaphore, device, id).ok()?.facts {
            Facts::Semaphore(state) => Some(state),
            _ => None,
        }
    }

    fn event_facts(&self, device: u64, id: u64) -> Option<EventState> {
        match self.objects.raw(Kind::Event, device, id).ok()?.facts {
            Facts::Event(state) => Some(state),
            _ => None,
        }
    }

    fn update_semaphore(&mut self, device: u64, id: u64, f: impl FnOnce(&mut SemaphoreState)) {
        if let Ok(object) = self.objects.raw_mut(Kind::Semaphore, device, id) {
            if let Facts::Semaphore(state) = &mut object.facts {
                f(state);
            }
        }
    }

    pub(super) fn update_event(&mut self, device: u64, id: u64, f: impl FnOnce(&mut EventState)) {
        if let Ok(object) = self.objects.raw_mut(Kind::Event, device, id) {
            if let Facts::Event(state) = &mut object.facts {
                f(state);
            }
        }
    }

    /// `vkSignalSemaphore(id, value)` reached the driver.
    pub(super) fn host_signalled(&mut self, device: u64, id: u64, value: u64) {
        self.update_semaphore(device, id, |s| s.host_value = s.host_value.max(value));
    }

    /// Whether timeline value `value` of semaphore `id` is covered: a
    /// signal of it or past it has reached the driver. An id that is not a
    /// timeline semaphore of `device` is the driver call's to refuse.
    #[must_use]
    pub(super) fn timeline_covered(&self, device: u64, id: u64, value: u64) -> bool {
        self.semaphore_facts(device, id)
            .is_none_or(|s| !s.timeline || s.host_value >= value)
    }

    /// Whether every wait of `ops` is covered, in order: a batch's signals
    /// cover a later batch's waits of the same submit. An id that names
    /// nothing counts as covered — the submit then reaches the translation,
    /// which refuses it, rather than waiting here for ever.
    #[must_use]
    pub(super) fn covered(&self, device: u64, ops: &SyncOps) -> bool {
        let mut semaphores: HashMap<u64, SemaphoreState> = HashMap::new();
        let mut events: HashMap<u64, bool> = HashMap::new();
        for batch in ops {
            for (id, value) in &batch.waits {
                let Some(state) = semaphores
                    .get(id)
                    .copied()
                    .or_else(|| self.semaphore_facts(device, *id))
                else {
                    continue;
                };
                if state.timeline {
                    if state.host_value < *value {
                        return false;
                    }
                } else if !state.host_pending {
                    return false;
                } else {
                    semaphores.insert(
                        *id,
                        SemaphoreState {
                            host_pending: false,
                            ..state
                        },
                    );
                }
            }
            for cb in &batch.cbs {
                let Some(uses) = self.recordings.events.get(cb) else {
                    continue;
                };
                for (event, used) in uses {
                    let set = events
                        .get(event)
                        .copied()
                        .or_else(|| self.event_facts(device, *event).map(|e| e.set));
                    if used.needs_set && set == Some(false) {
                        return false;
                    }
                    if let Some(exit) = used.exit {
                        events.insert(*event, exit);
                    }
                }
            }
            for (id, value) in &batch.signals {
                let Some(mut state) = semaphores
                    .get(id)
                    .copied()
                    .or_else(|| self.semaphore_facts(device, *id))
                else {
                    continue;
                };
                if state.timeline {
                    state.host_value = state.host_value.max(*value);
                } else {
                    state.host_pending = true;
                }
                semaphores.insert(*id, state);
            }
        }
        true
    }

    /// A submit of `ops` reached the driver: what it signals is covered from
    /// now on, what it waits on is consumed, what it does to events is the
    /// events' state.
    fn commit_host(&mut self, device: u64, ops: &SyncOps) {
        for batch in ops {
            for (id, _) in &batch.waits {
                self.update_semaphore(device, *id, |s| {
                    if !s.timeline {
                        s.host_pending = false;
                    }
                });
            }
            for cb in &batch.cbs {
                let uses: Vec<(u64, EventUse)> = self
                    .recordings
                    .events
                    .get(cb)
                    .map(|u| u.iter().map(|(e, u)| (*e, *u)).collect())
                    .unwrap_or_default();
                for (event, used) in uses {
                    self.update_event(device, event, |e| {
                        e.waiters |= used.needs_set;
                        if let Some(exit) = used.exit {
                            e.set = exit;
                        }
                    });
                }
            }
            for (id, value) in &batch.signals {
                self.update_semaphore(device, *id, |s| {
                    if s.timeline {
                        s.host_value = s.host_value.max(*value);
                    } else {
                        s.host_pending = true;
                    }
                });
            }
        }
    }

    /// Before `ops` reaches the driver: a command buffer resetting an event
    /// that submitted work may still be waiting on must not overtake that
    /// wait (the wait could then never pass), so the device's work is waited
    /// for first. The wait is finite: the event is set, and every wait the
    /// driver holds is covered.
    fn settle_for_resets(&mut self, device: u64, ops: &SyncOps) {
        let waited: Vec<u64> = ops
            .iter()
            .flat_map(|b| b.cbs.iter())
            .filter_map(|cb| self.recordings.events.get(cb))
            .flat_map(|uses| uses.iter())
            .filter(|(e, u)| u.resets && self.event_facts(device, **e).is_some_and(|s| s.waiters))
            .map(|(e, _)| *e)
            .collect();
        if waited.is_empty() {
            return;
        }
        self.settle(device);
        for event in waited {
            self.update_event(device, event, |e| e.waiters = false);
        }
    }

    /// A planned submit of `queue`: to the driver now if every wait in it is
    /// covered and nothing of its queue is held, otherwise held. A submit
    /// that reached the driver releases whatever it covers.
    ///
    /// # Errors
    /// A refused id or value, or a hold past its caps: fatal.
    pub(super) fn enqueue(
        &mut self,
        queue: u64,
        device: u64,
        command: &mut Command<'_>,
    ) -> Result<Enqueued, ExecError> {
        let ops = sync_ops(command);
        if self.holds.holds_queue(queue) || !self.covered(device, &ops) {
            self.hold_submit(queue, device, command, ops)?;
            generated::set_result(command, VK_SUCCESS);
            return Ok(Enqueued::Held);
        }
        self.submit_now(queue, device, command, &ops)?;
        let ret = generated::result_of(command).unwrap_or(VK_ERROR_UNKNOWN);
        self.release_held()?;
        Ok(Enqueued::Submitted(ret))
    }

    fn hold_submit(
        &mut self,
        queue: u64,
        device: u64,
        command: &Command<'_>,
        ops: SyncOps,
    ) -> Result<(), ExecError> {
        let name = command.name();
        let owned = owned(command).ok_or_else(|| invalid(name, "not a submit"))?;
        let refused = |refused| ExecError::Refused {
            command: name,
            refused,
        };
        let count = self
            .objects
            .limits()
            .charge(Class::HeldSubmits, 1)
            .map_err(refused)?;
        let bytes = u64::try_from(self.command_bytes)
            .unwrap_or(u64::MAX)
            .max(MIN_HELD_BYTES);
        let bytes = self
            .objects
            .limits()
            .charge(Class::HeldBytes, bytes)
            .map_err(refused)?;
        let behind = self.holds.holds_queue(queue);
        if self.holds.logged {
            tracing::debug!(
                ctx_id = self.ctx_id,
                queue = format_args!("{queue:#x}"),
                behind,
                "a Venus submit is held on the host"
            );
        } else {
            self.holds.logged = true;
            tracing::info!(
                ctx_id = self.ctx_id,
                queue = format_args!("{queue:#x}"),
                behind,
                "a Venus submit waits on something no submitted signal covers yet; it is held \
                 on the host until one is, and the driver never sees the wait (ADR-0004, the \
                 wait-before-signal amendment)"
            );
        }
        self.holds.push(
            queue,
            device,
            Held::Submit {
                command: Box::new(owned),
                ops,
                since: Instant::now(),
                _charges: (count, bytes),
            },
        );
        self.hold_stats.held.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }

    /// A submit of `queue` whose waits are covered, to the driver: the
    /// shared-payload claim and mark around it ([`super::writes`]), then the
    /// queue's record of pending work and the host's cover.
    ///
    /// # Errors
    /// A refused id, fatal.
    fn submit_now(
        &mut self,
        queue: u64,
        device: u64,
        command: &mut Command<'_>,
        ops: &SyncOps,
    ) -> Result<(), ExecError> {
        let name = command.name();
        let fence = match command {
            Command::QueueSubmit(a) => a.fence.0,
            Command::QueueSubmit2(a) => a.fence.0,
            _ => 0,
        };
        let guest_fence = match fence {
            0 => None,
            id => Some(
                self.objects
                    .raw(Kind::Fence, device, id)
                    .map_err(super::context::id_error(name))?
                    .host,
            ),
        };
        let buffers: Vec<u64> = ops.iter().flat_map(|b| b.cbs.iter().copied()).collect();
        self.settle_for_resets(device, ops);
        // Shared payloads first: no other owner's work on them may still be
        // running when this starts (`super::writes`). The mark follows the
        // submit whatever the driver answers, so the claim always ends.
        let claim = self.claim_payloads(queue, &buffers);
        let passed = self.pass_through(command);
        let submitted = passed.is_ok() && generated::result_of(command) == Some(VK_SUCCESS);
        if submitted {
            self.clock_submitted(queue, fence, guest_fence.unwrap_or(0));
            if self.objects.doomed() != 0 {
                // What the GPU has finished meanwhile frees the doomed even
                // when the guest destroys nothing more.
                self.poll_clocks(device);
            }
        }
        if let Some(claim) = claim {
            self.submit_mark(claim);
        }
        passed?;
        if submitted {
            self.commit_host(device, ops);
        }
        Ok(())
    }

    /// Release every held item whose turn has come: the front of each queue
    /// while it is covered, again and again until nothing moves.
    ///
    /// # Errors
    /// A released submit that names what is gone (the guest destroyed an
    /// object a pending submit uses): fatal, as it would have been had the
    /// submit not waited.
    pub(super) fn release_held(&mut self) -> Result<(), ExecError> {
        loop {
            let mut moved = false;
            let queues: Vec<u64> = self.holds.queues.keys().copied().collect();
            for queue in queues {
                loop {
                    let ready = match self
                        .holds
                        .queues
                        .get(&queue)
                        .and_then(|q| q.items.front().map(|item| (q.device, item)))
                    {
                        None => break,
                        Some((_, Held::RingFence { .. })) => true,
                        Some((device, Held::Submit { ops, .. })) => self.covered(device, ops),
                    };
                    if !ready {
                        break;
                    }
                    let device = self.holds.queues.get(&queue).map_or(0, |q| q.device);
                    let Some(item) = self.holds.pop(queue) else {
                        break;
                    };
                    moved = true;
                    match item {
                        Held::Submit {
                            mut command,
                            ops,
                            since,
                            _charges,
                        } => {
                            let held =
                                u64::try_from(since.elapsed().as_micros()).unwrap_or(u64::MAX);
                            self.hold_stats.released.fetch_add(1, Ordering::Relaxed);
                            self.hold_stats
                                .longest_us
                                .fetch_max(held, Ordering::Relaxed);
                            tracing::debug!(
                                ctx_id = self.ctx_id,
                                queue = format_args!("{queue:#x}"),
                                held_us = held,
                                "a held Venus submit is released"
                            );
                            self.submit_now(queue, device, &mut command, &ops)?;
                            let ret = generated::result_of(&command);
                            if ret != Some(VK_SUCCESS) {
                                tracing::debug!(
                                    ctx_id = self.ctx_id,
                                    ?ret,
                                    "a released Venus submit was refused by the driver"
                                );
                            }
                        }
                        Held::RingFence { fence, retire, .. } => {
                            match self.ring_fence_now(queue, fence, &retire) {
                                Ok(FenceOutcome::Pending) => {}
                                Ok(FenceOutcome::Signalled) => retire.retire(fence),
                                Err(why) => {
                                    tracing::debug!(
                                        ctx_id = self.ctx_id,
                                        %why,
                                        "a held ring fence is answered at once"
                                    );
                                    retire.retire(fence);
                                }
                            }
                        }
                    }
                }
            }
            if !moved {
                return Ok(());
            }
        }
    }

    /// A virtio-gpu fence on a queue with held work: held behind it, so it
    /// is never answered before the work it follows.
    ///
    /// # Errors
    /// The context's held items are at their cap: the device answers the
    /// fence at once, as for any refused fence.
    pub(super) fn hold_ring_fence(
        &mut self,
        queue: u64,
        device: u64,
        fence: RingFence,
        retire: &FenceRetirer,
    ) -> Result<FenceOutcome, String> {
        let charge = self
            .objects
            .limits()
            .charge(Class::HeldSubmits, 1)
            .map_err(|refused| refused.to_string())?;
        self.holds.push(
            queue,
            device,
            Held::RingFence {
                fence,
                retire: retire.clone(),
                _charge: charge,
            },
        );
        self.hold_stats.ring_fences.fetch_add(1, Ordering::Relaxed);
        Ok(FenceOutcome::Pending)
    }

    /// Drop everything held on `device`'s queues (every device's with
    /// `None`): the submits never reached the driver, so nothing waits for
    /// them, and the virtio-gpu fences behind them are retired now — the
    /// guest's work on them will never run (ADR-0005).
    pub(super) fn drop_held(&mut self, device: Option<u64>) {
        if self.holds.is_empty() {
            return;
        }
        let queues: Vec<u64> = self
            .holds
            .queues
            .iter()
            .filter(|(_, q)| device.is_none_or(|d| q.device == d))
            .map(|(id, _)| *id)
            .collect();
        for queue in queues {
            let Some(q) = self.holds.queues.remove(&queue) else {
                continue;
            };
            for item in q.items {
                match item {
                    Held::Submit { .. } => {
                        self.hold_stats.dropped.fetch_add(1, Ordering::Relaxed);
                    }
                    Held::RingFence { fence, retire, .. } => retire.retire(fence),
                }
            }
        }
    }

    /// What the holds have done, every context of the executor together.
    #[must_use]
    pub fn hold_counts(&self) -> HoldCounts {
        self.hold_stats.counts()
    }

    /// Items this context holds right now.
    #[must_use]
    pub fn held_items(&self) -> usize {
        self.holds.len()
    }

    /// Virtio-gpu fences not yet retired: on the fence threads, and held.
    #[must_use]
    pub fn pending_ring_fences(&self) -> usize {
        self.objects
            .pending_ring_fences()
            .saturating_add(self.holds.ring_fences())
    }

    /// A wait with nothing the driver can do for it yet: answered
    /// `VK_TIMEOUT` if the guest gave it no time, otherwise a nap
    /// ([`NAP`], off the context lock) and another look.
    pub(super) fn nap_or_time_out(&mut self, timeout: u64, ret: &mut i32) -> bool {
        if timeout == 0 {
            *ret = crate::venus::protocol::VK_TIMEOUT;
            return true;
        }
        self.nap = true;
        false
    }

    /// Whether the ring worker should nap before the next slice; asking
    /// clears it.
    pub fn take_nap(&mut self) -> bool {
        let nap = std::mem::take(&mut self.nap);
        if nap {
            self.hold_stats.naps.fetch_add(1, Ordering::Relaxed);
        }
        nap
    }

    // ------------------------------------------------------------ events

    /// `ops` recorded into command buffer `cb` (of `device`): its uses of
    /// each event, updated, for [`Self::store_event_uses`] once the host has
    /// taken the command.
    ///
    /// # Errors
    /// A wait on an event the command buffer reset (see [`EventUse::record`]).
    pub(super) fn plan_event_ops(
        &self,
        command: &'static str,
        cb: u64,
        ops: &[(u64, EventOp)],
    ) -> Result<Vec<(u64, EventUse)>, ExecError> {
        let current = self.recordings.events.get(&cb);
        let mut planned: Vec<(u64, EventUse)> = Vec::new();
        for (event, op) in ops {
            let at = match planned.iter().position(|(e, _)| e == event) {
                Some(at) => at,
                None => {
                    let use_ = current
                        .and_then(|c| c.get(event))
                        .copied()
                        .unwrap_or_default();
                    planned.push((*event, use_));
                    planned.len() - 1
                }
            };
            if let Some((_, use_)) = planned.get_mut(at) {
                use_.record(*op).map_err(|what| invalid(command, what))?;
            }
        }
        Ok(planned)
    }

    /// `vkCmdExecuteCommands` of `secondaries` into `primary`: the event
    /// uses the primary has afterwards.
    ///
    /// # Errors
    /// As [`Self::plan_event_ops`].
    pub(super) fn plan_inherited_events(
        &self,
        command: &'static str,
        primary: u64,
        secondaries: &[u64],
    ) -> Result<Vec<(u64, EventUse)>, ExecError> {
        let mut planned: Vec<(u64, EventUse)> = Vec::new();
        for secondary in secondaries {
            let Some(uses) = self.recordings.events.get(secondary) else {
                continue;
            };
            for (event, then) in uses {
                let at = match planned.iter().position(|(e, _)| e == event) {
                    Some(at) => at,
                    None => {
                        let use_ = self
                            .recordings
                            .events
                            .get(&primary)
                            .and_then(|c| c.get(event))
                            .copied()
                            .unwrap_or_default();
                        planned.push((*event, use_));
                        planned.len() - 1
                    }
                };
                if let Some((_, use_)) = planned.get_mut(at) {
                    use_.then(*then).map_err(|what| invalid(command, what))?;
                }
            }
        }
        Ok(planned)
    }

    /// Record what [`Self::plan_event_ops`] planned for `cb`.
    pub(super) fn store_event_uses(&mut self, cb: u64, uses: Vec<(u64, EventUse)>) {
        if uses.is_empty() {
            return;
        }
        let map = self.recordings.events.entry(cb).or_default();
        for (event, use_) in uses {
            map.insert(event, use_);
        }
    }

    /// `vkSetEvent` / `vkResetEvent` from the host (`set`), of `event` of
    /// `device`. A reset of an event submitted work may still wait on waits
    /// for that work first (see [`Self::settle_for_resets`]); a set releases
    /// what it covers.
    ///
    /// # Errors
    /// A refused id, fatal.
    pub(super) fn host_event(
        &mut self,
        command: &mut Command<'_>,
        device: u64,
        event: u64,
        set: bool,
    ) -> Result<(), ExecError> {
        if !set && self.event_facts(device, event).is_some_and(|e| e.waiters) {
            self.settle(device);
            self.update_event(device, event, |e| e.waiters = false);
        }
        self.pass_through(command)?;
        if generated::result_of(command) == Some(VK_SUCCESS) {
            self.update_event(device, event, |e| e.set = set);
            if set {
                self.release_held()?;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_event_use_is_summed_in_recording_order() {
        let mut u = EventUse::default();
        u.record(EventOp::Wait).unwrap();
        assert!(u.needs_set, "a wait before any set needs one from outside");
        u.record(EventOp::Reset).unwrap();
        u.record(EventOp::Set).unwrap();
        u.record(EventOp::Wait).unwrap();
        assert_eq!(
            u,
            EventUse {
                needs_set: true,
                resets: true,
                exit: Some(true)
            }
        );
        let mut split = EventUse::default();
        split.record(EventOp::Set).unwrap();
        split.record(EventOp::Wait).unwrap();
        assert!(!split.needs_set, "a split barrier inside one buffer");
        let mut bad = EventUse::default();
        bad.record(EventOp::Reset).unwrap();
        assert!(bad.record(EventOp::Wait).is_err());
    }

    #[test]
    fn a_secondary_s_use_follows_the_primary_s() {
        let mut primary = EventUse::default();
        primary.record(EventOp::Set).unwrap();
        let mut secondary = EventUse::default();
        secondary.record(EventOp::Wait).unwrap();
        primary.then(secondary).unwrap();
        assert!(!primary.needs_set, "the primary set it before");
        let mut reset = EventUse::default();
        reset.record(EventOp::Reset).unwrap();
        assert!(reset.then(secondary).is_err());
    }
}
