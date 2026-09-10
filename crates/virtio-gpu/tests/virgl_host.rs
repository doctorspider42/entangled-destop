//! The real renderer, on the real host GL (ADR-0004's GPU-001 spike made
//! permanent): loads libvirglrenderer, initializes surfaceless EGL and pushes
//! actual pixels through `Gpu3d` — context, `RESOURCE_CREATE_3D`, backing,
//! `TRANSFER_TO_HOST_3D`, an (empty) `SUBMIT_3D`, and the scanout readback
//! path virtio-gpu flushes use.
//!
//! Self-skips, like the KVM/WHP tests, when the host has no usable library or
//! EGL — so `cargo test --workspace` stays green on bare CI boxes while a WSL
//! or desktop host actually exercises the FFI.

#![cfg(target_os = "linux")]

use std::sync::Arc;

use virtio_gpu::protocol::{Box3d, MemEntry, Rect, ResourceCreate3d, Transfer3d};
use virtio_gpu::virgl::VirglRenderer;
use virtio_gpu::Gpu3d;
use vm_memory::{Bytes, GuestAddress};

const MEM_SIZE: u64 = 1 << 20;

/// `VIRGL_FORMAT_B8G8R8X8_UNORM` — what a Linux guest uses for its
/// framebuffer.
const FORMAT_BGRX: u32 = 2;
/// `PIPE_TEXTURE_2D`.
const TARGET_2D: u32 = 2;
/// `VIRGL_RES_BIND_RENDER_TARGET | VIRGL_RES_BIND_SCANOUT`.
const BIND_SCANOUT_RT: u32 = (1 << 1) | (1 << 18);

#[test]
fn virglrenderer_round_trips_pixels_through_host_gl() {
    let renderer = match VirglRenderer::load() {
        Ok(renderer) => renderer,
        Err(e) => {
            eprintln!("skipping: {e}");
            return;
        }
    };
    assert_venus_probe_is_self_consistent(&renderer);
    let mut gpu = Gpu3d::new(Box::new(renderer));
    let mem = Arc::new(virtio_core::testing::guest_memory(MEM_SIZE));

    // The first real command initializes EGL; a host with the library but no
    // GL is also a skip, not a failure.
    if let Err(e) = gpu.ctx_create(1, 0, "virgl-host-test") {
        eprintln!("skipping: renderer loaded but EGL/GL is unusable: {e}");
        return;
    }

    // Capsets must be the real thing now.
    assert!(
        gpu.num_capsets() >= 1,
        "an initialized virglrenderer serves at least the VIRGL capset"
    );
    let info = gpu.capset_info(0).expect("capset 0 exists");
    assert_eq!(info.id, 1, "capset 0 is VIRTIO_GPU_CAPSET_VIRGL");
    let blob = gpu.capset(info.id, info.max_version).expect("capset blob");
    assert_eq!(blob.len(), info.max_size as usize);
    assert!(
        blob.iter().any(|b| *b != 0),
        "a real capset advertises something"
    );

    // A 4x4 BGRX texture, fed from guest pages, read back through the same
    // call the scanout flush uses.
    let create = ResourceCreate3d {
        resource_id: 7,
        target: TARGET_2D,
        format: FORMAT_BGRX,
        bind: BIND_SCANOUT_RT,
        width: 4,
        height: 4,
        depth: 1,
        array_size: 1,
        last_level: 0,
        nr_samples: 0,
        flags: 0,
    };
    gpu.resource_create(&create).expect("resource_create_3d");
    gpu.ctx_resource(1, 7, true).expect("ctx_attach_resource");

    let red_bgrx = [0x00u8, 0x00, 0xff, 0xff].repeat(16);
    mem.write_slice(&red_bgrx, GuestAddress(0x4000))
        .expect("seed guest pixels");
    gpu.attach_backing(
        7,
        &mem,
        &[MemEntry {
            addr: 0x4000,
            length: 64,
        }],
    )
    .expect("attach_backing (iovec across the FFI)");

    let xfer = Transfer3d {
        region: Box3d {
            x: 0,
            y: 0,
            z: 0,
            w: 4,
            h: 4,
            d: 1,
        },
        offset: 0,
        resource_id: 7,
        level: 0,
        stride: 0,
        layer_stride: 0,
    };
    gpu.transfer(0, &xfer, true).expect("transfer_to_host_3d");

    // An empty command stream is a legal submit (mesa flushes empty cmdbufs).
    gpu.submit(1, &[]).expect("empty submit");

    // GPU-010: the readback path the scanout flush drives.
    let mut out = Vec::new();
    gpu.read_rect_bgra(
        7,
        Rect {
            x: 0,
            y: 0,
            width: 4,
            height: 4,
        },
        &mut out,
    )
    .expect("read_rect_bgra through virgl_renderer_transfer_read_iov");
    assert_eq!(
        out, red_bgrx,
        "pixels round-tripped guest pages -> host GL texture -> BGRA readback"
    );

    // Reads back into fresh guest pages too (TRANSFER_FROM_HOST_3D).
    mem.write_slice(&[0u8; 64], GuestAddress(0x8000))
        .expect("clear");
    gpu.detach_backing(7).expect("detach");
    gpu.attach_backing(
        7,
        &mem,
        &[MemEntry {
            addr: 0x8000,
            length: 64,
        }],
    )
    .expect("re-attach");
    gpu.transfer(1, &xfer, false)
        .expect("transfer_from_host_3d");
    let mut round = [0u8; 64];
    mem.read_slice(&mut round, GuestAddress(0x8000))
        .expect("read");
    assert_eq!(&round[..], red_bgrx.as_slice());

    // Teardown in kernel order; reset leaves the renderer reusable.
    gpu.ctx_resource(1, 7, false).expect("ctx_detach");
    gpu.resource_unref(7).expect("unref");
    gpu.ctx_destroy(1).expect("ctx_destroy");
    gpu.reset();
    gpu.ctx_create(2, 0, "after-reset")
        .expect("the renderer survives a device reset");
}

/// VEN-2003's host probe, made permanent and printable: what this machine's
/// libvirglrenderer actually offers for Venus.
///
/// Not an assertion about *which* answer is right — jammy's 0.9.1 has no Venus
/// and a self-built 1.x does — but about the answers being **consistent**: the
/// venus capset is advertised if and only if the renderer will accept a
/// venus-typed context and a blob, and if and only if it asks for a
/// host-visible window it will host-map. Folded into the round-trip test
/// rather than standing alone because the library is a process singleton, so
/// two tests that both `load()` would race for it and one would always skip.
fn assert_venus_probe_is_self_consistent(renderer: &VirglRenderer) {
    let capsets: Vec<_> = virtio_gpu::Renderer3d::capsets(renderer).to_vec();
    let blob = virtio_gpu::Renderer3d::blob_support(renderer);
    let venus = capsets
        .iter()
        .find(|c| c.id == virtio_gpu::CAPSET_VENUS)
        .copied();
    eprintln!("VEN-2003 host probe: capsets={capsets:?} blob_support={blob:?}");

    assert_eq!(
        venus.is_some(),
        blob.any(),
        "the venus capset and blob support come from the same set of symbols;          advertising one without the other hands the guest a device it cannot use"
    );
    // The window and the mode travel together, in both directions. A window
    // without `host_mapped` would be a device that writes pages the guest
    // reads through Vulkan; `host_mapped` without a window would be a mode
    // with nothing to apply it to.
    assert_eq!(
        blob.host_visible_bytes.is_some(),
        blob.host_mapped,
        "a Venus renderer maps its own pages into the window it asked for"
    );
    assert_eq!(
        blob.host_visible_bytes.is_some(),
        venus.is_some(),
        "only a Venus renderer has host memory to put in a window"
    );
    match venus {
        Some(info) => {
            assert!(info.max_size > 0, "an advertised capset must have a blob");
            eprintln!(
                "VEN-2003: this host serves Venus (capset {} bytes, {} MiB window)",
                info.max_size,
                blob.host_visible_bytes.unwrap_or(0) >> 20
            );
        }
        None => eprintln!(
            "VEN-2003: this host's virglrenderer has no Venus support;              the device stays on classic virgl"
        ),
    }
}
