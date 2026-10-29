use std::collections::{HashMap, HashSet};

use super::{RecipeError, SharedAllocationLink, SharedHandle, MAX_LINKS};
use crate::venus::wire::{Decoder, Encoder};

const VERSION: u32 = 1;
const MAX_LINK_BYTES: usize = 8 + 28 * MAX_LINKS;

/// Per-checkpoint shared-payload namespace. Capture registers handles from all
/// contexts and blobs in this one table. Only its dense ids enter the file.
/// Restore recreates each payload once, in table order, before resolving links.
#[derive(Default)]
pub struct SharedPayloads {
    by_identity: HashMap<usize, u32>,
    handles: Vec<SharedHandle>,
}

pub(super) fn check_links(links: &[SharedAllocationLink]) -> Result<(), RecipeError> {
    if links.len() > MAX_LINKS {
        return Err(RecipeError::Budget);
    }
    let mut serials = HashSet::new();
    let mut ids = HashSet::new();
    serials
        .try_reserve(links.len())
        .map_err(|_| RecipeError::Budget)?;
    ids.try_reserve(links.len())
        .map_err(|_| RecipeError::Budget)?;
    for link in links {
        if link.device == 0
            || link.memory == 0
            || link.allocation_serial == 0
            || !serials.insert(link.allocation_serial)
            || !ids.insert(link.memory)
        {
            return Err(RecipeError::Invalid(
                "invalid or duplicate shared allocation link",
            ));
        }
    }
    Ok(())
}

impl SharedPayloads {
    /// Stable file id, starting at one. Pointer identity is used only in RAM.
    pub fn register(&mut self, handle: &SharedHandle) -> Result<u32, RecipeError> {
        if let Some(id) = self.by_identity.get(&handle.key()) {
            return Ok(*id);
        }
        if self.handles.len() >= MAX_LINKS {
            return Err(RecipeError::Budget);
        }
        self.by_identity
            .try_reserve(1)
            .map_err(|_| RecipeError::Budget)?;
        self.handles
            .try_reserve(1)
            .map_err(|_| RecipeError::Budget)?;
        let id = self.handles.len() as u32 + 1;
        self.handles.push(handle.clone());
        self.by_identity.insert(handle.key(), id);
        Ok(id)
    }

    /// Payloads in id order, to be saved/recreated exactly once each. This is
    /// runtime state; a writer must serialize their contents, never these handles.
    pub fn handles(&self) -> &[SharedHandle] {
        &self.handles
    }

    /// Resolve a manifest root in the same global namespace as allocation links.
    pub fn handle(&self, id: u32) -> Result<&SharedHandle, RecipeError> {
        id.checked_sub(1)
            .and_then(|index| self.handles.get(index as usize))
            .ok_or(RecipeError::Invalid("unknown shared payload id"))
    }

    /// Bind the file's payload ids to already reconstructed native handles.
    /// Distinct file ids must not accidentally resolve to the same allocation.
    pub fn from_handles(handles: Vec<SharedHandle>) -> Result<Self, RecipeError> {
        if handles.len() > MAX_LINKS {
            return Err(RecipeError::Budget);
        }
        let mut table = Self::default();
        for (index, handle) in handles.into_iter().enumerate() {
            if table.register(&handle)? != index as u32 + 1 {
                return Err(RecipeError::Invalid(
                    "distinct shared payload ids resolve to one handle",
                ));
            }
        }
        Ok(table)
    }

    /// Encode one context's allocation generations against the global table.
    pub fn encode_links(&mut self, links: &[SharedAllocationLink]) -> Result<Vec<u8>, RecipeError> {
        check_links(links)?;
        let mut encoder = Encoder::with_limit(MAX_LINK_BYTES);
        encoder.u32(VERSION)?;
        encoder.u32(links.len() as u32)?;
        for link in links {
            let payload = self.register(&link.handle)?;
            encoder.u64(link.device)?;
            encoder.u64(link.memory)?;
            encoder.u64(link.allocation_serial)?;
            encoder.u32(payload)?;
        }
        Ok(encoder.finish()?)
    }

    /// Fully validate and resolve a context link file before recipe replay.
    pub fn resolve_links(&self, bytes: &[u8]) -> Result<Vec<SharedAllocationLink>, RecipeError> {
        if bytes.len() > MAX_LINK_BYTES {
            return Err(RecipeError::Budget);
        }
        let mut decoder = Decoder::new(bytes);
        let version = decoder.u32()?;
        if version != VERSION {
            return Err(RecipeError::Version(version));
        }
        let count = decoder.u32()? as usize;
        if count > MAX_LINKS || count > decoder.remaining() / 28 {
            return Err(RecipeError::Invalid(
                "shared allocation link count exceeds its file",
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
            let payload = decoder.u32()?;
            let index = payload
                .checked_sub(1)
                .ok_or(RecipeError::Invalid("zero shared payload id"))?;
            let handle = self
                .handles
                .get(index as usize)
                .ok_or(RecipeError::Invalid("unknown shared payload id"))?
                .clone();
            links.push(SharedAllocationLink {
                device,
                memory,
                allocation_serial,
                handle,
            });
        }
        if decoder.remaining() != 0 {
            return Err(RecipeError::Invalid(
                "trailing shared allocation link bytes",
            ));
        }
        check_links(&links)?;
        Ok(links)
    }
}
