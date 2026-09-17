//! The [`Renderer3d`] that ties the Venus layers together: it advertises the
//! capset, accepts a venus-typed context, puts host pages behind the blob the
//! guest wants its ring in, decodes the context command stream and pumps the
//! ring (EPIC 20, ADR-0004).
//!
//! **It executes no Vulkan.** This is the transport half and nothing else: a
//! real Mesa `venus` guest can get all the way to handing us a command ring and
//! writing commands into it, and what happens to those bytes is a
//! [`RingSink`] the caller supplies — [`CaptureSink`] for a test, [`WriteSink`]
//! for `entangled run` writing a capture file. Nothing here decodes a Vulkan
//! command, and deliberately so: everything up to the ring is pure logic over
//! bytes and is provable on a host with no GPU at all, which is the whole
//! reason the seam is drawn where [`super`]'s docs draw it.
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
//!    [`super::shmem`]).
//! 4. `RESOURCE_MAP_BLOB` — the pages go in front of the guest at the window
//!    offset it named, and the [`Publication`] that keeps them alive is held
//!    beside them.
//! 5. `SUBMIT_3D` carrying `vkCreateRingMESA` — the guest's proposed layout is
//!    judged by [`RingLayout::new`] against the size of *these* pages and
//!    adopted with [`RingPages::adopt`], which re-checks it against the
//!    allocation before a [`RingPump`] ever indexes anything.
//! 6. `SUBMIT_3D` carrying `vkNotifyRingMESA` — the doorbell. We consume
//!    everything the guest has produced, then publish
//!    [`STATUS_IDLE`](super::pump::STATUS_IDLE) again.
//!
//! # The doorbell is pumped synchronously, and there is no thread
//!
//! The ring protocol says the host publishes `IDLE` and then blocks until it is
//! notified; a guest that sees `IDLE` rings the doorbell for every batch it
//! writes. **A host that is always idle is therefore one the guest always
//! notifies**, and that is the design here:
//! [`STATUS_IDLE`](super::pump::STATUS_IDLE) is published as
//! soon as a ring is created and republished at the end of every doorbell, so
//! `vkNotifyRingMESA` arriving on the context stream is our only cue, and we
//! consume everything available before returning from
//! [`submit`](Renderer3d::submit).
//!
//! Publishing idle is not an optimisation to skip — it is what makes the design
//! work at all. A host that consumed on the doorbell but never advertised
//! `IDLE` would be a host whose guest never rings it, and the ring would sit
//! full while both sides waited for the other.
//!
//! What this costs: **a vCPU exit per submission**. The guest's producer thread
//! rings the doorbell through `SUBMIT_3D`, which is a virtqueue round trip, for
//! work a polling host thread would have picked up with no exit at all. A later
//! design will want that back, and when it takes it, it inherits two
//! obligations this file does not have:
//!
//! * **ADR-0005's [`Quiesce`](virtio_core::Quiesce) gate.** No host thread of
//!   ours touches guest memory here: every access to the ring happens inside a
//!   device call, on the device's own queue worker, which a pause already
//!   stops between commands. A pump thread would be a second toucher of guest
//!   memory and would have to take the gate — outside the device lock — before
//!   every pass.
//! * **ADR-0006's `save`/`load` pair.** There is host state here a resumed
//!   guest would notice missing: per ring, [`RingPump::cursor`] and
//!   [`RingPump::status`] (the pump's own docs say so), plus the layout and the
//!   window offset each set of pages was published at. It is not implemented
//!   because nothing in this file is reachable from a shipping VM yet — the
//!   renderer is opt-in and executes nothing — but a snapshot taken over a live
//!   ring without it would resume a guest whose `head` says one thing and whose
//!   host cursor says another, which is the quietest possible corruption.
//!
//! The `reset()` half of ADR-0005 *is* implemented: it drops every context,
//! ring, publication and page, so a rebooted guest finds no stale `head` in
//! shared memory because it finds no shared memory at all.
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
//! * A **ring** whose guest published an impossible `tail` is marked fatal by
//!   [`RingPump`] itself, which tells the guest's driver to give up instead of
//!   waiting on a `head` that will never move. We keep the dead pump exactly
//!   where it is: a later doorbell answers [`PumpError::Fatal`], and no code
//!   path here rebuilds a pump over a ring that has already failed.
//!
//! A *content* refusal — a ring layout that does not fit, a duplicate ring id —
//! is different, and is not sticky: the bytes were well formed, we simply will
//! not do what they asked. It fails that one command, the rest of the stream
//! still runs, and the first refusal is what the `SUBMIT_3D` is answered with.

use std::collections::HashMap;
use std::fmt;
use std::io;
use std::sync::{Arc, Mutex};

use thiserror::Error;
use virtio_core::{GuestMem, ShmBacking};

use crate::blob::{BlobMapping, BlobSupport};
use crate::error::CommandError;
use crate::protocol::{
    MemEntry, Rect, ResourceCreate3d, ResourceCreateBlob, Transfer3d, BLOB_MEM_HOST3D,
};
use crate::renderer::{CapsetInfo, Renderer3d};

use super::capset::{VenusCapset, VENUS_CAPSET_LEN, VENUS_CAPSET_MAX_VERSION};
use super::pump::{Batch, Consumed, Idle, Pass, PumpError, RingPump, RingSink};
use super::ring::{RingCreateInfo, RingLayout, RingLayoutError};
use super::shmem::{Publication, RingPages, ShmemError};
use super::transport::{Opcode, TransportCommand, TransportError, TransportStream};

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
pub const MAX_RINGS: usize = 32;

/// Most host blobs this renderer backs with pages at once.
pub const MAX_RING_BLOBS: usize = 64;

/// Most host bytes this renderer will allocate across all live blobs.
///
/// A ring's shared-memory resource is a guest-chosen size, so it is a guest
/// value naming a host allocation twice over: per blob,
/// [`super::shmem::MAX_RESOURCE_BYTES`] caps one; this caps their sum. A real
/// venus ring is ~1 MiB, so 64 MiB is two orders of magnitude of headroom and
/// still a number a host can afford to lose to a hostile guest.
pub const MAX_RING_BLOB_BYTES: u64 = 64 << 20;

/// Most passes one doorbell makes over one ring before giving up on it.
///
/// Every pass that continues the loop consumed at least one byte, so a guest
/// that stops producing ends the loop immediately and a well-behaved one needs
/// one or two passes. The cap exists for the guest that keeps producing from
/// another vCPU while we drain: without it, one `vkNotifyRingMESA` could hold
/// the device's queue worker for as long as the guest cared to feed it.
/// Exhausting it marks the ring fatal — telling the driver to give up — rather
/// than returning quietly and leaving work nobody will ever be asked for again.
pub const MAX_DOORBELL_PASSES: usize = 1024;

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

/// A [`RingSink`] that appends every byte it is offered to a shared buffer.
///
/// Cloning one clones the *handle*: the renderer takes a clone and the test
/// keeps another, which is what makes the captured bytes readable after the
/// sink has been moved into a `Box<dyn Renderer3d>`.
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

    /// A copy of everything consumed so far, in ring order.
    #[must_use]
    pub fn bytes(&self) -> Vec<u8> {
        self.with(|buf| buf.clone())
    }

    /// How many bytes have been consumed.
    #[must_use]
    pub fn len(&self) -> usize {
        self.with(|buf| buf.len())
    }

    /// Whether nothing has been consumed yet.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Drop everything captured so far.
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
        self.with(|buf| buf.extend_from_slice(batch.bytes()));
        batch.all()
    }
}

/// A [`RingSink`] that writes the ring's bytes to anything `std::io::Write` —
/// a file, for `entangled run` capturing a guest's Venus stream.
///
/// A write that fails does **not** stall the ring. The bytes are still reported
/// consumed, because refusing them would freeze the guest's Vulkan driver over
/// a host-side file error it can neither see nor fix; the failure is logged
/// once and latched in [`failed`](Self::failed) instead.
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

    /// Whether any write has failed. The ring kept running regardless.
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
}

impl<W: io::Write> RingSink for WriteSink<W> {
    fn consume(&mut self, batch: Batch<'_>) -> Consumed {
        self.written = self.written.saturating_add(batch.len() as u64);
        if let Err(error) = self.out.write_all(batch.bytes()) {
            if !self.failed {
                tracing::error!(%error, "the Venus ring capture could not be written");
            }
            self.failed = true;
        }
        batch.all()
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

    /// The ring protocol refused a pass — an impossible `tail`, or a ring
    /// already written off.
    #[error(transparent)]
    Pump(#[from] PumpError),

    /// The sink took nothing from a full ring, so nothing can ever arrive to
    /// unstick it ([`Pass::Deadlocked`]). The ring is marked fatal.
    #[error(
        "the ring {ring:#x} is full and its sink consumed none of the {offered:#x} bytes in it"
    )]
    RingDeadlocked {
        /// The ring that stalled.
        ring: u64,
        /// How many bytes were on offer — always the buffer's full length.
        offered: u32,
    },

    /// One doorbell made [`MAX_DOORBELL_PASSES`] passes and the guest was still
    /// producing. The ring is marked fatal.
    #[error(
        "one doorbell on ring {ring:#x} made {MAX_DOORBELL_PASSES} passes and the guest was \
         still producing; the ring is written off rather than held open"
    )]
    DoorbellExhausted {
        /// The ring that would not drain.
        ring: u64,
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
            | VenusError::BlobCarriesPages(id) => Self::UnknownResource(id),
            VenusError::ForeignRingResource { resource_id, .. } => {
                Self::UnknownResource(resource_id)
            }
            VenusError::Layout(_) | VenusError::Shmem(_) => {
                Self::InvalidStream("the proposed venus ring layout is not one this host serves")
            }
            VenusError::Pump(_)
            | VenusError::RingDeadlocked { .. }
            | VenusError::DoorbellExhausted { .. } => {
                Self::InvalidStream("the venus ring protocol was violated and the ring is dead")
            }
            VenusError::UnsupportedBlobMem(blob_mem) => Self::UnsupportedBlobMem(blob_mem),
            VenusError::DuplicateBlob(id) => Self::DuplicateResource(id),
            VenusError::TooManyBlobs | VenusError::BlobBudget { .. } => Self::OutOfMemory,
            VenusError::BlobAlreadyMapped(id) => Self::BlobAlreadyMapped(id),
            VenusError::BlobSpanMismatch { .. } | VenusError::WindowRefused { .. } => {
                Self::Renderer(err.to_string())
            }
            VenusError::NoWindow => Self::NoHostVisibleWindow,
            VenusError::NoClassic3d(_) => Self::Renderer(err.to_string()),
        }
    }
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
    /// The allocation. Rings built on it hold their own `Arc`.
    pages: Arc<RingPages>,
    /// The context the blob was created on; `0` is the kernel's own.
    ctx_id: u32,
    /// The renderer-side name the guest minted it under.
    blob_id: u64,
    /// The size the guest asked for, which is [`RingPages::resource_len`].
    size: u64,
}

/// One live command ring.
struct Ring {
    /// The pages it lives in. An `Arc`, so a ring keeps its own memory alive
    /// even if the blob is destroyed out from under it before we tear the ring
    /// down.
    pages: Arc<RingPages>,
    /// The resource the guest named, so destroying that blob can take its rings
    /// with it.
    resource_id: u32,
    /// The head/tail protocol. Never rebuilt: once this is fatal it stays
    /// fatal, which is what the module docs promise.
    pump: RingPump,
}

/// One venus context.
struct Context {
    /// Always [`crate::CAPSET_VENUS`]; kept so a diagnostic can say what a
    /// context was created as rather than what we assume.
    capset_id: u32,
    /// Rings by the handle the guest minted, *within this context*.
    rings: HashMap<u64, Ring>,
    /// Set once a transport stream on this context refused something.
    poisoned: bool,
}

/// The transport-half Venus renderer. See the module docs.
pub struct VenusRenderer<S> {
    sink: S,
    capset: VenusCapset,
    /// The host-visible window, once the machine layer has supplied one.
    window: Option<Arc<dyn ShmBacking>>,
    blobs: HashMap<u32, RingBlob>,
    /// Sum of [`RingBlob::size`], against [`MAX_RING_BLOB_BYTES`].
    blob_bytes: u64,
    contexts: HashMap<u32, Context>,
    /// Transport commands accepted but not executed (reply streams, seqnos):
    /// a diagnostic, and what a test asserts to show they were not refused.
    observed: u64,
}

impl<S> fmt::Debug for VenusRenderer<S> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("VenusRenderer")
            .field("contexts", &self.contexts.len())
            .field("rings", &self.ring_count())
            .field("blobs", &self.blobs.len())
            .field("blob_bytes", &self.blob_bytes)
            .field("window", &self.window.as_ref().map(|w| w.len()))
            .field("observed", &self.observed)
            .finish()
    }
}

impl<S> VenusRenderer<S> {
    /// A renderer that feeds every ring's bytes to `sink`.
    ///
    /// One sink serves every ring of every context, in the order the bytes were
    /// consumed. Nothing in the protocol labels a batch with its ring, so a
    /// caller that needs them told apart wants one renderer per capture, or a
    /// sink that is handed that structure some other way — which is a real
    /// limitation of this seam and is written down rather than papered over.
    pub fn new(sink: S) -> Self {
        Self {
            sink,
            capset: VenusCapset::new(),
            window: None,
            blobs: HashMap::new(),
            blob_bytes: 0,
            contexts: HashMap::new(),
            observed: 0,
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

    /// The sink, for a caller that kept no handle of its own.
    pub fn sink(&self) -> &S {
        &self.sink
    }

    /// The sink, mutably.
    pub fn sink_mut(&mut self) -> &mut S {
        &mut self.sink
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

    /// Transport commands that were decoded and accepted without being
    /// executed — the reply-stream and seqno commands this renderer carries but
    /// has no Vulkan to perform.
    #[must_use]
    pub fn observed_commands(&self) -> u64 {
        self.observed
    }

    /// The pump cursor and status word of one ring, for diagnostics and for the
    /// two values ADR-0006 would have to persist.
    #[must_use]
    pub fn ring_state(&self, ctx_id: u32, ring: u64) -> Option<(u32, u32)> {
        let ring = self.contexts.get(&ctx_id)?.rings.get(&ring)?;
        Some((ring.pump.cursor(), ring.pump.status()))
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

    /// `RESOURCE_CREATE_BLOB` for a host blob: allocate the pages a ring will
    /// live in.
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
        if self.blobs.len() >= MAX_RING_BLOBS {
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
        self.blobs.insert(
            args.resource_id,
            RingBlob {
                publication: None,
                pages,
                ctx_id,
                blob_id: args.blob_id,
                size: args.size,
            },
        );
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
        let publication =
            blob.pages
                .publish(window, offset)
                .map_err(|err| VenusError::WindowRefused {
                    resource_id,
                    reason: err.to_string(),
                })?;
        blob.publication = Some(publication);
        tracing::debug!(
            resource = resource_id,
            blob_id = blob.blob_id,
            offset = format_args!("{offset:#x}"),
            len = blob.pages.mapped_len(),
            "venus ring pages published into the shared-memory window"
        );
        // The bytes behind the window are plain host RAM, so cached is the
        // truthful answer; a real Venus renderer's host-visible heap would be
        // write-combining and would say so.
        Ok(BlobMapping::CACHED)
    }

    // --------------------------------------------------------------- rings
}

impl<S: RingSink> VenusRenderer<S> {
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
            TransportCommand::CreateRing { ring, info, .. } => self.create_ring(ctx_id, ring, info),
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
            // Carried, counted and not executed: these are the reply-stream and
            // seqno commands, and every one of them is about Vulkan work this
            // renderer does not do. Refusing them would stop a real guest before
            // it ever built its ring, which is the one thing this renderer
            // exists to let it do.
            TransportCommand::SetReplyCommandStream { .. }
            | TransportCommand::SeekReplyCommandStream { .. }
            | TransportCommand::ExecuteCommandStreams { .. }
            | TransportCommand::SubmitVirtqueueSeqno { .. }
            | TransportCommand::WaitRingSeqno { .. } => {
                self.observed = self.observed.saturating_add(1);
                Ok(())
            }
        }
    }

    /// `vkCreateRingMESA`: judge the proposed layout against the pages it
    /// claims to live in, adopt it, and publish `IDLE` so the guest knows to
    /// ring the doorbell.
    fn create_ring(
        &mut self,
        ctx_id: u32,
        ring: u64,
        info: RingCreateInfo,
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
        let pages = Arc::clone(&blob.pages);

        // Two judgements, and they are not the same one twice: `RingLayout`
        // proves the five regions fit the `resource_size` it is *told*, and
        // `adopt` re-proves them against the size actually allocated before a
        // pump can index anything. Handing the first the pages' own length is
        // what makes the second a formality rather than the only real check.
        let layout = RingLayout::new(info, pages.resource_len())?;
        let mut pump = pages.adopt(layout)?;

        // Publish IDLE now, before the guest has written a byte. A guest that
        // finds the host not-idle assumes it is being polled and rings no
        // doorbell — and nothing here polls.
        //
        // A guest that had already published an impossible `tail` fails here,
        // and the ring is never recorded. That is not a way to retry into a
        // fresh pump either: the pass marked the ring fatal in the bytes
        // themselves, so a second `vkCreateRingMESA` over the same pages is
        // refused for control words that are not zeroed.
        Self::drain(ring, &mut pump, &pages, &mut self.sink)?;

        tracing::debug!(
            ctx_id,
            ring = format_args!("{ring:#x}"),
            resource = resource_id,
            buffer = pump.buffer_len(),
            idle_timeout_ns = info.idle_timeout_ns,
            "venus command ring adopted"
        );
        // The context was live at the top of this function and nothing between
        // here and there can have removed it; `ok_or` rather than an `if let`
        // so that a future rearrangement is a refusal instead of a ring the
        // guest believes in and we do not hold.
        self.contexts
            .get_mut(&ctx_id)
            .ok_or(VenusError::UnknownContext(ctx_id))?
            .rings
            .insert(
                ring,
                Ring {
                    pages,
                    resource_id,
                    pump,
                },
            );
        Ok(())
    }

    /// `vkDestroyRingMESA`.
    ///
    /// A ring that is still healthy has its host words zeroed on the way out,
    /// so a guest that builds a new ring over the same bytes finds the
    /// power-on state [`RingPump::new`] insists on (ADR-0005's "no stale word
    /// in shared memory"). A ring that was marked **fatal** keeps its
    /// `STATUS_FATAL` bit: the guest is entitled to read why its ring died, and
    /// a resource that produced an impossible `tail` is not one to hand back
    /// looking fresh.
    fn destroy_ring(&mut self, ctx_id: u32, ring: u64) -> Result<(), VenusError> {
        let context = self
            .contexts
            .get_mut(&ctx_id)
            .ok_or(VenusError::UnknownContext(ctx_id))?;
        let mut dead = context
            .rings
            .remove(&ring)
            .ok_or(VenusError::UnknownRing { ctx_id, ring })?;
        if !dead.pump.is_fatal() {
            dead.pump.reset(&*dead.pages);
        }
        Ok(())
    }

    /// `vkNotifyRingMESA`: consume everything the guest has produced.
    fn doorbell(&mut self, ctx_id: u32, ring: u64) -> Result<(), VenusError> {
        // Field-by-field so the sink and the ring table are two disjoint
        // borrows rather than one of `self`.
        let Self { contexts, sink, .. } = self;
        let context = contexts
            .get_mut(&ctx_id)
            .ok_or(VenusError::UnknownContext(ctx_id))?;
        let live = context
            .rings
            .get_mut(&ring)
            .ok_or(VenusError::UnknownRing { ctx_id, ring })?;
        Self::drain(ring, &mut live.pump, &live.pages, sink)
    }

    /// Pump one ring until it has nothing more to offer, and leave
    /// [`STATUS_IDLE`](super::pump::STATUS_IDLE) published.
    ///
    /// The loop ends only on [`Idle::Park`], which is the pump's own statement
    /// that `IDLE` is up *and* a re-read of `tail` confirmed there is nothing to
    /// do. [`Idle::WorkArrived`] means the guest produced between the last pass
    /// and the publication — the lost-wakeup case — and `IDLE` has already been
    /// taken back down, so the only correct answer is to go round again rather
    /// than return with work waiting and no doorbell coming.
    fn drain(
        ring: u64,
        pump: &mut RingPump,
        pages: &RingPages,
        sink: &mut S,
    ) -> Result<(), VenusError> {
        for _ in 0..MAX_DOORBELL_PASSES {
            match pump.pump(pages, sink)? {
                Pass::Progress { .. } => continue,
                Pass::Idle | Pass::Stalled { .. } => match pump.enter_idle(pages) {
                    Idle::Park => return Ok(()),
                    Idle::WorkArrived => continue,
                },
                Pass::Deadlocked { offered } => {
                    // The sink took nothing from a full ring, so nothing can
                    // ever arrive to unstick it. The pump reports it rather
                    // than deciding; the decision is that a guest waiting on a
                    // `head` that cannot move should be told, not hung.
                    pump.mark_fatal(pages);
                    return Err(VenusError::RingDeadlocked { ring, offered });
                }
            }
        }
        pump.mark_fatal(pages);
        Err(VenusError::DoorbellExhausted { ring })
    }

    /// Drop every ring built on `resource_id`, wherever it lives.
    fn drop_rings_on(&mut self, resource_id: u32) {
        for context in self.contexts.values_mut() {
            context
                .rings
                .retain(|_, ring| ring.resource_id != resource_id);
        }
    }

    /// Drop a blob: its publication (which unmaps first and frees after), its
    /// pages, and every ring that was built on them.
    fn drop_blob(&mut self, resource_id: u32) {
        if let Some(blob) = self.blobs.remove(&resource_id) {
            self.blob_bytes = self.blob_bytes.saturating_sub(blob.size);
            self.drop_rings_on(resource_id);
            drop(blob);
        }
    }
}

impl<S: RingSink + Send> Renderer3d for VenusRenderer<S> {
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
        tracing::debug!(ctx_id, capset_id, name, "venus context created");
        Ok(())
    }

    fn ctx_destroy(&mut self, ctx_id: u32) {
        // Dropping the context drops its rings; the pages behind them belong to
        // the blobs and stay until those are destroyed or the device resets.
        self.contexts.remove(&ctx_id);
    }

    fn resource_create_3d(&mut self, _args: &ResourceCreate3d) -> Result<(), CommandError> {
        Err(VenusError::NoClassic3d("3D resources").into())
    }

    fn resource_unref(&mut self, _resource_id: u32) {}

    fn ctx_attach_resource(&mut self, _ctx_id: u32, _resource_id: u32) {}

    fn ctx_detach_resource(&mut self, _ctx_id: u32, _resource_id: u32) {}

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

    fn read_rect_bgra(
        &mut self,
        resource_id: u32,
        _rect: Rect,
        _out: &mut Vec<u8>,
    ) -> Result<(), CommandError> {
        Err(CommandError::UnknownResource(resource_id))
    }

    fn reset(&mut self) {
        // Order is not load-bearing for safety — every `Publication` unmaps
        // before its pages are freed, whatever drops it — but it is for
        // clarity: rings first, then the pages they pointed at.
        self.contexts.clear();
        self.blobs.clear();
        self.blob_bytes = 0;
        self.observed = 0;
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
        self.create_host_blob(ctx_id, args, entries)?;
        tracing::debug!(
            ctx_id,
            resource = args.resource_id,
            blob_id = args.blob_id,
            size = args.size,
            "venus host blob allocated"
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

    use virtio_core::{ShmAccessError, ShmMapError};

    use crate::protocol::{BLOB_FLAG_USE_MAPPABLE, BLOB_MEM_HOST3D_GUEST};
    use crate::renderer::Gpu3d;
    use crate::venus::pump::{STATUS_FATAL, STATUS_IDLE};
    use crate::venus::shmem::MAX_RESOURCE_BYTES;
    use crate::venus::transport::STYPE_RING_CREATE_INFO_MESA;
    use crate::venus::wire::{CommandHeader, Encoder, WireError};

    /// The blob every fixture puts its ring in: one 4 KiB page.
    const RESOURCE: u64 = 0x1000;
    /// A deliberately small command buffer, so a wrap is a few bytes away.
    const BUFFER: u64 = 64;
    /// Where inside the window the guest asks for its blob.
    const WINDOW_OFFSET: u64 = 0x2_0000;
    /// The context id the fixtures use.
    const CTX: u32 = 1;
    /// The resource id of the fixture's ring blob.
    const RESOURCE_ID: u32 = 9;
    /// The ring handle the fixtures mint.
    const RING: u64 = 0xdead_beef_0000_0001;

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
    /// `head` and `status` — performed from outside the renderer, through the
    /// published mapping, exactly as a guest reaches them.
    #[derive(Debug, Clone, Copy)]
    struct GuestView(u64);

    impl GuestView {
        fn store_word(self, offset: u64, value: u32) {
            let at = usize::try_from(offset).expect("a fixture offset fits a usize");
            assert!(offset % 4 == 0 && offset + 4 <= RESOURCE);
            // SAFETY: `self.0` is the base of a live `RingPages` allocation of
            // at least `RESOURCE` bytes — the renderer published it and the test
            // holds the blob that owns it for as long as this value is used —
            // and the assertion above keeps the four bytes inside that
            // allocation and on a 4-byte boundary, which is `AtomicU32`'s
            // alignment requirement.
            unsafe {
                AtomicU32::from_ptr((self.0 as *mut u8).add(at).cast::<u32>())
                    .store(value, Ordering::SeqCst);
            }
        }

        fn load_word(self, offset: u64) -> u32 {
            let at = usize::try_from(offset).expect("a fixture offset fits a usize");
            assert!(offset % 4 == 0 && offset + 4 <= RESOURCE);
            // SAFETY: as `store_word` directly above.
            unsafe {
                AtomicU32::from_ptr((self.0 as *mut u8).add(at).cast::<u32>())
                    .load(Ordering::SeqCst)
            }
        }

        /// Write command bytes at a free-running ring offset, masked and
        /// wrapping the end of the buffer the way the guest's producer does.
        fn produce(self, buffer_offset: u64, buffer_size: u64, at: u64, bytes: &[u8]) {
            for (i, byte) in bytes.iter().enumerate() {
                let position = (at + i as u64) % buffer_size;
                let index = buffer_offset + position;
                assert!(index < RESOURCE);
                let index = usize::try_from(index).expect("a fixture offset fits a usize");
                // SAFETY: as `store_word`; the assertion keeps the byte inside
                // the live allocation, and `AtomicU8` needs no alignment beyond
                // a byte.
                unsafe {
                    AtomicU8::from_ptr((self.0 as *mut u8).add(index))
                        .store(*byte, Ordering::Relaxed);
                }
            }
        }
    }

    // ------------------------------------------------------------- the streams

    fn ring_info() -> RingCreateInfo {
        RingCreateInfo {
            flags: 0,
            resource_id: RESOURCE_ID,
            offset: 0,
            size: RESOURCE,
            idle_timeout_ns: 1_000,
            head_offset: 0,
            tail_offset: 4,
            status_offset: 8,
            buffer_offset: 16,
            buffer_size: BUFFER,
            extra_offset: 16 + BUFFER,
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
    /// `sType`, an empty pNext chain, then the body in wire order.
    fn create_ring_stream(ring: u64, info: RingCreateInfo) -> Vec<u8> {
        let mut enc = Encoder::new();
        let put = |result: Result<(), WireError>| result.expect("the fixture encodes");
        put(enc.command_header(CommandHeader {
            opcode: Opcode::CreateRing.as_u32(),
            flags: 0,
        }));
        put(enc.handle(ring));
        put(enc.simple_pointer(true));
        put(enc.i32(STYPE_RING_CREATE_INFO_MESA));
        put(enc.simple_pointer(false));
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

    // ------------------------------------------------------------- the fixture

    /// A renderer with a window, a venus context and a mapped ring blob —
    /// everything a guest does before its first `vkCreateRingMESA`.
    struct Rig<S> {
        renderer: VenusRenderer<S>,
        window: Arc<Window>,
        mem: Arc<GuestMem>,
    }

    impl<S: RingSink + Send> Rig<S> {
        fn new(sink: S) -> Self {
            let mut renderer = VenusRenderer::new(sink);
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
            self.guest().produce(16, BUFFER, at, bytes);
        }

        fn create_ring(&mut self) {
            self.renderer
                .submit(CTX, &create_ring_stream(RING, ring_info()))
                .expect("the fixture layout fits these pages");
        }

        fn doorbell(&mut self) {
            self.renderer
                .submit(CTX, &notify_stream(RING))
                .expect("the doorbell");
        }
    }

    /// The common case: a rig whose sink is a [`CaptureSink`] the test keeps a
    /// handle to.
    fn capture_rig() -> (Rig<CaptureSink>, CaptureSink) {
        let capture = CaptureSink::new();
        (Rig::new(capture.clone()), capture)
    }

    // -------------------------------------------------------- what we advertise

    #[test]
    fn the_only_capset_is_venus_and_it_is_the_one_mesa_reads() {
        let mut renderer = VenusRenderer::new(CaptureSink::new());
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
        let mut gpu = Gpu3d::new(Box::new(VenusRenderer::new(CaptureSink::new())));
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
        let renderer = VenusRenderer::new(CaptureSink::new());
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
        let (mut rig, capture) = capture_rig();

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

        // IDLE is published before the guest has written a byte. Without it the
        // guest would never ring the doorbell, and nothing here polls.
        assert_eq!(guest.load_word(8), STATUS_IDLE);
        assert_eq!(guest.load_word(0), 0, "head starts at zero");

        // Eleven bytes, then the doorbell.
        rig.produce(0, b"hello venus");
        guest.store_word(4, 11);
        rig.doorbell();

        assert_eq!(capture.bytes(), b"hello venus".to_vec());
        assert_eq!(guest.load_word(0), 11, "head follows the sink");
        assert_eq!(guest.load_word(8), STATUS_IDLE, "still idle afterwards");
        assert_eq!(rig.renderer.ring_state(CTX, RING), Some((11, STATUS_IDLE)));

        // A second batch, continuing from the cursor and wrapping the end of the
        // 64-byte buffer — spliced back into one run by the pump.
        let more: Vec<u8> = (0u8..60).collect();
        rig.produce(11, &more);
        guest.store_word(4, 71);
        rig.doorbell();

        let mut want = b"hello venus".to_vec();
        want.extend_from_slice(&more);
        assert_eq!(capture.bytes(), want);
        assert_eq!(guest.load_word(0), 71);

        // Tearing a healthy ring down zeroes the words the guest polls: a reboot
        // that leaves a stale `head` in shared memory is ADR-0005's haunting.
        rig.renderer
            .submit(CTX, &destroy_stream(RING))
            .expect("the ring is torn down");
        assert_eq!(rig.renderer.ring_count(), 0);
        assert_eq!(guest.load_word(0), 0);
        assert_eq!(guest.load_word(8), 0);
    }

    #[test]
    fn one_doorbell_consumes_everything_that_arrived_since_the_last_one() {
        let (mut rig, capture) = capture_rig();
        rig.create_ring();
        let guest = rig.guest();

        // Three separate productions, one doorbell.
        rig.produce(0, b"aaa");
        rig.produce(3, b"bbbb");
        rig.produce(7, b"cc");
        guest.store_word(4, 9);
        rig.doorbell();
        assert_eq!(capture.bytes(), b"aaabbbbcc".to_vec());

        // A doorbell with nothing waiting is legal and consumes nothing.
        rig.doorbell();
        assert_eq!(capture.len(), 9);
        assert_eq!(guest.load_word(8), STATUS_IDLE);

        // The capture handle is shared, so clearing it is visible at both ends.
        capture.clear();
        rig.produce(9, b"dd");
        guest.store_word(4, 11);
        rig.doorbell();
        assert_eq!(capture.bytes(), b"dd".to_vec());
    }

    #[test]
    fn several_transport_commands_in_one_stream_are_all_executed() {
        let (mut rig, capture) = capture_rig();

        // The guest queued work before the host ever looked at the ring, which
        // the protocol allows: `tail` is the guest's word.
        rig.produce(0, b"early");
        rig.guest().store_word(4, 5);

        let mut stream = create_ring_stream(RING, ring_info());
        stream.extend_from_slice(&notify_stream(RING));
        stream.extend_from_slice(&notify_stream(RING));
        rig.renderer.submit(CTX, &stream).expect("all three");

        assert_eq!(capture.bytes(), b"early".to_vec());
        assert_eq!(rig.renderer.ring_count(), 1);
    }

    #[test]
    fn a_refused_command_does_not_swallow_the_doorbell_behind_it() {
        // A guest batches its ring commands, so a content refusal must not turn
        // into a silently skipped `vkNotifyRingMESA` — which the guest would
        // experience as a hang rather than as an error.
        let (mut rig, capture) = capture_rig();
        rig.create_ring();
        rig.produce(0, b"behind it");
        rig.guest().store_word(4, 9);

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
        assert_eq!(capture.bytes(), b"behind it".to_vec());
    }

    #[test]
    fn the_commands_this_renderer_carries_but_cannot_perform_are_answered_honestly() {
        let (mut rig, _capture) = capture_rig();

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
        let (mut rig, _capture) = capture_rig();

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
        rig.guest().store_word(0, 1);
        assert!(matches!(
            rig.renderer
                .dispatch(CTX, &create_ring_stream(RING, ring_info())),
            Err(VenusError::Shmem(ShmemError::Pump(
                PumpError::ControlWordsNotZeroed { .. }
            )))
        ));
        rig.guest().store_word(0, 0);

        // None of that left a ring behind, and the context is still usable — a
        // refused layout is a content refusal, not a framing one.
        assert_eq!(rig.renderer.ring_count(), 0);
        rig.create_ring();
        assert_eq!(rig.renderer.ring_count(), 1);
    }

    #[test]
    fn a_ring_can_only_be_built_on_a_blob_its_own_context_owns() {
        let (mut rig, _capture) = capture_rig();

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
        let (mut rig, _capture) = capture_rig();
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
        let (mut rig, _capture) = capture_rig();
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

        // A destroyed context takes its rings with it and stops answering.
        rig.create_ring();
        rig.renderer.ctx_destroy(CTX);
        assert_eq!(rig.renderer.ring_count(), 0);
        assert!(matches!(
            rig.renderer.submit(CTX, &notify_stream(RING)),
            Err(CommandError::UnknownContext(CTX))
        ));
    }

    // ------------------------------------------------------------ hostile blobs

    #[test]
    fn a_blob_cannot_be_mapped_twice_and_only_its_own_offset_unmaps_it() {
        let (mut rig, _capture) = capture_rig();
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
        let (mut rig, _capture) = capture_rig();

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
    fn destroying_a_blob_takes_its_rings_and_its_mapping_with_it() {
        let (mut rig, _capture) = capture_rig();
        rig.create_ring();

        rig.renderer.destroy_blob(RESOURCE_ID);
        assert_eq!(rig.renderer.blob_count(), 0);
        assert_eq!(rig.renderer.ring_count(), 0, "the ring went with its pages");
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
        let mut renderer = VenusRenderer::new(CaptureSink::new());
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
        let (mut rig, _capture) = capture_rig();

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
        let (mut rig, _capture) = capture_rig();
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
        let (mut rig, capture) = capture_rig();
        rig.create_ring();
        let guest = rig.guest();

        // A tail claiming more bytes than the ring can possibly hold — which is
        // also how a backwards tail arrives, in wrapping arithmetic.
        guest.store_word(4, 0x7fff_ffff);
        assert!(matches!(
            rig.renderer.dispatch(CTX, &notify_stream(RING)),
            Err(VenusError::Pump(PumpError::TailOutOfRange { .. }))
        ));
        // The guest is told, so its driver aborts instead of waiting on a head
        // that will never move again.
        assert_eq!(guest.load_word(8) & STATUS_FATAL, STATUS_FATAL);
        assert_eq!(guest.load_word(0), 0, "nothing was consumed");
        assert!(capture.is_empty());

        // And the ring stays dead: a legal tail afterwards changes nothing, and
        // nothing here rebuilds a pump over a ring that has already failed.
        guest.store_word(4, 4);
        rig.produce(0, b"late");
        assert_eq!(
            rig.renderer.dispatch(CTX, &notify_stream(RING)),
            Err(VenusError::Pump(PumpError::Fatal))
        );
        assert!(capture.is_empty());
        assert_eq!(
            rig.renderer.ring_state(CTX, RING).map(|(cur, _)| cur),
            Some(0)
        );

        // Destroying a fatal ring leaves its FATAL bit standing: the guest is
        // entitled to read why its ring died, and a resource that produced an
        // impossible `tail` is not one to hand back looking fresh.
        rig.renderer
            .dispatch(CTX, &destroy_stream(RING))
            .expect("it can still be torn down");
        assert_eq!(guest.load_word(8) & STATUS_FATAL, STATUS_FATAL);
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

        let mut rig = Rig::new(Refuses);
        rig.create_ring();
        let guest = rig.guest();

        // A partial batch is a stall, not a failure: the guest may yet produce
        // the rest, and the pump will not re-offer the same bytes meanwhile.
        rig.produce(0, &[7u8; 8]);
        guest.store_word(4, 8);
        rig.renderer
            .dispatch(CTX, &notify_stream(RING))
            .expect("a stall is not a failure");
        assert_eq!(guest.load_word(8), STATUS_IDLE);

        // A *full* ring the sink refuses is a dead end — the guest cannot
        // produce past `head + size`, and `head` only moves when the sink
        // consumes. Say so rather than let the guest wait forever.
        guest.store_word(4, BUFFER as u32);
        assert!(matches!(
            rig.renderer.dispatch(CTX, &notify_stream(RING)),
            Err(VenusError::RingDeadlocked { ring: RING, .. })
        ));
        assert_eq!(guest.load_word(8) & STATUS_FATAL, STATUS_FATAL);
    }

    // ---------------------------------------------------------------- the caps

    #[test]
    fn the_context_cap_and_the_context_id_rules_hold() {
        let mut renderer = VenusRenderer::new(CaptureSink::new());
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
        let mut renderer = VenusRenderer::new(CaptureSink::new());
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
    fn the_ring_caps_hold_per_context_and_overall() {
        let mem = Arc::new(virtio_core::testing::guest_memory(1 << 20));
        let mut renderer = VenusRenderer::new(CaptureSink::new());
        let window = Window::host_mapped();
        renderer.set_host_visible(Arc::clone(&window) as Arc<dyn ShmBacking>);

        // One blob per ring: two rings in one resource would have to overlap,
        // and the layout fixture is deliberately the same shape every time.
        let mut resource = 0u32;
        let mut blob_for = |renderer: &mut VenusRenderer<CaptureSink>, ctx: u32| {
            resource += 1;
            renderer
                .create_blob(ctx, &blob_args(resource, RESOURCE), &mem, &[])
                .expect("a blob");
            resource
        };

        renderer
            .ctx_create(CTX, crate::CAPSET_VENUS, "")
            .expect("a context");
        for slot in 0..MAX_RINGS_PER_CONTEXT as u64 {
            let resource_id = blob_for(&mut renderer, CTX);
            let info = RingCreateInfo {
                resource_id,
                ..ring_info()
            };
            renderer
                .dispatch(CTX, &create_ring_stream(slot, info))
                .expect("under the per-context cap");
        }
        let spare = blob_for(&mut renderer, CTX);
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
                let resource_id = blob_for(&mut renderer, ctx_id);
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
    }

    // --------------------------------------------------------------- the reset

    #[test]
    fn a_device_reset_takes_every_page_back_out_of_the_guest() {
        let (mut rig, _capture) = capture_rig();
        rig.create_ring();

        rig.renderer.reset();

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
        assert_eq!(rig.guest().load_word(8), STATUS_IDLE);
    }

    // --------------------------------------------------------------- the sinks

    #[test]
    fn the_write_sink_carries_the_ring_into_a_writer() {
        let mut rig = Rig::new(WriteSink::new(Vec::new()));
        rig.create_ring();
        rig.produce(0, b"to a file");
        rig.guest().store_word(4, 9);
        rig.doorbell();

        assert_eq!(rig.renderer.sink().written(), 9);
        assert!(!rig.renderer.sink().failed());
        assert_eq!(rig.renderer.sink().writer().as_slice(), b"to a file");
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

        let mut rig = Rig::new(WriteSink::new(Broken));
        rig.create_ring();
        rig.produce(0, b"lost");
        rig.guest().store_word(4, 4);
        rig.doorbell();

        assert!(rig.renderer.sink().failed());
        // The bytes are gone, but the ring moved on: a guest's Vulkan driver
        // cannot see a host file error and must not be frozen by one.
        assert_eq!(rig.guest().load_word(0), 4);
        assert_eq!(rig.guest().load_word(8), STATUS_IDLE);
    }
}
