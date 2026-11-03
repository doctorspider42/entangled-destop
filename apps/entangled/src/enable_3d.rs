//! `entangled enable-3d <profile>`: 3D graphics for a machine that was
//! installed without them.
//!
//! 3D is two halves, and only one of them is ours to set from outside the
//! guest:
//!
//! * the **host** half is the profile: `[display] venus = true` (plus, for a
//!   Windows guest on our own driver, `render_node = true`, ADR-0009);
//! * the **guest** half is configuration inside the installed system, which
//!   needs its administrator: on Linux the four files `install ubuntu --venus`
//!   writes at install time ([`crate::seed::venus_guest_script`]), on Windows
//!   test-signing, the certificate, the driver and the Vulkan runtime — the 3D
//!   half of the first-logon script ([`crate::install_windows::gpu3d_disc`]).
//!
//! So this command does the first and *delivers* the second: it writes a small
//! disc beside the profile, attaches it, and says what to run inside the
//! machine. The disc is the same files the installer would have used, so a
//! machine given 3D afterwards is the machine `--venus`/`--gpu3d` would have
//! made. The guest half cannot be done from here: the guest's disk is the
//! guest's, and writing into a filesystem a hypervisor does not own is how
//! filesystems get corrupted.

use std::path::{Path, PathBuf};

use control_api::{DiskSection, OpticalSection, VmConfig, MAX_OPTICAL};
use disk_image::optical::iso9660::IsoBuilder;

use control_api::enable_3d::{DISC_LABEL, LINUX_COMMAND, LINUX_SCRIPT, WINDOWS_SCRIPT};

/// What a GPU desktop gets at least — the floor `install ubuntu --venus`
/// applies too.
const VENUS_MEMORY_MIB: u64 = 4096;

pub fn run(profile: &Path, package: Option<&Path>) -> Result<(), String> {
    let text = std::fs::read_to_string(profile)
        .map_err(|e| format!("cannot read {}: {e}", profile.display()))?;
    let mut cfg = VmConfig::from_toml(&text).map_err(|e| format!("{}: {e}", profile.display()))?;
    let disc = profile.with_file_name(format!("{}-3d.iso", cfg.name));
    let windows = cfg.hyperv_enlightenments;

    if windows {
        let dir = match package {
            Some(dir) if dir != Path::new(crate::install_windows::GPU3D_AUTO) => {
                std::path::absolute(dir).map_err(|e| e.to_string())?
            }
            _ => crate::install_windows::media(crate::windows_media::Media::Gpu3d)?,
        };
        let bytes = crate::install_windows::gpu3d_disc(&dir)?;
        write_disc(&disc, &bytes)?;
        attach_windows(&mut cfg, &disc)?;
        cfg.display.venus = true;
        cfg.display.render_node = crate::install_windows::is_own_gpu_package(&dir);
    } else {
        if package.is_some() {
            return Err(
                "--package is for Windows guests; a Linux guest's 3D driver is \
                        already part of its Mesa"
                    .to_string(),
            );
        }
        let mut builder = IsoBuilder::new(DISC_LABEL);
        builder
            .add_file(LINUX_SCRIPT, crate::seed::venus_guest_script().into_bytes())
            .map_err(|e| e.to_string())?;
        write_disc(&disc, &builder.build())?;
        attach_linux(&mut cfg, &disc);
        cfg.display.venus = true;
        cfg.memory_mib = cfg.memory_mib.max(VENUS_MEMORY_MIB);
    }

    // The same check `entangled run` makes, before the file is replaced: a
    // profile this command breaks must never be written.
    let out = toml::to_string_pretty(&cfg).map_err(|e| e.to_string())?;
    VmConfig::from_toml(&out).map_err(|e| format!("the 3D profile would not load: {e}"))?;
    std::fs::write(profile, out).map_err(|e| format!("cannot write {}: {e}", profile.display()))?;

    if let Err(why) = crate::doctor::venus_host() {
        eprintln!(
            "warning: this computer cannot serve 3D yet ({why}); the machine will refuse to \
             start until it can — `entangled doctor` says what is missing"
        );
    }
    println!(
        "3D is on for {}. The disc {} is in its drive.",
        cfg.name,
        disc.display()
    );
    if windows {
        println!(
            "Start the machine, open the {DISC_LABEL} drive in File Explorer and run \
             \"{WINDOWS_SCRIPT}\". Windows restarts when it is done."
        );
    } else {
        println!("Start the machine, open a terminal in it and run:\n  {LINUX_COMMAND}");
        println!("Then log out and back in.");
    }
    Ok(())
}

fn write_disc(path: &Path, bytes: &[u8]) -> Result<(), String> {
    std::fs::write(path, bytes).map_err(|e| format!("cannot write {}: {e}", path.display()))
}

/// A Windows guest reads the disc from an AHCI DVD drive, which it has an
/// in-box driver for (ADR-0008 §4).
fn attach_windows(cfg: &mut VmConfig, disc: &Path) -> Result<(), String> {
    if cfg.optical.iter().any(|o| o.path == disc) {
        return Ok(());
    }
    if cfg.optical.len() >= MAX_OPTICAL {
        return Err(format!(
            "{} already has {MAX_OPTICAL} discs in its drives; eject one first",
            cfg.name
        ));
    }
    cfg.optical.push(OpticalSection {
        path: disc.to_path_buf(),
    });
    Ok(())
}

/// A Linux guest reads it as one more read-only disk, after every other: that
/// works on either transport and either boot mode, and appending it shifts no
/// disk's name.
fn attach_linux(cfg: &mut VmConfig, disc: &Path) {
    if cfg.disks.iter().any(|d| d.path == disc) {
        return;
    }
    cfg.disks.push(DiskSection {
        path: PathBuf::from(disc),
        writable: false,
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn linux_profile(dir: &Path) -> PathBuf {
        let profile = dir.join("ubuntu-1.toml");
        let disk = dir.join("ubuntu-1.raw");
        let firmware = dir.join("CLOUDHV.fd");
        std::fs::write(&disk, b"").unwrap();
        std::fs::write(&firmware, b"").unwrap();
        std::fs::write(
            &profile,
            format!(
                "name = \"ubuntu-1\"\nmemory_mib = 2048\nvcpus = 2\ntransport = \"pci\"\n\n\
                 [boot]\nmode = \"uefi\"\n\
                 firmware = {:?}\n\n[[disk]]\npath = {:?}\nwritable = true\n",
                firmware.display().to_string(),
                disk.display().to_string()
            ),
        )
        .unwrap();
        profile
    }

    #[test]
    fn a_linux_machine_gets_venus_memory_and_a_read_only_disc() {
        let dir = std::env::temp_dir().join(format!("enable-3d-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let profile = linux_profile(&dir);
        let first = run(&profile, None);
        // Twice is the same as once: no second disc.
        let second = run(&profile, None);
        let text = std::fs::read_to_string(&profile).unwrap();
        let disc = dir.join("ubuntu-1-3d.iso");
        let iso = std::fs::read(&disc).unwrap_or_default();
        let _ = std::fs::remove_dir_all(&dir);
        first.unwrap();
        second.unwrap();
        let cfg = VmConfig::from_toml(&text).unwrap();
        assert!(cfg.display.venus);
        assert!(!cfg.display.render_node);
        assert_eq!(cfg.memory_mib, VENUS_MEMORY_MIB);
        assert_eq!(cfg.disks.len(), 2);
        assert!(!cfg.disks[1].writable);
        assert_eq!(cfg.disks[1].path, disc);
        assert!(iso
            .windows(DISC_LABEL.len())
            .any(|w| w == DISC_LABEL.as_bytes()));
    }

    #[test]
    fn a_linux_machine_refuses_a_windows_package() {
        let dir = std::env::temp_dir().join(format!("enable-3d-pkg-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let profile = linux_profile(&dir);
        let refused = run(&profile, Some(Path::new("auto")));
        let _ = std::fs::remove_dir_all(&dir);
        assert!(refused.unwrap_err().contains("Windows guests"));
    }

    #[test]
    fn the_guest_script_writes_what_the_installer_writes() {
        let script = crate::seed::venus_guest_script();
        for path in [
            "/etc/drirc",
            crate::seed::VENUS_ENVIRONMENT_D,
            crate::seed::VENUS_GSCHEMA_OVERRIDE,
        ] {
            assert!(script.contains(&format!("> {path}")), "{path}");
        }
        assert!(script.contains("MESA_LOADER_DRIVER_OVERRIDE=zink"));
        assert!(script.contains("usermod -aG render \"$user\""));
        assert!(script.starts_with("#!/bin/sh\n"));
        assert!(!script.contains('\r'));
    }
}
