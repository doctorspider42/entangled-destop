//! A host Vulkan made of plain values, for the tests: devices described by
//! hand, handles that are counters, and a count of every live host object so
//! a test can prove teardown leaves none.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, Weak};

use crate::venus::capset::vk_make_api_version;
use crate::venus::protocol::*;
use crate::venus::shmem::RingPages;

use super::generated;
use super::host::{
    CallError, DeviceRequest, HostDeviceInfo, HostGroup, HostVulkan, ImageBind, InstanceRequest,
    MemoryRequest, RawHandle,
};
use super::objects::Kind;
use super::policy::{name_array, EXTERNAL_MEMORY_HOST};

/// `VK_PHYSICAL_DEVICE_TYPE_DISCRETE_GPU`.
pub const DISCRETE: i32 = 2;
/// `VK_PHYSICAL_DEVICE_TYPE_CPU`.
pub const CPU: i32 = 4;

/// What one fake device is.
#[derive(Debug, Clone)]
pub struct FakeDevice {
    /// What `describe_physical_device` answers.
    pub info: HostDeviceInfo,
}

/// An RTX-2070-shaped device: discrete, Vulkan 1.4.312, six memory types
/// (0–2 device local, 3 host visible and coherent, 4 host visible, coherent
/// and cached — both importable — and 5 the BAR: device local and host
/// visible, **not** importable), sparse features on, two queue families,
/// and — like the RTX 2070 — `VK_KHR_synchronization2` among its
/// extensions.
#[must_use]
pub fn gpu(name: &str) -> FakeDevice {
    let mut info = HostDeviceInfo::default();
    let props = &mut info.properties.properties;
    props.api_version = vk_make_api_version(0, 1, 4, 312);
    props.driver_version = 0x9100_0000;
    props.vendor_id = 0x10de;
    props.device_id = 0x1f02;
    props.device_type = DISCRETE;
    props.device_name = name_array(name);
    props.limits.max_image_dimension2d = 32768;
    // The fixed-size state stage 5b.2 bounds commands against, as an RTX
    // 2070 reports it.
    props.limits.max_bound_descriptor_sets = 32;
    props.limits.max_push_constants_size = 256;
    props.limits.max_vertex_input_bindings = 32;
    props.limits.max_vertex_input_attributes = 32;
    props.limits.max_viewports = 16;
    props.limits.max_color_attachments = 8;
    props.limits.max_descriptor_set_uniform_buffers_dynamic = 15;
    props.limits.max_descriptor_set_storage_buffers_dynamic = 16;
    info.properties.p_next = vec![
        VkPhysicalDeviceProperties2Next::VkPhysicalDeviceVulkan11Properties(
            VkPhysicalDeviceVulkan11Properties {
                subgroup_size: 32,
                max_multiview_view_count: 32,
                ..Default::default()
            },
        ),
        VkPhysicalDeviceProperties2Next::VkPhysicalDeviceVulkan12Properties(
            VkPhysicalDeviceVulkan12Properties {
                driver_id: 4,
                driver_name: name_array("NVIDIA"),
                ..Default::default()
            },
        ),
        VkPhysicalDeviceProperties2Next::VkPhysicalDeviceVulkan13Properties(
            VkPhysicalDeviceVulkan13Properties {
                max_inline_uniform_block_size: 256,
                ..Default::default()
            },
        ),
        VkPhysicalDeviceProperties2Next::VkPhysicalDeviceIDProperties(
            VkPhysicalDeviceIDProperties {
                device_uuid: [7; 16],
                ..Default::default()
            },
        ),
    ];
    info.features.features = VkPhysicalDeviceFeatures {
        robust_buffer_access: 1,
        geometry_shader: 1,
        sampler_anisotropy: 1,
        sparse_binding: 1,
        sparse_residency_buffer: 1,
        sparse_residency_image2d: 1,
        sparse_residency_aliased: 1,
        ..Default::default()
    };
    info.features.p_next = vec![
        VkPhysicalDeviceFeatures2Next::VkPhysicalDeviceVulkan11Features(
            VkPhysicalDeviceVulkan11Features {
                multiview: 1,
                shader_draw_parameters: 1,
                ..Default::default()
            },
        ),
        VkPhysicalDeviceFeatures2Next::VkPhysicalDeviceVulkan12Features(
            VkPhysicalDeviceVulkan12Features {
                timeline_semaphore: 1,
                buffer_device_address: 1,
                buffer_device_address_capture_replay: 1,
                ..Default::default()
            },
        ),
        VkPhysicalDeviceFeatures2Next::VkPhysicalDeviceVulkan13Features(
            VkPhysicalDeviceVulkan13Features {
                dynamic_rendering: 1,
                synchronization2: 1,
                ..Default::default()
            },
        ),
        VkPhysicalDeviceFeatures2Next::VkPhysicalDeviceSynchronization2Features(
            VkPhysicalDeviceSynchronization2Features {
                synchronization2: 1,
            },
        ),
        VkPhysicalDeviceFeatures2Next::VkPhysicalDeviceDynamicRenderingFeatures(
            VkPhysicalDeviceDynamicRenderingFeatures {
                dynamic_rendering: 1,
            },
        ),
        VkPhysicalDeviceFeatures2Next::VkPhysicalDeviceTimelineSemaphoreFeatures(
            VkPhysicalDeviceTimelineSemaphoreFeatures {
                timeline_semaphore: 1,
            },
        ),
    ];
    info.queue_families = vec![
        VkQueueFamilyProperties {
            queue_flags: 0xf,
            queue_count: 16,
            timestamp_valid_bits: 64,
            min_image_transfer_granularity: VkExtent3D {
                width: 1,
                height: 1,
                depth: 1,
            },
        },
        VkQueueFamilyProperties {
            queue_flags: 0xc,
            queue_count: 2,
            timestamp_valid_bits: 64,
            min_image_transfer_granularity: VkExtent3D {
                width: 1,
                height: 1,
                depth: 1,
            },
        },
    ];
    let flags = [0x1, 0x1, 0x1, 0x6, 0xe, 0x7];
    info.memory.memory_type_count = flags.len() as u32;
    for (slot, flags) in info.memory.memory_types.iter_mut().zip(flags) {
        slot.property_flags = flags;
        slot.heap_index = u32::from(flags & 0x1 == 0);
    }
    info.memory.memory_heap_count = 2;
    info.memory.memory_heaps[0] = VkMemoryHeap {
        size: 8 << 30,
        flags: 1,
    };
    info.memory.memory_heaps[1] = VkMemoryHeap {
        size: 16 << 30,
        flags: 0,
    };
    info.extensions = vec![
        VkExtensionProperties {
            extension_name: name_array("VK_KHR_swapchain"),
            spec_version: 70,
        },
        VkExtensionProperties {
            extension_name: name_array(EXTERNAL_MEMORY_HOST),
            spec_version: 1,
        },
        VkExtensionProperties {
            extension_name: name_array("VK_KHR_synchronization2"),
            spec_version: 1,
        },
    ];
    info.host_import_types = Some(0x18);
    info.host_import_alignment = 4096;
    FakeDevice { info }
}

/// A lavapipe-shaped device: a CPU implementation, which is never exposed.
#[must_use]
pub fn cpu() -> FakeDevice {
    let mut device = gpu("llvmpipe (LLVM 19.1.1, 256 bits)");
    device.info.properties.properties.device_type = CPU;
    device.info.properties.properties.vendor_id = 0x10005;
    device
}

/// One `vkAllocateMemory` that imported our pages, as the fake driver saw it.
#[derive(Debug, Clone)]
pub struct Import {
    /// The host pointer it was handed.
    pub addr: u64,
    /// `allocationSize`.
    pub len: u64,
    /// The pages themselves, without keeping them alive: `upgrade` fails
    /// once every holder is gone.
    pub pages: Weak<RingPages>,
}

/// The fake's `VkDeviceMemory`: a handle and, for an import, the pages the
/// driver would have pinned — kept alive exactly as long as the memory, as
/// the real host keeps them.
#[derive(Debug)]
pub struct FakeMemory {
    /// The handle.
    pub handle: u64,
    /// `allocationSize`.
    pub size: u64,
    /// The imported pages.
    pub pages: Option<Arc<RingPages>>,
}

#[derive(Debug, Default)]
struct Live {
    next: u64,
    by_kind: HashMap<&'static str, usize>,
    devices: Vec<(u64, DeviceRequest<usize>)>,
    images: usize,
    imports: Vec<Import>,
    allocations: Vec<(u32, u64, bool)>,
    buffer_binds: Vec<(u64, u64, u64)>,
    image_binds: Vec<(u64, u64, u64)>,
    host_memory_resources: Vec<(&'static str, bool)>,
    buffer_sizes: HashMap<u64, u64>,
    /// Every stage-5b.2 call and destroy, in order: `"vkCmdDraw"`,
    /// `"destroy VkPipeline"`, `"device idle"`.
    calls: Vec<String>,
    /// Every fence and whether it is signalled.
    fences: HashMap<u64, bool>,
    /// Fences of submissions the fake GPU has not finished.
    held: Vec<u64>,
    /// Command buffers and descriptor sets, and the pool each came from.
    pool_children: HashMap<u64, u64>,
    /// Every translated `vkQueueSubmit`'s command buffers, as host handles.
    submitted: Vec<Vec<u64>>,
    /// What each pool child is, for the live counts.
    child_kinds: HashMap<u64, &'static str>,
    /// Every translated submit's semaphores, as host handles: `(queue,
    /// waits, signals)` per batch (stage 5b.3).
    semaphore_ops: Vec<(u64, Vec<u64>, Vec<u64>)>,
    /// Timeline semaphores and their values.
    timelines: HashMap<u64, u64>,
    /// Every `vkCreateSemaphore`'s `VkExportSemaphoreCreateInfo` handle
    /// types, as the host saw it (`None`: no such structure).
    semaphore_exports: Vec<Option<u32>>,
    /// Fences of submits to a queue in [`FakeVulkan::stuck_queues`]: they
    /// signal only once the queue is released.
    stuck_fences: HashMap<u64, u64>,
}

/// The fake host. See the module docs.
pub struct FakeVulkan {
    /// What `vkEnumerateInstanceVersion` answers.
    pub version: u32,
    /// The physical devices, in enumeration order.
    pub devices: Vec<FakeDevice>,
    /// What `vkGetMemoryHostPointerPropertiesEXT` answers for any pages.
    pub host_pointer_bits: u32,
    live: Mutex<Live>,
    /// The fake GPU: finish every submission at once (`false`), or only when
    /// something waits for it (`true`).
    pub hold: AtomicBool,
    /// A GPU that never finishes: every wait times out.
    pub stuck: AtomicBool,
    /// A lost device: every submit and every wait answers
    /// `VK_ERROR_DEVICE_LOST`.
    pub lost: AtomicBool,
    /// Host queues whose work never finishes until
    /// [`FakeVulkan::release_queue`] (stage 5b.3).
    pub stuck_queues: Mutex<std::collections::HashSet<u64>>,
    /// Called with every stage-5b.2 command's name as it reaches the host.
    #[allow(clippy::type_complexity)]
    pub on_call: Mutex<Option<Box<dyn FnMut(&str) + Send>>>,
}

impl std::fmt::Debug for FakeVulkan {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FakeVulkan")
            .field("version", &self.version)
            .field("devices", &self.devices.len())
            .finish_non_exhaustive()
    }
}

impl FakeVulkan {
    /// A host with `devices`, loader 1.4.309.
    #[must_use]
    pub fn new(devices: Vec<FakeDevice>) -> Self {
        Self {
            version: vk_make_api_version(0, 1, 4, 309),
            devices,
            host_pointer_bits: 0x18,
            live: Mutex::new(Live::default()),
            hold: AtomicBool::new(false),
            stuck: AtomicBool::new(false),
            lost: AtomicBool::new(false),
            stuck_queues: Mutex::new(std::collections::HashSet::new()),
            on_call: Mutex::new(None),
        }
    }

    /// The RTX-2070-shaped GPU and a CPU device beside it.
    #[must_use]
    pub fn standard() -> Self {
        Self::new(vec![cpu(), gpu("NVIDIA GeForce RTX 2070")])
    }

    fn with<T>(&self, f: impl FnOnce(&mut Live) -> T) -> T {
        f(&mut self
            .live
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner))
    }

    fn create(&self, kind: &'static str) -> u64 {
        self.with(|live| {
            live.next += 1;
            *live.by_kind.entry(kind).or_default() += 1;
            live.next
        })
    }

    fn destroy(&self, kind: &'static str) {
        self.with(|live| {
            let count = live.by_kind.entry(kind).or_default();
            assert!(*count > 0, "a {kind} destroyed that was never created");
            *count -= 1;
        });
    }

    /// Host objects alive right now, of every kind.
    #[must_use]
    pub fn live_objects(&self) -> usize {
        self.with(|live| live.by_kind.values().sum())
    }

    /// Host objects of `kind` alive right now.
    #[must_use]
    pub fn live(&self, kind: &'static str) -> usize {
        self.with(|live| live.by_kind.get(kind).copied().unwrap_or(0))
    }

    /// Every `vkCreateDevice` request that reached the host.
    #[must_use]
    pub fn device_requests(&self) -> Vec<DeviceRequest<usize>> {
        self.with(|live| live.devices.iter().map(|(_, r)| r.clone()).collect())
    }

    /// How many `vkCreateImage`s reached the host.
    #[must_use]
    pub fn image_requests(&self) -> usize {
        self.with(|live| live.images)
    }

    /// Every import of our pages the fake driver saw, oldest first.
    #[must_use]
    pub fn imports(&self) -> Vec<Import> {
        self.with(|live| live.imports.clone())
    }

    /// Every `vkAllocateMemory` that reached the host: `(type, size,
    /// imported)`.
    #[must_use]
    pub fn allocations(&self) -> Vec<(u32, u64, bool)> {
        self.with(|live| live.allocations.clone())
    }

    /// Every buffer bind that reached the host: `(buffer, memory, offset)`.
    #[must_use]
    pub fn buffer_binds(&self) -> Vec<(u64, u64, u64)> {
        self.with(|live| live.buffer_binds.clone())
    }

    /// Every image bind that reached the host: `(image, memory, offset)`.
    #[must_use]
    pub fn image_binds(&self) -> Vec<(u64, u64, u64)> {
        self.with(|live| live.image_binds.clone())
    }

    /// Every buffer and image created, and whether it was created able to
    /// take host allocations.
    #[must_use]
    pub fn host_memory_resources(&self) -> Vec<(&'static str, bool)> {
        self.with(|live| live.host_memory_resources.clone())
    }

    /// Every stage-5b.2 command, destroy and idle wait that reached the
    /// host, in order.
    #[must_use]
    pub fn calls(&self) -> Vec<String> {
        self.with(|live| live.calls.clone())
    }

    /// How many times `name` reached the host.
    #[must_use]
    pub fn called(&self, name: &str) -> usize {
        self.with(|live| live.calls.iter().filter(|c| *c == name).count())
    }

    /// Every submit's command buffers, as host handles.
    #[must_use]
    pub fn submitted(&self) -> Vec<Vec<u64>> {
        self.with(|live| live.submitted.clone())
    }

    /// Submissions the fake GPU has not finished.
    #[must_use]
    pub fn held(&self) -> usize {
        self.with(|live| live.held.len())
    }

    /// Every submit batch's semaphores, as host handles: `(queue, waits,
    /// signals)`.
    #[must_use]
    pub fn semaphore_ops(&self) -> Vec<(u64, Vec<u64>, Vec<u64>)> {
        self.with(|live| live.semaphore_ops.clone())
    }

    /// Every `vkCreateSemaphore`'s export handle types as the host saw them.
    #[must_use]
    pub fn semaphore_exports(&self) -> Vec<Option<u32>> {
        self.with(|live| live.semaphore_exports.clone())
    }

    /// A timeline semaphore's value on the fake GPU.
    #[must_use]
    pub fn timeline_value(&self, semaphore: u64) -> Option<u64> {
        self.with(|live| live.timelines.get(&semaphore).copied())
    }

    /// Make host queue `queue`'s work never finish until released.
    pub fn stick_queue(&self, queue: u64) {
        self.stuck_queues
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(queue);
    }

    /// Let host queue `queue`'s work finish.
    pub fn release_queue(&self, queue: u64) {
        self.stuck_queues
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&queue);
        self.with(|live| live.stuck_fences.retain(|_, q| *q != queue));
    }

    fn queue_stuck(&self, queue: u64) -> bool {
        self.stuck_queues
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .contains(&queue)
    }

    fn forget_children(live: &mut Live, pool: u64) {
        let children: Vec<u64> = live
            .pool_children
            .iter()
            .filter(|(_, p)| **p == pool)
            .map(|(c, _)| *c)
            .collect();
        for child in children {
            live.pool_children.remove(&child);
            live.release_child(child);
        }
    }

    /// The whole fake GPU for one stage-5b.2 command: what `call` answers.
    fn serve(&self, command: &mut Command<'_>) -> i32 {
        let lost = self.lost.load(Ordering::SeqCst);
        let hold = self.hold.load(Ordering::SeqCst);
        let stuck = self.stuck.load(Ordering::SeqCst);
        let lost_ret = VK_ERROR_DEVICE_LOST;
        match command {
            Command::QueueSubmit(a) => {
                if lost {
                    return lost_ret;
                }
                let cbs: Vec<u64> = a
                    .p_submits
                    .iter()
                    .flatten()
                    .flat_map(|s| s.p_command_buffers.iter().flatten().map(|c| c.0))
                    .collect();
                for s in a.p_submits.iter().flatten() {
                    let waits: Vec<u64> =
                        s.p_wait_semaphores.iter().flatten().map(|x| x.0).collect();
                    let signals: Vec<u64> = s
                        .p_signal_semaphores
                        .iter()
                        .flatten()
                        .map(|x| x.0)
                        .collect();
                    let values: Vec<u64> = s
                        .p_next
                        .iter()
                        .find_map(|l| match l {
                            VkSubmitInfoNext::VkTimelineSemaphoreSubmitInfo(t) => {
                                t.p_signal_semaphore_values.clone()
                            }
                            _ => None,
                        })
                        .unwrap_or_default();
                    self.with(|live| {
                        for (sem, value) in signals.iter().zip(&values) {
                            if let Some(v) = live.timelines.get_mut(sem) {
                                *v = (*v).max(*value);
                            }
                        }
                        live.semaphore_ops.push((a.queue.0, waits, signals));
                    });
                }
                let stuck = self.queue_stuck(a.queue.0);
                self.submit(cbs, a.fence.0, hold);
                if stuck && a.fence.0 != 0 {
                    self.with(|live| live.stuck_fences.insert(a.fence.0, a.queue.0));
                }
                VK_SUCCESS
            }
            Command::CreateSemaphore(a) => {
                let exported = a.p_create_info.as_ref().and_then(|i| {
                    i.p_next.iter().find_map(|l| match l {
                        VkSemaphoreCreateInfoNext::VkExportSemaphoreCreateInfo(e) => {
                            Some(e.handle_types)
                        }
                        _ => None,
                    })
                });
                self.with(|live| live.semaphore_exports.push(exported));
                VK_SUCCESS
            }
            Command::GetSemaphoreCounterValue(a) => {
                a.p_value = Some(self.timeline_value(a.semaphore.0).unwrap_or(0));
                VK_SUCCESS
            }
            Command::SignalSemaphore(a) => {
                if let Some(info) = &a.p_signal_info {
                    self.with(|live| {
                        if let Some(v) = live.timelines.get_mut(&info.semaphore.0) {
                            *v = (*v).max(info.value);
                        }
                    });
                }
                VK_SUCCESS
            }
            Command::WaitSemaphores(a) => {
                if lost {
                    return lost_ret;
                }
                let Some(info) = &a.p_wait_info else {
                    return VK_ERROR_UNKNOWN;
                };
                let reached: Vec<bool> = self.with(|live| {
                    info.p_semaphores
                        .iter()
                        .flatten()
                        .zip(info.p_values.iter().flatten())
                        .map(|(s, v)| live.timelines.get(&s.0).copied().unwrap_or(0) >= *v)
                        .collect()
                });
                let any = info.flags & 1 != 0;
                let done = if any {
                    reached.iter().any(|r| *r)
                } else {
                    reached.iter().all(|r| *r)
                };
                if done && !stuck {
                    VK_SUCCESS
                } else {
                    std::thread::sleep(std::time::Duration::from_nanos(a.timeout.min(2_000_000)));
                    VK_TIMEOUT
                }
            }
            Command::QueueSubmit2(a) => {
                if lost {
                    return lost_ret;
                }
                let cbs: Vec<u64> = a
                    .p_submits
                    .iter()
                    .flatten()
                    .flat_map(|s| {
                        s.p_command_buffer_infos
                            .iter()
                            .flatten()
                            .map(|c| c.command_buffer.0)
                    })
                    .collect();
                for s in a.p_submits.iter().flatten() {
                    let waits: Vec<u64> = s
                        .p_wait_semaphore_infos
                        .iter()
                        .flatten()
                        .map(|x| x.semaphore.0)
                        .collect();
                    let signals: Vec<(u64, u64)> = s
                        .p_signal_semaphore_infos
                        .iter()
                        .flatten()
                        .map(|x| (x.semaphore.0, x.value))
                        .collect();
                    self.with(|live| {
                        for (sem, value) in &signals {
                            if let Some(v) = live.timelines.get_mut(sem) {
                                *v = (*v).max(*value);
                            }
                        }
                        live.semaphore_ops.push((
                            a.queue.0,
                            waits,
                            signals.iter().map(|(s, _)| *s).collect(),
                        ));
                    });
                }
                let stuck = self.queue_stuck(a.queue.0);
                self.submit(cbs, a.fence.0, hold);
                if stuck && a.fence.0 != 0 {
                    self.with(|live| live.stuck_fences.insert(a.fence.0, a.queue.0));
                }
                VK_SUCCESS
            }
            Command::GetFenceStatus(a) => {
                if lost {
                    return lost_ret;
                }
                let signalled = self.with(|live| live.fences.get(&a.fence.0).copied());
                if signalled == Some(true) {
                    VK_SUCCESS
                } else {
                    VK_NOT_READY
                }
            }
            Command::WaitForFences(a) => {
                if lost {
                    return lost_ret;
                }
                let fences: Vec<u64> = a.p_fences.iter().flatten().map(|f| f.0).collect();
                let on_stuck_queue =
                    self.with(|live| fences.iter().any(|f| live.stuck_fences.contains_key(f)));
                if stuck || on_stuck_queue {
                    std::thread::sleep(std::time::Duration::from_nanos(a.timeout.min(2_000_000)));
                    return VK_TIMEOUT;
                }
                self.with(|live| {
                    if a.timeout > 0 {
                        // Waiting is the GPU getting there.
                        for fence in &fences {
                            live.finish(*fence);
                        }
                    }
                    let done = |f: &u64| live.fences.get(f).copied() == Some(true);
                    let ok = if a.wait_all != 0 {
                        fences.iter().all(done)
                    } else {
                        fences.iter().any(done)
                    };
                    if ok {
                        VK_SUCCESS
                    } else {
                        VK_TIMEOUT
                    }
                })
            }
            Command::ResetFences(a) => {
                self.with(|live| {
                    for fence in a.p_fences.iter().flatten() {
                        live.fences.insert(fence.0, false);
                    }
                });
                VK_SUCCESS
            }
            Command::QueueWaitIdle(_) | Command::DeviceWaitIdle(_) => {
                if lost {
                    return lost_ret;
                }
                if stuck {
                    return VK_TIMEOUT;
                }
                self.with(Live::finish_all);
                VK_SUCCESS
            }
            Command::FreeCommandBuffers(a) => {
                self.with(|live| {
                    for cb in a.p_command_buffers.iter().flatten() {
                        if live.pool_children.remove(&cb.0).is_some() {
                            live.release_child(cb.0);
                        }
                    }
                });
                VK_SUCCESS
            }
            Command::FreeDescriptorSets(a) => {
                self.with(|live| {
                    for set in a.p_descriptor_sets.iter().flatten() {
                        if live.pool_children.remove(&set.0).is_some() {
                            live.release_child(set.0);
                        }
                    }
                });
                VK_SUCCESS
            }
            Command::ResetDescriptorPool(a) => {
                let pool = a.descriptor_pool.0;
                self.with(|live| Self::forget_children(live, pool));
                VK_SUCCESS
            }
            _ => VK_SUCCESS,
        }
    }

    fn submit(&self, cbs: Vec<u64>, fence: u64, hold: bool) {
        self.with(|live| {
            live.submitted.push(cbs);
            if fence != 0 {
                live.fences.insert(fence, !hold);
                if hold {
                    live.held.push(fence);
                }
            }
        });
    }
}

impl Live {
    fn finish(&mut self, fence: u64) {
        // A fence covers everything submitted before it.
        if let Some(at) = self.held.iter().position(|f| *f == fence) {
            for f in self.held.drain(..=at) {
                self.fences.insert(f, true);
            }
        }
    }

    fn finish_all(&mut self) {
        for f in self.held.drain(..) {
            self.fences.insert(f, true);
        }
    }

    /// A command buffer or descriptor set its pool gave back.
    fn release_child(&mut self, child: u64) {
        if let Some(kind) = self.child_kinds.remove(&child) {
            if let Some(count) = self.by_kind.get_mut(kind) {
                *count = count.saturating_sub(1);
            }
        }
    }
}

/// The fake's buffer requirements: 256-byte aligned, every type.
fn buffer_requirements(size: u64, out: &mut VkMemoryRequirements2) {
    out.memory_requirements = VkMemoryRequirements {
        size: size.next_multiple_of(256),
        alignment: 256,
        memory_type_bits: 0x3f,
    };
    for link in &mut out.p_next {
        let VkMemoryRequirements2Next::VkMemoryDedicatedRequirements(d) = link;
        d.prefers_dedicated_allocation = 0;
        d.requires_dedicated_allocation = 0;
    }
}

/// The fake's image requirements: 1 MiB, 1 KiB aligned, every type.
fn image_requirements(out: &mut VkMemoryRequirements2) {
    out.memory_requirements = VkMemoryRequirements {
        size: 1 << 20,
        alignment: 1024,
        memory_type_bits: 0x3f,
    };
    for link in &mut out.p_next {
        let VkMemoryRequirements2Next::VkMemoryDedicatedRequirements(d) = link;
        d.prefers_dedicated_allocation = 1;
        d.requires_dedicated_allocation = 0;
    }
}

impl HostVulkan for FakeVulkan {
    type Instance = u64;
    type PhysicalDevice = usize;
    type Device = u64;
    type Queue = u64;
    type CommandPool = u64;
    type Image = u64;
    type Memory = FakeMemory;
    type Buffer = u64;
    type BufferView = u64;
    type ImageView = u64;

    fn instance_version(&self) -> Result<u32, VkResult> {
        Ok(self.version)
    }

    fn create_instance(&self, _request: &InstanceRequest) -> Result<u64, VkResult> {
        Ok(self.create("instance"))
    }

    fn destroy_instance(&self, _instance: u64) {
        self.destroy("instance");
    }

    fn enumerate_physical_devices(&self, _instance: &u64) -> Result<Vec<usize>, VkResult> {
        Ok((0..self.devices.len()).collect())
    }

    fn describe_physical_device(&self, _instance: &u64, device: usize) -> HostDeviceInfo {
        self.devices
            .get(device)
            .map(|d| d.info.clone())
            .unwrap_or_default()
    }

    fn physical_device_groups(&self, _instance: &u64) -> Result<Vec<HostGroup<usize>>, VkResult> {
        Ok((0..self.devices.len())
            .map(|i| HostGroup {
                members: vec![i],
                subset_allocation: false,
            })
            .collect())
    }

    fn format_properties(
        &self,
        _instance: &u64,
        _device: usize,
        format: VkFormat,
        out: &mut VkFormatProperties2,
    ) {
        let features = if format == 0 { 0 } else { 0x1_d401 };
        out.format_properties = VkFormatProperties {
            linear_tiling_features: features & 0xff,
            optimal_tiling_features: features,
            buffer_features: 0x58,
        };
        for link in &mut out.p_next {
            if let VkFormatProperties2Next::VkFormatProperties3(p) = link {
                p.linear_tiling_features = u64::from(features & 0xff);
                p.optimal_tiling_features = u64::from(features);
                p.buffer_features = 0x58;
            }
        }
    }

    fn image_format_properties(
        &self,
        _instance: &u64,
        _device: usize,
        info: &VkPhysicalDeviceImageFormatInfo2,
        out: &mut VkImageFormatProperties2,
    ) -> VkResult {
        if info.format == 0 {
            return VK_ERROR_FORMAT_NOT_SUPPORTED;
        }
        out.image_format_properties = VkImageFormatProperties {
            max_extent: VkExtent3D {
                width: 16384,
                height: 16384,
                depth: if info.type_ == 2 { 2048 } else { 1 },
            },
            max_mip_levels: 15,
            max_array_layers: 2048,
            sample_counts: 0xf,
            max_resource_size: 1 << 31,
        };
        VK_SUCCESS
    }

    fn external_semaphore_properties(
        &self,
        _instance: &u64,
        _device: usize,
        handle_type: u32,
        _timeline: bool,
    ) -> VkExternalSemaphoreProperties {
        // As a Windows driver: its own opaque handles, and no sync file.
        if handle_type == 0x2 {
            VkExternalSemaphoreProperties {
                export_from_imported_handle_types: 0x2,
                compatible_handle_types: 0x2,
                external_semaphore_features: 0x3,
            }
        } else {
            VkExternalSemaphoreProperties::default()
        }
    }

    fn create_device(
        &self,
        _instance: &u64,
        _device: usize,
        request: &DeviceRequest<usize>,
    ) -> Result<u64, VkResult> {
        let handle = self.create("device");
        self.with(|live| live.devices.push((handle, request.clone())));
        Ok(handle)
    }

    fn destroy_device(&self, _device: u64) {
        self.destroy("device");
    }

    fn device_queue(&self, _device: &u64, _flags: u32, family: u32, index: u32) -> u64 {
        (u64::from(family) << 32) | u64::from(index)
    }

    fn create_command_pool(
        &self,
        _device: &u64,
        _info: &VkCommandPoolCreateInfo,
    ) -> Result<u64, VkResult> {
        Ok(self.create("command pool"))
    }

    fn destroy_command_pool(&self, _device: &u64, pool: u64) {
        self.with(|live| Self::forget_children(live, pool));
        self.destroy("command pool");
    }

    fn image_accepts_host_memory(&self, _device: &u64, info: &VkImageCreateInfo) -> bool {
        // As a driver that imports host memory for linear images only.
        info.tiling == 1
    }

    fn create_image(
        &self,
        _device: &u64,
        _info: &VkImageCreateInfo,
        host_memory: bool,
    ) -> Result<u64, VkResult> {
        self.with(|live| {
            live.images += 1;
            live.host_memory_resources.push(("image", host_memory));
        });
        Ok(self.create("image"))
    }

    fn destroy_image(&self, _device: &u64, _image: u64) {
        self.destroy("image");
    }

    fn image_memory_requirements(
        &self,
        _device: &u64,
        _image: u64,
        _plane: Option<VkImageAspectFlagBits>,
        out: &mut VkMemoryRequirements2,
    ) {
        image_requirements(out);
    }

    fn device_image_memory_requirements(
        &self,
        _device: &u64,
        _info: &VkImageCreateInfo,
        _host_memory: bool,
        _plane: Option<VkImageAspectFlagBits>,
        out: &mut VkMemoryRequirements2,
    ) {
        image_requirements(out);
    }

    fn image_subresource_layout(
        &self,
        _device: &u64,
        _image: u64,
        subresource: &VkImageSubresource,
    ) -> VkSubresourceLayout {
        VkSubresourceLayout {
            offset: u64::from(subresource.mip_level) * 0x1000,
            size: 0x1000,
            row_pitch: 256,
            array_pitch: 0x1000,
            depth_pitch: 0x1000,
        }
    }

    fn bind_image_memory(
        &self,
        _device: &u64,
        binds: &[ImageBind<'_, u64, FakeMemory>],
    ) -> VkResult {
        self.with(|live| {
            for bind in binds {
                live.image_binds
                    .push((bind.image, bind.memory.handle, bind.offset));
            }
        });
        VK_SUCCESS
    }

    fn create_image_view(
        &self,
        _device: &u64,
        _image: u64,
        _info: &VkImageViewCreateInfo,
    ) -> Result<u64, VkResult> {
        Ok(self.create("image view"))
    }

    fn destroy_image_view(&self, _device: &u64, _view: u64) {
        self.destroy("image view");
    }

    fn host_pointer_types(&self, _device: &u64, _pages: &RingPages) -> Result<u32, VkResult> {
        Ok(self.host_pointer_bits)
    }

    fn allocate_memory(
        &self,
        _device: &u64,
        request: &MemoryRequest<u64, u64>,
    ) -> Result<FakeMemory, VkResult> {
        let handle = self.create("memory");
        self.with(|live| {
            live.allocations
                .push((request.type_index, request.size, request.import.is_some()));
            if let Some(pages) = &request.import {
                live.imports.push(Import {
                    addr: pages.host_addr(),
                    len: request.size,
                    pages: Arc::downgrade(pages),
                });
            }
        });
        Ok(FakeMemory {
            handle,
            size: request.size,
            pages: request.import.clone(),
        })
    }

    fn free_memory(&self, _device: &u64, memory: FakeMemory) {
        // As the real host: the import's pages are held exactly as long as
        // the memory, and are all of it.
        assert!(memory
            .pages
            .as_ref()
            .is_none_or(|pages| pages.mapped_len() == memory.size));
        drop(memory);
        self.destroy("memory");
    }

    fn memory_commitment(&self, _device: &u64, memory: &FakeMemory) -> u64 {
        memory.size / 2
    }

    fn buffer_accepts_host_memory(&self, _device: &u64, _flags: u32, _usage: u32) -> bool {
        true
    }

    fn create_buffer(
        &self,
        _device: &u64,
        info: &VkBufferCreateInfo,
        host_memory: bool,
    ) -> Result<u64, VkResult> {
        let handle = self.create("buffer");
        self.with(|live| {
            live.host_memory_resources.push(("buffer", host_memory));
            live.buffer_sizes.insert(handle, info.size);
        });
        Ok(handle)
    }

    fn destroy_buffer(&self, _device: &u64, _buffer: u64) {
        self.destroy("buffer");
    }

    fn buffer_memory_requirements(
        &self,
        _device: &u64,
        buffer: u64,
        out: &mut VkMemoryRequirements2,
    ) {
        let size = self.with(|live| live.buffer_sizes.get(&buffer).copied().unwrap_or(0));
        buffer_requirements(size, out);
    }

    fn device_buffer_memory_requirements(
        &self,
        _device: &u64,
        info: &VkBufferCreateInfo,
        _host_memory: bool,
        out: &mut VkMemoryRequirements2,
    ) {
        buffer_requirements(info.size, out);
    }

    fn bind_buffer_memory(&self, _device: &u64, binds: &[(u64, &FakeMemory, u64)]) -> VkResult {
        self.with(|live| {
            for (buffer, memory, offset) in binds {
                live.buffer_binds.push((*buffer, memory.handle, *offset));
            }
        });
        VK_SUCCESS
    }

    fn buffer_device_address(&self, _device: &u64, buffer: u64) -> u64 {
        0x1_0000_0000 + (buffer << 16)
    }

    fn create_buffer_view(
        &self,
        _device: &u64,
        _buffer: u64,
        _info: &VkBufferViewCreateInfo,
    ) -> Result<u64, VkResult> {
        Ok(self.create("buffer view"))
    }

    fn destroy_buffer_view(&self, _device: &u64, _view: u64) {
        self.destroy("buffer view");
    }

    fn call(&self, _device: &u64, command: &mut Command<'_>) -> Result<(), CallError> {
        let name = command.name();
        if let Some(hook) = self
            .on_call
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_mut()
        {
            hook(name);
        }
        self.with(|live| live.calls.push(name.to_owned()));
        // Pool children remember their pool (a translated handle).
        let pool = match command {
            Command::AllocateCommandBuffers(a) => {
                a.p_allocate_info.as_ref().map(|i| i.command_pool.0)
            }
            Command::AllocateDescriptorSets(a) => {
                a.p_allocate_info.as_ref().map(|i| i.descriptor_pool.0)
            }
            _ => None,
        };
        let signalled = match command {
            Command::CreateFence(a) => a.p_create_info.as_ref().map(|i| i.flags & 1 != 0),
            _ => None,
        };
        let timeline = match command {
            Command::CreateSemaphore(a) => a.p_create_info.as_ref().and_then(|i| {
                i.p_next.iter().find_map(|l| match l {
                    VkSemaphoreCreateInfoNext::VkSemaphoreTypeCreateInfo(t)
                        if t.semaphore_type == 1 =>
                    {
                        Some(t.initial_value)
                    }
                    _ => None,
                })
            }),
            _ => None,
        };
        let ret = self.serve(command);
        generated::set_result(command, ret);
        if ret >= 0 {
            if let Some((kind, slots)) = generated::output_handles(command) {
                for slot in slots {
                    let handle = self.create(kind.name());
                    *slot = handle;
                    self.with(|live| {
                        if let Some(pool) = pool {
                            live.pool_children.insert(handle, pool);
                            live.child_kinds.insert(handle, kind.name());
                        }
                        if let Some(signalled) = signalled {
                            live.fences.insert(handle, signalled);
                        }
                        if let Some(value) = timeline {
                            live.timelines.insert(handle, value);
                        }
                    });
                }
            }
        }
        Ok(())
    }

    fn destroy_object(&self, _device: &u64, kind: Kind, raw: u64) {
        self.with(|live| {
            live.calls.push(format!("destroy {}", kind.name()));
            if kind == Kind::DescriptorPool {
                Self::forget_children(live, raw);
            }
            if kind == Kind::Fence {
                live.fences.remove(&raw);
            }
        });
        self.destroy(kind.name());
    }

    fn device_wait_idle(&self, _device: &u64) -> VkResult {
        self.with(|live| {
            live.calls.push("device idle".to_owned());
            live.finish_all();
        });
        if self.lost.load(Ordering::SeqCst) {
            VK_ERROR_DEVICE_LOST
        } else {
            VK_SUCCESS
        }
    }
}

impl RawHandle for FakeMemory {
    fn raw(&self) -> u64 {
        self.handle
    }
}
