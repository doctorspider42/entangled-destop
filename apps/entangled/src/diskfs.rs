//! Post-install disk inspection (backlog MVP-1008/1009, UEFI-1804): find the
//! installed root partition in the RAW image and read its ext4 UUID, so the
//! generated VM profile can boot with `root=UUID=…`, and — for a UEFI install —
//! prove the disk really carries a GPT with an EFI System Partition.
//!
//! Two partitioning schemes, because the two installers produce different
//! disks: the Debian preseed path (`mode = "direct-linux"`) writes an MBR with a
//! type-0x83 root, and the Ubuntu autoinstall path (`mode = "uefi"`) writes a
//! GPT with an ESP plus a Linux filesystem. [`find_installed_root`] reads the
//! first, [`find_uefi_install`] the second, and each refuses the other's disk by
//! name rather than by looking corrupt; the two share nothing beyond the ext4
//! superblock read.
//!
//! # Everything here is untrusted input
//!
//! Both tables were written by the guest — by an installer we did not write,
//! running on media we only verified the *provenance* of, and in the adversarial
//! case by a guest that wants the host to index an array out of bounds. So:
//! every field is range-checked against the actual image length before it is
//! used, the GPT's two CRC-32s are verified (a table that fails them is refused
//! rather than "best effort" parsed), the entry array is bounded before it is
//! allocated, partitions may not overlap, and every failure is a typed error.
//! Nothing in this module panics, indexes with a guest value, or allocates a
//! guest-chosen size.

use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

use thiserror::Error;

pub const SECTOR: u64 = 512;
const MBR_SIGNATURE_OFFSET: usize = 510;
const PARTITION_TABLE_OFFSET: usize = 446;
const PARTITION_ENTRY_LEN: usize = 16;
const TYPE_LINUX: u8 = 0x83;
const TYPE_GPT_PROTECTIVE: u8 = 0xee;

// ext4 superblock lives 1024 bytes into the partition.
const EXT4_SUPERBLOCK_OFFSET: u64 = 1024;
const EXT4_MAGIC_OFFSET: usize = 0x38;
const EXT4_UUID_OFFSET: usize = 0x68;

#[derive(Debug, Error)]
pub enum DiskFsError {
    #[error("cannot read disk image: {0}")]
    Io(#[from] std::io::Error),

    #[error("no MBR boot signature (0x55AA) — the disk looks uninstalled")]
    NoMbr,

    #[error("the disk uses a GPT protective MBR; parse it with the GPT reader")]
    Gpt,

    #[error("no Linux (type 0x83) partition in the MBR; partition types found: {found:?}")]
    NoLinuxPartition { found: Vec<u8> },

    #[error("partition {index} claims sectors beyond the disk image")]
    PartitionOutOfRange { index: usize },

    #[error("no ext4 superblock at partition {index} (magic mismatch)")]
    NotExt4 { index: usize },

    // ---- GPT (UEFI-1804) -------------------------------------------------
    #[error("no GPT: expected the \"EFI PART\" signature at LBA 1")]
    NotGpt,

    #[error(
        "this disk has an ordinary MBR partition table, not a GPT — it looks like a          Debian (direct-linux) installation rather than a UEFI one"
    )]
    MbrNotGpt,

    #[error("GPT header revision {revision:#010x} is not 1.0")]
    GptRevision { revision: u32 },

    #[error("GPT header size {size} outside the legal 92..=512 bytes")]
    GptHeaderSize { size: u32 },

    #[error("GPT header CRC-32 mismatch: header says {stored:#010x}, computed {computed:#010x}")]
    GptHeaderCrc { stored: u32, computed: u32 },

    #[error(
        "GPT partition entry array CRC-32 mismatch: header says {stored:#010x}, \
         computed {computed:#010x}"
    )]
    GptEntriesCrc { stored: u32, computed: u32 },

    #[error(
        "GPT entry array is {entries} × {entry_size} bytes at LBA {lba} — refused \
         (bounded to {max_entries} entries of 128..={max_entry_size} bytes inside the image)"
    )]
    GptEntryArray {
        entries: u32,
        entry_size: u32,
        lba: u64,
        max_entries: u32,
        max_entry_size: u32,
    },

    #[error(
        "GPT partition {index} spans LBA {first}..={last}, outside the {sectors}-sector image"
    )]
    GptPartitionOutOfRange {
        index: usize,
        first: u64,
        last: u64,
        sectors: u64,
    },

    #[error(
        "GPT partitions {a} (LBA {a_first}..={a_last}) and {b} (LBA {b_first}..={b_last}) overlap"
    )]
    GptOverlap {
        a: usize,
        a_first: u64,
        a_last: u64,
        b: usize,
        b_first: u64,
        b_last: u64,
    },

    #[error("no EFI System Partition (type {esp}) in the GPT; {found} partition(s) present")]
    NoEsp { esp: Guid, found: usize },

    #[error("no Linux filesystem partition in the GPT; {found} partition(s) present")]
    NoLinuxGptPartition { found: usize },
}

/// One MBR partition entry we care about.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Partition {
    /// 1-based index within the primary table (matches /dev/vdaN).
    pub index: usize,
    pub type_byte: u8,
    pub start_lba: u32,
    pub sectors: u32,
}

/// Parses the primary MBR partition table from the first sector.
pub fn parse_mbr(sector0: &[u8; 512]) -> Result<Vec<Partition>, DiskFsError> {
    if sector0[MBR_SIGNATURE_OFFSET] != 0x55 || sector0[MBR_SIGNATURE_OFFSET + 1] != 0xaa {
        return Err(DiskFsError::NoMbr);
    }
    let mut parts = Vec::new();
    for i in 0..4 {
        let entry =
            &sector0[PARTITION_TABLE_OFFSET + i * PARTITION_ENTRY_LEN..][..PARTITION_ENTRY_LEN];
        let type_byte = entry[4];
        if type_byte == 0 {
            continue;
        }
        if type_byte == TYPE_GPT_PROTECTIVE {
            return Err(DiskFsError::Gpt);
        }
        parts.push(Partition {
            index: i + 1,
            type_byte,
            start_lba: u32::from_le_bytes([entry[8], entry[9], entry[10], entry[11]]),
            sectors: u32::from_le_bytes([entry[12], entry[13], entry[14], entry[15]]),
        });
    }
    Ok(parts)
}

/// Result of a successful post-install inspection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstalledRoot {
    /// 1-based partition number: the guest device is /dev/vda<number>.
    pub partition: usize,
    /// Lower-case hyphenated ext4 filesystem UUID.
    pub uuid: String,
}

/// Finds the first Linux partition with an ext4 filesystem and returns its
/// UUID. This is what decides whether an installation actually happened.
pub fn find_installed_root(disk: &Path) -> Result<InstalledRoot, DiskFsError> {
    let mut file = std::fs::File::open(disk)?;
    let disk_len = file.metadata()?.len();

    let mut sector0 = [0u8; 512];
    file.read_exact(&mut sector0)?;
    let partitions = parse_mbr(&sector0)?;

    let linux: Vec<&Partition> = partitions
        .iter()
        .filter(|p| p.type_byte == TYPE_LINUX)
        .collect();
    if linux.is_empty() {
        return Err(DiskFsError::NoLinuxPartition {
            found: partitions.iter().map(|p| p.type_byte).collect(),
        });
    }

    for part in linux {
        let start = u64::from(part.start_lba) * SECTOR;
        let end = start
            .checked_add(u64::from(part.sectors) * SECTOR)
            .ok_or(DiskFsError::PartitionOutOfRange { index: part.index })?;
        if end > disk_len || part.sectors == 0 {
            return Err(DiskFsError::PartitionOutOfRange { index: part.index });
        }
        if let Some(uuid) = ext4_uuid(&mut file, start)? {
            return Ok(InstalledRoot {
                partition: part.index,
                uuid,
            });
        }
        tracing::debug!(partition = part.index, "linux partition without ext4 magic");
    }
    Err(DiskFsError::NotExt4 {
        index: partitions
            .iter()
            .find(|p| p.type_byte == TYPE_LINUX)
            .map_or(0, |p| p.index),
    })
}

// ---------------------------------------------------------------------------
// GPT (UEFI-1804): what an Ubuntu autoinstall leaves behind
// ---------------------------------------------------------------------------

/// A GUID in the on-disk GPT encoding: the first three fields little-endian,
/// the last two big-endian ("mixed endian"). Stored as the raw 16 bytes so that
/// comparisons never depend on getting that mixture right twice.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Guid([u8; 16]);

impl Guid {
    /// The registry form: `C12A7328-F81F-11D2-BA4B-00A0C93EC93B`.
    pub const fn from_parts(a: u32, b: u16, c: u16, rest: [u8; 8]) -> Self {
        let a = a.to_le_bytes();
        let b = b.to_le_bytes();
        let c = c.to_le_bytes();
        Self([
            a[0], a[1], a[2], a[3], b[0], b[1], c[0], c[1], rest[0], rest[1], rest[2], rest[3],
            rest[4], rest[5], rest[6], rest[7],
        ])
    }

    fn from_bytes(bytes: &[u8]) -> Option<Self> {
        Some(Self(<[u8; 16]>::try_from(bytes).ok()?))
    }

    fn is_zero(&self) -> bool {
        self.0 == [0u8; 16]
    }
}

impl std::fmt::Display for Guid {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let b = &self.0;
        write!(
            f,
            "{:08X}-{:04X}-{:04X}-{:02X}{:02X}-{:02X}{:02X}{:02X}{:02X}{:02X}{:02X}",
            u32::from_le_bytes([b[0], b[1], b[2], b[3]]),
            u16::from_le_bytes([b[4], b[5]]),
            u16::from_le_bytes([b[6], b[7]]),
            b[8],
            b[9],
            b[10],
            b[11],
            b[12],
            b[13],
            b[14],
            b[15]
        )
    }
}

/// EFI System Partition — what `grub-install --target=x86_64-efi` writes into
/// and what the firmware's `Boot####` entry points at.
pub const ESP_TYPE: Guid = Guid::from_parts(
    0xc12a_7328,
    0xf81f,
    0x11d2,
    [0xba, 0x4b, 0x00, 0xa0, 0xc9, 0x3e, 0xc9, 0x3b],
);

/// "Linux filesystem data" — what subiquity's `direct` layout gives the root
/// filesystem.
pub const LINUX_FS_TYPE: Guid = Guid::from_parts(
    0x0fc6_3daf,
    0x8483,
    0x4772,
    [0x8e, 0x79, 0x3d, 0x69, 0xd8, 0x47, 0x7d, 0xe4],
);

/// "Linux root partition (x86-64)" from the Discoverable Partitions Spec. Not
/// what subiquity writes today, but a root filesystem either way — accepted so
/// a layout change upstream does not read as "no installation found".
pub const LINUX_ROOT_X64_TYPE: Guid = Guid::from_parts(
    0x4f68_bce3,
    0xe8cd,
    0x4db1,
    [0x96, 0xe7, 0xfb, 0xca, 0xf9, 0x84, 0xb7, 0x09],
);

const GPT_SIGNATURE: &[u8; 8] = b"EFI PART";
const GPT_REVISION_1_0: u32 = 0x0001_0000;
const GPT_HEADER_MIN: u32 = 92;
const GPT_HEADER_CRC_OFFSET: usize = 16;

/// Upper bounds on the entry array, applied *before* anything is allocated or
/// read. UEFI 2.10 §5.3 requires room for at least 16 KiB of entries (128 × 128
/// bytes); real disks use exactly that. 512 entries of at most 4 KiB is 2 MiB,
/// which is generous by two orders of magnitude and still a fixed ceiling — a
/// header claiming `0xffff_ffff` entries must not become a 512 GiB `Vec`.
const GPT_MAX_ENTRIES: u32 = 512;
const GPT_MAX_ENTRY_SIZE: u32 = 4096;
const GPT_MIN_ENTRY_SIZE: u32 = 128;

/// The GPT header fields this project uses. Field names follow UEFI 2.10 §5.3.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GptHeader {
    pub revision: u32,
    pub header_size: u32,
    pub my_lba: u64,
    pub alternate_lba: u64,
    pub first_usable_lba: u64,
    pub last_usable_lba: u64,
    pub disk_guid: Guid,
    pub partition_entry_lba: u64,
    pub number_of_partition_entries: u32,
    pub size_of_partition_entry: u32,
    pub partition_entry_array_crc32: u32,
}

/// One used GPT partition entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GptPartition {
    /// 1-based index in the entry array — the guest device is `/dev/vda<index>`.
    pub index: usize,
    pub type_guid: Guid,
    pub unique_guid: Guid,
    pub first_lba: u64,
    pub last_lba: u64,
    pub attributes: u64,
    /// The UTF-16 partition name, lossily decoded (installers write things like
    /// "EFI System Partition"; the field is guest data like any other).
    pub name: String,
}

impl GptPartition {
    /// Byte offset of the partition's first sector.
    pub fn offset(&self) -> u64 {
        self.first_lba.saturating_mul(SECTOR)
    }

    /// Partition size in sectors (`last_lba` is inclusive).
    pub fn sectors(&self) -> u64 {
        self.last_lba
            .saturating_sub(self.first_lba)
            .saturating_add(1)
    }
}

/// What a finished UEFI installation looks like from the host side.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UefiInstall {
    pub disk_guid: Guid,
    pub esp: GptPartition,
    pub root: GptPartition,
    /// ext4 UUID of the root filesystem, when it is ext4. `None` means the
    /// partition exists but carries something else (btrfs, LVM, ZFS) — which is
    /// not our business in `mode = "uefi"`: the firmware boots the ESP and GRUB
    /// finds its own root, so this is reporting, not configuration.
    pub root_uuid: Option<String>,
}

/// True when sector 0 is a GPT protective MBR (a single type-0xEE partition).
pub fn has_protective_mbr(sector0: &[u8; 512]) -> bool {
    if sector0[MBR_SIGNATURE_OFFSET] != 0x55 || sector0[MBR_SIGNATURE_OFFSET + 1] != 0xaa {
        return false;
    }
    (0..4).any(|i| {
        sector0[PARTITION_TABLE_OFFSET + i * PARTITION_ENTRY_LEN + 4] == TYPE_GPT_PROTECTIVE
    })
}

/// Reads the primary GPT of `disk` and finds the ESP and the root partition.
/// This is what decides whether a UEFI installation happened.
pub fn find_uefi_install(disk: &Path) -> Result<UefiInstall, DiskFsError> {
    let mut file = std::fs::File::open(disk)?;
    // Say which *kind* of disk this is before saying it has no GPT: pointing the
    // Ubuntu installer at a disk a Debian install already owns is a plausible
    // mistake, and "no GPT" alone reads like corruption.
    let mut sector0 = [0u8; 512];
    file.read_exact(&mut sector0)?;
    if !has_protective_mbr(&sector0) && parse_mbr(&sector0).is_ok_and(|p| !p.is_empty()) {
        return Err(DiskFsError::MbrNotGpt);
    }
    read_uefi_install(&mut file)
}

fn read_uefi_install<R: Read + Seek>(reader: &mut R) -> Result<UefiInstall, DiskFsError> {
    let image_len = reader.seek(SeekFrom::End(0))?;
    let (header, partitions) = read_gpt(reader, image_len)?;

    // The ESP: exactly the type GUID, because that is what the firmware looks
    // for too. A "boot" partition of any other type is not one.
    let esp = partitions
        .iter()
        .find(|p| p.type_guid == ESP_TYPE)
        .ok_or(DiskFsError::NoEsp {
            esp: ESP_TYPE,
            found: partitions.len(),
        })?
        .clone();

    // The root: the largest Linux filesystem. subiquity's `direct` layout makes
    // exactly one, but a layout with /boot would make two and the big one is
    // always the root.
    let root = partitions
        .iter()
        .filter(|p| p.type_guid == LINUX_FS_TYPE || p.type_guid == LINUX_ROOT_X64_TYPE)
        .max_by_key(|p| p.sectors())
        .ok_or(DiskFsError::NoLinuxGptPartition {
            found: partitions.len(),
        })?
        .clone();

    let root_uuid = ext4_uuid(reader, root.offset())?;
    Ok(UefiInstall {
        disk_guid: header.disk_guid,
        esp,
        root,
        root_uuid,
    })
}

/// Parses the primary GPT header at LBA 1 and its entry array.
fn read_gpt<R: Read + Seek>(
    reader: &mut R,
    image_len: u64,
) -> Result<(GptHeader, Vec<GptPartition>), DiskFsError> {
    let sectors = image_len / SECTOR;
    let mut lba1 = [0u8; 512];
    reader.seek(SeekFrom::Start(SECTOR))?;
    reader.read_exact(&mut lba1)?;
    let header = parse_gpt_header(&lba1)?;

    // Entry array bounds, checked before the read: everything below multiplies
    // guest numbers together, so they get their ceiling first.
    let entries = header.number_of_partition_entries;
    let entry_size = header.size_of_partition_entry;
    let array_bytes = u64::from(entries) * u64::from(entry_size);
    let array_start = header.partition_entry_lba.saturating_mul(SECTOR);
    let refuse = || DiskFsError::GptEntryArray {
        entries,
        entry_size,
        lba: header.partition_entry_lba,
        max_entries: GPT_MAX_ENTRIES,
        max_entry_size: GPT_MAX_ENTRY_SIZE,
    };
    if entries == 0
        || entries > GPT_MAX_ENTRIES
        || !(GPT_MIN_ENTRY_SIZE..=GPT_MAX_ENTRY_SIZE).contains(&entry_size)
        || entry_size % 8 != 0
        || header.partition_entry_lba < 2
        || array_start
            .checked_add(array_bytes)
            .is_none_or(|end| end > image_len)
    {
        return Err(refuse());
    }
    let array_len = usize::try_from(array_bytes).map_err(|_| refuse())?;

    let mut array = vec![0u8; array_len];
    reader.seek(SeekFrom::Start(array_start))?;
    reader.read_exact(&mut array)?;
    let computed = crc32(&array);
    if computed != header.partition_entry_array_crc32 {
        return Err(DiskFsError::GptEntriesCrc {
            stored: header.partition_entry_array_crc32,
            computed,
        });
    }

    let partitions = parse_gpt_entries(&array, entry_size as usize, sectors)?;
    Ok((header, partitions))
}

/// Parses and validates a GPT header from LBA 1.
pub fn parse_gpt_header(lba1: &[u8; 512]) -> Result<GptHeader, DiskFsError> {
    if &lba1[..8] != GPT_SIGNATURE {
        return Err(DiskFsError::NotGpt);
    }
    let revision = u32::from_le_bytes([lba1[8], lba1[9], lba1[10], lba1[11]]);
    if revision != GPT_REVISION_1_0 {
        return Err(DiskFsError::GptRevision { revision });
    }
    let header_size = le32(lba1, 12);
    if !(GPT_HEADER_MIN..=SECTOR as u32).contains(&header_size) {
        return Err(DiskFsError::GptHeaderSize { size: header_size });
    }
    // The header CRC covers `header_size` bytes with its own CRC field zeroed.
    let stored = le32(lba1, GPT_HEADER_CRC_OFFSET);
    let mut scratch = lba1[..header_size as usize].to_vec();
    scratch[GPT_HEADER_CRC_OFFSET..GPT_HEADER_CRC_OFFSET + 4].fill(0);
    let computed = crc32(&scratch);
    if computed != stored {
        return Err(DiskFsError::GptHeaderCrc { stored, computed });
    }
    Ok(GptHeader {
        revision,
        header_size,
        my_lba: le64(lba1, 24),
        alternate_lba: le64(lba1, 32),
        first_usable_lba: le64(lba1, 40),
        last_usable_lba: le64(lba1, 48),
        // `Guid::from_bytes` cannot fail on a 16-byte slice of a 512-byte array.
        disk_guid: Guid::from_bytes(&lba1[56..72]).unwrap_or(Guid([0; 16])),
        partition_entry_lba: le64(lba1, 72),
        number_of_partition_entries: le32(lba1, 80),
        size_of_partition_entry: le32(lba1, 84),
        partition_entry_array_crc32: le32(lba1, 88),
    })
}

/// Turns a CRC-verified entry array into the used partitions, rejecting any
/// entry that leaves the image or overlaps another.
fn parse_gpt_entries(
    array: &[u8],
    entry_size: usize,
    disk_sectors: u64,
) -> Result<Vec<GptPartition>, DiskFsError> {
    // Every field read below lives in the first 128 bytes of an entry. The
    // caller already bounds this; re-checked here so the function is safe to
    // call on its own (tests do).
    if entry_size < GPT_MIN_ENTRY_SIZE as usize {
        return Err(DiskFsError::GptEntryArray {
            entries: 0,
            entry_size: entry_size as u32,
            lba: 2,
            max_entries: GPT_MAX_ENTRIES,
            max_entry_size: GPT_MAX_ENTRY_SIZE,
        });
    }
    let mut partitions: Vec<GptPartition> = Vec::new();
    for (i, entry) in array.chunks_exact(entry_size).enumerate() {
        let Some(type_guid) = Guid::from_bytes(&entry[..16]) else {
            continue;
        };
        // An all-zero type GUID means "unused entry", and unused entries are
        // not required to be at the end of the array.
        if type_guid.is_zero() {
            continue;
        }
        let index = i + 1;
        let first_lba = le64(entry, 32);
        let last_lba = le64(entry, 40);
        // `last_lba` is inclusive, so an empty or inverted range is malformed;
        // so is anything the image cannot hold.
        if first_lba == 0 || last_lba < first_lba || last_lba >= disk_sectors {
            return Err(DiskFsError::GptPartitionOutOfRange {
                index,
                first: first_lba,
                last: last_lba,
                sectors: disk_sectors,
            });
        }
        for other in &partitions {
            if first_lba <= other.last_lba && other.first_lba <= last_lba {
                return Err(DiskFsError::GptOverlap {
                    a: other.index,
                    a_first: other.first_lba,
                    a_last: other.last_lba,
                    b: index,
                    b_first: first_lba,
                    b_last: last_lba,
                });
            }
        }
        partitions.push(GptPartition {
            index,
            type_guid,
            unique_guid: Guid::from_bytes(&entry[16..32]).unwrap_or(Guid([0; 16])),
            first_lba,
            last_lba,
            attributes: le64(entry, 48),
            name: utf16_name(&entry[56..entry_size.min(56 + 72)]),
        });
    }
    Ok(partitions)
}

/// Decodes a GPT partition name: up to 36 UTF-16LE code units, NUL-terminated,
/// lossily (it is guest data, and an unpaired surrogate must not be fatal).
fn utf16_name(bytes: &[u8]) -> String {
    let units: Vec<u16> = bytes
        .chunks_exact(2)
        .map(|c| u16::from_le_bytes([c[0], c[1]]))
        .take_while(|&u| u != 0)
        .collect();
    String::from_utf16_lossy(&units)
}

fn le32(bytes: &[u8], at: usize) -> u32 {
    let mut v = [0u8; 4];
    v.copy_from_slice(&bytes[at..at + 4]);
    u32::from_le_bytes(v)
}

fn le64(bytes: &[u8], at: usize) -> u64 {
    let mut v = [0u8; 8];
    v.copy_from_slice(&bytes[at..at + 8]);
    u64::from_le_bytes(v)
}

/// CRC-32 as GPT (and Ethernet, and gzip) define it: reflected polynomial
/// `0xEDB88320`, initial value all-ones, final complement.
///
/// Hand-rolled rather than pulled in as a dependency: it is nine lines, this
/// module must build on every host (the compression crate that could provide one
/// is Linux-only here), and a checksum used to *reject* untrusted input is worth
/// having a local test for.
fn crc32(data: &[u8]) -> u32 {
    let mut crc = 0xffff_ffffu32;
    for &byte in data {
        crc ^= u32::from(byte);
        for _ in 0..8 {
            // Branch-free reflected update: mask is all-ones when bit 0 is set.
            let mask = (crc & 1).wrapping_neg();
            crc = (crc >> 1) ^ (0xedb8_8320 & mask);
        }
    }
    !crc
}

/// Reads the ext4 UUID of the filesystem starting at `partition_offset`, or
/// None when there is no ext4 magic there.
fn ext4_uuid<R: Read + Seek>(
    reader: &mut R,
    partition_offset: u64,
) -> Result<Option<String>, DiskFsError> {
    let Some(sb_offset) = partition_offset.checked_add(EXT4_SUPERBLOCK_OFFSET) else {
        return Ok(None);
    };
    let mut sb = [0u8; 1024];
    reader.seek(SeekFrom::Start(sb_offset))?;
    if reader.read_exact(&mut sb).is_err() {
        return Ok(None); // truncated image: not installed
    }
    if sb[EXT4_MAGIC_OFFSET] != 0x53 || sb[EXT4_MAGIC_OFFSET + 1] != 0xef {
        return Ok(None);
    }
    let u = &sb[EXT4_UUID_OFFSET..EXT4_UUID_OFFSET + 16];
    Ok(Some(format!(
        "{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        u[0], u[1], u[2], u[3], u[4], u[5], u[6], u[7], u[8], u[9], u[10], u[11], u[12], u[13],
        u[14], u[15]
    )))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn mbr_with(entries: &[(usize, u8, u32, u32)]) -> [u8; 512] {
        let mut s = [0u8; 512];
        s[510] = 0x55;
        s[511] = 0xaa;
        for &(slot, type_byte, start, sectors) in entries {
            let base = PARTITION_TABLE_OFFSET + slot * PARTITION_ENTRY_LEN;
            s[base + 4] = type_byte;
            s[base + 8..base + 12].copy_from_slice(&start.to_le_bytes());
            s[base + 12..base + 16].copy_from_slice(&sectors.to_le_bytes());
        }
        s
    }

    #[test]
    fn parses_a_typical_debian_layout() {
        // p1 = linux root, p2 = extended (0x05) holding swap.
        let mbr = mbr_with(&[(0, 0x83, 2048, 60000), (1, 0x05, 62048, 4096)]);
        let parts = parse_mbr(&mbr).unwrap();
        assert_eq!(parts.len(), 2);
        assert_eq!(parts[0].index, 1);
        assert_eq!(parts[0].type_byte, 0x83);
        assert_eq!(parts[0].start_lba, 2048);
    }

    #[test]
    fn rejects_blank_garbage_and_gpt() {
        assert!(matches!(parse_mbr(&[0u8; 512]), Err(DiskFsError::NoMbr)));
        let gpt = mbr_with(&[(0, TYPE_GPT_PROTECTIVE, 1, 100)]);
        assert!(matches!(parse_mbr(&gpt), Err(DiskFsError::Gpt)));
    }

    fn temp_disk(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join("entangled-diskfs-tests");
        std::fs::create_dir_all(&dir).unwrap();
        dir.join(format!("{name}-{}.raw", std::process::id()))
    }

    #[test]
    fn finds_the_installed_root_uuid() {
        let path = temp_disk("installed");
        let mut f = std::fs::File::create(&path).unwrap();
        let mbr = mbr_with(&[(0, 0x83, 4, 64)]);
        f.write_all(&mbr).unwrap();
        // ext4 superblock at partition start (LBA 4 => byte 2048) + 1024.
        let sb_at = 4 * 512 + 1024;
        f.set_len(4 * 512 + 64 * 512).unwrap();
        f.seek(SeekFrom::Start(sb_at + EXT4_MAGIC_OFFSET as u64))
            .unwrap();
        f.write_all(&[0x53, 0xef]).unwrap();
        f.seek(SeekFrom::Start(sb_at + EXT4_UUID_OFFSET as u64))
            .unwrap();
        f.write_all(&[
            0xde, 0xad, 0xbe, 0xef, 0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99,
            0xaa, 0xbb,
        ])
        .unwrap();
        drop(f);

        let root = find_installed_root(&path).unwrap();
        assert_eq!(root.partition, 1);
        assert_eq!(root.uuid, "deadbeef-0011-2233-4455-66778899aabb");
        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn uninstalled_disk_is_a_readable_error() {
        let path = temp_disk("blank");
        std::fs::File::create(&path)
            .unwrap()
            .set_len(1 << 20)
            .unwrap();
        assert!(matches!(
            find_installed_root(&path),
            Err(DiskFsError::NoMbr)
        ));
        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn linux_partition_without_ext4_is_rejected() {
        let path = temp_disk("noext4");
        let mut f = std::fs::File::create(&path).unwrap();
        f.write_all(&mbr_with(&[(0, 0x83, 4, 16)])).unwrap();
        f.set_len(4 * 512 + 16 * 512).unwrap();
        drop(f);
        assert!(matches!(
            find_installed_root(&path),
            Err(DiskFsError::NotExt4 { index: 1 })
        ));
        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn partition_past_the_image_end_is_rejected() {
        let path = temp_disk("oob");
        let mut f = std::fs::File::create(&path).unwrap();
        f.write_all(&mbr_with(&[(0, 0x83, 4, u32::MAX)])).unwrap();
        f.set_len(1 << 20).unwrap();
        drop(f);
        assert!(matches!(
            find_installed_root(&path),
            Err(DiskFsError::PartitionOutOfRange { index: 1 })
        ));
        std::fs::remove_file(&path).unwrap();
    }

    // ---- GPT (UEFI-1804) --------------------------------------------------

    /// The one CRC-32 value in this file that is not computed by the code under
    /// test: `crc32(b"123456789")` is the standard check value for CRC-32/ISO-HDLC.
    #[test]
    fn crc32_matches_the_published_check_value() {
        assert_eq!(crc32(b"123456789"), 0xcbf4_3926);
        assert_eq!(crc32(&[]), 0);
    }

    #[test]
    fn guid_renders_in_registry_form() {
        assert_eq!(ESP_TYPE.to_string(), "C12A7328-F81F-11D2-BA4B-00A0C93EC93B");
        assert_eq!(
            LINUX_FS_TYPE.to_string(),
            "0FC63DAF-8483-4772-8E79-3D69D8477DE4"
        );
        // Registry form is the mixed-endian on-disk order read back: the ESP
        // GUID's first byte on disk is 0x28, not 0xc1.
        assert_eq!(ESP_TYPE.0[0], 0x28);
        assert!(Guid([0; 16]).is_zero());
    }

    /// A GPT image built the way an installer builds one: protective MBR,
    /// header at LBA 1, 128 entries of 128 bytes at LBA 2, both CRCs correct.
    struct GptBuilder {
        sectors: u64,
        entries: Vec<(Guid, u64, u64, &'static str)>,
        entry_count: u32,
        entry_size: u32,
        entry_lba: u64,
        break_header_crc: bool,
        break_entries_crc: bool,
    }

    impl GptBuilder {
        fn new(sectors: u64) -> Self {
            Self {
                sectors,
                entries: Vec::new(),
                entry_count: 128,
                entry_size: 128,
                entry_lba: 2,
                break_header_crc: false,
                break_entries_crc: false,
            }
        }

        fn part(mut self, type_guid: Guid, first: u64, last: u64, name: &'static str) -> Self {
            self.entries.push((type_guid, first, last, name));
            self
        }

        /// A finished Ubuntu-style layout: 1 MiB ESP then the rest as root.
        fn ubuntu_layout(self) -> Self {
            let last = self.sectors - 34;
            self.part(ESP_TYPE, 2048, 4095, "EFI System Partition")
                .part(LINUX_FS_TYPE, 4096, last, "")
        }

        fn image(&self) -> Vec<u8> {
            let mut image = vec![0u8; (self.sectors * SECTOR) as usize];
            // Protective MBR.
            image[MBR_SIGNATURE_OFFSET] = 0x55;
            image[MBR_SIGNATURE_OFFSET + 1] = 0xaa;
            image[PARTITION_TABLE_OFFSET + 4] = TYPE_GPT_PROTECTIVE;

            // Entry array. The builder's own arithmetic is saturating because
            // the adversarial cases below set these fields to values whose
            // product does not fit in 32 bits — the *parser* must refuse them,
            // so the fixture must be able to express them.
            let array_len = usize::try_from(
                u64::from(self.entry_count)
                    .saturating_mul(u64::from(self.entry_size))
                    .min(1 << 20),
            )
            .unwrap_or(1 << 20);
            let mut array = vec![0u8; array_len];
            for (i, (type_guid, first, last, name)) in self.entries.iter().enumerate() {
                let at = i * self.entry_size as usize;
                // A deliberately absurd entry_size can push an entry out of the
                // clamped fixture; the parser refuses those shapes long before
                // it reads an entry, so there is nothing to plant.
                if at + 128 > array.len() {
                    break;
                }
                array[at..at + 16].copy_from_slice(&type_guid.0);
                // Unique GUID: any non-zero value.
                array[at + 16..at + 32].copy_from_slice(&[(i as u8) + 1; 16]);
                array[at + 32..at + 40].copy_from_slice(&first.to_le_bytes());
                array[at + 40..at + 48].copy_from_slice(&last.to_le_bytes());
                for (u, unit) in name.encode_utf16().enumerate().take(36) {
                    let n = at + 56 + u * 2;
                    array[n..n + 2].copy_from_slice(&unit.to_le_bytes());
                }
            }
            let mut entries_crc = crc32(&array);
            if self.break_entries_crc {
                entries_crc ^= 0xffff_ffff;
            }
            let array_at =
                usize::try_from(self.entry_lba.saturating_mul(SECTOR)).unwrap_or(usize::MAX);
            if array_at.saturating_add(array_len) <= image.len() {
                image[array_at..array_at + array_len].copy_from_slice(&array);
            }

            // Header.
            let mut header = vec![0u8; 92];
            header[..8].copy_from_slice(GPT_SIGNATURE);
            header[8..12].copy_from_slice(&GPT_REVISION_1_0.to_le_bytes());
            header[12..16].copy_from_slice(&92u32.to_le_bytes());
            header[24..32].copy_from_slice(&1u64.to_le_bytes()); // MyLBA
            header[32..40].copy_from_slice(&(self.sectors - 1).to_le_bytes()); // AlternateLBA
            header[40..48].copy_from_slice(&34u64.to_le_bytes()); // FirstUsableLBA
            header[48..56].copy_from_slice(&(self.sectors - 34).to_le_bytes());
            header[56..72].copy_from_slice(&[0xab; 16]); // DiskGUID
            header[72..80].copy_from_slice(&self.entry_lba.to_le_bytes());
            header[80..84].copy_from_slice(&self.entry_count.to_le_bytes());
            header[84..88].copy_from_slice(&self.entry_size.to_le_bytes());
            header[88..92].copy_from_slice(&entries_crc.to_le_bytes());
            let mut header_crc = crc32(&header);
            if self.break_header_crc {
                header_crc ^= 0xffff_ffff;
            }
            header[16..20].copy_from_slice(&header_crc.to_le_bytes());
            image[SECTOR as usize..SECTOR as usize + header.len()].copy_from_slice(&header);
            image
        }

        /// The image with an ext4 superblock planted at the start of the
        /// partition whose 1-based index is `index`.
        fn with_ext4(self, index: usize, uuid: [u8; 16]) -> Vec<u8> {
            let mut image = self.image();
            let (_, first, _, _) = self.entries[index - 1];
            let sb = (first * SECTOR + EXT4_SUPERBLOCK_OFFSET) as usize;
            image[sb + EXT4_MAGIC_OFFSET] = 0x53;
            image[sb + EXT4_MAGIC_OFFSET + 1] = 0xef;
            image[sb + EXT4_UUID_OFFSET..sb + EXT4_UUID_OFFSET + 16].copy_from_slice(&uuid);
            image
        }
    }

    fn inspect_bytes(bytes: &[u8]) -> Result<UefiInstall, DiskFsError> {
        let mut cursor = std::io::Cursor::new(bytes.to_vec());
        read_uefi_install(&mut cursor)
    }

    /// 16 MiB is enough image for a 1 MiB ESP and a root, and small enough to
    /// build in memory for every case below.
    const TEST_SECTORS: u64 = 32768;

    #[test]
    fn finds_the_esp_and_the_root_of_an_ubuntu_layout() {
        let uuid = [
            0x12, 0x34, 0x56, 0x78, 0x9a, 0xbc, 0xde, 0xf0, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66,
            0x77, 0x88,
        ];
        let image = GptBuilder::new(TEST_SECTORS)
            .ubuntu_layout()
            .with_ext4(2, uuid);
        let install = inspect_bytes(&image).expect("a finished UEFI installation");

        assert_eq!(install.esp.index, 1);
        assert_eq!(install.esp.type_guid, ESP_TYPE);
        assert_eq!(install.esp.name, "EFI System Partition");
        assert_eq!(install.esp.first_lba, 2048);
        assert_eq!(install.esp.sectors(), 2048);
        assert_eq!(install.esp.offset(), 2048 * SECTOR);
        assert_eq!(install.root.index, 2);
        assert_eq!(
            install.root_uuid.as_deref(),
            Some("12345678-9abc-def0-1122-334455667788")
        );
        assert_eq!(install.disk_guid.to_string().len(), 36);
    }

    /// Each reader must refuse the other's disk, and say which kind it found —
    /// pointing `install ubuntu` at a disk a Debian install owns (or the
    /// reverse) is a plausible mistake, and "no GPT"/"no MBR" alone reads like
    /// corruption.
    #[test]
    fn the_two_readers_refuse_each_others_disks() {
        let dir = std::env::temp_dir().join("entangled-diskfs-tests");
        std::fs::create_dir_all(&dir).unwrap();

        let gpt_path = dir.join(format!("gpt-{}.raw", std::process::id()));
        std::fs::write(
            &gpt_path,
            GptBuilder::new(TEST_SECTORS)
                .ubuntu_layout()
                .with_ext4(2, [0x5a; 16]),
        )
        .unwrap();
        assert!(find_uefi_install(&gpt_path).is_ok());
        // The MBR reader sees the protective MBR and stops.
        assert!(matches!(
            find_installed_root(&gpt_path),
            Err(DiskFsError::Gpt)
        ));
        let mut sector0 = [0u8; 512];
        sector0.copy_from_slice(&std::fs::read(&gpt_path).unwrap()[..512]);
        assert!(has_protective_mbr(&sector0));
        std::fs::remove_file(&gpt_path).unwrap();

        // And the Debian shape: the MBR reader works, the GPT reader names it.
        let mbr_path = temp_disk("dispatch-mbr");
        let mut f = std::fs::File::create(&mbr_path).unwrap();
        f.write_all(&mbr_with(&[(0, 0x83, 4, 64)])).unwrap();
        f.set_len(4 * 512 + 64 * 512).unwrap();
        let sb_at = 4 * 512 + 1024;
        f.seek(SeekFrom::Start(sb_at + EXT4_MAGIC_OFFSET as u64))
            .unwrap();
        f.write_all(&[0x53, 0xef]).unwrap();
        drop(f);
        assert!(find_installed_root(&mbr_path).is_ok());
        assert!(matches!(
            find_uefi_install(&mbr_path),
            Err(DiskFsError::MbrNotGpt)
        ));
        std::fs::remove_file(&mbr_path).unwrap();

        // A blank disk is neither, and both say so in their own words.
        let blank = temp_disk("dispatch-blank");
        std::fs::File::create(&blank)
            .unwrap()
            .set_len(1 << 20)
            .unwrap();
        assert!(matches!(
            find_installed_root(&blank),
            Err(DiskFsError::NoMbr)
        ));
        assert!(matches!(
            find_uefi_install(&blank),
            Err(DiskFsError::NotGpt)
        ));
        std::fs::remove_file(&blank).unwrap();
    }

    /// A root partition that is not ext4 is reported, not refused: in uefi mode
    /// the firmware boots the ESP and GRUB finds its own root.
    #[test]
    fn a_non_ext4_root_still_reports_the_partitions() {
        let image = GptBuilder::new(TEST_SECTORS).ubuntu_layout().image();
        let install = inspect_bytes(&image).unwrap();
        assert_eq!(install.root.type_guid, LINUX_FS_TYPE);
        assert_eq!(install.root_uuid, None);
    }

    /// The largest Linux partition wins, so a layout with a separate /boot does
    /// not name the small one as the root.
    #[test]
    fn the_largest_linux_partition_is_the_root() {
        let image = GptBuilder::new(TEST_SECTORS)
            .part(ESP_TYPE, 2048, 4095, "esp")
            .part(LINUX_FS_TYPE, 4096, 6143, "boot")
            .part(LINUX_ROOT_X64_TYPE, 6144, TEST_SECTORS - 34, "root")
            .image();
        let install = inspect_bytes(&image).unwrap();
        assert_eq!(install.root.index, 3);
        assert_eq!(install.root.name, "root");
    }

    /// An empty (or freshly created) disk must say "not installed", never parse
    /// as a table.
    #[test]
    fn a_blank_image_is_not_a_gpt() {
        let blank = vec![0u8; (TEST_SECTORS * SECTOR) as usize];
        assert!(matches!(inspect_bytes(&blank), Err(DiskFsError::NotGpt)));
        // And an image too short to hold LBA 1 at all is an IO error, not a panic.
        assert!(matches!(
            inspect_bytes(&[0u8; 600]),
            Err(DiskFsError::Io(_))
        ));
    }

    /// Adversarial: the guest wrote the table, so a corrupted or hostile one
    /// must produce a typed error. Each case below is a single field changed on
    /// an otherwise valid image.
    #[test]
    fn a_corrupted_header_is_refused() {
        let base = GptBuilder::new(TEST_SECTORS).ubuntu_layout();

        // Signature.
        let mut image = base.image();
        image[SECTOR as usize] = b'X';
        assert!(matches!(inspect_bytes(&image), Err(DiskFsError::NotGpt)));

        // Header CRC.
        let mut broken = GptBuilder::new(TEST_SECTORS).ubuntu_layout();
        broken.break_header_crc = true;
        assert!(matches!(
            inspect_bytes(&broken.image()),
            Err(DiskFsError::GptHeaderCrc { .. })
        ));

        // Entry array CRC — the header is fine, so this is the second checksum.
        let mut broken = GptBuilder::new(TEST_SECTORS).ubuntu_layout();
        broken.break_entries_crc = true;
        assert!(matches!(
            inspect_bytes(&broken.image()),
            Err(DiskFsError::GptEntriesCrc { .. })
        ));

        // Revision, and header size: both are checked before the CRC, so the
        // CRC is recomputed over the edited header to reach them.
        for (offset, value, want_revision) in [(8usize, 0x0002_0000u32, true), (12, 0xffff, false)]
        {
            let mut image = base.image();
            let at = SECTOR as usize;
            image[at + offset..at + offset + 4].copy_from_slice(&value.to_le_bytes());
            let error = inspect_bytes(&image).expect_err("must be refused");
            if want_revision {
                assert!(matches!(error, DiskFsError::GptRevision { .. }), "{error}");
            } else {
                assert!(
                    matches!(error, DiskFsError::GptHeaderSize { .. }),
                    "{error}"
                );
            }
        }
    }

    /// Adversarial: the entry array's shape is three guest numbers multiplied
    /// together. Every one of them gets a ceiling before anything is allocated.
    #[test]
    fn an_absurd_entry_array_is_refused_before_it_is_allocated() {
        for mutate in [
            // 2^32-1 entries × 128 bytes: a 512 GiB allocation if believed.
            (|b: &mut GptBuilder| b.entry_count = u32::MAX) as fn(&mut GptBuilder),
            |b: &mut GptBuilder| b.entry_count = 0,
            |b: &mut GptBuilder| b.entry_size = 0,
            |b: &mut GptBuilder| b.entry_size = 64, // below the 128-byte minimum
            |b: &mut GptBuilder| b.entry_size = 130, // not a multiple of 8
            |b: &mut GptBuilder| b.entry_size = 1 << 20,
            // The array itself parked outside the image, and inside the header.
            |b: &mut GptBuilder| b.entry_lba = u64::MAX / SECTOR,
            |b: &mut GptBuilder| b.entry_lba = 1,
            |b: &mut GptBuilder| b.entry_count = GPT_MAX_ENTRIES + 1,
        ] {
            let mut builder = GptBuilder::new(TEST_SECTORS).ubuntu_layout();
            mutate(&mut builder);
            let error = inspect_bytes(&builder.image()).expect_err("must be refused");
            assert!(
                matches!(error, DiskFsError::GptEntryArray { .. }),
                "expected an entry-array refusal, got {error}"
            );
        }
    }

    /// Adversarial: partition ranges. A partition that leaves the image would
    /// make `offset()` point past the file, and overlapping partitions mean the
    /// table is not describing a real disk — refuse both rather than pick one.
    #[test]
    fn out_of_range_and_overlapping_partitions_are_refused() {
        let cases: [(GptBuilder, bool); 5] = [
            // last_lba past the end of the image.
            (
                GptBuilder::new(TEST_SECTORS)
                    .part(ESP_TYPE, 2048, 4095, "esp")
                    .part(LINUX_FS_TYPE, 4096, u64::MAX, "root"),
                false,
            ),
            // Inverted range.
            (
                GptBuilder::new(TEST_SECTORS)
                    .part(ESP_TYPE, 2048, 4095, "esp")
                    .part(LINUX_FS_TYPE, 9000, 8000, "root"),
                false,
            ),
            // first_lba 0 would overlap the protective MBR and the GPT itself.
            (
                GptBuilder::new(TEST_SECTORS)
                    .part(ESP_TYPE, 0, 4095, "esp")
                    .part(LINUX_FS_TYPE, 4096, 8000, "root"),
                false,
            ),
            // The root swallowing the ESP.
            (
                GptBuilder::new(TEST_SECTORS)
                    .part(ESP_TYPE, 2048, 4095, "esp")
                    .part(LINUX_FS_TYPE, 2048, 9000, "root"),
                true,
            ),
            // One sector of overlap at the boundary is still an overlap.
            (
                GptBuilder::new(TEST_SECTORS)
                    .part(ESP_TYPE, 2048, 4096, "esp")
                    .part(LINUX_FS_TYPE, 4096, 9000, "root"),
                true,
            ),
        ];
        for (builder, expect_overlap) in cases {
            let error = inspect_bytes(&builder.image()).expect_err("must be refused");
            if expect_overlap {
                assert!(matches!(error, DiskFsError::GptOverlap { .. }), "{error}");
            } else {
                assert!(
                    matches!(error, DiskFsError::GptPartitionOutOfRange { .. }),
                    "{error}"
                );
            }
        }
    }

    /// A partitioned disk that is not an installation: no ESP, or no root. Both
    /// are what `entangled install` sees when the installer died early, so the
    /// message has to say which half is missing.
    #[test]
    fn a_gpt_without_an_esp_or_without_a_root_is_not_an_installation() {
        let no_esp = GptBuilder::new(TEST_SECTORS)
            .part(LINUX_FS_TYPE, 2048, 9000, "root")
            .image();
        assert!(matches!(
            inspect_bytes(&no_esp),
            Err(DiskFsError::NoEsp { found: 1, .. })
        ));

        let no_root = GptBuilder::new(TEST_SECTORS)
            .part(ESP_TYPE, 2048, 4095, "esp")
            .image();
        assert!(matches!(
            inspect_bytes(&no_root),
            Err(DiskFsError::NoLinuxGptPartition { found: 1 })
        ));

        // A GPT with a valid header and *no* used entries: the CRCs pass, the
        // partition list is empty, and that is "not installed", not a parse
        // failure.
        let empty = GptBuilder::new(TEST_SECTORS).image();
        assert!(matches!(
            inspect_bytes(&empty),
            Err(DiskFsError::NoEsp { found: 0, .. })
        ));
    }

    /// Used entries do not have to be dense: an installer that reuses entry 3
    /// after deleting 1 and 2 still describes a valid disk, and the reported
    /// index has to be the entry index (`/dev/vda3`), not a count.
    #[test]
    fn sparse_entry_arrays_keep_the_partition_numbers() {
        let mut builder = GptBuilder::new(TEST_SECTORS);
        // Entries 1 and 2 unused: an all-zero type GUID.
        builder.entries.push((Guid([0; 16]), 0, 0, ""));
        builder.entries.push((Guid([0; 16]), 0, 0, ""));
        let image = builder
            .part(ESP_TYPE, 2048, 4095, "esp")
            .part(LINUX_FS_TYPE, 4096, 9000, "root")
            .image();
        let install = inspect_bytes(&image).unwrap();
        assert_eq!(install.esp.index, 3);
        assert_eq!(install.root.index, 4);
    }

    /// Partition names are guest-supplied UTF-16: a non-ASCII name must decode,
    /// an unterminated one must stop at 36 code units, and a lone surrogate must
    /// not be fatal.
    #[test]
    fn partition_names_are_decoded_defensively() {
        assert_eq!(utf16_name(&[]), "");
        let mut bytes = Vec::new();
        for unit in "boot ✓".encode_utf16() {
            bytes.extend_from_slice(&unit.to_le_bytes());
        }
        assert_eq!(utf16_name(&bytes), "boot ✓");
        // Unpaired high surrogate, then a NUL: lossy, and it terminates.
        assert_eq!(
            utf16_name(&[0x00, 0xd8, 0x00, 0x00, 0x41, 0x00])
                .chars()
                .count(),
            1
        );
        // A trailing odd byte is ignored rather than read past.
        assert_eq!(utf16_name(&[0x41, 0x00, 0x42]), "A");
    }
}
