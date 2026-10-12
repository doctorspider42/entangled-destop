//! Renaming a machine: its name, its profile's file name, and the files named
//! after it.
//!
//! A machine's name is in more places than its profile's `name =` line: the
//! profile is `<name>.toml`, `install` names the disk `<name>.raw` and the
//! UEFI store `<name>.nvram`, and the snapshot, the logs and the manager's
//! card picture are all derived from it. A rename that changed only the line
//! would leave a machine called `lab` living in `ubuntu.raw` — and a second
//! machine created as `ubuntu` would then collide with it.
//!
//! What moves:
//!
//! * the profile, to `<new>.toml` in the same directory;
//! * every writable disk whose file name starts with the old name, to the same
//!   name with the new prefix, in the same directory — **unless another profile
//!   references it**, in which case it stays exactly where it is: renaming a
//!   disk out from under another machine would break that machine;
//! * a moving disk's `.nvram` sidecar, with it.
//!
//! All of these are same-directory renames, so nothing is copied and the
//! operation is close to atomic per file; a failure part-way renames back
//! everything already renamed. The MAC address is kept on purpose: the guest's
//! network configuration (and a DHCP reservation outside it) may be keyed to
//! it. A suspended machine must not be renamed — its snapshot fingerprints the
//! disk paths (ADR-0006) — and the caller refuses that before it gets here.

use std::path::{Path, PathBuf};

use control_api::VmConfig;
use thiserror::Error;

use crate::ops;
use crate::refs;

#[derive(Debug, Error)]
pub enum RenameError {
    #[error("cannot read the profile {path}: {message}")]
    Profile { path: PathBuf, message: String },

    #[error("{0} already exists — pick another name")]
    Exists(String),

    #[error("cannot rename {from} to {to}: {message}. Everything already renamed was put back")]
    Io {
        from: PathBuf,
        to: PathBuf,
        message: String,
    },

    #[error("cannot write the renamed profile: {0}. Everything was put back")]
    Write(String),
}

/// What a rename did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenameOutcome {
    /// The profile's new path.
    pub profile: PathBuf,
    /// `(old, new)` for every file renamed, the profile included.
    pub renamed: Vec<(PathBuf, PathBuf)>,
    /// Disks named after the machine that stayed put because another profile
    /// uses them too.
    pub shared_disks: Vec<PathBuf>,
}

fn resolve(declared: &Path, profile_dir: &Path) -> PathBuf {
    if declared.is_absolute() {
        declared.to_path_buf()
    } else {
        profile_dir.join(declared)
    }
}

/// Renames the machine described by `profile` to `new_name`. See the module
/// docs for what moves.
pub fn rename_machine(profile: &Path, new_name: &str) -> Result<RenameOutcome, RenameError> {
    let profile_err = |message: String| RenameError::Profile {
        path: profile.to_path_buf(),
        message,
    };
    let text = std::fs::read_to_string(profile).map_err(|e| profile_err(e.to_string()))?;
    let mut cfg = VmConfig::from_toml(&text).map_err(|e| profile_err(e.to_string()))?;
    let profile_dir = profile
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));
    let old_name = cfg.name.clone();
    let new_profile = profile_dir.join(format!("{new_name}.toml"));
    let same_profile_file = std::fs::canonicalize(profile)
        .ok()
        .zip(std::fs::canonicalize(&new_profile).ok())
        .is_some_and(|(a, b)| a == b);
    if new_profile.exists() && !same_profile_file {
        return Err(RenameError::Exists(new_profile.display().to_string()));
    }

    // Plan: which disks (and sidecars) move. Other profiles are looked for in
    // the profile's own directory and each disk's.
    let own = std::fs::canonicalize(profile).unwrap_or_else(|_| profile.to_path_buf());
    let mut moves: Vec<(PathBuf, PathBuf)> = Vec::new();
    let mut disk_moves: Vec<(usize, PathBuf)> = Vec::new();
    let mut nvram_move: Option<PathBuf> = None;
    let mut shared_disks = Vec::new();
    for (index, disk) in cfg.disks.iter().enumerate() {
        if !disk.writable {
            continue;
        }
        let src = resolve(&disk.path, &profile_dir);
        let Some(file_name) = src.file_name().map(|n| n.to_string_lossy().into_owned()) else {
            continue;
        };
        let Some(rest) = file_name.strip_prefix(&old_name) else {
            continue;
        };
        if old_name.is_empty() || !src.is_file() {
            continue;
        }
        let mut dirs = vec![profile_dir.clone()];
        if let Some(parent) = src.parent() {
            dirs.push(parent.to_path_buf());
        }
        let others = refs::find_references(&src, &dirs)
            .map(|found| {
                found
                    .into_iter()
                    .any(|r| std::fs::canonicalize(&r.profile).unwrap_or(r.profile) != own)
            })
            // Could not prove it is ours alone: leave it.
            .unwrap_or(true);
        if others {
            shared_disks.push(src);
            continue;
        }
        let dst = src.with_file_name(format!("{new_name}{rest}"));
        if dst.exists() {
            return Err(RenameError::Exists(dst.display().to_string()));
        }
        // The disk's sidecar follows it, and the profile's `boot.nvram` with
        // it when that is the sidecar.
        let sidecar = ops::nvram_sidecar_path(&src);
        if sidecar.is_file() {
            let sidecar_dst = ops::nvram_sidecar_path(&dst);
            if sidecar_dst.exists() {
                return Err(RenameError::Exists(sidecar_dst.display().to_string()));
            }
            let names_it = cfg
                .boot
                .nvram
                .as_ref()
                .is_some_and(|n| resolve(n, &profile_dir) == sidecar);
            if names_it {
                nvram_move = Some(sidecar_dst.clone());
            }
            moves.push((sidecar, sidecar_dst));
        }
        moves.push((src, dst.clone()));
        disk_moves.push((index, dst));
    }

    // Execute, undoing on the first failure.
    let mut done: Vec<(PathBuf, PathBuf)> = Vec::new();
    let undo = |done: &[(PathBuf, PathBuf)]| {
        for (from, to) in done.iter().rev() {
            let _ = std::fs::rename(to, from);
        }
    };
    for (from, to) in &moves {
        if let Err(e) = std::fs::rename(from, to) {
            undo(&done);
            return Err(RenameError::Io {
                from: from.clone(),
                to: to.clone(),
                message: e.to_string(),
            });
        }
        done.push((from.clone(), to.clone()));
    }

    cfg.name = new_name.to_string();
    for (index, dst) in &disk_moves {
        cfg.disks[*index].path = dst.clone();
    }
    if let Some(nvram) = nvram_move {
        cfg.boot.nvram = Some(nvram);
    }
    let written = toml::to_string_pretty(&cfg)
        .map_err(|e| e.to_string())
        .and_then(|out| {
            VmConfig::from_toml(&out).map_err(|e| e.to_string())?;
            Ok(out)
        })
        .and_then(|out| {
            // The new profile first, the old one removed after: a crash in
            // between leaves two profiles, never none.
            std::fs::write(&new_profile, out).map_err(|e| e.to_string())
        });
    if let Err(message) = written {
        if !same_profile_file {
            let _ = std::fs::remove_file(&new_profile);
        }
        undo(&done);
        return Err(RenameError::Write(message));
    }
    if !same_profile_file {
        if let Err(e) = std::fs::remove_file(profile) {
            tracing::warn!(path = %profile.display(), error = %e, "renamed, but cannot remove the old profile");
        }
        done.push((profile.to_path_buf(), new_profile.clone()));
    }

    Ok(RenameOutcome {
        profile: new_profile,
        renamed: done,
        shared_disks,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(tag: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("disk-image-rename-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn profile(dir: &Path, name: &str, disk: &str) -> String {
        format!(
            r#"name = "{name}"
memory_mib = 2048
vcpus = 2
transport = "pci"

[boot]
mode = "uefi"
firmware = "CLOUDHV.fd"
nvram = '{nvram}'

[[disk]]
path = '{disk}'
writable = true

[network]
backend = "usernet"
mac = "52:00:00:00:00:01"
"#,
            nvram = dir.join(disk).with_extension("nvram").display(),
            disk = dir.join(disk).display(),
        )
    }

    #[test]
    fn a_machine_takes_its_disk_and_nvram_with_it() {
        let dir = scratch("own");
        std::fs::write(dir.join("ubuntu.raw"), b"disk").unwrap();
        std::fs::write(dir.join("ubuntu.nvram"), b"vars").unwrap();
        let old = dir.join("ubuntu.toml");
        std::fs::write(&old, profile(&dir, "ubuntu", "ubuntu.raw")).unwrap();

        let outcome = rename_machine(&old, "lab").expect("rename");
        assert_eq!(outcome.profile, dir.join("lab.toml"));
        assert!(!old.exists());
        assert_eq!(std::fs::read(dir.join("lab.raw")).unwrap(), b"disk");
        assert_eq!(std::fs::read(dir.join("lab.nvram")).unwrap(), b"vars");
        let cfg =
            VmConfig::from_toml(&std::fs::read_to_string(dir.join("lab.toml")).unwrap()).unwrap();
        assert_eq!(cfg.name, "lab");
        assert_eq!(cfg.disks[0].path, dir.join("lab.raw"));
        assert_eq!(cfg.boot.nvram, Some(dir.join("lab.nvram")));
        assert_eq!(
            cfg.network.unwrap().mac.as_deref(),
            Some("52:00:00:00:00:01"),
            "the MAC is kept"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_disk_another_profile_uses_stays_put() {
        let dir = scratch("shared");
        std::fs::write(dir.join("ubuntu.raw"), b"disk").unwrap();
        std::fs::write(dir.join("ubuntu.nvram"), b"vars").unwrap();
        let old = dir.join("ubuntu-120.toml");
        std::fs::write(&old, profile(&dir, "ubuntu", "ubuntu.raw")).unwrap();
        std::fs::write(
            dir.join("ubuntu.toml"),
            profile(&dir, "ubuntu", "ubuntu.raw"),
        )
        .unwrap();

        let outcome = rename_machine(&old, "ubuntu-120").expect("rename");
        assert_eq!(outcome.shared_disks, vec![dir.join("ubuntu.raw")]);
        assert!(dir.join("ubuntu.raw").exists(), "the shared disk stays");
        assert!(dir.join("ubuntu.nvram").exists());
        let cfg =
            VmConfig::from_toml(&std::fs::read_to_string(dir.join("ubuntu-120.toml")).unwrap())
                .unwrap();
        assert_eq!(cfg.name, "ubuntu-120", "same file, new name");
        assert_eq!(cfg.disks[0].path, dir.join("ubuntu.raw"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_taken_name_refuses_and_touches_nothing() {
        let dir = scratch("taken");
        std::fs::write(dir.join("ubuntu.raw"), b"disk").unwrap();
        let old = dir.join("ubuntu.toml");
        std::fs::write(&old, profile(&dir, "ubuntu", "ubuntu.raw")).unwrap();
        std::fs::write(dir.join("lab.raw"), b"someone else's").unwrap();
        let result = rename_machine(&old, "lab");
        assert!(matches!(result, Err(RenameError::Exists(_))), "{result:?}");
        assert!(old.exists());
        assert_eq!(std::fs::read(dir.join("ubuntu.raw")).unwrap(), b"disk");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
