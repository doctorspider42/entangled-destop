//! The virtio-9p transport device (VirtIO spec 1.2, section 5.9): one request
//! queue, and a config space that names the share by its mount tag.
//!
//! A request is one descriptor chain: the T-message in device-readable
//! buffers, room for the R-message in device-writable ones. Linux's
//! zero-copy path splits a large read or write's payload into separate
//! buffers after the header; gathering the readable half in order and
//! scattering the reply over the writable half in order serves both shapes
//! with one code path.
//!
//! Untrusted-guest rules: the chain walk is `virtio_core::chain`'s (bounded,
//! index-checked), the gathered message is capped at the negotiable maximum
//! plus a header before anything is read, every guest access is a checked
//! `vm-memory` call, and a malformed request is answered with `Rlerror` or,
//! when there is not even room for that, dropped with a used length of 0.
//! The device never fails on guest input.

use std::sync::Arc;

use virtio_core::chain::{self, Segment};
use virtio_core::device::{DeviceError, DeviceResources, DeviceType, VirtioDevice};
use virtio_core::interrupt::Interrupt;
use virtio_core::{GuestMem, MAX_QUEUE_SIZE, VIRTIO_F_VERSION_1};
use virtio_queue::{Queue, QueueT};
use vm_memory::{Bytes, GuestAddress};

use crate::server::{errno, Server, MAX_MSIZE};
use crate::wire;

/// `VIRTIO_9P_MOUNT_TAG`: the config space carries the tag.
pub const VIRTIO_9P_MOUNT_TAG: u64 = 1 << 0;
/// The longest mount tag offered (QEMU's limit, and plenty for a name).
pub const MAX_TAG_LEN: usize = 31;
/// Bytes gathered for one request: the largest message the server ever
/// agrees to, plus slack for a header the guest got wrong.
pub const MAX_REQUEST_BYTES: usize = MAX_MSIZE as usize + 4096;
/// Chains served per kick.
pub const CHAINS_PER_NOTIFY: usize = 4 * MAX_QUEUE_SIZE as usize;

const REQUEST_QUEUE: u16 = 0;
static QUEUE_MAX_SIZES: [u16; 1] = [MAX_QUEUE_SIZE];

/// Is `tag` a mount tag the guest can type: ASCII letters, digits, `-`, `_`
/// and `.`, at most [`MAX_TAG_LEN`] bytes.
pub fn valid_tag(tag: &str) -> bool {
    !tag.is_empty()
        && tag.len() <= MAX_TAG_LEN
        && tag
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
}

/// One shared host directory as a virtio-9p device.
pub struct NinePDevice {
    tag: String,
    server: Server,
    acked_features: u64,
    mem: Option<Arc<GuestMem>>,
    queue: Option<Queue>,
    interrupt: Option<Arc<dyn Interrupt>>,
}

impl std::fmt::Debug for NinePDevice {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NinePDevice")
            .field("tag", &self.tag)
            .field("fids", &self.server.fid_count())
            .finish_non_exhaustive()
    }
}

impl NinePDevice {
    /// A device sharing `server`'s directory under `tag` (checked with
    /// [`valid_tag`] by the caller; an invalid one is refused here too).
    pub fn new(tag: &str, server: Server) -> Result<Self, String> {
        if !valid_tag(tag) {
            return Err(format!(
                "mount tag {tag:?} must be 1..={MAX_TAG_LEN} letters, digits, '-', '_' or '.'"
            ));
        }
        Ok(Self {
            tag: tag.to_string(),
            server,
            acked_features: 0,
            mem: None,
            queue: None,
            interrupt: None,
        })
    }

    pub fn tag(&self) -> &str {
        &self.tag
    }

    fn config_space(&self) -> Vec<u8> {
        let mut config = Vec::with_capacity(2 + self.tag.len());
        config.extend_from_slice(&(self.tag.len() as u16).to_le_bytes());
        config.extend_from_slice(self.tag.as_bytes());
        config
    }

    fn drain(
        &mut self,
        queue: &mut Queue,
        mem: &Arc<GuestMem>,
        interrupt: &dyn Interrupt,
    ) -> Result<(), DeviceError> {
        let desc_table = queue.desc_table();
        let queue_size = queue.size();
        let mut served = 0usize;
        while let Some(head) = queue
            .pop_descriptor_chain(Arc::clone(mem))
            .map(|chain| chain.head_index())
        {
            let written = self.handle_chain(mem, desc_table, queue_size, head);
            queue
                .add_used(mem.as_ref(), head, written)
                .map_err(|e| DeviceError::Queue(e.to_string()))?;
            served += 1;
            if served >= CHAINS_PER_NOTIFY {
                tracing::warn!(tag = %self.tag, served, "virtio-9p kick budget exhausted");
                break;
            }
        }
        if served > 0
            && queue
                .needs_notification(mem.as_ref())
                .map_err(|e| DeviceError::Queue(e.to_string()))?
        {
            interrupt.signal_used_queue(REQUEST_QUEUE)?;
        }
        Ok(())
    }

    /// Serves one chain; returns the bytes written for the used ring.
    fn handle_chain(&mut self, mem: &GuestMem, desc_table: u64, queue_size: u16, head: u16) -> u32 {
        let segments = match chain::walk(mem, desc_table, queue_size, head) {
            Ok(segments) => segments,
            Err(error) => {
                tracing::warn!(tag = %self.tag, head, %error, "dropping malformed virtio-9p chain");
                return 0;
            }
        };
        let (readable, writable) = match chain::split_rw(&segments) {
            Ok(split) => split,
            Err(error) => {
                tracing::warn!(tag = %self.tag, head, %error, "dropping virtio-9p chain");
                return 0;
            }
        };
        let room: u64 = writable.iter().map(|s| u64::from(s.len)).sum();
        let reply = match gather(mem, readable) {
            Ok(request) => self.server.handle(&request),
            Err(code) => wire::lerror(tag_of(mem, readable), code),
        };
        // A reply that does not fit is replaced by an error that does, so
        // the guest's request still completes.
        let reply = if reply.len() as u64 <= room {
            reply
        } else {
            let tag = u16::from_le_bytes([reply[5], reply[6]]);
            wire::lerror(tag, errno::EIO)
        };
        if reply.len() as u64 > room {
            return 0;
        }
        scatter(mem, writable, &reply)
    }
}

/// The request's tag, best effort, for an error reply to a request too broken
/// to gather.
fn tag_of(mem: &GuestMem, readable: &[Segment]) -> u16 {
    let mut header = [0u8; wire::HEADER];
    match readable.first() {
        Some(seg) if seg.len as usize >= wire::HEADER => {
            if mem.read_slice(&mut header, GuestAddress(seg.addr)).is_ok() {
                return u16::from_le_bytes([header[5], header[6]]);
            }
            0xffff
        }
        _ => 0xffff,
    }
}

/// The device-readable half of a chain, in order, capped at
/// [`MAX_REQUEST_BYTES`].
fn gather(mem: &GuestMem, readable: &[Segment]) -> Result<Vec<u8>, u32> {
    let total: u64 = readable.iter().map(|s| u64::from(s.len)).sum();
    if total < wire::HEADER as u64 {
        return Err(errno::EPROTO);
    }
    if total > MAX_REQUEST_BYTES as u64 {
        return Err(errno::EINVAL);
    }
    let mut out = vec![0u8; total as usize];
    let mut at = 0usize;
    for seg in readable {
        let len = seg.len as usize;
        mem.read_slice(&mut out[at..at + len], GuestAddress(seg.addr))
            .map_err(|_| errno::EIO)?;
        at += len;
    }
    Ok(out)
}

/// Writes `reply` over the device-writable half in order; returns the bytes
/// written (short only if a buffer turned out not to be guest memory).
fn scatter(mem: &GuestMem, writable: &[Segment], reply: &[u8]) -> u32 {
    let mut done = 0usize;
    for seg in writable {
        if done == reply.len() {
            break;
        }
        let n = (seg.len as usize).min(reply.len() - done);
        if mem
            .write_slice(&reply[done..done + n], GuestAddress(seg.addr))
            .is_err()
        {
            break;
        }
        done += n;
    }
    done as u32
}

impl VirtioDevice for NinePDevice {
    fn device_type(&self) -> DeviceType {
        DeviceType::NineP
    }

    fn queue_max_sizes(&self) -> &[u16] {
        &QUEUE_MAX_SIZES
    }

    fn device_features(&self) -> u64 {
        VIRTIO_F_VERSION_1 | VIRTIO_9P_MOUNT_TAG
    }

    fn ack_features(&mut self, negotiated: u64) -> bool {
        if negotiated & VIRTIO_F_VERSION_1 == 0 || negotiated & !self.device_features() != 0 {
            return false;
        }
        self.acked_features = negotiated;
        true
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

    fn write_config(&mut self, offset: u64, data: &[u8]) {
        tracing::warn!(tag = %self.tag, offset, len = data.len(), "ignoring a write to the virtio-9p config space");
    }

    fn activate(&mut self, resources: DeviceResources) -> Result<(), DeviceError> {
        let mut queues = resources.queues;
        if queues.len() != 1 {
            return Err(DeviceError::QueueCount {
                expected: 1,
                actual: queues.len(),
            });
        }
        self.queue = queues.pop();
        self.mem = Some(resources.mem);
        self.interrupt = Some(resources.interrupt);
        tracing::info!(tag = %self.tag, "virtio-9p ready");
        Ok(())
    }

    fn notify(&mut self, queue_index: u16) -> Result<(), DeviceError> {
        if queue_index != REQUEST_QUEUE {
            return Err(DeviceError::UnknownQueue(queue_index));
        }
        let mem = self.mem.clone().ok_or(DeviceError::NotActivated)?;
        let interrupt = self.interrupt.clone().ok_or(DeviceError::NotActivated)?;
        let mut queue = self.queue.take().ok_or(DeviceError::NotActivated)?;
        let result = self.drain(&mut queue, &mem, interrupt.as_ref());
        self.queue = Some(queue);
        result
    }

    /// Like virtio-blk, every request is served inside `notify`, so a quiesced
    /// device holds nothing in flight; only the position needs carrying.
    fn queue_positions(&self) -> Vec<virtio_core::QueuePosition> {
        self.queue
            .as_ref()
            .map(|q| {
                vec![virtio_core::QueuePosition {
                    next_avail: q.next_avail(),
                    next_used: q.next_used(),
                }]
            })
            .unwrap_or_default()
    }

    /// The guest's fids name open host files, which no snapshot can carry: a
    /// mounted share would come back as a mount whose every file handle is
    /// stale. Refused while anything is attached.
    fn snapshot_refusal(&self) -> Option<String> {
        (self.server.fid_count() > 0).then(|| {
            format!(
                "the shared folder '{}' is mounted in the guest, and its open files cannot be \
                 saved — unmount it first",
                self.tag
            )
        })
    }

    fn reset(&mut self) {
        self.server.reset();
        self.queue = None;
        self.mem = None;
        self.interrupt = None;
        self.acked_features = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tags_are_short_and_typeable() {
        assert!(valid_tag("home"));
        assert!(valid_tag("my-share_2.0"));
        assert!(!valid_tag(""));
        assert!(!valid_tag("has space"));
        assert!(!valid_tag(&"x".repeat(MAX_TAG_LEN + 1)));
    }
}
