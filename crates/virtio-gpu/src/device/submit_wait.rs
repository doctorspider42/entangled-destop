//! CPU-side Venus dependencies that precede submission of a GPU fence.

use std::sync::{Arc, Mutex, PoisonError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use virtio_core::chain::Segment;
use virtio_core::{DeviceError, GuestMem, HostWaker};
use virtio_queue::{Queue, QueueT};

use super::{GpuDevice, PendingResponse, Reply, Served};
use crate::protocol::{cmd, resp, CmdSubmit3d, CtrlHdr, FLAG_INFO_RING_IDX};
use crate::renderer::SubmitWait;
use crate::sink::ScanoutSink;

pub(super) struct HeldWait {
    head: u16,
    hdr: CtrlHdr,
    writable: Vec<Segment>,
    wait: SubmitWait,
    deferred_at: Instant,
}

impl HeldWait {
    pub(super) fn failed(self) -> PendingResponse {
        PendingResponse {
            head: self.head,
            hdr: self.hdr.response(resp::ERR_UNSPEC),
            writable: self.writable,
            body: Vec::new(),
        }
    }
}

#[derive(PartialEq, Eq)]
enum Deadline {
    Idle,
    At(Instant),
    Stop,
}

/// One timer for the control queue, created on demand. It only asks for a
/// queue wake; it cannot access guest RAM or write a response while paused.
pub(super) struct DeadlineWake {
    next: Arc<Mutex<Deadline>>,
    thread: Option<JoinHandle<()>>,
}

impl DeadlineWake {
    fn start(waker: Arc<dyn HostWaker>) -> std::io::Result<Self> {
        let next = Arc::new(Mutex::new(Deadline::Idle));
        let task = Arc::clone(&next);
        let thread = thread::Builder::new()
            .name("venus-wait-deadline".into())
            .spawn(move || loop {
                let deadline = {
                    let mut state = task.lock().unwrap_or_else(PoisonError::into_inner);
                    match *state {
                        Deadline::Stop => return,
                        Deadline::Idle => None,
                        Deadline::At(at) => {
                            let delay = at.saturating_duration_since(Instant::now());
                            if delay.is_zero() {
                                *state = Deadline::Idle;
                            }
                            Some(delay)
                        }
                    }
                };
                if let Some(delay) = deadline {
                    if delay.is_zero() {
                        waker.wake();
                    } else {
                        thread::park_timeout(delay);
                    }
                } else {
                    thread::park();
                }
            })?;
        Ok(Self {
            next,
            thread: Some(thread),
        })
    }

    fn set(&self, state: Deadline) {
        let mut next = self.next.lock().unwrap_or_else(PoisonError::into_inner);
        if *next == state {
            return;
        }
        *next = state;
        drop(next);
        if let Some(thread) = &self.thread {
            thread.thread().unpark();
        }
    }
}

impl Drop for DeadlineWake {
    fn drop(&mut self) {
        self.set(Deadline::Stop);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl<S: ScanoutSink> GpuDevice<S> {
    pub(super) fn try_defer_wait(
        &mut self,
        head: u16,
        hdr: CtrlHdr,
        request: &[u8],
        writable: &[Segment],
    ) -> bool {
        if !self.deferred_ring_waits
            || !self.fence_mode.is_deferred()
            || hdr.kind != cmd::SUBMIT_3D
            || !hdr.wants_fence()
            || hdr.flags & FLAG_INFO_RING_IDX == 0
            || !(1..64).contains(&hdr.ring_idx)
            || self.fence_slots_full()
        {
            return false;
        }
        let Some(waker) = self.waker.as_ref().filter(|waker| waker.can_wake()) else {
            return false;
        };
        let Some(submit) = CmdSubmit3d::parse(request) else {
            return false;
        };
        let end = CmdSubmit3d::LEN.saturating_add(submit.size as usize);
        let Some(stream) = request.get(CmdSubmit3d::LEN..end) else {
            return false;
        };
        let Some(wait) = self
            .three_d
            .as_mut()
            .and_then(|gpu| gpu.defer_submit_wait(hdr.ctx_id, stream, Arc::clone(waker)))
        else {
            return false;
        };
        if self.wait_deadline.is_none() {
            match DeadlineWake::start(Arc::clone(waker)) {
                Ok(timer) => self.wait_deadline = Some(timer),
                Err(error) => {
                    tracing::warn!(%error, "could not start the Venus wait timer");
                    return false;
                }
            }
        }
        tracing::trace!(
            head,
            ctx = hdr.ctx_id,
            fence = hdr.fence_id,
            "virtio-gpu ring wait deferred"
        );
        self.pending_waits.push(HeldWait {
            head,
            hdr,
            writable: writable.to_vec(),
            wait,
            deferred_at: Instant::now(),
        });
        true
    }

    /// Retire ready waits, or settle one context before letting a later
    /// command for it execute. The latter preserves the synchronous order
    /// for a pipelined caller. The Windows ICD serializes calls per context.
    pub(super) fn complete_submit_waits(
        &mut self,
        queue: &mut Queue,
        mem: &GuestMem,
        before_context: Option<u32>,
    ) -> Result<usize, DeviceError> {
        let mut completed = 0;
        let mut i = 0;
        while let Some(entry) = self.pending_waits.get(i) {
            let mut result = (entry.wait.poll)();
            if before_context == Some(entry.hdr.ctx_id) {
                while result.is_none() {
                    thread::sleep(Duration::from_micros(100));
                    result = (entry.wait.poll)();
                }
            }
            let Some(result) = result else {
                i += 1;
                continue;
            };
            let entry = self.pending_waits.swap_remove(i);
            let code = match result {
                Ok(()) => {
                    self.display.gpu_work();
                    resp::OK_NODATA
                }
                Err(error) => {
                    tracing::debug!(ctx = entry.hdr.ctx_id, %error, "virtio-gpu deferred ring wait failed");
                    error.resp_code()
                }
            };
            tracing::trace!(
                head = entry.head,
                ctx = entry.hdr.ctx_id,
                fence = entry.hdr.fence_id,
                waited_us = entry.deferred_at.elapsed().as_micros() as u64,
                code,
                "virtio-gpu ring wait completed"
            );
            let reply = Reply {
                code,
                body: Vec::new(),
            };
            if let Served::Done(written) =
                self.respond(mem, entry.head, &entry.writable, entry.hdr, reply)
            {
                queue
                    .add_used(mem, entry.head, written)
                    .map_err(|error| DeviceError::Queue(error.to_string()))?;
                completed += 1;
            }
        }
        if let Some(timer) = &self.wait_deadline {
            let next = self
                .pending_waits
                .iter()
                .map(|entry| entry.wait.deadline)
                .min();
            timer.set(next.map_or(Deadline::Idle, Deadline::At));
        }
        Ok(completed)
    }
}
