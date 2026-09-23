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
    // A made-up sType, and a real structure no Properties2 chain admits.
    for stype in [0x7fff_0001, VK_STRUCTURE_TYPE_IMAGE_CREATE_INFO] {
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
        refusal(&Bytes::default().u32(23).u32(0).done()),
        ProtocolError::NotGenerated {
            opcode: 23,
            command: "vkMapMemory"
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
    // The whole protocol: every command is generated here, hand-written in
    // venus::transport, or one the protocol cannot serialize at all — and
    // exactly one of the three.
    const NOT_SERIALIZABLE: [&str; 20] = [
        "vkGetDeviceProcAddr",
        "vkGetInstanceProcAddr",
        "vkMapMemory",
        "vkGetMemoryFdKHR",
        "vkGetMemoryFdPropertiesKHR",
        "vkGetSemaphoreFdKHR",
        "vkImportSemaphoreFdKHR",
        "vkGetFenceFdKHR",
        "vkImportFenceFdKHR",
        "vkUpdateDescriptorSetWithTemplate",
        "vkCmdPushDescriptorSetWithTemplate",
        "vkCopyAccelerationStructureToMemoryKHR",
        "vkCopyMemoryToAccelerationStructureKHR",
        "vkBuildAccelerationStructuresKHR",
        "vkCopyMemoryToImage",
        "vkCopyImageToMemory",
        "vkMapMemory2",
        "vkCmdPushDescriptorSetWithTemplate2",
        "vkWriteSamplerDescriptorsEXT",
        "vkWriteResourceDescriptorsEXT",
    ];
    let mut kinds = [0usize; 3];
    for opcode in 0..1024u32 {
        let Some(name) = command_type_name(opcode) else {
            continue;
        };
        let generated = GENERATED_COMMANDS.iter().any(|(op, _)| *op == opcode);
        let transport = Opcode::from_u32(opcode).is_some();
        let impossible = NOT_SERIALIZABLE.contains(&name);
        assert_eq!(
            u32::from(generated) + u32::from(transport) + u32::from(impossible),
            1,
            "{name} (opcode {opcode})"
        );
        kinds[usize::from(transport) + 2 * usize::from(impossible)] += 1;
    }
    assert_eq!(kinds, [315, 10, 20], "generated, transport, unserializable");
    assert_eq!(GENERATED_COMMANDS.len(), 315);
    assert!(GENERATED_COMMANDS
        .iter()
        .all(|(op, name)| command_type_name(*op) == Some(*name)));
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
    // Every extension of the protocol decodes, so the decodable mask is the
    // whole protocol's — what virglrenderer advertises (the differential
    // harness compares both with the C renderer's table).
    assert_eq!(info::EXTENSIONS.len(), 187);
    assert!(info::EXTENSIONS.iter().all(|e| e.decodable));
    assert_eq!(
        info::DECODABLE_EXTENSION_MASK,
        info::PROTOCOL_EXTENSION_MASK
    );
    assert!(info::EXTENSIONS.windows(2).all(|w| w[0].name < w[1].name));
    assert_eq!(
        info::extension("VK_MESA_venus_protocol").map(|e| e.spec_version),
        Some(4)
    );
    let custom_border_color = info::extension("VK_EXT_custom_border_color").expect("known");
    assert_eq!(custom_border_color.number, 288);
    assert!(info::DECODABLE_EXTENSION_MASK[288 / 32] & (1 << (288 % 32)) != 0);
    assert_eq!(info::WIRE_FORMAT_VERSION, 1);
    assert_eq!(info::VK_XML_VERSION, vk_make_api_version(0, 1, 4, 343));
    assert_eq!(info::CHAIN_API_VERSION, vk_make_api_version(0, 1, 4, 0));

    // The structure table is sorted, and knows where each structure comes
    // from.
    assert!(info::STRUCTURES.windows(2).all(|w| w[0].stype < w[1].stype));
    let features2 = info::structure(VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_FEATURES_2).expect("known");
    assert_eq!(features2.core, Some(vk_make_api_version(0, 1, 1, 0)));
    let timeline =
        info::structure(VK_STRUCTURE_TYPE_DEVICE_QUEUE_TIMELINE_INFO_MESA).expect("known");
    assert_eq!(timeline.core, None);
    assert_eq!(timeline.extensions, ["VK_MESA_venus_protocol"]);
    assert!(info::structure(0x7fff_0001).is_none());
}

// ---- the constructs past the bring-up set ------------------------------------------
//
// Hand-written bytes again, one command per construct the whole protocol
// adds, each followed by the ways a guest can get it wrong. The differential
// harness covers the same constructs against the C at scale; these pin the
// layout down independently of both generators.

/// Decode, re-encode, compare; then every truncation is refused.
fn round_trip_and_truncate(bytes: &[u8]) -> Command<'_> {
    let (header, cmd) = decode_one(bytes).expect("decodes");
    assert_eq!(reencode(&cmd, header.flags), bytes, "{}", cmd.name());
    for cut in 0..bytes.len() {
        let mut dec = Decoder::new(&bytes[..cut]);
        assert!(
            Command::decode_next(&mut dec).is_err(),
            "a {cut}-byte prefix of {} decoded",
            cmd.name()
        );
        assert!(dec.is_fatal());
    }
    cmd
}

/// vkCmdClearColorImage with a `VkClearColorValue` under `tag`.
fn clear_color_bytes(tag: u32) -> Vec<u8> {
    Bytes::default()
        .u32(VK_COMMAND_TYPE_CMD_CLEAR_COLOR_IMAGE_EXT)
        .u32(0)
        .u64(5) // commandBuffer
        .u64(6) // image
        .i32(1) // imageLayout
        .u64(1) // pColor present
        .u32(tag)
        .u64(4) // array_size of the member
        .u32(0x3f80_0000)
        .u32(2)
        .u32(3)
        .u32(4)
        .u32(0) // rangeCount
        .u64(0) // pRanges: null, count 0
        .done()
}

#[test]
fn a_default_tag_union_carries_its_tag_and_the_member_it_selects() {
    // Tag 2, the one Mesa always sends: the uint32 member.
    let bytes = clear_color_bytes(2);
    let Command::CmdClearColorImage(args) = round_trip_and_truncate(&bytes) else {
        panic!("opcode 119");
    };
    assert_eq!(
        args.p_color,
        Some(VkClearColorValue::Uint32([0x3f80_0000, 2, 3, 4]))
    );
    assert_eq!(VkClearColorValue::DEFAULT_TAG, 2);

    // Tag 0 selects float32, which the renderer accepts too; it re-encodes
    // under the tag it came with.
    let bytes = clear_color_bytes(0);
    let Command::CmdClearColorImage(args) = round_trip_and_truncate(&bytes) else {
        panic!("opcode 119");
    };
    assert!(matches!(
        args.p_color,
        Some(VkClearColorValue::Float32([one, _, _, _])) if one == 1.0
    ));

    // A tag with no member is fatal (`default: vn_cs_decoder_set_fatal`).
    assert!(matches!(
        refusal(&clear_color_bytes(3)),
        ProtocolError::UnknownUnionTag {
            union: "VkClearColorValue",
            tag: 3
        }
    ));
    assert!(matches!(
        refusal(&clear_color_bytes(u32::MAX)),
        ProtocolError::UnknownUnionTag { .. }
    ));
}

#[test]
fn a_union_member_the_wire_cannot_carry_is_only_ever_null() {
    // VkDeviceOrHostAddressKHR: tag 0 is a device address, tag 1 a host
    // pointer, which only travels as its null marker.
    let bytes = Bytes::default().u32(0).u64(0x1000).done();
    let mut dec = Decoder::new(&bytes);
    assert_eq!(
        VkDeviceOrHostAddressKHR::decode(&mut dec),
        Ok(VkDeviceOrHostAddressKHR::DeviceAddress(0x1000))
    );
    let bytes = Bytes::default().u32(1).u64(0).done();
    let mut dec = Decoder::new(&bytes);
    let host = VkDeviceOrHostAddressKHR::decode(&mut dec).expect("a null host address");
    assert_eq!(host, VkDeviceOrHostAddressKHR::HostAddress);
    let mut enc = Encoder::new();
    host.encode(&mut enc).expect("encodes");
    assert_eq!(enc.finish().expect("finishes"), bytes);
    let bytes = Bytes::default().u32(1).u64(1).done();
    let mut dec = Decoder::new(&bytes);
    assert!(matches!(
        VkDeviceOrHostAddressKHR::decode(&mut dec),
        Err(ProtocolError::UnsupportedPointer {
            owner: "VkDeviceOrHostAddressKHR",
            field: "hostAddress"
        })
    ));
    assert!(dec.is_fatal());
}

/// vkWriteResourceDescriptorMESA: a `VkResourceDescriptorInfoEXT` whose
/// `type` selects its union, and an 8-byte blob the reply carries.
fn resource_descriptor_bytes(selector: i32, tag: i32) -> Vec<u8> {
    Bytes::default()
        .u32(VK_COMMAND_TYPE_WRITE_RESOURCE_DESCRIPTOR_MESA_EXT)
        .u32(COMMAND_GENERATE_REPLY)
        .u64(4) // device
        .u64(1) // pResource present
        .i32(VK_STRUCTURE_TYPE_RESOURCE_DESCRIPTOR_INFO_EXT)
        .u64(0) // pNext
        .i32(selector) // type
        .i32(tag) // the union's own tag
        .u64(1) // pAddressRange present
        .u64(0xdead_0000) // address
        .u64(0x100) // size
        .u64(8) // dataSize
        .u64(8) // pData: an output, sized and not sent
        .done()
}

#[test]
fn a_selected_union_is_decoded_under_its_selector_and_must_agree_with_it() {
    let bytes = resource_descriptor_bytes(
        VK_DESCRIPTOR_TYPE_UNIFORM_BUFFER,
        VK_DESCRIPTOR_TYPE_UNIFORM_BUFFER,
    );
    let Command::WriteResourceDescriptorMESA(mut args) = round_trip_and_truncate(&bytes) else {
        panic!("opcode 336");
    };
    let resource = args.p_resource.clone().expect("present");
    assert_eq!(resource.type_, VK_DESCRIPTOR_TYPE_UNIFORM_BUFFER);
    assert_eq!(
        resource.data,
        VkResourceDescriptorDataEXT::PAddressRange(Some(VkDeviceAddressRangeEXT {
            address: 0xdead_0000,
            size: 0x100,
        }))
    );
    assert_eq!(
        args.p_data,
        Some(Vec::new()),
        "an output blob is not allocated at decode"
    );

    // The blob goes into the reply: its size, then the bytes, padded.
    args.ret = VK_SUCCESS;
    args.p_data = Some(vec![1, 2, 3, 4, 5, 6, 7, 8]);
    let reply = reply_of(&Command::WriteResourceDescriptorMESA(args.clone()));
    let expected = Bytes::default()
        .u32(VK_COMMAND_TYPE_WRITE_RESOURCE_DESCRIPTOR_MESA_EXT)
        .i32(VK_SUCCESS)
        .u64(8)
        .u32(0x0403_0201)
        .u32(0x0807_0605)
        .done();
    assert_eq!(reply, expected);
    // ... and the guest's decoder reads exactly that back.
    let mut dec = Decoder::new(&reply);
    let mut guest = args.clone();
    guest.p_data = Some(Vec::new());
    guest
        .decode_reply(&mut dec)
        .expect("the driver side decodes it");
    assert_eq!(dec.remaining(), 0);
    assert_eq!(guest.p_data, Some(vec![1, 2, 3, 4, 5, 6, 7, 8]));

    // A blob that disagrees with dataSize is never written.
    args.p_data = Some(vec![1, 2, 3]);
    assert!(matches!(
        Command::WriteResourceDescriptorMESA(args.clone()).reply_bytes(1 << 20),
        Err(ProtocolError::ArrayCountMismatch {
            count: 8,
            len: 3,
            ..
        })
    ));

    // A second tag for the same member, but not the selector's: refused,
    // where the C would hand the driver the member under another name.
    assert!(matches!(
        refusal(&resource_descriptor_bytes(
            VK_DESCRIPTOR_TYPE_UNIFORM_BUFFER,
            VK_DESCRIPTOR_TYPE_STORAGE_BUFFER
        )),
        ProtocolError::UnionSelectorMismatch {
            owner: "VkResourceDescriptorInfoEXT",
            field: "data",
            ..
        }
    ));
    // A tag no member answers to.
    assert!(matches!(
        refusal(&resource_descriptor_bytes(999, 999)),
        ProtocolError::UnknownUnionTag {
            union: "VkResourceDescriptorDataEXT",
            tag: 999
        }
    ));
    // Encode side: a selector that selects another member.
    let mut enc = Encoder::new();
    assert!(matches!(
        resource
            .data
            .encode_tagged(&mut enc, VK_DESCRIPTOR_TYPE_SAMPLED_IMAGE),
        Err(ProtocolError::UnionTagMismatch { .. })
    ));
}

/// vkCmdDrawMultiEXT with two draws, packed, and the stride the guest sent.
fn draw_multi_bytes(stride: u32) -> Vec<u8> {
    Bytes::default()
        .u32(VK_COMMAND_TYPE_CMD_DRAW_MULTI_EXT_EXT)
        .u32(0)
        .u64(3) // commandBuffer
        .u32(2) // drawCount
        .u64(2) // array_size
        .u32(0)
        .u32(3) // firstVertex, vertexCount
        .u32(3)
        .u32(6)
        .u32(1) // instanceCount
        .u32(0) // firstInstance
        .u32(stride)
        .done()
}

#[test]
fn a_strided_array_travels_packed_and_its_stride_must_be_the_element_size() {
    let bytes = draw_multi_bytes(8);
    let Command::CmdDrawMultiEXT(args) = round_trip_and_truncate(&bytes) else {
        panic!("opcode 247");
    };
    assert_eq!(
        args.p_vertex_info,
        Some(vec![
            VkMultiDrawInfoEXT {
                first_vertex: 0,
                vertex_count: 3
            },
            VkMultiDrawInfoEXT {
                first_vertex: 3,
                vertex_count: 6
            },
        ])
    );
    // Mesa packs the elements and rewrites the stride to their size; any
    // other stride would have the driver read past the packed array.
    for stride in [0, 4, 12, 16, u32::MAX] {
        assert!(matches!(
            refusal(&draw_multi_bytes(stride)),
            ProtocolError::BadStride { expected: 8, .. }
        ));
    }
    // Encoding as the driver does writes sizeof(element), whatever the
    // caller's stride.
    let cmd = Command::CmdDrawMultiEXT(CmdDrawMultiEXTArgs { stride: 40, ..args });
    assert_eq!(reencode(&cmd, 0), draw_multi_bytes(8));
}

/// vkCmdBuildAccelerationStructuresKHR, one build info with `geometry`
/// geometries, and `ranges` range infos behind an inner size of `inner`.
fn build_bytes(geometry: u32, inner: u64, ranges: u32) -> Vec<u8> {
    let mut b = Bytes::default()
        .u32(VK_COMMAND_TYPE_CMD_BUILD_ACCELERATION_STRUCTURES_KHR_EXT)
        .u32(0)
        .u64(3) // commandBuffer
        .u32(1) // infoCount
        .u64(1) // pInfos
        .i32(VK_STRUCTURE_TYPE_ACCELERATION_STRUCTURE_BUILD_GEOMETRY_INFO_KHR)
        .u64(0) // pNext
        .i32(0) // type
        .u32(0) // flags
        .i32(0) // mode
        .u64(0) // srcAccelerationStructure
        .u64(9) // dstAccelerationStructure
        .u32(geometry) // geometryCount
        .u64(0) // pGeometries: null (optional)
        .u64(0) // ppGeometries: null (optional)
        .u32(0) // scratchData: tag 0,
        .u64(0x2000) // ... a device address
        .u64(1) // ppBuildRangeInfos: infoCount outer arrays
        .u64(inner); // the inner array for pInfos[0]
    for i in 0..ranges {
        b = b.u32(i).u32(0).u32(0).u32(0);
    }
    b.done()
}

#[test]
fn a_nested_array_counts_each_inner_array_from_the_outer_arrays_element() {
    let bytes = build_bytes(2, 2, 2);
    let Command::CmdBuildAccelerationStructuresKHR(args) = round_trip_and_truncate(&bytes) else {
        panic!("opcode 306");
    };
    let ranges = args.pp_build_range_infos.expect("present");
    assert_eq!(ranges.len(), 1);
    assert_eq!(ranges[0].len(), 2);
    assert_eq!(ranges[0][1].primitive_count, 1);
    // An empty inner array is legal when the geometry count is zero.
    round_trip_and_truncate(&build_bytes(0, 0, 0));

    // The inner size must be pInfos[i].geometryCount; there is no null form.
    assert!(matches!(
        refusal(&build_bytes(2, 1, 1)),
        ProtocolError::Wire(WireError::ArrayLengthMismatch {
            expected: 2,
            found: 1
        })
    ));
    assert!(matches!(
        refusal(&build_bytes(2, 0, 0)),
        ProtocolError::Wire(WireError::ArrayLengthMismatch {
            expected: 2,
            found: 0
        })
    ));
    // A count the stream cannot hold is refused before anything is
    // allocated for it.
    assert!(matches!(
        refusal(&build_bytes(u32::MAX, u64::from(u32::MAX), 1)),
        ProtocolError::Wire(WireError::ArrayLongerThanStream { .. })
    ));
    // Encode side: an inner vector that disagrees with its count.
    let infos = VkAccelerationStructureBuildGeometryInfoKHR {
        geometry_count: 2,
        dst_acceleration_structure: VkAccelerationStructureKHR(9),
        ..Default::default()
    };
    let cmd = Command::CmdBuildAccelerationStructuresKHR(CmdBuildAccelerationStructuresKHRArgs {
        command_buffer: VkCommandBuffer(3),
        info_count: 1,
        p_infos: Some(vec![infos]),
        pp_build_range_infos: Some(vec![vec![
            VkAccelerationStructureBuildRangeInfoKHR::default(),
        ]]),
    });
    let mut enc = Encoder::new();
    assert!(matches!(
        cmd.encode_command(&mut enc, 0),
        Err(ProtocolError::ArrayCountMismatch {
            count: 2,
            len: 1,
            ..
        })
    ));
}

/// vkCreateShaderModule with `code_size` bytes of code, sent as `words`
/// words.
fn shader_bytes(code_size: u64, words: u32) -> Vec<u8> {
    let mut b = Bytes::default()
        .u32(VK_COMMAND_TYPE_CREATE_SHADER_MODULE_EXT)
        .u32(COMMAND_GENERATE_REPLY)
        .u64(4) // device
        .u64(1) // pCreateInfo
        .i32(VK_STRUCTURE_TYPE_SHADER_MODULE_CREATE_INFO)
        .u64(0) // pNext
        .u32(0) // flags
        .u64(code_size)
        .u64(u64::from(words));
    for w in 0..words {
        b = b.u32(0x0723_0203 + w);
    }
    b.u64(0) // pAllocator
        .u64(1) // pShaderModule present
        .u64(0x77) // its id
        .done()
}

/// vkCmdSetSampleMaskEXT for `samples`, with an array size of `size`.
fn sample_mask_bytes(samples: i32, size: u64, words: u32) -> Vec<u8> {
    let mut b = Bytes::default()
        .u32(VK_COMMAND_TYPE_CMD_SET_SAMPLE_MASK_EXT_EXT)
        .u32(0)
        .u64(3)
        .i32(samples)
        .u64(size);
    for _ in 0..words {
        b = b.u32(u32::MAX);
    }
    b.done()
}

#[test]
fn an_arithmetic_len_is_evaluated_as_the_c_evaluates_it() {
    // pCode: codeSize / 4 words.
    let bytes = shader_bytes(8, 2);
    let Command::CreateShaderModule(args) = round_trip_and_truncate(&bytes) else {
        panic!("opcode 59");
    };
    assert_eq!(
        args.p_create_info.expect("present").p_code,
        Some(vec![0x0723_0203, 0x0723_0204])
    );
    // Integer division, as C's: 9 bytes is still two words.
    round_trip_and_truncate(&shader_bytes(9, 2));
    assert!(matches!(
        refusal(&shader_bytes(12, 2)),
        ProtocolError::Wire(WireError::ArrayLengthMismatch {
            expected: 3,
            found: 2
        })
    ));

    // pSampleMask: (samples + 31) / 32 words, in the enum's unsigned type.
    round_trip_and_truncate(&sample_mask_bytes(64, 2, 2));
    round_trip_and_truncate(&sample_mask_bytes(1, 1, 1));
    // 0xffff_fff0 + 31 wraps to 15 in an unsigned int: zero words.
    round_trip_and_truncate(&sample_mask_bytes(-16, 0, 0));
    // 0x8000_0000 + 31, unsigned, is 0x400_0000 words: a real count the
    // stream is far too short for. (Signed arithmetic would make it a
    // negative count, and a mismatch instead.)
    assert!(matches!(
        refusal(&sample_mask_bytes(i32::MIN, 0x0400_0000, 0)),
        ProtocolError::Wire(WireError::ArrayLongerThanStream { .. })
    ));
    assert!(matches!(
        refusal(&sample_mask_bytes(64, 1, 1)),
        ProtocolError::Wire(WireError::ArrayLengthMismatch {
            expected: 2,
            found: 1
        })
    ));
}

/// vkGetDeviceAccelerationStructureCompatibilityKHR with a version blob of
/// `size` bytes.
fn compatibility_bytes(size: u64) -> Vec<u8> {
    let mut b = Bytes::default()
        .u32(VK_COMMAND_TYPE_GET_DEVICE_ACCELERATION_STRUCTURE_COMPATIBILITY_KHR_EXT)
        .u32(COMMAND_GENERATE_REPLY)
        .u64(4) // device
        .u64(1) // pVersionInfo
        .i32(VK_STRUCTURE_TYPE_ACCELERATION_STRUCTURE_VERSION_INFO_KHR)
        .u64(0) // pNext
        .u64(size);
    for i in 0..size.div_ceil(4) {
        b = b.u32(0x0101_0101u32.wrapping_mul(i as u32 + 1));
    }
    b.u64(1) // pCompatibility: an output, sized only
        .done()
}

#[test]
fn a_constant_len_is_the_registry_constant() {
    // pVersionData is 2 * VK_UUID_SIZE bytes, always.
    let bytes = compatibility_bytes(32);
    let Command::GetDeviceAccelerationStructureCompatibilityKHR(args) =
        round_trip_and_truncate(&bytes)
    else {
        panic!("opcode 318");
    };
    let data = args
        .p_version_info
        .expect("present")
        .p_version_data
        .expect("present");
    assert_eq!(data.len(), 32);
    for size in [31, 33, 16] {
        assert!(matches!(
            refusal(&compatibility_bytes(size)),
            ProtocolError::Wire(WireError::ArrayLengthMismatch { expected: 32, .. })
        ));
    }
}

#[test]
fn packed_uint16_arrays_are_two_bytes_each_padded_to_four() {
    // No command at git-70991d4c carries one; `vn_encode_uint16_t_array` is
    // still the generator's (types_scalar.h), and these are its bytes.
    let bytes = [1, 0, 2, 0, 3, 0, 0xaa, 0xbb, 9, 9, 9, 9];
    let mut dec = Decoder::new(&bytes);
    assert_eq!(decode_u16_array(&mut dec, 3), Ok(vec![1, 2, 3]));
    assert_eq!(dec.position(), 8, "six bytes and two of padding");
    let mut enc = Encoder::new();
    encode_u16_array(&mut enc, &[1, 2, 3]).expect("encodes");
    assert_eq!(enc.finish().expect("finishes"), [1, 0, 2, 0, 3, 0, 0, 0]);

    let fixed = Bytes::default().u64(2).u32(0x0005_0004).done();
    let mut dec = Decoder::new(&fixed);
    assert_eq!(decode_u16_fixed::<2>(&mut dec), Ok([4, 5]));
    assert_eq!(dec.remaining(), 0);
    let mut dec = Decoder::new(&fixed);
    assert!(
        decode_u16_fixed::<3>(&mut dec).is_err(),
        "a size of 2 for a [u16; 3]"
    );

    // Truncated, and a count whose byte length overflows.
    let mut dec = Decoder::new(&bytes[..6]);
    assert!(matches!(
        decode_u16_array(&mut dec, 3),
        Err(ProtocolError::Wire(WireError::Truncated { .. }))
    ));
    let mut dec = Decoder::new(&bytes);
    assert!(decode_u16_array(&mut dec, usize::MAX).is_err());
    assert!(dec.is_fatal());
}

#[test]
fn corrupting_the_new_constructs_never_panics() {
    let seeds = [
        clear_color_bytes(2),
        resource_descriptor_bytes(
            VK_DESCRIPTOR_TYPE_UNIFORM_BUFFER,
            VK_DESCRIPTOR_TYPE_UNIFORM_BUFFER,
        ),
        draw_multi_bytes(8),
        build_bytes(2, 2, 2),
        shader_bytes(8, 2),
        sample_mask_bytes(64, 2, 2),
        compatibility_bytes(32),
    ];
    let mut state = 0x9e37_79b9_7f4a_7c15u64;
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
                let mut enc = Encoder::new();
                let _ = cmd.encode_command(&mut enc, 1);
                let _ = cmd.reply_bytes(1 << 20);
                cmd.for_each_link(&mut |_, _| {});
            }
        }
    }
}

#[test]
fn every_pnext_link_of_a_command_is_visited() {
    // vkCreateDevice with two links: the walk the executor judges chains by
    // sees both, with the structure whose chain carries them.
    let bytes = create_device_bytes();
    let (_, cmd) = decode_one(&bytes).expect("decodes");
    let mut seen = Vec::new();
    cmd.for_each_link(&mut |parent, stype| seen.push((parent, stype)));
    assert_eq!(
        seen,
        [
            (
                "VkDeviceCreateInfo",
                VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_VULKAN_1_2_FEATURES
            ),
            (
                "VkDeviceCreateInfo",
                VK_STRUCTURE_TYPE_DEVICE_GROUP_DEVICE_CREATE_INFO
            ),
        ]
    );
    // A command with no chain anywhere visits nothing.
    let bytes = draw_multi_bytes(8);
    let (_, cmd) = decode_one(&bytes).expect("decodes");
    cmd.for_each_link(&mut |_, _| panic!("vkCmdDrawMultiEXT has no chain"));
}
