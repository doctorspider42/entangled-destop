## Copyright 2026 The Entangled Desktop authors
## SPDX-License-Identifier: MIT
##
## The Rust side of vn_protocol_renderer_dispatches.h: one opcode switch over
## the generated commands. Mirrors vn_dispatch_command, minus the execution.
<% any_lt = any(M.lifetime[c] for c in M.commands) %>\
//! One switch over every generated command.
//!
//! [`Command::decode`] is `vn_dispatch_command` without the execution: an
//! opcode with no decoder is fatal (there is no length field to step over
//! it), and so is a command flag this host does not define. Transport
//! commands (`vkSetReplyCommandStreamMESA`, `vkExecuteCommandStreamsMESA`,
//! …) are not in here: they are hand-written in `venus::transport`, and a
//! ring dispatcher routes them there by opcode before calling this.

use super::*;
use crate::venus::wire::{CommandHeader, Decoder, Encoder, COMMAND_GENERATE_REPLY};

/// Every generated command, by opcode and name.
pub const GENERATED_COMMANDS: &[(u32, &str)] = &[
% for cmd in M.commands:
    (${R.const_name(cmd)}, "${cmd.name}"),
% endfor
];

/// One decoded command.
#[derive(Debug, Clone, PartialEq)]
pub enum Command${"<'a>" if any_lt else ''} {
% for cmd in M.commands:
    /// `${cmd.name}`.
    ${R.command_variant(cmd)}(${R.command_args_name(cmd)}${"<'a>" if M.lifetime[cmd] else ''}),
% endfor
}

${"impl<'a>" if any_lt else 'impl'} Command${"<'a>" if any_lt else ''} {
    /// Decode the arguments of the command `header` announces.
    ///
    /// The caller has read the header and owns the per-command allocation
    /// budget; [`Command::decode_next`] does both.
    ///
    /// # Errors
    /// [`ProtocolError::UnknownOpcode`], [`ProtocolError::NotGenerated`],
    /// [`ProtocolError::UnknownCommandFlags`], or whatever the command's
    /// decoder refused. `dec` is left fatal on every one.
    pub fn decode(header: CommandHeader, dec: &mut Decoder<${"'a" if any_lt else "'_"}>) -> Result<Self, ProtocolError> {
        let name = match header.opcode {
% for cmd in M.commands:
            ${R.const_name(cmd)} => ${R.command_args_name(cmd)}::NAME,
% endfor
            opcode => {
                dec.set_fatal(crate::venus::wire::WireError::Poisoned);
                return Err(match command_type_name(opcode) {
                    Some(command) => ProtocolError::NotGenerated { opcode, command },
                    None => ProtocolError::UnknownOpcode { opcode },
                });
            }
        };
        let unknown = header.flags & !COMMAND_GENERATE_REPLY;
        if unknown != 0 {
            dec.set_fatal(crate::venus::wire::WireError::Poisoned);
            return Err(ProtocolError::UnknownCommandFlags { command: name, unknown });
        }
        Ok(match header.opcode {
% for cmd in M.commands:
            ${R.const_name(cmd)} => Self::${R.command_variant(cmd)}(${R.command_args_name(cmd)}::decode(dec)?),
% endfor
            opcode => {
                dec.set_fatal(crate::venus::wire::WireError::Poisoned);
                return Err(ProtocolError::UnknownOpcode { opcode });
            }
        })
    }

    /// Reset the allocation budget, read one command header, decode the
    /// command — the reference's per-command `vn_dispatch_command` loop body.
    ///
    /// # Errors
    /// As [`Command::decode`], or a truncated header.
    pub fn decode_next(dec: &mut Decoder<${"'a" if any_lt else "'_"}>) -> Result<(CommandHeader, Self), ProtocolError> {
        dec.reset_alloc_budget();
        let header = dec.command_header()?;
        let command = Self::decode(header, dec)?;
        Ok((header, command))
    }

    /// The command's `VkCommandTypeEXT`.
    #[must_use]
    pub fn opcode(&self) -> u32 {
        match self {
% for cmd in M.commands:
            Self::${R.command_variant(cmd)}(_) => ${R.command_args_name(cmd)}::OPCODE,
% endfor
        }
    }

    /// The command's registry name.
    #[must_use]
    pub fn name(&self) -> &'static str {
        match self {
% for cmd in M.commands:
            Self::${R.command_variant(cmd)}(_) => ${R.command_args_name(cmd)}::NAME,
% endfor
        }
    }

    /// Encode the command as the guest's driver would.
    ///
    /// # Errors
    /// As the command's `encode_command`.
    pub fn encode_command(&self, enc: &mut Encoder, flags: u32) -> Result<(), ProtocolError> {
        match self {
% for cmd in M.commands:
            Self::${R.command_variant(cmd)}(args) => args.encode_command(enc, flags),
% endfor
        }
    }

    /// Encode the reply. On `Err` the encoder holds a partial reply and must
    /// be dropped; [`Command::reply_bytes`] does that for its caller.
    ///
    /// # Errors
    /// As the command's `encode_reply`.
    pub fn encode_reply(&self, enc: &mut Encoder) -> Result<(), ProtocolError> {
        match self {
% for cmd in M.commands:
            Self::${R.command_variant(cmd)}(args) => args.encode_reply(enc),
% endfor
        }
    }

    /// The whole reply, or nothing: a reply that failed halfway is never
    /// handed out, because the guest's decoder has no length field either.
    ///
    /// # Errors
    /// As [`Command::encode_reply`], plus [`crate::venus::wire::WireError::ReplyTooLong`]
    /// past `limit` bytes.
    pub fn reply_bytes(&self, limit: usize) -> Result<Vec<u8>, ProtocolError> {
        let mut enc = Encoder::with_limit(limit);
        self.encode_reply(&mut enc)?;
        Ok(enc.finish()?)
    }

    /// Visit every pNext link the command carries, anywhere in its
    /// arguments, as `(parent structure, sType)`. The chain whitelists are
    /// the protocol's, which is wider than what an executor implements;
    /// this is how an executor judges them.
    pub fn for_each_link(&self, f: &mut dyn FnMut(&'static str, i32)) {
        match self {
% for cmd in M.commands:
%   if M.reach_chain[cmd]:
            Self::${R.command_variant(cmd)}(args) => args.for_each_link(f),
%   endif
% endfor
% if not all(M.reach_chain[c] for c in M.commands):
            _ => {}
% endif
        }
    }

    /// Decode a reply into this command's outputs, as the guest's driver would.
    ///
    /// # Errors
    /// As the command's `decode_reply`.
    pub fn decode_reply(&mut self, dec: &mut Decoder<${"'a" if any_lt else "'_"}>) -> Result<(), ProtocolError> {
        match self {
% for cmd in M.commands:
            Self::${R.command_variant(cmd)}(args) => args.decode_reply(dec),
% endfor
        }
    }
}
