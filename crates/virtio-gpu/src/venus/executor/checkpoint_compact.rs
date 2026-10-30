//! Discard superseded primary recordings, fully overwritten descriptor writes
//! and unreachable closed lifetimes. Never replay GPU work. Retained object
//! ordering, allocation/import generation identities and guest ids stay intact.
//!
//! Secondary buffers may be referenced by an older primary recording. Keep
//! their entire history (and their pool resets), including recycled ids. This
//! deliberately conservative policy needs no dependency-generation graph.

use std::collections::{BTreeMap, BTreeSet};

#[path = "checkpoint_lifetimes.rs"]
mod lifetimes;

use super::*;
use crate::venus::executor::{generated, objects::Kind};
use crate::venus::protocol::{
    DestroyCommandPoolArgs, DestroyDescriptorPoolArgs, ResetCommandPoolArgs,
    ResetDescriptorPoolArgs,
};

fn command(record: &[u8]) -> Result<Command<'_>, RecipeError> {
    let mut framing = Decoder::new(record);
    let _allocation_serial = framing.u64()?;
    let length = framing.u32()? as usize;
    let mut decoder = Decoder::new(framing.blob(length)?);
    Ok(Command::decode_next(&mut decoder)?.1)
}

#[derive(Default)]
struct Recording {
    pool: u64,
    records: Vec<usize>,
}

impl Recording {
    fn discard(&mut self, keep: &mut [bool]) {
        for index in self.records.drain(..) {
            keep[index] = false;
        }
    }
}

impl RecipeJournal {
    pub(super) fn compact_recordings(&mut self) -> Result<(), RecipeError> {
        // Pin ids over the entire journal, not just the currently live
        // generation. A freed secondary can have invalidated an old primary.
        let mut pinned = BTreeSet::new();
        let mut pinned_pools = BTreeSet::new();
        let mut copied_sets = BTreeSet::new();
        let mut descriptor_pools: BTreeMap<u64, BTreeSet<u64>> = BTreeMap::new();
        let mut freed_sets = BTreeMap::new();
        for (index, record) in self.records.iter().enumerate() {
            match command(record)? {
                Command::AllocateCommandBuffers(args) => {
                    if let Some(info) = &args.p_allocate_info {
                        if info.level != 0 {
                            pinned_pools.insert(info.command_pool.0);
                            pinned.extend(args.p_command_buffers.iter().flatten().map(|id| id.0));
                        }
                    }
                }
                Command::UpdateDescriptorSets(args) => {
                    // Copies read earlier descriptor values. Conservatively
                    // preserve every write to each source id across lifetimes.
                    copied_sets.extend(
                        args.p_descriptor_copies
                            .iter()
                            .flatten()
                            .map(|copy| copy.src_set.0),
                    );
                }
                Command::AllocateDescriptorSets(args) => {
                    if let Some(info) = &args.p_allocate_info {
                        descriptor_pools
                            .entry(info.descriptor_pool.0)
                            .or_default()
                            .extend(args.p_descriptor_sets.iter().flatten().map(|id| id.0));
                    }
                }
                Command::FreeDescriptorSets(args) => {
                    for id in args.p_descriptor_sets.iter().flatten() {
                        freed_sets.insert(id.0, index);
                        if let Some(ids) = descriptor_pools.get_mut(&args.descriptor_pool.0) {
                            ids.remove(&id.0);
                        }
                    }
                }
                Command::ResetDescriptorPool(ResetDescriptorPoolArgs {
                    descriptor_pool: pool,
                    ..
                })
                | Command::DestroyDescriptorPool(DestroyDescriptorPoolArgs {
                    descriptor_pool: pool,
                    ..
                }) => {
                    if let Some(ids) = descriptor_pools.remove(&pool.0) {
                        freed_sets.extend(ids.into_iter().map(|id| (id, index)));
                    }
                }
                _ => {}
            }
        }
        // A recycled pinned id can have belonged to another pool too.
        for record in &self.records {
            if let Command::AllocateCommandBuffers(args) = command(record)? {
                if args
                    .p_command_buffers
                    .iter()
                    .flatten()
                    .any(|id| pinned.contains(&id.0))
                {
                    if let Some(info) = &args.p_allocate_info {
                        pinned_pools.insert(info.command_pool.0);
                    }
                }
            }
        }

        let mut keep = Vec::new();
        keep.try_reserve_exact(self.records.len())
            .map_err(|_| RecipeError::Budget)?;
        keep.resize(self.records.len(), true);
        // Descriptor values are read by GPU execution, never by command-buffer
        // recording. Historical submissions are not replayed. A later write to
        // precisely the same range makes an earlier write unnecessary unless a
        // descriptor copy can read it. Exact ranges also handle writes spanning
        // adjacent bindings without guessing layout geometry. Keep whole records
        // with copies or even one write which is not completely superseded.
        let mut written = BTreeSet::new();
        let mut reset_fences = BTreeSet::new();
        let mut imported_semaphores = BTreeSet::new();
        for (index, record) in self.records.iter().enumerate().rev() {
            let cmd = command(record)?;
            if let Command::ResetFences(args) = &cmd {
                // No submissions/waits are replayed, so earlier resets of the
                // same fences cannot affect another reconstruction command.
                if args
                    .p_fences
                    .iter()
                    .flatten()
                    .all(|id| reset_fences.contains(&(args.device.0, id.0)))
                {
                    keep[index] = false;
                }
                reset_fences.extend(
                    args.p_fences
                        .iter()
                        .flatten()
                        .map(|id| (args.device.0, id.0)),
                );
            }
            if let Command::ImportSemaphoreResourceMESA(args) = &cmd {
                if let Some(info) = &args.p_import_semaphore_resource_info {
                    // Resource 0 only sets the emulated temporary flag. No
                    // external resource identity or GPU operation is involved.
                    if info.resource_id == 0
                        && !imported_semaphores.insert((args.device.0, info.semaphore.0))
                    {
                        keep[index] = false;
                    }
                }
            }
            if let Command::UpdateDescriptorSets(args) = cmd {
                let key = |write: &crate::venus::protocol::VkWriteDescriptorSet<'_>| {
                    (
                        args.device.0,
                        write.dst_set.0,
                        write.dst_binding,
                        write.dst_array_element,
                        write.descriptor_count,
                        write.descriptor_type,
                    )
                };
                let superseded = args.p_descriptor_writes.iter().flatten().all(|write| {
                    !copied_sets.contains(&write.dst_set.0)
                        && (written.contains(&key(write))
                            || freed_sets
                                .get(&write.dst_set.0)
                                .is_some_and(|at| *at > index))
                });
                if args.descriptor_copy_count == 0 && superseded {
                    keep[index] = false;
                }
                written.extend(args.p_descriptor_writes.iter().flatten().map(key));
            }
        }
        let mut buffers: BTreeMap<u64, Recording> = BTreeMap::new();
        let mut pools: BTreeMap<u64, BTreeSet<u64>> = BTreeMap::new();
        for (index, record) in self.records.iter().enumerate() {
            let cmd = command(record)?;
            match &cmd {
                Command::AllocateCommandBuffers(args) => {
                    if let Some(info) = &args.p_allocate_info {
                        for id in args.p_command_buffers.iter().flatten() {
                            if !pinned.contains(&id.0) {
                                buffers.insert(
                                    id.0,
                                    Recording {
                                        pool: info.command_pool.0,
                                        records: Vec::new(),
                                    },
                                );
                                pools.entry(info.command_pool.0).or_default().insert(id.0);
                            }
                        }
                    }
                }
                Command::FreeCommandBuffers(args) => {
                    for id in args.p_command_buffers.iter().flatten() {
                        if let Some(mut old) = buffers.remove(&id.0) {
                            old.discard(&mut keep);
                            if let Some(ids) = pools.get_mut(&old.pool) {
                                ids.remove(&id.0);
                            }
                        }
                    }
                }
                Command::ResetCommandPool(ResetCommandPoolArgs {
                    command_pool: pool, ..
                })
                | Command::DestroyCommandPool(DestroyCommandPoolArgs {
                    command_pool: pool, ..
                }) => {
                    let pool = pool.0;
                    if let Some(ids) = pools.get(&pool) {
                        for id in ids {
                            if let Some(old) = buffers.get_mut(id) {
                                old.discard(&mut keep);
                            }
                        }
                    }
                    if matches!(cmd, Command::DestroyCommandPool(_)) {
                        if let Some(ids) = pools.remove(&pool) {
                            for id in ids {
                                buffers.remove(&id);
                            }
                        }
                    } else if !pinned_pools.contains(&pool) {
                        // All surviving recordings begin from freshly allocated
                        // state. No reset is needed when the old recording was
                        // removed, including pools without RESET_COMMAND_BUFFER.
                        keep[index] = false;
                    }
                }
                _ => {
                    if let Some((Kind::CommandBuffer, id)) = generated::dispatchable(&cmd) {
                        if let Some(old) = buffers.get_mut(&id) {
                            if matches!(
                                cmd,
                                Command::BeginCommandBuffer(_) | Command::ResetCommandBuffer(_)
                            ) {
                                old.discard(&mut keep);
                            }
                            if matches!(cmd, Command::ResetCommandBuffer(_)) {
                                keep[index] = false;
                            } else {
                                old.records
                                    .try_reserve(1)
                                    .map_err(|_| RecipeError::Budget)?;
                                old.records.push(index);
                            }
                        }
                    }
                }
            }
        }
        lifetimes::prune(&self.records, &mut keep)?;
        let before = self.charge.amount();
        // Return unused vector slots too. Charging only surviving records but
        // retaining every context's previous peak capacity defeats the shared
        // storage budget even though the command blobs themselves were freed.
        let mut records = Vec::new();
        records
            .try_reserve_exact(keep.iter().filter(|retain| **retain).count())
            .map_err(|_| RecipeError::Budget)?;
        let mut released = 0;
        for (index, record) in self.records.drain(..).enumerate() {
            if keep[index] {
                records.push(record);
            } else {
                released += record.len() as u64 + RECORD_COST - GENERATION_BYTES;
                let serial = Decoder::new(&record).u64()?;
                if serial != 0 && self.imports.remove(&serial).is_some() {
                    released += crate::venus::executor::checkpoint_imports::META_COST;
                }
            }
        }
        self.records = records;
        self.charge.shrink_to(before - released);
        self.next_compaction = self.charge.amount().saturating_add(COMPACT_INTERVAL);
        tracing::debug!(
            before,
            after = self.charge.amount(),
            "compacted Venus reconstruction recordings"
        );
        if tracing::enabled!(tracing::Level::DEBUG) {
            let mut sizes: BTreeMap<&str, (u64, usize)> = BTreeMap::new();
            for record in &self.records {
                let cmd = command(record)?;
                let value = sizes.entry(cmd.name()).or_default();
                value.0 += record.len() as u64 + RECORD_COST - GENERATION_BYTES;
                value.1 += 1;
            }
            let mut sizes: Vec<_> = sizes.into_iter().collect();
            sizes.sort_unstable_by_key(|(_, (bytes, _))| std::cmp::Reverse(*bytes));
            sizes.truncate(8);
            tracing::debug!(
                ?sizes,
                secondary_ids = pinned.len(),
                "retained Venus recipe storage"
            );
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::venus::executor::{fake::FakeVulkan, harness as h, recording as r};
    use crate::venus::protocol::*;

    fn run(context: &mut VulkanContext<FakeVulkan>, mut command: Command<'_>) {
        context.execute(&mut command).unwrap();
        assert!(crate::venus::executor::context::result_of(&command).is_none_or(|v| v >= 0));
    }

    fn context(budget: &Arc<PageBudget>, pool_flags: u32) -> VulkanContext<FakeVulkan> {
        let mut context = VulkanContext::new(1, Arc::new(FakeVulkan::standard()));
        context.enable_object_recipes(budget).unwrap();
        run(&mut context, h::create_instance(h::INSTANCE));
        run(
            &mut context,
            h::enumerate(h::INSTANCE, Some(vec![h::PHYSICAL])),
        );
        run(
            &mut context,
            h::create_device(h::PHYSICAL, h::DEVICE, Vec::new()),
        );
        let mut pool = h::create_pool(h::DEVICE, h::POOL);
        if let Command::CreateCommandPool(args) = &mut pool {
            args.p_create_info.as_mut().unwrap().flags = pool_flags;
        }
        run(&mut context, pool);
        run(
            &mut context,
            h::create_buffer(
                h::DEVICE,
                h::BUFFER,
                h::buffer_info(65536, h::TRANSFER | 0x20),
            ),
        );
        run(
            &mut context,
            r::allocate_cbs(h::DEVICE, h::POOL, &[r::CB], false),
        );
        context
    }

    fn reset_pool() -> Command<'static> {
        Command::ResetCommandPool(ResetCommandPoolArgs {
            device: VkDevice(h::DEVICE),
            command_pool: VkCommandPool(h::POOL),
            flags: 0,
            ret: 0,
        })
    }

    #[test]
    fn more_than_64_mib_of_replaced_recordings_restore_and_keep_compacting() {
        for pool_reset in [false, true] {
            let budget = PageBudget::new(MAX_RECIPE_BYTES);
            let mut original = context(&budget, if pool_reset { 0 } else { 2 });
            // More historical bytes than the unchanged hard budget, through
            // both implicit Begin resets and pools without individual resets.
            static DATA: [u8; 65536] = [0x53; 65536];
            for _ in 0..1120 {
                if pool_reset {
                    run(&mut original, reset_pool());
                }
                run(&mut original, r::begin(r::CB));
                run(&mut original, r::update(r::CB, h::BUFFER, 0, &DATA));
                run(&mut original, r::end(r::CB));
            }
            assert!(budget.used() < 2 * COMPACT_INTERVAL);
            let journal = original.recipes.as_mut().unwrap();
            journal.compact_recordings().unwrap();
            assert!(journal.charge.amount() < 100_000);
            let bytes = original.save_object_recipes().unwrap();
            original.destroy_all();
            let mut restored = VulkanContext::new(1, Arc::new(FakeVulkan::standard()));
            restored.restore_object_recipes(&bytes, &budget).unwrap();
            run(&mut restored, reset_pool());
            run(&mut restored, r::begin(r::CB));
            run(&mut restored, r::end(r::CB));
            restored
                .recipes
                .as_mut()
                .unwrap()
                .compact_recordings()
                .unwrap();
            let again = restored.save_object_recipes().unwrap();
            restored.destroy_all();
            let mut restored = VulkanContext::new(1, Arc::new(FakeVulkan::standard()));
            restored.restore_object_recipes(&again, &budget).unwrap();
            restored.destroy_all();
            assert_eq!(budget.used(), 0);
        }
    }

    #[test]
    fn secondary_recordings_and_pool_resets_are_preserved_including_recycled_ids() {
        let budget = PageBudget::new(MAX_RECIPE_BYTES);
        let mut journal = RecipeJournal::new(&budget);
        let commands = [
            r::allocate_cbs(h::DEVICE, h::POOL, &[r::CB], true),
            r::begin(r::CB),
            r::end(r::CB),
            r::allocate_cbs(h::DEVICE, h::POOL + 2, &[r::CB + 1], false),
            r::begin(r::CB + 1),
            Command::CmdExecuteCommands(CmdExecuteCommandsArgs {
                command_buffer: VkCommandBuffer(r::CB + 1),
                command_buffer_count: 1,
                p_command_buffers: Some(vec![VkCommandBuffer(r::CB)]),
            }),
            r::end(r::CB + 1),
            reset_pool(),
            r::begin(r::CB),
            r::end(r::CB),
            Command::FreeCommandBuffers(FreeCommandBuffersArgs {
                device: VkDevice(h::DEVICE),
                command_pool: VkCommandPool(h::POOL),
                command_buffer_count: 1,
                p_command_buffers: Some(vec![VkCommandBuffer(r::CB)]),
            }),
            r::allocate_cbs(h::DEVICE, h::POOL + 1, &[r::CB], false),
            r::begin(r::CB),
            r::end(r::CB),
        ];
        for cmd in commands {
            journal.note(encode_recipe(&cmd), &cmd);
        }
        let before = journal.encode().unwrap();
        journal.compact_recordings().unwrap();
        assert_eq!(journal.encode().unwrap(), before);
    }

    #[test]
    fn a_partially_recorded_buffer_can_finish_after_restore() {
        let budget = PageBudget::new(MAX_RECIPE_BYTES);
        let mut original = context(&budget, 2);
        run(&mut original, r::begin(r::CB));
        run(&mut original, r::end(r::CB));
        run(&mut original, r::begin(r::CB));
        run(
            &mut original,
            r::fill(r::CB, h::BUFFER, 0, 65536, 0x12345678),
        );
        original
            .recipes
            .as_mut()
            .unwrap()
            .compact_recordings()
            .unwrap();
        let bytes = original.save_object_recipes().unwrap();
        original.destroy_all();
        let mut restored = VulkanContext::new(1, Arc::new(FakeVulkan::standard()));
        restored.restore_object_recipes(&bytes, &budget).unwrap();
        run(&mut restored, r::end(r::CB));
        restored.destroy_all();
        assert_eq!(budget.used(), 0);
    }

    #[test]
    fn a_final_reset_or_free_removes_recordings_but_preserves_object_lifetimes() {
        for ending in 0..3 {
            let budget = PageBudget::new(MAX_RECIPE_BYTES);
            let mut original = context(&budget, 2);
            run(&mut original, r::begin(r::CB));
            run(&mut original, r::end(r::CB));
            if ending == 1 {
                run(
                    &mut original,
                    Command::FreeCommandBuffers(FreeCommandBuffersArgs {
                        device: VkDevice(h::DEVICE),
                        command_pool: VkCommandPool(h::POOL),
                        command_buffer_count: 1,
                        p_command_buffers: Some(vec![VkCommandBuffer(r::CB)]),
                    }),
                );
                run(
                    &mut original,
                    r::allocate_cbs(h::DEVICE, h::POOL, &[r::CB], false),
                );
            } else if ending == 2 {
                run(
                    &mut original,
                    Command::DestroyCommandPool(DestroyCommandPoolArgs {
                        device: VkDevice(h::DEVICE),
                        command_pool: VkCommandPool(h::POOL),
                    }),
                );
                run(&mut original, h::create_pool(h::DEVICE, h::POOL));
                run(
                    &mut original,
                    r::allocate_cbs(h::DEVICE, h::POOL, &[r::CB], false),
                );
            } else {
                run(
                    &mut original,
                    Command::ResetCommandBuffer(ResetCommandBufferArgs {
                        command_buffer: VkCommandBuffer(r::CB),
                        flags: 0,
                        ret: 0,
                    }),
                );
            }
            original
                .recipes
                .as_mut()
                .unwrap()
                .compact_recordings()
                .unwrap();
            let bytes = original.save_object_recipes().unwrap();
            original.destroy_all();
            let mut original = VulkanContext::new(1, Arc::new(FakeVulkan::standard()));
            original.restore_object_recipes(&bytes, &budget).unwrap();
            run(&mut original, r::begin(r::CB));
            run(&mut original, r::end(r::CB));
            original.destroy_all();
        }
    }

    #[test]
    fn shared_budget_pressure_compacts_early_but_never_truncates_secondary_history() {
        let limit = 1 << 20;
        let budget = PageBudget::new(limit);
        let mut original = context(&budget, 2);
        static DATA: [u8; 65536] = [0x57; 65536];
        for _ in 0..50 {
            run(&mut original, r::begin(r::CB));
            run(&mut original, r::update(r::CB, h::BUFFER, 0, &DATA));
            run(&mut original, r::end(r::CB));
        }
        assert!(budget.used() < limit);
        assert!(original.save_object_recipes().is_ok());
        original.destroy_all();
        assert_eq!(budget.used(), 0);

        let mut journal = RecipeJournal::new(&budget);
        let allocate = r::allocate_cbs(h::DEVICE, h::POOL, &[r::CB], true);
        journal.note(encode_recipe(&allocate), &allocate);
        for _ in 0..50 {
            for cmd in [
                r::begin(r::CB),
                r::update(r::CB, h::BUFFER, 0, &DATA),
                r::end(r::CB),
            ] {
                journal.note(encode_recipe(&cmd), &cmd);
            }
        }
        assert!(!journal.recording());
        assert!(journal.encode().is_err());
        assert_eq!(budget.used(), 0);
    }

    fn descriptor_context(budget: &Arc<PageBudget>) -> VulkanContext<FakeVulkan> {
        let mut context = context(budget, 2);
        run(
            &mut context,
            h::create_buffer(
                h::DEVICE,
                h::BUFFER + 1,
                h::buffer_info(65536, h::TRANSFER | 0x20),
            ),
        );
        run(
            &mut context,
            r::create_storage_set_layout(h::DEVICE, r::SET_LAYOUT, r::STAGE_COMPUTE),
        );
        run(
            &mut context,
            r::create_descriptor_pool(h::DEVICE, r::DESC_POOL, 1),
        );
        run(
            &mut context,
            r::allocate_sets(
                h::DEVICE,
                r::DESC_POOL,
                r::SET_LAYOUT,
                &[r::SET, r::SET + 1],
            ),
        );
        context
    }

    fn descriptor_writes(journal: &RecipeJournal) -> Vec<(u64, u64)> {
        journal
            .records
            .iter()
            .flat_map(|record| {
                if let Command::UpdateDescriptorSets(args) = command(record).unwrap() {
                    args.p_descriptor_writes
                        .into_iter()
                        .flatten()
                        .map(|write| (write.dst_set.0, write.p_buffer_info.unwrap()[0].buffer.0))
                        .collect::<Vec<_>>()
                } else {
                    Vec::new()
                }
            })
            .collect()
    }

    #[test]
    fn transient_pools_and_their_explicit_or_implicit_frees_do_not_accumulate() {
        let budget = PageBudget::new(16 << 10);
        let mut original = descriptor_context(&budget);
        let objects = original.object_count();
        for _ in 0..1000 {
            for command in [
                h::create_pool(h::DEVICE, h::POOL + 1),
                r::allocate_cbs(h::DEVICE, h::POOL + 1, &[r::CB + 1, r::CB + 2], false),
                r::begin(r::CB + 1),
                r::update(r::CB + 1, h::BUFFER, 0, &[1, 2, 3, 4]),
                r::end(r::CB + 1),
                Command::FreeCommandBuffers(FreeCommandBuffersArgs {
                    device: VkDevice(h::DEVICE),
                    command_pool: VkCommandPool(h::POOL + 1),
                    command_buffer_count: 1,
                    p_command_buffers: Some(vec![VkCommandBuffer(r::CB + 1)]),
                }),
                Command::DestroyCommandPool(DestroyCommandPoolArgs {
                    device: VkDevice(h::DEVICE),
                    command_pool: VkCommandPool(h::POOL + 1),
                }),
                r::create_descriptor_pool(h::DEVICE, r::DESC_POOL + 100, 1),
                r::allocate_sets(
                    h::DEVICE,
                    r::DESC_POOL + 100,
                    r::SET_LAYOUT,
                    &[r::SET + 10, r::SET + 11],
                ),
                r::write_storage(h::DEVICE, r::SET + 10, h::BUFFER),
                Command::FreeDescriptorSets(FreeDescriptorSetsArgs {
                    device: VkDevice(h::DEVICE),
                    descriptor_pool: VkDescriptorPool(r::DESC_POOL + 100),
                    descriptor_set_count: 1,
                    p_descriptor_sets: Some(vec![VkDescriptorSet(r::SET + 10)]),
                    ret: VK_SUCCESS,
                }),
                Command::ResetDescriptorPool(ResetDescriptorPoolArgs {
                    device: VkDevice(h::DEVICE),
                    descriptor_pool: VkDescriptorPool(r::DESC_POOL + 100),
                    flags: 0,
                    ret: VK_SUCCESS,
                }),
                r::allocate_sets(h::DEVICE, r::DESC_POOL + 100, r::SET_LAYOUT, &[r::SET + 11]),
                Command::DestroyDescriptorPool(DestroyDescriptorPoolArgs {
                    device: VkDevice(h::DEVICE),
                    descriptor_pool: VkDescriptorPool(r::DESC_POOL + 100),
                }),
            ] {
                run(&mut original, command);
            }
        }
        original
            .recipes
            .as_mut()
            .unwrap()
            .compact_recordings()
            .unwrap();
        assert!(budget.used() < 4096);
        let bytes = original.save_object_recipes().unwrap();
        original.destroy_all();
        let mut restored = VulkanContext::new(1, Arc::new(FakeVulkan::standard()));
        restored.restore_object_recipes(&bytes, &budget).unwrap();
        assert_eq!(restored.object_count(), objects);
        restored.destroy_all();
    }

    #[test]
    fn pooled_batch_keeps_a_retired_sibling_free_when_another_output_is_live() {
        let budget = PageBudget::new(MAX_RECIPE_BYTES);
        let mut original = descriptor_context(&budget);
        run(
            &mut original,
            r::allocate_cbs(h::DEVICE, h::POOL, &[r::CB + 1, r::CB + 2], false),
        );
        run(
            &mut original,
            Command::FreeCommandBuffers(FreeCommandBuffersArgs {
                device: VkDevice(h::DEVICE),
                command_pool: VkCommandPool(h::POOL),
                command_buffer_count: 1,
                p_command_buffers: Some(vec![VkCommandBuffer(r::CB + 1)]),
            }),
        );
        run(&mut original, r::begin(r::CB + 2));
        run(&mut original, r::end(r::CB + 2));
        run(
            &mut original,
            Command::FreeDescriptorSets(FreeDescriptorSetsArgs {
                device: VkDevice(h::DEVICE),
                descriptor_pool: VkDescriptorPool(r::DESC_POOL),
                descriptor_set_count: 1,
                p_descriptor_sets: Some(vec![VkDescriptorSet(r::SET)]),
                ret: VK_SUCCESS,
            }),
        );
        original
            .recipes
            .as_mut()
            .unwrap()
            .compact_recordings()
            .unwrap();
        let objects = original.object_count();
        let bytes = original.save_object_recipes().unwrap();
        original.destroy_all();
        let mut restored = VulkanContext::new(1, Arc::new(FakeVulkan::standard()));
        restored.restore_object_recipes(&bytes, &budget).unwrap();
        assert_eq!(restored.object_count(), objects);
        assert_eq!(restored.save_object_recipes().unwrap(), bytes);
        restored.destroy_all();
    }

    #[test]
    fn overwritten_and_freed_descriptors_compact_without_changing_reused_set_values() {
        let budget = PageBudget::new(16 << 10);
        let mut original = descriptor_context(&budget);
        // The shared limit is far below the compaction interval. Repeated
        // descriptor updates alone must not disable checkpointing.
        for _ in 0..2000 {
            run(
                &mut original,
                r::write_storage(h::DEVICE, r::SET, h::BUFFER),
            );
        }
        run(
            &mut original,
            r::write_storage(h::DEVICE, r::SET, h::BUFFER + 1),
        );
        let journal = original.recipes.as_mut().unwrap();
        journal.compact_recordings().unwrap();
        assert_eq!(descriptor_writes(journal), vec![(r::SET, h::BUFFER + 1)]);

        run(
            &mut original,
            Command::ResetDescriptorPool(ResetDescriptorPoolArgs {
                device: VkDevice(h::DEVICE),
                descriptor_pool: VkDescriptorPool(r::DESC_POOL),
                flags: 0,
                ret: 0,
            }),
        );
        run(
            &mut original,
            r::allocate_sets(h::DEVICE, r::DESC_POOL, r::SET_LAYOUT, &[r::SET]),
        );
        run(
            &mut original,
            r::write_storage(h::DEVICE, r::SET, h::BUFFER),
        );
        original
            .recipes
            .as_mut()
            .unwrap()
            .compact_recordings()
            .unwrap();
        assert_eq!(
            descriptor_writes(original.recipes.as_ref().unwrap()),
            vec![(r::SET, h::BUFFER)]
        );
        let bytes = original.save_object_recipes().unwrap();
        original.destroy_all();
        let mut restored = VulkanContext::new(1, Arc::new(FakeVulkan::standard()));
        restored.restore_object_recipes(&bytes, &budget).unwrap();
        assert_eq!(
            descriptor_writes(restored.recipes.as_ref().unwrap()),
            vec![(r::SET, h::BUFFER)]
        );
        restored.destroy_all();
        assert_eq!(budget.used(), 0);
    }

    #[test]
    fn descriptor_copies_keep_the_source_values_at_the_time_of_the_copy() {
        let budget = PageBudget::new(MAX_RECIPE_BYTES);
        let mut original = descriptor_context(&budget);
        run(
            &mut original,
            r::write_storage(h::DEVICE, r::SET, h::BUFFER),
        );
        run(
            &mut original,
            Command::UpdateDescriptorSets(UpdateDescriptorSetsArgs {
                device: VkDevice(h::DEVICE),
                descriptor_write_count: 0,
                p_descriptor_writes: None,
                descriptor_copy_count: 1,
                p_descriptor_copies: Some(vec![VkCopyDescriptorSet {
                    src_set: VkDescriptorSet(r::SET),
                    src_binding: 0,
                    src_array_element: 0,
                    dst_set: VkDescriptorSet(r::SET + 1),
                    dst_binding: 0,
                    dst_array_element: 0,
                    descriptor_count: 1,
                }]),
            }),
        );
        run(
            &mut original,
            r::write_storage(h::DEVICE, r::SET, h::BUFFER + 1),
        );
        // The historical copy still needs its old source generation. Keep
        // the implicit frees before these pooled guest handles are reused.
        run(
            &mut original,
            Command::ResetDescriptorPool(ResetDescriptorPoolArgs {
                device: VkDevice(h::DEVICE),
                descriptor_pool: VkDescriptorPool(r::DESC_POOL),
                flags: 0,
                ret: VK_SUCCESS,
            }),
        );
        run(
            &mut original,
            r::allocate_sets(
                h::DEVICE,
                r::DESC_POOL,
                r::SET_LAYOUT,
                &[r::SET, r::SET + 1],
            ),
        );
        let before = original.save_object_recipes().unwrap();
        original
            .recipes
            .as_mut()
            .unwrap()
            .compact_recordings()
            .unwrap();
        assert_eq!(original.save_object_recipes().unwrap(), before);
        original.destroy_all();
        let mut restored = VulkanContext::new(1, Arc::new(FakeVulkan::standard()));
        restored.restore_object_recipes(&before, &budget).unwrap();
        restored.destroy_all();
    }

    #[test]
    fn repeated_fence_resets_and_temporary_imports_preserve_the_sync_checkpoint() {
        let budget = PageBudget::new(16 << 10);
        let mut original = context(&budget, 2);
        run(&mut original, r::create_fence(h::DEVICE, r::FENCE, false));
        run(
            &mut original,
            r::create_fence(h::DEVICE, r::FENCE + 1, true),
        );
        run(
            &mut original,
            r::create_semaphore(h::DEVICE, r::FENCE + 2, None, 0),
        );
        for _ in 0..2000 {
            run(
                &mut original,
                r::reset_fences(h::DEVICE, &[r::FENCE, r::FENCE + 1]),
            );
            run(
                &mut original,
                r::import_semaphore_resource(h::DEVICE, r::FENCE + 2, 0),
            );
        }
        let timeout = std::time::Duration::from_secs(2);
        let sync = original.save_sync_state(timeout).unwrap();
        original
            .recipes
            .as_mut()
            .unwrap()
            .compact_recordings()
            .unwrap();
        let journal = original.recipes.as_ref().unwrap();
        let names: Vec<_> = journal
            .records
            .iter()
            .map(|record| command(record).unwrap().name())
            .collect();
        assert_eq!(
            names
                .iter()
                .filter(|name| **name == "vkResetFences")
                .count(),
            1
        );
        assert_eq!(
            names
                .iter()
                .filter(|name| **name == "vkImportSemaphoreResourceMESA")
                .count(),
            1
        );
        let bytes = original.save_object_recipes().unwrap();
        original.destroy_all();
        let mut restored = VulkanContext::new(1, Arc::new(FakeVulkan::standard()));
        restored.restore_object_recipes(&bytes, &budget).unwrap();
        restored.restore_sync_state(&sync, timeout).unwrap();
        assert_eq!(restored.save_sync_state(timeout).unwrap(), sync);
        restored.destroy_all();
        assert_eq!(budget.used(), 0);
    }

    fn import_journal(
        budget: &Arc<PageBudget>,
    ) -> (
        RecipeJournal,
        super::super::super::checkpoint_imports::ImportRecipe,
    ) {
        let mut journal = RecipeJournal::new(budget);
        let allocate = r::allocate_cbs(h::DEVICE, h::POOL, &[r::CB], false);
        journal.note(encode_recipe(&allocate), &allocate);
        for _ in 0..100 {
            for cmd in [r::begin(r::CB), r::end(r::CB)] {
                journal.note(encode_recipe(&cmd), &cmd);
            }
        }
        let allocate = h::allocate(
            h::DEVICE,
            h::MEMORY,
            4096,
            3,
            vec![VkMemoryAllocateInfoNext::VkImportMemoryResourceInfoMESA(
                VkImportMemoryResourceInfoMESA { resource_id: 1 },
            )],
        );
        journal.note(encode_recipe(&allocate), &allocate);
        let meta = super::super::super::checkpoint_imports::ImportRecipe {
            serial: 1,
            device: h::DEVICE,
            memory: h::MEMORY,
            host_size: 4096,
            kind: 2,
        };
        (journal, meta)
    }

    #[test]
    fn import_metadata_pressure_reclaims_history_and_preserves_the_allocation_generation() {
        let limit = 64 << 10;
        let budget = PageBudget::new(limit);
        let (mut journal, meta) = import_journal(&budget);
        let before = journal.charge.amount();
        let previous_capacity = journal.records.capacity();
        // Another context leaves room for the allocation command, but not
        // its 128-byte import metadata. Both belong to one live transaction.
        let other = budget.charge_anyway(limit - before - 64);
        assert!(matches!(
            journal.note_import(meta),
            Err(RecipeError::Budget)
        ));
        journal.note_live_import(meta).unwrap();
        assert!(journal.charge.amount() < before);
        assert!(journal.records.capacity() < previous_capacity);
        assert_eq!(journal.imports.len(), 1);
        let bytes = journal.encode().unwrap();
        drop(other);
        drop(journal);
        let decoded = RecipeJournal::decode(&bytes, &budget).unwrap();
        assert_eq!(decoded.imports[&1].memory, h::MEMORY);
        assert_eq!(decoded.imports[&1].host_size, 4096);
        drop(decoded);
        assert_eq!(budget.used(), 0);
    }

    #[test]
    fn archive_import_metadata_cannot_compact_away_commands_before_semantic_replay() {
        let budget = PageBudget::new(64 << 10);
        let (mut journal, meta) = import_journal(&budget);
        journal.note_import(meta).unwrap();
        let bytes = journal.encode().unwrap();
        let small = PageBudget::new(journal.charge.amount() - 64);
        assert!(matches!(
            RecipeJournal::decode(&bytes, &small),
            Err(RecipeError::Budget)
        ));
        assert_eq!(small.used(), 0);
        journal.fail(RecipeError::Budget);
        assert_eq!(journal.records.capacity(), 0);
        assert_eq!(budget.used(), 0);
    }

    #[test]
    fn transient_buffers_do_not_exhaust_retained_history_and_live_generations_restore() {
        let budget = PageBudget::new(16 << 10);
        let mut original = context(&budget, 2);
        let live_count = original.object_count();
        for _ in 0..2000 {
            run(
                &mut original,
                h::create_buffer(h::DEVICE, h::BUFFER + 10, h::buffer_info(4096, h::TRANSFER)),
            );
            run(&mut original, h::destroy_buffer(h::DEVICE, h::BUFFER + 10));
        }
        assert_eq!(original.object_count(), live_count);
        let bytes = original.save_object_recipes().unwrap();
        original.destroy_all();
        let mut restored = VulkanContext::new(1, Arc::new(FakeVulkan::standard()));
        restored.restore_object_recipes(&bytes, &budget).unwrap();
        assert_eq!(restored.object_count(), live_count);
        restored.destroy_all();
        assert_eq!(budget.used(), 0);
    }

    #[test]
    fn a_recording_keeps_its_destroyed_buffer_until_it_is_replaced() {
        let budget = PageBudget::new(MAX_RECIPE_BYTES);
        let mut original = context(&budget, 2);
        let id = h::BUFFER + 10;
        run(
            &mut original,
            h::create_buffer(h::DEVICE, id, h::buffer_info(4096, h::TRANSFER)),
        );
        run(&mut original, r::begin(r::CB));
        run(&mut original, r::fill(r::CB, id, 0, 4096, 0x12345678));
        run(&mut original, r::end(r::CB));
        run(&mut original, h::destroy_buffer(h::DEVICE, id));
        original
            .recipes
            .as_mut()
            .unwrap()
            .compact_recordings()
            .unwrap();
        let still_referenced = original.save_object_recipes().unwrap();
        let journal = original.recipes.as_ref().unwrap();
        assert!(journal.records.iter().any(|r| matches!(command(r).unwrap(), Command::CreateBuffer(a) if a.p_buffer == Some(VkBuffer(id)))));
        let mut restored = VulkanContext::new(1, Arc::new(FakeVulkan::standard()));
        restored
            .restore_object_recipes(&still_referenced, &budget)
            .unwrap();
        restored.destroy_all();
        run(&mut original, r::begin(r::CB));
        run(&mut original, r::end(r::CB));
        original
            .recipes
            .as_mut()
            .unwrap()
            .compact_recordings()
            .unwrap();
        let journal = original.recipes.as_ref().unwrap();
        assert!(!journal.records.iter().any(|r| matches!(command(r).unwrap(), Command::CreateBuffer(a) if a.p_buffer == Some(VkBuffer(id)))));
        original.destroy_all();
    }

    #[test]
    fn a_live_pipeline_keeps_its_dead_shader_and_batch_siblings_with_original_cleanup() {
        let budget = PageBudget::new(MAX_RECIPE_BYTES);
        let mut original = context(&budget, 2);
        run(
            &mut original,
            r::create_shader_module(h::DEVICE, r::SHADER, &[0x07230203, 0x10000, 0, 1, 0]),
        );
        run(
            &mut original,
            r::create_pipeline_layout(h::DEVICE, r::PIPELINE_LAYOUT, &[]),
        );
        let mut batch =
            r::create_compute_pipeline(h::DEVICE, r::PIPELINE, r::SHADER, r::PIPELINE_LAYOUT);
        if let Command::CreateComputePipelines(a) = &mut batch {
            a.create_info_count = 2;
            let info = a.p_create_infos.as_ref().unwrap()[0].clone();
            a.p_create_infos.as_mut().unwrap().push(info);
            a.p_pipelines = Some(vec![VkPipeline(r::PIPELINE), VkPipeline(r::PIPELINE + 1)]);
        }
        run(&mut original, batch);
        run(
            &mut original,
            Command::DestroyShaderModule(DestroyShaderModuleArgs {
                device: VkDevice(h::DEVICE),
                shader_module: VkShaderModule(r::SHADER),
            }),
        );
        run(
            &mut original,
            Command::DestroyPipeline(DestroyPipelineArgs {
                device: VkDevice(h::DEVICE),
                pipeline: VkPipeline(r::PIPELINE),
            }),
        );
        let count = original.object_count();
        original
            .recipes
            .as_mut()
            .unwrap()
            .compact_recordings()
            .unwrap();
        let bytes = original.save_object_recipes().unwrap();
        let mut restored = VulkanContext::new(1, Arc::new(FakeVulkan::standard()));
        restored.restore_object_recipes(&bytes, &budget).unwrap();
        assert_eq!(restored.object_count(), count);
        restored.destroy_all();
        run(
            &mut original,
            Command::DestroyPipeline(DestroyPipelineArgs {
                device: VkDevice(h::DEVICE),
                pipeline: VkPipeline(r::PIPELINE + 1),
            }),
        );
        run(
            &mut original,
            Command::DestroyPipelineLayout(DestroyPipelineLayoutArgs {
                device: VkDevice(h::DEVICE),
                pipeline_layout: VkPipelineLayout(r::PIPELINE_LAYOUT),
            }),
        );
        original
            .recipes
            .as_mut()
            .unwrap()
            .compact_recordings()
            .unwrap();
        assert!(!original
            .recipes
            .as_ref()
            .unwrap()
            .records
            .iter()
            .any(|r| matches!(
                command(r).unwrap(),
                Command::CreateComputePipelines(_) | Command::CreateShaderModule(_)
            )));
        original.destroy_all();
    }

    #[test]
    fn live_pipeline_caches_keep_the_builds_that_modified_them() {
        let budget = PageBudget::new(MAX_RECIPE_BYTES);
        let mut original = context(&budget, 2);
        let cache = 0x8900;
        run(
            &mut original,
            Command::CreatePipelineCache(CreatePipelineCacheArgs {
                device: VkDevice(h::DEVICE),
                p_create_info: Some(Default::default()),
                p_pipeline_cache: Some(VkPipelineCache(cache)),
                ret: 0,
            }),
        );
        run(
            &mut original,
            r::create_shader_module(h::DEVICE, r::SHADER, &[0x07230203, 0x10000, 0, 1, 0]),
        );
        run(
            &mut original,
            r::create_pipeline_layout(h::DEVICE, r::PIPELINE_LAYOUT, &[]),
        );
        let mut pipeline =
            r::create_compute_pipeline(h::DEVICE, r::PIPELINE, r::SHADER, r::PIPELINE_LAYOUT);
        if let Command::CreateComputePipelines(a) = &mut pipeline {
            a.pipeline_cache = VkPipelineCache(cache);
        }
        run(&mut original, pipeline);
        run(
            &mut original,
            Command::DestroyPipeline(DestroyPipelineArgs {
                device: VkDevice(h::DEVICE),
                pipeline: VkPipeline(r::PIPELINE),
            }),
        );
        run(
            &mut original,
            Command::DestroyShaderModule(DestroyShaderModuleArgs {
                device: VkDevice(h::DEVICE),
                shader_module: VkShaderModule(r::SHADER),
            }),
        );
        original
            .recipes
            .as_mut()
            .unwrap()
            .compact_recordings()
            .unwrap();
        assert!(original
            .recipes
            .as_ref()
            .unwrap()
            .records
            .iter()
            .any(|r| matches!(command(r).unwrap(), Command::CreateComputePipelines(_))));
        let bytes = original.save_object_recipes().unwrap();
        let mut restored = VulkanContext::new(1, Arc::new(FakeVulkan::standard()));
        restored.restore_object_recipes(&bytes, &budget).unwrap();
        restored.destroy_all();
        run(
            &mut original,
            Command::DestroyPipelineCache(DestroyPipelineCacheArgs {
                device: VkDevice(h::DEVICE),
                pipeline_cache: VkPipelineCache(cache),
            }),
        );
        run(
            &mut original,
            Command::DestroyPipelineLayout(DestroyPipelineLayoutArgs {
                device: VkDevice(h::DEVICE),
                pipeline_layout: VkPipelineLayout(r::PIPELINE_LAYOUT),
            }),
        );
        original
            .recipes
            .as_mut()
            .unwrap()
            .compact_recordings()
            .unwrap();
        assert!(!original
            .recipes
            .as_ref()
            .unwrap()
            .records
            .iter()
            .any(|r| matches!(
                command(r).unwrap(),
                Command::CreateComputePipelines(_)
                    | Command::CreateShaderModule(_)
                    | Command::CreatePipelineCache(_)
            )));
        original.destroy_all();
    }

    #[test]
    fn freed_allocations_compact_without_renumbering_survivors_or_future_allocations() {
        let budget = PageBudget::new(16 << 10);
        let mut original = context(&budget, 2);
        for _ in 0..2000 {
            run(
                &mut original,
                h::allocate(h::DEVICE, h::MEMORY, 4096, 1, Vec::new()),
            );
            run(&mut original, h::free(h::DEVICE, h::MEMORY));
        }
        run(
            &mut original,
            h::allocate(h::DEVICE, h::MEMORY, 4096, 1, Vec::new()),
        );
        original
            .recipes
            .as_mut()
            .unwrap()
            .compact_recordings()
            .unwrap();
        assert_eq!(
            original
                .objects
                .memory(h::DEVICE, h::MEMORY)
                .unwrap()
                .allocation_serial,
            2001
        );
        let bytes = original.save_object_recipes().unwrap();
        original.destroy_all();
        let mut restored = VulkanContext::new(1, Arc::new(FakeVulkan::standard()));
        restored.restore_object_recipes(&bytes, &budget).unwrap();
        assert_eq!(restored.memory_sequence, 2001);
        assert_eq!(
            restored
                .objects
                .memory(h::DEVICE, h::MEMORY)
                .unwrap()
                .allocation_serial,
            2001
        );
        run(
            &mut restored,
            h::allocate(h::DEVICE, h::MEMORY + 1, 4096, 1, Vec::new()),
        );
        assert_eq!(
            restored
                .objects
                .memory(h::DEVICE, h::MEMORY + 1)
                .unwrap()
                .allocation_serial,
            2002
        );
        restored.destroy_all();
    }

    #[test]
    fn allocation_refusals_do_not_poison_a_later_snapshot_or_reuse_their_ordinal() {
        let budget = PageBudget::new(MAX_RECIPE_BYTES);
        let mut original = context(&budget, 2);
        let other = original
            .budget
            .charge_anyway(original.budget.limit() - 4096);
        let mut refused = h::allocate(h::DEVICE, h::MEMORY, 8192, 3, Vec::new());
        original.execute(&mut refused).unwrap();
        assert!(matches!(
            crate::venus::executor::context::result_of(&refused),
            Some(VK_ERROR_OUT_OF_HOST_MEMORY | VK_ERROR_OUT_OF_DEVICE_MEMORY)
        ));
        drop(other);
        run(
            &mut original,
            h::allocate(h::DEVICE, h::MEMORY, 4096, 3, Vec::new()),
        );
        let bytes = original.save_object_recipes().unwrap();
        original.destroy_all();
        let mut restored = VulkanContext::new(1, Arc::new(FakeVulkan::standard()));
        restored.restore_object_recipes(&bytes, &budget).unwrap();
        assert_eq!(
            restored
                .objects
                .memory(h::DEVICE, h::MEMORY)
                .unwrap()
                .allocation_serial,
            2
        );
        assert_eq!(restored.memory_sequence, 2);
        restored.destroy_all();
    }

    #[test]
    fn fully_retired_allocations_preserve_the_next_generation_after_restore() {
        let budget = PageBudget::new(MAX_RECIPE_BYTES);
        let mut original = context(&budget, 2);
        for _ in 0..128 {
            run(
                &mut original,
                h::allocate(h::DEVICE, h::MEMORY, 4096, 1, Vec::new()),
            );
            run(&mut original, h::free(h::DEVICE, h::MEMORY));
        }
        original
            .recipes
            .as_mut()
            .unwrap()
            .compact_recordings()
            .unwrap();
        assert!(original
            .recipes
            .as_ref()
            .unwrap()
            .records
            .iter()
            .all(|record| { !matches!(command(record).unwrap(), Command::AllocateMemory(_)) }));
        let bytes = original.save_object_recipes().unwrap();
        original.destroy_all();
        let mut restored = VulkanContext::new(1, Arc::new(FakeVulkan::standard()));
        restored.restore_object_recipes(&bytes, &budget).unwrap();
        assert_eq!(restored.memory_sequence, 128);
        run(
            &mut restored,
            h::allocate(h::DEVICE, h::MEMORY, 4096, 1, Vec::new()),
        );
        assert_eq!(
            restored
                .objects
                .memory(h::DEVICE, h::MEMORY)
                .unwrap()
                .allocation_serial,
            129
        );
        restored.destroy_all();
    }

    #[test]
    fn corrupt_allocation_generations_are_rejected_before_native_replay() {
        let budget = PageBudget::new(MAX_RECIPE_BYTES);
        let mut original = context(&budget, 2);
        for memory in [h::MEMORY, h::MEMORY + 1] {
            run(
                &mut original,
                h::allocate(h::DEVICE, memory, 4096, 1, Vec::new()),
            );
        }
        let bytes = original.save_object_recipes().unwrap();
        let mut offset = 16;
        let mut allocations = Vec::new();
        for record in &original.recipes.as_ref().unwrap().records {
            if matches!(command(record).unwrap(), Command::AllocateMemory(_)) {
                allocations.push(offset + 4);
            }
            offset += 4 + record.len().next_multiple_of(4);
        }
        assert_eq!(allocations.len(), 2);
        original.destroy_all();
        for (offset, serial, reason) in [
            (20, 1, "generation on a non-allocation command"),
            (allocations[0], 0, "allocation generations are not ordered"),
            (allocations[0], 3, "allocation generations are not ordered"),
            (allocations[1], 1, "allocation generations are not ordered"),
            (4, 1, "allocation generations are not ordered"),
        ] {
            let mut bad = bytes.clone();
            bad[offset..offset + 8].copy_from_slice(&u64::to_le_bytes(serial));
            let host = Arc::new(FakeVulkan::standard());
            let mut restored = VulkanContext::new(1, Arc::clone(&host));
            assert!(
                matches!(restored.restore_object_recipes(&bad, &budget), Err(RecipeError::Invalid(message)) if message == reason)
            );
            assert_eq!(host.live_objects(), 0);
            assert_eq!(budget.used(), 0);
        }
    }

    #[test]
    fn reclaimed_allocations_do_not_redirect_saved_host_page_links() {
        use crate::venus::executor::checkpoint_pages::HostPageTable;

        let budget = PageBudget::new(MAX_RECIPE_BYTES);
        let mut original = context(&budget, 2);
        for _ in 0..32 {
            run(
                &mut original,
                h::allocate(h::DEVICE, h::MEMORY, 4096, 3, Vec::new()),
            );
            run(&mut original, h::free(h::DEVICE, h::MEMORY));
        }
        run(
            &mut original,
            h::allocate(h::DEVICE, h::MEMORY, 4096, 3, Vec::new()),
        );
        original
            .recipes
            .as_mut()
            .unwrap()
            .compact_recordings()
            .unwrap();
        let links = original.host_allocation_links().unwrap();
        assert_eq!(links.len(), 1);
        assert_eq!(links[0].allocation_serial, 33);
        links[0].pages.write_bytes(17, &[0xa7; 97]).unwrap();
        let mut pages = HostPageTable::default();
        let encoded_links = pages.encode_links(&links).unwrap();
        let mut page_file = Vec::new();
        pages.save(&mut page_file).unwrap();
        let bytes = original.save_object_recipes().unwrap();
        original.destroy_all();
        drop(links);
        drop(pages);

        let loaded = HostPageTable::restore(&page_file, &PageBudget::new(1 << 20)).unwrap();
        let links = loaded.resolve_links(&encoded_links).unwrap();
        let mut restored = VulkanContext::new(1, Arc::new(FakeVulkan::standard()));
        restored
            .restore_object_recipes_with_memory(&bytes, &budget, &[], &links)
            .unwrap();
        let restored_links = restored.host_allocation_links().unwrap();
        assert_eq!(restored_links[0].allocation_serial, 33);
        assert!(Arc::ptr_eq(&restored_links[0].pages, &links[0].pages));
        let mut payload = [0; 97];
        restored_links[0]
            .pages
            .read_bytes(17, &mut payload)
            .unwrap();
        assert_eq!(payload, [0xa7; 97]);
        run(
            &mut restored,
            h::allocate(h::DEVICE, h::MEMORY + 1, 4096, 3, Vec::new()),
        );
        assert_eq!(
            restored
                .objects
                .memory(h::DEVICE, h::MEMORY + 1)
                .unwrap()
                .allocation_serial,
            34
        );
        restored.destroy_all();
    }

    #[test]
    fn legacy_v2_recipes_restore_then_resave_with_explicit_generations() {
        let budget = PageBudget::new(MAX_RECIPE_BYTES);
        let mut original = context(&budget, 2);
        run(
            &mut original,
            h::allocate(h::DEVICE, h::MEMORY, 4096, 1, Vec::new()),
        );
        run(&mut original, h::free(h::DEVICE, h::MEMORY));
        run(
            &mut original,
            h::allocate(h::DEVICE, h::MEMORY, 4096, 1, Vec::new()),
        );
        let journal = original.recipes.as_ref().unwrap();
        let mut legacy = Encoder::new();
        legacy.u32(2).unwrap();
        legacy.u32(journal.records.len() as u32).unwrap();
        for record in &journal.records {
            legacy.u32((record.len() - 8) as u32).unwrap();
            legacy.blob(&record[8..]).unwrap();
        }
        legacy.u32(0).unwrap();
        let legacy = legacy.finish().unwrap();
        original.destroy_all();
        let mut restored = VulkanContext::new(1, Arc::new(FakeVulkan::standard()));
        restored.restore_object_recipes(&legacy, &budget).unwrap();
        assert_eq!(restored.memory_sequence, 2);
        restored
            .recipes
            .as_mut()
            .unwrap()
            .compact_recordings()
            .unwrap();
        let v3 = restored.save_object_recipes().unwrap();
        restored.destroy_all();
        let mut next = VulkanContext::new(1, Arc::new(FakeVulkan::standard()));
        next.restore_object_recipes(&v3, &budget).unwrap();
        assert_eq!(next.memory_sequence, 2);
        assert_eq!(
            next.objects
                .memory(h::DEVICE, h::MEMORY)
                .unwrap()
                .allocation_serial,
            2
        );
        next.destroy_all();
    }

    #[test]
    fn legacy_journal_fits_its_original_core_budget_with_bounded_generation_metadata() {
        let budget = PageBudget::new(MAX_RECIPE_BYTES);
        let mut original = context(&budget, 2);
        for id in 0x9000..0x9400 {
            run(
                &mut original,
                h::create_buffer(h::DEVICE, id, h::buffer_info(4096, h::TRANSFER)),
            );
        }
        let journal = original.recipes.as_ref().unwrap();
        let exact_budget = journal.charge.amount();
        let count = journal.records.len();
        let mut legacy = Encoder::new();
        legacy.u32(2).unwrap();
        legacy.u32(count as u32).unwrap();
        for record in &journal.records {
            legacy.u32((record.len() - 8) as u32).unwrap();
            legacy.blob(&record[8..]).unwrap();
        }
        legacy.u32(0).unwrap();
        let legacy = legacy.finish().unwrap();
        let objects = original.object_count();
        original.destroy_all();
        let exact = PageBudget::new(exact_budget);
        let mut restored = VulkanContext::new(1, Arc::new(FakeVulkan::standard()));
        restored.restore_object_recipes(&legacy, &exact).unwrap();
        assert_eq!(restored.object_count(), objects);
        assert_eq!(exact.used(), exact_budget);
        assert!((count as u64) * GENERATION_BYTES <= MAX_RECIPE_GENERATION_BYTES);
        restored.destroy_all();
        assert_eq!(exact.used(), 0);
    }
}
