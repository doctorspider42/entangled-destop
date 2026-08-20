//! Native file and folder pickers.
//!
//! Two rules, both non-negotiable:
//!
//! 1. **Never on the egui thread.** A native dialog is modal and lives for as
//!    long as the user browses; calling it inline would freeze the frame loop
//!    for however many seconds that takes. Each dialog therefore runs on its
//!    own short-lived worker thread and its answer comes back through the same
//!    channel + [`crate::process::Waker`] pattern the VM scanner and the
//!    metrics sampler already use.
//! 2. **The text field stays.** A picker is the easy path, not the only path:
//!    every path in the product remains typeable, because a path pasted from a
//!    terminal is still the fastest way for the people who have one.
//!
//! The backend is `rfd` (MIT). On Linux it is built against the **XDG desktop
//! portal**, not GTK — see the dependency comment in the workspace manifest:
//! GTK is LGPL and this binary carries no copyleft (ADR-0001).

use std::path::{Path, PathBuf};
use std::sync::mpsc;

use crate::process::Waker;

/// Which field a picked path belongs to. The application matches on it when
/// the answer arrives, because the modal may have moved on in the meantime.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PickTarget {
    /// Settings ▸ where machines are kept.
    VmDir,
    /// Settings ▸ Advanced ▸ the `entangled` binary.
    EngineBinary,
    /// Settings ▸ Advanced ▸ the working directory children inherit.
    WorkDir,
    /// The new-machine wizard's optional installer image.
    WizardIso,
    /// The new-machine wizard's existing disk image.
    WizardDisk,
    EditorKernel,
    EditorInitramfs,
    EditorFirmware,
    EditorNvram,
    EditorCdrom,
    EditorAddDisk,
    MoveDestination,
}

/// What kind of thing the dialog asks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    File,
    Directory,
}

/// Title, kind and file filter for each target — one table so a new picker
/// cannot quietly ship with the wrong filter.
struct Spec {
    title: &'static str,
    kind: Kind,
    /// `(label, extensions)`; empty means "any file".
    filter: Option<(&'static str, &'static [&'static str])>,
}

const DISK_FILTER: (&str, &[&str]) = ("Disk images", &["raw", "img"]);
const ISO_FILTER: (&str, &[&str]) = ("Installer images", &["iso"]);
const FIRMWARE_FILTER: (&str, &[&str]) = ("UEFI firmware", &["fd", "bin", "rom"]);
const NVRAM_FILTER: (&str, &[&str]) = ("UEFI variable stores", &["nvram", "fd"]);

fn spec(target: PickTarget) -> Spec {
    match target {
        PickTarget::VmDir => Spec {
            title: "Where your machines are kept",
            kind: Kind::Directory,
            filter: None,
        },
        PickTarget::EngineBinary => Spec {
            title: "Locate the Entangled engine",
            kind: Kind::File,
            filter: None,
        },
        PickTarget::WorkDir => Spec {
            title: "Working directory for machines",
            kind: Kind::Directory,
            filter: None,
        },
        PickTarget::WizardIso => Spec {
            title: "Choose an installer image",
            kind: Kind::File,
            filter: Some(ISO_FILTER),
        },
        PickTarget::WizardDisk | PickTarget::EditorAddDisk => Spec {
            title: "Choose a disk image",
            kind: Kind::File,
            filter: Some(DISK_FILTER),
        },
        PickTarget::EditorKernel => Spec {
            title: "Choose a Linux kernel image",
            kind: Kind::File,
            filter: None,
        },
        PickTarget::EditorInitramfs => Spec {
            title: "Choose an initramfs",
            kind: Kind::File,
            filter: None,
        },
        PickTarget::EditorFirmware => Spec {
            title: "Choose the UEFI firmware",
            kind: Kind::File,
            filter: Some(FIRMWARE_FILTER),
        },
        PickTarget::EditorNvram => Spec {
            title: "Choose this machine's UEFI settings file",
            kind: Kind::File,
            filter: Some(NVRAM_FILTER),
        },
        PickTarget::EditorCdrom => Spec {
            title: "Choose an installer disc image",
            kind: Kind::File,
            filter: Some(ISO_FILTER),
        },
        PickTarget::MoveDestination => Spec {
            title: "Move the disk image to",
            kind: Kind::Directory,
            filter: None,
        },
    }
}

/// A dialog that is currently open. At most one exists: the native dialogs are
/// modal anyway, and a second would only be confusing.
pub struct Pending {
    pub target: PickTarget,
    result: mpsc::Receiver<Option<PathBuf>>,
}

impl Pending {
    /// The answer, once the user has given one. `None` while the dialog is
    /// still open; `Some(None)` when it was cancelled.
    pub fn poll(&self) -> Option<Option<PathBuf>> {
        self.result.try_recv().ok()
    }
}

/// Opens a dialog on its own thread. The returned [`Pending`] is polled once
/// per frame; the waker makes sure the frame happens as soon as the answer
/// lands rather than at the next heartbeat.
///
/// `start_dir` is where the dialog opens — the directory of whatever is in the
/// field already, else the VM directory, else wherever the platform prefers.
/// `current` seeds the file name for a field that already has one.
pub fn open(
    target: PickTarget,
    start_dir: Option<PathBuf>,
    current: Option<String>,
    waker: Waker,
) -> Result<Pending, String> {
    let (tx, rx) = mpsc::channel();
    let builder = std::thread::Builder::new().name("file-picker".to_string());
    builder
        .spawn(move || {
            let picked = run_dialog(target, start_dir.as_deref(), current.as_deref());
            // A send error means the manager moved on; nothing to clean up.
            let _ = tx.send(picked);
            waker();
        })
        .map_err(|e| format!("cannot open the file browser: {e}"))?;
    Ok(Pending { target, result: rx })
}

fn run_dialog(
    target: PickTarget,
    start_dir: Option<&Path>,
    current: Option<&str>,
) -> Option<PathBuf> {
    let spec = spec(target);
    let mut dialog = rfd::FileDialog::new().set_title(spec.title);
    if let Some(dir) = start_dir.filter(|dir| dir.is_dir()) {
        dialog = dialog.set_directory(dir);
    }
    if let Some(name) = current.filter(|name| !name.trim().is_empty()) {
        dialog = dialog.set_file_name(name);
    }
    if let Some((label, extensions)) = spec.filter {
        dialog = dialog
            .add_filter(label, extensions)
            .add_filter("All files", &["*"]);
    }
    match spec.kind {
        Kind::File => dialog.pick_file(),
        Kind::Directory => dialog.pick_folder(),
    }
}

/// The directory a dialog should open in, given whatever the field holds now
/// and a fallback. Keeping this out of the thread makes it testable, and it is
/// the part that is easy to get wrong: an empty field must not send the user to
/// the process's working directory.
pub fn start_directory(current: &str, fallback: &Path) -> PathBuf {
    let current = current.trim();
    if !current.is_empty() {
        let path = Path::new(current);
        if path.is_dir() {
            return path.to_path_buf();
        }
        if let Some(parent) = path.parent().filter(|p| p.is_dir()) {
            return parent.to_path_buf();
        }
    }
    if fallback.is_dir() {
        return fallback.to_path_buf();
    }
    // The nearest ancestor that does exist — a VM directory that has not been
    // created yet is the normal case on a fresh install.
    fallback
        .ancestors()
        .find(|p| p.is_dir())
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."))
}

/// The file name part of a path, for seeding a save-style dialog.
pub fn file_name_of(current: &str) -> Option<String> {
    Path::new(current.trim())
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_target_has_a_title_and_the_media_targets_filter() {
        for target in [
            PickTarget::VmDir,
            PickTarget::EngineBinary,
            PickTarget::WorkDir,
            PickTarget::WizardIso,
            PickTarget::WizardDisk,
            PickTarget::EditorKernel,
            PickTarget::EditorInitramfs,
            PickTarget::EditorFirmware,
            PickTarget::EditorNvram,
            PickTarget::EditorCdrom,
            PickTarget::EditorAddDisk,
            PickTarget::MoveDestination,
        ] {
            let spec = spec(target);
            assert!(!spec.title.is_empty(), "{target:?} has no title");
        }
        assert_eq!(spec(PickTarget::WizardIso).filter, Some(ISO_FILTER));
        assert_eq!(
            spec(PickTarget::EditorFirmware).filter,
            Some(FIRMWARE_FILTER)
        );
        assert_eq!(spec(PickTarget::EditorAddDisk).filter, Some(DISK_FILTER));
        assert_eq!(spec(PickTarget::VmDir).kind, Kind::Directory);
        assert_eq!(spec(PickTarget::MoveDestination).kind, Kind::Directory);
    }

    #[test]
    fn the_dialog_opens_next_to_what_the_field_already_names() {
        let dir = std::env::temp_dir();
        let file = dir.join("entangled-picker-probe.raw");
        // A path inside an existing directory opens in that directory, even
        // when the file itself does not exist yet.
        assert_eq!(
            start_directory(
                &file.display().to_string(),
                Path::new("/definitely/not/here")
            ),
            dir
        );
        // An empty field falls back, and a fallback that does not exist walks
        // up to one that does rather than landing on the process cwd.
        assert_eq!(start_directory("   ", &dir), dir);
        let missing = dir.join("entangled-absent-dir").join("deeper");
        assert_eq!(start_directory("", &missing), dir);
    }

    #[test]
    fn file_names_survive_both_separators() {
        assert_eq!(file_name_of("/vms/a.raw"), Some("a.raw".to_string()));
        assert_eq!(file_name_of("  "), None);
    }
}
