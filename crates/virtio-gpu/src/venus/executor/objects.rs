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
//! * **Destruction is in dependency order** — images, then command pools,
//!   then queues, then devices, then physical devices, then the instance —
//!   whether it comes from a `vkDestroy*`, from the context going away, or
//!   from a device reset, and every host object is destroyed exactly once
//!   because destroying it *takes* it out of the table.

use std::collections::HashMap;

use super::host::HostVulkan;
use super::policy::GuestDevice;

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
}

/// A `VkQueue`.
pub struct QueueObject<H: HostVulkan> {
    /// Its device's guest id.
    pub device: u64,
    /// The host queue.
    pub host: H::Queue,
    /// The virtio-gpu fence timeline (`ring_idx`) the guest bound it to.
    pub ring_idx: u32,
}

/// A `VkCommandPool` or a `VkImage`: a host handle and its device.
pub struct DeviceChild<T> {
    /// Its device's guest id.
    pub device: u64,
    /// The host handle.
    pub host: T,
}

/// The table. See the module docs.
pub struct Objects<H: HostVulkan> {
    kinds: HashMap<u64, Kind>,
    instance: Option<(u64, InstanceObject<H>)>,
    physical: HashMap<u64, usize>,
    devices: HashMap<u64, DeviceObject<H>>,
    queues: HashMap<u64, QueueObject<H>>,
    pools: HashMap<u64, DeviceChild<H::CommandPool>>,
    images: HashMap<u64, DeviceChild<H::Image>>,
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
        }
    }
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
        if id == 0 {
            return Err(IdError::Zero(kind.name()));
        }
        match self.kinds.get(&id) {
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

    // ------------------------------------------------ pools and images

    /// Bind command pool `id`. The caller has checked the id.
    pub fn insert_pool(&mut self, id: u64, pool: DeviceChild<H::CommandPool>) {
        self.kinds.insert(id, Kind::CommandPool);
        self.pools.insert(id, pool);
    }

    /// Bind image `id`. The caller has checked the id.
    pub fn insert_image(&mut self, id: u64, image: DeviceChild<H::Image>) {
        self.kinds.insert(id, Kind::Image);
        self.images.insert(id, image);
    }

    /// The image `id` names, which must belong to `device`.
    ///
    /// # Errors
    /// As [`Self::check`], or [`IdError::WrongParent`].
    pub fn image(&self, device: u64, id: u64) -> Result<H::Image, IdError> {
        self.check(id, Kind::Image)?;
        let image = self.images.get(&id).ok_or(IdError::Unknown {
            id,
            expected: Kind::Image.name(),
        })?;
        if image.device != device {
            return Err(IdError::WrongParent {
                child: Kind::Image.name(),
                id,
                parent: Kind::Device.name(),
                parent_id: device,
            });
        }
        Ok(image.host)
    }

    /// Take child `id` of `kind` out of the table, checking it belongs to
    /// `device`. `Ok(None)` for id 0 — `vkDestroy*` of `VK_NULL_HANDLE` is a
    /// no-op in Vulkan.
    fn take_child<T>(
        map: &mut HashMap<u64, DeviceChild<T>>,
        kinds: &mut HashMap<u64, Kind>,
        kind: Kind,
        device: u64,
        id: u64,
    ) -> Result<Option<T>, IdError> {
        if id == 0 {
            return Ok(None);
        }
        match kinds.get(&id) {
            None => {
                return Err(IdError::Unknown {
                    id,
                    expected: kind.name(),
                })
            }
            Some(found) if *found != kind => {
                return Err(IdError::WrongType {
                    id,
                    expected: kind.name(),
                    found: found.name(),
                })
            }
            Some(_) => {}
        }
        match map.get(&id) {
            Some(child) if child.device != device => {
                return Err(IdError::WrongParent {
                    child: kind.name(),
                    id,
                    parent: Kind::Device.name(),
                    parent_id: device,
                })
            }
            Some(_) => {}
            None => {
                return Err(IdError::Unknown {
                    id,
                    expected: kind.name(),
                })
            }
        }
        kinds.remove(&id);
        Ok(map.remove(&id).map(|child| child.host))
    }

    /// Take image `id` of `device` out of the table.
    ///
    /// # Errors
    /// As [`Self::image`].
    pub fn take_image(&mut self, device: u64, id: u64) -> Result<Option<H::Image>, IdError> {
        Self::take_child(&mut self.images, &mut self.kinds, Kind::Image, device, id)
    }

    /// Take command pool `id` of `device` out of the table.
    ///
    /// # Errors
    /// As [`Self::image`], for a pool.
    pub fn take_pool(&mut self, device: u64, id: u64) -> Result<Option<H::CommandPool>, IdError> {
        Self::take_child(
            &mut self.pools,
            &mut self.kinds,
            Kind::CommandPool,
            device,
            id,
        )
    }

    // ----------------------------------------------------------- teardown

    /// Destroy device `id` and everything under it: images, pools, queues,
    /// then the device. Unknown ids are the caller's to have refused.
    pub fn destroy_device(&mut self, host: &H, id: u64) {
        let Some(device) = self.devices.remove(&id) else {
            return;
        };
        self.kinds.remove(&id);
        let images: Vec<u64> = self
            .images
            .iter()
            .filter(|(_, image)| image.device == id)
            .map(|(child, _)| *child)
            .collect();
        for child in images {
            if let Some(image) = self.images.remove(&child) {
                self.kinds.remove(&child);
                host.destroy_image(&device.host, image.host);
            }
        }
        let pools: Vec<u64> = self
            .pools
            .iter()
            .filter(|(_, pool)| pool.device == id)
            .map(|(child, _)| *child)
            .collect();
        for child in pools {
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
    }
}
