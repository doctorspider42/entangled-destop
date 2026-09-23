#!/usr/bin/env python3
"""Regenerate crates/virtio-gpu/src/host_vulkan/convert.rs.

The Venus executor (crates/virtio-gpu/src/venus/executor/) speaks the
generated protocol structures; the host renderer (crates/virtio-gpu/src/
host_vulkan/) speaks `ash`. This script writes the field-for-field bridge
between the two so that nobody copies ~1500 fields by hand:

* `FromAsh` for every protocol structure **the executor uses** that has an
  `ash` counterpart whose fields can all be converted (host -> guest: what a
  query answers). "Uses" is computed, not listed: the structures reachable
  from the arguments of `EXECUTOR_COMMANDS` (checked against the `dispatch`
  match in venus/executor/context.rs), through members and through the pNext
  links the executor admits (`policy::admits_link`: core up to 1.3, and the
  venus protocol's own). The generated protocol is the whole protocol; this
  bridge stays the executor's size;
* `ToAsh` for the same set (guest -> host: what a create is built from);
* `query_features2` / `query_properties2`: one host call each that chains
  every structure the protocol can carry in that pNext chain, gated by the
  core version that introduced it, and hands the whole chain back;
* `DeviceLinks`: the owned `ash` twins of a `VkDeviceCreateInfo` chain, and
  the `push_next` calls that link them.

Fields are paired by name with underscores and case removed
(`storage_buffer16bit_access` == `storage_buffer16_bit_access`); a protocol
field with no `ash` twin is an error here, never a silently skipped field.

Needs only Python 3.8+ and the ash 0.38 sources in the cargo registry (the
workspace already depends on them through wgpu). Run from anywhere:

    python scripts/venus-ash-gen.py            # write
    python scripts/venus-ash-gen.py --check    # compare, exit 1 if stale

The output is passed through `rustfmt` (on PATH), so `--check` compares
exactly what is checked in.
"""

import argparse
import glob
import os
import re
import subprocess
import sys
import tempfile
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
PROTOCOL = ROOT / 'crates' / 'virtio-gpu' / 'src' / 'venus' / 'protocol'
OUT = ROOT / 'crates' / 'virtio-gpu' / 'src' / 'host_vulkan' / 'convert.rs'
ASH_VERSION = '0.38.0+1.3.281'

# Core version that introduced each structure a Features2/Properties2 chain
# may carry here (the selection is core <= 1.3 and no extensions). Querying a
# structure the device's version does not know is invalid usage, so the host
# chain is gated on min(device apiVersion, 1.3).
CORE_VERSION = {
    # features
    'VkPhysicalDevicePrivateDataFeatures': 13,
    'VkPhysicalDeviceVariablePointersFeatures': 11,
    'VkPhysicalDeviceMultiviewFeatures': 11,
    'VkPhysicalDevice16BitStorageFeatures': 11,
    'VkPhysicalDeviceShaderSubgroupExtendedTypesFeatures': 12,
    'VkPhysicalDeviceSamplerYcbcrConversionFeatures': 11,
    'VkPhysicalDeviceProtectedMemoryFeatures': 11,
    'VkPhysicalDeviceInlineUniformBlockFeatures': 13,
    'VkPhysicalDeviceMaintenance4Features': 13,
    'VkPhysicalDeviceShaderDrawParametersFeatures': 11,
    'VkPhysicalDeviceShaderFloat16Int8Features': 12,
    'VkPhysicalDeviceHostQueryResetFeatures': 12,
    'VkPhysicalDeviceDescriptorIndexingFeatures': 12,
    'VkPhysicalDeviceTimelineSemaphoreFeatures': 12,
    'VkPhysicalDevice8BitStorageFeatures': 12,
    'VkPhysicalDeviceVulkanMemoryModelFeatures': 12,
    'VkPhysicalDeviceShaderAtomicInt64Features': 12,
    'VkPhysicalDeviceScalarBlockLayoutFeatures': 12,
    'VkPhysicalDeviceUniformBufferStandardLayoutFeatures': 12,
    'VkPhysicalDeviceBufferDeviceAddressFeatures': 12,
    'VkPhysicalDeviceImagelessFramebufferFeatures': 12,
    'VkPhysicalDeviceTextureCompressionASTCHDRFeatures': 13,
    'VkPhysicalDeviceSeparateDepthStencilLayoutsFeatures': 12,
    'VkPhysicalDeviceShaderDemoteToHelperInvocationFeatures': 13,
    'VkPhysicalDeviceSubgroupSizeControlFeatures': 13,
    'VkPhysicalDevicePipelineCreationCacheControlFeatures': 13,
    'VkPhysicalDeviceVulkan11Features': 12,
    'VkPhysicalDeviceVulkan12Features': 12,
    'VkPhysicalDeviceVulkan13Features': 13,
    'VkPhysicalDeviceZeroInitializeWorkgroupMemoryFeatures': 13,
    'VkPhysicalDeviceImageRobustnessFeatures': 13,
    'VkPhysicalDeviceShaderTerminateInvocationFeatures': 13,
    'VkPhysicalDeviceSynchronization2Features': 13,
    'VkPhysicalDeviceShaderIntegerDotProductFeatures': 13,
    'VkPhysicalDeviceDynamicRenderingFeatures': 13,
    # properties
    'VkPhysicalDeviceDriverProperties': 12,
    'VkPhysicalDeviceIDProperties': 11,
    'VkPhysicalDeviceMultiviewProperties': 11,
    'VkPhysicalDeviceSubgroupProperties': 11,
    'VkPhysicalDevicePointClippingProperties': 11,
    'VkPhysicalDeviceProtectedMemoryProperties': 11,
    'VkPhysicalDeviceSamplerFilterMinmaxProperties': 12,
    'VkPhysicalDeviceInlineUniformBlockProperties': 13,
    'VkPhysicalDeviceMaintenance3Properties': 11,
    'VkPhysicalDeviceMaintenance4Properties': 13,
    'VkPhysicalDeviceFloatControlsProperties': 12,
    'VkPhysicalDeviceDescriptorIndexingProperties': 12,
    'VkPhysicalDeviceTimelineSemaphoreProperties': 12,
    'VkPhysicalDeviceDepthStencilResolveProperties': 12,
    'VkPhysicalDeviceTexelBufferAlignmentProperties': 13,
    'VkPhysicalDeviceSubgroupSizeControlProperties': 13,
    'VkPhysicalDeviceVulkan11Properties': 12,
    'VkPhysicalDeviceVulkan12Properties': 12,
    'VkPhysicalDeviceVulkan13Properties': 13,
    'VkPhysicalDeviceShaderIntegerDotProductProperties': 13,
}

# The commands `VulkanContext::dispatch` implements. The script refuses to
# run when this list and the `Command::X(args) =>` arms of that match differ.
EXECUTOR_COMMANDS = [
    'EnumerateInstanceVersion', 'CreateInstance', 'DestroyInstance',
    'EnumeratePhysicalDevices', 'EnumeratePhysicalDeviceGroups',
    'GetPhysicalDeviceProperties', 'GetPhysicalDeviceProperties2',
    'GetPhysicalDeviceFeatures2', 'GetPhysicalDeviceQueueFamilyProperties2',
    'GetPhysicalDeviceMemoryProperties2', 'EnumerateDeviceExtensionProperties',
    'GetPhysicalDeviceFormatProperties2', 'GetPhysicalDeviceImageFormatProperties2',
    'CreateDevice', 'DestroyDevice', 'GetDeviceQueue2', 'CreateCommandPool',
    'DestroyCommandPool', 'CreateImage', 'DestroyImage', 'GetImageMemoryRequirements2',
]
CONTEXT = ROOT / 'crates' / 'virtio-gpu' / 'src' / 'venus' / 'executor' / 'context.rs'

# The executor's chain policy (venus/executor/policy.rs `admits_link`),
# restated for the one question this script asks of it.
ADMITTED_API = (1 << 22) | (3 << 12)
ADMITTED_EXTENSIONS = {'VK_MESA_venus_protocol', 'VK_EXT_command_serialization'}

ASH_SCALARS = {'u8', 'u16', 'u32', 'i32', 'u64', 'i64', 'f32', 'f64', 'usize',
               'Bool32', 'DeviceSize', 'DeviceAddress', 'SampleMask', 'Flags', 'Flags64'}
PROTO_SCALARS = {'u8', 'u16', 'u32', 'i32', 'u64', 'i64', 'f32', 'f64'}


def norm(name):
    return name.replace('_', '').lower()


def find_ash():
    home = Path(os.environ.get('CARGO_HOME', Path.home() / '.cargo'))
    hits = glob.glob(str(home / 'registry' / 'src' / '*' / f'ash-{ASH_VERSION}' / 'src' / 'vk' / 'definitions.rs'))
    if not hits:
        sys.exit(f'cannot find ash {ASH_VERSION} in {home}/registry; run `cargo fetch` first')
    return Path(hits[0])


STRUCT_RE = re.compile(r"^pub struct (\w+)(<'a>)? \{\n(.*?)^\}", re.M | re.S)
FIELD_RE = re.compile(r"^\s*pub (\w+): (.+),\s*$")


def parse_structs(text):
    out = {}
    for m in STRUCT_RE.finditer(text):
        name, lt, body = m.group(1), m.group(2), m.group(3)
        fields = []
        # rustfmt wraps a long `name: Type,` onto two lines; rejoin them.
        body = re.sub(r':\n\s+', ': ', body)
        for line in body.splitlines():
            line = line.strip()
            if line.startswith('///') or line.startswith('#') or not line:
                continue
            fm = FIELD_RE.match(line)
            if fm:
                fields.append((fm.group(1), fm.group(2).strip()))
        out[name] = (bool(lt), fields)
    return out


ORIGIN_RE = re.compile(r'StructureInfo \{\s*stype: -?\d+,\s*name: "(\w+)",\s*'
                       r'core: (None|Some\((0x[0-9a-f]+)\)),\s*extensions: &\[([^\]]*)\],?\s*\}')


def admitted_structures():
    text = (PROTOCOL / 'info.rs').read_text(encoding='utf-8')
    out = set()
    for m in ORIGIN_RE.finditer(text):
        core = int(m.group(3), 16) if m.group(3) else None
        exts = set(re.findall(r'"(\w+)"', m.group(4)))
        if (core is not None and core <= ADMITTED_API) or exts & ADMITTED_EXTENSIONS:
            out.add(m.group(1))
    if not out:
        sys.exit('no StructureInfo entries parsed from protocol/info.rs')
    return out


def check_executor_commands():
    text = CONTEXT.read_text(encoding='utf-8')
    body = text[text.index('fn dispatch('):]
    body = body[:body.index('\n    }\n')]
    arms = re.findall(r'Command::(\w+)\(args\)', body)
    if sorted(arms) != sorted(EXECUTOR_COMMANDS):
        sys.exit('EXECUTOR_COMMANDS disagrees with the dispatch match in %s:\n  script: %s\n  match:  %s'
                 % (CONTEXT, sorted(EXECUTOR_COMMANDS), sorted(arms)))


def proto_structs():
    structs, aliases, handles, enums = {}, {}, set(), {}
    for f in sorted(PROTOCOL.glob('*.rs')):
        text = f.read_text(encoding='utf-8')
        for m in re.finditer(r'^pub type (Vk\w+) = (\w+);', text, re.M):
            aliases[m.group(1)] = m.group(2)
        for m in re.finditer(r'^pub struct (Vk\w+)\(pub u64\);', text, re.M):
            handles.add(m.group(1))
        for name, (_, fields) in parse_structs(text).items():
            if name.startswith('Vk') or name.endswith('Args'):
                structs[name] = fields
        for m in re.finditer(r"^pub enum (Vk\w+Next)(?:<'a>)? \{\n(.*?)^\}", text, re.M | re.S):
            variants = re.findall(r'^\s*(Vk\w+)\(', m.group(2), re.M)
            enums[m.group(1)] = variants
    return structs, aliases, handles, enums


ARRAY_RE = re.compile(r'^\[(\w+); (\w+)\]$')


class Gen:
    def __init__(self):
        check_executor_commands()
        self.pstructs, self.aliases, self.handles, self.enums = proto_structs()
        self.admitted = admitted_structures()
        self.enums = {k: [v for v in vs if v in self.admitted] for k, vs in self.enums.items()}
        self.astructs = parse_structs(find_ash().read_text(encoding='utf-8'))
        self.used = self.executor_structs()
        self.convertible = {}
        self.failures = {}

    def executor_structs(self):
        """Every protocol structure reachable from the executor's commands,
        through members and admitted pNext links."""
        seen = set()
        todo = ['%sArgs' % c for c in EXECUTOR_COMMANDS]
        for name in todo:
            if name not in self.pstructs:
                sys.exit(f'{name} is not in the generated protocol')
        while todo:
            name = todo.pop()
            if name in seen:
                continue
            seen.add(name)
            for _field, ty in self.pstructs.get(name, []):
                for ident in re.findall(r'\b(Vk\w+)', ty):
                    if ident in self.enums:
                        todo.extend(self.enums[ident])
                    elif ident in self.pstructs:
                        todo.append(ident)
        return {n for n in seen if n.startswith('Vk')}

    def ash_name(self, pname):
        return pname[2:]

    def proto_kind(self, ty):
        if ty in PROTO_SCALARS:
            return ('scalar', ty)
        if ty in self.aliases:
            return ('scalar', self.aliases[ty])
        if ty in self.handles:
            return ('handle', ty)
        m = ARRAY_RE.match(ty)
        if m:
            elem = m.group(1)
            k = self.proto_kind(elem)
            return ('array', k, m.group(2))
        if ty in self.pstructs:
            return ('struct', ty)
        return ('other', ty)

    def ash_kind(self, ty):
        if ty in ASH_SCALARS:
            return ('scalar', ty)
        if ty == 'c_char':
            return ('c_char', ty)
        if ty.startswith('*') or ty.startswith('PhantomData'):
            return ('other', ty)
        m = ARRAY_RE.match(ty)
        if m:
            return ('array', self.ash_kind(m.group(1)), m.group(2))
        if ty in self.astructs:
            return ('struct', ty)
        return ('newtype', ty)

    def field_exprs(self, pty, aty, src):
        """(from_ash expr, to_ash expr) for one field, or None."""
        pk, ak = self.proto_kind(pty), self.ash_kind(aty)
        if pk[0] == 'scalar':
            if ak[0] == 'scalar':
                if aty == 'usize':
                    return (f'{src} as {pk[1]}', f'{src} as usize')
                return (f'{src}', f'{src}')
            if ak[0] == 'newtype':
                # A `*FlagBits` parameter is an i32 enum on the wire and a
                # bitflags type in ash.
                if pk[1] == 'i32' and ('Flags' in aty):
                    return (f'{src}.as_raw() as i32', f'vk::{aty}::from_raw({src} as u32)')
                return (f'{src}.as_raw()', f'vk::{aty}::from_raw({src})')
            return None
        if pk[0] == 'struct' and ak[0] == 'struct':
            if not self.is_convertible(pk[1]):
                return None
            return (f'{pk[1]}::from_ash(&{src})', f'{src}.to_ash()')
        if pk[0] == 'array' and ak[0] == 'array':
            pe, ae = pk[1], ak[1]
            if pe[0] == 'scalar' and ae[0] == 'scalar':
                return (f'{src}', f'{src}')
            if pe == ('scalar', 'u8') and ae[0] == 'c_char':
                return (f'{src}.map(|c| c as u8)', f'{src}.map(|b| b as c_char)')
            if pe[0] == 'struct' and ae[0] == 'struct' and self.is_convertible(pe[1]):
                return (f'{src}.each_ref().map({pe[1]}::from_ash)',
                        f'{src}.each_ref().map(ToAsh::to_ash)')
            return None
        return None

    def is_convertible(self, pname):
        if pname in self.convertible:
            return self.convertible[pname] is not None
        self.convertible[pname] = None  # cycle guard
        aname = self.ash_name(pname)
        if aname not in self.astructs:
            self.failures[pname] = f'no ash struct {aname}'
            return False
        lt, afields = self.astructs[aname]
        amap = {norm(n if n != 'ty' else 'type'): (n, t) for n, t in afields}
        plan = []
        for pfield, pty in self.pstructs[pname]:
            if pfield == 'p_next':
                continue
            key = norm(pfield.rstrip('_'))
            if key not in amap:
                self.failures[pname] = f'field {pfield} has no ash twin'
                return False
            afield, aty = amap[key]
            exprs = self.field_exprs(pty, aty, f'src.{afield}')
            exprs_to = self.field_exprs(pty, aty, f'src.{pfield}')
            if exprs is None or exprs_to is None:
                self.failures[pname] = f'field {pfield}: {pty} <-> {aty}'
                return False
            plan.append((pfield, afield, exprs[0], exprs_to[1]))
        has_pnext = any(f == 'p_next' for f, _ in self.pstructs[pname])
        self.convertible[pname] = (aname, lt, plan, has_pnext)
        return True

    def emit(self):
        for name in sorted(self.used):
            self.is_convertible(name)
        out = []
        w = out.append
        w('// @generated by scripts/venus-ash-gen.py from the generated Venus protocol')
        w(f'// (crates/virtio-gpu/src/venus/protocol/) and ash {ASH_VERSION}. DO NOT EDIT:')
        w('// regenerate with `python scripts/venus-ash-gen.py`.')
        w('')
        w('//! Field-for-field conversion between the Venus protocol structures and')
        w('//! their `ash` twins, and the two whole-chain host queries built on it.')
        w('//!')
        w('//! Everything here is plain data movement except the two `unsafe` host')
        w('//! calls in `query_features2` / `query_properties2`, whose argument is a')
        w('//! chain of structures this file owns for the duration of the call.')
        w('')
        w('#![allow(clippy::all, dead_code, unused_imports)]')
        w('')
        w('use core::ffi::c_char;')
        w('')
        w('use ash::vk;')
        w('')
        w('use crate::venus::protocol::*;')
        w('')
        w('/// A protocol structure built from its `ash` twin (host answer -> guest).')
        w('pub trait FromAsh<A> {')
        w('    /// Every field copied; a pNext chain is left empty.')
        w('    fn from_ash(src: &A) -> Self;')
        w('}')
        w('')
        w('/// The `ash` twin of a protocol structure (guest request -> host).')
        w('pub trait ToAsh {')
        w('    /// The `ash` structure, `p_next` null.')
        w('    type Ash;')
        w('    /// Every field copied.')
        w('    fn to_ash(&self) -> Self::Ash;')
        w('}')
        w('')
        done = []
        for name in sorted(self.convertible):
            info = self.convertible[name]
            if info is None:
                continue
            aname, lt, plan, has_pnext = info
            aty = f"vk::{aname}<'_>" if lt else f'vk::{aname}'
            aty_static = f"vk::{aname}<'static>" if lt else f'vk::{aname}'
            w(f'impl FromAsh<{aty}> for {name} {{')
            w(f'    fn from_ash(src: &{aty}) -> Self {{')
            w('        Self {')
            if has_pnext:
                w('            p_next: Vec::new(),')
            for pfield, _afield, fexpr, _texpr in plan:
                w(f'            {pfield}: {fexpr},')
            w('        }')
            w('    }')
            w('}')
            w('')
            w(f'impl ToAsh for {name} {{')
            w(f'    type Ash = {aty_static};')
            w(f'    fn to_ash(&self) -> {aty_static} {{')
            w('        let src = self;')
            w(f'        vk::{aname} {{')
            for pfield, afield, _fexpr, texpr in plan:
                w(f'            {afield}: {texpr},')
            w('            ..Default::default()')
            w('        }')
            w('    }')
            w('}')
            w('')
            done.append(name)

        self.emit_query(w, 'VkPhysicalDeviceFeatures2Next', 'query_features2',
                        'PhysicalDeviceFeatures2', 'features', 'VkPhysicalDeviceFeatures',
                        'VkPhysicalDeviceFeatures2', 'get_physical_device_features2')
        self.emit_query(w, 'VkPhysicalDeviceProperties2Next', 'query_properties2',
                        'PhysicalDeviceProperties2', 'properties', 'VkPhysicalDeviceProperties',
                        'VkPhysicalDeviceProperties2', 'get_physical_device_properties2')
        self.emit_device_links(w)

        return '\n'.join(out) + '\n', done, self.failures

    def emit_query(self, w, enum, fname, ahead, core_field, core_ty, pty, call):
        variants = self.enums[enum]
        for v in variants:
            if v not in CORE_VERSION:
                sys.exit(f'{enum}::{v} has no core version in CORE_VERSION')
            if self.convertible.get(v) is None:
                sys.exit(f'{enum}::{v} is not convertible: {self.failures.get(v)}')
        w(f'/// `{call}` with every structure `{enum}` admits that')
        w('/// the device\'s version knows, chained at once; the whole chain back, in')
        w('/// protocol form. `api` is `min(device apiVersion, 1.3)`.')
        w('///')
        w('/// # Safety')
        w('/// `pd` must be a physical device enumerated from `instance`.')
        w(f'pub unsafe fn {fname}(instance: &ash::Instance, pd: vk::PhysicalDevice, api: u32) -> {pty} {{')
        for i, v in enumerate(variants):
            w(f'    let mut l{i} = vk::{self.ash_name(v)}::default();')
        for i, v in enumerate(variants):
            ver = CORE_VERSION[v]
            w(f'    let on{i} = api >= vk::API_VERSION_1_{ver % 10};')
        w(f'    let mut head = vk::{ahead}::default();')
        for i, _ in enumerate(variants):
            w(f'    if on{i} {{')
            w(f'        head = head.push_next(&mut l{i});')
            w('    }')
        w('    // SAFETY: the caller vouches for `pd`; `head` and every structure')
        w('    // chained onto it are locals of this function, correctly typed and')
        w('    // initialised by `Default` (sType set, pNext null) before `push_next`')
        w('    // linked them, and all of them outlive the call.')
        w(f'    unsafe {{ instance.{call}(pd, &mut head) }};')
        w(f'    let core = {core_ty}::from_ash(&head.{core_field});')
        w('    let mut p_next = Vec::new();')
        for i, v in enumerate(variants):
            w(f'    if on{i} {{')
            w(f'        p_next.push({enum}::{v}({v}::from_ash(&l{i})));')
            w('    }')
        w(f'    {pty} {{ p_next, {core_field}: core }}')
        w('}')
        w('')

    def emit_device_links(self, w):
        enum = 'VkDeviceCreateInfoNext'
        variants = [v for v in self.enums[enum] if v != 'VkDeviceGroupDeviceCreateInfo']
        for v in variants:
            if self.convertible.get(v) is None:
                sys.exit(f'{enum}::{v} is not convertible: {self.failures.get(v)}')
        w('/// The owned `ash` twins of a `VkDeviceCreateInfo` pNext chain, minus')
        w('/// `VkDeviceGroupDeviceCreateInfo`, whose handles only the caller can')
        w('/// translate. Only the links the executor admits have a twin here.')
        w('#[derive(Default)]')
        w('pub struct DeviceLinks {')
        for i, v in enumerate(variants):
            _a, lt, _p, _h = self.convertible[v]
            aty = f"vk::{self.ash_name(v)}<'static>" if lt else f'vk::{self.ash_name(v)}'
            w(f'    l{i}: Option<{aty}>,')
        w('}')
        w('')
        w('impl DeviceLinks {')
        w('    /// Convert every link but the device-group one.')
        w('    ///')
        w('    /// # Errors')
        w('    /// The `sType` of a link with no twin here: one the executor does')
        w('    /// not admit, which it refuses before a host is asked.')
        w(f'    pub fn new(links: &[{enum}]) -> Result<Self, i32> {{')
        w('        let mut out = Self::default();')
        w('        for link in links {')
        w('            match link {')
        for i, v in enumerate(variants):
            w(f'                {enum}::{v}(v) => out.l{i} = Some(v.to_ash()),')
        w(f'                {enum}::VkDeviceGroupDeviceCreateInfo(_) => {{}}')
        w('                other => return Err(ChainLink::structure_type(other)),')
        w('            }')
        w('        }')
        w('        Ok(out)')
        w('    }')
        w('')
        w('    /// Link every converted structure onto `info`.')
        w("    pub fn push<'a>(&'a mut self, mut info: vk::DeviceCreateInfo<'a>) -> vk::DeviceCreateInfo<'a> {")
        for i, _ in enumerate(variants):
            w(f'        if let Some(l) = self.l{i}.as_mut() {{')
            w('            info = info.push_next(l);')
            w('        }')
        w('        info')
        w('    }')
        w('}')


def edition():
    text = (ROOT / 'Cargo.toml').read_text(encoding='utf-8')
    m = re.search(r'^edition\s*=\s*"(\d+)"', text, re.M)
    return m.group(1) if m else '2021'


def formatted(text):
    with tempfile.TemporaryDirectory() as tmp:
        path = Path(tmp) / 'convert.rs'
        path.write_text(text, encoding='utf-8', newline='\n')
        subprocess.run(['rustfmt', '--edition', edition(), str(path)], check=True)
        return path.read_text(encoding='utf-8')


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument('--check', action='store_true')
    ap.add_argument('--report', action='store_true', help='list structures left unconverted')
    args = ap.parse_args()
    text, done, failures = Gen().emit()
    text = formatted(text)
    if args.report:
        for name in sorted(failures):
            print(f'unconverted {name}: {failures[name]}')
        print(f'{len(done)} converted')
    if args.check:
        current = OUT.read_text(encoding='utf-8') if OUT.exists() else ''
        if current != text:
            print(f'{OUT} is stale; regenerate', file=sys.stderr)
            return 1
        return 0
    OUT.parent.mkdir(parents=True, exist_ok=True)
    OUT.write_text(text, encoding='utf-8', newline='\n')
    print(f'wrote {OUT} ({len(done)} structures)')
    return 0


if __name__ == '__main__':
    sys.exit(main())
