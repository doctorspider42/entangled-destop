//! The wait-before-signal amendment against the fake host
//! (`executor::hold`): a wait nothing has submitted a signal for is held on
//! the host, never handed to the driver, and released — in its queue's
//! order, with the virtio-gpu fences behind it — by a signal from another
//! queue, from `vkSignalSemaphore`, by an event set on the host; a wait that
//! nothing ever covers costs only its own context, whose teardown does not
//! wait for it; the holds are capped; and the host-side waits whose answer
//! is behind held work, or that would wait inside the driver with no
//! timeout, never reach it.
//!
//! Every test ends by asking the fake driver whether it was ever handed a
//! wait nothing submitted before it could satisfy
//! (`FakeVulkan::unsatisfiable_waits`): the one thing a real driver must
//! never see.

use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::renderer::{FenceOutcome, FenceTimeline, Renderer3d};
use crate::venus::protocol::*;
use crate::venus::renderer::SinkFactory;

use super::fake::FakeVulkan;
use super::harness::*;
use super::limits::{Caps, Class};
use super::recording::*;
use super::ExecutorFactory;

const QUEUE2: u64 = QUEUE + 1;
const T: u64 = SEMAPHORE + 2;
const T2: u64 = SEMAPHORE + 3;
const BIN: u64 = SEMAPHORE;
const CB2: u64 = CB + 1;
const CB3: u64 = CB + 2;
const EVENT: u64 = 0x3e0;
const BUF: u64 = 0x200;

fn fake() -> Arc<FakeVulkan> {
    Arc::new(FakeVulkan::standard())
}

/// A device with two queues (timelines 1 and 2), three recorded command
/// buffers filling a buffer, and timeline semaphores `T` and `T2` at 0 and
/// a binary one `BIN`.
fn setup_on(mut h: Harness<FakeVulkan>) -> Harness<FakeVulkan> {
    boot(&mut h);
    let mut create = create_device(PHYSICAL, DEVICE, Vec::new());
    if let Command::CreateDevice(a) = &mut create {
        a.p_create_info.as_mut().unwrap().p_queue_create_infos =
            Some(vec![VkDeviceQueueCreateInfo {
                p_next: Vec::new(),
                flags: 0,
                queue_family_index: 0,
                queue_count: 2,
                p_queue_priorities: Some(vec![1.0, 1.0]),
            }]);
    }
    let Command::CreateDevice(d) = h.call(&create).unwrap() else {
        panic!()
    };
    assert_eq!(d.ret, VK_SUCCESS);
    h.send(&create_pool(DEVICE, POOL)).unwrap();
    h.call(&device_queue(DEVICE, QUEUE, 1)).unwrap();
    let mut second = device_queue(DEVICE, QUEUE2, 2);
    if let Command::GetDeviceQueue2(a) = &mut second {
        a.p_queue_info.as_mut().unwrap().queue_index = 1;
    }
    h.call(&second).unwrap();
    h.send(&allocate_cbs(DEVICE, POOL, &[CB, CB2, CB3], false))
        .unwrap();
    let Command::CreateBuffer(b) = h
        .call(&create_buffer(DEVICE, BUF, buffer_info(64 << 10, TRANSFER)))
        .unwrap()
    else {
        panic!()
    };
    assert_eq!(b.ret, VK_SUCCESS);
    h.send(&allocate(DEVICE, BUF | 0x1000, 64 << 10, 0, Vec::new()))
        .unwrap();
    h.send(&bind_buffers(DEVICE, &[(BUF, BUF | 0x1000, 0)]))
        .unwrap();
    for cb in [CB, CB2, CB3] {
        record(&mut h, cb, &[]);
    }
    h.send(&create_semaphore(DEVICE, T, Some(0), 0)).unwrap();
    h.send(&create_semaphore(DEVICE, T2, Some(0), 0)).unwrap();
    h.send(&create_semaphore(DEVICE, BIN, None, 0)).unwrap();
    assert!(!h.fatal());
    h
}

fn setup() -> (Harness<FakeVulkan>, Arc<FakeVulkan>) {
    let host = fake();
    let h = setup_on(Harness::new(Arc::clone(&host)));
    (h, host)
}

/// `cb` recorded to fill the buffer, with `extra` between the fill and the
/// end.
fn record(h: &mut Harness<FakeVulkan>, cb: u64, extra: &[Command<'static>]) {
    let mut commands = vec![begin(cb), fill(cb, BUF, 0, WHOLE_SIZE, 0xa5a5_a5a5)];
    commands.extend_from_slice(extra);
    commands.push(end(cb));
    assert_eq!(h.submit_recording(&commands), Outcome::Consumed);
}

fn held(h: &Harness<FakeVulkan>) -> usize {
    h.renderer
        .factory()
        .with_context(h.ctx, super::context::VulkanContext::held_items)
        .unwrap_or(0)
}

fn counts(h: &Harness<FakeVulkan>) -> super::hold::HoldCounts {
    h.renderer.factory().usage().holds
}

/// Execute `command` on the context directly, as its other ring would.
fn other_ring(h: &Harness<FakeVulkan>, mut command: Command<'static>) {
    h.renderer
        .factory()
        .with_context_mut(h.ctx, |c| c.execute(&mut command))
        .expect("the context")
        .expect("executed");
}

/// The host handle of guest object `id`.
fn raw(h: &Harness<FakeVulkan>, kind: super::objects::Kind, id: u64) -> u64 {
    h.renderer
        .factory()
        .with_context(h.ctx, |c| c.objects.raw_any(kind, id).map(|o| o.host))
        .expect("the context")
        .expect("the object")
}

fn assert_driver_never_waited_in_vain(host: &FakeVulkan) {
    let bad = host.unsatisfiable_waits();
    assert!(bad.is_empty(), "the driver was handed {bad:?}");
}

// ------------------------------------------------------------ timelines

#[test]
fn a_wait_before_its_signal_is_held_and_released_by_a_signal_from_another_queue() {
    let (mut h, host) = setup();
    let before = host.semaphore_ops().len();
    h.send(&submit_semaphores(QUEUE, &[CB], &[(T, 1)], &[], true, 0))
        .unwrap();
    assert!(!h.fatal());
    assert_eq!(held(&h), 1, "held on the host");
    assert_eq!(host.semaphore_ops().len(), before, "the driver saw nothing");
    h.send(&submit_semaphores(QUEUE2, &[CB2], &[], &[(T, 1)], true, 0))
        .unwrap();
    assert_eq!(held(&h), 0, "released by the signal");
    let sem = raw(&h, super::objects::Kind::Semaphore, T);
    let ops = host.semaphore_ops();
    let ops = &ops[before..];
    assert_eq!(ops.len(), 2);
    assert_eq!(
        (ops[0].0, ops[0].2.clone()),
        (1, vec![sem]),
        "the signal first, on queue 2"
    );
    assert_eq!(
        (ops[1].0, ops[1].1.clone()),
        (0, vec![sem]),
        "then the wait, on queue 1"
    );
    let c = counts(&h);
    assert_eq!((c.held, c.released, c.dropped), (1, 1, 0));
    assert_driver_never_waited_in_vain(&host);
}

#[test]
fn a_wait_is_released_by_vk_signal_semaphore_and_not_before_its_value() {
    let (mut h, host) = setup();
    h.send(&submit2_semaphores(QUEUE, &[CB], &[(T, 5)], &[], 0))
        .unwrap();
    assert_eq!(held(&h), 1);
    h.send(&signal_semaphore(DEVICE, T, 3)).unwrap();
    assert_eq!(held(&h), 1, "3 does not reach 5");
    // A host wait for a value nothing reaches never asks the driver.
    let waits = host.called("vkWaitSemaphores");
    let Command::WaitSemaphores(w) = h
        .call(&wait_semaphores(DEVICE, &[(T, 5)], 30_000_000))
        .unwrap()
    else {
        panic!()
    };
    assert_eq!(w.ret, VK_TIMEOUT);
    assert_eq!(
        host.called("vkWaitSemaphores"),
        waits,
        "napped, not waited in the driver"
    );
    h.send(&signal_semaphore(DEVICE, T, 5)).unwrap();
    assert_eq!(held(&h), 0);
    let Command::WaitSemaphores(w) = h
        .call(&wait_semaphores(DEVICE, &[(T, 5)], 1_000_000_000))
        .unwrap()
    else {
        panic!()
    };
    assert_eq!(w.ret, VK_SUCCESS);
    // A wait covered by the initial value is never held.
    h.send(&create_semaphore(DEVICE, T2 + 1, Some(9), 0))
        .unwrap();
    h.send(&submit_semaphores(
        QUEUE2,
        &[CB2],
        &[(T2 + 1, 9)],
        &[],
        true,
        0,
    ))
    .unwrap();
    assert_eq!(held(&h), 0);
    assert!(!h.fatal());
    assert_driver_never_waited_in_vain(&host);
}

#[test]
fn signals_of_earlier_batches_of_one_submit_cover_later_ones_and_nothing_is_held() {
    let (mut h, host) = setup();
    let mut submit = submit_semaphores(QUEUE, &[CB], &[], &[(T, 1)], true, 0);
    let Command::QueueSubmit(two) = submit_semaphores(QUEUE, &[CB2], &[(T, 1)], &[], true, 0)
    else {
        panic!()
    };
    if let Command::QueueSubmit(a) = &mut submit {
        a.p_submits.as_mut().unwrap().extend(two.p_submits.unwrap());
        a.submit_count = 2;
    }
    h.send(&submit).unwrap();
    assert_eq!(held(&h), 0);
    assert_eq!(counts(&h).held, 0);
    assert_driver_never_waited_in_vain(&host);
}

#[test]
fn a_binary_wait_on_a_held_signal_is_held_too_and_both_go_in_order() {
    let (mut h, host) = setup();
    // Queue 1: waits on T (nothing yet) and signals BIN.
    let mut first = submit_semaphores(QUEUE, &[CB], &[(T, 1)], &[(BIN, 0)], true, 0);
    if let Command::QueueSubmit(a) = &mut first {
        // BIN is binary: only T carries a value.
        let s = &mut a.p_submits.as_mut().unwrap()[0];
        s.p_next = vec![VkSubmitInfoNext::VkTimelineSemaphoreSubmitInfo(
            VkTimelineSemaphoreSubmitInfo {
                wait_semaphore_value_count: 1,
                p_wait_semaphore_values: Some(vec![1]),
                signal_semaphore_value_count: 1,
                p_signal_semaphore_values: Some(vec![0]),
            },
        )];
    }
    h.send(&first).unwrap();
    // Queue 2 waits on BIN: valid in the guest's order, but its signal is
    // not the driver's yet.
    h.send(&submit_semaphores(
        QUEUE2,
        &[CB2],
        &[(BIN, 0)],
        &[],
        false,
        0,
    ))
    .unwrap();
    assert!(!h.fatal(), "a binary wait after its signal is valid usage");
    assert_eq!(held(&h), 2);
    h.send(&signal_semaphore(DEVICE, T, 1)).unwrap();
    assert_eq!(held(&h), 0);
    let bin = raw(&h, super::objects::Kind::Semaphore, BIN);
    let ops = host.semaphore_ops();
    let n = ops.len();
    assert_eq!(
        ops[n - 2].2,
        vec![bin],
        "the signal reached the driver first"
    );
    assert_eq!(ops[n - 1].1, vec![bin]);
    assert_driver_never_waited_in_vain(&host);
}

#[test]
fn a_binary_wait_with_no_signal_anywhere_is_still_refused_and_ends_the_context() {
    let (mut h, host) = setup();
    let submits = host.called("vkQueueSubmit");
    assert!(h
        .send(&submit_semaphores(QUEUE, &[CB], &[(BIN, 0)], &[], false, 0))
        .is_err());
    assert!(h.fatal());
    assert_eq!(held(&h), 0, "refused, not held");
    assert_eq!(host.called("vkQueueSubmit"), submits);
}

#[test]
fn an_imported_payload_needs_no_signal_and_waits_only_for_its_queue_s_order() {
    let (mut h, host) = setup();
    // The sync-file emulation: a temporary signalled payload, consumed by
    // the next wait without the driver seeing it.
    h.send(&import_semaphore_resource(DEVICE, BIN, 0)).unwrap();
    h.send(&submit_semaphores(QUEUE, &[CB], &[(BIN, 0)], &[], false, 0))
        .unwrap();
    assert_eq!(held(&h), 0, "an import is a signal the host already has");
    // Behind held work on the same queue, it keeps the queue's order.
    h.send(&submit_semaphores(QUEUE, &[CB2], &[(T, 1)], &[], true, 0))
        .unwrap();
    h.send(&import_semaphore_resource(DEVICE, BIN, 0)).unwrap();
    h.send(&submit_semaphores(
        QUEUE,
        &[CB3],
        &[(BIN, 0)],
        &[],
        false,
        0,
    ))
    .unwrap();
    assert_eq!(held(&h), 2);
    let submitted = host.submitted().len();
    h.send(&signal_semaphore(DEVICE, T, 1)).unwrap();
    assert_eq!(held(&h), 0);
    let cb2 = raw(&h, super::objects::Kind::CommandBuffer, CB2);
    let cb3 = raw(&h, super::objects::Kind::CommandBuffer, CB3);
    assert_eq!(&host.submitted()[submitted..], &[vec![cb2], vec![cb3]]);
    assert_driver_never_waited_in_vain(&host);
}

#[test]
fn a_sync_file_export_of_a_held_signal_waits_behind_it() {
    let (mut h, host) = setup();
    let mut first = submit_semaphores(QUEUE, &[CB], &[(T, 1)], &[(BIN, 0)], true, 0);
    if let Command::QueueSubmit(a) = &mut first {
        let s = &mut a.p_submits.as_mut().unwrap()[0];
        s.p_next = vec![VkSubmitInfoNext::VkTimelineSemaphoreSubmitInfo(
            VkTimelineSemaphoreSubmitInfo {
                wait_semaphore_value_count: 1,
                p_wait_semaphore_values: Some(vec![1]),
                signal_semaphore_value_count: 1,
                p_signal_semaphore_values: Some(vec![0]),
            },
        )];
    }
    h.send(&first).unwrap();
    // `vkGetSemaphoreFdKHR`: an empty submit that waits on the payload.
    h.send(&wait_semaphore_resource(DEVICE, BIN)).unwrap();
    assert_eq!(held(&h), 2, "the export's wait is held behind the signal");
    h.send(&signal_semaphore(DEVICE, T, 1)).unwrap();
    assert_eq!(held(&h), 0);
    assert!(!h.fatal());
    assert_driver_never_waited_in_vain(&host);
}

// ----------------------------------------------------------------- order

#[test]
fn held_work_keeps_its_queue_order_and_the_fences_behind_it_wait_for_it() {
    let (mut h, host) = setup();
    let fence = |h: &mut Harness<FakeVulkan>, id: u32| {
        h.renderer.create_fence_on(CTX, Some(1), id).unwrap()
    };
    let ring1 = FenceTimeline::Ring {
        ctx_id: CTX,
        ring_idx: 1,
    };
    let submitted = host.submitted().len();
    h.send(&submit_semaphores(QUEUE, &[CB], &[(T, 1)], &[], true, 0))
        .unwrap();
    // A submit with nothing to wait for, behind it on the same queue.
    h.send(&queue_submit(QUEUE, &[CB2], 0)).unwrap();
    assert_eq!(fence(&mut h, 10), (ring1, FenceOutcome::Pending));
    h.send(&queue_submit(QUEUE, &[CB3], 0)).unwrap();
    // Another queue is not held up.
    h.send(&queue_submit(QUEUE2, &[CB3], 0)).unwrap();
    assert_eq!(held(&h), 4);
    let cb = |id| raw(&h, super::objects::Kind::CommandBuffer, id);
    let (c1, c2, c3) = (cb(CB), cb(CB2), cb(CB3));
    assert_eq!(
        &host.submitted()[submitted..],
        &[vec![c3]],
        "only queue 2's"
    );
    std::thread::sleep(Duration::from_millis(60));
    assert!(
        h.renderer.poll_fence_timelines(0).is_empty(),
        "the fence behind held work is not answered"
    );
    h.send(&signal_semaphore(DEVICE, T, 1)).unwrap();
    assert_eq!(held(&h), 0);
    // In the guest's order; the fence's own empty submit between.
    let after: Vec<Vec<u64>> = host.submitted()[submitted + 1..].to_vec();
    assert_eq!(after, vec![vec![c1], vec![c2], vec![], vec![c3]]);
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut got = Vec::new();
    while got.is_empty() && Instant::now() < deadline {
        got = h.renderer.poll_fence_timelines(0);
        std::thread::sleep(Duration::from_millis(2));
    }
    assert_eq!(got, vec![(ring1, 10)]);
    assert_eq!(counts(&h).ring_fences, 1);
    assert_driver_never_waited_in_vain(&host);
}

#[test]
fn a_guest_wait_for_held_work_naps_until_another_ring_covers_it() {
    let (mut h, host) = setup();
    h.send(&create_fence(DEVICE, FENCE, false)).unwrap();
    h.send(&submit_semaphores(
        QUEUE,
        &[CB],
        &[(T, 1)],
        &[],
        true,
        FENCE,
    ))
    .unwrap();
    // A status poll and a timed wait on the held fence: answered without
    // the driver.
    let waits = host.called("vkWaitForFences");
    let Command::WaitForFences(w) = h.call(&wait_fences(DEVICE, &[FENCE], 20_000_000)).unwrap()
    else {
        panic!()
    };
    assert_eq!(w.ret, VK_TIMEOUT);
    assert_eq!(host.called("vkWaitForFences"), waits);
    // vkQueueWaitIdle, asynchronously as a guest may send it: it stays
    // behind the held submit, on this ring alone.
    let head = h.head();
    h.produce(&async_bytes(&Command::QueueWaitIdle(QueueWaitIdleArgs {
        queue: VkQueue(QUEUE),
        ret: 0,
    })));
    std::thread::sleep(Duration::from_millis(60));
    assert_eq!(h.head(), head, "still waiting");
    assert_eq!(host.called("vkQueueWaitIdle"), 0, "not in the driver");
    // Another ring of the context signals: the submit goes, and the wait
    // after it ends.
    other_ring(&h, signal_semaphore(DEVICE, T, 1));
    h.wait_head(h.tail());
    assert_eq!(held(&h), 0);
    assert!(counts(&h).naps > 0, "the ring napped, off the lock");
    let Command::WaitForFences(w) = h
        .call(&wait_fences(DEVICE, &[FENCE], 1_000_000_000))
        .unwrap()
    else {
        panic!()
    };
    assert_eq!(w.ret, VK_SUCCESS);
    assert!(!h.fatal());
    assert_driver_never_waited_in_vain(&host);
}

// ------------------------------------------------------ never signalled

#[test]
fn a_wait_nothing_ever_signals_ends_only_its_context_and_its_teardown_does_not_wait() {
    let host = fake();
    let mut h = setup_on(Harness::new(Arc::clone(&host)));
    h.send(&create_fence(DEVICE, FENCE, false)).unwrap();
    h.send(&submit_semaphores(
        QUEUE,
        &[CB],
        &[(T, 1 << 40)],
        &[],
        true,
        FENCE,
    ))
    .unwrap();
    h.send(&queue_submit(QUEUE, &[CB2], 0)).unwrap();
    assert_eq!(
        h.renderer.create_fence_on(CTX, Some(1), 77).unwrap().1,
        FenceOutcome::Pending
    );
    assert_eq!(held(&h), 3);
    assert_eq!(
        h.renderer.factory().usage().limits.of(Class::HeldSubmits),
        3,
        "charged"
    );

    // A second context works as ever meanwhile.
    h.use_context(2);
    with_device(&mut h);
    h.send(&allocate_cbs(DEVICE, POOL, &[CB], false)).unwrap();
    h.send(&create_fence(DEVICE, FENCE, false)).unwrap();
    h.send(&queue_submit(QUEUE, &[], FENCE)).unwrap();
    let Command::WaitForFences(w) = h
        .call(&wait_fences(DEVICE, &[FENCE], 1_000_000_000))
        .unwrap()
    else {
        panic!()
    };
    assert_eq!(w.ret, VK_SUCCESS);

    // The first goes: its held work is dropped, its fence answered, and
    // nothing of it waits for the GPU.
    h.use_context(CTX);
    let started = Instant::now();
    h.renderer.ctx_destroy(CTX);
    let took = started.elapsed();
    assert!(
        took < super::objects::TEARDOWN_WAIT,
        "teardown took {took:?}: it waited for work the driver never had"
    );
    let factory = h.renderer.factory();
    assert_eq!(factory.usage().parked_devices, 0, "nothing was busy");
    assert_eq!(factory.usage().limits.of(Class::HeldSubmits), 0);
    assert_eq!(factory.usage().limits.of(Class::HeldBytes), 0);
    assert_eq!(counts(&h).dropped, 2);
    assert_eq!(
        h.renderer.poll_fence_timelines(0),
        vec![(
            FenceTimeline::Ring {
                ctx_id: CTX,
                ring_idx: 1
            },
            77
        )],
        "the held fence is answered (ADR-0005)"
    );
    h.use_context(2);
    assert!(!h.fatal(), "the other context is untouched");
    assert_driver_never_waited_in_vain(&host);
}

#[test]
fn destroying_the_device_or_resetting_drops_what_it_held() {
    let (mut h, host) = setup();
    h.send(&submit_semaphores(QUEUE, &[CB], &[(T, 1)], &[], true, 0))
        .unwrap();
    assert_eq!(held(&h), 1);
    h.send(&Command::DestroyDevice(DestroyDeviceArgs {
        device: VkDevice(DEVICE),
    }))
    .unwrap();
    assert_eq!(held(&h), 0);
    assert_eq!(
        h.renderer.factory().usage().limits.of(Class::HeldSubmits),
        0
    );
    assert_eq!(host.live("device"), 0);

    let (mut h, host) = setup();
    h.send(&submit_semaphores(QUEUE, &[CB], &[(T, 1)], &[], true, 0))
        .unwrap();
    let started = Instant::now();
    h.renderer.reset();
    assert!(started.elapsed() < super::objects::TEARDOWN_WAIT);
    assert_eq!(
        h.renderer.factory().usage().limits.of(Class::HeldSubmits),
        0
    );
    assert_eq!(host.live_objects(), 0);
    assert_driver_never_waited_in_vain(&host);
}

#[test]
fn a_context_that_goes_fatal_drops_its_holds() {
    let (mut h, host) = setup();
    h.send(&submit_semaphores(QUEUE, &[CB], &[(T, 1)], &[], true, 0))
        .unwrap();
    // Any refused command ends the context.
    assert!(h.send(&signal_semaphore(DEVICE, BIN, 1)).is_err());
    assert_eq!(held(&h), 0);
    assert_eq!(
        h.renderer.factory().usage().limits.of(Class::HeldSubmits),
        0
    );
    assert_driver_never_waited_in_vain(&host);
}

#[test]
fn a_held_submit_naming_what_the_guest_destroyed_ends_the_context_at_release() {
    let (mut h, host) = setup();
    h.send(&create_fence(DEVICE, FENCE, false)).unwrap();
    h.send(&submit_semaphores(
        QUEUE,
        &[CB],
        &[(T, 1)],
        &[],
        true,
        FENCE,
    ))
    .unwrap();
    h.send(&destroy_fence(DEVICE, FENCE)).unwrap();
    assert!(h.send(&signal_semaphore(DEVICE, T, 1)).is_err());
    assert!(h.fatal());
    assert_driver_never_waited_in_vain(&host);
}

// ----------------------------------------------------------------- caps

#[test]
fn held_submits_past_the_cap_end_the_context_and_everything_comes_back() {
    let host = fake();
    let factory = ExecutorFactory::new(Arc::clone(&host)).with_caps(Caps::default().with(
        Class::HeldSubmits,
        3,
        5,
    ));
    let mut h = setup_on(Harness::with_factory(factory));
    for _ in 0..3 {
        h.send(&submit_semaphores(QUEUE, &[CB], &[(T, 1)], &[], true, 0))
            .unwrap();
    }
    assert_eq!(held(&h), 3);
    assert!(
        h.send(&submit_semaphores(QUEUE, &[CB], &[(T, 1)], &[], true, 0))
            .is_err(),
        "a submit has no result to refuse with: the context ends"
    );
    assert!(h.fatal());
    assert_eq!(
        h.renderer.factory().usage().limits.of(Class::HeldSubmits),
        0
    );

    // Another context holds its own share, up to the renderer-wide whole.
    h.use_context(2);
    let mut h = setup_on_context(h);
    for _ in 0..3 {
        h.send(&submit_semaphores(QUEUE, &[CB], &[(T, 1)], &[], true, 0))
            .unwrap();
    }
    assert!(!h.fatal());
    assert_eq!(
        h.renderer.factory().usage().limits.of(Class::HeldSubmits),
        3
    );
    assert_driver_never_waited_in_vain(&host);
}

/// [`setup_on`] for the context the harness now drives.
fn setup_on_context(mut h: Harness<FakeVulkan>) -> Harness<FakeVulkan> {
    let ctx = h.ctx;
    h.use_context(ctx);
    setup_on(h)
}

#[test]
fn held_bytes_are_capped_too() {
    let host = fake();
    let factory = ExecutorFactory::new(Arc::clone(&host)).with_caps(Caps::default().with(
        Class::HeldBytes,
        300,
        1000,
    ));
    let mut h = setup_on(Harness::with_factory(factory));
    let mut refused = false;
    for _ in 0..8 {
        if h.send(&submit_semaphores(QUEUE, &[CB], &[(T, 1)], &[], true, 0))
            .is_err()
        {
            refused = true;
            break;
        }
    }
    assert!(refused, "300 bytes hold only a few submits");
    assert!(h.fatal());
    assert_eq!(h.renderer.factory().usage().limits.of(Class::HeldBytes), 0);
}

// ---------------------------------------------------------------- events

fn create_event(id: u64) -> Command<'static> {
    Command::CreateEvent(CreateEventArgs {
        device: VkDevice(DEVICE),
        p_create_info: Some(VkEventCreateInfo::default()),
        p_event: Some(VkEvent(id)),
        ret: 0,
    })
}

fn host_set(id: u64) -> Command<'static> {
    Command::SetEvent(SetEventArgs {
        device: VkDevice(DEVICE),
        event: VkEvent(id),
        ret: 0,
    })
}

fn host_reset(id: u64) -> Command<'static> {
    Command::ResetEvent(ResetEventArgs {
        device: VkDevice(DEVICE),
        event: VkEvent(id),
        ret: 0,
    })
}

fn cmd_set(cb: u64, id: u64) -> Command<'static> {
    Command::CmdSetEvent(CmdSetEventArgs {
        command_buffer: VkCommandBuffer(cb),
        event: VkEvent(id),
        stage_mask: STAGE_TRANSFER,
    })
}

fn cmd_reset(cb: u64, id: u64) -> Command<'static> {
    Command::CmdResetEvent(CmdResetEventArgs {
        command_buffer: VkCommandBuffer(cb),
        event: VkEvent(id),
        stage_mask: STAGE_TRANSFER,
    })
}

fn cmd_wait(cb: u64, id: u64) -> Command<'static> {
    Command::CmdWaitEvents(CmdWaitEventsArgs {
        command_buffer: VkCommandBuffer(cb),
        event_count: 1,
        p_events: Some(vec![VkEvent(id)]),
        src_stage_mask: STAGE_TRANSFER | 0x4000,
        dst_stage_mask: STAGE_TRANSFER,
        memory_barrier_count: 0,
        p_memory_barriers: None,
        buffer_memory_barrier_count: 0,
        p_buffer_memory_barriers: None,
        image_memory_barrier_count: 0,
        p_image_memory_barriers: None,
    })
}

#[test]
fn a_gpu_wait_on_an_event_only_the_host_sets_is_held_until_vk_set_event() {
    let (mut h, host) = setup();
    h.send(&create_event(EVENT)).unwrap();
    record(&mut h, CB, &[cmd_wait(CB, EVENT)]);
    h.send(&queue_submit(QUEUE, &[CB], 0)).unwrap();
    assert_eq!(held(&h), 1, "nothing has set the event");
    h.send(&host_set(EVENT)).unwrap();
    assert_eq!(held(&h), 0, "the host's set released it");
    let event = raw(&h, super::objects::Kind::Event, EVENT);
    assert!(host.event_set(event));
    // Set already: the next such submit goes at once.
    h.send(&queue_submit(QUEUE, &[CB], 0)).unwrap();
    assert_eq!(held(&h), 0);
    assert_driver_never_waited_in_vain(&host);
}

#[test]
fn events_set_on_the_gpu_cover_their_waits_and_a_split_barrier_is_never_held() {
    let (mut h, host) = setup();
    h.send(&create_event(EVENT)).unwrap();
    // A split barrier inside one command buffer.
    record(&mut h, CB, &[cmd_set(CB, EVENT), cmd_wait(CB, EVENT)]);
    h.send(&queue_submit(QUEUE, &[CB], 0)).unwrap();
    assert_eq!(held(&h), 0);
    // Set by a submitted command buffer, waited on by the next.
    h.send(&create_event(EVENT + 1)).unwrap();
    record(&mut h, CB2, &[cmd_set(CB2, EVENT + 1)]);
    record(&mut h, CB3, &[cmd_wait(CB3, EVENT + 1)]);
    h.send(&queue_submit(QUEUE, &[CB2], 0)).unwrap();
    h.send(&queue_submit(QUEUE, &[CB3], 0)).unwrap();
    assert_eq!(held(&h), 0);
    // Reset by a submitted buffer: the next wait is held until the host sets.
    record(&mut h, CB2, &[cmd_reset(CB2, EVENT + 1)]);
    h.send(&queue_submit(QUEUE, &[CB2], 0)).unwrap();
    h.send(&queue_submit(QUEUE, &[CB3], 0)).unwrap();
    assert_eq!(held(&h), 1);
    h.send(&host_set(EVENT + 1)).unwrap();
    assert_eq!(held(&h), 0);
    assert!(!h.fatal());
    assert_driver_never_waited_in_vain(&host);
}

#[test]
fn a_wait_on_an_event_its_own_buffer_reset_ends_the_context_before_the_driver_sees_it() {
    let (mut h, host) = setup();
    h.send(&create_event(EVENT)).unwrap();
    let waits = host.called("vkCmdWaitEvents");
    let outcome = h.submit_recording(&[
        begin(CB),
        cmd_reset(CB, EVENT),
        cmd_wait(CB, EVENT),
        end(CB),
    ]);
    assert!(matches!(outcome, Outcome::Fatal { .. }));
    assert_eq!(host.called("vkCmdWaitEvents"), waits);
}

#[test]
fn a_host_reset_waits_for_submitted_work_that_waits_on_the_event() {
    let (mut h, host) = setup();
    h.send(&create_event(EVENT)).unwrap();
    h.send(&host_set(EVENT)).unwrap();
    record(&mut h, CB, &[cmd_wait(CB, EVENT)]);
    host.hold.store(true, Ordering::SeqCst);
    h.send(&create_fence(DEVICE, FENCE, false)).unwrap();
    h.send(&queue_submit(QUEUE, &[CB], FENCE)).unwrap();
    assert_eq!(held(&h), 0, "the event was set");
    assert_eq!(host.held(), 1, "the fake GPU has not run it yet");
    // The reset may not overtake the wait: the executor waits first.
    h.send(&host_reset(EVENT)).unwrap();
    assert_eq!(host.held(), 0, "the work was waited for before the reset");
    let calls = host.calls();
    let reset = calls.iter().rposition(|c| c == "vkResetEvent").unwrap();
    let waited = calls.iter().rposition(|c| c == "vkWaitForFences").unwrap();
    assert!(waited < reset);
    assert_driver_never_waited_in_vain(&host);
}

// --------------------------------------------------------------- queries

#[test]
fn a_query_result_wait_is_polled_off_the_lock_never_waited_in_the_driver() {
    let (mut h, host) = setup();
    h.send(&Command::CreateQueryPool(CreateQueryPoolArgs {
        device: VkDevice(DEVICE),
        p_create_info: Some(VkQueryPoolCreateInfo {
            flags: 0,
            query_type: 0,
            query_count: 4,
            pipeline_statistics: 0,
        }),
        p_query_pool: Some(VkQueryPool(0x3f0)),
        ret: 0,
    }))
    .unwrap();
    host.queries_not_ready.store(true, Ordering::SeqCst);
    let flip = {
        let host = Arc::clone(&host);
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(80));
            host.queries_not_ready.store(false, Ordering::SeqCst);
        })
    };
    let started = Instant::now();
    let Command::GetQueryPoolResults(r) = h
        .call(&Command::GetQueryPoolResults(GetQueryPoolResultsArgs {
            device: VkDevice(DEVICE),
            query_pool: VkQueryPool(0x3f0),
            first_query: 0,
            query_count: 4,
            data_size: 32,
            p_data: Some(vec![0; 32]),
            stride: 8,
            flags: 0x1 | super::submit::QUERY_RESULT_WAIT,
            ret: 0,
        }))
        .unwrap()
    else {
        panic!()
    };
    flip.join().unwrap();
    assert_eq!(r.ret, VK_SUCCESS);
    assert!(started.elapsed() >= Duration::from_millis(70), "it waited");
    let flags = host.query_flags();
    assert!(flags.len() > 1, "polled: {flags:?}");
    assert!(
        flags
            .iter()
            .all(|f| f & super::submit::QUERY_RESULT_WAIT == 0),
        "the driver never saw WAIT: {flags:?}"
    );
    assert!(!h.fatal());
    assert_driver_never_waited_in_vain(&host);
}

// ------------------------------------------------------------------ cost

/// With nothing held, a submit's cover check is a lookup per wait: measured
/// here on the fake host, as the submit path's whole overhead.
#[test]
fn the_cover_check_costs_microseconds_on_the_submit_path() {
    let (mut h, _host) = setup();
    h.send(&submit_semaphores(QUEUE, &[CB], &[], &[(T, 1)], true, 0))
        .unwrap();
    let ops = super::hold::sync_ops(&submit_semaphores(
        QUEUE,
        &[CB, CB2, CB3],
        &[(T, 1), (T2, 0)],
        &[(T, 2)],
        true,
        0,
    ));
    let (n, took) = h
        .renderer
        .factory()
        .with_context(CTX, |c| {
            let start = Instant::now();
            let mut n = 0;
            for _ in 0..10_000 {
                n += usize::from(c.covered(DEVICE, &ops));
            }
            (n, start.elapsed())
        })
        .unwrap();
    assert_eq!(n, 10_000);
    eprintln!(
        "cover check: {:?} per submit of 2 waits and 3 buffers",
        took / 10_000
    );
    assert!(took / 10_000 < Duration::from_micros(50));
}
