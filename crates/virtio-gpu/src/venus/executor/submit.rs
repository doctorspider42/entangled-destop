//! Queue submission, fences, semaphores and waits (EPIC 20 stages 5b.2 and
//! 5b.3): `vkQueueSubmit` and `vkQueueSubmit2` with command buffers,
//! semaphores and an optional fence, the fence and semaphore commands, the
//! sync-file emulation Mesa's WSI needs, `vkQueueWaitIdle` and
//! `vkDeviceWaitIdle`, virtio-gpu fences on a queue's timeline, and what
//! keeps the host from freeing an object under the GPU.
//!
//! # What Mesa 26.0.8 sends (ADR-0004, 2026-09-24)
//!
//! * `vkQueueSubmit`, asynchronously — `vkQueueSubmit2` directly once the
//!   renderer's device is 1.3 (`vn_device.c:554`, `dev->has_sync2`). A
//!   fenced submit carries one more command buffer the driver appended: the
//!   fence's *feedback* command buffer, recorded once when the fence was
//!   created and resubmitted unchanged. A submit that signals a timeline
//!   semaphore carries one per semaphore likewise (`vn_feedback.c`, a copy of
//!   the value into the semaphore's slot). They are just command buffers
//!   writing memory the guest maps: nothing about them is special here.
//! * `vkWaitForFences` and `vkWaitSemaphores` never as calls the guest waits
//!   on: the guest polls the feedback slot and, once it reads signalled,
//!   sends an **asynchronous** wait with `UINT64_MAX` — so that the host, too,
//!   has seen the fence or the value before the guest reuses it. The ring
//!   blocks on it; the guest does not. `vkGetSemaphoreCounterValue` and
//!   `vkGetFenceStatus` as calls, only while feedback is off.
//! * `vkCreateSemaphore` (binary, or timeline through
//!   `VkSemaphoreTypeCreateInfo`, with `VkExportSemaphoreCreateInfo` when
//!   the application asked for an exportable one), `vkDestroySemaphore`,
//!   `vkSignalSemaphore`: asynchronously.
//! * On the WSI path: `vkImportSemaphoreResourceMESA` with resource 0 for a
//!   semaphore whose temporary payload was an imported sync file the guest
//!   has already waited for itself (`vn_queue.c:387-417`), right before the
//!   submit that waits on it; and `vkWaitSemaphoreResourceMESA` when a
//!   semaphore is exported as a sync file (`vn_queue.c:2440-2495`), which a
//!   guest can only do when the renderer says `SYNC_FD` is exportable — this
//!   one does not (`policy::external_semaphore_properties`) — but is served
//!   anyway. `vkImportFenceResourceMESA` is never sent, and
//!   `vkResetFenceResourceMESA` only with an exportable sync-file fence,
//!   which is not advertised; both are refused.
//! * `vkQueueWaitIdle` and `vkDeviceWaitIdle` never: the guest does them
//!   with a fence of its own (vkr refuses both as blocking calls). They are
//!   served here anyway, with the pending-work record below.
//!
//! # Semaphores: what the host sees, and what it does not
//!
//! A timeline semaphore is the host's: its value, signals and waits go to
//! the driver as they are. A binary one is too, with two exceptions this
//! file tracks per semaphore ([`SemaphoreState`]):
//!
//! * **`vkImportSemaphoreResourceMESA(resource 0)` is a temporary import of
//!   an already-signalled payload** (vkr imports a sync file of `-1`). A
//!   Windows host has no sync file, and a signalling submit would get the
//!   semantics wrong — it would wait behind the queue's earlier work, change
//!   the *permanent* payload a temporary import must leave alone, and be a
//!   signal of an already-signalled semaphore when the permanent payload is.
//!   So it is recorded, not performed: the next wait on the semaphore
//!   consumes the temporary payload, and a submit's wait on one is taken out
//!   of the batch before the host sees it — which is exactly what waiting on
//!   a signalled temporary payload is: nothing to wait for, and the
//!   permanent payload restored.
//! * **`vkWaitSemaphoreResourceMESA` consumes the pending payload**: the
//!   temporary one if there is one (nothing reaches the host), otherwise the
//!   permanent one, through an empty submit that waits on it.
//!
//! The same record refuses, before the host sees them, a binary wait with no
//! signal submitted before it (a GPU that waits forever) and a signal of a
//! binary semaphore already signalled (a driver's invalid usage), and every
//! array a driver indexes by a submit's semaphore count — timeline values,
//! device-group indices — is checked against that count.
//!
//! # Waiting
//!
//! Every wait is a real wait on the host, in slices: the ring worker
//! ([`super::ExecutingSink`]) asks for one slice at a time with the context
//! lock released between them, so another ring of the context is never held
//! up by more than a slice, a ring being torn down stops waiting within one,
//! and the guest's own timeout — `UINT64_MAX` is "forever" — is honoured to
//! the slice. The context's monitor thread keeps setting `ALIVE` in the
//! ring's status meanwhile: it runs apart from the worker for exactly this.
//!
//! # Nothing is freed under the GPU
//!
//! Vulkan makes destroying an object a pending submission still uses the
//! application's mistake, and vkr lets the driver meet it. Here every queue
//! keeps a record of what it may still be running ([`Pending`]): the fence
//! of its newest fenced submit, which covers every earlier batch of the
//! queue, and whether an unfenced submit came after. Every destroy, free and
//! pool reset first waits for the record to clear ([`VulkanContext::settle`])
//! — the fence if that is all, the queue going idle if not — so the host
//! never frees an object while a submission may use it, at the price of a
//! wait only when the guest destroys something while work is in flight
//! (Mesa's own pattern — wait for the fence, then destroy — never waits).
//! Device teardown waits for the device to go idle first, as vkr does.
//!
//! # Virtio-gpu fences on a queue's timeline
//!
//! [`VulkanContext::create_ring_fence`]: an empty submit with a host fence
//! on the queue bound to the `ring_idx`, handed to that queue's fence thread
//! ([`super::timeline`], which has the waiter model and its ADR-0005
//! argument). Its host fence is the thread's alone, so it is not a
//! [`Pending`] record: `settle` never waits on a fence another thread may be
//! destroying.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use crate::renderer::FenceOutcome;
use crate::venus::protocol::{
    Command, CreateFenceArgs, DeviceWaitIdleArgs, ImportSemaphoreResourceMESAArgs, QueueSubmitArgs,
    QueueWaitIdleArgs, VkDevice, VkFence, VkFenceCreateInfo, VkQueue, VkSemaphore, VkSubmitInfo,
    VkSubmitInfoNext, WaitForFencesArgs, WaitSemaphoreResourceMESAArgs, VK_ERROR_DEVICE_LOST,
    VK_ERROR_UNKNOWN, VK_SUCCESS, VK_TIMEOUT,
};
use crate::venus::renderer::{FenceRetirer, RingFence};

use super::context::{id_error, invalid, ExecError, VulkanContext};
use super::generated;
use super::host::{HostVulkan, RawHandle};
use super::objects::{Facts, Kind, Pending, SemaphoreState};
use super::timeline::QueueSync;

#[cfg(doc)]
use super::ExecutingSink;

/// The longest one slice of a wait holds the context lock: a guest's
/// `vkWaitForFences(UINT64_MAX)` on one ring holds up the others by no
/// more than this.
pub const WAIT_SLICE: Duration = Duration::from_millis(20);

/// How long [`VulkanContext::settle`] waits on a fence in one go before it
/// looks at the ring's stop signal again.
const SETTLE_SLICE: Duration = Duration::from_millis(100);

/// `VK_PIPELINE_STAGE_ALL_COMMANDS_BIT`: what an emulated wait blocks.
const STAGE_ALL_COMMANDS: u32 = 0x1_0000;

fn nanos(d: Duration) -> u64 {
    u64::try_from(d.as_nanos()).unwrap_or(u64::MAX)
}

/// Whether `command` is a wait the ring worker slices.
#[must_use]
pub fn is_wait(command: &Command<'_>) -> bool {
    matches!(
        command,
        Command::WaitForFences(_)
            | Command::WaitSemaphores(_)
            | Command::QueueWaitIdle(_)
            | Command::DeviceWaitIdle(_)
    )
}

/// How long the guest let `command` wait: `None` for "until it is done"
/// (`UINT64_MAX`, and the idle waits, which have no timeout).
#[must_use]
pub fn wait_limit(command: &Command<'_>) -> Option<Duration> {
    match command {
        Command::WaitForFences(a) if a.timeout != u64::MAX => Some(Duration::from_nanos(a.timeout)),
        Command::WaitSemaphores(a) if a.timeout != u64::MAX => {
            Some(Duration::from_nanos(a.timeout))
        }
        _ => None,
    }
}

/// Answer a wait that ran out of time.
pub fn time_out(command: &mut Command<'_>) {
    match command {
        Command::WaitForFences(a) => a.ret = VK_TIMEOUT,
        Command::WaitSemaphores(a) => a.ret = VK_TIMEOUT,
        _ => {}
    }
}

/// A binary semaphore's wait in a batch: consumes a temporary payload
/// (`true`: the wait is taken out of the batch) or the pending signal.
fn consume_wait(
    command: &'static str,
    id: u64,
    state: &mut SemaphoreState,
) -> Result<bool, ExecError> {
    if state.timeline {
        return Ok(false);
    }
    if state.temporary {
        state.temporary = false;
        return Ok(true);
    }
    if state.pending {
        state.pending = false;
        return Ok(false);
    }
    Err(ExecError::Semaphore {
        command,
        id,
        what: "a binary wait with no signal submitted before it, which the GPU would wait on \
               forever",
    })
}

/// A binary semaphore's signal in a batch.
fn produce_signal(
    command: &'static str,
    id: u64,
    state: &mut SemaphoreState,
) -> Result<(), ExecError> {
    if state.timeline {
        return Ok(());
    }
    if state.pending || state.temporary {
        return Err(ExecError::Semaphore {
            command,
            id,
            what: "a signal of a binary semaphore that is already signalled",
        });
    }
    state.pending = true;
    Ok(())
}

/// `items` without the ones `drop` marks.
fn keep<T: Clone>(items: &[T], drop: &[bool]) -> Vec<T> {
    items
        .iter()
        .zip(drop)
        .filter(|(_, d)| !**d)
        .map(|(x, _)| x.clone())
        .collect()
}

fn count_of(len: usize) -> u32 {
    u32::try_from(len).unwrap_or(u32::MAX)
}

impl<H: HostVulkan> VulkanContext<H> {
    fn stopping(&self) -> bool {
        self.stop
            .as_ref()
            .is_some_and(super::super::service::StopSignal::is_stopping)
    }

    /// `vkWaitForFences` on host handles, for the executor's own waits.
    fn host_wait_fences(&mut self, device: u64, fences: &[u64], all: bool, timeout: u64) -> i32 {
        let mut command = Command::WaitForFences(WaitForFencesArgs {
            device: VkDevice(device),
            fence_count: u32::try_from(fences.len()).unwrap_or(u32::MAX),
            p_fences: Some(fences.iter().copied().map(VkFence).collect()),
            wait_all: u32::from(all),
            timeout,
            ret: VK_ERROR_UNKNOWN,
        });
        match self.host_call(device, &mut command) {
            Ok(()) => generated::result_of(&command).unwrap_or(VK_ERROR_UNKNOWN),
            Err(_) => VK_ERROR_UNKNOWN,
        }
    }

    /// `vkQueueWaitIdle` on host queue `queue` of `device`.
    fn host_queue_idle(&mut self, device: u64, queue: u64) -> i32 {
        let mut command = Command::QueueWaitIdle(QueueWaitIdleArgs {
            queue: VkQueue(queue),
            ret: VK_ERROR_UNKNOWN,
        });
        match self.host_call(device, &mut command) {
            Ok(()) => generated::result_of(&command).unwrap_or(VK_ERROR_UNKNOWN),
            Err(_) => VK_ERROR_UNKNOWN,
        }
    }

    /// Wait for the work of queue `queue` to be done, in one slice of
    /// `slice` at most when a fence stands for all of it. `Some(ret)` once
    /// it is (the record then cleared), `None` if the slice ran out.
    fn queue_idle_slice(&mut self, queue: u64, slice: Duration) -> Option<i32> {
        let (device, host, pending) = match self.objects.queue(queue) {
            Ok(q) => (q.device, q.host.raw(), q.pending),
            Err(_) => return Some(VK_ERROR_UNKNOWN),
        };
        let ret = match pending {
            Pending {
                fence: None,
                unfenced: false,
            } => VK_SUCCESS,
            Pending {
                fence: Some((_, fence)),
                unfenced: false,
            } => {
                let ret = self.host_wait_fences(device, &[fence], true, nanos(slice));
                if ret == VK_TIMEOUT {
                    return None;
                }
                ret
            }
            Pending { unfenced: true, .. } => self.host_queue_idle(device, host),
        };
        if let Ok(q) = self.objects.queue_mut(queue) {
            q.pending = Pending::default();
        }
        Some(ret)
    }

    /// Wait until nothing submitted to `device` may still be running —
    /// before anything that work may use is destroyed, freed or reset. See
    /// the module docs. Gives up (and lets the destroy go ahead) only when
    /// the ring is being torn down, whose context teardown waits for the
    /// whole device anyway, or when the device is lost, when the driver
    /// allows destroying everything.
    pub(super) fn settle(&mut self, device: u64) {
        for queue in self.objects.queues_of(device) {
            loop {
                match self.queue_idle_slice(queue, SETTLE_SLICE) {
                    Some(ret) => {
                        self.note_result("the executor's own wait", Some(ret));
                        break;
                    }
                    None if self.stopping() => return,
                    None => {}
                }
            }
        }
    }

    /// `vkResetFences`: a fence a queue's record stands on is waited for
    /// first — resetting one still pending is the guest's error, and would
    /// leave the record waiting on a fence nothing will signal.
    pub(super) fn reset_fences(&mut self, command: &mut Command<'_>) -> Result<(), ExecError> {
        let Command::ResetFences(args) = command else {
            return Err(ExecError::NotImplemented {
                command: command.name(),
            });
        };
        let device = args.device.0;
        let fences: Vec<u64> = args.p_fences.iter().flatten().map(|f| f.0).collect();
        let tracked = self.objects.queues_of(device).into_iter().any(|q| {
            self.objects
                .queue(q)
                .ok()
                .and_then(|q| q.pending.fence)
                .is_some_and(|(id, _)| fences.contains(&id))
        });
        if tracked {
            self.settle(device);
        }
        self.pass_through(command)
    }

    /// One slice of `vkWaitForFences`, `vkWaitSemaphores`, `vkQueueWaitIdle`
    /// or `vkDeviceWaitIdle`, at most `slice` long: `Ok(true)` once the
    /// command is answered, `Ok(false)` if the slice ran out first. Every id
    /// is translated afresh each slice, so an object destroyed between two of
    /// them is refused, never waited on.
    ///
    /// # Errors
    /// A refused id or value, fatal.
    pub fn wait_slice(
        &mut self,
        command: &mut Command<'_>,
        slice: Duration,
    ) -> Result<bool, ExecError> {
        match command {
            Command::WaitForFences(args) => {
                let device = args.device.0;
                let mut attempt = Command::WaitForFences(WaitForFencesArgs {
                    timeout: nanos(slice).min(args.timeout),
                    ..args.clone()
                });
                self.translate(device, &mut attempt)?;
                self.host_call(device, &mut attempt)?;
                let ret = generated::result_of(&attempt).unwrap_or(VK_ERROR_UNKNOWN);
                if ret == VK_TIMEOUT && args.timeout > 0 {
                    return Ok(false);
                }
                args.ret = ret;
                if ret == VK_SUCCESS && (args.wait_all != 0 || args.fence_count == 1) {
                    // The host has seen these fences signalled: a queue whose
                    // record stood on one of them has nothing left before it.
                    let fences: Vec<u64> = args.p_fences.iter().flatten().map(|f| f.0).collect();
                    for queue in self.objects.queues_of(device) {
                        if let Ok(q) = self.objects.queue_mut(queue) {
                            if !q.pending.unfenced
                                && q.pending.fence.is_some_and(|(id, _)| fences.contains(&id))
                            {
                                q.pending = Pending::default();
                            }
                        }
                    }
                }
                Ok(true)
            }
            Command::WaitSemaphores(args) => {
                const NAME: &str = "vkWaitSemaphores";
                let device = args.device.0;
                if let Some(info) = &args.p_wait_info {
                    for id in info.p_semaphores.iter().flatten() {
                        self.require_timeline(NAME, device, id.0)?;
                    }
                }
                let mut attempt =
                    Command::WaitSemaphores(crate::venus::protocol::WaitSemaphoresArgs {
                        timeout: nanos(slice).min(args.timeout),
                        ..args.clone()
                    });
                self.translate(device, &mut attempt)?;
                self.host_call(device, &mut attempt)?;
                let ret = generated::result_of(&attempt).unwrap_or(VK_ERROR_UNKNOWN);
                if ret == VK_TIMEOUT && args.timeout > 0 {
                    return Ok(false);
                }
                args.ret = ret;
                Ok(true)
            }
            Command::QueueWaitIdle(args) => {
                self.objects
                    .queue(args.queue.0)
                    .map_err(id_error("vkQueueWaitIdle"))?;
                match self.queue_idle_slice(args.queue.0, slice) {
                    Some(ret) => {
                        args.ret = ret;
                        self.note_result("vkQueueWaitIdle", Some(ret));
                        Ok(true)
                    }
                    None => Ok(false),
                }
            }
            Command::DeviceWaitIdle(args) => {
                let device = args.device.0;
                self.objects
                    .device(device)
                    .map_err(id_error("vkDeviceWaitIdle"))?;
                let queues = self.objects.queues_of(device);
                let unfenced = queues
                    .iter()
                    .any(|q| self.objects.queue(*q).is_ok_and(|q| q.pending.unfenced));
                if unfenced {
                    let mut idle = Command::DeviceWaitIdle(DeviceWaitIdleArgs {
                        device: VkDevice(device),
                        ret: VK_ERROR_UNKNOWN,
                    });
                    self.host_call(device, &mut idle)?;
                    args.ret = generated::result_of(&idle).unwrap_or(VK_ERROR_UNKNOWN);
                    for queue in queues {
                        if let Ok(q) = self.objects.queue_mut(queue) {
                            q.pending = Pending::default();
                        }
                    }
                    return Ok(true);
                }
                let mut ret = VK_SUCCESS;
                for queue in queues {
                    match self.queue_idle_slice(queue, slice) {
                        Some(r) if r != VK_SUCCESS => ret = r,
                        Some(_) => {}
                        None => return Ok(false),
                    }
                }
                args.ret = ret;
                self.note_result("vkDeviceWaitIdle", Some(ret));
                Ok(true)
            }
            other => Err(ExecError::NotImplemented {
                command: other.name(),
            }),
        }
    }

    // ---------------------------------------------------------- semaphores

    /// What the executor knows of semaphore `id` of `device`.
    pub(super) fn semaphore_state(
        &self,
        command: &'static str,
        device: u64,
        id: u64,
    ) -> Result<SemaphoreState, ExecError> {
        match self
            .objects
            .raw(Kind::Semaphore, device, id)
            .map_err(id_error(command))?
            .facts
        {
            Facts::Semaphore(state) => Ok(state),
            _ => Err(invalid(command, "not a semaphore")),
        }
    }

    /// A semaphore's state as a submit being planned has left it.
    fn state_in(
        &self,
        states: &HashMap<u64, SemaphoreState>,
        command: &'static str,
        device: u64,
        id: u64,
    ) -> Result<SemaphoreState, ExecError> {
        match states.get(&id) {
            Some(state) => Ok(*state),
            None => self.semaphore_state(command, device, id),
        }
    }

    fn set_semaphore_state(&mut self, device: u64, id: u64, state: SemaphoreState) {
        if let Ok(object) = self.objects.raw_mut(Kind::Semaphore, device, id) {
            object.facts = Facts::Semaphore(state);
        }
    }

    /// A timeline-only command (`vkSignalSemaphore`, `vkWaitSemaphores`,
    /// `vkGetSemaphoreCounterValue`) must name a timeline semaphore.
    pub(super) fn require_timeline(
        &self,
        command: &'static str,
        device: u64,
        id: u64,
    ) -> Result<(), ExecError> {
        if self.semaphore_state(command, device, id)?.timeline {
            Ok(())
        } else {
            Err(ExecError::Semaphore {
                command,
                id,
                what: "a timeline semaphore's command on a binary semaphore",
            })
        }
    }

    /// `vkCreateSemaphore` (stage 5b.3): binary, or timeline through
    /// `VkSemaphoreTypeCreateInfo`. A `VkExportSemaphoreCreateInfo` loses
    /// `SYNC_FD`, which is this renderer's to emulate and which a Windows
    /// driver has not got; any other handle type in it must be one the host
    /// driver can export, and is passed through.
    pub(super) fn create_semaphore(&mut self, command: &mut Command<'_>) -> Result<(), ExecError> {
        use crate::venus::protocol::VkSemaphoreCreateInfoNext as N;
        const NAME: &str = "vkCreateSemaphore";
        let Command::CreateSemaphore(args) = command else {
            return Err(ExecError::NotImplemented {
                command: command.name(),
            });
        };
        let device = args.device.0;
        let Some(info) = args.p_create_info.as_mut() else {
            return Err(invalid(NAME, "pCreateInfo is null"));
        };
        let mut timeline = false;
        for link in &info.p_next {
            if let N::VkSemaphoreTypeCreateInfo(t) = link {
                match t.semaphore_type {
                    super::policy::SEMAPHORE_TYPE_BINARY => {}
                    super::policy::SEMAPHORE_TYPE_TIMELINE => timeline = true,
                    other => return Err(invalid(NAME, format!("semaphore type {other}"))),
                }
            }
        }
        let mut exported = 0u32;
        for link in &mut info.p_next {
            if let N::VkExportSemaphoreCreateInfo(e) = link {
                e.handle_types &= !super::policy::SEMAPHORE_HANDLE_SYNC_FD;
                exported |= e.handle_types;
            }
        }
        info.p_next
            .retain(|l| !matches!(l, N::VkExportSemaphoreCreateInfo(e) if e.handle_types == 0));
        if exported != 0 {
            let (physical, _) = {
                let d = self.objects.device(device).map_err(id_error(NAME))?;
                (d.physical, ())
            };
            let (instance, exposed) = self.objects.physical(physical).map_err(id_error(NAME))?;
            let mut exportable = 0u32;
            for bit in (0..32).map(|b| 1u32 << b).filter(|b| exported & b != 0) {
                let props =
                    self.host
                        .external_semaphore_properties(instance, exposed.host, bit, timeline);
                if props.external_semaphore_features & super::policy::SEMAPHORE_FEATURE_EXPORTABLE
                    != 0
                {
                    exportable |= bit;
                }
            }
            if exportable != exported {
                return Err(invalid(
                    NAME,
                    format!("export handle types {exported:#x}, of which the host exports {exportable:#x}"),
                ));
            }
        }
        let state = SemaphoreState {
            timeline,
            ..SemaphoreState::default()
        };
        self.create(command, Kind::Semaphore, Facts::Semaphore(state), 0)
    }

    /// `vkImportSemaphoreResourceMESA` (stage 5b.3): with resource 0, a
    /// temporary import of a signalled payload, recorded and not performed
    /// (see the module docs). Any other resource would be a sync file or
    /// dma-buf to import, which nothing here has; vkr only ever sees 0.
    fn import_semaphore_resource(
        &mut self,
        args: &ImportSemaphoreResourceMESAArgs,
    ) -> Result<(), ExecError> {
        const NAME: &str = "vkImportSemaphoreResourceMESA";
        let device = args.device.0;
        self.objects.device(device).map_err(id_error(NAME))?;
        let Some(info) = &args.p_import_semaphore_resource_info else {
            return Err(invalid(NAME, "pImportSemaphoreResourceInfo is null"));
        };
        if info.resource_id != 0 {
            return Err(invalid(
                NAME,
                format!(
                    "resource {} would be a sync file to import, which this host does not have",
                    info.resource_id
                ),
            ));
        }
        let id = info.semaphore.0;
        let mut state = self.semaphore_state(NAME, device, id)?;
        if state.timeline {
            return Err(ExecError::Semaphore {
                command: NAME,
                id,
                what: "a sync file import into a timeline semaphore (a sync file is binary)",
            });
        }
        state.temporary = true;
        self.set_semaphore_state(device, id, state);
        Ok(())
    }

    /// `vkWaitSemaphoreResourceMESA` (stage 5b.3): consume the semaphore's
    /// pending payload, as a sync-file export does — the temporary one, if
    /// there is one, without the host; otherwise the permanent one, with an
    /// empty submit waiting on it on the device's first queue.
    fn wait_semaphore_resource(
        &mut self,
        args: &WaitSemaphoreResourceMESAArgs,
    ) -> Result<(), ExecError> {
        const NAME: &str = "vkWaitSemaphoreResourceMESA";
        let device = args.device.0;
        self.objects.device(device).map_err(id_error(NAME))?;
        let id = args.semaphore.0;
        let mut state = self.semaphore_state(NAME, device, id)?;
        if state.timeline {
            return Err(ExecError::Semaphore {
                command: NAME,
                id,
                what: "a sync file export of a timeline semaphore (a sync file is binary)",
            });
        }
        if consume_wait(NAME, id, &mut state)? {
            self.set_semaphore_state(device, id, state);
            return Ok(());
        }
        let queue = self
            .objects
            .queues_of(device)
            .first()
            .copied()
            .ok_or_else(|| invalid(NAME, "the device has no queue the guest fetched"))?;
        let host_queue = self
            .objects
            .queue(queue)
            .map_err(id_error(NAME))?
            .host
            .raw();
        let host_semaphore = self
            .objects
            .raw(Kind::Semaphore, device, id)
            .map_err(id_error(NAME))?
            .host;
        let mut submit = Command::QueueSubmit(QueueSubmitArgs {
            queue: VkQueue(host_queue),
            submit_count: 1,
            p_submits: Some(vec![VkSubmitInfo {
                p_next: Vec::new(),
                wait_semaphore_count: 1,
                p_wait_semaphores: Some(vec![VkSemaphore(host_semaphore)]),
                p_wait_dst_stage_mask: Some(vec![STAGE_ALL_COMMANDS]),
                command_buffer_count: 0,
                p_command_buffers: None,
                signal_semaphore_count: 0,
                p_signal_semaphores: None,
            }]),
            fence: VkFence(0),
            ret: VK_ERROR_UNKNOWN,
        });
        self.host_call(device, &mut submit)?;
        if generated::result_of(&submit) == Some(VK_SUCCESS) {
            self.set_semaphore_state(device, id, state);
            if let Ok(q) = self.objects.queue_mut(queue) {
                q.pending.unfenced = true;
            }
        }
        Ok(())
    }

    /// The venus protocol's own commands that reach a device, and a refusal
    /// for every other one the protocol decodes (stage 5b.3).
    pub(super) fn dispatch_extension(
        &mut self,
        command: &mut Command<'_>,
    ) -> Result<(), ExecError> {
        match command {
            Command::ImportSemaphoreResourceMESA(args) => self.import_semaphore_resource(args),
            Command::WaitSemaphoreResourceMESA(args) => self.wait_semaphore_resource(args),
            // Stage 5c: the dma-buf import's query (`executor::memory`).
            Command::GetMemoryResourcePropertiesMESA(args) => self.memory_resource_properties(args),
            // Stage S1: the emulated modifier extension's one command
            // (`executor::modifier`), answered here, never by the host.
            Command::GetImageDrmFormatModifierPropertiesEXT(args) => {
                self.image_modifier_properties(args)
            }
            // `vkResetFenceResourceMESA` only follows a sync-file fence
            // export, which needs `VK_KHR_external_fence_fd` — not
            // advertised — and `vkImportFenceResourceMESA` is sent by no
            // Mesa release: both are refused as unimplemented, with every
            // other command no stage serves.
            other => Err(ExecError::NotImplemented {
                command: other.name(),
            }),
        }
    }

    // ---------------------------------------------------------- submission

    /// The size of the device group of `device` (1 without one).
    fn group_size(&self, command: &'static str, device: u64) -> Result<u32, ExecError> {
        Ok(self
            .objects
            .device(device)
            .map_err(id_error(command))?
            .group_size
            .max(1))
    }

    /// One `VkSubmitInfo`: its counts against the arrays a driver indexes by
    /// them, its semaphores against their state (`states`, updated), and the
    /// waits a temporary payload satisfies taken out of it.
    fn plan_submit(
        &self,
        name: &'static str,
        device: u64,
        group: u32,
        submit: &mut VkSubmitInfo,
        states: &mut HashMap<u64, SemaphoreState>,
    ) -> Result<(), ExecError> {
        let waits: Vec<u64> = submit
            .p_wait_semaphores
            .iter()
            .flatten()
            .map(|s| s.0)
            .collect();
        let signals: Vec<u64> = submit
            .p_signal_semaphores
            .iter()
            .flatten()
            .map(|s| s.0)
            .collect();
        let mut wait_timeline = false;
        for id in &waits {
            wait_timeline |= self.state_in(states, name, device, *id)?.timeline;
        }
        let mut signal_timeline = false;
        for id in &signals {
            signal_timeline |= self.state_in(states, name, device, *id)?.timeline;
        }
        for link in &submit.p_next {
            match link {
                VkSubmitInfoNext::VkTimelineSemaphoreSubmitInfo(t) => {
                    if (wait_timeline && t.wait_semaphore_value_count != count_of(waits.len()))
                        || (signal_timeline
                            && t.signal_semaphore_value_count != count_of(signals.len()))
                    {
                        return Err(invalid(
                            name,
                            "timeline semaphore values that are not one per semaphore",
                        ));
                    }
                }
                VkSubmitInfoNext::VkDeviceGroupSubmitInfo(g) => {
                    if g.wait_semaphore_count != count_of(waits.len())
                        || g.command_buffer_count != submit.command_buffer_count
                        || g.signal_semaphore_count != count_of(signals.len())
                    {
                        return Err(invalid(
                            name,
                            "device-group indices that are not one per semaphore and command buffer",
                        ));
                    }
                    let all = 1u32.checked_shl(group).map_or(u32::MAX, |b| b - 1);
                    let indices = g
                        .p_wait_semaphore_device_indices
                        .iter()
                        .flatten()
                        .chain(g.p_signal_semaphore_device_indices.iter().flatten());
                    if indices.clone().any(|i| *i >= group)
                        || g.p_command_buffer_device_masks
                            .iter()
                            .flatten()
                            .any(|m| *m & !all != 0)
                    {
                        return Err(invalid(
                            name,
                            "a device index or mask outside the device group",
                        ));
                    }
                }
                VkSubmitInfoNext::VkProtectedSubmitInfo(_) => {}
            }
        }
        if (wait_timeline || signal_timeline)
            && !submit
                .p_next
                .iter()
                .any(|l| matches!(l, VkSubmitInfoNext::VkTimelineSemaphoreSubmitInfo(_)))
        {
            return Err(invalid(
                name,
                "a timeline semaphore without VkTimelineSemaphoreSubmitInfo",
            ));
        }
        let mut dropped = vec![false; waits.len()];
        for (index, id) in waits.iter().enumerate() {
            let mut state = self.state_in(states, name, device, *id)?;
            if let Some(slot) = dropped.get_mut(index) {
                *slot = consume_wait(name, *id, &mut state)?;
            }
            states.insert(*id, state);
        }
        for id in &signals {
            let mut state = self.state_in(states, name, device, *id)?;
            produce_signal(name, *id, &mut state)?;
            states.insert(*id, state);
        }
        if dropped.iter().any(|d| *d) {
            let before = count_of(waits.len());
            let rest = count_of(dropped.iter().filter(|d| !**d).count());
            if let Some(v) = submit.p_wait_semaphores.as_mut() {
                *v = keep(v, &dropped);
            }
            if let Some(v) = submit.p_wait_dst_stage_mask.as_mut() {
                *v = keep(v, &dropped);
            }
            submit.wait_semaphore_count = rest;
            for link in &mut submit.p_next {
                match link {
                    VkSubmitInfoNext::VkTimelineSemaphoreSubmitInfo(t)
                        if t.wait_semaphore_value_count == before =>
                    {
                        if let Some(v) = t.p_wait_semaphore_values.as_mut() {
                            *v = keep(v, &dropped);
                        }
                        t.wait_semaphore_value_count = rest;
                    }
                    VkSubmitInfoNext::VkDeviceGroupSubmitInfo(g) => {
                        if let Some(v) = g.p_wait_semaphore_device_indices.as_mut() {
                            *v = keep(v, &dropped);
                        }
                        g.wait_semaphore_count = rest;
                    }
                    _ => {}
                }
            }
        }
        Ok(())
    }

    /// One `VkSubmitInfo2`, as [`Self::plan_submit`].
    fn plan_submit2(
        &self,
        name: &'static str,
        device: u64,
        group: u32,
        submit: &mut crate::venus::protocol::VkSubmitInfo2,
        states: &mut HashMap<u64, SemaphoreState>,
    ) -> Result<(), ExecError> {
        let all = 1u32.checked_shl(group).map_or(u32::MAX, |b| b - 1);
        let semaphores = submit
            .p_wait_semaphore_infos
            .iter()
            .flatten()
            .chain(submit.p_signal_semaphore_infos.iter().flatten());
        if semaphores.clone().any(|s| s.device_index >= group)
            || submit
                .p_command_buffer_infos
                .iter()
                .flatten()
                .any(|c| c.device_mask & !all != 0)
        {
            return Err(invalid(
                name,
                "a device index or mask outside the device group",
            ));
        }
        let waits: Vec<u64> = submit
            .p_wait_semaphore_infos
            .iter()
            .flatten()
            .map(|s| s.semaphore.0)
            .collect();
        let mut dropped = vec![false; waits.len()];
        for (index, id) in waits.iter().enumerate() {
            let mut state = self.state_in(states, name, device, *id)?;
            if let Some(slot) = dropped.get_mut(index) {
                *slot = consume_wait(name, *id, &mut state)?;
            }
            states.insert(*id, state);
        }
        for info in submit.p_signal_semaphore_infos.iter().flatten() {
            let id = info.semaphore.0;
            let mut state = self.state_in(states, name, device, id)?;
            produce_signal(name, id, &mut state)?;
            states.insert(id, state);
        }
        if dropped.iter().any(|d| *d) {
            if let Some(v) = submit.p_wait_semaphore_infos.as_mut() {
                *v = keep(v, &dropped);
                submit.wait_semaphore_info_count = count_of(v.len());
            }
        }
        Ok(())
    }

    /// `vkQueueSubmit` / `vkQueueSubmit2`: command buffers (primary, of the
    /// queue's device), semaphores (stage 5b.3, see the module docs) and an
    /// optional fence. A submit that went in updates the queue's record of
    /// pending work and the semaphores' state.
    pub(super) fn queue_submit(&mut self, command: &mut Command<'_>) -> Result<(), ExecError> {
        let name = command.name();
        let queue = match command {
            Command::QueueSubmit(a) => a.queue.0,
            Command::QueueSubmit2(a) => a.queue.0,
            other => {
                return Err(ExecError::NotImplemented {
                    command: other.name(),
                })
            }
        };
        let device = self.objects.queue(queue).map_err(id_error(name))?.device;
        let group = self.group_size(name, device)?;
        let mut states: HashMap<u64, SemaphoreState> = HashMap::new();
        let (fence, buffers, batches) = match command {
            Command::QueueSubmit(a) => {
                for submit in a.p_submits.iter_mut().flatten() {
                    self.plan_submit(name, device, group, submit, &mut states)?;
                }
                let submits = a.p_submits.as_deref().unwrap_or_default();
                (
                    a.fence.0,
                    submits
                        .iter()
                        .flat_map(|s| s.p_command_buffers.iter().flatten().map(|c| c.0))
                        .collect::<Vec<_>>(),
                    submits.len(),
                )
            }
            Command::QueueSubmit2(a) => {
                for submit in a.p_submits.iter_mut().flatten() {
                    self.plan_submit2(name, device, group, submit, &mut states)?;
                }
                let submits = a.p_submits.as_deref().unwrap_or_default();
                (
                    a.fence.0,
                    submits
                        .iter()
                        .flat_map(|s| {
                            s.p_command_buffer_infos
                                .iter()
                                .flatten()
                                .map(|c| c.command_buffer.0)
                        })
                        .collect::<Vec<_>>(),
                    submits.len(),
                )
            }
            other => {
                return Err(ExecError::NotImplemented {
                    command: other.name(),
                })
            }
        };
        for cb in &buffers {
            let object = self
                .objects
                .raw(Kind::CommandBuffer, device, *cb)
                .map_err(id_error(name))?;
            if object.facts != (Facts::CommandBuffer { secondary: false }) {
                return Err(invalid(
                    name,
                    format!("{cb:#x} is not a primary command buffer"),
                ));
            }
        }
        let fence_host = match fence {
            0 => None,
            id => Some(
                self.objects
                    .raw(Kind::Fence, device, id)
                    .map_err(id_error(name))?
                    .host,
            ),
        };
        self.pass_through(command)?;
        if generated::result_of(command) == Some(VK_SUCCESS) {
            for (id, state) in states {
                self.set_semaphore_state(device, id, state);
            }
            let q = self.objects.queue_mut(queue).map_err(id_error(name))?;
            match fence_host {
                Some(host) => {
                    q.pending = Pending {
                        fence: Some((fence, host)),
                        unfenced: false,
                    }
                }
                None if batches > 0 => q.pending.unfenced = true,
                None => {}
            }
        }
        Ok(())
    }

    // ------------------------------------------------------- ring fences

    /// A virtio-gpu fence on `fence.ring_idx` (stage 5b.3): an empty submit
    /// with a host fence on the queue bound to it, handed to the queue's
    /// fence thread ([`super::timeline`]), which retires it through `retire`
    /// once the queue's work before it is done. A context already fatal runs
    /// nothing more, and a lost device will never signal: both answer
    /// signalled.
    ///
    /// # Errors
    /// No queue bound to the timeline (vkr refuses it the same way), a
    /// queue with [`super::timeline::MAX_RING_FENCES_PER_QUEUE`] fences
    /// waiting, a fence thread that could not start, or a driver that
    /// refused the fence or the submit.
    pub fn create_ring_fence(
        &mut self,
        fence: RingFence,
        retire: &FenceRetirer,
    ) -> Result<FenceOutcome, String> {
        if self.fatal {
            return Ok(FenceOutcome::Signalled);
        }
        let queue = self
            .objects
            .queue_on_ring(u32::from(fence.ring_idx))
            .ok_or_else(|| format!("no VkQueue is bound to ring_idx {}", fence.ring_idx))?;
        let (device, host_queue) = {
            let q = self.objects.queue(queue).map_err(|e| e.to_string())?;
            if q.sync.as_ref().is_some_and(|s| !s.has_room()) {
                return Err(format!(
                    "{} fences already wait on ring_idx {}",
                    super::timeline::MAX_RING_FENCES_PER_QUEUE,
                    fence.ring_idx
                ));
            }
            (q.device, q.host.raw())
        };
        // The thread first: once the submit is in, the fence must have a
        // waiter, or it could only be destroyed after a device-wide wait.
        if self
            .objects
            .queue(queue)
            .map_err(|e| e.to_string())?
            .sync
            .is_none()
        {
            let handle = Arc::clone(&self.objects.device(device).map_err(|e| e.to_string())?.host);
            let slot = self.fence_threads.take().ok_or_else(|| {
                format!(
                    "the host already runs the {} fence threads it allows every guest process \
                     together",
                    self.fence_threads.limit()
                )
            })?;
            let sync = QueueSync::spawn(
                Arc::clone(&self.host),
                handle,
                retire.clone(),
                self.ctx_id,
                fence.ring_idx,
                slot,
            )
            .map_err(|e| {
                format!(
                    "the fence thread of ring_idx {} did not start: {e}",
                    fence.ring_idx
                )
            })?;
            self.objects
                .queue_mut(queue)
                .map_err(|e| e.to_string())?
                .sync = Some(sync);
        }
        let mut create = Command::CreateFence(CreateFenceArgs {
            device: VkDevice(0),
            p_create_info: Some(VkFenceCreateInfo {
                p_next: Vec::new(),
                flags: 0,
            }),
            p_fence: Some(VkFence(0)),
            ret: VK_ERROR_UNKNOWN,
        });
        self.host_call(device, &mut create)
            .map_err(|e| e.to_string())?;
        let host_fence = match (&create, generated::result_of(&create)) {
            (Command::CreateFence(a), Some(VK_SUCCESS)) => a.p_fence.map_or(0, |f| f.0),
            (_, ret) => return Err(format!("vkCreateFence failed ({ret:?})")),
        };
        let mut submit = Command::QueueSubmit(QueueSubmitArgs {
            queue: VkQueue(host_queue),
            submit_count: 0,
            p_submits: None,
            fence: VkFence(host_fence),
            ret: VK_ERROR_UNKNOWN,
        });
        let submitted = self
            .host_call(device, &mut submit)
            .ok()
            .and_then(|()| generated::result_of(&submit));
        let destroy = |ctx: &Self| {
            if let Ok(d) = ctx.objects.device(device) {
                ctx.host.destroy_object(&d.host, Kind::Fence, host_fence);
            }
        };
        match submitted {
            Some(VK_SUCCESS) => {}
            Some(VK_ERROR_DEVICE_LOST) => {
                destroy(self);
                return Ok(FenceOutcome::Signalled);
            }
            ret => {
                destroy(self);
                return Err(format!("the fence's vkQueueSubmit failed ({ret:?})"));
            }
        }
        let pushed = self
            .objects
            .queue(queue)
            .ok()
            .and_then(|q| q.sync.as_ref())
            .is_some_and(|s| s.push(fence.fence_id, host_fence));
        if !pushed {
            // Cannot happen: room was checked and only a device teardown,
            // which takes the thread first, stops one. Wait it out rather
            // than destroy a fence a submit still owns.
            let _ = self.host_wait_fences(device, &[host_fence], true, u64::MAX);
            destroy(self);
            return Ok(FenceOutcome::Signalled);
        }
        Ok(FenceOutcome::Pending)
    }
}
