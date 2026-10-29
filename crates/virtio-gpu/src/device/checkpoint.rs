//! Device-owned accelerated front end followed by the native renderer archive.
//! Transport registers/queues and 2D resources stay in the ordinary device
//! section. Both sections must be applied before any guest worker resumes.
use super::*;
use crate::blob::checkpoint::BlobCheckpoint;
use crate::renderer::checkpoint::{CheckpointReader, ContextCheckpoint};
use std::io::Write;
use vm_memory::GuestMemory;

const MAGIC: &[u8; 8] = b"ENTGLGPU";
const HEADER_BYTES: usize = 64;
const MAX_BLOB_METADATA: usize = 32 << 20;
const MAX_CONTEXT_METADATA: usize = 8 + crate::renderer::MAX_CONTEXTS * 8;

fn invalid(why: impl Into<String>) -> CommandError {
    CommandError::Renderer(why.into())
}
fn io(error: std::io::Error) -> CommandError {
    invalid(format!("accelerated GPU checkpoint I/O failed: {error}"))
}
fn word(bytes: &[u8; HEADER_BYTES], at: usize) -> u32 {
    // Every caller supplies a fixed field offset inside this fixed header.
    u32::from_le_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]])
}
fn read_metadata(
    reader: &mut dyn CheckpointReader,
    len: usize,
    limit: usize,
) -> Result<Vec<u8>, CommandError> {
    if len > limit {
        return Err(invalid("accelerated GPU metadata exceeds its budget"));
    }
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(len)
        .map_err(|_| CommandError::OutOfMemory)?;
    bytes.resize(len, 0);
    reader.read_exact(&mut bytes).map_err(io)?;
    Ok(bytes)
}

impl<S: ScanoutSink> GpuDevice<S> {
    /// Poll only already submitted work under the machine's closed gate. The
    /// runtime watchdog must not invent fence completion for a checkpoint.
    /// Completed responses/IRQs are returned before CPU/APIC capture; work that
    /// still needs a future guest signal remains pending and refuses capture.
    pub fn prepare_accelerated_checkpoint(&mut self) -> Result<(), DeviceError> {
        if self.renderer_lost
            || self.restored_3d_lost
            || self.three_d.as_ref().is_none_or(|gpu| !gpu.is_alive())
        {
            return Err(DeviceError::Backend(
                "GPU checkpoint has lost renderer state".into(),
            ));
        }
        if self.pending_fences.is_empty() {
            if let Some(gpu) = &mut self.three_d {
                gpu.poll_fence_timelines(0);
            }
            return Ok(());
        }
        let mem = self.mem.clone().ok_or(DeviceError::NotActivated)?;
        let interrupt = self.interrupt.clone().ok_or(DeviceError::NotActivated)?;
        let mut queue = self.control.take().ok_or(DeviceError::NotActivated)?;
        let deadline = Instant::now() + Duration::from_secs(2);
        let result = (|| loop {
            let completed = self.complete_fences_with_watchdog(&mut queue, &mem, false)?;
            if completed != 0
                && queue
                    .needs_notification(mem.as_ref())
                    .map_err(|error| DeviceError::Queue(error.to_string()))?
            {
                interrupt.signal_used_queue(CONTROL_QUEUE)?;
            }
            if self.pending_fences.is_empty() {
                return Ok(());
            }
            if Instant::now() >= deadline {
                return Err(DeviceError::Backend(
                    "GPU checkpoint still has pending responses; no fence was completed by timeout"
                        .into(),
                ));
            }
            std::thread::sleep(Duration::from_millis(1));
        })();
        self.control = Some(queue);
        result
    }

    /// Opt in before any guest context exists. Production snapshot refusal is
    /// unchanged until machine/transport integration applies this component.
    pub fn enable_accelerated_checkpoints(
        &mut self,
        recipe_bytes: u64,
    ) -> Result<(), CommandError> {
        self.three_d
            .as_mut()
            .ok_or_else(|| invalid("no accelerated renderer"))?
            .enable_checkpoints(recipe_bytes)?;
        self.accelerated_checkpoints = true;
        Ok(())
    }

    /// Stream while all guest, renderer and presenter writers are quiesced.
    /// Pending virtqueue responses must first be settled by prepare_save.
    pub fn save_accelerated_checkpoint(
        &mut self,
        out: &mut dyn Write,
        timeout: Duration,
    ) -> Result<(), CommandError> {
        if !self.pending_fences.is_empty() || self.renderer_lost || self.restored_3d_lost {
            return Err(invalid(
                "GPU checkpoint has pending responses or lost renderer state",
            ));
        }
        let gpu = self
            .three_d
            .as_ref()
            .ok_or_else(|| invalid("no accelerated renderer"))?;
        let contexts = gpu
            .checkpoint_contexts()
            .map_err(|error| invalid(error.to_string()))?
            .encode()
            .map_err(|error| invalid(error.to_string()))?;
        let blobs = self
            .blobs
            .save_checkpoint()
            .map_err(|error| invalid(error.to_string()))?;
        let mut header = [0; HEADER_BYTES];
        header[..8].copy_from_slice(MAGIC);
        let scanout = self.scanout;
        let (kind, stride, offset) = match scanout.map(|binding| binding.source) {
            None => (0, 0, 0),
            Some(ScanoutSource::TwoD) => (1, 0, 0),
            Some(ScanoutSource::Blob { stride, offset }) => (2, stride, offset),
            Some(ScanoutSource::RendererBlob { spec }) => (3, spec.stride, spec.offset),
            Some(ScanoutSource::ThreeD) => {
                return Err(invalid("classic 3D scanout is not checkpointed"))
            }
        };
        let rect = scanout.map(|binding| binding.rect).unwrap_or(Rect {
            x: 0,
            y: 0,
            width: 0,
            height: 0,
        });
        let values = [
            1,
            self.events_read.load(std::sync::atomic::Ordering::Acquire),
            contexts.len() as u32,
            blobs.len() as u32,
            kind,
            scanout.map_or(0, |binding| binding.resource_id),
            rect.x,
            rect.y,
            rect.width,
            rect.height,
            stride,
            offset,
        ];
        for (chunk, value) in header[8..56].chunks_exact_mut(4).zip(values) {
            chunk.copy_from_slice(&value.to_le_bytes());
        }
        self.validate_checkpoint_scanout(&header, &self.blobs)?;
        out.write_all(&header).map_err(io)?;
        out.write_all(&contexts).map_err(io)?;
        out.write_all(&blobs).map_err(io)?;
        self.three_d
            .as_mut()
            .ok_or_else(|| invalid("no accelerated renderer"))?
            .save_checkpoint(out, &self.blobs, timeout)?;
        Ok(())
    }

    fn validate_checkpoint_scanout(
        &self,
        header: &[u8; HEADER_BYTES],
        blobs: &BlobTable,
    ) -> Result<Option<ScanoutBinding>, CommandError> {
        let kind = word(header, 24);
        if kind == 0 {
            if header[28..56].iter().any(|byte| *byte != 0) {
                return Err(invalid("disabled scanout has live fields"));
            }
            return Ok(None);
        }
        let id = word(header, 28);
        let rect = Rect {
            x: word(header, 32),
            y: word(header, 36),
            width: word(header, 40),
            height: word(header, 44),
        };
        if id == 0
            || rect.width == 0
            || rect.height == 0
            || rect.pixels() > crate::MAX_RESOURCE_PIXELS
        {
            return Err(invalid("invalid checkpoint scanout geometry"));
        }
        let stride = word(header, 48);
        let offset = word(header, 52);
        let source = match kind {
            1 => {
                let resource = self
                    .resources
                    .get(id)
                    .ok_or(CommandError::UnknownResource(id))?;
                if stride != 0
                    || offset != 0
                    || !rect.fits_within(resource.width(), resource.height())
                {
                    return Err(invalid("2D checkpoint scanout differs"));
                }
                ScanoutSource::TwoD
            }
            2 => {
                let blob = blobs.get(id).ok_or(CommandError::UnknownResource(id))?;
                let end_x = u64::from(rect.x) + u64::from(rect.width);
                let end_y = u64::from(rect.y) + u64::from(rect.height);
                let end = u64::from(stride)
                    .checked_mul(end_y)
                    .and_then(|bytes| bytes.checked_add(u64::from(offset)));
                if blob.is_host()
                    || u64::from(stride) < end_x * u64::from(crate::BYTES_PER_PIXEL)
                    || end.is_none_or(|end| end > blob.size())
                {
                    return Err(invalid("guest blob checkpoint scanout exceeds backing"));
                }
                ScanoutSource::Blob { stride, offset }
            }
            3 => {
                let blob = blobs.get(id).ok_or(CommandError::UnknownResource(id))?;
                let spec = blob
                    .scanout_spec()
                    .ok_or_else(|| invalid("renderer checkpoint scanout was never accepted"))?;
                if !blob.is_host()
                    || stride != spec.stride
                    || offset != spec.offset
                    || !rect.fits_within(spec.width, spec.height)
                {
                    return Err(invalid(
                        "renderer checkpoint scanout differs from accepted layout",
                    ));
                }
                ScanoutSource::RendererBlob { spec }
            }
            _ => return Err(invalid("unknown checkpoint scanout source")),
        };
        Ok(Some(ScanoutBinding {
            resource_id: id,
            rect,
            source,
        }))
    }

    /// Apply after ordinary transport/2D state and RAM, under the closed gate.
    /// Backend failures discard native reconstruction; front-end failures after
    /// upload also clear native objects/publications and leave the device lost.
    pub fn load_accelerated_checkpoint(
        &mut self,
        reader: &mut dyn CheckpointReader,
        timeout: Duration,
    ) -> Result<(), CommandError> {
        if !self.blobs.is_empty()
            || self.blobs.window().mapping_count() != 0
            || !self.pending_fences.is_empty()
            || self
                .three_d
                .as_ref()
                .is_none_or(|gpu| gpu.context_count() != 0)
        {
            return Err(invalid(
                "GPU checkpoint restore requires a cold accelerated front end",
            ));
        }
        let mut header = [0; HEADER_BYTES];
        reader.read_exact(&mut header).map_err(io)?;
        if &header[..8] != MAGIC
            || word(&header, 8) != 1
            || header[56..].iter().any(|byte| *byte != 0)
        {
            return Err(invalid("unknown accelerated GPU checkpoint header"));
        }
        let context_bytes = word(&header, 16) as usize;
        let blob_bytes = word(&header, 20) as usize;
        if context_bytes > MAX_CONTEXT_METADATA || blob_bytes > MAX_BLOB_METADATA {
            return Err(invalid("accelerated GPU metadata exceeds its budget"));
        }
        let start = reader.stream_position().map_err(io)?;
        let end = reader.seek(std::io::SeekFrom::End(0)).map_err(io)?;
        reader.seek(std::io::SeekFrom::Start(start)).map_err(io)?;
        if end
            .checked_sub(start)
            .is_none_or(|remaining| remaining < (context_bytes + blob_bytes + 16) as u64)
        {
            return Err(invalid(
                "accelerated GPU metadata exceeds the remaining section",
            ));
        }
        let contexts =
            ContextCheckpoint::decode(&read_metadata(reader, context_bytes, MAX_CONTEXT_METADATA)?)
                .map_err(|error| invalid(error.to_string()))?;
        let blobs = BlobCheckpoint::decode(
            &read_metadata(reader, blob_bytes, MAX_BLOB_METADATA)?,
            self.blob_support,
        )
        .map_err(|error| invalid(error.to_string()))?;
        let scanout = self.validate_checkpoint_scanout(&header, blobs.table())?;
        if self.mem.is_none() && (contexts.contexts().len() != 0 || !blobs.table().is_empty()) {
            return Err(invalid(
                "accelerated GPU checkpoint requires an activated device",
            ));
        }
        for blob in blobs.table().iter() {
            if self.resources.get(blob.id()).is_some() {
                return Err(invalid("2D and blob checkpoint resource ids collide"));
            }
            for entry in blob.backing() {
                if entry.length != 0
                    && self.mem.as_ref().is_none_or(|mem| {
                        !mem.check_range(GuestAddress(entry.addr), entry.length as usize)
                    })
                {
                    return Err(invalid(
                        "blob checkpoint backing is outside restored guest RAM",
                    ));
                }
            }
        }
        self.three_d
            .as_mut()
            .ok_or_else(|| invalid("no accelerated renderer"))?
            .load_checkpoint(reader, &contexts, blobs.table(), timeout)?;
        let adoption = (|| {
            blobs
                .restore(&mut self.blobs)
                .map_err(|error| invalid(error.to_string()))?;
            self.scanout = scanout;
            self.shared_active = false;
            self.shared_declined = false;
            self.shared_logged = false;
            self.renderer_blob_scanned_out = matches!(
                scanout.map(|binding| binding.source),
                Some(ScanoutSource::RendererBlob { .. })
            );
            if let Some(scanout) = scanout {
                self.display
                    .set_resolution(scanout.rect.width, scanout.rect.height)
                    .map_err(|error| CommandError::Display(error.to_string()))?;
                let mut pixels = Vec::new();
                match scanout.source {
                    ScanoutSource::TwoD => {
                        if !self.present_whole_scanout() {
                            return Err(invalid("could not present restored 2D scanout"));
                        }
                    }
                    ScanoutSource::Blob { stride, offset } => {
                        self.read_blob_rect(
                            scanout.resource_id,
                            scanout.rect,
                            stride,
                            offset,
                            &mut pixels,
                        )?;
                        self.display
                            .update_scanout(0, 0, scanout.rect.width, scanout.rect.height, &pixels)
                            .map_err(|error| CommandError::Display(error.to_string()))?;
                    }
                    ScanoutSource::RendererBlob { spec } => {
                        self.three_d
                            .as_mut()
                            .ok_or_else(|| invalid("no accelerated renderer"))?
                            .read_scanout_blob(
                                scanout.resource_id,
                                &spec,
                                scanout.rect,
                                &mut pixels,
                            )?;
                        self.display
                            .update_scanout(0, 0, scanout.rect.width, scanout.rect.height, &pixels)
                            .map_err(|error| CommandError::Display(error.to_string()))?;
                    }
                    ScanoutSource::ThreeD => {
                        return Err(invalid("classic 3D scanout is not checkpointed"))
                    }
                }
            }
            self.events_read
                .store(word(&header, 12), std::sync::atomic::Ordering::Release);
            self.restored_3d_lost = false;
            self.restored_3d_reported = false;
            self.renderer_lost = false;
            self.renderer_loss_reported = false;
            Ok(())
        })();
        if adoption.is_err() {
            if let Some(gpu) = &mut self.three_d {
                gpu.reset();
            }
            self.blobs.clear();
            self.scanout = None;
            self.restored_3d_lost = true;
        }
        adoption
    }
}

#[cfg(test)]
mod tests;
