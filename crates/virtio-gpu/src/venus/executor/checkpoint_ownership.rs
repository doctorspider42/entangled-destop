//! Executed shared-image state. Recorded barriers are insufficient: only an
//! accepted submit publishes state, with an epoch reserved before native work.
//! Epochs let a delayed CPU bookkeeping commit avoid replacing a newer submit.
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Mutex, PoisonError};

use super::checkpoint::RecipeError;
use super::image_layouts::{Cell, Owner};
use super::modifier::CanonicalImage;
use super::objects::{ImageObject, Objects};
use super::{ExecutorFactory, HostVulkan};
use crate::venus::renderer::{ScanoutRelease, ScanoutTarget, SharedHandle, SharedRef};

const MAX_PAYLOADS: usize = 1 << 16;

/// An unused allocation has no pixels or external release to reconstruct.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SharedImageState {
    Initial,
    Released(ScanoutRelease),
    ReleasedTransition {
        old_layout: i32,
        release: ScanoutRelease,
    },
}

impl SharedImageState {
    pub(super) fn transfer(self) -> Option<(i32, ScanoutRelease)> {
        match self {
            Self::Initial => None,
            Self::Released(release) => Some((release.layout, release)),
            Self::ReleasedTransition {
                old_layout,
                release,
            } => Some((old_layout, release)),
        }
    }
}

impl From<ScanoutRelease> for SharedImageState {
    fn from(release: ScanoutRelease) -> Self {
        Self::Released(release)
    }
}

/// Proof that the private native uploader completed reconstruction of these
/// pixels and returned the allocation to its saved external ownership.
pub struct RestoredSharedImage {
    pub(super) target: ScanoutTarget,
    pub(super) state: SharedImageState,
    pub(super) owner: std::sync::Arc<crate::venus::shmem::PageBudget>,
}

impl RestoredSharedImage {
    pub fn target(&self) -> &ScanoutTarget {
        &self.target
    }
    pub fn state(&self) -> SharedImageState {
        self.state
    }
}

struct Executed {
    shared: SharedRef,
    epoch: u64,
    canonical: CanonicalImage,
    image_state: Option<SharedImageState>,
}

#[derive(Default)]
struct State {
    error: Option<&'static str>,
    images: HashMap<usize, Executed>,
}

#[derive(Default)]
pub(super) struct SharedOwnership {
    enabled: AtomicBool,
    next: AtomicU64,
    state: Mutex<State>,
}

impl std::fmt::Debug for SharedOwnership {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SharedOwnership")
            .field("enabled", &self.enabled.load(Ordering::Relaxed))
            .finish_non_exhaustive()
    }
}

impl SharedOwnership {
    pub(super) fn reset(&self) {
        if self.enabled.load(Ordering::Acquire) {
            *self.state.lock().unwrap_or_else(PoisonError::into_inner) = State::default();
            self.next.store(0, Ordering::Relaxed);
        }
    }
    pub(super) fn enable(&self) {
        self.enabled.store(true, Ordering::Release);
    }

    pub(super) fn invalidate(&self, reason: &'static str) {
        if self.enabled.load(Ordering::Acquire) {
            let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
            state.error.get_or_insert(reason);
        }
    }

    // try_update is the Rust 1.99 rename, beyond the workspace's 1.85 MSRV.
    #[allow(deprecated)]
    pub(super) fn reserve_epoch(&self) -> u64 {
        if !self.enabled.load(Ordering::Acquire) {
            return 0;
        }
        match self
            .next
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |value| {
                value.checked_add(1)
            }) {
            Ok(old) => old + 1,
            Err(_) => {
                self.invalidate("shared checkpoint submit epoch exhausted");
                0
            }
        }
    }

    #[cfg(test)]
    fn record(
        &self,
        shared: SharedRef,
        epoch: u64,
        canonical: CanonicalImage,
        release: Option<ScanoutRelease>,
    ) {
        self.record_state(
            shared,
            epoch,
            canonical,
            release.map(SharedImageState::Released),
        );
    }

    fn record_state(
        &self,
        shared: SharedRef,
        epoch: u64,
        canonical: CanonicalImage,
        image_state: Option<SharedImageState>,
    ) {
        if epoch == 0 {
            return;
        }
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        if state.error.is_some() {
            return;
        }
        if state
            .images
            .get(&shared.key())
            .is_some_and(|previous| previous.canonical != canonical)
        {
            state.error = Some("shared payload uses incompatible canonical image aliases");
            return;
        }
        if epoch % 128 == 0 || state.images.len() >= MAX_PAYLOADS {
            state.images.retain(|_, image| image.shared.is_alive());
        }
        if state
            .images
            .get(&shared.key())
            .is_some_and(|previous| previous.epoch > epoch)
        {
            return;
        }
        if !state.images.contains_key(&shared.key())
            && (state.images.len() >= MAX_PAYLOADS || state.images.try_reserve(1).is_err())
        {
            state.error = Some("shared checkpoint ownership table is full");
            return;
        }
        state.images.insert(
            shared.key(),
            Executed {
                shared,
                epoch,
                canonical,
                image_state,
            },
        );
    }

    pub(super) fn observe<H: HostVulkan>(
        &self,
        epoch: u64,
        claimed: Option<&[SharedRef]>,
        image: &ImageObject<H>,
        cell: Option<Cell>,
        objects: &Objects<H>,
    ) {
        if epoch == 0 {
            return;
        }
        for (binding, identity) in image.bindings.iter().zip(&image.binding_identities) {
            let Ok(memory) = objects.memory(image.device, binding.memory) else {
                continue;
            };
            if !std::sync::Arc::ptr_eq(identity, &memory.identity) {
                continue;
            }
            let Some(shared) = &memory.shared else {
                continue;
            };
            if claimed
                .is_some_and(|claimed| !claimed.iter().any(|claim| claim.key() == shared.key()))
            {
                self.invalidate("shared image submit had no completion claim");
                continue;
            }
            let Some(canonical) = image
                .canonical
                .as_ref()
                .filter(|_| binding.offset == 0 && image.bindings.len() == 1)
            else {
                self.invalidate("executed shared image has no canonical checkpoint representation");
                continue;
            };
            let release = cell.and_then(|cell| match cell.owner {
                Owner::Released {
                    to,
                    old_layout,
                    new_layout,
                    ..
                } if matches!(
                    to,
                    super::policy::QUEUE_FAMILY_EXTERNAL | super::policy::QUEUE_FAMILY_FOREIGN
                ) && cell.layout == new_layout =>
                {
                    let release = ScanoutRelease {
                        layout: cell.layout,
                        family: to,
                    };
                    Some(if old_layout == new_layout {
                        SharedImageState::Released(release)
                    } else {
                        SharedImageState::ReleasedTransition {
                            old_layout,
                            release,
                        }
                    })
                }
                Owner::Initial if cell.layout == 0 => Some(SharedImageState::Initial),
                _ => None,
            });
            self.record_state(shared.clone(), epoch, canonical.clone(), release);
        }
    }

    fn released(
        &self,
        handle: &SharedHandle,
    ) -> Result<(CanonicalImage, ScanoutRelease), RecipeError> {
        if !self.enabled.load(Ordering::Acquire) {
            return Err(RecipeError::Invalid(
                "executed shared ownership retention was not enabled",
            ));
        }
        let state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(error) = state.error {
            return Err(RecipeError::Invalid(error));
        }
        let record = state
            .images
            .get(&SharedRef::of(handle).key())
            .filter(|record| record.shared.is(handle))
            .ok_or(RecipeError::Invalid(
                "shared payload has no executed canonical image",
            ))?;
        Ok((
            record.canonical.clone(),
            record
                .image_state
                .and_then(SharedImageState::transfer)
                .map(|(_, release)| release)
                .ok_or(RecipeError::Invalid(
                    "shared payload was not executed as externally released",
                ))?,
        ))
    }
}

impl<H: HostVulkan> ExecutorFactory<H> {
    /// Bounded, read-only evidence for a refused payload. Never infer ownership
    /// or discard allocation contents merely because no image was observed.
    fn log_shared_checkpoint_refusal(
        &self,
        resource_id: u32,
        handle: &SharedHandle,
        error: &RecipeError,
    ) {
        let export = handle.0.downcast_ref::<super::memory::HandleExport<H>>();
        tracing::warn!(
            resource_id,
            bytes = export.map(|export| export.size),
            memory_type = export.map(|export| export.type_index),
            %error,
            "shared checkpoint payload refused; inspecting live allocation bindings"
        );
        let mut remaining = 16;
        for (ctx_id, context) in &self.contexts {
            let context = super::lock(context);
            for (memory_id, memory) in context.objects.checkpoint_memories() {
                if !memory
                    .shared
                    .as_ref()
                    .is_some_and(|shared| shared.is(handle))
                {
                    continue;
                }
                if remaining == 0 {
                    return;
                }
                remaining -= 1;
                tracing::warn!(
                    ctx_id, memory_id, device = memory.device,
                    serial = memory.allocation_serial, bytes = memory.size,
                    host_bytes = memory.host_size, flags = memory.allocate_flags,
                    properties = memory.property_flags, dedicated = ?memory.dedicated,
                    handle_kind = ?memory.handle,
                    "refused shared checkpoint allocation"
                );
                for (image_id, image) in context.objects.checkpoint_images() {
                    if !image
                        .binding_identities
                        .iter()
                        .any(|identity| std::sync::Arc::ptr_eq(identity, &memory.identity))
                    {
                        continue;
                    }
                    if remaining == 0 {
                        return;
                    }
                    remaining -= 1;
                    let cell = context
                        .image_tracking
                        .images
                        .get(&image_id)
                        .and_then(|state| state.checkpoint_cell(0, 0));
                    tracing::warn!(
                        ctx_id, memory_id, image_id, facts = ?image.facts,
                        canonical = ?image.canonical,
                        bindings = ?&image.bindings[..image.bindings.len().min(4)],
                        executed_color_cell = ?cell,
                        "image bound to refused shared checkpoint allocation"
                    );
                }
            }
        }
    }

    /// Seed executed ownership only from a successful native pixel upload,
    /// before adopting contexts or starting any restored workers.
    pub fn adopt_checkpoint_shared_image(
        &self,
        restored: &RestoredSharedImage,
        owner: u32,
        budgets: &super::checkpoint_budgets::BudgetTable,
    ) -> Result<(), RecipeError> {
        if !self.contexts.is_empty() {
            return Err(RecipeError::Invalid(
                "shared ownership adoption needs a fresh factory",
            ));
        }
        let _ = self.checkpoint_payload_limits(owner, budgets)?;
        let export = restored
            .target
            .handle
            .0
            .downcast_ref::<super::memory::HandleExport<H>>()
            .ok_or(RecipeError::Invalid(
                "uploaded shared image belongs to another native backend",
            ))?;
        if !std::sync::Arc::ptr_eq(&restored.owner, budgets.budget(owner)?) {
            return Err(RecipeError::Invalid(
                "uploaded shared image belongs to another allocation owner",
            ));
        }
        if export.charge.as_ref().is_none_or(|charge| {
            charge.amount() != export.size
                || charge
                    .budget()
                    .is_none_or(|budget| !std::sync::Arc::ptr_eq(budget, &restored.owner))
        }) {
            return Err(RecipeError::Invalid(
                "uploaded shared image has no matching live allocation charge",
            ));
        }
        self.payloads.ownership.enable();
        let epoch = self.payloads.ownership.reserve_epoch();
        self.payloads.ownership.record_state(
            SharedRef::of(&restored.target.handle),
            epoch,
            restored.target.image.clone(),
            Some(restored.state),
        );
        if restored.state != SharedImageState::Initial {
            self.payloads.ownership.released(&restored.target.handle)?;
        }
        Ok(())
    }
    /// Called after all guest workers and GPU/presenter writers are drained.
    /// Select actual executed metadata, never the directory's recorded release.
    /// Retained weak identities also cover an exporter destroyed after its work.
    pub fn checkpoint_shared_target(
        &self,
        resource_id: u32,
        handle: &SharedHandle,
    ) -> Result<(ScanoutTarget, SharedImageState), RecipeError> {
        if !self.graveyard.is_empty() || !self.payloads.running(&SharedRef::of(handle)).is_empty() {
            return Err(RecipeError::Invalid(
                "shared checkpoint still has running native owners",
            ));
        }
        for context in self.contexts.values() {
            let context = super::lock(context);
            if context.is_fatal()
                || !context.holds.is_empty()
                || context.image_tracking.error.is_some()
            {
                return Err(RecipeError::Invalid(
                    "shared checkpoint has an incomplete or untracked context",
                ));
            }
        }
        let (image, release) = self.shared_checkpoint_state(handle).inspect_err(|error| {
            self.log_shared_checkpoint_refusal(resource_id, handle, error);
        })?;
        let stride = u32::try_from(image.layout().row_pitch).map_err(|_| RecipeError::Budget)?;
        let spec = crate::renderer::ScanoutBlobSpec {
            format: crate::FORMAT_B8G8R8A8_UNORM,
            width: image.width,
            height: image.height,
            stride,
            offset: 0,
        };
        Ok((
            ScanoutTarget {
                resource_id,
                handle: handle.clone(),
                image,
                spec,
            },
            release,
        ))
    }
}

#[cfg(test)]
mod tests;

mod initial;
