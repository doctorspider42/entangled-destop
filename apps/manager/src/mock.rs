//! Deterministic, side-effect-free data for reviewing the manager UI.
//!
//! Nothing here probes the host or creates files. `--mock` uses these values
//! instead of starting the scanner, metrics sampler or CLI children.

use std::collections::HashMap;
use std::path::PathBuf;

use crate::app::Status;
use crate::discovery::{DiskAttachment, DiskInfo, DiskRow, Scan, VmEntry};
use crate::metrics::{HostStats, Snapshot, VmStats};

const GIB: u64 = 1024 * 1024 * 1024;

pub fn scan() -> Scan {
    let root = PathBuf::from("mock-vms");
    let nebula_disk = root.join("nebula-dev.raw");
    let ubuntu_disk = root.join("ubuntu-lab.raw");
    let scratch_disk = root.join("scratch-data.raw");

    let vms = vec![
        VmEntry {
            name: "nebula-dev".into(),
            profile_path: root.join("nebula-dev.toml"),
            memory_mib: 8192,
            vcpus: 8,
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
            display: (1280, 800),
            network_interface: None,
            uefi: false,
            disks: Vec::new(),
        },
    ];

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
        problems: Vec::new(),
    }
}

pub fn statuses() -> HashMap<String, Status> {
    [
        ("nebula-dev".to_string(), Status::Running),
        ("ubuntu-lab".to_string(), Status::Stopped),
        ("installer-preview".to_string(), Status::Installing),
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
        assert_eq!(scan.vms.len(), 3);
        assert_eq!(scan.disks.len(), 3);
        assert!(statuses.values().any(|status| *status == Status::Running));
        assert!(statuses
            .values()
            .any(|status| *status == Status::Installing));
        assert!(scan.disks.iter().any(|disk| disk.attachments.is_empty()));
    }
}
