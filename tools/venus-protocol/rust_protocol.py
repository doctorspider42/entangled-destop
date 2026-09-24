#!/usr/bin/env python3

# Copyright 2026 The Entangled Desktop authors
# SPDX-License-Identifier: MIT
#
# Rust output for the vendored venus-protocol generator. Not upstream: see
# VENDORED.md. Upstream's `vn_protocol.py` and `vkxml.py` are imported as
# libraries, unmodified; everything that decides what a byte means comes from
# their model (`Gen`), and this file only translates that model into Rust
# that calls `crates/virtio-gpu/src/venus/wire.rs`.
#
# The one rule this file is written to: **the C templates are the
# specification.** Every emitter below cites the `vn_protocol.py` function it
# mirrors (`_encode_variable`, `_decode_variable`, `_get_variable_validity`,
# ...). Where Rust needs a different *shape* (an `Option<Vec<T>>` for a
# pointer-and-count pair, an enum for a union), the bytes on the wire must
# still be exactly the C's; `tools/venus-protocol/harness` checks that against
# the C output.
#
# Every construct the protocol at the pinned revision uses is translated:
# unions (selector-tagged and default-tagged), strided arrays, two-level
# dynamic arrays, blobs a reply carries, arithmetic `len` expressions,
# constant-length pointers and packed `uint16_t` arrays. What is left raises
# `Unsupported` at generation time rather than emitting something plausible,
# so a newer upstream revision that needs something new is a generator
# change made on purpose, not a silent mistranslation.

import argparse
import re
import sys
import xml.etree.ElementTree as ET
from pathlib import Path

from mako.lookup import TemplateLookup
from mako.template import Template

import vn_protocol as vp
from vkxml import VkRegistry, VkType

HERE = Path(__file__).resolve().parent
TEMPLATE_DIR = HERE.joinpath('templates')
TEMPLATE_LOOKUP = TemplateLookup(str(TEMPLATE_DIR))

# The upstream revision these files were vendored from. See VENDORED.md.
VENUS_PROTOCOL_REVISION = '70991d4c'

VALID = vp.Gen.VariableInfo.VALID
INVALID = vp.Gen.VariableInfo.INVALID
PARTIAL = vp.Gen.VariableInfo.PARTIAL

RUST_KEYWORDS = {
    'as', 'break', 'const', 'continue', 'crate', 'else', 'enum', 'extern',
    'false', 'fn', 'for', 'if', 'impl', 'in', 'let', 'loop', 'match', 'mod',
    'move', 'mut', 'pub', 'ref', 'return', 'self', 'static', 'struct',
    'super', 'trait', 'true', 'type', 'unsafe', 'use', 'where', 'while',
    'async', 'await', 'dyn', 'abstract', 'become', 'box', 'do', 'final',
    'macro', 'override', 'priv', 'typeof', 'unsized', 'virtual', 'yield',
    'try', 'gen',
}

# Locals the generated functions use themselves; a field may not shadow them.
RESERVED_LOCALS = {'dec', 'enc', 'partial'}


class Unsupported(Exception):
    """A construct this generator deliberately does not translate yet."""


def snake(name):
    """camelCase Vulkan member names to snake_case Rust field names.

    An underscore goes before an upper-case letter that follows a lower-case
    one, and before the last capital of an acronym that is followed by a
    lower-case letter. Digits never take one: `maxImageDimension1D` becomes
    `max_image_dimension1d`, `deviceUUID` becomes `device_uuid`.
    """
    out = []
    for i, c in enumerate(name):
        if c.isupper():
            prev = name[i - 1] if i else ''
            nxt = name[i + 1] if i + 1 < len(name) else ''
            if prev and (prev.islower() or (prev.isupper() and nxt.islower())):
                out.append('_')
            out.append(c.lower())
        else:
            out.append(c)
    res = ''.join(out)
    if res in RUST_KEYWORDS or res in RESERVED_LOCALS:
        res += '_'
    return res


def camel(name):
    """A union member name as an enum variant: `float32` -> `Float32`."""
    return name[:1].upper() + name[1:]


# ---------------------------------------------------------------------------
# Selection
# ---------------------------------------------------------------------------

ALL = '*'


class Selection:
    """rust-selection.txt: `*` selects everything a section can hold, and in
    `[commands]` a `-name` line takes one command back out of it."""

    def __init__(self, api, extensions, commands, excluded):
        self.api = api
        self.extensions = extensions
        self.commands = commands
        self.excluded = excluded

    @staticmethod
    def parse(path):
        sections = {'api': [], 'extensions': [], 'commands': []}
        excluded = []
        current = None
        for raw in Path(path).read_text(encoding='utf-8').splitlines():
            line = raw.split('#', 1)[0].strip()
            if not line:
                continue
            if line.startswith('[') and line.endswith(']'):
                current = line[1:-1]
                if current not in sections:
                    raise ValueError('%s: unknown section [%s]' % (path, current))
                continue
            if current is None:
                raise ValueError('%s: entry %r before any section' % (path, line))
            if line.startswith('-'):
                if current != 'commands':
                    raise ValueError('%s: %r: only [commands] takes exclusions' % (path, line))
                if line[1:] in excluded:
                    raise ValueError('%s: %r excluded twice' % (path, line))
                excluded.append(line[1:])
                continue
            if line in sections[current]:
                raise ValueError('%s: %r listed twice' % (path, line))
            sections[current].append(line)
        for name, entries in sections.items():
            if ALL in entries and len(entries) != 1:
                raise ValueError('%s: [%s] holds `*` and more' % (path, name))
        api = sections['api']
        if len(api) != 1 or not (api[0] == ALL or re.fullmatch(r'1\.\d+', api[0])):
            raise ValueError('%s: [api] must hold exactly one version like 1.3, or *' % path)
        if excluded and sections['commands'] != [ALL]:
            raise ValueError('%s: a `-name` exclusion needs `*` in [commands]' % path)
        return Selection(api[0], sections['extensions'], sections['commands'], excluded)


# ---------------------------------------------------------------------------
# Wire shapes
# ---------------------------------------------------------------------------

class Elem:
    """What one element of a field is: a scalar, a handle, a struct or a
    union."""

    SCALAR = 'scalar'
    HANDLE = 'handle'
    STRUCT = 'struct'
    UNION = 'union'

    # Primitive C type -> (Rust type, Decoder/Encoder method).
    PRIMS = {
        'uint32_t': ('u32', 'u32'),
        'int32_t': ('i32', 'i32'),
        'float': ('f32', 'f32'),
        'uint64_t': ('u64', 'u64'),
        'int64_t': ('i64', 'i64'),
        'uint8_t': ('u8', 'u8'),
        'uint16_t': ('u16', 'u16'),
        'double': ('f64', 'f64'),
        'size_t': ('u64', 'size'),
        'char': ('u8', 'u8'),
        'void': ('u8', 'u8'),
    }

    def __init__(self, kind, ty, rust, method=None, prim=None):
        self.kind = kind
        self.ty = ty          # the VkType (base)
        self.rust = rust      # the Rust type it is held as
        self.method = method  # Decoder/Encoder method, scalars only
        self.prim = prim      # underlying C primitive, scalars only

    def is_byte(self):
        return self.kind == self.SCALAR and self.prim in ('char', 'uint8_t', 'void')

    def is_u16(self):
        return self.kind == self.SCALAR and self.prim == 'uint16_t'

    def is_compound(self):
        return self.kind in (self.STRUCT, self.UNION)


class Count:
    """How many elements one level of an array has, as the C computes it
    (`VariableInfo._init_loop_info`).

    PATH     a member (or `pointer->member`) named as the count;
    EXPR     C arithmetic over one member, `codeSize / 4`;
    CONST    known at generation time, `2*VK_UUID_SIZE` or the `1` of a
             pointer-to-one inner level;
    INDEXED  an inner level's count read from the outer array's element,
             `pInfos[i].geometryCount`.
    """

    PATH = 'path'
    EXPR = 'expr'
    CONST = 'const'
    INDEXED = 'indexed'

    def __init__(self, kind, text, path=None, value=None, ast=None, guarded=True):
        self.kind = kind
        self.text = text        # the registry's own spelling
        self.path = path        # [VkVariable] for PATH / INDEXED / EXPR (its one var)
        self.value = value      # CONST
        self.ast = ast          # EXPR
        self.guarded = guarded  # INDEXED: whether the C guards a null outer array


class Field:
    """One member of a struct or union, or one parameter of a command, in
    Rust terms."""

    PLAIN = 'plain'
    STATIC = 'static'
    PTR = 'ptr'
    DYN = 'dyn'
    NESTED = 'nested'
    STRING = 'string'
    STRING_ARRAY = 'string_array'
    BLOB = 'blob'
    BLOB_OUT = 'blob_out'
    NULL_ONLY = 'null_only'

    def __init__(self, owner, var):
        self.owner = owner
        self.var = var
        self.c_name = var.name
        self.name = snake(var.name)
        self.shape = None
        self.elem = None
        self.n = None           # STATIC length
        self.count = None       # Count of a dynamic array (outer level)
        self.inner = None       # NESTED: Count of the inner level
        self.condition = None   # IGNORABLE_LIST translation, encode side only
        self.selector = None    # a union member: the variable naming its tag
        self.stride_of = None   # a stride parameter: the C sizeof it must equal
        self.strided = None     # a strided array: the name of its stride parameter
        self.optional = var.is_optional()
        self.can_validate = var.can_validate()


class Model:
    def __init__(self, selection):
        self.selection = selection
        reg = VkRegistry.parse(vp.VN_PROTOCOL_VK_XML, vp.VN_PROTOCOL_PRIVATE_XMLS)
        self.gen = vp.Gen(False, reg)
        # Gen deep-copies the registry and fixes up *its* copy; every type
        # this model compares must come from that one.
        self.reg = self.gen.reg
        self.constants = self._parse_api_constants(vp.VN_PROTOCOL_VK_XML)
        self.fields = {}          # VkType -> [Field]
        self.chains = {}          # VkType -> [VkType]
        self.zero_width = {}      # VkType -> bool (partial encoding is empty)
        self.lifetime = {}        # VkType -> bool
        self.reach_chain = {}     # VkType -> bool (a pNext chain is reachable)
        self.aliases = []         # scalar alias VkTypes, registry order
        self.handles = []         # handle VkTypes, registry order
        self.enum_consts = {}     # name -> (rust type, value)
        self.constructs = {}      # construct -> set of command names reaching it

        self._init_chain_allowed()
        self._init_commands()
        self._init_closure()
        self._init_fields()
        self._check_partial_closure()
        self._init_zero_width()
        self._init_lifetimes()
        self._init_reach_chain()
        self._init_groups()
        self._init_constructs()

    # ---- registry helpers -------------------------------------------------

    @staticmethod
    def _parse_api_constants(vk_xml):
        consts = {}
        root = ET.parse(vk_xml).getroot()
        for enums in root.iter('enums'):
            if enums.attrib.get('type') != 'constants':
                continue
            for e in enums.iter('enum'):
                if 'value' in e.attrib:
                    consts[e.attrib['name']] = e.attrib['value']
                elif 'alias' in e.attrib:
                    consts[e.attrib['name']] = consts[e.attrib['alias']]
        return consts

    def static_len(self, var):
        dim = var.ty.static_array_size()
        if dim is None or not re.fullmatch(r'[A-Z0-9_]+', dim):
            raise Unsupported('%s: array dimension %r' % (var.name, dim))
        if dim.isdigit():
            return int(dim)
        value = self.constants.get(dim)
        if value is None or not value.isdigit():
            raise Unsupported('%s: array dimension %s = %r' % (var.name, dim, value))
        return int(value)

    def newest_api(self):
        return max((f.number for f in self.reg.features),
                   key=lambda n: tuple(int(x) for x in n.split('.')))

    def api(self):
        """The `[api]` version, `*` resolved to the newest vk.xml knows."""
        return self.newest_api() if self.selection.api == ALL else self.selection.api

    def chosen_extensions(self):
        if self.selection.extensions == [ALL]:
            return list(vp.VK_XML_EXTENSION_LIST)
        return list(self.selection.extensions)

    # ---- which chain structures are admitted -------------------------------

    def _init_chain_allowed(self):
        api = self.api()
        major, minor = (int(x) for x in api.split('.'))
        numbers = {feat.number for feat in self.reg.features}
        if api not in numbers:
            raise ValueError('[api] %s is not a Vulkan version in vk.xml' % api)

        exts = {e.name: e for e in self.reg.extensions}
        chosen = self.chosen_extensions()
        for name in chosen:
            if name not in vp.VK_XML_EXTENSION_LIST:
                raise ValueError('extension %s is not in VK_XML_EXTENSION_LIST' % name)
            if name not in exts:
                raise ValueError('extension %s is not in the registry' % name)

        # Everything, which is the default: the chain whitelist is exactly
        # the protocol's (`Gen.get_chain`, the `switch` of every
        # `vn_decode_*_pnext_temp`), with no filter of our own on top.
        if api == self.newest_api() and set(chosen) == set(vp.VK_XML_EXTENSION_LIST):
            self.chain_allowed = None
            return

        allowed = set()
        for feat in self.reg.features:
            fmaj, fmin = (int(x) for x in feat.number.split('.'))
            if (fmaj, fmin) <= (major, minor):
                allowed.update(feat.types)
        chosen_set = set(chosen)
        for name in chosen:
            ext = exts[name]
            allowed.update(ext.types)
            for deps, types in ext.optional_types.items():
                if self._deps_met(deps, chosen_set):
                    allowed.update(types)
        self.chain_allowed = allowed

    @staticmethod
    def _deps_met(deps, chosen):
        # Same grammar as Gen.support_type_depends: ',' is OR, '+' is AND.
        for or_dep in deps.split(','):
            if all(d in chosen for d in or_dep.split('+')):
                return True
        return False

    # ---- commands and the structures they reach -----------------------------

    def _init_commands(self):
        supported = self.gen.supported_types[VkType.COMMAND]
        by_name = {c.name: c for c in supported}
        sel = self.selection
        if sel.commands == [ALL]:
            for name in sel.excluded:
                if name not in by_name:
                    raise ValueError('excluded command %s is not in the venus protocol' % name)
            names = [c.name for c in supported
                     if self.gen.is_serializable(c) and c.name not in sel.excluded]
        else:
            names = sel.commands
        self.commands = []
        for name in names:
            cmd = by_name.get(name)
            if cmd is None:
                raise ValueError('command %s is not in the venus protocol' % name)
            if not self.gen.is_serializable(cmd):
                raise ValueError('command %s is not serializable' % name)
            self.commands.append(cmd)
        # registry order, so the output does not depend on the list's order
        order = {c: i for i, c in enumerate(supported)}
        self.commands.sort(key=lambda c: order[c])

    def chain_of(self, ty):
        types, _skipped = self.gen.get_chain(ty)
        if self.chain_allowed is None:
            return types
        return [t for t in types if t in self.chain_allowed]

    def _init_closure(self):
        seen = []
        visiting = []

        def visit(ty):
            if ty.category not in (VkType.STRUCT, VkType.UNION):
                return
            if ty in seen:
                return
            if ty in visiting:
                raise Unsupported('%s contains itself' % ty.name)
            if ty.category == VkType.UNION and not self.gen.is_serializable(ty):
                raise Unsupported('union %s is not serializable' % ty.name)
            visiting.append(ty)
            for var in ty.variables:
                if var.is_p_next():
                    continue
                if not self.gen.is_serializable(var):
                    continue
                visit(var.ty.base)
            visiting.pop()
            seen.append(ty)
            if ty.category == VkType.STRUCT:
                for nxt in self.chain_of(ty):
                    visit(nxt)

        for cmd in self.commands:
            for var in cmd.variables:
                if not self.gen.is_serializable(var):
                    continue
                visit(var.ty.base)
        # Dependency order (members before the structs holding them) is what
        # the DFS produced; keep the registry order among independent types
        # so an unrelated addition does not reshuffle the file.
        self.structs = seen

    # ---- fields --------------------------------------------------------------

    def elem_of(self, base):
        cat = base.category
        if cat == VkType.HANDLE:
            if base not in self.handles:
                self.handles.append(base)
            return Elem(Elem.HANDLE, base, base.name)
        if cat == VkType.STRUCT:
            return Elem(Elem.STRUCT, base, base.name)
        if cat == VkType.UNION:
            return Elem(Elem.UNION, base, base.name)
        prim = self._primitive_of(base)
        rust, method = Elem.PRIMS[prim]
        if cat == VkType.DEFAULT:
            return Elem(Elem.SCALAR, base, rust, method, prim)
        if base not in self.aliases:
            self.aliases.append(base)
        return Elem(Elem.SCALAR, base, base.name, method, prim)

    def _primitive_of(self, base):
        cat = base.category
        if cat == VkType.DEFAULT:
            if base.name not in Elem.PRIMS:
                raise Unsupported('scalar %s' % base.name)
            return base.name
        if cat == VkType.BASETYPE:
            if not base.typedef:
                raise Unsupported('opaque basetype %s' % base.name)
            return self._primitive_of(base.typedef)
        if cat == VkType.ENUM:
            return 'int32_t' if base.enums.bitwidth == 32 else 'uint64_t'
        if cat == VkType.BITMASK:
            return self._primitive_of(base.typedef)
        raise Unsupported('type %s (category %d)' % (base.name, cat))

    def c_layout(self, ty):
        """(sizeof, alignof) of a C type, for the strided arrays whose stride
        the driver rewrites to `sizeof(element)`. Only what a stride element
        can be: scalars, and structures of them."""
        cat = ty.category
        if cat in (VkType.DEFAULT, VkType.BASETYPE, VkType.ENUM, VkType.BITMASK):
            prim = self._primitive_of(ty)
            size = {'uint32_t': 4, 'int32_t': 4, 'float': 4, 'uint64_t': 8,
                    'int64_t': 8, 'double': 8, 'uint16_t': 2, 'uint8_t': 1}.get(prim)
            if size is None:
                raise Unsupported('sizeof %s' % ty.name)
            return size, size
        if cat != VkType.STRUCT or ty.s_type:
            raise Unsupported('sizeof %s' % ty.name)
        size, align = 0, 1
        for var in ty.variables:
            if var.ty.is_pointer():
                raise Unsupported('sizeof %s: pointer member' % ty.name)
            s, a = self.c_layout(var.ty.base)
            if var.ty.is_static_array():
                s *= self.static_len(var)
            size = (size + a - 1) // a * a + s
            align = max(align, a)
        return (size + align - 1) // align * align, align

    def _field(self, owner, var):
        f = Field(owner, var)
        ty = var.ty
        base = ty.base

        if not self.gen.is_serializable(var):
            # `_decode_variable` over an unserializable pointer: the marker,
            # fatal when set, and when null fatal again unless optional or
            # noautovalidity. A union's host address is the one non-optional.
            if var.ty.is_pointer():
                f.shape = Field.NULL_ONLY
                return f
            raise Unsupported('%s.%s is not serializable' % (owner.name, var.name))

        for ign in vp.Gen.IGNORABLE_LIST:
            if ign.struct == owner.name and ign.var == var.name:
                f.condition = self._translate_condition(ign.condition)

        if 'selector' in var.attrs:
            if ty.is_pointer() or ty.is_static_array() or base.category != VkType.UNION \
                    or not base.is_valid_union():
                raise Unsupported('%s.%s: selector on a non-union' % (owner.name, var.name))
            sel = [v for v in owner.variables if v.name == var.attrs['selector']]
            if len(sel) != 1 or sel[0].ty.is_pointer():
                raise Unsupported('%s.%s: selector %s' % (owner.name, var.name, var.attrs['selector']))
            f.selector = sel[0]
            self.elem_of(base.sty)

        if ty.is_static_array():
            if ty.is_pointer():
                raise Unsupported('%s.%s: array of pointers' % (owner.name, var.name))
            if 'len_exprs' in var.attrs and 'wa_require_static_len' not in var.attrs:
                raise Unsupported('%s.%s: static array with a len' % (owner.name, var.name))
            f.shape = Field.STATIC
            f.n = self.static_len(var)
            f.elem = self.elem_of(base)
            if f.elem.kind == Elem.UNION:
                raise Unsupported('%s.%s: array of unions' % (owner.name, var.name))
            return f

        if not ty.is_pointer():
            f.shape = Field.PLAIN
            f.elem = self.elem_of(base)
            return f

        depth = ty.indirection_depth()
        if 'len_exprs' not in var.attrs:
            if depth != 1:
                raise Unsupported('%s.%s: pointer to pointer' % (owner.name, var.name))
            if base.name == 'void':
                raise Unsupported('%s.%s: void pointer without len' % (owner.name, var.name))
            f.shape = Field.PTR
            f.elem = self.elem_of(base)
            if f.elem.kind == Elem.UNION and base.is_valid_union():
                raise Unsupported('%s.%s: pointer to a selected union' % (owner.name, var.name))
            return f

        exprs = var.attrs['len_exprs']
        names = var.attrs['len_names']
        if var.has_c_string():
            if depth == 1 and exprs == ['null-terminated']:
                f.shape = Field.STRING
                f.elem = self.elem_of(base)
                return f
            if depth == 2 and len(exprs) == 2 and exprs[1] == 'null-terminated':
                f.shape = Field.STRING_ARRAY
                f.elem = self.elem_of(base)
                f.count = self._count(owner, var, exprs[0], names[0])
                return f
            raise Unsupported('%s.%s: string shape %r' % (owner.name, var.name, exprs))

        if depth == 2 and len(exprs) == 2:
            f.shape = Field.NESTED
            f.elem = self.elem_of(base)
            f.count = self._count(owner, var, exprs[0], names[0])
            f.inner = self._inner_count(owner, var, exprs[1], names[1])
            if f.elem.kind == Elem.UNION or f.elem.is_byte() or f.elem.is_u16():
                raise Unsupported('%s.%s: nested array of %s' % (owner.name, var.name, base.name))
            return f
        if depth != 1 or len(exprs) != 1:
            raise Unsupported('%s.%s: nested dynamic array' % (owner.name, var.name))
        f.count = self._count(owner, var, exprs[0], names[0])
        if 'stride' in var.attrs:
            f.strided = var.attrs['stride']
        # A blob the host writes — an output parameter, or a member a
        # skeleton leaves unwritten — is held owned; an input one is
        # borrowed from the command bytes.
        written = 'var_out' in var.attrs or (
            owner.category == VkType.STRUCT and 'need_partial' in owner.attrs
            and self.gen._get_variable_validity(owner, var, False) == INVALID)
        if var.is_blob():
            f.shape = Field.BLOB_OUT if written else Field.BLOB
            return f
        f.elem = self.elem_of(base)
        if f.elem.is_byte():
            f.shape = Field.BLOB_OUT if written else Field.BLOB
            f.elem = None
        else:
            f.shape = Field.DYN
            if f.elem.kind == Elem.UNION and base.is_valid_union():
                raise Unsupported('%s.%s: array of selected unions' % (owner.name, var.name))
        return f

    # ---- counts: mirrors VariableInfo._init_loop_info ------------------------

    def _count(self, owner, var, expr, name):
        if not name:
            return Count(Count.CONST, expr, value=self._const_value(owner, var, expr))
        path = owner.find_variables(name)
        if not path or len(path) > 2:
            raise Unsupported('%s.%s: len %r' % (owner.name, var.name, name))
        if expr == name:
            return Count(Count.PATH, expr, path=path)
        if len(path) != 1 or path[0].ty.is_pointer():
            raise Unsupported('%s.%s: len expression over %r' % (owner.name, var.name, name))
        ast = self._parse_expr(owner, var, expr, name)
        self.c_unsigned_width(path[0])
        return Count(Count.EXPR, expr, path=path, ast=ast)

    def _inner_count(self, owner, var, expr, name):
        if not name:
            return Count(Count.CONST, expr, value=self._const_value(owner, var, expr))
        if '[i].' in name:
            path = owner.find_variables(name)
            if len(path) != 2 or expr != name or path[0].ty.indirection_depth() != 1:
                raise Unsupported('%s.%s: inner len %r' % (owner.name, var.name, name))
            # `VariableInfo._init_loop_info` leaves ppBuildRangeInfos's inner
            # count unguarded; the outer array is required there, so the
            # difference never shows on a stream that decodes.
            return Count(Count.INDEXED, expr, path=path, guarded=var.name != 'ppBuildRangeInfos')
        raise Unsupported('%s.%s: inner len %r' % (owner.name, var.name, expr))

    def _const_value(self, owner, var, expr):
        ast = self._parse_expr(owner, var, expr, None)
        return self._eval_const(ast)

    TOKEN = re.compile(r'\s*(?:(\d+)|([A-Za-z_]\w*)|(.))')

    def _parse_expr(self, owner, var, text, name):
        """A registry `len` expression: integers, API constants, the one
        member `name`, `+ - * /` and parentheses. Anything else is refused."""
        toks = []
        for m in self.TOKEN.finditer(text):
            if m.group(1):
                toks.append(('int', int(m.group(1))))
            elif m.group(2):
                ident = m.group(2)
                if ident == name:
                    toks.append(('var', ident))
                elif ident in self.constants and self.constants[ident].isdigit():
                    toks.append(('int', int(self.constants[ident])))
                else:
                    raise Unsupported('%s.%s: len %r names %s' % (owner.name, var.name, text, ident))
            elif m.group(3) and m.group(3).strip():
                if m.group(3) not in '+-*/()':
                    raise Unsupported('%s.%s: len %r' % (owner.name, var.name, text))
                toks.append(('op', m.group(3)))
        pos = [0]

        def peek():
            return toks[pos[0]] if pos[0] < len(toks) else ('end', None)

        def take():
            t = peek()
            pos[0] += 1
            return t

        def primary():
            t = take()
            if t[0] in ('int', 'var'):
                return t
            if t == ('op', '('):
                e = expr()
                if take() != ('op', ')'):
                    raise Unsupported('%s.%s: len %r' % (owner.name, var.name, text))
                return e
            raise Unsupported('%s.%s: len %r' % (owner.name, var.name, text))

        def term():
            e = primary()
            while peek() in (('op', '*'), ('op', '/')):
                op = take()[1]
                rhs = primary()
                if op == '/' and (rhs[0] != 'int' or rhs[1] == 0):
                    raise Unsupported('%s.%s: len %r divides by a non-constant' % (
                        owner.name, var.name, text))
                e = ('bin', op, e, rhs)
            return e

        def expr():
            e = term()
            while peek() in (('op', '+'), ('op', '-')):
                op = take()[1]
                e = ('bin', op, e, term())
            return e

        ast = expr()
        if peek()[0] != 'end':
            raise Unsupported('%s.%s: len %r' % (owner.name, var.name, text))
        return ast

    @staticmethod
    def _eval_const(ast):
        if ast[0] == 'int':
            return ast[1]
        if ast[0] == 'var':
            raise Unsupported('a constant len names a member')
        _, op, a, b = ast
        a, b = Model._eval_const(a), Model._eval_const(b)
        return {'+': a + b, '-': a - b, '*': a * b, '/': a // b}[op]

    def c_unsigned_width(self, var):
        """The C type an arithmetic `len` computes in once `var` is read: the
        usual arithmetic conversions over an unsigned operand. An enum with
        no negative value is `unsigned int` under GCC and Clang, which is
        what virglrenderer is built with (the harness asserts it)."""
        base = var.ty.base
        if base.category == VkType.ENUM:
            if base.enums.bitwidth != 32 or any(int(v, 0) < 0 for v in base.enums.values.values()):
                raise Unsupported('len over enum %s' % base.name)
            return 32
        prim = self._primitive_of(base)
        if prim in ('uint64_t', 'size_t'):
            return 64
        if prim == 'uint32_t':
            return 32
        raise Unsupported('len arithmetic over %s' % base.name)

    def _translate_condition(self, cond):
        m = re.fullmatch(r'val->(\w+) == (VK_\w+)', cond)
        if m:
            self._need_const(m.group(2))
            return 'self.%s == %s' % (snake(m.group(1)), m.group(2))
        m = re.fullmatch(r'!\(val->(\w+) & (VK_\w+)\)', cond)
        if m:
            self._need_const(m.group(2))
            return 'self.%s & %s == 0' % (snake(m.group(1)), m.group(2))
        raise Unsupported('IGNORABLE condition %r' % cond)

    def _need_const(self, key):
        for ty in self.reg.type_table.values():
            if ty.category in (VkType.ENUM,) and ty.enums and key in ty.enums.values:
                value = int(ty.enums.values[key], 0)
                if ty.name.endswith('FlagBits'):
                    flags = ty.name.replace('FlagBits', 'Flags')
                    rust = flags if flags in self.reg.type_table else 'u32'
                else:
                    rust = ty.name
                    self.elem_of(ty)
                self.enum_consts[key] = (rust, value)
                return value
        raise Unsupported('constant %s' % key)

    def _init_fields(self):
        for ty in self.structs:
            fields = []
            for var in ty.variables:
                if ty.s_type and var.name in ('sType', 'pNext'):
                    continue
                if var.is_p_next():
                    raise Unsupported('%s: pNext without sType' % ty.name)
                fields.append(self._field(ty, var))
            self.fields[ty] = fields
            if ty.category == VkType.STRUCT:
                self.chains[ty] = [t for t in self.chain_of(ty) if t in self.structs]
            else:
                self.chains[ty] = []
                self._init_union(ty)
            self._init_strides(ty)
            self._check_names(ty, fields)
        for cmd in self.commands:
            fields = [self._field(cmd, var) for var in cmd.variables]
            self.fields[cmd] = fields
            self._init_strides(cmd)
            self._check_names(cmd, fields)
            if cmd.ret:
                self.elem_of(cmd.ret.ty.base)
        # VkResult names every reply's return value and defines.rs lists its
        # values whether or not a selected command returns one.
        self.elem_of(self.reg.type_table['VkResult'])

    def _init_union(self, ty):
        """A union's cases, as `get_union_cases` lists them: the selector
        values each member answers to, or its index for a union the
        protocol always sends with its default tag (`UNION_DEFAULT_TAGS`)."""
        cases = {}
        order = []
        for tag, var in ty.get_union_cases():
            value = self._need_const(tag) if ty.is_valid_union() else tag
            f = next(f for f in self.fields[ty] if f.var is var)
            if f.shape not in (Field.PLAIN, Field.STATIC, Field.PTR, Field.NULL_ONLY):
                raise Unsupported('%s.%s: union member shape %s' % (ty.name, var.name, f.shape))
            if f.shape in (Field.PLAIN, Field.PTR) and f.elem.kind == Elem.UNION                     and f.elem.ty.is_valid_union():
                raise Unsupported('%s.%s: selected union in a union' % (ty.name, var.name))
            if var.name not in cases:
                cases[var.name] = []
                order.append(f)
            cases[var.name].append((tag, value))
        ty.attrs['rust_cases'] = [(f, cases[f.var.name]) for f in order]
        if ty.is_valid_union():
            self.elem_of(ty.sty)
            ty.attrs['rust_default'] = order[0]
        else:
            default = vp.Gen.UNION_DEFAULT_TAGS[ty.name]
            ty.attrs['rust_default'] = next(f for f, tags in ty.attrs['rust_cases']
                                            if tags[0][1] == default)
            ty.attrs['rust_default_tag'] = default

    def _init_strides(self, owner):
        """A strided array's stride parameter goes out as
        `sizeof(element)`: the driver packs the elements and rewrites it
        (`stride = sizeof(...)` in `_encode_variable`)."""
        for f in self.fields[owner]:
            if not f.strided:
                continue
            if f.shape != Field.DYN or f.elem.kind != Elem.STRUCT:
                raise Unsupported('%s.%s: strided %s' % (owner.name, f.c_name, f.shape))
            stride = [g for g in self.fields[owner] if g.c_name == f.strided]
            if len(stride) != 1 or stride[0].shape != Field.PLAIN \
                    or stride[0].elem.kind != Elem.SCALAR or stride[0].elem.prim != 'uint32_t':
                raise Unsupported('%s.%s: stride %s' % (owner.name, f.c_name, f.strided))
            if 'var_out' in f.var.attrs or 'var_out' in stride[0].var.attrs:
                raise Unsupported('%s.%s: output stride' % (owner.name, f.c_name))
            stride[0].stride_of = self.c_layout(f.elem.ty)[0]

    @staticmethod
    def _check_names(ty, fields):
        names = [f.name for f in fields]
        if len(set(names)) != len(names):
            raise Unsupported('%s: field names collide after snake_case' % ty.name)
        if 'p_next' in names or 'ret' in names:
            raise Unsupported('%s: field shadows p_next/ret' % ty.name)
        for f in fields:
            if f.selector is not None and '%s_tag' % f.name in names:
                raise Unsupported('%s: %s_tag collides' % (ty.name, f.name))

    # ---- validity: mirrors vn_protocol.Gen._get_variable_validity -----------

    def validity(self, ty, field, initialized):
        return self.gen._get_variable_validity(ty, field.var, initialized)

    def command_validity(self, cmd, field):
        return self.gen._get_variable_validity(cmd, field.var, 'var_in' in field.var.attrs)

    @staticmethod
    def has_partial(ty):
        """Whether `ty` has a skeleton form at all: vn_protocol sets
        `need_partial` on every type an output parameter reaches, members and
        chain links included, and only those get `*_partial` codecs."""
        return ty.category == VkType.STRUCT and 'need_partial' in ty.attrs

    def _check_partial_closure(self):
        for ty in self.structs:
            if not self.has_partial(ty):
                continue
            for t in self.chains[ty]:
                if not self.has_partial(t):
                    raise AssertionError('%s: chain link %s has no partial form' % (ty.name, t.name))
            for f in self.fields[ty]:
                if (f.elem is not None and f.elem.kind == Elem.STRUCT
                        and self.validity(ty, f, False) == PARTIAL
                        and not self.has_partial(f.elem.ty)):
                    raise AssertionError('%s.%s has no partial form' % (ty.name, f.c_name))

    @staticmethod
    def is_out(field):
        return 'var_out' in field.var.attrs

    # ---- derived properties ---------------------------------------------------

    def _init_zero_width(self):
        # A struct whose *partial* encoding writes no bytes at all. Arrays of
        # those are sized by their count field alone, so their decoder must
        # not charge the stream for elements that are not there.
        for ty in self.structs:
            if ty.s_type or not self.has_partial(ty):
                self.zero_width[ty] = False
                continue
            empty = True
            for f in self.fields[ty]:
                v = self.validity(ty, f, False)
                if f.shape in (Field.PLAIN, Field.STATIC) and v == INVALID:
                    continue
                if (f.shape == Field.PLAIN and v == PARTIAL and f.elem.kind == Elem.STRUCT
                        and self.zero_width.get(f.elem.ty, False)):
                    continue
                empty = False
                break
            self.zero_width[ty] = empty

    def _init_lifetimes(self):
        for ty in self.structs:
            self.lifetime[ty] = False
        changed = True
        while changed:
            changed = False
            for ty in self.structs:
                if self.lifetime[ty]:
                    continue
                if self._needs_lifetime(ty):
                    self.lifetime[ty] = True
                    changed = True
        for cmd in self.commands:
            self.lifetime[cmd] = self._needs_lifetime(cmd)

    def _needs_lifetime(self, ty):
        for f in self.fields[ty]:
            if f.shape in (Field.STRING, Field.STRING_ARRAY, Field.BLOB):
                return True
            if f.elem is not None and f.elem.is_compound() and self.lifetime[f.elem.ty]:
                return True
        return any(self.lifetime[t] for t in self.chains.get(ty, []))

    def chain_lifetime(self, ty):
        return any(self.lifetime[t] for t in self.chains[ty])

    def _init_reach_chain(self):
        """Which types carry a pNext link somewhere inside them, for the
        link walk the executor judges chains with."""
        for ty in self.structs:
            self.reach_chain[ty] = bool(self.chains[ty])
        changed = True
        while changed:
            changed = False
            for ty in self.structs:
                if self.reach_chain[ty]:
                    continue
                if any(self.walks(f) for f in self.fields[ty]) or \
                        any(self.reach_chain[t] for t in self.chains[ty]):
                    self.reach_chain[ty] = True
                    changed = True
        for cmd in self.commands:
            self.reach_chain[cmd] = any(self.walks(f) for f in self.fields[cmd])

    def walks(self, f):
        """Whether field `f` can hold a pNext link."""
        return f.elem is not None and f.elem.is_compound() and self.reach_chain.get(f.elem.ty, False)

    # ---- groups: mirrors GenStructsAndCommands --------------------------------

    def _init_groups(self):
        rules = vp.GenStructsAndCommands.RULES
        groups = [(name, rule) for name, rule in rules.items()]
        groups.reverse()

        def group_of(cmd):
            for name, rule in groups:
                if not rule or any(cmd.name[2:].startswith(r) for r in rule):
                    return name
            raise AssertionError(cmd.name)

        reach = {}
        for cmd in self.commands:
            g = group_of(cmd)
            s = reach.setdefault(g, set())
            self._reach(cmd, s)
        owner = {}
        for ty in self.structs:
            gs = [g for g, s in reach.items() if ty in s]
            owner[ty] = gs[0] if len(gs) == 1 else 'structs'

        self.groups = {}
        for name in rules:
            self.groups[name] = {'structs': [], 'commands': []}
        for ty in self.structs:
            self.groups[owner[ty]]['structs'].append(ty)
        for cmd in self.commands:
            self.groups[group_of(cmd)]['commands'].append(cmd)
        self.groups = {k: v for k, v in self.groups.items() if v['structs'] or v['commands']}
        if 'structs' not in self.groups:
            self.groups['structs'] = {'structs': [], 'commands': []}

    def _reach(self, ty, acc):
        for f in self.fields[ty]:
            if f.elem is not None and f.elem.is_compound() and f.elem.ty not in acc:
                acc.add(f.elem.ty)
                self._reach(f.elem.ty, acc)
        for t in self.chains.get(ty, []):
            if t not in acc:
                acc.add(t)
                self._reach(t, acc)

    # ---- construct coverage, for the harness report ----------------------------

    def field_constructs(self, f):
        out = set()
        if f.elem is not None and f.elem.kind == Elem.UNION:
            out.add('union (selector)' if f.selector is not None else 'union (default tag)')
        if f.strided:
            out.add('strided array')
        if f.shape == Field.NESTED:
            out.add('nested dynamic array')
        if f.shape == Field.BLOB_OUT:
            out.add('blob in the reply')
        if f.count is not None and f.count.kind == Count.EXPR:
            out.add('arithmetic len')
        if f.count is not None and f.count.kind == Count.CONST:
            out.add('constant len')
        if f.elem is not None and f.elem.is_u16() and f.shape in (Field.DYN, Field.STATIC):
            out.add('packed uint16_t array')
        if f.shape == Field.NULL_ONLY and not f.var.maybe_null():
            out.add('unserializable pointer')
        return out

    def _init_constructs(self):
        memo = {}

        def of(ty, stack):
            if ty in memo:
                return memo[ty]
            acc = set()
            stack = stack | {ty}
            for f in self.fields[ty]:
                acc |= self.field_constructs(f)
                if f.elem is not None and f.elem.is_compound() and f.elem.ty not in stack:
                    acc |= of(f.elem.ty, stack)
            for t in self.chains.get(ty, []):
                if t not in stack:
                    acc |= of(t, stack)
            memo[ty] = acc
            return acc

        for cmd in self.commands:
            for c in of(cmd, frozenset()):
                self.constructs.setdefault(c, set()).add(cmd.name)

    # ---- enum and version tables --------------------------------------------

    def structure_types(self):
        values = self.reg.type_table['VkStructureType'].enums.values
        out = []
        for ty in self.structs:
            if ty.s_type:
                out.append((ty.s_type, int(values[ty.s_type], 0)))
        return out

    def structure_origins(self):
        """Where every generated extensible structure comes from in the
        registry: the first core version that has it, and the protocol's
        extensions that add it. Sorted by sType."""
        values = self.reg.type_table['VkStructureType'].enums.values
        exts = [e for e in self.reg.extensions if e.name in vp.VK_XML_EXTENSION_LIST]
        out = []
        for ty in self.structs:
            if not ty.s_type:
                continue
            core = None
            for feat in self.reg.features:
                if ty in feat.types:
                    ver = tuple(int(x) for x in feat.number.split('.'))
                    if core is None or ver < core:
                        core = ver
            names = set()
            for ext in exts:
                if ty in ext.types:
                    names.add(ext.name)
                for deps, types in ext.optional_types.items():
                    if ty in types and vp.Gen.support_type_depends(deps):
                        names.add(ext.name)
            packed = (core[0] << 22) | (core[1] << 12) if core else None
            out.append((int(values[ty.s_type], 0), ty.name, ty.s_type, packed, sorted(names)))
        out.sort(key=lambda t: t[0])
        return out

    def command_types(self):
        """Every VkCommandTypeEXT value, as (rust const name, value, primary cmd name)."""
        values = self.reg.type_table['VkCommandTypeEXT'].enums.values
        out = []
        seen = set()
        for key, val in values.items():
            m = re.fullmatch(r'VK_COMMAND_TYPE_(vk\w+)_EXT', key)
            if not m:
                raise Unsupported('VkCommandTypeEXT entry %s' % key)
            cmd_name = m.group(1)
            const = 'VK_COMMAND_TYPE_%s_EXT' % self.reg.upper_name(cmd_name[2:])
            if const in seen:
                raise Unsupported('opcode constant %s collides' % const)
            seen.add(const)
            ty = self.reg.type_table.get(cmd_name)
            primary = ty is not None and ty.name == cmd_name
            out.append((const, int(val, 0), cmd_name, primary))
        return out

    def vk_results(self):
        values = self.reg.type_table['VkResult'].enums.values
        return [(k, int(v, 0)) for k, v in values.items()]

    def vk_xml_version(self):
        m = re.fullmatch(r'VK_MAKE_API_VERSION\((\d+), (\d+), (\d+), (\d+)\)',
                         self.reg.vk_xml_version)
        if not m:
            raise Unsupported('vk_xml_version %r' % self.reg.vk_xml_version)
        return tuple(int(x) for x in m.groups())

    def extensions(self):
        exts = [e for e in self.reg.extensions if e.name in vp.VK_XML_EXTENSION_LIST]
        exts.sort(key=lambda e: e.name)
        chosen = set(self.chosen_extensions())
        return [(e.name, e.number, e.version, e.name in chosen) for e in exts]

    @staticmethod
    def mask(numbers, words, sentinel):
        mask = [0] * words
        for n in numbers:
            if n // 32 >= words:
                raise Unsupported('extension number %d does not fit the mask' % n)
            mask[n // 32] |= 1 << (n % 32)
        if sentinel:
            assert not mask[0] & 1
            mask[0] |= 1
        return mask


# ---------------------------------------------------------------------------
# Rust emission
# ---------------------------------------------------------------------------

class Rust:
    """Code fragments. Every method returns Rust source text.

    Decode fragments are *expressions* evaluating to the field's value;
    encode fragments are *statements*. `§` marks the nested-struct partial
    flag, substituted by the caller (see `body_decode`).
    """

    def __init__(self, model):
        self.m = model

    # ---- types -----------------------------------------------------------

    def lt(self, ty):
        return "<'a>" if self.m.lifetime.get(ty) else ''

    def elem_type(self, elem):
        if elem.is_compound():
            return elem.rust + self.lt(elem.ty)
        return elem.rust

    def field_type(self, f):
        if f.shape == Field.PLAIN:
            return self.elem_type(f.elem)
        if f.shape == Field.STATIC:
            if f.elem.is_byte():
                return '[u8; %d]' % f.n
            return '[%s; %d]' % (self.elem_type(f.elem), f.n)
        if f.shape == Field.PTR:
            return 'Option<%s>' % self.elem_type(f.elem)
        if f.shape == Field.DYN:
            return 'Option<Vec<%s>>' % self.elem_type(f.elem)
        if f.shape == Field.NESTED:
            return 'Option<Vec<Vec<%s>>>' % self.elem_type(f.elem)
        if f.shape in (Field.STRING, Field.BLOB):
            return "Option<&'a [u8]>"
        if f.shape == Field.BLOB_OUT:
            return 'Option<Vec<u8>>'
        if f.shape == Field.STRING_ARRAY:
            return "Option<Vec<&'a [u8]>>"
        raise AssertionError(f.shape)

    def default_expr(self, f):
        if f.shape == Field.STATIC:
            if f.elem.is_byte():
                return '[0; %d]' % f.n
            return 'std::array::from_fn(|_| Default::default())'
        return 'Default::default()'

    def derives_default(self, ty):
        """Whether `#[derive(Default)]` works: std implements `Default` for
        arrays of up to 32 elements only, and `char[256]` is common."""
        return all(f.shape != Field.STATIC or f.n <= 32 for f in self.m.fields[ty])

    def field_doc(self, f):
        return '`%s`' % f.var.to_c()

    # ---- counts ----------------------------------------------------------

    def count_expr(self, count, access, index=None):
        """An array level's length, as a `u64` expression.

        Mirrors `VariableInfo._init_loop_info`: a pointer count reads as
        `(p ? *p : 0)`, a count inside a pointed-to struct as
        `(p ? p->count : 0)`, an inner count as `(outer ? outer[i].c : 0)`,
        and arithmetic as C evaluates it in its unsigned type.
        """
        if count.kind == Count.CONST:
            return '%du64' % count.value
        if count.kind == Count.EXPR:
            var = count.path[0]
            width = self.m.c_unsigned_width(var)
            ty = 'u%d' % width
            operand = access(snake(var.name))
            prim = self.m._primitive_of(var.ty.base)
            if prim == 'int32_t':
                operand = '(%s as u32)' % operand
            text = self._expr(count.ast, operand, ty)
            return text if width == 64 else 'u64::from(%s)' % text
        path = count.path
        head = path[0]
        head_name = access(snake(head.name))
        if count.kind == Count.INDEXED:
            member = path[1]
            inner = self._to_u64(member.ty.base, 'e.%s' % snake(member.name))
            return '%s.as_deref().and_then(|v| v.get(%s)).map_or(0, |e| %s)' % (
                head_name, index, inner)
        if len(path) == 1:
            base = head.ty.base
            if head.ty.is_pointer():
                return self._to_u64(base, '%s.unwrap_or(0)' % head_name)
            return self._to_u64(base, head_name)
        member = path[1]
        inner = self._to_u64(member.ty.base, 'v.%s' % snake(member.name))
        return '%s.as_ref().map_or(0, |v| %s)' % (head_name, inner)

    def _expr(self, ast, operand, ty):
        """C arithmetic in Rust: wrapping like C's unsigned types, and a
        division (always by a non-zero constant) parenthesised only where it
        is a method's receiver."""
        text, _div = self._expr_parts(ast, operand, ty)
        return text

    def _expr_parts(self, ast, operand, ty):
        if ast[0] == 'int':
            return '%d%s' % (ast[1], ty), False
        if ast[0] == 'var':
            return operand, False
        _, op, a, b = ast
        a, a_div = self._expr_parts(a, operand, ty)
        if op == '/':
            return '%s / %d%s' % (a, b[1], ty), True
        b, _b_div = self._expr_parts(b, operand, ty)
        method = {'+': 'wrapping_add', '-': 'wrapping_sub', '*': 'wrapping_mul'}[op]
        return '%s.%s(%s)' % ('(%s)' % a if a_div else a, method, b), False

    def _to_u64(self, base, expr):
        prim = self.m._primitive_of(base)
        if prim in ('uint64_t', 'size_t'):
            return expr
        if prim in ('uint32_t', 'uint16_t', 'uint8_t'):
            return 'u64::from(%s)' % expr
        raise Unsupported('array count of type %s' % base.name)

    # ---- element decode/encode --------------------------------------------

    def elem_decode(self, elem):
        """Expression decoding one element; `?` converts into ProtocolError."""
        if elem.kind == Elem.SCALAR:
            if elem.prim == 'double':
                return 'f64::from_bits(dec.u64()?)'
            return 'dec.%s()?' % elem.method
        if elem.kind == Elem.HANDLE:
            return '%s(dec.handle()?)' % elem.rust
        if elem.kind == Elem.UNION:
            return '%s::decode(dec)?' % elem.rust
        return '%s::decode_with(dec, §)?' % elem.rust

    def elem_decode_closure(self, elem):
        """A closure `|dec| -> Result<T, ProtocolError>` decoding one element."""
        if elem.kind == Elem.SCALAR:
            if elem.prim == 'double':
                return '|dec| dec.u64().map(f64::from_bits).map_err(ProtocolError::from)'
            return '|dec| dec.%s().map_err(ProtocolError::from)' % elem.method
        if elem.kind == Elem.HANDLE:
            return '|dec| dec.handle().map(%s).map_err(ProtocolError::from)' % elem.rust
        if elem.kind == Elem.UNION:
            return '%s::decode' % elem.rust
        return '|dec| %s::decode_with(dec, §)' % elem.rust

    def elem_encode(self, elem, value):
        """Statement encoding one element held in `value` (a place, not a ref)."""
        if elem.kind == Elem.SCALAR:
            if elem.prim == 'double':
                return 'enc.u64(%s.to_bits())?;' % value
            return 'enc.%s(%s)?;' % (elem.method, value)
        if elem.kind == Elem.HANDLE:
            return 'enc.handle(%s.0)?;' % value.lstrip('*')
        if elem.kind == Elem.UNION:
            return '%s.encode(enc)?;' % value
        return '%s.encode_with(enc, §)?;' % value

    def elem_zero_width(self, elem, validity):
        return (elem.kind == Elem.STRUCT and validity == PARTIAL
                and self.m.zero_width[elem.ty])

    def dyn_elems(self, elem, n):
        """Decode `n` elements of a dynamic array: typed bulk reads for
        scalars (`vn_decode_*_array`), packed for `uint16_t`, one by one for
        everything else."""
        if elem.kind == Elem.SCALAR and elem.method in ('u32', 'f32'):
            return 'dec.%s_array(%s)?' % (elem.method, n)
        if elem.kind == Elem.SCALAR and elem.method in ('u64', 'size'):
            return 'dec.u64_array(%s)?' % n
        if elem.is_u16():
            return 'decode_u16_array(dec, %s)?' % n
        return 'decode_vec(dec, %s, %s)?' % (n, self.elem_decode_closure(elem))

    def each_encode(self, elem, seq):
        """Encode every element of `seq` (a slice)."""
        if elem.is_u16():
            return 'encode_u16_array(enc, %s)?;' % seq
        if elem.is_compound():
            return 'for e in %s { %s }' % (seq, self.elem_encode(elem, 'e'))
        return 'for e in %s { %s }' % (seq, self.elem_encode(elem, '*e'))

    # ---- decode: mirrors _decode_variable_info + _decode_variable -----------

    def decode_expr(self, f, validity, driver, access, owner):
        """The value of field `f`, decoded with the given validity.

        `driver` selects the guest-side null-branch rules used for replies:
        a null array is always read unchecked and a null pointer is never
        fatal (`not self.is_driver and ...` in `_decode_variable`).
        """
        what = '"%s", "%s"' % (owner, f.c_name)
        shape = f.shape

        if shape == Field.PLAIN:
            if validity == INVALID:
                return 'Default::default()'
            return self.elem_decode(f.elem)

        if shape == Field.STATIC:
            if validity == INVALID:
                return self.default_expr(f)
            if f.elem.is_byte():
                helper = 'decode_char_array' if f.elem.prim == 'char' else 'decode_byte_array'
                return '%s::<%d>(dec)?' % (helper, f.n)
            if f.elem.is_u16():
                return 'decode_u16_fixed::<%d>(dec)?' % f.n
            return 'decode_fixed_array::<_, %d>(dec, %s)?' % (f.n, self.elem_decode_closure(f.elem))

        if shape == Field.PTR:
            if validity == INVALID:
                inner = 'Default::default()'
            else:
                inner = self.elem_decode(f.elem)
            if not driver and not f.optional and f.can_validate:
                null = 'return Err(null_pointer(dec, %s))' % what
            else:
                null = 'None'
            return 'if dec.simple_pointer()? { Some(%s) } else { %s }' % (inner, null)

        if shape == Field.NULL_ONLY:
            raise AssertionError('NULL_ONLY is a statement, not a value')

        if shape == Field.STRING:
            if validity != VALID:
                raise Unsupported('%s.%s: output string' % (owner, f.c_name))
            # peek, then the size unchecked in both branches: exactly
            # Decoder::opt_string
            return 'dec.opt_string()?'

        # DYN, NESTED, STRING_ARRAY, BLOB, BLOB_OUT: the array size is the
        # presence marker.
        count = self.count_expr(f.count, access)
        checked = (not driver and not f.optional and f.can_validate)
        presence = 'array_presence(dec, %s, %s)?' % (
            count, 'NullArray::Checked' if checked else 'NullArray::Unchecked')

        if validity == INVALID:
            if shape not in (Field.DYN, Field.BLOB_OUT):
                raise Unsupported('%s.%s: output %s' % (owner, f.c_name, shape))
            return '%s.map(|_| Vec::new())' % presence

        if shape == Field.STRING_ARRAY:
            elems = 'decode_vec(dec, n, decode_string_element)?'
        elif shape == Field.BLOB:
            elems = 'dec.blob(n)?'
        elif shape == Field.BLOB_OUT:
            elems = 'decode_owned_blob(dec, n)?'
        elif shape == Field.NESTED:
            if validity != VALID:
                raise Unsupported('%s.%s: partial nested array' % (owner, f.c_name))
            index = '_i' if f.inner.kind == Count.CONST else 'i'
            inner = self.count_expr(f.inner, access, 'i')
            each = self.dyn_elems(f.elem, 'n').replace('§', 'false')
            if each.startswith(('decode_vec(', 'decode_u16_array(')):
                each = each[:-1]    # already a Result<_, ProtocolError>
            else:
                each = 'Ok(%s)' % each
            body = 'let n = inner_array(dec, %s)?; %s' % (inner, each)
            elems = 'decode_vec_indexed(dec, n, |dec, %s| { %s })?' % (index, body)
        elif self.elem_zero_width(f.elem, validity):
            return '%s.map(|_| Vec::new())' % presence
        else:
            elems = self.dyn_elems(f.elem, 'n')
        return 'match %s { Some(n) => Some(%s), None => None }' % (presence, elems)

    def null_only_decode(self, f, owner):
        """`_decode_variable` for a pointer the wire cannot carry: fatal when
        set, and when null fatal too unless optional or noautovalidity."""
        if f.var.ty.base.name == 'VkAllocationCallbacks':
            return 'dec.null_allocator()?;'
        what = '"%s", "%s"' % (owner, f.c_name)
        if not f.optional and f.can_validate:
            return ('if dec.simple_pointer()? { return Err(unsupported_pointer(dec, %s)); } '
                    'else { return Err(null_pointer(dec, %s)); }' % (what, what))
        return 'if dec.simple_pointer()? { return Err(unsupported_pointer(dec, %s)); }' % what

    # ---- encode: mirrors _encode_variable_info + _encode_variable -----------

    def encode_stmts(self, f, validity, access, owner):
        what = '"%s", "%s"' % (owner, f.c_name)
        shape = f.shape
        val = access(f.name)

        if shape == Field.NULL_ONLY:
            if f.var.ty.base.name == 'VkAllocationCallbacks':
                return 'enc.null_allocator()?;'
            return 'enc.simple_pointer(false)?;'

        if shape == Field.PLAIN:
            if validity == INVALID:
                return ''
            if f.stride_of is not None:
                # `stride = sizeof(...)` before it is encoded: the elements
                # travel packed, whatever stride the caller used.
                return 'enc.u32(%d)?;' % f.stride_of
            if f.selector is not None:
                return '%s.encode_tagged(enc, %s)?;' % (val, access(snake(f.selector.name)))
            return self.elem_encode(f.elem, val)

        if shape == Field.STATIC:
            if validity == INVALID:
                return ''
            if f.elem.is_byte():
                return 'enc.array_size(%d)?; enc.blob(&%s)?;' % (f.n, val)
            return 'enc.array_size(%d)?; %s' % (f.n, self.each_encode(f.elem, '&' + val))

        if shape == Field.PTR:
            if validity == INVALID:
                return 'enc.simple_pointer(%s.is_some())?;' % val
            if f.elem.is_compound():
                body = self.elem_encode(f.elem, 'v')
            else:
                body = self.elem_encode(f.elem, '*v')
            return 'enc.simple_pointer(%s.is_some())?; if let Some(v) = &%s { %s }' % (
                val, val, body)

        if shape == Field.STRING:
            if validity != VALID:
                raise Unsupported('%s.%s: output string' % (owner, f.c_name))
            return 'enc.opt_string(%s)?;' % val

        count = self.count_expr(f.count, access)

        if validity == INVALID:
            return 'enc.array_size(if %s.is_some() { %s } else { 0 })?;' % (val, count)

        if shape == Field.STRING_ARRAY:
            each = 'for e in v { enc.opt_string(Some(e))?; }'
        elif shape in (Field.BLOB, Field.BLOB_OUT):
            each = 'enc.blob(v)?;'
        elif shape == Field.NESTED:
            inner = self.count_expr(f.inner, access, 'i')
            loop = 'for e in v' if f.inner.kind == Count.CONST else \
                'for (i, e) in v.iter().enumerate()'
            each = ('%s { let inner = %s; check_len(%s, inner, e.len())?; '
                    'enc.array_size(inner)?; %s }' % (
                        loop, inner, what, self.each_encode(f.elem, 'e').replace('§', 'false')))
        elif self.elem_zero_width(f.elem, validity):
            each = None
        else:
            each = self.each_encode(f.elem, 'v')

        if each is None:
            # Zero-width elements: the count alone is the encoding, and the
            # vector is not required to hold `count` default elements.
            present = 'enc.array_size(%s)?;' % count
        else:
            present = ('let count = %s; check_len(%s, count, v.len())?; '
                       'enc.array_size(count)?; %s' % (count, what, each))

        if each is None:
            if f.condition:
                return 'if %s { %s } else { enc.array_size(0)?; }' % (f.condition, present)
            return 'if %s.is_some() { %s } else { enc.array_size(0)?; }' % (val, present)
        if f.condition:
            # IGNORABLE_LIST: the C encodes the array only when the condition
            # holds, and a null pointer there is its crash, our count check.
            assert access('') == 'self.', 'conditions are struct members'
            return ('if %s { let v = %s.as_deref().unwrap_or_default(); %s } '
                    'else { enc.array_size(0)?; }' % (f.condition, val, present))
        return ('if let Some(v) = &%s { %s } else { enc.array_size(0)?; }' % (val, present))

    # ---- field decode, shared by structs and commands ------------------------

    def let_decode(self, owner, f, expr):
        """The `let` (or `let`s) binding field `f` to `expr`, plus the
        checks that follow the whole body. A selected union binds its wire
        tag beside it; a stride parameter is checked where it is read."""
        name = owner.name
        if f.selector is not None:
            sel = self.m.elem_of(f.elem.ty.sty)
            stmt = 'let (%s_tag, %s): (%s, %s) = %s::decode_tagged(dec)?;' % (
                f.name, f.name, sel.rust, self.field_type(f), f.elem.rust)
            post = ('check_union_tag(dec, "%s", "%s", i64::from(%s_tag), i64::from(%s))?;' % (
                name, f.c_name, f.name, snake(f.selector.name)))
            return [stmt], [post]
        out = ['let %s: %s = %s;' % (f.name, self.field_type(f), expr)]
        if f.stride_of is not None:
            out.append('check_stride(dec, "%s", "%s", %s, %d)?;' % (
                name, f.c_name, f.name, f.stride_of))
        return out, []

    # ---- struct bodies ------------------------------------------------------

    def body_decode(self, ty):
        """`let` statements decoding every field of struct `ty`, honouring the
        runtime `partial` flag. Returns (statements, uses_partial)."""
        out = []
        post = []
        uses = False
        owner = ty.name

        def access(name):
            return name

        for f in self.m.fields[ty]:
            if f.shape == Field.NULL_ONLY:
                out.append(self.null_only_decode(f, owner))
                continue
            vf = VALID
            vp_ = self.m.validity(ty, f, False) if self.m.has_partial(ty) else VALID
            if f.selector is not None:
                if vp_ != VALID:
                    raise Unsupported('%s.%s: a selected union in a skeleton' % (owner, f.c_name))
                stmts, checks = self.let_decode(ty, f, None)
                out.extend(stmts)
                post.extend(checks)
                continue
            a = self.decode_expr(f, vf, False, access, owner)
            b = self.decode_expr(f, vp_, False, access, owner)
            if a == b:
                if '§' in a and vp_ == PARTIAL:
                    uses = True
                    expr = a.replace('§', 'partial')
                else:
                    expr = a.replace('§', 'false')
            else:
                uses = True
                expr = 'if partial { %s } else { %s }' % (
                    b.replace('§', 'true' if vp_ == PARTIAL else 'false'),
                    a.replace('§', 'false'))
            stmts, checks = self.let_decode(ty, f, expr)
            out.extend(stmts)
            post.extend(checks)
        return out + post, uses

    def body_encode(self, ty):
        out = []
        uses = False
        owner = ty.name

        def access(name):
            return 'self.' + name

        for f in self.m.fields[ty]:
            vp_ = self.m.validity(ty, f, False) if self.m.has_partial(ty) else VALID
            a = self.encode_stmts(f, VALID, access, owner)
            b = self.encode_stmts(f, vp_, access, owner)
            if a == b:
                if '§' in a and vp_ == PARTIAL:
                    uses = True
                    out.append(a.replace('§', 'partial'))
                elif a:
                    out.append(a.replace('§', 'false'))
            else:
                uses = True
                a = a.replace('§', 'false')
                b = b.replace('§', 'true' if vp_ == PARTIAL else 'false')
                if not b:
                    out.append('if !partial { %s }' % a)
                elif not a:
                    out.append('if partial { %s }' % b)
                else:
                    out.append('if partial { %s } else { %s }' % (b, a))
        return out, uses

    # ---- unions: mirrors types_union.h -----------------------------------------

    def union_cases(self, ty):
        return ty.attrs['rust_cases']

    def union_variant(self, f):
        return camel(f.c_name)

    def union_member_decode(self, ty, f):
        """The expression a case decodes its member with (VALID, with
        storage: `decode_struct_member(ty, var, 'val->', False, ...)`)."""
        return self.decode_expr(f, VALID, False, lambda n: n, ty.name).replace('§', 'false')

    def union_member_encode(self, f):
        """Statements encoding a case's member, bound as `value` (a ref)."""
        sh = f.shape
        if sh == Field.NULL_ONLY:
            return 'enc.simple_pointer(false)?;'
        if sh == Field.PLAIN:
            if f.elem.kind == Elem.SCALAR:
                if f.elem.prim == 'double':
                    return 'enc.u64(value.to_bits())?;'
                return 'enc.%s(*value)?;' % f.elem.method
            if f.elem.kind == Elem.HANDLE:
                return 'enc.handle(value.0)?;'
            if f.elem.kind == Elem.UNION:
                return 'value.encode(enc)?;'
            return 'value.encode_with(enc, false)?;'
        if sh == Field.STATIC:
            if f.elem.is_byte():
                return 'enc.array_size(%d)?; enc.blob(value)?;' % f.n
            return 'enc.array_size(%d)?; %s' % (f.n, self.each_encode(f.elem, 'value').replace('§', 'false'))
        if sh == Field.PTR:
            if f.elem.is_compound():
                body = self.elem_encode(f.elem, 'v').replace('§', 'false')
            else:
                body = self.elem_encode(f.elem, '*v')
            return 'enc.simple_pointer(value.is_some())?; if let Some(v) = value { %s }' % body
        raise AssertionError(sh)

    @staticmethod
    def union_tag_pattern(tags):
        return ' | '.join(t[0] if isinstance(t[0], str) else str(t[1]) for t in tags)

    # ---- the link walk -----------------------------------------------------------

    def walk_stmts(self, fields, access):
        """Statements visiting every pNext link reachable through `fields`."""
        out = []
        for f in fields:
            if not self.m.walks(f):
                continue
            val = access(f.name)
            if f.shape == Field.PLAIN:
                out.append('%s.for_each_link(f);' % val)
            elif f.shape == Field.PTR:
                out.append('if let Some(v) = &%s { v.for_each_link(f); }' % val)
            elif f.shape == Field.STATIC:
                out.append('for e in &%s { e.for_each_link(f); }' % val)
            elif f.shape == Field.DYN:
                out.append('for e in %s.iter().flatten() { e.for_each_link(f); }' % val)
            elif f.shape == Field.NESTED:
                out.append('for e in %s.iter().flatten().flatten() { e.for_each_link(f); }' % val)
            else:
                raise AssertionError(f.shape)
        return out

    def union_walk(self, ty):
        """The body of a union's `for_each_link`: a `match`, or an `if let`
        where only one member can hold a link."""
        arms = []
        exhaustive = True
        for f, _tags in self.union_cases(ty):
            if not self.m.walks(f):
                exhaustive = False
                continue
            v = self.union_variant(f)
            if f.shape == Field.PLAIN:
                arms.append(('Self::%s(value)' % v, 'value.for_each_link(f);'))
            elif f.shape == Field.PTR:
                exhaustive = False
                arms.append(('Self::%s(Some(value))' % v, 'value.for_each_link(f);'))
            elif f.shape == Field.STATIC:
                arms.append(('Self::%s(value)' % v, 'for e in value { e.for_each_link(f); }'))
            else:
                raise AssertionError(f.shape)
        return self.match_or_if_let(arms, exhaustive)

    @staticmethod
    def match_or_if_let(arms, exhaustive):
        if len(arms) == 1 and not exhaustive:
            return 'if let %s = self { %s }' % arms[0]
        body = ' '.join('%s => { %s }' % a for a in arms)
        if not exhaustive:
            body += ' _ => {}'
        return 'match self { %s }' % body

    def next_walk(self, ty):
        """The body of a pNext enum's `for_each_link`."""
        chain = self.m.chains[ty]
        arms = [('Self::%s(link)' % c.name, 'link.for_each_link(f);')
                for c in chain if self.m.reach_chain[c]]
        return self.match_or_if_let(arms, len(arms) == len(chain))

    # ---- commands -----------------------------------------------------------

    def command_decode(self, cmd):
        out = []
        post = []
        owner = cmd.name

        def access(name):
            return name

        fields = self.m.fields[cmd]
        for i, f in enumerate(fields):
            if f.shape == Field.NULL_ONLY:
                out.append(self.null_only_decode(f, owner))
                continue
            v = self.m.command_validity(cmd, f)
            if f.selector is not None:
                raise Unsupported('%s.%s: selected union parameter' % (owner, f.c_name))
            expr = self.decode_expr(f, v, False, access, owner)
            expr = expr.replace('§', 'true' if v == PARTIAL else 'false')
            stmts, checks = self.let_decode(cmd, f, expr)
            out.extend(stmts)
            post.extend(checks)
            if i == 0 and f.shape == Field.PLAIN and f.elem.kind == Elem.HANDLE \
                    and f.elem.ty.dispatchable:
                out.append('if %s.0 == 0 { return Err(null_dispatch_handle(dec, "%s")); }' % (
                    f.name, owner))
        return out + post

    def command_encode(self, cmd):
        out = []

        def access(name):
            return 'self.' + name

        for f in self.m.fields[cmd]:
            v = self.m.command_validity(cmd, f)
            s = self.encode_stmts(f, v, access, cmd.name)
            s = s.replace('§', 'true' if v == PARTIAL else 'false')
            if s:
                out.append(s)
        return out

    def ret_elem(self, cmd):
        return self.m.elem_of(cmd.ret.ty.base) if cmd.ret else None

    def reply_encode(self, cmd):
        out = []

        def access(name):
            return 'self.' + name

        if cmd.ret:
            out.append(self.elem_encode(self.ret_elem(cmd), 'self.ret'))
        for f in self.m.fields[cmd]:
            if not self.m.is_out(f):
                continue
            s = self.encode_stmts(f, VALID, access, cmd.name).replace('§', 'false')
            if s:
                out.append(s)
        return out

    def reply_decode(self, cmd):
        out = []

        def access(name):
            return 'self.' + name

        if cmd.ret:
            out.append('self.ret = %s;' % self.elem_decode(self.ret_elem(cmd)))
        for f in self.m.fields[cmd]:
            if not self.m.is_out(f):
                continue
            expr = self.decode_expr(f, VALID, True, access, cmd.name).replace('§', 'false')
            out.append('self.%s = %s;' % (f.name, expr))
        return out

    def command_args_name(self, cmd):
        return cmd.name[2:] + 'Args'

    def command_variant(self, cmd):
        return cmd.name[2:]

    def const_name(self, cmd):
        return 'VK_COMMAND_TYPE_%s_EXT' % self.m.reg.upper_name(cmd.name[2:])


# ---------------------------------------------------------------------------
# Driver
# ---------------------------------------------------------------------------

def banner(template_name):
    return ('// @generated by tools/venus-protocol/rust_protocol.py (templates/%s)\n'
            '// from venus-protocol git-%s. DO NOT EDIT: regenerate with\n'
            '//     python scripts/venus-gen.py\n' % (template_name, VENUS_PROTOCOL_REVISION))


def render(template_name, **kwargs):
    template = Template(filename=str(TEMPLATE_DIR.joinpath(template_name)),
                        lookup=TEMPLATE_LOOKUP, output_encoding='utf-8')
    body = template.render(**kwargs).decode('utf-8')
    return banner(template_name) + '\n' + body


def generate(selection_path, outdir):
    selection = Selection.parse(selection_path)
    model = Model(selection)
    rust = Rust(model)
    outdir = Path(outdir)
    outdir.mkdir(parents=True, exist_ok=True)
    files = {}
    common = dict(M=model, R=rust, VALID=VALID, PARTIAL=PARTIAL, INVALID=INVALID,
                  Field=Field, Elem=Elem, VkType=VkType)
    files['defines.rs'] = render('rust_defines.rs', **common)
    files['info.rs'] = render('rust_info.rs', **common)
    for group, content in model.groups.items():
        files['%s.rs' % group] = render('rust_group.rs', GROUP=group,
                                        STRUCTS=content['structs'],
                                        COMMANDS=content['commands'], **common)
    files['dispatch.rs'] = render('rust_dispatch.rs', **common)
    for name, text in files.items():
        with open(outdir.joinpath(name), 'w', encoding='utf-8', newline='\n') as f:
            f.write(text)
    return model, sorted(files)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--selection', required=True)
    parser.add_argument('--outdir', required=True)
    args = parser.parse_args()
    try:
        _model, names = generate(args.selection, args.outdir)
    except Unsupported as err:
        sys.exit('rust_protocol.py: unsupported construct: %s' % err)
    for name in names:
        print(name)


if __name__ == '__main__':
    main()
