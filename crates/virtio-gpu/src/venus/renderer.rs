//! The [`Renderer3d`] that ties the Venus layers together: it advertises the
//! capset, accepts a venus-typed context, puts host pages behind the blob the
//! guest wants its ring in, decodes the context command stream and pumps the
//! ring (EPIC 20, ADR-0004).
//!
//! **It executes no Vulkan itself.** This is the transport half: a real Mesa
//! `venus` guest can get all the way to handing us a command ring and writing
//! commands into it, and what happens to those bytes is a [`RingSink`] per
//! ring, made by a [`SinkFactory`] the caller supplies — [`CaptureSink`] for a
//! test, [`WriteSink`] for `entangled run` writing one capture file per ring,
//! or [`ExecutorFactory`](super::executor::ExecutorFactory)'s sink, which
//! executes and answers (stage 5a.3). Nothing in this file decodes a Vulkan
//! command, and deliberately so: everything up to the ring is pure logic over
//! bytes and is provable on a host with no GPU at all, which is the whole
//! reason the seam is drawn where [`super`]'s docs draw it.
//!
//! What a sink needs beyond its bytes it gets through the factory: a
//! [`RingEnv`] per ring, whose [`ContextBlobs`] is the context's view of the
//! host blobs — where an executing sink writes its replies — and the
//! `context_created` / `context_destroyed` / `reset` hooks, each called only
//! once every ring worker it concerns has been joined.
//!
//! # The guest's route through here
//!
//! 1. `GET_CAPSET_INFO`/`GET_CAPSET` — [`capsets`](Renderer3d::capsets) offers
//!    exactly one, [`crate::CAPSET_VENUS`], and
//!    [`capset`](Renderer3d::capset) serves [`VenusCapset::new`]'s 160 bytes.
//! 2. `CTX_CREATE` with `context_init = CAPSET_VENUS` — a venus context, and
//!    nothing else is accepted.
//! 3. `RESOURCE_CREATE_BLOB` of a `HOST3D` blob — the ring's memory. We
//!    allocate [`RingPages`] for it here, because a ring's three control words
//!    are lock-free atomics both sides hammer and only host-owned pages
//!    published with [`RingPages::publish`] can express that (see
//!    [`super::shmem`]). That is `blob_id` 0. A blob with any other
//!    `blob_id` names a `VkDeviceMemory` of the same context, and gets no new
//!    pages: [`SinkFactory::export_memory`] hands back **the pages that
//!    memory already is** (stage 5b.1), so mapping the blob shows the guest
//!    exactly the bytes the GPU uses — or, for exportable device-local
//!    memory, a host handle to it (stage S1): a **handle blob**
//!    ([`ExportedMemory::Handle`]), which has no pages, is refused if mapped,
//!    and is what another context attached to it imports.
//! 4. `RESOURCE_MAP_BLOB` — the pages go in front of the guest at the window
//!    offset it named, and the [`Publication`] that keeps them alive is held
//!    beside them.
//! 5. `SUBMIT_3D` carrying `vkCreateRingMESA` — the guest's proposed layout is
//!    judged by [`RingLayout::new`] against the size of *these* pages and
//!    adopted with [`RingPages::adopt`], which re-checks it against the
//!    allocation before a [`RingPump`] ever indexes anything. The ring then
//!    gets its own [`RingWorker`] thread and, if the guest chained a
//!    `VkRingMonitorInfoMESA`, a place on its context's [`RingMonitor`].
//! 6. `SUBMIT_3D` carrying `vkNotifyRingMESA` — the doorbell. It wakes the
//!    ring's worker and does nothing else: the device's queue worker never
//!    touches ring pages.
//!
//! # Every ring is served by its own thread
//!
//! [`super::service`] has the whole argument. In short: Mesa rings the
//! doorbell at most once a millisecond and relies on the host polling for the
//! `idleTimeout` it passed at ring creation, so a host that only looks when
//! the doorbell rings misses the second of every pair of submissions — which
//! is how the previous, synchronous design of this file left a real guest's
//! first Vulkan command unread until its watchdog aborted. The worker polls
//! for `idleTimeout`, publishes `IDLE`, re-reads `tail`, and only then parks;
//! the context's monitor sets `ALIVE` for the guest's watchdog.
//!
//! What those threads cost this file:
//!
//! * **ADR-0005's [`Quiesce`] gate.** Both kinds of thread write guest-visible
//!   pages, so both take it before every pass, outside every lock. The gate is
//!   the device's, handed over with [`set_quiesce`](Renderer3d::set_quiesce);
//!   until then it is a gate that never closes, which is what a renderer with
//!   no VM around it wants.
//! * **Joins on every teardown path.** `vkDestroyRingMESA`, `ctx_destroy`,
//!   destroying a ring's blob and [`reset`](Renderer3d::reset) all stop and
//!   join the threads they end, and none of them can deadlock: the threads take
//!   no lock the renderer holds, and a thread parked at the pause gate is woken
//!   to notice it is being stopped. [`VenusRenderer::live_threads`] is how a
//!   test proves it.
//! * **ADR-0006's `save`/`load` pair is still owed.** There is host state here
//!   a resumed guest would notice missing: per ring, [`RingPump::cursor`] and
//!   [`RingPump::status`], plus the layout, the monitor period and the window
//!   offset each set of pages was published at. It is not implemented because
//!   nothing in this file is reachable from a shipping VM yet — the renderer is
//!   opt-in and diagnostic; a factory whose sinks hold host Vulkan objects
//!   refuses a snapshot by name ([`SinkFactory::snapshot_refusal`]) — but a
//!   snapshot taken over a live ring
//!   without it would resume a guest whose `head` says one thing and whose host
//!   cursor says another, which is the quietest possible corruption.
//!
//! The `reset()` half of ADR-0005 *is* implemented: it stops and joins every
//! thread, then drops every context, ring, publication and page, so a rebooted
//! guest finds no stale `head` in shared memory because it finds no shared
//! memory at all.
//!
//! # Scanout of its own blobs (stage S2b of "GNOME on the GPU")
//!
//! [`Renderer3d::scanout_blob`] and [`Renderer3d::read_rect_bgra`] serve a
//! guest compositor's flips (ADR-0004's S2b amendment):
//!
//! * a **page blob** (host-visible memory: our pages) is accepted when the
//!   image fits the pages — `offset + stride × (height − 1) + width × 4`, in
//!   u64 — and read row by row out of them;
//! * a **handle blob** is accepted only when a canonical DRM-modifier image
//!   recorded on it ([`ScanoutImage`], put there by the executor when the
//!   image was bound to the memory) is exactly the image the spec describes
//!   ([`scanout_mismatch`]), and is read by the factory
//!   ([`SinkFactory::read_scanout`]) — for the executor, through a scanout
//!   device of the renderer's own, between an acquire and a release that
//!   match the guest's last recorded release ([`ScanoutRelease`]);
//! * anything else is refused, and a refusal keeps the last acceptance.
//!
//! The directory is where the image and the release are recorded, under its
//! lock, found by the handle's identity ([`SharedRef`]) rather than by a
//! guest id.
//!
//! # Every guest-supplied id is a name, never an index
//!
//! Contexts, rings, blobs and resources all live in [`HashMap`]s keyed by the
//! guest's value, each with a cap and a named refusal for exceeding it
//! ([`MAX_VENUS_CONTEXTS`], [`MAX_RINGS_PER_CONTEXT`], [`MAX_RINGS`],
//! [`MAX_RING_BLOBS`], [`MAX_RING_BLOB_BYTES`]). A ring is looked up *inside*
//! the context that created it, so a doorbell cannot reach another context's
//! ring by naming its handle, and a ring may only be built on a blob the same
//! context created (or on one created outside any context, which is what
//! `ctx_id = 0` means on `RESOURCE_CREATE_BLOB`).
//!
//! # Failure is sticky, in both of the places the layers below make it sticky
//!
//! * A **transport stream** that refuses anything is unrecoverable by
//!   construction — Venus commands carry no length field, so there is nothing
//!   to resynchronise on ([`super::transport`]'s framing section). The context
//!   that carried it is poisoned, and every later `SUBMIT_3D` on it is refused
//!   with [`VenusError::ContextPoisoned`] rather than decoded from a position
//!   nobody can justify.
//! * A **ring** whose guest published an impossible `tail`, or whose sink met
//!   a command it cannot answer, is marked fatal by its worker, which tells the
//!   guest's driver to give up instead of waiting on a `head` that will never
//!   move, and ends. We keep the dead ring exactly where it is: a later
//!   doorbell answers [`VenusError::RingStopped`], and no code path here
//!   rebuilds a pump over a ring that has already failed.
//!
//! A *content* refusal — a ring layout that does not fit, a duplicate ring id —
//! is different, and is not sticky: the bytes were well formed, we simply will
//! not do what they asked. It fails that one command, the rest of the stream
//! still runs, and the first refusal is what the `SUBMIT_3D` is answered with.

use std::collections::HashMap;
use std::fmt;
use std::io;
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use thiserror::Error;
use virtio_core::{GuestMem, HostWaker, Quiesce, ShmBacking};

use crate::blob::{BlobMapping, BlobSupport};
use crate::error::CommandError;
use crate::protocol::{
    MemEntry, Rect, ResourceCreate3d, ResourceCreateBlob, Transfer3d, BLOB_MEM_HOST3D,
};
use crate::renderer::{CapsetInfo, FenceOutcome, FenceTimeline, Renderer3d, ScanoutBlobSpec};

use super::capset::{VenusCapset, VENUS_CAPSET_LEN, VENUS_CAPSET_MAX_VERSION};
use super::executor::modifier::CanonicalImage;
#[cfg(doc)]
use super::pump::RingPump;
use super::pump::{Batch, Consumed, RingBacking, RingSink};
use super::ring::{RingCreateInfo, RingLayout, RingLayoutError};
use super::service::{monitor_period, LiveThreads, RingMonitor, RingService, RingWorker};
use super::shmem::{Publication, RingPages, ShmemError};
use super::transport::{
    Opcode, TransportCommand, TransportError, TransportRequest, TransportStream,
};
use super::wire::WireError;

/// Size of the host-visible window this renderer asks the machine layer for.
///
/// It is a *window*, not an allocation: nothing is committed until a guest maps
/// a blob into it, and what lands there is the [`RingPages`] of whichever blob
/// it named. 256 MiB is what the portable loopback declares too, and is large
/// enough that a guest's mapping offsets are realistic rather than all crammed
/// against zero.
pub const VENUS_HOST_VISIBLE_BYTES: u64 = 256 << 20;

/// Most venus contexts this renderer keeps at once.
///
/// Each is one Vulkan connection from one guest process. `Gpu3d` already caps
/// contexts of *all* types at [`crate::renderer::MAX_CONTEXTS`]; this is the
/// renderer's own bound, because a renderer is not entitled to assume the
/// validation front in front of it is the only caller.
pub const MAX_VENUS_CONTEXTS: usize = 64;

/// Most rings one context may hold open.
///
/// Mesa creates one ring per `vn_ring`, which is one per device connection plus
/// a small number for its own internal queues; a guest asking for more than a
/// handful is not a guest doing graphics.
pub const MAX_RINGS_PER_CONTEXT: usize = 8;

/// Most rings across all contexts.
///
/// The per-context cap alone would allow
/// [`MAX_VENUS_CONTEXTS`] × [`MAX_RINGS_PER_CONTEXT`] pumps, and a pump's
/// shadow buffer grows to the largest batch its guest produces — up to
/// [`super::ring::MAX_BUFFER_BYTES`] (16 MiB) each. This is the bound that
/// makes the worst case a number: 32 × 16 MiB of host shadow, and only for a
/// guest that really wrote that many bytes.
///
/// It is also the bound on host threads: every ring has its own worker
/// ([`super::service::RingWorker`]), plus at most one `ALIVE` monitor per
/// context, so a guest can make this renderer run at most `MAX_RINGS` +
/// [`MAX_VENUS_CONTEXTS`] threads, all of them parked or sleeping unless the
/// guest is producing.
pub const MAX_RINGS: usize = 32;

/// Most host blobs this renderer backs with pages at once.
pub const MAX_RING_BLOBS: usize = 64;

/// Most blobs of `VkDeviceMemory` this renderer holds at once.
///
/// They allocate nothing — their pages are the memory's, charged to the
/// executor's budget — so this bounds only the table: Mesa makes one per
/// host-visible allocation it maps, and 4096 is the `maxMemoryAllocationCount`
/// most drivers report.
pub const MAX_MEMORY_BLOBS: usize = 4096;

/// Most host bytes this renderer will allocate across all live blobs.
///
/// A ring's shared-memory resource is a guest-chosen size, so it is a guest
/// value naming a host allocation twice over: per blob,
/// [`super::shmem::MAX_RESOURCE_BYTES`] caps one; this caps their sum. A real
/// venus ring is ~1 MiB, so 64 MiB is two orders of magnitude of headroom and
/// still a number a host can afford to lose to a hostile guest.
pub const MAX_RING_BLOB_BYTES: u64 = 64 << 20;

/// Canonical images one handle blob keeps on record (stage S2b): the
/// exporter's and a few importers'. Past it the oldest is forgotten — each
/// record is a claim about the same payload, so any one of them describes it.
pub const MAX_SCANOUT_IMAGES: usize = 8;

/// How long a `vkWaitRingSeqnoMESA` on the context stream waits for its ring
/// before refusing. The device's queue worker is blocked for as long as it
/// waits, so it is bounded — generously, because what it waits for is a
/// command the ring worker is executing now (Mesa sends it before making a
/// blob of memory it allocated asynchronously, `vn_device_memory_wait_alloc`).
pub const WAIT_RING_SEQNO_TIMEOUT: Duration = Duration::from_secs(5);

/// The capsets this renderer serves: Venus, and nothing else.
///
/// Deliberately *not* the virgl pair that
/// [`NullRenderer`](crate::NullRenderer) also advertises. Offering
/// `VIRTIO_GPU_CAPSET_VIRGL` here would let a guest negotiate mesa's virgl
/// driver against a renderer that never draws a triangle — the exact mistake
/// the null renderer's own docs warn about, and one that costs a guest its
/// desktop rather than one Vulkan device.
const CAPSETS: [CapsetInfo; 1] = [CapsetInfo {
    id: crate::CAPSET_VENUS,
    max_version: VENUS_CAPSET_MAX_VERSION,
    max_size: VENUS_CAPSET_LEN as u32,
}];

// ---------------------------------------------------------------- the sinks

/// Makes the [`RingSink`] for each ring a guest creates: one sink per ring,
/// never one shared by all of them.
///
/// Each ring's sink lives on that ring's worker thread, so it must be `Send`
/// and own everything it touches. A closure `FnMut(ctx_id, ring) ->
/// io::Result<S>` is a factory; so is [`CaptureSink::factory`].
///
/// A factory that fails refuses the `vkCreateRingMESA` that asked, by name
/// ([`VenusError::SinkUnavailable`]): a ring with nowhere to put its bytes is
/// not one to adopt and then quietly drop the bytes of.
pub trait SinkFactory: Send {
    /// The sink every ring gets.
    type Sink: RingSink + Send + 'static;

    /// The sink for ring `ring` (the guest's handle) of context `ctx_id`.
    ///
    /// # Errors
    ///
    /// Whatever stopped the sink being made — for `entangled run`, a capture
    /// file that could not be created.
    fn sink_for(&mut self, ctx_id: u32, ring: u64) -> io::Result<Self::Sink>;

    /// The sink for a ring, given everything the renderer can hand it beyond
    /// the bytes: the context's host blobs, where an executing sink writes
    /// its replies. The renderer calls this, never
    /// [`sink_for`](Self::sink_for) directly; the default forwards to it, so
    /// a capture ignores what it has no use for.
    ///
    /// # Errors
    ///
    /// As [`sink_for`](Self::sink_for).
    fn sink_for_ring(&mut self, env: RingEnv) -> io::Result<Self::Sink> {
        self.sink_for(env.ctx_id, env.ring)
    }

    /// What `VkDeviceMemory` `blob_id` of context `ctx_id` is, for a
    /// `HOST3D` blob of `size` bytes naming it: its pages (stage 5b.1) —
    /// which the renderer publishes exactly when the blob is mapped, so the
    /// guest sees the bytes the host driver imported — or, for exportable
    /// device-local memory, a host handle to it (stage S1): a blob that is
    /// never mapped, and that another context may import.
    ///
    /// # Errors
    ///
    /// Why no such blob can be made — no such memory in that context, a type
    /// that is neither ours nor exportable, a size that is not the
    /// allocation's, a blob made of it already. The default: this factory
    /// holds no Vulkan memory.
    fn export_memory(
        &mut self,
        ctx_id: u32,
        blob_id: u64,
        size: u64,
    ) -> Result<ExportedMemory, String> {
        let _ = size;
        Err(format!(
            "venus context {ctx_id} has no Vulkan memory {blob_id:#x}: this renderer executes \
             no Vulkan"
        ))
    }

    /// A venus context was created. Called before any of its rings exist.
    fn context_created(&mut self, _ctx_id: u32) {}

    /// A venus context is gone. Called **after** every ring worker of it has
    /// been stopped and joined, so nothing of the context is running and a
    /// factory that keeps per-context state (host Vulkan objects) may destroy
    /// it without racing a sink.
    fn context_destroyed(&mut self, _ctx_id: u32) {}

    /// The device reset (ADR-0005). Called after every context has been
    /// dropped and every thread joined; a factory holding host state must
    /// leave none behind.
    fn reset(&mut self) {}

    /// Why a snapshot taken now would lose something a resumed guest would
    /// notice (ADR-0006), or `None` if it would not. A factory whose sinks
    /// hold host Vulkan objects must refuse by name: those cannot be written
    /// to a file.
    fn snapshot_refusal(&self) -> Option<String> {
        None
    }

    /// Whether this factory retires virtio-gpu fences on a context's
    /// `ring_idx` timelines ([`Self::create_ring_fence`]) — what the capset's
    /// `supports_multiple_timelines` says (stage 5b.3). The default: no.
    fn retires_ring_fences(&self) -> bool {
        false
    }

    /// A virtio-gpu fence on `fence.ring_idx` (1..64) of `fence.ctx_id`: it
    /// retires once the work submitted so far to the `VkQueue` the guest
    /// bound to that timeline is done, reported through `retire` from
    /// whichever thread sees it ([`FenceRetirer::retire`]).
    /// [`FenceOutcome::Signalled`] when it is already past (a lost device).
    ///
    /// Called on the device's queue worker, after the context commands
    /// before the fence have been executed.
    ///
    /// # Errors
    /// Why no host queue can carry it — vkr refuses a fence on a timeline no
    /// queue is bound to (`vkr_context_submit_fence`). The default: this
    /// factory executes no Vulkan.
    fn create_ring_fence(
        &mut self,
        fence: RingFence,
        retire: &FenceRetirer,
    ) -> Result<FenceOutcome, String> {
        let _ = (fence, retire);
        Err("this renderer executes no Vulkan, so no queue is bound to any ring_idx".into())
    }

    /// Ring fences created and not yet retired.
    fn pending_ring_fences(&self) -> usize {
        0
    }

    /// Stage S2b: get ready to read handle blob `target.resource_id` back as
    /// `target.spec`, the renderer having judged the spec against the
    /// canonical image recorded on the blob (`target.image`) — for the
    /// executor, import the blob's handle on the renderer's own scanout
    /// device and create exactly that image over it. Replaces whatever this
    /// resource had prepared only on success.
    ///
    /// # Errors
    /// Why the blob cannot be read back on this host. The default: this
    /// factory executes no Vulkan.
    fn prepare_scanout(&mut self, target: &ScanoutTarget) -> Result<(), String> {
        let _ = target;
        Err("this renderer executes no Vulkan, so a handle blob cannot be read back".into())
    }

    /// Stage S2b: `rect` of a prepared (or evicted, and so prepared again)
    /// handle blob's image as packed BGRA, exactly `rect.width *
    /// rect.height * 4` bytes into `out`, acquiring the image from the
    /// guest's last recorded release and handing it back the same way.
    ///
    /// # Errors
    /// Why not: a GPU that did not finish in time, a lost device, a host
    /// refusal. The default: this factory executes no Vulkan.
    fn read_scanout(
        &mut self,
        target: &ScanoutTarget,
        release: ScanoutRelease,
        rect: Rect,
        out: &mut Vec<u8>,
    ) -> Result<(), String> {
        let _ = (target, release, rect, out);
        Err("this renderer executes no Vulkan, so a handle blob cannot be read back".into())
    }

    /// Stage S2b: resource `resource_id` is gone (or scanned out no more):
    /// whatever was prepared for it goes.
    fn forget_scanout(&mut self, resource_id: u32) {
        let _ = resource_id;
    }

    /// Resources with a prepared scanout right now — a diagnostic.
    fn scanout_targets(&self) -> usize {
        0
    }
}

/// A handle blob to read back, as the renderer accepted it (stage S2b,
/// [`SinkFactory::prepare_scanout`]).
#[derive(Debug, Clone)]
pub struct ScanoutTarget {
    /// The blob.
    pub resource_id: u32,
    /// Its host handle.
    pub handle: SharedHandle,
    /// The canonical image recorded on it that the spec matched.
    pub image: CanonicalImage,
    /// The accepted layout.
    pub spec: ScanoutBlobSpec,
}

// ------------------------------------------------------------ ring fences

/// A virtio-gpu fence on one of a context's `ring_idx` timelines (EPIC 20
/// stage 5b.3; the kernel's per-`(context, ring_idx)` fence context).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RingFence {
    /// The context.
    pub ctx_id: u32,
    /// The timeline, 1..64 (0 is the CPU's, which never reaches a factory).
    pub ring_idx: u8,
    /// The fence id, as the device keys it.
    pub fence_id: u32,
}

/// Most retired ring fences held for the device at once. The device asks
/// for a fence only while its own table ([`crate::MAX_PENDING_FENCES`]) has
/// room and collects retirements whenever it is woken, so this is only ever
/// reached by retirements nobody is waiting for any more (the device's
/// watchdog already answered them); the oldest of those are dropped.
pub const MAX_RETIRED_RING_FENCES: usize = 4 * crate::MAX_PENDING_FENCES;

#[derive(Default)]
struct RetireState {
    retired: Mutex<Vec<RingFence>>,
    waker: Mutex<Option<Arc<dyn HostWaker>>>,
}

/// Where a [`SinkFactory`] reports ring fences it has retired: a list the
/// device collects on its (pause-gated) queue worker through
/// [`Renderer3d::poll_fence_timelines`], and the device's [`HostWaker`] to
/// ask it to. Cheap to clone; every clone is the same list.
///
/// The thread that retires touches no guest memory — the guest learns of a
/// retirement only when the device's worker writes the held response — so
/// it needs no pass of the pause gate (ADR-0005's row for renderer threads).
#[derive(Clone, Default)]
pub struct FenceRetirer(Arc<RetireState>);

impl fmt::Debug for FenceRetirer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FenceRetirer")
            .field("retired", &self.len())
            .field("waker", &self.has_waker())
            .finish()
    }
}

impl FenceRetirer {
    fn retired(&self) -> std::sync::MutexGuard<'_, Vec<RingFence>> {
        self.0
            .retired
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn waker(&self) -> Option<Arc<dyn HostWaker>> {
        self.0
            .waker
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// Report `fence` retired, and wake the device to collect it.
    pub fn retire(&self, fence: RingFence) {
        {
            let mut retired = self.retired();
            if retired.len() >= MAX_RETIRED_RING_FENCES {
                retired.remove(0);
            }
            retired.push(fence);
        }
        if let Some(waker) = self.waker() {
            waker.wake();
        }
    }

    /// Everything retired since the last call, oldest first.
    pub fn take(&self) -> Vec<RingFence> {
        std::mem::take(&mut *self.retired())
    }

    /// Retired fences not collected yet.
    #[must_use]
    pub fn len(&self) -> usize {
        self.retired().len()
    }

    /// Whether nothing is waiting to be collected.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Whether the device has handed over a waker. Without one nothing would
    /// ever collect a retirement, so no fence may be deferred
    /// ([`Renderer3d::set_host_waker`]).
    #[must_use]
    pub fn has_waker(&self) -> bool {
        self.waker().is_some()
    }

    /// Install the device's waker.
    pub fn set_waker(&self, waker: Arc<dyn HostWaker>) {
        *self
            .0
            .waker
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(waker);
    }

    fn clear(&self) {
        self.retired().clear();
    }
}

/// What the renderer hands a [`SinkFactory`] for a new ring.
#[derive(Debug, Clone)]
pub struct RingEnv {
    /// The context the ring belongs to.
    pub ctx_id: u32,
    /// The guest's handle for the ring.
    pub ring: u64,
    /// The host blobs this ring's context may name as a reply window.
    pub blobs: ContextBlobs,
}

// ------------------------------------------------- blobs a sink may write

/// Why a reply window could not be bound or written.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum ReplyBlobError {
    /// The resource is not a host blob this renderer holds.
    #[error("resource {0} is not a host blob")]
    NotAHostBlob(u32),
    /// The blob belongs to another context.
    #[error("resource {resource_id} belongs to venus context {owner}, not to {ctx_id}")]
    Foreign {
        /// The blob named.
        resource_id: u32,
        /// Its context.
        owner: u32,
        /// The context that named it.
        ctx_id: u32,
    },
    /// `offset + size` passes the end of the blob.
    #[error("a {size:#x}-byte window at {offset:#x} does not fit the {blob:#x}-byte resource {resource_id}")]
    OutsideBlob {
        /// The blob named.
        resource_id: u32,
        /// The window's offset.
        offset: u64,
        /// The window's size.
        size: u64,
        /// The blob's size.
        blob: u64,
    },
    /// The blob was destroyed (or replaced by another under the same id)
    /// since the window was bound.
    #[error("resource {0} was destroyed while it was a reply window")]
    Gone(u32),
}

/// A bound reply window's blob, as the directory knew it when it was bound.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BlobRef {
    /// The resource id.
    pub resource_id: u32,
    /// Which incarnation of that id: a blob destroyed and re-created under
    /// the same id is a different one, and a window on the old one is gone.
    pub generation: u64,
}

#[derive(Debug)]
struct DirectoryEntry {
    ctx_id: u32,
    backing: ExportedMemory,
    generation: u64,
    /// `Some` for a blob of `VkDeviceMemory` (stage 5c): its size, and the
    /// other contexts it is attached to (`CTX_ATTACH_RESOURCE`) — the ones
    /// that may import it. `None` for a ring or reply blob.
    memory: Option<MemoryEntry>,
}

impl DirectoryEntry {
    /// The pages of a ring, reply or page blob; `None` for a handle blob.
    fn pages(&self) -> Option<&Arc<RingPages>> {
        self.backing.pages()
    }
}

#[derive(Debug, Default)]
struct MemoryEntry {
    size: u64,
    attached: Vec<u32>,
    /// Stage S2b, handle blobs only: the canonical images bound to the
    /// payload, newest last, at most [`MAX_SCANOUT_IMAGES`].
    images: Vec<ScanoutImage>,
    /// Stage S2b: the last release of the payload out of the instance
    /// recorded by any context whose canonical image is on record here.
    release: Option<ScanoutRelease>,
}

/// A handle blob's host handle **without holding it open** (stage S2b): what
/// a `VkDeviceMemory` keeps of the handle its blob holds, to find that blob
/// again by identity when a canonical image is bound to it.
#[derive(Clone)]
pub struct SharedRef(Weak<dyn std::any::Any + Send + Sync>);

impl SharedRef {
    /// A reference to `handle`.
    #[must_use]
    pub fn of(handle: &SharedHandle) -> Self {
        Self(Arc::downgrade(&handle.0))
    }

    /// Whether `handle` is the handle this refers to.
    #[must_use]
    pub fn is(&self, handle: &SharedHandle) -> bool {
        std::ptr::addr_eq(Weak::as_ptr(&self.0), Arc::as_ptr(&handle.0))
    }

    /// Whether the handle is still held by anything (a blob, an import in
    /// progress).
    #[must_use]
    pub fn is_alive(&self) -> bool {
        self.0.strong_count() > 0
    }

    /// The handle's address: its key in the directory's index. Stable, and
    /// never another handle's, while this reference exists — a `Weak` keeps
    /// the allocation (not the value) from being reused.
    fn key(&self) -> usize {
        Weak::as_ptr(&self.0).cast::<()>() as usize
    }
}

impl SharedHandle {
    /// [`SharedRef::key`] of this handle.
    fn key(&self) -> usize {
        Arc::as_ptr(&self.0).cast::<()>() as usize
    }
}

impl fmt::Debug for SharedRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("SharedRef(..)")
    }
}

/// One canonical image bound to a handle blob's payload (stage S2b).
#[derive(Debug, Clone)]
pub struct ScanoutImage {
    /// The context whose image it is.
    pub ctx_id: u32,
    /// The image's guest id there.
    pub image: u64,
    /// `memoryOffset` of the bind.
    pub offset: u64,
    /// The image exactly.
    pub canonical: CanonicalImage,
    /// Alive as long as the image is.
    pub alive: Weak<()>,
}

impl ScanoutImage {
    /// Whether the image still exists.
    #[must_use]
    pub fn is_alive(&self) -> bool {
        self.alive.strong_count() > 0
    }
}

/// How the guest last released a handle blob's image out of its instance
/// (stage S2b): the layout it left it in and the family it released it to
/// (`VK_QUEUE_FAMILY_FOREIGN_EXT`, or `VK_QUEUE_FAMILY_EXTERNAL`). The
/// scanout device acquires from exactly this and releases back to it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScanoutRelease {
    /// `newLayout` of the release.
    pub layout: i32,
    /// `dstQueueFamilyIndex` of the release.
    pub family: u32,
}

/// A host handle to exportable device-local memory (stage S1), as the
/// executor made it: opaque to the renderer — it only holds and hands it
/// out — and the executor's to look inside (it knows its host's type).
/// Dropping the last clone closes the handle.
#[derive(Clone)]
pub struct SharedHandle(pub Arc<dyn std::any::Any + Send + Sync>);

impl fmt::Debug for SharedHandle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("SharedHandle(..)")
    }
}

/// What a blob of `VkDeviceMemory` is made of
/// ([`SinkFactory::export_memory`]).
#[derive(Debug, Clone)]
pub enum ExportedMemory {
    /// Host-visible memory (stage 5b.1): our imported pages, which a mapping
    /// of the blob shows the guest.
    Pages(Arc<RingPages>),
    /// Exportable device-local memory (stage S1): a **handle blob**. No
    /// pages — the guest can never map it (`RESOURCE_MAP_BLOB` is refused) —
    /// and a host handle to the allocation, which another context attached
    /// to the blob imports into its own device. The handle holds the
    /// allocation, so the blob outlives the exporting memory, its device and
    /// its context, as a dma-buf outlives its exporter.
    Handle(SharedHandle),
}

impl ExportedMemory {
    /// The pages, for a page blob.
    #[must_use]
    pub fn pages(&self) -> Option<&Arc<RingPages>> {
        match self {
            Self::Pages(pages) => Some(pages),
            Self::Handle(_) => None,
        }
    }
}

/// A blob of `VkDeviceMemory` a context may import as memory of its own
/// (`VkImportMemoryResourceInfoMESA`, stages 5c and S1): the same `Arc` of
/// pages, or of the host handle, the blob holds.
#[derive(Debug, Clone)]
pub struct MemoryBlob {
    /// Its pages or its handle.
    pub backing: ExportedMemory,
    /// The blob's size (the allocation rounded to 4 KiB).
    pub size: u64,
    /// The context whose memory it is a blob of.
    pub owner: u32,
}

#[derive(Debug, Default)]
struct DirectoryState {
    blobs: HashMap<u32, DirectoryEntry>,
    next_generation: u64,
    /// Stage S2b: every live handle blob by its handle's address
    /// ([`SharedRef::key`]), so a bind or a release finds its blob at once.
    by_handle: HashMap<usize, u32>,
    /// Stage S2b: canonical images recorded against a handle whose blob is
    /// not in the directory yet — the exporter's image, bound before Mesa's
    /// `RESOURCE_CREATE_BLOB` of its memory arrived — adopted by that blob
    /// when it is inserted. At most [`MAX_SCANOUT_IMAGES`] × 8, the dead
    /// pruned first.
    pending: Vec<(SharedRef, ScanoutImage)>,
}

impl DirectoryState {
    /// The handle blob holding `shared`, if it is in the directory.
    fn handle_entry(&mut self, shared: &SharedRef) -> Option<&mut DirectoryEntry> {
        let id = *self.by_handle.get(&shared.key())?;
        self.blobs
            .get_mut(&id)
            .filter(|entry| matches!(&entry.backing, ExportedMemory::Handle(h) if shared.is(h)))
    }
}

/// Take a removed entry's handle out of the index, if the index still
/// names that entry's handle.
fn forget_handle(by_handle: &mut HashMap<usize, u32>, entry: &DirectoryEntry) {
    if let ExportedMemory::Handle(handle) = &entry.backing {
        by_handle.remove(&handle.key());
    }
}

/// Add `image` to a handle blob's record: a record of the same image
/// replaces the old one, the dead go, and past [`MAX_SCANOUT_IMAGES`] the
/// oldest.
fn push_image(images: &mut Vec<ScanoutImage>, image: ScanoutImage) {
    images.retain(|r| r.is_alive() && !(r.ctx_id == image.ctx_id && r.image == image.image));
    if images.len() >= MAX_SCANOUT_IMAGES {
        images.remove(0);
    }
    images.push(image);
}

/// Every host blob the renderer holds, shared with the ring workers behind a
/// lock so a sink can write a reply into one.
///
/// The renderer inserts on `RESOURCE_CREATE_BLOB` and removes on destroy and
/// reset; a sink writes **under the same lock**, so a blob that has been
/// removed can never be written afterwards — the removal either happened
/// before the write (which then finds the blob gone and refuses) or waits for
/// it. That is the whole of the "destroyed while it is the reply window"
/// case, which virglrenderer tolerates by unbinding the stream
/// (`vkr_cs_encoder_check_stream`); a later reply with no window is then
/// fatal, as there.
#[derive(Debug, Clone, Default)]
pub struct BlobDirectory(Arc<Mutex<DirectoryState>>);

impl BlobDirectory {
    fn with<T>(&self, f: impl FnOnce(&mut DirectoryState) -> T) -> T {
        match self.0.lock() {
            Ok(mut guard) => f(&mut guard),
            Err(poisoned) => f(&mut poisoned.into_inner()),
        }
    }

    fn insert(&self, resource_id: u32, ctx_id: u32, pages: Arc<RingPages>) {
        self.insert_entry(resource_id, ctx_id, ExportedMemory::Pages(pages), None);
    }

    fn insert_memory(&self, resource_id: u32, ctx_id: u32, backing: ExportedMemory, size: u64) {
        self.insert_entry(
            resource_id,
            ctx_id,
            backing,
            Some(MemoryEntry {
                size,
                ..MemoryEntry::default()
            }),
        );
    }

    fn insert_entry(
        &self,
        resource_id: u32,
        ctx_id: u32,
        backing: ExportedMemory,
        memory: Option<MemoryEntry>,
    ) {
        self.with(|state| {
            state.next_generation = state.next_generation.wrapping_add(1);
            let generation = state.next_generation;
            let mut memory = memory;
            if let (ExportedMemory::Handle(handle), Some(memory)) = (&backing, memory.as_mut()) {
                // Stage S2b: the images bound to this memory before its blob
                // existed are this blob's now.
                state.by_handle.insert(handle.key(), resource_id);
                let pending = std::mem::take(&mut state.pending);
                for (shared, image) in pending {
                    if shared.is(handle) {
                        push_image(&mut memory.images, image);
                    } else if shared.is_alive() && image.is_alive() {
                        state.pending.push((shared, image));
                    }
                }
            }
            if let Some(old) = state.blobs.insert(
                resource_id,
                DirectoryEntry {
                    ctx_id,
                    backing,
                    generation,
                    memory,
                },
            ) {
                forget_handle(&mut state.by_handle, &old);
            }
        });
    }

    /// `CTX_ATTACH_RESOURCE` / `CTX_DETACH_RESOURCE` of a blob of memory:
    /// whether context `ctx_id` may import it. Nothing for any other blob.
    fn attach(&self, resource_id: u32, ctx_id: u32, attach: bool) {
        self.with(|state| {
            let Some(memory) = state
                .blobs
                .get_mut(&resource_id)
                .and_then(|e| e.memory.as_mut())
            else {
                return;
            };
            memory.attached.retain(|c| *c != ctx_id);
            // Bounded by the contexts a renderer holds.
            if attach && memory.attached.len() < MAX_VENUS_CONTEXTS {
                memory.attached.push(ctx_id);
            }
        });
    }

    /// Forget context `ctx_id` in every attachment list and every scanout
    /// record: it is gone.
    fn forget_context(&self, ctx_id: u32) {
        self.with(|state| {
            for entry in state.blobs.values_mut() {
                if let Some(memory) = entry.memory.as_mut() {
                    memory.attached.retain(|c| *c != ctx_id);
                    memory.images.retain(|r| r.ctx_id != ctx_id && r.is_alive());
                }
            }
        });
    }

    /// Stage S2b: what handle blob `resource_id` holds for a scanout — its
    /// handle, the canonical images still alive on record (newest first) and
    /// the last release. `None` for anything that is not a handle blob.
    fn scanout_state(
        &self,
        resource_id: u32,
    ) -> Option<(SharedHandle, Vec<ScanoutImage>, Option<ScanoutRelease>)> {
        self.with(|state| {
            let entry = state.blobs.get_mut(&resource_id)?;
            let ExportedMemory::Handle(handle) = &entry.backing else {
                return None;
            };
            let handle = handle.clone();
            let memory = entry.memory.as_mut()?;
            memory.images.retain(ScanoutImage::is_alive);
            let images = memory.images.iter().rev().cloned().collect();
            Some((handle, images, memory.release))
        })
    }

    fn remove(&self, resource_id: u32) {
        self.with(|state| {
            if let Some(old) = state.blobs.remove(&resource_id) {
                forget_handle(&mut state.by_handle, &old);
            }
        });
    }

    fn clear(&self) {
        self.with(|state| {
            state.blobs.clear();
            state.by_handle.clear();
            state.pending.clear();
        });
    }

    /// The part of the directory context `ctx_id` may reach.
    #[must_use]
    pub fn for_context(&self, ctx_id: u32) -> ContextBlobs {
        ContextBlobs {
            ctx_id,
            directory: self.clone(),
        }
    }
}

/// The host blobs one context may name: its own, and the kernel's (context
/// 0), exactly the blobs a ring of it may be built on.
#[derive(Debug, Clone)]
pub struct ContextBlobs {
    ctx_id: u32,
    directory: BlobDirectory,
}

impl ContextBlobs {
    /// Judge a `vkSetReplyCommandStreamMESA`: the resource must be a host blob
    /// of this context and `offset + size` must fit it (`vkr_transport.c:14-29`,
    /// `vkr_cs.c:10-38`).
    ///
    /// # Errors
    /// [`ReplyBlobError`] naming the rule broken.
    pub fn bind(
        &self,
        resource_id: u32,
        offset: u64,
        size: u64,
    ) -> Result<BlobRef, ReplyBlobError> {
        self.directory.with(|state| {
            let entry = state
                .blobs
                .get(&resource_id)
                .filter(|entry| entry.memory.is_none())
                .ok_or(ReplyBlobError::NotAHostBlob(resource_id))?;
            if entry.ctx_id != 0 && entry.ctx_id != self.ctx_id {
                return Err(ReplyBlobError::Foreign {
                    resource_id,
                    owner: entry.ctx_id,
                    ctx_id: self.ctx_id,
                });
            }
            let blob = entry
                .pages()
                .ok_or(ReplyBlobError::NotAHostBlob(resource_id))?
                .resource_len();
            if offset.checked_add(size).is_none_or(|end| end > blob) {
                return Err(ReplyBlobError::OutsideBlob {
                    resource_id,
                    offset,
                    size,
                    blob,
                });
            }
            Ok(BlobRef {
                resource_id,
                generation: entry.generation,
            })
        })
    }

    /// Copy `size` bytes at `offset` of host blob `resource_id` out, under
    /// the directory lock: a `vkExecuteCommandStreamsMESA` stream. The same
    /// rules as [`Self::bind`] (a blob of this context or the kernel's, the
    /// range inside it, `vkr_cs_decoder_set_resource_stream`); the copy is
    /// private, so a guest still writing the blob changes nothing the
    /// executor has begun to decode.
    ///
    /// # Errors
    /// [`ReplyBlobError`] naming the rule broken.
    pub fn read(
        &self,
        resource_id: u32,
        offset: u64,
        size: u64,
    ) -> Result<Vec<u8>, ReplyBlobError> {
        let blob = self.bind(resource_id, offset, size)?;
        let len = usize::try_from(size).map_err(|_| ReplyBlobError::OutsideBlob {
            resource_id,
            offset,
            size,
            blob: 0,
        })?;
        self.directory.with(|state| {
            let entry = state
                .blobs
                .get(&resource_id)
                .filter(|entry| entry.generation == blob.generation)
                .ok_or(ReplyBlobError::Gone(resource_id))?;
            let pages = entry
                .pages()
                .ok_or(ReplyBlobError::NotAHostBlob(resource_id))?;
            let mut bytes = vec![0u8; len];
            pages
                .read_bytes(offset, &mut bytes)
                .map_err(|_| ReplyBlobError::OutsideBlob {
                    resource_id,
                    offset,
                    size,
                    blob: pages.resource_len(),
                })?;
            Ok(bytes)
        })
    }

    /// The pages or the host handle of blob `resource_id` for an import as
    /// memory of this context (stages 5c and S1): a blob of `VkDeviceMemory`
    /// of **this renderer**, made by this context or attached to it
    /// (`CTX_ATTACH_RESOURCE`, which the guest kernel sends when a process
    /// opens a GEM handle of another's dma-buf). The answer is the same `Arc`
    /// the blob holds, taken under the directory lock, so what it names lives
    /// as long as the import needs it whatever the exporter, its blob or its
    /// context do afterwards.
    ///
    /// # Errors
    /// [`ReplyBlobError::NotAHostBlob`] for anything else — no such blob,
    /// or a ring or reply blob — and [`ReplyBlobError::Foreign`] for a blob
    /// of memory this context may not reach.
    pub fn memory(&self, resource_id: u32) -> Result<MemoryBlob, ReplyBlobError> {
        self.directory.with(|state| {
            let entry = state
                .blobs
                .get(&resource_id)
                .ok_or(ReplyBlobError::NotAHostBlob(resource_id))?;
            let memory = entry
                .memory
                .as_ref()
                .ok_or(ReplyBlobError::NotAHostBlob(resource_id))?;
            if entry.ctx_id != self.ctx_id && !memory.attached.contains(&self.ctx_id) {
                return Err(ReplyBlobError::Foreign {
                    resource_id,
                    owner: entry.ctx_id,
                    ctx_id: self.ctx_id,
                });
            }
            Ok(MemoryBlob {
                backing: entry.backing.clone(),
                size: memory.size,
                owner: entry.ctx_id,
            })
        })
    }

    /// Stage S2b: record `image` — a canonical image bound to memory whose
    /// handle is `shared` — on the handle blob holding that handle, under the
    /// directory lock. The blob is found by the handle's identity, never by
    /// an id, so a blob id reused since cannot be confused with it. A handle
    /// whose blob is not in the directory yet keeps the record pending for
    /// it (the exporter's bind comes before `RESOURCE_CREATE_BLOB` when an
    /// application binds first); a handle nothing holds any more records
    /// nothing. A record of the same image replaces the old one; past
    /// [`MAX_SCANOUT_IMAGES`] the oldest goes.
    pub fn record_scanout_image(&self, shared: &SharedRef, image: ScanoutImage) {
        self.directory.with(|state| {
            if let Some(memory) = state
                .handle_entry(shared)
                .and_then(|entry| entry.memory.as_mut())
            {
                push_image(&mut memory.images, image);
                return;
            }
            if !shared.is_alive() {
                return;
            }
            state.pending.retain(|(s, i)| s.is_alive() && i.is_alive());
            if state.pending.len() >= MAX_SCANOUT_IMAGES * 8 {
                state.pending.remove(0);
            }
            state.pending.push((shared.clone(), image));
        });
    }

    /// Stage S2b: a release out of the instance — to `family`, left in
    /// `layout` — of a canonical image bound to memory whose handle is
    /// `shared`, under the directory lock. Nothing if its blob is gone.
    pub fn record_release(&self, shared: &SharedRef, layout: i32, family: u32) {
        self.directory.with(|state| {
            if let Some(memory) = state
                .handle_entry(shared)
                .and_then(|entry| entry.memory.as_mut())
            {
                memory.release = Some(ScanoutRelease { layout, family });
            }
        });
    }

    /// Write `bytes` at resource offset `at` of the blob `blob` names, under
    /// the directory lock.
    ///
    /// # Errors
    /// [`ReplyBlobError::Gone`] if that blob no longer exists, and
    /// [`ReplyBlobError::OutsideBlob`] if the bytes do not fit it.
    pub fn write(&self, blob: BlobRef, at: u64, bytes: &[u8]) -> Result<(), ReplyBlobError> {
        self.directory.with(|state| {
            let entry = state
                .blobs
                .get(&blob.resource_id)
                .filter(|entry| entry.generation == blob.generation)
                .ok_or(ReplyBlobError::Gone(blob.resource_id))?;
            let pages = entry
                .pages()
                .ok_or(ReplyBlobError::NotAHostBlob(blob.resource_id))?;
            pages
                .write_bytes(at, bytes)
                .map_err(|_| ReplyBlobError::OutsideBlob {
                    resource_id: blob.resource_id,
                    offset: at,
                    size: bytes.len() as u64,
                    blob: pages.resource_len(),
                })
        })
    }
}

impl<F, S> SinkFactory for F
where
    F: FnMut(u32, u64) -> io::Result<S> + Send,
    S: RingSink + Send + 'static,
{
    type Sink = S;

    fn sink_for(&mut self, ctx_id: u32, ring: u64) -> io::Result<S> {
        self(ctx_id, ring)
    }
}

/// Whether a capture can honestly consume a transport command it found **in a
/// ring**: only the two that do nothing but move the reply cursor, and only
/// when they ask for no reply of their own.
///
/// Everything else is a request for work — `vkExecuteCommandStreamsMESA` runs
/// commands out of another buffer, and every command outside the transport
/// set is a Vulkan call — and consuming a request for work moves `head` past
/// it, which the guest reads as "your reply is written". The context-only
/// commands are refused here as the reference refuses them on a ring
/// (`vkr_transport.c`'s `is_dispatched_from_vkr_context`).
fn capture_may_consume(request: &TransportRequest) -> bool {
    !request.wants_reply()
        && matches!(
            request.command,
            TransportCommand::SetReplyCommandStream { .. }
                | TransportCommand::SeekReplyCommandStream { .. }
        )
}

/// The honest answer of a sink that can execute nothing: consume the reply
/// bookkeeping at the front of the batch, record it, and stop at the first
/// command that would need an answer — recording what that command and the
/// rest of the batch were, so the capture shows what the guest asked, and
/// declaring the ring dead so the guest's driver aborts at once instead of
/// waiting out its watchdog.
///
/// A command that is merely *incomplete* — the batch ends inside it, which a
/// guest that stores `tail` after writing never produces but which the
/// protocol allows — is left for the next pass rather than judged.
fn capture_batch(batch: Batch<'_>, mut record: impl FnMut(&[u8])) -> Consumed {
    let bytes = batch.bytes();
    let mut stream = TransportStream::new(bytes);
    let mut done = 0usize;
    loop {
        match stream.next_command() {
            None => return batch.consumed(done),
            Some(Ok(request)) if capture_may_consume(&request) => {
                let end = stream.position().min(bytes.len());
                record(bytes.get(done..end).unwrap_or_default());
                done = end;
            }
            Some(Err(TransportError::Wire(WireError::Truncated { .. }))) => {
                return batch.consumed(done);
            }
            Some(Ok(_) | Err(_)) => {
                let rest = bytes.get(done..).unwrap_or_default();
                let opcode = rest
                    .get(..4)
                    .and_then(|b| <[u8; 4]>::try_from(b).ok())
                    .map(u32::from_le_bytes);
                tracing::warn!(
                    opcode,
                    at = done,
                    recorded = rest.len(),
                    "the Venus capture reached a command it cannot answer; the ring is \
                     declared fatal so the guest aborts rather than waits"
                );
                record(rest);
                return batch.fatal_after(done);
            }
        }
    }
}

/// A [`RingSink`] that records into a shared buffer, and answers the ring
/// honestly: see [`capture_batch`].
///
/// It consumes `vkSetReplyCommandStreamMESA`/`vkSeekReplyCommandStreamMESA` and
/// records them; at the first command it would have to answer — every Vulkan
/// command — it records that command **and the rest of the batch** (so the
/// capture shows what the guest asked), leaves `head` in front of it, and
/// declares the ring fatal. A real Mesa guest therefore aborts on "ring fatal
/// error" at once, with `SetReply` + its first command in the capture, instead
/// of waiting out a 3.5 s watchdog on a `head` that never moves.
///
/// Cloning one clones the *handle*: [`factory`](Self::factory) hands every ring
/// a clone of the same buffer, which is right for a test driving one ring and
/// wrong for anything else — `entangled run` uses one [`WriteSink`] per ring.
#[derive(Debug, Clone, Default)]
pub struct CaptureSink {
    captured: Arc<Mutex<Vec<u8>>>,
}

impl CaptureSink {
    /// An empty capture.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// A [`SinkFactory`] handing every ring a clone of this handle.
    pub fn factory(&self) -> impl SinkFactory<Sink = Self> {
        let capture = self.clone();
        move |_ctx_id: u32, _ring: u64| Ok(capture.clone())
    }

    /// A copy of everything recorded so far, in ring order.
    #[must_use]
    pub fn bytes(&self) -> Vec<u8> {
        self.with(|buf| buf.clone())
    }

    /// How many bytes have been recorded.
    #[must_use]
    pub fn len(&self) -> usize {
        self.with(|buf| buf.len())
    }

    /// Whether nothing has been recorded yet.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Drop everything recorded so far.
    pub fn clear(&self) {
        self.with(|buf| buf.clear());
    }

    /// Run `f` over the buffer, taking the lock back from a panicking holder
    /// rather than panicking in turn: this is on a path a guest steers, and
    /// nothing on one of those may `unwrap`.
    fn with<T>(&self, f: impl FnOnce(&mut Vec<u8>) -> T) -> T {
        match self.captured.lock() {
            Ok(mut guard) => f(&mut guard),
            Err(poisoned) => f(&mut poisoned.into_inner()),
        }
    }
}

impl RingSink for CaptureSink {
    fn consume(&mut self, batch: Batch<'_>) -> Consumed {
        capture_batch(batch, |bytes| self.with(|buf| buf.extend_from_slice(bytes)))
    }
}

/// A [`RingSink`] that writes a ring's bytes to anything `std::io::Write` — a
/// file, for `entangled run` capturing a guest's Venus stream, one per ring.
///
/// It answers the ring exactly as [`CaptureSink`] does ([`capture_batch`]):
/// reply bookkeeping consumed and written, then the first command it cannot
/// answer written together with the rest of its batch, and the ring declared
/// fatal.
///
/// A write that fails does **not** change that answer. The bookkeeping is still
/// reported consumed, because refusing it would freeze the guest's Vulkan
/// driver over a host-side file error it can neither see nor fix; the failure
/// is logged once and latched in [`failed`](Self::failed) instead.
#[derive(Debug)]
pub struct WriteSink<W> {
    out: W,
    written: u64,
    failed: bool,
}

impl<W: io::Write> WriteSink<W> {
    /// Capture into `out`.
    pub fn new(out: W) -> Self {
        Self {
            out,
            written: 0,
            failed: false,
        }
    }

    /// Bytes handed to the writer, successfully or not.
    #[must_use]
    pub fn written(&self) -> u64 {
        self.written
    }

    /// Whether any write has failed. The ring's answer did not change.
    #[must_use]
    pub fn failed(&self) -> bool {
        self.failed
    }

    /// The writer, for a caller that wants to look at what it has collected
    /// without taking it back.
    pub fn writer(&self) -> &W {
        &self.out
    }

    /// The writer back.
    pub fn into_inner(self) -> W {
        self.out
    }

    fn record(&mut self, bytes: &[u8]) {
        if bytes.is_empty() {
            return;
        }
        self.written = self.written.saturating_add(bytes.len() as u64);
        // Flushed per record: a guest that aborts on the fatal ring usually
        // takes the VM run down soon after, and a capture still sitting in a
        // buffer is a capture that was never taken.
        let result = self.out.write_all(bytes).and_then(|()| self.out.flush());
        if let Err(error) = result {
            if !self.failed {
                tracing::error!(%error, "the Venus ring capture could not be written");
            }
            self.failed = true;
        }
    }
}

impl<W: io::Write> RingSink for WriteSink<W> {
    fn consume(&mut self, batch: Batch<'_>) -> Consumed {
        capture_batch(batch, |bytes| self.record(bytes))
    }
}

// --------------------------------------------------------------- the errors

/// Why this renderer refused something.
///
/// Kept typed rather than folded straight into [`CommandError`] because the
/// interesting failures here carry values a debugging session needs — which
/// ring, which resource, which layout rule — and `CommandError`'s 3D variants
/// carry static strings. The conversion below picks the closest response code
/// for the guest and the message goes to the log, which is the same bargain the
/// rest of the crate makes.
///
/// None of these is a host error. Each is a guest that asked for something this
/// renderer will not do, or a ring already written off because of one.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum VenusError {
    /// A context type this renderer does not serve. It serves exactly one.
    #[error(
        "capset {0} names a context type this renderer cannot execute; it serves \
         only the Venus capset"
    )]
    NotVenusCapset(u32),

    /// `CTX_CREATE` for an id that is already live, or id 0.
    #[error("venus context {0} already exists, or is the reserved id 0")]
    DuplicateContext(u32),

    /// The [`MAX_VENUS_CONTEXTS`] cap.
    #[error("the host limit of {MAX_VENUS_CONTEXTS} venus contexts is reached")]
    TooManyContexts,

    /// A command naming a context this renderer never created.
    #[error("no such venus context: {0}")]
    UnknownContext(u32),

    /// A `SUBMIT_3D` on a context whose transport stream already refused
    /// something. See the module docs on sticky failure.
    #[error(
        "venus context {0} refused a transport command earlier; a Venus stream has no length \
         field, so there is no position after it worth decoding from"
    )]
    ContextPoisoned(u32),

    /// The bytes of the context command stream were wrong.
    #[error(transparent)]
    Transport(#[from] TransportError),

    /// A transport command that is legal only on a *ring* dispatch arrived on
    /// the context stream, where honouring it would block the whole context
    /// (`vkr_transport.c`'s `is_dispatched_from_vkr_context`).
    #[error("{0} is legal only on a ring dispatch, never on the context command stream")]
    RingOnlyCommand(&'static str),

    /// A transport command this renderer has no way to perform.
    #[error("{command} is not implemented by this renderer: {reason}")]
    Unimplemented {
        /// The Mesa name of the command.
        command: &'static str,
        /// Why it cannot be done here.
        reason: &'static str,
    },

    /// `vkCreateRingMESA` for a ring handle the context already holds.
    #[error("venus context {ctx_id} already holds a ring with handle {ring:#x}")]
    DuplicateRing {
        /// The context that asked.
        ctx_id: u32,
        /// The handle it re-used.
        ring: u64,
    },

    /// The [`MAX_RINGS_PER_CONTEXT`] cap.
    #[error(
        "venus context {ctx_id} already holds the host limit of {MAX_RINGS_PER_CONTEXT} rings"
    )]
    TooManyRingsOnContext {
        /// The context that asked.
        ctx_id: u32,
    },

    /// The [`MAX_RINGS`] cap.
    #[error("the host limit of {MAX_RINGS} venus rings across all contexts is reached")]
    TooManyRings,

    /// A command naming a ring the context does not hold. Rings are looked up
    /// inside their own context, so another context's handle is "no such ring"
    /// rather than a way in.
    #[error("venus context {ctx_id} holds no ring with handle {ring:#x}")]
    UnknownRing {
        /// The context that asked.
        ctx_id: u32,
        /// The handle it named.
        ring: u64,
    },

    /// `vkCreateRingMESA` naming a resource that is not one of this renderer's
    /// host blobs.
    #[error("resource {0} is not a host blob this renderer put pages behind")]
    UnknownRingResource(u32),

    /// `vkCreateRingMESA` naming another context's blob. A host blob is named
    /// by `(ctx_id, blob_id)`; asking a different context for it finds nothing.
    #[error("resource {resource_id} belongs to venus context {owner}, not to {ctx_id}")]
    ForeignRingResource {
        /// The resource named.
        resource_id: u32,
        /// The context that created the blob.
        owner: u32,
        /// The context that asked.
        ctx_id: u32,
    },

    /// The guest's proposed ring layout is not one this host will serve.
    #[error(transparent)]
    Layout(#[from] RingLayoutError),

    /// The pages could not be allocated, or the layout was not theirs.
    #[error(transparent)]
    Shmem(#[from] ShmemError),

    /// A doorbell for a ring whose worker has stopped for good — an impossible
    /// `tail`, a full ring its sink could not take anything from, or a command
    /// its sink could not answer. `FATAL` is already published in the ring.
    #[error("venus context {ctx_id}'s ring {ring:#x} has stopped for good and FATAL is published")]
    RingStopped {
        /// The context that rang.
        ctx_id: u32,
        /// The dead ring.
        ring: u64,
    },

    /// `vkCreateRingMESA` chaining a `VkRingMonitorInfoMESA` with a reporting
    /// period of zero, which the reference refuses too
    /// (`vkr_transport.c:228-232`): a period of nothing is not one anybody can
    /// keep.
    #[error("ring {ring:#x} on venus context {ctx_id} asked for an ALIVE period of zero")]
    ZeroMonitorPeriod {
        /// The context that asked.
        ctx_id: u32,
        /// The ring it was creating.
        ring: u64,
    },

    /// The [`SinkFactory`] could not make a sink for a new ring.
    #[error("no sink could be made for ring {ring:#x} on venus context {ctx_id}: {reason}")]
    SinkUnavailable {
        /// The context that asked.
        ctx_id: u32,
        /// The ring it was creating.
        ring: u64,
        /// What the factory said.
        reason: String,
    },

    /// The host would not start a ring worker or monitor thread.
    #[error("the host could not start a venus {what} thread: {reason}")]
    ThreadSpawn {
        /// Which kind of thread.
        what: &'static str,
        /// What the OS said.
        reason: String,
    },

    /// A blob memory type this renderer does not serve.
    #[error("blob memory type {0} is not a host blob this renderer can back with pages")]
    UnsupportedBlobMem(u32),

    /// A `HOST3D` blob that arrived with guest pages attached.
    #[error("a HOST3D blob carries no guest page list, but resource {0} arrived with one")]
    BlobCarriesPages(u32),

    /// `RESOURCE_CREATE_BLOB` re-using a live resource id.
    #[error("resource {0} already names a host blob")]
    DuplicateBlob(u32),

    /// A command naming a blob this renderer does not hold.
    #[error("resource {0} is not a host blob of this renderer")]
    UnknownBlob(u32),

    /// The [`MAX_RING_BLOBS`] cap.
    #[error("the host limit of {MAX_RING_BLOBS} venus host blobs is reached")]
    TooManyBlobs,

    /// The [`MAX_MEMORY_BLOBS`] cap.
    #[error("the host limit of {MAX_MEMORY_BLOBS} blobs of Vulkan memory is reached")]
    TooManyMemoryBlobs,

    /// A blob naming a `VkDeviceMemory` that cannot be one.
    #[error("resource {resource_id} cannot be a blob of Vulkan memory {blob_id:#x}: {reason}")]
    MemoryBlob {
        /// The blob.
        resource_id: u32,
        /// The memory it named.
        blob_id: u64,
        /// What the executor said.
        reason: String,
    },

    /// `RESOURCE_MAP_BLOB` of a handle blob (stage S1): device-local memory
    /// the guest cannot map — its type is not host-visible, and there are no
    /// pages of ours behind it to publish.
    #[error(
        "resource {0} is exportable device-local Vulkan memory, which has no pages the guest \
         could map"
    )]
    HandleBlobNotMappable(u32),

    /// `vkCreateRingMESA` over a blob of Vulkan memory: a ring lives in
    /// pages of its own.
    #[error("resource {0} is a blob of Vulkan memory, and a ring lives in a blob of its own")]
    RingOnDeviceMemory(u32),

    /// `vkWaitRingSeqnoMESA` for a seqno its ring cannot reach, or did not
    /// reach in [`WAIT_RING_SEQNO_TIMEOUT`].
    #[error("ring {ring:#x} of venus context {ctx_id} did not reach seqno {seqno:#x}: {why}")]
    RingSeqno {
        /// The context.
        ctx_id: u32,
        /// The ring.
        ring: u64,
        /// The seqno.
        seqno: u64,
        /// Why not.
        why: &'static str,
    },

    /// The [`MAX_RING_BLOB_BYTES`] budget.
    #[error(
        "a {size:#x}-byte blob would take the host past the {max:#x} bytes this renderer will \
         hold in venus blobs at once"
    )]
    BlobBudget {
        /// The size asked for.
        size: u64,
        /// [`MAX_RING_BLOB_BYTES`].
        max: u64,
    },

    /// `RESOURCE_MAP_BLOB` for a blob that is already in the window.
    #[error("resource {0} is already published into the shared-memory window")]
    BlobAlreadyMapped(u32),

    /// The device asked for a span that is not this blob's whole length. The
    /// device computes it from the blob's own size, so the two disagreeing is a
    /// host bug rather than a guest one — refuse rather than publish pages over
    /// a span nobody agrees the length of.
    #[error("a map of resource {resource_id} asked for {size:#x} bytes; the blob is {actual:#x}")]
    BlobSpanMismatch {
        /// The blob named.
        resource_id: u32,
        /// What the device asked for.
        size: u64,
        /// What was allocated.
        actual: u64,
    },

    /// There is no host-visible window, so there is nowhere to publish pages.
    #[error("this renderer has no shared-memory window, so a blob's pages cannot reach the guest")]
    NoWindow,

    /// The machine layer refused the span.
    #[error("the shared-memory window refused the pages of resource {resource_id}: {reason}")]
    WindowRefused {
        /// The blob whose pages were refused.
        resource_id: u32,
        /// What the machine layer said.
        reason: String,
    },

    /// A classic-3D command on a renderer that serves only Venus.
    #[error("this renderer serves only Venus contexts and has no {0}")]
    NoClassic3d(&'static str),

    /// A virtio-gpu fence on a `ring_idx` timeline the factory could not
    /// put on a host queue (stage 5b.3): no queue bound to it, no context,
    /// or a factory that executes no Vulkan.
    #[error("a fence on ring_idx {ring_idx} of venus context {ctx_id}: {why}")]
    RingFence {
        /// The context.
        ctx_id: u32,
        /// The timeline.
        ring_idx: u8,
        /// Why.
        why: String,
    },

    /// A `SET_SCANOUT_BLOB` layout this renderer cannot read back (stage
    /// S2b).
    #[error("resource {resource_id} cannot be scanned out with that layout: {reason}")]
    ScanoutRefused {
        /// The blob.
        resource_id: u32,
        /// Why.
        reason: String,
    },

    /// A scanout readback that failed on the host (stage S2b).
    #[error("the scanout readback of resource {resource_id} failed: {reason}")]
    ScanoutRead {
        /// The blob.
        resource_id: u32,
        /// Why.
        reason: String,
    },
}

impl From<VenusError> for CommandError {
    /// Pick the response code the guest gets. Where the natural
    /// [`CommandError`] variant exists it is used; the content-level refusals
    /// that have no counterpart become [`CommandError::InvalidStream`] with a
    /// static reason, because the guest's driver acts on the *code* and the
    /// values live in the log line the refusal site writes.
    fn from(err: VenusError) -> Self {
        match err {
            VenusError::NotVenusCapset(capset) => Self::UnsupportedContextType(capset),
            VenusError::DuplicateContext(id) => Self::BadContextId(id),
            VenusError::TooManyContexts => Self::TooManyContexts,
            VenusError::UnknownContext(id) => Self::UnknownContext(id),
            VenusError::ContextPoisoned(_) => {
                Self::InvalidStream("this venus context already refused a transport command")
            }
            VenusError::Transport(_) => {
                Self::InvalidStream("the venus context command stream could not be decoded")
            }
            VenusError::RingOnlyCommand(_) => Self::InvalidStream(
                "a ring-only venus command arrived on the context command stream",
            ),
            VenusError::Unimplemented { .. } => {
                Self::InvalidStream("this renderer does not implement that venus command")
            }
            VenusError::DuplicateRing { .. } | VenusError::UnknownRing { .. } => {
                Self::InvalidStream("the venus ring handle is already taken, or names no ring")
            }
            VenusError::TooManyRingsOnContext { .. } | VenusError::TooManyRings => {
                Self::OutOfMemory
            }
            VenusError::UnknownRingResource(id)
            | VenusError::UnknownBlob(id)
            | VenusError::BlobCarriesPages(id)
            | VenusError::RingOnDeviceMemory(id) => Self::UnknownResource(id),
            VenusError::MemoryBlob { .. } => {
                Self::InvalidStream("a blob names Vulkan memory it cannot be")
            }
            VenusError::RingSeqno { .. } => {
                Self::InvalidStream("a venus ring did not reach the seqno waited for")
            }
            VenusError::ForeignRingResource { resource_id, .. } => {
                Self::UnknownResource(resource_id)
            }
            VenusError::Layout(_) | VenusError::Shmem(_) => {
                Self::InvalidStream("the proposed venus ring layout is not one this host serves")
            }
            VenusError::RingStopped { .. } => {
                Self::InvalidStream("the venus ring protocol was violated and the ring is dead")
            }
            VenusError::ZeroMonitorPeriod { .. } => {
                Self::InvalidStream("a venus ring asked for an ALIVE period of zero")
            }
            VenusError::SinkUnavailable { .. } | VenusError::ThreadSpawn { .. } => {
                Self::Renderer(err.to_string())
            }
            VenusError::UnsupportedBlobMem(blob_mem) => Self::UnsupportedBlobMem(blob_mem),
            VenusError::DuplicateBlob(id) => Self::DuplicateResource(id),
            VenusError::TooManyBlobs
            | VenusError::TooManyMemoryBlobs
            | VenusError::BlobBudget { .. } => Self::OutOfMemory,
            VenusError::BlobAlreadyMapped(id) => Self::BlobAlreadyMapped(id),
            VenusError::HandleBlobNotMappable(id) => Self::BlobNotMappable(id),
            VenusError::BlobSpanMismatch { .. } | VenusError::WindowRefused { .. } => {
                Self::Renderer(err.to_string())
            }
            VenusError::NoWindow => Self::NoHostVisibleWindow,
            VenusError::NoClassic3d(_) | VenusError::RingFence { .. } => {
                Self::Renderer(err.to_string())
            }
            VenusError::ScanoutRefused {
                resource_id,
                reason,
            } => Self::ScanoutLayout {
                resource_id,
                reason,
            },
            VenusError::ScanoutRead { .. } => Self::Renderer(err.to_string()),
        }
    }
}

/// Why a canonical image recorded on a handle blob is not the image a
/// `SET_SCANOUT_BLOB` of `spec` describes (stage S2b), or `None` when it is.
///
/// The format must be BGRA-ordered — `B8G8R8A8_UNORM` or its sRGB twin
/// ([`CanonicalImage::is_bgra8`]), which is what GBM's `XRGB8888` and
/// `ARGB8888` are to Zink — because the device accepts only the two BGRA
/// scanout formats: an RGBA-ordered image under one would be a guest that
/// named the wrong fourcc, and is refused rather than silently swizzled. The
/// extent must be the framebuffer's, and the plane the one the guest was
/// told for this image (`executor::modifier`, lie 3): offset 0 — in the
/// blob and in the memory the image is bound at — and exactly the
/// synthesized row pitch. The pitch is metadata, never an address: the
/// pixels are read through the image.
#[must_use]
pub fn scanout_mismatch(image: &ScanoutImage, spec: &ScanoutBlobSpec) -> Option<String> {
    let canonical = &image.canonical;
    if spec.format != crate::FORMAT_B8G8R8X8_UNORM && spec.format != crate::FORMAT_B8G8R8A8_UNORM {
        return Some(format!("scanout format {} is not a BGRA one", spec.format));
    }
    if !canonical.is_bgra8() {
        return Some(format!(
            "the image bound to it is of VkFormat {}, not B8G8R8A8 (UNORM or SRGB)",
            canonical.format
        ));
    }
    if (canonical.width, canonical.height) != (spec.width, spec.height) {
        return Some(format!(
            "the image bound to it is {}x{}, the framebuffer {}x{}",
            canonical.width, canonical.height, spec.width, spec.height
        ));
    }
    if image.offset != 0 || spec.offset != 0 {
        return Some(format!(
            "plane 0 at offset {} of a blob whose image is bound at {}: the plane is at 0",
            spec.offset, image.offset
        ));
    }
    let pitch = canonical.layout().row_pitch;
    if u64::from(spec.stride) != pitch {
        return Some(format!(
            "stride {} is not the {pitch}-byte pitch the guest was told for the image",
            spec.stride
        ));
    }
    None
}

// ---------------------------------------------------------------- the state

/// One host blob: the pages behind it, and the publication that is currently
/// showing them to the guest.
struct RingBlob {
    /// Live while the blob is mapped. Dropping it calls `unmap_host` and only
    /// then releases its own `Arc` of the pages, which is the whole of
    /// [`super::shmem`]'s lifetime argument — so it is never bypassed by
    /// holding the address anywhere else.
    publication: Option<Publication>,
    /// The allocation — pages, or for a handle blob the host handle. Rings
    /// built on pages hold their own `Arc`.
    backing: ExportedMemory,
    /// The context the blob was created on; `0` is the kernel's own.
    ctx_id: u32,
    /// The renderer-side name the guest minted it under.
    blob_id: u64,
    /// The size the guest asked for: [`RingPages::resource_len`] for a
    /// ring blob, the span a memory blob's mapping covers for one of those.
    size: u64,
    /// Whether the pages are the blob's own (`blob_id` 0), a
    /// `VkDeviceMemory`'s, or no pages at all.
    kind: BlobKind,
    /// Stage S2b: the layout a `SET_SCANOUT_BLOB` of this blob was accepted
    /// as, and — for a handle blob — the canonical image it matched.
    scanout: Option<AcceptedScanout>,
}

/// A scanout layout the renderer accepted for one blob (stage S2b).
#[derive(Debug, Clone)]
struct AcceptedScanout {
    spec: ScanoutBlobSpec,
    /// The canonical image the spec matched; `None` for a page blob.
    image: Option<CanonicalImage>,
}

impl RingBlob {
    /// The pages, for every blob but a handle blob.
    fn pages(&self) -> Option<&Arc<RingPages>> {
        self.backing.pages()
    }
}

/// What a host blob's pages are.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BlobKind {
    /// Allocated for the blob (`blob_id` 0): rings and reply windows.
    Shm,
    /// A `VkDeviceMemory`'s imported pages, shared with the executor.
    Memory,
    /// Exportable device-local `VkDeviceMemory` (stage S1): a host handle,
    /// no pages, never mapped.
    Handle,
}

/// One live command ring.
struct Ring {
    /// The pages it lives in. An `Arc`, so a ring keeps its own memory alive
    /// even if the blob is destroyed out from under it before we tear the ring
    /// down — and the worker holds one too, for the same reason.
    pages: Arc<RingPages>,
    /// The resource the guest named, so destroying that blob can take its rings
    /// with it.
    resource_id: u32,
    /// Where the control words are, for diagnostics and for the monitor.
    layout: RingLayout,
    /// Whether the context's monitor is keeping this ring alive.
    monitored: bool,
    /// The thread that owns the ring's [`RingPump`] and sink. Never restarted:
    /// once the ring is fatal the worker has ended, and it stays ended, which
    /// is what the module docs promise. Dropping it stops and joins it.
    worker: RingWorker,
}

/// One venus context.
///
/// Dropping one stops and joins every thread it owns. Its [`Drop`] signals
/// every ring's worker first and only then lets the fields join them one by
/// one, so the stops overlap instead of queueing.
struct Context {
    /// Always [`crate::CAPSET_VENUS`]; kept so a diagnostic can say what a
    /// context was created as rather than what we assume.
    capset_id: u32,
    /// Rings by the handle the guest minted, *within this context*.
    rings: HashMap<u64, Ring>,
    /// Set once a transport stream on this context refused something.
    poisoned: bool,
    /// The `ALIVE` monitor, started by the first ring that asked for one and
    /// kept until the context goes, as the reference keeps it.
    monitor: Option<RingMonitor>,
}

impl Context {
    /// Take `ring` out of this context. Its monitor stops writing its status
    /// word *before* this returns (see [`RingMonitor::unwatch`]); its worker is
    /// still running and is the caller's to stop.
    fn take_ring(&mut self, ring: u64) -> Option<Ring> {
        let taken = self.rings.remove(&ring)?;
        if taken.monitored {
            if let Some(monitor) = &self.monitor {
                monitor.unwatch(ring);
            }
        }
        Some(taken)
    }

    /// Ask every ring's worker to stop, without waiting for any of them.
    fn signal_stop(&self) {
        for ring in self.rings.values() {
            ring.worker.signal_stop();
        }
    }
}

impl Drop for Context {
    fn drop(&mut self) {
        self.signal_stop();
    }
}

/// The transport-half Venus renderer. See the module docs.
pub struct VenusRenderer<F> {
    /// Makes each new ring's sink.
    sinks: F,
    capset: VenusCapset,
    /// The host-visible window, once the machine layer has supplied one.
    window: Option<Arc<dyn ShmBacking>>,
    blobs: HashMap<u32, RingBlob>,
    /// The same blobs, shared with the ring workers so an executing sink can
    /// write replies into them; kept in step with `blobs`.
    directory: BlobDirectory,
    /// Sum of [`RingBlob::size`] over ring blobs, against
    /// [`MAX_RING_BLOB_BYTES`].
    blob_bytes: u64,
    /// Ring blobs, against [`MAX_RING_BLOBS`].
    shm_blobs: usize,
    /// Memory blobs, against [`MAX_MEMORY_BLOBS`].
    memory_blobs: usize,
    contexts: HashMap<u32, Context>,
    /// Transport commands accepted but not executed (reply streams, seqnos):
    /// a diagnostic, and what a test asserts to show they were not refused.
    observed: u64,
    /// The VM's pause gate, which every ring worker and monitor takes before
    /// it touches a ring (ADR-0005). An always-open gate until the device
    /// hands over its own.
    quiesce: Arc<Quiesce>,
    /// Every ring worker and monitor this renderer has running.
    live: LiveThreads,
    /// Ring fences the factory retired, for the device to collect (stage
    /// 5b.3).
    retirer: FenceRetirer,
}

impl<F> fmt::Debug for VenusRenderer<F> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("VenusRenderer")
            .field("contexts", &self.contexts.len())
            .field("rings", &self.ring_count())
            .field("blobs", &self.blobs.len())
            .field("blob_bytes", &self.blob_bytes)
            .field("window", &self.window.as_ref().map(|w| w.len()))
            .field("observed", &self.observed)
            .field("threads", &self.live.count())
            .finish()
    }
}

impl<F> VenusRenderer<F> {
    /// A renderer that asks `sinks` for one [`RingSink`] per ring the guest
    /// creates — see [`SinkFactory`].
    ///
    /// Each ring's bytes go to its own sink, on its own worker thread, so rings
    /// from different contexts (different guest `VkInstance`s) never interleave.
    pub fn new(sinks: F) -> Self
    where
        F: SinkFactory,
    {
        let mut capset = VenusCapset::new();
        // Per-queue fence timelines are honest exactly when the factory
        // retires them; the device keeps one fence FIFO per
        // `(context, ring_idx)` either way (`crate::fence`).
        capset.supports_multiple_timelines = sinks.retires_ring_fences();
        Self {
            sinks,
            capset,
            window: None,
            blobs: HashMap::new(),
            directory: BlobDirectory::default(),
            blob_bytes: 0,
            shm_blobs: 0,
            memory_blobs: 0,
            contexts: HashMap::new(),
            observed: 0,
            quiesce: Quiesce::new(),
            live: LiveThreads::new(),
            retirer: FenceRetirer::default(),
        }
    }

    /// The capset this renderer advertises. [`VenusCapset::new`] by default.
    #[must_use]
    pub fn capset_value(&self) -> VenusCapset {
        self.capset
    }

    /// Advertise a different capset — a renderer that grows the ability to
    /// execute Vulkan says so here rather than by editing this file.
    pub fn set_capset(&mut self, capset: VenusCapset) {
        self.capset = capset;
    }

    /// Live venus contexts.
    #[must_use]
    pub fn context_count(&self) -> usize {
        self.contexts.len()
    }

    /// Live rings across every context.
    #[must_use]
    pub fn ring_count(&self) -> usize {
        self.contexts.values().map(|ctx| ctx.rings.len()).sum()
    }

    /// Live host blobs.
    #[must_use]
    pub fn blob_count(&self) -> usize {
        self.blobs.len()
    }

    /// The pages behind a host blob, for a test playing the guest.
    #[cfg(test)]
    pub(crate) fn blob_pages(&self, resource_id: u32) -> Option<Arc<RingPages>> {
        self.blobs
            .get(&resource_id)
            .and_then(RingBlob::pages)
            .map(Arc::clone)
    }

    /// Live handle blobs (stage S1): device-local memory exported as a host
    /// handle, which a snapshot cannot carry.
    #[must_use]
    pub fn handle_blob_count(&self) -> usize {
        self.blobs
            .values()
            .filter(|blob| blob.kind == BlobKind::Handle)
            .count()
    }

    /// The factory, for a test that inspects what it holds.
    #[cfg(test)]
    pub(crate) fn factory(&self) -> &F {
        &self.sinks
    }

    /// Transport commands that were decoded and accepted without being
    /// executed — the reply-stream and seqno commands this renderer carries but
    /// has no Vulkan to perform.
    #[must_use]
    pub fn observed_commands(&self) -> u64 {
        self.observed
    }

    /// One ring's `head` and `status` words as they are in shared memory right
    /// now — `head` is the pump's cursor, which with the status is what
    /// ADR-0006 would have to persist. The status includes the monitor's
    /// `ALIVE` and anything the guest has cleared.
    #[must_use]
    pub fn ring_state(&self, ctx_id: u32, ring: u64) -> Option<(u32, u32)> {
        let ring = self.contexts.get(&ctx_id)?.rings.get(&ring)?;
        Some((
            ring.pages.load_host_word(&ring.layout.head()),
            ring.pages.load_host_word(&ring.layout.status()),
        ))
    }

    /// Whether a ring's worker has stopped for good (`FATAL` published).
    #[must_use]
    pub fn ring_stopped(&self, ctx_id: u32, ring: u64) -> Option<bool> {
        let ring = self.contexts.get(&ctx_id)?.rings.get(&ring)?;
        Some(ring.worker.has_ended())
    }

    /// The `ALIVE` period a context's monitor is keeping, if it has one.
    #[must_use]
    pub fn monitor_period(&self, ctx_id: u32) -> Option<Duration> {
        self.contexts
            .get(&ctx_id)?
            .monitor
            .as_ref()
            .map(RingMonitor::period)
    }

    /// Ring workers and monitors running right now. Zero after a
    /// [`reset`](Renderer3d::reset), which joins them all.
    #[must_use]
    pub fn live_threads(&self) -> usize {
        self.live.count()
    }

    // ------------------------------------------------------------ contexts

    /// `CTX_CREATE` for a venus-typed context.
    fn create_context(&mut self, ctx_id: u32, capset_id: u32) -> Result<(), VenusError> {
        if capset_id != crate::CAPSET_VENUS {
            return Err(VenusError::NotVenusCapset(capset_id));
        }
        if ctx_id == 0 || self.contexts.contains_key(&ctx_id) {
            return Err(VenusError::DuplicateContext(ctx_id));
        }
        if self.contexts.len() >= MAX_VENUS_CONTEXTS {
            return Err(VenusError::TooManyContexts);
        }
        self.contexts.insert(
            ctx_id,
            Context {
                capset_id,
                rings: HashMap::new(),
                poisoned: false,
                monitor: None,
            },
        );
        Ok(())
    }

    /// The capset id a context was created with, if it is live.
    #[must_use]
    pub fn context_type(&self, ctx_id: u32) -> Option<u32> {
        self.contexts.get(&ctx_id).map(|ctx| ctx.capset_id)
    }

    // --------------------------------------------------------------- blobs

    /// `RESOURCE_CREATE_BLOB` for a host blob with `blob_id` 0: allocate the
    /// pages a ring or a reply window will live in.
    ///
    /// Only [`BLOB_MEM_HOST3D`] is served. `HOST3D_GUEST` would hand us guest
    /// pages *as well*, and a ring cannot live in them — the control words are
    /// atomics the host performs its own accesses on, which is the whole
    /// argument of [`super::shmem`] — so accepting one would mean silently
    /// ignoring half of what the guest asked for.
    fn create_host_blob(
        &mut self,
        ctx_id: u32,
        args: &ResourceCreateBlob,
        entries: &[MemEntry],
    ) -> Result<(), VenusError> {
        self.check_host_blob(ctx_id, args, entries)?;
        if self.shm_blobs >= MAX_RING_BLOBS {
            return Err(VenusError::TooManyBlobs);
        }
        if self.blob_bytes.saturating_add(args.size) > MAX_RING_BLOB_BYTES {
            return Err(VenusError::BlobBudget {
                size: args.size,
                max: MAX_RING_BLOB_BYTES,
            });
        }

        // The size is guest-chosen, and `RingPages::new` is where that is
        // bounded and refused by name rather than believed.
        let pages = Arc::new(RingPages::new(args.size)?);
        self.blob_bytes = self.blob_bytes.saturating_add(args.size);
        self.shm_blobs = self.shm_blobs.saturating_add(1);
        self.directory
            .insert(args.resource_id, ctx_id, Arc::clone(&pages));
        self.blobs.insert(
            args.resource_id,
            RingBlob {
                publication: None,
                backing: ExportedMemory::Pages(pages),
                ctx_id,
                blob_id: args.blob_id,
                size: args.size,
                kind: BlobKind::Shm,
                scanout: None,
            },
        );
        Ok(())
    }

    /// What every host blob must be, whichever pages it gets: `HOST3D`, no
    /// guest pages, a live context (or the kernel's), an unused id.
    fn check_host_blob(
        &self,
        ctx_id: u32,
        args: &ResourceCreateBlob,
        entries: &[MemEntry],
    ) -> Result<(), VenusError> {
        if args.blob_mem != BLOB_MEM_HOST3D {
            return Err(VenusError::UnsupportedBlobMem(args.blob_mem));
        }
        if !entries.is_empty() {
            return Err(VenusError::BlobCarriesPages(args.resource_id));
        }
        if ctx_id != 0 && !self.contexts.contains_key(&ctx_id) {
            return Err(VenusError::UnknownContext(ctx_id));
        }
        if self.blobs.contains_key(&args.resource_id) {
            return Err(VenusError::DuplicateBlob(args.resource_id));
        }
        Ok(())
    }

    /// `RESOURCE_MAP_BLOB`: put a blob's pages in front of the guest.
    fn map_host_blob(
        &mut self,
        resource_id: u32,
        offset: u64,
        size: u64,
    ) -> Result<BlobMapping, VenusError> {
        let window = Arc::clone(self.window.as_ref().ok_or(VenusError::NoWindow)?);
        let blob = self
            .blobs
            .get_mut(&resource_id)
            .ok_or(VenusError::UnknownBlob(resource_id))?;
        if blob.publication.is_some() {
            return Err(VenusError::BlobAlreadyMapped(resource_id));
        }
        if size != blob.size {
            return Err(VenusError::BlobSpanMismatch {
                resource_id,
                size,
                actual: blob.size,
            });
        }
        // A ring blob shows all of its pages; a memory blob shows the span
        // the device reserved for it, which its pages may run past (they are
        // rounded to the driver's import alignment) — a prefix of our own
        // allocation, so still nothing but ours. A handle blob has no pages:
        // device-local memory the guest can never map (stage S1).
        let published = match (blob.kind, &blob.backing) {
            (BlobKind::Shm, ExportedMemory::Pages(pages)) => pages.publish(window, offset),
            (BlobKind::Memory, ExportedMemory::Pages(pages)) => {
                pages.publish_len(window, offset, blob.size)
            }
            _ => return Err(VenusError::HandleBlobNotMappable(resource_id)),
        };
        let publication = published.map_err(|err| VenusError::WindowRefused {
            resource_id,
            reason: err.to_string(),
        })?;
        tracing::debug!(
            resource = resource_id,
            blob_id = blob.blob_id,
            offset = format_args!("{offset:#x}"),
            len = publication.len(),
            kind = ?blob.kind,
            "venus blob pages published into the shared-memory window"
        );
        blob.publication = Some(publication);
        // The bytes behind the window are plain host RAM — ours, for a ring
        // and for imported memory alike — so cached is the truthful answer.
        // ADR-0004's measurement: our pages run at full speed in the guest
        // even as the driver's write-combined type 3, because the host-side
        // mapping decides the memory type.
        Ok(BlobMapping::CACHED)
    }

    // --------------------------------------------------------------- rings
}

impl<F: SinkFactory> VenusRenderer<F> {
    /// `SET_SCANOUT_BLOB` of a blob of this renderer (stage S2b,
    /// [`Renderer3d::scanout_blob`]): see the module docs' scanout section.
    fn accept_scanout(
        &mut self,
        resource_id: u32,
        spec: &ScanoutBlobSpec,
    ) -> Result<(), VenusError> {
        let refused = |reason: String| VenusError::ScanoutRefused {
            resource_id,
            reason,
        };
        let blob = self
            .blobs
            .get(&resource_id)
            .ok_or(VenusError::UnknownBlob(resource_id))?;
        let image = match (blob.kind, &blob.backing) {
            (BlobKind::Memory, ExportedMemory::Pages(pages)) => {
                // `offset + stride × (height − 1) + width × 4`, in u64: the
                // last byte a row of the image touches.
                let last = u64::from(spec.stride)
                    .checked_mul(u64::from(spec.height.saturating_sub(1)))
                    .and_then(|rows| rows.checked_add(u64::from(spec.offset)))
                    .and_then(|at| at.checked_add(u64::from(spec.width).checked_mul(4)?));
                match last {
                    Some(end) if spec.width > 0 && spec.height > 0 && end <= pages.mapped_len() => {}
                    _ => {
                        return Err(refused(format!(
                            "{}x{} at stride {} from offset {} does not fit the {:#x} host bytes                              of the memory",
                            spec.width,
                            spec.height,
                            spec.stride,
                            spec.offset,
                            pages.mapped_len()
                        )))
                    }
                }
                None
            }
            (BlobKind::Handle, ExportedMemory::Handle(_)) => {
                let (handle, images, _) = self
                    .directory
                    .scanout_state(resource_id)
                    .ok_or_else(|| refused("the handle blob is not in the directory".into()))?;
                let Some(newest) = images.first() else {
                    return Err(refused(
                        "no canonical DRM-modifier image is bound to its memory, so nothing says                          how its bytes are laid out"
                            .into(),
                    ));
                };
                let Some(matched) = images
                    .iter()
                    .find(|image| scanout_mismatch(image, spec).is_none())
                else {
                    return Err(refused(
                        scanout_mismatch(newest, spec).unwrap_or_else(|| "no image matches".into()),
                    ));
                };
                let target = ScanoutTarget {
                    resource_id,
                    handle,
                    image: matched.canonical.clone(),
                    spec: *spec,
                };
                self.sinks.prepare_scanout(&target).map_err(refused)?;
                Some(target.image)
            }
            _ => {
                return Err(refused(
                    "a ring or reply blob is not an image; only a blob of Vulkan memory is".into(),
                ))
            }
        };
        if let Some(blob) = self.blobs.get_mut(&resource_id) {
            blob.scanout = Some(AcceptedScanout { spec: *spec, image });
        }
        Ok(())
    }

    /// `read_rect_bgra` of a blob accepted for scanout (stage S2b).
    fn read_scanout(
        &mut self,
        resource_id: u32,
        rect: Rect,
        out: &mut Vec<u8>,
    ) -> Result<(), VenusError> {
        let failed = |reason: String| VenusError::ScanoutRead {
            resource_id,
            reason,
        };
        let blob = self
            .blobs
            .get(&resource_id)
            .ok_or(VenusError::UnknownBlob(resource_id))?;
        let accepted = blob
            .scanout
            .clone()
            .ok_or(VenusError::UnknownBlob(resource_id))?;
        let spec = accepted.spec;
        if !rect.fits_within(spec.width, spec.height) {
            return Err(failed(format!(
                "the {}x{} rect at ({}, {}) is outside the {}x{} image",
                rect.width, rect.height, rect.x, rect.y, spec.width, spec.height
            )));
        }
        let row = usize::try_from(u64::from(rect.width) * 4)
            .map_err(|_| failed("a row larger than this host addresses".into()))?;
        let len = usize::try_from(rect.pixels().saturating_mul(4))
            .map_err(|_| failed("a rect larger than this host addresses".into()))?;
        match (&blob.backing, accepted.image) {
            (ExportedMemory::Pages(pages), None) => {
                out.clear();
                out.try_reserve_exact(len)
                    .map_err(|_| failed(format!("{len} bytes of readback buffer")))?;
                out.resize(len, 0);
                for (y, dst) in (rect.y..).zip(out.chunks_exact_mut(row.max(1))) {
                    // Bounded by `accept_scanout`'s check of the whole image
                    // against the pages, and again by `read_bytes` itself.
                    let at = u64::from(spec.offset)
                        + u64::from(y) * u64::from(spec.stride)
                        + u64::from(rect.x) * 4;
                    pages
                        .read_bytes(at, dst)
                        .map_err(|error| failed(error.to_string()))?;
                }
                Ok(())
            }
            (ExportedMemory::Handle(handle), Some(image)) => {
                let release = self
                    .directory
                    .scanout_state(resource_id)
                    .and_then(|(_, _, release)| release)
                    .ok_or_else(|| {
                        failed(
                            "the guest has not released its image to a queue family outside                              its instance yet, so there is no frame to acquire"
                                .into(),
                        )
                    })?;
                let target = ScanoutTarget {
                    resource_id,
                    handle: handle.clone(),
                    image,
                    spec,
                };
                self.sinks
                    .read_scanout(&target, release, rect, out)
                    .map_err(failed)?;
                if out.len() != len {
                    return Err(failed(format!(
                        "the scanout device returned {} bytes for {len}",
                        out.len()
                    )));
                }
                Ok(())
            }
            _ => Err(failed("the accepted layout does not match the blob".into())),
        }
    }

    /// `SUBMIT_3D` on a venus context: decode the transport stream and act.
    ///
    /// A **decode** refusal poisons the context (see the module docs); an
    /// **execution** refusal — a layout we will not serve, a handle already
    /// taken — fails that one command and leaves both the stream and the
    /// context running, because the framing is still intact. The first such
    /// refusal is what the `SUBMIT_3D` is answered with.
    fn dispatch(&mut self, ctx_id: u32, stream: &[u8]) -> Result<(), VenusError> {
        match self.contexts.get(&ctx_id) {
            None => return Err(VenusError::UnknownContext(ctx_id)),
            Some(ctx) if ctx.poisoned => return Err(VenusError::ContextPoisoned(ctx_id)),
            Some(_) => {}
        }

        let mut stream = TransportStream::new(stream);
        let mut refused: Option<VenusError> = None;
        while let Some(next) = stream.next_command() {
            let request = match next {
                Ok(request) => request,
                Err(error) => {
                    tracing::warn!(
                        ctx_id,
                        %error,
                        at = stream.position(),
                        "a venus transport stream was refused; the context is poisoned"
                    );
                    if let Some(ctx) = self.contexts.get_mut(&ctx_id) {
                        ctx.poisoned = true;
                    }
                    return Err(error.into());
                }
            };
            if let Err(error) = self.execute(ctx_id, request.command) {
                tracing::warn!(
                    ctx_id,
                    %error,
                    "a venus transport command was refused"
                );
                // Keep going. A guest batches its ring commands — a create and
                // a doorbell in one `SUBMIT_3D` is the ordinary shape — and the
                // ones after a refusal are independently decodable and often
                // independently meaningful. Stopping here would turn one
                // refused command into a silently skipped doorbell, which the
                // guest experiences as a hang rather than as an error. The
                // first refusal is what the command is answered with.
                refused.get_or_insert(error);
            }
        }
        match refused {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }

    /// Perform one decoded transport command.
    fn execute(&mut self, ctx_id: u32, command: TransportCommand) -> Result<(), VenusError> {
        match command {
            TransportCommand::CreateRing {
                ring,
                info,
                monitor_period_us,
                ..
            } => self.create_ring(ctx_id, ring, info, monitor_period_us),
            TransportCommand::DestroyRing { ring } => self.destroy_ring(ctx_id, ring),
            TransportCommand::NotifyRing { ring, .. } => self.doorbell(ctx_id, ring),
            // The `extra` region is host-written: the protocol stores one `u32`
            // there for the guest to poll. `RingLayout` hands out its bounds but
            // no typed store for it — `store_host_word` takes a `HostWord`,
            // which only the three control words are — so honouring this would
            // mean either a second unsafe store path in this file (against the
            // `venus` module's rule that `shmem` is the only one) or a new
            // accessor down there. Refusing loudly beats writing nothing and
            // letting the guest poll a word that never changes.
            TransportCommand::WriteRingExtra { .. } => Err(VenusError::Unimplemented {
                command: Opcode::WriteRingExtra.name(),
                reason: "the layers below expose no typed store into a ring's extra region",
            }),
            // `vkr_transport.c` refuses this on the context stream, and must:
            // waiting here would block the context that has to service the ring
            // the wait is on. `super::transport` cannot tell the two dispatches
            // apart — it says so — which makes this our check to make.
            TransportCommand::WaitVirtqueueSeqno { .. } => Err(VenusError::RingOnlyCommand(
                Opcode::WaitVirtqueueSeqno.name(),
            )),
            TransportCommand::WaitRingSeqno { ring, seqno } => {
                self.wait_ring_seqno(ctx_id, ring, seqno)
            }
            // Carried, counted and not executed: these are the reply-stream and
            // seqno commands, and every one of them is about Vulkan work this
            // renderer does not do. Refusing them would stop a real guest before
            // it ever built its ring, which is the one thing this renderer
            // exists to let it do.
            TransportCommand::SetReplyCommandStream { .. }
            | TransportCommand::SeekReplyCommandStream { .. }
            | TransportCommand::ExecuteCommandStreams { .. }
            | TransportCommand::SubmitVirtqueueSeqno { .. } => {
                self.observed = self.observed.saturating_add(1);
                Ok(())
            }
        }
    }

    /// `vkCreateRingMESA`: judge the proposed layout against the pages it
    /// claims to live in, adopt it, and start its service — a worker thread
    /// that owns the pump and the ring's sink, and, when the guest chained a
    /// `VkRingMonitorInfoMESA`, a place on the context's `ALIVE` monitor.
    ///
    /// The ring starts **polling**, not idle, as virglrenderer's ring thread
    /// does: work the guest queued before we looked, or writes right after
    /// creating the ring, is picked up without a doorbell, and `IDLE` goes up
    /// only once `idleTimeout` has passed with nothing to do.
    fn create_ring(
        &mut self,
        ctx_id: u32,
        ring: u64,
        info: RingCreateInfo,
        monitor_period_us: Option<u32>,
    ) -> Result<(), VenusError> {
        let context = self
            .contexts
            .get(&ctx_id)
            .ok_or(VenusError::UnknownContext(ctx_id))?;
        if context.rings.contains_key(&ring) {
            return Err(VenusError::DuplicateRing { ctx_id, ring });
        }
        if context.rings.len() >= MAX_RINGS_PER_CONTEXT {
            return Err(VenusError::TooManyRingsOnContext { ctx_id });
        }
        if self.ring_count() >= MAX_RINGS {
            return Err(VenusError::TooManyRings);
        }
        let monitor = match monitor_period_us {
            None => None,
            Some(us) => {
                Some(monitor_period(us).ok_or(VenusError::ZeroMonitorPeriod { ctx_id, ring })?)
            }
        };

        let resource_id = info.resource_id;
        let blob = self
            .blobs
            .get(&resource_id)
            .ok_or(VenusError::UnknownRingResource(resource_id))?;
        // A host blob is named by `(ctx_id, blob_id)`. Context 0 is the
        // kernel's own and is reachable from any context, which is how a guest
        // allocates a blob before it opens a Vulkan connection.
        if blob.ctx_id != 0 && blob.ctx_id != ctx_id {
            return Err(VenusError::ForeignRingResource {
                resource_id,
                owner: blob.ctx_id,
                ctx_id,
            });
        }
        let pages = match (blob.kind, blob.pages()) {
            (BlobKind::Shm, Some(pages)) => Arc::clone(pages),
            _ => return Err(VenusError::RingOnDeviceMemory(resource_id)),
        };

        // Two judgements, and they are not the same one twice: `RingLayout`
        // proves the five regions fit the `resource_size` it is *told*, and
        // `adopt` re-proves them against the size actually allocated before a
        // pump can index anything. Handing the first the pages' own length is
        // what makes the second a formality rather than the only real check.
        let layout = RingLayout::new(info, pages.resource_len())?;
        let pump = pages.adopt(layout)?;

        let env = RingEnv {
            ctx_id,
            ring,
            blobs: self.directory.for_context(ctx_id),
        };
        let sink = self
            .sinks
            .sink_for_ring(env)
            .map_err(|error| VenusError::SinkUnavailable {
                ctx_id,
                ring,
                reason: error.to_string(),
            })?;

        let quiesce = Arc::clone(&self.quiesce);
        let live = self.live.clone();
        // The context was live at the top of this function and nothing between
        // here and there can have removed it; `ok_or` rather than an `if let`
        // so that a future rearrangement is a refusal instead of a ring the
        // guest believes in and we do not hold.
        let context = self
            .contexts
            .get_mut(&ctx_id)
            .ok_or(VenusError::UnknownContext(ctx_id))?;
        if let (Some(period), None) = (monitor, context.monitor.as_ref()) {
            let started = RingMonitor::spawn(
                format!("venus-mon-{ctx_id}"),
                period,
                Arc::clone(&quiesce),
                &live,
            )
            .map_err(|error| VenusError::ThreadSpawn {
                what: "ring monitor",
                reason: error.to_string(),
            })?;
            context.monitor = Some(started);
        }

        let worker = RingWorker::spawn(
            format!("venus-ring-{ctx_id}"),
            RingService::new(pump, sink, Duration::ZERO),
            Arc::clone(&pages),
            quiesce,
            &live,
        )
        .map_err(|error| VenusError::ThreadSpawn {
            what: "ring worker",
            reason: error.to_string(),
        })?;

        let monitored = match (monitor, context.monitor.as_ref()) {
            (Some(period), Some(running)) => {
                running.watch(ring, Arc::clone(&pages), layout.status(), period);
                true
            }
            _ => false,
        };

        tracing::debug!(
            ctx_id,
            ring = format_args!("{ring:#x}"),
            resource = resource_id,
            buffer = layout.buffer().len(),
            idle_timeout_ns = info.idle_timeout_ns,
            monitor_period_us,
            "venus command ring adopted and its worker started"
        );
        context.rings.insert(
            ring,
            Ring {
                pages,
                resource_id,
                layout,
                monitored,
                worker,
            },
        );
        Ok(())
    }

    /// `vkDestroyRingMESA`: take the ring off the monitor, stop and join its
    /// worker, and zero its words.
    ///
    /// A ring that is still healthy has its host words zeroed on the way out,
    /// so a guest that builds a new ring over the same bytes finds the
    /// power-on state [`RingPump::new`] insists on (ADR-0005's "no stale word
    /// in shared memory"). The monitor is taken off first, synchronously, so
    /// that `ALIVE` cannot land again after the zeroing. A ring that was marked
    /// **fatal** keeps its `STATUS_FATAL` bit: the guest is entitled to read
    /// why its ring died, and a resource that produced an impossible `tail` is
    /// not one to hand back looking fresh.
    fn destroy_ring(&mut self, ctx_id: u32, ring: u64) -> Result<(), VenusError> {
        let context = self
            .contexts
            .get_mut(&ctx_id)
            .ok_or(VenusError::UnknownContext(ctx_id))?;
        let Ring { pages, worker, .. } = context
            .take_ring(ring)
            .ok_or(VenusError::UnknownRing { ctx_id, ring })?;
        match worker.stop() {
            Some(mut pump) if !pump.is_fatal() => pump.reset(&*pages),
            Some(_) => {}
            // Only a panicked worker hands nothing back, and nothing in it
            // should be able to panic. Its words are left as they are rather
            // than zeroed by a second path that bypasses the pump.
            None => tracing::error!(
                ctx_id,
                ring = format_args!("{ring:#x}"),
                "a venus ring worker was lost; its words were not reset"
            ),
        }
        Ok(())
    }

    /// `vkNotifyRingMESA`: wake the ring's worker. Nothing else — the ring is
    /// the worker's, and this runs on the device's queue worker.
    ///
    /// A doorbell for a ring whose worker has already stopped for good is
    /// answered with [`VenusError::RingStopped`]: the guest has `FATAL` in its
    /// status word already, and saying so again is more honest than pretending
    /// the doorbell did something.
    fn doorbell(&mut self, ctx_id: u32, ring: u64) -> Result<(), VenusError> {
        let context = self
            .contexts
            .get(&ctx_id)
            .ok_or(VenusError::UnknownContext(ctx_id))?;
        let live = context
            .rings
            .get(&ring)
            .ok_or(VenusError::UnknownRing { ctx_id, ring })?;
        if live.worker.has_ended() {
            return Err(VenusError::RingStopped { ctx_id, ring });
        }
        live.worker.notify();
        Ok(())
    }

    /// Stop and drop every ring built on `resource_id`, wherever it lives.
    fn drop_rings_on(&mut self, resource_id: u32) {
        for context in self.contexts.values_mut() {
            let doomed: Vec<u64> = context
                .rings
                .iter()
                .filter(|(_, ring)| ring.resource_id == resource_id)
                .map(|(handle, _)| *handle)
                .collect();
            for handle in &doomed {
                if let Some(ring) = context.rings.get(handle) {
                    ring.worker.signal_stop();
                }
            }
            for handle in doomed {
                // Dropping the worker joins it.
                drop(context.take_ring(handle));
            }
        }
    }

    /// Drop a blob: its publication (which unmaps first and frees after), its
    /// pages, and every ring that was built on them.
    fn drop_blob(&mut self, resource_id: u32) {
        if let Some(blob) = self.blobs.remove(&resource_id) {
            match blob.kind {
                BlobKind::Shm => {
                    // Out of the sinks' reach first: after this returns no
                    // reply can land in these pages, whichever ring was about
                    // to write one.
                    self.directory.remove(resource_id);
                    self.blob_bytes = self.blob_bytes.saturating_sub(blob.size);
                    self.shm_blobs = self.shm_blobs.saturating_sub(1);
                    self.drop_rings_on(resource_id);
                }
                // Neither a reply window nor a ring can be one; its pages go
                // back to the executor's memory object, or — if that was
                // freed first — to the allocator, once the publication below
                // has unmapped them.
                // A handle blob's handle is closed when the last `Arc` of it
                // goes — this one, or an import being made right now; an
                // import already made references the allocation itself.
                BlobKind::Memory | BlobKind::Handle => {
                    // No new import can take the pages after this; an
                    // import made already holds its own `Arc` of them.
                    self.directory.remove(resource_id);
                    self.memory_blobs = self.memory_blobs.saturating_sub(1);
                    // Stage S2b: the scanout device's import of it goes too.
                    if blob.kind == BlobKind::Handle {
                        self.sinks.forget_scanout(resource_id);
                    }
                }
            }
            drop(blob);
        }
    }

    /// `RESOURCE_CREATE_BLOB` with a `blob_id`: a blob of `VkDeviceMemory`
    /// `blob_id` of context `ctx_id`, wrapping the pages the executor already
    /// imported for it (`vkr_context_create_resource_from_device_memory`).
    fn create_memory_blob(
        &mut self,
        ctx_id: u32,
        args: &ResourceCreateBlob,
        entries: &[MemEntry],
    ) -> Result<(), VenusError> {
        self.check_host_blob(ctx_id, args, entries)?;
        if self.memory_blobs >= MAX_MEMORY_BLOBS {
            return Err(VenusError::TooManyMemoryBlobs);
        }
        let backing = self
            .sinks
            .export_memory(ctx_id, args.blob_id, args.size)
            .map_err(|reason| VenusError::MemoryBlob {
                resource_id: args.resource_id,
                blob_id: args.blob_id,
                reason,
            })?;
        let kind = match backing {
            ExportedMemory::Pages(_) => BlobKind::Memory,
            ExportedMemory::Handle(_) => BlobKind::Handle,
        };
        self.memory_blobs = self.memory_blobs.saturating_add(1);
        // In the directory too, where another context attached to it may
        // find it to import (stages 5c and S1) — and never as a reply window.
        self.directory
            .insert_memory(args.resource_id, ctx_id, backing.clone(), args.size);
        self.blobs.insert(
            args.resource_id,
            RingBlob {
                publication: None,
                backing,
                ctx_id,
                blob_id: args.blob_id,
                size: args.size,
                kind,
                scanout: None,
            },
        );
        Ok(())
    }

    /// `vkWaitRingSeqnoMESA` on the context stream
    /// (`vkr_context_wait_ring_seqno`): block until the ring's `head` has
    /// reached `seqno` — its worker has executed every command before it.
    ///
    /// Mesa sends it before making a blob of memory it allocated without a
    /// reply (`vn_device_memory_wait_alloc`), so the blob's
    /// `RESOURCE_CREATE_BLOB`, which follows on the same virtqueue, finds the
    /// memory the ring made. A seqno past the ring's `tail` can never be
    /// reached and is refused at once, as vkr's ring thread refuses it; a
    /// ring that has died, or that has not got there in
    /// [`WAIT_RING_SEQNO_TIMEOUT`], is refused too. Seqnos compare as the
    /// wrapping 32-bit ring positions they are (`vkr_seqno_ge`).
    fn wait_ring_seqno(&mut self, ctx_id: u32, ring: u64, seqno: u64) -> Result<(), VenusError> {
        let context = self
            .contexts
            .get(&ctx_id)
            .ok_or(VenusError::UnknownContext(ctx_id))?;
        let live = context
            .rings
            .get(&ring)
            .ok_or(VenusError::UnknownRing { ctx_id, ring })?;
        // The ring position is 32 bits; vkr compares the low half too.
        let target = seqno as u32;
        let reached = |position: u32| position.wrapping_sub(target) <= i32::MAX as u32;
        let refuse = |why| VenusError::RingSeqno {
            ctx_id,
            ring,
            seqno,
            why,
        };
        if !reached(live.pages.load_guest_word(&live.layout.tail())) {
            return Err(refuse("the ring's tail is short of it"));
        }
        live.worker.notify();
        let deadline = std::time::Instant::now() + WAIT_RING_SEQNO_TIMEOUT;
        let mut spins = 0u32;
        loop {
            if reached(live.pages.load_host_word(&live.layout.head())) {
                return Ok(());
            }
            if live.worker.has_ended() {
                return Err(refuse("the ring stopped first"));
            }
            if std::time::Instant::now() >= deadline {
                return Err(refuse("the wait timed out"));
            }
            spins = spins.saturating_add(1);
            if spins < 64 {
                std::thread::yield_now();
            } else {
                std::thread::sleep(Duration::from_micros(100));
            }
        }
    }
}

impl<F: SinkFactory> Renderer3d for VenusRenderer<F> {
    fn capsets(&self) -> &[CapsetInfo] {
        &CAPSETS
    }

    fn capset(&mut self, id: u32, version: u32) -> Result<Vec<u8>, CommandError> {
        if id != crate::CAPSET_VENUS || version > VENUS_CAPSET_MAX_VERSION {
            return Err(CommandError::UnknownCapset { id, version });
        }
        Ok(self.capset.to_bytes().to_vec())
    }

    fn ctx_create(&mut self, ctx_id: u32, capset_id: u32, name: &str) -> Result<(), CommandError> {
        self.create_context(ctx_id, capset_id)?;
        self.sinks.context_created(ctx_id);
        tracing::debug!(ctx_id, capset_id, name, "venus context created");
        Ok(())
    }

    fn ctx_destroy(&mut self, ctx_id: u32) {
        // Dropping the context stops and joins its ring workers and its
        // monitor, then drops its rings; the pages behind them belong to the
        // blobs and stay until those are destroyed or the device resets.
        if self.contexts.remove(&ctx_id).is_some() {
            // Only now, with every ring of it joined, may the factory tear
            // down what its sinks shared (host Vulkan objects).
            self.sinks.context_destroyed(ctx_id);
            // A context id the guest reuses starts with no attachments.
            self.directory.forget_context(ctx_id);
        }
    }

    fn resource_create_3d(&mut self, _args: &ResourceCreate3d) -> Result<(), CommandError> {
        Err(VenusError::NoClassic3d("3D resources").into())
    }

    fn resource_unref(&mut self, _resource_id: u32) {}

    fn ctx_attach_resource(&mut self, _ctx_id: u32, _resource_id: u32) {}

    fn ctx_detach_resource(&mut self, _ctx_id: u32, _resource_id: u32) {}

    /// A blob attached to or detached from a venus context (stage 5c): for a
    /// blob of `VkDeviceMemory`, whether that context may import it.
    fn ctx_attach_blob(&mut self, ctx_id: u32, resource_id: u32, attach: bool) {
        if self.contexts.contains_key(&ctx_id) || !attach {
            self.directory.attach(resource_id, ctx_id, attach);
        }
    }

    fn attach_backing(
        &mut self,
        resource_id: u32,
        _mem: &Arc<GuestMem>,
        _entries: &[MemEntry],
    ) -> Result<(), CommandError> {
        Err(CommandError::UnknownResource(resource_id))
    }

    fn detach_backing(&mut self, _resource_id: u32) {}

    fn transfer_to_host(&mut self, _ctx_id: u32, xfer: &Transfer3d) -> Result<(), CommandError> {
        Err(CommandError::UnknownResource(xfer.resource_id))
    }

    fn transfer_from_host(&mut self, _ctx_id: u32, xfer: &Transfer3d) -> Result<(), CommandError> {
        Err(CommandError::UnknownResource(xfer.resource_id))
    }

    fn submit(&mut self, ctx_id: u32, stream: &[u8]) -> Result<(), CommandError> {
        self.dispatch(ctx_id, stream)?;
        Ok(())
    }

    /// Only a blob accepted for scanout ([`Self::scanout_blob`]) has pixels
    /// to read (stage S2b); there is no other resource here.
    fn read_rect_bgra(
        &mut self,
        resource_id: u32,
        rect: Rect,
        out: &mut Vec<u8>,
    ) -> Result<(), CommandError> {
        self.read_scanout(resource_id, rect, out).map_err(|error| {
            tracing::debug!(resource = resource_id, %error, "venus scanout readback failed");
            error.into()
        })
    }

    /// Stage S2b: a page blob is accepted when the image fits its pages; a
    /// handle blob when a canonical image recorded on it is exactly the
    /// image the spec describes ([`scanout_mismatch`]) and the factory could
    /// prepare the renderer's own read of it. A refusal keeps what was
    /// accepted before.
    fn scanout_blob(
        &mut self,
        resource_id: u32,
        spec: &ScanoutBlobSpec,
    ) -> Result<(), CommandError> {
        self.accept_scanout(resource_id, spec).map_err(|error| {
            tracing::warn!(resource = resource_id, ?spec, %error, "venus scanout refused");
            error.into()
        })
    }

    fn reset(&mut self) {
        // Every thread first — signalled all at once so the joins overlap,
        // then joined as the contexts drop — so that nothing is writing a ring
        // by the time the pages go. Order is not load-bearing for memory
        // safety — every thread holds its own `Arc` of its pages, and every
        // `Publication` unmaps before its pages are freed — but it is for
        // "reset means stopped": a worker still running after `reset` returns
        // would be a thread of the old boot writing the new one's memory.
        for context in self.contexts.values() {
            context.signal_stop();
        }
        self.contexts.clear();
        // Every thread is joined: the factory's shared state can go — and
        // with it every host queue's fence thread, joined by the factory,
        // whose last retirements belong to the boot that ends here: the
        // device drops its held responses on reset, and a stale retirement
        // must not complete a new boot's fence of the same id.
        self.sinks.reset();
        self.retirer.clear();
        self.directory.clear();
        self.blobs.clear();
        self.blob_bytes = 0;
        self.shm_blobs = 0;
        self.memory_blobs = 0;
        self.observed = 0;
    }

    fn snapshot_refusal(&self) -> Option<String> {
        let pending = self.sinks.pending_ring_fences() + self.retirer.len();
        if pending > 0 {
            // A held response is a descriptor chain the guest is waiting on
            // for GPU work this process owns; a restored VM would wait for a
            // host fence nothing will ever signal.
            return Some(format!(
                "{pending} virtio-gpu fences on Venus queue timelines are waiting for host GPU \
                 work, which a snapshot cannot carry"
            ));
        }
        let handles = self.handle_blob_count();
        if handles > 0 {
            // Stage S1: a handle blob can outlive every Vulkan object — the
            // exporter's memory freed, its context gone — and it is still a
            // dma-buf the guest holds, of device-local memory only the host
            // driver can read.
            return Some(format!(
                "{handles} Venus blobs are exported device-local GPU memory the guest holds as \
                 dma-bufs, which a snapshot cannot carry"
            ));
        }
        self.sinks.snapshot_refusal()
    }

    fn set_host_waker(&mut self, waker: Arc<dyn HostWaker>) {
        self.retirer.set_waker(waker);
    }

    /// Stage 5b.3. A fence without a `ring_idx` is on the device's timeline
    /// and, as before, already signalled: the context commands before it
    /// have been executed by the time the device asks. `ring_idx` 0 is the
    /// context's CPU timeline, retired the same way for the same reason
    /// (`vkr_context_submit_fence`). Every other `ring_idx` goes to the
    /// factory, which puts a host fence on the queue bound to it; without a
    /// waker nothing would collect it, so it is signalled at once instead
    /// (the trait's contract).
    fn create_fence_on(
        &mut self,
        ctx_id: u32,
        ring_idx: Option<u8>,
        fence_id: u32,
    ) -> Result<(FenceTimeline, FenceOutcome), CommandError> {
        let Some(ring_idx) = ring_idx else {
            return Ok((FenceTimeline::Device, FenceOutcome::Signalled));
        };
        let timeline = FenceTimeline::Ring { ctx_id, ring_idx };
        if ring_idx == 0 {
            return Ok((timeline, FenceOutcome::Signalled));
        }
        if !self.contexts.contains_key(&ctx_id) {
            return Err(VenusError::UnknownContext(ctx_id).into());
        }
        if !self.retirer.has_waker() {
            tracing::debug!(
                ctx_id,
                ring_idx,
                "no host waker: a venus queue fence completes synchronously"
            );
            return Ok((timeline, FenceOutcome::Signalled));
        }
        let fence = RingFence {
            ctx_id,
            ring_idx,
            fence_id,
        };
        match self.sinks.create_ring_fence(fence, &self.retirer) {
            Ok(outcome) => Ok((timeline, outcome)),
            Err(why) => Err(VenusError::RingFence {
                ctx_id,
                ring_idx,
                why,
            }
            .into()),
        }
    }

    fn poll_fence_timelines(&mut self, _still_pending: usize) -> Vec<(FenceTimeline, u32)> {
        self.retirer
            .take()
            .into_iter()
            .map(|f| {
                (
                    FenceTimeline::Ring {
                        ctx_id: f.ctx_id,
                        ring_idx: f.ring_idx,
                    },
                    f.fence_id,
                )
            })
            .collect()
    }

    fn blob_support(&self) -> BlobSupport {
        BlobSupport {
            // Guest-memory blobs never reach a renderer; the device tracks them
            // itself. Saying yes costs nothing and lets a guest use them.
            guest: true,
            host3d: true,
            host_visible_bytes: Some(VENUS_HOST_VISIBLE_BYTES),
            // The bytes a guest reads through this window are *our* pages, put
            // there one blob at a time by `RingPages::publish`. That is the
            // whole mechanism a command ring needs, and it is exclusive with a
            // device-written window (`ShmRegion::host_mapped`).
            host_mapped: true,
        }
    }

    fn create_blob(
        &mut self,
        ctx_id: u32,
        args: &ResourceCreateBlob,
        _mem: &Arc<GuestMem>,
        entries: &[MemEntry],
    ) -> Result<(), CommandError> {
        // `blob_id` 0 is plain shared memory (vkr: `!blob_id && flags ==
        // MAPPABLE`); anything else names a `VkDeviceMemory`.
        if args.blob_id == 0 {
            self.create_host_blob(ctx_id, args, entries)?;
        } else {
            self.create_memory_blob(ctx_id, args, entries)?;
        }
        tracing::debug!(
            ctx_id,
            resource = args.resource_id,
            blob_id = args.blob_id,
            size = args.size,
            "venus host blob created"
        );
        Ok(())
    }

    fn destroy_blob(&mut self, resource_id: u32) {
        self.drop_blob(resource_id);
    }

    fn map_blob(
        &mut self,
        resource_id: u32,
        offset: u64,
        size: u64,
    ) -> Result<BlobMapping, CommandError> {
        Ok(self.map_host_blob(resource_id, offset, size)?)
    }

    fn unmap_blob(&mut self, resource_id: u32, offset: u64) {
        let Some(blob) = self.blobs.get_mut(&resource_id) else {
            return;
        };
        match blob.publication.as_ref().map(Publication::offset) {
            Some(at) if at == offset => {
                // Dropping it calls `unmap_host` and only then releases the
                // pages, which is the order the guest's safety depends on.
                blob.publication = None;
            }
            Some(at) => tracing::warn!(
                resource = resource_id,
                asked = format_args!("{offset:#x}"),
                mapped = format_args!("{at:#x}"),
                "an unmap named an offset this blob is not published at"
            ),
            None => {}
        }
    }

    fn set_quiesce(&mut self, quiesce: Arc<Quiesce>) {
        // Rings already running keep the gate they were started with; the
        // device hands this over at activation, before a guest can have
        // created any.
        self.quiesce = quiesce;
    }

    fn set_host_visible(&mut self, backing: Arc<dyn virtio_core::ShmBacking>) {
        if !backing.host_mapped() {
            // We asked for a host-mapped window in `blob_support`; one that is
            // not is a window our pages cannot go into, and pretending
            // otherwise would fail every `RESOURCE_MAP_BLOB` with a confusing
            // refusal instead of one clear line here.
            tracing::error!(
                len = backing.len(),
                "the venus renderer was given a shared-memory window that is not host-mapped"
            );
            return;
        }
        self.window = Some(backing);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::sync::atomic::{AtomicU32, AtomicU8, Ordering};
    use std::sync::Condvar;
    use std::time::Instant;

    use virtio_core::{ShmAccessError, ShmMapError};

    use crate::protocol::{BLOB_FLAG_USE_MAPPABLE, BLOB_MEM_HOST3D_GUEST};
    use crate::renderer::Gpu3d;
    use crate::venus::pump::{
        PumpError, MAX_IDLE_TIMEOUT, STATUS_ALIVE, STATUS_FATAL, STATUS_IDLE,
    };
    use crate::venus::service::MIN_MONITOR_PERIOD;
    use crate::venus::shmem::MAX_RESOURCE_BYTES;
    use crate::venus::transport::{STYPE_RING_CREATE_INFO_MESA, STYPE_RING_MONITOR_INFO_MESA};
    use crate::venus::wire::{CommandHeader, Encoder, WireError};

    /// The blob every fixture puts its ring in: one 4 KiB page.
    const RESOURCE: u64 = 0x1000;
    /// A deliberately small command buffer, so a wrap is a few bytes away.
    const BUFFER: u64 = 64;
    /// Where the fixture's command buffer starts inside the blob.
    const BUFFER_OFFSET: u64 = 16;
    /// Where inside the window the guest asks for its blob.
    const WINDOW_OFFSET: u64 = 0x2_0000;
    /// The context id the fixtures use.
    const CTX: u32 = 1;
    /// The resource id of the fixture's ring blob.
    const RESOURCE_ID: u32 = 9;
    /// The ring handle the fixtures mint.
    const RING: u64 = 0xdead_beef_0000_0001;
    /// The three control words, as the fixture layout places them.
    const HEAD: u64 = 0;
    const TAIL: u64 = 4;
    const STATUS: u64 = 8;

    /// `vkEnumerateInstanceVersion` exactly as Mesa 26 encodes it: opcode 137,
    /// `GENERATE_REPLY`, a present `pApiVersion` (spec §0.7).
    const ENUMERATE_INSTANCE_VERSION: [u8; 16] =
        [0x89, 0, 0, 0, 1, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0];

    /// Wait — generously — for `cond`, failing with `what` if it never holds.
    /// Every real-thread test here goes through this, so a slow machine costs
    /// time, never a flake.
    fn eventually(what: &str, cond: impl Fn() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while !cond() {
            assert!(Instant::now() < deadline, "timed out waiting for {what}");
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    // ---------------------------------------------------------- a fake window

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum Event {
        Map { offset: u64, addr: u64, len: u64 },
        Unmap { offset: u64 },
    }

    /// A [`ShmBacking`] that records what it was told and hands the test the
    /// host address the renderer published — which is precisely what the guest
    /// would be looking at through the device's shared-memory region. No
    /// hypervisor and no guest: `map_host` taking an address rather than a
    /// descriptor is what makes that possible on any host.
    #[derive(Debug, Default)]
    struct Window {
        events: Mutex<Vec<Event>>,
        host_mapped: bool,
    }

    impl Window {
        fn host_mapped() -> Arc<Self> {
            Arc::new(Self {
                host_mapped: true,
                ..Self::default()
            })
        }

        fn events(&self) -> Vec<Event> {
            self.events.lock().expect("uncontended").clone()
        }

        /// The address of the live mapping at `offset` — where the guest's view
        /// of those pages really is, or `None` once it has been taken down.
        fn mapped_at(&self, offset: u64) -> Option<u64> {
            let mut addr = None;
            for event in self.events() {
                match event {
                    Event::Map {
                        offset: at,
                        addr: a,
                        ..
                    } if at == offset => addr = Some(a),
                    Event::Unmap { offset: at } if at == offset => addr = None,
                    _ => {}
                }
            }
            addr
        }
    }

    impl ShmBacking for Window {
        fn len(&self) -> u64 {
            VENUS_HOST_VISIBLE_BYTES
        }

        fn read(&self, offset: u64, buf: &mut [u8]) -> Result<(), ShmAccessError> {
            Err(ShmAccessError {
                offset,
                len: buf.len() as u64,
                window: self.len(),
            })
        }

        fn write(&self, offset: u64, data: &[u8]) -> Result<(), ShmAccessError> {
            Err(ShmAccessError {
                offset,
                len: data.len() as u64,
                window: self.len(),
            })
        }

        fn fill(&self, offset: u64, len: u64, _byte: u8) -> Result<(), ShmAccessError> {
            Err(ShmAccessError {
                offset,
                len,
                window: self.len(),
            })
        }

        fn host_mapped(&self) -> bool {
            self.host_mapped
        }

        unsafe fn map_host(
            &self,
            offset: u64,
            host_addr: u64,
            len: u64,
        ) -> Result<(), ShmMapError> {
            self.events.lock().expect("uncontended").push(Event::Map {
                offset,
                addr: host_addr,
                len,
            });
            Ok(())
        }

        fn unmap_host(&self, offset: u64) {
            self.events
                .lock()
                .expect("uncontended")
                .push(Event::Unmap { offset });
        }
    }

    // ------------------------------------------------ the guest, played by hand

    /// The guest's view of its ring: the base address the window was handed.
    ///
    /// Everything below is what a guest's `vn_ring` does — aligned atomic stores
    /// to `tail`, relaxed byte stores into the command buffer, atomic loads of
    /// `head` and `status`, and the watchdog's atomic AND on `status` —
    /// performed from outside the renderer, through the published mapping,
    /// exactly as a guest reaches them.
    #[derive(Debug, Clone, Copy)]
    struct GuestView(u64);

    impl GuestView {
        fn word(self, offset: u64) -> &'static AtomicU32 {
            let at = usize::try_from(offset).expect("a fixture offset fits a usize");
            assert!(offset % 4 == 0 && offset + 4 <= RESOURCE);
            // SAFETY: `self.0` is the base of a live `RingPages` allocation of
            // at least `RESOURCE` bytes — the renderer published it and the test
            // holds the blob that owns it for as long as this value is used —
            // and the assertion above keeps the four bytes inside that
            // allocation and on a 4-byte boundary, which is `AtomicU32`'s
            // alignment requirement. The `'static` is a test convenience: no
            // fixture uses a view after its rig is dropped.
            unsafe { AtomicU32::from_ptr((self.0 as *mut u8).add(at).cast::<u32>()) }
        }

        fn store_word(self, offset: u64, value: u32) {
            self.word(offset).store(value, Ordering::SeqCst);
        }

        fn load_word(self, offset: u64) -> u32 {
            self.word(offset).load(Ordering::SeqCst)
        }

        /// The guest's watchdog arming: `vn_ring_unset_status_bits`.
        fn clear_status_bits(self, bits: u32) {
            self.word(STATUS).fetch_and(!bits, Ordering::SeqCst);
        }

        /// Write command bytes at a free-running ring offset, masked and
        /// wrapping the end of the buffer the way the guest's producer does.
        fn produce(self, at: u64, bytes: &[u8]) {
            for (i, byte) in bytes.iter().enumerate() {
                let position = (at + i as u64) % BUFFER;
                let index = BUFFER_OFFSET + position;
                assert!(index < RESOURCE);
                let index = usize::try_from(index).expect("a fixture offset fits a usize");
                // SAFETY: as `word`; the assertion keeps the byte inside the
                // live allocation, and `AtomicU8` needs no alignment beyond a
                // byte.
                unsafe {
                    AtomicU8::from_ptr((self.0 as *mut u8).add(index))
                        .store(*byte, Ordering::Relaxed);
                }
            }
        }

        /// One Mesa ring submission: write at the current `tail`, then store
        /// the new `tail`. Answers whether the guest would now ring the
        /// doorbell — whether it saw `IDLE` (ignoring Mesa's rate limit, which
        /// the tests that care about it model themselves).
        fn submit(self, bytes: &[u8]) -> bool {
            let tail = self.load_word(TAIL);
            self.produce(u64::from(tail), bytes);
            let len = u32::try_from(bytes.len()).expect("a fixture batch fits a u32");
            self.store_word(TAIL, tail.wrapping_add(len));
            self.load_word(STATUS) & STATUS_IDLE != 0
        }
    }

    // ------------------------------------------------------------- the streams

    fn ring_info() -> RingCreateInfo {
        RingCreateInfo {
            flags: 0,
            resource_id: RESOURCE_ID,
            offset: 0,
            size: RESOURCE,
            // One microsecond: the worker parks almost at once, so a test can
            // wait for IDLE rather than for a timeout.
            idle_timeout_ns: 1_000,
            head_offset: HEAD,
            tail_offset: TAIL,
            status_offset: STATUS,
            buffer_offset: BUFFER_OFFSET,
            buffer_size: BUFFER,
            extra_offset: BUFFER_OFFSET + BUFFER,
            extra_size: 4,
        }
    }

    fn blob_args(resource_id: u32, size: u64) -> ResourceCreateBlob {
        ResourceCreateBlob {
            resource_id,
            blob_mem: BLOB_MEM_HOST3D,
            blob_flags: BLOB_FLAG_USE_MAPPABLE,
            nr_entries: 0,
            blob_id: 0,
            size,
        }
    }

    /// `vkCreateRingMESA` on the wire: the handle, a present pointer, the
    /// `sType`, the pNext chain — empty, or one `VkRingMonitorInfoMESA` — then
    /// the body in wire order.
    fn create_ring_stream_with(
        ring: u64,
        info: RingCreateInfo,
        monitor_us: Option<u32>,
    ) -> Vec<u8> {
        let mut enc = Encoder::new();
        let put = |result: Result<(), WireError>| result.expect("the fixture encodes");
        put(enc.command_header(CommandHeader {
            opcode: Opcode::CreateRing.as_u32(),
            flags: 0,
        }));
        put(enc.handle(ring));
        put(enc.simple_pointer(true));
        put(enc.i32(STYPE_RING_CREATE_INFO_MESA));
        match monitor_us {
            None => put(enc.simple_pointer(false)),
            Some(period) => {
                // One link: marker, sType, its own (empty) chain, its body.
                put(enc.simple_pointer(true));
                put(enc.i32(STYPE_RING_MONITOR_INFO_MESA));
                put(enc.simple_pointer(false));
                put(enc.u32(period));
            }
        }
        put(enc.flags(info.flags));
        put(enc.u32(info.resource_id));
        for value in [
            info.offset,
            info.size,
            info.idle_timeout_ns,
            info.head_offset,
            info.tail_offset,
            info.status_offset,
            info.buffer_offset,
            info.buffer_size,
            info.extra_offset,
            info.extra_size,
        ] {
            put(enc.u64(value));
        }
        enc.finish().expect("the fixture encodes")
    }

    fn create_ring_stream(ring: u64, info: RingCreateInfo) -> Vec<u8> {
        create_ring_stream_with(ring, info, None)
    }

    fn notify_stream(ring: u64) -> Vec<u8> {
        let mut enc = Encoder::new();
        enc.command_header(CommandHeader {
            opcode: Opcode::NotifyRing.as_u32(),
            flags: 0,
        })
        .expect("encode");
        enc.handle(ring).expect("encode");
        enc.u32(0).expect("encode");
        enc.flags(0).expect("encode");
        enc.finish().expect("encode")
    }

    fn destroy_stream(ring: u64) -> Vec<u8> {
        let mut enc = Encoder::new();
        enc.command_header(CommandHeader {
            opcode: Opcode::DestroyRing.as_u32(),
            flags: 0,
        })
        .expect("encode");
        enc.handle(ring).expect("encode");
        enc.finish().expect("encode")
    }

    /// `vkSetReplyCommandStreamMESA` as a guest writes it into a ring: 36
    /// bytes (spec §2.1).
    fn set_reply(resource_id: u32, offset: u64, size: u64) -> Vec<u8> {
        let mut enc = Encoder::new();
        enc.command_header(CommandHeader {
            opcode: Opcode::SetReplyCommandStream.as_u32(),
            flags: 0,
        })
        .expect("encode");
        enc.simple_pointer(true).expect("encode");
        enc.u32(resource_id).expect("encode");
        enc.size(offset).expect("encode");
        enc.size(size).expect("encode");
        let bytes = enc.finish().expect("encode");
        assert_eq!(bytes.len(), 36);
        bytes
    }

    // ---------------------------------------------------------------- the sinks

    /// A sink that takes every byte it is offered and keeps them — the
    /// transport tests' stand-in for a renderer that could answer anything, so
    /// they can use arbitrary bytes rather than real Vulkan commands.
    #[derive(Debug, Clone, Default)]
    struct Tap(Arc<Mutex<Vec<u8>>>);

    impl Tap {
        fn bytes(&self) -> Vec<u8> {
            self.0.lock().expect("uncontended").clone()
        }

        fn len(&self) -> usize {
            self.bytes().len()
        }

        fn factory(&self) -> impl SinkFactory<Sink = Self> {
            let tap = self.clone();
            move |_ctx_id: u32, _ring: u64| Ok(tap.clone())
        }
    }

    impl RingSink for Tap {
        fn consume(&mut self, batch: Batch<'_>) -> Consumed {
            self.0
                .lock()
                .expect("uncontended")
                .extend_from_slice(batch.bytes());
            batch.all()
        }
    }

    /// A writer the test can read back while the sink that owns a clone of it
    /// lives on a ring worker.
    #[derive(Debug, Clone, Default)]
    struct SharedVec(Arc<Mutex<Vec<u8>>>);

    impl io::Write for SharedVec {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.0.lock().expect("uncontended").extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    // ------------------------------------------------------------- the fixture

    /// A renderer with a window, a venus context and a mapped ring blob —
    /// everything a guest does before its first `vkCreateRingMESA`.
    struct Rig<F> {
        renderer: VenusRenderer<F>,
        window: Arc<Window>,
        mem: Arc<GuestMem>,
    }

    impl<F: SinkFactory> Rig<F> {
        fn new(sinks: F) -> Self {
            let mut renderer = VenusRenderer::new(sinks);
            let window = Window::host_mapped();
            let mem = Arc::new(virtio_core::testing::guest_memory(1 << 20));

            renderer.set_host_visible(Arc::clone(&window) as Arc<dyn ShmBacking>);
            renderer
                .ctx_create(CTX, crate::CAPSET_VENUS, "venus")
                .expect("a venus context");
            renderer
                .create_blob(CTX, &blob_args(RESOURCE_ID, RESOURCE), &mem, &[])
                .expect("a host blob");
            renderer
                .map_blob(RESOURCE_ID, WINDOW_OFFSET, RESOURCE)
                .expect("the pages go into the window");

            Self {
                renderer,
                window,
                mem,
            }
        }

        fn guest(&self) -> GuestView {
            GuestView(
                self.window
                    .mapped_at(WINDOW_OFFSET)
                    .expect("the blob is published"),
            )
        }

        fn produce(&self, at: u64, bytes: &[u8]) {
            self.guest().produce(at, bytes);
        }

        fn head(&self) -> u32 {
            self.guest().load_word(HEAD)
        }

        fn status(&self) -> u32 {
            self.guest().load_word(STATUS)
        }

        fn create_ring(&mut self) {
            self.create_ring_with(ring_info(), None);
        }

        fn create_ring_with(&mut self, info: RingCreateInfo, monitor_us: Option<u32>) {
            self.renderer
                .submit(CTX, &create_ring_stream_with(RING, info, monitor_us))
                .expect("the fixture layout fits these pages");
        }

        fn doorbell(&mut self) {
            self.renderer
                .submit(CTX, &notify_stream(RING))
                .expect("the doorbell");
        }

        fn wait_head(&self, head: u32) {
            eventually(&format!("head {head}"), || self.head() == head);
        }

        fn wait_parked(&self) {
            eventually("the ring to publish IDLE", || {
                self.status() & STATUS_IDLE != 0
            });
        }

        fn wait_fatal(&self) {
            eventually("the ring to publish FATAL", || {
                self.status() & STATUS_FATAL != 0
            });
            eventually("the worker to end", || {
                self.renderer.ring_stopped(CTX, RING) == Some(true)
            });
        }
    }

    /// The common case: a rig whose rings all feed one [`Tap`] the test keeps.
    fn tap_rig() -> (Rig<impl SinkFactory<Sink = Tap>>, Tap) {
        let tap = Tap::default();
        (Rig::new(tap.factory()), tap)
    }

    // -------------------------------------------------------- what we advertise

    #[test]
    fn the_only_capset_is_venus_and_it_is_the_one_mesa_reads() {
        let mut renderer = VenusRenderer::new(CaptureSink::new().factory());
        assert_eq!(renderer.capsets().len(), 1);
        let info = renderer.capsets()[0];
        assert_eq!(info.id, crate::CAPSET_VENUS);
        assert_eq!(info.max_size as usize, VENUS_CAPSET_LEN);

        let blob = renderer
            .capset(crate::CAPSET_VENUS, 0)
            .expect("the venus capset");
        assert_eq!(blob.len(), VENUS_CAPSET_LEN);
        assert_eq!(
            VenusCapset::parse(&blob),
            Some(VenusCapset::new()),
            "the bytes a guest reads are the capset this project serves"
        );

        // Nothing else is served, in either axis.
        for (id, version) in [
            (crate::CAPSET_VIRGL, 0),
            (crate::CAPSET_VIRGL2, 0),
            (0, 0),
            (crate::CAPSET_VENUS, 1),
        ] {
            assert!(
                matches!(
                    renderer.capset(id, version),
                    Err(CommandError::UnknownCapset { .. })
                ),
                "capset {id} v{version} was served"
            );
        }

        // And the validation front in front of it agrees: a venus context is a
        // real context type here, a virgl one is not.
        let mut gpu = Gpu3d::new(Box::new(VenusRenderer::new(CaptureSink::new().factory())));
        assert!(gpu.serves_venus());
        assert!(gpu.has_context_types());
        assert_eq!(gpu.num_capsets(), 1);
        gpu.ctx_create(CTX, crate::CAPSET_VENUS, "venus")
            .expect("a venus context");
        assert!(matches!(
            gpu.ctx_create(2, crate::CAPSET_VIRGL, "virgl"),
            Err(CommandError::UnsupportedContextType(_))
        ));
        // A classic virgl context (`context_init = 0`) reaches the renderer,
        // which refuses it: this renderer executes no Gallium command stream.
        assert!(matches!(
            gpu.ctx_create(3, 0, "classic"),
            Err(CommandError::UnsupportedContextType(0))
        ));
    }

    #[test]
    fn the_blob_support_is_a_host_mapped_window_because_a_ring_needs_one() {
        let renderer = VenusRenderer::new(CaptureSink::new().factory());
        let support = renderer.blob_support();
        assert!(support.any());
        assert!(support.host3d);
        assert!(support.host_mapped, "a ring's atomics are host pages");
        assert_eq!(support.host_visible_bytes, Some(VENUS_HOST_VISIBLE_BYTES));
        assert!(support.accepts(BLOB_MEM_HOST3D));
    }

    // ----------------------------------------------------------- the whole flow

    #[test]
    fn a_guest_builds_a_ring_and_every_byte_it_writes_reaches_the_sink() {
        let (mut rig, tap) = tap_rig();

        // The pages really went into the window: whole, and at the offset the
        // guest named.
        assert_eq!(rig.window.events().len(), 1);
        assert!(matches!(
            rig.window.events()[0],
            Event::Map { offset, len, .. } if offset == WINDOW_OFFSET && len >= RESOURCE
        ));
        let guest = rig.guest();

        rig.create_ring();
        assert_eq!(rig.renderer.ring_count(), 1);
        assert_eq!(rig.renderer.live_threads(), 1, "one worker, no monitor");

        // Nothing to do for a whole `idleTimeout`: IDLE goes up, so the guest
        // knows to ring the doorbell.
        rig.wait_parked();
        assert_eq!(guest.load_word(HEAD), 0, "head starts at zero");

        // Eleven bytes, then the doorbell.
        assert!(
            guest.submit(b"hello venus"),
            "a parked ring asks for a doorbell"
        );
        rig.doorbell();
        rig.wait_head(11);
        assert_eq!(tap.bytes(), b"hello venus".to_vec());
        rig.wait_parked();
        assert_eq!(rig.renderer.ring_state(CTX, RING), Some((11, STATUS_IDLE)));

        // A second batch, continuing from the cursor and wrapping the end of the
        // 64-byte buffer — spliced back into one run by the pump.
        let more: Vec<u8> = (0u8..60).collect();
        guest.submit(&more);
        rig.doorbell();
        rig.wait_head(71);
        let mut want = b"hello venus".to_vec();
        want.extend_from_slice(&more);
        assert_eq!(tap.bytes(), want);

        // Tearing a healthy ring down joins its worker and zeroes the words the
        // guest polls: a reboot that leaves a stale `head` in shared memory is
        // ADR-0005's haunting.
        rig.renderer
            .submit(CTX, &destroy_stream(RING))
            .expect("the ring is torn down");
        assert_eq!(rig.renderer.ring_count(), 0);
        assert_eq!(rig.renderer.live_threads(), 0, "the worker was joined");
        assert_eq!(guest.load_word(HEAD), 0);
        assert_eq!(guest.load_word(STATUS), 0);
    }

    #[test]
    fn one_doorbell_consumes_everything_that_arrived_since_the_last_one() {
        let (mut rig, tap) = tap_rig();
        rig.create_ring();
        rig.wait_parked();
        let guest = rig.guest();

        // Three separate productions, one doorbell.
        rig.produce(0, b"aaa");
        rig.produce(3, b"bbbb");
        rig.produce(7, b"cc");
        guest.store_word(TAIL, 9);
        rig.doorbell();
        rig.wait_head(9);
        assert_eq!(tap.bytes(), b"aaabbbbcc".to_vec());

        // A doorbell with nothing waiting is legal and consumes nothing.
        rig.wait_parked();
        rig.doorbell();
        rig.wait_parked();
        assert_eq!(tap.len(), 9);
    }

    #[test]
    fn work_queued_before_or_straight_after_ring_creation_needs_no_doorbell() {
        // The ring starts polling, as virglrenderer's does: `tail` is the
        // guest's word, and it may have queued work before the host looked.
        let (mut rig, tap) = tap_rig();
        rig.produce(0, b"early");
        rig.guest().store_word(TAIL, 5);

        let mut stream = create_ring_stream(RING, ring_info());
        stream.extend_from_slice(&notify_stream(RING));
        stream.extend_from_slice(&notify_stream(RING));
        rig.renderer.submit(CTX, &stream).expect("all three");

        rig.wait_head(5);
        assert_eq!(tap.bytes(), b"early".to_vec());
        assert_eq!(rig.renderer.ring_count(), 1);
    }

    /// The fix this stage exists for, with real threads: two submissions
    /// inside one `idleTimeout`, and only the first rang the doorbell — which
    /// is what Mesa's one-per-millisecond rate limit makes of every
    /// reply-bearing command. The previous, synchronous renderer consumed the
    /// first, republished IDLE and never looked at the second.
    #[test]
    fn two_submissions_with_one_doorbell_are_both_consumed_by_the_worker() {
        let window = MAX_IDLE_TIMEOUT;
        for attempt in 0..3 {
            let (mut rig, tap) = tap_rig();
            rig.create_ring_with(
                RingCreateInfo {
                    idle_timeout_ns: u64::try_from(window.as_nanos()).expect("fits"),
                    ..ring_info()
                },
                None,
            );
            rig.wait_parked();
            let guest = rig.guest();

            // Submission one sees IDLE and rings.
            assert!(guest.submit(&set_reply(3, 0, 20)));
            rig.doorbell();
            rig.wait_head(36);
            let seen = Instant::now();

            // Submission two, rate-limited: no doorbell, whatever the status.
            guest.submit(&ENUMERATE_INSTANCE_VERSION);
            if seen.elapsed() > window / 2 {
                // The test thread was descheduled for half the window; the
                // worker may legitimately have parked. Inconclusive — go again
                // rather than flake.
                eprintln!("attempt {attempt}: descheduled, retrying");
                continue;
            }
            rig.wait_head(52);
            let mut want = set_reply(3, 0, 20);
            want.extend_from_slice(&ENUMERATE_INSTANCE_VERSION);
            assert_eq!(tap.bytes(), want);
            // And with a whole window of nothing, it parks again.
            rig.wait_parked();
            return;
        }
        panic!("three attempts in a row were descheduled for half a 100 ms window");
    }

    #[test]
    fn a_parked_ring_waits_for_its_doorbell_and_a_refused_command_does_not_swallow_it() {
        // A guest batches its ring commands, so a content refusal must not turn
        // into a silently skipped `vkNotifyRingMESA` — which the guest would
        // experience as a hang rather than as an error.
        let (mut rig, tap) = tap_rig();
        rig.create_ring();
        rig.wait_parked();

        // Parked means parked: a submission with no doorbell stays unread.
        rig.produce(0, b"behind it");
        rig.guest().store_word(TAIL, 9);
        std::thread::sleep(Duration::from_millis(30));
        assert_eq!(rig.head(), 0, "a parked ring consumed without a doorbell");

        // A duplicate ring handle (refused), then the doorbell (performed).
        let mut stream = create_ring_stream(RING, ring_info());
        stream.extend_from_slice(&notify_stream(RING));
        assert_eq!(
            rig.renderer.dispatch(CTX, &stream),
            Err(VenusError::DuplicateRing {
                ctx_id: CTX,
                ring: RING
            }),
            "the command is answered with the first refusal"
        );
        rig.wait_head(9);
        assert_eq!(tap.bytes(), b"behind it".to_vec());
    }

    #[test]
    fn each_ring_gets_its_own_sink_and_they_never_interleave() {
        // Two contexts — two guest `VkInstance`s — one ring each. The phase-4
        // capture put both into one file; here each has its own.
        let made: Arc<Mutex<Vec<(u32, u64, Tap)>>> = Arc::default();
        let factory = {
            let made = Arc::clone(&made);
            move |ctx_id: u32, ring: u64| {
                let tap = Tap::default();
                made.lock()
                    .expect("uncontended")
                    .push((ctx_id, ring, tap.clone()));
                Ok(tap)
            }
        };
        let mut rig = Rig::new(factory);
        rig.renderer
            .ctx_create(2, crate::CAPSET_VENUS, "second")
            .expect("a second context");
        rig.renderer
            .create_blob(2, &blob_args(21, RESOURCE), &rig.mem, &[])
            .expect("its blob");
        rig.renderer
            .map_blob(21, WINDOW_OFFSET + 0x1_0000, RESOURCE)
            .expect("published");
        let second = GuestView(
            rig.window
                .mapped_at(WINDOW_OFFSET + 0x1_0000)
                .expect("published"),
        );

        rig.create_ring();
        rig.renderer
            .dispatch(
                2,
                &create_ring_stream(
                    RING + 1,
                    RingCreateInfo {
                        resource_id: 21,
                        ..ring_info()
                    },
                ),
            )
            .expect("the second ring");

        rig.guest().submit(b"first");
        second.submit(b"SECOND");
        rig.doorbell();
        rig.renderer
            .dispatch(2, &notify_stream(RING + 1))
            .expect("its doorbell");
        rig.wait_head(5);
        eventually("the second ring's head", || second.load_word(HEAD) == 6);

        let made = made.lock().expect("uncontended");
        assert_eq!(made.len(), 2);
        for (ctx_id, ring, tap) in made.iter() {
            match (*ctx_id, *ring) {
                (CTX, RING) => assert_eq!(tap.bytes(), b"first".to_vec()),
                (2, r) if r == RING + 1 => assert_eq!(tap.bytes(), b"SECOND".to_vec()),
                other => panic!("a sink was made for {other:?}"),
            }
        }
    }

    #[test]
    fn a_ring_whose_sink_cannot_be_made_is_refused_by_name() {
        let mut rig = Rig::new(|_ctx_id: u32, _ring: u64| -> io::Result<Tap> {
            Err(io::Error::other("the capture directory is gone"))
        });
        assert!(matches!(
            rig.renderer
                .dispatch(CTX, &create_ring_stream(RING, ring_info())),
            Err(VenusError::SinkUnavailable {
                ctx_id: CTX,
                ring: RING,
                ..
            })
        ));
        assert_eq!(rig.renderer.ring_count(), 0);
        assert_eq!(rig.renderer.live_threads(), 0);
        // Nothing was written into the ring either: it is still a fresh one.
        assert_eq!(rig.status(), 0);
    }

    #[test]
    fn the_commands_this_renderer_carries_but_cannot_perform_are_answered_honestly() {
        let (mut rig, _tap) = tap_rig();

        // Carried and counted: refusing these would stop a real guest before it
        // ever built a ring.
        let mut enc = Encoder::new();
        enc.command_header(CommandHeader {
            opcode: Opcode::SeekReplyCommandStream.as_u32(),
            flags: 0,
        })
        .expect("encode");
        enc.size(0x40).expect("encode");
        let seek = enc.finish().expect("encode");
        rig.renderer.submit(CTX, &seek).expect("carried");
        assert_eq!(rig.renderer.observed_commands(), 1);

        // Refused by name: a ring-only command on the context stream would block
        // the very context that has to serve the ring it waits on.
        let mut enc = Encoder::new();
        enc.command_header(CommandHeader {
            opcode: Opcode::WaitVirtqueueSeqno.as_u32(),
            flags: 0,
        })
        .expect("encode");
        enc.u64(7).expect("encode");
        let wait = enc.finish().expect("encode");
        assert_eq!(
            rig.renderer.dispatch(CTX, &wait),
            Err(VenusError::RingOnlyCommand("vkWaitVirtqueueSeqnoMESA"))
        );

        // Refused because the layers below expose no typed store into a ring's
        // `extra` region — see the comment on the match arm.
        rig.create_ring();
        let mut enc = Encoder::new();
        enc.command_header(CommandHeader {
            opcode: Opcode::WriteRingExtra.as_u32(),
            flags: 0,
        })
        .expect("encode");
        enc.handle(RING).expect("encode");
        enc.size(0).expect("encode");
        enc.u32(0x1234).expect("encode");
        let extra = enc.finish().expect("encode");
        assert!(matches!(
            rig.renderer.dispatch(CTX, &extra),
            Err(VenusError::Unimplemented {
                command: "vkWriteRingExtraMESA",
                ..
            })
        ));

        // The context survived all of it: these were content refusals, not
        // framing ones.
        rig.renderer.submit(CTX, &seek).expect("still usable");
    }

    // ---------------------------------------------------------- hostile layouts

    #[test]
    fn a_ring_whose_layout_does_not_fit_the_pages_is_refused_by_name() {
        let (mut rig, _tap) = tap_rig();

        // A buffer that starts inside the resource and ends past it.
        let overhanging = RingCreateInfo {
            buffer_offset: RESOURCE - 16,
            buffer_size: BUFFER,
            extra_offset: 0,
            extra_size: 0,
            ..ring_info()
        };
        assert!(matches!(
            rig.renderer
                .dispatch(CTX, &create_ring_stream(RING, overhanging)),
            Err(VenusError::Layout(
                RingLayoutError::RegionOutsideRing { .. }
            ))
        ));

        // A ring region claiming more of the resource than exists. The layout is
        // judged against the *pages'* own length, so a bigger declared `size`
        // buys the guest nothing.
        let oversized = RingCreateInfo {
            size: RESOURCE * 4,
            ..ring_info()
        };
        assert!(matches!(
            rig.renderer
                .dispatch(CTX, &create_ring_stream(RING, oversized)),
            Err(VenusError::Layout(
                RingLayoutError::RingOutsideResource { .. }
            ))
        ));

        // Command buffers that are not a non-zero power of two.
        for buffer_size in [63u64, 0] {
            let info = RingCreateInfo {
                buffer_size,
                extra_offset: 0,
                extra_size: 0,
                ..ring_info()
            };
            assert!(
                matches!(
                    rig.renderer.dispatch(CTX, &create_ring_stream(RING, info)),
                    Err(VenusError::Layout(_))
                ),
                "a {buffer_size}-byte command buffer was accepted"
            );
        }

        // Overlapping regions: the tail word inside the command buffer.
        let overlapping = RingCreateInfo {
            tail_offset: 20,
            ..ring_info()
        };
        assert!(matches!(
            rig.renderer
                .dispatch(CTX, &create_ring_stream(RING, overlapping)),
            Err(VenusError::Layout(RingLayoutError::RegionsOverlap { .. }))
        ));

        // A ring whose host-owned words are not zeroed: the guest is either
        // confused about who owns them or trying to start the host mid-stream.
        rig.guest().store_word(HEAD, 1);
        assert!(matches!(
            rig.renderer
                .dispatch(CTX, &create_ring_stream(RING, ring_info())),
            Err(VenusError::Shmem(ShmemError::Pump(
                PumpError::ControlWordsNotZeroed { .. }
            )))
        ));
        rig.guest().store_word(HEAD, 0);

        // A monitor asking for an ALIVE period of zero, which the reference
        // refuses too.
        assert_eq!(
            rig.renderer
                .dispatch(CTX, &create_ring_stream_with(RING, ring_info(), Some(0))),
            Err(VenusError::ZeroMonitorPeriod {
                ctx_id: CTX,
                ring: RING
            })
        );

        // None of that left a ring or a thread behind, and the context is still
        // usable — a refused layout is a content refusal, not a framing one.
        assert_eq!(rig.renderer.ring_count(), 0);
        assert_eq!(rig.renderer.live_threads(), 0);
        rig.create_ring();
        assert_eq!(rig.renderer.ring_count(), 1);
    }

    #[test]
    fn a_ring_can_only_be_built_on_a_blob_its_own_context_owns() {
        let (mut rig, _tap) = tap_rig();

        // A resource that is no blob of ours at all.
        let elsewhere = RingCreateInfo {
            resource_id: 77,
            ..ring_info()
        };
        assert_eq!(
            rig.renderer
                .dispatch(CTX, &create_ring_stream(RING, elsewhere)),
            Err(VenusError::UnknownRingResource(77))
        );

        // A blob another context created. The id names nothing from here.
        rig.renderer
            .ctx_create(2, crate::CAPSET_VENUS, "other")
            .expect("a second context");
        rig.renderer
            .create_blob(2, &blob_args(21, RESOURCE), &rig.mem, &[])
            .expect("its own blob");
        let theirs = RingCreateInfo {
            resource_id: 21,
            ..ring_info()
        };
        assert_eq!(
            rig.renderer
                .dispatch(CTX, &create_ring_stream(RING, theirs)),
            Err(VenusError::ForeignRingResource {
                resource_id: 21,
                owner: 2,
                ctx_id: CTX,
            })
        );
        // Its owner may, though.
        rig.renderer
            .dispatch(2, &create_ring_stream(RING, theirs))
            .expect("the owning context");
    }

    #[test]
    fn two_rings_cannot_share_one_handle_and_a_doorbell_needs_a_ring() {
        let (mut rig, _tap) = tap_rig();
        rig.create_ring();

        assert_eq!(
            rig.renderer
                .dispatch(CTX, &create_ring_stream(RING, ring_info())),
            Err(VenusError::DuplicateRing {
                ctx_id: CTX,
                ring: RING
            })
        );
        assert_eq!(rig.renderer.ring_count(), 1, "the first ring is untouched");
        assert_eq!(rig.renderer.live_threads(), 1, "and no second worker");

        // A doorbell for a handle nobody minted.
        assert_eq!(
            rig.renderer.dispatch(CTX, &notify_stream(RING + 1)),
            Err(VenusError::UnknownRing {
                ctx_id: CTX,
                ring: RING + 1
            })
        );
        // …including one another context holds: a ring is looked up inside the
        // context that created it, so a handle is never a way across.
        rig.renderer
            .ctx_create(2, crate::CAPSET_VENUS, "other")
            .expect("a second context");
        assert_eq!(
            rig.renderer.dispatch(2, &notify_stream(RING)),
            Err(VenusError::UnknownRing {
                ctx_id: 2,
                ring: RING
            })
        );
        // And destroying one that does not exist is a refusal, not a silence.
        assert_eq!(
            rig.renderer.dispatch(CTX, &destroy_stream(RING + 1)),
            Err(VenusError::UnknownRing {
                ctx_id: CTX,
                ring: RING + 1
            })
        );
    }

    #[test]
    fn a_context_that_was_never_created_gets_nothing() {
        let (mut rig, _tap) = tap_rig();
        assert!(matches!(
            rig.renderer.submit(99, &notify_stream(RING)),
            Err(CommandError::UnknownContext(99))
        ));
        assert_eq!(
            rig.renderer
                .dispatch(99, &create_ring_stream(RING, ring_info())),
            Err(VenusError::UnknownContext(99))
        );
        assert!(matches!(
            rig.renderer
                .create_blob(99, &blob_args(31, RESOURCE), &rig.mem, &[]),
            Err(CommandError::UnknownContext(99))
        ));

        // A destroyed context takes its rings — and their threads — with it and
        // stops answering.
        rig.create_ring_with(ring_info(), Some(3_000_000));
        assert_eq!(rig.renderer.live_threads(), 2, "a worker and a monitor");
        rig.renderer.ctx_destroy(CTX);
        assert_eq!(rig.renderer.ring_count(), 0);
        assert_eq!(rig.renderer.live_threads(), 0, "both were joined");
        assert!(matches!(
            rig.renderer.submit(CTX, &notify_stream(RING)),
            Err(CommandError::UnknownContext(CTX))
        ));
    }

    // ------------------------------------------------------------ hostile blobs

    #[test]
    fn a_blob_cannot_be_mapped_twice_and_only_its_own_offset_unmaps_it() {
        let (mut rig, _tap) = tap_rig();
        assert!(matches!(
            rig.renderer.map_blob(RESOURCE_ID, 0x4_0000, RESOURCE),
            Err(CommandError::BlobAlreadyMapped(RESOURCE_ID))
        ));
        // Not even at the offset it is already published at.
        assert!(matches!(
            rig.renderer.map_blob(RESOURCE_ID, WINDOW_OFFSET, RESOURCE),
            Err(CommandError::BlobAlreadyMapped(RESOURCE_ID))
        ));
        assert_eq!(rig.window.events().len(), 1, "one map, no unmap");

        // An unmap naming the wrong offset is ignored rather than obeyed —
        // taking the mapping down would leave the guest reading a hole.
        rig.renderer.unmap_blob(RESOURCE_ID, 0x4_0000);
        assert_eq!(rig.window.events().len(), 1);
        assert!(rig.window.mapped_at(WINDOW_OFFSET).is_some());

        // The right one works, and then it may be published again.
        rig.renderer.unmap_blob(RESOURCE_ID, WINDOW_OFFSET);
        assert_eq!(
            rig.window.events().last(),
            Some(&Event::Unmap {
                offset: WINDOW_OFFSET
            })
        );
        rig.renderer
            .map_blob(RESOURCE_ID, 0x4_0000, RESOURCE)
            .expect("a second publication after the first came down");

        // A span that is not the blob's whole length is a host-side
        // disagreement, and is refused rather than published.
        rig.renderer
            .create_blob(CTX, &blob_args(42, RESOURCE), &rig.mem, &[])
            .expect("another blob");
        assert!(matches!(
            rig.renderer.map_blob(42, 0x6_0000, RESOURCE / 2),
            Err(CommandError::Renderer(_))
        ));
        // …and an unknown blob is published nowhere.
        assert!(matches!(
            rig.renderer.map_blob(4242, 0x6_0000, RESOURCE),
            Err(CommandError::UnknownResource(4242))
        ));
    }

    #[test]
    fn the_blob_types_and_sizes_this_renderer_will_not_back_are_refused() {
        let (mut rig, _tap) = tap_rig();

        // `HOST3D_GUEST` would carry guest pages too, and a ring cannot live in
        // them: its control words are atomics the host performs its own accesses
        // on.
        let host3d_guest = ResourceCreateBlob {
            blob_mem: BLOB_MEM_HOST3D_GUEST,
            ..blob_args(11, RESOURCE)
        };
        assert!(matches!(
            rig.renderer.create_blob(CTX, &host3d_guest, &rig.mem, &[]),
            Err(CommandError::UnsupportedBlobMem(BLOB_MEM_HOST3D_GUEST))
        ));

        // A host blob arriving with a page list is the guest and the device
        // disagreeing about what was just created.
        let entries = [MemEntry {
            addr: 0x4000,
            length: 0x1000,
        }];
        assert!(matches!(
            rig.renderer
                .create_blob(CTX, &blob_args(12, RESOURCE), &rig.mem, &entries),
            Err(CommandError::UnknownResource(12))
        ));

        // A duplicate id.
        assert!(matches!(
            rig.renderer
                .create_blob(CTX, &blob_args(RESOURCE_ID, RESOURCE), &rig.mem, &[]),
            Err(CommandError::DuplicateResource(RESOURCE_ID))
        ));

        // Sizes `RingPages` will not allocate: zero, and past its own cap.
        for size in [0u64, MAX_RESOURCE_BYTES + 1] {
            assert!(
                rig.renderer
                    .create_blob(CTX, &blob_args(13, size), &rig.mem, &[])
                    .is_err(),
                "a {size:#x}-byte blob was allocated"
            );
        }

        // The total-bytes budget: what stops a guest turning blob creation into
        // host memory exhaustion.
        let chunk = 8u64 << 20;
        let mut created = 0u32;
        loop {
            match rig
                .renderer
                .create_blob(CTX, &blob_args(100 + created, chunk), &rig.mem, &[])
            {
                Ok(()) => created += 1,
                Err(CommandError::OutOfMemory) => break,
                Err(other) => panic!("unexpected refusal: {other}"),
            }
            assert!(created < 64, "the budget never bit");
        }
        assert_eq!(
            u64::from(created),
            (MAX_RING_BLOB_BYTES - RESOURCE) / chunk,
            "the budget is exact, and the fixture's own blob counts against it"
        );

        // Destroying one frees its share of the budget again.
        rig.renderer.destroy_blob(100);
        rig.renderer
            .create_blob(CTX, &blob_args(200, chunk), &rig.mem, &[])
            .expect("the freed budget is reusable");
    }

    #[test]
    fn destroying_a_blob_takes_its_rings_their_threads_and_its_mapping_with_it() {
        let (mut rig, _tap) = tap_rig();
        rig.create_ring_with(ring_info(), Some(3_000_000));

        rig.renderer.destroy_blob(RESOURCE_ID);
        assert_eq!(rig.renderer.blob_count(), 0);
        assert_eq!(rig.renderer.ring_count(), 0, "the ring went with its pages");
        assert_eq!(
            rig.renderer.live_threads(),
            1,
            "the worker was joined; the context's monitor stays with the context"
        );
        assert_eq!(
            rig.window.events().last(),
            Some(&Event::Unmap {
                offset: WINDOW_OFFSET
            }),
            "the publication came down before the pages were freed"
        );
        assert_eq!(
            rig.renderer.dispatch(CTX, &notify_stream(RING)),
            Err(VenusError::UnknownRing {
                ctx_id: CTX,
                ring: RING
            })
        );
    }

    #[test]
    fn without_a_host_mapped_window_there_is_nowhere_to_put_the_pages() {
        let mem = Arc::new(virtio_core::testing::guest_memory(1 << 20));
        let mut renderer = VenusRenderer::new(CaptureSink::new().factory());
        renderer
            .ctx_create(CTX, crate::CAPSET_VENUS, "venus")
            .expect("a context");
        renderer
            .create_blob(CTX, &blob_args(RESOURCE_ID, RESOURCE), &mem, &[])
            .expect("the pages are ours either way");
        assert!(matches!(
            renderer.map_blob(RESOURCE_ID, 0, RESOURCE),
            Err(CommandError::NoHostVisibleWindow)
        ));

        // A window that shows its own pages is refused at installation, not
        // discovered later as a confusing map failure.
        let plain = Arc::new(Window::default());
        renderer.set_host_visible(Arc::clone(&plain) as Arc<dyn ShmBacking>);
        assert!(matches!(
            renderer.map_blob(RESOURCE_ID, 0, RESOURCE),
            Err(CommandError::NoHostVisibleWindow)
        ));
        assert!(plain.events().is_empty());
    }

    // ---------------------------------------------------------- sticky failures

    #[test]
    fn a_stream_that_refuses_once_poisons_its_context_for_good() {
        let (mut rig, _tap) = tap_rig();

        // An opcode with no decoder. A Venus stream has no length field, so this
        // is not a command to step over — it is the end of the stream.
        let mut enc = Encoder::new();
        enc.command_header(CommandHeader {
            opcode: 4242,
            flags: 0,
        })
        .expect("encode");
        let garbage = enc.finish().expect("encode");

        assert_eq!(
            rig.renderer.dispatch(CTX, &garbage),
            Err(VenusError::Transport(TransportError::UnknownOpcode {
                opcode: 4242
            }))
        );

        // A perfectly well-formed stream afterwards gets nothing: there is no
        // position after the refusal worth decoding from.
        assert_eq!(
            rig.renderer
                .dispatch(CTX, &create_ring_stream(RING, ring_info())),
            Err(VenusError::ContextPoisoned(CTX))
        );
        assert!(matches!(
            rig.renderer.submit(CTX, &notify_stream(RING)),
            Err(CommandError::InvalidStream(_))
        ));
        assert_eq!(rig.renderer.ring_count(), 0);

        // A *different* context is unaffected: the poison belongs to the stream,
        // not to the renderer.
        rig.renderer
            .ctx_create(2, crate::CAPSET_VENUS, "healthy")
            .expect("a second context");
        rig.renderer
            .create_blob(2, &blob_args(21, RESOURCE), &rig.mem, &[])
            .expect("its own blob");
        rig.renderer
            .dispatch(
                2,
                &create_ring_stream(
                    RING,
                    RingCreateInfo {
                        resource_id: 21,
                        ..ring_info()
                    },
                ),
            )
            .expect("an untouched context still works");
    }

    #[test]
    fn a_truncated_command_is_a_refusal_rather_than_a_guess() {
        let (mut rig, _tap) = tap_rig();
        let mut short = create_ring_stream(RING, ring_info());
        short.truncate(short.len() - 4);
        assert!(matches!(
            rig.renderer.dispatch(CTX, &short),
            Err(VenusError::Transport(TransportError::Wire(_)))
        ));
        assert_eq!(rig.renderer.ring_count(), 0);
        assert_eq!(
            rig.renderer.dispatch(CTX, &notify_stream(RING)),
            Err(VenusError::ContextPoisoned(CTX))
        );
    }

    #[test]
    fn a_ring_the_guest_lied_to_is_dead_and_stays_dead() {
        let (mut rig, tap) = tap_rig();
        rig.create_ring();
        rig.wait_parked();
        let guest = rig.guest();

        // A tail claiming more bytes than the ring can possibly hold — which is
        // also how a backwards tail arrives, in wrapping arithmetic.
        guest.store_word(TAIL, 0x7fff_ffff);
        rig.doorbell();
        // The guest is told, so its driver aborts instead of waiting on a head
        // that will never move again, and the worker ends.
        rig.wait_fatal();
        assert_eq!(guest.load_word(HEAD), 0, "nothing was consumed");
        assert_eq!(tap.len(), 0);
        assert_eq!(rig.renderer.live_threads(), 0, "the worker ended");

        // And the ring stays dead: a legal tail afterwards changes nothing, and
        // nothing here rebuilds a pump over a ring that has already failed.
        guest.store_word(TAIL, 4);
        rig.produce(0, b"late");
        assert_eq!(
            rig.renderer.dispatch(CTX, &notify_stream(RING)),
            Err(VenusError::RingStopped {
                ctx_id: CTX,
                ring: RING
            })
        );
        assert_eq!(tap.len(), 0);
        assert_eq!(rig.head(), 0);

        // Destroying a fatal ring leaves its FATAL bit standing: the guest is
        // entitled to read why its ring died, and a resource that produced an
        // impossible `tail` is not one to hand back looking fresh.
        rig.renderer
            .dispatch(CTX, &destroy_stream(RING))
            .expect("it can still be torn down");
        assert_eq!(guest.load_word(STATUS) & STATUS_FATAL, STATUS_FATAL);
    }

    #[test]
    fn a_sink_that_takes_nothing_from_a_full_ring_is_reported_rather_than_hung() {
        /// A sink that refuses everything — a stand-in for a decoder that cannot
        /// make a whole command out of what it is shown.
        struct Refuses;
        impl RingSink for Refuses {
            fn consume(&mut self, batch: Batch<'_>) -> Consumed {
                batch.nothing()
            }
        }

        let mut rig = Rig::new(|_: u32, _: u64| Ok(Refuses));
        rig.create_ring();
        rig.wait_parked();
        let guest = rig.guest();

        // A partial batch is a stall, not a failure: the guest may yet produce
        // the rest, and the ring parks on it rather than spinning.
        rig.produce(0, &[7u8; 8]);
        guest.store_word(TAIL, 8);
        rig.doorbell();
        std::thread::sleep(Duration::from_millis(20));
        rig.wait_parked();
        assert_eq!(rig.renderer.ring_stopped(CTX, RING), Some(false));

        // A *full* ring the sink refuses is a dead end — the guest cannot
        // produce past `head + size`, and `head` only moves when the sink
        // consumes. Say so rather than let the guest wait forever.
        guest.store_word(TAIL, BUFFER as u32);
        rig.doorbell();
        rig.wait_fatal();
        assert_eq!(rig.head(), 0);
    }

    // ---------------------------------------------------------------- the caps

    #[test]
    fn the_context_cap_and_the_context_id_rules_hold() {
        let mut renderer = VenusRenderer::new(CaptureSink::new().factory());
        for id in 1..=MAX_VENUS_CONTEXTS as u32 {
            renderer
                .ctx_create(id, crate::CAPSET_VENUS, "")
                .expect("under the cap");
        }
        assert!(matches!(
            renderer.ctx_create(u32::MAX, crate::CAPSET_VENUS, ""),
            Err(CommandError::TooManyContexts)
        ));
        assert!(matches!(
            renderer.ctx_create(1, crate::CAPSET_VENUS, ""),
            Err(CommandError::BadContextId(1))
        ));
        assert!(matches!(
            renderer.ctx_create(0, crate::CAPSET_VENUS, ""),
            Err(CommandError::BadContextId(0))
        ));
        assert!(matches!(
            renderer.ctx_create(u32::MAX, crate::CAPSET_VIRGL, ""),
            Err(CommandError::UnsupportedContextType(crate::CAPSET_VIRGL))
        ));
        assert_eq!(renderer.context_type(1), Some(crate::CAPSET_VENUS));
    }

    #[test]
    fn the_blob_count_cap_holds() {
        let mem = Arc::new(virtio_core::testing::guest_memory(1 << 20));
        let mut renderer = VenusRenderer::new(CaptureSink::new().factory());
        renderer
            .ctx_create(CTX, crate::CAPSET_VENUS, "")
            .expect("a context");
        // 64 × 4 KiB is 256 KiB, comfortably under the byte budget, so this is
        // the count cap and nothing else.
        for id in 1..=MAX_RING_BLOBS as u32 {
            renderer
                .create_blob(CTX, &blob_args(id, RESOURCE), &mem, &[])
                .expect("under the cap");
        }
        assert_eq!(renderer.blob_count(), MAX_RING_BLOBS);
        assert!(matches!(
            renderer.create_blob(CTX, &blob_args(9999, RESOURCE), &mem, &[]),
            Err(CommandError::OutOfMemory)
        ));
    }

    #[test]
    fn the_ring_caps_hold_per_context_and_overall_and_bound_the_threads() {
        let mem = Arc::new(virtio_core::testing::guest_memory(1 << 20));
        let mut renderer = VenusRenderer::new(CaptureSink::new().factory());
        let window = Window::host_mapped();
        renderer.set_host_visible(Arc::clone(&window) as Arc<dyn ShmBacking>);

        // One blob per ring: two rings in one resource would have to overlap,
        // and the layout fixture is deliberately the same shape every time.
        fn blob_for<F: SinkFactory>(
            renderer: &mut VenusRenderer<F>,
            mem: &Arc<GuestMem>,
            resource: &mut u32,
            ctx: u32,
        ) -> u32 {
            *resource += 1;
            renderer
                .create_blob(ctx, &blob_args(*resource, RESOURCE), mem, &[])
                .expect("a blob");
            *resource
        }
        let mut resource = 0u32;

        renderer
            .ctx_create(CTX, crate::CAPSET_VENUS, "")
            .expect("a context");
        for slot in 0..MAX_RINGS_PER_CONTEXT as u64 {
            let resource_id = blob_for(&mut renderer, &mem, &mut resource, CTX);
            let info = RingCreateInfo {
                resource_id,
                ..ring_info()
            };
            renderer
                .dispatch(CTX, &create_ring_stream(slot, info))
                .expect("under the per-context cap");
        }
        let spare = blob_for(&mut renderer, &mem, &mut resource, CTX);
        assert_eq!(
            renderer.dispatch(
                CTX,
                &create_ring_stream(
                    999,
                    RingCreateInfo {
                        resource_id: spare,
                        ..ring_info()
                    }
                )
            ),
            Err(VenusError::TooManyRingsOnContext { ctx_id: CTX })
        );

        // The global cap, reached across contexts.
        let mut ctx_id = CTX;
        'outer: loop {
            ctx_id += 1;
            renderer
                .ctx_create(ctx_id, crate::CAPSET_VENUS, "")
                .expect("a context");
            for slot in 0..MAX_RINGS_PER_CONTEXT as u64 {
                let resource_id = blob_for(&mut renderer, &mem, &mut resource, ctx_id);
                let info = RingCreateInfo {
                    resource_id,
                    ..ring_info()
                };
                match renderer.dispatch(ctx_id, &create_ring_stream(slot, info)) {
                    Ok(()) => {}
                    Err(VenusError::TooManyRings) => break 'outer,
                    Err(other) => panic!("unexpected refusal: {other}"),
                }
            }
            assert!(ctx_id < 16, "the global ring cap never bit");
        }
        assert_eq!(renderer.ring_count(), MAX_RINGS);
        // One worker per ring, and the cap on rings is the cap on threads.
        assert_eq!(renderer.live_threads(), MAX_RINGS);
        renderer.reset();
        assert_eq!(renderer.live_threads(), 0);
    }

    // --------------------------------------------------------------- the reset

    #[test]
    fn a_device_reset_joins_every_thread_and_takes_every_page_back_out_of_the_guest() {
        let (mut rig, _tap) = tap_rig();
        rig.create_ring_with(ring_info(), Some(3_000_000));
        assert_eq!(rig.renderer.live_threads(), 2);

        rig.renderer.reset();

        assert_eq!(
            rig.renderer.live_threads(),
            0,
            "reset left a thread running"
        );
        assert_eq!(rig.renderer.context_count(), 0);
        assert_eq!(rig.renderer.ring_count(), 0);
        assert_eq!(rig.renderer.blob_count(), 0);
        assert_eq!(
            rig.window.events().last(),
            Some(&Event::Unmap {
                offset: WINDOW_OFFSET
            }),
            "the mapping came down; ADR-0005 leaves no stale window behind"
        );

        // The window itself survives — the machine layer installs it once,
        // before the device is activated — so a fresh driver can start over.
        rig.renderer
            .ctx_create(CTX, crate::CAPSET_VENUS, "again")
            .expect("a fresh context");
        rig.renderer
            .create_blob(CTX, &blob_args(RESOURCE_ID, RESOURCE), &rig.mem, &[])
            .expect("a fresh blob");
        rig.renderer
            .map_blob(RESOURCE_ID, WINDOW_OFFSET, RESOURCE)
            .expect("a fresh publication");
        rig.create_ring();
        rig.wait_parked();
    }

    #[test]
    fn a_reset_while_the_vm_is_paused_still_joins_every_thread() {
        // ADR-0005: a reset runs on a quiesced VM, and a worker parked at the
        // pause gate must still be able to leave, or the reset deadlocks.
        let gate = Quiesce::new();
        let (mut rig, _tap) = tap_rig();
        rig.renderer.set_quiesce(Arc::clone(&gate));
        rig.create_ring_with(ring_info(), Some(1_000));
        rig.wait_parked();

        gate.pause();
        assert!(gate.wait_until_idle(Duration::from_secs(5)));
        // Give both threads work that would need a pass: a doorbell for the
        // worker, and the monitor's next period.
        rig.guest().submit(b"frozen");
        rig.doorbell();
        std::thread::sleep(Duration::from_millis(20));

        let started = Instant::now();
        rig.renderer.reset();
        assert_eq!(rig.renderer.live_threads(), 0);
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "a paused reset took {:?}",
            started.elapsed()
        );
        assert!(gate.is_paused(), "the reset opened the gate to get out");
    }

    // ----------------------------------------------------------- the pause gate

    #[test]
    fn a_paused_vm_gets_no_ring_pass_until_it_resumes() {
        let gate = Quiesce::new();
        let (mut rig, tap) = tap_rig();
        rig.renderer.set_quiesce(Arc::clone(&gate));
        rig.create_ring();
        rig.wait_parked();

        gate.pause();
        assert!(gate.wait_until_idle(Duration::from_secs(5)));
        assert!(rig.guest().submit(b"while paused"));
        // The doorbell itself touches no ring memory, so it is fine on a
        // paused VM; it only wakes a worker that then waits at the gate.
        rig.doorbell();
        std::thread::sleep(Duration::from_millis(50));
        assert_eq!(rig.head(), 0, "the worker wrote guest memory while paused");
        assert_eq!(tap.len(), 0);
        assert_eq!(
            rig.status(),
            STATUS_IDLE,
            "even waking (IDLE down) is a write, and it waited too"
        );

        gate.resume();
        rig.wait_head(12);
        assert_eq!(tap.bytes(), b"while paused".to_vec());
    }

    // ------------------------------------------------------------- the monitor

    #[test]
    fn a_monitored_ring_gets_alive_back_every_time_the_guest_clears_it() {
        let (mut rig, _tap) = tap_rig();
        // Mesa asks for 3 s; ask for 1 µs to prove the floor is what is kept,
        // and to make the test quick.
        rig.create_ring_with(ring_info(), Some(1));
        assert_eq!(rig.renderer.monitor_period(CTX), Some(MIN_MONITOR_PERIOD));
        assert_eq!(rig.renderer.live_threads(), 2);
        let guest = rig.guest();
        let alive = || guest.load_word(STATUS) & STATUS_ALIVE != 0;

        eventually("the first ALIVE", alive);
        for _ in 0..5 {
            guest.clear_status_bits(STATUS_ALIVE);
            eventually("ALIVE after the watchdog cleared it", alive);
        }

        // Destroying the ring takes it off the monitor first, so the zeroed
        // status stays zero — a new ring built on the same bytes must find
        // them zeroed, and a late ALIVE would get it refused.
        rig.renderer
            .submit(CTX, &destroy_stream(RING))
            .expect("destroyed");
        std::thread::sleep(Duration::from_millis(20));
        assert_eq!(guest.load_word(STATUS), 0);
        rig.create_ring_with(ring_info(), Some(1));
        eventually("ALIVE on the new ring", alive);
    }

    #[test]
    fn an_unmonitored_ring_is_never_given_alive() {
        let (mut rig, _tap) = tap_rig();
        rig.create_ring();
        assert_eq!(rig.renderer.monitor_period(CTX), None);
        rig.wait_parked();
        std::thread::sleep(Duration::from_millis(20));
        assert_eq!(rig.status() & STATUS_ALIVE, 0);
    }

    /// The reason the monitor is its own thread: a ring worker busy inside one
    /// long command cannot report on itself, and that is exactly when the
    /// guest has been waiting longest.
    #[test]
    fn alive_keeps_coming_while_the_ring_worker_is_stuck_in_its_sink() {
        #[derive(Clone, Default)]
        struct Gate(Arc<(Mutex<(bool, bool)>, Condvar)>);
        impl Gate {
            fn entered(&self) -> bool {
                self.0 .0.lock().expect("uncontended").0
            }
            fn release(&self) {
                self.0 .0.lock().expect("uncontended").1 = true;
                self.0 .1.notify_all();
            }
        }
        /// Blocks inside `consume` until released: a command that takes a
        /// very long time to execute.
        struct Slow(Gate);
        impl RingSink for Slow {
            fn consume(&mut self, batch: Batch<'_>) -> Consumed {
                let (lock, cond) = &*self.0 .0;
                let mut state = lock.lock().expect("uncontended");
                state.0 = true;
                while !state.1 {
                    state = cond.wait(state).expect("uncontended");
                }
                batch.all()
            }
        }

        let gate = Gate::default();
        let mut rig = Rig::new({
            let gate = gate.clone();
            move |_: u32, _: u64| Ok(Slow(gate.clone()))
        });
        rig.create_ring_with(ring_info(), Some(1_000));
        let guest = rig.guest();
        let alive = || guest.load_word(STATUS) & STATUS_ALIVE != 0;

        guest.submit(b"a very long command");
        eventually("the worker to be inside the sink", || gate.entered());
        for _ in 0..3 {
            guest.clear_status_bits(STATUS_ALIVE);
            eventually("ALIVE while the worker is busy", alive);
        }
        assert_eq!(rig.head(), 0, "the worker really was still busy");

        gate.release();
        rig.wait_head(19);
    }

    // --------------------------------------------------------------- the sinks

    #[test]
    fn the_capture_records_set_reply_and_the_command_it_cannot_answer_then_fails_the_ring() {
        // What a real Mesa guest's first ring traffic is (spec §0.4): a 36-byte
        // `SetReply` and a 16-byte `vkEnumerateInstanceVersion`.
        let capture = CaptureSink::new();
        let mut rig = Rig::new(capture.factory());
        rig.create_ring();
        rig.wait_parked();
        let guest = rig.guest();

        let mut want = set_reply(10, 0, 20);
        guest.submit(&want);
        guest.submit(&ENUMERATE_INSTANCE_VERSION);
        want.extend_from_slice(&ENUMERATE_INSTANCE_VERSION);
        rig.doorbell();

        // The guest aborts on "ring fatal error" at once, instead of a 3.5 s
        // watchdog.
        rig.wait_fatal();
        // `head` moved past the `SetReply` — it is finished — and not past the
        // command nobody answered.
        assert_eq!(rig.head(), 36);
        // And the capture holds exactly what the guest asked.
        assert_eq!(capture.bytes(), want);
        assert_eq!(capture.len(), 52);
    }

    #[test]
    fn the_capture_consumes_reply_bookkeeping_and_waits_for_a_command_to_be_whole() {
        let capture = CaptureSink::new();
        let mut rig = Rig::new(capture.factory());
        rig.create_ring();
        rig.wait_parked();
        let guest = rig.guest();

        // A `SetReply`, then only half of the next command's header — which a
        // guest that stores `tail` last never produces, but may.
        let reply = set_reply(10, 0, 20);
        guest.submit(&reply);
        guest.submit(&ENUMERATE_INSTANCE_VERSION[..4]);
        rig.doorbell();
        rig.wait_head(36);
        rig.wait_parked();
        assert_eq!(
            rig.status() & STATUS_FATAL,
            0,
            "an incomplete command is not judged"
        );
        assert_eq!(capture.bytes(), reply);

        // The rest arrives: now it is a whole command nobody can answer.
        guest.submit(&ENUMERATE_INSTANCE_VERSION[4..]);
        rig.doorbell();
        rig.wait_fatal();
        assert_eq!(rig.head(), 36);
        let mut want = reply;
        want.extend_from_slice(&ENUMERATE_INSTANCE_VERSION);
        assert_eq!(capture.bytes(), want);
    }

    #[test]
    fn the_capture_refuses_transport_commands_that_are_requests_for_work() {
        // `vkExecuteCommandStreamsMESA` runs commands out of another buffer, and
        // a context-only command has no business in a ring; consuming either
        // would move `head` past work nobody did.
        for opcode in [Opcode::ExecuteCommandStreams, Opcode::DestroyRing] {
            let capture = CaptureSink::new();
            let mut rig = Rig::new(capture.factory());
            rig.create_ring();
            rig.wait_parked();
            let guest = rig.guest();

            let mut enc = Encoder::new();
            enc.command_header(CommandHeader {
                opcode: opcode.as_u32(),
                flags: 0,
            })
            .expect("encode");
            match opcode {
                Opcode::ExecuteCommandStreams => {
                    // One stream, no reply positions, no dependencies, flags 0.
                    enc.u32(1).expect("encode");
                    enc.u64(1).expect("encode");
                    enc.u32(10).expect("encode");
                    enc.size(0).expect("encode");
                    enc.size(16).expect("encode");
                    enc.u64(0).expect("encode");
                    enc.u32(0).expect("encode");
                    enc.u64(0).expect("encode");
                    enc.flags(0).expect("encode");
                }
                _ => enc.handle(RING).expect("encode"),
            }
            let command = enc.finish().expect("encode");
            guest.submit(&command);
            rig.doorbell();
            rig.wait_fatal();
            assert_eq!(rig.head(), 0, "{} was consumed", opcode.name());
            assert_eq!(
                capture.bytes(),
                command,
                "{} was not recorded",
                opcode.name()
            );
        }
    }

    #[test]
    fn the_write_sink_carries_each_ring_into_its_own_writer() {
        let out = SharedVec::default();
        let mut rig = Rig::new({
            let out = out.clone();
            move |_: u32, _: u64| Ok(WriteSink::new(out.clone()))
        });
        rig.create_ring();
        rig.wait_parked();
        let guest = rig.guest();

        let mut want = set_reply(10, 0, 20);
        guest.submit(&want);
        guest.submit(&ENUMERATE_INSTANCE_VERSION);
        want.extend_from_slice(&ENUMERATE_INSTANCE_VERSION);
        rig.doorbell();
        rig.wait_fatal();
        assert_eq!(rig.head(), 36);
        assert_eq!(*out.0.lock().expect("uncontended"), want);
    }

    #[test]
    fn a_failing_writer_does_not_freeze_the_guests_ring() {
        /// A writer that fails every call, like a full disk.
        struct Broken;
        impl io::Write for Broken {
            fn write(&mut self, _buf: &[u8]) -> io::Result<usize> {
                Err(io::Error::other("no room"))
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }

        let mut rig = Rig::new(|_: u32, _: u64| Ok(WriteSink::new(Broken)));
        rig.create_ring();
        rig.wait_parked();
        rig.guest().submit(&set_reply(10, 0, 20));
        rig.doorbell();

        // The bytes are gone, but the ring moved on: a guest's Vulkan driver
        // cannot see a host file error and must not be frozen by one.
        rig.wait_head(36);
        rig.wait_parked();
        assert_eq!(rig.status() & STATUS_FATAL, 0);
    }

    #[test]
    fn the_capture_logic_answers_a_batch_without_any_threads() {
        // `capture_batch` directly: the bookkeeping is consumed, the first real
        // command and everything after it recorded, and the ring declared dead
        // after the bookkeeping.
        let mut batch = set_reply(1, 0, 20);
        batch.extend_from_slice(&set_reply(1, 20, 28));
        batch.extend_from_slice(&ENUMERATE_INSTANCE_VERSION);
        batch.extend_from_slice(b"trailing");
        let mut recorded = Vec::new();
        let consumed = capture_batch(Batch::for_test(&batch), |b| recorded.extend_from_slice(b));
        assert!(consumed.is_fatal());
        assert_eq!(consumed.bytes(), 72);
        assert_eq!(recorded, batch);

        // Bookkeeping alone is consumed whole and is not fatal.
        let only = set_reply(1, 0, 20);
        let mut recorded = Vec::new();
        let consumed = capture_batch(Batch::for_test(&only), |b| recorded.extend_from_slice(b));
        assert!(!consumed.is_fatal());
        assert_eq!(consumed.bytes(), 36);
        assert_eq!(recorded, only);

        // A reply-bearing `SetReply` is not bookkeeping.
        let mut flagged = set_reply(1, 0, 20);
        flagged[4] = 1;
        let consumed = capture_batch(Batch::for_test(&flagged), |_| {});
        assert!(consumed.is_fatal());
        assert_eq!(consumed.bytes(), 0);
    }
}
