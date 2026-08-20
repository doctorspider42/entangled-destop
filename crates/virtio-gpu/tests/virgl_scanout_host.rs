//! The phase-2 **packed scanout readback** against the real library
//! (ADR-0004 phase 2).
//!
//! Its own test binary for the same reason as `virgl_fence_host.rs`: one
//! initialized virglrenderer per process.

#![cfg(target_os = "linux")]

use std::sync::Arc;

use virtio_gpu::protocol::{Box3d, MemEntry, Rect, ResourceCreate3d, Transfer3d};
use virtio_gpu::virgl::VirglRenderer;
use virtio_gpu::Gpu3d;
use vm_memory::{Bytes, GuestAddress};

const MEM_SIZE: u64 = 1 << 20;

/// `VIRGL_FORMAT_B8G8R8X8_UNORM`.
const FORMAT_BGRX: u32 = 2;
/// `PIPE_TEXTURE_2D`.
const TARGET_2D: u32 = 2;
/// `VIRGL_RES_BIND_RENDER_TARGET | VIRGL_RES_BIND_SCANOUT`.
const BIND_SCANOUT_RT: u32 = (1 << 1) | (1 << 18);

/// The phase-2 readback: a **partial** rect must come back packed and from
/// the right place in the texture.
///
/// Phase 1 read the rect into a full-frame shadow at a full-frame offset and
/// then repacked the rows; the packed direct read replaces both, and this is
/// the test that pins the stride semantics of
/// `virgl_renderer_transfer_read_iov` against the real library rather than
/// against a model.
#[test]
fn a_partial_scanout_rect_reads_back_packed_and_correct() {
    let renderer = match VirglRenderer::load() {
        Ok(renderer) => renderer,
        Err(e) => {
            eprintln!("skipping: {e}");
            return;
        }
    };
    let mut gpu = Gpu3d::new(Box::new(renderer));
    let mem = Arc::new(virtio_core::testing::guest_memory(MEM_SIZE));
    if let Err(e) = gpu.ctx_create(1, 0, "rect-test") {
        eprintln!("skipping: renderer loaded but EGL/GL is unusable: {e}");
        return;
    }

    const W: u32 = 8;
    const H: u32 = 8;
    gpu.resource_create(&ResourceCreate3d {
        resource_id: 21,
        target: TARGET_2D,
        format: FORMAT_BGRX,
        bind: BIND_SCANOUT_RT,
        width: W,
        height: H,
        depth: 1,
        array_size: 1,
        last_level: 0,
        nr_samples: 0,
        flags: 0,
    })
    .expect("resource_create_3d");

    // Every pixel encodes its own coordinates: B = x, G = y.
    let mut image = Vec::with_capacity((W * H * 4) as usize);
    for y in 0..H {
        for x in 0..W {
            image.extend_from_slice(&[x as u8, y as u8, 0x20, 0xff]);
        }
    }
    mem.write_slice(&image, GuestAddress(0x4000))
        .expect("seed guest pixels");
    gpu.attach_backing(
        21,
        &mem,
        &[MemEntry {
            addr: 0x4000,
            length: image.len() as u32,
        }],
    )
    .expect("attach_backing");
    gpu.transfer(
        0,
        &Transfer3d {
            region: Box3d {
                x: 0,
                y: 0,
                z: 0,
                w: W,
                h: H,
                d: 1,
            },
            offset: 0,
            resource_id: 21,
            level: 0,
            stride: 0,
            layer_stride: 0,
        },
        true,
    )
    .expect("transfer_to_host_3d");

    // A 3x2 rect at (5, 6) — narrower than the resource, and against the
    // right/bottom edge, which is where a stride mistake shows up.
    let rect = Rect {
        x: 5,
        y: 6,
        width: 3,
        height: 2,
    };
    let mut out = Vec::new();
    gpu.read_rect_bgra(21, rect, &mut out).expect("read_rect");
    assert_eq!(
        out.len(),
        (rect.width * rect.height * 4) as usize,
        "the readback must be tightly packed: no full-frame padding"
    );
    let mut expected = Vec::new();
    for y in rect.y..rect.y + rect.height {
        for x in rect.x..rect.x + rect.width {
            expected.extend_from_slice(&[x as u8, y as u8, 0x20, 0xff]);
        }
    }
    assert_eq!(
        out, expected,
        "a partial rect must come back as its own rows, from its own offset"
    );

    gpu.reset();
}
