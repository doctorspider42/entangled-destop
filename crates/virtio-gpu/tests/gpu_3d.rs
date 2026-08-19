//! End-to-end virtio-gpu **3D** tests over a real split virtqueue (ADR-0004,
//! GPU-002…GPU-010's device-side acceptance).
//!
//! Same discipline as `gpu_queue.rs` — the device is brought up through the
//! mmio registers exactly like a driver would, chains are laid out by hand,
//! and the pixels are asserted through a windowless
//! `display::DisplayHandle::detached` — but the device is built with
//! [`GpuDevice::with_renderer`] and the portable [`NullRenderer`], so the
//! whole 3D path (negotiation, capsets, contexts, `RESOURCE_CREATE_3D`,
//! backing, `TRANSFER_*_3D`, `SUBMIT_3D`, 3D scanout readback) runs on every
//! host OS with no GPU at all.
//!
//! Two groups: the well-behaved driver (the full pipeline, fences, teardown,
//! reset) and the malicious guest (bad ids, bad boxes, malformed streams,
//! oversized submits — none may panic, none may set DEVICE_NEEDS_RESET).

use std::sync::Arc;

use display::DisplayHandle;
use virtio_core::chain::VIRTQ_DESC_F_WRITE;
use virtio_core::testing::{guest_memory, SplitRing, TestIrqLine};
use virtio_core::{mmio, status, GuestMem, MmioTransport};
use virtio_gpu::protocol::{cmd, resp, Rect, CTRL_HDR_LEN};
use virtio_gpu::renderer::MAX_SUBMIT_BYTES;
use virtio_gpu::{GpuDevice, NullRenderer, VIRTIO_GPU_F_VIRGL};
use vm_memory::{Bytes, GuestAddress};

const MEM_SIZE: u64 = 16 << 20;
const CONTROL_RING_BASE: u64 = 0x1000;
const CURSOR_RING_BASE: u64 = 0x2000;
const RING_SIZE: u16 = 16;
const REQ_ADDR: u64 = 0x4000;
const RESP_ADDR: u64 = 0x8000;
const RESP_CAPACITY: u32 = 1024;
const FB_ADDR: u64 = 0x10_0000;

const RED_BGRA: [u8; 4] = [0x00, 0x00, 0xff, 0xff];
const RED_RGBA: [u8; 4] = [0xff, 0x00, 0x00, 0xff];

// ===================================================== the guest's half

#[derive(Debug, Clone)]
struct Request(Vec<u8>);

impl Request {
    fn new(kind: u32) -> Self {
        let mut raw = vec![0u8; CTRL_HDR_LEN];
        raw[0..4].copy_from_slice(&kind.to_le_bytes());
        Self(raw)
    }

    /// Sets the header's `ctx_id` — where every 3D command carries its
    /// context.
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

    fn bytes(&self) -> &[u8] {
        &self.0
    }
}

fn rect(x: u32, y: u32, width: u32, height: u32) -> Rect {
    Rect {
        x,
        y,
        width,
        height,
    }
}

fn ctx_create(ctx_id: u32, name: &str) -> Request {
    let mut debug_name = [0u8; 64];
    let len = name.len().min(64);
    debug_name[..len].copy_from_slice(&name.as_bytes()[..len]);
    Request::new(cmd::CTX_CREATE)
        .ctx(ctx_id)
        .u32(len as u32)
        .u32(0)
        .raw(&debug_name)
}

fn ctx_destroy(ctx_id: u32) -> Request {
    Request::new(cmd::CTX_DESTROY).ctx(ctx_id)
}

#[allow(clippy::too_many_arguments)]
fn create_3d(id: u32, target: u32, format: u32, width: u32, height: u32, depth: u32) -> Request {
    Request::new(cmd::RESOURCE_CREATE_3D)
        .u32(id)
        .u32(target)
        .u32(format)
        .u32(1 << 18) // bind: VIRGL_BIND_SCANOUT
        .u32(width)
        .u32(height)
        .u32(depth)
        .u32(1) // array_size
        .u32(0) // last_level
        .u32(0) // nr_samples
        .u32(0) // flags
        .u32(0) // padding
}

fn ctx_attach(ctx_id: u32, resource_id: u32) -> Request {
    Request::new(cmd::CTX_ATTACH_RESOURCE)
        .ctx(ctx_id)
        .u32(resource_id)
        .u32(0)
}

fn ctx_detach(ctx_id: u32, resource_id: u32) -> Request {
    Request::new(cmd::CTX_DETACH_RESOURCE)
        .ctx(ctx_id)
        .u32(resource_id)
        .u32(0)
}

fn attach_backing(id: u32, entries: &[(u64, u32)]) -> Request {
    let mut request = Request::new(cmd::RESOURCE_ATTACH_BACKING)
        .u32(id)
        .u32(u32::try_from(entries.len()).expect("small"));
    for &(addr, length) in entries {
        request = request.u64(addr).u32(length).u32(0);
    }
    request
}

fn transfer_3d(
    kind: u32,
    ctx_id: u32,
    id: u32,
    b: (u32, u32, u32, u32, u32, u32),
    offset: u64,
) -> Request {
    Request::new(kind)
        .ctx(ctx_id)
        .u32(b.0)
        .u32(b.1)
        .u32(b.2)
        .u32(b.3)
        .u32(b.4)
        .u32(b.5)
        .u64(offset)
        .u32(id)
        .u32(0) // level
        .u32(0) // stride
        .u32(0) // layer_stride
}

fn submit_3d(ctx_id: u32, stream: &[u8]) -> Request {
    Request::new(cmd::SUBMIT_3D)
        .ctx(ctx_id)
        .u32(u32::try_from(stream.len()).expect("small"))
        .u32(0)
        .raw(stream)
}

/// A virgl-shaped stream: `count` commands with the given payload sizes.
fn stream_of(payload_dwords: &[u16]) -> Vec<u8> {
    let mut out = Vec::new();
    for (i, dwords) in payload_dwords.iter().enumerate() {
        let header = (u32::from(*dwords) << 16) | ((i as u32 + 1) & 0xff);
        out.extend_from_slice(&header.to_le_bytes());
        out.extend(std::iter::repeat_n(0u8, usize::from(*dwords) * 4));
    }
    out
}

fn set_scanout(scanout_id: u32, resource_id: u32, region: Rect) -> Request {
    Request::new(cmd::SET_SCANOUT)
        .u32(region.x)
        .u32(region.y)
        .u32(region.width)
        .u32(region.height)
        .u32(scanout_id)
        .u32(resource_id)
}

fn resource_flush(id: u32, region: Rect) -> Request {
    Request::new(cmd::RESOURCE_FLUSH)
        .u32(region.x)
        .u32(region.y)
        .u32(region.width)
        .u32(region.height)
        .u32(id)
        .u32(0)
}

fn resource_unref(id: u32) -> Request {
    Request::new(cmd::RESOURCE_UNREF).u32(id).u32(0)
}

// ============================================================== responses

#[derive(Debug)]
struct Response {
    used_len: u32,
    raw: Vec<u8>,
}

impl Response {
    fn field32(&self, at: usize) -> u32 {
        let bytes: [u8; 4] = self.raw[at..at + 4].try_into().expect("in range");
        u32::from_le_bytes(bytes)
    }

    fn kind(&self) -> u32 {
        self.field32(0)
    }

    fn fence_id(&self) -> u64 {
        let bytes: [u8; 8] = self.raw[8..16].try_into().expect("in range");
        u64::from_le_bytes(bytes)
    }

    fn body(&self) -> &[u8] {
        &self.raw[CTRL_HDR_LEN..self.used_len as usize]
    }
}

fn assert_ok(response: &Response) {
    assert_eq!(
        response.kind(),
        resp::OK_NODATA,
        "expected OK_NODATA, got {:#06x}",
        response.kind()
    );
}

fn assert_err(response: &Response, expected: u32) {
    assert_eq!(
        response.kind(),
        expected,
        "expected {:#06x}, got {:#06x}",
        expected,
        response.kind()
    );
}

// ============================================================== harness

struct Harness {
    mem: Arc<GuestMem>,
    control: SplitRing,
    #[allow(dead_code)]
    cursor: SplitRing,
    transport: MmioTransport,
    display: DisplayHandle,
}

impl Harness {
    fn new(width: u32, height: u32) -> Self {
        let display = DisplayHandle::detached(width, height).expect("detached display");
        let device = GpuDevice::with_renderer(display.clone(), Box::new(NullRenderer::new()));
        let mem = Arc::new(guest_memory(MEM_SIZE));
        let irq = Arc::new(TestIrqLine::default());
        let transport = MmioTransport::new(0, Box::new(device), Arc::clone(&mem), irq)
            .expect("transport accepts the device");
        let mut harness = Harness {
            mem,
            control: SplitRing::layout(CONTROL_RING_BASE, RING_SIZE),
            cursor: SplitRing::layout(CURSOR_RING_BASE, RING_SIZE),
            transport,
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

    fn device_features(&mut self) -> u64 {
        self.write32(mmio::DEVICE_FEATURES_SEL, 0);
        let low = u64::from(self.read32(mmio::DEVICE_FEATURES));
        self.write32(mmio::DEVICE_FEATURES_SEL, 1);
        let high = u64::from(self.read32(mmio::DEVICE_FEATURES));
        low | (high << 32)
    }

    fn bring_up(&mut self) {
        assert_eq!(self.read32(mmio::DEVICE_ID), 16, "virtio-gpu device id");
        let features = self.device_features();
        assert_ne!(
            features & VIRTIO_GPU_F_VIRGL,
            0,
            "a device with a renderer must offer VIRTIO_GPU_F_VIRGL"
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
        assert_ne!(self.transport.status() & status::FEATURES_OK, 0);

        for (index, ring) in [(0u32, self.control), (1, self.cursor)] {
            self.write32(mmio::QUEUE_SEL, index);
            self.write32(mmio::QUEUE_NUM, u32::from(RING_SIZE));
            self.write32(mmio::QUEUE_DESC_LOW, ring.desc_table() as u32);
            self.write32(mmio::QUEUE_DESC_HIGH, 0);
            self.write32(mmio::QUEUE_DRIVER_LOW, ring.driver_area() as u32);
            self.write32(mmio::QUEUE_DRIVER_HIGH, 0);
            self.write32(mmio::QUEUE_DEVICE_LOW, ring.device_area() as u32);
            self.write32(mmio::QUEUE_DEVICE_HIGH, 0);
            self.write32(mmio::QUEUE_READY, 1);
        }
        self.write32(
            mmio::STATUS,
            status::ACKNOWLEDGE | status::DRIVER | status::FEATURES_OK | status::DRIVER_OK,
        );
        assert!(self.transport.is_activated(), "device must be live");
    }

    /// `num_capsets` from the device config space.
    fn num_capsets(&mut self) -> u32 {
        let mut raw = [0u8; 16];
        self.transport.read(mmio::CONFIG_SPACE, &mut raw);
        u32::from_le_bytes(raw[12..16].try_into().expect("in range"))
    }

    fn write_mem(&self, addr: u64, bytes: &[u8]) {
        self.mem
            .write_slice(bytes, GuestAddress(addr))
            .expect("test write inside guest memory");
    }

    fn read_mem(&self, addr: u64, len: usize) -> Vec<u8> {
        let mut buf = vec![0u8; len];
        self.mem
            .read_slice(&mut buf, GuestAddress(addr))
            .expect("test read inside guest memory");
        buf
    }

    fn run(&mut self, request: &Request) -> Response {
        self.write_mem(REQ_ADDR, request.bytes());
        self.write_mem(RESP_ADDR, &vec![0xff; RESP_CAPACITY as usize]);
        let len = u32::try_from(request.bytes().len()).expect("small");
        let control = self.control;
        let descs = [
            (REQ_ADDR, len, 0),
            (RESP_ADDR, RESP_CAPACITY, VIRTQ_DESC_F_WRITE),
        ];
        let last = descs.len() - 1;
        for (i, &(addr, len, flags)) in descs.iter().enumerate() {
            let index = u16::try_from(i).expect("small");
            let (flags, next) = if i == last {
                (flags, 0)
            } else {
                (flags | virtio_core::chain::VIRTQ_DESC_F_NEXT, index + 1)
            };
            control.write_desc(&self.mem, index, addr, len, flags, next);
        }
        control.publish(&self.mem, 0);
        self.write32(mmio::QUEUE_NOTIFY, 0);
        let idx = control.used_idx(&self.mem);
        assert!(idx > 0, "device did not add anything to the used ring");
        let (head, used_len) = control.used_elem(&self.mem, (idx - 1) % RING_SIZE);
        assert_eq!(head, 0);
        Response {
            used_len,
            raw: self.read_mem(RESP_ADDR, RESP_CAPACITY as usize),
        }
    }

    fn needs_reset(&self) -> bool {
        self.transport.status() & status::DEVICE_NEEDS_RESET != 0
    }

    /// One pixel of the current scanout, decoded from a PNG screenshot.
    fn px(&self, x: u32, y: u32) -> [u8; 4] {
        let png_bytes = self.display.screenshot_png().expect("screenshot encodes");
        let decoder = png::Decoder::new(std::io::Cursor::new(&png_bytes));
        let mut reader = decoder.read_info().expect("png parses");
        let mut buf = vec![0u8; reader.output_buffer_size().expect("sane png size")];
        let info = reader.next_frame(&mut buf).expect("png decodes");
        assert_eq!(info.color_type, png::ColorType::Rgba);
        let at = (y as usize * info.width as usize + x as usize) * 4;
        buf[at..at + 4].try_into().expect("in range")
    }
}

// ================================================================= tests

/// GPU-002/003/004/005/006/007/008/010 in one driver-shaped conversation.
#[test]
fn the_full_3d_pipeline_paints_the_scanout() {
    let mut h = Harness::new(4, 4);
    assert_eq!(h.num_capsets(), 2, "the null renderer serves VIRGL+VIRGL2");

    // Capset discovery, exactly like the guest kernel does it.
    let info = h.run(&Request::new(cmd::GET_CAPSET_INFO).u32(0).u32(0));
    assert_eq!(info.kind(), resp::OK_CAPSET_INFO);
    assert_eq!(info.body()[0..4], 1u32.to_le_bytes(), "capset 0 is VIRGL");
    let info = h.run(&Request::new(cmd::GET_CAPSET_INFO).u32(1).u32(0));
    assert_eq!(info.body()[0..4], 2u32.to_le_bytes(), "capset 1 is VIRGL2");
    // Past the end: OK with a zeroed body, not an error.
    let info = h.run(&Request::new(cmd::GET_CAPSET_INFO).u32(9).u32(0));
    assert_eq!(info.kind(), resp::OK_CAPSET_INFO);
    assert_eq!(info.body()[0..4], 0u32.to_le_bytes());

    let capset = h.run(&Request::new(cmd::GET_CAPSET).u32(1).u32(1));
    assert_eq!(capset.kind(), resp::OK_CAPSET);
    assert_eq!(capset.body().len(), 308, "VIRGL capset v1 size");

    // A mesa process appears.
    assert_ok(&h.run(&ctx_create(1, "glxgears")));
    assert_ok(&h.run(&create_3d(10, 2, 2, 4, 4, 1)));
    assert_ok(&h.run(&ctx_attach(1, 10)));
    h.write_mem(FB_ADDR, &RED_BGRA.repeat(16));
    assert_ok(&h.run(&attach_backing(10, &[(FB_ADDR, 64)])));
    assert_ok(&h.run(&transfer_3d(
        cmd::TRANSFER_TO_HOST_3D,
        1,
        10,
        (0, 0, 0, 4, 4, 1),
        0,
    )));

    // A fenced submit: the fence must be echoed (synchronous completion).
    let submitted = h.run(&submit_3d(1, &stream_of(&[3, 0, 1])).fenced(0x77));
    assert_ok(&submitted);
    assert_eq!(submitted.fence_id(), 0x77, "fence echoed on the response");

    // The kernel binds the renderer resource to the scanout and flushes.
    assert_ok(&h.run(&set_scanout(0, 10, rect(0, 0, 4, 4))));
    assert_ok(&h.run(&resource_flush(10, rect(0, 0, 4, 4))));
    assert_eq!(h.px(0, 0), RED_RGBA, "3D scanout readback reached the host");
    assert_eq!(h.px(3, 3), RED_RGBA);

    // TRANSFER_FROM_HOST_3D: the guest reads its rendering back.
    h.write_mem(FB_ADDR + 0x1000, &[0u8; 64]);
    assert_ok(&h.run(&Request::new(cmd::RESOURCE_DETACH_BACKING).u32(10).u32(0)));
    assert_ok(&h.run(&attach_backing(10, &[(FB_ADDR + 0x1000, 64)])));
    assert_ok(&h.run(&transfer_3d(
        cmd::TRANSFER_FROM_HOST_3D,
        1,
        10,
        (0, 0, 0, 4, 4, 1),
        0,
    )));
    assert_eq!(
        h.read_mem(FB_ADDR + 0x1000, 64),
        RED_BGRA.repeat(16),
        "readback returned the transferred pixels"
    );

    // Teardown, in the order the kernel uses.
    assert_ok(&h.run(&ctx_detach(1, 10)));
    assert_ok(&h.run(&resource_unref(10)));
    assert_ok(&h.run(&ctx_destroy(1)));
    assert!(!h.needs_reset());
}

/// MVP-309's malicious-guest rule, 3D edition: nothing here may panic, wedge
/// the device or leak into another command's state.
#[test]
fn a_malicious_3d_guest_is_answered_in_band() {
    let mut h = Harness::new(4, 4);

    // Contexts.
    assert_err(&h.run(&ctx_create(0, "zero")), resp::ERR_INVALID_CONTEXT_ID);
    assert_ok(&h.run(&ctx_create(1, "x")));
    assert_err(&h.run(&ctx_create(1, "dup")), resp::ERR_INVALID_CONTEXT_ID);
    assert_err(&h.run(&ctx_destroy(99)), resp::ERR_INVALID_CONTEXT_ID);

    // Resources: id 0, duplicates (across BOTH tables), absurd geometry.
    assert_err(
        &h.run(&create_3d(0, 2, 2, 4, 4, 1)),
        resp::ERR_INVALID_RESOURCE_ID,
    );
    assert_ok(&h.run(&create_3d(10, 2, 2, 4, 4, 1)));
    assert_err(
        &h.run(&create_3d(10, 2, 2, 4, 4, 1)),
        resp::ERR_INVALID_RESOURCE_ID,
    );
    assert_err(
        &h.run(&create_3d(11, 2, 2, u32::MAX, u32::MAX, u32::MAX)),
        resp::ERR_INVALID_PARAMETER,
    );
    assert_err(
        &h.run(&create_3d(11, 2, 2, 4, 0, 1)),
        resp::ERR_INVALID_PARAMETER,
    );

    // Attach/detach: unknown ids and cross-table routing.
    assert_err(
        &h.run(&attach_backing(66, &[(FB_ADDR, 64)])),
        resp::ERR_INVALID_RESOURCE_ID,
    );
    assert_err(&h.run(&ctx_attach(1, 66)), resp::ERR_INVALID_RESOURCE_ID);
    assert_err(&h.run(&ctx_attach(9, 10)), resp::ERR_INVALID_CONTEXT_ID);

    // Transfers: no backing, then a box outside the resource, then an offset
    // past the backing.
    assert_err(
        &h.run(&transfer_3d(
            cmd::TRANSFER_TO_HOST_3D,
            1,
            10,
            (0, 0, 0, 4, 4, 1),
            0,
        )),
        resp::ERR_INVALID_PARAMETER,
    );
    assert_ok(&h.run(&attach_backing(10, &[(FB_ADDR, 64)])));
    assert_err(
        &h.run(&transfer_3d(
            cmd::TRANSFER_TO_HOST_3D,
            1,
            10,
            (1, 0, 0, 4, 4, 1),
            0,
        )),
        resp::ERR_INVALID_PARAMETER,
    );
    assert_err(
        &h.run(&transfer_3d(
            cmd::TRANSFER_TO_HOST_3D,
            1,
            10,
            (0, 0, 0, 4, 4, 1),
            u64::MAX,
        )),
        resp::ERR_INVALID_PARAMETER,
    );
    assert_err(
        &h.run(&transfer_3d(
            cmd::TRANSFER_TO_HOST_3D,
            42,
            10,
            (0, 0, 0, 4, 4, 1),
            0,
        )),
        resp::ERR_INVALID_CONTEXT_ID,
    );

    // Submits: wrong ctx, malformed stream, a stream whose declared size
    // exceeds what the chain carried, and one over the budget.
    assert_err(&h.run(&submit_3d(9, &[])), resp::ERR_INVALID_CONTEXT_ID);
    assert_err(
        &h.run(&submit_3d(1, &u32::MAX.to_le_bytes())),
        resp::ERR_INVALID_PARAMETER,
    );
    let lying = Request::new(cmd::SUBMIT_3D).ctx(1).u32(4096).u32(0); // no stream bytes follow
    assert_err(&h.run(&lying), resp::ERR_INVALID_PARAMETER);
    // Over budget: the gather cap truncates the chain before dispatch, so it
    // must come back as an in-band error, not a wedged device. (The chain
    // itself stays under the test ring's limits: claim > carry.)
    let over = u32::try_from(MAX_SUBMIT_BYTES + 4).expect("fits");
    let claiming = Request::new(cmd::SUBMIT_3D).ctx(1).u32(over).u32(0);
    assert_err(&h.run(&claiming), resp::ERR_INVALID_PARAMETER);

    // Scanout of a 3D resource with a rect outside it.
    assert_err(
        &h.run(&set_scanout(0, 10, rect(0, 0, 5, 5))),
        resp::ERR_INVALID_PARAMETER,
    );
    // Flush of an unref'd 3D resource is an error the guest can see.
    assert_ok(&h.run(&resource_unref(10)));
    assert_err(
        &h.run(&resource_flush(10, rect(0, 0, 4, 4))),
        resp::ERR_INVALID_RESOURCE_ID,
    );
    assert_err(&h.run(&resource_unref(10)), resp::ERR_INVALID_RESOURCE_ID);

    assert!(!h.needs_reset(), "in-band errors never wedge the device");
}

/// A device reset mid-session drops contexts and 3D resources; the driver
/// can start over.
#[test]
fn reset_clears_the_3d_state_and_the_device_comes_back() {
    let mut h = Harness::new(4, 4);
    assert_ok(&h.run(&ctx_create(1, "one")));
    assert_ok(&h.run(&create_3d(10, 2, 2, 4, 4, 1)));

    // Reset via the status register, then bring the device back up.
    h.write32(mmio::STATUS, 0);
    h.bring_up();

    // The old names are gone…
    assert_err(&h.run(&ctx_destroy(1)), resp::ERR_INVALID_CONTEXT_ID);
    assert_err(
        &h.run(&attach_backing(10, &[(FB_ADDR, 64)])),
        resp::ERR_INVALID_RESOURCE_ID,
    );
    // …and can be created afresh.
    assert_ok(&h.run(&ctx_create(1, "again")));
    assert_ok(&h.run(&create_3d(10, 2, 2, 4, 4, 1)));
    assert!(!h.needs_reset());
}
