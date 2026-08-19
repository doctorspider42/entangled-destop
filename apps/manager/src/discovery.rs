//! VM discovery: turn a directory of `*.toml` profiles into cards the UI can
//! draw, and delete a VM again (profile + disks) with guards.

use std::path::{Path, PathBuf};

use control_api::VmConfig;
use thiserror::Error;

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
    pub display: (u32, u32),
    pub network_interface: Option<String>,
    pub disks: Vec<DiskInfo>,
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
                size_bytes: meta.as_ref().map(|m| m.len()).unwrap_or(0),
                allocated_bytes: meta.as_ref().and_then(allocated_bytes),
                resolved,
            }
        })
        .collect();

    VmEntry {
        name: cfg.name.clone(),
        profile_path: path.to_path_buf(),
        memory_mib: cfg.memory_mib,
        vcpus: cfg.vcpus,
        display: (cfg.display.width, cfg.display.height),
        network_interface: cfg.network.as_ref().map(|n| n.interface.clone()),
        disks,
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

/// Allocated size in bytes. Unix reports 512-byte blocks; other platforms have
/// no portable equivalent, so the UI just omits the number there.
#[cfg(unix)]
fn allocated_bytes(meta: &std::fs::Metadata) -> Option<u64> {
    use std::os::unix::fs::MetadataExt as _;
    Some(meta.blocks().saturating_mul(512))
}

#[cfg(not(unix))]
fn allocated_bytes(_meta: &std::fs::Metadata) -> Option<u64> {
    None
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
    // Byproducts of the install flow and our own logs, best-effort.
    for byproduct in [
        vm_dir.join(format!("{}-install.log", entry.name)),
        vm_dir.join(format!("{}-run.log", entry.name)),
        vm_dir.join(format!("{}.install-initrd.img", entry.name)),
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

/// Binary-prefix formatting for the card labels.
pub fn format_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else if value >= 100.0 {
        format!("{value:.0} {}", UNITS[unit])
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
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
