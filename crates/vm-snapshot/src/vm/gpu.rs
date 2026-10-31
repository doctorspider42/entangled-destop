//! Streaming device payloads inside the atomic whole-VM snapshot. The small
//! virtio section still owns transport registers; this one owns GPU content.
use super::*;
use crate::format::VerifiedSectionReader;
use std::fs::File;
use std::io::Write;

const VERSION: u32 = 1;
const MAX_BYTES: u64 = 64 << 30;
const MAX_SLOTS: usize = 256;
const GPU_DEVICE_TYPE: u32 = 16;

/// Supplies archives for a bounded, sorted set of virtio GPU slots. Save must
/// stream data and propagate errors; it never buffers all VRAM in this crate.
pub trait GpuSave {
    fn slots(&self) -> &[u32];
    fn save(&mut self, slot: u32, out: &mut dyn Write) -> Result<()>;
}

/// Owns the file opened for the successful whole-VM restore, so GPU loading
/// cannot accidentally reopen a replaced path. Every read verifies the section
/// digest, then authenticates each random-access cache refill against it.
#[derive(Debug)]
pub struct GpuArchives {
    reader: SnapshotReader<File>,
    slots: Vec<u32>,
}
impl GpuArchives {
    pub(crate) fn new(reader: SnapshotReader<File>, slots: Vec<u32>) -> Self {
        Self { reader, slots }
    }
    pub fn slots(&self) -> &[u32] {
        &self.slots
    }
    pub fn section(&mut self, slot: u32) -> Result<VerifiedSectionReader<'_, File>> {
        if self.slots.binary_search(&slot).is_err() {
            return Err(SnapshotError::Restore(format!(
                "no GPU archive for virtio slot {slot}"
            )));
        }
        self.reader.verified_section(SectionKind::Gpu, slot)
    }
}

fn validate_slots(slots: &[u32], devices: &[machine_x86::state::SavedVirtioSlot]) -> Result<()> {
    if slots.len() > MAX_SLOTS || slots.windows(2).any(|pair| pair[0] >= pair[1]) {
        return Err(SnapshotError::Restore(
            "GPU archive slots are oversized, duplicated or unsorted".into(),
        ));
    }
    for slot in slots {
        if !devices
            .iter()
            .any(|device| device.slot == *slot && device.state.device_type == GPU_DEVICE_TYPE)
        {
            return Err(SnapshotError::Restore(format!(
                "GPU archive names a missing/non-GPU virtio slot {slot}"
            )));
        }
    }
    Ok(())
}

struct BoundedWriter<'a> {
    out: &'a mut dyn Write,
    written: u64,
}
impl Write for BoundedWriter<'_> {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if bytes.len() as u64 > MAX_BYTES.saturating_sub(self.written) {
            return Err(std::io::Error::other("GPU archive exceeds its size limit"));
        }
        let count = self.out.write(bytes)?;
        if count > bytes.len() {
            return Err(std::io::Error::other("invalid GPU archive writer result"));
        }
        self.written += count as u64;
        Ok(count)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.out.flush()
    }
}

pub(crate) fn save_sections(
    writer: &mut SnapshotWriter<File>,
    devices: &[machine_x86::state::SavedVirtioSlot],
    gpu: Option<&mut dyn GpuSave>,
) -> Result<()> {
    let Some(gpu) = gpu else {
        return Ok(());
    };
    validate_slots(gpu.slots(), devices)?;
    let slots = gpu.slots().to_vec(); // <= 256 trusted/validated scalar slot ids
    for slot in slots {
        let mut section = writer.section(SectionKind::Gpu, VERSION, slot)?;
        let mut bounded = BoundedWriter {
            out: &mut section,
            written: 0,
        };
        gpu.save(slot, &mut bounded)?;
        if bounded.written == 0 {
            return Err(SnapshotError::Restore("GPU archive is empty".into()));
        }
        section.finish()?;
    }
    Ok(())
}

pub(crate) fn validate_sections(
    reader: &mut SnapshotReader<File>,
    devices: &[machine_x86::state::SavedVirtioSlot],
    allow_gpu: bool,
) -> Result<Vec<u32>> {
    let slots = reader.instances(SectionKind::Gpu);
    if !slots.is_empty() && !allow_gpu {
        return Err(SnapshotError::Restore(
            "this restore path cannot apply saved accelerated GPU state".into(),
        ));
    }
    validate_slots(&slots, devices)?;
    for slot in &slots {
        reader.require_version(SectionKind::Gpu, *slot, VERSION)?;
        let section = reader
            .sections()
            .iter()
            .find(|entry| entry.kind == SectionKind::Gpu && entry.instance == *slot)
            .ok_or(SnapshotError::MissingSection("gpu"))?;
        if section.len == 0 || section.len > MAX_BYTES {
            return Err(SnapshotError::TooLarge {
                what: "GPU archive bytes",
                value: section.len,
                max: MAX_BYTES,
            });
        }
        // Validate before guest RAM changes. Later uploads verify again using
        // this reader's original directory digest and the very same file.
        reader.verified_section(SectionKind::Gpu, *slot)?;
    }
    Ok(slots)
}
