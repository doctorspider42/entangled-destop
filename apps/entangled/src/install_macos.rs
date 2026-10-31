//! Native Ubuntu ARM64 cloud-rootfs provisioning. No host mounts or QEMU.
use crate::{run_vm, ubuntu_arm64_image, InstallArgs};
use control_api::{
    BootMode, BootSection, DiskSection, DisplaySection, GuestArchitecture, NetworkBackend,
    NetworkSection, ShareSection, VirtioTransport, VmConfig,
};
use std::fs::OpenOptions;
use std::io::Write;
use std::time::{SystemTime, UNIX_EPOCH};

pub fn run(args: &InstallArgs) -> Result<(), String> {
    if !args.distro.eq_ignore_ascii_case("ubuntu") || !args.auto {
        return Err("native ARM64 provisioning requires `install ubuntu --auto`".into());
    }
    if args.iso.is_some()
        || args.firmware.is_some()
        || args.preseed.is_some()
        || args.autoinstall.is_some()
        || args.kickstart.is_some()
    {
        return Err("native ARM64 provisioning uses the signed cloud rootfs; ISO/UEFI and custom installer inputs are not supported yet".into());
    }
    if args.network != "usernet" {
        return Err("native ARM64 provisioning requires usernet".into());
    }
    let vcpus = args.vcpus.unwrap_or_else(control_api::host_default_vcpus);
    if vcpus == 0 || vcpus > machine_arm64::MAX_CPUS {
        return Err("ARM64 vCPU count must be 1..64".into());
    }
    let name = args
        .name
        .clone()
        .or_else(|| {
            args.disk
                .as_ref()
                .and_then(|path| path.file_stem())
                .map(|s| s.to_string_lossy().into_owned())
        })
        .unwrap_or_else(|| "ubuntu".into());
    if name.is_empty()
        || name.len() > 63
        || !name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'))
    {
        return Err("machine name must be 1..63 letters, digits, '-' or '_'".into());
    }
    let target = match &args.disk {
        Some(path) => path.clone(),
        None => disk_image::refs::manager_vm_dir()
            .ok_or("no VM directory; specify --disk")?
            .join(format!("{name}.raw")),
    };
    let target = if target.is_absolute() {
        target
    } else {
        std::env::current_dir()
            .map_err(|e| e.to_string())?
            .join(target)
    };
    let profile = target.with_extension("toml");
    let boot_directory = target.with_extension("boot");
    if [&target, &profile, &boot_directory]
        .into_iter()
        .any(|path| path.exists())
    {
        return Err("refusing to replace an existing disk, profile or boot directory".into());
    }
    let bytes = disk_image::parse_size(&args.size).map_err(|e| e.to_string())?;
    if bytes < 8 << 30 {
        return Err("an Ubuntu ARM64 desktop needs a disk of at least 8G".into());
    }
    let parent = target.parent().ok_or("disk has no parent directory")?;
    std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    let cache = crate::paths::cache_root()?
        .join("ubuntu-cloud")
        .join(debian_media::UBUNTU_CLOUD_VERSION);
    tracing::info!("verifying Ubuntu ARM64 base media");
    let image = debian_media::fetch_ubuntu_cloud_with(
        &debian_media::UreqTransport::new(),
        &cache,
        debian_media::FetchOptions::default(),
    )
    .map_err(|e| e.to_string())?;
    tracing::info!(path=%target.display(), "extracting Ubuntu ARM64 sparse disk");
    ubuntu_arm64_image::extract_rootfs(&image.path, &target, bytes)?;
    let mut owns_boot_directory = false;
    let result = (|| {
        let (kernel, initramfs) = ubuntu_arm64_image::extract_boot(&target, &boot_directory)?;
        owns_boot_directory = true;
        let epoch = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|e| e.to_string())?;
        let marker = format!(
            "ENTANGLED_UBUNTU_READY_{}_{}",
            std::process::id(),
            epoch.as_nanos()
        );
        let stage = std::env::temp_dir().join(format!(
            "entangled-ubuntu-{}-{}",
            std::process::id(),
            epoch.as_nanos()
        ));
        std::fs::create_dir(&stage).map_err(|e| e.to_string())?;
        let stage_result = (|| {
            std::fs::write(
                stage.join("provision-desktop.sh"),
                include_str!("../../../guest/ubuntu-arm64/provision-desktop.sh"),
            )
            .map_err(|e| e.to_string())?;
            std::fs::write(
                stage.join("configure-venus-desktop.sh"),
                include_str!("../../../guest/ubuntu-arm64/configure-venus-desktop.sh"),
            )
            .map_err(|e| e.to_string())?;
            let cfg = VmConfig {
                name: name.clone(), architecture: GuestArchitecture::Aarch64,
                memory_mib: args.memory_mib.max(4096), vcpus, transport: VirtioTransport::Mmio,
                boot: BootSection { mode: BootMode::DirectLinux, kernel: Some(kernel), initramfs: Some(initramfs),
                    cmdline: "console=tty0 console=ttyAMA0 earlycon=pl011,0x9000000 root=/dev/vda rw rootwait net.ifnames=0".into(),
                    ..Default::default() },
                disks: vec![DiskSection { path: target.clone(), writable: true }], cdrom: None, optical: vec![],
                network: Some(NetworkSection { backend: NetworkBackend::Usernet,
                    mac: Some(control_api::new_machine_mac(&name, &target)), interface: None, forward: vec![] }),
                display: DisplaySection { venus: true, width: 1024, height: 768,
                    refresh_hz: args.refresh_hz.unwrap_or(60), ..Default::default() },
                sound: Default::default(), gamepad: Default::default(), shares: vec![], reclaim_memory: false, ignore_msrs: false, hyperv_enlightenments: false, guest_agent: false,
            };
            VmConfig::from_toml(&toml::to_string_pretty(&cfg).map_err(|e| e.to_string())?)
                .map_err(|e| e.to_string())?;
            let net = virtio_net::UserNetConfig::for_host();
            let hostname = name.replace('_', "-").to_ascii_lowercase();
            let mut installer = cfg.clone();
            installer.shares.push(ShareSection {
                tag: "entangled".into(),
                path: stage.clone(),
                read_only: true,
            });
            installer.boot.cmdline.push_str(&format!(
                " init=/bin/sh -- -c \"mkdir -p /run/entangled; modprobe 9pnet_virtio; mount -t 9p -o trans=virtio,version=9p2000.L,ro entangled /run/entangled; /bin/sh /run/entangled/provision-desktop.sh {} {} {} {} {}; /sbin/poweroff -f\"",
                epoch.as_secs(), marker, hostname, net.guest, net.gateway));
            tracing::info!(
                "provisioning Ubuntu GNOME with Zink/Venus; progress follows on the guest console"
            );
            run_vm::run_with_success_marker(
                installer,
                run_vm::RunOptions {
                    headless: true,
                    ..Default::default()
                },
                marker,
            )?;
            let text = toml::to_string_pretty(&cfg).map_err(|e| e.to_string())?;
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&profile)
                .map_err(|e| e.to_string())?;
            let written = file
                .write_all(text.as_bytes())
                .and_then(|()| file.sync_all());
            drop(file);
            if let Err(error) = written {
                let _ = std::fs::remove_file(&profile);
                return Err(error.to_string());
            }
            println!("Ubuntu ARM64 desktop ready: {}", profile.display());
            println!(
                "Guest login: entangled / entangled. Change the password in Ubuntu after login."
            );
            Ok(())
        })();
        let _ = std::fs::remove_dir_all(&stage);
        stage_result
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&target);
        if owns_boot_directory {
            let _ = std::fs::remove_dir_all(&boot_directory);
        }
    }
    result
}
