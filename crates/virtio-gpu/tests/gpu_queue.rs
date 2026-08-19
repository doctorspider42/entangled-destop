//! End-to-end virtio-gpu tests over a real split virtqueue (backlog EPIC 8
//! acceptance criteria, MVP-309's malicious-guest rule).
//!
//! Each test brings the device up exactly the way a driver does — through the
//! virtio-mmio registers, both queues programmed — lays descriptor chains out by
//! hand in a `GuestMemoryMmap`, kicks `QUEUE_NOTIFY` and inspects the used ring,
//! the response header and, crucially, the **pixels**: the device is wired to a
//! windowless `display::DisplayHandle::detached(w, h)`, so
//! `DisplayHandle::screenshot_png()` shows exactly what a window would.
//!
//! Three groups:
//!
//! * "well-behaved driver": the MVP-801…810 pipeline (`GET_DISPLAY_INFO` →
//!   `RESOURCE_CREATE_2D` → `RESOURCE_ATTACH_BACKING` → `SET_SCANOUT` →
//!   `TRANSFER_TO_HOST_2D` → `RESOURCE_FLUSH`), dirty rects, resolution changes,
//!   fences, teardown, reset;
//! * "malicious guest": rects outside the resource, resource id 0 / unknown /
//!   double unref, transfers without backing, backing pages outside guest RAM,
//!   oversized and zero-sized resources, truncated commands, response buffers
//!   too small, looped and interleaved chains. None of them may panic, and none
//!   may change a host pixel;
//! * the wire format, cross-checked: the requests here are encoded by hand (the
//!   guest's half) and must line up with the lengths `virtio_gpu::protocol`
//!   parses.

use std::sync::Arc;

use display::DisplayHandle;
use virtio_core::chain::{VIRTQ_DESC_F_INDIRECT, VIRTQ_DESC_F_NEXT, VIRTQ_DESC_F_WRITE};
use virtio_core::testing::{guest_memory, SplitRing, TestIrqLine};
use virtio_core::{mmio, status, GuestMem, MmioTransport, VirtioDevice, VIRTIO_F_VERSION_1};
use virtio_gpu::protocol::{
    cmd, resp, AttachBacking, Rect, ResourceCreate2d, ResourceFlush, ResourceUnref, SetScanout,
    TransferToHost2d, CTRL_HDR_LEN, DISPLAY_INFO_BODY_LEN, DISPLAY_ONE_LEN, FLAG_FENCE,
    FLAG_INFO_RING_IDX, MEM_ENTRY_LEN,
};
use virtio_gpu::resource::MAX_BACKING_ENTRIES;
use virtio_gpu::{
    GpuDevice, FORMAT_B8G8R8A8_UNORM, FORMAT_B8G8R8X8_UNORM, MAX_RESOURCE_PIXELS, NUM_CAPSETS,
    NUM_SCANOUTS,
};
use vm_memory::{Bytes, GuestAddress};

const MEM_SIZE: u64 = 16 << 20;
const CONTROL_RING_BASE: u64 = 0x1000;
const CURSOR_RING_BASE: u64 = 0x2000;
const RING_SIZE: u16 = 16;
/// Where the guest writes its command buffer.
const REQ_ADDR: u64 = 0x4000;
/// Where the device is asked to write the response.
const RESP_ADDR: u64 = 0x8000;
/// Plenty for the 408-byte `GET_DISPLAY_INFO` reply.
const RESP_CAPACITY: u32 = 1024;
/// Guest "framebuffer" pages, clear of the rings and the command buffers.
const FB_ADDR: u64 = 0x10_0000;

/// BGRA (the guest's byte order) test colors and their RGBA screenshot values.
const RED_BGRA: [u8; 4] = [0x00, 0x00, 0xff, 0xff];
const BLUE_BGRA: [u8; 4] = [0xff, 0x00, 0x00, 0xff];
const GREEN_BGRA: [u8; 4] = [0x00, 0xff, 0x00, 0xff];
const WHITE_BGRA: [u8; 4] = [0xff, 0xff, 0xff, 0xff];
const RED_RGBA: [u8; 4] = [0xff, 0x00, 0x00, 0xff];
const BLUE_RGBA: [u8; 4] = [0x00, 0x00, 0xff, 0xff];
const GREEN_RGBA: [u8; 4] = [0x00, 0xff, 0x00, 0xff];
const WHITE_RGBA: [u8; 4] = [0xff, 0xff, 0xff, 0xff];
const BLACK_RGBA: [u8; 4] = [0x00, 0x00, 0x00, 0xff];

// ===================================================== the guest's half

/// A control command, encoded by hand the way a driver would.
#[derive(Debug, Clone)]
struct Request(Vec<u8>);

impl Request {
    fn new(kind: u32) -> Self {
        Self::fenced(kind, 0, 0)
    }

    fn fenced(kind: u32, flags: u32, fence_id: u64) -> Self {
        let mut raw = vec![0u8; CTRL_HDR_LEN];
        raw[0..4].copy_from_slice(&kind.to_le_bytes());
        raw[4..8].copy_from_slice(&flags.to_le_bytes());
        raw[8..16].copy_from_slice(&fence_id.to_le_bytes());
        Self(raw)
    }

    fn u32(mut self, value: u32) -> Self {
        self.0.extend_from_slice(&value.to_le_bytes());
        self
    }

    fn u64(mut self, value: u64) -> Self {
        self.0.extend_from_slice(&value.to_le_bytes());
        self
    }

    fn rect(self, rect: Rect) -> Self {
        self.u32(rect.x)
            .u32(rect.y)
            .u32(rect.width)
            .u32(rect.height)
    }

    fn bytes(&self) -> &[u8] {
        &self.0
    }

    fn len(&self) -> usize {
        self.0.len()
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

fn get_display_info() -> Request {
    Request::new(cmd::GET_DISPLAY_INFO)
}

fn create_2d(id: u32, format: u32, width: u32, height: u32) -> Request {
    Request::new(cmd::RESOURCE_CREATE_2D)
        .u32(id)
        .u32(format)
        .u32(width)
        .u32(height)
}

fn attach_backing(id: u32, entries: &[(u64, u32)]) -> Request {
    let mut request = Request::new(cmd::RESOURCE_ATTACH_BACKING)
        .u32(id)
        .u32(u32::try_from(entries.len()).expect("test entry counts are small"));
    for &(addr, length) in entries {
        request = request.u64(addr).u32(length).u32(0);
    }
    request
}

/// An attach-backing command that *claims* `nr_entries` but carries `entries`.
fn attach_backing_claiming(id: u32, nr_entries: u32, entries: &[(u64, u32)]) -> Request {
    let mut request = Request::new(cmd::RESOURCE_ATTACH_BACKING)
        .u32(id)
        .u32(nr_entries);
    for &(addr, length) in entries {
        request = request.u64(addr).u32(length).u32(0);
    }
    request
}

fn set_scanout(scanout_id: u32, resource_id: u32, region: Rect) -> Request {
    Request::new(cmd::SET_SCANOUT)
        .rect(region)
        .u32(scanout_id)
        .u32(resource_id)
}

fn transfer_to_host(id: u32, region: Rect, offset: u64) -> Request {
    Request::new(cmd::TRANSFER_TO_HOST_2D)
        .rect(region)
        .u64(offset)
        .u32(id)
        .u32(0)
}

fn resource_flush(id: u32, region: Rect) -> Request {
    Request::new(cmd::RESOURCE_FLUSH)
        .rect(region)
        .u32(id)
        .u32(0)
}

fn resource_unref(id: u32) -> Request {
    Request::new(cmd::RESOURCE_UNREF).u32(id).u32(0)
}

fn detach_backing(id: u32) -> Request {
    Request::new(cmd::RESOURCE_DETACH_BACKING).u32(id).u32(0)
}

/// What the device wrote back.
#[derive(Debug)]
struct Response {
    /// Used-ring length, i.e. bytes the device claims it wrote.
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

    fn flags(&self) -> u32 {
        self.field32(4)
    }

    fn fence_id(&self) -> u64 {
        let bytes: [u8; 8] = self.raw[8..16].try_into().expect("in range");
        u64::from_le_bytes(bytes)
    }

    fn ring_idx(&self) -> u8 {
        self.raw[20]
    }

    fn body(&self) -> &[u8] {
        &self.raw[CTRL_HDR_LEN..]
    }

    /// `(rect, enabled, flags)` of pmode `index` of a display-info reply.
    fn pmode(&self, index: usize) -> (Rect, u32, u32) {
        let at = CTRL_HDR_LEN + index * DISPLAY_ONE_LEN;
        let f = |off: usize| self.field32(at + off);
        (
            rect(f(0), f(4), f(8), f(12)),
            self.field32(at + 16),
            self.field32(at + 20),
        )
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

fn assert_err(response: &Response, expected: u32) {
    assert_eq!(
        response.kind(),
        expected,
        "expected {:#06x}, got {:#06x}",
        expected,
        response.kind()
    );
    assert_eq!(
        response.used_len, CTRL_HDR_LEN as u32,
        "error replies are header-only"
    );
}

/// A decoded screenshot, in PNG's RGBA order.
struct Image {
    width: u32,
    height: u32,
    rgba: Vec<u8>,
}

impl Image {
    fn px(&self, x: u32, y: u32) -> [u8; 4] {
        assert!(x < self.width && y < self.height, "pixel out of the image");
        let at = (y as usize * self.width as usize + x as usize) * 4;
        self.rgba[at..at + 4].try_into().expect("in range")
    }
}

// ============================================================== harness

/// The device behind an mmio transport, both rings, and the display it paints.
struct Harness {
    mem: Arc<GuestMem>,
    control: SplitRing,
    cursor: SplitRing,
    transport: MmioTransport,
    irq: Arc<TestIrqLine>,
    display: DisplayHandle,
}

impl Harness {
    fn new(width: u32, height: u32) -> Self {
        let display = DisplayHandle::detached(width, height).expect("detached display");
        let device = GpuDevice::new(display.clone());
        let mem = Arc::new(guest_memory(MEM_SIZE));
        let irq = Arc::new(TestIrqLine::default());
        let transport = MmioTransport::new(0, Box::new(device), Arc::clone(&mem), irq.clone())
            .expect("transport accepts the device");
        let mut harness = Harness {
            mem,
            control: SplitRing::layout(CONTROL_RING_BASE, RING_SIZE),
            cursor: SplitRing::layout(CURSOR_RING_BASE, RING_SIZE),
            transport,
            irq,
            display,
        };
        harness.bring_up();
        harness
    }

    // ---------------------------------------------------------- registers

    fn read32(&mut self, offset: u64) -> u32 {
        let mut data = [0u8; 4];
        self.transport.read(offset, &mut data);
        u32::from_le_bytes(data)
    }

    fn write32(&mut self, offset: u64, value: u32) {
        self.transport.write(offset, &value.to_le_bytes());
    }

    /// Full driver bring-up: features, both queues, DRIVER_OK.
    fn bring_up(&mut self) {
        assert_eq!(self.read32(mmio::MAGIC_VALUE), mmio::MAGIC);
        assert_eq!(self.read32(mmio::VERSION_REG), 2);
        assert_eq!(self.read32(mmio::DEVICE_ID), 16, "virtio-gpu device id");

        self.write32(mmio::DEVICE_FEATURES_SEL, 0);
        let low = self.read32(mmio::DEVICE_FEATURES);
        self.write32(mmio::DEVICE_FEATURES_SEL, 1);
        let high = self.read32(mmio::DEVICE_FEATURES);

        self.write32(mmio::STATUS, status::ACKNOWLEDGE);
        self.write32(mmio::STATUS, status::ACKNOWLEDGE | status::DRIVER);
        self.write32(mmio::DRIVER_FEATURES_SEL, 0);
        self.write32(mmio::DRIVER_FEATURES, low);
        self.write32(mmio::DRIVER_FEATURES_SEL, 1);
        self.write32(mmio::DRIVER_FEATURES, high);
        self.write32(
            mmio::STATUS,
            status::ACKNOWLEDGE | status::DRIVER | status::FEATURES_OK,
        );
        assert_ne!(
            self.transport.status() & status::FEATURES_OK,
            0,
            "device must accept the features it offered"
        );

        for (index, ring) in [(0u32, self.control), (1, self.cursor)] {
            self.write32(mmio::QUEUE_SEL, index);
            assert!(self.read32(mmio::QUEUE_NUM_MAX) >= u32::from(RING_SIZE));
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

    fn device_features(&mut self) -> u64 {
        self.write32(mmio::DEVICE_FEATURES_SEL, 0);
        let low = u64::from(self.read32(mmio::DEVICE_FEATURES));
        self.write32(mmio::DEVICE_FEATURES_SEL, 1);
        let high = u64::from(self.read32(mmio::DEVICE_FEATURES));
        low | (high << 32)
    }

    fn config(&mut self) -> [u32; 4] {
        let mut raw = [0u8; 16];
        self.transport.read(mmio::CONFIG_SPACE, &mut raw);
        let word = |i: usize| {
            let bytes: [u8; 4] = raw[i * 4..i * 4 + 4].try_into().expect("in range");
            u32::from_le_bytes(bytes)
        };
        [word(0), word(1), word(2), word(3)]
    }

    // ------------------------------------------------------- guest memory

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

    /// Chains `descs` (address, length, extra flags) from descriptor index 0 of
    /// `ring` and publishes the head.
    fn submit(&self, ring: &SplitRing, descs: &[(u64, u32, u16)]) {
        let last = descs.len().saturating_sub(1);
        for (i, &(addr, len, flags)) in descs.iter().enumerate() {
            let index = u16::try_from(i).expect("test chains stay small");
            let (flags, next) = if i == last {
                (flags, 0)
            } else {
                (flags | VIRTQ_DESC_F_NEXT, index + 1)
            };
            ring.write_desc(&self.mem, index, addr, len, flags, next);
        }
        ring.publish(&self.mem, 0);
    }

    fn notify(&mut self, queue: u32) {
        self.write32(mmio::QUEUE_NOTIFY, queue);
    }

    /// `(head, len)` of the most recent used-ring entry of `ring`.
    fn last_used(&self, ring: &SplitRing) -> (u32, u32) {
        let idx = ring.used_idx(&self.mem);
        assert!(idx > 0, "device did not add anything to the used ring");
        ring.used_elem(&self.mem, (idx - 1) % RING_SIZE)
    }

    // ---------------------------------------------------------- commands

    /// Runs one command through the control queue: request in one descriptor,
    /// response in one [`RESP_CAPACITY`]-byte descriptor.
    fn run(&mut self, request: &Request) -> Response {
        self.run_with_capacity(request, RESP_CAPACITY)
    }

    fn run_with_capacity(&mut self, request: &Request, capacity: u32) -> Response {
        self.write_mem(REQ_ADDR, request.bytes());
        // Poison the response buffer: a device that writes nothing is caught.
        self.write_mem(RESP_ADDR, &vec![0xff; capacity as usize]);
        let len = u32::try_from(request.len()).expect("test requests are small");
        let control = self.control;
        self.submit(
            &control,
            &[
                (REQ_ADDR, len, 0),
                (RESP_ADDR, capacity, VIRTQ_DESC_F_WRITE),
            ],
        );
        self.notify(0);
        let (head, used_len) = self.last_used(&control);
        assert_eq!(head, 0, "used ring must report the chain head");
        Response {
            used_len,
            raw: self.read_mem(RESP_ADDR, capacity as usize),
        }
    }

    /// Same, but the request and the response are each split over two
    /// descriptors — the gather/scatter path a real driver can produce.
    fn run_split(&mut self, request: &Request) -> Response {
        let bytes = request.bytes();
        let head_len = u32::try_from(CTRL_HDR_LEN).expect("small");
        let tail_len = u32::try_from(bytes.len() - CTRL_HDR_LEN).expect("small");
        self.write_mem(REQ_ADDR, &bytes[..CTRL_HDR_LEN]);
        self.write_mem(REQ_ADDR + 0x100, &bytes[CTRL_HDR_LEN..]);
        self.write_mem(RESP_ADDR, &vec![0xff; RESP_CAPACITY as usize]);
        let control = self.control;
        // 12 bytes of response in the first buffer, the rest in the second:
        // the header itself straddles the descriptor boundary.
        self.submit(
            &control,
            &[
                (REQ_ADDR, head_len, 0),
                (REQ_ADDR + 0x100, tail_len, 0),
                (RESP_ADDR, 12, VIRTQ_DESC_F_WRITE),
                (RESP_ADDR + 12, RESP_CAPACITY - 12, VIRTQ_DESC_F_WRITE),
            ],
        );
        self.notify(0);
        let (head, used_len) = self.last_used(&control);
        assert_eq!(head, 0);
        Response {
            used_len,
            raw: self.read_mem(RESP_ADDR, RESP_CAPACITY as usize),
        }
    }

    /// create → attach → set_scanout, each asserted OK.
    fn setup_scanout(&mut self, id: u32, width: u32, height: u32, backing: u64) {
        assert_ok(&self.run(&create_2d(id, FORMAT_B8G8R8X8_UNORM, width, height)));
        let bytes = width * height * 4;
        assert_ok(&self.run(&attach_backing(id, &[(backing, bytes)])));
        assert_ok(&self.run(&set_scanout(0, id, rect(0, 0, width, height))));
    }

    /// Writes a BGRA quadrant pattern (red, blue / green, white) at `addr`.
    fn write_quadrants(&self, addr: u64, width: u32, height: u32) {
        let mut image = Vec::with_capacity((width * height * 4) as usize);
        for y in 0..height {
            for x in 0..width {
                let color = match (x < width / 2, y < height / 2) {
                    (true, true) => RED_BGRA,
                    (false, true) => BLUE_BGRA,
                    (true, false) => GREEN_BGRA,
                    (false, false) => WHITE_BGRA,
                };
                image.extend_from_slice(&color);
            }
        }
        self.write_mem(addr, &image);
    }

    /// Fills `width`×`height` guest pixels at `addr` with one BGRA color.
    fn fill_backing(&self, addr: u64, width: u32, height: u32, color: [u8; 4]) {
        let mut image = Vec::with_capacity((width * height * 4) as usize);
        for _ in 0..width * height {
            image.extend_from_slice(&color);
        }
        self.write_mem(addr, &image);
    }

    /// The host's current scanout, decoded from a PNG screenshot (MVP-707).
    fn screenshot(&self) -> Image {
        let png_bytes = self.display.screenshot_png().expect("screenshot encodes");
        assert_eq!(&png_bytes[1..4], b"PNG");
        let decoder = png::Decoder::new(std::io::Cursor::new(&png_bytes));
        let mut reader = decoder.read_info().expect("PNG header");
        let mut rgba = vec![0; reader.output_buffer_size().expect("buffer size")];
        let info = reader.next_frame(&mut rgba).expect("PNG frame");
        assert_eq!(info.color_type, png::ColorType::Rgba);
        rgba.truncate(info.buffer_size());
        Image {
            width: info.width,
            height: info.height,
            rgba,
        }
    }
}

// ===================================================== well-behaved driver

#[test]
fn hand_encoded_requests_match_the_parsers_wire_lengths() {
    // The requests above are written like a driver writes them; if these
    // lengths ever disagree with `virtio_gpu::protocol`, one of the two is
    // wrong about the spec.
    assert_eq!(get_display_info().len(), CTRL_HDR_LEN);
    assert_eq!(create_2d(1, 2, 3, 4).len(), ResourceCreate2d::LEN);
    assert_eq!(resource_unref(1).len(), ResourceUnref::LEN);
    assert_eq!(detach_backing(1).len(), ResourceUnref::LEN);
    assert_eq!(set_scanout(0, 1, rect(0, 0, 1, 1)).len(), SetScanout::LEN);
    assert_eq!(
        resource_flush(1, rect(0, 0, 1, 1)).len(),
        ResourceFlush::LEN
    );
    assert_eq!(
        transfer_to_host(1, rect(0, 0, 1, 1), 0).len(),
        TransferToHost2d::LEN
    );
    assert_eq!(attach_backing(1, &[]).len(), AttachBacking::LEN);
    assert_eq!(
        attach_backing(1, &[(0, 1), (1, 2)]).len(),
        AttachBacking::LEN + 2 * MEM_ENTRY_LEN
    );
}

#[test]
fn advertises_the_gpu_identity_features_and_config_space() {
    let mut h = Harness::new(64, 64);
    let features = h.device_features();
    assert_ne!(features & VIRTIO_F_VERSION_1, 0);
    assert_ne!(
        features & virtio_gpu::VIRTIO_GPU_F_EDID,
        0,
        "EDID is offered (MVP-811): GNOME/mutter builds its outputs from it"
    );
    assert_eq!(
        features,
        VIRTIO_F_VERSION_1 | virtio_gpu::VIRTIO_GPU_F_EDID,
        "no VIRGL (3D is post-MVP), no blob resources"
    );

    let [events_read, events_clear, num_scanouts, num_capsets] = h.config();
    assert_eq!(events_read, 0);
    assert_eq!(events_clear, 0, "write-to-clear register reads zero");
    assert_eq!(num_scanouts, NUM_SCANOUTS);
    assert_eq!(num_capsets, NUM_CAPSETS);

    // Writing events_clear is accepted (and clears nothing, since no events
    // are ever raised); reads past the config space are zeroes, not panics.
    h.transport
        .write(mmio::CONFIG_SPACE + 4, &1u32.to_le_bytes());
    assert_eq!(h.config()[0], 0);
    h.transport.write(mmio::CONFIG_SPACE, &7u32.to_le_bytes());
    assert_eq!(h.config()[2], NUM_SCANOUTS, "num_scanouts is read-only");
    let mut far = [0xffu8; 8];
    h.transport.read(mmio::CONFIG_SPACE + 0x400, &mut far);
    assert_eq!(far, [0u8; 8]);
}

#[test]
fn get_display_info_reports_one_enabled_scanout() {
    let mut h = Harness::new(1920, 1080);
    let response = h.run(&get_display_info());
    assert_eq!(response.kind(), resp::OK_DISPLAY_INFO);
    assert_eq!(
        response.used_len as usize,
        CTRL_HDR_LEN + DISPLAY_INFO_BODY_LEN,
        "408-byte display-info reply"
    );

    let (region, enabled, flags) = response.pmode(0);
    assert_eq!(region, rect(0, 0, 1920, 1080));
    assert_eq!(enabled, 1);
    assert_eq!(flags, 0);
    // Every other pmode is disabled.
    for index in 1..16 {
        let (region, enabled, _) = response.pmode(index);
        assert_eq!(enabled, 0, "pmode {index} must be disabled");
        assert_eq!(region, rect(0, 0, 0, 0));
    }
    assert!(
        response.body()[DISPLAY_INFO_BODY_LEN..]
            .iter()
            .all(|b| *b == 0xff),
        "the device must not scribble past its response"
    );
}

/// The EPIC 8 headline: guest pixels really do reach the host scanout.
#[test]
fn full_pipeline_paints_a_quadrant_pattern_on_the_host_scanout() {
    const SIDE: u32 = 64;
    let mut h = Harness::new(SIDE, SIDE);

    // Black before the guest does anything.
    let before = h.screenshot();
    assert_eq!((before.width, before.height), (SIDE, SIDE));
    assert_eq!(before.px(0, 0), BLACK_RGBA);

    // create → attach → set_scanout → transfer → flush.
    h.write_quadrants(FB_ADDR, SIDE, SIDE);
    h.setup_scanout(1, SIDE, SIDE, FB_ADDR);
    assert_ok(&h.run(&transfer_to_host(1, rect(0, 0, SIDE, SIDE), 0)));
    assert_ok(&h.run(&resource_flush(1, rect(0, 0, SIDE, SIDE))));

    let image = h.screenshot();
    assert_eq!((image.width, image.height), (SIDE, SIDE));
    // Quadrant centres…
    assert_eq!(image.px(15, 15), RED_RGBA, "top-left quadrant");
    assert_eq!(image.px(47, 15), BLUE_RGBA, "top-right quadrant");
    assert_eq!(image.px(15, 47), GREEN_RGBA, "bottom-left quadrant");
    assert_eq!(image.px(47, 47), WHITE_RGBA, "bottom-right quadrant");
    // …and the exact corners, which is where a stride bug shows up first.
    assert_eq!(image.px(0, 0), RED_RGBA);
    assert_eq!(image.px(SIDE - 1, 0), BLUE_RGBA);
    assert_eq!(image.px(0, SIDE - 1), GREEN_RGBA);
    assert_eq!(image.px(SIDE - 1, SIDE - 1), WHITE_RGBA);
    // The seam between quadrants is exactly in the middle.
    assert_eq!(image.px(SIDE / 2 - 1, 0), RED_RGBA);
    assert_eq!(image.px(SIDE / 2, 0), BLUE_RGBA);

    // The driver was notified, with the vring interrupt bit set.
    assert!(h.irq.count() > 0, "device must signal used buffers");
    assert_ne!(h.transport.interrupt_status() & mmio::INT_VRING, 0);
    h.write32(mmio::INTERRUPT_ACK, mmio::INT_VRING);
    assert_eq!(h.transport.interrupt_status() & mmio::INT_VRING, 0);

    // Copy statistics prove the pixels went through the scanout mirror once.
    let stats = h.display.stats();
    assert_eq!(stats.rejected, 0);
    assert_eq!(stats.bytes, u64::from(SIDE * SIDE * 4));
}

#[test]
fn the_pipeline_works_with_split_request_and_response_descriptors() {
    const SIDE: u32 = 32;
    let mut h = Harness::new(SIDE, SIDE);
    h.fill_backing(FB_ADDR, SIDE, SIDE, RED_BGRA);

    // Every command of the pipeline through split chains, including the
    // 408-byte display-info reply whose header straddles two descriptors.
    let info = h.run_split(&get_display_info());
    assert_eq!(info.kind(), resp::OK_DISPLAY_INFO);
    assert_eq!(info.pmode(0).0, rect(0, 0, SIDE, SIDE));

    assert_ok(&h.run_split(&create_2d(1, FORMAT_B8G8R8A8_UNORM, SIDE, SIDE)));
    assert_ok(&h.run_split(&attach_backing(1, &[(FB_ADDR, SIDE * SIDE * 4)])));
    assert_ok(&h.run_split(&set_scanout(0, 1, rect(0, 0, SIDE, SIDE))));
    assert_ok(&h.run_split(&transfer_to_host(1, rect(0, 0, SIDE, SIDE), 0)));
    assert_ok(&h.run_split(&resource_flush(1, rect(0, 0, SIDE, SIDE))));
    assert_eq!(h.screenshot().px(SIDE / 2, SIDE / 2), RED_RGBA);
}

#[test]
fn backing_spread_over_many_entries_is_reassembled_in_order() {
    const W: u32 = 8;
    const H: u32 = 8;
    let mut h = Harness::new(W, H);
    // Four pages of two rows each, deliberately out of address order so a
    // device that sorted or ignored the list would produce the wrong image.
    let rows_per_page = 2;
    let page_bytes = W * rows_per_page * 4;
    let pages = [
        FB_ADDR + 0x3000,
        FB_ADDR + 0x1000,
        FB_ADDR + 0x2000,
        FB_ADDR,
    ];
    let colors = [RED_BGRA, BLUE_BGRA, GREEN_BGRA, WHITE_BGRA];
    for (page, color) in pages.iter().zip(colors) {
        h.fill_backing(*page, W, rows_per_page, color);
    }

    assert_ok(&h.run(&create_2d(1, FORMAT_B8G8R8X8_UNORM, W, H)));
    let entries: Vec<(u64, u32)> = pages.iter().map(|addr| (*addr, page_bytes)).collect();
    assert_ok(&h.run(&attach_backing(1, &entries)));
    assert_ok(&h.run(&set_scanout(0, 1, rect(0, 0, W, H))));
    assert_ok(&h.run(&transfer_to_host(1, rect(0, 0, W, H), 0)));
    assert_ok(&h.run(&resource_flush(1, rect(0, 0, W, H))));

    let image = h.screenshot();
    for (band, expected) in [RED_RGBA, BLUE_RGBA, GREEN_RGBA, WHITE_RGBA]
        .into_iter()
        .enumerate()
    {
        let y = band as u32 * rows_per_page;
        assert_eq!(image.px(0, y), expected, "band {band} row 0");
        assert_eq!(image.px(W - 1, y + 1), expected, "band {band} row 1");
    }
}

#[test]
fn dirty_rect_flush_updates_only_the_flushed_region() {
    // MVP-810: a flush must move exactly the rect it names.
    const SIDE: u32 = 16;
    let mut h = Harness::new(SIDE, SIDE);
    h.fill_backing(FB_ADDR, SIDE, SIDE, RED_BGRA);
    h.setup_scanout(1, SIDE, SIDE, FB_ADDR);
    assert_ok(&h.run(&transfer_to_host(1, rect(0, 0, SIDE, SIDE), 0)));
    assert_ok(&h.run(&resource_flush(1, rect(0, 0, SIDE, SIDE))));
    assert_eq!(h.screenshot().px(0, 0), RED_RGBA);

    // The guest repaints its whole buffer blue but only transfers and flushes
    // a 4x4 block at (4,4): everything else on the host must stay red.
    h.fill_backing(FB_ADDR, SIDE, SIDE, BLUE_BGRA);
    let stride = u64::from(SIDE) * 4;
    let block = rect(4, 4, 4, 4);
    let offset = stride * 4 + 4 * 4;
    assert_ok(&h.run(&transfer_to_host(1, block, offset)));
    assert_ok(&h.run(&resource_flush(1, block)));

    let image = h.screenshot();
    assert_eq!(image.px(4, 4), BLUE_RGBA, "inside the dirty rect");
    assert_eq!(image.px(7, 7), BLUE_RGBA, "inside the dirty rect");
    assert_eq!(image.px(3, 4), RED_RGBA, "one pixel left of the rect");
    assert_eq!(image.px(8, 7), RED_RGBA, "one pixel right of the rect");
    assert_eq!(image.px(4, 3), RED_RGBA, "one row above the rect");
    assert_eq!(image.px(7, 8), RED_RGBA, "one row below the rect");
    assert_eq!(image.px(0, 0), RED_RGBA);
    assert_eq!(image.px(SIDE - 1, SIDE - 1), RED_RGBA);
}

#[test]
fn set_scanout_with_a_different_size_changes_the_host_resolution() {
    let mut h = Harness::new(64, 64);
    assert_eq!(h.display.resolution(), (64, 64));

    // A 32x16 resource bound in full: the host follows the guest's mode.
    h.fill_backing(FB_ADDR, 32, 16, GREEN_BGRA);
    h.setup_scanout(1, 32, 16, FB_ADDR);
    assert_eq!(h.display.resolution(), (32, 16));
    assert_eq!(h.display.stats().resolutions, 1);

    assert_ok(&h.run(&transfer_to_host(1, rect(0, 0, 32, 16), 0)));
    assert_ok(&h.run(&resource_flush(1, rect(0, 0, 32, 16))));
    let image = h.screenshot();
    assert_eq!((image.width, image.height), (32, 16));
    assert_eq!(image.px(31, 15), GREEN_RGBA);

    // GET_DISPLAY_INFO now reports the new mode.
    assert_eq!(h.run(&get_display_info()).pmode(0).0, rect(0, 0, 32, 16));

    // Binding a region of the same size again does not touch the resolution.
    assert_ok(&h.run(&set_scanout(0, 1, rect(0, 0, 32, 16))));
    assert_eq!(h.display.stats().resolutions, 1);

    // A *sub*-region is a smaller mode: 16x8 out of the same resource.
    assert_ok(&h.run(&set_scanout(0, 1, rect(4, 2, 16, 8))));
    assert_eq!(h.display.resolution(), (16, 8));
    assert_eq!(h.display.stats().resolutions, 2);
    // Flushing the whole resource clips to the visible region and lands at the
    // scanout origin, not at the resource origin.
    assert_ok(&h.run(&resource_flush(1, rect(0, 0, 32, 16))));
    let image = h.screenshot();
    assert_eq!((image.width, image.height), (16, 8));
    assert_eq!(image.px(0, 0), GREEN_RGBA);
    assert_eq!(image.px(15, 7), GREEN_RGBA);

    // Resource 0 disables the scanout; later flushes are harmless no-ops.
    assert_ok(&h.run(&set_scanout(0, 0, rect(0, 0, 0, 0))));
    assert_ok(&h.run(&resource_flush(1, rect(0, 0, 32, 16))));
    assert_eq!(h.display.resolution(), (16, 8), "disabling keeps the mode");
}

#[test]
fn a_1920x1080_frame_survives_the_round_trip() {
    // Acceptance criterion: "działa minimum 1920×1080".
    const W: u32 = 1920;
    const H: u32 = 1080;
    let mut h = Harness::new(W, H);
    h.write_quadrants(FB_ADDR, W, H);
    // Three uneven backing entries, the way a fragmented guest allocation
    // arrives (the last one carries the remainder).
    let total = W * H * 4;
    let first = 0x40_0000u32;
    let second = 0x20_0000u32;
    let third = total - first - second;
    assert_ok(&h.run(&create_2d(1, FORMAT_B8G8R8X8_UNORM, W, H)));
    assert_ok(&h.run(&attach_backing(
        1,
        &[
            (FB_ADDR, first),
            (FB_ADDR + u64::from(first), second),
            (FB_ADDR + u64::from(first) + u64::from(second), third),
        ],
    )));
    assert_ok(&h.run(&set_scanout(0, 1, rect(0, 0, W, H))));
    assert_ok(&h.run(&transfer_to_host(1, rect(0, 0, W, H), 0)));
    assert_ok(&h.run(&resource_flush(1, rect(0, 0, W, H))));

    let image = h.screenshot();
    assert_eq!((image.width, image.height), (W, H));
    assert_eq!(image.px(0, 0), RED_RGBA);
    assert_eq!(image.px(W - 1, 0), BLUE_RGBA);
    assert_eq!(image.px(0, H - 1), GREEN_RGBA);
    assert_eq!(image.px(W - 1, H - 1), WHITE_RGBA);
    assert_eq!(image.px(W / 2, H / 2), WHITE_RGBA);
    assert_eq!(image.px(W / 2 - 1, H / 2 - 1), RED_RGBA);
}

#[test]
fn fenced_commands_echo_the_fence_in_the_response() {
    let mut h = Harness::new(16, 16);
    let fenced = Request::fenced(cmd::RESOURCE_CREATE_2D, FLAG_FENCE, 0xfeed_face)
        .u32(1)
        .u32(FORMAT_B8G8R8X8_UNORM)
        .u32(16)
        .u32(16);
    let response = h.run(&fenced);
    assert_eq!(response.kind(), resp::OK_NODATA);
    assert_eq!(response.flags(), FLAG_FENCE);
    assert_eq!(response.fence_id(), 0xfeed_face);

    // Fences are echoed on error replies too, or the driver would wait forever.
    let fenced_bad = Request::fenced(cmd::RESOURCE_CREATE_2D, FLAG_FENCE, 0x1234)
        .u32(0)
        .u32(FORMAT_B8G8R8X8_UNORM)
        .u32(16)
        .u32(16);
    let response = h.run(&fenced_bad);
    assert_eq!(response.kind(), resp::ERR_INVALID_RESOURCE_ID);
    assert_eq!(response.flags(), FLAG_FENCE);
    assert_eq!(response.fence_id(), 0x1234);

    // With INFO_RING_IDX the ring index comes back as well.
    let mut ringed = Request::fenced(
        cmd::GET_DISPLAY_INFO,
        FLAG_FENCE | FLAG_INFO_RING_IDX,
        0x5555,
    );
    ringed.0[20] = 2;
    let response = h.run(&ringed);
    assert_eq!(response.kind(), resp::OK_DISPLAY_INFO);
    assert_eq!(response.flags(), FLAG_FENCE | FLAG_INFO_RING_IDX);
    assert_eq!(response.fence_id(), 0x5555);
    assert_eq!(response.ring_idx(), 2);

    // An unfenced command gets a zeroed fence.
    let response = h.run(&get_display_info());
    assert_eq!(response.flags(), 0);
    assert_eq!(response.fence_id(), 0);
    assert_eq!(response.ring_idx(), 0);
}

#[test]
fn detach_and_unref_tear_the_resource_down() {
    const SIDE: u32 = 16;
    let mut h = Harness::new(SIDE, SIDE);
    h.fill_backing(FB_ADDR, SIDE, SIDE, RED_BGRA);
    h.setup_scanout(1, SIDE, SIDE, FB_ADDR);
    assert_ok(&h.run(&transfer_to_host(1, rect(0, 0, SIDE, SIDE), 0)));
    assert_ok(&h.run(&resource_flush(1, rect(0, 0, SIDE, SIDE))));

    // Detaching the backing keeps the resource (and the host pixels) alive but
    // makes transfers fail.
    assert_ok(&h.run(&detach_backing(1)));
    assert_err(
        &h.run(&transfer_to_host(1, rect(0, 0, SIDE, SIDE), 0)),
        resp::ERR_INVALID_PARAMETER,
    );
    assert_ok(&h.run(&resource_flush(1, rect(0, 0, SIDE, SIDE))));
    assert_eq!(h.screenshot().px(0, 0), RED_RGBA);

    // Re-attaching works, and so does the pipeline afterwards.
    h.fill_backing(FB_ADDR, SIDE, SIDE, BLUE_BGRA);
    assert_ok(&h.run(&attach_backing(1, &[(FB_ADDR, SIDE * SIDE * 4)])));
    assert_ok(&h.run(&transfer_to_host(1, rect(0, 0, SIDE, SIDE), 0)));
    assert_ok(&h.run(&resource_flush(1, rect(0, 0, SIDE, SIDE))));
    assert_eq!(h.screenshot().px(0, 0), BLUE_RGBA);

    // Unref drops the resource and the scanout binding with it.
    assert_ok(&h.run(&resource_unref(1)));
    assert_err(
        &h.run(&resource_flush(1, rect(0, 0, SIDE, SIDE))),
        resp::ERR_INVALID_RESOURCE_ID,
    );
    assert_err(
        &h.run(&transfer_to_host(1, rect(0, 0, SIDE, SIDE), 0)),
        resp::ERR_INVALID_RESOURCE_ID,
    );
    assert_err(
        &h.run(&set_scanout(0, 1, rect(0, 0, SIDE, SIDE))),
        resp::ERR_INVALID_RESOURCE_ID,
    );
    assert_err(&h.run(&resource_unref(1)), resp::ERR_INVALID_RESOURCE_ID);
    assert_err(&h.run(&detach_backing(1)), resp::ERR_INVALID_RESOURCE_ID);
    // The host still shows the last frame; nothing crashed.
    assert_eq!(h.screenshot().px(0, 0), BLUE_RGBA);

    // The id can be recycled.
    assert_ok(&h.run(&create_2d(1, FORMAT_B8G8R8X8_UNORM, SIDE, SIDE)));
}

/// A well-formed `virtio_gpu_update_cursor` for either cursor command.
fn cursor_command(kind: u32, x: u32, y: u32, resource_id: u32, hot: (u32, u32)) -> Request {
    Request::new(kind)
        .u32(0) // pos.scanout_id
        .u32(x)
        .u32(y)
        .u32(0) // pos.padding
        .u32(resource_id)
        .u32(hot.0)
        .u32(hot.1)
        .u32(0) // padding
}

impl Harness {
    /// Submits one chain on the cursor queue (no device-writable part — that
    /// is how Linux submits cursor commands) and asserts it comes back with a
    /// zero used length and a healthy device.
    fn run_cursor(&mut self, request: &Request) {
        self.write_mem(REQ_ADDR, request.bytes());
        let cursor = self.cursor;
        let len = u32::try_from(request.len()).expect("small");
        self.submit(&cursor, &[(REQ_ADDR, len, 0)]);
        self.notify(1);
        let (head, used_len) = self.last_used(&cursor);
        assert_eq!(head, 0);
        assert_eq!(used_len, 0, "cursor commands carry no response payload");
        assert_eq!(self.transport.status() & status::DEVICE_NEEDS_RESET, 0);
    }
}

#[test]
fn cursor_queue_chains_are_returned_even_when_malformed() {
    // A truncated cursor command is dropped, but the buffers must come back:
    // Linux sleeps on a full cursor queue.
    let mut h = Harness::new(16, 16);
    let truncated = Request::new(cmd::UPDATE_CURSOR).u32(0).u32(0);
    h.run_cursor(&truncated);

    // The control queue is unaffected.
    assert_eq!(h.run(&get_display_info()).kind(), resp::OK_DISPLAY_INFO);
}

#[test]
fn the_cursor_plane_is_composited_moved_and_hidden() {
    // MVP-812: a 2x2 opaque red cursor over a blue 16x16 scanout.
    let mut h = Harness::new(16, 16);
    h.fill_backing(FB_ADDR, 16, 16, BLUE_BGRA);
    h.setup_scanout(1, 16, 16, FB_ADDR);
    assert_ok(&h.run(&transfer_to_host(1, rect(0, 0, 16, 16), 0)));
    assert_ok(&h.run(&resource_flush(1, rect(0, 0, 16, 16))));

    // The cursor image is its own 2D resource; ARGB (B8G8R8A8) with a=0xff is
    // premultiplied opaque red.
    let cursor_fb = FB_ADDR + 0x4000;
    assert_ok(&h.run(&create_2d(2, FORMAT_B8G8R8A8_UNORM, 2, 2)));
    h.fill_backing(cursor_fb, 2, 2, RED_BGRA);
    assert_ok(&h.run(&attach_backing(2, &[(cursor_fb, 2 * 2 * 4)])));
    assert_ok(&h.run(&transfer_to_host(2, rect(0, 0, 2, 2), 0)));

    // Show it with the hotspot (1,1) landing on (8,8): the image's top-left
    // pixel is at (7,7).
    h.run_cursor(&cursor_command(cmd::UPDATE_CURSOR, 8, 8, 2, (1, 1)));
    let shot = h.screenshot();
    assert_eq!(shot.px(7, 7), RED_RGBA, "cursor pixel");
    assert_eq!(shot.px(8, 8), RED_RGBA, "hotspot pixel");
    assert_eq!(shot.px(0, 0), BLUE_RGBA, "base pixels are untouched");
    assert_eq!(shot.px(9, 9), BLUE_RGBA, "past the 2x2 image");

    // Move it to the top-left corner: the old position is restored from the
    // guest pixels, and the clip handles the hotspot hanging off the edge.
    h.run_cursor(&cursor_command(cmd::MOVE_CURSOR, 0, 0, 0, (0, 0)));
    let shot = h.screenshot();
    assert_eq!(shot.px(0, 0), RED_RGBA, "cursor followed the move");
    assert_eq!(shot.px(8, 8), BLUE_RGBA, "old position restored");

    // Resource 0 hides the plane.
    h.run_cursor(&cursor_command(cmd::UPDATE_CURSOR, 0, 0, 0, (0, 0)));
    let shot = h.screenshot();
    assert_eq!(shot.px(0, 0), BLUE_RGBA, "cursor hidden");

    // And the guest's own framebuffer was never written to compose any of it.
    assert_eq!(h.read_mem(FB_ADDR, 4), BLUE_BGRA.to_vec());
}

#[test]
fn bogus_cursor_commands_are_dropped_without_breaking_the_device() {
    let mut h = Harness::new(16, 16);
    h.fill_backing(FB_ADDR, 16, 16, GREEN_BGRA);
    h.setup_scanout(1, 16, 16, FB_ADDR);
    assert_ok(&h.run(&transfer_to_host(1, rect(0, 0, 16, 16), 0)));
    assert_ok(&h.run(&resource_flush(1, rect(0, 0, 16, 16))));

    // Unknown resource, bogus scanout, and an image over the cursor cap: each
    // is dropped, the chain comes back, nothing shows.
    h.run_cursor(&cursor_command(cmd::UPDATE_CURSOR, 4, 4, 99, (0, 0)));
    let huge = virtio_gpu::MAX_CURSOR_DIM + 4;
    assert_ok(&h.run(&create_2d(3, FORMAT_B8G8R8A8_UNORM, huge, 4)));
    h.run_cursor(&cursor_command(cmd::UPDATE_CURSOR, 4, 4, 3, (0, 0)));
    let mut bad_scanout = cursor_command(cmd::UPDATE_CURSOR, 4, 4, 1, (0, 0));
    bad_scanout.0[CTRL_HDR_LEN..CTRL_HDR_LEN + 4].copy_from_slice(&7u32.to_le_bytes());
    h.run_cursor(&bad_scanout);

    let shot = h.screenshot();
    assert_eq!(shot.px(4, 4), GREEN_RGBA, "no cursor was composited");
    // The control queue still works.
    assert_eq!(h.run(&get_display_info()).kind(), resp::OK_DISPLAY_INFO);
}

#[test]
fn get_edid_returns_a_valid_block_for_the_scanout() {
    let mut h = Harness::new(1920, 1080);
    let request = Request::new(cmd::GET_EDID).u32(0).u32(0);
    let response = h.run_with_capacity(&request, 2048);
    assert_eq!(response.kind(), resp::OK_EDID);
    assert_eq!(
        response.used_len as usize,
        CTRL_HDR_LEN + 8 + 1024,
        "struct virtio_gpu_resp_edid is 1056 bytes"
    );
    let body = response.body();
    let size = u32::from_le_bytes(body[0..4].try_into().expect("4 bytes"));
    assert_eq!(size, 128, "one EDID base block");
    let edid = &body[8..8 + 128];
    assert_eq!(&edid[0..8], &[0x00, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0x00]);
    let sum: u8 = edid.iter().fold(0u8, |acc, b| acc.wrapping_add(*b));
    assert_eq!(sum, 0, "EDID checksum");
    // The preferred detailed timing carries the scanout resolution.
    let d = &edid[54..72];
    let h_active = u32::from(d[2]) | ((u32::from(d[4]) >> 4) << 8);
    let v_active = u32::from(d[5]) | ((u32::from(d[7]) >> 4) << 8);
    assert_eq!((h_active, v_active), (1920, 1080));

    // A scanout the device does not have.
    let request = Request::new(cmd::GET_EDID).u32(5).u32(0);
    assert_err(
        &h.run_with_capacity(&request, 2048),
        resp::ERR_INVALID_SCANOUT_ID,
    );
}

#[test]
fn many_commands_in_one_notification() {
    let mut h = Harness::new(16, 16);
    // Eight independent create-2d chains published before a single kick.
    let count = 8u16;
    for i in 0..count {
        let request = create_2d(u32::from(i) + 1, FORMAT_B8G8R8X8_UNORM, 4, 4);
        let req_addr = REQ_ADDR + u64::from(i) * 0x100;
        let resp_addr = RESP_ADDR + u64::from(i) * 0x100;
        h.write_mem(req_addr, request.bytes());
        h.write_mem(resp_addr, &[0xff; CTRL_HDR_LEN]);
        let base = i * 2;
        let len = u32::try_from(request.len()).expect("small");
        h.control
            .write_desc(&h.mem, base, req_addr, len, VIRTQ_DESC_F_NEXT, base + 1);
        h.control.write_desc(
            &h.mem,
            base + 1,
            resp_addr,
            CTRL_HDR_LEN as u32,
            VIRTQ_DESC_F_WRITE,
            0,
        );
        h.control.publish(&h.mem, base);
    }
    h.notify(0);

    assert_eq!(h.control.used_idx(&h.mem), count);
    for i in 0..count {
        let resp_addr = RESP_ADDR + u64::from(i) * 0x100;
        let raw = h.read_mem(resp_addr, 4);
        let kind = u32::from_le_bytes(raw.try_into().expect("4 bytes"));
        assert_eq!(kind, resp::OK_NODATA, "command {i}");
    }
}

#[test]
fn the_pipeline_survives_a_device_reset() {
    const SIDE: u32 = 16;
    let mut h = Harness::new(SIDE, SIDE);
    h.fill_backing(FB_ADDR, SIDE, SIDE, RED_BGRA);
    h.setup_scanout(1, SIDE, SIDE, FB_ADDR);
    assert_ok(&h.run(&transfer_to_host(1, rect(0, 0, SIDE, SIDE), 0)));
    assert_ok(&h.run(&resource_flush(1, rect(0, 0, SIDE, SIDE))));

    // Driver-initiated reset: STATUS = 0.
    h.write32(mmio::STATUS, 0);
    assert_eq!(h.transport.status(), 0);
    assert!(!h.transport.is_activated());
    // A notify while down must be ignored, not crash.
    h.notify(0);
    h.notify(1);

    // Bring it back up: the resources are gone, so id 1 is free again and the
    // stale scanout binding cannot be flushed.
    h.control.rewind(&h.mem);
    h.cursor.rewind(&h.mem);
    h.bring_up();
    assert_err(
        &h.run(&resource_flush(1, rect(0, 0, SIDE, SIDE))),
        resp::ERR_INVALID_RESOURCE_ID,
    );

    h.fill_backing(FB_ADDR, SIDE, SIDE, BLUE_BGRA);
    h.setup_scanout(1, SIDE, SIDE, FB_ADDR);
    assert_ok(&h.run(&transfer_to_host(1, rect(0, 0, SIDE, SIDE), 0)));
    assert_ok(&h.run(&resource_flush(1, rect(0, 0, SIDE, SIDE))));
    assert_eq!(h.screenshot().px(SIDE / 2, SIDE / 2), BLUE_RGBA);
}

// ========================================================= malicious guest

#[test]
fn unsupported_formats_are_rejected() {
    let mut h = Harness::new(16, 16);
    // Everything except the two BGRA layouts needs a swizzle we do not do.
    for format in [0u32, 3, 4, 67, 68, 121, 134, u32::MAX] {
        assert_err(
            &h.run(&create_2d(1, format, 16, 16)),
            resp::ERR_INVALID_PARAMETER,
        );
    }
    // …and the two we do support work.
    assert_ok(&h.run(&create_2d(1, FORMAT_B8G8R8A8_UNORM, 16, 16)));
    assert_ok(&h.run(&create_2d(2, FORMAT_B8G8R8X8_UNORM, 16, 16)));
}

#[test]
fn zero_sized_oversized_and_duplicate_resources_are_rejected() {
    let mut h = Harness::new(16, 16);
    for (w, hgt) in [
        (0, 16),
        (16, 0),
        (0, 0),
        (u32::MAX, u32::MAX),
        (u32::MAX, 1),
        (65536, 65536),
    ] {
        assert_err(
            &h.run(&create_2d(1, FORMAT_B8G8R8X8_UNORM, w, hgt)),
            resp::ERR_INVALID_PARAMETER,
        );
    }
    // One pixel past the per-resource cap.
    let side = 4096u32;
    let tall = u32::try_from(MAX_RESOURCE_PIXELS / u64::from(side)).expect("fits");
    assert_err(
        &h.run(&create_2d(1, FORMAT_B8G8R8X8_UNORM, side, tall + 1)),
        resp::ERR_INVALID_PARAMETER,
    );
    // Resource id 0 is never valid.
    assert_err(
        &h.run(&create_2d(0, FORMAT_B8G8R8X8_UNORM, 16, 16)),
        resp::ERR_INVALID_RESOURCE_ID,
    );
    // Duplicates are refused.
    assert_ok(&h.run(&create_2d(5, FORMAT_B8G8R8X8_UNORM, 16, 16)));
    assert_err(
        &h.run(&create_2d(5, FORMAT_B8G8R8X8_UNORM, 16, 16)),
        resp::ERR_INVALID_RESOURCE_ID,
    );
}

#[test]
fn too_many_resources_run_out_of_host_memory_cleanly() {
    let mut h = Harness::new(16, 16);
    // 64 resources fit; the 65th does not, and the device stays healthy.
    for id in 1..=64u32 {
        assert_ok(&h.run(&create_2d(id, FORMAT_B8G8R8X8_UNORM, 2, 2)));
    }
    assert_err(
        &h.run(&create_2d(65, FORMAT_B8G8R8X8_UNORM, 2, 2)),
        resp::ERR_OUT_OF_MEMORY,
    );
    assert_eq!(h.transport.status() & status::DEVICE_NEEDS_RESET, 0);
    // Freeing one makes room again.
    assert_ok(&h.run(&resource_unref(1)));
    assert_ok(&h.run(&create_2d(65, FORMAT_B8G8R8X8_UNORM, 2, 2)));
}

#[test]
fn rects_outside_the_resource_are_rejected_everywhere() {
    const SIDE: u32 = 16;
    let mut h = Harness::new(SIDE, SIDE);
    h.fill_backing(FB_ADDR, SIDE, SIDE, RED_BGRA);
    h.setup_scanout(1, SIDE, SIDE, FB_ADDR);
    assert_ok(&h.run(&transfer_to_host(1, rect(0, 0, SIDE, SIDE), 0)));
    assert_ok(&h.run(&resource_flush(1, rect(0, 0, SIDE, SIDE))));
    let reference = h.screenshot();

    let evil = [
        rect(0, 0, SIDE + 1, SIDE),
        rect(0, 0, SIDE, SIDE + 1),
        rect(1, 0, SIDE, SIDE),
        rect(0, 1, SIDE, SIDE),
        rect(SIDE, 0, 1, 1),
        rect(0, 0, 0, SIDE),
        rect(0, 0, SIDE, 0),
        rect(u32::MAX, u32::MAX, 2, 2),
        rect(u32::MAX - 1, 0, 4, 4),
        rect(0, 0, u32::MAX, u32::MAX),
    ];
    for bad in evil {
        assert_err(
            &h.run(&transfer_to_host(1, bad, 0)),
            resp::ERR_INVALID_PARAMETER,
        );
        assert_err(&h.run(&resource_flush(1, bad)), resp::ERR_INVALID_PARAMETER);
        assert_err(&h.run(&set_scanout(0, 1, bad)), resp::ERR_INVALID_PARAMETER);
    }
    // Not one host pixel moved, and the scanout is still the good one.
    let after = h.screenshot();
    assert_eq!(
        (after.width, after.height),
        (reference.width, reference.height)
    );
    assert_eq!(after.rgba, reference.rgba);
    assert_eq!(h.display.resolution(), (SIDE, SIDE));
}

#[test]
fn transfers_without_backing_or_with_short_backing_are_rejected() {
    const SIDE: u32 = 16;
    let mut h = Harness::new(SIDE, SIDE);
    assert_ok(&h.run(&create_2d(1, FORMAT_B8G8R8X8_UNORM, SIDE, SIDE)));

    // No backing at all.
    assert_err(
        &h.run(&transfer_to_host(1, rect(0, 0, SIDE, SIDE), 0)),
        resp::ERR_INVALID_PARAMETER,
    );

    // Backing one byte too short for the full-frame transfer.
    let full = SIDE * SIDE * 4;
    assert_ok(&h.run(&attach_backing(1, &[(FB_ADDR, full - 1)])));
    assert_err(
        &h.run(&transfer_to_host(1, rect(0, 0, SIDE, SIDE), 0)),
        resp::ERR_INVALID_PARAMETER,
    );
    // A shorter rect still fits in that backing.
    assert_ok(&h.run(&transfer_to_host(1, rect(0, 0, SIDE, SIDE - 1), 0)));

    // Offsets that push the source past the end of the backing store.
    assert_ok(&h.run(&attach_backing(1, &[(FB_ADDR, full)])));
    for offset in [1u64, u64::from(full), u64::MAX, u64::MAX - 16, 1 << 40] {
        assert_err(
            &h.run(&transfer_to_host(1, rect(0, 0, SIDE, SIDE), offset)),
            resp::ERR_INVALID_PARAMETER,
        );
    }
    // Zero entries, and an entry count no chain could carry.
    assert_err(&h.run(&attach_backing(1, &[])), resp::ERR_INVALID_PARAMETER);
    assert_err(
        &h.run(&attach_backing_claiming(
            1,
            MAX_BACKING_ENTRIES + 1,
            &[(FB_ADDR, full)],
        )),
        resp::ERR_INVALID_PARAMETER,
    );
    assert_err(
        &h.run(&attach_backing_claiming(1, u32::MAX, &[(FB_ADDR, full)])),
        resp::ERR_INVALID_PARAMETER,
    );
    // A count the command's own length does not cover.
    assert_err(
        &h.run(&attach_backing_claiming(1, 8, &[(FB_ADDR, full)])),
        resp::ERR_INVALID_PARAMETER,
    );
    // Attaching to an unknown resource, or to id 0.
    assert_err(
        &h.run(&attach_backing(99, &[(FB_ADDR, full)])),
        resp::ERR_INVALID_RESOURCE_ID,
    );
    assert_err(
        &h.run(&attach_backing(0, &[(FB_ADDR, full)])),
        resp::ERR_INVALID_RESOURCE_ID,
    );
}

#[test]
fn backing_pages_outside_guest_ram_cannot_move_host_pixels() {
    // The EPIC 8 acceptance criterion: "VM nie może wymusić kopiowania spoza
    // swojej pamięci".
    const SIDE: u32 = 16;
    let full = SIDE * SIDE * 4;
    let mut h = Harness::new(SIDE, SIDE);
    h.fill_backing(FB_ADDR, SIDE, SIDE, RED_BGRA);
    h.setup_scanout(1, SIDE, SIDE, FB_ADDR);
    assert_ok(&h.run(&transfer_to_host(1, rect(0, 0, SIDE, SIDE), 0)));
    assert_ok(&h.run(&resource_flush(1, rect(0, 0, SIDE, SIDE))));
    let reference = h.screenshot();

    for evil in [
        MEM_SIZE,
        MEM_SIZE + 0x1000,
        MEM_SIZE - 4,
        0xdead_0000_0000,
        u64::MAX - u64::from(full),
        u64::MAX,
    ] {
        assert_ok(&h.run(&attach_backing(1, &[(evil, full)])));
        assert_err(
            &h.run(&transfer_to_host(1, rect(0, 0, SIDE, SIDE), 0)),
            resp::ERR_INVALID_PARAMETER,
        );
        // Partial-width transfers take the row-by-row path; it must be just as
        // safe.
        assert_err(
            &h.run(&transfer_to_host(1, rect(1, 1, 2, 2), 0)),
            resp::ERR_INVALID_PARAMETER,
        );
        assert_ok(&h.run(&resource_flush(1, rect(0, 0, SIDE, SIDE))));
    }
    // A list whose first page is fine and whose second is not: the transfer is
    // refused as a whole, so not even the good half lands.
    assert_ok(&h.run(&attach_backing(
        1,
        &[(FB_ADDR, full / 2), (MEM_SIZE + 0x2000, full / 2)],
    )));
    h.fill_backing(FB_ADDR, SIDE, SIDE / 2, BLUE_BGRA);
    assert_err(
        &h.run(&transfer_to_host(1, rect(0, 0, SIDE, SIDE), 0)),
        resp::ERR_INVALID_PARAMETER,
    );
    assert_ok(&h.run(&resource_flush(1, rect(0, 0, SIDE, SIDE))));

    assert_eq!(h.screenshot().rgba, reference.rgba, "host pixels unchanged");
    assert_eq!(h.transport.status() & status::DEVICE_NEEDS_RESET, 0);
}

#[test]
fn unknown_scanout_ids_are_rejected() {
    let mut h = Harness::new(16, 16);
    assert_ok(&h.run(&create_2d(1, FORMAT_B8G8R8X8_UNORM, 16, 16)));
    for scanout in [NUM_SCANOUTS, NUM_SCANOUTS + 1, 15, 16, u32::MAX] {
        assert_err(
            &h.run(&set_scanout(scanout, 1, rect(0, 0, 16, 16))),
            resp::ERR_INVALID_SCANOUT_ID,
        );
    }
    // …including when the resource id would also be wrong: the scanout check
    // comes first, which is what the spec's ordering implies.
    assert_err(
        &h.run(&set_scanout(7, 99, rect(0, 0, 16, 16))),
        resp::ERR_INVALID_SCANOUT_ID,
    );
}

#[test]
fn commands_for_unknown_resources_are_rejected() {
    let mut h = Harness::new(16, 16);
    for id in [1u32, 2, 99, u32::MAX] {
        assert_err(
            &h.run(&transfer_to_host(id, rect(0, 0, 4, 4), 0)),
            resp::ERR_INVALID_RESOURCE_ID,
        );
        assert_err(
            &h.run(&resource_flush(id, rect(0, 0, 4, 4))),
            resp::ERR_INVALID_RESOURCE_ID,
        );
        assert_err(&h.run(&resource_unref(id)), resp::ERR_INVALID_RESOURCE_ID);
        assert_err(&h.run(&detach_backing(id)), resp::ERR_INVALID_RESOURCE_ID);
    }
    // Resource id 0 in every command that takes one.
    assert_err(
        &h.run(&transfer_to_host(0, rect(0, 0, 4, 4), 0)),
        resp::ERR_INVALID_RESOURCE_ID,
    );
    assert_err(
        &h.run(&resource_flush(0, rect(0, 0, 4, 4))),
        resp::ERR_INVALID_RESOURCE_ID,
    );
    assert_err(&h.run(&resource_unref(0)), resp::ERR_INVALID_RESOURCE_ID);
    assert_err(&h.run(&detach_backing(0)), resp::ERR_INVALID_RESOURCE_ID);
}

#[test]
fn unknown_commands_get_err_unspec() {
    let mut h = Harness::new(16, 16);
    for kind in [
        0u32,
        0x00ff,
        cmd::GET_EDID,
        0x0108,
        0x0200,
        0x0201,
        // Cursor commands do not belong on the control queue.
        cmd::UPDATE_CURSOR,
        cmd::MOVE_CURSOR,
        0xffff_ffff,
    ] {
        let response = h.run(&Request::new(kind).u64(0).u64(0));
        assert_err(&response, resp::ERR_UNSPEC);
    }
    assert_eq!(h.transport.status() & status::DEVICE_NEEDS_RESET, 0);
}

#[test]
fn truncated_commands_are_rejected() {
    let mut h = Harness::new(16, 16);
    // Bodies cut short: the header parses, the command does not.
    let full = create_2d(1, FORMAT_B8G8R8X8_UNORM, 16, 16);
    for len in CTRL_HDR_LEN..full.len() {
        let response = h.run(&Request(full.bytes()[..len].to_vec()));
        assert_err(&response, resp::ERR_INVALID_PARAMETER);
    }
    // Headers cut short: nothing to dispatch on at all.
    for len in 0..CTRL_HDR_LEN {
        let response = h.run(&Request(full.bytes()[..len].to_vec()));
        assert_err(&response, resp::ERR_UNSPEC);
    }
    // A chain with no device-readable part whatsoever.
    h.write_mem(RESP_ADDR, &[0xff; CTRL_HDR_LEN]);
    let control = h.control;
    h.submit(
        &control,
        &[(RESP_ADDR, CTRL_HDR_LEN as u32, VIRTQ_DESC_F_WRITE)],
    );
    h.notify(0);
    let (_, used_len) = h.last_used(&control);
    assert_eq!(used_len, CTRL_HDR_LEN as u32);
    let kind = u32::from_le_bytes(h.read_mem(RESP_ADDR, 4).try_into().expect("4 bytes"));
    assert_eq!(kind, resp::ERR_UNSPEC);

    // The device is still perfectly usable.
    assert_ok(&h.run(&create_2d(1, FORMAT_B8G8R8X8_UNORM, 16, 16)));
}

#[test]
fn oversized_commands_are_rejected_without_staging_them() {
    let mut h = Harness::new(16, 16);
    let control = h.control;
    // Response buffer well clear of the huge readable ranges below.
    let resp_addr = 0x20_0000u64;

    // A chain whose device-readable part claims far more than the largest
    // command the device accepts (~256 KiB): must be refused, not staged.
    h.write_mem(
        REQ_ADDR,
        create_2d(1, FORMAT_B8G8R8X8_UNORM, 16, 16).bytes(),
    );
    h.write_mem(resp_addr, &[0xff; CTRL_HDR_LEN]);
    h.submit(
        &control,
        &[
            (REQ_ADDR, 1 << 20, 0),
            (resp_addr, CTRL_HDR_LEN as u32, VIRTQ_DESC_F_WRITE),
        ],
    );
    h.notify(0);
    let kind = u32::from_le_bytes(h.read_mem(resp_addr, 4).try_into().expect("4 bytes"));
    assert_eq!(kind, resp::ERR_UNSPEC, "unparsable request");
    assert_eq!(h.transport.status() & status::DEVICE_NEEDS_RESET, 0);

    // Several descriptors that together exceed the cap (8 × 64 KiB = 512 KiB),
    // still within the 16-entry ring.
    let mut descs: Vec<(u64, u32, u16)> = Vec::new();
    for i in 0..8u64 {
        descs.push((REQ_ADDR + i * 0x1000, 64 * 1024, 0));
    }
    descs.push((resp_addr, CTRL_HDR_LEN as u32, VIRTQ_DESC_F_WRITE));
    h.write_mem(resp_addr, &[0xff; CTRL_HDR_LEN]);
    h.submit(&control, &descs);
    h.notify(0);
    let kind = u32::from_le_bytes(h.read_mem(resp_addr, 4).try_into().expect("4 bytes"));
    assert_eq!(kind, resp::ERR_UNSPEC);
    assert_ok(&h.run(&create_2d(1, FORMAT_B8G8R8X8_UNORM, 16, 16)));
}

#[test]
fn response_buffers_that_are_too_small_are_handled() {
    let mut h = Harness::new(16, 16);

    // Room for a header but not for the 384-byte display-info body: the device
    // must not write a truncated body, so it answers ERR_UNSPEC instead.
    let response = h.run_with_capacity(&get_display_info(), CTRL_HDR_LEN as u32);
    assert_err(&response, resp::ERR_UNSPEC);
    let response = h.run_with_capacity(&get_display_info(), 100);
    assert_eq!(response.kind(), resp::ERR_UNSPEC);
    assert_eq!(response.used_len, CTRL_HDR_LEN as u32);
    // Exactly enough is enough.
    let response = h.run_with_capacity(
        &get_display_info(),
        (CTRL_HDR_LEN + DISPLAY_INFO_BODY_LEN) as u32,
    );
    assert_eq!(response.kind(), resp::OK_DISPLAY_INFO);

    // Not even room for a header: the chain is unusable, so it comes back with
    // zero length and nothing is written.
    let control = h.control;
    let request = create_2d(1, FORMAT_B8G8R8X8_UNORM, 16, 16);
    h.write_mem(REQ_ADDR, request.bytes());
    h.write_mem(RESP_ADDR, &[0xff; 32]);
    h.submit(
        &control,
        &[
            (REQ_ADDR, u32::try_from(request.len()).expect("small"), 0),
            (RESP_ADDR, 8, VIRTQ_DESC_F_WRITE),
        ],
    );
    h.notify(0);
    let (_, used_len) = h.last_used(&control);
    assert_eq!(used_len, 0);
    assert_eq!(h.read_mem(RESP_ADDR, 8), vec![0xff; 8]);

    // No device-writable buffer at all: same treatment.
    h.submit(
        &control,
        &[(REQ_ADDR, u32::try_from(request.len()).expect("small"), 0)],
    );
    h.notify(0);
    let (_, used_len) = h.last_used(&control);
    assert_eq!(used_len, 0);
    assert_eq!(h.transport.status() & status::DEVICE_NEEDS_RESET, 0);
}

#[test]
fn response_buffers_outside_guest_ram_do_not_crash_the_host() {
    let mut h = Harness::new(16, 16);
    let control = h.control;
    let request = create_2d(1, FORMAT_B8G8R8X8_UNORM, 16, 16);
    h.write_mem(REQ_ADDR, request.bytes());
    h.submit(
        &control,
        &[
            (REQ_ADDR, u32::try_from(request.len()).expect("small"), 0),
            (MEM_SIZE + 0x1000, 64, VIRTQ_DESC_F_WRITE),
        ],
    );
    h.notify(0);
    let (_, used_len) = h.last_used(&control);
    assert_eq!(used_len, 0, "nothing could be written");
    assert_eq!(h.transport.status() & status::DEVICE_NEEDS_RESET, 0);
    // The command itself did take effect — the resource exists — which is why
    // this is reported as a write failure and not retried.
    assert_err(
        &h.run(&create_2d(1, FORMAT_B8G8R8X8_UNORM, 16, 16)),
        resp::ERR_INVALID_RESOURCE_ID,
    );
}

#[test]
fn request_buffers_outside_guest_ram_are_dropped() {
    let mut h = Harness::new(16, 16);
    let control = h.control;
    h.write_mem(RESP_ADDR, &[0xff; CTRL_HDR_LEN]);
    h.submit(
        &control,
        &[
            (0xffff_0000_0000, 40, 0),
            (RESP_ADDR, CTRL_HDR_LEN as u32, VIRTQ_DESC_F_WRITE),
        ],
    );
    h.notify(0);
    let kind = u32::from_le_bytes(h.read_mem(RESP_ADDR, 4).try_into().expect("4 bytes"));
    assert_eq!(kind, resp::ERR_UNSPEC);
    assert_eq!(h.transport.status() & status::DEVICE_NEEDS_RESET, 0);
}

#[test]
fn looped_and_malformed_chains_are_dropped_without_hanging() {
    let mut h = Harness::new(16, 16);
    let control = h.control;
    h.write_mem(REQ_ADDR, get_display_info().bytes());

    // Descriptor 0 chains to itself forever.
    h.control
        .write_desc(&h.mem, 0, REQ_ADDR, 24, VIRTQ_DESC_F_NEXT, 0);
    h.control.publish(&h.mem, 0);
    h.notify(0);
    assert_eq!(h.last_used(&control).1, 0);

    // Two-descriptor loop.
    h.control
        .write_desc(&h.mem, 0, REQ_ADDR, 24, VIRTQ_DESC_F_NEXT, 1);
    h.control
        .write_desc(&h.mem, 1, REQ_ADDR, 24, VIRTQ_DESC_F_NEXT, 0);
    h.control.publish(&h.mem, 0);
    h.notify(0);
    assert_eq!(h.last_used(&control).1, 0);

    // `next` index past the ring.
    h.control
        .write_desc(&h.mem, 0, REQ_ADDR, 24, VIRTQ_DESC_F_NEXT, RING_SIZE + 40);
    h.control.publish(&h.mem, 0);
    h.notify(0);
    assert_eq!(h.last_used(&control).1, 0);

    // Indirect descriptors are a protocol violation (never negotiated).
    h.control
        .write_desc(&h.mem, 0, REQ_ADDR, 64, VIRTQ_DESC_F_INDIRECT, 0);
    h.control.publish(&h.mem, 0);
    h.notify(0);
    assert_eq!(h.last_used(&control).1, 0);

    // Device-writable buffer before the device-readable request.
    h.submit(
        &control,
        &[
            (RESP_ADDR, CTRL_HDR_LEN as u32, VIRTQ_DESC_F_WRITE),
            (REQ_ADDR, 24, 0),
        ],
    );
    h.notify(0);
    assert_eq!(h.last_used(&control).1, 0);

    // An entirely empty chain.
    h.control.write_desc(&h.mem, 0, 0, 0, 0, 0);
    h.control.publish(&h.mem, 0);
    h.notify(0);
    assert_eq!(h.last_used(&control).1, 0);

    assert_eq!(h.transport.status() & status::DEVICE_NEEDS_RESET, 0);
    // Still healthy afterwards.
    assert_eq!(h.run(&get_display_info()).kind(), resp::OK_DISPLAY_INFO);
}

#[test]
fn zero_length_descriptors_are_tolerated() {
    let mut h = Harness::new(16, 16);
    let control = h.control;
    let request = create_2d(1, FORMAT_B8G8R8X8_UNORM, 16, 16);
    h.write_mem(REQ_ADDR, request.bytes());
    h.write_mem(RESP_ADDR, &[0xff; 64]);
    // Zero-length padding descriptors on both sides of the chain.
    h.submit(
        &control,
        &[
            (REQ_ADDR, 0, 0),
            (REQ_ADDR, u32::try_from(request.len()).expect("small"), 0),
            (REQ_ADDR, 0, 0),
            (RESP_ADDR, 0, VIRTQ_DESC_F_WRITE),
            (RESP_ADDR, 64, VIRTQ_DESC_F_WRITE),
        ],
    );
    h.notify(0);
    let (_, used_len) = h.last_used(&control);
    assert_eq!(used_len, CTRL_HDR_LEN as u32);
    let kind = u32::from_le_bytes(h.read_mem(RESP_ADDR, 4).try_into().expect("4 bytes"));
    assert_eq!(kind, resp::OK_NODATA);
}

#[test]
fn a_bare_device_refuses_notifications_and_unknown_queues() {
    let display = DisplayHandle::detached(16, 16).expect("detached display");
    let mut device = GpuDevice::new(display);
    // Not activated yet: any notify is an error, never a panic.
    assert!(device.notify(0).is_err());
    assert!(device.notify(1).is_err());
    assert!(device.notify(2).is_err());
    assert!(device.notify(u16::MAX).is_err());
    // Reset on a device that was never activated is infallible.
    device.reset();
    assert!(device.notify(0).is_err());
    // Config reads on a fresh device are well-formed, not a panic.
    let mut config = [0xffu8; 16];
    device.read_config(0, &mut config);
    assert_eq!(config[8], 1, "num_scanouts");
    device.read_config(u64::MAX, &mut config);
    assert_eq!(config, [0u8; 16], "reads past the config space are zeroes");
}
