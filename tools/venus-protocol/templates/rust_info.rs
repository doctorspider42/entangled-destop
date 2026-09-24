## Copyright 2026 The Entangled Desktop authors
## SPDX-License-Identifier: MIT
##
## The Rust side of vn_protocol_renderer_info.h, plus which extensions this
## crate can actually decode and where every chainable structure comes from.
<%
    ver = M.vk_xml_version()
    packed = (ver[0] << 29) | (ver[1] << 22) | (ver[2] << 12) | ver[3]
    amaj, amin = (int(x) for x in M.api().split('.'))
    api_packed = (amaj << 22) | (amin << 12)
    exts = M.extensions()
    decodable = [e for e in exts if e[3]]
    all_mask = M.mask([e[1] for e in exts], 32, True)
    dec_mask = M.mask([e[1] for e in decodable], 32, True)
    origins = M.structure_origins()
%>\
//! What the protocol revision knows, and what this crate decodes of it.
//!
//! The extension table is `vn_protocol_renderer_info.h`'s: every extension in
//! venus-protocol's `VK_XML_EXTENSION_LIST`, sorted by name, with its registry
//! number and spec version. `decodable` marks the ones whose structures the
//! selection file (`[extensions]`) had generated. The capset's
//! `vk_extension_mask1` should advertise [`DECODABLE_EXTENSION_MASK`] and
//! nothing wider: the guest's encoder drops a chained structure whose
//! extension is not in the mask, and any structure it does send that this
//! crate did not generate is fatal on arrival (spec §6; virglrenderer sets
//! the sentinel and enumerates its own list, `vkr_renderer.c:40-48`).
//!
//! Decodable is not *implemented*: which device extensions a guest is
//! offered, and which chained structures an executor accepts, are the
//! executor's policy. [`STRUCTURES`] is the registry fact that policy is
//! written against.

/// `VN_WIRE_FORMAT_VERSION`: must equal the guest's exactly, or Mesa stubs
/// the instance out.
pub const WIRE_FORMAT_VERSION: u32 = ${1};

/// The `vk.xml` the protocol was generated from,
/// `VK_MAKE_API_VERSION(${ver[0]}, ${ver[1]}, ${ver[2]}, ${ver[3]})`.
pub const VK_XML_VERSION: u32 = ${'%#x' % packed};

/// The newest core version whose structures pNext chains may carry here
/// (`[api]` in the selection file), `VK_MAKE_API_VERSION(0, ${amaj}, ${amin}, 0)`.
/// A capset `vk_xml_version` at or below this keeps the guest from chaining
/// a newer core structure.
pub const CHAIN_API_VERSION: u32 = ${'%#x' % api_packed};

/// One entry of the protocol's extension table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExtensionInfo {
    /// `VK_KHR_...`.
    pub name: &'static str,
    /// The registry's extension number: its bit in the capset mask.
    pub number: u32,
    /// `*_SPEC_VERSION` in the registry the protocol was generated from.
    pub spec_version: u32,
    /// Whether every structure it adds to a pNext chain was generated.
    pub decodable: bool,
}

/// Every extension the protocol revision knows, sorted by name.
pub const EXTENSIONS: &[ExtensionInfo] = &[
% for name, number, version, dec in exts:
    ExtensionInfo { name: "${name}", number: ${number}, spec_version: ${version}, decodable: ${'true' if dec else 'false'} },
% endfor
];

/// `vk_extension_mask1` for exactly the decodable extensions, validity
/// sentinel (bit 0) set: the mask the capset should carry.
pub const DECODABLE_EXTENSION_MASK: [u32; 32] = [
% for w in dec_mask:
    ${'%#010x' % w},
% endfor
];

/// `vk_extension_mask1` for every extension in the protocol, sentinel set:
/// what virglrenderer's `vn_info_extension_mask_init` advertises.
pub const PROTOCOL_EXTENSION_MASK: [u32; 32] = [
% for w in all_mask:
    ${'%#010x' % w},
% endfor
];

/// The table entry for `name`.
#[must_use]
pub fn extension(name: &str) -> Option<&'static ExtensionInfo> {
    EXTENSIONS
        .binary_search_by(|e| e.name.cmp(name))
        .ok()
        .and_then(|i| EXTENSIONS.get(i))
}

/// Registry numbers of the decodable extensions, for
/// `capset::ExtensionMask::enumerating`.
pub fn decodable_extension_numbers() -> impl Iterator<Item = u32> {
    EXTENSIONS.iter().filter(|e| e.decodable).map(|e| e.number)
}

/// Where a generated extensible structure comes from in the registry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StructureInfo {
    /// Its `VkStructureType` value.
    pub stype: i32,
    /// `Vk...`.
    pub name: &'static str,
    /// The first core version that has it, in `VK_MAKE_API_VERSION(0, major,
    /// minor, 0)` packing; `None` for a structure only extensions add.
    pub core: Option<u32>,
    /// The protocol's extensions that add it, sorted.
    pub extensions: &'static [&'static str],
}

/// Every generated structure that has an `sType`, sorted by it.
pub const STRUCTURES: &[StructureInfo] = &[
% for value, name, stype, core, names in origins:
    StructureInfo { stype: ${value}, name: "${name}", core: ${'Some(%#x)' % core if core is not None else 'None'}, extensions: &[${', '.join('"%s"' % n for n in names)}] },
% endfor
];

/// The [`STRUCTURES`] entry for `stype`.
#[must_use]
pub fn structure(stype: i32) -> Option<&'static StructureInfo> {
    STRUCTURES
        .binary_search_by(|s| s.stype.cmp(&stype))
        .ok()
        .and_then(|i| STRUCTURES.get(i))
}
