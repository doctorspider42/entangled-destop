//! The Windows presenter of shared frames (ADR-0004, "zero-copy presentation
//! of the GPU-composited desktop"): the display's own wgpu **Vulkan** device
//! imports the renderer's scanout buffers and copies each flipped frame on the
//! GPU into a texture of its own, which the window samples.
//!
//! See [`crate::shared`] for the contract and the portable decisions; this
//! module is only what has to touch Vulkan.
//!
//! # The device
//!
//! A wgpu device on the Vulkan backend — the only one that can import an
//! `OPAQUE_WIN32` Vulkan allocation (a DX12 device cannot) — opened by
//! [`request_device`] with two extensions wgpu does not enable by itself:
//! `VK_KHR_external_memory_win32` (wgpu's own
//! `Features::VULKAN_EXTERNAL_MEMORY_WIN32`) for the import, and
//! `VK_EXT_queue_family_foreign` (added through wgpu-hal's device-creation
//! callback) because the guest compositor releases every scanout buffer to
//! `VK_QUEUE_FAMILY_FOREIGN_EXT`, and acquiring from that family needs it.
//! The window uses the same device for its surface; a headless VM gets one of
//! its own ([`GpuScanout::open_offscreen`]).
//!
//! # One present
//!
//! On the device's queue worker, inside `ScanoutSink::present_shared`, while
//! the renderer holds its claim on the image's payload
//! (`venus::executor::writes`: every guest GPU submission touching it has
//! finished, and none starts until the device drops the lease):
//!
//! 1. **import**, once per image (by [`SharedScanoutImage::serial`]): the
//!    duplicated NT handle into a `VkDeviceMemory` of the export's own size
//!    and type, undedicated, and exactly the canonical image over it;
//! 2. on one command buffer — raw commands recorded into a wgpu encoder
//!    (`CommandEncoder::as_hal_mut`), so wgpu's own submission ordering holds —
//!    **acquire** the image from the guest's release family and layout, copy
//!    the damaged rect (the whole visible region into a texture that does not
//!    hold the frame before) into the display's texture, and **release** the
//!    image back to the same family in the same layout: the barriers of the
//!    renderer's scanout device exactly, with the display's texture going
//!    `SHADER_READ_ONLY → TRANSFER_DST → SHADER_READ_ONLY` around the copy;
//! 3. submit on wgpu's queue and return: the device's queue worker does not
//!    wait for the GPU. The lease — the renderer's claim — goes to the
//!    **retirer**, a thread of this presenter's that polls wgpu while a copy
//!    is outstanding and drops each lease, in submission order, once its copy
//!    has finished; a copy the GPU has not finished within [`COPY_WAIT`] has
//!    its lease dropped anyway (counted `abandoned`). A present finding
//!    [`MAX_IN_FLIGHT`] outstanding waits for the oldest (bounded), and one
//!    finding a copy past the bound fails in band.
//!
//! The display's texture is wgpu's, and wgpu believes it is always in the
//! `RESOURCE` state: it is cleared and moved there once when made
//! ([`GpuScanout::ensure_target`]), wgpu only ever samples it, and the raw
//! copy returns it to exactly that layout. So the window's draws, which wgpu
//! orders on the same queue, never see a copy half done.
//!
//! # Lifetimes
//!
//! Imports go with their resource ([`SharedPresenter::forget`]), on eviction
//! past [`MAX_SHARED_IMPORTS`], and with the presenter — never while a copy of
//! theirs may still run (anything outstanding is waited out first). Nothing
//! here touches guest memory (a handle blob is host GPU memory): presents run
//! inside a device call, and the retirer only drops leases — host state — so
//! pausing (ADR-0005) owes nothing here, and a paused VM's window keeps
//! showing the texture.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use ash::vk;
use virtio_gpu::{Rect, SharedPresent, SharedScanoutFrame, SharedScanoutImage, SharedScanoutLease};
use wgpu::hal::api::Vulkan;

use crate::present::ImagePipeline;
use crate::shared::{
    copy_layout, copy_region, judge, CopyRegion, PresenterDevice, SharedPresenter, SharedStats,
    SharedTexture, MAX_SHARED_IMPORTS,
};
use crate::sync::lock;
use crate::DisplayError;

/// How long a copy may take on the GPU before its lease is dropped anyway:
/// 100 ms, six frames at 60 Hz, the renderer's own bound for its readback
/// (`SCANOUT_WAIT`). A 1080p copy takes well under a millisecond even at the
/// idle clocks a guest desktop leaves the GPU at; a copy this late is a GPU in
/// trouble.
pub const COPY_WAIT: Duration = Duration::from_millis(100);

/// Copies outstanding at once: a present finding this many waits for the
/// oldest (back-pressure, bounded by [`COPY_WAIT`]) before it submits. A
/// compositor flips at most a few times a frame, and every copy is a fraction
/// of one, so only a GPU far behind ever meets it.
pub const MAX_IN_FLIGHT: usize = 8;

/// How often the retirer looks while a copy is outstanding. Windows makes it
/// half a millisecond or more; a lease is only ever wanted back a frame later.
const RETIRE_POLL: Duration = Duration::from_micros(250);

/// How long a screenshot waits for the GPU.
const READ_BACK_WAIT: Duration = Duration::from_secs(2);

/// How long a waiter only yields before it starts to sleep: Windows' shortest
/// sleep is half a millisecond, longer than the copy.
const SPIN: Duration = Duration::from_millis(2);

/// Imports that failed, remembered so a flip of them is declined at once.
const MAX_REFUSED: usize = 16;

/// Presents between two statistics lines.
const REPORT_EVERY: u64 = 600;

/// Opens a wgpu device on `adapter` that can import and acquire a renderer's
/// scanout buffers: the Vulkan backend with `VK_KHR_external_memory_win32`
/// and `VK_EXT_queue_family_foreign`. See the module docs.
///
/// With `latency`, it also enables `VK_NV_low_latency2` and the
/// `VK_KHR_present_id` it requires ([`crate::latency::EXTENSIONS`]) when the
/// adapter has both — the window's GPU boost (ADR-0004, the GPU-boost
/// amendment); an adapter without them opens as before, and the window then
/// has no boost to ask for.
///
/// # Errors
/// Why not, as a sentence: not a Vulkan adapter, an extension missing, a
/// driver refusal.
pub fn request_device(
    adapter: &wgpu::Adapter,
    label: &'static str,
    latency: bool,
) -> Result<(wgpu::Device, wgpu::Queue), String> {
    let features = wgpu::Features::VULKAN_EXTERNAL_MEMORY_WIN32;
    if !adapter.features().contains(features) {
        return Err("the adapter has no VK_KHR_external_memory_win32".into());
    }
    let open = {
        // SAFETY: the hal adapter is only used inside this block, while the
        // `wgpu::Adapter` it came from is alive (borrowed for the whole call).
        let Some(hal) = (unsafe { adapter.as_hal::<Vulkan>() }) else {
            return Err("the adapter is not a Vulkan one".into());
        };
        if !hal
            .physical_device_capabilities()
            .supports_extension(ash::ext::queue_family_foreign::NAME)
        {
            return Err("the adapter has no VK_EXT_queue_family_foreign".into());
        }
        let caps = hal.physical_device_capabilities();
        // `VK_NV_low_latency2` also requires Vulkan 1.2 (or
        // `VK_KHR_timeline_semaphore`): the physical device's version.
        let latency = latency
            && caps.properties().api_version >= vk::API_VERSION_1_2
            && crate::latency::EXTENSIONS
                .iter()
                .all(|name| caps.supports_extension(name));
        let hints = wgpu::MemoryHints::default();
        let add_foreign: Box<wgpu::hal::vulkan::CreateDeviceCallback<'_>> = Box::new(
            move |args: wgpu::hal::vulkan::CreateDeviceCallbackArgs<'_, '_, '_>| {
                args.extensions.push(ash::ext::queue_family_foreign::NAME);
                if latency {
                    args.extensions.extend(crate::latency::EXTENSIONS);
                }
            },
        );
        // SAFETY: `features` are the adapter's own (checked above), and the
        // callback only *adds* extensions the adapter supports (checked
        // above), which `open_with_callback` allows; the device it opens is
        // handed straight to `create_device_from_hal` on the same adapter.
        // `VK_NV_low_latency2`'s own requirements, a Vulkan 1.2 device and
        // `VK_KHR_present_id`, are checked above and enabled beside it.
        unsafe { hal.open_with_callback(features, &hints, Some(add_foreign)) }
            .map_err(|e| format!("the Vulkan device could not be opened ({e})"))?
    };
    // SAFETY: `open` was opened from this very adapter's hal, with the
    // features passed in the descriptor.
    unsafe {
        adapter.create_device_from_hal(
            open,
            &wgpu::DeviceDescriptor {
                label: Some(label),
                required_features: features,
                required_limits: adapter.limits(),
                ..Default::default()
            },
        )
    }
    .map_err(|e| format!("wgpu refused the Vulkan device ({e})"))
}

/// One image imported onto the display's device.
struct Import {
    serial: u64,
    resource_id: u32,
    image: vk::Image,
    memory: vk::DeviceMemory,
}

/// The display's copy of the shared scanout: wgpu's texture and its image.
struct Target {
    view: wgpu::TextureView,
    raw: vk::Image,
    size: (u32, u32),
    generation: u64,
    /// Kept for its lifetime; wgpu owns the image.
    _texture: wgpu::Texture,
}

struct Work {
    /// Least recently presented first.
    imports: Vec<Import>,
    refused: Vec<u64>,
    target: Option<Target>,
    next_generation: u64,
    /// Why the presenter stopped working for good.
    broken: Option<String>,
    /// The screenshot pipeline (`Bgra8Unorm`, nearest), made at the first.
    snap: Option<ImagePipeline>,
    stats: SharedStats,
}

#[derive(Default)]
struct Published {
    active: bool,
    current: Option<SharedTexture>,
}

/// A copy on the GPU and the lease it holds.
struct Outstanding {
    done: Arc<AtomicBool>,
    submitted: Instant,
    /// Held for its drop, which ends the renderer's claim.
    _lease: SharedScanoutLease,
}

#[derive(Default)]
struct RetireState {
    /// In submission order.
    queue: VecDeque<Outstanding>,
    stop: bool,
    retired: u64,
    retire_us: u64,
    retire_max_us: u64,
    abandoned: u64,
}

/// The retirer's half, shared with the presenter. See the module docs.
#[derive(Default)]
struct Retire {
    state: Mutex<RetireState>,
    /// Something to retire (or stop).
    wake: Condvar,
    /// Something was retired: room for a present waiting on the backlog.
    room: Condvar,
}

impl Retire {
    fn lock(&self) -> std::sync::MutexGuard<'_, RetireState> {
        lock(&self.state, "shared scanout retirer")
    }

    fn push(&self, outstanding: Outstanding) {
        self.lock().queue.push_back(outstanding);
        self.wake.notify_one();
    }

    /// Room for one more copy: at once while fewer than [`MAX_IN_FLIGHT`]
    /// are outstanding, else once the retirer has taken the oldest — for
    /// [`COPY_WAIT`] at most. `Err` says why not: the backlog did not move, or
    /// the oldest copy is already past the bound.
    fn make_room(&self) -> Result<(), String> {
        let deadline = Instant::now() + COPY_WAIT;
        let mut state = self.lock();
        loop {
            if state
                .queue
                .front()
                .is_some_and(|o| o.submitted.elapsed() > COPY_WAIT)
            {
                return Err(format!(
                    "the display's GPU has not finished a scanout copy in {COPY_WAIT:?}"
                ));
            }
            if state.queue.len() < MAX_IN_FLIGHT {
                return Ok(());
            }
            let now = Instant::now();
            if now >= deadline {
                return Err(format!(
                    "the display's GPU still has {} scanout copies outstanding after {COPY_WAIT:?}",
                    state.queue.len()
                ));
            }
            state = self
                .room
                .wait_timeout(state, deadline - now)
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .0;
        }
    }

    fn outstanding(&self) -> usize {
        self.lock().queue.len()
    }

    /// Takes every copy that has finished — or is past [`COPY_WAIT`] — off
    /// the front, in order, and counts them; the caller drops their leases.
    fn finished(&self) -> Vec<Outstanding> {
        let mut state = self.lock();
        let mut out = Vec::new();
        while let Some(front) = state.queue.front() {
            let done = front.done.load(Ordering::Acquire);
            let late = front.submitted.elapsed() > COPY_WAIT;
            if !done && !late {
                break;
            }
            let Some(front) = state.queue.pop_front() else {
                break;
            };
            if done {
                let us = u64::try_from(front.submitted.elapsed().as_micros()).unwrap_or(u64::MAX);
                state.retired += 1;
                state.retire_us = state.retire_us.saturating_add(us);
                state.retire_max_us = state.retire_max_us.max(us);
            } else {
                state.abandoned += 1;
                tracing::warn!(
                    "display: a scanout copy has not finished on the GPU in {COPY_WAIT:?}; its \
                     buffer is handed back to the guest anyway"
                );
            }
            out.push(front);
        }
        out
    }
}

/// The retirer: while a copy is outstanding, polls wgpu (never a blocking
/// wait — wgpu holds its fence lock across one, which would stall every
/// submit, the window's included) and drops each finished copy's lease.
fn retire(retire: &Retire, device: &wgpu::Device) {
    loop {
        {
            let mut state = retire.lock();
            while state.queue.is_empty() && !state.stop {
                state = retire
                    .wake
                    .wait(state)
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
            }
            if state.stop {
                return;
            }
        }
        let _ = device.poll(wgpu::PollType::Poll);
        // The leases are dropped here, outside the lock: dropping one ends a
        // claim of the renderer's.
        let finished = retire.finished();
        if !finished.is_empty() {
            drop(finished);
            retire.room.notify_all();
        }
        if retire.outstanding() > 0 {
            std::thread::sleep(RETIRE_POLL);
        }
    }
}

/// The presenter. See the module docs.
pub struct GpuScanout {
    device: wgpu::Device,
    queue: wgpu::Queue,
    raw: ash::Device,
    family: u32,
    memory_types: u32,
    info: PresenterDevice,
    work: Mutex<Work>,
    published: Mutex<Published>,
    retire: Arc<Retire>,
    retirer: Option<std::thread::JoinHandle<()>>,
}

impl std::fmt::Debug for GpuScanout {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GpuScanout")
            .field("family", &self.family)
            .finish_non_exhaustive()
    }
}

/// `(device, driver)` UUIDs and `maxImageDimension2D` of a physical device.
fn identity(
    instance: &ash::Instance,
    physical: vk::PhysicalDevice,
) -> (PresenterDevice, vk::PhysicalDeviceMemoryProperties) {
    let mut id = vk::PhysicalDeviceIDProperties::default();
    let max_dimension = {
        let mut props = vk::PhysicalDeviceProperties2::default().push_next(&mut id);
        // SAFETY: `physical` is the device wgpu opened, of `instance`, which
        // is at least Vulkan 1.1 (wgpu requires it); `props` and its chain are
        // locals that outlive the call.
        unsafe { instance.get_physical_device_properties2(physical, &mut props) };
        props.properties.limits.max_image_dimension2_d
    };
    // SAFETY: as above.
    let memory = unsafe { instance.get_physical_device_memory_properties(physical) };
    (
        PresenterDevice {
            device_uuid: id.device_uuid,
            driver_uuid: id.driver_uuid,
            max_dimension,
        },
        memory,
    )
}

impl GpuScanout {
    /// The presenter on `device` — which must have come from
    /// [`request_device`].
    ///
    /// # Errors
    /// Not a Vulkan device, or one without the two extensions.
    pub fn new(device: wgpu::Device, queue: wgpu::Queue) -> Result<Self, String> {
        let (raw, family, info, memory_types) = {
            // SAFETY: the hal device is only used inside this block, while
            // `device` is alive; the raw device cloned out of it is kept
            // together with `device` (fields of one value), so it never
            // outlives the device wgpu owns.
            let Some(hal) = (unsafe { device.as_hal::<Vulkan>() }) else {
                return Err("the display's device is not a Vulkan one".into());
            };
            let enabled = hal.enabled_device_extensions();
            for needed in [
                ash::khr::external_memory_win32::NAME,
                ash::ext::queue_family_foreign::NAME,
            ] {
                if !enabled.contains(&needed) {
                    return Err(format!(
                        "the display's Vulkan device was opened without {}",
                        needed.to_string_lossy()
                    ));
                }
            }
            let instance = hal.shared_instance().raw_instance();
            let (info, memory) = identity(instance, hal.raw_physical_device());
            (
                hal.raw_device().clone(),
                hal.queue_family_index(),
                info,
                memory.memory_type_count,
            )
        };
        let retire = Arc::new(Retire::default());
        let retirer = {
            let (retire, device) = (Arc::clone(&retire), device.clone());
            std::thread::Builder::new()
                .name("shared-scanout-retire".into())
                .spawn(move || self::retire(&retire, &device))
                .map_err(|e| format!("cannot start the shared scanout retirer: {e}"))?
        };
        tracing::info!(
            family,
            "display: the renderer's scanout buffers are imported onto the display's own GPU \
             (zero-copy presentation)"
        );
        Ok(Self {
            device,
            queue,
            raw,
            family,
            memory_types,
            info,
            work: Mutex::new(Work {
                imports: Vec::new(),
                refused: Vec::new(),
                target: None,
                next_generation: 1,
                broken: None,
                snap: None,
                stats: SharedStats::default(),
            }),
            published: Mutex::new(Published::default()),
            retire,
            retirer: Some(retirer),
        })
    }

    /// A presenter on a device of its own, for a display with no window
    /// (`--headless`): the frames are still presented — copied on the GPU,
    /// where screenshots read them — exactly as the window's are.
    ///
    /// # Errors
    /// No Vulkan adapter, or none that can share.
    pub fn open_offscreen() -> Result<Self, String> {
        let instance = wgpu::Instance::new(&wgpu::InstanceDescriptor {
            backends: wgpu::Backends::VULKAN,
            ..wgpu::InstanceDescriptor::from_env_or_default()
        });
        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::HighPerformance,
            force_fallback_adapter: false,
            compatible_surface: None,
        }))
        .map_err(|e| format!("no Vulkan adapter ({e})"))?;
        let (device, queue) = request_device(&adapter, "entangled-display-offscreen", false)?;
        device.on_uncaptured_error(Box::new(|err| {
            tracing::error!(%err, "wgpu reported an uncaptured error (off-screen presenter)");
        }));
        Self::new(device, queue)
    }

    /// Nothing of ours on the GPU, however long that takes — before
    /// destroying what a copy may use. Rare (a buffer unref'd or evicted),
    /// and the only blocking wait here: it holds up wgpu's submits while it
    /// lasts, which is a copy's worth of GPU time.
    fn idle(&self) {
        if self.retire.outstanding() > 0 {
            let _ = self.device.poll(wgpu::PollType::Wait);
        }
    }

    /// Polls wgpu until `done` or `limit` passes: whether it did.
    fn wait(&self, done: &AtomicBool, limit: Duration) -> bool {
        let start = Instant::now();
        loop {
            let _ = self.device.poll(wgpu::PollType::Poll);
            if done.load(Ordering::Acquire) {
                return true;
            }
            let elapsed = start.elapsed();
            if elapsed >= limit {
                return false;
            }
            if elapsed < SPIN {
                std::thread::yield_now();
            } else {
                std::thread::sleep(Duration::from_micros(200));
            }
        }
    }

    /// A flag set once everything submitted so far has finished.
    fn done_flag(&self) -> Arc<AtomicBool> {
        let done = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&done);
        self.queue
            .on_submitted_work_done(move || flag.store(true, Ordering::Release));
        done
    }

    /// Destroys `import`'s objects; the caller made sure no copy of it runs.
    fn destroy(&self, import: Import) {
        // SAFETY: both were created on `self.raw` by `import_image`, are
        // destroyed exactly once (the value is consumed), and no GPU work
        // using them is in flight: every caller waits out whatever is
        // outstanding first (`idle`, or `Drop`'s wait).
        unsafe {
            self.raw.destroy_image(import.image, None);
            self.raw.free_memory(import.memory, None);
        }
    }

    /// Imports `image`: its memory, and exactly its canonical image bound at 0.
    fn import_image(&self, image: &SharedScanoutImage) -> Result<Import, String> {
        let handle = image
            .handle
            .raw()
            .ok_or("the image's handle names nothing")?;
        if image.memory_type_index >= self.memory_types.min(32) {
            return Err(format!(
                "memory type {} does not exist on the display's GPU",
                image.memory_type_index
            ));
        }
        let info = &image.info;
        let formats: Vec<vk::Format> = info
            .view_formats
            .iter()
            .map(|f| vk::Format::from_raw(*f))
            .collect();
        let mut external = vk::ExternalMemoryImageCreateInfo::default()
            .handle_types(vk::ExternalMemoryHandleTypeFlags::OPAQUE_WIN32);
        let mut list = vk::ImageFormatListCreateInfo::default().view_formats(&formats);
        let mut create = vk::ImageCreateInfo::default()
            .flags(vk::ImageCreateFlags::from_raw(info.flags))
            .image_type(vk::ImageType::TYPE_2D)
            .format(vk::Format::from_raw(info.format))
            .extent(vk::Extent3D {
                width: info.width,
                height: info.height,
                depth: 1,
            })
            .mip_levels(1)
            .array_layers(1)
            .samples(vk::SampleCountFlags::TYPE_1)
            .tiling(vk::ImageTiling::OPTIMAL)
            .usage(vk::ImageUsageFlags::from_raw(info.usage))
            .sharing_mode(vk::SharingMode::EXCLUSIVE)
            .initial_layout(vk::ImageLayout::UNDEFINED)
            .push_next(&mut external);
        if !formats.is_empty() {
            create = create.push_next(&mut list);
        }
        // SAFETY: `self.raw` is the display's live device, created with
        // `VK_KHR_external_memory_win32`; the create info is the renderer's
        // canonical image — values the host already accepted for this GPU —
        // and it, its chain and the format list are locals that outlive the
        // call.
        let vk_image = unsafe { self.raw.create_image(&create, None) }
            .map_err(|e| format!("vkCreateImage of the canonical image ({e})"))?;
        // SAFETY: `vk_image` was just created on `self.raw`.
        let req = unsafe { self.raw.get_image_memory_requirements(vk_image) };
        let type_bit = 1u32 << image.memory_type_index;
        if req.size > image.allocation_size || req.memory_type_bits & type_bit == 0 {
            // SAFETY: created above, never used, destroyed once.
            unsafe { self.raw.destroy_image(vk_image, None) };
            return Err(format!(
                "the canonical image needs {:#x} bytes of types {:#x}; the export is {:#x} of \
                 type {}",
                req.size, req.memory_type_bits, image.allocation_size, image.memory_type_index
            ));
        }
        let mut import = vk::ImportMemoryWin32HandleInfoKHR::default()
            .handle_type(vk::ExternalMemoryHandleTypeFlags::OPAQUE_WIN32)
            .handle(handle as isize);
        let allocate = vk::MemoryAllocateInfo::default()
            .allocation_size(image.allocation_size)
            .memory_type_index(image.memory_type_index)
            .push_next(&mut import);
        // SAFETY: an import of a live NT handle (the `SharedScanoutImage`
        // holding its owned duplicate is borrowed for the whole call; an NT
        // handle import only reads it and takes no ownership), exported by a
        // device of this very GPU and driver (`judge` compared the UUIDs), at
        // the export's own size and type, undedicated like the export.
        let memory = match unsafe { self.raw.allocate_memory(&allocate, None) } {
            Ok(memory) => memory,
            Err(e) => {
                // SAFETY: created above, never used, destroyed once.
                unsafe { self.raw.destroy_image(vk_image, None) };
                return Err(format!("vkAllocateMemory importing the handle ({e})"));
            }
        };
        // SAFETY: both are ours and unbound; offset 0 of an allocation at
        // least `req.size` long of a type the image accepts (checked above).
        if let Err(e) = unsafe { self.raw.bind_image_memory(vk_image, memory, 0) } {
            // SAFETY: created above, never used, destroyed once each.
            unsafe {
                self.raw.destroy_image(vk_image, None);
                self.raw.free_memory(memory, None);
            }
            return Err(format!("vkBindImageMemory of the import ({e})"));
        }
        Ok(Import {
            serial: image.serial,
            resource_id: image.resource_id,
            image: vk_image,
            memory,
        })
    }

    /// The import of `image`, made if need be, moved to the back of the
    /// eviction order: its `vk::Image`.
    fn import_for(&self, work: &mut Work, image: &SharedScanoutImage) -> Result<vk::Image, String> {
        if let Some(at) = work.imports.iter().position(|i| i.serial == image.serial) {
            let hit = work.imports.remove(at);
            let raw = hit.image;
            work.imports.push(hit);
            return Ok(raw);
        }
        let made = self.import_image(image)?;
        work.stats.imports += 1;
        // An older layout of the same resource, and past the cap the least
        // recently presented, go — once nothing of theirs is on the GPU.
        if work
            .imports
            .iter()
            .any(|i| i.resource_id == image.resource_id)
            || work.imports.len() >= MAX_SHARED_IMPORTS
        {
            self.idle();
        }
        let (stale, mut kept): (Vec<Import>, Vec<Import>) = std::mem::take(&mut work.imports)
            .into_iter()
            .partition(|i| i.resource_id == image.resource_id);
        while kept.len() >= MAX_SHARED_IMPORTS {
            let oldest = kept.remove(0);
            self.destroy(oldest);
        }
        for import in stale {
            self.destroy(import);
        }
        let raw = made.image;
        kept.push(made);
        work.imports = kept;
        tracing::debug!(
            resource = image.resource_id,
            serial = image.serial,
            width = image.info.width,
            height = image.info.height,
            "display: a renderer scanout buffer imported"
        );
        Ok(raw)
    }

    /// The display's texture at `size`, made (cleared, and moved to wgpu's
    /// `RESOURCE` state) if there is none of that size: whether it is new.
    fn ensure_target(&self, work: &mut Work, size: (u32, u32)) -> Result<bool, String> {
        if work.target.as_ref().is_some_and(|t| t.size == size) {
            return Ok(false);
        }
        let texture = self.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("shared-scanout"),
            size: wgpu::Extent3d {
                width: size.0,
                height: size.1,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: crate::SCANOUT_TEXTURE_FORMAT,
            usage: wgpu::TextureUsages::TEXTURE_BINDING
                | wgpu::TextureUsages::COPY_DST
                | wgpu::TextureUsages::COPY_SRC
                | wgpu::TextureUsages::RENDER_ATTACHMENT,
            view_formats: &[],
        });
        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        // wgpu's view of it from here on: initialized (the clear), and in
        // `RESOURCE` — the only state it is ever used in by wgpu, and the
        // layout every raw copy leaves it in.
        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("shared-scanout-prime"),
            });
        drop(encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("shared-scanout-clear"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: &view,
                depth_slice: None,
                resolve_target: None,
                ops: wgpu::Operations {
                    load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                    store: wgpu::StoreOp::Store,
                },
            })],
            depth_stencil_attachment: None,
            timestamp_writes: None,
            occlusion_query_set: None,
        }));
        encoder.transition_resources(
            std::iter::empty(),
            std::iter::once(wgpu::TextureTransition {
                texture: &texture,
                selector: None,
                state: wgpu::TextureUses::RESOURCE,
            }),
        );
        self.queue.submit(std::iter::once(encoder.finish()));
        let raw = {
            // SAFETY: the hal texture is only read inside this block, while
            // `texture` is alive; the raw image is kept beside the texture
            // (one `Target`) and never destroyed by us — wgpu owns it.
            let hal = unsafe { texture.as_hal::<Vulkan>() }
                .ok_or("the display's texture is not a Vulkan one")?;
            // SAFETY: as above.
            unsafe { hal.raw_handle() }
        };
        let generation = work.next_generation;
        work.next_generation += 1;
        work.target = Some(Target {
            view,
            raw,
            size,
            generation,
            _texture: texture,
        });
        tracing::debug!(
            width = size.0,
            height = size.1,
            generation,
            "display: shared scanout texture made"
        );
        Ok(true)
    }

    /// Records the acquire, the copy and the release into a wgpu encoder,
    /// submits it and returns its done-flag.
    fn copy(
        &self,
        src: vk::Image,
        dst: vk::Image,
        frame: &SharedScanoutFrame,
        region: CopyRegion,
    ) -> Result<Arc<AtomicBool>, String> {
        let release = frame.release;
        let read_layout = copy_layout(release.layout)
            .ok_or("the release is in a layout the display does not acquire from")?;
        let (guest_layout, read_layout) = (
            vk::ImageLayout::from_raw(release.layout),
            vk::ImageLayout::from_raw(read_layout),
        );
        let color = vk::ImageSubresourceRange {
            aspect_mask: vk::ImageAspectFlags::COLOR,
            base_mip_level: 0,
            level_count: 1,
            base_array_layer: 0,
            layer_count: 1,
        };
        let before = [
            // The guest's image, acquired from its release.
            vk::ImageMemoryBarrier::default()
                .src_access_mask(vk::AccessFlags::empty())
                .dst_access_mask(vk::AccessFlags::TRANSFER_READ)
                .old_layout(guest_layout)
                .new_layout(read_layout)
                .src_queue_family_index(release.family)
                .dst_queue_family_index(self.family)
                .image(src)
                .subresource_range(color),
            // The display's texture, out of wgpu's `RESOURCE` for the copy;
            // earlier samples of it (the window's frames) finish first.
            vk::ImageMemoryBarrier::default()
                .src_access_mask(vk::AccessFlags::MEMORY_WRITE)
                .dst_access_mask(vk::AccessFlags::TRANSFER_WRITE)
                .old_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)
                .new_layout(vk::ImageLayout::TRANSFER_DST_OPTIMAL)
                .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .image(dst)
                .subresource_range(color),
        ];
        let after = [
            // Handed back to the guest's family, in the layout it left.
            vk::ImageMemoryBarrier::default()
                .src_access_mask(vk::AccessFlags::empty())
                .dst_access_mask(vk::AccessFlags::empty())
                .old_layout(read_layout)
                .new_layout(guest_layout)
                .src_queue_family_index(self.family)
                .dst_queue_family_index(release.family)
                .image(src)
                .subresource_range(color),
            // Back in wgpu's `RESOURCE`, the copy visible to everything after.
            vk::ImageMemoryBarrier::default()
                .src_access_mask(vk::AccessFlags::TRANSFER_WRITE)
                .dst_access_mask(vk::AccessFlags::SHADER_READ | vk::AccessFlags::TRANSFER_READ)
                .old_layout(vk::ImageLayout::TRANSFER_DST_OPTIMAL)
                .new_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)
                .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .image(dst)
                .subresource_range(color),
        ];
        let layers = vk::ImageSubresourceLayers {
            aspect_mask: vk::ImageAspectFlags::COLOR,
            mip_level: 0,
            base_array_layer: 0,
            layer_count: 1,
        };
        let offset = |x: u32, y: u32| vk::Offset3D {
            x: i32::try_from(x).unwrap_or(i32::MAX),
            y: i32::try_from(y).unwrap_or(i32::MAX),
            z: 0,
        };
        let Rect {
            x,
            y,
            width,
            height,
        } = region.src;
        let copy = vk::ImageCopy {
            src_subresource: layers,
            src_offset: offset(x, y),
            dst_subresource: layers,
            dst_offset: offset(region.dst.0, region.dst.1),
            extent: vk::Extent3D {
                width,
                height,
                depth: 1,
            },
        };
        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("shared-scanout-copy"),
            });
        let raw = &self.raw;
        // SAFETY: the callback runs while the encoder is recording and gets
        // its open hal encoder; every command goes into that command buffer,
        // which wgpu submits below and keeps alive until it has run. `src` is
        // an import of ours on this device, owned by the guest's release
        // family until the acquire; `dst` is the display's texture, which
        // wgpu holds in `SHADER_READ_ONLY_OPTIMAL` (`ensure_target`) and gets
        // back in exactly that layout. The copy region lies inside both: the
        // image is the framebuffer, `region.src` is inside its visible region
        // (`judge`), and the texture is that region's size. Nothing recorded
        // here is destroyed before it has run (`idle`, `Drop`).
        let recorded = unsafe {
            encoder.as_hal_mut::<Vulkan, _, _>(|hal| {
                let hal = hal?;
                let cb = hal.raw_handle();
                raw.cmd_pipeline_barrier(
                    cb,
                    vk::PipelineStageFlags::ALL_COMMANDS,
                    vk::PipelineStageFlags::TRANSFER,
                    vk::DependencyFlags::empty(),
                    &[],
                    &[],
                    &before,
                );
                raw.cmd_copy_image(
                    cb,
                    src,
                    read_layout,
                    dst,
                    vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                    &[copy],
                );
                raw.cmd_pipeline_barrier(
                    cb,
                    vk::PipelineStageFlags::TRANSFER,
                    vk::PipelineStageFlags::ALL_COMMANDS,
                    vk::DependencyFlags::empty(),
                    &[],
                    &[],
                    &after,
                );
                Some(())
            })
        };
        if recorded.is_none() {
            return Err("the display's command encoder is not a Vulkan one".into());
        }
        self.queue.submit(std::iter::once(encoder.finish()));
        Ok(self.done_flag())
    }

    fn report(&self, presented: u64) {
        if presented % REPORT_EVERY != 0 || presented == 0 {
            return;
        }
        let stats = &self.stats();
        tracing::info!(
            presented = stats.presented,
            full_copies = stats.full_copies,
            imports = stats.imports,
            declined = stats.declined,
            failed = stats.failed,
            mean_ms = format_args!(
                "{:.3}",
                stats.present_us as f64 / stats.presented as f64 / 1000.0
            ),
            max_ms = format_args!("{:.3}", stats.present_max_us as f64 / 1000.0),
            gpu_mean_ms = format_args!(
                "{:.3}",
                stats.retire_us as f64 / stats.retired.max(1) as f64 / 1000.0
            ),
            gpu_max_ms = format_args!("{:.3}", stats.retire_max_us as f64 / 1000.0),
            abandoned = stats.abandoned,
            mpix = format_args!("{:.1}", stats.pixels as f64 / 1e6),
            "display: shared scanout statistics"
        );
    }

    /// Draws the shown frame into an off-screen texture through the window's
    /// own image pipeline (nearest, 1:1) and reads it back. The presenter's
    /// lock is held only to record and submit — a later copy into the texture
    /// is queued behind the draw on the GPU — so a screenshot never holds up
    /// a flush while it waits.
    fn draw_and_read(
        &self,
        mut work: std::sync::MutexGuard<'_, Work>,
    ) -> Result<(u32, u32, Vec<u8>), DisplayError> {
        let target = work
            .target
            .as_ref()
            .ok_or(DisplayError::Config("no shared frame is shown"))?;
        let (width, height) = target.size;
        let source = target.view.clone();
        let snap = work.snap.get_or_insert_with(|| {
            ImagePipeline::new(
                &self.device,
                crate::SCANOUT_TEXTURE_FORMAT,
                wgpu::FilterMode::Nearest,
            )
        });
        let extent = wgpu::Extent3d {
            width,
            height,
            depth_or_array_layers: 1,
        };
        let out = self.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("shared-scanout-screenshot"),
            size: extent,
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: crate::SCANOUT_TEXTURE_FORMAT,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        let out_view = out.create_view(&wgpu::TextureViewDescriptor::default());
        let bind = snap.bind(&self.device, &source);
        let row = u64::from(width) * 4;
        let padded = row.next_multiple_of(u64::from(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT));
        let buffer = self.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("shared-scanout-readback"),
            size: padded * u64::from(height),
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });
        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("shared-scanout-screenshot"),
            });
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("shared-scanout-screenshot"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &out_view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
            });
            pass.set_pipeline(&snap.pipeline);
            pass.set_bind_group(0, &bind, &[]);
            pass.draw(0..3, 0..1);
        }
        let bytes_per_row = u32::try_from(padded)
            .map_err(|_| DisplayError::Config("a screenshot row wider than a copy allows"))?;
        encoder.copy_texture_to_buffer(
            wgpu::TexelCopyTextureInfo {
                texture: &out,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            wgpu::TexelCopyBufferInfo {
                buffer: &buffer,
                layout: wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(bytes_per_row),
                    rows_per_image: Some(height),
                },
            },
            extent,
        );
        self.queue.submit(std::iter::once(encoder.finish()));
        drop(work);
        let mapped = Arc::new(AtomicBool::new(false));
        let ok = Arc::new(AtomicBool::new(false));
        {
            let (mapped, ok) = (Arc::clone(&mapped), Arc::clone(&ok));
            buffer
                .slice(..)
                .map_async(wgpu::MapMode::Read, move |result| {
                    ok.store(result.is_ok(), Ordering::Release);
                    mapped.store(true, Ordering::Release);
                });
        }
        if !self.wait(&mapped, READ_BACK_WAIT) || !ok.load(Ordering::Acquire) {
            return Err(DisplayError::Config(
                "the display's GPU did not answer the screenshot",
            ));
        }
        let data = buffer.slice(..).get_mapped_range();
        let row = usize::try_from(row).unwrap_or(usize::MAX);
        let padded = usize::try_from(padded).unwrap_or(usize::MAX);
        let mut pixels = Vec::with_capacity(row.saturating_mul(height as usize));
        for line in data.chunks(padded).take(height as usize) {
            pixels.extend_from_slice(line.get(..row).unwrap_or(line));
        }
        drop(data);
        buffer.unmap();
        Ok((width, height, pixels))
    }
}

impl SharedPresenter for GpuScanout {
    fn present(&self, frame: &SharedScanoutFrame, lease: SharedScanoutLease) -> SharedPresent {
        let started = Instant::now();
        let mut work = lock(&self.work, "shared scanout");
        if let Some(why) = &work.broken {
            return SharedPresent::never(why.clone());
        }
        if let Err(declined) = judge(frame, &self.info) {
            work.stats.declined += 1;
            return declined;
        }
        if let Err(why) = self.retire.make_room() {
            work.stats.failed += 1;
            return SharedPresent::Failed(why);
        }
        let image = &frame.image;
        if work.refused.contains(&image.serial) {
            work.stats.declined += 1;
            return SharedPresent::not_now("this image did not import");
        }
        let src = match self.import_for(&mut work, image) {
            Ok(src) => src,
            Err(why) => {
                tracing::warn!(
                    resource = image.resource_id,
                    %why,
                    "display: a renderer scanout buffer did not import; its flips are read back"
                );
                if work.refused.len() >= MAX_REFUSED {
                    work.refused.remove(0);
                }
                work.refused.push(image.serial);
                work.stats.declined += 1;
                return SharedPresent::not_now(why);
            }
        };
        let size = (frame.visible.width, frame.visible.height);
        let fresh = match self.ensure_target(&mut work, size) {
            Ok(fresh) => fresh,
            Err(why) => {
                work.broken = Some(why.clone());
                return SharedPresent::never(why);
            }
        };
        let Some((dst, generation, view)) = work
            .target
            .as_ref()
            .map(|t| (t.raw, t.generation, t.view.clone()))
        else {
            return SharedPresent::not_now("no shared scanout texture");
        };
        let current = !fresh && {
            let published = lock(&self.published, "shared scanout published");
            published.active
                && published
                    .current
                    .as_ref()
                    .is_some_and(|c| c.generation == generation)
        };
        let region = copy_region(frame, current);
        let done = match self.copy(src, dst, frame, region) {
            Ok(done) => done,
            Err(why) => {
                work.broken = Some(why.clone());
                return SharedPresent::never(why);
            }
        };
        // The claim is the retirer's to end, once the copy has run.
        self.retire.push(Outstanding {
            done,
            submitted: Instant::now(),
            _lease: lease,
        });
        {
            let mut published = lock(&self.published, "shared scanout published");
            published.active = true;
            published.current = Some(SharedTexture {
                generation,
                view,
                size,
            });
        }
        let us = u64::try_from(started.elapsed().as_micros()).unwrap_or(u64::MAX);
        let stats = &mut work.stats;
        stats.presented += 1;
        stats.full_copies += u64::from(!current);
        stats.pixels += u64::from(region.src.width) * u64::from(region.src.height);
        stats.present_us = stats.present_us.saturating_add(us);
        stats.present_max_us = stats.present_max_us.max(us);
        let presented = stats.presented;
        drop(work);
        self.report(presented);
        SharedPresent::Presented
    }

    fn forget(&self, resource_id: Option<u32>) {
        let mut work = lock(&self.work, "shared scanout");
        let doomed = work
            .imports
            .iter()
            .any(|i| resource_id.is_none_or(|id| i.resource_id == id));
        if !doomed {
            return;
        }
        self.idle();
        let (gone, kept): (Vec<Import>, Vec<Import>) = std::mem::take(&mut work.imports)
            .into_iter()
            .partition(|i| resource_id.is_none_or(|id| i.resource_id == id));
        work.imports = kept;
        for import in gone {
            self.destroy(import);
        }
    }

    fn deactivate(&self) {
        lock(&self.published, "shared scanout published").active = false;
    }

    fn current(&self) -> Option<SharedTexture> {
        let published = lock(&self.published, "shared scanout published");
        if published.active {
            published.current.clone()
        } else {
            None
        }
    }

    fn shown(&self) -> bool {
        let published = lock(&self.published, "shared scanout published");
        published.active && published.current.is_some()
    }

    fn read_back(&self) -> Result<(u32, u32, Vec<u8>), DisplayError> {
        if !self.shown() {
            return Err(DisplayError::Config("no shared frame is shown"));
        }
        self.draw_and_read(lock(&self.work, "shared scanout"))
    }

    fn stats(&self) -> SharedStats {
        let mut stats = lock(&self.work, "shared scanout").stats;
        let retire = self.retire.lock();
        stats.retired = retire.retired;
        stats.retire_us = retire.retire_us;
        stats.retire_max_us = retire.retire_max_us;
        stats.abandoned = retire.abandoned;
        stats
    }
}

impl Drop for GpuScanout {
    fn drop(&mut self) {
        self.retire.lock().stop = true;
        self.retire.wake.notify_all();
        if let Some(retirer) = self.retirer.take() {
            let _ = retirer.join();
        }
        let stats = self.stats();
        let work = self
            .work
            .get_mut()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let imports = std::mem::take(&mut work.imports);
        // Nothing of ours may still run when its image goes, nor when a lease
        // is handed back.
        let _ = self.device.poll(wgpu::PollType::Wait);
        drop(std::mem::take(&mut self.retire.lock().queue));
        for import in imports {
            self.destroy(import);
        }
        if stats.presented > 0 {
            tracing::info!(
                presented = stats.presented,
                full_copies = stats.full_copies,
                imports = stats.imports,
                declined = stats.declined,
                failed = stats.failed,
                retired = stats.retired,
                abandoned = stats.abandoned,
                "display: shared scanout presenter closed"
            );
        }
    }
}
