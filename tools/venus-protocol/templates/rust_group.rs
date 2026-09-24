## Copyright 2026 The Entangled Desktop authors
## SPDX-License-Identifier: MIT
##
## One group module: the Rust side of vn_protocol_renderer_${GROUP}.h (same
## grouping rules, GenStructsAndCommands.RULES), for the selected commands and
## the structures they reach. Structure codecs mirror types_struct.h and
## types_chain.h, union codecs types_union.h, command codecs types_command.h.
<%def name="lt(ty)">${R.lt(ty)}</%def>\
<%def name="impl_lt(ty)">${"impl<'a>" if M.lifetime.get(ty) else 'impl'}</%def>\
<%def name="dec_lt(ty)">${"'a" if M.lifetime.get(ty) else "'_"}</%def>\
//! ${'Structures shared by more than one command group.' if GROUP == 'structs' else 'The `%s` group: its commands and the structures only they reach.' % GROUP}

#![allow(unused_imports)]

use super::*;
use crate::venus::wire::{CommandHeader, Decoder, Encoder};
% for ty in STRUCTS:
%   if ty.category == VkType.UNION:
<%
    cases = R.union_cases(ty)
    default_f = ty.attrs['rust_default']
    tagged = ty.is_valid_union()
    sel = M.elem_of(ty.sty) if tagged else None
    tag_ty = sel.rust if tagged else 'u32'
    walking = [(f, t) for f, t in cases if M.walks(f)]
%>

/// `union ${ty.name}`${', selected by a `%s` its holder carries' % ty.sty.name if tagged else ', always sent with tag %d (`UNION_DEFAULT_TAGS`)' % ty.attrs['rust_default_tag']}.
///
/// One variant per member. On the wire: the tag, then the member it selects.
#[derive(Debug, Clone, PartialEq)]
pub enum ${ty.name}${lt(ty)} {
%     for f, tags in cases:
    /// ${R.field_doc(f)}, tag ${R.union_tag_pattern(tags).replace(' | ', ', ')}${': the wire carries only its null marker' if f.shape == Field.NULL_ONLY else ''}.
%       if f.shape == Field.NULL_ONLY:
    ${R.union_variant(f)},
%       else:
    ${R.union_variant(f)}(${R.field_type(f)}),
%       endif
%     endfor
}

${impl_lt(ty)} Default for ${ty.name}${lt(ty)} {
    fn default() -> Self {
%     if default_f.shape == Field.NULL_ONLY:
        Self::${R.union_variant(default_f)}
%     else:
        Self::${R.union_variant(default_f)}(${R.default_expr(default_f)})
%     endif
    }
}

${impl_lt(ty)} ${ty.name}${lt(ty)} {
%     if not tagged:
    /// The tag Mesa's encoder always sends for this union.
    pub const DEFAULT_TAG: u32 = ${ty.attrs['rust_default_tag']};

    /// The tag of the member this value holds.
    #[must_use]
    pub fn tag(&self) -> u32 {
        match self {
%       for f, tags in cases:
            Self::${R.union_variant(f)}${'' if f.shape == Field.NULL_ONLY else '(_)'} => ${tags[0][1]},
%       endfor
        }
    }

    /// Decode the tag and the member it selects (`vn_decode_${ty.name}_temp`).
    ///
    /// # Errors
    /// [`ProtocolError::UnknownUnionTag`], or whatever the member refused.
    pub fn decode(dec: &mut Decoder<${dec_lt(ty)}>) -> Result<Self, ProtocolError> {
        let tag = dec.u32()?;
        Ok(match tag {
%       for f, tags in cases:
%         if f.shape == Field.NULL_ONLY:
            ${R.union_tag_pattern(tags)} => {
                ${R.null_only_decode(f, ty.name)}
                Self::${R.union_variant(f)}
            }
%         else:
            ${R.union_tag_pattern(tags)} => Self::${R.union_variant(f)}(${R.union_member_decode(ty, f)}),
%         endif
%       endfor
            _ => return Err(unknown_union_tag(dec, "${ty.name}", i64::from(tag))),
        })
    }

    /// Encode the tag of the member held, then the member. Mesa's encoder
    /// always sends [`Self::DEFAULT_TAG`]; a value decoded with another tag
    /// is written back with the tag it came with.
    ///
    /// # Errors
    /// Whatever the encoder refused.
    pub fn encode(&self, enc: &mut Encoder) -> Result<(), ProtocolError> {
        enc.u32(self.tag())?;
        match self {
%       for f, tags in cases:
%         if f.shape == Field.NULL_ONLY:
            Self::${R.union_variant(f)} => {
                ${R.union_member_encode(f)}
            }
%         else:
            Self::${R.union_variant(f)}(value) => {
                ${R.union_member_encode(f)}
            }
%         endif
%       endfor
        }
        Ok(())
    }
%     else:
    /// Decode the tag and the member it selects
    /// (`vn_decode_${ty.name}_temp`). The tag is returned beside the value:
    /// the structure holding the union checks it against its selector.
    ///
    /// # Errors
    /// [`ProtocolError::UnknownUnionTag`], or whatever the member refused.
    pub fn decode_tagged(dec: &mut Decoder<${dec_lt(ty)}>) -> Result<(${tag_ty}, Self), ProtocolError> {
        let tag = dec.${sel.method}()?;
        let value = match tag {
%       for f, tags in cases:
%         if f.shape == Field.NULL_ONLY:
            ${R.union_tag_pattern(tags)} => {
                ${R.null_only_decode(f, ty.name)}
                Self::${R.union_variant(f)}
            }
%         else:
            ${R.union_tag_pattern(tags)} => Self::${R.union_variant(f)}(${R.union_member_decode(ty, f)}),
%         endif
%       endfor
            _ => return Err(unknown_union_tag(dec, "${ty.name}", i64::from(tag))),
        };
        Ok((tag, value))
    }

    /// Encode `tag` — the holder's selector — and the member it selects
    /// (`vn_encode_${ty.name}(enc, val, tag)`).
    ///
    /// # Errors
    /// [`ProtocolError::UnionTagMismatch`] when `tag` selects a member this
    /// value does not hold, or whatever the encoder refused.
    pub fn encode_tagged(&self, enc: &mut Encoder, tag: ${tag_ty}) -> Result<(), ProtocolError> {
        match (tag, self) {
%       for f, tags in cases:
%         if f.shape == Field.NULL_ONLY:
            (${R.union_tag_pattern(tags)}, Self::${R.union_variant(f)}) => {
                enc.${sel.method}(tag)?;
                ${R.union_member_encode(f)}
            }
%         else:
            (${R.union_tag_pattern(tags)}, Self::${R.union_variant(f)}(value)) => {
                enc.${sel.method}(tag)?;
                ${R.union_member_encode(f)}
            }
%         endif
%       endfor
            _ => {
                return Err(ProtocolError::UnionTagMismatch {
                    union: "${ty.name}",
                    tag: i64::from(tag),
                })
            }
        }
        Ok(())
    }
%     endif
%     if walking:

    /// Visit every pNext link the member held carries, as `(parent, sType)`.
    pub fn for_each_link(&self, f: &mut dyn FnMut(&'static str, i32)) {
        ${R.union_walk(ty)}
    }
%     endif
}
%   else:
<%
    fields = M.fields[ty]
    chain = M.chains[ty]
    has_chain = bool(ty.s_type)
    dec_stmts, dec_uses = R.body_decode(ty)
    enc_stmts, enc_uses = R.body_encode(ty)
    next_name = ty.name + 'Next'
    body = 'body' if has_chain else 'with'
    walk = R.walk_stmts(fields, lambda n: 'self.' + n)
    walking_links = [c for c in chain if M.reach_chain[c]]
%>

/// `${ty.name}`${(' (`%s`)' % ty.s_type) if ty.s_type else ''}.
#[derive(Debug, Clone, PartialEq${', Default' if R.derives_default(ty) else ''})]
pub struct ${ty.name}${lt(ty)} {
%   if chain:
    /// The pNext chain, in the guest's order. Admits: ${', '.join('`%s`' % c.name for c in chain)}.
    pub p_next: Vec<${next_name}${"<'a>" if M.chain_lifetime(ty) else ''}>,
%   endif
%   for f in fields:
%     if f.shape != Field.NULL_ONLY:
    /// ${R.field_doc(f)}
    pub ${f.name}: ${R.field_type(f)},
%     endif
%   endfor
}

%   if not R.derives_default(ty):
${impl_lt(ty)} Default for ${ty.name}${lt(ty)} {
    fn default() -> Self {
        Self {
%   if chain:
            p_next: Vec::new(),
%   endif
%   for f in fields:
%     if f.shape != Field.NULL_ONLY:
            ${f.name}: ${R.default_expr(f)},
%     endif
%   endfor
        }
    }
}
%   endif

${impl_lt(ty)} ${ty.name}${lt(ty)} {
%   if not M.has_partial(ty):
    // No output parameter reaches this structure, so it has no skeleton
    // form and the `partial` flag below is ignored.

%   endif
%   if has_chain:
    /// `${ty.s_type}`.
    pub const STRUCTURE_TYPE: i32 = ${ty.s_type};

%   endif
    /// Decode the whole structure, as an input carries it.
    ///
    /// # Errors
    /// Whatever the wire or this layer refused; `dec` is left fatal.
    pub fn decode(dec: &mut Decoder<${dec_lt(ty)}>) -> Result<Self, ProtocolError> {
        Self::decode_with(dec, false)
    }

%   if M.has_partial(ty):
    /// Decode the skeleton an output structure is sent as inside a command
    /// (`vn_decode_${ty.name}_partial_temp`).
    ///
    /// # Errors
    /// As [`Self::decode`].
    pub fn decode_partial(dec: &mut Decoder<${dec_lt(ty)}>) -> Result<Self, ProtocolError> {
        Self::decode_with(dec, true)
    }
%   endif

    /// Encode the whole structure, as a reply carries it.
    ///
    /// # Errors
    /// Whatever the encoder refused, or an array that disagrees with its count.
    pub fn encode(&self, enc: &mut Encoder) -> Result<(), ProtocolError> {
        self.encode_with(enc, false)
    }

%   if M.has_partial(ty):
    /// Encode the skeleton form (`vn_encode_${ty.name}_partial`).
    ///
    /// # Errors
    /// As [`Self::encode`].
    pub fn encode_partial(&self, enc: &mut Encoder) -> Result<(), ProtocolError> {
        self.encode_with(enc, true)
    }
%   endif

%   if has_chain:
    /// `decode` or `decode_partial`: `sType`, the pNext chain, the body.
    ///
    /// # Errors
    /// As [`Self::decode`].
    pub fn decode_with(dec: &mut Decoder<${dec_lt(ty)}>, partial: bool) -> Result<Self, ProtocolError> {
        expect_structure_type(dec, "${ty.name}", Self::STRUCTURE_TYPE)?;
%     if chain:
        let p_next = decode_chain::<${next_name}>(dec, partial)?;
        let mut value = Self::decode_body(dec, partial)?;
        value.p_next = p_next;
        Ok(value)
%     else:
        dec.empty_pnext_chain("${ty.name}")?;
        Self::decode_body(dec, partial)
%     endif
    }

    /// `encode` or `encode_partial`: `sType`, the pNext chain, the body.
    ///
    /// # Errors
    /// As [`Self::encode`].
    pub fn encode_with(&self, enc: &mut Encoder, partial: bool) -> Result<(), ProtocolError> {
        enc.structure_type(Self::STRUCTURE_TYPE)?;
%     if chain:
        encode_chain(enc, &self.p_next, partial)?;
%     else:
        enc.simple_pointer(false)?;
%     endif
        self.encode_body(enc, partial)
    }

    /// The fields after `sType` and `pNext`, as a chain link carries them
    /// (`vn_decode_${ty.name}_self_temp`).
    ///
    /// # Errors
    /// As [`Self::decode`].
%   else:
    /// `decode` or `decode_partial`.
    ///
    /// # Errors
    /// As [`Self::decode`].
%   endif
    pub fn decode_${body}(${'dec' if dec_stmts else '_dec'}: &mut Decoder<${dec_lt(ty)}>, ${'' if dec_uses else '_'}partial: bool) -> Result<Self, ProtocolError> {
%   for s in dec_stmts:
        ${s}
%   endfor
        Ok(Self {
%   if chain:
            p_next: Vec::new(),
%   endif
%   for f in fields:
%     if f.shape != Field.NULL_ONLY:
            ${f.name},
%     endif
%   endfor
        })
    }

%   if has_chain:
    /// The fields after `sType` and `pNext`, as a chain link carries them.
%   else:
    /// `encode` or `encode_partial`.
%   endif
    ///
    /// # Errors
    /// As [`Self::encode`].
    pub fn encode_${body}(&self, ${'enc' if enc_stmts else '_enc'}: &mut Encoder, ${'' if enc_uses else '_'}partial: bool) -> Result<(), ProtocolError> {
%   for s in enc_stmts:
        ${s}
%   endfor
        Ok(())
    }
%   if M.reach_chain[ty]:

    /// Visit every pNext link this structure carries — its own chain's and
    /// its members' — as `(parent, sType)`.
    pub fn for_each_link(&self, f: &mut dyn FnMut(&'static str, i32)) {
%     if chain:
        for link in &self.p_next {
            f("${ty.name}", link.structure_type());
%       if walking_links:
            link.for_each_link(f);
%       endif
        }
%     endif
%     for s in walk:
        ${s}
%     endfor
    }
%   endif
}
%   if chain:
<% nlt = "<'a>" if M.chain_lifetime(ty) else '' %>

/// A link of `${ty.name}`'s pNext chain.
#[derive(Debug, Clone, PartialEq)]
pub enum ${next_name}${nlt} {
%     for c in chain:
    /// `${c.s_type}`.
    ${c.name}(${c.name}${lt(c)}),
%     endfor
}

impl<'a> ChainLink<'a> for ${next_name}${nlt} {
    const PARENT: &'static str = "${ty.name}";

    fn structure_type(&self) -> i32 {
        match self {
%     for c in chain:
            Self::${c.name}(_) => ${c.s_type},
%     endfor
        }
    }

    fn decode_body(stype: i32, dec: &mut Decoder<'a>, partial: bool) -> Option<Result<Self, ProtocolError>> {
        Some(match stype {
%     for c in chain:
            ${c.s_type} => ${c.name}::decode_body(dec, partial).map(Self::${c.name}),
%     endfor
            _ => return None,
        })
    }

    fn encode_body(&self, enc: &mut Encoder, partial: bool) -> Result<(), ProtocolError> {
        match self {
%     for c in chain:
%       if M.chains[c]:
            Self::${c.name}(link) => {
                if !link.p_next.is_empty() {
                    return Err(ProtocolError::NestedPnextChain { parent: "${ty.name}", link: "${c.name}" });
                }
                link.encode_body(enc, partial)
            }
%       else:
            Self::${c.name}(link) => link.encode_body(enc, partial),
%       endif
%     endfor
        }
    }
}
%     if walking_links:

${"impl<'a>" if nlt else 'impl'} ${next_name}${nlt} {
    /// Visit the pNext links the members of this link carry.
    pub fn for_each_link(&self, f: &mut dyn FnMut(&'static str, i32)) {
        ${R.next_walk(ty)}
    }
}
%     endif
%   endif
%   endif
% endfor
% for cmd in COMMANDS:
<%
    fields = [f for f in M.fields[cmd] if f.shape != Field.NULL_ONLY]
    name = R.command_args_name(cmd)
    ret = R.ret_elem(cmd)
    has_lt = M.lifetime[cmd]
    dlt = "'a" if has_lt else "'_"
    reply_dec = R.reply_decode(cmd)
    walk = R.walk_stmts(fields, lambda n: 'self.' + n)
%>

/// The arguments of `${cmd.name}`, and its reply.
///
/// Inputs are as the guest sent them. Outputs are as the guest *sized*
/// them: present or null, output handles carrying the guest's chosen id,
/// output structures carrying their `sType` and chain skeleton. The executor
/// fills the outputs in place${' and sets `ret`' if cmd.ret else ''}, then
/// [`Self::encode_reply`] writes them back.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct ${name}${"<'a>" if has_lt else ''} {
%   for f in fields:
<%
        v = M.command_validity(cmd, f)
        role = 'in/out' if M.is_out(f) and 'var_in' in f.var.attrs else ('out' if M.is_out(f) else 'in')
%>\
    /// ${R.field_doc(f)} — ${role}.
    pub ${f.name}: ${R.field_type(f)},
%   endfor
%   if cmd.ret:
    /// The return value (`${cmd.ret.ty.name}`), written first in the reply.
    pub ret: ${R.elem_type(ret)},
%   endif
}

${"impl<'a>" if has_lt else 'impl'} ${name}${"<'a>" if has_lt else ''} {
    /// `${cmd.attrs['c_type']}`.
    pub const OPCODE: u32 = ${R.const_name(cmd)};

    /// The command's registry name.
    pub const NAME: &'static str = "${cmd.name}";

    /// Decode the arguments that follow the 8-byte command header
    /// (`vn_decode_${cmd.name}_args_temp`).
    ///
    /// # Errors
    /// Whatever the wire or this layer refused; `dec` is left fatal.
    pub fn decode(dec: &mut Decoder<${dlt}>) -> Result<Self, ProtocolError> {
%   for s in R.command_decode(cmd):
        ${s}
%   endfor
        Ok(Self {
%   for f in fields:
            ${f.name},
%   endfor
%   if cmd.ret:
            ret: Default::default(),
%   endif
        })
    }

    /// Encode the command as the guest's driver does, header included
    /// (`vn_encode_${cmd.name}`): outputs as skeletons.
    ///
    /// # Errors
    /// Whatever the encoder refused, or an array that disagrees with its count.
    pub fn encode_command(&self, enc: &mut Encoder, flags: u32) -> Result<(), ProtocolError> {
        enc.command_header(CommandHeader { opcode: Self::OPCODE, flags })?;
%   for s in R.command_encode(cmd):
        ${s}
%   endfor
        Ok(())
    }

    /// Encode the reply (`vn_encode_${cmd.name}_reply`): the opcode, the
    /// return value if any, then every output parameter.
    ///
    /// On `Err` the encoder holds a partial reply and must be dropped.
    ///
    /// # Errors
    /// Whatever the encoder refused, or an output array that disagrees with
    /// its count.
    pub fn encode_reply(&self, enc: &mut Encoder) -> Result<(), ProtocolError> {
        enc.reply_header(Self::OPCODE)?;
%   for s in R.reply_encode(cmd):
        ${s}
%   endfor
        Ok(())
    }

    /// Decode a reply into this command's outputs, as the guest's driver
    /// does (`vn_decode_${cmd.name}_reply`): arrays in a reply are never
    /// cross-checked in their null branch, and a null output is not fatal.
    ///
    /// # Errors
    /// Whatever the wire or this layer refused.
    pub fn decode_reply(&mut self, dec: &mut Decoder<${dlt}>) -> Result<(), ProtocolError> {
        let found = dec.reply_header()?;
        if found != Self::OPCODE {
            dec.set_fatal(crate::venus::wire::WireError::Poisoned);
            return Err(ProtocolError::WrongReplyOpcode { expected: Self::OPCODE, found });
        }
%   for s in reply_dec:
        ${s}
%   endfor
        Ok(())
    }
%   if M.reach_chain[cmd]:

    /// Visit every pNext link the arguments carry, as `(parent, sType)`.
    pub fn for_each_link(&self, f: &mut dyn FnMut(&'static str, i32)) {
%     for s in walk:
        ${s}
%     endfor
    }
%   endif
}
% endfor
