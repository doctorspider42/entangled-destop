//! The executor against the fake host: the whole spec §1.2 bring-up through a
//! real renderer, ring and reply window, then every way a guest can abuse the
//! object table and the reply window, then teardown and snapshots.

use std::sync::Arc;

use crate::renderer::Renderer3d;
use crate::venus::capset::vk_make_api_version;
use crate::venus::protocol::*;
use crate::venus::wire::{Encoder, COMMAND_GENERATE_REPLY};

use super::fake::{cpu, gpu, FakeVulkan};
use super::harness::*;
use super::policy::{c_name, EXTERNAL_MEMORY_HOST};

fn standard() -> (Harness<FakeVulkan>, Arc<FakeVulkan>) {
    let host = Arc::new(FakeVulkan::standard());
    (Harness::new(Arc::clone(&host)), host)
}

/// A call that must kill the ring on its command, leaving `head` in front of
/// it (after the `SetReply` that preceded it).
fn assert_fatal_on_command(h: &mut Harness<FakeVulkan>, command: &Command<'_>) {
    let head = h.call(command).expect_err("the command must be fatal");
    assert_eq!(
        head, h.last_start,
        "head stays in front of the refused command"
    );
    assert!(h.fatal());
    assert_eq!(h.renderer.factory().context_fatal(CTX), Some(true));
}

// ------------------------------------------------------- the whole bring-up

#[test]
fn the_whole_vulkaninfo_bring_up_is_answered_the_way_mesa_decodes_it() {
    let (mut h, host) = standard();

    // Row 2: min(host loader 1.4.309, 1.3) with the patch kept, as vkr.
    let Command::EnumerateInstanceVersion(v) = h.call(&enumerate_instance_version()).unwrap()
    else {
        panic!()
    };
    assert_eq!(v.ret, VK_SUCCESS);
    assert_eq!(v.p_api_version, Some(vk_make_api_version(0, 1, 3, 309)));

    // Row 3: one host instance, the guest's id echoed.
    let Command::CreateInstance(i) = h.call(&create_instance(INSTANCE)).unwrap() else {
        panic!()
    };
    assert_eq!(i.ret, VK_SUCCESS);
    assert_eq!(i.p_instance, Some(VkInstance(INSTANCE)));
    assert_eq!(host.live("instance"), 1);

    // Rows 4–5: the two-call count protocol; the CPU device is hidden.
    let Command::EnumeratePhysicalDevices(e) = h.call(&enumerate(INSTANCE, None)).unwrap() else {
        panic!()
    };
    assert_eq!((e.ret, e.p_physical_device_count), (VK_SUCCESS, Some(1)));
    assert_eq!(
        e.p_physical_devices, None,
        "a count query echoes a null array"
    );
    let Command::EnumeratePhysicalDevices(e) =
        h.call(&enumerate(INSTANCE, Some(vec![PHYSICAL]))).unwrap()
    else {
        panic!()
    };
    assert_eq!(e.ret, VK_SUCCESS);
    assert_eq!(e.p_physical_device_count, Some(1));
    assert_eq!(e.p_physical_devices, Some(vec![VkPhysicalDevice(PHYSICAL)]));

    // Row 6: the RTX, apiVersion capped at 1.3 (patch kept).
    let Command::GetPhysicalDeviceProperties(p) = h.call(&properties(PHYSICAL)).unwrap() else {
        panic!()
    };
    let p = p.p_properties.unwrap();
    assert_eq!(c_name(&p.device_name), b"NVIDIA GeForce RTX 2070");
    assert_eq!(p.vendor_id, 0x10de);
    assert_eq!(p.api_version, vk_make_api_version(0, 1, 3, 312));

    // Rows 7–8: nothing the protocol cannot decode is advertised — the host's
    // swapchain and external-memory-host are filtered out.
    let ext_query =
        Command::EnumerateDeviceExtensionProperties(EnumerateDeviceExtensionPropertiesArgs {
            physical_device: VkPhysicalDevice(PHYSICAL),
            p_layer_name: None,
            p_property_count: Some(0),
            p_properties: None,
            ret: 0,
        });
    let Command::EnumerateDeviceExtensionProperties(x) = h.call(&ext_query).unwrap() else {
        panic!()
    };
    assert_eq!((x.ret, x.p_property_count), (VK_SUCCESS, Some(0)));

    // Row 9: Features2 with the chain Mesa builds at 1.3 (prepended, so
    // 1.3 first); sparse masked, the rest passed through, order kept.
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
    let f = f.p_features.unwrap();
    assert_eq!(f.features.geometry_shader, 1);
    assert_eq!(f.features.sparse_binding, 0, "sparse is masked");
    assert_eq!(f.features.sparse_residency_image2d, 0);
    assert_eq!(f.features.sparse_residency_aliased, 0);
    match f.p_next.as_slice() {
        [VkPhysicalDeviceFeatures2Next::VkPhysicalDeviceVulkan13Features(v13), VkPhysicalDeviceFeatures2Next::VkPhysicalDeviceVulkan12Features(v12), VkPhysicalDeviceFeatures2Next::VkPhysicalDeviceVulkan11Features(v11)] =>
        {
            assert_eq!(v13.dynamic_rendering, 1);
            assert_eq!(v12.timeline_semaphore, 1);
            assert_eq!(v11.multiview, 1);
        }
        other => panic!("the chain came back as {other:?}"),
    }

    // Row 10: Properties2, apiVersion capped here too.
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
    assert_eq!(p2.properties.api_version, vk_make_api_version(0, 1, 3, 312));
    match p2.p_next.as_slice() {
        [VkPhysicalDeviceProperties2Next::VkPhysicalDeviceVulkan13Properties(v13), VkPhysicalDeviceProperties2Next::VkPhysicalDeviceVulkan12Properties(v12), VkPhysicalDeviceProperties2Next::VkPhysicalDeviceVulkan11Properties(v11)] =>
        {
            assert_eq!(v13.max_inline_uniform_block_size, 256);
            assert_eq!(c_name(&v12.driver_name), b"NVIDIA");
            assert_eq!(v11.subgroup_size, 32);
        }
        other => panic!("the chain came back as {other:?}"),
    }

    // Rows 11–12: queue families, count then array.
    let qf = |count: u32, array: bool| {
        Command::GetPhysicalDeviceQueueFamilyProperties2(
            GetPhysicalDeviceQueueFamilyProperties2Args {
                physical_device: VkPhysicalDevice(PHYSICAL),
                p_queue_family_property_count: Some(count),
                p_queue_family_properties: array
                    .then(|| vec![VkQueueFamilyProperties2::default(); count as usize]),
            },
        )
    };
    let Command::GetPhysicalDeviceQueueFamilyProperties2(q) = h.call(&qf(0, false)).unwrap() else {
        panic!()
    };
    assert_eq!(q.p_queue_family_property_count, Some(2));
    let Command::GetPhysicalDeviceQueueFamilyProperties2(q) = h.call(&qf(2, true)).unwrap() else {
        panic!()
    };
    let families = q.p_queue_family_properties.unwrap();
    assert_eq!(families[0].queue_family_properties.queue_count, 16);
    assert_eq!(families[1].queue_family_properties.queue_flags, 0xc);

    // Row 13: the memory policy. Indices unchanged; the BAR type (5) is no
    // longer host visible; 3 and 4, which import our pages, still are.
    let mem = Command::GetPhysicalDeviceMemoryProperties2(GetPhysicalDeviceMemoryProperties2Args {
        physical_device: VkPhysicalDevice(PHYSICAL),
        p_memory_properties: Some(Default::default()),
    });
    let Command::GetPhysicalDeviceMemoryProperties2(m) = h.call(&mem).unwrap() else {
        panic!()
    };
    let m = m.p_memory_properties.unwrap().memory_properties;
    assert_eq!(m.memory_type_count, 6);
    let flags: Vec<u32> = m.memory_types[..6]
        .iter()
        .map(|t| t.property_flags)
        .collect();
    assert_eq!(flags, vec![0x1, 0x1, 0x1, 0x6, 0xe, 0x1]);
    assert!(super::policy::has_coherent_host_type(&m));

    // Rows 14–15: groups; the second call's id-0 slots are filled in.
    let groups = |count: u32, array: bool| {
        Command::EnumeratePhysicalDeviceGroups(EnumeratePhysicalDeviceGroupsArgs {
            instance: VkInstance(INSTANCE),
            p_physical_device_group_count: Some(count),
            p_physical_device_group_properties: array
                .then(|| vec![VkPhysicalDeviceGroupProperties::default(); count as usize]),
            ret: 0,
        })
    };
    let Command::EnumeratePhysicalDeviceGroups(g) = h.call(&groups(0, false)).unwrap() else {
        panic!()
    };
    assert_eq!(
        (g.ret, g.p_physical_device_group_count),
        (VK_SUCCESS, Some(1))
    );
    let Command::EnumeratePhysicalDeviceGroups(g) = h.call(&groups(1, true)).unwrap() else {
        panic!()
    };
    let group = &g.p_physical_device_group_properties.unwrap()[0];
    assert_eq!(group.physical_device_count, 1);
    assert_eq!(group.physical_devices[0], VkPhysicalDevice(PHYSICAL));
    assert_eq!(group.physical_devices[1], VkPhysicalDevice(0));

    // Row 17: the device, with a feature the guest was told is there.
    let chain = vec![VkDeviceCreateInfoNext::VkPhysicalDeviceVulkan12Features(
        VkPhysicalDeviceVulkan12Features {
            timeline_semaphore: 1,
            ..Default::default()
        },
    )];
    let Command::CreateDevice(d) = h.call(&create_device(PHYSICAL, DEVICE, chain)).unwrap() else {
        panic!()
    };
    assert_eq!((d.ret, d.p_device), (VK_SUCCESS, Some(VkDevice(DEVICE))));
    let request = host.device_requests().pop().unwrap();
    assert_eq!(request.extensions, vec![EXTERNAL_MEMORY_HOST.to_owned()]);
    assert_eq!(request.queues.len(), 1);
    assert_eq!(
        request.chain.len(),
        1,
        "the feature link is rebuilt, not dropped"
    );

    // Row 18: the feedback command pool, async.
    h.send(&create_pool(DEVICE, POOL)).unwrap();
    assert_eq!(host.live("command pool"), 1);

    // Row 19: the queue, bound to fence timeline 1.
    let Command::GetDeviceQueue2(q) = h.call(&device_queue(DEVICE, QUEUE, 1)).unwrap() else {
        panic!()
    };
    assert_eq!(q.p_queue, Some(VkQueue(QUEUE)));

    // Rows 20–21: format probing.
    for format in [RGBA8, 50, 126] {
        let cmd =
            Command::GetPhysicalDeviceFormatProperties2(GetPhysicalDeviceFormatProperties2Args {
                physical_device: VkPhysicalDevice(PHYSICAL),
                format,
                p_format_properties: Some(VkFormatProperties2 {
                    p_next: vec![VkFormatProperties2Next::VkFormatProperties3(
                        Default::default(),
                    )],
                    ..Default::default()
                }),
            });
        let Command::GetPhysicalDeviceFormatProperties2(fp) = h.call(&cmd).unwrap() else {
            panic!()
        };
        let fp = fp.p_format_properties.unwrap();
        assert_eq!(fp.format_properties.optimal_tiling_features, 0x1_d401);
        let [VkFormatProperties2Next::VkFormatProperties3(p3)] = fp.p_next.as_slice() else {
            panic!("the chain came back as {:?}", fp.p_next)
        };
        assert_eq!(p3.optimal_tiling_features, 0x1_d401);
    }
    let ifp = Command::GetPhysicalDeviceImageFormatProperties2(
        GetPhysicalDeviceImageFormatProperties2Args {
            physical_device: VkPhysicalDevice(PHYSICAL),
            p_image_format_info: Some(VkPhysicalDeviceImageFormatInfo2 {
                p_next: Vec::new(),
                format: RGBA8,
                type_: 1,
                tiling: 0,
                usage: 0x4,
                flags: 0,
            }),
            p_image_format_properties: Some(Default::default()),
            ret: 0,
        },
    );
    let Command::GetPhysicalDeviceImageFormatProperties2(r) = h.call(&ifp).unwrap() else {
        panic!()
    };
    assert_eq!(r.ret, VK_SUCCESS);
    assert_eq!(
        r.p_image_format_properties
            .unwrap()
            .image_format_properties
            .max_extent
            .width,
        16384
    );

    // Row 22: an image and its requirements; memoryTypeBits name the same
    // (unchanged) indices as the host's, minus the host-visible types an
    // optimal image cannot be bound to (stage 5b.1: the fake driver imports
    // host memory for linear images only).
    let Command::CreateImage(ci) = h.call(&create_image(DEVICE, IMAGE, image_info())).unwrap()
    else {
        panic!()
    };
    assert_eq!((ci.ret, ci.p_image), (VK_SUCCESS, Some(VkImage(IMAGE))));
    let Command::GetImageMemoryRequirements2(mr) =
        h.call(&memory_requirements(DEVICE, IMAGE)).unwrap()
    else {
        panic!()
    };
    let mr = mr.p_memory_requirements.unwrap();
    assert_eq!(mr.memory_requirements.memory_type_bits, 0x3f & !0x18);
    let [VkMemoryRequirements2Next::VkMemoryDedicatedRequirements(dedicated)] =
        mr.p_next.as_slice()
    else {
        panic!()
    };
    assert_eq!(dedicated.prefers_dedicated_allocation, 1);

    // Rows 23–24: async teardown of the image, pool and device.
    h.send(&destroy_image(DEVICE, IMAGE)).unwrap();
    h.send(&Command::DestroyCommandPool(DestroyCommandPoolArgs {
        device: VkDevice(DEVICE),
        command_pool: VkCommandPool(POOL),
    }))
    .unwrap();
    h.send(&Command::DestroyDevice(DestroyDeviceArgs {
        device: VkDevice(DEVICE),
    }))
    .unwrap();
    assert_eq!(host.live("device"), 0);
    assert_eq!(host.live("image"), 0);

    // Row 25: vulkaninfo's second device, with a device group.
    let chain = vec![VkDeviceCreateInfoNext::VkDeviceGroupDeviceCreateInfo(
        VkDeviceGroupDeviceCreateInfo {
            physical_device_count: 1,
            p_physical_devices: Some(vec![VkPhysicalDevice(PHYSICAL)]),
        },
    )];
    let Command::CreateDevice(d) = h.call(&create_device(PHYSICAL, DEVICE + 1, chain)).unwrap()
    else {
        panic!()
    };
    assert_eq!(d.ret, VK_SUCCESS);
    let request = host.device_requests().pop().unwrap();
    assert_eq!(
        request.group,
        Some(vec![1]),
        "the guest id became the host's device 1"
    );
    assert!(
        request.chain.is_empty(),
        "the group travels translated, not raw"
    );
    h.call(&device_queue(DEVICE + 1, QUEUE + 1, 1))
        .expect("timeline 1 is free again");
    h.send(&Command::DestroyDevice(DestroyDeviceArgs {
        device: VkDevice(DEVICE + 1),
    }))
    .unwrap();

    // Row 26: the instance goes, and with it every host object.
    h.send(&Command::DestroyInstance(DestroyInstanceArgs {
        instance: VkInstance(INSTANCE),
    }))
    .unwrap();
    assert_eq!(host.live_objects(), 0);
    assert_eq!(h.renderer.factory().host_objects(), 0);
    assert!(!h.fatal());
}

#[test]
fn a_short_array_is_answered_incomplete_and_re_enumeration_keeps_its_ids() {
    let host = Arc::new(FakeVulkan::new(vec![gpu("A"), cpu(), gpu("B")]));
    let mut h = Harness::new(Arc::clone(&host));
    h.call(&create_instance(INSTANCE)).unwrap();
    let Command::EnumeratePhysicalDevices(e) = h.call(&enumerate(INSTANCE, None)).unwrap() else {
        panic!()
    };
    assert_eq!(
        e.p_physical_device_count,
        Some(2),
        "two GPUs; the CPU is hidden"
    );
    let Command::EnumeratePhysicalDevices(e) =
        h.call(&enumerate(INSTANCE, Some(vec![PHYSICAL]))).unwrap()
    else {
        panic!()
    };
    assert_eq!(e.ret, VK_INCOMPLETE);
    assert_eq!(e.p_physical_device_count, Some(1));
    assert_eq!(e.p_physical_devices, Some(vec![VkPhysicalDevice(PHYSICAL)]));
    let Command::EnumeratePhysicalDevices(e) = h
        .call(&enumerate(INSTANCE, Some(vec![PHYSICAL, PHYSICAL + 1])))
        .unwrap()
    else {
        panic!()
    };
    assert_eq!(e.ret, VK_SUCCESS);
    assert_eq!(e.p_physical_device_count, Some(2));
    let Command::GetPhysicalDeviceProperties(p) = h.call(&properties(PHYSICAL + 1)).unwrap() else {
        panic!()
    };
    assert_eq!(c_name(&p.p_properties.unwrap().device_name), b"B");

    // A re-enumeration that renames a bound device is fatal (vkr).
    assert_fatal_on_command(
        &mut h,
        &enumerate(INSTANCE, Some(vec![PHYSICAL + 7, PHYSICAL + 1])),
    );
}

#[test]
fn a_host_with_only_a_cpu_device_shows_the_guest_none() {
    let host = Arc::new(FakeVulkan::new(vec![cpu()]));
    let mut h = Harness::new(Arc::clone(&host));
    h.call(&create_instance(INSTANCE)).unwrap();
    let Command::EnumeratePhysicalDevices(e) = h.call(&enumerate(INSTANCE, None)).unwrap() else {
        panic!()
    };
    assert_eq!(e.p_physical_device_count, Some(0));
    assert!(
        super::probe(&*host).is_err(),
        "and the run is refused up front"
    );
    assert_eq!(
        host.live_objects(),
        1,
        "the probe cleaned up; the guest's instance is still live"
    );
}

#[test]
fn a_device_without_importable_coherent_memory_is_hidden() {
    // Our pages import only into a device-local type: nothing the guest
    // could map would be ours.
    let mut bar_only = gpu("device-local imports only");
    bar_only.info.host_import_types = Some(1 << 0);
    let mut no_ext = gpu("no import");
    no_ext
        .info
        .extensions
        .retain(|e| c_name(&e.extension_name) != EXTERNAL_MEMORY_HOST.as_bytes());
    let host = FakeVulkan::new(vec![bar_only, no_ext, gpu("fine")]);
    let shown = super::probe(&host).expect("one device survives");
    assert_eq!(shown.len(), 1);
    assert_eq!(shown[0].name(), "fine");
    assert_eq!(host.live_objects(), 0);
}

#[test]
fn layers_and_instance_extensions_are_refused_as_vkr_refuses_them() {
    let (mut h, host) = standard();
    let mut layered = create_instance(INSTANCE);
    if let Command::CreateInstance(args) = &mut layered {
        let info = args.p_create_info.as_mut().unwrap();
        info.enabled_layer_count = 1;
        info.pp_enabled_layer_names = Some(vec![b"VK_LAYER_KHRONOS_validation"]);
    }
    let Command::CreateInstance(r) = h.call(&layered).unwrap() else {
        panic!()
    };
    assert_eq!(r.ret, VK_ERROR_LAYER_NOT_PRESENT);
    let mut extended = create_instance(INSTANCE);
    if let Command::CreateInstance(args) = &mut extended {
        let info = args.p_create_info.as_mut().unwrap();
        info.enabled_extension_count = 1;
        info.pp_enabled_extension_names = Some(vec![b"VK_KHR_surface"]);
    }
    let Command::CreateInstance(r) = h.call(&extended).unwrap() else {
        panic!()
    };
    assert_eq!(r.ret, VK_ERROR_EXTENSION_NOT_PRESENT);
    assert_eq!(host.live_objects(), 0, "neither reached the host");
    // A second instance on the context is fatal.
    h.call(&create_instance(INSTANCE)).unwrap();
    assert_fatal_on_command(&mut h, &create_instance(INSTANCE + 1));
}

#[test]
fn a_device_asking_for_what_it_was_not_offered_is_refused_in_vulkan_terms() {
    let (mut h, host) = standard();
    boot(&mut h);
    // Sparse was masked, so asking for it is FEATURE_NOT_PRESENT.
    let mut sparse = create_device(PHYSICAL, DEVICE, Vec::new());
    if let Command::CreateDevice(args) = &mut sparse {
        args.p_create_info.as_mut().unwrap().p_enabled_features = Some(VkPhysicalDeviceFeatures {
            sparse_binding: 1,
            ..Default::default()
        });
    }
    let Command::CreateDevice(r) = h.call(&sparse).unwrap() else {
        panic!()
    };
    assert_eq!(r.ret, VK_ERROR_FEATURE_NOT_PRESENT);
    // A host extension we did not advertise is EXTENSION_NOT_PRESENT.
    let mut swapchain = create_device(PHYSICAL, DEVICE, Vec::new());
    if let Command::CreateDevice(args) = &mut swapchain {
        let info = args.p_create_info.as_mut().unwrap();
        info.enabled_extension_count = 1;
        info.pp_enabled_extension_names = Some(vec![b"VK_KHR_swapchain"]);
    }
    let Command::CreateDevice(r) = h.call(&swapchain).unwrap() else {
        panic!()
    };
    assert_eq!(r.ret, VK_ERROR_EXTENSION_NOT_PRESENT);
    // A queue family the host does not have is VK_ERROR_UNKNOWN, as vkr.
    let mut family = create_device(PHYSICAL, DEVICE, Vec::new());
    if let Command::CreateDevice(args) = &mut family {
        let queues = args
            .p_create_info
            .as_mut()
            .unwrap()
            .p_queue_create_infos
            .as_mut()
            .unwrap();
        queues[0].queue_family_index = 9;
    }
    let Command::CreateDevice(r) = h.call(&family).unwrap() else {
        panic!()
    };
    assert_eq!(r.ret, VK_ERROR_UNKNOWN);
    assert!(
        host.device_requests().is_empty(),
        "none of them reached the host"
    );
    // A priority outside [0, 1] is not a request, it is a broken guest.
    let mut priority = create_device(PHYSICAL, DEVICE, Vec::new());
    if let Command::CreateDevice(args) = &mut priority {
        let queues = args
            .p_create_info
            .as_mut()
            .unwrap()
            .p_queue_create_infos
            .as_mut()
            .unwrap();
        queues[0].p_queue_priorities = Some(vec![f32::NAN]);
    }
    assert_fatal_on_command(&mut h, &priority);
}

// ----------------------------------------------------- object-table abuse

#[test]
fn a_duplicate_id_is_fatal_whatever_type_holds_it() {
    let (mut h, _) = standard();
    boot(&mut h);
    // The instance's id, reused for a device.
    assert_fatal_on_command(&mut h, &create_device(PHYSICAL, INSTANCE, Vec::new()));
}

#[test]
fn an_unknown_id_is_fatal() {
    let (mut h, _) = standard();
    boot(&mut h);
    assert_fatal_on_command(&mut h, &properties(0x999));
}

#[test]
fn an_id_of_the_wrong_type_is_fatal() {
    let (mut h, _) = standard();
    boot(&mut h);
    // The instance, where a physical device belongs.
    assert_fatal_on_command(&mut h, &properties(INSTANCE));
}

#[test]
fn id_zero_is_fatal_where_the_handle_is_required_and_a_no_op_where_it_is_optional() {
    let (mut h, host) = standard();
    with_device(&mut h);
    h.send(&destroy_image(DEVICE, 0))
        .expect("vkDestroyImage(VK_NULL_HANDLE) is a no-op");
    assert_eq!(host.live("image"), 0);
    assert_fatal_on_command(&mut h, &create_pool(DEVICE, 0));
}

#[test]
fn destroying_an_unknown_id_is_fatal() {
    let (mut h, _) = standard();
    with_device(&mut h);
    let head = h.send(&destroy_image(DEVICE, 0x1234)).expect_err("fatal");
    assert_eq!(head, h.last_start);
}

#[test]
fn using_an_object_after_destroying_it_is_fatal() {
    let (mut h, host) = standard();
    with_device(&mut h);
    h.call(&create_image(DEVICE, IMAGE, image_info())).unwrap();
    h.send(&destroy_image(DEVICE, IMAGE)).unwrap();
    assert_eq!(host.live("image"), 0);
    assert_fatal_on_command(&mut h, &memory_requirements(DEVICE, IMAGE));
}

#[test]
fn a_child_named_through_the_wrong_parent_is_fatal() {
    let (mut h, _) = standard();
    with_device(&mut h);
    h.call(&create_image(DEVICE, IMAGE, image_info())).unwrap();
    // A second device, and the first device's image named through it.
    h.call(&create_device(PHYSICAL, DEVICE + 1, Vec::new()))
        .unwrap();
    assert_fatal_on_command(&mut h, &memory_requirements(DEVICE + 1, IMAGE));
}

#[test]
fn queue_timelines_follow_vkr() {
    let (mut h, _) = standard();
    with_device(&mut h);
    // The same queue fetched twice is fatal.
    assert_fatal_on_command(&mut h, &device_queue(DEVICE, QUEUE + 1, 2));

    let (mut h, _) = standard();
    boot(&mut h);
    h.call(&create_device(PHYSICAL, DEVICE, Vec::new()))
        .unwrap();
    // Timeline 0 is the CPU's; 64 is past the end.
    assert_fatal_on_command(&mut h, &device_queue(DEVICE, QUEUE, 0));
    let (mut h, _) = standard();
    boot(&mut h);
    h.call(&create_device(PHYSICAL, DEVICE, Vec::new()))
        .unwrap();
    assert_fatal_on_command(&mut h, &device_queue(DEVICE, QUEUE, 64));
}

#[test]
fn an_image_outside_vulkan_1_3_never_reaches_the_driver() {
    for (what, info) in [
        (
            "sparse",
            VkImageCreateInfo {
                flags: 0x1,
                ..image_info()
            },
        ),
        (
            "an extension format",
            VkImageCreateInfo {
                format: 1_000_054_000,
                ..image_info()
            },
        ),
        (
            "an extension tiling",
            VkImageCreateInfo {
                tiling: 1_000_158_000,
                ..image_info()
            },
        ),
        (
            "an unknown usage bit",
            VkImageCreateInfo {
                usage: 0x100,
                ..image_info()
            },
        ),
        (
            "an impossible layout",
            VkImageCreateInfo {
                initial_layout: 2,
                ..image_info()
            },
        ),
        (
            "three samples",
            VkImageCreateInfo {
                samples: 3,
                ..image_info()
            },
        ),
        (
            "larger than the host allows",
            VkImageCreateInfo {
                extent: VkExtent3D {
                    width: 1 << 20,
                    height: 1,
                    depth: 1,
                },
                ..image_info()
            },
        ),
    ] {
        let (mut h, host) = standard();
        with_device(&mut h);
        let head = h.call(&create_image(DEVICE, IMAGE, info)).expect_err(what);
        assert_eq!(head, h.last_start, "{what}");
        assert!(host.image_requests() == 0, "{what} reached the driver");
    }
}

// ------------------------------------------------------ reply-window abuse

fn version_bytes() -> Vec<u8> {
    call_bytes(&enumerate_instance_version())
}

#[test]
fn a_reply_with_no_window_bound_is_fatal_before_it_runs() {
    let (mut h, host) = standard();
    let create = call_bytes(&create_instance(INSTANCE));
    assert_eq!(h.submit(&create), Outcome::Fatal { head: 0 });
    assert_eq!(
        host.live_objects(),
        0,
        "the refused command had no side effect"
    );
}

#[test]
fn a_window_outside_its_blob_is_fatal() {
    let (mut h, _) = standard();
    let bytes = set_reply(REPLY_RES, REPLY_BYTES - 8, 16);
    assert_eq!(h.submit(&bytes), Outcome::Fatal { head: 0 });
    let (mut h, _) = standard();
    let bytes = set_reply(REPLY_RES, u64::MAX, 2);
    assert_eq!(h.submit(&bytes), Outcome::Fatal { head: 0 });
}

#[test]
fn a_window_in_a_resource_that_is_not_a_host_blob_of_this_context_is_fatal() {
    let (mut h, _) = standard();
    assert_eq!(
        h.submit(&set_reply(0x4242, 0, 64)),
        Outcome::Fatal { head: 0 }
    );

    let (mut h, _) = standard();
    h.renderer
        .ctx_create(2, crate::CAPSET_VENUS, "other")
        .expect("a second context");
    h.create_blob(2, 9, 4096);
    assert_eq!(h.submit(&set_reply(9, 0, 64)), Outcome::Fatal { head: 0 });
}

#[test]
fn a_reply_that_does_not_fit_its_window_is_fatal_and_writes_nothing() {
    let (mut h, _) = standard();
    let junk = [0xcc; 64];
    h.reply.write_bytes(0, &junk).unwrap();
    assert_eq!(h.submit(&set_reply(REPLY_RES, 0, 8)), Outcome::Consumed);
    // The version reply is 20 bytes.
    let at = h.tail();
    assert_eq!(h.submit(&version_bytes()), Outcome::Fatal { head: at });
    let mut after = [0u8; 64];
    h.reply.read_bytes(0, &mut after).unwrap();
    assert_eq!(after, junk, "not one byte of a reply that did not fit");
}

#[test]
fn a_seek_moves_the_cursor_and_a_seek_past_the_window_is_fatal() {
    let (mut h, _) = standard();
    let mut bytes = set_reply(REPLY_RES, 0x100, 64);
    bytes.extend(seek_reply(8));
    bytes.extend(version_bytes());
    assert_eq!(h.submit(&bytes), Outcome::Consumed);
    let mut reply = [0u8; 20];
    h.reply.read_bytes(0x108, &mut reply).unwrap();
    assert_eq!(
        &reply[..4],
        &137u32.to_le_bytes(),
        "the reply landed after the seek"
    );

    let at = h.tail();
    assert_eq!(h.submit(&seek_reply(65)), Outcome::Fatal { head: at });
}

#[test]
fn a_window_whose_blob_is_destroyed_is_never_written_again() {
    let (mut h, _) = standard();
    assert_eq!(h.submit(&set_reply(REPLY_RES, 0, 64)), Outcome::Consumed);
    let pages = Arc::clone(&h.reply);
    let junk = [0xcc; 64];
    pages.write_bytes(0, &junk).unwrap();
    h.renderer.destroy_blob(REPLY_RES);
    let at = h.tail();
    assert_eq!(h.submit(&version_bytes()), Outcome::Fatal { head: at });
    let mut after = [0u8; 64];
    pages.read_bytes(0, &mut after).unwrap();
    assert_eq!(after, junk);
}

#[test]
fn a_transport_command_asking_for_a_reply_is_fatal() {
    let (mut h, _) = standard();
    let mut bytes = set_reply(REPLY_RES, 0, 64);
    bytes[4] = COMMAND_GENERATE_REPLY as u8;
    assert_eq!(h.submit(&bytes), Outcome::Fatal { head: 0 });
}

#[test]
fn a_command_the_protocol_cannot_carry_is_fatal_and_left_unconsumed() {
    let (mut h, _) = standard();
    assert_eq!(h.submit(&set_reply(REPLY_RES, 0, 64)), Outcome::Consumed);
    // vkMapMemory, opcode 23: a real command, which the protocol does not
    // serialize (a host pointer out), so nothing decodes it.
    assert_eq!(command_type_name(23), Some("vkMapMemory"));
    let mut enc = Encoder::new();
    enc.command_header(crate::venus::wire::CommandHeader {
        opcode: 23,
        flags: COMMAND_GENERATE_REPLY,
    })
    .unwrap();
    enc.handle(DEVICE).unwrap();
    let at = h.tail();
    assert_eq!(
        h.submit(&enc.finish().unwrap()),
        Outcome::Fatal { head: at }
    );
}

#[test]
fn a_decodable_command_this_stage_does_not_implement_is_fatal_and_left_unconsumed() {
    let (mut h, _) = standard();
    assert_eq!(h.submit(&set_reply(REPLY_RES, 0, 64)), Outcome::Consumed);
    // vkGetPhysicalDeviceFeatures (v1), opcode 3: generated, decodable, and
    // not one this stage answers.
    assert_eq!(command_type_name(3), Some("vkGetPhysicalDeviceFeatures"));
    assert!(GENERATED_COMMANDS.iter().any(|(op, _)| *op == 3));
    let mut enc = Encoder::new();
    enc.command_header(crate::venus::wire::CommandHeader {
        opcode: 3,
        flags: COMMAND_GENERATE_REPLY,
    })
    .unwrap();
    enc.handle(PHYSICAL).unwrap();
    enc.simple_pointer(true).unwrap();
    let at = h.tail();
    assert_eq!(
        h.submit(&enc.finish().unwrap()),
        Outcome::Fatal { head: at }
    );
}

#[test]
fn a_decodable_command_without_a_handler_is_refused_as_not_implemented() {
    let mut ctx = super::VulkanContext::new(CTX, Arc::new(FakeVulkan::standard()));
    let mut command = Command::GetPhysicalDeviceFeatures(GetPhysicalDeviceFeaturesArgs {
        physical_device: VkPhysicalDevice(PHYSICAL),
        p_features: Some(VkPhysicalDeviceFeatures::default()),
    });
    assert_eq!(
        ctx.execute(&mut command),
        Err(super::ExecError::NotImplemented {
            command: "vkGetPhysicalDeviceFeatures"
        })
    );
    assert!(ctx.is_fatal(), "the context is done, as for any refusal");

    // An extension's command buffer command (no extension is advertised):
    // refused, not silently recorded.
    let mut ctx = super::VulkanContext::new(CTX, Arc::new(FakeVulkan::standard()));
    let mut stipple = Command::CmdSetLineStippleEnableEXT(CmdSetLineStippleEnableEXTArgs {
        command_buffer: VkCommandBuffer(0x99),
        stippled_line_enable: 1,
    });
    assert_eq!(
        ctx.execute(&mut stipple),
        Err(super::ExecError::NotImplemented {
            command: "vkCmdSetLineStippleEnableEXT"
        })
    );

    // Stage 5b.2 serves vkCmdDraw; on a command buffer nobody allocated it
    // is refused as the unknown id it is.
    let mut ctx = super::VulkanContext::new(CTX, Arc::new(FakeVulkan::standard()));
    let mut draw = Command::CmdDraw(CmdDrawArgs {
        command_buffer: VkCommandBuffer(0x99),
        vertex_count: 3,
        instance_count: 1,
        first_vertex: 0,
        first_instance: 0,
    });
    assert_eq!(
        ctx.execute(&mut draw),
        Err(super::ExecError::Id {
            command: "vkCmdDraw",
            error: super::objects::IdError::Unknown {
                id: 0x99,
                expected: "VkCommandBuffer"
            }
        })
    );
}

#[test]
fn a_chained_structure_this_stage_does_not_implement_is_fatal_and_never_reaches_the_host() {
    let drm = || {
        VkImageCreateInfoNext::VkImageDrmFormatModifierListCreateInfoEXT(
            VkImageDrmFormatModifierListCreateInfoEXT {
                drm_format_modifier_count: 1,
                p_drm_format_modifiers: Some(vec![0]),
            },
        )
    };
    // It decodes: the protocol admits it in VkImageCreateInfo's chain.
    let info = VkImageCreateInfo {
        p_next: vec![drm()],
        ..image_info()
    };
    let bytes = call_bytes(&create_image(DEVICE, IMAGE, info.clone()));
    let mut dec = crate::venus::wire::Decoder::new(&bytes);
    let (_, mut decoded) = Command::decode_next(&mut dec).expect("decodes");

    // The executor refuses it, naming it, before anything else is judged.
    let mut ctx = super::VulkanContext::new(CTX, Arc::new(FakeVulkan::standard()));
    assert_eq!(
        ctx.execute(&mut decoded),
        Err(super::ExecError::UnimplementedLink {
            command: "vkCreateImage",
            parent: "VkImageCreateInfo",
            stype: VK_STRUCTURE_TYPE_IMAGE_DRM_FORMAT_MODIFIER_LIST_CREATE_INFO_EXT,
            name: "VkImageDrmFormatModifierListCreateInfoEXT",
        })
    );

    // Through the ring: fatal on that command, and the host never asked.
    let (mut h, host) = standard();
    with_device(&mut h);
    let head = h
        .call(&create_image(DEVICE, IMAGE, info))
        .expect_err("an unimplemented link is fatal");
    assert_eq!(head, h.last_start, "head stays in front of the command");
    assert_eq!(host.image_requests(), 0);
    assert!(h.fatal());
}

/// Every sType `N`'s whitelist holds, found by asking it to decode each
/// structure the protocol knows from no bytes at all: an admitted one fails
/// on the missing bytes, an unadmitted one is not recognised.
fn whitelist<N: ChainLink<'static>>() -> Vec<i32> {
    info::STRUCTURES
        .iter()
        .filter(|s| {
            let mut dec = crate::venus::wire::Decoder::new(&[]);
            N::decode_body(s.stype, &mut dec, false).is_some()
        })
        .map(|s| s.stype)
        .collect()
}

fn admitted_names<N: ChainLink<'static>>() -> Vec<&'static str> {
    let mut names: Vec<&'static str> = whitelist::<N>()
        .into_iter()
        .filter(|s| super::policy::admits_link(*s))
        .filter_map(|s| info::structure(s).map(|i| i.name))
        .collect();
    names.sort_unstable();
    names
}

#[test]
fn the_links_this_stage_admits_are_exactly_the_ones_the_bring_up_protocol_decoded() {
    // The pNext whitelists of the bring-up commands as the generator emitted
    // them with `[api] 1.3` and the two venus extensions, before it
    // generated the whole protocol: the executor's policy must keep
    // admitting exactly these, whatever the decoder now accepts.
    let sorted = |mut v: Vec<&'static str>| {
        v.sort_unstable();
        v
    };
    let features = [
        "VkPhysicalDevicePrivateDataFeatures",
        "VkPhysicalDeviceVariablePointersFeatures",
        "VkPhysicalDeviceMultiviewFeatures",
        "VkPhysicalDevice16BitStorageFeatures",
        "VkPhysicalDeviceShaderSubgroupExtendedTypesFeatures",
        "VkPhysicalDeviceSamplerYcbcrConversionFeatures",
        "VkPhysicalDeviceProtectedMemoryFeatures",
        "VkPhysicalDeviceInlineUniformBlockFeatures",
        "VkPhysicalDeviceMaintenance4Features",
        "VkPhysicalDeviceShaderDrawParametersFeatures",
        "VkPhysicalDeviceShaderFloat16Int8Features",
        "VkPhysicalDeviceHostQueryResetFeatures",
        "VkPhysicalDeviceDescriptorIndexingFeatures",
        "VkPhysicalDeviceTimelineSemaphoreFeatures",
        "VkPhysicalDevice8BitStorageFeatures",
        "VkPhysicalDeviceVulkanMemoryModelFeatures",
        "VkPhysicalDeviceShaderAtomicInt64Features",
        "VkPhysicalDeviceScalarBlockLayoutFeatures",
        "VkPhysicalDeviceUniformBufferStandardLayoutFeatures",
        "VkPhysicalDeviceBufferDeviceAddressFeatures",
        "VkPhysicalDeviceImagelessFramebufferFeatures",
        "VkPhysicalDeviceTextureCompressionASTCHDRFeatures",
        "VkPhysicalDeviceSeparateDepthStencilLayoutsFeatures",
        "VkPhysicalDeviceShaderDemoteToHelperInvocationFeatures",
        "VkPhysicalDeviceSubgroupSizeControlFeatures",
        "VkPhysicalDevicePipelineCreationCacheControlFeatures",
        "VkPhysicalDeviceVulkan11Features",
        "VkPhysicalDeviceVulkan12Features",
        "VkPhysicalDeviceVulkan13Features",
        "VkPhysicalDeviceZeroInitializeWorkgroupMemoryFeatures",
        "VkPhysicalDeviceImageRobustnessFeatures",
        "VkPhysicalDeviceShaderTerminateInvocationFeatures",
        "VkPhysicalDeviceSynchronization2Features",
        "VkPhysicalDeviceShaderIntegerDotProductFeatures",
        "VkPhysicalDeviceDynamicRenderingFeatures",
    ];
    let mut device = features.to_vec();
    device.extend([
        "VkDevicePrivateDataCreateInfo",
        "VkPhysicalDeviceFeatures2",
        "VkDeviceGroupDeviceCreateInfo",
    ]);
    assert_eq!(admitted_names::<VkDeviceCreateInfoNext>(), sorted(device));
    assert_eq!(
        admitted_names::<VkPhysicalDeviceFeatures2Next>(),
        sorted(features.to_vec())
    );
    assert_eq!(
        admitted_names::<VkPhysicalDeviceProperties2Next>(),
        sorted(vec![
            "VkPhysicalDeviceDriverProperties",
            "VkPhysicalDeviceIDProperties",
            "VkPhysicalDeviceMultiviewProperties",
            "VkPhysicalDeviceSubgroupProperties",
            "VkPhysicalDevicePointClippingProperties",
            "VkPhysicalDeviceProtectedMemoryProperties",
            "VkPhysicalDeviceSamplerFilterMinmaxProperties",
            "VkPhysicalDeviceInlineUniformBlockProperties",
            "VkPhysicalDeviceMaintenance3Properties",
            "VkPhysicalDeviceMaintenance4Properties",
            "VkPhysicalDeviceFloatControlsProperties",
            "VkPhysicalDeviceDescriptorIndexingProperties",
            "VkPhysicalDeviceTimelineSemaphoreProperties",
            "VkPhysicalDeviceDepthStencilResolveProperties",
            "VkPhysicalDeviceTexelBufferAlignmentProperties",
            "VkPhysicalDeviceSubgroupSizeControlProperties",
            "VkPhysicalDeviceVulkan11Properties",
            "VkPhysicalDeviceVulkan12Properties",
            "VkPhysicalDeviceVulkan13Properties",
            "VkPhysicalDeviceShaderIntegerDotProductProperties",
        ])
    );
    assert_eq!(
        admitted_names::<VkFormatProperties2Next>(),
        ["VkFormatProperties3"]
    );
    assert_eq!(
        admitted_names::<VkPhysicalDeviceImageFormatInfo2Next>(),
        sorted(vec![
            "VkPhysicalDeviceExternalImageFormatInfo",
            "VkImageFormatListCreateInfo",
            "VkImageStencilUsageCreateInfo",
        ])
    );
    assert_eq!(
        admitted_names::<VkImageFormatProperties2Next>(),
        sorted(vec![
            "VkExternalImageFormatProperties",
            "VkSamplerYcbcrConversionImageFormatProperties",
        ])
    );
    assert_eq!(
        admitted_names::<VkDeviceQueueInfo2Next>(),
        ["VkDeviceQueueTimelineInfoMESA"]
    );
    assert_eq!(
        admitted_names::<VkImageCreateInfoNext>(),
        sorted(vec![
            "VkExternalMemoryImageCreateInfo",
            "VkImageFormatListCreateInfo",
            "VkImageStencilUsageCreateInfo",
        ])
    );
    assert_eq!(
        admitted_names::<VkImageMemoryRequirementsInfo2Next>(),
        ["VkImagePlaneMemoryRequirementsInfo"]
    );
    assert_eq!(
        admitted_names::<VkMemoryRequirements2Next>(),
        ["VkMemoryDedicatedRequirements"]
    );
    // Chains the bring-up protocol had none for admit nothing.
    for (what, n) in [
        (
            "VkQueueFamilyProperties2",
            admitted_names::<VkQueueFamilyProperties2Next>().len(),
        ),
        (
            "VkPhysicalDeviceMemoryProperties2",
            admitted_names::<VkPhysicalDeviceMemoryProperties2Next>().len(),
        ),
        (
            "VkDeviceQueueCreateInfo",
            admitted_names::<VkDeviceQueueCreateInfoNext>().len(),
        ),
    ] {
        assert_eq!(n, 0, "{what}");
    }
    // And the decoder does accept more than that: the whitelist is the
    // protocol's.
    assert!(
        whitelist::<VkDeviceCreateInfoNext>().len() > 100,
        "the decoder admits the whole protocol's device chain"
    );
}

#[test]
fn the_advertised_extensions_are_still_only_the_implemented_ones() {
    // Every extension is decodable now; the host's list still reaches the
    // guest only through what this stage implements.
    assert!(info::EXTENSIONS.iter().all(|e| e.decodable));
    let named = |name: &[u8]| {
        let mut ext = VkExtensionProperties {
            spec_version: 1,
            ..Default::default()
        };
        for (slot, byte) in ext.extension_name.iter_mut().zip(name) {
            *slot = *byte;
        }
        ext
    };
    let host = [
        named(b"VK_KHR_swapchain"),
        named(b"VK_EXT_custom_border_color"),
        named(b"VK_KHR_timeline_semaphore"),
    ];
    assert!(super::policy::advertised_extensions(&host).is_empty());
}

#[test]
fn a_command_split_across_two_submissions_waits_for_its_second_half() {
    let (mut h, _) = standard();
    let mut bytes = set_reply(REPLY_RES, 0, 64);
    bytes.extend(version_bytes());
    let (front, back) = bytes.split_at(bytes.len() - 6);
    h.produce(front);
    h.wait_head(36);
    std::thread::sleep(std::time::Duration::from_millis(20));
    assert_eq!(h.head(), 36, "SetReply consumed; the half command waits");
    assert!(!h.fatal());
    assert_eq!(h.submit(back), Outcome::Consumed);
}

// ------------------------------------------------------- teardown, snapshot

#[test]
fn destroying_the_context_leaves_no_host_object_behind() {
    let (mut h, host) = standard();
    with_device(&mut h);
    h.call(&create_image(DEVICE, IMAGE, image_info())).unwrap();
    assert!(host.live_objects() >= 4);
    h.renderer.ctx_destroy(CTX);
    assert_eq!(host.live_objects(), 0);
    assert_eq!(h.renderer.factory().host_objects(), 0);
}

#[test]
fn a_device_reset_leaves_no_host_object_behind() {
    let (mut h, host) = standard();
    with_device(&mut h);
    h.call(&create_image(DEVICE, IMAGE, image_info())).unwrap();
    h.renderer.reset();
    assert_eq!(host.live_objects(), 0);
    assert_eq!(h.renderer.live_threads(), 0);
    assert_eq!(h.renderer.snapshot_refusal(), None);
}

#[test]
fn a_fatal_context_still_tears_down_cleanly() {
    let (mut h, host) = standard();
    with_device(&mut h);
    assert_fatal_on_command(&mut h, &properties(0x999));
    assert!(host.live_objects() > 0);
    h.renderer.ctx_destroy(CTX);
    assert_eq!(host.live_objects(), 0);
}

#[test]
fn a_snapshot_is_refused_by_name_while_host_vulkan_objects_are_live() {
    let (mut h, _) = standard();
    assert_eq!(h.renderer.snapshot_refusal(), None, "nothing to lose yet");
    boot(&mut h);
    let why = h.renderer.snapshot_refusal().expect("refused");
    assert!(why.contains("host Vulkan objects"), "{why}");

    // And through the whole device, which is what the machine asks.
    struct NoSink;
    impl crate::sink::ScanoutSink for NoSink {
        fn resolution(&self) -> (u32, u32) {
            (64, 64)
        }
        fn set_resolution(&self, _: u32, _: u32) -> Result<(), crate::sink::SinkError> {
            Ok(())
        }
        fn update_scanout(
            &self,
            _: u32,
            _: u32,
            _: u32,
            _: u32,
            _: &[u8],
        ) -> Result<(), crate::sink::SinkError> {
            Ok(())
        }
        fn set_cursor(
            &self,
            _: u32,
            _: u32,
            _: u32,
            _: u32,
            _: u32,
            _: u32,
            _: &[u8],
        ) -> Result<(), crate::sink::SinkError> {
            Ok(())
        }
        fn move_cursor(&self, _: u32, _: u32) -> Result<(), crate::sink::SinkError> {
            Ok(())
        }
        fn hide_cursor(&self) -> Result<(), crate::sink::SinkError> {
            Ok(())
        }
    }
    let Harness { renderer, .. } = h;
    let gpu = crate::GpuDevice::with_renderer(NoSink, Box::new(renderer));
    let why = virtio_core::VirtioDevice::snapshot_refusal(&gpu).expect("the device refuses");
    assert!(why.starts_with("virtio-gpu: "), "{why}");
}
