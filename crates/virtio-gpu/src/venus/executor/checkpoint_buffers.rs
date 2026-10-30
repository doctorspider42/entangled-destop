//! GPU readback/upload of device-local buffers. Staging is bounded to one MiB,
//! never mapped into the guest, and retained until its completion fence signals.
//! The caller parks guest/ring writers for the entire checkpoint operation.

use std::collections::HashSet;
use std::io::Write;
use std::sync::Arc;
use std::time::{Duration, Instant};

use super::checkpoint::RecipeError;
use super::checkpoint_sync::remaining;
use super::host::{HostVulkan, MemoryRequest, RawHandle, ResourceMemory};
use super::objects::{Binding, Kind, MemoryHandle};
use super::policy::{MEMORY_PROPERTY_DEVICE_LOCAL, MEMORY_PROPERTY_HOST_VISIBLE};
use super::VulkanContext;
use crate::venus::protocol::*;
use crate::venus::shmem::PrivatePages;

const VERSION: u32 = 1;
pub(super) const CHUNK_BYTES: usize = 1 << 20;
const MAX_BUFFERS: usize = 1 << 16;
const ALL_COMMANDS: u32 = 0x1_0000;
const TRANSFER: u32 = 0x1000;
const HOST: u32 = 0x4000;
const MEMORY_READ_WRITE: u32 = 0x1_8000;
const TRANSFER_READ: u32 = 0x800;
const TRANSFER_WRITE: u32 = 0x1000;
const HOST_READ: u32 = 0x2000;
const HOST_WRITE: u32 = 0x4000;

#[cfg(test)]
mod tests;

pub(super) struct TransferRequest<'a> {
    pub device: u64,
    pub queue: u64,
    pub len: usize,
    pub upload: Option<&'a [u8]>,
    pub deadline: Instant,
}

fn failed(reason: impl Into<String>) -> RecipeError {
    RecipeError::Replay(reason.into())
}

struct Target<H: HostVulkan> {
    id: u64,
    device: u64,
    host: H::Buffer,
    size: u64,
    binding: Binding,
    queue: u64,
}

/// Host resources owned by the queue clock after a transfer is submitted.
/// A failed save can return to Running while these remain on the GPU; ordinary
/// buffer destruction then observes the private submission's queue serial too.
pub struct CheckpointScratch<H: HostVulkan> {
    pool: Option<H::CommandPool>,
    buffer: Option<H::Buffer>,
    memory: Option<H::Memory>,
    pages: Option<PrivatePages>,
    pub(super) semaphores: Vec<u64>,
    // Only fences not transferred to a queue clock remain here.
    pub(super) fences: Vec<u64>,
}

impl<H: HostVulkan> CheckpointScratch<H> {
    pub(super) fn destroy(mut self, host: &H, device: &H::Device) {
        for semaphore in self.semaphores.drain(..) {
            host.destroy_object(device, Kind::Semaphore, semaphore);
        }
        for fence in self.fences.drain(..) {
            host.destroy_object(device, Kind::Fence, fence);
        }
        if let Some(pool) = self.pool.take() {
            host.destroy_command_pool(device, pool);
        }
        if let Some(buffer) = self.buffer.take() {
            host.destroy_buffer(device, buffer);
        }
        if let Some(memory) = self.memory.take() {
            host.free_memory(device, memory);
        }
        // The imported pages outlive vkFreeMemory, including on GPU loss.
        self.pages = None;
    }
}

fn barrier(
    cb: VkCommandBuffer,
    buffer: u64,
    offset: u64,
    size: u64,
    before: (u32, u32),
    after: (u32, u32),
) -> Command<'static> {
    Command::CmdPipelineBarrier(CmdPipelineBarrierArgs {
        command_buffer: cb,
        src_stage_mask: before.0,
        dst_stage_mask: after.0,
        buffer_memory_barrier_count: 1,
        p_buffer_memory_barriers: Some(vec![VkBufferMemoryBarrier {
            p_next: Vec::new(),
            src_access_mask: before.1,
            dst_access_mask: after.1,
            src_queue_family_index: u32::MAX,
            dst_queue_family_index: u32::MAX,
            buffer: VkBuffer(buffer),
            offset,
            size,
        }]),
        ..Default::default()
    })
}

impl<H: HostVulkan> VulkanContext<H> {
    /// Exact framed component length without reading GPU memory.
    pub fn checkpoint_buffers_length(&self) -> Result<u64, RecipeError> {
        self.checkpoint_buffer_targets()?
            .iter()
            .try_fold(8u64, |length, target| {
                target
                    .size
                    .checked_next_multiple_of(4)
                    .and_then(|size| size.checked_add(40))
                    .and_then(|size| length.checked_add(size))
                    .ok_or(RecipeError::Budget)
            })
    }
    fn checkpoint_buffer_targets(&self) -> Result<Vec<Target<H>>, RecipeError> {
        if self.device_addresses_exposed {
            return Err(failed(
                "buffer device addresses exposed to the guest are not captured",
            ));
        }
        if self.buffer_ownership_transfers {
            return Err(failed(
                "buffer queue-family ownership transfers are not captured",
            ));
        }
        let aliases = self.checkpoint_binding_aliases()?;
        let mut targets = Vec::new();
        for (id, buffer) in self.objects.checkpoint_buffers() {
            let Some(binding) = buffer.bound else {
                continue;
            };
            let Ok(memory) = self.objects.memory(buffer.device, binding.memory) else {
                // FreeMemory may leave an unusable buffer handle alive.
                continue;
            };
            if !buffer
                .bound_identity
                .as_ref()
                .is_some_and(|identity| Arc::ptr_eq(identity, &memory.identity))
            {
                // Reusing a guest memory id does not rebind that old buffer.
                continue;
            }
            if memory.pages.is_some() {
                continue;
            }
            if memory.property_flags & MEMORY_PROPERTY_DEVICE_LOCAL == 0
                || memory.property_flags & MEMORY_PROPERTY_HOST_VISIBLE != 0
            {
                return Err(failed(
                    "buffer memory is neither imported host pages nor private device-local memory",
                ));
            }
            if buffer.flags != 0 || buffer.usage & 3 != 3 {
                return Err(failed(
                    "device-local buffer needs unprotected transfer usage",
                ));
            }
            if memory.handle != MemoryHandle::None
                || memory.exported
                || buffer.handle
                || buffer.external
            {
                return Err(failed(
                    "shared/exported device-local buffer payload needs cross-context checkpointing",
                ));
            }
            if aliases.buffer(id) {
                return Err(failed(
                    "buffer/image memory aliases need image layout and active-alias checkpointing",
                ));
            }
            let owner = self.checkpoint_buffer_family(buffer.device, id)?;
            let queue = match owner {
                Some(family) => self.checkpoint_family_queue(buffer.device, Some(family))?,
                None => self.checkpoint_copy_queue(buffer.device)?,
            };
            let family = self
                .objects
                .queue(queue)
                .map_err(|e| failed(e.to_string()))?
                .family;
            if !buffer.sharing.allows(family) {
                return Err(failed(
                    "buffer checkpoint queue is outside its concurrent sharing families",
                ));
            }
            targets.push(Target {
                id,
                device: buffer.device,
                host: buffer.host,
                size: buffer.size,
                binding,
                queue,
            });
            if targets.len() > MAX_BUFFERS {
                return Err(RecipeError::Budget);
            }
        }
        targets.sort_unstable_by_key(|t| t.id);
        Ok(targets)
    }

    /// Stream actual device-local buffer bytes through bounded private staging.
    /// Only the buffer component; images, sharing and rings remain separate.
    /// Failure does not reset/destroy the live context or its source buffers.
    pub fn save_device_buffers(
        &mut self,
        writer: &mut impl Write,
        timeout: Duration,
    ) -> Result<(), RecipeError> {
        let targets = self.checkpoint_buffer_targets()?;
        let deadline = Instant::now()
            .checked_add(timeout)
            .ok_or_else(|| failed("invalid GPU checkpoint timeout"))?;
        self.checkpoint_wait_idle(deadline)?;
        writer.write_all(&VERSION.to_le_bytes())?;
        writer.write_all(&(targets.len() as u32).to_le_bytes())?;
        let mut bytes = Vec::new();
        for target in &targets {
            for value in [
                target.device,
                target.id,
                target.size,
                target.binding.memory,
                target.binding.offset,
            ] {
                writer.write_all(&value.to_le_bytes())?;
            }
            let mut offset = 0;
            while offset < target.size {
                let len = (target.size - offset).min(CHUNK_BYTES as u64) as usize;
                let pages = self.checkpoint_buffer_chunk(target, offset, len, None, deadline)?;
                // The GPU fence completed before this plain CPU read.
                pages.read_rows(0, len as u64, len, 1, &mut bytes)?;
                writer.write_all(&bytes)?;
                offset += len as u64;
            }
            super::checkpoint::write_padding(writer, target.size)?;
        }
        Ok(())
    }

    /// Validate every allocation/buffer identity and all file lengths before
    /// the first upload. The reconstructed guest/rings must remain stopped.
    /// On a host failure the partial reconstructed context is destroyed/parked.
    pub fn restore_device_buffers(
        &mut self,
        bytes: &[u8],
        timeout: Duration,
    ) -> Result<(), RecipeError> {
        self.restore_device_buffers_reader(&mut std::io::Cursor::new(bytes), timeout)
    }

    /// Restore a standalone/section-bounded component with at most one staging
    /// chunk of CPU storage. All framing and bindings validate before GPU work.
    pub fn restore_device_buffers_reader(
        &mut self,
        reader: &mut (impl std::io::Read + std::io::Seek),
        timeout: Duration,
    ) -> Result<(), RecipeError> {
        let targets = self.checkpoint_buffer_targets()?;
        let mut input = super::checkpoint_io::ComponentReader::new(reader)?;
        let version = input.u32()?;
        if version != VERSION {
            return Err(RecipeError::Version(version));
        }
        let count = input.u32()? as usize;
        if count != targets.len() {
            return Err(RecipeError::Invalid(
                "streamed device-local buffer count differs",
            ));
        }
        let mut records = Vec::new();
        records
            .try_reserve_exact(count)
            .map_err(|_| RecipeError::Budget)?;
        let mut seen = HashSet::new();
        for _ in 0..count {
            let device = input.u64()?;
            let id = input.u64()?;
            let size = input.u64()?;
            let memory = input.u64()?;
            let offset = input.u64()?;
            let target = targets
                .binary_search_by_key(&id, |target| target.id)
                .ok()
                .and_then(|index| targets.get(index))
                .ok_or(RecipeError::Invalid("unknown streamed device-local buffer"))?;
            if !seen.insert(id)
                || target.device != device
                || target.size != size
                || target.binding.memory != memory
                || target.binding.offset != offset
            {
                return Err(RecipeError::Invalid(
                    "streamed buffer identity or binding differs",
                ));
            }
            records.push((target, input.blob_offset(size)?));
        }
        input.finish_metadata()?;
        let deadline = Instant::now()
            .checked_add(timeout)
            .ok_or_else(|| failed("invalid GPU stream restore timeout"))?;
        self.checkpoint_wait_idle(deadline)?;
        let mut bytes = Vec::new();
        if count != 0 {
            bytes
                .try_reserve_exact(CHUNK_BYTES)
                .map_err(|_| RecipeError::Budget)?;
            bytes.resize(CHUNK_BYTES, 0);
        }
        let result = (|| {
            for (target, start) in records {
                let mut offset = 0;
                while offset < target.size {
                    let len = (target.size - offset).min(CHUNK_BYTES as u64) as usize;
                    input.read_at(
                        start.checked_add(offset).ok_or(RecipeError::Budget)?,
                        &mut bytes[..len],
                    )?;
                    self.checkpoint_buffer_chunk(
                        target,
                        offset,
                        len,
                        Some(&bytes[..len]),
                        deadline,
                    )?;
                    offset += len as u64;
                }
                let family = self
                    .objects
                    .queue(target.queue)
                    .map_err(|e| failed(e.to_string()))?
                    .family;
                let buffer = self
                    .objects
                    .buffer_mut(target.device, target.id)
                    .map_err(|e| failed(e.to_string()))?;
                if matches!(buffer.sharing, super::objects::Sharing::Exclusive) {
                    buffer.checkpoint_owner = Some((family, true));
                }
            }
            input.finish()
        })();
        if result.is_err() {
            self.destroy_all();
        }
        result
    }

    fn checkpoint_buffer_chunk(
        &mut self,
        target: &Target<H>,
        offset: u64,
        len: usize,
        upload: Option<&[u8]>,
        deadline: Instant,
    ) -> Result<PrivatePages, RecipeError> {
        remaining(deadline)?;
        if len == 0
            || len > CHUNK_BYTES
            || offset
                .checked_add(len as u64)
                .is_none_or(|end| end > target.size)
            || upload.is_some_and(|data| data.len() != len)
        {
            return Err(RecipeError::Invalid("invalid checkpoint buffer chunk"));
        }
        self.checkpoint_transfer(
            TransferRequest {
                device: target.device,
                queue: target.queue,
                len,
                upload,
                deadline,
            },
            |context, cb, staging| {
                let device = target.device;
                let access = if upload.is_some() {
                    TRANSFER_WRITE
                } else {
                    TRANSFER_READ
                };
                context.checkpoint_call(
                    device,
                    barrier(
                        cb,
                        target.host.raw(),
                        offset,
                        len as u64,
                        (ALL_COMMANDS, MEMORY_READ_WRITE),
                        (TRANSFER, access),
                    ),
                )?;
                let (src, dst, src_offset, dst_offset) = if upload.is_some() {
                    (staging.raw(), target.host.raw(), 0, offset)
                } else {
                    (target.host.raw(), staging.raw(), offset, 0)
                };
                context.checkpoint_call(
                    device,
                    Command::CmdCopyBuffer(CmdCopyBufferArgs {
                        command_buffer: cb,
                        src_buffer: VkBuffer(src),
                        dst_buffer: VkBuffer(dst),
                        region_count: 1,
                        p_regions: Some(vec![VkBufferCopy {
                            src_offset,
                            dst_offset,
                            size: len as u64,
                        }]),
                    }),
                )?;
                context.checkpoint_call(
                    device,
                    barrier(
                        cb,
                        target.host.raw(),
                        offset,
                        len as u64,
                        (TRANSFER, access),
                        (ALL_COMMANDS, MEMORY_READ_WRITE),
                    ),
                )?;
                Ok(())
            },
        )
    }

    pub(super) fn checkpoint_transfer(
        &mut self,
        request: TransferRequest<'_>,
        record: impl FnOnce(&Self, VkCommandBuffer, H::Buffer) -> Result<(), RecipeError>,
    ) -> Result<PrivatePages, RecipeError> {
        let TransferRequest {
            device,
            queue: queue_id,
            len,
            upload,
            deadline,
        } = request;
        remaining(deadline)?;
        if len == 0 || len > CHUNK_BYTES || upload.is_some_and(|data| data.len() != len) {
            return Err(RecipeError::Invalid("invalid checkpoint staging chunk"));
        }
        let (owner, guest) = self
            .objects
            .device_and_guest(device)
            .map_err(|e| failed(e.to_string()))?;
        let owner = Arc::clone(&owner.host);
        let (alignment, host_types) = (guest.import_alignment, guest.coherent_import_host_types());
        let queue = self
            .objects
            .queue(queue_id)
            .map_err(|e| failed(e.to_string()))?;
        let family = queue.family;
        let mut scratch = CheckpointScratch::<H> {
            pool: None,
            buffer: None,
            memory: None,
            pages: None,
            semaphores: Vec::new(),
            fences: Vec::new(),
        };
        let prepared = (|| {
            let buffer = self
                .host
                .create_buffer(
                    &owner,
                    &VkBufferCreateInfo {
                        size: len as u64,
                        usage: 3,
                        ..Default::default()
                    },
                    ResourceMemory::HostPages,
                )
                .map_err(|r| failed(format!("checkpoint staging buffer: {r}")))?;
            scratch.buffer = Some(buffer);
            let mut requirements = VkMemoryRequirements2::default();
            self.host
                .buffer_memory_requirements(&owner, buffer, &mut requirements);
            let req = requirements.memory_requirements;
            let align = alignment
                .max(req.alignment)
                .checked_next_power_of_two()
                .ok_or_else(|| failed("invalid staging alignment"))?;
            let pages = PrivatePages::for_memory(req.size.max(len as u64), align, &self.budget)?;
            scratch.pages = Some(pages.clone());
            let types = self
                .host
                .host_pointer_types(&owner, pages.import_pages())
                .map_err(|r| failed(format!("checkpoint staging host types: {r}")))?
                & req.memory_type_bits
                & host_types;
            if types == 0 {
                return Err(failed("no coherent imported staging memory type"));
            }
            let memory = self
                .host
                .allocate_memory(
                    &owner,
                    &MemoryRequest {
                        size: pages.mapped_len(),
                        type_index: types.trailing_zeros(),
                        import: Some(Arc::clone(pages.import_pages())),
                        flags: None,
                        dedicated: None,
                        export_handle: false,
                        import_handle: None,
                    },
                )
                .map_err(|r| failed(format!("checkpoint staging allocation: {r}")))?;
            scratch.memory = Some(memory);
            let memory = scratch
                .memory
                .as_ref()
                .ok_or_else(|| failed("missing staging memory"))?;
            let ret = self.host.bind_buffer_memory(&owner, &[(buffer, memory, 0)]);
            if ret != VK_SUCCESS {
                return Err(failed(format!("checkpoint staging bind: {ret}")));
            }
            if let Some(data) = upload {
                pages.import_pages().write_bytes(0, data)?;
            }
            let pool = self
                .host
                .create_command_pool(
                    &owner,
                    &VkCommandPoolCreateInfo {
                        flags: 0,
                        queue_family_index: family,
                    },
                )
                .map_err(|r| failed(format!("checkpoint transfer command pool: {r}")))?;
            scratch.pool = Some(pool);
            let command = self.checkpoint_call(
                device,
                Command::AllocateCommandBuffers(AllocateCommandBuffersArgs {
                    device: VkDevice(device),
                    p_allocate_info: Some(VkCommandBufferAllocateInfo {
                        command_pool: VkCommandPool(pool.raw()),
                        level: 0,
                        command_buffer_count: 1,
                    }),
                    p_command_buffers: Some(vec![VkCommandBuffer(0)]),
                    ret: VK_ERROR_UNKNOWN,
                }),
            )?;
            let Command::AllocateCommandBuffers(args) = command else {
                return Err(failed("missing transfer command buffer"));
            };
            let cb = args
                .p_command_buffers
                .as_ref()
                .and_then(|v| v.first())
                .copied()
                .filter(|cb| cb.0 != 0)
                .ok_or_else(|| failed("missing transfer command buffer"))?;
            self.checkpoint_call(
                device,
                Command::BeginCommandBuffer(BeginCommandBufferArgs {
                    command_buffer: cb,
                    p_begin_info: Some(VkCommandBufferBeginInfo {
                        flags: 1,
                        ..Default::default()
                    }),
                    ret: VK_ERROR_UNKNOWN,
                }),
            )?;
            if upload.is_some() {
                self.checkpoint_call(
                    device,
                    barrier(
                        cb,
                        buffer.raw(),
                        0,
                        len as u64,
                        (HOST, HOST_WRITE),
                        (TRANSFER, TRANSFER_READ),
                    ),
                )?;
            }
            record(self, cb, buffer)?;
            if upload.is_none() {
                self.checkpoint_call(
                    device,
                    barrier(
                        cb,
                        buffer.raw(),
                        0,
                        len as u64,
                        (TRANSFER, TRANSFER_WRITE),
                        (HOST, HOST_READ),
                    ),
                )?;
            }
            self.checkpoint_call(
                device,
                Command::EndCommandBuffer(EndCommandBufferArgs {
                    command_buffer: cb,
                    ret: VK_ERROR_UNKNOWN,
                }),
            )?;
            let bridge = self.checkpoint_prepare_copy(device, queue_id, &mut scratch)?;
            Ok((cb, bridge, pages))
        })();
        let (cb, bridge, pages) = match prepared {
            Ok(result) => result,
            Err(error) => {
                scratch.destroy(&self.host, &owner);
                return Err(error);
            }
        };
        let submitted = self.checkpoint_submit_copy(device, queue_id, cb, bridge, &mut scratch);
        self.objects.retain_checkpoint_scratch(device, scratch);
        if submitted.is_err() {
            self.poll_clocks(device);
        }
        let fences = submitted?;
        let timeout = u64::try_from(remaining(deadline)?.as_nanos()).unwrap_or(u64::MAX);
        self.checkpoint_call(
            device,
            Command::WaitForFences(WaitForFencesArgs {
                device: VkDevice(device),
                fence_count: fences.len() as u32,
                p_fences: Some(fences),
                wait_all: 1,
                timeout,
                ret: VK_ERROR_UNKNOWN,
            }),
        )?;
        self.poll_clocks(device);
        Ok(pages)
    }
}
