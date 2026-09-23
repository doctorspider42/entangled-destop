#!/usr/bin/env python3

# Copyright 2026 The Entangled Desktop authors
# SPDX-License-Identifier: MIT

"""Differential check of the generated Rust against the generated C.

The vendored generator emits two C implementations of the protocol besides
our Rust: the *driver* (what Mesa compiles into the guest) and the
*renderer* (what virglrenderer compiles into the host). This harness makes
them the oracle:

  phase 1 (C)     for every generated command and a spread of seeds, build
                  randomised parameters — present and null pointers, empty
                  and non-empty arrays, strings, pNext chains drawn from the
                  whitelist — and encode them with the C driver encoder.
                  Decode each with the C renderer decoder, fill every output
                  with random values, and encode the reply with the C
                  renderer. Some seeds instead poison a chain with a structure
                  the protocol knows but this crate does not admit.
  phase 2 (Rust)  decode each command with the generated Rust, re-encode it
                  as the driver would, and require the bytes to be identical;
                  decode the C reply into the command, re-encode it, and
                  require identical bytes again. Poisoned commands must be
                  refused.
  phase 3 (C)     decode every Rust-encoded reply with the C driver's reply
                  decoder, rebuilt from the same seed, and require it to
                  consume exactly the reply without going fatal.

Needs Python with Mako (for the vendored generator), a C11 compiler (`cc`,
`gcc` or `clang`; tested with gcc under WSL and on ubuntu-latest) and cargo.
All scratch output goes to a temporary directory outside the repository
unless --workdir says otherwise.

    python tools/venus-protocol/harness/run_differential.py [--seeds N]
"""

import argparse
import os
import shutil
import struct
import subprocess
import sys
import tempfile
from pathlib import Path

HERE = Path(__file__).resolve().parent
TOOLS = HERE.parent
ROOT = TOOLS.parent.parent
sys.path.insert(0, str(TOOLS))

import rust_protocol as rp  # noqa: E402
from rust_protocol import Elem, Field, INVALID, PARTIAL, VALID  # noqa: E402
from vkxml import VkType  # noqa: E402

MODE_IN, MODE_SKEL, MODE_OUT = 0, 1, 2


class CGen:
    """C fillers and per-command drivers, from the same Model as the Rust."""

    def __init__(self, model):
        self.m = model

    # ---- helpers ----------------------------------------------------------

    def c_elem_type(self, elem):
        return elem.ty.name

    def rand_scalar(self, elem, dst):
        prim = elem.prim
        ty = elem.ty
        if ty.category == VkType.ENUM:
            return '%s = (%s)hrng_below(r, 4);' % (dst, ty.name)
        if prim == 'float':
            return '%s = h_float(r);' % dst
        if prim == 'double':
            return '%s = (double)h_float(r);' % dst
        if prim in ('uint64_t', 'int64_t', 'size_t'):
            return '%s = (%s)hrng_next(r);' % (dst, ty.name)
        return '%s = (%s)hrng_next(r);' % (dst, ty.name)

    def rand_elem(self, elem, dst, mode):
        if elem.kind == Elem.SCALAR:
            return self.rand_scalar(elem, dst)
        if elem.kind == Elem.HANDLE:
            return '%s = (%s)(uintptr_t)h_id(r);' % (dst, elem.ty.name)
        return 'fill_%s(&%s, r, %d);' % (elem.ty.name, dst, mode)

    def len_targets(self, ty):
        """Members named as another member's dynamic-array count."""
        out = set()
        for f in self.m.fields[ty]:
            if f.len_var and len(f.len_var) == 1:
                out.add(f.len_var[0].name)
        return out

    # ---- per-structure fillers ----------------------------------------------

    def struct_fill(self, ty):
        name = ty.name
        chain = self.m.chains[ty]
        lines = []
        body = []
        claimed = set()
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
                lines.append('    int order[%d];' % n)
                lines.append('    for (int i = 0; i < %d; i++) order[i] = i;' % n)
                lines.append('    for (int i = %d - 1; i > 0; i--) { int j = (int)hrng_below(r, (uint32_t)i + 1); int t = order[i]; order[i] = order[j]; order[j] = t; }' % n)
                lines.append('    int take = (int)hrng_below(r, %d);' % (min(n, 4) + 1))
                lines.append('    for (int k = 0; k < take; k++) {')
                lines.append('        VkBaseOutStructure *link = NULL;')
                lines.append('        switch (order[k]) {')
                for i, c in enumerate(chain):
                    lines.append('        case %d: { %s *t = h_calloc(1, sizeof(*t)); t->sType = %s; fill_%s_body(t, r, mode); link = (VkBaseOutStructure *)t; break; }' % (i, c.name, c.s_type, c.name))
                lines.append('        }')
                lines.append('        if (tail) tail->pNext = link; else head = link;')
                lines.append('        tail = link;')
                lines.append('    }')
            bad = self.unadmitted(ty)
            if bad:
                lines.append('    if (r->poison) {')
                lines.append('        VkBaseOutStructure *link = NULL;')
                lines.append('        switch (hrng_below(r, %d)) {' % len(bad))
                for i, c in enumerate(bad):
                    lines.append('        case %d: { %s *t = h_calloc(1, sizeof(*t)); t->sType = %s; link = (VkBaseOutStructure *)t; break; }' % (i, c.name, c.s_type))
                lines.append('        }')
                lines.append('        if (link) { link->pNext = (VkBaseOutStructure *)head; head = link; if (!tail) tail = link; r->poison = 0; r->poisoned = 1; }')
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

    def unadmitted(self, ty):
        """Chain structures the protocol admits and this crate does not."""
        types, _ = self.m.gen.get_chain(ty)
        ours = set(self.m.chains[ty])
        return [t for t in types if t not in ours][:24]

    def field_fill(self, ty, f, dst, vpart, claimed, targets):
        """C statements filling field `f`, switching on `mode` at run time."""
        in_code = self.fill_in(ty, f, dst, VALID, claimed, targets, MODE_IN)
        skel_code = self.fill_in(ty, f, dst, vpart, set(claimed), targets, MODE_SKEL)
        out_code = self.fill_out(ty, f, dst, targets)
        return 'if (mode == %d) { %s } else if (mode == %d) { %s } else { %s }' % (
            MODE_IN, in_code, MODE_SKEL, skel_code, out_code)

    def claim(self, f, dst_base, claimed, owner_prefix):
        head = f.len_var[0]
        if len(f.len_var) != 1 or head.ty.is_pointer():
            raise rp.Unsupported('harness: len %s of %s' % (head.name, f.c_name))
        cnt = '%s%s' % (owner_prefix, head.name)
        if head.name in claimed:
            return 'uint32_t n = (uint32_t)%s;' % cnt
        claimed.add(head.name)
        return 'uint32_t n = hrng_below(r, 4); %s = (%s)n;' % (cnt, head.ty.base.name)

    def fill_in(self, ty, f, dst, validity, claimed, targets, mode):
        """IN (everything) or SKEL (only what the partial encoding carries)."""
        sh = f.shape
        prefix = 'val->'
        if sh == Field.NULL_ONLY:
            return '%s = NULL;' % dst
        if validity == INVALID and sh in (Field.PLAIN, Field.STATIC):
            return ''
        if sh == Field.PLAIN:
            return self.rand_elem(f.elem, dst, mode)
        if sh == Field.STATIC:
            if f.elem.prim == 'char':
                return 'h_chars(r, %s, %d);' % (dst, f.n)
            return 'for (uint32_t i = 0; i < %d; i++) { %s }' % (
                f.n, self.rand_elem(f.elem, '%s[i]' % dst, mode))
        if sh == Field.PTR:
            if validity == INVALID:
                inner = ''
            elif f.elem.kind == Elem.STRUCT:
                inner = 'fill_%s(t, r, mode);' % f.elem.ty.name
            else:
                inner = self.rand_elem(f.elem, '*t', mode)
            make = '{ %s *t = h_calloc(1, sizeof(*t)); %s %s = t; }' % (
                f.elem.ty.name, inner, dst)
            if f.optional:
                return 'if (hrng_below(r, 3)) %s else %s = NULL;' % (make, dst)
            return make
        if sh == Field.STRING:
            if f.optional:
                return '%s = hrng_below(r, 3) ? h_string(r) : NULL;' % dst
            return '%s = h_string(r);' % dst
        claim = self.claim(f, dst, claimed, prefix)
        null_ok = 'n == 0 || %s' % ('1' if f.optional else '0')
        if sh == Field.STRING_ARRAY:
            make = '{ const char **a = h_calloc(n, sizeof(*a)); for (uint32_t i = 0; i < n; i++) a[i] = h_string(r); %s = a; }' % dst
        elif sh == Field.BLOB:
            make = '{ uint8_t *a = h_calloc(n, 1); for (uint32_t i = 0; i < n; i++) a[i] = (uint8_t)hrng_next(r); %s = a; }' % dst
        else:
            if validity == INVALID:
                each = ''
            else:
                each = 'for (uint32_t i = 0; i < n; i++) { %s }' % self.rand_elem(f.elem, 'a[i]', mode)
            make = '{ %s *a = h_calloc(n, sizeof(*a)); %s %s = a; }' % (f.elem.ty.name, each, dst)
        return '{ %s if ((%s) && hrng_below(r, 2)) %s = NULL; else %s }' % (claim, null_ok, dst, make)

    def fill_out(self, ty, f, dst, targets):
        """OUT: the values a reply carries; shape (pointers, counts) untouched."""
        sh = f.shape
        if f.c_name in targets:
            return ''
        if sh == Field.PLAIN:
            return self.rand_elem(f.elem, dst, MODE_OUT)
        if sh == Field.STATIC:
            if f.elem.prim == 'char':
                return 'h_chars(r, %s, %d);' % (dst, f.n)
            return 'for (uint32_t i = 0; i < %d; i++) { %s }' % (
                f.n, self.rand_elem(f.elem, '%s[i]' % dst, MODE_OUT))
        return ''

    def fillers(self):
        protos = []
        bodies = []
        for ty in self.m.structs:
            protos.append('static void fill_%s(%s *val, struct hrng *r, int mode);' % (ty.name, ty.name))
            protos.append('static void fill_%s_body(%s *val, struct hrng *r, int mode);' % (ty.name, ty.name))
            bodies.append(self.struct_fill(ty))
        return '\n'.join(protos) + '\n\n' + '\n\n'.join(bodies) + '\n'

    # ---- per-command parameters (driver side) ------------------------------

    def params_struct(self, cmd):
        lines = ['struct p_%s {' % cmd.name]
        for var in cmd.variables:
            lines.append('    %s;' % var.to_c())
        lines.append('};')
        return '\n'.join(lines)

    def build_params(self, cmd):
        """Fill a params struct with a random but well-formed call."""
        out = []
        claimed = set()
        for f in self.m.fields[cmd]:
            dst = 'p->%s' % f.c_name
            sh = f.shape
            out_ = self.m.is_out(f)
            in_ = 'var_in' in f.var.attrs
            v = self.m.command_validity(cmd, f)
            if sh == Field.NULL_ONLY:
                out.append('%s = NULL;' % dst)
            elif sh == Field.PLAIN:
                if f.elem.kind == Elem.HANDLE:
                    out.append('%s = (%s)(uintptr_t)h_id(r);' % (dst, f.elem.ty.name))
                elif f.elem.kind == Elem.STRUCT:
                    out.append('fill_%s(&%s, r, %d);' % (f.elem.ty.name, dst, MODE_IN))
                else:
                    out.append(self.rand_scalar(f.elem, dst))
            elif sh == Field.PTR:
                mode = MODE_IN if not out_ else MODE_SKEL
                if f.elem.kind == Elem.STRUCT:
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
                head = f.len_var[0]
                if len(f.len_var) != 1:
                    raise rp.Unsupported('harness: %s.%s len path' % (cmd.name, f.c_name))
                if head.ty.is_pointer():
                    count = 'uint32_t n = p->%s ? (uint32_t)*p->%s : 0;' % (head.name, head.name)
                elif head.name in claimed:
                    count = 'uint32_t n = (uint32_t)p->%s;' % head.name
                else:
                    claimed.add(head.name)
                    count = 'uint32_t n = hrng_below(r, 4); p->%s = n;' % head.name
                mode = MODE_SKEL if out_ else MODE_IN
                if sh == Field.DYN:
                    each = '' if v == INVALID else 'for (uint32_t i = 0; i < n; i++) { %s }' % (
                        self.rand_elem(f.elem, 'a[i]', mode))
                    make = '{ %s *a = h_calloc(n, sizeof(*a)); %s %s = a; }' % (f.elem.ty.name, each, dst)
                elif sh == Field.STRING_ARRAY:
                    make = '{ const char **a = h_calloc(n, sizeof(*a)); for (uint32_t i = 0; i < n; i++) a[i] = h_string(r); %s = a; }' % dst
                else:
                    make = '{ uint8_t *a = h_calloc(n, 1); %s = a; }' % dst
                null_ok = 'n == 0 || %s' % ('1' if f.optional else '0')
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
            if f.len_var and len(f.len_var) == 1:
                arrays_by_count.setdefault(f.len_var[0].name, []).append(f)
        for f in self.m.fields[cmd]:
            if not self.m.is_out(f):
                continue
            dst = 'args.%s' % f.c_name
            sh = f.shape
            if sh == Field.PTR:
                if f.elem.kind == Elem.STRUCT:
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
                head = f.len_var[0]
                n = '(args.%s ? *args.%s : 0)' % (head.name, head.name) if head.ty.is_pointer() else 'args.%s' % head.name
                if f.elem.kind == Elem.STRUCT:
                    each = 'fill_%s(&%s[i], r, %d);' % (f.elem.ty.name, dst, MODE_OUT)
                elif f.elem.kind == Elem.HANDLE:
                    each = ''
                else:
                    each = self.rand_elem(f.elem, '%s[i]' % dst, MODE_OUT)
                if each:
                    out.append('if (%s) for (uint32_t i = 0; i < (uint32_t)%s; i++) { %s }' % (dst, n, each))
            elif sh in (Field.STRING, Field.STRING_ARRAY, Field.BLOB):
                raise rp.Unsupported('harness: output %s' % f.c_name)
        return out

    def call_args(self, cmd):
        return ', '.join('p->%s' % v.name for v in cmd.variables)

    # ---- files --------------------------------------------------------------

    def driver_c(self):
        lines = ['#include "vn_protocol_driver.h"', '#include "harness.h"', '#include "harness_cases.h"', '']
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
            lines.append('    vn_encode_%s(&enc, VK_COMMAND_GENERATE_REPLY_BIT_EXT, %s);' % (cmd.name, self.call_args(cmd)))
            lines.append('    *out = enc.buf;')
            lines.append('    return r->poisoned;')
            lines.append('}')
            lines.append('int drv_check_reply_%d(uint64_t seed, const uint8_t *data, size_t len)' % i)
            lines.append('{')
            lines.append('    struct hrng rng, *r = &rng; hrng_seed(r, seed);')
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
            lines.append('int rnd_reply_%d(uint64_t seed, const uint8_t *data, size_t len, struct hbuf *out)' % i)
            lines.append('{')
            lines.append('    struct hrng rng, *r = &rng; hrng_seed(r, seed ^ 0x5eed5eedULL);')
            lines.append('    struct vkr_cs_decoder d = { data, len, 0, false };')
            lines.append('    struct vn_cs_decoder *dec = (struct vn_cs_decoder *)&d;')
            lines.append('    VkCommandTypeEXT type; VkFlags flags;')
            lines.append('    vn_decode_VkCommandTypeEXT(dec, &type); vn_decode_VkFlags(dec, &flags);')
            lines.append('    struct vn_command_%s args; memset(&args, 0, sizeof(args));' % cmd.name)
            if 'need_blob_encode' in cmd.attrs:
                raise rp.Unsupported('harness: blob reply')
            lines.append('    vn_decode_%s_args_temp(dec, &args);' % cmd.name)
            lines.append('    if (d.fatal || d.pos != len || type != %s) return 0;' % cmd.attrs['c_type'])
            lines.extend('    ' + s for s in self.fill_reply(cmd))
            lines.append('    struct vkr_cs_encoder e = { 0 };')
            lines.append('    vn_encode_%s_reply((struct vn_cs_encoder *)&e, &args);' % cmd.name)
            lines.append('    *out = e.buf;')
            lines.append('    return 1;')
            lines.append('}')
            lines.append('')
        return '\n'.join(lines)

    def cases_h(self):
        lines = ['#ifndef HARNESS_CASES_H', '#define HARNESS_CASES_H', '#include "harness.h"']
        for i, _cmd in enumerate(self.m.commands):
            lines.append('int drv_encode_%d(uint64_t seed, int poison, struct hbuf *out);' % i)
            lines.append('int drv_check_reply_%d(uint64_t seed, const uint8_t *data, size_t len);' % i)
            lines.append('int rnd_reply_%d(uint64_t seed, const uint8_t *data, size_t len, struct hbuf *out);' % i)
        lines.append('struct harness_command { const char *name; int (*encode)(uint64_t, int, struct hbuf *);')
        lines.append('    int (*check)(uint64_t, const uint8_t *, size_t); int (*reply)(uint64_t, const uint8_t *, size_t, struct hbuf *); };')
        lines.append('#endif')
        return '\n'.join(lines) + '\n'

    def main_c(self):
        lines = ['#include "harness_cases.h"', '']
        lines.append('static const struct harness_command commands[] = {')
        for i, cmd in enumerate(self.m.commands):
            lines.append('    { "%s", drv_encode_%d, drv_check_reply_%d, rnd_reply_%d },' % (cmd.name, i, i, i))
        lines.append('};')
        lines.append('#define COMMAND_COUNT %d' % len(self.m.commands))
        lines.append(MAIN_C)
        return '\n'.join(lines)


MAIN_C = r'''
/*
 * phase1 <cases.bin> <seeds>: for each command and seed, a record
 *     u32 command index, u32 kind (0 positive, 1 poisoned), u64 seed,
 *     u32 command length, command, u32 reply length, reply
 * phase3 <cases.bin> <rust_replies.bin>: rust_replies holds, per positive
 *     record in order, u32 reply length, reply.
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
    unsigned positive = 0, poisoned = 0, bad = 0;
    for (uint32_t c = 0; c < COMMAND_COUNT; c++) {
        for (uint32_t s = 0; s < seeds; s++) {
            uint64_t seed = ((uint64_t)c << 32) | s;
            int poison = (s % 8) == 7;
            struct hbuf cmd = { 0 }, reply = { 0 };
            int kind = commands[c].encode(seed, poison, &cmd);
            if (!kind) {
                if (!commands[c].reply(seed, cmd.data, cmd.len, &reply)) {
                    fprintf(stderr, "phase1: the C renderer refused %s seed %u\n", commands[c].name, s);
                    bad++;
                    continue;
                }
                positive++;
            } else {
                poisoned++;
            }
            fwrite(&c, 4, 1, f);
            uint32_t k = (uint32_t)kind;
            fwrite(&k, 4, 1, f);
            fwrite(&seed, 8, 1, f);
            put_blob(f, cmd.data, cmd.len);
            put_blob(f, reply.data, reply.len);
        }
    }
    fclose(f);
    printf("phase1: %u positive cases, %u poisoned, %u refused by the C renderer\n", positive, poisoned, bad);
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
        if (kind)
            continue;
        uint8_t *reply = get_blob(g, &len);
        if (!reply || c >= COMMAND_COUNT)
            return 2;
        if (commands[c].check(seed, reply, len)) {
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
    fprintf(stderr, "usage: %s phase1 <cases.bin> <seeds> | phase3 <cases.bin> <rust_replies.bin>\n", argv[0]);
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
        run([str(exe), 'phase1', str(cases), str(args.seeds)])
        if args.c_only:
            print('run_differential: phase 1 only (--c-only); cases in', cases)
            return
        env = dict(os.environ, VENUS_DIFF_CASES=str(cases), VENUS_DIFF_REPLIES=str(replies))
        run(['cargo', 'test', '-p', 'virtio-gpu', '--test', 'venus_protocol_differential',
             '--', '--ignored', '--nocapture'], cwd=str(ROOT), env=env)
        run([str(exe), 'phase3', str(cases), str(replies)])
        print('run_differential: all phases passed')
    except subprocess.CalledProcessError as err:
        sys.exit('run_differential: %s failed with %s' % (err.cmd[0], err.returncode))
    finally:
        if tmp and not args.keep:
            shutil.rmtree(tmp, ignore_errors=True)


if __name__ == '__main__':
    main()
