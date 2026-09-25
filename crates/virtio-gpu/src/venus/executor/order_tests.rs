//! Shared payloads ordered on the host ([`super::writes`]) against the fake
//! host: a handle blob's scanout read waits for the guest's last submission
//! that touched it, and nothing else; a guest submission touching a payload
//! waits for another context's that is still running, and for the scanout
//! copy; and nothing is left behind.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::error::CommandError;
use crate::protocol::Rect;
use crate::renderer::{Renderer3d, ScanoutBlobSpec};
use crate::venus::protocol::*;
use crate::venus::renderer::SharedRef;
use crate::FORMAT_B8G8R8X8_UNORM;

use super::fake::{texel, FakeVulkan};
use super::harness::*;
use super::policy::QUEUE_FAMILY_FOREIGN as FOREIGN;
use super::recording::*;
use super::s1_tests::{
    device_on, export, gpu, host_with, import_of, importer_image, second_context,
    DEVICE_LOCAL_TYPE, EXPORTED_RES, EXPORTER, H, IMPORTED_MEM, IMPORTER, PITCH, S1_EXTENSIONS, W,
};
use super::scanout::{LAYOUT_COLOR_ATTACHMENT, SCANOUT_WAIT};
use super::writes::{Owner, SharedWaits, SHARED_WAIT};

const STAGE2_COLOR_OUTPUT: u64 = 0x400;
const STAGE2_ALL_COMMANDS: u64 = 0x1_0000;
const ACCESS2_COLOR_WRITE: u64 = 0x100;
const SECONDARY: u64 = 0xc1;

fn flip() -> ScanoutBlobSpec {
    ScanoutBlobSpec {
        format: FORMAT_B8G8R8X8_UNORM,
        width: W,
        height: H,
        stride: PITCH as u32,
        offset: 0,
    }
}

/// Context 1 exporting the compositor's scanout buffer, accepted for
/// scanout.
fn compositor() -> (Harness<FakeVulkan>, Arc<FakeVulkan>) {
    let (h, host, _) = compositor_with_blob();
    (h, host)
}

/// [`compositor`], and the blob's size.
fn compositor_with_blob() -> (Harness<FakeVulkan>, Arc<FakeVulkan>, u64) {
    let host = host_with(vec![gpu("NVIDIA GeForce RTX 2070", 7)]);
    let mut h = Harness::new(Arc::clone(&host));
    device_on(&mut h, &[PHYSICAL], PHYSICAL, S1_EXTENSIONS);
    let blob = export(&mut h);
    h.renderer
        .scanout_blob(EXPORTED_RES, &flip())
        .expect("the canonical image matches the flip");
    (h, host, blob)
}

/// The host handle of context `ctx`'s `DEVICE`.
fn host_device(h: &Harness<FakeVulkan>, ctx: u32) -> u64 {
    h.renderer
        .factory()
        .with_context(ctx, |c| c.objects.device(DEVICE).map(|d| *d.host))
        .expect("the context")
        .expect("its device")
}

fn waits(h: &Harness<FakeVulkan>, ctx: u32) -> SharedWaits {
    h.renderer
        .factory()
        .with_context(ctx, super::context::VulkanContext::shared_waits)
        .expect("the context")
}

/// The payload of the exported blob, as the table keys it.
fn payload(h: &Harness<FakeVulkan>) -> SharedRef {
    h.renderer
        .factory()
        .with_context(CTX, |c| {
            c.objects
                .image(DEVICE, EXPORTER)
                .ok()
                .and_then(|i| i.scanout.as_ref())
                .map(|s| s.shared.clone())
        })
        .flatten()
        .expect("the exporter's image is on record")
}

/// A batch on `image`, as Zink records one: acquired from `FOREIGN`, used
/// (the fake has no draw worth recording; the barriers are what is
/// tracked), and released back — recorded into `CB` and submitted to
/// `QUEUE` without a fence, as Zink submits.
fn zink_batch(h: &mut Harness<FakeVulkan>, image: u64) {
    let outcome = h.submit_recording(&[
        begin(CB),
        image_barrier2_families(
            CB,
            image,
            (LAYOUT_COLOR_ATTACHMENT, LAYOUT_COLOR_ATTACHMENT),
            (0, 0),
            (STAGE2_COLOR_OUTPUT, ACCESS2_COLOR_WRITE),
            (FOREIGN, 0),
        ),
        image_barrier2_families(
            CB,
            image,
            (LAYOUT_COLOR_ATTACHMENT, LAYOUT_COLOR_ATTACHMENT),
            (STAGE2_COLOR_OUTPUT, ACCESS2_COLOR_WRITE),
            (STAGE2_ALL_COMMANDS, 0),
            (0, FOREIGN),
        ),
        end(CB),
    ]);
    assert_eq!(outcome, Outcome::Consumed);
    h.send(&queue_submit(QUEUE, &[CB], 0)).expect("submitted");
    assert!(!h.fatal());
}

fn read(h: &mut Harness<FakeVulkan>, r: Rect) -> Result<Vec<u8>, CommandError> {
    let mut out = Vec::new();
    h.renderer.read_rect_bgra(EXPORTED_RES, r, &mut out)?;
    Ok(out)
}

fn rect(x: u32, y: u32, width: u32, height: u32) -> Rect {
    Rect {
        x,
        y,
        width,
        height,
    }
}

fn texels(r: Rect) -> Vec<u8> {
    (r.y..r.y + r.height)
        .flat_map(|y| (r.x..r.x + r.width).flat_map(move |x| texel(x, y)))
        .collect()
}

/// Wait (bounded) until nothing of the payload is running any more: the
/// fence thread has seen the marks.
fn settle(h: &Harness<FakeVulkan>, p: &SharedRef) {
    let payloads = h.renderer.factory().payloads();
    let deadline = Instant::now() + Duration::from_secs(5);
    while !payloads.running(p).is_empty() {
        assert!(Instant::now() < deadline, "the marks never completed");
        std::thread::sleep(Duration::from_millis(1));
    }
}

/// The rule for the scanout: while the guest's last submission on the
/// scanout buffer has not finished on the host GPU, the scanout device
/// copies nothing — the flush fails in band (the window keeps its last
/// frame) within the bound — and once it has, the read succeeds.
#[test]
fn a_scanout_read_waits_for_the_last_submission_that_touched_the_buffer() {
    let (mut h, host) = compositor();
    let device = host_device(&h, CTX);
    let p = payload(&h);
    host.stick_device(device);
    zink_batch(&mut h, EXPORTER);
    assert_eq!(
        h.renderer.factory().payloads().running(&p),
        vec![Owner::Queue {
            ctx_id: CTX,
            queue: QUEUE
        }],
        "the frame's submit is on record, running"
    );

    let before = host.image_barriers().len();
    let start = Instant::now();
    let err = read(&mut h, rect(0, 0, 4, 4)).expect_err("the frame is still on the GPU");
    assert!(err.to_string().contains("did not finish"), "{err}");
    let took = start.elapsed();
    assert!(took >= SCANOUT_WAIT, "it waited: {took:?}");
    assert!(
        took < SCANOUT_WAIT * 10,
        "and the wait is bounded: {took:?}"
    );
    assert_eq!(
        host.image_barriers().len(),
        before,
        "the scanout device recorded no copy of an unfinished frame"
    );

    host.release_device(device);
    let r = rect(3, 5, 8, 2);
    assert_eq!(read(&mut h, r).expect("finished now"), texels(r));
    assert_eq!(
        host.image_barriers().len(),
        before + 2,
        "one acquire, one release"
    );
    assert!(
        h.renderer.factory().payloads().running(&p).is_empty(),
        "the copy's own touch is complete"
    );

    // The next frame on the same queue is waited for again.
    host.stick_device(device);
    zink_batch(&mut h, EXPORTER);
    assert!(read(&mut h, r).is_err());
    host.release_device(device);
    assert_eq!(read(&mut h, r).unwrap(), texels(r));
}

/// Only work that touched the buffer is waited for: a later submit on the
/// same queue that does not (the compositor's next frame into another
/// buffer), and a command buffer that touched it but was begun again before
/// it was submitted, hold nothing back.
#[test]
fn work_that_did_not_touch_the_buffer_is_not_waited_for() {
    let (mut h, host) = compositor();
    let device = host_device(&h, CTX);
    let p = payload(&h);
    zink_batch(&mut h, EXPORTER);
    settle(&h, &p);
    let r = rect(0, 0, W, 1);
    assert_eq!(read(&mut h, r).unwrap(), texels(r));

    host.stick_device(device);
    h.submit_recording(&[begin(CB), end(CB)]);
    h.send(&queue_submit(QUEUE, &[CB], 0)).unwrap();
    h.submit_recording(&[
        begin(CB),
        image_barrier2_families(
            CB,
            EXPORTER,
            (LAYOUT_COLOR_ATTACHMENT, LAYOUT_COLOR_ATTACHMENT),
            (STAGE2_COLOR_OUTPUT, ACCESS2_COLOR_WRITE),
            (STAGE2_ALL_COMMANDS, 0),
            (0, FOREIGN),
        ),
    ]);
    h.submit_recording(&[begin(CB), end(CB)]);
    h.send(&queue_submit(QUEUE, &[CB], 0)).unwrap();
    assert!(!h.fatal());
    assert!(
        h.renderer.factory().payloads().running(&p).is_empty(),
        "nothing on record"
    );
    let start = Instant::now();
    assert_eq!(read(&mut h, r).expect("not held back"), texels(r));
    assert!(start.elapsed() < SCANOUT_WAIT, "and not waited for");
    host.release_device(device);
}

/// A barrier recorded into a secondary is the primary's once the primary
/// executes it.
#[test]
fn a_secondarys_touch_is_its_primarys() {
    let (mut h, host) = compositor();
    let device = host_device(&h, CTX);
    h.send(&allocate_cbs(DEVICE, POOL, &[SECONDARY], true))
        .unwrap();
    h.submit_recording(&[
        Command::BeginCommandBuffer(BeginCommandBufferArgs {
            command_buffer: VkCommandBuffer(SECONDARY),
            p_begin_info: Some(VkCommandBufferBeginInfo {
                p_next: Vec::new(),
                flags: 0,
                p_inheritance_info: Some(VkCommandBufferInheritanceInfo::default()),
            }),
            ret: 0,
        }),
        image_barrier2_families(
            SECONDARY,
            EXPORTER,
            (LAYOUT_COLOR_ATTACHMENT, LAYOUT_COLOR_ATTACHMENT),
            (STAGE2_COLOR_OUTPUT, ACCESS2_COLOR_WRITE),
            (STAGE2_ALL_COMMANDS, 0),
            (0, FOREIGN),
        ),
        end(SECONDARY),
        begin(CB),
        Command::CmdExecuteCommands(CmdExecuteCommandsArgs {
            command_buffer: VkCommandBuffer(CB),
            command_buffer_count: 1,
            p_command_buffers: Some(vec![VkCommandBuffer(SECONDARY)]),
        }),
        end(CB),
    ]);
    assert!(!h.fatal());
    host.stick_device(device);
    h.send(&queue_submit(QUEUE, &[CB], 0)).unwrap();
    assert!(read(&mut h, rect(0, 0, 1, 1)).is_err(), "waited for");
    host.release_device(device);
    assert!(read(&mut h, rect(0, 0, 1, 1)).is_ok());
}

/// Context 2 imports context 1's buffer (a compositor sampling a client's
/// frame). While context 2's submission on it is still running on the host
/// GPU, context 1's next submission on it waits — until it has finished, or
/// the bound, after which it goes ahead unordered and is counted. A
/// context's submissions never wait for its own, and a submission that does
/// not touch the buffer never waits.
#[test]
fn a_submission_on_a_shared_image_waits_for_another_contexts_running_one() {
    let (mut h, host, blob) = compositor_with_blob();
    second_context(&mut h, &[PHYSICAL], PHYSICAL);
    h.renderer.ctx_attach_blob(2, EXPORTED_RES, true);
    assert_eq!(
        super::s1_tests::create(&mut h, IMPORTER, importer_image(PITCH)),
        VK_SUCCESS
    );
    h.send(&allocate(
        DEVICE,
        IMPORTED_MEM,
        blob,
        DEVICE_LOCAL_TYPE,
        vec![import_of(EXPORTED_RES)],
    ))
    .unwrap();
    h.send(&bind_image(DEVICE, IMPORTER, IMPORTED_MEM, 0))
        .unwrap();
    let (client, compositor) = (host_device(&h, CTX), host_device(&h, 2));
    assert_ne!(client, compositor);

    // The compositor samples the buffer, on a GPU that is busy.
    host.stick_device(compositor);
    zink_batch(&mut h, IMPORTER);
    // Its own next batch on it does not wait for it.
    let start = Instant::now();
    zink_batch(&mut h, IMPORTER);
    assert!(start.elapsed() < SHARED_WAIT, "its own work is its own");
    assert_eq!(waits(&h, 2), SharedWaits::default());

    // The client, meanwhile, touches something else: no wait.
    h.use_context(CTX);
    let start = Instant::now();
    h.submit_recording(&[begin(CB), end(CB)]);
    h.send(&queue_submit(QUEUE, &[CB], 0)).unwrap();
    assert!(start.elapsed() < SHARED_WAIT);

    // The client renders into the buffer again: held until the bound.
    let start = Instant::now();
    zink_batch(&mut h, EXPORTER);
    let took = start.elapsed();
    assert!(took >= SHARED_WAIT, "it waited: {took:?}");
    assert_eq!(
        waits(&h, CTX),
        SharedWaits {
            waited: 1,
            timed_out: 1
        }
    );

    // Now the compositor's GPU finishes while the client waits: the client
    // goes ahead then, before the bound.
    h.use_context(2);
    zink_batch(&mut h, IMPORTER);
    h.use_context(CTX);
    let released = Arc::new(AtomicBool::new(false));
    let releaser = {
        let (host, released) = (Arc::clone(&host), Arc::clone(&released));
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(20));
            released.store(true, Ordering::SeqCst);
            host.release_device(compositor);
        })
    };
    zink_batch(&mut h, EXPORTER);
    assert!(
        released.load(Ordering::SeqCst),
        "it did not go ahead before the compositor's work finished"
    );
    releaser.join().unwrap();
    assert_eq!(
        waits(&h, CTX),
        SharedWaits {
            waited: 2,
            timed_out: 1
        },
        "the second wait ended with the work, not the bound"
    );
}

/// While the scanout device copies the buffer, its own touch is on record
/// and running, so a guest submission touching the buffer cannot start
/// under the copy; it is complete as soon as the copy is.
#[test]
fn the_scanout_copy_is_a_touch_of_its_own_while_it_runs() {
    let (mut h, host) = compositor();
    let p = payload(&h);
    zink_batch(&mut h, EXPORTER);
    settle(&h, &p);
    let payloads = h.renderer.factory().payloads();
    let copies = Arc::new(AtomicUsize::new(0));
    let seen = Arc::new(AtomicBool::new(true));
    {
        let (copies, seen, payloads, p) = (
            Arc::clone(&copies),
            Arc::clone(&seen),
            Arc::clone(&payloads),
            p.clone(),
        );
        *host.on_call.lock().unwrap() = Some(Box::new(move |name: &str| {
            if name == "vkCmdCopyImageToBuffer" {
                copies.fetch_add(1, Ordering::SeqCst);
                seen.fetch_and(
                    payloads.running(&p) == vec![Owner::Scanout],
                    Ordering::SeqCst,
                );
            }
        }));
    }
    let r = rect(0, 0, 2, 2);
    assert_eq!(read(&mut h, r).unwrap(), texels(r));
    *host.on_call.lock().unwrap() = None;
    assert_eq!(copies.load(Ordering::SeqCst), 1);
    assert!(
        seen.load(Ordering::SeqCst),
        "the copy ran as the scanout's touch"
    );
    assert!(payloads.running(&p).is_empty(), "and ended with it");
}

/// Every mark is a host fence of the guest's device, owned by the queue's
/// fence thread: destroyed with the device on context teardown and on a
/// reset, running or not, and its touches forgotten.
#[test]
fn marks_go_with_their_device() {
    let (mut h, host) = compositor();
    let device = host_device(&h, CTX);
    let p = payload(&h);
    for _ in 0..3 {
        zink_batch(&mut h, EXPORTER);
    }
    host.stick_device(device);
    zink_batch(&mut h, EXPORTER);
    assert!(!h.renderer.factory().payloads().running(&p).is_empty());
    host.release_device(device);
    h.renderer.ctx_destroy(CTX);
    assert!(h.renderer.factory().payloads().running(&p).is_empty());
    assert_eq!(
        host.live("VkFence"),
        1,
        "no mark is left: only the scanout device's own fence"
    );
    let (mut h, host) = compositor();
    zink_batch(&mut h, EXPORTER);
    h.renderer.reset();
    drop(h);
    assert_eq!(host.live_objects(), 0, "nothing left on the host");
}
