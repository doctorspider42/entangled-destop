//! The isolated renderer, end to end, including its crash (ADR-0004's
//! GPU-012 amendment; on Windows, backlog VEN-2004).
//!
//! A **real second process** is involved: the client spawns a helper, the
//! whole 3D path runs across the channel (contexts, resources, backing
//! shadows, transfers both ways, submits, scanout readback, fences), and then
//! the helper is killed mid-flight to prove the failure is contained —
//! commands fail in band, `is_alive()` goes false, and nothing in this process
//! dies.
//!
//! **Both hosts run this file**, which is the point of VEN-2004: the same
//! assertions, over a `socketpair` on Unix and a duplex named pipe on Windows,
//! with `SIGKILL` and `TerminateProcess` as the two spellings of "the renderer
//! died without a goodbye". Only [`kill9`] and the two not-a-renderer commands
//! in [`a_helper_that_cannot_start_fails_the_spawn`] are host-specific.
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
//! virglrenderer: what is under test is the *containment*, not the GL. That
//! matters more on Windows than on Linux, because Windows has no 3D renderer
//! to isolate yet — the isolation itself is what ships, and this is its proof.

#![cfg(any(unix, windows))]

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
    let missing = if cfg!(windows) {
        r"C:\nonexistent\entangled-gpu-renderer.exe"
    } else {
        "/nonexistent/entangled-gpu-renderer"
    };
    let mut command = Command::new(missing);
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
    let mut exits_at_once = if cfg!(windows) {
        let mut command = Command::new("cmd.exe");
        command.args(["/c", "exit"]);
        command
    } else {
        Command::new("/bin/true")
    };
    // Its stdout is the test harness's, and `cmd /c exit` says nothing — but
    // keep the child quiet either way so a failure here is readable.
    exits_at_once.stdout(std::process::Stdio::null());
    let error = RemoteRenderer::spawn_with(exits_at_once)
        .expect_err("a helper that never answers cannot succeed");
    assert!(
        matches!(error, SpawnError::Handshake(_)),
        "expected a handshake failure, got {error}"
    );
}

/// The kill a crashing renderer would deliver to itself: no cleanup, no
/// goodbye, no chance to close the channel politely.
///
/// `SIGKILL` on Unix, `TerminateProcess` on Windows — spelled through the
/// system's own tool so neither host needs a new dependency for one call.
fn kill9(pid: u32) {
    #[cfg(unix)]
    let mut command = {
        let mut command = Command::new("kill");
        command.arg("-9").arg(pid.to_string());
        command
    };
    #[cfg(windows)]
    let mut command = {
        let mut command = Command::new("taskkill.exe");
        command.args(["/F", "/PID", &pid.to_string()]);
        command
    };
    let status = command
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .expect("the system's process-killing tool");
    assert!(status.success(), "killing {pid} failed: {status}");
}

/// No handles leak when renderers come and go (VEN-2004).
///
/// Windows only, because it is the host where this could plausibly go wrong:
/// the channel is four handles per renderer (the pipe server end, our clone of
/// it for the reader, the inheritable client end, and the child's process
/// handle), and three of them are ours to close. A leak here would be silent
/// until a long-lived VMM ran out — so it is asserted, not hoped for.
#[cfg(windows)]
#[test]
fn spawning_and_dropping_renderers_leaks_no_handles() {
    // One warm-up renderer: the first spawn also charges for whatever the
    // runtime opens lazily, and that is not a leak.
    drop(spawn_helper().expect("warm-up helper"));

    let before = process_handle_count();
    for _ in 0..8 {
        // Spawning at all means the handshake crossed the pipe, which is the
        // only liveness this test needs before dropping the renderer again.
        let renderer = spawn_helper().expect("helper");
        assert!(renderer.pid() > 0);
        drop(renderer);
    }
    let after = process_handle_count();

    // Exact equality is the wrong assertion — the thread pool and the child
    // reaper both float by one or two — but eight renderers leaking four
    // handles each would be +32, and that is what this catches.
    assert!(
        after <= before + 8,
        "handle count grew from {before} to {after} over eight renderers"
    );
}

#[cfg(windows)]
fn process_handle_count() -> u32 {
    use windows::Win32::System::Threading::{GetCurrentProcess, GetProcessHandleCount};
    let mut count = 0u32;
    // SAFETY: `GetCurrentProcess` returns the pseudo-handle for this process,
    // which is always valid and needs no closing, and `count` is a live `u32`
    // for the duration of the call.
    unsafe { GetProcessHandleCount(GetCurrentProcess(), &mut count) }
        .expect("this process's handle count");
    count
}

struct NoopWaker;

impl virtio_core::HostWaker for NoopWaker {
    fn wake(&self) {}
}
