//! Renderer-owned blob names, accounting and guest aperture publications.
//! Directory/allocation identities stay in the shared global checkpoint tables.
//! Native contexts and payload bytes must already be reconstructed at load.

use std::collections::HashSet;

use super::checkpoint_directory::{DirectoryCheckpoint, SavedScanoutImage};
use super::*;
use crate::blob::{MAX_BLOB_BYTES, MAX_BLOB_RESOURCES, MAX_TOTAL_BLOB_BYTES};
use crate::venus::executor::checkpoint::RecipeError;
use crate::venus::executor::checkpoint_pages::HostPageTable;
use crate::venus::executor::checkpoint_shared::SharedPayloads;
use crate::venus::shmem::MIN_PAGE_BYTES;
use crate::venus::wire::{Decoder, Encoder};

const VERSION: u32 = 1;
const MAX_DIRECTORY_BYTES: usize = 32 << 20;
const MAX_BYTES: usize = MAX_DIRECTORY_BYTES + (2 << 20);

#[derive(Debug)]
struct Blob {
    resource: u32,
    owner: u32,
    blob_id: u64,
    size: u64,
    kind: BlobKind,
    publication: Option<(u64, u64)>,
    scanout: Option<(ScanoutBlobSpec, Option<CanonicalImage>)>,
}

#[derive(Debug)]
pub struct BlobCheckpointSet {
    directory: DirectoryCheckpoint,
    host_visible_bytes: u64,
    limits: HostBlobLimits,
    blobs: Vec<Blob>,
}

fn invalid(reason: &'static str) -> RecipeError {
    RecipeError::Invalid(reason)
}

fn reserve<T>(vec: &mut Vec<T>, count: usize) -> Result<(), RecipeError> {
    vec.try_reserve(count).map_err(|_| RecipeError::Budget)
}

fn boolean(decoder: &mut Decoder<'_>) -> Result<bool, RecipeError> {
    match decoder.u32()? {
        0 => Ok(false),
        1 => Ok(true),
        _ => Err(invalid("invalid blob manifest boolean")),
    }
}

fn canonical_encode(image: &CanonicalImage, encoder: &mut Encoder) -> Result<(), RecipeError> {
    for word in [
        image.format as u32,
        image.flags,
        image.usage,
        image.width,
        image.height,
    ] {
        encoder.u32(word)?;
    }
    encoder.u32(image.view_formats.len() as u32)?;
    for format in &image.view_formats {
        encoder.i32(*format)?;
    }
    Ok(())
}

fn canonical_decode(decoder: &mut Decoder<'_>) -> Result<CanonicalImage, RecipeError> {
    let format = decoder.i32()?;
    let flags = decoder.u32()?;
    let usage = decoder.u32()?;
    let width = decoder.u32()?;
    let height = decoder.u32()?;
    let count = decoder.u32()? as usize;
    if count > 64 {
        return Err(RecipeError::Budget);
    }
    let mut view_formats = Vec::new();
    reserve(&mut view_formats, count)?;
    for _ in 0..count {
        view_formats.push(decoder.i32()?);
    }
    Ok(CanonicalImage {
        format,
        flags,
        usage,
        width,
        height,
        view_formats,
    })
}

#[derive(Default)]
struct Accounting {
    host_count: usize,
    host_bytes: u64,
    host_by_context: HashMap<u32, (usize, u64)>,
    memory_count: usize,
    memory_by_context: HashMap<u32, usize>,
}

impl BlobCheckpointSet {
    /// Compare the virtio front end with this native-resource inventory before
    /// restoration publishes any pages. Guest-only resources have no renderer
    /// object; every host resource must agree in both directions.
    pub fn validate_frontend(&self, table: &crate::blob::BlobTable) -> Result<(), RecipeError> {
        self.validate()?;
        if table.window().len() != self.host_visible_bytes
            || table.iter().filter(|blob| blob.is_host()).count() != self.blobs.len()
        {
            return Err(invalid(
                "front-end/native blob inventory or aperture differs",
            ));
        }
        for saved in &self.blobs {
            let blob = table
                .get(saved.resource)
                .filter(|blob| blob.is_host())
                .ok_or(invalid("native blob has no front-end host resource"))?;
            if blob.blob_id() != saved.blob_id
                || blob.size() != saved.size
                || blob.scanout_spec() != saved.scanout.as_ref().map(|(spec, _)| *spec)
                || blob.mapped_at().map(|offset| (offset, blob.size())) != saved.publication
            {
                return Err(invalid(
                    "front-end/native blob identity, scanout or publication differs",
                ));
            }
        }
        Ok(())
    }

    /// An uncharged root is only a renderer-budgeted ring/reply blob. Native
    /// imports and allocations must retain a factory host-page owner instead.
    pub fn validate_page_owners(
        &self,
        owners: &crate::venus::executor::checkpoint_pages::PageBudgetOwners,
    ) -> Result<(), RecipeError> {
        self.validate()?;
        let mut roles = HashMap::new();
        roles
            .try_reserve(self.blobs.len())
            .map_err(|_| RecipeError::Budget)?;
        for blob in &self.blobs {
            roles.insert(blob.resource, blob.kind);
        }
        let mut anonymous = HashSet::new();
        anonymous
            .try_reserve(self.blobs.len())
            .map_err(|_| RecipeError::Budget)?;
        for (resource, page, native) in self.directory.page_bindings() {
            let kind = roles
                .get(&resource)
                .ok_or(invalid("directory page has no blob record"))?;
            if (!native && *kind != BlobKind::Shm)
                || (native && *kind != BlobKind::Memory)
                || ((owners.owner(page)? == 0) == native)
            {
                return Err(invalid(
                    "blob page allocation owner/kind differs from its directory",
                ));
            }
            if !native {
                anonymous.insert(page);
            }
        }
        for page in 1..=owners.len() as u32 {
            if owners.owner(page)? == 0 && !anonymous.contains(&page) {
                return Err(invalid("anonymous page root is not a renderer host blob"));
            }
        }
        Ok(())
    }

    /// File-visible aperture ranges, never the source process's host pointers.
    pub fn mappings(&self) -> impl Iterator<Item = (u32, u64, u64)> + '_ {
        self.blobs.iter().filter_map(|blob| {
            blob.publication
                .map(|(offset, len)| (blob.resource, offset, len))
        })
    }

    fn validate(&self) -> Result<Accounting, RecipeError> {
        if self.blobs.len() > MAX_BLOB_RESOURCES {
            return Err(RecipeError::Budget);
        }
        let defaults = HostBlobLimits::default();
        if self.host_visible_bytes == 0
            || self.limits.blobs > defaults.blobs
            || self.limits.bytes > defaults.bytes
            || self.limits.blobs_per_context > defaults.blobs_per_context
            || self.limits.bytes_per_context > defaults.bytes_per_context
        {
            return Err(invalid("invalid blob manifest capacity"));
        }
        let mut accounting = Accounting::default();
        let mut resources = HashSet::new();
        let mut total = 0_u64;
        let mut ranges = Vec::new();
        reserve(&mut ranges, self.blobs.len())?;
        for blob in &self.blobs {
            if blob.resource == 0
                || !resources.insert(blob.resource)
                || blob.size == 0
                || blob.size > MAX_BLOB_BYTES
                || ((blob.kind == BlobKind::Shm) != (blob.blob_id == 0))
            {
                return Err(invalid("invalid blob identity, kind or size"));
            }
            total = total
                .checked_add(blob.size)
                .filter(|total| *total <= MAX_TOTAL_BLOB_BYTES)
                .ok_or(RecipeError::Budget)?;
            if blob.kind == BlobKind::Shm {
                accounting.host_count += 1;
                accounting.host_bytes += blob.size;
                let share = accounting.host_by_context.entry(blob.owner).or_default();
                share.0 += 1;
                share.1 += blob.size;
                if accounting.host_count > self.limits.blobs
                    || accounting.host_bytes > self.limits.bytes
                    || share.0 > self.limits.blobs_per_context
                    || share.1 > self.limits.bytes_per_context
                {
                    return Err(RecipeError::Budget);
                }
            } else {
                accounting.memory_count += 1;
                let share = accounting.memory_by_context.entry(blob.owner).or_default();
                *share += 1;
                if accounting.memory_count > MAX_MEMORY_BLOBS
                    || *share > MAX_MEMORY_BLOBS_PER_CONTEXT
                {
                    return Err(RecipeError::Budget);
                }
            }
            if let Some((offset, len)) = blob.publication {
                let end = offset
                    .checked_add(len)
                    .filter(|end| *end <= self.host_visible_bytes)
                    .ok_or(invalid("blob publication exceeds the saved aperture"))?;
                if blob.kind == BlobKind::Handle
                    || len != blob.size
                    || offset % MIN_PAGE_BYTES as u64 != 0
                    || len % MIN_PAGE_BYTES as u64 != 0
                {
                    return Err(invalid("invalid saved blob publication"));
                }
                ranges.push((offset, end));
            }
            if let Some((spec, image)) = &blob.scanout {
                if blob.kind == BlobKind::Shm
                    || spec.width == 0
                    || spec.height == 0
                    || u64::from(spec.width) * u64::from(spec.height) > crate::MAX_RESOURCE_PIXELS
                    || !matches!(
                        spec.format,
                        crate::FORMAT_B8G8R8A8_UNORM | crate::FORMAT_B8G8R8X8_UNORM
                    )
                    || u64::from(spec.stride) < u64::from(spec.width) * 4
                    || (image.is_some() != (blob.kind == BlobKind::Handle))
                {
                    return Err(invalid("invalid saved scanout geometry or kind"));
                }
                if let Some(image) = image {
                    if image.width == 0 || image.height == 0 || image.view_formats.len() > 64 {
                        return Err(invalid("invalid saved canonical scanout"));
                    }
                }
            }
        }
        ranges.sort_unstable();
        if ranges.windows(2).any(|pair| pair[0].1 > pair[1].0) {
            return Err(invalid("saved blob publications overlap"));
        }
        Ok(accounting)
    }

    pub fn encode(&self) -> Result<Vec<u8>, RecipeError> {
        self.validate()?;
        let directory = self.directory.encode()?;
        let mut encoder = Encoder::with_limit(MAX_BYTES);
        encoder.u32(VERSION)?;
        encoder.u64(self.host_visible_bytes)?;
        for value in [
            self.limits.blobs as u64,
            self.limits.bytes,
            self.limits.blobs_per_context as u64,
            self.limits.bytes_per_context,
        ] {
            encoder.u64(value)?;
        }
        encoder.u32(directory.len() as u32)?;
        encoder.blob(&directory)?;
        encoder.u32(self.blobs.len() as u32)?;
        for blob in &self.blobs {
            encoder.u32(blob.resource)?;
            encoder.u32(blob.owner)?;
            encoder.u64(blob.blob_id)?;
            encoder.u64(blob.size)?;
            encoder.u32(match blob.kind {
                BlobKind::Shm => 0,
                BlobKind::Memory => 1,
                BlobKind::Handle => 2,
            })?;
            encoder.u32(u32::from(blob.publication.is_some()))?;
            if let Some((offset, len)) = blob.publication {
                encoder.u64(offset)?;
                encoder.u64(len)?;
            }
            encoder.u32(u32::from(blob.scanout.is_some()))?;
            if let Some((spec, image)) = &blob.scanout {
                for word in [
                    spec.format,
                    spec.width,
                    spec.height,
                    spec.stride,
                    spec.offset,
                ] {
                    encoder.u32(word)?;
                }
                encoder.u32(u32::from(image.is_some()))?;
                if let Some(image) = image {
                    canonical_encode(image, &mut encoder)?;
                }
            }
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
        let host_visible_bytes = decoder.u64()?;
        let blobs = usize::try_from(decoder.u64()?).map_err(|_| RecipeError::Budget)?;
        let blob_bytes = decoder.u64()?;
        let blobs_per_context = usize::try_from(decoder.u64()?).map_err(|_| RecipeError::Budget)?;
        let bytes_per_context = decoder.u64()?;
        let limits = HostBlobLimits {
            blobs,
            bytes: blob_bytes,
            blobs_per_context,
            bytes_per_context,
        };
        let len = decoder.u32()? as usize;
        if len > MAX_DIRECTORY_BYTES {
            return Err(RecipeError::Budget);
        }
        let directory = DirectoryCheckpoint::decode(decoder.blob(len)?)?;
        let count = decoder.u32()? as usize;
        if count > MAX_BLOB_RESOURCES {
            return Err(RecipeError::Budget);
        }
        let mut blobs = Vec::new();
        reserve(&mut blobs, count)?;
        for _ in 0..count {
            let resource = decoder.u32()?;
            let owner = decoder.u32()?;
            let blob_id = decoder.u64()?;
            let size = decoder.u64()?;
            let kind = match decoder.u32()? {
                0 => BlobKind::Shm,
                1 => BlobKind::Memory,
                2 => BlobKind::Handle,
                _ => return Err(invalid("unknown saved blob kind")),
            };
            let publication = if boolean(&mut decoder)? {
                Some((decoder.u64()?, decoder.u64()?))
            } else {
                None
            };
            let scanout = if boolean(&mut decoder)? {
                let spec = ScanoutBlobSpec {
                    format: decoder.u32()?,
                    width: decoder.u32()?,
                    height: decoder.u32()?,
                    stride: decoder.u32()?,
                    offset: decoder.u32()?,
                };
                let image = if boolean(&mut decoder)? {
                    Some(canonical_decode(&mut decoder)?)
                } else {
                    None
                };
                Some((spec, image))
            } else {
                None
            };
            blobs.push(Blob {
                resource,
                owner,
                blob_id,
                size,
                kind,
                publication,
                scanout,
            });
        }
        if decoder.position() != bytes.len() {
            return Err(invalid("trailing blob manifest data"));
        }
        let saved = Self {
            directory,
            host_visible_bytes,
            limits,
            blobs,
        };
        saved.validate()?;
        Ok(saved)
    }
}

impl<F: SinkFactory> VenusRenderer<F> {
    /// All guest/ring writers must be parked; save pages and factory payloads
    /// through the same namespace after this metadata has been captured.
    pub fn checkpoint_blobs(
        &self,
        pages: &mut HostPageTable,
        payloads: &mut SharedPayloads,
    ) -> Result<BlobCheckpointSet, RecipeError> {
        if !self.quiesce.is_paused() || !self.quiesce.wait_until_idle(Duration::from_secs(5)) {
            return Err(invalid("blob capture requires a paused, settled renderer"));
        }
        let directory = self.directory.checkpoint(pages, payloads)?;
        let mut blobs = Vec::new();
        reserve(&mut blobs, self.blobs.len())?;
        for (resource, blob) in &self.blobs {
            self.sinks
                .validate_checkpoint_blob(&blob.backing, blob.size)
                .map_err(RecipeError::Replay)?;
            blobs.push(Blob {
                resource: *resource,
                owner: blob.ctx_id,
                blob_id: blob.blob_id,
                size: blob.size,
                kind: blob.kind,
                publication: blob
                    .publication
                    .as_ref()
                    .map(|mapping| (mapping.offset(), mapping.len())),
                scanout: blob
                    .scanout
                    .as_ref()
                    .map(|scanout| (scanout.spec, scanout.image.clone())),
            });
        }
        blobs.sort_unstable_by_key(|blob| blob.resource);
        let saved = BlobCheckpointSet {
            directory,
            host_visible_bytes: self.host_visible_bytes,
            limits: self.host_blob_limits,
            blobs,
        };
        saved.validate()?;
        self.directory.with(|state| -> Result<(), RecipeError> {
            if state.blobs.len() != saved.blobs.len() {
                return Err(invalid("source directory/blob inventory differs"));
            }
            for blob in &saved.blobs {
                let entry = state
                    .blobs
                    .get(&blob.resource)
                    .ok_or(invalid("source blob is absent from its directory"))?;
                if let Some((spec, Some(canonical))) = &blob.scanout {
                    let matches = entry.memory.as_ref().is_some_and(|memory| {
                        memory.images.iter().any(|image| {
                            image.is_alive()
                                && image.canonical == *canonical
                                && scanout_mismatch(image, spec).is_none()
                        })
                    });
                    if !matches {
                        return Err(invalid(
                            "scanout has no live canonical image recipe to reconstruct",
                        ));
                    }
                }
            }
            Ok(())
        })?;
        Ok(saved)
    }

    /// Adopt into a fresh renderer under a closed gate. Validate the full
    /// manifest, resolved backing, aperture ranges and native image associations
    /// before publishing anything. On failure publications/scanout preparations
    /// are rolled back; discard the partial cold reconstruction's contexts.
    pub fn restore_blobs(
        &mut self,
        saved: &BlobCheckpointSet,
        pages: &HostPageTable,
        payloads: &SharedPayloads,
        image: impl FnMut(&SavedScanoutImage, &SharedHandle) -> Result<ScanoutImage, RecipeError>,
    ) -> Result<(), RecipeError> {
        if !self.quiesce.is_paused()
            || !self.blobs.is_empty()
            || !self.contexts.is_empty()
            || self.live.count() != 0
            || self.directory.with(|state| !state.blobs.is_empty())
        {
            return Err(invalid("blob restore requires a fresh paused renderer"));
        }
        let accounting = saved.validate()?;
        if saved.host_visible_bytes != self.host_visible_bytes
            || saved.limits != self.host_blob_limits
        {
            return Err(invalid("renderer aperture or blob limits changed"));
        }
        let directory = BlobDirectory::from_checkpoint(&saved.directory, pages, payloads, image)?;
        let mut restored = HashMap::new();
        restored
            .try_reserve(saved.blobs.len())
            .map_err(|_| RecipeError::Budget)?;
        directory.with(|state| -> Result<(), RecipeError> {
            if state.blobs.len() != saved.blobs.len() {
                return Err(invalid("directory/blob inventory differs"));
            }
            for blob in &saved.blobs {
                let entry = state
                    .blobs
                    .get(&blob.resource)
                    .ok_or(invalid("blob is absent from directory"))?;
                self.sinks
                    .validate_checkpoint_blob(&entry.backing, blob.size)
                    .map_err(RecipeError::Replay)?;
                let kind = match (&entry.backing, &entry.memory) {
                    (ExportedMemory::Pages(_), None) => BlobKind::Shm,
                    (ExportedMemory::Pages(_), Some(_)) => BlobKind::Memory,
                    (ExportedMemory::Handle(_), Some(_)) => BlobKind::Handle,
                    _ => return Err(invalid("directory blob kind differs")),
                };
                if entry.ctx_id != blob.owner
                    || kind != blob.kind
                    || entry
                        .memory
                        .as_ref()
                        .is_some_and(|memory| memory.size != blob.size)
                {
                    return Err(invalid("blob owner, kind or memory size differs"));
                }
                if let Some(page) = entry.pages() {
                    if blob.size > page.mapped_len()
                        || (kind == BlobKind::Shm && blob.size != page.resource_len())
                    {
                        return Err(invalid("blob page geometry differs"));
                    }
                }
                if let Some((offset, len)) = blob.publication {
                    let window = self
                        .window
                        .as_ref()
                        .filter(|window| window.host_mapped())
                        .ok_or(invalid("blob publication requires a host-mapped aperture"))?;
                    if offset.checked_add(len).is_none_or(|end| end > window.len()) {
                        return Err(invalid("publication exceeds the new aperture"));
                    }
                }
                if let Some((spec, image)) = &blob.scanout {
                    if let Some(canonical) = image {
                        let matches = entry.memory.as_ref().is_some_and(|memory| {
                            memory.images.iter().any(|image| {
                                image.is_alive()
                                    && image.canonical == *canonical
                                    && scanout_mismatch(image, spec).is_none()
                            })
                        });
                        if !matches {
                            return Err(invalid(
                                "scanout has no matching reconstructed canonical image",
                            ));
                        }
                    } else {
                        let end = u64::from(spec.stride)
                            .checked_mul(u64::from(spec.height - 1))
                            .and_then(|at| at.checked_add(u64::from(spec.offset)))
                            .and_then(|at| at.checked_add(u64::from(spec.width) * 4));
                        if end.is_none_or(|end| {
                            entry.pages().is_none_or(|page| end > page.mapped_len())
                        }) {
                            return Err(invalid("saved page scanout exceeds its allocation"));
                        }
                    }
                }
                restored.insert(
                    blob.resource,
                    RingBlob {
                        publication: None,
                        backing: entry.backing.clone(),
                        ctx_id: blob.owner,
                        blob_id: blob.blob_id,
                        size: blob.size,
                        kind,
                        scanout: blob.scanout.as_ref().map(|(spec, image)| AcceptedScanout {
                            spec: *spec,
                            image: image.clone(),
                            shared: None,
                        }),
                    },
                );
            }
            Ok(())
        })?;
        let mut prepared = Vec::new();
        reserve(&mut prepared, saved.blobs.len())?;
        let applied = (|| -> Result<(), RecipeError> {
            for blob in &saved.blobs {
                let restored = restored
                    .get_mut(&blob.resource)
                    .ok_or(invalid("validated blob disappeared"))?;
                if let Some((offset, len)) = blob.publication {
                    let page = restored
                        .pages()
                        .ok_or(invalid("mapped blob has no pages"))?;
                    let window = self
                        .window
                        .as_ref()
                        .ok_or(invalid("mapped blob has no aperture"))?;
                    restored.publication = Some(
                        page.publish_len(window.clone(), offset, len)
                            .map_err(|error| RecipeError::Replay(error.to_string()))?,
                    );
                }
                if let (ExportedMemory::Handle(handle), Some(scanout)) =
                    (&restored.backing, &restored.scanout)
                {
                    let image = scanout
                        .image
                        .as_ref()
                        .ok_or(invalid("handle scanout has no canonical image"))?;
                    prepared.push(blob.resource);
                    self.sinks
                        .prepare_scanout(&ScanoutTarget {
                            resource_id: blob.resource,
                            handle: handle.clone(),
                            image: image.clone(),
                            spec: scanout.spec,
                        })
                        .map_err(RecipeError::Replay)?;
                }
            }
            Ok(())
        })();
        if let Err(error) = applied {
            for resource in prepared {
                self.sinks.forget_scanout(resource);
            }
            return Err(error);
        }
        self.blobs = restored;
        self.directory = directory;
        self.shm_blobs = accounting.host_count;
        self.blob_bytes = accounting.host_bytes;
        self.host_blob_use = accounting.host_by_context;
        self.memory_blobs = accounting.memory_count;
        self.memory_blob_use = accounting.memory_by_context;
        Ok(())
    }

    pub fn blob_directory(&self) -> &BlobDirectory {
        &self.directory
    }
}

#[cfg(test)]
mod tests;
