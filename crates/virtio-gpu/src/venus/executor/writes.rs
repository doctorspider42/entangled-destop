//! Shared payloads, ordered on the host: no host submission that touches
//! a handle blob's payload runs while another context's — or the renderer's
//! scanout copy — that touches it is still running.
//!
//! # Why
//!
//! A guest desktop on the GPU shares images between processes as handle
//! blobs (stage S1): a client renders into its swapchain image, the
//! compositor imports and samples it, and the compositor's own frame is read
//! back by the renderer's scanout device (stage S2b). Each of those runs on
//! its own host `VkDevice`, and nothing on the host ordered one device's
//! work on a payload after another's: every acquire from `FOREIGN` has
//! `srcStageMask = TOP_OF_PIPE`, which waits for nothing on another device.
//! The only ordering was the guest's, through sync files. Measured under
//! GNOME on the GPU (ADR-0004, "the scanout tear"):
//!
//! * **The scanout copy.** Mutter flips a frame only once its fence has
//!   signalled, so the copy never met running work in the guest — but
//!   nothing *here* made that so: a frame flipped straight after its submit
//!   was read part-drawn, 1.36 million of 2.07 million pixels of a heavy
//!   1080p frame not yet the frame's
//!   (`a_heavy_frame_flipped_at_once_is_read_back_complete_never_partial`).
//! * **Client and compositor.** In one minute of glmark2 and vkcube under
//!   GNOME, a client's submission touching its buffer started ~180 times
//!   while the compositor's batch sampling that buffer was still running on
//!   the host GPU (the running touch finished a median 0.7 ms, at most
//!   3.4 ms, later), and the compositor's started ~310 times on a buffer the
//!   client's work was still running on. The guest's implicit sync has a
//!   hole there: Zink puts its batch's fence on a dma-buf only after venus
//!   has waited for the batch on the CPU (`zink_batch.c:829-839` →
//!   `vn_GetSemaphoreFdKHR` → `vn_wsi_sync_wait`), while Mutter releases a
//!   client's buffer as soon as a newer one is applied; a client that
//!   acquires it back in between exports a sync file without the
//!   compositor's fence.
//!
//! (What the window actually showed torn — one triangle of every GNOME
//! window quad — was something else: a pipeline of the wrong topology,
//! [`super::policy::ADMITTED_EXTENSIONS`]' `VK_EXT_extended_dynamic_state`.)
//!
//! The renderer's rule, whatever the guest does: **a submission touching a
//! payload starts only once every other owner's submissions touching it
//! have finished on the host GPU**, and the scanout copy is one such owner.
//! It is the ordering a kernel's dma-buf reservation gives native drivers,
//! kept by the host because the guest kernel cannot see inside a venus
//! submission.
//!
//! # How
//!
//! * **Touches, recorded.** Every image barrier on an image that is a handle
//!   blob's canonical image (its [`ImageScanout`] record) marks the command
//!   buffer it is recorded into as touching that payload. Zink acquires every
//!   exported or imported dma-buf image from `FOREIGN` at its first use in a
//!   batch and releases it at the end of the batch (`zink_batch.c:900-934`),
//!   and Mesa's WSI acquires and releases every swapchain image, so a
//!   command buffer that reads or writes a shared image carries a barrier on
//!   it. `vkCmdExecuteCommands` passes a secondary's touches to its primary;
//!   `vkBeginCommandBuffer`, a reset and a free forget them.
//! * **Marks, per queue.** A submit whose command buffers touch a payload
//!   takes the queue's next serial, and is followed at once, on the same host
//!   queue, by an empty `vkQueueSubmit` with a host fence of the executor's
//!   own: its **mark**, which signals once every batch before it on the
//!   queue has finished. The queue's fence thread ([`super::timeline`])
//!   waits for it beside the ring fences and advances the queue's
//!   [`Progress`] — so a waiter needs neither the lock of the context nor a
//!   handle of its device.
//! * **Claims.** Every executor shares one [`Payloads`] table: per payload,
//!   each owner's newest touch as `(serial, progress)` — an owner is a queue
//!   of a context, the scanout device, or a presenter (the display copying
//!   the image on its own GPU, ADR-0004's zero-copy presentation, whose touch
//!   lasts until it drops its lease, after the flush). Before a touching submit is passed
//!   to the driver, its context **claims** the payloads it touches: under the
//!   table's lock, if another owner's touch is still running the claim waits
//!   for it (bounded by [`SHARED_WAIT`]) and tries again; once none is, it
//!   records its own touch, still running, and returns. Check and record are
//!   one step, so two claims cannot both see the other finished. The
//!   scanout read claims its blob the same way before its copy (bounded by
//!   [`super::scanout::SCANOUT_WAIT`], after which the flush fails in band
//!   and the window keeps its frame) and completes its touch once the copy's
//!   fence has signalled — so no guest submission touching the buffer starts
//!   under the copy either. A presenter's lease claims the same way and
//!   completes its touch when the display drops it, once its copy has run.
//!   It has its own serials and progress: its touch outlives the flush, and
//!   the scanout device's synchronous reads must not complete it early.
//!
//! A mark covers exactly the work before it on its queue; the wait is for
//! the other owner's touches, not a device or a queue going idle. Two claims
//! of one context on its own payloads never wait for each other — ordering
//! inside one device is the guest's, by Vulkan's rules.
//!
//! # Why a bounded wait cannot deadlock, and what it costs
//!
//! A claim waits only for work already submitted to the host GPU, which
//! finishes without any guest thread doing anything more — unless that work
//! waits on a timeline semaphore of its own device that a later submit of
//! its own context must signal, and that context is itself blocked in a
//! claim. The bound breaks such a cycle: past [`SHARED_WAIT`] a guest submit
//! goes ahead unordered, counted in [`SharedWaits::timed_out`] and logged.
//! The cost is the other owner's remaining GPU time on a payload both use at
//! once — the compositor's frame for a client that reuses a buffer early,
//! which is exactly the wait implicit sync would have made it take.
//!
//! # Lifetimes
//!
//! Marks are host fences of the context's device, owned by the queue's fence
//! thread from the moment they are pushed, destroyed after they signal or
//! when the device goes idle at teardown (which completes their serials).
//! A dropped fence thread completes its whole progress, so a touch of a
//! queue that is gone never holds a waiter. The table holds a [`SharedRef`]
//! per payload — a `Weak`, so its key is never another handle's while it is
//! kept — and prunes touches that have finished and payloads nothing holds
//! any more.
//!
//! [`ImageScanout`]: super::objects::ImageScanout

use std::collections::HashMap;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use crate::venus::protocol::{
    Command, CreateFenceArgs, QueueSubmitArgs, VkDevice, VkFence, VkFenceCreateInfo, VkQueue,
    VK_ERROR_UNKNOWN, VK_SUCCESS,
};
use crate::venus::renderer::SharedRef;

use super::context::VulkanContext;
use super::generated;
use super::host::{HostVulkan, RawHandle};
use super::objects::Kind;
pub use super::timeline::Progress;

/// How long a guest submit waits for another owner's touches of a payload
/// before it goes ahead unordered: 100 ms, six frames at 60 Hz. A
/// compositor's frame, or a client's, is a few milliseconds of GPU work.
pub const SHARED_WAIT: Duration = Duration::from_millis(100);

/// Payloads one command buffer records touches of at most: the frame's
/// scanout buffer and every client it samples.
pub const MAX_TOUCHES_PER_COMMAND_BUFFER: usize = 64;

/// Who a touch belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Owner {
    /// A queue (guest id) of a context.
    Queue {
        /// The context.
        ctx_id: u32,
        /// The queue's guest id.
        queue: u64,
    },
    /// The renderer's scanout device ([`super::scanout`]).
    Scanout,
    /// A presenter's copy (ADR-0004, zero-copy presentation): the display
    /// copying the image on its own GPU, under a lease that may outlive the
    /// flush — so an owner of its own, with a progress of its own, which the
    /// scanout device's synchronous reads can never complete early.
    Presenter,
}

impl Owner {
    fn context(self) -> Option<u32> {
        match self {
            Self::Queue { ctx_id, .. } => Some(ctx_id),
            Self::Scanout | Self::Presenter => None,
        }
    }
}

/// One owner's newest touch of a payload.
#[derive(Debug, Clone)]
struct Touch {
    owner: Owner,
    serial: u64,
    progress: Arc<Progress>,
}

impl Touch {
    fn running(&self) -> bool {
        self.progress.completed() < self.serial
    }
}

#[derive(Debug)]
struct Payload {
    shared: SharedRef,
    touches: Vec<Touch>,
}

/// What a guest context's claims have waited for, for the usage log and the
/// tests.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SharedWaits {
    /// Claims that found another owner's touch running and waited for it.
    pub waited: u64,
    /// Of those, the ones that gave up at [`SHARED_WAIT`] and went ahead.
    pub timed_out: u64,
}

/// What one [`Payloads::claim`] did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Claimed {
    /// Whether it found another owner's touch running.
    pub waited: bool,
    /// Whether it gave up waiting at its deadline.
    pub timed_out: bool,
}

/// Every payload's running touches, shared by every context of an executor
/// and its scanout device. See the module docs.
#[derive(Debug, Default)]
pub struct Payloads {
    map: Mutex<HashMap<usize, Payload>>,
}

impl Payloads {
    /// An empty table.
    #[must_use]
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<usize, Payload>> {
        self.map.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Claim `payloads` for `owner`'s touch at `serial` of `progress`: once
    /// no other owner's touch of any of them is running — waiting for those
    /// until `deadline` — record it, running, and return. At the deadline
    /// the touch is recorded anyway if `proceed`, and not at all otherwise.
    pub fn claim(
        &self,
        payloads: &[SharedRef],
        owner: Owner,
        serial: u64,
        progress: &Arc<Progress>,
        deadline: Instant,
        proceed: bool,
    ) -> Claimed {
        let mut claimed = Claimed::default();
        loop {
            let running: Vec<(Arc<Progress>, u64)> = {
                let mut map = self.lock();
                map.retain(|_, p| {
                    p.touches.retain(Touch::running);
                    p.shared.is_alive() && !p.touches.is_empty()
                });
                let running: Vec<&Touch> = payloads
                    .iter()
                    .filter_map(|s| map.get(&s.key()).filter(|p| p.shared.key() == s.key()))
                    .flat_map(|p| p.touches.iter())
                    .filter(|t| t.owner != owner && t.running())
                    .collect();
                let out_of_time = Instant::now() >= deadline;
                if running.is_empty() || (out_of_time && proceed) {
                    claimed.timed_out = !running.is_empty();
                    for shared in payloads {
                        let payload = map.entry(shared.key()).or_insert_with(|| Payload {
                            shared: shared.clone(),
                            touches: Vec::new(),
                        });
                        let touch = Touch {
                            owner,
                            serial,
                            progress: Arc::clone(progress),
                        };
                        match payload.touches.iter_mut().find(|t| t.owner == owner) {
                            Some(slot) => *slot = touch,
                            None => payload.touches.push(touch),
                        }
                    }
                    return claimed;
                }
                if out_of_time {
                    claimed.timed_out = true;
                    return claimed;
                }
                running
                    .into_iter()
                    .map(|t| (Arc::clone(&t.progress), t.serial))
                    .collect()
            };
            claimed.waited = true;
            for (progress, serial) in running {
                if !progress.wait(serial, deadline) {
                    break;
                }
            }
        }
    }

    /// The owners whose touch of `payload` is running now (a test's view).
    #[must_use]
    pub fn running(&self, payload: &SharedRef) -> Vec<Owner> {
        self.lock()
            .get(&payload.key())
            .filter(|p| p.shared.key() == payload.key())
            .map(|p| {
                p.touches
                    .iter()
                    .filter(|t| t.running())
                    .map(|t| t.owner)
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Forget every touch of context `ctx_id` (it is gone; its queues'
    /// progress was completed when their threads went).
    pub fn forget_context(&self, ctx_id: u32) {
        let mut map = self.lock();
        for payload in map.values_mut() {
            payload
                .touches
                .retain(|t| t.owner.context() != Some(ctx_id));
        }
        map.retain(|_, p| !p.touches.is_empty());
    }

    /// Payloads with a running touch (the usage log).
    #[must_use]
    pub fn len(&self) -> usize {
        self.lock().len()
    }

    /// Whether no payload has a running touch.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// A context's recordings: which payloads each command buffer touches.
#[derive(Debug, Default)]
pub struct Recordings {
    /// By command buffer guest id.
    touches: HashMap<u64, Vec<SharedRef>>,
    /// What each command buffer does to each event it names, by guest ids
    /// ([`super::hold`]). Bounded by what it holds of the recording bytes:
    /// every event command recorded is charged at least 64 bytes.
    pub(super) events: HashMap<u64, HashMap<u64, super::hold::EventUse>>,
}

/// A claim a context made for one submit, to finish once the submit is in.
#[derive(Debug)]
pub struct PendingMark {
    queue: u64,
    serial: u64,
}

impl<H: HostVulkan> VulkanContext<H> {
    /// Image barriers on `images` (guest ids of `device`) are being recorded
    /// into command buffer `cb`: note the handle blobs among them.
    pub(super) fn note_touches(&mut self, device: u64, cb: u64, images: &[u64]) {
        for image in images {
            let Some(shared) = self
                .objects
                .image(device, *image)
                .ok()
                .and_then(|i| i.scanout.as_ref())
                .map(|s| s.shared.clone())
            else {
                continue;
            };
            let list = self.recordings.touches.entry(cb).or_default();
            if list.iter().any(|s| s.key() == shared.key()) {
                continue;
            }
            if list.len() >= MAX_TOUCHES_PER_COMMAND_BUFFER {
                list.remove(0);
            }
            list.push(shared);
        }
    }

    /// Command buffers `cbs` were begun, reset or freed: their recordings
    /// touch nothing any more.
    pub(super) fn forget_recordings(&mut self, cbs: &[u64]) {
        for cb in cbs {
            self.recordings.touches.remove(cb);
            self.recordings.events.remove(cb);
        }
    }

    /// Every recording of a command buffer that no longer exists goes (a
    /// pool destroyed, a device gone).
    pub(super) fn prune_recordings(&mut self) {
        let objects = &self.objects;
        self.recordings
            .touches
            .retain(|cb, _| objects.raw_any(Kind::CommandBuffer, *cb).is_ok());
        self.recordings
            .events
            .retain(|cb, _| objects.raw_any(Kind::CommandBuffer, *cb).is_ok());
    }

    /// `vkCmdExecuteCommands`: the secondaries' touches become the
    /// primary's.
    pub(super) fn inherit_recordings(&mut self, primary: u64, secondaries: &[u64]) {
        let inherited: Vec<SharedRef> = secondaries
            .iter()
            .filter_map(|cb| self.recordings.touches.get(cb))
            .flatten()
            .cloned()
            .collect();
        if inherited.is_empty() {
            return;
        }
        let list = self.recordings.touches.entry(primary).or_default();
        for shared in inherited {
            if !list.iter().any(|s| s.key() == shared.key())
                && list.len() < MAX_TOUCHES_PER_COMMAND_BUFFER
            {
                list.push(shared);
            }
        }
    }

    /// The payloads command buffers `cbs` touch, each once.
    fn touched_by(&self, cbs: &[u64]) -> Vec<SharedRef> {
        let mut touched: Vec<SharedRef> = Vec::new();
        for shared in cbs
            .iter()
            .filter_map(|cb| self.recordings.touches.get(cb))
            .flatten()
        {
            if !touched.iter().any(|s| s.key() == shared.key()) {
                touched.push(shared.clone());
            }
        }
        touched
    }

    /// Before a submit of `cbs` to `queue` is passed to the driver: claim
    /// the payloads they touch (see the module docs). `None` if they touch
    /// none, or the queue can have no mark (no fence thread): the submit is
    /// then not ordered.
    pub(super) fn claim_payloads(&mut self, queue: u64, cbs: &[u64]) -> Option<PendingMark> {
        let touched = self.touched_by(cbs);
        if touched.is_empty() {
            return None;
        }
        if let Err(why) = self.start_queue_sync(queue) {
            if !self.unordered_logged {
                self.unordered_logged = true;
                tracing::warn!(
                    ctx_id = self.ctx_id,
                    %why,
                    "venus: submits touching shared images of this context go unordered"
                );
            }
            return None;
        }
        let q = self.objects.queue_mut(queue).ok()?;
        let progress = Arc::clone(q.sync.as_ref()?.progress());
        q.marks += 1;
        let serial = q.marks;
        let owner = Owner::Queue {
            ctx_id: self.ctx_id,
            queue,
        };
        let claimed = self.payloads.claim(
            &touched,
            owner,
            serial,
            &progress,
            Instant::now() + SHARED_WAIT,
            true,
        );
        if claimed.waited {
            self.shared_waits.waited += 1;
        }
        if claimed.timed_out {
            self.shared_waits.timed_out += 1;
            tracing::debug!(
                ctx_id = self.ctx_id,
                "venus: a submit touching a shared image went ahead after {SHARED_WAIT:?} \
                 waiting for another process's GPU work on it"
            );
        }
        Some(PendingMark { queue, serial })
    }

    /// After the submit a [`Self::claim_payloads`] was for (whatever the
    /// driver answered): its mark, which completes the claim's touch once
    /// the queue's work up to it has finished. A mark the host refuses
    /// completes it at once — the submit then is not ordered after.
    pub(super) fn submit_mark(&mut self, pending: PendingMark) {
        let PendingMark { queue, serial } = pending;
        let Some(fence) = self.mark_fence(queue) else {
            self.complete_unmarked(queue, serial);
            return;
        };
        let pushed = self
            .objects
            .queue(queue)
            .ok()
            .and_then(|q| q.sync.as_ref())
            .is_some_and(|s| s.push_mark(serial, fence, Instant::now() + SHARED_WAIT));
        if !pushed {
            // Submitted but nobody waits for it: the queue held
            // `MAX_RING_FENCES_PER_QUEUE` fences for a whole `SHARED_WAIT`, or
            // is going away. Wait it out here, as a ring fence does, rather
            // than destroy a fence a submit still owns.
            if let Some(device) = self.objects.queue(queue).ok().map(|q| q.device) {
                let _ = self.host_wait_fences(device, &[fence], true, u64::MAX);
                if let Ok(d) = self.objects.device(device) {
                    self.host.destroy_object(&d.host, Kind::Fence, fence);
                }
            }
            self.complete_unmarked(queue, serial);
        }
    }

    /// An empty submit with a new host fence on `queue`; the fence, or
    /// `None` if the host refused either.
    fn mark_fence(&mut self, queue: u64) -> Option<u64> {
        let q = self.objects.queue(queue).ok()?;
        let (device, host_queue) = (q.device, q.host.raw());
        let handle = Arc::clone(&self.objects.device(device).ok()?.host);
        let mut create = Command::CreateFence(CreateFenceArgs {
            device: VkDevice(0),
            p_create_info: Some(VkFenceCreateInfo {
                p_next: Vec::new(),
                flags: 0,
            }),
            p_fence: Some(VkFence(0)),
            ret: VK_ERROR_UNKNOWN,
        });
        self.host.call(&handle, &mut create).ok()?;
        let fence = match (&create, generated::result_of(&create)) {
            (Command::CreateFence(a), Some(VK_SUCCESS)) => a.p_fence.map(|f| f.0)?,
            _ => return None,
        };
        let mut submit = Command::QueueSubmit(QueueSubmitArgs {
            queue: VkQueue(host_queue),
            submit_count: 0,
            p_submits: None,
            fence: VkFence(fence),
            ret: VK_ERROR_UNKNOWN,
        });
        let ok = self.host.call(&handle, &mut submit).is_ok()
            && generated::result_of(&submit) == Some(VK_SUCCESS);
        if !ok {
            // Never submitted, so unsignalled and free to destroy.
            self.host.destroy_object(&handle, Kind::Fence, fence);
            return None;
        }
        Some(fence)
    }

    /// Serial `serial` of `queue` will have no mark: nothing waits for it.
    fn complete_unmarked(&self, queue: u64, serial: u64) {
        if let Some(sync) = self.objects.queue(queue).ok().and_then(|q| q.sync.as_ref()) {
            sync.progress().complete(serial);
        }
    }

    /// What this context's claims have waited for.
    #[must_use]
    pub fn shared_waits(&self) -> SharedWaits {
        self.shared_waits
    }
}
