//! One combined report per disk image — sizes, partition table, filesystem
//! probes, the `.nvram` sidecar — for `entangled disk inspect` (human and
//! `--json` output) and the manager's Disks view (which links this function
//! instead of shelling out).

use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::layout::{self, DiskFsError, PartitionTable, SECTOR};
use crate::ops;

/// Which table the disk carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum TableKind {
    /// A blank image: no MBR signature, no GPT.
    None,
    Mbr,
    Gpt,
}

impl std::fmt::Display for TableKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TableKind::None => f.write_str("none"),
            TableKind::Mbr => f.write_str("MBR"),
            TableKind::Gpt => f.write_str("GPT"),
        }
    }
}

/// One partition, table-agnostic.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PartitionReport {
    /// 1-based: the guest device is `/dev/vda<index>`.
    pub index: usize,
    /// Human type name ("Linux", "EFI System", …) — reporting, not policy.
    pub type_name: String,
    /// The raw type: `0x83` for MBR, the registry-form GUID for GPT.
    pub type_id: String,
    /// GPT partition label, when present (guest data, decoded lossily).
    pub name: Option<String>,
    pub first_lba: u64,
    pub sectors: u64,
    pub size_bytes: u64,
    /// ext4 filesystem UUID, when the partition carries an ext4 superblock.
    pub ext4_uuid: Option<String>,
    /// ext4 volume label, when set.
    pub ext4_label: Option<String>,
}

/// Everything `disk inspect` reports about one image.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DiskReport {
    pub path: PathBuf,
    /// The image's nominal size (`metadata.len()`).
    pub apparent_bytes: u64,
    /// Bytes actually occupied on the host filesystem (sparse images are much
    /// smaller). `None` where the host cannot say.
    pub allocated_bytes: Option<u64>,
    pub table: TableKind,
    /// GPT only.
    pub disk_guid: Option<String>,
    pub partitions: Vec<PartitionReport>,
    /// The `.nvram` UEFI variable-store sidecar, when one exists next to the
    /// image (a UEFI VM's boot entries live there — it travels with the disk).
    pub nvram_sidecar: Option<PathBuf>,
}

impl DiskReport {
    /// Machine-readable form for `disk inspect --json`.
    pub fn to_json(&self) -> String {
        serde_json::to_string_pretty(self).unwrap_or_else(|_| "{}".to_string())
    }

    /// One-line partition summary for list views:
    /// `"GPT · EFI System 1.0 MiB · Linux filesystem 14.9 GiB"`.
    pub fn partition_summary(&self) -> String {
        match self.table {
            TableKind::None => "blank — no partition table".to_string(),
            kind => {
                if self.partitions.is_empty() {
                    return format!("{kind} · no partitions");
                }
                let parts: Vec<String> = self
                    .partitions
                    .iter()
                    .map(|p| format!("{} {}", p.type_name, ops::format_bytes(p.size_bytes)))
                    .collect();
                format!("{kind} · {}", parts.join(" · "))
            }
        }
    }
}

/// Inspects one RAW disk image. A blank image is a valid report
/// ([`TableKind::None`]); a *corrupt* table is a typed error — the report
/// never guesses.
pub fn inspect_disk(path: &Path) -> Result<DiskReport, DiskFsError> {
    let mut file = std::fs::File::open(path)?;
    let apparent = file.metadata()?.len();
    let table = layout::read_partition_table_from(&mut file, apparent)?;

    let (kind, disk_guid, partitions) = match table {
        PartitionTable::Empty => (TableKind::None, None, Vec::new()),
        PartitionTable::Mbr(parts) => {
            let mut reports = Vec::with_capacity(parts.len());
            for p in &parts {
                let first_lba = u64::from(p.start_lba);
                let sectors = u64::from(p.sectors);
                let start = first_lba.saturating_mul(SECTOR);
                let end = start.saturating_add(sectors.saturating_mul(SECTOR));
                // Probe only ranges the image can actually hold; an entry past
                // the end is still *reported* (that is what inspection is for),
                // just never dereferenced.
                let ext4 = if sectors > 0 && end <= apparent {
                    layout::ext4_info(&mut file, start)?
                } else {
                    None
                };
                reports.push(PartitionReport {
                    index: p.index,
                    type_name: layout::mbr_type_name(p.type_byte).to_string(),
                    type_id: format!("{:#04x}", p.type_byte),
                    name: None,
                    first_lba,
                    sectors,
                    size_bytes: sectors.saturating_mul(SECTOR),
                    ext4_uuid: ext4.as_ref().map(|i| i.uuid.clone()),
                    ext4_label: ext4.and_then(|i| i.label),
                });
            }
            (TableKind::Mbr, None, reports)
        }
        PartitionTable::Gpt { header, partitions } => {
            let mut reports = Vec::with_capacity(partitions.len());
            for p in &partitions {
                // GPT partitions were bounds-checked against the image by the
                // parser, so the probe is always in range.
                let ext4 = layout::ext4_info(&mut file, p.offset())?;
                reports.push(PartitionReport {
                    index: p.index,
                    type_name: layout::gpt_type_name(&p.type_guid).to_string(),
                    type_id: p.type_guid.to_string(),
                    name: (!p.name.is_empty()).then(|| p.name.clone()),
                    first_lba: p.first_lba,
                    sectors: p.sectors(),
                    size_bytes: p.sectors().saturating_mul(SECTOR),
                    ext4_uuid: ext4.as_ref().map(|i| i.uuid.clone()),
                    ext4_label: ext4.and_then(|i| i.label),
                });
            }
            (TableKind::Gpt, Some(header.disk_guid.to_string()), reports)
        }
    };

    drop(file);

    Ok(DiskReport {
        path: path.to_path_buf(),
        apparent_bytes: apparent,
        allocated_bytes: ops::allocated_bytes(path),
        table: kind,
        disk_guid,
        partitions,
        nvram_sidecar: ops::existing_nvram_sidecar(path),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixtures::{mbr_with, temp_dir, GptBuilder};
    use crate::layout::{ESP_TYPE, LINUX_FS_TYPE};
    use std::io::Write as _;

    const TEST_SECTORS: u64 = 32768; // 16 MiB

    #[test]
    fn reports_a_gpt_disk_with_esp_root_and_sidecar() {
        let dir = temp_dir("inspect-gpt");
        let disk = dir.join("desktop.raw");
        std::fs::write(
            &disk,
            GptBuilder::new(TEST_SECTORS)
                .ubuntu_layout()
                .with_ext4(2, [0xab; 16]),
        )
        .unwrap();
        std::fs::write(dir.join("desktop.nvram"), b"vars").unwrap();

        let report = inspect_disk(&disk).expect("inspect");
        assert_eq!(report.table, TableKind::Gpt);
        assert_eq!(report.apparent_bytes, TEST_SECTORS * SECTOR);
        assert_eq!(report.disk_guid.as_deref().map(|g| g.len()), Some(36));
        assert_eq!(report.partitions.len(), 2);

        let esp = &report.partitions[0];
        assert_eq!(esp.type_name, "EFI System");
        assert_eq!(esp.type_id, ESP_TYPE.to_string());
        assert_eq!(esp.name.as_deref(), Some("EFI System Partition"));
        assert_eq!(esp.size_bytes, 2048 * SECTOR);
        assert_eq!(esp.ext4_uuid, None);

        let root = &report.partitions[1];
        assert_eq!(root.type_name, "Linux filesystem");
        assert_eq!(
            root.ext4_uuid.as_deref(),
            Some("abababab-abab-abab-abab-abababababab")
        );

        assert_eq!(report.nvram_sidecar, Some(dir.join("desktop.nvram")));

        let summary = report.partition_summary();
        assert!(summary.starts_with("GPT · EFI System 1.0 MiB"), "{summary}");
        assert!(summary.contains("Linux filesystem"), "{summary}");
    }

    #[test]
    fn reports_an_mbr_disk_with_the_ext4_uuid() {
        let dir = temp_dir("inspect-mbr");
        let disk = dir.join("debian.raw");
        let mut f = std::fs::File::create(&disk).unwrap();
        f.write_all(&mbr_with(&[(0, 0x83, 4, 64), (1, 0x82, 68, 32)]))
            .unwrap();
        f.set_len((68 + 32) * 512).unwrap();
        // ext4 magic + uuid on p1.
        use std::io::{Seek as _, SeekFrom};
        let sb_at = 4 * 512 + 1024;
        f.seek(SeekFrom::Start(sb_at + 0x38)).unwrap();
        f.write_all(&[0x53, 0xef]).unwrap();
        f.seek(SeekFrom::Start(sb_at + 0x68)).unwrap();
        f.write_all(&[0x11; 16]).unwrap();
        drop(f);

        let report = inspect_disk(&disk).expect("inspect");
        assert_eq!(report.table, TableKind::Mbr);
        assert_eq!(report.disk_guid, None);
        assert_eq!(report.partitions.len(), 2);
        assert_eq!(report.partitions[0].type_name, "Linux");
        assert_eq!(report.partitions[0].type_id, "0x83");
        assert_eq!(
            report.partitions[0].ext4_uuid.as_deref(),
            Some("11111111-1111-1111-1111-111111111111")
        );
        assert_eq!(report.partitions[1].type_name, "Linux swap");
        assert_eq!(report.nvram_sidecar, None);

        let summary = report.partition_summary();
        assert!(summary.starts_with("MBR · Linux 32.0 KiB"), "{summary}");
    }

    #[test]
    fn a_blank_image_reports_no_table() {
        let dir = temp_dir("inspect-blank");
        let disk = dir.join("fresh.raw");
        crate::ops::create_raw(&disk, 4 << 20).unwrap();

        let report = inspect_disk(&disk).expect("inspect");
        assert_eq!(report.table, TableKind::None);
        assert!(report.partitions.is_empty());
        assert_eq!(report.apparent_bytes, 4 << 20);
        assert_eq!(report.partition_summary(), "blank — no partition table");
        // Sparse: where the host can answer, the image occupies almost nothing.
        if let Some(allocated) = report.allocated_bytes {
            assert!(allocated < 1 << 20, "allocated {allocated}");
        }
    }

    #[test]
    fn an_mbr_partition_past_the_image_is_reported_but_never_probed() {
        let dir = temp_dir("inspect-oob");
        let disk = dir.join("hostile.raw");
        let mut f = std::fs::File::create(&disk).unwrap();
        f.write_all(&mbr_with(&[(0, 0x83, 4, u32::MAX)])).unwrap();
        f.set_len(1 << 20).unwrap();
        drop(f);

        let report = inspect_disk(&disk).expect("inspect");
        assert_eq!(report.partitions.len(), 1);
        assert_eq!(report.partitions[0].ext4_uuid, None);
        assert_eq!(
            report.partitions[0].size_bytes,
            u64::from(u32::MAX) * SECTOR
        );
    }

    #[test]
    fn a_corrupt_gpt_is_a_typed_error_not_a_report() {
        let dir = temp_dir("inspect-corrupt");
        let disk = dir.join("corrupt.raw");
        let mut broken = GptBuilder::new(TEST_SECTORS).ubuntu_layout();
        broken.break_entries_crc = true;
        std::fs::write(&disk, broken.image()).unwrap();
        assert!(matches!(
            inspect_disk(&disk),
            Err(DiskFsError::GptEntriesCrc { .. })
        ));
    }

    #[test]
    fn json_output_carries_the_load_bearing_fields() {
        let dir = temp_dir("inspect-json");
        let disk = dir.join("j.raw");
        std::fs::write(
            &disk,
            GptBuilder::new(TEST_SECTORS)
                .part(ESP_TYPE, 2048, 4095, "esp")
                .part(LINUX_FS_TYPE, 4096, 9000, "root")
                .image(),
        )
        .unwrap();

        let json = inspect_disk(&disk).unwrap().to_json();
        let value: serde_json::Value = serde_json::from_str(&json).expect("valid JSON");
        assert_eq!(value["table"], "gpt");
        assert_eq!(value["apparent_bytes"], TEST_SECTORS * SECTOR);
        assert_eq!(value["partitions"][0]["type_name"], "EFI System");
        assert_eq!(
            value["partitions"][0]["type_id"],
            "C12A7328-F81F-11D2-BA4B-00A0C93EC93B"
        );
        assert_eq!(value["partitions"][1]["name"], "root");
        assert!(value["nvram_sidecar"].is_null());
    }
}
