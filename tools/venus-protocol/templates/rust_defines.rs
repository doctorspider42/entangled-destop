## Copyright 2026 The Entangled Desktop authors
## SPDX-License-Identifier: MIT
##
## Constants and type names: the Rust side of vn_protocol_renderer_defines.h,
## narrowed to what the selected commands reach (opcodes are all emitted).
//! Opcodes, structure types, result codes, handle and scalar type names.
//!
//! Handles are newtypes over the guest's **64-bit object id** — the value
//! Mesa's `vn_object_id` counter handed out — and never a host handle. Enums
//! and flag words are type aliases over their wire width: the generated
//! decoders carry them raw, and range checks belong to whoever hands them to
//! a real driver.

#![allow(dead_code)]

// ---- handles ---------------------------------------------------------------
% for ty in M.handles:

/// `${ty.name}`${' (dispatchable)' if ty.dispatchable else ''}: the guest's object id.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, PartialOrd, Ord)]
pub struct ${ty.name}(pub u64);

impl ${ty.name} {
    /// `VK_NULL_HANDLE`.
    pub const NULL: Self = Self(0);

    /// Whether this is `VK_NULL_HANDLE`.
    #[must_use]
    pub fn is_null(self) -> bool {
        self.0 == 0
    }
}
% endfor

// ---- scalar type names -------------------------------------------------------
% for ty in M.aliases:
<%
    prim = M._primitive_of(ty)
    rust = Elem.PRIMS[prim][0]
    what = {ty.ENUM: 'enum', ty.BITMASK: 'flags', ty.BASETYPE: 'basetype'}[ty.category]
%>
/// `${ty.name}` (${what}), as it travels: `${prim}`.
pub type ${ty.name} = ${rust};
% endfor

// ---- enum values the generated code names ----------------------------------
% for key, (rust, value) in sorted(M.enum_consts.items()):

/// `${key}`.
pub const ${key}: ${rust} = ${value};
% endfor

// ---- VkResult ----------------------------------------------------------------
% for key, value in M.vk_results():

/// `${key}`.
pub const ${key}: VkResult = ${value};
% endfor

// ---- VkStructureType, for every generated structure ---------------------------
% for key, value in M.structure_types():

/// `${key}`.
pub const ${key}: i32 = ${value};
% endfor

// ---- VkCommandTypeEXT: every opcode the protocol defines ---------------------
% for const, value, name, primary in M.command_types():

/// `${name}`${'' if primary else ' (alias)'}.
pub const ${const}: u32 = ${value};
% endfor

/// The command an opcode names, by its registry name — for refusal messages
/// and traces. Aliases share their command's value and are not listed.
#[must_use]
pub fn command_type_name(opcode: u32) -> Option<&'static str> {
    match opcode {
% for const, value, name, primary in M.command_types():
%   if primary:
        ${value} => Some("${name}"),
%   endif
% endfor
        _ => None,
    }
}
