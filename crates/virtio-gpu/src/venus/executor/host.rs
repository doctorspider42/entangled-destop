//! The host Vulkan the executor drives: exactly the calls stage 5a.3 makes,
//! as a trait, so that everything above it — the object table, the id rules,
//! the policies and the reply shapes — is provable on a host with no GPU.
//!
//! The trait speaks the **generated protocol structures**, not `ash`'s. That
//! is the seam's whole point: a fake implements it by filling in plain Rust
//! values, and the one real implementation
//! ([`crate::host_vulkan::AshVulkan`]) is the only code that ever converts a
//! guest-shaped value into a driver-shaped one. Nothing reaches an
//! implementation that the executor has not already validated — ids resolved
//! to host objects, enums and flag words range-checked, counts clamped.
//!
//! Host objects are associated types and are **owned** by whoever holds them:
//! `destroy_*` takes the value, so an object cannot be destroyed twice or used
//! after destruction without the executor's table having handed it out twice,
//! which it cannot.

use std::fmt;

use crate::venus::protocol::{
    VkCommandPoolCreateInfo, VkDeviceCreateInfoNext, VkExtensionProperties, VkFormat,
    VkFormatProperties2, VkImageAspectFlagBits, VkImageCreateInfo, VkImageFormatProperties2,
    VkMemoryRequirements2, VkPhysicalDeviceFeatures, VkPhysicalDeviceFeatures2,
    VkPhysicalDeviceImageFormatInfo2, VkPhysicalDeviceMemoryProperties,
    VkPhysicalDeviceProperties2, VkQueueFamilyProperties, VkResult,
};

/// Everything the host says about one physical device, gathered once when
/// a guest instance first enumerates (properties and features are invariant,
/// which is why virglrenderer caches them too).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct HostDeviceInfo {
    /// `vkGetPhysicalDeviceProperties2` with every structure the protocol can
    /// carry in that chain and the device's version knows, in any order.
    pub properties: VkPhysicalDeviceProperties2,
    /// `vkGetPhysicalDeviceFeatures2`, likewise.
    pub features: VkPhysicalDeviceFeatures2,
    /// `vkGetPhysicalDeviceQueueFamilyProperties`.
    pub queue_families: Vec<VkQueueFamilyProperties>,
    /// `vkGetPhysicalDeviceMemoryProperties`, exactly as the host reports it.
    pub memory: VkPhysicalDeviceMemoryProperties,
    /// `vkEnumerateDeviceExtensionProperties(NULL layer)`.
    pub extensions: Vec<VkExtensionProperties>,
    /// The memory types that accept an import of **our own host pages**
    /// (`VK_EXT_external_memory_host`, `HOST_ALLOCATION`), as the
    /// `memoryTypeBits` `vkGetMemoryHostPointerPropertiesEXT` answers. `None`
    /// when the device lacks the extension or the probe failed — and such a
    /// device is not exposed, because nothing it could map is memory the VMM
    /// owns (ADR-0004, 2026-09-23).
    pub host_import_types: Option<u32>,
}

/// One `VkPhysicalDeviceGroupProperties`, host side.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostGroup<P> {
    /// The group's members, as host handles.
    pub members: Vec<P>,
    /// `subsetAllocation`.
    pub subset_allocation: bool,
}

/// The application identity a guest instance asked for, validated as UTF-8.
/// Forwarded because drivers key application profiles on it, as
/// virglrenderer forwards it.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct InstanceRequest {
    /// `pApplicationName`.
    pub application_name: Option<String>,
    /// `applicationVersion`.
    pub application_version: u32,
    /// `pEngineName`.
    pub engine_name: Option<String>,
    /// `engineVersion`.
    pub engine_version: u32,
}

/// One `VkDeviceQueueCreateInfo`, validated.
#[derive(Debug, Clone, PartialEq)]
pub struct QueueRequest {
    /// `flags` (zero or `VK_DEVICE_QUEUE_CREATE_PROTECTED_BIT`).
    pub flags: u32,
    /// `queueFamilyIndex`, inside the host's family count.
    pub family: u32,
    /// `pQueuePriorities`, one per queue, each in `[0, 1]`.
    pub priorities: Vec<f32>,
}

/// A `vkCreateDevice`, rebuilt from the decoded structures and validated —
/// never the guest's bytes.
#[derive(Debug, Clone, PartialEq)]
pub struct DeviceRequest<P> {
    /// The queues, at most one entry per `(family, flags)`.
    pub queues: Vec<QueueRequest>,
    /// Extensions to enable: the guest's (each one we advertised) plus the
    /// ones the renderer needs for itself.
    pub extensions: Vec<String>,
    /// `pEnabledFeatures`.
    pub features: Option<VkPhysicalDeviceFeatures>,
    /// The feature and private-data structures of the pNext chain.
    /// `VkDeviceGroupDeviceCreateInfo` is never in here: it carries handles,
    /// which arrive translated in [`DeviceRequest::group`].
    pub chain: Vec<VkDeviceCreateInfoNext>,
    /// `VkDeviceGroupDeviceCreateInfo::pPhysicalDevices`, as host handles.
    pub group: Option<Vec<P>>,
}

/// The host Vulkan calls this stage makes. See the module docs.
///
/// Every method is infallible where the Vulkan call is, and returns the
/// driver's `VkResult` where it is not; a result is handed to the guest as
/// the command's return value, never turned into a fatal error, because a
/// driver refusing a well-formed request is an answer, not a protocol fault.
pub trait HostVulkan: Send + Sync + 'static {
    /// A host `VkInstance` together with whatever calling it needs.
    type Instance: Send + 'static;
    /// A host `VkPhysicalDevice`.
    type PhysicalDevice: Copy + Eq + Send + fmt::Debug + 'static;
    /// A host `VkDevice` together with whatever calling it needs.
    type Device: Send + 'static;
    /// A host `VkQueue`.
    type Queue: Copy + Send + 'static;
    /// A host `VkCommandPool`.
    type CommandPool: Copy + Send + 'static;
    /// A host `VkImage`.
    type Image: Copy + Send + 'static;

    /// `vkEnumerateInstanceVersion` of the host loader.
    ///
    /// # Errors
    /// The loader's `VkResult`.
    fn instance_version(&self) -> Result<u32, VkResult>;

    /// `vkCreateInstance` at Vulkan 1.3, no layers, and only the instance
    /// extensions the renderer itself needs (none, this stage).
    ///
    /// # Errors
    /// The driver's `VkResult`.
    fn create_instance(&self, request: &InstanceRequest) -> Result<Self::Instance, VkResult>;

    /// `vkDestroyInstance`. Every child has already been destroyed.
    fn destroy_instance(&self, instance: Self::Instance);

    /// `vkEnumeratePhysicalDevices`, all of them.
    ///
    /// # Errors
    /// The driver's `VkResult`.
    fn enumerate_physical_devices(
        &self,
        instance: &Self::Instance,
    ) -> Result<Vec<Self::PhysicalDevice>, VkResult>;

    /// Everything [`HostDeviceInfo`] holds, for one device.
    fn describe_physical_device(
        &self,
        instance: &Self::Instance,
        device: Self::PhysicalDevice,
    ) -> HostDeviceInfo;

    /// `vkEnumeratePhysicalDeviceGroups`, all of them.
    ///
    /// # Errors
    /// The driver's `VkResult`.
    fn physical_device_groups(
        &self,
        instance: &Self::Instance,
    ) -> Result<Vec<HostGroup<Self::PhysicalDevice>>, VkResult>;

    /// `vkGetPhysicalDeviceFormatProperties2`, filling `out` and exactly the
    /// output links it already carries.
    fn format_properties(
        &self,
        instance: &Self::Instance,
        device: Self::PhysicalDevice,
        format: VkFormat,
        out: &mut VkFormatProperties2,
    );

    /// `vkGetPhysicalDeviceImageFormatProperties2`, filling `out` and exactly
    /// the output links it already carries.
    fn image_format_properties(
        &self,
        instance: &Self::Instance,
        device: Self::PhysicalDevice,
        info: &VkPhysicalDeviceImageFormatInfo2,
        out: &mut VkImageFormatProperties2,
    ) -> VkResult;

    /// `vkCreateDevice`.
    ///
    /// # Errors
    /// The driver's `VkResult`.
    fn create_device(
        &self,
        instance: &Self::Instance,
        device: Self::PhysicalDevice,
        request: &DeviceRequest<Self::PhysicalDevice>,
    ) -> Result<Self::Device, VkResult>;

    /// `vkDestroyDevice`. Every child has already been destroyed.
    fn destroy_device(&self, device: Self::Device);

    /// `vkGetDeviceQueue2` for a queue the device was created with.
    fn device_queue(
        &self,
        device: &Self::Device,
        flags: u32,
        family: u32,
        index: u32,
    ) -> Self::Queue;

    /// `vkCreateCommandPool`.
    ///
    /// # Errors
    /// The driver's `VkResult`.
    fn create_command_pool(
        &self,
        device: &Self::Device,
        info: &VkCommandPoolCreateInfo,
    ) -> Result<Self::CommandPool, VkResult>;

    /// `vkDestroyCommandPool`.
    fn destroy_command_pool(&self, device: &Self::Device, pool: Self::CommandPool);

    /// `vkCreateImage` from a validated create info and its chain.
    ///
    /// # Errors
    /// The driver's `VkResult`.
    fn create_image(
        &self,
        device: &Self::Device,
        info: &VkImageCreateInfo,
    ) -> Result<Self::Image, VkResult>;

    /// `vkDestroyImage`.
    fn destroy_image(&self, device: &Self::Device, image: Self::Image);

    /// `vkGetImageMemoryRequirements2`, filling `out` and exactly the output
    /// links it already carries. `plane` is a chained
    /// `VkImagePlaneMemoryRequirementsInfo`.
    fn image_memory_requirements(
        &self,
        device: &Self::Device,
        image: Self::Image,
        plane: Option<VkImageAspectFlagBits>,
        out: &mut VkMemoryRequirements2,
    );
}
