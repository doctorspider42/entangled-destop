//! The virtio-gpu 2D device (backlog MVP-801…810).
//!
//! Request layout on the wire (VirtIO spec 1.2, section 5.7.6): one descriptor
//! chain of
//!
//! * device-readable bytes — a 24-byte [`CtrlHdr`] followed by the command
//!   body (and, for `RESOURCE_ATTACH_BACKING`, the entry array),
//! * device-writable bytes — where the device writes the response header and,
//!   for `GET_DISPLAY_INFO`, the 384-byte pmodes array.
//!
//! Both halves may be split over any number of descriptors, so the device
//! gathers the request into one buffer and scatters the response back out.
//!
//! # The MVP command set
//!
//! ```text
//! GET_DISPLAY_INFO ──▶ one enabled scanout, sized from the host window
//! RESOURCE_CREATE_2D ──▶ host BGRA image, B8G8R8A8_UNORM only
//! RESOURCE_ATTACH_BACKING ──▶ remember the guest page list (not read yet)
//! SET_SCANOUT ──▶ bind a resource region to scanout 0 (resolution follows)
//! TRANSFER_TO_HOST_2D ──▶ guest pages ──▶ host image (the only guest read)
//! RESOURCE_FLUSH ──▶ dirty rect ──▶ ScanoutSink ──▶ window
//! RESOURCE_UNREF / RESOURCE_DETACH_BACKING ──▶ teardown
//! anything else ──▶ ERR_UNSPEC
//! ```
//!
//! # Failure policy
//!
//! Every value in a chain is guest-controlled. A command the guest malformed is
//! answered **in band** with the matching `VIRTIO_GPU_RESP_ERR_*` code (see
//! [`CommandError`]) and [`GpuDevice::notify`] still returns `Ok`, so one bad
//! command never takes the device down. Only a chain so broken that there is
//! nowhere to put a response (no device-writable bytes, an unwalkable chain) is
//! dropped with a zero-length used-ring entry. Nothing here can panic on guest
//! input: no `unwrap`, no unchecked slice index, no unchecked arithmetic on a
//! guest value.

use std::sync::Arc;
use std::time::{Duration, Instant};

use virtio_core::chain::{self, Segment};
use virtio_core::device::{DeviceError, DeviceResources, DeviceType, HostWaker, VirtioDevice};
use virtio_core::interrupt::Interrupt;
use virtio_core::{GuestMem, MAX_QUEUE_SIZE, VIRTIO_F_VERSION_1};
use virtio_queue::{Queue, QueueT};
use vm_memory::{Bytes, GuestAddress};

use crate::blob::{BlobSupport, BlobTable, MAX_BLOB_ENTRIES};
use crate::error::CommandError;
use crate::fence::{FenceQueue, MAX_PENDING_FENCES};
use crate::pacing::FramePacing;
use crate::protocol::{
    capset_info_body, cmd, config_bytes, display_info_body, edid_body, map_info_body, resp,
    AttachBacking, CmdSubmit3d, CtrlHdr, CtxCreate, CtxResource, DisplayOne, GetCapset,
    GetCapsetInfo, GetEdid, MemEntry, Rect, ResourceCreate2d, ResourceCreate3d, ResourceCreateBlob,
    ResourceFlush, ResourceMapBlob, ResourceUnref, SetScanout, SetScanoutBlob, Transfer3d,
    TransferToHost2d, UpdateCursor, BLOB_MEM_GUEST, CONFIG_LEN, MEM_ENTRY_LEN,
};
use crate::renderer::FenceOutcome;
use crate::renderer::{Gpu3d, Renderer3d, MAX_SUBMIT_BYTES};
use crate::resource::{ResourceTable, MAX_BACKING_ENTRIES};
use crate::sink::ScanoutSink;
use crate::{
    MAX_CURSOR_DIM, VIRTIO_GPU_F_CONTEXT_INIT, VIRTIO_GPU_F_EDID, VIRTIO_GPU_F_RESOURCE_BLOB,
    VIRTIO_GPU_F_VIRGL,
};

/// controlq and cursorq, in queue order (spec section 5.7.2).
pub const NUM_QUEUES: usize = 2;
/// Index of the control queue — all 2D commands arrive here.
pub const CONTROL_QUEUE: u16 = 0;
/// Index of the cursor queue (MVP-812: drained, not yet acted upon).
pub const CURSOR_QUEUE: u16 = 1;

static QUEUE_MAX_SIZES: [u16; NUM_QUEUES] = [MAX_QUEUE_SIZE, MAX_QUEUE_SIZE];

/// Scanouts (virtual displays) the device exposes. One window, one scanout.
pub const NUM_SCANOUTS: u32 = 1;

/// `VIRTIO_GPU_SHM_ID_HOST_VISIBLE`: the shared-memory region id blob
/// mappings land in (VEN-2001). The spec reserves 0 for "undefined", so
/// host-visible is 1.
pub const VIRTIO_GPU_SHM_ID_HOST_VISIBLE: u8 = 1;

/// Capability sets in 2D mode. Zero: without a renderer the guest never asks
/// for `GET_CAPSET_INFO`. With one, the count comes from [`Gpu3d`].
pub const NUM_CAPSETS: u32 = 0;

/// Largest command the 2D device will gather, i.e. an attach-backing carrying
/// the maximum entry count. Bounds the staging buffer a guest can make the
/// host allocate (~256 KiB).
pub const MAX_COMMAND_BYTES: usize =
    AttachBacking::LEN + MAX_BACKING_ENTRIES as usize * MEM_ENTRY_LEN;

/// Largest command with a 3D renderer attached: a `SUBMIT_3D` carrying the
/// full stream budget ([`MAX_SUBMIT_BYTES`]). Still a named, tested bound —
/// just a bigger one, because command streams dwarf backing lists.
pub const MAX_COMMAND_BYTES_3D: usize = CmdSubmit3d::LEN + MAX_SUBMIT_BYTES;

/// Largest command once blob resources are offered: a `RESOURCE_CREATE_BLOB`
/// carrying the maximum page list (VEN-2001).
///
/// It is 24 bytes longer than the 2D attach-backing bound — the fixed part of
/// a create-blob is bigger — which is exactly why it is its own constant
/// rather than a reuse: a device that offered blob and kept
/// [`MAX_COMMAND_BYTES`] would reject a *legal* maximum-length command by 24
/// bytes, which is the kind of off-by-one that only shows up under a real
/// guest.
pub const MAX_COMMAND_BYTES_BLOB: usize =
    ResourceCreateBlob::LEN + MAX_BLOB_ENTRIES as usize * MEM_ENTRY_LEN;

// The 3D cap must never regress below the 2D one, or attach-backing commands
// would start failing the moment a renderer is attached.
const _: () = assert!(MAX_COMMAND_BYTES_3D > MAX_COMMAND_BYTES);
const _: () = assert!(MAX_COMMAND_BYTES_BLOB > MAX_COMMAND_BYTES);
const _: () = assert!(MAX_COMMAND_BYTES_3D > MAX_COMMAND_BYTES_BLOB);

/// Hard bound on how many chains one notification processes, so a guest that
/// keeps refilling the ring from another vCPU cannot pin this thread forever.
pub const CHAINS_PER_NOTIFY: usize = 4 * MAX_QUEUE_SIZE as usize;

/// How long a deferred fenced response may wait for its host fence before the
/// device gives up and answers anyway (ADR-0004 phase 2's no-wedge rule).
///
/// A fence that never retires means the host GL stack is wedged or gone; the
/// guest must not inherit that. Two seconds is far past any plausible frame
/// (a 60 Hz compositor's fences retire in ~16 ms, a heavy shader compile in
/// tens of ms) and far below the guest's own DRM timeouts, so a timeout here
/// is always a host fault worth logging.
pub const FENCE_TIMEOUT: Duration = Duration::from_secs(2);

/// Environment variable that forces a fence mode, for the phase-2 before/after
/// measurement and for a host where deferral misbehaves. Read by the app
/// layer (`entangled run`), the same way `ENTANGLED_QUEUE_NOTIFY` is.
pub const FENCE_MODE_ENV: &str = "ENTANGLED_GPU_FENCES";

/// Whether fenced responses may be deferred (ADR-0004 phase 2) or complete as
/// soon as the command executes (phase 1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum FenceMode {
    /// Hold the response until the host fence retires — the default, and the
    /// whole point of phase 2.
    #[default]
    Deferred,
    /// Answer immediately, as phase 1 did. Spec-legal (the command *has*
    /// executed), it just gives the guest no pipelining — which is exactly
    /// what makes it the baseline to measure against.
    Synchronous,
}

impl FenceMode {
    /// Reads [`FENCE_MODE_ENV`]; anything unrecognised keeps the default.
    pub fn from_env() -> Self {
        match std::env::var(FENCE_MODE_ENV) {
            Ok(value) => Self::parse(&value).unwrap_or_else(|| {
                tracing::warn!(
                    var = FENCE_MODE_ENV,
                    value = %value,
                    "unrecognised virtio-gpu fence mode, using the default"
                );
                Self::default()
            }),
            Err(_) => Self::default(),
        }
    }

    /// Parses the accepted spellings; `None` for anything else.
    pub fn parse(value: &str) -> Option<Self> {
        let value = value.trim();
        if value.eq_ignore_ascii_case("deferred") || value.eq_ignore_ascii_case("async") {
            Some(Self::Deferred)
        } else if value.eq_ignore_ascii_case("sync") || value.eq_ignore_ascii_case("synchronous") {
            Some(Self::Synchronous)
        } else {
            None
        }
    }

    pub fn is_deferred(self) -> bool {
        matches!(self, Self::Deferred)
    }
}

/// The 3D commands whose fences are worth deferring: the ones that put work
/// on the host GL timeline.
///
/// Everything else a guest may fence (capset queries, resource creation,
/// scanout binding) either touches no timeline or — like `RESOURCE_FLUSH` on
/// the readback path — already finished synchronously by the time the
/// response is built, so deferring it would add latency and no correctness.
const FENCED_3D_COMMANDS: [u32; 3] = [
    cmd::SUBMIT_3D,
    cmd::TRANSFER_TO_HOST_3D,
    cmd::TRANSFER_FROM_HOST_3D,
];

/// What [`GpuDevice::handle_command`] did with a chain.
enum Served {
    /// The response is written; `.0` bytes went into the used ring.
    Done(u32),
    /// The response is held in [`GpuDevice::pending_fences`] until the host
    /// fence retires, so the chain must *not* be returned to the used ring
    /// yet.
    Deferred,
}

/// A response waiting for its host fence (ADR-0004 phase 2).
///
/// It carries a *snapshot* of the chain's device-writable segments rather
/// than re-walking the descriptor table at completion time: the guest owns
/// that table and may have rewritten it since, and a device that answers into
/// wherever the descriptors point *now* would let a guest redirect its own
/// completion. The walk that produced these segments was bounded by
/// `MAX_DESC_CHAIN_LEN`, so the snapshot is too.
struct PendingResponse {
    head: u16,
    writable: Vec<Segment>,
    hdr: CtrlHdr,
    body: Vec<u8>,
}

/// Counters for the fence path: diagnostics, and the evidence for phase 2's
/// before/after measurement.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct FenceStats {
    /// Fenced responses that were held back for a host fence.
    pub deferred: u64,
    /// Deferred responses completed by a retiring host fence.
    pub retired: u64,
    /// Fenced commands answered immediately because the renderer completes
    /// fences synchronously (phase 1's model, and every 2D device).
    pub synchronous: u64,
    /// Fenced commands answered immediately because
    /// [`MAX_PENDING_FENCES`] were already outstanding — the guest asked for
    /// more in-flight fences than the host will hold.
    pub over_cap: u64,
    /// Deferred responses completed by [`FENCE_TIMEOUT`] instead of by a
    /// fence. Non-zero means the host renderer stalled or died.
    pub timed_out: u64,
    /// Deepest the pending table has been.
    pub peak_pending: usize,
    /// Summed host-fence wait of every retired response, in microseconds —
    /// with `retired`, the mean time the guest spent pipelined behind the
    /// host GPU instead of blocked on it.
    pub wait_us_total: u64,
    /// Longest single host-fence wait, in microseconds.
    pub wait_us_max: u64,
}

impl FenceStats {
    /// Mean host-fence wait of the responses that retired, in microseconds.
    pub fn mean_wait_us(&self) -> u64 {
        if self.retired == 0 {
            return 0;
        }
        self.wait_us_total / self.retired
    }
}

/// Where the pixels of a bound scanout actually live — which decides how
/// `RESOURCE_FLUSH` gets at them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ScanoutSource {
    /// The host 2D table's image.
    TwoD,
    /// The 3D renderer (flushes read back through it).
    ThreeD,
    /// A blob resource (VEN-2001): the pixels are guest pages, and the layout
    /// came from `SET_SCANOUT_BLOB` rather than from a resource's own
    /// geometry — the blob has none.
    Blob {
        /// Bytes per row, guest-declared and validated against the mode.
        stride: u32,
        /// Byte offset of the plane inside the blob.
        offset: u32,
    },
}

/// What the guest bound to scanout 0.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ScanoutBinding {
    resource_id: u32,
    /// Region of the resource the scanout shows; its size is the guest mode.
    rect: Rect,
    /// Which half of the device owns the pixels.
    source: ScanoutSource,
}

impl ScanoutBinding {
    /// Whether the 3D renderer owns this binding — the question GPU-012's
    /// degrade path asks.
    fn is_three_d(&self) -> bool {
        matches!(self.source, ScanoutSource::ThreeD)
    }
}

/// One control-command result: a response code plus an optional body.
struct Reply {
    code: u32,
    /// Empty for every `OK_NODATA`/error reply, so the hot path never
    /// allocates; only `GET_DISPLAY_INFO` carries bytes.
    body: Vec<u8>,
}

impl Reply {
    fn ok() -> Self {
        Self {
            code: resp::OK_NODATA,
            body: Vec::new(),
        }
    }

    fn error(code: u32) -> Self {
        Self {
            code,
            body: Vec::new(),
        }
    }
}

/// The virtio-gpu 2D device.
///
/// Construct it with the host display handle (see [`ScanoutSink`]) and hand it
/// to a transport:
///
/// ```no_run
/// # fn wire<S: virtio_gpu::ScanoutSink + 'static>(display: S) -> Box<dyn virtio_core::VirtioDevice> {
/// Box::new(virtio_gpu::GpuDevice::new(display))
/// # }
/// ```
pub struct GpuDevice<S: ScanoutSink> {
    display: S,
    resources: ResourceTable,
    /// The 3D half (ADR-0004): validation front + host renderer. `None` in
    /// the 2D-only device, and then no 3D feature or command exists.
    three_d: Option<Gpu3d>,
    /// Blob resources (VEN-2001). Always present so the routing code has one
    /// shape; empty and inert unless the renderer declared blob support, and
    /// then `VIRTIO_GPU_F_RESOURCE_BLOB` is not offered either.
    blobs: BlobTable,
    /// What the renderer said it can do with blobs, cached so the hot path
    /// does not go through the trait object per command.
    blob_support: BlobSupport,
    scanout: Option<ScanoutBinding>,
    /// `events_read` of `struct virtio_gpu_config`. Nothing raises events in
    /// the MVP (they are for hot-plugged displays / EDID changes), so this
    /// stays zero; the write-to-clear path is implemented anyway.
    events_read: u32,
    features: u64,
    acked_features: u64,
    /// Staging buffer for the gathered request, reused across commands.
    req_buf: Vec<u8>,
    /// Staging buffer for a gathered partial-width flush rect.
    flush_buf: Vec<u8>,
    /// Cursor-queue commands processed so far (MVP-812), for diagnostics.
    cursor_commands: u64,
    /// Fenced responses held back for the host renderer (ADR-0004 phase 2).
    /// Always empty on a 2D device and on a renderer that fences
    /// synchronously.
    pending_fences: FenceQueue<PendingResponse>,
    fence_stats: FenceStats,
    /// Watchdog deadline for a deferred response ([`FENCE_TIMEOUT`] unless
    /// [`GpuDevice::set_fence_timeout`] changed it).
    fence_timeout: Duration,
    /// Whether fenced responses may be held back at all.
    fence_mode: FenceMode,
    /// Set when the host renderer has been found dead and the device has
    /// degraded to 2D (GPU-012).
    renderer_lost: bool,
    /// Whether the driver has already been told about that loss, so
    /// `DEVICE_NEEDS_RESET` is raised exactly once.
    renderer_loss_reported: bool,
    /// Frame-interval statistics of the scanout path (phase 2 measurement).
    pacing: FramePacing,
    /// Where `--frame-stats` mirrors those statistics as JSON, if anywhere.
    frame_stats: Option<std::path::PathBuf>,
    /// The host waker the machine layer gave this device, kept so a renderer
    /// attached later — and the crash-containment path — can use it.
    waker: Option<Arc<dyn HostWaker>>,

    // Set on activate(), cleared on reset().
    mem: Option<Arc<GuestMem>>,
    control: Option<Queue>,
    cursor: Option<Queue>,
    interrupt: Option<Arc<dyn Interrupt>>,
}

impl<S: ScanoutSink> std::fmt::Debug for GpuDevice<S> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GpuDevice")
            .field("resources", &self.resources.len())
            .field("scanout", &self.scanout)
            .field("activated", &self.control.is_some())
            .finish_non_exhaustive()
    }
}

impl<S: ScanoutSink> GpuDevice<S> {
    /// Builds the device around a host display.
    ///
    /// `display` is normally `display::DisplayHandle` (cloneable and `Send`, so
    /// the window keeps its own copy); tests use
    /// `DisplayHandle::detached(w, h)` or any other [`ScanoutSink`].
    pub fn new(display: S) -> Self {
        Self {
            display,
            resources: ResourceTable::new(),
            three_d: None,
            blobs: BlobTable::new(0),
            blob_support: BlobSupport::NONE,
            scanout: None,
            events_read: 0,
            // EDID (MVP-811) because GNOME/mutter builds its outputs from it;
            // no resource UUID / blob resources. VIRGL is added by
            // [`Self::with_renderer`] only.
            features: VIRTIO_F_VERSION_1 | VIRTIO_GPU_F_EDID,
            acked_features: 0,
            req_buf: Vec::new(),
            flush_buf: Vec::new(),
            cursor_commands: 0,
            pending_fences: FenceQueue::new(),
            fence_stats: FenceStats::default(),
            fence_timeout: FENCE_TIMEOUT,
            fence_mode: FenceMode::default(),
            renderer_lost: false,
            renderer_loss_reported: false,
            pacing: FramePacing::new(),
            frame_stats: None,
            waker: None,
            mem: None,
            control: None,
            cursor: None,
            interrupt: None,
        }
    }

    /// Builds the device with a host 3D renderer attached (ADR-0004): offers
    /// `VIRTIO_GPU_F_VIRGL`, serves the renderer's capsets and accepts the 3D
    /// command set. `renderer` must actually render — hosts without one use
    /// [`Self::new`] so the guest stays on its own software GL.
    pub fn with_renderer(display: S, renderer: Box<dyn Renderer3d>) -> Self {
        let mut device = Self::new(display);
        device.features |= VIRTIO_GPU_F_VIRGL;
        let gpu = Gpu3d::new(renderer);

        // VEN-2001: blob resources are offered only when the renderer can
        // actually serve them. A guest that negotiates the bit and then gets
        // ERR_UNSPEC for every create is worse off than one that never saw it.
        let support = gpu.blob_support();
        if support.any() {
            device.features |= VIRTIO_GPU_F_RESOURCE_BLOB;
            device.blobs = BlobTable::new(support.host_visible_bytes.unwrap_or(0));
        }
        device.blob_support = support;

        // VEN-2002: `context_init` is only meaningful when there is a context
        // type beyond classic virgl to select.
        if gpu.has_context_types() {
            device.features |= VIRTIO_GPU_F_CONTEXT_INIT;
        }
        tracing::info!(
            blob_guest = support.guest,
            blob_host3d = support.host3d,
            host_visible_bytes = support.host_visible_bytes.unwrap_or(0),
            venus = gpu.serves_venus(),
            capsets = gpu.num_capsets(),
            "virtio-gpu 3D renderer attached"
        );
        device.three_d = Some(gpu);
        device
    }

    /// Largest command this device gathers — the 3D bound when a renderer is
    /// attached ([`MAX_COMMAND_BYTES_3D`]), the blob bound when blobs are
    /// offered without one, the 2D bound otherwise.
    fn max_command_bytes(&self) -> usize {
        if self.three_d.is_some() {
            MAX_COMMAND_BYTES_3D
        } else if self.blob_support.any() {
            MAX_COMMAND_BYTES_BLOB
        } else {
            MAX_COMMAND_BYTES
        }
    }

    /// The blob half, or the in-band error every blob command gets on a device
    /// that never offered [`VIRTIO_GPU_F_RESOURCE_BLOB`].
    fn blobs_mut(&mut self, kind: u32) -> Result<&mut BlobTable, CommandError> {
        if !self.blob_support.any() {
            return Err(CommandError::UnsupportedCommand(kind));
        }
        Ok(&mut self.blobs)
    }

    /// Live blob resources (diagnostics, tests).
    pub fn blob_count(&self) -> usize {
        self.blobs.len()
    }

    /// Bytes promised across every live blob.
    pub fn blob_bytes(&self) -> u64 {
        self.blobs.total_bytes()
    }

    /// The device's shared-memory region, as the transport must publish it
    /// (VEN-2001): `None` when this device has no host-visible window, which
    /// is what keeps the region absent — and both transports' absent-region
    /// behaviour intact — on every host that cannot back one.
    pub fn shm_region(&self) -> Option<virtio_core::ShmRegion> {
        let len = self.blob_support.host_visible_bytes?;
        Some(virtio_core::ShmRegion {
            id: VIRTIO_GPU_SHM_ID_HOST_VISIBLE,
            len,
        })
    }

    /// The 3D validation front, or the in-band error every 3D command gets on
    /// a 2D-only device.
    fn three_d_mut(&mut self, kind: u32) -> Result<&mut Gpu3d, CommandError> {
        self.three_d
            .as_mut()
            .ok_or(CommandError::UnsupportedCommand(kind))
    }

    /// The host display this device presents to.
    pub fn display(&self) -> &S {
        &self.display
    }

    /// Number of live 2D resources (diagnostics, `entangled doctor`).
    pub fn resource_count(&self) -> usize {
        self.resources.len()
    }

    /// Resource currently bound to scanout 0, if any.
    pub fn scanout_resource(&self) -> Option<u32> {
        self.scanout.map(|s| s.resource_id)
    }

    /// Region of the scanout resource being shown, if any.
    pub fn scanout_rect(&self) -> Option<Rect> {
        self.scanout.map(|s| s.rect)
    }

    /// Cursor-queue commands drained and ignored (MVP-812 is not implemented).
    pub fn ignored_cursor_commands(&self) -> u64 {
        self.cursor_commands
    }

    // ------------------------------------------------------ queue draining

    /// Processes every available control chain, then notifies the driver once.
    fn drain_control(
        &mut self,
        queue: &mut Queue,
        mem: &Arc<GuestMem>,
        interrupt: &dyn Interrupt,
    ) -> Result<(), DeviceError> {
        let desc_table = queue.desc_table();
        let queue_size = queue.size();
        // `served` drives the interrupt (used-ring entries added); `taken`
        // drives the budget (chains popped), and the two differ once a fenced
        // chain is held back — a deferred chain has been taken off the
        // available ring without being used yet.
        let mut served = 0usize;
        let mut taken = 0usize;

        // Fences first: a response the host renderer retired while the guest
        // was busy frees a descriptor the guest may be waiting for, so
        // completing before draining keeps the ring moving.
        served += self.complete_fences(queue, mem)?;

        while let Some(head) = queue
            .pop_descriptor_chain(Arc::clone(mem))
            .map(|chain| chain.head_index())
        {
            taken += 1;
            match self.handle_command(mem, desc_table, queue_size, head) {
                Served::Done(written) => {
                    queue
                        .add_used(mem.as_ref(), head, written)
                        .map_err(|e| DeviceError::Queue(e.to_string()))?;
                    served += 1;
                }
                // The chain stays out of the used ring until its fence
                // retires; `complete_fences` puts it back.
                Served::Deferred => (),
            }
            if taken >= CHAINS_PER_NOTIFY {
                tracing::warn!(
                    served,
                    taken,
                    "virtio-gpu controlq notification budget exhausted; deferring the rest"
                );
                break;
            }
        }

        if served > 0
            && queue
                .needs_notification(mem.as_ref())
                .map_err(|e| DeviceError::Queue(e.to_string()))?
        {
            interrupt.signal_used_queue(CONTROL_QUEUE)?;
        }
        // Reported only after the guest has its completions and its
        // interrupt: this makes the transport set `DEVICE_NEEDS_RESET`, and a
        // driver that acts on it must still find the chains it was owed.
        if self.renderer_lost && !self.renderer_loss_reported {
            self.renderer_loss_reported = true;
            return Err(DeviceError::Backend(
                "the host 3D renderer was lost; virtio-gpu degraded to 2D".into(),
            ));
        }
        Ok(())
    }

    /// Drains the cursor queue, acting on each command (MVP-812).
    ///
    /// `UPDATE_CURSOR`/`MOVE_CURSOR` carry no response payload (Linux'
    /// `virtio_gpu_queue_cursor` submits them with no device-writable buffer at
    /// all), but the chains *must* be returned to the used ring: the driver
    /// sleeps on `vq->num_free` when the queue fills up, so a device that
    /// silently kept the buffers would hang the guest's cursor updates for
    /// good. A malformed cursor command is therefore logged and dropped — there
    /// is nowhere to answer it — and never fails the device.
    fn drain_cursor(
        &mut self,
        queue: &mut Queue,
        mem: &Arc<GuestMem>,
        interrupt: &dyn Interrupt,
    ) -> Result<(), DeviceError> {
        let mut served = 0usize;
        while let Some(head) = queue
            .pop_descriptor_chain(Arc::clone(mem))
            .map(|chain| chain.head_index())
        {
            self.handle_cursor_command(mem, queue.desc_table(), queue.size(), head);
            queue
                .add_used(mem.as_ref(), head, 0)
                .map_err(|e| DeviceError::Queue(e.to_string()))?;
            self.cursor_commands = self.cursor_commands.saturating_add(1);
            served += 1;
            if served >= CHAINS_PER_NOTIFY {
                break;
            }
        }
        if served > 0
            && queue
                .needs_notification(mem.as_ref())
                .map_err(|e| DeviceError::Queue(e.to_string()))?
        {
            interrupt.signal_used_queue(CURSOR_QUEUE)?;
        }
        Ok(())
    }

    /// One cursor-queue chain: gather, parse, act. All failure paths log and
    /// return — the cursor protocol has no response channel.
    fn handle_cursor_command(
        &mut self,
        mem: &GuestMem,
        desc_table: u64,
        queue_size: u16,
        head: u16,
    ) {
        let segments = match chain::walk(mem, desc_table, queue_size, head) {
            Ok(segments) => segments,
            Err(error) => {
                tracing::warn!(head, %error, "dropping malformed virtio-gpu cursor chain");
                return;
            }
        };
        let readable = match chain::split_rw(&segments) {
            Ok((readable, _)) => readable,
            Err(error) => {
                tracing::warn!(head, %error, "dropping virtio-gpu cursor chain");
                return;
            }
        };
        let mut request = std::mem::take(&mut self.req_buf);
        request.clear();
        let gathered = gather_request(mem, readable, &mut request, self.max_command_bytes());
        let result = gathered
            .and_then(|()| {
                CtrlHdr::parse(&request).ok_or(CommandError::Truncated {
                    kind: 0,
                    len: request.len(),
                    expected: CtrlHdr::LEN,
                })
            })
            .and_then(|hdr| match hdr.kind {
                cmd::UPDATE_CURSOR => self.update_cursor(&request),
                cmd::MOVE_CURSOR => self.move_cursor(&request),
                other => Err(CommandError::UnsupportedCommand(other)),
            });
        if let Err(error) = result {
            tracing::warn!(head, %error, "virtio-gpu cursor command rejected");
        }
        self.req_buf = request;
    }

    /// `UPDATE_CURSOR`: replace the cursor plane's image from a 2D resource
    /// (or hide the plane when the resource id is 0), then position it.
    fn update_cursor(&mut self, buf: &[u8]) -> Result<(), CommandError> {
        let cursor = UpdateCursor::parse(buf)
            .ok_or_else(|| truncated(cmd::UPDATE_CURSOR, buf.len(), UpdateCursor::LEN))?;
        if cursor.scanout_id >= NUM_SCANOUTS {
            return Err(CommandError::UnknownScanout(cursor.scanout_id));
        }
        if cursor.resource_id == 0 {
            self.display
                .hide_cursor()
                .map_err(|error| CommandError::Display(error.to_string()))?;
            tracing::debug!("virtio-gpu cursor hidden");
            return Ok(());
        }
        // Either half may own the cursor image (in virgl mode the guest
        // kernel creates it through RESOURCE_CREATE_3D like everything else).
        let (width, height, three_d) = match self.resources.get(cursor.resource_id) {
            Some(resource) => (resource.width(), resource.height(), false),
            None => {
                let desc = self
                    .three_d
                    .as_ref()
                    .and_then(|gpu| gpu.desc(cursor.resource_id))
                    .ok_or(CommandError::UnknownResource(cursor.resource_id))?;
                (desc.width, desc.height, true)
            }
        };
        if width == 0 || height == 0 || width > MAX_CURSOR_DIM || height > MAX_CURSOR_DIM {
            return Err(CommandError::CursorTooLarge { width, height });
        }
        // The whole resource is the cursor image; the guest transferred its
        // pixels into it (fenced) before submitting this command.
        let full = Rect {
            x: 0,
            y: 0,
            width,
            height,
        };
        let mut scratch = std::mem::take(&mut self.flush_buf);
        let pixels =
            if three_d {
                self.three_d_mut(cmd::UPDATE_CURSOR)
                    .and_then(|gpu| gpu.read_rect_bgra(cursor.resource_id, full, &mut scratch))
                    .map(|()| scratch.as_slice())
            } else {
                match self.resources.get(cursor.resource_id) {
                    Some(resource) => resource.rect_bytes(full, &mut scratch).ok_or(
                        CommandError::RectOutOfBounds {
                            rect: full,
                            width,
                            height,
                        },
                    ),
                    None => Err(CommandError::UnknownResource(cursor.resource_id)),
                }
            };
        let outcome = pixels.and_then(|pixels| {
            self.display
                .set_cursor(
                    width,
                    height,
                    cursor.hot_x,
                    cursor.hot_y,
                    cursor.x,
                    cursor.y,
                    pixels,
                )
                .map_err(|error| CommandError::Display(error.to_string()))
        });
        self.flush_buf = scratch;
        outcome?;
        tracing::debug!(
            resource = cursor.resource_id,
            width,
            height,
            x = cursor.x,
            y = cursor.y,
            "virtio-gpu cursor updated"
        );
        Ok(())
    }

    /// `MOVE_CURSOR`: reposition the plane without touching its image.
    fn move_cursor(&mut self, buf: &[u8]) -> Result<(), CommandError> {
        let cursor = UpdateCursor::parse(buf)
            .ok_or_else(|| truncated(cmd::MOVE_CURSOR, buf.len(), UpdateCursor::LEN))?;
        if cursor.scanout_id >= NUM_SCANOUTS {
            return Err(CommandError::UnknownScanout(cursor.scanout_id));
        }
        self.display
            .move_cursor(cursor.x, cursor.y)
            .map_err(|error| CommandError::Display(error.to_string()))
    }

    /// Handles one control chain: either writes the response (and reports how
    /// many bytes went into the used ring) or defers it until a host fence
    /// retires (ADR-0004 phase 2).
    ///
    /// Never returns an error: see the module-level failure policy.
    fn handle_command(
        &mut self,
        mem: &Arc<GuestMem>,
        desc_table: u64,
        queue_size: u16,
        head: u16,
    ) -> Served {
        let segments = match chain::walk(mem, desc_table, queue_size, head) {
            Ok(segments) => segments,
            Err(error) => {
                tracing::warn!(head, %error, "dropping malformed virtio-gpu descriptor chain");
                return Served::Done(0);
            }
        };
        let (readable, writable) = match chain::split_rw(&segments) {
            Ok(split) => split,
            Err(error) => {
                tracing::warn!(head, %error, "dropping virtio-gpu chain");
                return Served::Done(0);
            }
        };
        let capacity: u64 = writable.iter().map(|s| u64::from(s.len)).sum();
        if capacity < CtrlHdr::LEN as u64 {
            tracing::warn!(
                head,
                capacity,
                "virtio-gpu chain has no room for a response header; dropping it"
            );
            return Served::Done(0);
        }

        // Gather the request into the reusable staging buffer. Taken out of
        // `self` for the duration so the command handlers can borrow `self`
        // mutably; always put back.
        let mut request = std::mem::take(&mut self.req_buf);
        request.clear();
        let gathered = gather_request(mem, readable, &mut request, self.max_command_bytes());
        let (resp_hdr, body, fence) = match gathered.and_then(|()| {
            CtrlHdr::parse(&request).ok_or(CommandError::Truncated {
                kind: 0,
                len: request.len(),
                expected: CtrlHdr::LEN,
            })
        }) {
            Ok(hdr) => {
                // One tick of the frame's command clock (GAME-2105): the
                // first command after a present is what separates a guest
                // that is *waiting* from one that is *working*.
                self.pacing.note_command();
                let reply = self.dispatch(mem, &hdr, &request);
                let fence = self.fence_for(&hdr, reply.code);
                (hdr.response(reply.code), reply.body, fence)
            }
            Err(error) => {
                tracing::warn!(head, %error, "unusable virtio-gpu request");
                // No parsed header, so no fence information to echo.
                (
                    CtrlHdr {
                        kind: resp::ERR_UNSPEC,
                        ..CtrlHdr::default()
                    },
                    Vec::new(),
                    None,
                )
            }
        };
        self.req_buf = request;

        let needed = CtrlHdr::LEN as u64 + body.len() as u64;
        if capacity < needed {
            // The driver did not offer room for the body it asked for. Answering
            // with a truncated body would be a protocol lie, so it gets a
            // header-only error instead (the fence, if any, is still echoed).
            tracing::warn!(
                head,
                capacity,
                needed,
                "virtio-gpu response buffer is too small; replying ERR_UNSPEC"
            );
            let hdr = CtrlHdr {
                kind: resp::ERR_UNSPEC,
                ..resp_hdr
            };
            return Served::Done(write_response(mem, writable, [&hdr.to_bytes(), &[]]));
        }

        // A host fence is outstanding for this command: hold the response
        // until it retires (ADR-0004 phase 2). Over the cap — or with no room
        // recorded — the response goes out now, which is phase 1's model and
        // strictly safer than pinning more chains.
        if let Some(fence_id) = fence {
            let pending = PendingResponse {
                head,
                writable: writable.to_vec(),
                hdr: resp_hdr,
                body,
            };
            match self.pending_fences.push(fence_id, pending) {
                Ok(()) => {
                    self.fence_stats.deferred = self.fence_stats.deferred.saturating_add(1);
                    self.fence_stats.peak_pending =
                        self.fence_stats.peak_pending.max(self.pending_fences.len());
                    tracing::trace!(
                        head,
                        fence = fence_id,
                        pending = self.pending_fences.len(),
                        "virtio-gpu response deferred until its host fence retires"
                    );
                    return Served::Deferred;
                }
                Err(pending) => {
                    self.fence_stats.over_cap = self.fence_stats.over_cap.saturating_add(1);
                    tracing::warn!(
                        head,
                        fence = fence_id,
                        cap = MAX_PENDING_FENCES,
                        "virtio-gpu fence table is full; answering this fence synchronously"
                    );
                    return Served::Done(write_response(
                        mem,
                        writable,
                        [&pending.hdr.to_bytes(), &pending.body],
                    ));
                }
            }
        }
        Served::Done(write_response(mem, writable, [&resp_hdr.to_bytes(), &body]))
    }

    /// Whether this command's response must wait for a host fence, and on
    /// which fence id (ADR-0004 phase 2).
    ///
    /// `None` — answer now — for everything that is not a fenced 3D command,
    /// for a command that failed (a fence on a rejected command has nothing
    /// to wait for: the driver must see the error immediately), and whenever
    /// the renderer says the fence is already signalled.
    fn fence_for(&mut self, hdr: &CtrlHdr, code: u32) -> Option<u32> {
        if !hdr.wants_fence() || !FENCED_3D_COMMANDS.contains(&hdr.kind) {
            return None;
        }
        if !self.fence_mode.is_deferred() {
            // Phase 1 on purpose (the measurement baseline): the command has
            // executed, so the response goes out now and no host fence is
            // created at all.
            self.fence_stats.synchronous = self.fence_stats.synchronous.saturating_add(1);
            return None;
        }
        // Errors are answered at once, with the fence echoed exactly as
        // phase 1 did.
        if code >= resp::ERR_UNSPEC {
            return None;
        }
        // The cap is checked *before* the renderer is asked for a fence: a
        // host fence nobody will wait for is pure waste (it sits in the
        // renderer's fence list until it retires), and a guest that fences
        // everything without collecting must fall back to phase 1's
        // synchronous completion rather than pin more chains.
        if self.pending_fences.is_full() {
            self.fence_stats.over_cap = self.fence_stats.over_cap.saturating_add(1);
            tracing::warn!(
                cap = MAX_PENDING_FENCES,
                kind = format_args!("{:#06x}", hdr.kind),
                "virtio-gpu fence table is full; this fence completes synchronously"
            );
            return None;
        }
        let gpu = self.three_d.as_mut()?;
        // The wire's fence id is 64-bit and virglrenderer's is 32-bit; the low
        // half is what the host timeline sees. Ids are only ever *compared*,
        // and the pending table is 64 deep, so the truncation cannot alias
        // anything that matters.
        let fence_id = hdr.fence_id as u32;
        match gpu.create_fence(hdr.ctx_id, fence_id) {
            Ok(FenceOutcome::Pending) => Some(fence_id),
            Ok(FenceOutcome::Signalled) => {
                self.fence_stats.synchronous = self.fence_stats.synchronous.saturating_add(1);
                None
            }
            Err(error) => {
                // A fence the renderer refused: the command itself already
                // succeeded, so the guest gets its (immediate) completion and
                // the host gets a log line. Failing the command here would
                // undo work that has happened.
                tracing::warn!(
                    ctx = hdr.ctx_id,
                    fence = fence_id,
                    %error,
                    "virtio-gpu could not create a host fence; completing synchronously"
                );
                self.fence_stats.synchronous = self.fence_stats.synchronous.saturating_add(1);
                None
            }
        }
    }

    /// Retires host fences and returns the chains they were holding to the
    /// used ring. Returns how many used-ring entries were added.
    ///
    /// Also the watchdog: an entry older than [`FENCE_TIMEOUT`] is completed
    /// regardless, because a host fence that never retires must not become a
    /// guest that never wakes up.
    fn complete_fences(
        &mut self,
        queue: &mut Queue,
        mem: &Arc<GuestMem>,
    ) -> Result<usize, DeviceError> {
        // A renderer that has died takes precedence over anything it still
        // owes: everything held goes back to the guest as an error (GPU-012).
        let lost = self.degrade_if_renderer_lost();
        if !lost.is_empty() {
            let mut served = 0usize;
            for entry in lost {
                let written =
                    write_response(mem, &entry.writable, [&entry.hdr.to_bytes(), &entry.body]);
                queue
                    .add_used(mem.as_ref(), entry.head, written)
                    .map_err(|e| DeviceError::Queue(e.to_string()))?;
                served += 1;
            }
            return Ok(served);
        }
        if self.pending_fences.is_empty() {
            return Ok(0);
        }
        let pending = self.pending_fences.len();
        let retired = match self.three_d.as_mut() {
            Some(gpu) => gpu.poll_fences(pending),
            None => Vec::new(),
        };
        let mut done: Vec<(Duration, PendingResponse)> = Vec::new();
        for fence_id in retired {
            done.extend(self.pending_fences.complete(fence_id));
        }
        self.fence_stats.retired = self.fence_stats.retired.saturating_add(done.len() as u64);
        for (waited, _) in &done {
            let us = u64::try_from(waited.as_micros()).unwrap_or(u64::MAX);
            self.fence_stats.wait_us_total = self.fence_stats.wait_us_total.saturating_add(us);
            self.fence_stats.wait_us_max = self.fence_stats.wait_us_max.max(us);
        }

        // The watchdog: everything still waiting past the deadline goes out
        // too. Completing in submission order keeps the guest's fence
        // timeline monotonic, which is why this drains a prefix rather than
        // picking out individual stale entries.
        if let Some(oldest) = self.pending_fences.oldest_age() {
            if oldest >= self.fence_timeout {
                let abandoned = self.pending_fences.drain_all();
                self.fence_stats.timed_out = self
                    .fence_stats
                    .timed_out
                    .saturating_add(abandoned.len() as u64);
                tracing::warn!(
                    entries = abandoned.len(),
                    waited_ms = oldest.as_millis(),
                    "host fences did not retire within the timeout; completing the \
                     pending virtio-gpu responses anyway (the host renderer is \
                     stalled or gone)"
                );
                done.extend(abandoned);
            }
        }

        let mut served = 0usize;
        for (waited, entry) in done {
            let written =
                write_response(mem, &entry.writable, [&entry.hdr.to_bytes(), &entry.body]);
            queue
                .add_used(mem.as_ref(), entry.head, written)
                .map_err(|e| DeviceError::Queue(e.to_string()))?;
            served += 1;
            tracing::trace!(
                head = entry.head,
                fence = entry.hdr.fence_id,
                waited_us = waited.as_micros(),
                "virtio-gpu deferred response completed"
            );
        }
        Ok(served)
    }

    /// GPU-012: has the host renderer stopped being usable, and if so, take
    /// the one-time degradation.
    ///
    /// Everything about the failure is *reported*, never fatal: the pending
    /// responses go back to the guest as errors (a chain the guest never gets
    /// back is a hang), the renderer is dropped — which turns every later 3D
    /// command into an in-band `ERR_UNSPEC` and leaves the 2D half of the
    /// device fully working — and the caller tells the driver the device needs
    /// a reset. The VM keeps running; the worst the guest sees is a desktop
    /// that fell back to software GL on its next start.
    fn degrade_if_renderer_lost(&mut self) -> Vec<PendingResponse> {
        let alive = self.three_d.as_ref().is_none_or(|gpu| gpu.is_alive());
        if alive || self.renderer_lost {
            return Vec::new();
        }
        self.renderer_lost = true;
        let held: Vec<PendingResponse> = self
            .pending_fences
            .drain_all()
            .into_iter()
            .map(|(_, mut entry)| {
                // The work behind this fence did not finish and never will.
                entry.hdr.kind = resp::ERR_UNSPEC;
                entry.body.clear();
                entry
            })
            .collect();
        // Dropping the renderer releases the host GL state (and, for an
        // isolated renderer, the dead worker's socket).
        self.three_d = None;
        if self.scanout.is_some_and(|s| s.is_three_d()) {
            tracing::warn!("the scanout was a renderer resource; the window keeps its last frame");
            self.scanout = None;
        }
        tracing::error!(
            released = held.len(),
            fence_stats = ?self.fence_stats,
            "the host 3D renderer is gone; virtio-gpu has degraded to 2D for this \
             VM (the guest's GL stack will fall back to software rendering). The \
             VM itself is unaffected — see ADR-0004 GPU-012"
        );
        held
    }

    /// Fence-path counters (`entangled doctor`, tests, the phase-2
    /// measurement).
    pub fn fence_stats(&self) -> FenceStats {
        self.fence_stats
    }

    /// Whether the host renderer has been lost and the device degraded to 2D
    /// (GPU-012).
    pub fn renderer_lost(&self) -> bool {
        self.renderer_lost
    }

    /// Chooses whether fenced responses are deferred (phase 2) or answered
    /// immediately (phase 1).
    ///
    /// `entangled run` sets this from [`FENCE_MODE_ENV`], which is how the
    /// phase-2 before/after measurement is taken on one binary.
    pub fn set_fence_mode(&mut self, mode: FenceMode) {
        self.fence_mode = mode;
    }

    /// Mirrors the frame statistics into `path` as JSON, rewritten every
    /// [`crate::pacing::REPORT_EVERY`] frames (`entangled run --frame-stats`).
    ///
    /// The log already carries every window; the file exists so two runs can
    /// be compared with a diff, and so the numbers survive whatever ends the
    /// VM — the file is always at most one report window stale.
    pub fn set_frame_stats(&mut self, path: Option<std::path::PathBuf>) {
        if let Some(path) = &path {
            tracing::info!(path = %path.display(), "virtio-gpu frame statistics enabled");
        }
        self.frame_stats = path;
    }

    /// Overrides the fence watchdog deadline (default [`FENCE_TIMEOUT`]).
    ///
    /// Exists for tests — a two-second wait is not something to put in a unit
    /// test — and for a host that wants a different tolerance for its GL
    /// stack.
    pub fn set_fence_timeout(&mut self, timeout: Duration) {
        self.fence_timeout = timeout;
    }

    /// Fenced responses currently held back.
    pub fn pending_fences(&self) -> usize {
        self.pending_fences.len()
    }

    /// Routes one parsed command and turns a [`CommandError`] into the
    /// in-band response code.
    fn dispatch(&mut self, mem: &Arc<GuestMem>, hdr: &CtrlHdr, buf: &[u8]) -> Reply {
        let result = match hdr.kind {
            cmd::GET_DISPLAY_INFO => self.get_display_info(),
            cmd::GET_EDID => self.get_edid(buf),
            cmd::RESOURCE_CREATE_2D => self.resource_create_2d(buf),
            cmd::RESOURCE_UNREF => self.resource_unref(buf),
            cmd::SET_SCANOUT => self.set_scanout(buf),
            cmd::RESOURCE_FLUSH => self.resource_flush(buf),
            cmd::TRANSFER_TO_HOST_2D => self.transfer_to_host_2d(mem, buf),
            cmd::RESOURCE_ATTACH_BACKING => self.attach_backing(mem, buf),
            cmd::RESOURCE_DETACH_BACKING => self.detach_backing(buf),
            // Blob resources (VEN-2001). Like the 3D set, these are unknown
            // commands *before* any body parsing on a device that never
            // offered the feature.
            kind @ (cmd::RESOURCE_CREATE_BLOB
            | cmd::SET_SCANOUT_BLOB
            | cmd::RESOURCE_MAP_BLOB
            | cmd::RESOURCE_UNMAP_BLOB)
                if !self.blob_support.any() =>
            {
                Err(CommandError::UnsupportedCommand(kind))
            }
            cmd::RESOURCE_CREATE_BLOB => self.resource_create_blob(mem, buf),
            cmd::SET_SCANOUT_BLOB => self.set_scanout_blob(buf),
            cmd::RESOURCE_MAP_BLOB => self.resource_map_blob(buf),
            cmd::RESOURCE_UNMAP_BLOB => self.resource_unmap_blob(buf),
            // The 3D set (ADR-0004). On a 2D-only device these are unknown
            // commands — ERR_UNSPEC *before* any body parsing, so a truncated
            // 3D command on a 2D device is still "unsupported", not "invalid".
            kind @ (cmd::GET_CAPSET_INFO
            | cmd::GET_CAPSET
            | cmd::CTX_CREATE
            | cmd::CTX_DESTROY
            | cmd::CTX_ATTACH_RESOURCE
            | cmd::CTX_DETACH_RESOURCE
            | cmd::RESOURCE_CREATE_3D
            | cmd::TRANSFER_TO_HOST_3D
            | cmd::TRANSFER_FROM_HOST_3D
            | cmd::SUBMIT_3D)
                if self.three_d.is_none() =>
            {
                Err(CommandError::UnsupportedCommand(kind))
            }
            cmd::GET_CAPSET_INFO => self.get_capset_info(buf),
            cmd::GET_CAPSET => self.get_capset(buf),
            cmd::CTX_CREATE => self.ctx_create(hdr, buf),
            cmd::CTX_DESTROY => self.ctx_destroy(hdr),
            cmd::CTX_ATTACH_RESOURCE => self.ctx_resource(hdr, buf, true),
            cmd::CTX_DETACH_RESOURCE => self.ctx_resource(hdr, buf, false),
            cmd::RESOURCE_CREATE_3D => self.resource_create_3d(buf),
            cmd::TRANSFER_TO_HOST_3D => self.transfer_3d(hdr, buf, true),
            cmd::TRANSFER_FROM_HOST_3D => self.transfer_3d(hdr, buf, false),
            cmd::SUBMIT_3D => self.submit_3d(hdr, buf),
            other => Err(CommandError::UnsupportedCommand(other)),
        };
        match result {
            Ok(reply) => reply,
            Err(error) => {
                let code = error.resp_code();
                tracing::debug!(
                    command = format_args!("{:#06x}", hdr.kind),
                    response = format_args!("{code:#06x}"),
                    %error,
                    "virtio-gpu command rejected"
                );
                Reply::error(code)
            }
        }
    }

    // ---------------------------------------------------------- commands

    /// `GET_DISPLAY_INFO` (MVP-802): one enabled scanout at the host window's
    /// current guest resolution.
    fn get_display_info(&self) -> Result<Reply, CommandError> {
        let (width, height) = self.display.resolution();
        let modes = [DisplayOne {
            rect: Rect {
                x: 0,
                y: 0,
                width,
                height,
            },
            enabled: true,
            flags: 0,
        }];
        tracing::debug!(width, height, "virtio-gpu GET_DISPLAY_INFO");
        Ok(Reply {
            code: resp::OK_DISPLAY_INFO,
            body: display_info_body(&modes).to_vec(),
        })
    }

    /// `GET_EDID` (MVP-811): a valid EDID 1.4 block whose preferred detailed
    /// timing is the current scanout resolution.
    fn get_edid(&self, buf: &[u8]) -> Result<Reply, CommandError> {
        let cmd =
            GetEdid::parse(buf).ok_or_else(|| truncated(cmd::GET_EDID, buf.len(), GetEdid::LEN))?;
        if cmd.scanout >= NUM_SCANOUTS {
            return Err(CommandError::UnknownScanout(cmd.scanout));
        }
        let (width, height) = self.display.resolution();
        let block = crate::edid::edid_block(width, height)
            .ok_or(CommandError::UnencodableMode { width, height })?;
        tracing::debug!(width, height, "virtio-gpu GET_EDID");
        Ok(Reply {
            code: resp::OK_EDID,
            body: edid_body(&block).to_vec(),
        })
    }

    /// `RESOURCE_CREATE_2D` (MVP-803/809).
    fn resource_create_2d(&mut self, buf: &[u8]) -> Result<Reply, CommandError> {
        let cmd = ResourceCreate2d::parse(buf)
            .ok_or_else(|| truncated(cmd::RESOURCE_CREATE_2D, buf.len(), ResourceCreate2d::LEN))?;
        if self.blobs.owns(cmd.resource_id) {
            return Err(CommandError::DuplicateResource(cmd.resource_id));
        }
        self.resources
            .create(cmd.resource_id, cmd.format, cmd.width, cmd.height)?;
        tracing::debug!(
            resource = cmd.resource_id,
            width = cmd.width,
            height = cmd.height,
            "virtio-gpu resource created"
        );
        Ok(Reply::ok())
    }

    /// `RESOURCE_UNREF` (MVP-808): drops the resource, its backing list and the
    /// scanout binding if it pointed here. Routed to whichever half — 2D
    /// table or 3D renderer — owns the id.
    fn resource_unref(&mut self, buf: &[u8]) -> Result<Reply, CommandError> {
        let cmd = ResourceUnref::parse(buf)
            .ok_or_else(|| truncated(cmd::RESOURCE_UNREF, buf.len(), ResourceUnref::LEN))?;
        if cmd.resource_id == 0 {
            return Err(CommandError::ZeroResourceId);
        }
        if self.blobs.owns(cmd.resource_id) {
            // Unref of a mapped blob has to tear the host mapping down too;
            // the table reports whether there was one.
            let was_mapped_at = self.blobs.remove(cmd.resource_id)?;
            if let Some(gpu) = self.three_d.as_mut() {
                if let Some(offset) = was_mapped_at {
                    gpu.unmap_blob(cmd.resource_id, offset);
                }
                gpu.destroy_blob(cmd.resource_id);
            }
        } else {
            match self.three_d.as_mut() {
                Some(gpu) if gpu.owns(cmd.resource_id) => gpu.resource_unref(cmd.resource_id)?,
                _ => {
                    self.resources.remove(cmd.resource_id)?;
                }
            }
        }
        if self
            .scanout
            .is_some_and(|s| s.resource_id == cmd.resource_id)
        {
            tracing::info!(
                resource = cmd.resource_id,
                "scanout resource was unref'd; scanout 0 disabled"
            );
            self.scanout = None;
        }
        tracing::debug!(resource = cmd.resource_id, "virtio-gpu resource unref'd");
        Ok(Reply::ok())
    }

    /// `RESOURCE_ATTACH_BACKING` (MVP-804).
    ///
    /// The guest page addresses are deliberately *not* checked here: the guest
    /// may attach pages it is about to make valid, and a bad page must fail the
    /// transfer that touches it, not the attach. Only the entry count and the
    /// command length are validated, both before anything is allocated.
    fn attach_backing(&mut self, mem: &Arc<GuestMem>, buf: &[u8]) -> Result<Reply, CommandError> {
        let fixed = AttachBacking::parse(buf).ok_or_else(|| {
            truncated(cmd::RESOURCE_ATTACH_BACKING, buf.len(), AttachBacking::LEN)
        })?;
        if fixed.resource_id == 0 {
            return Err(CommandError::ZeroResourceId);
        }
        if fixed.nr_entries == 0 {
            return Err(CommandError::NoEntries);
        }
        if fixed.nr_entries > MAX_BACKING_ENTRIES {
            return Err(CommandError::TooManyEntries(fixed.nr_entries));
        }
        let expected = AttachBacking::total_len(fixed.nr_entries)
            .ok_or(CommandError::TooManyEntries(fixed.nr_entries))?;
        if buf.len() < expected {
            return Err(truncated(cmd::RESOURCE_ATTACH_BACKING, buf.len(), expected));
        }
        // A blob's pages are fixed at RESOURCE_CREATE_BLOB time and the
        // device sized every bound around them, so re-pointing them later is
        // refused rather than quietly honoured (VEN-2001).
        if self.blobs.owns(fixed.resource_id) {
            return Err(CommandError::NotABlobCommand(fixed.resource_id));
        }
        let owned_3d = self
            .three_d
            .as_ref()
            .is_some_and(|gpu| gpu.owns(fixed.resource_id));
        if !owned_3d && self.resources.get(fixed.resource_id).is_none() {
            return Err(CommandError::UnknownResource(fixed.resource_id));
        }

        let mut entries = Vec::new();
        entries
            .try_reserve_exact(usize::try_from(fixed.nr_entries).unwrap_or(0))
            .map_err(|_| CommandError::OutOfMemory)?;
        for index in 0..fixed.nr_entries {
            let entry = MemEntry::parse_at(buf, index)
                .ok_or_else(|| truncated(cmd::RESOURCE_ATTACH_BACKING, buf.len(), expected))?;
            entries.push(entry);
        }

        if owned_3d {
            self.three_d_mut(cmd::RESOURCE_ATTACH_BACKING)?
                .attach_backing(fixed.resource_id, mem, &entries)?;
            tracing::debug!(
                resource = fixed.resource_id,
                entries = fixed.nr_entries,
                "virtio-gpu 3D backing attached"
            );
            return Ok(Reply::ok());
        }

        let resource = self
            .resources
            .get_mut(fixed.resource_id)
            .ok_or(CommandError::UnknownResource(fixed.resource_id))?;
        if resource.backing_entries() > 0 {
            tracing::warn!(
                resource = fixed.resource_id,
                previous = resource.backing_entries(),
                "replacing a virtio-gpu backing store that was never detached"
            );
        }
        resource.attach(entries);
        tracing::debug!(
            resource = fixed.resource_id,
            entries = fixed.nr_entries,
            bytes = resource.backing_len(),
            "virtio-gpu backing attached"
        );
        Ok(Reply::ok())
    }

    /// `RESOURCE_DETACH_BACKING` (MVP-808). Same wire layout as unref.
    fn detach_backing(&mut self, buf: &[u8]) -> Result<Reply, CommandError> {
        let cmd = ResourceUnref::parse(buf).ok_or_else(|| {
            truncated(cmd::RESOURCE_DETACH_BACKING, buf.len(), ResourceUnref::LEN)
        })?;
        if cmd.resource_id == 0 {
            return Err(CommandError::ZeroResourceId);
        }
        if self.blobs.owns(cmd.resource_id) {
            return Err(CommandError::NotABlobCommand(cmd.resource_id));
        }
        if let Some(gpu) = self.three_d.as_mut() {
            if gpu.owns(cmd.resource_id) {
                gpu.detach_backing(cmd.resource_id)?;
                tracing::debug!(resource = cmd.resource_id, "virtio-gpu 3D backing detached");
                return Ok(Reply::ok());
            }
        }
        self.resources
            .get_mut(cmd.resource_id)
            .ok_or(CommandError::UnknownResource(cmd.resource_id))?
            .detach();
        tracing::debug!(resource = cmd.resource_id, "virtio-gpu backing detached");
        Ok(Reply::ok())
    }

    /// `SET_SCANOUT` (MVP-806): binds a region of a resource to scanout 0, and
    /// changes the host resolution when that region is a different size
    /// (MVP-813's guest-driven half).
    fn set_scanout(&mut self, buf: &[u8]) -> Result<Reply, CommandError> {
        let cmd = SetScanout::parse(buf)
            .ok_or_else(|| truncated(cmd::SET_SCANOUT, buf.len(), SetScanout::LEN))?;
        if cmd.scanout_id >= NUM_SCANOUTS {
            return Err(CommandError::UnknownScanout(cmd.scanout_id));
        }
        // Resource 0 means "disable this scanout" (spec 5.7.6.8).
        if cmd.resource_id == 0 {
            if self.scanout.take().is_some() {
                tracing::info!(scanout = cmd.scanout_id, "virtio-gpu scanout disabled");
            }
            return Ok(Reply::ok());
        }

        // Either half may own the resource, and in virgl mode both halves are
        // live at once: mesa's buffers arrive through RESOURCE_CREATE_3D, while
        // the kernel keeps creating its own dumb/console framebuffer with
        // RESOURCE_CREATE_2D (observed on Ubuntu 26.04: fb0 is a 1920x1080
        // B8G8R8X8 2D resource on a device advertising +virgl).
        // A blob has no geometry, so it can only be bound with
        // `SET_SCANOUT_BLOB`, which carries the layout the plain command
        // lacks. Refusing here (rather than falling through to "unknown
        // resource") tells the guest which of its two mistakes it made.
        if self.blobs.owns(cmd.resource_id) {
            return Err(CommandError::NotABlobCommand(cmd.resource_id));
        }
        let (width, height, source) = match self.resources.get(cmd.resource_id) {
            Some(resource) => (resource.width(), resource.height(), ScanoutSource::TwoD),
            None => {
                let desc = self
                    .three_d
                    .as_ref()
                    .and_then(|gpu| gpu.desc(cmd.resource_id))
                    .ok_or(CommandError::UnknownResource(cmd.resource_id))?;
                (desc.width, desc.height, ScanoutSource::ThreeD)
            }
        };
        if !cmd.rect.fits_within(width, height) {
            return Err(CommandError::RectOutOfBounds {
                rect: cmd.rect,
                width,
                height,
            });
        }
        self.bind_scanout(cmd.scanout_id, cmd.resource_id, cmd.rect, source)
    }

    /// Shared tail of `SET_SCANOUT` and `SET_SCANOUT_BLOB`: resize the host
    /// window to the guest's mode and record the binding.
    fn bind_scanout(
        &mut self,
        scanout_id: u32,
        resource_id: u32,
        rect: Rect,
        source: ScanoutSource,
    ) -> Result<Reply, CommandError> {
        if self.display.resolution() != (rect.width, rect.height) {
            self.display
                .set_resolution(rect.width, rect.height)
                .map_err(|error| CommandError::Display(error.to_string()))?;
        }
        self.scanout = Some(ScanoutBinding {
            resource_id,
            rect,
            source,
        });
        tracing::info!(
            scanout = scanout_id,
            resource = resource_id,
            width = rect.width,
            height = rect.height,
            source = ?source,
            "virtio-gpu scanout set"
        );
        Ok(Reply::ok())
    }

    /// `TRANSFER_TO_HOST_2D` (MVP-805): guest backing pages → host image.
    fn transfer_to_host_2d(&mut self, mem: &GuestMem, buf: &[u8]) -> Result<Reply, CommandError> {
        let cmd = TransferToHost2d::parse(buf)
            .ok_or_else(|| truncated(cmd::TRANSFER_TO_HOST_2D, buf.len(), TransferToHost2d::LEN))?;
        if cmd.resource_id == 0 {
            return Err(CommandError::ZeroResourceId);
        }
        let resource = self
            .resources
            .get_mut(cmd.resource_id)
            .ok_or(CommandError::UnknownResource(cmd.resource_id))?;
        resource.transfer_from_backing(mem, cmd.rect, cmd.offset)?;
        tracing::trace!(
            resource = cmd.resource_id,
            width = cmd.rect.width,
            height = cmd.rect.height,
            "virtio-gpu transfer to host"
        );
        Ok(Reply::ok())
    }

    /// `RESOURCE_FLUSH` (MVP-807/810): pushes the dirty rect of the scanout
    /// resource to the host display.
    ///
    /// A flush of a resource that is not on the scanout is a no-op, not an
    /// error: guests flush offscreen resources routinely (the spec lets the
    /// host ignore those), and failing them would spam the driver's log.
    fn resource_flush(&mut self, buf: &[u8]) -> Result<Reply, CommandError> {
        let cmd = ResourceFlush::parse(buf)
            .ok_or_else(|| truncated(cmd::RESOURCE_FLUSH, buf.len(), ResourceFlush::LEN))?;
        if cmd.resource_id == 0 {
            return Err(CommandError::ZeroResourceId);
        }
        // Unknown ids are still an error — that is how a guest notices it
        // flushed something it had already unref'd. Either half may own the
        // resource (see `set_scanout`).
        // A blob has no geometry of its own: the only rect it can be flushed
        // against is the one `SET_SCANOUT_BLOB` declared for it.
        let (width, height, three_d) = if self.blobs.owns(cmd.resource_id) {
            match self.scanout {
                Some(s) if s.resource_id == cmd.resource_id => {
                    (s.rect.x + s.rect.width, s.rect.y + s.rect.height, false)
                }
                // Not the bound scanout: nothing to present, and no geometry
                // to bounds-check against. Same "offscreen flush" no-op the
                // 2D path takes below.
                _ => return Ok(Reply::ok()),
            }
        } else {
            match self.resources.get(cmd.resource_id) {
                Some(resource) => (resource.width(), resource.height(), false),
                None => {
                    let desc = self
                        .three_d
                        .as_ref()
                        .and_then(|gpu| gpu.desc(cmd.resource_id))
                        .ok_or(CommandError::UnknownResource(cmd.resource_id))?;
                    (desc.width, desc.height, true)
                }
            }
        };
        if !cmd.rect.fits_within(width, height) {
            return Err(CommandError::RectOutOfBounds {
                rect: cmd.rect,
                width,
                height,
            });
        }

        let Some(scanout) = self.scanout else {
            tracing::trace!(resource = cmd.resource_id, "flush with no scanout bound");
            return Ok(Reply::ok());
        };
        if scanout.resource_id != cmd.resource_id {
            tracing::trace!(
                resource = cmd.resource_id,
                scanout_resource = scanout.resource_id,
                "flush of an offscreen resource ignored"
            );
            return Ok(Reply::ok());
        }
        // Clip to the region the scanout actually shows; a flush entirely
        // outside it has nothing to present.
        let Some(clip) = scanout.rect.intersect(&cmd.rect) else {
            return Ok(Reply::ok());
        };
        // `clip` is inside `scanout.rect`, so both subtractions are positive.
        let dst_x = clip.x - scanout.rect.x;
        let dst_y = clip.y - scanout.rect.y;

        // How long the device itself spends presenting this rect: the readback
        // out of the renderer plus the push into the sink. Reported next to
        // the frame interval, because the interval alone cannot say whether a
        // slow frame is the guest's doing or ours (ADR-0004 phase 2).
        self.pacing
            .begin_flush(u64::from(clip.width) * u64::from(clip.height));
        if let ScanoutSource::Blob { stride, offset } = scanout.source {
            // VEN-2001: the pixels are guest pages. Gather the clipped rows
            // out of the blob's backing list — through the same checked
            // `vm-memory` path every other guest read uses — and push them
            // down the sink.
            let mut scratch = std::mem::take(&mut self.flush_buf);
            let outcome = self
                .read_blob_rect(cmd.resource_id, clip, stride, offset, &mut scratch)
                .and_then(|()| {
                    self.display
                        .update_scanout(dst_x, dst_y, clip.width, clip.height, &scratch)
                        .map_err(|error| CommandError::Display(error.to_string()))
                });
            self.flush_buf = scratch;
            outcome?;
        } else if three_d {
            // GPU-010: the rendered pixels live in the host renderer; read
            // the dirty rect back as BGRA and push it down the same sink.
            let mut scratch = std::mem::take(&mut self.flush_buf);
            let read = self
                .three_d_mut(cmd::RESOURCE_FLUSH)
                .and_then(|gpu| gpu.read_rect_bgra(cmd.resource_id, clip, &mut scratch));
            let outcome = read.and_then(|()| {
                self.display
                    .update_scanout(dst_x, dst_y, clip.width, clip.height, &scratch)
                    .map_err(|error| CommandError::Display(error.to_string()))
            });
            self.flush_buf = scratch;
            outcome?;
        } else {
            let resource = self
                .resources
                .get(cmd.resource_id)
                .ok_or(CommandError::UnknownResource(cmd.resource_id))?;
            let pixels = resource.rect_bytes(clip, &mut self.flush_buf).ok_or(
                CommandError::RectOutOfBounds {
                    rect: clip,
                    width,
                    height,
                },
            )?;
            self.display
                .update_scanout(dst_x, dst_y, clip.width, clip.height, pixels)
                .map_err(|error| CommandError::Display(error.to_string()))?;
        }
        tracing::trace!(
            resource = cmd.resource_id,
            x = dst_x,
            y = dst_y,
            width = clip.width,
            height = clip.height,
            three_d,
            "virtio-gpu flush"
        );
        // A flush of the scanout resource *is* a guest present, which makes
        // this the host's frame clock (ADR-0004 phase 2's measurement).
        if let Some(report) = self.pacing.record(Instant::now()) {
            let fences = self.fence_stats;
            tracing::info!(
                fps = report.fps(),
                mean_ms = report.mean_us as f64 / 1000.0,
                min_ms = report.min_us as f64 / 1000.0,
                max_ms = report.max_us as f64 / 1000.0,
                low_1_fps = report.low_1_fps(),
                low_01_fps = report.low_01_fps(),
                late = report.late,
                idle_gaps = report.idle_gaps,
                duplicate = report.duplicate,
                dropped = report.dropped,
                // The decomposition that says *whose* millisecond it is
                // (GAME-2105): quiet + submit + service is the interval.
                quiet_ms = report.quiet_mean_us as f64 / 1000.0,
                quiet_max_ms = report.quiet_max_us as f64 / 1000.0,
                submit_ms = report.submit_mean_us as f64 / 1000.0,
                commands = report.commands_mean,
                pixels = report.pixels_mean,
                service_mean_ms = report.service_mean_us as f64 / 1000.0,
                service_max_ms = report.service_max_us as f64 / 1000.0,
                fence_deferred = fences.deferred,
                fence_mean_wait_us = fences.mean_wait_us(),
                fence_max_wait_us = fences.wait_us_max,
                fence_peak_pending = fences.peak_pending,
                "virtio-gpu frame pacing"
            );
            if let Some(path) = &self.frame_stats {
                // Best effort by design: a measurement aid must never fail a
                // guest's frame. One warning, then it keeps trying — a full
                // disk that clears should start working again.
                if let Err(error) = self.pacing.write_json(path, Some(&report)) {
                    tracing::warn!(path = %path.display(), %error, "cannot write frame statistics");
                }
            }
        }
        Ok(Reply::ok())
    }

    // ------------------------------------------------ 3D commands (ADR-0004)

    /// `GET_CAPSET_INFO` (GPU-003). An index past `num_capsets` answers OK
    /// with a zeroed body (id 0 = "no capset"), matching QEMU — the driver
    /// probes indices in order and stops on the zeros.
    fn get_capset_info(&mut self, buf: &[u8]) -> Result<Reply, CommandError> {
        let cmd = GetCapsetInfo::parse(buf)
            .ok_or_else(|| truncated(cmd::GET_CAPSET_INFO, buf.len(), GetCapsetInfo::LEN))?;
        let gpu = self.three_d_mut(cmd::GET_CAPSET_INFO)?;
        let body = match gpu.capset_info(cmd.capset_index) {
            Some(info) => capset_info_body(info.id, info.max_version, info.max_size),
            None => capset_info_body(0, 0, 0),
        };
        Ok(Reply {
            code: resp::OK_CAPSET_INFO,
            body: body.to_vec(),
        })
    }

    /// `GET_CAPSET` (GPU-003): the renderer's capability blob.
    fn get_capset(&mut self, buf: &[u8]) -> Result<Reply, CommandError> {
        let cmd = GetCapset::parse(buf)
            .ok_or_else(|| truncated(cmd::GET_CAPSET, buf.len(), GetCapset::LEN))?;
        let gpu = self.three_d_mut(cmd::GET_CAPSET)?;
        let body = gpu.capset(cmd.capset_id, cmd.capset_version)?;
        tracing::debug!(
            capset = cmd.capset_id,
            version = cmd.capset_version,
            bytes = body.len(),
            "virtio-gpu GET_CAPSET"
        );
        Ok(Reply {
            code: resp::OK_CAPSET,
            body,
        })
    }

    /// `CTX_CREATE` (GPU-004): the context id is the header's `ctx_id`.
    fn ctx_create(&mut self, hdr: &CtrlHdr, buf: &[u8]) -> Result<Reply, CommandError> {
        let cmd = CtxCreate::parse(buf)
            .ok_or_else(|| truncated(cmd::CTX_CREATE, buf.len(), CtxCreate::LEN))?;
        let name = cmd.name();
        let ctx_id = hdr.ctx_id;
        self.three_d_mut(cmd::CTX_CREATE)?
            .ctx_create(ctx_id, cmd.context_init, &name)?;
        tracing::debug!(ctx = ctx_id, name, "virtio-gpu 3D context created");
        Ok(Reply::ok())
    }

    /// `CTX_DESTROY` (GPU-004).
    fn ctx_destroy(&mut self, hdr: &CtrlHdr) -> Result<Reply, CommandError> {
        let ctx_id = hdr.ctx_id;
        self.three_d_mut(cmd::CTX_DESTROY)?.ctx_destroy(ctx_id)?;
        tracing::debug!(ctx = ctx_id, "virtio-gpu 3D context destroyed");
        Ok(Reply::ok())
    }

    /// `CTX_ATTACH_RESOURCE` / `CTX_DETACH_RESOURCE` (GPU-006).
    ///
    /// **Either half of the id namespace may own the resource.** The
    /// Linux driver attaches every object a DRM client opens to that client's
    /// 3D context (`virtio_gpu_gem_object_open`) and detaches it on close —
    /// *including* the objects the kernel itself created with
    /// `RESOURCE_CREATE_2D`. A virgl boot shows exactly one such pair: the
    /// fbdev console framebuffer (1920×1080 `B8G8R8X8_UNORM`, created 2D,
    /// attached to context 1, then detached when the DRM client drops its
    /// handle). QEMU and crosvm answer those with OK because they keep a
    /// *single* resource table — QEMU's virgl path even re-creates a 2D
    /// resource inside virglrenderer (`virgl_cmd_create_resource_2d`).
    ///
    /// Here the 2D table owns those ids and the renderer has no handle for
    /// them, so the command is fully validated (context must exist, the id must
    /// name a live resource) and then completes without calling the renderer —
    /// there is nothing to attach on the host side, and the guest never names a
    /// 2D resource inside a `SUBMIT_3D` stream (it reaches it through
    /// `TRANSFER_TO_HOST_2D` / `SET_SCANOUT`, which route by ownership too).
    fn ctx_resource(
        &mut self,
        hdr: &CtrlHdr,
        buf: &[u8],
        attach: bool,
    ) -> Result<Reply, CommandError> {
        let kind = if attach {
            cmd::CTX_ATTACH_RESOURCE
        } else {
            cmd::CTX_DETACH_RESOURCE
        };
        let cmd =
            CtxResource::parse(buf).ok_or_else(|| truncated(kind, buf.len(), CtxResource::LEN))?;
        if cmd.resource_id == 0 {
            return Err(CommandError::ZeroResourceId);
        }
        let ctx_id = hdr.ctx_id;
        // Looked up before the 3D front is borrowed; a 2D id is never an index.
        // A *blob* id belongs here too and takes the same path: venus attaches
        // its ring blob to its context, and the renderer has no 3D handle for
        // it, exactly like the kernel's 2D console framebuffer (ADR-0004's
        // mixed-namespace amendment).
        let owned_2d =
            self.resources.get(cmd.resource_id).is_some() || self.blobs.owns(cmd.resource_id);
        let gpu = self.three_d_mut(kind)?;
        if owned_2d {
            if !gpu.has_context(ctx_id) {
                return Err(CommandError::UnknownContext(ctx_id));
            }
            tracing::debug!(
                ctx = ctx_id,
                resource = cmd.resource_id,
                attach,
                "virtio-gpu ctx attach/detach of a 2D-created resource: \
                 accepted, nothing to do in the renderer"
            );
            return Ok(Reply::ok());
        }
        gpu.ctx_resource(ctx_id, cmd.resource_id, attach)?;
        Ok(Reply::ok())
    }

    /// `RESOURCE_CREATE_3D` (GPU-005). Ids share one namespace with the 2D
    /// table, so a clash there is a duplicate even before the 3D front looks.
    fn resource_create_3d(&mut self, buf: &[u8]) -> Result<Reply, CommandError> {
        let cmd = ResourceCreate3d::parse(buf)
            .ok_or_else(|| truncated(cmd::RESOURCE_CREATE_3D, buf.len(), ResourceCreate3d::LEN))?;
        if self.resources.get(cmd.resource_id).is_some() || self.blobs.owns(cmd.resource_id) {
            return Err(CommandError::DuplicateResource(cmd.resource_id));
        }
        self.three_d_mut(cmd::RESOURCE_CREATE_3D)?
            .resource_create(&cmd)?;
        tracing::debug!(
            resource = cmd.resource_id,
            target = cmd.target,
            format = cmd.format,
            width = cmd.width,
            height = cmd.height,
            depth = cmd.depth,
            "virtio-gpu 3D resource created"
        );
        Ok(Reply::ok())
    }

    /// `TRANSFER_TO_HOST_3D` / `TRANSFER_FROM_HOST_3D` (GPU-008).
    fn transfer_3d(
        &mut self,
        hdr: &CtrlHdr,
        buf: &[u8],
        to_host: bool,
    ) -> Result<Reply, CommandError> {
        let kind = if to_host {
            cmd::TRANSFER_TO_HOST_3D
        } else {
            cmd::TRANSFER_FROM_HOST_3D
        };
        let cmd =
            Transfer3d::parse(buf).ok_or_else(|| truncated(kind, buf.len(), Transfer3d::LEN))?;
        if cmd.resource_id == 0 {
            return Err(CommandError::ZeroResourceId);
        }
        if self.blobs.owns(cmd.resource_id) {
            // TRANSFER_*_3D names a box in a resource's geometry; a blob has
            // none. Venus moves blob bytes through its own ring instead.
            return Err(CommandError::NotABlobCommand(cmd.resource_id));
        }
        let ctx_id = hdr.ctx_id;
        self.three_d_mut(kind)?.transfer(ctx_id, &cmd, to_host)?;
        Ok(Reply::ok())
    }

    /// `SUBMIT_3D` (GPU-007): the command stream follows the fixed part in
    /// the same gathered buffer.
    fn submit_3d(&mut self, hdr: &CtrlHdr, buf: &[u8]) -> Result<Reply, CommandError> {
        let cmd = CmdSubmit3d::parse(buf)
            .ok_or_else(|| truncated(cmd::SUBMIT_3D, buf.len(), CmdSubmit3d::LEN))?;
        let declared = cmd.size as usize;
        let stream = buf
            .get(CmdSubmit3d::LEN..CmdSubmit3d::LEN.saturating_add(declared))
            .ok_or_else(|| truncated(cmd::SUBMIT_3D, buf.len(), CmdSubmit3d::LEN + declared))?;
        let ctx_id = hdr.ctx_id;
        self.three_d_mut(cmd::SUBMIT_3D)?.submit(ctx_id, stream)?;
        tracing::trace!(ctx = ctx_id, bytes = declared, "virtio-gpu 3D submit");
        Ok(Reply::ok())
    }

    // ------------------------------------- blob resources (EPIC 20/VEN-2001)

    /// `RESOURCE_CREATE_BLOB`.
    ///
    /// Everything here is guest-chosen: the id, the memory type, the flags,
    /// the entry count, every entry, the size and the `blob_id`. The order is
    /// deliberate — parse and bound the *count* before touching the trailing
    /// entries, validate the whole shape before allocating the entry vector,
    /// and call the renderer only once nothing can still fail on our side, so
    /// a rejection never leaves a half-created blob in either table.
    fn resource_create_blob(
        &mut self,
        mem: &Arc<GuestMem>,
        buf: &[u8],
    ) -> Result<Reply, CommandError> {
        let kind = cmd::RESOURCE_CREATE_BLOB;
        let args = ResourceCreateBlob::parse(buf)
            .ok_or_else(|| truncated(kind, buf.len(), ResourceCreateBlob::LEN))?;
        if args.nr_entries > MAX_BLOB_ENTRIES {
            return Err(CommandError::TooManyEntries(args.nr_entries));
        }
        let expected = ResourceCreateBlob::total_len(args.nr_entries)
            .ok_or(CommandError::TooManyEntries(args.nr_entries))?;
        if buf.len() < expected {
            return Err(truncated(kind, buf.len(), expected));
        }
        // One id namespace across all three tables (the ADR's routing rule).
        if self.resources.get(args.resource_id).is_some()
            || self
                .three_d
                .as_ref()
                .is_some_and(|gpu| gpu.owns(args.resource_id))
        {
            return Err(CommandError::DuplicateResource(args.resource_id));
        }

        let mut entries = Vec::new();
        entries
            .try_reserve_exact(usize::try_from(args.nr_entries).unwrap_or(0))
            .map_err(|_| CommandError::OutOfMemory)?;
        for index in 0..args.nr_entries {
            let entry = ResourceCreateBlob::entry_at(buf, index)
                .ok_or_else(|| truncated(kind, buf.len(), expected))?;
            entries.push(entry);
        }

        let support = self.blob_support;
        let backing_len = self.blobs.validate(&args, support, &entries)?;
        // A guest-memory blob is bookkeeping only: the pages are the guest's,
        // and putting them in front of the host renderer would hand a C
        // library guest pointers for no reason at all.
        if args.blob_mem != BLOB_MEM_GUEST {
            self.three_d_mut(kind)?.create_blob(&args, mem, &entries)?;
        }
        self.blobs.insert(&args, &entries, backing_len)?;
        tracing::debug!(
            resource = args.resource_id,
            blob_mem = args.blob_mem,
            blob_flags = format_args!("{:#x}", args.blob_flags),
            blob_id = args.blob_id,
            size = args.size,
            entries = args.nr_entries,
            "virtio-gpu blob resource created"
        );
        Ok(Reply::ok())
    }

    /// `RESOURCE_MAP_BLOB`: place the blob's host memory in the device's
    /// shared-memory region at a guest-chosen offset.
    ///
    /// The guest names the offset into a *host* mapping, which makes this the
    /// sharpest guest-controlled value in the epic. It is validated against
    /// the window length, the page grid and every live mapping
    /// ([`crate::blob::HostVisibleWindow`]) before the renderer is asked to
    /// back it, and rolled back if the renderer refuses.
    fn resource_map_blob(&mut self, buf: &[u8]) -> Result<Reply, CommandError> {
        let kind = cmd::RESOURCE_MAP_BLOB;
        let cmd = ResourceMapBlob::parse(buf)
            .ok_or_else(|| truncated(kind, buf.len(), ResourceMapBlob::LEN))?;
        if cmd.resource_id == 0 {
            return Err(CommandError::ZeroResourceId);
        }
        let size = self
            .blobs_mut(kind)?
            .reserve_mapping(cmd.resource_id, cmd.offset)?;
        let mapping = match self
            .three_d
            .as_mut()
            .ok_or(CommandError::NoHostVisibleWindow)
            .and_then(|gpu| gpu.map_blob(cmd.resource_id, cmd.offset, size))
        {
            Ok(mapping) => mapping,
            Err(error) => {
                // The window reservation must not outlive the failed map, or
                // the guest loses that span of the window for ever.
                self.blobs.unreserve(cmd.resource_id, cmd.offset);
                return Err(error);
            }
        };
        self.blobs.commit_mapping(cmd.resource_id, cmd.offset);
        tracing::debug!(
            resource = cmd.resource_id,
            offset = format_args!("{:#x}", cmd.offset),
            size,
            map_info = mapping.wire(),
            "virtio-gpu blob mapped into the shared-memory region"
        );
        Ok(Reply {
            code: resp::OK_MAP_INFO,
            body: map_info_body(mapping.wire()).to_vec(),
        })
    }

    /// `RESOURCE_UNMAP_BLOB`. Same wire layout as unref.
    fn resource_unmap_blob(&mut self, buf: &[u8]) -> Result<Reply, CommandError> {
        let kind = cmd::RESOURCE_UNMAP_BLOB;
        let cmd = ResourceUnref::parse(buf)
            .ok_or_else(|| truncated(kind, buf.len(), ResourceUnref::LEN))?;
        if cmd.resource_id == 0 {
            return Err(CommandError::ZeroResourceId);
        }
        let offset = self.blobs_mut(kind)?.unmap(cmd.resource_id)?;
        if let Some(gpu) = self.three_d.as_mut() {
            gpu.unmap_blob(cmd.resource_id, offset);
        }
        tracing::debug!(
            resource = cmd.resource_id,
            offset = format_args!("{offset:#x}"),
            "virtio-gpu blob unmapped"
        );
        Ok(Reply::ok())
    }

    /// `SET_SCANOUT_BLOB`: bind a guest-memory blob to scanout 0.
    ///
    /// This is the one blob command with a *format*, because a blob has none
    /// of its own — the guest declares width, height, format and per-plane
    /// strides here. Only single-plane BGRA is accepted (the same two layouts
    /// the 2D path takes), and only for a blob whose bytes are guest pages:
    /// presenting a host3d blob would need the zero-copy export this host
    /// cannot do (ADR-0004 phase 2's dmabuf probe), and pretending otherwise
    /// would show the guest a black screen instead of an error.
    fn set_scanout_blob(&mut self, buf: &[u8]) -> Result<Reply, CommandError> {
        let kind = cmd::SET_SCANOUT_BLOB;
        let cmd = SetScanoutBlob::parse(buf)
            .ok_or_else(|| truncated(kind, buf.len(), SetScanoutBlob::LEN))?;
        if cmd.scanout_id >= NUM_SCANOUTS {
            return Err(CommandError::UnknownScanout(cmd.scanout_id));
        }
        if cmd.resource_id == 0 {
            if self.scanout.take().is_some() {
                tracing::info!(scanout = cmd.scanout_id, "virtio-gpu scanout disabled");
            }
            return Ok(Reply::ok());
        }
        if !crate::is_supported_format(cmd.format) {
            return Err(CommandError::UnsupportedFormat(cmd.format));
        }
        // Planes 1..3 belong to planar YUV formats we do not accept; a
        // non-zero stride there means the guest thinks it bound something we
        // did not.
        if cmd.strides[1..].iter().any(|s| *s != 0) || cmd.offsets[1..].iter().any(|o| *o != 0) {
            return Err(CommandError::UnsupportedFormat(cmd.format));
        }
        let blob = self
            .blobs_mut(kind)?
            .get(cmd.resource_id)
            .ok_or(CommandError::UnknownResource(cmd.resource_id))?;
        if blob.blob_mem() != BLOB_MEM_GUEST {
            return Err(CommandError::BadBlobMem {
                blob_mem: blob.blob_mem(),
                reason: "only a guest-memory blob can be scanned out on this host",
            });
        }
        let backing_len = blob.backing_len();

        if !cmd.rect.fits_within(cmd.width, cmd.height) {
            return Err(CommandError::RectOutOfBounds {
                rect: cmd.rect,
                width: cmd.width,
                height: cmd.height,
            });
        }
        // The stride must hold a row, and the whole declared image must fit
        // inside the pages the blob actually has. All of it in u64.
        let min_stride = u64::from(cmd.width) * u64::from(crate::BYTES_PER_PIXEL);
        let stride = u64::from(cmd.strides[0]);
        if stride < min_stride {
            return Err(CommandError::BadGeometry {
                width: cmd.width,
                height: cmd.height,
            });
        }
        let needed = u64::from(cmd.offsets[0])
            .checked_add(stride.saturating_mul(u64::from(cmd.height)))
            .ok_or(CommandError::BadGeometry {
                width: cmd.width,
                height: cmd.height,
            })?;
        if needed > backing_len {
            return Err(CommandError::ShortBacking {
                need: needed,
                have: backing_len,
            });
        }

        self.bind_scanout(
            cmd.scanout_id,
            cmd.resource_id,
            cmd.rect,
            ScanoutSource::Blob {
                stride: cmd.strides[0],
                offset: cmd.offsets[0],
            },
        )
    }

    /// Gathers `rect` out of a blob's guest pages as packed BGRA rows.
    ///
    /// The layout was validated at `SET_SCANOUT_BLOB` time against the blob's
    /// backing length, so the arithmetic here cannot run past it — but it is
    /// still done in `u64` and the read still goes through the checked
    /// `vm-memory` path, because "validated earlier" is not a reason to skip a
    /// bound on a guest-controlled value.
    fn read_blob_rect(
        &self,
        resource_id: u32,
        rect: Rect,
        stride: u32,
        plane_offset: u32,
        out: &mut Vec<u8>,
    ) -> Result<(), CommandError> {
        let mem = self
            .mem
            .as_ref()
            .ok_or(CommandError::UnknownResource(resource_id))?;
        let blob = self
            .blobs
            .get(resource_id)
            .ok_or(CommandError::UnknownResource(resource_id))?;
        let bpp = u64::from(crate::BYTES_PER_PIXEL);
        let row_bytes =
            usize::try_from(u64::from(rect.width) * bpp).map_err(|_| CommandError::OutOfMemory)?;
        let total = row_bytes
            .checked_mul(usize::try_from(rect.height).unwrap_or(usize::MAX))
            .ok_or(CommandError::OutOfMemory)?;
        out.clear();
        out.try_reserve(total)
            .map_err(|_| CommandError::OutOfMemory)?;
        out.resize(total, 0);
        let backing = blob.backing();
        for row in 0..u64::from(rect.height) {
            let offset = u64::from(plane_offset)
                .checked_add((u64::from(rect.y) + row).saturating_mul(u64::from(stride)))
                .and_then(|at| at.checked_add(u64::from(rect.x) * bpp))
                .ok_or(CommandError::ShortBacking {
                    need: u64::MAX,
                    have: blob.backing_len(),
                })?;
            let start = usize::try_from(row)
                .unwrap_or(usize::MAX)
                .saturating_mul(row_bytes);
            let dst = out
                .get_mut(start..start.saturating_add(row_bytes))
                .ok_or(CommandError::OutOfMemory)?;
            crate::resource::read_backing(mem, backing, offset, dst)?;
        }
        Ok(())
    }
}

/// Shorthand for the truncated-command error.
fn truncated(kind: u32, len: usize, expected: usize) -> CommandError {
    CommandError::Truncated {
        kind,
        len,
        expected,
    }
}

/// Copies the device-readable part of a chain into `out`.
///
/// Bounded by `cap` ([`MAX_COMMAND_BYTES`], or [`MAX_COMMAND_BYTES_3D`] with a
/// renderer attached) and read through checked `vm-memory` calls, so neither
/// an enormous chain nor a buffer outside guest RAM can hurt the host.
fn gather_request(
    mem: &GuestMem,
    segments: &[Segment],
    out: &mut Vec<u8>,
    cap: usize,
) -> Result<(), CommandError> {
    for segment in segments {
        let len = segment.len as usize;
        if len == 0 {
            continue;
        }
        let total = out.len().saturating_add(len);
        if total > cap {
            return Err(CommandError::RequestTooLarge(total as u64));
        }
        let start = out.len();
        out.resize(total, 0);
        let slot = out
            .get_mut(start..total)
            .ok_or(CommandError::RequestTooLarge(total as u64))?;
        mem.read_slice(slot, GuestAddress(segment.addr))
            .map_err(|error| CommandError::Unreadable {
                addr: segment.addr,
                reason: error.to_string(),
            })?;
    }
    Ok(())
}

/// Scatters a response (header, then body) across the device-writable segments
/// of a chain and returns how many bytes landed.
///
/// The caller has already checked that the segments hold at least the bytes
/// being written; a guest-writable buffer that turns out not to be writable
/// guest memory truncates the response instead of failing the device.
fn write_response(mem: &GuestMem, writable: &[Segment], parts: [&[u8]; 2]) -> u32 {
    let mut written = 0u32;
    let mut segments = writable.iter();
    // Where the next byte goes: (guest address, bytes left in this segment).
    let mut cursor: Option<(u64, u32)> = None;

    for part in parts {
        let mut data = part;
        while !data.is_empty() {
            let (addr, remaining) = loop {
                match cursor {
                    Some((addr, remaining)) if remaining > 0 => break (addr, remaining),
                    _ => match segments.next() {
                        Some(segment) if segment.len > 0 => break (segment.addr, segment.len),
                        // Zero-length descriptors are legal and carry nothing.
                        Some(_) => continue,
                        None => return written,
                    },
                }
            };
            let take = data.len().min(remaining as usize);
            let (chunk, rest) = data.split_at(take);
            if let Err(error) = mem.write_slice(chunk, GuestAddress(addr)) {
                tracing::warn!(
                    addr = format_args!("{addr:#x}"),
                    len = chunk.len(),
                    %error,
                    "virtio-gpu response buffer is not writable guest memory"
                );
                return written;
            }
            written = written.saturating_add(take as u32);
            data = rest;
            // `take` never exceeds `remaining`, so neither line can overflow.
            cursor = Some((addr.saturating_add(take as u64), remaining - take as u32));
        }
    }
    written
}

impl<S: ScanoutSink> VirtioDevice for GpuDevice<S> {
    fn device_type(&self) -> DeviceType {
        DeviceType::Gpu
    }

    fn queue_max_sizes(&self) -> &[u16] {
        &QUEUE_MAX_SIZES
    }

    fn device_features(&self) -> u64 {
        self.features
    }

    fn ack_features(&mut self, negotiated: u64) -> bool {
        if negotiated & VIRTIO_F_VERSION_1 == 0 {
            return false;
        }
        if negotiated & !self.features != 0 {
            tracing::warn!(
                negotiated = format_args!("{negotiated:#x}"),
                offered = format_args!("{:#x}", self.features),
                "driver accepted virtio-gpu features the device never offered"
            );
            return false;
        }
        self.acked_features = negotiated;
        true
    }

    fn read_config(&self, offset: u64, data: &mut [u8]) {
        let num_capsets = self
            .three_d
            .as_ref()
            .map_or(NUM_CAPSETS, |gpu| gpu.num_capsets());
        let config = config_bytes(self.events_read, NUM_SCANOUTS, num_capsets);
        debug_assert_eq!(config.len(), CONFIG_LEN);
        for (i, byte) in data.iter_mut().enumerate() {
            let index = offset.saturating_add(i as u64);
            *byte = usize::try_from(index)
                .ok()
                .and_then(|i| config.get(i))
                .copied()
                .unwrap_or(0);
        }
    }

    fn write_config(&mut self, offset: u64, data: &[u8]) {
        // `events_clear` is the only writable field: a write clears the
        // corresponding bits of `events_read`.
        if offset == 4 && data.len() == 4 {
            let mut raw = [0u8; 4];
            raw.copy_from_slice(data);
            let clear = u32::from_le_bytes(raw);
            self.events_read &= !clear;
            tracing::debug!(
                clear = format_args!("{clear:#x}"),
                "virtio-gpu events cleared"
            );
            return;
        }
        tracing::warn!(
            offset,
            len = data.len(),
            "ignoring guest write to a read-only part of the virtio-gpu config space"
        );
    }

    fn activate(&mut self, resources: DeviceResources) -> Result<(), DeviceError> {
        if resources.queues.len() != NUM_QUEUES {
            return Err(DeviceError::QueueCount {
                expected: NUM_QUEUES,
                actual: resources.queues.len(),
            });
        }
        let mut queues = resources.queues.into_iter();
        self.control = queues.next();
        self.cursor = queues.next();
        self.mem = Some(resources.mem);
        self.interrupt = Some(resources.interrupt);
        let (width, height) = self.display.resolution();
        tracing::info!(width, height, scanouts = NUM_SCANOUTS, "virtio-gpu ready");
        Ok(())
    }

    fn notify(&mut self, queue_index: u16) -> Result<(), DeviceError> {
        let mem = self.mem.clone().ok_or(DeviceError::NotActivated)?;
        let interrupt = self.interrupt.clone().ok_or(DeviceError::NotActivated)?;
        match queue_index {
            CONTROL_QUEUE => {
                // Taken out for the duration so `self` stays mutably usable in
                // the drain loop; always put back, even on error.
                let mut queue = self.control.take().ok_or(DeviceError::NotActivated)?;
                let result = self.drain_control(&mut queue, &mem, interrupt.as_ref());
                self.control = Some(queue);
                result
            }
            CURSOR_QUEUE => {
                let mut queue = self.cursor.take().ok_or(DeviceError::NotActivated)?;
                let result = self.drain_cursor(&mut queue, &mem, interrupt.as_ref());
                self.cursor = Some(queue);
                result
            }
            other => Err(DeviceError::UnknownQueue(other)),
        }
    }

    fn shm_regions(&self) -> Vec<virtio_core::ShmRegion> {
        self.shm_region().into_iter().collect()
    }

    fn set_host_waker(&mut self, waker: Arc<dyn HostWaker>) {
        // The renderer is the only thing here with asynchronous host work; it
        // decides whether the waker is usable (and therefore whether fences
        // may be deferred at all — see `Renderer3d::create_fence`).
        if let Some(gpu) = self.three_d.as_mut() {
            gpu.set_host_waker(Arc::clone(&waker));
        }
        self.waker = Some(waker);
    }

    fn reset(&mut self) {
        // Fenced responses the guest will never collect: the driver is
        // tearing the queues down, so the chains they pinned simply go away
        // with the rest of the queue state. Dropping them before the queues
        // is what keeps the fence table from outliving the ring it points
        // into.
        let abandoned = self.pending_fences.drain_all().len();
        if abandoned > 0 {
            tracing::info!(
                abandoned,
                "virtio-gpu reset dropped fenced responses that were still waiting"
            );
        }

        // The host keeps showing the last frame until the driver comes back and
        // programs a new scanout; dropping the resources here is what frees the
        // (guest-triggered) host allocations.
        //
        // Order matters: the renderer goes first, because a renderer may hold
        // host pointers into guest memory (virgl iovecs) and must drop them
        // before this device lets go of its `Arc<GuestMem>`.
        if let Some(gpu) = self.three_d.as_mut() {
            gpu.reset();
        }
        self.control = None;
        self.cursor = None;
        self.mem = None;
        self.interrupt = None;
        self.acked_features = 0;
        self.events_read = 0;
        self.scanout = None;
        self.resources.clear();
        self.blobs.clear();
        self.req_buf = Vec::new();
        self.flush_buf = Vec::new();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::MEM_ENTRY_LEN;

    #[test]
    fn command_buffer_bound_matches_the_entry_limit() {
        assert_eq!(
            AttachBacking::total_len(MAX_BACKING_ENTRIES),
            Some(MAX_COMMAND_BYTES)
        );
        assert_eq!(
            MAX_COMMAND_BYTES,
            AttachBacking::LEN + MAX_BACKING_ENTRIES as usize * MEM_ENTRY_LEN
        );
    }

    /// MVP-1407: the staging buffer a guest can make the host allocate for one
    /// command is capped, and the cap is enforced *before* the copy — a chain
    /// whose readable segments add up past it fails without ever growing `out`
    /// to the requested size.
    #[test]
    fn gather_request_refuses_a_chain_over_the_command_cap() {
        let mem = virtio_core::testing::guest_memory(0x1_0000);
        // One page of guest memory, referenced over and over: the guest controls
        // the segment count, not how much memory it actually owns.
        let per_segment = 0x1000u32;
        let segments: Vec<Segment> = (0..=(MAX_COMMAND_BYTES / per_segment as usize))
            .map(|_| Segment {
                addr: 0,
                len: per_segment,
                writable: false,
            })
            .collect();
        let mut out = Vec::new();
        let error = gather_request(&mem, &segments, &mut out, MAX_COMMAND_BYTES)
            .expect_err("a chain over the command cap must be refused");
        assert!(matches!(error, CommandError::RequestTooLarge(_)), "{error}");
        assert!(
            out.len() <= MAX_COMMAND_BYTES,
            "the staging buffer grew to {} bytes despite the cap",
            out.len()
        );

        // Exactly at the cap still works, so the bound is not off by one.
        let mut out = Vec::new();
        let exact = [Segment {
            addr: 0,
            len: u32::try_from(MAX_COMMAND_BYTES).expect("cap fits in u32"),
            writable: false,
        }];
        // The read itself fails (guest memory is smaller than the cap), but it
        // must fail as an unreadable buffer, not as a size violation.
        match gather_request(&mem, &exact, &mut out, MAX_COMMAND_BYTES) {
            Ok(()) | Err(CommandError::Unreadable { .. }) => (),
            Err(other) => panic!("a request exactly at the cap must not be too large: {other}"),
        }
    }

    /// The 3D bound is the submit budget plus the fixed part, and a device
    /// with a renderer gathers up to it while the 2D device keeps the small
    /// cap.
    #[test]
    fn the_3d_command_cap_covers_a_full_submit_and_nothing_more() {
        assert_eq!(
            MAX_COMMAND_BYTES_3D,
            CmdSubmit3d::LEN + crate::renderer::MAX_SUBMIT_BYTES
        );

        struct NoSink;
        impl crate::sink::ScanoutSink for NoSink {
            fn resolution(&self) -> (u32, u32) {
                (64, 64)
            }
            fn set_resolution(&self, _: u32, _: u32) -> Result<(), crate::sink::SinkError> {
                Ok(())
            }
            fn update_scanout(
                &self,
                _: u32,
                _: u32,
                _: u32,
                _: u32,
                _: &[u8],
            ) -> Result<(), crate::sink::SinkError> {
                Ok(())
            }
            fn set_cursor(
                &self,
                _: u32,
                _: u32,
                _: u32,
                _: u32,
                _: u32,
                _: u32,
                _: &[u8],
            ) -> Result<(), crate::sink::SinkError> {
                Ok(())
            }
            fn move_cursor(&self, _: u32, _: u32) -> Result<(), crate::sink::SinkError> {
                Ok(())
            }
            fn hide_cursor(&self) -> Result<(), crate::sink::SinkError> {
                Ok(())
            }
        }

        let two_d = GpuDevice::new(NoSink);
        assert_eq!(two_d.max_command_bytes(), MAX_COMMAND_BYTES);
        assert_eq!(two_d.device_features() & crate::VIRTIO_GPU_F_VIRGL, 0);

        let three_d =
            GpuDevice::with_renderer(NoSink, Box::new(crate::null_renderer::NullRenderer::new()));
        assert_eq!(three_d.max_command_bytes(), MAX_COMMAND_BYTES_3D);
        assert_ne!(three_d.device_features() & crate::VIRTIO_GPU_F_VIRGL, 0);
    }

    #[test]
    fn queue_geometry_matches_the_spec() {
        assert_eq!(QUEUE_MAX_SIZES.len(), NUM_QUEUES);
        assert_eq!(CONTROL_QUEUE, 0);
        assert_eq!(CURSOR_QUEUE, 1);
        assert!(QUEUE_MAX_SIZES.iter().all(|s| s.is_power_of_two()));
    }
}
