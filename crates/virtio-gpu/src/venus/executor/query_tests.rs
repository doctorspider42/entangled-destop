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
