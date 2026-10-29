//! Whole-renderer object graph. GPU allocation contents and VM transport state
//! remain separate. Every save/load here requires a closed quiesce gate.
use super::checkpoint_blobs::BlobCheckpointSet;
use super::checkpoint_rings::RingCheckpointSet;
use super::*;
use crate::venus::executor::checkpoint::RecipeError;
use crate::venus::executor::checkpoint_budgets::BudgetTable;
use crate::venus::executor::checkpoint_factory::FactoryObjectCheckpoint;
use crate::venus::executor::checkpoint_pages::{HostPageTable, PageBudgetOwners};
use crate::venus::executor::checkpoint_shared::{PayloadBudgetOwners, SharedPayloads};
use crate::venus::executor::{ExecutorFactory, HostVulkan, VulkanContext};
use crate::venus::shmem::PageBudget;
use crate::venus::wire::{Decoder, Encoder};
use std::time::Instant;

const VERSION: u32 = 1;
const LIMITS: [usize; 6] = [1 << 20, 1 << 20, 4 << 20, 512 << 20, 34 << 20, 2 << 20];
const MAX_BYTES: usize = 576 << 20;

pub struct RendererObjectCheckpoint {
    budget_bytes: Vec<u8>,
    page_owners: Vec<u8>,
    payload_owners: Vec<u8>,
    objects: FactoryObjectCheckpoint,
    blobs: BlobCheckpointSet,
    rings: RingCheckpointSet,
}

pub struct CapturedRendererObjects {
    pub metadata: RendererObjectCheckpoint,
    pub budgets: BudgetTable,
    pub pages: HostPageTable,
    pub payloads: SharedPayloads,
    /// Executed canonical metadata for each dense payload id, in id order.
    pub shared_images: Vec<(
        ScanoutTarget,
        crate::venus::executor::checkpoint_ownership::SharedImageState,
    )>,
}

fn invalid(why: &'static str) -> RecipeError {
    RecipeError::Invalid(why)
}

fn remaining(deadline: Instant) -> Result<Duration, RecipeError> {
    deadline
        .checked_duration_since(Instant::now())
        .filter(|duration| !duration.is_zero())
        .ok_or(invalid("renderer object checkpoint deadline expired"))
}

impl RendererObjectCheckpoint {
    /// Check the complete virtio/native namespace before restoring any pages
    /// or native handles. This does not adopt the front-end tables itself.
    pub fn validate_frontend(
        &self,
        contexts: &crate::renderer::checkpoint::ContextCheckpoint,
        blobs: &crate::blob::BlobTable,
    ) -> Result<(), RecipeError> {
        self.validate_contexts()?;
        if contexts.contexts().len() != self.rings.contexts.len()
            || contexts.contexts().any(|(id, capset)| {
                !self
                    .rings
                    .contexts
                    .iter()
                    .any(|native| native.ctx_id == id && native.capset_id == capset)
            })
        {
            return Err(invalid("virtio/native context namespaces differ"));
        }
        self.blobs.validate_frontend(blobs)
    }

    pub fn context_ids(&self) -> impl Iterator<Item = u32> + '_ {
        self.objects.context_ids()
    }
    fn validate_contexts(&self) -> Result<(), RecipeError> {
        let native = self
            .objects
            .context_ids()
            .collect::<std::collections::HashSet<_>>();
        let rings = self
            .rings
            .contexts
            .iter()
            .map(|context| context.ctx_id)
            .collect::<std::collections::HashSet<_>>();
        if native != rings {
            return Err(invalid("renderer and native context inventories differ"));
        }
        Ok(())
    }

    pub fn encode(&self) -> Result<Vec<u8>, RecipeError> {
        self.validate_contexts()?;
        let objects = self.objects.encode()?;
        let blobs = self.blobs.encode()?;
        let rings = self.rings.encode()?;
        let parts = [
            &self.budget_bytes,
            &self.page_owners,
            &self.payload_owners,
            &objects,
            &blobs,
            &rings,
        ];
        let mut encoder = Encoder::with_limit(MAX_BYTES);
        encoder.u32(VERSION)?;
        for (part, limit) in parts.into_iter().zip(LIMITS) {
            if part.len() > limit {
                return Err(RecipeError::Budget);
            }
            encoder.u32(part.len() as u32)?;
            encoder.blob(part)?;
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
        let mut parts = [&[][..]; 6];
        for (part, limit) in parts.iter_mut().zip(LIMITS) {
            let len = decoder.u32()? as usize;
            if len > limit {
                return Err(RecipeError::Budget);
            }
            *part = decoder.blob(len)?;
        }
        if decoder.remaining() != 0 {
            return Err(invalid("trailing renderer object data"));
        }
        let copy = |bytes: &[u8]| -> Result<Vec<u8>, RecipeError> {
            let mut owned = Vec::new();
            owned
                .try_reserve_exact(bytes.len())
                .map_err(|_| RecipeError::Budget)?;
            owned.extend_from_slice(bytes);
            Ok(owned)
        };
        let saved = Self {
            budget_bytes: copy(parts[0])?,
            page_owners: copy(parts[1])?,
            payload_owners: copy(parts[2])?,
            objects: FactoryObjectCheckpoint::decode(parts[3])?,
            blobs: BlobCheckpointSet::decode(parts[4])?,
            rings: RingCheckpointSet::decode(parts[5])?,
        };
        saved.validate_contexts()?;
        Ok(saved)
    }

    /// Resolve trusted current caps before allocating pages or native payloads.
    pub fn resolve_budgets<H: HostVulkan>(
        &self,
        factory: &ExecutorFactory<H>,
    ) -> Result<BudgetTable, RecipeError> {
        let budgets = factory.restore_budget_table(&self.budget_bytes)?;
        let pages = PageBudgetOwners::decode(&self.page_owners, &budgets)?;
        self.blobs.validate_page_owners(&pages)?;
        PayloadBudgetOwners::decode(&self.payload_owners, &budgets)?;
        Ok(budgets)
    }

    pub fn page_owners(&self, budgets: &BudgetTable) -> Result<PageBudgetOwners, RecipeError> {
        let pages = PageBudgetOwners::decode(&self.page_owners, budgets)?;
        self.blobs.validate_page_owners(&pages)?;
        Ok(pages)
    }

    pub fn payload_owners(
        &self,
        budgets: &BudgetTable,
    ) -> Result<PayloadBudgetOwners, RecipeError> {
        PayloadBudgetOwners::decode(&self.payload_owners, budgets)
    }
}

impl<H: HostVulkan> VenusRenderer<ExecutorFactory<H>> {
    /// Capture the shared identity graph at one quiesced point. Keep guest,
    /// presenter and ring writers stopped while streaming allocation bytes.
    pub fn checkpoint_object_graph(
        &self,
        timeout: Duration,
    ) -> Result<CapturedRendererObjects, RecipeError> {
        let deadline = Instant::now()
            .checked_add(timeout.min(Duration::from_secs(30)))
            .ok_or(invalid("invalid renderer checkpoint timeout"))?;
        if !self.quiesce.is_paused() || !self.quiesce.wait_until_idle(remaining(deadline)?) {
            return Err(invalid("renderer object capture requires quiesced workers"));
        }
        let mut budgets = self.sinks.checkpoint_budget_table()?;
        let mut pages = HostPageTable::default();
        let mut payloads = SharedPayloads::default();
        let objects = self.sinks.checkpoint_objects(
            &mut budgets,
            &mut pages,
            &mut payloads,
            remaining(deadline)?,
        )?;
        if self.sinks.pending_ring_fences() != 0 || !self.retirer.is_empty() {
            return Err(invalid(
                "renderer checkpoint still has transport retirement state",
            ));
        }
        let blobs = self.checkpoint_blobs(&mut pages, &mut payloads)?;
        let rings = self.checkpoint_rings(&mut pages, remaining(deadline)?)?;
        let page_owners = pages.checkpoint_budget_owners(&mut budgets)?;
        blobs.validate_page_owners(&page_owners)?;
        let payload_owners = payloads.checkpoint_budget_owners::<H>(&mut budgets)?;
        let mut shared_images = Vec::new();
        shared_images
            .try_reserve_exact(payloads.handles().len())
            .map_err(|_| RecipeError::Budget)?;
        for (index, handle) in payloads.handles().iter().enumerate() {
            remaining(deadline)?;
            shared_images.push(
                self.sinks
                    .checkpoint_shared_target(index as u32 + 1, handle)?,
            );
        }
        let metadata = RendererObjectCheckpoint {
            budget_bytes: budgets.encode()?,
            page_owners: page_owners.encode(&budgets)?,
            payload_owners: payload_owners.encode(&budgets)?,
            objects,
            blobs,
            rings,
        };
        metadata.validate_contexts()?;
        Ok(CapturedRendererObjects {
            metadata,
            budgets,
            pages,
            payloads,
            shared_images,
        })
    }

    /// Adopt cold pages/payloads, restore each context's private allocation
    /// contents through the callback, then publish mappings and start paused
    /// workers. The caller opens the gate only after VM transport/CPU restore.
    /// Any failure resets this cold reconstruction, including mapped ranges.
    pub fn restore_object_graph(
        &mut self,
        saved: &RendererObjectCheckpoint,
        budgets: &BudgetTable,
        pages: &HostPageTable,
        payloads: &SharedPayloads,
        recipes: &Arc<PageBudget>,
        mut private_memory: impl FnMut(u32, &mut VulkanContext<H>) -> Result<(), RecipeError>,
    ) -> Result<(), RecipeError> {
        if !self.quiesce.is_paused()
            || !self.contexts.is_empty()
            || self.live.count() != 0
            || self.sinks.host_objects() != 0
            || !self.blobs.is_empty()
        {
            return Err(invalid(
                "object graph restore requires a fresh paused renderer",
            ));
        }
        let restore = (|| {
            saved.validate_contexts()?;
            self.sinks.prepare_checkpoint_retention(recipes)?;
            let owners = saved.page_owners(budgets)?;
            owners.validate_pages(pages, budgets)?;
            saved
                .payload_owners(budgets)?
                .validate_handles::<H>(payloads, budgets)?;
            let mut contexts = self.sinks.restore_checkpoint_objects(
                &saved.objects,
                budgets,
                pages,
                payloads,
                recipes,
                Duration::from_secs(30),
            )?;
            for context in &mut contexts {
                private_memory(context.context_id(), context)?;
            }
            self.restore_blobs(&saved.blobs, pages, payloads, |image, payload| {
                contexts
                    .iter_mut()
                    .find(|context| context.context_id() == image.ctx_id)
                    .ok_or(invalid("directory image context was not reconstructed"))?
                    .restore_directory_image(image, payload)
            })?;
            for context in contexts {
                self.sinks.adopt_checkpoint_context(context)?;
            }
            self.sinks
                .validate_initial_shared_images(payloads.handles())?;
            self.restore_rings(&saved.rings, pages)
        })();
        if restore.is_err() {
            self.reset();
        }
        restore
    }
}

#[cfg(test)]
mod tests;
