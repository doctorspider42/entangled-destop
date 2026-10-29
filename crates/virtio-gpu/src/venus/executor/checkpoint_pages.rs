//! Global host-page identity and allocation links. Rings, reply windows, memory
//! exports and all Vulkan imports must register in the same page table.

use std::collections::{HashMap, HashSet};
use std::io::Write;
use std::sync::Arc;

use super::checkpoint::RecipeError;
use super::context::{id_error, invalid, ExecError};
use super::host::{HostVulkan, MemoryRequest};
use super::objects::{DedicatedTo, MemoryHandle, MemoryObject};
use super::{policy, VulkanContext};
use crate::venus::protocol::*;
use crate::venus::shmem::{page_size, PageBudget, RingPages, MAX_MEMORY_ALIGNMENT};
use crate::venus::wire::{Decoder, Encoder};

const VERSION: u32 = 1;
const MAX_PAGES: usize = 1 << 16;
const CHUNK: usize = 64 << 10;

mod owners;
pub use owners::PageBudgetOwners;

#[cfg(test)]
mod tests;

#[derive(Debug, Clone)]
pub struct HostAllocationLink {
    pub device: u64,
    pub memory: u64,
    pub allocation_serial: u64,
    pub exported: bool,
    pub pages: Arc<RingPages>,
}

/// Dense page ids, independent of native pointers and guest resource ids.
#[derive(Default)]
pub struct HostPageTable {
    by_identity: HashMap<usize, u32>,
    pages: Vec<Arc<RingPages>>,
}

fn geometry(declared: u64, mapped: u64, align: u64) -> Result<(), RecipeError> {
    if declared == 0
        || align < page_size() as u64
        || !align.is_power_of_two()
        || align > MAX_MEMORY_ALIGNMENT
        || declared.checked_next_multiple_of(align) != Some(mapped)
        || usize::try_from(mapped).is_err()
    {
        return Err(RecipeError::Invalid("invalid checkpoint page geometry"));
    }
    Ok(())
}

pub(super) fn check_links(links: &[HostAllocationLink]) -> Result<(), RecipeError> {
    if links.len() > MAX_PAGES {
        return Err(RecipeError::Budget);
    }
    let mut ids = HashSet::new();
    let mut serials = HashSet::new();
    ids.try_reserve(links.len())
        .map_err(|_| RecipeError::Budget)?;
    serials
        .try_reserve(links.len())
        .map_err(|_| RecipeError::Budget)?;
    for link in links {
        if link.device == 0
            || link.memory == 0
            || link.allocation_serial == 0
            || !ids.insert(link.memory)
            || !serials.insert(link.allocation_serial)
            || link.pages.is_host_private()
            || link.pages.is_poisoned()
        {
            return Err(RecipeError::Invalid(
                "invalid or duplicate host-page allocation link",
            ));
        }
    }
    Ok(())
}

impl HostPageTable {
    pub fn encoded_len(&self) -> Result<u64, RecipeError> {
        self.pages.iter().try_fold(8u64, |length, page| {
            page.mapped_len()
                .checked_add(24)
                .and_then(|bytes| length.checked_add(bytes))
                .ok_or(RecipeError::Budget)
        })
    }
    pub fn register(&mut self, pages: &Arc<RingPages>) -> Result<u32, RecipeError> {
        if pages.is_host_private() || pages.is_poisoned() {
            return Err(RecipeError::Invalid(
                "private or poisoned pages cannot be published by a checkpoint",
            ));
        }
        let key = Arc::as_ptr(pages) as usize;
        if let Some(id) = self.by_identity.get(&key) {
            return Ok(*id);
        }
        if self.pages.len() >= MAX_PAGES {
            return Err(RecipeError::Budget);
        }
        self.pages.try_reserve(1).map_err(|_| RecipeError::Budget)?;
        self.by_identity
            .try_reserve(1)
            .map_err(|_| RecipeError::Budget)?;
        let id = self.pages.len() as u32 + 1;
        self.pages.push(pages.clone());
        self.by_identity.insert(key, id);
        Ok(id)
    }

    pub fn pages(&self) -> &[Arc<RingPages>] {
        &self.pages
    }

    pub fn page(&self, id: u32) -> Result<&Arc<RingPages>, RecipeError> {
        id.checked_sub(1)
            .and_then(|id| self.pages.get(id as usize))
            .ok_or(RecipeError::Invalid("unknown checkpoint page id"))
    }

    /// All CPU, guest, ring and GPU writers must be parked/drained. Stream each
    /// allocation once, including the entire published/mapped padding span.
    pub fn save(&self, writer: &mut impl Write) -> Result<(), RecipeError> {
        if self
            .pages
            .iter()
            .any(|page| page.is_host_private() || page.is_poisoned())
        {
            return Err(RecipeError::Invalid(
                "checkpoint pages became private or poisoned",
            ));
        }
        writer.write_all(&VERSION.to_le_bytes())?;
        writer.write_all(&(self.pages.len() as u32).to_le_bytes())?;
        let mut chunk = [0u8; CHUNK];
        for pages in &self.pages {
            for value in [
                pages.resource_len(),
                pages.mapped_len(),
                pages.allocation_alignment(),
            ] {
                writer.write_all(&value.to_le_bytes())?;
            }
            let mut offset = 0;
            while offset < pages.mapped_len() {
                let n = (pages.mapped_len() - offset).min(CHUNK as u64) as usize;
                pages.read_mapped_bytes(offset, &mut chunk[..n])?;
                writer.write_all(&chunk[..n])?;
                offset += n as u64;
            }
        }
        Ok(())
    }

    pub fn restore(bytes: &[u8], budget: &Arc<PageBudget>) -> Result<Self, RecipeError> {
        Self::restore_with_budgets(bytes, |_| budget.clone())
    }

    /// The renderer chooses each allocation's original owner/share budget;
    /// imports must not charge the same pages a second time. No writer or guest
    /// mapping may exist until this whole operation and object replay succeed.
    pub fn restore_with_budgets(
        bytes: &[u8],
        mut budget: impl FnMut(u32) -> Arc<PageBudget>,
    ) -> Result<Self, RecipeError> {
        Self::restore_with_owners(bytes, |id| Ok(Some(budget(id))))
    }

    pub(super) fn restore_with_owners(
        bytes: &[u8],
        mut budget: impl FnMut(u32) -> Result<Option<Arc<PageBudget>>, RecipeError>,
    ) -> Result<Self, RecipeError> {
        let mut decoder = Decoder::new(bytes);
        let version = decoder.u32()?;
        if version != VERSION {
            return Err(RecipeError::Version(version));
        }
        let count = decoder.u32()? as usize;
        if count > MAX_PAGES || count > decoder.remaining() / 24 {
            return Err(RecipeError::Invalid(
                "checkpoint page count exceeds its file",
            ));
        }
        let mut records = Vec::new();
        records
            .try_reserve_exact(count)
            .map_err(|_| RecipeError::Budget)?;
        for _ in 0..count {
            let declared = decoder.u64()?;
            let mapped = decoder.u64()?;
            let align = decoder.u64()?;
            geometry(declared, mapped, align)?;
            let data = decoder.blob(mapped as usize)?;
            records.push((declared, mapped, align, data));
        }
        if decoder.remaining() != 0 {
            return Err(RecipeError::Invalid("trailing checkpoint page bytes"));
        }
        let mut owners = Vec::new();
        owners
            .try_reserve_exact(records.len())
            .map_err(|_| RecipeError::Budget)?;
        let mut anonymous_count = 0;
        let mut anonymous_bytes = 0_u64;
        for (index, (declared, _, align, _)) in records.iter().enumerate() {
            let owner = budget(index as u32 + 1)?;
            if owner.is_none() {
                anonymous_count += 1;
                anonymous_bytes = anonymous_bytes
                    .checked_add(*declared)
                    .ok_or(RecipeError::Budget)?;
                if *align != page_size() as u64
                    || anonymous_count > crate::venus::renderer::MAX_RING_BLOBS
                    || anonymous_bytes > crate::venus::renderer::MAX_RING_BLOB_BYTES
                {
                    return Err(RecipeError::Invalid(
                        "anonymous page roots exceed ring-blob geometry or caps",
                    ));
                }
            }
            owners.push(owner);
        }
        let mut table = Self::default();
        for ((declared, mapped, align, data), owner) in records.into_iter().zip(owners) {
            let pages = Arc::new(if let Some(owner) = owner {
                RingPages::from_checkpoint(declared, mapped, align, &owner)?
            } else {
                let pages = RingPages::new(declared)?;
                if pages.mapped_len() != mapped {
                    return Err(RecipeError::Invalid("anonymous page span changed"));
                }
                pages
            });
            for (chunk, data) in data.chunks(CHUNK).enumerate() {
                pages.write_mapped_bytes(chunk as u64 * CHUNK as u64, data)?;
            }
            table.register(&pages)?;
        }
        Ok(table)
    }

    pub fn encode_links(&mut self, links: &[HostAllocationLink]) -> Result<Vec<u8>, RecipeError> {
        check_links(links)?;
        let mut encoder = Encoder::with_limit(8 + 32 * MAX_PAGES);
        encoder.u32(VERSION)?;
        encoder.u32(links.len() as u32)?;
        for link in links {
            let page = self.register(&link.pages)?;
            for value in [link.device, link.memory, link.allocation_serial] {
                encoder.u64(value)?;
            }
            encoder.u32(page)?;
            encoder.u32(u32::from(link.exported))?;
        }
        Ok(encoder.finish()?)
    }

    pub fn resolve_links(&self, bytes: &[u8]) -> Result<Vec<HostAllocationLink>, RecipeError> {
        if bytes.len() > 8 + 32 * MAX_PAGES {
            return Err(RecipeError::Budget);
        }
        let mut decoder = Decoder::new(bytes);
        let version = decoder.u32()?;
        if version != VERSION {
            return Err(RecipeError::Version(version));
        }
        let count = decoder.u32()? as usize;
        if count > MAX_PAGES || count > decoder.remaining() / 32 {
            return Err(RecipeError::Invalid(
                "host-page link count exceeds its file",
            ));
        }
        let mut links = Vec::new();
        links
            .try_reserve_exact(count)
            .map_err(|_| RecipeError::Budget)?;
        for _ in 0..count {
            let device = decoder.u64()?;
            let memory = decoder.u64()?;
            let allocation_serial = decoder.u64()?;
            let pages = self.page(decoder.u32()?)?.clone();
            let exported = match decoder.u32()? {
                0 => false,
                1 => true,
                _ => return Err(RecipeError::Invalid("invalid host-page export flag")),
            };
            links.push(HostAllocationLink {
                device,
                memory,
                allocation_serial,
                exported,
                pages,
            });
        }
        if decoder.remaining() != 0 {
            return Err(RecipeError::Invalid("trailing host-page link bytes"));
        }
        check_links(&links)?;
        Ok(links)
    }
}

impl<H: HostVulkan> VulkanContext<H> {
    pub fn host_allocation_links(&self) -> Result<Vec<HostAllocationLink>, RecipeError> {
        let mut links = Vec::new();
        for (id, memory) in self.objects.checkpoint_memories() {
            let Some(pages) = &memory.pages else {
                continue;
            };
            if links.len() >= MAX_PAGES {
                return Err(RecipeError::Budget);
            }
            links.try_reserve(1).map_err(|_| RecipeError::Budget)?;
            links.push(HostAllocationLink {
                device: memory.device,
                memory: id,
                allocation_serial: memory.allocation_serial,
                exported: memory.exported,
                pages: pages.clone(),
            });
        }
        links.sort_unstable_by_key(|link| link.allocation_serial);
        Ok(links)
    }

    pub(super) fn restore_linked_pages(
        &mut self,
        args: &mut AllocateMemoryArgs,
        id: u64,
        link: &HostAllocationLink,
        properties: (u32, Option<(u32, u32)>),
        dedicated: Option<DedicatedTo>,
        sharing: (bool, u32),
    ) -> Result<(), ExecError> {
        const NAME: &str = "vkAllocateMemory";
        let (property_flags, flags) = properties;
        let (importing, export_types) = sharing;
        let info = args
            .p_allocate_info
            .as_ref()
            .ok_or_else(|| invalid(NAME, "missing page allocation info"))?;
        let (device, guest) = self
            .objects
            .device_and_guest(args.device.0)
            .map_err(id_error(NAME))?;
        let pages = &link.pages;
        if self.strict_checkpoint_budgets {
            let owner = pages
                .checkpoint_budget()
                .ok_or_else(|| invalid(NAME, "native allocation has anonymous ring pages"))?;
            let whole = self
                .budget
                .whole()
                .ok_or_else(|| invalid(NAME, "context host budget has no factory root"))?;
            if owner
                .whole()
                .is_none_or(|parent| !Arc::ptr_eq(parent, whole))
                || (!importing && !Arc::ptr_eq(owner, &self.budget))
            {
                return Err(invalid(
                    NAME,
                    "restored pages belong to a different allocation owner or factory",
                ));
            }
        }
        let host_type = guest.host_type(info.memory_type_index);
        let alignment = guest.import_alignment.max(page_size() as u64);
        if id != link.memory
            || args.device.0 != link.device
            || self.memory_sequence != link.allocation_serial
            || property_flags & policy::MEMORY_PROPERTY_HOST_VISIBLE == 0
            || device.group_size != 1
            || flags.is_some_and(|(flags, mask)| flags & !3 != 0 || flags & 1 != 0 && mask != 1)
            || pages.is_host_private()
            || pages.is_poisoned()
            || pages.resource_len() != pages.mapped_len()
            || pages.allocation_alignment() < alignment
            || info.allocation_size > pages.mapped_len()
            || (!importing
                && info.allocation_size.checked_next_multiple_of(alignment)
                    != Some(pages.mapped_len()))
            || (!importing
                && export_types != 0
                && (export_types != policy::MEMORY_HANDLE_DMA_BUF || !device.dma_buf()))
            || (importing && (export_types & !0x201 != 0 || !link.exported))
            || self
                .restoring_import
                .is_some_and(|meta| meta.kind != 2 || meta.host_size != pages.mapped_len())
        {
            return Err(invalid(
                NAME,
                format!("restored host-page allocation differs from its recipe: context {}, memory {id:#x}, flags {flags:?}, property flags {property_flags:#x}, size {}, page geometry {}/{}/{}", self.ctx_id, info.allocation_size, pages.resource_len(), pages.mapped_len(), pages.allocation_alignment()),
            ));
        }
        let bits = self
            .host
            .host_pointer_types(&device.host, pages)
            .map_err(|ret| invalid(NAME, format!("restored page import types: {ret}")))?;
        if bits & 1u32.checked_shl(host_type).unwrap_or(0) == 0 {
            return Err(invalid(
                NAME,
                "restored host-page memory type is unavailable",
            ));
        }
        let request = MemoryRequest {
            size: pages.mapped_len(),
            type_index: host_type,
            import: Some(pages.clone()),
            flags,
            dedicated: None,
            export_handle: false,
            import_handle: None,
        };
        match self.host.allocate_memory(&device.host, &request) {
            Ok(host) => {
                self.objects.insert_memory(
                    id,
                    MemoryObject {
                        dedicated_identity: self
                            .checkpoint_dedicated_identity(args.device.0, dedicated),
                        checkpoint_placeholder: false,
                        allocation_serial: self.memory_sequence,
                        shared_handle: None,
                        identity: Arc::new(()),
                        device: args.device.0,
                        host,
                        size: info.allocation_size,
                        host_size: pages.mapped_len(),
                        type_index: info.memory_type_index,
                        property_flags,
                        pages: Some(pages.clone()),
                        exported: link.exported,
                        dedicated,
                        allocate_flags: flags.map_or(0, |(flags, _)| flags),
                        handle: MemoryHandle::None,
                        shared: None,
                        charge: None,
                    },
                );
                args.ret = VK_SUCCESS;
            }
            Err(ret) => args.ret = ret,
        }
        Ok(())
    }
}
