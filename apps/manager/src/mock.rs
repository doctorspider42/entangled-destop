//! Deterministic, side-effect-free data for reviewing the manager UI.
//!
//! Nothing here probes the host or creates files. `--mock` uses these values
//! instead of starting the scanner, metrics sampler or CLI children.

use std::collections::HashMap;
use std::path::PathBuf;

use crate::app::Status;
use crate::discovery::{DiskAttachment, DiskInfo, DiskRow, Scan, VmEntry};
use crate::metrics::{HostStats, Snapshot, VmStats};
use crate::snapshots::{SnapshotFacts, SnapshotRow};

const GIB: u64 = 1024 * 1024 * 1024;

pub fn scan() -> Scan {
    let root = PathBuf::from("mock-vms");
    let nebula_disk = root.join("nebula-dev.raw");
    let ubuntu_disk = root.join("ubuntu-lab.raw");
    let scratch_disk = root.join("scratch-data.raw");
    let aurora_disk = root.join("aurora-lab.raw");
    let orion_disk = root.join("orion-build.raw");

    // Name order, because that is what `discovery::scan` sorts a real
    // directory into — a fixture whose grid is laid out differently from every
    // real one is a fixture that hides layout problems.
    let mut vms = vec![
        VmEntry {
            name: "nebula-dev".into(),
            profile_path: root.join("nebula-dev.toml"),
            memory_mib: 8192,
            vcpus: 8,
            transport: control_api::VirtioTransport::Pci,
            display: (2560, 1440),
            network_interface: Some("usernet".into()),
            uefi: false,
            disks: vec![disk_info(&nebula_disk, 64 * GIB, 18 * GIB)],
        },
        VmEntry {
            name: "ubuntu-lab".into(),
            profile_path: root.join("ubuntu-lab.toml"),
            memory_mib: 4096,
            vcpus: 4,
            transport: control_api::VirtioTransport::Pci,
            display: (1920, 1080),
            network_interface: Some("entangled0".into()),
            // An installed Ubuntu boots through the firmware, so the mock has
            // one of each — the run pre-flight warns per boot mode.
            uefi: true,
            disks: vec![disk_info(&ubuntu_disk, 32 * GIB, 9 * GIB)],
        },
        VmEntry {
            name: "installer-preview".into(),
            profile_path: root.join("installer-preview.toml"),
            memory_mib: 2048,
            vcpus: 2,
            transport: control_api::VirtioTransport::Pci,
            display: (1280, 800),
            network_interface: None,
            uefi: false,
            disks: Vec::new(),
        },
        // The third resting state: stopped, but with a session on disk. Its
        // shape matches `aurora_snapshot()`, so the card really does read as
        // resumable rather than being told to.
        VmEntry {
            name: "aurora-lab".into(),
            profile_path: root.join("aurora-lab.toml"),
            memory_mib: 4096,
            vcpus: 4,
            transport: control_api::VirtioTransport::Pci,
            display: (1920, 1080),
            network_interface: Some("usernet".into()),
            uefi: true,
            disks: vec![disk_info(&aurora_disk, 40 * GIB, 11 * GIB)],
        },
        // And the state in between: on its way into a file.
        VmEntry {
            name: "orion-build".into(),
            profile_path: root.join("orion-build.toml"),
            memory_mib: 2048,
            vcpus: 2,
            transport: control_api::VirtioTransport::Pci,
            display: (1280, 800),
            network_interface: Some("usernet".into()),
            uefi: false,
            disks: vec![disk_info(&orion_disk, 16 * GIB, 3 * GIB)],
        },
    ];
    vms.sort_by(|a, b| a.name.cmp(&b.name));

    let disks = vec![
        disk_row(
            &nebula_disk,
            64 * GIB,
            18 * GIB,
            false,
            vec![attachment("nebula-dev", &root, &nebula_disk)],
            "GPT · EFI System 512 MiB · Linux filesystem 63.5 GiB",
        ),
        disk_row(
            &ubuntu_disk,
            32 * GIB,
            9 * GIB,
            true,
            vec![attachment("ubuntu-lab", &root, &ubuntu_disk)],
            "GPT · EFI System 1 GiB · Linux filesystem 31 GiB",
        ),
        disk_row(
            &scratch_disk,
            12 * GIB,
            640 * 1024 * 1024,
            false,
            Vec::new(),
            "MBR · Linux filesystem 12 GiB",
        ),
    ];

    Scan {
        vms,
        disks,
        snapshots: snapshots(&root),
        problems: Vec::new(),
    }
}

/// Roughly when the fixture's snapshots were taken, so "3 hours ago" reads the
/// same in a screenshot taken at any time of day.
fn taken(hours_ago: i64) -> i64 {
    vm_snapshot::meta::now_unix() - hours_ago * 3600
}

/// The three cases the view exists to tell apart: one that resumes, one that
/// was taken on the other hypervisor, and one this build cannot read at all.
fn snapshots(root: &std::path::Path) -> Vec<SnapshotRow> {
    vec![
        SnapshotRow {
            path: root.join("aurora-lab.esnap"),
            file_name: "aurora-lab.esnap".into(),
            apparent_bytes: 1_140_850_688,
            allocated_bytes: Some(1_040_000_000),
            facts: Ok(aurora_snapshot()),
        },
        SnapshotRow {
            path: root.join("kepler-old.esnap"),
            file_name: "kepler-old.esnap".into(),
            apparent_bytes: 565_182_464,
            allocated_bytes: Some(520_000_000),
            facts: Ok(SnapshotFacts {
                vm_name: "kepler-old".into(),
                created_unix: taken(52),
                writer: "entangled 0.2.0".into(),
                // Deliberately the host this build is *not*, so the
                // foreign-hypervisor refusal is on screen on both hosts.
                host: if cfg!(windows) {
                    vm_snapshot::HostKind::KvmLinux
                } else {
                    vm_snapshot::HostKind::WhpWindows
                },
                vcpus: 2,
                memory_bytes: 2048 << 20,
                transport: "pci".into(),
                boot_mode: "uefi".into(),
                files: Vec::new(),
                sections: 11,
            }),
        },
        SnapshotRow {
            path: root.join("vega-legacy.esnap"),
            file_name: "vega-legacy.esnap".into(),
            apparent_bytes: 213_909_504,
            allocated_bytes: Some(198_000_000),
            facts: Err(
                "snapshot format version 8 cannot be restored by this build (it writes and \
                 reads version 1)"
                    .into(),
            ),
        },
    ]
}

fn aurora_snapshot() -> SnapshotFacts {
    SnapshotFacts {
        vm_name: "aurora-lab".into(),
        created_unix: taken(3),
        writer: concat!("entangled ", env!("CARGO_PKG_VERSION")).into(),
        // Whatever this build could restore, so the fixture's healthy row is
        // healthy on both hosts.
        host: vm_snapshot::HostKind::current().unwrap_or(vm_snapshot::HostKind::KvmLinux),
        vcpus: 4,
        memory_bytes: 4096 << 20,
        transport: "pci".into(),
        boot_mode: "uefi".into(),
        files: Vec::new(),
        sections: 14,
    }
}

pub fn statuses() -> HashMap<String, Status> {
    [
        ("nebula-dev".to_string(), Status::Running),
        ("ubuntu-lab".to_string(), Status::Stopped),
        ("installer-preview".to_string(), Status::Installing),
        // `aurora-lab` is left out on purpose: its Suspended badge has to come
        // from the snapshot row through `status_of`, the way a real one does,
        // rather than from this table.
        ("orion-build".to_string(), Status::Suspending),
    ]
    .into_iter()
    .collect()
}

pub fn metrics() -> Snapshot {
    let mut vms = HashMap::new();
    vms.insert(
        "nebula-dev".into(),
        VmStats {
            cpu_percent: Some(37.0),
            rss_bytes: Some(5 * GIB + 320 * 1024 * 1024),
            history: vec![12.0, 18.0, 25.0, 21.0, 32.0, 37.0],
        },
    );
    Snapshot {
        host: HostStats {
            cpu_percent: Some(18.0),
            history: vec![9.0, 13.0, 11.0, 16.0, 18.0],
            mem_total: Some(32 * GIB),
            mem_available: Some(19 * GIB),
            vm_dir_space: Some((412 * GIB, 953 * GIB)),
        },
        vms,
    }
}

fn disk_info(path: &std::path::Path, apparent: u64, allocated: u64) -> DiskInfo {
    DiskInfo {
        declared: path.to_path_buf(),
        resolved: path.to_path_buf(),
        exists: true,
        writable: true,
        size_bytes: apparent,
        allocated_bytes: Some(allocated),
    }
}

fn attachment(vm: &str, root: &std::path::Path, disk: &std::path::Path) -> DiskAttachment {
    DiskAttachment {
        vm: vm.into(),
        profile: root.join(format!("{vm}.toml")),
        declared: disk.to_path_buf(),
        writable: true,
    }
}

fn disk_row(
    path: &std::path::Path,
    apparent: u64,
    allocated: u64,
    nvram: bool,
    attachments: Vec<DiskAttachment>,
    summary: &str,
) -> DiskRow {
    DiskRow {
        path: path.to_path_buf(),
        file_name: path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| "mock.raw".into()),
        exists: true,
        apparent_bytes: apparent,
        allocated_bytes: Some(allocated),
        attachments,
        nvram,
        summary: Ok(summary.into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fixture_covers_machine_disk_and_status_states() {
        let scan = scan();
        let statuses = statuses();
        assert_eq!(scan.vms.len(), 5);
        assert_eq!(scan.disks.len(), 3);
        assert!(statuses.values().any(|status| *status == Status::Running));
        assert!(statuses
            .values()
            .any(|status| *status == Status::Installing));
        assert!(statuses
            .values()
            .any(|status| *status == Status::Suspending));
        assert!(scan.disks.iter().any(|disk| disk.attachments.is_empty()));
    }

    /// The Snapshots view has three things to say — resumable, taken elsewhere,
    /// unreadable — and a fixture that shows only the happy one is a fixture
    /// nobody reviews the other two in.
    #[test]
    fn the_snapshot_fixture_shows_every_verdict_on_both_hosts() {
        let scan = scan();
        assert_eq!(scan.snapshots.len(), 3);
        let backend = crate::backend::Backend::Native;
        let vms = &scan.vms;
        let find = |name: &str| vms.iter().find(|vm| vm.name == name);

        let healthy = &scan.snapshots[0];
        let verdict = crate::snapshots::verdict(healthy, backend, find("aurora-lab"));
        assert!(verdict.resumable(), "{:?}", verdict.blocked);
        // The card only shows Suspended when the snapshot sits exactly where a
        // bare `save` would have written it.
        assert_eq!(
            healthy.path,
            crate::snapshots::path_for(find("aurora-lab").expect("machine"))
        );

        let foreign = crate::snapshots::verdict(&scan.snapshots[1], backend, None);
        assert!(!foreign.resumable());
        let unreadable = crate::snapshots::verdict(&scan.snapshots[2], backend, None);
        assert!(!unreadable.resumable());
    }
}
