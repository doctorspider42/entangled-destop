//! Post-install disk inspection (backlog MVP-1008/1009): find the installed
//! root partition in the RAW image and read its ext4 UUID, so the generated
//! VM profile can boot with `root=UUID=…`.
//!
//! The disk content is written by the guest and therefore untrusted: every
//! read is bounds-checked, every failure is a typed error, nothing panics.

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

    #[error("the disk uses a GPT protective MBR; GPT parsing is not implemented yet")]
    Gpt,

    #[error("no Linux (type 0x83) partition in the MBR; partition types found: {found:?}")]
    NoLinuxPartition { found: Vec<u8> },

    #[error("partition {index} claims sectors beyond the disk image")]
    PartitionOutOfRange { index: usize },

    #[error("no ext4 superblock at partition {index} (magic mismatch)")]
    NotExt4 { index: usize },
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
}
