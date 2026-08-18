//! Firmware image inspection: is this a PVH ELF, or a flash blob for the
//! reset vector? Pure parsing — no guest memory, no host OS assumptions, so it
//! is tested on every development platform.
//!
//! The guest supplies nothing here: the image comes from the VM profile, i.e.
//! from the host operator. It is still parsed defensively (every offset is
//! bounds-checked, every arithmetic step is checked) because a truncated or
//! hand-edited `.fd` must produce a typed error, never a panic.

use std::path::{Path, PathBuf};

use crate::{read_image, FirmwareError};

/// `EI_NIDENT`-prefixed ELF magic.
const ELF_MAGIC: [u8; 4] = [0x7f, b'E', b'L', b'F'];
/// `EI_CLASS` value for 64-bit ELF (CloudHv is ELF64 even though its
/// `e_machine` says i386 — it is entered in 32-bit mode).
const ELFCLASS64: u8 = 2;
/// `EI_DATA` value for little endian.
const ELFDATA2LSB: u8 = 1;
/// `p_type` of a note segment.
const PT_NOTE: u32 = 4;
/// Xen's note type for the 32-bit entry point.
const XEN_ELFNOTE_PHYS32_ENTRY: u32 = 18;
/// `n_name` of the notes we care about, NUL included.
const XEN_NOTE_NAME: &[u8] = b"Xen\0";

/// How a firmware image must be entered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FirmwareKind {
    /// An ELF with `XEN_ELFNOTE_PHYS32_ENTRY`: load the program headers into
    /// guest RAM and enter at this address in 32-bit protected mode.
    PvhElf { entry: u64 },
    /// Anything else: map at the top of the 32-bit address space and let the
    /// vCPU come out of reset at `0xffff_fff0`.
    ResetVector,
}

/// A firmware image read into host memory, with its entry protocol decided.
#[derive(Debug, Clone)]
pub struct FirmwareImage {
    path: PathBuf,
    bytes: Vec<u8>,
    kind: FirmwareKind,
}

impl FirmwareImage {
    pub fn read(path: &Path) -> Result<Self, FirmwareError> {
        let bytes = read_image(path)?;
        let kind = classify(&bytes)?;
        Ok(Self {
            path: path.to_path_buf(),
            bytes,
            kind,
        })
    }

    /// Classifies an in-memory image; the constructor tests use it directly.
    pub fn from_bytes(path: &Path, bytes: Vec<u8>) -> Result<Self, FirmwareError> {
        if bytes.is_empty() {
            return Err(FirmwareError::Empty {
                path: path.to_path_buf(),
            });
        }
        let kind = classify(&bytes)?;
        Ok(Self {
            path: path.to_path_buf(),
            bytes,
            kind,
        })
    }

    pub fn kind(&self) -> FirmwareKind {
        self.kind
    }

    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    pub fn len(&self) -> u64 {
        self.bytes.len() as u64
    }

    pub fn is_empty(&self) -> bool {
        self.bytes.is_empty()
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

/// Reads a little-endian integer out of `bytes` at `offset`, or `None` when it
/// does not fit.
fn read_u16(bytes: &[u8], offset: usize) -> Option<u16> {
    let raw: [u8; 2] = bytes.get(offset..offset.checked_add(2)?)?.try_into().ok()?;
    Some(u16::from_le_bytes(raw))
}

fn read_u32(bytes: &[u8], offset: usize) -> Option<u32> {
    let raw: [u8; 4] = bytes.get(offset..offset.checked_add(4)?)?.try_into().ok()?;
    Some(u32::from_le_bytes(raw))
}

fn read_u64(bytes: &[u8], offset: usize) -> Option<u64> {
    let raw: [u8; 8] = bytes.get(offset..offset.checked_add(8)?)?.try_into().ok()?;
    Some(u64::from_le_bytes(raw))
}

/// Decides how an image must be entered.
///
/// A non-ELF image is a flash blob by definition — that is not an error, it is
/// the other supported shape. An image that *is* an ELF64 but carries no PVH
/// note is an error: it cannot be executed either way, and silently mapping it
/// as a ROM would be a much worse diagnostic than saying so.
fn classify(bytes: &[u8]) -> Result<FirmwareKind, FirmwareError> {
    if bytes.len() < 4 || bytes[..4] != ELF_MAGIC {
        return Ok(FirmwareKind::ResetVector);
    }
    match pvh_entry(bytes)? {
        Some(entry) => Ok(FirmwareKind::PvhElf { entry }),
        None => Err(FirmwareError::MalformedElf(
            "ELF image without a XEN_ELFNOTE_PHYS32_ENTRY note: not a PVH \
             firmware, and an ELF cannot be entered through the reset vector",
        )),
    }
}

/// Scans the ELF64 program headers for a `PT_NOTE` segment carrying
/// `XEN_ELFNOTE_PHYS32_ENTRY` and returns the entry point it encodes.
///
/// ELF64 header offsets used: `e_ident[4] = EI_CLASS`, `e_ident[5] = EI_DATA`,
/// `e_phoff = 0x20`, `e_phentsize = 0x36`, `e_phnum = 0x38`. Program header
/// offsets: `p_type = 0x00`, `p_offset = 0x08`, `p_filesz = 0x20`.
fn pvh_entry(bytes: &[u8]) -> Result<Option<u64>, FirmwareError> {
    let bad = FirmwareError::MalformedElf;
    if bytes.get(4) != Some(&ELFCLASS64) {
        return Err(bad("only ELF64 firmware images are supported"));
    }
    if bytes.get(5) != Some(&ELFDATA2LSB) {
        return Err(bad("big-endian ELF on a little-endian machine"));
    }
    let phoff = read_u64(bytes, 0x20).ok_or(bad("truncated ELF header"))? as usize;
    let phentsize = read_u16(bytes, 0x36).ok_or(bad("truncated ELF header"))? as usize;
    let phnum = read_u16(bytes, 0x38).ok_or(bad("truncated ELF header"))? as usize;
    if phentsize < 0x38 {
        return Err(bad("ELF64 program header entry smaller than 56 bytes"));
    }

    for i in 0..phnum {
        let ph = phoff
            .checked_add(
                i.checked_mul(phentsize)
                    .ok_or(bad("program header overflow"))?,
            )
            .ok_or(bad("program header overflow"))?;
        let Some(p_type) = read_u32(bytes, ph) else {
            return Err(bad("program header table runs past the end of the image"));
        };
        if p_type != PT_NOTE {
            continue;
        }
        let p_offset = read_u64(bytes, ph + 0x08).ok_or(bad("truncated program header"))? as usize;
        let p_filesz = read_u64(bytes, ph + 0x20).ok_or(bad("truncated program header"))? as usize;
        let end = p_offset
            .checked_add(p_filesz)
            .ok_or(bad("note segment size overflow"))?;
        let notes = bytes
            .get(p_offset..end)
            .ok_or(bad("note segment runs past the end of the image"))?;
        if let Some(entry) = scan_notes(notes)? {
            return Ok(Some(entry));
        }
    }
    Ok(None)
}

/// Walks an ELF note section (`n_namesz`, `n_descsz`, `n_type`, name, desc —
/// each field 4-byte aligned) looking for Xen's 32-bit entry note.
fn scan_notes(notes: &[u8]) -> Result<Option<u64>, FirmwareError> {
    let bad = FirmwareError::MalformedElf;
    let align4 = |n: usize| n.checked_add(3).map(|n| n & !3);
    let mut cursor = 0usize;
    while cursor + 12 <= notes.len() {
        let namesz = read_u32(notes, cursor).ok_or(bad("truncated note header"))? as usize;
        let descsz = read_u32(notes, cursor + 4).ok_or(bad("truncated note header"))? as usize;
        let ntype = read_u32(notes, cursor + 8).ok_or(bad("truncated note header"))?;
        let name_at = cursor + 12;
        let desc_at = name_at
            .checked_add(align4(namesz).ok_or(bad("note name size overflow"))?)
            .ok_or(bad("note name size overflow"))?;
        let next = desc_at
            .checked_add(align4(descsz).ok_or(bad("note desc size overflow"))?)
            .ok_or(bad("note desc size overflow"))?;
        if next > notes.len() {
            return Err(bad("note runs past the end of the note segment"));
        }
        if ntype == XEN_ELFNOTE_PHYS32_ENTRY
            && notes.get(name_at..name_at + namesz) == Some(XEN_NOTE_NAME)
        {
            // Xen encodes the entry as 4 or 8 bytes depending on the producer;
            // EDK2's CloudHv header writes 4.
            let entry = match descsz {
                4 => u64::from(read_u32(notes, desc_at).ok_or(bad("truncated note desc"))?),
                8 => read_u64(notes, desc_at).ok_or(bad("truncated note desc"))?,
                _ => return Err(bad("XEN_ELFNOTE_PHYS32_ENTRY is neither 4 nor 8 bytes")),
            };
            return Ok(Some(entry));
        }
        cursor = next;
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Builds the minimal ELF64 the classifier must accept: header, one
    /// `PT_NOTE` program header, one Xen entry note. Mirrors the shape of
    /// `OvmfPkg/CloudHv/CloudHvElfHeader.fdf.inc`.
    fn pvh_elf(entry: u32, note_type: u32, name: &[u8]) -> Vec<u8> {
        let phoff = 0x40usize;
        let phentsize = 0x38usize;
        let notes_at = 0xb0usize;
        let mut v = vec![0u8; notes_at];
        v[..4].copy_from_slice(&ELF_MAGIC);
        v[4] = ELFCLASS64;
        v[5] = ELFDATA2LSB;
        v[0x20..0x28].copy_from_slice(&(phoff as u64).to_le_bytes());
        v[0x36..0x38].copy_from_slice(&(phentsize as u16).to_le_bytes());
        v[0x38..0x3a].copy_from_slice(&1u16.to_le_bytes());

        // PT_NOTE program header at phoff.
        let note_len = 12 + name.len().next_multiple_of(4) + 4;
        v[phoff..phoff + 4].copy_from_slice(&PT_NOTE.to_le_bytes());
        v[phoff + 0x08..phoff + 0x10].copy_from_slice(&(notes_at as u64).to_le_bytes());
        v[phoff + 0x20..phoff + 0x28].copy_from_slice(&(note_len as u64).to_le_bytes());

        v.extend_from_slice(&(name.len() as u32).to_le_bytes());
        v.extend_from_slice(&4u32.to_le_bytes());
        v.extend_from_slice(&note_type.to_le_bytes());
        v.extend_from_slice(name);
        while v.len() % 4 != 0 {
            v.push(0);
        }
        v.extend_from_slice(&entry.to_le_bytes());
        v
    }

    fn classify_bytes(bytes: Vec<u8>) -> Result<FirmwareKind, FirmwareError> {
        FirmwareImage::from_bytes(Path::new("fw.fd"), bytes).map(|i| i.kind())
    }

    #[test]
    fn recognises_a_pvh_elf() {
        // The real CloudHv entry, so the expectation is not self-referential.
        let kind =
            classify_bytes(pvh_elf(0x004f_ffd0, XEN_ELFNOTE_PHYS32_ENTRY, b"Xen\0")).unwrap();
        assert_eq!(kind, FirmwareKind::PvhElf { entry: 0x004f_ffd0 });
    }

    #[test]
    fn a_flash_blob_is_a_reset_vector_image() {
        // 4 MiB of erased flash with a jump at the reset vector: not an ELF.
        let mut fd = vec![0xffu8; 0x1000];
        fd[0xff0] = 0xe9;
        assert_eq!(classify_bytes(fd).unwrap(), FirmwareKind::ResetVector);
    }

    #[test]
    fn an_elf_without_the_pvh_note_is_rejected() {
        // Right note name, wrong type: an ordinary ELF, unbootable either way.
        let err = classify_bytes(pvh_elf(0x1000, 3, b"Xen\0")).unwrap_err();
        assert!(matches!(err, FirmwareError::MalformedElf(_)), "{err}");
        // Right type, foreign vendor.
        let err = classify_bytes(pvh_elf(0x1000, XEN_ELFNOTE_PHYS32_ENTRY, b"GNU\0")).unwrap_err();
        assert!(matches!(err, FirmwareError::MalformedElf(_)), "{err}");
    }

    /// Truncation must be a typed error at every step, never a panic: these
    /// are all prefixes of a valid PVH ELF.
    #[test]
    fn truncated_elves_error_instead_of_panicking() {
        let full = pvh_elf(0x004f_ffd0, XEN_ELFNOTE_PHYS32_ENTRY, b"Xen\0");
        for cut in 4..full.len() {
            let result = classify_bytes(full[..cut].to_vec());
            if let Ok(kind) = result {
                assert_eq!(
                    kind,
                    FirmwareKind::PvhElf { entry: 0x004f_ffd0 },
                    "prefix of {cut} bytes classified as {kind:?}"
                );
            }
        }
    }

    /// Absurd header fields must produce errors, not out-of-bounds reads. A
    /// huge `e_phnum` is *not* an error by itself — the scan stops at the first
    /// matching note, which is header 0 here — so the interesting case is a
    /// huge count with no note to find, and an out-of-range `e_phoff`.
    #[test]
    fn absurd_program_header_fields_are_rejected() {
        let mut v = pvh_elf(0x1000, XEN_ELFNOTE_PHYS32_ENTRY, b"Xen\0");
        v[0x20..0x28].copy_from_slice(&u64::MAX.to_le_bytes()); // e_phoff
        assert!(classify_bytes(v).is_err(), "e_phoff past the end");

        let mut v = pvh_elf(0x1000, XEN_ELFNOTE_PHYS32_ENTRY, b"Xen\0");
        v[0x36..0x38].copy_from_slice(&0u16.to_le_bytes()); // e_phentsize
        assert!(classify_bytes(v).is_err(), "zero e_phentsize");

        // A huge count and the note turned into something else: the walk must
        // run off the end of the image and stop with an error.
        let mut v = pvh_elf(0x1000, XEN_ELFNOTE_PHYS32_ENTRY, b"Xen\0");
        v[0x40..0x44].copy_from_slice(&1u32.to_le_bytes()); // PT_LOAD, not PT_NOTE
        v[0x38..0x3a].copy_from_slice(&u16::MAX.to_le_bytes());
        assert!(classify_bytes(v).is_err(), "e_phnum past the end");

        // Finding the note in header 0 short-circuits, huge count or not.
        let mut v = pvh_elf(0x004f_ffd0, XEN_ELFNOTE_PHYS32_ENTRY, b"Xen\0");
        v[0x38..0x3a].copy_from_slice(&u16::MAX.to_le_bytes());
        assert_eq!(
            classify_bytes(v).unwrap(),
            FirmwareKind::PvhElf { entry: 0x004f_ffd0 }
        );
    }

    #[test]
    fn empty_images_are_rejected() {
        assert!(matches!(
            FirmwareImage::from_bytes(Path::new("fw.fd"), Vec::new()),
            Err(FirmwareError::Empty { .. })
        ));
    }
}
