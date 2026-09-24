//! One venus context's Vulkan: its object table, its host, and what each of
//! the bring-up's commands does to them (spec §1.2 rows 2–26). Memory,
//! buffers, image binding and views are [`super::memory`]'s.
//!
//! Every command follows virglrenderer's `vkr_dispatch_*` unless the function
//! says otherwise. The shape of each is the same: resolve every id to a host
//! object (a failure is fatal), validate every enum, flag word and count the
//! guest chose (a failure is fatal), then either answer from what the context
//! already knows or make exactly one host call, and put the answer into the
//! command's own output skeletons, which the caller encodes as the reply.
//!
//! Fatal means the context: vkr's `cs_fatal_error` is per context, and so is
//! [`VulkanContext::fatal`]. Once set, every ring of the context refuses
//! everything.

use std::collections::HashSet;
use std::sync::Arc;

use crate::venus::shmem::PageBudget;

use thiserror::Error;

use crate::venus::protocol::{
    ChainLink, Command, CreateCommandPoolArgs, CreateDeviceArgs, CreateImageArgs,
    CreateInstanceArgs, DestroyCommandPoolArgs, DestroyDeviceArgs, DestroyImageArgs,
    DestroyInstanceArgs, EnumerateDeviceExtensionPropertiesArgs, EnumeratePhysicalDeviceGroupsArgs,
    EnumeratePhysicalDevicesArgs, GetDeviceQueue2Args, GetImageMemoryRequirements2Args,
    GetPhysicalDeviceFeatures2Args, GetPhysicalDeviceFormatProperties2Args,
    GetPhysicalDeviceImageFormatProperties2Args, GetPhysicalDeviceMemoryProperties2Args,
    GetPhysicalDeviceProperties2Args, GetPhysicalDevicePropertiesArgs,
    GetPhysicalDeviceQueueFamilyProperties2Args, VkDeviceCreateInfoNext, VkDeviceQueueInfo2Next,
    VkImageCreateInfo, VkImageCreateInfoNext, VkImageFormatProperties2,
    VkImageFormatProperties2Next, VkImageMemoryRequirementsInfo2Next, VkMemoryRequirements2,
    VkMemoryRequirements2Next, VkPhysicalDevice, VkPhysicalDeviceGroupProperties,
    VkPhysicalDeviceImageFormatInfo2, VkPhysicalDeviceImageFormatInfo2Next,
    VkQueueFamilyProperties2, VkResult, VK_ERROR_EXTENSION_NOT_PRESENT,
    VK_ERROR_FEATURE_NOT_PRESENT, VK_ERROR_FORMAT_NOT_SUPPORTED, VK_ERROR_INITIALIZATION_FAILED,
    VK_ERROR_INVALID_DRM_FORMAT_MODIFIER_PLANE_LAYOUT_EXT, VK_ERROR_LAYER_NOT_PRESENT,
    VK_ERROR_OUT_OF_DEVICE_MEMORY, VK_ERROR_OUT_OF_HOST_MEMORY, VK_ERROR_UNKNOWN, VK_INCOMPLETE,
    VK_SHARING_MODE_CONCURRENT, VK_SUCCESS,
};
use crate::venus::wire::Encoder;

use super::host::{DeviceRequest, HostVulkan, InstanceRequest, QueueRequest, ResourceMemory};
use super::memory::{
    external_handle_types, external_type_bits, image_facts, image_planes, resource_memory,
};
use super::modifier::{self, ModifierLayout};
use super::objects::{
    CreatedQueue, DeviceChild, DeviceObject, ExposedDevice, IdError, ImageObject, Kind, Objects,
    Pending, QueueObject,
};
use super::policy::{self, GuestDevice, MAX_API_VERSION, MIN_API_VERSION};

/// The fence timelines a context has (`VIRTGPU_CONTEXT_PARAM_NUM_RINGS` = 64
/// in Mesa, `ARRAY_SIZE(ctx->sync_queues)` in vkr); timeline 0 is the CPU's.
pub const MAX_RING_IDX: u32 = 64;

/// Why a command could not be executed. Every variant is fatal to the
/// context, and to the ring that carried it.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum ExecError {
    /// An id the table refused.
    #[error("{command}: {error}")]
    Id {
        /// The command.
        command: &'static str,
        /// What was wrong with the id.
        error: IdError,
    },
    /// An id inside a command's structures that translation refused
    /// (stage 5b.2): the field it was in, and why.
    #[error("{command}: {field}: {error}")]
    IdIn {
        /// The command.
        command: &'static str,
        /// The member or parameter, `Struct.member`.
        field: &'static str,
        /// What was wrong with the id.
        error: IdError,
    },
    /// The host refused a translated command before calling the driver.
    #[error("{command}: {error}")]
    HostCall {
        /// The command.
        command: &'static str,
        /// Why.
        error: super::host::CallError,
    },
    /// A command newer than the device's Vulkan version as the guest sees it.
    #[error("{command} is Vulkan {major}.{minor}, newer than the device the guest was shown")]
    TooNew {
        /// The command.
        command: &'static str,
        /// Its core major version.
        major: u32,
        /// Its core minor version.
        minor: u32,
    },
    /// An admitted extension's command on a device the guest enabled none of
    /// the extensions that bring it on (stage 5c): its host entry point may
    /// not exist.
    #[error("{command} needs one of {extensions:?}, which its device did not enable")]
    NotEnabled {
        /// The command.
        command: &'static str,
        /// The extensions that bring it.
        extensions: &'static [&'static str],
    },
    /// A semaphore used in a way its type or state forbids (stage 5b.3): a
    /// binary wait with no signal before it, a signal of a binary semaphore
    /// already signalled, a timeline-only command on a binary one.
    #[error("{command}: semaphore {id:#x}: {what}")]
    Semaphore {
        /// The command.
        command: &'static str,
        /// The semaphore's guest id.
        id: u64,
        /// What was wrong.
        what: &'static str,
    },
    /// A second `vkCreateInstance` on one context (`vkr_instance.c:87-90`).
    #[error("vkCreateInstance on a context that already has instance {0:#x}")]
    SecondInstance(u64),
    /// A value the guest chose that no correct driver sends — an enum out of
    /// range, a flag bit Vulkan 1.3 does not define, a count that contradicts
    /// another.
    #[error("{command}: {what}")]
    Invalid {
        /// The command.
        command: &'static str,
        /// What was wrong.
        what: String,
    },
    /// The context already went fatal on another ring.
    #[error("venus context {0} is fatal and executes nothing more")]
    ContextFatal(u32),
    /// A command the protocol decodes and this stage does not implement.
    /// Fatal as an undecodable one was: accepting it without doing it would
    /// leave the guest believing in work that never happened.
    #[error("{command} is decodable but not implemented by this renderer")]
    NotImplemented {
        /// The command.
        command: &'static str,
    },
    /// A chained structure the protocol decodes and this stage does not
    /// accept ([`policy::admits_link`]).
    #[error(
        "{command}: {parent} chains sType {stype} ({name}), which this renderer does not implement"
    )]
    UnimplementedLink {
        /// The command.
        command: &'static str,
        /// The structure whose chain carried it.
        parent: &'static str,
        /// The link's `VkStructureType`.
        stype: i32,
        /// Its structure name, when the protocol knows it.
        name: &'static str,
    },
}

/// The first chained structure `command` carries that the policy does not
/// admit, as the refusal it is.
fn unadmitted_link(command: &Command<'_>) -> Option<ExecError> {
    let mut found = None;
    command.for_each_link(&mut |parent, stype| {
        if found.is_none() && !policy::admits_link(stype) {
            found = Some((parent, stype));
        }
    });
    found.map(|(parent, stype)| unimplemented_link(command.name(), parent, stype))
}

pub(super) fn unimplemented_link(
    command: &'static str,
    parent: &'static str,
    stype: i32,
) -> ExecError {
    ExecError::UnimplementedLink {
        command,
        parent,
        stype,
        name: crate::venus::protocol::info::structure(stype).map_or("unknown", |s| s.name),
    }
}

pub(super) fn invalid(command: &'static str, what: impl Into<String>) -> ExecError {
    ExecError::Invalid {
        command,
        what: what.into(),
    }
}

pub(super) fn id_error(command: &'static str) -> impl Fn(IdError) -> ExecError {
    move |error| ExecError::Id { command, error }
}

/// One context's Vulkan state, shared by its rings behind a lock (which also
/// satisfies Vulkan's external-synchronization rules for everything this
/// stage calls).
pub struct VulkanContext<H: HostVulkan> {
    pub(super) ctx_id: u32,
    pub(super) host: Arc<H>,
    pub(super) objects: Objects<H>,
    pub(super) fatal: bool,
    /// The driver answered `VK_ERROR_DEVICE_LOST`: the command that saw it
    /// is answered, and the context goes fatal straight after
    /// ([`Self::take_lost`]).
    pub(super) lost: bool,
    /// The renderer-wide budget every host-visible allocation's pages are
    /// charged to ([`super::MAX_HOST_VISIBLE_BYTES`]).
    pub(super) budget: Arc<PageBudget>,
    /// The stop signal of the ring whose command is executing, for a wait
    /// that must give up when the ring is torn down.
    pub(super) stop: Option<crate::venus::service::StopSignal>,
    /// The host blobs this context may reach (its rings' [`ContextBlobs`]):
    /// where an import of a blob of memory finds its pages (stage 5c).
    /// `None` until a ring of the context exists.
    pub(super) blobs: Option<crate::venus::renderer::ContextBlobs>,
}

impl<H: HostVulkan> VulkanContext<H> {
    /// An empty context on `host`, with a host-visible budget of its own
    /// ([`super::MAX_HOST_VISIBLE_BYTES`]).
    pub fn new(ctx_id: u32, host: Arc<H>) -> Self {
        Self::with_budget(ctx_id, host, PageBudget::new(super::MAX_HOST_VISIBLE_BYTES))
    }

    /// An empty context on `host`, charging host-visible memory to `budget`
    /// — the one every context of a renderer shares.
    pub fn with_budget(ctx_id: u32, host: Arc<H>, budget: Arc<PageBudget>) -> Self {
        Self {
            ctx_id,
            host,
            objects: Objects::default(),
            fatal: false,
            lost: false,
            budget,
            stop: None,
            blobs: None,
        }
    }

    /// Whether a command has seen `VK_ERROR_DEVICE_LOST` since the last ask;
    /// the caller makes the context fatal once the command's reply is out.
    pub fn take_lost(&mut self) -> bool {
        std::mem::take(&mut self.lost)
    }

    /// The host device, a queue of it and its family, and a buffer of it,
    /// for a real-GPU test that has the host do something to a buffer the
    /// guest made.
    #[cfg(test)]
    pub(crate) fn with_host_buffer<T>(
        &self,
        device: u64,
        buffer: u64,
        f: impl FnOnce(&H, &H::Device, H::Queue, u32, H::Buffer) -> T,
    ) -> Option<T> {
        let host_device = self.objects.device(device).ok()?;
        let queue = self.objects.any_queue(device)?;
        let buffer = self.objects.buffer(device, buffer).ok()?;
        Some(f(
            &self.host,
            &host_device.host,
            queue.host,
            queue.family,
            buffer.host,
        ))
    }

    /// Guest-visible objects in the table (the physical devices included).
    #[must_use]
    pub fn object_count(&self) -> usize {
        self.objects.len()
    }

    /// Whether the context went fatal.
    #[must_use]
    pub fn is_fatal(&self) -> bool {
        self.fatal
    }

    /// Mark the context fatal: every ring stops at its next command.
    pub fn set_fatal(&mut self) {
        self.fatal = true;
    }

    /// Destroy every host object, in dependency order. Used by context
    /// destruction and device reset; the table is empty afterwards.
    pub fn destroy_all(&mut self) {
        let host = Arc::clone(&self.host);
        self.objects.destroy_all(&host);
    }

    /// Execute one decoded command, filling its outputs in place.
    ///
    /// # Errors
    /// [`ExecError`], after which the caller must end the context.
    pub fn execute(&mut self, command: &mut Command<'_>) -> Result<(), ExecError> {
        if self.fatal {
            return Err(ExecError::ContextFatal(self.ctx_id));
        }
        let result = match unadmitted_link(command) {
            Some(error) => Err(error),
            None => self.dispatch(command),
        };
        if result.is_err() {
            self.fatal = true;
        }
        result
    }

    /// One slice of a wait command, at most `slice` long, with every rule
    /// [`Self::execute`] applies ([`super::submit`]'s `wait_slice`).
    /// `Ok(true)` once the command is answered.
    ///
    /// # Errors
    /// As [`Self::execute`].
    pub fn execute_wait(
        &mut self,
        command: &mut Command<'_>,
        slice: std::time::Duration,
    ) -> Result<bool, ExecError> {
        if self.fatal {
            return Err(ExecError::ContextFatal(self.ctx_id));
        }
        let result = match unadmitted_link(command) {
            Some(error) => Err(error),
            None => self.wait_slice(command, slice),
        };
        if result.is_err() {
            self.fatal = true;
        }
        result
    }

    /// [`Self::execute`] after the chains passed: one arm per command this
    /// stage implements, and a refusal for every other one the protocol
    /// decodes.
    fn dispatch(&mut self, command: &mut Command<'_>) -> Result<(), ExecError> {
        match command {
            Command::EnumerateInstanceVersion(args) => {
                match self.host.instance_version() {
                    Ok(version) => {
                        args.ret = VK_SUCCESS;
                        args.p_api_version = Some(policy::cap_minor(version, MAX_API_VERSION));
                    }
                    Err(ret) => {
                        args.ret = ret;
                        args.p_api_version = Some(0);
                    }
                }
                Ok(())
            }
            Command::CreateInstance(args) => self.create_instance(args),
            Command::DestroyInstance(args) => self.destroy_instance(args),
            Command::EnumeratePhysicalDevices(args) => self.enumerate_physical_devices(args),
            Command::EnumeratePhysicalDeviceGroups(args) => self.enumerate_groups(args),
            Command::GetPhysicalDeviceProperties(args) => self.properties(args),
            Command::GetPhysicalDeviceProperties2(args) => self.properties2(args),
            Command::GetPhysicalDeviceFeatures2(args) => self.features2(args),
            Command::GetPhysicalDeviceQueueFamilyProperties2(args) => self.queue_families(args),
            Command::GetPhysicalDeviceMemoryProperties2(args) => self.memory_properties(args),
            Command::EnumerateDeviceExtensionProperties(args) => self.device_extensions(args),
            Command::GetPhysicalDeviceExternalSemaphoreProperties(args) => {
                self.external_semaphore_properties(args)
            }
            Command::GetPhysicalDeviceExternalBufferProperties(args) => {
                self.external_buffer_properties(args)
            }
            Command::GetPhysicalDeviceFormatProperties2(args) => self.format_properties(args),
            Command::GetPhysicalDeviceImageFormatProperties2(args) => {
                self.image_format_properties(args)
            }
            Command::CreateDevice(args) => self.create_device(args),
            Command::DestroyDevice(args) => self.destroy_device(args),
            Command::GetDeviceQueue2(args) => self.device_queue(args),
            Command::CreateCommandPool(args) => self.create_command_pool(args),
            Command::DestroyCommandPool(args) => self.destroy_command_pool(args),
            Command::CreateImage(args) => self.create_image(args),
            Command::DestroyImage(args) => self.destroy_image(args),
            Command::GetImageMemoryRequirements2(args) => self.image_memory_requirements(args),
            Command::AllocateMemory(args) => self.allocate_memory(args),
            Command::FreeMemory(args) => self.free_memory(args),
            Command::GetDeviceMemoryCommitment(args) => self.memory_commitment(args),
            Command::CreateBuffer(args) => self.create_buffer(args),
            Command::DestroyBuffer(args) => self.destroy_buffer(args),
            Command::GetBufferMemoryRequirements(args) => self.buffer_requirements(args),
            Command::GetBufferMemoryRequirements2(args) => self.buffer_requirements2(args),
            Command::GetDeviceBufferMemoryRequirements(args) => {
                self.device_buffer_requirements(args)
            }
            Command::BindBufferMemory(args) => self.bind_buffer_memory(args),
            Command::BindBufferMemory2(args) => self.bind_buffer_memory2(args),
            Command::GetBufferDeviceAddress(args) => self.buffer_device_address(args),
            Command::CreateBufferView(args) => self.create_buffer_view(args),
            Command::DestroyBufferView(args) => self.destroy_buffer_view(args),
            Command::GetImageMemoryRequirements(args) => self.image_requirements(args),
            Command::GetDeviceImageMemoryRequirements(args) => self.device_image_requirements(args),
            Command::BindImageMemory(args) => self.bind_image_memory(args),
            Command::BindImageMemory2(args) => self.bind_image_memory2(args),
            Command::GetImageSubresourceLayout(args) => self.image_subresource_layout(args),
            Command::CreateImageView(args) => self.create_image_view(args),
            Command::DestroyImageView(args) => self.destroy_image_view(args),
            // Stage 5b.2 (`device_objects`), and a refusal for every other
            // decodable command there. `vkGetBufferOpaqueCaptureAddress` and
            // `vkGetDeviceMemoryOpaqueCaptureAddress` are refused on purpose:
            // they are only valid with `bufferDeviceAddressCaptureReplay`,
            // which `policy::mask_features` reports false.
            other => self.dispatch_objects(other),
        }
    }

    // ------------------------------------------------------------ instance

    /// `vkCreateInstance`, as `vkr_dispatch_vkCreateInstance`: a second one on
    /// the context is fatal, any layer is `VK_ERROR_LAYER_NOT_PRESENT` and any
    /// instance extension `VK_ERROR_EXTENSION_NOT_PRESENT` (the decodable set
    /// holds no instance extension a host could enable, so "only decodable and
    /// supported ones" and vkr's "none" are the same rule today).
    ///
    /// Differs from vkr in one respect: the host instance is created at
    /// Vulkan 1.3 whatever the guest's `apiVersion`, rather than at
    /// `max(guest, 1.1)`, so every structure the protocol can chain is
    /// queryable. `flags` is not forwarded (1.3 core defines none).
    fn create_instance(&mut self, args: &mut CreateInstanceArgs<'_>) -> Result<(), ExecError> {
        const NAME: &str = "vkCreateInstance";
        if let Some(existing) = self.objects.instance_id() {
            return Err(ExecError::SecondInstance(existing));
        }
        let id = args.p_instance.map(|h| h.0).unwrap_or(0);
        self.objects
            .check_new(id, Kind::Instance)
            .map_err(id_error(NAME))?;
        let Some(info) = &args.p_create_info else {
            return Err(invalid(NAME, "pCreateInfo is null"));
        };
        if info.enabled_layer_count != 0 {
            args.ret = VK_ERROR_LAYER_NOT_PRESENT;
            return Ok(());
        }
        if info.enabled_extension_count != 0 {
            args.ret = VK_ERROR_EXTENSION_NOT_PRESENT;
            return Ok(());
        }
        match self.host.instance_version() {
            Err(ret) => {
                args.ret = ret;
                return Ok(());
            }
            Ok(version) if version < MIN_API_VERSION => {
                args.ret = VK_ERROR_INITIALIZATION_FAILED;
                return Ok(());
            }
            Ok(_) => {}
        }
        if info.flags != 0 {
            tracing::debug!(
                ctx_id = self.ctx_id,
                flags = info.flags,
                "vkCreateInstance flags are not forwarded"
            );
        }
        let text = |bytes: Option<&[u8]>| {
            bytes
                .and_then(|b| std::str::from_utf8(b).ok())
                .map(str::to_owned)
        };
        let request = match &info.p_application_info {
            Some(app) => InstanceRequest {
                application_name: text(app.p_application_name),
                application_version: app.application_version,
                engine_name: text(app.p_engine_name),
                engine_version: app.engine_version,
            },
            None => InstanceRequest::default(),
        };
        match self.host.create_instance(&request) {
            Ok(instance) => {
                self.objects.insert_instance(id, instance);
                args.ret = VK_SUCCESS;
            }
            Err(ret) => args.ret = ret,
        }
        Ok(())
    }

    /// `vkDestroyInstance`: must name the context's instance
    /// (`vkr_dispatch_vkDestroyInstance`); everything under it goes too.
    fn destroy_instance(&mut self, args: &DestroyInstanceArgs) -> Result<(), ExecError> {
        self.objects
            .instance(args.instance.0)
            .map_err(id_error("vkDestroyInstance"))?;
        self.destroy_all();
        Ok(())
    }

    /// The instance's exposed devices, enumerating and judging the host's the
    /// first time (`vkr_instance_enumerate_physical_devices`, which caches in
    /// the same way). `Err` is the host's `VkResult`.
    fn exposed(
        &mut self,
        command: &'static str,
        instance: u64,
    ) -> Result<Result<usize, VkResult>, ExecError> {
        let host = Arc::clone(&self.host);
        let ctx_id = self.ctx_id;
        let object = self
            .objects
            .instance_mut(instance)
            .map_err(id_error(command))?;
        if let Some(devices) = &object.devices {
            return Ok(Ok(devices.len()));
        }
        let handles = match host.enumerate_physical_devices(&object.host) {
            Ok(handles) => handles,
            Err(ret) => return Ok(Err(ret)),
        };
        let mut devices = Vec::new();
        for handle in handles {
            let info = host.describe_physical_device(&object.host, handle);
            let name =
                String::from_utf8_lossy(policy::c_name(&info.properties.properties.device_name))
                    .into_owned();
            match policy::expose(info) {
                Ok(guest) => {
                    tracing::info!(ctx_id, device = %name, "exposing a host Vulkan device to the guest");
                    devices.push(ExposedDevice {
                        host: handle,
                        guest,
                        id: None,
                    });
                }
                Err(why) => {
                    tracing::info!(ctx_id, device = %name, %why, "hiding a host Vulkan device from the guest");
                }
            }
        }
        let count = devices.len();
        object.devices = Some(devices);
        Ok(Ok(count))
    }

    /// `vkEnumeratePhysicalDevices`, as vkr's: the count protocol exactly
    /// (`VK_INCOMPLETE` when the guest's array is short, never more elements
    /// than it sized), and the guest's pre-assigned ids bound by position on
    /// the first call that carries them — a later call must present the same
    /// ids (`vkr_physical_device.c:369-373`).
    fn enumerate_physical_devices(
        &mut self,
        args: &mut EnumeratePhysicalDevicesArgs,
    ) -> Result<(), ExecError> {
        const NAME: &str = "vkEnumeratePhysicalDevices";
        let count = match self.exposed(NAME, args.instance.0)? {
            Ok(count) => count,
            Err(ret) => {
                args.ret = ret;
                return Ok(());
            }
        };
        let Some(ids) = args.p_physical_devices.as_mut() else {
            args.p_physical_device_count = Some(u32::try_from(count).unwrap_or(u32::MAX));
            args.ret = VK_SUCCESS;
            return Ok(());
        };
        let asked = ids.len();
        let n = asked.min(count);
        args.ret = if asked < count {
            VK_INCOMPLETE
        } else {
            VK_SUCCESS
        };
        ids.truncate(n);
        for (index, guest) in ids.iter().enumerate() {
            let bound = self
                .objects
                .instance(args.instance.0)
                .map_err(id_error(NAME))?
                .devices
                .as_ref()
                .and_then(|devices| devices.get(index))
                .and_then(|device| device.id);
            match bound {
                Some(id) if id == guest.0 => {}
                Some(id) => {
                    return Err(invalid(
                        NAME,
                        format!(
                        "physical device {index} is {id:#x}, and a re-enumeration named it {:#x}",
                        guest.0
                    ),
                    ))
                }
                None => {
                    self.objects
                        .check_new(guest.0, Kind::PhysicalDevice)
                        .map_err(id_error(NAME))?;
                    self.objects.insert_physical(guest.0, index);
                    if let Some(device) = self
                        .objects
                        .instance_mut(args.instance.0)
                        .map_err(id_error(NAME))?
                        .devices
                        .as_mut()
                        .and_then(|devices| devices.get_mut(index))
                    {
                        device.id = Some(guest.0);
                    }
                }
            }
        }
        args.p_physical_device_count = Some(u32::try_from(n).unwrap_or(u32::MAX));
        Ok(())
    }

    /// `vkEnumeratePhysicalDeviceGroups`: the host's groups, restricted to the
    /// exposed devices (a group left empty is dropped), each member named by
    /// the guest id enumeration bound. A member not bound yet is
    /// `VK_ERROR_INITIALIZATION_FAILED`, as vkr answers ("venus driver is
    /// required to call vkEnumeratePhysicalDevices first").
    fn enumerate_groups(
        &mut self,
        args: &mut EnumeratePhysicalDeviceGroupsArgs,
    ) -> Result<(), ExecError> {
        const NAME: &str = "vkEnumeratePhysicalDeviceGroups";
        if let Err(ret) = self.exposed(NAME, args.instance.0)? {
            args.ret = ret;
            return Ok(());
        }
        let instance = self
            .objects
            .instance(args.instance.0)
            .map_err(id_error(NAME))?;
        let groups = match self.host.physical_device_groups(&instance.host) {
            Ok(groups) => groups,
            Err(ret) => {
                args.ret = ret;
                return Ok(());
            }
        };
        let exposed = instance.devices.as_deref().unwrap_or_default();
        let mut answer: Vec<VkPhysicalDeviceGroupProperties> = Vec::new();
        for group in groups {
            let mut out = VkPhysicalDeviceGroupProperties {
                subset_allocation: u32::from(group.subset_allocation),
                ..Default::default()
            };
            let mut n = 0usize;
            for member in &group.members {
                let Some(device) = exposed.iter().find(|d| d.host == *member) else {
                    continue;
                };
                let Some(id) = device.id else {
                    args.ret = VK_ERROR_INITIALIZATION_FAILED;
                    return Ok(());
                };
                if let Some(slot) = out.physical_devices.get_mut(n) {
                    *slot = VkPhysicalDevice(id);
                    n += 1;
                }
            }
            if n > 0 {
                out.physical_device_count = u32::try_from(n).unwrap_or(0);
                answer.push(out);
            }
        }
        let Some(slots) = args.p_physical_device_group_properties.as_mut() else {
            args.p_physical_device_group_count = Some(u32::try_from(answer.len()).unwrap_or(0));
            args.ret = VK_SUCCESS;
            return Ok(());
        };
        let asked = slots.len();
        let n = asked.min(answer.len());
        args.ret = if asked < answer.len() {
            VK_INCOMPLETE
        } else {
            VK_SUCCESS
        };
        answer.truncate(n);
        *slots = answer;
        args.p_physical_device_group_count = Some(u32::try_from(n).unwrap_or(0));
        Ok(())
    }

    // --------------------------------------------------- physical devices

    fn guest_device(&self, command: &'static str, id: u64) -> Result<&GuestDevice, ExecError> {
        self.objects
            .physical(id)
            .map(|(_, device)| &device.guest)
            .map_err(id_error(command))
    }

    /// `vkGetPhysicalDeviceProperties`: the cached, capped properties
    /// (`vkr_dispatch_vkGetPhysicalDeviceProperties`).
    fn properties(&mut self, args: &mut GetPhysicalDevicePropertiesArgs) -> Result<(), ExecError> {
        let guest = self.guest_device("vkGetPhysicalDeviceProperties", args.physical_device.0)?;
        args.p_properties = Some(guest.properties.properties.clone());
        Ok(())
    }

    /// `vkGetPhysicalDeviceProperties2`: every link the guest chained,
    /// answered from the host's whole chain (a link the host's version does
    /// not know stays zero), in the guest's order — the guest's reply decoder
    /// searches forward through its own chain, so the reply must echo it
    /// exactly (spec §2.2).
    fn properties2(
        &mut self,
        args: &mut GetPhysicalDeviceProperties2Args,
    ) -> Result<(), ExecError> {
        let guest = self.guest_device("vkGetPhysicalDeviceProperties2", args.physical_device.0)?;
        if let Some(out) = args.p_properties.as_mut() {
            out.properties = guest.properties.properties.clone();
            for link in &mut out.p_next {
                let stype = ChainLink::structure_type(link);
                if let Some(answer) = guest
                    .properties
                    .p_next
                    .iter()
                    .find(|l| ChainLink::structure_type(*l) == stype)
                {
                    *link = answer.clone();
                }
            }
        }
        Ok(())
    }

    /// `vkGetPhysicalDeviceFeatures2`, as [`Self::properties2`], from the
    /// masked features.
    fn features2(&mut self, args: &mut GetPhysicalDeviceFeatures2Args) -> Result<(), ExecError> {
        let guest = self.guest_device("vkGetPhysicalDeviceFeatures2", args.physical_device.0)?;
        if let Some(out) = args.p_features.as_mut() {
            out.features = guest.features.features.clone();
            for link in &mut out.p_next {
                let stype = ChainLink::structure_type(link);
                if let Some(answer) = guest
                    .features
                    .p_next
                    .iter()
                    .find(|l| ChainLink::structure_type(*l) == stype)
                {
                    *link = answer.clone();
                }
            }
        }
        Ok(())
    }

    /// `vkGetPhysicalDeviceQueueFamilyProperties2`: the count protocol (a void
    /// command, so a short array is simply filled short).
    fn queue_families(
        &mut self,
        args: &mut GetPhysicalDeviceQueueFamilyProperties2Args,
    ) -> Result<(), ExecError> {
        let guest = self.guest_device(
            "vkGetPhysicalDeviceQueueFamilyProperties2",
            args.physical_device.0,
        )?;
        let families = &guest.queue_families;
        match args.p_queue_family_properties.as_mut() {
            None => {
                args.p_queue_family_property_count =
                    Some(u32::try_from(families.len()).unwrap_or(0));
            }
            Some(slots) => {
                let n = slots.len().min(families.len());
                *slots = families
                    .iter()
                    .take(n)
                    .map(|family| VkQueueFamilyProperties2 {
                        p_next: Vec::new(),
                        queue_family_properties: family.clone(),
                    })
                    .collect();
                args.p_queue_family_property_count = Some(u32::try_from(n).unwrap_or(0));
            }
        }
        Ok(())
    }

    /// `vkGetPhysicalDeviceMemoryProperties2`: the guest view of memory — see
    /// [`policy::guest_memory`], which is where this differs from vkr.
    fn memory_properties(
        &mut self,
        args: &mut GetPhysicalDeviceMemoryProperties2Args,
    ) -> Result<(), ExecError> {
        let guest = self.guest_device(
            "vkGetPhysicalDeviceMemoryProperties2",
            args.physical_device.0,
        )?;
        if let Some(out) = args.p_memory_properties.as_mut() {
            out.memory_properties = guest.memory.clone();
        }
        Ok(())
    }

    /// `vkEnumerateDeviceExtensionProperties`: the advertised set
    /// ([`policy::advertised_extensions`]); a layer name is fatal, as vkr.
    fn device_extensions(
        &mut self,
        args: &mut EnumerateDeviceExtensionPropertiesArgs<'_>,
    ) -> Result<(), ExecError> {
        const NAME: &str = "vkEnumerateDeviceExtensionProperties";
        let guest = self.guest_device(NAME, args.physical_device.0)?;
        if args.p_layer_name.is_some() {
            return Err(invalid(NAME, "device layers are not served"));
        }
        let extensions = &guest.extensions;
        let asked = args.p_property_count.unwrap_or(0);
        match args.p_properties.as_mut() {
            None => {
                args.p_property_count = Some(u32::try_from(extensions.len()).unwrap_or(0));
                args.ret = VK_SUCCESS;
            }
            Some(out) => {
                let asked = usize::try_from(asked).unwrap_or(usize::MAX);
                let n = asked.min(extensions.len());
                *out = extensions.iter().take(n).cloned().collect();
                args.p_property_count = Some(u32::try_from(n).unwrap_or(0));
                args.ret = if asked < extensions.len() {
                    VK_INCOMPLETE
                } else {
                    VK_SUCCESS
                };
            }
        }
        Ok(())
    }

    /// `vkGetPhysicalDeviceExternalSemaphoreProperties` (stage 5b.3):
    /// `SYNC_FD` synthesized, every other handle type the host's answer
    /// ([`policy::external_semaphore_properties`]). The handle type must be
    /// one bit of Vulkan 1.3's, and the chain's semaphore type a real one.
    fn external_semaphore_properties(
        &mut self,
        args: &mut crate::venus::protocol::GetPhysicalDeviceExternalSemaphorePropertiesArgs,
    ) -> Result<(), ExecError> {
        use crate::venus::protocol::VkPhysicalDeviceExternalSemaphoreInfoNext as N;
        const NAME: &str = "vkGetPhysicalDeviceExternalSemaphoreProperties";
        let Some(info) = &args.p_external_semaphore_info else {
            return Err(invalid(NAME, "pExternalSemaphoreInfo is null"));
        };
        let handle = u32::try_from(info.handle_type).unwrap_or(0);
        if !handle.is_power_of_two() || handle & !policy::SEMAPHORE_HANDLE_CORE != 0 {
            return Err(invalid(NAME, format!("handle type {handle:#x}")));
        }
        let mut timeline = false;
        for link in &info.p_next {
            match link {
                N::VkSemaphoreTypeCreateInfo(t) => match t.semaphore_type {
                    policy::SEMAPHORE_TYPE_BINARY => {}
                    policy::SEMAPHORE_TYPE_TIMELINE => timeline = true,
                    other => return Err(invalid(NAME, format!("semaphore type {other}"))),
                },
            }
        }
        let (instance, device) = self
            .objects
            .physical(args.physical_device.0)
            .map_err(id_error(NAME))?;
        let host = &self.host;
        let answer = policy::external_semaphore_properties(handle, timeline, || {
            host.external_semaphore_properties(instance, device.host, handle, timeline)
        });
        args.p_external_semaphore_properties = Some(answer);
        Ok(())
    }

    /// `vkGetPhysicalDeviceExternalBufferProperties` (stage 5c): Mesa
    /// 26.0.8 sends it for every handle type it supports, rewritten to the
    /// renderer's `DMA_BUF` (`vn_physical_device.c:2900-2955`), and gets
    /// [`policy::external_memory_properties`] — exportable and importable
    /// exactly when such a buffer can live in our pages or (stage S1) in
    /// device-local memory the host exports as `OPAQUE_WIN32`. Every other handle
    /// type (a core 1.3 one) is answered "nothing": no other is served. The
    /// flags and usage must be ones the device could create a buffer with.
    fn external_buffer_properties(
        &mut self,
        args: &mut crate::venus::protocol::GetPhysicalDeviceExternalBufferPropertiesArgs,
    ) -> Result<(), ExecError> {
        const NAME: &str = "vkGetPhysicalDeviceExternalBufferProperties";
        let Some(info) = &args.p_external_buffer_info else {
            return Err(invalid(NAME, "pExternalBufferInfo is null"));
        };
        let (instance, device) = self
            .objects
            .physical(args.physical_device.0)
            .map_err(id_error(NAME))?;
        let handle = u32::try_from(info.handle_type).unwrap_or(0);
        let known_handle = handle.is_power_of_two()
            && (handle & !policy::EXTERNAL_MEMORY_HANDLE_CORE == 0
                || handle == policy::MEMORY_HANDLE_DMA_BUF);
        if !known_handle {
            return Err(invalid(NAME, format!("handle type {handle:#x}")));
        }
        let advertised = |name: &str| policy::has_extension(&device.guest.extensions, name);
        let usage_known = policy::BUFFER_USAGE_CORE
            | policy::BUFFER_USAGE_DEVICE_ADDRESS
            | policy::buffer_usage_of_extensions(advertised);
        if info.usage == 0
            || info.usage & !usage_known != 0
            || info.flags & !policy::BUFFER_CREATE_CORE != 0
        {
            return Err(invalid(
                NAME,
                format!("usage {:#x} or flags {:#x}", info.usage, info.flags),
            ));
        }
        let answer = if handle == policy::MEMORY_HANDLE_DMA_BUF {
            // Our pages, or (stage S1) exportable device-local memory.
            let shareable =
                self.host
                    .buffer_importable(instance, device.host, info.flags, info.usage)
                    || (device.guest.memory_export
                        && self.host.buffer_exportable(
                            instance,
                            device.host,
                            info.flags,
                            info.usage,
                        ));
            policy::external_memory_properties(shareable)
        } else {
            crate::venus::protocol::VkExternalMemoryProperties {
                external_memory_features: 0,
                export_from_imported_handle_types: 0,
                compatible_handle_types: handle,
            }
        };
        args.p_external_buffer_properties =
            Some(crate::venus::protocol::VkExternalBufferProperties {
                external_memory_properties: answer,
            });
        Ok(())
    }

    /// `vkGetPhysicalDeviceFormatProperties2`, forwarded with a checked
    /// format — and, stage S1, `VkDrmFormatModifierPropertiesListEXT` and
    /// `…List2EXT` answered here: one entry, `DRM_FORMAT_MOD_LINEAR` with one
    /// plane and the canonical image's optimal features, for a scanout format
    /// whose canonical image this host can make, on a device shown the
    /// emulated extension; none for anything else (`executor::modifier`).
    fn format_properties(
        &mut self,
        args: &mut GetPhysicalDeviceFormatProperties2Args,
    ) -> Result<(), ExecError> {
        use crate::venus::protocol::{
            VkDrmFormatModifierProperties2EXT, VkDrmFormatModifierPropertiesEXT,
            VkFormatProperties2Next as N,
        };
        const NAME: &str = "vkGetPhysicalDeviceFormatProperties2";
        if !policy::is_core_format(args.format) {
            return Err(invalid(
                NAME,
                format!("format {} is not a Vulkan 1.3 format", args.format),
            ));
        }
        let (instance, device) = self
            .objects
            .physical(args.physical_device.0)
            .map_err(id_error(NAME))?;
        let Some(out) = args.p_format_properties.as_mut() else {
            return Ok(());
        };
        self.host
            .format_properties(instance, device.host, args.format, out);
        let asks = out.p_next.iter().any(|l| {
            matches!(
                l,
                N::VkDrmFormatModifierPropertiesListEXT(_)
                    | N::VkDrmFormatModifierPropertiesList2EXT(_)
            )
        });
        if !asks {
            return Ok(());
        }
        let canonical =
            policy::has_extension(&device.guest.extensions, policy::IMAGE_DRM_FORMAT_MODIFIER)
                .then(|| {
                    modifier::canonical_format(&*self.host, instance, device.host, args.format)
                })
                .flatten();
        for link in &mut out.p_next {
            match link {
                N::VkDrmFormatModifierPropertiesListEXT(list) => {
                    let entries: Vec<VkDrmFormatModifierPropertiesEXT> = canonical
                        .iter()
                        .map(|c| VkDrmFormatModifierPropertiesEXT {
                            drm_format_modifier: modifier::DRM_FORMAT_MOD_LINEAR,
                            drm_format_modifier_plane_count: 1,
                            drm_format_modifier_tiling_features: c.features,
                        })
                        .collect();
                    fill_modifier_list(
                        &mut list.drm_format_modifier_count,
                        &mut list.p_drm_format_modifier_properties,
                        entries,
                    );
                }
                N::VkDrmFormatModifierPropertiesList2EXT(list) => {
                    let entries: Vec<VkDrmFormatModifierProperties2EXT> = canonical
                        .iter()
                        .map(|c| VkDrmFormatModifierProperties2EXT {
                            drm_format_modifier: modifier::DRM_FORMAT_MOD_LINEAR,
                            drm_format_modifier_plane_count: 1,
                            drm_format_modifier_tiling_features: c.features2,
                        })
                        .collect();
                    fill_modifier_list(
                        &mut list.drm_format_modifier_count,
                        &mut list.p_drm_format_modifier_properties,
                        entries,
                    );
                }
                _ => {}
            }
        }
        Ok(())
    }

    /// `vkGetPhysicalDeviceImageFormatProperties2`, forwarded with a checked
    /// info and chain.
    fn image_format_properties(
        &mut self,
        args: &mut GetPhysicalDeviceImageFormatProperties2Args,
    ) -> Result<(), ExecError> {
        const NAME: &str = "vkGetPhysicalDeviceImageFormatProperties2";
        let Some(info) = &args.p_image_format_info else {
            return Err(invalid(NAME, "pImageFormatInfo is null"));
        };
        let (instance, device) = self
            .objects
            .physical(args.physical_device.0)
            .map_err(id_error(NAME))?;
        let drm =
            policy::has_extension(&device.guest.extensions, policy::IMAGE_DRM_FORMAT_MODIFIER);
        check_image_format_info(NAME, info, drm)?;
        if info.tiling == modifier::IMAGE_TILING_DRM_FORMAT_MODIFIER {
            let answer = modifier::image_format_properties(
                &*self.host,
                instance,
                device.host,
                info,
                args.p_image_format_properties.as_mut(),
            );
            args.ret = answer;
            return Ok(());
        }
        let dma_buf = info.p_next.iter().any(|link| {
            matches!(link,
                VkPhysicalDeviceImageFormatInfo2Next::VkPhysicalDeviceExternalImageFormatInfo(e)
                    if u32::try_from(e.handle_type).ok() == Some(policy::MEMORY_HANDLE_DMA_BUF))
        });
        let Some(out) = args.p_image_format_properties.as_mut() else {
            return Ok(());
        };
        if !dma_buf {
            args.ret = self
                .host
                .image_format_properties(instance, device.host, info, out);
            return Ok(());
        }
        // `DMA_BUF` (stage 5c; Mesa 26.0.8 answers it itself for every
        // tiling but DRM modifiers, `vn_physical_device.c:2812-2817`, so only
        // another guest driver sends it): asked of the host as our pages
        // (`HOST_ALLOCATION`), and answered as a dma-buf of them —
        // supported exactly when the host would import our pages for such
        // an image.
        let mut query = info.clone();
        for link in &mut query.p_next {
            if let VkPhysicalDeviceImageFormatInfo2Next::VkPhysicalDeviceExternalImageFormatInfo(
                e,
            ) = link
            {
                e.handle_type = policy::MEMORY_HANDLE_HOST_ALLOCATION as i32;
            }
        }
        // The host's external answer is needed whether or not the guest
        // chained one.
        let mut probe = out.clone();
        if !probe.p_next.iter().any(|l| {
            matches!(
                l,
                VkImageFormatProperties2Next::VkExternalImageFormatProperties(_)
            )
        }) {
            probe.p_next.push(
                VkImageFormatProperties2Next::VkExternalImageFormatProperties(Default::default()),
            );
        }
        let ret = self
            .host
            .image_format_properties(instance, device.host, &query, &mut probe);
        let importable = probe.p_next.iter().any(|l| {
            matches!(l,
                VkImageFormatProperties2Next::VkExternalImageFormatProperties(p)
                    if p.external_memory_properties.external_memory_features
                        & policy::MEMORY_FEATURE_IMPORTABLE != 0)
        });
        out.image_format_properties = probe.image_format_properties.clone();
        for link in &mut out.p_next {
            let stype = ChainLink::structure_type(link);
            if let Some(answer) = probe
                .p_next
                .iter()
                .find(|l| ChainLink::structure_type(*l) == stype)
            {
                *link = answer.clone();
            }
            if let VkImageFormatProperties2Next::VkExternalImageFormatProperties(p) = link {
                p.external_memory_properties = policy::external_memory_properties(importable);
            }
        }
        args.ret = if ret == VK_SUCCESS && !importable {
            VK_ERROR_FORMAT_NOT_SUPPORTED
        } else {
            ret
        };
        Ok(())
    }

    // -------------------------------------------------------------- device

    /// `vkCreateDevice`, rebuilt from the decoded structures.
    ///
    /// As vkr: a queue family index outside the host's families, or more
    /// queues from a family than it has, is `VK_ERROR_UNKNOWN`. Beyond vkr:
    /// a family named twice with the same flags is `VK_ERROR_UNKNOWN` too
    /// (Vulkan forbids it; vkr sums the counts and lets the driver judge), an
    /// extension outside the advertised set is `VK_ERROR_EXTENSION_NOT_PRESENT`
    /// and a feature the guest was told is false is `VK_ERROR_FEATURE_NOT_PRESENT`
    /// — the answers a driver gives, given before the driver is asked. Every
    /// flag word, priority and chained id is checked, and the host device
    /// always enables `VK_EXT_external_memory_host` for the renderer's own use.
    fn create_device(&mut self, args: &mut CreateDeviceArgs<'_>) -> Result<(), ExecError> {
        const NAME: &str = "vkCreateDevice";
        let physical = args.physical_device.0;
        let id = args.p_device.map(|h| h.0).unwrap_or(0);
        let (_, exposed) = self.objects.physical(physical).map_err(id_error(NAME))?;
        let guest = exposed.guest.clone();
        let host_physical = exposed.host;
        self.objects
            .check_new(id, Kind::Device)
            .map_err(id_error(NAME))?;
        let Some(info) = &args.p_create_info else {
            return Err(invalid(NAME, "pCreateInfo is null"));
        };
        if info.flags != 0 {
            return Err(invalid(
                NAME,
                format!("flags {:#x} are reserved", info.flags),
            ));
        }

        // Queues.
        let infos = info.p_queue_create_infos.as_deref().unwrap_or_default();
        if infos.is_empty() {
            args.ret = VK_ERROR_INITIALIZATION_FAILED;
            return Ok(());
        }
        let mut queues = Vec::with_capacity(infos.len());
        let mut used = vec![0u32; guest.queue_families.len()];
        let mut seen: HashSet<(u32, u32)> = HashSet::new();
        for queue in infos {
            if queue.flags & !policy::QUEUE_CREATE_PROTECTED != 0 {
                return Err(invalid(NAME, format!("queue flags {:#x}", queue.flags)));
            }
            if queue.queue_count == 0 {
                return Err(invalid(NAME, "a queue create info asks for zero queues"));
            }
            let priorities = queue.p_queue_priorities.as_deref().unwrap_or_default();
            if priorities.len() != queue.queue_count as usize
                || priorities.iter().any(|p| !(0.0..=1.0).contains(p))
            {
                return Err(invalid(
                    NAME,
                    "queue priorities must be one per queue, in [0, 1]",
                ));
            }
            if queue.flags & policy::QUEUE_CREATE_PROTECTED != 0 && !guest.protected_memory() {
                args.ret = VK_ERROR_FEATURE_NOT_PRESENT;
                return Ok(());
            }
            let family = usize::try_from(queue.queue_family_index).unwrap_or(usize::MAX);
            let (Some(props), Some(taken)) =
                (guest.queue_families.get(family), used.get_mut(family))
            else {
                args.ret = VK_ERROR_UNKNOWN;
                return Ok(());
            };
            if !seen.insert((queue.queue_family_index, queue.flags))
                || props.queue_count.saturating_sub(*taken) < queue.queue_count
            {
                args.ret = VK_ERROR_UNKNOWN;
                return Ok(());
            }
            *taken += queue.queue_count;
            queues.push(QueueRequest {
                flags: queue.flags,
                family: queue.queue_family_index,
                priorities: priorities.to_vec(),
            });
        }

        // Extensions: only what we advertised, plus our own — and never an
        // emulated one (`VK_KHR_external_semaphore_fd`, which Mesa adds for
        // every device an application wants a swapchain on,
        // `vn_device.c:333-337`, and the two dma-buf ones it adds beside it,
        // `:318-330`): the host driver may not have it, and what it stands
        // for is this renderer's to do, not the driver's. Every name the
        // guest enabled is recorded, emulated ones included: the admitted
        // extensions' commands, values and structures are judged by it.
        let mut extensions: Vec<String> = Vec::new();
        let mut enabled: Vec<String> = Vec::new();
        for name in info
            .pp_enabled_extension_names
            .as_deref()
            .unwrap_or_default()
        {
            let advertised = std::str::from_utf8(name).ok().filter(|name| {
                guest
                    .extensions
                    .iter()
                    .any(|e| policy::c_name(&e.extension_name) == name.as_bytes())
            });
            match advertised {
                Some(name) if policy::is_emulated_extension(name) => {
                    enabled.push(name.to_owned());
                }
                Some(name) => {
                    enabled.push(name.to_owned());
                    extensions.push(name.to_owned());
                }
                None => {
                    args.ret = VK_ERROR_EXTENSION_NOT_PRESENT;
                    return Ok(());
                }
            }
        }
        enabled.sort_unstable();
        enabled.dedup();
        if !extensions.iter().any(|e| e == policy::EXTERNAL_MEMORY_HOST) {
            extensions.push(policy::EXTERNAL_MEMORY_HOST.to_owned());
        }
        // Stage S1: exportable device-local memory, and its imports, rest on
        // the host's own external-memory extension, which the guest is never
        // shown.
        if guest.memory_export {
            extensions.push(policy::EXTERNAL_MEMORY_WIN32.to_owned());
        }

        // Features: never more than the guest was told.
        let has_features2 = info
            .p_next
            .iter()
            .any(|l| matches!(l, VkDeviceCreateInfoNext::VkPhysicalDeviceFeatures2(_)));
        if has_features2 && info.p_enabled_features.is_some() {
            return Err(invalid(
                NAME,
                "pEnabledFeatures and a chained VkPhysicalDeviceFeatures2",
            ));
        }
        let mut wanted: Vec<(i32, Vec<u8>)> = Vec::new();
        if let Some(core) = &info.p_enabled_features {
            wanted.push((0, words(|enc| core.encode(enc))));
        }
        let mut chain = Vec::new();
        let mut group = None;
        let buffer_device_address = info.p_next.iter().any(|link| match link {
            VkDeviceCreateInfoNext::VkPhysicalDeviceVulkan12Features(f) => {
                f.buffer_device_address != 0
            }
            VkDeviceCreateInfoNext::VkPhysicalDeviceBufferDeviceAddressFeatures(f) => {
                f.buffer_device_address != 0
            }
            _ => false,
        });
        // robustness2's `nullDescriptor` (stage 5c) counts only with one of
        // the extensions that define it enabled.
        let robustness2 = enabled
            .iter()
            .any(|e| e == "VK_EXT_robustness2" || e == "VK_KHR_robustness2");
        let null_descriptor = robustness2
            && info.p_next.iter().any(|link| {
                matches!(link,
                    VkDeviceCreateInfoNext::VkPhysicalDeviceRobustness2FeaturesKHR(f)
                        if f.null_descriptor != 0)
            });
        for link in &info.p_next {
            match link {
                VkDeviceCreateInfoNext::VkDeviceGroupDeviceCreateInfo(g) => {
                    let ids = g.p_physical_devices.as_deref().unwrap_or_default();
                    let mut members = Vec::with_capacity(ids.len());
                    for member in ids {
                        let (_, device) =
                            self.objects.physical(member.0).map_err(id_error(NAME))?;
                        members.push(device.host);
                    }
                    if !ids.iter().any(|m| m.0 == physical) {
                        return Err(invalid(
                            NAME,
                            "a device group that does not contain the device's physical device",
                        ));
                    }
                    group = Some(members);
                }
                VkDeviceCreateInfoNext::VkDevicePrivateDataCreateInfo(_) => {
                    chain.push(link.clone())
                }
                VkDeviceCreateInfoNext::VkPhysicalDeviceFeatures2(f) => {
                    wanted.push((0, words(|enc| f.features.encode(enc))));
                    chain.push(link.clone());
                }
                other => {
                    let stype = ChainLink::structure_type(other);
                    wanted.push((stype, words(|enc| other.encode_body(enc, false))));
                    // An admitted extension's feature structure (stage 5c)
                    // reaches the driver only with one of the extensions that
                    // define it enabled: the driver is owed no structure of
                    // an extension it was not asked for. Its features are
                    // still judged against what the guest was told.
                    if extension_enabled_for(stype, &enabled) {
                        chain.push(link.clone());
                    }
                }
            }
        }
        for (stype, requested) in &wanted {
            let reported = if *stype == 0 {
                words(|enc| guest.features.features.encode(enc))
            } else {
                guest
                    .features
                    .p_next
                    .iter()
                    .find(|l| ChainLink::structure_type(*l) == *stype)
                    .map(|l| words(|enc| l.encode_body(enc, false)))
                    .unwrap_or_default()
            };
            if !subset(requested, &reported) {
                args.ret = VK_ERROR_FEATURE_NOT_PRESENT;
                return Ok(());
            }
        }

        if !self.objects.has_room() {
            args.ret = VK_ERROR_OUT_OF_HOST_MEMORY;
            return Ok(());
        }
        // Containment for what the checks let past (stage 5b.2, ADR-0004):
        // the host device always has `robustBufferAccess` when it supports
        // it, whatever the guest enabled, so an out-of-range buffer access
        // from a shader stays inside the buffer. The guest is told nothing:
        // it enabled what it enabled, and a robust device only behaves better
        // than one that is not. vkr enables no robustness of its own.
        let mut features = info.p_enabled_features.clone();
        let robust = guest.features.features.robust_buffer_access != 0;
        if robust {
            if has_features2 {
                for link in &mut chain {
                    if let VkDeviceCreateInfoNext::VkPhysicalDeviceFeatures2(f) = link {
                        f.features.robust_buffer_access = 1;
                    }
                }
            } else {
                features
                    .get_or_insert_with(Default::default)
                    .robust_buffer_access = 1;
            }
        }
        let request = DeviceRequest {
            queues,
            extensions,
            features,
            chain,
            group,
        };
        let group_size = request.group.as_ref().map_or(1, |members| {
            u32::try_from(members.len()).unwrap_or(1).max(1)
        });
        let (instance, _) = self.objects.physical(physical).map_err(id_error(NAME))?;
        match self.host.create_device(instance, host_physical, &request) {
            Ok(device) => {
                let mut created = Vec::new();
                for queue in &request.queues {
                    for index in 0..queue.priorities.len() {
                        created.push(CreatedQueue {
                            flags: queue.flags,
                            family: queue.family,
                            index: u32::try_from(index).unwrap_or(u32::MAX),
                            id: None,
                        });
                    }
                }
                self.objects.insert_device(
                    id,
                    DeviceObject {
                        host: Arc::new(device),
                        physical,
                        queues: created,
                        group_size,
                        buffer_device_address,
                        extensions: enabled,
                        null_descriptor,
                        custom_border_samplers: 0,
                    },
                );
                args.ret = VK_SUCCESS;
            }
            Err(ret) => args.ret = ret,
        }
        Ok(())
    }

    /// `vkDestroyDevice`: the device and every child it still has, as
    /// `vkr_device_destroy` does.
    fn destroy_device(&mut self, args: &DestroyDeviceArgs) -> Result<(), ExecError> {
        self.objects
            .device(args.device.0)
            .map_err(id_error("vkDestroyDevice"))?;
        let host = Arc::clone(&self.host);
        self.objects.destroy_device(&host, args.device.0);
        Ok(())
    }

    /// `vkGetDeviceQueue2`, as `vkr_dispatch_vkGetDeviceQueue2`: the queue
    /// must be one the device was created with and not yet fetched, and it
    /// must carry a `VkDeviceQueueTimelineInfoMESA` naming a fence timeline
    /// in `1..64` no other queue of the context holds. The timeline is
    /// recorded; fences on it are the next stage's.
    fn device_queue(&mut self, args: &mut GetDeviceQueue2Args) -> Result<(), ExecError> {
        const NAME: &str = "vkGetDeviceQueue2";
        let device_id = args.device.0;
        let Some(info) = &args.p_queue_info else {
            return Err(invalid(NAME, "pQueueInfo is null"));
        };
        let ring_idx = info
            .p_next
            .iter()
            .map(|link| match link {
                VkDeviceQueueInfo2Next::VkDeviceQueueTimelineInfoMESA(t) => t.ring_idx,
            })
            .next()
            .ok_or_else(|| invalid(NAME, "no VkDeviceQueueTimelineInfoMESA"))?;
        if ring_idx == 0 || ring_idx >= MAX_RING_IDX {
            return Err(invalid(
                NAME,
                format!("ring_idx {ring_idx} is outside 1..{MAX_RING_IDX}"),
            ));
        }
        if self.objects.ring_idx_taken(ring_idx) {
            return Err(invalid(
                NAME,
                format!("ring_idx {ring_idx} is already bound"),
            ));
        }
        let id = args.p_queue.map(|h| h.0).unwrap_or(0);
        self.objects
            .check_new(id, Kind::Queue)
            .map_err(id_error(NAME))?;
        let device = self.objects.device(device_id).map_err(id_error(NAME))?;
        let slot = device
            .queues
            .iter()
            .position(|q| {
                q.flags == info.flags
                    && q.family == info.queue_family_index
                    && q.index == info.queue_index
            })
            .ok_or_else(|| invalid(NAME, "no such queue was created with the device"))?;
        if device.queues.get(slot).and_then(|q| q.id).is_some() {
            return Err(invalid(NAME, "the queue was already fetched"));
        }
        let host_queue = self.host.device_queue(
            &device.host,
            info.flags,
            info.queue_family_index,
            info.queue_index,
        );
        if let Some(queue) = self
            .objects
            .device_mut(device_id)
            .map_err(id_error(NAME))?
            .queues
            .get_mut(slot)
        {
            queue.id = Some(id);
        }
        self.objects.insert_queue(
            id,
            QueueObject {
                device: device_id,
                host: host_queue,
                ring_idx,
                family: info.queue_family_index,
                pending: Pending::default(),
                sync: None,
            },
        );
        Ok(())
    }

    // --------------------------------------------------------- pool, image

    /// `vkCreateCommandPool`: flags inside 1.3 core, a family the *physical*
    /// device has, and `PROTECTED` only for a family the device was created
    /// with a protected queue in.
    ///
    /// Not "a family the device has a queue in": the spec asks only that
    /// `queueFamilyIndex` name one of the physical device's families
    /// (`VUID-vkCreateCommandPool-queueFamilyIndex-01937`), and Mesa's WSI
    /// relies on exactly that — `wsi_swapchain_init` makes a blit pool for
    /// every family, queues or not. Requiring a queue killed the first
    /// `vkcube` a real guest ran, on family 1 of the RTX 2070.
    fn create_command_pool(&mut self, args: &mut CreateCommandPoolArgs) -> Result<(), ExecError> {
        const NAME: &str = "vkCreateCommandPool";
        let device_id = args.device.0;
        let id = args.p_command_pool.map(|h| h.0).unwrap_or(0);
        let device = self.objects.device(device_id).map_err(id_error(NAME))?;
        self.objects
            .check_new(id, Kind::CommandPool)
            .map_err(id_error(NAME))?;
        let Some(info) = &args.p_create_info else {
            return Err(invalid(NAME, "pCreateInfo is null"));
        };
        if info.flags & !policy::COMMAND_POOL_CREATE_CORE != 0 {
            return Err(invalid(NAME, format!("flags {:#x}", info.flags)));
        }
        let protected = info.flags & policy::COMMAND_POOL_CREATE_PROTECTED != 0;
        let families = self
            .guest_device(NAME, device.physical)?
            .queue_families
            .len();
        let family = usize::try_from(info.queue_family_index).unwrap_or(usize::MAX);
        if family >= families {
            return Err(invalid(
                NAME,
                format!(
                    "queue family {} is not one of the device's {families}",
                    info.queue_family_index
                ),
            ));
        }
        if protected
            && !device.queues.iter().any(|q| {
                q.family == info.queue_family_index && q.flags & policy::QUEUE_CREATE_PROTECTED != 0
            })
        {
            return Err(invalid(
                NAME,
                format!(
                    "a protected pool on family {} needs a protected queue there",
                    info.queue_family_index
                ),
            ));
        }
        if !self.objects.has_room() {
            args.ret = VK_ERROR_OUT_OF_HOST_MEMORY;
            return Ok(());
        }
        match self.host.create_command_pool(&device.host, info) {
            Ok(pool) => {
                self.objects.insert_pool(
                    id,
                    DeviceChild {
                        device: device_id,
                        host: pool,
                    },
                );
                args.ret = VK_SUCCESS;
            }
            Err(ret) => args.ret = ret,
        }
        Ok(())
    }

    /// `vkDestroyCommandPool`; `VK_NULL_HANDLE` is a no-op. Its command
    /// buffers go with it (the driver frees them), after any work that may
    /// still be running them.
    fn destroy_command_pool(&mut self, args: &DestroyCommandPoolArgs) -> Result<(), ExecError> {
        const NAME: &str = "vkDestroyCommandPool";
        self.objects.device(args.device.0).map_err(id_error(NAME))?;
        if args.command_pool.0 != 0 {
            self.objects
                .pool(args.device.0, args.command_pool.0)
                .map_err(id_error(NAME))?;
            self.settle(args.device.0);
            self.objects.forget_pool_children(args.command_pool.0);
        }
        if let Some(pool) = self
            .objects
            .take_pool(args.device.0, args.command_pool.0)
            .map_err(id_error(NAME))?
        {
            let device = self.objects.device(args.device.0).map_err(id_error(NAME))?;
            self.host.destroy_command_pool(&device.host, pool);
        }
        Ok(())
    }

    /// `vkCreateImage`, with every field range-checked and the request checked
    /// against the host's own `vkGetPhysicalDeviceImageFormatProperties2`
    /// limits first: an image outside them is invalid usage the driver is not
    /// required to survive.
    fn create_image(&mut self, args: &mut CreateImageArgs) -> Result<(), ExecError> {
        const NAME: &str = "vkCreateImage";
        let device_id = args.device.0;
        let id = args.p_image.map(|h| h.0).unwrap_or(0);
        let device = self.objects.device(device_id).map_err(id_error(NAME))?;
        self.objects
            .check_new(id, Kind::Image)
            .map_err(id_error(NAME))?;
        let Some(info) = &args.p_create_info else {
            return Err(invalid(NAME, "pCreateInfo is null"));
        };
        let (instance, exposed) = self
            .objects
            .physical(device.physical)
            .map_err(id_error(NAME))?;
        let check = check_image_create_info(NAME, info, &exposed.guest, ImageRules::of(device))?;
        if let Some(choice) = &check.modifier {
            return self.create_modifier_image(args, choice);
        }
        let external = check.external;
        if !image_limits_hold(&*self.host, instance, exposed.host, info) {
            return Err(invalid(
                NAME,
                "the image is outside what the host reports for its format, type, tiling, usage and flags",
            ));
        }
        let planes = image_planes(NAME, info)?;

        if !self.objects.has_room() {
            args.ret = VK_ERROR_OUT_OF_HOST_MEMORY;
            return Ok(());
        }
        let host_memory = self.host.image_accepts_host_memory(&device.host, info);
        // Stage S1: a `DMA_BUF` image our pages cannot hold lives in
        // exportable device-local memory, where the host can export it.
        let handle = external
            && !host_memory
            && image_exportable(&*self.host, instance, exposed.host, &exposed.guest, info);
        match self
            .host
            .create_image(&device.host, info, resource_memory(host_memory, handle))
        {
            Ok(image) => {
                self.objects.insert_image(
                    id,
                    ImageObject {
                        device: device_id,
                        host: image,
                        facts: image_facts(info, planes),
                        host_memory,
                        external,
                        handle,
                        modifier: None,
                        bound_planes: 0,
                    },
                );
                args.ret = VK_SUCCESS;
            }
            Err(ret) => args.ret = ret,
        }
        Ok(())
    }

    /// The host create info of a DRM-modifier image (stage S1): the
    /// canonical optimal image of its format and extent
    /// (`executor::modifier`), once the guest's own create info is inside
    /// every rule the emulation holds it to — 2D, one level, one layer, one
    /// sample, exclusive; a scanout format this host makes canonical; usage,
    /// flags and view formats inside the canonical ones. `Ok(Err(ret))` is
    /// an answer rather than a refusal: an explicit plane layout that is not
    /// the synthesized one (`VK_ERROR_INVALID_DRM_FORMAT_MODIFIER_PLANE_LAYOUT_EXT`).
    ///
    /// # Errors
    /// Fatal: a create info no correct guest sends after the format query.
    pub(super) fn modifier_create_info(
        &self,
        command: &'static str,
        physical: u64,
        info: &VkImageCreateInfo,
        choice: &ModifierChoice,
    ) -> Result<Result<(VkImageCreateInfo<'static>, ModifierLayout), VkResult>, ExecError> {
        let (instance, exposed) = self.objects.physical(physical).map_err(id_error(command))?;
        if info.image_type != 1
            || info.extent.depth != 1
            || info.mip_levels != 1
            || info.array_layers != 1
            || info.samples != 1
            || info.sharing_mode != modifier::SHARING_MODE_EXCLUSIVE
        {
            return Err(invalid(
                command,
                "a DRM format modifier image that is not a 2D, single-level, single-layer, \
                 single-sample, exclusive image",
            ));
        }
        let Some(canonical) =
            modifier::canonical_format(&*self.host, instance, exposed.host, info.format)
        else {
            return Err(invalid(
                command,
                format!(
                    "format {} has no DRM format modifier on this host",
                    info.format
                ),
            ));
        };
        let mut view_formats = None;
        for link in &info.p_next {
            match link {
                VkImageCreateInfoNext::VkImageFormatListCreateInfo(l) => {
                    view_formats = Some(l.p_view_formats.as_deref().unwrap_or_default());
                }
                VkImageCreateInfoNext::VkImageStencilUsageCreateInfo(_) => {
                    return Err(invalid(
                        command,
                        "a stencil usage on a colour DRM format modifier image",
                    ));
                }
                _ => {}
            }
        }
        if !canonical.admits(info.flags, view_formats) {
            return Err(invalid(
                command,
                format!(
                    "flags {:#x} or view formats outside the canonical image's",
                    info.flags
                ),
            ));
        }
        if info.usage & !canonical.usage != 0 {
            return Err(invalid(
                command,
                format!(
                    "usage {:#x} outside the canonical image's {:#x}",
                    info.usage, canonical.usage
                ),
            ));
        }
        let layout = ModifierLayout::new(info.extent.width, info.extent.height);
        if let ModifierChoice::Explicit(plane) = choice {
            if plane.offset != 0 || plane.row_pitch != layout.row_pitch {
                return Ok(Err(VK_ERROR_INVALID_DRM_FORMAT_MODIFIER_PLANE_LAYOUT_EXT));
            }
        }
        let host =
            canonical.create_info(info.extent.width, info.extent.height, info.initial_layout);
        if !image_limits_hold(&*self.host, instance, exposed.host, &host) {
            return Err(invalid(
                command,
                "the canonical image is outside what the host reports for it",
            ));
        }
        Ok(Ok((host, layout)))
    }

    /// `vkCreateImage` of a DRM-modifier image (stage S1): the canonical
    /// host image ([`Self::modifier_create_info`]), created for
    /// `OPAQUE_WIN32`, recorded with the guest's own create info (its views
    /// and commands are judged by it) and the synthesized plane. A canonical
    /// image the host would still want dedicated memory for is refused
    /// (`VK_ERROR_OUT_OF_DEVICE_MEMORY`): its memory is exported undedicated.
    fn create_modifier_image(
        &mut self,
        args: &mut CreateImageArgs,
        choice: &ModifierChoice,
    ) -> Result<(), ExecError> {
        const NAME: &str = "vkCreateImage";
        let device_id = args.device.0;
        let id = args.p_image.map(|h| h.0).unwrap_or(0);
        let Some(info) = &args.p_create_info else {
            return Err(invalid(NAME, "pCreateInfo is null"));
        };
        let physical = self
            .objects
            .device(device_id)
            .map_err(id_error(NAME))?
            .physical;
        let (host_info, layout) = match self.modifier_create_info(NAME, physical, info, choice)? {
            Ok(created) => created,
            Err(ret) => {
                args.ret = ret;
                return Ok(());
            }
        };
        if !self.objects.has_room() {
            args.ret = VK_ERROR_OUT_OF_HOST_MEMORY;
            return Ok(());
        }
        let device = self.objects.device(device_id).map_err(id_error(NAME))?;
        let image = match self
            .host
            .create_image(&device.host, &host_info, ResourceMemory::Handle)
        {
            Ok(image) => image,
            Err(ret) => {
                args.ret = ret;
                return Ok(());
            }
        };
        let mut req = VkMemoryRequirements2 {
            p_next: vec![VkMemoryRequirements2Next::VkMemoryDedicatedRequirements(
                Default::default(),
            )],
            ..Default::default()
        };
        self.host
            .image_memory_requirements(&device.host, image, None, &mut req);
        let dedicated_only = req.p_next.iter().any(|l| {
            let VkMemoryRequirements2Next::VkMemoryDedicatedRequirements(d) = l;
            d.requires_dedicated_allocation != 0
        });
        if dedicated_only {
            tracing::warn!(
                ctx_id = self.ctx_id,
                format = info.format,
                "the host requires dedicated memory for a canonical modifier image"
            );
            self.host.destroy_image(&device.host, image);
            args.ret = VK_ERROR_OUT_OF_DEVICE_MEMORY;
            return Ok(());
        }
        self.objects.insert_image(
            id,
            ImageObject {
                device: device_id,
                host: image,
                facts: image_facts(info, 1),
                host_memory: false,
                external: true,
                handle: true,
                modifier: Some(layout),
                bound_planes: 0,
            },
        );
        args.ret = VK_SUCCESS;
        Ok(())
    }

    /// `vkDestroyImage`; `VK_NULL_HANDLE` is a no-op.
    fn destroy_image(&mut self, args: &DestroyImageArgs) -> Result<(), ExecError> {
        const NAME: &str = "vkDestroyImage";
        self.objects.device(args.device.0).map_err(id_error(NAME))?;
        // Nothing the GPU may still be using is freed under it.
        self.settle(args.device.0);
        if let Some(image) = self
            .objects
            .take_image(args.device.0, args.image.0)
            .map_err(id_error(NAME))?
        {
            let device = self.objects.device(args.device.0).map_err(id_error(NAME))?;
            self.host.destroy_image(&device.host, image);
        }
        Ok(())
    }

    /// `vkGetImageMemoryRequirements2`. Type indices need no translation
    /// ([`policy::guest_memory`] keeps every one), but the bits are filtered
    /// to what this image may be bound to ([`guest_type_bits`]): no
    /// host-visible type unless it was created able to take our imported
    /// pages. A plane is named exactly when the image is disjoint.
    fn image_memory_requirements(
        &mut self,
        args: &mut GetImageMemoryRequirements2Args,
    ) -> Result<(), ExecError> {
        const NAME: &str = "vkGetImageMemoryRequirements2";
        let device_id = args.device.0;
        let Some(info) = &args.p_info else {
            return Err(invalid(NAME, "pInfo is null"));
        };
        let image = self
            .objects
            .image(device_id, info.image.0)
            .map_err(id_error(NAME))?;
        let mut plane = None;
        for link in &info.p_next {
            match link {
                VkImageMemoryRequirementsInfo2Next::VkImagePlaneMemoryRequirementsInfo(p) => {
                    if !policy::is_plane_aspect(p.plane_aspect) {
                        return Err(invalid(NAME, format!("plane aspect {:#x}", p.plane_aspect)));
                    }
                    plane = Some(p.plane_aspect);
                }
            }
        }
        let disjoint = image.facts.flags & policy::IMAGE_CREATE_DISJOINT != 0;
        let plane_in_range = plane.is_some_and(|aspect| {
            let index = aspect.trailing_zeros().saturating_sub(4);
            index < image.facts.planes
        });
        if disjoint != plane.is_some() || (disjoint && !plane_in_range) {
            return Err(invalid(
                NAME,
                "a plane is named exactly when the image is disjoint, and must be one it has",
            ));
        }
        let (device, guest) = self
            .objects
            .device_and_guest(device_id)
            .map_err(id_error(NAME))?;
        if let Some(out) = args.p_memory_requirements.as_mut() {
            self.host
                .image_memory_requirements(&device.host, image.host, plane, out);
            out.memory_requirements.memory_type_bits = external_type_bits(
                guest,
                image.host_memory,
                image.external,
                out.memory_requirements.memory_type_bits,
            );
            if let Some(layout) = image.modifier {
                let req = &mut out.memory_requirements;
                req.size = layout.requirement(req.size, req.alignment);
            }
        }
        Ok(())
    }
}

/// The rules a `VkImageCreateInfo` is judged by on one device: whether it
/// enabled the emulated dma-buf external memory (stage 5c) and the emulated
/// DRM format modifiers (stage S1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct ImageRules {
    /// `VK_EXT_external_memory_dma_buf` (or `VK_KHR_external_memory_fd`).
    pub dma_buf: bool,
    /// `VK_EXT_image_drm_format_modifier`.
    pub drm_modifier: bool,
}

impl ImageRules {
    /// The rules of `device`.
    pub(super) fn of<H: HostVulkan>(device: &DeviceObject<H>) -> Self {
        Self {
            dma_buf: device.dma_buf(),
            drm_modifier: device.enabled(policy::IMAGE_DRM_FORMAT_MODIFIER),
        }
    }
}

/// How a DRM-modifier image names its modifier (stage S1): a list the
/// implementation picks from — `DRM_FORMAT_MOD_LINEAR` alone here — or an
/// explicit LINEAR with its one plane's layout.
#[derive(Debug, Clone, PartialEq)]
pub(super) enum ModifierChoice {
    /// `VkImageDrmFormatModifierListCreateInfoEXT`, every entry LINEAR.
    List,
    /// `VkImageDrmFormatModifierExplicitCreateInfoEXT` of LINEAR, one plane.
    Explicit(crate::venus::protocol::VkSubresourceLayout),
}

/// What [`check_image_create_info`] found.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct ImageCheck {
    /// Created for `DMA_BUF` export.
    pub external: bool,
    /// A DRM-modifier image, and how it named LINEAR.
    pub modifier: Option<ModifierChoice>,
}

/// Whether a chained structure of type `stype` may reach the driver on a
/// device the guest enabled `enabled` on: always for a core one (up to 1.3),
/// and for one an admitted extension adds, only with one of the extensions
/// that bring it enabled.
fn extension_enabled_for(stype: i32, enabled: &[String]) -> bool {
    crate::venus::protocol::info::structure(stype).is_none_or(|s| {
        s.core
            .is_some_and(|core| core <= policy::ADMITTED_CHAIN_API)
            || s.extensions
                .iter()
                .any(|e| policy::PROTOCOL_EXTENSIONS.contains(e) || enabled.iter().any(|x| x == e))
    })
}

/// The count protocol of a DRM format modifier list (stage S1): with no
/// array, how many entries there are; with one, as many as its capacity
/// (`count`, the skeleton carries no elements) holds, and the count of those
/// — the reply must carry an array exactly its count long.
fn fill_modifier_list<T>(count: &mut u32, array: &mut Option<Vec<T>>, entries: Vec<T>) {
    match array {
        None => *count = u32::try_from(entries.len()).unwrap_or(0),
        Some(slots) => {
            let room = usize::try_from(*count).unwrap_or(usize::MAX);
            let n = room.min(entries.len());
            *slots = entries.into_iter().take(n).collect();
            *count = u32::try_from(n).unwrap_or(0);
        }
    }
}

/// A structure's wire body, as the bytes the generated encoder writes: for a
/// feature structure, one little-endian `VkBool32` per member.
fn words(
    f: impl FnOnce(&mut Encoder) -> Result<(), crate::venus::protocol::ProtocolError>,
) -> Vec<u8> {
    let mut enc = Encoder::new();
    match f(&mut enc) {
        Ok(()) => enc.finish().unwrap_or_default(),
        Err(_) => Vec::new(),
    }
}

/// Whether every `VkBool32` set in `requested` is set in `reported` too. A
/// reported body shorter than the requested one (the host did not know the
/// structure) counts as all false.
fn subset(requested: &[u8], reported: &[u8]) -> bool {
    requested.chunks(4).enumerate().all(|(i, word)| {
        let wants = word.iter().any(|b| *b != 0);
        let has = reported
            .get(i * 4..i * 4 + 4)
            .is_some_and(|w| w.iter().any(|b| *b != 0));
        !wants || has
    })
}

/// The checks `vkGetPhysicalDeviceImageFormatProperties2` gets before the
/// driver sees its info, on a physical device that is (`drm`) or is not
/// shown the emulated `VK_EXT_image_drm_format_modifier` (stage S1): its
/// tiling and its `VkPhysicalDeviceImageDrmFormatModifierInfoEXT` come
/// together or not at all (`VUID-VkPhysicalDeviceImageFormatInfo2-tiling-02249`).
fn check_image_format_info(
    command: &'static str,
    info: &VkPhysicalDeviceImageFormatInfo2,
    drm: bool,
) -> Result<(), ExecError> {
    let drm_tiling = drm && info.tiling == modifier::IMAGE_TILING_DRM_FORMAT_MODIFIER;
    if !policy::is_core_format(info.format)
        || !policy::is_image_type(info.type_)
        || !(policy::is_image_tiling(info.tiling) || drm_tiling)
    {
        return Err(invalid(
            command,
            "format, type or tiling outside Vulkan 1.3 and the extensions shown",
        ));
    }
    let modifier_info = info.p_next.iter().any(|l| {
        matches!(
            l,
            VkPhysicalDeviceImageFormatInfo2Next::VkPhysicalDeviceImageDrmFormatModifierInfoEXT(_)
        )
    });
    if modifier_info != drm_tiling {
        return Err(invalid(
            command,
            "VkPhysicalDeviceImageDrmFormatModifierInfoEXT is chained exactly for DRM format \
             modifier tiling, on a device shown the extension",
        ));
    }
    if info.usage == 0 || info.usage & !policy::IMAGE_USAGE_CORE != 0 {
        return Err(invalid(command, format!("usage {:#x}", info.usage)));
    }
    if info.flags & !policy::IMAGE_CREATE_CORE != 0 {
        return Err(invalid(command, format!("flags {:#x}", info.flags)));
    }
    for link in &info.p_next {
        match link {
            VkPhysicalDeviceImageFormatInfo2Next::VkPhysicalDeviceExternalImageFormatInfo(e) => {
                let dma_buf =
                    u32::try_from(e.handle_type).ok() == Some(policy::MEMORY_HANDLE_DMA_BUF);
                if !policy::is_handle_type_bit(e.handle_type) && !dma_buf {
                    return Err(invalid(
                        command,
                        format!("handle type {:#x}", e.handle_type),
                    ));
                }
            }
            VkPhysicalDeviceImageFormatInfo2Next::VkImageFormatListCreateInfo(l) => {
                check_view_formats(command, l.p_view_formats.as_deref())?;
            }
            VkPhysicalDeviceImageFormatInfo2Next::VkImageStencilUsageCreateInfo(s) => {
                check_usage(command, s.stencil_usage)?;
            }
            VkPhysicalDeviceImageFormatInfo2Next::VkPhysicalDeviceImageDrmFormatModifierInfoEXT(
                m,
            ) => {
                // Judged as a query (`modifier::image_format_properties`);
                // only the value ranges are checked here.
                if !policy::is_sharing_mode(m.sharing_mode) {
                    return Err(invalid(command, "a sharing mode outside Vulkan 1.3"));
                }
            }
            other => {
                return Err(unimplemented_link(
                    command,
                    "VkPhysicalDeviceImageFormatInfo2",
                    ChainLink::structure_type(other),
                ))
            }
        }
    }
    Ok(())
}

fn check_usage(command: &'static str, usage: u32) -> Result<(), ExecError> {
    if usage == 0 || usage & !policy::IMAGE_USAGE_CORE != 0 {
        return Err(invalid(command, format!("usage {usage:#x}")));
    }
    Ok(())
}

fn check_view_formats(command: &'static str, formats: Option<&[i32]>) -> Result<(), ExecError> {
    if formats
        .unwrap_or_default()
        .iter()
        .any(|f| *f == 0 || !policy::is_core_format(*f))
    {
        return Err(invalid(command, "a view format outside Vulkan 1.3"));
    }
    Ok(())
}

/// Whether an image of `info` may live in exportable device-local memory
/// (stage S1): the device can export at all ([`GuestDevice::memory_export`]),
/// and the host answers `OPAQUE_WIN32` `EXPORTABLE | IMPORTABLE`, not
/// dedicated-only, for its format, type, tiling, usage, flags and view
/// formats.
pub(super) fn image_exportable<H: HostVulkan>(
    host: &H,
    instance: &H::Instance,
    physical: H::PhysicalDevice,
    guest: &GuestDevice,
    info: &VkImageCreateInfo,
) -> bool {
    if !guest.memory_export {
        return false;
    }
    let mut p_next = vec![
        VkPhysicalDeviceImageFormatInfo2Next::VkPhysicalDeviceExternalImageFormatInfo(
            crate::venus::protocol::VkPhysicalDeviceExternalImageFormatInfo {
                handle_type: policy::MEMORY_HANDLE_OPAQUE_WIN32 as i32,
            },
        ),
    ];
    for link in &info.p_next {
        if let VkImageCreateInfoNext::VkImageFormatListCreateInfo(l) = link {
            p_next
                .push(VkPhysicalDeviceImageFormatInfo2Next::VkImageFormatListCreateInfo(l.clone()));
        }
    }
    let query = VkPhysicalDeviceImageFormatInfo2 {
        p_next,
        format: info.format,
        type_: info.image_type,
        tiling: info.tiling,
        usage: info.usage,
        flags: info.flags,
    };
    let mut out = VkImageFormatProperties2 {
        p_next: vec![
            VkImageFormatProperties2Next::VkExternalImageFormatProperties(Default::default()),
        ],
        image_format_properties: Default::default(),
    };
    if host.image_format_properties(instance, physical, &query, &mut out) != VK_SUCCESS {
        return false;
    }
    out.p_next.iter().any(|l| {
        let VkImageFormatProperties2Next::VkExternalImageFormatProperties(p) = l else {
            return false;
        };
        let features = p.external_memory_properties.external_memory_features;
        let want = policy::MEMORY_FEATURE_EXPORTABLE | policy::MEMORY_FEATURE_IMPORTABLE;
        features & want == want && features & policy::MEMORY_FEATURE_DEDICATED_ONLY == 0
    })
}

/// Whether an image of `info` is inside what the host's own
/// `vkGetPhysicalDeviceImageFormatProperties2` reports for its format, type,
/// tiling, usage and flags: an image outside them is invalid usage the
/// driver is not required to survive.
pub(super) fn image_limits_hold<H: HostVulkan>(
    host: &H,
    instance: &H::Instance,
    physical: H::PhysicalDevice,
    info: &VkImageCreateInfo,
) -> bool {
    let query = VkPhysicalDeviceImageFormatInfo2 {
        p_next: Vec::new(),
        format: info.format,
        type_: info.image_type,
        tiling: info.tiling,
        usage: info.usage,
        flags: info.flags,
    };
    let mut limits = VkImageFormatProperties2::default();
    let ret = host.image_format_properties(instance, physical, &query, &mut limits);
    let limits = &limits.image_format_properties;
    let samples = u32::try_from(info.samples).unwrap_or(0);
    ret == VK_SUCCESS
        && info.extent.width <= limits.max_extent.width
        && info.extent.height <= limits.max_extent.height
        && info.extent.depth <= limits.max_extent.depth
        && info.mip_levels <= limits.max_mip_levels
        && info.array_layers <= limits.max_array_layers
        && limits.sample_counts & samples != 0
}

/// The checks `vkCreateImage` gets before the driver sees its create info,
/// on a device under `rules`. Answers whether the image is created for
/// `DMA_BUF` export (stage 5c) — which Zink asks for every shared image,
/// whatever the image-format query said (`zink_resource.c:1336-1340`,
/// `:1504`, `OPAQUE_FD` rewritten to `DMA_BUF` by Mesa's venus): it is
/// created as any image is, and an export of its memory succeeds exactly
/// when that memory is ours — and whether it is a DRM-modifier image (stage
/// S1), whose modifier must be `DRM_FORMAT_MOD_LINEAR`, named by exactly one
/// of a list and an explicit create info (`VUID-VkImageCreateInfo-tiling-02261`,
/// `-pNext-02262`), the explicit one of one plane with a zero size, array
/// pitch and depth pitch (`VUID-VkImageDrmFormatModifierExplicitCreateInfoEXT-size-02267`
/// and the two after it).
pub(super) fn check_image_create_info(
    command: &'static str,
    info: &VkImageCreateInfo,
    guest: &GuestDevice,
    rules: ImageRules,
) -> Result<ImageCheck, ExecError> {
    let drm_tiling =
        rules.drm_modifier && info.tiling == modifier::IMAGE_TILING_DRM_FORMAT_MODIFIER;
    if info.flags & !policy::IMAGE_CREATE_CORE != 0 || info.flags & policy::IMAGE_CREATE_SPARSE != 0
    {
        return Err(invalid(command, format!("flags {:#x}", info.flags)));
    }
    if info.flags & policy::IMAGE_CREATE_PROTECTED != 0 && !guest.protected_memory() {
        return Err(invalid(
            command,
            "a protected image without protectedMemory",
        ));
    }
    if info.format == 0
        || !policy::is_core_format(info.format)
        || !policy::is_image_type(info.image_type)
        || !(policy::is_image_tiling(info.tiling) || drm_tiling)
        || !policy::is_sharing_mode(info.sharing_mode)
        || !policy::is_sample_count(info.samples)
        || !policy::is_initial_layout(info.initial_layout)
    {
        return Err(invalid(
            command,
            "format, type, tiling, sharing mode, samples or layout outside Vulkan 1.3",
        ));
    }
    if info.extent.width == 0
        || info.extent.height == 0
        || info.extent.depth == 0
        || info.mip_levels == 0
        || info.array_layers == 0
    {
        return Err(invalid(command, "a zero extent, mip count or layer count"));
    }
    check_usage(command, info.usage)?;
    if info.sharing_mode == VK_SHARING_MODE_CONCURRENT {
        let families = info.p_queue_family_indices.as_deref().unwrap_or_default();
        if families.len() < 2
            || families
                .iter()
                .any(|f| usize::try_from(*f).map_or(true, |f| f >= guest.queue_families.len()))
        {
            return Err(invalid(
                command,
                "concurrent sharing needs two or more real families",
            ));
        }
    }
    let mut external = false;
    let mut chosen: Vec<ModifierChoice> = Vec::new();
    let not_linear = |what: &str| {
        invalid(
            command,
            format!(
                "{what}: DRM_FORMAT_MOD_LINEAR is the only modifier offered, and only for DRM \
                 format modifier tiling"
            ),
        )
    };
    for link in &info.p_next {
        match link {
            VkImageCreateInfoNext::VkExternalMemoryImageCreateInfo(e) => {
                external |= external_handle_types(command, e.handle_types, rules.dma_buf)?;
            }
            VkImageCreateInfoNext::VkImageFormatListCreateInfo(l) => {
                check_view_formats(command, l.p_view_formats.as_deref())?;
            }
            VkImageCreateInfoNext::VkImageStencilUsageCreateInfo(s) => {
                check_usage(command, s.stencil_usage)?;
            }
            VkImageCreateInfoNext::VkImageDrmFormatModifierListCreateInfoEXT(l) => {
                let list = l.p_drm_format_modifiers.as_deref().unwrap_or_default();
                if !drm_tiling
                    || list.is_empty()
                    || list.iter().any(|m| *m != modifier::DRM_FORMAT_MOD_LINEAR)
                {
                    return Err(not_linear("a modifier list"));
                }
                chosen.push(ModifierChoice::List);
            }
            VkImageCreateInfoNext::VkImageDrmFormatModifierExplicitCreateInfoEXT(e) => {
                let planes = e.p_plane_layouts.as_deref().unwrap_or_default();
                if !drm_tiling || e.drm_format_modifier != modifier::DRM_FORMAT_MOD_LINEAR {
                    return Err(not_linear("an explicit modifier"));
                }
                let [plane] = planes else {
                    return Err(invalid(
                        command,
                        "an explicit LINEAR modifier of other than one plane",
                    ));
                };
                if plane.size != 0 || plane.array_pitch != 0 || plane.depth_pitch != 0 {
                    return Err(invalid(
                        command,
                        "an explicit plane layout with a size, array pitch or depth pitch",
                    ));
                }
                chosen.push(ModifierChoice::Explicit(plane.clone()));
            }
            other => {
                return Err(unimplemented_link(
                    command,
                    "VkImageCreateInfo",
                    ChainLink::structure_type(other),
                ))
            }
        }
    }
    let modifier = match (drm_tiling, chosen.len()) {
        (false, _) => None,
        (true, 1) => chosen.pop(),
        (true, _) => {
            return Err(invalid(
                command,
                "DRM format modifier tiling needs exactly one of a modifier list and an \
                 explicit modifier",
            ))
        }
    };
    Ok(ImageCheck { external, modifier })
}
