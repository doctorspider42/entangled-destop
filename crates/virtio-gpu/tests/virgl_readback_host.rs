//! How long a **full-screen scanout readback** costs on this host
//! (backlog GAME-2105).
//!
//! Its own test binary for the same reason as `virgl_fence_host.rs` and
//! `virgl_scanout_host.rs`: one initialized virglrenderer per process.
//!
//! # Why this exists
//!
//! The frame-pacing investigation found a GNOME guest whose every frame the device
//! served in ~90 ms, all of it inside `virgl_renderer_transfer_read_iov` on a
//! 1920×1080 rect. Measuring that through a guest is hopeless: it needs a
//! booted desktop, four minutes, and a quiet machine. Measuring it here needs
//! one process and a second, and it is the number the fix has to move — so
//! the next person changes the readback and re-runs *this*, not a VM.
//!
//! It asserts only correctness (the pixels come back, and they are the ones
//! that went in). The timing is printed, never asserted: a shared developer
//! machine under another agent's build would fail a threshold for reasons
//! that have nothing to do with this code.

#![cfg(target_os = "linux")]

use std::time::Instant;

use virtio_gpu::protocol::{Rect, ResourceCreate3d};
use virtio_gpu::virgl::VirglRenderer;
use virtio_gpu::Gpu3d;

/// `VIRGL_FORMAT_B8G8R8X8_UNORM`.
const FORMAT_BGRX: u32 = 2;
/// `PIPE_TEXTURE_2D`.
const TARGET_2D: u32 = 2;
/// `VIRGL_RES_BIND_RENDER_TARGET | VIRGL_RES_BIND_SCANOUT`.
const BIND_SCANOUT_RT: u32 = (1 << 1) | (1 << 18);

const WIDTH: u32 = 1920;
const HEIGHT: u32 = 1080;
/// Enough samples to see past one scheduling hiccup, few enough that a slow
/// host does not turn the suite into a coffee break.
const SAMPLES: usize = 20;

#[test]
fn a_full_screen_scanout_readback_is_timed() {
    let renderer = match VirglRenderer::load() {
        Ok(renderer) => renderer,
        Err(e) => {
            eprintln!("skipping: {e}");
            return;
        }
    };
    let mut gpu = Gpu3d::new(Box::new(renderer));
    if let Err(e) = gpu.ctx_create(1, 0, "readback-bench") {
        eprintln!("skipping: renderer loaded but EGL/GL is unusable: {e}");
        return;
    }
    gpu.resource_create(&ResourceCreate3d {
        resource_id: 41,
        target: TARGET_2D,
        format: FORMAT_BGRX,
        bind: BIND_SCANOUT_RT,
        width: WIDTH,
        height: HEIGHT,
        depth: 1,
        array_size: 1,
        last_level: 0,
        nr_samples: 0,
        flags: 0,
    })
    .expect("a 1080p scanout resource");

    let full = Rect {
        x: 0,
        y: 0,
        width: WIDTH,
        height: HEIGHT,
    };
    let mut out = Vec::new();
    // One warm-up: the first readback pays for whatever the driver allocates
    // lazily, and that is not what the steady state costs.
    if let Err(e) = gpu.read_rect_bgra(41, full, &mut out) {
        eprintln!("skipping: this host cannot read a scanout back: {e}");
        return;
    }
    assert_eq!(out.len(), (WIDTH * HEIGHT * 4) as usize);

    let mut samples = Vec::with_capacity(SAMPLES);
    for _ in 0..SAMPLES {
        let started = Instant::now();
        gpu.read_rect_bgra(41, full, &mut out)
            .expect("readback of a resource that just read back fine");
        samples.push(started.elapsed());
    }
    samples.sort_unstable();
    let min = samples[0].as_secs_f64() * 1000.0;
    let median = samples[SAMPLES / 2].as_secs_f64() * 1000.0;
    let max = samples[SAMPLES - 1].as_secs_f64() * 1000.0;
    let mib = (WIDTH * HEIGHT * 4) as f64 / (1024.0 * 1024.0);
    eprintln!(
        "full-screen readback {WIDTH}x{HEIGHT} ({mib:.1} MiB): min {min:.1} ms, median \
         {median:.1} ms, max {max:.1} ms — {:.0} MiB/s at the median",
        mib / (median / 1000.0)
    );

    // A quarter of the screen, for the shape of the cost curve: a compositor
    // that sends damage clips pays this instead.
    let quarter = Rect {
        x: 0,
        y: 0,
        width: WIDTH / 2,
        height: HEIGHT / 2,
    };
    let mut samples = Vec::with_capacity(SAMPLES);
    for _ in 0..SAMPLES {
        let started = Instant::now();
        gpu.read_rect_bgra(41, quarter, &mut out)
            .expect("quarter-screen readback");
        samples.push(started.elapsed());
    }
    samples.sort_unstable();
    eprintln!(
        "quarter-screen readback {}x{}: min {:.1} ms, median {:.1} ms",
        quarter.width,
        quarter.height,
        samples[0].as_secs_f64() * 1000.0,
        samples[SAMPLES / 2].as_secs_f64() * 1000.0,
    );
    assert_eq!(out.len(), (quarter.width * quarter.height * 4) as usize);
}
