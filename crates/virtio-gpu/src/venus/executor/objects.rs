//! One context's object table: the guest's 64-bit ids, bound to host objects
//! (spec §3).
//!
//! The rules are virglrenderer's (`vkr_context.h:164-224`, `vkr_cs.h:301-325`)
//! plus the ones it leaves to the host driver:
//!
//! * **An id names exactly one object in the whole context**, whatever its
//!   type. Creating with an id that is 0 or already taken is fatal.
//! * **Every lookup is typed.** A `VkDevice` id handed where a `VkImage` is
//!   expected is fatal, and so is an id nobody created. Id 0 is accepted only
//!   where the handle is optional (the `vkDestroy*` family), and means "do
//!   nothing".
//! * **Every child records its parent**, and a command that names a child
//!   through the wrong parent (an image of device A destroyed through device
//!   B) is fatal. virglrenderer relies on the driver for this; we do not hand
//!   the driver a pair it has to judge.
//! * **Destruction is in dependency order** — image views, buffer views,
//!   images, buffers, device memory, command pools, queues, then devices,
//!   then physical devices, then the instance — whether it comes from a
//!   `vkDestroy*`, from the context going away, or from a device reset, and
//!   every host object is destroyed exactly once because destroying it
//!   *takes* it out of the table. Memory goes after everything that may be
//!   bound to it; its pages go when the last holder does (the blob of it may
//!   outlive the table — [`crate::venus::shmem`] has the argument).

use std::collections::HashMap;
use std::sync::Arc;

use super::host::HostVulkan;
use super::policy::GuestDevice;
use crate::venus::shmem::RingPages;

/// Most objects one context may hold. A guest id names a host allocation,
/// so the table is bounded like every other guest-sized thing in this crate;
/// past it a create answers `VK_ERROR_OUT_OF_HOST_MEMORY`, which is what a
/// driver out of room says.
pub const MAX_OBJECTS_PER_CONTEXT: usize = 1 << 16;

/// The object types this stage can create.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Kind {
    /// `VkInstance`.
    Instance,
    /// `VkPhysicalDevice`.
    PhysicalDevice,
    /// `VkDevice`.
    Device,
    /// `VkQueue`.
    Queue,
    /// `VkCommandPool`.
    CommandPool,
    /// `VkImage`.
    Image,
    /// `VkDeviceMemory`.
    DeviceMemory,
    /// `VkBuffer`.
    Buffer,
    /// `VkBufferView`.
    BufferView,
    /// `VkImageView`.
    ImageView,
}

impl Kind {
    /// The Vulkan type name, for refusals.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Instance => "VkInstance",
            Self::PhysicalDevice => "VkPhysicalDevice",
            Self::Device => "VkDevice",
            Self::Queue => "VkQueue",
            Self::CommandPool => "VkCommandPool",
            Self::Image => "VkImage",
            Self::DeviceMemory => "VkDeviceMemory",
            Self::Buffer => "VkBuffer",
            Self::BufferView => "VkBufferView",
            Self::ImageView => "VkImageView",
        }
    }
}

/// Why an id was refused. Every one is fatal to the context.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum IdError {
    /// Id 0 where the handle is required.
    #[error("a {0} id of 0 where the handle is required")]
    Zero(&'static str),
    /// A create naming an id the context already holds.
    #[error("object id {id:#x} is already a {existing}")]
    Duplicate {
        /// The id.
        id: u64,
        /// What it already names.
        existing: &'static str,
    },
    /// An id nobody created (or one already destroyed).
    #[error("object id {id:#x} names no {expected}")]
    Unknown {
        /// The id.
        id: u64,
        /// What was expected.
        expected: &'static str,
    },
    /// An id of the wrong type.
    #[error("object id {id:#x} is a {found}, not the {expected} expected")]
    WrongType {
        /// The id.
        id: u64,
        /// What was expected.
        expected: &'static str,
        /// What it is.
        found: &'static str,
    },
    /// A child named through a parent that is not its own.
    #[error("{child} {id:#x} does not belong to {parent} {parent_id:#x}")]
    WrongParent {
        /// The child's type.
        child: &'static str,
        /// The child's id.
        id: u64,
        /// The parent's type.
        parent: &'static str,
        /// The parent named.
        parent_id: u64,
    },
}

/// The one instance a context holds.
pub struct InstanceObject<H: HostVulkan> {
    /// The host instance.
    pub host: H::Instance,
    /// The exposed devices, filled on the first enumeration and fixed
    /// thereafter: the host handle, what the guest is shown, and the guest id
    /// once one is bound.
    pub devices: Option<Vec<ExposedDevice<H>>>,
}

/// One exposed physical device of the instance.
pub struct ExposedDevice<H: HostVulkan> {
    /// The host handle.
    pub host: H::PhysicalDevice,
    /// What the guest is told about it.
    pub guest: GuestDevice,
    /// The guest id bound to it, once one is.
    pub id: Option<u64>,
}

/// A queue the device was created with, and whether the guest has fetched it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CreatedQueue {
    /// `VkDeviceQueueCreateInfo::flags`.
    pub flags: u32,
    /// Its family.
    pub family: u32,
    /// Its index within the family.
    pub index: u32,
    /// The guest id bound to it by `vkGetDeviceQueue2`.
    pub id: Option<u64>,
}

/// A `VkDevice`.
pub struct DeviceObject<H: HostVulkan> {
    /// The host device.
    pub host: H::Device,
    /// The guest id of its physical device.
    pub physical: u64,
    /// Every queue it was created with.
    pub queues: Vec<CreatedQueue>,
    /// Physical devices in its device group (1 without a group).
    pub group_size: u32,
    /// Whether it was created with `bufferDeviceAddress` enabled.
    pub buffer_device_address: bool,
}

/// A `VkQueue`.
pub struct QueueObject<H: HostVulkan> {
    /// Its device's guest id.
    pub device: u64,
    /// The host queue.
    pub host: H::Queue,
    /// The virtio-gpu fence timeline (`ring_idx`) the guest bound it to.
    pub ring_idx: u32,
    /// Its queue family.
    pub family: u32,
}

/// A `VkCommandPool`: a host handle and its device.
pub struct DeviceChild<T> {
    /// Its device's guest id.
    pub device: u64,
    /// The host handle.
    pub host: T,
}

/// What a `VkMemoryDedicatedAllocateInfo` named, by guest id.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DedicatedTo {
    /// A buffer.
    Buffer(u64),
    /// An image.
    Image(u64),
}

/// A `VkDeviceMemory`.
pub struct MemoryObject<H: HostVulkan> {
    /// Its device's guest id.
    pub device: u64,
    /// The host memory.
    pub host: H::Memory,
    /// `allocationSize` as the guest asked for it — what every bind is
    /// judged against (the host's may be rounded up past it).
    pub size: u64,
    /// `memoryTypeIndex`.
    pub type_index: u32,
    /// The type's property flags **as the guest sees them**.
    pub property_flags: u32,
    /// Our imported pages, for a host-visible type: the same `Arc` the host
    /// memory keeps and a blob of this memory wraps.
    pub pages: Option<Arc<RingPages>>,
    /// Whether a blob was made of it: once only, as in vkr
    /// (`vkr_device_memory_export_blob`), so two resources never share one
    /// storage.
    pub exported: bool,
    /// The dedicated resource, if the allocation named one.
    pub dedicated: Option<DedicatedTo>,
    /// `VkMemoryAllocateFlagsInfo::flags`, 0 without one.
    pub allocate_flags: u32,
}

/// Where a buffer or an image plane is bound.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Binding {
    /// The memory's guest id.
    pub memory: u64,
    /// `memoryOffset`.
    pub offset: u64,
}

/// A `VkBuffer`.
pub struct BufferObject<H: HostVulkan> {
    /// Its device's guest id.
    pub device: u64,
    /// The host buffer.
    pub host: H::Buffer,
    /// `size`.
    pub size: u64,
    /// `usage`.
    pub usage: u32,
    /// `flags`.
    pub flags: u32,
    /// Whether it was created able to take our imported pages; its
    /// `memoryTypeBits` name the host-visible types only if so.
    pub host_memory: bool,
    /// Its binding, once bound. A buffer is bound at most once.
    pub bound: Option<Binding>,
}

/// What the executor keeps of a `VkImageCreateInfo`: every field a later
/// command is judged against.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ImageFacts {
    /// `flags`.
    pub flags: u32,
    /// `imageType`.
    pub image_type: i32,
    /// `format`.
    pub format: i32,
    /// `mipLevels`.
    pub mip_levels: u32,
    /// `arrayLayers`.
    pub array_layers: u32,
    /// `tiling`.
    pub tiling: i32,
    /// `usage`.
    pub usage: u32,
    /// Memory planes to bind: 1, or the format's plane count if `DISJOINT`.
    pub planes: u32,
    /// `extent.depth`: the layers a 2D view of a 3D image may address.
    pub depth: u32,
}

/// A `VkImage`.
pub struct ImageObject<H: HostVulkan> {
    /// Its device's guest id.
    pub device: u64,
    /// The host image.
    pub host: H::Image,
    /// What it was created as.
    pub facts: ImageFacts,
    /// As [`BufferObject::host_memory`].
    pub host_memory: bool,
    /// Bit `n` set once plane `n` is bound (bit 0 for a non-disjoint image).
    pub bound_planes: u32,
}

impl<H: HostVulkan> ImageObject<H> {
    /// Whether every memory plane is bound.
    #[must_use]
    pub fn fully_bound(&self) -> bool {
        let all = 1u32
            .checked_shl(self.facts.planes)
            .map_or(u32::MAX, |b| b - 1);
        self.bound_planes & all == all
    }
}

/// A `VkBufferView` or a `VkImageView`: its device, the resource it views,
/// and the host handle.
pub struct ViewObject<T> {
    /// Its device's guest id.
    pub device: u64,
    /// The buffer or image it views.
    pub parent: u64,
    /// The host handle.
    pub host: T,
}

/// A table entry that belongs to a device.
pub trait Child {
    /// The device's guest id.
    fn device(&self) -> u64;
}

impl<T> Child for DeviceChild<T> {
    fn device(&self) -> u64 {
        self.device
    }
}
impl<T> Child for ViewObject<T> {
    fn device(&self) -> u64 {
        self.device
    }
}
impl<H: HostVulkan> Child for MemoryObject<H> {
    fn device(&self) -> u64 {
        self.device
    }
}
impl<H: HostVulkan> Child for BufferObject<H> {
    fn device(&self) -> u64 {
        self.device
    }
}
impl<H: HostVulkan> Child for ImageObject<H> {
    fn device(&self) -> u64 {
        self.device
    }
}

/// The table. See the module docs.
pub struct Objects<H: HostVulkan> {
    kinds: HashMap<u64, Kind>,
    instance: Option<(u64, InstanceObject<H>)>,
    physical: HashMap<u64, usize>,
    devices: HashMap<u64, DeviceObject<H>>,
    queues: HashMap<u64, QueueObject<H>>,
    pools: HashMap<u64, DeviceChild<H::CommandPool>>,
    images: HashMap<u64, ImageObject<H>>,
    memories: HashMap<u64, MemoryObject<H>>,
    buffers: HashMap<u64, BufferObject<H>>,
    buffer_views: HashMap<u64, ViewObject<H::BufferView>>,
    image_views: HashMap<u64, ViewObject<H::ImageView>>,
}

impl<H: HostVulkan> Default for Objects<H> {
    fn default() -> Self {
        Self {
            kinds: HashMap::new(),
            instance: None,
            physical: HashMap::new(),
            devices: HashMap::new(),
            queues: HashMap::new(),
            pools: HashMap::new(),
            images: HashMap::new(),
            memories: HashMap::new(),
            buffers: HashMap::new(),
            buffer_views: HashMap::new(),
            image_views: HashMap::new(),
        }
    }
}

/// `id` of `kind` in `map`, which must belong to `device`.
fn child_in<'a, T: Child>(
    map: &'a HashMap<u64, T>,
    kinds: &HashMap<u64, Kind>,
    kind: Kind,
    device: u64,
    id: u64,
) -> Result<&'a T, IdError> {
    check_kind(kinds, id, kind)?;
    let child = map.get(&id).ok_or(IdError::Unknown {
        id,
        expected: kind.name(),
    })?;
    if child.device() != device {
        return Err(IdError::WrongParent {
            child: kind.name(),
            id,
            parent: Kind::Device.name(),
            parent_id: device,
        });
    }
    Ok(child)
}

/// [`child_in`], mutably.
fn child_in_mut<'a, T: Child>(
    map: &'a mut HashMap<u64, T>,
    kinds: &HashMap<u64, Kind>,
    kind: Kind,
    device: u64,
    id: u64,
) -> Result<&'a mut T, IdError> {
    check_kind(kinds, id, kind)?;
    let child = map.get_mut(&id).ok_or(IdError::Unknown {
        id,
        expected: kind.name(),
    })?;
    if child.device() != device {
        return Err(IdError::WrongParent {
            child: kind.name(),
            id,
            parent: Kind::Device.name(),
            parent_id: device,
        });
    }
    Ok(child)
}

/// Take child `id` of `kind` out of the table, checking it belongs to
/// `device`. `Ok(None)` for id 0 — `vkDestroy*`/`vkFreeMemory` of
/// `VK_NULL_HANDLE` is a no-op in Vulkan.
fn take_in<T: Child>(
    map: &mut HashMap<u64, T>,
    kinds: &mut HashMap<u64, Kind>,
    kind: Kind,
    device: u64,
    id: u64,
) -> Result<Option<T>, IdError> {
    if id == 0 {
        return Ok(None);
    }
    child_in(map, kinds, kind, device, id)?;
    kinds.remove(&id);
    Ok(map.remove(&id))
}

fn check_kind(kinds: &HashMap<u64, Kind>, id: u64, kind: Kind) -> Result<(), IdError> {
    if id == 0 {
        return Err(IdError::Zero(kind.name()));
    }
    match kinds.get(&id) {
        None => Err(IdError::Unknown {
            id,
            expected: kind.name(),
        }),
        Some(found) if *found != kind => Err(IdError::WrongType {
            id,
            expected: kind.name(),
            found: found.name(),
        }),
        Some(_) => Ok(()),
    }
}

/// Every id in `map` that belongs to `device`.
fn children_of<T: Child>(map: &HashMap<u64, T>, device: u64) -> Vec<u64> {
    map.iter()
        .filter(|(_, child)| child.device() == device)
        .map(|(id, _)| *id)
        .collect()
}

impl<H: HostVulkan> Objects<H> {
    /// Objects of every type, the physical devices included.
    #[must_use]
    pub fn len(&self) -> usize {
        self.kinds.len()
    }

    /// Whether the table is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.kinds.is_empty()
    }

    /// Whether another object fits under [`MAX_OBJECTS_PER_CONTEXT`].
    #[must_use]
    pub fn has_room(&self) -> bool {
        self.kinds.len() < MAX_OBJECTS_PER_CONTEXT
    }

    /// Check that `id` may name a new object: nonzero and unused.
    ///
    /// # Errors
    /// [`IdError::Zero`] or [`IdError::Duplicate`].
    pub fn check_new(&self, id: u64, kind: Kind) -> Result<(), IdError> {
        if id == 0 {
            return Err(IdError::Zero(kind.name()));
        }
        match self.kinds.get(&id) {
            Some(existing) => Err(IdError::Duplicate {
                id,
                existing: existing.name(),
            }),
            None => Ok(()),
        }
    }

    /// Check that `id` names an object of `kind`.
    ///
    /// # Errors
    /// [`IdError::Zero`], [`IdError::Unknown`] or [`IdError::WrongType`].
    pub fn check(&self, id: u64, kind: Kind) -> Result<(), IdError> {
        check_kind(&self.kinds, id, kind)
    }

    // ------------------------------------------------------------ instance

    /// The context's instance, if it has one.
    #[must_use]
    pub fn instance_id(&self) -> Option<u64> {
        self.instance.as_ref().map(|(id, _)| *id)
    }

    /// The instance `id` names, which must be the context's.
    ///
    /// # Errors
    /// As [`Self::check`].
    pub fn instance(&self, id: u64) -> Result<&InstanceObject<H>, IdError> {
        self.check(id, Kind::Instance)?;
        match &self.instance {
            Some((own, instance)) if *own == id => Ok(instance),
            _ => Err(IdError::Unknown {
                id,
                expected: Kind::Instance.name(),
            }),
        }
    }

    /// The instance, mutably.
    ///
    /// # Errors
    /// As [`Self::instance`].
    pub fn instance_mut(&mut self, id: u64) -> Result<&mut InstanceObject<H>, IdError> {
        self.check(id, Kind::Instance)?;
        match &mut self.instance {
            Some((own, instance)) if *own == id => Ok(instance),
            _ => Err(IdError::Unknown {
                id,
                expected: Kind::Instance.name(),
            }),
        }
    }

    /// Bind the context's instance. The caller has checked the id and that
    /// there is none yet.
    pub fn insert_instance(&mut self, id: u64, host: H::Instance) {
        self.kinds.insert(id, Kind::Instance);
        self.instance = Some((
            id,
            InstanceObject {
                host,
                devices: None,
            },
        ));
    }

    // ---------------------------------------------------- physical devices

    /// Bind physical-device id `id` to exposed index `index`. The caller has
    /// checked the id.
    pub fn insert_physical(&mut self, id: u64, index: usize) {
        self.kinds.insert(id, Kind::PhysicalDevice);
        self.physical.insert(id, index);
    }

    /// The exposed device `id` names, and its host handle.
    ///
    /// # Errors
    /// As [`Self::check`].
    pub fn physical(&self, id: u64) -> Result<(&H::Instance, &ExposedDevice<H>), IdError> {
        self.check(id, Kind::PhysicalDevice)?;
        let unknown = IdError::Unknown {
            id,
            expected: Kind::PhysicalDevice.name(),
        };
        let index = *self.physical.get(&id).ok_or(unknown)?;
        let (_, instance) = self.instance.as_ref().ok_or(unknown)?;
        let device = instance
            .devices
            .as_ref()
            .and_then(|devices| devices.get(index))
            .ok_or(unknown)?;
        Ok((&instance.host, device))
    }

    // ------------------------------------------------------------- devices

    /// Bind device `id`. The caller has checked the id.
    pub fn insert_device(&mut self, id: u64, device: DeviceObject<H>) {
        self.kinds.insert(id, Kind::Device);
        self.devices.insert(id, device);
    }

    /// The device `id` names.
    ///
    /// # Errors
    /// As [`Self::check`].
    pub fn device(&self, id: u64) -> Result<&DeviceObject<H>, IdError> {
        self.check(id, Kind::Device)?;
        self.devices.get(&id).ok_or(IdError::Unknown {
            id,
            expected: Kind::Device.name(),
        })
    }

    /// The device `id` names, mutably.
    ///
    /// # Errors
    /// As [`Self::check`].
    pub fn device_mut(&mut self, id: u64) -> Result<&mut DeviceObject<H>, IdError> {
        self.check(id, Kind::Device)?;
        self.devices.get_mut(&id).ok_or(IdError::Unknown {
            id,
            expected: Kind::Device.name(),
        })
    }

    /// The device `id` names and what its physical device shows the guest.
    ///
    /// # Errors
    /// As [`Self::device`] and [`Self::physical`].
    pub fn device_and_guest(&self, id: u64) -> Result<(&DeviceObject<H>, &GuestDevice), IdError> {
        let device = self.device(id)?;
        let (_, exposed) = self.physical(device.physical)?;
        Ok((device, &exposed.guest))
    }

    // -------------------------------------------------------------- queues

    /// Bind queue `id`. The caller has checked the id.
    pub fn insert_queue(&mut self, id: u64, queue: QueueObject<H>) {
        self.kinds.insert(id, Kind::Queue);
        self.queues.insert(id, queue);
    }

    /// Whether any queue of the context is bound to fence timeline `ring_idx`.
    #[must_use]
    pub fn ring_idx_taken(&self, ring_idx: u32) -> bool {
        self.queues.values().any(|queue| queue.ring_idx == ring_idx)
    }

    /// The queue `id` names.
    ///
    /// # Errors
    /// As [`Self::check`].
    pub fn queue(&self, id: u64) -> Result<&QueueObject<H>, IdError> {
        self.check(id, Kind::Queue)?;
        self.queues.get(&id).ok_or(IdError::Unknown {
            id,
            expected: Kind::Queue.name(),
        })
    }

    /// Some queue of `device`, for a test that submits.
    #[cfg(test)]
    pub(crate) fn any_queue(&self, device: u64) -> Option<&QueueObject<H>> {
        self.queues.values().find(|queue| queue.device == device)
    }

    // ------------------------------------------------ pools and images

    /// Bind command pool `id`. The caller has checked the id.
    pub fn insert_pool(&mut self, id: u64, pool: DeviceChild<H::CommandPool>) {
        self.kinds.insert(id, Kind::CommandPool);
        self.pools.insert(id, pool);
    }

    /// Bind image `id`. The caller has checked the id.
    pub fn insert_image(&mut self, id: u64, image: ImageObject<H>) {
        self.kinds.insert(id, Kind::Image);
        self.images.insert(id, image);
    }

    /// The image `id` names, which must belong to `device`.
    ///
    /// # Errors
    /// As [`Self::check`], or [`IdError::WrongParent`].
    pub fn image(&self, device: u64, id: u64) -> Result<&ImageObject<H>, IdError> {
        child_in(&self.images, &self.kinds, Kind::Image, device, id)
    }

    /// The image `id` names, mutably.
    ///
    /// # Errors
    /// As [`Self::image`].
    pub fn image_mut(&mut self, device: u64, id: u64) -> Result<&mut ImageObject<H>, IdError> {
        child_in_mut(&mut self.images, &self.kinds, Kind::Image, device, id)
    }

    /// Take image `id` of `device` out of the table.
    ///
    /// # Errors
    /// As [`Self::image`].
    pub fn take_image(&mut self, device: u64, id: u64) -> Result<Option<H::Image>, IdError> {
        take_in(&mut self.images, &mut self.kinds, Kind::Image, device, id)
            .map(|image| image.map(|i| i.host))
    }

    /// Take command pool `id` of `device` out of the table.
    ///
    /// # Errors
    /// As [`Self::image`], for a pool.
    pub fn take_pool(&mut self, device: u64, id: u64) -> Result<Option<H::CommandPool>, IdError> {
        take_in(
            &mut self.pools,
            &mut self.kinds,
            Kind::CommandPool,
            device,
            id,
        )
        .map(|pool| pool.map(|p| p.host))
    }

    // ------------------------------------------------------------- memory

    /// Bind memory `id`. The caller has checked the id.
    pub fn insert_memory(&mut self, id: u64, memory: MemoryObject<H>) {
        self.kinds.insert(id, Kind::DeviceMemory);
        self.memories.insert(id, memory);
    }

    /// The memory `id` names, which must belong to `device`.
    ///
    /// # Errors
    /// As [`Self::image`], for memory.
    pub fn memory(&self, device: u64, id: u64) -> Result<&MemoryObject<H>, IdError> {
        child_in(&self.memories, &self.kinds, Kind::DeviceMemory, device, id)
    }

    /// The memory `id` names, whatever its device: how a blob finds the
    /// `VkDeviceMemory` its `blob_id` names (a blob carries no device).
    ///
    /// # Errors
    /// As [`Self::check`].
    pub fn memory_by_id_mut(&mut self, id: u64) -> Result<&mut MemoryObject<H>, IdError> {
        self.check(id, Kind::DeviceMemory)?;
        self.memories.get_mut(&id).ok_or(IdError::Unknown {
            id,
            expected: Kind::DeviceMemory.name(),
        })
    }

    /// Take memory `id` of `device` out of the table.
    ///
    /// # Errors
    /// As [`Self::memory`].
    pub fn take_memory(
        &mut self,
        device: u64,
        id: u64,
    ) -> Result<Option<MemoryObject<H>>, IdError> {
        take_in(
            &mut self.memories,
            &mut self.kinds,
            Kind::DeviceMemory,
            device,
            id,
        )
    }

    // ------------------------------------------------------------ buffers

    /// Bind buffer `id`. The caller has checked the id.
    pub fn insert_buffer(&mut self, id: u64, buffer: BufferObject<H>) {
        self.kinds.insert(id, Kind::Buffer);
        self.buffers.insert(id, buffer);
    }

    /// The buffer `id` names, which must belong to `device`.
    ///
    /// # Errors
    /// As [`Self::image`], for a buffer.
    pub fn buffer(&self, device: u64, id: u64) -> Result<&BufferObject<H>, IdError> {
        child_in(&self.buffers, &self.kinds, Kind::Buffer, device, id)
    }

    /// The buffer `id` names, mutably.
    ///
    /// # Errors
    /// As [`Self::buffer`].
    pub fn buffer_mut(&mut self, device: u64, id: u64) -> Result<&mut BufferObject<H>, IdError> {
        child_in_mut(&mut self.buffers, &self.kinds, Kind::Buffer, device, id)
    }

    /// Take buffer `id` of `device` out of the table.
    ///
    /// # Errors
    /// As [`Self::buffer`].
    pub fn take_buffer(&mut self, device: u64, id: u64) -> Result<Option<H::Buffer>, IdError> {
        take_in(&mut self.buffers, &mut self.kinds, Kind::Buffer, device, id)
            .map(|buffer| buffer.map(|b| b.host))
    }

    // -------------------------------------------------------------- views

    /// Bind buffer view `id`. The caller has checked the id.
    pub fn insert_buffer_view(&mut self, id: u64, view: ViewObject<H::BufferView>) {
        self.kinds.insert(id, Kind::BufferView);
        self.buffer_views.insert(id, view);
    }

    /// Take buffer view `id` of `device` out of the table.
    ///
    /// # Errors
    /// As [`Self::image`], for a view.
    pub fn take_buffer_view(
        &mut self,
        device: u64,
        id: u64,
    ) -> Result<Option<H::BufferView>, IdError> {
        take_in(
            &mut self.buffer_views,
            &mut self.kinds,
            Kind::BufferView,
            device,
            id,
        )
        .map(|view| view.map(|v| v.host))
    }

    /// Bind image view `id`. The caller has checked the id.
    pub fn insert_image_view(&mut self, id: u64, view: ViewObject<H::ImageView>) {
        self.kinds.insert(id, Kind::ImageView);
        self.image_views.insert(id, view);
    }

    /// Take image view `id` of `device` out of the table.
    ///
    /// # Errors
    /// As [`Self::image`], for a view.
    pub fn take_image_view(
        &mut self,
        device: u64,
        id: u64,
    ) -> Result<Option<H::ImageView>, IdError> {
        take_in(
            &mut self.image_views,
            &mut self.kinds,
            Kind::ImageView,
            device,
            id,
        )
        .map(|view| view.map(|v| v.host))
    }

    // ----------------------------------------------------------- teardown

    /// Destroy device `id` and everything under it, in dependency order:
    /// views, images, buffers, memory, pools, queues, then the device.
    /// Unknown ids are the caller's to have refused.
    pub fn destroy_device(&mut self, host: &H, id: u64) {
        let Some(device) = self.devices.remove(&id) else {
            return;
        };
        self.kinds.remove(&id);
        for child in children_of(&self.image_views, id) {
            if let Some(view) = self.image_views.remove(&child) {
                self.kinds.remove(&child);
                host.destroy_image_view(&device.host, view.host);
            }
        }
        for child in children_of(&self.buffer_views, id) {
            if let Some(view) = self.buffer_views.remove(&child) {
                self.kinds.remove(&child);
                host.destroy_buffer_view(&device.host, view.host);
            }
        }
        for child in children_of(&self.images, id) {
            if let Some(image) = self.images.remove(&child) {
                self.kinds.remove(&child);
                host.destroy_image(&device.host, image.host);
            }
        }
        for child in children_of(&self.buffers, id) {
            if let Some(buffer) = self.buffers.remove(&child) {
                self.kinds.remove(&child);
                host.destroy_buffer(&device.host, buffer.host);
            }
        }
        for child in children_of(&self.memories, id) {
            if let Some(memory) = self.memories.remove(&child) {
                self.kinds.remove(&child);
                host.free_memory(&device.host, memory.host);
                // `memory.pages` drops here: the table's hold on the pages
                // goes; a blob's, if one was made, stays.
            }
        }
        for child in children_of(&self.pools, id) {
            if let Some(pool) = self.pools.remove(&child) {
                self.kinds.remove(&child);
                host.destroy_command_pool(&device.host, pool.host);
            }
        }
        let queues: Vec<u64> = self
            .queues
            .iter()
            .filter(|(_, queue)| queue.device == id)
            .map(|(child, _)| *child)
            .collect();
        for child in queues {
            self.queues.remove(&child);
            self.kinds.remove(&child);
        }
        host.destroy_device(device.host);
    }

    /// Destroy the instance and everything under it. After this the table is
    /// empty and holds no host object.
    pub fn destroy_all(&mut self, host: &H) {
        let devices: Vec<u64> = self.devices.keys().copied().collect();
        for id in devices {
            self.destroy_device(host, id);
        }
        for id in self.physical.drain().map(|(id, _)| id) {
            self.kinds.remove(&id);
        }
        if let Some((id, instance)) = self.instance.take() {
            self.kinds.remove(&id);
            host.destroy_instance(instance.host);
        }
        // Nothing can be left, but a table that says so is the contract.
        self.kinds.clear();
        self.queues.clear();
        self.pools.clear();
        self.images.clear();
        self.memories.clear();
        self.buffers.clear();
        self.buffer_views.clear();
        self.image_views.clear();
    }
}
