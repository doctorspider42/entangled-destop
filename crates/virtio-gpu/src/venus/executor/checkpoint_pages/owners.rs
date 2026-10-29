//! Host-page roots retain the allocating share, even after the original guest
//! memory object/context goes away. Importers never charge these bytes again.

use super::*;
use crate::venus::executor::checkpoint_budgets::{BudgetRootKey, BudgetTable};

#[derive(Debug, Clone)]
pub struct PageBudgetOwners {
    owners: Vec<u32>,
}

impl PageBudgetOwners {
    pub fn validate_pages(
        &self,
        pages: &HostPageTable,
        budgets: &BudgetTable,
    ) -> Result<(), RecipeError> {
        self.validate(budgets)?;
        if pages.pages().len() != self.len() {
            return Err(RecipeError::Invalid(
                "restored page count differs from owner inventory",
            ));
        }
        for (index, page) in pages.pages().iter().enumerate() {
            let owner = self.owner(index as u32 + 1)?;
            let matches = match (owner, page.checkpoint_budget()) {
                (0, None) => true,
                (0, Some(_)) | (_, None) => false,
                (id, Some(actual)) => Arc::ptr_eq(actual, budgets.budget(id)?),
            };
            if !matches {
                return Err(RecipeError::Invalid(
                    "restored page has a different allocation owner",
                ));
            }
        }
        Ok(())
    }
    fn validate(&self, budgets: &BudgetTable) -> Result<(), RecipeError> {
        if self.owners.len() > MAX_PAGES {
            return Err(RecipeError::Budget);
        }
        for owner in &self.owners {
            if *owner != 0
                && (budgets.root_key(*owner)? != BudgetRootKey::HostPages
                    || budgets.budget(*owner)?.whole().is_none())
            {
                return Err(RecipeError::Invalid(
                    "page owner is not a factory host-page share",
                ));
            }
        }
        Ok(())
    }

    pub fn len(&self) -> usize {
        self.owners.len()
    }
    pub fn is_empty(&self) -> bool {
        self.owners.is_empty()
    }

    pub fn owner(&self, page: u32) -> Result<u32, RecipeError> {
        page.checked_sub(1)
            .and_then(|index| self.owners.get(index as usize))
            .copied()
            .ok_or(RecipeError::Invalid("unknown page-owner id"))
    }

    pub fn encode(&self, budgets: &BudgetTable) -> Result<Vec<u8>, RecipeError> {
        self.validate(budgets)?;
        let mut encoder = Encoder::with_limit(8 + MAX_PAGES * 4);
        encoder.u32(VERSION)?;
        encoder.u32(self.owners.len() as u32)?;
        for id in &self.owners {
            encoder.u32(*id)?;
        }
        Ok(encoder.finish()?)
    }

    pub fn decode(bytes: &[u8], budgets: &BudgetTable) -> Result<Self, RecipeError> {
        if bytes.len() > 8 + MAX_PAGES * 4 {
            return Err(RecipeError::Budget);
        }
        let mut decoder = Decoder::new(bytes);
        let version = decoder.u32()?;
        if version != VERSION {
            return Err(RecipeError::Version(version));
        }
        let count = decoder.u32()? as usize;
        if count > MAX_PAGES || count > decoder.remaining() / 4 {
            return Err(RecipeError::Budget);
        }
        let mut owners = Vec::new();
        owners
            .try_reserve_exact(count)
            .map_err(|_| RecipeError::Budget)?;
        for _ in 0..count {
            owners.push(decoder.u32()?);
        }
        if decoder.remaining() != 0 {
            return Err(RecipeError::Invalid("trailing page-owner data"));
        }
        let owners = Self { owners };
        owners.validate(budgets)?;
        Ok(owners)
    }
}

impl HostPageTable {
    /// Restore a complete page component without buffering its allocation
    /// bytes. The reader must be a bounded section or a standalone component
    /// file. Validate all framing/owners before the first allocation.
    pub fn restore_owned_reader(
        reader: &mut (impl std::io::Read + std::io::Seek),
        owners: &PageBudgetOwners,
        budgets: &BudgetTable,
    ) -> Result<Self, RecipeError> {
        use std::io::SeekFrom;
        owners.validate(budgets)?;
        let start = reader.stream_position()?;
        let end = reader.seek(SeekFrom::End(0))?;
        reader.seek(SeekFrom::Start(start))?;
        let mut header = [0; 8];
        reader.read_exact(&mut header)?;
        let version = u32::from_le_bytes(
            header[..4]
                .try_into()
                .map_err(|_| RecipeError::Invalid("page header"))?,
        );
        if version != VERSION {
            return Err(RecipeError::Version(version));
        }
        let count = u32::from_le_bytes(
            header[4..]
                .try_into()
                .map_err(|_| RecipeError::Invalid("page count"))?,
        ) as usize;
        if count != owners.len() || count > MAX_PAGES {
            return Err(RecipeError::Invalid(
                "page stream and owner inventory differ",
            ));
        }
        let mut records = Vec::new();
        records
            .try_reserve_exact(count)
            .map_err(|_| RecipeError::Budget)?;
        let mut anonymous_count = 0;
        let mut anonymous_bytes = 0_u64;
        for index in 0..count {
            let mut bytes = [0; 24];
            reader.read_exact(&mut bytes)?;
            let mut words = bytes.chunks_exact(8).map(|word| {
                let mut out = [0; 8];
                out.copy_from_slice(word);
                u64::from_le_bytes(out)
            });
            let declared = words
                .next()
                .ok_or(RecipeError::Invalid("page stream declaration"))?;
            let mapped = words
                .next()
                .ok_or(RecipeError::Invalid("page stream span"))?;
            let align = words
                .next()
                .ok_or(RecipeError::Invalid("page stream alignment"))?;
            geometry(declared, mapped, align)?;
            let offset = reader.stream_position()?;
            let next = offset
                .checked_add(mapped)
                .filter(|next| *next <= end)
                .ok_or(RecipeError::Invalid(
                    "page stream allocation exceeds the section",
                ))?;
            let owner = owners.owner(index as u32 + 1)?;
            let owner = if owner == 0 {
                anonymous_count += 1;
                anonymous_bytes = anonymous_bytes
                    .checked_add(declared)
                    .ok_or(RecipeError::Budget)?;
                if align != page_size() as u64
                    || anonymous_count > crate::venus::renderer::MAX_RING_BLOBS
                    || anonymous_bytes > crate::venus::renderer::MAX_RING_BLOB_BYTES
                {
                    return Err(RecipeError::Invalid(
                        "anonymous page stream exceeds ring-blob geometry or caps",
                    ));
                }
                None
            } else {
                Some(budgets.budget(owner)?.clone())
            };
            records.push((declared, mapped, align, offset, owner));
            reader.seek(SeekFrom::Start(next))?;
        }
        if reader.stream_position()? != end {
            return Err(RecipeError::Invalid("trailing page stream bytes"));
        }
        let mut pages = Self::default();
        let mut chunk = [0; CHUNK];
        for (declared, mapped, align, offset, owner) in records {
            let page = Arc::new(if let Some(owner) = owner {
                RingPages::from_checkpoint(declared, mapped, align, &owner)?
            } else {
                let page = RingPages::new(declared)?;
                if page.mapped_len() != mapped {
                    return Err(RecipeError::Invalid("anonymous page stream span changed"));
                }
                page
            });
            reader.seek(SeekFrom::Start(offset))?;
            let mut at = 0;
            while at < mapped {
                let take = (mapped - at).min(CHUNK as u64) as usize;
                reader.read_exact(&mut chunk[..take])?;
                page.write_mapped_bytes(at, &chunk[..take])?;
                at += take as u64;
            }
            pages.register(&page)?;
        }
        reader.seek(SeekFrom::Start(end))?;
        Ok(pages)
    }

    pub fn checkpoint_budget_owners(
        &self,
        budgets: &mut BudgetTable,
    ) -> Result<PageBudgetOwners, RecipeError> {
        let mut owners = Vec::new();
        owners
            .try_reserve_exact(self.pages.len())
            .map_err(|_| RecipeError::Budget)?;
        for page in &self.pages {
            owners.push(match page.checkpoint_budget() {
                Some(budget) => budgets.register(budget)?,
                None => 0,
            });
        }
        let owners = PageBudgetOwners { owners };
        owners.validate(budgets)?;
        Ok(owners)
    }

    /// Owner zero is a renderer-budgeted ring blob, not an uncharged native
    /// allocation. It retains RingPages::new's size/alignment/aggregate caps.
    /// The enclosing factory also cross-checks anonymous roots against its
    /// blob manifest and refuses native allocation links to an anonymous root.
    pub fn restore_owned(
        bytes: &[u8],
        owners: &PageBudgetOwners,
        budgets: &BudgetTable,
    ) -> Result<Self, RecipeError> {
        owners.validate(budgets)?;
        let mut decoder = Decoder::new(bytes);
        if decoder.u32()? != VERSION || decoder.u32()? as usize != owners.len() {
            return Err(RecipeError::Invalid("page data and owner inventory differ"));
        }
        Self::restore_with_owners(bytes, |page| {
            let owner = owners.owner(page)?;
            if owner == 0 {
                Ok(None)
            } else {
                Ok(Some(budgets.budget(owner)?.clone()))
            }
        })
    }
}

#[cfg(test)]
mod tests;
