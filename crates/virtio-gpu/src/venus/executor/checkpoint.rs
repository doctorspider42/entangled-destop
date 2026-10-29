//! Portable reconstruction recipes for a Venus context. These are guest-id
//! commands captured *before* translation, never serialised native handles.
//!
//! This component reconstructs the object namespace and command recordings.
//! Memory contents, external payload identities, ring state and synchronisation
//! checkpoints are separate requirements; a recipe journal alone must never
//! waive the renderer's snapshot refusal.

use std::collections::{BTreeMap, HashSet};
use std::io::Write;
use std::sync::Arc;

use thiserror::Error;

use crate::venus::protocol::{Command, ProtocolError};
use crate::venus::shmem::{Charge, PageBudget};
use crate::venus::wire::{Decoder, Encoder, WireError};

use super::{HostVulkan, VulkanContext};

const VERSION: u32 = 1;
const RECIPE_VERSION: u32 = 2;
/// Total retained recipe storage, including record headers and vector entries.
pub const MAX_RECIPE_BYTES: u64 = 64 << 20;
const MAX_RECORD_BYTES: usize = 8 << 20;
const RECORD_COST: u64 = 4 + std::mem::size_of::<Vec<u8>>() as u64;

/// These checkpoint codecs use the checked Venus byte decoder, whose blobs
/// carry four-byte padding. Streaming writers must emit that padding too,
/// including when a guest allocation/buffer has an odd declared length.
pub(super) fn write_padding(writer: &mut impl Write, length: u64) -> Result<(), RecipeError> {
    let pad = ((4 - (length % 4)) % 4) as usize;
    writer.write_all(&[0u8; 3][..pad])?;
    Ok(())
}

#[derive(Debug, Error)]
pub enum RecipeError {
    #[error("Venus reconstruction recipe version {0} is unsupported")]
    Version(u32),
    #[error("Venus reconstruction recipes exceed their storage budget")]
    Budget,
    #[error("invalid Venus reconstruction recipes: {0}")]
    Invalid(&'static str),
    #[error("cannot encode or decode a Venus reconstruction command: {0}")]
    Protocol(#[from] ProtocolError),
    #[error("cannot encode or decode Venus reconstruction recipes: {0}")]
    Wire(#[from] WireError),
    #[error("a Venus reconstruction command failed: {0}")]
    Replay(String),
    #[error("Venus checkpoint memory access failed: {0}")]
    Memory(#[from] crate::venus::shmem::ShmemError),
    #[error("Venus checkpoint I/O failed: {0}")]
    Io(#[from] std::io::Error),
}

/// GPU submissions and waits belong to the execution/synchronisation
/// checkpoint, not object reconstruction. Replaying historical GPU work can
/// read different memory, run twice, or wait for signals which no longer exist.
pub(super) fn is_recipe(command: &Command<'_>) -> bool {
    let name = command.name();
    // Allow state construction, not every operation the executor may gain in
    // the future. In particular getters can wait (GetQueryPoolResults), and
    // host image copies/queue operations can execute GPU work immediately.
    [
        "vkCreate",
        "vkDestroy",
        "vkAllocate",
        "vkFree",
        "vkBind",
        "vkCmd",
        "vkReset",
    ]
    .iter()
    .any(|prefix| name.starts_with(prefix))
        || matches!(
            name,
            "vkEnumeratePhysicalDevices"
                | "vkEnumeratePhysicalDeviceGroups"
                | "vkGetDeviceQueue"
                | "vkGetDeviceQueue2"
                | "vkBeginCommandBuffer"
                | "vkEndCommandBuffer"
                | "vkTrimCommandPool"
                | "vkUpdateDescriptorSets"
                | "vkUpdateDescriptorSetWithTemplate"
                | "vkMergePipelineCaches"
                | "vkSetPrivateData"
                | "vkSetEvent"
                | "vkSignalSemaphore"
                | "vkImportSemaphoreResourceMESA"
        )
}

pub(super) fn encode_recipe(command: &Command<'_>) -> Result<Option<Vec<u8>>, RecipeError> {
    if !is_recipe(command) {
        return Ok(None);
    }
    let mut encoder = Encoder::with_limit(MAX_RECORD_BYTES);
    command.encode_command(&mut encoder, 0)?;
    Ok(Some(encoder.finish()?))
}

/// A bounded journal. Crossing its budget disables *checkpointing*, leaving
/// ordinary guest execution alone. No truncated journal can be saved.
pub struct RecipeJournal {
    records: Vec<Vec<u8>>,
    pub(super) imports: BTreeMap<u64, super::checkpoint_imports::ImportRecipe>,
    pub(super) charge: Charge,
    failure: Option<RecipeError>,
}

impl RecipeJournal {
    pub(super) fn clear(&mut self) {
        self.records.clear();
        self.imports.clear();
        self.charge.shrink_to(0);
        self.failure = None;
    }
    pub(super) fn recording(&self) -> bool {
        self.failure.is_none()
    }
    pub fn new(budget: &Arc<PageBudget>) -> Self {
        Self {
            records: Vec::new(),
            imports: BTreeMap::new(),
            // Zero bytes cannot cross a budget, including a zero-sized one.
            charge: budget.charge_anyway(0),
            failure: None,
        }
    }

    pub(super) fn note(
        &mut self,
        encoded: Result<Option<Vec<u8>>, RecipeError>,
        completed: &Command<'_>,
    ) {
        if self.failure.is_some() {
            return;
        }
        let request = match encoded {
            Ok(Some(record)) => record,
            Ok(None) => return,
            Err(error) => {
                self.fail(error);
                return;
            }
        };
        // Some multi-object operations can change state before returning an
        // error. Until their rollback is represented, neither skipping them
        // nor replaying their original inputs is a reconstruction recipe.
        if super::context::result_of(completed).is_some_and(|ret| ret < 0) {
            self.fail(RecipeError::Invalid("a state-changing command failed"));
            return;
        }
        let record = (|| {
            let mut reply = Encoder::with_limit(MAX_RECORD_BYTES);
            completed.encode_reply(&mut reply)?;
            let reply = reply.finish()?;
            let mut record = Encoder::with_limit(MAX_RECORD_BYTES);
            record.u32(u32::try_from(request.len()).map_err(|_| RecipeError::Budget)?)?;
            record.blob(&request)?;
            record.blob(&reply)?;
            Ok(record.finish()?)
        })();
        if let Err(error) = record.and_then(|record| self.append(record)) {
            self.fail(error);
        }
    }

    pub(super) fn fail(&mut self, error: RecipeError) {
        self.failure = Some(error);
        self.records.clear();
        self.imports.clear();
        self.charge.shrink_to(0);
    }

    fn append(&mut self, bytes: Vec<u8>) -> Result<(), RecipeError> {
        let cost = (bytes.len() as u64).saturating_add(RECORD_COST);
        if self.charge.amount().saturating_add(cost) > MAX_RECIPE_BYTES {
            return Err(RecipeError::Budget);
        }
        self.charge.grow(cost).map_err(|_| RecipeError::Budget)?;
        self.records
            .try_reserve(1)
            .map_err(|_| RecipeError::Budget)?;
        self.records.push(bytes);
        Ok(())
    }

    /// Explicit framing: version, record count, then records containing the
    /// original request and its completed reply. Native handles never occur
    /// in either: replies carry the identifiers returned to the guest.
    pub fn encode(&self) -> Result<Vec<u8>, RecipeError> {
        if let Some(error) = &self.failure {
            return Err(RecipeError::Replay(error.to_string()));
        }
        let mut encoder = Encoder::with_limit(MAX_RECIPE_BYTES as usize);
        encoder.u32(RECIPE_VERSION)?;
        encoder.u32(u32::try_from(self.records.len()).map_err(|_| RecipeError::Budget)?)?;
        for record in &self.records {
            encoder.u32(u32::try_from(record.len()).map_err(|_| RecipeError::Budget)?)?;
            encoder.blob(record)?;
        }
        encoder.u32(self.imports.len() as u32)?;
        for meta in self.imports.values() {
            meta.encode(&mut encoder)?;
        }
        Ok(encoder.finish()?)
    }

    /// Validate every record before a caller creates any host objects. In
    /// particular a hostile file cannot insert a GPU submission into recipes.
    pub fn decode(bytes: &[u8], budget: &Arc<PageBudget>) -> Result<Self, RecipeError> {
        if bytes.len() as u64 > MAX_RECIPE_BYTES {
            return Err(RecipeError::Budget);
        }
        let mut decoder = Decoder::new(bytes);
        let version = decoder.u32()?;
        if version != RECIPE_VERSION {
            return Err(RecipeError::Version(version));
        }
        let count = decoder.u32()? as usize;
        if count > decoder.remaining() / 12 {
            return Err(RecipeError::Invalid("record count exceeds the file"));
        }
        let mut journal = Self::new(budget);
        let mut expected_imports = BTreeMap::new();
        let mut allocation_serial = 0u64;
        for _ in 0..count {
            let len = decoder.u32()? as usize;
            if !(12..=MAX_RECORD_BYTES).contains(&len) {
                return Err(RecipeError::Invalid("command length is outside its bounds"));
            }
            let record = decoder.blob(len)?;
            let mut record_decoder = Decoder::new(record);
            let request_len = record_decoder.u32()? as usize;
            let mut command_decoder = Decoder::new(record_decoder.blob(request_len)?);
            let (header, command) = Command::decode_next(&mut command_decoder)?;
            if command_decoder.remaining() != 0 || header.flags != 0 || !is_recipe(&command) {
                return Err(RecipeError::Invalid(
                    "record is not one reconstruction command",
                ));
            }
            if let Command::AllocateMemory(args) = &command {
                allocation_serial += 1;
                if super::checkpoint_imports::is_import(args) {
                    let info = args
                        .p_allocate_info
                        .as_ref()
                        .ok_or(RecipeError::Invalid("missing import info"))?;
                    expected_imports.insert(
                        allocation_serial,
                        (
                            args.device.0,
                            args.p_memory.map_or(0, |id| id.0),
                            info.allocation_size,
                        ),
                    );
                }
            }
            let mut owned = Vec::new();
            owned
                .try_reserve_exact(record.len())
                .map_err(|_| RecipeError::Budget)?;
            owned.extend_from_slice(record);
            journal.append(owned)?;
        }
        let imports = decoder.u32()? as usize;
        if imports != expected_imports.len() || imports > decoder.remaining() / 36 {
            return Err(RecipeError::Invalid("import metadata count differs"));
        }
        for _ in 0..imports {
            let meta = super::checkpoint_imports::ImportRecipe::decode(&mut decoder)?;
            let expected = expected_imports
                .remove(&meta.serial)
                .ok_or(RecipeError::Invalid(
                    "unknown or duplicate import generation",
                ))?;
            if (meta.device, meta.memory) != (expected.0, expected.1) || meta.host_size < expected.2
            {
                return Err(RecipeError::Invalid(
                    "import metadata does not match its command",
                ));
            }
            journal.note_import(meta)?;
        }
        if decoder.remaining() != 0 {
            return Err(RecipeError::Invalid("trailing bytes"));
        }
        Ok(journal)
    }

    pub(super) fn records(&self) -> &[Vec<u8>] {
        &self.records
    }
}

impl<H: HostVulkan> VulkanContext<H> {
    /// Write object-owned host-visible memory in bounded chunks. The caller
    /// must have stopped the guest, ring workers and GPU writers. Unlike
    /// reconstruction recipes these are the actual allocation contents.
    /// Exported orphan allocations and device-local images need their own
    /// checkpoint records; this is not a complete renderer checkpoint.
    pub fn save_host_memory(&self, writer: &mut impl Write) -> Result<(), RecipeError> {
        let pages: BTreeMap<_, _> = self.objects.checkpoint_pages().collect();
        writer.write_all(&VERSION.to_le_bytes())?;
        writer.write_all(
            &u32::try_from(pages.len())
                .map_err(|_| RecipeError::Budget)?
                .to_le_bytes(),
        )?;
        let mut chunk = [0u8; 64 << 10];
        for (id, pages) in pages {
            writer.write_all(&id.to_le_bytes())?;
            writer.write_all(&pages.resource_len().to_le_bytes())?;
            let mut at = 0;
            while at < pages.resource_len() {
                let take = (pages.resource_len() - at).min(chunk.len() as u64) as usize;
                pages.read_bytes(at, &mut chunk[..take])?;
                writer.write_all(&chunk[..take])?;
                at += take as u64;
            }
            write_padding(writer, pages.resource_len())?;
        }
        Ok(())
    }

    /// Apply memory only after the complete record set has been checked
    /// against freshly reconstructed allocations. A bad trailing record must
    /// not half-apply earlier records. No guest/GPU writer may run yet.
    pub fn restore_host_memory(&self, bytes: &[u8]) -> Result<(), RecipeError> {
        let pages: BTreeMap<_, _> = self.objects.checkpoint_pages().collect();
        let mut decoder = Decoder::new(bytes);
        let version = decoder.u32()?;
        if version != VERSION {
            return Err(RecipeError::Version(version));
        }
        let count = decoder.u32()? as usize;
        if count != pages.len() || count > decoder.remaining() / 16 {
            return Err(RecipeError::Invalid(
                "host-memory allocation count does not match",
            ));
        }
        let mut records = Vec::new();
        records
            .try_reserve_exact(count)
            .map_err(|_| RecipeError::Budget)?;
        let mut seen = HashSet::new();
        for _ in 0..count {
            let id = decoder.u64()?;
            let len = decoder.u64()?;
            let target = pages
                .get(&id)
                .ok_or(RecipeError::Invalid("unknown host-memory allocation"))?;
            if !seen.insert(id) || len != target.resource_len() {
                return Err(RecipeError::Invalid(
                    "duplicate or resized host-memory allocation",
                ));
            }
            let len = usize::try_from(len).map_err(|_| RecipeError::Budget)?;
            records.push((*target, decoder.blob(len)?));
        }
        if decoder.remaining() != 0 {
            return Err(RecipeError::Invalid("trailing host-memory bytes"));
        }
        for (target, data) in records {
            target.write_bytes(0, data)?;
        }
        Ok(())
    }

    /// Enable recipe retention before this context executes its first object
    /// creation. The budget may be shared between every context of a renderer.
    pub fn enable_object_recipes(&mut self, budget: &Arc<PageBudget>) -> Result<(), RecipeError> {
        if self.object_count() != 0
            || self.recipes.is_some()
            || self.memory_sequence != 0
            || self.is_fatal()
        {
            return Err(RecipeError::Invalid(
                "recipe retention must start on a fresh context",
            ));
        }
        self.recipes = Some(RecipeJournal::new(budget));
        self.payloads.ownership.enable();
        Ok(())
    }

    /// Only the reconstruction component of a checkpoint, not a complete
    /// context checkpoint. The renderer must still checkpoint memory and rings.
    pub fn save_object_recipes(&self) -> Result<Vec<u8>, RecipeError> {
        self.recipes
            .as_ref()
            .ok_or(RecipeError::Invalid("recipe retention was not enabled"))?
            .encode()
    }

    /// Reconstruct into a fresh context. Validate the whole recipe framing and
    /// protocol first; on a host/executor failure destroy all partially created
    /// objects before returning. No guest queue may be running during this call.
    pub fn restore_object_recipes(
        &mut self,
        bytes: &[u8],
        budget: &Arc<PageBudget>,
    ) -> Result<(), RecipeError> {
        self.restore_object_recipes_with_shared(bytes, budget, &[])
    }

    pub(super) fn restore_recipe_journal(
        &mut self,
        bytes: &[u8],
        budget: &Arc<PageBudget>,
    ) -> Result<(), RecipeError> {
        if self.object_count() != 0 || self.recipes.is_some() || self.is_fatal() {
            return Err(RecipeError::Invalid("recipe restore needs a fresh context"));
        }
        let journal = RecipeJournal::decode(bytes, budget)?;
        for record in journal.records() {
            let mut framing = Decoder::new(record);
            let request_len = framing.u32()? as usize;
            let mut decoder = Decoder::new(framing.blob(request_len)?);
            let (_, mut command) = Command::decode_next(&mut decoder)?;
            let expected_reply = framing.blob(framing.remaining())?;
            self.command_bytes = request_len;
            self.restoring_import = if matches!(command, Command::AllocateMemory(_)) {
                journal.imports.get(&(self.memory_sequence + 1)).copied()
            } else {
                None
            };
            let executed = self.execute(&mut command);
            self.restoring_import = None;
            if executed.is_err() || super::context::result_of(&command).is_some_and(|v| v < 0) {
                let reason = executed.err().map_or_else(
                    || {
                        format!(
                            "{} returned {:?}",
                            command.name(),
                            super::context::result_of(&command)
                        )
                    },
                    |error| error.to_string(),
                );
                self.destroy_all();
                return Err(RecipeError::Replay(reason));
            }
            let mut reply = Encoder::with_limit(MAX_RECORD_BYTES);
            let actual_reply = command
                .encode_reply(&mut reply)
                .map_err(RecipeError::from)
                .and_then(|()| reply.finish().map_err(RecipeError::from));
            match actual_reply {
                Ok(actual) if actual == expected_reply => {}
                other => {
                    let reason = match other {
                        Err(error) => error.to_string(),
                        Ok(_) => format!("{} reconstruction outputs changed", command.name()),
                    };
                    self.destroy_all();
                    return Err(RecipeError::Replay(reason));
                }
            }
        }
        if self
            .objects
            .checkpoint_memories()
            .any(|(_, memory)| memory.checkpoint_placeholder)
        {
            self.destroy_all();
            return Err(RecipeError::Invalid(
                "a live import has no restored shared payload",
            ));
        }
        self.recipes = Some(journal);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::super::fake::FakeVulkan;
    use super::super::harness::*;
    use super::*;
    use crate::venus::protocol::*;

    fn execute(context: &mut VulkanContext<FakeVulkan>, mut command: Command<'_>) {
        context.execute(&mut command).expect("execute");
        assert!(super::super::context::result_of(&command).is_none_or(|v| v >= 0));
    }

    #[test]
    fn object_namespace_is_rebuilt_on_a_fresh_host_without_native_handles() {
        let host = Arc::new(FakeVulkan::standard());
        let budget = PageBudget::new(MAX_RECIPE_BYTES);
        let mut original = VulkanContext::new(1, Arc::clone(&host));
        original.enable_object_recipes(&budget).unwrap();
        execute(&mut original, create_instance(INSTANCE));
        execute(&mut original, enumerate(INSTANCE, Some(vec![PHYSICAL])));
        execute(&mut original, create_device(PHYSICAL, DEVICE, Vec::new()));
        execute(&mut original, device_queue(DEVICE, QUEUE, 1));
        execute(&mut original, create_pool(DEVICE, POOL));
        execute(
            &mut original,
            create_buffer(DEVICE, BUFFER, buffer_info(4096, TRANSFER)),
        );
        execute(&mut original, allocate(DEVICE, MEMORY, 4096, 1, Vec::new()));
        execute(&mut original, bind_buffers(DEVICE, &[(BUFFER, MEMORY, 0)]));
        let count = original.object_count();
        let bytes = original.save_object_recipes().unwrap();
        original.destroy_all();
        assert_eq!(host.live_objects(), 0);
        drop(original);
        assert_eq!(budget.used(), 0);

        let fresh_host = Arc::new(FakeVulkan::standard());
        let mut restored = VulkanContext::new(1, Arc::clone(&fresh_host));
        restored.restore_object_recipes(&bytes, &budget).unwrap();
        assert_eq!(restored.object_count(), count);
        assert_eq!(fresh_host.live("buffer"), 1);
        // The guest's old identifiers still address the rebuilt objects.
        execute(&mut restored, buffer_requirements(DEVICE, BUFFER));
        execute(&mut restored, destroy_buffer(DEVICE, BUFFER));
        assert_eq!(fresh_host.live("buffer"), 0);
        restored.destroy_all();
        assert_eq!(fresh_host.live_objects(), 0);
    }

    #[test]
    fn exhausted_recipe_storage_does_not_kill_guest_execution_or_write_a_partial_checkpoint() {
        let host = Arc::new(FakeVulkan::standard());
        let budget = PageBudget::new(1);
        let mut context = VulkanContext::new(1, Arc::clone(&host));
        context.enable_object_recipes(&budget).unwrap();
        execute(&mut context, create_instance(INSTANCE));
        assert!(!context.is_fatal());
        assert!(context.save_object_recipes().is_err());
        assert_eq!(budget.used(), 0);
        context.destroy_all();
        assert_eq!(host.live_objects(), 0);
    }

    #[test]
    fn a_hostile_recipe_file_is_refused_before_touching_the_host() {
        let budget = PageBudget::new(MAX_RECIPE_BYTES);
        let mut valid = RecipeJournal::new(&budget);
        let command = create_instance(INSTANCE);
        valid.note(encode_recipe(&command), &command);
        let bytes = valid.encode().unwrap();
        drop(valid);
        for end in 0..bytes.len() {
            let host = Arc::new(FakeVulkan::standard());
            let mut context = VulkanContext::new(1, Arc::clone(&host));
            assert!(context
                .restore_object_recipes(&bytes[..end], &budget)
                .is_err());
            assert_eq!(host.live_objects(), 0);
            assert_eq!(budget.used(), 0);
        }
        for (offset, value) in [(0, 99u32), (4, u32::MAX), (8, u32::MAX)] {
            let mut bad = bytes.clone();
            bad[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
            assert!(RecipeJournal::decode(&bad, &budget).is_err());
            assert_eq!(budget.used(), 0);
        }
    }

    #[test]
    fn a_semantically_bad_recipe_destroys_partial_host_objects() {
        let budget = PageBudget::new(MAX_RECIPE_BYTES);
        let mut journal = RecipeJournal::new(&budget);
        let command = create_instance(INSTANCE);
        journal.note(encode_recipe(&command), &command);
        let command = create_device(0xdead, DEVICE, Vec::new());
        journal.note(encode_recipe(&command), &command);
        let bytes = journal.encode().unwrap();
        drop(journal);
        let host = Arc::new(FakeVulkan::standard());
        let mut restored = VulkanContext::new(1, Arc::clone(&host));
        assert!(restored.restore_object_recipes(&bytes, &budget).is_err());
        assert_eq!(
            host.live_objects(),
            0,
            "the instance created before the error was destroyed"
        );
        assert_eq!(budget.used(), 0);
    }

    #[test]
    fn a_file_cannot_smuggle_gpu_execution_into_reconstruction() {
        let budget = PageBudget::new(MAX_RECIPE_BYTES);
        let mut journal = RecipeJournal::new(&budget);
        let command = create_instance(INSTANCE);
        journal.note(encode_recipe(&command), &command);
        let command = Command::DeviceWaitIdle(DeviceWaitIdleArgs {
            device: VkDevice(DEVICE),
            ret: 0,
        });
        assert!(encode_recipe(&command).unwrap().is_none());
        let mut encoder = Encoder::new();
        command.encode_command(&mut encoder, 0).unwrap();
        journal.append(encoder.finish().unwrap()).unwrap();
        let bytes = journal.encode().unwrap();
        drop(journal);
        let host = Arc::new(FakeVulkan::standard());
        let mut restored = VulkanContext::new(1, Arc::clone(&host));
        assert!(restored.restore_object_recipes(&bytes, &budget).is_err());
        assert_eq!(
            host.live_objects(),
            0,
            "the whole file was checked before the first create"
        );
        assert_eq!(budget.used(), 0);
    }

    #[test]
    fn changed_creation_outputs_refuse_restore_and_destroy_the_rebuilt_objects() {
        let budget = PageBudget::new(MAX_RECIPE_BYTES);
        let mut journal = RecipeJournal::new(&budget);
        let request = create_instance(INSTANCE);
        let mut completed = request.clone();
        if let Command::CreateInstance(args) = &mut completed {
            // A successful replay with another guest id must not pass just
            // because both Vulkan calls reported success. This also catches
            // partial pipeline creation changing after a driver cache warms.
            args.p_instance = Some(VkInstance(INSTANCE + 1));
        }
        journal.note(encode_recipe(&request), &completed);
        let bytes = journal.encode().unwrap();
        drop(journal);
        let host = Arc::new(FakeVulkan::standard());
        let mut restored = VulkanContext::new(1, Arc::clone(&host));
        assert!(matches!(
            restored.restore_object_recipes(&bytes, &budget),
            Err(RecipeError::Replay(_))
        ));
        assert_eq!(host.live_objects(), 0);
        assert_eq!(restored.object_count(), 0);
        assert_eq!(budget.used(), 0);
    }

    #[test]
    fn a_failed_state_change_cannot_silently_disappear_from_the_checkpoint() {
        let budget = PageBudget::new(MAX_RECIPE_BYTES);
        let mut journal = RecipeJournal::new(&budget);
        let request = create_instance(INSTANCE);
        journal.note(encode_recipe(&request), &request);
        assert!(budget.used() > 0);
        let mut completed = request.clone();
        if let Command::CreateInstance(args) = &mut completed {
            args.ret = VK_ERROR_OUT_OF_HOST_MEMORY;
            args.p_instance = Some(VkInstance(0));
        }
        journal.note(encode_recipe(&request), &completed);
        assert!(journal.encode().is_err());
        assert!(!journal.recording());
        assert_eq!(budget.used(), 0);
    }

    #[test]
    fn imported_host_pages_are_checkpointed_and_restored_exactly() {
        let host = Arc::new(FakeVulkan::standard());
        let budget = PageBudget::new(MAX_RECIPE_BYTES);
        let mut original = VulkanContext::new(1, Arc::clone(&host));
        original.enable_object_recipes(&budget).unwrap();
        execute(&mut original, create_instance(INSTANCE));
        execute(&mut original, enumerate(INSTANCE, Some(vec![PHYSICAL])));
        execute(&mut original, create_device(PHYSICAL, DEVICE, Vec::new()));
        execute(
            &mut original,
            allocate(DEVICE, MEMORY, (64 << 10) + 1, 3, Vec::new()),
        );
        let pattern: Vec<u8> = (0..65537).map(|i| (i % 251) as u8).collect();
        original
            .objects
            .memory(DEVICE, MEMORY)
            .unwrap()
            .pages
            .as_ref()
            .unwrap()
            .write_bytes(0, &pattern)
            .unwrap();
        let recipes = original.save_object_recipes().unwrap();
        let mut memory = Vec::new();
        original.save_host_memory(&mut memory).unwrap();
        original.destroy_all();
        drop(original);
        assert_eq!(host.live_objects(), 0);

        let mut restored = VulkanContext::new(1, Arc::clone(&host));
        restored.restore_object_recipes(&recipes, &budget).unwrap();
        let pages = restored
            .objects
            .memory(DEVICE, MEMORY)
            .unwrap()
            .pages
            .as_ref()
            .unwrap();
        let mut data = vec![0u8; pattern.len()];
        pages.read_bytes(0, &mut data).unwrap();
        assert_eq!(data, vec![0u8; pattern.len()]);
        let mut bad = memory.clone();
        bad.push(0);
        assert!(restored.restore_host_memory(&bad).is_err());
        pages.read_bytes(0, &mut data).unwrap();
        assert_eq!(
            data,
            vec![0u8; pattern.len()],
            "bad trailing data did not half-apply pages"
        );
        restored.restore_host_memory(&memory).unwrap();
        pages.read_bytes(0, &mut data).unwrap();
        assert_eq!(data, pattern);
        restored.destroy_all();
        assert_eq!(host.live_objects(), 0);
    }
}
