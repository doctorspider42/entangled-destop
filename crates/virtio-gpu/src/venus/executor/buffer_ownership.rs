//! Conservative executed buffer-family proofs. Direct accesses and opaque
//! command families bound where a buffer's defined contents could originate.
//! Unclassified or mixed ownership refuses cross-family checkpoint copies.
use super::checkpoint::RecipeError;
use super::host::HostVulkan;
use super::limits::Class;
use super::objects::{Kind, Objects, Sharing};
use super::VulkanContext;
use crate::venus::protocol::*;
use crate::venus::shmem::Charge;
use crate::venus::wire::{Decoder, Encoder};
use std::collections::{BTreeSet, HashMap};
use std::sync::Arc;

const MAX_USES: usize = 1 << 16;
#[derive(Clone)]
pub(super) struct Use {
    device: u64,
    id: u64,
    identity: Arc<()>,
    whole: bool,
}
struct Recording {
    uses: Vec<Use>,
    opaque: bool,
    charge: Charge,
}
pub(super) enum Note {
    None,
    Clear(u64),
    ClearPool(u64),
    Prune,
    Append {
        cb: u64,
        uses: Vec<Use>,
        opaque: bool,
        _charge: Charge,
    },
}
#[derive(Default)]
pub(super) struct BufferTracking {
    pub error: Option<String>,
    recordings: HashMap<u64, Recording>,
    opaque: BTreeSet<(u64, u32)>,
}
impl BufferTracking {
    pub fn prepare<H: HostVulkan>(
        &self,
        command: &Command<'_>,
        objects: &Objects<H>,
    ) -> Result<Note, String> {
        if self.error.is_some() {
            return Ok(Note::None);
        }
        match command {
            Command::BeginCommandBuffer(a) => return Ok(Note::Clear(a.command_buffer.0)),
            Command::ResetCommandBuffer(a) => return Ok(Note::Clear(a.command_buffer.0)),
            Command::ResetCommandPool(a) => {
                // The finish step removes every child recording for this pool.
                return Ok(Note::ClearPool(a.command_pool.0));
            }
            Command::DestroyCommandPool(_)
            | Command::FreeCommandBuffers(_)
            | Command::DestroyDevice(_) => return Ok(Note::Prune),
            _ => {}
        }
        if !command.name().starts_with("vkCmd") {
            return Ok(Note::None);
        }
        let Some((Kind::CommandBuffer, cb)) = super::generated::dispatchable(command) else {
            return Err("missing buffer tracking command buffer".into());
        };
        let device = objects
            .raw_any(Kind::CommandBuffer, cb)
            .map_err(|e| e.to_string())?
            .device;
        let mut uses = Vec::new();
        let mut push = |id: u64, offset: u64, size: u64| -> Result<(), String> {
            let buffer = objects.buffer(device, id).map_err(|e| e.to_string())?;
            if uses.len() >= MAX_USES {
                return Err("too many recorded buffer uses".into());
            }
            uses.push(Use {
                device,
                id,
                identity: buffer.identity.clone(),
                whole: offset == 0 && (size == u64::MAX || size >= buffer.size),
            });
            Ok(())
        };
        let opaque = match command {
            Command::CmdFillBuffer(a) => {
                push(a.dst_buffer.0, a.dst_offset, a.size)?;
                false
            }
            Command::CmdUpdateBuffer(a) => {
                push(a.dst_buffer.0, a.dst_offset, a.data_size)?;
                false
            }
            Command::CmdCopyBuffer(a) => {
                for region in a.p_regions.iter().flatten() {
                    push(a.src_buffer.0, region.src_offset, region.size)?;
                    push(a.dst_buffer.0, region.dst_offset, region.size)?;
                }
                false
            }
            Command::CmdCopyBuffer2(a) => {
                if let Some(info) = &a.p_copy_buffer_info {
                    for region in info.p_regions.iter().flatten() {
                        push(info.src_buffer.0, region.src_offset, region.size)?;
                        push(info.dst_buffer.0, region.dst_offset, region.size)?;
                    }
                }
                false
            }
            Command::CmdPipelineBarrier(a) => {
                for barrier in a.p_buffer_memory_barriers.iter().flatten() {
                    push(barrier.buffer.0, barrier.offset, barrier.size)?;
                }
                false
            }
            Command::CmdWaitEvents(a) => {
                for barrier in a.p_buffer_memory_barriers.iter().flatten() {
                    push(barrier.buffer.0, barrier.offset, barrier.size)?;
                }
                false
            }
            Command::CmdPipelineBarrier2(a) => {
                for barrier in a
                    .p_dependency_info
                    .iter()
                    .flat_map(|info| info.p_buffer_memory_barriers.iter().flatten())
                {
                    push(barrier.buffer.0, barrier.offset, barrier.size)?;
                }
                false
            }
            Command::CmdWaitEvents2(a) => {
                for barrier in a
                    .p_dependency_infos
                    .iter()
                    .flatten()
                    .flat_map(|info| info.p_buffer_memory_barriers.iter().flatten())
                {
                    push(barrier.buffer.0, barrier.offset, barrier.size)?;
                }
                false
            }
            Command::CmdSetEvent(_) | Command::CmdResetEvent(_) | Command::CmdResetEvent2(_) => {
                false
            }
            Command::CmdExecuteCommands(a) => {
                for child in a.p_command_buffers.iter().flatten() {
                    if let Some(recording) = self.recordings.get(&child.0) {
                        if uses.len().saturating_add(recording.uses.len()) > MAX_USES {
                            return Err("too many inherited buffer uses".into());
                        }
                        uses.extend(recording.uses.iter().cloned());
                    }
                }
                a.p_command_buffers
                    .iter()
                    .flatten()
                    .any(|child| self.recordings.get(&child.0).is_some_and(|r| r.opaque))
            }
            // Unknown commands are never assumed to touch only host pages.
            _ => true,
        };
        let charge = objects
            .limits()
            .charge(
                Class::RecordingBytes,
                (uses.len() * std::mem::size_of::<Use>() + 64) as u64,
            )
            .map_err(|e| format!("buffer tracking budget: {e:?}"))?;
        Ok(Note::Append {
            cb,
            uses,
            opaque,
            _charge: charge,
        })
    }

    pub fn finish<H: HostVulkan>(&mut self, note: Result<Note, String>, objects: &Objects<H>) {
        let result = (|| {
            match note? {
                Note::None => {}
                Note::Prune => {
                    self.recordings
                        .retain(|cb, _| objects.raw_any(Kind::CommandBuffer, *cb).is_ok());
                    self.opaque
                        .retain(|(device, _)| objects.device(*device).is_ok());
                }
                Note::ClearPool(pool) => {
                    self.recordings.retain(|cb, _| {
                        objects
                            .raw_any(Kind::CommandBuffer, *cb)
                            .is_ok_and(|r| r.pool != pool)
                    });
                }
                Note::Clear(cb) => {
                    self.recordings.remove(&cb);
                }
                Note::Append {
                    cb, uses, opaque, ..
                } => {
                    if let std::collections::hash_map::Entry::Vacant(entry) =
                        self.recordings.entry(cb)
                    {
                        let charge = objects
                            .limits()
                            .charge(Class::RecordingBytes, 64)
                            .map_err(|e| format!("buffer tracking budget: {e:?}"))?;
                        entry.insert(Recording {
                            uses: Vec::new(),
                            opaque: false,
                            charge,
                        });
                    }
                    let recording = self
                        .recordings
                        .get_mut(&cb)
                        .ok_or("missing buffer recording")?;
                    if recording.uses.len().saturating_add(uses.len()) > MAX_USES {
                        return Err("too many accumulated buffer uses".into());
                    }
                    recording
                        .charge
                        .grow((uses.len() * std::mem::size_of::<Use>()) as u64)
                        .map_err(|e| format!("buffer recording budget: {e:?}"))?;
                    recording
                        .uses
                        .try_reserve(uses.len())
                        .map_err(|_| "buffer recording allocation")?;
                    recording.uses.extend(uses);
                    recording.opaque |= opaque;
                }
            }
            Ok(())
        })();
        if let Err(error) = result {
            self.error = Some(error);
        }
    }

    pub fn submitted<H: HostVulkan>(
        &mut self,
        cb: u64,
        device: u64,
        family: u32,
        objects: &mut Objects<H>,
    ) {
        if self.error.is_some() {
            return;
        }
        let Some(recording) = self.recordings.get(&cb) else {
            return;
        };
        if recording.opaque {
            if self.opaque.len() >= MAX_USES {
                self.error = Some("too many opaque buffer families".into());
                return;
            }
            self.opaque.insert((device, family));
        }
        for used in &recording.uses {
            let Ok(buffer) = objects.buffer_mut(used.device, used.id) else {
                continue;
            };
            if !Arc::ptr_eq(&buffer.identity, &used.identity)
                || !matches!(buffer.sharing, Sharing::Exclusive)
            {
                continue;
            }
            match buffer.checkpoint_owner {
                Some((owner, _)) if owner != family => {
                    self.error = Some(
                        "buffer ranges used by multiple families need range ownership tracking"
                            .into(),
                    );
                    return;
                }
                owner => {
                    buffer.checkpoint_owner = Some((
                        family,
                        used.whole || owner.is_some_and(|(_, complete)| complete),
                    ))
                }
            }
        }
    }
}

impl<H: HostVulkan> VulkanContext<H> {
    pub(super) fn checkpoint_buffer_family(
        &self,
        device: u64,
        id: u64,
    ) -> Result<Option<u32>, RecipeError> {
        if let Some(error) = &self.buffer_tracking.error {
            return Err(RecipeError::Replay(error.clone()));
        }
        let buffer = self
            .objects
            .buffer(device, id)
            .map_err(|e| RecipeError::Replay(e.to_string()))?;
        if !matches!(buffer.sharing, Sharing::Exclusive) {
            return Ok(None);
        }
        let queues = self.objects.queues_of(device);
        let first = queues
            .first()
            .and_then(|id| self.objects.queue(*id).ok())
            .map(|q| q.family);
        if queues.iter().all(|id| {
            self.objects
                .queue(*id)
                .is_ok_and(|q| Some(q.family) == first)
        }) {
            return Ok(first);
        }
        // No direct use was ever accepted for this generation. Every other
        // vkCmd* that could access it contributes an opaque family. If exactly
        // one such family exists, every defined byte can only belong to it.
        // Unused ranges are undefined; implicit acquisition cannot discard
        // defined contents from any other family (Vulkan Resource Sharing).
        let mut opaque = self
            .buffer_tracking
            .opaque
            .iter()
            .filter(|(owner, _)| *owner == device)
            .map(|(_, family)| *family);
        let sole_opaque = opaque.next().filter(|_| opaque.next().is_none());
        let proof = buffer
            .checkpoint_owner
            .or_else(|| sole_opaque.map(|family| (family, true)));
        let Some((family, true)) = proof else {
            return Err(RecipeError::Replay(format!(
                "context {} exclusive buffer {id:#x} ({} bytes, usage {:#x}, owner {:?}) needs a complete executed queue-family proof",
                self.ctx_id, buffer.size, buffer.usage, buffer.checkpoint_owner
            )));
        };
        if self
            .buffer_tracking
            .opaque
            .iter()
            .any(|(owner, other)| *owner == device && *other != family)
        {
            return Err(RecipeError::Invalid(
                "opaque buffer accesses span different families",
            ));
        }
        Ok(Some(family))
    }

    pub(super) fn save_buffer_ownership(&self) -> Result<Vec<u8>, RecipeError> {
        if let Some(error) = &self.buffer_tracking.error {
            return Err(RecipeError::Replay(error.clone()));
        }
        let mut buffers: Vec<_> = self.objects.checkpoint_buffers().collect();
        buffers.sort_unstable_by_key(|(id, _)| *id);
        if buffers.len() > MAX_USES || self.buffer_tracking.opaque.len() > MAX_USES {
            return Err(RecipeError::Budget);
        }
        let mut out = Encoder::with_limit(4 << 20);
        out.u32(1)?;
        out.u32(buffers.len() as u32)?;
        for (id, buffer) in buffers {
            out.u64(buffer.device)?;
            out.u64(id)?;
            out.u64(buffer.size)?;
            out.u32(
                buffer
                    .checkpoint_owner
                    .map_or(0, |(_, whole)| 1 + u32::from(whole)),
            )?;
            out.u32(buffer.checkpoint_owner.map_or(0, |(family, _)| family))?;
        }
        out.u32(self.buffer_tracking.opaque.len() as u32)?;
        for (device, family) in &self.buffer_tracking.opaque {
            out.u64(*device)?;
            out.u32(*family)?;
        }
        Ok(out.finish()?)
    }

    pub(super) fn restore_buffer_ownership(&mut self, bytes: &[u8]) -> Result<(), RecipeError> {
        let invalid = RecipeError::Invalid;
        if bytes.len() > 4 << 20 {
            return Err(RecipeError::Budget);
        }
        let mut input = Decoder::new(bytes);
        if input.u32()? != 1 {
            return Err(invalid("buffer ownership version"));
        }
        let count = input.u32()? as usize;
        if count > MAX_USES
            || count != self.objects.checkpoint_buffers().count()
            || count > input.remaining() / 32
        {
            return Err(invalid("buffer ownership inventory"));
        }
        let mut records = Vec::new();
        records
            .try_reserve_exact(count)
            .map_err(|_| RecipeError::Budget)?;
        let mut previous = None;
        for _ in 0..count {
            let (device, id, size) = (input.u64()?, input.u64()?, input.u64()?);
            let (tag, family) = (input.u32()?, input.u32()?);
            let buffer = self
                .objects
                .buffer(device, id)
                .map_err(|e| RecipeError::Replay(e.to_string()))?;
            if previous.is_some_and(|prior| prior >= id)
                || size != buffer.size
                || tag > 2
                || (tag == 0 && family != 0)
            {
                return Err(invalid("buffer ownership record"));
            }
            if tag != 0
                && !self
                    .objects
                    .queues_of(device)
                    .iter()
                    .any(|id| self.objects.queue(*id).is_ok_and(|q| q.family == family))
            {
                return Err(invalid("buffer ownership queue family"));
            }
            records.push((device, id, (tag != 0).then_some((family, tag == 2))));
            previous = Some(id);
        }
        let count = input.u32()? as usize;
        if count > MAX_USES || count > input.remaining() / 12 {
            return Err(invalid("opaque buffer family count"));
        }
        let mut opaque = BTreeSet::new();
        for _ in 0..count {
            let key = (input.u64()?, input.u32()?);
            if opaque.last().is_some_and(|last| *last >= key)
                || !self
                    .objects
                    .queues_of(key.0)
                    .iter()
                    .any(|id| self.objects.queue(*id).is_ok_and(|q| q.family == key.1))
            {
                return Err(invalid("opaque buffer family record"));
            }
            opaque.insert(key);
        }
        if input.remaining() != 0 {
            return Err(invalid("trailing buffer ownership data"));
        }
        for (device, id, owner) in records {
            self.objects
                .buffer_mut(device, id)
                .map_err(|e| RecipeError::Replay(e.to_string()))?
                .checkpoint_owner = owner;
        }
        self.buffer_tracking.opaque = opaque;
        Ok(())
    }
}
