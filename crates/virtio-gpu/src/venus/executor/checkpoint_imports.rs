//! Historical imports need allocation geometry after their old blob is gone.
//! Private stand-ins reconstruct dependent objects/recordings without replaying
//! GPU work. Every stand-in must be destroyed before reconstruction finishes.

use std::sync::Arc;

use super::checkpoint::{RecipeError, RecipeJournal, MAX_RECIPE_BYTES};
use super::context::{id_error, invalid, ExecError};
use super::host::{HostVulkan, MemoryRequest};
use super::objects::{DedicatedTo, MemoryHandle, MemoryObject};
use super::{policy, VulkanContext};
use crate::venus::protocol::*;
use crate::venus::shmem::RingPages;
use crate::venus::wire::{Decoder, Encoder};

const META_COST: u64 = 128;

#[derive(Debug, Clone, Copy)]
pub(super) struct ImportRecipe {
    pub serial: u64,
    pub device: u64,
    pub memory: u64,
    pub host_size: u64,
    /// 1 = native device-local handle; 2 = imported host pages.
    pub kind: u32,
}

pub(super) fn is_import(args: &AllocateMemoryArgs) -> bool {
    args.p_allocate_info.as_ref().is_some_and(|info| {
        info.p_next.iter().any(|link| {
            matches!(
                link,
                VkMemoryAllocateInfoNext::VkImportMemoryResourceInfoMESA(_)
            )
        })
    })
}

impl ImportRecipe {
    pub fn encode(self, encoder: &mut Encoder) -> Result<(), RecipeError> {
        for value in [self.serial, self.device, self.memory, self.host_size] {
            encoder.u64(value)?;
        }
        encoder.u32(self.kind)?;
        Ok(())
    }

    pub fn decode(decoder: &mut Decoder<'_>) -> Result<Self, RecipeError> {
        let value = Self {
            serial: decoder.u64()?,
            device: decoder.u64()?,
            memory: decoder.u64()?,
            host_size: decoder.u64()?,
            kind: decoder.u32()?,
        };
        if value.serial == 0
            || value.device == 0
            || value.memory == 0
            || value.host_size == 0
            || value.host_size % crate::blob::BLOB_PAGE_SIZE != 0
            || !matches!(value.kind, 1 | 2)
        {
            return Err(RecipeError::Invalid("invalid historical import metadata"));
        }
        Ok(value)
    }
}

impl RecipeJournal {
    pub(super) fn note_import(&mut self, meta: ImportRecipe) -> Result<(), RecipeError> {
        if !self.recording() {
            return Ok(());
        }
        if self.charge.amount().saturating_add(META_COST) > MAX_RECIPE_BYTES {
            return Err(RecipeError::Budget);
        }
        self.charge
            .grow(META_COST)
            .map_err(|_| RecipeError::Budget)?;
        if self.imports.insert(meta.serial, meta).is_some() {
            return Err(RecipeError::Invalid("duplicate import generation"));
        }
        Ok(())
    }
}

impl<H: HostVulkan> VulkanContext<H> {
    pub(super) fn import_recipe(&self, command: &Command<'_>) -> Option<ImportRecipe> {
        let Command::AllocateMemory(args) = command else {
            return None;
        };
        if args.ret != VK_SUCCESS || !is_import(args) {
            return None;
        }
        let id = args.p_memory?.0;
        let memory = self.objects.memory(args.device.0, id).ok()?;
        Some(ImportRecipe {
            serial: memory.allocation_serial,
            device: args.device.0,
            memory: id,
            host_size: memory.host_size,
            kind: if memory.pages.is_some() { 2 } else { 1 },
        })
    }

    pub(super) fn restore_import_placeholder(
        &mut self,
        args: &mut AllocateMemoryArgs,
        id: u64,
        meta: ImportRecipe,
        properties: (u32, Option<(u32, u32)>),
        dedicated: Option<DedicatedTo>,
        sharing: (bool, u32),
    ) -> Result<(), ExecError> {
        const NAME: &str = "vkAllocateMemory";
        let (property_flags, flags) = properties;
        let info = args
            .p_allocate_info
            .as_ref()
            .ok_or_else(|| invalid(NAME, "missing import allocation info"))?;
        let (device, guest) = self
            .objects
            .device_and_guest(args.device.0)
            .map_err(id_error(NAME))?;
        let pages_kind = property_flags & policy::MEMORY_PROPERTY_HOST_VISIBLE != 0;
        if !sharing.0
            || sharing.1 & !0x201 != 0
            || meta.serial != self.memory_sequence
            || meta.device != args.device.0
            || meta.memory != id
            || meta.host_size < info.allocation_size
            || pages_kind != (meta.kind == 2)
        {
            return Err(invalid(
                NAME,
                "historical import metadata differs from its allocation",
            ));
        }
        let ty = guest.host_type(info.memory_type_index);
        let pages = if pages_kind {
            let pages = RingPages::for_memory(meta.host_size, guest.import_alignment, &self.budget)
                .map_err(|e| invalid(NAME, e.to_string()))?;
            if pages.mapped_len() != meta.host_size {
                return Err(invalid(NAME, "historical host import alignment differs"));
            }
            let pages = Arc::new(pages);
            let bits = self
                .host
                .host_pointer_types(&device.host, &pages)
                .map_err(|ret| invalid(NAME, format!("historical import host types: {ret}")))?;
            if bits & 1u32.checked_shl(ty).unwrap_or(0) == 0 {
                return Err(invalid(NAME, "historical host import type is unavailable"));
            }
            Some(pages)
        } else {
            if !guest.memory_export {
                return Err(invalid(
                    NAME,
                    "historical device-local imports are unsupported",
                ));
            }
            None
        };
        let charge = if pages.is_none() {
            let memory_type = guest
                .memory
                .memory_types
                .get(info.memory_type_index as usize)
                .ok_or_else(|| invalid(NAME, "invalid historical memory type"))?;
            match self.charge_allocation(guest, memory_type.heap_index, meta.host_size) {
                Ok(charge) => Some(Arc::new(charge)),
                Err(ret) => {
                    args.ret = ret;
                    return Ok(());
                }
            }
        } else {
            None
        };
        let request = MemoryRequest {
            size: meta.host_size,
            type_index: ty,
            import: pages.clone(),
            flags,
            dedicated: None,
            export_handle: !pages_kind,
            import_handle: None,
        };
        match self.host.allocate_memory(&device.host, &request) {
            Ok(host) => {
                self.objects.insert_memory(
                    id,
                    MemoryObject {
                        dedicated_identity: self
                            .checkpoint_dedicated_identity(args.device.0, dedicated),
                        checkpoint_placeholder: true,
                        allocation_serial: self.memory_sequence,
                        shared_handle: None,
                        identity: Arc::new(()),
                        device: args.device.0,
                        host,
                        size: info.allocation_size,
                        host_size: meta.host_size,
                        type_index: info.memory_type_index,
                        property_flags,
                        pages,
                        exported: true,
                        dedicated,
                        allocate_flags: flags.map_or(0, |(flags, _)| flags),
                        handle: if pages_kind {
                            MemoryHandle::None
                        } else {
                            MemoryHandle::Imported
                        },
                        shared: None,
                        charge,
                    },
                );
                args.ret = VK_SUCCESS;
            }
            Err(ret) => args.ret = ret,
        }
        Ok(())
    }
}
