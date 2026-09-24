//! The executor against the host's real GPU — the RTX 2070 on the Windows
//! development machine. Every test self-skips, with a line saying why, on a
//! host with no Vulkan loader or no device the executor would expose (Linux
//! CI, the WSL side of this machine where the only ICD is lavapipe).

use std::sync::Arc;

use ash::vk;

use super::AshVulkan;
use crate::renderer::Renderer3d;
use crate::venus::executor::harness::*;
use crate::venus::executor::policy::{c_name, MEMORY_PROPERTY_HOST_ANY};
use crate::venus::protocol::*;

/// The host, if it has a device the executor would expose.
fn host() -> Option<Arc<AshVulkan>> {
    let host = match AshVulkan::load() {
        Ok(host) => host,
        Err(why) => {
            eprintln!("skipping: {why}");
            return None;
        }
    };
    match host.usable_devices() {
        Ok(devices) => {
            for device in &devices {
                eprintln!("host device the guest would see: {}", device.name());
            }
            Some(Arc::new(host))
        }
        Err(why) => {
            eprintln!("skipping: {why}");
            None
        }
    }
}

/// What `ash` says the first non-CPU device is, asked directly.
fn first_gpu_directly() -> (String, u32, u32) {
    // SAFETY: test-only direct use of the loader, mirroring what the
    // renderer does: a local instance, enumerated and destroyed here.
    unsafe {
        let entry = ash::Entry::load().expect("the loader loaded a moment ago");
        let app = vk::ApplicationInfo::default().api_version(vk::API_VERSION_1_3);
        let info = vk::InstanceCreateInfo::default().application_info(&app);
        let instance = entry.create_instance(&info, None).expect("an instance");
        let mut found = None;
        for device in instance.enumerate_physical_devices().expect("devices") {
            let props = instance.get_physical_device_properties(device);
            if props.device_type != vk::PhysicalDeviceType::CPU && found.is_none() {
                let name = props
                    .device_name_as_c_str()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default();
                found = Some((name, props.vendor_id, props.device_id));
            }
        }
        instance.destroy_instance(None);
        found.expect("a GPU")
    }
}

#[test]
fn the_bring_up_runs_on_the_host_gpu_and_names_it_as_ash_does() {
    let Some(host) = host() else { return };
    let mut h = Harness::new(Arc::clone(&host));

    let Command::EnumerateInstanceVersion(v) = h.call(&enumerate_instance_version()).unwrap()
    else {
        panic!()
    };
    assert_eq!(v.ret, VK_SUCCESS);
    let (_, major, minor, _) = crate::venus::capset::vk_api_version_parts(v.p_api_version.unwrap());
    assert!((major, minor) >= (1, 1) && (major, minor) <= (1, 3));

    let Command::CreateInstance(i) = h.call(&create_instance(INSTANCE)).unwrap() else {
        panic!()
    };
    assert_eq!(i.ret, VK_SUCCESS);
    let Command::EnumeratePhysicalDevices(e) = h.call(&enumerate(INSTANCE, None)).unwrap() else {
        panic!()
    };
    let count = e.p_physical_device_count.unwrap();
    assert!(count >= 1);
    let ids: Vec<u64> = (0..u64::from(count)).map(|i| PHYSICAL + i).collect();
    let Command::EnumeratePhysicalDevices(e) = h.call(&enumerate(INSTANCE, Some(ids))).unwrap()
    else {
        panic!()
    };
    assert_eq!(e.ret, VK_SUCCESS);

    let Command::GetPhysicalDeviceProperties(p) = h.call(&properties(PHYSICAL)).unwrap() else {
        panic!()
    };
    let p = p.p_properties.unwrap();
    let (name, vendor, device) = first_gpu_directly();
    assert_eq!(String::from_utf8_lossy(c_name(&p.device_name)), name);
    assert_eq!((p.vendor_id, p.device_id), (vendor, device));
    let (_, major, minor, _) = crate::venus::capset::vk_api_version_parts(p.api_version);
    assert!((major, minor) <= (1, 3), "apiVersion is capped at 1.3");

    // The memory table, printed for the record, and the policy checked
    // against what the host itself reports.
    let mem = Command::GetPhysicalDeviceMemoryProperties2(GetPhysicalDeviceMemoryProperties2Args {
        physical_device: VkPhysicalDevice(PHYSICAL),
        p_memory_properties: Some(Default::default()),
    });
    let Command::GetPhysicalDeviceMemoryProperties2(m) = h.call(&mem).unwrap() else {
        panic!()
    };
    let guest = m.p_memory_properties.unwrap().memory_properties;
    let (host_memory, importable) = {
        let shown = host.usable_devices().unwrap();
        let first = &shown[0];
        (first.host_memory.clone(), first.importable)
    };
    eprintln!("{name}: memoryTypeBits importable from host allocations = {importable:#x}");
    eprintln!("type | heap | host flags | guest flags");
    assert_eq!(guest.memory_type_count, host_memory.memory_type_count);
    for i in 0..guest.memory_type_count as usize {
        let (h_ty, g_ty) = (&host_memory.memory_types[i], &guest.memory_types[i]);
        eprintln!(
            "{i:>4} | {:>4} | {:#010x} | {:#010x}",
            h_ty.heap_index, h_ty.property_flags, g_ty.property_flags
        );
        assert_eq!(g_ty.heap_index, h_ty.heap_index, "indices unchanged");
        if importable & (1 << i) != 0 {
            assert_eq!(g_ty.property_flags, h_ty.property_flags);
        } else {
            assert_eq!(
                g_ty.property_flags,
                h_ty.property_flags & !MEMORY_PROPERTY_HOST_ANY
            );
        }
    }
    assert!(crate::venus::executor::policy::has_coherent_host_type(
        &guest
    ));
    if name.contains("RTX 2070") && guest.memory_type_count >= 6 {
        // The 2026-09-23 probe: 3 and 4 import our pages, 5 (the BAR) cannot.
        assert_eq!(importable & 0x38, 0x18);
        assert_ne!(guest.memory_types[3].property_flags & 0x2, 0);
        assert_ne!(guest.memory_types[4].property_flags & 0x2, 0);
        assert_ne!(host_memory.memory_types[5].property_flags & 0x2, 0);
        assert_eq!(
            guest.memory_types[5].property_flags & MEMORY_PROPERTY_HOST_ANY,
            0
        );
    }

    // Features and properties through their chains, then the rest of the
    // bring-up on the real device.
    let features = Command::GetPhysicalDeviceFeatures2(GetPhysicalDeviceFeatures2Args {
        physical_device: VkPhysicalDevice(PHYSICAL),
        p_features: Some(VkPhysicalDeviceFeatures2 {
            p_next: vec![
                VkPhysicalDeviceFeatures2Next::VkPhysicalDeviceVulkan13Features(Default::default()),
                VkPhysicalDeviceFeatures2Next::VkPhysicalDeviceVulkan12Features(Default::default()),
                VkPhysicalDeviceFeatures2Next::VkPhysicalDeviceVulkan11Features(Default::default()),
            ],
            ..Default::default()
        }),
    });
    let Command::GetPhysicalDeviceFeatures2(f) = h.call(&features).unwrap() else {
        panic!()
    };
    assert_eq!(
        f.p_features.unwrap().features.sparse_binding,
        0,
        "sparse is masked"
    );
    let props2 = Command::GetPhysicalDeviceProperties2(GetPhysicalDeviceProperties2Args {
        physical_device: VkPhysicalDevice(PHYSICAL),
        p_properties: Some(VkPhysicalDeviceProperties2 {
            p_next: vec![
                VkPhysicalDeviceProperties2Next::VkPhysicalDeviceVulkan13Properties(
                    Default::default(),
                ),
                VkPhysicalDeviceProperties2Next::VkPhysicalDeviceVulkan12Properties(
                    Default::default(),
                ),
                VkPhysicalDeviceProperties2Next::VkPhysicalDeviceVulkan11Properties(
                    Default::default(),
                ),
            ],
            ..Default::default()
        }),
    });
    let Command::GetPhysicalDeviceProperties2(p2) = h.call(&props2).unwrap() else {
        panic!()
    };
    let p2 = p2.p_properties.unwrap();
    assert_eq!(c_name(&p2.properties.device_name), name.as_bytes());
    if let [_, VkPhysicalDeviceProperties2Next::VkPhysicalDeviceVulkan12Properties(v12), _] =
        p2.p_next.as_slice()
    {
        eprintln!(
            "driver: {} {}",
            String::from_utf8_lossy(c_name(&v12.driver_name)),
            String::from_utf8_lossy(c_name(&v12.driver_info))
        );
    }

    with_device_on(&mut h);
    let fp = Command::GetPhysicalDeviceFormatProperties2(GetPhysicalDeviceFormatProperties2Args {
        physical_device: VkPhysicalDevice(PHYSICAL),
        format: RGBA8,
        p_format_properties: Some(VkFormatProperties2 {
            p_next: vec![VkFormatProperties2Next::VkFormatProperties3(
                Default::default(),
            )],
            ..Default::default()
        }),
    });
    let Command::GetPhysicalDeviceFormatProperties2(fp) = h.call(&fp).unwrap() else {
        panic!()
    };
    assert_ne!(
        fp.p_format_properties
            .unwrap()
            .format_properties
            .optimal_tiling_features,
        0
    );
    let Command::CreateImage(ci) = h.call(&create_image(DEVICE, IMAGE, image_info())).unwrap()
    else {
        panic!()
    };
    assert_eq!(ci.ret, VK_SUCCESS);
    let Command::GetImageMemoryRequirements2(mr) =
        h.call(&memory_requirements(DEVICE, IMAGE)).unwrap()
    else {
        panic!()
    };
    let bits = mr
        .p_memory_requirements
        .unwrap()
        .memory_requirements
        .memory_type_bits;
    assert_ne!(bits, 0);
    assert_eq!(
        bits >> guest.memory_type_count,
        0,
        "every bit names a type the guest sees"
    );
    h.send(&destroy_image(DEVICE, IMAGE)).unwrap();
    h.send(&Command::DestroyInstance(DestroyInstanceArgs {
        instance: VkInstance(INSTANCE),
    }))
    .unwrap();
    assert_eq!(h.renderer.factory().host_objects(), 0);
    assert!(!h.fatal());
}

/// `with_device` without the enumeration `boot` would repeat.
fn with_device_on(h: &mut Harness<AshVulkan>) {
    let Command::CreateDevice(d) = h
        .call(&create_device(PHYSICAL, DEVICE, Vec::new()))
        .unwrap()
    else {
        panic!()
    };
    assert_eq!(d.ret, VK_SUCCESS);
    h.send(&create_pool(DEVICE, POOL)).unwrap();
    let Command::GetDeviceQueue2(q) = h.call(&device_queue(DEVICE, QUEUE, 1)).unwrap() else {
        panic!()
    };
    assert_eq!(q.p_queue, Some(VkQueue(QUEUE)));
}

#[test]
fn a_reset_on_the_real_gpu_leaves_nothing_behind() {
    let Some(host) = host() else { return };
    let mut h = Harness::new(host);
    boot(&mut h);
    with_device_on(&mut h);
    h.call(&create_image(DEVICE, IMAGE, image_info())).unwrap();
    assert!(h.renderer.factory().host_objects() > 0);
    crate::renderer::Renderer3d::reset(&mut h.renderer);
    assert_eq!(h.renderer.factory().host_objects(), 0);
}

/// Stage 5b.1 end to end on the host GPU: a buffer in host-visible memory the
/// executor allocated (our pages, imported), the blob the guest makes of that
/// memory mapped into the window, the **host GPU** filling the buffer, and
/// the pattern read back through the blob's pages — the bytes the guest's
/// mapping shows.
#[test]
fn the_host_gpu_fills_a_buffer_and_the_guest_reads_it_through_the_blob() {
    const SIZE: u64 = 64 << 10;
    const MEM_RES: u32 = 20;
    const MAP_AT: u64 = 0x40_0000;
    const PATTERN: u32 = 0xC0FF_EE11;
    let Some(host) = host() else { return };
    let mut h = Harness::new(Arc::clone(&host));
    boot(&mut h);
    with_device_on(&mut h);

    let Command::GetPhysicalDeviceMemoryProperties2(m) =
        h.call(&memory_properties(PHYSICAL)).unwrap()
    else {
        panic!()
    };
    let memory = m.p_memory_properties.unwrap().memory_properties;
    // Every host-visible coherent type the guest may map (types 3 and 4 on
    // the RTX 2070), each through the whole path; Mesa's feedback buffer
    // takes the first of them (`vn_get_memory_type_index`).
    let coherent = 0x2 | 0x4;
    let candidates: Vec<u32> = (0..memory.memory_type_count)
        .filter(|i| memory.memory_types[*i as usize].property_flags & coherent == coherent)
        .collect();
    assert!(!candidates.is_empty(), "a host-visible coherent type");
    for (round, type_index) in candidates.into_iter().enumerate() {
        let res = MEM_RES + u32::try_from(round).unwrap();
        let Command::CreateBuffer(b) = h
            .call(&create_buffer(DEVICE, BUFFER, buffer_info(SIZE, TRANSFER)))
            .unwrap()
        else {
            panic!()
        };
        assert_eq!(b.ret, VK_SUCCESS);
        let Command::GetBufferMemoryRequirements2(r) =
            h.call(&buffer_requirements(DEVICE, BUFFER)).unwrap()
        else {
            panic!()
        };
        let req = r.p_memory_requirements.unwrap().memory_requirements;
        eprintln!(
            "type {type_index} (guest flags {:#x}): buffer of {SIZE:#x} needs {:#x}, alignment {:#x}, memoryTypeBits {:#x}",
            memory.memory_types[type_index as usize].property_flags,
            req.size,
            req.alignment,
            req.memory_type_bits
        );
        assert_ne!(
            req.memory_type_bits & (1 << type_index),
            0,
            "a transfer buffer may live in host-visible memory"
        );
        h.send(&allocate(DEVICE, MEMORY, req.size, type_index, Vec::new()))
            .unwrap();
        h.send(&bind_buffers(DEVICE, &[(BUFFER, MEMORY, 0)]))
            .unwrap();
        assert!(!h.fatal(), "the allocation and the bind were accepted");
        assert!(h.renderer.factory().host_visible_bytes() >= req.size);

        let blob_size = req.size.next_multiple_of(4096);
        h.memory_blob(CTX, res, MEMORY, blob_size)
            .expect("a blob of the memory");
        h.renderer
            .map_blob(res, MAP_AT, blob_size)
            .expect("mapped into the window");
        let pages = h.renderer.blob_pages(res).expect("the blob's pages");
        assert_eq!(
            h.window.at(MAP_AT),
            Some((pages.host_addr(), blob_size)),
            "the guest's mapping is the imported pages"
        );
        // The guest writes something the GPU must overwrite.
        pages
            .write_bytes(0, &vec![0xaa; usize::try_from(SIZE).unwrap()])
            .unwrap();

        let pattern = PATTERN ^ type_index;
        let filled = h
            .renderer
            .factory()
            .with_context(CTX, |ctx| {
                ctx.with_host_buffer(DEVICE, BUFFER, |host, device, queue, family, buffer| {
                    host.fill_buffer(device, (queue, family), buffer, 0, SIZE, pattern)
                })
            })
            .flatten()
            .expect("the context, its device, a queue and the buffer");
        filled.expect("the host GPU filled the buffer");

        let mut seen = vec![0u8; usize::try_from(SIZE).unwrap()];
        pages.read_bytes(0, &mut seen).unwrap();
        let words: Vec<u32> = seen
            .chunks_exact(4)
            .map(|w| u32::from_le_bytes([w[0], w[1], w[2], w[3]]))
            .collect();
        let wrong = words.iter().filter(|w| **w != pattern).count();
        eprintln!(
            "type {type_index}: read back {} words through the blob: first {:#010x}, last {:#010x}, {wrong} wrong",
            words.len(),
            words[0],
            words[words.len() - 1]
        );
        assert_eq!(wrong, 0, "every word the GPU wrote is what the guest reads");

        // Teardown as Mesa does it: the bo first, then the memory, then the
        // buffer; nothing of the host is left.
        h.renderer.unmap_blob(res, MAP_AT);
        h.renderer.destroy_blob(res);
        drop(pages);
        h.send(&free(DEVICE, MEMORY)).unwrap();
        h.send(&destroy_buffer(DEVICE, BUFFER)).unwrap();
        assert_eq!(h.renderer.factory().host_visible_bytes(), 0);
    }
    h.send(&Command::DestroyInstance(DestroyInstanceArgs {
        instance: VkInstance(INSTANCE),
    }))
    .unwrap();
    assert_eq!(h.renderer.factory().host_objects(), 0);
    assert!(!h.fatal());
}
