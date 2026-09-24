#!/usr/bin/env python3
"""Regenerate the Venus executor's mechanical majority (EPIC 20 stage 5b.2).

Two files, from one model:

* crates/virtio-gpu/src/venus/executor/generated.rs -- portable, no `unsafe`:
  for every core Vulkan 1.0-1.3 command the executor serves by translation
  ([generated] and [handwritten] in tools/venus-protocol/executor-classes.txt),
  the walk that replaces every guest object id in its inputs -- nested in
  structures, in arrays of structures, in pNext links -- by the host handle a
  `Resolve` answers (typed, parented, 0 only where vk.xml says the handle may
  be null), range-checks every enum and flag word against the values core
  Vulkan 1.0-1.3 defines (vk.xml, `<feature>` <= 1.3), and checks every array
  against the count it travels with. Plus the per-command tables the executor
  needs: which object a command is dispatched on, which core version brought
  it, its `VkResult`, and its output handles.
* crates/virtio-gpu/src/host_vulkan/calls.rs -- the one place the translated
  command becomes a driver call: every structure rebuilt as its `ash` twin in
  an arena (pointers into arena-owned copies, pNext links chained in the
  guest's order), the core entry point called through `ash`'s function table,
  and the outputs (handles, blobs, structures) written back into the command.

Nothing here decides policy. Which commands are served, and how, is the
classification file; what a hand-written command checks beyond this is
executor code. The script refuses to run when the classification does not
cover every core <= 1.3 command of the protocol exactly once, or when its two
hand-written sections disagree with the `match` arms that implement them.

Needs Python 3.8+ with Mako (tools/venus-protocol/rust_protocol.py imports
it; see scripts/venus-gen.py for a venv), the ash 0.38 sources in the cargo
registry (`cargo fetch`), and rustfmt on PATH:

    python scripts/venus-exec-gen.py            # write
    python scripts/venus-exec-gen.py --check    # compare, exit 1 if stale
"""

import argparse
import importlib.util
import re
import sys
import xml.etree.ElementTree as ET
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
TOOLS = ROOT / 'tools' / 'venus-protocol'
sys.path.insert(0, str(TOOLS))
# Importing the two sibling generators must leave no __pycache__ behind.
sys.dont_write_bytecode = True

try:
    import rust_protocol as rp  # noqa: E402
    from vkxml import VkType  # noqa: E402
except ImportError as err:
    sys.exit('venus-exec-gen: %s (is Mako installed in this Python? see scripts/venus-gen.py)' % err)

_spec = importlib.util.spec_from_file_location('venus_ash_gen', ROOT / 'scripts' / 'venus-ash-gen.py')
ashgen = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(ashgen)

CLASSES = TOOLS / 'executor-classes.txt'
SELECTION = TOOLS / 'rust-selection.txt'
EXECUTOR = ROOT / 'crates' / 'virtio-gpu' / 'src' / 'venus' / 'executor'
OUT_EXEC = EXECUTOR / 'generated.rs'
OUT_HOST = ROOT / 'crates' / 'virtio-gpu' / 'src' / 'host_vulkan' / 'calls.rs'

MAX_CORE = (1, 3)
ADMITTED_API = (1 << 22) | (3 << 12)
ADMITTED_EXTENSIONS = {'VK_EXT_command_serialization', 'VK_MESA_venus_protocol'}

# Handles vk.xml lets be null that the executor requires anyway, because a
# driver handed VK_NULL_HANDLE there by a guest dereferences it:
# * a stage's module may be null only with VK_KHR_maintenance5 or graphics
#   pipeline libraries, neither of which a guest can enable here;
# * a compute or graphics pipeline's layout may be null only for a library;
# * vkUpdateDescriptorSets' dstSet is optional only for push descriptors;
# * null set layouts and bound sets are graphics-pipeline-library only;
# * a null immutable sampler is never valid (the array itself is ignored for
#   other descriptor types, and Mesa sends it null then);
# * null index and vertex buffers need maintenance6 / nullDescriptor.
REQUIRED_HANDLES = {
    ('VkPipelineShaderStageCreateInfo', 'module'),
    ('VkComputePipelineCreateInfo', 'layout'),
    ('VkGraphicsPipelineCreateInfo', 'layout'),
    ('VkWriteDescriptorSet', 'dstSet'),
    ('VkPipelineLayoutCreateInfo', 'pSetLayouts'),
    ('VkDescriptorSetLayoutBinding', 'pImmutableSamplers'),
    ('vkCmdBindDescriptorSets', 'pDescriptorSets'),
    ('vkCmdBindIndexBuffer', 'buffer'),
    ('vkCmdBindVertexBuffers', 'pBuffers'),
    ('vkCmdBindVertexBuffers2', 'pBuffers'),
}

# Enum types that are not the guest's to range-check here.
UNCHECKED_ENUMS = {'VkStructureType', 'VkResult'}

ASH_SCALARS = {'u8', 'u16', 'u32', 'i32', 'u64', 'i64', 'f32', 'f64', 'Bool32', 'DeviceSize',
               'DeviceAddress', 'SampleMask', 'Flags', 'Flags64'}


def norm(name):
    return name.replace('_', '').lower()


# --------------------------------------------------------------- classes

def parse_classes():
    sections = {'bespoke': [], 'handwritten': [], 'generated': [], 'refused': []}
    current = None
    for raw in CLASSES.read_text(encoding='utf-8').splitlines():
        line = raw.split('#', 1)[0].strip()
        if not line:
            continue
        if line.startswith('[') and line.endswith(']'):
            current = line[1:-1]
            if current not in sections:
                sys.exit('%s: unknown section [%s]' % (CLASSES, current))
            continue
        if current is None:
            sys.exit('%s: %r before any section' % (CLASSES, line))
        sections[current].append(line)
    seen = {}
    for sec, names in sections.items():
        for n in names:
            if n in seen:
                sys.exit('%s: %s is in [%s] and [%s]' % (CLASSES, n, seen[n], sec))
            seen[n] = sec
    return sections


def match_arms(fn_name):
    """The `Command::X(args)` arms of the match in `fn <fn_name>(`."""
    for path in sorted(EXECUTOR.glob('*.rs')):
        text = path.read_text(encoding='utf-8')
        at = text.find('fn %s(' % fn_name)
        if at < 0:
            continue
        body = text[at:]
        body = body[:body.index('\n    }\n')]
        return path, sorted(set(re.findall(r'Command::(\w+)\((?:args|_)\)', body)))
    sys.exit('venus-exec-gen: no `fn %s(` in %s' % (fn_name, EXECUTOR))


def kind_variants():
    text = (EXECUTOR / 'objects.rs').read_text(encoding='utf-8')
    body = text[text.index('pub enum Kind {'):]
    body = body[:body.index('\n}\n')]
    return set(re.findall(r'^\s*(\w+),', body, re.M))


# ---------------------------------------------------------------- vk.xml

def enum_value(e, extnumber):
    if 'value' in e.attrib:
        return int(e.get('value'), 0)
    if 'bitpos' in e.attrib:
        return 1 << int(e.get('bitpos'))
    if 'offset' in e.attrib:
        n = int(e.get('extnumber', extnumber))
        v = 1000000000 + (n - 1) * 1000 + int(e.get('offset'))
        return -v if e.get('dir') == '-' else v
    return None


def for_vulkan(e):
    api = e.get('api')
    return api is None or 'vulkan' in api.split(',')


def core_enums(vk_xml):
    """Every enum and bitmask type's values in core Vulkan <= 1.3, and each
    Flags type's FlagBits."""
    root = ET.parse(vk_xml).getroot()
    values, bitwidth = {}, {}
    for enums in root.findall('enums'):
        if enums.get('type') not in ('enum', 'bitmask'):
            continue
        name = enums.get('name')
        bitwidth[name] = int(enums.get('bitwidth', '32'))
        d = values.setdefault(name, {})
        for e in enums.findall('enum'):
            if e.get('alias') or not for_vulkan(e):
                continue
            v = enum_value(e, None)
            if v is not None:
                d[e.get('name')] = v
    for feat in root.findall('feature'):
        if not for_vulkan(feat) or 'vulkan' not in feat.get('api', 'vulkan').split(','):
            continue
        if tuple(int(x) for x in feat.get('number').split('.')) > MAX_CORE:
            continue
        for req in feat.findall('require'):
            if not for_vulkan(req):
                continue
            for e in req.findall('enum'):
                ext = e.get('extends')
                if not ext or e.get('alias') or not for_vulkan(e):
                    continue
                v = enum_value(e, None)
                if v is not None:
                    values.setdefault(ext, {})[e.get('name')] = v
    flagbits = {}
    for t in root.find('types').findall('type'):
        if t.get('category') != 'bitmask':
            continue
        name = t.get('name') or (t.find('name').text if t.find('name') is not None else None)
        if t.get('alias'):
            continue
        bits = t.get('requires') or t.get('bitvalues')
        flagbits[name] = bits
    return values, bitwidth, flagbits


def ranges(values):
    vs = sorted(set(values))
    out = []
    for v in vs:
        if out and v == out[-1][1] + 1:
            out[-1][1] = v
        else:
            out.append([v, v])
    return out


# ------------------------------------------------------------------ ash

UNION_RE = re.compile(r"^pub union (\w+)(<'a>)? \{\n(.*?)^\}", re.M | re.S)
PFN_RE = re.compile(r'^pub type PFN_(vk\w+) =\s*unsafe extern "system" fn\((.*?)\)(?:\s*->\s*([\w:]+))?;',
                    re.M | re.S)
FN_TABLE_RE = re.compile(r'^pub struct DeviceFnV1_(\d) \{\n(.*?)^\}', re.M | re.S)


def ash_sources():
    defs = ashgen.find_ash()
    vk = defs.parent
    src = vk.parent
    structs = ashgen.parse_structs(defs.read_text(encoding='utf-8'))
    unions = {}
    for m in UNION_RE.finditer(defs.read_text(encoding='utf-8')):
        fields = []
        for line in m.group(3).splitlines():
            fm = ashgen.FIELD_RE.match(line.strip())
            if fm:
                fields.append((fm.group(1), fm.group(2).strip()))
        unions[m.group(1)] = fields
    pfns = {}
    pfn_text = (vk / 'features.rs').read_text(encoding='utf-8') + (vk / 'extensions.rs').read_text(encoding='utf-8')
    for m in PFN_RE.finditer(pfn_text):
        params = []
        for p in re.split(r',\s*', m.group(2).strip().rstrip(',')):
            p = p.strip()
            if not p:
                continue
            pn, pt = p.split(':', 1)
            params.append((pn.strip(), pt.strip()))
        pfns[m.group(1)] = (params, m.group(3))
    tables = {}
    for m in FN_TABLE_RE.finditer((src / 'tables.rs').read_text(encoding='utf-8')):
        for fm in re.finditer(r'pub (\w+): PFN_(vk\w+),', m.group(2)):
            tables[fm.group(2)] = ('fp_v1_%s' % m.group(1), fm.group(1))
    return structs, unions, pfns, tables


# ------------------------------------------------------------ the model

class Gen:
    def __init__(self):
        self.classes = parse_classes()
        self.m = rp.Model(rp.Selection.parse(SELECTION))
        self.r = rp.Rust(self.m)
        self.values, self.bitwidth, self.flagbits = core_enums(rp.vp.VN_PROTOCOL_VK_XML)
        self.kinds = kind_variants()
        self.core = {}
        for feat in self.m.reg.features:
            ver = tuple(int(x) for x in feat.number.split('.'))
            for ty in feat.types:
                if ty.category == VkType.COMMAND:
                    if ty.name not in self.core or ver < self.core[ty.name]:
                        self.core[ty.name] = ver
        self.inscope = [c for c in self.m.commands
                        if c.name in self.core and self.core[c.name] <= MAX_CORE]
        self.check_classes()
        self.served = [c for c in self.inscope if c.name in self.cls_translated]
        self.origins = {name: (packed, exts)
                        for (_v, name, _s, packed, exts) in self.m.structure_origins()}
        self.input_structs = self.reach(self.served)
        self.structs, self.unions, self.pfns, self.tables = ash_sources()

    # ---- classification ---------------------------------------------------

    def check_classes(self):
        names = {c.name for c in self.inscope}
        listed = set()
        for sec, entries in self.classes.items():
            for n in entries:
                if n not in names:
                    sys.exit('%s: [%s] %s is not a core <= 1.3 command of the protocol' % (CLASSES, sec, n))
                listed.add(n)
        missing = sorted(names - listed)
        if missing:
            sys.exit('%s: core commands in no section: %s' % (CLASSES, ', '.join(missing)))
        for sec, fn in (('bespoke', 'dispatch'), ('handwritten', 'dispatch_objects')):
            want = sorted(n[2:] for n in self.classes[sec])
            path, arms = match_arms(fn)
            if want != arms:
                sys.exit('%s: [%s] disagrees with `fn %s` in %s:\n  only listed: %s\n  only in the match: %s'
                         % (CLASSES, sec, fn, path, sorted(set(want) - set(arms)),
                            sorted(set(arms) - set(want))))
        self.cls_translated = set(self.classes['handwritten']) | set(self.classes['generated'])

    # ---- reachability -----------------------------------------------------

    def admitted(self, ty):
        o = self.origins.get(ty.name)
        if o is None:
            return True
        packed, exts = o
        return (packed is not None and packed <= ADMITTED_API) or bool(set(exts) & ADMITTED_EXTENSIONS)

    def reach(self, commands):
        seen, order = set(), []

        def visit(ty):
            if ty in seen or ty.category not in (VkType.STRUCT, VkType.UNION):
                return
            seen.add(ty)
            for f in self.m.fields[ty]:
                if f.elem is not None and f.elem.is_compound():
                    visit(f.elem.ty)
            for t in self.m.chains.get(ty, []):
                if self.admitted(t):
                    visit(t)
            order.append(ty)

        for c in commands:
            for f in self.m.fields[c]:
                if self.m.is_out(f):
                    continue
                if f.elem is not None and f.elem.is_compound():
                    visit(f.elem.ty)
        return order

    # ---- helpers ----------------------------------------------------------

    def lt(self, ty):
        return "<'_>" if self.m.lifetime.get(ty) else ''

    def kind(self, handle_ty):
        k = handle_ty.name[2:]
        if k not in self.kinds:
            sys.exit('venus-exec-gen: handle %s has no objects::Kind::%s' % (handle_ty.name, k))
        return 'Kind::%s' % k

    def may_be_null(self, owner, f, level=0):
        if (owner.name, f.c_name) in REQUIRED_HANDLES:
            return False
        opt = f.var.attrs.get('optional', [])
        if level < len(opt) and opt[level] == 'true':
            return True
        return not f.can_validate

    def enum_check(self, owner, f, value):
        """A boolean Rust expression that is true when `value` is allowed,
        or None when the field is not checked."""
        ty = f.elem.ty
        if f.elem.kind != rp.Elem.SCALAR or not f.can_validate:
            return None
        if ty.category == VkType.ENUM:
            if ty.name in UNCHECKED_ENUMS:
                return None
            vals = self.values.get(ty.name)
            if vals is None:
                sys.exit('venus-exec-gen: no vk.xml values for %s' % ty.name)
            if 'FlagBits' in ty.name:
                mask = 0
                for v in vals.values():
                    mask |= v
                # A FlagBits member names bits of its Flags: inside the core
                # mask, and not 0 unless 0 is a core value (NONE) or the
                # member is optional.
                zero_ok = 0 in vals.values() or f.optional
                if self.bitwidth.get(ty.name, 32) == 64:
                    expr = '%s & !%#x_u64 == 0' % (value, mask)
                else:
                    expr = '(%s as u32) & !%#x_u32 == 0' % (value, mask)
                if not zero_ok:
                    expr = '%s != 0 && %s' % (value, expr)
                return expr
            return 'e_%s(%s)' % (ty.name, value)
        if ty.category == VkType.BITMASK:
            bits = self.flagbits.get(ty.name)
            mask = 0
            if bits:
                for v in self.values.get(bits, {}).values():
                    mask |= v
            if self.m._primitive_of(ty) == 'uint64_t':
                return '%s & !%#x_u64 == 0' % (value, mask)
            return '%s & !%#x_u32 == 0' % (value, mask)
        return None

    # ---- executor side ----------------------------------------------------

    def field_checks(self, owner, fields, acc, owner_name, skip_out=False, skip_first=None):
        out = []
        for f in fields:
            if skip_out and self.m.is_out(f):
                continue
            if f.shape == rp.Field.NULL_ONLY:
                continue
            what = '"%s.%s"' % (owner_name, f.c_name)
            v = acc(f.name)
            # counts
            if f.count is not None and f.shape in (rp.Field.DYN, rp.Field.BLOB, rp.Field.STRING_ARRAY):
                count = self.r.count_expr(f.count, acc)
                out.append('if let Some(x) = &%s { chk_len(r, %s, x.len(), %s)?; }' % (v, what, count))
                if not f.optional and f.can_validate:
                    out.append('if %s.is_none() && %s != 0 { return Err(r.invalid(format!("{} is null with a nonzero count", %s))); }' % (v, count, what))
            e = f.elem
            if e is None:
                continue
            if e.kind == rp.Elem.HANDLE:
                k = self.kind(e.ty)
                if f.shape == rp.Field.PLAIN:
                    null = 'true' if self.may_be_null(owner, f) else 'false'
                    if skip_first is not None and f is skip_first:
                        null = 'false'
                    out.append('%s.0 = r.handle(%s, %s.0, %s, %s)?;' % (v, k, v, null, what))
                elif f.shape == rp.Field.DYN:
                    null = 'true' if self.may_be_null(owner, f, 1) else 'false'
                    out.append('for h in %s.iter_mut().flatten() { h.0 = r.handle(%s, h.0, %s, %s)?; }' % (v, k, null, what))
                elif f.shape == rp.Field.PTR:
                    null = 'true' if self.may_be_null(owner, f, 1) else 'false'
                    out.append('if let Some(h) = %s.as_mut() { h.0 = r.handle(%s, h.0, %s, %s)?; }' % (v, k, null, what))
                elif f.shape == rp.Field.STATIC:
                    out.append('for h in %s.iter_mut() { h.0 = r.handle(%s, h.0, false, %s)?; }' % (v, k, what))
                continue
            if e.kind == rp.Elem.STRUCT:
                name = e.ty.name
                if f.shape == rp.Field.PLAIN:
                    out.append('t_%s(r, &mut %s)?;' % (name, v))
                elif f.shape == rp.Field.PTR:
                    out.append('if let Some(x) = %s.as_mut() { t_%s(r, x)?; }' % (v, name))
                elif f.shape in (rp.Field.DYN, rp.Field.STATIC):
                    it = '%s.iter_mut().flatten()' % v if f.shape == rp.Field.DYN else '%s.iter_mut()' % v
                    out.append('for x in %s { t_%s(r, x)?; }' % (it, name))
                continue
            if e.kind == rp.Elem.UNION:
                continue
            # scalars: enum and flag checks
            if f.shape == rp.Field.PLAIN:
                chk = self.enum_check(owner, f, v)
                if chk:
                    out.append('if !(%s) { return Err(r.invalid(format!("{} = {:#x} is not a Vulkan 1.3 value", %s, %s))); }' % (chk, what, v))
            elif f.shape in (rp.Field.DYN, rp.Field.STATIC):
                chk = self.enum_check(owner, f, '*x')
                if chk:
                    it = '%s.iter().flatten()' % v if f.shape == rp.Field.DYN else '%s.iter()' % v
                    out.append('for x in %s { if !(%s) { return Err(r.invalid(format!("{} = {:#x} is not a Vulkan 1.3 value", %s, *x))); } }' % (it, chk, what))
            elif f.shape == rp.Field.PTR:
                chk = self.enum_check(owner, f, '*x')
                if chk:
                    out.append('if let Some(x) = %s.as_ref() { if !(%s) { return Err(r.invalid(format!("{} = {:#x} is not a Vulkan 1.3 value", %s, *x))); } }' % (v, chk, what))
        return out

    def emit_exec(self):
        out = []
        w = out.append
        w('// @generated by scripts/venus-exec-gen.py from the generated Venus protocol,')
        w('// vk.xml (core <= 1.3) and tools/venus-protocol/executor-classes.txt.')
        w('// DO NOT EDIT: regenerate with `python scripts/venus-exec-gen.py`.')
        w('')
        w('//! The executor\'s mechanical majority (EPIC 20 stage 5b.2): for every core')
        w('//! command it serves by translation, the walk that turns guest object ids')
        w('//! into host handles and range-checks every value a driver would index by,')
        w('//! and the tables the executor asks of a command. No `unsafe`, no policy:')
        w('//! [`Resolve`] decides what an id means, the executor decides what to do.')
        w('')
        w('#![allow(clippy::all, clippy::pedantic, non_snake_case, dead_code, unused_variables, unreachable_patterns)]')
        w('')
        w('use crate::venus::protocol::*;')
        w('')
        w('use super::context::ExecError;')
        w('use super::objects::Kind;')
        w('')
        w('/// What translation asks of the context.')
        w('pub trait Resolve {')
        w('    /// The host handle guest id `id` of `kind` names, as a raw `u64`, or the')
        w('    /// refusal: an unknown id, one of another type or another device. `0` is')
        w('    /// answered with `0` exactly when `may_be_null`. `what` names the field.')
        w('    ///')
        w('    /// # Errors')
        w('    /// The refusal, fatal to the context.')
        w('    fn handle(&mut self, kind: Kind, id: u64, may_be_null: bool, what: &\'static str) -> Result<u64, ExecError>;')
        w('    /// A refusal of a value.')
        w('    fn invalid(&self, what: String) -> ExecError;')
        w('    /// A refusal of a chained structure the executor does not admit.')
        w('    fn link(&self, parent: &\'static str, stype: i32) -> ExecError;')
        w('}')
        w('')
        w('fn chk_len(r: &dyn Resolve, what: &\'static str, len: usize, count: u64) -> Result<(), ExecError> {')
        w('    if u64::try_from(len).map_or(true, |len| len != count) {')
        w('        return Err(r.invalid(format!("{what} has {len} elements and a count of {count}")));')
        w('    }')
        w('    Ok(())')
        w('}')
        w('')
        # enum predicates
        used_enums = set()
        for ty in self.input_structs:
            for f in self.m.fields[ty]:
                if f.elem is not None and f.elem.kind == rp.Elem.SCALAR and f.elem.ty.category == VkType.ENUM \
                        and 'FlagBits' not in f.elem.ty.name and f.elem.ty.name not in UNCHECKED_ENUMS:
                    used_enums.add(f.elem.ty.name)
        for c in self.served:
            for f in self.m.fields[c]:
                if f.elem is not None and f.elem.kind == rp.Elem.SCALAR and f.elem.ty.category == VkType.ENUM \
                        and 'FlagBits' not in f.elem.ty.name and f.elem.ty.name not in UNCHECKED_ENUMS:
                    used_enums.add(f.elem.ty.name)
        used_enums.add('VkFormat')
        used_enums.add('VkObjectType')
        for name in sorted(used_enums):
            vals = self.values.get(name, {})
            rs = ranges(vals.values())
            pats = ' | '.join(('%d' % a) if a == b else '%d..=%d' % (a, b) for a, b in rs) or '_ if false'
            w('/// Whether `v` is a `%s` value of core Vulkan 1.0-1.3.' % name)
            w('#[must_use]')
            w('pub fn e_%s(v: i32) -> bool { matches!(v, %s) }' % (name, pats))
            w('')
        # structs
        for ty in self.input_structs:
            if ty.category == VkType.UNION:
                continue
            acc = lambda n: 'v.' + n
            body = []
            chain = self.m.chains.get(ty, [])
            if chain:
                arms = []
                for c in chain:
                    if self.admitted(c):
                        arms.append('%sNext::%s(x) => t_%s(r, x)?,' % (ty.name, c.name, c.name))
                arms.append('other => return Err(r.link("%s", ChainLink::structure_type(other))),' % ty.name)
                body.append('for link in v.p_next.iter_mut() { match link { %s } }' % ' '.join(arms))
            body.extend(self.field_checks(ty, self.m.fields[ty], acc, ty.name))
            w('fn t_%s(r: &mut dyn Resolve, v: &mut %s%s) -> Result<(), ExecError> {' % (ty.name, ty.name, self.lt(ty)))
            for b in body:
                w('    ' + b)
            w('    Ok(())')
            w('}')
            w('')
        # commands
        for c in self.served:
            acc = lambda n: 'a.' + n
            fields = self.m.fields[c]
            first = fields[0] if fields and fields[0].elem is not None and fields[0].elem.kind == rp.Elem.HANDLE else None
            body = self.field_checks(c, fields, acc, c.name, skip_out=True, skip_first=first)
            w('fn t_%s(r: &mut dyn Resolve, a: &mut %s%s) -> Result<(), ExecError> {' % (
                c.name[2:], self.r.command_args_name(c), self.lt(c)))
            for b in body:
                w('    ' + b)
            w('    Ok(())')
            w('}')
            w('')
        w('/// Translate every input of `command`: ids to host handles, every value checked.')
        w('///')
        w('/// # Errors')
        w('/// The first refusal, or `NotImplemented` for a command with no generated translation.')
        w('pub fn translate(r: &mut dyn Resolve, command: &mut Command<\'_>) -> Result<(), ExecError> {')
        w('    match command {')
        for c in self.served:
            w('        Command::%s(a) => t_%s(r, a),' % (c.name[2:], c.name[2:]))
        w('        other => Err(ExecError::NotImplemented { command: other.name() }),')
        w('    }')
        w('}')
        w('')
        # dispatchable
        w('/// The object `command` is dispatched on: its first parameter.')
        w('#[must_use]')
        w('pub fn dispatchable(command: &Command<\'_>) -> Option<(Kind, u64)> {')
        w('    match command {')
        for c in self.inscope:
            fields = self.m.fields[c]
            if fields and fields[0].elem is not None and fields[0].elem.kind == rp.Elem.HANDLE \
                    and fields[0].shape == rp.Field.PLAIN:
                w('        Command::%s(a) => Some((%s, a.%s.0)),' % (c.name[2:], self.kind(fields[0].elem.ty), fields[0].name))
        w('        _ => None,')
        w('    }')
        w('}')
        w('')
        w('/// The core version that brought `command` (`VK_MAKE_API_VERSION(0, 1, x, 0)`), 0 for')
        w('/// one outside core 1.0-1.3.')
        w('#[must_use]')
        w('pub fn min_api(command: &Command<\'_>) -> u32 {')
        w('    match command {')
        by_ver = {}
        for c in self.inscope:
            by_ver.setdefault(self.core[c.name], []).append(c)
        for ver in sorted(by_ver):
            w('        %s => %d,' % (' | '.join('Command::%s(_)' % c.name[2:] for c in by_ver[ver]),
                                     (ver[0] << 22) | (ver[1] << 12)))
        w('        _ => 0,')
        w('    }')
        w('}')
        w('')
        w('/// Whether `command` is served by translation and the host call alone')
        w('/// (`[generated]` in executor-classes.txt).')
        w('#[must_use]')
        w('pub fn is_pass_through(command: &Command<\'_>) -> bool {')
        gen = [c for c in self.inscope if c.name in self.classes['generated']]
        w('    matches!(command, %s)' % ' | '.join('Command::%s(_)' % c.name[2:] for c in gen))
        w('}')
        w('')
        w('/// Set the `VkResult` `command` returns; `false` for a command without one.')
        w('pub fn set_result(command: &mut Command<\'_>, ret: i32) -> bool {')
        w('    match command {')
        for c in self.served:
            if c.ret and c.ret.ty.base.name == 'VkResult':
                w('        Command::%s(a) => a.ret = ret,' % c.name[2:])
        w('        _ => return false,')
        w('    }')
        w('    true')
        w('}')
        w('')
        w('/// The `VkResult` `command` returns, for a command that returns one.')
        w('#[must_use]')
        w('pub fn result_of(command: &Command<\'_>) -> Option<i32> {')
        w('    match command {')
        for c in self.served:
            if c.ret and c.ret.ty.base.name == 'VkResult':
                w('        Command::%s(a) => Some(a.ret),' % c.name[2:])
        w('        _ => None,')
        w('    }')
        w('}')
        w('')
        w('/// Every handle `command` creates, in order, and their type: the ids the guest')
        w('/// chose before the host call, the host\'s handles after it.')
        w('pub fn output_handles<\'c>(command: &\'c mut Command<\'_>) -> Option<(Kind, Vec<&\'c mut u64>)> {')
        w('    match command {')
        for c in self.served:
            for f in self.m.fields[c]:
                if not self.m.is_out(f) or f.elem is None or f.elem.kind != rp.Elem.HANDLE:
                    continue
                if f.shape == rp.Field.PTR:
                    w('        Command::%s(a) => Some((%s, a.%s.iter_mut().map(|h| &mut h.0).collect())),' % (
                        c.name[2:], self.kind(f.elem.ty), f.name))
                elif f.shape == rp.Field.DYN:
                    w('        Command::%s(a) => Some((%s, a.%s.iter_mut().flatten().map(|h| &mut h.0).collect())),' % (
                        c.name[2:], self.kind(f.elem.ty), f.name))
        w('        _ => None,')
        w('    }')
        w('}')
        w('')
        w('/// Commands this file translates, by name, for the report and the tests.')
        w('pub const TRANSLATED: &[&str] = &[%s];' % ', '.join('"%s"' % c.name for c in self.served))
        w('')
        w('/// Commands served by translation and the host call alone.')
        w('pub const PASS_THROUGH: &[&str] = &[%s];' % ', '.join('"%s"' % c.name for c in gen))
        w('')
        return '\n'.join(out) + '\n'

    # ---- host side --------------------------------------------------------

    def ash_struct(self, ty):
        name = ty.name[2:]
        if ty.category == VkType.UNION:
            if name not in self.unions:
                sys.exit('venus-exec-gen: no ash union %s' % name)
            return name, False, self.unions[name]
        if name not in self.structs:
            sys.exit('venus-exec-gen: no ash struct %s' % name)
        lt, fields = self.structs[name]
        return name, lt, fields

    def conv_scalar(self, v, f_elem, aty):
        """`v` (a protocol scalar value) as ash type `aty`."""
        aty = aty.replace('crate::vk::', '')
        if aty in ASH_SCALARS:
            return v
        if aty == 'usize':
            return '%s as usize' % v
        if aty == 'isize':
            return '%s as isize' % v
        if aty == 'c_char':
            return '%s as c_char' % v
        if re.fullmatch(r'\w+', aty):
            if f_elem.kind == rp.Elem.HANDLE:
                return 'vk::%s::from_raw(%s.0)' % (aty, v.lstrip('*'))
            prim = self.m._primitive_of(f_elem.ty)
            if prim == 'int32_t' and 'Flags' in aty:
                return 'vk::%s::from_raw(%s as u32)' % (aty, v)
            return 'vk::%s::from_raw(%s)' % (aty, v)
        raise rp.Unsupported('scalar to %s' % aty)

    def pointee(self, aty):
        m = re.fullmatch(r'\*(const|mut) (.+)', aty)
        if not m:
            return None
        inner = m.group(2).replace("<'_>", '').replace("<'a>", '').replace('crate::vk::', '').strip()
        return inner

    def value_expr(self, owner, f, v, aty, pre):
        """The ash value of protocol field `f` (accessed as `v`) for ash type `aty`.
        `pre` collects statements that must run first."""
        shape, e = f.shape, f.elem
        what = '"%s.%s"' % (owner.name, f.c_name)
        if shape == rp.Field.NULL_ONLY:
            return 'core::ptr::null()'
        if shape == rp.Field.PLAIN:
            if e.kind == rp.Elem.STRUCT:
                return 'c_%s(a, &%s)?' % (e.ty.name, v)
            if e.kind == rp.Elem.UNION:
                return 'u_%s(a, &%s)?' % (e.ty.name, v)
            return self.conv_scalar(v, e, aty)
        if shape == rp.Field.STATIC:
            if aty.startswith('*const ['):
                return '&%s as *const _' % v
            inner = re.fullmatch(r'\[(.+); (\w+)\]', aty)
            if not inner:
                raise rp.Unsupported('%s: static to %s' % (what, aty))
            at = inner.group(1)
            if e.kind == rp.Elem.STRUCT:
                return '[%s]' % ', '.join('c_%s(a, &%s[%d])?' % (e.ty.name, v, i) for i in range(f.n))
            if at in ASH_SCALARS:
                return v
            if at == 'c_char':
                return '%s.map(|b| b as c_char)' % v
            return '%s.map(|x| %s)' % (v, self.conv_scalar('x', e, at))
        pt = self.pointee(aty)
        if pt is None:
            raise rp.Unsupported('%s: %s to %s' % (what, shape, aty))
        out = self.pointer_exprs(shape, e, v, pt, what)
        if aty.startswith('*mut'):
            out = out.replace('.cast_const()', '').replace('core::ptr::null()', 'core::ptr::null_mut()')
        return out

    def pointer_exprs(self, shape, e, v, pt, what):
        if shape == rp.Field.PTR:
            if e.kind == rp.Elem.STRUCT:
                conv = 'c_%s(a, x)?' % e.ty.name
            elif e.kind == rp.Elem.UNION:
                conv = 'u_%s(a, x)?' % e.ty.name
            else:
                conv = self.conv_scalar('*x', e, pt)
            return 'match &%s { Some(x) => { let c = %s; a.one(c).cast_const() } None => core::ptr::null() }' % (v, conv)
        if shape == rp.Field.DYN:
            if e.kind == rp.Elem.STRUCT:
                conv = 'c_%s(a, x)' % e.ty.name
                return 'match &%s { Some(xs) => { let c = xs.iter().map(|x| %s).collect::<Result<Vec<_>, _>>()?; a.slice(c).cast_const() } None => core::ptr::null() }' % (v, conv)
            if e.kind == rp.Elem.UNION:
                conv = 'u_%s(a, x)' % e.ty.name
                return 'match &%s { Some(xs) => { let c = xs.iter().map(|x| %s).collect::<Result<Vec<_>, _>>()?; a.slice(c).cast_const() } None => core::ptr::null() }' % (v, conv)
            if pt == 'c_void':
                raise rp.Unsupported('%s: array to void pointer' % what)
            conv = self.conv_scalar('*x', e, pt)
            return 'match &%s { Some(xs) => { let c: Vec<_> = xs.iter().map(|x| %s).collect(); a.slice(c).cast_const() } None => core::ptr::null() }' % (v, conv)
        if shape == rp.Field.BLOB:
            return '%s.map_or(core::ptr::null(), |b| b.as_ptr()) as _' % v
        if shape == rp.Field.STRING:
            return '%s.map_or(core::ptr::null(), |s| a.cstr(s))' % v
        if shape == rp.Field.STRING_ARRAY:
            return 'match &%s { Some(xs) => { let c: Vec<*const c_char> = xs.iter().map(|s| a.cstr(s)).collect(); a.slice(c).cast_const() } None => core::ptr::null() }' % v
        raise rp.Unsupported('%s: shape %s' % (what, shape))

    def host_counts(self, owner, fields, acc):
        out = []
        for f in fields:
            if self.m.is_out(f):
                continue
            if f.count is not None and f.shape in (rp.Field.DYN, rp.Field.BLOB, rp.Field.STRING_ARRAY):
                count = self.r.count_expr(f.count, acc)
                out.append('if let Some(x) = &%s { same(x.len(), %s, "%s.%s")?; }' % (
                    acc(f.name), count, owner.name, f.c_name))
        return out

    def emit_host(self):
        out = []
        w = out.append
        w('// @generated by scripts/venus-exec-gen.py from the generated Venus protocol,')
        w('// vk.xml and the ash 0.38 sources. DO NOT EDIT: regenerate with')
        w('// `python scripts/venus-exec-gen.py`.')
        w('')
        w('//! Every command the executor serves by translation, as a call into the')
        w('//! host driver through `ash`\'s function table (EPIC 20 stage 5b.2).')
        w('//!')
        w('//! Each structure of the command is rebuilt as its `ash` twin inside an')
        w('//! [`Arena`], every pointer pointing at an arena-owned copy that lives until')
        w('//! the call has returned, pNext links chained in the guest\'s order. The one')
        w('//! `unsafe` operation is the call, in [`call`]: see its safety contract.')
        w('')
        w('#![allow(clippy::all, clippy::pedantic, non_snake_case, dead_code, unused_imports, unused_variables, unused_mut, unreachable_patterns, unused_unsafe)]')
        w('')
        w('use core::ffi::{c_char, c_void};')
        w('')
        w('use ash::vk::{self, Handle};')
        w('')
        w('use crate::venus::protocol::*;')
        w('')
        w('use super::arena::{bounded, bounded_bytes, same, Arena, CallError};')
        w('')
        # struct converters
        for ty in self.input_structs:
            aname, lt, afields = self.ash_struct(ty)
            if ty.category == VkType.UNION:
                w('fn u_%s(a: &mut Arena, v: &%s%s) -> Result<vk::%s, CallError> {' % (ty.name, ty.name, self.lt(ty), aname))
                w('    Ok(match v {')
                amap = {norm(n): (n, t) for n, t in afields}
                for fld, _tags in ty.attrs['rust_cases']:
                    variant = self.r.union_variant(fld)
                    an, at = amap[norm(fld.c_name)]
                    val = self.value_expr(ty, fld, 'x', at, None) if fld.shape != rp.Field.PLAIN or fld.elem.kind != rp.Elem.SCALAR else self.conv_scalar('*x', fld.elem, at)
                    if fld.shape == rp.Field.STATIC:
                        val = val.replace('x.map', '(*x).map') if '.map' in val else '*x'
                    if fld.shape == rp.Field.PLAIN and fld.elem.kind == rp.Elem.STRUCT:
                        val = 'c_%s(a, x)?' % fld.elem.ty.name
                    w('        %s::%s(x) => vk::%s { %s: %s },' % (ty.name, variant, aname, an, val))
                w('    })')
                w('}')
                w('')
                continue
            amap = {norm(n if n != 'ty' else 'type'): (n, t) for n, t in afields}
            body = self.host_counts(ty, self.m.fields[ty], lambda n: 'v.' + n)
            inits = []
            for f in self.m.fields[ty]:
                if f.shape == rp.Field.NULL_ONLY:
                    continue
                key = norm(f.name.rstrip('_'))
                if key not in amap:
                    sys.exit('venus-exec-gen: %s.%s has no ash twin' % (ty.name, f.c_name))
                an, at = amap[key]
                inits.append('%s: %s,' % (an, self.value_expr(ty, f, 'v.' + f.name, at, None)))
            chain = self.m.chains.get(ty, [])
            if chain:
                body.append('let next = ch_%s(a, &v.p_next)?;' % ty.name)
                inits.insert(0, 'p_next: next as _,')
            ret = "vk::%s<'static>" % aname if lt else 'vk::%s' % aname
            w('fn c_%s(a: &mut Arena, v: &%s%s) -> Result<%s, CallError> {' % (ty.name, ty.name, self.lt(ty), ret))
            for b in body:
                w('    ' + b)
            w('    Ok(vk::%s { %s ..Default::default() })' % (aname, ' '.join(inits)))
            w('}')
            w('')
            if chain and not any(self.admitted(c) for c in chain):
                nlt = "<'_>" if self.m.chain_lifetime(ty) else ''
                w('fn ch_%s(_a: &mut Arena, links: &[%sNext%s]) -> Result<*mut c_void, CallError> {' % (ty.name, ty.name, nlt))
                w('    match links.first() {')
                w('        Some(other) => Err(CallError::Link { parent: "%s", stype: ChainLink::structure_type(other) }),' % ty.name)
                w('        None => Ok(core::ptr::null_mut()),')
                w('    }')
                w('}')
                w('')
            elif chain:
                nlt = "<'_>" if self.m.chain_lifetime(ty) else ''
                w('fn ch_%s(a: &mut Arena, links: &[%sNext%s]) -> Result<*mut c_void, CallError> {' % (ty.name, ty.name, nlt))
                w('    let mut next: *mut c_void = core::ptr::null_mut();')
                w('    for link in links.iter().rev() {')
                w('        next = match link {')
                for c in chain:
                    if self.admitted(c):
                        w('            %sNext::%s(x) => { let mut c = c_%s(a, x)?; c.p_next = next as _; a.one(c).cast() }' % (ty.name, c.name, c.name))
                w('            other => return Err(CallError::Link { parent: "%s", stype: ChainLink::structure_type(other) }),' % ty.name)
                w('        };')
                w('    }')
                w('    Ok(next)')
                w('}')
                w('')
        # output struct helpers
        out_structs = []
        for c in self.served:
            for f in self.m.fields[c]:
                if self.m.is_out(f) and f.shape == rp.Field.PTR and f.elem.kind == rp.Elem.STRUCT:
                    out_structs.append(f.elem.ty)
        done_out = set()

        def emit_out(ty):
            if ty in done_out:
                return
            done_out.add(ty)
            aname, lt, afields = self.ash_struct(ty)
            amap = {norm(n if n != 'ty' else 'type'): (n, t) for n, t in afields}
            for f in self.m.fields[ty]:
                if f.elem is not None and f.elem.kind == rp.Elem.STRUCT and f.shape == rp.Field.PLAIN:
                    emit_out(f.elem.ty)
            for c in self.m.chains.get(ty, []):
                emit_out(c)
            ret = "vk::%s<'static>" % aname if lt else 'vk::%s' % aname
            w('fn b_%s(src: &%s, dst: &mut %s%s) {' % (ty.name, ret, ty.name, self.lt(ty)))
            for f in self.m.fields[ty]:
                an, at = amap[norm(f.name.rstrip('_'))]
                if f.shape == rp.Field.PLAIN and f.elem.kind == rp.Elem.SCALAR:
                    if at in ASH_SCALARS:
                        w('    dst.%s = src.%s;' % (f.name, an))
                    elif at == 'usize':
                        w('    dst.%s = src.%s as u64;' % (f.name, an))
                    else:
                        prim = self.m._primitive_of(f.elem.ty)
                        cast = ' as i32' if prim == 'int32_t' and 'Flags' in at else ''
                        w('    dst.%s = src.%s.as_raw()%s;' % (f.name, an, cast))
                elif f.shape == rp.Field.PLAIN and f.elem.kind == rp.Elem.STRUCT:
                    w('    b_%s(&src.%s, &mut dst.%s);' % (f.elem.ty.name, an, f.name))
                elif f.shape == rp.Field.STATIC and f.elem.kind == rp.Elem.SCALAR:
                    if at.startswith('[c_char'):
                        w('    dst.%s = src.%s.map(|c| c as u8);' % (f.name, an))
                    else:
                        w('    dst.%s = src.%s;' % (f.name, an))
                else:
                    raise rp.Unsupported('output %s.%s' % (ty.name, f.c_name))
            w('}')
            w('')
        for ty in out_structs:
            emit_out(ty)
        # command calls
        for c in self.served:
            self.emit_call(w, c)
        w('/// Call the host driver for `command`, whose inputs [the executor translated]')
        w('/// (`crate::venus::executor::generated::translate`), and write its outputs back.')
        w('///')
        w('/// # Errors')
        w('/// A command with no generated call, a chained structure with no twin here, or')
        w('/// an array that disagrees with its count: nothing reached the driver.')
        w('///')
        w('/// # Safety')
        w('/// `device` is a live device of this host, and every handle in `command` is one')
        w('/// of its live objects of the type the field names, or null where Vulkan allows')
        w('/// null there; every enum and flag word is a core Vulkan 1.3 value; every count')
        w('/// agrees with its array; the command is core in `device`\'s version; and the')
        w('/// caller holds the external synchronisation Vulkan requires for the objects it')
        w('/// names (the executor\'s context lock). Translation establishes all but the')
        w('/// last two, which the executor does.')
        w('pub unsafe fn call(device: &ash::Device, command: &mut Command<\'_>) -> Result<(), CallError> {')
        w('    let mut arena = Arena::default();')
        w('    // SAFETY: this function\'s own contract, passed on to the one arm taken.')
        w('    unsafe {')
        w('        match command {')
        for c in self.served:
            w('            Command::%s(args) => call_%s(device, &mut arena, args),' % (c.name[2:], c.name[2:]))
        w('            other => Err(CallError::NoCall(other.name())),')
        w('        }')
        w('    }')
        w('}')
        return '\n'.join(out) + '\n'

    def emit_call(self, w, c):
        fields = self.m.fields[c]
        if c.name not in self.tables:
            sys.exit('venus-exec-gen: %s has no ash function table entry' % c.name)
        table, fn = self.tables[c.name]
        params, ret = self.pfns[c.name]
        if len(params) != len(fields):
            sys.exit('venus-exec-gen: %s: %d ash parameters, %d protocol fields' % (c.name, len(params), len(fields)))
        acc = lambda n: 'args.' + n
        pre = self.host_counts(c, fields, acc)
        first = []
        post = []
        exprs = []
        for i, (f, (pname, pty)) in enumerate(zip(fields, params)):
            v = 'args.' + f.name
            pty_clean = pty.replace('crate::vk::', '')
            if i == 0 and pty_clean == 'Device':
                exprs.append('device.handle()')
                continue
            if self.m.is_out(f):
                pt = self.pointee(pty)
                if f.shape == rp.Field.PTR and f.elem.kind == rp.Elem.HANDLE:
                    pre.append('let mut o_%s = vk::%s::null();' % (f.name, pt))
                    exprs.append('&mut o_%s' % f.name)
                    post.append('%s = Some(%s(o_%s.as_raw()));' % (v, f.elem.ty.name, f.name))
                elif f.shape == rp.Field.DYN and f.elem.kind == rp.Elem.HANDLE:
                    count = self.r.count_expr(f.count, acc)
                    pre.append('let n_%s = bounded(%s, "%s.%s")?;' % (f.name, count, c.name, f.c_name))
                    pre.append('let mut o_%s = vec![vk::%s::null(); n_%s];' % (f.name, pt, f.name))
                    exprs.append('o_%s.as_mut_ptr()' % f.name)
                    post.append('%s = Some(o_%s.iter().map(|h| %s(h.as_raw())).collect());' % (v, f.name, f.elem.ty.name))
                elif f.shape == rp.Field.PTR and f.elem.kind == rp.Elem.SCALAR:
                    # In/out (`pDataSize`): read before the call, written
                    # back before any output whose length it is.
                    if pt == 'usize':
                        pre.append('let mut o_%s: usize = %s.map_or(0, |x| x as usize);' % (f.name, v))
                        first.append('%s = Some(o_%s as u64);' % (v, f.name))
                    elif pt in ASH_SCALARS:
                        pre.append('let mut o_%s = %s.unwrap_or_default();' % (f.name, v))
                        first.append('%s = Some(o_%s);' % (v, f.name))
                    else:
                        prim = self.m._primitive_of(f.elem.ty)
                        cast = ' as i32' if prim == 'int32_t' and 'Flags' in pt else ''
                        pre.append('let mut o_%s = vk::%s::default();' % (f.name, pt))
                        first.append('%s = Some(o_%s.as_raw()%s);' % (v, f.name, cast))
                    exprs.append('&mut o_%s' % f.name)
                elif f.shape == rp.Field.PTR and f.elem.kind == rp.Elem.STRUCT:
                    ty = f.elem.ty
                    aname, lt, _af = self.ash_struct(ty)
                    chain = self.m.chains.get(ty, [])
                    pre.append('let mut o_%s = vk::%s::default();' % (f.name, aname))
                    links = []
                    if chain:
                        pre.append('let mut l_%s: Vec<*mut c_void> = Vec::new();' % f.name)
                        arms = ' '.join(
                            '%sNext::%s(_) => { let mut c = vk::%s::default(); c.p_next = o_%s.p_next; a.one(c).cast() }'
                            % (ty.name, l.name, self.ash_struct(l)[0], f.name) for l in chain)
                        pre.append('if let Some(s) = &%s { for link in s.p_next.iter().rev() { '
                                   'let p: *mut c_void = match link { %s }; o_%s.p_next = p; l_%s.insert(0, p); } }'
                                   % (v, arms, f.name, f.name))
                        back = ''.join('%sNext::%s(d) => {\n// SAFETY: `p` is the arena copy of this link, which the driver\n// wrote and nothing else holds.\nlet src: &vk::%s = unsafe { &*p.cast() };\nb_%s(src, d);\n}\n'
                                       % (ty.name, l.name, self.ash_struct(l)[0], l.name) for l in chain)
                        post.append('if let Some(s) = %s.as_mut() {\nfor (link, p) in s.p_next.iter_mut().zip(&l_%s) {\n'
                                    'match link {\n%s}\n}\n}' % (v, f.name, back))
                    exprs.append('&mut o_%s' % f.name)
                    post.append('if let Some(d) = %s.as_mut() { b_%s(&o_%s, d); }' % (v, ty.name, f.name))
                elif f.shape == rp.Field.BLOB_OUT:
                    count = self.r.count_expr(f.count, acc)
                    pre.append('let n_%s = bounded_bytes(%s, "%s.%s")?;' % (f.name, count, c.name, f.c_name))
                    pre.append('let mut o_%s: Option<Vec<u8>> = %s.as_ref().map(|_| vec![0u8; n_%s]);' % (f.name, v, f.name))
                    exprs.append('o_%s.as_mut().map_or(core::ptr::null_mut(), |b| b.as_mut_ptr().cast())' % f.name)
                    # a count that the call itself writes (the pDataSize idiom)
                    cpath = f.count.path if f.count.kind == rp.Count.PATH else None
                    if cpath and len(cpath) == 1 and cpath[0].ty.is_pointer():
                        post.append('if let Some(b) = o_%s.as_mut() { b.truncate(usize::try_from(%s.unwrap_or(0)).unwrap_or(0)); }' % (
                            f.name, 'args.' + rp.snake(cpath[0].name)))
                    post.append('if o_%s.is_some() { %s = o_%s.take(); }' % (f.name, v, f.name))
                else:
                    sys.exit('venus-exec-gen: %s.%s: output shape %s' % (c.name, f.c_name, f.shape))
                continue
            exprs.append(self.value_expr(c, f, v, pty, pre))
        # outputs written after scalar in/out outputs: order posts so a size is written first
        for i, e in enumerate(exprs):
            if e.startswith('&mut o_') or e.startswith('o_') or e == 'device.handle()' or re.fullmatch(r'args\.\w+', e):
                continue
            pre.append('let p%d = %s;' % (i, e))
            exprs[i] = 'p%d' % i
        call = '(device.%s().%s)(%s)' % (table, fn, ', '.join(exprs))
        w('unsafe fn call_%s(device: &ash::Device, a: &mut Arena, args: &mut %s%s) -> Result<(), CallError> {' % (
            c.name[2:], self.r.command_args_name(c), self.lt(c)))
        for p in pre:
            w('    ' + p)
        # SAFETY is the caller's (see `call`).
        w('    // SAFETY: the contract of `call`, which this is one arm of; every pointer')
        w('    // argument points into `a` or a local of this function, both of which')
        w('    // outlive the call.')
        if ret == 'Result':
            w('    let r = unsafe { %s };' % call)
            w('    args.ret = r.as_raw();')
        elif ret in ('u64', 'DeviceAddress'):
            w('    let r = unsafe { %s };' % call)
            w('    args.ret = r;')
        elif ret is None:
            w('    unsafe { %s };' % call)
        else:
            sys.exit('venus-exec-gen: %s returns %s' % (c.name, ret))
        for p in first + post:
            w('    ' + p)
        w('    Ok(())')
        w('}')
        w('')


def edition():
    return ashgen.edition()


def formatted(text, name):
    import subprocess
    import tempfile
    with tempfile.TemporaryDirectory() as tmp:
        path = Path(tmp) / name
        path.write_text(text, encoding='utf-8', newline='\n')
        subprocess.run(['rustfmt', '--edition', edition(), str(path)], check=True)
        return path.read_text(encoding='utf-8')


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument('--check', action='store_true')
    ap.add_argument('--report', action='store_true')
    args = ap.parse_args()
    g = Gen()
    outputs = {
        OUT_EXEC: formatted(g.emit_exec(), 'generated.rs'),
        OUT_HOST: formatted(g.emit_host(), 'calls.rs'),
    }
    if args.report:
        for sec in ('bespoke', 'handwritten', 'generated', 'refused'):
            print('%-12s %3d' % (sec, len(g.classes[sec])))
        print('translated structures: %d' % len(g.input_structs))
    if args.check:
        stale = [str(p) for p, text in outputs.items()
                 if not p.exists() or p.read_text(encoding='utf-8') != text]
        if stale:
            print('stale; regenerate: %s' % ', '.join(stale), file=sys.stderr)
            return 1
        return 0
    for p, text in outputs.items():
        p.write_text(text, encoding='utf-8', newline='\n')
        print('wrote %s' % p)
    return 0


if __name__ == '__main__':
    sys.exit(main())
