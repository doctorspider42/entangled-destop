//! The isolated renderer, end to end, including its crash (ADR-0004's
//! GPU-012 amendment).
//!
//! A **real second process** is involved: the client spawns a helper, the
//! whole 3D path runs across the socket (contexts, resources, backing
//! shadows, transfers both ways, submits, scanout readback, fences), and then
//! the helper is `SIGKILL`ed mid-flight to prove the failure is contained —
//! commands fail in band, `is_alive()` goes false, and nothing in this process
//! dies.
//!
//! # How the helper is spawned
//!
//! This test binary re-executes *itself* with `ENTANGLED_REMOTE_TEST_SERVER`
//! set, which makes [`the_remote_test_server_child`] serve the protocol on its
//! stdin instead of asserting anything. That keeps the containment test
//! honest — a genuine process boundary, a genuine `kill` — without shipping a
//! test-only binary or depending on the `entangled` executable from a
//! different crate.
//!
//! The helper runs a [`NullRenderer`], so this test needs no GPU and no
//! virglrenderer: what is under test is the *containment*, not the GL.

#![cfg(unix)]

use std::process::Command;
use std::sync::Arc;
use std::time::{Duration, Instant};

use virtio_gpu::protocol::{Box3d, MemEntry, Rect, ResourceCreate3d, Transfer3d};
use virtio_gpu::remote::{RemoteRenderer, SpawnError};
use virtio_gpu::renderer::FenceOutcome;
use virtio_gpu::{CommandError, Gpu3d, NullRenderer};
use vm_memory::{Bytes, GuestAddress};

const SERVER_ENV: &str = "ENTANGLED_REMOTE_TEST_SERVER";
const MEM_SIZE: u64 = 1 << 20;
const FB_ADDR: u64 = 0x4000;

/// Not a test: the helper process. Returns immediately unless
/// [`SERVER_ENV`] marks this process as the child.
#[test]
fn the_remote_test_server_child() {
    if std::env::var_os(SERVER_ENV).is_none() {
        return;
    }
    // Serving until the client hangs up is this process's whole life.
    let outcome = virtio_gpu::remote::serve_stdin(Box::new(NullRenderer::new()));
    if let Err(error) = outcome {
        eprintln!("remote test server: {error}");
    }
    // `exit` rather than returning: the libtest harness would otherwise print
    // a summary into the (inherited) stdout of the VMM-side test.
    std::process::exit(0);
}

/// Spawns this binary as a renderer helper.
fn spawn_helper() -> Result<RemoteRenderer, SpawnError> {
    let exe = std::env::current_exe().expect("test binary path");
    let mut command = Command::new(exe);
    command
        .arg("--exact")
        .arg("the_remote_test_server_child")
        .arg("--nocapture")
        .arg("--quiet")
        .env(SERVER_ENV, "1");
    RemoteRenderer::spawn_with(command)
}

fn create_args(id: u32, width: u32, height: u32) -> ResourceCreate3d {
    ResourceCreate3d {
        resource_id: id,
        target: 2,
        format: 2,
        bind: 1 << 18,
        width,
        height,
        depth: 1,
        array_size: 1,
        last_level: 0,
        nr_samples: 0,
        flags: 0,
    }
}

fn xfer(id: u32, w: u32, h: u32) -> Transfer3d {
    Transfer3d {
        region: Box3d {
            x: 0,
            y: 0,
            z: 0,
            w,
            h,
            d: 1,
        },
        offset: 0,
        resource_id: id,
        level: 0,
        stride: 0,
        layer_stride: 0,
    }
}

/// The whole 3D path across a process boundary, including pixels: guest pages
/// → socket → the helper's shadow backing → its renderer → back over the
/// socket as a packed BGRA rect.
///
/// This is what proves the "the helper never sees a guest address" design
/// actually works: every byte crossed by copy, and the pixels still match.
#[test]
fn the_isolated_renderer_serves_the_whole_3d_path() {
    let renderer = match spawn_helper() {
        Ok(renderer) => renderer,
        Err(error) => panic!("could not start the renderer helper: {error}"),
    };
    let pid = renderer.pid();
    assert!(pid > 0);
    let mut gpu = Gpu3d::new(Box::new(renderer));
    let mem = Arc::new(virtio_core::testing::guest_memory(MEM_SIZE));

    // Capsets came back with the handshake, before any command.
    assert_eq!(
        gpu.num_capsets(),
        2,
        "the helper's capsets crossed the wire"
    );
    let info = gpu.capset_info(0).expect("capset 0");
    assert_eq!(info.id, 1);
    let blob = gpu.capset(1, 1).expect("capset blob");
    assert_eq!(blob.len(), info.max_size as usize);

    gpu.ctx_create(1, 0, "remote").expect("ctx_create");
    gpu.resource_create(&create_args(10, 2, 2))
        .expect("resource_create_3d");
    gpu.ctx_resource(1, 10, true).expect("ctx_attach");

    // Guest pixels; the client reads them out of guest memory and ships the
    // bytes, so the helper never learns FB_ADDR.
    let image: Vec<u8> = (0..16u8).collect();
    mem.write_slice(&image, GuestAddress(FB_ADDR))
        .expect("seed");
    gpu.attach_backing(
        10,
        &mem,
        &[MemEntry {
            addr: FB_ADDR,
            length: 16,
        }],
    )
    .expect("attach_backing");
    gpu.transfer(1, &xfer(10, 2, 2), true)
        .expect("transfer_to_host");
    gpu.submit(1, &[]).expect("empty submit");

    let mut out = Vec::new();
    gpu.read_rect_bgra(
        10,
        Rect {
            x: 0,
            y: 0,
            width: 2,
            height: 2,
        },
        &mut out,
    )
    .expect("read_rect_bgra");
    assert_eq!(
        out, image,
        "pixels survived the round trip through the helper"
    );

    // …and back into different guest pages (TRANSFER_FROM_HOST_3D).
    mem.write_slice(&[0u8; 16], GuestAddress(FB_ADDR + 0x1000))
        .expect("clear");
    gpu.detach_backing(10).expect("detach");
    gpu.attach_backing(
        10,
        &mem,
        &[MemEntry {
            addr: FB_ADDR + 0x1000,
            length: 16,
        }],
    )
    .expect("re-attach");
    gpu.transfer(1, &xfer(10, 2, 2), false)
        .expect("transfer_from_host");
    let mut round = [0u8; 16];
    mem.read_slice(&mut round, GuestAddress(FB_ADDR + 0x1000))
        .expect("read");
    assert_eq!(
        &round[..],
        image.as_slice(),
        "the helper's readback landed in the new guest pages"
    );

    // Teardown, then reset: the helper survives both and stays usable.
    gpu.ctx_resource(1, 10, false).expect("ctx_detach");
    gpu.resource_unref(10).expect("unref");
    gpu.ctx_destroy(1).expect("ctx_destroy");
    gpu.reset();
    gpu.ctx_create(2, 0, "after-reset")
        .expect("the helper survives a device reset");
    assert!(gpu.is_alive());
}

/// GPU-012, demonstrated: the renderer process is killed **under load** — a
/// fence outstanding, a context and a backed resource live — and this process
/// keeps running. Every later command fails in band, and `is_alive()` reports
/// the loss so the device can degrade the VM to 2D.
#[test]
fn killing_the_renderer_process_is_contained() {
    let renderer = match spawn_helper() {
        Ok(renderer) => renderer,
        Err(error) => panic!("could not start the renderer helper: {error}"),
    };
    let pid = renderer.pid();
    let mut gpu = Gpu3d::new(Box::new(renderer));
    let mem = Arc::new(virtio_core::testing::guest_memory(MEM_SIZE));

    // A working session first: this must be a crash *under load*, not a
    // failure to start.
    gpu.set_host_waker(Arc::new(NoopWaker));
    gpu.ctx_create(1, 0, "doomed").expect("ctx_create");
    gpu.resource_create(&create_args(10, 64, 64))
        .expect("resource_create_3d");
    let backing = vec![0x5au8; 64 * 64 * 4];
    mem.write_slice(&backing[..16], GuestAddress(FB_ADDR))
        .expect("seed");
    gpu.attach_backing(
        10,
        &mem,
        &[MemEntry {
            addr: FB_ADDR,
            length: 4096,
        }],
    )
    .expect("attach_backing");
    gpu.submit(1, &[]).expect("submit");
    // The fence crosses the wire and comes back — `Signalled` here, because
    // the *helper's* renderer is a NullRenderer whose fences are synchronous;
    // with virglrenderer behind it this is `Pending` and the deferral works
    // exactly as in `gpu_fence.rs`.
    assert!(matches!(
        gpu.create_fence(1, 0x900d),
        Ok(FenceOutcome::Signalled | FenceOutcome::Pending)
    ));
    assert!(gpu.is_alive());

    // Kill it the way the mesa segfault would: no cleanup, no goodbye.
    kill9(pid);

    // The client notices on its next call. Which call that is depends on
    // socket buffering, so drive a few and require that they all *fail in
    // band* and that the renderer is reported lost.
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut errors = 0usize;
    while gpu.is_alive() && Instant::now() < deadline {
        match gpu.submit(1, &[]) {
            Ok(()) => std::thread::sleep(Duration::from_millis(20)),
            Err(CommandError::Renderer(_)) => errors += 1,
            Err(other) => panic!("expected an in-band renderer error, got {other}"),
        }
    }
    assert!(
        !gpu.is_alive(),
        "the client must report a dead renderer within the deadline"
    );
    assert!(errors > 0, "the loss must surface as an in-band error");

    // Everything from here on is refused in band — never a panic, never a
    // hang — which is what lets the device answer the guest and degrade.
    for outcome in [
        gpu.ctx_create(2, 0, "after"),
        gpu.submit(1, &[]),
        gpu.transfer(1, &xfer(10, 2, 2), true),
    ] {
        assert!(
            matches!(outcome, Err(CommandError::Renderer(_))),
            "expected an in-band renderer error, got {outcome:?}"
        );
    }
    let mut out = Vec::new();
    assert!(gpu
        .read_rect_bgra(
            10,
            Rect {
                x: 0,
                y: 0,
                width: 2,
                height: 2
            },
            &mut out
        )
        .is_err());
    // Polling a dead renderer is a no-op, not a wedge: the device's watchdog
    // is what releases the fence it was holding.
    assert!(gpu.poll_fences(1).is_empty());
    // And a reset against a corpse is still infallible.
    gpu.reset();
}

/// A helper that cannot come up must fail the *spawn*, not limp along: a
/// profile that asked for 3D never silently gets software GL (ADR-0004 §7).
#[test]
fn a_helper_that_cannot_start_fails_the_spawn() {
    let mut command = Command::new("/nonexistent/entangled-gpu-renderer");
    command.arg("gpu-renderer");
    let error = RemoteRenderer::spawn_with(command).expect_err("a missing helper cannot succeed");
    assert!(
        matches!(error, SpawnError::Spawn(_)),
        "expected a spawn failure, got {error}"
    );

    // A helper that starts but is not a renderer (here: one that exits at
    // once) fails the handshake rather than limping on. A helper that *hangs*
    // instead of exiting hits `HANDSHAKE_TIMEOUT`, which is the same path with
    // a ten-second wait — not something to spend a test on.
    let error = RemoteRenderer::spawn_with(Command::new("/bin/true"))
        .expect_err("a helper that never answers cannot succeed");
    assert!(
        matches!(error, SpawnError::Handshake(_)),
        "expected a handshake failure, got {error}"
    );
}

/// `SIGKILL`, without pulling in a libc dependency for one call.
fn kill9(pid: u32) {
    let status = Command::new("kill")
        .arg("-9")
        .arg(pid.to_string())
        .status()
        .expect("kill");
    assert!(status.success(), "kill -9 {pid} failed");
}

struct NoopWaker;

impl virtio_core::HostWaker for NoopWaker {
    fn wake(&self) {}
}
