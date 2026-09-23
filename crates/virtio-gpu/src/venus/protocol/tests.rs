//! Hand-written tests for the generated protocol.
//!
//! The expected bytes below are written out by hand from the spec's worked
//! examples and from the real guest capture, never produced by the encoder
//! under test: a decoder and an encoder generated from one model agree with
//! each other whatever the model says, so only independent bytes can catch
//! the model being wrong. `tools/venus-protocol/harness` checks the rest
//! against Mesa's own generated C.

use super::*;
use crate::venus::capset::{vk_make_api_version, ExtensionMask};
use crate::venus::transport::Opcode;
use crate::venus::wire::{Decoder, Encoder, WireError, COMMAND_GENERATE_REPLY};

// ---- byte builders ------------------------------------------------------------

/// Little-endian bytes, appended by hand.
#[derive(Default)]
struct Bytes(Vec<u8>);

impl Bytes {
    fn u32(mut self, v: u32) -> Self {
        self.0.extend_from_slice(&v.to_le_bytes());
        self
    }
    fn i32(self, v: i32) -> Self {
        self.u32(v as u32)
    }
    fn u64(mut self, v: u64) -> Self {
        self.0.extend_from_slice(&v.to_le_bytes());
        self
    }
    fn done(self) -> Vec<u8> {
        self.0
    }
}

fn decode_one(bytes: &[u8]) -> Result<(CommandHeader, Command<'_>), ProtocolError> {
    let mut dec = Decoder::new(bytes);
    let out = Command::decode_next(&mut dec)?;
    assert_eq!(
        dec.remaining(),
        0,
        "the command must consume exactly its bytes"
    );
    Ok(out)
}

use crate::venus::wire::CommandHeader;

fn reply_of(cmd: &Command<'_>) -> Vec<u8> {
    cmd.reply_bytes(1 << 20).expect("reply encodes")
}

const STYPE_PROPERTIES_2: i32 = 0x3b9b_b079;
const STYPE_ID_PROPERTIES: i32 = 0x3b9b_df5c;

// ---- the real guest bytes (spec §0 item 7) --------------------------------------

#[test]
fn the_captured_enumerate_instance_version_decodes_and_is_answered_byte_for_byte() {
    let command = [0x89, 0, 0, 0, 0x01, 0, 0, 0, 0x01, 0, 0, 0, 0, 0, 0, 0];
    let (header, cmd) = decode_one(&command).expect("the guest's first ring command decodes");
    assert_eq!(header.opcode, 137);
    assert!(header.wants_reply());
    let Command::EnumerateInstanceVersion(mut args) = cmd else {
        panic!("opcode 137 is vkEnumerateInstanceVersion");
    };
    assert_eq!(
        args.p_api_version,
        Some(0),
        "an output pointer arrives present, unfilled"
    );

    let api = vk_make_api_version(0, 1, 3, 0);
    args.p_api_version = Some(api);
    args.ret = VK_SUCCESS;
    let reply = reply_of(&Command::EnumerateInstanceVersion(args));
    let expected = Bytes::default().u32(0x89).i32(0).u64(1).u32(api).done();
    assert_eq!(reply, expected);
    assert_eq!(reply.len(), 20, "Mesa sized this reply window at 20 bytes");
    assert_eq!(
        &reply[..16],
        &[0x89, 0, 0, 0, 0, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0]
    );
}

// ---- spec §2.1: vkEnumeratePhysicalDevices ----------------------------------

#[test]
fn enumerate_physical_devices_count_query_matches_the_worked_example() {
    let command = Bytes::default()
        .u32(2) // opcode
        .u32(COMMAND_GENERATE_REPLY)
        .u64(1) // instance id 1
        .u64(1) // pPhysicalDeviceCount present
        .u32(0) // *pPhysicalDeviceCount = 0
        .u64(0) // pPhysicalDevices: array_size 0 = NULL
        .done();
    assert_eq!(command.len(), 36);
    let (_, cmd) = decode_one(&command).expect("decodes");
    let Command::EnumeratePhysicalDevices(mut args) = cmd else {
        panic!("opcode 2");
    };
    assert_eq!(args.instance, VkInstance(1));
    assert_eq!(args.p_physical_device_count, Some(0));
    assert_eq!(args.p_physical_devices, None);

    args.p_physical_device_count = Some(1);
    args.ret = VK_SUCCESS;
    let reply = reply_of(&Command::EnumeratePhysicalDevices(args));
    let expected = Bytes::default().u32(2).i32(0).u64(1).u32(1).u64(0).done();
    assert_eq!(reply, expected);
    assert_eq!(reply.len(), 28);
}

#[test]
fn enumerate_physical_devices_handle_query_echoes_the_guest_ids() {
    let command = Bytes::default()
        .u32(2)
        .u32(COMMAND_GENERATE_REPLY)
        .u64(1) // instance id 1
        .u64(1)
        .u32(1) // count present, count = 1
        .u64(1) // array_size 1 (must == count)
        .u64(2) // physical device id 2, guest-chosen
        .done();
    assert_eq!(command.len(), 44);
    let (_, cmd) = decode_one(&command).expect("decodes");
    let Command::EnumeratePhysicalDevices(mut args) = cmd else {
        panic!("opcode 2");
    };
    assert_eq!(args.p_physical_devices, Some(vec![VkPhysicalDevice(2)]));

    args.ret = VK_SUCCESS;
    let reply = reply_of(&Command::EnumeratePhysicalDevices(args));
    let expected = Bytes::default()
        .u32(2)
        .i32(0)
        .u64(1)
        .u32(1)
        .u64(1)
        .u64(2)
        .done();
    assert_eq!(reply, expected);
    assert_eq!(reply.len(), 36);
}

// ---- spec §2.2: vkGetPhysicalDeviceProperties2 with a chained struct ---------

fn properties2_command() -> Vec<u8> {
    Bytes::default()
        .u32(148)
        .u32(COMMAND_GENERATE_REPLY)
        .u64(2) // physicalDevice id 2
        .u64(1) // pProperties present
        .i32(STYPE_PROPERTIES_2)
        .u64(1) // pNext present
        .i32(STYPE_ID_PROPERTIES)
        .u64(0) // ID.pNext = NULL
        .done()
}

#[test]
fn get_physical_device_properties2_decodes_the_partial_skeleton() {
    let command = properties2_command();
    assert_eq!(command.len(), 48);
    let (_, cmd) = decode_one(&command).expect("decodes");
    let Command::GetPhysicalDeviceProperties2(args) = cmd else {
        panic!("opcode 148");
    };
    assert_eq!(args.physical_device, VkPhysicalDevice(2));
    let props = args.p_properties.expect("present");
    assert_eq!(props.p_next.len(), 1);
    assert!(matches!(
        props.p_next[0],
        VkPhysicalDeviceProperties2Next::VkPhysicalDeviceIDProperties(_)
    ));
    assert_eq!(props.properties, VkPhysicalDeviceProperties::default());
}

#[test]
fn get_physical_device_properties2_reply_has_the_worked_example_layout() {
    let command = properties2_command();
    let (_, cmd) = decode_one(&command).expect("decodes");
    let Command::GetPhysicalDeviceProperties2(mut args) = cmd else {
        panic!("opcode 148");
    };
    let props = args.p_properties.as_mut().expect("present");
    let Some(VkPhysicalDeviceProperties2Next::VkPhysicalDeviceIDProperties(id)) =
        props.p_next.first_mut()
    else {
        panic!("the chain carries the ID properties");
    };
    id.device_uuid = [0xd1; 16];
    id.driver_uuid = [0xd2; 16];
    id.device_luid = [0xd3; 8];
    id.device_node_mask = 0x0102_0304;
    id.device_luid_valid = 1;
    let p = &mut props.properties;
    p.api_version = vk_make_api_version(0, 1, 3, 0);
    p.driver_version = 0x1111;
    p.vendor_id = 0x10de;
    p.device_id = 0x2222;
    p.device_type = 2;
    p.device_name[..5].copy_from_slice(b"Venus");
    p.pipeline_cache_uuid = [0xcc; 16];
    p.limits.max_image_dimension1d = 0xa1a1_a1a1;
    p.sparse_properties.residency_standard2d_block_shape = 0x5eed;
    let reply = reply_of(&Command::GetPhysicalDeviceProperties2(args.clone()));

    assert_eq!(reply.len(), 976, "Limits is 540 bytes, the whole reply 976");
    let at = |off: usize, n: usize| &reply[off..off + n];
    let u32_at = |off: usize| u32::from_le_bytes(at(off, 4).try_into().unwrap());
    let u64_at = |off: usize| u64::from_le_bytes(at(off, 8).try_into().unwrap());
    assert_eq!(
        u32_at(0x000),
        148,
        "opcode, and no VkResult: the command is void"
    );
    assert_eq!(u64_at(0x004), 1, "pProperties present");
    assert_eq!(u32_at(0x00c) as i32, STYPE_PROPERTIES_2);
    assert_eq!(u64_at(0x010), 1, "chain: present");
    assert_eq!(u32_at(0x018) as i32, STYPE_ID_PROPERTIES);
    assert_eq!(u64_at(0x01c), 0, "ID.pNext end");
    assert_eq!(u64_at(0x024), 16);
    assert_eq!(at(0x02c, 16), &[0xd1; 16]);
    assert_eq!(u64_at(0x03c), 16);
    assert_eq!(at(0x044, 16), &[0xd2; 16]);
    assert_eq!(u64_at(0x054), 8);
    assert_eq!(at(0x05c, 8), &[0xd3; 8]);
    assert_eq!(u32_at(0x064), 0x0102_0304);
    assert_eq!(u32_at(0x068), 1);
    assert_eq!(u32_at(0x06c), vk_make_api_version(0, 1, 3, 0));
    assert_eq!(u32_at(0x070), 0x1111);
    assert_eq!(u32_at(0x074), 0x10de);
    assert_eq!(u32_at(0x078), 0x2222);
    assert_eq!(u32_at(0x07c), 2);
    assert_eq!(at(0x080, 8), &[0, 1, 0, 0, 0, 0, 0, 0], "array_size 256");
    assert_eq!(at(0x088, 5), b"Venus");
    assert_eq!(u64_at(0x188), 16);
    assert_eq!(at(0x190, 16), &[0xcc; 16]);
    assert_eq!(u32_at(0x1a0), 0xa1a1_a1a1, "Limits starts at 0x1a0");
    assert_eq!(u32_at(0x3bc), 0x5eed, "SparseProperties starts at 0x3bc");

    // And the guest's side of it reads back what was written.
    let mut guest = GetPhysicalDeviceProperties2Args::default();
    let mut dec = Decoder::new(&reply);
    guest
        .decode_reply(&mut dec)
        .expect("the reply decodes as the guest reads it");
    assert_eq!(dec.remaining(), 0);
    assert_eq!(guest.p_properties, args.p_properties);
}

// ---- round trips of commands the tests build ------------------------------------

fn create_instance_bytes() -> Vec<u8> {
    let info = VkInstanceCreateInfo {
        p_application_info: Some(VkApplicationInfo {
            p_application_name: Some(b"vulkaninfo"),
            application_version: 1,
            p_engine_name: None,
            engine_version: 0,
            api_version: vk_make_api_version(0, 1, 3, 0),
        }),
        enabled_layer_count: 0,
        pp_enabled_layer_names: None,
        enabled_extension_count: 2,
        pp_enabled_extension_names: Some(vec![b"VK_KHR_surface", b"VK_EXT_debug_utils"]),
        ..Default::default()
    };
    let args = CreateInstanceArgs {
        p_create_info: Some(info),
        p_instance: Some(VkInstance(7)),
        ret: 0,
    };
    let mut enc = Encoder::new();
    args.encode_command(&mut enc, COMMAND_GENERATE_REPLY)
        .expect("encodes");
    enc.finish().expect("finishes")
}

fn create_device_bytes() -> Vec<u8> {
    let info = VkDeviceCreateInfo {
        p_next: vec![
            VkDeviceCreateInfoNext::VkPhysicalDeviceVulkan12Features(
                VkPhysicalDeviceVulkan12Features {
                    timeline_semaphore: 1,
                    ..Default::default()
                },
            ),
            VkDeviceCreateInfoNext::VkDeviceGroupDeviceCreateInfo(VkDeviceGroupDeviceCreateInfo {
                physical_device_count: 1,
                p_physical_devices: Some(vec![VkPhysicalDevice(2)]),
            }),
        ],
        queue_create_info_count: 1,
        p_queue_create_infos: Some(vec![VkDeviceQueueCreateInfo {
            queue_family_index: 0,
            queue_count: 2,
            p_queue_priorities: Some(vec![1.0, 0.5]),
            ..Default::default()
        }]),
        enabled_extension_count: 1,
        pp_enabled_extension_names: Some(vec![b"VK_KHR_swapchain"]),
        p_enabled_features: None,
        ..Default::default()
    };
    let args = CreateDeviceArgs {
        physical_device: VkPhysicalDevice(2),
        p_create_info: Some(info),
        p_device: Some(VkDevice(9)),
        ret: 0,
    };
    let mut enc = Encoder::new();
    args.encode_command(&mut enc, COMMAND_GENERATE_REPLY)
        .expect("encodes");
    enc.finish().expect("finishes")
}

fn reencode(cmd: &Command<'_>, flags: u32) -> Vec<u8> {
    let mut enc = Encoder::new();
    cmd.encode_command(&mut enc, flags).expect("re-encodes");
    enc.finish().expect("finishes")
}

#[test]
fn commands_with_strings_arrays_and_chains_round_trip() {
    for bytes in [
        create_instance_bytes(),
        create_device_bytes(),
        properties2_command(),
    ] {
        let (header, cmd) = decode_one(&bytes).expect("decodes");
        assert_eq!(reencode(&cmd, header.flags), bytes, "{}", cmd.name());
    }
    let bytes = create_instance_bytes();
    let (_, cmd) = decode_one(&bytes).expect("decodes");
    let Command::CreateInstance(args) = cmd else {
        panic!("opcode 0");
    };
    let info = args.p_create_info.expect("present");
    assert_eq!(
        info.pp_enabled_extension_names,
        Some(vec![&b"VK_KHR_surface"[..], &b"VK_EXT_debug_utils"[..]])
    );
    let app = info.p_application_info.expect("present");
    assert_eq!(app.p_application_name, Some(&b"vulkaninfo"[..]));
    assert_eq!(app.p_engine_name, None);
}

#[test]
fn a_zero_width_output_array_is_sized_by_its_count_not_by_the_stream() {
    // 200 VkExtensionProperties slots cost the guest nothing but the array
    // size: a partial VkExtensionProperties has no bytes at all. A bound of
    // four bytes per element would refuse this legitimate command.
    let command = Bytes::default()
        .u32(14)
        .u32(COMMAND_GENERATE_REPLY)
        .u64(2) // physical device
        .u64(0) // pLayerName = NULL
        .u64(1)
        .u32(200) // *pPropertyCount = 200
        .u64(200) // pProperties: array_size 200, no element bytes
        .done();
    let (_, cmd) = decode_one(&command).expect("decodes");
    let Command::EnumerateDeviceExtensionProperties(mut args) = cmd else {
        panic!("opcode 14");
    };
    assert_eq!(args.p_property_count, Some(200));
    assert_eq!(
        args.p_properties,
        Some(Vec::new()),
        "not allocated at decode"
    );
    assert_eq!(
        reencode(
            &Command::EnumerateDeviceExtensionProperties(args.clone()),
            1
        ),
        command
    );

    // The executor fills two and says so; the reply carries exactly those.
    let mut ext = VkExtensionProperties::default();
    ext.extension_name[..16].copy_from_slice(b"VK_KHR_swapchain");
    ext.spec_version = 70;
    args.p_property_count = Some(2);
    args.p_properties = Some(vec![ext.clone(), ext]);
    let reply = reply_of(&Command::EnumerateDeviceExtensionProperties(args));
    assert_eq!(
        reply.len(),
        4 + 4 + 12 + 8 + 2 * (8 + 256 + 4),
        "28 + 268 per element"
    );
}

// ---- malformed input: refused, never a panic --------------------------------------

#[test]
fn every_truncation_of_a_command_is_refused() {
    for bytes in [
        create_instance_bytes(),
        create_device_bytes(),
        properties2_command(),
        [0x89, 0, 0, 0, 1, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0].to_vec(),
    ] {
        for cut in 0..bytes.len() {
            let mut dec = Decoder::new(&bytes[..cut]);
            let result = Command::decode_next(&mut dec);
            assert!(
                result.is_err(),
                "a {cut}-byte prefix of {} bytes decoded",
                bytes.len()
            );
            assert!(dec.is_fatal(), "a refusal must poison the decoder");
        }
    }
}

#[test]
fn random_corruption_never_panics() {
    let seeds = [
        create_instance_bytes(),
        create_device_bytes(),
        properties2_command(),
    ];
    let mut state = 0x2545_f491_4f6c_dd1du64;
    let mut next = move || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state
    };
    for seed in &seeds {
        for _ in 0..3000 {
            let mut bytes = seed.clone();
            for _ in 0..(next() % 4 + 1) {
                let i = (next() as usize) % bytes.len();
                bytes[i] = next() as u8;
            }
            let mut dec = Decoder::new(&bytes);
            if let Ok((_, cmd)) = Command::decode_next(&mut dec) {
                // Whatever decoded must re-encode without a panic too.
                let mut enc = Encoder::new();
                let _ = cmd.encode_command(&mut enc, 1);
                let _ = cmd.reply_bytes(1 << 20);
            }
        }
    }
}

fn refusal(bytes: &[u8]) -> ProtocolError {
    let mut dec = Decoder::new(bytes);
    let err = Command::decode_next(&mut dec).expect_err("must be refused");
    assert!(dec.is_fatal());
    err
}

#[test]
fn oversized_counts_are_refused_before_anything_is_allocated() {
    for size in [u64::from(u32::MAX), u64::MAX] {
        let bytes = Bytes::default()
            .u32(2)
            .u32(1)
            .u64(1)
            .u64(1)
            .u32(u32::MAX)
            .u64(size)
            .u64(2)
            .done();
        let err = refusal(&bytes);
        assert!(
            matches!(
                err,
                ProtocolError::Wire(
                    WireError::ArrayLongerThanStream { .. } | WireError::ArrayLengthMismatch { .. }
                )
            ),
            "{err:?}"
        );
    }
}

#[test]
fn an_array_size_that_contradicts_its_count_is_refused() {
    // count 1, array_size 2
    let bytes = Bytes::default()
        .u32(2)
        .u32(1)
        .u64(1)
        .u64(1)
        .u32(1)
        .u64(2)
        .u64(2)
        .u64(3)
        .done();
    assert!(matches!(
        refusal(&bytes),
        ProtocolError::Wire(WireError::ArrayLengthMismatch {
            expected: 1,
            found: 2
        })
    ));

    // A *required* array sent null while its count says two: the null branch
    // is cross-checked (VkInstanceCreateInfo::ppEnabledExtensionNames).
    let bytes = Bytes::default()
        .u32(0)
        .u32(1)
        .u64(1) // pCreateInfo present
        .i32(VK_STRUCTURE_TYPE_INSTANCE_CREATE_INFO)
        .u64(0) // pNext
        .u32(0) // flags
        .u64(0) // pApplicationInfo NULL
        .u32(0) // enabledLayerCount
        .u64(0) // ppEnabledLayerNames NULL
        .u32(2) // enabledExtensionCount = 2
        .u64(0) // ... but NULL
        .u64(0) // pAllocator
        .u64(1)
        .u64(7) // pInstance
        .done();
    assert!(matches!(
        refusal(&bytes),
        ProtocolError::Wire(WireError::ArrayLengthMismatch {
            expected: 2,
            found: 0
        })
    ));
}

#[test]
fn an_unknown_or_unadmitted_stype_in_a_chain_is_refused() {
    // A made-up sType, and a real one this selection does not admit (a 1.4
    // structure, past `[api] 1.3`).
    for stype in [
        0x7fff_0001,
        VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_VULKAN_1_4_PROPERTIES_RAW,
    ] {
        let bytes = Bytes::default()
            .u32(148)
            .u32(1)
            .u64(2)
            .u64(1)
            .i32(STYPE_PROPERTIES_2)
            .u64(1)
            .i32(stype)
            .u64(0)
            .done();
        assert!(
            matches!(
                refusal(&bytes),
                ProtocolError::Wire(WireError::UnknownPnextStype {
                    parent: "VkPhysicalDeviceProperties2",
                    ..
                })
            ),
            "sType {stype}"
        );
    }
}

/// `VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_VULKAN_1_4_PROPERTIES`, spelled out
/// because the generator (rightly) did not emit a constant for it.
const VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_VULKAN_1_4_PROPERTIES_RAW: i32 = 56;

#[test]
fn structural_refusals_each_have_their_own_error() {
    // Wrong sType for the struct itself.
    let mut bytes = properties2_command();
    bytes[0x18] ^= 1;
    assert!(matches!(
        refusal(&bytes),
        ProtocolError::WrongStructureType { .. }
    ));

    // A duplicated chain link.
    let bytes = Bytes::default()
        .u32(148)
        .u32(1)
        .u64(2)
        .u64(1)
        .i32(STYPE_PROPERTIES_2)
        .u64(1)
        .i32(STYPE_ID_PROPERTIES)
        .u64(1)
        .i32(STYPE_ID_PROPERTIES)
        .u64(0)
        .done();
    assert!(matches!(
        refusal(&bytes),
        ProtocolError::DuplicatePnextStype {
            stype: STYPE_ID_PROPERTIES,
            ..
        }
    ));

    // A chain deeper than the host will walk.
    let mut deep = Bytes::default()
        .u32(148)
        .u32(1)
        .u64(2)
        .u64(1)
        .i32(STYPE_PROPERTIES_2);
    for _ in 0..40 {
        deep = deep.u64(1).i32(STYPE_ID_PROPERTIES);
    }
    assert!(matches!(
        refusal(&deep.u64(0).done()),
        ProtocolError::Wire(WireError::PnextChainTooDeep { .. })
    ));

    // A required pointer sent null.
    let bytes = Bytes::default().u32(137).u32(1).u64(0).done();
    assert!(matches!(
        refusal(&bytes),
        ProtocolError::NullPointer {
            owner: "vkEnumerateInstanceVersion",
            field: "pApiVersion"
        }
    ));

    // A null dispatchable handle.
    let bytes = Bytes::default()
        .u32(2)
        .u32(1)
        .u64(0)
        .u64(1)
        .u32(0)
        .u64(0)
        .done();
    assert!(matches!(
        refusal(&bytes),
        ProtocolError::NullDispatchHandle {
            command: "vkEnumeratePhysicalDevices"
        }
    ));

    // A non-null pAllocator (vkDestroyInstance: instance, pAllocator).
    let bytes = Bytes::default().u32(1).u32(0).u64(1).u64(1).done();
    assert!(matches!(
        refusal(&bytes),
        ProtocolError::Wire(WireError::AllocatorNotNull(1))
    ));

    // Opcodes: not Venus at all; Venus but not generated; unknown flags.
    assert!(matches!(
        refusal(&Bytes::default().u32(9999).u32(0).done()),
        ProtocolError::UnknownOpcode { opcode: 9999 }
    ));
    assert!(matches!(
        refusal(&Bytes::default().u32(18).u32(0).done()),
        ProtocolError::NotGenerated {
            opcode: 18,
            command: "vkQueueSubmit"
        }
    ));
    assert!(matches!(
        refusal(&Bytes::default().u32(137).u32(2).u64(1).done()),
        ProtocolError::UnknownCommandFlags { unknown: 2, .. }
    ));
}

// ---- encode-side refusals -----------------------------------------------------------

#[test]
fn a_reply_array_that_disagrees_with_its_count_is_refused_whole() {
    let args = EnumeratePhysicalDevicesArgs {
        instance: VkInstance(1),
        p_physical_device_count: Some(2),
        p_physical_devices: Some(vec![VkPhysicalDevice(2)]),
        ret: VK_SUCCESS,
    };
    assert!(matches!(
        Command::EnumeratePhysicalDevices(args).reply_bytes(1 << 20),
        Err(ProtocolError::ArrayCountMismatch {
            count: 2,
            len: 1,
            ..
        })
    ));
}

#[test]
fn a_reply_that_does_not_fit_its_window_is_refused() {
    let args = EnumerateInstanceVersionArgs {
        p_api_version: Some(1),
        ret: VK_SUCCESS,
    };
    assert!(matches!(
        Command::EnumerateInstanceVersion(args).reply_bytes(19),
        Err(ProtocolError::Wire(WireError::ReplyTooLong { .. }))
    ));
}

// ---- the tables ---------------------------------------------------------------------

#[test]
fn generated_opcodes_agree_with_the_hand_written_transport() {
    assert_eq!(
        VK_COMMAND_TYPE_SET_REPLY_COMMAND_STREAM_MESA_EXT,
        Opcode::SetReplyCommandStream.as_u32()
    );
    assert_eq!(
        VK_COMMAND_TYPE_EXECUTE_COMMAND_STREAMS_MESA_EXT,
        Opcode::ExecuteCommandStreams.as_u32()
    );
    assert_eq!(
        VK_COMMAND_TYPE_CREATE_RING_MESA_EXT,
        Opcode::CreateRing.as_u32()
    );
    assert_eq!(
        VK_COMMAND_TYPE_NOTIFY_RING_MESA_EXT,
        Opcode::NotifyRing.as_u32()
    );
    assert_eq!(
        VK_COMMAND_TYPE_WAIT_RING_SEQNO_MESA_EXT,
        Opcode::WaitRingSeqno.as_u32()
    );
    assert_eq!(command_type_name(137), Some("vkEnumerateInstanceVersion"));
    assert_eq!(command_type_name(178), Some("vkSetReplyCommandStreamMESA"));
    // The milestone set, as spec §4 lists it.
    let mut ops: Vec<u32> = GENERATED_COMMANDS.iter().map(|(op, _)| *op).collect();
    ops.sort_unstable();
    assert_eq!(
        ops,
        [
            0, 1, 2, 6, 11, 12, 14, 54, 55, 85, 86, 137, 143, 144, 147, 148, 149, 150, 151, 152,
            155
        ]
    );
}

#[test]
fn the_decodable_extension_mask_is_what_the_capset_type_would_build() {
    let mask = ExtensionMask::enumerating(info::decodable_extension_numbers()).expect("in range");
    let built = ExtensionMask::enumerating(
        (1..1024u32)
            .filter(|n| info::DECODABLE_EXTENSION_MASK[*n as usize / 32] & (1 << (n % 32)) != 0),
    )
    .expect("in range");
    assert_eq!(mask, built);
    assert!(info::DECODABLE_EXTENSION_MASK[0] & 1 != 0, "sentinel set");
    let names: Vec<&str> = info::EXTENSIONS
        .iter()
        .filter(|e| e.decodable)
        .map(|e| e.name)
        .collect();
    assert_eq!(
        names,
        ["VK_EXT_command_serialization", "VK_MESA_venus_protocol"]
    );
    assert!(info::EXTENSIONS.windows(2).all(|w| w[0].name < w[1].name));
    assert_eq!(
        info::extension("VK_MESA_venus_protocol").map(|e| e.spec_version),
        Some(4)
    );
    assert_eq!(info::WIRE_FORMAT_VERSION, 1);
    assert_eq!(info::VK_XML_VERSION, vk_make_api_version(0, 1, 4, 343));
    assert_eq!(info::CHAIN_API_VERSION, vk_make_api_version(0, 1, 3, 0));
}
