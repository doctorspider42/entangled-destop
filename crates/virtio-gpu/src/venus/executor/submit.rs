//! Queue submission, fences and waits (EPIC 20 stage 5b.2): `vkQueueSubmit`
//! and `vkQueueSubmit2` with command buffers and an optional binary fence,
//! the fence commands, `vkQueueWaitIdle` and `vkDeviceWaitIdle`, and what
//! keeps the host from freeing an object under the GPU.
//!
//! # What Mesa 26.0.8 sends (ADR-0004, 2026-09-24)
//!
//! * `vkQueueSubmit`, asynchronously; `vkQueueSubmit2` only with sync2,
//!   which the guest does not have yet. A fenced submit carries one more
//!   command buffer the driver appended: the fence's *feedback* command
//!   buffer, recorded once when the fence was created (a barrier, a
//!   `vkCmdFillBuffer` of `VK_SUCCESS` into the fence's slot of a
//!   host-visible feedback buffer, a barrier to `HOST`) and resubmitted
//!   unchanged. It is just a command buffer writing memory the guest maps:
//!   served by everything else here, nothing about it is special.
//! * No semaphore on a plain submit: Mesa adds one only to sparse binds.
//! * `vkWaitForFences` never as a call the guest waits on: the guest polls
//!   the fence's feedback slot, and once it reads signalled sends an
//!   **asynchronous** `vkWaitForFences(1, &fence, VK_TRUE, UINT64_MAX)` —
//!   so that the host, too, has seen the fence signalled before the guest
//!   resets or destroys it. The ring blocks on it; the guest does not.
//! * `vkQueueWaitIdle` and `vkDeviceWaitIdle` never: the guest does them
//!   with a fence of its own (vkr refuses both as blocking calls). They are
//!   served here anyway, with the pending-work record below.
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

use std::time::Duration;

use crate::venus::protocol::{
    Command, DeviceWaitIdleArgs, QueueWaitIdleArgs, VkDevice, VkFence, VkQueue, WaitForFencesArgs,
    VK_ERROR_UNKNOWN, VK_SUCCESS, VK_TIMEOUT,
};

use super::context::{id_error, invalid, ExecError, VulkanContext};
use super::generated;
use super::host::{HostVulkan, RawHandle};
use super::objects::{Facts, Kind, Pending};

#[cfg(doc)]
use super::ExecutingSink;

/// The longest one slice of a wait holds the context lock: a guest's
/// `vkWaitForFences(UINT64_MAX)` on one ring holds up the others by no
/// more than this.
pub const WAIT_SLICE: Duration = Duration::from_millis(20);

/// How long [`VulkanContext::settle`] waits on a fence in one go before it
/// looks at the ring's stop signal again.
const SETTLE_SLICE: Duration = Duration::from_millis(100);

fn nanos(d: Duration) -> u64 {
    u64::try_from(d.as_nanos()).unwrap_or(u64::MAX)
}

/// Whether `command` is a wait the ring worker slices.
#[must_use]
pub fn is_wait(command: &Command<'_>) -> bool {
    matches!(
        command,
        Command::WaitForFences(_) | Command::QueueWaitIdle(_) | Command::DeviceWaitIdle(_)
    )
}

/// How long the guest let `command` wait: `None` for "until it is done"
/// (`UINT64_MAX`, and the idle waits, which have no timeout).
#[must_use]
pub fn wait_limit(command: &Command<'_>) -> Option<Duration> {
    match command {
        Command::WaitForFences(a) if a.timeout != u64::MAX => Some(Duration::from_nanos(a.timeout)),
        _ => None,
    }
}

/// Answer a wait that ran out of time.
pub fn time_out(command: &mut Command<'_>) {
    if let Command::WaitForFences(a) = command {
        a.ret = VK_TIMEOUT;
    }
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

    /// One slice of `vkWaitForFences`, `vkQueueWaitIdle` or
    /// `vkDeviceWaitIdle`, at most `slice` long: `Ok(true)` once the command
    /// is answered, `Ok(false)` if the slice ran out first. Every id is
    /// translated afresh each slice, so a fence destroyed between two of them
    /// is refused, never waited on.
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

    /// `vkQueueSubmit` / `vkQueueSubmit2`: command buffers (primary, of the
    /// queue's device) and an optional binary fence; a semaphore anywhere is
    /// stage 5b.3 and refused. A submit that went in updates the queue's
    /// record of pending work.
    pub(super) fn queue_submit(&mut self, command: &mut Command<'_>) -> Result<(), ExecError> {
        let name = command.name();
        let (queue, fence, buffers, semaphores, batches) = match command {
            Command::QueueSubmit(a) => {
                let submits = a.p_submits.as_deref().unwrap_or_default();
                (
                    a.queue.0,
                    a.fence.0,
                    submits
                        .iter()
                        .flat_map(|s| s.p_command_buffers.iter().flatten().map(|c| c.0))
                        .collect::<Vec<_>>(),
                    submits
                        .iter()
                        .map(|s| {
                            u64::from(s.wait_semaphore_count) + u64::from(s.signal_semaphore_count)
                        })
                        .sum::<u64>(),
                    submits.len(),
                )
            }
            Command::QueueSubmit2(a) => {
                let submits = a.p_submits.as_deref().unwrap_or_default();
                (
                    a.queue.0,
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
                    submits
                        .iter()
                        .map(|s| {
                            u64::from(s.wait_semaphore_info_count)
                                + u64::from(s.signal_semaphore_info_count)
                        })
                        .sum::<u64>(),
                    submits.len(),
                )
            }
            other => {
                return Err(ExecError::NotImplemented {
                    command: other.name(),
                })
            }
        };
        if semaphores > 0 {
            tracing::warn!(
                ctx_id = self.ctx_id,
                command = name,
                semaphores,
                "a Venus submit names semaphores, which are stage 5b.3; the context ends here"
            );
            return Err(ExecError::Semaphore(name));
        }
        let device = self.objects.queue(queue).map_err(id_error(name))?.device;
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
}
