# Copyright 2020 Google LLC
# SPDX-License-Identifier: MIT

import xml.etree.ElementTree as ET

class VkDecl:
    """Parse a C-declaration like 'int* const* const blah[4]' into

    name := 'blah'
    type_name := 'int'
    type_decor.qual = 'const'
    type_decor.dim = '4'
    type_decor.ref_quals = ['', 'const']
    """

    class Decor:
        def __init__(self, qual, dim, bit_size, ref_quals):
            self.qual = qual
            self.dim = dim
            self.bit_size = bit_size
            self.ref_quals = ref_quals

    def __init__(self, name, type_name, type_decor=None):
        if not type_decor:
            type_decor = self.Decor(None, None, None, [])

        self.name = name
        self.type_name = type_name
        self.type_decor = type_decor

    def to_c(self, type_only):
        c_decl = self.type_name

        quals = self.type_decor.ref_quals[:]
        quals.append(self.type_decor.qual)
        for i, qual in enumerate(quals):
            is_first = i == 0
            is_last = i == len(quals) - 1

            if qual:
                # first qualifier is prepended
                if is_first:
                    c_decl = qual + ' ' + c_decl
                else:
                    c_decl = c_decl + ' ' + qual

            if is_last:
                if not type_only:
                    c_decl = c_decl + ' ' + self.name

                if self.type_decor.dim:
                    if c_decl[-1] == '*':
                        c_decl = c_decl + ' '
                    c_decl = c_decl + '[' + self.type_decor.dim + ']'

                if self.type_decor.bit_size:
                    c_decl = c_decl + ':' + self.type_decor.bit_size
            else:
                c_decl += '*'

        return c_decl

    @staticmethod
    def from_c(c_decl):
        """This is very limited."""
        # extract bit size
        bit_size = None
        index = c_decl.find(':')
        if index != -1:
            bit_size = c_decl[index + 1:].strip()
            c_decl = c_decl[:index]

        # extract array size
        array_size = None
        index = c_decl.find('[')
        if index != -1:
            array_size = c_decl[index + 1:c_decl.rfind(']')].strip()
            c_decl = c_decl[:index]

        # extract name
        end = len(c_decl)
        while not c_decl[end - 1].isalnum():
            end -= 1
        index = c_decl.rfind(' ', 0, end)
        name = c_decl[index + 1:end]
        c_decl = c_decl[:index]

        # extract base type which is always before the first '*'
        quals = c_decl.split('*')
        qualified_type_name = quals[0].split()
        type_name = qualified_type_name.pop()
        quals[0] = ' '.join(qualified_type_name)

        ref_quals = [qual.strip() for qual in quals]
        qual = ref_quals.pop()
        type_decor = VkDecl.Decor(qual, array_size, bit_size, ref_quals)

        return VkDecl(name, type_name, type_decor)

class VkVariable:
    def __init__(self, ty, name='unnamed', attrs={}):
        self.ty = ty
        self.name = name
        self.attrs = attrs

    def can_validate(self):
        if 'noautovalidity' in self.attrs and \
                self.attrs['noautovalidity'] == 'true':
            return False
        return True

    def maybe_null(self):
        return self.ty.is_pointer() and self.is_optional()

    def is_optional(self):
        return 'optional' in self.attrs and \
                self.attrs['optional'][0] == 'true'

    def is_blob(self):
        return self.ty.indirection_depth() == 1 and \
               not self.ty.is_static_array() and \
               self.ty.base.name == 'void' and  \
               'len_exprs' in self.attrs

    def is_dynamic_array(self):
        return self.ty.is_pointer() and 'len_exprs' in self.attrs

    def has_c_string(self):
        """Return True for C-strings and arrays of C-strings."""
        if not self.is_dynamic_array():
            return False
        for len_expr in self.attrs['len_exprs']:
            if len_expr == 'null-terminated':
                return True
        return False

    def is_p_next(self):
        return self.name == 'pNext'

    def to_c(self):
        return VkDecl(self.name, self.ty.base.name, self.ty.decor).to_c(False)

TYPE_BLOCK_LIST = [
    # Block layering structs from VK_KHR_maintenance7 as the driver will
    # fill those with core property structs.
    'VkPhysicalDeviceLayeredApiPropertiesListKHR',
    'VkPhysicalDeviceLayeredApiPropertiesKHR',
    'VkPhysicalDeviceLayeredApiKHR',
    'VkPhysicalDeviceLayeredApiVulkanPropertiesKHR',
]

class VkType:
    INCLUDE        = 0
    DEFINE         = 1
    DEFAULT        = 2
    BASETYPE       = 3
    HANDLE         = 4
    ENUM           = 5
    BITMASK        = 6
    STRUCT         = 7
    UNION          = 8
    FUNCPOINTER    = 9
    COMMAND        = 10
    DERIVED        = 11
    CATEGORY_COUNT = 12

    def __init__(self):
        self.name = None
        self.category = None
        self.base = None

        self.aliases = []
        self.ext_aliases = {}

        self.attrs = {}

        # True if defined by private XMLs
        self.is_private = None

        # for DEFINE
        self.define = None

        # for BASETYPE/BITMASK
        self.typedef = None

        # for HANDLE
        self.dispatchable = None

        # for ENUM
        self.enums = None

        # for BITMASK (optional)
        self.requires = None

        # for STRUCT (optional)
        self.s_type = None
        self._struct_extends = []
        self.p_next = []

        # for FUNCPOINTER/COMMAND
        self.ret = None
        self.can_device_lost = False
        self.has_out_ty = False

        # for STRUCT/UNION/FUNCPOINTER/COMMAND
        self.variables = []

        # for DERIVED
        self.decor = None

        # selector type for UNION
        self.sty = None

    def init(self, name, category):
        assert name and self.name is None
        assert category < self.CATEGORY_COUNT and self.category is None
        self.name = name
        self.category = category
        if category != self.DERIVED:
            self.base = self

    def validate(self):
        assert self.name is not None
        assert self.category is not None
        assert self.base == self.base.base

        if self.category != self.INCLUDE:
            assert self.base.name.isidentifier()

        if self.category == self.DERIVED:
            assert self.base != self
            assert self.decor
        else:
            assert self.base == self
            assert self.decor is None
            if self.category == self.BASETYPE and self.typedef:
                assert self.typedef == self.typedef.base

        if self.s_type:
            assert self.variables[0].name == 'sType'
            assert self.variables[1].name == 'pNext'

    def is_static_array(self):
        return bool(self.decor.dim) if self.decor else False

    def static_array_size(self):
        return self.decor.dim if self.decor else None

    def is_pointer(self):
        return bool(self.decor.ref_quals) if self.decor else False

    def indirection_depth(self):
        return len(self.decor.ref_quals) if self.decor else 0

    def is_const_static_array(self):
        return self.is_static_array() and 'const' in self.decor.qual

    def is_const_pointer(self):
        if self.is_pointer():
            for qual in self.decor.ref_quals:
                if 'const' in qual:
                    return True
        return False

    def is_c_string(self):
        """Return True if the type is a C-string.

        This does not include char arrays or arrays of C-strings.
        """
        return self.base.name == 'char' and self.indirection_depth() == 1

    def is_valid_union(self):
        if self.category != self.UNION:
            return False

        for var in self.variables:
            if not 'selection' in var.attrs:
                return False
        return True

    def get_union_cases(self):
        if not self.is_valid_union():
            return enumerate(self.variables)

        cases = []
        for var in self.variables:
            for s in var.attrs['selection']:
                cases.append((s, var))
        return cases

    def might_contain_handle(self):
        return (self.category == VkType.HANDLE or
                self.category == VkType.STRUCT or
                self.is_valid_union())

    def find_variables(self, len_name):
        names = len_name.split('->')

        # TODO: formalize the handling of ppBuildRangeInfos
        if '[i].' in len_name:
            names = len_name.split('[i].')

        var_list = []
        for name in names:
            if var_list:
                candidates = var_list[-1].ty.base.variables
            else:
                candidates = self.base.variables

            found = None
            for var in candidates:
                if var.name == name:
                    found = var
                    break
            if not found:
                return []
            var_list.append(found)

        return var_list

    def _add_deps(self, deps):
        ty = self.base

        if ty.typedef:
            ty.typedef._add_deps(deps)
        if ty.requires:
            ty.requires._add_deps(deps)

        # ty.p_next is not a dependency

        if ty.ret:
            ty.ret.ty._add_deps(deps)
        for var in ty.variables:
            if var.ty.base != ty:
                var.ty._add_deps(deps)

        if ty not in deps:
            deps.append(ty)

    def get_dependencies(self):
        deps = []
        self._add_deps(deps)
        return deps

    def set_attribute(self, key, val):
        ty = self.base

        # Avoid circular reference.
        # VkBaseOutStructure refers to itself in
        # VkBaseOutStructure* VkBaseOutStructure::pNext
        if ty.name == 'VkBaseOutStructure':
            return

        ty.attrs[key] = val
        for var in ty.variables:
            var.ty.set_attribute(key, val)
        for next_ty in ty.p_next:
            next_ty.set_attribute(key, val)

    def c_func_ret(self):
        return self.ret.ty.name if self.ret else 'void'

    def c_func_params(self, separator=', '):
        c_params = [var.to_c() for var in self.variables]
        return separator.join(c_params)

    def c_func_args(self, separator=', '):
        c_args = [var.name for var in self.variables]
        return separator.join(c_args)

    @staticmethod
    def _get_inner_text(elem):
        text = []
        if elem.text:
            text.append(elem.text)
        for child in elem:
            if child.tag == 'comment':
                continue
            if child.text:
                text.append(child.text)
            if child.tail:
                text.append(child.tail)
        return ' '.join(text)

    @staticmethod
    def _get_type(key, type_table):
        if isinstance(key, VkDecl):
            base_name = key.type_name
            name = key.to_c(True)
        else:
            base_name = key
            name = key

        # return the type if exists
        if name in type_table:
            return type_table[name]

        # get (or create) the base type first
        if base_name in type_table:
            base_ty = type_table[base_name]
        else:
            base_ty = VkType()
            type_table[base_name] = base_ty

        # this is a base type
        if name == base_name:
            return base_ty

        # create the derived type
        ty = VkType()
        ty.init(name, ty.DERIVED)
        ty.base = base_ty
        ty.decor = key.type_decor
        type_table[name] = ty

        return ty

    @staticmethod
    def _parse_alias(type_elem, type_table):
        name = type_elem.attrib['name']
        alias = type_elem.attrib['alias']

        assert name not in type_table
        ty = VkType._get_type(alias, type_table)
        ty.aliases.append(name)
        type_table[name] = ty

    @staticmethod
    def _parse_variable(elem, type_table):
        c_decl = VkType._get_inner_text(elem)
        decl = VkDecl.from_c(c_decl)

        # sanity check
        assert decl.name == elem.find('name').text
        assert decl.type_name == elem.find('type').text
        enum_elem = elem.find('enum')
        if enum_elem is not None:
            assert decl.type_decor.dim == enum_elem.text

        ty = VkType._get_type(decl, type_table)

        attrs = {}
        if 'values' in elem.attrib:
            attrs['values'] = elem.attrib['values'].split(',')
        if 'len' in elem.attrib:
            lens = []
            if 'altlen' in elem.attrib:
                lens = elem.attrib['altlen'].split(',')
            else:
                lens = elem.attrib['len'].split(',')

                # TODO: drop the workaround after registry has nested array info
                # Ideally, pInfos[].geometryCount is good so the gen script here
                # can fill the iter dynamically instead of hard-coding an 'i'.
                if decl.name in ['ppBuildRangeInfos', 'ppMaxPrimitiveCounts']:
                    lens.append('pInfos[i].geometryCount')

            len_exprs = []
            len_names = []
            for l in lens:
                if l == 'null-terminated':
                    len_exprs.append(l)
                    len_names.append('')
                    continue

                len_exprs.append(l)

                # extract "thisFoo->thatBar"
                begin = 0
                while begin < len(l):
                    if l[begin].islower():
                        break
                    begin += 1

                end = begin + 1
                while end < len(l):
                    if l[end].isidentifier() or l[end].isdigit():
                        end += 1
                    elif l[end:end + 2] == '->':
                        end += 2
                    elif l[end:end + 4] == '[i].':
                        # TODO formalize the handling of ppBuildRangeInfos
                        end += 4
                    else:
                        break

                len_names.append(l[begin:end])

            if len_exprs:
                attrs['len_exprs'] = len_exprs
                attrs['len_names'] = len_names
        if 'optional' in elem.attrib:
            attrs['optional'] = elem.attrib['optional'].split(',')
        if 'noautovalidity' in elem.attrib:
            attrs['noautovalidity'] = elem.attrib['noautovalidity']
        if 'stride' in elem.attrib:
            attrs['stride'] = elem.attrib['stride']
        if 'selector' in elem.attrib:
            attrs['selector'] = elem.attrib['selector']
        if 'selection' in elem.attrib:
            attrs['selection'] = elem.attrib['selection'].split(',')
        # workaround for backcompat of static array with API constant size
        if enum_elem is not None:
            attrs['wa_require_static_len'] = enum_elem.text

        return VkVariable(ty, decl.name, attrs)

    @staticmethod
    def _parse_type_define(ty, type_elem, type_table):
        # get and save the #define macro
        ty.define = VkType._get_inner_text(type_elem)

    @staticmethod
    def _parse_type_basetype(ty, type_elem, type_table):
        typedef_elem = type_elem.find('type')
        if typedef_elem is not None:
            ty.typedef = VkType._get_type(typedef_elem.text, type_table)

    @staticmethod
    def _parse_type_handle(ty, type_elem, type_table):
        if type_elem.find('type').text == 'VK_DEFINE_HANDLE':
            ty.dispatchable = True

    @staticmethod
    def _parse_type_enum(ty, type_elem, type_table):
        # enum values will be filled in when <enums> is parsed
        ty.enums = VkEnums()

    @staticmethod
    def _parse_type_bitmask(ty, type_elem, type_table):
        to = type_elem.find('type').text
        assert to in ['VkFlags', 'VkFlags64']

        requires_ty = None
        if 'requires' in type_elem.attrib:
            requires = type_elem.attrib['requires']
            requires_ty = VkType._get_type(requires, type_table)
        elif 'bitvalues' in type_elem.attrib:
            requires = type_elem.attrib['bitvalues']
            requires_ty = VkType._get_type(requires, type_table)

        ty.typedef = VkType._get_type(to, type_table)
        ty.requires = requires_ty

    @staticmethod
    def _sort_struct_members(members):
        """Ensures dynamic array length is reordered before the array.

        VkHostAddressRangeEXT and VkHostAddressRangeConstEXT have placed size
        member after address. With this helper, the dynamic array size can be
        validated properly.
        """
        sorted_members = []
        for var in members:
            # skip reordered len_var
            if var in sorted_members:
                continue
            if 'len_names' not in var.attrs:
                sorted_members.append(var)
                continue
            for len_name in var.attrs['len_names']:
                # consider nested dynamic array
                len_name = len_name.split('[')[0]
                len_var_found = False
                for sorted_var in sorted_members:
                    if sorted_var.name == len_name:
                        len_var_found = True
                        break
                if not len_var_found:
                   for len_var in members:
                       if len_var.name == len_name:
                           sorted_members.append(len_var)
                           break
            sorted_members.append(var)
        return sorted_members

    @staticmethod
    def _parse_type_struct(ty, type_elem, type_table):
        members = []
        for member_elem in type_elem.iterfind('member'):
            var = VkType._parse_variable(member_elem, type_table)
            members.append(var)

        # union type only has selection info, so we find the selector type info here
        for var in members:
            if 'selector' in var.attrs:
                for s in members:
                    if s.name == var.attrs['selector']:
                        var.ty.sty = s.ty
                        break

        s_type = None
        if members[0].name == 'sType' and 'values' in members[0].attrs:
            s_type = members[0].attrs['values'][0]

        struct_extends = []
        if 'structextends' in type_elem.attrib:
            struct_extends = type_elem.attrib['structextends'].split(',')

        returnedonly = type_elem.attrib.get(
                'returnedonly', 'false') != 'false'

        ty.variables = VkType._sort_struct_members(members)
        ty.s_type = s_type
        ty._struct_extends = struct_extends
        if returnedonly:
            ty.attrs['returnedonly'] = True

    @staticmethod
    def _parse_type_union(ty, type_elem, type_table):
        VkType._parse_type_struct(ty, type_elem, type_table)

    @staticmethod
    def _parse_command_decls(ty, elem, type_table):
        params = []
        ret_ty = None
        for child in elem:
            if child.tag == 'proto':
                c_decl = VkType._get_inner_text(child)
                decl = VkDecl.from_c(c_decl)
                ret_ty = VkType._get_type(decl, type_table)
                if ret_ty.name == 'void':
                    ret_ty = None
            elif child.tag == 'param':
                var = VkType._parse_variable(child, type_table)
                params.append(var)

        ty.variables = params
        if ret_ty:
            ty.ret = VkVariable(ret_ty, 'ret')

    @staticmethod
    def _parse_legacy_funcpointer(ty, type_elem, type_table):
        c_decls = VkType._get_inner_text(type_elem).splitlines()

        # clean up the first line to abuse VkDecl
        c_decl = c_decls.pop(0)
        assert c_decl.startswith('typedef ')
        c_decl = c_decl[8:]
        c_decl = c_decl.replace('(VKAPI_PTR * ', '', 1)
        index = c_decl.rfind('(')
        c_decl = c_decl[:index]

        decl = VkDecl.from_c(c_decl)
        assert ty.name == decl.name

        ret_ty = VkType._get_type(decl, type_table)
        if ret_ty.name == 'void':
            ret_ty = None

        params = []
        for c_decl in c_decls:
            decl = VkDecl.from_c(c_decl)
            param_ty = VkType._get_type(decl, type_table)
            params.append(VkVariable(param_ty, decl.name))

        ty.variables = params
        if ret_ty:
            ty.ret = VkVariable(ret_ty, 'ret')

    @staticmethod
    def _parse_type_funcpointer(ty, type_elem, type_table):
        if type_elem.find('proto') is None:
            VkType._parse_legacy_funcpointer(ty, type_elem, type_table)
        else:
            VkType._parse_command_decls(ty, type_elem, type_table)

    @staticmethod
    def parse_type(type_elem, type_table):
        """Parse <type> into a VkType."""
        if 'alias' in type_elem.attrib:
            VkType._parse_alias(type_elem, type_table)
            return

        category, parse_func = {
            'include':     (VkType.INCLUDE,     None),
            'define':      (VkType.DEFINE,      VkType._parse_type_define),
            None:          (VkType.DEFAULT,     None),
            'basetype':    (VkType.BASETYPE,    VkType._parse_type_basetype),
            'handle':      (VkType.HANDLE,      VkType._parse_type_handle),
            'enum':        (VkType.ENUM,        VkType._parse_type_enum),
            'bitmask':     (VkType.BITMASK,     VkType._parse_type_bitmask),
            'struct':      (VkType.STRUCT,      VkType._parse_type_struct),
            'union':       (VkType.UNION,       VkType._parse_type_union),
            'funcpointer': (VkType.FUNCPOINTER, VkType._parse_type_funcpointer),
        }[type_elem.attrib.get('category')]

        if 'name' in type_elem.attrib:
            name = type_elem.attrib['name']
        elif type_elem.find('name') is not None:
            name = type_elem.find('name').text
        else:
            assert type_elem.attrib['category'] == 'funcpointer'
            name = type_elem.find('proto').find('name').text

        if name in TYPE_BLOCK_LIST:
            return

        ty = VkType._get_type(name, type_table)
        ty.init(name, category)
        if parse_func:
            parse_func(ty, type_elem, type_table)

    @staticmethod
    def parse_command(command_elem, type_table):
        """Parse <command> into a VkType."""
        if 'alias' in command_elem.attrib:
            VkType._parse_alias(command_elem, type_table)
            return

        name = command_elem.find('proto').find('name').text
        ty = VkType._get_type(name, type_table)
        ty.init(name, ty.COMMAND)

        VkType._parse_command_decls(ty, command_elem, type_table)

        if 'errorcodes' in command_elem.attrib:
            errorcodes = command_elem.attrib['errorcodes'].split(',')
            ty.can_device_lost = 'VK_ERROR_DEVICE_LOST' in errorcodes

class VkEnums:
    """Represent a <enums>."""

    def __init__(self):
        self.bitwidth = 32
        self.values = {}
        self.vk_xml_values = None

    def init_values(self, bitwidth, values):
        assert not self.values
        self.bitwidth = bitwidth
        self.values = values

    def extend_value(self, enum_elem, ext_number):
        key, val = self._parse_enum(enum_elem, ext_number)
        if key in self.values:
            assert self.values[key] == val
        else:
            self.values[key] = val

    @staticmethod
    def _parse_enum(enum_elem, ext_number=None):
        """Parse <enum> into a (key, val) pair."""
        key = enum_elem.attrib['name']

        if 'alias' in enum_elem.attrib:
            val = enum_elem.attrib['alias']
        elif 'value' in enum_elem.attrib:
            val = enum_elem.attrib['value']
        elif 'bitpos' in enum_elem.attrib:
            bit = int(enum_elem.attrib['bitpos'])
            val = '0x%08x' % (1 << bit)
        elif 'offset' in enum_elem.attrib:
            offset = int(enum_elem.attrib['offset'])
            extnumber = int(enum_elem.attrib.get('extnumber', ext_number))
            assert extnumber > 0
            val = str(1000000000 + (extnumber - 1) * 1000 + offset)
        else:
            assert False

        if 'dir' in enum_elem.attrib:
            val = enum_elem.attrib['dir'] + val

        return key, val

    @staticmethod
    def parse_enums(enums_elem, type_table):
        """Parse <enums> and update the corresponding VkType."""
        name = enums_elem.attrib['name']
        if name in TYPE_BLOCK_LIST:
            return

        bitwidth = 32
        if 'bitwidth' in enums_elem.attrib:
            bitwidth = int(enums_elem.attrib['bitwidth'])

        values = {}
        for enum_elem in enums_elem.iterfind('enum'):
            key, val = VkEnums._parse_enum(enum_elem, values)
            assert key not in values
            values[key] = val

        # look up and update VkType
        ty = type_table[name]
        ty.enums.init_values(bitwidth, values)

class VkFeature:
    """Represent a <feature>."""

    def __init__(self, api, name, number, types):
        self.api = api
        self.name = name
        self.number = number
        self.types = types

    @staticmethod
    def parse_require(require_elem, type_table, ext_number=None):
        """Parse <require> into a list of VkType."""
        types = []
        names = []
        for child in require_elem:
            name = child.attrib['name'] if 'name' in child.attrib else ''
            if name in TYPE_BLOCK_LIST:
                continue

            if child.tag == 'enum':
                if 'extends' not in child.attrib:
                    continue
                ty = type_table[child.attrib['extends']]
                ty.enums.extend_value(child, ext_number)
            elif child.tag in ['type', 'command']:
                ty = type_table[name]
                types.append(ty)
                names.append(name)

        return (types, names)

    @staticmethod
    def parse_feature(feature_elem, type_table):
        """Parse <feature> into a VkFeature."""
        api = feature_elem.attrib['api']
        name = feature_elem.attrib['name']
        number = feature_elem.attrib['number']

        types = []
        for require_elem in feature_elem.iterfind('require'):
            require_types, _ = VkFeature.parse_require(require_elem,
                                                       type_table)
            for ty in require_types:
                if ty not in types:
                    types.append(ty)

        return VkFeature(api, name, number, types)

class VkExtension:
    """Represent a <extension>."""

    def __init__(self, name, number, supported):
        self.name = name
        self.number = int(number)
        self.supported = supported

        self.platform = None
        self.promoted = None
        self.requires = []

        self.version = 0
        self.types = []
        self.optional_types = {}

    @staticmethod
    def filter_depends(deps):
        if not deps:
            return None

        if not '+' in deps and not ',' in deps:
            if deps.startswith('VK_VERSION'):
                return None
            elif not deps.startswith('VK_'):
                # Not needed if it's an extension feature requirement
                return None
            return deps

        or_dep_list = []
        for dep in deps.split(','):
            dep = dep.strip("()")
            # Skip the "OR" dep that only requires core version
            if not '+' in dep and dep.startswith('VK_VERSION'):
                continue

            and_dep_list = []
            for and_dep in dep.split('+'):
                and_dep = and_dep.strip("()")
                filtered_sub_dep = VkExtension.filter_depends(and_dep)
                if filtered_sub_dep:
                    and_dep_list.append(filtered_sub_dep)

            if and_dep_list:
                or_dep_list.append('+'.join(and_dep_list))
            else:
                return None

        return ','.join(or_dep_list)

    @staticmethod
    def parse_extension(elem, type_table):
        """Parse <extension> into a VkExtension."""
        name = elem.attrib['name']
        number = elem.attrib['number']
        supported = elem.attrib['supported'].split(',')

        ext = VkExtension(name, number, supported)

        ext.platform = elem.attrib.get('platform')
        ext.promoted = elem.attrib.get('promotedto')

        reqs = elem.attrib.get('requires')
        if reqs:
            ext.requires = reqs.split(',')

        for require_elem in elem.iterfind('require'):
            # parse SPEC_VERSION to get the extension version
            for enum_elem in require_elem.iterfind('enum'):
                if enum_elem.attrib['name'].endswith('SPEC_VERSION'):
                    ext.version = int(enum_elem.attrib['value'])
                    break

            if 'vulkan' not in ext.supported:
                continue

            require_types, require_names = VkFeature.parse_require(
                    require_elem, type_table, number)

            # check if this <require> depends on another extension
            require_dep = VkExtension.filter_depends(require_elem.attrib.get('depends'))
            if require_dep:
                if require_dep not in ext.optional_types:
                    ext.optional_types[require_dep] = []
                types = ext.optional_types[require_dep]
            else:
                types = ext.types

            for ty, alias in zip(require_types, require_names):
                ty.ext_aliases[name] = alias
                if ty not in types:
                    types.append(ty)

        return ext

class VkRegistry:
    """Represent a <registry>."""

    def __init__(self):
        self.platform_guards = {}
        self.tags = []
        self.type_table = {}
        self.features = []
        self.extensions = []
        self.vk_xml_extension_count = 0

        self.vk_xml_version = None
        self.max_vk_command_type_value = None

    @staticmethod
    def parse(vk_xml, private_xmls=[]):
        """Parse vk.xml and optional private XMLs into a VkRegistry."""
        reg = VkRegistry()
        reg._parse_xml(vk_xml)

        reg.vk_xml_extension_count = len(reg.extensions)
        vk_xml_types = set(reg.type_table.values())
        for ty in vk_xml_types:
            if ty.category == ty.ENUM:
                ty.enums.vk_xml_values = set(ty.enums.values.keys())

        for xml in private_xmls:
            reg._parse_xml(xml)

        reg._resolve(vk_xml_types)
        reg._validate()

        return reg

    def upper_name(self, name):
        """Convert FooBarEXT to FOO_BAR_EXT."""
        suffix = ''
        for tag in self.tags:
            if name.endswith(tag):
                name = name[:-len(tag)]
                suffix = '_' + tag
                break

        underscore = "".join([c if c.islower() else '_' + c for c in name])
        return underscore.lstrip('_').upper() + suffix

    @staticmethod
    def is_vulkansc(child):
        return 'api' in child.attrib and child.attrib['api'] == 'vulkansc'

    @staticmethod
    def filter_vulkansc(root):
        root[:] = [child for child in root if not VkRegistry.is_vulkansc(child)]
        for child in root:
            VkRegistry.filter_vulkansc(child)

    def _parse_xml(self, xml):
        tree = ET.parse(xml)
        root = tree.getroot()
        self.filter_vulkansc(root)
        for child in root:
            if child.tag == 'platforms':
                self._parse_platforms(child)
            elif child.tag == 'tags':
                self._parse_tags(child)
            elif child.tag == 'types':
                self._parse_types(child)
            elif child.tag == 'enums':
                self._parse_enums(child)
            elif child.tag == 'commands':
                self._parse_commands(child)
            elif child.tag == 'feature':
                self._parse_feature(child)
            elif child.tag == 'extensions':
                self._parse_extensions(child)

    def _parse_platforms(self, platforms_elem):
        """Parse <platforms>."""
        for plat_elem in platforms_elem.iterfind('platform'):
            self.platform_guards[plat_elem.attrib['name']] = \
                    plat_elem.attrib['protect']

    def _parse_tags(self, tags_elem):
        """Parse <tags>."""
        for tag_elem in tags_elem.iterfind('tag'):
            self.tags.append(tag_elem.attrib['name'])

    def _parse_types(self, types_elem):
        """Parse <types>."""
        for type_elem in types_elem.iterfind('type'):
            VkType.parse_type(type_elem, self.type_table)

    def _parse_enums(self, enums_elem):
        """Parse <enums>.  There is one for each enumerated type."""
        if 'type' in enums_elem.attrib and enums_elem.attrib['type'] != 'constants':
            VkEnums.parse_enums(enums_elem, self.type_table)

    def _parse_commands(self, commands_elem):
        """Parse <commands>."""
        for command_elem in commands_elem.iterfind('command'):
            VkType.parse_command(command_elem, self.type_table)

    def _parse_feature(self, feature_elem):
        """Parse <feature>.  There is one for each Vulkan version."""
        feat = VkFeature.parse_feature(feature_elem, self.type_table)
        self.features.append(feat)

    def _parse_extensions(self, extensions_elem):
        """Parse <extensions>."""
        for extension_elem in extensions_elem.iterfind('extension'):
            ext = VkExtension.parse_extension(extension_elem, self.type_table)
            self.extensions.append(ext)

    def _get_xml_version(self, ver_ty, complete_ver_ty):
        ver = ver_ty.define
        ver = ver[(ver.rindex(' ') + 1):]
        assert ver.isdigit()

        complete_ver = complete_ver_ty.define
        complete_ver = complete_ver[(complete_ver.rindex('(') + 1):-1]
        complete_ver = complete_ver.replace(ver_ty.name, ver)

        if complete_ver.count(',') == 3:
            return 'VK_MAKE_API_VERSION(%s)' % complete_ver
        else:
            return 'VK_MAKE_VERSION(%s)' % complete_ver

    def _resolve(self, vk_xml_types):
        """Resolve after all XMLs are parsed."""
        for name, ty in self.type_table.items():
            # skip aliases
            if name in ty.aliases:
                continue

            ty.is_private = ty not in vk_xml_types

            # add ty to p_next of the type it extends
            if ty._struct_extends:
                extended_tys = [VkType._get_type(name, self.type_table) for name in ty._struct_extends]
                for extended_ty in extended_tys:
                    assert ty not in extended_ty.p_next
                    extended_ty.p_next.append(ty)

            if ty.category != ty.ENUM or not ty.enums.values:
                continue

            # resolve enum value aliases
            for key, val in ty.enums.values.items():
                if val not in ty.enums.values:
                    continue

                # resolve alias
                while val in ty.enums.values:
                    val = ty.enums.values[val]
                ty.enums.values[key] = val

        self.vk_xml_version = self._get_xml_version(
                self.type_table['VK_HEADER_VERSION'],
                self.type_table['VK_HEADER_VERSION_COMPLETE'])

        # sort the VkCommandTypeEXT dict, so that the driver and renderer
        # defines headers are more stable against spec registry changes
        sorted_command_type_enums = sorted(
                self.type_table['VkCommandTypeEXT'].enums.values.items(),
                key=lambda item: int(item[1]))
        self.type_table['VkCommandTypeEXT'].enums.values = dict(sorted_command_type_enums)
        self.max_vk_command_type_value = int(sorted_command_type_enums[-1][1])

    def _validate(self):
        """Sanity check."""
        for ty in self.type_table.values():
            ty.validate()

def test():
    C_DECLS = [
        'int a',
        'int* a',
        'const int a',
        'const int* a',
        'int* const a',
        'int a[3]',
        'int* a[3]',
        'const int a[3]',
        'const int* a[3]',
        'int* const a[3]',
    ]
    for c_decl in C_DECLS:
        decl = VkDecl.from_c(c_decl)
        assert decl.to_c(False) == c_decl

if __name__ == '__main__':
    test()
