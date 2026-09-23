#!/usr/bin/env python3

# Copyright 2020 Google LLC
# SPDX-License-Identifier: MIT

from pathlib import Path
import sys

VN_PROTOCOL_DIR = Path(__file__).resolve().parent.parent
sys.path.append(str(VN_PROTOCOL_DIR))

from vkxml import VkRegistry
from vn_protocol import VN_PROTOCOL_VK_XML, VN_PROTOCOL_PRIVATE_XMLS

def main():
    reg = VkRegistry.parse(VN_PROTOCOL_VK_XML, VN_PROTOCOL_PRIVATE_XMLS)

    wsi = [
        'VK_KHR_display',
        'VK_KHR_surface',
        'VK_KHR_swapchain',
    ]

    exts = {}
    for ext in reg.extensions:
        if ext.supported != 'vulkan':
            continue

        is_wsi = ext.name in wsi
        if not is_wsi and ext.requires:
            for req in ext.requires:
                if req in wsi:
                    wsi.append(ext.name)
                    is_wsi = True
                    break

        if is_wsi and ext.name.startswith('VK_KHR_'):
            key = 'WSI'
        else:
            key = ext.promoted

        if key not in exts:
            exts[key] = []
        exts[key].append(ext)

    for key in exts:
        exts[key].sort(key=lambda ext: ext.name)

        print(key)
        for ext in exts[key]:
            line = '      .%s = true, ' % ext.name
            pad = 59 - len(line)
            if pad > 0:
                line += ' ' * pad
            line += '/* specVersion ' + str(ext.version)
            if ext.platform:
                line += ', %s' % ext.platform
            line += ' */'

            print(line)

if __name__ == '__main__':
    main()
