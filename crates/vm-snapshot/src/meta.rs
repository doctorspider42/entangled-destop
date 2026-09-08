//! The metadata section: what VM this snapshot is of, and every fingerprint a
//! restore is checked against.
//!
//! # Why the refusals are the feature
//!
//! Restoring a guest is putting a live kernel back on top of storage it thinks
//! it still owns. The kernel's page cache holds inodes, directory entries and
//! journal state that describe the filesystem **as it was at the instant of the
//! snapshot**. Let that kernel loose on a disk image that has moved on since —
//! mounted elsewhere, resized, restored from a backup, written by a second VM —
//! and it will write its stale metadata over the new contents. The filesystem
//! is then corrupt in a way that surfaces hours later.
//!
//! So a changed disk is not a warning, it is a refusal, and the refusal says
//! which disk and what about it changed. The same holds, less catastrophically,
//! for the machine's shape: a snapshot restored onto a VM with a different
//! device order is a guest whose `/dev/vda` is now somebody else's disk.

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::codec::{Reader, Writer};
use crate::error::{Result, SnapshotError};

/// Version of the metadata section's own encoding.
pub const METADATA_VERSION: u32 = 1;

const MAX_NAME: usize = 256;
const MAX_PATH: usize = 4096;
const MAX_CONFIG: usize = 1 << 20;
const MAX_FILES: usize = 64;
const MAX_DEVICES: usize = 64;

/// What a referenced file is to the VM, which decides how strict the check is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileRole {
    /// A `[[disk]]`. The guest's kernel holds cached metadata for it.
    Disk,
    /// The CD-ROM. Read-only to the guest, but its contents are still mounted.
    Cdrom,
    /// The UEFI variable store. Re-read at restore, so a change is survivable.
    Nvram,
    /// The firmware image; only re-read on a later in-place reset.
    Firmware,
    /// A direct-Linux kernel; only re-read on a later in-place reset.
    Kernel,
    /// A direct-Linux initramfs; same.
    Initramfs,
}

impl FileRole {
    /// True when a change makes the restore unsafe rather than merely odd.
    pub const fn is_strict(self) -> bool {
        matches!(self, FileRole::Disk | FileRole::Cdrom)
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            FileRole::Disk => "disk",
            FileRole::Cdrom => "cdrom",
            FileRole::Nvram => "nvram",
            FileRole::Firmware => "firmware",
            FileRole::Kernel => "kernel",
            FileRole::Initramfs => "initramfs",
        }
    }

    const fn code(self) -> u32 {
        match self {
            FileRole::Disk => 1,
            FileRole::Cdrom => 2,
            FileRole::Nvram => 3,
            FileRole::Firmware => 4,
            FileRole::Kernel => 5,
            FileRole::Initramfs => 6,
        }
    }

    fn from_code(code: u32) -> Result<Self> {
        Ok(match code {
            1 => FileRole::Disk,
            2 => FileRole::Cdrom,
            3 => FileRole::Nvram,
            4 => FileRole::Firmware,
            5 => FileRole::Kernel,
            6 => FileRole::Initramfs,
            other => {
                return Err(SnapshotError::BadValue {
                    what: "file role",
                    value: u64::from(other),
                })
            }
        })
    }
}

/// One file the VM had open, as it looked when the snapshot was taken.
///
/// Size and modification time rather than a content digest: hashing a 32 GiB
/// image at both ends of every suspend would cost more than the snapshot
/// itself, and the pair catches every accident this is meant to catch — a
/// resize, a re-provision, a copy back from a backup, a second VM having run
/// against the same image.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileFingerprint {
    pub role: FileRole,
    pub path: PathBuf,
    pub len: u64,
    /// Nanoseconds since the Unix epoch, or 0 where the host does not answer.
    pub modified_nanos: u64,
    /// Whether the file existed at all when the snapshot was taken.
    pub present: bool,
}

impl FileFingerprint {
    /// Measures `path` now.
    pub fn measure(role: FileRole, path: &Path) -> Self {
        let meta = std::fs::metadata(path).ok();
        let modified_nanos = meta
            .as_ref()
            .and_then(|m| m.modified().ok())
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .map(|d| d.as_nanos().min(u128::from(u64::MAX)) as u64)
            .unwrap_or(0);
        Self {
            role,
            path: path.to_path_buf(),
            len: meta.as_ref().map(|m| m.len()).unwrap_or(0),
            modified_nanos,
            present: meta.is_some(),
        }
    }

    /// Compares this fingerprint against the file as it is now.
    ///
    /// Returns the refusal for a strict role, or `Ok(Some(reason))` for an
    /// advisory one so the caller can log it.
    pub fn check(&self) -> Result<Option<String>> {
        let now = FileFingerprint::measure(self.role, &self.path);
        let differs = |what: &'static str, was: String, is: String| {
            if self.role.is_strict() {
                Err(SnapshotError::DiskChanged {
                    path: self.path.display().to_string(),
                    what,
                    snapshot: was,
                    current: is,
                })
            } else {
                Ok(Some(format!(
                    "{} {} changed since the snapshot ({what}: was {was}, is {is})",
                    self.role.as_str(),
                    self.path.display()
                )))
            }
        };
        if self.present && !now.present {
            return differs("existence", "present".into(), "missing".into());
        }
        if !self.present && now.present {
            return differs("existence", "missing".into(), "present".into());
        }
        if !self.present {
            return Ok(None);
        }
        if self.len != now.len {
            return differs("size", self.len.to_string(), now.len.to_string());
        }
        // A zero from either side means the host would not answer; comparing it
        // would refuse a perfectly good restore on a filesystem with no mtime.
        if self.modified_nanos != 0
            && now.modified_nanos != 0
            && self.modified_nanos != now.modified_nanos
        {
            return differs(
                "modification time",
                format_nanos(self.modified_nanos),
                format_nanos(now.modified_nanos),
            );
        }
        Ok(None)
    }

    fn encode(&self, w: &mut Writer) {
        w.u32(self.role.code());
        w.string(&self.path.to_string_lossy());
        w.u64(self.len);
        w.u64(self.modified_nanos);
        w.bool(self.present);
    }

    fn decode(r: &mut Reader<'_>) -> Result<Self> {
        Ok(Self {
            role: FileRole::from_code(r.u32("file role")?)?,
            path: PathBuf::from(r.string("file path", MAX_PATH)?),
            len: r.u64("file length")?,
            modified_nanos: r.u64("file mtime")?,
            present: r.bool("file present")?,
        })
    }
}

fn format_nanos(nanos: u64) -> String {
    format!("{}.{:09}", nanos / 1_000_000_000, nanos % 1_000_000_000)
}

/// One virtio device as the machine attached it: its type and the slot it sits
/// in.
///
/// Order is guest-visible naming (`/dev/vda`, `00:01.0`), so a snapshot
/// restored onto a differently-ordered device list is a corrupted guest even
/// when every individual device matches.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DeviceSlot {
    /// `virtio_core::DeviceType::id()`.
    pub device_type: u32,
    pub slot: u32,
}

/// The machine's shape, compared field by field so a refusal can name what
/// differs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MachineShape {
    pub vcpus: u32,
    pub memory_bytes: u64,
    /// `"mmio"` or `"pci"`.
    pub transport: String,
    /// `"direct-linux"` or `"uefi"`.
    pub boot_mode: String,
    pub devices: Vec<DeviceSlot>,
}

impl MachineShape {
    /// Refuses a machine that is not the one the snapshot came off, naming the
    /// first field that differs.
    pub fn check(&self, current: &MachineShape) -> Result<()> {
        let mismatch = |field: &str, a: String, b: String| SnapshotError::Mismatch {
            field: field.into(),
            snapshot: a,
            current: b,
        };
        if self.vcpus != current.vcpus {
            return Err(mismatch(
                "vcpus",
                self.vcpus.to_string(),
                current.vcpus.to_string(),
            ));
        }
        if self.memory_bytes != current.memory_bytes {
            return Err(mismatch(
                "memory",
                format!("{} MiB", self.memory_bytes >> 20),
                format!("{} MiB", current.memory_bytes >> 20),
            ));
        }
        if self.transport != current.transport {
            return Err(mismatch(
                "virtio transport",
                self.transport.clone(),
                current.transport.clone(),
            ));
        }
        if self.boot_mode != current.boot_mode {
            return Err(mismatch(
                "boot mode",
                self.boot_mode.clone(),
                current.boot_mode.clone(),
            ));
        }
        if self.devices.len() != current.devices.len() {
            return Err(mismatch(
                "device count",
                self.devices.len().to_string(),
                current.devices.len().to_string(),
            ));
        }
        for (index, (was, is)) in self.devices.iter().zip(&current.devices).enumerate() {
            if was != is {
                return Err(mismatch(
                    &format!("device {index}"),
                    format!("type {} in slot {}", was.device_type, was.slot),
                    format!("type {} in slot {}", is.device_type, is.slot),
                ));
            }
        }
        Ok(())
    }
}

/// Everything the metadata section carries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Metadata {
    /// The VM's name from its profile.
    pub vm_name: String,
    /// When the snapshot was taken, seconds since the Unix epoch.
    pub created_unix: i64,
    /// Which build wrote it — for a bug report, never for a compatibility
    /// decision (the format version decides that).
    pub writer: String,
    /// The profile the VM was started from, verbatim, so `entangled resume`
    /// needs nothing but the snapshot file.
    pub config_toml: String,
    pub shape: MachineShape,
    pub files: Vec<FileFingerprint>,
}

impl Metadata {
    pub fn encode(&self) -> Vec<u8> {
        let mut w = Writer::with_capacity(1024 + self.config_toml.len());
        w.string(&self.vm_name);
        w.i64(self.created_unix);
        w.string(&self.writer);
        w.string(&self.config_toml);
        w.u32(self.shape.vcpus);
        w.u64(self.shape.memory_bytes);
        w.string(&self.shape.transport);
        w.string(&self.shape.boot_mode);
        w.count(self.shape.devices.len());
        for device in &self.shape.devices {
            w.u32(device.device_type).u32(device.slot);
        }
        w.count(self.files.len());
        for file in &self.files {
            file.encode(&mut w);
        }
        w.into_bytes()
    }

    pub fn decode(bytes: &[u8]) -> Result<Self> {
        let mut r = Reader::new(bytes);
        let vm_name = r.string("vm name", MAX_NAME)?;
        let created_unix = r.i64("created")?;
        let writer = r.string("writer", MAX_NAME)?;
        let config_toml = r.string("config", MAX_CONFIG)?;
        let vcpus = r.u32("vcpus")?;
        let memory_bytes = r.u64("memory bytes")?;
        let transport = r.string("transport", MAX_NAME)?;
        let boot_mode = r.string("boot mode", MAX_NAME)?;
        let device_count = r.count("device count", MAX_DEVICES, 8)?;
        let mut devices = Vec::with_capacity(device_count);
        for _ in 0..device_count {
            devices.push(DeviceSlot {
                device_type: r.u32("device type")?,
                slot: r.u32("device slot")?,
            });
        }
        // 29 = the smallest a `FileFingerprint` can encode to (four zero-length
        // strings would still cost their length prefixes).
        let file_count = r.count("file count", MAX_FILES, 29)?;
        let mut files = Vec::with_capacity(file_count);
        for _ in 0..file_count {
            files.push(FileFingerprint::decode(&mut r)?);
        }
        r.finish("metadata section")?;
        Ok(Self {
            vm_name,
            created_unix,
            writer,
            config_toml,
            shape: MachineShape {
                vcpus,
                memory_bytes,
                transport,
                boot_mode,
                devices,
            },
            files,
        })
    }

    /// Checks every fingerprint against the host as it is now. Returns the
    /// advisory notes; a strict change is the error.
    pub fn check_files(&self) -> Result<Vec<String>> {
        let mut notes = Vec::new();
        for file in &self.files {
            if let Some(note) = file.check()? {
                notes.push(note);
            }
        }
        Ok(notes)
    }
}

/// Seconds since the Unix epoch, or 0 on a host whose clock is before it.
pub fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Metadata {
        Metadata {
            vm_name: "demo".into(),
            created_unix: 1_760_000_000,
            writer: "entangled 0.2.0".into(),
            config_toml: "name = \"demo\"\n".into(),
            shape: MachineShape {
                vcpus: 2,
                memory_bytes: 4 << 30,
                transport: "pci".into(),
                boot_mode: "uefi".into(),
                devices: vec![
                    DeviceSlot {
                        device_type: 2,
                        slot: 0,
                    },
                    DeviceSlot {
                        device_type: 16,
                        slot: 1,
                    },
                ],
            },
            files: vec![FileFingerprint {
                role: FileRole::Disk,
                path: PathBuf::from("/vm/demo.raw"),
                len: 32 << 30,
                modified_nanos: 1_760_000_000_000_000_000,
                present: true,
            }],
        }
    }

    #[test]
    fn metadata_round_trips() {
        let meta = sample();
        let decoded = Metadata::decode(&meta.encode()).unwrap();
        assert_eq!(decoded, meta);
    }

    #[test]
    fn a_truncated_metadata_section_is_an_error_at_every_length() {
        let bytes = sample().encode();
        for cut in 0..bytes.len() {
            let err = Metadata::decode(&bytes[..cut]).unwrap_err();
            let _ = err.to_string();
        }
    }

    #[test]
    fn the_shape_check_names_the_field_that_differs() {
        let meta = sample();
        let mut other = meta.shape.clone();
        other.vcpus = 4;
        let err = meta.shape.check(&other).unwrap_err();
        assert!(
            matches!(&err, SnapshotError::Mismatch { field, .. } if field == "vcpus"),
            "{err}"
        );

        let mut other = meta.shape.clone();
        other.memory_bytes = 2 << 30;
        let err = meta.shape.check(&other).unwrap_err();
        assert!(err.to_string().contains("4096 MiB"), "{err}");

        let mut other = meta.shape.clone();
        other.devices.swap(0, 1);
        let err = meta.shape.check(&other).unwrap_err();
        assert!(
            matches!(&err, SnapshotError::Mismatch { field, .. } if field == "device 0"),
            "{err}"
        );

        assert!(meta.shape.check(&meta.shape).is_ok());
    }

    /// A disk whose size moved is refused, and the message names the disk.
    #[test]
    fn a_resized_disk_is_refused() {
        let dir = std::env::temp_dir().join(format!("entangled-fp-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("disk.raw");
        std::fs::write(&path, [0u8; 64]).unwrap();
        let fp = FileFingerprint::measure(FileRole::Disk, &path);
        assert!(fp.check().unwrap().is_none());

        std::fs::write(&path, [0u8; 128]).unwrap();
        let err = fp.check().unwrap_err();
        assert!(
            matches!(&err, SnapshotError::DiskChanged { what: "size", .. }),
            "{err}"
        );
        assert!(err.to_string().contains("disk.raw"), "{err}");

        std::fs::remove_file(&path).unwrap();
        let err = fp.check().unwrap_err();
        assert!(
            matches!(
                &err,
                SnapshotError::DiskChanged {
                    what: "existence",
                    ..
                }
            ),
            "{err}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A firmware image that moved is a note, not a refusal: it is only re-read
    /// by a later in-place reset, and the restored guest is long past it.
    #[test]
    fn an_advisory_role_reports_rather_than_refuses() {
        let dir = std::env::temp_dir().join(format!("entangled-fp2-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("CLOUDHV.fd");
        std::fs::write(&path, [0u8; 64]).unwrap();
        let fp = FileFingerprint::measure(FileRole::Firmware, &path);
        std::fs::write(&path, [0u8; 65]).unwrap();
        let note = fp.check().unwrap().expect("a note");
        assert!(note.contains("firmware"), "{note}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
