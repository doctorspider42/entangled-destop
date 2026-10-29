//! Private-copy queue selection. Fetched queues are joined by private
//! semaphore dependencies around every transfer, never by submission order alone.
use super::checkpoint::RecipeError;
use super::{HostVulkan, VulkanContext};
use crate::venus::protocol::*;

pub(super) fn buffer_transfer(command: &Command<'_>) -> bool {
    let legacy = |barriers: &[VkBufferMemoryBarrier]| {
        barriers
            .iter()
            .any(|barrier| barrier.src_queue_family_index != barrier.dst_queue_family_index)
    };
    let dependency = |info: &VkDependencyInfo| {
        info.p_buffer_memory_barriers
            .iter()
            .flatten()
            .any(|barrier| barrier.src_queue_family_index != barrier.dst_queue_family_index)
    };
    match command {
        Command::CmdPipelineBarrier(args) => {
            legacy(args.p_buffer_memory_barriers.as_deref().unwrap_or_default())
        }
        Command::CmdWaitEvents(args) => {
            legacy(args.p_buffer_memory_barriers.as_deref().unwrap_or_default())
        }
        Command::CmdPipelineBarrier2(args) => args.p_dependency_info.iter().any(dependency),
        Command::CmdSetEvent2(args) => args.p_dependency_info.iter().any(dependency),
        Command::CmdWaitEvents2(args) => args.p_dependency_infos.iter().flatten().any(dependency),
        _ => false,
    }
}

#[cfg(test)]
mod tests;

impl<H: HostVulkan> VulkanContext<H> {
    pub(super) fn checkpoint_copy_queue(&self, device_id: u64) -> Result<u64, RecipeError> {
        self.checkpoint_family_queue(device_id, None)
    }

    pub(super) fn checkpoint_family_queue(
        &self,
        device_id: u64,
        family: Option<u32>,
    ) -> Result<u64, RecipeError> {
        let (device, guest) = self
            .objects
            .device_and_guest(device_id)
            .map_err(|error| RecipeError::Replay(error.to_string()))?;
        if device.group_size != 1 {
            return Err(RecipeError::Invalid(
                "private checkpoint copy needs a single physical device",
            ));
        }
        // On timeout the source resumes while private GPU work can still run.
        // Every queue it could later fetch must already carry the fan-out wait.
        if device.queues.iter().any(|queue| queue.id.is_none()) {
            return Err(RecipeError::Invalid("private checkpoint copies need all declared queues fetched to cover future work after a timeout"));
        }
        let mut fetched: Vec<_> = device
            .queues
            .iter()
            .filter(|queue| queue.id.is_some())
            .collect();
        if fetched.len() > 64 {
            return Err(RecipeError::Budget);
        }
        fetched.sort_unstable_by_key(|queue| (queue.family, queue.index, queue.id));
        if fetched.iter().any(|queue| queue.flags != 0) {
            return Err(RecipeError::Replay(format!(
                "private checkpoint copy needs unprotected queues; fetched families: {:?}",
                fetched
                    .iter()
                    .map(|queue| (queue.family, queue.index))
                    .collect::<Vec<_>>()
            )));
        }
        let first = fetched
            .iter()
            .find(|queue| family.is_none_or(|wanted| wanted == queue.family))
            .ok_or(RecipeError::Invalid(
                "missing private checkpoint queue in the resource's family",
            ))?;
        let id = first
            .id
            .ok_or(RecipeError::Invalid("missing fetched checkpoint queue"))?;
        let queue = self
            .objects
            .queue(id)
            .map_err(|error| RecipeError::Replay(error.to_string()))?;
        let properties =
            guest
                .queue_families
                .get(queue.family as usize)
                .ok_or(RecipeError::Invalid(
                    "missing private checkpoint queue properties",
                ))?;
        if first.flags != 0 || properties.queue_flags & 7 == 0 || queue.device != device_id {
            return Err(RecipeError::Invalid(
                "checkpoint copy needs an unprotected graphics, compute or transfer queue",
            ));
        }
        Ok(id)
    }
}
