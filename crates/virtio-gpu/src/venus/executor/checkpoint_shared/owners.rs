//! Exact owners of exported native allocations. Contents are carried separately;
//! matching metadata alone is insufficient without the fresh charge identity.
use super::*;
use crate::venus::executor::checkpoint_budgets::{BudgetRootKey, BudgetTable};
use crate::venus::executor::memory::HandleExport;
use crate::venus::wire::{Decoder, Encoder};

const VERSION: u32 = 1;
const RECORD_BYTES: usize = 48;
const MAX_BYTES: usize = 8 + MAX_LINKS * RECORD_BYTES;

#[derive(Debug, Clone)]
struct Record {
    owner: u32,
    size: u64,
    type_index: u32,
    uuids: ([u8; 16], [u8; 16]),
}

#[derive(Debug, Clone)]
pub struct PayloadBudgetOwners {
    records: Vec<Record>,
}

impl PayloadBudgetOwners {
    fn validate(&self, budgets: &BudgetTable) -> Result<(), RecipeError> {
        if self.records.len() > MAX_LINKS {
            return Err(RecipeError::Budget);
        }
        for record in &self.records {
            let role = budgets.root_key(record.owner)?;
            if record.size == 0
                || record.type_index >= 32
                || budgets.budget(record.owner)?.whole().is_none()
                || !matches!(role, BudgetRootKey::HostPages | BudgetRootKey::Heap(_))
                || matches!(role, BudgetRootKey::Heap((uuid, _)) if uuid != record.uuids.0)
            {
                return Err(RecipeError::Invalid(
                    "invalid shared allocation owner or geometry",
                ));
            }
        }
        Ok(())
    }

    pub fn len(&self) -> usize {
        self.records.len()
    }
    pub fn is_empty(&self) -> bool {
        self.records.is_empty()
    }
    pub fn owner(&self, payload: u32) -> Result<u32, RecipeError> {
        payload
            .checked_sub(1)
            .and_then(|index| self.records.get(index as usize))
            .map(|record| record.owner)
            .ok_or(RecipeError::Invalid("unknown payload-owner id"))
    }

    pub fn encode(&self, budgets: &BudgetTable) -> Result<Vec<u8>, RecipeError> {
        self.validate(budgets)?;
        let mut encoder = Encoder::with_limit(MAX_BYTES);
        encoder.u32(VERSION)?;
        encoder.u32(self.records.len() as u32)?;
        for record in &self.records {
            encoder.u32(record.owner)?;
            encoder.u64(record.size)?;
            encoder.u32(record.type_index)?;
            encoder.blob(&record.uuids.0)?;
            encoder.blob(&record.uuids.1)?;
        }
        Ok(encoder.finish()?)
    }

    pub fn decode(bytes: &[u8], budgets: &BudgetTable) -> Result<Self, RecipeError> {
        if bytes.len() > MAX_BYTES {
            return Err(RecipeError::Budget);
        }
        let mut decoder = Decoder::new(bytes);
        let version = decoder.u32()?;
        if version != VERSION {
            return Err(RecipeError::Version(version));
        }
        let count = decoder.u32()? as usize;
        if count > MAX_LINKS || count > decoder.remaining() / RECORD_BYTES {
            return Err(RecipeError::Budget);
        }
        let mut records = Vec::new();
        records
            .try_reserve_exact(count)
            .map_err(|_| RecipeError::Budget)?;
        for _ in 0..count {
            let owner = decoder.u32()?;
            let size = decoder.u64()?;
            let type_index = decoder.u32()?;
            let mut uuids = ([0; 16], [0; 16]);
            uuids.0.copy_from_slice(decoder.blob(16)?);
            uuids.1.copy_from_slice(decoder.blob(16)?);
            records.push(Record {
                owner,
                size,
                type_index,
                uuids,
            });
        }
        if decoder.remaining() != 0 {
            return Err(RecipeError::Invalid("trailing shared owner data"));
        }
        let saved = Self { records };
        saved.validate(budgets)?;
        Ok(saved)
    }

    /// Check actual new backend allocations and charges before object imports.
    /// A forged file cannot substitute an equal-sized foreign or uncharged handle.
    pub fn validate_handles<H: HostVulkan>(
        &self,
        payloads: &SharedPayloads,
        budgets: &BudgetTable,
    ) -> Result<(), RecipeError> {
        self.validate(budgets)?;
        if payloads.handles().len() != self.records.len() {
            return Err(RecipeError::Invalid(
                "shared payload count differs from owner inventory",
            ));
        }
        for (record, handle) in self.records.iter().zip(payloads.handles()) {
            let expected_owner = budgets.budget(record.owner)?;
            let export = handle
                .0
                .downcast_ref::<HandleExport<H>>()
                .ok_or(RecipeError::Invalid(
                    "payload owner belongs to another native backend",
                ))?;
            if export.size != record.size
                || export.type_index != record.type_index
                || export.uuids != record.uuids
                || export
                    .charge
                    .as_ref()
                    .and_then(|charge| charge.budget())
                    .is_none_or(|owner| !Arc::ptr_eq(owner, expected_owner))
            {
                return Err(RecipeError::Invalid(
                    "reconstructed shared payload has different geometry or owner",
                ));
            }
        }
        Ok(())
    }
}

impl SharedPayloads {
    pub fn checkpoint_budget_owners<H: HostVulkan>(
        &self,
        budgets: &mut BudgetTable,
    ) -> Result<PayloadBudgetOwners, RecipeError> {
        let mut records = Vec::new();
        records
            .try_reserve_exact(self.handles().len())
            .map_err(|_| RecipeError::Budget)?;
        for handle in self.handles() {
            let export = handle
                .0
                .downcast_ref::<HandleExport<H>>()
                .ok_or(RecipeError::Invalid(
                    "shared payload belongs to another backend",
                ))?;
            let owner = export
                .charge
                .as_ref()
                .and_then(|charge| charge.budget())
                .ok_or(RecipeError::Invalid(
                    "shared payload has no allocation charge",
                ))?;
            records.push(Record {
                owner: budgets.register(owner)?,
                size: export.size,
                type_index: export.type_index,
                uuids: export.uuids,
            });
        }
        let saved = PayloadBudgetOwners { records };
        saved.validate(budgets)?;
        Ok(saved)
    }
}

#[cfg(test)]
mod tests;
