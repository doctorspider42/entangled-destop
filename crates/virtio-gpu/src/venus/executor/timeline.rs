//! Virtio-gpu fences on a queue's `ring_idx` timeline (EPIC 20 stage 5b.3):
//! the host side of `VIRTIO_GPU_FLAG_INFO_RING_IDX`.
//!
//! Mesa's venus binds every `VkQueue` to a fence timeline in 1..64
//! (`vn_device.c:83-99`, recorded by `vkGetDeviceQueue2`), and on its sync
//! file paths submits a virtio-gpu execbuffer with a fence on that timeline,
//! expecting it to signal once the queue's earlier work has finished
//! (`vn_create_sync_file`, `vn_queue.c:1873-1915`). The execbuffer carries a
//! `vkWaitRingSeqnoMESA` for the ring position of the queue's last submit, so
//! by the time the device asks for the fence, the executor has already
//! handed that work to the host queue.
//!
//! # The waiter model: one thread per queue that has had a fence
//!
//! As vkr (`vkr_queue.c`, the sync thread): a fence becomes an empty
//! `vkQueueSubmit` with a host `VkFence` on the queue bound to the
//! timeline, made under the context lock like every other submit, and is
//! appended to that queue's FIFO; the queue's thread waits for the FIFO's
//! head with `vkWaitForFences` and, when it signals, destroys the host fence
//! and reports the virtio-gpu fence retired ([`FenceRetirer`]), which wakes
//! the device to complete the held response. One queue's fences therefore
//! retire in submission order, and never wait behind another queue's.
//!
//! Why a thread, and why per queue:
//!
//! * The ring workers cannot wait: a ring worker that is idle is parked on
//!   its doorbell, and one that is busy is executing the guest's next
//!   commands. The device's queue worker must not block at all.
//! * `vkWaitForFences` needs one device and blocks: one thread per queue
//!   waits on exactly one fence at a time, the oldest, and a fence added
//!   behind it never needs the wait interrupted. A single thread for all
//!   queues would have to poll, which adds latency to every fence, or wait
//!   on one queue while another's fence sits already signalled.
//! * The thread is started by a queue's first fence, so a guest that never
//!   exports a sync file (Mesa 26.0.8 with an importable-only renderer, the
//!   default here — `policy::external_semaphore_properties`) has none, and
//!   at most one per bound queue otherwise (fewer than 64 per context).
//!
//! # Marks
//!
//! The same FIFO carries the **marks** of submits that touched a shared
//! payload ([`super::writes`]): an empty submit's host fence, like a ring
//! fence's, but completing a serial of the queue's [`Progress`] instead of
//! retiring a virtio-gpu fence. A queue's first mark starts its thread just
//! as a first ring fence does. Marks are not counted as pending fences —
//! no guest waits on one — and a thread that goes completes its whole
//! progress, so no waiter is left waiting on a queue that is gone.
//!
//! # What the thread may touch
//!
//! A host fence it owns, the device it waits on (an `Arc`, whose other holder
//! is the object table), and the [`FenceRetirer`]'s list. **No guest memory**
//! and no lock the executor or the renderer hold: the guest learns of a
//! retirement only when the device's queue worker — which parks at the pause
//! gate — writes the held response. So it takes no pass of ADR-0005's gate,
//! and a pause neither waits for it nor is broken by it; GPU work already
//! submitted cannot be paused in any case.
//!
//! # Teardown
//!
//! [`Objects::destroy_device`](super::objects::Objects::destroy_device) —
//! reached by `vkDestroyDevice`, context destruction and device reset — tells
//! every fence thread of the device to stop and joins it (within one
//! [`SYNC_SLICE`], because every wait is sliced), waits for the device to go
//! idle, and only then destroys the fences still queued and retires them, in
//! order, as vkr does when a queue goes.
//!
//! # Bounds
//!
//! At most [`MAX_RING_FENCES_PER_QUEUE`] fences wait on one queue; the
//! device's own table is smaller than that ([`crate::MAX_PENDING_FENCES`]
//! across every timeline), so only fences whose responses the device's
//! watchdog already gave up on can pile up here, and a queue full of them
//! refuses the next one (the device then answers it at once).

use std::collections::VecDeque;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::thread::JoinHandle;
use std::time::Duration;

use crate::venus::protocol::{Command, VkDevice, VkFence, WaitForFencesArgs, VK_TIMEOUT};
use crate::venus::renderer::{FenceRetirer, RingFence};

use super::generated;
use super::host::HostVulkan;
use super::objects::Kind;

/// The longest one wait of a fence thread lasts before it looks at its stop
/// flag again: how long a teardown can wait for it to be joined.
pub const SYNC_SLICE: Duration = Duration::from_millis(50);

/// Most virtio-gpu fences one queue holds unretired: twice the device's
/// whole table.
pub const MAX_RING_FENCES_PER_QUEUE: usize = 2 * crate::MAX_PENDING_FENCES;

/// Most fence threads one executor runs, every context together.
///
/// A queue gets its thread with its first ring fence and keeps it for as long
/// as it lives. One context can have at most one per `ring_idx` (1..64,
/// [`super::context::MAX_RING_IDX`]); nothing but this bounded the sum, so a
/// guest of [`crate::venus::renderer::MAX_VENUS_CONTEXTS`] contexts could have
/// made this host run 64 × 63 threads. A Mesa 26.0.8 client fences on the
/// one or two queues it uses; 256 is four per context at the context cap
/// (ADR-0004, the capacity amendment). Past it the fence is refused and the
/// device answers it at once, as for a full queue.
pub const MAX_FENCE_THREADS: usize = 256;

/// The fence threads of one executor, counted against [`MAX_FENCE_THREADS`].
#[derive(Debug)]
pub struct FenceThreads {
    live: AtomicUsize,
    limit: usize,
}

impl FenceThreads {
    /// A count of none, capped at `limit`.
    #[must_use]
    pub fn new(limit: usize) -> Arc<Self> {
        Arc::new(Self {
            live: AtomicUsize::new(0),
            limit,
        })
    }

    /// Fence threads running (or about to be started) now.
    #[must_use]
    pub fn live(&self) -> usize {
        self.live.load(Ordering::Acquire)
    }

    /// The cap.
    #[must_use]
    pub fn limit(&self) -> usize {
        self.limit
    }

    /// A slot for one more thread, or `None` at the cap. The slot is given
    /// back when it drops — after the thread it was taken for is joined.
    #[must_use]
    pub fn take(self: &Arc<Self>) -> Option<FenceThreadSlot> {
        self.live
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |live| {
                (live < self.limit).then_some(live + 1)
            })
            .ok()
            .map(|_| FenceThreadSlot(Arc::clone(self)))
    }
}

/// One fence thread's place in [`FenceThreads`].
#[derive(Debug)]
pub struct FenceThreadSlot(Arc<FenceThreads>);

impl Drop for FenceThreadSlot {
    fn drop(&mut self) {
        let _ = self
            .0
            .live
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |live| {
                Some(live.saturating_sub(1))
            });
    }
}

/// One host fence the thread waits for: a virtio-gpu fence's, or the mark
/// of a submit that touched a handle blob ([`super::writes`]).
#[derive(Debug, Clone, Copy)]
enum Entry {
    /// Retired as virtio-gpu fence `fence_id` once `host` signals.
    Ring { fence_id: u32, host: u64 },
    /// Completes serial `serial` of the queue's [`Progress`] once `host`
    /// signals.
    Mark { serial: u64, host: u64 },
}

impl Entry {
    fn host(self) -> u64 {
        match self {
            Self::Ring { host, .. } | Self::Mark { host, .. } => host,
        }
    }
}

#[derive(Debug, Default)]
struct List {
    entries: VecDeque<Entry>,
    stop: bool,
    /// Where virtio-gpu fences are retired: given with the first one, since
    /// a queue whose first entry was a mark had none to give.
    retire: Option<FenceRetirer>,
}

#[derive(Debug, Default)]
struct Shared {
    list: Mutex<List>,
    changed: Condvar,
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, List> {
        self.list.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// One queue's fence thread and FIFO. See the module docs.
pub struct QueueSync<H: HostVulkan> {
    shared: Arc<Shared>,
    thread: Option<JoinHandle<()>>,
    /// How far the queue's tracked submits have finished ([`super::writes`]).
    progress: Arc<Progress>,
    ctx_id: u32,
    ring_idx: u8,
    /// Its place in the executor's [`FenceThreads`]; declared after
    /// `thread`, and dropped only after [`Drop`] has joined it.
    _slot: FenceThreadSlot,
    _host: std::marker::PhantomData<fn() -> H>,
}

impl<H: HostVulkan> std::fmt::Debug for QueueSync<H> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("QueueSync")
            .field("ctx_id", &self.ctx_id)
            .field("ring_idx", &self.ring_idx)
            .field("pending", &self.pending())
            .finish()
    }
}

/// `vkWaitForFences(1, fence, VK_TRUE, timeout)` on host handles.
fn wait<H: HostVulkan>(host: &H, device: &H::Device, fence: u64, timeout: Duration) -> i32 {
    let mut command = Command::WaitForFences(WaitForFencesArgs {
        device: VkDevice(0),
        fence_count: 1,
        p_fences: Some(vec![VkFence(fence)]),
        wait_all: 1,
        timeout: u64::try_from(timeout.as_nanos()).unwrap_or(u64::MAX),
        ret: crate::venus::protocol::VK_ERROR_UNKNOWN,
    });
    match host.call(device, &mut command) {
        Ok(()) => {
            generated::result_of(&command).unwrap_or(crate::venus::protocol::VK_ERROR_UNKNOWN)
        }
        Err(_) => crate::venus::protocol::VK_ERROR_UNKNOWN,
    }
}

impl<H: HostVulkan> QueueSync<H> {
    /// Start the fence thread of the queue bound to `ring_idx` of `ctx_id`,
    /// on `device`.
    ///
    /// # Errors
    /// The thread could not be started.
    pub fn spawn(
        host: Arc<H>,
        device: Arc<H::Device>,
        ctx_id: u32,
        ring_idx: u8,
        slot: FenceThreadSlot,
    ) -> std::io::Result<Self> {
        let shared = Arc::new(Shared::default());
        let progress = Arc::new(Progress::default());
        let thread = {
            let shared = Arc::clone(&shared);
            let progress = Arc::clone(&progress);
            std::thread::Builder::new()
                .name(format!("venus-fence-{ctx_id}-{ring_idx}"))
                .spawn(move || run(&*host, &device, &shared, &progress, ctx_id, ring_idx))?
        };
        Ok(Self {
            shared,
            thread: Some(thread),
            progress,
            ctx_id,
            ring_idx,
            _slot: slot,
            _host: std::marker::PhantomData,
        })
    }

    /// Queue host fence `host` (already submitted to the queue) for
    /// virtio-gpu fence `fence_id`, retired through `retire`. `false` when
    /// the queue holds [`MAX_RING_FENCES_PER_QUEUE`] already — which the
    /// caller checks with [`Self::has_room`] before it submits anything.
    pub fn push(&self, fence_id: u32, host: u64, retire: &FenceRetirer) -> bool {
        let mut list = self.shared.lock();
        if list.entries.len() >= MAX_RING_FENCES_PER_QUEUE || list.stop {
            return false;
        }
        list.retire.get_or_insert_with(|| retire.clone());
        list.entries.push_back(Entry::Ring { fence_id, host });
        drop(list);
        self.shared.changed.notify_all();
        true
    }

    /// Queue host fence `host` — submitted to the queue straight after a
    /// tracked submit — as the mark of serial `serial` ([`super::writes`]),
    /// waiting until `deadline` at most for room. `false` if there was none,
    /// or the thread is stopping: the caller owns the fence again.
    pub fn push_mark(&self, serial: u64, host: u64, deadline: std::time::Instant) -> bool {
        let mut list = self.shared.lock();
        loop {
            if list.stop {
                return false;
            }
            if list.entries.len() < MAX_RING_FENCES_PER_QUEUE {
                break;
            }
            let now = std::time::Instant::now();
            if now >= deadline {
                return false;
            }
            list = self
                .shared
                .changed
                .wait_timeout(list, deadline - now)
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        }
        list.entries.push_back(Entry::Mark { serial, host });
        drop(list);
        self.shared.changed.notify_all();
        true
    }

    /// How far the queue's tracked submits have finished.
    #[must_use]
    pub fn progress(&self) -> &Arc<Progress> {
        &self.progress
    }

    /// Whether another fence fits.
    #[must_use]
    pub fn has_room(&self) -> bool {
        self.shared.lock().entries.len() < MAX_RING_FENCES_PER_QUEUE
    }

    /// Virtio-gpu fences queued and not retired. Marks are not counted: no
    /// guest waits on one.
    #[must_use]
    pub fn pending(&self) -> usize {
        self.shared
            .lock()
            .entries
            .iter()
            .filter(|e| matches!(e, Entry::Ring { .. }))
            .count()
    }

    /// Ask the thread to stop, without waiting for it.
    pub fn signal_stop(&self) {
        self.shared.lock().stop = true;
        self.shared.changed.notify_all();
    }

    /// Stop the thread and wait for it: within one [`SYNC_SLICE`].
    pub fn join(&mut self) {
        self.signal_stop();
        if let Some(thread) = self.thread.take() {
            if thread.join().is_err() {
                tracing::error!(
                    ctx_id = self.ctx_id,
                    ring_idx = self.ring_idx,
                    "a venus fence thread panicked"
                );
            }
        }
    }

    /// After [`Self::join`] and with `device` idle: destroy every host fence
    /// still queued and retire its virtio-gpu fence, oldest first.
    pub fn finish(mut self, host: &H, device: &H::Device) {
        self.join();
        let (entries, retire) = {
            let mut list = self.shared.lock();
            (
                list.entries.drain(..).collect::<Vec<_>>(),
                list.retire.clone(),
            )
        };
        for entry in entries {
            host.destroy_object(device, Kind::Fence, entry.host());
            match entry {
                Entry::Ring { fence_id, .. } => {
                    if let Some(retire) = &retire {
                        retire.retire(RingFence {
                            ctx_id: self.ctx_id,
                            ring_idx: self.ring_idx,
                            fence_id,
                        });
                    }
                }
                Entry::Mark { serial, .. } => self.progress.complete(serial),
            }
        }
    }
}

impl<H: HostVulkan> Drop for QueueSync<H> {
    fn drop(&mut self) {
        // Only reached without `finish` when the queue's table entry goes
        // some other way; the thread must still not outlive its device.
        self.join();
        // Nothing of this queue is waited for after this: a touch still on
        // record as running must not hold a waiter to its deadline.
        self.progress.complete(u64::MAX);
    }
}

/// How far one queue's tracked submits have finished ([`super::writes`]):
/// the serial of the newest whose mark has signalled, and a condition
/// variable to sleep on. Shared by `Arc` with every record of a touch on the
/// queue, so a waiter needs neither the lock of the queue's context nor a
/// handle of its device.
#[derive(Debug, Default)]
pub struct Progress {
    completed: Mutex<u64>,
    changed: Condvar,
}

impl Progress {
    fn lock(&self) -> MutexGuard<'_, u64> {
        self.completed
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    /// The serial up to which every tracked submit has finished.
    #[must_use]
    pub fn completed(&self) -> u64 {
        *self.lock()
    }

    /// Serial `serial`, and so every one before it, has finished.
    pub fn complete(&self, serial: u64) {
        let mut completed = self.lock();
        if serial > *completed {
            *completed = serial;
            drop(completed);
            self.changed.notify_all();
        }
    }

    /// Wait until serial `serial` has finished, until `deadline` at most:
    /// whether it has.
    pub fn wait(&self, serial: u64, deadline: std::time::Instant) -> bool {
        let mut completed = self.lock();
        loop {
            if *completed >= serial {
                return true;
            }
            let now = std::time::Instant::now();
            if now >= deadline {
                return false;
            }
            completed = self
                .changed
                .wait_timeout(completed, deadline - now)
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        }
    }
}

/// The fence thread. See the module docs.
fn run<H: HostVulkan>(
    host: &H,
    device: &H::Device,
    shared: &Shared,
    progress: &Progress,
    ctx_id: u32,
    ring_idx: u8,
) {
    loop {
        let head = {
            let mut list = shared.lock();
            loop {
                if list.stop {
                    return;
                }
                if let Some(head) = list.entries.front() {
                    break *head;
                }
                list = shared
                    .changed
                    .wait(list)
                    .unwrap_or_else(PoisonError::into_inner);
            }
        };
        let ret = wait(host, device, head.host(), SYNC_SLICE);
        if ret == VK_TIMEOUT {
            continue;
        }
        if ret != crate::venus::protocol::VK_SUCCESS {
            // A lost device (or a driver error) will never signal it: the
            // fence is retired anyway, so the guest is not left waiting, and
            // the context learns of the loss from its own next call.
            tracing::warn!(
                ctx_id,
                ring_idx,
                ret,
                "a venus queue fence could not be waited for; retiring it"
            );
        }
        host.destroy_object(device, Kind::Fence, head.host());
        match head {
            Entry::Ring { fence_id, .. } => {
                let retire = shared.lock().retire.clone();
                if let Some(retire) = retire {
                    retire.retire(RingFence {
                        ctx_id,
                        ring_idx,
                        fence_id,
                    });
                }
            }
            Entry::Mark { serial, .. } => {
                progress.complete(serial);
            }
        }
        // Taken off the list only after the retirement is recorded, so that
        // a snapshot asking in between counts it twice rather than never.
        shared.lock().entries.pop_front();
        // Room for a mark waiting on a full queue.
        shared.changed.notify_all();
    }
}
