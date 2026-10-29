//! Blob identities, import permissions and canonical-image associations. The
//! enclosing renderer checkpoint owns pages/payload data and context/ring state.
//! No native handle or pointer enters this versioned directory manifest.

use std::collections::HashSet;

use super::*;
use crate::blob::{MAX_BLOB_BYTES, MAX_BLOB_RESOURCES, MAX_TOTAL_BLOB_BYTES};
use crate::venus::executor::checkpoint::RecipeError;
use crate::venus::executor::checkpoint_pages::HostPageTable;
use crate::venus::executor::checkpoint_shared::SharedPayloads;
use crate::venus::wire::{Decoder, Encoder};

const VERSION: u32 = 1;
const MAX_BYTES: usize = 32 << 20;
const MAX_PENDING: usize = MAX_SCANOUT_IMAGES * 8;
const MAX_VIEW_FORMATS: usize = 64;

/// A live image association. Restore resolves it to an image in the freshly
/// reconstructed context and checks its recipe, offset and liveness token.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SavedScanoutImage {
    pub ctx_id: u32,
    pub image: u64,
    pub offset: u64,
    pub canonical: CanonicalImage,
}

#[derive(Debug)]
struct Entry {
    resource: u32,
    owner: u32,
    generation: u64,
    handle: bool,
    root: u32,
    indexed: bool,
    memory: Option<Memory>,
}

#[derive(Debug)]
struct Memory {
    size: u64,
    attached: Vec<u32>,
    images: Vec<SavedScanoutImage>,
    release: Option<ScanoutRelease>,
}

/// Decode and validate this whole record before resolving native image tokens.
/// The directory's release is historical metadata, not proof of executed GPU
/// ownership; payload capture still needs the executed-state/writer-drain proof.
#[derive(Debug)]
pub struct DirectoryCheckpoint {
    next_generation: u64,
    entries: Vec<Entry>,
    pending: Vec<(u32, SavedScanoutImage)>,
}

fn invalid(reason: &'static str) -> RecipeError {
    RecipeError::Invalid(reason)
}

fn reserve<T>(vec: &mut Vec<T>, count: usize) -> Result<(), RecipeError> {
    vec.try_reserve(count).map_err(|_| RecipeError::Budget)
}

fn count(decoder: &mut Decoder<'_>, limit: usize) -> Result<usize, RecipeError> {
    let count = decoder.u32()? as usize;
    if count > limit {
        return Err(RecipeError::Budget);
    }
    Ok(count)
}

fn boolean(decoder: &mut Decoder<'_>) -> Result<bool, RecipeError> {
    match decoder.u32()? {
        0 => Ok(false),
        1 => Ok(true),
        _ => Err(invalid("invalid directory boolean")),
    }
}

impl SavedScanoutImage {
    fn capture(image: &ScanoutImage) -> Self {
        Self {
            ctx_id: image.ctx_id,
            image: image.image,
            offset: image.offset,
            canonical: image.canonical.clone(),
        }
    }

    fn validate(&self) -> Result<(), RecipeError> {
        let image = &self.canonical;
        if self.ctx_id == 0
            || self.image == 0
            || image.width == 0
            || image.height == 0
            || u64::from(image.width) * u64::from(image.height) > crate::MAX_RESOURCE_PIXELS
            || image.view_formats.len() > MAX_VIEW_FORMATS
        {
            return Err(invalid("invalid directory image geometry or identity"));
        }
        Ok(())
    }

    fn encode(&self, encoder: &mut Encoder) -> Result<(), RecipeError> {
        encoder.u32(self.ctx_id)?;
        encoder.u64(self.image)?;
        encoder.u64(self.offset)?;
        for word in [
            self.canonical.format as u32,
            self.canonical.flags,
            self.canonical.usage,
            self.canonical.width,
            self.canonical.height,
        ] {
            encoder.u32(word)?;
        }
        encoder.u32(self.canonical.view_formats.len() as u32)?;
        for format in &self.canonical.view_formats {
            encoder.i32(*format)?;
        }
        Ok(())
    }

    fn decode(decoder: &mut Decoder<'_>) -> Result<Self, RecipeError> {
        let ctx_id = decoder.u32()?;
        let image = decoder.u64()?;
        let offset = decoder.u64()?;
        let format = decoder.i32()?;
        let flags = decoder.u32()?;
        let usage = decoder.u32()?;
        let width = decoder.u32()?;
        let height = decoder.u32()?;
        let n = count(decoder, MAX_VIEW_FORMATS)?;
        let mut view_formats = Vec::new();
        reserve(&mut view_formats, n)?;
        for _ in 0..n {
            view_formats.push(decoder.i32()?);
        }
        let saved = Self {
            ctx_id,
            image,
            offset,
            canonical: CanonicalImage {
                format,
                flags,
                usage,
                width,
                height,
                view_formats,
            },
        };
        saved.validate()?;
        Ok(saved)
    }
}

impl DirectoryCheckpoint {
    pub(crate) fn page_bindings(&self) -> impl Iterator<Item = (u32, u32, bool)> + '_ {
        self.entries
            .iter()
            .filter(|entry| !entry.handle)
            .map(|entry| (entry.resource, entry.root, entry.memory.is_some()))
    }

    fn validate(&self) -> Result<(), RecipeError> {
        if self.entries.len() > MAX_BLOB_RESOURCES || self.pending.len() > MAX_PENDING {
            return Err(RecipeError::Budget);
        }
        let mut resources = HashSet::new();
        let mut generations = HashSet::new();
        let mut indexed = HashSet::new();
        for entry in &self.entries {
            if entry.resource == 0
                || entry.root == 0
                || entry.generation == 0
                || entry.generation > self.next_generation
                || !resources.insert(entry.resource)
                || !generations.insert(entry.generation)
                || (entry.handle && entry.memory.is_none())
                || (entry.indexed && (!entry.handle || !indexed.insert(entry.root)))
            {
                return Err(invalid("invalid or duplicate directory identity"));
            }
            if let Some(memory) = &entry.memory {
                if memory.size == 0
                    || memory.size > MAX_BLOB_BYTES
                    || memory.attached.len() > MAX_VENUS_CONTEXTS
                    || memory.images.len() > MAX_SCANOUT_IMAGES
                    || (!entry.handle && (!memory.images.is_empty() || memory.release.is_some()))
                {
                    return Err(invalid("invalid directory memory metadata"));
                }
                let mut attached = HashSet::new();
                for ctx in &memory.attached {
                    if *ctx == 0 || !attached.insert(*ctx) {
                        return Err(invalid("invalid or duplicate directory attachment"));
                    }
                }
                let mut images = HashSet::new();
                for image in &memory.images {
                    image.validate()?;
                    if image.offset >= memory.size || !images.insert((image.ctx_id, image.image)) {
                        return Err(invalid("invalid or duplicate directory image"));
                    }
                }
                if let Some(release) = memory.release {
                    if !matches!(release.family, 0xffff_fffe | 0xffff_fffd) {
                        return Err(invalid("invalid directory external family"));
                    }
                }
            }
        }
        for (root, image) in &self.pending {
            if *root == 0 {
                return Err(invalid("zero pending payload id"));
            }
            image.validate()?;
        }
        Ok(())
    }

    pub fn encode(&self) -> Result<Vec<u8>, RecipeError> {
        self.validate()?;
        let mut encoder = Encoder::with_limit(MAX_BYTES);
        encoder.u32(VERSION)?;
        encoder.u64(self.next_generation)?;
        encoder.u32(self.entries.len() as u32)?;
        for entry in &self.entries {
            encoder.u32(entry.resource)?;
            encoder.u32(entry.owner)?;
            encoder.u64(entry.generation)?;
            encoder.u32(u32::from(entry.handle))?;
            encoder.u32(entry.root)?;
            encoder.u32(u32::from(entry.indexed))?;
            encoder.u32(u32::from(entry.memory.is_some()))?;
            if let Some(memory) = &entry.memory {
                encoder.u64(memory.size)?;
                encoder.u32(memory.attached.len() as u32)?;
                for ctx in &memory.attached {
                    encoder.u32(*ctx)?;
                }
                encoder.u32(memory.images.len() as u32)?;
                for image in &memory.images {
                    image.encode(&mut encoder)?;
                }
                encoder.u32(u32::from(memory.release.is_some()))?;
                if let Some(release) = memory.release {
                    encoder.i32(release.layout)?;
                    encoder.u32(release.family)?;
                }
            }
        }
        encoder.u32(self.pending.len() as u32)?;
        for (root, image) in &self.pending {
            encoder.u32(*root)?;
            image.encode(&mut encoder)?;
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
        let next_generation = decoder.u64()?;
        let n = count(&mut decoder, MAX_BLOB_RESOURCES)?;
        let mut entries = Vec::new();
        reserve(&mut entries, n)?;
        for _ in 0..n {
            let resource = decoder.u32()?;
            let owner = decoder.u32()?;
            let generation = decoder.u64()?;
            let handle = boolean(&mut decoder)?;
            let root = decoder.u32()?;
            let indexed = boolean(&mut decoder)?;
            let memory = if boolean(&mut decoder)? {
                let size = decoder.u64()?;
                let n = count(&mut decoder, MAX_VENUS_CONTEXTS)?;
                let mut attached = Vec::new();
                reserve(&mut attached, n)?;
                for _ in 0..n {
                    attached.push(decoder.u32()?);
                }
                let n = count(&mut decoder, MAX_SCANOUT_IMAGES)?;
                let mut images = Vec::new();
                reserve(&mut images, n)?;
                for _ in 0..n {
                    images.push(SavedScanoutImage::decode(&mut decoder)?);
                }
                let release = if boolean(&mut decoder)? {
                    Some(ScanoutRelease {
                        layout: decoder.i32()?,
                        family: decoder.u32()?,
                    })
                } else {
                    None
                };
                Some(Memory {
                    size,
                    attached,
                    images,
                    release,
                })
            } else {
                None
            };
            entries.push(Entry {
                resource,
                owner,
                generation,
                handle,
                root,
                indexed,
                memory,
            });
        }
        let n = count(&mut decoder, MAX_PENDING)?;
        let mut pending = Vec::new();
        reserve(&mut pending, n)?;
        for _ in 0..n {
            pending.push((decoder.u32()?, SavedScanoutImage::decode(&mut decoder)?));
        }
        if decoder.position() != bytes.len() {
            return Err(invalid("trailing directory data"));
        }
        let saved = Self {
            next_generation,
            entries,
            pending,
        };
        saved.validate()?;
        Ok(saved)
    }
}

impl BlobDirectory {
    /// Park all guest/ring/queue writers first. Register these roots in the
    /// same tables used by every context's allocation links, not separate ones.
    pub fn checkpoint(
        &self,
        pages: &mut HostPageTable,
        payloads: &mut SharedPayloads,
    ) -> Result<DirectoryCheckpoint, RecipeError> {
        self.with(|state| {
            if state.blobs.len() > MAX_BLOB_RESOURCES || state.pending.len() > MAX_PENDING {
                return Err(RecipeError::Budget);
            }
            let mut entries = Vec::new();
            reserve(&mut entries, state.blobs.len())?;
            for (resource, entry) in &state.blobs {
                let (handle, root, indexed) = match &entry.backing {
                    ExportedMemory::Pages(p) => (false, pages.register(p)?, false),
                    ExportedMemory::Handle(h) => (
                        true,
                        payloads.register(h)?,
                        state.by_handle.get(&h.key()) == Some(resource),
                    ),
                };
                let memory = entry.memory.as_ref().map(|memory| Memory {
                    size: memory.size,
                    attached: memory.attached.clone(),
                    images: memory
                        .images
                        .iter()
                        .filter(|image| image.is_alive())
                        .map(SavedScanoutImage::capture)
                        .collect(),
                    release: memory.release,
                });
                entries.push(Entry {
                    resource: *resource,
                    owner: entry.ctx_id,
                    generation: entry.generation,
                    handle,
                    root,
                    indexed,
                    memory,
                });
            }
            entries.sort_unstable_by_key(|entry| entry.resource);
            let mut pending = Vec::new();
            reserve(&mut pending, state.pending.len())?;
            for (shared, image) in &state.pending {
                if !image.is_alive() {
                    continue;
                }
                if let Some(handle) = shared.0.upgrade() {
                    pending.push((
                        payloads.register(&SharedHandle(handle))?,
                        SavedScanoutImage::capture(image),
                    ));
                }
            }
            let saved = DirectoryCheckpoint {
                next_generation: state.next_generation,
                entries,
                pending,
            };
            saved.validate()?;
            Ok(saved)
        })
    }

    /// Make a separate directory; failure cannot mutate the source or any sink
    /// already pointing at it. Recreate payloads and Vulkan contexts first. The
    /// resolver must verify the new image is bound to the supplied payload.
    pub fn from_checkpoint(
        saved: &DirectoryCheckpoint,
        pages: &HostPageTable,
        payloads: &SharedPayloads,
        mut image: impl FnMut(&SavedScanoutImage, &SharedHandle) -> Result<ScanoutImage, RecipeError>,
    ) -> Result<Self, RecipeError> {
        saved.validate()?;
        // Resolve every root before asking a native-context resolver for tokens.
        let mut total = 0_u64;
        for entry in &saved.entries {
            if entry.handle {
                payloads.handle(entry.root)?;
            } else {
                let page = pages.page(entry.root)?;
                if page.is_host_private()
                    || page.is_poisoned()
                    || entry
                        .memory
                        .as_ref()
                        .is_some_and(|memory| memory.size > page.mapped_len())
                {
                    return Err(invalid("directory page is private, poisoned or too short"));
                }
            }
            let size = if let Some(memory) = &entry.memory {
                memory.size
            } else {
                pages.page(entry.root)?.resource_len()
            };
            if size > MAX_BLOB_BYTES {
                return Err(invalid("directory blob exceeds the resource size cap"));
            }
            total = total
                .checked_add(size)
                .filter(|sum| *sum <= MAX_TOTAL_BLOB_BYTES)
                .ok_or(RecipeError::Budget)?;
        }
        for (root, _) in &saved.pending {
            payloads.handle(*root)?;
        }
        let mut resolve = |saved: &SavedScanoutImage, handle: &SharedHandle| {
            let restored = image(saved, handle)?;
            if !restored.is_alive() || SavedScanoutImage::capture(&restored) != *saved {
                return Err(invalid("reconstructed directory image differs"));
            }
            Ok(restored)
        };
        let mut state = DirectoryState {
            next_generation: saved.next_generation,
            ..Default::default()
        };
        state
            .blobs
            .try_reserve(saved.entries.len())
            .map_err(|_| RecipeError::Budget)?;
        state
            .by_handle
            .try_reserve(saved.entries.len())
            .map_err(|_| RecipeError::Budget)?;
        for entry in &saved.entries {
            let backing = if entry.handle {
                ExportedMemory::Handle(payloads.handle(entry.root)?.clone())
            } else {
                ExportedMemory::Pages(pages.page(entry.root)?.clone())
            };
            let memory = if let Some(memory) = &entry.memory {
                let mut images = Vec::new();
                reserve(&mut images, memory.images.len())?;
                if let ExportedMemory::Handle(handle) = &backing {
                    for saved in &memory.images {
                        images.push(resolve(saved, handle)?);
                    }
                }
                Some(MemoryEntry {
                    size: memory.size,
                    attached: memory.attached.clone(),
                    images,
                    release: memory.release,
                })
            } else {
                None
            };
            if entry.indexed {
                if let ExportedMemory::Handle(handle) = &backing {
                    state.by_handle.insert(handle.key(), entry.resource);
                }
            }
            state.blobs.insert(
                entry.resource,
                DirectoryEntry {
                    ctx_id: entry.owner,
                    backing,
                    generation: entry.generation,
                    memory,
                },
            );
        }
        reserve(&mut state.pending, saved.pending.len())?;
        for (root, saved) in &saved.pending {
            let handle = payloads.handle(*root)?;
            state
                .pending
                .push((SharedRef::of(handle), resolve(saved, handle)?));
        }
        Ok(Self(Arc::new(Mutex::new(state))))
    }
}

impl<F> VenusRenderer<F> {
    /// Directory half of the renderer manifest. The caller must additionally
    /// capture blob layouts/mappings, contexts/rings, budgets and factory state.
    pub fn checkpoint_directory(
        &self,
        pages: &mut HostPageTable,
        payloads: &mut SharedPayloads,
    ) -> Result<DirectoryCheckpoint, RecipeError> {
        self.directory.checkpoint(pages, payloads)
    }
}

#[cfg(test)]
mod tests;
