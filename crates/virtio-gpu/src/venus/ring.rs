//! The Venus command ring's **layout**: five byte ranges a guest proposes
//! inside one shared-memory resource, and the validation that turns them into
//! something the host may index (EPIC 20, ADR-0004).
//!
//! A guest's mesa `venus` driver sets up its ring by sending one
//! `VkRingCreateInfoMESA` — a resource id, a byte range inside that resource,
//! and *seven more offsets naming the pieces inside the range*. Every one of
//! those numbers is guest-chosen, 64-bit, and arrives before the host has
//! agreed to anything. Everything the guest ever sends afterwards arrives
//! through a ring whose shape it proposed here, which makes this the densest
//! security surface in the protocol.
//!
//! So this module has exactly one job: take those numbers plus the size of the
//! resource they claim to live in, and either refuse them with a named reason
//! ([`RingLayoutError`]) or hand back a [`RingLayout`]. The type has private
//! fields and one fallible constructor, so **holding a `RingLayout` is proof
//! the ranges inside it are in bounds, disjoint, aligned and indexable** — a
//! consumer never re-derives any of it, and there is nothing to forget.
//!
//! What is deliberately *not* here: the ring protocol itself. Head/tail
//! progress, batch consumption, wrapping the buffer, the idle timeout's
//! meaning — all of that is built on top of this type, and none of it can
//! start until the layout is known good.
//!
//! # The five regions
//!
//! | region | length | written by | purpose |
//! |---|---|---|---|
//! | `head` | 4 | **host** | how far the host has consumed |
//! | `tail` | 4 | **guest** | how far the guest has produced |
//! | `status` | 4 | **host** | idle/alive word the guest polls |
//! | `buffer` | `bufferSize` | guest | the command bytes |
//! | `extra` | `extraSize` | host | scratch the host stores into for the guest to poll |
//!
//! Who may *store* to each control word is encoded in the API rather than in
//! a comment: [`RingLayout::head`] and [`RingLayout::status`] return a
//! [`HostWord`], which has [`ControlWord::store_offset`]; [`RingLayout::tail`]
//! returns a [`GuestWord`], which does not. A future writer path takes a
//! `&ControlWord<HostWrites>` and the compiler refuses the tail word at the
//! call site, where the mistake is cheap, instead of in review.
//!
//! # The rules
//!
//! The reference is virglrenderer 1.1.0, `vkr_ring_layout_init` in
//! `vkr_transport.c` (~lines 110-175), which puts all five regions in one
//! array and runs one loop over them:
//!
//! * the ring region lies inside the resource, and every one of the five
//!   regions lies wholly inside the ring region — so a control word near the
//!   end is refused for straddling it rather than silently clipped;
//! * **every region is four-byte aligned at both ends**, not just the three
//!   control words (`vkr_region_is_aligned` tests `begin | end`). The one that
//!   earns its keep is `extra`: the host stores a `u32` there for the guest to
//!   poll, so a misaligned `extra` is a misaligned atomic, exactly the hazard
//!   the control words are aligned against. The buffer's end-alignment follows
//!   from a power-of-two size of at least four, but it is *checked* rather than
//!   reasoned about, because the reasoning is one edit away from being wrong;
//! * all five regions are pairwise disjoint;
//! * `bufferSize` is a non-zero power of two, at most [`MAX_BUFFER_BYTES`]
//!   (`util_is_power_of_two_nonzero`, so zero is the reference's refusal too,
//!   not an addition of ours);
//! * `head`, `tail` and `status` are four bytes each.
//!
//! # What this implementation adds
//!
//! Not claims about what the reference omits — claims about what this file
//! does, under the workspace rule that a guest value never reaches host memory
//! unchecked:
//!
//! * no arithmetic on a guest value may wrap. Every addition on the way to a
//!   bounds decision is checked, and an offset that would not fit a host
//!   `usize` is refused by name rather than truncated later.
//! * a zero-sized ring region is refused by name. Nothing could be placed in
//!   one anyway — the head word alone would not fit — but a named refusal
//!   beats an incidental one.
//! * **alignment is judged on the offset within the resource, not within the
//!   ring.** `offset` is guest-chosen as well, so a perfectly 4-aligned
//!   `headOffset` inside a ring that starts on an odd byte still lands on a
//!   misaligned host word. Resources are page-aligned where the host maps
//!   them, so resource-relative alignment is host alignment. This matches the
//!   reference rather than adding to it — `vkr_ring_layout_init` builds every
//!   region as `VKR_REGION_INIT(info->offset + info->xOffset, ...)`, i.e.
//!   absolute, and aligns that — but it is listed here because the rule as it
//!   is usually *written down* says only "4-byte aligned", which is not enough
//!   to implement from.
//! * a zero-sized region owns no bytes and therefore overlaps nothing,
//!   wherever the guest put it (see [`Region::overlaps`]). This is a real
//!   divergence: the reference's endpoint test calls an empty `extra` at the
//!   start of the command buffer disjoint and the identical one a byte further
//!   in overlapping, while its own comment says `region->size == 0` is valid.
//!   Nothing can come of either answer — an empty `extra` is reported as
//!   [`None`](RingLayout::extra) and its offset is discarded.
//!
//! Nothing here allocates, reads guest memory or touches a renderer, so the
//! whole surface is testable on every host, including one with no GPU at all.

use std::fmt;
use std::marker::PhantomData;
use std::ops::Range;

use thiserror::Error;

/// Width of each of the three control words. Fixed by the protocol: they are
/// lock-free 32-bit atomics shared with the guest.
pub const CONTROL_WORD_LEN: u64 = 4;

/// Required alignment of **every** region, at both ends, in bytes.
///
/// It is the control words that make it necessary — a 32-bit atomic straddling
/// a 4-byte boundary cannot be loaded or stored atomically on x86-64, and the
/// guest is reading the same address from its side — but the same hazard
/// reaches `extra`, where the host stores a word for the guest to poll, so the
/// reference applies it to all five regions and so does this. The buffer's end
/// alignment already follows from a power-of-two size of at least four; it is
/// checked rather than reasoned about.
pub const REGION_ALIGN: u64 = 4;

/// Largest command buffer a ring may declare, matching virglrenderer's
/// `VKR_RING_BUFFER_MAX_SIZE`. The buffer is indexed modulo its size, which is
/// why it is also required to be a power of two.
pub const MAX_BUFFER_BYTES: u64 = 16 << 20;

/// Which of the five ranges a refusal is about. Carried by every placement
/// error so a debugging session reads the *name* of the offending offset
/// rather than counting fields in the create info.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum RingRegion {
    /// The host-written consumer index.
    Head,
    /// The guest-written producer index.
    Tail,
    /// The host-written status word the guest polls.
    Status,
    /// The command bytes.
    Buffer,
    /// Host scratch the guest polls; optional.
    Extra,
}

impl RingRegion {
    /// The name this region has in `VkRingCreateInfoMESA`, minus the `Offset`
    /// suffix.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Head => "head",
            Self::Tail => "tail",
            Self::Status => "status",
            Self::Buffer => "buffer",
            Self::Extra => "extra",
        }
    }
}

impl fmt::Display for RingRegion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// Why a proposed ring layout was refused.
///
/// Every variant names one distinct thing the guest got wrong, with the values
/// that made it wrong, because these messages are what a future debugging
/// session reads when a guest's Vulkan driver silently fails to start. None of
/// them is a host error: a bad layout fails the `vkCreateRingMESA` that carried
/// it and the device carries on.
///
/// Every variant is about the *values*, never about the bytes they came in:
/// a truncated or malformed encoding is refused before this type is reached,
/// by [`wire`](super::wire), and has its own vocabulary there. One failure,
/// one name for it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum RingLayoutError {
    #[error("the ring region is zero-sized")]
    ZeroSizedRing,

    #[error("the ring region at {offset:#x} plus its size {size:#x} overflows a 64-bit offset")]
    RingRangeOverflows { offset: u64, size: u64 },

    #[error(
        "the ring region {offset:#x}..+{size:#x} does not fit inside the \
         {resource_size:#x}-byte resource"
    )]
    RingOutsideResource {
        offset: u64,
        size: u64,
        resource_size: u64,
    },

    #[error(
        "the ring region ends at {end:#x}, past the largest offset a host \
         pointer on this machine can name"
    )]
    RingBeyondHostAddressSpace { end: u64 },

    #[error(
        "the {region} region at ring offset {offset:#x} plus its length \
         {len:#x} overflows a 64-bit offset"
    )]
    RegionRangeOverflows {
        region: RingRegion,
        offset: u64,
        len: u64,
    },

    #[error(
        "the {region} region {offset:#x}..+{len:#x} does not fit inside the \
         {ring_size:#x}-byte ring region"
    )]
    RegionOutsideRing {
        region: RingRegion,
        offset: u64,
        len: u64,
        ring_size: u64,
    },

    #[error(
        "the {region} region at ring offset {ring_offset:#x} lands at resource \
         offsets {resource_start:#x}..{resource_end:#x}, and both ends must be \
         {align}-byte aligned",
        align = REGION_ALIGN
    )]
    MisalignedRegion {
        region: RingRegion,
        ring_offset: u64,
        resource_start: u64,
        resource_end: u64,
    },

    #[error(
        "the {first} region {first_at:#x}..+{first_len:#x} overlaps the \
         {second} region {second_at:#x}..+{second_len:#x}"
    )]
    RegionsOverlap {
        first: RingRegion,
        first_at: u64,
        first_len: u64,
        second: RingRegion,
        second_at: u64,
        second_len: u64,
    },

    #[error("the command buffer is zero-sized")]
    ZeroSizedBuffer,

    #[error("the command buffer size {size:#x} is not a power of two")]
    BufferSizeNotPowerOfTwo { size: u64 },

    #[error("the command buffer size {size:#x} exceeds the {max:#x}-byte limit")]
    BufferTooLarge { size: u64, max: u64 },
}

/// The raw, **unvalidated** `VkRingCreateInfoMESA` a guest sent.
///
/// This is guest input and nothing more: constructing one asserts nothing, and
/// no field here may be used to index anything. It exists so that decoding the
/// wire bytes and judging them are separate steps, owned by separate modules:
/// [`transport`](super::transport) walks the `sType` and the pNext chain and
/// fills one of these in, and the only thing that may then be done with it is
/// [`RingLayout::new`].
///
/// Field names follow the Mesa struct (`headOffset` → `head_offset`) and are
/// declared in wire order; all the `*_offset` values are relative to the start
/// of the ring region, which is itself at `offset` within the resource.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct RingCreateInfo {
    /// `VkRingCreateInfoMESA::flags`, carried **unvalidated**.
    ///
    /// The house rule elsewhere in this crate is that unknown flag bits are
    /// refused rather than ignored, because ignoring them hands the guest a
    /// resource that silently cannot do what it asked for. That rule needs a
    /// set of known bits to compare against, and this field has none defined
    /// in venus-protocol at all — so there is nothing here to judge, and a
    /// refusal would be inventing a rule rather than enforcing one. It is
    /// carried through to [`RingLayout::flags`] so that whoever gains a
    /// meaning for a bit can refuse the rest at that point, with something to
    /// refuse them against.
    pub flags: u32,
    /// The shared-memory resource the ring lives in.
    pub resource_id: u32,
    /// Where the ring region starts inside that resource.
    pub offset: u64,
    /// How many bytes of the resource the ring region occupies.
    pub size: u64,
    /// How long the host should keep polling before parking, in nanoseconds.
    /// A hint, not a bound: see [`RingLayout::idle_timeout_ns`].
    pub idle_timeout_ns: u64,
    /// Ring-relative offset of the 4-byte head word.
    pub head_offset: u64,
    /// Ring-relative offset of the 4-byte tail word.
    pub tail_offset: u64,
    /// Ring-relative offset of the 4-byte status word.
    pub status_offset: u64,
    /// Ring-relative offset of the command buffer.
    pub buffer_offset: u64,
    /// Length of the command buffer.
    pub buffer_size: u64,
    /// Ring-relative offset of the host scratch region.
    pub extra_offset: u64,
    /// Length of the host scratch region; zero means there is none.
    pub extra_size: u64,
}

impl RingCreateInfo {
    /// Bytes the fixed body occupies on the wire: two `uint32_t`s (`flags`,
    /// then `resourceId`) and ten 64-bit fields (`size_t` is 8 bytes on both
    /// ends of this protocol, and `idleTimeout` is a `uint64_t`), with no
    /// padding — the Venus command stream is 4-byte granular, so the 64-bit
    /// fields are not 8-byte aligned in it.
    ///
    /// The order is `vn_decode_VkRingCreateInfoMESA_self_temp`'s
    /// (`venus-protocol/vn_protocol_renderer_transport.h:208-223`).
    ///
    /// **Nothing in this file decodes with it.** It is the *size* half of the
    /// struct's description, kept beside the fields it describes so the two
    /// cannot drift, and it exists for
    /// [`transport::RING_CREATE_INFO_MIN_WIRE_LEN`](super::transport::RING_CREATE_INFO_MIN_WIRE_LEN),
    /// which adds the `sType` and the empty-chain marker to it to get the
    /// least a command carrying one of these can possibly be. Reading the
    /// bytes is [`transport`](super::transport)'s job, through
    /// [`wire::Decoder`](super::wire::Decoder): a bare slice could not do it
    /// anyway, since the body is preceded by an arbitrary-length pNext chain
    /// that has to be walked before anyone knows where it starts.
    pub const WIRE_LEN: usize = 2 * 4 + 10 * 8;
}

/// A validated byte range **inside the shared-memory resource**.
///
/// `start` is an offset from the start of the resource, not from the start of
/// the ring region: that is what a consumer indexes its mapping of the
/// resource with, and every ring-relative value was already folded in and
/// checked on the way here. A `Region` can only be produced by
/// [`RingLayout::new`], and every one it produces satisfies
/// `start + len <= resource_size` and `start + len <= usize::MAX`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Region {
    start: u64,
    len: u64,
}

impl Region {
    /// Offset of the first byte, from the start of the resource.
    #[must_use]
    pub fn start(&self) -> u64 {
        self.start
    }

    /// Length in bytes.
    #[must_use]
    pub fn len(&self) -> u64 {
        self.len
    }

    /// `true` when the region has no bytes. Only ever true for `extra`, and
    /// then [`RingLayout::extra`] reports it as absent instead.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// One past the last byte, from the start of the resource. Cannot
    /// overflow: the constructor refused any layout where it would.
    #[must_use]
    pub fn end(&self) -> u64 {
        self.start.saturating_add(self.len)
    }

    /// The region as a host slice index range, for indexing a mapping of the
    /// whole resource. Infallible because the constructor refused any layout
    /// whose end does not fit a `usize`.
    #[must_use]
    pub fn range(&self) -> Range<usize> {
        let start = usize::try_from(self.start).unwrap_or(usize::MAX);
        let len = usize::try_from(self.len).unwrap_or(0);
        start..start.saturating_add(len)
    }

    /// Whether `len` bytes at `offset` *within this region* stay inside it.
    /// The arithmetic is checked, so a guest-supplied `offset`/`len` pair that
    /// would wrap answers `false` rather than aliasing the start of the
    /// region.
    #[must_use]
    pub fn contains_range(&self, offset: u64, len: u64) -> bool {
        offset
            .checked_add(len)
            .is_some_and(|end_within| end_within <= self.len)
    }

    /// Whether two regions share a byte.
    ///
    /// An empty region owns no bytes, so it overlaps nothing — including
    /// itself, and including a region it sits in the middle of. The plain
    /// half-open test is not enough for that last case (`8..24` against
    /// `16..16` satisfies it), and the reference's interval test has the same
    /// hole in reverse: it calls an empty `extra` at the *start* of the buffer
    /// disjoint and the same empty `extra` one byte further in overlapping.
    /// Neither owns a byte, so neither can conflict.
    #[must_use]
    pub fn overlaps(&self, other: &Self) -> bool {
        !self.is_empty()
            && !other.is_empty()
            && self.start < other.end()
            && other.start < self.end()
    }
}

mod sealed {
    /// Keeps [`super::WordWriter`] closed: the protocol has exactly two sides
    /// and a third marker would be meaningless.
    pub trait Sealed {}
}

/// Which side of the ring may store to a control word.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Writer {
    /// We may store to it; the guest only loads it.
    Host,
    /// The guest stores to it; we may only load it.
    Guest,
}

/// Marker trait for the two sides. Sealed.
pub trait WordWriter: sealed::Sealed {
    /// The side this marker names.
    const WRITER: Writer;
}

/// Marker: **the host** stores to this word, the guest only loads it. Uninhabited.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostWrites {}

/// Marker: **the guest** stores to this word, the host only loads it. Uninhabited.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GuestWrites {}

impl sealed::Sealed for HostWrites {}
impl WordWriter for HostWrites {
    const WRITER: Writer = Writer::Host;
}

impl sealed::Sealed for GuestWrites {}
impl WordWriter for GuestWrites {
    const WRITER: Writer = Writer::Guest;
}

/// One validated 4-byte control word, tagged with the side that owns writes to
/// it.
///
/// Loading is symmetric — the host reads all three words — so
/// [`offset`](Self::offset) exists for both. Storing is not, so
/// [`store_offset`](ControlWord::store_offset) exists only on
/// `ControlWord<HostWrites>`. The consuming side's writer takes a
/// `&ControlWord<HostWrites>`, and "the host must never write the tail" stops
/// being a comment somebody has to have read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ControlWord<W: WordWriter> {
    start: u64,
    _writer: PhantomData<W>,
}

/// A control word the host owns writes to: `head` and `status`.
pub type HostWord = ControlWord<HostWrites>;

/// A control word the guest owns writes to: `tail`.
pub type GuestWord = ControlWord<GuestWrites>;

impl<W: WordWriter> ControlWord<W> {
    /// Offset of the word from the start of the resource. Four-byte aligned
    /// and wholly inside the ring region, both established at construction.
    #[must_use]
    pub fn offset(&self) -> u64 {
        self.start
    }

    /// The word as a [`Region`], for code that treats all five ranges alike.
    #[must_use]
    pub fn region(&self) -> Region {
        Region {
            start: self.start,
            len: CONTROL_WORD_LEN,
        }
    }

    /// The word as a host slice index range.
    #[must_use]
    pub fn range(&self) -> Range<usize> {
        self.region().range()
    }

    /// Which side of the ring may store to this word.
    #[must_use]
    pub fn writer(&self) -> Writer {
        W::WRITER
    }
}

impl ControlWord<HostWrites> {
    /// Offset the **host** may store a 32-bit word to, from the start of the
    /// resource. Deliberately absent on [`GuestWord`].
    #[must_use]
    pub fn store_offset(&self) -> u64 {
        self.start
    }
}

/// A ring layout that has been validated against the size of the resource it
/// claims to live in.
///
/// Holding one is the proof: its fields are private and its only constructor is
/// [`RingLayout::new`], so every region it hands out is inside the resource,
/// inside the ring region, disjoint from the other four, indexable as a host
/// slice, and — for the control words — 4-byte aligned. Nothing downstream
/// needs to re-check any of that, and there is no way to build one that skips
/// it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RingLayout {
    flags: u32,
    resource_id: u32,
    ring: Region,
    idle_timeout_ns: u64,
    head: HostWord,
    tail: GuestWord,
    status: HostWord,
    buffer: Region,
    extra: Option<Region>,
}

impl RingLayout {
    /// Validate one guest-proposed layout against the resource it names.
    ///
    /// `resource_size` is the length of the shared-memory resource
    /// `info.resource_id` refers to, as the *host* knows it — never a value
    /// from the same message. The caller resolves the resource first; a
    /// resource that does not exist is its refusal to make, not this one's.
    ///
    /// The checks run in a fixed order so the error a guest gets is stable and
    /// the most specific one available: the ring region itself, then the
    /// buffer-size rules (properties of the value, independent of where it is
    /// placed), then each region's placement in declaration order, then
    /// alignment, then pairwise overlap. An overlap is only reported once
    /// everything is known to be in bounds, because a region that is out of
    /// bounds has no meaningful position to overlap from.
    ///
    /// # Errors
    ///
    /// One [`RingLayoutError`] per distinct refusal; see that type.
    pub fn new(info: RingCreateInfo, resource_size: u64) -> Result<Self, RingLayoutError> {
        // The ring region itself. Zero-sized is refused outright (ours): a
        // ring has to hold at least its three control words, and a zero-sized
        // one only ever means the guest computed a length wrong.
        if info.size == 0 {
            return Err(RingLayoutError::ZeroSizedRing);
        }
        let ring_end =
            info.offset
                .checked_add(info.size)
                .ok_or(RingLayoutError::RingRangeOverflows {
                    offset: info.offset,
                    size: info.size,
                })?;
        if ring_end > resource_size {
            return Err(RingLayoutError::RingOutsideResource {
                offset: info.offset,
                size: info.size,
                resource_size,
            });
        }
        // Every offset this module hands out is later used to index a host
        // mapping. On a 32-bit host a 64-bit offset would truncate, so refuse
        // it here rather than let `as usize` wrap somewhere downstream. On
        // x86-64 this is unreachable, which is the point: it costs nothing and
        // removes the question.
        if usize::try_from(ring_end).is_err() {
            return Err(RingLayoutError::RingBeyondHostAddressSpace { end: ring_end });
        }

        // Buffer size rules, before any question of where it sits: a buffer is
        // indexed modulo its own size, so zero would mask nothing and a
        // non-power-of-two would not mask at all.
        if info.buffer_size == 0 {
            return Err(RingLayoutError::ZeroSizedBuffer);
        }
        if !info.buffer_size.is_power_of_two() {
            return Err(RingLayoutError::BufferSizeNotPowerOfTwo {
                size: info.buffer_size,
            });
        }
        if info.buffer_size > MAX_BUFFER_BYTES {
            return Err(RingLayoutError::BufferTooLarge {
                size: info.buffer_size,
                max: MAX_BUFFER_BYTES,
            });
        }

        let place = |which: RingRegion, offset: u64, len: u64| -> Result<Region, RingLayoutError> {
            // `offset + len` is guest arithmetic on two guest values: checked.
            let end = offset
                .checked_add(len)
                .ok_or(RingLayoutError::RegionRangeOverflows {
                    region: which,
                    offset,
                    len,
                })?;
            if end > info.size {
                return Err(RingLayoutError::RegionOutsideRing {
                    region: which,
                    offset,
                    len,
                    ring_size: info.size,
                });
            }
            // Cannot overflow now (`offset <= info.size` and
            // `info.offset + info.size` did not), but the guest chose both
            // halves, so it stays checked.
            let start =
                info.offset
                    .checked_add(offset)
                    .ok_or(RingLayoutError::RegionRangeOverflows {
                        region: which,
                        offset,
                        len,
                    })?;
            Ok(Region { start, len })
        };

        let head = place(RingRegion::Head, info.head_offset, CONTROL_WORD_LEN)?;
        let tail = place(RingRegion::Tail, info.tail_offset, CONTROL_WORD_LEN)?;
        let status = place(RingRegion::Status, info.status_offset, CONTROL_WORD_LEN)?;
        let buffer = place(RingRegion::Buffer, info.buffer_offset, info.buffer_size)?;
        let extra = place(RingRegion::Extra, info.extra_offset, info.extra_size)?;

        // Alignment: all five regions, both ends, on the offset *within the
        // resource* — `info.offset` is guest-chosen as well, so a 4-aligned
        // `headOffset` in a ring that starts on an odd byte is still a
        // misaligned host word. Resources are page-aligned where the host maps
        // them, so this is host alignment. `extra` is the one this catches in
        // practice: the host stores a `u32` there for the guest to poll.
        for (which, relative, region) in [
            (RingRegion::Head, info.head_offset, head),
            (RingRegion::Tail, info.tail_offset, tail),
            (RingRegion::Status, info.status_offset, status),
            (RingRegion::Buffer, info.buffer_offset, buffer),
            (RingRegion::Extra, info.extra_offset, extra),
        ] {
            if (region.start | region.end()) % REGION_ALIGN != 0 {
                return Err(RingLayoutError::MisalignedRegion {
                    region: which,
                    ring_offset: relative,
                    resource_start: region.start,
                    resource_end: region.end(),
                });
            }
        }

        // Pairwise disjoint, all ten pairs. An empty `extra` overlaps nothing.
        let regions = [
            (RingRegion::Head, head),
            (RingRegion::Tail, tail),
            (RingRegion::Status, status),
            (RingRegion::Buffer, buffer),
            (RingRegion::Extra, extra),
        ];
        for (i, (first, a)) in regions.iter().enumerate() {
            for (second, b) in regions.iter().skip(i + 1) {
                if a.overlaps(b) {
                    return Err(RingLayoutError::RegionsOverlap {
                        first: *first,
                        first_at: a.start,
                        first_len: a.len,
                        second: *second,
                        second_at: b.start,
                        second_len: b.len,
                    });
                }
            }
        }

        Ok(Self {
            flags: info.flags,
            resource_id: info.resource_id,
            ring: Region {
                start: info.offset,
                len: info.size,
            },
            idle_timeout_ns: info.idle_timeout_ns,
            head: ControlWord {
                start: head.start,
                _writer: PhantomData,
            },
            tail: ControlWord {
                start: tail.start,
                _writer: PhantomData,
            },
            status: ControlWord {
                start: status.start,
                _writer: PhantomData,
            },
            buffer,
            extra: (!extra.is_empty()).then_some(extra),
        })
    }

    /// The shared-memory resource this ring lives in. Every offset below is
    /// relative to the start of *that* resource.
    #[must_use]
    pub fn resource_id(&self) -> u32 {
        self.resource_id
    }

    /// `VkRingCreateInfoMESA::flags`, exactly as the guest sent it.
    ///
    /// Carried, not judged: venus-protocol defines no bits for it, so there is
    /// nothing here to check it against. See [`RingCreateInfo::flags`] for why
    /// that is not the usual "unknown bits are refused" answer, and where the
    /// refusal belongs once a bit means something.
    #[must_use]
    pub fn flags(&self) -> u32 {
        self.flags
    }

    /// The whole ring region, inside the resource.
    #[must_use]
    pub fn ring(&self) -> Region {
        self.ring
    }

    /// How long the guest asked the host to keep polling before parking, in
    /// nanoseconds.
    ///
    /// Unvalidated on purpose: it names no memory, so there is nothing here to
    /// check it against. It is still a guest value, so whoever *waits* on it
    /// must clamp it — a `u64::MAX` nanosecond park is a hung host thread.
    #[must_use]
    pub fn idle_timeout_ns(&self) -> u64 {
        self.idle_timeout_ns
    }

    /// The head word: how far the host has consumed. The **host** stores here.
    #[must_use]
    pub fn head(&self) -> HostWord {
        self.head
    }

    /// The tail word: how far the guest has produced. The **guest** stores
    /// here; the host may only load it, which is why the returned type has no
    /// `store_offset`.
    #[must_use]
    pub fn tail(&self) -> GuestWord {
        self.tail
    }

    /// The status word the guest polls. The **host** stores here.
    #[must_use]
    pub fn status(&self) -> HostWord {
        self.status
    }

    /// The command buffer: non-empty, a power of two in length, at most
    /// [`MAX_BUFFER_BYTES`].
    #[must_use]
    pub fn buffer(&self) -> Region {
        self.buffer
    }

    /// The host scratch region, or `None` when the guest declared none. The
    /// host must not store a word "for the guest to poll" when this is `None`;
    /// making it an `Option` is what forces that decision to be made.
    #[must_use]
    pub fn extra(&self) -> Option<Region> {
        self.extra
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One tampering a malicious guest could apply to a well-formed create
    /// info. The tables below are pairs of "what should be refused" and "how
    /// to provoke it", which keeps the expectation next to the mutation.
    type Tamper = fn(&mut RingCreateInfo);

    /// The same, for tamperings that need the ring's size (or another offset)
    /// to place themselves relative to it.
    type TamperWith = fn(&mut RingCreateInfo, u64);

    /// The resource every fixture lives in: 16 KiB.
    const RESOURCE: u64 = 0x4000;

    /// A layout a well-behaved guest might send: an 8 KiB ring at 4 KiB into
    /// a 16 KiB resource, control words at the front, a 4 KiB command buffer,
    /// a small scratch region near the end.
    fn good() -> RingCreateInfo {
        RingCreateInfo {
            flags: 0,
            resource_id: 7,
            offset: 0x1000,
            size: 0x2000,
            idle_timeout_ns: 1_000_000,
            head_offset: 0x000,
            tail_offset: 0x010,
            status_offset: 0x020,
            buffer_offset: 0x400,
            buffer_size: 0x400,
            extra_offset: 0x1000,
            extra_size: 0x010,
        }
    }

    fn accept(info: RingCreateInfo) -> RingLayout {
        match RingLayout::new(info, RESOURCE) {
            Ok(layout) => layout,
            Err(err) => panic!("expected {info:?} to be accepted, got {err}"),
        }
    }

    fn reject(info: RingCreateInfo) -> RingLayoutError {
        match RingLayout::new(info, RESOURCE) {
            Ok(_) => panic!("expected {info:?} to be refused"),
            Err(err) => err,
        }
    }

    #[test]
    fn a_well_formed_layout_reports_resource_relative_offsets() {
        let layout = accept(good());

        assert_eq!(layout.resource_id(), 7);
        assert_eq!(layout.idle_timeout_ns(), 1_000_000);
        assert_eq!(layout.ring().start(), 0x1000);
        assert_eq!(layout.ring().len(), 0x2000);
        assert_eq!(layout.ring().end(), 0x3000);

        // Ring-relative in, resource-relative out: every offset has the ring's
        // own 0x1000 folded into it exactly once.
        assert_eq!(layout.head().offset(), 0x1000);
        assert_eq!(layout.tail().offset(), 0x1010);
        assert_eq!(layout.status().offset(), 0x1020);
        assert_eq!(layout.buffer().start(), 0x1400);
        assert_eq!(layout.buffer().len(), 0x400);
        assert_eq!(layout.buffer().end(), 0x1800);
        assert_eq!(
            layout.extra().map(|e| (e.start(), e.len())),
            Some((0x2000, 0x10))
        );

        assert_eq!(layout.head().range(), 0x1000..0x1004);
        assert_eq!(layout.buffer().range(), 0x1400..0x1800);

        // Every region ends inside the resource, which is the only property a
        // consumer indexing the mapping actually needs.
        for end in [
            layout.head().region().end(),
            layout.tail().region().end(),
            layout.status().region().end(),
            layout.buffer().end(),
            layout.extra().map_or(0, |e| e.end()),
        ] {
            assert!(end <= RESOURCE, "{end:#x} escapes the resource");
        }
    }

    #[test]
    fn the_api_says_who_may_write_each_word() {
        let layout = accept(good());

        assert_eq!(layout.head().writer(), Writer::Host);
        assert_eq!(layout.status().writer(), Writer::Host);
        assert_eq!(layout.tail().writer(), Writer::Guest);

        // Only the host-written words offer a store offset. The tail word's
        // type has no `store_offset` at all, so a host write to it does not
        // compile — the check this test cannot spell is the one that matters:
        //
        //     layout.tail().store_offset();   // error[E0599]
        assert_eq!(layout.head().store_offset(), layout.head().offset());
        assert_eq!(layout.status().store_offset(), layout.status().offset());

        // A generic writer path is spelled against the marker, not the name.
        fn store_to(word: &ControlWord<HostWrites>) -> u64 {
            word.store_offset()
        }
        assert_eq!(store_to(&layout.head()), 0x1000);
        assert_eq!(store_to(&layout.status()), 0x1020);
    }

    #[test]
    fn a_ring_flush_against_the_end_of_the_resource_is_accepted() {
        // Every region flush against its neighbour, the last one flush against
        // the end of the ring, and the ring flush against the end of the
        // resource. A validator that is merely paranoid rejects this.
        let info = RingCreateInfo {
            offset: RESOURCE - 0x2000,
            size: 0x2000,
            head_offset: 0,
            tail_offset: 4,
            status_offset: 0x2000 - 4,
            buffer_offset: 8,
            buffer_size: 0x1000,
            extra_offset: 8 + 0x1000,
            extra_size: 0x2000 - 4 - (8 + 0x1000),
            ..good()
        };
        let layout = accept(info);
        assert_eq!(layout.ring().end(), RESOURCE);
        assert_eq!(layout.status().region().end(), RESOURCE);
        assert_eq!(layout.extra().map(|e| e.end()), Some(RESOURCE - 4));

        // One byte more of resource demanded than exists, and it is refused.
        assert!(matches!(
            RingLayout::new(info, RESOURCE - 1),
            Err(RingLayoutError::RingOutsideResource {
                offset: 0x2000,
                size: 0x2000,
                resource_size: 0x3fff,
            })
        ));
    }

    #[test]
    fn a_zero_sized_ring_is_refused_before_anything_else() {
        let info = RingCreateInfo { size: 0, ..good() };
        assert!(matches!(reject(info), RingLayoutError::ZeroSizedRing));

        // Even when nothing else is wrong with it and the ring is empty *and*
        // the resource is empty.
        assert!(matches!(
            RingLayout::new(
                RingCreateInfo {
                    size: 0,
                    offset: 0,
                    ..good()
                },
                0
            ),
            Err(RingLayoutError::ZeroSizedRing)
        ));
    }

    #[test]
    fn a_ring_larger_than_its_resource_is_refused() {
        for (offset, size) in [
            (0, RESOURCE + 1),
            (0x1000, RESOURCE),
            (RESOURCE, 1),
            (RESOURCE - 1, 2),
            (u64::MAX - 0x1000, 0x1000),
        ] {
            let info = RingCreateInfo {
                offset,
                size,
                ..good()
            };
            assert!(
                matches!(reject(info), RingLayoutError::RingOutsideResource { .. }),
                "{offset:#x}..+{size:#x} was not refused as outside the resource"
            );
        }

        // And a resource of zero bytes holds no ring at all.
        assert!(matches!(
            RingLayout::new(good(), 0),
            Err(RingLayoutError::RingOutsideResource { .. })
        ));
    }

    #[test]
    fn offsets_that_would_wrap_are_refused_rather_than_wrapped() {
        // The ring region's own arithmetic.
        let info = RingCreateInfo {
            offset: u64::MAX - 0x100,
            size: 0x200,
            ..good()
        };
        assert!(matches!(
            reject(info),
            RingLayoutError::RingRangeOverflows {
                offset,
                size: 0x200
            } if offset == u64::MAX - 0x100
        ));

        // `offset + size == u64::MAX` exactly: no overflow, still not in a
        // 16 KiB resource.
        let info = RingCreateInfo {
            offset: u64::MAX - 0x200,
            size: 0x200,
            ..good()
        };
        assert!(matches!(
            reject(info),
            RingLayoutError::RingOutsideResource { .. }
        ));

        // Each region's own arithmetic, one at a time. A wrapping validator
        // would compute a tiny end offset here and wave every one of them
        // through.
        let cases: [(RingRegion, Tamper); 5] = [
            (RingRegion::Head, |i| i.head_offset = u64::MAX - 1),
            (RingRegion::Tail, |i| i.tail_offset = u64::MAX - 3),
            (RingRegion::Status, |i| i.status_offset = u64::MAX),
            (RingRegion::Buffer, |i| {
                i.buffer_offset = u64::MAX - 0xff;
                i.buffer_size = 0x400;
            }),
            (RingRegion::Extra, |i| {
                i.extra_offset = u64::MAX;
                i.extra_size = 0x10;
            }),
        ];
        for (which, mutate) in cases {
            let mut info = good();
            mutate(&mut info);
            let err = reject(info);
            assert!(
                matches!(err, RingLayoutError::RegionRangeOverflows { region, .. } if region == which),
                "{which}: expected an overflow refusal, got {err}"
            );
        }

        // A huge offset with a *zero* length does not overflow — it is simply
        // outside the ring, and must be refused as that.
        let info = RingCreateInfo {
            extra_offset: u64::MAX,
            extra_size: 0,
            ..good()
        };
        assert!(matches!(
            reject(info),
            RingLayoutError::RegionOutsideRing {
                region: RingRegion::Extra,
                ..
            }
        ));
    }

    #[test]
    fn every_region_can_be_pushed_out_of_the_ring_on_its_own() {
        let size = good().size;
        // Each case puts exactly one region one byte past the end of the ring.
        let cases: [(RingRegion, TamperWith); 5] = [
            (RingRegion::Head, |i, size| i.head_offset = size - 3),
            (RingRegion::Tail, |i, size| i.tail_offset = size - 3),
            (RingRegion::Status, |i, size| i.status_offset = size - 3),
            (RingRegion::Buffer, |i, size| {
                i.buffer_offset = size - i.buffer_size + 1;
            }),
            (RingRegion::Extra, |i, size| {
                i.extra_offset = size - i.extra_size + 1;
            }),
        ];
        for (which, mutate) in cases {
            let mut info = good();
            mutate(&mut info, size);
            let err = reject(info);
            assert!(
                matches!(err, RingLayoutError::RegionOutsideRing { region, ring_size, .. }
                    if region == which && ring_size == size),
                "{which}: expected an out-of-ring refusal, got {err}"
            );
        }
    }

    #[test]
    fn a_control_word_may_not_straddle_the_end_of_the_ring() {
        // Ours, not the reference's. The last legal position for a word is
        // `size - 4`; every byte after that leaves a word half outside.
        let size = good().size;
        for bad in [size - 3, size - 2, size - 1, size] {
            let info = RingCreateInfo {
                status_offset: bad,
                ..good()
            };
            let err = reject(info);
            // A misaligned straddle is reported as a straddle: placement is
            // checked before alignment, and being outside is the worse fact.
            assert!(
                matches!(
                    err,
                    RingLayoutError::RegionOutsideRing {
                        region: RingRegion::Status,
                        ..
                    }
                ),
                "status at {bad:#x} of {size:#x}: {err}"
            );
        }
        // And `size - 4` — the last word that does fit — is accepted.
        let info = RingCreateInfo {
            status_offset: size - 4,
            ..good()
        };
        assert_eq!(accept(info).status().offset(), 0x1000 + size - 4);
    }

    #[test]
    fn every_region_must_be_four_byte_aligned_at_its_start() {
        // All five, not just the control words. `extra` is the one that
        // matters beyond the three: the host stores a `u32` there for the
        // guest to poll, so a misaligned `extra` is a misaligned atomic.
        let cases: [(RingRegion, TamperWith); 5] = [
            (RingRegion::Head, |i, off| i.head_offset = off),
            (RingRegion::Tail, |i, off| i.tail_offset = off),
            (RingRegion::Status, |i, off| i.status_offset = off),
            (RingRegion::Buffer, |i, off| i.buffer_offset = off),
            (RingRegion::Extra, |i, off| i.extra_offset = off),
        ];
        for (which, mutate) in cases {
            for bad in [0x101, 0x102, 0x103, 0x1ff] {
                let mut info = good();
                mutate(&mut info, bad);
                let err = reject(info);
                assert!(
                    matches!(err, RingLayoutError::MisalignedRegion { region, ring_offset, resource_start, .. }
                        if region == which && ring_offset == bad && resource_start == 0x1000 + bad),
                    "{which} at {bad:#x}: {err}"
                );
            }
        }
    }

    #[test]
    fn a_region_must_be_four_byte_aligned_at_its_end_too() {
        // A buffer of one or two bytes is a power of two and in bounds, and
        // still leaves its end — and everything packed after it — misaligned.
        for tiny in [1, 2] {
            let info = RingCreateInfo {
                buffer_size: tiny,
                ..good()
            };
            let err = reject(info);
            assert!(
                matches!(err, RingLayoutError::MisalignedRegion {
                    region: RingRegion::Buffer, resource_start: 0x1400, resource_end, ..
                } if resource_end == 0x1400 + tiny),
                "a {tiny}-byte buffer: {err}"
            );
        }

        // And the same for the scratch region, whose size the guest picks
        // freely — no power-of-two rule stands behind it at all.
        for odd in [1, 2, 3, 0xf] {
            let info = RingCreateInfo {
                extra_size: odd,
                ..good()
            };
            let err = reject(info);
            assert!(
                matches!(
                    err,
                    RingLayoutError::MisalignedRegion {
                        region: RingRegion::Extra,
                        resource_start: 0x2000,
                        ..
                    }
                ),
                "a {odd}-byte scratch region: {err}"
            );
        }

        // Four bytes — one pollable word — is the smallest scratch that works.
        assert_eq!(
            accept(RingCreateInfo {
                extra_size: 4,
                ..good()
            })
            .extra()
            .map(|e| e.len()),
            Some(4)
        );
    }

    #[test]
    fn alignment_is_judged_where_the_word_lands_not_where_it_was_asked_for() {
        // `offset` is guest-chosen too. Every ring-relative offset here is
        // perfectly 4-aligned; the ring itself starts two bytes into the
        // resource, so all three host words would be misaligned atomics.
        let info = RingCreateInfo {
            offset: 0x1002,
            ..good()
        };
        assert!(matches!(
            reject(info),
            RingLayoutError::MisalignedRegion {
                region: RingRegion::Head,
                ring_offset: 0,
                resource_start: 0x1002,
                resource_end: 0x1006,
            }
        ));

        // The same layout one aligned ring-start later is fine, which is what
        // makes the check about the sum rather than about either half.
        assert_eq!(
            accept(RingCreateInfo {
                offset: 0x1004,
                ..good()
            })
            .head()
            .offset(),
            0x1004
        );
    }

    #[test]
    fn the_command_buffer_size_is_a_nonzero_power_of_two_within_the_limit() {
        assert!(matches!(
            reject(RingCreateInfo {
                buffer_size: 0,
                ..good()
            }),
            RingLayoutError::ZeroSizedBuffer
        ));

        for bad in [3, 5, 6, 0x300, 0x401, MAX_BUFFER_BYTES - 1, u64::MAX] {
            let info = RingCreateInfo {
                buffer_size: bad,
                ..good()
            };
            assert!(
                matches!(reject(info), RingLayoutError::BufferSizeNotPowerOfTwo { size } if size == bad),
                "{bad:#x} was not refused as a non-power-of-two"
            );
        }

        for bad in [MAX_BUFFER_BYTES * 2, MAX_BUFFER_BYTES << 8, 1 << 63] {
            let info = RingCreateInfo {
                buffer_size: bad,
                ..good()
            };
            assert!(
                matches!(
                    reject(info),
                    RingLayoutError::BufferTooLarge { size, max } if size == bad && max == MAX_BUFFER_BYTES
                ),
                "{bad:#x} was not refused as over the limit"
            );
        }

        // Exactly the limit is accepted, in a resource big enough to hold it.
        let info = RingCreateInfo {
            offset: 0,
            size: MAX_BUFFER_BYTES + 0x1000,
            head_offset: 0,
            tail_offset: 4,
            status_offset: 8,
            buffer_offset: 0x1000,
            buffer_size: MAX_BUFFER_BYTES,
            extra_offset: 0x10,
            extra_size: 0x10,
            ..good()
        };
        let layout = RingLayout::new(info, MAX_BUFFER_BYTES + 0x1000).expect("at the limit");
        assert_eq!(layout.buffer().len(), MAX_BUFFER_BYTES);
    }

    #[test]
    fn every_pair_of_regions_can_be_made_to_overlap() {
        // Ten distinct pairings; each mutation makes exactly the named pair
        // overlap and leaves every earlier pair disjoint, so the reported pair
        // is the one under test.
        let cases: [(RingRegion, RingRegion, Tamper); 10] = [
            (RingRegion::Head, RingRegion::Tail, |i| {
                i.tail_offset = 0x000
            }),
            (RingRegion::Head, RingRegion::Status, |i| {
                i.status_offset = 0x000;
            }),
            (RingRegion::Head, RingRegion::Buffer, |i| {
                i.buffer_offset = 0x000;
                i.buffer_size = 4;
            }),
            (RingRegion::Head, RingRegion::Extra, |i| {
                i.extra_offset = 0x000;
                i.extra_size = 4;
            }),
            (RingRegion::Tail, RingRegion::Status, |i| {
                i.status_offset = 0x010;
            }),
            (RingRegion::Tail, RingRegion::Buffer, |i| {
                i.buffer_offset = 0x010;
                i.buffer_size = 4;
            }),
            (RingRegion::Tail, RingRegion::Extra, |i| {
                i.extra_offset = 0x010;
                i.extra_size = 4;
            }),
            (RingRegion::Status, RingRegion::Buffer, |i| {
                i.buffer_offset = 0x020;
                i.buffer_size = 4;
            }),
            (RingRegion::Status, RingRegion::Extra, |i| {
                i.extra_offset = 0x020;
                i.extra_size = 4;
            }),
            (RingRegion::Buffer, RingRegion::Extra, |i| {
                i.extra_offset = 0x400 + 0x100;
                i.extra_size = 4;
            }),
        ];
        for (first, second, mutate) in cases {
            let mut info = good();
            mutate(&mut info);
            let err = reject(info);
            assert!(
                matches!(err, RingLayoutError::RegionsOverlap { first: f, second: s, .. }
                    if f == first && s == second),
                "{first}/{second}: expected that pair to be reported, got {err}"
            );
        }
    }

    #[test]
    fn regions_that_merely_touch_do_not_overlap() {
        // Adjacency is legal and common — mesa packs the three words together.
        let info = RingCreateInfo {
            head_offset: 0,
            tail_offset: 4,
            status_offset: 8,
            buffer_offset: 12,
            buffer_size: 0x400,
            extra_offset: 12 + 0x400,
            extra_size: 0x10,
            ..good()
        };
        let layout = accept(info);
        assert_eq!(layout.buffer().start(), 0x100c);
        assert_eq!(layout.extra().map(|e| e.start()), Some(0x140c));
    }

    #[test]
    fn the_scratch_region_is_optional_but_still_bounds_checked() {
        // Zero-sized extra: accepted, reported as absent, and its offset is
        // not silently ignored.
        let layout = accept(RingCreateInfo {
            extra_offset: 0x1000,
            extra_size: 0,
            ..good()
        });
        assert!(layout.extra().is_none());

        // An empty region may sit anywhere inside the ring, including on top
        // of another region: it owns no bytes to conflict over. Both the edge
        // of the command buffer and the middle of it, because those two cases
        // are exactly where the reference's interval test disagrees with
        // itself.
        for inside_the_buffer in [0x400, 0x404, 0x500, 0x7fc, 0x800] {
            assert!(accept(RingCreateInfo {
                extra_offset: inside_the_buffer,
                extra_size: 0,
                ..good()
            })
            .extra()
            .is_none());
        }

        // But not outside it.
        let size = good().size;
        assert!(matches!(
            reject(RingCreateInfo {
                extra_offset: size + 1,
                extra_size: 0,
                ..good()
            }),
            RingLayoutError::RegionOutsideRing {
                region: RingRegion::Extra,
                ..
            }
        ));
    }

    #[test]
    fn a_region_bounds_checks_its_own_sub_ranges_without_wrapping() {
        let layout = accept(good());
        let buffer = layout.buffer();

        assert!(buffer.contains_range(0, 0x400));
        assert!(buffer.contains_range(0x3ff, 1));
        assert!(!buffer.contains_range(0x400, 1));
        assert!(!buffer.contains_range(0x3ff, 2));
        // The pair a wrapping check would wave through.
        assert!(!buffer.contains_range(u64::MAX, 1));
        assert!(!buffer.contains_range(1, u64::MAX));

        assert!(!buffer.is_empty());
        assert_eq!(buffer.range().len(), 0x400);
    }

    #[test]
    fn the_wire_length_counts_the_fields_beside_it() {
        // Nothing here decodes, so this constant's only guard is that it still
        // describes the struct it sits next to: two `uint32_t`s and ten 64-bit
        // fields. `transport` builds its minimum-command length on it.
        assert_eq!(RingCreateInfo::WIRE_LEN, 88);
        assert_eq!(RingCreateInfo::WIRE_LEN, 2 * 4 + 10 * 8);
    }

    #[test]
    fn flags_are_carried_through_unjudged() {
        // No bit is defined, so no bit is refused — but the value has to
        // arrive intact for whoever gains a meaning for one.
        for flags in [0, 1, 0xdead_beef, u32::MAX] {
            let layout = accept(RingCreateInfo { flags, ..good() });
            assert_eq!(layout.flags(), flags);
        }
    }

    #[test]
    fn an_all_zero_create_info_is_refused() {
        // What the decoder hands us when a guest sends nothing at all: every
        // field zero, which is a zero-sized ring before it is anything else.
        assert_eq!(
            RingLayout::new(RingCreateInfo::default(), RESOURCE),
            Err(RingLayoutError::ZeroSizedRing)
        );
        // And in a resource that is itself empty.
        assert_eq!(
            RingLayout::new(RingCreateInfo::default(), 0),
            Err(RingLayoutError::ZeroSizedRing)
        );
    }

    /// An independent judgement of one layout, written the slow obvious way:
    /// 128-bit arithmetic that cannot overflow, and a byte-by-byte occupancy
    /// map instead of interval comparisons. It shares no code with the
    /// validator, which is the point.
    fn oracle_accepts(info: RingCreateInfo, resource: u64) -> bool {
        let size = u128::from(info.size);
        if size == 0 || u128::from(info.offset) + size > u128::from(resource) {
            return false;
        }
        if info.buffer_size == 0
            || !info.buffer_size.is_power_of_two()
            || info.buffer_size > MAX_BUFFER_BYTES
        {
            return false;
        }
        let regions = [
            (info.head_offset, CONTROL_WORD_LEN),
            (info.tail_offset, CONTROL_WORD_LEN),
            (info.status_offset, CONTROL_WORD_LEN),
            (info.buffer_offset, info.buffer_size),
            (info.extra_offset, info.extra_size),
        ];
        for (offset, len) in regions {
            if u128::from(offset) + u128::from(len) > size {
                return false;
            }
        }
        for (offset, len) in regions {
            let start = u128::from(info.offset) + u128::from(offset);
            let align = u128::from(REGION_ALIGN);
            if start % align != 0 || (start + u128::from(len)) % align != 0 {
                return false;
            }
        }
        let Ok(bytes) = usize::try_from(info.size) else {
            return false;
        };
        let mut occupied = vec![false; bytes];
        for (offset, len) in regions {
            for byte in offset..offset + len {
                let Ok(index) = usize::try_from(byte) else {
                    return false;
                };
                match occupied.get_mut(index) {
                    Some(slot) if !*slot => *slot = true,
                    _ => return false,
                }
            }
        }
        true
    }

    #[test]
    fn a_swept_grid_of_layouts_agrees_with_an_independent_judgement() {
        // A small ring, swept over every interesting placement: aligned and
        // misaligned ring starts, in-bounds and just-past-the-end offsets,
        // legal and illegal buffer sizes, present and absent scratch.
        const SIZE: u64 = 32;
        const WORDS: [u64; 9] = [0, 4, 8, 12, 16, 24, 28, 30, 32];
        const BUFFER_OFFSETS: [u64; 5] = [0, 2, 4, 8, 16];
        // 1 and 2 are powers of two that leave the buffer's *end* misaligned.
        const BUFFER_SIZES: [u64; 7] = [0, 1, 2, 4, 8, 16, 32];
        const EXTRA_OFFSETS: [u64; 4] = [0, 2, 16, 32];
        const EXTRA_SIZES: [u64; 3] = [0, 2, 4];

        let mut accepted = 0u32;
        let mut checked = 0u32;
        for ring_offset in [0u64, 2, 4] {
            for head_offset in WORDS {
                for tail_offset in WORDS {
                    for status_offset in WORDS {
                        for buffer_offset in BUFFER_OFFSETS {
                            for buffer_size in BUFFER_SIZES {
                                for extra_offset in EXTRA_OFFSETS {
                                    for extra_size in EXTRA_SIZES {
                                        let info = RingCreateInfo {
                                            flags: 0,
                                            resource_id: 1,
                                            offset: ring_offset,
                                            size: SIZE,
                                            idle_timeout_ns: 0,
                                            head_offset,
                                            tail_offset,
                                            status_offset,
                                            buffer_offset,
                                            buffer_size,
                                            extra_offset,
                                            extra_size,
                                        };
                                        let got = RingLayout::new(info, 64);
                                        let want = oracle_accepts(info, 64);
                                        assert_eq!(
                                            got.is_ok(),
                                            want,
                                            "{info:?}: validator said {got:?}, oracle said {want}"
                                        );
                                        checked += 1;
                                        accepted += u32::from(want);
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
        // Guard against a sweep that proves nothing because it accepted
        // nothing (or everything).
        assert!(
            accepted > 0 && accepted < checked,
            "{accepted} of {checked}"
        );
    }
}
