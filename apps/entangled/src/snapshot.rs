//! What `entangled` puts in a snapshot's metadata, and how it resumes from one
//! ([ADR-0006](../../../docs/adr/0006-suspend-restore.md)).
//!
//! `vm-snapshot` owns the format and every refusal; this module owns the two
//! things only the CLI knows — which files a profile references, and what the
//! machine's shape is called in the profile's own vocabulary.

use std::path::{Path, PathBuf};

use control_api::{BootMode, VmConfig};
use vm_snapshot::meta::{FileFingerprint, FileRole, MachineShape, Metadata};

/// Conventional snapshot path for a VM whose profile lives at `config`.
///
/// Beside the profile rather than beside the disk: a snapshot belongs to a
/// *configuration* (it is only restorable onto that machine's shape), and a
/// disk can be referenced by several.
pub fn default_path(config: &Path, name: &str) -> PathBuf {
    config
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join(format!("{name}.{}", vm_snapshot::EXTENSION))
}

/// The machine's shape as the profile describes it, minus the device list —
/// which comes from the bus, because the bus is what actually attached them.
pub fn shape(cfg: &VmConfig, devices: Vec<vm_snapshot::DeviceSlot>) -> MachineShape {
    MachineShape {
        vcpus: cfg.vcpus,
        memory_bytes: cfg.memory_mib << 20,
        transport: cfg.transport.to_string(),
        boot_mode: match cfg.boot.mode {
            BootMode::DirectLinux => "direct-linux".into(),
            BootMode::Uefi => "uefi".into(),
        },
        devices,
    }
}

/// Every file this VM has open or would re-read, fingerprinted as it is now.
///
/// Measured with the VM already quiesced, so nothing is writing any of them and
/// the readings are stable until the process exits. The **strict** ones — the
/// disks and the CD-ROM — are what a restore refuses on; the rest are recorded
/// so a difference can be reported rather than discovered.
pub fn fingerprints(cfg: &VmConfig) -> Vec<FileFingerprint> {
    let mut files = Vec::with_capacity(cfg.disks.len() + 4);
    for disk in &cfg.disks {
        files.push(FileFingerprint::measure(FileRole::Disk, &disk.path));
    }
    if let Some(cdrom) = &cfg.cdrom {
        files.push(FileFingerprint::measure(FileRole::Cdrom, &cdrom.path));
    }
    for (role, path) in [
        (FileRole::Nvram, cfg.boot.nvram.as_ref()),
        (FileRole::Firmware, cfg.boot.firmware.as_ref()),
        (FileRole::Kernel, cfg.boot.kernel.as_ref()),
        (FileRole::Initramfs, cfg.boot.initramfs.as_ref()),
    ] {
        if let Some(path) = path {
            files.push(FileFingerprint::measure(role, path));
        }
    }
    files
}

/// The metadata section for a VM about to be written down.
///
/// The profile goes in **verbatim**, so `entangled resume <file>` needs nothing
/// but the file: a snapshot that had to be paired with a TOML someone might
/// have edited in the meantime would be a snapshot with a second, unversioned
/// half.
pub fn metadata(cfg: &VmConfig, devices: Vec<vm_snapshot::DeviceSlot>) -> Metadata {
    Metadata {
        vm_name: cfg.name.clone(),
        created_unix: vm_snapshot::meta::now_unix(),
        writer: vm_snapshot::writer_id(),
        config_toml: toml::to_string_pretty(cfg).unwrap_or_default(),
        shape: shape(cfg, devices),
        files: fingerprints(cfg),
    }
}

/// The profile a snapshot was taken from.
///
/// Re-parsed through `VmConfig::from_toml`, validation and all: the string in
/// the file is data like everything else in it, and a profile that no longer
/// validates against this build's rules must fail here rather than half-way
/// through assembling a machine.
pub fn config_from(metadata: &Metadata) -> Result<VmConfig, String> {
    VmConfig::from_toml(&metadata.config_toml).map_err(|e| {
        format!(
            "the profile stored in the snapshot is not one this build accepts: {e}. \
             It was written by {}",
            metadata.writer
        )
    })
}

/// Reads a snapshot's header and metadata, refusing early and by name.
///
/// Called before a single byte of a VM is allocated, so `entangled resume` on a
/// snapshot from the other host — or from a future build — costs a file open
/// and says exactly what is wrong.
pub fn open(path: &Path) -> Result<vm_snapshot::SnapshotInfo, String> {
    let info = vm_snapshot::inspect(path).map_err(|e| format!("{}: {e}", path.display()))?;
    if let Some(refusal) = &info.refusal {
        return Err(format!("{}: {refusal}", path.display()));
    }
    Ok(info)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn profile() -> VmConfig {
        VmConfig::from_toml(
            r#"
name = "demo"
memory_mib = 2048
vcpus = 2
transport = "pci"

[boot]
mode = "direct-linux"
kernel = "/does/not/exist/vmlinuz"
cmdline = "console=ttyS0"

[[disk]]
path = "/does/not/exist/root.raw"
"#,
        )
        .expect("profile")
    }

    #[test]
    fn the_shape_comes_from_the_profile() {
        let shape = shape(&profile(), Vec::new());
        assert_eq!(shape.vcpus, 2);
        assert_eq!(shape.memory_bytes, 2048 << 20);
        assert_eq!(shape.transport, "pci");
        assert_eq!(shape.boot_mode, "direct-linux");
    }

    /// Every file a profile names is fingerprinted, present or not — a disk
    /// that has *appeared* since the snapshot is as much a change as one that
    /// has gone.
    #[test]
    fn every_referenced_file_is_fingerprinted() {
        let files = fingerprints(&profile());
        let roles: Vec<FileRole> = files.iter().map(|f| f.role).collect();
        assert_eq!(roles, vec![FileRole::Disk, FileRole::Kernel]);
        assert!(files.iter().all(|f| !f.present));
    }

    /// The profile survives the round trip through the metadata section, which
    /// is what makes `entangled resume <file>` self-contained.
    #[test]
    fn the_profile_round_trips_through_the_metadata() {
        let cfg = profile();
        let meta = metadata(&cfg, Vec::new());
        let back = config_from(&meta).expect("re-parse");
        assert_eq!(back.name, cfg.name);
        assert_eq!(back.vcpus, cfg.vcpus);
        assert_eq!(back.memory_mib, cfg.memory_mib);
        assert_eq!(back.disks.len(), 1);
        assert_eq!(back.boot.cmdline, cfg.boot.cmdline);
    }

    #[test]
    fn a_profile_the_build_rejects_is_a_named_error() {
        let mut meta = metadata(&profile(), Vec::new());
        meta.config_toml = "name = \"\"\n".into();
        let err = config_from(&meta).unwrap_err();
        assert!(err.contains("not one this build accepts"), "{err}");
    }

    #[test]
    fn the_default_path_sits_beside_the_profile() {
        let path = default_path(Path::new("/vm/profiles/demo.toml"), "demo");
        assert_eq!(path, PathBuf::from("/vm/profiles/demo.esnap"));
        // A bare filename still lands somewhere sensible.
        assert_eq!(
            default_path(Path::new("demo.toml"), "demo"),
            PathBuf::from("demo.esnap")
        );
    }
}
