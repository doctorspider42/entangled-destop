//! Virtio's context validation table, separate from native Vulkan handles.
//! Restoring this table must not issue CTX_CREATE a second time.
use super::*;

/// Object-safe seekable input. Callers pass a verified, section-bounded view,
/// never an unbounded VM file or a source-process memory address.
pub use virtio_core::StateReader as CheckpointReader;

/// Explicitly opt-in streaming seam; legacy renderers refuse it by default.
pub enum CheckpointRequest<'a> {
    Enable {
        recipe_bytes: u64,
    },
    Save {
        out: &'a mut dyn std::io::Write,
        contexts: &'a ContextCheckpoint,
        blobs: &'a crate::blob::BlobTable,
        timeout: std::time::Duration,
    },
    Load {
        reader: &'a mut dyn CheckpointReader,
        contexts: &'a ContextCheckpoint,
        blobs: &'a crate::blob::BlobTable,
        timeout: std::time::Duration,
    },
}

#[derive(Debug, thiserror::Error)]
pub enum ContextCheckpointError {
    #[error("invalid virtio-gpu context checkpoint: {0}")]
    Invalid(&'static str),
    #[error("virtio-gpu context checkpoint exceeds its metadata budget")]
    Budget,
}
use ContextCheckpointError::{Budget, Invalid};

#[derive(Debug)]
pub struct ContextCheckpoint {
    contexts: Vec<(u32, u32)>,
}

impl ContextCheckpoint {
    pub fn contexts(&self) -> impl ExactSizeIterator<Item = (u32, u32)> + '_ {
        self.contexts.iter().copied()
    }

    pub fn encode(&self) -> Result<Vec<u8>, ContextCheckpointError> {
        let mut bytes = Vec::new();
        bytes
            .try_reserve_exact(8 + self.contexts.len() * 8)
            .map_err(|_| Budget)?;
        bytes.extend_from_slice(&1_u32.to_le_bytes());
        bytes.extend_from_slice(&(self.contexts.len() as u32).to_le_bytes());
        for (id, capset) in self.contexts() {
            bytes.extend_from_slice(&id.to_le_bytes());
            bytes.extend_from_slice(&capset.to_le_bytes());
        }
        Ok(bytes)
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, ContextCheckpointError> {
        let word = |offset: usize| {
            bytes
                .get(offset..offset + 4)
                .and_then(|value| <[u8; 4]>::try_from(value).ok())
                .map(u32::from_le_bytes)
                .ok_or(Invalid("truncated context table"))
        };
        if word(0)? != 1 {
            return Err(Invalid("unsupported version"));
        }
        let count = word(4)? as usize;
        if count > MAX_CONTEXTS {
            return Err(Budget);
        }
        if bytes.len() != 8 + 8 * count {
            return Err(Invalid("context count does not match component length"));
        }
        let mut contexts = Vec::new();
        contexts.try_reserve_exact(count).map_err(|_| Budget)?;
        let mut last_id = 0;
        for offset in (8..bytes.len()).step_by(8) {
            let id = word(offset)?;
            let capset = word(offset + 4)?;
            if id <= last_id || capset != crate::CAPSET_VENUS {
                return Err(Invalid("context ids must be sorted Venus contexts"));
            }
            contexts.push((id, capset));
            last_id = id;
        }
        Ok(Self { contexts })
    }
}

impl Gpu3d {
    /// Enable bounded recipe capture before creating the first native context.
    pub fn enable_checkpoints(&mut self, recipe_bytes: u64) -> Result<(), CommandError> {
        self.renderer
            .checkpoint(CheckpointRequest::Enable { recipe_bytes })
    }

    /// Stream native contents after validating both front-end namespaces.
    pub fn save_checkpoint(
        &mut self,
        out: &mut dyn std::io::Write,
        blobs: &crate::blob::BlobTable,
        timeout: std::time::Duration,
    ) -> Result<ContextCheckpoint, CommandError> {
        let contexts = self
            .checkpoint_contexts()
            .map_err(|error| CommandError::Renderer(error.to_string()))?;
        self.renderer.checkpoint(CheckpointRequest::Save {
            out,
            contexts: &contexts,
            blobs,
            timeout,
        })?;
        Ok(contexts)
    }

    /// Restore native state before adopting virtio's context table. Any later
    /// front-end adoption failure destroys the entire cold native reconstruction.
    pub fn load_checkpoint(
        &mut self,
        reader: &mut dyn CheckpointReader,
        contexts: &ContextCheckpoint,
        blobs: &crate::blob::BlobTable,
        timeout: std::time::Duration,
    ) -> Result<(), CommandError> {
        if !self.contexts.is_empty() || !self.resources.is_empty() || self.total_elements != 0 {
            return Err(CommandError::Renderer(
                "checkpoint restore requires a cold GPU validation table".into(),
            ));
        }
        self.renderer.checkpoint(CheckpointRequest::Load {
            reader,
            contexts,
            blobs,
            timeout,
        })?;
        if let Err(error) = self.restore_checkpoint_contexts(contexts) {
            self.reset();
            return Err(CommandError::Renderer(error.to_string()));
        }
        Ok(())
    }

    /// Capture only the Venus context table. Classic 3D resources need their
    /// own contents and are explicitly refused rather than lost on restore.
    pub fn checkpoint_contexts(&self) -> Result<ContextCheckpoint, ContextCheckpointError> {
        if !self.resources.is_empty() || self.total_elements != 0 {
            return Err(Invalid("classic 3D resources are not checkpointed"));
        }
        if self.contexts.len() > MAX_CONTEXTS {
            return Err(Budget);
        }
        if self
            .contexts
            .values()
            .any(|capset| *capset != crate::CAPSET_VENUS)
        {
            return Err(Invalid("only Venus contexts can be checkpointed"));
        }
        let mut contexts = Vec::new();
        contexts
            .try_reserve_exact(self.contexts.len())
            .map_err(|_| Budget)?;
        contexts.extend(self.contexts.iter().map(|(id, capset)| (*id, *capset)));
        contexts.sort_unstable_by_key(|(id, _)| *id);
        Ok(ContextCheckpoint { contexts })
    }

    /// Commit after the matching renderer graph has been restored. The caller
    /// must validate exact context identities against that graph first. No
    /// renderer command is issued here: native contexts already exist.
    pub fn restore_checkpoint_contexts(
        &mut self,
        saved: &ContextCheckpoint,
    ) -> Result<(), ContextCheckpointError> {
        if !self.contexts.is_empty() || !self.resources.is_empty() || self.total_elements != 0 {
            return Err(Invalid("restore requires a cold validation table"));
        }
        if !saved.contexts.is_empty() && !self.serves_venus() {
            return Err(Invalid("renderer does not serve the saved capset"));
        }
        self.contexts
            .try_reserve(saved.contexts.len())
            .map_err(|_| Budget)?;
        self.contexts.extend(saved.contexts());
        Ok(())
    }
}

#[cfg(test)]
mod tests;
