//! Stream all currently supported renderer payloads in one forward-only GPU
//! section. The outer VM container supplies its digest and a verified reader.
//! Front-end virtio resource/retirement state and VM lifecycle remain separate.
use super::checkpoint_objects::RendererObjectCheckpoint;
use super::*;
use crate::venus::executor::checkpoint::RecipeError;
use crate::venus::executor::checkpoint_pages::HostPageTable;
use crate::venus::executor::checkpoint_shared::SharedPayloads;
use crate::venus::executor::{ExecutorFactory, HostVulkan};
use crate::venus::shmem::PageBudget;
use std::collections::HashSet;
use std::io::{Read, Seek, Write};
use std::time::Instant;

mod format;
use format::{Kind, Part, PartReader};

fn invalid(why: &'static str) -> RecipeError {
    RecipeError::Invalid(why)
}
fn remaining(deadline: Instant) -> Result<Duration, RecipeError> {
    deadline
        .checked_duration_since(Instant::now())
        .filter(|duration| !duration.is_zero())
        .ok_or(invalid("Venus archive deadline expired"))
}

impl<H: HostVulkan> VenusRenderer<ExecutorFactory<H>> {
    pub(crate) fn checkpoint_request(
        &mut self,
        request: crate::renderer::checkpoint::CheckpointRequest<'_>,
    ) -> Result<(), CommandError> {
        use crate::renderer::checkpoint::CheckpointRequest;
        let result = match request {
            CheckpointRequest::Enable { recipe_bytes } => {
                if recipe_bytes == 0 || recipe_bytes > 64 << 20 {
                    Err(invalid(
                        "recipe retention budget must be between 1 byte and 64 MiB",
                    ))
                } else {
                    self.sinks
                        .enable_checkpoint_recipes(&PageBudget::new(recipe_bytes))
                }
            }
            CheckpointRequest::Save {
                mut out,
                contexts,
                blobs,
                timeout,
            } => self.save_checkpoint_archive_with_frontend(&mut out, timeout, contexts, blobs),
            CheckpointRequest::Load {
                mut reader,
                contexts,
                blobs,
                timeout,
            } => match self.sinks.checkpoint_storage() {
                Some(recipes) => self.load_checkpoint_archive_with_frontend(
                    &mut reader,
                    &recipes,
                    timeout,
                    contexts,
                    blobs,
                ),
                None => Err(invalid("recipe retention was not enabled before restore")),
            },
        };
        result.map_err(|error| CommandError::Renderer(error.to_string()))
    }

    /// One GPU section, without a whole-VRAM Vec or a second sizing readback.
    /// All guest/presenter writers stay stopped until this call completes.
    pub fn save_checkpoint_archive(
        &mut self,
        out: &mut impl Write,
        timeout: Duration,
    ) -> Result<(), RecipeError> {
        self.save_archive(out, timeout, None)
    }

    /// Device-aware capture rejects a disagreement with the virtio tables
    /// before writing the first archive byte.
    pub fn save_checkpoint_archive_with_frontend(
        &mut self,
        out: &mut impl Write,
        timeout: Duration,
        contexts: &crate::renderer::checkpoint::ContextCheckpoint,
        blobs: &crate::blob::BlobTable,
    ) -> Result<(), RecipeError> {
        self.save_archive(out, timeout, Some((contexts, blobs)))
    }

    fn save_archive(
        &mut self,
        out: &mut impl Write,
        timeout: Duration,
        frontend: Option<(
            &crate::renderer::checkpoint::ContextCheckpoint,
            &crate::blob::BlobTable,
        )>,
    ) -> Result<(), RecipeError> {
        let deadline = Instant::now()
            .checked_add(timeout.min(Duration::from_secs(120)))
            .ok_or(invalid("invalid Venus archive deadline"))?;
        let graph = self.checkpoint_object_graph(remaining(deadline)?)?;
        if let Some((contexts, blobs)) = frontend {
            graph.metadata.validate_frontend(contexts, blobs)?;
        }
        let metadata = graph.metadata.encode()?;
        let mut parts = Vec::new();
        parts
            .try_reserve_exact(
                2 + graph.shared_images.len() + 2 * graph.metadata.context_ids().count(),
            )
            .map_err(|_| RecipeError::Budget)?;
        parts.push(Part {
            kind: Kind::Metadata,
            id: 0,
            len: metadata.len() as u64,
        });
        parts.push(Part {
            kind: Kind::Pages,
            id: 0,
            len: graph.pages.encoded_len()?,
        });
        for (index, (image, release)) in graph.shared_images.iter().enumerate() {
            let len =
                crate::venus::executor::scanout::checkpoint::encoded_len(&image.image, *release)
                    .map_err(|error| RecipeError::Replay(error.to_string()))?;
            parts.push(Part {
                kind: Kind::SharedImage,
                id: index as u32 + 1,
                len,
            });
        }
        for id in graph.metadata.context_ids() {
            let (buffers, images) = self.sinks.checkpoint_context_payload_lengths(id)?;
            parts.push(Part {
                kind: Kind::Buffers,
                id,
                len: buffers,
            });
            parts.push(Part {
                kind: Kind::Images,
                id,
                len: images,
            });
        }
        format::write_header(out, &parts)?;
        for part in parts {
            remaining(deadline)?;
            format::write_part(out, part, |output| match part.kind {
                Kind::Metadata => {
                    output.write_all(&metadata)?;
                    Ok(())
                }
                Kind::Pages => graph.pages.save(output),
                Kind::SharedImage => {
                    let (target, release) = graph
                        .shared_images
                        .get(part.id as usize - 1)
                        .ok_or(invalid("shared archive payload is absent"))?;
                    self.sinks
                        .save_checkpoint_shared_image(target, *release, output)
                }
                Kind::Buffers => {
                    self.sinks
                        .save_checkpoint_buffers(part.id, output, remaining(deadline)?)
                }
                Kind::Images => {
                    self.sinks
                        .save_checkpoint_images(part.id, output, remaining(deadline)?)
                }
            })?;
        }
        Ok(())
    }

    /// Restore from a section-bounded Read+Seek view whose bytes the VM
    /// container verifies. No guest work may start until this and the front-end
    /// transport/resource restoration have both succeeded.
    pub fn load_checkpoint_archive(
        &mut self,
        reader: &mut (impl Read + Seek),
        recipes: &Arc<PageBudget>,
        timeout: Duration,
    ) -> Result<(), RecipeError> {
        self.load_archive(reader, recipes, timeout, None)
    }

    /// Validate the saved virtio namespace before native replay, allocating
    /// payloads, publishing mappings or starting paused workers.
    pub fn load_checkpoint_archive_with_frontend(
        &mut self,
        reader: &mut (impl Read + Seek),
        recipes: &Arc<PageBudget>,
        timeout: Duration,
        contexts: &crate::renderer::checkpoint::ContextCheckpoint,
        blobs: &crate::blob::BlobTable,
    ) -> Result<(), RecipeError> {
        self.load_archive(reader, recipes, timeout, Some((contexts, blobs)))
    }

    fn load_archive(
        &mut self,
        reader: &mut (impl Read + Seek),
        recipes: &Arc<PageBudget>,
        timeout: Duration,
        frontend: Option<(
            &crate::renderer::checkpoint::ContextCheckpoint,
            &crate::blob::BlobTable,
        )>,
    ) -> Result<(), RecipeError> {
        if !self.quiesce.is_paused()
            || !self.contexts.is_empty()
            || self.live.count() != 0
            || self.sinks.host_objects() != 0
            || !self.blobs.is_empty()
        {
            return Err(invalid(
                "Venus archive restore requires a cold paused renderer",
            ));
        }
        let deadline = Instant::now()
            .checked_add(timeout.min(Duration::from_secs(120)))
            .ok_or(invalid("invalid Venus archive restore deadline"))?;
        let restore = (|| {
            let entries = format::read_index(reader)?;
            let bytes = format::read_bytes(
                reader,
                format::entry(&entries, Kind::Metadata, 0)?,
                576 << 20,
            )?;
            let saved = RendererObjectCheckpoint::decode(&bytes)?;
            if let Some((contexts, blobs)) = frontend {
                saved.validate_frontend(contexts, blobs)?;
            }
            self.sinks.prepare_checkpoint_retention(recipes)?;
            let budgets = saved.resolve_budgets(&self.sinks)?;
            let owners = saved.payload_owners(&budgets)?;
            let mut expected = HashSet::new();
            expected
                .try_reserve(2 + owners.len() + 2 * saved.context_ids().count())
                .map_err(|_| RecipeError::Budget)?;
            expected.insert((Kind::Metadata, 0));
            expected.insert((Kind::Pages, 0));
            for id in 1..=owners.len() as u32 {
                expected.insert((Kind::SharedImage, id));
            }
            for id in saved.context_ids() {
                expected.insert((Kind::Buffers, id));
                expected.insert((Kind::Images, id));
            }
            if expected.len() != entries.len()
                || entries
                    .iter()
                    .any(|entry| !expected.contains(&(entry.part.kind, entry.part.id)))
            {
                return Err(invalid("Venus archive and object-graph inventories differ"));
            }
            let pages = HostPageTable::restore_owned_reader(
                &mut PartReader::new(reader, format::entry(&entries, Kind::Pages, 0)?),
                &saved.page_owners(&budgets)?,
                &budgets,
            )?;
            let mut handles = Vec::new();
            handles
                .try_reserve_exact(owners.len())
                .map_err(|_| RecipeError::Budget)?;
            for id in 1..=owners.len() as u32 {
                remaining(deadline)?;
                let bytes = format::read_bytes(
                    reader,
                    format::entry(&entries, Kind::SharedImage, id)?,
                    100 + 4 * crate::MAX_RESOURCE_PIXELS,
                )?;
                let image = self.sinks.restore_checkpoint_shared_image(
                    id,
                    &bytes,
                    owners.owner(id)?,
                    &budgets,
                )?;
                handles.push(image.target().handle.clone());
            }
            let payloads = SharedPayloads::from_handles(handles)?;
            self.restore_object_graph(
                &saved,
                &budgets,
                &pages,
                &payloads,
                recipes,
                |id, context| {
                    context.restore_device_buffers_reader(
                        &mut PartReader::new(reader, format::entry(&entries, Kind::Buffers, id)?),
                        remaining(deadline)?,
                    )?;
                    context.restore_device_images_reader(
                        &mut PartReader::new(reader, format::entry(&entries, Kind::Images, id)?),
                        remaining(deadline)?,
                    )
                },
            )?;
            remaining(deadline)?;
            Ok(())
        })();
        if restore.is_err() {
            self.reset();
        }
        restore
    }
}

#[cfg(test)]
mod tests;
