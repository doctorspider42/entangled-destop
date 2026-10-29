//! The completed GPU synchronisation state, separate from construction history.
//! Callers must park guest/ring writers throughout capture and reconstruction.

use std::collections::HashSet;
use std::time::{Duration, Instant};

use super::checkpoint::RecipeError;
use super::host::{HostVulkan, Idle, RawHandle};
use super::objects::{
    EventSignal, EventSignalRecipe, EventSignalResource, EventState, Facts, Kind, SemaphoreState,
};
use super::VulkanContext;
use crate::venus::protocol::*;
use crate::venus::wire::{Decoder, Encoder};

const VERSION: u32 = 1;
const MAX_RECORDS: usize = 1 << 16;
const RECORD_BYTES: usize = 40;
const MAX_SYNC_BYTES: usize = 64 << 20;
const MAX_SIGNAL_BYTES: usize = 8 << 20;
const ALL_COMMANDS: u32 = 0x1_0000;

#[derive(Debug, Clone)]
enum State {
    Fence(bool),
    Event {
        set: bool,
        device_only: bool,
        signal: Option<EventSignal>,
    },
    Binary {
        pending: bool,
        temporary: bool,
    },
    Timeline(u64),
}

#[cfg(test)]
pub(crate) mod tests {
    use super::super::checkpoint::MAX_RECIPE_BYTES;
    use super::super::fake::FakeVulkan;
    use super::super::harness::*;
    use super::super::recording::*;
    use super::*;
    use crate::venus::shmem::PageBudget;
    use std::sync::atomic::Ordering;
    use std::sync::Arc;

    const EVENT: u64 = 0x700;
    const DEVICE_EVENT: u64 = EVENT + 1;
    const BIN: u64 = 0x710;
    const TEMP: u64 = BIN + 1;
    const TIMELINE: u64 = BIN + 2;
    const WAIT: Duration = Duration::from_secs(2);
    const CB: u64 = super::super::recording::CB + 100;
    const FENCE: u64 = super::super::recording::FENCE + 100;

    fn run<H: HostVulkan>(context: &mut VulkanContext<H>, mut command: Command<'_>) {
        context.execute(&mut command).unwrap();
        assert!(super::super::context::result_of(&command).is_none_or(|r| r == VK_SUCCESS));
    }

    fn fixture(host: Arc<FakeVulkan>, budget: &Arc<PageBudget>) -> VulkanContext<FakeVulkan> {
        let mut context = VulkanContext::new(1, host);
        context.enable_object_recipes(budget).unwrap();
        run(&mut context, create_instance(INSTANCE));
        run(&mut context, enumerate(INSTANCE, Some(vec![PHYSICAL])));
        run(&mut context, create_device(PHYSICAL, DEVICE, Vec::new()));
        run(&mut context, device_queue(DEVICE, QUEUE, 1));
        run(&mut context, create_pool(DEVICE, POOL));
        populate(&mut context);
        context
    }

    pub(crate) fn populate<H: HostVulkan>(context: &mut VulkanContext<H>) {
        run(context, create_fence(DEVICE, FENCE, false));
        run(context, create_fence(DEVICE, FENCE + 1, true));
        run(context, create_fence(DEVICE, FENCE + 2, false));
        run(context, create_semaphore(DEVICE, BIN, None, 0));
        run(context, create_semaphore(DEVICE, TEMP, None, 0));
        run(context, create_semaphore(DEVICE, TIMELINE, Some(1), 0));
        for (id, flags) in [(EVENT, 0), (DEVICE_EVENT, 1)] {
            run(
                context,
                Command::CreateEvent(CreateEventArgs {
                    device: VkDevice(DEVICE),
                    p_create_info: Some(VkEventCreateInfo { flags }),
                    p_event: Some(VkEvent(id)),
                    ret: 0,
                }),
            );
        }
        run(context, allocate_cbs(DEVICE, POOL, &[CB], false));
        run(context, begin(CB));
        run(
            context,
            Command::CmdSetEvent(CmdSetEventArgs {
                command_buffer: VkCommandBuffer(CB),
                event: VkEvent(EVENT),
                stage_mask: STAGE_TRANSFER,
            }),
        );
        run(
            context,
            Command::CmdSetEvent2(CmdSetEvent2Args {
                command_buffer: VkCommandBuffer(CB),
                event: VkEvent(DEVICE_EVENT),
                p_dependency_info: Some(VkDependencyInfo {
                    memory_barrier_count: 1,
                    p_memory_barriers: Some(vec![VkMemoryBarrier2 {
                        src_stage_mask: u64::from(ALL_COMMANDS),
                        ..Default::default()
                    }]),
                    ..Default::default()
                }),
            }),
        );
        run(context, end(CB));
        run(
            context,
            submit_semaphores(QUEUE, &[CB], &[], &[(BIN, 0), (TIMELINE, 17)], true, FENCE),
        );
        // Consume an imported temporary payload while keeping the permanent
        // GPU signal. History alone leaves this import incorrectly pending.
        run(context, import_semaphore_resource(DEVICE, BIN, 0));
        run(
            context,
            submit_semaphores(QUEUE, &[], &[(BIN, 0)], &[], false, 0),
        );
        run(context, import_semaphore_resource(DEVICE, TEMP, 0));
        run(context, wait_fences(DEVICE, &[FENCE], 2_000_000_000));
        run(
            context,
            Command::FreeCommandBuffers(FreeCommandBuffersArgs {
                device: VkDevice(DEVICE),
                command_pool: VkCommandPool(POOL),
                command_buffer_count: 1,
                p_command_buffers: Some(vec![VkCommandBuffer(CB)]),
            }),
        );
    }

    pub(crate) fn check_after_restore<H: HostVulkan>(context: &mut VulkanContext<H>) {
        run(context, wait_fences(DEVICE, &[FENCE, FENCE + 1], 0));
        let mut counter = counter_value(DEVICE, TIMELINE);
        context.execute(&mut counter).unwrap();
        let Command::GetSemaphoreCounterValue(args) = counter else {
            panic!()
        };
        assert_eq!(args.p_value, Some(17));
        let wait_cb = CB + 1;
        run(context, allocate_cbs(DEVICE, POOL, &[wait_cb], false));
        run(context, begin(wait_cb));
        run(
            context,
            Command::CmdWaitEvents(CmdWaitEventsArgs {
                command_buffer: VkCommandBuffer(wait_cb),
                event_count: 1,
                p_events: Some(vec![VkEvent(EVENT)]),
                src_stage_mask: STAGE_TRANSFER,
                dst_stage_mask: STAGE_TRANSFER,
                ..Default::default()
            }),
        );
        run(
            context,
            Command::CmdWaitEvents2(CmdWaitEvents2Args {
                command_buffer: VkCommandBuffer(wait_cb),
                event_count: 1,
                p_events: Some(vec![VkEvent(DEVICE_EVENT)]),
                p_dependency_infos: Some(vec![VkDependencyInfo {
                    memory_barrier_count: 1,
                    p_memory_barriers: Some(vec![VkMemoryBarrier2 {
                        src_stage_mask: u64::from(ALL_COMMANDS),
                        ..Default::default()
                    }]),
                    ..Default::default()
                }]),
            }),
        );
        run(context, end(wait_cb));
        run(
            context,
            submit_semaphores(
                QUEUE,
                &[wait_cb],
                &[(BIN, 0), (TEMP, 0), (TIMELINE, 17)],
                &[],
                true,
                FENCE + 2,
            ),
        );
        run(context, wait_fences(DEVICE, &[FENCE + 2], 2_000_000_000));
    }

    #[test]
    fn completed_gpu_signals_survive_reconstruction_and_still_release_guest_waits() {
        let budget = PageBudget::new(MAX_RECIPE_BYTES);
        let host = Arc::new(FakeVulkan::standard());
        let mut original = fixture(host.clone(), &budget);
        let state = original.save_sync_state(WAIT).unwrap();
        let recipes = original.save_object_recipes().unwrap();
        original.destroy_all();
        drop(original);
        assert_eq!(host.live_objects(), 0);
        let host = Arc::new(FakeVulkan::standard());
        let mut restored = VulkanContext::new(1, host.clone());
        restored.restore_object_recipes(&recipes, &budget).unwrap();
        let mut status = fence_status(DEVICE, FENCE);
        restored.execute(&mut status).unwrap();
        assert_eq!(
            super::super::context::result_of(&status),
            Some(VK_NOT_READY)
        );
        restored.restore_sync_state(&state, WAIT).unwrap();
        assert_eq!(restored.save_sync_state(WAIT).unwrap(), state);
        check_after_restore(&mut restored);
        assert!(host.unsatisfiable_waits().is_empty());
        assert!(
            !restored
                .semaphore_state("test", DEVICE, BIN)
                .unwrap()
                .pending
        );
        let temporary = restored.semaphore_state("test", DEVICE, TEMP).unwrap();
        assert!(!temporary.pending && !temporary.temporary);
        restored.destroy_all();
        assert_eq!(host.live_objects(), 0);
    }

    #[test]
    fn a_malformed_sync_file_never_half_resets_native_signals() {
        let budget = PageBudget::new(MAX_RECIPE_BYTES);
        let host = Arc::new(FakeVulkan::standard());
        let mut context = fixture(host.clone(), &budget);
        let state = context.save_sync_state(WAIT).unwrap();
        let calls = host.calls();
        let mut cases: Vec<_> = (0..state.len()).map(|end| state[..end].to_vec()).collect();
        for (offset, value) in [(0, 99u32), (4, u32::MAX), (8, 99), (12, 99)] {
            let mut bytes = state.clone();
            bytes[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
            cases.push(bytes);
        }
        let mut trailing = state.clone();
        trailing.push(0);
        cases.push(trailing);
        // A well-formed executable command is not an event signal recipe.
        // Reject it while validating the file, before resetting any fence.
        let mut decoder = Decoder::new(&state);
        decoder.u32().unwrap();
        let count = decoder.u32().unwrap();
        for _ in 0..count {
            decoder.blob(32).unwrap();
            let len_offset = state.len() - decoder.remaining();
            let len = decoder.u32().unwrap() as usize;
            decoder.u32().unwrap();
            let payload_offset = state.len() - decoder.remaining();
            decoder.blob(len).unwrap();
            if len != 0 {
                let mut encoder = Encoder::new();
                queue_submit(QUEUE, &[], 0)
                    .encode_command(&mut encoder, 0)
                    .unwrap();
                let replacement = encoder.finish().unwrap();
                let mut smuggled = state.clone();
                smuggled[len_offset..len_offset + 4]
                    .copy_from_slice(&(replacement.len() as u32).to_le_bytes());
                smuggled.splice(payload_offset..payload_offset + len, replacement);
                cases.push(smuggled);
                break;
            }
        }
        for bytes in cases {
            assert!(context.restore_sync_state(&bytes, WAIT).is_err());
            assert_eq!(host.calls(), calls);
            assert!(!context.is_fatal());
        }
        assert_eq!(context.save_sync_state(WAIT).unwrap(), state);
        context.destroy_all();
    }

    #[test]
    fn an_event_dependency_cannot_change_when_a_guest_reuses_a_buffer_id() {
        let budget = PageBudget::new(MAX_RECIPE_BYTES);
        let host = Arc::new(FakeVulkan::standard());
        let mut context = fixture(host.clone(), &budget);
        run(
            &mut context,
            create_buffer(DEVICE, BUFFER, buffer_info(4096, TRANSFER)),
        );
        run(&mut context, allocate(DEVICE, MEMORY, 4096, 1, Vec::new()));
        run(&mut context, bind_buffers(DEVICE, &[(BUFFER, MEMORY, 0)]));
        run(&mut context, allocate_cbs(DEVICE, POOL, &[CB], false));
        run(&mut context, begin(CB));
        run(
            &mut context,
            Command::CmdResetEvent(CmdResetEventArgs {
                command_buffer: VkCommandBuffer(CB),
                event: VkEvent(EVENT),
                stage_mask: ALL_COMMANDS,
            }),
        );
        run(
            &mut context,
            Command::CmdSetEvent2(CmdSetEvent2Args {
                command_buffer: VkCommandBuffer(CB),
                event: VkEvent(EVENT),
                p_dependency_info: Some(VkDependencyInfo {
                    buffer_memory_barrier_count: 1,
                    p_buffer_memory_barriers: Some(vec![VkBufferMemoryBarrier2 {
                        src_stage_mask: u64::from(ALL_COMMANDS),
                        src_queue_family_index: u32::MAX,
                        dst_queue_family_index: u32::MAX,
                        buffer: VkBuffer(BUFFER),
                        size: 4096,
                        ..Default::default()
                    }]),
                    ..Default::default()
                }),
            }),
        );
        run(&mut context, end(CB));
        run(&mut context, queue_submit(QUEUE, &[CB], 0));
        context.save_sync_state(WAIT).unwrap();
        run(&mut context, destroy_buffer(DEVICE, BUFFER));
        run(
            &mut context,
            create_buffer(DEVICE, BUFFER, buffer_info(4096, TRANSFER)),
        );
        assert!(context.save_sync_state(WAIT).is_err());
        assert!(!context.is_fatal());
        context.destroy_all();
        assert_eq!(host.live_objects(), 0);
    }

    #[test]
    fn a_redundant_host_set_does_not_replace_the_original_gpu_signal() {
        let budget = PageBudget::new(MAX_RECIPE_BYTES);
        let host = Arc::new(FakeVulkan::standard());
        let mut context = fixture(host, &budget);
        context.save_sync_state(WAIT).unwrap();
        run(
            &mut context,
            Command::SetEvent(SetEventArgs {
                device: VkDevice(DEVICE),
                event: VkEvent(EVENT),
                ret: 0,
            }),
        );
        let object = context.objects.raw(Kind::Event, DEVICE, EVENT).unwrap();
        assert!(
            matches!(&object.facts, Facts::Event(state) if matches!(state.signal, Some(EventSignal::Device { .. })))
        );
        run(
            &mut context,
            Command::ResetEvent(ResetEventArgs {
                device: VkDevice(DEVICE),
                event: VkEvent(EVENT),
                ret: 0,
            }),
        );
        run(
            &mut context,
            Command::SetEvent(SetEventArgs {
                device: VkDevice(DEVICE),
                event: VkEvent(EVENT),
                ret: 0,
            }),
        );
        let object = context.objects.raw(Kind::Event, DEVICE, EVENT).unwrap();
        assert!(
            matches!(&object.facts, Facts::Event(state) if matches!(state.signal, Some(EventSignal::Host)))
        );
        context.destroy_all();
    }

    #[test]
    fn a_busy_gpu_refuses_capture_and_the_running_context_remains_intact() {
        let budget = PageBudget::new(MAX_RECIPE_BYTES);
        let host = Arc::new(FakeVulkan::standard());
        let mut context = fixture(host.clone(), &budget);
        let native = *context.objects.device(DEVICE).unwrap().host;
        let objects = context.object_count();
        host.wedge_device(native);
        assert!(context.save_sync_state(Duration::from_millis(1)).is_err());
        assert_eq!(context.object_count(), objects);
        assert!(!context.is_fatal());
        host.unwedge_device(native);
        context.save_sync_state(WAIT).unwrap();
        context.destroy_all();
        assert_eq!(host.live_objects(), 0);
    }

    #[test]
    fn a_timed_out_device_event_copy_keeps_its_private_pool_alive_until_the_gpu_finishes() {
        let budget = PageBudget::new(MAX_RECIPE_BYTES);
        let host = Arc::new(FakeVulkan::standard());
        let mut context = fixture(host.clone(), &budget);
        context.save_sync_state(WAIT).unwrap();
        let pools = host.live("command pool");
        host.stuck.store(true, Ordering::SeqCst);
        assert!(context
            .restore_device_event(DEVICE, DEVICE_EVENT, None, Instant::now() + WAIT)
            .is_err());
        assert_eq!(host.live("command pool"), pools + 1);
        assert_eq!(context.objects.doomed(), 1);
        host.stuck.store(false, Ordering::SeqCst);
        context.checkpoint_wait_idle(Instant::now() + WAIT).unwrap();
        assert_eq!(host.live("command pool"), pools);
        assert_eq!(context.objects.doomed(), 0);
        context.destroy_all();
        assert_eq!(host.live_objects(), 0);
    }
}

impl State {
    fn kind(&self) -> Kind {
        match self {
            Self::Fence(_) => Kind::Fence,
            Self::Event { .. } => Kind::Event,
            Self::Binary { .. } | Self::Timeline(_) => Kind::Semaphore,
        }
    }
}

#[derive(Debug, Clone)]
struct Record {
    id: u64,
    device: u64,
    state: State,
}

fn failed(what: impl Into<String>) -> RecipeError {
    RecipeError::Replay(what.into())
}

pub(super) fn remaining(deadline: Instant) -> Result<Duration, RecipeError> {
    deadline
        .checked_duration_since(Instant::now())
        .ok_or_else(|| failed("the GPU checkpoint deadline expired"))
}

impl<H: HostVulkan> VulkanContext<H> {
    fn event_signal_resources(
        &self,
        device: u64,
        command: &Command<'_>,
    ) -> Result<Vec<EventSignalResource>, super::ExecError> {
        let mut resources = Vec::new();
        if let Command::CmdSetEvent2(args) = command {
            if let Some(info) = &args.p_dependency_info {
                for barrier in info.p_buffer_memory_barriers.iter().flatten() {
                    let id = barrier.buffer.0;
                    let identity = self
                        .objects
                        .buffer(device, id)
                        .map_err(super::context::id_error(command.name()))?
                        .identity
                        .clone();
                    resources.push(EventSignalResource {
                        kind: Kind::Buffer,
                        id,
                        identity,
                    });
                }
                for barrier in info.p_image_memory_barriers.iter().flatten() {
                    let id = barrier.image.0;
                    let identity = self
                        .objects
                        .image(device, id)
                        .map_err(super::context::id_error(command.name()))?
                        .identity
                        .clone();
                    resources.push(EventSignalResource {
                        kind: Kind::Image,
                        id,
                        identity,
                    });
                }
            }
        }
        Ok(resources)
    }

    pub(super) fn retain_event_signal(
        &self,
        command: &Command<'_>,
    ) -> Result<EventSignal, super::ExecError> {
        let name = command.name();
        let cb = match command {
            Command::CmdSetEvent(args) => args.command_buffer.0,
            Command::CmdSetEvent2(args) => args.command_buffer.0,
            _ => return Err(super::context::invalid(name, "not an event signal")),
        };
        let device = self.cmd_device(name, cb)?;
        let resources = self.event_signal_resources(device, command)?;
        let mut encoder = Encoder::with_limit(MAX_SIGNAL_BYTES);
        command
            .encode_command(&mut encoder, 0)
            .map_err(|e| super::context::invalid(name, e.to_string()))?;
        let bytes = encoder
            .finish()
            .map_err(|e| super::context::invalid(name, e.to_string()))?;
        let charge = self
            .objects
            .limits()
            .charge(
                super::limits::Class::RecordingBytes,
                (bytes.len() + resources.len() * std::mem::size_of::<EventSignalResource>()) as u64,
            )
            .map_err(|refused| super::ExecError::Refused {
                command: name,
                refused,
            })?;
        Ok(EventSignal::Device {
            recipe: std::sync::Arc::new(EventSignalRecipe {
                bytes,
                resources,
                _charge: charge,
            }),
            family: 0,
        })
    }
    fn checkpoint_invoke(&self, device: u64, command: &mut Command<'_>) -> Result<(), RecipeError> {
        let owner = self
            .objects
            .device(device)
            .map_err(|e| failed(e.to_string()))?;
        self.host
            .call(&owner.host, command)
            .map_err(|e| failed(format!("{}: {e}", command.name())))
    }

    fn checkpoint_queue(&self, device: u64, family: Option<u32>) -> Result<u64, RecipeError> {
        self.objects
            .device(device)
            .map_err(|e| failed(e.to_string()))?
            .queues
            .iter()
            .filter(|q| q.flags == 0 && family.is_none_or(|family| family == q.family))
            .find_map(|q| q.id)
            .ok_or_else(|| {
                failed(
                    "checkpoint reconstruction needs an unprotected queue of the original family",
                )
            })
    }

    pub(super) fn checkpoint_call<'a>(
        &self,
        device: u64,
        mut command: Command<'a>,
    ) -> Result<Command<'a>, RecipeError> {
        self.checkpoint_invoke(device, &mut command)?;
        if super::context::result_of(&command).is_some_and(|ret| ret != VK_SUCCESS) {
            return Err(failed(format!(
                "{} returned {:?}",
                command.name(),
                super::context::result_of(&command)
            )));
        }
        Ok(command)
    }

    pub(super) fn checkpoint_wait_idle(&mut self, deadline: Instant) -> Result<(), RecipeError> {
        if self.is_fatal() || !self.holds.is_empty() {
            return Err(failed(
                "GPU checkpoint needs a live context without deferred submissions",
            ));
        }
        for id in self.objects.checkpoint_device_ids() {
            let device = self.objects.device(id).map_err(|e| failed(e.to_string()))?;
            let queues: Vec<_> = self
                .objects
                .queues_of(id)
                .iter()
                .map(|q| self.objects.queue(*q).map(|q| q.host))
                .collect::<Result<_, _>>()
                .map_err(|e| failed(e.to_string()))?;
            match self
                .host
                .device_idle_within(&device.host, &queues, remaining(deadline)?)
            {
                Idle::Idle => self.checkpoint_complete_idle(id),
                Idle::Busy => {
                    return Err(failed(
                        "GPU work did not finish before the checkpoint deadline",
                    ))
                }
                Idle::Lost => return Err(failed("the GPU device was lost during checkpointing")),
            }
        }
        Ok(())
    }

    /// Capture completed fences, events and semaphore payloads after a bounded
    /// GPU drain. A timeout neither destroys objects nor drops deferred work.
    /// The guest, ring workers and other GPU writers must already be parked.
    pub fn save_sync_state(&mut self, timeout: Duration) -> Result<Vec<u8>, RecipeError> {
        let deadline = Instant::now()
            .checked_add(timeout)
            .ok_or_else(|| failed("invalid GPU checkpoint timeout"))?;
        self.checkpoint_wait_idle(deadline)?;
        let objects = self.objects.checkpoint_sync_objects();
        if objects.len() > MAX_RECORDS {
            return Err(RecipeError::Budget);
        }
        let mut encoder = Encoder::with_limit(MAX_SYNC_BYTES);
        encoder.u32(VERSION)?;
        encoder.u32(objects.len() as u32)?;
        for (id, object) in objects {
            remaining(deadline)?;
            let (tag, value, flags, signal) = match object.kind {
                Kind::Fence => {
                    let mut command = Command::GetFenceStatus(GetFenceStatusArgs {
                        device: VkDevice(object.device),
                        fence: VkFence(object.host),
                        ret: VK_ERROR_UNKNOWN,
                    });
                    self.checkpoint_invoke(object.device, &mut command)?;
                    let Some(ret) = super::context::result_of(&command) else {
                        return Err(failed("missing fence status"));
                    };
                    match ret {
                        VK_SUCCESS => (0, 1, 0, None),
                        VK_NOT_READY => (0, 0, 0, None),
                        _ => return Err(failed(format!("checkpoint fence status {ret}"))),
                    }
                }
                Kind::Event => {
                    let Facts::Event(state) = object.facts else {
                        return Err(failed("missing event facts"));
                    };
                    let set = if state.device_only {
                        // This event explicitly forbids host status queries.
                        // With all queues idle, the submitted event operations
                        // have completed in their synchronised execution order.
                        state.set
                    } else {
                        let mut command = Command::GetEventStatus(GetEventStatusArgs {
                            device: VkDevice(object.device),
                            event: VkEvent(object.host),
                            ret: VK_ERROR_UNKNOWN,
                        });
                        self.checkpoint_invoke(object.device, &mut command)?;
                        match super::context::result_of(&command) {
                            Some(VK_EVENT_SET) => true,
                            Some(VK_EVENT_RESET) => false,
                            other => {
                                return Err(failed(format!("checkpoint event status {other:?}")))
                            }
                        }
                    };
                    let signal = if set {
                        if !state.set {
                            return Err(failed(
                                "an event's completed signal differs from its submitted state",
                            ));
                        }
                        Some(
                            state
                                .signal
                                .ok_or_else(|| failed("missing event signal recipe"))?,
                        )
                    } else {
                        None
                    };
                    let flags = match &signal {
                        None => 0,
                        Some(EventSignal::Host) => 1,
                        Some(EventSignal::Device { .. }) => 2,
                    };
                    (
                        if state.device_only { 4 } else { 1 },
                        u64::from(set),
                        flags,
                        signal,
                    )
                }
                Kind::Semaphore => {
                    let Facts::Semaphore(state) = object.facts else {
                        return Err(failed("missing semaphore facts"));
                    };
                    if state.timeline {
                        let command = self.checkpoint_call(
                            object.device,
                            Command::GetSemaphoreCounterValue(GetSemaphoreCounterValueArgs {
                                device: VkDevice(object.device),
                                semaphore: VkSemaphore(object.host),
                                p_value: Some(0),
                                ret: VK_ERROR_UNKNOWN,
                            }),
                        )?;
                        let Command::GetSemaphoreCounterValue(args) = command else {
                            return Err(failed("missing timeline value"));
                        };
                        let value = args
                            .p_value
                            .ok_or_else(|| failed("missing timeline value"))?;
                        if value < state.host_value {
                            return Err(failed(
                                "an idle timeline has not reached its submitted value",
                            ));
                        }
                        (3, value, 0, None)
                    } else {
                        if state.pending != state.host_pending {
                            return Err(failed("binary semaphore still has deferred state"));
                        }
                        (
                            2,
                            u64::from(state.host_pending),
                            u32::from(state.temporary),
                            None,
                        )
                    }
                }
                _ => return Err(failed("not a synchronisation object")),
            };
            encoder.u32(tag)?;
            encoder.u32(flags)?;
            encoder.u64(object.device)?;
            encoder.u64(id)?;
            encoder.u64(value)?;
            if let Some(EventSignal::Device { recipe, family }) = signal {
                for resource in &recipe.resources {
                    let current = match resource.kind {
                        Kind::Buffer => self
                            .objects
                            .buffer(object.device, resource.id)
                            .ok()
                            .map(|b| &b.identity),
                        Kind::Image => self
                            .objects
                            .image(object.device, resource.id)
                            .ok()
                            .map(|i| &i.identity),
                        _ => None,
                    };
                    if current
                        .is_none_or(|current| !std::sync::Arc::ptr_eq(current, &resource.identity))
                    {
                        return Err(failed("an event signal references a retired resource generation; its dependency needs reconstruction"));
                    }
                }
                encoder.u32(recipe.bytes.len() as u32)?;
                encoder.u32(family)?;
                encoder.blob(&recipe.bytes)?;
            } else {
                encoder.u32(0)?;
                encoder.u32(0)?;
            }
        }
        Ok(encoder.finish()?)
    }

    fn decode_sync_state(&self, bytes: &[u8]) -> Result<Vec<Record>, RecipeError> {
        if bytes.len() > MAX_SYNC_BYTES {
            return Err(RecipeError::Budget);
        }
        let mut decoder = Decoder::new(bytes);
        let version = decoder.u32()?;
        if version != VERSION {
            return Err(RecipeError::Version(version));
        }
        let count = decoder.u32()? as usize;
        if count > MAX_RECORDS
            || count != self.objects.checkpoint_sync_objects().len()
            || count > decoder.remaining() / RECORD_BYTES
        {
            return Err(RecipeError::Invalid(
                "synchronisation object count/length differs",
            ));
        }
        let mut records = Vec::new();
        records
            .try_reserve_exact(count)
            .map_err(|_| RecipeError::Budget)?;
        let mut seen = HashSet::new();
        for _ in 0..count {
            let tag = decoder.u32()?;
            let flags = decoder.u32()?;
            let device = decoder.u64()?;
            let id = decoder.u64()?;
            let value = decoder.u64()?;
            let signal_len = decoder.u32()? as usize;
            let family = decoder.u32()?;
            if signal_len > MAX_SIGNAL_BYTES {
                return Err(RecipeError::Budget);
            }
            let signal_bytes = decoder.blob(signal_len)?;
            let is_event = matches!(tag, 1 | 4);
            let signal = if is_event && value == 1 && flags == 2 {
                let mut command_decoder = Decoder::new(signal_bytes);
                let (header, command) = Command::decode_next(&mut command_decoder)?;
                let named_event = match &command {
                    Command::CmdSetEvent(a) => a.event.0,
                    Command::CmdSetEvent2(a) => a.event.0,
                    _ => return Err(RecipeError::Invalid("an event recipe is not a GPU signal")),
                };
                if header.flags != 0 || named_event != id || command_decoder.remaining() != 0 {
                    return Err(RecipeError::Invalid("invalid event signal recipe"));
                }
                self.checkpoint_queue(device, Some(family))?;
                let resources = self
                    .event_signal_resources(device, &command)
                    .map_err(|e| failed(e.to_string()))?;
                let _charge = self
                    .objects
                    .limits()
                    .charge(
                        super::limits::Class::RecordingBytes,
                        (signal_len + resources.len() * std::mem::size_of::<EventSignalResource>())
                            as u64,
                    )
                    .map_err(|_| RecipeError::Budget)?;
                let mut bytes = Vec::new();
                bytes
                    .try_reserve_exact(signal_len)
                    .map_err(|_| RecipeError::Budget)?;
                bytes.extend_from_slice(signal_bytes);
                Some(EventSignal::Device {
                    recipe: std::sync::Arc::new(EventSignalRecipe {
                        bytes,
                        resources,
                        _charge,
                    }),
                    family,
                })
            } else {
                if signal_len != 0 || family != 0 {
                    return Err(RecipeError::Invalid("unexpected event signal bytes"));
                }
                if is_event && value == 1 && flags == 1 {
                    Some(EventSignal::Host)
                } else {
                    None
                }
            };
            let state = match (tag, value, flags) {
                (0, 0..=1, 0) => State::Fence(value != 0),
                (1 | 4, 0, 0) => State::Event {
                    set: false,
                    device_only: tag == 4,
                    signal: None,
                },
                (1, 1, 1) | (1 | 4, 1, 2) => State::Event {
                    set: true,
                    device_only: tag == 4,
                    signal,
                },
                (2, 0..=1, 0..=1) => State::Binary {
                    pending: value != 0,
                    temporary: flags != 0,
                },
                (3, _, 0) => State::Timeline(value),
                _ => return Err(RecipeError::Invalid("invalid synchronisation state")),
            };
            if !seen.insert(id) {
                return Err(RecipeError::Invalid("duplicate synchronisation id"));
            }
            let object = self
                .objects
                .raw(state.kind(), device, id)
                .map_err(|e| failed(e.to_string()))?;
            match (&state, &object.facts) {
                (State::Timeline(_), Facts::Semaphore(s)) if s.timeline => {}
                (State::Binary { .. }, Facts::Semaphore(s)) if !s.timeline => {}
                (State::Event { device_only, .. }, Facts::Event(s))
                    if *device_only == s.device_only => {}
                (State::Fence(_), _) => {}
                _ => return Err(RecipeError::Invalid("synchronisation object type differs")),
            }
            records.push(Record { id, device, state });
        }
        if decoder.remaining() != 0 {
            return Err(RecipeError::Invalid("trailing synchronization bytes"));
        }
        Ok(records)
    }

    /// Apply a synchronization checkpoint to a reconstructed context before
    /// any guest/ring worker resumes. Validate the entire file before changing
    /// native state. On a host failure discard/park partial reconstruction.
    pub fn restore_sync_state(
        &mut self,
        bytes: &[u8],
        timeout: Duration,
    ) -> Result<(), RecipeError> {
        let records = self.decode_sync_state(bytes)?;
        let deadline = Instant::now()
            .checked_add(timeout)
            .ok_or_else(|| failed("invalid GPU checkpoint timeout"))?;
        self.checkpoint_wait_idle(deadline)?;
        let result = (|| {
            for record in records {
                remaining(deadline)?;
                self.restore_sync_record(record, deadline)?;
            }
            self.checkpoint_wait_idle(deadline)
        })();
        if result.is_err() {
            self.destroy_all();
        }
        result
    }

    fn checkpoint_empty_submit(
        &mut self,
        device: u64,
        fence: u64,
        binary: Option<(u64, bool)>,
    ) -> Result<(), RecipeError> {
        let queue = self.checkpoint_queue(device, None)?;
        let host_queue = self
            .objects
            .queue(queue)
            .map_err(|e| failed(e.to_string()))?
            .host
            .raw();
        let mut batch = VkSubmitInfo::default();
        if let Some((semaphore, signal)) = binary {
            if signal {
                batch.signal_semaphore_count = 1;
                batch.p_signal_semaphores = Some(vec![VkSemaphore(semaphore)]);
            } else {
                batch.wait_semaphore_count = 1;
                batch.p_wait_semaphores = Some(vec![VkSemaphore(semaphore)]);
                batch.p_wait_dst_stage_mask = Some(vec![ALL_COMMANDS]);
            }
        }
        self.checkpoint_call(
            device,
            Command::QueueSubmit(QueueSubmitArgs {
                queue: VkQueue(host_queue),
                submit_count: 1,
                p_submits: Some(vec![batch]),
                fence: VkFence(fence),
                ret: VK_ERROR_UNKNOWN,
            }),
        )?;
        self.clock_submitted(queue, 0, 0);
        Ok(())
    }

    fn restore_sync_record(
        &mut self,
        record: Record,
        deadline: Instant,
    ) -> Result<(), RecipeError> {
        let Record { device, id, state } = record;
        let object = self
            .objects
            .raw(state.kind(), device, id)
            .map_err(|e| failed(e.to_string()))?
            .clone();
        match state {
            State::Fence(signalled) => {
                let mut status = Command::GetFenceStatus(GetFenceStatusArgs {
                    device: VkDevice(device),
                    fence: VkFence(object.host),
                    ret: VK_ERROR_UNKNOWN,
                });
                self.checkpoint_invoke(device, &mut status)?;
                match (super::context::result_of(&status), signalled) {
                    (Some(VK_SUCCESS), true) | (Some(VK_NOT_READY), false) => {}
                    (Some(VK_SUCCESS), false) => {
                        self.checkpoint_call(
                            device,
                            Command::ResetFences(ResetFencesArgs {
                                device: VkDevice(device),
                                fence_count: 1,
                                p_fences: Some(vec![VkFence(object.host)]),
                                ret: VK_ERROR_UNKNOWN,
                            }),
                        )?;
                    }
                    (Some(VK_NOT_READY), true) => {
                        self.checkpoint_empty_submit(device, object.host, None)?
                    }
                    other => return Err(failed(format!("checkpoint fence status {other:?}"))),
                }
            }
            State::Timeline(value) => {
                let command = self.checkpoint_call(
                    device,
                    Command::GetSemaphoreCounterValue(GetSemaphoreCounterValueArgs {
                        device: VkDevice(device),
                        semaphore: VkSemaphore(object.host),
                        p_value: Some(0),
                        ret: VK_ERROR_UNKNOWN,
                    }),
                )?;
                let Command::GetSemaphoreCounterValue(args) = command else {
                    return Err(failed("missing timeline value"));
                };
                let current = args
                    .p_value
                    .ok_or_else(|| failed("missing timeline value"))?;
                if value < current {
                    return Err(failed("checkpoint would decrease a timeline semaphore"));
                }
                if value > current {
                    self.checkpoint_call(
                        device,
                        Command::SignalSemaphore(SignalSemaphoreArgs {
                            device: VkDevice(device),
                            p_signal_info: Some(VkSemaphoreSignalInfo {
                                semaphore: VkSemaphore(object.host),
                                value,
                            }),
                            ret: VK_ERROR_UNKNOWN,
                        }),
                    )?;
                }
                self.objects
                    .raw_mut(Kind::Semaphore, device, id)
                    .map_err(|e| failed(e.to_string()))?
                    .facts = Facts::Semaphore(SemaphoreState {
                    timeline: true,
                    host_value: value,
                    ..Default::default()
                });
            }
            State::Binary { pending, temporary } => {
                let Facts::Semaphore(current) = object.facts else {
                    return Err(failed("missing binary semaphore state"));
                };
                if current.host_pending != pending {
                    self.checkpoint_empty_submit(device, 0, Some((object.host, pending)))?;
                }
                self.objects
                    .raw_mut(Kind::Semaphore, device, id)
                    .map_err(|e| failed(e.to_string()))?
                    .facts = Facts::Semaphore(SemaphoreState {
                    pending,
                    host_pending: pending,
                    temporary,
                    ..Default::default()
                });
            }
            State::Event {
                set,
                device_only,
                signal,
            } => {
                match &signal {
                    Some(EventSignal::Device { recipe, family }) => {
                        self.restore_device_event(device, id, Some((recipe, *family)), deadline)?
                    }
                    Some(EventSignal::Host) if !device_only => {
                        self.checkpoint_call(
                            device,
                            Command::ResetEvent(ResetEventArgs {
                                device: VkDevice(device),
                                event: VkEvent(object.host),
                                ret: VK_ERROR_UNKNOWN,
                            }),
                        )?;
                        self.checkpoint_call(
                            device,
                            Command::SetEvent(SetEventArgs {
                                device: VkDevice(device),
                                event: VkEvent(object.host),
                                ret: VK_ERROR_UNKNOWN,
                            }),
                        )?;
                    }
                    None if !set => {
                        if device_only {
                            let Facts::Event(current) = &object.facts else {
                                return Err(failed("missing event state"));
                            };
                            if current.set {
                                self.restore_device_event(device, id, None, deadline)?;
                            }
                        } else {
                            self.checkpoint_call(
                                device,
                                Command::ResetEvent(ResetEventArgs {
                                    device: VkDevice(device),
                                    event: VkEvent(object.host),
                                    ret: VK_ERROR_UNKNOWN,
                                }),
                            )?;
                        }
                    }
                    _ => return Err(failed("invalid event signal origin")),
                }
                self.objects
                    .raw_mut(Kind::Event, device, id)
                    .map_err(|e| failed(e.to_string()))?
                    .facts = Facts::Event(EventState {
                    set,
                    device_only,
                    waiters: false,
                    signal,
                });
            }
        }
        Ok(())
    }

    /// DEVICE_ONLY events cannot be set from the CPU. The private pool and its
    /// fence enter the normal deferred-destruction machinery as soon as their
    /// submit succeeds, so a timed-out restore never frees them under the GPU.
    fn restore_device_event(
        &mut self,
        device: u64,
        event: u64,
        signal: Option<(&EventSignalRecipe, u32)>,
        deadline: Instant,
    ) -> Result<(), RecipeError> {
        let queue_id = self.checkpoint_queue(device, signal.map(|(_, family)| family))?;
        let host_event = self
            .objects
            .raw(Kind::Event, device, event)
            .map_err(|e| failed(e.to_string()))?
            .host;
        let queue = self
            .objects
            .queue(queue_id)
            .map_err(|e| failed(e.to_string()))?;
        let (host_queue, family) = (queue.host.raw(), queue.family);
        let owner = self
            .objects
            .device(device)
            .map_err(|e| failed(e.to_string()))?
            .host
            .clone();
        if self
            .objects
            .device(device)
            .map_err(|e| failed(e.to_string()))?
            .group_size
            != 1
        {
            return Err(failed(
                "an event checkpoint needs its single-device execution mask",
            ));
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
            .map_err(|r| failed(format!("checkpoint command pool: {r}")))?;
        let recorded = (|| {
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
                return Err(failed("missing checkpoint command buffer"));
            };
            let cb = args
                .p_command_buffers
                .as_ref()
                .and_then(|v| v.first())
                .copied()
                .filter(|cb| cb.0 != 0)
                .ok_or_else(|| failed("missing checkpoint command buffer"))?;
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
            self.checkpoint_call(
                device,
                Command::CmdResetEvent(CmdResetEventArgs {
                    command_buffer: cb,
                    event: VkEvent(host_event),
                    stage_mask: ALL_COMMANDS,
                }),
            )?;
            if let Some((recipe, _)) = signal {
                let mut decoder = Decoder::new(&recipe.bytes);
                let (_, mut command) = Command::decode_next(&mut decoder)?;
                self.translate_checkpoint_event(device, cb.0, &mut command)
                    .map_err(|e| failed(e.to_string()))?;
                self.checkpoint_call(device, command)?;
            }
            self.checkpoint_call(
                device,
                Command::EndCommandBuffer(EndCommandBufferArgs {
                    command_buffer: cb,
                    ret: VK_ERROR_UNKNOWN,
                }),
            )?;
            Ok(cb)
        })();
        let cb = match recorded {
            Ok(cb) => cb,
            Err(error) => {
                self.host.destroy_command_pool(&owner, pool);
                return Err(error);
            }
        };
        let created = self.checkpoint_call(
            device,
            Command::CreateFence(CreateFenceArgs {
                device: VkDevice(device),
                p_create_info: Some(VkFenceCreateInfo::default()),
                p_fence: Some(VkFence(0)),
                ret: VK_ERROR_UNKNOWN,
            }),
        );
        let fence = match created {
            Ok(Command::CreateFence(args)) => args.p_fence.filter(|f| f.0 != 0),
            _ => None,
        };
        let Some(fence) = fence else {
            self.host.destroy_command_pool(&owner, pool);
            return Err(failed("could not create a checkpoint fence"));
        };
        let submitted = self.checkpoint_call(
            device,
            Command::QueueSubmit(QueueSubmitArgs {
                queue: VkQueue(host_queue),
                submit_count: 1,
                p_submits: Some(vec![VkSubmitInfo {
                    command_buffer_count: 1,
                    p_command_buffers: Some(vec![cb]),
                    ..Default::default()
                }]),
                fence,
                ret: VK_ERROR_UNKNOWN,
            }),
        );
        if let Err(error) = submitted {
            self.host.destroy_object(&owner, Kind::Fence, fence.0);
            self.host.destroy_command_pool(&owner, pool);
            return Err(error);
        }
        self.clock_submitted(queue_id, 0, 0);
        let queue = self
            .objects
            .queue_mut(queue_id)
            .map_err(|e| failed(e.to_string()))?;
        queue
            .clock
            .in_flight
            .push_back((queue.clock.submitted, fence.0, true));
        self.objects.retain_checkpoint_pool(device, queue_id, pool);
        let timeout = u64::try_from(remaining(deadline)?.as_nanos()).unwrap_or(u64::MAX);
        self.checkpoint_call(
            device,
            Command::WaitForFences(WaitForFencesArgs {
                device: VkDevice(device),
                fence_count: 1,
                p_fences: Some(vec![fence]),
                wait_all: 1,
                timeout,
                ret: VK_ERROR_UNKNOWN,
            }),
        )?;
        self.poll_clocks(device);
        Ok(())
    }
}
