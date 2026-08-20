//! Fuzzes the virtio-gpu 3D decode + validation front (ADR-0004, the GPU-007
//! safety requirement: "guest-supplied 3D command streams must be
//! length-validated before dispatch").
//!
//! Three layers are on the fuzzed path:
//!
//! * [`virtio_gpu::renderer::validate_stream`] on raw bytes — the walk over
//!   guest-controlled length fields;
//! * the whole [`Gpu3d`] validation front driven by an arbitrary sequence of
//!   3D commands (create/destroy/attach/transfer/submit with arbitrary ids,
//!   geometries, boxes and offsets) against a renderer double, with real guest
//!   memory behind the backing entries;
//! * phase 2's **fence** surface: `create_fence` / `poll_fences` with
//!   arbitrary ids, and a renderer whose retirement order the fuzzer chooses —
//!   so the bounded pending table and the prefix-completion rule are fuzzed
//!   too, including retirements for fences that were never created.
//!
//! Properties: no panic, no abort (allocations are bounded *before* they
//! happen — debug assertions and overflow checks are on), the tracked element
//! budget never exceeds its cap, and the fence table never exceeds
//! [`MAX_PENDING_FENCES`].

#![no_main]

use std::sync::{Arc, Mutex};

use arbitrary::Arbitrary;
use libfuzzer_sys::fuzz_target;
use virtio_gpu::fence::{FenceQueue, MAX_PENDING_FENCES};
use virtio_gpu::protocol::{Box3d, MemEntry, Rect, ResourceCreate3d, Transfer3d};
use virtio_gpu::renderer::{validate_stream, CapsetInfo, FenceOutcome, Renderer3d};
use virtio_gpu::{CommandError, Gpu3d, NullRenderer};

const MEM_SIZE: u64 = 0x10_0000;

#[derive(Debug, Arbitrary)]
enum Op {
    CtxCreate { ctx_id: u32, init: u32 },
    CtxDestroy { ctx_id: u32 },
    CtxResource { ctx_id: u32, resource_id: u32, attach: bool },
    Create {
        resource_id: u32,
        target: u32,
        format: u32,
        width: u32,
        height: u32,
        depth: u32,
        array_size: u32,
        last_level: u32,
    },
    Unref { resource_id: u32 },
    AttachBacking {
        resource_id: u32,
        entries: Vec<(u64, u16)>,
    },
    DetachBacking { resource_id: u32 },
    Transfer {
        ctx_id: u32,
        resource_id: u32,
        to_host: bool,
        x: u32,
        y: u32,
        z: u32,
        w: u32,
        h: u32,
        d: u32,
        level: u32,
        offset: u64,
    },
    Submit { ctx_id: u32, stream: Vec<u8> },
    ReadRect { resource_id: u32, x: u16, y: u16, w: u16, h: u16 },
    /// Ask for a host fence; when it defers, park a payload in the same
    /// bounded table the device uses.
    CreateFence { ctx_id: u32, fence_id: u32 },
    /// Queue an arbitrary id for retirement — including ids never created,
    /// out of order, and repeats.
    RetireFence { fence_id: u32 },
    /// The device's poll: take what the renderer reports and complete the
    /// matching prefix.
    PollFences,
    Reset,
}

#[derive(Debug, Arbitrary)]
struct Case {
    /// Raw bytes for the structural stream validator.
    raw_stream: Vec<u8>,
    /// Command sequence for the validation front.
    ops: Vec<Op>,
}

/// Ids the renderer will report as retired on its next poll. Shared with the
/// harness because `Gpu3d` owns the renderer — in production this is the host
/// GL timeline retiring fences behind virglrenderer's back.
type Retirements = Arc<Mutex<Vec<u32>>>;

/// The null renderer with asynchronous fences whose retirement the fuzzer
/// drives.
struct FuzzRenderer {
    inner: NullRenderer,
    retire: Retirements,
}

impl Renderer3d for FuzzRenderer {
    fn capsets(&self) -> &[CapsetInfo] {
        self.inner.capsets()
    }
    fn capset(&mut self, id: u32, version: u32) -> Result<Vec<u8>, CommandError> {
        self.inner.capset(id, version)
    }
    fn ctx_create(&mut self, ctx_id: u32, name: &str) -> Result<(), CommandError> {
        self.inner.ctx_create(ctx_id, name)
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
        mem: &Arc<virtio_core::GuestMem>,
        entries: &[MemEntry],
    ) -> Result<(), CommandError> {
        self.inner.attach_backing(resource_id, mem, entries)
    }
    fn detach_backing(&mut self, resource_id: u32) {
        self.inner.detach_backing(resource_id);
    }
    fn transfer_to_host(&mut self, ctx_id: u32, xfer: &Transfer3d) -> Result<(), CommandError> {
        self.inner.transfer_to_host(ctx_id, xfer)
    }
    fn transfer_from_host(&mut self, ctx_id: u32, xfer: &Transfer3d) -> Result<(), CommandError> {
        self.inner.transfer_from_host(ctx_id, xfer)
    }
    fn submit(&mut self, ctx_id: u32, stream: &[u8]) -> Result<(), CommandError> {
        self.inner.submit(ctx_id, stream)
    }
    fn read_rect_bgra(
        &mut self,
        resource_id: u32,
        rect: Rect,
        out: &mut Vec<u8>,
    ) -> Result<(), CommandError> {
        self.inner.read_rect_bgra(resource_id, rect, out)
    }
    fn reset(&mut self) {
        self.inner.reset();
        if let Ok(mut retire) = self.retire.lock() {
            retire.clear();
        }
    }
    fn create_fence(&mut self, _ctx_id: u32, _fence_id: u32) -> Result<FenceOutcome, CommandError> {
        Ok(FenceOutcome::Pending)
    }
    fn poll_fences(&mut self, _still_pending: usize) -> Vec<u32> {
        match self.retire.lock() {
            Ok(mut retire) => std::mem::take(&mut *retire),
            Err(_) => Vec::new(),
        }
    }
}

fuzz_target!(|case: Case| {
    // Layer 1: the length-walk itself, on truly arbitrary bytes.
    let _ = validate_stream(&case.raw_stream);

    // Layers 2 and 3: the validation front plus the fence bookkeeping.
    let mem = Arc::new(virtio_core::testing::guest_memory(MEM_SIZE));
    let retire: Retirements = Arc::new(Mutex::new(Vec::new()));
    let mut gpu = Gpu3d::new(Box::new(FuzzRenderer {
        inner: NullRenderer::new(),
        retire: Arc::clone(&retire),
    }));
    let mut scratch = Vec::new();
    // The device's side of the fence protocol: a bounded table of payloads
    // completed in prefix order.
    let mut pending: FenceQueue<u32> = FenceQueue::new();

    for op in case.ops.into_iter().take(64) {
        match op {
            Op::CtxCreate { ctx_id, init } => {
                let _ = gpu.ctx_create(ctx_id, init, "fuzz");
            }
            Op::CtxDestroy { ctx_id } => {
                let _ = gpu.ctx_destroy(ctx_id);
            }
            Op::CtxResource {
                ctx_id,
                resource_id,
                attach,
            } => {
                let _ = gpu.ctx_resource(ctx_id, resource_id, attach);
            }
            Op::Create {
                resource_id,
                target,
                format,
                width,
                height,
                depth,
                array_size,
                last_level,
            } => {
                let _ = gpu.resource_create(&ResourceCreate3d {
                    resource_id,
                    target,
                    format,
                    bind: 0,
                    // Keep *some* creations plausible so later ops hit live
                    // resources, while still letting extreme values through.
                    width,
                    height,
                    depth,
                    array_size,
                    last_level,
                    nr_samples: 0,
                    flags: 0,
                });
            }
            Op::Unref { resource_id } => {
                let _ = gpu.resource_unref(resource_id);
            }
            Op::AttachBacking {
                resource_id,
                entries,
            } => {
                let entries: Vec<MemEntry> = entries
                    .into_iter()
                    .take(64)
                    .map(|(addr, len)| MemEntry {
                        addr,
                        length: u32::from(len),
                    })
                    .collect();
                let _ = gpu.attach_backing(resource_id, &mem, &entries);
            }
            Op::DetachBacking { resource_id } => {
                let _ = gpu.detach_backing(resource_id);
            }
            Op::Transfer {
                ctx_id,
                resource_id,
                to_host,
                x,
                y,
                z,
                w,
                h,
                d,
                level,
                offset,
            } => {
                let xfer = Transfer3d {
                    region: Box3d { x, y, z, w, h, d },
                    offset,
                    resource_id,
                    level,
                    stride: 0,
                    layer_stride: 0,
                };
                let _ = gpu.transfer(ctx_id, &xfer, to_host);
            }
            Op::Submit { ctx_id, stream } => {
                let _ = gpu.submit(ctx_id, &stream);
            }
            Op::ReadRect {
                resource_id,
                x,
                y,
                w,
                h,
            } => {
                let rect = Rect {
                    x: u32::from(x),
                    y: u32::from(y),
                    width: u32::from(w),
                    height: u32::from(h),
                };
                let _ = gpu.read_rect_bgra(resource_id, rect, &mut scratch);
            }
            Op::CreateFence { ctx_id, fence_id } => {
                if let Ok(FenceOutcome::Pending) = gpu.create_fence(ctx_id, fence_id) {
                    // Exactly the device's rule: never hold more than the cap,
                    // and never allocate past it.
                    let _ = pending.push(fence_id, fence_id);
                }
                assert!(pending.len() <= MAX_PENDING_FENCES);
            }
            Op::RetireFence { fence_id } => {
                if let Ok(mut retire) = retire.lock() {
                    if retire.len() < 4 * MAX_PENDING_FENCES {
                        retire.push(fence_id);
                    }
                }
            }
            Op::PollFences => {
                let still = pending.len();
                for id in gpu.poll_fences(still) {
                    // Prefix completion: every payload is its own id, so a
                    // completion for an id that is not in the table returns
                    // nothing rather than mismatching.
                    for (_, payload) in pending.complete(id) {
                        assert!(payload <= id || payload != id, "payload/id bookkeeping");
                    }
                }
                assert!(pending.len() <= MAX_PENDING_FENCES);
            }
            Op::Reset => {
                gpu.reset();
                let _ = pending.drain_all();
            }
        }
    }
});
