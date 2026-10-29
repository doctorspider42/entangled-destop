//! Semaphore fan-in before a private copy and fan-out afterwards. The copy
//! uses the resource's proven family; no ownership transfer is invented. Prepare every
//! native object before submitting, and retain accepted work on failure/timeout.
use super::checkpoint::RecipeError;
use super::checkpoint_buffers::CheckpointScratch;
use super::host::RawHandle;
use super::{HostVulkan, VulkanContext};
use crate::venus::protocol::*;

pub(super) struct CopyBridge(Vec<(u64, VkSemaphore, VkSemaphore)>);
fn invalid(reason: &'static str) -> RecipeError {
    RecipeError::Invalid(reason)
}

impl<H: HostVulkan> VulkanContext<H> {
    pub(super) fn checkpoint_prepare_copy(
        &self,
        device: u64,
        queue: u64,
        scratch: &mut CheckpointScratch<H>,
    ) -> Result<CopyBridge, RecipeError> {
        let family = self
            .objects
            .queue(queue)
            .map_err(|error| RecipeError::Replay(error.to_string()))?
            .family;
        if self.checkpoint_family_queue(device, Some(family))? != queue {
            return Err(invalid("copy queue selection changed"));
        }
        let mut queues = self.objects.queues_of(device);
        queues.sort_unstable();
        let mut bridge = CopyBridge(Vec::new());
        for other in queues.into_iter().filter(|id| *id != queue) {
            let mut semaphores = Vec::with_capacity(2);
            for _ in 0..2 {
                let command = self.checkpoint_call(
                    device,
                    Command::CreateSemaphore(CreateSemaphoreArgs {
                        device: VkDevice(device),
                        p_create_info: Some(VkSemaphoreCreateInfo::default()),
                        p_semaphore: Some(VkSemaphore(0)),
                        ret: VK_ERROR_UNKNOWN,
                    }),
                )?;
                let Command::CreateSemaphore(args) = command else {
                    return Err(invalid("missing copy semaphore"));
                };
                let semaphore = args
                    .p_semaphore
                    .filter(|value| value.0 != 0)
                    .ok_or(invalid("missing copy semaphore"))?;
                scratch.semaphores.push(semaphore.0);
                semaphores.push(semaphore);
            }
            bridge.0.push((other, semaphores[0], semaphores[1]));
        }
        for _ in 0..(bridge.0.len() * 2 + 1) {
            let command = self.checkpoint_call(
                device,
                Command::CreateFence(CreateFenceArgs {
                    device: VkDevice(device),
                    p_create_info: Some(VkFenceCreateInfo::default()),
                    p_fence: Some(VkFence(0)),
                    ret: VK_ERROR_UNKNOWN,
                }),
            )?;
            let Command::CreateFence(args) = command else {
                return Err(invalid("missing copy fence"));
            };
            scratch.fences.push(
                args.p_fence
                    .filter(|value| value.0 != 0)
                    .ok_or(invalid("missing copy fence"))?
                    .0,
            );
        }
        Ok(bridge)
    }

    fn checkpoint_private_submit(
        &mut self,
        device: u64,
        queue: u64,
        info: VkSubmitInfo,
        scratch: &mut CheckpointScratch<H>,
    ) -> Result<VkFence, RecipeError> {
        let host_queue = self
            .objects
            .queue(queue)
            .map_err(|error| RecipeError::Replay(error.to_string()))?
            .host
            .raw();
        let fence = scratch
            .fences
            .pop()
            .ok_or(invalid("missing prepared copy fence"))?;
        if let Err(error) = self.checkpoint_call(
            device,
            Command::QueueSubmit(QueueSubmitArgs {
                queue: VkQueue(host_queue),
                submit_count: 1,
                p_submits: Some(vec![info]),
                fence: VkFence(fence),
                ret: VK_ERROR_UNKNOWN,
            }),
        ) {
            scratch.fences.push(fence);
            return Err(error);
        }
        self.clock_submitted(queue, 0, 0);
        let queue = self
            .objects
            .queue_mut(queue)
            .map_err(|error| RecipeError::Replay(error.to_string()))?;
        queue
            .clock
            .in_flight
            .push_back((queue.clock.submitted, fence, true));
        Ok(VkFence(fence))
    }

    pub(super) fn checkpoint_submit_copy(
        &mut self,
        device: u64,
        queue: u64,
        cb: VkCommandBuffer,
        bridge: CopyBridge,
        scratch: &mut CheckpointScratch<H>,
    ) -> Result<Vec<VkFence>, RecipeError> {
        for &(other, inbound, _) in &bridge.0 {
            self.checkpoint_private_submit(
                device,
                other,
                VkSubmitInfo {
                    signal_semaphore_count: 1,
                    p_signal_semaphores: Some(vec![inbound]),
                    ..Default::default()
                },
                scratch,
            )?;
        }
        let count = bridge.0.len() as u32;
        let fence = self.checkpoint_private_submit(
            device,
            queue,
            VkSubmitInfo {
                wait_semaphore_count: count,
                p_wait_semaphores: Some(bridge.0.iter().map(|(_, sem, _)| *sem).collect()),
                p_wait_dst_stage_mask: Some(vec![0x1_0000; count as usize]), // ALL_COMMANDS
                command_buffer_count: 1,
                p_command_buffers: Some(vec![cb]),
                signal_semaphore_count: count,
                p_signal_semaphores: Some(bridge.0.iter().map(|(_, _, sem)| *sem).collect()),
                ..Default::default()
            },
            scratch,
        )?;
        let mut fences = vec![fence];
        // Finish submitting these dependencies even if the deadline just expired.
        // No wait here depends on a future signal from the resumed guest.
        for (other, _, outbound) in bridge.0 {
            fences.push(self.checkpoint_private_submit(
                device,
                other,
                VkSubmitInfo {
                    wait_semaphore_count: 1,
                    p_wait_semaphores: Some(vec![outbound]),
                    p_wait_dst_stage_mask: Some(vec![0x1_0000]),
                    ..Default::default()
                },
                scratch,
            )?);
        }
        Ok(fences)
    }
}
