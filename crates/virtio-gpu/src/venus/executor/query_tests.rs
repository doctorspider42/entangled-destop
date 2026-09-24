//! Queries about values outside what the device serves are questions, not
//! attacks: they are answered "unsupported" or "nothing" and the context
//! lives, while creating something of such a value stays fatal. Found by
//! kmscube on Zink, whose format table probes
//! `VK_FORMAT_A1B5G5R5_UNORM_PACK16_KHR` (maintenance5, 1000470000).

use std::sync::Arc;

use crate::venus::protocol::*;

use super::fake::FakeVulkan;
use super::harness::*;

/// `VK_FORMAT_A1B5G5R5_UNORM_PACK16_KHR`.
const A1B5G5R5: i32 = 1_000_470_000;

fn harness() -> (Harness<FakeVulkan>, Arc<FakeVulkan>) {
    let host = Arc::new(FakeVulkan::standard());
    let mut h = Harness::new(Arc::clone(&host));
    with_device(&mut h);
    (h, host)
}

fn format_query(format: i32) -> Command<'static> {
    Command::GetPhysicalDeviceFormatProperties2(GetPhysicalDeviceFormatProperties2Args {
        physical_device: VkPhysicalDevice(PHYSICAL),
        format,
        p_format_properties: Some(VkFormatProperties2 {
            p_next: vec![
                VkFormatProperties2Next::VkFormatProperties3(VkFormatProperties3::default()),
                VkFormatProperties2Next::VkDrmFormatModifierPropertiesListEXT(
                    VkDrmFormatModifierPropertiesListEXT {
                        drm_format_modifier_count: 16,
                        p_drm_format_modifier_properties: Some(Vec::new()),
                    },
                ),
            ],
            ..Default::default()
        }),
    })
}

fn image_query(info: VkPhysicalDeviceImageFormatInfo2) -> Command<'static> {
    Command::GetPhysicalDeviceImageFormatProperties2(GetPhysicalDeviceImageFormatProperties2Args {
        physical_device: VkPhysicalDevice(PHYSICAL),
        p_image_format_info: Some(info),
        p_image_format_properties: Some(VkImageFormatProperties2::default()),
        ret: 0,
    })
}

fn image_info2(format: i32, usage: u32, flags: u32) -> VkPhysicalDeviceImageFormatInfo2 {
    VkPhysicalDeviceImageFormatInfo2 {
        p_next: Vec::new(),
        format,
        type_: 1,
        tiling: 0,
        usage,
        flags,
    }
}

#[test]
fn a_format_query_outside_core_answers_no_features_and_the_context_lives() {
    let (mut h, _) = harness();
    // Every format has features on the fake host, so zeros can only be ours.
    let Command::GetPhysicalDeviceFormatProperties2(core) = h.call(&format_query(RGBA8)).unwrap()
    else {
        panic!()
    };
    assert_ne!(
        core.p_format_properties
            .unwrap()
            .format_properties
            .optimal_tiling_features,
        0
    );
    let Command::GetPhysicalDeviceFormatProperties2(q) = h
        .call(&format_query(A1B5G5R5))
        .expect("answered, not fatal")
    else {
        panic!()
    };
    let props = q.p_format_properties.unwrap();
    assert_eq!(props.format_properties, VkFormatProperties::default());
    for link in &props.p_next {
        match link {
            VkFormatProperties2Next::VkFormatProperties3(p) => {
                assert_eq!(*p, VkFormatProperties3::default());
            }
            VkFormatProperties2Next::VkDrmFormatModifierPropertiesListEXT(l) => {
                assert_eq!(l.drm_format_modifier_count, 0);
                assert!(l
                    .p_drm_format_modifier_properties
                    .as_ref()
                    .is_none_or(Vec::is_empty));
            }
            _ => panic!("an unexpected link"),
        }
    }
    assert!(!h.fatal());
    // Still a device: a core query after it is answered by the host.
    h.call(&format_query(RGBA8)).expect("the context lives");
}

#[test]
fn an_image_format_query_outside_what_is_served_is_unsupported_not_fatal() {
    let (mut h, _) = harness();
    let answer = |h: &mut Harness<FakeVulkan>, info| {
        let Command::GetPhysicalDeviceImageFormatProperties2(q) =
            h.call(&image_query(info)).expect("answered, not fatal")
        else {
            panic!()
        };
        q.ret
    };
    assert_eq!(answer(&mut h, image_info2(RGBA8, 0x6, 0)), VK_SUCCESS);
    for (info, what) in [
        (image_info2(A1B5G5R5, 0x6, 0), "a maintenance5 format"),
        (
            image_info2(RGBA8, 0x6 | 0x8_0000, 0),
            "an extension's usage bit (attachment feedback loop)",
        ),
        (image_info2(RGBA8, 0x6, 0x4_0000), "an extension's flag bit"),
        (
            VkPhysicalDeviceImageFormatInfo2 {
                p_next: vec![
                    VkPhysicalDeviceImageFormatInfo2Next::VkPhysicalDeviceExternalImageFormatInfo(
                        VkPhysicalDeviceExternalImageFormatInfo { handle_type: 0x800 },
                    ),
                ],
                ..image_info2(RGBA8, 0x6, 0)
            },
            "an extension's handle type",
        ),
        (
            VkPhysicalDeviceImageFormatInfo2 {
                p_next: vec![
                    VkPhysicalDeviceImageFormatInfo2Next::VkImageFormatListCreateInfo(
                        VkImageFormatListCreateInfo {
                            view_format_count: 2,
                            p_view_formats: Some(vec![RGBA8, A1B5G5R5]),
                        },
                    ),
                ],
                ..image_info2(RGBA8, 0x6, 0x8)
            },
            "an extension's view format",
        ),
    ] {
        assert_eq!(
            answer(&mut h, info),
            VK_ERROR_FORMAT_NOT_SUPPORTED,
            "{what}"
        );
        assert!(!h.fatal(), "{what}");
    }
    // What no Vulkan defines, or what valid usage forbids outright, is
    // still fatal.
    for (info, what) in [
        (image_info2(RGBA8, 0, 0), "usage 0"),
        (
            VkPhysicalDeviceImageFormatInfo2 {
                type_: 7,
                ..image_info2(RGBA8, 0x6, 0)
            },
            "an image type nothing defines",
        ),
    ] {
        let (mut h, _) = harness();
        assert!(h.call(&image_query(info)).is_err(), "{what}");
        assert!(h.fatal(), "{what}");
    }
}

#[test]
fn creating_an_image_of_a_format_outside_core_is_still_fatal() {
    let (mut h, _) = harness();
    let info = VkImageCreateInfo {
        format: A1B5G5R5,
        ..image_info()
    };
    assert!(h.call(&create_image(DEVICE, IMAGE, info)).is_err());
    assert!(h.fatal());
}

#[test]
fn sparse_fence_semaphore_and_buffer_queries_about_unserved_values_answer_nothing() {
    let (mut h, _) = harness();
    let Command::GetPhysicalDeviceSparseImageFormatProperties2(q) = h
        .call(&Command::GetPhysicalDeviceSparseImageFormatProperties2(
            GetPhysicalDeviceSparseImageFormatProperties2Args {
                physical_device: VkPhysicalDevice(PHYSICAL),
                p_format_info: Some(VkPhysicalDeviceSparseImageFormatInfo2 {
                    format: A1B5G5R5,
                    type_: 1,
                    samples: 1,
                    usage: 0x6,
                    tiling: 0,
                }),
                p_property_count: Some(4),
                p_properties: Some(vec![Default::default(); 4]),
            },
        ))
        .expect("answered")
    else {
        panic!()
    };
    assert_eq!(q.p_property_count, Some(0), "no sparse residency is served");
    assert!(q.p_properties.is_none_or(|p| p.is_empty()));

    let Command::GetPhysicalDeviceExternalFenceProperties(f) = h
        .call(&Command::GetPhysicalDeviceExternalFenceProperties(
            GetPhysicalDeviceExternalFencePropertiesArgs {
                physical_device: VkPhysicalDevice(PHYSICAL),
                p_external_fence_info: Some(VkPhysicalDeviceExternalFenceInfo {
                    handle_type: 0x8, // SYNC_FD
                }),
                p_external_fence_properties: Some(Default::default()),
            },
        ))
        .expect("answered")
    else {
        panic!()
    };
    assert_eq!(
        f.p_external_fence_properties
            .unwrap()
            .external_fence_features,
        0
    );

    let Command::GetPhysicalDeviceExternalSemaphoreProperties(s) = h
        .call(&Command::GetPhysicalDeviceExternalSemaphoreProperties(
            GetPhysicalDeviceExternalSemaphorePropertiesArgs {
                physical_device: VkPhysicalDevice(PHYSICAL),
                p_external_semaphore_info: Some(VkPhysicalDeviceExternalSemaphoreInfo {
                    p_next: Vec::new(),
                    handle_type: 0x80, // ZIRCON_EVENT_BIT_FUCHSIA
                }),
                p_external_semaphore_properties: Some(Default::default()),
            },
        ))
        .expect("answered")
    else {
        panic!()
    };
    assert_eq!(
        s.p_external_semaphore_properties.unwrap(),
        VkExternalSemaphoreProperties::default()
    );

    for (usage, handle, what) in [
        (0x3, 0x800, "an extension's memory handle type"),
        (0x3 | 0x100_0000, 0x1, "an extension's usage bit"),
    ] {
        let Command::GetPhysicalDeviceExternalBufferProperties(b) = h
            .call(&Command::GetPhysicalDeviceExternalBufferProperties(
                GetPhysicalDeviceExternalBufferPropertiesArgs {
                    physical_device: VkPhysicalDevice(PHYSICAL),
                    p_external_buffer_info: Some(VkPhysicalDeviceExternalBufferInfo {
                        p_next: Vec::new(),
                        flags: 0,
                        usage,
                        handle_type: handle,
                    }),
                    p_external_buffer_properties: Some(Default::default()),
                },
            ))
            .expect(what)
        else {
            panic!()
        };
        assert_eq!(
            b.p_external_buffer_properties
                .unwrap()
                .external_memory_properties
                .external_memory_features,
            0,
            "{what}"
        );
    }
    assert!(!h.fatal());
}

// ------------------------------------------------ long pNext chains
//
// A guest shown 70-odd extensions asks `vkGetPhysicalDeviceFeatures2` about
// dozens of structures in one chain; at 33 links the old depth cap killed
// the context (found by GNOME on the GPU, zink under it).

/// Every link `N` admits, found by asking the generated decoder about every
/// structure type the registry can number — core ones below 1000, every
/// extension's `1_000_000_000 + (extension - 1) * 1000 + offset` — with a
/// zeroed skeleton body, which is what an output link is on the wire.
fn admitted<N: for<'a> ChainLink<'a>>() -> Vec<N> {
    let zeros = [0u8; 4096];
    let core = 0..1000i32;
    let extensions =
        (0..1000i32).flat_map(|ext| (0..1000i32).map(move |at| 1_000_000_000 + ext * 1000 + at));
    let mut links = Vec::new();
    for stype in core.chain(extensions) {
        let mut dec = crate::venus::wire::Decoder::new(&zeros);
        if let Some(link) = N::decode_body(stype, &mut dec, true) {
            links.push(link.unwrap_or_else(|e| panic!("sType {stype}: {e}")));
        }
    }
    links
}

/// The links of [`admitted`] the executor serves ([`super::policy::admits_link`]):
/// what the capset's extension mask lets a guest chain at all, and so the
/// longest chain Mesa can send.
fn served<N: for<'a> ChainLink<'a>>() -> Vec<N> {
    let mut links = admitted::<N>();
    links.retain(|l| super::policy::admits_link(l.structure_type()));
    links
}

fn features_query(p_next: Vec<VkPhysicalDeviceFeatures2Next>) -> Command<'static> {
    Command::GetPhysicalDeviceFeatures2(GetPhysicalDeviceFeatures2Args {
        physical_device: VkPhysicalDevice(PHYSICAL),
        p_features: Some(VkPhysicalDeviceFeatures2 {
            p_next,
            ..Default::default()
        }),
    })
}

fn properties_query(p_next: Vec<VkPhysicalDeviceProperties2Next>) -> Command<'static> {
    Command::GetPhysicalDeviceProperties2(GetPhysicalDeviceProperties2Args {
        physical_device: VkPhysicalDevice(PHYSICAL),
        p_properties: Some(VkPhysicalDeviceProperties2 {
            p_next,
            ..Default::default()
        }),
    })
}

#[test]
fn the_depth_cap_is_at_least_twice_the_longest_chain_any_parent_admits() {
    // The decoder runs before the policy, so what bounds its recursion is
    // everything the protocol admits (117, 51 and 120 today)...
    let features = admitted::<VkPhysicalDeviceFeatures2Next>().len();
    let properties = admitted::<VkPhysicalDeviceProperties2Next>().len();
    let device = admitted::<VkDeviceCreateInfoNext>().len();
    let longest = features.max(properties).max(device);
    assert!(
        2 * longest <= crate::venus::wire::MAX_PNEXT_DEPTH as usize,
        "a regenerated protocol admits {longest} links: raise MAX_PNEXT_DEPTH"
    );
    // ...and what a guest really sends is what the executor serves (44, 27
    // and 47 today), which is past the 32 links the cap used to be.
    let chained = served::<VkPhysicalDeviceFeatures2Next>().len();
    assert!(chained > 32, "{chained}");
    assert!(served::<VkDeviceCreateInfoNext>().len() > 32);
}

#[test]
fn a_features_query_chaining_every_admitted_structure_is_answered_in_order() {
    let (mut h, _) = harness();
    let links = served::<VkPhysicalDeviceFeatures2Next>();
    let asked: Vec<i32> = links.iter().map(ChainLink::structure_type).collect();
    let Command::GetPhysicalDeviceFeatures2(f) =
        h.call(&features_query(links)).expect("answered, not fatal")
    else {
        panic!()
    };
    let answer = f.p_features.unwrap().p_next;
    let got: Vec<i32> = answer.iter().map(ChainLink::structure_type).collect();
    assert_eq!(got, asked, "the reply echoes the guest's chain");
    // The structures the host knows are answered from it, wherever in the
    // chain they sit.
    assert!(answer.iter().any(|l| matches!(l,
        VkPhysicalDeviceFeatures2Next::VkPhysicalDeviceVulkan13Features(v)
            if v.synchronization2 == 1 && v.dynamic_rendering == 1)));
    assert!(!h.fatal());
    h.call(&features_query(Vec::new()))
        .expect("the context lives");
}

#[test]
fn a_properties_query_chaining_every_admitted_structure_is_answered_in_order() {
    let (mut h, _) = harness();
    let links = served::<VkPhysicalDeviceProperties2Next>();
    let asked: Vec<i32> = links.iter().map(ChainLink::structure_type).collect();
    let Command::GetPhysicalDeviceProperties2(all) = h
        .call(&properties_query(links.clone()))
        .expect("answered, not fatal")
    else {
        panic!()
    };
    let all = all.p_properties.unwrap().p_next;
    let got: Vec<i32> = all.iter().map(ChainLink::structure_type).collect();
    assert_eq!(got, asked, "the reply echoes the guest's chain");
    // Each link answers exactly what it answers on its own.
    for (link, answered) in links.into_iter().zip(&all).step_by(7) {
        let Command::GetPhysicalDeviceProperties2(one) =
            h.call(&properties_query(vec![link])).expect("answered")
        else {
            panic!()
        };
        assert_eq!(&one.p_properties.unwrap().p_next[0], answered);
    }
    assert!(!h.fatal());
}

#[test]
fn a_chain_past_the_depth_cap_and_a_duplicate_link_are_still_fatal() {
    use crate::venus::wire::{
        Decoder, Encoder, WireError, COMMAND_GENERATE_REPLY, MAX_PNEXT_DEPTH,
    };
    let decode = |command: &Command<'_>| {
        let mut enc = Encoder::new();
        command
            .encode_command(&mut enc, COMMAND_GENERATE_REPLY)
            .expect("encode");
        let bytes = enc.finish().expect("encode");
        Command::decode_next(&mut Decoder::new(&bytes)).map(|_| ())
    };
    let link =
        || VkPhysicalDeviceFeatures2Next::VkPhysicalDeviceVulkan13Features(Default::default());
    // What the executor refuses, it refuses as the decoder does: too deep
    // on the way down, before any duplicate can be seen...
    let deep = features_query((0..=MAX_PNEXT_DEPTH).map(|_| link()).collect());
    assert!(matches!(
        decode(&deep),
        Err(ProtocolError::Wire(WireError::PnextChainTooDeep {
            parent: "VkPhysicalDeviceFeatures2"
        }))
    ));
    // ...and a link named twice, however short the chain.
    let twice = features_query(vec![link(), link()]);
    assert!(matches!(
        decode(&twice),
        Err(ProtocolError::DuplicatePnextStype {
            parent: "VkPhysicalDeviceFeatures2",
            ..
        })
    ));
    // Exactly at the cap, a chain is only refused for its duplicates.
    let at_cap = features_query((0..MAX_PNEXT_DEPTH).map(|_| link()).collect());
    assert!(matches!(
        decode(&at_cap),
        Err(ProtocolError::DuplicatePnextStype { .. })
    ));

    for command in [deep, twice] {
        let (mut h, _) = harness();
        let Err(head) = h.call(&command) else {
            panic!("not refused")
        };
        assert!(h.fatal());
        assert_ne!(head, h.tail(), "head passed the refused command");
        assert_eq!(h.renderer.factory().context_fatal(CTX), Some(true));
    }
}

#[test]
fn a_device_created_with_every_served_structure_chained_is_created() {
    // Zink creates its device with the chain it queried: dozens of feature
    // structures (all zero here, so all a subset of what was reported). The
    // device-group link needs real members, so it is left to its own tests.
    let host = Arc::new(FakeVulkan::standard());
    let mut h = Harness::new(Arc::clone(&host));
    boot(&mut h);
    let mut chain = served::<VkDeviceCreateInfoNext>();
    chain.retain(|l| !matches!(l, VkDeviceCreateInfoNext::VkDeviceGroupDeviceCreateInfo(_)));
    assert!(chain.len() > 32, "{}", chain.len());
    let Command::CreateDevice(reply) = h
        .call(&create_device(PHYSICAL, DEVICE, chain))
        .expect("answered, not fatal")
    else {
        panic!()
    };
    assert_eq!(reply.ret, VK_SUCCESS);
    assert!(!h.fatal());
}
