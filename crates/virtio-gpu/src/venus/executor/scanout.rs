//! The renderer's own **scanout device** (stage S2b of "GNOME on the GPU"):
//! how a handle blob a guest compositor flips to reaches the window.
//!
//! # What arrives
//!
//! Mutter on Zink allocates each scanout buffer through GBM; Zink exports it
//! as a LINEAR DRM-modifier image — here, the canonical optimal image in
//! exportable device-local memory (`executor::modifier`), whose blob is a
//! **handle blob** — and at the end of every batch releases it out of its
//! instance (`zink_batch.c:900-934`): one image barrier with `oldLayout ==
//! newLayout == res->layout` (whatever layout the frame left it in — Zink
//! does not transition for the release), `srcAccessMask` its last access,
//! `dstAccessMask` 0, `dstStageMask` `ALL_COMMANDS`, `srcQueueFamilyIndex`
//! its queue's family and `dstQueueFamilyIndex` `VK_QUEUE_FAMILY_FOREIGN_EXT`.
//! Mutter commits the flip only once the frame's native fence has signalled
//! (`meta-kms-impl-device.c:2089-2116`, the update's `sync_fd`), so by the
//! time `SET_SCANOUT_BLOB` + `RESOURCE_FLUSH` reach the device the release
//! has executed on the host GPU.
//!
//! The executor records, on the blob, the canonical image bound to its memory
//! ([`crate::venus::renderer::ScanoutImage`]) and every such release's layout
//! and family ([`crate::venus::renderer::ScanoutRelease`]).
//!
//! # The device
//!
//! One host `VkDevice` the renderer owns, not any guest context — so no
//! guest's `vkDestroyDevice`, context teardown or loss can take it — created
//! lazily by the first handle-blob scanout on the physical device whose
//! `deviceUUID`/`driverUUID` are the export's (an opaque import requires
//! them), with one queue (the first graphics family, else the first with
//! transfers), one command pool, one command buffer and one fence, and the
//! three extensions a read needs: `VK_KHR_external_memory_win32` (the
//! import), `VK_EXT_external_memory_host` (the staging buffer is our pages)
//! and `VK_EXT_queue_family_foreign` (the ownership transfer).
//!
//! Per blob it keeps a [`Target`]: the export's handle imported
//! (`VkImportMemoryWin32HandleInfoKHR`, the export's own type and size),
//! **exactly the canonical image** recorded on the blob
//! ([`CanonicalImage::create_info`] — the function the exporter's and every
//! importer's create info come from) bound to it at offset 0, and a staging
//! buffer of `width × height × 4` bytes of our own pages. At most
//! [`MAX_SCANOUT_TARGETS`] are kept, least recently read going first; one
//! evicted is simply made again by the next read.
//!
//! # One read
//!
//! On the one command buffer, fenced:
//!
//! 1. **acquire** — `srcQueueFamilyIndex` the family the guest released to,
//!    `dstQueueFamilyIndex` ours, `oldLayout` the layout it released in
//!    (never `UNDEFINED`, which would discard the frame), `newLayout` that
//!    layout if a copy may read in it (`GENERAL`, `TRANSFER_SRC_OPTIMAL`) or
//!    else `TRANSFER_SRC_OPTIMAL`; `TOP_OF_PIPE` → `TRANSFER`, access 0 →
//!    `TRANSFER_READ`;
//! 2. `vkCmdCopyImageToBuffer` of the rect, packed, into the staging buffer;
//! 3. **release** — back from ours to the guest's family, `newLayout` the
//!    layout the guest released in, so its next acquire (`oldLayout =
//!    res->layout`, from `FOREIGN`) is exactly consistent; `TRANSFER` →
//!    `BOTTOM_OF_PIPE`, access 0 → 0 — beside a buffer barrier making the
//!    copy visible to the host (`TRANSFER_WRITE` → `HOST_READ`).
//!
//! The fence is waited for [`SCANOUT_WAIT`] at most. A GPU that has not
//! finished by then is an error for this flush (the window keeps its last
//! frame), and the next read waits for that work first, again bounded; a
//! lost device is an error and the whole device is dropped, to be made again
//! by the next scanout. Only then is the rect copied out of the staging
//! pages — packed, so in one plain block, through the bounded
//! [`PrivatePages::read_rows`]. The staging pages are [`PrivatePages`]: no
//! guest ever maps them (they are no blob, and publishing them is refused),
//! and nothing else writes them once the fence has signalled, so the copy
//! needs none of the byte-wise atomic loads guest-visible pages do.
//!
//! The copy is of the flush's rect only, on the GPU and on the CPU alike:
//! the device passes the guest's damage rect clipped to the scanout, and the
//! image copy reads exactly that region, packed at its own width.
//!
//! # Lifecycle
//!
//! Everything here runs inside a trait call on the device's queue worker,
//! which the pause gate already stops: no thread of its own, so no `Quiesce`
//! obligation (ADR-0005). A blob's target goes with the blob, with a changed
//! layout, and on [`ScanoutDevice::clear`] (a device reset). The `VkDevice`
//! itself **survives a reset**: it holds nothing of any guest after `clear`,
//! and a rebooted desktop scans out again within seconds, so re-creating it
//! would only be a stall. It goes when the renderer does, when the device is
//! lost, or when a blob of another GPU is scanned out.

use std::sync::Arc;
use std::time::Duration;

use crate::protocol::Rect;
use crate::venus::protocol::{
    AllocateCommandBuffersArgs, BeginCommandBufferArgs, CmdCopyImageToBufferArgs,
    CmdPipelineBarrierArgs, Command, CreateFenceArgs, EndCommandBufferArgs, QueueSubmitArgs,
    ResetCommandBufferArgs, ResetFencesArgs, VkBufferCreateInfo, VkBufferImageCopy,
    VkBufferMemoryBarrier, VkCommandBuffer, VkCommandBufferAllocateInfo, VkCommandBufferBeginInfo,
    VkCommandPoolCreateInfo, VkDevice, VkExtent3D, VkFence, VkFenceCreateInfo,
    VkImageMemoryBarrier, VkImageSubresourceLayers, VkImageSubresourceRange, VkMemoryRequirements2,
    VkOffset3D, VkQueue, VkSubmitInfo, WaitForFencesArgs, VK_ERROR_DEVICE_LOST, VK_ERROR_UNKNOWN,
    VK_SUCCESS, VK_TIMEOUT,
};
use crate::venus::protocol::{VkBuffer, VkImage};
use crate::venus::renderer::{ScanoutRelease, ScanoutTarget, SharedRef};
use crate::venus::shmem::{PageBudget, PrivatePages};

use super::generated;
use super::host::{
    DeviceRequest, HostVulkan, ImageBind, InstanceRequest, MemoryRequest, QueueRequest, RawHandle,
    ResourceMemory,
};
use super::memory::HandleExport;
use super::modifier::CanonicalImage;
use super::objects::Kind;
use super::policy;

/// How long one scanout read waits for the GPU: 100 ms, six frames at
/// 60 Hz. The copy of a 1080p frame takes a fraction of a millisecond on a
/// real GPU; a wait this long is a GPU in trouble, and the device's queue
/// worker must not be held longer than that for a frame the window can
/// simply not show.
pub const SCANOUT_WAIT: Duration = Duration::from_millis(100);

/// Blobs the scanout device keeps an import of at once: a compositor flips
/// between two or three; one more covers a mode change in flight.
pub const MAX_SCANOUT_TARGETS: usize = 4;

/// `VK_IMAGE_LAYOUT_GENERAL`.
pub const LAYOUT_GENERAL: i32 = 1;
/// `VK_IMAGE_LAYOUT_COLOR_ATTACHMENT_OPTIMAL`.
pub const LAYOUT_COLOR_ATTACHMENT: i32 = 2;
/// `VK_IMAGE_LAYOUT_SHADER_READ_ONLY_OPTIMAL`.
pub const LAYOUT_SHADER_READ_ONLY: i32 = 5;
/// `VK_IMAGE_LAYOUT_TRANSFER_SRC_OPTIMAL`.
pub const LAYOUT_TRANSFER_SRC: i32 = 6;
/// `VK_IMAGE_LAYOUT_TRANSFER_DST_OPTIMAL`.
pub const LAYOUT_TRANSFER_DST: i32 = 7;
/// `VK_IMAGE_LAYOUT_UNDEFINED`.
const LAYOUT_UNDEFINED: i32 = 0;

const STAGE_TOP_OF_PIPE: u32 = 0x1;
const STAGE_TRANSFER: u32 = 0x1000;
const STAGE_BOTTOM_OF_PIPE: u32 = 0x2000;
const STAGE_HOST: u32 = 0x4000;
const ACCESS_TRANSFER_READ: u32 = 0x800;
const ACCESS_TRANSFER_WRITE: u32 = 0x1000;
const ACCESS_HOST_READ: u32 = 0x2000;
const QUEUE_GRAPHICS: u32 = 0x1;
const QUEUE_COMPUTE: u32 = 0x2;
const QUEUE_TRANSFER: u32 = 0x4;
const BUFFER_USAGE_TRANSFER_DST: u32 = 0x2;
/// `VK_COMMAND_POOL_CREATE_RESET_COMMAND_BUFFER_BIT`.
const POOL_RESET_COMMAND_BUFFER: u32 = 0x2;
const ASPECT_COLOR: u32 = 0x1;

/// The layout a copy reads a released image in: the release's own if a
/// copy may read in it, else `TRANSFER_SRC_OPTIMAL` — and `None` for a
/// release this device will not acquire from: `UNDEFINED` or
/// `PREINITIALIZED` (the frame would be discarded, or never was), and any
/// layout outside the colour layouts of core 1.0 (the two of
/// `synchronization2`, which this device does not enable, and those of
/// extensions it does not know).
#[must_use]
pub fn copy_layout(released: i32) -> Option<i32> {
    match released {
        LAYOUT_GENERAL | LAYOUT_TRANSFER_SRC => Some(released),
        LAYOUT_COLOR_ATTACHMENT | LAYOUT_SHADER_READ_ONLY | LAYOUT_TRANSFER_DST => {
            Some(LAYOUT_TRANSFER_SRC)
        }
        _ => None,
    }
}

/// Why a scanout read failed.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ScanoutError {
    /// Something this device will not or cannot do; nothing ran.
    #[error("{0}")]
    Refused(String),
    /// The GPU did not finish the copy in [`SCANOUT_WAIT`].
    #[error("the host GPU did not finish the scanout copy within {SCANOUT_WAIT:?}")]
    Timeout,
    /// The host device is lost; the scanout device must be made again.
    #[error("the scanout device is lost: {0}")]
    Lost(String),
}

fn refused(why: impl Into<String>) -> ScanoutError {
    ScanoutError::Refused(why.into())
}

/// One blob's import on the scanout device. See the module docs.
struct Target<H: HostVulkan> {
    resource_id: u32,
    /// The handle it was imported from, by identity, not held open: the
    /// import references the payload itself.
    shared: SharedRef,
    image_key: CanonicalImage,
    spec: crate::renderer::ScanoutBlobSpec,
    memory: Option<H::Memory>,
    image: Option<H::Image>,
    buffer: Option<H::Buffer>,
    staging: Option<H::Memory>,
    /// The staging buffer's pages: host-private, never a blob.
    pages: Option<PrivatePages>,
}

impl<H: HostVulkan> Target<H> {
    fn matches(&self, target: &ScanoutTarget) -> bool {
        self.resource_id == target.resource_id
            && self.shared.is(&target.handle)
            && self.image_key == target.image
            && self.spec == target.spec
    }

    /// Destroy every host object, children first. The caller has made sure
    /// no copy is in flight.
    fn destroy(mut self, host: &H, device: &H::Device) {
        if let Some(buffer) = self.buffer.take() {
            host.destroy_buffer(device, buffer);
        }
        if let Some(staging) = self.staging.take() {
            host.free_memory(device, staging);
        }
        if let Some(image) = self.image.take() {
            host.destroy_image(device, image);
        }
        if let Some(memory) = self.memory.take() {
            host.free_memory(device, memory);
        }
        drop(self.pages.take());
    }
}

/// The renderer's scanout device. See the module docs.
pub struct ScanoutDevice<H: HostVulkan> {
    host: Arc<H>,
    budget: Arc<PageBudget>,
    uuids: ([u8; 16], [u8; 16]),
    instance: Option<H::Instance>,
    device: Option<H::Device>,
    family: u32,
    queue: Option<H::Queue>,
    pool: Option<H::CommandPool>,
    cb: u64,
    fence: u64,
    import_alignment: u64,
    /// A submission the last read gave up waiting for.
    in_flight: bool,
    /// Least recently read first.
    targets: Vec<Target<H>>,
}

impl<H: HostVulkan> std::fmt::Debug for ScanoutDevice<H> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ScanoutDevice")
            .field("family", &self.family)
            .field("targets", &self.targets.len())
            .field("in_flight", &self.in_flight)
            .finish_non_exhaustive()
    }
}

/// `vkWaitForFences(1, fence, VK_TRUE, timeout)` on host handles.
fn wait_fence<H: HostVulkan>(host: &H, device: &H::Device, fence: u64, timeout: Duration) -> i32 {
    let mut command = Command::WaitForFences(WaitForFencesArgs {
        device: VkDevice(0),
        fence_count: 1,
        p_fences: Some(vec![VkFence(fence)]),
        wait_all: 1,
        timeout: u64::try_from(timeout.as_nanos()).unwrap_or(u64::MAX),
        ret: VK_ERROR_UNKNOWN,
    });
    match host.call(device, &mut command) {
        Ok(()) => generated::result_of(&command).unwrap_or(VK_ERROR_UNKNOWN),
        Err(_) => VK_ERROR_UNKNOWN,
    }
}

impl<H: HostVulkan> ScanoutDevice<H> {
    /// Open the scanout device on the physical device whose
    /// `(deviceUUID, driverUUID)` are `uuids`, its staging pages charged to
    /// `budget`.
    ///
    /// # Errors
    /// Why not, as a sentence: no such device, a missing extension, no
    /// queue, a driver refusal.
    pub fn open(
        host: Arc<H>,
        budget: Arc<PageBudget>,
        uuids: ([u8; 16], [u8; 16]),
    ) -> Result<Self, String> {
        let instance = host
            .create_instance(&InstanceRequest {
                application_name: Some("entangled venus scanout".into()),
                ..Default::default()
            })
            .map_err(|r| format!("vkCreateInstance for the scanout device failed ({r})"))?;
        let mut this = Self {
            host: Arc::clone(&host),
            budget,
            uuids,
            instance: Some(instance),
            device: None,
            family: 0,
            queue: None,
            pool: None,
            cb: 0,
            fence: 0,
            import_alignment: 4096,
            in_flight: false,
            targets: Vec::new(),
        };
        this.create_device()?;
        Ok(this)
    }

    /// The device, its queue, pool, command buffer and fence. On an error
    /// whatever was made is left for `Drop`.
    fn create_device(&mut self) -> Result<(), String> {
        let host = Arc::clone(&self.host);
        let instance = self.instance.as_ref().ok_or("no instance")?;
        let physicals = host
            .enumerate_physical_devices(instance)
            .map_err(|r| format!("vkEnumeratePhysicalDevices failed ({r})"))?;
        let found = physicals.into_iter().find_map(|physical| {
            let info = host.describe_physical_device(instance, physical);
            let guest = policy::expose(info.clone()).ok()?;
            (guest.uuids() == Some(self.uuids)).then_some((physical, info, guest))
        });
        let Some((physical, info, guest)) = found else {
            return Err("no host GPU has the exporting device's deviceUUID and driverUUID".into());
        };
        let needed = [
            policy::EXTERNAL_MEMORY_WIN32,
            policy::EXTERNAL_MEMORY_HOST,
            policy::QUEUE_FAMILY_FOREIGN_EXT,
        ];
        if let Some(missing) = needed
            .iter()
            .find(|name| !policy::has_extension(&info.extensions, name))
        {
            return Err(format!("the host GPU lacks {missing}"));
        }
        let family = [QUEUE_GRAPHICS, QUEUE_TRANSFER | QUEUE_COMPUTE]
            .iter()
            .find_map(|want| {
                info.queue_families
                    .iter()
                    .position(|f| f.queue_flags & want != 0 && f.queue_count > 0)
            })
            .and_then(|i| u32::try_from(i).ok())
            .ok_or("the host GPU has no queue family that can copy")?;
        self.family = family;
        self.import_alignment = guest.import_alignment.max(4096);
        let request = DeviceRequest {
            queues: vec![QueueRequest {
                flags: 0,
                family,
                priorities: vec![1.0],
            }],
            extensions: needed.iter().map(|n| (*n).to_owned()).collect(),
            features: None,
            chain: Vec::new(),
            group: None,
        };
        let device = host
            .create_device(instance, physical, &request)
            .map_err(|r| format!("vkCreateDevice for the scanout device failed ({r})"))?;
        self.queue = Some(host.device_queue(&device, 0, family, 0));
        let device = self.device.insert(device);
        let pool = host
            .create_command_pool(
                device,
                &VkCommandPoolCreateInfo {
                    flags: POOL_RESET_COMMAND_BUFFER,
                    queue_family_index: family,
                },
            )
            .map_err(|r| format!("vkCreateCommandPool failed ({r})"))?;
        self.pool = Some(pool);
        let mut allocate = Command::AllocateCommandBuffers(AllocateCommandBuffersArgs {
            device: VkDevice(0),
            p_allocate_info: Some(VkCommandBufferAllocateInfo {
                command_pool: crate::venus::protocol::VkCommandPool(pool.raw()),
                level: 0,
                command_buffer_count: 1,
            }),
            p_command_buffers: Some(vec![VkCommandBuffer(0)]),
            ret: VK_ERROR_UNKNOWN,
        });
        host.call(device, &mut allocate)
            .map_err(|e| format!("vkAllocateCommandBuffers: {e}"))?;
        self.cb = match (&allocate, generated::result_of(&allocate)) {
            (Command::AllocateCommandBuffers(a), Some(VK_SUCCESS)) => a
                .p_command_buffers
                .as_ref()
                .and_then(|c| c.first())
                .map_or(0, |c| c.0),
            (_, ret) => return Err(format!("vkAllocateCommandBuffers failed ({ret:?})")),
        };
        let mut create = Command::CreateFence(CreateFenceArgs {
            device: VkDevice(0),
            p_create_info: Some(VkFenceCreateInfo {
                p_next: Vec::new(),
                flags: 0,
            }),
            p_fence: Some(VkFence(0)),
            ret: VK_ERROR_UNKNOWN,
        });
        host.call(device, &mut create)
            .map_err(|e| format!("vkCreateFence: {e}"))?;
        self.fence = match (&create, generated::result_of(&create)) {
            (Command::CreateFence(a), Some(VK_SUCCESS)) => a.p_fence.map_or(0, |f| f.0),
            (_, ret) => return Err(format!("vkCreateFence failed ({ret:?})")),
        };
        if self.cb == 0 || self.fence == 0 {
            return Err("the scanout device's command buffer or fence is null".into());
        }
        tracing::info!(
            family,
            "venus scanout device created: handle blobs are read back on the host GPU"
        );
        Ok(())
    }

    /// The `(deviceUUID, driverUUID)` it was opened on.
    #[must_use]
    pub fn uuids(&self) -> ([u8; 16], [u8; 16]) {
        self.uuids
    }

    /// Blobs with an import right now.
    #[must_use]
    pub fn target_count(&self) -> usize {
        self.targets.len()
    }

    /// The queue family its queue is of.
    #[must_use]
    pub fn family(&self) -> u32 {
        self.family
    }

    /// Wait (bounded) for a submission an earlier read gave up on.
    fn settle(&mut self) -> Result<(), ScanoutError> {
        if !self.in_flight {
            return Ok(());
        }
        let device = self.device.as_ref().ok_or_else(|| refused("no device"))?;
        match wait_fence(&*self.host, device, self.fence, SCANOUT_WAIT) {
            VK_SUCCESS => {
                self.in_flight = false;
                Ok(())
            }
            VK_TIMEOUT => Err(ScanoutError::Timeout),
            VK_ERROR_DEVICE_LOST => Err(ScanoutError::Lost("the fence wait".into())),
            ret => Err(ScanoutError::Lost(format!(
                "vkWaitForFences answered {ret}"
            ))),
        }
    }

    /// Wait until nothing of ours is on the GPU, however long, before
    /// objects it may use are destroyed. Only ever our own copy.
    fn idle(&mut self) {
        if self.in_flight {
            if let Some(device) = self.device.as_ref() {
                let _ = self.host.device_wait_idle(device);
            }
            self.in_flight = false;
        }
    }

    /// Forget resource `resource_id`'s import.
    pub fn forget(&mut self, resource_id: u32) {
        if !self.targets.iter().any(|t| t.resource_id == resource_id) {
            return;
        }
        self.idle();
        let (gone, kept): (Vec<_>, Vec<_>) = std::mem::take(&mut self.targets)
            .into_iter()
            .partition(|t| t.resource_id == resource_id);
        self.targets = kept;
        if let Some(device) = self.device.as_ref() {
            for target in gone {
                target.destroy(&*self.host, device);
            }
        }
    }

    /// Forget every import (a device reset). The device itself stays.
    pub fn clear(&mut self) {
        self.idle();
        if let Some(device) = self.device.as_ref() {
            for target in self.targets.drain(..) {
                target.destroy(&*self.host, device);
            }
        }
        self.targets.clear();
    }

    /// Import `target`'s blob and create its image, replacing an older
    /// import of the same resource only once the new one is complete.
    ///
    /// # Errors
    /// Why not; the previous import, if any, is untouched.
    pub fn prepare(&mut self, target: &ScanoutTarget) -> Result<(), ScanoutError> {
        self.target_index(target).map(|_| ())
    }

    /// The index of `target`'s import, made if need be, moved to the back
    /// of the eviction order.
    fn target_index(&mut self, target: &ScanoutTarget) -> Result<usize, ScanoutError> {
        if let Some(at) = self.targets.iter().position(|t| t.matches(target)) {
            let hit = self.targets.remove(at);
            self.targets.push(hit);
            return Ok(self.targets.len() - 1);
        }
        let made = self.make_target(target)?;
        self.idle();
        // The old import of this resource (another layout) and, past the
        // cap, the least recently read go.
        let mut doomed = Vec::new();
        let mut kept = Vec::new();
        for t in std::mem::take(&mut self.targets) {
            if t.resource_id == target.resource_id {
                doomed.push(t);
            } else {
                kept.push(t);
            }
        }
        while kept.len() >= MAX_SCANOUT_TARGETS {
            doomed.push(kept.remove(0));
        }
        self.targets = kept;
        if let Some(device) = self.device.as_ref() {
            for t in doomed {
                t.destroy(&*self.host, device);
            }
        }
        self.targets.push(made);
        Ok(self.targets.len() - 1)
    }

    /// A new import of `target`, every object made or none.
    fn make_target(&mut self, target: &ScanoutTarget) -> Result<Target<H>, ScanoutError> {
        let host = Arc::clone(&self.host);
        let export = target
            .handle
            .0
            .downcast_ref::<HandleExport<H>>()
            .ok_or_else(|| refused("a handle this host did not make"))?;
        if export.uuids != self.uuids {
            return Err(refused("the blob was exported by another GPU or driver"));
        }
        let device = self.device.as_ref().ok_or_else(|| refused("no device"))?;
        let mut made = Target::<H> {
            resource_id: target.resource_id,
            shared: SharedRef::of(&target.handle),
            image_key: target.image.clone(),
            spec: target.spec,
            memory: None,
            image: None,
            buffer: None,
            staging: None,
            pages: None,
        };
        let result = self.fill_target(&mut made, export, device);
        match result {
            Ok(()) => Ok(made),
            Err(error) => {
                made.destroy(&host, device);
                Err(error)
            }
        }
    }

    fn fill_target(
        &self,
        made: &mut Target<H>,
        export: &HandleExport<H>,
        device: &H::Device,
    ) -> Result<(), ScanoutError> {
        let host = &*self.host;
        let memory = host
            .allocate_memory(
                device,
                &MemoryRequest {
                    size: export.size,
                    type_index: export.type_index,
                    import: None,
                    flags: None,
                    dedicated: None,
                    export_handle: false,
                    import_handle: Some(Arc::clone(&export.shared)),
                },
            )
            .map_err(|r| {
                refused(format!(
                    "vkAllocateMemory importing the blob's handle ({r})"
                ))
            })?;
        let memory = made.memory.insert(memory);
        // Exactly the canonical image — the create info the exporter's and
        // every importer's come from.
        let info = made.image_key.create_info(LAYOUT_UNDEFINED);
        let image = host
            .create_image(device, &info, ResourceMemory::Handle)
            .map_err(|r| refused(format!("vkCreateImage of the canonical image ({r})")))?;
        made.image = Some(image);
        let mut req = VkMemoryRequirements2::default();
        host.image_memory_requirements(device, image, None, &mut req);
        let req = req.memory_requirements;
        if req.size > export.size || req.memory_type_bits & (1u32 << (export.type_index & 31)) == 0
        {
            return Err(refused(format!(
                "the canonical image needs {:#x} bytes of types {:#x}; the export is {:#x} of \
                 type {}",
                req.size, req.memory_type_bits, export.size, export.type_index
            )));
        }
        let ret = host.bind_image_memory(
            device,
            &[ImageBind {
                image,
                memory: &*memory,
                offset: 0,
                plane: None,
            }],
        );
        if ret != VK_SUCCESS {
            return Err(refused(format!("vkBindImageMemory of the import ({ret})")));
        }

        // The staging buffer: `width × height × 4` bytes of our pages.
        let bytes = u64::from(made.spec.width)
            .saturating_mul(u64::from(made.spec.height))
            .saturating_mul(4);
        let buffer_info = VkBufferCreateInfo {
            p_next: Vec::new(),
            flags: 0,
            size: bytes,
            usage: BUFFER_USAGE_TRANSFER_DST,
            sharing_mode: 0,
            queue_family_index_count: 0,
            p_queue_family_indices: None,
        };
        let buffer = host
            .create_buffer(device, &buffer_info, ResourceMemory::HostPages)
            .map_err(|r| refused(format!("vkCreateBuffer of the staging buffer ({r})")))?;
        made.buffer = Some(buffer);
        let mut req = VkMemoryRequirements2::default();
        host.buffer_memory_requirements(device, buffer, &mut req);
        let req = req.memory_requirements;
        let align = self
            .import_alignment
            .max(req.alignment)
            .checked_next_power_of_two()
            .unwrap_or(u64::MAX);
        // Host-private: the guest never maps these, so the readback may be
        // one plain copy (`PrivatePages::read_rows`).
        let pages = PrivatePages::for_memory(req.size.max(bytes), align, &self.budget)
            .map_err(|e| refused(format!("staging pages: {e}")))?;
        let types = host
            .host_pointer_types(device, pages.import_pages())
            .map_err(|r| refused(format!("vkGetMemoryHostPointerPropertiesEXT ({r})")))?
            & req.memory_type_bits;
        if types == 0 {
            return Err(refused(
                "no memory type takes our pages for the staging buffer",
            ));
        }
        let staging = host
            .allocate_memory(
                device,
                &MemoryRequest {
                    size: pages.mapped_len(),
                    type_index: types.trailing_zeros(),
                    import: Some(Arc::clone(pages.import_pages())),
                    flags: None,
                    dedicated: None,
                    export_handle: false,
                    import_handle: None,
                },
            )
            .map_err(|r| {
                refused(format!(
                    "vkAllocateMemory importing the staging pages ({r})"
                ))
            })?;
        made.pages = Some(pages);
        let staging = made.staging.insert(staging);
        let ret = host.bind_buffer_memory(device, &[(buffer, &*staging, 0)]);
        if ret != VK_SUCCESS {
            return Err(refused(format!(
                "vkBindBufferMemory of the staging buffer ({ret})"
            )));
        }
        Ok(())
    }

    /// Read `rect` of `target`'s image as packed BGRA into `out`: see the
    /// module docs.
    ///
    /// # Errors
    /// [`ScanoutError`]; `out` is then unspecified.
    pub fn read(
        &mut self,
        target: &ScanoutTarget,
        release: ScanoutRelease,
        rect: Rect,
        out: &mut Vec<u8>,
    ) -> Result<(), ScanoutError> {
        use policy::{QUEUE_FAMILY_EXTERNAL, QUEUE_FAMILY_FOREIGN};
        let copy = copy_layout(release.layout).ok_or_else(|| {
            refused(format!(
                "the guest released its image in layout {}, which this device does not acquire \
                 from",
                release.layout
            ))
        })?;
        if release.family != QUEUE_FAMILY_FOREIGN && release.family != QUEUE_FAMILY_EXTERNAL {
            return Err(refused(
                "the release is not to a queue family outside the instance",
            ));
        }
        if !target.image.is_bgra8() {
            return Err(refused("the image is not BGRA-ordered"));
        }
        if !rect.fits_within(target.spec.width, target.spec.height) {
            return Err(refused("the rect is outside the image"));
        }
        // The copy below packs the rect at its own width (`bufferRowLength`
        // 0), so the staging rows are `row` bytes apart: one block.
        let row = usize::try_from(u64::from(rect.width) * 4)
            .map_err(|_| refused("a row larger than this host addresses"))?;
        let rows = usize::try_from(rect.height)
            .map_err(|_| refused("a rect larger than this host addresses"))?;
        self.settle()?;
        let at = self.target_index(target)?;
        let t = self.targets.get(at).ok_or_else(|| refused("no import"))?;
        let (Some(image), Some(buffer), Some(pages)) = (t.image, t.buffer, t.pages.clone()) else {
            return Err(refused("an incomplete import"));
        };
        let device = self.device.as_ref().ok_or_else(|| refused("no device"))?;
        let queue = self.queue.ok_or_else(|| refused("no queue"))?;
        let (cb, fence, family) = (self.cb, self.fence, self.family);
        let range = VkImageSubresourceRange {
            aspect_mask: ASPECT_COLOR,
            base_mip_level: 0,
            level_count: 1,
            base_array_layer: 0,
            layer_count: 1,
        };
        let barrier =
            |src: (u32, u32), dst: (u32, u32), layouts: (i32, i32), families: (u32, u32)| {
                VkImageMemoryBarrier {
                    p_next: Vec::new(),
                    src_access_mask: src.1,
                    dst_access_mask: dst.1,
                    old_layout: layouts.0,
                    new_layout: layouts.1,
                    src_queue_family_index: families.0,
                    dst_queue_family_index: families.1,
                    image: VkImage(image.raw()),
                    subresource_range: range.clone(),
                }
            };
        let acquire = barrier(
            (STAGE_TOP_OF_PIPE, 0),
            (STAGE_TRANSFER, ACCESS_TRANSFER_READ),
            (release.layout, copy),
            (release.family, family),
        );
        let give_back = barrier(
            (STAGE_TRANSFER, 0),
            (STAGE_BOTTOM_OF_PIPE, 0),
            (copy, release.layout),
            (family, release.family),
        );
        let visible = VkBufferMemoryBarrier {
            p_next: Vec::new(),
            src_access_mask: ACCESS_TRANSFER_WRITE,
            dst_access_mask: ACCESS_HOST_READ,
            src_queue_family_index: policy::QUEUE_FAMILY_IGNORED,
            dst_queue_family_index: policy::QUEUE_FAMILY_IGNORED,
            buffer: VkBuffer(buffer.raw()),
            offset: 0,
            size: u64::MAX,
        };
        let commands = vec![
            Command::ResetFences(ResetFencesArgs {
                device: VkDevice(0),
                fence_count: 1,
                p_fences: Some(vec![VkFence(fence)]),
                ret: VK_ERROR_UNKNOWN,
            }),
            Command::ResetCommandBuffer(ResetCommandBufferArgs {
                command_buffer: VkCommandBuffer(cb),
                flags: 0,
                ret: VK_ERROR_UNKNOWN,
            }),
            Command::BeginCommandBuffer(BeginCommandBufferArgs {
                command_buffer: VkCommandBuffer(cb),
                p_begin_info: Some(VkCommandBufferBeginInfo {
                    p_next: Vec::new(),
                    flags: 0x1, // ONE_TIME_SUBMIT
                    p_inheritance_info: None,
                }),
                ret: VK_ERROR_UNKNOWN,
            }),
            Command::CmdPipelineBarrier(CmdPipelineBarrierArgs {
                command_buffer: VkCommandBuffer(cb),
                src_stage_mask: STAGE_TOP_OF_PIPE,
                dst_stage_mask: STAGE_TRANSFER,
                dependency_flags: 0,
                memory_barrier_count: 0,
                p_memory_barriers: None,
                buffer_memory_barrier_count: 0,
                p_buffer_memory_barriers: None,
                image_memory_barrier_count: 1,
                p_image_memory_barriers: Some(vec![acquire]),
            }),
            Command::CmdCopyImageToBuffer(CmdCopyImageToBufferArgs {
                command_buffer: VkCommandBuffer(cb),
                src_image: VkImage(image.raw()),
                src_image_layout: copy,
                dst_buffer: VkBuffer(buffer.raw()),
                region_count: 1,
                p_regions: Some(vec![VkBufferImageCopy {
                    buffer_offset: 0,
                    buffer_row_length: 0,
                    buffer_image_height: 0,
                    image_subresource: VkImageSubresourceLayers {
                        aspect_mask: ASPECT_COLOR,
                        mip_level: 0,
                        base_array_layer: 0,
                        layer_count: 1,
                    },
                    image_offset: VkOffset3D {
                        x: i32::try_from(rect.x).unwrap_or(i32::MAX),
                        y: i32::try_from(rect.y).unwrap_or(i32::MAX),
                        z: 0,
                    },
                    image_extent: VkExtent3D {
                        width: rect.width,
                        height: rect.height,
                        depth: 1,
                    },
                }]),
            }),
            Command::CmdPipelineBarrier(CmdPipelineBarrierArgs {
                command_buffer: VkCommandBuffer(cb),
                src_stage_mask: STAGE_TRANSFER,
                dst_stage_mask: STAGE_BOTTOM_OF_PIPE | STAGE_HOST,
                dependency_flags: 0,
                memory_barrier_count: 0,
                p_memory_barriers: None,
                buffer_memory_barrier_count: 1,
                p_buffer_memory_barriers: Some(vec![visible]),
                image_memory_barrier_count: 1,
                p_image_memory_barriers: Some(vec![give_back]),
            }),
            Command::EndCommandBuffer(EndCommandBufferArgs {
                command_buffer: VkCommandBuffer(cb),
                ret: VK_ERROR_UNKNOWN,
            }),
        ];
        for mut command in commands {
            let name = command.name();
            self.host
                .call(device, &mut command)
                .map_err(|e| refused(format!("{name}: {e}")))?;
            match generated::result_of(&command) {
                None | Some(VK_SUCCESS) => {}
                Some(VK_ERROR_DEVICE_LOST) => return Err(ScanoutError::Lost(name.into())),
                Some(ret) => return Err(refused(format!("{name} failed ({ret})"))),
            }
        }
        let mut submit = Command::QueueSubmit(QueueSubmitArgs {
            queue: VkQueue(queue.raw()),
            submit_count: 1,
            p_submits: Some(vec![VkSubmitInfo {
                p_next: Vec::new(),
                wait_semaphore_count: 0,
                p_wait_semaphores: None,
                p_wait_dst_stage_mask: None,
                command_buffer_count: 1,
                p_command_buffers: Some(vec![VkCommandBuffer(cb)]),
                signal_semaphore_count: 0,
                p_signal_semaphores: None,
            }]),
            fence: VkFence(fence),
            ret: VK_ERROR_UNKNOWN,
        });
        self.host
            .call(device, &mut submit)
            .map_err(|e| refused(format!("vkQueueSubmit: {e}")))?;
        match generated::result_of(&submit) {
            Some(VK_SUCCESS) => {}
            Some(VK_ERROR_DEVICE_LOST) => return Err(ScanoutError::Lost("vkQueueSubmit".into())),
            ret => return Err(refused(format!("vkQueueSubmit failed ({ret:?})"))),
        }
        match wait_fence(&*self.host, device, fence, SCANOUT_WAIT) {
            VK_SUCCESS => {}
            VK_TIMEOUT => {
                self.in_flight = true;
                return Err(ScanoutError::Timeout);
            }
            VK_ERROR_DEVICE_LOST => {
                self.in_flight = true;
                return Err(ScanoutError::Lost("the fence wait".into()));
            }
            ret => {
                self.in_flight = true;
                return Err(ScanoutError::Lost(format!(
                    "vkWaitForFences answered {ret}"
                )));
            }
        }
        // The fence has signalled: the copy is done and visible to the host,
        // and nothing else writes these pages — `read_rows`' precondition.
        pages
            .read_rows(0, row as u64, row, rows, out)
            .map_err(|e| refused(format!("the staging pages: {e}")))?;
        Ok(())
    }
}

impl<H: HostVulkan> Drop for ScanoutDevice<H> {
    fn drop(&mut self) {
        let host = Arc::clone(&self.host);
        if let Some(device) = self.device.take() {
            // Whatever is in flight is ours; a lost device returns at once.
            let _ = host.device_wait_idle(&device);
            for target in self.targets.drain(..) {
                target.destroy(&host, &device);
            }
            if self.fence != 0 {
                host.destroy_object(&device, Kind::Fence, self.fence);
            }
            // The command buffer goes with its pool.
            if let Some(pool) = self.pool.take() {
                host.destroy_command_pool(&device, pool);
            }
            host.destroy_device(device);
        }
        if let Some(instance) = self.instance.take() {
            host.destroy_instance(instance);
        }
    }
}
