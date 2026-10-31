//! Manual installation from an unmodified local x86-64 UEFI ISO.
//! Keep the profile and media across interrupted installs; the user ejects the
//! disc in the manager after installing. No distro-specific disk inspection.

use std::io::Write;
use std::path::Path;

use control_api::{
    BootMode, BootSection, CdromSection, DiskSection, DisplaySection, VirtioTransport, VmConfig,
};

use crate::{install, run_vm, InstallArgs};

fn absolute(path: &Path) -> Result<std::path::PathBuf, String> {
    std::path::absolute(path).map_err(|e| e.to_string())
}

pub fn run(args: &InstallArgs) -> Result<(), String> {
    let (cfg, snapshot) = prepare(args)?;
    run_vm::run(
        cfg,
        run_vm::RunOptions {
            control_stdin: args.control_stdin,
            snapshot: Some(snapshot),
            ..Default::default()
        },
    )
}

fn prepare(args: &InstallArgs) -> Result<(VmConfig, std::path::PathBuf), String> {
    if args.auto
        || args.preseed.is_some()
        || args.autoinstall.is_some()
        || args.kickstart.is_some()
        || args.venus
        || args.headless
    {
        return Err("install other runs interactively in a window; automation, --venus and --headless are not supported".into());
    }
    let iso = absolute(
        args.iso
            .as_deref()
            .ok_or("install other requires --iso with a Linux x86-64 UEFI installer image")?,
    )?;
    if !iso.is_file() {
        return Err(format!("installer ISO {} was not found", iso.display()));
    }
    let firmware = absolute(
        &crate::firmware::resolve(args.firmware.as_deref())
            .map_err(|e| e.to_string())?
            .path,
    )?;
    let target = absolute(&install::target_disk(args)?)?;
    if target == iso || (target.exists() && target.canonicalize().ok() == iso.canonicalize().ok()) {
        return Err("the installer ISO cannot also be the writable target disk".into());
    }
    let name = install::vm_name(args);
    if name.is_empty()
        || name.len() > 63
        || !name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'))
    {
        return Err("machine name must be 1..63 letters, digits, '-' or '_'".into());
    }
    let profile_path = target.with_file_name(format!("{name}.toml"));
    let nvram = target.with_file_name(format!("{name}.nvram"));
    if profile_path.exists() || nvram.exists() {
        return Err("a profile or NVRAM store with this name already exists; start the existing machine to continue its installation".into());
    }
    let net = install::net_plan(&args.network, &args.interface)?;
    let cfg = VmConfig {
        architecture: Default::default(),
        name: name.clone(),
        memory_mib: args.memory_mib,
        vcpus: install::installed_vcpus_here(args),
        transport: VirtioTransport::Pci,
        boot: BootSection {
            mode: BootMode::Uefi,
            firmware: Some(firmware),
            nvram: Some(nvram),
            ..Default::default()
        },
        disks: vec![DiskSection {
            path: target.clone(),
            writable: true,
        }],
        cdrom: Some(CdromSection { path: iso }),
        optical: vec![],
        network: net.machine_section(&name, &target),
        display: DisplaySection {
            refresh_hz: install::installed_refresh_hz_here(args),
            ..Default::default()
        },
        sound: Default::default(),
        gamepad: Default::default(),
        shares: Vec::new(),
        reclaim_memory: false,
        ignore_msrs: false,
        hyperv_enlightenments: false,
    };
    let text = toml::to_string_pretty(&cfg).map_err(|e| e.to_string())?;
    let cfg = VmConfig::from_toml(&text).map_err(|e| e.to_string())?;
    let bytes = crate::disk::parse_size(&args.size).map_err(|e| e.to_string())?;
    if !target.exists() {
        crate::disk::create_raw(&target, bytes).map_err(|e| e.to_string())?;
    }
    // Never replace another machine's profile, including a concurrent creator.
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&profile_path)
        .map_err(|e| format!("cannot create {}: {e}", profile_path.display()))?;
    if let Err(error) = file.write_all(text.as_bytes()) {
        drop(file);
        let _ = std::fs::remove_file(&profile_path);
        return Err(format!("cannot write {}: {error}", profile_path.display()));
    }
    drop(file);
    tracing::info!(profile = %profile_path.display(), "manual installation: use the guest window; firmware and bootloader output is in the serial console. After installing, power off and eject the ISO in machine settings before starting again");
    Ok((cfg, target.with_file_name(format!("{name}.esnap"))))
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[test]
    fn manual_install_persists_media_and_never_replaces_existing_files() {
        let dir =
            std::env::temp_dir().join(format!("entangled-manual-install-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let iso = dir.join("installer.iso");
        let firmware = dir.join("firmware.fd");
        let disk = dir.join("manual.raw");
        std::fs::write(&iso, b"test ISO").unwrap();
        std::fs::write(&firmware, b"test firmware").unwrap();
        let cli = crate::Cli::try_parse_from([
            "entangled",
            "install",
            "other",
            "--name",
            "manual",
            "--iso",
            iso.to_str().unwrap(),
            "--firmware",
            firmware.to_str().unwrap(),
            "--disk",
            disk.to_str().unwrap(),
            "--size",
            "1M",
            "--memory-mib",
            "4096",
            "--vcpus",
            "3",
            "--control-stdin",
        ])
        .unwrap();
        let crate::Command::Install(mut args) = cli.command else {
            panic!("install command")
        };
        args.auto = true;
        assert!(prepare(&args).is_err());
        assert!(!disk.exists());
        args.auto = false;
        let (cfg, _) = prepare(&args).unwrap();
        assert_eq!(cfg.vcpus, 3);
        assert_eq!(cfg.memory_mib, 4096);
        assert_eq!(cfg.cdrom.as_ref().unwrap().path, iso);
        assert_eq!(std::fs::metadata(&disk).unwrap().len(), 1 << 20);
        let profile = dir.join("manual.toml");
        let before = std::fs::read(&profile).unwrap();
        assert!(prepare(&args).is_err());
        assert_eq!(std::fs::read(&profile).unwrap(), before);
        std::fs::remove_file(&profile).unwrap();
        std::fs::write(&disk, b"existing disk contents").unwrap();
        prepare(&args).unwrap();
        assert_eq!(std::fs::read(&disk).unwrap(), b"existing disk contents");
        // Ejecting changes only the profile: no ext4/root-layout assumptions.
        let mut saved = VmConfig::from_toml(std::str::from_utf8(&before).unwrap()).unwrap();
        saved.cdrom = None;
        let saved = VmConfig::from_toml(&toml::to_string(&saved).unwrap()).unwrap();
        assert_eq!(saved.boot.nvram, cfg.boot.nvram);
        assert_eq!(saved.disks, cfg.disks);
        for path in [&profile, &disk, &iso, &firmware] {
            std::fs::remove_file(path).unwrap();
        }
        std::fs::remove_dir(&dir).unwrap();
    }
}
