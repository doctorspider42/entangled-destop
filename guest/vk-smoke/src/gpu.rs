//! Instance, physical device and device bring-up (checks 1 and 2), plus the
//! small helpers every later check shares: errors that name the VkResult,
//! memory-type choice, one-shot submits, and a cleanup scope.

use std::cell::Cell;
use std::ffi::{c_char, CStr};
use std::time::{Duration, Instant};

use ash::vk;

/// `VK_ERROR_DEVICE_LOST`, not "device lost".
pub fn result_name(r: vk::Result) -> String {
    format!("VK_{r:?}")
}

pub trait VkExt<T> {
    /// Turns a VkResult error into `"<call>: VK_<NAME>"`.
    fn vk(self, call: &str) -> Result<T, String>;
}

impl<T> VkExt<T> for Result<T, vk::Result> {
    fn vk(self, call: &str) -> Result<T, String> {
        self.map_err(|e| format!("{call}: {}", result_name(e)))
    }
}

pub fn version_string(v: u32) -> String {
    format!(
        "{}.{}.{}",
        vk::api_version_major(v),
        vk::api_version_minor(v),
        vk::api_version_patch(v)
    )
}

fn minor(v: u32) -> (u32, u32) {
    (vk::api_version_major(v), vk::api_version_minor(v))
}

fn vendor_name(id: u32) -> &'static str {
    match id {
        0x1002 => "AMD",
        0x1010 => "ImgTec",
        0x10de => "NVIDIA",
        0x13b5 => "ARM",
        0x14e4 => "Broadcom",
        0x5143 => "Qualcomm",
        0x8086 => "Intel",
        0x1af4 => "virtio",
        0x10005 => "Mesa",
        0x1414 => "Microsoft",
        _ => "unknown",
    }
}

fn cstr(chars: &[c_char]) -> String {
    // The spec guarantees these fixed arrays are NUL-terminated; if a broken
    // driver does not, stop at the end of the array rather than read past it.
    let bytes: Vec<u8> = chars
        .iter()
        .take_while(|&&c| c != 0)
        .map(|&c| c as u8)
        .collect();
    String::from_utf8_lossy(&bytes).into_owned()
}

pub struct Opts {
    pub device_index: Option<usize>,
    pub allow_cpu: bool,
    pub timeout: Duration,
    pub checks: Option<Vec<u32>>,
    /// Highest (major, minor) to request, below the default 1.3: makes a 1.3
    /// device take the 1.2/1.1 paths (KHR extensions) a Venus guest takes.
    pub api_cap: Option<(u32, u32)>,
}

/// What check 1 hands to check 2.
pub struct Picked {
    pub entry: ash::Entry,
    pub instance: ash::Instance,
    pub phys: vk::PhysicalDevice,
    pub props: vk::PhysicalDeviceProperties,
    /// min(instance apiVersion, device apiVersion): what the device may use.
    pub api: u32,
    pub exts: Vec<String>,
}

struct DevInfo {
    phys: vk::PhysicalDevice,
    props: vk::PhysicalDeviceProperties,
    driver: String,
    exts: Vec<String>,
}

fn type_name(t: vk::PhysicalDeviceType) -> &'static str {
    match t {
        vk::PhysicalDeviceType::DISCRETE_GPU => "discrete",
        vk::PhysicalDeviceType::INTEGRATED_GPU => "integrated",
        vk::PhysicalDeviceType::VIRTUAL_GPU => "virtual",
        vk::PhysicalDeviceType::CPU => "cpu",
        _ => "other",
    }
}

fn type_rank(t: vk::PhysicalDeviceType) -> u32 {
    match t {
        vk::PhysicalDeviceType::DISCRETE_GPU => 0,
        vk::PhysicalDeviceType::INTEGRATED_GPU => 1,
        vk::PhysicalDeviceType::VIRTUAL_GPU => 2,
        vk::PhysicalDeviceType::OTHER => 3,
        _ => 4,
    }
}

/// Check 1: load the loader, create an instance, list every physical device
/// and select one.
pub fn check_instance(opts: &Opts) -> Result<(String, Picked), String> {
    // SAFETY: loading the system Vulkan loader runs its initialisers; that is
    // the documented way to use it and nothing else is loaded concurrently.
    let entry =
        unsafe { ash::Entry::load() }.map_err(|e| format!("cannot load the Vulkan loader: {e}"))?;
    // SAFETY: a global command with no arguments, on a loaded entry.
    let loader_api = unsafe { entry.try_enumerate_instance_version() }
        .vk("vkEnumerateInstanceVersion")?
        .unwrap_or(vk::API_VERSION_1_0);
    // Ask for at most 1.3 (what the checks use), the loader's version, and
    // --api-cap, whichever is lowest.
    let want = [minor(loader_api), (1, 3), opts.api_cap.unwrap_or((1, 3))]
        .into_iter()
        .min()
        .unwrap_or((1, 0));
    let instance_api = vk::make_api_version(0, want.0, want.1, 0);
    let app = vk::ApplicationInfo::default()
        .application_name(c"vk-smoke")
        .application_version(1)
        .engine_name(c"entangled-vk-smoke")
        .api_version(instance_api);
    let info = vk::InstanceCreateInfo::default().application_info(&app);
    // SAFETY: `info` and everything it points to live across the call.
    let instance = unsafe { entry.create_instance(&info, None) }.vk("vkCreateInstance")?;
    println!(
        "# loader apiVersion {}, instance created with {}",
        version_string(loader_api),
        version_string(instance_api)
    );

    match pick(&instance, instance_api, opts) {
        Ok((detail, dev)) => {
            let api = if minor(dev.props.api_version) < minor(instance_api) {
                dev.props.api_version
            } else {
                instance_api
            };
            let picked = Picked {
                entry,
                instance,
                phys: dev.phys,
                props: dev.props,
                api,
                exts: dev.exts,
            };
            Ok((detail, picked))
        }
        Err(e) => {
            // SAFETY: no child objects exist yet.
            unsafe { instance.destroy_instance(None) };
            Err(e)
        }
    }
}

fn pick(
    instance: &ash::Instance,
    instance_api: u32,
    opts: &Opts,
) -> Result<(String, DevInfo), String> {
    // SAFETY: a valid instance.
    let physes =
        unsafe { instance.enumerate_physical_devices() }.vk("vkEnumeratePhysicalDevices")?;
    if physes.is_empty() {
        return Err("vkEnumeratePhysicalDevices returned no devices".into());
    }
    let mut devs = Vec::new();
    for (i, &phys) in physes.iter().enumerate() {
        // SAFETY: `phys` came from this instance.
        let props = unsafe { instance.get_physical_device_properties(phys) };
        // SAFETY: as above.
        let exts: Vec<String> = unsafe { instance.enumerate_device_extension_properties(phys) }
            .vk("vkEnumerateDeviceExtensionProperties")?
            .iter()
            .map(|e| cstr(&e.extension_name))
            .collect();
        let driver = driver_name(instance, instance_api, phys, &props, &exts);
        println!(
            "# device[{i}]: {} type={} vendor=0x{:04x}({}) device=0x{:04x} api={} driver={}",
            cstr(&props.device_name),
            type_name(props.device_type),
            props.vendor_id,
            vendor_name(props.vendor_id),
            props.device_id,
            version_string(props.api_version),
            driver
        );
        devs.push(DevInfo {
            phys,
            props,
            driver,
            exts,
        });
    }

    let index = match opts.device_index {
        Some(i) if i < devs.len() => i,
        Some(i) => {
            return Err(format!(
                "--device-index {i} but only {} device(s)",
                devs.len()
            ))
        }
        None => {
            let mut order: Vec<usize> = (0..devs.len()).collect();
            order.sort_by_key(|&i| type_rank(devs[i].props.device_type));
            let best = order[0];
            if devs[best].props.device_type == vk::PhysicalDeviceType::CPU && !opts.allow_cpu {
                return Err(format!(
                    "only CPU device(s) present ({}); pass --allow-cpu or --device-index to test one",
                    cstr(&devs[best].props.device_name)
                ));
            }
            best
        }
    };
    let dev = devs.swap_remove(index);
    let cpu = dev.props.device_type == vk::PhysicalDeviceType::CPU;
    if cpu && !(opts.allow_cpu || opts.device_index.is_some()) {
        return Err("selected device is a CPU device".into());
    }
    let detail = format!(
        "device[{index}] \"{}\" type={} vendor=0x{:04x}({}) apiVersion={} driverName={}{}",
        cstr(&dev.props.device_name),
        type_name(dev.props.device_type),
        dev.props.vendor_id,
        vendor_name(dev.props.vendor_id),
        version_string(dev.props.api_version),
        dev.driver,
        if cpu {
            " (CPU device, explicitly allowed)"
        } else {
            ""
        }
    );
    Ok((detail, dev))
}

fn driver_name(
    instance: &ash::Instance,
    instance_api: u32,
    phys: vk::PhysicalDevice,
    props: &vk::PhysicalDeviceProperties,
    exts: &[String],
) -> String {
    let core = minor(props.api_version) >= (1, 2) && minor(instance_api) >= (1, 2);
    let ext = minor(instance_api) >= (1, 1) && exts.iter().any(|e| e == "VK_KHR_driver_properties");
    if !(core || ext) {
        return "(unavailable: needs Vulkan 1.2 or VK_KHR_driver_properties)".into();
    }
    let mut driver = vk::PhysicalDeviceDriverProperties::default();
    let mut props2 = vk::PhysicalDeviceProperties2::default().push_next(&mut driver);
    // SAFETY: instance >= 1.1, and the chained struct is one the device
    // supports (core 1.2 or its extension is advertised).
    unsafe { instance.get_physical_device_properties2(phys, &mut props2) };
    format!(
        "\"{}\" ({})",
        cstr(&driver.driver_name),
        cstr(&driver.driver_info)
    )
}

pub enum Timeline {
    Core,
    Khr(ash::khr::timeline_semaphore::Device),
}

pub enum DynRender {
    Core,
    Khr(ash::khr::dynamic_rendering::Device),
}

/// What check 2 hands to every later check.
pub struct Gpu {
    pub device: ash::Device,
    pub queue: vk::Queue,
    pub pool: vk::CommandPool,
    pub mem: vk::PhysicalDeviceMemoryProperties,
    pub limits: vk::PhysicalDeviceLimits,
    /// The effective API version (see [`Picked::api`]).
    pub api: u32,
    pub timeline: Option<Timeline>,
    pub dynrender: Option<DynRender>,
    pub timeout_ns: u64,
    /// Set once a wait has timed out. From then on nothing is destroyed —
    /// freeing objects the GPU may still be using is undefined behaviour, and
    /// a smoke test that leaks is better than one that crashes the driver.
    pub hung: Cell<bool>,
}

/// Check 2: one device, one graphics+compute queue, and the optional features
/// checks 7 and 8 need, enabled when the device has them.
pub fn check_device(p: &Picked, opts: &Opts) -> Result<(String, Gpu), String> {
    let inst = &p.instance;
    // SAFETY: `p.phys` belongs to `inst`.
    let families = unsafe { inst.get_physical_device_queue_family_properties(p.phys) };
    let want = vk::QueueFlags::GRAPHICS | vk::QueueFlags::COMPUTE;
    let family = families
        .iter()
        .position(|f| f.queue_flags.contains(want) && f.queue_count > 0)
        .ok_or_else(|| {
            format!(
                "no queue family with GRAPHICS|COMPUTE among {}",
                families.len()
            )
        })? as u32;

    let has = |name: &str| p.exts.iter().any(|e| e == name);
    let api = minor(p.api);

    // What the device supports.
    let mut f12 = vk::PhysicalDeviceVulkan12Features::default();
    let mut f13 = vk::PhysicalDeviceVulkan13Features::default();
    let mut ftl = vk::PhysicalDeviceTimelineSemaphoreFeatures::default();
    let mut fdr = vk::PhysicalDeviceDynamicRenderingFeatures::default();
    if api >= (1, 1) {
        let mut f2 = vk::PhysicalDeviceFeatures2::default();
        if api >= (1, 2) {
            f2 = f2.push_next(&mut f12);
        } else if has("VK_KHR_timeline_semaphore") {
            f2 = f2.push_next(&mut ftl);
        }
        if api >= (1, 3) {
            f2 = f2.push_next(&mut f13);
        } else if has("VK_KHR_dynamic_rendering") {
            f2 = f2.push_next(&mut fdr);
        }
        // SAFETY: instance and device are >= 1.1; every chained struct is core
        // at the effective version or belongs to an advertised extension.
        unsafe { inst.get_physical_device_features2(p.phys, &mut f2) };
    }
    let timeline_core = api >= (1, 2) && f12.timeline_semaphore == vk::TRUE;
    let timeline_khr = !timeline_core && ftl.timeline_semaphore == vk::TRUE;
    let dyn_core = api >= (1, 3) && f13.dynamic_rendering == vk::TRUE;
    let dyn_khr = !dyn_core && fdr.dynamic_rendering == vk::TRUE;

    // What we enable.
    let mut ext_names: Vec<&CStr> = Vec::new();
    if timeline_khr {
        ext_names.push(c"VK_KHR_timeline_semaphore");
    }
    if dyn_khr {
        ext_names.push(c"VK_KHR_dynamic_rendering");
        if api < (1, 2) {
            // Its dependencies, core from 1.2.
            for dep in [
                c"VK_KHR_depth_stencil_resolve",
                c"VK_KHR_create_renderpass2",
                c"VK_KHR_multiview",
                c"VK_KHR_maintenance2",
            ] {
                if has(dep.to_str().unwrap_or_default()) {
                    ext_names.push(dep);
                }
            }
        }
    }
    let ext_ptrs: Vec<*const c_char> = ext_names.iter().map(|n| n.as_ptr()).collect();

    let prio = [1.0f32];
    let queue_info = [vk::DeviceQueueCreateInfo::default()
        .queue_family_index(family)
        .queue_priorities(&prio)];
    let mut en12 = vk::PhysicalDeviceVulkan12Features::default().timeline_semaphore(true);
    let mut en13 = vk::PhysicalDeviceVulkan13Features::default().dynamic_rendering(true);
    let mut entl = vk::PhysicalDeviceTimelineSemaphoreFeatures::default().timeline_semaphore(true);
    let mut endr = vk::PhysicalDeviceDynamicRenderingFeatures::default().dynamic_rendering(true);
    let mut info = vk::DeviceCreateInfo::default()
        .queue_create_infos(&queue_info)
        .enabled_extension_names(&ext_ptrs);
    if timeline_core {
        info = info.push_next(&mut en12);
    }
    if timeline_khr {
        info = info.push_next(&mut entl);
    }
    if dyn_core {
        info = info.push_next(&mut en13);
    }
    if dyn_khr {
        info = info.push_next(&mut endr);
    }
    // SAFETY: every pointer in `info` outlives the call; features and
    // extensions requested are exactly those the device reported.
    let device = unsafe { inst.create_device(p.phys, &info, None) }.vk("vkCreateDevice")?;
    // SAFETY: family/index 0 were created above.
    let queue = unsafe { device.get_device_queue(family, 0) };
    let pool_info = vk::CommandPoolCreateInfo::default()
        .queue_family_index(family)
        .flags(vk::CommandPoolCreateFlags::RESET_COMMAND_BUFFER);
    // SAFETY: a valid device and create info.
    let pool = match unsafe { device.create_command_pool(&pool_info, None) } {
        Ok(pool) => pool,
        Err(e) => {
            // SAFETY: the device has no other children.
            unsafe { device.destroy_device(None) };
            return Err(format!("vkCreateCommandPool: {}", result_name(e)));
        }
    };
    // SAFETY: `p.phys` belongs to `inst`.
    let mem = unsafe { inst.get_physical_device_memory_properties(p.phys) };
    for i in 0..mem.memory_type_count as usize {
        let t = mem.memory_types[i];
        println!(
            "# memory type {i}: heap {} ({} MiB) {:?}",
            t.heap_index,
            mem.memory_heaps[t.heap_index as usize].size >> 20,
            t.property_flags
        );
    }

    let timeline = if timeline_core {
        Some(Timeline::Core)
    } else if timeline_khr {
        Some(Timeline::Khr(ash::khr::timeline_semaphore::Device::new(
            inst, &device,
        )))
    } else {
        None
    };
    let dynrender = if dyn_core {
        Some(DynRender::Core)
    } else if dyn_khr {
        Some(DynRender::Khr(ash::khr::dynamic_rendering::Device::new(
            inst, &device,
        )))
    } else {
        None
    };
    let how = |core: bool, khr: bool| {
        if core {
            "core"
        } else if khr {
            "khr"
        } else {
            "none"
        }
    };
    let detail = format!(
        "queue family {family} ({:?}), timeline_semaphore={}, dynamic_rendering={}, extensions={:?}",
        families[family as usize].queue_flags,
        how(timeline_core, timeline_khr),
        how(dyn_core, dyn_khr),
        ext_names
    );
    let gpu = Gpu {
        device,
        queue,
        pool,
        mem,
        limits: p.props.limits,
        api: p.api,
        timeline,
        dynrender,
        timeout_ns: u64::try_from(opts.timeout.as_nanos()).unwrap_or(u64::MAX),
        hung: Cell::new(false),
    };
    Ok((detail, gpu))
}

impl Gpu {
    /// A memory type allowed by `bits` with every `required` flag, preferring
    /// one that also has `preferred`.
    pub fn mem_type(
        &self,
        bits: u32,
        required: vk::MemoryPropertyFlags,
        preferred: vk::MemoryPropertyFlags,
    ) -> Option<u32> {
        let find = |flags: vk::MemoryPropertyFlags| {
            (0..self.mem.memory_type_count).find(|&i| {
                bits & (1 << i) != 0
                    && self.mem.memory_types[i as usize]
                        .property_flags
                        .contains(flags)
            })
        };
        find(required | preferred).or_else(|| find(required))
    }

    /// Waits for fences; a timeout marks the device hung (see [`Gpu::hung`]).
    pub fn wait_fences(&self, fences: &[vk::Fence], what: &str) -> Result<(), String> {
        // SAFETY: fences belong to this device.
        match unsafe { self.device.wait_for_fences(fences, true, self.timeout_ns) } {
            Ok(()) => Ok(()),
            Err(vk::Result::TIMEOUT) => {
                self.hung.set(true);
                Err(format!(
                    "vkWaitForFences({what}): VK_TIMEOUT after {} ms — the GPU never signalled",
                    self.timeout_ns / 1_000_000
                ))
            }
            Err(e) => Err(format!("vkWaitForFences({what}): {}", result_name(e))),
        }
    }

    /// Records one primary command buffer, submits it with a fence and waits.
    /// Returns how long the submit took to complete.
    pub fn one_shot(
        &self,
        what: &str,
        record: impl FnOnce(&ash::Device, vk::CommandBuffer),
    ) -> Result<Duration, String> {
        let mut scope = Scope::new(self);
        let cb = scope.command_buffer()?;
        let dev = &self.device;
        let begin = vk::CommandBufferBeginInfo::default()
            .flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT);
        // SAFETY: `cb` is a fresh primary command buffer of this device.
        unsafe { dev.begin_command_buffer(cb, &begin) }.vk("vkBeginCommandBuffer")?;
        record(dev, cb);
        // SAFETY: recording was begun above.
        unsafe { dev.end_command_buffer(cb) }.vk("vkEndCommandBuffer")?;
        let fence = scope.fence(false)?;
        let cbs = [cb];
        let submit = [vk::SubmitInfo::default().command_buffers(&cbs)];
        let t0 = Instant::now();
        // SAFETY: `cb` is executable, `fence` unsignalled, both of this device.
        unsafe { dev.queue_submit(self.queue, &submit, fence) }.vk("vkQueueSubmit")?;
        self.wait_fences(&[fence], what)?;
        Ok(t0.elapsed())
    }
}

/// A host-visible or device-local buffer with its own allocation.
#[derive(Clone, Copy)]
pub struct Buf {
    pub buffer: vk::Buffer,
    pub size: u64,
    /// Persistently mapped when the memory type is host-visible, else null.
    ptr: *mut u8,
    pub mem_type: u32,
}

impl Buf {
    pub fn write_u32s(&self, offset_words: usize, data: &[u32]) {
        assert!(!self.ptr.is_null(), "buffer is not host-visible");
        assert!(
            ((offset_words + data.len()) * 4) as u64 <= self.size,
            "write past the buffer"
        );
        // SAFETY: the mapping covers `size` bytes (checked above) and the
        // source is a distinct host allocation.
        unsafe {
            std::ptr::copy_nonoverlapping(
                data.as_ptr().cast::<u8>(),
                self.ptr.add(offset_words * 4),
                data.len() * 4,
            )
        };
    }

    pub fn read_u32s(&self, n: usize) -> Vec<u32> {
        assert!(!self.ptr.is_null(), "buffer is not host-visible");
        assert!((n * 4) as u64 <= self.size, "read past the buffer");
        let mut out = vec![0u32; n];
        // SAFETY: as in write_u32s.
        unsafe { std::ptr::copy_nonoverlapping(self.ptr, out.as_mut_ptr().cast::<u8>(), n * 4) };
        out
    }

    pub fn read_bytes(&self, n: usize) -> Vec<u8> {
        assert!(!self.ptr.is_null(), "buffer is not host-visible");
        assert!(n as u64 <= self.size, "read past the buffer");
        let mut out = vec![0u8; n];
        // SAFETY: as in write_u32s.
        unsafe { std::ptr::copy_nonoverlapping(self.ptr, out.as_mut_ptr(), n) };
        out
    }
}

type Cleanup<'a> = Box<dyn FnOnce(&ash::Device) + 'a>;

/// Destroys what a check created, in reverse order, when the check ends —
/// unless the GPU hung, in which case everything is leaked on purpose.
pub struct Scope<'a> {
    pub gpu: &'a Gpu,
    actions: Vec<Cleanup<'a>>,
}

impl Drop for Scope<'_> {
    fn drop(&mut self) {
        if self.gpu.hung.get() {
            self.actions.clear();
            return;
        }
        while let Some(action) = self.actions.pop() {
            action(&self.gpu.device);
        }
    }
}

impl<'a> Scope<'a> {
    pub fn new(gpu: &'a Gpu) -> Self {
        Self {
            gpu,
            actions: Vec::new(),
        }
    }

    pub fn defer(&mut self, f: impl FnOnce(&ash::Device) + 'a) {
        self.actions.push(Box::new(f));
    }

    /// A buffer whose memory has every `required` flag (and `preferred` ones
    /// when some type has them), bound, and mapped if host-visible.
    pub fn buffer(
        &mut self,
        size: u64,
        usage: vk::BufferUsageFlags,
        required: vk::MemoryPropertyFlags,
        preferred: vk::MemoryPropertyFlags,
    ) -> Result<Buf, String> {
        let dev = &self.gpu.device;
        let info = vk::BufferCreateInfo::default()
            .size(size)
            .usage(usage)
            .sharing_mode(vk::SharingMode::EXCLUSIVE);
        // SAFETY: valid device and create info.
        let buffer = unsafe { dev.create_buffer(&info, None) }.vk("vkCreateBuffer")?;
        // SAFETY: `buffer` is live; destroyed exactly once, by the scope.
        self.defer(move |d| unsafe { d.destroy_buffer(buffer, None) });
        // SAFETY: `buffer` is live.
        let req = unsafe { dev.get_buffer_memory_requirements(buffer) };
        let mem_type = self
            .gpu
            .mem_type(req.memory_type_bits, required, preferred)
            .ok_or_else(|| {
                format!(
                    "no memory type with {required:?} in memoryTypeBits 0x{:x}",
                    req.memory_type_bits
                )
            })?;
        let alloc = vk::MemoryAllocateInfo::default()
            .allocation_size(req.size)
            .memory_type_index(mem_type);
        // SAFETY: valid device and allocate info.
        let memory = unsafe { dev.allocate_memory(&alloc, None) }.vk(&format!(
            "vkAllocateMemory({} bytes, type {mem_type})",
            req.size
        ))?;
        // SAFETY: freed exactly once, after the buffer (reverse order).
        self.defer(move |d| unsafe { d.free_memory(memory, None) });
        // SAFETY: fresh memory of a type the buffer allows, offset 0.
        unsafe { dev.bind_buffer_memory(buffer, memory, 0) }.vk("vkBindBufferMemory")?;
        let flags = self.gpu.mem.memory_types[mem_type as usize].property_flags;
        let ptr = if flags.contains(vk::MemoryPropertyFlags::HOST_VISIBLE) {
            // SAFETY: host-visible memory, whole range, not yet mapped.
            unsafe { dev.map_memory(memory, 0, vk::WHOLE_SIZE, vk::MemoryMapFlags::empty()) }
                .vk("vkMapMemory")?
                .cast::<u8>()
        } else {
            std::ptr::null_mut()
        };
        Ok(Buf {
            buffer,
            size,
            ptr,
            mem_type,
        })
    }

    pub fn command_buffer(&mut self) -> Result<vk::CommandBuffer, String> {
        Ok(self.command_buffers(1)?[0])
    }

    pub fn command_buffers(&mut self, n: u32) -> Result<Vec<vk::CommandBuffer>, String> {
        let pool = self.gpu.pool;
        let info = vk::CommandBufferAllocateInfo::default()
            .command_pool(pool)
            .level(vk::CommandBufferLevel::PRIMARY)
            .command_buffer_count(n);
        // SAFETY: valid pool of this device.
        let cbs = unsafe { self.gpu.device.allocate_command_buffers(&info) }
            .vk("vkAllocateCommandBuffers")?;
        let freed = cbs.clone();
        // SAFETY: freed once, after every submit using them has completed
        // (the scope skips this if a wait timed out).
        self.defer(move |d| unsafe { d.free_command_buffers(pool, &freed) });
        Ok(cbs)
    }

    pub fn fence(&mut self, signaled: bool) -> Result<vk::Fence, String> {
        let flags = if signaled {
            vk::FenceCreateFlags::SIGNALED
        } else {
            vk::FenceCreateFlags::empty()
        };
        // SAFETY: valid device and create info.
        let fence = unsafe {
            self.gpu
                .device
                .create_fence(&vk::FenceCreateInfo::default().flags(flags), None)
        }
        .vk("vkCreateFence")?;
        // SAFETY: destroyed once, after its submit completed.
        self.defer(move |d| unsafe { d.destroy_fence(fence, None) });
        Ok(fence)
    }

    pub fn shader(&mut self, spv: &[u8]) -> Result<vk::ShaderModule, String> {
        let words = ash::util::read_spv(&mut std::io::Cursor::new(spv))
            .map_err(|e| format!("bad SPIR-V: {e}"))?;
        let info = vk::ShaderModuleCreateInfo::default().code(&words);
        // SAFETY: valid device; `words` is naga's validated output.
        let module = unsafe { self.gpu.device.create_shader_module(&info, None) }
            .vk("vkCreateShaderModule")?;
        // SAFETY: destroyed once.
        self.defer(move |d| unsafe { d.destroy_shader_module(module, None) });
        Ok(module)
    }
}

/// A buffer barrier covering the whole buffer.
pub fn buffer_barrier(
    dev: &ash::Device,
    cb: vk::CommandBuffer,
    buffer: vk::Buffer,
    (src_stage, src_access): (vk::PipelineStageFlags, vk::AccessFlags),
    (dst_stage, dst_access): (vk::PipelineStageFlags, vk::AccessFlags),
) {
    let barrier = [vk::BufferMemoryBarrier::default()
        .src_access_mask(src_access)
        .dst_access_mask(dst_access)
        .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
        .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
        .buffer(buffer)
        .offset(0)
        .size(vk::WHOLE_SIZE)];
    // SAFETY: `cb` is recording; `buffer` belongs to this device.
    unsafe {
        dev.cmd_pipeline_barrier(
            cb,
            src_stage,
            dst_stage,
            vk::DependencyFlags::empty(),
            &[],
            &barrier,
            &[],
        )
    };
}

pub const TRANSFER_WRITE: (vk::PipelineStageFlags, vk::AccessFlags) = (
    vk::PipelineStageFlags::TRANSFER,
    vk::AccessFlags::TRANSFER_WRITE,
);
pub const TRANSFER_READ: (vk::PipelineStageFlags, vk::AccessFlags) = (
    vk::PipelineStageFlags::TRANSFER,
    vk::AccessFlags::TRANSFER_READ,
);
pub const HOST_READ: (vk::PipelineStageFlags, vk::AccessFlags) =
    (vk::PipelineStageFlags::HOST, vk::AccessFlags::HOST_READ);

impl Drop for Gpu {
    fn drop(&mut self) {
        if self.hung.get() {
            return;
        }
        // SAFETY: every check's scope has already destroyed its children; the
        // pool's command buffers are freed with it.
        unsafe {
            let _ = self.device.device_wait_idle();
            self.device.destroy_command_pool(self.pool, None);
            self.device.destroy_device(None);
        }
    }
}
