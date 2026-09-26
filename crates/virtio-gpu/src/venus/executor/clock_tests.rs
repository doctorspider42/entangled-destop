//! The queue clock and doomed objects against the fake host (ADR-0004, the
//! amendment on CSS pages): every submit carries a fence, a destroy behind
//! work in flight waits for nothing, and what it destroyed outlives that
//! work on the host, still charged, until the clock says it has finished.

use std::sync::atomic::Ordering;
use std::sync::Arc;

use super::fake::FakeVulkan;
use super::harness::*;
use super::host::HostVulkan;
use super::limits::{Caps, Class, LimitUsage};
use super::recording::*;
use super::submit::MAX_CLOCK_IN_FLIGHT;
use super::ExecutorFactory;
use crate::renderer::Renderer3d;
use crate::venus::protocol::*;

const BUF: u64 = 0x200;

fn setup_with(factory: ExecutorFactory<FakeVulkan>, host: Arc<FakeVulkan>) -> Harness<FakeVulkan> {
    let _ = host;
    let mut h = Harness::with_factory(factory);
    with_device(&mut h);
    h.send(&allocate_cbs(DEVICE, POOL, &[CB], false)).unwrap();
    h
}

fn setup() -> (Harness<FakeVulkan>, Arc<FakeVulkan>) {
    let host = Arc::new(FakeVulkan::standard());
    let h = setup_with(ExecutorFactory::new(Arc::clone(&host)), Arc::clone(&host));
    (h, host)
}

/// A 64 KiB transfer buffer bound to device-local memory of its own.
fn buffer(h: &mut Harness<FakeVulkan>, id: u64) {
    let Command::CreateBuffer(b) = h
        .call(&create_buffer(DEVICE, id, buffer_info(64 << 10, TRANSFER)))
        .unwrap()
    else {
        panic!()
    };
    assert_eq!(b.ret, VK_SUCCESS);
    h.send(&allocate(DEVICE, id | 0x10_0000, 64 << 10, 0, Vec::new()))
        .unwrap();
    h.send(&bind_buffers(DEVICE, &[(id, id | 0x10_0000, 0)]))
        .unwrap();
}

fn record_fill(h: &mut Harness<FakeVulkan>, cb: u64, buffer: u64) {
    let outcome = h.submit_recording(&[
        begin(cb),
        fill(cb, buffer, 0, WHOLE_SIZE, 0x2222_2222),
        end(cb),
    ]);
    assert_eq!(outcome, Outcome::Consumed);
}

fn queue_wait_idle(h: &mut Harness<FakeVulkan>) -> i32 {
    let Command::QueueWaitIdle(q) = h
        .call(&Command::QueueWaitIdle(QueueWaitIdleArgs {
            queue: VkQueue(QUEUE),
            ret: 0,
        }))
        .unwrap()
    else {
        panic!()
    };
    q.ret
}

fn context_usage(h: &Harness<FakeVulkan>) -> LimitUsage {
    h.renderer
        .factory()
        .with_context(CTX, |c| c.objects.limits().usage())
        .expect("the context exists")
}

fn doomed(h: &Harness<FakeVulkan>) -> usize {
    h.renderer
        .factory()
        .with_context(CTX, |c| c.objects.doomed())
        .expect("the context exists")
}

/// Zink's pattern, the one a CSS page in Firefox sends hundreds of times a
/// frame: buffers of a batch that has finished destroyed while the next
/// batch runs. Not one waits; each is destroyed on the host once the batch
/// that was running when it went has finished, and is charged until then.
#[test]
fn destroys_behind_a_running_batch_wait_for_nothing_and_stay_charged_until_it_ends() {
    const N: u64 = 200;
    let (mut h, host) = setup();
    let ids: Vec<u64> = (0..N).map(|i| BUF + i).collect();
    for id in &ids {
        buffer(&mut h, *id);
    }
    record_fill(&mut h, CB, BUF);
    host.hold.store(true, Ordering::SeqCst);
    h.send(&queue_submit(QUEUE, &[CB], 0)).unwrap();
    let before = context_usage(&h).of(Class::Objects);
    let buffers = host.live("buffer");
    for id in &ids {
        h.send(&destroy_buffer(DEVICE, *id)).unwrap();
    }
    for never in ["vkQueueWaitIdle", "vkDeviceWaitIdle", "vkWaitForFences"] {
        assert_eq!(host.called(never), 0, "{never}: no destroy waited");
    }
    assert_eq!(host.live("buffer"), buffers, "every one outlives the batch");
    assert_eq!(doomed(&h), N as usize);
    assert_eq!(
        context_usage(&h).of(Class::Objects),
        before,
        "and every one is still charged"
    );
    // The guest waits for its batch: the clock's fence signals, the doomed go.
    assert_eq!(queue_wait_idle(&mut h), VK_SUCCESS);
    assert_eq!(host.live("buffer"), buffers - N as usize);
    assert_eq!(doomed(&h), 0);
    assert_eq!(
        context_usage(&h).of(Class::Objects),
        before - N,
        "their charges came back with them"
    );
    assert!(!h.fatal());
}

/// With nothing in flight a destroy is at once, and asks the driver
/// nothing it would have to wait for.
#[test]
fn a_destroy_with_nothing_in_flight_is_immediate() {
    let (mut h, host) = setup();
    buffer(&mut h, BUF);
    record_fill(&mut h, CB, BUF);
    h.send(&queue_submit(QUEUE, &[CB], 0)).unwrap();
    assert_eq!(queue_wait_idle(&mut h), VK_SUCCESS);
    let buffers = host.live("buffer");
    h.send(&destroy_buffer(DEVICE, BUF)).unwrap();
    assert_eq!(host.live("buffer"), buffers - 1);
    assert_eq!(doomed(&h), 0);
}

/// A doomed object goes as soon as the clock is next asked and the GPU
/// is done, even when the guest never waits: the next submit asks.
#[test]
fn the_next_submit_frees_the_doomed_once_the_gpu_is_done() {
    let (mut h, host) = setup();
    buffer(&mut h, BUF);
    buffer(&mut h, BUF + 1);
    record_fill(&mut h, CB, BUF + 1);
    host.hold.store(true, Ordering::SeqCst);
    h.send(&queue_submit(QUEUE, &[CB], 0)).unwrap();
    h.send(&destroy_buffer(DEVICE, BUF)).unwrap();
    assert_eq!(doomed(&h), 1);
    // The fake GPU finishes (a wait anywhere finishes held work), then the
    // guest submits again without waiting itself.
    host.hold.store(false, Ordering::SeqCst);
    let _ = host.device_wait_idle(&0);
    record_fill(&mut h, CB, BUF + 1);
    h.send(&queue_submit(QUEUE, &[CB], 0)).unwrap();
    assert_eq!(doomed(&h), 0, "the submit asked the clock");
    assert!(!h.fatal());
}

/// A guest fence the clock stands on, destroyed while its submit runs, is
/// doomed like anything else: the clock keeps asking it, and it goes after.
#[test]
fn a_guest_fence_destroyed_while_its_submit_runs_outlives_the_submit() {
    let (mut h, host) = setup();
    buffer(&mut h, BUF);
    record_fill(&mut h, CB, BUF);
    h.send(&create_fence(DEVICE, FENCE, false)).unwrap();
    host.hold.store(true, Ordering::SeqCst);
    h.send(&queue_submit(QUEUE, &[CB], FENCE)).unwrap();
    let clock_fences = host.called("vkCreateFence");
    h.send(&destroy_fence(DEVICE, FENCE)).unwrap();
    assert_eq!(host.called("destroy VkFence"), 0, "still the clock's");
    assert_eq!(queue_wait_idle(&mut h), VK_SUCCESS);
    let calls = host.calls();
    let waited = calls
        .iter()
        .rposition(|c| c == "vkWaitForFences")
        .expect("the idle waited on the guest's fence");
    let destroyed = calls
        .iter()
        .rposition(|c| c == "destroy VkFence")
        .expect("then it went");
    assert!(waited < destroyed);
    assert_eq!(
        host.called("vkCreateFence"),
        clock_fences,
        "a fenced submit needs no fence of the clock's"
    );
    assert!(!h.fatal());
}

/// Resetting a fence whose submit may still run waits for that submit
/// first, and only for it; one the host has seen signalled waits for
/// nothing.
#[test]
fn resetting_a_fence_still_in_flight_waits_for_its_submit_first() {
    let (mut h, host) = setup();
    buffer(&mut h, BUF);
    record_fill(&mut h, CB, BUF);
    h.send(&create_fence(DEVICE, FENCE, false)).unwrap();
    host.hold.store(true, Ordering::SeqCst);
    h.send(&queue_submit(QUEUE, &[CB], FENCE)).unwrap();
    h.send(&reset_fences(DEVICE, &[FENCE])).unwrap();
    let calls = host.calls();
    assert!(
        index_of(&calls, "vkWaitForFences") < index_of(&calls, "vkResetFences"),
        "{calls:?}"
    );
    assert_eq!(host.held(), 0);
    // Signalled and seen: a reset waits for nothing.
    h.send(&queue_submit(QUEUE, &[CB], FENCE)).unwrap();
    h.send(&wait_fences(DEVICE, &[FENCE], u64::MAX)).unwrap();
    let waits = host.called("vkWaitForFences");
    h.send(&reset_fences(DEVICE, &[FENCE])).unwrap();
    assert_eq!(host.called("vkWaitForFences"), waits);
    assert!(!h.fatal());
}

/// The marks' fences are reused once signalled, so a guest waiting frame
/// after frame makes the host create a handful, not one a frame.
#[test]
fn the_clocks_fences_are_reused() {
    let (mut h, host) = setup();
    buffer(&mut h, BUF);
    let fences = host.called("vkCreateFence");
    for _ in 0..50 {
        record_fill(&mut h, CB, BUF);
        h.send(&queue_submit(QUEUE, &[CB], 0)).unwrap();
        assert_eq!(queue_wait_idle(&mut h), VK_SUCCESS);
    }
    assert!(
        host.called("vkCreateFence") - fences <= 2,
        "{} fences for 50 submits",
        host.called("vkCreateFence") - fences
    );
    h.renderer.reset();
    assert_eq!(host.live_objects(), 0);
}

/// The clock stands on at most [`MAX_CLOCK_IN_FLIGHT`] fences of a queue:
/// past it the newest guest fence stands in for the one before, so a guest
/// with hundreds of fenced submits in flight is never made to wait, and a
/// reset then waits for the whole queue rather than for a fence nobody
/// remembers.
#[test]
fn a_queue_has_a_bounded_number_of_fences_standing() {
    let (mut h, host) = setup();
    buffer(&mut h, BUF);
    record_fill(&mut h, CB, BUF);
    host.hold.store(true, Ordering::SeqCst);
    let extra = 10;
    let fences: Vec<u64> = (0..(MAX_CLOCK_IN_FLIGHT + extra) as u64)
        .map(|i| 0x7_0000 + i)
        .collect();
    for f in &fences {
        h.send(&create_fence(DEVICE, *f, false)).unwrap();
    }
    for f in &fences {
        h.send(&queue_submit(QUEUE, &[CB], *f)).unwrap();
    }
    assert_eq!(host.called("vkWaitForFences"), 0, "nothing waited");
    let standing = h
        .renderer
        .factory()
        .with_context(CTX, |c| {
            let q = c.objects.queue(QUEUE).expect("the queue");
            (q.clock.in_flight.len(), q.clock.guest_fences.len())
        })
        .expect("the context");
    assert_eq!(standing, (MAX_CLOCK_IN_FLIGHT, MAX_CLOCK_IN_FLIGHT));
    // The first fence is forgotten: resetting it waits for the queue.
    h.send(&reset_fences(DEVICE, &[fences[0]])).unwrap();
    assert!(host.called("vkWaitForFences") >= 1);
    assert_eq!(host.held(), 0, "everything before the reset is done");
    assert!(!h.fatal());
    h.renderer.reset();
    assert_eq!(host.live_objects(), 0);
}

/// A context that goes, and a device the guest destroys, take their
/// doomed objects and the clock's fences with them once idle.
#[test]
fn teardown_destroys_the_doomed_and_the_clock_fences() {
    let (mut h, host) = setup();
    buffer(&mut h, BUF);
    buffer(&mut h, BUF + 1);
    record_fill(&mut h, CB, BUF);
    host.hold.store(true, Ordering::SeqCst);
    h.send(&queue_submit(QUEUE, &[CB], 0)).unwrap();
    h.send(&destroy_buffer(DEVICE, BUF + 1)).unwrap();
    assert_eq!(doomed(&h), 1);
    h.send(&Command::DestroyDevice(DestroyDeviceArgs {
        device: VkDevice(DEVICE),
    }))
    .unwrap();
    assert_eq!(host.live("buffer"), 0);
    assert_eq!(host.live("VkFence"), 0);
    assert_eq!(host.live("device"), 0);
    assert!(!h.fatal());
}

/// A doomed object is still charged: a context at its object cap cannot
/// get past it by destroying what the GPU is still using.
#[test]
fn doomed_objects_count_against_the_objects_cap() {
    let host = Arc::new(FakeVulkan::standard());
    let caps = Caps::default().with(Class::Objects, 40, 1 << 16);
    let mut h = setup_with(
        ExecutorFactory::new(Arc::clone(&host)).with_caps(caps),
        Arc::clone(&host),
    );
    buffer(&mut h, BUF);
    record_fill(&mut h, CB, BUF);
    host.hold.store(true, Ordering::SeqCst);
    h.send(&queue_submit(QUEUE, &[CB], 0)).unwrap();
    // Fill the cap with shader-module-sized nothing: pipeline layouts.
    let mut made = Vec::new();
    for i in 0..64 {
        let id = 0x9000 + i;
        let Command::CreatePipelineLayout(p) =
            h.call(&create_pipeline_layout(DEVICE, id, &[])).unwrap()
        else {
            panic!()
        };
        if p.ret != VK_SUCCESS {
            break;
        }
        made.push(id);
    }
    let cap_reached = made.len();
    assert!(cap_reached < 64, "the cap bit");
    for id in &made {
        h.send(&Command::DestroyPipelineLayout(DestroyPipelineLayoutArgs {
            device: VkDevice(DEVICE),
            pipeline_layout: VkPipelineLayout(*id),
        }))
        .unwrap();
    }
    assert_eq!(doomed(&h), cap_reached);
    let Command::CreatePipelineLayout(p) = h
        .call(&create_pipeline_layout(DEVICE, 0xa000, &[]))
        .unwrap()
    else {
        panic!()
    };
    assert_ne!(p.ret, VK_SUCCESS, "the doomed still count");
    assert_eq!(queue_wait_idle(&mut h), VK_SUCCESS);
    let Command::CreatePipelineLayout(p) = h
        .call(&create_pipeline_layout(DEVICE, 0xa001, &[]))
        .unwrap()
    else {
        panic!()
    };
    assert_eq!(p.ret, VK_SUCCESS, "and give their room back when they go");
    assert!(!h.fatal());
}

fn index_of(calls: &[String], what: &str) -> usize {
    calls
        .iter()
        .position(|c| c == what)
        .unwrap_or_else(|| panic!("{what} never reached the host: {calls:?}"))
}
