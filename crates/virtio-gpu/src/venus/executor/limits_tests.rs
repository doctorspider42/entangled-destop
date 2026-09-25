//! The executor's caps against a hostile guest (ADR-0004, the
//! resource-exhaustion amendment): every class exhausted answers what Vulkan
//! says, is given back when what it counts goes — explicitly, with its pool,
//! with its device, with its context, at a reset — and one context at a cap
//! costs another nothing. Then a lost device, met at several calls, ends its
//! own context only; a device whose GPU work never finishes is parked, not
//! waited for; and a decode past the pool ends its context without the pool
//! staying charged.

use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::renderer::Renderer3d;
use crate::venus::protocol::*;
use crate::venus::renderer::SinkFactory;

use super::fake::{self, FakeVulkan};
use super::harness::*;
use super::limits::{Caps, Class, LimitUsage};
use super::recording::*;
use super::ExecutorFactory;

const HOST_COHERENT_TYPE: u32 = 3;
const DEVICE_LOCAL_TYPE: u32 = 1;
const MIB: u64 = 1 << 20;

fn harness(caps: Caps) -> (Harness<FakeVulkan>, Arc<FakeVulkan>) {
    let host = Arc::new(FakeVulkan::standard());
    let h = Harness::with_factory(ExecutorFactory::new(Arc::clone(&host)).with_caps(caps));
    (h, host)
}

/// Renderer-wide usage, and the most of one context.
fn usage(h: &Harness<FakeVulkan>) -> LimitUsage {
    h.renderer.factory().usage().limits
}

/// Context `ctx`'s own usage.
fn context_usage(h: &Harness<FakeVulkan>, ctx: u32) -> LimitUsage {
    h.renderer
        .factory()
        .with_context(ctx, |c| c.objects.limits().usage())
        .expect("the context exists")
}

fn fence_ret(h: &mut Harness<FakeVulkan>, id: u64) -> i32 {
    let Command::CreateFence(f) = h.call(&create_fence(DEVICE, id, false)).expect("answered")
    else {
        panic!("wrong reply")
    };
    f.ret
}

fn alloc_ret(h: &mut Harness<FakeVulkan>, id: u64, size: u64, ty: u32) -> i32 {
    let Command::AllocateMemory(a) = h
        .call(&allocate(DEVICE, id, size, ty, Vec::new()))
        .expect("answered")
    else {
        panic!("wrong reply")
    };
    a.ret
}

fn create_query_pool(device: u64, id: u64, count: u32) -> Command<'static> {
    Command::CreateQueryPool(CreateQueryPoolArgs {
        device: VkDevice(device),
        p_create_info: Some(VkQueryPoolCreateInfo {
            flags: 0,
            query_type: 0,
            query_count: count,
            pipeline_statistics: 0,
        }),
        p_query_pool: Some(VkQueryPool(id)),
        ret: 0,
    })
}

fn destroy_pool(device: u64, pool: u64) -> Command<'static> {
    Command::DestroyCommandPool(DestroyCommandPoolArgs {
        device: VkDevice(device),
        command_pool: VkCommandPool(pool),
    })
}

fn destroy_descriptor_pool(device: u64, pool: u64) -> Command<'static> {
    Command::DestroyDescriptorPool(DestroyDescriptorPoolArgs {
        device: VkDevice(device),
        descriptor_pool: VkDescriptorPool(pool),
    })
}

fn destroy_device(device: u64) -> Command<'static> {
    Command::DestroyDevice(DestroyDeviceArgs {
        device: VkDevice(device),
    })
}

// --------------------------------------------------------------- counts

/// A class of objects refuses at its per-context cap with what a driver out
/// of room says, a destroy gives the room back, and the context lives on.
#[test]
fn a_counted_kind_is_refused_at_its_cap_and_given_back_on_destroy() {
    let (mut h, host) = harness(Caps::default().with(Class::Fences, 2, 8));
    with_device(&mut h);
    assert_eq!(fence_ret(&mut h, FENCE), VK_SUCCESS);
    assert_eq!(fence_ret(&mut h, FENCE + 1), VK_SUCCESS);
    assert_eq!(
        fence_ret(&mut h, FENCE + 2),
        VK_ERROR_OUT_OF_HOST_MEMORY,
        "past the context's two"
    );
    assert_eq!(
        host.live("VkFence"),
        2,
        "the refused one never reached the host"
    );
    assert_eq!(context_usage(&h, CTX).of(Class::Fences), 2);
    h.send(&destroy_fence(DEVICE, FENCE)).expect("destroy");
    assert_eq!(usage(&h).of(Class::Fences), 1);
    assert_eq!(fence_ret(&mut h, FENCE + 2), VK_SUCCESS, "room again");
    assert!(!h.fatal(), "a refusal is an answer, not an end");
}

/// `vkAllocateMemory` past the memory-object count answers
/// `VK_ERROR_TOO_MANY_OBJECTS` — `maxMemoryAllocationCount`'s answer — and a
/// device past its count, `VK_ERROR_OUT_OF_HOST_MEMORY`.
#[test]
fn memory_objects_and_devices_are_counted_with_their_own_answers() {
    let (mut h, _) = harness(Caps::default().with(Class::Memories, 2, 8).with(
        Class::Devices,
        1,
        8,
    ));
    with_device(&mut h);
    assert_eq!(
        alloc_ret(&mut h, 0x70, 4096, HOST_COHERENT_TYPE),
        VK_SUCCESS
    );
    assert_eq!(alloc_ret(&mut h, 0x71, 4096, DEVICE_LOCAL_TYPE), VK_SUCCESS);
    assert_eq!(
        alloc_ret(&mut h, 0x72, 4096, DEVICE_LOCAL_TYPE),
        VK_ERROR_TOO_MANY_OBJECTS
    );
    h.send(&free(DEVICE, 0x70)).expect("free");
    assert_eq!(alloc_ret(&mut h, 0x72, 4096, DEVICE_LOCAL_TYPE), VK_SUCCESS);

    let Command::CreateDevice(d) = h
        .call(&create_device(PHYSICAL, 0x31, Vec::new()))
        .expect("answered")
    else {
        panic!()
    };
    assert_eq!(d.ret, VK_ERROR_OUT_OF_HOST_MEMORY, "one device per context");
    assert!(!h.fatal());
}

/// The costs an object carries beyond its count: SPIR-V bytes, a
/// descriptor pool's capacity, a query pool's slots — each refused before
/// the host is asked, and given back when the object goes.
#[test]
fn shader_bytes_descriptors_and_query_slots_are_refused_and_given_back() {
    let (mut h, host) = harness(
        Caps::default()
            .with(Class::ShaderBytes, 4096, 1 << 20)
            .with(Class::Descriptors, 16, 1 << 20)
            .with(Class::Queries, 100, 1 << 20),
    );
    with_device(&mut h);
    let module = |h: &mut Harness<FakeVulkan>, id: u64, words: usize| {
        let Command::CreateShaderModule(m) = h
            .call(&create_shader_module(DEVICE, id, &vec![0x0723_0203; words]))
            .expect("answered")
        else {
            panic!()
        };
        m.ret
    };
    assert_eq!(module(&mut h, SHADER, 768), VK_SUCCESS, "3 KiB");
    assert_eq!(
        module(&mut h, SHADER + 1, 512),
        VK_ERROR_OUT_OF_HOST_MEMORY,
        "2 KiB more is past 4"
    );
    assert_eq!(usage(&h).of(Class::ShaderBytes), 3072);
    h.send(&Command::DestroyShaderModule(DestroyShaderModuleArgs {
        device: VkDevice(DEVICE),
        shader_module: VkShaderModule(SHADER),
    }))
    .expect("destroy");
    assert_eq!(usage(&h).of(Class::ShaderBytes), 0);
    assert_eq!(module(&mut h, SHADER + 1, 512), VK_SUCCESS);

    // A pool of 4 sets and 4 descriptors costs 8.
    let pool = |h: &mut Harness<FakeVulkan>, id: u64| {
        let Command::CreateDescriptorPool(p) = h
            .call(&create_descriptor_pool(DEVICE, id, 0))
            .expect("answered")
        else {
            panic!()
        };
        p.ret
    };
    assert_eq!(pool(&mut h, DESC_POOL), VK_SUCCESS);
    assert_eq!(pool(&mut h, DESC_POOL + 1), VK_SUCCESS);
    assert_eq!(pool(&mut h, DESC_POOL + 2), VK_ERROR_OUT_OF_DEVICE_MEMORY);
    assert_eq!(host.live("VkDescriptorPool"), 2);
    h.send(&destroy_descriptor_pool(DEVICE, DESC_POOL))
        .expect("destroy");
    assert_eq!(pool(&mut h, DESC_POOL + 2), VK_SUCCESS);

    let query = |h: &mut Harness<FakeVulkan>, id: u64, count: u32| {
        let Command::CreateQueryPool(q) = h
            .call(&create_query_pool(DEVICE, id, count))
            .expect("answered")
        else {
            panic!()
        };
        q.ret
    };
    assert_eq!(query(&mut h, 0xe0, 64), VK_SUCCESS);
    assert_eq!(query(&mut h, 0xe1, 64), VK_ERROR_OUT_OF_DEVICE_MEMORY);
    assert_eq!(usage(&h).of(Class::Queries), 64);
    assert!(!h.fatal());
}

/// The table's own bound, and the renderer-wide one behind every share.
#[test]
fn the_object_table_is_bounded_per_context_and_renderer_wide() {
    let (mut h, _) = harness(Caps::default().with(Class::Objects, 10, 16));
    with_device(&mut h);
    // Instance, physical device, device, pool, queue: 5 held.
    let mut made = 0;
    while fence_ret(&mut h, FENCE + made) == VK_SUCCESS {
        made += 1;
    }
    assert_eq!(made, 5, "ten in all");
    assert_eq!(context_usage(&h, CTX).of(Class::Objects), 10);

    h.use_context(2);
    with_device(&mut h);
    let mut made = 0;
    while fence_ret(&mut h, FENCE + made) == VK_SUCCESS {
        made += 1;
    }
    assert_eq!(made, 1, "the renderer's sixteen are spent, not the share");
    assert_eq!(usage(&h).of(Class::Objects), 16);
    assert!(!h.fatal());
}

/// One context at its share takes nothing from another's.
#[test]
fn a_context_at_a_cap_does_not_affect_another() {
    let (mut h, _) = harness(Caps::default().with(Class::Fences, 2, 64));
    with_device(&mut h);
    for i in 0..2 {
        assert_eq!(fence_ret(&mut h, FENCE + i), VK_SUCCESS);
    }
    assert_eq!(fence_ret(&mut h, FENCE + 2), VK_ERROR_OUT_OF_HOST_MEMORY);

    h.use_context(2);
    with_device(&mut h);
    for i in 0..2 {
        assert_eq!(fence_ret(&mut h, FENCE + i), VK_SUCCESS, "its own share");
    }
    assert!(!h.fatal());
    h.use_context(CTX);
    assert!(!h.fatal(), "the first is refused, not ended");
    let u = usage(&h);
    assert_eq!((u.of(Class::Fences), u.context_max(Class::Fences)), (4, 2));
}

// ------------------------------------------------------------ recording

/// A command buffer's recording is charged by its wire size; beginning it
/// again gives that back, and a recording past the cap — whose commands
/// have no result to answer — ends the context.
#[test]
fn a_recording_past_its_cap_ends_the_context_and_a_new_begin_gives_it_back() {
    let (mut h, _) = harness(Caps::default().with(Class::RecordingBytes, 64 * 64, 1 << 20));
    with_device(&mut h);
    h.send(&create_buffer(DEVICE, BUFFER, buffer_info(4096, 0x2)))
        .expect("buffer");
    h.send(&allocate(
        DEVICE,
        MEMORY,
        4096,
        HOST_COHERENT_TYPE,
        Vec::new(),
    ))
    .expect("memory");
    h.send(&bind_buffers(DEVICE, &[(BUFFER, MEMORY, 0)]))
        .expect("bind");
    h.send(&allocate_cbs(DEVICE, POOL, &[CB], false))
        .expect("command buffer");
    let mut short = vec![begin(CB)];
    short.extend((0..10).map(|_| fill(CB, BUFFER, 0, 256, 7)));
    short.push(end(CB));
    assert!(matches!(h.submit_recording(&short), Outcome::Consumed));
    let held = usage(&h).of(Class::RecordingBytes);
    assert!(held >= 12 * 64, "{held}");
    // Begun again: what the old recording held is given back.
    assert!(matches!(
        h.submit_recording(&[begin(CB), end(CB)]),
        Outcome::Consumed
    ));
    assert!(usage(&h).of(Class::RecordingBytes) < held);

    let mut long = vec![begin(CB)];
    long.extend((0..100).map(|_| fill(CB, BUFFER, 0, 256, 7)));
    long.push(end(CB));
    assert!(matches!(h.submit_recording(&long), Outcome::Fatal { .. }));
    // The context is fatal; its table goes with it, and so do its charges.
    h.renderer.ctx_destroy(CTX);
    assert!(usage(&h).holds_nothing(), "{}", usage(&h));
}

// -------------------------------------------------------- implicit frees

/// Everything a pool frees with it, a device takes with it and a context
/// or a reset tears down gives back every charge: counts, costs and bytes.
#[test]
fn implicit_frees_give_back_every_charge() {
    let (mut h, host) = harness(Caps::default());
    with_device(&mut h);
    // Command buffers with their pool, their recordings with them.
    h.send(&create_pool(DEVICE, POOL + 1)).expect("pool");
    h.send(&allocate_cbs(DEVICE, POOL + 1, &[CB, CB + 1], false))
        .expect("command buffers");
    assert!(matches!(
        h.submit_recording(&[begin(CB), end(CB)]),
        Outcome::Consumed
    ));
    assert_eq!(usage(&h).of(Class::CommandBuffers), 2);
    assert!(usage(&h).of(Class::RecordingBytes) > 0);
    h.send(&destroy_pool(DEVICE, POOL + 1)).expect("destroy");
    let u = usage(&h);
    assert_eq!(
        (
            u.of(Class::CommandBuffers),
            u.of(Class::RecordingBytes),
            u.of(Class::CommandPools)
        ),
        (0, 0, 1),
        "the pool took its buffers and their recordings; POOL remains"
    );

    // Descriptor sets with their pool.
    h.send(&create_storage_set_layout(DEVICE, SET_LAYOUT, 0x20))
        .expect("layout");
    h.send(&create_descriptor_pool(DEVICE, DESC_POOL, 0))
        .expect("pool");
    let before = usage(&h).of(Class::Objects);
    h.send(&allocate_sets(
        DEVICE,
        DESC_POOL,
        SET_LAYOUT,
        &[SET, SET + 1],
    ))
    .expect("sets");
    assert_eq!(usage(&h).of(Class::Objects), before + 2);
    assert_eq!(usage(&h).of(Class::Descriptors), 8);
    h.send(&destroy_descriptor_pool(DEVICE, DESC_POOL))
        .expect("destroy");
    assert_eq!(usage(&h).of(Class::Objects), before - 1);
    assert_eq!(usage(&h).of(Class::Descriptors), 0);

    // Everything with its device: objects of every class, memory of both
    // budgets, a recording.
    h.send(&create_shader_module(DEVICE, SHADER, &[0x0723_0203; 64]))
        .expect("module");
    h.send(&create_query_pool(DEVICE, 0xe0, 32))
        .expect("queries");
    h.send(&create_fence(DEVICE, FENCE, false)).expect("fence");
    h.send(&allocate(
        DEVICE,
        0x71,
        8 * MIB,
        DEVICE_LOCAL_TYPE,
        Vec::new(),
    ))
    .expect("device-local");
    h.send(&allocate(DEVICE, 0x72, MIB, HOST_COHERENT_TYPE, Vec::new()))
        .expect("host-visible");
    h.send(&allocate_cbs(DEVICE, POOL, &[CB + 2], false))
        .expect("command buffer");
    assert!(matches!(
        h.submit_recording(&[begin(CB + 2), end(CB + 2)]),
        Outcome::Consumed
    ));
    let u = usage(&h);
    assert_eq!(u.device_local_bytes, 8 * MIB);
    assert!(u.of(Class::ShaderBytes) > 0 && u.of(Class::Queries) > 0);
    h.send(&destroy_device(DEVICE)).expect("destroy the device");
    let u = usage(&h);
    assert_eq!(
        u.of(Class::Objects),
        2,
        "the instance and its physical device are all that is left"
    );
    for class in super::limits::CLASSES {
        if class != Class::Objects {
            assert_eq!(u.of(class), 0, "{class:?}");
        }
    }
    assert_eq!(u.device_local_bytes, 0);
    assert_eq!(h.renderer.factory().host_visible_bytes(), 0);

    // With the context: everything.
    h.renderer.ctx_destroy(CTX);
    assert!(usage(&h).holds_nothing(), "{}", usage(&h));
    assert_eq!(host.live_objects(), 0);

    // At a reset: everything, every context.
    h.use_context(2);
    with_device(&mut h);
    h.send(&allocate(DEVICE, 0x71, MIB, DEVICE_LOCAL_TYPE, Vec::new()))
        .expect("device-local");
    h.use_context(3);
    with_device(&mut h);
    h.send(&create_fence(DEVICE, FENCE, false)).expect("fence");
    h.renderer.reset();
    assert!(usage(&h).holds_nothing(), "{}", usage(&h));
}

// ---------------------------------------------------- device-local memory

/// Device-local memory is charged per heap of the GPU: a context is refused
/// at its share, the renderer at its whole, `VK_ERROR_OUT_OF_DEVICE_MEMORY`
/// either way; the guest is told the heap is its share; freeing gives it
/// back. A profile may set the whole (`[display] gpu_memory_mib`).
#[test]
fn device_local_memory_is_refused_at_the_share_and_the_whole_and_given_back() {
    let host = Arc::new(FakeVulkan::standard());
    let mut h = Harness::with_factory(
        ExecutorFactory::new(Arc::clone(&host)).with_gpu_memory(Some(64 * MIB)),
    );
    with_device(&mut h);
    let Command::GetPhysicalDeviceMemoryProperties2(m) =
        h.call(&memory_properties(PHYSICAL)).expect("answered")
    else {
        panic!()
    };
    let told = m.p_memory_properties.expect("filled").memory_properties;
    assert_eq!(told.memory_heaps[0].size, 48 * MIB, "three quarters of 64");

    assert_eq!(
        alloc_ret(&mut h, 0x70, 32 * MIB, DEVICE_LOCAL_TYPE),
        VK_SUCCESS
    );
    assert_eq!(
        alloc_ret(&mut h, 0x71, 16 * MIB, DEVICE_LOCAL_TYPE),
        VK_SUCCESS
    );
    assert_eq!(
        alloc_ret(&mut h, 0x72, MIB, DEVICE_LOCAL_TYPE),
        VK_ERROR_OUT_OF_DEVICE_MEMORY,
        "past the share"
    );
    // The BAR type is the same heap on the fake: the same share.
    assert_eq!(
        alloc_ret(&mut h, 0x73, MIB, 5),
        VK_ERROR_OUT_OF_DEVICE_MEMORY
    );
    assert_eq!(
        host.allocations().len(),
        2,
        "the refused never reached the host"
    );

    h.use_context(2);
    with_device(&mut h);
    assert_eq!(
        alloc_ret(&mut h, 0x70, 16 * MIB, DEVICE_LOCAL_TYPE),
        VK_SUCCESS
    );
    assert_eq!(
        alloc_ret(&mut h, 0x71, MIB, DEVICE_LOCAL_TYPE),
        VK_ERROR_OUT_OF_DEVICE_MEMORY,
        "inside its share, past the whole"
    );
    let u = usage(&h);
    assert_eq!(
        (u.device_local_bytes, u.max_context_device_local_bytes),
        (64 * MIB, 48 * MIB)
    );

    h.use_context(CTX);
    h.send(&free(DEVICE, 0x70)).expect("free");
    h.use_context(2);
    assert_eq!(
        alloc_ret(&mut h, 0x71, 16 * MIB, DEVICE_LOCAL_TYPE),
        VK_SUCCESS
    );
    assert_eq!(usage(&h).device_local_bytes, 48 * MIB);
    assert!(!h.fatal());
}

/// Host RAM a driver allocates for a type the guest cannot map — a heap
/// that is not device local — is charged to the host-visible share, as our
/// pages are, and the guest is told that heap is the share.
#[test]
fn plain_system_memory_is_charged_to_the_host_visible_share() {
    let mut gpu = fake::gpu("NVIDIA GeForce RTX 2070");
    let types = &mut gpu.info.memory;
    types.memory_types[6] = VkMemoryType {
        property_flags: 0,
        heap_index: 1,
    };
    types.memory_type_count = 7;
    let host = Arc::new(FakeVulkan::new(vec![gpu]));
    let mut h = Harness::with_factory(ExecutorFactory::with_budgets(
        Arc::clone(&host),
        8 * MIB,
        4 * MIB,
    ));
    with_device(&mut h);
    assert_eq!(alloc_ret(&mut h, 0x70, 3 * MIB, 6), VK_SUCCESS);
    assert_eq!(
        alloc_ret(&mut h, 0x71, 2 * MIB, HOST_COHERENT_TYPE),
        VK_ERROR_OUT_OF_DEVICE_MEMORY,
        "our pages and the driver's host RAM share one budget"
    );
    assert_eq!(h.renderer.factory().host_visible_bytes(), 3 * MIB);
    h.send(&free(DEVICE, 0x70)).expect("free");
    assert_eq!(h.renderer.factory().host_visible_bytes(), 0);
}

// ------------------------------------------------------------ lost devices

/// Two contexts, each with a device of its own; the second's device is lost
/// at `call`, which `run` then makes. The command is answered — `head`
/// moves past it — the second context ends, and the first carries on; the
/// second's charges go with it, and nothing panics.
fn lost_at(
    call: &str,
    run: impl FnOnce(&mut Harness<FakeVulkan>) -> Result<Command<'static>, u32>,
) {
    let (mut h, host) = harness(Caps::default());
    with_device(&mut h);
    assert_eq!(fence_ret(&mut h, FENCE), VK_SUCCESS);
    let first = context_usage(&h, CTX);

    h.use_context(2);
    with_device(&mut h);
    h.send(&create_buffer(DEVICE, BUFFER, buffer_info(4096, 0x2)))
        .expect("buffer");
    h.send(&allocate(
        DEVICE,
        MEMORY,
        4096,
        HOST_COHERENT_TYPE,
        Vec::new(),
    ))
    .expect("memory");
    h.send(&create_fence(DEVICE, FENCE, true)).expect("fence");
    let lost_device = *host.device_handles().last().expect("two devices");
    host.lose_at(call);
    match run(&mut h) {
        Err(head) => assert_eq!(head, h.tail(), "{call}: answered, then fatal"),
        Ok(_) => panic!("{call}: the context went on after a lost device"),
    }
    assert!(host.device_lost(lost_device), "{call} reached the host");
    assert!(h.fatal(), "{call}");

    h.use_context(CTX);
    assert!(!h.fatal(), "{call}: the other context is untouched");
    assert_eq!(fence_ret(&mut h, FENCE + 1), VK_SUCCESS, "{call}");
    h.renderer.ctx_destroy(2);
    let after = usage(&h);
    assert_eq!(
        after.of(Class::Objects),
        first.of(Class::Objects) + 1,
        "{call}: only the first context's objects are held"
    );
    assert_eq!(after.of(Class::Devices), 1, "{call}");
}

#[test]
fn a_lost_device_ends_only_its_own_context_wherever_the_driver_says_so() {
    lost_at("vkQueueSubmit", |h| h.call(&queue_submit(QUEUE, &[], 0)));
    lost_at("vkWaitForFences", |h| {
        h.call(&wait_fences(DEVICE, &[FENCE], u64::MAX))
    });
    lost_at("vkAllocateMemory", |h| {
        h.call(&allocate(DEVICE, 0x77, 4096, DEVICE_LOCAL_TYPE, Vec::new()))
    });
    lost_at("vkCreateBuffer", |h| {
        h.call(&create_buffer(DEVICE, 0x78, buffer_info(4096, 0x2)))
    });
    lost_at("vkBindBufferMemory2", |h| {
        h.call(&bind_buffers(DEVICE, &[(BUFFER, MEMORY, 0)]))
    });
    lost_at("vkCreateImage", |h| {
        h.call(&create_image(DEVICE, 0x79, image_info()))
    });
    lost_at("vkCreateFence", |h| {
        h.call(&create_fence(DEVICE, 0x7a, false))
    });
    lost_at("vkCreateShaderModule", |h| {
        h.call(&create_shader_module(DEVICE, 0x7b, &[0x0723_0203; 16]))
    });
}

// ---------------------------------------------------------------- parking

/// A context whose GPU work never finishes — a queue waiting on a timeline
/// value nothing will signal — is not waited for when it goes: its devices
/// are parked, still charged, and destroyed once their work is done.
#[test]
fn a_context_whose_gpu_work_never_finishes_is_parked_not_waited_for() {
    let (mut h, host) = harness(Caps::default());
    with_device(&mut h);
    h.use_context(2);
    with_device(&mut h);
    h.send(&allocate(
        DEVICE,
        0x71,
        4 * MIB,
        DEVICE_LOCAL_TYPE,
        Vec::new(),
    ))
    .expect("device-local");
    let wedged = *host.device_handles().last().expect("two devices");
    host.wedge_device(wedged);

    let started = Instant::now();
    h.renderer.ctx_destroy(2);
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "teardown is bounded"
    );
    let factory = h.renderer.factory();
    assert_eq!(factory.usage().parked_devices, 1);
    let u = usage(&h);
    assert_eq!(
        u.of(Class::Devices),
        2,
        "the parked device is still charged"
    );
    assert_eq!(u.device_local_bytes, 4 * MIB);
    assert_eq!(host.live("device"), 2, "nothing destroyed under the GPU");
    assert!(h.renderer.snapshot_refusal().is_some());

    // The other context is untouched.
    h.use_context(CTX);
    assert_eq!(fence_ret(&mut h, FENCE), VK_SUCCESS);

    host.unwedge_device(wedged);
    let factory = h.renderer.factory();
    assert_eq!(factory.usage().parked_devices, 0, "the next look reaps it");
    let u = usage(&h);
    assert_eq!((u.of(Class::Devices), u.device_local_bytes), (1, 0));
    assert_eq!(host.live("device"), 1);
}

// ----------------------------------------------------------------- decode

/// A command whose decode would take its context past its share of the
/// decode pool ends that context — without the pool staying charged, and
/// without costing another context anything.
#[test]
fn a_decode_past_the_pool_ends_its_context_and_gives_the_pool_back() {
    let (mut h, _) = harness(Caps::default().with(Class::DecodeBytes, 16 << 10, 64 << 10));
    with_device(&mut h);
    let Command::CreateShaderModule(m) = h
        .call(&create_shader_module(DEVICE, SHADER, &[0x0723_0203; 1024]))
        .expect("4 KiB decodes")
    else {
        panic!()
    };
    assert_eq!(m.ret, VK_SUCCESS);
    // A decode lives a millisecond; the high-water mark still saw it.
    let high = usage(&h).high[Class::DecodeBytes.index()];
    assert!((4096..16 << 10).contains(&high), "{high}");
    h.use_context(2);
    with_device(&mut h);
    assert!(
        h.send(&create_shader_module(DEVICE, SHADER, &[0x0723_0203; 8192]))
            .is_err(),
        "32 KiB of SPIR-V is past a 16 KiB share"
    );
    assert_eq!(usage(&h).of(Class::DecodeBytes), 0, "nothing stays charged");
    h.use_context(CTX);
    assert!(!h.fatal());
    let Command::CreateShaderModule(m) = h
        .call(&create_shader_module(
            DEVICE,
            SHADER + 1,
            &[0x0723_0203; 1024],
        ))
        .expect("the first context still decodes")
    else {
        panic!()
    };
    assert_eq!(m.ret, VK_SUCCESS);
}

/// The copy of a command stream is charged to the pool too.
#[test]
fn a_command_stream_past_the_pool_is_refused_before_it_is_copied() {
    let (mut h, _) = harness(Caps::default().with(Class::DecodeBytes, 16 << 10, 64 << 10));
    with_device(&mut h);
    let stream = vec![0u8; 32 << 10];
    assert!(matches!(
        h.submit_indirect(&[&stream]),
        Outcome::Fatal { .. }
    ));
    assert_eq!(usage(&h).of(Class::DecodeBytes), 0);
}

/// What a hostile stream can make one command's decode allocate: at most
/// the per-command bound, however large the count it declares — a count the
/// stream cannot hold costs nothing, and a real one past the bound is
/// refused at the bound, with the pool given back.
#[test]
fn one_commands_decode_is_bounded_whatever_count_it_declares() {
    use crate::venus::wire::{Decoder, WireError};

    let pool = crate::venus::shmem::PageBudget::new(1 << 30);
    let code = vec![0x0723_0203u32; 16 << 10];
    let bytes = call_bytes(&create_shader_module(DEVICE, SHADER, &code));
    {
        let mut dec = Decoder::with_pool(&bytes, super::limits::MAX_COMMAND_DECODE_BYTES, &pool);
        Command::decode_next(&mut dec).expect("64 KiB of SPIR-V decodes");
        let peak = dec.peak_alloc();
        assert!((64 << 10..(64 << 10) + 4096).contains(&peak), "{peak}");
        assert_eq!(pool.used(), peak as u64, "held while the command runs");
    }
    assert_eq!(pool.used(), 0);

    // The same command claiming 2^40 words of code behind its 64 KiB: both
    // the size and the array length on the wire say so.
    let mut lying = bytes.clone();
    for claimed in [64u64 << 10, 16 << 10] {
        let at = lying
            .windows(8)
            .position(|w| w == claimed.to_le_bytes())
            .expect("the size and the length on the wire");
        lying[at..at + 8].copy_from_slice(&(1u64 << 40).to_le_bytes());
    }
    let mut dec = Decoder::with_pool(&lying, super::limits::MAX_COMMAND_DECODE_BYTES, &pool);
    assert!(Command::decode_next(&mut dec).is_err());
    assert!(dec.peak_alloc() < 4096, "{}", dec.peak_alloc());
    drop(dec);

    // A per-command bound below the real code: refused at the bound.
    let mut dec = Decoder::with_pool(&bytes, 32 << 10, &pool);
    let err = Command::decode_next(&mut dec).expect_err("past 32 KiB");
    assert!(
        matches!(
            err,
            ProtocolError::Wire(WireError::AllocationBudgetExhausted { .. })
        ),
        "{err}"
    );
    assert!(dec.peak_alloc() <= 32 << 10);
    drop(dec);
    assert_eq!(pool.used(), 0);
}
