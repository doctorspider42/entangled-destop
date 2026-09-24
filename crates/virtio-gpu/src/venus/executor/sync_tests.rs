//! Stage 5b.3 against the fake host: semaphores through submits and host
//! waits, the sync-file emulation Mesa's WSI relies on, the extensions and
//! properties a Vulkan 1.3 guest is shown, dynamic rendering's bounds, and
//! virtio-gpu fences on a queue's `ring_idx` timeline — including teardown,
//! pausing and snapshots with fences pending.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use virtio_core::HostWaker;

use super::fake::{self, FakeVulkan};
use super::harness::*;
use super::objects::Kind;
use super::policy;
use super::recording::*;
use crate::renderer::{FenceOutcome, FenceTimeline, Renderer3d};
use crate::venus::capset::vk_make_api_version;
use crate::venus::protocol::*;
use crate::venus::renderer::SinkFactory;

const BUF: u64 = 0x200;
const QUEUE2: u64 = QUEUE + 1;
const SEM_A: u64 = SEMAPHORE;
const SEM_B: u64 = SEMAPHORE + 1;
const TIMELINE: u64 = SEMAPHORE + 2;
const CB2: u64 = CB + 1;

fn setup() -> (Harness<FakeVulkan>, Arc<FakeVulkan>) {
    let host = Arc::new(FakeVulkan::standard());
    let mut h = Harness::new(Arc::clone(&host));
    with_device(&mut h);
    h.send(&allocate_cbs(DEVICE, POOL, &[CB, CB2], false))
        .unwrap();
    (h, host)
}

/// A 64 KiB transfer buffer bound to device-local memory, and `cb` recorded
/// to fill it.
fn recorded(h: &mut Harness<FakeVulkan>, cb: u64) {
    if h.renderer
        .factory()
        .with_context(CTX, |c| c.objects.buffer(DEVICE, BUF).is_err())
        .unwrap_or(true)
    {
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
    }
    let outcome = h.submit_recording(&[
        begin(cb),
        fill(cb, BUF, 0, WHOLE_SIZE, 0xa5a5_a5a5),
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

// ------------------------------------------------------------- semaphores

#[test]
fn vk_smoke_check_8_a_timeline_across_two_submits_and_a_host_wait() {
    let (mut h, host) = setup();
    recorded(&mut h, CB);
    recorded(&mut h, CB2);
    h.send(&create_semaphore(DEVICE, TIMELINE, Some(0), 0))
        .unwrap();
    let Command::GetSemaphoreCounterValue(c) = h.call(&counter_value(DEVICE, TIMELINE)).unwrap()
    else {
        panic!()
    };
    assert_eq!((c.ret, c.p_value), (VK_SUCCESS, Some(0)));
    // Submit A signals 1; a separate submit B waits for 1 and signals 2.
    h.send(&submit_semaphores(
        QUEUE,
        &[CB],
        &[],
        &[(TIMELINE, 1)],
        true,
        0,
    ))
    .unwrap();
    h.send(&submit_semaphores(
        QUEUE,
        &[CB2],
        &[(TIMELINE, 1)],
        &[(TIMELINE, 2)],
        true,
        0,
    ))
    .unwrap();
    // The host wait, as vk-smoke makes it (a call with a timeout)...
    let Command::WaitSemaphores(w) = h
        .call(&wait_semaphores(DEVICE, &[(TIMELINE, 2)], 10_000_000_000))
        .unwrap()
    else {
        panic!()
    };
    assert_eq!(w.ret, VK_SUCCESS);
    // ... and as Mesa sends it once its feedback slot shows the value.
    h.send(&wait_semaphores(DEVICE, &[(TIMELINE, 2)], u64::MAX))
        .unwrap();
    let Command::GetSemaphoreCounterValue(c) = h.call(&counter_value(DEVICE, TIMELINE)).unwrap()
    else {
        panic!()
    };
    assert_eq!(c.p_value, Some(2));
    // Both batches reached the host with the host's own handle for the
    // semaphore.
    let sem = raw(&h, Kind::Semaphore, TIMELINE);
    assert_ne!(sem, TIMELINE, "translated");
    let ops = host.semaphore_ops();
    assert_eq!(ops.len(), 2);
    assert_eq!((ops[0].1.clone(), ops[0].2.clone()), (vec![], vec![sem]));
    assert_eq!((ops[1].1.clone(), ops[1].2.clone()), (vec![sem], vec![sem]));
    assert_eq!(host.timeline_value(sem), Some(2));
    // vkSignalSemaphore from the host side.
    h.send(&signal_semaphore(DEVICE, TIMELINE, 5)).unwrap();
    assert_eq!(host.timeline_value(sem), Some(5));
    h.send(&destroy_semaphore(DEVICE, TIMELINE)).unwrap();
    assert_eq!(host.live("VkSemaphore"), 0);
    assert!(!h.fatal());
}

#[test]
fn binary_semaphores_order_two_submits_and_submit2_carries_them_too() {
    let (mut h, host) = setup();
    recorded(&mut h, CB);
    recorded(&mut h, CB2);
    h.send(&create_semaphore(DEVICE, SEM_A, None, 0)).unwrap();
    h.send(&queue_submit(QUEUE, &[CB], 0)).unwrap();
    h.send(&submit_semaphores(
        QUEUE,
        &[CB],
        &[],
        &[(SEM_A, 0)],
        false,
        0,
    ))
    .unwrap();
    h.send(&submit2_semaphores(QUEUE, &[CB2], &[(SEM_A, 0)], &[], 0))
        .unwrap();
    assert!(!h.fatal());
    let sem = raw(&h, Kind::Semaphore, SEM_A);
    let ops = host.semaphore_ops();
    assert_eq!(
        ops.last().map(|o| o.1.clone()),
        Some(vec![sem]),
        "the wait reached the host"
    );
    // Signalled again, now through vkQueueSubmit2.
    h.send(&submit2_semaphores(QUEUE, &[CB], &[], &[(SEM_A, 0)], 0))
        .unwrap();
    assert!(!h.fatal());
}

#[test]
fn a_binary_wait_with_nothing_to_wait_for_is_refused_before_the_host_sees_it() {
    let (mut h, host) = setup();
    recorded(&mut h, CB);
    h.send(&create_semaphore(DEVICE, SEM_A, None, 0)).unwrap();
    let submits = host.called("vkQueueSubmit");
    let start = h.tail();
    assert_eq!(
        h.send(&submit_semaphores(
            QUEUE,
            &[CB],
            &[(SEM_A, 0)],
            &[],
            false,
            0
        )),
        Err(start),
        "the GPU would wait on it forever"
    );
    assert_eq!(host.called("vkQueueSubmit"), submits);
}

#[test]
fn a_second_signal_of_a_signalled_binary_semaphore_is_refused() {
    let (mut h, host) = setup();
    recorded(&mut h, CB);
    h.send(&create_semaphore(DEVICE, SEM_A, None, 0)).unwrap();
    h.send(&submit_semaphores(
        QUEUE,
        &[CB],
        &[],
        &[(SEM_A, 0)],
        false,
        0,
    ))
    .unwrap();
    let submits = host.called("vkQueueSubmit");
    let start = h.tail();
    assert_eq!(
        h.send(&submit_semaphores(
            QUEUE,
            &[CB],
            &[],
            &[(SEM_A, 0)],
            false,
            0
        )),
        Err(start)
    );
    assert_eq!(host.called("vkQueueSubmit"), submits);
}

#[test]
fn timeline_values_must_be_one_per_semaphore_and_timeline_commands_need_a_timeline() {
    let (mut h, host) = setup();
    recorded(&mut h, CB);
    h.send(&create_semaphore(DEVICE, TIMELINE, Some(0), 0))
        .unwrap();
    // A timeline signal whose values array is short: the driver would read
    // past it.
    let mut short = submit_semaphores(QUEUE, &[CB], &[], &[(TIMELINE, 1)], true, 0);
    if let Command::QueueSubmit(a) = &mut short {
        for link in &mut a.p_submits.as_mut().unwrap()[0].p_next {
            if let VkSubmitInfoNext::VkTimelineSemaphoreSubmitInfo(t) = link {
                t.signal_semaphore_value_count = 0;
                t.p_signal_semaphore_values = Some(Vec::new());
            }
        }
    }
    let start = h.tail();
    assert_eq!(h.send(&short), Err(start));
    assert_eq!(host.semaphore_ops().len(), 0);

    let (mut h, _) = setup();
    h.send(&create_semaphore(DEVICE, SEM_A, None, 0)).unwrap();
    let start = h.tail();
    assert_eq!(h.send(&signal_semaphore(DEVICE, SEM_A, 1)), Err(start));
    let (mut h, _) = setup();
    h.send(&create_semaphore(DEVICE, SEM_A, None, 0)).unwrap();
    assert!(h.call(&counter_value(DEVICE, SEM_A)).is_err());
}

#[test]
fn a_semaphore_wait_with_a_timeout_answers_vk_timeout_after_that_long() {
    let (mut h, _host) = setup();
    h.send(&create_semaphore(DEVICE, TIMELINE, Some(0), 0))
        .unwrap();
    let start = Instant::now();
    let Command::WaitSemaphores(w) = h
        .call(&wait_semaphores(DEVICE, &[(TIMELINE, 1)], 60_000_000))
        .unwrap()
    else {
        panic!()
    };
    assert_eq!(w.ret, VK_TIMEOUT);
    assert!(start.elapsed() >= Duration::from_millis(60));
    assert!(!h.fatal());
}

#[test]
fn a_ring_in_a_semaphore_wait_stays_alive_and_a_teardown_stops_it_within_a_slice() {
    let host = Arc::new(FakeVulkan::standard());
    let mut h = Harness::monitored(Arc::clone(&host), 1000);
    with_device(&mut h);
    h.send(&create_semaphore(DEVICE, TIMELINE, Some(0), 0))
        .unwrap();
    host.stuck.store(true, Ordering::SeqCst);
    h.produce(&async_bytes(&wait_semaphores(
        DEVICE,
        &[(TIMELINE, 1)],
        u64::MAX,
    )));
    for _ in 0..3 {
        h.clear_alive();
        std::thread::sleep(Duration::from_millis(30));
        assert!(
            h.alive(),
            "the monitor keeps ALIVE set while the ring waits"
        );
        assert_ne!(h.head(), h.tail(), "the wait has not returned");
    }
    let start = Instant::now();
    h.renderer.ctx_destroy(CTX);
    assert!(start.elapsed() < Duration::from_secs(2));
    assert_eq!(host.live_objects(), 0);
}

// ------------------------------------------------------ sync-fd emulation

#[test]
fn a_sync_fd_import_is_a_temporary_payload_the_next_wait_consumes_without_the_host() {
    let (mut h, host) = setup();
    recorded(&mut h, CB);
    // The acquire semaphore of a swapchain image: binary, not signalled.
    h.send(&create_semaphore(DEVICE, SEM_A, None, 0)).unwrap();
    // `vn_queue_submission_fix_batch_semaphores`: the guest waited for the
    // sync file itself, and imports a signalled payload before the submit.
    h.send(&import_semaphore_resource(DEVICE, SEM_A, 0))
        .unwrap();
    assert_eq!(host.called("vkQueueSubmit"), 0, "no host operation");
    h.send(&submit_semaphores(
        QUEUE,
        &[CB],
        &[(SEM_A, 0)],
        &[],
        false,
        0,
    ))
    .unwrap();
    assert!(!h.fatal());
    let ops = host.semaphore_ops();
    assert_eq!(ops.len(), 1);
    assert!(
        ops[0].1.is_empty(),
        "the wait a temporary payload satisfies never reaches the host: {ops:?}"
    );
    // The temporary payload is consumed: a second wait has nothing.
    let start = h.tail();
    assert_eq!(
        h.send(&submit_semaphores(
            QUEUE,
            &[CB],
            &[(SEM_A, 0)],
            &[],
            false,
            0
        )),
        Err(start)
    );
}

#[test]
fn a_temporary_payload_over_a_signalled_one_leaves_the_permanent_one_for_the_next_wait() {
    let (mut h, host) = setup();
    recorded(&mut h, CB);
    h.send(&create_semaphore(DEVICE, SEM_A, None, 0)).unwrap();
    h.send(&submit_semaphores(
        QUEUE,
        &[CB],
        &[],
        &[(SEM_A, 0)],
        false,
        0,
    ))
    .unwrap();
    h.send(&import_semaphore_resource(DEVICE, SEM_A, 0))
        .unwrap();
    // The first wait consumes the temporary payload, the host sees none of
    // it; the second consumes the permanent one, on the host.
    h.send(&submit_semaphores(
        QUEUE,
        &[CB],
        &[(SEM_A, 0)],
        &[],
        false,
        0,
    ))
    .unwrap();
    h.send(&submit_semaphores(
        QUEUE,
        &[CB],
        &[(SEM_A, 0)],
        &[],
        false,
        0,
    ))
    .unwrap();
    assert!(!h.fatal());
    let sem = raw(&h, Kind::Semaphore, SEM_A);
    let waits: Vec<Vec<u64>> = host.semaphore_ops().into_iter().map(|o| o.1).collect();
    assert_eq!(waits, vec![vec![], vec![], vec![sem]]);
}

#[test]
fn a_sync_fd_export_consumes_the_pending_payload_with_an_empty_waiting_submit() {
    let (mut h, host) = setup();
    recorded(&mut h, CB);
    h.send(&create_semaphore(DEVICE, SEM_A, None, 0)).unwrap();
    h.send(&submit_semaphores(
        QUEUE,
        &[CB],
        &[],
        &[(SEM_A, 0)],
        false,
        0,
    ))
    .unwrap();
    // `vn_GetSemaphoreFdKHR` on a device-only payload.
    h.send(&wait_semaphore_resource(DEVICE, SEM_A)).unwrap();
    let sem = raw(&h, Kind::Semaphore, SEM_A);
    let ops = host.semaphore_ops();
    assert_eq!(ops.last().map(|o| o.1.clone()), Some(vec![sem]));
    assert_eq!(
        host.submitted().last(),
        Some(&Vec::new()),
        "no command buffer"
    );
    // Consumed: it may be signalled again, and a second export has nothing.
    h.send(&submit_semaphores(
        QUEUE,
        &[CB],
        &[],
        &[(SEM_A, 0)],
        false,
        0,
    ))
    .unwrap();
    h.send(&wait_semaphore_resource(DEVICE, SEM_A)).unwrap();
    let start = h.tail();
    assert_eq!(h.send(&wait_semaphore_resource(DEVICE, SEM_A)), Err(start));
}

#[test]
fn an_export_after_a_temporary_import_needs_no_host() {
    let (mut h, host) = setup();
    h.send(&create_semaphore(DEVICE, SEM_A, None, 0)).unwrap();
    // `vn_GetSemaphoreFdKHR` on an imported payload: import 0, then wait.
    h.send(&import_semaphore_resource(DEVICE, SEM_A, 0))
        .unwrap();
    h.send(&wait_semaphore_resource(DEVICE, SEM_A)).unwrap();
    assert_eq!(host.called("vkQueueSubmit"), 0);
    assert!(!h.fatal());
}

#[test]
fn a_resource_import_or_an_import_into_a_timeline_is_refused() {
    let (mut h, _) = setup();
    h.send(&create_semaphore(DEVICE, SEM_A, None, 0)).unwrap();
    let start = h.tail();
    assert_eq!(
        h.send(&import_semaphore_resource(DEVICE, SEM_A, 7)),
        Err(start),
        "a resource would be a sync file to import"
    );
    let (mut h, _) = setup();
    h.send(&create_semaphore(DEVICE, TIMELINE, Some(0), 0))
        .unwrap();
    let start = h.tail();
    assert_eq!(
        h.send(&import_semaphore_resource(DEVICE, TIMELINE, 0)),
        Err(start)
    );
}

#[test]
fn the_export_create_info_loses_sync_fd_before_the_host_and_other_types_must_be_the_hosts() {
    let (mut h, host) = setup();
    h.send(&create_semaphore(
        DEVICE,
        SEM_A,
        None,
        policy::SEMAPHORE_HANDLE_SYNC_FD,
    ))
    .unwrap();
    // OPAQUE_WIN32, which the fake (like a Windows driver) exports.
    h.send(&create_semaphore(
        DEVICE,
        SEM_B,
        None,
        0x2 | policy::SEMAPHORE_HANDLE_SYNC_FD,
    ))
    .unwrap();
    assert!(!h.fatal());
    assert_eq!(
        host.semaphore_exports(),
        vec![None, Some(0x2)],
        "SYNC_FD never reaches the host driver; its own type does"
    );
    // OPAQUE_FD, which it does not.
    let start = h.tail();
    assert_eq!(
        h.send(&create_semaphore(DEVICE, TIMELINE, None, 0x1)),
        Err(start)
    );
}

// ------------------------------------------- what a 1.3 guest is shown

#[test]
fn the_sync_fd_properties_are_synthesized_and_every_other_type_is_the_hosts() {
    let host = Arc::new(FakeVulkan::standard());
    let mut h = Harness::new(Arc::clone(&host));
    boot(&mut h);
    let query = |h: &mut Harness<FakeVulkan>, handle: i32, timeline: bool| {
        let Command::GetPhysicalDeviceExternalSemaphoreProperties(p) = h
            .call(&external_semaphore_query(PHYSICAL, handle, timeline))
            .unwrap()
        else {
            panic!()
        };
        p.p_external_semaphore_properties.unwrap()
    };
    let sync_fd = query(&mut h, 0x10, false);
    assert_eq!(
        (
            sync_fd.external_semaphore_features,
            sync_fd.compatible_handle_types,
            sync_fd.export_from_imported_handle_types
        ),
        (
            policy::SEMAPHORE_FEATURE_IMPORTABLE | policy::SEMAPHORE_FEATURE_EXPORTABLE,
            0x10,
            0x10
        ),
        "IMPORTABLE, which is what Mesa 26.0.8 gates sync2 and the swapchain on, and \
         (stage S1) EXPORTABLE, which it gates VK_KHR_external_semaphore_fd on"
    );
    assert_eq!(
        query(&mut h, 0x10, true),
        VkExternalSemaphoreProperties::default()
    );
    assert_eq!(query(&mut h, 0x2, false).external_semaphore_features, 0x3);
    assert_eq!(
        query(&mut h, 0x1, false),
        VkExternalSemaphoreProperties::default()
    );
    let start = h.tail();
    assert!(h
        .call(&external_semaphore_query(PHYSICAL, 0x3, false))
        .is_err());
    assert!(h.tail() > start);
}

#[test]
fn the_emulated_extension_is_enabled_for_the_guest_and_never_for_the_host() {
    let host = Arc::new(FakeVulkan::standard());
    let mut h = Harness::new(Arc::clone(&host));
    boot(&mut h);
    let mut create = create_device(PHYSICAL, DEVICE, Vec::new());
    if let Command::CreateDevice(a) = &mut create {
        let info = a.p_create_info.as_mut().unwrap();
        // What Mesa sends for an application that wants a swapchain and
        // sync2 (`vn_device.c:333-337`).
        info.enabled_extension_count = 2;
        info.pp_enabled_extension_names = Some(vec![
            b"VK_KHR_synchronization2".as_slice(),
            b"VK_KHR_external_semaphore_fd".as_slice(),
        ]);
    }
    let Command::CreateDevice(d) = h.call(&create).unwrap() else {
        panic!()
    };
    assert_eq!(d.ret, VK_SUCCESS);
    let request = host.device_requests().pop().unwrap();
    assert!(request
        .extensions
        .iter()
        .any(|e| e == "VK_KHR_synchronization2"));
    assert!(
        !request
            .extensions
            .iter()
            .any(|e| e == "VK_KHR_external_semaphore_fd"),
        "{:?}",
        request.extensions
    );
}

#[test]
fn features2_answers_the_1_3_structures_mesa_asks_for() {
    let host = Arc::new(FakeVulkan::standard());
    let mut h = Harness::new(Arc::clone(&host));
    boot(&mut h);
    let Command::GetPhysicalDeviceFeatures2(f) = h
        .call(&Command::GetPhysicalDeviceFeatures2(
            GetPhysicalDeviceFeatures2Args {
                physical_device: VkPhysicalDevice(PHYSICAL),
                p_features: Some(VkPhysicalDeviceFeatures2 {
                    p_next: vec![
                        VkPhysicalDeviceFeatures2Next::VkPhysicalDeviceVulkan13Features(
                            Default::default(),
                        ),
                        VkPhysicalDeviceFeatures2Next::VkPhysicalDeviceSynchronization2Features(
                            Default::default(),
                        ),
                        VkPhysicalDeviceFeatures2Next::VkPhysicalDeviceDynamicRenderingFeatures(
                            Default::default(),
                        ),
                        VkPhysicalDeviceFeatures2Next::VkPhysicalDeviceTimelineSemaphoreFeatures(
                            Default::default(),
                        ),
                    ],
                    ..Default::default()
                }),
            },
        ))
        .unwrap()
    else {
        panic!()
    };
    let links = f.p_features.unwrap().p_next;
    use VkPhysicalDeviceFeatures2Next as N;
    assert!(matches!(&links[0], N::VkPhysicalDeviceVulkan13Features(v)
        if v.synchronization2 == 1 && v.dynamic_rendering == 1));
    assert!(
        matches!(&links[1], N::VkPhysicalDeviceSynchronization2Features(v) if v.synchronization2 == 1)
    );
    assert!(
        matches!(&links[2], N::VkPhysicalDeviceDynamicRenderingFeatures(v) if v.dynamic_rendering == 1)
    );
    assert!(
        matches!(&links[3], N::VkPhysicalDeviceTimelineSemaphoreFeatures(v) if v.timeline_semaphore == 1)
    );
}

#[test]
fn core_1_3_commands_pass_only_on_a_device_the_guest_sees_as_1_3() {
    // The fake RTX: 1.4 host, 1.3 shown, VK_KHR_synchronization2 listed.
    let (mut h, host) = setup();
    let outcome = h.submit_recording(&[
        begin(CB),
        Command::CmdSetCullMode(CmdSetCullModeArgs {
            command_buffer: VkCommandBuffer(CB),
            cull_mode: 0,
        }),
        end(CB),
    ]);
    assert_eq!(outcome, Outcome::Consumed);
    assert_eq!(host.called("vkCmdSetCullMode"), 1);
    // A 1.3 host that does not list sync2: Mesa 26.0.8 shows its guest 1.2,
    // so a 1.3 command cannot be one of its own.
    let mut quiet = fake::gpu("a GPU without the extension");
    quiet
        .info
        .extensions
        .retain(|e| policy::c_name(&e.extension_name) != b"VK_KHR_synchronization2");
    quiet.info.properties.properties.api_version = vk_make_api_version(0, 1, 3, 280);
    let host = Arc::new(FakeVulkan::new(vec![quiet]));
    let mut h = Harness::new(Arc::clone(&host));
    with_device(&mut h);
    h.send(&allocate_cbs(DEVICE, POOL, &[CB], false)).unwrap();
    let outcome = h.submit_recording(&[
        begin(CB),
        Command::CmdSetCullMode(CmdSetCullModeArgs {
            command_buffer: VkCommandBuffer(CB),
            cull_mode: 0,
        }),
    ]);
    assert!(matches!(outcome, Outcome::Fatal { .. }));
    assert_eq!(host.called("vkCmdSetCullMode"), 0);
}

// ------------------------------------------------------ dynamic rendering

#[test]
fn dynamic_rendering_is_bounded_like_a_render_pass() {
    let (mut h, host) = setup();
    const IMG: u64 = 0x400;
    const VIEW: u64 = 0x402;
    h.call(&create_image(DEVICE, IMG, image_info())).unwrap();
    h.send(&allocate(DEVICE, IMG | 0x1000, 1 << 20, 0, Vec::new()))
        .unwrap();
    h.send(&bind_image(DEVICE, IMG, IMG | 0x1000, 0)).unwrap();
    h.send(&create_image_view(DEVICE, VIEW, IMG, RGBA8))
        .unwrap();
    let outcome = h.submit_recording(&[
        begin(CB),
        begin_rendering(CB, VIEW, 64, [0.0; 4]),
        end_rendering(CB),
        end(CB),
    ]);
    assert_eq!(outcome, Outcome::Consumed);
    assert_eq!(host.called("vkCmdBeginRendering"), 1);
    // Nine colour attachments on a device of eight.
    let mut wide = begin_rendering(CB2, VIEW, 64, [0.0; 4]);
    if let Command::CmdBeginRendering(a) = &mut wide {
        let info = a.p_rendering_info.as_mut().unwrap();
        let one = info.p_color_attachments.as_ref().unwrap()[0].clone();
        info.color_attachment_count = 9;
        info.p_color_attachments = Some(vec![one; 9]);
    }
    let outcome = h.submit_recording(&[begin(CB2), wide]);
    assert!(matches!(outcome, Outcome::Fatal { .. }));
    assert_eq!(host.called("vkCmdBeginRendering"), 1);

    // A pipeline for dynamic rendering naming nine formats.
    let (mut h, host) = setup();
    h.send(&create_shader_module(DEVICE, SHADER, &[0x0723_0203; 8]))
        .unwrap();
    h.send(&create_pipeline_layout(DEVICE, PIPELINE_LAYOUT, &[]))
        .unwrap();
    let mut pipeline =
        create_dynamic_triangle_pipeline(DEVICE, PIPELINE, SHADER, PIPELINE_LAYOUT, RGBA8, 64);
    if let Command::CreateGraphicsPipelines(a) = &mut pipeline {
        for link in &mut a.p_create_infos.as_mut().unwrap()[0].p_next {
            if let VkGraphicsPipelineCreateInfoNext::VkPipelineRenderingCreateInfo(r) = link {
                r.color_attachment_count = 9;
                r.p_color_attachment_formats = Some(vec![RGBA8; 9]);
            }
        }
    }
    let start = h.tail();
    assert_eq!(h.send(&pipeline), Err(start));
    assert_eq!(host.called("vkCreateGraphicsPipelines"), 0);
}

// ------------------------------------------- fences on a queue's timeline

#[derive(Default)]
struct CountingWaker(AtomicUsize);

impl HostWaker for CountingWaker {
    fn wake(&self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

/// A device with two queues of family 0, bound to timelines 1 and 2 (host
/// queue handles 0 and 1), and the device's waker installed.
fn two_queues(h: &mut Harness<FakeVulkan>) -> Arc<CountingWaker> {
    boot(h);
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
    h.call(&device_queue(DEVICE, QUEUE, 1)).unwrap();
    let mut second = device_queue(DEVICE, QUEUE2, 2);
    if let Command::GetDeviceQueue2(a) = &mut second {
        a.p_queue_info.as_mut().unwrap().queue_index = 1;
    }
    h.call(&second).unwrap();
    assert!(!h.fatal());
    let waker = Arc::new(CountingWaker::default());
    h.renderer
        .set_host_waker(Arc::clone(&waker) as Arc<dyn HostWaker>);
    waker
}

fn ring(ring_idx: u8) -> FenceTimeline {
    FenceTimeline::Ring {
        ctx_id: CTX,
        ring_idx,
    }
}

/// Everything the device would collect within `within`, until `want` have
/// come.
fn collect(
    h: &mut Harness<FakeVulkan>,
    want: usize,
    within: Duration,
) -> Vec<(FenceTimeline, u32)> {
    let deadline = Instant::now() + within;
    let mut got = Vec::new();
    while got.len() < want && Instant::now() < deadline {
        got.extend(h.renderer.poll_fence_timelines(0));
        std::thread::sleep(Duration::from_millis(2));
    }
    got
}

#[test]
fn the_executing_renderer_says_it_has_multiple_timelines_and_a_capture_does_not() {
    let host = Arc::new(FakeVulkan::standard());
    let h = Harness::new(host);
    assert!(h.renderer.capset_value().supports_multiple_timelines);
    let capture = crate::venus::renderer::CaptureSink::new();
    let renderer = crate::venus::renderer::VenusRenderer::new(capture.factory());
    assert!(!renderer.capset_value().supports_multiple_timelines);
}

#[test]
fn fences_on_two_timelines_retire_independently_and_in_order_within_each() {
    let host = Arc::new(FakeVulkan::standard());
    let mut h = Harness::new(Arc::clone(&host));
    let waker = two_queues(&mut h);
    // Timeline 2 is the second queue (host handle 1), whose GPU never
    // finishes until released.
    host.stick_queue(1);
    let fence = |h: &mut Harness<FakeVulkan>, r: u8, id: u32| {
        let (timeline, outcome) = h.renderer.create_fence_on(CTX, Some(r), id).unwrap();
        assert_eq!((timeline, outcome), (ring(r), FenceOutcome::Pending));
    };
    fence(&mut h, 1, 10);
    fence(&mut h, 2, 20);
    fence(&mut h, 1, 11);
    fence(&mut h, 2, 21);
    fence(&mut h, 1, 12);
    let first = collect(&mut h, 3, Duration::from_secs(5));
    assert_eq!(first, vec![(ring(1), 10), (ring(1), 11), (ring(1), 12)]);
    // Timeline 2 retires nothing while its queue is busy.
    assert!(collect(&mut h, 1, Duration::from_millis(150)).is_empty());
    assert!(h.renderer.snapshot_refusal().unwrap().contains("fences"));
    host.release_queue(1);
    let second = collect(&mut h, 2, Duration::from_secs(5));
    assert_eq!(second, vec![(ring(2), 20), (ring(2), 21)]);
    assert!(
        waker.0.load(Ordering::SeqCst) >= 5,
        "every retirement woke the device"
    );
    // Every host fence the timelines used is gone.
    assert_eq!(host.live("VkFence"), 0);
    let refusal = h.renderer.snapshot_refusal().unwrap_or_default();
    assert!(!refusal.contains("fences"), "{refusal}");
}

#[test]
fn ring_0_and_a_fence_without_a_ring_are_signalled_and_an_unbound_ring_is_refused() {
    let host = Arc::new(FakeVulkan::standard());
    let mut h = Harness::new(Arc::clone(&host));
    two_queues(&mut h);
    assert_eq!(
        h.renderer.create_fence_on(CTX, Some(0), 1).unwrap(),
        (ring(0), FenceOutcome::Signalled),
        "the CPU timeline: the context commands before it are done"
    );
    assert_eq!(
        h.renderer.create_fence_on(CTX, None, 2).unwrap(),
        (FenceTimeline::Device, FenceOutcome::Signalled)
    );
    // No queue is bound to timeline 5: vkr refuses the fence, and the
    // device answers it at once.
    assert!(h.renderer.create_fence_on(CTX, Some(5), 3).is_err());
    assert_eq!(host.called("vkQueueSubmit"), 0, "nothing reached the host");
    assert!(!h.fatal(), "a refused fence does not end the context");
}

#[test]
fn without_a_waker_a_timeline_fence_is_signalled_at_once() {
    let host = Arc::new(FakeVulkan::standard());
    let mut h = Harness::new(Arc::clone(&host));
    with_device(&mut h);
    assert_eq!(
        h.renderer.create_fence_on(CTX, Some(1), 7).unwrap(),
        (ring(1), FenceOutcome::Signalled)
    );
}

#[test]
fn destroying_the_context_retires_its_pending_fences_after_the_device_is_idle() {
    let host = Arc::new(FakeVulkan::standard());
    let mut h = Harness::new(Arc::clone(&host));
    two_queues(&mut h);
    host.stick_queue(0);
    for id in [30, 31] {
        h.renderer.create_fence_on(CTX, Some(1), id).unwrap();
    }
    assert!(collect(&mut h, 1, Duration::from_millis(100)).is_empty());
    let start = Instant::now();
    h.renderer.ctx_destroy(CTX);
    assert!(
        start.elapsed() < Duration::from_secs(2),
        "joined within a slice"
    );
    // As vkr retires a queue's outstanding syncs when it goes.
    assert_eq!(
        h.renderer.poll_fence_timelines(0),
        vec![(ring(1), 30), (ring(1), 31)]
    );
    assert_eq!(host.live_objects(), 0);
}

#[test]
fn a_reset_with_fences_pending_joins_every_fence_thread_and_forgets_them() {
    let host = Arc::new(FakeVulkan::standard());
    let mut h = Harness::new(Arc::clone(&host));
    two_queues(&mut h);
    host.stick_queue(0);
    host.stick_queue(1);
    h.renderer.create_fence_on(CTX, Some(1), 40).unwrap();
    h.renderer.create_fence_on(CTX, Some(2), 41).unwrap();
    assert!(h.renderer.snapshot_refusal().unwrap().contains("fences"));
    let start = Instant::now();
    h.renderer.reset();
    assert!(start.elapsed() < Duration::from_secs(2));
    // The device drops its held responses on reset; a stale retirement of
    // the old boot must not complete a new one's fence.
    assert!(h.renderer.poll_fence_timelines(0).is_empty());
    assert_eq!(h.renderer.factory().pending_ring_fences(), 0);
    assert_eq!(host.live_objects(), 0);
    assert_eq!(h.renderer.snapshot_refusal(), None);
}

#[test]
fn a_fence_retires_while_the_vm_is_paused_and_a_paused_reset_still_joins() {
    let host = Arc::new(FakeVulkan::standard());
    let quiesce = virtio_core::Quiesce::new();
    let mut h = Harness::gated(Arc::clone(&host), Arc::clone(&quiesce));
    two_queues(&mut h);
    quiesce.pause();
    assert!(
        quiesce.wait_until_idle(Duration::from_secs(2)),
        "the ring parked"
    );
    // The fence thread touches no guest memory and takes no pass: its
    // retirement is only collected, and the device's worker is what parks.
    h.renderer.create_fence_on(CTX, Some(1), 50).unwrap();
    assert_eq!(
        collect(&mut h, 1, Duration::from_secs(5)),
        vec![(ring(1), 50)]
    );
    host.stick_queue(0);
    h.renderer.create_fence_on(CTX, Some(1), 51).unwrap();
    let start = Instant::now();
    h.renderer.reset();
    assert!(
        start.elapsed() < Duration::from_secs(2),
        "a paused reset joins"
    );
    assert_eq!(h.renderer.live_threads(), 0);
    assert_eq!(host.live_objects(), 0);
    quiesce.resume();
}

#[test]
fn a_timeline_fence_on_a_lost_device_is_signalled() {
    let host = Arc::new(FakeVulkan::standard());
    let mut h = Harness::new(Arc::clone(&host));
    two_queues(&mut h);
    host.lost.store(true, Ordering::SeqCst);
    assert_eq!(
        h.renderer.create_fence_on(CTX, Some(1), 60).unwrap(),
        (ring(1), FenceOutcome::Signalled)
    );
    assert_eq!(host.live("VkFence"), 0, "its host fence is gone");
}
