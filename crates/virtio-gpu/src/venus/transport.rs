//! The Venus commands that arrive on the *context* command stream, before any
//! ring exists (EPIC 20, ADR-0004).
//!
//! Almost every Venus command travels through a shared-memory command ring.
//! The ring has to be created first, and the commands that create, destroy and
//! poke it cannot travel through it — they arrive on the ordinary virtio-gpu
//! context command stream, the `SUBMIT_3D`/execbuffer path. This module decodes
//! that small, privileged set. Until it works there is no ring, so these are
//! the first Venus bytes a host ever has to understand.
//!
//! The reference is virglrenderer 1.1.0: `venus-protocol/vn_protocol_renderer_transport.h`
//! for the generated argument decoders, and `vkr_transport.c` for what the host
//! then does with them. Line numbers below are from that release.
//!
//! # What this layer is
//!
//! A decoder and a shape check, and nothing else. It does not execute
//! anything, does not touch shared memory, does not know what a resource id
//! names and does not judge whether a proposed ring layout is sane — that last
//! one is [`ring::RingLayout::new`]'s job, and this module hands it an
//! entirely untrusted [`RingCreateInfo`] to judge. Decoding produces a
//! [`TransportCommand`] the caller matches on.
//!
//! [`TransportCommand`] is deliberately **not** `#[non_exhaustive]`: adding an
//! opcode should break every caller's `match`, because a transport command that
//! is silently ignored is a guest that hangs forever waiting for a ring.
//!
//! # Framing, and why an unknown opcode is the end
//!
//! A command is a `u32` opcode then a `u32` flags word — eight bytes, and **no
//! length field anywhere** (`vn_dispatch_command`,
//! `vn_protocol_renderer_dispatches.h:604-618`). An opcode with no decoder
//! therefore cannot be skipped: nothing on the wire says where it ends, so the
//! only honest thing to do is to kill the stream. The reference does exactly
//! that (`vn_cs_decoder_set_fatal` in the `else` branch of the dispatch table
//! lookup), and so does [`TransportStream`].
//!
//! The same argument makes every refusal here sticky. [`Decoder`] already
//! models that; this layer adds its own refusals to it, so a
//! [`TransportStream`] that has refused once yields nothing afterwards.
//!
//! # The opcodes
//!
//! From `vn_protocol_renderer_defines.h:379-392`. The ten that
//! `vkr_context_init_transport_dispatch` registers, and the only ten this
//! module knows:
//!
//! | Opcode | Command | Arrives on |
//! |---|---|---|
//! | 178 | `vkSetReplyCommandStreamMESA` | context or ring |
//! | 179 | `vkSeekReplyCommandStreamMESA` | context or ring |
//! | 180 | `vkExecuteCommandStreamsMESA` | context or ring |
//! | 188 | `vkCreateRingMESA` | context only |
//! | 189 | `vkDestroyRingMESA` | context only |
//! | 190 | `vkNotifyRingMESA` | context only |
//! | 191 | `vkWriteRingExtraMESA` | context only |
//! | 251 | `vkSubmitVirtqueueSeqnoMESA` | context only |
//! | 252 | `vkWaitVirtqueueSeqnoMESA` | ring only |
//! | 253 | `vkWaitRingSeqnoMESA` | context only |
//!
//! "Arrives on" is `vkr_transport.c`'s `is_dispatched_from_vkr_context` check
//! and is **not** enforced here: this module cannot see which stream it was
//! handed. The layer that owns contexts and rings enforces it, and it must,
//! because `vkWaitVirtqueueSeqnoMESA` on the context stream would block the
//! whole context.
//!
//! # The allocation budget
//!
//! [`Decoder`] charges guest-declared arrays against a per-command budget that
//! the dispatcher is expected to reset between commands — the reference's
//! `vn_cs_decoder_reset_temp_pool`, called at the tail of every
//! `vn_dispatch_*`. [`TransportStream`] is that dispatcher: it calls
//! [`Decoder::reset_alloc_budget`] at the top of every
//! [`next_command`](TransportStream::next_command), before the header is even
//! read, so a long batch cannot starve on its own earlier arrays. A caller
//! that drives [`TransportCommand::decode`] by hand owns that reset itself.
//!
//! [`Decoder`]: super::wire::Decoder
//! [`ring::RingLayout::new`]: super::ring::RingLayout::new

use thiserror::Error;

use super::ring::RingCreateInfo;
use super::wire::{
    CommandHeader, Decoder, PnextVisitor, WireError, COMMAND_GENERATE_REPLY, SCALAR_BYTES,
    WIDE_BYTES,
};

/// `VK_STRUCTURE_TYPE_RING_CREATE_INFO_MESA` (`vn_protocol_renderer_defines.h:21`).
pub const STYPE_RING_CREATE_INFO_MESA: i32 = 1_000_384_000;

/// `VK_STRUCTURE_TYPE_RING_MONITOR_INFO_MESA` (`vn_protocol_renderer_defines.h:27`).
pub const STYPE_RING_MONITOR_INFO_MESA: i32 = 1_000_384_006;

/// `VK_STRUCTURE_TYPE_RING_PRIORITY_INFO_MESA` (`vn_protocol_renderer_defines.h:28`).
pub const STYPE_RING_PRIORITY_INFO_MESA: i32 = 1_000_384_007;

/// Every bit `VkCommandFlagsEXT` defines: just
/// `VK_COMMAND_GENERATE_REPLY_BIT_EXT` (`vn_protocol_renderer_defines.h:396`).
///
/// A bit outside this set is refused rather than ignored, which is the house
/// rule for flag words in this crate. The reference ignores unknown bits, and
/// that is the wrong trade here: the only thing a command flag can ask for is
/// a change in how the *reply* is produced, so honouring a command while
/// ignoring the bit that says how to answer it leaves the guest's reply parser
/// reading bytes that were never written.
pub const KNOWN_COMMAND_FLAGS: u32 = COMMAND_GENERATE_REPLY;

/// Wire bytes of a `VkCommandStreamDescriptionMESA`: a `uint32_t` and two
/// `size_t`s (`vn_protocol_renderer_transport.h:23-28`).
pub const COMMAND_STREAM_DESCRIPTION_WIRE_LEN: usize = SCALAR_BYTES + 2 * WIDE_BYTES;

/// Wire bytes of a `VkCommandStreamDependencyMESA`: two `uint32_t`s
/// (`vn_protocol_renderer_transport.h:40-45`).
pub const COMMAND_STREAM_DEPENDENCY_WIRE_LEN: usize = 2 * SCALAR_BYTES;

/// Wire bytes of a `VkRingCreateInfoMESA` whose pNext chain is empty: the
/// `sType`, the eight-byte "no chain" marker, and the body.
///
/// A minimum, not a size — every pNext link adds its own marker, `sType` and
/// body on top.
pub const RING_CREATE_INFO_MIN_WIRE_LEN: usize =
    SCALAR_BYTES + WIDE_BYTES + RingCreateInfo::WIRE_LEN;

/// One of the ten commands `vkr_context_init_transport_dispatch` registers.
///
/// The discriminants are `VkCommandTypeEXT`
/// (`vn_protocol_renderer_defines.h:379-392`) and are load-bearing: they are
/// what a guest actually writes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u32)]
pub enum Opcode {
    /// Point the reply encoder at a shared-memory range.
    SetReplyCommandStream = 178,
    /// Move the reply encoder's cursor within that range.
    SeekReplyCommandStream = 179,
    /// Run the commands in one or more shared-memory ranges.
    ExecuteCommandStreams = 180,
    /// Create a ring from a guest-proposed layout. The one that matters.
    CreateRing = 188,
    /// Tear a ring down.
    DestroyRing = 189,
    /// The doorbell: a ring has work.
    NotifyRing = 190,
    /// Write one `u32` into a ring's `extra` region.
    WriteRingExtra = 191,
    /// Publish a virtqueue seqno on a ring.
    SubmitVirtqueueSeqno = 251,
    /// Block until a virtqueue seqno is reached. Ring dispatch only.
    WaitVirtqueueSeqno = 252,
    /// Block until a ring's own seqno is reached.
    WaitRingSeqno = 253,
}

impl Opcode {
    /// The opcode a guest writes for this command.
    #[must_use]
    pub fn as_u32(self) -> u32 {
        self as u32
    }

    /// The command this opcode names, or `None` — which is fatal for the
    /// stream that carried it, because there is no length field to skip with.
    #[must_use]
    pub fn from_u32(opcode: u32) -> Option<Self> {
        match opcode {
            178 => Some(Self::SetReplyCommandStream),
            179 => Some(Self::SeekReplyCommandStream),
            180 => Some(Self::ExecuteCommandStreams),
            188 => Some(Self::CreateRing),
            189 => Some(Self::DestroyRing),
            190 => Some(Self::NotifyRing),
            191 => Some(Self::WriteRingExtra),
            251 => Some(Self::SubmitVirtqueueSeqno),
            252 => Some(Self::WaitVirtqueueSeqno),
            253 => Some(Self::WaitRingSeqno),
            _ => None,
        }
    }

    /// The Mesa name, for refusal messages and traces.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::SetReplyCommandStream => "vkSetReplyCommandStreamMESA",
            Self::SeekReplyCommandStream => "vkSeekReplyCommandStreamMESA",
            Self::ExecuteCommandStreams => "vkExecuteCommandStreamsMESA",
            Self::CreateRing => "vkCreateRingMESA",
            Self::DestroyRing => "vkDestroyRingMESA",
            Self::NotifyRing => "vkNotifyRingMESA",
            Self::WriteRingExtra => "vkWriteRingExtraMESA",
            Self::SubmitVirtqueueSeqno => "vkSubmitVirtqueueSeqnoMESA",
            Self::WaitVirtqueueSeqno => "vkWaitVirtqueueSeqnoMESA",
            Self::WaitRingSeqno => "vkWaitRingSeqnoMESA",
        }
    }
}

/// Why a transport command could not be decoded.
///
/// Every variant is fatal to the stream that produced it. There is no length
/// field to resynchronise on, so "refuse this command and carry on" is not a
/// thing this protocol can express; the caller's only correct response is to
/// stop reading and tear the context down.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum TransportError {
    /// The bytes themselves were wrong — truncated, an impossible array
    /// length, an unknown pNext `sType`, a chain deeper than the host will
    /// walk.
    #[error(transparent)]
    Wire(#[from] WireError),

    /// An opcode with no decoder here. Fatal by construction: see the module
    /// docs on framing.
    #[error(
        "opcode {opcode} is not a Venus transport command, and a stream with no length field \
         cannot step over one"
    )]
    UnknownOpcode {
        /// The `VkCommandTypeEXT` the guest wrote.
        opcode: u32,
    },

    /// Flag bits outside [`KNOWN_COMMAND_FLAGS`].
    #[error("{command} carries command flag bits {unknown:#x} that this host does not define")]
    UnknownCommandFlags {
        /// The command that carried them.
        command: &'static str,
        /// Only the bits this host has no meaning for.
        unknown: u32,
    },

    /// `vkCreateRingMESA` with a null `pCreateInfo`. The reference calls this
    /// fatal too (`vn_protocol_renderer_transport.h:389-392`), and it must: the
    /// struct is the whole content of the command.
    #[error(
        "vkCreateRingMESA carries a null pCreateInfo, so there is nothing to create a ring from"
    )]
    NullCreateInfo,

    /// `vkSetReplyCommandStreamMESA` with a null `pStream`, likewise fatal in
    /// the reference (`vn_protocol_renderer_transport.h:281-291`).
    #[error("vkSetReplyCommandStreamMESA carries a null pStream, so there is nowhere to reply")]
    NullCommandStream,

    /// The `VkRingCreateInfoMESA` did not announce itself as one.
    #[error(
        "VkRingCreateInfoMESA announces sType {found:#x}, not the {expected:#x} it must carry"
    )]
    WrongStructureType {
        /// What the guest wrote.
        found: i32,
        /// [`STYPE_RING_CREATE_INFO_MESA`].
        expected: i32,
    },

    /// The same extension appeared twice in one pNext chain.
    ///
    /// Ours, not the reference's: `vkr_find_struct` simply takes the first
    /// match and never looks for a second. Vulkan does not allow a structure
    /// type to appear twice in a chain, and accepting it would mean deciding
    /// which of two conflicting ring priorities the guest meant — with the
    /// extra trap that the chain's bodies decode back-to-front (see
    /// [`RingCreateInfoPnext`]), so "the first one" is not the first one
    /// decoded.
    #[error("the pNext chain of VkRingCreateInfoMESA carries sType {stype:#x} more than once")]
    DuplicatePnextStype {
        /// The `VkStructureType` that repeated.
        stype: i32,
    },
}

/// A `VkCommandStreamDescriptionMESA`: a range of a shared-memory resource
/// holding commands, or a place to put replies.
///
/// Entirely untrusted. `resource_id` names nothing until the layer that owns
/// resources looks it up, and `offset`/`size` are guest `size_t`s that have
/// been range-checked against nothing at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct CommandStreamDescription {
    /// The shared-memory resource the range lives in.
    pub resource_id: u32,
    /// Where the range starts inside it.
    pub offset: u64,
    /// How long the range is.
    pub size: u64,
}

impl CommandStreamDescription {
    /// Decode one, per `vn_decode_VkCommandStreamDescriptionMESA_temp`
    /// (`vn_protocol_renderer_transport.h:23-28`). Consumes exactly
    /// [`COMMAND_STREAM_DESCRIPTION_WIRE_LEN`] bytes.
    ///
    /// # Errors
    /// [`WireError::Truncated`], or [`WireError::Poisoned`] on a dead stream.
    fn decode(dec: &mut Decoder<'_>) -> Result<Self, WireError> {
        let resource_id = dec.u32()?;
        let offset = dec.size()?;
        let size = dec.size()?;
        Ok(Self {
            resource_id,
            offset,
            size,
        })
    }
}

/// A `VkCommandStreamDependencyMESA`: an ordering edge between two of the
/// streams named in the same `vkExecuteCommandStreamsMESA`.
///
/// The indices are untrusted and are not checked against the stream count
/// here — that is the executor's check, and it has the streams to check
/// against.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct CommandStreamDependency {
    /// Index of the stream that must run first.
    pub src_command_stream: u32,
    /// Index of the stream that must run after it.
    pub dst_command_stream: u32,
}

impl CommandStreamDependency {
    /// Decode one, per `vn_decode_VkCommandStreamDependencyMESA_temp`
    /// (`vn_protocol_renderer_transport.h:40-45`). Consumes exactly
    /// [`COMMAND_STREAM_DEPENDENCY_WIRE_LEN`] bytes.
    ///
    /// # Errors
    /// As [`CommandStreamDescription::decode`].
    fn decode(dec: &mut Decoder<'_>) -> Result<Self, WireError> {
        let src_command_stream = dec.u32()?;
        let dst_command_stream = dec.u32()?;
        Ok(Self {
            src_command_stream,
            dst_command_stream,
        })
    }
}

/// The pNext whitelist of `VkRingCreateInfoMESA`
/// (`vn_decode_VkRingCreateInfoMESA_pnext_temp`,
/// `vn_protocol_renderer_transport.h:165-199`): a ring monitor period, a ring
/// priority, and nothing else.
///
/// The traversal order is the generated decoder's and it is not the order
/// anyone guesses: the chain's *markers and `sType`s* are written front to
/// back, then the terminating null, then the **bodies back to front**, because
/// each link decodes the whole rest of the chain before its own fields. So
/// [`PnextVisitor::visit`] is called on the deepest link first. Nothing here
/// depends on that order — duplicates are refused rather than resolved — but
/// anything that ever does must know it.
#[derive(Debug, Default)]
struct RingCreateInfoPnext {
    monitor_period_us: Option<u32>,
    priority: Option<i32>,
    duplicate: Option<i32>,
}

impl<'a> PnextVisitor<'a> for RingCreateInfoPnext {
    const PARENT: &'static str = "VkRingCreateInfoMESA";

    fn visit(&mut self, stype: i32, dec: &mut Decoder<'a>) -> Result<(), WireError> {
        match stype {
            STYPE_RING_MONITOR_INFO_MESA => {
                // `VkRingMonitorInfoMESA::maxReportingPeriodMicroseconds`.
                let period = dec.u32()?;
                if self.monitor_period_us.replace(period).is_some() {
                    self.duplicate.get_or_insert(stype);
                }
                Ok(())
            }
            STYPE_RING_PRIORITY_INFO_MESA => {
                // `VkRingPriorityInfoMESA::priority`, a signed enum.
                let priority = dec.i32()?;
                if self.priority.replace(priority).is_some() {
                    self.duplicate.get_or_insert(stype);
                }
                Ok(())
            }
            _ => Err(Self::unknown(stype)),
        }
    }
}

/// One decoded transport command.
///
/// Every field is guest input that has been checked for *shape* and nothing
/// else: handles name nothing yet, resource ids name nothing yet, and the
/// [`RingCreateInfo`] is exactly as untrusted as the bytes it came from — it
/// is [`ring::RingLayout::new`] that decides whether it describes a ring this
/// host will serve.
///
/// [`ring::RingLayout::new`]: super::ring::RingLayout::new
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TransportCommand {
    /// `vkSetReplyCommandStreamMESA`: point the reply encoder at a range.
    SetReplyCommandStream {
        /// Where replies are to be written.
        stream: CommandStreamDescription,
    },

    /// `vkSeekReplyCommandStreamMESA`: move the reply cursor.
    SeekReplyCommandStream {
        /// Byte offset within the reply stream.
        position: u64,
    },

    /// `vkExecuteCommandStreamsMESA`: run the commands in `streams`, in order.
    ExecuteCommandStreams {
        /// The ranges to execute. Empty when the guest sent a null array,
        /// which the cross-check makes equivalent to a zero `streamCount` —
        /// and which the executor refuses (`vkr_transport.c`: "no stream
        /// specified").
        streams: Vec<CommandStreamDescription>,
        /// Where to seek the reply stream before each of `streams`. `None` is
        /// the common case and is *not* the same as an empty list: absent
        /// means "do not seek at all". Present, it has exactly
        /// `streams.len()` entries.
        reply_positions: Option<Vec<u64>>,
        /// Ordering edges between the streams. Empty when absent.
        dependencies: Vec<CommandStreamDependency>,
        /// `VkCommandStreamExecuteFlagsMESA`, carried whole: no bit is defined
        /// in venus-protocol, so there is nothing here to judge.
        flags: u32,
    },

    /// `vkCreateRingMESA`: the one that matters. Everything the guest later
    /// sends arrives through a ring whose shape it proposed here.
    CreateRing {
        /// The id the guest will refer to this ring by. An opaque `uint64_t`;
        /// whether it is already taken is the ring table's business.
        ring: u64,
        /// The proposed layout, **unvalidated**. Feed it to
        /// [`ring::RingLayout::new`] with the resource's real size.
        ///
        /// [`ring::RingLayout::new`]: super::ring::RingLayout::new
        info: RingCreateInfo,
        /// `VkRingMonitorInfoMESA::maxReportingPeriodMicroseconds`, when the
        /// chain carried one. The reference refuses a zero period; that is a
        /// policy check for the layer that starts the monitoring thread, not a
        /// shape check, so it is not made here.
        monitor_period_us: Option<u32>,
        /// `VkRingPriorityInfoMESA::priority`, when the chain carried one.
        priority: Option<i32>,
    },

    /// `vkDestroyRingMESA`.
    DestroyRing {
        /// The ring to tear down.
        ring: u64,
    },

    /// `vkNotifyRingMESA`: the doorbell.
    NotifyRing {
        /// The ring with work.
        ring: u64,
        /// Carried, and ignored by the reference — `vkr_dispatch_vkNotifyRingMESA`
        /// reads neither this nor `flags`, it just wakes the ring. Kept here
        /// because a decoder that drops fields cannot later grow a use for
        /// them, and because dropping it would hide a guest sending nonsense.
        seqno: u32,
        /// Carried and ignored, as `seqno` is.
        flags: u32,
    },

    /// `vkWriteRingExtraMESA`: store one `u32` in a ring's `extra` region.
    WriteRingExtra {
        /// The ring whose `extra` region is written.
        ring: u64,
        /// Offset within that region. Bounds-checked where the region is
        /// known, which is not here.
        offset: u64,
        /// The value to store.
        value: u32,
    },

    /// `vkSubmitVirtqueueSeqnoMESA`.
    SubmitVirtqueueSeqno {
        /// The ring the seqno belongs to.
        ring: u64,
        /// The seqno being published.
        seqno: u64,
    },

    /// `vkWaitVirtqueueSeqnoMESA`. Legal only on a *ring* dispatch; on the
    /// context stream it would block the context, and `vkr_transport.c`
    /// refuses it there. This module cannot tell the two apart — the caller
    /// must.
    WaitVirtqueueSeqno {
        /// The seqno to wait for.
        seqno: u64,
    },

    /// `vkWaitRingSeqnoMESA`.
    WaitRingSeqno {
        /// The ring to wait on.
        ring: u64,
        /// The seqno to wait for.
        seqno: u64,
    },
}

impl TransportCommand {
    /// Which command this is.
    #[must_use]
    pub fn opcode(&self) -> Opcode {
        match self {
            Self::SetReplyCommandStream { .. } => Opcode::SetReplyCommandStream,
            Self::SeekReplyCommandStream { .. } => Opcode::SeekReplyCommandStream,
            Self::ExecuteCommandStreams { .. } => Opcode::ExecuteCommandStreams,
            Self::CreateRing { .. } => Opcode::CreateRing,
            Self::DestroyRing { .. } => Opcode::DestroyRing,
            Self::NotifyRing { .. } => Opcode::NotifyRing,
            Self::WriteRingExtra { .. } => Opcode::WriteRingExtra,
            Self::SubmitVirtqueueSeqno { .. } => Opcode::SubmitVirtqueueSeqno,
            Self::WaitVirtqueueSeqno { .. } => Opcode::WaitVirtqueueSeqno,
            Self::WaitRingSeqno { .. } => Opcode::WaitRingSeqno,
        }
    }

    /// Decode one command's arguments, given the header the caller already
    /// read off `dec`.
    ///
    /// `dec` is left positioned immediately after the command on success, and
    /// **fatal** on every failure — including this layer's own refusals, which
    /// are recorded on the decoder as [`WireError::Poisoned`] so that nothing
    /// can read past them. The refusal itself is what this returns; a caller
    /// that wants it kept should use [`TransportStream`], which does.
    ///
    /// A caller driving this directly owns the per-command allocation budget:
    /// call [`Decoder::reset_alloc_budget`] before each command, as
    /// [`TransportStream::next_command`] does.
    ///
    /// # Errors
    ///
    /// [`TransportError::UnknownOpcode`] for an opcode with no decoder,
    /// [`TransportError::UnknownCommandFlags`] for a flag bit outside
    /// [`KNOWN_COMMAND_FLAGS`], the null-pointer and `sType` refusals for the
    /// two commands that carry structs, or [`TransportError::Wire`] for
    /// anything the byte layer refused.
    pub fn decode(header: CommandHeader, dec: &mut Decoder<'_>) -> Result<Self, TransportError> {
        let Some(opcode) = Opcode::from_u32(header.opcode) else {
            return refuse(
                dec,
                TransportError::UnknownOpcode {
                    opcode: header.opcode,
                },
            );
        };

        let unknown = header.flags & !KNOWN_COMMAND_FLAGS;
        if unknown != 0 {
            return refuse(
                dec,
                TransportError::UnknownCommandFlags {
                    command: opcode.name(),
                    unknown,
                },
            );
        }

        match opcode {
            Opcode::SetReplyCommandStream => decode_set_reply_command_stream(dec),
            Opcode::SeekReplyCommandStream => Ok(Self::SeekReplyCommandStream {
                position: dec.size()?,
            }),
            Opcode::ExecuteCommandStreams => decode_execute_command_streams(dec),
            Opcode::CreateRing => decode_create_ring(dec),
            Opcode::DestroyRing => Ok(Self::DestroyRing {
                ring: dec.handle()?,
            }),
            Opcode::NotifyRing => {
                let ring = dec.handle()?;
                let seqno = dec.u32()?;
                let flags = dec.flags()?;
                Ok(Self::NotifyRing { ring, seqno, flags })
            }
            Opcode::WriteRingExtra => {
                let ring = dec.handle()?;
                let offset = dec.size()?;
                let value = dec.u32()?;
                Ok(Self::WriteRingExtra {
                    ring,
                    offset,
                    value,
                })
            }
            Opcode::SubmitVirtqueueSeqno => {
                let ring = dec.handle()?;
                let seqno = dec.u64()?;
                Ok(Self::SubmitVirtqueueSeqno { ring, seqno })
            }
            Opcode::WaitVirtqueueSeqno => Ok(Self::WaitVirtqueueSeqno { seqno: dec.u64()? }),
            Opcode::WaitRingSeqno => {
                let ring = dec.handle()?;
                let seqno = dec.u64()?;
                Ok(Self::WaitRingSeqno { ring, seqno })
            }
        }
    }
}

/// Record a refusal from this layer on the byte decoder, so that a caller who
/// ignores the `Err` still cannot read another field, and hand the refusal
/// back.
///
/// The decoder is poisoned with [`WireError::Poisoned`] rather than with a
/// wire refusal it did not make: the real reason is this function's `err`, and
/// [`TransportStream`] is what keeps it.
fn refuse<T>(dec: &mut Decoder<'_>, err: TransportError) -> Result<T, TransportError> {
    dec.set_fatal(WireError::Poisoned);
    Err(err)
}

/// `vn_decode_vkSetReplyCommandStreamMESA_args_temp`
/// (`vn_protocol_renderer_transport.h:281-291`).
fn decode_set_reply_command_stream(
    dec: &mut Decoder<'_>,
) -> Result<TransportCommand, TransportError> {
    if !dec.simple_pointer()? {
        return refuse(dec, TransportError::NullCommandStream);
    }
    let stream = CommandStreamDescription::decode(dec)?;
    Ok(TransportCommand::SetReplyCommandStream { stream })
}

/// `vn_decode_vkExecuteCommandStreamsMESA_args_temp`
/// (`vn_protocol_renderer_transport.h:322-351`).
///
/// Two array shapes, and the difference between them is the whole reason this
/// is not four lines. `pStreams` and `pDependencies` cross-check their length
/// word against the count field in **both** branches, so a null array is only
/// legal when the count was zero. `pReplyPositions` does not: the generated
/// code peeks the length, and takes the *unchecked* path when it is zero — so
/// a guest may legitimately omit the reply positions while naming three
/// streams, and refusing that would break every real Mesa submission.
fn decode_execute_command_streams(
    dec: &mut Decoder<'_>,
) -> Result<TransportCommand, TransportError> {
    let stream_count = dec.u32()?;
    let streams = dec
        .array(u64::from(stream_count), |d, count| {
            d.repeat(count, |d| CommandStreamDescription::decode(d))
        })?
        .unwrap_or_default();

    let reply_positions = if dec.peek_array_size()? == 0 {
        // The null branch still owes its eight bytes, and owes them unchecked.
        let _unchecked = dec.array_size_unchecked()?;
        None
    } else {
        let size = dec.array_size(u64::from(stream_count))?;
        // `size` equals `stream_count`, a `u32`, so this only clamps on a host
        // with a 16-bit `usize` — where the array would then be refused for
        // not fitting in the stream rather than silently truncated.
        let count = usize::try_from(size).unwrap_or(usize::MAX);
        Some(dec.u64_array(count)?)
    };

    let dependency_count = dec.u32()?;
    let dependencies = dec
        .array(u64::from(dependency_count), |d, count| {
            d.repeat(count, |d| CommandStreamDependency::decode(d))
        })?
        .unwrap_or_default();

    let flags = dec.flags()?;
    Ok(TransportCommand::ExecuteCommandStreams {
        streams,
        reply_positions,
        dependencies,
        flags,
    })
}

/// `vn_decode_vkCreateRingMESA_args_temp`
/// (`vn_protocol_renderer_transport.h:385-395`) and the struct decoder it
/// calls (`:225-236`, body at `:208-223`).
///
/// A struct is `sType`, then the **whole** pNext chain, then its own body —
/// pNext before the body, which is not the order anyone guesses.
fn decode_create_ring(dec: &mut Decoder<'_>) -> Result<TransportCommand, TransportError> {
    let ring = dec.handle()?;
    if !dec.simple_pointer()? {
        return refuse(dec, TransportError::NullCreateInfo);
    }

    let found = dec.structure_type()?;
    if found != STYPE_RING_CREATE_INFO_MESA {
        // The reference sets fatal here and then keeps decoding the body
        // anyway; there is nothing to gain from reading fields out of a struct
        // that has already said it is a different struct.
        return refuse(
            dec,
            TransportError::WrongStructureType {
                found,
                expected: STYPE_RING_CREATE_INFO_MESA,
            },
        );
    }

    let mut chain = RingCreateInfoPnext::default();
    let _links = dec.pnext_chain(&mut chain)?;
    if let Some(stype) = chain.duplicate {
        return refuse(dec, TransportError::DuplicatePnextStype { stype });
    }

    // Wire order, and it is the only order: no field here announces its own
    // width and a misordered read desynchronises everything after it.
    let flags = dec.flags()?;
    let resource_id = dec.u32()?;
    let offset = dec.size()?;
    let size = dec.size()?;
    let idle_timeout_ns = dec.u64()?;
    let head_offset = dec.size()?;
    let tail_offset = dec.size()?;
    let status_offset = dec.size()?;
    let buffer_offset = dec.size()?;
    let buffer_size = dec.size()?;
    let extra_offset = dec.size()?;
    let extra_size = dec.size()?;

    Ok(TransportCommand::CreateRing {
        ring,
        info: RingCreateInfo {
            flags,
            resource_id,
            offset,
            size,
            idle_timeout_ns,
            head_offset,
            tail_offset,
            status_offset,
            buffer_offset,
            buffer_size,
            extra_offset,
            extra_size,
        },
        monitor_period_us: chain.monitor_period_us,
        priority: chain.priority,
    })
}

/// A decoded command together with the header that framed it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransportRequest {
    /// The eight-byte header: the opcode, and the flags word whose only
    /// defined bit asks for a reply.
    pub header: CommandHeader,
    /// What the guest asked for.
    pub command: TransportCommand,
}

impl TransportRequest {
    /// Whether the guest set `VK_COMMAND_GENERATE_REPLY_BIT_EXT`.
    #[must_use]
    pub fn wants_reply(&self) -> bool {
        self.header.wants_reply()
    }
}

/// The dispatcher over one span of context-stream bytes: header, command,
/// header, command, until the bytes run out or something is wrong.
///
/// This is the type that owns the two protocol-level obligations a caller
/// should not have to remember:
///
/// * **the per-command allocation budget is reset before every command**, as
///   `vn_cs_decoder_reset_temp_pool` is called at the tail of every
///   `vn_dispatch_*`, so a batch cannot starve on its own earlier arrays; and
/// * **a refusal ends the stream.** After one,
///   [`next_command`](Self::next_command) yields `None` forever and
///   [`fatal_error`](Self::fatal_error) holds the reason. `None` therefore
///   means "no more commands", not "all is well" — check
///   [`is_fatal`](Self::is_fatal) when the loop ends. It is deliberately not
///   an endless stream of `Err`s: a caller that ignores errors should
///   terminate, not spin.
#[derive(Debug)]
pub struct TransportStream<'a> {
    dec: Decoder<'a>,
    fatal: Option<TransportError>,
}

impl<'a> TransportStream<'a> {
    /// A dispatcher over `bytes`, which are a *copy* of whatever the guest
    /// wrote — nothing here re-reads memory a guest can still change.
    #[must_use]
    pub fn new(bytes: &'a [u8]) -> Self {
        Self {
            dec: Decoder::new(bytes),
            fatal: None,
        }
    }

    /// Offset of the next unread byte. Frozen once the stream has refused.
    #[must_use]
    pub fn position(&self) -> usize {
        self.dec.position()
    }

    /// Bytes not yet consumed.
    #[must_use]
    pub fn remaining(&self) -> usize {
        self.dec.remaining()
    }

    /// Host bytes the command *just decoded* was charged for, subtracted from
    /// the full budget it started with. Reset before every command.
    #[must_use]
    pub fn alloc_budget(&self) -> usize {
        self.dec.alloc_budget()
    }

    /// Whether this stream has refused something.
    #[must_use]
    pub fn is_fatal(&self) -> bool {
        self.fatal.is_some() || self.dec.is_fatal()
    }

    /// The first refusal, if there was one. Everything after it was never
    /// read.
    #[must_use]
    pub fn fatal_error(&self) -> Option<TransportError> {
        self.fatal
    }

    /// The next command, `None` at the end of the stream **or** after a
    /// refusal.
    ///
    /// The allocation budget is restored here, before the header is read, so a
    /// long batch of array-carrying commands cannot starve on its own earlier
    /// arrays. That reset never clears the fatal flag — the reference does not
    /// clear its own either, and a stream that has gone wrong has nothing left
    /// to say.
    ///
    /// # Errors
    ///
    /// Whatever [`TransportCommand::decode`] refused, or a truncated header.
    /// The refusal is returned once and kept in
    /// [`fatal_error`](Self::fatal_error).
    pub fn next_command(&mut self) -> Option<Result<TransportRequest, TransportError>> {
        if self.is_fatal() || self.dec.remaining() == 0 {
            return None;
        }
        self.dec.reset_alloc_budget();
        Some(self.decode_one())
    }

    fn decode_one(&mut self) -> Result<TransportRequest, TransportError> {
        let header = match self.dec.command_header() {
            Ok(header) => header,
            Err(err) => return Err(self.remember(err.into())),
        };
        match TransportCommand::decode(header, &mut self.dec) {
            Ok(command) => Ok(TransportRequest { header, command }),
            Err(err) => Err(self.remember(err)),
        }
    }

    /// Keep the first refusal and hand it back.
    fn remember(&mut self, err: TransportError) -> TransportError {
        *self.fatal.get_or_insert(err)
    }
}

#[cfg(test)]
mod tests {
    use std::mem::size_of;

    use super::super::wire::{
        Encoder, COMMAND_HEADER_BYTES, MAX_PNEXT_DEPTH, MAX_TEMP_ALLOC_BYTES,
    };
    use super::*;

    // ---- byte-stream builders ----------------------------------------------

    /// A chainable encoder. Tests are allowed to panic; guest paths are not.
    #[derive(Debug)]
    struct Bytes(Encoder);

    impl Bytes {
        fn new() -> Self {
            Self(Encoder::new())
        }

        fn header(self, opcode: Opcode, flags: u32) -> Self {
            self.raw_header(opcode.as_u32(), flags)
        }

        fn raw_header(mut self, opcode: u32, flags: u32) -> Self {
            self.0
                .command_header(CommandHeader { opcode, flags })
                .expect("encode header");
            self
        }

        fn u32(mut self, value: u32) -> Self {
            self.0.u32(value).expect("encode u32");
            self
        }

        fn i32(mut self, value: i32) -> Self {
            self.0.i32(value).expect("encode i32");
            self
        }

        fn u64(mut self, value: u64) -> Self {
            self.0.u64(value).expect("encode u64");
            self
        }

        fn ptr(self, present: bool) -> Self {
            self.u64(u64::from(present))
        }

        fn description(self, desc: CommandStreamDescription) -> Self {
            self.u32(desc.resource_id).u64(desc.offset).u64(desc.size)
        }

        fn dependency(self, dep: CommandStreamDependency) -> Self {
            self.u32(dep.src_command_stream).u32(dep.dst_command_stream)
        }

        fn ring_create_info(self, info: RingCreateInfo, chain: &[Link]) -> Self {
            self.i32(STYPE_RING_CREATE_INFO_MESA)
                .chain(chain)
                .u32(info.flags)
                .u32(info.resource_id)
                .u64(info.offset)
                .u64(info.size)
                .u64(info.idle_timeout_ns)
                .u64(info.head_offset)
                .u64(info.tail_offset)
                .u64(info.status_offset)
                .u64(info.buffer_offset)
                .u64(info.buffer_size)
                .u64(info.extra_offset)
                .u64(info.extra_size)
        }

        /// A pNext chain as the generated decoder reads it: every link's
        /// marker and `sType` front to back, the terminating null, then the
        /// bodies **back to front**.
        fn chain(mut self, links: &[Link]) -> Self {
            for link in links {
                self = self.ptr(true).i32(link.stype());
            }
            self = self.ptr(false);
            for link in links.iter().rev() {
                self = link.body(self);
            }
            self
        }

        fn finish(self) -> Vec<u8> {
            self.0.finish().expect("encode")
        }
    }

    #[derive(Debug, Clone, Copy)]
    enum Link {
        Monitor(u32),
        Priority(i32),
        Unknown(i32),
    }

    impl Link {
        fn stype(self) -> i32 {
            match self {
                Self::Monitor(_) => STYPE_RING_MONITOR_INFO_MESA,
                Self::Priority(_) => STYPE_RING_PRIORITY_INFO_MESA,
                Self::Unknown(stype) => stype,
            }
        }

        fn body(self, bytes: Bytes) -> Bytes {
            match self {
                Self::Monitor(period) => bytes.u32(period),
                Self::Priority(priority) => bytes.i32(priority),
                // A body the host would have had to know the length of.
                Self::Unknown(_) => bytes.u32(0xdead_beef),
            }
        }
    }

    fn sample_info() -> RingCreateInfo {
        RingCreateInfo {
            flags: 0x0000_0001,
            resource_id: 7,
            offset: 0x1000,
            size: 0x4000,
            idle_timeout_ns: 1_000_000,
            head_offset: 0x0,
            tail_offset: 0x4,
            status_offset: 0x8,
            buffer_offset: 0x40,
            buffer_size: 0x1000,
            extra_offset: 0x2000,
            extra_size: 0x100,
        }
    }

    fn desc(resource_id: u32, offset: u64, size: u64) -> CommandStreamDescription {
        CommandStreamDescription {
            resource_id,
            offset,
            size,
        }
    }

    fn create_ring_bytes(ring: u64, info: RingCreateInfo, chain: &[Link]) -> Vec<u8> {
        Bytes::new()
            .header(Opcode::CreateRing, 0)
            .u64(ring)
            .ptr(true)
            .ring_create_info(info, chain)
            .finish()
    }

    /// Decode exactly one command out of `bytes` and assert it consumed all of
    /// them and left the stream clean.
    fn decode_one(bytes: &[u8]) -> TransportRequest {
        let mut stream = TransportStream::new(bytes);
        let request = stream
            .next_command()
            .expect("a command")
            .expect("a well-formed command");
        assert_eq!(stream.position(), bytes.len(), "consumed the whole command");
        assert_eq!(stream.remaining(), 0);
        assert!(!stream.is_fatal());
        assert!(stream.next_command().is_none(), "nothing left over");
        request
    }

    fn refusal(bytes: &[u8]) -> TransportError {
        let mut stream = TransportStream::new(bytes);
        let err = stream
            .next_command()
            .expect("a command was attempted")
            .expect_err("a refusal");
        assert!(stream.is_fatal());
        assert_eq!(stream.fatal_error(), Some(err), "the refusal is kept");
        assert!(
            stream.next_command().is_none(),
            "a refused stream yields nothing more"
        );
        err
    }

    // ---- the opcodes --------------------------------------------------------

    #[test]
    fn opcodes_are_the_generated_values() {
        // vn_protocol_renderer_defines.h:379-392. These are what a guest
        // writes; getting one wrong is not a compile error anywhere.
        for (opcode, value, name) in [
            (
                Opcode::SetReplyCommandStream,
                178,
                "vkSetReplyCommandStreamMESA",
            ),
            (
                Opcode::SeekReplyCommandStream,
                179,
                "vkSeekReplyCommandStreamMESA",
            ),
            (
                Opcode::ExecuteCommandStreams,
                180,
                "vkExecuteCommandStreamsMESA",
            ),
            (Opcode::CreateRing, 188, "vkCreateRingMESA"),
            (Opcode::DestroyRing, 189, "vkDestroyRingMESA"),
            (Opcode::NotifyRing, 190, "vkNotifyRingMESA"),
            (Opcode::WriteRingExtra, 191, "vkWriteRingExtraMESA"),
            (
                Opcode::SubmitVirtqueueSeqno,
                251,
                "vkSubmitVirtqueueSeqnoMESA",
            ),
            (Opcode::WaitVirtqueueSeqno, 252, "vkWaitVirtqueueSeqnoMESA"),
            (Opcode::WaitRingSeqno, 253, "vkWaitRingSeqnoMESA"),
        ] {
            assert_eq!(opcode.as_u32(), value, "{name}");
            assert_eq!(Opcode::from_u32(value), Some(opcode), "{name}");
            assert_eq!(opcode.name(), name);
        }
    }

    #[test]
    fn the_gaps_in_the_opcode_block_are_not_transport_commands() {
        // 181..187 and 192 sit between the transport opcodes and belong to
        // other dispatch tables; 283 is past the end of the reference's.
        for opcode in [0, 177, 181, 187, 192, 250, 254, 283, u32::MAX] {
            assert_eq!(Opcode::from_u32(opcode), None, "opcode {opcode}");
        }
    }

    #[test]
    fn the_struct_wire_lengths_are_the_generated_ones() {
        assert_eq!(COMMAND_STREAM_DESCRIPTION_WIRE_LEN, 20);
        assert_eq!(COMMAND_STREAM_DEPENDENCY_WIRE_LEN, 8);
        assert_eq!(RingCreateInfo::WIRE_LEN, 88);
        assert_eq!(RING_CREATE_INFO_MIN_WIRE_LEN, 100);
    }

    // ---- one command at a time ---------------------------------------------

    #[test]
    fn set_reply_command_stream_decodes_and_costs_36_bytes() {
        let stream = desc(9, 0x2000, 0x800);
        let bytes = Bytes::new()
            .header(Opcode::SetReplyCommandStream, 0)
            .ptr(true)
            .description(stream)
            .finish();
        assert_eq!(bytes.len(), COMMAND_HEADER_BYTES + 8 + 20);

        let request = decode_one(&bytes);
        assert_eq!(
            request.command,
            TransportCommand::SetReplyCommandStream { stream }
        );
        assert!(!request.wants_reply());
    }

    #[test]
    fn seek_reply_command_stream_decodes_and_costs_16_bytes() {
        let bytes = Bytes::new()
            .header(Opcode::SeekReplyCommandStream, 0)
            .u64(0xdead_0000)
            .finish();
        assert_eq!(bytes.len(), 16);
        assert_eq!(
            decode_one(&bytes).command,
            TransportCommand::SeekReplyCommandStream {
                position: 0xdead_0000
            }
        );
    }

    #[test]
    fn destroy_ring_decodes_and_costs_16_bytes() {
        let bytes = Bytes::new()
            .header(Opcode::DestroyRing, 0)
            .u64(0x1234_5678_9abc_def0)
            .finish();
        assert_eq!(bytes.len(), 16);
        assert_eq!(
            decode_one(&bytes).command,
            TransportCommand::DestroyRing {
                ring: 0x1234_5678_9abc_def0
            }
        );
    }

    #[test]
    fn notify_ring_carries_the_seqno_and_flags_the_reference_ignores() {
        let bytes = Bytes::new()
            .header(Opcode::NotifyRing, 0)
            .u64(42)
            .u32(0x0102_0304)
            .u32(0xffff_ffff)
            .finish();
        assert_eq!(bytes.len(), 24);
        assert_eq!(
            decode_one(&bytes).command,
            TransportCommand::NotifyRing {
                ring: 42,
                seqno: 0x0102_0304,
                flags: 0xffff_ffff,
            }
        );
    }

    #[test]
    fn write_ring_extra_decodes_and_costs_28_bytes() {
        let bytes = Bytes::new()
            .header(Opcode::WriteRingExtra, 0)
            .u64(42)
            .u64(0x40)
            .u32(0xcafe_f00d)
            .finish();
        assert_eq!(bytes.len(), 28);
        assert_eq!(
            decode_one(&bytes).command,
            TransportCommand::WriteRingExtra {
                ring: 42,
                offset: 0x40,
                value: 0xcafe_f00d,
            }
        );
    }

    #[test]
    fn submit_virtqueue_seqno_decodes_and_costs_24_bytes() {
        let bytes = Bytes::new()
            .header(Opcode::SubmitVirtqueueSeqno, 0)
            .u64(42)
            .u64(9_000_000_000)
            .finish();
        assert_eq!(bytes.len(), 24);
        assert_eq!(
            decode_one(&bytes).command,
            TransportCommand::SubmitVirtqueueSeqno {
                ring: 42,
                seqno: 9_000_000_000,
            }
        );
    }

    #[test]
    fn wait_virtqueue_seqno_decodes_and_costs_16_bytes() {
        let bytes = Bytes::new()
            .header(Opcode::WaitVirtqueueSeqno, 0)
            .u64(7)
            .finish();
        assert_eq!(bytes.len(), 16);
        assert_eq!(
            decode_one(&bytes).command,
            TransportCommand::WaitVirtqueueSeqno { seqno: 7 }
        );
    }

    #[test]
    fn wait_ring_seqno_decodes_and_costs_24_bytes() {
        let bytes = Bytes::new()
            .header(Opcode::WaitRingSeqno, 0)
            .u64(42)
            .u64(7)
            .finish();
        assert_eq!(bytes.len(), 24);
        assert_eq!(
            decode_one(&bytes).command,
            TransportCommand::WaitRingSeqno { ring: 42, seqno: 7 }
        );
    }

    // ---- vkCreateRingMESA ---------------------------------------------------

    #[test]
    fn create_ring_without_extensions_decodes_and_costs_124_bytes() {
        let info = sample_info();
        let bytes = create_ring_bytes(0xabcd, info, &[]);
        assert_eq!(
            bytes.len(),
            COMMAND_HEADER_BYTES + 8 + 8 + RING_CREATE_INFO_MIN_WIRE_LEN
        );
        assert_eq!(bytes.len(), 124);

        assert_eq!(
            decode_one(&bytes).command,
            TransportCommand::CreateRing {
                ring: 0xabcd,
                info,
                monitor_period_us: None,
                priority: None,
            }
        );
    }

    #[test]
    fn create_ring_picks_up_both_known_pnext_extensions() {
        let info = sample_info();
        for chain in [
            vec![Link::Monitor(5_000), Link::Priority(-3)],
            vec![Link::Priority(-3), Link::Monitor(5_000)],
        ] {
            let bytes = create_ring_bytes(1, info, &chain);
            // Two links: marker + sType + body each, plus the null terminator.
            assert_eq!(bytes.len(), 124 + 2 * (8 + 4 + 4));
            assert_eq!(
                decode_one(&bytes).command,
                TransportCommand::CreateRing {
                    ring: 1,
                    info,
                    monitor_period_us: Some(5_000),
                    priority: Some(-3),
                },
                "chain {chain:?}"
            );
        }
    }

    #[test]
    fn create_ring_reads_the_chain_bodies_back_to_front() {
        // Hand-built, so the assertion does not merely agree with the builder:
        // markers and sTypes front to back, terminator, bodies in reverse.
        let info = sample_info();
        let bytes = Bytes::new()
            .header(Opcode::CreateRing, 0)
            .u64(1)
            .ptr(true)
            .i32(STYPE_RING_CREATE_INFO_MESA)
            .ptr(true)
            .i32(STYPE_RING_MONITOR_INFO_MESA)
            .ptr(true)
            .i32(STYPE_RING_PRIORITY_INFO_MESA)
            .ptr(false)
            .i32(-3) // the priority body, decoded first
            .u32(5_000) // the monitor body, decoded second
            .u32(info.flags)
            .u32(info.resource_id)
            .u64(info.offset)
            .u64(info.size)
            .u64(info.idle_timeout_ns)
            .u64(info.head_offset)
            .u64(info.tail_offset)
            .u64(info.status_offset)
            .u64(info.buffer_offset)
            .u64(info.buffer_size)
            .u64(info.extra_offset)
            .u64(info.extra_size)
            .finish();

        assert_eq!(
            decode_one(&bytes).command,
            TransportCommand::CreateRing {
                ring: 1,
                info,
                monitor_period_us: Some(5_000),
                priority: Some(-3),
            }
        );
    }

    #[test]
    fn create_ring_with_an_unknown_pnext_link_is_fatal() {
        let bytes = create_ring_bytes(1, sample_info(), &[Link::Unknown(0x7fff_0000)]);
        assert_eq!(
            refusal(&bytes),
            TransportError::Wire(WireError::UnknownPnextStype {
                parent: "VkRingCreateInfoMESA",
                stype: 0x7fff_0000,
            })
        );
    }

    #[test]
    fn create_ring_with_an_unknown_link_behind_a_known_one_is_fatal() {
        // The unknown link is deeper in the chain, so it is the *first* body
        // the decoder reaches — the order that catches a host which only
        // checks the head of the chain.
        let bytes = create_ring_bytes(
            1,
            sample_info(),
            &[Link::Monitor(1), Link::Unknown(0x1234_5678)],
        );
        assert_eq!(
            refusal(&bytes),
            TransportError::Wire(WireError::UnknownPnextStype {
                parent: "VkRingCreateInfoMESA",
                stype: 0x1234_5678,
            })
        );
    }

    #[test]
    fn create_ring_with_a_repeated_pnext_stype_is_fatal() {
        for (chain, stype) in [
            (
                vec![Link::Monitor(1), Link::Monitor(2)],
                STYPE_RING_MONITOR_INFO_MESA,
            ),
            (
                vec![Link::Priority(1), Link::Monitor(9), Link::Priority(2)],
                STYPE_RING_PRIORITY_INFO_MESA,
            ),
        ] {
            let bytes = create_ring_bytes(1, sample_info(), &chain);
            assert_eq!(
                refusal(&bytes),
                TransportError::DuplicatePnextStype { stype },
                "chain {chain:?}"
            );
        }
    }

    #[test]
    fn create_ring_with_a_runaway_pnext_chain_is_fatal() {
        let deep = vec![Link::Monitor(1); MAX_PNEXT_DEPTH as usize * 2];
        let bytes = create_ring_bytes(1, sample_info(), &deep);
        assert_eq!(
            refusal(&bytes),
            TransportError::Wire(WireError::PnextChainTooDeep {
                parent: "VkRingCreateInfoMESA",
            }),
            "the depth cap fires on the way down, before any duplicate does"
        );
    }

    #[test]
    fn create_ring_with_the_wrong_structure_type_is_fatal() {
        let info = sample_info();
        let bytes = Bytes::new()
            .header(Opcode::CreateRing, 0)
            .u64(1)
            .ptr(true)
            .i32(STYPE_RING_MONITOR_INFO_MESA)
            .chain(&[])
            .u32(info.flags)
            .u32(info.resource_id)
            .finish();
        assert_eq!(
            refusal(&bytes),
            TransportError::WrongStructureType {
                found: STYPE_RING_MONITOR_INFO_MESA,
                expected: STYPE_RING_CREATE_INFO_MESA,
            }
        );
    }

    #[test]
    fn create_ring_with_a_null_create_info_is_fatal() {
        let bytes = Bytes::new()
            .header(Opcode::CreateRing, 0)
            .u64(1)
            .ptr(false)
            .finish();
        assert_eq!(refusal(&bytes), TransportError::NullCreateInfo);
    }

    #[test]
    fn create_ring_carries_its_fields_untouched() {
        // Every offset at its maximum: this layer judges none of them, and
        // handing RingLayout::new something it has already "fixed" would be
        // worse than useless.
        let info = RingCreateInfo {
            flags: u32::MAX,
            resource_id: u32::MAX,
            offset: u64::MAX,
            size: u64::MAX,
            idle_timeout_ns: u64::MAX,
            head_offset: u64::MAX,
            tail_offset: u64::MAX,
            status_offset: u64::MAX,
            buffer_offset: u64::MAX,
            buffer_size: u64::MAX,
            extra_offset: u64::MAX,
            extra_size: u64::MAX,
        };
        let bytes = create_ring_bytes(u64::MAX, info, &[]);
        assert_eq!(
            decode_one(&bytes).command,
            TransportCommand::CreateRing {
                ring: u64::MAX,
                info,
                monitor_period_us: None,
                priority: None,
            }
        );
    }

    #[test]
    fn create_ring_distinguishes_every_field_by_position() {
        // Each field gets a distinct value, so a decoder that reads two of
        // them in the wrong order cannot pass.
        let info = RingCreateInfo {
            flags: 1,
            resource_id: 2,
            offset: 3,
            size: 4,
            idle_timeout_ns: 5,
            head_offset: 6,
            tail_offset: 7,
            status_offset: 8,
            buffer_offset: 9,
            buffer_size: 10,
            extra_offset: 11,
            extra_size: 12,
        };
        let TransportCommand::CreateRing { info: decoded, .. } =
            decode_one(&create_ring_bytes(0, info, &[])).command
        else {
            panic!("expected a CreateRing");
        };
        assert_eq!(decoded, info);
    }

    // ---- vkExecuteCommandStreamsMESA ---------------------------------------

    #[test]
    fn execute_command_streams_decodes_every_array() {
        let streams = [desc(1, 0x10, 0x20), desc(2, 0x30, 0x40)];
        let deps = [CommandStreamDependency {
            src_command_stream: 0,
            dst_command_stream: 1,
        }];
        let bytes = Bytes::new()
            .header(Opcode::ExecuteCommandStreams, 0)
            .u32(2) // streamCount
            .u64(2) // pStreams length
            .description(streams[0])
            .description(streams[1])
            .u64(2) // pReplyPositions length
            .u64(0x100)
            .u64(0x200)
            .u32(1) // dependencyCount
            .u64(1) // pDependencies length
            .dependency(deps[0])
            .u32(0) // flags
            .finish();
        assert_eq!(
            bytes.len(),
            COMMAND_HEADER_BYTES + 4 + 8 + 2 * 20 + 8 + 2 * 8 + 4 + 8 + 8 + 4
        );

        assert_eq!(
            decode_one(&bytes).command,
            TransportCommand::ExecuteCommandStreams {
                streams: streams.to_vec(),
                reply_positions: Some(vec![0x100, 0x200]),
                dependencies: deps.to_vec(),
                flags: 0,
            }
        );
    }

    #[test]
    fn execute_command_streams_null_arrays_still_cost_their_length_words() {
        let bytes = Bytes::new()
            .header(Opcode::ExecuteCommandStreams, 0)
            .u32(0) // streamCount
            .u64(0) // pStreams: null, and the length is still consumed
            .u64(0) // pReplyPositions: null
            .u32(0) // dependencyCount
            .u64(0) // pDependencies: null
            .u32(0x8000_0000) // flags, undefined in venus-protocol, carried whole
            .finish();
        assert_eq!(bytes.len(), COMMAND_HEADER_BYTES + 4 + 8 + 8 + 4 + 8 + 4);
        assert_eq!(
            decode_one(&bytes).command,
            TransportCommand::ExecuteCommandStreams {
                streams: Vec::new(),
                reply_positions: None,
                dependencies: Vec::new(),
                flags: 0x8000_0000,
            }
        );
    }

    #[test]
    fn execute_command_streams_may_omit_only_the_reply_positions() {
        // The asymmetry that matters: pStreams cross-checks its length in both
        // branches, pReplyPositions takes the *unchecked* path when the length
        // is zero. Refusing this would break every real Mesa submission.
        let stream = desc(3, 0, 0x80);
        let bytes = Bytes::new()
            .header(Opcode::ExecuteCommandStreams, 0)
            .u32(1)
            .u64(1)
            .description(stream)
            .u64(0) // no reply positions, though streamCount is 1
            .u32(0)
            .u64(0)
            .u32(0)
            .finish();
        assert_eq!(
            decode_one(&bytes).command,
            TransportCommand::ExecuteCommandStreams {
                streams: vec![stream],
                reply_positions: None,
                dependencies: Vec::new(),
                flags: 0,
            }
        );
    }

    #[test]
    fn execute_command_streams_refuses_a_null_stream_array_it_has_a_count_for() {
        let bytes = Bytes::new()
            .header(Opcode::ExecuteCommandStreams, 0)
            .u32(3) // streamCount
            .u64(0) // ...and no streams
            .finish();
        assert_eq!(
            refusal(&bytes),
            TransportError::Wire(WireError::ArrayLengthMismatch {
                expected: 3,
                found: 0,
            })
        );
    }

    #[test]
    fn execute_command_streams_refuses_a_reply_position_count_that_contradicts_the_streams() {
        let stream = desc(1, 0, 0x40);
        let bytes = Bytes::new()
            .header(Opcode::ExecuteCommandStreams, 0)
            .u32(1)
            .u64(1)
            .description(stream)
            .u64(2) // two reply positions for one stream
            .u64(0)
            .u64(0)
            .finish();
        assert_eq!(
            refusal(&bytes),
            TransportError::Wire(WireError::ArrayLengthMismatch {
                expected: 1,
                found: 2,
            })
        );
    }

    #[test]
    fn execute_command_streams_refuses_a_dependency_count_the_array_contradicts() {
        let bytes = Bytes::new()
            .header(Opcode::ExecuteCommandStreams, 0)
            .u32(0)
            .u64(0)
            .u64(0)
            .u32(2) // dependencyCount
            .u64(1) // one dependency
            .finish();
        assert_eq!(
            refusal(&bytes),
            TransportError::Wire(WireError::ArrayLengthMismatch {
                expected: 2,
                found: 1,
            })
        );
    }

    #[test]
    fn a_vast_stream_count_never_reaches_the_allocator() {
        // 4 billion descriptions would be ~96 GB of host Vec. The stream is
        // 28 bytes long, and that is what refuses it.
        let bytes = Bytes::new()
            .header(Opcode::ExecuteCommandStreams, 0)
            .u32(u32::MAX)
            .u64(u64::from(u32::MAX))
            .finish();
        assert!(matches!(
            refusal(&bytes),
            TransportError::Wire(WireError::ArrayLongerThanStream { .. })
        ));
    }

    #[test]
    fn a_vast_reply_position_count_never_reaches_the_allocator() {
        let bytes = Bytes::new()
            .header(Opcode::ExecuteCommandStreams, 0)
            .u32(u32::MAX)
            .u64(u64::from(u32::MAX))
            // Enough descriptions to keep the first array happy? No — the
            // stream is short, so the *first* array refuses. Use a zero stream
            // count instead to reach the second one.
            .finish();
        assert!(matches!(
            refusal(&bytes),
            TransportError::Wire(WireError::ArrayLongerThanStream { .. })
        ));

        // And the reply-position array on its own: a length that contradicts a
        // zero stream count is caught by the cross-check before any reserve.
        let bytes = Bytes::new()
            .header(Opcode::ExecuteCommandStreams, 0)
            .u32(0)
            .u64(0)
            .u64(u64::MAX)
            .finish();
        assert_eq!(
            refusal(&bytes),
            TransportError::Wire(WireError::ArrayLengthMismatch {
                expected: 0,
                found: u64::MAX,
            })
        );
    }

    // ---- framing refusals ---------------------------------------------------

    #[test]
    fn an_unknown_opcode_is_fatal_and_consumes_only_its_header() {
        for opcode in [0u32, 181, 192, 254, u32::MAX] {
            let bytes = Bytes::new()
                .raw_header(opcode, 0)
                .u64(0xdead_beef)
                .u64(0xfeed_face)
                .finish();
            let mut stream = TransportStream::new(&bytes);
            let err = stream
                .next_command()
                .expect("a command was attempted")
                .expect_err("an unknown opcode is fatal");
            assert_eq!(err, TransportError::UnknownOpcode { opcode });
            assert_eq!(
                stream.position(),
                COMMAND_HEADER_BYTES,
                "not one argument byte was consumed"
            );
            assert!(stream.is_fatal());
            assert!(
                stream.next_command().is_none(),
                "the rest of the stream is unreachable, not skippable"
            );
        }
    }

    #[test]
    fn an_undefined_command_flag_bit_is_refused() {
        let bytes = Bytes::new()
            .header(Opcode::DestroyRing, COMMAND_GENERATE_REPLY | 0x8000_0002)
            .u64(1)
            .finish();
        assert_eq!(
            refusal(&bytes),
            TransportError::UnknownCommandFlags {
                command: "vkDestroyRingMESA",
                unknown: 0x8000_0002,
            }
        );
    }

    #[test]
    fn the_generate_reply_bit_is_accepted_and_reported() {
        let bytes = Bytes::new()
            .header(Opcode::DestroyRing, COMMAND_GENERATE_REPLY)
            .u64(1)
            .finish();
        let request = decode_one(&bytes);
        assert!(request.wants_reply());
        assert_eq!(request.header.flags, COMMAND_GENERATE_REPLY);
        assert_eq!(request.command.opcode(), Opcode::DestroyRing);
    }

    #[test]
    fn a_null_reply_stream_is_fatal() {
        let bytes = Bytes::new()
            .header(Opcode::SetReplyCommandStream, 0)
            .ptr(false)
            .finish();
        assert_eq!(refusal(&bytes), TransportError::NullCommandStream);
    }

    // ---- truncation ---------------------------------------------------------

    /// Every prefix of a well-formed command is refused, and the stream dies.
    fn assert_every_truncation_refused(full: &[u8], what: &str) {
        for len in 1..full.len() {
            let mut stream = TransportStream::new(&full[..len]);
            let attempt = stream
                .next_command()
                .unwrap_or_else(|| panic!("{what}: {len} bytes is still a command attempt"));
            let err = match attempt {
                Ok(command) => panic!("{what}: {len} bytes decoded to {command:?}"),
                Err(err) => err,
            };
            assert!(
                matches!(
                    err,
                    TransportError::Wire(
                        WireError::Truncated { .. } | WireError::ArrayLongerThanStream { .. }
                    )
                ),
                "{what}: {len} bytes gave {err:?}"
            );
            assert!(
                stream.is_fatal(),
                "{what}: {len} bytes left the stream alive"
            );
            assert!(stream.next_command().is_none(), "{what}: {len} bytes");
        }
    }

    #[test]
    fn every_truncation_of_every_fixed_size_command_is_refused() {
        let fixed: [(&str, Vec<u8>); 8] = [
            (
                "vkSetReplyCommandStreamMESA",
                Bytes::new()
                    .header(Opcode::SetReplyCommandStream, 0)
                    .ptr(true)
                    .description(desc(1, 2, 3))
                    .finish(),
            ),
            (
                "vkSeekReplyCommandStreamMESA",
                Bytes::new()
                    .header(Opcode::SeekReplyCommandStream, 0)
                    .u64(1)
                    .finish(),
            ),
            (
                "vkDestroyRingMESA",
                Bytes::new().header(Opcode::DestroyRing, 0).u64(1).finish(),
            ),
            (
                "vkNotifyRingMESA",
                Bytes::new()
                    .header(Opcode::NotifyRing, 0)
                    .u64(1)
                    .u32(2)
                    .u32(3)
                    .finish(),
            ),
            (
                "vkWriteRingExtraMESA",
                Bytes::new()
                    .header(Opcode::WriteRingExtra, 0)
                    .u64(1)
                    .u64(2)
                    .u32(3)
                    .finish(),
            ),
            (
                "vkSubmitVirtqueueSeqnoMESA",
                Bytes::new()
                    .header(Opcode::SubmitVirtqueueSeqno, 0)
                    .u64(1)
                    .u64(2)
                    .finish(),
            ),
            (
                "vkWaitVirtqueueSeqnoMESA",
                Bytes::new()
                    .header(Opcode::WaitVirtqueueSeqno, 0)
                    .u64(1)
                    .finish(),
            ),
            (
                "vkWaitRingSeqnoMESA",
                Bytes::new()
                    .header(Opcode::WaitRingSeqno, 0)
                    .u64(1)
                    .u64(2)
                    .finish(),
            ),
        ];
        for (what, bytes) in &fixed {
            assert_every_truncation_refused(bytes, what);
        }
    }

    #[test]
    fn every_truncation_of_create_ring_is_refused() {
        assert_every_truncation_refused(
            &create_ring_bytes(1, sample_info(), &[]),
            "vkCreateRingMESA",
        );
        assert_every_truncation_refused(
            &create_ring_bytes(1, sample_info(), &[Link::Monitor(9), Link::Priority(-1)]),
            "vkCreateRingMESA with a pNext chain",
        );
    }

    #[test]
    fn every_truncation_of_execute_command_streams_is_refused() {
        let bytes = Bytes::new()
            .header(Opcode::ExecuteCommandStreams, 0)
            .u32(2)
            .u64(2)
            .description(desc(1, 0x10, 0x20))
            .description(desc(2, 0x30, 0x40))
            .u64(2)
            .u64(0x100)
            .u64(0x200)
            .u32(1)
            .u64(1)
            .dependency(CommandStreamDependency {
                src_command_stream: 0,
                dst_command_stream: 1,
            })
            .u32(0)
            .finish();
        assert_every_truncation_refused(&bytes, "vkExecuteCommandStreamsMESA");
    }

    #[test]
    fn an_empty_stream_yields_no_commands_and_is_not_fatal() {
        let mut stream = TransportStream::new(&[]);
        assert!(stream.next_command().is_none());
        assert!(!stream.is_fatal());
        assert_eq!(stream.fatal_error(), None);
        assert_eq!(stream.position(), 0);
    }

    #[test]
    fn a_stray_tail_shorter_than_a_header_is_a_truncated_command_not_a_clean_end() {
        // `vkr_cs_decoder_has_command` is `cur < end`: one leftover byte means
        // there is a command, and it is short.
        let mut bytes = Bytes::new().header(Opcode::DestroyRing, 0).u64(1).finish();
        bytes.extend_from_slice(&[0u8; 4]);

        let mut stream = TransportStream::new(&bytes);
        assert!(stream.next_command().expect("first").is_ok());
        let err = stream
            .next_command()
            .expect("the tail is a command attempt")
            .expect_err("and it is short");
        // The opcode word fits; the flags word is what runs out. A header is
        // eight bytes and both halves are read, so the refusal names offset 20.
        assert_eq!(
            err,
            TransportError::Wire(WireError::Truncated {
                at: 20,
                needed: 4,
                left: 0,
            })
        );
    }

    // ---- batches, stickiness and the budget --------------------------------

    #[test]
    fn a_batch_decodes_in_order_and_leaves_nothing_behind() {
        let info = sample_info();
        let stream_desc = desc(4, 0x80, 0x100);
        let mut bytes = Bytes::new()
            .header(Opcode::SetReplyCommandStream, 0)
            .ptr(true)
            .description(stream_desc)
            .header(Opcode::SeekReplyCommandStream, 0)
            .u64(0x40)
            .finish();
        bytes.extend_from_slice(&create_ring_bytes(0x99, info, &[Link::Priority(2)]));
        bytes.extend_from_slice(
            &Bytes::new()
                .header(Opcode::NotifyRing, COMMAND_GENERATE_REPLY)
                .u64(0x99)
                .u32(1)
                .u32(0)
                .header(Opcode::DestroyRing, 0)
                .u64(0x99)
                .finish(),
        );

        let mut stream = TransportStream::new(&bytes);
        let mut decoded = Vec::new();
        while let Some(result) = stream.next_command() {
            decoded.push(result.expect("a well-formed batch"));
        }
        assert!(!stream.is_fatal(), "the batch ended cleanly");
        assert_eq!(stream.remaining(), 0, "nothing left behind");
        assert_eq!(stream.position(), bytes.len());

        let commands: Vec<TransportCommand> = decoded.iter().map(|r| r.command.clone()).collect();
        assert_eq!(
            commands,
            vec![
                TransportCommand::SetReplyCommandStream {
                    stream: stream_desc
                },
                TransportCommand::SeekReplyCommandStream { position: 0x40 },
                TransportCommand::CreateRing {
                    ring: 0x99,
                    info,
                    monitor_period_us: None,
                    priority: Some(2),
                },
                TransportCommand::NotifyRing {
                    ring: 0x99,
                    seqno: 1,
                    flags: 0,
                },
                TransportCommand::DestroyRing { ring: 0x99 },
            ]
        );
        assert_eq!(
            decoded
                .iter()
                .map(TransportRequest::wants_reply)
                .collect::<Vec<_>>(),
            vec![false, false, false, true, false],
            "only the notify asked for a reply"
        );
    }

    #[test]
    fn a_fatal_stream_stays_fatal_even_when_perfectly_good_commands_follow() {
        let mut bytes = Bytes::new()
            .header(Opcode::DestroyRing, 0)
            .u64(1)
            .raw_header(181, 0) // not a transport opcode
            .u64(0)
            .finish();
        // Three more impeccable commands after the poison.
        for ring in 2..5u64 {
            bytes.extend_from_slice(
                &Bytes::new()
                    .header(Opcode::DestroyRing, 0)
                    .u64(ring)
                    .finish(),
            );
        }

        let mut stream = TransportStream::new(&bytes);
        assert_eq!(
            stream.next_command().expect("first").expect("ok").command,
            TransportCommand::DestroyRing { ring: 1 }
        );
        let err = stream
            .next_command()
            .expect("second")
            .expect_err("the unknown opcode");
        assert_eq!(err, TransportError::UnknownOpcode { opcode: 181 });

        let dead_at = stream.position();
        for _ in 0..5 {
            assert!(stream.next_command().is_none());
            assert!(stream.is_fatal());
            assert_eq!(stream.fatal_error(), Some(err), "the first refusal is kept");
            assert_eq!(stream.position(), dead_at, "and the cursor never moves");
        }
        assert!(stream.remaining() > 0, "the good commands were never read");
    }

    #[test]
    fn a_refusal_from_this_layer_poisons_the_byte_decoder_too() {
        // A caller who drives TransportCommand::decode by hand and drops the
        // Err must still be unable to read another field.
        let bytes = Bytes::new()
            .header(Opcode::CreateRing, 0)
            .u64(1)
            .ptr(false) // null pCreateInfo: this layer's refusal, not wire.rs's
            .u64(0xdead_beef)
            .finish();
        let mut dec = Decoder::new(&bytes);
        let header = dec.command_header().expect("header");
        let err = TransportCommand::decode(header, &mut dec).expect_err("null pCreateInfo");
        assert_eq!(err, TransportError::NullCreateInfo);
        assert!(dec.is_fatal());
        assert_eq!(dec.fatal_error(), Some(WireError::Poisoned));
        assert_eq!(dec.u64(), Err(WireError::Poisoned));
    }

    #[test]
    fn the_allocation_budget_is_reset_between_commands() {
        // Design point: wire.rs charges arrays against a per-command budget
        // and expects the dispatcher to reset it. Two identical commands must
        // therefore cost the same, not twice as much.
        let streams: Vec<CommandStreamDescription> =
            (0..8).map(|i| desc(i, u64::from(i) * 0x10, 0x10)).collect();
        let mut one = Bytes::new()
            .header(Opcode::ExecuteCommandStreams, 0)
            .u32(8)
            .u64(8);
        for stream in &streams {
            one = one.description(*stream);
        }
        let one = one.u64(0).u32(0).u64(0).u32(0).finish();

        let mut both = one.clone();
        both.extend_from_slice(&one);

        let mut stream = TransportStream::new(&both);
        assert!(stream.next_command().expect("first").is_ok());
        let after_first = stream.alloc_budget();
        assert!(stream.next_command().expect("second").is_ok());
        let after_second = stream.alloc_budget();

        assert_eq!(
            after_first, after_second,
            "the second command was charged from a full budget, not the first's remainder"
        );
        assert_eq!(
            MAX_TEMP_ALLOC_BYTES - after_first,
            8 * size_of::<CommandStreamDescription>(),
            "and it was charged exactly what its array costs"
        );
        assert!(!stream.is_fatal());
        assert_eq!(stream.remaining(), 0);
    }

    #[test]
    fn a_fresh_stream_starts_with_the_whole_budget() {
        let stream = TransportStream::new(&[]);
        assert_eq!(stream.alloc_budget(), MAX_TEMP_ALLOC_BYTES);
    }

    #[test]
    fn the_command_the_enum_reports_is_the_opcode_that_decoded_it() {
        let cases = [
            Bytes::new()
                .header(Opcode::SetReplyCommandStream, 0)
                .ptr(true)
                .description(desc(1, 2, 3))
                .finish(),
            Bytes::new()
                .header(Opcode::SeekReplyCommandStream, 0)
                .u64(0)
                .finish(),
            Bytes::new()
                .header(Opcode::ExecuteCommandStreams, 0)
                .u32(0)
                .u64(0)
                .u64(0)
                .u32(0)
                .u64(0)
                .u32(0)
                .finish(),
            create_ring_bytes(1, sample_info(), &[]),
            Bytes::new().header(Opcode::DestroyRing, 0).u64(1).finish(),
            Bytes::new()
                .header(Opcode::NotifyRing, 0)
                .u64(1)
                .u32(0)
                .u32(0)
                .finish(),
            Bytes::new()
                .header(Opcode::WriteRingExtra, 0)
                .u64(1)
                .u64(0)
                .u32(0)
                .finish(),
            Bytes::new()
                .header(Opcode::SubmitVirtqueueSeqno, 0)
                .u64(1)
                .u64(0)
                .finish(),
            Bytes::new()
                .header(Opcode::WaitVirtqueueSeqno, 0)
                .u64(0)
                .finish(),
            Bytes::new()
                .header(Opcode::WaitRingSeqno, 0)
                .u64(1)
                .u64(0)
                .finish(),
        ];
        for bytes in &cases {
            let request = decode_one(bytes);
            assert_eq!(
                request.command.opcode().as_u32(),
                request.header.opcode,
                "{:?}",
                request.command
            );
        }
        assert_eq!(cases.len(), 10, "all ten transport commands are covered");
    }

    /// Seventy-two bytes a real Mesa venus driver sent us.
    ///
    /// Every other test in this file encodes what we believe the protocol to
    /// be and then decodes it, which proves self-consistency and nothing else:
    /// a field misread the same way twice round-trips perfectly. These bytes
    /// were not written by us. They came out of `libvulkan_virtio.so` in an
    /// Ubuntu guest on 2026-09-17, through virtio-gpu, into the capture sink,
    /// and they are the first thing that driver says to a renderer: it points
    /// the ring's reply encoder at a window before it asks anything. There are
    /// two because there were two `VkInstance`s — each one is its own virtio-gpu
    /// context with its own ring and its own reply pool, which is why both
    /// windows start at offset 0 — and the capture sink of that day fed every
    /// ring into one file. It is not a retry.
    ///
    /// What they pin down is exactly what a round trip cannot: that the
    /// command header is `{ opcode, flags }` and not the reverse, that a
    /// `simple_pointer` is a **64-bit** presence marker rather than the 32-bit
    /// one it would be natural to write, and that `VkCommandStreamDescription`
    /// puts its `uint32_t` first and then two `size_t`s with no padding
    /// between them — so a `resource_id` lands 4-mod-8 and the `offset` after
    /// it is unaligned. Get any one of those wrong and this decodes into
    /// plausible garbage.
    #[test]
    fn the_first_bytes_a_real_mesa_venus_driver_sent_us() {
        // vkSetReplyCommandStreamMESA{ resourceId: 8, offset: 0, size: 20 },
        // then the same with resourceId 10.
        const MESA_BRINGUP: [u8; 72] = [
            0xb2, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // opcode 178, flags 0
            0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // pStream is non-null
            0x08, 0x00, 0x00, 0x00, // resourceId
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // offset
            0x14, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // size
            0xb2, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, //
            0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, //
            0x0a, 0x00, 0x00, 0x00, //
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, //
            0x14, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, //
        ];

        let mut stream = TransportStream::new(&MESA_BRINGUP);
        let mut seen = Vec::new();
        while let Some(request) = stream.next_command() {
            seen.push(request.expect("Mesa's own bytes decode"));
        }
        assert_eq!(stream.fatal_error(), None, "nothing in here is a refusal");
        assert_eq!(seen.len(), 2, "two commands, consuming all 72 bytes");

        for (request, resource_id) in seen.iter().zip([8, 10]) {
            assert_eq!(request.command.opcode(), Opcode::SetReplyCommandStream);
            assert!(
                !request.wants_reply(),
                "pointing at the reply window is not itself a question"
            );
            assert_eq!(
                request.command,
                TransportCommand::SetReplyCommandStream {
                    stream: CommandStreamDescription {
                        resource_id,
                        offset: 0,
                        // Twenty bytes: enough for the reply to one query and
                        // no more, which is why the driver resets it so often.
                        size: 20,
                    },
                },
            );
        }
    }
}
