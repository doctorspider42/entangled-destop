//! One **real host fence** on the real host GL (ADR-0004 phase 2).
//!
//! Its own test binary because `virglrenderer` is a process singleton (one
//! initialized renderer per process, ADR-0004's first amendment): two tests
//! in one binary means the second one skips. Cargo gives every integration
//! test file its own process, which is exactly the isolation this needs.
//!
//! Self-skips without the library or a usable EGL, like `virgl_host.rs`.

#![cfg(target_os = "linux")]

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use virtio_core::HostWaker;
use virtio_gpu::protocol::ResourceCreate3d;
use virtio_gpu::renderer::FenceOutcome;
use virtio_gpu::virgl::VirglRenderer;
use virtio_gpu::Gpu3d;

/// `VIRGL_FORMAT_B8G8R8X8_UNORM`.
const FORMAT_BGRX: u32 = 2;
/// `PIPE_TEXTURE_2D`.
const TARGET_2D: u32 = 2;
/// `VIRGL_RES_BIND_RENDER_TARGET | VIRGL_RES_BIND_SCANOUT`.
const BIND_SCANOUT_RT: u32 = (1 << 1) | (1 << 18);

/// Counts the wakeups the fence monitor asks for; stands in for the machine
/// layer's queue-0 eventfd.
#[derive(Default)]
struct CountingWaker(AtomicUsize);

impl HostWaker for CountingWaker {
    fn wake(&self) {
        self.0.fetch_add(1, Ordering::Release);
    }
}

impl CountingWaker {
    fn count(&self) -> usize {
        self.0.load(Ordering::Acquire)
    }
}

/// ADR-0004 phase 2: a **real** host fence, created on the host GL timeline
/// and retired asynchronously, with the monitor thread doing the waking.
///
/// This is the whole deferred-response mechanism minus the virtqueue: the
/// device's contract is exactly `create_fence` → `Pending` → some later
/// `poll_fences` reports the id, and a wakeup arrives to make that call
/// happen.
#[test]
fn a_real_host_fence_retires_asynchronously_and_wakes_the_device() {
    let renderer = match VirglRenderer::load() {
        Ok(renderer) => renderer,
        Err(e) => {
            eprintln!("skipping: {e}");
            return;
        }
    };
    let mut gpu = Gpu3d::new(Box::new(renderer));
    let waker = Arc::new(CountingWaker::default());
    gpu.set_host_waker(Arc::clone(&waker) as Arc<dyn HostWaker>);

    if let Err(e) = gpu.ctx_create(1, 0, "fence-test") {
        eprintln!("skipping: renderer loaded but EGL/GL is unusable: {e}");
        return;
    }
    // Real GL work to hang the fence on: a resource and a (legal, empty)
    // command stream, exactly what a guest submit looks like at its smallest.
    gpu.resource_create(&ResourceCreate3d {
        resource_id: 11,
        target: TARGET_2D,
        format: FORMAT_BGRX,
        bind: BIND_SCANOUT_RT,
        width: 64,
        height: 64,
        depth: 1,
        array_size: 1,
        last_level: 0,
        nr_samples: 0,
        flags: 0,
    })
    .expect("resource_create_3d");
    gpu.ctx_resource(1, 11, true).expect("ctx_attach_resource");
    gpu.submit(1, &[]).expect("empty submit");

    let fence_id = 0x4242u32;
    match gpu.create_fence(1, fence_id) {
        Ok(FenceOutcome::Pending) => (),
        Ok(FenceOutcome::Signalled) => {
            panic!("a renderer with a host waker must defer its fences (phase 2)")
        }
        Err(e) => panic!("create_fence: {e}"),
    }

    // The device's side of the loop: wait to be woken, then poll. A host
    // fence for trivial work retires in microseconds, so five seconds is a
    // huge deadline — it exists so a stalled host fails the test instead of
    // hanging it.
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut retired = Vec::new();
    while retired.is_empty() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(2));
        retired = gpu.poll_fences(1);
    }
    assert_eq!(
        retired,
        vec![fence_id],
        "the host fence must retire and report exactly its own id"
    );
    assert!(
        waker.count() > 0,
        "the fence monitor must have asked the device to poll at least once"
    );

    // Nothing outstanding: polling is idempotent and reports nothing new.
    assert!(gpu.poll_fences(0).is_empty());

    // A second fence on the same timeline still works (the monitor is
    // re-armed rather than one-shot).
    gpu.submit(1, &[]).expect("second submit");
    assert!(matches!(
        gpu.create_fence(1, fence_id + 1),
        Ok(FenceOutcome::Pending)
    ));
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut retired = Vec::new();
    while retired.is_empty() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(2));
        retired = gpu.poll_fences(1);
    }
    assert_eq!(retired, vec![fence_id + 1]);

    gpu.reset();
}
