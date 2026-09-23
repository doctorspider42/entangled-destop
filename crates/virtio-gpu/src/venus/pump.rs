//! The Venus command ring's **consumption protocol**: the head/tail dance over
//! a validated [`RingLayout`], and nothing above it (EPIC 20, ADR-0004).
//!
//! [`super::ring`] decides *where* the ring is. This module decides *how much
//! of it is ours to read, when, and what we tell the guest afterwards* — and
//! stops there. No command is decoded here, no Vulkan is spoken, no thread is
//! spawned. What comes out is a private copy of some bytes and a decision; who
//! runs the loop, who parks it, and who decodes the bytes are all somebody
//! else's file.
//!
//! The reference is virglrenderer 1.1.0, `src/venus/vkr_ring.c`
//! (`vkr_ring_thread`, `vkr_ring_read_buffer`, `vkr_ring_init_control`), read
//! against `venus-protocol/vn_protocol_renderer_defines.h` for the status bits.
//! Where this file diverges from it, it says so and says why.
//!
//! # The protocol in one paragraph
//!
//! Three lock-free 32-bit atomics live in the shared resource beside the
//! command buffer. **The host stores `head` and `status`; the guest stores
//! `tail`** — [`RingLayout`] already encodes that in the type system, and so
//! does [`RingBacking`] below. There is no length prefix anywhere: the amount
//! of work waiting is `tail - cur` in **wrapping `u32` arithmetic**, where
//! `cur` is how far the host has consumed. Both counters are free-running byte
//! counts, not indices — a byte's position in the buffer is `offset & (size -
//! 1)`, which is why [`RingLayout`] insists the buffer is a power of two. The
//! host republishes `head = cur` as it consumes, and that is the only thing
//! that ever frees ring space for the guest.
//!
//! # What this layer promises the layer above
//!
//! 1. **The bytes handed to a [`RingSink`] are a stable private copy.** They
//!    live in a host-owned shadow buffer that the guest cannot reach. This is
//!    not an optimisation and not a convenience: the guest can rewrite the ring
//!    while the host reads it, so anything decoded *in place* can change
//!    between the length check and the use of the thing measured — the classic
//!    double-fetch. The reference copies to `ring->cmd` before decoding for
//!    exactly this reason, and so do we. It is mandatory.
//! 2. **The copy is arbitrary bytes, not trustworthy bytes.** Stability is all
//!    that is promised. A guest that scribbles over the ring mid-copy gets a
//!    shadow spliced from two different versions of its own data; that is its
//!    problem, and the decoder above must already be safe against any byte
//!    sequence at all.
//! 3. **Each call is a fresh view.** Bytes a sink declines are re-offered on a
//!    later pass, re-read from the ring, and a hostile guest may have changed
//!    them in between. A sink must not carry decoded state across calls that
//!    assumes byte stability between them.
//! 4. **The cursor's unit is bytes.** [`RingPump::cursor`] is the free-running
//!    wrapping count of bytes consumed since the ring was created, in exactly
//!    the units `head` and `tail` are expressed in — never an index into the
//!    command buffer, never a command count. It is published verbatim as
//!    `head`, and `head == cursor` holds after every pass.
//!
//! # Two deliberate divergences from the reference
//!
//! **Consumption granularity is the sink's, not ours.** The reference
//! republishes `head` after every dispatched command, which it can do because
//! it decodes as it goes. This file does not know where a command ends — that
//! is [`super::wire`]'s job and the dispatcher's above it — so it hands the
//! whole shadow to the sink and publishes exactly what the sink says it took.
//! A sink that decodes one command per call gets the reference's granularity;
//! one that decodes a whole batch gets a batch's.
//!
//! **We advance `cur` by what was consumed, not by what was copied.** The
//! reference advances `buffer.cur` by the whole batch inside
//! `vkr_ring_read_buffer`, before a single command has been dispatched, and
//! then walks `head` forward through it separately — so a trailing partial
//! command is dropped by `vkr_cs_decoder_reset` rather than completed. Here
//! there is one cursor, it moves only over bytes the sink accepted, and the
//! remainder is re-offered when more arrives. That is the only way a command
//! split across two guest writes can ever be decoded, and it keeps `head` and
//! `cur` the same number, which is one fewer thing a snapshot has to agree
//! about.
//!
//! # The guest is untrusted, and here is the whole list
//!
//! Everything below is a guest-controlled value and none of it may reach host
//! memory unchecked:
//!
//! * `tail` **moving backwards**, or forwards by more than the buffer holds.
//!   Both show up as one condition — `tail - cur` (wrapping) greater than the
//!   buffer size — because no correct producer can ever have more than a
//!   bufferful outstanding. That is the reference's check
//!   (`cmd_size > ring->buffer.size` is `-EINVAL` and a `FATAL` status bit) and
//!   it is ours. Note what it cannot distinguish and must not try to: a
//!   backwards move of almost 2³² *is* a legal forward move in wrapping
//!   arithmetic, and answering "legal" there is correct, not lenient.
//! * `tail` **changing while we copy**. We resolve this by snapshot: `tail` is
//!   loaded once, the batch is exactly `tail - cur` bytes, and the copy is not
//!   extended or re-validated afterwards. The acquire load of `tail` is what
//!   orders the guest's writes to `[cur, tail)` before our reads of them;
//!   bytes past `tail` carry no such ordering, so reading them would be
//!   reading with no happens-before at all. Growing the batch mid-copy buys
//!   nothing anyway — the next pass picks the new bytes up immediately.
//! * a batch that **wraps the end of the buffer**, including one exactly the
//!   buffer's length. Both halves are computed from the mask and are provably
//!   inside the buffer region, and the argument for that is written out over
//!   `RingPump::fill_shadow`.
//! * `head`/`tail` values that are **not aligned to anything**. The protocol
//!   promises no alignment of either cursor, so nothing here assumes any.
//! * an **`idleTimeout` of `u64::MAX`** nanoseconds — 584 years. [`RingLayout`]
//!   carries it unvalidated on purpose; the clamp is [`MAX_IDLE_TIMEOUT`] and
//!   lives here, where the waiting happens.
//! * a **sink that makes no progress**, which must not become a spin. See
//!   [`Pass::Stalled`] and [`Pass::Deadlocked`].
//!
//! # What is deliberately not here
//!
//! The thread, the backoff, the condition variable, the `virtio_core::Quiesce`
//! gate and the `save`/`load` pair (ADR-0005, ADR-0006). A driver built on this
//! needs to persist exactly two things across a snapshot — [`RingPump::cursor`]
//! and [`RingPump::status`] — plus the layout it was built from; the shadow is
//! scratch and is refilled from the ring on the next pass.
//!
//! The reference's loop, for whoever writes that thread:
//!
//! ```text
//! loop {
//!     if now >= last_progress + pump.idle_timeout() {
//!         match pump.enter_idle(&backing) {          // publishes IDLE, then
//!             Idle::Park => { block_until_notified(); // RE-READS tail
//!                             pump.leave_idle(&backing);
//!                             last_progress = now; }
//!             Idle::WorkArrived => {}                // IDLE already taken down
//!         }
//!     }
//!     match pump.pump(&backing, &mut sink)? {
//!         Pass::Progress { .. } => last_progress = now,
//!         Pass::Idle | Pass::Stalled { .. } => relax(),
//!         Pass::Deadlocked { .. } => break,
//!     }
//! }
//! ```
//!
//! The order inside `enter_idle` is the part that is easy to get wrong and
//! impossible to debug: publish IDLE *first*, then re-read `tail`. Publishing
//! idle and then not re-checking is a lost wakeup — the guest reads a
//! not-yet-idle status, decides no doorbell is needed, stores `tail`, and the
//! host parks forever on work that is already there.

use std::time::Duration;

use thiserror::Error;

use super::ring::{GuestWord, HostWord, Region, RingLayout};

/// `VK_RING_STATUS_IDLE_BIT_MESA`: the host has stopped polling this ring and
/// needs a doorbell to notice new work.
pub const STATUS_IDLE: u32 = 0x0000_0001;

/// `VK_RING_STATUS_FATAL_BIT_MESA`: the host has given up on this ring. The
/// guest's driver aborts rather than waiting for a `head` that will never move.
pub const STATUS_FATAL: u32 = 0x0000_0002;

/// `VK_RING_STATUS_ALIVE_BIT_MESA`: the answer to the guest's watchdog.
///
/// Defined here for completeness and never set by this module: in the
/// reference it is `vkr_context`'s monitor that sets it, one level above the
/// ring (`vkr_context.c`, `vkr_ring_set_status_bits(ring, ALIVE)`), because the
/// question it answers is "is the ring thread still running", which a ring
/// cannot answer about itself.
pub const STATUS_ALIVE: u32 = 0x0000_0004;

/// The longest the guest may ask the host to keep polling before parking.
///
/// `VkRingCreateInfoMESA::idleTimeout` is a guest-chosen `u64` of nanoseconds
/// that nothing in the protocol validates — the reference passes it from
/// `vkr_transport.c` straight into `vkr_ring_create` and compares against it
/// with no ceiling at all, so `u64::MAX` buys a 584-year spin. The clamp is
/// ours, and 100 ms is where it sits for three reasons:
///
/// * the value is a *spin* budget, not a deadline. Every real producer's is in
///   the microsecond-to-low-millisecond range, so 100 ms cannot clip a
///   legitimate one — it is two orders of magnitude of headroom over anything
///   a driver has reason to ask for;
/// * it is the worst-case extra latency a pause, reset or snapshot inherits.
///   ADR-0005 makes quiescing wait for every host thread that touches guest
///   memory to reach a stopping point, and a pump thread's stopping point is
///   the park; a timeout is therefore a direct tax on "paused" meaning paused;
/// * it is below the threshold where a stall is perceptible, so clamping a
///   greedier guest down to it costs at worst one doorbell round-trip and never
///   a visible hitch.
///
/// There is deliberately no floor. Zero is a meaningful request — "park at
/// once, I will ring the doorbell" — and is honoured exactly.
pub const MAX_IDLE_TIMEOUT: Duration = Duration::from_millis(100);

/// The shared memory the ring lives in, as the four operations this module
/// performs on it.
///
/// Implementing this is what connects the pump to a real guest; a test
/// implements it over a `Vec<u8>` and plays the guest by hand, which is why
/// every hostile case below has a test and none of them needs a VM.
///
/// # What an implementation owes
///
/// * **A mapping of the whole resource the layout was validated against**, for
///   as long as the pump exists. Every offset the pump passes came out of a
///   [`RingLayout`] that was checked against that resource's size, so they are
///   in bounds by construction — but only of *that* resource. Tearing the
///   mapping down while a pump is live is the owner's bug to avoid, which is
///   why these methods cannot fail: a backing that might vanish is not a
///   backing, it is a lifetime problem to solve one level up.
/// * **Sequentially consistent** loads and stores of the control words. The
///   reference is weaker — release on `head`, acquire on `tail`, `seq_cst`
///   read-modify-write on `status` — and we could be too, except that the idle
///   handshake in [`RingPump::enter_idle`] is a store to `status` followed by a
///   load of `tail`, the one pairing that release/acquire does not order. On
///   both of our hosts (x86-64, always) `SeqCst` costs a plain `mov` on the
///   load and an `xchg` on the store, and buying the `StoreLoad` barrier
///   outright is cheaper than reasoning about which call site needs it.
/// * **A whole-buffer read that cannot tear into host memory.** The bytes may
///   be changing under the copy — that is the guest's privilege and the reason
///   the shadow exists — so the implementation must perform the copy in a way
///   that stays defined when it races (a volatile copy, or `vm-memory`'s
///   checked read). The *values* are allowed to be nonsense.
///
/// # Who may write what
///
/// The type-state from [`super::ring`] comes through here intact: there is a
/// store for a [`HostWord`] and there is no store for a [`GuestWord`], so a
/// backing implementation cannot be asked to write the tail even by mistake.
pub trait RingBacking {
    /// Load one of the words the **host** owns (`head`, `status`).
    ///
    /// Used once, at construction, to check the guest zeroed them.
    fn load_host_word(&self, word: &HostWord) -> u32;

    /// Load the word the **guest** owns (`tail`).
    fn load_guest_word(&self, word: &GuestWord) -> u32;

    /// Store to one of the words the **host** owns. There is no counterpart
    /// for [`GuestWord`], and that is the point.
    fn store_host_word(&self, word: &HostWord, value: u32);

    /// Copy `dst.len()` bytes out of the command buffer, starting `offset`
    /// bytes into it, filling `dst` completely.
    ///
    /// `offset` is relative to `buffer.start()`, and the pump guarantees
    /// `offset + dst.len() <= buffer.len()` — both halves of a wrapped batch
    /// are derived from the power-of-two mask, so neither can escape. An
    /// implementation should nonetheless clamp rather than panic if it is ever
    /// handed something else; nothing in this crate may panic on a path a
    /// guest can steer.
    fn read_buffer(&self, buffer: &Region, offset: u64, dst: &mut [u8]);
}

/// One batch of ring bytes, as a private copy the sink may read freely.
///
/// The only way to answer a [`RingSink::consume`] is with a [`Consumed`], and
/// the only way to build a `Consumed` is from the `Batch` that was offered —
/// so "the sink consumed more than it was given" is not a bug that can be
/// written, rather than one that has to be asserted against.
#[derive(Debug, Clone, Copy)]
pub struct Batch<'a> {
    bytes: &'a [u8],
}

impl<'a> Batch<'a> {
    /// The bytes, in ring order: index 0 is the byte at the pump's cursor, and
    /// a batch that wrapped the end of the buffer has already been spliced
    /// back into one contiguous run.
    #[must_use]
    pub fn bytes(&self) -> &'a [u8] {
        self.bytes
    }

    /// How many bytes are on offer. Never zero — the pump does not call a sink
    /// with an empty batch.
    #[must_use]
    pub fn len(&self) -> usize {
        self.bytes.len()
    }

    /// Always `false`; present because clippy asks for it beside [`len`](Self::len).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.bytes.is_empty()
    }

    /// Report that the first `n` bytes were consumed, clamped to the batch.
    #[must_use]
    pub fn consumed(&self, n: usize) -> Consumed {
        let n = n.min(self.bytes.len());
        Consumed(u32::try_from(n).unwrap_or(0))
    }

    /// Report that the whole batch was consumed.
    #[must_use]
    pub fn all(&self) -> Consumed {
        self.consumed(self.bytes.len())
    }

    /// Report that none of it was consumed — "I cannot make progress on this
    /// many bytes". The pump will not offer the same bytes again until the
    /// guest produces more; see [`Pass::Stalled`].
    #[must_use]
    pub fn nothing(&self) -> Consumed {
        Consumed(0)
    }
}

/// How many bytes of a [`Batch`] a sink took. Constructible only from the batch
/// it answers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Consumed(u32);

impl Consumed {
    /// The count, in bytes.
    #[must_use]
    pub fn bytes(self) -> u32 {
        self.0
    }
}

/// Whatever turns ring bytes into work.
///
/// The pump knows nothing about command boundaries, so the sink sets the
/// granularity: consume one command and the pump republishes `head` after every
/// command, like the reference; consume the batch and it republishes once.
///
/// Returning [`Batch::nothing`] is the correct answer to a batch that does not
/// yet hold a whole unit of work. It is not an error, and it does not cost a
/// spin: the pump will not re-offer the same bytes until the guest's `tail`
/// moves (see [`Pass::Stalled`]).
pub trait RingSink {
    /// Take some prefix of `batch` and say how much.
    fn consume(&mut self, batch: Batch<'_>) -> Consumed;
}

/// What one pass over the ring did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pass {
    /// The guest has produced nothing the host has not already consumed.
    /// Nothing was copied, no sink was called, `head` was not republished.
    Idle,

    /// `consumed` of the `offered` bytes were taken and `head` was republished
    /// as the new cursor. `consumed` is never zero and never exceeds `offered`.
    Progress {
        /// How many bytes the guest had waiting.
        offered: u32,
        /// How many of them the sink took.
        consumed: u32,
    },

    /// Bytes were waiting and the sink took none of them; `head` is unchanged.
    ///
    /// The pump remembers the `tail` this happened at and will not call the
    /// sink again with the same bytes, so a driver that loops on `Stalled` is
    /// cheap rather than hot — but it should still treat this exactly like
    /// [`Pass::Idle`] and go round the idle path, because nothing will change
    /// until the guest produces more. [`RingPump::enter_idle`] knows about the
    /// stall and will park on it.
    Stalled {
        /// How many bytes were on offer and refused.
        offered: u32,
    },

    /// The sink took nothing and the ring is **full**, so nothing can ever
    /// arrive to unstick it: the guest cannot produce past `head + size`, and
    /// `head` only moves when the sink consumes.
    ///
    /// This is a liveness dead end rather than a safety problem, so the pump
    /// reports it instead of deciding what to do about it. A driver that just
    /// keeps pumping hangs the guest silently, which is the worst available
    /// outcome; the usual answer is [`RingPump::mark_fatal`], which tells the
    /// guest's driver to give up rather than wait.
    Deadlocked {
        /// How many bytes were on offer. Always the buffer's full length.
        offered: u32,
    },
}

/// What [`RingPump::enter_idle`] decided.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Idle {
    /// [`STATUS_IDLE`] is published and a re-read of `tail` confirmed there is
    /// still nothing to do. The driver may block until the doorbell rings, and
    /// must call [`RingPump::leave_idle`] when it wakes.
    Park,
    /// Work appeared between the last pass and the idle publication, so
    /// [`STATUS_IDLE`] has already been taken back down and the driver must
    /// **not** block. This is the lost-wakeup case the re-read exists for.
    WorkArrived,
}

/// Why a pass could not be made.
///
/// None of these is a host error: each is a guest that produced something no
/// correct producer produces, or a ring already written off because of one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum PumpError {
    #[error(
        "the host owns head and status and expects both zeroed at ring \
         creation, but head is {head:#x} and status is {status:#x}"
    )]
    ControlWordsNotZeroed { head: u32, status: u32 },

    #[error(
        "the guest published a tail of {tail:#x} against a cursor of \
         {cur:#x}, claiming {claimed:#x} bytes from a {buffer_size:#x}-byte \
         ring buffer"
    )]
    TailOutOfRange {
        cur: u32,
        tail: u32,
        claimed: u32,
        buffer_size: u32,
    },

    #[error(
        "the command buffer is {size:#x} bytes, which is not a usable \
         power-of-two ring size"
    )]
    UnusableBuffer { size: u64 },

    #[error("the ring has been marked fatal and will consume nothing further")]
    Fatal,
}

/// The head/tail protocol over one validated ring, as pure synchronous logic.
///
/// Owns the host-private shadow buffer and the consumed cursor, and nothing
/// else — no thread, no lock, no clock. Every method takes the backing it
/// should act on, so the same pump can be exercised against a mock in a test
/// and a mapped resource in a VM without changing shape.
#[derive(Debug)]
pub struct RingPump {
    layout: RingLayout,
    /// [`RingLayout::idle_timeout_ns`] clamped to [`MAX_IDLE_TIMEOUT`].
    idle_timeout: Duration,
    /// `layout.buffer().len()`, which [`RingLayout`] guarantees is a power of
    /// two in `1..=MAX_BUFFER_BYTES` and therefore fits a `u32`.
    buffer_size: u32,
    /// `buffer_size - 1`: the mask that turns a free-running cursor into a
    /// position in the buffer.
    mask: u32,
    /// Bytes consumed since the ring was created, wrapping. Published verbatim
    /// as `head`.
    cur: u32,
    /// Our mirror of the status word.
    ///
    /// The reference uses `atomic_fetch_or`/`atomic_fetch_and` on the shared
    /// word; we keep the value host-side and store it whole. The read-modify-
    /// write buys nothing once [`super::ring`] has established that the host is
    /// the only writer — and preserving bits a misbehaving guest scribbled into
    /// a word it does not own is not a property worth having.
    status: u32,
    /// The host-private copy the sink is handed. Grows to the largest batch
    /// seen and never past `buffer_size`; a ring that only ever carries small
    /// batches never pays for the 16 MiB one it was allowed to declare. (The
    /// reference `malloc`s the full buffer size up front.)
    shadow: Vec<u8>,
    /// The `tail` at which the sink last refused to make progress, if it did.
    /// Re-offering the same bytes to the same sink cannot produce a different
    /// answer, so this is what keeps a stalled ring from becoming a spin.
    stalled_at: Option<u32>,
    /// Set once the ring is written off; every later pass refuses.
    fatal: bool,
}

impl RingPump {
    /// Start consuming a validated ring.
    ///
    /// Checks the one precondition the reference checks in
    /// `vkr_ring_init_control`: `head` and `status` are the host's words, and
    /// the host expects to find them zeroed. A guest that hands over a ring
    /// with either already set is either confused about who owns them or
    /// trying to start the host mid-stream, and the ring is refused rather than
    /// adopted. `tail` is deliberately *not* checked — a guest is entitled to
    /// have queued commands before the host looked.
    ///
    /// # Errors
    ///
    /// [`PumpError::ControlWordsNotZeroed`] for a dirty `head` or `status`, and
    /// [`PumpError::UnusableBuffer`] for a buffer size [`RingLayout`] should
    /// already have made impossible.
    pub fn new(layout: RingLayout, backing: &impl RingBacking) -> Result<Self, PumpError> {
        // `RingLayout` refuses any buffer outside `1..=MAX_BUFFER_BYTES` and
        // any that is not a power of two, so neither of these can fire. They
        // cost one branch each and remove the question of what the mask would
        // mean if the invariant were ever weakened — the same bargain
        // `ring.rs` makes with its 32-bit-host check.
        let size = u32::try_from(layout.buffer().len()).unwrap_or(0);
        if size == 0 || !size.is_power_of_two() {
            return Err(PumpError::UnusableBuffer {
                size: layout.buffer().len(),
            });
        }

        let head = backing.load_host_word(&layout.head());
        let status = backing.load_host_word(&layout.status());
        if head != 0 || status != 0 {
            return Err(PumpError::ControlWordsNotZeroed { head, status });
        }

        Ok(Self {
            layout,
            idle_timeout: Duration::from_nanos(layout.idle_timeout_ns()).min(MAX_IDLE_TIMEOUT),
            buffer_size: size,
            mask: size - 1,
            cur: 0,
            status: 0,
            shadow: Vec::new(),
            stalled_at: None,
            fatal: false,
        })
    }

    /// The layout this pump was built from.
    #[must_use]
    pub fn layout(&self) -> &RingLayout {
        &self.layout
    }

    /// The guest's `idleTimeout`, clamped to [`MAX_IDLE_TIMEOUT`]. How long a
    /// driver should keep polling after the last progress before it calls
    /// [`enter_idle`](Self::enter_idle).
    #[must_use]
    pub fn idle_timeout(&self) -> Duration {
        self.idle_timeout
    }

    /// The command buffer's length in bytes: the most a single batch can ever
    /// be, and the value a [`Pass::Deadlocked`] reports.
    #[must_use]
    pub fn buffer_len(&self) -> u32 {
        self.buffer_size
    }

    /// Bytes consumed since the ring was created, wrapping — the value last
    /// published as `head`. Not an index into the buffer; see the module docs.
    #[must_use]
    pub fn cursor(&self) -> u32 {
        self.cur
    }

    /// The status word as the host last published it.
    #[must_use]
    pub fn status(&self) -> u32 {
        self.status
    }

    /// Whether [`STATUS_IDLE`] is currently published.
    #[must_use]
    pub fn is_idle(&self) -> bool {
        self.status & STATUS_IDLE != 0
    }

    /// Whether the ring has been written off.
    #[must_use]
    pub fn is_fatal(&self) -> bool {
        self.fatal
    }

    /// Write the ring off: publish [`STATUS_FATAL`] so the guest's driver
    /// aborts instead of waiting on a `head` that will never move again, and
    /// refuse every later pass.
    ///
    /// Called automatically when the guest's `tail` claims more than can exist;
    /// exposed because [`Pass::Deadlocked`] is the caller's policy decision,
    /// not this layer's.
    pub fn mark_fatal(&mut self, backing: &impl RingBacking) {
        self.fatal = true;
        self.set_status(backing, self.status | STATUS_FATAL);
    }

    /// Return the ring to power-on: cursor and status zeroed on both sides,
    /// shadow dropped, fatal cleared (ADR-0005).
    ///
    /// The two host-owned words are stored, not merely forgotten — a reboot
    /// that leaves a stale `head` in shared memory is exactly the haunting
    /// ADR-0005 is about, and a guest that re-creates its ring re-runs the
    /// zeroed check in [`new`](Self::new).
    pub fn reset(&mut self, backing: &impl RingBacking) {
        self.cur = 0;
        self.stalled_at = None;
        self.fatal = false;
        self.shadow = Vec::new();
        self.status = 0;
        // Unconditionally, unlike `set_status`: a reset is the one moment the
        // host's mirror of shared memory is worth distrusting, because the
        // guest that is being reset may have scribbled on words it does not
        // own, and "power-on" has to mean the bytes, not our opinion of them.
        backing.store_host_word(&self.layout.head(), 0);
        backing.store_host_word(&self.layout.status(), 0);
    }

    /// Make one pass over the ring.
    ///
    /// Loads `tail` once, copies `tail - cur` bytes into the shadow (splicing a
    /// wrap back together), offers them to the sink, and republishes `head` as
    /// whatever the sink took. Everything about the amount of work is decided
    /// from that single load; see the module docs on the snapshot.
    ///
    /// # Errors
    ///
    /// [`PumpError::TailOutOfRange`] when the guest claims more bytes than the
    /// ring can hold — which is also how a backwards `tail` arrives. The ring
    /// is marked fatal, so the error is reported once and every later call
    /// answers [`PumpError::Fatal`].
    pub fn pump(
        &mut self,
        backing: &impl RingBacking,
        sink: &mut impl RingSink,
    ) -> Result<Pass, PumpError> {
        if self.fatal {
            return Err(PumpError::Fatal);
        }

        // One load. Everything below is decided against this snapshot, and
        // nothing re-reads it — see "tail changing while we copy".
        let tail = backing.load_guest_word(&self.layout.tail());
        let claimed = tail.wrapping_sub(self.cur);

        if claimed == 0 {
            self.stalled_at = None;
            return Ok(Pass::Idle);
        }
        if claimed > self.buffer_size {
            // Backwards, or forwards past the end of what can exist. The
            // reference's `-EINVAL` plus `VK_RING_STATUS_FATAL_BIT_MESA`.
            self.mark_fatal(backing);
            return Err(PumpError::TailOutOfRange {
                cur: self.cur,
                tail,
                claimed,
                buffer_size: self.buffer_size,
            });
        }

        // The sink already saw exactly these bytes and took none of them;
        // asking again cannot produce a different answer, so do not copy and
        // do not call it. This is what stops "a sink that consumes nothing"
        // from becoming a busy loop through the shadow buffer.
        if self.stalled_at == Some(tail) {
            return Ok(self.stall_outcome(claimed));
        }

        self.fill_shadow(backing, claimed);
        let taken = sink
            .consume(Batch {
                bytes: &self.shadow,
            })
            .0;
        // `Consumed` can only be built from the batch it answers, so this
        // cannot exceed `claimed`. Clamped anyway: the alternative to a
        // redundant `min` here is a cursor that runs past the guest's tail.
        let taken = taken.min(claimed);

        if taken == 0 {
            self.stalled_at = Some(tail);
            return Ok(self.stall_outcome(claimed));
        }

        self.stalled_at = None;
        self.cur = self.cur.wrapping_add(taken);
        backing.store_host_word(&self.layout.head(), self.cur);
        Ok(Pass::Progress {
            offered: claimed,
            consumed: taken,
        })
    }

    /// Publish [`STATUS_IDLE`] and then re-read `tail`, in that order.
    ///
    /// The ordering is the whole point and is the reference's
    /// (`vkr_ring_thread`): the guest decides whether to ring the doorbell by
    /// reading `status`, so a host that parks before publishing idle — or
    /// publishes idle without re-checking — loses the wakeup for work stored
    /// between the two.
    ///
    /// Answers [`Idle::Park`] when there is genuinely nothing to do, which
    /// includes the case where bytes are waiting but the sink already refused
    /// them at this exact `tail`: re-offering them cannot help, and spinning on
    /// them is the busy loop this exists to prevent. Any *new* byte moves
    /// `tail` and wakes the ring normally.
    pub fn enter_idle(&mut self, backing: &impl RingBacking) -> Idle {
        self.set_status(backing, self.status | STATUS_IDLE);
        let tail = backing.load_guest_word(&self.layout.tail());
        if tail == self.cur || self.stalled_at == Some(tail) {
            Idle::Park
        } else {
            self.set_status(backing, self.status & !STATUS_IDLE);
            Idle::WorkArrived
        }
    }

    /// Take [`STATUS_IDLE`] back down after waking from a park. Idempotent.
    pub fn leave_idle(&mut self, backing: &impl RingBacking) {
        if self.is_idle() {
            self.set_status(backing, self.status & !STATUS_IDLE);
        }
    }

    /// A refused batch, told apart from one that can never be un-refused.
    fn stall_outcome(&self, offered: u32) -> Pass {
        if offered == self.buffer_size {
            Pass::Deadlocked { offered }
        } else {
            Pass::Stalled { offered }
        }
    }

    /// Store the status word and remember what we stored. Skips the store when
    /// nothing changes, so a driver polling the idle path does not put a
    /// pointless `xchg` on a word the guest is reading.
    ///
    /// Skipping it also skips the `StoreLoad` barrier
    /// [`enter_idle`](Self::enter_idle) relies on, which is safe for the only
    /// case that can reach it: the bit is already published, so the guest is
    /// already ringing the doorbell for every store it makes, and there is no
    /// wakeup left to lose.
    fn set_status(&mut self, backing: &impl RingBacking, value: u32) {
        if self.status != value {
            self.status = value;
            backing.store_host_word(&self.layout.status(), value);
        }
    }

    /// Copy `len` bytes from the cursor into the shadow, splicing a wrap.
    ///
    /// The caller has established `len <= buffer_size`. Masking gives
    /// `start < buffer_size`, so the first read covers
    /// `start..start + min(len, size - start)` — inside the buffer — and the
    /// second covers `0..len - first`, where `len - first` is at most `start`
    /// and therefore also inside. Neither range depends on a guest value that
    /// has not been masked or bounded, and no arithmetic here can wrap.
    fn fill_shadow(&mut self, backing: &impl RingBacking, len: u32) {
        let buffer = self.layout.buffer();
        let size = u64::from(self.buffer_size);
        let start = u64::from(self.cur & self.mask);
        // `len` is at most the buffer size, which `RingLayout` caps at 16 MiB;
        // the fallback keeps the promise that nothing here panics.
        let want = usize::try_from(len).unwrap_or(0);
        // Truncates or zero-extends; every byte in `0..want` is overwritten
        // below, so no stale bytes from an earlier batch can reach the sink.
        self.shadow.resize(want, 0);

        let first = size.saturating_sub(start).min(u64::from(len));
        let split = usize::try_from(first).unwrap_or(0).min(want);
        let (front, back) = self.shadow.split_at_mut(split);
        backing.read_buffer(&buffer, start, front);
        if !back.is_empty() {
            backing.read_buffer(&buffer, 0, back);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::venus::ring::RingCreateInfo;

    use std::cell::RefCell;
    use std::rc::Rc;

    /// The resource every fixture lives in.
    const RESOURCE: u64 = 0x1000;
    /// A deliberately tiny command buffer, so a wrap is four bytes away.
    const BUFFER: u32 = 64;

    /// Every operation a backing performed, in order. The idle handshake is an
    /// *ordering* property, so a test that cannot see the order cannot assert
    /// the thing that matters.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum Op {
        LoadHost(u64),
        LoadGuest(u64),
        StoreHost(u64, u32),
        ReadBuffer(u64, usize),
    }

    /// A guest, played by hand: one flat resource, plus hooks for the two
    /// races a real one can run against us.
    #[derive(Debug, Default)]
    struct Inner {
        mem: Vec<u8>,
        log: Vec<Op>,
        /// Bytes written into the command buffer *after* every `read_buffer`,
        /// at a buffer-relative offset: a guest rewriting the ring under the
        /// copy.
        rewrite_after_read: Option<(u64, Vec<u8>)>,
        /// Added to `tail` after every `read_buffer`: a guest producing more
        /// while the copy is in flight.
        bump_tail_after_read: u32,
    }

    #[derive(Debug)]
    struct MockRing {
        inner: RefCell<Inner>,
        layout: RingLayout,
    }

    impl MockRing {
        fn new() -> Rc<Self> {
            Self::with_buffer_size(BUFFER)
        }

        fn with_buffer_size(buffer_size: u32) -> Rc<Self> {
            Self::with_info(RingCreateInfo {
                buffer_size: u64::from(buffer_size),
                ..base_info()
            })
        }

        fn with_info(info: RingCreateInfo) -> Rc<Self> {
            let layout = RingLayout::new(info, RESOURCE).expect("fixture layout is valid");
            Rc::new(Self {
                inner: RefCell::new(Inner {
                    mem: vec![0; RESOURCE as usize],
                    ..Inner::default()
                }),
                layout,
            })
        }

        fn layout(&self) -> RingLayout {
            self.layout
        }

        fn buffer_size(&self) -> u32 {
            u32::try_from(self.layout.buffer().len()).expect("fixture buffer fits a u32")
        }

        /// Write into the command buffer at a free-running ring offset, the way
        /// the guest does: masked, and wrapping the end.
        fn produce(&self, at: u32, bytes: &[u8]) {
            let base = self.layout.buffer().start();
            let size = self.buffer_size();
            let mut inner = self.inner.borrow_mut();
            for (i, byte) in bytes.iter().enumerate() {
                let off = at.wrapping_add(u32::try_from(i).expect("test batch fits a u32")) % size;
                let index = (base + u64::from(off)) as usize;
                inner.mem[index] = *byte;
            }
        }

        fn set_tail(&self, tail: u32) {
            let at = self.layout.tail().offset() as usize;
            let mut inner = self.inner.borrow_mut();
            inner.mem[at..at + 4].copy_from_slice(&tail.to_le_bytes());
        }

        fn word(&self, at: u64) -> u32 {
            let at = at as usize;
            let inner = self.inner.borrow();
            u32::from_le_bytes(inner.mem[at..at + 4].try_into().expect("four bytes"))
        }

        fn head(&self) -> u32 {
            self.word(self.layout.head().offset())
        }

        fn status(&self) -> u32 {
            self.word(self.layout.status().offset())
        }

        /// Scribble directly on a host-owned word, to build a ring that is
        /// already dirty before the host adopts it.
        fn poke(&self, at: u64, value: u32) {
            let at = at as usize;
            let mut inner = self.inner.borrow_mut();
            inner.mem[at..at + 4].copy_from_slice(&value.to_le_bytes());
        }

        fn ops(&self) -> Vec<Op> {
            self.inner.borrow().log.clone()
        }

        fn clear_ops(&self) {
            self.inner.borrow_mut().log.clear();
        }

        fn stores_to(&self, word: u64) -> Vec<u32> {
            self.ops()
                .into_iter()
                .filter_map(|op| match op {
                    Op::StoreHost(at, v) if at == word => Some(v),
                    _ => None,
                })
                .collect()
        }
    }

    impl RingBacking for MockRing {
        fn load_host_word(&self, word: &HostWord) -> u32 {
            let at = word.offset();
            self.inner.borrow_mut().log.push(Op::LoadHost(at));
            self.word(at)
        }

        fn load_guest_word(&self, word: &GuestWord) -> u32 {
            let at = word.offset();
            self.inner.borrow_mut().log.push(Op::LoadGuest(at));
            self.word(at)
        }

        fn store_host_word(&self, word: &HostWord, value: u32) {
            let at = word.store_offset();
            self.inner.borrow_mut().log.push(Op::StoreHost(at, value));
            self.poke(at, value);
        }

        fn read_buffer(&self, buffer: &Region, offset: u64, dst: &mut [u8]) {
            // The pump promises this; a mock that does not check it would let a
            // masking bug through as a silently wrong byte instead of a failure.
            assert!(
                buffer.contains_range(offset, dst.len() as u64),
                "pump asked for {offset:#x}..+{:#x} of a {:#x}-byte buffer",
                dst.len(),
                buffer.len()
            );
            {
                let mut inner = self.inner.borrow_mut();
                inner.log.push(Op::ReadBuffer(offset, dst.len()));
                let from = (buffer.start() + offset) as usize;
                dst.copy_from_slice(&inner.mem[from..from + dst.len()]);
            }

            // Now play whatever race this test scripted, *after* the bytes are
            // in the host's hands.
            let rewrite = self.inner.borrow().rewrite_after_read.clone();
            if let Some((at, bytes)) = rewrite {
                self.produce(u32::try_from(at).expect("test offset fits a u32"), &bytes);
            }
            let bump = self.inner.borrow().bump_tail_after_read;
            if bump != 0 {
                let tail = self.word(self.layout.tail().offset());
                self.set_tail(tail.wrapping_add(bump));
            }
        }
    }

    /// How much of a batch a scripted sink feels like taking.
    type Appetite<'f> = Box<dyn FnMut(&[u8]) -> usize + 'f>;

    /// A sink whose appetite is a closure, recording every batch it was shown.
    struct Sink<'f> {
        policy: Appetite<'f>,
        seen: Vec<Vec<u8>>,
        calls: usize,
    }

    impl<'f> Sink<'f> {
        fn new(policy: impl FnMut(&[u8]) -> usize + 'f) -> Self {
            Self {
                policy: Box::new(policy),
                seen: Vec::new(),
                calls: 0,
            }
        }

        fn everything() -> Self {
            Self::new(|bytes: &[u8]| bytes.len())
        }

        fn nothing() -> Self {
            Self::new(|_: &[u8]| 0)
        }

        fn take(n: usize) -> Self {
            Self::new(move |bytes: &[u8]| n.min(bytes.len()))
        }
    }

    impl RingSink for Sink<'_> {
        fn consume(&mut self, batch: Batch<'_>) -> Consumed {
            self.calls += 1;
            self.seen.push(batch.bytes().to_vec());
            let n = (self.policy)(batch.bytes());
            batch.consumed(n)
        }
    }

    fn pump_on(ring: &MockRing) -> RingPump {
        RingPump::new(ring.layout(), ring).expect("a zeroed ring is adopted")
    }

    // ----------------------------------------------------------------- setup

    #[test]
    fn a_zeroed_ring_is_adopted_and_a_dirty_one_is_refused() {
        let ring = MockRing::new();
        let pump = pump_on(&ring);
        assert_eq!(pump.cursor(), 0);
        assert_eq!(pump.status(), 0);
        assert_eq!(pump.buffer_len(), BUFFER);
        assert!(!pump.is_fatal());

        // The reference's `vkr_ring_init_control` check: the host owns these
        // two words and expects to find them zero.
        for (word, head, status) in [
            (ring.layout().head().offset(), 1, 0),
            (ring.layout().status().offset(), 0, STATUS_IDLE),
        ] {
            let dirty = MockRing::new();
            dirty.poke(word, head | status);
            assert_eq!(
                RingPump::new(dirty.layout(), &*dirty).expect_err("a dirty ring is refused"),
                PumpError::ControlWordsNotZeroed { head, status }
            );
        }

        // A non-zero `tail` is *not* refused: a guest may legitimately have
        // queued work before the host ever looked at the ring.
        let early = MockRing::new();
        early.set_tail(8);
        early.produce(0, b"12345678");
        let mut pump = pump_on(&early);
        let mut sink = Sink::everything();
        assert_eq!(
            pump.pump(&*early, &mut sink),
            Ok(Pass::Progress {
                offered: 8,
                consumed: 8
            })
        );
        assert_eq!(sink.seen, vec![b"12345678".to_vec()]);
    }

    #[test]
    fn the_idle_timeout_is_clamped_to_something_a_pause_can_wait_for() {
        for (asked, want) in [
            (0u64, Duration::ZERO),
            (1_000, Duration::from_micros(1)),
            (50_000_000, Duration::from_millis(50)),
            // Exactly the clamp, and then the two values that make the clamp
            // load-bearing: 584 years, and one nanosecond over the line.
            (100_000_000, MAX_IDLE_TIMEOUT),
            (100_000_001, MAX_IDLE_TIMEOUT),
            (u64::MAX, MAX_IDLE_TIMEOUT),
        ] {
            let ring = MockRing::with_info(RingCreateInfo {
                idle_timeout_ns: asked,
                ..base_info()
            });
            assert_eq!(
                pump_on(&ring).idle_timeout(),
                want,
                "an idleTimeout of {asked} ns"
            );
        }
    }

    fn base_info() -> RingCreateInfo {
        RingCreateInfo {
            flags: 0,
            resource_id: 3,
            offset: 0,
            size: RESOURCE,
            idle_timeout_ns: 1_000,
            head_offset: 0,
            tail_offset: 4,
            status_offset: 8,
            buffer_offset: 16,
            buffer_size: u64::from(BUFFER),
            extra_offset: 0,
            extra_size: 0,
        }
    }

    // ------------------------------------------------------- ordinary passes

    #[test]
    fn nothing_produced_means_nothing_copied_and_no_head_store() {
        let ring = MockRing::new();
        let mut pump = pump_on(&ring);
        let mut sink = Sink::everything();
        ring.clear_ops();

        assert_eq!(pump.pump(&*ring, &mut sink), Ok(Pass::Idle));
        assert_eq!(sink.calls, 0);
        assert_eq!(pump.cursor(), 0);
        // One load of `tail`, and nothing else at all: no copy, no store.
        assert_eq!(ring.ops(), vec![Op::LoadGuest(4)]);
        assert_eq!(ring.head(), 0);
    }

    #[test]
    fn a_batch_reaches_the_sink_byte_for_byte_and_head_follows_it() {
        let ring = MockRing::new();
        let mut pump = pump_on(&ring);
        let mut sink = Sink::everything();

        ring.produce(0, b"hello venus");
        ring.set_tail(11);
        ring.clear_ops();

        assert_eq!(
            pump.pump(&*ring, &mut sink),
            Ok(Pass::Progress {
                offered: 11,
                consumed: 11
            })
        );
        assert_eq!(sink.seen, vec![b"hello venus".to_vec()]);
        assert_eq!(pump.cursor(), 11);
        assert_eq!(ring.head(), 11);
        assert_eq!(ring.stores_to(0), vec![11]);

        // A second batch continues from the cursor, and neither cursor nor
        // batch length is a multiple of anything — the protocol promises no
        // alignment of either.
        ring.produce(11, b"xyz");
        ring.set_tail(14);
        assert_eq!(
            pump.pump(&*ring, &mut sink),
            Ok(Pass::Progress {
                offered: 3,
                consumed: 3
            })
        );
        assert_eq!(sink.seen[1], b"xyz".to_vec());
        assert_eq!(ring.head(), 14);
    }

    #[test]
    fn a_partly_consumed_batch_republishes_only_what_was_taken() {
        let ring = MockRing::new();
        let mut pump = pump_on(&ring);
        let mut sink = Sink::take(4);

        ring.produce(0, b"abcdefghij");
        ring.set_tail(10);

        assert_eq!(
            pump.pump(&*ring, &mut sink),
            Ok(Pass::Progress {
                offered: 10,
                consumed: 4
            })
        );
        assert_eq!(ring.head(), 4);
        assert_eq!(pump.cursor(), 4);

        // The remainder is re-offered from the new cursor, not from the start.
        assert_eq!(
            pump.pump(&*ring, &mut sink),
            Ok(Pass::Progress {
                offered: 6,
                consumed: 4
            })
        );
        assert_eq!(sink.seen[1], b"efghij".to_vec());
        assert_eq!(ring.head(), 8);

        assert_eq!(
            pump.pump(&*ring, &mut sink),
            Ok(Pass::Progress {
                offered: 2,
                consumed: 2
            })
        );
        assert_eq!(sink.seen[2], b"ij".to_vec());
        assert_eq!(ring.head(), 10);
        assert_eq!(pump.pump(&*ring, &mut sink), Ok(Pass::Idle));
    }

    // ------------------------------------------------------------- the wrap

    #[test]
    fn a_batch_that_wraps_the_end_of_the_buffer_arrives_spliced_and_in_order() {
        let ring = MockRing::new();
        let mut pump = pump_on(&ring);

        // Walk the cursor to four bytes short of the end.
        let mut warmup = Sink::everything();
        ring.produce(0, &[0u8; 60]);
        ring.set_tail(60);
        assert!(matches!(
            pump.pump(&*ring, &mut warmup),
            Ok(Pass::Progress { .. })
        ));
        assert_eq!(pump.cursor(), 60);

        // Ten bytes from offset 60: four before the seam, six after it.
        let batch: Vec<u8> = (100u8..110).collect();
        ring.produce(60, &batch);
        ring.set_tail(70);
        ring.clear_ops();

        let mut sink = Sink::everything();
        assert_eq!(
            pump.pump(&*ring, &mut sink),
            Ok(Pass::Progress {
                offered: 10,
                consumed: 10
            })
        );
        assert_eq!(sink.seen[0], batch);
        // Two reads, in ring order, the second from the start of the buffer.
        assert_eq!(
            ring.ops()
                .into_iter()
                .filter(|op| matches!(op, Op::ReadBuffer(..)))
                .collect::<Vec<_>>(),
            vec![Op::ReadBuffer(60, 4), Op::ReadBuffer(0, 6)]
        );
        assert_eq!(ring.head(), 70);
    }

    #[test]
    fn a_batch_exactly_the_length_of_the_buffer_is_legal_wrapped_or_not() {
        // Flush against the start: one contiguous read of the whole buffer.
        let ring = MockRing::new();
        let mut pump = pump_on(&ring);
        let full: Vec<u8> = (0..BUFFER).map(|i| i as u8).collect();
        ring.produce(0, &full);
        ring.set_tail(BUFFER);
        ring.clear_ops();

        let mut sink = Sink::everything();
        assert_eq!(
            pump.pump(&*ring, &mut sink),
            Ok(Pass::Progress {
                offered: BUFFER,
                consumed: BUFFER
            })
        );
        assert_eq!(sink.seen[0], full);
        assert_eq!(
            ring.ops()
                .into_iter()
                .filter(|op| matches!(op, Op::ReadBuffer(..)))
                .collect::<Vec<_>>(),
            vec![Op::ReadBuffer(0, BUFFER as usize)]
        );

        // And a bufferful straddling the seam: both halves, still exactly the
        // buffer's length, still accepted.
        let straddling: Vec<u8> = (0..BUFFER).map(|i| (i as u8).wrapping_add(200)).collect();
        ring.produce(BUFFER, &straddling);
        // The cursor is at 64, so 64 bytes puts the tail at 128 and the first
        // read starts at buffer offset 0 again — move the cursor off the seam
        // first so the wrap is genuinely uneven.
        ring.set_tail(BUFFER + 7);
        assert!(matches!(
            pump.pump(&*ring, &mut Sink::everything()),
            Ok(Pass::Progress { .. })
        ));
        assert_eq!(pump.cursor(), BUFFER + 7);

        let uneven: Vec<u8> = (0..BUFFER).map(|i| (i as u8).wrapping_add(50)).collect();
        ring.produce(BUFFER + 7, &uneven);
        ring.set_tail(BUFFER + 7 + BUFFER);
        ring.clear_ops();
        let mut sink = Sink::everything();
        assert_eq!(
            pump.pump(&*ring, &mut sink),
            Ok(Pass::Progress {
                offered: BUFFER,
                consumed: BUFFER
            })
        );
        assert_eq!(sink.seen[0], uneven);
        assert_eq!(
            ring.ops()
                .into_iter()
                .filter(|op| matches!(op, Op::ReadBuffer(..)))
                .collect::<Vec<_>>(),
            vec![Op::ReadBuffer(7, 57), Op::ReadBuffer(0, 7)]
        );
    }

    #[test]
    fn the_cursor_wraps_at_the_top_of_a_u32_without_losing_a_byte() {
        let ring = MockRing::new();
        let mut pump = pump_on(&ring);
        // Reach into the pump rather than consuming four billion bytes to get
        // here. `cur` is a free-running u32 and the guest can run it all the
        // way round; the masking and the `wrapping_sub` have to survive it.
        pump.cur = u32::MAX - 3;

        let batch = b"ABCDEFGH";
        ring.produce(u32::MAX - 3, batch);
        ring.set_tail(4); // (u32::MAX - 3) + 8, wrapped.
        ring.clear_ops();

        let mut sink = Sink::everything();
        assert_eq!(
            pump.pump(&*ring, &mut sink),
            Ok(Pass::Progress {
                offered: 8,
                consumed: 8
            })
        );
        assert_eq!(sink.seen[0], batch.to_vec());
        assert_eq!(pump.cursor(), 4);
        assert_eq!(ring.head(), 4);
        // (u32::MAX - 3) & 63 == 60, so this batch straddles the seam too.
        assert_eq!(
            ring.ops()
                .into_iter()
                .filter(|op| matches!(op, Op::ReadBuffer(..)))
                .collect::<Vec<_>>(),
            vec![Op::ReadBuffer(60, 4), Op::ReadBuffer(0, 4)]
        );
    }

    // ------------------------------------------------------- hostile tails

    #[test]
    fn a_tail_claiming_more_than_the_ring_holds_is_fatal() {
        for tail in [
            BUFFER + 1,
            BUFFER + 2,
            1 << 20,
            u32::MAX / 2,
            u32::MAX - BUFFER,
        ] {
            let ring = MockRing::new();
            let mut pump = pump_on(&ring);
            ring.set_tail(tail);
            let mut sink = Sink::everything();

            assert_eq!(
                pump.pump(&*ring, &mut sink),
                Err(PumpError::TailOutOfRange {
                    cur: 0,
                    tail,
                    claimed: tail,
                    buffer_size: BUFFER,
                }),
                "a tail of {tail:#x}"
            );
            assert_eq!(sink.calls, 0);
            assert!(pump.is_fatal());
            // The guest is told, so its driver aborts instead of waiting on a
            // head that will never move again.
            assert_eq!(ring.status() & STATUS_FATAL, STATUS_FATAL);
            assert_eq!(ring.head(), 0);
            // And the ring stays dead.
            assert_eq!(pump.pump(&*ring, &mut sink), Err(PumpError::Fatal));
        }
    }

    #[test]
    fn a_tail_that_moves_backwards_is_refused_as_an_impossible_claim() {
        let ring = MockRing::new();
        let mut pump = pump_on(&ring);
        ring.produce(0, &[7u8; 32]);
        ring.set_tail(32);
        assert!(matches!(
            pump.pump(&*ring, &mut Sink::everything()),
            Ok(Pass::Progress { .. })
        ));
        assert_eq!(pump.cursor(), 32);

        // Every backwards step lands as a gigantic wrapping claim, which is
        // the one condition that catches it.
        for back in [1u32, 2, 31, 32] {
            let ring = MockRing::new();
            let mut pump = pump_on(&ring);
            pump.cur = 32;
            ring.set_tail(32 - back);
            let err = pump
                .pump(&*ring, &mut Sink::everything())
                .expect_err("a backwards tail is refused");
            assert!(
                matches!(err, PumpError::TailOutOfRange { claimed, .. } if claimed == back.wrapping_neg()),
                "tail moved back {back}: {err}"
            );
        }
    }

    #[test]
    fn a_backwards_step_of_almost_four_billion_is_a_legal_forward_step() {
        // The protocol is wrapping arithmetic and nothing else: a tail that
        // looks like a huge backwards jump is indistinguishable from — and
        // therefore *is* — a small forward one. Answering "legal" here is
        // correct, not lenient, and a validator that tried to be cleverer
        // would reject the ordinary wrap in the test above.
        let ring = MockRing::new();
        let mut pump = pump_on(&ring);
        pump.cur = 10;
        ring.produce(10, b"ok");
        ring.set_tail(12);
        assert_eq!(
            pump.pump(&*ring, &mut Sink::everything()),
            Ok(Pass::Progress {
                offered: 2,
                consumed: 2
            })
        );
    }

    // ------------------------------------------------ the guest races the copy

    #[test]
    fn the_sink_sees_a_snapshot_the_guest_cannot_touch() {
        let ring = MockRing::new();
        let mut pump = pump_on(&ring);
        ring.produce(0, b"original");
        ring.set_tail(8);

        // The guest overwrites the ring the instant the host reads it — and
        // then again from inside the sink, while the sink is holding the
        // bytes. Neither is allowed to be visible.
        ring.inner.borrow_mut().rewrite_after_read = Some((0, b"TAMPERED".to_vec()));

        let observed = Rc::clone(&ring);
        let mut before = Vec::new();
        let mut after = Vec::new();
        {
            let mut sink = Sink::new(|bytes| {
                before = bytes.to_vec();
                // Scribble over every byte of the command buffer from under
                // the sink's feet.
                observed.produce(0, &vec![0xffu8; BUFFER as usize]);
                after = bytes.to_vec();
                bytes.len()
            });
            assert_eq!(
                pump.pump(&*ring, &mut sink),
                Ok(Pass::Progress {
                    offered: 8,
                    consumed: 8
                })
            );
        }
        // The copy is private: it did not change under the sink...
        assert_eq!(before, after);
        // ...and it is the snapshot taken at copy time, not what the ring held
        // a moment later.
        assert_eq!(before, b"original".to_vec());
    }

    #[test]
    fn a_tail_that_grows_during_the_copy_is_left_for_the_next_pass() {
        let ring = MockRing::new();
        let mut pump = pump_on(&ring);
        ring.produce(0, b"first");
        ring.set_tail(5);
        // Every buffer read pushes the tail on by three: a guest producing as
        // fast as we consume. The batch must still be the five bytes the
        // single `tail` load promised — those are the only bytes the acquire
        // load ordered.
        ring.inner.borrow_mut().bump_tail_after_read = 3;

        let mut sink = Sink::everything();
        assert_eq!(
            pump.pump(&*ring, &mut sink),
            Ok(Pass::Progress {
                offered: 5,
                consumed: 5
            })
        );
        assert_eq!(sink.seen[0], b"first".to_vec());
        assert_eq!(pump.cursor(), 5);

        // Nothing is lost: the bytes the guest added mid-copy are simply the
        // next pass's batch. (The tail is now 8 from the first read, plus 3
        // more for the read this pass performs.)
        ring.produce(5, b"abc");
        ring.inner.borrow_mut().bump_tail_after_read = 0;
        ring.set_tail(8);
        assert_eq!(
            pump.pump(&*ring, &mut sink),
            Ok(Pass::Progress {
                offered: 3,
                consumed: 3
            })
        );
        assert_eq!(sink.seen[1], b"abc".to_vec());
    }

    #[test]
    fn declined_bytes_are_re_read_and_may_have_changed() {
        // A consequence of promise 3 in the module docs, asserted rather than
        // left implied: a sink that declines must not assume the bytes it saw
        // will still be there. A correct guest never rewrites `[head, tail)`;
        // this one is not correct.
        let ring = MockRing::new();
        let mut pump = pump_on(&ring);
        ring.produce(0, b"aaaabbbb");
        ring.set_tail(8);

        let mut sink = Sink::take(4);
        assert_eq!(
            pump.pump(&*ring, &mut sink),
            Ok(Pass::Progress {
                offered: 8,
                consumed: 4
            })
        );
        assert_eq!(sink.seen[0], b"aaaabbbb".to_vec());

        ring.produce(4, b"ZZZZ");
        assert_eq!(
            pump.pump(&*ring, &mut sink),
            Ok(Pass::Progress {
                offered: 4,
                consumed: 4
            })
        );
        assert_eq!(sink.seen[1], b"ZZZZ".to_vec());
    }

    // ----------------------------------------------------- the stalling sink

    #[test]
    fn a_sink_that_takes_nothing_is_not_asked_again_until_the_guest_moves() {
        let ring = MockRing::new();
        let mut pump = pump_on(&ring);
        ring.produce(0, b"half a command");
        ring.set_tail(14);

        let mut sink = Sink::nothing();
        assert_eq!(
            pump.pump(&*ring, &mut sink),
            Ok(Pass::Stalled { offered: 14 })
        );
        assert_eq!(sink.calls, 1);
        assert_eq!(ring.head(), 0);

        ring.clear_ops();
        for _ in 0..10 {
            assert_eq!(
                pump.pump(&*ring, &mut sink),
                Ok(Pass::Stalled { offered: 14 })
            );
        }
        // Ten more passes, and the sink was never troubled again — and neither
        // was the shadow buffer.
        assert_eq!(sink.calls, 1);
        assert!(!ring.ops().iter().any(|op| matches!(op, Op::ReadBuffer(..))));

        // One more byte from the guest and the whole batch is offered afresh.
        ring.produce(14, b"!");
        ring.set_tail(15);
        let mut hungry = Sink::everything();
        assert_eq!(
            pump.pump(&*ring, &mut hungry),
            Ok(Pass::Progress {
                offered: 15,
                consumed: 15
            })
        );
        assert_eq!(hungry.seen[0], b"half a command!".to_vec());
    }

    #[test]
    fn a_full_ring_the_sink_refuses_is_reported_as_the_dead_end_it_is() {
        let ring = MockRing::new();
        let mut pump = pump_on(&ring);
        ring.produce(0, &vec![0xaau8; BUFFER as usize]);
        ring.set_tail(BUFFER);

        let mut sink = Sink::nothing();
        // The guest cannot produce past `head + size` and `head` only moves
        // when the sink consumes, so this can never resolve itself.
        assert_eq!(
            pump.pump(&*ring, &mut sink),
            Ok(Pass::Deadlocked { offered: BUFFER })
        );
        assert_eq!(
            pump.pump(&*ring, &mut sink),
            Ok(Pass::Deadlocked { offered: BUFFER })
        );
        assert_eq!(sink.calls, 1);

        // The policy is the driver's; the usual one is to stop pretending.
        pump.mark_fatal(&*ring);
        assert_eq!(ring.status() & STATUS_FATAL, STATUS_FATAL);
        assert_eq!(pump.pump(&*ring, &mut sink), Err(PumpError::Fatal));
    }

    #[test]
    fn a_sink_cannot_claim_more_than_it_was_given() {
        // `Consumed` is only constructible from the `Batch` that was offered
        // and clamps on the way, so the cursor cannot be walked past the
        // guest's tail even by a sink that tries.
        let ring = MockRing::new();
        let mut pump = pump_on(&ring);
        ring.produce(0, b"four");
        ring.set_tail(4);

        let mut greedy = Sink::new(|_| usize::MAX);
        assert_eq!(
            pump.pump(&*ring, &mut greedy),
            Ok(Pass::Progress {
                offered: 4,
                consumed: 4
            })
        );
        assert_eq!(pump.cursor(), 4);
        assert_eq!(ring.head(), 4);
        assert_eq!(pump.pump(&*ring, &mut greedy), Ok(Pass::Idle));
    }

    // ------------------------------------------------------------ going idle

    #[test]
    fn the_idle_bit_goes_up_before_the_tail_is_re_read() {
        let ring = MockRing::new();
        let mut pump = pump_on(&ring);
        ring.clear_ops();

        assert_eq!(pump.enter_idle(&*ring), Idle::Park);
        // The ordering is the whole protocol: publish, *then* look again.
        // Reversed, the guest reads a not-yet-idle status, skips the doorbell,
        // stores its tail, and the host parks on work that is already there.
        assert_eq!(
            ring.ops(),
            vec![Op::StoreHost(8, STATUS_IDLE), Op::LoadGuest(4)]
        );
        assert!(pump.is_idle());
        assert_eq!(ring.status(), STATUS_IDLE);

        pump.leave_idle(&*ring);
        assert!(!pump.is_idle());
        assert_eq!(ring.status(), 0);
        // Idempotent: a second wake does not store again.
        ring.clear_ops();
        pump.leave_idle(&*ring);
        assert!(ring.stores_to(8).is_empty());
    }

    #[test]
    fn work_that_lands_between_the_last_pass_and_the_idle_bit_is_not_lost() {
        let ring = MockRing::new();
        let mut pump = pump_on(&ring);
        assert_eq!(pump.pump(&*ring, &mut Sink::everything()), Ok(Pass::Idle));

        // The guest stores its tail here — after the host decided it was out
        // of work, before the host parked.
        ring.produce(0, b"late");
        ring.set_tail(4);
        ring.clear_ops();

        assert_eq!(pump.enter_idle(&*ring), Idle::WorkArrived);
        // Idle went up and came straight back down, and the driver must not
        // block.
        assert_eq!(ring.stores_to(8), vec![STATUS_IDLE, 0]);
        assert!(!pump.is_idle());
        assert_eq!(ring.status(), 0);

        let mut sink = Sink::everything();
        assert_eq!(
            pump.pump(&*ring, &mut sink),
            Ok(Pass::Progress {
                offered: 4,
                consumed: 4
            })
        );
        assert_eq!(sink.seen[0], b"late".to_vec());
    }

    #[test]
    fn bytes_the_sink_already_refused_do_not_keep_the_pump_awake() {
        // The reference parks only when `cur == tail`, which with a sink that
        // can stall would spin forever. Ours parks when nothing has changed
        // since the refusal — and still wakes for the first new byte.
        let ring = MockRing::new();
        let mut pump = pump_on(&ring);
        ring.produce(0, b"partial");
        ring.set_tail(7);

        let mut sink = Sink::nothing();
        assert_eq!(
            pump.pump(&*ring, &mut sink),
            Ok(Pass::Stalled { offered: 7 })
        );
        assert_eq!(pump.enter_idle(&*ring), Idle::Park);
        assert!(pump.is_idle());

        // The doorbell rings with one more byte, and the pump wakes properly.
        pump.leave_idle(&*ring);
        ring.produce(7, b"!");
        ring.set_tail(8);
        assert_eq!(pump.enter_idle(&*ring), Idle::WorkArrived);
        assert!(!pump.is_idle());
    }

    #[test]
    fn idle_and_fatal_are_separate_bits_on_one_word() {
        let ring = MockRing::new();
        let mut pump = pump_on(&ring);
        assert_eq!(pump.enter_idle(&*ring), Idle::Park);
        pump.mark_fatal(&*ring);
        assert_eq!(pump.status(), STATUS_IDLE | STATUS_FATAL);
        assert_eq!(ring.status(), STATUS_IDLE | STATUS_FATAL);
        // And `STATUS_ALIVE` is nobody's business here: the ring never sets it.
        assert_eq!(pump.status() & STATUS_ALIVE, 0);
    }

    // ----------------------------------------------------------------- reset

    #[test]
    fn reset_returns_the_ring_to_power_on_in_shared_memory_too() {
        let ring = MockRing::new();
        let mut pump = pump_on(&ring);
        ring.produce(0, b"work");
        ring.set_tail(4);
        assert!(matches!(
            pump.pump(&*ring, &mut Sink::everything()),
            Ok(Pass::Progress { .. })
        ));
        pump.mark_fatal(&*ring);
        assert_eq!(ring.head(), 4);
        assert_ne!(ring.status(), 0);

        pump.reset(&*ring);
        assert_eq!(pump.cursor(), 0);
        assert_eq!(pump.status(), 0);
        assert!(!pump.is_fatal());
        // The words the host owns are *stored*, not merely forgotten: a stale
        // head left in shared memory across a reboot is the haunting ADR-0005
        // is about.
        assert_eq!(ring.head(), 0);
        assert_eq!(ring.status(), 0);
    }

    // ------------------------------------------------------ a scripted guest

    /// A whole session against an independent model of the byte stream.
    ///
    /// The "guest" produces a known infinite sequence — byte `n` of the stream
    /// is `f(n)` — in randomly sized chunks, waiting for ring space like a real
    /// one; the sink accepts randomly sized prefixes. The model tracks only how
    /// many bytes have been delivered, and checks that every byte the sink ever
    /// sees is the next one in the stream. Nothing about wrapping, splitting or
    /// republishing is modelled, which is what makes disagreement meaningful.
    #[test]
    fn a_scripted_guest_and_sink_deliver_the_stream_in_order_without_a_gap() {
        for buffer_size in [4u32, 16, 64, 256] {
            let ring = MockRing::with_buffer_size(buffer_size);
            let mut pump = pump_on(&ring);

            // Deterministic, so a failure is reproducible.
            let mut seed = 0x1234_5678u32 ^ buffer_size;
            let mut next = move || {
                seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                seed >> 16
            };

            let stream = |n: u32| (n.wrapping_mul(31).wrapping_add(7) & 0xff) as u8;

            let mut produced = 0u32;
            let mut delivered = 0u32;

            for _ in 0..4_000 {
                // The guest tops the ring up, never past `head + size`.
                let room = buffer_size - produced.wrapping_sub(pump.cursor());
                let chunk = if room == 0 { 0 } else { next() % (room + 1) };
                for i in 0..chunk {
                    let at = produced.wrapping_add(i);
                    ring.produce(at, &[stream(at)]);
                }
                produced = produced.wrapping_add(chunk);
                ring.set_tail(produced);

                // The sink takes a random prefix — sometimes nothing at all.
                let appetite = next() as usize;
                let expect_from = delivered;
                let mut taken = 0usize;
                let mut ok = true;
                let pass;
                {
                    let mut sink = Sink::new(|bytes: &[u8]| {
                        for (i, byte) in bytes.iter().enumerate() {
                            let want =
                                stream(expect_from.wrapping_add(u32::try_from(i).unwrap_or(0)));
                            ok &= *byte == want;
                        }
                        taken = appetite.min(bytes.len());
                        taken
                    });
                    pass = pump
                        .pump(&*ring, &mut sink)
                        .expect("a well-behaved guest is never refused");
                }
                match pass {
                    Pass::Progress { consumed, .. } => {
                        assert_eq!(usize::try_from(consumed).unwrap_or(0), taken);
                    }
                    Pass::Idle => assert_eq!(produced, delivered),
                    Pass::Stalled { .. } | Pass::Deadlocked { .. } => assert_eq!(taken, 0),
                }
                assert!(ok, "the sink was handed a byte out of stream order");
                delivered = delivered.wrapping_add(u32::try_from(taken).unwrap_or(0));
                assert_eq!(pump.cursor(), delivered);
                assert_eq!(ring.head(), delivered);
            }

            // The sweep has to have done real work, including wrapping the
            // buffer many times over, or it proves nothing.
            assert!(
                delivered > buffer_size * 20,
                "{buffer_size}-byte ring only delivered {delivered} bytes"
            );
        }
    }
}
