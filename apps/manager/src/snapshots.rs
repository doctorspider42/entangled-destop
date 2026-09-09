//! Suspended machines: reading the snapshot files in the VM directory, and
//! deciding — before anything is launched — whether one could actually be
//! resumed here ([ADR-0006](../../../docs/adr/0006-suspend-restore.md)).
//!
//! # Why the refusals are computed in the manager
//!
//! `entangled resume` refuses a snapshot it cannot restore, by name, and that
//! is the backstop. But a GUI that only learns this by starting a child and
//! watching it fail has told the user nothing until after they clicked: the
//! Resume button was bright, the window flickered, and a red toast arrived with
//! an engine's sentence in it. `vm_snapshot::inspect` reads the header, the
//! index and the metadata — no disk opened, no guest memory allocated — so the
//! same answer is available for the price of a file open, before the button is
//! even drawn.
//!
//! # The one thing `inspect` cannot answer here
//!
//! [`vm_snapshot::SnapshotInfo::restorable_here`] means "restorable by *this
//! process*". The manager is not the process that would restore it: on Windows
//! a machine may be set to run through WSL, where the hypervisor is KVM and a
//! Linux snapshot is exactly right. So the host check is redone against the
//! machine's chosen [`Backend`], and `restorable_here` is deliberately not used.

use std::path::{Path, PathBuf};

use vm_snapshot::meta::FileFingerprint;
use vm_snapshot::{HostKind, SnapshotError};

use crate::backend::Backend;
use crate::discovery::{format_bytes, VmEntry};

/// Everything a snapshot file says about itself, once read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SnapshotFacts {
    /// The machine it is of, from its own embedded profile.
    pub vm_name: String,
    pub created_unix: i64,
    /// The build that wrote it — for a bug report, never for a decision.
    pub writer: String,
    pub host: HostKind,
    pub vcpus: u32,
    pub memory_bytes: u64,
    /// `"mmio"` or `"pci"`.
    pub transport: String,
    /// `"direct-linux"` or `"uefi"`.
    pub boot_mode: String,
    /// Every file the machine had open, as it looked then. The strict ones
    /// (disks, CD-ROM) are what a restore refuses on.
    pub files: Vec<FileFingerprint>,
    pub sections: usize,
}

/// One row of the Snapshots view.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SnapshotRow {
    pub path: PathBuf,
    pub file_name: String,
    /// Size of the file itself.
    pub apparent_bytes: u64,
    /// Blocks actually allocated. A snapshot is written sparse
    /// (`disk_image::ops::mark_sparse`), so the two differ.
    pub allocated_bytes: Option<u64>,
    /// What the file says, or why it could not be read at all — a corrupt or
    /// foreign-version snapshot is still a row, because a file taking up half a
    /// gigabyte deserves to be visible and deletable.
    pub facts: Result<SnapshotFacts, String>,
}

impl SnapshotRow {
    /// The machine this snapshot belongs to, when it is readable.
    pub fn vm_name(&self) -> Option<&str> {
        self.facts.as_ref().ok().map(|f| f.vm_name.as_str())
    }

    /// One line of hardware, for under the title.
    pub fn shape_line(&self) -> String {
        match &self.facts {
            Ok(facts) => format!(
                "{} vCPU · {} RAM · {} · {} boot",
                facts.vcpus,
                format_bytes(facts.memory_bytes),
                facts.transport,
                facts.boot_mode
            ),
            Err(_) => "unreadable".to_string(),
        }
    }
}

/// Whether a snapshot can be resumed, and everything worth saying about it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Verdict {
    /// Reasons a resume is refused. Non-empty means the button is off, and
    /// each entry is a plain sentence a person can act on.
    pub blocked: Vec<String>,
    /// True but not disqualifying: an advisory file change, a machine whose
    /// profile has gone, disks this host cannot see from here.
    pub notes: Vec<String>,
}

impl Verdict {
    pub fn resumable(&self) -> bool {
        self.blocked.is_empty()
    }
}

/// Where a machine's snapshot lives.
///
/// Beside the profile, named after the machine — the same answer
/// `entangled run` computes for a bare `save` on its control channel, derived
/// the same way from the same two facts, so the manager looks for the file the
/// engine will write without having to name it on the command line.
pub fn path_for(vm: &VmEntry) -> PathBuf {
    vm.profile_path
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join(format!("{}.{}", vm.name, vm_snapshot::EXTENSION))
}

/// Reads one snapshot file into a row. Never fails: an unreadable file becomes
/// a row that says so.
///
/// Called from the scan worker, never from the frame loop — it is a file open
/// plus a few kilobytes, but the scan thread is where file work belongs.
pub fn read(path: &Path) -> SnapshotRow {
    let meta = std::fs::metadata(path).ok();
    let facts = match vm_snapshot::inspect(path) {
        Ok(info) => Ok(SnapshotFacts {
            vm_name: info.metadata.vm_name.clone(),
            created_unix: info.metadata.created_unix,
            writer: info.metadata.writer.clone(),
            host: info.host,
            vcpus: info.metadata.shape.vcpus,
            memory_bytes: info.metadata.shape.memory_bytes,
            transport: info.metadata.shape.transport.clone(),
            boot_mode: info.metadata.shape.boot_mode.clone(),
            files: info.metadata.files.clone(),
            sections: info.sections.len(),
        }),
        Err(error) => Err(error.to_string()),
    };
    SnapshotRow {
        path: path.to_path_buf(),
        file_name: path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| path.display().to_string()),
        apparent_bytes: meta.as_ref().map(|m| m.len()).unwrap_or(0),
        allocated_bytes: disk_image::allocated_bytes(path),
        facts,
    }
}

/// Which hypervisor a backend actually runs on, which is what a snapshot is
/// bound to — not which operating system the manager happens to be.
pub fn host_of(backend: Backend) -> Option<HostKind> {
    if backend.is_linux_kvm() {
        return Some(HostKind::KvmLinux);
    }
    cfg!(windows).then_some(HostKind::WhpWindows)
}

/// Everything standing between this snapshot and a resume.
///
/// `profile` is the machine as it is on disk *now*, when one of that name still
/// exists. A snapshot carries its own copy of the profile, so a missing one is
/// a note rather than a refusal — but a profile that has been **edited** is a
/// refusal, because the machine the engine would assemble is no longer the one
/// the snapshot came off.
pub fn verdict(row: &SnapshotRow, backend: Backend, profile: Option<&VmEntry>) -> Verdict {
    let mut verdict = Verdict::default();
    let facts = match &row.facts {
        Ok(facts) => facts,
        Err(error) => {
            verdict.blocked.push(format!(
                "This file cannot be read by this version of Entangled: {error}"
            ));
            return verdict;
        }
    };

    if let Some(here) = host_of(backend) {
        if here != facts.host {
            let mut message = format!(
                "This was saved on {} and the machine is set to run on {}. A saved processor \
                 carries its own hypervisor's state, which the other one cannot load.",
                facts.host.as_str(),
                here.as_str()
            );
            // On Windows there is a second engine to point at; on Linux there
            // is not, and offering one would be a dead end.
            if Backend::Wsl.available_on_host() {
                let fix = if facts.host == HostKind::KvmLinux {
                    "WSL (KVM)"
                } else {
                    Backend::Native.label()
                };
                message.push_str(&format!(
                    " Set this machine to run on \"{fix}\" and it will resume."
                ));
            }
            verdict.blocked.push(message);
        }
    }

    match profile {
        None => verdict.notes.push(format!(
            "There is no machine profile named '{}' any more. Resuming still works — the \
             snapshot carries its own copy of it.",
            facts.vm_name
        )),
        Some(vm) => {
            if let Some(changed) = shape_change(facts, vm) {
                verdict.blocked.push(format!(
                    "'{}' has been changed since this was saved ({changed}). A snapshot only \
                     goes back onto the machine it came off — put the setting back, or start \
                     the machine fresh and delete this file.",
                    facts.vm_name
                ));
            }
        }
    }

    for file in &facts.files {
        match check_file(file) {
            FileVerdict::Unchanged => {}
            FileVerdict::Elsewhere => verdict.notes.push(format!(
                "The {} {} is named the way the other engine sees it, so Entangled checks it \
                 when the machine resumes rather than now.",
                file.role.as_str(),
                file.path.display()
            )),
            FileVerdict::Advisory(note) => verdict.notes.push(note),
            FileVerdict::Refused(reason) => verdict.blocked.push(reason),
        }
    }
    verdict
}

/// The first field of the machine's shape that no longer matches, in the words
/// the editor uses for it.
fn shape_change(facts: &SnapshotFacts, vm: &VmEntry) -> Option<String> {
    if facts.memory_bytes != vm.memory_mib << 20 {
        return Some(format!(
            "memory: was {} MiB, is {} MiB",
            facts.memory_bytes >> 20,
            vm.memory_mib
        ));
    }
    if facts.vcpus != vm.vcpus {
        return Some(format!("vCPUs: were {}, are {}", facts.vcpus, vm.vcpus));
    }
    let transport = vm.transport.to_string();
    if facts.transport != transport {
        return Some(format!(
            "virtio transport: was {}, is {transport}",
            facts.transport
        ));
    }
    let boot_mode = if vm.uefi { "uefi" } else { "direct-linux" };
    if facts.boot_mode != boot_mode {
        return Some(format!(
            "boot mode: was {}, is {boot_mode}",
            facts.boot_mode
        ));
    }
    None
}

enum FileVerdict {
    Unchanged,
    /// The path belongs to the other engine's filesystem, so this host cannot
    /// answer for it and must not pretend the file has vanished.
    Elsewhere,
    Advisory(String),
    Refused(String),
}

fn check_file(file: &FileFingerprint) -> FileVerdict {
    if !nameable_here(&file.path.to_string_lossy(), cfg!(windows)) {
        return FileVerdict::Elsewhere;
    }
    match file.check() {
        Ok(None) => FileVerdict::Unchanged,
        Ok(Some(note)) => FileVerdict::Advisory(format!(
            "The {note}. That is survivable — it is only re-read if the machine reboots.",
        )),
        Err(SnapshotError::DiskChanged {
            path,
            what,
            snapshot,
            current,
        }) => FileVerdict::Refused(format!(
            "The {} {path} has changed since this was saved ({what}: was {snapshot}, is \
             {current}). The suspended guest still holds that filesystem in its memory, so \
             resuming onto it would corrupt it.",
            file.role.as_str()
        )),
        Err(other) => FileVerdict::Refused(other.to_string()),
    }
}

/// Whether a path recorded by the engine is one *this* host could even look at.
///
/// A machine run through WSL records `/mnt/d/vms/root.raw`; a Windows manager
/// asking the filesystem about that gets "not found" and would report a disk
/// that has vanished — the loudest possible refusal, for a disk that is
/// perfectly fine. Pure string work, and both branches are tested on both
/// hosts, because the answer depends on the host and the tests must not.
fn nameable_here(path: &str, on_windows: bool) -> bool {
    let looks_posix = path.starts_with('/');
    let looks_windows = {
        let bytes = path.as_bytes();
        bytes.len() >= 3
            && (bytes[0] as char).is_ascii_alphabetic()
            && bytes[1] == b':'
            && (bytes[2] == b'\\' || bytes[2] == b'/')
    };
    if on_windows {
        // A relative path resolves against the manager's own directory, which
        // is not where the engine resolved it either; leave it to the engine.
        looks_windows
    } else {
        looks_posix
    }
}

/// The sentence a delete confirmation puts under the title.
pub fn what_is_lost(row: &SnapshotRow) -> String {
    match &row.facts {
        Ok(facts) => format!(
            "'{}' as it was at {} — everything it had open, and everything it was in the \
             middle of. Its disks and its profile are untouched, so the machine will cold-boot \
             the next time it starts.",
            facts.vm_name,
            vm_snapshot::meta::format_unix(facts.created_unix)
        ),
        Err(_) => "A snapshot this build cannot read. Nothing else is touched.".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use control_api::VirtioTransport;
    use vm_snapshot::meta::{FileFingerprint, FileRole};

    fn facts() -> SnapshotFacts {
        SnapshotFacts {
            vm_name: "ubuntu-lab".into(),
            created_unix: 1_787_303_645,
            writer: "entangled 0.2.0".into(),
            host: HostKind::KvmLinux,
            vcpus: 2,
            memory_bytes: 2048 << 20,
            transport: "pci".into(),
            boot_mode: "uefi".into(),
            files: Vec::new(),
            sections: 9,
        }
    }

    fn row(facts: Result<SnapshotFacts, String>) -> SnapshotRow {
        SnapshotRow {
            path: PathBuf::from("/vms/ubuntu-lab.esnap"),
            file_name: "ubuntu-lab.esnap".into(),
            apparent_bytes: 539_500_000,
            allocated_bytes: Some(512 << 20),
            facts,
        }
    }

    fn vm() -> VmEntry {
        VmEntry {
            name: "ubuntu-lab".into(),
            profile_path: PathBuf::from("/vms/ubuntu-lab.toml"),
            memory_mib: 2048,
            vcpus: 2,
            transport: VirtioTransport::Pci,
            display: (1920, 1080),
            network_interface: None,
            disks: Vec::new(),
            uefi: true,
        }
    }

    /// The whole point of doing this in the manager: a machine that is fine
    /// says so before anything is launched.
    #[test]
    fn a_matching_machine_on_the_right_engine_is_resumable() {
        let verdict = verdict(&row(Ok(facts())), Backend::Wsl, Some(&vm()));
        assert!(verdict.resumable(), "{:?}", verdict.blocked);
        assert!(verdict.notes.is_empty(), "{:?}", verdict.notes);
    }

    /// `inspect`'s own `restorable_here` answers for the manager's process.
    /// The manager is not the process that restores anything — on Windows the
    /// machine may be set to run under WSL, where a Linux snapshot is right.
    #[test]
    fn the_host_check_follows_the_backend_not_the_manager() {
        // WSL always runs a Linux kernel with KVM, on either host.
        assert_eq!(host_of(Backend::Wsl), Some(HostKind::KvmLinux));
        let linux_snapshot = row(Ok(facts()));
        assert!(verdict(&linux_snapshot, Backend::Wsl, None)
            .blocked
            .is_empty());

        let mut windows = facts();
        windows.host = HostKind::WhpWindows;
        let refusal = verdict(&row(Ok(windows)), Backend::Wsl, None);
        let message = refusal.blocked.first().expect("refused");
        assert!(message.contains("Windows/WHP"), "{message}");
        assert!(message.contains("Linux/KVM"), "{message}");
        // A refusal with no way out is only half a message; on Windows there is
        // another engine to point at.
        if Backend::Wsl.available_on_host() {
            assert!(message.contains("Set this machine to run on"), "{message}");
        }
    }

    /// Editing the machine is how a snapshot is most easily orphaned, and the
    /// only warning today is inside the engine at resume time.
    #[test]
    fn an_edited_machine_is_refused_and_the_field_is_named() {
        let against = |vm: &VmEntry| verdict(&row(Ok(facts())), Backend::Wsl, Some(vm));

        let mut edited = vm();
        edited.memory_mib = 4096;
        let message = against(&edited).blocked.first().cloned().expect("refused");
        assert!(message.contains("was 2048 MiB, is 4096 MiB"), "{message}");

        let mut retransported = vm();
        retransported.transport = VirtioTransport::Mmio;
        let transport = against(&retransported);
        assert!(
            transport.blocked[0].contains("virtio transport"),
            "{:?}",
            transport.blocked
        );

        let mut rebooted = vm();
        rebooted.uefi = false;
        let boot = against(&rebooted);
        assert!(boot.blocked[0].contains("boot mode"), "{boot:?}");
    }

    /// The snapshot carries its own profile, so a deleted one costs nothing —
    /// and saying "it is gone" as a refusal would be a lie.
    #[test]
    fn a_machine_whose_profile_is_gone_is_still_resumable() {
        let verdict = verdict(&row(Ok(facts())), Backend::Wsl, None);
        assert!(verdict.resumable());
        assert!(
            verdict.notes.iter().any(|n| n.contains("carries its own")),
            "{:?}",
            verdict.notes
        );
    }

    #[test]
    fn an_unreadable_file_is_a_row_with_one_reason() {
        let verdict = verdict(
            &row(Err("snapshot format version 8 cannot be restored".into())),
            Backend::Native,
            None,
        );
        assert!(!verdict.resumable());
        assert!(verdict.blocked[0].contains("version 8"), "{:?}", verdict);
    }

    /// A disk recorded as `/mnt/d/...` by the WSL engine must not read as
    /// "vanished" to a Windows manager: that is the loudest refusal in the
    /// product, for a file that is fine.
    #[test]
    fn the_other_engines_paths_are_not_reported_as_missing() {
        assert!(nameable_here("/home/spider/vms/root.raw", false));
        assert!(!nameable_here("/mnt/d/vms/root.raw", true));
        assert!(nameable_here(r"D:\vms\root.raw", true));
        assert!(nameable_here("D:/vms/root.raw", true));
        assert!(!nameable_here(r"D:\vms\root.raw", false));
        // Relative paths belong to whichever engine resolves them.
        assert!(!nameable_here("vms/root.raw", true));
        assert!(!nameable_here("vms/root.raw", false));

        let mut with_disk = facts();
        with_disk.files = vec![FileFingerprint {
            role: FileRole::Disk,
            path: if cfg!(windows) {
                PathBuf::from("/mnt/d/vms/root.raw")
            } else {
                PathBuf::from(r"D:\vms\root.raw")
            },
            len: 8 << 30,
            modified_nanos: 1,
            present: true,
        }];
        let verdict = verdict(&row(Ok(with_disk)), Backend::Wsl, None);
        assert!(verdict.resumable(), "{:?}", verdict.blocked);
        assert!(
            verdict
                .notes
                .iter()
                .any(|n| n.contains("when the machine resumes")),
            "{:?}",
            verdict.notes
        );
    }

    /// A disk that really has changed is the refusal that matters most, and it
    /// has to name the disk and what moved.
    #[test]
    fn a_disk_that_grew_is_refused_by_name() {
        let dir = std::env::temp_dir().join(format!("entangled-snap-gui-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let disk = dir.join("root.raw");
        std::fs::write(&disk, [0u8; 64]).expect("write");
        let taken = FileFingerprint::measure(FileRole::Disk, &disk);
        std::fs::write(&disk, [0u8; 128]).expect("grow");

        let mut with_disk = facts();
        // Whatever this host could restore, so the disk refusal is the *only*
        // one and the test does not accidentally assert the host check.
        with_disk.host = host_of(Backend::Native).unwrap_or(HostKind::KvmLinux);
        with_disk.files = vec![taken];
        let verdict = verdict(&row(Ok(with_disk)), Backend::Native, None);
        assert_eq!(verdict.blocked.len(), 1, "{:?}", verdict.blocked);
        let message = verdict.blocked.first().expect("refused");
        assert!(message.contains("root.raw"), "{message}");
        assert!(message.contains("was 64, is 128"), "{message}");
        assert!(message.contains("corrupt"), "{message}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_snapshot_sits_beside_its_profile_under_the_machines_name() {
        assert_eq!(
            path_for(&vm()),
            PathBuf::from("/vms/ubuntu-lab.esnap"),
            "the engine's bare `save` writes exactly here"
        );
    }

    #[test]
    fn the_delete_confirmation_says_what_goes_and_what_stays() {
        let text = what_is_lost(&row(Ok(facts())));
        assert!(text.contains("ubuntu-lab"), "{text}");
        assert!(text.contains("2026-08-21"), "{text}");
        assert!(
            text.contains("disks and its profile are untouched"),
            "{text}"
        );
    }
}
