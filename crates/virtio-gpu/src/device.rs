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

use virtio_core::chain::{self, Segment};
use virtio_core::device::{DeviceError, DeviceResources, DeviceType, VirtioDevice};
use virtio_core::interrupt::Interrupt;
use virtio_core::{GuestMem, MAX_QUEUE_SIZE, VIRTIO_F_VERSION_1};
use virtio_queue::{Queue, QueueT};
use vm_memory::{Bytes, GuestAddress};

use crate::error::CommandError;
use crate::protocol::{
    cmd, config_bytes, display_info_body, resp, AttachBacking, CtrlHdr, DisplayOne, MemEntry, Rect,
    ResourceCreate2d, ResourceFlush, ResourceUnref, SetScanout, TransferToHost2d, CONFIG_LEN,
    MEM_ENTRY_LEN,
};
use crate::resource::{ResourceTable, MAX_BACKING_ENTRIES};
use crate::sink::ScanoutSink;

/// controlq and cursorq, in queue order (spec section 5.7.2).
pub const NUM_QUEUES: usize = 2;
/// Index of the control queue — all 2D commands arrive here.
pub const CONTROL_QUEUE: u16 = 0;
/// Index of the cursor queue (MVP-812: drained, not yet acted upon).
pub const CURSOR_QUEUE: u16 = 1;

static QUEUE_MAX_SIZES: [u16; NUM_QUEUES] = [MAX_QUEUE_SIZE, MAX_QUEUE_SIZE];

/// Scanouts (virtual displays) the device exposes. One window, one scanout.
pub const NUM_SCANOUTS: u32 = 1;

/// Capability sets. Zero: no VirGL/3D in the MVP, so the guest never asks for
/// `GET_CAPSET_INFO`.
pub const NUM_CAPSETS: u32 = 0;

/// Largest command the device will gather, i.e. an attach-backing carrying the
/// maximum entry count. Bounds the staging buffer a guest can make the host
/// allocate (~256 KiB).
pub const MAX_COMMAND_BYTES: usize =
    AttachBacking::LEN + MAX_BACKING_ENTRIES as usize * MEM_ENTRY_LEN;

/// Hard bound on how many chains one notification processes, so a guest that
/// keeps refilling the ring from another vCPU cannot pin this thread forever.
const CHAINS_PER_NOTIFY: usize = 4 * MAX_QUEUE_SIZE as usize;

/// What the guest bound to scanout 0.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ScanoutBinding {
    resource_id: u32,
    /// Region of the resource the scanout shows; its size is the guest mode.
    rect: Rect,
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
    /// Cursor-queue commands drained and ignored so far (MVP-812).
    cursor_commands: u64,

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
            scanout: None,
            events_read: 0,
            // No VIRTIO_GPU_F_* features in the MVP: no VIRGL (3D is post-MVP),
            // no EDID (MVP-811), no resource UUID / blob resources.
            features: VIRTIO_F_VERSION_1,
            acked_features: 0,
            req_buf: Vec::new(),
            flush_buf: Vec::new(),
            cursor_commands: 0,
            mem: None,
            control: None,
            cursor: None,
            interrupt: None,
        }
    }

    /// The host display this device presents to.
    pub fn display(&self) -> &S {
        &self.display
    }

    /// Number of live 2D resources (diagnostics, `vmhost doctor`).
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
        let mut served = 0usize;

        while let Some(head) = queue
            .pop_descriptor_chain(Arc::clone(mem))
            .map(|chain| chain.head_index())
        {
            let written = self.handle_command(mem, desc_table, queue_size, head);
            queue
                .add_used(mem.as_ref(), head, written)
                .map_err(|e| DeviceError::Queue(e.to_string()))?;
            served += 1;
            if served >= CHAINS_PER_NOTIFY {
                tracing::warn!(
                    served,
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
        Ok(())
    }

    /// Drains the cursor queue without acting on it.
    ///
    /// TODO(MVP-812, P1): hardware cursor. `UPDATE_CURSOR`/`MOVE_CURSOR` carry
    /// no response payload (the driver never reads one back), but the chains
    /// *must* be returned to the used ring: Linux' `virtio_gpu_queue_cursor`
    /// sleeps on `vq->num_free` when the queue fills up, so a device that
    /// silently kept the buffers would hang the guest's cursor updates for
    /// good. Until the cursor is implemented the guest sees a device that
    /// accepts cursor commands and draws nothing — the pointer is still visible
    /// because the guest composites it into the scanout.
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
            queue
                .add_used(mem.as_ref(), head, 0)
                .map_err(|e| DeviceError::Queue(e.to_string()))?;
            self.cursor_commands = self.cursor_commands.saturating_add(1);
            served += 1;
            if served >= CHAINS_PER_NOTIFY {
                break;
            }
        }
        if served > 0 {
            tracing::trace!(served, "ignored virtio-gpu cursor commands (MVP-812)");
            if queue
                .needs_notification(mem.as_ref())
                .map_err(|e| DeviceError::Queue(e.to_string()))?
            {
                interrupt.signal_used_queue(CURSOR_QUEUE)?;
            }
        }
        Ok(())
    }

    /// Handles one control chain. Returns the number of bytes written into
    /// device-writable buffers, which is what goes into the used ring.
    ///
    /// Never returns an error: see the module-level failure policy.
    fn handle_command(
        &mut self,
        mem: &GuestMem,
        desc_table: u64,
        queue_size: u16,
        head: u16,
    ) -> u32 {
        let segments = match chain::walk(mem, desc_table, queue_size, head) {
            Ok(segments) => segments,
            Err(error) => {
                tracing::warn!(head, %error, "dropping malformed virtio-gpu descriptor chain");
                return 0;
            }
        };
        let (readable, writable) = match chain::split_rw(&segments) {
            Ok(split) => split,
            Err(error) => {
                tracing::warn!(head, %error, "dropping virtio-gpu chain");
                return 0;
            }
        };
        let capacity: u64 = writable.iter().map(|s| u64::from(s.len)).sum();
        if capacity < CtrlHdr::LEN as u64 {
            tracing::warn!(
                head,
                capacity,
                "virtio-gpu chain has no room for a response header; dropping it"
            );
            return 0;
        }

        // Gather the request into the reusable staging buffer. Taken out of
        // `self` for the duration so the command handlers can borrow `self`
        // mutably; always put back.
        let mut request = std::mem::take(&mut self.req_buf);
        request.clear();
        let gathered = gather_request(mem, readable, &mut request);
        let (resp_hdr, body) = match gathered.and_then(|()| {
            CtrlHdr::parse(&request).ok_or(CommandError::Truncated {
                kind: 0,
                len: request.len(),
                expected: CtrlHdr::LEN,
            })
        }) {
            Ok(hdr) => {
                let reply = self.dispatch(mem, &hdr, &request);
                (hdr.response(reply.code), reply.body)
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
            return write_response(mem, writable, [&hdr.to_bytes(), &[]]);
        }
        write_response(mem, writable, [&resp_hdr.to_bytes(), &body])
    }

    /// Routes one parsed command and turns a [`CommandError`] into the
    /// in-band response code.
    fn dispatch(&mut self, mem: &GuestMem, hdr: &CtrlHdr, buf: &[u8]) -> Reply {
        let result = match hdr.kind {
            cmd::GET_DISPLAY_INFO => self.get_display_info(),
            cmd::RESOURCE_CREATE_2D => self.resource_create_2d(buf),
            cmd::RESOURCE_UNREF => self.resource_unref(buf),
            cmd::SET_SCANOUT => self.set_scanout(buf),
            cmd::RESOURCE_FLUSH => self.resource_flush(buf),
            cmd::TRANSFER_TO_HOST_2D => self.transfer_to_host_2d(mem, buf),
            cmd::RESOURCE_ATTACH_BACKING => self.attach_backing(buf),
            cmd::RESOURCE_DETACH_BACKING => self.detach_backing(buf),
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

    /// `RESOURCE_CREATE_2D` (MVP-803/809).
    fn resource_create_2d(&mut self, buf: &[u8]) -> Result<Reply, CommandError> {
        let cmd = ResourceCreate2d::parse(buf)
            .ok_or_else(|| truncated(cmd::RESOURCE_CREATE_2D, buf.len(), ResourceCreate2d::LEN))?;
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
    /// scanout binding if it pointed here.
    fn resource_unref(&mut self, buf: &[u8]) -> Result<Reply, CommandError> {
        let cmd = ResourceUnref::parse(buf)
            .ok_or_else(|| truncated(cmd::RESOURCE_UNREF, buf.len(), ResourceUnref::LEN))?;
        self.resources.remove(cmd.resource_id)?;
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
    fn attach_backing(&mut self, buf: &[u8]) -> Result<Reply, CommandError> {
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
        if self.resources.get(fixed.resource_id).is_none() {
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

        let resource = self
            .resources
            .get(cmd.resource_id)
            .ok_or(CommandError::UnknownResource(cmd.resource_id))?;
        if !cmd.rect.fits_within(resource.width(), resource.height()) {
            return Err(CommandError::RectOutOfBounds {
                rect: cmd.rect,
                width: resource.width(),
                height: resource.height(),
            });
        }

        if self.display.resolution() != (cmd.rect.width, cmd.rect.height) {
            self.display
                .set_resolution(cmd.rect.width, cmd.rect.height)
                .map_err(|error| CommandError::Display(error.to_string()))?;
        }
        self.scanout = Some(ScanoutBinding {
            resource_id: cmd.resource_id,
            rect: cmd.rect,
        });
        tracing::info!(
            scanout = cmd.scanout_id,
            resource = cmd.resource_id,
            width = cmd.rect.width,
            height = cmd.rect.height,
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
        // flushed something it had already unref'd.
        let resource = self
            .resources
            .get(cmd.resource_id)
            .ok_or(CommandError::UnknownResource(cmd.resource_id))?;
        if !cmd.rect.fits_within(resource.width(), resource.height()) {
            return Err(CommandError::RectOutOfBounds {
                rect: cmd.rect,
                width: resource.width(),
                height: resource.height(),
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

        let pixels = resource.rect_bytes(clip, &mut self.flush_buf).ok_or(
            CommandError::RectOutOfBounds {
                rect: clip,
                width: resource.width(),
                height: resource.height(),
            },
        )?;
        self.display
            .update_scanout(dst_x, dst_y, clip.width, clip.height, pixels)
            .map_err(|error| CommandError::Display(error.to_string()))?;
        tracing::trace!(
            resource = cmd.resource_id,
            x = dst_x,
            y = dst_y,
            width = clip.width,
            height = clip.height,
            "virtio-gpu flush"
        );
        Ok(Reply::ok())
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
/// Bounded by [`MAX_COMMAND_BYTES`] and read through checked `vm-memory` calls,
/// so neither an enormous chain nor a buffer outside guest RAM can hurt the
/// host.
fn gather_request(
    mem: &GuestMem,
    segments: &[Segment],
    out: &mut Vec<u8>,
) -> Result<(), CommandError> {
    for segment in segments {
        let len = segment.len as usize;
        if len == 0 {
            continue;
        }
        let total = out.len().saturating_add(len);
        if total > MAX_COMMAND_BYTES {
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
        let config = config_bytes(self.events_read, NUM_SCANOUTS, NUM_CAPSETS);
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

    fn reset(&mut self) {
        // The host keeps showing the last frame until the driver comes back and
        // programs a new scanout; dropping the resources here is what frees the
        // (guest-triggered) host allocations.
        self.control = None;
        self.cursor = None;
        self.mem = None;
        self.interrupt = None;
        self.acked_features = 0;
        self.events_read = 0;
        self.scanout = None;
        self.resources.clear();
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

    #[test]
    fn queue_geometry_matches_the_spec() {
        assert_eq!(QUEUE_MAX_SIZES.len(), NUM_QUEUES);
        assert_eq!(CONTROL_QUEUE, 0);
        assert_eq!(CURSOR_QUEUE, 1);
        assert!(QUEUE_MAX_SIZES.iter().all(|s| s.is_power_of_two()));
    }
}
