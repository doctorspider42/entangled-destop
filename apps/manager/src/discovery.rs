//! VM discovery: turn a directory of `*.toml` profiles into cards the UI can
//! draw, list every disk image those profiles (or the directory) hold, and
//! delete/rewire them again with guards.
//!
//! All disk knowledge (sizes, partition tables, sidecars) comes from the
//! shared `disk-image` crate — the same code `entangled disk` runs, linked
//! rather than shelled out to.

use std::path::{Path, PathBuf};

use control_api::{DiskSection, VirtioTransport, VmConfig};
use thiserror::Error;

pub use disk_image::format_bytes;

#[derive(Debug, Error)]
pub enum DiscoveryError {
    #[error("cannot read the VM directory {path}: {source}")]
    ReadDir {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

#[derive(Debug, Error)]
pub enum DeleteError {
    #[error("'{0}' is busy — stop it before deleting")]
    Busy(String),

    #[error("the typed name does not match '{0}'")]
    NameMismatch(String),

    #[error("cannot remove {path}: {source}")]
    Remove {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

/// One disk of a VM, with the numbers the card shows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiskInfo {
    /// Path exactly as written in the profile.
    pub declared: PathBuf,
    /// Path after resolving a relative entry against the child working
    /// directory and then the profile directory.
    pub resolved: PathBuf,
    pub exists: bool,
    /// The `writable` flag of the `[[disk]]` entry.
    pub writable: bool,
    /// Nominal image size (`metadata.len()`).
    pub size_bytes: u64,
    /// Blocks actually allocated — RAW images are sparse, so this is usually
    /// much smaller. `None` on platforms without the block count.
    pub allocated_bytes: Option<u64>,
}

/// A VM as discovered on disk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VmEntry {
    pub name: String,
    pub profile_path: PathBuf,
    pub memory_mib: u64,
    pub vcpus: u32,
    /// Which virtio transport the guest sees. Part of the machine's *shape*,
    /// so a snapshot taken with one cannot be restored onto the other — which
    /// is why the card layer needs it and not just the editor.
    pub transport: VirtioTransport,
    pub display: (u32, u32),
    pub network_interface: Option<String>,
    pub disks: Vec<DiskInfo>,
    /// The profile boots through UEFI firmware rather than loading a kernel
    /// directly. Kept because the two boot modes need *different* host
    /// artifacts present, and warning about the wrong one is worse than not
    /// warning: an installed Ubuntu needs `artifacts/firmware/CLOUDHV.fd`, and
    /// has no use for a bootstrap kernel.
    pub uefi: bool,
}

impl VmEntry {
    pub fn size_bytes(&self) -> u64 {
        self.disks.iter().map(|d| d.size_bytes).sum()
    }

    pub fn allocated_bytes(&self) -> Option<u64> {
        self.disks
            .iter()
            .map(|d| d.allocated_bytes)
            .try_fold(0u64, |acc, v| v.map(|v| acc + v))
    }
}

/// A profile file that could not be used, kept so the UI can say why instead
/// of hiding it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScanProblem {
    pub path: PathBuf,
    pub message: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Scan {
    pub vms: Vec<VmEntry>,
    pub problems: Vec<ScanProblem>,
    /// Every disk the profiles reference plus every loose `*.raw` in the VM
    /// directory (the Disks view).
    pub disks: Vec<DiskRow>,
    /// Every `*.esnap` in the VM directory, read (the Snapshots view, and the
    /// third resting state a card can be in).
    pub snapshots: Vec<crate::snapshots::SnapshotRow>,
}

/// Scans `vm_dir` for VM profiles. A single broken profile never fails the
/// scan; only an unreadable directory does.
pub fn scan(vm_dir: &Path, work_dir: Option<&Path>) -> Result<Scan, DiscoveryError> {
    let entries = std::fs::read_dir(vm_dir).map_err(|source| DiscoveryError::ReadDir {
        path: vm_dir.to_path_buf(),
        source,
    })?;

    let mut scan = Scan::default();
    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(e) => {
                scan.problems.push(ScanProblem {
                    path: vm_dir.to_path_buf(),
                    message: e.to_string(),
                });
                continue;
            }
        };
        let path = entry.path();
        if !path.is_file() || path.extension().is_none_or(|ext| ext != "toml") {
            continue;
        }
        match load_profile(&path, work_dir) {
            Ok(vm) => scan.vms.push(vm),
            Err(message) => scan.problems.push(ScanProblem { path, message }),
        }
    }

    scan.vms.sort_by(|a, b| a.name.cmp(&b.name));
    scan.problems.sort_by(|a, b| a.path.cmp(&b.path));
    scan.disks = disk_rows(&scan.vms, vm_dir);
    scan.snapshots = snapshot_rows(vm_dir);
    Ok(scan)
}

/// Reads and validates one profile through `control-api`, then measures its
/// disks.
pub fn load_profile(path: &Path, work_dir: Option<&Path>) -> Result<VmEntry, String> {
    let text = std::fs::read_to_string(path).map_err(|e| e.to_string())?;
    let cfg = VmConfig::from_toml(&text).map_err(|e| e.to_string())?;
    Ok(entry_from_config(path, &cfg, work_dir))
}

fn entry_from_config(path: &Path, cfg: &VmConfig, work_dir: Option<&Path>) -> VmEntry {
    let profile_dir = path.parent().map(Path::to_path_buf);
    let disks = cfg
        .disks
        .iter()
        .map(|disk| {
            let resolved = resolve(&disk.path, work_dir, profile_dir.as_deref());
            let meta = std::fs::metadata(&resolved).ok();
            DiskInfo {
                declared: disk.path.clone(),
                exists: meta.is_some(),
                writable: disk.writable,
                size_bytes: meta.as_ref().map(|m| m.len()).unwrap_or(0),
                allocated_bytes: meta
                    .is_some()
                    .then(|| disk_image::allocated_bytes(&resolved))
                    .flatten(),
                resolved,
            }
        })
        .collect();

    VmEntry {
        name: cfg.name.clone(),
        profile_path: path.to_path_buf(),
        memory_mib: cfg.memory_mib,
        vcpus: cfg.vcpus,
        transport: cfg.transport,
        display: (cfg.display.width, cfg.display.height),
        network_interface: cfg.network.as_ref().and_then(|n| n.interface.clone()),
        disks,
        uefi: matches!(cfg.boot.mode, control_api::BootMode::Uefi),
    }
}

/// Relative paths in a profile resolve like the CLI resolves them (against the
/// child working directory); if nothing is there, the profile directory is
/// tried, which is where `entangled install` puts the disk.
fn resolve(path: &Path, work_dir: Option<&Path>, profile_dir: Option<&Path>) -> PathBuf {
    if path.is_absolute() {
        return path.to_path_buf();
    }
    for base in [work_dir, profile_dir].into_iter().flatten() {
        let candidate = base.join(path);
        if candidate.exists() {
            return candidate;
        }
    }
    match work_dir.or(profile_dir) {
        Some(base) => base.join(path),
        None => path.to_path_buf(),
    }
}

/// Every snapshot file in the VM directory, read into a row.
///
/// The directory rather than "one per machine": a snapshot survives the machine
/// it came from — its profile can be deleted, renamed or never have existed on
/// this computer — and a half-gigabyte file nobody can see is a file nobody can
/// delete. The row says which machine it is of, from the snapshot's own copy of
/// the profile.
pub fn snapshot_rows(vm_dir: &Path) -> Vec<crate::snapshots::SnapshotRow> {
    let Ok(entries) = std::fs::read_dir(vm_dir) else {
        return Vec::new();
    };
    let mut rows: Vec<crate::snapshots::SnapshotRow> = entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| {
            path.is_file()
                && path
                    .extension()
                    .is_some_and(|ext| ext == vm_snapshot::EXTENSION)
        })
        .map(|path| crate::snapshots::read(&path))
        .collect();
    // Newest first: the one just taken is the one being looked for.
    rows.sort_by(|a, b| {
        let key = |row: &crate::snapshots::SnapshotRow| {
            row.facts.as_ref().map(|f| f.created_unix).unwrap_or(0)
        };
        key(b)
            .cmp(&key(a))
            .then_with(|| a.file_name.cmp(&b.file_name))
    });
    rows
}

/// Files a delete would remove, in order. Disks outside the VM directory are
/// deliberately left alone: a profile may point at a shared base image.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct DeletePlan {
    pub remove: Vec<PathBuf>,
    pub kept_outside_vm_dir: Vec<PathBuf>,
}

pub fn plan_delete(entry: &VmEntry, vm_dir: &Path) -> DeletePlan {
    let mut plan = DeletePlan::default();
    plan.remove.push(entry.profile_path.clone());
    for disk in &entry.disks {
        if !disk.exists {
            continue;
        }
        if disk.resolved.starts_with(vm_dir) {
            plan.remove.push(disk.resolved.clone());
        } else {
            plan.kept_outside_vm_dir.push(disk.resolved.clone());
        }
    }
    // Byproducts of the install flow and our own logs, best-effort — plus the
    // suspended session, which is bound to this machine's disks and useless the
    // moment they are gone (ADR-0006).
    for byproduct in [
        vm_dir.join(format!("{}-install.log", entry.name)),
        vm_dir.join(format!("{}-run.log", entry.name)),
        vm_dir.join(format!("{}.install-initrd.img", entry.name)),
        crate::snapshots::path_for(entry),
    ] {
        if byproduct.exists() {
            plan.remove.push(byproduct);
        }
    }
    plan
}

/// Deletes a VM. `busy` and `typed_name` are the two guards required by
/// GUI-1604 — a running VM is never deleted, and the user must retype the name.
pub fn delete(
    entry: &VmEntry,
    vm_dir: &Path,
    busy: bool,
    typed_name: &str,
) -> Result<Vec<PathBuf>, DeleteError> {
    if busy {
        return Err(DeleteError::Busy(entry.name.clone()));
    }
    if typed_name.trim() != entry.name {
        return Err(DeleteError::NameMismatch(entry.name.clone()));
    }

    let plan = plan_delete(entry, vm_dir);
    let mut removed = Vec::new();
    for path in plan.remove {
        match std::fs::remove_file(&path) {
            Ok(()) => removed.push(path),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(source) => return Err(DeleteError::Remove { path, source }),
        }
    }
    Ok(removed)
}

/// Rewrites `memory_mib`/`vcpus` in a profile the CLI just wrote, so the
/// wizard's choices survive the install (the CLI hardcodes 2048 MiB / 2 vCPUs).
pub fn apply_resources(profile: &Path, memory_mib: u64, vcpus: u32) -> Result<(), String> {
    let text = std::fs::read_to_string(profile).map_err(|e| e.to_string())?;
    let mut cfg = VmConfig::from_toml(&text).map_err(|e| e.to_string())?;
    if cfg.memory_mib == memory_mib && cfg.vcpus == vcpus {
        return Ok(());
    }
    cfg.memory_mib = memory_mib;
    cfg.vcpus = vcpus;
    let out = toml::to_string_pretty(&cfg).map_err(|e| e.to_string())?;
    std::fs::write(profile, out).map_err(|e| e.to_string())
}

// ---------------------------------------------------------------------------
// The Disks view: rows, attach/detach
// ---------------------------------------------------------------------------

/// One profile that attaches a disk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiskAttachment {
    pub vm: String,
    pub profile: PathBuf,
    /// The `[[disk]]` path exactly as written in the profile — the key a
    /// detach uses, so rewrites never guess at path equivalence.
    pub declared: PathBuf,
    pub writable: bool,
}

/// One row of the Disks view.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiskRow {
    /// Resolved path (absolute wherever the profile allowed resolving it).
    pub path: PathBuf,
    pub file_name: String,
    pub exists: bool,
    pub apparent_bytes: u64,
    pub allocated_bytes: Option<u64>,
    /// VMs whose profiles attach this disk.
    pub attachments: Vec<DiskAttachment>,
    /// A `.nvram` UEFI variable-store sidecar sits next to the image.
    pub nvram: bool,
    /// Partition summary from `disk-image` — or why inspection refused the
    /// image (a corrupt table is worth surfacing, not hiding).
    pub summary: Result<String, String>,
}

/// Builds the Disks view rows: every disk the profiles reference, plus every
/// loose `*.raw` in the VM directory nothing references (installer leftovers,
/// hand-made images). ISOs attached via `[cdrom]` are media, not disks — they
/// stay out.
pub fn disk_rows(vms: &[VmEntry], vm_dir: &Path) -> Vec<DiskRow> {
    let mut rows: Vec<DiskRow> = Vec::new();
    // Identity for dedup: canonical where possible, the resolved path itself
    // otherwise (a missing disk cannot be canonicalized).
    let mut seen: Vec<PathBuf> = Vec::new();
    let identity = |path: &Path| -> PathBuf {
        std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
    };

    for vm in vms {
        for disk in &vm.disks {
            let id = identity(&disk.resolved);
            let attachment = DiskAttachment {
                vm: vm.name.clone(),
                profile: vm.profile_path.clone(),
                declared: disk.declared.clone(),
                writable: disk.writable,
            };
            if let Some(at) = seen.iter().position(|s| *s == id) {
                rows[at].attachments.push(attachment);
                continue;
            }
            seen.push(id);
            rows.push(row_for(&disk.resolved, vec![attachment]));
        }
    }

    // Loose *.raw files in the VM directory.
    if let Ok(entries) = std::fs::read_dir(vm_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_file() || path.extension().is_none_or(|ext| ext != "raw") {
                continue;
            }
            let id = identity(&path);
            if seen.contains(&id) {
                continue;
            }
            seen.push(id);
            rows.push(row_for(&path, Vec::new()));
        }
    }

    rows.sort_by(|a, b| a.file_name.cmp(&b.file_name).then(a.path.cmp(&b.path)));
    rows
}

fn row_for(path: &Path, attachments: Vec<DiskAttachment>) -> DiskRow {
    let meta = std::fs::metadata(path).ok();
    let exists = meta.is_some();
    let summary = if exists {
        disk_image::inspect_disk(path)
            .map(|report| report.partition_summary())
            .map_err(|e| e.to_string())
    } else {
        Err("the image file is missing".to_string())
    };
    DiskRow {
        file_name: path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| path.display().to_string()),
        exists,
        apparent_bytes: meta.map(|m| m.len()).unwrap_or(0),
        allocated_bytes: exists.then(|| disk_image::allocated_bytes(path)).flatten(),
        attachments,
        nvram: disk_image::existing_nvram_sidecar(path).is_some(),
        summary,
        path: path.to_path_buf(),
    }
}

/// Attaches `disk` to the profile as a writable `[[disk]]`, through
/// `control-api` types (parse → mutate → re-validate → write; never a string
/// edit). Refuses a duplicate attachment.
pub fn attach_disk(profile: &Path, disk: &Path) -> Result<(), String> {
    let text = std::fs::read_to_string(profile).map_err(|e| e.to_string())?;
    let mut cfg = VmConfig::from_toml(&text).map_err(|e| e.to_string())?;
    let profile_dir = profile.parent().map(Path::to_path_buf);
    let duplicate = cfg
        .disks
        .iter()
        .any(|d| d.path == disk || resolve(&d.path, None, profile_dir.as_deref()) == disk);
    if duplicate {
        return Err(format!(
            "{} is already attached to '{}'",
            disk.display(),
            cfg.name
        ));
    }
    cfg.disks.push(DiskSection {
        path: disk.to_path_buf(),
        writable: true,
    });
    write_validated(profile, &cfg)
}

/// Removes the `[[disk]]` entry whose path is exactly `declared` (the string
/// the profile carries, resolved-agnostic).
pub fn detach_disk(profile: &Path, declared: &Path) -> Result<(), String> {
    let text = std::fs::read_to_string(profile).map_err(|e| e.to_string())?;
    let mut cfg = VmConfig::from_toml(&text).map_err(|e| e.to_string())?;
    let before = cfg.disks.len();
    cfg.disks.retain(|d| d.path != declared);
    if cfg.disks.len() == before {
        return Err(format!(
            "'{}' has no disk entry {}",
            cfg.name,
            declared.display()
        ));
    }
    write_validated(profile, &cfg)
}

/// Serializes and **re-validates** a config before it replaces a working
/// profile — a mutation that control-api would refuse must never reach disk.
fn write_validated(profile: &Path, cfg: &VmConfig) -> Result<(), String> {
    let out = toml::to_string_pretty(cfg).map_err(|e| e.to_string())?;
    VmConfig::from_toml(&out).map_err(|e| format!("the change is invalid: {e}"))?;
    std::fs::write(profile, out).map_err(|e| e.to_string())
}

/// Names must be safe as both a file stem and a VM/hostname.
pub fn validate_name(name: &str) -> Result<(), String> {
    if name.is_empty() {
        return Err("name must not be empty".into());
    }
    if name.len() > 40 {
        return Err("name must be 40 characters or fewer".into());
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
    {
        return Err("use letters, digits, '-', '_' or '.' only".into());
    }
    if !name.starts_with(|c: char| c.is_ascii_alphanumeric()) {
        return Err("name must start with a letter or digit".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const PROFILE: &str = r#"
name = "{NAME}"
memory_mib = 2048
vcpus = 2

[boot]
mode = "direct-linux"
kernel = "artifacts/bootstrap/vmlinuz"
initramfs = "artifacts/bootstrap/initrd.img"
cmdline = "console=ttyS0 root=UUID=deadbeef rw"

[[disk]]
path = "{DISK}"
writable = true

[network]
backend = "tap"
interface = "entangled0"
"#;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "entangled-manager-tests/{tag}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("test temp dir");
        dir
    }

    fn write_vm(dir: &Path, name: &str, disk_bytes: usize) -> PathBuf {
        let disk = dir.join(format!("{name}.raw"));
        std::fs::write(&disk, vec![0u8; disk_bytes]).expect("disk");
        let profile = dir.join(format!("{name}.toml"));
        std::fs::write(
            &profile,
            PROFILE
                .replace("{NAME}", name)
                .replace("{DISK}", &disk.display().to_string().replace('\\', "\\\\")),
        )
        .expect("profile");
        profile
    }

    #[test]
    fn scans_profiles_sorted_and_reports_broken_ones() {
        let dir = temp_dir("scan");
        write_vm(&dir, "zeta", 4096);
        write_vm(&dir, "alpha", 2048);
        std::fs::write(dir.join("broken.toml"), "name = 3\n").expect("broken");
        std::fs::write(dir.join("notes.txt"), "ignored").expect("txt");
        std::fs::write(dir.join("invalid.toml"), PROFILE.replace("2048", "1")).expect("invalid");

        let scan = scan(&dir, None).expect("scan");
        assert_eq!(
            scan.vms.iter().map(|v| v.name.as_str()).collect::<Vec<_>>(),
            vec!["alpha", "zeta"]
        );
        assert_eq!(scan.vms[0].memory_mib, 2048);
        assert_eq!(scan.vms[0].vcpus, 2);
        assert_eq!(scan.vms[0].display, (1920, 1080));
        assert_eq!(scan.vms[0].network_interface.as_deref(), Some("entangled0"));
        assert_eq!(scan.vms[0].disks.len(), 1);
        assert!(scan.vms[0].disks[0].exists);
        assert_eq!(scan.vms[0].size_bytes(), 2048);

        let problems: Vec<_> = scan
            .problems
            .iter()
            .map(|p| p.path.file_name().and_then(|n| n.to_str()).unwrap_or(""))
            .collect();
        assert_eq!(problems, vec!["broken.toml", "invalid.toml"]);
    }

    #[test]
    fn missing_directory_is_an_error() {
        let dir = temp_dir("scan-missing");
        assert!(matches!(
            scan(&dir.join("nope"), None),
            Err(DiscoveryError::ReadDir { .. })
        ));
    }

    #[test]
    fn relative_disks_resolve_against_the_work_dir() {
        let dir = temp_dir("scan-relative");
        let work = dir.join("work");
        std::fs::create_dir_all(work.join("images")).expect("images");
        std::fs::write(work.join("images/rel.raw"), vec![0u8; 512]).expect("disk");
        std::fs::write(
            dir.join("rel.toml"),
            PROFILE
                .replace("{NAME}", "rel")
                .replace("{DISK}", "images/rel.raw"),
        )
        .expect("profile");

        let scan = scan(&dir, Some(&work)).expect("scan");
        assert_eq!(scan.vms.len(), 1);
        assert!(scan.vms[0].disks[0].exists);
        assert_eq!(scan.vms[0].disks[0].resolved, work.join("images/rel.raw"));
    }

    #[test]
    fn delete_refuses_while_busy_and_on_name_mismatch() {
        let dir = temp_dir("delete-guards");
        let profile = write_vm(&dir, "guarded", 1024);
        let entry = load_profile(&profile, None).expect("profile");

        assert!(matches!(
            delete(&entry, &dir, true, "guarded"),
            Err(DeleteError::Busy(_))
        ));
        assert!(matches!(
            delete(&entry, &dir, false, "guardedx"),
            Err(DeleteError::NameMismatch(_))
        ));
        assert!(matches!(
            delete(&entry, &dir, false, ""),
            Err(DeleteError::NameMismatch(_))
        ));
        assert!(
            profile.exists(),
            "nothing may be removed by a refused delete"
        );
        assert!(dir.join("guarded.raw").exists());
    }

    #[test]
    fn delete_removes_profile_disk_and_byproducts() {
        let dir = temp_dir("delete-ok");
        let profile = write_vm(&dir, "doomed", 1024);
        std::fs::write(dir.join("doomed-run.log"), "log").expect("log");
        std::fs::write(dir.join("doomed.install-initrd.img"), "img").expect("initrd");
        let entry = load_profile(&profile, None).expect("profile");

        // A trailing newline from the text field must still match.
        let removed = delete(&entry, &dir, false, "doomed\n").expect("delete");
        assert_eq!(removed.len(), 4, "removed: {removed:?}");
        assert!(!profile.exists());
        assert!(!dir.join("doomed.raw").exists());
        assert!(!dir.join("doomed-run.log").exists());
    }

    #[test]
    fn delete_keeps_disks_outside_the_vm_directory() {
        let dir = temp_dir("delete-outside");
        let elsewhere = dir.join("shared");
        std::fs::create_dir_all(&elsewhere).expect("dir");
        let disk = elsewhere.join("base.raw");
        std::fs::write(&disk, vec![0u8; 64]).expect("disk");
        let vm_dir = dir.join("vms");
        std::fs::create_dir_all(&vm_dir).expect("dir");
        let profile = vm_dir.join("shared-base.toml");
        std::fs::write(
            &profile,
            PROFILE
                .replace("{NAME}", "shared-base")
                .replace("{DISK}", &disk.display().to_string().replace('\\', "\\\\")),
        )
        .expect("profile");
        let entry = load_profile(&profile, None).expect("profile");

        let plan = plan_delete(&entry, &vm_dir);
        assert_eq!(plan.kept_outside_vm_dir, vec![disk.clone()]);
        let removed = delete(&entry, &vm_dir, false, "shared-base").expect("delete");
        assert_eq!(removed, vec![profile]);
        assert!(disk.exists(), "a disk outside the VM directory is kept");
    }

    #[test]
    fn apply_resources_rewrites_only_memory_and_vcpus() {
        let dir = temp_dir("apply-resources");
        let profile = write_vm(&dir, "resized", 128);
        apply_resources(&profile, 3072, 6).expect("apply");

        let entry = load_profile(&profile, None).expect("reload");
        assert_eq!(entry.memory_mib, 3072);
        assert_eq!(entry.vcpus, 6);
        let text = std::fs::read_to_string(&profile).expect("read");
        assert!(
            text.contains("root=UUID=deadbeef"),
            "boot section preserved"
        );
        assert!(text.contains("entangled0"), "network section preserved");
    }

    #[test]
    fn disk_rows_merge_profile_disks_and_loose_raws() {
        let dir = temp_dir("disk-rows");
        write_vm(&dir, "alpha", 4096);
        std::fs::write(dir.join("alpha.nvram"), b"vars").expect("nvram");
        std::fs::write(dir.join("loose.raw"), vec![0u8; 2048]).expect("loose");
        std::fs::write(dir.join("notes.txt"), "ignored").expect("txt");

        let scan = scan(&dir, None).expect("scan");
        assert_eq!(scan.disks.len(), 2, "{:?}", scan.disks);

        let alpha = scan
            .disks
            .iter()
            .find(|d| d.file_name == "alpha.raw")
            .expect("alpha row");
        assert!(alpha.exists);
        assert!(alpha.nvram, "sidecar badge");
        assert_eq!(alpha.apparent_bytes, 4096);
        assert_eq!(alpha.attachments.len(), 1);
        assert_eq!(alpha.attachments[0].vm, "alpha");
        assert!(alpha.attachments[0].writable);
        // A blank image is a summary, not an error.
        assert_eq!(alpha.summary.as_deref(), Ok("blank — no partition table"));

        let loose = scan
            .disks
            .iter()
            .find(|d| d.file_name == "loose.raw")
            .expect("loose row");
        assert!(loose.attachments.is_empty());
        assert!(!loose.nvram);
    }

    #[test]
    fn a_disk_shared_by_two_vms_is_one_row_with_two_attachments() {
        let dir = temp_dir("disk-rows-shared");
        let disk = dir.join("shared.raw");
        std::fs::write(&disk, vec![0u8; 1024]).expect("disk");
        for name in ["one", "two"] {
            std::fs::write(
                dir.join(format!("{name}.toml")),
                PROFILE
                    .replace("{NAME}", name)
                    .replace("{DISK}", &disk.display().to_string().replace('\\', "\\\\")),
            )
            .expect("profile");
        }

        let scan = scan(&dir, None).expect("scan");
        assert_eq!(scan.disks.len(), 1, "{:?}", scan.disks);
        let vms: Vec<_> = scan.disks[0]
            .attachments
            .iter()
            .map(|a| a.vm.as_str())
            .collect();
        assert_eq!(vms, vec!["one", "two"]);
    }

    #[test]
    fn attach_and_detach_rewrite_the_profile_through_control_api() {
        let dir = temp_dir("attach-detach");
        let profile = write_vm(&dir, "editable", 1024);
        let extra = dir.join("extra.raw");
        std::fs::write(&extra, vec![0u8; 512]).expect("extra disk");

        attach_disk(&profile, &extra).expect("attach");
        let entry = load_profile(&profile, None).expect("reload");
        assert_eq!(entry.disks.len(), 2);
        assert_eq!(entry.disks[1].declared, extra);
        assert!(entry.disks[1].writable, "attached disks default writable");
        // The rest of the profile survived the rewrite.
        let text = std::fs::read_to_string(&profile).expect("read");
        assert!(text.contains("root=UUID=deadbeef"), "boot section kept");

        // Attaching the same disk again is refused.
        let error = attach_disk(&profile, &extra).expect_err("duplicate");
        assert!(error.contains("already attached"), "{error}");

        detach_disk(&profile, &extra).expect("detach");
        let entry = load_profile(&profile, None).expect("reload");
        assert_eq!(entry.disks.len(), 1);

        // Detaching a path the profile does not carry is a readable error.
        let error = detach_disk(&profile, &extra).expect_err("unknown");
        assert!(error.contains("no disk entry"), "{error}");
    }

    #[test]
    fn formats_sizes_and_validates_names() {
        assert_eq!(format_bytes(0), "0 B");
        assert_eq!(format_bytes(1023), "1023 B");
        assert_eq!(format_bytes(1024), "1.0 KiB");
        assert_eq!(format_bytes(16 * 1024 * 1024 * 1024), "16.0 GiB");
        assert_eq!(format_bytes(700 * 1024 * 1024), "700 MiB");

        assert!(validate_name("debian-demo").is_ok());
        assert!(validate_name("vm.1_a").is_ok());
        assert!(validate_name("").is_err());
        assert!(validate_name("-leading").is_err());
        assert!(validate_name("with space").is_err());
        assert!(validate_name("../escape").is_err());
        assert!(validate_name("sub/dir").is_err());
        assert!(validate_name(&"x".repeat(41)).is_err());
    }
}
