//! Mesa's roundtrip: `vkSubmitVirtqueueSeqnoMESA` on the context stream,
//! `vkWaitVirtqueueSeqnoMESA` in the ring (the module docs' "Roundtrips").
//! Found by GNOME composited on the GPU, whose zink clients import dma-bufs:
//! the executor refused the wait and the context died.

use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::renderer::Renderer3d;
use crate::venus::pump::{STATUS_FATAL, STATUS_IDLE};

use super::fake::FakeVulkan;
use super::harness::*;
use super::recording::{execute_streams, STREAM_RES};

fn booted() -> Harness<FakeVulkan> {
    let host = Arc::new(FakeVulkan::standard());
    let mut h = Harness::new(host);
    boot(&mut h);
    h
}

/// A cheap ring command with no reply, to stand for "whatever came after the
/// wait": `vkSeekReplyCommandStreamMESA` to the start of the bound window.
fn after() -> Vec<u8> {
    seek_reply(0)
}

/// Give a blocked ring every chance to run past its wait, then check it did
/// not: `head` still at the wait, nothing published but what a live ring
/// publishes.
fn assert_still_blocked(h: &Harness<FakeVulkan>, at: u32) {
    std::thread::sleep(Duration::from_millis(150));
    assert_eq!(h.head(), at, "the ring ran past its wait");
    assert_eq!(h.status() & (STATUS_FATAL | STATUS_IDLE), 0);
}

/// Wait until `head` reaches `head`, and say how long that took.
fn released(h: &Harness<FakeVulkan>, head: u32) -> Duration {
    let start = Instant::now();
    h.wait_head(head);
    start.elapsed()
}

#[test]
fn a_submit_on_the_context_stream_then_a_wait_in_the_ring_goes_straight_through() {
    let mut h = booted();
    h.submit_virtqueue_seqno(1).expect("recorded");
    assert_eq!(h.submit(&wait_virtqueue_seqno_bytes(1)), Outcome::Consumed);
    // And the ring goes on serving calls after it.
    h.call(&enumerate_instance_version()).expect("still alive");
    // A seqno below what was recorded, and zero (nothing yet waited for),
    // pass too.
    assert_eq!(h.submit(&wait_virtqueue_seqno_bytes(0)), Outcome::Consumed);
    assert!(!h.fatal());
}

#[test]
fn a_wait_sent_before_its_submit_blocks_the_ring_until_the_submit_arrives() {
    let mut h = booted();
    let at = h.tail();
    let mut bytes = wait_virtqueue_seqno_bytes(5);
    bytes.extend_from_slice(&after());
    h.produce(&bytes);
    let end = h.tail();
    assert_still_blocked(&h, at);
    // A doorbell is not a submit, and a submit short of the value is not it.
    h.produce(&[]);
    h.submit_virtqueue_seqno(4).expect("recorded");
    assert_still_blocked(&h, at);
    // The one it waits for releases it at once — the renderer rings the
    // worker's doorbell — not at the end of a poll.
    h.submit_virtqueue_seqno(5).expect("recorded");
    assert!(released(&h, end) < Duration::from_secs(2));
    assert!(!h.fatal());
    h.call(&enumerate_instance_version()).expect("still alive");
}

#[test]
fn the_seqno_is_a_plain_64_bit_count_as_in_vkr_and_a_submit_overwrites_it() {
    let mut h = booted();
    // vkr compares `virtqueue_seqno < seqno` as u64, and so do we: a value
    // past 2^32 is not folded to its low half (which would read 1 < 2 and
    // block this wait), and one past it is still ahead.
    h.submit_virtqueue_seqno(0x1_0000_0001).expect("recorded");
    assert_eq!(h.submit(&wait_virtqueue_seqno_bytes(2)), Outcome::Consumed);
    let at = h.tail();
    h.produce(&wait_virtqueue_seqno_bytes(0x1_0000_0002));
    assert_still_blocked(&h, at);
    h.submit_virtqueue_seqno(u64::MAX).expect("recorded");
    h.wait_head(h.tail());
    assert_eq!(
        h.submit(&wait_virtqueue_seqno_bytes(u64::MAX)),
        Outcome::Consumed
    );
    // A submit replaces the value rather than raising it
    // (`vkr_ring_submit_virtqueue_seqno`): Mesa's counter only grows, so the
    // two agree for a real guest, and a guest that goes backwards waits.
    h.submit_virtqueue_seqno(3).expect("recorded");
    let at = h.tail();
    h.produce(&wait_virtqueue_seqno_bytes(4));
    assert_still_blocked(&h, at);
    h.submit_virtqueue_seqno(4).expect("recorded");
    released(&h, h.tail());
    assert!(!h.fatal());
}

#[test]
fn a_ring_blocked_in_a_wait_is_torn_down_at_once() {
    let mut h = booted();
    let at = h.tail();
    h.produce(&wait_virtqueue_seqno_bytes(9));
    assert_still_blocked(&h, at);
    let start = Instant::now();
    h.renderer.ctx_destroy(CTX);
    assert!(start.elapsed() < Duration::from_secs(1), "joined at once");
    assert_eq!(h.renderer.live_threads(), 0);

    // And by a device reset.
    let mut h = booted();
    let at = h.tail();
    h.produce(&wait_virtqueue_seqno_bytes(9));
    assert_still_blocked(&h, at);
    let start = Instant::now();
    h.renderer.reset();
    assert!(start.elapsed() < Duration::from_secs(1), "joined at once");
    assert_eq!(h.renderer.live_threads(), 0);
}

#[test]
fn a_blocked_ring_holds_no_pass_so_a_pause_settles_and_a_paused_reset_joins() {
    let host = Arc::new(FakeVulkan::standard());
    let quiesce = virtio_core::Quiesce::new();
    let mut h = Harness::gated(Arc::clone(&host), Arc::clone(&quiesce));
    boot(&mut h);
    let at = h.tail();
    let mut bytes = wait_virtqueue_seqno_bytes(1);
    bytes.extend_from_slice(&after());
    h.produce(&bytes);
    let end = h.tail();
    assert_still_blocked(&h, at);
    // The submit it waits for is behind the (gated) device worker; the
    // pause must not wait for it.
    quiesce.pause();
    assert!(
        quiesce.wait_until_idle(Duration::from_secs(1)),
        "the blocked ring held a pass"
    );
    // The submit is recorded while paused; the ring, parked at the gate,
    // runs nothing until the VM resumes.
    h.submit_virtqueue_seqno(1).expect("recorded");
    assert_still_blocked(&h, at);
    quiesce.resume();
    assert!(released(&h, end) < Duration::from_secs(2));

    // Blocked again, paused, reset: the reset joins the worker at the gate.
    let at = h.tail();
    h.produce(&wait_virtqueue_seqno_bytes(2));
    assert_still_blocked(&h, at);
    quiesce.pause();
    assert!(quiesce.wait_until_idle(Duration::from_secs(1)));
    let start = Instant::now();
    h.renderer.reset();
    assert!(
        start.elapsed() < Duration::from_secs(1),
        "a paused reset joins"
    );
    assert_eq!(h.renderer.live_threads(), 0);
    quiesce.resume();
}

#[test]
fn the_monitor_keeps_alive_up_while_a_ring_is_blocked() {
    let host = Arc::new(FakeVulkan::standard());
    // Mesa asks for 3 s; a test cannot wait that long, and the period is
    // the guest's to choose.
    let mut h = Harness::monitored(host, 20_000);
    boot(&mut h);
    let at = h.tail();
    h.produce(&wait_virtqueue_seqno_bytes(1));
    assert_still_blocked(&h, at);
    // The guest's watchdog arms by clearing ALIVE; the host sets it again.
    h.clear_alive();
    let deadline = Instant::now() + Duration::from_secs(2);
    while !h.alive() {
        assert!(Instant::now() < deadline, "ALIVE never came back");
        std::thread::sleep(Duration::from_millis(5));
    }
    h.submit_virtqueue_seqno(1).expect("recorded");
    released(&h, h.tail());
}

#[test]
fn a_wait_inside_a_command_stream_is_waited_for_in_place() {
    let mut h = booted();
    let pages = h.stream_blob();
    let mut stream = wait_virtqueue_seqno_bytes(3);
    stream.extend_from_slice(&after());
    pages.write_bytes(0, &stream).expect("fits the blob");
    let at = h.tail();
    h.produce(&execute_streams(&[(STREAM_RES, 0, stream.len() as u64)]));
    let end = h.tail();
    assert_still_blocked(&h, at);
    h.submit_virtqueue_seqno(3).expect("recorded");
    assert!(released(&h, end) < Duration::from_secs(2));
    assert!(!h.fatal());

    // And a teardown in the middle of one still joins.
    let mut stream = wait_virtqueue_seqno_bytes(4);
    stream.extend_from_slice(&after());
    pages.write_bytes(0, &stream).expect("fits the blob");
    let at = h.tail();
    h.produce(&execute_streams(&[(STREAM_RES, 0, stream.len() as u64)]));
    assert_still_blocked(&h, at);
    let start = Instant::now();
    h.renderer.ctx_destroy(CTX);
    assert!(start.elapsed() < Duration::from_secs(1));
    assert_eq!(h.renderer.live_threads(), 0);
}

#[test]
fn the_wait_stays_refused_on_the_context_stream_and_the_submit_in_a_ring() {
    let mut h = booted();
    // On the context stream it would block the worker that has to deliver
    // the very submit it waits for (`vkr_transport.c`).
    assert!(h
        .renderer
        .submit(CTX, &wait_virtqueue_seqno_bytes(1))
        .is_err());
    // A submit must name a ring of this context.
    assert!(h
        .renderer
        .submit(CTX, &submit_virtqueue_seqno_bytes(0xdead, 1))
        .is_err());
    // Neither refusal cost the context anything.
    assert_eq!(h.submit(&wait_virtqueue_seqno_bytes(0)), Outcome::Consumed);
    // In a ring the submit is refused as vkr refuses it: fatal.
    let at = h.tail();
    assert_eq!(
        h.submit(&submit_virtqueue_seqno_bytes(RING, 1)),
        Outcome::Fatal { head: at }
    );
}
