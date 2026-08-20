//! Deferred fence responses over a real virtqueue (ADR-0004 phase 2).
//!
//! Phase 1 answered every fenced command as soon as it parsed. Phase 2 holds
//! the response back until the host renderer retires the fence, which is a
//! *protocol* change (a chain leaves the available ring and comes back
//! later), so it is tested where the protocol lives: through the mmio
//! registers, with hand-laid chains, exactly like `gpu_3d.rs`.
//!
//! The host renderer here is a double whose fences retire only when the test
//! says so — that is what makes the deferral observable at all, and it runs
//! on every host OS with no GPU.
//!
//! What each test pins down:
//!
//! * a fenced submit gets **no** used-ring entry until the fence retires;
//! * a retirement completes the whole prefix, in submission order;
//! * a *failed* fenced command answers immediately (an error has nothing to
//!   wait for);
//! * the pending table's cap ([`MAX_PENDING_FENCES`]) degrades to synchronous
//!   completion instead of pinning chains without limit — a guest cannot
//!   wedge the device by fencing everything;
//! * a fence that never retires is completed by the watchdog;
//! * a device reset releases everything held;
//! * a renderer that dies mid-flight (GPU-012) degrades the device instead of
//!   killing it: the pending responses come back, the 2D path still works,
//!   and the driver is told the device needs a reset.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use display::DisplayHandle;
use virtio_core::chain::VIRTQ_DESC_F_WRITE;
use virtio_core::testing::{guest_memory, SplitRing, TestIrqLine};
use virtio_core::{mmio, status, GuestMem, HostWaker, MmioTransport, VirtioDevice};
use virtio_gpu::protocol::{cmd, resp, MemEntry, Rect, ResourceCreate3d, Transfer3d, CTRL_HDR_LEN};
use virtio_gpu::renderer::{CapsetInfo, FenceOutcome, Renderer3d};
use virtio_gpu::{CommandError, GpuDevice, NullRenderer, MAX_PENDING_FENCES};
use vm_memory::{Bytes, GuestAddress};

const MEM_SIZE: u64 = 32 << 20;
const CONTROL_RING_BASE: u64 = 0x1000;
const CURSOR_RING_BASE: u64 = 0x8_0000;
/// One request buffer and one response buffer per in-flight chain.
const REQ_BASE: u64 = 0x10_0000;
const REQ_STRIDE: u64 = 0x400;
const RESP_BASE: u64 = 0x40_0000;
const RESP_STRIDE: u64 = 0x400;
const RESP_CAPACITY: u32 = 0x400;
const FB_ADDR: u64 = 0x80_0000;

// ===================================================== the renderer double

/// What the test can do to the double's fences from outside.
#[derive(Default)]
struct FenceState {
    /// Fence ids created, in creation order.
    created: Vec<u32>,
    /// Fence ids the next `poll_fences` will report.
    retire: Vec<u32>,
    /// `poll_fences` calls (i.e. how often the device came looking).
    polls: usize,
    /// Wakeups the renderer asked for through its host waker.
    wakes: usize,
    /// Set to make every later command look like a dead renderer (GPU-012).
    dead: bool,
    /// `still_pending` values the device reported.
    reported_pending: Vec<usize>,
}

#[derive(Clone, Default)]
struct FenceControl(Arc<Mutex<FenceState>>);

impl FenceControl {
    fn with<R>(&self, f: impl FnOnce(&mut FenceState) -> R) -> R {
        f(&mut self.0.lock().expect("fence state"))
    }

    fn created(&self) -> Vec<u32> {
        self.with(|s| s.created.clone())
    }

    /// Marks `fence_id` retired; the device learns of it on its next poll.
    fn retire(&self, fence_id: u32) {
        self.with(|s| s.retire.push(fence_id));
    }

    fn polls(&self) -> usize {
        self.with(|s| s.polls)
    }

    fn wakes(&self) -> usize {
        self.with(|s| s.wakes)
    }

    fn kill_renderer(&self) {
        self.with(|s| s.dead = true);
    }
}

/// A [`NullRenderer`] whose fences are asynchronous and controlled by the
/// test — the smallest thing that makes phase 2's deferral observable.
struct DeferringRenderer {
    inner: NullRenderer,
    control: FenceControl,
    waker: Option<Arc<dyn HostWaker>>,
}

impl DeferringRenderer {
    fn new(control: FenceControl) -> Self {
        Self {
            inner: NullRenderer::new(),
            control,
            waker: None,
        }
    }

    /// The in-band error a dead renderer answers with.
    fn dead_error() -> CommandError {
        CommandError::Renderer("renderer is gone (test)".into())
    }

    fn is_dead(&self) -> bool {
        self.control.with(|s| s.dead)
    }
}

impl Renderer3d for DeferringRenderer {
    fn capsets(&self) -> &[CapsetInfo] {
        self.inner.capsets()
    }

    fn capset(&mut self, id: u32, version: u32) -> Result<Vec<u8>, CommandError> {
        self.inner.capset(id, version)
    }

    fn ctx_create(&mut self, ctx_id: u32, capset_id: u32, name: &str) -> Result<(), CommandError> {
        self.inner.ctx_create(ctx_id, capset_id, name)
    }

    fn ctx_destroy(&mut self, ctx_id: u32) {
        self.inner.ctx_destroy(ctx_id);
    }

    fn resource_create_3d(&mut self, args: &ResourceCreate3d) -> Result<(), CommandError> {
        self.inner.resource_create_3d(args)
    }

    fn resource_unref(&mut self, resource_id: u32) {
        self.inner.resource_unref(resource_id);
    }

    fn ctx_attach_resource(&mut self, ctx_id: u32, resource_id: u32) {
        self.inner.ctx_attach_resource(ctx_id, resource_id);
    }

    fn ctx_detach_resource(&mut self, ctx_id: u32, resource_id: u32) {
        self.inner.ctx_detach_resource(ctx_id, resource_id);
    }

    fn attach_backing(
        &mut self,
        resource_id: u32,
        mem: &Arc<GuestMem>,
        entries: &[MemEntry],
    ) -> Result<(), CommandError> {
        self.inner.attach_backing(resource_id, mem, entries)
    }

    fn detach_backing(&mut self, resource_id: u32) {
        self.inner.detach_backing(resource_id);
    }

    fn transfer_to_host(&mut self, ctx_id: u32, xfer: &Transfer3d) -> Result<(), CommandError> {
        if self.is_dead() {
            return Err(Self::dead_error());
        }
        self.inner.transfer_to_host(ctx_id, xfer)
    }

    fn transfer_from_host(&mut self, ctx_id: u32, xfer: &Transfer3d) -> Result<(), CommandError> {
        if self.is_dead() {
            return Err(Self::dead_error());
        }
        self.inner.transfer_from_host(ctx_id, xfer)
    }

    fn submit(&mut self, ctx_id: u32, stream: &[u8]) -> Result<(), CommandError> {
        if self.is_dead() {
            return Err(Self::dead_error());
        }
        self.inner.submit(ctx_id, stream)
    }

    fn read_rect_bgra(
        &mut self,
        resource_id: u32,
        rect: Rect,
        out: &mut Vec<u8>,
    ) -> Result<(), CommandError> {
        if self.is_dead() {
            return Err(Self::dead_error());
        }
        self.inner.read_rect_bgra(resource_id, rect, out)
    }

    fn reset(&mut self) {
        self.inner.reset();
        self.control.with(|s| {
            s.created.clear();
            s.retire.clear();
        });
    }

    fn set_host_waker(&mut self, waker: Arc<dyn HostWaker>) {
        self.waker = Some(waker);
    }

    fn create_fence(&mut self, _ctx_id: u32, fence_id: u32) -> Result<FenceOutcome, CommandError> {
        if self.is_dead() {
            return Err(Self::dead_error());
        }
        self.control.with(|s| s.created.push(fence_id));
        // A real renderer's monitor thread wakes the device; here the test
        // does the waking, but the count is still asserted on.
        if let Some(waker) = &self.waker {
            self.control.with(|s| s.wakes += 1);
            let _ = waker;
        }
        Ok(FenceOutcome::Pending)
    }

    fn poll_fences(&mut self, still_pending: usize) -> Vec<u32> {
        self.control.with(|s| {
            s.polls += 1;
            s.reported_pending.push(still_pending);
            std::mem::take(&mut s.retire)
        })
    }

    fn is_alive(&self) -> bool {
        !self.is_dead()
    }
}

/// Counts wakeups, standing in for the machine layer's queue-0 eventfd.
#[derive(Default)]
struct CountingWaker(AtomicUsize);

impl HostWaker for CountingWaker {
    fn wake(&self) {
        self.0.fetch_add(1, Ordering::Release);
    }
}

// ============================================================== the guest

#[derive(Debug, Clone)]
struct Request(Vec<u8>);

impl Request {
    fn new(kind: u32) -> Self {
        let mut raw = vec![0u8; CTRL_HDR_LEN];
        raw[0..4].copy_from_slice(&kind.to_le_bytes());
        Self(raw)
    }

    fn ctx(mut self, ctx_id: u32) -> Self {
        self.0[16..20].copy_from_slice(&ctx_id.to_le_bytes());
        self
    }

    fn fenced(mut self, fence_id: u64) -> Self {
        self.0[4..8].copy_from_slice(&virtio_gpu::FLAG_FENCE.to_le_bytes());
        self.0[8..16].copy_from_slice(&fence_id.to_le_bytes());
        self
    }

    fn u32(mut self, value: u32) -> Self {
        self.0.extend_from_slice(&value.to_le_bytes());
        self
    }

    fn u64(mut self, value: u64) -> Self {
        self.0.extend_from_slice(&value.to_le_bytes());
        self
    }

    fn raw(mut self, bytes: &[u8]) -> Self {
        self.0.extend_from_slice(bytes);
        self
    }
}

fn ctx_create(ctx_id: u32) -> Request {
    Request::new(cmd::CTX_CREATE)
        .ctx(ctx_id)
        .u32(4)
        .u32(0)
        .raw(&[0u8; 64])
}

fn create_3d(id: u32, width: u32, height: u32) -> Request {
    Request::new(cmd::RESOURCE_CREATE_3D)
        .u32(id)
        .u32(2) // target: PIPE_TEXTURE_2D
        .u32(2) // format: B8G8R8X8_UNORM
        .u32(1 << 18) // bind: SCANOUT
        .u32(width)
        .u32(height)
        .u32(1) // depth
        .u32(1) // array_size
        .u32(0) // last_level
        .u32(0) // nr_samples
        .u32(0) // flags
        .u32(0) // padding
}

fn attach_backing(id: u32, addr: u64, len: u32) -> Request {
    Request::new(cmd::RESOURCE_ATTACH_BACKING)
        .u32(id)
        .u32(1)
        .u64(addr)
        .u32(len)
        .u32(0)
}

fn submit_3d(ctx_id: u32, stream: &[u8]) -> Request {
    Request::new(cmd::SUBMIT_3D)
        .ctx(ctx_id)
        .u32(u32::try_from(stream.len()).expect("small"))
        .u32(0)
        .raw(stream)
}

fn transfer_to_host_3d(ctx_id: u32, id: u32, w: u32, h: u32) -> Request {
    Request::new(cmd::TRANSFER_TO_HOST_3D)
        .ctx(ctx_id)
        .u32(0) // x
        .u32(0) // y
        .u32(0) // z
        .u32(w)
        .u32(h)
        .u32(1) // d
        .u64(0) // offset
        .u32(id)
        .u32(0) // level
        .u32(0) // stride
        .u32(0) // layer_stride
}

fn resource_create_2d(id: u32, width: u32, height: u32) -> Request {
    Request::new(cmd::RESOURCE_CREATE_2D)
        .u32(id)
        .u32(2) // B8G8R8X8_UNORM
        .u32(width)
        .u32(height)
}

/// A virgl-shaped stream: one command with `dwords` of payload.
fn stream_of(dwords: u16) -> Vec<u8> {
    let mut out = Vec::new();
    let header = (u32::from(dwords) << 16) | 1;
    out.extend_from_slice(&header.to_le_bytes());
    out.extend(std::iter::repeat_n(0u8, usize::from(dwords) * 4));
    out
}

// ============================================================= responses

#[derive(Debug, Clone)]
struct Response {
    head: u16,
    used_len: u32,
    raw: Vec<u8>,
}

impl Response {
    fn kind(&self) -> u32 {
        u32::from_le_bytes(self.raw[0..4].try_into().expect("in range"))
    }

    fn fence_id(&self) -> u64 {
        u64::from_le_bytes(self.raw[8..16].try_into().expect("in range"))
    }

    fn flags(&self) -> u32 {
        u32::from_le_bytes(self.raw[4..8].try_into().expect("in range"))
    }
}

// =============================================================== harness

struct Harness {
    mem: Arc<GuestMem>,
    control: SplitRing,
    cursor: SplitRing,
    transport: MmioTransport,
    ring_size: u16,
    /// Descriptor slots handed out so far (two descriptors each).
    slots: u16,
    /// Used-ring entries already collected.
    collected: u16,
    #[allow(dead_code)]
    display: DisplayHandle,
}

impl Harness {
    fn new(control: FenceControl) -> Self {
        Self::build(control, 16, None)
    }

    fn with_ring(control: FenceControl, ring_size: u16) -> Self {
        Self::build(control, ring_size, None)
    }

    /// A harness whose fence watchdog fires after `timeout` — zero makes the
    /// watchdog observable without a two-second test.
    fn with_fence_timeout(control: FenceControl, timeout: Duration) -> Self {
        Self::build(control, 16, Some(timeout))
    }

    fn build(control: FenceControl, ring_size: u16, timeout: Option<Duration>) -> Self {
        let display = DisplayHandle::detached(64, 64).expect("detached display");
        let mut device =
            GpuDevice::with_renderer(display.clone(), Box::new(DeferringRenderer::new(control)));
        // The machine layer hands every device a waker before it reaches its
        // transport; without one a renderer must not defer at all.
        device.set_host_waker(Arc::new(CountingWaker::default()));
        if let Some(timeout) = timeout {
            device.set_fence_timeout(timeout);
        }
        let mem = Arc::new(guest_memory(MEM_SIZE));
        let irq = Arc::new(TestIrqLine::default());
        let transport = MmioTransport::new(0, Box::new(device), Arc::clone(&mem), irq)
            .expect("transport accepts the device");
        let mut harness = Harness {
            mem,
            control: SplitRing::layout(CONTROL_RING_BASE, ring_size),
            cursor: SplitRing::layout(CURSOR_RING_BASE, ring_size),
            transport,
            ring_size,
            slots: 0,
            collected: 0,
            display,
        };
        harness.bring_up();
        harness
    }

    fn read32(&mut self, offset: u64) -> u32 {
        let mut data = [0u8; 4];
        self.transport.read(offset, &mut data);
        u32::from_le_bytes(data)
    }

    fn write32(&mut self, offset: u64, value: u32) {
        self.transport.write(offset, &value.to_le_bytes());
    }

    fn bring_up(&mut self) {
        self.write32(mmio::DEVICE_FEATURES_SEL, 0);
        let low = u64::from(self.read32(mmio::DEVICE_FEATURES));
        self.write32(mmio::DEVICE_FEATURES_SEL, 1);
        let features = low | (u64::from(self.read32(mmio::DEVICE_FEATURES)) << 32);
        assert_ne!(
            features & virtio_gpu::VIRTIO_GPU_F_VIRGL,
            0,
            "a device with a renderer offers VIRGL"
        );
        self.write32(mmio::STATUS, status::ACKNOWLEDGE);
        self.write32(mmio::STATUS, status::ACKNOWLEDGE | status::DRIVER);
        self.write32(mmio::DRIVER_FEATURES_SEL, 0);
        self.write32(mmio::DRIVER_FEATURES, features as u32);
        self.write32(mmio::DRIVER_FEATURES_SEL, 1);
        self.write32(mmio::DRIVER_FEATURES, (features >> 32) as u32);
        self.write32(
            mmio::STATUS,
            status::ACKNOWLEDGE | status::DRIVER | status::FEATURES_OK,
        );
        for (index, ring) in [(0u32, self.control), (1, self.cursor)] {
            self.write32(mmio::QUEUE_SEL, index);
            self.write32(mmio::QUEUE_NUM, u32::from(self.ring_size));
            self.write32(mmio::QUEUE_DESC_LOW, ring.desc_table() as u32);
            self.write32(mmio::QUEUE_DESC_HIGH, (ring.desc_table() >> 32) as u32);
            self.write32(mmio::QUEUE_DRIVER_LOW, ring.driver_area() as u32);
            self.write32(mmio::QUEUE_DRIVER_HIGH, (ring.driver_area() >> 32) as u32);
            self.write32(mmio::QUEUE_DEVICE_LOW, ring.device_area() as u32);
            self.write32(mmio::QUEUE_DEVICE_HIGH, (ring.device_area() >> 32) as u32);
            self.write32(mmio::QUEUE_READY, 1);
        }
        self.write32(
            mmio::STATUS,
            status::ACKNOWLEDGE | status::DRIVER | status::FEATURES_OK | status::DRIVER_OK,
        );
        assert!(self.transport.is_activated(), "device must be live");
    }

    fn write_mem(&self, addr: u64, bytes: &[u8]) {
        self.mem
            .write_slice(bytes, GuestAddress(addr))
            .expect("test write inside guest memory");
    }

    /// Lays one request into a fresh descriptor pair and publishes it,
    /// without kicking: several chains can be queued before the device runs.
    fn publish(&mut self, request: &Request) -> u16 {
        let slot = self.slots;
        self.slots += 1;
        let desc = slot * 2;
        assert!(desc + 1 < self.ring_size, "ran out of descriptors");
        let req_addr = REQ_BASE + u64::from(slot) * REQ_STRIDE;
        let resp_addr = RESP_BASE + u64::from(slot) * RESP_STRIDE;
        assert!(
            request.0.len() as u64 <= REQ_STRIDE,
            "request does not fit its slot"
        );
        self.write_mem(req_addr, &request.0);
        self.write_mem(resp_addr, &vec![0xff; RESP_CAPACITY as usize]);
        let len = u32::try_from(request.0.len()).expect("small");
        self.control.write_desc(
            &self.mem,
            desc,
            req_addr,
            len,
            virtio_core::chain::VIRTQ_DESC_F_NEXT,
            desc + 1,
        );
        self.control.write_desc(
            &self.mem,
            desc + 1,
            resp_addr,
            RESP_CAPACITY,
            VIRTQ_DESC_F_WRITE,
            0,
        );
        // `publish` appends at avail_idx, which is what a driver does.
        self.control.publish(&self.mem, desc);
        desc
    }

    /// A queue kick — a guest notification, or (identically, by design) the
    /// host waker asking the device to look at its fences.
    fn kick(&mut self) {
        self.write32(mmio::QUEUE_NOTIFY, 0);
    }

    /// Every used-ring entry that appeared since the last call.
    fn collect(&mut self) -> Vec<Response> {
        let idx = self.control.used_idx(&self.mem);
        let mut out = Vec::new();
        while self.collected != idx {
            let slot = self.collected % self.ring_size;
            let (head, used_len) = self.control.used_elem(&self.mem, slot);
            let head = u16::try_from(head).expect("head fits");
            let resp_addr = RESP_BASE + u64::from(head / 2) * RESP_STRIDE;
            let mut raw = vec![0u8; RESP_CAPACITY as usize];
            self.mem
                .read_slice(&mut raw, GuestAddress(resp_addr))
                .expect("read response");
            out.push(Response {
                head,
                used_len,
                raw,
            });
            self.collected = self.collected.wrapping_add(1);
        }
        out
    }

    /// Publish one request, kick, and return whatever came back.
    fn run(&mut self, request: &Request) -> Vec<Response> {
        self.publish(request);
        self.kick();
        self.collect()
    }

    /// Publish one request that must answer immediately.
    fn run_one(&mut self, request: &Request) -> Response {
        let mut responses = self.run(request);
        assert_eq!(
            responses.len(),
            1,
            "expected exactly one response, got {}",
            responses.len()
        );
        responses.remove(0)
    }

    fn needs_reset(&self) -> bool {
        self.transport.status() & status::DEVICE_NEEDS_RESET != 0
    }

    /// Brings a context and a backed 3D resource up, the way a guest does
    /// before it starts submitting.
    fn open_3d(&mut self) {
        assert_ok(&self.run_one(&ctx_create(1)));
        assert_ok(&self.run_one(&create_3d(10, 4, 4)));
        self.write_mem(FB_ADDR, &[0x11u8; 64]);
        assert_ok(&self.run_one(&attach_backing(10, FB_ADDR, 64)));
    }
}

fn assert_ok(response: &Response) {
    assert_eq!(
        response.kind(),
        resp::OK_NODATA,
        "expected OK_NODATA, got {:#06x}",
        response.kind()
    );
    assert_eq!(response.used_len, CTRL_HDR_LEN as u32);
}

// ================================================================= tests

/// The core of phase 2: a fenced submit produces **no** used-ring entry until
/// the host fence retires, and then produces exactly the right one.
#[test]
fn a_fenced_submit_is_answered_only_when_the_host_fence_retires() {
    let control = FenceControl::default();
    let mut h = Harness::new(control.clone());
    h.open_3d();

    let stream = stream_of(3);
    let pending = h.run(&submit_3d(1, &stream).fenced(0x1001));
    assert!(
        pending.is_empty(),
        "a deferred fence must not be answered yet: {pending:?}"
    );
    assert_eq!(
        control.created(),
        vec![0x1001],
        "the renderer was asked for a host fence"
    );
    assert!(
        control.wakes() > 0,
        "the renderer holds a usable host waker"
    );

    // Another kick with nothing retired still answers nothing — the device
    // does not invent completions.
    h.kick();
    assert!(h.collect().is_empty());
    assert!(control.polls() > 0, "the device polled for retirement");

    // The host finishes the work; the next kick (in production, the waker's)
    // completes the response.
    control.retire(0x1001);
    h.kick();
    let done = h.collect();
    assert_eq!(done.len(), 1, "the fence retired exactly one response");
    assert_ok(&done[0]);
    assert_eq!(done[0].fence_id(), 0x1001, "the fence id is echoed");
    assert_ne!(
        done[0].flags() & virtio_gpu::FLAG_FENCE,
        0,
        "the response carries VIRTIO_GPU_FLAG_FENCE"
    );
    assert!(!h.needs_reset());
}

/// Fence timelines are ordered: retiring one fence completes everything
/// submitted before it, oldest first. This is what lets a renderer coalesce
/// retirement callbacks (and what the guest's DRM fence timeline requires).
#[test]
fn retiring_one_fence_completes_the_whole_prefix_in_order() {
    let control = FenceControl::default();
    let mut h = Harness::new(control.clone());
    h.open_3d();

    let stream = stream_of(1);
    for fence in 1..=3u64 {
        assert!(h
            .run(&submit_3d(1, &stream).fenced(0x2000 + fence))
            .is_empty());
    }
    assert_eq!(control.created(), vec![0x2001, 0x2002, 0x2003]);

    // Only the newest id is reported — the coalescing case.
    control.retire(0x2003);
    h.kick();
    let done = h.collect();
    assert_eq!(done.len(), 3, "the whole prefix completes");
    let fences: Vec<u64> = done.iter().map(Response::fence_id).collect();
    assert_eq!(
        fences,
        vec![0x2001, 0x2002, 0x2003],
        "completions are in submission order"
    );
    // Heads come back in submission order too, which is what the driver's
    // fence bookkeeping assumes.
    let heads: Vec<u16> = done.iter().map(|r| r.head).collect();
    assert!(heads.windows(2).all(|w| w[0] < w[1]), "{heads:?}");
}

/// A fenced command that *fails* is answered at once: there is no host work
/// to wait for, and a driver blocked on a fence for a rejected command would
/// be a wedge of our own making.
#[test]
fn a_failed_fenced_command_is_answered_immediately() {
    let control = FenceControl::default();
    let mut h = Harness::new(control.clone());
    h.open_3d();

    // Submit on a context that does not exist.
    let error = h.run_one(&submit_3d(99, &stream_of(1)).fenced(0x3001));
    assert_eq!(error.kind(), resp::ERR_INVALID_CONTEXT_ID);
    assert_eq!(error.fence_id(), 0x3001, "the fence is still echoed");
    assert!(
        control.created().is_empty(),
        "no host fence for a command that never ran"
    );

    // A transfer with a box outside the resource: same rule.
    let error = h.run_one(&transfer_to_host_3d(1, 10, 99, 99).fenced(0x3002));
    assert_eq!(error.kind(), resp::ERR_INVALID_PARAMETER);
    assert!(control.created().is_empty());
}

/// Only the commands that put work on the host GL timeline are deferred;
/// everything else keeps answering immediately even when fenced, so a
/// fence-happy driver never stalls on a capset query.
#[test]
fn non_timeline_commands_are_never_deferred_even_when_fenced() {
    let control = FenceControl::default();
    let mut h = Harness::new(control.clone());
    h.open_3d();

    let info = h.run_one(&Request::new(cmd::GET_CAPSET_INFO).u32(0).u32(0).fenced(1));
    assert_eq!(info.kind(), resp::OK_CAPSET_INFO);
    let created = h.run_one(&create_3d(11, 4, 4).fenced(2));
    assert_ok(&created);
    assert!(
        control.created().is_empty(),
        "capset queries and resource creation need no host fence"
    );
}

/// The cap: a guest that fences everything and collects nothing must not be
/// able to pin chains without limit. Past [`MAX_PENDING_FENCES`] the device
/// answers synchronously (phase 1's model) instead of holding more.
#[test]
fn the_pending_fence_table_is_capped_and_never_wedges_the_device() {
    let control = FenceControl::default();
    // Two descriptors per chain, and we want more chains in flight than the
    // cap allows.
    let mut h = Harness::with_ring(control.clone(), 256);
    h.open_3d();

    let stream = stream_of(1);
    let attempts = MAX_PENDING_FENCES + 8;
    let mut answered = 0usize;
    for i in 0..attempts {
        let fence = 0x4000 + i as u64;
        answered += h.run(&submit_3d(1, &stream).fenced(fence)).len();
    }
    assert_eq!(
        answered, 8,
        "exactly the commands past the cap are answered synchronously"
    );
    assert_eq!(
        control.created().len(),
        MAX_PENDING_FENCES,
        "the renderer is only asked for as many fences as the device will hold"
    );
    assert!(!h.needs_reset(), "the cap is not a device failure");

    // The device is still perfectly usable, and the held chains still
    // complete when the host catches up.
    control.retire(0x4000 + (MAX_PENDING_FENCES - 1) as u32);
    h.kick();
    assert_eq!(h.collect().len(), MAX_PENDING_FENCES);
}

/// A host fence that never retires must not become a guest that never wakes
/// up: the watchdog completes the pending responses.
#[test]
fn a_fence_that_never_retires_is_completed_by_the_watchdog() {
    let control = FenceControl::default();
    // A zero deadline makes the watchdog observable without a two-second
    // test; the production default is `virtio_gpu::device::FENCE_TIMEOUT`.
    let mut h = Harness::with_fence_timeout(control.clone(), Duration::ZERO);
    h.open_3d();

    assert!(h
        .run(&submit_3d(1, &stream_of(1)).fenced(0x5001))
        .is_empty());
    // Nothing retired — but the deadline has passed, so the next poll gives
    // up on it and answers the guest anyway.
    h.kick();
    let done = h.collect();
    assert_eq!(done.len(), 1, "the watchdog completed the response");
    assert_ok(&done[0]);
    assert_eq!(done[0].fence_id(), 0x5001);
    assert!(
        !h.needs_reset(),
        "a stalled host fence is not a device protocol failure"
    );
}

/// A device reset while fences are pending drops them with the rest of the
/// queue state — and the device comes back usable.
#[test]
fn a_reset_releases_the_pending_fences() {
    let control = FenceControl::default();
    let mut h = Harness::new(control.clone());
    h.open_3d();
    assert!(h
        .run(&submit_3d(1, &stream_of(1)).fenced(0x6001))
        .is_empty());

    // Status 0 is a reset (the driver rebinding, or the guest rebooting).
    h.write32(mmio::STATUS, 0);
    assert!(!h.transport.is_activated());

    // Bring it back up the way a fresh driver does; the stale fence must not
    // resurface as a completion in the new ring.
    h.collected = 0;
    h.slots = 0;
    h.control.rewind(&h.mem);
    h.bring_up();
    control.retire(0x6001);
    h.kick();
    assert!(
        h.collect().is_empty(),
        "a fence from before the reset must not complete into the new ring"
    );
    // …and the device works.
    assert_ok(&h.run_one(&ctx_create(2)));
}

/// GPU-012: the renderer dies while responses are pending. The VM must
/// survive, the guest must get its chains back, the 2D path must keep
/// working, and the driver must be told the device wants a reset.
#[test]
fn a_dead_renderer_degrades_the_device_instead_of_killing_it() {
    let control = FenceControl::default();
    let mut h = Harness::new(control.clone());
    h.open_3d();
    assert!(h
        .run(&submit_3d(1, &stream_of(1)).fenced(0x7001))
        .is_empty());

    // The host renderer is gone (in production: its subprocess died, or the
    // in-process guard trapped a fault inside the GL driver).
    control.kill_renderer();
    h.kick();

    let done = h.collect();
    assert_eq!(
        done.len(),
        1,
        "the pending response is released when the renderer dies"
    );
    assert_eq!(
        done[0].kind(),
        resp::ERR_UNSPEC,
        "the guest is told the work did not happen"
    );
    assert_eq!(done[0].fence_id(), 0x7001);
    assert!(
        h.needs_reset(),
        "the driver is told the device needs a reset (degrade to 2D)"
    );

    // 3D is refused from here on, in band…
    let refused = h.run_one(&submit_3d(1, &stream_of(1)));
    assert_eq!(refused.kind(), resp::ERR_UNSPEC);
    // …while the 2D half of the device still works, which is what "degrades
    // to 2D" means: the guest can keep a console framebuffer on screen.
    assert_ok(&h.run_one(&resource_create_2d(200, 4, 4)));
    assert_ok(&h.run_one(&attach_backing(200, FB_ADDR, 64)));
    let transfer = h.run_one(
        &Request::new(cmd::TRANSFER_TO_HOST_2D)
            .u32(0)
            .u32(0)
            .u32(4)
            .u32(4)
            .u64(0)
            .u32(200)
            .u32(0),
    );
    assert_ok(&transfer);
}
