//! One shared host directory as a virtio-fs device (device type 26): what
//! a Windows guest mounts through virtio-win's `viofs` driver and WinFsp.
//!
//! Two queues, hiprio and one request queue, both served the same way: each
//! chain is a FUSE request (the device-readable half) and room for its reply
//! (the writable half), answered inside `notify` by [`FuseServer`], which
//! translates to the 9P server this crate already trusts with the share.
use std::sync::Arc;

use virtio_core::chain::{self, Segment};
use virtio_core::device::{DeviceError, DeviceResources, DeviceType, VirtioDevice};
use virtio_core::interrupt::Interrupt;
use virtio_core::{GuestMem, MAX_QUEUE_SIZE, VIRTIO_F_VERSION_1};
use virtio_queue::{Queue, QueueT};
use vm_memory::{Bytes, GuestAddress};

use crate::device::{scatter, valid_tag, CHAINS_PER_NOTIFY, MAX_TAG_LEN};
use crate::fuse::{FuseServer, IN_HEADER, MAX_WRITE};
use crate::server::Server;

/// `virtio_fs_config.tag`: 36 bytes, NUL-padded.
const TAG_BYTES: usize = 36;
/// The largest request gathered: a full WRITE plus its headers.
const MAX_REQUEST_BYTES: usize = MAX_WRITE as usize + 4096;
static QUEUE_MAX_SIZES: [u16; 2] = [MAX_QUEUE_SIZE, MAX_QUEUE_SIZE];

/// A shared folder as virtio-fs.
pub struct FsDevice {
    tag: String,
    fuse: FuseServer,
    mem: Option<Arc<GuestMem>>,
    queues: Vec<Queue>,
    interrupt: Option<Arc<dyn Interrupt>>,
}

impl std::fmt::Debug for FsDevice {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FsDevice")
            .field("tag", &self.tag)
            .finish_non_exhaustive()
    }
}

impl FsDevice {
    /// A device sharing `server`'s directory under `tag`.
    pub fn new(tag: &str, server: Server) -> Result<Self, String> {
        if !valid_tag(tag) {
            return Err(format!(
                "mount tag {tag:?} must be 1..={MAX_TAG_LEN} letters, digits, '-', '_' or '.'"
            ));
        }
        Ok(Self {
            tag: tag.to_string(),
            fuse: FuseServer::new(server),
            mem: None,
            queues: Vec::new(),
            interrupt: None,
        })
    }

    fn config_space(&self) -> [u8; TAG_BYTES + 4] {
        let mut config = [0u8; TAG_BYTES + 4];
        config[..self.tag.len()].copy_from_slice(self.tag.as_bytes());
        config[TAG_BYTES..].copy_from_slice(&1u32.to_le_bytes()); // num_request_queues
        config
    }

    fn serve(&mut self, mem: &GuestMem, desc_table: u64, size: u16, head: u16) -> u32 {
        let Ok(segments) = chain::walk(mem, desc_table, size, head) else {
            tracing::warn!(tag = %self.tag, head, "dropping a malformed virtio-fs chain");
            return 0;
        };
        let Ok((readable, writable)) = chain::split_rw(&segments) else {
            tracing::warn!(tag = %self.tag, head, "dropping a virtio-fs chain");
            return 0;
        };
        let Some(request) = gather(mem, readable) else {
            return 0;
        };
        let Some(reply) = self.fuse.handle(&request) else {
            return 0; // FORGET: no reply
        };
        let room: u64 = writable.iter().map(|s| u64::from(s.len)).sum();
        if reply.len() as u64 > room {
            tracing::warn!(tag = %self.tag, "a virtio-fs reply does not fit the guest's buffer");
            return 0;
        }
        scatter(mem, writable, &reply)
    }
}

fn gather(mem: &GuestMem, readable: &[Segment]) -> Option<Vec<u8>> {
    let total: u64 = readable.iter().map(|s| u64::from(s.len)).sum();
    if total < IN_HEADER as u64 || total > MAX_REQUEST_BYTES as u64 {
        return None;
    }
    let mut out = vec![0u8; total as usize];
    let mut at = 0usize;
    for seg in readable {
        let len = seg.len as usize;
        mem.read_slice(&mut out[at..at + len], GuestAddress(seg.addr))
            .ok()?;
        at += len;
    }
    Some(out)
}

impl VirtioDevice for FsDevice {
    fn device_type(&self) -> DeviceType {
        DeviceType::Fs
    }

    fn queue_max_sizes(&self) -> &[u16] {
        &QUEUE_MAX_SIZES
    }

    fn device_features(&self) -> u64 {
        VIRTIO_F_VERSION_1
    }

    fn ack_features(&mut self, negotiated: u64) -> bool {
        negotiated & VIRTIO_F_VERSION_1 != 0 && negotiated & !self.device_features() == 0
    }

    fn read_config(&self, offset: u64, data: &mut [u8]) {
        let config = self.config_space();
        for (i, byte) in data.iter_mut().enumerate() {
            *byte = usize::try_from(offset.saturating_add(i as u64))
                .ok()
                .and_then(|i| config.get(i))
                .copied()
                .unwrap_or(0);
        }
    }

    fn write_config(&mut self, _offset: u64, _data: &[u8]) {}

    fn activate(&mut self, resources: DeviceResources) -> Result<(), DeviceError> {
        if resources.queues.len() != 2 {
            return Err(DeviceError::QueueCount {
                expected: 2,
                actual: resources.queues.len(),
            });
        }
        self.queues = resources.queues;
        self.mem = Some(resources.mem);
        self.interrupt = Some(resources.interrupt);
        tracing::info!(tag = %self.tag, "virtio-fs ready");
        Ok(())
    }

    fn notify(&mut self, queue_index: u16) -> Result<(), DeviceError> {
        let index = usize::from(queue_index);
        if index >= self.queues.len() {
            return Err(DeviceError::UnknownQueue(queue_index));
        }
        let mem = self.mem.clone().ok_or(DeviceError::NotActivated)?;
        let interrupt = self.interrupt.clone().ok_or(DeviceError::NotActivated)?;
        let mut queue = std::mem::replace(
            &mut self.queues[index],
            Queue::new(1).map_err(|e| DeviceError::Queue(e.to_string()))?,
        );
        let desc_table = queue.desc_table();
        let size = queue.size();
        let mut served = 0usize;
        let mut result = Ok(());
        while let Some(head) = queue
            .pop_descriptor_chain(Arc::clone(&mem))
            .map(|chain| chain.head_index())
        {
            let written = self.serve(&mem, desc_table, size, head);
            if let Err(e) = queue.add_used(mem.as_ref(), head, written) {
                result = Err(DeviceError::Queue(e.to_string()));
                break;
            }
            served += 1;
            if served >= CHAINS_PER_NOTIFY {
                break;
            }
        }
        if result.is_ok() && served > 0 {
            match queue.needs_notification(mem.as_ref()) {
                Ok(true) => result = interrupt.signal_used_queue(queue_index).map_err(Into::into),
                Ok(false) => {}
                Err(e) => result = Err(DeviceError::Queue(e.to_string())),
            }
        }
        self.queues[index] = queue;
        result
    }

    fn queue_positions(&self) -> Vec<virtio_core::QueuePosition> {
        self.queues
            .iter()
            .map(|q| virtio_core::QueuePosition {
                next_avail: q.next_avail(),
                next_used: q.next_used(),
            })
            .collect()
    }

    fn snapshot_refusal(&self) -> Option<String> {
        self.fuse.busy().then(|| {
            format!(
                "the shared folder '{}' is in use in the guest, and its open files cannot be \
                 saved",
                self.tag
            )
        })
    }

    fn reset(&mut self) {
        self.fuse.reset();
        self.queues.clear();
        self.mem = None;
        self.interrupt = None;
    }
}
