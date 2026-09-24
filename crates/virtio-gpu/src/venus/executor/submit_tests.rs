//! Stage 5b.2 against the fake host: recording, submission, fences and waits
//! as Mesa 26.0.8 sends them (fence feedback included),
//! `vkExecuteCommandStreamsMESA`, a lost device, and nothing freed or torn
//! down under work the GPU may still be doing.

use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};

use super::fake::FakeVulkan;
use super::harness::*;
use super::objects::Kind;
use super::recording::*;
use crate::renderer::Renderer3d;
use crate::venus::protocol::*;
use crate::venus::transport::Opcode;
use crate::venus::wire::{CommandHeader, Decoder, Encoder};

const BUF: u64 = 0x200;
const FB_CB: u64 = CB + 1;

fn setup() -> (Harness<FakeVulkan>, Arc<FakeVulkan>) {
    let host = Arc::new(FakeVulkan::standard());
    let mut h = Harness::new(Arc::clone(&host));
    with_device(&mut h);
    h.send(&allocate_cbs(DEVICE, POOL, &[CB], false)).unwrap();
    (h, host)
}

/// A 64 KiB transfer buffer bound to device-local memory.
fn buffer(h: &mut Harness<FakeVulkan>, id: u64) {
    let Command::CreateBuffer(b) = h
        .call(&create_buffer(DEVICE, id, buffer_info(64 << 10, TRANSFER)))
        .unwrap()
    else {
        panic!()
    };
    assert_eq!(b.ret, VK_SUCCESS);
    h.send(&allocate(DEVICE, id | 0x1000, 64 << 10, 0, Vec::new()))
        .unwrap();
    h.send(&bind_buffers(DEVICE, &[(id, id | 0x1000, 0)]))
        .unwrap();
}

/// Mesa's recording of one command buffer that fills `buffer`.
fn record_fill(h: &mut Harness<FakeVulkan>, cb: u64, buffer: u64) {
    let outcome = h.submit_recording(&[
        begin(cb),
        fill(cb, buffer, 0, WHOLE_SIZE, 0x1111_1111),
        buffer_barrier(
            cb,
            buffer,
            (STAGE_TRANSFER, ACCESS_TRANSFER_WRITE),
            (STAGE_HOST, ACCESS_HOST_READ),
        ),
        end(cb),
    ]);
    assert_eq!(outcome, Outcome::Consumed);
}

/// The host handle guest object `id` of `kind` is bound to.
fn raw(h: &Harness<FakeVulkan>, kind: Kind, id: u64) -> u64 {
    h.renderer
        .factory()
        .with_context(CTX, |ctx| ctx.objects.raw_any(kind, id).map(|o| o.host))
        .expect("the context")
        .expect("the object")
}

/// Many commands without replies, as few ring submissions as fit.
fn send_all(h: &mut Harness<FakeVulkan>, commands: &[Command<'_>]) {
    let mut batch = Vec::new();
    for command in commands {
        let bytes = async_bytes(command);
        if batch.len() + bytes.len() > 32 << 10 {
            assert_eq!(h.submit(&batch), Outcome::Consumed);
            batch.clear();
        }
        batch.extend_from_slice(&bytes);
    }
    if !batch.is_empty() {
        assert_eq!(h.submit(&batch), Outcome::Consumed);
    }
}

fn index_of(calls: &[String], what: &str) -> usize {
    calls
        .iter()
        .position(|c| c == what)
        .unwrap_or_else(|| panic!("{what} never reached the host: {calls:?}"))
}

// ------------------------------------------------------------ recording

#[test]
fn a_recording_submitted_with_a_fence_is_translated_and_waited_for_as_mesa_does() {
    let (mut h, host) = setup();
    buffer(&mut h, BUF);
    record_fill(&mut h, CB, BUF);
    h.send(&create_fence(DEVICE, FENCE, false)).unwrap();
    h.send(&queue_submit(QUEUE, &[CB], FENCE)).unwrap();
    // Mesa's poll found the feedback slot signalled: an async wait.
    h.send(&wait_fences(DEVICE, &[FENCE], u64::MAX)).unwrap();
    let Command::GetFenceStatus(s) = h.call(&fence_status(DEVICE, FENCE)).unwrap() else {
        panic!()
    };
    assert_eq!(s.ret, VK_SUCCESS);
    assert!(!h.fatal());

    let calls = host.calls();
    let order = [
        "vkAllocateCommandBuffers",
        "vkBeginCommandBuffer",
        "vkCmdFillBuffer",
        "vkCmdPipelineBarrier",
        "vkEndCommandBuffer",
        "vkCreateFence",
        "vkQueueSubmit",
        "vkWaitForFences",
        "vkGetFenceStatus",
    ];
    let at: Vec<usize> = order.iter().map(|c| index_of(&calls, c)).collect();
    assert!(at.windows(2).all(|w| w[0] < w[1]), "in order: {calls:?}");
    // The host saw its own handle for the command buffer, never the guest's
    // id.
    let cb = raw(&h, Kind::CommandBuffer, CB);
    assert_ne!(cb, CB);
    assert_eq!(host.submitted(), vec![vec![cb]]);
}

/// Mesa 26.0.8's fence feedback, as `vn_queue.c` and `vn_feedback.c` do it:
/// the fence's own command buffer (recorded once when the fence is created)
/// appended to the batch, resubmitted unchanged after a reset.
#[test]
fn mesa_fence_feedback_rides_the_same_submit_as_just_another_command_buffer() {
    let (mut h, host) = setup();
    buffer(&mut h, BUF);
    buffer(&mut h, BUF + 1); // the feedback pool's buffer
    record_fill(&mut h, CB, BUF);
    // vn_CreateFence: the feedback command buffer, then the fence.
    h.send(&allocate_cbs(DEVICE, POOL, &[FB_CB], false))
        .unwrap();
    let outcome = h.submit_recording(&[
        Command::BeginCommandBuffer(BeginCommandBufferArgs {
            command_buffer: VkCommandBuffer(FB_CB),
            p_begin_info: Some(VkCommandBufferBeginInfo::default()),
            ret: 0,
        }),
        buffer_barrier(
            FB_CB,
            BUF + 1,
            (STAGE_ALL_COMMANDS, 0),
            (STAGE_TRANSFER, ACCESS_TRANSFER_WRITE),
        ),
        fill(FB_CB, BUF + 1, 8, 4, 0),
        buffer_barrier(
            FB_CB,
            BUF + 1,
            (STAGE_TRANSFER, ACCESS_TRANSFER_WRITE),
            (STAGE_HOST, ACCESS_HOST_READ),
        ),
        end(FB_CB),
    ]);
    assert_eq!(outcome, Outcome::Consumed);
    h.send(&create_fence(DEVICE, FENCE, false)).unwrap();
    for round in 0..3 {
        if round > 0 {
            record_fill(&mut h, CB, BUF);
            h.send(&reset_fences(DEVICE, &[FENCE])).unwrap();
        }
        h.send(&queue_submit(QUEUE, &[CB, FB_CB], FENCE)).unwrap();
        h.send(&wait_fences(DEVICE, &[FENCE], u64::MAX)).unwrap();
    }
    // vn_DestroyFence: the fence, then its feedback command buffer.
    h.send(&destroy_fence(DEVICE, FENCE)).unwrap();
    h.send(&Command::FreeCommandBuffers(FreeCommandBuffersArgs {
        device: VkDevice(DEVICE),
        command_pool: VkCommandPool(POOL),
        command_buffer_count: 1,
        p_command_buffers: Some(vec![VkCommandBuffer(FB_CB)]),
    }))
    .unwrap();
    assert!(!h.fatal());
    let cb = raw(&h, Kind::CommandBuffer, CB);
    let submitted = host.submitted();
    assert_eq!(submitted.len(), 3);
    assert!(submitted.iter().all(|s| s.len() == 2 && s[0] == cb));
    assert!(
        submitted.windows(2).all(|w| w[0][1] == w[1][1]),
        "the same feedback buffer"
    );
    assert_eq!(host.called("destroy VkFence"), 1);
    assert_eq!(host.called("vkFreeCommandBuffers"), 1);
}

#[test]
fn a_thousand_submits_each_with_its_own_fence_all_signal() {
    const N: u64 = 1000;
    let (mut h, host) = setup();
    buffer(&mut h, BUF);
    let cbs: Vec<u64> = (0..N).map(|i| 0x1_0000 + i).collect();
    let fences: Vec<u64> = (0..N).map(|i| 0x2_0000 + i).collect();
    h.send(&allocate_cbs(DEVICE, POOL, &cbs, false)).unwrap();
    let mut recording = Vec::new();
    for (i, cb) in cbs.iter().enumerate() {
        recording.push(begin(*cb));
        recording.push(fill(*cb, BUF, i as u64 * 4, 4, i as u32));
        recording.push(end(*cb));
    }
    send_all(&mut h, &recording);
    send_all(
        &mut h,
        &fences
            .iter()
            .map(|f| create_fence(DEVICE, *f, false))
            .collect::<Vec<_>>(),
    );
    send_all(
        &mut h,
        &cbs.iter()
            .zip(&fences)
            .map(|(c, f)| queue_submit(QUEUE, &[*c], *f))
            .collect::<Vec<_>>(),
    );
    send_all(
        &mut h,
        &fences
            .iter()
            .map(|f| wait_fences(DEVICE, &[*f], u64::MAX))
            .collect::<Vec<_>>(),
    );
    assert!(!h.fatal());
    assert_eq!(host.submitted().len(), N as usize);
    assert_eq!(host.called("vkWaitForFences"), N as usize);
    for f in fences.iter().step_by(97) {
        let Command::GetFenceStatus(s) = h.call(&fence_status(DEVICE, *f)).unwrap() else {
            panic!()
        };
        assert_eq!(s.ret, VK_SUCCESS);
    }
}

// ---------------------------------------------------- command streams

fn execute_streams_with(ranges: &[(u32, u64, u64)], dependencies: &[(u32, u32)]) -> Vec<u8> {
    let mut enc = Encoder::new();
    enc.command_header(CommandHeader {
        opcode: Opcode::ExecuteCommandStreams.as_u32(),
        flags: 0,
    })
    .unwrap();
    enc.u32(ranges.len() as u32).unwrap();
    enc.array_size(ranges.len() as u64).unwrap();
    for (resource, offset, size) in ranges {
        enc.u32(*resource).unwrap();
        enc.size(*offset).unwrap();
        enc.size(*size).unwrap();
    }
    enc.array_size(0).unwrap();
    enc.u32(dependencies.len() as u32).unwrap();
    enc.array_size(dependencies.len() as u64).unwrap();
    for (src, dst) in dependencies {
        enc.u32(*src).unwrap();
        enc.u32(*dst).unwrap();
    }
    enc.flags(0).unwrap();
    enc.finish().unwrap()
}

#[test]
fn a_stream_runs_like_the_ring_and_its_reply_lands_in_the_ring_window() {
    let (mut h, _host) = setup();
    // A large sync call, the way `vn_ring_submit_command` routes one
    // through `ring->upload`: the reply window set on the ring, the command
    // with its reply flag in the stream.
    let at = 0x8_0000;
    assert_eq!(
        h.submit(&set_reply(REPLY_RES, at, WINDOW)),
        Outcome::Consumed
    );
    let command = enumerate_instance_version();
    assert_eq!(
        h.submit_indirect(&[&call_bytes(&command)]),
        Outcome::Consumed
    );
    let mut reply = vec![0u8; WINDOW as usize];
    h.reply.read_bytes(at, &mut reply).unwrap();
    let bytes = call_bytes(&command);
    let mut out = Command::decode_next(&mut Decoder::new(&bytes)).unwrap().1;
    out.decode_reply(&mut Decoder::new(&reply)).unwrap();
    let Command::EnumerateInstanceVersion(v) = out else {
        panic!()
    };
    assert_eq!(v.ret, VK_SUCCESS);
    assert!(v.p_api_version.unwrap() > 0);
    assert!(!h.fatal());
}

#[test]
fn a_recording_past_the_direct_size_goes_through_one_stream_and_runs_whole() {
    let (mut h, host) = setup();
    buffer(&mut h, BUF);
    let mut recording = vec![begin(CB)];
    for i in 0..400u64 {
        recording.push(fill(CB, BUF, i * 16, 16, i as u32));
    }
    recording.push(end(CB));
    let bytes: usize = recording.iter().map(|c| async_bytes(c).len()).sum();
    assert!(bytes > DIRECT_SIZE);
    assert_eq!(h.submit_recording(&recording), Outcome::Consumed);
    assert_eq!(host.called("vkCmdFillBuffer"), 400);
    assert_eq!(host.called("vkEndCommandBuffer"), 1);
    assert!(!h.fatal());
}

#[test]
fn a_stream_outside_its_blob_is_fatal_and_runs_nothing() {
    let (mut h, host) = setup();
    h.stream_blob();
    let before = host.calls().len();
    let start = h.tail();
    let outcome = h.submit(&execute_streams(&[(STREAM_RES, STREAM_BYTES - 16, 64)]));
    assert_eq!(outcome, Outcome::Fatal { head: start });
    assert_eq!(host.calls().len(), before);
    assert_eq!(h.renderer.factory().context_fatal(CTX), Some(true));
}

#[test]
fn a_stream_of_a_blob_that_does_not_exist_is_fatal() {
    let (mut h, _host) = setup();
    let start = h.tail();
    assert_eq!(
        h.submit(&execute_streams(&[(0x77, 0, 64)])),
        Outcome::Fatal { head: start }
    );
}

#[test]
fn a_stream_inside_a_stream_is_refused_as_nested() {
    let (mut h, host) = setup();
    let before = host.calls().len();
    // The inner call names a perfectly good (empty) range: only the nesting
    // is wrong.
    let inner = execute_streams(&[(STREAM_RES, 0x1000, 0)]);
    let start = h.tail();
    assert_eq!(h.submit_indirect(&[&inner]), Outcome::Fatal { head: start });
    assert_eq!(host.calls().len(), before);
}

#[test]
fn a_stream_in_another_contexts_blob_is_refused() {
    let (mut h, host) = setup();
    h.renderer
        .ctx_create(2, crate::CAPSET_VENUS, "venus")
        .expect("a second context");
    h.create_blob(2, 30, 1 << 16);
    // A perfectly good command, in a blob this context does not own.
    let pages = h.renderer.blob_pages(30).unwrap();
    let command = async_bytes(&create_fence(DEVICE, FENCE, false));
    pages.write_bytes(0, &command).unwrap();
    let start = h.tail();
    assert_eq!(
        h.submit(&execute_streams(&[(30, 0, command.len() as u64)])),
        Outcome::Fatal { head: start }
    );
    assert_eq!(host.called("vkCreateFence"), 0);
}

#[test]
fn a_stream_the_guest_rewrites_while_it_runs_changes_nothing_after_the_copy() {
    let (mut h, host) = setup();
    let pages = h.stream_blob();
    let stream: Vec<u8> = [
        create_fence(DEVICE, FENCE, false),
        create_fence(DEVICE, FENCE + 1, true),
    ]
    .iter()
    .flat_map(|c| async_bytes(c))
    .collect();
    // While the first command of the stream is at the host, the guest
    // scribbles over the whole blob: had the executor decoded in place (as
    // vkr does), the second command would be garbage.
    let scribble = Arc::clone(&pages);
    let armed = std::sync::atomic::AtomicBool::new(true);
    *host.on_call.lock().unwrap() = Some(Box::new(move |name| {
        if name == "vkCreateFence" && armed.swap(false, Ordering::SeqCst) {
            scribble.write_bytes(0, &vec![0xff; 4096]).unwrap();
        }
    }));
    assert_eq!(h.submit_indirect(&[&stream]), Outcome::Consumed);
    assert_eq!(
        host.called("vkCreateFence"),
        2,
        "both fences, from the copy"
    );
    assert!(!h.fatal());
    let mut now = vec![0u8; 16];
    pages.read_bytes(0, &mut now).unwrap();
    assert_eq!(now, vec![0xff; 16], "the guest's bytes did change");
}

#[test]
fn a_stream_that_ends_inside_a_command_is_fatal() {
    let (mut h, host) = setup();
    let command = async_bytes(&create_fence(DEVICE, FENCE, false));
    let start = h.tail();
    assert_eq!(
        h.submit_indirect(&[&command[..command.len() - 4]]),
        Outcome::Fatal { head: start }
    );
    assert_eq!(host.called("vkCreateFence"), 0);
}

#[test]
fn stream_dependencies_must_point_forward_and_the_bytes_are_bounded() {
    let (mut h, _host) = setup();
    h.stream_blob();
    let start = h.tail();
    assert_eq!(
        h.submit(&execute_streams_with(
            &[(STREAM_RES, 0, 0), (STREAM_RES, 0, 0)],
            &[(1, 0)]
        )),
        Outcome::Fatal { head: start }
    );

    let (mut h, host) = setup();
    h.stream_blob();
    // 17 x 4 MiB is past the 64 MiB one call may name: refused before any
    // byte is copied.
    let ranges = vec![(STREAM_RES, 0, STREAM_BYTES); 17];
    let start = h.tail();
    assert_eq!(
        h.submit(&execute_streams(&ranges)),
        Outcome::Fatal { head: start }
    );
    assert!(host.calls().iter().all(|c| !c.starts_with("vkCmd")));

    // Forward dependencies and empty streams are fine.
    let (mut h, _host) = setup();
    h.stream_blob();
    assert_eq!(
        h.submit(&execute_streams_with(
            &[(STREAM_RES, 0, 0), (STREAM_RES, 0, 0)],
            &[(0, 1)]
        )),
        Outcome::Consumed
    );
}

// ---------------------------------------------------------- lost device

#[test]
fn a_lost_device_answers_the_command_then_ends_the_context_and_nothing_else() {
    let (mut h, host) = setup();
    buffer(&mut h, BUF);
    record_fill(&mut h, CB, BUF);
    h.send(&create_fence(DEVICE, FENCE, false)).unwrap();
    host.lost.store(true, Ordering::SeqCst);
    // The guest is told, then the ring and the context end: the command
    // that met the loss is answered and consumed, and FATAL is up.
    let at = 0x8_0000;
    assert_eq!(
        h.submit(&set_reply(REPLY_RES, at, WINDOW)),
        Outcome::Consumed
    );
    let command = fence_status(DEVICE, FENCE);
    let bytes = call_bytes(&command);
    let past = h.tail().wrapping_add(bytes.len() as u32);
    assert_eq!(h.submit(&bytes), Outcome::Fatal { head: past });
    let mut reply = vec![0u8; WINDOW as usize];
    h.reply.read_bytes(at, &mut reply).unwrap();
    let mut out = Command::decode_next(&mut Decoder::new(&bytes)).unwrap().1;
    out.decode_reply(&mut Decoder::new(&reply)).unwrap();
    let Command::GetFenceStatus(s) = out else {
        panic!()
    };
    assert_eq!(s.ret, VK_ERROR_DEVICE_LOST, "the guest was told");
    assert_eq!(h.renderer.factory().context_fatal(CTX), Some(true));
    // And the host objects go as ever: a lost device may be torn down.
    h.renderer.reset();
    assert_eq!(host.live_objects(), 0);
}

#[test]
fn a_submit_to_a_lost_device_is_consumed_then_the_ring_is_fatal() {
    let (mut h, host) = setup();
    buffer(&mut h, BUF);
    record_fill(&mut h, CB, BUF);
    host.lost.store(true, Ordering::SeqCst);
    let before = h.tail();
    let bytes = async_bytes(&queue_submit(QUEUE, &[CB], 0));
    assert_eq!(
        h.submit(&bytes),
        Outcome::Fatal {
            head: before + bytes.len() as u32
        }
    );
    assert_eq!(host.called("vkQueueSubmit"), 1);
}

// ------------------------------------------- nothing freed under the GPU

#[test]
fn destroying_what_a_fenced_submission_may_use_waits_for_its_fence_first() {
    let (mut h, host) = setup();
    host.hold.store(true, Ordering::SeqCst);
    buffer(&mut h, BUF);
    h.send(&create_pipeline_layout(DEVICE, PIPELINE_LAYOUT, &[]))
        .unwrap();
    record_fill(&mut h, CB, BUF);
    h.send(&create_fence(DEVICE, FENCE, false)).unwrap();
    h.send(&queue_submit(QUEUE, &[CB], FENCE)).unwrap();
    assert_eq!(host.held(), 1, "the fake GPU is still at it");
    h.send(&Command::DestroyPipelineLayout(DestroyPipelineLayoutArgs {
        device: VkDevice(DEVICE),
        pipeline_layout: VkPipelineLayout(PIPELINE_LAYOUT),
    }))
    .unwrap();
    let calls = host.calls();
    assert!(
        index_of(&calls, "vkWaitForFences") < index_of(&calls, "destroy VkPipelineLayout"),
        "waited, then destroyed: {calls:?}"
    );
    assert_eq!(host.held(), 0);
    // With nothing in flight a destroy waits for nothing.
    let waits = host.called("vkWaitForFences");
    h.send(&destroy_buffer(DEVICE, BUF)).unwrap();
    assert_eq!(host.called("vkWaitForFences"), waits);
    assert!(!h.fatal());
}

#[test]
fn destroying_after_an_unfenced_submit_waits_for_the_queue() {
    let (mut h, host) = setup();
    host.hold.store(true, Ordering::SeqCst);
    buffer(&mut h, BUF);
    h.send(&create_pipeline_layout(DEVICE, PIPELINE_LAYOUT, &[]))
        .unwrap();
    record_fill(&mut h, CB, BUF);
    h.send(&queue_submit(QUEUE, &[CB], 0)).unwrap();
    h.send(&Command::DestroyPipelineLayout(DestroyPipelineLayoutArgs {
        device: VkDevice(DEVICE),
        pipeline_layout: VkPipelineLayout(PIPELINE_LAYOUT),
    }))
    .unwrap();
    let calls = host.calls();
    assert!(index_of(&calls, "vkQueueWaitIdle") < index_of(&calls, "destroy VkPipelineLayout"));
    assert!(!h.fatal());
}

#[test]
fn a_reset_with_work_in_flight_waits_for_the_device_and_leaves_nothing() {
    let (mut h, host) = setup();
    host.hold.store(true, Ordering::SeqCst);
    buffer(&mut h, BUF);
    h.send(&create_pipeline_layout(DEVICE, PIPELINE_LAYOUT, &[]))
        .unwrap();
    record_fill(&mut h, CB, BUF);
    h.send(&create_fence(DEVICE, FENCE, false)).unwrap();
    h.send(&queue_submit(QUEUE, &[CB], FENCE)).unwrap();
    assert_eq!(host.held(), 1);
    h.renderer.reset();
    let calls = host.calls();
    let idle = index_of(&calls, "device idle");
    assert!(
        calls
            .iter()
            .enumerate()
            .all(|(i, c)| !c.starts_with("destroy") || i > idle),
        "the device went idle before anything was destroyed: {calls:?}"
    );
    assert_eq!(host.held(), 0);
    assert_eq!(host.live_objects(), 0);
}

#[test]
fn destroying_the_command_pool_takes_its_command_buffers_with_it() {
    let (mut h, host) = setup();
    h.send(&allocate_cbs(DEVICE, POOL, &[CB + 5, CB + 6], false))
        .unwrap();
    h.send(&Command::DestroyCommandPool(DestroyCommandPoolArgs {
        device: VkDevice(DEVICE),
        command_pool: VkCommandPool(POOL),
    }))
    .unwrap();
    // The ids are gone: using one is fatal.
    let start = h.tail();
    assert_eq!(h.send(&begin(CB)), Err(start));
    assert_eq!(host.live("VkCommandBuffer"), 0);
}

// ---------------------------------------------------------------- waits

#[test]
fn a_wait_with_a_timeout_answers_vk_timeout_after_that_long() {
    let (mut h, host) = setup();
    host.hold.store(true, Ordering::SeqCst);
    host.stuck.store(true, Ordering::SeqCst);
    buffer(&mut h, BUF);
    record_fill(&mut h, CB, BUF);
    h.send(&create_fence(DEVICE, FENCE, false)).unwrap();
    h.send(&queue_submit(QUEUE, &[CB], FENCE)).unwrap();
    let start = Instant::now();
    let Command::WaitForFences(w) = h.call(&wait_fences(DEVICE, &[FENCE], 60_000_000)).unwrap()
    else {
        panic!()
    };
    assert_eq!(w.ret, VK_TIMEOUT);
    assert!(start.elapsed() >= Duration::from_millis(60));
    let Command::WaitForFences(w) = h.call(&wait_fences(DEVICE, &[FENCE], 0)).unwrap() else {
        panic!()
    };
    assert_eq!(w.ret, VK_TIMEOUT, "a zero timeout is a poll");
    assert!(!h.fatal());
}

#[test]
fn a_ring_blocked_in_a_wait_stays_alive_and_resumes_when_the_gpu_finishes() {
    let host = Arc::new(FakeVulkan::standard());
    let mut h = Harness::monitored(Arc::clone(&host), 1000);
    with_device(&mut h);
    h.send(&allocate_cbs(DEVICE, POOL, &[CB], false)).unwrap();
    host.hold.store(true, Ordering::SeqCst);
    host.stuck.store(true, Ordering::SeqCst);
    buffer(&mut h, BUF);
    record_fill(&mut h, CB, BUF);
    h.send(&create_fence(DEVICE, FENCE, false)).unwrap();
    h.send(&queue_submit(QUEUE, &[CB], FENCE)).unwrap();
    // Mesa's UINT64_MAX wait: the ring blocks on it.
    h.produce(&async_bytes(&wait_fences(DEVICE, &[FENCE], u64::MAX)));
    for _ in 0..5 {
        h.clear_alive();
        std::thread::sleep(Duration::from_millis(30));
        assert!(
            h.alive(),
            "the monitor keeps ALIVE set while the ring waits"
        );
        assert_ne!(h.head(), h.tail(), "the wait has not returned");
    }
    // Another ring of the context is not held up for longer than a slice:
    // the context lock is free between slices.
    let lock_free = h
        .renderer
        .factory()
        .with_context(CTX, |ctx| ctx.object_count())
        .is_some();
    assert!(lock_free);
    host.stuck.store(false, Ordering::SeqCst);
    let tail = h.tail();
    h.wait_head(tail);
    assert!(!h.fatal());
}

#[test]
fn a_ring_torn_down_in_the_middle_of_a_wait_stops_waiting() {
    let (mut h, host) = setup();
    host.hold.store(true, Ordering::SeqCst);
    host.stuck.store(true, Ordering::SeqCst);
    buffer(&mut h, BUF);
    record_fill(&mut h, CB, BUF);
    h.send(&create_fence(DEVICE, FENCE, false)).unwrap();
    h.send(&queue_submit(QUEUE, &[CB], FENCE)).unwrap();
    h.produce(&async_bytes(&wait_fences(DEVICE, &[FENCE], u64::MAX)));
    std::thread::sleep(Duration::from_millis(50));
    let start = Instant::now();
    h.renderer.ctx_destroy(CTX);
    assert!(
        start.elapsed() < Duration::from_secs(2),
        "the worker left its wait within a slice"
    );
    assert_eq!(host.live_objects(), 0);
}

#[test]
fn queue_and_device_idle_are_served_from_what_was_submitted() {
    let (mut h, host) = setup();
    host.hold.store(true, Ordering::SeqCst);
    buffer(&mut h, BUF);
    record_fill(&mut h, CB, BUF);
    h.send(&create_fence(DEVICE, FENCE, false)).unwrap();
    h.send(&queue_submit(QUEUE, &[CB], FENCE)).unwrap();
    let Command::QueueWaitIdle(q) = h
        .call(&Command::QueueWaitIdle(QueueWaitIdleArgs {
            queue: VkQueue(QUEUE),
            ret: 0,
        }))
        .unwrap()
    else {
        panic!()
    };
    assert_eq!(q.ret, VK_SUCCESS);
    assert_eq!(host.held(), 0, "the queue's work is done");
    h.send(&queue_submit(QUEUE, &[CB], 0)).unwrap();
    let Command::DeviceWaitIdle(d) = h
        .call(&Command::DeviceWaitIdle(DeviceWaitIdleArgs {
            device: VkDevice(DEVICE),
            ret: 0,
        }))
        .unwrap()
    else {
        panic!()
    };
    assert_eq!(d.ret, VK_SUCCESS);
    assert_eq!(
        host.called("vkDeviceWaitIdle"),
        1,
        "an unfenced submit needs the device's own"
    );
    assert!(!h.fatal());
}

// ------------------------------------------------------------ refusals

#[test]
fn a_submit_naming_a_semaphore_is_refused_until_stage_5b3() {
    let (mut h, host) = setup();
    buffer(&mut h, BUF);
    record_fill(&mut h, CB, BUF);
    let Command::QueueSubmit(mut submit) = queue_submit(QUEUE, &[CB], 0) else {
        panic!()
    };
    if let Some(s) = submit.p_submits.as_mut().and_then(|s| s.first_mut()) {
        s.signal_semaphore_count = 1;
        s.p_signal_semaphores = Some(vec![VkSemaphore(0x999)]);
    }
    let start = h.tail();
    assert_eq!(h.send(&Command::QueueSubmit(submit)), Err(start));
    assert_eq!(host.called("vkQueueSubmit"), 0);
}

#[test]
fn a_secondary_or_foreign_command_buffer_cannot_be_submitted() {
    let (mut h, host) = setup();
    h.send(&allocate_cbs(DEVICE, POOL, &[CB + 9], true))
        .unwrap();
    let start = h.tail();
    assert_eq!(h.send(&queue_submit(QUEUE, &[CB + 9], 0)), Err(start));
    assert_eq!(host.called("vkQueueSubmit"), 0);
}

#[test]
fn a_wrong_type_id_deep_inside_a_write_is_fatal_and_never_reaches_the_host() {
    let (mut h, host) = setup();
    buffer(&mut h, BUF);
    h.send(&create_storage_set_layout(DEVICE, SET_LAYOUT, 0x20))
        .unwrap();
    h.send(&create_descriptor_pool(DEVICE, DESC_POOL, 0))
        .unwrap();
    h.send(&allocate_sets(DEVICE, DESC_POOL, SET_LAYOUT, &[SET]))
        .unwrap();
    // The descriptor's buffer is the command pool's id.
    let start = h.tail();
    assert_eq!(h.send(&write_storage(DEVICE, SET, POOL)), Err(start));
    assert_eq!(host.called("vkUpdateDescriptorSets"), 0);
}

#[test]
fn a_descriptor_write_past_its_binding_is_fatal() {
    let (mut h, host) = setup();
    buffer(&mut h, BUF);
    h.send(&create_storage_set_layout(DEVICE, SET_LAYOUT, 0x20))
        .unwrap();
    h.send(&create_descriptor_pool(DEVICE, DESC_POOL, 0))
        .unwrap();
    h.send(&allocate_sets(DEVICE, DESC_POOL, SET_LAYOUT, &[SET]))
        .unwrap();
    // Element 1 of a one-descriptor binding.
    let Command::UpdateDescriptorSets(mut write) = write_storage(DEVICE, SET, BUF) else {
        panic!()
    };
    if let Some(w) = write
        .p_descriptor_writes
        .as_mut()
        .and_then(|w| w.first_mut())
    {
        w.dst_array_element = 1;
    }
    let start = h.tail();
    assert_eq!(h.send(&Command::UpdateDescriptorSets(write)), Err(start));
    assert_eq!(host.called("vkUpdateDescriptorSets"), 0);
}

#[test]
fn an_enum_outside_vulkan_1_3_is_fatal_before_the_host_sees_it() {
    let (mut h, host) = setup();
    let Command::CreateDescriptorSetLayout(mut layout) =
        create_storage_set_layout(DEVICE, SET_LAYOUT, 0x20)
    else {
        panic!()
    };
    if let Some(b) = layout
        .p_create_info
        .as_mut()
        .and_then(|i| i.p_bindings.as_mut())
        .and_then(|b| b.first_mut())
    {
        b.descriptor_type = 1_000_150_000; // VK_DESCRIPTOR_TYPE_ACCELERATION_STRUCTURE_KHR
    }
    let start = h.tail();
    assert_eq!(
        h.send(&Command::CreateDescriptorSetLayout(layout)),
        Err(start)
    );
    assert_eq!(host.called("vkCreateDescriptorSetLayout"), 0);
}

#[test]
fn recording_is_bounded_by_the_objects_and_the_limits_it_names() {
    // Each of these is refused, fatal, and never reaches the host.
    type Case = (&'static str, Box<dyn Fn() -> Vec<Command<'static>>>);
    let cases: Vec<Case> = vec![
        (
            "a fill past the end of its buffer",
            Box::new(|| vec![begin(CB), fill(CB, BUF, 60 << 10, 8 << 10, 0)]),
        ),
        (
            "a copy region past the end of its buffer",
            Box::new(|| {
                vec![
                    begin(CB),
                    copy_buffer(CB, BUF, BUF + 1, &[(0, 32 << 10, 64 << 10)]),
                ]
            }),
        ),
        (
            "push constants past maxPushConstantsSize",
            Box::new(|| {
                vec![
                    begin(CB),
                    Command::CmdPushConstants(CmdPushConstantsArgs {
                        command_buffer: VkCommandBuffer(CB),
                        layout: VkPipelineLayout(PIPELINE_LAYOUT),
                        stage_flags: 0x20,
                        offset: 256,
                        size: 4,
                        p_values: Some(&[0; 4]),
                    }),
                ]
            }),
        ),
        (
            "viewports past maxViewports",
            Box::new(|| {
                vec![
                    begin(CB),
                    Command::CmdSetViewport(CmdSetViewportArgs {
                        command_buffer: VkCommandBuffer(CB),
                        first_viewport: 15,
                        viewport_count: 2,
                        p_viewports: Some(vec![VkViewport::default(); 2]),
                    }),
                ]
            }),
        ),
        (
            "a primary command buffer executed as a secondary",
            Box::new(|| {
                vec![
                    begin(CB),
                    Command::CmdExecuteCommands(CmdExecuteCommandsArgs {
                        command_buffer: VkCommandBuffer(CB),
                        command_buffer_count: 1,
                        p_command_buffers: Some(vec![VkCommandBuffer(CB)]),
                    }),
                ]
            }),
        ),
        (
            "a set bound past its layout's sets",
            Box::new(|| vec![begin(CB), bind_sets(CB, 1, PIPELINE_LAYOUT, &[SET, SET])]),
        ),
    ];
    for (what, commands) in cases {
        let (mut h, host) = setup();
        buffer(&mut h, BUF);
        buffer(&mut h, BUF + 1);
        h.send(&create_storage_set_layout(DEVICE, SET_LAYOUT, 0x20))
            .unwrap();
        h.send(&create_descriptor_pool(DEVICE, DESC_POOL, 0))
            .unwrap();
        h.send(&allocate_sets(DEVICE, DESC_POOL, SET_LAYOUT, &[SET]))
            .unwrap();
        h.send(&create_pipeline_layout(
            DEVICE,
            PIPELINE_LAYOUT,
            &[SET_LAYOUT],
        ))
        .unwrap();
        let commands = commands();
        let refused = commands.last().unwrap().name();
        let outcome = h.submit_recording(&commands);
        assert!(
            matches!(outcome, Outcome::Fatal { .. }),
            "{what}: {outcome:?}"
        );
        assert_eq!(host.called(refused), 0, "{what} reached the host");
    }
}

#[test]
fn a_render_pass_reference_outside_the_pass_is_fatal() {
    let (mut h, host) = setup();
    let Command::CreateRenderPass(mut rp) = create_render_pass(DEVICE, RENDER_PASS, RGBA8) else {
        panic!()
    };
    if let Some(r) = rp
        .p_create_info
        .as_mut()
        .and_then(|i| i.p_subpasses.as_mut())
        .and_then(|s| s.first_mut())
        .and_then(|s| s.p_color_attachments.as_mut())
        .and_then(|c| c.first_mut())
    {
        r.attachment = 7;
    }
    let start = h.tail();
    assert_eq!(h.send(&Command::CreateRenderPass(rp)), Err(start));
    assert_eq!(host.called("vkCreateRenderPass"), 0);
}

#[test]
fn bound_sets_must_bring_one_dynamic_offset_per_dynamic_descriptor() {
    let (mut h, host) = setup();
    let Command::CreateDescriptorSetLayout(mut layout) =
        create_storage_set_layout(DEVICE, SET_LAYOUT, 0x20)
    else {
        panic!()
    };
    if let Some(b) = layout
        .p_create_info
        .as_mut()
        .and_then(|i| i.p_bindings.as_mut())
        .and_then(|b| b.first_mut())
    {
        b.descriptor_type = 9; // STORAGE_BUFFER_DYNAMIC
    }
    h.send(&Command::CreateDescriptorSetLayout(layout)).unwrap();
    let Command::CreateDescriptorPool(mut pool) = create_descriptor_pool(DEVICE, DESC_POOL, 0)
    else {
        panic!()
    };
    if let Some(s) = pool
        .p_create_info
        .as_mut()
        .and_then(|i| i.p_pool_sizes.as_mut())
        .and_then(|s| s.first_mut())
    {
        s.type_ = 9;
    }
    h.send(&Command::CreateDescriptorPool(pool)).unwrap();
    h.send(&allocate_sets(DEVICE, DESC_POOL, SET_LAYOUT, &[SET]))
        .unwrap();
    h.send(&create_pipeline_layout(
        DEVICE,
        PIPELINE_LAYOUT,
        &[SET_LAYOUT],
    ))
    .unwrap();
    // No dynamic offset for the one dynamic descriptor.
    let outcome = h.submit_recording(&[begin(CB), bind_sets(CB, 1, PIPELINE_LAYOUT, &[SET])]);
    assert!(matches!(outcome, Outcome::Fatal { .. }));
    assert_eq!(host.called("vkCmdBindDescriptorSets"), 0);
}

// ------------------------------------------------------ device, teardown

#[test]
fn robust_buffer_access_is_on_for_every_host_device_that_has_it() {
    let (_h, host) = setup();
    let request = host.device_requests().pop().expect("a device");
    assert_eq!(
        request.features.map(|f| f.robust_buffer_access),
        Some(1),
        "enabled although the guest did not ask"
    );
}

#[test]
fn every_kind_of_object_goes_with_the_instance_in_order() {
    let (mut h, host) = setup();
    h.send(&create_shader_module(
        DEVICE,
        SHADER,
        &[0x0723_0203, 0, 0, 0, 0],
    ))
    .unwrap();
    h.send(&create_storage_set_layout(DEVICE, SET_LAYOUT, 0x20))
        .unwrap();
    h.send(&create_descriptor_pool(DEVICE, DESC_POOL, 1))
        .unwrap();
    h.send(&allocate_sets(
        DEVICE,
        DESC_POOL,
        SET_LAYOUT,
        &[SET, SET + 0x100],
    ))
    .unwrap();
    h.send(&create_pipeline_layout(
        DEVICE,
        PIPELINE_LAYOUT,
        &[SET_LAYOUT],
    ))
    .unwrap();
    h.send(&create_compute_pipeline(
        DEVICE,
        PIPELINE,
        SHADER,
        PIPELINE_LAYOUT,
    ))
    .unwrap();
    h.send(&create_render_pass(DEVICE, RENDER_PASS, RGBA8))
        .unwrap();
    h.send(&create_fence(DEVICE, FENCE, true)).unwrap();
    h.send(&Command::CreateEvent(CreateEventArgs {
        device: VkDevice(DEVICE),
        p_create_info: Some(VkEventCreateInfo::default()),
        p_event: Some(VkEvent(0xe5)),
        ret: 0,
    }))
    .unwrap();
    h.send(&Command::CreateQueryPool(CreateQueryPoolArgs {
        device: VkDevice(DEVICE),
        p_create_info: Some(VkQueryPoolCreateInfo {
            flags: 0,
            query_type: 2, // TIMESTAMP
            query_count: 8,
            pipeline_statistics: 0,
        }),
        p_query_pool: Some(VkQueryPool(0xe6)),
        ret: 0,
    }))
    .unwrap();
    // A set of a pool with FREE_DESCRIPTOR_SET freed one by one.
    h.send(&Command::FreeDescriptorSets(FreeDescriptorSetsArgs {
        device: VkDevice(DEVICE),
        descriptor_pool: VkDescriptorPool(DESC_POOL),
        descriptor_set_count: 1,
        p_descriptor_sets: Some(vec![VkDescriptorSet(SET + 0x100)]),
        ret: 0,
    }))
    .unwrap();
    assert!(!h.fatal());
    assert!(host.live("VkPipeline") == 1 && host.live("VkQueryPool") == 1);
    h.send(&Command::DestroyInstance(DestroyInstanceArgs {
        instance: VkInstance(INSTANCE),
    }))
    .unwrap();
    assert_eq!(host.live_objects(), 0);
    let calls = host.calls();
    assert!(index_of(&calls, "destroy VkPipeline") < index_of(&calls, "destroy VkPipelineLayout"));
    assert!(
        index_of(&calls, "destroy VkDescriptorPool")
            < index_of(&calls, "destroy VkDescriptorSetLayout")
    );
    assert_eq!(h.renderer.factory().host_objects(), 0);
}

#[test]
fn a_command_newer_than_the_device_the_guest_was_shown_is_refused() {
    let mut old = super::fake::gpu("an old GPU");
    old.info.properties.properties.api_version =
        crate::venus::capset::vk_make_api_version(0, 1, 2, 0);
    let host = Arc::new(FakeVulkan::new(vec![old]));
    let mut h = Harness::new(Arc::clone(&host));
    with_device(&mut h);
    h.send(&allocate_cbs(DEVICE, POOL, &[CB], false)).unwrap();
    let outcome = h.submit_recording(&[
        begin(CB),
        Command::CmdEndRendering(CmdEndRenderingArgs {
            command_buffer: VkCommandBuffer(CB),
        }),
    ]);
    assert!(matches!(outcome, Outcome::Fatal { .. }));
    assert_eq!(host.called("vkCmdEndRendering"), 0);
}
