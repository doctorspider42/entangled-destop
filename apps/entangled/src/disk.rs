//! `entangled disk …` — the CLI surface over the shared `disk-image` crate
//! (backlog MVP-409/1001 plus the disk-management set: inspect, resize, rm,
//! move). All logic lives in `disk-image`, where the manager GUI links it too;
//! this module only parses arguments and renders results.

use std::path::{Path, PathBuf};

// The installers (`install.rs`, `install_ubuntu.rs`) create their target disks
// through this module's namespace.
pub use disk_image::{create_raw, parse_size};

use disk_image::{format_bytes, DiskReport};

/// `disk create <path> --size N`.
pub fn create(path: &Path, size: &str) -> Result<(), String> {
    let bytes = parse_size(size).map_err(|e| e.to_string())?;
    create_raw(path, bytes).map_err(|e| e.to_string())?;
    println!("created {} ({} bytes, sparse)", path.display(), bytes);
    Ok(())
}

/// `disk inspect <path> [--json]`.
pub fn inspect(path: &Path, json: bool) -> Result<(), String> {
    let report = disk_image::inspect_disk(path).map_err(|e| e.to_string())?;
    if json {
        println!("{}", report.to_json());
    } else {
        print!("{}", render_report(&report));
    }
    Ok(())
}

/// `disk resize <path> --size N` — grow-only; shrinking is refused by
/// `disk-image` with an error naming exactly what would be lost.
pub fn resize(path: &Path, size: &str) -> Result<(), String> {
    let bytes = parse_size(size).map_err(|e| e.to_string())?;
    let outcome = disk_image::resize_raw(path, bytes).map_err(|e| e.to_string())?;
    if !outcome.grew() {
        println!(
            "{} already is {} ({} bytes) — nothing to do",
            path.display(),
            format_bytes(outcome.new_bytes),
            outcome.new_bytes
        );
        return Ok(());
    }
    println!(
        "grew {} from {} to {} (sparse — the new space occupies no disk until written)",
        path.display(),
        format_bytes(outcome.previous_bytes),
        format_bytes(outcome.new_bytes)
    );
    println!(
        "note: the partition table inside the image still describes the old size; grow it \
         inside the guest (e.g. growpart /dev/vda N && resize2fs), and for GPT disks let the \
         tool relocate the backup header to the new end"
    );
    Ok(())
}

/// `disk rm <path> [--force]` — refuses while any VM profile next to the disk
/// or in the manager's VM directory references it.
pub fn rm(path: &Path, force: bool) -> Result<(), String> {
    let dirs = disk_image::default_scan_dirs(path);
    let outcome = disk_image::remove_disk(path, force, &dirs).map_err(|e| e.to_string())?;
    for removed in &outcome.removed {
        if removed.extension().is_some_and(|ext| ext == "nvram") {
            println!(
                "removed {} (the disk's UEFI variable-store sidecar)",
                removed.display()
            );
        } else {
            println!("removed {}", removed.display());
        }
    }
    for reference in &outcome.overridden {
        println!("warning: {reference} still references the removed disk");
    }
    Ok(())
}

/// `disk move <path> --to <dir>` — sparse-preserving, verified; updates every
/// profile (next to the disk and in the manager's VM directory) that
/// references the disk or its `.nvram` sidecar.
pub fn mv(path: &Path, dest: &Path) -> Result<(), String> {
    let dirs = disk_image::default_scan_dirs(path);
    let profiles: Vec<PathBuf> = disk_image::find_references(path, &dirs)
        .map_err(|e| e.to_string())?
        .into_iter()
        .filter_map(|r| r.vm.is_some().then_some(r.profile))
        .collect();

    // Worst-case warning: the copy only needs the allocated data, but a guest
    // can later grow the image to its apparent size.
    if let (Some((data, apparent)), Some((free, _))) = (
        disk_image::relocate::copy_bill(path),
        disk_image::disk_space(dest),
    ) {
        if free < apparent {
            eprintln!(
                "warning: {} has {} free, less than the image's {} apparent size — the move \
                 copies only the {} of data, but the guest can grow the image beyond the \
                 destination's space later",
                dest.display(),
                format_bytes(free),
                format_bytes(apparent),
                format_bytes(data),
            );
        }
    }

    // Coarse progress on stderr: one line per ~10%.
    let mut last_decile = 0u64;
    let mut progress = |done: u64, total: u64| {
        if total == 0 {
            return;
        }
        let decile = done * 10 / total;
        if decile > last_decile || (done == total && last_decile < 10) {
            last_decile = decile;
            eprintln!("  copied {} / {}", format_bytes(done), format_bytes(total));
        }
    };
    let outcome =
        disk_image::move_disk(path, dest, &profiles, &mut progress).map_err(|e| e.to_string())?;

    for (from, to) in &outcome.moved {
        println!("moved {} -> {}", from.display(), to.display());
    }
    println!(
        "copied {} of data ({} apparent), verified against the source before deleting it",
        format_bytes(outcome.data_bytes),
        format_bytes(outcome.apparent_bytes)
    );
    for profile in &outcome.updated_profiles {
        println!("updated profile {}", profile.display());
    }
    for leftover in &outcome.leftover_sources {
        println!(
            "warning: could not delete the source {} — the verified copy is in place, delete \
             the leftover by hand",
            leftover.display()
        );
    }
    Ok(())
}

/// The human-readable `disk inspect` rendering.
fn render_report(report: &DiskReport) -> String {
    use std::fmt::Write as _;
    let mut out = String::new();
    let _ = writeln!(out, "{}", report.path.display());

    let mut size = format!(
        "{} apparent ({} bytes)",
        format_bytes(report.apparent_bytes),
        report.apparent_bytes
    );
    match report.allocated_bytes {
        Some(allocated) => {
            let _ = write!(size, " · {} on disk", format_bytes(allocated));
        }
        None => size.push_str(" · on-disk size unavailable"),
    }
    let _ = writeln!(out, "  size:   {size}");

    let table = match (&report.table, &report.disk_guid) {
        (disk_image::TableKind::Gpt, Some(guid)) => format!("GPT (disk GUID {guid})"),
        (kind, _) => kind.to_string(),
    };
    let _ = writeln!(out, "  table:  {table}");

    match &report.nvram_sidecar {
        Some(nvram) => {
            let _ = writeln!(
                out,
                "  nvram:  {} (UEFI variable store — travels with the disk)",
                nvram.display()
            );
        }
        None => {
            let _ = writeln!(out, "  nvram:  no .nvram sidecar");
        }
    }

    if report.partitions.is_empty() {
        let _ = writeln!(out, "  partitions: none");
    } else {
        let _ = writeln!(out, "  partitions:");
        for p in &report.partitions {
            let mut line = format!(
                "    {}  {:<20} {:>9}  LBA {}+{}",
                p.index,
                p.type_name,
                format_bytes(p.size_bytes),
                p.first_lba,
                p.sectors
            );
            if let Some(name) = &p.name {
                let _ = write!(line, "  \"{name}\"");
            }
            if let Some(uuid) = &p.ext4_uuid {
                let _ = write!(line, "  ext4 {uuid}");
            }
            if let Some(label) = &p.ext4_label {
                let _ = write!(line, "  label \"{label}\"");
            }
            let _ = writeln!(out, "{line}");
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The CLI rendering is string-assembled here (not in `disk-image`), so the
    /// load-bearing fields get a smoke test against a synthetic report.
    #[test]
    fn renders_the_report_fields() {
        let report = DiskReport {
            path: PathBuf::from("images/x.raw"),
            apparent_bytes: 16 << 30,
            allocated_bytes: Some(4 << 30),
            table: disk_image::TableKind::Gpt,
            disk_guid: Some("ABABABAB-ABAB-ABAB-ABAB-ABABABABABAB".into()),
            partitions: vec![disk_image::PartitionReport {
                index: 1,
                type_name: "EFI System".into(),
                type_id: "C12A7328-F81F-11D2-BA4B-00A0C93EC93B".into(),
                name: Some("EFI System Partition".into()),
                first_lba: 2048,
                sectors: 2048,
                size_bytes: 2048 * 512,
                ext4_uuid: None,
                ext4_label: None,
            }],
            nvram_sidecar: Some(PathBuf::from("images/x.nvram")),
        };
        let text = render_report(&report);
        assert!(text.contains("16.0 GiB apparent"), "{text}");
        assert!(text.contains("4.0 GiB on disk"), "{text}");
        assert!(text.contains("GPT (disk GUID ABABABAB"), "{text}");
        assert!(text.contains("x.nvram"), "{text}");
        assert!(text.contains("EFI System"), "{text}");
        assert!(text.contains("LBA 2048+2048"), "{text}");
    }

    #[test]
    fn renders_a_blank_disk_without_pretending_a_table() {
        let report = DiskReport {
            path: PathBuf::from("fresh.raw"),
            apparent_bytes: 1 << 20,
            allocated_bytes: None,
            table: disk_image::TableKind::None,
            disk_guid: None,
            partitions: vec![],
            nvram_sidecar: None,
        };
        let text = render_report(&report);
        assert!(text.contains("table:  none"), "{text}");
        assert!(text.contains("partitions: none"), "{text}");
        assert!(text.contains("on-disk size unavailable"), "{text}");
        assert!(text.contains("no .nvram sidecar"), "{text}");
    }
}
