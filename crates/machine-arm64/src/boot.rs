//! Checked direct Linux Image loading using rust-vmm's ARM64 loader.
use crate::{MachineError, RAM_BASE};
use linux_loader::loader::{pe::PE, KernelLoader};
use std::io::{BufRead, Cursor, Read};
use std::path::Path;
use vm_memory::{Bytes, GuestAddress, GuestMemory};
use vmm_core::GuestMem;

pub const MAX_IMAGE_BYTES: u64 = 256 << 20;
pub const INITRD_OFFSET: u64 = 256 << 20;
pub const DTB_RESERVE: u64 = 2 << 20;

pub fn read_image(path: &Path) -> Result<Vec<u8>, MachineError> {
    let file = std::fs::File::open(path)?;
    let mut reader = std::io::BufReader::new(file);
    let magic = reader.fill_buf().map_err(MachineError::Serial)?;
    let gzip = magic.starts_with(&[0x1f, 0x8b]);
    let reader: Box<dyn Read> = if gzip {
        Box::new(flate2::read::GzDecoder::new(reader))
    } else {
        Box::new(reader)
    };
    let mut bytes = Vec::new();
    reader.take(MAX_IMAGE_BYTES + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_IMAGE_BYTES {
        return Err(MachineError::Boot("image exceeds 256 MiB".into()));
    }
    Ok(bytes)
}

pub fn load_kernel(mem: &GuestMem, image: &[u8]) -> Result<u64, MachineError> {
    if image.len() < 64 || image.len() as u64 > MAX_IMAGE_BYTES {
        return Err(MachineError::Boot(
            "kernel header is truncated or image exceeds the size cap".into(),
        ));
    }
    let field = |offset: usize| -> Result<u64, MachineError> {
        let bytes = image[offset..offset + 8]
            .try_into()
            .map_err(|_| MachineError::Boot("truncated image field".into()))?;
        Ok(u64::from_le_bytes(bytes))
    };
    let text_offset = field(8)?;
    let image_size = field(16)?;
    if image_size == 0 || field(24)? & 1 != 0 {
        return Err(MachineError::Boot(
            "requires a modern little-endian ARM64 Image".into(),
        ));
    }
    let start = RAM_BASE
        .checked_add(text_offset)
        .ok_or_else(|| MachineError::Boot("kernel address overflows".into()))?;
    // image_size includes the runtime BSS, which the PE loader's file span
    // does not reserve. Reject overlap before writing any guest memory.
    let span = image_size.max(image.len() as u64);
    let end = start
        .checked_add(span)
        .filter(|end| *end <= RAM_BASE + INITRD_OFFSET)
        .ok_or_else(|| MachineError::Boot("kernel overlaps the initrd area".into()))?;
    if !mem.check_range(GuestAddress(start), (end - start) as usize) {
        return Err(MachineError::Boot(
            "kernel runtime span is outside RAM".into(),
        ));
    }
    let result = PE::load(
        mem,
        Some(GuestAddress(RAM_BASE)),
        &mut Cursor::new(image),
        None,
    )
    .map_err(|e| MachineError::Boot(e.to_string()))?;
    if result.kernel_end > RAM_BASE + INITRD_OFFSET {
        return Err(MachineError::Boot("kernel overlaps the initrd area".into()));
    }
    Ok(result.kernel_load.0)
}

pub fn write_initrd(
    mem: &GuestMem,
    bytes: &[u8],
    ram_bytes: u64,
) -> Result<(u64, u64), MachineError> {
    let start = RAM_BASE + INITRD_OFFSET;
    let end = start
        .checked_add(bytes.len() as u64)
        .ok_or_else(|| MachineError::Boot("initrd size overflows".into()))?;
    let limit = ram_bytes
        .checked_sub(DTB_RESERVE)
        .and_then(|bytes| RAM_BASE.checked_add(bytes))
        .ok_or_else(|| MachineError::Boot("invalid RAM size".into()))?;
    if bytes.is_empty() || end > limit {
        return Err(MachineError::Boot(
            "initrd overlaps Device Tree or is empty".into(),
        ));
    }
    mem.write_slice(bytes, GuestAddress(start))
        .map_err(|e| MachineError::Boot(e.to_string()))?;
    Ok((start, bytes.len() as u64))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn image(offset: u64, size: u64) -> Vec<u8> {
        let mut image = vec![0; 64];
        image[8..16].copy_from_slice(&offset.to_le_bytes());
        image[16..24].copy_from_slice(&size.to_le_bytes());
        image[56..60].copy_from_slice(&0x644d5241u32.to_le_bytes());
        image
    }

    #[test]
    fn kernel_runtime_span_is_checked_before_writing_the_file() {
        let mem = GuestMem::from_ranges(&[(GuestAddress(RAM_BASE), 2 << 20)]).unwrap();
        mem.write_slice(&[0xa5; 64], GuestAddress(RAM_BASE))
            .unwrap();
        for bytes in [
            image(0, 3 << 20),
            image(u64::MAX, 64),
            image(0, 0),
            image(INITRD_OFFSET, 64),
        ] {
            assert!(load_kernel(&mem, &bytes).is_err());
            let mut after = [0; 64];
            mem.read_slice(&mut after, GuestAddress(RAM_BASE)).unwrap();
            assert_eq!(after, [0xa5; 64]);
        }
        assert_eq!(load_kernel(&mem, &image(0, 1 << 20)).unwrap(), RAM_BASE);
    }

    #[test]
    fn invalid_initrd_geometry_cannot_underflow_or_write_ram() {
        let start = RAM_BASE + INITRD_OFFSET;
        let mem = GuestMem::from_ranges(&[(GuestAddress(start), 0x10000)]).unwrap();
        mem.write_slice(&[0xa5], GuestAddress(start)).unwrap();
        for ram in [0, 1, DTB_RESERVE - 1, INITRD_OFFSET, u64::MAX] {
            assert!(write_initrd(&mem, &[0], ram).is_err());
            assert_eq!(mem.read_obj::<u8>(GuestAddress(start)).unwrap(), 0xa5);
        }
        assert!(write_initrd(&mem, &[], 512 << 20).is_err());
        assert_eq!(write_initrd(&mem, &[0x42], 512 << 20).unwrap(), (start, 1));
    }
}
