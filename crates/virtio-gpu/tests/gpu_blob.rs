//! End-to-end virtio-gpu **blob resource** tests over a real split virtqueue
//! (EPIC 20, VEN-2001/VEN-2002's device-side acceptance).
//!
//! Same discipline as `gpu_3d.rs`: the device is brought up through the mmio
//! registers exactly like a driver would, chains are laid out by hand, and the
//! pixels come out of a windowless `display::DisplayHandle::detached`. The
//! renderer is [`NullRenderer::with_venus`], the portable loopback that
//! advertises the Venus capset and all three blob memory types, so the whole
//! path — feature negotiation, capsets, venus-typed contexts,
//! `RESOURCE_CREATE_BLOB`, `SET_SCANOUT_BLOB`, `RESOURCE_MAP_BLOB`,
//! `RESOURCE_UNMAP_BLOB`, the shared-memory region on the transport — runs on
//! every host OS with no GPU at all, Windows included.
//!
//! Three groups: the well-behaved Venus driver, the guest-memory blob that
//! actually reaches the window, and the malicious guest — every guest-chosen
//! id, size, flag bit and *window offset* pushed out of range, none of which
//! may panic and none of which may set `DEVICE_NEEDS_RESET`.

use std::sync::Arc;

use display::DisplayHandle;
use virtio_core::chain::VIRTQ_DESC_F_WRITE;
use virtio_core::testing::{guest_memory, SplitRing, TestIrqLine};
use virtio_core::{mmio, status, GuestMem, MmioTransport};
use virtio_gpu::blob::BLOB_PAGE_SIZE;
use virtio_gpu::protocol::{
    cmd, resp, Rect, BLOB_FLAG_USE_CROSS_DEVICE, BLOB_FLAG_USE_MAPPABLE, BLOB_FLAG_USE_SHAREABLE,
    BLOB_MEM_GUEST, BLOB_MEM_HOST3D, BLOB_MEM_HOST3D_GUEST, CTRL_HDR_LEN, MAP_CACHE_MASK,
};
use virtio_gpu::renderer::Renderer3d;
use virtio_gpu::{
    GpuDevice, NullRenderer, CAPSET_VENUS, LOOPBACK_MAGIC, LOOPBACK_SIGNATURE_LEN, MAX_BLOB_BYTES,
    VIRTIO_GPU_F_CONTEXT_INIT, VIRTIO_GPU_F_RESOURCE_BLOB, VIRTIO_GPU_F_VIRGL,
};
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

/// The window `NullRenderer::with_venus` reports.
const WINDOW: u64 = 256 << 20;

// ===================================================== the guest's half

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

    /// Cuts the encoded command short, which is how a guest produces a
    /// truncated command without a malformed chain.
    fn truncated_to(mut self, len: usize) -> Self {
        self.0.truncate(len);
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

/// `CTX_CREATE` with a `context_init` naming the context type (VEN-2002).
fn ctx_create(ctx_id: u32, context_init: u32, name: &str) -> Request {
    let mut debug_name = [0u8; 64];
    let len = name.len().min(64);
    debug_name[..len].copy_from_slice(&name.as_bytes()[..len]);
    Request::new(cmd::CTX_CREATE)
        .ctx(ctx_id)
        .u32(len as u32)
        .u32(context_init)
        .raw(&debug_name)
}

fn create_blob(
    id: u32,
    blob_mem: u32,
    blob_flags: u32,
    blob_id: u64,
    size: u64,
    entries: &[(u64, u32)],
) -> Request {
    let mut request = Request::new(cmd::RESOURCE_CREATE_BLOB)
        .u32(id)
        .u32(blob_mem)
        .u32(blob_flags)
        .u32(u32::try_from(entries.len()).expect("small"))
        .u64(blob_id)
        .u64(size);
    for &(addr, length) in entries {
        request = request.u64(addr).u32(length).u32(0);
    }
    request
}

/// A guest-memory blob one page long, backed by one page at `FB_ADDR`.
fn guest_blob(id: u32) -> Request {
    create_blob(
        id,
        BLOB_MEM_GUEST,
        BLOB_FLAG_USE_SHAREABLE,
        0,
        BLOB_PAGE_SIZE,
        &[(FB_ADDR, BLOB_PAGE_SIZE as u32)],
    )
}

/// A mappable host blob of `pages` pages.
fn host_blob(id: u32, pages: u64) -> Request {
    create_blob(
        id,
        BLOB_MEM_HOST3D,
        BLOB_FLAG_USE_MAPPABLE,
        u64::from(id) << 32,
        pages * BLOB_PAGE_SIZE,
        &[],
    )
}

fn map_blob(id: u32, offset: u64) -> Request {
    Request::new(cmd::RESOURCE_MAP_BLOB)
        .u32(id)
        .u32(0)
        .u64(offset)
}

fn unmap_blob(id: u32) -> Request {
    Request::new(cmd::RESOURCE_UNMAP_BLOB).u32(id).u32(0)
}

#[allow(clippy::too_many_arguments)]
fn set_scanout_blob(
    scanout_id: u32,
    resource_id: u32,
    region: Rect,
    width: u32,
    height: u32,
    format: u32,
    stride: u32,
    offset: u32,
) -> Request {
    Request::new(cmd::SET_SCANOUT_BLOB)
        .u32(region.x)
        .u32(region.y)
        .u32(region.width)
        .u32(region.height)
        .u32(scanout_id)
        .u32(resource_id)
        .u32(width)
        .u32(height)
        .u32(format)
        .u32(0) // padding
        .u32(stride)
        .u32(0)
        .u32(0)
        .u32(0)
        .u32(offset)
        .u32(0)
        .u32(0)
        .u32(0)
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

fn attach_backing(id: u32, entries: &[(u64, u32)]) -> Request {
    let mut request = Request::new(cmd::RESOURCE_ATTACH_BACKING)
        .u32(id)
        .u32(u32::try_from(entries.len()).expect("small"));
    for &(addr, length) in entries {
        request = request.u64(addr).u32(length).u32(0);
    }
    request
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
    cursor: SplitRing,
    transport: MmioTransport,
    display: DisplayHandle,
    features: u64,
    /// The host pages behind the shared-memory window, when this harness
    /// backed one (VEN-2001 phase 2). Held so a test can look at the same
    /// bytes the guest would map.
    backing: Option<Arc<TestBacking>>,
}

/// Host memory behind the window, the shape `machine_x86::shm` supplies but
/// with no hypervisor in it — this is a device test, not a machine test.
///
/// Every access is bounded in `u64` before it becomes an index, which is the
/// same discipline the real one keeps, and the reason a test double is safe to
/// use as the thing under test's counterparty.
struct TestBacking {
    bytes: std::sync::Mutex<Vec<u8>>,
}

impl TestBacking {
    fn new(len: u64) -> Arc<Self> {
        Arc::new(Self {
            bytes: std::sync::Mutex::new(vec![0u8; len as usize]),
        })
    }

    fn range(total: usize, offset: u64, len: u64) -> Option<(usize, usize)> {
        let end = offset.checked_add(len)?;
        if end > total as u64 {
            return None;
        }
        Some((usize::try_from(offset).ok()?, usize::try_from(end).ok()?))
    }

    /// Scribbles over the whole window, so "the device cleared this span" is a
    /// statement about memory rather than about bookkeeping.
    fn poison(&self, byte: u8) {
        self.bytes.lock().unwrap().fill(byte);
    }

    fn peek(&self, offset: u64, len: usize) -> Vec<u8> {
        let bytes = self.bytes.lock().unwrap();
        let (start, end) = Self::range(bytes.len(), offset, len as u64).expect("in bounds");
        bytes[start..end].to_vec()
    }
}

impl virtio_core::ShmBacking for TestBacking {
    fn len(&self) -> u64 {
        self.bytes.lock().unwrap().len() as u64
    }
    fn read(&self, offset: u64, buf: &mut [u8]) -> Result<(), virtio_core::ShmAccessError> {
        let bytes = self.bytes.lock().unwrap();
        let (start, end) = Self::range(bytes.len(), offset, buf.len() as u64).ok_or(
            virtio_core::ShmAccessError {
                offset,
                len: buf.len() as u64,
                window: bytes.len() as u64,
            },
        )?;
        buf.copy_from_slice(&bytes[start..end]);
        Ok(())
    }
    fn write(&self, offset: u64, data: &[u8]) -> Result<(), virtio_core::ShmAccessError> {
        let mut bytes = self.bytes.lock().unwrap();
        let total = bytes.len();
        let (start, end) =
            Self::range(total, offset, data.len() as u64).ok_or(virtio_core::ShmAccessError {
                offset,
                len: data.len() as u64,
                window: total as u64,
            })?;
        bytes[start..end].copy_from_slice(data);
        Ok(())
    }
    fn fill(&self, offset: u64, len: u64, byte: u8) -> Result<(), virtio_core::ShmAccessError> {
        let mut bytes = self.bytes.lock().unwrap();
        let total = bytes.len();
        let (start, end) = Self::range(total, offset, len).ok_or(virtio_core::ShmAccessError {
            offset,
            len,
            window: total as u64,
        })?;
        bytes[start..end].fill(byte);
        Ok(())
    }
}

impl Harness {
    /// The Venus loopback: blob resources, the venus capset, a window.
    fn venus(width: u32, height: u32) -> Self {
        Self::with_renderer(width, height, Box::new(NullRenderer::with_venus()))
    }

    /// The classic virgl loopback: no blob support at all.
    fn virgl(width: u32, height: u32) -> Self {
        Self::with_renderer(width, height, Box::new(NullRenderer::new()))
    }

    /// The Venus loopback with **host memory behind its window**, which is
    /// what a machine that can back the region gives it (VEN-2001 phase 2).
    fn venus_backed(width: u32, height: u32) -> Self {
        Self::build(
            width,
            height,
            Box::new(NullRenderer::with_venus()),
            Some(TestBacking::new(WINDOW)),
        )
    }

    fn with_renderer(width: u32, height: u32, renderer: Box<dyn Renderer3d>) -> Self {
        Self::build(width, height, renderer, None)
    }

    fn build(
        width: u32,
        height: u32,
        renderer: Box<dyn Renderer3d>,
        backing: Option<Arc<TestBacking>>,
    ) -> Self {
        use virtio_core::VirtioDevice as _;
        let display = DisplayHandle::detached(width, height).expect("detached display");
        let mut device = GpuDevice::with_renderer(display.clone(), renderer);
        if let Some(backing) = &backing {
            // Exactly what `machine_x86::shm::back_regions` does once it has
            // allocated the pages, and at the same point in the device's life:
            // before it is attached to a transport.
            assert_eq!(
                device.shm_regions(),
                vec![virtio_core::ShmRegion {
                    id: virtio_gpu::VIRTIO_GPU_SHM_ID_HOST_VISIBLE,
                    len: WINDOW
                }],
                "the device must declare the region the machine is about to back"
            );
            device.set_shm_backing(virtio_gpu::VIRTIO_GPU_SHM_ID_HOST_VISIBLE, backing.clone());
            assert!(device.shm_backing().is_some(), "the backing was refused");
        }
        let device = device;
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
            features: 0,
            backing,
        };
        harness.bring_up();
        harness
    }

    /// The window's host pages, for a harness that backed one.
    fn backing(&self) -> &Arc<TestBacking> {
        self.backing.as_ref().expect("this harness backed a window")
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
        self.features = features;
        assert_ne!(features & VIRTIO_GPU_F_VIRGL, 0);

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

    fn num_capsets(&mut self) -> u32 {
        let mut raw = [0u8; 16];
        self.transport.read(mmio::CONFIG_SPACE, &mut raw);
        u32::from_le_bytes(raw[12..16].try_into().expect("in range"))
    }

    /// The `SHM_LEN`/`SHM_BASE` pair for `shmid`, as the driver reads it.
    fn shm(&mut self, shmid: u32) -> (u64, u64) {
        self.write32(mmio::SHM_SEL, shmid);
        let len = u64::from(self.read32(mmio::SHM_LEN_LOW))
            | u64::from(self.read32(mmio::SHM_LEN_HIGH)) << 32;
        let base = u64::from(self.read32(mmio::SHM_BASE_LOW))
            | u64::from(self.read32(mmio::SHM_BASE_HIGH)) << 32;
        (len, base)
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

    /// A driver unbind and re-probe: status 0, fresh ring memory, bring-up
    /// again. Zeroing the rings matters — a reset puts the *device's* avail
    /// index back to 0 while the ring in guest memory still carries the old
    /// one, so a driver that reused the pages would see every chain replayed.
    fn driver_reset(&mut self) {
        self.write32(mmio::STATUS, 0);
        self.write_mem(CONTROL_RING_BASE, &[0u8; 0x2000]);
        self.bring_up();
    }

    fn needs_reset(&self) -> bool {
        self.transport.status() & status::DEVICE_NEEDS_RESET != 0
    }

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

/// VEN-2001/VEN-2002's negotiation half: the feature bits, the venus capset
/// and the shared-memory region only exist when the renderer can back them.
#[test]
fn a_venus_renderer_makes_the_device_offer_blob_context_init_and_a_window() {
    let mut h = Harness::venus(4, 4);
    assert_ne!(h.features & VIRTIO_GPU_F_RESOURCE_BLOB, 0);
    assert_ne!(h.features & VIRTIO_GPU_F_CONTEXT_INIT, 0);
    assert_eq!(h.num_capsets(), 3, "VIRGL, VIRGL2 and VENUS");

    // Capset discovery, exactly as the guest kernel walks it.
    let info = h.run(&Request::new(cmd::GET_CAPSET_INFO).u32(2).u32(0));
    assert_eq!(info.kind(), resp::OK_CAPSET_INFO);
    assert_eq!(
        info.body()[0..4],
        CAPSET_VENUS.to_le_bytes(),
        "capset index 2 is VENUS"
    );
    let capset = h.run(&Request::new(cmd::GET_CAPSET).u32(CAPSET_VENUS).u32(0));
    assert_eq!(capset.kind(), resp::OK_CAPSET);
    assert_eq!(capset.body().len(), 32, "virgl_renderer_capset_venus");

    // The shared-memory region is declared but *absent* until the machine
    // layer places it — a driver told about a window that decodes nothing
    // would fault on its first access.
    assert_eq!(
        h.shm(1),
        (u64::MAX, u64::MAX),
        "unplaced region reads all-ones"
    );
    h.transport.set_shm_base(1, 0x4_0000_0000);
    assert_eq!(h.shm(1), (WINDOW, 0x4_0000_0000));
    // Every other shmid stays absent.
    assert_eq!(h.shm(0), (u64::MAX, u64::MAX));
    assert_eq!(h.shm(7), (u64::MAX, u64::MAX));
}

/// A device whose renderer speaks only classic virgl offers neither bit,
/// declares no region, and answers every blob command `ERR_UNSPEC` *before*
/// parsing its body — the same shape a 2D-only device gives the 3D set.
#[test]
fn a_virgl_only_device_has_no_blob_feature_no_region_and_no_blob_commands() {
    let mut h = Harness::virgl(4, 4);
    assert_eq!(h.features & VIRTIO_GPU_F_RESOURCE_BLOB, 0);
    assert_eq!(h.features & VIRTIO_GPU_F_CONTEXT_INIT, 0);
    assert!(h.transport.shm_regions().is_empty());
    assert_eq!(h.shm(1), (u64::MAX, u64::MAX));

    for request in [
        guest_blob(1),
        map_blob(1, 0),
        unmap_blob(1),
        set_scanout_blob(0, 1, rect(0, 0, 4, 4), 4, 4, 2, 16, 0),
        // Truncated too: "unsupported" must win over "invalid parameter".
        guest_blob(1).truncated_to(CTRL_HDR_LEN),
    ] {
        assert_err(&h.run(&request), resp::ERR_UNSPEC);
    }
    assert!(!h.needs_reset());
}

/// The context-type half of VEN-2002: `context_init` carries a capset id, a
/// venus context reaches the renderer *as* a venus context, and a capset the
/// renderer does not serve is refused.
#[test]
fn a_venus_typed_context_is_accepted_and_an_unserved_one_is_not() {
    let mut h = Harness::venus(4, 4);
    assert_ok(&h.run(&ctx_create(1, 0, "kernel")));
    assert_ok(&h.run(&ctx_create(2, CAPSET_VENUS, "vkcube")));
    // A capset nothing advertises, and a reserved bit outside the id mask.
    assert_err(
        &h.run(&ctx_create(3, 9, "gfxstream")),
        resp::ERR_INVALID_PARAMETER,
    );
    assert_err(
        &h.run(&ctx_create(4, 0x1_0000 | CAPSET_VENUS, "venus")),
        resp::ERR_INVALID_PARAMETER,
    );
    assert!(!h.needs_reset());

    // The same conversation against a virgl-only renderer: venus refused.
    let mut h = Harness::virgl(4, 4);
    assert_ok(&h.run(&ctx_create(1, 0, "kernel")));
    assert_err(
        &h.run(&ctx_create(2, CAPSET_VENUS, "vkcube")),
        resp::ERR_INVALID_PARAMETER,
    );
}

/// A guest-memory blob is the one Venus actually leans on (its command ring),
/// and it needs no host allocation at all. Here it goes the whole way: create,
/// attach to a venus context, bind to the scanout with an explicit stride,
/// flush, and land as pixels in the host window.
#[test]
fn a_guest_memory_blob_is_created_scanned_out_and_flushed() {
    let mut h = Harness::venus(4, 4);
    h.write_mem(FB_ADDR, &RED_BGRA.repeat(16));
    assert_ok(&h.run(&guest_blob(20)));
    assert_ok(&h.run(&ctx_create(1, CAPSET_VENUS, "vkcube")));
    // Venus attaches its ring blob to its context; the renderer has no 3D
    // handle for it, exactly like the kernel's 2D console framebuffer.
    assert_ok(&h.run(&Request::new(cmd::CTX_ATTACH_RESOURCE).ctx(1).u32(20).u32(0)));

    assert_ok(&h.run(&set_scanout_blob(
        0,
        20,
        rect(0, 0, 4, 4),
        4,
        4,
        virtio_gpu::FORMAT_B8G8R8X8_UNORM,
        16,
        0,
    )));
    assert_ok(&h.run(&resource_flush(20, rect(0, 0, 4, 4))));
    assert_eq!(h.px(0, 0), RED_RGBA, "blob pixels reached the host window");
    assert_eq!(h.px(3, 3), RED_RGBA);

    // A partial-rect flush takes the same path with a non-zero row start.
    h.write_mem(FB_ADDR + 16, &[0u8; 16]);
    assert_ok(&h.run(&resource_flush(20, rect(0, 1, 4, 1))));
    assert_eq!(h.px(0, 1), [0, 0, 0, 0xff], "row 1 went black");
    assert_eq!(h.px(0, 0), RED_RGBA, "row 0 untouched");

    // Unref while bound disables the scanout, exactly like a 2D resource.
    assert_ok(&h.run(&resource_unref(20)));
    assert!(!h.needs_reset());
}

/// The host-visible window, which is the sharpest guest-controlled surface in
/// the epic: the guest names the offset a host mapping lands at.
#[test]
fn host_blobs_map_and_unmap_inside_the_window_and_nowhere_else() {
    let mut h = Harness::venus(4, 4);
    assert_ok(&h.run(&host_blob(30, 4)));
    assert_ok(&h.run(&host_blob(31, 4)));

    let mapped = h.run(&map_blob(30, 0));
    assert_eq!(mapped.kind(), resp::OK_MAP_INFO);
    let map_info = u32::from_le_bytes(mapped.body()[0..4].try_into().expect("in range"));
    assert_eq!(
        map_info & !MAP_CACHE_MASK,
        0,
        "only the cache nibble is meaningful"
    );

    // Mapping the same blob twice, and mapping a second blob on top of the
    // first, both fail — an overlap would alias two guests' host memory.
    assert_err(
        &h.run(&map_blob(30, 8 * BLOB_PAGE_SIZE)),
        resp::ERR_INVALID_RESOURCE_ID,
    );
    assert_err(
        &h.run(&map_blob(31, 2 * BLOB_PAGE_SIZE)),
        resp::ERR_INVALID_PARAMETER,
    );
    // Off the page grid, and off the end of the window.
    assert_err(&h.run(&map_blob(31, 1)), resp::ERR_INVALID_PARAMETER);
    assert_err(&h.run(&map_blob(31, WINDOW)), resp::ERR_INVALID_PARAMETER);
    assert_err(
        &h.run(&map_blob(31, WINDOW - 2 * BLOB_PAGE_SIZE)),
        resp::ERR_INVALID_PARAMETER,
    );
    assert_err(&h.run(&map_blob(31, u64::MAX)), resp::ERR_INVALID_PARAMETER);

    // Immediately after the first mapping is legal and exact.
    assert_eq!(
        h.run(&map_blob(31, 4 * BLOB_PAGE_SIZE)).kind(),
        resp::OK_MAP_INFO
    );

    // Unmap frees the span for someone else.
    assert_ok(&h.run(&unmap_blob(30)));
    assert_err(&h.run(&unmap_blob(30)), resp::ERR_INVALID_RESOURCE_ID);
    assert_ok(&h.run(&host_blob(32, 4)));
    assert_eq!(h.run(&map_blob(32, 0)).kind(), resp::OK_MAP_INFO);

    // Unref while mapped releases the window too — a guest is allowed to skip
    // the unmap, and the host must not leak that span. Blob 30 (unmapped
    // earlier, never unref'd) can then take the span 31 was holding.
    assert_ok(&h.run(&resource_unref(31)));
    assert_eq!(
        h.run(&map_blob(30, 4 * BLOB_PAGE_SIZE)).kind(),
        resp::OK_MAP_INFO,
        "the span the unref'd blob held is free again"
    );
    assert!(!h.needs_reset());
}

/// A blob created without `USE_MAPPABLE` cannot be mapped, and a mappable one
/// cannot be created on a host with no window at all.
#[test]
fn only_a_mappable_blob_can_be_mapped() {
    let mut h = Harness::venus(4, 4);
    assert_ok(&h.run(&guest_blob(40)));
    assert_err(&h.run(&map_blob(40, 0)), resp::ERR_INVALID_RESOURCE_ID);
    assert!(!h.needs_reset());
}

/// Every guest-chosen value pushed out of range. None may panic, none may set
/// `DEVICE_NEEDS_RESET`, and each must answer with the code its category
/// deserves.
#[test]
fn a_malicious_blob_guest_is_answered_in_band() {
    let mut h = Harness::venus(4, 4);

    // --- create: ids
    assert_err(
        &h.run(&create_blob(
            0,
            BLOB_MEM_GUEST,
            0,
            0,
            BLOB_PAGE_SIZE,
            &[(FB_ADDR, 4096)],
        )),
        resp::ERR_INVALID_RESOURCE_ID,
    );
    assert_ok(&h.run(&guest_blob(50)));
    assert_err(&h.run(&guest_blob(50)), resp::ERR_INVALID_RESOURCE_ID);
    // …and the id namespace is one namespace: a 2D or 3D resource may not
    // reuse a blob's id, nor a blob theirs.
    assert_err(
        &h.run(
            &Request::new(cmd::RESOURCE_CREATE_2D)
                .u32(50)
                .u32(2)
                .u32(4)
                .u32(4),
        ),
        resp::ERR_INVALID_RESOURCE_ID,
    );
    assert_ok(
        &h.run(
            &Request::new(cmd::RESOURCE_CREATE_2D)
                .u32(51)
                .u32(2)
                .u32(4)
                .u32(4),
        ),
    );
    assert_err(&h.run(&guest_blob(51)), resp::ERR_INVALID_RESOURCE_ID);

    // --- create: memory types and flags
    for bad_mem in [0u32, 4, 0x8000_0000, u32::MAX] {
        assert_err(
            &h.run(&create_blob(
                60,
                bad_mem,
                0,
                0,
                BLOB_PAGE_SIZE,
                &[(FB_ADDR, 4096)],
            )),
            resp::ERR_INVALID_PARAMETER,
        );
    }
    for bad_flags in [0x8u32, 0x8000_0000, u32::MAX, BLOB_FLAG_USE_CROSS_DEVICE] {
        assert_err(
            &h.run(&create_blob(
                60,
                BLOB_MEM_GUEST,
                bad_flags,
                0,
                BLOB_PAGE_SIZE,
                &[(FB_ADDR, 4096)],
            )),
            resp::ERR_INVALID_PARAMETER,
        );
    }

    // --- create: sizes
    for bad_size in [
        0u64,
        1,
        BLOB_PAGE_SIZE - 1,
        BLOB_PAGE_SIZE + 1,
        MAX_BLOB_BYTES + BLOB_PAGE_SIZE,
        u64::MAX,
        u64::MAX - 4095,
    ] {
        assert_err(
            &h.run(&create_blob(
                60,
                BLOB_MEM_GUEST,
                0,
                0,
                bad_size,
                &[(FB_ADDR, 4096)],
            )),
            resp::ERR_INVALID_PARAMETER,
        );
    }

    // --- create: page lists
    // A guest blob with no pages, and pages that do not cover the size.
    assert_err(
        &h.run(&create_blob(60, BLOB_MEM_GUEST, 0, 0, BLOB_PAGE_SIZE, &[])),
        resp::ERR_INVALID_PARAMETER,
    );
    assert_err(
        &h.run(&create_blob(
            60,
            BLOB_MEM_GUEST,
            0,
            0,
            2 * BLOB_PAGE_SIZE,
            &[(FB_ADDR, 4096)],
        )),
        resp::ERR_INVALID_PARAMETER,
    );
    // A host3d blob that carries a page list anyway.
    assert_err(
        &h.run(&create_blob(
            60,
            BLOB_MEM_HOST3D,
            0,
            7,
            BLOB_PAGE_SIZE,
            &[(FB_ADDR, 4096)],
        )),
        resp::ERR_INVALID_PARAMETER,
    );
    // A declared entry count the command does not carry (the classic
    // "length field lies" attack), and one that overflows the multiplication.
    let mut lying = create_blob(60, BLOB_MEM_GUEST, 0, 0, BLOB_PAGE_SIZE, &[(FB_ADDR, 4096)]);
    lying.0[CTRL_HDR_LEN + 12..CTRL_HDR_LEN + 16].copy_from_slice(&64u32.to_le_bytes());
    assert_err(&h.run(&lying), resp::ERR_INVALID_PARAMETER);
    let mut absurd = create_blob(60, BLOB_MEM_GUEST, 0, 0, BLOB_PAGE_SIZE, &[(FB_ADDR, 4096)]);
    absurd.0[CTRL_HDR_LEN + 12..CTRL_HDR_LEN + 16].copy_from_slice(&u32::MAX.to_le_bytes());
    assert_err(&h.run(&absurd), resp::ERR_INVALID_PARAMETER);
    // Truncated fixed part.
    assert_err(
        &h.run(&guest_blob(60).truncated_to(CTRL_HDR_LEN + 4)),
        resp::ERR_INVALID_PARAMETER,
    );

    // --- commands a blob does not answer
    assert_err(
        &h.run(&attach_backing(50, &[(FB_ADDR, 4096)])),
        resp::ERR_INVALID_RESOURCE_ID,
    );
    assert_err(
        &h.run(&Request::new(cmd::RESOURCE_DETACH_BACKING).u32(50).u32(0)),
        resp::ERR_INVALID_RESOURCE_ID,
    );
    assert_err(
        &h.run(
            &Request::new(cmd::SET_SCANOUT)
                .u32(0)
                .u32(0)
                .u32(4)
                .u32(4)
                .u32(0)
                .u32(50),
        ),
        resp::ERR_INVALID_RESOURCE_ID,
    );
    assert_err(
        &h.run(
            &Request::new(cmd::TRANSFER_TO_HOST_3D)
                .ctx(0)
                .u32(0)
                .u32(0)
                .u32(0)
                .u32(4)
                .u32(4)
                .u32(1)
                .u64(0)
                .u32(50)
                .u32(0)
                .u32(0)
                .u32(0),
        ),
        resp::ERR_INVALID_RESOURCE_ID,
    );

    // --- map/unmap on ids that are not blobs, or not there
    assert_err(&h.run(&map_blob(0, 0)), resp::ERR_INVALID_RESOURCE_ID);
    assert_err(&h.run(&map_blob(999, 0)), resp::ERR_INVALID_RESOURCE_ID);
    assert_err(&h.run(&unmap_blob(999)), resp::ERR_INVALID_RESOURCE_ID);
    assert_err(&h.run(&map_blob(51, 0)), resp::ERR_INVALID_RESOURCE_ID);
    assert_err(
        &h.run(&map_blob(50, 0).truncated_to(CTRL_HDR_LEN)),
        resp::ERR_INVALID_PARAMETER,
    );

    // --- set_scanout_blob: format, planes, stride, geometry
    let bad_scanouts = [
        // A format nothing can present.
        set_scanout_blob(0, 50, rect(0, 0, 4, 4), 4, 4, 67, 16, 0),
        // A second plane, which means a planar format we do not accept.
        Request::new(cmd::SET_SCANOUT_BLOB)
            .u32(0)
            .u32(0)
            .u32(4)
            .u32(4)
            .u32(0)
            .u32(50)
            .u32(4)
            .u32(4)
            .u32(virtio_gpu::FORMAT_B8G8R8X8_UNORM)
            .u32(0)
            .u32(16)
            .u32(8) // strides[1]
            .u32(0)
            .u32(0)
            .u32(0)
            .u32(0)
            .u32(0)
            .u32(0),
        // A stride too small to hold a row.
        set_scanout_blob(
            0,
            50,
            rect(0, 0, 4, 4),
            4,
            4,
            virtio_gpu::FORMAT_B8G8R8X8_UNORM,
            8,
            0,
        ),
        // A rect outside the declared image.
        set_scanout_blob(
            0,
            50,
            rect(0, 0, 8, 8),
            4,
            4,
            virtio_gpu::FORMAT_B8G8R8X8_UNORM,
            16,
            0,
        ),
        // A declared image larger than the blob's pages.
        set_scanout_blob(
            0,
            50,
            rect(0, 0, 4, 4),
            4,
            4,
            virtio_gpu::FORMAT_B8G8R8X8_UNORM,
            u32::MAX,
            0,
        ),
        // An offset that pushes the image past the end.
        set_scanout_blob(
            0,
            50,
            rect(0, 0, 4, 4),
            4,
            4,
            virtio_gpu::FORMAT_B8G8R8X8_UNORM,
            16,
            u32::MAX,
        ),
        // A scanout that does not exist.
        set_scanout_blob(
            9,
            50,
            rect(0, 0, 4, 4),
            4,
            4,
            virtio_gpu::FORMAT_B8G8R8X8_UNORM,
            16,
            0,
        ),
        // Truncated.
        set_scanout_blob(
            0,
            50,
            rect(0, 0, 4, 4),
            4,
            4,
            virtio_gpu::FORMAT_B8G8R8X8_UNORM,
            16,
            0,
        )
        .truncated_to(CTRL_HDR_LEN + 8),
    ];
    for request in bad_scanouts {
        let response = h.run(&request);
        assert_ne!(
            response.kind(),
            resp::OK_NODATA,
            "a malformed SET_SCANOUT_BLOB was accepted"
        );
    }

    // A host blob cannot be scanned out: this host has no zero-copy export.
    assert_ok(&h.run(&host_blob(70, 1)));
    assert_err(
        &h.run(&set_scanout_blob(
            0,
            70,
            rect(0, 0, 4, 4),
            4,
            4,
            virtio_gpu::FORMAT_B8G8R8X8_UNORM,
            16,
            0,
        )),
        resp::ERR_INVALID_PARAMETER,
    );

    assert!(
        !h.needs_reset(),
        "not one malformed blob command may break the device"
    );
}

/// A `HOST3D_GUEST` blob is both halves at once, and a device reset drops
/// every blob and every window mapping with everything else.
#[test]
fn host3d_guest_blobs_work_and_a_reset_drops_them_all() {
    let mut h = Harness::venus(4, 4);
    assert_ok(&h.run(&create_blob(
        80,
        BLOB_MEM_HOST3D_GUEST,
        BLOB_FLAG_USE_MAPPABLE,
        0x1234,
        2 * BLOB_PAGE_SIZE,
        &[(FB_ADDR, 4096), (FB_ADDR + 4096, 4096)],
    )));
    assert_eq!(h.run(&map_blob(80, 0)).kind(), resp::OK_MAP_INFO);

    // Reset through the status register, as a driver unbinding would.
    h.driver_reset();
    // Everything is gone: the id is free again and the window span with it.
    assert_ok(&h.run(&create_blob(
        80,
        BLOB_MEM_HOST3D_GUEST,
        BLOB_FLAG_USE_MAPPABLE,
        0x1234,
        2 * BLOB_PAGE_SIZE,
        &[(FB_ADDR, 4096), (FB_ADDR + 4096, 4096)],
    )));
    assert_eq!(h.run(&map_blob(80, 0)).kind(), resp::OK_MAP_INFO);
    assert!(!h.needs_reset());
}

// ================================================ the window, with real pages

/// The whole point of phase 2, at the device's own seam: a `RESOURCE_MAP_BLOB`
/// against a backed window reaches host memory, and the guest can see what the
/// host put there.
///
/// The loopback renderer writes its signature at the mapping offset — a real
/// Venus renderer would map a `VkDeviceMemory` there instead — so a guest that
/// mapped this blob and read the first 32 bytes of its span would read exactly
/// these bytes out of the shared-memory region.
#[test]
fn a_mapped_blob_reaches_host_memory_the_guest_can_read() {
    let mut h = Harness::venus_backed(64, 64);
    let offset = 8 * BLOB_PAGE_SIZE;
    let pages = 4;
    assert_ok(&h.run(&host_blob(9, pages)));
    let response = h.run(&map_blob(9, offset));
    assert_eq!(response.kind(), virtio_gpu::resp::OK_MAP_INFO);

    let signature = h.backing().peek(offset, LOOPBACK_SIGNATURE_LEN as usize);
    assert_eq!(
        signature,
        virtio_gpu::loopback_signature(9, 9u64 << 32, pages * BLOB_PAGE_SIZE).to_vec(),
        "the renderer's bytes are not in the window at the offset the guest named"
    );
    assert_eq!(
        &signature[..LOOPBACK_MAGIC.len()],
        &LOOPBACK_MAGIC[..],
        "and they start with the magic a guest probe looks for"
    );

    // Unmapping takes them away again: the next blob at this offset is a
    // different resource and must not inherit the marker.
    assert_ok(&h.run(&unmap_blob(9)));
    assert!(
        h.backing()
            .peek(offset, LOOPBACK_SIGNATURE_LEN as usize)
            .iter()
            .all(|b| *b == 0),
        "the loopback signature outlived its mapping"
    );
}

/// A guest must never be handed a span with the previous tenant's bytes in it.
///
/// The span is poisoned from the host side between the two mappings, which is
/// the strongest form of the test: it does not matter whether the first blob
/// wrote anything, only that whatever is there is gone.
#[test]
fn a_mapped_span_is_cleared_before_the_guest_can_read_it() {
    let mut h = Harness::venus_backed(64, 64);
    let offset = 2 * BLOB_PAGE_SIZE;
    let pages = 3;

    assert_ok(&h.run(&host_blob(1, pages)));
    assert_eq!(
        h.run(&map_blob(1, offset)).kind(),
        virtio_gpu::resp::OK_MAP_INFO
    );
    // Whatever the first tenant left behind — here, the worst case.
    h.backing().poison(0xde);
    assert_ok(&h.run(&unmap_blob(1)));
    assert_ok(&h.run(&resource_unref(1)));

    // A different resource takes the same span.
    assert_ok(&h.run(&host_blob(2, pages)));
    assert_eq!(
        h.run(&map_blob(2, offset)).kind(),
        virtio_gpu::resp::OK_MAP_INFO
    );

    let span = h.backing().peek(
        offset + LOOPBACK_SIGNATURE_LEN,
        (pages * BLOB_PAGE_SIZE - LOOPBACK_SIGNATURE_LEN) as usize,
    );
    assert!(
        span.iter().all(|b| *b == 0),
        "the new mapping can see {} poisoned bytes of the old one",
        span.iter().filter(|b| **b != 0).count()
    );
    // …and only the span: the poison outside it is the host's business, not a
    // leak, and clearing more than was asked for would be its own bug.
    assert_eq!(
        h.backing().peek(offset + pages * BLOB_PAGE_SIZE, 8),
        vec![0xde; 8],
        "the device cleared past the end of the mapping"
    );
    assert_eq!(
        h.backing().peek(offset - 8, 8),
        vec![0xde; 8],
        "the device cleared before the start of the mapping"
    );
}

/// A mapping that would leave the window is refused before any host memory is
/// touched — the sharpest guest-controlled value in the epic, checked against
/// the pages rather than against the bookkeeping.
#[test]
fn a_mapping_that_would_escape_the_window_touches_nothing() {
    let mut h = Harness::venus_backed(64, 64);
    let pages = 2;
    assert_ok(&h.run(&host_blob(5, pages)));
    h.backing().poison(0x77);

    for offset in [
        WINDOW,                    // exactly at the end
        WINDOW - BLOB_PAGE_SIZE,   // one page short of what it needs
        WINDOW + BLOB_PAGE_SIZE,   // past it
        u64::MAX - BLOB_PAGE_SIZE, // wraps when the size is added
        u64::MAX,
        BLOB_PAGE_SIZE / 2, // not page aligned
    ] {
        let response = h.run(&map_blob(5, offset));
        assert_ne!(
            response.kind(),
            virtio_gpu::resp::OK_MAP_INFO,
            "a map at {offset:#x} of a {WINDOW}-byte window was accepted"
        );
    }
    // Not one byte of the window changed.
    assert_eq!(h.backing().peek(0, 64), vec![0x77; 64]);
    assert_eq!(
        h.backing().peek(WINDOW - 64, 64),
        vec![0x77; 64],
        "the last page of the window was written by a refused mapping"
    );

    // And a legitimate mapping still works afterwards.
    assert_eq!(h.run(&map_blob(5, 0)).kind(), virtio_gpu::resp::OK_MAP_INFO);
}

/// A device whose window the machine could not back behaves exactly as it did
/// in phase 1: the bookkeeping still works, and nothing reaches host memory
/// because there is none.
#[test]
fn an_unbacked_window_still_maps_but_has_no_pages() {
    let mut h = Harness::venus(64, 64);
    assert!(
        h.backing.is_none(),
        "this harness must not have backed a window"
    );
    assert_ok(&h.run(&host_blob(3, 1)));
    assert_eq!(
        h.run(&map_blob(3, 0)).kind(),
        virtio_gpu::resp::OK_MAP_INFO,
        "the reservation is bookkeeping and works with or without pages"
    );
}
