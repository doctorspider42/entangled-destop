#!/usr/bin/env python3

# Copyright 2020 Google LLC
# SPDX-License-Identifier: MIT

from pathlib import Path
import sys

VN_PROTOCOL_DIR = Path(__file__).resolve().parent.parent
sys.path.append(str(VN_PROTOCOL_DIR))

from vkxml import VkRegistry
from vn_protocol import VK_XML_EXTENSION_LIST, VN_PROTOCOL_VK_XML, VN_PROTOCOL_PRIVATE_XMLS, Gen

class Command:
    def __init__(self, ty):
        self.ty = ty
        self.name = self._enum_name(ty.name)
        self.id = None

        self.other_names = []
        for alias in ty.aliases:
            self.other_names.append(self._enum_name(alias))

    def assign_id(self, known_ids, next_id):
        # assign from XML first
        for name in [self.name] + self.other_names:
            if name in known_ids:
                self.id = known_ids[name]
                break

        # assign a new id
        if not self.id:
            self.id = str(next_id)
            next_id += 1

        return next_id

    @staticmethod
    def _enum_name(name):
        return 'VK_COMMAND_TYPE_%s_EXT' % name

class Group:
    def __init__(self, name, commands):
        self.name = name
        self.commands = commands

    def assign_ids(self, known_ids, next_id):
        for cmd in self.commands:
            next_id = cmd.assign_id(known_ids, next_id)
        return next_id

def get_commands(reg):
    all_commands = set()
    groups = []

    for feat in reg.features:
        commands = []
        for ty in feat.types:
            if ty.category == ty.COMMAND and ty not in all_commands:
                commands.append(Command(ty))
                all_commands.add(ty)
        groups.append(Group(feat.name, commands))

    for ext in reg.extensions:
        if ext.name not in VK_XML_EXTENSION_LIST:
            continue

        commands = []
        for ty in ext.types:
            if ty.category == ty.COMMAND and ty not in all_commands:
                commands.append(Command(ty))
                all_commands.add(ty)
        for opt in ext.optional_types:
            if not Gen.support_type_depends(opt):
                continue
            for ty in ext.optional_types[opt]:
                if ty.category == ty.COMMAND and ty not in all_commands:
                    commands.append(Command(ty))
                    all_commands.add(ty)
        if commands:
            groups.append(Group(ext.name, commands))

    return groups

def print_commands(name, groups):
    print('    <enums name="%s" type="enum">' % name)

    for group in groups:
        if group != groups[0]:
            print()
        print('        <comment>%s</comment>' % group.name)
        for cmd in group.commands:
            spaces = ' ' * (6 - len(cmd.id))
            print('        <enum value="%s"%sname="%s"/>' % (cmd.id, spaces, cmd.name))
            for alias in cmd.other_names:
                spaces = ' ' * (9 + len(cmd.id) * 2)
                print('        <enum%sname="%s" alias="%s"/>' % (spaces, alias, cmd.name))

    print('    </enums>')

def main():
    reg = VkRegistry.parse(VN_PROTOCOL_VK_XML, VN_PROTOCOL_PRIVATE_XMLS)

    groups = get_commands(reg)

    # assign ids to commands
    command_type_ty = reg.type_table['VkCommandTypeEXT']
    next_id = reg.max_vk_command_type_value + 1
    for group in groups:
        next_id = group.assign_ids(command_type_ty.enums.values, next_id)

    print_commands(command_type_ty.name, groups)

if __name__ == '__main__':
    main()
