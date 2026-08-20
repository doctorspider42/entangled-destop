//! Which VM profiles reference a disk image — the guard behind
//! `entangled disk rm` and the manager's delete/detach actions.
//!
//! A "reference" is a `[[disk]]` or `[cdrom]` entry that resolves to the same
//! file (relative entries resolve against the profile's directory, then the
//! current working directory — the same order the CLI uses at run time). A
//! profile that fails to parse is still checked *conservatively*: if its raw
//! text names the disk's file name, it counts as a reference, because "we
//! could not prove it is unused" must never delete data.

use std::io;
use std::path::{Path, PathBuf};

use control_api::VmConfig;
use thiserror::Error;

use crate::ops;

/// One profile that references the disk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProfileRef {
    pub profile: PathBuf,
    /// The VM name, when the profile parses; `None` marks the conservative
    /// text-only match on a broken profile.
    pub vm: Option<String>,
}

impl std::fmt::Display for ProfileRef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.vm {
            Some(vm) => write!(f, "{} (VM '{vm}')", self.profile.display()),
            None => write!(f, "{} (unparseable profile)", self.profile.display()),
        }
    }
}

/// The directories `disk rm` scans by default: the disk's own directory plus
/// the manager's VM directory (from `manager.toml`, or its default
/// `~/entangled-vms`), deduplicated.
pub fn default_scan_dirs(disk: &Path) -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = Vec::new();
    if let Some(parent) = disk.parent() {
        if !parent.as_os_str().is_empty() {
            dirs.push(parent.to_path_buf());
        } else {
            dirs.push(PathBuf::from("."));
        }
    }
    if let Some(vm_dir) = manager_vm_dir() {
        if !dirs.iter().any(|d| same_dir(d, &vm_dir)) {
            dirs.push(vm_dir);
        }
    }
    dirs
}

/// Every profile in `dirs` that references `disk` (which must exist). Missing
/// or unreadable directories are skipped — a guard that fails open on *scan*
/// errors would be one thing, but a directory that does not exist holds no
/// profiles either way.
pub fn find_references(disk: &Path, dirs: &[PathBuf]) -> io::Result<Vec<ProfileRef>> {
    let target = std::fs::canonicalize(disk)?;
    let file_name = disk.file_name().map(|n| n.to_string_lossy().into_owned());

    let mut seen_profiles = std::collections::HashSet::new();
    let mut out = Vec::new();
    for dir in dirs {
        let Ok(entries) = std::fs::read_dir(dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_file() || path.extension().is_none_or(|ext| ext != "toml") {
                continue;
            }
            let identity = std::fs::canonicalize(&path).unwrap_or_else(|_| path.clone());
            if !seen_profiles.insert(identity) {
                continue;
            }
            let Ok(text) = std::fs::read_to_string(&path) else {
                continue;
            };
            match VmConfig::from_toml(&text) {
                Ok(cfg) => {
                    if config_references(&cfg, &path, &target) {
                        out.push(ProfileRef {
                            profile: path,
                            vm: Some(cfg.name),
                        });
                    }
                }
                Err(_) => {
                    // Conservative: a profile we cannot parse but which names
                    // the file may still mean it.
                    if let Some(name) = &file_name {
                        if text.contains(name.as_str()) {
                            out.push(ProfileRef {
                                profile: path,
                                vm: None,
                            });
                        }
                    }
                }
            }
        }
    }
    out.sort_by(|a, b| a.profile.cmp(&b.profile));
    Ok(out)
}

/// True when any disk/cdrom entry of `cfg` resolves to `target` (canonical).
fn config_references(cfg: &VmConfig, profile: &Path, target: &Path) -> bool {
    let profile_dir = profile.parent();
    let mut candidates: Vec<&Path> = cfg.disks.iter().map(|d| d.path.as_path()).collect();
    if let Some(cdrom) = &cfg.cdrom {
        candidates.push(cdrom.path.as_path());
    }
    candidates
        .into_iter()
        .any(|p| resolves_to(p, profile_dir, target))
}

/// Does `declared` (a path as written in a profile) point at `target`?
/// Relative entries are tried against the profile directory and the current
/// working directory — the two bases the CLI itself uses.
pub(crate) fn resolves_to(declared: &Path, profile_dir: Option<&Path>, target: &Path) -> bool {
    let mut candidates: Vec<PathBuf> = Vec::new();
    if declared.is_absolute() {
        candidates.push(declared.to_path_buf());
    } else {
        if let Some(dir) = profile_dir {
            candidates.push(dir.join(declared));
        }
        if let Ok(cwd) = std::env::current_dir() {
            candidates.push(cwd.join(declared));
        }
    }
    candidates
        .into_iter()
        .any(|c| std::fs::canonicalize(&c).is_ok_and(|c| c == *target))
}

fn same_dir(a: &Path, b: &Path) -> bool {
    match (std::fs::canonicalize(a), std::fs::canonicalize(b)) {
        (Ok(a), Ok(b)) => a == b,
        _ => a == b,
    }
}

/// The manager's VM directory: `vm_dir` from its `manager.toml` when set,
/// otherwise the same `~/entangled-vms` default the manager uses. `None` only
/// when the host has no home directory at all.
pub fn manager_vm_dir() -> Option<PathBuf> {
    if let Some(config) = manager_config_path() {
        if let Ok(text) = std::fs::read_to_string(&config) {
            // A partial, tolerant read: only vm_dir matters here, and a future
            // manager may add keys this crate has never heard of.
            #[derive(serde::Deserialize, Default)]
            #[serde(default)]
            struct Partial {
                vm_dir: Option<PathBuf>,
            }
            if let Ok(partial) = toml::from_str::<Partial>(&text) {
                if let Some(dir) = partial.vm_dir {
                    return Some(dir);
                }
            }
        }
    }
    home_dir().map(|home| home.join("entangled-vms"))
}

/// `$XDG_CONFIG_HOME/entangled/manager.toml`, `%APPDATA%\entangled\manager.toml`
/// on Windows, `$HOME/.config/…` otherwise — the same resolution the manager's
/// own settings module performs.
fn manager_config_path() -> Option<PathBuf> {
    if let Some(dir) = non_empty_env("XDG_CONFIG_HOME") {
        return Some(PathBuf::from(dir).join("entangled").join("manager.toml"));
    }
    #[cfg(windows)]
    if let Some(dir) = non_empty_env("APPDATA") {
        return Some(PathBuf::from(dir).join("entangled").join("manager.toml"));
    }
    home_dir().map(|home| home.join(".config").join("entangled").join("manager.toml"))
}

fn home_dir() -> Option<PathBuf> {
    non_empty_env("HOME")
        .or_else(|| non_empty_env("USERPROFILE"))
        .map(PathBuf::from)
}

fn non_empty_env(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|v| !v.trim().is_empty())
}

// ---------------------------------------------------------------------------
// disk rm
// ---------------------------------------------------------------------------

#[derive(Debug, Error)]
pub enum RemoveError {
    #[error("{0} does not exist or is not a regular file")]
    Missing(String),

    #[error(
        "{disk} is still referenced by: {list} — detach it from those profiles \
         (or delete the VMs) first, or pass --force to remove it anyway"
    )]
    Referenced { disk: String, list: String },

    #[error("cannot scan for references to {disk}: {source}")]
    Scan {
        disk: String,
        #[source]
        source: io::Error,
    },

    #[error("cannot remove {path}: {source}")]
    Remove {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
}

/// What [`remove_disk`] deleted.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RemoveOutcome {
    /// The image and, when present, its `.nvram` sidecar.
    pub removed: Vec<PathBuf>,
    /// The references that `--force` overrode (empty without `--force`).
    pub overridden: Vec<ProfileRef>,
}

/// Removes a disk image and its `.nvram` sidecar, refusing while any profile
/// in `dirs` references the disk unless `force` is set.
pub fn remove_disk(
    disk: &Path,
    force: bool,
    dirs: &[PathBuf],
) -> Result<RemoveOutcome, RemoveError> {
    if !disk.is_file() {
        return Err(RemoveError::Missing(disk.display().to_string()));
    }
    let references = find_references(disk, dirs).map_err(|source| RemoveError::Scan {
        disk: disk.display().to_string(),
        source,
    })?;
    if !references.is_empty() && !force {
        let list = references
            .iter()
            .map(ProfileRef::to_string)
            .collect::<Vec<_>>()
            .join(", ");
        return Err(RemoveError::Referenced {
            disk: disk.display().to_string(),
            list,
        });
    }

    // Find the sidecar before deleting the disk (the lookup is by path, but
    // the intent reads better this way).
    let nvram = ops::existing_nvram_sidecar(disk);
    let mut outcome = RemoveOutcome {
        removed: Vec::new(),
        overridden: references,
    };
    std::fs::remove_file(disk).map_err(|source| RemoveError::Remove {
        path: disk.to_path_buf(),
        source,
    })?;
    outcome.removed.push(disk.to_path_buf());
    if let Some(nvram) = nvram {
        std::fs::remove_file(&nvram).map_err(|source| RemoveError::Remove {
            path: nvram.clone(),
            source,
        })?;
        outcome.removed.push(nvram);
    }
    Ok(outcome)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixtures::temp_dir;

    fn profile_text(name: &str, disk: &Path) -> String {
        format!(
            r#"
name = "{name}"
memory_mib = 2048
vcpus = 2

[boot]
mode = "direct-linux"
kernel = "artifacts/bootstrap/vmlinuz"

[[disk]]
path = "{}"
writable = true
"#,
            disk.display().to_string().replace('\\', "\\\\")
        )
    }

    #[test]
    fn finds_absolute_and_relative_references() {
        let dir = temp_dir("refs-basic");
        let disk = dir.join("demo.raw");
        std::fs::write(&disk, vec![0u8; 512]).unwrap();

        // Absolute reference.
        std::fs::write(dir.join("abs.toml"), profile_text("abs-vm", &disk)).unwrap();
        // Relative reference (resolves against the profile's own directory).
        std::fs::write(
            dir.join("rel.toml"),
            profile_text("rel-vm", Path::new("demo.raw")),
        )
        .unwrap();
        // A profile referencing some other disk.
        std::fs::write(
            dir.join("other.toml"),
            profile_text("other-vm", Path::new("other.raw")),
        )
        .unwrap();
        // Not a profile at all, but it names the file: conservative match.
        std::fs::write(dir.join("broken.toml"), "this mentions demo.raw =").unwrap();

        let refs = find_references(&disk, std::slice::from_ref(&dir)).expect("scan");
        let vms: Vec<_> = refs.iter().map(|r| r.vm.clone()).collect();
        assert_eq!(refs.len(), 3, "{refs:?}");
        assert!(vms.contains(&Some("abs-vm".to_string())));
        assert!(vms.contains(&Some("rel-vm".to_string())));
        assert!(vms.contains(&None), "broken profile counted conservatively");
    }

    #[test]
    fn scanning_the_same_directory_twice_reports_each_profile_once() {
        let dir = temp_dir("refs-dedup");
        let disk = dir.join("demo.raw");
        std::fs::write(&disk, vec![0u8; 512]).unwrap();
        std::fs::write(dir.join("a.toml"), profile_text("a", &disk)).unwrap();

        let refs = find_references(&disk, &[dir.clone(), dir.clone()]).expect("scan");
        assert_eq!(refs.len(), 1);
    }

    #[test]
    fn rm_refuses_referenced_disks_and_force_overrides() {
        let dir = temp_dir("refs-rm");
        let disk = dir.join("guarded.raw");
        std::fs::write(&disk, vec![0u8; 512]).unwrap();
        std::fs::write(dir.join("guarded.nvram"), b"vars").unwrap();
        std::fs::write(dir.join("owner.toml"), profile_text("owner", &disk)).unwrap();

        let error = remove_disk(&disk, false, std::slice::from_ref(&dir)).expect_err("guard");
        assert!(matches!(error, RemoveError::Referenced { .. }));
        assert!(error.to_string().contains("owner"), "{error}");
        assert!(disk.exists(), "a refused rm removes nothing");
        assert!(dir.join("guarded.nvram").exists());

        let outcome = remove_disk(&disk, true, std::slice::from_ref(&dir)).expect("force");
        assert_eq!(outcome.removed.len(), 2, "{outcome:?}");
        assert!(!disk.exists());
        assert!(!dir.join("guarded.nvram").exists(), "sidecar travels");
        assert_eq!(outcome.overridden.len(), 1);
    }

    #[test]
    fn rm_of_an_unreferenced_disk_removes_it_and_the_sidecar() {
        let dir = temp_dir("refs-rm-free");
        let disk = dir.join("loose.raw");
        std::fs::write(&disk, vec![0u8; 512]).unwrap();
        std::fs::write(dir.join("loose.nvram"), b"vars").unwrap();

        let outcome = remove_disk(&disk, false, std::slice::from_ref(&dir)).expect("rm");
        assert_eq!(outcome.removed, vec![disk.clone(), dir.join("loose.nvram")]);
        assert!(outcome.overridden.is_empty());
        assert!(matches!(
            remove_disk(&disk, false, std::slice::from_ref(&dir)),
            Err(RemoveError::Missing(_))
        ));
    }

    #[test]
    fn cdrom_entries_count_as_references() {
        let dir = temp_dir("refs-cdrom");
        let iso = dir.join("boot.iso");
        std::fs::write(&iso, vec![0u8; 512]).unwrap();
        std::fs::write(
            dir.join("iso-boot.toml"),
            format!(
                r#"
name = "iso-boot"
memory_mib = 2048
vcpus = 2
transport = "pci"

[boot]
mode = "uefi"
firmware = "artifacts/firmware/CLOUDHV.fd"

[cdrom]
path = "{}"
"#,
                iso.display().to_string().replace('\\', "\\\\")
            ),
        )
        .unwrap();

        let refs = find_references(&iso, std::slice::from_ref(&dir)).expect("scan");
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0].vm.as_deref(), Some("iso-boot"));
    }

    #[test]
    fn missing_scan_directories_are_skipped_not_fatal() {
        let dir = temp_dir("refs-missing-dir");
        let disk = dir.join("d.raw");
        std::fs::write(&disk, vec![0u8; 512]).unwrap();
        let refs =
            find_references(&disk, &[dir.join("nope"), dir.clone()]).expect("scan tolerates");
        assert!(refs.is_empty());
    }
}
