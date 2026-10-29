//! A missing executed record alone proves nothing. An unused payload requires
//! a live dedicated exporter and the same unused, dedicated image in every
//! importing allocation. Resource and allocation generations must both match.
use super::super::objects::{DedicatedTo, MemoryHandle};
use super::*;
use std::sync::Arc;

impl SharedOwnership {
    pub(in super::super) fn require_initialized(
        &self,
        handle: &SharedHandle,
    ) -> Result<(), String> {
        let state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        if state
            .images
            .get(&SharedRef::of(handle).key())
            .is_some_and(|record| {
                record.shared.is(handle) && record.image_state == Some(SharedImageState::Initial)
            })
        {
            return Err("shared image has not been initialized since restore".into());
        }
        Ok(())
    }
}

impl<H: HostVulkan> ExecutorFactory<H> {
    pub(super) fn shared_checkpoint_state(
        &self,
        handle: &SharedHandle,
    ) -> Result<(CanonicalImage, SharedImageState), RecipeError> {
        let ownership = &self.payloads.ownership;
        if !ownership.enabled.load(Ordering::Acquire) {
            return Err(RecipeError::Invalid(
                "executed shared ownership retention was not enabled",
            ));
        }
        let mut restored_canonical = None;
        {
            let state = ownership
                .state
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            if let Some(error) = state.error {
                return Err(RecipeError::Invalid(error));
            }
            if let Some(record) = state.images.get(&SharedRef::of(handle).key()) {
                if !record.shared.is(handle) {
                    return Err(RecipeError::Invalid("shared checkpoint identity differs"));
                }
                if record.image_state != Some(SharedImageState::Initial) {
                    return record
                        .image_state
                        .map(|state| (record.canonical.clone(), state))
                        .ok_or(RecipeError::Invalid(
                            "shared payload was not executed as externally released",
                        ));
                }
                restored_canonical = Some(record.canonical.clone());
            }
        }
        let mut canonical = None;
        let mut exporter = false;
        let mut allocations = 0usize;
        for context in self.contexts.values() {
            let context = super::super::lock(context);
            for (memory_id, memory) in context.objects.checkpoint_memories() {
                if !memory
                    .shared
                    .as_ref()
                    .is_some_and(|shared| shared.is(handle))
                {
                    continue;
                }
                allocations += 1;
                let Some(DedicatedTo::Image(id)) = memory.dedicated else {
                    return Err(RecipeError::Invalid(
                        "unused shared payload is not dedicated to an image",
                    ));
                };
                let image = context.objects.image(memory.device, id).map_err(|_| {
                    RecipeError::Invalid("unused shared payload lost its dedicated image")
                })?;
                let tracked =
                    context
                        .image_tracking
                        .images
                        .get(&id)
                        .ok_or(RecipeError::Invalid(
                            "unused shared image has no executed state",
                        ))?;
                if memory.property_flags & 3 != 1
                    || memory.checkpoint_placeholder
                    || memory
                        .dedicated_identity
                        .as_ref()
                        .is_none_or(|identity| !Arc::ptr_eq(identity, &image.identity))
                    || !Arc::ptr_eq(&tracked.identity, &image.identity)
                    || image.bindings.len() != 1
                    || image.binding_identities.len() != 1
                    || image.bindings[0].memory != memory_id
                    || image.bindings[0].offset != 0
                    || !Arc::ptr_eq(&image.binding_identities[0], &memory.identity)
                    || image.facts.mip_levels != 1
                    || image.facts.array_layers != 1
                    || image.facts.depth != 1
                    || image.facts.planes != 1
                    || image.facts.samples != 1
                    || tracked.checkpoint_cell(0, 0)
                        != Some(Cell {
                            layout: 0,
                            owner: Owner::Initial,
                        })
                {
                    return Err(RecipeError::Invalid(
                        "shared payload is not a proven unused dedicated image",
                    ));
                }
                let current = image
                    .canonical
                    .as_ref()
                    .ok_or(RecipeError::Invalid("unused shared image is not canonical"))?;
                if canonical
                    .as_ref()
                    .is_some_and(|previous| previous != current)
                {
                    return Err(RecipeError::Invalid("unused shared image aliases differ"));
                }
                canonical = Some(current.clone());
                exporter |= memory.handle == MemoryHandle::Exportable;
            }
        }
        if !exporter || allocations == 0 {
            return Err(RecipeError::Invalid(
                "unused shared image requires a live dedicated exporter",
            ));
        }
        let canonical = canonical.ok_or(RecipeError::Invalid("missing unused canonical image"))?;
        if restored_canonical.is_some_and(|saved| saved != canonical) {
            return Err(RecipeError::Invalid(
                "restored Initial image differs from its live canonical bindings",
            ));
        }
        Ok((canonical, SharedImageState::Initial))
    }

    /// Validate restored Initial metadata against the reconstructed live graph
    /// before starting workers; a hostile archive cannot claim defined images
    /// have no contents. GPU reads remain blocked until accepted guest work.
    pub(crate) fn validate_initial_shared_images(
        &self,
        handles: &[SharedHandle],
    ) -> Result<(), RecipeError> {
        for handle in handles {
            if self.payloads.ownership.require_initialized(handle).is_err() {
                self.shared_checkpoint_state(handle)?;
            }
        }
        Ok(())
    }
}
