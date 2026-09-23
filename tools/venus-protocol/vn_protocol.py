#!/usr/bin/env python3

# Copyright 2020 Google LLC
# SPDX-License-Identifier: MIT

import argparse
import copy
from pathlib import Path
from typing import NamedTuple

from mako.lookup import TemplateLookup
from mako.template import Template

from vkxml import VkRegistry, VkType, VkVariable

VN_PROTOCOL_DIR = Path(__file__).resolve().parent
VN_TEMPLATE_DIR = VN_PROTOCOL_DIR.joinpath('templates')
VN_TEMPLATE_LOOKUP = TemplateLookup(str(VN_TEMPLATE_DIR))

VN_PROTOCOL_VK_XML = VN_PROTOCOL_DIR.joinpath('xmls/vk.xml')
VN_PROTOCOL_PRIVATE_XMLS = [
    VN_PROTOCOL_DIR.joinpath('xmls/VK_EXT_command_serialization.xml'),
    VN_PROTOCOL_DIR.joinpath('xmls/VK_MESA_venus_protocol.xml'),
]

# This is bumped whenever a backward-incompatible change is made, and please
# ensure to clean up all the existing WAs before bumping this up.
VN_WIRE_FORMAT_VERSION = 1

# list of supported extensions
VK_XML_EXTENSION_LIST = [
    # Venus extensions
    'VK_EXT_command_serialization',
    'VK_MESA_venus_protocol',
    # promoted to VK_VERSION_1_1
    'VK_KHR_16bit_storage',
    'VK_KHR_bind_memory2',
    'VK_KHR_dedicated_allocation',
    'VK_KHR_descriptor_update_template',
    'VK_KHR_device_group',
    'VK_KHR_device_group_creation',
    'VK_KHR_external_fence',
    'VK_KHR_external_fence_capabilities',
    'VK_KHR_external_memory',
    'VK_KHR_external_memory_capabilities',
    'VK_KHR_external_semaphore',
    'VK_KHR_external_semaphore_capabilities',
    'VK_KHR_get_memory_requirements2',
    'VK_KHR_get_physical_device_properties2',
    'VK_KHR_maintenance1',
    'VK_KHR_maintenance2',
    'VK_KHR_maintenance3',
    'VK_KHR_multiview',
    'VK_KHR_relaxed_block_layout',
    'VK_KHR_sampler_ycbcr_conversion',
    'VK_KHR_shader_draw_parameters',
    'VK_KHR_storage_buffer_storage_class',
    'VK_KHR_variable_pointers',
    # promoted to VK_VERSION_1_2
    'VK_KHR_8bit_storage',
    'VK_KHR_buffer_device_address',
    'VK_KHR_create_renderpass2',
    'VK_KHR_depth_stencil_resolve',
    'VK_KHR_draw_indirect_count',
    'VK_KHR_driver_properties',
    'VK_KHR_image_format_list',
    'VK_KHR_imageless_framebuffer',
    'VK_KHR_sampler_mirror_clamp_to_edge',
    'VK_KHR_separate_depth_stencil_layouts',
    'VK_KHR_shader_atomic_int64',
    'VK_KHR_shader_float16_int8',
    'VK_KHR_shader_float_controls',
    'VK_KHR_shader_subgroup_extended_types',
    'VK_KHR_spirv_1_4',
    'VK_KHR_timeline_semaphore',
    'VK_KHR_uniform_buffer_standard_layout',
    'VK_KHR_vulkan_memory_model',
    'VK_EXT_descriptor_indexing',
    'VK_EXT_host_query_reset',
    'VK_EXT_sampler_filter_minmax',
    'VK_EXT_scalar_block_layout',
    'VK_EXT_separate_stencil_usage',
    'VK_EXT_shader_viewport_index_layer',
    # promoted to VK_VERSION_1_3
    'VK_KHR_copy_commands2',
    'VK_KHR_dynamic_rendering',
    'VK_KHR_format_feature_flags2',
    'VK_KHR_maintenance4',
    'VK_KHR_shader_integer_dot_product',
    'VK_KHR_shader_non_semantic_info',
    'VK_KHR_shader_terminate_invocation',
    'VK_KHR_synchronization2',
    'VK_KHR_zero_initialize_workgroup_memory',
    'VK_EXT_4444_formats',
    'VK_EXT_extended_dynamic_state',
    'VK_EXT_extended_dynamic_state2',
    'VK_EXT_image_robustness',
    'VK_EXT_inline_uniform_block',
    'VK_EXT_pipeline_creation_cache_control',
    'VK_EXT_pipeline_creation_feedback',
    'VK_EXT_private_data',
    'VK_EXT_shader_demote_to_helper_invocation',
    'VK_EXT_subgroup_size_control',
    'VK_EXT_texel_buffer_alignment',
    'VK_EXT_texture_compression_astc_hdr',
    'VK_EXT_tooling_info',
    'VK_EXT_ycbcr_2plane_444_formats',
    # promoted to VK_VERSION_1_4
    'VK_KHR_dynamic_rendering_local_read',
    'VK_KHR_global_priority',
    'VK_KHR_index_type_uint8',
    'VK_KHR_line_rasterization',
    'VK_KHR_load_store_op_none',
    'VK_KHR_maintenance5',
    'VK_KHR_maintenance6',
    'VK_KHR_map_memory2',
    'VK_KHR_push_descriptor',
    'VK_KHR_shader_expect_assume',
    'VK_KHR_shader_float_controls2',
    'VK_KHR_shader_subgroup_rotate',
    'VK_KHR_vertex_attribute_divisor',
    'VK_EXT_host_image_copy',
    'VK_EXT_pipeline_protected_access',
    'VK_EXT_pipeline_robustness',
    # KHR extensions
    'VK_KHR_acceleration_structure',
    'VK_KHR_calibrated_timestamps',
    'VK_KHR_compute_shader_derivatives',
    'VK_KHR_cooperative_matrix',
    'VK_KHR_deferred_host_operations',
    'VK_KHR_depth_clamp_zero_one',
    'VK_KHR_external_fence_fd',
    'VK_KHR_external_memory_fd',
    'VK_KHR_external_semaphore_fd',
    'VK_KHR_fragment_shader_barycentric',
    'VK_KHR_fragment_shading_rate',
    'VK_KHR_maintenance7',
    'VK_KHR_pipeline_library',
    'VK_KHR_ray_query',
    'VK_KHR_ray_tracing_maintenance1',
    'VK_KHR_ray_tracing_pipeline',
    'VK_KHR_ray_tracing_position_fetch',
    'VK_KHR_robustness2',
    'VK_KHR_shader_bfloat16',
    'VK_KHR_shader_clock',
    'VK_KHR_shader_fma',
    'VK_KHR_shader_maximal_reconvergence',
    'VK_KHR_shader_quad_control',
    'VK_KHR_shader_relaxed_extended_instruction',
    'VK_KHR_shader_subgroup_uniform_control_flow',
    'VK_KHR_shader_untyped_pointers',
    'VK_KHR_workgroup_memory_explicit_layout',
    # EXT extensions
    'VK_EXT_attachment_feedback_loop_dynamic_state',
    'VK_EXT_attachment_feedback_loop_layout',
    'VK_EXT_blend_operation_advanced',
    'VK_EXT_border_color_swizzle',
    'VK_EXT_buffer_device_address',
    'VK_EXT_calibrated_timestamps',
    'VK_EXT_color_write_enable',
    'VK_EXT_conditional_rendering',
    'VK_EXT_conservative_rasterization',
    'VK_EXT_custom_border_color',
    'VK_EXT_depth_bias_control',
    'VK_EXT_depth_clamp_control',
    'VK_EXT_depth_clamp_zero_one',
    'VK_EXT_depth_clip_control',
    'VK_EXT_depth_clip_enable',
    'VK_EXT_depth_range_unrestricted',
    'VK_EXT_descriptor_heap',
    'VK_EXT_dynamic_rendering_unused_attachments',
    'VK_EXT_extended_dynamic_state3',
    'VK_EXT_external_memory_acquire_unmodified',
    'VK_EXT_external_memory_dma_buf',
    'VK_EXT_filter_cubic',
    'VK_EXT_fragment_shader_interlock',
    'VK_EXT_global_priority',
    'VK_EXT_global_priority_query',
    'VK_EXT_graphics_pipeline_library',
    'VK_EXT_image_2d_view_of_3d',
    'VK_EXT_image_drm_format_modifier',
    'VK_EXT_image_sliced_view_of_3d',
    'VK_EXT_image_view_min_lod',
    'VK_EXT_index_type_uint8',
    'VK_EXT_legacy_dithering',
    'VK_EXT_legacy_vertex_attributes',
    'VK_EXT_line_rasterization',
    'VK_EXT_load_store_op_none',
    'VK_EXT_memory_budget',
    'VK_EXT_mesh_shader',
    'VK_EXT_multi_draw',
    'VK_EXT_multisampled_render_to_single_sampled',
    'VK_EXT_mutable_descriptor_type',
    'VK_EXT_nested_command_buffer',
    'VK_EXT_non_seamless_cube_map',
    'VK_EXT_pci_bus_info',
    'VK_EXT_pipeline_library_group_handles',
    'VK_EXT_post_depth_coverage',
    'VK_EXT_primitive_topology_list_restart',
    'VK_EXT_primitives_generated_query',
    'VK_EXT_provoking_vertex',
    'VK_EXT_queue_family_foreign',
    'VK_EXT_rasterization_order_attachment_access',
    'VK_EXT_robustness2',
    'VK_EXT_sample_locations',
    'VK_EXT_shader_atomic_float',
    'VK_EXT_shader_atomic_float2',
    'VK_EXT_shader_float8',
    'VK_EXT_shader_image_atomic_int64',
    'VK_EXT_shader_replicated_composites',
    'VK_EXT_shader_stencil_export',
    'VK_EXT_shader_subgroup_ballot',
    'VK_EXT_shader_subgroup_vote',
    'VK_EXT_shader_uniform_buffer_unsized_array',
    'VK_EXT_transform_feedback',
    'VK_EXT_vertex_attribute_divisor',
    'VK_EXT_vertex_input_dynamic_state',
    'VK_EXT_ycbcr_image_arrays',
    # vendor extensions
    'VK_ARM_rasterization_order_attachment_access',
    'VK_GOOGLE_decorate_string',
    'VK_GOOGLE_hlsl_functionality1',
    'VK_GOOGLE_user_type',
    'VK_IMG_filter_cubic',
    'VK_NV_compute_shader_derivatives',
    'VK_VALVE_mutable_descriptor_type',
]

class Ignorable(NamedTuple):
    struct: str
    var: str
    condition: str

class Gen:
    PRIMITIVE_TYPES = {
        'float': 4,
        'double': 8,
        'uint8_t': 1,
        'uint16_t': 2,
        'uint32_t': 4,
        'uint64_t': 8,
        'int32_t': 4,
        'int64_t': 8,
    }

    UNION_DEFAULT_TAGS = {
        'VkClearColorValue': 2,
        'VkClearValue': 0,
        'VkDeviceOrHostAddressKHR': 0,
        'VkDeviceOrHostAddressConstKHR': 0,
        'VkPipelineExecutableStatisticValueKHR': 2,
    }

    COMMAND_BLOCK_LIST = [
        # Most VK_KHR_acceleration_structure host commands are blocked since
        # VkDeviceOrHostAddressKHR and VkDeviceOrHostAddressConstKHR have been
        # redirected to VkDeviceAddress. This avoids invalid helpers.
        'vkBuildAccelerationStructuresKHR',
        'vkCopyAccelerationStructureToMemoryKHR',
        'vkCopyMemoryToAccelerationStructureKHR',
        # vkWriteSamplerDescriptorMESA and vkWriteResourceDescriptorMESA are
        # used instead of adding complicated var_in_out support.
        'vkWriteSamplerDescriptorsEXT',
        'vkWriteResourceDescriptorsEXT',
    ]

    IGNORABLE_LIST = [
        Ignorable(
            'VkImageCreateInfo',
            'pQueueFamilyIndices',
            'val->sharingMode == VK_SHARING_MODE_CONCURRENT',
        ),
        Ignorable(
            'VkBufferCreateInfo',
            'pQueueFamilyIndices',
            'val->sharingMode == VK_SHARING_MODE_CONCURRENT',
        ),
        Ignorable(
            'VkPhysicalDeviceImageDrmFormatModifierInfoEXT',
            'pQueueFamilyIndices',
            'val->sharingMode == VK_SHARING_MODE_CONCURRENT',
        ),
        Ignorable(
            'VkFramebufferCreateInfo',
            'pAttachments',
            '!(val->flags & VK_FRAMEBUFFER_CREATE_IMAGELESS_BIT)',
        ),
    ]

    def __init__(self, is_driver, reg):
        self.is_driver = is_driver

        self.reg = copy.deepcopy(reg)
        self._fixup_registry()

        self.supported_types = {}
        self._init_supported_types()

        # validate VkCommandTypeEXT
        command_type_ty = self.reg.type_table['VkCommandTypeEXT']
        enum_value_count = 0
        for cmd in self.supported_types[VkType.COMMAND]:
            key = 'VK_COMMAND_TYPE_' + cmd.name + '_EXT'
            if key not in command_type_ty.enums.values:
                raise KeyError('%s not defined for %s' % (key, command_type_ty.name))
            for alias in cmd.aliases:
                key = 'VK_COMMAND_TYPE_' + alias + '_EXT'
                if key not in command_type_ty.enums.values:
                    raise KeyError('%s not defined for %s' % (key, command_type_ty.name))
            enum_value_count += 1 + len(cmd.aliases)
        assert enum_value_count == len(command_type_ty.enums.values)

    def _set_type_needs(self, ty):
        for var in ty.variables:
            if var.ty.is_pointer() or var.ty.is_static_array():
                var.ty.base.attrs['need_array'] = True
                if var.ty.base.typedef:
                    var.ty.base.typedef.base.attrs['need_array'] = True

            if 'var_in' in var.attrs:
                if self.is_driver:
                    var.ty.set_attribute('need_encode', True)
                else:
                    var.ty.set_attribute('need_decode', True)

            if 'var_out' in var.attrs:
                if self.is_driver:
                    var.ty.set_attribute('need_partial', True)
                    var.ty.set_attribute('need_decode', True)
                    ty.has_out_ty = True
                else:
                    var.ty.set_attribute('need_partial', True)
                    var.ty.set_attribute('need_encode', True)
                    if var.is_blob():
                        ty.set_attribute('need_blob_encode', True)

            # check ignored args to conditionally skip
            for ignore in self.IGNORABLE_LIST:
                if ignore.struct == ty.name and ignore.var == var.name:
                    var.attrs['condition'] = ignore.condition

        if ty.ret:
            if ty.ret.ty.is_pointer() or ty.ret.ty.is_static_array():
                ty.ret.ty.base.attrs['need_array'] = True
                if ty.ret.ty.base.typedef:
                    ty.ret.ty.base.typedef.base.attrs['need_array'] = True

            if self.is_driver:
                ty.ret.ty.set_attribute('need_decode', True)
                ty.has_out_ty = True
            else:
                ty.ret.ty.set_attribute('need_encode', True)

    def _fixup_registry(self):
        for ty in self.reg.type_table.values():
            if ty.category == ty.COMMAND:
                ty.attrs['c_type'] = 'VK_COMMAND_TYPE_' + ty.name + '_EXT'

                if ty.ret:
                    ty.ret.attrs['var_out'] = True
                for var in ty.variables:
                    # non-const pointers are considerer outs
                    if var.ty.is_pointer() and not var.ty.is_const_pointer():
                        var.attrs['var_out'] = True
                    else:
                        var.attrs['var_in'] = True

                # outs appear in 'len_names' are in/out
                for var in ty.variables:
                    for name in var.attrs.get('len_names', []):
                        v = ty.find_variables(name)
                        if v:
                            v = v[0]
                        if v and 'var_out' in v.attrs:
                            v.attrs['var_in'] = var
                            v.attrs['var_out'] = var

                # vkRegisterCustomBorderColorEXT::pIndex is in/out
                if ty.name == 'vkRegisterCustomBorderColorEXT':
                    for var in ty.variables:
                        if var.name == 'pIndex':
                            var.attrs['var_in'] = True

            elif ty.category == ty.HANDLE:
                objtype = 'VK_OBJECT_TYPE_' + self.reg.upper_name(ty.name[2:])
                ty.attrs['c_objtype'] = objtype

            self._set_type_needs(ty)

    @staticmethod
    def support_type_depends(deps):
        if not '+' in deps and not ',' in deps:
            return True if deps in VK_XML_EXTENSION_LIST else False

        for or_dep in deps.split(','):
            if not '+' in or_dep and or_dep in VK_XML_EXTENSION_LIST:
                return True

            support_or_dep = True
            for and_dep in or_dep.split('+'):
                if not Gen.support_type_depends(and_dep):
                    support_or_dep = False
                    break
            if support_or_dep:
                return True

        return False

    def _get_supported_types(self):
        # collect types from features and extensions
        types = []
        for feat in self.reg.features:
            types.extend(feat.types)
        for ext in self.reg.extensions:
            if ext.name not in VK_XML_EXTENSION_LIST:
                continue

            types.extend(ext.types)
            for key in ext.optional_types:
                if Gen.support_type_depends(key):
                    types.extend(ext.optional_types[key])

        types_with_deps = set()
        for ty in types:
            if ty not in types_with_deps:
                types_with_deps.update(ty.get_dependencies())

        return types_with_deps

    def _init_supported_types(self):
        supported_types = self._get_supported_types()

        # filter p_next
        for ty in supported_types:
            p_next = []
            for tmp in ty.p_next:
                if tmp in supported_types:
                    p_next.append(tmp)
            ty.p_next = p_next

        # keep type_table order
        for ty in self.reg.type_table.values():
            if ty not in supported_types:
                continue

            if ty.category not in self.supported_types:
                self.supported_types[ty.category] = []
            if ty not in self.supported_types[ty.category]:
                self.supported_types[ty.category].append(ty)

    def is_serializable(self, var):
        if isinstance(var, VkType):
            return self.is_serializable(VkVariable(var))

        ty = var.ty.base
        if ty.category == ty.BASETYPE:
            if not ty.typedef:
                return False
            ty = ty.typedef

        if ty.category in [ty.INCLUDE, ty.DEFINE, ty.FUNCPOINTER]:
            return False
        elif ty.category == ty.DEFAULT:
            if ty.name in self.PRIMITIVE_TYPES:
                return True
            elif ty.name in ['char', 'size_t']:
                return True
            elif ty.name == 'void':
                return var.is_blob()
            return False
        elif ty.category in [ty.HANDLE, ty.ENUM, ty.BITMASK]:
            return True
        elif ty.category == ty.UNION:
            return ty.is_valid_union() or ty.name in self.UNION_DEFAULT_TAGS

        assert ty.category in [ty.STRUCT, ty.COMMAND]
        if ty.category == ty.STRUCT:
            if ty.name in ['VkBaseInStructure', 'VkBaseOutStructure']:
                return False
        elif ty.category == ty.COMMAND:
            if ty.name in self.COMMAND_BLOCK_LIST:
                return False
            elif ty.ret and not self.is_serializable(ty.ret):
                return False

        for var in ty.variables:
            if var.maybe_null() or var.is_p_next():
                continue
            if not self.is_serializable(var):
                return False

        return True

    def get_chain(self, ty):
        types = []
        skipped = []
        for next_ty in ty.p_next:
            if self.is_serializable(next_ty):
                types.append(next_ty)
            else:
                skipped.append(next_ty)
        return types, skipped

    @staticmethod
    def _api_number_to_version(num):
        major, minor = num.split('.')
        return 'VK_API_VERSION_' + major + '_' + minor

    def get_type_condition(self, ty):
        if not self.is_driver:
            return None

        # pNext chain support is required
        COND_NONE = 0
        # pNext chain depends on core api version
        # cover only new types from core api
        COND_API = 1
        # pNext chain depends on extension(s) or extension pair(s)
        # cover types from exts and promoted core exts
        COND_EXT = 2

        # type of condition
        cond = COND_NONE
        # api version e.g. '4206592'
        api_version = 0
        # map name to ext of exts in VK_XML_EXTENSION_LIST
        ext_map = {}
        # ext condition of the type
        exts = []
        # ext pair condition of the type
        ext_pairs = []

        for feat in self.reg.features:
            if ty in feat.types:
                # 1.2 is required for protocol
                if feat.number in ['1.0', '1.1', '1.2']:
                    assert cond == COND_NONE
                    return None
                api_version = self._api_number_to_version(feat.number)
                cond = COND_API
                break

        for ext in self.reg.extensions:
            if ext.name in VK_XML_EXTENSION_LIST:
                ext_map[ext.name] = ext

        for ext in ext_map.values():
            if ty in ext.types:
                # venus protocol versioning is not handled here
                if ext.name == 'VK_MESA_venus_protocol':
                    assert cond == COND_NONE
                    return None
                exts.append(ext)
                cond = COND_EXT
            elif ext.optional_types:
                for key in ext.optional_types:
                    if Gen.support_type_depends(key) and ty in ext.optional_types[key]:
                        ext_pairs.append((ext, ext_map[key]))
                        cond = COND_EXT
                        break

        assert cond != COND_NONE

        stmt = ''
        if cond == COND_EXT:
            ext_check = 'vn_cs_renderer_protocol_has_extension'
            stmt_exts = ' && '.join(f'!{ext_check}({ext.number} /* {ext.name} */)'
                                    for ext in exts)
            stmt_ext_pairs = ' && '.join(
                f'!({ext_check}({ext1.number} /* {ext1.name} */) && '
                f'{ext_check}({ext2.number} /* {ext2.name} */))'
                for ext1, ext2 in ext_pairs)
            stmt = ' && '.join(filter(None, [stmt_exts, stmt_ext_pairs]))
        else:
            stmt = '!vn_cs_renderer_protocol_has_api_version(%s)' % api_version

        return stmt

    class LoopInfo:
        """Information needed to generate loops to access a variable."""

        def __init__(self):
            self.loops = []

        def add_loop(self, iter_type, iter_count):
            level = len(self.loops)
            iter_name = chr(ord('i') + level)
            loop = self.Loop(level, iter_type, iter_name, iter_count)
            self.loops.append(loop)

        def pop_loop(self):
            return self.loops.pop()

        def is_last(self, level):
            return level == len(self.loops) - 1

        def get_iter_counts(self):
            return [loop.iter_count for loop in self.loops]

        def get_subscripts(self, to_level):
            iter_names = [loop.iter_name for loop in self.loops[:to_level]]
            return '[' + ']['.join(iter_names) + ']'

        def __bool__(self):
            return bool(self.loops)

        def __len__(self):
            return len(self.loops)

        def __iter__(self):
            return iter(self.loops)

        class Loop:
            def __init__(self, level, iter_type, iter_name, iter_count):
                # code() will print
                #
                #     for (iter_type iter_name = 0;
                #          iter_name < iter_count;
                #          iter_name++)
                #
                # For example, when the variable is an int32_t array of N
                # elements, we have
                #
                #     iter_type = 'uint32_t'
                #     iter_name = 'i'
                #     iter_count = 'N'
                self.level = level
                self.iter_type = iter_type
                self.iter_name = iter_name
                self.iter_count = iter_count

                # code() will print statements before the for-loop.
                self.statements = []

    class VariableInfo:
        # the variable is initialized, such as an input to the driver
        VALID = 0
        # the variable is uninitialized, such as an output to the driver
        INVALID = 1
        # the variable is partially initialized, such as an output to the
        # driver that is also a handle (because the id is specified by the
        # driver) or a struct (because it can potentially include handles,
        # sType, or pNext)
        PARTIAL = 2

        def __init__(self, ty, var, prefix, validity):
            self.ty = ty
            self.var = var
            self.prefix = prefix
            self.validity = validity

            self.func_stem = None
            self._init_func_stem()

            self.loop_info = None
            self._init_loop_info()

            # remember the dynamic array size before unrolling
            self.dynamic_array_size = None
            if self.var.is_dynamic_array():
                loop = self.loop_info.loops[0]
                if self.var.ty.is_c_string():
                    # not usable
                    assert 'strlen' in loop.iter_count
                else:
                    self.dynamic_array_size = loop.iter_count

            # union that has a selector
            #
            # For example, for member var foo with type foo_type and selector s:
            #
            #     statements = [
            #         'vn_sizeof_foo_type(val->foo, val->s)',
            #     ]
            self.selector = None

            # Try to unroll the inner most loop and save its iter_count to
            # array_size.
            #
            # There is no nested loops after unrolling currently.
            self.array_size = None
            self._unroll_loop()

            # code() will print statements line-by-line.
            #
            # For example, when the variable is an int32_t array, we have
            # this for encode
            #
            #     statements = [
            #         'vn_encode_array_size(...)',
            #         'vn_encode_int32_t_array(...)',
            #     ]
            #
            # and, when allocation is needed, this for decode
            #
            #     statements = [
            #         'vn_decode_array_size(...)',
            #         'foo = vn_cs_decoder_alloc_temp(...)',
            #         'if (!foo) return',
            #         'vn_decode_int32_t_array(...)',
            #     ]
            self.statements = []

        def _init_func_stem(self):
            if self.var.is_blob():
                self.func_stem = 'blob'
            elif self.var.ty.base.category == VkType.BITMASK:
                self.func_stem = self.var.ty.base.typedef.name
            else:
                self.func_stem = self.var.ty.base.name

        def _var_deref(self, var=None):
            if not var:
                var = self.var
            return '*' * (var.ty.indirection_depth() + var.ty.is_static_array())

        def _var_const_cast(self):
            return '(%s %s)' % (self.var.ty.base.name, self._var_deref())

        def _var_loop_indices(self, loop_level):
            if loop_level:
                return self.loop_info.get_subscripts(loop_level)
            else:
                return ''

        def _var_name(self, loop_level=0, const_cast=False):
            var_name = self.prefix + self.var.name
            indices = self._var_loop_indices(loop_level)
            if const_cast and (self.var.ty.is_const_pointer() or
                    self.var.ty.is_const_static_array()):
                if indices:
                    return '(%s%s)%s' % (self._var_const_cast(), var_name, indices)
                else:
                    return '%s%s' % (self._var_const_cast(), var_name)
            else:
                return '%s%s' % (var_name, indices)

        def _init_loop_info(self):
            self.loop_info = Gen.LoopInfo()

            if 'wa_require_static_len' in self.var.attrs or 'len_exprs' not in self.var.attrs:
                if self.var.ty.is_static_array():
                    iter_count = self.var.ty.static_array_size()
                    self.loop_info.add_loop('uint32_t', iter_count)
                return

            len_exprs = self.var.attrs['len_exprs']
            len_names = self.var.attrs['len_names']
            for level, (expr, name) in enumerate(zip(len_exprs, len_names)):
                if expr == 'null-terminated':
                    iter_type = 'size_t'
                    iter_count = 'strlen(%s) + 1' % self._var_name(level)
                elif name:
                    loop_vars = self.ty.find_variables(name)

                    deref = self._var_deref(loop_vars[-1])
                    iter_type = loop_vars[-1].ty.base.name
                    iter_count = expr.replace(name, deref + self.prefix + name)

                    assert len(loop_vars) <= 2
                    if len(loop_vars) > 1 or loop_vars[-1].ty.is_pointer():
                        # TODO: formalize the handling of ppBuildRangeInfos
                        if self.var.name != 'ppBuildRangeInfos':
                            iter_count = '(%s%s ? %s : 0)' % (self.prefix,
                                    loop_vars[0].name, iter_count)
                else:
                    iter_type = 'uint32_t'
                    iter_count = expr

                self.loop_info.add_loop(iter_type, iter_count)

        def _unroll_loop(self):
            # unroll loops for scalar arrays to get padding right
            scalar_categories = [VkType.DEFAULT, VkType.BASETYPE, VkType.ENUM]
            if not self.loop_info:
                return
            if not self.var.ty.base.category in scalar_categories:
                return

            loop = self.loop_info.pop_loop()
            self.func_stem += '_array'
            self.array_size = loop.iter_count

        def init_alloc_stmts(self):
            alloc_counts = self.loop_info.get_iter_counts()
            if self.array_size:
                alloc_counts.append(self.array_size)

            if not alloc_counts:
                var_name = self._var_name()
                deref = self._var_deref()
                alloc_stmt = '%s = vn_cs_decoder_alloc_temp(dec, sizeof(%s%s));' % (
                        var_name, deref, var_name)
                check_stmt = 'if (!%s) return;' % var_name
                self.statements.append(alloc_stmt)
                self.statements.append(check_stmt)
                return

            for level, count in enumerate(alloc_counts):
                if self.var.is_blob():
                    if 'var_out' in self.var.attrs:
                        alloc_stmt = '%s = vn_cs_encoder_get_blob_storage(enc, offset, %s);' % (
                                self._var_name(level, level > 0), count)
                    elif self.var.ty.is_const_pointer():
                        alloc_stmt = '%s = vn_cs_decoder_get_blob_storage(dec, %s);' % (
                                self._var_name(level, level > 0), count)
                    else:
                        alloc_stmt = '%s = vn_cs_decoder_alloc_temp(dec, %s);' % (
                                self._var_name(level, level > 0), count)
                else:
                    alloc_stmt = '%s = vn_cs_decoder_alloc_temp_array(dec, sizeof(*%s), %s);' % (
                            self._var_name(level, level > 0), self._var_name(level), count)
                check_stmt = 'if (!%s) return;' % self._var_name(level)

                if level < len(self.loop_info):
                    loop = self.loop_info.loops[level]
                    loop.statements.append(alloc_stmt)
                    loop.statements.append(check_stmt)
                else:
                    self.statements.append(alloc_stmt)
                    self.statements.append(check_stmt)

        def func_args(self, const_cast):
            var_name = self.prefix + self.var.name
            loop_level = len(self.loop_info)

            deref_count = self.var.ty.indirection_depth() + self.var.ty.is_static_array()
            deref_count -= loop_level
            # we want a pointer to var
            deref_count -= 1

            deref = ''
            if deref_count > 0:
                deref = '*' * deref_count
            elif deref_count < 0:
                deref = '&' * -deref_count

            args = '%s%s' % (deref, self._var_name(loop_level, const_cast))
            if self.array_size:
                args += ', ' + self.array_size
            elif self.selector:
                args += ', ' + self.prefix + self.selector

            return args

        def _code_enter_loops(self, indent_level, bracket_last):
            code = ''
            indent = '    ' * indent_level
            for loop in self.loop_info:
                for stmt in loop.statements:
                    code += '%s%s\n' % (indent, stmt)

                init_expr = '%s %c = 0' % (loop.iter_type, loop.iter_name)
                cond_expr = '%c < %s' % (loop.iter_name, loop.iter_count)
                incr_expr = '%c++' % loop.iter_name

                bracket = ' {'
                if not bracket_last and self.loop_info.is_last(loop.level):
                    bracket = ''

                code += '%sfor (%s; %s; %s)%s\n' % (indent, init_expr,
                        cond_expr, incr_expr, bracket)

                indent += '    '

            return code

        def _code_body(self, indent_level):
            loop_level = len(self.loop_info)

            code = ''
            indent = '    ' * (indent_level + loop_level)

            for stmt in self.statements:
                code += '%s%s\n' % (indent, stmt)

            return code

        def _code_leave_loops(self, indent_level, bracket_last):
            code = ''
            for level in reversed(range(len(self.loop_info))):
                if bracket_last or not self.loop_info.is_last(level):
                    indent = '    ' * (indent_level + level)
                    code += '%s}\n' % indent
            return code

        def need_bracket(self):
            if self.loop_info:
                return True

            if len(self.statements) > 1:
                return True

            return False

        def code(self, indent_level):
            code_body = self._code_body(indent_level)

            bracket_last = code_body.count(';') > 1
            code_enter_loops = self._code_enter_loops(
                    indent_level, bracket_last)
            code_leave_loops = self._code_leave_loops(
                    indent_level, bracket_last)

            return code_enter_loops + code_body + code_leave_loops

    def _sizeof_variable(self, info, dst, indent_level):
        indent = '    ' * indent_level
        if info.validity == info.INVALID:
            if info.var.ty.is_pointer():
                return '%s%s += vn_sizeof_simple_pointer(%s); /* out */' % (
                       indent, dst, info._var_name())
            else:
                return '%s/* skip %s */' % (indent, info._var_name())

        condition = info._var_name()
        if 'condition' in info.var.attrs:
            condition = info.var.attrs['condition']

        stmts = []
        if info.var.is_dynamic_array():
            stmts.append('if (%s) {' % condition)
            stmts.append('    %s' % info.code(indent_level + 1).strip())
            stmts.append('} else {')
            stmts.append('    %s += vn_sizeof_array_size(0);' % dst)
            stmts.append('}')
        elif info.var.ty.is_pointer():
            stmts.append('%s += vn_sizeof_simple_pointer(%s);' % (dst, info._var_name()))
            stmts.append('if (%s)' % info._var_name())
            stmts.append('    %s' % info.code(indent_level + 1).strip())
        else:
            stmts.append(info.code(indent_level).strip())

        code = ''
        for stmt in stmts:
            code += '%s%s\n' % (indent, stmt)
        return code

    def _encode_variable(self, info, indent_level):
        indent = '    ' * indent_level
        if info.validity == info.INVALID:
            if info.var.is_dynamic_array():
                return '%svn_encode_array_size(enc, %s ? %s : 0); /* out */' % (
                        indent, info._var_name(), info.array_size)
            elif info.var.ty.is_pointer():
                return '%svn_encode_simple_pointer(enc, %s); /* out */' % (
                       indent, info._var_name())
            else:
                return '%s/* skip %s */' % (indent, info._var_name())

        condition = info._var_name()
        if 'condition' in info.var.attrs:
            condition = info.var.attrs['condition']

        stmts = []
        if info.var.is_dynamic_array():
            stmts.append('if (%s) {' % condition)
            stmts.append('    %s' % info.code(indent_level + 1).strip())
            stmts.append('} else {')
            stmts.append('    vn_encode_array_size(enc, 0);')
            stmts.append('}')
        elif info.var.ty.is_pointer():
            stmts.append('if (vn_encode_simple_pointer(enc, %s))' % info._var_name())
            stmts.append('    %s' % info.code(indent_level + 1).strip())
        else:
            stmts.append(info.code(indent_level).strip())

        if 'stride' in info.var.attrs:
            stmts.append('%s = sizeof(%s);' % (info.var.attrs['stride'], info.func_stem))

        code = ''
        for stmt in stmts:
            code += '%s%s\n' % (indent, stmt)
        return code

    def _decode_variable(self, ty, info, indent_level):
        indent = '    ' * indent_level
        if info.validity == info.INVALID:
            if not info.var.ty.is_pointer():
                return '%s/* skip %s */' % (indent, info._var_name())

        stmts = []
        if info.var.is_dynamic_array():
            stmts.append('if (vn_peek_array_size(dec)) {')
            if 'need_blob_encode' in ty.attrs and 'var_out' in info.var.attrs:
                stmts.append('    offset += vn_sizeof_array_size(%s);' % \
                        (info.dynamic_array_size))
            stmts.append('    %s' % info.code(indent_level + 1).strip())
            if 'need_blob_encode' in ty.attrs and 'var_out' in info.var.attrs:
                stmts.append('    offset += vn_sizeof_%s(%s);' % \
                        (info.func_stem, info.func_args(False)))
            stmts.append('} else {')
            if not self.is_driver and not info.var.is_optional() and \
                    info.var.can_validate() and info.dynamic_array_size:
                stmts.append('    vn_decode_array_size(dec, %s);' % info.dynamic_array_size)
            else:
                stmts.append('    vn_decode_array_size_unchecked(dec);')
            stmts.append('    %s = NULL;' % info._var_name())
            stmts.append('}')
        elif info.var.ty.is_pointer():
            stmts.append('if (vn_decode_simple_pointer(dec)) {')
            if 'need_blob_encode' in ty.attrs and 'var_out' in info.var.attrs:
                stmts.append('    offset += vn_sizeof_simple_pointer(%s);' % \
                        (info._var_name()))
            stmts.append('    %s' % info.code(indent_level + 1).strip())
            if 'need_blob_encode' in ty.attrs and 'var_out' in info.var.attrs:
                stmts.append('    offset += vn_sizeof_%s(%s);' % \
                        (info.func_stem, info.func_args(False)))
            stmts.append('} else {')
            stmts.append('    %s = NULL;' % info._var_name())
            if not self.is_driver and not info.var.is_optional() and \
                    info.var.can_validate():
                stmts.append('    vn_cs_decoder_set_fatal(dec);')
            stmts.append('}')
        elif info.need_bracket():
            stmts.append('{')
            stmts.append('    %s' % info.code(indent_level + 1).strip())
            stmts.append('}')
        else:
            stmts.append(info.code(indent_level).strip())


        code = ''
        for stmt in stmts:
            code += '%s%s\n' % (indent, stmt)
        return code

    def _replace_variable_handle(self, info, indent_level):
        indent = '    ' * indent_level
        if (info.validity != info.VALID or
            not info.var.ty.base.might_contain_handle() or
            not self.is_serializable(info.var)):
            return '%s/* skip %s */' % (indent, info._var_name())

        stmts = []
        if info.var.ty.is_pointer() and info.need_bracket():
            stmts.append('if (%s) {' % info._var_name())
            stmts.append('   %s' % info.code(indent_level + 1).strip())
            stmts.append('}')
        elif info.var.ty.is_pointer():
            stmts.append('if (%s)' % info._var_name())
            stmts.append('    %s' % info.code(indent_level + 1).strip())
        else:
            stmts.append(info.code(indent_level).strip())

        code = ''
        for stmt in stmts:
            code += '%s%s\n' % (indent, stmt)
        return code

    def _sizeof_variable_info(self, ty, var, prefix, validity, dst):
        info = self.VariableInfo(ty, var, prefix, validity)
        if not self.is_serializable(var):
            # okay if part of a union with default tags since not selected
            assert var.maybe_null() or ty.name in self.UNION_DEFAULT_TAGS
            info.statements.append('assert(false);')
            return info

        # for passing selector as a second func arg
        if 'selector' in var.attrs:
            info.selector = var.attrs['selector']

        # save strlen result to a temp
        if var.has_c_string():
            assert info.array_size.startswith('strlen')
            stmt = 'const size_t string_size = %s;' % info.array_size
            info.statements.append(stmt)
            info.array_size = 'string_size'

        # encode array sizes
        for loop in info.loop_info:
            stmt = '%s += vn_sizeof_array_size(%s);' % (dst, loop.iter_count)
            loop.statements.append(stmt)
        if info.array_size:
            stmt = '%s += vn_sizeof_array_size(%s);' % (dst, info.array_size)
            info.statements.append(stmt)

        # nothing to encode
        if validity == info.INVALID:
            return info

        func_name = 'vn_sizeof_' + info.func_stem
        if validity == info.PARTIAL and var.ty.base.category == ty.STRUCT:
            func_name += '_partial'

        stmt = '%s += %s(%s);' % (dst, func_name, info.func_args(False))

        info.statements.append(stmt)

        return info

    def _encode_variable_info(self, ty, var, prefix, validity):
        info = self.VariableInfo(ty, var, prefix, validity)
        if not self.is_serializable(var):
            # okay if part of a union with default tags since not selected
            assert var.maybe_null() or ty.name in self.UNION_DEFAULT_TAGS
            info.statements.append('assert(false);')
            return info

        # for passing selector as a second func arg
        if 'selector' in var.attrs:
            info.selector = var.attrs['selector']

        # save strlen result to a temp
        if var.has_c_string():
            assert info.array_size.startswith('strlen')
            stmt = 'const size_t string_size = %s;' % info.array_size
            info.statements.append(stmt)
            info.array_size = 'string_size'

        # encode array sizes
        for loop in info.loop_info:
            stmt = 'vn_encode_array_size(enc, %s);' % loop.iter_count
            loop.statements.append(stmt)
        if info.array_size:
            stmt = 'vn_encode_array_size(enc, %s);' % info.array_size
            info.statements.append(stmt)

        # nothing to encode
        if validity == info.INVALID:
            return info

        func_name = 'vn_encode_' + info.func_stem
        if validity == info.PARTIAL and var.ty.base.category == ty.STRUCT:
            func_name += '_partial'

        if 'stride' in var.attrs:
            stmt = '%s(enc, (void *)%s + %s * %c);' % (func_name, info._var_name(), var.attrs['stride'], loop.iter_name)
        else:
            stmt = '%s(enc, %s);' % (func_name, info.func_args(False))

        info.statements.append(stmt)

        return info

    def _decode_variable_info(self, ty, var, prefix, validity, alloc_storage):
        info = self.VariableInfo(ty, var, prefix, validity)
        if not self.is_serializable(var):
            # okay if part of a union with default tags since not selected
            assert var.maybe_null() or ty.name in self.UNION_DEFAULT_TAGS
            if self.is_driver:
                stmt = 'assert(false);'
            else:
                stmt = 'vn_cs_decoder_set_fatal(dec);'
            info.statements.append(stmt)
            return info

        # decode array sizes
        for loop in info.loop_info:
            temp_name = 'iter_count'
            if loop.level > 0:
                temp_name += '_' + loop.iter_name

            stmt = 'const %s %s = vn_decode_array_size(dec, %s);' % \
                    (loop.iter_type, temp_name, loop.iter_count)
            loop.statements.append(stmt)
            loop.iter_count = temp_name

        # decode the encoded array size
        if info.array_size:
            if var.has_c_string():
                assert info.array_size.startswith('strlen')
                assert info.func_stem == 'char_array'
                stmt = 'const size_t string_size = vn_decode_array_size_unchecked(dec);'
                info.statements.append(stmt)
                info.array_size = 'string_size'
            else:
                stmt = 'const size_t array_size = vn_decode_array_size(dec, %s);' \
                        % info.array_size
                info.statements.append(stmt)
                info.array_size = 'array_size'

        if alloc_storage and var.ty.is_pointer():
            info.init_alloc_stmts()

        # nothing to decode
        if validity == info.INVALID:
            return info

        func_name = 'vn_decode_' + info.func_stem
        if validity == info.PARTIAL and var.ty.base.category == ty.STRUCT:
            func_name += '_partial'

        if alloc_storage and var.ty.base.category in [ty.STRUCT, ty.UNION]:
            func_name += '_temp'

        if var.ty.base.category == ty.HANDLE:
            if validity == info.VALID:
                # automatic handle lookup
                if not self.is_driver:
                    func_name += '_lookup'
            elif alloc_storage and var.ty.base.dispatchable:
                func_name += '_temp'

        stmt = '%s(dec, %s);' % (func_name, info.func_args(True))

        info.statements.append(stmt)

        return info

    def _replace_variable_handle_info(self, ty, var, prefix, validity):
        info = self.VariableInfo(ty, var, prefix, validity)

        # nothing to replace
        if (validity != info.VALID or
            not var.ty.base.might_contain_handle() or
            not self.is_serializable(var)):
            return info

        # for passing selector as a second func arg
        if 'selector' in var.attrs:
            info.selector = var.attrs['selector']

        stmt = 'vn_replace_%s_handle(%s);' % (info.func_stem,
                info.func_args(True))
        info.statements.append(stmt)

        return info

    def _get_variable_validity(self, ty, var, initialized):
        if initialized:
            return self.VariableInfo.VALID

        for other_var in ty.variables:
            if var == other_var:
                continue
            if 'wa_require_static_len' not in other_var.attrs:
                for name in other_var.attrs.get('len_names', []):
                    len_vars = ty.find_variables(name)
                    if var in len_vars:
                        return self.VariableInfo.VALID

        partially_initialized = [ty.HANDLE, ty.STRUCT]
        if var.ty.base.category in partially_initialized:
            return self.VariableInfo.PARTIAL

        return self.VariableInfo.INVALID

    def sizeof_struct_member(self, ty, var, prefix, struct_is_partial, dst, indent_level=1):
        validity = self._get_variable_validity(ty, var, not struct_is_partial)
        info = self._sizeof_variable_info(ty, var, prefix, validity, dst)
        return self._sizeof_variable(info, dst, indent_level).strip()

    def encode_struct_member(self, ty, var, prefix, struct_is_partial, indent_level=1):
        validity = self._get_variable_validity(ty, var, not struct_is_partial)
        info = self._encode_variable_info(ty, var, prefix, validity)
        return self._encode_variable(info, indent_level).strip()

    def decode_struct_member(self, ty, var, prefix, struct_is_partial, alloc_storage, indent_level=1):
        validity = self._get_variable_validity(ty, var, not struct_is_partial)
        info = self._decode_variable_info(ty, var, prefix, validity, alloc_storage)
        return self._decode_variable(ty, info, indent_level).strip()

    def replace_struct_member_handle(self, ty, var, prefix, indent_level=1):
        validity = self._get_variable_validity(ty, var, True)
        info = self._replace_variable_handle_info(ty, var, prefix, validity)
        return self._replace_variable_handle(info, indent_level).strip()

    def sizeof_command_arg(self, ty, var, prefix, dst):
        validity = self._get_variable_validity(ty, var, 'var_in' in var.attrs)
        info = self._sizeof_variable_info(ty, var, prefix, validity, dst)
        return self._sizeof_variable(info, dst, 1).strip()

    def encode_command_arg(self, ty, var, prefix):
        validity = self._get_variable_validity(ty, var, 'var_in' in var.attrs)
        info = self._encode_variable_info(ty, var, prefix, validity)
        return self._encode_variable(info, 1).strip()

    def decode_command_arg(self, ty, var, prefix):
        validity = self._get_variable_validity(ty, var, 'var_in' in var.attrs)
        info = self._decode_variable_info(ty, var, prefix, validity, True)
        return self._decode_variable(ty, info, 1).strip()

    def replace_command_arg_handle(self, ty, var, prefix):
        validity = self._get_variable_validity(ty, var, 'var_in' in var.attrs)
        info = self._replace_variable_handle_info(ty, var, prefix, validity)
        return self._replace_variable_handle(info, 1).strip()

    def sizeof_command_reply(self, ty, var, prefix, dst):
        if 'var_out' not in var.attrs:
            return '/* skip %s%s */' % (prefix, var.name)

        info = self._sizeof_variable_info(ty, var, prefix,
                self.VariableInfo.VALID, dst)
        return self._sizeof_variable(info, dst, 1).strip()

    def encode_command_reply(self, ty, var, prefix):
        if 'var_out' not in var.attrs:
            return '/* skip %s%s */' % (prefix, var.name)

        info = self._encode_variable_info(ty, var, prefix,
                self.VariableInfo.VALID)
        return self._encode_variable(info, 1).strip()

    def decode_command_reply(self, ty, var, prefix):
        if 'var_out' not in var.attrs:
            return '/* skip %s%s */' % (prefix, var.name)

        info = self._decode_variable_info(ty, var, prefix,
                self.VariableInfo.VALID, False)
        return self._decode_variable(ty, info, 1).strip()

class GenCS:
    def __init__(self, gen):
        self.gen = gen

    def generate(self, template):
        return template.render()

class GenDefines:
    def __init__(self, gen):
        self.gen = gen

        self.enum_extends = []
        for ty in self.gen.reg.type_table.values():
            if ty.category == ty.ENUM and ty.enums.vk_xml_values:
                if len(ty.enums.values) != len(ty.enums.vk_xml_values):
                    self.enum_extends.append(ty)

        self.typedef_types = []
        self.enum_types = []
        self.bitmask_types = []
        self.struct_types = []
        exts = self.gen.reg.extensions[self.gen.reg.vk_xml_extension_count:]
        for ext in exts:
            for ty in ext.types:
                if ty.category == ty.BASETYPE and ty.typedef:
                    self.typedef_types.append(ty)
                elif ty.category == ty.ENUM and ty.enums.values:
                    self.enum_types.append(ty)
                elif ty.category == ty.BITMASK:
                    self.bitmask_types.append(ty)
                elif ty.category == ty.STRUCT:
                    self.struct_types.append(ty)

        self.command_types = self.gen.supported_types[VkType.COMMAND]

    def generate(self, template):
        return template.render(
                TYPEDEF_TYPES=self.typedef_types,
                ENUM_EXTENDS=self.enum_extends,
                ENUM_TYPES=self.enum_types,
                BITMASK_TYPES=self.bitmask_types,
                STRUCT_TYPES=self.struct_types,
                COMMAND_TYPES=self.command_types)

class GenInfo:
    def __init__(self, gen):
        self.gen = gen

        self.exts = [ext for ext in self.gen.reg.extensions
                if ext.name in VK_XML_EXTENSION_LIST]
        self.exts = sorted(self.exts, key=self.extension_sort_key)

        self.max_ext_number = 0
        for ext in self.exts:
            if ext.number > self.max_ext_number:
                self.max_ext_number = ext.number
        assert self.max_ext_number > 0

    def generate(self, template):
        return template.render(
                WIRE_FORMAT_VERSION=VN_WIRE_FORMAT_VERSION,
                VK_XML_VERSION=self.gen.reg.vk_xml_version,
                EXTENSIONS=self.exts,
                MAX_EXTENSION_NUMBER=self.max_ext_number)

    @staticmethod
    def extension_sort_key(ext):
        # sort by names for bsearch
        return ext.name

class GenTypes:
    def __init__(self, gen):
        self.gen = gen

        self.early_scalar_types = []
        self.scalar_types = []

        for ty in self.gen.supported_types[VkType.DEFAULT]:
            if ty.name in self.gen.PRIMITIVE_TYPES:
                if ty.name in ['uint64_t', 'int32_t']:
                    self.early_scalar_types.append(ty)
                else:
                    self.scalar_types.append(ty)

        for ty in self.gen.supported_types[VkType.BASETYPE]:
            if ty.typedef and self.gen.is_serializable(ty.typedef):
                self.scalar_types.append(ty)

        for ty in self.gen.supported_types[VkType.ENUM]:
            if ty.enums.values:
                if ty.name == 'VkStructureType':
                    self.early_scalar_types.append(ty)
                else:
                    self.scalar_types.append(ty)

    def generate(self, template):
        return template.render(
                GEN=self.gen,
                EARLY_SCALAR_TYPES=self.early_scalar_types,
                SCALAR_TYPES=self.scalar_types)

class GenHandles:
    def __init__(self, gen):
        self.gen = gen

        self.handle_types = self.gen.supported_types[VkType.HANDLE]

    def generate(self, template):
        return template.render(
                GEN=self.gen,
                HANDLE_TYPES=self.handle_types)

class GenStructsAndCommands:
    # the order matters!
    RULES = {
        'structs': [],
        'transport': [], # matches all
        'instance': [
            'CreateInstance',
            'DestroyInstance',
            'EnumerateInstance',
            'GetInstance',
        ],
        # put VkPhysicalDevice and VkDevice in the same group because
        # VkPhysicalDeviceFeatures2 causes too much code to be generated in
        # the common header
        'device': [
            'EnumeratePhysicalDevice',
            'CreateDevice',
            'DestroyDevice',
            'Device',
            'GetDevice',
            'GetCalibratedTimestamps',
            'GetPhysicalDevice',
            'EnumerateDevice',
        ],
        'queue': [
            'Queue',
        ],
        'fence': [
            'CreateFence',
            'DestroyFence',
            'WaitForFence',
            'ResetFence',
            'GetFence',
            'ImportFence',
        ],
        'semaphore': [
            'CreateSemaphore',
            'DestroySemaphore',
            'WaitSemaphore',
            'GetSemaphore',
            'SignalSemaphore',
            'ImportSemaphore',
        ],
        'event': [
            'CreateEvent',
            'DestroyEvent',
            'ResetEvent',
            'SetEvent',
            'GetEvent',
        ],
        'device_memory': [
            'AllocateMemory',
            'FlushMappedMemory',
            'FreeMemory',
            'GetDeviceMemory',
            'InvalidateMappedMemory',
            'MapMemory',
            'UnmapMemory',
            'GetMemory',
        ],
        'image': [
            'BindImage',
            'CreateImage',
            'DestroyImage',
            'GetImage',
            'GetDeviceImage',
        ],
        'image_view': [
            'CreateImageView',
            'DestroyImageView',
        ],
        'sampler': [
            'CreateSampler',
            'DestroySampler',
        ],
        'sampler_ycbcr_conversion': [
            'CreateSamplerYcbcrConversion',
            'DestroySamplerYcbcrConversion',
        ],
        'buffer': [
            'BindBuffer',
            'CreateBuffer',
            'DestroyBuffer',
            'GetBuffer',
            'GetDeviceBuffer',
        ],
        'buffer_view': [
            'CreateBufferView',
            'DestroyBufferView',
        ],
        'descriptor_pool': [
            'CreateDescriptorPool',
            'DestroyDescriptorPool',
            'ResetDescriptorPool',
        ],
        'descriptor_set': [
            'AllocateDescriptorSet',
            'FreeDescriptorSet',
            'UpdateDescriptorSet',
        ],
        'descriptor_set_layout': [
            'CreateDescriptorSetLayout',
            'DestroyDescriptorSetLayout',
            'GetDescriptorSetLayout',
        ],
        'descriptor_update_template': [
            'CreateDescriptorUpdateTemplate',
            'DestroyDescriptorUpdateTemplate',
        ],
        'render_pass': [
            'CreateRenderPass',
            'DestroyRenderPass',
            'GetRenderArea',
            'GetRenderingArea',
        ],
        'framebuffer': [
            'CreateFramebuffer',
            'DestroyFramebuffer',
        ],
        'query_pool': [
            'CreateQueryPool',
            'DestroyQueryPool',
            'ResetQueryPool',
            'GetQueryPool',
        ],
        'shader_module': [
            'CreateShaderModule',
            'DestroyShaderModule',
        ],
        'pipeline': [
            'CreateComputePipeline',
            'CreateGraphicsPipeline',
            'CreateRayTracingPipeline',
            'DestroyPipeline',
            'GetRayTracing',
        ],
        'pipeline_layout': [
            'CreatePipelineLayout',
            'DestroyPipelineLayout',
        ],
        'pipeline_cache': [
            'CreatePipelineCache',
            'DestroyPipelineCache',
            'GetPipelineCache',
            'MergePipelineCache',
        ],
        'command_pool': [
            'CreateCommandPool',
            'DestroyCommandPool',
            'ResetCommandPool',
            'TrimCommandPool',
        ],
        'command_buffer': [
            'AllocateCommandBuffer',
            'BeginCommandBuffer',
            'EndCommandBuffer',
            'FreeCommandBuffer',
            'ResetCommandBuffer',
            'Cmd',
        ],
        'private_data_slot': [
            'CreatePrivateDataSlot',
            'DestroyPrivateDataSlot',
            'GetPrivateData',
            'SetPrivateData',
        ],
        'host_copy': [
            'CopyImageToImage',
            'CopyImageToMemory',
            'CopyMemoryToImage',
            'TransitionImageLayout',
        ],
        'acceleration_structure': [
            'CreateAccelerationStructure',
            'DestroyAccelerationStructure',
            'GetAccelerationStructure',
            'GetDeviceAccelerationStructure',
            'BuildAccelerationStructure',
            'CopyAccelerationStructure',
            'CopyMemoryToAccelerationStructure',
            'WriteAccelerationStructure',
        ],
        'descriptor_heap': [
            'WriteResourceDescriptor',
            'WriteSamplerDescriptor',
            'GetImageOpaqueCaptureData',
            'RegisterCustomBorderColor',
            'UnregisterCustomBorderColor',
        ],
    }

    class Group:
        def __init__(self, name, rules=[]):
            self.name = name
            self.rules = rules

            self.type_set = set()
            self.generated = set()

            self.commands = []
            self.skipped_commands = []
            self.structs = []
            self.manual_unions = []
            self.skipped_structs = []

        def match_command(self, cmd):
            for rule in self.rules:
                if cmd.name[2:].startswith(rule):
                    return True
            return False if self.rules else True

    def __init__(self, gen):
        self.gen = gen
        self._init_groups()

    def _init_groups(self):
        self.groups = []
        for key, val in self.RULES.items():
            self.groups.append(self.Group(key, val))

        # reverse before matching
        self.groups.reverse()

        # add commands and their dependencies to type_sets
        for cmd in self.gen.supported_types[VkType.COMMAND]:
            group = None
            for g in self.groups:
                if g.match_command(cmd):
                    group = g
                    break
            assert group
            self._add_type_set_recursive(group.type_set, cmd)

        # make sure each type belongs to just one type_set
        common_type_set = set()
        for i, g1 in enumerate(self.groups):
            for g2 in self.groups[i + 1:]:
                intersection = g1.type_set.intersection(g2.type_set)
                if intersection:
                    g2.type_set.difference_update(intersection)
                    common_type_set.update(intersection)
            g1.type_set.difference_update(common_type_set)

        # the remaining types belong to the last group's type_set
        last_group = self.groups[-1]
        assert last_group.name == 'structs' and not last_group.type_set
        last_group.type_set = common_type_set

        # now we can add types to groups
        for cmd in self.gen.supported_types[VkType.COMMAND]:
            group = None
            for g in self.groups:
                if g.match_command(cmd):
                    group = g
                    break
            assert group
            self._add_group_recursive(group, cmd)

    def _add_type_set_recursive(self, type_set, ty):
        # only commands, structs, and unions
        if ty.category not in [ty.COMMAND, ty.STRUCT, ty.UNION]:
            return
        if ty in type_set:
            return

        deps = [var.ty.base for var in ty.variables] + ty.p_next
        if ty.ret:
            deps.append(ty.ret.ty.base)

        for dep in deps:
            self._add_type_set_recursive(type_set, dep)
        type_set.add(ty)

    def _add_group_recursive(self, group, ty):
        # only commands, structs, and unions
        if ty.category not in [ty.COMMAND, ty.STRUCT, ty.UNION]:
            return

        # redirect to base group
        if ty not in group.type_set:
            group = self.groups[-1]
            assert ty in group.type_set

        if ty in group.generated:
            return
        group.generated.add(ty)

        if self.gen.is_serializable(ty):
            # add dependencies first
            deps = [var.ty.base for var in ty.variables] + ty.p_next
            if ty.ret:
                deps.append(ty.ret.ty.base)
            for dep in deps:
                self._add_group_recursive(group, dep)

            if ty.category == ty.COMMAND:
                group.commands.append(ty)
            else:
                group.structs.append(ty)
        elif ty.category == ty.UNION:
            can_manual = True
            for var in ty.variables:
                if not self.gen.is_serializable(var):
                    can_manual = False
                    break

            if can_manual:
                group.manual_unions.append(ty)
            else:
                group.skipped_structs.append(ty)
        elif ty.category == ty.COMMAND:
            group.skipped_commands.append(ty)
        else:
            group.skipped_structs.append(ty)

    def generate(self, template, group):
        return template.render(
                GEN=self.gen,
                GUARD=group.name.upper(),
                COMMAND_TYPES=group.commands,
                COMMAND_SKIPPED=group.skipped_commands,
                STRUCT_TYPES=group.structs,
                STRUCT_SKIPPED=group.skipped_structs,
                MANUAL_UNION_TYPES=group.manual_unions)

class GenUtil:
    def __init__(self, gen):
        self.gen = gen
        self.global_commands = []
        self.instance_commands = []
        self.physical_device_commands = []
        self.device_commands = []

        cmds = self.gen.supported_types[VkType.COMMAND]
        for cmd in cmds:
            if (cmd.name == 'vkGetInstanceProcAddr' or
                cmd.is_private or not
                cmd.variables):
                continue

            dispatch_handle = cmd.variables[0].ty

            cmd_feat = None
            cmd_exts = []
            for feat in self.gen.reg.features:
                if cmd in feat.types:
                    cmd_feat = feat
                    break
            for ext in self.gen.reg.extensions:
                if ext.name in VK_XML_EXTENSION_LIST:
                    if cmd in ext.types:
                        cmd_exts.append(ext)
                        continue

                    for key in ext.optional_types:
                        if (cmd in ext.optional_types[key] and
                            Gen.support_type_depends(key)):
                            cmd_exts.append(ext)

            assert cmd_feat or cmd_exts
            entry = (cmd, cmd_feat, cmd_exts)

            assert(dispatch_handle.category != cmd.HANDLE or
                dispatch_handle.dispatchable)

            if dispatch_handle.category != cmd.HANDLE:
                self.global_commands.append(entry)
            elif dispatch_handle.name == 'VkInstance':
                self.instance_commands.append(entry)
            elif (dispatch_handle.name == 'VkPhysicalDevice' or
                cmd.name == 'vkGetDeviceProcAddr'):
                self.physical_device_commands.append(entry)
            else:
                # it is faster to use the pointers returned by
                # vkGetDeviceProcAddr
                self.device_commands.append(entry)

    def generate(self, template):
        return template.render(
                GEN=self.gen,
                GLOBAL_COMMANDS=sorted(self.global_commands,
                    key=lambda entry: entry[0].name),
                INSTANCE_COMMANDS=sorted(self.instance_commands,
                    key=lambda entry: entry[0].name),
                PHYSICAL_DEVICE_COMMANDS=sorted(self.physical_device_commands,
                    key=lambda entry: entry[0].name),
                DEVICE_COMMANDS=sorted(self.device_commands,
                    key=lambda entry: entry[0].name))

class GenDispatches:
    def __init__(self, gen):
        self.gen = gen

        self.includes = GenStructsAndCommands.RULES.keys()
        self.commands = []
        self.skipped = []
        for ty in self.gen.supported_types[VkType.COMMAND]:
            if self.gen.is_serializable(ty):
                self.commands.append(ty)
            else:
                self.skipped.append(ty)

    def generate(self, template):
        return template.render(
                GEN=self.gen,
                INCLUDES=self.includes,
                COMMAND_TABLE_SIZE=self.gen.reg.max_vk_command_type_value + 1,
                COMMAND_TYPES=self.commands,
                COMMAND_SKIPPED=self.skipped)

def get_args():
    parser = argparse.ArgumentParser()
    parser.add_argument('--outdir', help='Where to write the files.',
                        required=True)
    parser.add_argument('--banner', help='Path to the banner file.')
    parser.add_argument('--renderer',
                        help='Generate for the renderer.',
                        action='store_true')
    return parser.parse_args()

def get_generators(gen):
    classes = [
        GenCS,
        GenDefines,
        GenInfo,
        GenTypes,
        GenHandles,
        GenStructsAndCommands,
        GenUtil,
        GenDispatches,
    ]

    generators = {}
    for cls in classes:
        generators[cls] = cls(gen)

    return generators

def get_file_banner(filename):
    banner = None
    if filename:
        with open(filename, 'rb') as f:
            banner = f.read()
    if not banner:
        banner = '/* This file is generated by venus-protocol. */\n\n'.encode()

    return banner

def get_template(template_name):
    return Template(filename=str(VN_TEMPLATE_DIR.joinpath(template_name)),
                    lookup=VN_TEMPLATE_LOOKUP, output_encoding='utf-8')

def generate_base_headers(generators, base_headers, banner, outdir):
    for cls, name in base_headers:
        generator = generators[cls]
        template = get_template(name)
        output = Path(outdir).joinpath('vn_protocol_' + name)
        with open(output, 'wb') as f:
            f.write(banner)
            f.write(generator.generate(template))

def generate_command_headers(generator, variant, banner, outdir):
    template = get_template(variant + '_commands.h')
    for group in generator.groups:
        output = Path(outdir).joinpath('vn_protocol_%s_%s.h' % (variant, group.name))
        with open(output, 'wb') as f:
            f.write(banner)
            f.write(generator.generate(template, group))

def main():
    args = get_args()

    reg = VkRegistry.parse(VN_PROTOCOL_VK_XML, VN_PROTOCOL_PRIVATE_XMLS)
    gen = Gen(not args.renderer, reg)
    generators = get_generators(gen)

    if gen.is_driver:
        variant = 'driver'
        base_headers = [
            (GenCS,         variant + '_cs.h'),
            (GenDefines,    variant + '_defines.h'),
            (GenInfo,       variant + '_info.h'),
            (GenTypes,      variant + '_types.h'),
            (GenHandles,    variant + '_handles.h'),
        ]
    else:
        variant = 'renderer'
        base_headers = [
            (GenCS,         variant + '_cs.h'),
            (GenDefines,    variant + '_defines.h'),
            (GenInfo,       variant + '_info.h'),
            (GenTypes,      variant + '_types.h'),
            (GenHandles,    variant + '_handles.h'),
            (GenUtil,       variant + '_util.h'),
            (GenDispatches, variant + '_dispatches.h'),
        ]

    banner = '/* This file is generated by venus-protocol.  See vn_protocol_%s.h. */\n\n'
    banner = (banner % variant).encode()

    generate_base_headers(generators, base_headers, banner, args.outdir)

    generator = generators[GenStructsAndCommands]
    generate_command_headers(generator, variant, banner, args.outdir)

    banner = get_file_banner(args.banner)

    # generate a header that includes all other headers
    template = get_template(variant + '.h')
    output = Path(args.outdir).joinpath('vn_protocol_%s.h' % variant)
    with open(output, 'wb') as f:
        template_filenames = [hdr[1] for hdr in base_headers]
        template_filenames.extend(['%s_%s.h' % (variant, name) for name in
            GenStructsAndCommands.RULES.keys()])
        f.write(banner)
        f.write(template.render(TEMPLATE_FILENAMES=template_filenames))

if __name__ == '__main__':
    main()
