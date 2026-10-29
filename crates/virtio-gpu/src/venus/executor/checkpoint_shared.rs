//! Linking reconstructed allocation generations to preloaded shared payloads.
//! Links are runtime objects: a renderer file must encode stable payload ids,
//! recreate each payload once, then resolve those ids to fresh SharedHandles.

use std::sync::Arc;

use super::checkpoint::RecipeError;
use super::context::{id_error, invalid, ExecError};
use super::host::{HostVulkan, MemoryRequest};
use super::objects::{DedicatedTo, MemoryHandle, MemoryObject};
use super::policy::{self, MEMORY_PROPERTY_HOST_VISIBLE};
use super::VulkanContext;
use crate::venus::protocol::*;
use crate::venus::renderer::{SharedHandle, SharedRef};
use crate::venus::shmem::PageBudget;

const MAX_LINKS: usize = 1 << 16;

mod payloads;
pub use payloads::SharedPayloads;
mod owners;
pub use owners::PayloadBudgetOwners;

#[cfg(test)]
mod tests;

/// One live allocation's generation in its context and its shared backing.
/// Guest memory ids can be reused, so `memory` alone is insufficient.
#[derive(Debug, Clone)]
pub struct SharedAllocationLink {
    pub device: u64,
    pub memory: u64,
    pub allocation_serial: u64,
    pub handle: SharedHandle,
}

impl<H: HostVulkan> VulkanContext<H> {
    /// Resolve a directory association only after allocation links and object
    /// recipes have been restored. Check the actual binding generation and
    /// payload, not just its guest memory id. No native operation is issued.
    pub fn restore_directory_image(
        &mut self,
        saved: &crate::venus::renderer::checkpoint_directory::SavedScanoutImage,
        payload: &SharedHandle,
    ) -> Result<crate::venus::renderer::ScanoutImage, RecipeError> {
        let invalid = RecipeError::Invalid;
        if saved.ctx_id != self.ctx_id {
            return Err(invalid("directory image belongs to another context"));
        }
        let image = self
            .objects
            .checkpoint_image(saved.image)
            .ok_or(invalid("directory image is absent"))?;
        let (memory, offset) = image
            .bound_memory
            .ok_or(invalid("directory image is unbound"))?;
        let allocation = self
            .objects
            .memory(image.device, memory)
            .map_err(|_| invalid("directory image allocation is absent"))?;
        if offset != saved.offset
            || image.canonical.as_ref() != Some(&saved.canonical)
            || image.bindings.len() != 1
            || image.binding_identities.len() != 1
            || image.bindings[0].memory != memory
            || image.bindings[0].offset != offset
            || !Arc::ptr_eq(&image.binding_identities[0], &allocation.identity)
            || allocation
                .shared_handle
                .as_ref()
                .is_none_or(|handle| !SharedRef::of(handle).is(payload))
            || image
                .scanout
                .as_ref()
                .is_some_and(|scanout| !scanout.shared.is(payload))
        {
            return Err(invalid(
                "directory image recipe, allocation generation or payload differs",
            ));
        }
        let device = image.device;
        let token = image
            .scanout
            .as_ref()
            .map_or_else(|| Arc::new(()), |scanout| scanout.token.clone());
        let image = self
            .objects
            .image_mut(device, saved.image)
            .map_err(|_| invalid("directory image disappeared"))?;
        image.scanout = Some(super::objects::ImageScanout {
            shared: SharedRef::of(payload),
            token: token.clone(),
        });
        Ok(crate::venus::renderer::ScanoutImage {
            ctx_id: saved.ctx_id,
            image: saved.image,
            offset: saved.offset,
            canonical: saved.canonical.clone(),
            alive: Arc::downgrade(&token),
        })
    }

    /// Inventory live shared allocations without collapsing reused guest ids.
    /// Capture payload bytes separately, once per SharedHandle identity across
    /// every context and blob, and replace these handles with stable file ids.
    pub fn shared_allocation_links(&self) -> Result<Vec<SharedAllocationLink>, RecipeError> {
        let mut links = Vec::new();
        for (id, memory) in self.objects.checkpoint_memories() {
            let Some(handle) = &memory.shared_handle else {
                continue;
            };
            if links.len() >= MAX_LINKS {
                return Err(RecipeError::Budget);
            }
            links.try_reserve(1).map_err(|_| RecipeError::Budget)?;
            links.push(SharedAllocationLink {
                device: memory.device,
                memory: id,
                allocation_serial: memory.allocation_serial,
                handle: handle.clone(),
            });
        }
        links.sort_unstable_by_key(|link| link.allocation_serial);
        Ok(links)
    }

    /// Reconstruct objects with allocations importing preloaded shared payloads.
    /// No guest/GPU work may run. Links must resolve to the same fresh payload
    /// for all importers. Failed reconstruction discards its partial objects.
    pub fn restore_object_recipes_with_shared(
        &mut self,
        bytes: &[u8],
        budget: &Arc<PageBudget>,
        links: &[SharedAllocationLink],
    ) -> Result<(), RecipeError> {
        self.restore_recipes_linked(bytes, budget, links, &[], false)
    }

    /// Reconstruct device-local and host-page shared identities together. Each
    /// page/payload must already be loaded once in the renderer-wide namespace.
    pub fn restore_object_recipes_with_memory(
        &mut self,
        bytes: &[u8],
        budget: &Arc<PageBudget>,
        links: &[SharedAllocationLink],
        pages: &[super::checkpoint_pages::HostAllocationLink],
    ) -> Result<(), RecipeError> {
        self.restore_recipes_linked(bytes, budget, links, pages, true)
    }

    fn restore_recipes_linked(
        &mut self,
        bytes: &[u8],
        budget: &Arc<PageBudget>,
        links: &[SharedAllocationLink],
        pages: &[super::checkpoint_pages::HostAllocationLink],
        require_all_pages: bool,
    ) -> Result<(), RecipeError> {
        if self.object_count() != 0
            || self.recipes.is_some()
            || self.is_fatal()
            || self.memory_sequence != 0
            || !self.restoring_shared.is_empty()
            || !self.restoring_pages.is_empty()
        {
            return Err(RecipeError::Invalid(
                "shared recipe restore needs a fresh context",
            ));
        }
        payloads::check_links(links)?;
        super::checkpoint_pages::check_links(pages)?;
        let serials: std::collections::HashSet<_> =
            links.iter().map(|link| link.allocation_serial).collect();
        let ids: std::collections::HashSet<_> = links.iter().map(|link| link.memory).collect();
        if pages
            .iter()
            .any(|page| serials.contains(&page.allocation_serial) || ids.contains(&page.memory))
        {
            return Err(RecipeError::Invalid(
                "a shared allocation is linked to both pages and VRAM",
            ));
        }
        self.restoring_pages = pages
            .iter()
            .map(|link| (link.allocation_serial, link.clone()))
            .collect();
        let prepared = links
            .iter()
            .map(|link| (link.allocation_serial, link.clone()))
            .collect();
        self.restoring_shared = prepared;
        let result = self.restore_recipe_journal(bytes, budget).and_then(|()| {
            // A file cannot redirect a historical allocation which is destroyed
            // later and leave the actual live generation reconstructed privately.
            for link in self.restoring_shared.values() {
                let memory = self.objects.memory(link.device, link.memory).map_err(|_| {
                    RecipeError::Invalid("shared link does not name a live allocation")
                })?;
                if memory.allocation_serial != link.allocation_serial
                    || memory
                        .shared_handle
                        .as_ref()
                        .is_none_or(|handle| !Arc::ptr_eq(&handle.0, &link.handle.0))
                {
                    return Err(RecipeError::Invalid(
                        "shared allocation generation or payload differs",
                    ));
                }
            }
            if require_all_pages
                && self.objects.checkpoint_memories().any(|(_, memory)| {
                    memory.pages.is_some()
                        && !self.restoring_pages.contains_key(&memory.allocation_serial)
                })
            {
                return Err(RecipeError::Invalid(
                    "a live host allocation is missing its page graph link",
                ));
            }
            for link in self.restoring_pages.values() {
                let memory = self
                    .objects
                    .memory(link.device, link.memory)
                    .map_err(|_| RecipeError::Invalid("host-page link is not a live allocation"))?;
                if memory.allocation_serial != link.allocation_serial
                    || memory.exported != link.exported
                    || memory
                        .pages
                        .as_ref()
                        .is_none_or(|pages| !Arc::ptr_eq(pages, &link.pages))
                {
                    return Err(RecipeError::Invalid(
                        "host-page generation or backing differs",
                    ));
                }
            }
            Ok(())
        });
        self.restoring_pages.clear();
        self.restoring_shared.clear();
        if result.is_err() && self.object_count() != 0 {
            self.destroy_all();
        }
        result
    }

    pub(super) fn restore_linked_allocation(
        &mut self,
        args: &mut AllocateMemoryArgs,
        id: u64,
        link: &SharedAllocationLink,
        properties: (u32, Option<(u32, u32)>),
        dedicated: Option<DedicatedTo>,
        sharing: (bool, u32),
    ) -> Result<(), ExecError> {
        const NAME: &str = "vkAllocateMemory";
        let (property_flags, flags) = properties;
        let (importing, export_types) = sharing;
        let device_id = args.device.0;
        let info = args
            .p_allocate_info
            .as_ref()
            .ok_or_else(|| invalid(NAME, "missing linked allocation info"))?;
        let (device, guest) = self
            .objects
            .device_and_guest(device_id)
            .map_err(id_error(NAME))?;
        if id != link.memory
            || device_id != link.device
            || link.allocation_serial != self.memory_sequence
            || property_flags & MEMORY_PROPERTY_HOST_VISIBLE != 0
            || device.group_size != 1
            || flags.is_some_and(|(flags, mask)| flags & !3 != 0 || flags & 1 != 0 && mask != 1)
            || (!importing && (export_types != 0x200 || !device.dma_buf()))
            || (importing && export_types & !0x201 != 0)
        {
            return Err(invalid(
                NAME,
                "shared reconstruction allocation does not match its recipe",
            ));
        }
        let export = Self::handle_export(&link.handle, guest).map_err(|why| invalid(NAME, why))?;
        if self.strict_checkpoint_budgets {
            let host_type = guest.host_type(info.memory_type_index);
            let ty = guest
                .host_memory
                .memory_types
                .get(host_type as usize)
                .filter(|_| host_type < guest.host_memory.memory_type_count)
                .ok_or_else(|| invalid(NAME, "shared allocation has no host memory type"))?;
            let heap = guest
                .host_memory
                .memory_heaps
                .get(ty.heap_index as usize)
                .filter(|_| ty.heap_index < guest.host_memory.memory_heap_count)
                .ok_or_else(|| invalid(NAME, "shared allocation has no host heap"))?;
            let (whole, expected) = if heap.flags & policy::MEMORY_HEAP_DEVICE_LOCAL != 0 {
                let key = (super::memory::heap_gpu(guest), ty.heap_index);
                let limits = self.objects.limits();
                (
                    limits.renderer().device_local(key, heap.size),
                    (!importing).then(|| limits.device_local(key, heap.size)),
                )
            } else {
                (
                    self.budget
                        .whole()
                        .cloned()
                        .ok_or_else(|| invalid(NAME, "context has no host budget root"))?,
                    (!importing).then(|| self.budget.clone()),
                )
            };
            let owner = export
                .charge
                .as_ref()
                .and_then(|charge| charge.budget())
                .ok_or_else(|| invalid(NAME, "shared allocation has no captured budget owner"))?;
            if owner.whole().is_none_or(|root| !Arc::ptr_eq(root, &whole))
                || expected
                    .as_ref()
                    .is_some_and(|expected| !Arc::ptr_eq(owner, expected))
            {
                return Err(invalid(
                    NAME,
                    "shared allocation belongs to a different owner or factory",
                ));
            }
        }
        if self
            .restoring_import
            .is_some_and(|meta| meta.kind != 1 || meta.host_size != export.size)
        {
            return Err(invalid(
                NAME,
                "shared payload differs from captured import geometry",
            ));
        }
        if export.type_index != info.memory_type_index
            || info.allocation_size > export.size
            || (!importing
                && info
                    .allocation_size
                    .checked_next_multiple_of(crate::blob::BLOB_PAGE_SIZE)
                    != Some(export.size))
        {
            return Err(invalid(
                NAME,
                "shared reconstruction allocation type or size differs",
            ));
        }
        let request = MemoryRequest {
            size: export.size,
            type_index: export.type_index,
            import: None,
            flags,
            dedicated: None,
            export_handle: false,
            import_handle: Some(export.shared.clone()),
        };
        match self.host.allocate_memory(&device.host, &request) {
            Ok(host) => {
                self.objects.insert_memory(
                    id,
                    MemoryObject {
                        dedicated_identity: self
                            .checkpoint_dedicated_identity(device_id, dedicated),
                        checkpoint_placeholder: false,
                        allocation_serial: self.memory_sequence,
                        shared_handle: Some(link.handle.clone()),
                        identity: Arc::new(()),
                        device: device_id,
                        host,
                        size: info.allocation_size,
                        host_size: export.size,
                        type_index: export.type_index,
                        property_flags,
                        pages: None,
                        exported: true,
                        dedicated,
                        allocate_flags: flags.map_or(0, |(flags, _)| flags),
                        handle: if importing {
                            MemoryHandle::Imported
                        } else {
                            MemoryHandle::Exportable
                        },
                        shared: Some(SharedRef::of(&link.handle)),
                        charge: export.charge.clone(),
                    },
                );
                args.ret = VK_SUCCESS;
            }
            Err(ret) => args.ret = ret,
        }
        Ok(())
    }
}
