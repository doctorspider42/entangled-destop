//! Synthetic disk images for tests: MBR sectors, a GPT builder that produces
//! installer-shaped (and deliberately hostile) images, and temp-file helpers.
//! Test-only; shared by the `layout`, `inspect`, `refs` and `relocate` tests.

use std::path::PathBuf;

use crate::layout::{
    crc32, Guid, EXT4_MAGIC_OFFSET, EXT4_SUPERBLOCK_OFFSET, EXT4_UUID_OFFSET, GPT_REVISION_1_0,
    GPT_SIGNATURE, MBR_SIGNATURE_OFFSET, PARTITION_ENTRY_LEN, PARTITION_TABLE_OFFSET, SECTOR,
    TYPE_GPT_PROTECTIVE,
};

/// A primary MBR sector with the given `(slot, type_byte, start_lba, sectors)`
/// entries.
pub(crate) fn mbr_with(entries: &[(usize, u8, u32, u32)]) -> [u8; 512] {
    let mut s = [0u8; 512];
    s[MBR_SIGNATURE_OFFSET] = 0x55;
    s[MBR_SIGNATURE_OFFSET + 1] = 0xaa;
    for &(slot, type_byte, start, sectors) in entries {
        let base = PARTITION_TABLE_OFFSET + slot * PARTITION_ENTRY_LEN;
        s[base + 4] = type_byte;
        s[base + 8..base + 12].copy_from_slice(&start.to_le_bytes());
        s[base + 12..base + 16].copy_from_slice(&sectors.to_le_bytes());
    }
    s
}

/// A per-process temp path for on-disk fixtures.
pub(crate) fn temp_disk(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join("entangled-disk-image-tests");
    std::fs::create_dir_all(&dir).expect("test temp dir");
    dir.join(format!("{tag}-{}.raw", std::process::id()))
}

/// A per-process temp *directory* for fixtures that need several files.
pub(crate) fn temp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "entangled-disk-image-tests/{tag}-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("test temp dir");
    dir
}

/// A GPT image built the way an installer builds one: protective MBR,
/// header at LBA 1, 128 entries of 128 bytes at LBA 2, both CRCs correct.
pub(crate) struct GptBuilder {
    pub sectors: u64,
    pub entries: Vec<(Guid, u64, u64, &'static str)>,
    pub entry_count: u32,
    pub entry_size: u32,
    pub entry_lba: u64,
    pub break_header_crc: bool,
    pub break_entries_crc: bool,
}

impl GptBuilder {
    pub fn new(sectors: u64) -> Self {
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

    pub fn part(mut self, type_guid: Guid, first: u64, last: u64, name: &'static str) -> Self {
        self.entries.push((type_guid, first, last, name));
        self
    }

    /// A finished Ubuntu-style layout: 1 MiB ESP then the rest as root.
    pub fn ubuntu_layout(self) -> Self {
        let last = self.sectors - 34;
        self.part(crate::layout::ESP_TYPE, 2048, 4095, "EFI System Partition")
            .part(crate::layout::LINUX_FS_TYPE, 4096, last, "")
    }

    pub fn image(&self) -> Vec<u8> {
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
        let array_at = usize::try_from(self.entry_lba.saturating_mul(SECTOR)).unwrap_or(usize::MAX);
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
    pub fn with_ext4(self, index: usize, uuid: [u8; 16]) -> Vec<u8> {
        let mut image = self.image();
        let (_, first, _, _) = self.entries[index - 1];
        let sb = (first * SECTOR + EXT4_SUPERBLOCK_OFFSET) as usize;
        image[sb + EXT4_MAGIC_OFFSET] = 0x53;
        image[sb + EXT4_MAGIC_OFFSET + 1] = 0xef;
        image[sb + EXT4_UUID_OFFSET..sb + EXT4_UUID_OFFSET + 16].copy_from_slice(&uuid);
        image
    }
}
