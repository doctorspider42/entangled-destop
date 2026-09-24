//! The byte layer every Venus command is built from: a decoder over borrowed
//! guest bytes and an encoder into an owned reply buffer (EPIC 20, ADR-0004).
//!
//! Nothing here knows what a Vulkan call is. It knows how wide a scalar is,
//! how an array announces itself, where the padding goes and — above all —
//! **exactly how many bytes each of those consumes**, because the Venus
//! command stream has no length field anywhere in it. A primitive that
//! consumes four bytes where the protocol says eight does not return a wrong
//! value; it moves the cursor into the middle of the next field and every
//! command after it in the batch is noise that still parses. That is why this
//! module's tests assert byte consumption per primitive rather than values.
//!
//! The reference is virglrenderer 1.1.0's generated headers, in
//! `src/venus/venus-protocol/`; `vkr_cs.h` is the hand-written decoder they
//! call into. Line numbers below are from that release.
//!
//! # The format
//!
//! * **Host-native little-endian, 4-byte granular, no padding between
//!   fields.** A 64-bit field therefore routinely starts at a 4-mod-8 offset,
//!   so every read here goes through `from_le_bytes` on a copied array and is
//!   unaligned by construction. There is no `unsafe` in this file.
//! * **Scalars and enums cost four bytes** — including the narrow ones.
//!   `vn_decode_uint8_t` and `vn_decode_uint16_t` read one or two bytes of
//!   value out of a four-byte slot (`vn_protocol_renderer_types.h:357`,
//!   `:386`), so [`Decoder::u8`] and [`Decoder::u16`] advance by four.
//! * **Handles, `size_t`, array lengths and optional-pointer markers cost
//!   eight.** `VkFlags`, `VkBool32`, `VkResult` and `VkStructureType` are
//!   four; `VkDeviceSize` and `VkDeviceAddress` are eight.
//! * **Blobs and strings are the bytes, then padding up to a four-byte
//!   boundary** (`(size + 3) & ~3`). The padding's content is not controlled:
//!   the reference's encoder advances over it without writing it
//!   (`vkr_cs_encoder_write` copies `val_size` and advances `size`), so it is
//!   whatever the reply buffer held. [`Decoder::blob`] never looks at it.
//! * **An array is a `u32` element count somewhere in the body, then an
//!   8-byte length cross-checked against it** — and the null branch consumes
//!   that length too. See [`Decoder::array`].
//! * **A struct is `sType`, then the whole pNext chain, then its own body.**
//!   pNext *before* the body, which is not the order anyone guesses
//!   (`vn_protocol_renderer_device.h:706-716`). See [`Decoder::pnext_chain`].
//! * **`pAllocator` is an 8-byte zero**; non-zero is fatal, because a host
//!   allocator callback cannot be honoured across the wire
//!   (`vn_protocol_renderer_device.h:16938-16941`). See
//!   [`Decoder::null_allocator`].
//! * **The command header is 8 bytes and the reply header is 4.** A command
//!   is `u32` opcode then `u32` flags (`vn_dispatch_command`,
//!   `vn_protocol_renderer_dispatches.h:604-611`); a reply is the opcode
//!   alone (`vn_protocol_renderer_device.h:16705-16709`). The asymmetry is
//!   real and it is a trap: an encoder that writes the flags back desynchronises
//!   the guest's reply parser at the first reply.
//!
//! # Refusals are sticky
//!
//! The reference keeps one `fatal_error` flag per context and never clears it
//! (`vkr_cs_decoder_set_fatal`); the dispatcher checks it once per command and
//! tears the context down. [`Decoder`] models the same thing, but harder: the
//! flag is checked on the way *in* to every operation, so once a stream is
//! fatal nothing after it can return a value at all. The reference does not do
//! that — its `vkr_cs_decoder_read` keeps reading and zero-fills on failure,
//! and only the generated `if (!ptr) return;` guards stop the caller — which
//! means a reference decode that has already gone wrong can still hand a
//! plausible-looking zero to the next field. Here it cannot.
//!
//! # What a guest cannot make the host do
//!
//! * **Allocate on its say-so.** An array's element count is bounded by the
//!   bytes actually left in the stream before anything is reserved: no element
//!   is narrower than four bytes on this wire, so `count * 4 > remaining` is
//!   refused outright and a 400 MB `Vec` never gets as far as the allocator.
//!   What survives that is then charged against a per-command budget
//!   ([`MAX_TEMP_ALLOC_BYTES`], the reference's `VKR_CS_DECODER_TEMP_POOL_MAX_SIZE`)
//!   and reserved with `try_reserve_exact`, so a host that cannot satisfy it
//!   refuses rather than aborts.
//! * **Recurse the host off its stack.** The pNext chain is a recursive
//!   length-free list and the reference's decoder recurses once per link with
//!   no depth limit at all; at 12 bytes per link a 16 MiB ring buffer names
//!   over a million of them. [`MAX_PNEXT_DEPTH`] caps it here.
//! * **Index anything.** Nothing in this file indexes with a guest value:
//!   every read goes through `slice::get` and every length through checked
//!   arithmetic.

use std::collections::TryReserveError;
use std::mem::size_of;

use thiserror::Error;

/// Bytes a scalar or enum occupies, however narrow its value is.
pub const SCALAR_BYTES: usize = 4;

/// Bytes a handle, `size_t`, array length or optional-pointer marker occupies.
pub const WIDE_BYTES: usize = 8;

/// Granularity of the whole stream: every field starts on a multiple of this,
/// and every blob is padded up to it.
pub const GRANULE: usize = 4;

/// Smallest number of bytes one element of a non-blob array can occupy.
///
/// The stream is four-byte granular and the narrowest thing
/// [`Decoder::repeat`] can be asked to read is a scalar, so `count * 4` is a
/// sound lower bound on what an array of `count` elements costs on the wire.
/// That bound is what stops a guest-declared count from reaching the
/// allocator. Byte-wide arrays are not elements in this sense: they are blobs,
/// and go through [`Decoder::blob`], which allocates nothing.
pub const MIN_ELEMENT_BYTES: usize = SCALAR_BYTES;

/// A command header: `u32` opcode, then `u32` flags.
pub const COMMAND_HEADER_BYTES: usize = 8;

/// A reply header: the `u32` opcode, and nothing else. Deliberately *not*
/// [`COMMAND_HEADER_BYTES`].
pub const REPLY_HEADER_BYTES: usize = 4;

/// `VK_COMMAND_GENERATE_REPLY_BIT_EXT` (`vn_protocol_renderer_defines.h:396`):
/// the guest wants a reply encoded for this command.
pub const COMMAND_GENERATE_REPLY: u32 = 0x0000_0001;

/// Temporary host bytes one command stream may be charged for, matching
/// virglrenderer's `VKR_CS_DECODER_TEMP_POOL_MAX_SIZE` (`vkr_cs.h:16`).
///
/// The dispatcher resets it per command with
/// [`Decoder::reset_alloc_budget`], exactly as the reference resets its temp
/// pool.
pub const MAX_TEMP_ALLOC_BYTES: usize = 1 << 30;

/// Links a pNext chain may have before it is refused: 256.
///
/// Ours, not the reference's: `vn_decode_*_pnext_temp` recurses per link with
/// no limit, and every link the guest writes costs it twelve bytes of ring
/// buffer against one host stack frame. This is a **stack** bound, and only
/// that — what bounds a chain's *content* is that no `sType` may appear in it
/// twice ([`crate::venus::protocol::ProtocolError::DuplicatePnextStype`]),
/// so a valid chain is never longer than its parent admits, and that every
/// link decoded is charged to the command's allocation budget
/// ([`Decoder::charge`]).
///
/// It used to be 32, on the theory that real chains are a handful of links
/// deep. They are not: a guest shown 70-odd extensions asks
/// `vkGetPhysicalDeviceFeatures2` about dozens of feature structures in one
/// chain (zink does), and creates its device with the same chain, and the
/// 33rd link killed the context. The executor serves 44 structures under
/// `VkPhysicalDeviceFeatures2`, 27 under `VkPhysicalDeviceProperties2` and 47
/// under `VkDeviceCreateInfo` — the longest chains the capset lets a guest
/// send — but this decoder runs before the executor's policy and must walk
/// whatever the generated protocol admits: 117, 51 and 120 (the most any
/// parent admits). A nested chain (a structure inside a link) continues the
/// same count. So the bound is the longest *decodable* chain with more than
/// twice its length to spare, for the structures the next Vulkan releases
/// add; a decodable link the executor does not serve is then refused by name
/// rather than as "too deep". 256 frames of this recursion are a few tens of
/// KiB of a thread's stack.
/// `venus::executor::query_tests` pins that it stays at least twice the
/// longest admitted chain.
pub const MAX_PNEXT_DEPTH: u32 = 256;

/// Why a Venus command stream could not be decoded, or a reply encoded.
///
/// Every variant is one distinct way the bytes were wrong, with the numbers
/// that made them wrong. None of them is recoverable: the stream has no length
/// fields, so there is no next command to resynchronise on, and every one of
/// these poisons the [`Decoder`] or [`Encoder`] that produced it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum WireError {
    /// Something already went wrong on this stream. The refusal that started
    /// it was returned when it happened and is still available from
    /// [`Decoder::fatal_error`] / [`Encoder::fatal_error`].
    #[error("the stream is already fatal; nothing after the first refusal can be decoded")]
    Poisoned,

    #[error("{needed} bytes wanted at offset {at:#x}, but only {left} remain")]
    Truncated {
        /// Offset the read started at.
        at: usize,
        /// Bytes the primitive needed.
        needed: usize,
        /// Bytes actually left from `at`.
        left: usize,
    },

    #[error("an array length of {found} contradicts the {expected} elements the command declared")]
    ArrayLengthMismatch {
        /// The count named by the `u32` field earlier in the body.
        expected: u64,
        /// The 8-byte length that followed it.
        found: u64,
    },

    #[error(
        "an array of {count} elements needs at least {needed} bytes and the stream has {left}"
    )]
    ArrayLongerThanStream {
        /// Elements the guest declared.
        count: u64,
        /// Bytes they cannot be shorter than, saturating.
        needed: u64,
        /// Bytes left in the stream.
        left: usize,
    },

    #[error(
        "decoding this command would need {wanted} more temporary bytes, past the {budget} left \
         of its {max}-byte budget",
        max = MAX_TEMP_ALLOC_BYTES
    )]
    AllocationBudgetExhausted {
        /// Host bytes the array would cost.
        wanted: usize,
        /// Host bytes this command stream has left.
        budget: usize,
    },

    #[error("the host could not allocate the {wanted} bytes a guest-declared array needs")]
    OutOfMemory {
        /// Host bytes the allocator refused.
        wanted: usize,
    },

    #[error(
        "pAllocator is {0:#x}; Venus requires it null, because a host allocator callback cannot \
         be called across the wire"
    )]
    AllocatorNotNull(u64),

    #[error("a zero-length string has no room for the terminator every Venus string carries")]
    EmptyString,

    #[error("a {len}-byte string contains no NUL terminator")]
    UnterminatedString {
        /// Length the guest declared, before padding.
        len: usize,
    },

    #[error("a string is not valid UTF-8")]
    NonUtf8String,

    #[error(
        "sType {stype:#x} is not one {parent} accepts in its pNext chain, and a length-free chain \
         cannot skip what it cannot decode"
    )]
    UnknownPnextStype {
        /// Struct whose whitelist rejected it.
        parent: &'static str,
        /// The `VkStructureType` the guest sent, as the signed enum it is.
        stype: i32,
    },

    #[error("the pNext chain of {parent} is deeper than {max} links", max = MAX_PNEXT_DEPTH)]
    PnextChainTooDeep {
        /// Struct whose chain ran away.
        parent: &'static str,
    },

    #[error("the reply would grow to {wanted} bytes, past its {limit}-byte limit")]
    ReplyTooLong {
        /// Length the reply would reach.
        wanted: usize,
        /// Length it may not exceed.
        limit: usize,
    },
}

impl WireError {
    /// `true` for the refusal a *later* operation gets once the stream has
    /// already gone fatal, as opposed to the one that made it fatal.
    #[must_use]
    pub fn is_poisoned(self) -> bool {
        matches!(self, Self::Poisoned)
    }
}

/// The per-parent-struct pNext whitelist, as a thing a struct decoder plugs
/// in.
///
/// The set of `sType`s allowed in a pNext chain is a property of the *parent*
/// struct — `vn_decode_VkDeviceCreateInfo_pnext_temp` has a different `switch`
/// from `vn_decode_VkImageCreateInfo_pnext_temp` — so the whitelist cannot
/// live here. What lives here is the traversal: the 8-byte presence marker,
/// the `sType`, the recursion into the rest of the chain *before* this link's
/// body, and the depth cap. An implementor supplies only [`visit`](Self::visit)
/// and accumulates whatever it wants into itself.
///
/// An `sType` the implementor does not know is **fatal**, and [`unknown`](Self::unknown)
/// spells that refusal. It cannot be anything else: the link carries no
/// length, so there is no way to step over it.
pub trait PnextVisitor<'a> {
    /// The parent struct's name, for the refusal message. `"VkDeviceCreateInfo"`.
    const PARENT: &'static str;

    /// Decode the body of one chain link with this `sType`, or refuse it.
    ///
    /// The link's own pNext has already been consumed when this is called —
    /// that is the order the wire uses — so `dec` is positioned at the link's
    /// first body field.
    ///
    /// # Errors
    ///
    /// [`Self::unknown`] for an `sType` outside this parent's whitelist, or
    /// whatever decoding the body produced. Either way the stream is left
    /// fatal.
    fn visit(&mut self, stype: i32, dec: &mut Decoder<'a>) -> Result<(), WireError>;

    /// The refusal for an `sType` this parent does not accept.
    #[must_use]
    fn unknown(stype: i32) -> WireError
    where
        Self: Sized,
    {
        WireError::UnknownPnextStype {
            parent: Self::PARENT,
            stype,
        }
    }
}

/// A Venus command header: what the command is, and whether it wants a reply.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CommandHeader {
    /// `VkCommandTypeEXT`. Carried unjudged: which opcodes exist is the
    /// dispatcher's table, not this layer's.
    pub opcode: u32,
    /// `VkCommandFlagsEXT`, carried whole so an unknown bit can be refused
    /// where the known set is defined.
    pub flags: u32,
}

impl CommandHeader {
    /// Whether the guest asked for a reply to be encoded.
    #[must_use]
    pub fn wants_reply(&self) -> bool {
        self.flags & COMMAND_GENERATE_REPLY != 0
    }
}

/// A decoder over one borrowed span of guest-written command bytes.
///
/// The span is a *copy* of what the ring held — see `super::pump` — so nothing
/// here races a guest that is still writing. Holding a `Decoder` asserts
/// nothing about the bytes; every method is fallible and the first failure is
/// permanent.
#[derive(Debug)]
pub struct Decoder<'a> {
    bytes: &'a [u8],
    at: usize,
    /// The refusal that killed this stream, if any. Set once, never cleared.
    fatal: Option<WireError>,
    /// Host bytes this command may still be charged for.
    budget: usize,
    /// What [`Decoder::reset_alloc_budget`] restores. Remembered rather than
    /// assumed to be [`MAX_TEMP_ALLOC_BYTES`], because a caller who asked for
    /// a smaller budget meant it for the whole stream, not for its first
    /// command — resetting to the maximum made
    /// [`Decoder::with_alloc_budget`] a promise that expired after one
    /// command, which is worse than not offering it.
    initial_budget: usize,
    /// How deep the pNext recursion currently is.
    depth: u32,
}

impl<'a> Decoder<'a> {
    /// A decoder over `bytes`, with a full [`MAX_TEMP_ALLOC_BYTES`] budget.
    #[must_use]
    pub fn new(bytes: &'a [u8]) -> Self {
        Self {
            bytes,
            at: 0,
            fatal: None,
            budget: MAX_TEMP_ALLOC_BYTES,
            initial_budget: MAX_TEMP_ALLOC_BYTES,
            depth: 0,
        }
    }

    /// A decoder with a smaller allocation budget than the protocol's maximum.
    /// A caller that knows what a command can legitimately need should say so.
    #[must_use]
    pub fn with_alloc_budget(bytes: &'a [u8], budget: usize) -> Self {
        let budget = budget.min(MAX_TEMP_ALLOC_BYTES);
        Self {
            budget,
            initial_budget: budget,
            ..Self::new(bytes)
        }
    }

    /// Offset of the next unread byte.
    #[must_use]
    pub fn position(&self) -> usize {
        self.at
    }

    /// Bytes left unread. Zero once the stream is exhausted; frozen once it is
    /// fatal.
    #[must_use]
    pub fn remaining(&self) -> usize {
        self.bytes.len().saturating_sub(self.at)
    }

    /// Host bytes this command may still be charged for.
    #[must_use]
    pub fn alloc_budget(&self) -> usize {
        self.budget
    }

    /// Whether anything has gone wrong on this stream.
    #[must_use]
    pub fn is_fatal(&self) -> bool {
        self.fatal.is_some()
    }

    /// The refusal that made this stream fatal, if one did. This is the
    /// *first* one: everything after it returns [`WireError::Poisoned`].
    #[must_use]
    pub fn fatal_error(&self) -> Option<WireError> {
        self.fatal
    }

    /// Charge `bytes` of host storage the caller keeps for this command to the
    /// allocation budget, as [`Decoder::repeat`] charges an array's — for
    /// storage that grows one element at a time: a pNext chain's links.
    ///
    /// # Errors
    /// [`WireError::AllocationBudgetExhausted`] past the budget (the stream
    /// is then fatal), or [`WireError::Poisoned`].
    pub fn charge(&mut self, bytes: usize) -> Result<(), WireError> {
        self.guard()?;
        if bytes > self.budget {
            let budget = self.budget;
            return self.fail(WireError::AllocationBudgetExhausted {
                wanted: bytes,
                budget,
            });
        }
        self.budget -= bytes;
        Ok(())
    }

    /// Give the next command a full allocation budget again, as
    /// `vn_cs_decoder_reset_temp_pool` does. Does not, and must not, clear the
    /// fatal flag — the reference never clears its own either.
    pub fn reset_alloc_budget(&mut self) {
        self.budget = self.initial_budget;
    }

    /// Poison the stream from a layer above this one — an opcode with no
    /// dispatch entry, an object id that names nothing, a body field outside
    /// its legal range.
    ///
    /// Returns `err` so the call site reads
    /// `return Err(dec.set_fatal(WireError::…))`. A second call keeps the
    /// first refusal.
    pub fn set_fatal(&mut self, err: WireError) -> WireError {
        self.fatal.get_or_insert(err);
        err
    }

    // ---- the plumbing every primitive shares --------------------------------

    /// Record `err` as the reason this stream died, and hand it back as the
    /// `Err` of whatever the caller was doing.
    fn fail<T>(&mut self, err: WireError) -> Result<T, WireError> {
        self.fatal.get_or_insert(err);
        Err(err)
    }

    /// Poison the stream with a caller's (or closure's) refusal, keeping
    /// whichever came first.
    fn absorb<T>(&mut self, result: Result<T, WireError>) -> Result<T, WireError> {
        match result {
            Ok(value) => Ok(value),
            Err(err) => {
                self.fatal.get_or_insert(err);
                Err(err)
            }
        }
    }

    /// The gate every public method passes through first. This, and not the
    /// per-call `?`, is what makes a refusal stick: a caller that drops one
    /// `Result` still cannot get a value out of the next call.
    fn guard(&self) -> Result<(), WireError> {
        match self.fatal {
            Some(_) => Err(WireError::Poisoned),
            None => Ok(()),
        }
    }

    /// `n` bytes at the cursor, without advancing.
    fn peek_bytes(&mut self, n: usize) -> Result<&'a [u8], WireError> {
        self.guard()?;
        let bytes = self.bytes;
        let at = self.at;
        let end = match at.checked_add(n) {
            Some(end) => end,
            None => {
                let left = self.remaining();
                return self.fail(WireError::Truncated {
                    at,
                    needed: n,
                    left,
                });
            }
        };
        match bytes.get(at..end) {
            Some(slice) => Ok(slice),
            None => {
                let left = self.remaining();
                self.fail(WireError::Truncated {
                    at,
                    needed: n,
                    left,
                })
            }
        }
    }

    /// `n` bytes at the cursor, advancing past them. Advances by exactly `n`
    /// or not at all: a short read leaves the cursor where it was and the
    /// stream fatal.
    fn take(&mut self, n: usize) -> Result<&'a [u8], WireError> {
        let slice = self.peek_bytes(n)?;
        self.at = self.at.saturating_add(n);
        Ok(slice)
    }

    /// The first four bytes of a four-byte slot.
    fn slot4(&mut self) -> Result<[u8; 4], WireError> {
        let slice = self.take(SCALAR_BYTES)?;
        let mut word = [0u8; 4];
        word.copy_from_slice(slice);
        Ok(word)
    }

    /// An eight-byte field, which the protocol's 4-byte granularity routinely
    /// puts at a 4-mod-8 offset. `from_le_bytes` over a copy, so alignment
    /// never enters into it.
    fn slot8(&mut self) -> Result<[u8; 8], WireError> {
        let slice = self.take(WIDE_BYTES)?;
        let mut word = [0u8; 8];
        word.copy_from_slice(slice);
        Ok(word)
    }

    // ---- scalars ------------------------------------------------------------

    /// A `uint32_t`. Four bytes.
    ///
    /// # Errors
    /// [`WireError::Truncated`] past the end, [`WireError::Poisoned`] after a
    /// previous refusal.
    pub fn u32(&mut self) -> Result<u32, WireError> {
        self.slot4().map(u32::from_le_bytes)
    }

    /// An `int32_t`, and with it every Vulkan enum and `VkResult`. Four bytes.
    ///
    /// # Errors
    /// As [`Decoder::u32`].
    pub fn i32(&mut self) -> Result<i32, WireError> {
        self.slot4().map(i32::from_le_bytes)
    }

    /// A `VkStructureType`. Four bytes, signed, because that is how the
    /// generated code compares it (`switch ((int32_t)stype)`).
    ///
    /// # Errors
    /// As [`Decoder::u32`].
    pub fn structure_type(&mut self) -> Result<i32, WireError> {
        self.i32()
    }

    /// A `float`. Four bytes.
    ///
    /// # Errors
    /// As [`Decoder::u32`].
    pub fn f32(&mut self) -> Result<f32, WireError> {
        self.slot4().map(f32::from_le_bytes)
    }

    /// A `VkFlags`. Four bytes, carried whole: which bits are legal belongs
    /// with the command that has a meaning for them.
    ///
    /// # Errors
    /// As [`Decoder::u32`].
    pub fn flags(&mut self) -> Result<u32, WireError> {
        self.u32()
    }

    /// A `VkBool32`. Four bytes, returned raw rather than as a `bool`: Vulkan
    /// says any non-zero is true, and the caller may want to see which
    /// non-zero it was.
    ///
    /// # Errors
    /// As [`Decoder::u32`].
    pub fn bool32(&mut self) -> Result<u32, WireError> {
        self.u32()
    }

    /// A `uint16_t`. **Four** bytes consumed, two of value
    /// (`vn_protocol_renderer_types.h:386`).
    ///
    /// # Errors
    /// As [`Decoder::u32`].
    pub fn u16(&mut self) -> Result<u16, WireError> {
        let word = self.slot4()?;
        Ok(u16::from_le_bytes([word[0], word[1]]))
    }

    /// A `uint8_t`. **Four** bytes consumed, one of value
    /// (`vn_protocol_renderer_types.h:357`). The other three are padding whose
    /// content the reference does not control.
    ///
    /// # Errors
    /// As [`Decoder::u32`].
    pub fn u8(&mut self) -> Result<u8, WireError> {
        Ok(self.slot4()?[0])
    }

    /// A `uint64_t`. Eight bytes, unaligned-safe.
    ///
    /// # Errors
    /// As [`Decoder::u32`].
    pub fn u64(&mut self) -> Result<u64, WireError> {
        self.slot8().map(u64::from_le_bytes)
    }

    /// An `int64_t`. Eight bytes.
    ///
    /// # Errors
    /// As [`Decoder::u32`].
    pub fn i64(&mut self) -> Result<i64, WireError> {
        self.slot8().map(i64::from_le_bytes)
    }

    /// A `size_t`. Eight bytes on this wire regardless of the host's word
    /// size, and returned as `u64` for that reason — narrowing it is the
    /// caller's decision to make and to check.
    ///
    /// # Errors
    /// As [`Decoder::u32`].
    pub fn size(&mut self) -> Result<u64, WireError> {
        self.u64()
    }

    /// A `VkDeviceSize` or `VkDeviceAddress`. Eight bytes.
    ///
    /// # Errors
    /// As [`Decoder::u32`].
    pub fn device_size(&mut self) -> Result<u64, WireError> {
        self.u64()
    }

    /// A Vulkan object handle, as the 64-bit id the guest knows it by. Eight
    /// bytes. Resolving the id to a host object is the object table's job, and
    /// an id that names nothing is exactly the kind of refusal
    /// [`Decoder::set_fatal`] exists for.
    ///
    /// # Errors
    /// As [`Decoder::u32`].
    pub fn handle(&mut self) -> Result<u64, WireError> {
        self.u64()
    }

    // ---- pointers, blobs and strings ---------------------------------------

    /// An optional-pointer marker: eight bytes, non-zero meaning "the thing it
    /// points at follows".
    ///
    /// Non-zero, not "exactly 1": `vn_decode_simple_pointer` *is*
    /// `vn_decode_array_size_unchecked` — literally the same function — so the
    /// two cannot be judged differently without judging array lengths the same
    /// way.
    ///
    /// # Errors
    /// As [`Decoder::u32`].
    pub fn simple_pointer(&mut self) -> Result<bool, WireError> {
        Ok(self.u64()? != 0)
    }

    /// The `pAllocator` slot: eight bytes that must be zero.
    ///
    /// # Errors
    /// [`WireError::AllocatorNotNull`] when it is not, which is fatal — the
    /// guest asked the host to call back into guest code for every allocation,
    /// and there is no wire for that.
    pub fn null_allocator(&mut self) -> Result<(), WireError> {
        let value = self.u64()?;
        if value != 0 {
            return self.fail(WireError::AllocatorNotNull(value));
        }
        Ok(())
    }

    /// `len` bytes of opaque data, consuming `len` rounded up to the next
    /// four-byte boundary.
    ///
    /// Borrowed from the stream, so it costs no allocation however large `len`
    /// is — the bytes have to be there already. The padding is consumed but
    /// never read: the reference's encoder does not write it, so it is
    /// whatever was in the buffer before.
    ///
    /// This is also how a `uint8_t` *array* arrives
    /// (`vn_decode_uint8_t_array`), which is the same padded blob.
    ///
    /// # Errors
    /// [`WireError::Truncated`] if the padded length is not there.
    pub fn blob(&mut self, len: usize) -> Result<&'a [u8], WireError> {
        let padded = match len.checked_next_multiple_of(GRANULE) {
            Some(padded) => padded,
            None => {
                let (at, left) = (self.at, self.remaining());
                return self.fail(WireError::Truncated {
                    at,
                    needed: len,
                    left,
                });
            }
        };
        let slice = self.take(padded)?;
        slice.get(..len).ok_or(WireError::Poisoned)
    }

    /// The bytes of a NUL-terminated string of declared length `len`
    /// (terminator included), padded to four bytes, returned **without** the
    /// terminator.
    ///
    /// The reference forces `val[size - 1] = '\0'` after copying, so a string
    /// with no terminator is silently truncated there. Truncating a layer or
    /// extension name into a different, valid name is worse than refusing it
    /// and no encoder produces one, so this refuses.
    ///
    /// # Errors
    /// [`WireError::EmptyString`] for `len == 0` (the reference sets fatal
    /// there too), [`WireError::UnterminatedString`] when no NUL is present,
    /// [`WireError::Truncated`] if the padded bytes are not there.
    pub fn string_bytes(&mut self, len: usize) -> Result<&'a [u8], WireError> {
        if len == 0 {
            return self.fail(WireError::EmptyString);
        }
        let raw = self.blob(len)?;
        match raw.iter().position(|&byte| byte == 0) {
            Some(nul) => raw.get(..nul).ok_or(WireError::Poisoned),
            None => self.fail(WireError::UnterminatedString { len }),
        }
    }

    /// An optional string: its own 8-byte length, then the padded bytes.
    ///
    /// A null string is an 8-byte zero **and that is all** — the length is
    /// consumed either way, which is the same trap [`Decoder::array`] exists
    /// to close, and the reason this is a combinator rather than two calls.
    ///
    /// # Errors
    /// As [`Decoder::string_bytes`], plus [`WireError::ArrayLongerThanStream`]
    /// for a length no host slice could hold.
    pub fn opt_string(&mut self) -> Result<Option<&'a [u8]>, WireError> {
        let size = self.array_size_unchecked()?;
        if size == 0 {
            return Ok(None);
        }
        let len = match usize::try_from(size) {
            Ok(len) => len,
            Err(_) => {
                let left = self.remaining();
                return self.fail(WireError::ArrayLongerThanStream {
                    count: size,
                    needed: size,
                    left,
                });
            }
        };
        self.string_bytes(len).map(Some)
    }

    /// [`Decoder::opt_string`], validated as UTF-8.
    ///
    /// Calling this rather than `opt_string` is a decision to make a non-UTF-8
    /// name fatal. Vulkan's strings are UTF-8, but the reference does not
    /// check, so the choice belongs at the call site.
    ///
    /// # Errors
    /// As [`Decoder::opt_string`], plus [`WireError::NonUtf8String`].
    pub fn opt_str(&mut self) -> Result<Option<&'a str>, WireError> {
        match self.opt_string()? {
            None => Ok(None),
            Some(bytes) => match std::str::from_utf8(bytes) {
                Ok(text) => Ok(Some(text)),
                Err(_) => self.fail(WireError::NonUtf8String),
            },
        }
    }

    // ---- arrays -------------------------------------------------------------

    /// An 8-byte array length, cross-checked against the element count the
    /// body declared earlier.
    ///
    /// A mismatch is fatal, exactly as in `vn_decode_array_size`
    /// (`vn_protocol_renderer_types.h:221-230`).
    ///
    /// Prefer [`Decoder::array`]: calling this by hand is how the null branch
    /// gets forgotten.
    ///
    /// # Errors
    /// [`WireError::ArrayLengthMismatch`], or [`WireError::Truncated`].
    pub fn array_size(&mut self, expected_count: u64) -> Result<u64, WireError> {
        let found = self.u64()?;
        if found != expected_count {
            return self.fail(WireError::ArrayLengthMismatch {
                expected: expected_count,
                found,
            });
        }
        Ok(found)
    }

    /// An 8-byte array length with nothing to cross-check it against, as
    /// strings and reply arrays use (`vn_decode_array_size_unchecked`).
    ///
    /// # Errors
    /// As [`Decoder::u32`].
    pub fn array_size_unchecked(&mut self) -> Result<u64, WireError> {
        self.u64()
    }

    /// The 8-byte array length at the cursor, **without** consuming it.
    ///
    /// The generated code peeks this to choose a branch and then consumes it
    /// in both. [`Decoder::array`] does the same thing with the consume hoisted
    /// out of the branch, so this is here only for a caller that must decide
    /// something else first. A short peek is fatal, as it is in
    /// `vkr_cs_decoder_peek_internal`.
    ///
    /// # Errors
    /// As [`Decoder::u32`].
    pub fn peek_array_size(&mut self) -> Result<u64, WireError> {
        let slice = self.peek_bytes(WIDE_BYTES)?;
        let mut word = [0u8; 8];
        word.copy_from_slice(slice);
        Ok(u64::from_le_bytes(word))
    }

    /// An optional array: **always** its 8-byte length, cross-checked against
    /// `expected_count`, and then the elements only if there are any.
    ///
    /// This is the single easiest way to desynchronise a Venus stream, so the
    /// API does not offer the mistake. `vn_protocol_renderer_device.h:688-703`
    /// shows both branches of the generated code, and the `else` — the *null*
    /// branch — calls `vn_decode_array_size` purely for its side effect of
    /// moving the cursor eight bytes. Here the length is consumed before the
    /// branch exists, so there is nothing for a caller to leave out.
    ///
    /// `elements` is called with the length only when it is non-zero, and gets
    /// it as a `usize` it can loop over. Returning `Ok(None)` means the guest
    /// sent a null array, which is a legitimate thing for it to do — and it is
    /// only legitimate when `expected_count` was zero too, which the
    /// cross-check enforces for free.
    ///
    /// An error out of `elements` poisons the stream: a caller-level refusal
    /// halfway through an array leaves the cursor inside an element, and there
    /// is no way back from that either.
    ///
    /// # Errors
    /// [`WireError::ArrayLengthMismatch`], [`WireError::Truncated`], or
    /// whatever `elements` returned.
    pub fn array<T>(
        &mut self,
        expected_count: u64,
        elements: impl FnOnce(&mut Self, usize) -> Result<T, WireError>,
    ) -> Result<Option<T>, WireError> {
        let size = self.array_size(expected_count)?;
        self.array_body(size, elements)
    }

    /// [`Decoder::array`] without a count to cross-check against, for the
    /// places the generated code uses `vn_decode_array_size_unchecked`.
    ///
    /// # Errors
    /// As [`Decoder::array`], minus the mismatch.
    pub fn array_unchecked<T>(
        &mut self,
        elements: impl FnOnce(&mut Self, usize) -> Result<T, WireError>,
    ) -> Result<Option<T>, WireError> {
        let size = self.array_size_unchecked()?;
        self.array_body(size, elements)
    }

    fn array_body<T>(
        &mut self,
        size: u64,
        elements: impl FnOnce(&mut Self, usize) -> Result<T, WireError>,
    ) -> Result<Option<T>, WireError> {
        if size == 0 {
            return Ok(None);
        }
        let count = match usize::try_from(size) {
            Ok(count) => count,
            Err(_) => {
                let left = self.remaining();
                return self.fail(WireError::ArrayLongerThanStream {
                    count: size,
                    needed: size,
                    left,
                });
            }
        };
        let produced = elements(self, count);
        self.absorb(produced).map(Some)
    }

    /// `count` elements, each decoded by `item`, collected into a `Vec`.
    ///
    /// The allocation is bounded twice before it happens. First by the stream:
    /// no element on this wire is narrower than [`MIN_ELEMENT_BYTES`], so a
    /// `count` whose elements could not fit in what is left is refused without
    /// the allocator ever hearing about it — which is what stops a 16-byte
    /// command from asking for a 400 MB `Vec`. Then by the per-command budget,
    /// and finally by `try_reserve_exact`, so a host that simply cannot
    /// allocate refuses rather than aborts.
    ///
    /// # Errors
    /// [`WireError::ArrayLongerThanStream`],
    /// [`WireError::AllocationBudgetExhausted`], [`WireError::OutOfMemory`],
    /// or whatever `item` returned.
    pub fn repeat<T>(
        &mut self,
        count: usize,
        mut item: impl FnMut(&mut Self) -> Result<T, WireError>,
    ) -> Result<Vec<T>, WireError> {
        let mut out = self.admit::<T>(count, MIN_ELEMENT_BYTES)?;
        for _ in 0..count {
            let value = item(self);
            out.push(self.absorb(value)?);
        }
        Ok(out)
    }

    /// `count` `uint32_t`s.
    ///
    /// # Errors
    /// As [`Decoder::repeat`].
    pub fn u32_array(&mut self, count: usize) -> Result<Vec<u32>, WireError> {
        let mut out = self.admit::<u32>(count, SCALAR_BYTES)?;
        for _ in 0..count {
            let value = self.u32()?;
            out.push(value);
        }
        Ok(out)
    }

    /// `count` `float`s.
    ///
    /// # Errors
    /// As [`Decoder::repeat`].
    pub fn f32_array(&mut self, count: usize) -> Result<Vec<f32>, WireError> {
        let mut out = self.admit::<f32>(count, SCALAR_BYTES)?;
        for _ in 0..count {
            let value = self.f32()?;
            out.push(value);
        }
        Ok(out)
    }

    /// `count` `uint64_t`s — handles, sizes, addresses.
    ///
    /// # Errors
    /// As [`Decoder::repeat`].
    pub fn u64_array(&mut self, count: usize) -> Result<Vec<u64>, WireError> {
        let mut out = self.admit::<u64>(count, WIDE_BYTES)?;
        for _ in 0..count {
            let value = self.u64()?;
            out.push(value);
        }
        Ok(out)
    }

    /// Bound `count` elements of `wire_stride` bytes each against the stream
    /// and the budget, then reserve host storage for them. Charges the budget
    /// only once the reservation succeeded.
    fn admit<T>(&mut self, count: usize, wire_stride: usize) -> Result<Vec<T>, WireError> {
        self.guard()?;
        let left = self.remaining();
        let needed = count.checked_mul(wire_stride);
        if needed.is_none_or(|needed| needed > left) {
            let count = u64::try_from(count).unwrap_or(u64::MAX);
            let needed = needed
                .and_then(|needed| u64::try_from(needed).ok())
                .unwrap_or(u64::MAX);
            return self.fail(WireError::ArrayLongerThanStream {
                count,
                needed,
                left,
            });
        }

        let wanted = match count.checked_mul(size_of::<T>()) {
            Some(wanted) => wanted,
            None => {
                let budget = self.budget;
                return self.fail(WireError::AllocationBudgetExhausted {
                    wanted: usize::MAX,
                    budget,
                });
            }
        };
        if wanted > self.budget {
            let budget = self.budget;
            return self.fail(WireError::AllocationBudgetExhausted { wanted, budget });
        }

        let mut out: Vec<T> = Vec::new();
        if let Err(err) = out.try_reserve_exact(count) {
            let _: TryReserveError = err;
            return self.fail(WireError::OutOfMemory { wanted });
        }
        self.budget -= wanted;
        Ok(out)
    }

    // ---- structures ---------------------------------------------------------

    /// A pNext chain, walked as the wire orders it, with each link's `sType`
    /// offered to `visitor`.
    ///
    /// Returns the number of links decoded. The traversal is exactly the
    /// generated `vn_decode_*_pnext_temp`: an 8-byte presence marker, then the
    /// `sType`, then **the whole rest of the chain**, and only then this
    /// link's body — which is why `visitor.visit` is called on the way back
    /// out. An `sType` the visitor does not accept is fatal and has to be:
    /// nothing on the wire says how long the link is.
    ///
    /// A struct's own decoder calls this immediately after its `sType` and
    /// before any of its body.
    ///
    /// # Errors
    /// [`WireError::PnextChainTooDeep`], whatever `visitor` refused (typically
    /// [`WireError::UnknownPnextStype`]), or [`WireError::Truncated`].
    pub fn pnext_chain<V: PnextVisitor<'a>>(
        &mut self,
        visitor: &mut V,
    ) -> Result<usize, WireError> {
        if self.depth > MAX_PNEXT_DEPTH {
            return self.fail(WireError::PnextChainTooDeep { parent: V::PARENT });
        }
        if !self.simple_pointer()? {
            return Ok(0);
        }
        let stype = self.structure_type()?;

        self.depth = self.depth.saturating_add(1);
        let rest = self.pnext_chain(visitor);
        self.depth = self.depth.saturating_sub(1);
        let rest = rest?;

        let body = visitor.visit(stype, self);
        self.absorb(body)?;
        Ok(rest.saturating_add(1))
    }

    /// The pNext chain of a struct that accepts no extensions at all: present
    /// is fatal, absent consumes its eight bytes.
    ///
    /// # Errors
    /// [`WireError::UnknownPnextStype`] for any link at all.
    pub fn empty_pnext_chain(&mut self, parent: &'static str) -> Result<(), WireError> {
        if self.simple_pointer()? {
            let stype = self.structure_type()?;
            return self.fail(WireError::UnknownPnextStype { parent, stype });
        }
        Ok(())
    }

    // ---- framing ------------------------------------------------------------

    /// A command header: `u32` opcode then `u32` flags, eight bytes.
    ///
    /// # Errors
    /// As [`Decoder::u32`].
    pub fn command_header(&mut self) -> Result<CommandHeader, WireError> {
        let opcode = self.u32()?;
        let flags = self.u32()?;
        Ok(CommandHeader { opcode, flags })
    }

    /// A reply header: the `u32` opcode, **four** bytes, with no flags after
    /// it. See [`REPLY_HEADER_BYTES`].
    ///
    /// # Errors
    /// As [`Decoder::u32`].
    pub fn reply_header(&mut self) -> Result<u32, WireError> {
        self.u32()
    }
}

/// An encoder that builds one reply into an owned buffer.
///
/// Mirrors [`Decoder`] primitive for primitive, including the stickiness: a
/// write that fails poisons the encoder, and [`Encoder::finish`] refuses to
/// hand out a half-written reply. A reply that is short by four bytes is
/// exactly as bad as a command that is — the guest's reply decoder has no
/// length field either.
#[derive(Debug)]
pub struct Encoder {
    buf: Vec<u8>,
    limit: usize,
    fatal: Option<WireError>,
}

impl Default for Encoder {
    fn default() -> Self {
        Self::new()
    }
}

impl Encoder {
    /// An empty encoder with the protocol's maximum reply length.
    #[must_use]
    pub fn new() -> Self {
        Self::with_limit(MAX_TEMP_ALLOC_BYTES)
    }

    /// An empty encoder that refuses to grow past `limit` bytes. A caller that
    /// knows how much reply shmem the guest provided should say so here.
    #[must_use]
    pub fn with_limit(limit: usize) -> Self {
        Self {
            buf: Vec::new(),
            limit,
            fatal: None,
        }
    }

    /// Bytes written so far.
    #[must_use]
    pub fn len(&self) -> usize {
        self.buf.len()
    }

    /// Whether nothing has been written yet.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.buf.is_empty()
    }

    /// Whether a write has failed.
    #[must_use]
    pub fn is_fatal(&self) -> bool {
        self.fatal.is_some()
    }

    /// The first refusal, if there was one.
    #[must_use]
    pub fn fatal_error(&self) -> Option<WireError> {
        self.fatal
    }

    /// What has been written, whole or not. For tests and for logging a reply
    /// that could not be completed; [`Encoder::finish`] is what a caller with
    /// a guest to answer uses.
    #[must_use]
    pub fn as_bytes(&self) -> &[u8] {
        &self.buf
    }

    /// The finished reply.
    ///
    /// # Errors
    /// The first refusal, if any write failed. A partial reply is never
    /// returned: handing one to the guest desynchronises its reply decoder
    /// just as surely as a bad command desynchronises ours.
    pub fn finish(self) -> Result<Vec<u8>, WireError> {
        match self.fatal {
            Some(err) => Err(err),
            None => Ok(self.buf),
        }
    }

    fn fail<T>(&mut self, err: WireError) -> Result<T, WireError> {
        self.fatal.get_or_insert(err);
        Err(err)
    }

    fn absorb<T>(&mut self, result: Result<T, WireError>) -> Result<T, WireError> {
        match result {
            Ok(value) => Ok(value),
            Err(err) => {
                self.fatal.get_or_insert(err);
                Err(err)
            }
        }
    }

    /// Append `bytes`, refusing rather than aborting if the host cannot hold
    /// them.
    fn push(&mut self, bytes: &[u8]) -> Result<(), WireError> {
        if self.fatal.is_some() {
            return Err(WireError::Poisoned);
        }
        let wanted = match self.buf.len().checked_add(bytes.len()) {
            Some(wanted) => wanted,
            None => {
                let limit = self.limit;
                return self.fail(WireError::ReplyTooLong {
                    wanted: usize::MAX,
                    limit,
                });
            }
        };
        if wanted > self.limit {
            let limit = self.limit;
            return self.fail(WireError::ReplyTooLong { wanted, limit });
        }
        if let Err(err) = self.buf.try_reserve(bytes.len()) {
            let _: TryReserveError = err;
            return self.fail(WireError::OutOfMemory {
                wanted: bytes.len(),
            });
        }
        self.buf.extend_from_slice(bytes);
        Ok(())
    }

    /// A `uint32_t`, four bytes.
    ///
    /// # Errors
    /// [`WireError::ReplyTooLong`], [`WireError::OutOfMemory`], or
    /// [`WireError::Poisoned`].
    pub fn u32(&mut self, value: u32) -> Result<(), WireError> {
        self.push(&value.to_le_bytes())
    }

    /// An `int32_t`, four bytes.
    ///
    /// # Errors
    /// As [`Encoder::u32`].
    pub fn i32(&mut self, value: i32) -> Result<(), WireError> {
        self.push(&value.to_le_bytes())
    }

    /// A `VkStructureType`, four bytes.
    ///
    /// # Errors
    /// As [`Encoder::u32`].
    pub fn structure_type(&mut self, value: i32) -> Result<(), WireError> {
        self.i32(value)
    }

    /// A `float`, four bytes.
    ///
    /// # Errors
    /// As [`Encoder::u32`].
    pub fn f32(&mut self, value: f32) -> Result<(), WireError> {
        self.push(&value.to_le_bytes())
    }

    /// A `VkFlags`, four bytes.
    ///
    /// # Errors
    /// As [`Encoder::u32`].
    pub fn flags(&mut self, value: u32) -> Result<(), WireError> {
        self.u32(value)
    }

    /// A `VkBool32`, four bytes.
    ///
    /// # Errors
    /// As [`Encoder::u32`].
    pub fn bool32(&mut self, value: u32) -> Result<(), WireError> {
        self.u32(value)
    }

    /// A `uint16_t`: two bytes of value in a four-byte slot. The pad is
    /// written as zero — the reference leaves whatever was in the reply buffer
    /// there, and the decoder ignores it either way, but a deterministic reply
    /// is worth more than bug-compatibility with uninitialised memory.
    ///
    /// # Errors
    /// As [`Encoder::u32`].
    pub fn u16(&mut self, value: u16) -> Result<(), WireError> {
        let [low, high] = value.to_le_bytes();
        self.push(&[low, high, 0, 0])
    }

    /// A `uint8_t`: one byte of value in a four-byte slot, padded with zeros.
    ///
    /// # Errors
    /// As [`Encoder::u32`].
    pub fn u8(&mut self, value: u8) -> Result<(), WireError> {
        self.push(&[value, 0, 0, 0])
    }

    /// A `uint64_t`, eight bytes, at whatever offset the stream has reached.
    ///
    /// # Errors
    /// As [`Encoder::u32`].
    pub fn u64(&mut self, value: u64) -> Result<(), WireError> {
        self.push(&value.to_le_bytes())
    }

    /// An `int64_t`, eight bytes.
    ///
    /// # Errors
    /// As [`Encoder::u32`].
    pub fn i64(&mut self, value: i64) -> Result<(), WireError> {
        self.push(&value.to_le_bytes())
    }

    /// A `size_t`, eight bytes.
    ///
    /// # Errors
    /// As [`Encoder::u32`].
    pub fn size(&mut self, value: u64) -> Result<(), WireError> {
        self.u64(value)
    }

    /// A `VkDeviceSize` or `VkDeviceAddress`, eight bytes.
    ///
    /// # Errors
    /// As [`Encoder::u32`].
    pub fn device_size(&mut self, value: u64) -> Result<(), WireError> {
        self.u64(value)
    }

    /// An object handle, eight bytes.
    ///
    /// # Errors
    /// As [`Encoder::u32`].
    pub fn handle(&mut self, value: u64) -> Result<(), WireError> {
        self.u64(value)
    }

    /// An optional-pointer marker: eight bytes, 1 or 0.
    ///
    /// # Errors
    /// As [`Encoder::u32`].
    pub fn simple_pointer(&mut self, present: bool) -> Result<(), WireError> {
        self.u64(u64::from(present))
    }

    /// A null `pAllocator`: eight zero bytes. The only value this side is
    /// allowed to send, and the only one [`Decoder::null_allocator`] accepts.
    ///
    /// # Errors
    /// As [`Encoder::u32`].
    pub fn null_allocator(&mut self) -> Result<(), WireError> {
        self.u64(0)
    }

    /// An 8-byte array length.
    ///
    /// # Errors
    /// As [`Encoder::u32`].
    pub fn array_size(&mut self, count: u64) -> Result<(), WireError> {
        self.u64(count)
    }

    /// Opaque bytes, padded to the next four-byte boundary with zeros.
    ///
    /// # Errors
    /// As [`Encoder::u32`].
    pub fn blob(&mut self, bytes: &[u8]) -> Result<(), WireError> {
        self.push(bytes)?;
        let pad = bytes.len() % GRANULE;
        if pad != 0 {
            self.push(&[0u8; GRANULE][..GRANULE - pad])?;
        }
        Ok(())
    }

    /// An optional string: its 8-byte length (the bytes *plus* the
    /// terminator), then the NUL-terminated bytes, padded.
    ///
    /// `None` writes the 8-byte zero and nothing else — the same shape
    /// [`Decoder::opt_string`] consumes, so the pair round-trips.
    ///
    /// # Errors
    /// As [`Encoder::u32`]. An embedded NUL is not an error here; it simply
    /// makes the decoded string shorter, exactly as C would.
    pub fn opt_string(&mut self, text: Option<&[u8]>) -> Result<(), WireError> {
        match text {
            None => self.array_size(0),
            Some(bytes) => {
                let len = match bytes.len().checked_add(1) {
                    Some(len) => len,
                    None => {
                        let limit = self.limit;
                        return self.fail(WireError::ReplyTooLong {
                            wanted: usize::MAX,
                            limit,
                        });
                    }
                };
                self.array_size(u64::try_from(len).unwrap_or(u64::MAX))?;
                self.push(bytes)?;
                let padded = len.next_multiple_of(GRANULE);
                let tail = padded - bytes.len();
                self.push(&[0u8; 2 * GRANULE][..tail])
            }
        }
    }

    /// An optional array: **always** the 8-byte length, and the elements only
    /// when `count` is non-zero.
    ///
    /// The mirror image of [`Decoder::array`], and for the same reason: the
    /// null branch still owes its eight bytes, so the API does not let a
    /// caller reach the elements without having written them.
    ///
    /// # Errors
    /// As [`Encoder::u32`], or whatever `elements` returned.
    pub fn array(
        &mut self,
        count: u64,
        elements: impl FnOnce(&mut Self) -> Result<(), WireError>,
    ) -> Result<(), WireError> {
        self.array_size(count)?;
        if count == 0 {
            return Ok(());
        }
        let written = elements(self);
        self.absorb(written)
    }

    /// A command header: opcode then flags, eight bytes.
    ///
    /// # Errors
    /// As [`Encoder::u32`].
    pub fn command_header(&mut self, header: CommandHeader) -> Result<(), WireError> {
        self.u32(header.opcode)?;
        self.u32(header.flags)
    }

    /// A reply header: the opcode alone, **four** bytes. Writing the flags
    /// after it is the trap this method exists to make unavailable.
    ///
    /// # Errors
    /// As [`Encoder::u32`].
    pub fn reply_header(&mut self, opcode: u32) -> Result<(), WireError> {
        self.u32(opcode)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A decoder over bytes a test wrote, with a full budget.
    fn dec(bytes: &[u8]) -> Decoder<'_> {
        Decoder::new(bytes)
    }

    /// Bytes that are recognisably not zero and not padding, so a primitive
    /// that reads the wrong window produces an obviously wrong value.
    fn ramp(len: usize) -> Vec<u8> {
        (0..len)
            .map(|i| (i as u8).wrapping_mul(17).wrapping_add(3))
            .collect()
    }

    /// One primitive, named, with the bytes the protocol says it costs and a
    /// call that exercises it. Non-capturing closures so the table is a
    /// `const`-shaped array of function pointers.
    type Primitive = (
        &'static str,
        usize,
        fn(&mut Decoder<'_>) -> Result<(), WireError>,
    );

    /// Every primitive whose consumption is a fixed number of bytes, with that
    /// number. This table is the specification; the tests below hold it to it
    /// from several directions.
    fn fixed_primitives() -> Vec<Primitive> {
        vec![
            ("u32", 4, |d| d.u32().map(drop)),
            ("i32", 4, |d| d.i32().map(drop)),
            ("f32", 4, |d| d.f32().map(drop)),
            ("flags", 4, |d| d.flags().map(drop)),
            ("bool32", 4, |d| d.bool32().map(drop)),
            ("structure_type", 4, |d| d.structure_type().map(drop)),
            ("u16", 4, |d| d.u16().map(drop)),
            ("u8", 4, |d| d.u8().map(drop)),
            ("reply_header", 4, |d| d.reply_header().map(drop)),
            ("u64", 8, |d| d.u64().map(drop)),
            ("i64", 8, |d| d.i64().map(drop)),
            ("size", 8, |d| d.size().map(drop)),
            ("device_size", 8, |d| d.device_size().map(drop)),
            ("handle", 8, |d| d.handle().map(drop)),
            ("simple_pointer", 8, |d| d.simple_pointer().map(drop)),
            ("array_size_unchecked", 8, |d| {
                d.array_size_unchecked().map(drop)
            }),
            ("command_header", 8, |d| d.command_header().map(drop)),
        ]
    }

    // ---- consumption --------------------------------------------------------

    #[test]
    fn every_primitive_consumes_exactly_the_bytes_the_protocol_says() {
        let bytes = ramp(64);
        for (name, width, op) in fixed_primitives() {
            let mut d = dec(&bytes);
            op(&mut d).unwrap_or_else(|err| panic!("{name} failed on 64 good bytes: {err}"));
            assert_eq!(d.position(), width, "{name} moved the cursor wrongly");
            assert_eq!(d.remaining(), bytes.len() - width, "{name}");
            assert!(!d.is_fatal(), "{name} went fatal on good bytes");

            // Twice in a row: the second one starts exactly where the first
            // stopped, which is the property a desynchronising primitive
            // breaks.
            let mut d = dec(&bytes);
            op(&mut d).expect("first");
            op(&mut d).expect("second");
            assert_eq!(d.position(), width * 2, "{name} is not repeatable");
        }
    }

    #[test]
    fn a_scalar_narrower_than_four_bytes_still_costs_four() {
        // The value comes out of the low bytes of the slot and the rest is
        // skipped, not read. `vn_decode_uint8_t` reads 1 of 4, `uint16_t` 2.
        let bytes = [0xaa, 0xbb, 0xcc, 0xdd, 0x11, 0x22, 0x33, 0x44];
        let mut d = dec(&bytes);
        assert_eq!(d.u8().expect("u8"), 0xaa);
        assert_eq!(d.position(), 4);
        assert_eq!(d.u16().expect("u16"), 0x2211);
        assert_eq!(d.position(), 8);
    }

    #[test]
    fn a_64_bit_field_at_a_4_mod_8_offset_reads_the_right_value() {
        // The protocol is 4-byte granular with no inter-field padding, so this
        // is the normal case and not an edge one: a u32 then a u64 puts the
        // u64 at offset 4.
        let mut enc = Encoder::new();
        enc.u32(0xdead_beef).expect("u32");
        enc.u64(0x0123_4567_89ab_cdef).expect("u64");
        enc.u32(0xfeed_face).expect("u32");
        enc.u64(0xfedc_ba98_7654_3210).expect("u64");
        let bytes = enc.finish().expect("finish");
        assert_eq!(bytes.len(), 4 + 8 + 4 + 8);

        let mut d = dec(&bytes);
        assert_eq!(d.u32().expect("a"), 0xdead_beef);
        assert_eq!(d.position() % 8, 4, "the fixture must be 4-mod-8 here");
        assert_eq!(d.u64().expect("b"), 0x0123_4567_89ab_cdef);
        assert_eq!(d.u32().expect("c"), 0xfeed_face);
        assert_eq!(d.position() % 8, 0);
        assert_eq!(d.u64().expect("d"), 0xfedc_ba98_7654_3210);
        assert_eq!(d.remaining(), 0);

        // And at every 4-byte offset in a window, not just the one above.
        let mut buf = vec![0u8; 8];
        buf.extend_from_slice(&0x1122_3344_5566_7788u64.to_le_bytes());
        buf.extend_from_slice(&[0u8; 8]);
        for skip in (0..=8).step_by(4) {
            let mut window = vec![0u8; skip];
            window.extend_from_slice(&buf[8..16]);
            let mut d = dec(&window);
            for _ in 0..skip / 4 {
                d.u32().expect("skip");
            }
            assert_eq!(
                d.u64().expect("unaligned"),
                0x1122_3344_5566_7788,
                "wrong value at offset {skip}"
            );
        }
    }

    #[test]
    fn the_reply_header_is_half_the_command_header() {
        // Verified at `vn_protocol_renderer_device.h:16705-16709`: a reply
        // encodes only `VkCommandTypeEXT`. A host that echoes the flags back
        // desynchronises the guest's reply decoder on the very first reply.
        assert_eq!(COMMAND_HEADER_BYTES, 8);
        assert_eq!(REPLY_HEADER_BYTES, 4);

        let mut enc = Encoder::new();
        enc.reply_header(42).expect("reply header");
        assert_eq!(enc.len(), REPLY_HEADER_BYTES);

        let mut enc = Encoder::new();
        enc.command_header(CommandHeader {
            opcode: 42,
            flags: COMMAND_GENERATE_REPLY,
        })
        .expect("command header");
        assert_eq!(enc.len(), COMMAND_HEADER_BYTES);
        let bytes = enc.finish().expect("finish");

        let mut d = dec(&bytes);
        let header = d.command_header().expect("decode");
        assert_eq!(header.opcode, 42);
        assert!(header.wants_reply());
        assert_eq!(d.position(), 8);

        let mut d = dec(&bytes);
        assert_eq!(d.reply_header().expect("decode"), 42);
        assert_eq!(d.position(), 4, "a reply header must not eat the flags");
    }

    // ---- truncation ---------------------------------------------------------

    #[test]
    fn truncation_at_every_boundary_is_a_typed_refusal_and_never_a_panic() {
        let bytes = ramp(16);
        for (name, width, op) in fixed_primitives() {
            for short in 0..width {
                let mut d = dec(&bytes[..short]);
                let err = match op(&mut d) {
                    Ok(()) => panic!("{name} accepted a {short}-byte stream"),
                    Err(err) => err,
                };
                assert!(
                    matches!(err, WireError::Truncated { .. }),
                    "{name} at {short}: {err}"
                );
                assert!(d.is_fatal(), "{name} at {short} did not go fatal");
                // A short read must not move the cursor: a half-consumed field
                // is worse than none.
                assert!(
                    d.position() <= short,
                    "{name} at {short} moved the cursor past the data"
                );
            }
        }
    }

    #[test]
    fn a_composite_truncated_mid_field_leaves_the_cursor_where_it_was() {
        // A command header is two u32s. Six bytes is enough for the opcode and
        // not the flags; the opcode must not have been silently consumed into
        // a value the caller can see.
        let bytes = ramp(6);
        let mut d = dec(&bytes);
        assert!(d.command_header().is_err());
        assert!(d.is_fatal());
        assert_eq!(
            d.position(),
            4,
            "the opcode was consumed, the flags were not"
        );
        // and nothing can be read afterwards regardless.
        assert_eq!(d.u32(), Err(WireError::Poisoned));
    }

    #[test]
    fn a_blob_truncated_in_its_padding_is_still_refused() {
        // Five bytes of blob cost eight on the wire. Seven bytes of stream is
        // enough for the value and not the padding, and consuming the value
        // anyway would desynchronise by one byte.
        let bytes = ramp(7);
        let mut d = dec(&bytes);
        let err = d.blob(5).expect_err("a blob without its padding");
        assert!(
            matches!(
                err,
                WireError::Truncated {
                    needed: 8,
                    left: 7,
                    ..
                }
            ),
            "{err}"
        );
        assert!(d.is_fatal());

        let bytes = ramp(8);
        let mut d = dec(&bytes);
        assert_eq!(d.blob(5).expect("blob").len(), 5);
        assert_eq!(d.position(), 8);
    }

    // ---- arrays -------------------------------------------------------------

    #[test]
    fn a_null_array_still_consumes_its_eight_byte_length() {
        // The single easiest way to desynchronise a Venus stream.
        // `vn_protocol_renderer_device.h:688-703`: the `else` branch — the one
        // that produces a NULL pointer — calls `vn_decode_array_size` purely
        // for its side effect of moving the cursor eight bytes. Miss it and
        // every command after this one in the batch is garbage that still
        // parses.
        let mut enc = Encoder::new();
        enc.u32(0).expect("count"); // the element count in the body: zero
        enc.array(0, |_| unreachable!("a zero-length array has no elements"))
            .expect("null array");
        enc.u32(0x5a5a_5a5a).expect("the field after the array");
        let bytes = enc.finish().expect("finish");

        // 4 for the count, 8 for the length that "is not there", 4 for the
        // next field. The eight in the middle are the whole point.
        assert_eq!(bytes.len(), 4 + 8 + 4);

        let mut d = dec(&bytes);
        let count = u64::from(d.u32().expect("count"));
        let elements = d
            .array(count, |d, n| d.u32_array(n))
            .expect("null array decodes");
        assert_eq!(elements, None, "a zero length means a null array");
        assert_eq!(d.position(), 12, "the null branch owes its eight bytes");
        assert_eq!(
            d.u32().expect("the field after the array"),
            0x5a5a_5a5a,
            "the stream desynchronised across a null array"
        );
        assert_eq!(d.remaining(), 0);
    }

    #[test]
    fn a_non_null_array_consumes_its_length_and_then_its_elements() {
        let mut enc = Encoder::new();
        enc.u32(3).expect("count");
        enc.array(3, |e| {
            for value in [7u32, 8, 9] {
                e.u32(value)?;
            }
            Ok(())
        })
        .expect("array");
        enc.u32(0x5a5a_5a5a).expect("sentinel");
        let bytes = enc.finish().expect("finish");
        assert_eq!(bytes.len(), 4 + 8 + 12 + 4);

        let mut d = dec(&bytes);
        let count = u64::from(d.u32().expect("count"));
        let values = d.array(count, |d, n| d.u32_array(n)).expect("array");
        assert_eq!(values.as_deref(), Some(&[7u32, 8, 9][..]));
        assert_eq!(d.position(), 4 + 8 + 12);
        assert_eq!(d.u32().expect("sentinel"), 0x5a5a_5a5a);
    }

    #[test]
    fn an_array_length_that_disagrees_with_its_count_is_fatal() {
        // `vn_decode_array_size` sets fatal and returns zero on a mismatch;
        // returning zero would take the null branch and desynchronise, so
        // here it is simply the end of the stream.
        let mut enc = Encoder::new();
        enc.u32(3).expect("count says three");
        enc.array_size(2).expect("length says two");
        enc.u32(1).expect("e");
        enc.u32(2).expect("e");
        let bytes = enc.finish().expect("finish");

        let mut d = dec(&bytes);
        let count = u64::from(d.u32().expect("count"));
        let err = d
            .array(count, |d, n| d.u32_array(n))
            .expect_err("a mismatch must not decode");
        assert_eq!(
            err,
            WireError::ArrayLengthMismatch {
                expected: 3,
                found: 2
            }
        );
        assert!(d.is_fatal());
    }

    #[test]
    fn a_null_array_whose_count_is_not_zero_is_fatal() {
        // The null branch's `vn_decode_array_size(dec, count)` cross-checks
        // too, so "count 4, length 0" is a contradiction and not a null array.
        let mut enc = Encoder::new();
        enc.array_size(0).expect("length");
        let bytes = enc.finish().expect("finish");

        let mut d = dec(&bytes);
        let err = d
            .array(4, |d, n| d.u32_array(n))
            .expect_err("count 4 with a null array");
        assert_eq!(
            err,
            WireError::ArrayLengthMismatch {
                expected: 4,
                found: 0
            }
        );
        assert!(d.is_fatal());
    }

    #[test]
    fn an_oversized_array_length_is_refused_before_anything_is_allocated() {
        // A guest that declares a hundred million elements behind sixteen
        // bytes of stream. No element on this wire is narrower than four
        // bytes, so this is arithmetic, not an allocation attempt.
        for count in [100_000_000u64, u64::from(u32::MAX), u64::MAX / 4, u64::MAX] {
            let mut enc = Encoder::new();
            enc.array_size(count).expect("length");
            enc.u32(1).expect("one lonely element");
            let bytes = enc.finish().expect("finish");

            let mut d = dec(&bytes);
            let err = d
                .array(count, |d, n| d.u32_array(n))
                .expect_err("an oversized array must be refused");
            assert!(
                matches!(err, WireError::ArrayLongerThanStream { .. }),
                "{count}: {err}"
            );
            assert!(d.is_fatal());
        }
    }

    #[test]
    fn an_array_is_bounded_by_the_stream_before_it_is_bounded_by_the_budget() {
        // 16 elements' worth of stream, 17 declared: the stream bound bites
        // first and nothing is reserved.
        let bytes = ramp(4 + 64);
        let mut d = dec(&bytes);
        d.u32().expect("skip");
        let before = d.alloc_budget();
        let err = d.repeat(17, |d| d.u32()).expect_err("17 into 16");
        assert!(
            matches!(err, WireError::ArrayLongerThanStream { .. }),
            "{err}"
        );
        assert_eq!(
            d.alloc_budget(),
            before,
            "a refused array must cost nothing"
        );
    }

    #[test]
    fn the_allocation_budget_is_spent_across_a_command_and_can_be_reset() {
        let bytes = vec![0u8; 4096];
        let mut d = Decoder::with_alloc_budget(&bytes, 64);
        assert_eq!(d.alloc_budget(), 64);
        let first = d.u32_array(8).expect("32 bytes of u32");
        assert_eq!(first.len(), 8);
        assert_eq!(d.alloc_budget(), 32);
        d.u32_array(8).expect("another 32");
        assert_eq!(d.alloc_budget(), 0);

        let err = d.u32_array(1).expect_err("nothing left");
        assert_eq!(
            err,
            WireError::AllocationBudgetExhausted {
                wanted: 4,
                budget: 0
            }
        );
        assert!(d.is_fatal());

        // Resetting the budget is what the dispatcher does between commands.
        // It must not resurrect a fatal stream.
        d.reset_alloc_budget();
        assert_eq!(
            d.alloc_budget(),
            64,
            "a reset restores the budget this decoder was BUILT with, not the              protocol maximum — otherwise `with_alloc_budget` would be a              promise that expired after one command"
        );
        assert!(d.is_fatal(), "a budget reset is not a fatal reset");
        assert_eq!(d.u32(), Err(WireError::Poisoned));
    }

    /// The bug the test above used to enshrine: a decoder built with a smaller
    /// budget got the protocol maximum back on the first reset, so a
    /// dispatcher silently widened every limit a caller had set. Nothing in
    /// this crate passes a custom budget yet, which is exactly why it needs a
    /// test — the first caller to try would have found it the hard way.
    #[test]
    fn a_custom_allocation_budget_survives_every_reset() {
        let bytes = vec![0u8; 256];
        let mut d = Decoder::with_alloc_budget(&bytes, 32);
        for command in 0..4 {
            assert_eq!(d.alloc_budget(), 32, "command {command} starts at 32");
            d.u32_array(8).expect("32 bytes of array");
            assert_eq!(d.alloc_budget(), 0, "command {command} spends it all");
            d.reset_alloc_budget();
        }
        assert_ne!(d.alloc_budget(), MAX_TEMP_ALLOC_BYTES);
    }

    /// And the default really is the maximum, so the fix above did not quietly
    /// shrink the ordinary path.
    #[test]
    fn a_plain_decoder_resets_to_the_protocol_maximum() {
        let bytes = vec![0u8; 16];
        let mut d = Decoder::new(&bytes);
        d.u32_array(4).expect("16 bytes");
        assert_eq!(d.alloc_budget(), MAX_TEMP_ALLOC_BYTES - 16);
        d.reset_alloc_budget();
        assert_eq!(d.alloc_budget(), MAX_TEMP_ALLOC_BYTES);
    }

    #[test]
    fn a_u64_array_is_charged_its_real_width_on_both_sides() {
        // Eight bytes per element on the wire *and* eight in the host Vec, so
        // a count that fits the wire exactly also fits the budget exactly.
        let bytes = vec![0u8; 80];
        let mut d = Decoder::with_alloc_budget(&bytes, 80);
        d.u64_array(10).expect("ten u64");
        assert_eq!(d.position(), 80);
        assert_eq!(d.alloc_budget(), 0);

        let bytes = vec![0u8; 80];
        let mut d = dec(&bytes);
        let err = d.u64_array(11).expect_err("eleven u64 do not fit 80 bytes");
        assert!(
            matches!(err, WireError::ArrayLongerThanStream { .. }),
            "{err}"
        );
    }

    #[test]
    fn peeking_an_array_size_does_not_consume_it() {
        let mut enc = Encoder::new();
        enc.array_size(0x1234).expect("size");
        let bytes = enc.finish().expect("finish");
        let mut d = dec(&bytes);
        assert_eq!(d.peek_array_size().expect("peek"), 0x1234);
        assert_eq!(d.position(), 0);
        assert_eq!(d.array_size_unchecked().expect("decode"), 0x1234);
        assert_eq!(d.position(), 8);

        // Peeking past the end is fatal, as `vkr_cs_decoder_peek_internal` is.
        let short = ramp(7);
        let mut d = dec(&short);
        assert!(d.peek_array_size().is_err());
        assert!(d.is_fatal());
    }

    // ---- blobs and strings --------------------------------------------------

    #[test]
    fn a_blob_is_padded_to_four_bytes_and_its_padding_is_never_read() {
        for len in 0usize..=17 {
            let padded = len.next_multiple_of(4);
            let mut bytes = ramp(len);
            // Padding a malicious guest filled with something other than zero.
            bytes.resize(padded, 0xff);
            bytes.extend_from_slice(&0x5a5a_5a5au32.to_le_bytes());

            let mut d = dec(&bytes);
            let blob = d.blob(len).unwrap_or_else(|e| panic!("len {len}: {e}"));
            assert_eq!(blob, &ramp(len)[..], "len {len}: wrong bytes");
            assert_eq!(blob.len(), len, "len {len}: the padding leaked in");
            assert_eq!(d.position(), padded, "len {len}: wrong consumption");
            assert_eq!(
                d.u32().expect("sentinel"),
                0x5a5a_5a5a,
                "len {len}: the stream desynchronised across the padding"
            );
        }
    }

    #[test]
    fn a_string_is_cut_at_its_terminator_and_padded_like_a_blob() {
        let mut enc = Encoder::new();
        enc.opt_string(Some(b"VK_KHR_swapchain")).expect("string");
        enc.u32(0x5a5a_5a5a).expect("sentinel");
        let bytes = enc.finish().expect("finish");
        // 8 for the length, 17 bytes of NUL-terminated text padded to 20.
        assert_eq!(bytes.len(), 8 + 20 + 4);

        let mut d = dec(&bytes);
        assert_eq!(
            d.opt_string().expect("string"),
            Some(&b"VK_KHR_swapchain"[..])
        );
        assert_eq!(d.position(), 28);
        assert_eq!(d.u32().expect("sentinel"), 0x5a5a_5a5a);

        // A null string is eight bytes of zero and nothing else.
        let mut enc = Encoder::new();
        enc.opt_string(None).expect("null string");
        enc.u32(0x5a5a_5a5a).expect("sentinel");
        let bytes = enc.finish().expect("finish");
        assert_eq!(bytes.len(), 8 + 4);
        let mut d = dec(&bytes);
        assert_eq!(d.opt_string().expect("null string"), None);
        assert_eq!(d.position(), 8);
        assert_eq!(d.u32().expect("sentinel"), 0x5a5a_5a5a);
    }

    #[test]
    fn a_string_without_a_terminator_is_refused_rather_than_truncated() {
        // The reference overwrites the last byte with NUL, which turns
        // "VK_KHR_swapchainX" into a different, valid-looking name.
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&4u64.to_le_bytes());
        bytes.extend_from_slice(b"abcd");
        let mut d = dec(&bytes);
        let err = d.opt_string().expect_err("unterminated");
        assert_eq!(err, WireError::UnterminatedString { len: 4 });
        assert!(d.is_fatal());
    }

    #[test]
    fn a_zero_length_string_is_fatal_but_a_null_one_is_not() {
        // A declared length of zero is not the same as a null pointer: the
        // reference sets fatal for the former (`vn_decode_char_array`'s `else`)
        // and returns NULL for the latter. Only one of those can be spelled
        // through `opt_string`, so the distinction is exercised through the
        // primitive.
        let bytes = ramp(8);
        let mut d = dec(&bytes);
        assert_eq!(d.string_bytes(0), Err(WireError::EmptyString));
        assert!(d.is_fatal());

        let bytes = 0u64.to_le_bytes();
        let mut d = dec(&bytes);
        assert_eq!(d.opt_string().expect("null"), None);
        assert!(!d.is_fatal());
    }

    #[test]
    fn an_embedded_nul_cuts_a_string_short_without_desynchronising() {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&8u64.to_le_bytes());
        bytes.extend_from_slice(b"ab\0cdef\0");
        bytes.extend_from_slice(&0x5a5a_5a5au32.to_le_bytes());
        let mut d = dec(&bytes);
        assert_eq!(d.opt_string().expect("string"), Some(&b"ab"[..]));
        assert_eq!(d.position(), 16, "the whole declared length is consumed");
        assert_eq!(d.u32().expect("sentinel"), 0x5a5a_5a5a);
    }

    #[test]
    fn a_non_utf8_string_is_refused_only_when_the_caller_asks_for_utf8() {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&4u64.to_le_bytes());
        bytes.extend_from_slice(&[0xff, 0xfe, 0xfd, 0x00]);

        let mut d = dec(&bytes);
        assert_eq!(
            d.opt_string().expect("raw bytes are fine"),
            Some(&[0xffu8, 0xfe, 0xfd][..])
        );

        let mut d = dec(&bytes);
        assert_eq!(d.opt_str(), Err(WireError::NonUtf8String));
        assert!(d.is_fatal());
    }

    // ---- pAllocator ---------------------------------------------------------

    #[test]
    fn a_non_null_pallocator_is_fatal_and_a_null_one_costs_eight_bytes() {
        let mut enc = Encoder::new();
        enc.null_allocator().expect("null");
        enc.u32(0x5a5a_5a5a).expect("sentinel");
        let bytes = enc.finish().expect("finish");
        assert_eq!(bytes.len(), 12);

        let mut d = dec(&bytes);
        d.null_allocator().expect("a null allocator");
        assert_eq!(d.position(), 8);
        assert_eq!(d.u32().expect("sentinel"), 0x5a5a_5a5a);

        for value in [1u64, 0x1000, u64::MAX] {
            let bytes = value.to_le_bytes();
            let mut d = dec(&bytes);
            assert_eq!(d.null_allocator(), Err(WireError::AllocatorNotNull(value)));
            assert!(d.is_fatal());
        }
    }

    #[test]
    fn an_optional_pointer_is_any_non_zero_value() {
        // `vn_decode_simple_pointer` *is* `vn_decode_array_size_unchecked`, so
        // it cannot be stricter than "non-zero" without judging array lengths
        // the same way.
        for (raw, present) in [(0u64, false), (1, true), (2, true), (u64::MAX, true)] {
            let bytes = raw.to_le_bytes();
            let mut d = dec(&bytes);
            assert_eq!(d.simple_pointer().expect("pointer"), present, "{raw:#x}");
            assert_eq!(d.position(), 8);
        }
    }

    // ---- pNext --------------------------------------------------------------

    /// A whitelist of two sTypes, recording what it saw and each link's body.
    #[derive(Default)]
    struct TwoStypes {
        seen: Vec<(i32, u32)>,
    }

    impl<'a> PnextVisitor<'a> for TwoStypes {
        const PARENT: &'static str = "VkTestCreateInfo";

        fn visit(&mut self, stype: i32, dec: &mut Decoder<'a>) -> Result<(), WireError> {
            match stype {
                1_000_001 | 1_000_002 => {
                    let body = dec.u32()?;
                    self.seen.push((stype, body));
                    Ok(())
                }
                other => Err(dec.set_fatal(Self::unknown(other))),
            }
        }
    }

    /// Encodes one pNext link: present marker, sType, then the rest of the
    /// chain, then this link's one-word body. The order is the whole point.
    fn encode_chain(enc: &mut Encoder, links: &[(i32, u32)]) -> Result<(), WireError> {
        match links.split_first() {
            None => enc.simple_pointer(false),
            Some(((stype, body), rest)) => {
                enc.simple_pointer(true)?;
                enc.structure_type(*stype)?;
                encode_chain(enc, rest)?;
                enc.u32(*body)
            }
        }
    }

    #[test]
    fn a_struct_serialises_its_pnext_chain_before_its_own_body() {
        // `vn_protocol_renderer_device.h:706-716`: sType, then pNext, then
        // self. And within the chain, each link's pNext comes before *its*
        // body too, so the bodies unwind in reverse order.
        let mut enc = Encoder::new();
        enc.structure_type(999).expect("parent sType");
        encode_chain(&mut enc, &[(1_000_001, 0xaa), (1_000_002, 0xbb)]).expect("chain");
        enc.u32(0x5a5a_5a5a).expect("parent body");
        let bytes = enc.finish().expect("finish");

        // parent sType 4; two links of marker 8 + sType 4; the terminating
        // marker 8; the two link bodies 4 each; the parent's body 4.
        assert_eq!(bytes.len(), 4 + 12 + 12 + 8 + 4 + 4 + 4);

        let mut d = dec(&bytes);
        assert_eq!(d.structure_type().expect("parent sType"), 999);
        let mut visitor = TwoStypes::default();
        assert_eq!(d.pnext_chain(&mut visitor).expect("chain"), 2);
        // Innermost body first: the recursion decodes the rest of the chain
        // before this link's own fields.
        assert_eq!(visitor.seen, vec![(1_000_002, 0xbb), (1_000_001, 0xaa)]);
        assert_eq!(
            d.u32().expect("parent body"),
            0x5a5a_5a5a,
            "the parent's body must follow the whole chain"
        );
        assert_eq!(d.remaining(), 0);
    }

    #[test]
    fn an_absent_pnext_chain_still_costs_its_eight_byte_marker() {
        let mut enc = Encoder::new();
        encode_chain(&mut enc, &[]).expect("empty chain");
        enc.u32(0x5a5a_5a5a).expect("body");
        let bytes = enc.finish().expect("finish");
        assert_eq!(bytes.len(), 8 + 4);

        let mut d = dec(&bytes);
        let mut visitor = TwoStypes::default();
        assert_eq!(d.pnext_chain(&mut visitor).expect("chain"), 0);
        assert_eq!(d.position(), 8);
        assert_eq!(d.u32().expect("body"), 0x5a5a_5a5a);
    }

    #[test]
    fn an_unknown_pnext_stype_is_fatal_because_nothing_says_how_long_it_is() {
        let mut enc = Encoder::new();
        encode_chain(&mut enc, &[(1_000_001, 0xaa), (0x7fff_0000, 0xbb)]).expect("chain");
        let bytes = enc.finish().expect("finish");

        let mut d = dec(&bytes);
        let mut visitor = TwoStypes::default();
        let err = d.pnext_chain(&mut visitor).expect_err("unknown sType");
        assert_eq!(
            err,
            WireError::UnknownPnextStype {
                parent: "VkTestCreateInfo",
                stype: 0x7fff_0000
            }
        );
        assert!(d.is_fatal());
        assert_eq!(d.u32(), Err(WireError::Poisoned));
    }

    #[test]
    fn a_struct_that_accepts_no_extensions_refuses_every_link() {
        let mut enc = Encoder::new();
        encode_chain(&mut enc, &[]).expect("empty");
        let bytes = enc.finish().expect("finish");
        let mut d = dec(&bytes);
        d.empty_pnext_chain("VkPlainInfo").expect("no chain");
        assert_eq!(d.position(), 8);

        let mut enc = Encoder::new();
        encode_chain(&mut enc, &[(1_000_001, 0)]).expect("one link");
        let bytes = enc.finish().expect("finish");
        let mut d = dec(&bytes);
        assert_eq!(
            d.empty_pnext_chain("VkPlainInfo"),
            Err(WireError::UnknownPnextStype {
                parent: "VkPlainInfo",
                stype: 1_000_001
            })
        );
    }

    #[test]
    fn a_deep_pnext_chain_is_refused_rather_than_recursed_off_the_stack() {
        // The reference recurses once per link with no cap at all. At twelve
        // bytes per link a 16 MiB ring buffer names over a million of them,
        // and the host has one stack.
        let links: Vec<(i32, u32)> = (0..MAX_PNEXT_DEPTH * 2).map(|i| (1_000_001, i)).collect();
        let mut enc = Encoder::new();
        encode_chain(&mut enc, &links).expect("deep chain");
        let bytes = enc.finish().expect("finish");

        let mut d = dec(&bytes);
        let mut visitor = TwoStypes::default();
        let err = d.pnext_chain(&mut visitor).expect_err("too deep");
        assert_eq!(
            err,
            WireError::PnextChainTooDeep {
                parent: "VkTestCreateInfo"
            }
        );
        assert!(d.is_fatal());

        // Exactly at the cap is still accepted, so the bound is the documented
        // one and not one link off it.
        let links: Vec<(i32, u32)> = (0..MAX_PNEXT_DEPTH).map(|i| (1_000_001, i)).collect();
        let mut enc = Encoder::new();
        encode_chain(&mut enc, &links).expect("chain at the cap");
        let bytes = enc.finish().expect("finish");
        let mut d = dec(&bytes);
        let mut visitor = TwoStypes::default();
        assert_eq!(
            d.pnext_chain(&mut visitor).expect("at the cap"),
            MAX_PNEXT_DEPTH as usize
        );
    }

    // ---- stickiness ---------------------------------------------------------

    #[test]
    fn a_fatal_decoder_stays_fatal_and_yields_nothing_afterwards() {
        // The reference keeps decoding after a refusal and zero-fills, so a
        // caller that misses one `if` gets a plausible zero. Here the flag is
        // checked on the way *in* to every operation.
        let bytes = ramp(64);
        let mut d = dec(&bytes);
        d.u32().expect("a good read first");
        let first = d.set_fatal(WireError::AllocatorNotNull(7));
        assert_eq!(first, WireError::AllocatorNotNull(7));
        assert_eq!(d.fatal_error(), Some(WireError::AllocatorNotNull(7)));

        let at = d.position();
        for (name, _, op) in fixed_primitives() {
            assert_eq!(op(&mut d), Err(WireError::Poisoned), "{name}");
            assert_eq!(d.position(), at, "{name} moved a dead cursor");
        }
        assert_eq!(d.blob(4), Err(WireError::Poisoned));
        assert_eq!(d.string_bytes(4), Err(WireError::Poisoned));
        assert_eq!(d.opt_string(), Err(WireError::Poisoned));
        assert_eq!(d.opt_str(), Err(WireError::Poisoned));
        assert_eq!(d.null_allocator(), Err(WireError::Poisoned));
        assert_eq!(d.array_size(0), Err(WireError::Poisoned));
        assert_eq!(d.peek_array_size(), Err(WireError::Poisoned));
        assert_eq!(d.u32_array(1), Err(WireError::Poisoned));
        assert_eq!(d.u64_array(1), Err(WireError::Poisoned));
        assert_eq!(d.f32_array(1), Err(WireError::Poisoned));
        assert_eq!(d.repeat(1, |d| d.u32()), Err(WireError::Poisoned));
        assert_eq!(d.array(0, |d, n| d.u32_array(n)), Err(WireError::Poisoned));
        assert_eq!(
            d.array_unchecked(|d, n| d.u32_array(n)),
            Err(WireError::Poisoned)
        );
        assert_eq!(d.empty_pnext_chain("X"), Err(WireError::Poisoned));
        let mut visitor = TwoStypes::default();
        assert_eq!(d.pnext_chain(&mut visitor), Err(WireError::Poisoned));

        // The original refusal is still the one on record.
        assert_eq!(d.fatal_error(), Some(WireError::AllocatorNotNull(7)));
        assert!(!d.fatal_error().is_some_and(WireError::is_poisoned));
    }

    #[test]
    fn the_first_refusal_is_the_one_kept() {
        let bytes = ramp(4);
        let mut d = dec(&bytes);
        let err = d.u64().expect_err("truncated");
        assert!(matches!(err, WireError::Truncated { .. }));
        d.set_fatal(WireError::EmptyString);
        assert_eq!(
            d.fatal_error(),
            Some(err),
            "a later refusal overwrote the first"
        );
    }

    #[test]
    fn an_error_out_of_an_array_element_poisons_the_stream() {
        // A caller-level refusal halfway through an array leaves the cursor
        // inside an element, which is exactly as unrecoverable as a truncation.
        let mut bytes = 3u64.to_le_bytes().to_vec();
        bytes.resize(64, 0);
        let mut d = dec(&bytes);
        let err = d
            .array_unchecked(|d, count| {
                assert_eq!(count, 3);
                d.u32()?;
                Err::<(), _>(WireError::EmptyString)
            })
            .expect_err("the closure refused");
        assert_eq!(err, WireError::EmptyString);
        assert!(d.is_fatal());
        assert_eq!(d.u32(), Err(WireError::Poisoned));
    }

    #[test]
    fn a_fatal_encoder_refuses_to_hand_out_a_partial_reply() {
        let mut enc = Encoder::with_limit(12);
        enc.reply_header(3).expect("header");
        enc.u32(1).expect("a");
        enc.u32(2).expect("b");
        let err = enc.u32(3).expect_err("past the limit");
        assert_eq!(
            err,
            WireError::ReplyTooLong {
                wanted: 16,
                limit: 12
            }
        );
        assert!(enc.is_fatal());
        assert_eq!(enc.u32(4), Err(WireError::Poisoned));
        assert_eq!(enc.len(), 12, "a refused write must not have landed");
        assert_eq!(enc.finish(), Err(err));
    }

    // ---- round trips --------------------------------------------------------

    #[test]
    fn every_primitive_round_trips_through_the_encoder() {
        let mut enc = Encoder::new();
        enc.u32(0xdead_beef).expect("u32");
        enc.i32(-42).expect("i32");
        enc.f32(-0.5).expect("f32");
        enc.flags(0x8000_0001).expect("flags");
        enc.bool32(1).expect("bool32");
        enc.structure_type(1_000_123).expect("stype");
        enc.u16(0xbeef).expect("u16");
        enc.u8(0x5a).expect("u8");
        enc.u64(0x0123_4567_89ab_cdef).expect("u64");
        enc.i64(-1).expect("i64");
        enc.size(1 << 40).expect("size");
        enc.device_size(4096).expect("device size");
        enc.handle(0xfeed_face_cafe_babe).expect("handle");
        enc.simple_pointer(true).expect("pointer");
        enc.simple_pointer(false).expect("null pointer");
        enc.null_allocator().expect("allocator");
        enc.array_size(3).expect("array size");
        enc.blob(&[1, 2, 3, 4, 5]).expect("blob");
        enc.opt_string(Some(b"venus")).expect("string");
        enc.opt_string(None).expect("null string");
        enc.command_header(CommandHeader {
            opcode: 7,
            flags: COMMAND_GENERATE_REPLY,
        })
        .expect("command header");
        enc.reply_header(7).expect("reply header");
        let bytes = enc.finish().expect("finish");

        let mut d = dec(&bytes);
        assert_eq!(d.u32().expect("u32"), 0xdead_beef);
        assert_eq!(d.i32().expect("i32"), -42);
        assert!((d.f32().expect("f32") + 0.5).abs() < f32::EPSILON);
        assert_eq!(d.flags().expect("flags"), 0x8000_0001);
        assert_eq!(d.bool32().expect("bool32"), 1);
        assert_eq!(d.structure_type().expect("stype"), 1_000_123);
        assert_eq!(d.u16().expect("u16"), 0xbeef);
        assert_eq!(d.u8().expect("u8"), 0x5a);
        assert_eq!(d.u64().expect("u64"), 0x0123_4567_89ab_cdef);
        assert_eq!(d.i64().expect("i64"), -1);
        assert_eq!(d.size().expect("size"), 1 << 40);
        assert_eq!(d.device_size().expect("device size"), 4096);
        assert_eq!(d.handle().expect("handle"), 0xfeed_face_cafe_babe);
        assert!(d.simple_pointer().expect("pointer"));
        assert!(!d.simple_pointer().expect("null pointer"));
        d.null_allocator().expect("allocator");
        assert_eq!(d.array_size(3).expect("array size"), 3);
        assert_eq!(d.blob(5).expect("blob"), &[1, 2, 3, 4, 5]);
        assert_eq!(d.opt_string().expect("string"), Some(&b"venus"[..]));
        assert_eq!(d.opt_string().expect("null string"), None);
        let header = d.command_header().expect("command header");
        assert_eq!(header.opcode, 7);
        assert!(header.wants_reply());
        assert_eq!(d.reply_header().expect("reply header"), 7);
        assert_eq!(d.remaining(), 0, "the round trip left bytes over");
        assert!(!d.is_fatal());
    }

    #[test]
    fn arrays_round_trip_in_both_shapes() {
        let mut enc = Encoder::new();
        enc.array(4, |e| {
            for value in [1u64, 2, 3, 4] {
                e.handle(value)?;
            }
            Ok(())
        })
        .expect("handles");
        enc.array(0, |_| unreachable!()).expect("null");
        enc.array(2, |e| {
            e.f32(1.5)?;
            e.f32(-2.5)
        })
        .expect("floats");
        let bytes = enc.finish().expect("finish");

        let mut d = dec(&bytes);
        assert_eq!(
            d.array(4, |d, n| d.u64_array(n))
                .expect("handles")
                .as_deref(),
            Some(&[1u64, 2, 3, 4][..])
        );
        assert_eq!(d.array(0, |d, n| d.u32_array(n)).expect("null"), None);
        let floats = d.array(2, |d, n| d.f32_array(n)).expect("floats");
        assert_eq!(floats.as_deref(), Some(&[1.5f32, -2.5][..]));
        assert_eq!(d.remaining(), 0);
    }

    // ---- sweeps -------------------------------------------------------------

    /// A deterministic generator, so a failure is reproducible from the seed
    /// printed in the assertion. No dependency, and no dev-dependency on a
    /// property-testing crate this workspace does not carry.
    struct Lcg(u64);

    impl Lcg {
        fn next_u64(&mut self) -> u64 {
            self.0 = self
                .0
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            self.0 ^ (self.0 >> 33)
        }

        fn below(&mut self, n: u64) -> u64 {
            self.next_u64() % n
        }
    }

    /// One primitive in a generated program: what to write, what to read back,
    /// and how many bytes it must cost.
    #[derive(Debug, Clone, Copy)]
    enum Step {
        U32(u32),
        I32(i32),
        U16(u16),
        U8(u8),
        U64(u64),
        Pointer(bool),
        ArraySize(u64),
        Blob(usize),
        NullString,
    }

    impl Step {
        fn wire_len(self) -> usize {
            match self {
                Self::U32(_) | Self::I32(_) | Self::U16(_) | Self::U8(_) => 4,
                Self::U64(_) | Self::Pointer(_) | Self::ArraySize(_) | Self::NullString => 8,
                Self::Blob(len) => len.next_multiple_of(4),
            }
        }
    }

    #[test]
    fn a_generated_program_round_trips_with_every_cursor_position_matching() {
        for seed in 0..64u64 {
            let mut rng = Lcg(seed.wrapping_mul(0x9e37_79b9_7f4a_7c15).wrapping_add(1));
            let mut program = Vec::new();
            for _ in 0..40 {
                let step = match rng.below(9) {
                    0 => Step::U32(rng.next_u64() as u32),
                    1 => Step::I32(rng.next_u64() as i32),
                    2 => Step::U16(rng.next_u64() as u16),
                    3 => Step::U8(rng.next_u64() as u8),
                    4 => Step::U64(rng.next_u64()),
                    5 => Step::Pointer(rng.below(2) == 1),
                    6 => Step::ArraySize(rng.below(1024)),
                    7 => Step::Blob(rng.below(19) as usize),
                    _ => Step::NullString,
                };
                program.push(step);
            }

            let mut enc = Encoder::new();
            let mut blobs = Vec::new();
            let mut expected_positions = Vec::new();
            let mut at = 0usize;
            for step in &program {
                match *step {
                    Step::U32(v) => enc.u32(v).expect("u32"),
                    Step::I32(v) => enc.i32(v).expect("i32"),
                    Step::U16(v) => enc.u16(v).expect("u16"),
                    Step::U8(v) => enc.u8(v).expect("u8"),
                    Step::U64(v) => enc.u64(v).expect("u64"),
                    Step::Pointer(v) => enc.simple_pointer(v).expect("pointer"),
                    Step::ArraySize(v) => enc.array_size(v).expect("array size"),
                    Step::Blob(len) => {
                        let payload = ramp(len);
                        let written = enc.blob(&payload);
                        blobs.push(payload);
                        written.expect("blob")
                    }
                    Step::NullString => enc.opt_string(None).expect("null string"),
                }
                at += step.wire_len();
                expected_positions.push(at);
            }
            let bytes = enc.finish().expect("finish");
            assert_eq!(bytes.len(), at, "seed {seed}: encoder length");

            let mut d = dec(&bytes);
            let mut blobs = blobs.into_iter();
            for (step, expected_at) in program.iter().zip(&expected_positions) {
                match *step {
                    Step::U32(v) => assert_eq!(d.u32().expect("u32"), v, "seed {seed}"),
                    Step::I32(v) => assert_eq!(d.i32().expect("i32"), v, "seed {seed}"),
                    Step::U16(v) => assert_eq!(d.u16().expect("u16"), v, "seed {seed}"),
                    Step::U8(v) => assert_eq!(d.u8().expect("u8"), v, "seed {seed}"),
                    Step::U64(v) => assert_eq!(d.u64().expect("u64"), v, "seed {seed}"),
                    Step::Pointer(v) => {
                        assert_eq!(d.simple_pointer().expect("pointer"), v, "seed {seed}");
                    }
                    Step::ArraySize(v) => {
                        assert_eq!(d.array_size(v).expect("array size"), v, "seed {seed}");
                    }
                    Step::Blob(len) => {
                        let expected = blobs.next().expect("a blob was recorded");
                        assert_eq!(d.blob(len).expect("blob"), &expected[..], "seed {seed}");
                    }
                    Step::NullString => {
                        assert_eq!(d.opt_string().expect("null string"), None, "seed {seed}");
                    }
                }
                assert_eq!(
                    d.position(),
                    *expected_at,
                    "seed {seed}: {step:?} desynchronised the stream"
                );
            }
            assert_eq!(d.remaining(), 0, "seed {seed}");
            assert!(!d.is_fatal(), "seed {seed}");
        }
    }

    #[test]
    fn hostile_bytes_driven_by_a_hostile_program_never_panic() {
        // Random bytes, random operations, random lengths: the only claims are
        // that nothing panics, the cursor never passes the end, and a stream
        // that has gone fatal never produces a value again.
        for seed in 0..512u64 {
            let mut rng = Lcg(seed.wrapping_mul(0xff51_afd7_ed55_8ccd).wrapping_add(7));
            let len = rng.below(48) as usize;
            let bytes: Vec<u8> = (0..len).map(|_| rng.next_u64() as u8).collect();
            let mut d = Decoder::with_alloc_budget(&bytes, rng.below(256) as usize);

            for _ in 0..24 {
                let was_fatal = d.is_fatal();
                let before = d.position();
                let outcome: Result<(), WireError> = match rng.below(14) {
                    0 => d.u32().map(drop),
                    1 => d.u64().map(drop),
                    2 => d.u8().map(drop),
                    3 => d.u16().map(drop),
                    4 => d.simple_pointer().map(drop),
                    5 => d.null_allocator(),
                    6 => d.blob(rng.below(64) as usize).map(drop),
                    7 => d.opt_string().map(drop),
                    8 => d.opt_str().map(drop),
                    9 => d.array_size(rng.below(8)).map(drop),
                    10 => d.u32_array(rng.below(4096) as usize).map(drop),
                    11 => d.u64_array(rng.below(4096) as usize).map(drop),
                    12 => d.array(rng.below(8), |d, n| d.u32_array(n)).map(drop),
                    _ => {
                        let mut visitor = TwoStypes::default();
                        d.pnext_chain(&mut visitor).map(drop)
                    }
                };

                assert!(d.position() <= bytes.len(), "seed {seed}: cursor escaped");
                if was_fatal {
                    assert_eq!(
                        outcome,
                        Err(WireError::Poisoned),
                        "seed {seed}: a dead stream produced a value"
                    );
                    assert_eq!(d.position(), before, "seed {seed}: a dead cursor moved");
                }
                if outcome.is_err() {
                    assert!(d.is_fatal(), "seed {seed}: a refusal did not stick");
                }
            }
        }
    }

    #[test]
    fn a_hostile_program_cannot_talk_the_host_into_a_large_allocation() {
        // Every array shape a guest can name, with nothing behind it. The
        // budget must be untouched afterwards: nothing was allocated, so
        // nothing was charged.
        let bytes = ramp(16);
        for count in [
            1usize << 20,
            1 << 30,
            usize::MAX / 8,
            usize::MAX / 4,
            usize::MAX,
        ] {
            for shape in 0..4u8 {
                let mut d = dec(&bytes);
                let before = d.alloc_budget();
                let err = match shape {
                    0 => d.u32_array(count).map(drop).expect_err("u32 array"),
                    1 => d.u64_array(count).map(drop).expect_err("u64 array"),
                    2 => d.f32_array(count).map(drop).expect_err("f32 array"),
                    _ => d.repeat(count, |d| d.u32()).map(drop).expect_err("repeat"),
                };
                assert!(
                    matches!(err, WireError::ArrayLongerThanStream { .. }),
                    "{count}/{shape}: {err}"
                );
                assert_eq!(d.alloc_budget(), before, "{count}/{shape}: budget charged");
            }
        }
    }
}
