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
# pointer-and-count pair), the bytes on the wire must still be exactly the
# C's; `tools/venus-protocol/harness` checks that against the C output.
#
# Constructs the milestone never reaches (unions, strided arrays, nested
# dynamic arrays, blobs written straight into the reply, arithmetic `len`
# expressions) raise `Unsupported` at generation time rather than emitting
# something plausible. Adding a command that needs one is a generator change,
# made on purpose, not a silent mistranslation.

import argparse
import copy
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
    'try',
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


# ---------------------------------------------------------------------------
# Selection
# ---------------------------------------------------------------------------

class Selection:
    def __init__(self, api, extensions, commands):
        self.api = api
        self.extensions = extensions
        self.commands = commands

    @staticmethod
    def parse(path):
        sections = {'api': [], 'extensions': [], 'commands': []}
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
            if line in sections[current]:
                raise ValueError('%s: %r listed twice' % (path, line))
            sections[current].append(line)
        if len(sections['api']) != 1 or not re.fullmatch(r'1\.\d+', sections['api'][0]):
            raise ValueError('%s: [api] must hold exactly one version like 1.3' % path)
        return Selection(sections['api'][0], sections['extensions'], sections['commands'])


# ---------------------------------------------------------------------------
# Wire shapes
# ---------------------------------------------------------------------------

class Elem:
    """What one element of a field is: a scalar, a handle or a struct."""

    SCALAR = 'scalar'
    HANDLE = 'handle'
    STRUCT = 'struct'

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


class Field:
    """One member of a struct or one parameter of a command, in Rust terms."""

    PLAIN = 'plain'
    STATIC = 'static'
    PTR = 'ptr'
    DYN = 'dyn'
    STRING = 'string'
    STRING_ARRAY = 'string_array'
    BLOB = 'blob'
    NULL_ONLY = 'null_only'

    def __init__(self, owner, var):
        self.owner = owner
        self.var = var
        self.c_name = var.name
        self.name = snake(var.name)
        self.shape = None
        self.elem = None
        self.n = None           # STATIC length
        self.len_var = None     # DYN-like: path of VkVariables naming the count
        self.condition = None   # IGNORABLE_LIST translation, encode side only
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
        self.aliases = []         # scalar alias VkTypes, registry order
        self.handles = []         # handle VkTypes, registry order
        self.enum_consts = {}     # name -> (rust type, value)

        self._init_chain_allowed()
        self._init_commands()
        self._init_closure()
        self._init_fields()
        self._check_partial_closure()
        self._init_zero_width()
        self._init_lifetimes()
        self._init_groups()

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

    def enum_value(self, enum_ty_name, key):
        values = self.reg.type_table[enum_ty_name].enums.values
        return int(values[key], 0)

    # ---- which chain structures are admitted -------------------------------

    def _init_chain_allowed(self):
        api = self.selection.api
        major, minor = (int(x) for x in api.split('.'))
        allowed = set()
        numbers = set()
        for feat in self.reg.features:
            fmaj, fmin = (int(x) for x in feat.number.split('.'))
            numbers.add(feat.number)
            if (fmaj, fmin) <= (major, minor):
                allowed.update(feat.types)
        if api not in numbers:
            raise ValueError('[api] %s is not a Vulkan version in vk.xml' % api)

        exts = {e.name: e for e in self.reg.extensions}
        chosen = set(self.selection.extensions)
        for name in self.selection.extensions:
            if name not in vp.VK_XML_EXTENSION_LIST:
                raise ValueError('extension %s is not in VK_XML_EXTENSION_LIST' % name)
            if name not in exts:
                raise ValueError('extension %s is not in the registry' % name)
        for name in self.selection.extensions:
            ext = exts[name]
            allowed.update(ext.types)
            for deps, types in ext.optional_types.items():
                if self._deps_met(deps, chosen):
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
        self.commands = []
        for name in self.selection.commands:
            cmd = by_name.get(name)
            if cmd is None:
                raise ValueError('command %s is not in the venus protocol' % name)
            if not self.gen.is_serializable(cmd):
                raise ValueError('command %s is not serializable' % name)
            if 'need_blob_encode' in cmd.attrs:
                raise Unsupported('%s writes a blob straight into its reply' % name)
            self.commands.append(cmd)
        # registry order, so the output does not depend on the list's order
        order = {c: i for i, c in enumerate(supported)}
        self.commands.sort(key=lambda c: order[c])

    def chain_of(self, ty):
        types, _skipped = self.gen.get_chain(ty)
        return [t for t in types if t in self.chain_allowed]

    def _init_closure(self):
        seen = []
        visiting = []

        def visit(ty):
            if ty.category == VkType.UNION:
                raise Unsupported('union %s' % ty.name)
            if ty.category != VkType.STRUCT:
                return
            if ty in seen:
                return
            if ty in visiting:
                raise Unsupported('%s contains itself' % ty.name)
            visiting.append(ty)
            for var in ty.variables:
                if var.is_p_next():
                    continue
                if var.maybe_null() and not self.gen.is_serializable(var):
                    continue
                visit(var.ty.base)
            visiting.pop()
            seen.append(ty)
            for nxt in self.chain_of(ty):
                visit(nxt)

        for cmd in self.commands:
            for var in cmd.variables:
                if var.maybe_null() and not self.gen.is_serializable(var):
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
            raise Unsupported('union %s' % base.name)
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

    def _field(self, owner, var):
        f = Field(owner, var)
        ty = var.ty
        base = ty.base

        if 'stride' in var.attrs:
            raise Unsupported('%s.%s: strided array' % (owner.name, var.name))
        if 'selector' in var.attrs:
            raise Unsupported('%s.%s: union selector' % (owner.name, var.name))

        if not self.gen.is_serializable(var):
            if var.maybe_null():
                f.shape = Field.NULL_ONLY
                return f
            raise Unsupported('%s.%s is not serializable' % (owner.name, var.name))

        for ign in vp.Gen.IGNORABLE_LIST:
            if ign.struct == owner.name and ign.var == var.name:
                f.condition = self._translate_condition(ign.condition)

        if ty.is_static_array():
            if ty.is_pointer():
                raise Unsupported('%s.%s: array of pointers' % (owner.name, var.name))
            if 'len_exprs' in var.attrs and 'wa_require_static_len' not in var.attrs:
                raise Unsupported('%s.%s: static array with a len' % (owner.name, var.name))
            f.shape = Field.STATIC
            f.n = self.static_len(var)
            f.elem = self.elem_of(base)
            if f.elem.kind == Elem.SCALAR and f.elem.prim == 'uint16_t':
                raise Unsupported('%s.%s: packed uint16_t array' % (owner.name, var.name))
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
                f.len_var = self._len_path(owner, var, exprs[0], names[0])
                return f
            raise Unsupported('%s.%s: string shape %r' % (owner.name, var.name, exprs))
        if depth != 1 or len(exprs) != 1:
            raise Unsupported('%s.%s: nested dynamic array' % (owner.name, var.name))
        f.len_var = self._len_path(owner, var, exprs[0], names[0])
        if var.is_blob():
            f.shape = Field.BLOB
            f.elem = None
            return f
        f.elem = self.elem_of(base)
        if f.elem.is_byte():
            f.shape = Field.BLOB
        elif f.elem.kind == Elem.SCALAR and f.elem.prim == 'uint16_t':
            raise Unsupported('%s.%s: packed uint16_t array' % (owner.name, var.name))
        else:
            f.shape = Field.DYN
        return f

    def _len_path(self, owner, var, expr, name):
        if expr != name or not name:
            raise Unsupported('%s.%s: len expression %r' % (owner.name, var.name, expr))
        path = owner.find_variables(name)
        if not path or len(path) > 2:
            raise Unsupported('%s.%s: len %r' % (owner.name, var.name, name))
        return path

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
                self.enum_consts[key] = (rust, value)
                return
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
            self.chains[ty] = [t for t in self.chain_of(ty) if t in self.structs]
            self._check_names(ty, fields)
        for cmd in self.commands:
            fields = [self._field(cmd, var) for var in cmd.variables]
            self.fields[cmd] = fields
            self._check_names(cmd, fields)
            if cmd.ret:
                self.elem_of(cmd.ret.ty.base)
        # VkResult names every reply's return value and defines.rs lists its
        # values whether or not a selected command returns one.
        self.elem_of(self.reg.type_table['VkResult'])

    @staticmethod
    def _check_names(ty, fields):
        names = [f.name for f in fields]
        if len(set(names)) != len(names):
            raise Unsupported('%s: field names collide after snake_case' % ty.name)
        if 'p_next' in names or 'ret' in names:
            raise Unsupported('%s: field shadows p_next/ret' % ty.name)

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
        return 'need_partial' in ty.attrs

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
            if f.elem is not None and f.elem.kind == Elem.STRUCT and self.lifetime[f.elem.ty]:
                return True
        return any(self.lifetime[t] for t in self.chains.get(ty, []))

    def chain_lifetime(self, ty):
        return any(self.lifetime[t] for t in self.chains[ty])

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
            if f.elem is not None and f.elem.kind == Elem.STRUCT and f.elem.ty not in acc:
                acc.add(f.elem.ty)
                self._reach(f.elem.ty, acc)
        for t in self.chains.get(ty, []):
            if t not in acc:
                acc.add(t)
                self._reach(t, acc)

    # ---- enum and version tables --------------------------------------------

    def structure_types(self):
        values = self.reg.type_table['VkStructureType'].enums.values
        out = []
        for ty in self.structs:
            if ty.s_type:
                out.append((ty.s_type, int(values[ty.s_type], 0)))
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
        chosen = set(self.selection.extensions)
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
        if elem.kind == Elem.STRUCT:
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
        if f.shape in (Field.STRING, Field.BLOB):
            return "Option<&'a [u8]>"
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

    def count_expr(self, f, access):
        """The array length a len-path names, as a `u64` expression.

        Mirrors `VariableInfo._init_loop_info`: a pointer count reads as
        `(p ? *p : 0)`, a count inside a pointed-to struct as
        `(p ? p->count : 0)`.
        """
        path = f.len_var
        head = path[0]
        head_name = access(snake(head.name))
        if len(path) == 1:
            base = head.ty.base
            if head.ty.is_pointer():
                return self._to_u64(base, '%s.unwrap_or(0)' % head_name)
            return self._to_u64(base, head_name)
        member = path[1]
        inner = self._to_u64(member.ty.base, 'v.%s' % snake(member.name))
        return '%s.as_ref().map_or(0, |v| %s)' % (head_name, inner)

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
        return '%s::decode_with(dec, §)?' % elem.rust

    def elem_decode_closure(self, elem):
        """A closure `|dec| -> Result<T, ProtocolError>` decoding one element."""
        if elem.kind == Elem.SCALAR:
            if elem.prim == 'double':
                return '|dec| dec.u64().map(f64::from_bits).map_err(ProtocolError::from)'
            return '|dec| dec.%s().map_err(ProtocolError::from)' % elem.method
        if elem.kind == Elem.HANDLE:
            return '|dec| dec.handle().map(%s).map_err(ProtocolError::from)' % elem.rust
        return '|dec| %s::decode_with(dec, §)' % elem.rust

    def elem_encode(self, elem, value):
        """Statement encoding one element held in `value` (a place, not a ref)."""
        if elem.kind == Elem.SCALAR:
            if elem.prim == 'double':
                return 'enc.u64(%s.to_bits())?;' % value
            return 'enc.%s(%s)?;' % (elem.method, value)
        if elem.kind == Elem.HANDLE:
            return 'enc.handle(%s.0)?;' % value.lstrip('*')
        return '%s.encode_with(enc, §)?;' % value

    def elem_zero_width(self, elem, validity):
        return (elem.kind == Elem.STRUCT and validity == PARTIAL
                and self.m.zero_width[elem.ty])

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

        # DYN, STRING_ARRAY, BLOB: the array size is the presence marker.
        count = self.count_expr(f, access)
        checked = (not driver and not f.optional and f.can_validate)
        presence = 'array_presence(dec, %s, %s)?' % (
            count, 'NullArray::Checked' if checked else 'NullArray::Unchecked')

        if validity == INVALID:
            if shape != Field.DYN:
                raise Unsupported('%s.%s: output blob' % (owner, f.c_name))
            return '%s.map(|_| Vec::new())' % presence

        if shape == Field.STRING_ARRAY:
            elems = 'decode_vec(dec, n, decode_string_element)?'
        elif shape == Field.BLOB:
            elems = 'dec.blob(n)?'
        elif self.elem_zero_width(f.elem, validity):
            return '%s.map(|_| Vec::new())' % presence
        elif f.elem.kind == Elem.SCALAR and f.elem.method in ('u32', 'f32'):
            elems = 'dec.%s_array(n)?' % f.elem.method
        elif f.elem.kind == Elem.SCALAR and f.elem.method in ('u64', 'size'):
            elems = 'dec.u64_array(n)?'
        else:
            elems = 'decode_vec(dec, n, %s)?' % self.elem_decode_closure(f.elem)
        return 'match %s { Some(n) => Some(%s), None => None }' % (presence, elems)

    def null_only_decode(self, f, owner):
        if f.var.ty.base.name == 'VkAllocationCallbacks':
            return 'dec.null_allocator()?;'
        return 'if dec.simple_pointer()? { return Err(unsupported_pointer(dec, "%s", "%s")); }' % (
            owner, f.c_name)

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
            if f.elem.kind == Elem.STRUCT:
                return self.elem_encode(f.elem, val)
            return self.elem_encode(f.elem, val)

        if shape == Field.STATIC:
            if validity == INVALID:
                return ''
            if f.elem.is_byte():
                return 'enc.array_size(%d)?; enc.blob(&%s)?;' % (f.n, val)
            if f.elem.kind == Elem.STRUCT:
                body = self.elem_encode(f.elem, 'e')
            else:
                body = self.elem_encode(f.elem, '*e')
            return 'enc.array_size(%d)?; for e in &%s { %s }' % (f.n, val, body)

        if shape == Field.PTR:
            if validity == INVALID:
                return 'enc.simple_pointer(%s.is_some())?;' % val
            if f.elem.kind == Elem.STRUCT:
                body = self.elem_encode(f.elem, 'v')
            else:
                body = self.elem_encode(f.elem, '*v')
            return 'enc.simple_pointer(%s.is_some())?; if let Some(v) = &%s { %s }' % (
                val, val, body)

        if shape == Field.STRING:
            if validity != VALID:
                raise Unsupported('%s.%s: output string' % (owner, f.c_name))
            return 'enc.opt_string(%s)?;' % val

        count = self.count_expr(f, access)

        if validity == INVALID:
            return 'enc.array_size(if %s.is_some() { %s } else { 0 })?;' % (val, count)

        if shape == Field.STRING_ARRAY:
            each = 'for e in v { enc.opt_string(Some(e))?; }'
        elif shape == Field.BLOB:
            each = 'enc.blob(v)?;'
        elif self.elem_zero_width(f.elem, validity):
            each = None
        elif f.elem.kind == Elem.STRUCT:
            each = 'for e in v { %s }' % self.elem_encode(f.elem, 'e')
        else:
            each = 'for e in v { %s }' % self.elem_encode(f.elem, '*e')

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

    # ---- struct bodies ------------------------------------------------------

    def body_decode(self, ty):
        """`let` statements decoding every field of struct `ty`, honouring the
        runtime `partial` flag. Returns (statements, uses_partial)."""
        out = []
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
            out.append('let %s: %s = %s;' % (f.name, self.field_type(f), expr))
        return out, uses

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

    # ---- commands -----------------------------------------------------------

    def command_decode(self, cmd):
        out = []
        owner = cmd.name

        def access(name):
            return name

        fields = self.m.fields[cmd]
        for i, f in enumerate(fields):
            if f.shape == Field.NULL_ONLY:
                out.append(self.null_only_decode(f, owner))
                continue
            v = self.m.command_validity(cmd, f)
            expr = self.decode_expr(f, v, False, access, owner)
            expr = expr.replace('§', 'true' if v == PARTIAL else 'false')
            out.append('let %s: %s = %s;' % (f.name, self.field_type(f), expr))
            if i == 0 and f.shape == Field.PLAIN and f.elem.kind == Elem.HANDLE \
                    and f.elem.ty.dispatchable:
                out.append('if %s.0 == 0 { return Err(null_dispatch_handle(dec, "%s")); }' % (
                    f.name, owner))
        return out

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
                  Field=Field, Elem=Elem)
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
