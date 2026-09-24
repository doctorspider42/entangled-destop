//! The host Vulkan the executor drives: exactly the calls stages 5a.3 and
//! 5b.1 make, as a trait, so that everything above it — the object table, the id rules,
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
use std::sync::Arc;

use crate::venus::protocol::{
    VkBufferCreateInfo, VkBufferViewCreateInfo, VkCommandPoolCreateInfo, VkDeviceCreateInfoNext,
    VkExtensionProperties, VkFormat, VkFormatProperties2, VkImageAspectFlagBits, VkImageCreateInfo,
    VkImageFormatProperties2, VkImageSubresource, VkImageViewCreateInfo, VkMemoryRequirements2,
    VkPhysicalDeviceFeatures, VkPhysicalDeviceFeatures2, VkPhysicalDeviceImageFormatInfo2,
    VkPhysicalDeviceMemoryProperties, VkPhysicalDeviceProperties2, VkQueueFamilyProperties,
    VkResult, VkSubresourceLayout,
};
use crate::venus::shmem::RingPages;

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
    /// `VkPhysicalDeviceExternalMemoryHostPropertiesEXT::minImportedHostPointerAlignment`,
    /// 0 when unknown.
    pub host_import_alignment: u64,
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

/// The resource a `VkMemoryDedicatedAllocateInfo` names, as host handles.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dedicated<B, I> {
    /// A buffer.
    Buffer(B),
    /// An image.
    Image(I),
}

/// A `vkAllocateMemory`, rebuilt from the decoded structures and validated.
#[derive(Debug, Clone)]
pub struct MemoryRequest<B, I> {
    /// `allocationSize` as the host is asked for it: the guest's, or — for
    /// an import — the whole of `import`, which is the guest's rounded up to
    /// the driver's import alignment.
    pub size: u64,
    /// `memoryTypeIndex`, the host's own (the guest sees the same indices).
    pub type_index: u32,
    /// Our pages, to import with `VK_EXT_external_memory_host`
    /// (`HOST_ALLOCATION_BIT_EXT`). The host keeps this `Arc` for as long as
    /// the memory object lives and drops it only after `vkFreeMemory` has
    /// returned, so the driver can never be left importing freed pages.
    pub import: Option<Arc<RingPages>>,
    /// `VkMemoryAllocateFlagsInfo`: `(flags, deviceMask)`.
    pub flags: Option<(u32, u32)>,
    /// `VkMemoryDedicatedAllocateInfo`, never with an import.
    pub dedicated: Option<Dedicated<B, I>>,
}

/// One `VkBindImageMemoryInfo`, host side.
#[derive(Debug)]
pub struct ImageBind<'m, I, M> {
    /// The image.
    pub image: I,
    /// The memory.
    pub memory: &'m M,
    /// `memoryOffset`.
    pub offset: u64,
    /// A chained `VkBindImagePlaneMemoryInfo`'s plane, for a disjoint image.
    pub plane: Option<VkImageAspectFlagBits>,
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
    /// A host `VkDeviceMemory` together with whatever keeps it valid (the
    /// imported pages' `Arc`). Owned: [`HostVulkan::free_memory`] takes it.
    type Memory: Send + 'static;
    /// A host `VkBuffer`.
    type Buffer: Copy + Send + 'static;
    /// A host `VkBufferView`.
    type BufferView: Copy + Send + 'static;
    /// A host `VkImageView`.
    type ImageView: Copy + Send + 'static;

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

    /// Whether an image created from `info` may be bound to our imported
    /// pages — `VK_EXTERNAL_MEMORY_HANDLE_TYPE_HOST_ALLOCATION_BIT_EXT` is
    /// `IMPORTABLE` for it — so that it is created ready for them and its
    /// requirements may name the host-visible types.
    fn image_accepts_host_memory(&self, device: &Self::Device, info: &VkImageCreateInfo) -> bool;

    /// `vkCreateImage` from a validated create info and its chain; with
    /// `host_memory`, created with a `VkExternalMemoryImageCreateInfo` for
    /// host allocations, as binding imported memory requires.
    ///
    /// # Errors
    /// The driver's `VkResult`.
    fn create_image(
        &self,
        device: &Self::Device,
        info: &VkImageCreateInfo,
        host_memory: bool,
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

    /// `vkGetDeviceImageMemoryRequirements` for an image that
    /// [`create_image`](Self::create_image) would make of `info` and
    /// `host_memory`, filling `out` and exactly the output links it carries.
    fn device_image_memory_requirements(
        &self,
        device: &Self::Device,
        info: &VkImageCreateInfo,
        host_memory: bool,
        plane: Option<VkImageAspectFlagBits>,
        out: &mut VkMemoryRequirements2,
    );

    /// `vkGetImageSubresourceLayout` for a linear image.
    fn image_subresource_layout(
        &self,
        device: &Self::Device,
        image: Self::Image,
        subresource: &VkImageSubresource,
    ) -> VkSubresourceLayout;

    /// `vkBindImageMemory2`, every bind validated.
    fn bind_image_memory(
        &self,
        device: &Self::Device,
        binds: &[ImageBind<'_, Self::Image, Self::Memory>],
    ) -> VkResult;

    /// `vkCreateImageView` of `image` from a validated create info.
    ///
    /// # Errors
    /// The driver's `VkResult`.
    fn create_image_view(
        &self,
        device: &Self::Device,
        image: Self::Image,
        info: &VkImageViewCreateInfo,
    ) -> Result<Self::ImageView, VkResult>;

    /// `vkDestroyImageView`.
    fn destroy_image_view(&self, device: &Self::Device, view: Self::ImageView);

    // ------------------------------------------------------------- memory

    /// `vkGetMemoryHostPointerPropertiesEXT` for `pages`: the memory types
    /// they may be imported into.
    ///
    /// # Errors
    /// The driver's `VkResult`.
    fn host_pointer_types(&self, device: &Self::Device, pages: &RingPages)
        -> Result<u32, VkResult>;

    /// `vkAllocateMemory`.
    ///
    /// # Errors
    /// The driver's `VkResult`.
    fn allocate_memory(
        &self,
        device: &Self::Device,
        request: &MemoryRequest<Self::Buffer, Self::Image>,
    ) -> Result<Self::Memory, VkResult>;

    /// `vkFreeMemory`. For imported memory the host waits for the device to
    /// go idle first: pages the GPU may still be writing are not pages the
    /// executor may let go of.
    fn free_memory(&self, device: &Self::Device, memory: Self::Memory);

    /// `vkGetDeviceMemoryCommitment`, for a lazily allocated type.
    fn memory_commitment(&self, device: &Self::Device, memory: &Self::Memory) -> u64;

    // ------------------------------------------------------------ buffers

    /// Whether a buffer of these `flags` and `usage` may be bound to our
    /// imported pages; see [`image_accepts_host_memory`](Self::image_accepts_host_memory).
    fn buffer_accepts_host_memory(&self, device: &Self::Device, flags: u32, usage: u32) -> bool;

    /// `vkCreateBuffer` from a validated create info; `host_memory` as for
    /// [`create_image`](Self::create_image).
    ///
    /// # Errors
    /// The driver's `VkResult`.
    fn create_buffer(
        &self,
        device: &Self::Device,
        info: &VkBufferCreateInfo,
        host_memory: bool,
    ) -> Result<Self::Buffer, VkResult>;

    /// `vkDestroyBuffer`.
    fn destroy_buffer(&self, device: &Self::Device, buffer: Self::Buffer);

    /// `vkGetBufferMemoryRequirements2`, filling `out` and exactly the output
    /// links it already carries.
    fn buffer_memory_requirements(
        &self,
        device: &Self::Device,
        buffer: Self::Buffer,
        out: &mut VkMemoryRequirements2,
    );

    /// `vkGetDeviceBufferMemoryRequirements` for the buffer
    /// [`create_buffer`](Self::create_buffer) would make of the same
    /// arguments.
    fn device_buffer_memory_requirements(
        &self,
        device: &Self::Device,
        info: &VkBufferCreateInfo,
        host_memory: bool,
        out: &mut VkMemoryRequirements2,
    );

    /// `vkBindBufferMemory2`, every bind validated: `(buffer, memory,
    /// offset)`.
    fn bind_buffer_memory(
        &self,
        device: &Self::Device,
        binds: &[(Self::Buffer, &Self::Memory, u64)],
    ) -> VkResult;

    /// `vkGetBufferDeviceAddress`, for a buffer created with
    /// `SHADER_DEVICE_ADDRESS` usage on a device with `bufferDeviceAddress`.
    fn buffer_device_address(&self, device: &Self::Device, buffer: Self::Buffer) -> u64;

    /// `vkCreateBufferView` of `buffer` from a validated create info.
    ///
    /// # Errors
    /// The driver's `VkResult`.
    fn create_buffer_view(
        &self,
        device: &Self::Device,
        buffer: Self::Buffer,
        info: &VkBufferViewCreateInfo,
    ) -> Result<Self::BufferView, VkResult>;

    /// `vkDestroyBufferView`.
    fn destroy_buffer_view(&self, device: &Self::Device, view: Self::BufferView);
}
