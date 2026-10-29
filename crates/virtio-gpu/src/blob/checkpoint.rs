//! Logical virtio blob names, backing lists and aperture reservations. This
//! component never creates native resources, maps pages or clears their bytes.
//! The renderer archive and restored guest RAM supply those separately.
use super::*;
use crate::protocol::ResourceCreateBlob;
use std::io::{Cursor, Read};

const VERSION: u32 = 1;
const MAX_BYTES: usize = 32 << 20;
const HEADER_BYTES: usize = 16;
const RESOURCE_BYTES: usize = 64;

#[derive(Debug, thiserror::Error)]
pub enum BlobCheckpointError {
    #[error("invalid virtio-gpu blob checkpoint: {0}")]
    Invalid(&'static str),
    #[error("virtio-gpu blob checkpoint exceeds its metadata budget")]
    Budget,
    #[error("truncated virtio-gpu blob checkpoint")]
    Truncated(#[from] std::io::Error),
    #[error("invalid virtio-gpu blob checkpoint resource: {0}")]
    Resource(#[from] CommandError),
}
use BlobCheckpointError::{Budget, Invalid};

/// Fully validated logical state, prepared without touching live guest pages.
#[derive(Debug)]
pub struct BlobCheckpoint {
    table: BlobTable,
}

fn u32_at(input: &mut Cursor<&[u8]>) -> Result<u32, BlobCheckpointError> {
    let mut value = [0; 4];
    input.read_exact(&mut value)?;
    Ok(u32::from_le_bytes(value))
}
fn u64_at(input: &mut Cursor<&[u8]>) -> Result<u64, BlobCheckpointError> {
    let mut value = [0; 8];
    input.read_exact(&mut value)?;
    Ok(u64::from_le_bytes(value))
}

fn scanout_valid(spec: ScanoutBlobSpec, size: u64) -> bool {
    matches!(
        spec.format,
        crate::FORMAT_B8G8R8A8_UNORM | crate::FORMAT_B8G8R8X8_UNORM
    ) && spec.width != 0
        && spec.height != 0
        && u64::from(spec.width) * u64::from(spec.height) <= crate::MAX_RESOURCE_PIXELS
        && u64::from(spec.stride) >= u64::from(spec.width) * u64::from(crate::BYTES_PER_PIXEL)
        && u64::from(spec.offset) + u64::from(spec.stride) * u64::from(spec.height) <= size
}

impl BlobTable {
    /// Bounded metadata only, in deterministic resource-id order. Source
    /// mappings and bytes are unchanged even if encoding fails.
    pub fn save_checkpoint(&self) -> Result<Vec<u8>, BlobCheckpointError> {
        let mut len = HEADER_BYTES;
        if self.len() > MAX_BLOB_RESOURCES {
            return Err(Budget);
        }
        for blob in self.iter() {
            if blob.backing.len() > MAX_BLOB_ENTRIES as usize {
                return Err(Budget);
            }
            len = len
                .checked_add(RESOURCE_BYTES + 12 * blob.backing.len())
                .ok_or(Budget)?;
            if len > MAX_BYTES {
                return Err(Budget);
            }
        }
        let mut out = Vec::new();
        out.try_reserve_exact(len).map_err(|_| Budget)?;
        out.extend_from_slice(&VERSION.to_le_bytes());
        out.extend_from_slice(&self.window.len.to_le_bytes());
        out.extend_from_slice(&(self.len() as u32).to_le_bytes());
        let mut blobs = Vec::new();
        blobs.try_reserve_exact(self.len()).map_err(|_| Budget)?;
        blobs.extend(self.iter());
        blobs.sort_unstable_by_key(|blob| blob.id);
        for blob in blobs {
            for word in [
                blob.id,
                blob.blob_mem,
                blob.blob_flags,
                blob.backing.len() as u32,
            ] {
                out.extend_from_slice(&word.to_le_bytes());
            }
            for word in [blob.blob_id, blob.size, blob.mapped_at.unwrap_or(u64::MAX)] {
                out.extend_from_slice(&word.to_le_bytes());
            }
            out.extend_from_slice(&u32::from(blob.scanout_spec.is_some()).to_le_bytes());
            let spec = blob.scanout_spec.unwrap_or(ScanoutBlobSpec {
                format: 0,
                width: 0,
                height: 0,
                stride: 0,
                offset: 0,
            });
            for word in [
                spec.format,
                spec.width,
                spec.height,
                spec.stride,
                spec.offset,
            ] {
                out.extend_from_slice(&word.to_le_bytes());
            }
            for entry in &blob.backing {
                out.extend_from_slice(&entry.addr.to_le_bytes());
                out.extend_from_slice(&entry.length.to_le_bytes());
            }
        }
        Ok(out)
    }
}

impl BlobCheckpoint {
    /// Parse every record and mapping into an isolated table. All counts are
    /// checked against both policy and remaining bytes before allocation.
    pub fn decode(bytes: &[u8], support: BlobSupport) -> Result<Self, BlobCheckpointError> {
        if bytes.len() > MAX_BYTES {
            return Err(Budget);
        }
        let mut input = Cursor::new(bytes);
        if u32_at(&mut input)? != VERSION {
            return Err(Invalid("unsupported version"));
        }
        let window_len = u64_at(&mut input)?;
        if window_len != support.host_visible_bytes.unwrap_or(0) {
            return Err(Invalid("host-visible window differs from current policy"));
        }
        let count = u32_at(&mut input)? as usize;
        if count > MAX_BLOB_RESOURCES
            || count > bytes.len().saturating_sub(HEADER_BYTES) / RESOURCE_BYTES
        {
            return Err(Budget);
        }
        let mut table = BlobTable::new(window_len);
        table.resources.try_reserve(count).map_err(|_| Budget)?;
        let mut last_id = 0;
        for _ in 0..count {
            let resource_id = u32_at(&mut input)?;
            if resource_id <= last_id {
                return Err(Invalid("resource ids must be nonzero, unique and sorted"));
            }
            last_id = resource_id;
            let blob_mem = u32_at(&mut input)?;
            let blob_flags = u32_at(&mut input)?;
            let nr_entries = u32_at(&mut input)?;
            let blob_id = u64_at(&mut input)?;
            let size = u64_at(&mut input)?;
            let mapped_at = u64_at(&mut input)?;
            let scanout_present = u32_at(&mut input)?;
            let spec = ScanoutBlobSpec {
                format: u32_at(&mut input)?,
                width: u32_at(&mut input)?,
                height: u32_at(&mut input)?,
                stride: u32_at(&mut input)?,
                offset: u32_at(&mut input)?,
            };
            if scanout_present > 1
                || (scanout_present == 0
                    && spec
                        != (ScanoutBlobSpec {
                            format: 0,
                            width: 0,
                            height: 0,
                            stride: 0,
                            offset: 0,
                        }))
                || (scanout_present == 1
                    && (blob_mem == BLOB_MEM_GUEST || !scanout_valid(spec, size)))
            {
                return Err(Invalid("invalid accepted renderer scanout"));
            }
            let left = bytes.len().saturating_sub(input.position() as usize);
            if nr_entries > MAX_BLOB_ENTRIES || nr_entries as usize > left / 12 {
                return Err(Budget);
            }
            let mut entries = Vec::new();
            entries
                .try_reserve_exact(nr_entries as usize)
                .map_err(|_| Budget)?;
            for _ in 0..nr_entries {
                let addr = u64_at(&mut input)?;
                let length = u32_at(&mut input)?;
                if addr.checked_add(u64::from(length)).is_none() {
                    return Err(Invalid("guest backing address wraps"));
                }
                entries.push(MemEntry { addr, length });
            }
            let args = ResourceCreateBlob {
                resource_id,
                blob_mem,
                blob_flags,
                nr_entries,
                blob_id,
                size,
            };
            let backing_len = table.validate(&args, support, &entries)?;
            table.insert(&args, &entries, backing_len)?;
            if mapped_at != u64::MAX {
                if blob_flags & BLOB_FLAG_USE_MAPPABLE == 0 {
                    return Err(Invalid("unmappable blob has an aperture reservation"));
                }
                // Deliberately bypass reserve_mapping: its clear_span belongs
                // to a *new* allocation, never to checkpointed content.
                table.window.reserve(resource_id, mapped_at, size)?;
                table.commit_mapping(resource_id, mapped_at);
            }
            if scanout_present == 1 {
                table.set_scanout_spec(resource_id, spec);
            }
        }
        if input.position() != bytes.len() as u64 {
            return Err(Invalid("trailing bytes"));
        }
        Ok(Self { table })
    }

    /// Used to compare logical names, layouts and publications with the
    /// renderer manifest before any native resource or mapping is adopted.
    pub fn table(&self) -> &BlobTable {
        &self.table
    }

    /// Commit after renderer/RAM restoration succeeds. Preserve the cold
    /// machine's backing object without writing or republishing its pages.
    pub fn restore(mut self, target: &mut BlobTable) -> Result<(), BlobCheckpointError> {
        if !target.is_empty() || target.window.mapping_count() != 0 || target.total_bytes != 0 {
            return Err(Invalid("restore requires a cold logical blob table"));
        }
        if target.window.len != self.table.window.len {
            return Err(Invalid("restore aperture length differs"));
        }
        self.table.window.backing = target.window.backing.clone();
        *target = self.table;
        Ok(())
    }
}

#[cfg(test)]
mod tests;
