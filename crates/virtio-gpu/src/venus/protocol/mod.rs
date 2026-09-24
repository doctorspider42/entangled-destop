//! The Vulkan half of the Venus protocol: every command a guest's Mesa driver
//! sends through the ring once it exists, decoded and answered in Rust
//! generated from the same model that generates Mesa's own encoders (EPIC 20,
//! ADR-0004).
//!
//! # Where this comes from
//!
//! `tools/venus-protocol/` vendors the venus-protocol generator
//! (`vn_protocol.py`, `vkxml.py`, the XML registries and upstream's C
//! templates) at the revision Mesa 26.2.3 bundles, git-70991d4c. Upstream's
//! Python model decides everything that makes a byte mean something: which
//! parameters are inputs and which are outputs, which outputs are sent as
//! "partial" skeletons, which pointers may be null, which arrays cross-check
//! their count, which structures a pNext chain may carry. Our own
//! `rust_protocol.py` and the `templates/rust_*.rs` it drives translate that
//! model into calls on [`wire::Decoder`](super::wire::Decoder) and
//! [`wire::Encoder`](super::wire::Encoder). Nothing here re-derives a rule
//! from `vk.xml` by hand, which is the point: a field misread identically by
//! a hand-written decoder and a hand-written encoder round-trips perfectly
//! and is still wrong (ADR-0004).
//!
//! Every file in this directory except this one and `tests.rs` is
//! **generated — do not edit**. Regenerate with
//!
//! ```text
//! python scripts/venus-gen.py
//! ```
//!
//! (with Mako installed; see that script), and CI fails if the checked-in
//! output differs from what the generator produces.
//!
//! # What is generated, and for what
//!
//! `tools/venus-protocol/rust-selection.txt` names the commands, and says
//! `*`: **the whole protocol** — every one of the 325 commands venus-protocol
//! can serialize, less the ten transport commands [`super::transport`]
//! hand-writes, every structure and union they reach and every pNext
//! whitelist exactly as the C renderer's. For each command the generator
//! emits a `*Args` type in its group module ([`instance`], [`device`],
//! [`command_buffer`], …) with:
//!
//! * `decode` — the renderer side: arguments as the guest encoded them, with
//!   output parameters present as the *skeletons* the guest sent (a marker,
//!   an `sType` and the pNext chain the reply must fill, guest-chosen object
//!   ids for output handles);
//! * `encode_reply` — the reply: opcode, the return value if any, then every
//!   output parameter in declaration order;
//! * `encode_command` and `decode_reply` — the *driver* side, mirroring
//!   Mesa's encoder and reply decoder. The host never needs them; the tests
//!   and the differential harness do, and a reply the host builds can be
//!   checked by decoding it the way the guest will.
//!
//! Every structure those commands reach — by value, by pointer, or through a
//! pNext chain — gets `decode`/`encode` and `decode_partial`/`encode_partial`
//! (the skeleton form an output structure takes inside a *command*). A union
//! is an enum with one variant per member: `decode`/`encode` for the ones the
//! protocol always sends under a default tag, `decode_tagged`/`encode_tagged`
//! for the ones a selector field in their holder tags. Every type that can
//! hold a pNext link has `for_each_link`, which is how an executor judges a
//! command's chains against what it implements.
//! [`dispatch::Command`] ties them together behind one opcode switch.
//!
//! Decoding a command is not implementing it: the executor refuses every
//! generated command it has no handler for, and every chained structure its
//! policy does not admit.
//!
//! # What the host must still do with it
//!
//! Decoding checks **shape** — lengths, markers, structure types, chain
//! whitelists — exactly as far as the generated C does, and a little
//! further (see below). It does not check *meaning*:
//!
//! * handles are the guest's 64-bit object ids, not host handles, and name
//!   nothing until an object table resolves them;
//! * enums and flag words are carried raw — the generated C does not
//!   range-check them either (`vn_protocol_renderer_types.h`), and the host
//!   must before any of them reaches a real driver;
//! * output arrays arrive sized by the guest's count field; the executor
//!   decides how many elements to produce (clamped to that count) and the
//!   reply encoder refuses a vector that disagrees with the count it writes.
//!
//! # Where this is stricter than the C
//!
//! * A structure whose `sType` is wrong ends the decode; the C sets fatal and
//!   keeps reading fields out of what it has just said is the wrong struct.
//! * A pNext chain may carry each `sType` once. Vulkan forbids repeats; the C
//!   does not look. [`super::transport`] made the same call.
//! * A pNext chain may only carry structures this crate generated. With the
//!   default selection that is exactly the C's whitelist; a narrower
//!   selection filters it to core versions up to `[api]` and the extensions
//!   in `[extensions]`, and [`info::DECODABLE_EXTENSION_MASK`] is the capset
//!   mask that keeps the guest's encoder inside that set.
//! * A selected union's own tag must equal its holder's selector. Mesa writes
//!   the selector as the tag; the C decoder reads both and compares neither.
//! * A strided array's stride must be the element size. Mesa packs the
//!   elements and rewrites the stride to `sizeof(element)`; the C decoder
//!   takes whatever came and hands it to the driver with the packed array.
//! * A string must carry its NUL. The C writes one over the last byte.
//!   (Fixed-size `char[N]` arrays follow the C: their last byte is forced to
//!   NUL.)
//!
//! # Allocation
//!
//! Every host allocation a guest can size goes through
//! [`Decoder::repeat`](super::wire::Decoder::repeat) or its typed siblings,
//! which bound the count by the bytes actually left in the stream and charge
//! the per-command budget. Output arrays whose elements have no wire bytes
//! at all (a partial `VkExtensionProperties` is nothing but a slot) are the
//! one shape that bound cannot cover — a guest may legitimately ask for 200
//! of them in 20 bytes — so they are **not** allocated at decode: they
//! decode as `Some(Vec::new())`, and the count field says how many the guest
//! can take. The same goes for an output blob (`vkGetQueryPoolResults`'
//! `pData`): its size is the guest's `dataSize`, and the executor allocates
//! it when it has something to put there. Input blobs and strings are
//! borrowed from the command bytes and cost nothing. A pNext chain is
//! bounded by [`MAX_PNEXT_DEPTH`](super::wire::MAX_PNEXT_DEPTH).
//!
//! No generated function panics, indexes with a guest value, or
//! `unwrap`s; every refusal poisons the decoder it came from, so a caller
//! who drops an `Err` still cannot read past it.

// The generated code names things the way the registry does, and some lints
// are about names or sizes that are the protocol's, not ours to choose.
//
// * `enum_variant_names`: a pNext enum's variants are the structure names it
//   admits, which all start with `VkPhysicalDevice` for most parents.
// * `large_enum_variant`: a chain link is a whole Vulkan structure by value;
//   boxing each would be an allocation the budget does not see.

use thiserror::Error;

use super::wire::{Decoder, Encoder, PnextVisitor, WireError};

#[allow(clippy::enum_variant_names, clippy::large_enum_variant)]
pub mod acceleration_structure;
#[allow(clippy::enum_variant_names, clippy::large_enum_variant)]
pub mod buffer;
#[allow(clippy::enum_variant_names, clippy::large_enum_variant)]
pub mod buffer_view;
#[allow(clippy::enum_variant_names, clippy::large_enum_variant)]
pub mod command_buffer;
#[allow(clippy::enum_variant_names, clippy::large_enum_variant)]
pub mod command_pool;
pub mod defines;
#[allow(clippy::enum_variant_names, clippy::large_enum_variant)]
pub mod descriptor_heap;
#[allow(clippy::enum_variant_names, clippy::large_enum_variant)]
pub mod descriptor_pool;
#[allow(clippy::enum_variant_names, clippy::large_enum_variant)]
pub mod descriptor_set;
#[allow(clippy::enum_variant_names, clippy::large_enum_variant)]
pub mod descriptor_set_layout;
#[allow(clippy::enum_variant_names, clippy::large_enum_variant)]
pub mod descriptor_update_template;
#[allow(clippy::enum_variant_names, clippy::large_enum_variant)]
pub mod device;
#[allow(clippy::enum_variant_names, clippy::large_enum_variant)]
pub mod device_memory;
#[allow(clippy::large_enum_variant)]
pub mod dispatch;
#[allow(clippy::enum_variant_names, clippy::large_enum_variant)]
pub mod event;
#[allow(clippy::enum_variant_names, clippy::large_enum_variant)]
pub mod fence;
#[allow(clippy::enum_variant_names, clippy::large_enum_variant)]
pub mod framebuffer;
#[allow(clippy::enum_variant_names, clippy::large_enum_variant)]
pub mod host_copy;
#[allow(clippy::enum_variant_names, clippy::large_enum_variant)]
pub mod image;
#[allow(clippy::enum_variant_names, clippy::large_enum_variant)]
pub mod image_view;
pub mod info;
#[allow(clippy::enum_variant_names, clippy::large_enum_variant)]
pub mod instance;
#[allow(clippy::enum_variant_names, clippy::large_enum_variant)]
pub mod pipeline;
#[allow(clippy::enum_variant_names, clippy::large_enum_variant)]
pub mod pipeline_cache;
#[allow(clippy::enum_variant_names, clippy::large_enum_variant)]
pub mod pipeline_layout;
#[allow(clippy::enum_variant_names, clippy::large_enum_variant)]
pub mod private_data_slot;
#[allow(clippy::enum_variant_names, clippy::large_enum_variant)]
pub mod query_pool;
#[allow(clippy::enum_variant_names, clippy::large_enum_variant)]
pub mod queue;
#[allow(clippy::enum_variant_names, clippy::large_enum_variant)]
pub mod render_pass;
#[allow(clippy::enum_variant_names, clippy::large_enum_variant)]
pub mod sampler;
#[allow(clippy::enum_variant_names, clippy::large_enum_variant)]
pub mod sampler_ycbcr_conversion;
#[allow(clippy::enum_variant_names, clippy::large_enum_variant)]
pub mod semaphore;
#[allow(clippy::enum_variant_names, clippy::large_enum_variant)]
pub mod shader_module;
#[allow(clippy::enum_variant_names, clippy::large_enum_variant)]
pub mod structs;
#[allow(clippy::enum_variant_names, clippy::large_enum_variant)]
pub mod transport;

#[cfg(test)]
mod tests;

pub use acceleration_structure::*;
pub use buffer::*;
pub use buffer_view::*;
pub use command_buffer::*;
pub use command_pool::*;
pub use defines::*;
pub use descriptor_heap::*;
pub use descriptor_pool::*;
pub use descriptor_set::*;
pub use descriptor_set_layout::*;
pub use descriptor_update_template::*;
pub use device::*;
pub use device_memory::*;
pub use dispatch::*;
pub use event::*;
pub use fence::*;
pub use framebuffer::*;
pub use host_copy::*;
pub use image::*;
pub use image_view::*;
pub use instance::*;
pub use pipeline::*;
pub use pipeline_cache::*;
pub use pipeline_layout::*;
pub use private_data_slot::*;
pub use query_pool::*;
pub use queue::*;
pub use render_pass::*;
pub use sampler::*;
pub use sampler_ycbcr_conversion::*;
pub use semaphore::*;
pub use shader_module::*;
pub use structs::*;
pub use transport::*;

/// Why a Venus command could not be decoded, or a reply encoded.
///
/// Every decode-side variant is fatal to the stream: there is no length field
/// to resynchronise on, and the [`Decoder`] that produced it is poisoned so
/// nothing can be read past it. Encode-side variants mean the *host* asked
/// for a reply that cannot be written faithfully; the encoder holding the
/// partial reply must be dropped, which [`dispatch::Command::reply_bytes`]
/// does for its caller.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum ProtocolError {
    /// The bytes themselves were wrong — truncated, an impossible or
    /// contradictory array length, an unknown pNext `sType`, a chain deeper
    /// than the host will walk, a non-null `pAllocator`.
    #[error(transparent)]
    Wire(#[from] WireError),

    /// A structure did not announce itself as the one the parameter holds.
    #[error("{structure} announces sType {found}, not the {expected} it must carry")]
    WrongStructureType {
        /// The structure the parameter holds.
        structure: &'static str,
        /// What the guest wrote.
        found: i32,
        /// Its `VK_STRUCTURE_TYPE_*`.
        expected: i32,
    },

    /// A pointer the protocol requires was null (`vn_cs_decoder_set_fatal`
    /// in the `else` branch of a non-optional `simple_pointer`).
    #[error("{owner}::{field} is required and the guest sent it null")]
    NullPointer {
        /// Structure or command holding the pointer.
        owner: &'static str,
        /// Its C member or parameter name.
        field: &'static str,
    },

    /// A pointer to something the wire cannot carry (a callback, a host
    /// address) was non-null. `pAllocator` has its own
    /// [`WireError::AllocatorNotNull`].
    #[error("{owner}::{field} cannot be serialized and must be null")]
    UnsupportedPointer {
        /// Structure or command holding the pointer.
        owner: &'static str,
        /// Its C member or parameter name.
        field: &'static str,
    },

    /// The dispatchable handle a command is issued on was
    /// `VK_NULL_HANDLE` (`if (!args.instance) vn_cs_decoder_set_fatal`).
    #[error("{command} was issued on a null dispatchable handle")]
    NullDispatchHandle {
        /// The command.
        command: &'static str,
    },

    /// The same `sType` twice in one pNext chain.
    #[error("the pNext chain of {parent} carries sType {stype} more than once")]
    DuplicatePnextStype {
        /// Structure owning the chain.
        parent: &'static str,
        /// The repeated `VkStructureType`.
        stype: i32,
    },

    /// A chain link that has a chain of its own. Encode side: links are
    /// written flat into their parent's chain, so a link's own `p_next`
    /// would be silently dropped.
    #[error("{link} in the pNext chain of {parent} carries a chain of its own")]
    NestedPnextChain {
        /// Structure owning the chain.
        parent: &'static str,
        /// The link that carries one.
        link: &'static str,
    },

    /// An array whose vector disagrees with the count field the encoder
    /// writes beside it. Encode side: the guest's decoder would read `count`
    /// elements whatever the vector held.
    #[error("{owner}::{field} holds {len} elements but its count says {count}")]
    ArrayCountMismatch {
        /// Structure or command holding the array.
        owner: &'static str,
        /// Its C member or parameter name.
        field: &'static str,
        /// The count field's value.
        count: u64,
        /// Elements actually present.
        len: usize,
    },

    /// A reply whose opcode is not the command's.
    #[error("a reply to opcode {expected} starts with opcode {found}")]
    WrongReplyOpcode {
        /// The command's `VkCommandTypeEXT`.
        expected: u32,
        /// What the reply carried.
        found: u32,
    },

    /// An opcode that is not a `VkCommandTypeEXT` at all.
    #[error(
        "opcode {opcode} is not a Venus command, and a stream with no length field cannot \
         step over one"
    )]
    UnknownOpcode {
        /// What the guest wrote.
        opcode: u32,
    },

    /// A real Venus command that this crate did not generate a decoder for:
    /// one venus-protocol cannot serialize at all (`vkMapMemory`, the fd
    /// commands, ...), a transport command asked of the wrong decoder, or one
    /// a narrower `tools/venus-protocol/rust-selection.txt` left out. Fatal
    /// for the same reason as [`ProtocolError::UnknownOpcode`].
    #[error("{command} (opcode {opcode}) has no generated decoder")]
    NotGenerated {
        /// Its `VkCommandTypeEXT`.
        opcode: u32,
        /// Its name.
        command: &'static str,
    },

    /// Command flag bits other than `VK_COMMAND_GENERATE_REPLY_BIT_EXT`.
    #[error("{command} carries command flag bits {unknown:#x} that this host does not define")]
    UnknownCommandFlags {
        /// The command.
        command: &'static str,
        /// Only the unknown bits.
        unknown: u32,
    },

    /// A union tag that selects none of the union's members
    /// (`vn_cs_decoder_set_fatal` in the `default:` of the union's switch).
    #[error("union {union} has no member for tag {tag}")]
    UnknownUnionTag {
        /// The union.
        union: &'static str,
        /// The tag the guest sent.
        tag: i64,
    },

    /// A selected union whose own wire tag is not its holder's selector.
    /// Mesa's encoder writes the selector as the tag, so the two cannot
    /// differ in a stream it produced; the C decoder reads both and compares
    /// neither, which would hand the driver one member under the other's
    /// name. Stricter than the C.
    #[error("{owner}::{field} carries union tag {tag} but its selector says {selector}")]
    UnionSelectorMismatch {
        /// The structure holding the union.
        owner: &'static str,
        /// The union member.
        field: &'static str,
        /// The tag on the wire.
        tag: i64,
        /// The selector field's value.
        selector: i64,
    },

    /// Encode side: a union asked to go out under a tag that selects a
    /// member it does not hold.
    #[error("union {union} does not hold the member tag {tag} selects")]
    UnionTagMismatch {
        /// The union.
        union: &'static str,
        /// The tag asked for.
        tag: i64,
    },

    /// A strided array's stride that is not the element's size. Mesa's
    /// encoder packs the elements and rewrites the stride to
    /// `sizeof(element)`; the C decoder reads whatever came, and a driver
    /// handed the packed array with a larger stride reads past it. Stricter
    /// than the C.
    #[error(
        "{owner}::{field} is {stride}, not the {expected}-byte element size the array is packed at"
    )]
    BadStride {
        /// The command or structure.
        owner: &'static str,
        /// The stride parameter.
        field: &'static str,
        /// What the guest sent.
        stride: u32,
        /// `sizeof(element)`.
        expected: u32,
    },
}

/// Record `err` on the decoder, so that nothing is read past it, and hand it
/// back. The decoder is poisoned with [`WireError::Poisoned`]: the real
/// reason is `err`, which the caller returns. Same shape as
/// `transport::refuse`.
fn refuse(dec: &mut Decoder<'_>, err: ProtocolError) -> ProtocolError {
    match err {
        ProtocolError::Wire(wire) => dec.set_fatal(wire),
        _ => dec.set_fatal(WireError::Poisoned),
    };
    err
}

/// [`ProtocolError::NullPointer`], recorded on `dec`.
pub fn null_pointer(
    dec: &mut Decoder<'_>,
    owner: &'static str,
    field: &'static str,
) -> ProtocolError {
    refuse(dec, ProtocolError::NullPointer { owner, field })
}

/// [`ProtocolError::UnsupportedPointer`], recorded on `dec`.
pub fn unsupported_pointer(
    dec: &mut Decoder<'_>,
    owner: &'static str,
    field: &'static str,
) -> ProtocolError {
    refuse(dec, ProtocolError::UnsupportedPointer { owner, field })
}

/// [`ProtocolError::NullDispatchHandle`], recorded on `dec`.
pub fn null_dispatch_handle(dec: &mut Decoder<'_>, command: &'static str) -> ProtocolError {
    refuse(dec, ProtocolError::NullDispatchHandle { command })
}

/// [`ProtocolError::UnknownUnionTag`], recorded on `dec`.
pub fn unknown_union_tag(dec: &mut Decoder<'_>, union: &'static str, tag: i64) -> ProtocolError {
    refuse(dec, ProtocolError::UnknownUnionTag { union, tag })
}

/// A selected union's wire tag against its holder's selector field.
///
/// # Errors
/// [`ProtocolError::UnionSelectorMismatch`] when they differ.
pub fn check_union_tag(
    dec: &mut Decoder<'_>,
    owner: &'static str,
    field: &'static str,
    tag: i64,
    selector: i64,
) -> Result<(), ProtocolError> {
    if tag == selector {
        return Ok(());
    }
    Err(refuse(
        dec,
        ProtocolError::UnionSelectorMismatch {
            owner,
            field,
            tag,
            selector,
        },
    ))
}

/// A stride parameter against the element size its array travels packed at.
///
/// # Errors
/// [`ProtocolError::BadStride`] when they differ.
pub fn check_stride(
    dec: &mut Decoder<'_>,
    owner: &'static str,
    field: &'static str,
    stride: u32,
    expected: u32,
) -> Result<(), ProtocolError> {
    if stride == expected {
        return Ok(());
    }
    Err(refuse(
        dec,
        ProtocolError::BadStride {
            owner,
            field,
            stride,
            expected,
        },
    ))
}

/// Read and check a structure's `sType`.
///
/// # Errors
/// [`ProtocolError::WrongStructureType`], or whatever the read refused.
pub fn expect_structure_type(
    dec: &mut Decoder<'_>,
    structure: &'static str,
    expected: i32,
) -> Result<(), ProtocolError> {
    let found = dec.structure_type()?;
    if found != expected {
        return Err(refuse(
            dec,
            ProtocolError::WrongStructureType {
                structure,
                found,
                expected,
            },
        ));
    }
    Ok(())
}

/// Encode side: `len` elements are about to be written behind a count field
/// that says `count`.
///
/// # Errors
/// [`ProtocolError::ArrayCountMismatch`] when they differ.
pub fn check_len(
    owner: &'static str,
    field: &'static str,
    count: u64,
    len: usize,
) -> Result<(), ProtocolError> {
    if u64::try_from(len).ok() == Some(count) {
        Ok(())
    } else {
        Err(ProtocolError::ArrayCountMismatch {
            owner,
            field,
            count,
            len,
        })
    }
}

/// How a null array's length word is judged.
///
/// The generated C peeks the 8-byte length to choose a branch and then
/// consumes it in both. In the null branch it cross-checks it against the
/// count field for an array the registry calls required
/// (`vn_decode_array_size`), and reads it unchecked for an optional one, a
/// `noautovalidity` one, and every array in a reply
/// (`vn_decode_array_size_unchecked`). `_decode_variable` in
/// `vn_protocol.py` is where the choice is made.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NullArray {
    /// A null array is legal only when the count is zero.
    Checked,
    /// A null array is legal whatever the count says.
    Unchecked,
}

/// The 8-byte length that *is* a dynamic array's presence marker: `None` for
/// a null array, `Some(len)` for a present one — whose length has been
/// cross-checked against `expected`, which the C does in the present branch
/// for every array.
///
/// # Errors
/// [`WireError::ArrayLengthMismatch`], [`WireError::ArrayLongerThanStream`]
/// for a length no host slice could hold, or [`WireError::Truncated`].
pub fn array_presence(
    dec: &mut Decoder<'_>,
    expected: u64,
    null: NullArray,
) -> Result<Option<usize>, ProtocolError> {
    if dec.peek_array_size()? == 0 {
        match null {
            NullArray::Checked => {
                dec.array_size(expected)?;
            }
            NullArray::Unchecked => {
                dec.array_size_unchecked()?;
            }
        }
        return Ok(None);
    }
    let size = dec.array_size(expected)?;
    match usize::try_from(size) {
        Ok(len) => Ok(Some(len)),
        Err(_) => {
            let left = dec.remaining();
            Err(refuse(
                dec,
                ProtocolError::Wire(WireError::ArrayLongerThanStream {
                    count: size,
                    needed: size,
                    left,
                }),
            ))
        }
    }
}

/// `n` elements decoded by `item`, through [`Decoder::repeat`] — so bounded
/// by the stream and charged to the budget — with `item`'s own refusals
/// kept intact rather than flattened into a [`WireError`].
///
/// # Errors
/// Whatever the bound, the budget or `item` refused.
pub fn decode_vec<'a, T>(
    dec: &mut Decoder<'a>,
    n: usize,
    mut item: impl FnMut(&mut Decoder<'a>) -> Result<T, ProtocolError>,
) -> Result<Vec<T>, ProtocolError> {
    let mut failure: Option<ProtocolError> = None;
    let out = dec.repeat(n, |d| match item(d) {
        Ok(value) => Ok(value),
        Err(ProtocolError::Wire(wire)) => Err(wire),
        Err(other) => {
            failure.get_or_insert(other);
            Err(WireError::Poisoned)
        }
    });
    match (out, failure) {
        (_, Some(err)) => Err(err),
        (Ok(values), None) => Ok(values),
        (Err(wire), None) => Err(ProtocolError::Wire(wire)),
    }
}

/// [`decode_vec`], with each element told its index — the outer level of a
/// two-level array, whose inner counts are read from the element of another
/// array with the same index (`pInfos[i].geometryCount`).
///
/// # Errors
/// As [`decode_vec`].
pub fn decode_vec_indexed<'a, T>(
    dec: &mut Decoder<'a>,
    n: usize,
    mut item: impl FnMut(&mut Decoder<'a>, usize) -> Result<T, ProtocolError>,
) -> Result<Vec<T>, ProtocolError> {
    let mut index = 0usize;
    decode_vec(dec, n, |d| {
        let value = item(d, index);
        index = index.saturating_add(1);
        value
    })
}

/// The inner level of a two-level array: its 8-byte length, which must
/// equal `expected` (`vn_decode_array_size`, no null form), as a count.
///
/// # Errors
/// [`WireError::ArrayLengthMismatch`], [`WireError::ArrayLongerThanStream`]
/// for a length no host slice could hold, or [`WireError::Truncated`].
pub fn inner_array(dec: &mut Decoder<'_>, expected: u64) -> Result<usize, ProtocolError> {
    let size = dec.array_size(expected)?;
    match usize::try_from(size) {
        Ok(len) => Ok(len),
        Err(_) => {
            let left = dec.remaining();
            Err(refuse(
                dec,
                ProtocolError::Wire(WireError::ArrayLongerThanStream {
                    count: size,
                    needed: size,
                    left,
                }),
            ))
        }
    }
}

/// `n` opaque bytes padded to four, copied out of the stream — a blob the
/// host keeps or writes itself (`vn_decode_blob_array` into storage it
/// owns). Bounded by the stream: the bytes have to be there.
///
/// # Errors
/// [`WireError::Truncated`], or [`WireError::OutOfMemory`].
pub fn decode_owned_blob(dec: &mut Decoder<'_>, n: usize) -> Result<Vec<u8>, ProtocolError> {
    let bytes = dec.blob(n)?;
    let mut out = Vec::new();
    if out.try_reserve_exact(bytes.len()).is_err() {
        return Err(refuse(
            dec,
            ProtocolError::Wire(WireError::OutOfMemory {
                wanted: bytes.len(),
            }),
        ));
    }
    out.extend_from_slice(bytes);
    Ok(out)
}

/// `n` packed `uint16_t`s: `2 * n` bytes, then padding to four — not one
/// four-byte slot each as a lone `uint16_t` travels
/// (`vn_decode_uint16_t_array`, `(size + 3) & ~3`).
///
/// # Errors
/// [`WireError::Truncated`], [`WireError::ArrayLongerThanStream`] for a
/// count whose bytes overflow, or [`WireError::OutOfMemory`].
pub fn decode_u16_array(dec: &mut Decoder<'_>, n: usize) -> Result<Vec<u16>, ProtocolError> {
    let Some(len) = n.checked_mul(2) else {
        let left = dec.remaining();
        let count = u64::try_from(n).unwrap_or(u64::MAX);
        return Err(refuse(
            dec,
            ProtocolError::Wire(WireError::ArrayLongerThanStream {
                count,
                needed: u64::MAX,
                left,
            }),
        ));
    };
    let bytes = dec.blob(len)?;
    let mut out = Vec::new();
    if out.try_reserve_exact(n).is_err() {
        return Err(refuse(
            dec,
            ProtocolError::Wire(WireError::OutOfMemory { wanted: len }),
        ));
    }
    out.extend(
        bytes
            .chunks_exact(2)
            .map(|pair| u16::from_le_bytes([pair[0], pair[1]])),
    );
    Ok(out)
}

/// A fixed `uint16_t[N]`: the 8-byte length (which must be `N`), then the
/// `N` values packed as [`decode_u16_array`] reads them.
///
/// # Errors
/// [`WireError::ArrayLengthMismatch`] or [`WireError::Truncated`].
pub fn decode_u16_fixed<const N: usize>(dec: &mut Decoder<'_>) -> Result<[u16; N], ProtocolError> {
    dec.array_size(N as u64)?;
    let bytes = dec.blob(N.saturating_mul(2))?;
    let mut out = [0u16; N];
    for (slot, pair) in out.iter_mut().zip(bytes.chunks_exact(2)) {
        *slot = u16::from_le_bytes([pair[0], pair[1]]);
    }
    Ok(out)
}

/// Packed `uint16_t`s, the mirror of [`decode_u16_array`]: two bytes each,
/// then zero padding to four.
///
/// # Errors
/// Whatever the encoder refused.
pub fn encode_u16_array(enc: &mut Encoder, values: &[u16]) -> Result<(), ProtocolError> {
    let mut bytes = Vec::new();
    if bytes
        .try_reserve_exact(values.len().saturating_mul(2))
        .is_err()
    {
        return Err(ProtocolError::Wire(WireError::OutOfMemory {
            wanted: values.len().saturating_mul(2),
        }));
    }
    for value in values {
        bytes.extend_from_slice(&value.to_le_bytes());
    }
    Ok(enc.blob(&bytes)?)
}

/// One element of an array of strings: its own 8-byte length, unchecked,
/// then the bytes. Unlike a lone string it has no null form — a zero length
/// is [`WireError::EmptyString`], where the C sets fatal too
/// (`vn_decode_char_array` with `size == 0`).
///
/// # Errors
/// As [`Decoder::string_bytes`].
pub fn decode_string_element<'a>(dec: &mut Decoder<'a>) -> Result<&'a [u8], ProtocolError> {
    let size = dec.array_size_unchecked()?;
    let len = match usize::try_from(size) {
        Ok(len) => len,
        Err(_) => {
            let left = dec.remaining();
            return Err(refuse(
                dec,
                ProtocolError::Wire(WireError::ArrayLongerThanStream {
                    count: size,
                    needed: size,
                    left,
                }),
            ));
        }
    };
    Ok(dec.string_bytes(len)?)
}

/// A fixed `char[N]`: the 8-byte length (which must be `N`), then `N` bytes
/// padded to four, with the last byte forced to NUL as `vn_decode_char_array`
/// does. The bytes after the first NUL are kept, so a re-encode is
/// byte-exact.
///
/// # Errors
/// [`WireError::ArrayLengthMismatch`] or [`WireError::Truncated`].
pub fn decode_char_array<const N: usize>(dec: &mut Decoder<'_>) -> Result<[u8; N], ProtocolError> {
    let mut out = decode_byte_array::<N>(dec)?;
    if let Some(last) = out.last_mut() {
        *last = 0;
    }
    Ok(out)
}

/// A fixed `uint8_t[N]`: the 8-byte length (which must be `N`), then `N`
/// bytes padded to four.
///
/// # Errors
/// [`WireError::ArrayLengthMismatch`] or [`WireError::Truncated`].
pub fn decode_byte_array<const N: usize>(dec: &mut Decoder<'_>) -> Result<[u8; N], ProtocolError> {
    dec.array_size(N as u64)?;
    let bytes = dec.blob(N)?;
    let mut out = [0u8; N];
    for (slot, byte) in out.iter_mut().zip(bytes) {
        *slot = *byte;
    }
    Ok(out)
}

/// A fixed `T[N]`: the 8-byte length (which must be `N`), then `N` elements.
/// No allocation: the array is part of the structure holding it.
///
/// # Errors
/// [`WireError::ArrayLengthMismatch`], or whatever `item` refused.
pub fn decode_fixed_array<'a, T: Default, const N: usize>(
    dec: &mut Decoder<'a>,
    mut item: impl FnMut(&mut Decoder<'a>) -> Result<T, ProtocolError>,
) -> Result<[T; N], ProtocolError> {
    dec.array_size(N as u64)?;
    let mut out: [T; N] = std::array::from_fn(|_| T::default());
    for slot in &mut out {
        *slot = item(dec)?;
    }
    Ok(out)
}

/// One pNext chain's admissible links: a generated enum per parent
/// structure, one variant per `sType` its whitelist holds.
///
/// The whitelist is `vn_decode_*_pnext_temp`'s `switch` narrowed to what
/// this crate generated. What is not in it is fatal: a link carries no
/// length, so there is no way to step over one.
pub trait ChainLink<'a>: Sized {
    /// The structure owning the chain, for refusals.
    const PARENT: &'static str;

    /// The link's `VkStructureType`.
    fn structure_type(&self) -> i32;

    /// Decode the body of a link with this `sType` — the fields after
    /// `sType` and `pNext` — or `None` if the parent does not admit it.
    fn decode_body(
        stype: i32,
        dec: &mut Decoder<'a>,
        partial: bool,
    ) -> Option<Result<Self, ProtocolError>>;

    /// Encode the body of this link.
    ///
    /// # Errors
    /// Whatever the body's encoder refused.
    fn encode_body(&self, enc: &mut Encoder, partial: bool) -> Result<(), ProtocolError>;
}

/// The [`PnextVisitor`] behind [`decode_chain`]: collects links in the order
/// [`Decoder::pnext_chain`] visits them (deepest first) and keeps the first
/// non-wire refusal to hand back intact.
struct ChainVisitor<N> {
    links: Vec<N>,
    partial: bool,
    failure: Option<ProtocolError>,
    duplicate: Option<i32>,
}

impl<'a, N: ChainLink<'a>> PnextVisitor<'a> for ChainVisitor<N> {
    const PARENT: &'static str = N::PARENT;

    fn visit(&mut self, stype: i32, dec: &mut Decoder<'a>) -> Result<(), WireError> {
        match N::decode_body(stype, dec, self.partial) {
            None => Err(Self::unknown(stype)),
            Some(Ok(link)) => {
                // What the chain costs the host is charged like any array's,
                // so the budget bounds it as well as the depth does.
                dec.charge(std::mem::size_of::<N>())?;
                if self.links.iter().any(|l| l.structure_type() == stype) {
                    self.duplicate.get_or_insert(stype);
                }
                if self.links.try_reserve(1).is_err() {
                    return Err(WireError::OutOfMemory {
                        wanted: std::mem::size_of::<N>(),
                    });
                }
                self.links.push(link);
                Ok(())
            }
            Some(Err(ProtocolError::Wire(wire))) => Err(wire),
            Some(Err(other)) => {
                self.failure.get_or_insert(other);
                Err(WireError::Poisoned)
            }
        }
    }
}

/// A pNext chain, in the guest's order (the first link is the one the
/// parent's `pNext` points at).
///
/// The wire writes every link's marker and `sType` front to back, then the
/// terminating null, then the **bodies back to front** — each link decodes
/// the whole rest of the chain before its own fields. [`Decoder::pnext_chain`]
/// walks that; this reverses what it collected.
///
/// # Errors
/// [`WireError::UnknownPnextStype`], [`WireError::PnextChainTooDeep`],
/// [`ProtocolError::DuplicatePnextStype`], or whatever a link's body refused.
pub fn decode_chain<'a, N: ChainLink<'a>>(
    dec: &mut Decoder<'a>,
    partial: bool,
) -> Result<Vec<N>, ProtocolError> {
    let mut visitor = ChainVisitor {
        links: Vec::new(),
        partial,
        failure: None,
        duplicate: None,
    };
    let walked = dec.pnext_chain(&mut visitor);
    if let Some(err) = visitor.failure {
        return Err(err);
    }
    walked?;
    if let Some(stype) = visitor.duplicate {
        return Err(refuse(
            dec,
            ProtocolError::DuplicatePnextStype {
                parent: N::PARENT,
                stype,
            },
        ));
    }
    visitor.links.reverse();
    Ok(visitor.links)
}

/// Encode a pNext chain given in the guest's order: every marker and `sType`
/// front to back, the terminating null, then every body back to front. The
/// mirror image of [`decode_chain`].
///
/// # Errors
/// Whatever the encoder or a link's body refused.
pub fn encode_chain<'a, N: ChainLink<'a>>(
    enc: &mut Encoder,
    links: &[N],
    partial: bool,
) -> Result<(), ProtocolError> {
    for link in links {
        enc.simple_pointer(true)?;
        enc.structure_type(link.structure_type())?;
    }
    enc.simple_pointer(false)?;
    for link in links.iter().rev() {
        link.encode_body(enc, partial)?;
    }
    Ok(())
}
