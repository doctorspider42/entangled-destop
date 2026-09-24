//! Checks 3 to 9. Each one is self-contained: it creates what it needs in a
//! [`Scope`], proves one thing, and destroys everything on the way out.
//!
//! A check returns `Ok(Out::Pass(detail))`, `Ok(Out::Skip(reason))`, or
//! `Err(diagnosis)` for a FAIL.

use std::time::Instant;

use ash::vk;

use crate::gpu::{
    buffer_barrier, result_name, Buf, DynRender, Gpu, Scope, Timeline, VkExt, HOST_READ,
    TRANSFER_READ, TRANSFER_WRITE,
};
use crate::raster;

pub enum Out {
    Pass(String),
    Skip(String),
}

const COMPUTE_SPV: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/compute.spv"));
const TRIANGLE_SPV: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/triangle.spv"));

const HV: vk::MemoryPropertyFlags = vk::MemoryPropertyFlags::from_raw(
    vk::MemoryPropertyFlags::HOST_VISIBLE.as_raw()
        | vk::MemoryPropertyFlags::HOST_COHERENT.as_raw(),
);
const NONE: vk::MemoryPropertyFlags = vk::MemoryPropertyFlags::empty();
const DEVICE_LOCAL: vk::MemoryPropertyFlags = vk::MemoryPropertyFlags::DEVICE_LOCAL;

fn ms(d: std::time::Duration) -> String {
    format!("{:.2} ms", d.as_secs_f64() * 1000.0)
}

/// The first `n` indices where `got` differs from `want(i)`, and how many do.
fn mismatches(got: &[u32], want: impl Fn(usize) -> u32) -> Option<String> {
    let bad: Vec<usize> = (0..got.len()).filter(|&i| got[i] != want(i)).collect();
    if bad.is_empty() {
        return None;
    }
    let shown: Vec<String> = bad
        .iter()
        .take(4)
        .map(|&i| format!("[{i}]=0x{:08x} want 0x{:08x}", got[i], want(i)))
        .collect();
    Some(format!(
        "{} of {} words wrong: {}",
        bad.len(),
        got.len(),
        shown.join(", ")
    ))
}

// ---------------------------------------------------------------------------
// 3. host-visible memory

fn pattern3(i: usize, seed: u32) -> u32 {
    (i as u32).wrapping_mul(0x9e37_79b1) ^ seed
}

pub fn host_memory(gpu: &Gpu) -> Result<Out, String> {
    const SIZE: u64 = 1 << 20;
    const WORDS: usize = (SIZE / 4) as usize;
    let dev = &gpu.device;
    let mut scope = Scope::new(gpu);
    let info = vk::BufferCreateInfo::default()
        .size(SIZE)
        .usage(vk::BufferUsageFlags::TRANSFER_SRC | vk::BufferUsageFlags::TRANSFER_DST)
        .sharing_mode(vk::SharingMode::EXCLUSIVE);
    // SAFETY: valid device and create info.
    let buffer = unsafe { dev.create_buffer(&info, None) }.vk("vkCreateBuffer")?;
    // SAFETY: destroyed once, by the scope.
    scope.defer(move |d| unsafe { d.destroy_buffer(buffer, None) });
    // SAFETY: `buffer` is live.
    let req = unsafe { dev.get_buffer_memory_requirements(buffer) };
    let mem_type = gpu
        .mem_type(req.memory_type_bits, HV, NONE)
        .ok_or_else(|| {
            format!(
                "no HOST_VISIBLE|HOST_COHERENT type in 0x{:x}",
                req.memory_type_bits
            )
        })?;
    let alloc = vk::MemoryAllocateInfo::default()
        .allocation_size(req.size)
        .memory_type_index(mem_type);
    // SAFETY: valid device and allocate info.
    let memory = unsafe { dev.allocate_memory(&alloc, None) }.vk("vkAllocateMemory")?;
    // SAFETY: freed once, after the buffer.
    scope.defer(move |d| unsafe { d.free_memory(memory, None) });
    // SAFETY: fresh memory of an allowed type.
    unsafe { dev.bind_buffer_memory(buffer, memory, 0) }.vk("vkBindBufferMemory")?;

    let map = |offset: u64, size: u64| -> Result<*mut u32, String> {
        // SAFETY: host-visible memory, range inside the allocation, not mapped.
        unsafe { dev.map_memory(memory, offset, size, vk::MemoryMapFlags::empty()) }
            .vk(&format!("vkMapMemory(offset {offset}, size {size})"))
            .map(|p| p.cast::<u32>())
    };

    // Map, write, unmap.
    let ptr = map(0, vk::WHOLE_SIZE)?;
    if ptr.is_null() {
        return Err("vkMapMemory returned VK_SUCCESS and a null pointer".into());
    }
    let want: Vec<u32> = (0..WORDS).map(|i| pattern3(i, 0x5eed_0001)).collect();
    // SAFETY: the mapping covers SIZE bytes, mapped pointers are aligned to
    // minMemoryMapAlignment (>= 64), and `want` is a distinct allocation.
    unsafe { std::ptr::copy_nonoverlapping(want.as_ptr(), ptr, WORDS) };
    // SAFETY: mapped above.
    unsafe { dev.unmap_memory(memory) };

    // Remap and read back the whole thing.
    let ptr = map(0, vk::WHOLE_SIZE)?;
    let mut got = vec![0u32; WORDS];
    // SAFETY: as above.
    unsafe { std::ptr::copy_nonoverlapping(ptr, got.as_mut_ptr(), WORDS) };
    // SAFETY: mapped above.
    unsafe { dev.unmap_memory(memory) };
    if let Some(bad) = mismatches(&got, |i| want[i]) {
        return Err(format!("after unmap+remap: {bad}"));
    }

    // And a remap at an offset must see the same bytes at the right place.
    let offset_words = WORDS / 4 + 16;
    let ptr = map((offset_words * 4) as u64, 4096)?;
    let mut window = vec![0u32; 1024];
    // SAFETY: the mapping covers 4096 bytes.
    unsafe { std::ptr::copy_nonoverlapping(ptr, window.as_mut_ptr(), 1024) };
    // SAFETY: mapped above.
    unsafe { dev.unmap_memory(memory) };
    if let Some(bad) = mismatches(&window, |i| want[offset_words + i]) {
        return Err(format!("remap at offset {}: {bad}", offset_words * 4));
    }
    Ok(Out::Pass(format!(
        "1 MiB in memory type {mem_type} ({:?}): write, unmap, remap, read back ok; offset remap ok",
        gpu.mem.memory_types[mem_type as usize].property_flags
    )))
}

// ---------------------------------------------------------------------------
// 4. transfer: fill + update + copy, executed by the GPU

pub fn transfer(gpu: &Gpu) -> Result<Out, String> {
    const SIZE: u64 = 64 * 1024;
    const WORDS: usize = (SIZE / 4) as usize;
    const HALF: u64 = SIZE / 2;
    let mut scope = Scope::new(gpu);
    let usage = vk::BufferUsageFlags::TRANSFER_SRC | vk::BufferUsageFlags::TRANSFER_DST;
    let src = scope.buffer(SIZE, usage, NONE, DEVICE_LOCAL)?;
    let dst = scope.buffer(SIZE, vk::BufferUsageFlags::TRANSFER_DST, HV, NONE)?;
    dst.write_u32s(0, &vec![0xdddd_dddd; WORDS]);

    let update: Vec<u32> = (0..16).map(|i| 0x7700_0000 | i).collect();
    const UPDATE_AT: usize = 15000;

    let elapsed = gpu.one_shot("transfer", |dev, cb| {
        // SAFETY: `cb` is recording; offsets and sizes are multiples of 4 and
        // inside the buffers; every hazard below has its barrier.
        unsafe {
            dev.cmd_fill_buffer(cb, src.buffer, 0, vk::WHOLE_SIZE, 0x1111_1111);
            buffer_barrier(dev, cb, src.buffer, TRANSFER_WRITE, TRANSFER_WRITE);
            dev.cmd_fill_buffer(cb, src.buffer, 4096, 4096, 0xcafe_babe);
            dev.cmd_fill_buffer(cb, src.buffer, 40960, 4096, 0x0bad_f00d);
            let bytes: Vec<u8> = update.iter().flat_map(|w| w.to_le_bytes()).collect();
            dev.cmd_update_buffer(cb, src.buffer, (UPDATE_AT * 4) as u64, &bytes);
            buffer_barrier(dev, cb, src.buffer, TRANSFER_WRITE, TRANSFER_READ);
            // Swap halves on the way, so the copy regions' offsets are tested.
            let regions = [
                vk::BufferCopy {
                    src_offset: 0,
                    dst_offset: HALF,
                    size: HALF,
                },
                vk::BufferCopy {
                    src_offset: HALF,
                    dst_offset: 0,
                    size: HALF,
                },
            ];
            dev.cmd_copy_buffer(cb, src.buffer, dst.buffer, &regions);
            buffer_barrier(dev, cb, dst.buffer, TRANSFER_WRITE, HOST_READ);
        }
    })?;

    // The same operations on the CPU.
    let mut model = vec![0x1111_1111u32; WORDS];
    model[1024..2048].fill(0xcafe_babe);
    model[10240..11264].fill(0x0bad_f00d);
    model[UPDATE_AT..UPDATE_AT + 16].copy_from_slice(&update);
    let half = WORDS / 2;
    let want = |i: usize| {
        if i < half {
            model[i + half]
        } else {
            model[i - half]
        }
    };
    let got = dst.read_u32s(WORDS);
    if let Some(bad) = mismatches(&got, want) {
        return Err(bad);
    }
    Ok(Out::Pass(format!(
        "fill x3 + update + 2-region copy of 64 KiB (src type {}, dst type {}) verified, GPU time {}",
        src.mem_type,
        dst.mem_type,
        ms(elapsed)
    )))
}

// ---------------------------------------------------------------------------
// 5. compute

/// Must match `f` in shaders/compute.wgsl.
pub fn compute_f(i: u32) -> u32 {
    let mut x = i.wrapping_mul(2_654_435_761);
    x ^= x >> 15;
    x.wrapping_add(i << 3).wrapping_add(0x9e37_79b9)
}

pub fn compute(gpu: &Gpu) -> Result<Out, String> {
    const N: u32 = 1 << 20;
    const WG: u32 = 64;
    let dev = &gpu.device;
    let max_groups = gpu.limits.max_compute_work_group_count[0];
    if max_groups < N / WG {
        return Err(format!(
            "maxComputeWorkGroupCount[0] = {max_groups}, below the {} this needs",
            N / WG
        ));
    }
    let mut scope = Scope::new(gpu);
    let buf = scope.buffer(
        u64::from(N) * 4,
        vk::BufferUsageFlags::STORAGE_BUFFER,
        HV,
        NONE,
    )?;
    buf.write_u32s(0, &vec![0xffff_ffff; N as usize]);

    let module = scope.shader(COMPUTE_SPV)?;
    let bindings = [vk::DescriptorSetLayoutBinding::default()
        .binding(0)
        .descriptor_type(vk::DescriptorType::STORAGE_BUFFER)
        .descriptor_count(1)
        .stage_flags(vk::ShaderStageFlags::COMPUTE)];
    // SAFETY (this block and the ones below): valid device, create infos whose
    // pointers outlive each call, every object destroyed once by the scope.
    let set_layout = unsafe {
        dev.create_descriptor_set_layout(
            &vk::DescriptorSetLayoutCreateInfo::default().bindings(&bindings),
            None,
        )
    }
    .vk("vkCreateDescriptorSetLayout")?;
    // SAFETY: see above.
    scope.defer(move |d| unsafe { d.destroy_descriptor_set_layout(set_layout, None) });
    let sizes = [vk::DescriptorPoolSize {
        ty: vk::DescriptorType::STORAGE_BUFFER,
        descriptor_count: 1,
    }];
    // SAFETY: see above.
    let pool = unsafe {
        dev.create_descriptor_pool(
            &vk::DescriptorPoolCreateInfo::default()
                .max_sets(1)
                .pool_sizes(&sizes),
            None,
        )
    }
    .vk("vkCreateDescriptorPool")?;
    // SAFETY: see above; destroying the pool frees its set.
    scope.defer(move |d| unsafe { d.destroy_descriptor_pool(pool, None) });
    let layouts = [set_layout];
    // SAFETY: see above.
    let set = unsafe {
        dev.allocate_descriptor_sets(
            &vk::DescriptorSetAllocateInfo::default()
                .descriptor_pool(pool)
                .set_layouts(&layouts),
        )
    }
    .vk("vkAllocateDescriptorSets")?[0];
    let buf_info = [vk::DescriptorBufferInfo {
        buffer: buf.buffer,
        offset: 0,
        range: vk::WHOLE_SIZE,
    }];
    let write = [vk::WriteDescriptorSet::default()
        .dst_set(set)
        .dst_binding(0)
        .descriptor_type(vk::DescriptorType::STORAGE_BUFFER)
        .buffer_info(&buf_info)];
    // SAFETY: `set` is live and not in use.
    unsafe { dev.update_descriptor_sets(&write, &[]) };
    // SAFETY: see above.
    let layout = unsafe {
        dev.create_pipeline_layout(
            &vk::PipelineLayoutCreateInfo::default().set_layouts(&layouts),
            None,
        )
    }
    .vk("vkCreatePipelineLayout")?;
    // SAFETY: see above.
    scope.defer(move |d| unsafe { d.destroy_pipeline_layout(layout, None) });
    let stage = vk::PipelineShaderStageCreateInfo::default()
        .stage(vk::ShaderStageFlags::COMPUTE)
        .module(module)
        .name(c"main");
    let info = [vk::ComputePipelineCreateInfo::default()
        .stage(stage)
        .layout(layout)];
    // SAFETY: see above.
    let pipeline = unsafe { dev.create_compute_pipelines(vk::PipelineCache::null(), &info, None) }
        .map_err(|(_, e)| format!("vkCreateComputePipelines: {}", result_name(e)))?[0];
    // SAFETY: see above.
    scope.defer(move |d| unsafe { d.destroy_pipeline(pipeline, None) });

    let elapsed = gpu.one_shot("compute", |dev, cb| {
        // SAFETY: `cb` is recording; pipeline, layout and set are compatible.
        unsafe {
            dev.cmd_bind_pipeline(cb, vk::PipelineBindPoint::COMPUTE, pipeline);
            dev.cmd_bind_descriptor_sets(
                cb,
                vk::PipelineBindPoint::COMPUTE,
                layout,
                0,
                &[set],
                &[],
            );
            dev.cmd_dispatch(cb, N / WG, 1, 1);
        }
        buffer_barrier(
            dev,
            cb,
            buf.buffer,
            (
                vk::PipelineStageFlags::COMPUTE_SHADER,
                vk::AccessFlags::SHADER_WRITE,
            ),
            HOST_READ,
        );
    })?;
    let got = buf.read_u32s(N as usize);
    if let Some(bad) = mismatches(&got, |i| compute_f(i as u32)) {
        return Err(bad);
    }
    Ok(Out::Pass(format!(
        "{N} elements, {} workgroups of {WG}, all f(i) correct, GPU time {}",
        N / WG,
        ms(elapsed)
    )))
}

// ---------------------------------------------------------------------------
// 6 and 7. graphics

const COLOR_FORMAT: vk::Format = vk::Format::R8G8B8A8_UNORM;

enum Target<'a> {
    RenderPass,
    Dynamic(&'a DynRender),
}

fn color_subresource() -> vk::ImageSubresourceRange {
    vk::ImageSubresourceRange {
        aspect_mask: vk::ImageAspectFlags::COLOR,
        base_mip_level: 0,
        level_count: 1,
        base_array_layer: 0,
        layer_count: 1,
    }
}

fn image_barrier(
    dev: &ash::Device,
    cb: vk::CommandBuffer,
    image: vk::Image,
    (old, new): (vk::ImageLayout, vk::ImageLayout),
    (src_stage, src_access): (vk::PipelineStageFlags, vk::AccessFlags),
    (dst_stage, dst_access): (vk::PipelineStageFlags, vk::AccessFlags),
) {
    let barrier = [vk::ImageMemoryBarrier::default()
        .src_access_mask(src_access)
        .dst_access_mask(dst_access)
        .old_layout(old)
        .new_layout(new)
        .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
        .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
        .image(image)
        .subresource_range(color_subresource())];
    // SAFETY: `cb` is recording; `image` belongs to this device.
    unsafe {
        dev.cmd_pipeline_barrier(
            cb,
            src_stage,
            dst_stage,
            vk::DependencyFlags::empty(),
            &[],
            &[],
            &barrier,
        )
    };
}

fn render_triangle(gpu: &Gpu, target: Target<'_>, clear: [f32; 4]) -> Result<String, String> {
    let dev = &gpu.device;
    let size = raster::SIZE;
    let extent = vk::Extent2D {
        width: size,
        height: size,
    };
    let mut scope = Scope::new(gpu);

    // The colour image.
    let image_info = vk::ImageCreateInfo::default()
        .image_type(vk::ImageType::TYPE_2D)
        .format(COLOR_FORMAT)
        .extent(vk::Extent3D {
            width: size,
            height: size,
            depth: 1,
        })
        .mip_levels(1)
        .array_layers(1)
        .samples(vk::SampleCountFlags::TYPE_1)
        .tiling(vk::ImageTiling::OPTIMAL)
        .usage(vk::ImageUsageFlags::COLOR_ATTACHMENT | vk::ImageUsageFlags::TRANSFER_SRC)
        .sharing_mode(vk::SharingMode::EXCLUSIVE)
        .initial_layout(vk::ImageLayout::UNDEFINED);
    // SAFETY (all object creation in this function): valid device, create
    // infos whose pointers outlive each call, every object destroyed once, in
    // reverse order, by the scope.
    let image = unsafe { dev.create_image(&image_info, None) }.vk("vkCreateImage")?;
    // SAFETY: see above.
    scope.defer(move |d| unsafe { d.destroy_image(image, None) });
    // SAFETY: `image` is live.
    let req = unsafe { dev.get_image_memory_requirements(image) };
    let mem_type = gpu
        .mem_type(req.memory_type_bits, NONE, DEVICE_LOCAL)
        .ok_or_else(|| {
            format!(
                "no memory type in 0x{:x} for the image",
                req.memory_type_bits
            )
        })?;
    let alloc = vk::MemoryAllocateInfo::default()
        .allocation_size(req.size)
        .memory_type_index(mem_type);
    // SAFETY: see above.
    let memory = unsafe { dev.allocate_memory(&alloc, None) }.vk("vkAllocateMemory(image)")?;
    // SAFETY: see above.
    scope.defer(move |d| unsafe { d.free_memory(memory, None) });
    // SAFETY: fresh memory of an allowed type.
    unsafe { dev.bind_image_memory(image, memory, 0) }.vk("vkBindImageMemory")?;
    let view_info = vk::ImageViewCreateInfo::default()
        .image(image)
        .view_type(vk::ImageViewType::TYPE_2D)
        .format(COLOR_FORMAT)
        .subresource_range(color_subresource());
    // SAFETY: see above.
    let view = unsafe { dev.create_image_view(&view_info, None) }.vk("vkCreateImageView")?;
    // SAFETY: see above.
    scope.defer(move |d| unsafe { d.destroy_image_view(view, None) });

    // Vertex and readback buffers.
    let vertices = raster::vertex_data();
    let vbuf = scope.buffer(
        std::mem::size_of_val(&vertices) as u64,
        vk::BufferUsageFlags::VERTEX_BUFFER,
        HV,
        NONE,
    )?;
    vbuf.write_u32s(0, &vertices.map(f32::to_bits));
    let bytes = u64::from(size * size * 4);
    let readback: Buf = scope.buffer(bytes, vk::BufferUsageFlags::TRANSFER_DST, HV, NONE)?;
    readback.write_u32s(0, &vec![0xdead_beef; (bytes / 4) as usize]);

    // Render pass + framebuffer, for the classic path only.
    let render_pass = match target {
        Target::RenderPass => {
            let attachments = [vk::AttachmentDescription::default()
                .format(COLOR_FORMAT)
                .samples(vk::SampleCountFlags::TYPE_1)
                .load_op(vk::AttachmentLoadOp::CLEAR)
                .store_op(vk::AttachmentStoreOp::STORE)
                .stencil_load_op(vk::AttachmentLoadOp::DONT_CARE)
                .stencil_store_op(vk::AttachmentStoreOp::DONT_CARE)
                .initial_layout(vk::ImageLayout::UNDEFINED)
                .final_layout(vk::ImageLayout::TRANSFER_SRC_OPTIMAL)];
            let color_ref = [vk::AttachmentReference {
                attachment: 0,
                layout: vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL,
            }];
            let subpass = [vk::SubpassDescription::default()
                .pipeline_bind_point(vk::PipelineBindPoint::GRAPHICS)
                .color_attachments(&color_ref)];
            let deps = [
                vk::SubpassDependency {
                    src_subpass: vk::SUBPASS_EXTERNAL,
                    dst_subpass: 0,
                    src_stage_mask: vk::PipelineStageFlags::COLOR_ATTACHMENT_OUTPUT,
                    dst_stage_mask: vk::PipelineStageFlags::COLOR_ATTACHMENT_OUTPUT,
                    src_access_mask: vk::AccessFlags::empty(),
                    dst_access_mask: vk::AccessFlags::COLOR_ATTACHMENT_WRITE,
                    dependency_flags: vk::DependencyFlags::empty(),
                },
                vk::SubpassDependency {
                    src_subpass: 0,
                    dst_subpass: vk::SUBPASS_EXTERNAL,
                    src_stage_mask: vk::PipelineStageFlags::COLOR_ATTACHMENT_OUTPUT,
                    dst_stage_mask: vk::PipelineStageFlags::TRANSFER,
                    src_access_mask: vk::AccessFlags::COLOR_ATTACHMENT_WRITE,
                    dst_access_mask: vk::AccessFlags::TRANSFER_READ,
                    dependency_flags: vk::DependencyFlags::empty(),
                },
            ];
            let info = vk::RenderPassCreateInfo::default()
                .attachments(&attachments)
                .subpasses(&subpass)
                .dependencies(&deps);
            // SAFETY: see above.
            let rp = unsafe { dev.create_render_pass(&info, None) }.vk("vkCreateRenderPass")?;
            // SAFETY: see above.
            scope.defer(move |d| unsafe { d.destroy_render_pass(rp, None) });
            Some(rp)
        }
        Target::Dynamic(_) => None,
    };
    let framebuffer = match render_pass {
        Some(rp) => {
            let views = [view];
            let info = vk::FramebufferCreateInfo::default()
                .render_pass(rp)
                .attachments(&views)
                .width(size)
                .height(size)
                .layers(1);
            // SAFETY: see above.
            let fb = unsafe { dev.create_framebuffer(&info, None) }.vk("vkCreateFramebuffer")?;
            // SAFETY: see above.
            scope.defer(move |d| unsafe { d.destroy_framebuffer(fb, None) });
            Some(fb)
        }
        None => None,
    };

    // The pipeline.
    let module = scope.shader(TRIANGLE_SPV)?;
    // SAFETY: see above.
    let layout =
        unsafe { dev.create_pipeline_layout(&vk::PipelineLayoutCreateInfo::default(), None) }
            .vk("vkCreatePipelineLayout")?;
    // SAFETY: see above.
    scope.defer(move |d| unsafe { d.destroy_pipeline_layout(layout, None) });
    let stages = [
        vk::PipelineShaderStageCreateInfo::default()
            .stage(vk::ShaderStageFlags::VERTEX)
            .module(module)
            .name(c"vs_main"),
        vk::PipelineShaderStageCreateInfo::default()
            .stage(vk::ShaderStageFlags::FRAGMENT)
            .module(module)
            .name(c"fs_main"),
    ];
    let vb_bindings = [vk::VertexInputBindingDescription {
        binding: 0,
        stride: 5 * 4,
        input_rate: vk::VertexInputRate::VERTEX,
    }];
    let vb_attrs = [
        vk::VertexInputAttributeDescription {
            location: 0,
            binding: 0,
            format: vk::Format::R32G32_SFLOAT,
            offset: 0,
        },
        vk::VertexInputAttributeDescription {
            location: 1,
            binding: 0,
            format: vk::Format::R32G32B32_SFLOAT,
            offset: 8,
        },
    ];
    let vertex_input = vk::PipelineVertexInputStateCreateInfo::default()
        .vertex_binding_descriptions(&vb_bindings)
        .vertex_attribute_descriptions(&vb_attrs);
    let assembly = vk::PipelineInputAssemblyStateCreateInfo::default()
        .topology(vk::PrimitiveTopology::TRIANGLE_LIST);
    let viewports = [vk::Viewport {
        x: 0.0,
        y: 0.0,
        width: size as f32,
        height: size as f32,
        min_depth: 0.0,
        max_depth: 1.0,
    }];
    let scissors = [vk::Rect2D {
        offset: vk::Offset2D { x: 0, y: 0 },
        extent,
    }];
    let viewport = vk::PipelineViewportStateCreateInfo::default()
        .viewports(&viewports)
        .scissors(&scissors);
    let raster_state = vk::PipelineRasterizationStateCreateInfo::default()
        .polygon_mode(vk::PolygonMode::FILL)
        .cull_mode(vk::CullModeFlags::NONE)
        .front_face(vk::FrontFace::COUNTER_CLOCKWISE)
        .line_width(1.0);
    let multisample = vk::PipelineMultisampleStateCreateInfo::default()
        .rasterization_samples(vk::SampleCountFlags::TYPE_1);
    let blend_attachments = [vk::PipelineColorBlendAttachmentState::default()
        .blend_enable(false)
        .color_write_mask(vk::ColorComponentFlags::RGBA)];
    let blend = vk::PipelineColorBlendStateCreateInfo::default().attachments(&blend_attachments);
    let formats = [COLOR_FORMAT];
    let mut rendering =
        vk::PipelineRenderingCreateInfo::default().color_attachment_formats(&formats);
    let mut info = vk::GraphicsPipelineCreateInfo::default()
        .stages(&stages)
        .vertex_input_state(&vertex_input)
        .input_assembly_state(&assembly)
        .viewport_state(&viewport)
        .rasterization_state(&raster_state)
        .multisample_state(&multisample)
        .color_blend_state(&blend)
        .layout(layout);
    match render_pass {
        Some(rp) => info = info.render_pass(rp).subpass(0),
        None => info = info.push_next(&mut rendering),
    }
    // SAFETY: see above.
    let pipeline =
        unsafe { dev.create_graphics_pipelines(vk::PipelineCache::null(), &[info], None) }
            .map_err(|(_, e)| format!("vkCreateGraphicsPipelines: {}", result_name(e)))?[0];
    // SAFETY: see above.
    scope.defer(move |d| unsafe { d.destroy_pipeline(pipeline, None) });

    let clear_value = [vk::ClearValue {
        color: vk::ClearColorValue { float32: clear },
    }];
    let area = vk::Rect2D {
        offset: vk::Offset2D { x: 0, y: 0 },
        extent,
    };
    let draw = |dev: &ash::Device, cb: vk::CommandBuffer| {
        // SAFETY: `cb` is recording inside a render pass / rendering scope
        // compatible with `pipeline`; the vertex buffer holds 3 vertices.
        unsafe {
            dev.cmd_bind_pipeline(cb, vk::PipelineBindPoint::GRAPHICS, pipeline);
            dev.cmd_bind_vertex_buffers(cb, 0, &[vbuf.buffer], &[0]);
            dev.cmd_draw(cb, 3, 1, 0, 0);
        }
    };
    let copy = |dev: &ash::Device, cb: vk::CommandBuffer| {
        let region = [vk::BufferImageCopy {
            buffer_offset: 0,
            buffer_row_length: 0,
            buffer_image_height: 0,
            image_subresource: vk::ImageSubresourceLayers {
                aspect_mask: vk::ImageAspectFlags::COLOR,
                mip_level: 0,
                base_array_layer: 0,
                layer_count: 1,
            },
            image_offset: vk::Offset3D { x: 0, y: 0, z: 0 },
            image_extent: vk::Extent3D {
                width: size,
                height: size,
                depth: 1,
            },
        }];
        // SAFETY: `cb` is recording; the image is in TRANSFER_SRC_OPTIMAL and
        // the buffer holds exactly one tightly packed RGBA8 image.
        unsafe {
            dev.cmd_copy_image_to_buffer(
                cb,
                image,
                vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
                readback.buffer,
                &region,
            )
        };
        buffer_barrier(dev, cb, readback.buffer, TRANSFER_WRITE, HOST_READ);
    };

    let elapsed = match target {
        Target::RenderPass => gpu.one_shot("render pass", |dev, cb| {
            let begin = vk::RenderPassBeginInfo::default()
                .render_pass(render_pass.unwrap_or_default())
                .framebuffer(framebuffer.unwrap_or_default())
                .render_area(area)
                .clear_values(&clear_value);
            // SAFETY: `cb` is recording; render pass and framebuffer match.
            unsafe { dev.cmd_begin_render_pass(cb, &begin, vk::SubpassContents::INLINE) };
            draw(dev, cb);
            // SAFETY: inside the render pass begun above.
            unsafe { dev.cmd_end_render_pass(cb) };
            copy(dev, cb);
        })?,
        Target::Dynamic(dr) => gpu.one_shot("dynamic rendering", |dev, cb| {
            image_barrier(
                dev,
                cb,
                image,
                (
                    vk::ImageLayout::UNDEFINED,
                    vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL,
                ),
                (
                    vk::PipelineStageFlags::TOP_OF_PIPE,
                    vk::AccessFlags::empty(),
                ),
                (
                    vk::PipelineStageFlags::COLOR_ATTACHMENT_OUTPUT,
                    vk::AccessFlags::COLOR_ATTACHMENT_WRITE,
                ),
            );
            let color = [vk::RenderingAttachmentInfo::default()
                .image_view(view)
                .image_layout(vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL)
                .load_op(vk::AttachmentLoadOp::CLEAR)
                .store_op(vk::AttachmentStoreOp::STORE)
                .clear_value(clear_value[0])];
            let info = vk::RenderingInfo::default()
                .render_area(area)
                .layer_count(1)
                .color_attachments(&color);
            // SAFETY: `cb` is recording; the view is in the declared layout;
            // the feature was enabled at device creation (check 2).
            unsafe {
                match dr {
                    DynRender::Core => dev.cmd_begin_rendering(cb, &info),
                    DynRender::Khr(khr) => khr.cmd_begin_rendering(cb, &info),
                }
            }
            draw(dev, cb);
            // SAFETY: inside the rendering scope begun above.
            unsafe {
                match dr {
                    DynRender::Core => dev.cmd_end_rendering(cb),
                    DynRender::Khr(khr) => khr.cmd_end_rendering(cb),
                }
            }
            image_barrier(
                dev,
                cb,
                image,
                (
                    vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL,
                    vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
                ),
                (
                    vk::PipelineStageFlags::COLOR_ATTACHMENT_OUTPUT,
                    vk::AccessFlags::COLOR_ATTACHMENT_WRITE,
                ),
                TRANSFER_READ,
            );
            copy(dev, cb);
        })?,
    };
    let got = readback.read_bytes(bytes as usize);
    let detail = raster::verify(&got, clear)?;
    Ok(format!(
        "{detail}, image memory type {mem_type}, GPU time {}",
        ms(elapsed)
    ))
}

pub fn graphics(gpu: &Gpu) -> Result<Out, String> {
    render_triangle(gpu, Target::RenderPass, raster::CLEAR_6)
        .map(|d| Out::Pass(format!("render pass: {d}")))
}

pub fn dynamic_rendering(gpu: &Gpu) -> Result<Out, String> {
    match &gpu.dynrender {
        None => Ok(Out::Skip(format!(
            "effective Vulkan {} is below 1.3 and VK_KHR_dynamic_rendering is absent, off, or not queryable (features2 needs 1.1)",
            crate::gpu::version_string(gpu.api)
        ))),
        Some(dr) => {
            let how = match dr {
                DynRender::Core => "core 1.3",
                DynRender::Khr(_) => "VK_KHR_dynamic_rendering",
            };
            render_triangle(gpu, Target::Dynamic(dr), raster::CLEAR_7)
                .map(|d| Out::Pass(format!("{how}: {d}")))
        }
    }
}

// ---------------------------------------------------------------------------
// 8. sync

fn record(
    gpu: &Gpu,
    cb: vk::CommandBuffer,
    f: impl FnOnce(&ash::Device, vk::CommandBuffer),
) -> Result<(), String> {
    let dev = &gpu.device;
    let begin =
        vk::CommandBufferBeginInfo::default().flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT);
    // SAFETY: `cb` is a fresh or reset primary command buffer of this device.
    unsafe { dev.begin_command_buffer(cb, &begin) }.vk("vkBeginCommandBuffer")?;
    f(dev, cb);
    // SAFETY: recording was begun above.
    unsafe { dev.end_command_buffer(cb) }.vk("vkEndCommandBuffer")
}

pub fn sync(gpu: &Gpu) -> Result<Out, String> {
    let Some(timeline) = &gpu.timeline else {
        return Ok(Out::Skip(format!(
            "no timeline semaphores at effective Vulkan {} (needs 1.2, or 1.1 + VK_KHR_timeline_semaphore)",
            crate::gpu::version_string(gpu.api)
        )));
    };
    const SIZE: u64 = 16 * 1024;
    const WORDS: usize = (SIZE / 4) as usize;
    let dev = &gpu.device;
    let mut scope = Scope::new(gpu);
    let usage = vk::BufferUsageFlags::TRANSFER_SRC | vk::BufferUsageFlags::TRANSFER_DST;
    let x = scope.buffer(SIZE, usage, NONE, DEVICE_LOCAL)?;
    let h = scope.buffer(SIZE, vk::BufferUsageFlags::TRANSFER_DST, HV, NONE)?;
    h.write_u32s(0, &vec![0; WORDS]);

    let mut type_info = vk::SemaphoreTypeCreateInfo::default()
        .semaphore_type(vk::SemaphoreType::TIMELINE)
        .initial_value(0);
    let sem_info = vk::SemaphoreCreateInfo::default().push_next(&mut type_info);
    // SAFETY: valid device; timeline semaphores were enabled in check 2.
    let sem = unsafe { dev.create_semaphore(&sem_info, None) }.vk("vkCreateSemaphore(timeline)")?;
    // SAFETY: destroyed once, after every submit using it completed.
    scope.defer(move |d| unsafe { d.destroy_semaphore(sem, None) });
    let counter = |what: &str| -> Result<u64, String> {
        // SAFETY: `sem` is a live timeline semaphore of this device.
        unsafe {
            match timeline {
                Timeline::Core => dev.get_semaphore_counter_value(sem),
                Timeline::Khr(khr) => khr.get_semaphore_counter_value(sem),
            }
        }
        .vk(&format!("vkGetSemaphoreCounterValue({what})"))
    };
    if counter("initial")? != 0 {
        return Err("a fresh timeline semaphore with initialValue 0 reads non-zero".into());
    }

    let cbs = scope.command_buffers(4)?;
    // Submit A fills X and signals 1; submit B waits for 1, copies X to H and
    // signals 2. Only the semaphore orders them.
    record(gpu, cbs[0], |dev, cb| {
        // SAFETY: `cb` is recording; whole-buffer fill.
        unsafe { dev.cmd_fill_buffer(cb, x.buffer, 0, vk::WHOLE_SIZE, 0xa5a5_a5a5) };
    })?;
    record(gpu, cbs[1], |dev, cb| {
        // SAFETY: `cb` is recording; same-size buffers.
        unsafe {
            dev.cmd_copy_buffer(
                cb,
                x.buffer,
                h.buffer,
                &[vk::BufferCopy {
                    src_offset: 0,
                    dst_offset: 0,
                    size: SIZE,
                }],
            )
        };
        buffer_barrier(dev, cb, h.buffer, TRANSFER_WRITE, HOST_READ);
    })?;
    let t0 = Instant::now();
    {
        let sems = [sem];
        let one = [1u64];
        let two = [2u64];
        let stages = [vk::PipelineStageFlags::TRANSFER];
        let cb_a = [cbs[0]];
        let cb_b = [cbs[1]];
        let mut tl_a = vk::TimelineSemaphoreSubmitInfo::default().signal_semaphore_values(&one);
        let mut tl_b = vk::TimelineSemaphoreSubmitInfo::default()
            .wait_semaphore_values(&one)
            .signal_semaphore_values(&two);
        let submits = [
            vk::SubmitInfo::default()
                .command_buffers(&cb_a)
                .signal_semaphores(&sems)
                .push_next(&mut tl_a),
            vk::SubmitInfo::default()
                .wait_semaphores(&sems)
                .wait_dst_stage_mask(&stages)
                .command_buffers(&cb_b)
                .signal_semaphores(&sems)
                .push_next(&mut tl_b),
        ];
        // Two separate vkQueueSubmit calls, not one batch: the dependency has to
        // cross a submit boundary.
        // SAFETY: executable command buffers; the semaphore values increase.
        unsafe { dev.queue_submit(gpu.queue, &submits[..1], vk::Fence::null()) }
            .vk("vkQueueSubmit(A)")?;
        // SAFETY: as above.
        unsafe { dev.queue_submit(gpu.queue, &submits[1..], vk::Fence::null()) }
            .vk("vkQueueSubmit(B)")?;

        let wait = vk::SemaphoreWaitInfo::default()
            .semaphores(&sems)
            .values(&two);
        // SAFETY: `sem` is a live timeline semaphore of this device.
        let waited = unsafe {
            match timeline {
                Timeline::Core => dev.wait_semaphores(&wait, gpu.timeout_ns),
                Timeline::Khr(khr) => khr.wait_semaphores(&wait, gpu.timeout_ns),
            }
        };
        match waited {
            Ok(()) => {}
            Err(vk::Result::TIMEOUT) => {
                gpu.hung.set(true);
                return Err(format!(
                    "vkWaitSemaphores(value 2): VK_TIMEOUT after {} ms, counter never reached 2",
                    gpu.timeout_ns / 1_000_000
                ));
            }
            Err(e) => return Err(format!("vkWaitSemaphores(value 2): {}", result_name(e))),
        }
    }
    let waited = t0.elapsed();
    let value = counter("after the wait")?;
    if value != 2 {
        return Err(format!(
            "vkWaitSemaphores returned but the counter reads {value}, not 2"
        ));
    }
    if let Some(bad) = mismatches(&h.read_u32s(WORDS), |_| 0xa5a5_a5a5) {
        return Err(format!("after the timeline chain: {bad}"));
    }

    // vkQueueWaitIdle, then vkDeviceWaitIdle, each the only thing standing
    // between a submit and the host reading its result.
    for (i, (value, idle)) in [
        (0x5a5a_5a5au32, "vkQueueWaitIdle"),
        (0x3c3c_3c3c, "vkDeviceWaitIdle"),
    ]
    .into_iter()
    .enumerate()
    {
        let cb = cbs[2 + i];
        record(gpu, cb, |dev, cb| {
            // SAFETY: `cb` is recording; whole-buffer fill.
            unsafe { dev.cmd_fill_buffer(cb, h.buffer, 0, vk::WHOLE_SIZE, value) };
            buffer_barrier(dev, cb, h.buffer, TRANSFER_WRITE, HOST_READ);
        })?;
        let cb_arr = [cb];
        let submit = [vk::SubmitInfo::default().command_buffers(&cb_arr)];
        // SAFETY: executable command buffer, no fence.
        unsafe { dev.queue_submit(gpu.queue, &submit, vk::Fence::null()) }.vk("vkQueueSubmit")?;
        // SAFETY: valid queue / device. (Neither call has a timeout; main's
        // watchdog covers a hang here.)
        let r = unsafe {
            if i == 0 {
                dev.queue_wait_idle(gpu.queue)
            } else {
                dev.device_wait_idle()
            }
        };
        r.vk(idle)?;
        if let Some(bad) = mismatches(&h.read_u32s(WORDS), |_| value) {
            return Err(format!("after {idle}: {bad}"));
        }
    }
    let how = match timeline {
        Timeline::Core => "core",
        Timeline::Khr(_) => "KHR",
    };
    Ok(Out::Pass(format!(
        "{how} timeline: submit A signals 1, submit B waits 1 and signals 2, host vkWaitSemaphores(2) in {}, counter=2, data ok; vkQueueWaitIdle ok; vkDeviceWaitIdle ok",
        ms(waited)
    )))
}

// ---------------------------------------------------------------------------
// 9. many submits

pub fn many_submits(gpu: &Gpu) -> Result<Out, String> {
    const N: usize = 1000;
    let dev = &gpu.device;
    let mut scope = Scope::new(gpu);
    let h = scope.buffer((N * 4) as u64, vk::BufferUsageFlags::TRANSFER_DST, HV, NONE)?;
    h.write_u32s(0, &[0; N]);
    let value = |i: usize| 0x1000_0000 | i as u32;

    let t_record = Instant::now();
    let cbs = scope.command_buffers(N as u32)?;
    for (i, &cb) in cbs.iter().enumerate() {
        record(gpu, cb, |dev, cb| {
            // SAFETY: `cb` is recording; one aligned word inside the buffer.
            unsafe { dev.cmd_fill_buffer(cb, h.buffer, (i * 4) as u64, 4, value(i)) };
            buffer_barrier(dev, cb, h.buffer, TRANSFER_WRITE, HOST_READ);
        })?;
    }
    let mut fences = Vec::with_capacity(N);
    for _ in 0..N {
        fences.push(scope.fence(false)?);
    }
    let recorded = t_record.elapsed();

    let t_submit = Instant::now();
    for (i, (&cb, &fence)) in cbs.iter().zip(&fences).enumerate() {
        let cb_arr = [cb];
        let submit = [vk::SubmitInfo::default().command_buffers(&cb_arr)];
        // SAFETY: executable command buffer, unsignalled fence, both ours.
        unsafe { dev.queue_submit(gpu.queue, &submit, fence) }
            .vk(&format!("vkQueueSubmit #{i}"))?;
    }
    let submitted = t_submit.elapsed();
    gpu.wait_fences(&fences, "1000 fences")?;
    let total = t_submit.elapsed();

    let mut unsignalled = Vec::new();
    for (i, &fence) in fences.iter().enumerate() {
        // SAFETY: a live fence of this device.
        match unsafe { dev.get_fence_status(fence) } {
            Ok(true) => {}
            Ok(false) => unsignalled.push(format!("#{i} unsignalled")),
            Err(e) => unsignalled.push(format!("#{i} {}", result_name(e))),
        }
    }
    if !unsignalled.is_empty() {
        return Err(format!(
            "vkWaitForFences(all) succeeded but {} fence(s) are not signalled: {}",
            unsignalled.len(),
            unsignalled
                .iter()
                .take(4)
                .cloned()
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    if let Some(bad) = mismatches(&h.read_u32s(N), value) {
        return Err(format!("lost submits: {bad}"));
    }
    Ok(Out::Pass(format!(
        "{N} submits, {N} fences, none lost; record {} + submit {}, all signalled after {} ({:.1} us/submit)",
        ms(recorded),
        ms(submitted),
        ms(total),
        total.as_secs_f64() * 1e6 / N as f64
    )))
}

#[cfg(test)]
mod tests {
    #[test]
    fn compute_f_is_not_trivial() {
        let values: std::collections::HashSet<u32> = (0..4096).map(super::compute_f).collect();
        assert_eq!(values.len(), 4096);
        assert_ne!(super::compute_f(0), 0);
    }
}
