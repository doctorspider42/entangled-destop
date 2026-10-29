//! Object/synchronisation part of a whole factory checkpoint. Allocation bytes,
//! page/payload owner tables, directory, mappings and rings remain separate.
//! All guest/ring/presenter writers must be parked throughout capture/restore.
use std::collections::HashSet;
use std::sync::Arc;
use std::time::{Duration, Instant};

use super::checkpoint::RecipeError;
use super::checkpoint_budgets::{BudgetTable, CheckpointContext, ContextBudgetLinks};
use super::checkpoint_pages::HostPageTable;
use super::checkpoint_shared::SharedPayloads;
use super::{ExecutorFactory, HostVulkan, VulkanContext};
use crate::venus::renderer::MAX_VENUS_CONTEXTS;
use crate::venus::shmem::PageBudget;
use crate::venus::wire::{Decoder, Encoder};

const VERSION: u32 = 2;
const MAX_COMPONENT: usize = 68 << 20;
const MAX_BYTES: usize = 512 << 20;
const PARTS: usize = 6;

impl<H: HostVulkan> VulkanContext<H> {
    /// Payload classes whose live data has no durable representation yet.
    /// Reject both capture and cold reconstruction rather than silently reset it.
    pub fn checkpoint_data_refusal(&self) -> Option<&'static str> {
        if self.objects.checkpoint_device_ids().iter().any(|id| {
            self.objects
                .device(*id)
                .is_ok_and(|device| device.group_size != 1)
        }) {
            return Some("multi-device allocation state is not captured");
        }
        if self.objects.count_of(super::objects::Kind::QueryPool) != 0 {
            return Some("query-pool results still need a durable GPU representation");
        }
        if self.device_addresses_exposed {
            return Some("buffer device addresses exposed to the guest are not captured");
        }
        if self.buffer_ownership_transfers {
            return Some("buffer queue-family ownership transfers are not captured");
        }
        if self.objects.checkpoint_images().any(|(_, image)| {
            image.facts.tiling != 1
                && image.bindings.iter().zip(&image.binding_identities).any(
                    |(binding, identity)| {
                        self.objects
                            .memory(image.device, binding.memory)
                            .is_ok_and(|memory| {
                                memory.pages.is_some() && Arc::ptr_eq(identity, &memory.identity)
                            })
                    },
                )
        }) {
            return Some("host-visible optimal image state is not captured");
        }
        None
    }
}

struct ContextRecord {
    id: u32,
    // Budgets, recipes, shared links, host-page links, sync, buffer ownership.
    parts: [Vec<u8>; PARTS],
}

pub struct FactoryObjectCheckpoint {
    contexts: Vec<ContextRecord>,
}

fn invalid(why: &'static str) -> RecipeError {
    RecipeError::Invalid(why)
}

fn remaining(deadline: Instant) -> Result<Duration, RecipeError> {
    deadline
        .checked_duration_since(Instant::now())
        .filter(|duration| !duration.is_zero())
        .ok_or(invalid("factory checkpoint deadline expired"))
}

impl FactoryObjectCheckpoint {
    pub fn context_ids(&self) -> impl Iterator<Item = u32> + '_ {
        self.contexts.iter().map(|context| context.id)
    }

    fn validate(&self) -> Result<(), RecipeError> {
        if self.contexts.len() > MAX_VENUS_CONTEXTS {
            return Err(RecipeError::Budget);
        }
        let mut ids = HashSet::new();
        let mut total = 8usize;
        for context in &self.contexts {
            if context.id == 0 || !ids.insert(context.id) {
                return Err(invalid("duplicate or zero factory context id"));
            }
            total = total
                .checked_add(4 + PARTS * 4)
                .ok_or(RecipeError::Budget)?;
            for part in &context.parts {
                if part.len() > MAX_COMPONENT {
                    return Err(RecipeError::Budget);
                }
                total = total
                    .checked_add(part.len().next_multiple_of(4))
                    .ok_or(RecipeError::Budget)?;
            }
            if total > MAX_BYTES {
                return Err(RecipeError::Budget);
            }
        }
        Ok(())
    }

    pub fn encode(&self) -> Result<Vec<u8>, RecipeError> {
        self.validate()?;
        let mut encoder = Encoder::with_limit(MAX_BYTES);
        encoder.u32(VERSION)?;
        encoder.u32(self.contexts.len() as u32)?;
        for context in &self.contexts {
            encoder.u32(context.id)?;
            for part in &context.parts {
                encoder.u32(part.len() as u32)?;
                encoder.blob(part)?;
            }
        }
        Ok(encoder.finish()?)
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, RecipeError> {
        if bytes.len() > MAX_BYTES {
            return Err(RecipeError::Budget);
        }
        let mut decoder = Decoder::new(bytes);
        let version = decoder.u32()?;
        if version != VERSION {
            return Err(RecipeError::Version(version));
        }
        let count = decoder.u32()? as usize;
        if count > MAX_VENUS_CONTEXTS || count > decoder.remaining() / (4 + PARTS * 4) {
            return Err(RecipeError::Budget);
        }
        // Complete framing is validated before copying any component.
        let mut views = Vec::new();
        views
            .try_reserve_exact(count)
            .map_err(|_| RecipeError::Budget)?;
        let mut ids = HashSet::new();
        for _ in 0..count {
            let id = decoder.u32()?;
            if id == 0 || !ids.insert(id) {
                return Err(invalid("duplicate or zero factory context id"));
            }
            let mut parts = [&[][..]; PARTS];
            for part in &mut parts {
                let len = decoder.u32()? as usize;
                if len > MAX_COMPONENT {
                    return Err(RecipeError::Budget);
                }
                *part = decoder.blob(len)?;
            }
            views.push((id, parts));
        }
        if decoder.remaining() != 0 {
            return Err(invalid("trailing factory object data"));
        }
        let mut contexts = Vec::new();
        contexts
            .try_reserve_exact(count)
            .map_err(|_| RecipeError::Budget)?;
        for (id, views) in views {
            let mut parts = std::array::from_fn(|_| Vec::new());
            for (part, view) in parts.iter_mut().zip(views) {
                part.try_reserve_exact(view.len())
                    .map_err(|_| RecipeError::Budget)?;
                part.extend_from_slice(view);
            }
            contexts.push(ContextRecord { id, parts });
        }
        Ok(Self { contexts })
    }
}

impl<H: HostVulkan> ExecutorFactory<H> {
    pub(crate) fn checkpoint_storage(&self) -> Option<Arc<PageBudget>> {
        self.checkpoint_recipes.clone()
    }

    pub fn checkpoint_context_payload_lengths(&self, id: u32) -> Result<(u64, u64), RecipeError> {
        let context = self
            .contexts
            .get(&id)
            .ok_or(invalid("checkpoint context is absent"))?;
        let context = super::lock(context);
        Ok((
            context.checkpoint_buffers_length()?,
            context.checkpoint_images_length()?,
        ))
    }

    pub fn save_checkpoint_buffers(
        &self,
        id: u32,
        writer: &mut impl std::io::Write,
        timeout: Duration,
    ) -> Result<(), RecipeError> {
        let context = self
            .contexts
            .get(&id)
            .ok_or(invalid("checkpoint context is absent"))?;
        super::lock(context).save_device_buffers(writer, timeout)
    }

    pub fn save_checkpoint_images(
        &self,
        id: u32,
        writer: &mut impl std::io::Write,
        timeout: Duration,
    ) -> Result<(), RecipeError> {
        let context = self
            .contexts
            .get(&id)
            .ok_or(invalid("checkpoint context is absent"))?;
        super::lock(context).save_device_images(writer, timeout)
    }

    pub fn save_checkpoint_shared_image(
        &mut self,
        target: &crate::venus::renderer::ScanoutTarget,
        release: impl Into<super::checkpoint_ownership::SharedImageState>,
        writer: &mut impl std::io::Write,
    ) -> Result<(), RecipeError> {
        let release = release.into();
        let (executed, actual_release) =
            self.checkpoint_shared_target(target.resource_id, &target.handle)?;
        if executed.image != target.image || actual_release != release {
            return Err(invalid(
                "shared checkpoint target differs from executed ownership",
            ));
        }
        self.scanout_device(target)
            .map_err(RecipeError::Replay)?
            .save_shared_image(target, release, writer)
            .map_err(|error| RecipeError::Replay(error.to_string()))
    }

    pub fn restore_checkpoint_shared_image(
        &mut self,
        id: u32,
        bytes: &[u8],
        owner: u32,
        budgets: &BudgetTable,
    ) -> Result<super::checkpoint_ownership::RestoredSharedImage, RecipeError> {
        if !self.contexts.is_empty() {
            return Err(invalid("shared upload needs a cold factory"));
        }
        let uuids = super::scanout::checkpoint::image_uuids(bytes)
            .map_err(|error| RecipeError::Replay(error.to_string()))?;
        let limits = self.checkpoint_payload_limits(owner, budgets)?;
        let owner_budget = budgets.budget(owner)?.clone();
        if self
            .scanout
            .as_ref()
            .is_some_and(|device| device.uuids() != uuids)
        {
            self.scanout = None;
        }
        if self.scanout.is_none() {
            self.scanout = Some(
                super::scanout::ScanoutDevice::open(self.host.clone(), self.budget.clone(), uuids)
                    .map_err(RecipeError::Replay)?,
            );
        }
        let restored = self
            .scanout
            .as_mut()
            .ok_or(invalid("shared uploader is absent"))?
            .restore_shared_image_checkpoint(id, bytes, &limits, &owner_budget)
            .map_err(|error| RecipeError::Replay(error.to_string()))?;
        self.adopt_checkpoint_shared_image(&restored, owner, budgets)?;
        Ok(restored)
    }
    pub(crate) fn prepare_checkpoint_retention(
        &mut self,
        budget: &Arc<PageBudget>,
    ) -> Result<(), RecipeError> {
        match &self.checkpoint_recipes {
            Some(existing) if Arc::ptr_eq(existing, budget) => Ok(()),
            Some(_) => Err(invalid(
                "checkpoint recipe storage belongs to another factory budget",
            )),
            None => self.enable_checkpoint_recipes(budget),
        }
    }
    /// Retain bounded reconstruction state for every future context, including
    /// empty contexts and contexts recreated under a previously used guest id.
    /// Journal exhaustion refuses a later save, never ongoing guest execution.
    pub fn enable_checkpoint_recipes(
        &mut self,
        budget: &Arc<PageBudget>,
    ) -> Result<(), RecipeError> {
        if self.checkpoint_seen_contexts
            || !self.contexts.is_empty()
            || self.checkpoint_recipes.is_some()
        {
            return Err(invalid(
                "factory recipe retention must start before creating contexts",
            ));
        }
        self.payloads.ownership.enable();
        self.checkpoint_recipes = Some(budget.clone());
        Ok(())
    }
    /// Capture only native object metadata after the caller parked all writers.
    /// A failure leaves the source factory and its contexts alive.
    pub fn checkpoint_objects(
        &self,
        budgets: &mut BudgetTable,
        pages: &mut HostPageTable,
        payloads: &mut SharedPayloads,
        timeout: Duration,
    ) -> Result<FactoryObjectCheckpoint, RecipeError> {
        if !self.graveyard.is_empty() {
            return Err(invalid(
                "factory checkpoint still has retired native devices",
            ));
        }
        let deadline = Instant::now()
            .checked_add(timeout.min(Duration::from_secs(30)))
            .ok_or(invalid("invalid factory checkpoint timeout"))?;
        let mut ids = self.contexts.keys().copied().collect::<Vec<_>>();
        ids.sort_unstable();
        if ids.len() > MAX_VENUS_CONTEXTS {
            return Err(RecipeError::Budget);
        }
        let mut saved = FactoryObjectCheckpoint {
            contexts: Vec::new(),
        };
        saved
            .contexts
            .try_reserve_exact(ids.len())
            .map_err(|_| RecipeError::Budget)?;
        for id in ids {
            let mut context = super::lock(&self.contexts[&id]);
            if let Some(reason) = context.checkpoint_data_refusal() {
                return Err(invalid(reason));
            }
            let sync = context.save_sync_state(remaining(deadline)?)?;
            let parts = [
                context.checkpoint_budget_links(budgets)?.encode(budgets)?,
                context.save_object_recipes()?,
                payloads.encode_links(&context.shared_allocation_links()?)?,
                pages.encode_links(&context.host_allocation_links()?)?,
                sync,
                context.save_buffer_ownership()?,
            ];
            saved.contexts.push(ContextRecord { id, parts });
            saved.validate()?;
        }
        Ok(saved)
    }

    /// Prepare every native context without committing any to the factory.
    /// Guards destroy all reconstructed native objects on a later failure.
    /// Allocation bytes must be restored before adopting/starting these contexts.
    pub fn restore_checkpoint_objects(
        &self,
        saved: &FactoryObjectCheckpoint,
        budgets: &BudgetTable,
        pages: &HostPageTable,
        payloads: &SharedPayloads,
        recipe_budget: &Arc<PageBudget>,
        timeout: Duration,
    ) -> Result<Vec<CheckpointContext<H>>, RecipeError> {
        if !self.contexts.is_empty() {
            return Err(invalid("factory object restore needs a fresh factory"));
        }
        saved.validate()?;
        let deadline = Instant::now()
            .checked_add(timeout.min(Duration::from_secs(30)))
            .ok_or(invalid("invalid factory restore timeout"))?;
        let mut links = Vec::new();
        links
            .try_reserve_exact(saved.contexts.len())
            .map_err(|_| RecipeError::Budget)?;
        // Check all owner roles and global graph links before replay creates
        // even the first native context. Guest ids cannot establish ownership.
        for record in &saved.contexts {
            links.push((
                ContextBudgetLinks::decode(&record.parts[0], budgets)?,
                payloads.resolve_links(&record.parts[2])?,
                pages.resolve_links(&record.parts[3])?,
            ));
        }
        let mut contexts = Vec::new();
        contexts
            .try_reserve_exact(saved.contexts.len())
            .map_err(|_| RecipeError::Budget)?;
        for (record, (owners, shared, host_pages)) in saved.contexts.iter().zip(links) {
            remaining(deadline)?;
            let mut context = self.new_checkpoint_context(record.id, &owners, budgets)?;
            context.restore_object_recipes_with_memory(
                &record.parts[1],
                recipe_budget,
                &shared,
                &host_pages,
            )?;
            if let Some(reason) = context.checkpoint_data_refusal() {
                return Err(invalid(reason));
            }
            context.restore_sync_state(&record.parts[4], remaining(deadline)?)?;
            context.restore_buffer_ownership(&record.parts[5])?;
            contexts.push(context);
        }
        Ok(contexts)
    }
}

#[cfg(test)]
mod tests;
