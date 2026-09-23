//! The host's real Vulkan, through `ash`: the one implementation of the Venus
//! executor's [`HostVulkan`] trait (EPIC 20 stage 5a.3, ADR-0004).
//!
//! # Loaded, not linked
//!
//! [`AshVulkan::load`] opens the system's Vulkan loader at run time
//! (`ash::Entry::load`: `vulkan-1.dll` on Windows, `libvulkan.so.1` on
//! Linux). Nothing links against it, so a host with no Vulkan at all builds,
//! runs and passes its tests; it simply cannot offer the executing renderer,
//! and `entangled run` says so before a guest ever boots.
//!
//! # Why this is outside `venus`
//!
//! Every Vulkan call is `unsafe`, and the `venus` module family keeps its one
//! `unsafe` in `venus::shmem`. So the executor, the object table and every
//! policy live there and see only the trait; this module is the only code
//! that turns a validated protocol value into a driver structure, and every
//! `unsafe` block in it says what makes that one call sound.
//!
//! What a call's soundness rests on, once, so each `SAFETY` comment can be
//! short:
//!
//! * **Handles are ours.** Every instance, physical device, device and child
//!   handle passed in was created by this type and is still alive: the
//!   executor's object table hands a host object out only while it holds it,
//!   and destroys children before parents ([`crate::venus::executor::objects`]).
//! * **Structures are ours.** Every structure a call reads or writes is built
//!   in the calling function, from `ash`'s `Default` (so `sType` is right and
//!   `pNext` null) and linked with `push_next`, and outlives the call; every
//!   slice it points at is a local `Vec` that does too.
//! * **Values are checked.** Every enum, flag word and count reaching a call
//!   was range-checked by the executor against Vulkan 1.3 core
//!   ([`crate::venus::executor::policy`]).
//! * **External synchronization** is the executor's per-context lock.

pub mod convert;

use std::ffi::{c_char, c_void, CString};

use ash::vk;

use crate::venus::executor::host::{
    DeviceRequest, HostDeviceInfo, HostGroup, HostVulkan, InstanceRequest,
};
use crate::venus::executor::policy::{has_extension, EXTERNAL_MEMORY_HOST};
use crate::venus::executor::GuestDevice;
use crate::venus::protocol::{
    VkCommandPoolCreateInfo, VkExtensionProperties, VkFormat, VkFormatProperties2,
    VkFormatProperties2Next, VkImageAspectFlagBits, VkImageCreateInfo, VkImageCreateInfoNext,
    VkImageFormatProperties2, VkImageFormatProperties2Next, VkMemoryRequirements2,
    VkMemoryRequirements2Next, VkPhysicalDeviceFeatures, VkPhysicalDeviceFeatures2,
    VkPhysicalDeviceImageFormatInfo2, VkPhysicalDeviceImageFormatInfo2Next,
    VkPhysicalDeviceMemoryProperties, VkPhysicalDeviceProperties, VkPhysicalDeviceProperties2,
    VkQueueFamilyProperties, VkResult, VK_SUCCESS,
};
use crate::venus::shmem::RingPages;

use convert::{DeviceLinks, FromAsh, ToAsh};

/// The newest version the renderer uses of any host device.
const API_1_3: u32 = vk::API_VERSION_1_3;

/// The largest `minImportedHostPointerAlignment` the probe will honour. The
/// RTX 2070 says 4 KiB; anything past 2 MiB is a driver the probe does not
/// trust with a real allocation.
const MAX_IMPORT_ALIGNMENT: u64 = 2 << 20;

/// The host's Vulkan loader. See the module docs.
pub struct AshVulkan {
    entry: ash::Entry,
}

impl std::fmt::Debug for AshVulkan {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AshVulkan").finish_non_exhaustive()
    }
}

impl AshVulkan {
    /// Open the system Vulkan loader.
    ///
    /// # Errors
    /// A sentence saying why the loader could not be opened.
    pub fn load() -> Result<Self, String> {
        // SAFETY: `Entry::load` dlopens the platform's Vulkan loader and
        // resolves `vkGetInstanceProcAddr` from it. Its safety condition is
        // that the library really is a Vulkan loader, which the platform's
        // own library name (`vulkan-1.dll`, `libvulkan.so.1`) is; the
        // returned `Entry` keeps the library loaded for as long as it lives.
        let entry = unsafe { ash::Entry::load() }
            .map_err(|e| format!("the host has no usable Vulkan loader: {e}"))?;
        Ok(Self { entry })
    }

    /// The devices a guest would be shown, and why the rest are hidden:
    /// [`crate::venus::executor::probe`] on this host.
    ///
    /// # Errors
    /// Why no device would be shown.
    pub fn usable_devices(&self) -> Result<Vec<GuestDevice>, String> {
        crate::venus::executor::probe(self)
    }

    /// `min(device apiVersion, 1.3)`: the structures a query may chain.
    fn device_api(instance: &ash::Instance, device: vk::PhysicalDevice) -> u32 {
        // SAFETY: `device` was enumerated from `instance` (module docs,
        // "handles are ours"); the call only writes the returned struct.
        let props = unsafe { instance.get_physical_device_properties(device) };
        props.api_version.min(API_1_3)
    }

    /// `vkGetMemoryHostPointerPropertiesEXT` for a page of our own memory:
    /// the memory types a host allocation can be imported into, or `None`.
    ///
    /// A throwaway device with `VK_EXT_external_memory_host` enabled is made
    /// for the question, because the query is device-level; the memory
    /// asked about is a [`RingPages`] allocation, the same kind of pages the
    /// next stage imports for real.
    fn probe_host_import(
        &self,
        instance: &ash::Instance,
        device: vk::PhysicalDevice,
        families: &[VkQueueFamilyProperties],
    ) -> Option<u32> {
        let mut host_props = vk::PhysicalDeviceExternalMemoryHostPropertiesEXT::default();
        let mut props2 = vk::PhysicalDeviceProperties2::default().push_next(&mut host_props);
        // SAFETY: `device` is ours; `props2` and the one structure chained on
        // it are locals that outlive the call.
        unsafe { instance.get_physical_device_properties2(device, &mut props2) };
        let align = host_props.min_imported_host_pointer_alignment;
        if !align.is_power_of_two() || align > MAX_IMPORT_ALIGNMENT {
            return None;
        }
        let family = families.iter().position(|f| f.queue_count > 0)?;
        let family = u32::try_from(family).ok()?;
        let priorities = [1.0f32];
        let queues = [vk::DeviceQueueCreateInfo::default()
            .queue_family_index(family)
            .queue_priorities(&priorities)];
        let names = [vk::EXT_EXTERNAL_MEMORY_HOST_NAME.as_ptr()];
        let info = vk::DeviceCreateInfo::default()
            .queue_create_infos(&queues)
            .enabled_extension_names(&names);
        // SAFETY: `device` is ours; `info`, the queue array and the name
        // array are locals that outlive the call, and the extension is one
        // the device reported (the caller checked).
        let probe = unsafe { instance.create_device(device, &info, None) }.ok()?;

        // Resolved by hand rather than through `ash::ext::external_memory_host`,
        // whose fallback for a missing entry point panics.
        // SAFETY: `probe` is a live device created with the extension
        // enabled, and the name is a NUL-terminated literal.
        let raw = unsafe {
            instance.get_device_proc_addr(
                probe.handle(),
                c"vkGetMemoryHostPointerPropertiesEXT".as_ptr(),
            )
        };
        let answer = raw.and_then(|raw| {
            // SAFETY: the loader returned this pointer for exactly this
            // entry point, whose C signature `PFN_vkGetMemoryHostPointerPropertiesEXT`
            // is; transmuting between two `extern "system"` function pointer
            // types of the same ABI is how every Vulkan loader binding works.
            let get: vk::PFN_vkGetMemoryHostPointerPropertiesEXT =
                unsafe { std::mem::transmute(raw) };
            let pages = RingPages::new(align.checked_mul(2)?).ok()?;
            let base = pages.as_ptr();
            let offset = base.align_offset(usize::try_from(align).ok()?);
            let pointer = base.wrapping_add(offset).cast::<c_void>().cast_const();
            let mut out = vk::MemoryHostPointerPropertiesEXT::default();
            // SAFETY: `get` is the device's own entry point; `pointer` is
            // aligned to `minImportedHostPointerAlignment` and lies inside
            // `pages`, a live allocation of twice that size which outlives
            // the call; `out` is a local of the right type. The query only
            // inspects the pointer, it imports nothing.
            let result = unsafe {
                get(
                    probe.handle(),
                    vk::ExternalMemoryHandleTypeFlags::HOST_ALLOCATION_EXT,
                    pointer,
                    &mut out,
                )
            };
            drop(pages);
            (result == vk::Result::SUCCESS).then_some(out.memory_type_bits)
        });
        // SAFETY: `probe` was created above, owns no child, and is not used
        // after this.
        unsafe { probe.destroy_device(None) };
        answer
    }
}

fn result_code(error: vk::Result) -> VkResult {
    error.as_raw()
}

/// Keep a guest-supplied name only if it can be a C string.
fn c_string(text: &Option<String>) -> Option<CString> {
    text.as_deref().and_then(|t| CString::new(t).ok())
}

impl HostVulkan for AshVulkan {
    type Instance = ash::Instance;
    type PhysicalDevice = vk::PhysicalDevice;
    type Device = ash::Device;
    type Queue = vk::Queue;
    type CommandPool = vk::CommandPool;
    type Image = vk::Image;

    fn instance_version(&self) -> Result<u32, VkResult> {
        // SAFETY: a global command with no arguments but the out value.
        match unsafe { self.entry.try_enumerate_instance_version() } {
            Ok(Some(version)) => Ok(version),
            Ok(None) => Ok(vk::API_VERSION_1_0),
            Err(error) => Err(result_code(error)),
        }
    }

    fn create_instance(&self, request: &InstanceRequest) -> Result<ash::Instance, VkResult> {
        let app_name = c_string(&request.application_name);
        let engine_name = c_string(&request.engine_name);
        let mut app = vk::ApplicationInfo::default()
            .application_version(request.application_version)
            .engine_version(request.engine_version)
            .api_version(API_1_3);
        if let Some(name) = &app_name {
            app = app.application_name(name);
        }
        if let Some(name) = &engine_name {
            app = app.engine_name(name);
        }
        let info = vk::InstanceCreateInfo::default().application_info(&app);
        // SAFETY: `info` and the application info and names it points at
        // are locals that outlive the call; no layer, no extension.
        unsafe { self.entry.create_instance(&info, None) }.map_err(result_code)
    }

    fn destroy_instance(&self, instance: ash::Instance) {
        // SAFETY: the executor destroyed every child first; the value is
        // consumed, so it cannot be used again.
        unsafe { instance.destroy_instance(None) };
    }

    fn enumerate_physical_devices(
        &self,
        instance: &ash::Instance,
    ) -> Result<Vec<vk::PhysicalDevice>, VkResult> {
        // SAFETY: `instance` is ours.
        unsafe { instance.enumerate_physical_devices() }.map_err(result_code)
    }

    fn describe_physical_device(
        &self,
        instance: &ash::Instance,
        device: vk::PhysicalDevice,
    ) -> HostDeviceInfo {
        let api = Self::device_api(instance, device);
        let (properties, features) = if api >= vk::API_VERSION_1_1 {
            // SAFETY: `device` is ours, and `api` gates every chained
            // structure to what its version knows (see `convert`).
            unsafe {
                (
                    convert::query_properties2(instance, device, api),
                    convert::query_features2(instance, device, api),
                )
            }
        } else {
            // SAFETY: `device` is ours; the calls only write their results.
            let (props, feats) = unsafe {
                (
                    instance.get_physical_device_properties(device),
                    instance.get_physical_device_features(device),
                )
            };
            (
                VkPhysicalDeviceProperties2 {
                    p_next: Vec::new(),
                    properties: VkPhysicalDeviceProperties::from_ash(&props),
                },
                VkPhysicalDeviceFeatures2 {
                    p_next: Vec::new(),
                    features: VkPhysicalDeviceFeatures::from_ash(&feats),
                },
            )
        };
        // SAFETY: `device` is ours; each call only writes its results.
        let (families, memory, extensions) = unsafe {
            (
                instance.get_physical_device_queue_family_properties(device),
                instance.get_physical_device_memory_properties(device),
                instance
                    .enumerate_device_extension_properties(device)
                    .unwrap_or_default(),
            )
        };
        let queue_families: Vec<VkQueueFamilyProperties> = families
            .iter()
            .map(VkQueueFamilyProperties::from_ash)
            .collect();
        let extensions: Vec<VkExtensionProperties> = extensions
            .iter()
            .map(VkExtensionProperties::from_ash)
            .collect();
        let host_import_types = (api >= vk::API_VERSION_1_1
            && has_extension(&extensions, EXTERNAL_MEMORY_HOST))
        .then(|| self.probe_host_import(instance, device, &queue_families))
        .flatten();
        HostDeviceInfo {
            properties,
            features,
            queue_families,
            memory: VkPhysicalDeviceMemoryProperties::from_ash(&memory),
            extensions,
            host_import_types,
        }
    }

    fn physical_device_groups(
        &self,
        instance: &ash::Instance,
    ) -> Result<Vec<HostGroup<vk::PhysicalDevice>>, VkResult> {
        // SAFETY: `instance` is ours; the second call writes exactly the
        // `len` structures the first reported, into a local vector of them.
        let groups = unsafe {
            let len = instance
                .enumerate_physical_device_groups_len()
                .map_err(result_code)?;
            let mut groups = vec![vk::PhysicalDeviceGroupProperties::default(); len];
            instance
                .enumerate_physical_device_groups(&mut groups)
                .map_err(result_code)?;
            groups
        };
        Ok(groups
            .iter()
            .map(|g| {
                let n = usize::try_from(g.physical_device_count)
                    .unwrap_or(0)
                    .min(g.physical_devices.len());
                HostGroup {
                    members: g.physical_devices.iter().take(n).copied().collect(),
                    subset_allocation: g.subset_allocation != 0,
                }
            })
            .collect())
    }

    fn format_properties(
        &self,
        instance: &ash::Instance,
        device: vk::PhysicalDevice,
        format: VkFormat,
        out: &mut VkFormatProperties2,
    ) {
        let api = Self::device_api(instance, device);
        let wants3 = api >= API_1_3
            && out
                .p_next
                .iter()
                .any(|l| matches!(l, VkFormatProperties2Next::VkFormatProperties3(_)));
        let mut props3 = vk::FormatProperties3::default();
        let mut head = vk::FormatProperties2::default();
        if wants3 {
            head = head.push_next(&mut props3);
        }
        // SAFETY: `device` is ours, `format` a checked 1.3 core format, and
        // `head` and its chain are locals that outlive the call.
        unsafe {
            instance.get_physical_device_format_properties2(
                device,
                vk::Format::from_raw(format),
                &mut head,
            );
        }
        out.format_properties = FromAsh::from_ash(&head.format_properties);
        for link in &mut out.p_next {
            let VkFormatProperties2Next::VkFormatProperties3(p) = link;
            if wants3 {
                *p = FromAsh::from_ash(&props3);
            }
        }
    }

    fn image_format_properties(
        &self,
        instance: &ash::Instance,
        device: vk::PhysicalDevice,
        info: &VkPhysicalDeviceImageFormatInfo2,
        out: &mut VkImageFormatProperties2,
    ) -> VkResult {
        let mut external = None;
        let mut list: Option<Vec<vk::Format>> = None;
        let mut stencil = None;
        for link in &info.p_next {
            match link {
                VkPhysicalDeviceImageFormatInfo2Next::VkPhysicalDeviceExternalImageFormatInfo(
                    e,
                ) => {
                    external = Some(e.to_ash());
                }
                VkPhysicalDeviceImageFormatInfo2Next::VkImageFormatListCreateInfo(l) => {
                    list = Some(
                        l.p_view_formats
                            .as_deref()
                            .unwrap_or_default()
                            .iter()
                            .map(|f| vk::Format::from_raw(*f))
                            .collect(),
                    );
                }
                VkPhysicalDeviceImageFormatInfo2Next::VkImageStencilUsageCreateInfo(s) => {
                    stencil = Some(s.to_ash());
                }
            }
        }
        let mut list_info = list
            .as_deref()
            .map(|formats| vk::ImageFormatListCreateInfo::default().view_formats(formats));
        let mut query: vk::PhysicalDeviceImageFormatInfo2<'_> = info.to_ash();
        if let Some(e) = external.as_mut() {
            query = query.push_next(e);
        }
        if let Some(l) = list_info.as_mut() {
            query = query.push_next(l);
        }
        if let Some(s) = stencil.as_mut() {
            query = query.push_next(s);
        }

        let mut external_out = vk::ExternalImageFormatProperties::default();
        let mut ycbcr_out = vk::SamplerYcbcrConversionImageFormatProperties::default();
        let wants_external = out.p_next.iter().any(|l| {
            matches!(
                l,
                VkImageFormatProperties2Next::VkExternalImageFormatProperties(_)
            )
        });
        let wants_ycbcr = out.p_next.iter().any(|l| {
            matches!(
                l,
                VkImageFormatProperties2Next::VkSamplerYcbcrConversionImageFormatProperties(_)
            )
        });
        let mut head = vk::ImageFormatProperties2::default();
        if wants_external {
            head = head.push_next(&mut external_out);
        }
        if wants_ycbcr {
            head = head.push_next(&mut ycbcr_out);
        }
        // SAFETY: `device` is ours; every field of `query` was checked
        // against 1.3 core; `query`, `head`, both chains and the view-format
        // slice are locals that outlive the call.
        let result = unsafe {
            instance.get_physical_device_image_format_properties2(device, &query, &mut head)
        };
        out.image_format_properties = FromAsh::from_ash(&head.image_format_properties);
        for link in &mut out.p_next {
            match link {
                VkImageFormatProperties2Next::VkExternalImageFormatProperties(p) => {
                    *p = FromAsh::from_ash(&external_out);
                }
                VkImageFormatProperties2Next::VkSamplerYcbcrConversionImageFormatProperties(p) => {
                    *p = FromAsh::from_ash(&ycbcr_out);
                }
            }
        }
        match result {
            Ok(()) => VK_SUCCESS,
            Err(error) => result_code(error),
        }
    }

    fn create_device(
        &self,
        instance: &ash::Instance,
        device: vk::PhysicalDevice,
        request: &DeviceRequest<vk::PhysicalDevice>,
    ) -> Result<ash::Device, VkResult> {
        let queues: Vec<vk::DeviceQueueCreateInfo<'_>> = request
            .queues
            .iter()
            .map(|q| {
                vk::DeviceQueueCreateInfo::default()
                    .flags(vk::DeviceQueueCreateFlags::from_raw(q.flags))
                    .queue_family_index(q.family)
                    .queue_priorities(&q.priorities)
            })
            .collect();
        let names: Vec<CString> = request
            .extensions
            .iter()
            .filter_map(|e| CString::new(e.as_str()).ok())
            .collect();
        let name_ptrs: Vec<*const c_char> = names.iter().map(|n| n.as_ptr()).collect();
        let features = request.features.as_ref().map(ToAsh::to_ash);
        let mut group = request
            .group
            .as_deref()
            .map(|members| vk::DeviceGroupDeviceCreateInfo::default().physical_devices(members));
        let mut links = DeviceLinks::new(&request.chain);
        let mut info = vk::DeviceCreateInfo::default()
            .queue_create_infos(&queues)
            .enabled_extension_names(&name_ptrs);
        if let Some(f) = features.as_ref() {
            info = info.enabled_features(f);
        }
        if let Some(g) = group.as_mut() {
            info = info.push_next(g);
        }
        info = links.push(info);
        // SAFETY: `device` is ours; the create info was rebuilt from checked
        // protocol values (families and counts within the host's, priorities
        // in [0, 1], extensions advertised or the renderer's own, features
        // no more than reported); every array and chained structure it
        // points at is a local that outlives the call.
        unsafe { instance.create_device(device, &info, None) }.map_err(result_code)
    }

    fn destroy_device(&self, device: ash::Device) {
        // SAFETY: the executor destroyed every child first; the value is
        // consumed. Waiting for idle first is what vkr does on its worker
        // thread; this stage submits nothing, so it returns at once.
        unsafe {
            let _ = device.device_wait_idle();
            device.destroy_device(None);
        }
    }

    fn device_queue(&self, device: &ash::Device, flags: u32, family: u32, index: u32) -> vk::Queue {
        if flags == 0 {
            // SAFETY: `(family, index)` is a queue the device was created
            // with (the executor checked); plain `vkGetDeviceQueue` for
            // flags 0, as vkr does for drivers that predate the spec fix.
            unsafe { device.get_device_queue(family, index) }
        } else {
            let info = vk::DeviceQueueInfo2::default()
                .flags(vk::DeviceQueueCreateFlags::from_raw(flags))
                .queue_family_index(family)
                .queue_index(index);
            // SAFETY: as above, with the flags it was created with.
            unsafe { device.get_device_queue2(&info) }
        }
    }

    fn create_command_pool(
        &self,
        device: &ash::Device,
        info: &VkCommandPoolCreateInfo,
    ) -> Result<vk::CommandPool, VkResult> {
        let info = info.to_ash();
        // SAFETY: `device` is ours; flags and family were checked.
        unsafe { device.create_command_pool(&info, None) }.map_err(result_code)
    }

    fn destroy_command_pool(&self, device: &ash::Device, pool: vk::CommandPool) {
        // SAFETY: `pool` was created on `device` and is destroyed once.
        unsafe { device.destroy_command_pool(pool, None) };
    }

    fn create_image(
        &self,
        device: &ash::Device,
        info: &VkImageCreateInfo,
    ) -> Result<vk::Image, VkResult> {
        let families: Vec<u32> = info.p_queue_family_indices.clone().unwrap_or_default();
        let mut external = None;
        let mut list: Option<Vec<vk::Format>> = None;
        let mut stencil = None;
        for link in &info.p_next {
            match link {
                VkImageCreateInfoNext::VkExternalMemoryImageCreateInfo(e) => {
                    external = Some(e.to_ash());
                }
                VkImageCreateInfoNext::VkImageFormatListCreateInfo(l) => {
                    list = Some(
                        l.p_view_formats
                            .as_deref()
                            .unwrap_or_default()
                            .iter()
                            .map(|f| vk::Format::from_raw(*f))
                            .collect(),
                    );
                }
                VkImageCreateInfoNext::VkImageStencilUsageCreateInfo(s) => {
                    stencil = Some(s.to_ash());
                }
            }
        }
        let mut list_info = list
            .as_deref()
            .map(|formats| vk::ImageFormatListCreateInfo::default().view_formats(formats));
        let mut create = vk::ImageCreateInfo::default()
            .flags(vk::ImageCreateFlags::from_raw(info.flags))
            .image_type(vk::ImageType::from_raw(info.image_type))
            .format(vk::Format::from_raw(info.format))
            .extent(info.extent.to_ash())
            .mip_levels(info.mip_levels)
            .array_layers(info.array_layers)
            .samples(vk::SampleCountFlags::from_raw(
                u32::try_from(info.samples).unwrap_or(1),
            ))
            .tiling(vk::ImageTiling::from_raw(info.tiling))
            .usage(vk::ImageUsageFlags::from_raw(info.usage))
            .sharing_mode(vk::SharingMode::from_raw(info.sharing_mode))
            .initial_layout(vk::ImageLayout::from_raw(info.initial_layout));
        if info.sharing_mode == vk::SharingMode::CONCURRENT.as_raw() {
            create = create.queue_family_indices(&families);
        }
        if let Some(e) = external.as_mut() {
            create = create.push_next(e);
        }
        if let Some(l) = list_info.as_mut() {
            create = create.push_next(l);
        }
        if let Some(s) = stencil.as_mut() {
            create = create.push_next(s);
        }
        // SAFETY: `device` is ours; every field was range-checked and the
        // image checked against the host's own format limits; the create
        // info, its chain and its slices are locals that outlive the call.
        unsafe { device.create_image(&create, None) }.map_err(result_code)
    }

    fn destroy_image(&self, device: &ash::Device, image: vk::Image) {
        // SAFETY: `image` was created on `device` and is destroyed once.
        unsafe { device.destroy_image(image, None) };
    }

    fn image_memory_requirements(
        &self,
        device: &ash::Device,
        image: vk::Image,
        plane: Option<VkImageAspectFlagBits>,
        out: &mut VkMemoryRequirements2,
    ) {
        let mut plane_info = plane.map(|aspect| {
            vk::ImagePlaneMemoryRequirementsInfo::default().plane_aspect(
                vk::ImageAspectFlags::from_raw(u32::try_from(aspect).unwrap_or(0)),
            )
        });
        let mut info = vk::ImageMemoryRequirementsInfo2::default().image(image);
        if let Some(p) = plane_info.as_mut() {
            info = info.push_next(p);
        }
        let wants_dedicated = !out.p_next.is_empty();
        let mut dedicated = vk::MemoryDedicatedRequirements::default();
        let mut head = vk::MemoryRequirements2::default();
        if wants_dedicated {
            head = head.push_next(&mut dedicated);
        }
        // SAFETY: `image` was created on `device`; the plane aspect was
        // checked; `info`, `head` and their chains are locals that outlive
        // the call.
        unsafe { device.get_image_memory_requirements2(&info, &mut head) };
        out.memory_requirements = FromAsh::from_ash(&head.memory_requirements);
        for link in &mut out.p_next {
            let VkMemoryRequirements2Next::VkMemoryDedicatedRequirements(d) = link;
            *d = FromAsh::from_ash(&dedicated);
        }
    }
}

#[cfg(test)]
mod tests;
