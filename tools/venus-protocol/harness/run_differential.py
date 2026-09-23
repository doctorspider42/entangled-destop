#!/usr/bin/env python3

# Copyright 2026 The Entangled Desktop authors
# SPDX-License-Identifier: MIT

"""Differential check of the generated Rust against the generated C.

The vendored generator emits two C implementations of the protocol besides
our Rust: the *driver* (what Mesa compiles into the guest) and the
*renderer* (what virglrenderer compiles into the host). This harness makes
them the oracle:

  phase 0 (C)     dump the renderer's extension table and the capset mask
                  virglrenderer builds from it (`vn_info_extension_mask_init`
                  plus the sentinel), for the Rust to compare with info.rs.
  phase 1 (C)     for every generated command and a spread of seeds, build
                  randomised parameters — present and null pointers, empty
                  and non-empty arrays, strings, unions under every tag their
                  selector allows, strided and two-level arrays, pNext chains
                  drawn from the whitelist — and encode them with the C
                  driver encoder. Decode each with the C renderer decoder,
                  fill every output with random values (blobs included), and
                  encode the reply with the C renderer. One seed in eight
                  instead poisons a chain: the command is encoded with a link
                  whose sType is then rewritten, in the bytes, to one its
                  parent does not admit (a real structure of another chain,
                  or no structure at all); the C renderer must refuse it.
  phase 2 (Rust)  decode each command with the generated Rust, re-encode it
                  as the driver would, and require the bytes to be identical;
                  decode the C reply into the command, re-encode it, and
                  require identical bytes again. Poisoned commands must be
                  refused, as an unknown pNext sType.
  phase 3 (C)     decode every Rust-encoded reply with the C driver's reply
                  decoder, rebuilt from the same seed, and require it to
                  consume exactly the reply without going fatal.

It ends with a coverage table: for every construct the generator translates
(unions, strided arrays, blobs in replies, ...), how many commands reach it
and how many of their cases round-tripped.

Needs Python with Mako (for the vendored generator), a C11 compiler (`cc`,
`gcc` or `clang`; tested with gcc under WSL and on ubuntu-latest) and cargo.
All scratch output goes to a temporary directory outside the repository
unless --workdir says otherwise.

    python tools/venus-protocol/harness/run_differential.py [--seeds N]
"""

import argparse
import os
import re
import shutil
import subprocess
import sys
import tempfile
from pathlib import Path

HERE = Path(__file__).resolve().parent
TOOLS = HERE.parent
ROOT = TOOLS.parent.parent
sys.path.insert(0, str(TOOLS))

import rust_protocol as rp  # noqa: E402
from rust_protocol import Count, Elem, Field, INVALID, PARTIAL, VALID  # noqa: E402
from vkxml import VkType  # noqa: E402

MODE_IN, MODE_SKEL, MODE_OUT = 0, 1, 2
UNKNOWN_STYPE = 0x7fff0001


class CGen:
    """C fillers and per-command drivers, from the same Model as the Rust."""

    def __init__(self, model):
        self.m = model
        values = model.reg.type_table['VkStructureType'].enums.values
        self.stype_value = {ty.s_type: int(values[ty.s_type], 0)
                            for ty in model.structs if ty.s_type}
        links = []
        for ty in model.structs:
            for c in model.chains.get(ty, []):
                if c.s_type not in links:
                    links.append(c.s_type)
        self.chain_links = links

    # ---- helpers ----------------------------------------------------------

    def rand_scalar(self, elem, dst):
        prim = elem.prim
        ty = elem.ty
        if ty.category == VkType.ENUM:
            return '%s = (%s)hrng_below(r, 4);' % (dst, ty.name)
        if prim == 'float':
            return '%s = h_float(r);' % dst
        if prim == 'double':
            return '%s = (double)h_float(r);' % dst
        return '%s = (%s)hrng_next(r);' % (dst, ty.name)

    def rand_elem(self, elem, dst, mode):
        if elem.kind == Elem.SCALAR:
            return self.rand_scalar(elem, dst)
        if elem.kind == Elem.HANDLE:
            return '%s = (%s)(uintptr_t)h_id(r);' % (dst, elem.ty.name)
        if elem.kind == Elem.UNION:
            if elem.ty.is_valid_union():
                raise rp.Unsupported('harness: %s without its selector' % elem.ty.name)
            return 'fill_%s(&%s, r, %s);' % (elem.ty.name, dst, mode)
        return 'fill_%s(&%s, r, %s);' % (elem.ty.name, dst, mode)

    def len_targets(self, ty):
        """Members named as another member's dynamic-array count."""
        out = set()
        for f in self.m.fields[ty]:
            if f.count is not None and f.count.kind in (Count.PATH, Count.EXPR) \
                    and len(f.count.path) == 1:
                out.add(f.count.path[0].name)
        return out

    def selectors(self, ty):
        """Selector member name -> the union field it selects for."""
        return {f.selector.name: f for f in self.m.fields[ty] if f.selector is not None}

    @staticmethod
    def c_expr(count, prefix):
        """The C expression a Count reads as, with members under `prefix`."""
        if count.kind == Count.CONST:
            return str(count.value)
        name = count.path[0].name
        if count.kind == Count.EXPR:
            return '(%s)' % re.sub(r'\b%s\b' % name, prefix + name, count.text)
        if len(count.path) == 1:
            if count.path[0].ty.is_pointer():
                return '(%s%s ? *%s%s : 0)' % (prefix, name, prefix, name)
            return '%s%s' % (prefix, name)
        return '(%s%s ? %s%s->%s : 0)' % (prefix, name, prefix, name, count.path[1].name)

    def claim(self, count, claimed, prefix):
        """C statements leaving the array length in `n`: a count member not
        yet set gets a small random value first, which every later array it
        also counts then shares."""
        if count.kind == Count.CONST:
            return 'uint32_t n = %d;' % count.value
        head = count.path[0]
        if count.kind == Count.EXPR:
            out = ''
            if head.name not in claimed:
                claimed.add(head.name)
                out = '%s%s = (%s)hrng_below(r, 70); ' % (prefix, head.name, head.ty.base.name)
            if head.ty.base.category == VkType.ENUM:
                # the arithmetic is unsigned only if the enum is (GCC/Clang)
                out += '_Static_assert((%s)0 - 1 > 0, "enum %s is unsigned"); ' % (
                    head.ty.base.name, head.ty.base.name)
            return out + 'uint32_t n = (uint32_t)%s;' % self.c_expr(count, prefix)
        if len(count.path) == 2:
            # `pAllocateInfo->descriptorSetCount`: the member is a plain
            # field of a structure filled at random, so give it a small value
            # here, unless it already counts an array of its own.
            member = count.path[1]
            key = '%s->%s' % (head.name, member.name)
            if key in claimed or member.name in self.len_targets(head.ty.base):
                return 'uint32_t n = (uint32_t)%s;' % self.c_expr(count, prefix)
            claimed.add(key)
            return ('uint32_t n = 0; if (%s%s) { n = hrng_below(r, 4); ((%s *)%s%s)->%s = n; }' % (
                prefix, head.name, head.ty.base.name, prefix, head.name, member.name))
        if head.ty.is_pointer():
            return 'uint32_t n = (uint32_t)%s;' % self.c_expr(count, prefix)
        cnt = '%s%s' % (prefix, head.name)
        if head.name in claimed:
            return 'uint32_t n = (uint32_t)%s;' % cnt
        claimed.add(head.name)
        return 'uint32_t n = hrng_below(r, 4); %s = (%s)n;' % (cnt, head.ty.base.name)

    def inner_c(self, f, prefix):
        if f.inner.kind == Count.CONST:
            return str(f.inner.value)
        outer, member = f.inner.path
        return '(%s%s ? %s%s[i].%s : 0)' % (prefix, outer.name, prefix, outer.name, member.name)

    # ---- unions --------------------------------------------------------------

    def union_fill(self, ty):
        name = ty.name
        lines = []
        cases = ty.attrs['rust_cases']
        if ty.is_valid_union():
            lines.append('static void fill_%s_tag(%s *val, struct hrng *r, int mode, int64_t tag)' % (name, name))
            lines.append('{')
            lines.append('    (void)val; (void)r; (void)mode;')
            lines.append('    switch (tag) {')
            for f, tags in cases:
                for t in tags:
                    lines.append('    case %s:' % t[0])
                lines.append('        { %s } break;' % self.fill_in(ty, f, 'val->%s' % f.c_name, VALID,
                                                                    set(), set(), 'mode'))
            lines.append('    default: break;')
            lines.append('    }')
            lines.append('}')
        else:
            # Mesa's encoder only ever sends the default member.
            f = ty.attrs['rust_default']
            lines.append('static void fill_%s(%s *val, struct hrng *r, int mode)' % (name, name))
            lines.append('{')
            lines.append('    (void)val; (void)r; (void)mode;')
            lines.append('    %s' % self.fill_in(ty, f, 'val->%s' % f.c_name, VALID, set(), set(), 'mode'))
            lines.append('}')
        return '\n'.join(lines)

    def selector_pick(self, ty, f, dst):
        """A selector gets one of the tags its union answers to."""
        u = self.selectors(ty)[f.c_name]
        tags = [t[0] for _f, ts in u.elem.ty.attrs['rust_cases'] for t in ts]
        return '{ static const int64_t tags[] = { %s }; %s = (%s)tags[hrng_below(r, %d)]; }' % (
            ', '.join(tags), dst, f.var.ty.base.name, len(tags))

    # ---- per-structure fillers ----------------------------------------------

    def struct_fill(self, ty):
        name = ty.name
        chain = self.m.chains[ty]
        lines = []
        body = []
        claimed = (set(), set())    # count members set so far, IN and SKEL
        targets = self.len_targets(ty)
        has_partial = self.m.has_partial(ty)
        for f in self.m.fields[ty]:
            dst = 'val->%s' % f.c_name
            vpart = self.m.validity(ty, f, False) if has_partial else VALID
            body.append(self.field_fill(ty, f, dst, vpart, claimed, targets))
        lines.append('static void fill_%s_body(%s *val, struct hrng *r, int mode)' % (name, name))
        lines.append('{')
        lines.append('    (void)val; (void)r; (void)mode;')
        lines.extend('    ' + b for b in body if b)
        lines.append('}')
        if ty.s_type:
            lines.append('static const void *chain_%s(struct hrng *r, int mode)' % name)
            lines.append('{')
            lines.append('    VkBaseOutStructure *head = NULL, *tail = NULL;')
            lines.append('    (void)r; (void)mode;')
            n = len(chain)
            if n:
                bad = self.bad_stypes(ty)
                lines.append('    int order[%d];' % n)
                lines.append('    for (int i = 0; i < %d; i++) order[i] = i;' % n)
                lines.append('    for (int i = %d - 1; i > 0; i--) { int j = (int)hrng_below(r, (uint32_t)i + 1); int t = order[i]; order[i] = order[j]; order[j] = t; }' % n)
                lines.append('    int take = (int)hrng_below(r, %d);' % (min(n, 4) + 1))
                lines.append('    int poison = r->poison && !r->poison_stype;')
                lines.append('    if (poison && !take) take = 1;')
                lines.append('    for (int k = 0; k < take; k++) {')
                lines.append('        VkBaseOutStructure *link = NULL;')
                lines.append('        switch (order[k]) {')
                for i, c in enumerate(chain):
                    lines.append('        case %d: { %s *t = h_calloc(1, sizeof(*t)); t->sType = %s; fill_%s_body(t, r, mode); link = (VkBaseOutStructure *)t; break; }' % (i, c.name, c.s_type, c.name))
                lines.append('        }')
                lines.append('        if (poison && k == 0) {')
                lines.append('            static const int32_t bad[] = { %s };' % ', '.join(str(b) for b in bad))
                lines.append('            r->poison_stype = (int32_t)link->sType;')
                lines.append('            r->poison_with = bad[hrng_below(r, %d)];' % len(bad))
                lines.append('        }')
                lines.append('        if (tail) tail->pNext = link; else head = link;')
                lines.append('        tail = link;')
                lines.append('    }')
            lines.append('    return head;')
            lines.append('}')
            lines.append('static void walk_%s(const void *chain, struct hrng *r)' % name)
            lines.append('{')
            lines.append('    (void)r;')
            lines.append('    for (VkBaseOutStructure *p = (VkBaseOutStructure *)chain; p; p = p->pNext) {')
            lines.append('        switch ((int32_t)p->sType) {')
            for c in chain:
                lines.append('        case %s: fill_%s_body((%s *)p, r, %d); break;' % (c.s_type, c.name, c.name, MODE_OUT))
            lines.append('        default: break;')
            lines.append('        }')
            lines.append('    }')
            lines.append('}')
            lines.append('static void fill_%s(%s *val, struct hrng *r, int mode)' % (name, name))
            lines.append('{')
            lines.append('    if (mode == %d) { walk_%s(val->pNext, r); } else { val->sType = %s; val->pNext = (void *)chain_%s(r, mode); }' % (MODE_OUT, name, ty.s_type, name))
            lines.append('    fill_%s_body(val, r, mode);' % name)
            lines.append('}')
        else:
            lines.append('static void fill_%s(%s *val, struct hrng *r, int mode)' % (name, name))
            lines.append('{')
            lines.append('    fill_%s_body(val, r, mode);' % name)
            lines.append('}')
        return '\n'.join(lines)

    def bad_stypes(self, ty):
        """sTypes a poisoned link of `ty`'s chain is rewritten to: one no
        structure has, and real ones that other chains admit and this one
        does not."""
        mine = {c.s_type for c in self.m.chains[ty]}
        mine.add(ty.s_type)
        others = [self.stype_value[s] for s in self.chain_links if s not in mine][:3]
        return [UNKNOWN_STYPE] + others

    def field_fill(self, ty, f, dst, vpart, claimed, targets):
        """C statements filling field `f`, switching on `mode` at run time."""
        in_code = self.fill_in(ty, f, dst, VALID, claimed[0], targets, MODE_IN)
        skel_code = self.fill_in(ty, f, dst, vpart, claimed[1], targets, MODE_SKEL)
        out_code = self.fill_out(ty, f, dst, targets)
        return 'if (mode == %d) { %s } else if (mode == %d) { %s } else { %s }' % (
            MODE_IN, in_code, MODE_SKEL, skel_code, out_code)

    def fill_in(self, ty, f, dst, validity, claimed, targets, mode):
        """IN (everything) or SKEL (only what the partial encoding carries).
        `mode` is a C expression for the nested fillers' mode."""
        sh = f.shape
        prefix = 'val->'
        if sh == Field.NULL_ONLY:
            return '%s = NULL;' % dst
        if validity == INVALID and sh in (Field.PLAIN, Field.STATIC):
            return ''
        if sh == Field.PLAIN:
            if f.c_name in self.selectors(ty):
                return self.selector_pick(ty, f, dst)
            if f.selector is not None:
                return 'fill_%s_tag(&%s, r, %s, (int64_t)%s%s);' % (
                    f.elem.ty.name, dst, mode, prefix, f.selector.name)
            return self.rand_elem(f.elem, dst, mode)
        if sh == Field.STATIC:
            if f.elem.prim == 'char':
                return 'h_chars(r, %s, %d);' % (dst, f.n)
            return 'for (uint32_t i = 0; i < %d; i++) { %s }' % (
                f.n, self.rand_elem(f.elem, '%s[i]' % dst, mode))
        if sh == Field.PTR:
            if validity == INVALID:
                inner = ''
            elif f.elem.kind in (Elem.STRUCT, Elem.UNION):
                inner = 'fill_%s(t, r, %s);' % (f.elem.ty.name, mode)
            else:
                inner = self.rand_elem(f.elem, '*t', mode)
            make = '{ %s *t = h_calloc(1, sizeof(*t)); %s %s = (void *)t; }' % (
                f.elem.ty.name, inner, dst)
            if f.optional:
                return 'if (hrng_below(r, 3)) %s else %s = NULL;' % (make, dst)
            return make
        if sh == Field.STRING:
            if f.optional:
                return '%s = hrng_below(r, 3) ? h_string(r) : NULL;' % dst
            return '%s = h_string(r);' % dst
        claim = self.claim(f.count, claimed, prefix)
        null_ok = 'n == 0 || %s' % ('1' if f.optional else '0')
        if sh == Field.STRING_ARRAY:
            make = '{ const char **a = h_calloc(n, sizeof(*a)); for (uint32_t i = 0; i < n; i++) a[i] = h_string(r); %s = a; }' % dst
        elif sh in (Field.BLOB, Field.BLOB_OUT):
            fill = '' if validity == INVALID else 'for (uint32_t i = 0; i < n; i++) a[i] = (uint8_t)hrng_next(r);'
            make = '{ uint8_t *a = h_calloc(n, 1); %s %s = (void *)a; }' % (fill, dst)
        elif sh == Field.NESTED:
            make = self.nested_make(f, dst, prefix, mode)
        else:
            if validity == INVALID:
                each = ''
            else:
                each = 'for (uint32_t i = 0; i < n; i++) { %s }' % self.rand_elem(f.elem, 'a[i]', mode)
            make = '{ %s *a = h_calloc(n, sizeof(*a)); %s %s = (void *)a; }' % (f.elem.ty.name, each, dst)
        return '{ %s if ((%s) && hrng_below(r, 2)) %s = NULL; else %s }' % (claim, null_ok, dst, make)

    def nested_make(self, f, dst, prefix, mode):
        t = f.elem.ty.name
        return ('{ %s **a = h_calloc(n, sizeof(*a)); for (uint32_t i = 0; i < n; i++) { '
                'uint32_t m = (uint32_t)%s; %s *b = h_calloc(m, sizeof(*b)); '
                'for (uint32_t j = 0; j < m; j++) { %s } a[i] = b; } %s = (void *)a; }' % (
                    t, self.inner_c(f, prefix), t, self.rand_elem(f.elem, 'b[j]', mode), dst))

    def fill_out(self, ty, f, dst, targets):
        """OUT: the values a reply carries; shape (pointers, counts) untouched."""
        sh = f.shape
        if f.c_name in targets or f.c_name in self.selectors(ty):
            return ''
        if sh == Field.PLAIN:
            if f.selector is not None:
                return 'fill_%s_tag(&%s, r, %d, (int64_t)val->%s);' % (
                    f.elem.ty.name, dst, MODE_OUT, f.selector.name)
            return self.rand_elem(f.elem, dst, MODE_OUT)
        if sh == Field.STATIC:
            if f.elem.prim == 'char':
                return 'h_chars(r, %s, %d);' % (dst, f.n)
            return 'for (uint32_t i = 0; i < %d; i++) { %s }' % (
                f.n, self.rand_elem(f.elem, '%s[i]' % dst, MODE_OUT))
        if sh == Field.BLOB_OUT:
            return 'if (%s) { uint32_t n = (uint32_t)%s; for (uint32_t i = 0; i < n; i++) ((uint8_t *)%s)[i] = (uint8_t)hrng_next(r); }' % (
                dst, self.c_expr(f.count, 'val->'), dst)
        return ''

    def fillers(self):
        protos = []
        bodies = []
        for ty in self.m.structs:
            if ty.category == VkType.UNION:
                if ty.is_valid_union():
                    protos.append('static void fill_%s_tag(%s *val, struct hrng *r, int mode, int64_t tag);' % (ty.name, ty.name))
                else:
                    protos.append('static void fill_%s(%s *val, struct hrng *r, int mode);' % (ty.name, ty.name))
                bodies.append(self.union_fill(ty))
                continue
            protos.append('static void fill_%s(%s *val, struct hrng *r, int mode);' % (ty.name, ty.name))
            protos.append('static void fill_%s_body(%s *val, struct hrng *r, int mode);' % (ty.name, ty.name))
            bodies.append(self.struct_fill(ty))
        return '\n'.join(protos) + '\n\n' + '\n\n'.join(bodies) + '\n'

    # ---- per-command parameters (driver side) ------------------------------

    def params_struct(self, cmd):
        lines = ['struct p_%s {' % cmd.name]
        for var in cmd.variables:
            decl = var.to_c()
            if var.ty.is_static_array() and decl.startswith('const '):
                decl = decl[len('const '):]  # the builder fills it in place
            lines.append('    %s;' % decl)
        lines.append('};')
        return '\n'.join(lines)

    def build_params(self, cmd):
        """Fill a params struct with a random but well-formed call."""
        out = []
        claimed = set()
        prefix = 'p->'
        for f in self.m.fields[cmd]:
            dst = 'p->%s' % f.c_name
            sh = f.shape
            out_ = self.m.is_out(f)
            in_ = 'var_in' in f.var.attrs
            v = self.m.command_validity(cmd, f)
            mode = MODE_SKEL if out_ else MODE_IN
            if f.stride_of is not None:
                continue  # set by its array
            if sh == Field.NULL_ONLY:
                out.append('%s = NULL;' % dst)
            elif sh == Field.PLAIN:
                if f.c_name in claimed:
                    continue
                if f.elem.kind == Elem.HANDLE:
                    out.append('%s = (%s)(uintptr_t)h_id(r);' % (dst, f.elem.ty.name))
                elif f.elem.kind in (Elem.STRUCT, Elem.UNION):
                    out.append(self.rand_elem(f.elem, dst, MODE_IN))
                else:
                    out.append(self.rand_scalar(f.elem, dst))
            elif sh == Field.STATIC:
                out.append(self.fill_in(cmd, f, dst, VALID, claimed, set(), MODE_IN))
            elif sh == Field.PTR:
                if f.elem.kind in (Elem.STRUCT, Elem.UNION):
                    inner = 'fill_%s(t, r, %d);' % (f.elem.ty.name, mode)
                elif f.elem.kind == Elem.HANDLE:
                    inner = '*t = (%s)(uintptr_t)h_id(r);' % f.elem.ty.name
                elif in_:
                    # an in/out count: small, so the arrays it sizes stay small
                    inner = '*t = hrng_below(r, 4);'
                else:
                    inner = ''
                make = '{ %s *t = h_calloc(1, sizeof(*t)); %s %s = t; }' % (f.elem.ty.name, inner, dst)
                if f.optional:
                    out.append('if (hrng_below(r, 3)) %s else %s = NULL;' % (make, dst))
                else:
                    out.append(make)
            elif sh == Field.STRING:
                out.append('%s = %s;' % (dst, 'hrng_below(r, 2) ? h_string(r) : NULL' if f.optional else 'h_string(r)'))
            else:
                count = self.claim(f.count, claimed, prefix)
                null_ok = 'n == 0 || %s' % ('1' if f.optional else '0')
                if f.strided:
                    t = f.elem.ty.name
                    stride = 'p->%s' % f.strided
                    make = ('{ size_t s = sizeof(%s) * (1 + hrng_below(r, 2)); uint8_t *a = h_calloc(n ? n : 1, s); '
                            'for (uint32_t i = 0; i < n; i++) { fill_%s((%s *)(a + s * i), r, %d); } '
                            '%s = (void *)a; %s = (uint32_t)s; }' % (t, t, t, mode, dst, stride))
                    out.append('%s = sizeof(%s);' % (stride, t))
                elif sh == Field.DYN:
                    each = '' if v == INVALID else 'for (uint32_t i = 0; i < n; i++) { %s }' % (
                        self.rand_elem(f.elem, 'a[i]', mode))
                    make = '{ %s *a = h_calloc(n, sizeof(*a)); %s %s = (void *)a; }' % (f.elem.ty.name, each, dst)
                elif sh == Field.NESTED:
                    make = self.nested_make(f, dst, prefix, mode)
                elif sh == Field.STRING_ARRAY:
                    make = '{ const char **a = h_calloc(n, sizeof(*a)); for (uint32_t i = 0; i < n; i++) a[i] = h_string(r); %s = a; }' % dst
                else:
                    fill = '' if v == INVALID else 'for (uint32_t i = 0; i < n; i++) a[i] = (uint8_t)hrng_next(r);'
                    make = '{ uint8_t *a = h_calloc(n, 1); %s %s = (void *)a; }' % (fill, dst)
                out.append('{ %s if ((%s) && hrng_below(r, 2)) %s = NULL; else %s }' % (count, null_ok, dst, make))
        return out

    def fill_reply(self, cmd):
        """Renderer side: after decoding the args, give every output a value."""
        out = []
        if cmd.ret:
            base = cmd.ret.ty.base
            if base.name == 'VkResult':
                out.append('args.ret = (VkResult)(hrng_below(r, 2) ? 0 : 5);')
            else:
                out.append(self.rand_scalar(self.m.elem_of(base), 'args.ret'))
        arrays_by_count = {}
        for f in self.m.fields[cmd]:
            if f.count is not None and f.count.kind == Count.PATH and len(f.count.path) == 1:
                arrays_by_count.setdefault(f.count.path[0].name, []).append(f)
        for f in self.m.fields[cmd]:
            if not self.m.is_out(f):
                continue
            dst = 'args.%s' % f.c_name
            sh = f.shape
            if sh == Field.PTR:
                if f.elem.kind in (Elem.STRUCT, Elem.UNION):
                    out.append('if (%s) fill_%s(%s, r, %d);' % (dst, f.elem.ty.name, dst, MODE_OUT))
                elif f.elem.kind == Elem.HANDLE:
                    pass  # echoed
                elif f.c_name in arrays_by_count:
                    arrs = arrays_by_count[f.c_name]
                    cond = ' && '.join('!args.%s' % a.c_name for a in arrs)
                    out.append('if (%s && %s) *%s = hrng_below(r, 6);' % (dst, cond, dst))
                else:
                    out.append('if (%s) { %s }' % (dst, self.rand_elem(f.elem, '*' + dst, MODE_OUT)))
            elif sh == Field.DYN:
                n = self.c_expr(f.count, 'args.')
                if f.elem.kind in (Elem.STRUCT, Elem.UNION):
                    each = 'fill_%s(&%s[i], r, %d);' % (f.elem.ty.name, dst, MODE_OUT)
                elif f.elem.kind == Elem.HANDLE:
                    each = ''
                else:
                    each = self.rand_elem(f.elem, '%s[i]' % dst, MODE_OUT)
                if each:
                    out.append('if (%s) for (uint32_t i = 0; i < (uint32_t)%s; i++) { %s }' % (dst, n, each))
            elif sh == Field.BLOB_OUT:
                n = self.c_expr(f.count, 'args.')
                out.append('if (%s) for (uint32_t i = 0; i < (uint32_t)%s; i++) ((uint8_t *)%s)[i] = (uint8_t)hrng_next(r);' % (dst, n, dst))
            elif sh in (Field.STRING, Field.STRING_ARRAY, Field.BLOB, Field.NESTED, Field.STATIC):
                raise rp.Unsupported('harness: output %s %s' % (sh, f.c_name))
        return out

    def call_args(self, cmd):
        return ', '.join('p->%s' % v.name for v in cmd.variables)

    # ---- files --------------------------------------------------------------

    def driver_c(self):
        lines = ['#include "vn_protocol_driver.h"', '#include "harness.h"', '#include "harness_cases.h"', '']
        lines.append('struct hwrite *h_writes; size_t h_nwrites, h_capwrites;')
        lines.append(POISON_C)
        lines.append(self.fillers())
        for i, cmd in enumerate(self.m.commands):
            lines.append(self.params_struct(cmd))
            lines.append('static void build_%s(struct p_%s *p, struct hrng *r)' % (cmd.name, cmd.name))
            lines.append('{')
            lines.extend('    ' + s for s in self.build_params(cmd))
            lines.append('}')
            lines.append('int drv_encode_%d(uint64_t seed, int poison, struct hbuf *out)' % i)
            lines.append('{')
            lines.append('    struct hrng rng, *r = &rng; hrng_seed(r, seed); r->poison = poison;')
            lines.append('    struct p_%s params, *p = &params; memset(p, 0, sizeof(*p));' % cmd.name)
            lines.append('    build_%s(p, r);' % cmd.name)
            lines.append('    struct vn_cs_encoder enc = { 0 };')
            lines.append('    h_nwrites = 0;')
            lines.append('    vn_encode_%s(&enc, VK_COMMAND_GENERATE_REPLY_BIT_EXT, %s);' % (cmd.name, self.call_args(cmd)))
            lines.append('    *out = enc.buf;')
            lines.append('    return r->poison_stype ? h_poison(out, r->poison_stype, r->poison_with) : 0;')
            lines.append('}')
            lines.append('int drv_check_reply_%d(uint64_t seed, int poison, const uint8_t *data, size_t len)' % i)
            lines.append('{')
            lines.append('    struct hrng rng, *r = &rng; hrng_seed(r, seed); r->poison = poison;')
            lines.append('    struct p_%s params, *p = &params; memset(p, 0, sizeof(*p));' % cmd.name)
            lines.append('    build_%s(p, r);' % cmd.name)
            lines.append('    struct vn_cs_decoder dec = { data, len, 0, false };')
            lines.append('    vn_decode_%s_reply(&dec, %s);' % (cmd.name, self.call_args(cmd)))
            lines.append('    return !dec.fatal && dec.pos == len;')
            lines.append('}')
            lines.append('')
        return '\n'.join(lines)

    def renderer_c(self):
        lines = ['#include "vn_protocol_renderer.h"', '#include "harness.h"', '#include "harness_cases.h"', '']
        lines.append(self.fillers())
        for i, cmd in enumerate(self.m.commands):
            blob = 'need_blob_encode' in cmd.attrs
            decode = 'vn_decode_%s_args_temp(dec, %s&args);' % (
                cmd.name, '(struct vn_cs_encoder *)&e, ' if blob else '')
            head = ['    struct vkr_cs_decoder d = { data, len, 0, false };',
                    '    struct vn_cs_decoder *dec = (struct vn_cs_decoder *)&d;',
                    '    struct vkr_cs_encoder e = { 0 };',
                    '    VkCommandTypeEXT type; VkFlags flags;',
                    '    vn_decode_VkCommandTypeEXT(dec, &type); vn_decode_VkFlags(dec, &flags);',
                    '    struct vn_command_%s args; memset(&args, 0, sizeof(args));' % cmd.name,
                    '    ' + decode]
            lines.append('int rnd_decodes_%d(const uint8_t *data, size_t len)' % i)
            lines.append('{')
            lines.extend(head)
            lines.append('    return !d.fatal && d.pos == len && type == %s;' % cmd.attrs['c_type'])
            lines.append('}')
            lines.append('int rnd_reply_%d(uint64_t seed, const uint8_t *data, size_t len, struct hbuf *out)' % i)
            lines.append('{')
            lines.append('    struct hrng rng, *r = &rng; hrng_seed(r, seed ^ 0x5eed5eedULL);')
            lines.extend(head)
            lines.append('    if (d.fatal || d.pos != len || type != %s) return 0;' % cmd.attrs['c_type'])
            lines.extend('    ' + s for s in self.fill_reply(cmd))
            lines.append('    struct vkr_cs_encoder re = { 0 };')
            lines.append('    vn_encode_%s_reply((struct vn_cs_encoder *)&re, &args);' % cmd.name)
            lines.append('    *out = re.buf;')
            lines.append('    return 1;')
            lines.append('}')
            lines.append('')
        lines.append(INFO_C)
        return '\n'.join(lines)

    def cases_h(self):
        lines = ['#ifndef HARNESS_CASES_H', '#define HARNESS_CASES_H', '#include "harness.h"']
        for i, _cmd in enumerate(self.m.commands):
            lines.append('int drv_encode_%d(uint64_t seed, int poison, struct hbuf *out);' % i)
            lines.append('int drv_check_reply_%d(uint64_t seed, int poison, const uint8_t *data, size_t len);' % i)
            lines.append('int rnd_reply_%d(uint64_t seed, const uint8_t *data, size_t len, struct hbuf *out);' % i)
            lines.append('int rnd_decodes_%d(const uint8_t *data, size_t len);' % i)
        lines.append('struct harness_command { const char *name; int (*encode)(uint64_t, int, struct hbuf *);')
        lines.append('    int (*check)(uint64_t, int, const uint8_t *, size_t); int (*reply)(uint64_t, const uint8_t *, size_t, struct hbuf *);')
        lines.append('    int (*decodes)(const uint8_t *, size_t); };')
        lines.append('int dump_info(const char *path);')
        lines.append('#endif')
        return '\n'.join(lines) + '\n'

    def main_c(self):
        lines = ['#include "harness_cases.h"', '']
        lines.append('static const struct harness_command commands[] = {')
        for i, cmd in enumerate(self.m.commands):
            lines.append('    { "%s", drv_encode_%d, drv_check_reply_%d, rnd_reply_%d, rnd_decodes_%d },' % (
                cmd.name, i, i, i, i))
        lines.append('};')
        lines.append('#define COMMAND_COUNT %d' % len(self.m.commands))
        lines.append(MAIN_C)
        return '\n'.join(lines)


POISON_C = r'''
/*
 * Poisoning happens in the bytes: the driver's encoder skips a link it does
 * not know, so a chain can only carry an unadmitted sType if it is written
 * there after encoding. vn_cs.h logs every write; the marker (8 bytes, 1)
 * followed by the sType of the link the filler chose is the spot.
 */
static int h_poison(struct hbuf *buf, int32_t stype, int32_t with)
{
    for (size_t i = 0; i + 1 < h_nwrites; i++) {
        if (h_writes[i].size == 8 && h_writes[i].value == 1 &&
            h_writes[i + 1].size == 4 && (int32_t)h_writes[i + 1].value == stype &&
            h_writes[i + 1].at + 4 <= buf->len) {
            memcpy(buf->data + h_writes[i + 1].at, &with, 4);
            return 1;
        }
    }
    return 0;
}
'''

INFO_C = r'''
/* phase 0: the renderer's extension table and virglrenderer's capset mask */
int dump_info(const char *path)
{
    FILE *f = fopen(path, "w");
    if (!f) { perror(path); return 2; }
    for (uint32_t i = 0; i < _vn_info_extension_count; i++)
        fprintf(f, "ext %s %u %u\n", _vn_info_extensions[i].name,
                _vn_info_extensions[i].number, _vn_info_extensions[i].spec_version);
    uint32_t mask[32] = { 0 };
    uint32_t ext_mask[VN_INFO_EXTENSION_MAX_NUMBER / 32 + 1] = { 0 };
    vn_info_extension_mask_init(ext_mask);
    memcpy(mask, ext_mask, sizeof(ext_mask));
    mask[0] |= 1; /* vkr_renderer.c: the sentinel */
    fprintf(f, "mask");
    for (int i = 0; i < 32; i++)
        fprintf(f, " %u", mask[i]);
    fprintf(f, "\nvk_xml_version %u\nwire_format_version %u\n", vn_info_vk_xml_version(),
            vn_info_wire_format_version());
    fclose(f);
    return 0;
}
'''

MAIN_C = r'''
/*
 * phase1 <cases.bin> <seeds>: for each command and seed, a record
 *     u32 command index, u32 kind (0 positive, 1 poisoned), u64 seed,
 *     u32 command length, command, u32 reply length, reply
 * phase3 <cases.bin> <rust_replies.bin>: rust_replies holds, per positive
 *     record in order, u32 reply length, reply.
 * info <out.txt>: phase 0.
 */
static void put_blob(FILE *f, const uint8_t *data, size_t len)
{
    uint32_t n = (uint32_t)len;
    fwrite(&n, 4, 1, f);
    if (len)
        fwrite(data, 1, len, f);
}

static int get_u32(FILE *f, uint32_t *v) { return fread(v, 4, 1, f) == 1; }
static int get_u64(FILE *f, uint64_t *v) { return fread(v, 8, 1, f) == 1; }

static uint8_t *get_blob(FILE *f, uint32_t *len)
{
    if (!get_u32(f, len))
        return NULL;
    uint8_t *data = h_calloc(*len, 1);
    if (*len && fread(data, 1, *len, f) != *len)
        return NULL;
    return data;
}

static int phase1(const char *path, uint32_t seeds)
{
    FILE *f = fopen(path, "wb");
    if (!f) { perror(path); return 2; }
    unsigned positive = 0, poisoned = 0, bad = 0, unpoisonable = 0;
    int trace = getenv("HARNESS_TRACE") != NULL;
    for (uint32_t c = 0; c < COMMAND_COUNT; c++) {
        for (uint32_t s = 0; s < seeds; s++) {
            uint64_t seed = ((uint64_t)c << 32) | s;
            int poison = (s % 8) == 7;
            struct hbuf cmd = { 0 }, reply = { 0 };
            if (trace)
                fprintf(stderr, "phase1: %s seed %u\n", commands[c].name, s);
            int kind = commands[c].encode(seed, poison, &cmd);
            if (poison && !kind) {
                /* nothing chainable reached the encoder: an ordinary case */
                unpoisonable++;
            }
            if (!kind) {
                if (!commands[c].reply(seed, cmd.data, cmd.len, &reply)) {
                    fprintf(stderr, "phase1: the C renderer refused %s seed %u\n", commands[c].name, s);
                    bad++;
                    continue;
                }
                positive++;
            } else {
                if (commands[c].decodes(cmd.data, cmd.len)) {
                    fprintf(stderr, "phase1: the C renderer accepted a poisoned %s seed %u\n", commands[c].name, s);
                    bad++;
                    continue;
                }
                poisoned++;
            }
            fwrite(&c, 4, 1, f);
            uint32_t k = (uint32_t)kind | ((uint32_t)poison << 1);
            fwrite(&k, 4, 1, f);
            fwrite(&seed, 8, 1, f);
            put_blob(f, cmd.data, cmd.len);
            put_blob(f, reply.data, reply.len);
        }
    }
    fclose(f);
    printf("phase1: %u positive cases, %u poisoned (all refused by the C renderer), %u poison seeds with no chain, %u refused by the C renderer\n",
           positive, poisoned, unpoisonable, bad);
    return bad ? 1 : 0;
}

static int phase3(const char *cases, const char *replies)
{
    FILE *f = fopen(cases, "rb"), *g = fopen(replies, "rb");
    if (!f || !g) { perror("phase3"); return 2; }
    unsigned ok = 0, bad = 0;
    uint32_t c, kind;
    uint64_t seed;
    while (get_u32(f, &c) && get_u32(f, &kind) && get_u64(f, &seed)) {
        uint32_t clen, rlen, len;
        if (!get_blob(f, &clen) || !get_blob(f, &rlen))
            return 2;
        if (kind & 1)
            continue;
        uint8_t *reply = get_blob(g, &len);
        if (!reply || c >= COMMAND_COUNT)
            return 2;
        if (commands[c].check(seed, (int)(kind >> 1), reply, len)) {
            ok++;
        } else {
            fprintf(stderr, "phase3: the C driver rejects the Rust reply to %s seed %u\n",
                    commands[c].name, (unsigned)(seed & 0xffffffff));
            bad++;
        }
    }
    printf("phase3: %u Rust replies accepted by the C driver's decoder, %u rejected\n", ok, bad);
    return bad ? 1 : 0;
}

int main(int argc, char **argv)
{
    if (argc == 4 && !strcmp(argv[1], "phase1"))
        return phase1(argv[2], (uint32_t)strtoul(argv[3], NULL, 10));
    if (argc == 4 && !strcmp(argv[1], "phase3"))
        return phase3(argv[2], argv[3]);
    if (argc == 3 && !strcmp(argv[1], "info"))
        return dump_info(argv[2]);
    fprintf(stderr, "usage: %s phase1 <cases.bin> <seeds> | phase3 <cases.bin> <rust_replies.bin> | info <out>\n", argv[0]);
    return 2;
}
'''


def find_cc():
    for cc in (os.environ.get('CC'), 'cc', 'gcc', 'clang'):
        if cc and shutil.which(cc):
            return cc
    sys.exit('run_differential: no C compiler (set CC)')


def run(cmd, **kw):
    print('+', ' '.join(str(c) for c in cmd), flush=True)
    return subprocess.run(cmd, check=True, **kw)


LINE_RE = re.compile(r'^\s+(vk\w+)\s+(\d+) / (\d+)\s+round-tripped, (\d+) poisoned refused')


def coverage(model, output):
    """Per-construct totals from the Rust side's per-command lines."""
    per_cmd = {}
    for line in output.splitlines():
        m = LINE_RE.match(line)
        if m:
            per_cmd[m.group(1)] = tuple(int(x) for x in m.group(2, 3, 4))
    missing = [c.name for c in model.commands if c.name not in per_cmd]
    print('\ncoverage: %d of %d generated commands exercised%s' % (
        len(model.commands) - len(missing), len(model.commands),
        (' (missing: %s)' % ', '.join(missing)) if missing else ''))
    print('  %-26s %9s %10s %10s' % ('construct', 'commands', 'cases', 'poisoned'))
    for construct in sorted(model.constructs):
        cmds = sorted(model.constructs[construct])
        passed = sum(per_cmd.get(c, (0, 0, 0))[0] for c in cmds)
        refused = sum(per_cmd.get(c, (0, 0, 0))[2] for c in cmds)
        print('  %-26s %9d %10d %10d' % (construct, len(cmds), passed, refused))
    return missing


def main():
    parser = argparse.ArgumentParser(description=__doc__,
                                     formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument('--seeds', type=int, default=256, help='cases per command')
    parser.add_argument('--workdir', help='scratch directory (default: a temporary one)')
    parser.add_argument('--keep', action='store_true', help='keep the scratch directory')
    parser.add_argument('--c-only', action='store_true',
                        help='stop after phase 1 (build and run the C side only)')
    args = parser.parse_args()

    tmp = None
    if args.workdir:
        work = Path(args.workdir)
        work.mkdir(parents=True, exist_ok=True)
    else:
        tmp = tempfile.mkdtemp(prefix='venus-diff-')
        work = Path(tmp)
    try:
        gen_dir = work / 'gen'
        gen_dir.mkdir(exist_ok=True)
        banner = gen_dir / 'banner'
        banner.write_text('/* generated for the differential harness */\n\n')
        for variant in ([], ['--renderer']):
            run([sys.executable, str(TOOLS / 'vn_protocol.py'), '--outdir', str(gen_dir),
                 '--banner', str(banner)] + variant)

        model = rp.Model(rp.Selection.parse(TOOLS / 'rust-selection.txt'))
        cg = CGen(model)
        (work / 'harness_cases.h').write_text(cg.cases_h())
        (work / 'driver.c').write_text(cg.driver_c())
        (work / 'renderer.c').write_text(cg.renderer_c())
        (work / 'main.c').write_text(cg.main_c())
        cc = find_cc()
        flags = ['-std=gnu11', '-O1', '-g', '-w', '-I', str(work), '-I', str(HERE),
                 '-I', str(gen_dir), '-I', str(TOOLS / 'include'),
                 '-I', str(TOOLS / 'include' / 'vulkan')]
        objs = []
        for src in ('driver.c', 'renderer.c', 'main.c'):
            obj = work / (src[:-2] + '.o')
            run([cc] + flags + ['-c', str(work / src), '-o', str(obj)])
            objs.append(str(obj))
        exe = work / 'harness'
        run([cc] + objs + ['-o', str(exe)])

        cases = work / 'cases.bin'
        replies = work / 'rust_replies.bin'
        info = work / 'info.txt'
        run([str(exe), 'info', str(info)])
        run([str(exe), 'phase1', str(cases), str(args.seeds)])
        if args.c_only:
            print('run_differential: phase 1 only (--c-only); cases in', cases)
            return
        env = dict(os.environ, VENUS_DIFF_CASES=str(cases), VENUS_DIFF_REPLIES=str(replies),
                   VENUS_DIFF_INFO=str(info))
        cmd = ['cargo', 'test', '-p', 'virtio-gpu', '--test', 'venus_protocol_differential',
               '--', '--ignored', '--nocapture']
        print('+', ' '.join(cmd), flush=True)
        proc = subprocess.run(cmd, cwd=str(ROOT), env=env, stdout=subprocess.PIPE, text=True)
        sys.stdout.write(proc.stdout)
        if proc.returncode != 0:
            raise subprocess.CalledProcessError(proc.returncode, cmd)
        run([str(exe), 'phase3', str(cases), str(replies)])
        missing = coverage(model, proc.stdout)
        if missing:
            sys.exit('run_differential: %d commands had no case' % len(missing))
        print('run_differential: all phases passed')
    except subprocess.CalledProcessError as err:
        sys.exit('run_differential: %s failed with %s' % (err.cmd[0], err.returncode))
    finally:
        if tmp and not args.keep:
            shutil.rmtree(tmp, ignore_errors=True)


if __name__ == '__main__':
    main()
