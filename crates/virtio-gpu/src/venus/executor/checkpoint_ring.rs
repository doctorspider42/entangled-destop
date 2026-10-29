//! Per-ring reply encoder and virtqueue roundtrip state. Capture between whole
//! sink calls under quiesce; restore the directory's exact generations first.
//! This record does not capture a worker, nested call in progress or GPU work.

use super::checkpoint::RecipeError;
use super::host::HostVulkan;
use super::{ExecutingSink, ReplyWindow};
use crate::venus::renderer::{BlobRef, ReplyBlobError};
use crate::venus::wire::Decoder;

impl<H: HostVulkan> ExecutingSink<H> {
    /// Fixed-size record without native handles or host addresses. The matching
    /// blob generations, page data, pump cursor and context must be saved too.
    #[must_use]
    pub fn checkpoint_ring(&self) -> [u8; 64] {
        let mut bytes = [0; 64];
        bytes[0..4].copy_from_slice(&1_u32.to_le_bytes());
        bytes[4..8].copy_from_slice(&self.ctx_id.to_le_bytes());
        bytes[8..16].copy_from_slice(&self.ring.to_le_bytes());
        if let Some(window) = self.window {
            bytes[16..20].copy_from_slice(&1_u32.to_le_bytes());
            bytes[20..24].copy_from_slice(&window.blob.resource_id.to_le_bytes());
            for (dst, word) in bytes[24..56].chunks_exact_mut(8).zip([
                window.blob.generation,
                window.offset,
                window.size,
                window.cursor,
            ]) {
                dst.copy_from_slice(&word.to_le_bytes());
            }
        }
        bytes[56..64].copy_from_slice(&self.virtqueue_seqno.current().to_le_bytes());
        bytes
    }

    /// Restore into a fresh sink while workers/guest remain stopped. Directory
    /// generations must match the saved manifest exactly: allocating new
    /// generations could turn a stale binding into a live, unrelated blob.
    /// Invalid records leave both the sink and roundtrip count unchanged.
    pub fn restore_ring_checkpoint(&mut self, bytes: &[u8]) -> Result<(), RecipeError> {
        let invalid = RecipeError::Invalid;
        if self.window.is_some() || self.blocked.is_some() || self.virtqueue_seqno.current() != 0 {
            return Err(invalid("ring sink is not fresh"));
        }
        if bytes.len() != 64 {
            return Err(invalid("ring sink record size"));
        }
        let mut decoder = Decoder::new(bytes);
        let version = decoder.u32()?;
        let ctx_id = decoder.u32()?;
        let ring = decoder.u64()?;
        if version != 1 || ctx_id != self.ctx_id || ring != self.ring {
            return Err(invalid("ring sink version or identity"));
        }
        let present = decoder.u32()?;
        let resource_id = decoder.u32()?;
        let generation = decoder.u64()?;
        let offset = decoder.u64()?;
        let size = decoder.u64()?;
        let cursor = decoder.u64()?;
        let seqno = decoder.u64()?;
        let window = match present {
            0 if [u64::from(resource_id), generation, offset, size, cursor] == [0; 5] => None,
            1 if cursor <= size && offset.checked_add(size).is_some() => {
                let blob = BlobRef {
                    resource_id,
                    generation,
                };
                match self.blobs.check_saved_reply(blob, offset, size) {
                    Ok(()) | Err(ReplyBlobError::Gone(_)) => {}
                    Err(_) => return Err(invalid("saved reply binding is inaccessible")),
                }
                // Keep a stale window stale. Converting it to None would also
                // change when a later reply-bearing Vulkan command is refused.
                Some(ReplyWindow {
                    blob,
                    offset,
                    size,
                    cursor,
                })
            }
            _ => return Err(invalid("ring sink reply geometry or presence")),
        };
        self.window = window;
        self.virtqueue_seqno.submit(seqno);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::super::fake::FakeVulkan;
    use super::super::VulkanContext;
    use super::*;
    use crate::venus::protocol::Command;
    use crate::venus::renderer::{BlobDirectory, RingEnv, VirtqueueSeqno};
    use std::sync::{Arc, Mutex};

    fn sink() -> ExecutingSink<FakeVulkan> {
        ExecutingSink::new(
            RingEnv {
                ctx_id: 1,
                ring: 17,
                blobs: BlobDirectory::default().for_context(1),
                virtqueue_seqno: Arc::new(VirtqueueSeqno::default()),
            },
            Arc::new(Mutex::new(VulkanContext::new(
                1,
                Arc::new(FakeVulkan::standard()),
            ))),
        )
    }

    #[test]
    fn reply_cursor_and_stale_incarnation_survive_without_rebinding() {
        let mut original = sink();
        original.window = Some(ReplyWindow {
            blob: BlobRef {
                resource_id: 8,
                generation: 29,
            },
            offset: 32,
            size: 1024,
            cursor: 400,
        });
        original.virtqueue_seqno.submit(u64::MAX - 1);
        let record = original.checkpoint_ring();
        let mut restored = sink();
        restored.restore_ring_checkpoint(&record).unwrap();
        assert_eq!(restored.window, original.window);
        assert_eq!(restored.virtqueue_seqno.current(), u64::MAX - 1);
        assert!(restored.virtqueue_seqno.reached(u64::MAX - 2));
        assert!(!restored.virtqueue_seqno.reached(u64::MAX));
        let mut command = super::super::harness::enumerate_instance_version();
        if let Command::EnumerateInstanceVersion(ref mut args) = command {
            args.p_api_version = Some(123);
        }
        assert!(matches!(
            restored.reply(&command),
            Err(super::super::SinkError::NoReplyWindow(_))
        ));
        assert!(restored.window.is_none());
    }

    #[test]
    fn malformed_ring_records_do_not_change_sink_or_roundtrip_state() {
        let original = sink();
        original.virtqueue_seqno.submit(400);
        let good = original.checkpoint_ring();
        let mut restored = sink();
        for at in [0, 4, 8, 16, 20, 24, 32, 40, 48] {
            let mut bad = good;
            bad[at] = 255;
            assert!(restored.restore_ring_checkpoint(&bad).is_err());
            assert!(restored.window.is_none());
            assert_eq!(restored.virtqueue_seqno.current(), 0);
        }
        assert!(restored.restore_ring_checkpoint(&good[..63]).is_err());
        let mut extra = good.to_vec();
        extra.push(0);
        assert!(restored.restore_ring_checkpoint(&extra).is_err());
        restored.restore_ring_checkpoint(&good).unwrap();
        assert!(restored.window.is_none());
        assert_eq!(restored.virtqueue_seqno.current(), 400);
        assert!(restored.restore_ring_checkpoint(&good).is_err());
    }

    #[test]
    fn reply_geometry_is_validated_before_restoring_the_roundtrip() {
        let mut original = sink();
        original.window = Some(ReplyWindow {
            blob: BlobRef {
                resource_id: 8,
                generation: 29,
            },
            offset: 32,
            size: 16,
            cursor: 4,
        });
        original.virtqueue_seqno.submit(99);
        for (offset, size, cursor) in [(32_u64, 16_u64, 17_u64), (u64::MAX, 1, 0)] {
            let mut bytes = original.checkpoint_ring();
            bytes[32..40].copy_from_slice(&offset.to_le_bytes());
            bytes[40..48].copy_from_slice(&size.to_le_bytes());
            bytes[48..56].copy_from_slice(&cursor.to_le_bytes());
            let mut restored = sink();
            assert!(restored.restore_ring_checkpoint(&bytes).is_err());
            assert_eq!(restored.virtqueue_seqno.current(), 0);
            assert!(restored.window.is_none());
        }
    }
}
