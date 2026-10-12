//! Cloning a whole machine: every writable disk (sparse-preserving and
//! verified, through [`crate::relocate`]'s copy), its UEFI variable store, and
//! a new profile that points at the copies.
//!
//! What the clone is, precisely:
//!
//! * **Writable disks are copied** next to their source, renamed for the new
//!   machine (`ubuntu.raw` → `ubuntu-2.raw`; a disk whose name does not start
//!   with the machine's name gets the new name as a prefix). **Read-only disks
//!   and the CD-ROM are shared**: nothing writes to them, so a second copy
//!   would only cost space.
//! * **The NVRAM is copied**, because it holds the `Boot####` entry that makes
//!   the copied disk bootable at all — a clone without it lands in the EFI
//!   shell. A disk's `.nvram` sidecar follows it; an explicit `boot.nvram`
//!   elsewhere is copied beside the new profile.
//! * **The network gets a MAC of its own** (`control_api::new_machine_mac` of
//!   the new name and disk), so the two machines can run side by side on one
//!   TAP bridge. **Port forwards are dropped**: two machines cannot both own a
//!   host port, and the clone would fail to start beside its source.
//! * **Snapshots are not cloned.** A snapshot is fingerprinted to the disk it
//!   came off (ADR-0006); the copy starts from a cold boot.
//!
//! What it is not: the guest inside is the same installation — the same
//! hostname, machine-id and SSH host keys. That is the guest's business; the
//! user guide says so.
//!
//! Order and rollback as in `relocate`: copy → verify → write the profile, and
//! a failure at any step removes every file this call created. The source is
//! never written.

use std::io;
use std::path::{Path, PathBuf};

use control_api::VmConfig;
use thiserror::Error;

use crate::ops;
use crate::relocate::{self, Progress};

#[derive(Debug, Error)]
pub enum CloneError {
    #[error("cannot read the profile {path}: {message}")]
    Profile { path: PathBuf, message: String },

    #[error("{0} already exists — pick another name")]
    Exists(String),

    #[error("the disk {0} does not exist")]
    MissingDisk(String),

    #[error(
        "not enough space at {dest}: {free} bytes free, but the copies carry {needed} \
         bytes of data"
    )]
    NoSpace {
        dest: String,
        free: u64,
        needed: u64,
    },

    #[error(
        "verification failed for {0}: the copy does not match its source. Every copy \
         was removed; the source machine is untouched"
    )]
    Verify(String),

    #[error("cannot write the new profile: {0}")]
    Write(String),

    #[error("i/o error on {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
}

/// What a successful clone made.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CloneOutcome {
    /// The new machine's profile.
    pub profile: PathBuf,
    /// `(source, copy)` for every disk and NVRAM file copied.
    pub copied: Vec<(PathBuf, PathBuf)>,
    /// Bytes actually copied (the allocated data of every disk).
    pub data_bytes: u64,
    /// Port forwards the source had and the clone does not.
    pub dropped_forwards: usize,
}

/// The file name a copied disk gets: the old machine name swapped for the new
/// one at the front of the stem, or the new name prefixed.
fn cloned_file_name(file_name: &str, old: &str, new: &str) -> String {
    match file_name.strip_prefix(old) {
        Some(rest) if !old.is_empty() => format!("{new}{rest}"),
        _ => format!("{new}-{file_name}"),
    }
}

/// Resolves a path as a profile declares it: absolute as is, relative against
/// the profile's directory (where the manager and `install` put everything).
fn resolve(declared: &Path, profile_dir: &Path) -> PathBuf {
    if declared.is_absolute() {
        declared.to_path_buf()
    } else {
        profile_dir.join(declared)
    }
}

/// Clones the machine described by `profile` as `new_name`; the new profile is
/// `<profile dir>/<new_name>.toml`. The caller makes sure the source machine
/// is not running — a disk copied under a live guest is a crash-consistent
/// image at best.
pub fn clone_machine(
    profile: &Path,
    new_name: &str,
    progress: Progress,
) -> Result<CloneOutcome, CloneError> {
    let profile_err = |message: String| CloneError::Profile {
        path: profile.to_path_buf(),
        message,
    };
    let text = std::fs::read_to_string(profile).map_err(|e| profile_err(e.to_string()))?;
    let mut cfg = VmConfig::from_toml(&text).map_err(|e| profile_err(e.to_string()))?;
    let profile_dir = profile
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));
    let new_profile = profile_dir.join(format!("{new_name}.toml"));
    if new_profile.exists() {
        return Err(CloneError::Exists(new_profile.display().to_string()));
    }
    let old_name = cfg.name.clone();

    // Plan every copy before writing a byte, so a name collision or a missing
    // disk refuses with nothing to undo.
    struct Planned {
        index: usize,
        src: PathBuf,
        dst: PathBuf,
    }
    let mut disks: Vec<Planned> = Vec::new();
    for (index, disk) in cfg.disks.iter().enumerate() {
        if !disk.writable {
            continue;
        }
        let src = resolve(&disk.path, &profile_dir);
        if !src.is_file() {
            return Err(CloneError::MissingDisk(src.display().to_string()));
        }
        let file_name = src
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .ok_or_else(|| CloneError::MissingDisk(src.display().to_string()))?;
        let dst = src.with_file_name(cloned_file_name(&file_name, &old_name, new_name));
        if dst.exists() || disks.iter().any(|p| p.dst == dst) {
            return Err(CloneError::Exists(dst.display().to_string()));
        }
        disks.push(Planned { index, src, dst });
    }

    // The NVRAM: the explicit one when the profile names it, else none (the
    // engine then keeps variables in memory only, and so does the clone).
    let nvram: Option<(PathBuf, PathBuf)> = match &cfg.boot.nvram {
        None => None,
        Some(declared) => {
            let src = resolve(declared, &profile_dir);
            // The usual layout: it is a copied disk's sidecar, so it follows
            // that disk's new name. Otherwise it goes beside the new profile.
            let dst = disks
                .iter()
                .find(|p| ops::nvram_sidecar_path(&p.src) == src)
                .map(|p| ops::nvram_sidecar_path(&p.dst))
                .unwrap_or_else(|| profile_dir.join(format!("{new_name}.nvram")));
            if dst.exists() {
                return Err(CloneError::Exists(dst.display().to_string()));
            }
            // A store the firmware has not written yet is fine: the clone
            // simply starts with an empty one too.
            src.is_file().then_some((src, dst))
        }
    };

    // Space, per destination directory — they are usually all one.
    let mut bills: Vec<(PathBuf, u64)> = Vec::new();
    for p in &disks {
        let (data, _) = relocate::copy_bill(&p.src).unwrap_or((0, 0));
        let dir = p.dst.parent().map(Path::to_path_buf).unwrap_or_default();
        match bills.iter_mut().find(|(d, _)| *d == dir) {
            Some((_, n)) => *n += data,
            None => bills.push((dir, data)),
        }
    }
    for (dir, needed) in &bills {
        if let Some((free, _)) = ops::disk_space(dir) {
            if free < *needed {
                return Err(CloneError::NoSpace {
                    dest: dir.display().to_string(),
                    free,
                    needed: *needed,
                });
            }
        }
    }
    let total: u64 = bills.iter().map(|(_, n)| n).sum();

    // From here on every created file is recorded, and every failure removes
    // them all.
    let mut created: Vec<PathBuf> = Vec::new();
    let undo = |created: &[PathBuf]| {
        for path in created {
            let _ = std::fs::remove_file(path);
        }
    };

    let mut copied: Vec<(PathBuf, PathBuf)> = Vec::new();
    let mut done_before = 0u64;
    for p in &disks {
        // One progress bar across every disk.
        let mut forward = |done: u64, _: u64| progress(done_before + done, total);
        created.push(p.dst.clone());
        match relocate::copy_verified(&p.src, &p.dst, &mut forward) {
            Ok(Some(data)) => done_before += data,
            Ok(None) => {
                undo(&created);
                return Err(CloneError::Verify(p.dst.display().to_string()));
            }
            Err(source) => {
                undo(&created);
                return Err(CloneError::Io {
                    path: p.dst.clone(),
                    source,
                });
            }
        }
        copied.push((p.src.clone(), p.dst.clone()));
    }
    if let Some((src, dst)) = &nvram {
        created.push(dst.clone());
        let result =
            std::fs::copy(src, dst).and_then(|_| Ok(std::fs::read(src)? == std::fs::read(dst)?));
        match result {
            Ok(true) => copied.push((src.clone(), dst.clone())),
            Ok(false) => {
                undo(&created);
                return Err(CloneError::Verify(dst.display().to_string()));
            }
            Err(source) => {
                undo(&created);
                return Err(CloneError::Io {
                    path: dst.clone(),
                    source,
                });
            }
        }
    }

    // The new profile. Paths are written absolute: the copies were resolved
    // against the source profile, and absolute keeps the clone correct even if
    // someone moves its profile later.
    cfg.name = new_name.to_string();
    for p in &disks {
        cfg.disks[p.index].path = p.dst.clone();
    }
    if let Some((_, dst)) = &nvram {
        cfg.boot.nvram = Some(dst.clone());
    } else if let Some(declared) = &cfg.boot.nvram {
        // Named but not yet written: give the clone a path of its own, never
        // the source's (the firmware would then write the source's store).
        let fresh = disks
            .iter()
            .find(|p| ops::nvram_sidecar_path(&p.src) == resolve(declared, &profile_dir))
            .map(|p| ops::nvram_sidecar_path(&p.dst))
            .unwrap_or_else(|| profile_dir.join(format!("{new_name}.nvram")));
        cfg.boot.nvram = Some(fresh);
    }
    let mut dropped_forwards = 0;
    if let Some(net) = &mut cfg.network {
        let anchor = disks
            .first()
            .map(|p| p.dst.clone())
            .unwrap_or_else(|| new_profile.clone());
        net.mac = Some(control_api::new_machine_mac(new_name, &anchor));
        dropped_forwards = net.forward.len();
        net.forward.clear();
        // A TAP interface is one machine's too.
        net.interface = None;
    }

    let serialized = toml::to_string_pretty(&cfg)
        .map_err(|e| e.to_string())
        .and_then(|out| {
            // Re-validated before it exists as a profile anyone could start.
            VmConfig::from_toml(&out).map_err(|e| e.to_string())?;
            Ok(out)
        });
    let out = match serialized {
        Ok(out) => out,
        Err(message) => {
            undo(&created);
            return Err(CloneError::Write(message));
        }
    };
    // `create_new`: a profile that appeared since the check is someone else's
    // and is left alone.
    let mut file = match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&new_profile)
    {
        Ok(file) => file,
        Err(e) => {
            undo(&created);
            return Err(CloneError::Write(e.to_string()));
        }
    };
    created.push(new_profile.clone());
    if let Err(e) = io::Write::write_all(&mut file, out.as_bytes()) {
        drop(file);
        undo(&created);
        return Err(CloneError::Write(e.to_string()));
    }

    Ok(CloneOutcome {
        profile: new_profile,
        copied,
        data_bytes: total,
        dropped_forwards,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn profile_text(dir: &Path, name: &str) -> String {
        let disk = dir.join(format!("{name}.raw"));
        let nvram = dir.join(format!("{name}.nvram"));
        let iso = dir.join("installer.iso");
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

[[disk]]
path = '{iso}'
writable = false

[network]
backend = "usernet"
mac = "52:00:00:00:00:01"
forward = ["tcp:8080:80"]
"#,
            nvram = nvram.display(),
            disk = disk.display(),
            iso = iso.display(),
        )
    }

    fn scratch(tag: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("disk-image-clone-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn names_follow_the_machine() {
        assert_eq!(
            cloned_file_name("ubuntu.raw", "ubuntu", "ubuntu-2"),
            "ubuntu-2.raw"
        );
        assert_eq!(
            cloned_file_name("ubuntu-data.raw", "ubuntu", "lab"),
            "lab-data.raw"
        );
        assert_eq!(
            cloned_file_name("disk0.raw", "ubuntu", "lab"),
            "lab-disk0.raw"
        );
    }

    #[test]
    fn a_clone_copies_writable_disks_and_nvram_and_gets_its_own_identity() {
        let dir = scratch("full");
        let disk = dir.join("ubuntu.raw");
        {
            let file = std::fs::File::create(&disk).unwrap();
            file.set_len(8 << 20).unwrap();
        }
        // Some data at both ends, a hole between.
        {
            use std::io::{Seek, SeekFrom, Write};
            let mut f = std::fs::OpenOptions::new().write(true).open(&disk).unwrap();
            f.write_all(b"head").unwrap();
            f.seek(SeekFrom::Start((8 << 20) - 4)).unwrap();
            f.write_all(b"tail").unwrap();
        }
        std::fs::write(dir.join("ubuntu.nvram"), b"vars").unwrap();
        std::fs::write(dir.join("installer.iso"), b"iso").unwrap();
        let profile = dir.join("ubuntu.toml");
        std::fs::write(&profile, profile_text(&dir, "ubuntu")).unwrap();

        let mut last = (0, 0);
        let outcome =
            clone_machine(&profile, "ubuntu-2", &mut |d, t| last = (d, t)).expect("clone");
        assert_eq!(outcome.profile, dir.join("ubuntu-2.toml"));
        assert_eq!(outcome.dropped_forwards, 1);
        assert_eq!(last.0, last.1, "progress reaches its total");

        let copy = dir.join("ubuntu-2.raw");
        assert_eq!(std::fs::read(&copy).unwrap(), std::fs::read(&disk).unwrap());
        assert_eq!(std::fs::read(dir.join("ubuntu-2.nvram")).unwrap(), b"vars");

        let cfg = VmConfig::from_toml(&std::fs::read_to_string(&outcome.profile).unwrap()).unwrap();
        assert_eq!(cfg.name, "ubuntu-2");
        assert_eq!(cfg.disks[0].path, copy);
        assert_eq!(
            cfg.disks[1].path,
            dir.join("installer.iso"),
            "read-only is shared"
        );
        assert_eq!(cfg.boot.nvram, Some(dir.join("ubuntu-2.nvram")));
        let net = cfg.network.unwrap();
        assert_ne!(net.mac.as_deref(), Some("52:00:00:00:00:01"));
        assert!(net.forward.is_empty());

        // The source is untouched.
        assert_eq!(
            std::fs::read_to_string(&profile).unwrap(),
            profile_text(&dir, "ubuntu")
        );

        // A second clone under the same name refuses before copying anything.
        let again = clone_machine(&profile, "ubuntu-2", &mut |_, _| {});
        assert!(matches!(again, Err(CloneError::Exists(_))), "{again:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_missing_disk_refuses_and_leaves_nothing_behind() {
        let dir = scratch("missing");
        std::fs::write(dir.join("installer.iso"), b"iso").unwrap();
        let profile = dir.join("ubuntu.toml");
        std::fs::write(&profile, profile_text(&dir, "ubuntu")).unwrap();
        let result = clone_machine(&profile, "lab", &mut |_, _| {});
        assert!(
            matches!(result, Err(CloneError::MissingDisk(_))),
            "{result:?}"
        );
        assert!(!dir.join("lab.toml").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
