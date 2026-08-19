//! Fuzzes the virtio-gpu 3D decode + validation front (ADR-0004, the GPU-007
//! safety requirement: "guest-supplied 3D command streams must be
//! length-validated before dispatch").
//!
//! Two layers are on the fuzzed path:
//!
//! * [`virtio_gpu::renderer::validate_stream`] on raw bytes — the walk over
//!   guest-controlled length fields;
//! * the whole [`Gpu3d`] validation front driven by an arbitrary sequence of
//!   3D commands (create/destroy/attach/transfer/submit with arbitrary ids,
//!   geometries, boxes and offsets) against the [`NullRenderer`], with real
//!   guest memory behind the backing entries.
//!
//! Properties: no panic, no abort (allocations are bounded *before* they
//! happen — debug assertions and overflow checks are on), and the tracked
//! element budget never exceeds its cap.

#![no_main]

use std::sync::Arc;

use arbitrary::Arbitrary;
use libfuzzer_sys::fuzz_target;
use virtio_gpu::protocol::{Box3d, MemEntry, Rect, ResourceCreate3d, Transfer3d};
use virtio_gpu::renderer::validate_stream;
use virtio_gpu::{Gpu3d, NullRenderer};

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
    Reset,
}

#[derive(Debug, Arbitrary)]
struct Case {
    /// Raw bytes for the structural stream validator.
    raw_stream: Vec<u8>,
    /// Command sequence for the validation front.
    ops: Vec<Op>,
}

fuzz_target!(|case: Case| {
    // Layer 1: the length-walk itself, on truly arbitrary bytes.
    let _ = validate_stream(&case.raw_stream);

    // Layer 2: the validation front over the null renderer.
    let mem = Arc::new(virtio_core::testing::guest_memory(MEM_SIZE));
    let mut gpu = Gpu3d::new(Box::new(NullRenderer::new()));
    let mut scratch = Vec::new();

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
            Op::Reset => gpu.reset(),
        }
    }
});
