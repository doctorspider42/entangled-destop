//! A guest, played by hand, for the executor's tests — fake host and real
//! GPU alike: a real [`VenusRenderer`] with a real ring and a real reply blob,
//! commands encoded with the generated **driver-side** `encode_command`, and
//! replies decoded with the generated `decode_reply`, exactly as Mesa's venus
//! would do both. No `unsafe`: the ring and the reply window are reached
//! through [`RingPages`]' bounded copies and test-only word accessors.

use std::sync::Arc;
use std::time::{Duration, Instant};

use virtio_core::GuestMem;

use crate::protocol::{ResourceCreateBlob, BLOB_FLAG_USE_MAPPABLE, BLOB_MEM_HOST3D};
use crate::renderer::Renderer3d;
use crate::venus::capset::vk_make_api_version;
use crate::venus::protocol::*;
use crate::venus::pump::STATUS_FATAL;
use crate::venus::renderer::VenusRenderer;
use crate::venus::shmem::RingPages;
use crate::venus::transport::{Opcode, STYPE_RING_CREATE_INFO_MESA};
use crate::venus::wire::{CommandHeader, Decoder, Encoder, COMMAND_GENERATE_REPLY};

use super::host::HostVulkan;
use super::ExecutorFactory;

/// The context the harness creates.
pub const CTX: u32 = 1;
/// The ring's resource.
pub const RING_RES: u32 = 7;
/// The reply pool's resource.
pub const REPLY_RES: u32 = 8;
/// The ring handle.
pub const RING: u64 = 0x5555_0000_0000_0001;
/// Size of the reply pool, as Mesa's (1 MiB, `vn_instance.c:315-316`).
pub const REPLY_BYTES: u64 = 1 << 20;
/// Size of each reply window the harness binds.
pub const WINDOW: u64 = 16 << 10;

const HEAD: u64 = 0;
const TAIL: u64 = 4;
const STATUS: u64 = 8;
const BUFFER_OFFSET: u64 = 0x100;
const BUFFER: u64 = 0x1_0000;
const RING_BYTES: u64 = BUFFER_OFFSET + BUFFER + 0x100;

/// What a submission came to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// Every byte consumed.
    Consumed,
    /// The ring went fatal with `head` here.
    Fatal {
        /// `head` as the guest reads it.
        head: u32,
    },
}

/// The fixture. See the module docs.
pub struct Harness<H: HostVulkan> {
    /// The renderer under test.
    pub renderer: VenusRenderer<ExecutorFactory<H>>,
    /// Guest memory, which a `HOST3D` blob never touches.
    pub mem: Arc<GuestMem>,
    ring: Arc<RingPages>,
    /// The reply pool the harness binds its windows in.
    pub reply: Arc<RingPages>,
    tail: u32,
    reply_at: u64,
    /// Where the last submission started: the `head` a fatal on its first
    /// command leaves.
    pub last_start: u32,
}

fn blob(resource_id: u32, size: u64) -> ResourceCreateBlob {
    ResourceCreateBlob {
        resource_id,
        blob_mem: BLOB_MEM_HOST3D,
        blob_flags: BLOB_FLAG_USE_MAPPABLE,
        nr_entries: 0,
        blob_id: 0,
        size,
    }
}

fn encoded(f: impl FnOnce(&mut Encoder)) -> Vec<u8> {
    let mut enc = Encoder::new();
    f(&mut enc);
    enc.finish().expect("the fixture encodes")
}

/// `vkSetReplyCommandStreamMESA` as a guest writes it into a ring.
#[must_use]
pub fn set_reply(resource_id: u32, offset: u64, size: u64) -> Vec<u8> {
    encoded(|enc| {
        enc.command_header(CommandHeader {
            opcode: Opcode::SetReplyCommandStream.as_u32(),
            flags: 0,
        })
        .expect("encode");
        enc.simple_pointer(true).expect("encode");
        enc.u32(resource_id).expect("encode");
        enc.size(offset).expect("encode");
        enc.size(size).expect("encode");
    })
}

/// `vkSeekReplyCommandStreamMESA`.
#[must_use]
pub fn seek_reply(position: u64) -> Vec<u8> {
    encoded(|enc| {
        enc.command_header(CommandHeader {
            opcode: Opcode::SeekReplyCommandStream.as_u32(),
            flags: 0,
        })
        .expect("encode");
        enc.size(position).expect("encode");
    })
}

/// `command` as Mesa encodes it, reply flag set.
#[must_use]
pub fn call_bytes(command: &Command<'_>) -> Vec<u8> {
    encoded(|enc| {
        command
            .encode_command(enc, COMMAND_GENERATE_REPLY)
            .expect("the driver-side encoder takes it");
    })
}

/// `command` as Mesa encodes an async one: no reply.
#[must_use]
pub fn async_bytes(command: &Command<'_>) -> Vec<u8> {
    encoded(|enc| {
        command
            .encode_command(enc, 0)
            .expect("the driver-side encoder takes it");
    })
}

fn create_ring_stream() -> Vec<u8> {
    encoded(|enc| {
        enc.command_header(CommandHeader {
            opcode: Opcode::CreateRing.as_u32(),
            flags: 0,
        })
        .expect("encode");
        enc.handle(RING).expect("encode");
        enc.simple_pointer(true).expect("encode");
        enc.i32(STYPE_RING_CREATE_INFO_MESA).expect("encode");
        enc.simple_pointer(false).expect("encode");
        enc.flags(0).expect("encode");
        enc.u32(RING_RES).expect("encode");
        for value in [
            0,
            RING_BYTES,
            1_000,
            HEAD,
            TAIL,
            STATUS,
            BUFFER_OFFSET,
            BUFFER,
            BUFFER_OFFSET + BUFFER,
            4,
        ] {
            enc.u64(value).expect("encode");
        }
    })
}

fn notify_stream() -> Vec<u8> {
    encoded(|enc| {
        enc.command_header(CommandHeader {
            opcode: Opcode::NotifyRing.as_u32(),
            flags: 0,
        })
        .expect("encode");
        enc.handle(RING).expect("encode");
        enc.u32(0).expect("encode");
        enc.flags(0).expect("encode");
    })
}

impl<H: HostVulkan> Harness<H> {
    /// A renderer over `host` with context [`CTX`], a ring and a reply pool.
    pub fn new(host: Arc<H>) -> Self {
        let mut renderer = VenusRenderer::new(ExecutorFactory::new(host));
        let mem = Arc::new(virtio_core::testing::guest_memory(1 << 20));
        renderer
            .ctx_create(CTX, crate::CAPSET_VENUS, "venus")
            .expect("a venus context");
        renderer
            .create_blob(CTX, &blob(RING_RES, RING_BYTES), &mem, &[])
            .expect("the ring blob");
        renderer
            .create_blob(CTX, &blob(REPLY_RES, REPLY_BYTES), &mem, &[])
            .expect("the reply pool");
        renderer
            .submit(CTX, &create_ring_stream())
            .expect("the ring is adopted");
        let ring = renderer.blob_pages(RING_RES).expect("the ring blob");
        let reply = renderer.blob_pages(REPLY_RES).expect("the reply pool");
        Self {
            renderer,
            mem,
            ring,
            reply,
            tail: 0,
            reply_at: 0,
            last_start: 0,
        }
    }

    /// Create another host blob, on `ctx_id`.
    pub fn create_blob(&mut self, ctx_id: u32, resource_id: u32, size: u64) {
        self.renderer
            .create_blob(ctx_id, &blob(resource_id, size), &self.mem, &[])
            .expect("a host blob");
    }

    /// `head` as the guest reads it.
    #[must_use]
    pub fn head(&self) -> u32 {
        self.ring
            .guest_load_word(HEAD)
            .expect("head is in the ring")
    }

    /// Whether the ring published `FATAL`.
    #[must_use]
    pub fn fatal(&self) -> bool {
        self.ring
            .guest_load_word(STATUS)
            .is_some_and(|s| s & STATUS_FATAL != 0)
    }

    /// One ring submission: write `bytes` at `tail`, store `tail`, ring the
    /// doorbell, and wait until `head` catches up or the ring dies.
    pub fn submit(&mut self, bytes: &[u8]) -> Outcome {
        self.produce(bytes);
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            if self.fatal() {
                return Outcome::Fatal { head: self.head() };
            }
            if self.head() == self.tail {
                return Outcome::Consumed;
            }
            assert!(Instant::now() < deadline, "the ring never caught up");
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    /// Wait until `head` reaches `head`.
    pub fn wait_head(&self, head: u32) {
        let deadline = Instant::now() + Duration::from_secs(20);
        while self.head() != head {
            assert!(Instant::now() < deadline, "head never reached {head}");
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    /// Write `bytes` at `tail`, store `tail` and ring the doorbell, without
    /// waiting for anything.
    pub fn produce(&mut self, bytes: &[u8]) {
        self.last_start = self.tail;
        let mut at = u64::from(self.tail);
        let mut rest = bytes;
        while !rest.is_empty() {
            let position = at % BUFFER;
            let room = usize::try_from(BUFFER - position).expect("fits");
            let (now, later) = rest.split_at(rest.len().min(room));
            self.ring
                .write_bytes(BUFFER_OFFSET + position, now)
                .expect("inside the ring");
            at += now.len() as u64;
            rest = later;
        }
        let len = u32::try_from(bytes.len()).expect("a fixture batch fits a u32");
        self.tail = self.tail.wrapping_add(len);
        assert!(self.ring.guest_store_word(TAIL, self.tail));
        // Harmless when the worker is already polling, and answered with a
        // refusal once the ring is dead, which is the case being waited for.
        let _ = self.renderer.submit(CTX, &notify_stream());
    }

    /// `tail` so far: where the next command will start.
    #[must_use]
    pub fn tail(&self) -> u32 {
        self.tail
    }

    /// The next reply window, prefilled with a pattern no reply is made of.
    fn next_window(&mut self) -> u64 {
        if self.reply_at + WINDOW > REPLY_BYTES {
            self.reply_at = 0;
        }
        let at = self.reply_at;
        self.reply_at += WINDOW;
        let junk = vec![0xcc; usize::try_from(WINDOW).expect("fits")];
        self.reply.write_bytes(at, &junk).expect("inside the pool");
        at
    }

    /// A `vn_call_*`: `SetReply`, then `command` with the reply flag, as two
    /// submissions (Mesa's shape, spec §0.1); the reply decoded with the
    /// generated driver-side decoder into a copy of `command`.
    ///
    /// # Errors
    /// `head` where the ring died, if it did.
    pub fn call(&mut self, command: &Command<'_>) -> Result<Command<'static>, u32> {
        let at = self.next_window();
        if let Outcome::Fatal { head } = self.submit(&set_reply(REPLY_RES, at, WINDOW)) {
            return Err(head);
        }
        if let Outcome::Fatal { head } = self.submit(&call_bytes(command)) {
            return Err(head);
        }
        let mut reply = vec![0u8; usize::try_from(WINDOW).expect("fits")];
        self.reply
            .read_bytes(at, &mut reply)
            .expect("inside the pool");
        let reply: &'static [u8] = Box::leak(reply.into_boxed_slice());
        // The driver side re-encodes the command to get a value it can
        // decode into; the round trip through the encoder is what makes the
        // copy `'static`.
        let bytes: &'static [u8] = Box::leak(call_bytes(command).into_boxed_slice());
        let mut out = {
            let mut dec = Decoder::new(bytes);
            Command::decode_next(&mut dec)
                .expect("the renderer-side decoder takes what the driver encoded")
                .1
        };
        let mut dec = Decoder::new(reply);
        out.decode_reply(&mut dec)
            .expect("the reply decodes the way the guest's driver decodes it");
        Ok(out)
    }

    /// A `vn_async_*`: the command without the reply flag.
    ///
    /// # Errors
    /// `head` where the ring died, if it did.
    pub fn send(&mut self, command: &Command<'_>) -> Result<(), u32> {
        match self.submit(&async_bytes(command)) {
            Outcome::Consumed => Ok(()),
            Outcome::Fatal { head } => Err(head),
        }
    }
}

// ------------------------------------------------------ command builders
//
// The bring-up's commands, as Mesa's venus fills them in before encoding.

pub const INSTANCE: u64 = 0x10;
pub const PHYSICAL: u64 = 0x20;
pub const DEVICE: u64 = 0x30;
pub const POOL: u64 = 0x40;
pub const QUEUE: u64 = 0x50;
pub const IMAGE: u64 = 0x60;

/// `VK_FORMAT_R8G8B8A8_UNORM`.
pub const RGBA8: i32 = 37;

pub fn enumerate_instance_version() -> Command<'static> {
    Command::EnumerateInstanceVersion(EnumerateInstanceVersionArgs {
        p_api_version: Some(0),
        ret: 0,
    })
}

pub fn create_instance(id: u64) -> Command<'static> {
    Command::CreateInstance(CreateInstanceArgs {
        p_create_info: Some(VkInstanceCreateInfo {
            p_application_info: Some(VkApplicationInfo {
                p_application_name: Some(b"vulkaninfo"),
                application_version: 1,
                p_engine_name: None,
                engine_version: 0,
                api_version: vk_make_api_version(0, 1, 3, 0),
            }),
            ..Default::default()
        }),
        p_instance: Some(VkInstance(id)),
        ret: 0,
    })
}

pub fn enumerate(instance: u64, ids: Option<Vec<u64>>) -> Command<'static> {
    Command::EnumeratePhysicalDevices(EnumeratePhysicalDevicesArgs {
        instance: VkInstance(instance),
        p_physical_device_count: Some(ids.as_ref().map_or(0, |v| v.len() as u32)),
        p_physical_devices: ids.map(|v| v.into_iter().map(VkPhysicalDevice).collect()),
        ret: 0,
    })
}

pub fn properties(physical: u64) -> Command<'static> {
    Command::GetPhysicalDeviceProperties(GetPhysicalDevicePropertiesArgs {
        physical_device: VkPhysicalDevice(physical),
        p_properties: Some(VkPhysicalDeviceProperties::default()),
    })
}

pub fn create_device(
    physical: u64,
    id: u64,
    chain: Vec<VkDeviceCreateInfoNext>,
) -> Command<'static> {
    Command::CreateDevice(CreateDeviceArgs {
        physical_device: VkPhysicalDevice(physical),
        p_create_info: Some(VkDeviceCreateInfo {
            p_next: chain,
            queue_create_info_count: 1,
            p_queue_create_infos: Some(vec![VkDeviceQueueCreateInfo {
                p_next: Vec::new(),
                flags: 0,
                queue_family_index: 0,
                queue_count: 1,
                p_queue_priorities: Some(vec![1.0]),
            }]),
            ..Default::default()
        }),
        p_device: Some(VkDevice(id)),
        ret: 0,
    })
}

pub fn device_queue(device: u64, id: u64, ring_idx: u32) -> Command<'static> {
    Command::GetDeviceQueue2(GetDeviceQueue2Args {
        device: VkDevice(device),
        p_queue_info: Some(VkDeviceQueueInfo2 {
            p_next: vec![VkDeviceQueueInfo2Next::VkDeviceQueueTimelineInfoMESA(
                VkDeviceQueueTimelineInfoMESA { ring_idx },
            )],
            flags: 0,
            queue_family_index: 0,
            queue_index: 0,
        }),
        p_queue: Some(VkQueue(id)),
    })
}

pub fn create_pool(device: u64, id: u64) -> Command<'static> {
    Command::CreateCommandPool(CreateCommandPoolArgs {
        device: VkDevice(device),
        p_create_info: Some(VkCommandPoolCreateInfo {
            flags: 0x2,
            queue_family_index: 0,
        }),
        p_command_pool: Some(VkCommandPool(id)),
        ret: 0,
    })
}

pub fn image_info() -> VkImageCreateInfo<'static> {
    VkImageCreateInfo {
        image_type: 1,
        format: RGBA8,
        extent: VkExtent3D {
            width: 64,
            height: 64,
            depth: 1,
        },
        mip_levels: 1,
        array_layers: 1,
        samples: 1,
        tiling: 0,
        usage: 0x6,
        ..Default::default()
    }
}

pub fn create_image(device: u64, id: u64, info: VkImageCreateInfo<'static>) -> Command<'static> {
    Command::CreateImage(CreateImageArgs {
        device: VkDevice(device),
        p_create_info: Some(info),
        p_image: Some(VkImage(id)),
        ret: 0,
    })
}

pub fn memory_requirements(device: u64, image: u64) -> Command<'static> {
    Command::GetImageMemoryRequirements2(GetImageMemoryRequirements2Args {
        device: VkDevice(device),
        p_info: Some(VkImageMemoryRequirementsInfo2 {
            p_next: Vec::new(),
            image: VkImage(image),
        }),
        p_memory_requirements: Some(VkMemoryRequirements2 {
            p_next: vec![VkMemoryRequirements2Next::VkMemoryDedicatedRequirements(
                VkMemoryDedicatedRequirements::default(),
            )],
            ..Default::default()
        }),
    })
}

pub fn destroy_image(device: u64, image: u64) -> Command<'static> {
    Command::DestroyImage(DestroyImageArgs {
        device: VkDevice(device),
        image: VkImage(image),
    })
}

/// Rows 2–5 of spec §1.2: version, instance, both enumerations.
pub fn boot<H: HostVulkan>(h: &mut Harness<H>) {
    h.call(&enumerate_instance_version()).expect("version");
    h.call(&create_instance(INSTANCE)).expect("instance");
    h.call(&enumerate(INSTANCE, None)).expect("count");
    h.call(&enumerate(INSTANCE, Some(vec![PHYSICAL])))
        .expect("ids");
}

/// Boot plus a device, its queue and a command pool.
pub fn with_device<H: HostVulkan>(h: &mut Harness<H>) {
    boot(h);
    let Command::CreateDevice(reply) = h
        .call(&create_device(PHYSICAL, DEVICE, Vec::new()))
        .expect("device")
    else {
        panic!("wrong reply")
    };
    assert_eq!(reply.ret, VK_SUCCESS);
    h.send(&create_pool(DEVICE, POOL)).expect("pool");
    h.call(&device_queue(DEVICE, QUEUE, 1)).expect("queue");
}
