//! What the Venus path does to **GPU time**, measured on the host GPU with no
//! guest and no ring (ADR-0004, 2026-09-25): vk-smoke's compute shader over
//! 1 Mi elements and a 4 MiB buffer copy, each timed by timestamp queries
//! around the work, in every kind of memory the renderer can put a guest's
//! buffer in — the driver's own host-visible types, **our pages imported**
//! with `VK_EXT_external_memory_host` exactly as `RingPages::for_memory` makes
//! them, and device-local memory, bound to buffers created with and without
//! the `VkExternalMemoryBufferCreateInfo{HOST_ALLOCATION}` the executor adds
//! — on a device with and without the `robustBufferAccess` it forces on, and
//! with the submits back to back or 2 ms apart (a guest's round trip).
//!
//! This separates what the renderer does to resources from what
//! virtualisation costs. It prints a table and asserts only that every
//! result is right; it self-skips like the rest of `host_vulkan`'s tests.

use std::ffi::c_void;
use std::sync::Arc;
use std::time::Duration;

use ash::vk;

use crate::venus::shmem::{PageBudget, RingPages};

const COMPUTE_WGSL: &str = include_str!("../../../../guest/vk-smoke/shaders/compute.wgsl");

const N: u32 = 1 << 20;
const BYTES: u64 = N as u64 * 4;
const RUNS: usize = 12;

/// vk-smoke's `compute_f`.
fn f(i: u32) -> u32 {
    let mut x = i.wrapping_mul(2_654_435_761);
    x ^= x >> 15;
    x.wrapping_add(i << 3).wrapping_add(0x9e37_79b9)
}

fn spirv(source: &str) -> Vec<u32> {
    let module = naga::front::wgsl::parse_str(source).expect("the WGSL parses");
    let info = naga::valid::Validator::new(
        naga::valid::ValidationFlags::all(),
        naga::valid::Capabilities::empty(),
    )
    .validate(&module)
    .expect("the WGSL validates");
    let options = naga::back::spv::Options {
        lang_version: (1, 0),
        flags: naga::back::spv::WriterFlags::empty(),
        ..naga::back::spv::Options::default()
    };
    naga::back::spv::write_vec(&module, &info, &options, None).expect("SPIR-V")
}

/// Where a buffer's memory comes from.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Placement {
    /// `vkAllocateMemory` of the first type with these flags (the driver's
    /// own pages).
    Driver(vk::MemoryPropertyFlags),
    /// Our pages, imported into the first importable type with these flags.
    Imported(vk::MemoryPropertyFlags),
}

struct Buffer {
    buffer: vk::Buffer,
    memory: vk::DeviceMemory,
    type_index: u32,
    mapped: *mut u32,
    _pages: Option<RingPages>,
}

struct Bench {
    _entry: ash::Entry,
    instance: ash::Instance,
    device: ash::Device,
    queue: vk::Queue,
    pool: vk::CommandPool,
    stamps: vk::QueryPool,
    period_ns: f64,
    mem: vk::PhysicalDeviceMemoryProperties,
    host_pointer: vk::PFN_vkGetMemoryHostPointerPropertiesEXT,
    align: u64,
    budget: Arc<PageBudget>,
    pipeline: vk::Pipeline,
    layout: vk::PipelineLayout,
    set_layout: vk::DescriptorSetLayout,
    set_pool: vk::DescriptorPool,
}

const HOST_ALLOCATION: vk::ExternalMemoryHandleTypeFlags =
    vk::ExternalMemoryHandleTypeFlags::HOST_ALLOCATION_EXT;

impl Bench {
    /// The first discrete or integrated GPU with `VK_EXT_external_memory_host`,
    /// a device on it with or without `robustBufferAccess`; `None` (a skip)
    /// without one.
    fn new(robust: bool) -> Option<Self> {
        // SAFETY: test-only direct use of the loader; every structure is a
        // local that outlives its call, and `Drop` destroys what is made here.
        unsafe {
            let entry = ash::Entry::load().ok()?;
            let app = vk::ApplicationInfo::default().api_version(vk::API_VERSION_1_3);
            let instance = entry
                .create_instance(
                    &vk::InstanceCreateInfo::default().application_info(&app),
                    None,
                )
                .ok()?;
            let ext = c"VK_EXT_external_memory_host";
            let phys = instance
                .enumerate_physical_devices()
                .ok()?
                .into_iter()
                .find(|&p| {
                    let props = instance.get_physical_device_properties(p);
                    let has = instance
                        .enumerate_device_extension_properties(p)
                        .unwrap_or_default()
                        .iter()
                        .any(|e| e.extension_name_as_c_str() == Ok(ext));
                    has && matches!(
                        props.device_type,
                        vk::PhysicalDeviceType::DISCRETE_GPU
                            | vk::PhysicalDeviceType::INTEGRATED_GPU
                    )
                });
            let Some(phys) = phys else {
                eprintln!("skipping: no GPU with VK_EXT_external_memory_host");
                instance.destroy_instance(None);
                return None;
            };
            let props = instance.get_physical_device_properties(phys);
            let mut host_props = vk::PhysicalDeviceExternalMemoryHostPropertiesEXT::default();
            let mut props2 = vk::PhysicalDeviceProperties2::default().push_next(&mut host_props);
            instance.get_physical_device_properties2(phys, &mut props2);
            let align = host_props.min_imported_host_pointer_alignment;
            let family = instance
                .get_physical_device_queue_family_properties(phys)
                .iter()
                .position(|q| {
                    q.queue_flags.contains(vk::QueueFlags::COMPUTE) && q.timestamp_valid_bits >= 64
                })? as u32;
            let prio = [1.0];
            let queues = [vk::DeviceQueueCreateInfo::default()
                .queue_family_index(family)
                .queue_priorities(&prio)];
            let features = vk::PhysicalDeviceFeatures::default().robust_buffer_access(robust);
            let exts = [ext.as_ptr()];
            let device = instance
                .create_device(
                    phys,
                    &vk::DeviceCreateInfo::default()
                        .queue_create_infos(&queues)
                        .enabled_extension_names(&exts)
                        .enabled_features(&features),
                    None,
                )
                .ok()?;
            let host_pointer: vk::PFN_vkGetMemoryHostPointerPropertiesEXT =
                std::mem::transmute(instance.get_device_proc_addr(
                    device.handle(),
                    c"vkGetMemoryHostPointerPropertiesEXT".as_ptr(),
                )?);
            let queue = device.get_device_queue(family, 0);
            let pool = device
                .create_command_pool(
                    &vk::CommandPoolCreateInfo::default()
                        .queue_family_index(family)
                        .flags(vk::CommandPoolCreateFlags::RESET_COMMAND_BUFFER),
                    None,
                )
                .ok()?;
            let stamps = device
                .create_query_pool(
                    &vk::QueryPoolCreateInfo::default()
                        .query_type(vk::QueryType::TIMESTAMP)
                        .query_count(2),
                    None,
                )
                .ok()?;
            let code = spirv(COMPUTE_WGSL);
            let module = device
                .create_shader_module(&vk::ShaderModuleCreateInfo::default().code(&code), None)
                .ok()?;
            let bindings = [vk::DescriptorSetLayoutBinding::default()
                .binding(0)
                .descriptor_type(vk::DescriptorType::STORAGE_BUFFER)
                .descriptor_count(1)
                .stage_flags(vk::ShaderStageFlags::COMPUTE)];
            let set_layout = device
                .create_descriptor_set_layout(
                    &vk::DescriptorSetLayoutCreateInfo::default().bindings(&bindings),
                    None,
                )
                .ok()?;
            let set_layouts = [set_layout];
            let layout = device
                .create_pipeline_layout(
                    &vk::PipelineLayoutCreateInfo::default().set_layouts(&set_layouts),
                    None,
                )
                .ok()?;
            let stage = vk::PipelineShaderStageCreateInfo::default()
                .stage(vk::ShaderStageFlags::COMPUTE)
                .module(module)
                .name(c"main");
            let pipeline = device
                .create_compute_pipelines(
                    vk::PipelineCache::null(),
                    &[vk::ComputePipelineCreateInfo::default()
                        .stage(stage)
                        .layout(layout)],
                    None,
                )
                .ok()?[0];
            device.destroy_shader_module(module, None);
            let sizes = [vk::DescriptorPoolSize {
                ty: vk::DescriptorType::STORAGE_BUFFER,
                descriptor_count: 64,
            }];
            let set_pool = device
                .create_descriptor_pool(
                    &vk::DescriptorPoolCreateInfo::default()
                        .max_sets(64)
                        .pool_sizes(&sizes),
                    None,
                )
                .ok()?;
            Some(Self {
                _entry: entry,
                mem: instance.get_physical_device_memory_properties(phys),
                instance,
                device,
                queue,
                pool,
                stamps,
                period_ns: f64::from(props.limits.timestamp_period),
                host_pointer,
                align,
                budget: PageBudget::new(1 << 30),
                pipeline,
                layout,
                set_layout,
                set_pool,
            })
        }
    }

    fn first_type(&self, bits: u32, flags: vk::MemoryPropertyFlags) -> Option<u32> {
        (0..self.mem.memory_type_count).find(|&i| {
            bits & (1 << i) != 0
                && self.mem.memory_types[i as usize]
                    .property_flags
                    .contains(flags)
        })
    }

    /// A 4 MiB storage/transfer buffer, created for host allocations when
    /// `external` (as the executor creates every buffer the driver may
    /// import for), in `placement`; `None` when no type fits.
    fn buffer(&self, placement: Placement, external: bool) -> Option<Buffer> {
        let d = &self.device;
        let mut ext = vk::ExternalMemoryBufferCreateInfo::default().handle_types(HOST_ALLOCATION);
        let mut info = vk::BufferCreateInfo::default().size(BYTES).usage(
            vk::BufferUsageFlags::STORAGE_BUFFER
                | vk::BufferUsageFlags::TRANSFER_SRC
                | vk::BufferUsageFlags::TRANSFER_DST,
        );
        if external {
            info = info.push_next(&mut ext);
        }
        // SAFETY: test-only; valid device, locals that outlive each call;
        // what is made is destroyed in `free`.
        unsafe {
            let buffer = d.create_buffer(&info, None).ok()?;
            let req = d.get_buffer_memory_requirements(buffer);
            let (memory, type_index, pages) = match placement {
                Placement::Driver(flags) => {
                    let type_index = self.first_type(req.memory_type_bits, flags)?;
                    let memory = d
                        .allocate_memory(
                            &vk::MemoryAllocateInfo::default()
                                .allocation_size(req.size)
                                .memory_type_index(type_index),
                            None,
                        )
                        .ok()?;
                    (memory, type_index, None)
                }
                Placement::Imported(flags) => {
                    let pages = RingPages::for_memory(req.size, self.align, &self.budget).ok()?;
                    let mut out = vk::MemoryHostPointerPropertiesEXT::default();
                    let r = (self.host_pointer)(
                        d.handle(),
                        HOST_ALLOCATION,
                        pages.as_ptr().cast::<c_void>().cast_const(),
                        &mut out,
                    );
                    assert_eq!(r, vk::Result::SUCCESS);
                    let type_index =
                        self.first_type(req.memory_type_bits & out.memory_type_bits, flags)?;
                    let mut import = vk::ImportMemoryHostPointerInfoEXT::default()
                        .handle_type(HOST_ALLOCATION)
                        .host_pointer(pages.as_ptr().cast::<c_void>());
                    let memory = d
                        .allocate_memory(
                            &vk::MemoryAllocateInfo::default()
                                .allocation_size(pages.resource_len())
                                .memory_type_index(type_index)
                                .push_next(&mut import),
                            None,
                        )
                        .ok()?;
                    (memory, type_index, Some(pages))
                }
            };
            d.bind_buffer_memory(buffer, memory, 0).ok()?;
            let host_visible = self.mem.memory_types[type_index as usize]
                .property_flags
                .contains(vk::MemoryPropertyFlags::HOST_VISIBLE);
            let mapped = if host_visible {
                d.map_memory(memory, 0, BYTES, vk::MemoryMapFlags::empty())
                    .ok()?
                    .cast::<u32>()
            } else {
                std::ptr::null_mut()
            };
            Some(Buffer {
                buffer,
                memory,
                type_index,
                mapped,
                _pages: pages,
            })
        }
    }

    fn free(&self, b: Buffer) {
        // SAFETY: the device is idle (every submit was waited for); both
        // objects are of this device and destroyed once.
        unsafe {
            self.device.destroy_buffer(b.buffer, None);
            self.device.free_memory(b.memory, None);
        }
    }

    fn set_for(&self, b: &Buffer) -> vk::DescriptorSet {
        // SAFETY: test-only; the pool has room for every set these tests
        // make, and the write names a live buffer.
        unsafe {
            let layouts = [self.set_layout];
            let set = self
                .device
                .allocate_descriptor_sets(
                    &vk::DescriptorSetAllocateInfo::default()
                        .descriptor_pool(self.set_pool)
                        .set_layouts(&layouts),
                )
                .expect("a set")[0];
            let info = [vk::DescriptorBufferInfo {
                buffer: b.buffer,
                offset: 0,
                range: vk::WHOLE_SIZE,
            }];
            let write = [vk::WriteDescriptorSet::default()
                .dst_set(set)
                .dst_binding(0)
                .descriptor_type(vk::DescriptorType::STORAGE_BUFFER)
                .buffer_info(&info)];
            self.device.update_descriptor_sets(&write, &[]);
            set
        }
    }

    /// `record` between two timestamps, submitted and waited for `RUNS`
    /// times with `gap` between submits; the median GPU time in ms.
    fn time(&self, gap: Duration, record: impl Fn(vk::CommandBuffer)) -> f64 {
        let d = &self.device;
        let mut times = Vec::new();
        // SAFETY: test-only; one command buffer and fence of this device,
        // every submit waited for before the next reuses them.
        unsafe {
            let cb = d
                .allocate_command_buffers(
                    &vk::CommandBufferAllocateInfo::default()
                        .command_pool(self.pool)
                        .level(vk::CommandBufferLevel::PRIMARY)
                        .command_buffer_count(1),
                )
                .expect("a command buffer")[0];
            let fence = d
                .create_fence(&vk::FenceCreateInfo::default(), None)
                .expect("a fence");
            for _ in 0..RUNS {
                std::thread::sleep(gap);
                d.begin_command_buffer(cb, &vk::CommandBufferBeginInfo::default())
                    .expect("begin");
                d.cmd_reset_query_pool(cb, self.stamps, 0, 2);
                d.cmd_write_timestamp(cb, vk::PipelineStageFlags::TOP_OF_PIPE, self.stamps, 0);
                record(cb);
                d.cmd_write_timestamp(cb, vk::PipelineStageFlags::BOTTOM_OF_PIPE, self.stamps, 1);
                d.end_command_buffer(cb).expect("end");
                let cbs = [cb];
                d.queue_submit(
                    self.queue,
                    &[vk::SubmitInfo::default().command_buffers(&cbs)],
                    fence,
                )
                .expect("submit");
                d.wait_for_fences(&[fence], true, 10_000_000_000)
                    .expect("the GPU finished");
                d.reset_fences(&[fence]).expect("reset");
                let mut ticks = [0u64; 2];
                d.get_query_pool_results(
                    self.stamps,
                    0,
                    &mut ticks,
                    vk::QueryResultFlags::TYPE_64 | vk::QueryResultFlags::WAIT,
                )
                .expect("timestamps");
                times.push(ticks[1].wrapping_sub(ticks[0]) as f64 * self.period_ns / 1e6);
            }
            d.destroy_fence(fence, None);
            d.free_command_buffers(self.pool, &[cb]);
        }
        times.sort_by(f64::total_cmp);
        times[times.len() / 2]
    }

    fn dispatch(&self, cb: vk::CommandBuffer, set: vk::DescriptorSet, b: &Buffer) {
        let d = &self.device;
        // SAFETY: `cb` is recording; pipeline, layout and set match.
        unsafe {
            d.cmd_bind_pipeline(cb, vk::PipelineBindPoint::COMPUTE, self.pipeline);
            d.cmd_bind_descriptor_sets(
                cb,
                vk::PipelineBindPoint::COMPUTE,
                self.layout,
                0,
                &[set],
                &[],
            );
            d.cmd_dispatch(cb, N / 64, 1, 1);
            let barrier = [vk::BufferMemoryBarrier::default()
                .src_access_mask(vk::AccessFlags::SHADER_WRITE)
                .dst_access_mask(vk::AccessFlags::HOST_READ | vk::AccessFlags::TRANSFER_READ)
                .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .buffer(b.buffer)
                .size(vk::WHOLE_SIZE)];
            d.cmd_pipeline_barrier(
                cb,
                vk::PipelineStageFlags::COMPUTE_SHADER,
                vk::PipelineStageFlags::HOST | vk::PipelineStageFlags::TRANSFER,
                vk::DependencyFlags::empty(),
                &[],
                &barrier,
                &[],
            );
        }
    }

    fn copy(&self, cb: vk::CommandBuffer, src: &Buffer, dst: &Buffer) {
        // SAFETY: `cb` is recording; both buffers hold `BYTES`.
        unsafe {
            self.device.cmd_copy_buffer(
                cb,
                src.buffer,
                dst.buffer,
                &[vk::BufferCopy {
                    src_offset: 0,
                    dst_offset: 0,
                    size: BYTES,
                }],
            );
        }
    }
}

impl Drop for Bench {
    fn drop(&mut self) {
        // SAFETY: every object below is of this device/instance, idle and
        // destroyed once, children first.
        unsafe {
            let _ = self.device.device_wait_idle();
            self.device.destroy_descriptor_pool(self.set_pool, None);
            self.device.destroy_pipeline(self.pipeline, None);
            self.device.destroy_pipeline_layout(self.layout, None);
            self.device
                .destroy_descriptor_set_layout(self.set_layout, None);
            self.device.destroy_query_pool(self.stamps, None);
            self.device.destroy_command_pool(self.pool, None);
            self.device.destroy_device(None);
            self.instance.destroy_instance(None);
        }
    }
}

fn check(b: &Buffer) {
    if b.mapped.is_null() {
        return;
    }
    // SAFETY: `mapped` is the live mapping of `BYTES` bytes of `b`'s memory,
    // coherent, and the GPU's writes were made visible to the host (barrier +
    // fence) before this read.
    let got = unsafe { std::slice::from_raw_parts(b.mapped, N as usize) };
    let wrong = (0..N).filter(|&i| got[i as usize] != f(i)).count();
    assert_eq!(wrong, 0, "memory type {}", b.type_index);
}

/// The table: for each memory a guest's buffer can be in, the compute shader
/// writing 4 MiB into it and a 4 MiB copy from device-local memory into it.
#[test]
fn gpu_time_of_each_memory_the_renderer_can_give_a_guest() {
    use vk::MemoryPropertyFlags as M;
    let hv = M::HOST_VISIBLE | M::HOST_COHERENT;
    let rows: [(&str, Placement, bool); 7] = [
        ("device-local", Placement::Driver(M::DEVICE_LOCAL), false),
        (
            "device-local, created for host allocations",
            Placement::Driver(M::DEVICE_LOCAL),
            true,
        ),
        (
            "driver host-visible (type 3 shape)",
            Placement::Driver(hv),
            false,
        ),
        (
            "driver host-visible cached (type 4 shape)",
            Placement::Driver(hv | M::HOST_CACHED),
            false,
        ),
        (
            "our pages imported, uncached type",
            Placement::Imported(hv),
            true,
        ),
        (
            "our pages imported, cached type",
            Placement::Imported(hv | M::HOST_CACHED),
            true,
        ),
        (
            "driver host-visible, created for host allocations",
            Placement::Driver(hv),
            true,
        ),
    ];
    for robust in [false, true] {
        let Some(bench) = Bench::new(robust) else {
            return;
        };
        let src = bench
            .buffer(Placement::Driver(M::DEVICE_LOCAL), false)
            .expect("a device-local buffer");
        let src_set = bench.set_for(&src);
        eprintln!(
            "robustBufferAccess {robust}: median of {RUNS}, GPU timestamps, ms (back to back / 2 ms apart)"
        );
        for (name, placement, external) in rows {
            let Some(buf) = bench.buffer(placement, external) else {
                eprintln!("  {name:<52} no such memory");
                continue;
            };
            let set = bench.set_for(&buf);
            let dispatch = |gap| bench.time(gap, |cb| bench.dispatch(cb, set, &buf));
            let (d0, d2) = (dispatch(Duration::ZERO), dispatch(Duration::from_millis(2)));
            check(&buf);
            // Fill `src` with the answer, then copy it over.
            bench.time(Duration::ZERO, |cb| bench.dispatch(cb, src_set, &src));
            let copy = |gap| bench.time(gap, |cb| bench.copy(cb, &src, &buf));
            let (c0, c2) = (copy(Duration::ZERO), copy(Duration::from_millis(2)));
            check(&buf);
            eprintln!(
                "  {name:<52} type {}: dispatch {d0:.3} / {d2:.3}, copy 4 MiB in {c0:.3} / {c2:.3} ({:.1} GB/s)",
                buf.type_index,
                BYTES as f64 / (c0 * 1e6)
            );
            bench.free(buf);
        }
        bench.free(src);
    }
}

/// The dispatch into device-local memory on a device created at once, and
/// on one created after the GPU has been idle for five seconds — which is
/// what a guest's `vkCreateDevice` is to the host: a device made late in a
/// long-lived process. A native process gets full clocks for about two
/// seconds from its first device; the numbers say whether a later device
/// gets them too.
#[test]
fn a_device_made_late_in_a_process_runs_at_the_clocks_the_gpu_is_at() {
    use vk::MemoryPropertyFlags as M;
    let time_one = |when: &str| {
        let Some(bench) = Bench::new(true) else {
            return false;
        };
        let buf = bench
            .buffer(Placement::Driver(M::DEVICE_LOCAL), true)
            .expect("a device-local buffer");
        let set = bench.set_for(&buf);
        let t = bench.time(Duration::ZERO, |cb| bench.dispatch(cb, set, &buf));
        eprintln!(
            "device made {when}: dispatch into device-local memory {t:.3} ms (median of {RUNS})"
        );
        bench.free(buf);
        true
    };
    if !time_one("first in the process") {
        return;
    }
    std::thread::sleep(Duration::from_secs(5));
    time_one("after 5 s of an idle GPU");
}
