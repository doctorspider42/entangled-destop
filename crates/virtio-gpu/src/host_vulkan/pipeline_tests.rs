//! Stage 5b.2 on the host's real GPU, driven as the guest drives it: every
//! command encoded by the generated **driver-side** encoder, recordings sent
//! as one ring submission (or through `vkExecuteCommandStreamsMESA` past the
//! ring's direct size), fences waited for the way Mesa 26.0.8 does — an
//! asynchronous `vkWaitForFences(UINT64_MAX)` — and every result read back
//! through the blob pages the guest maps. The shaders are vk-smoke's own
//! WGSL, compiled with naga here, and the triangle is checked against
//! vk-smoke's CPU reference pixel for pixel. Self-skipping like the rest of
//! `host_vulkan`'s tests.

use std::sync::Arc;

use super::tests::{host, with_device_on};
use super::AshVulkan;
use crate::venus::executor::harness::*;
use crate::venus::executor::recording::*;
use crate::venus::protocol::*;
use crate::venus::shmem::RingPages;

#[path = "../../../../guest/vk-smoke/src/raster.rs"]
#[allow(dead_code, clippy::all)]
mod raster;

const COMPUTE_WGSL: &str = include_str!("../../../../guest/vk-smoke/shaders/compute.wgsl");
const TRIANGLE_WGSL: &str = include_str!("../../../../guest/vk-smoke/shaders/triangle.wgsl");

/// WGSL to SPIR-V words, exactly as vk-smoke's build.rs compiles it (SPIR-V
/// 1.0, no coordinate-space adjustment).
fn spirv(source: &str) -> Vec<u32> {
    let module = naga::front::wgsl::parse_str(source).expect("the WGSL parses");
    let info = naga::valid::Validator::new(
        naga::valid::ValidationFlags::all(),
        naga::valid::Capabilities::empty(),
    )
    .validate(&module)
    .expect("the WGSL validates");
    let options = naga::back::spv::Options {
        lang_version: (1, 0),
        flags: naga::back::spv::WriterFlags::empty(),
        ..naga::back::spv::Options::default()
    };
    naga::back::spv::write_vec(&module, &info, &options, None).expect("SPIR-V")
}

const MEM_PROPERTY_DEVICE_LOCAL: u32 = 0x1;
const MEM_PROPERTY_HOST_COHERENT: u32 = 0x2 | 0x4;
const USAGE_TRANSFER_SRC: u32 = 0x1;
const USAGE_TRANSFER_DST: u32 = 0x2;
const USAGE_STORAGE: u32 = 0x20;
const USAGE_VERTEX: u32 = 0x80;

/// A buffer the test made, and — when it is host-visible — the blob pages
/// the guest would map it through.
struct Buf {
    id: u64,
    pages: Option<Arc<RingPages>>,
}

impl Buf {
    fn pages(&self) -> &RingPages {
        self.pages.as_ref().expect("a host-visible buffer")
    }

    fn write_words(&self, words: &[u32]) {
        let bytes: Vec<u8> = words.iter().flat_map(|w| w.to_le_bytes()).collect();
        self.pages()
            .write_bytes(0, &bytes)
            .expect("inside the blob");
    }

    fn read_words(&self, n: usize) -> Vec<u32> {
        let mut bytes = vec![0u8; n * 4];
        self.pages()
            .read_bytes(0, &mut bytes)
            .expect("inside the blob");
        bytes
            .chunks_exact(4)
            .map(|w| u32::from_le_bytes([w[0], w[1], w[2], w[3]]))
            .collect()
    }
}

/// The memory types of the device, as the guest sees them.
fn memory_types(h: &mut Harness<AshVulkan>) -> VkPhysicalDeviceMemoryProperties {
    let Command::GetPhysicalDeviceMemoryProperties2(m) =
        h.call(&memory_properties(PHYSICAL)).unwrap()
    else {
        panic!()
    };
    m.p_memory_properties.unwrap().memory_properties
}

fn pick_type(types: &VkPhysicalDeviceMemoryProperties, bits: u32, want: u32) -> u32 {
    (0..types.memory_type_count)
        .find(|i| {
            bits & (1 << i) != 0 && types.memory_types[*i as usize].property_flags & want == want
        })
        .unwrap_or_else(|| panic!("no memory type with {want:#x} in {bits:#x}"))
}

/// A buffer of `size` bytes: host-visible and coherent, bound, with its
/// blob made and its pages at hand — or device-local.
fn buffer(h: &mut Harness<AshVulkan>, id: u64, size: u64, usage: u32, host_visible: bool) -> Buf {
    let types = memory_types(h);
    let Command::CreateBuffer(b) = h
        .call(&create_buffer(DEVICE, id, buffer_info(size, usage)))
        .unwrap()
    else {
        panic!()
    };
    assert_eq!(b.ret, VK_SUCCESS);
    let Command::GetBufferMemoryRequirements2(r) =
        h.call(&buffer_requirements(DEVICE, id)).unwrap()
    else {
        panic!()
    };
    let req = r.p_memory_requirements.unwrap().memory_requirements;
    let want = if host_visible {
        MEM_PROPERTY_HOST_COHERENT
    } else {
        MEM_PROPERTY_DEVICE_LOCAL
    };
    let ty = pick_type(&types, req.memory_type_bits, want);
    let memory = id | 0x1000;
    h.send(&allocate(DEVICE, memory, req.size, ty, Vec::new()))
        .unwrap();
    h.send(&bind_buffers(DEVICE, &[(id, memory, 0)])).unwrap();
    let pages = host_visible.then(|| {
        let res = u32::try_from(id).unwrap() + 100;
        h.memory_blob(CTX, res, memory, req.size.next_multiple_of(4096))
            .expect("a blob of the memory");
        h.renderer.blob_pages(res).expect("the blob's pages")
    });
    Buf { id, pages }
}

/// Boot, a device, its pool and queue, and a command buffer.
fn setup(host: Arc<AshVulkan>) -> Harness<AshVulkan> {
    let mut h = Harness::new(host);
    boot(&mut h);
    with_device_on(&mut h);
    h.send(&allocate_cbs(DEVICE, POOL, &[CB], false)).unwrap();
    assert!(!h.fatal());
    h
}

/// Submit `cbs` with a fresh fence, wait for it the way Mesa does, and check
/// the host agrees it is signalled.
fn submit_and_wait(h: &mut Harness<AshVulkan>, cbs: &[u64], fence: u64) {
    h.send(&create_fence(DEVICE, fence, false)).unwrap();
    h.send(&queue_submit(QUEUE, cbs, fence)).unwrap();
    h.send(&wait_fences(DEVICE, &[fence], u64::MAX)).unwrap();
    let Command::GetFenceStatus(s) = h.call(&fence_status(DEVICE, fence)).unwrap() else {
        panic!()
    };
    assert_eq!(
        s.ret, VK_SUCCESS,
        "the fence is signalled once the wait returned"
    );
    assert!(!h.fatal());
}

fn teardown(mut h: Harness<AshVulkan>) {
    h.send(&Command::DestroyInstance(DestroyInstanceArgs {
        instance: VkInstance(INSTANCE),
    }))
    .unwrap();
    assert_eq!(h.renderer.factory().host_objects(), 0);
    assert!(!h.fatal());
}

/// vk-smoke check 4 plus Mesa's fence feedback: fills, an update and a
/// two-region copy recorded as one submission, the fence's feedback command
/// buffer appended to the batch as Mesa appends it, and every word read back
/// through the blob — the destination's and the feedback slot's.
#[test]
fn the_host_gpu_fills_updates_and_copies_and_the_guest_reads_every_word() {
    const SIZE: u64 = 64 * 1024;
    const WORDS: usize = (SIZE / 4) as usize;
    const HALF: u64 = SIZE / 2;
    const UPDATE_AT: usize = 15000;
    const FB_CB: u64 = CB + 1;
    let Some(host) = host() else { return };
    let mut h = setup(host);
    let src = buffer(
        &mut h,
        0x200,
        SIZE,
        USAGE_TRANSFER_SRC | USAGE_TRANSFER_DST,
        false,
    );
    let dst = buffer(&mut h, 0x210, SIZE, USAGE_TRANSFER_DST, true);
    dst.write_words(&vec![0xdddd_dddd; WORDS]);
    // The feedback buffer: Mesa's 4096-byte pool buffer, the fence's slot
    // written VK_NOT_READY by the guest when the fence was made.
    let feedback = buffer(
        &mut h,
        0x220,
        4096,
        USAGE_TRANSFER_SRC | USAGE_TRANSFER_DST,
        true,
    );
    feedback.write_words(&[1]);

    static UPDATE: [u8; 64] = {
        let mut bytes = [0u8; 64];
        let mut i = 0;
        while i < 16 {
            let w = (0x7700_0000u32 | i as u32).to_le_bytes();
            bytes[i * 4] = w[0];
            bytes[i * 4 + 1] = w[1];
            bytes[i * 4 + 2] = w[2];
            bytes[i * 4 + 3] = w[3];
            i += 1;
        }
        bytes
    };
    let tw = (STAGE_TRANSFER, ACCESS_TRANSFER_WRITE);
    let tr = (STAGE_TRANSFER, ACCESS_TRANSFER_READ);
    let hr = (STAGE_HOST, ACCESS_HOST_READ);
    let outcome = h.submit_recording(&[
        begin(CB),
        fill(CB, src.id, 0, WHOLE_SIZE, 0x1111_1111),
        buffer_barrier(CB, src.id, tw, tw),
        fill(CB, src.id, 4096, 4096, 0xcafe_babe),
        fill(CB, src.id, 40960, 4096, 0x0bad_f00d),
        update(CB, src.id, (UPDATE_AT * 4) as u64, &UPDATE),
        buffer_barrier(CB, src.id, tw, tr),
        copy_buffer(CB, src.id, dst.id, &[(0, HALF, HALF), (HALF, 0, HALF)]),
        buffer_barrier(CB, dst.id, tw, hr),
        end(CB),
    ]);
    assert_eq!(outcome, Outcome::Consumed);
    // The fence's feedback command buffer, as `vn_feedback_cmd_record`
    // records it once when the fence is created.
    h.send(&allocate_cbs(DEVICE, POOL, &[FB_CB], false))
        .unwrap();
    let outcome = h.submit_recording(&[
        Command::BeginCommandBuffer(BeginCommandBufferArgs {
            command_buffer: VkCommandBuffer(FB_CB),
            p_begin_info: Some(VkCommandBufferBeginInfo::default()),
            ret: 0,
        }),
        buffer_barrier(FB_CB, feedback.id, (STAGE_ALL_COMMANDS, 0), tw),
        fill(FB_CB, feedback.id, 0, 4, 0),
        buffer_barrier(FB_CB, feedback.id, tw, hr),
        end(FB_CB),
    ]);
    assert_eq!(outcome, Outcome::Consumed);
    submit_and_wait(&mut h, &[CB, FB_CB], FENCE);

    let mut model = vec![0x1111_1111u32; WORDS];
    model[1024..2048].fill(0xcafe_babe);
    model[10240..11264].fill(0x0bad_f00d);
    for i in 0..16 {
        model[UPDATE_AT + i] = 0x7700_0000 | i as u32;
    }
    let half = WORDS / 2;
    let got = dst.read_words(WORDS);
    let wrong = (0..WORDS)
        .filter(|&i| {
            got[i]
                != if i < half {
                    model[i + half]
                } else {
                    model[i - half]
                }
        })
        .count();
    eprintln!(
        "transfer: {WORDS} words through the blob, {wrong} wrong; first {:#010x}, update[0] at {} = {:#010x}",
        got[0],
        UPDATE_AT - half,
        got[UPDATE_AT - half]
    );
    assert_eq!(wrong, 0);
    assert_eq!(
        feedback.read_words(1),
        vec![0],
        "the feedback slot reads VK_SUCCESS, as the guest polls it"
    );
    teardown(h);
}

/// vk-smoke check 5: vk-smoke's own compute shader writes f(i) into 1 Mi
/// elements of a storage buffer through a descriptor set.
#[test]
fn the_host_gpu_runs_the_compute_shader_and_every_element_is_right() {
    const N: u32 = 1 << 20;
    let Some(host) = host() else { return };
    let mut h = setup(host);
    let data = buffer(&mut h, 0x300, u64::from(N) * 4, USAGE_STORAGE, true);
    data.write_words(&vec![0xffff_ffff; N as usize]);
    compute_pipeline(&mut h, &spirv(COMPUTE_WGSL), data.id);
    let outcome = h.submit_recording(&[
        begin(CB),
        bind_pipeline(CB, 1, PIPELINE),
        bind_sets(CB, 1, PIPELINE_LAYOUT, &[SET]),
        dispatch(CB, N / 64),
        buffer_barrier(
            CB,
            data.id,
            (STAGE_COMPUTE, ACCESS_SHADER_WRITE),
            (STAGE_HOST, ACCESS_HOST_READ),
        ),
        end(CB),
    ]);
    assert_eq!(outcome, Outcome::Consumed);
    submit_and_wait(&mut h, &[CB], FENCE);
    let got = data.read_words(N as usize);
    let f = |i: u32| {
        let mut x = i.wrapping_mul(2_654_435_761);
        x ^= x >> 15;
        x.wrapping_add(i << 3).wrapping_add(0x9e37_79b9)
    };
    let wrong = (0..N).filter(|&i| got[i as usize] != f(i)).count();
    eprintln!(
        "compute: {N} elements, {wrong} wrong; [0] = {:#010x}, [N-1] = {:#010x}",
        got[0],
        got[N as usize - 1]
    );
    assert_eq!(wrong, 0);
    teardown(h);
}

/// Shader module, set layout, pool, set, write, pipeline layout and compute
/// pipeline, all async as Mesa sends them.
fn compute_pipeline(h: &mut Harness<AshVulkan>, code: &[u32], buffer: u64) {
    h.send(&create_shader_module(DEVICE, SHADER, code)).unwrap();
    h.send(&create_storage_set_layout(DEVICE, SET_LAYOUT, 0x20))
        .unwrap();
    h.send(&create_descriptor_pool(DEVICE, DESC_POOL, 0))
        .unwrap();
    h.send(&allocate_sets(DEVICE, DESC_POOL, SET_LAYOUT, &[SET]))
        .unwrap();
    h.send(&write_storage(DEVICE, SET, buffer)).unwrap();
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
    assert!(!h.fatal(), "every object was accepted");
}

/// A compute shader far larger than the ring's direct size: its
/// `vkCreateShaderModule` travels through `vkExecuteCommandStreamsMESA`, as
/// Mesa sends any command past 8 KiB, and the pipeline built from it runs.
#[test]
fn a_shader_too_large_for_the_ring_arrives_through_a_command_stream_and_runs() {
    const N: u32 = 4096;
    const STEPS: u32 = 3000;
    let Some(host) = host() else { return };
    let mut h = setup(host);
    let mut wgsl = String::from(
        "@group(0) @binding(0) var<storage, read_write> data: array<u32>;\n\
         @compute @workgroup_size(64)\n\
         fn main(@builtin(global_invocation_id) gid: vec3<u32>) {\n\
         let i = gid.x;\n\
         if (i >= arrayLength(&data)) { return; }\n\
         var x = i;\n",
    );
    for step in 0..STEPS {
        wgsl.push_str(&format!("x = (x ^ {}u) + {}u;\n", step * 7 + 1, step));
    }
    wgsl.push_str("data[i] = x;\n}\n");
    let code = spirv(&wgsl);
    let bytes = code.len() * 4;
    eprintln!("large shader: {bytes} bytes of SPIR-V");
    assert!(
        bytes > 4 * DIRECT_SIZE,
        "the module is past the direct size"
    );

    let data = buffer(&mut h, 0x300, u64::from(N) * 4, USAGE_STORAGE, true);
    data.write_words(&vec![0; N as usize]);
    // The module alone, the way `vn_ring_submit_command` sends a command
    // past `direct_size`: uploaded, and named by one stream.
    let module = async_bytes(&create_shader_module(DEVICE, SHADER, &code));
    assert_eq!(h.submit_indirect(&[&module]), Outcome::Consumed);
    h.send(&create_storage_set_layout(DEVICE, SET_LAYOUT, 0x20))
        .unwrap();
    h.send(&create_descriptor_pool(DEVICE, DESC_POOL, 0))
        .unwrap();
    h.send(&allocate_sets(DEVICE, DESC_POOL, SET_LAYOUT, &[SET]))
        .unwrap();
    h.send(&write_storage(DEVICE, SET, data.id)).unwrap();
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
    assert!(!h.fatal());
    let outcome = h.submit_recording(&[
        begin(CB),
        bind_pipeline(CB, 1, PIPELINE),
        bind_sets(CB, 1, PIPELINE_LAYOUT, &[SET]),
        dispatch(CB, N / 64),
        buffer_barrier(
            CB,
            data.id,
            (STAGE_COMPUTE, ACCESS_SHADER_WRITE),
            (STAGE_HOST, ACCESS_HOST_READ),
        ),
        end(CB),
    ]);
    assert_eq!(outcome, Outcome::Consumed);
    submit_and_wait(&mut h, &[CB], FENCE);
    let got = data.read_words(N as usize);
    let f = |i: u32| {
        let mut x = i;
        for step in 0..STEPS {
            x = (x ^ (step * 7 + 1)).wrapping_add(step);
        }
        x
    };
    let wrong = (0..N).filter(|&i| got[i as usize] != f(i)).count();
    eprintln!("large shader: {N} elements, {wrong} wrong");
    assert_eq!(wrong, 0);
    teardown(h);
}

/// vk-smoke check 6: the triangle through a classic render pass into a
/// 256×256 RGBA8 image, copied to a buffer, and every pixel compared with
/// vk-smoke's CPU reference.
#[test]
fn the_host_gpu_renders_the_triangle_and_every_pixel_matches_the_reference() {
    const SIZE: u32 = raster::SIZE;
    const IMG: u64 = 0x400;
    const IMG_MEM: u64 = 0x401;
    const VIEW: u64 = 0x402;
    let Some(host) = host() else { return };
    let mut h = setup(host);
    let types = memory_types(&mut h);

    // The colour image, in device-local memory.
    let info = VkImageCreateInfo {
        image_type: 1,
        format: RGBA8,
        extent: VkExtent3D {
            width: SIZE,
            height: SIZE,
            depth: 1,
        },
        mip_levels: 1,
        array_layers: 1,
        samples: 1,
        tiling: 0,
        usage: 0x10 | 0x1, // COLOR_ATTACHMENT | TRANSFER_SRC
        ..Default::default()
    };
    let Command::CreateImage(i) = h.call(&create_image(DEVICE, IMG, info)).unwrap() else {
        panic!()
    };
    assert_eq!(i.ret, VK_SUCCESS);
    let Command::GetImageMemoryRequirements2(r) =
        h.call(&memory_requirements(DEVICE, IMG)).unwrap()
    else {
        panic!()
    };
    let req = r.p_memory_requirements.unwrap().memory_requirements;
    let ty = pick_type(&types, req.memory_type_bits, MEM_PROPERTY_DEVICE_LOCAL);
    h.send(&allocate(DEVICE, IMG_MEM, req.size, ty, Vec::new()))
        .unwrap();
    h.send(&bind_image(DEVICE, IMG, IMG_MEM, 0)).unwrap();
    h.send(&create_image_view(DEVICE, VIEW, IMG, RGBA8))
        .unwrap();

    let vertices = buffer(&mut h, 0x410, 60, USAGE_VERTEX, true);
    vertices.write_words(&raster::vertex_data().map(f32::to_bits));
    let bytes = u64::from(SIZE * SIZE * 4);
    let readback = buffer(&mut h, 0x420, bytes, USAGE_TRANSFER_DST, true);
    readback.write_words(&vec![0xdead_beef; (bytes / 4) as usize]);

    h.send(&create_render_pass(DEVICE, RENDER_PASS, RGBA8))
        .unwrap();
    h.send(&create_framebuffer(
        DEVICE,
        FRAMEBUFFER,
        RENDER_PASS,
        VIEW,
        SIZE,
    ))
    .unwrap();
    h.send(&create_shader_module(DEVICE, SHADER, &spirv(TRIANGLE_WGSL)))
        .unwrap();
    h.send(&create_pipeline_layout(DEVICE, PIPELINE_LAYOUT, &[]))
        .unwrap();
    h.send(&create_triangle_pipeline(
        DEVICE,
        PIPELINE,
        SHADER,
        PIPELINE_LAYOUT,
        RENDER_PASS,
        SIZE,
    ))
    .unwrap();
    assert!(!h.fatal(), "every object was accepted");

    let outcome = h.submit_recording(&[
        begin(CB),
        begin_render_pass(CB, RENDER_PASS, FRAMEBUFFER, SIZE, raster::CLEAR_6),
        bind_pipeline(CB, 0, PIPELINE),
        bind_vertex_buffer(CB, vertices.id),
        draw(CB, 3),
        end_render_pass(CB),
        copy_image_to_buffer(CB, IMG, readback.id, SIZE),
        buffer_barrier(
            CB,
            readback.id,
            (STAGE_TRANSFER, ACCESS_TRANSFER_WRITE),
            (STAGE_HOST, ACCESS_HOST_READ),
        ),
        end(CB),
    ]);
    assert_eq!(outcome, Outcome::Consumed);
    submit_and_wait(&mut h, &[CB], FENCE);

    let mut got = vec![0u8; bytes as usize];
    readback.pages().read_bytes(0, &mut got).unwrap();
    match raster::verify(&got, raster::CLEAR_6) {
        Ok(detail) => eprintln!("graphics: {detail}"),
        Err(why) => panic!("the rendered image is wrong: {why}"),
    }
    teardown(h);
}

/// vk-smoke check 9, shortened: many separate submits, each with its own
/// fence and its own word, every fence waited for as Mesa does.
#[test]
fn many_fenced_submits_each_land_their_word() {
    const N: u64 = 200;
    let Some(host) = host() else { return };
    let mut h = setup(host);
    let words = buffer(&mut h, 0x500, N * 4, USAGE_TRANSFER_DST, true);
    words.write_words(&vec![0; N as usize]);
    let cbs: Vec<u64> = (0..N).map(|i| 0x1_0000 + i).collect();
    h.send(&allocate_cbs(DEVICE, POOL, &cbs, false)).unwrap();
    for (i, cb) in cbs.iter().enumerate() {
        let outcome = h.submit_recording(&[
            begin(*cb),
            fill(*cb, words.id, i as u64 * 4, 4, 0x1000_0000 | i as u32),
            buffer_barrier(
                *cb,
                words.id,
                (STAGE_TRANSFER, ACCESS_TRANSFER_WRITE),
                (STAGE_HOST, ACCESS_HOST_READ),
            ),
            end(*cb),
        ]);
        assert_eq!(outcome, Outcome::Consumed);
    }
    let fences: Vec<u64> = (0..N).map(|i| 0x2_0000 + i).collect();
    for fence in &fences {
        h.send(&create_fence(DEVICE, *fence, false)).unwrap();
    }
    let start = std::time::Instant::now();
    for (cb, fence) in cbs.iter().zip(&fences) {
        h.send(&queue_submit(QUEUE, &[*cb], *fence)).unwrap();
    }
    for fence in &fences {
        h.send(&wait_fences(DEVICE, &[*fence], u64::MAX)).unwrap();
    }
    eprintln!("{N} submits and waits in {:?}", start.elapsed());
    let got = words.read_words(N as usize);
    assert!(
        (0..N as usize).all(|i| got[i] == 0x1000_0000 | i as u32),
        "no word lost"
    );
    teardown(h);
}
