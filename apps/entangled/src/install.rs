//! `entangled install` — the installer mode (backlog EPIC 10).
//!
//! Boots the verified Debian installer against a target RAW disk, with the
//! full device set (window, GPU, input, net). In `--auto` mode a preseed
//! file is appended to the installer initrd (gzip-concatenated cpio, which
//! the kernel treats as one initramfs) and the network is configured
//! statically on the kernel command line — the host TAP has no DHCP server.
//! After the installer's final reboot the disk is inspected (MBR + ext4
//! UUID) and a ready-to-run VM profile is written next to it (MVP-1008/1009).

use std::io::Write as _;
use std::path::{Path, PathBuf};

use control_api::{
    BootMode, BootSection, DiskSection, DisplaySection, NetworkBackend, NetworkSection, VmConfig,
};
use debian_media::{FetchOptions, FetchReport, MediaKind};
use flate2::write::GzEncoder;
use flate2::Compression;

use crate::diskfs;
use crate::disk;
use crate::InstallArgs;

/// Static guest address matching scripts/setup-tap.sh's 172.30.0.1/24 host side.
const GUEST_IP: &str = "172.30.0.2";
const GUEST_GATEWAY: &str = "172.30.0.1";
const GUEST_NETMASK: &str = "255.255.255.0";
const GUEST_DNS: &str = "1.1.1.1";

/// The maintained automated profile (EPIC 13). Compiled in so `--auto` works
/// from any working directory.
const AUTO_PRESEED: &str = include_str!("../../../assets/preseed/auto-weston.cfg");

pub fn run(args: &InstallArgs) -> Result<(), String> {
    if !args.distro.eq_ignore_ascii_case("debian") {
        return Err(format!("unknown distro '{}': only debian", args.distro));
    }

    // 1. Verified installer media (cache-first; EPIC 6 owns the trust chain).
    let report = debian_media::fetch_debian(
        &args.distro,
        "stable",
        "amd64",
        &args.variant,
        FetchOptions::default(),
    )
    .map_err(|e| format!("cannot obtain installer media: {e}"))?;
    let kernel = artifact_path(&report, MediaKind::Kernel)?;
    let initrd = artifact_path(&report, MediaKind::Initrd)?;
    tracing::info!(version = %report.version, variant = %args.variant, "installer media ready");

    // 2. Target disk (MVP-1001/1006).
    if !args.disk.exists() {
        let bytes = disk::parse_size(&args.size).map_err(|e| e.to_string())?;
        disk::create_raw(&args.disk, bytes).map_err(|e| e.to_string())?;
        tracing::info!(disk = %args.disk.display(), bytes, "created target disk");
    }

    // 3. Preseed (MVP-1010) and command line (MVP-1003).
    let vm_name = args
        .name
        .clone()
        .unwrap_or_else(|| stem_of(&args.disk, "debian"));
    // A preseed is ALWAYS appended to the initrd: at minimum it carries the
    // virtio_mmio modprobe (see EARLY_MODPROBE_PRESEED — the d-i kernel
    // command line parser cannot carry values with spaces, so early_command
    // must ride in a preseed file). Interactive installs get only that line.
    let (preseed, automated) = match (&args.preseed, args.auto) {
        (Some(path), _) => {
            let mut content = std::fs::read(path)
                .map_err(|e| format!("cannot read preseed {}: {e}", path.display()))?;
            if !content
                .windows(b"preseed/early_command".len())
                .any(|w| w == b"preseed/early_command")
            {
                content.extend_from_slice(EARLY_MODPROBE_PRESEED.as_bytes());
            }
            (content, true)
        }
        (None, true) => (AUTO_PRESEED.as_bytes().to_vec(), true),
        (None, false) => (EARLY_MODPROBE_PRESEED.as_bytes().to_vec(), false),
    };
    let initramfs = preseeded_initrd(&initrd, &preseed, &args.disk)?;
    let cmdline = if automated {
        auto_cmdline(&vm_name)
    } else {
        "console=ttyS0 panic=1 reboot=k".to_string()
    };

    // 4. The installer VM: target disk as /dev/vda, network, window unless
    //    --headless (MVP-1004/1005/1006).
    let cfg = VmConfig {
        name: format!("{vm_name}-install"),
        memory_mib: args.memory_mib,
        vcpus: 2,
        boot: BootSection {
            mode: BootMode::DirectLinux,
            kernel,
            initramfs: Some(initramfs.clone()),
            cmdline,
        },
        disks: vec![DiskSection {
            path: args.disk.clone(),
            writable: true,
        }],
        network: Some(NetworkSection {
            backend: NetworkBackend::Tap,
            interface: args.interface.clone(),
            mac: None,
        }),
        display: DisplaySection::default(),
    };

    tracing::info!(
        vm = %cfg.name,
        auto = automated,
        "starting the installer; it reboots when done (Ctrl+Alt+Q / Ctrl+C aborts)"
    );
    let run_result = crate::run_vm::run(cfg, args.headless);
    // Keep the derived initrd for debugging on failure; remove it on success.
    match &run_result {
        Ok(()) => {
            if initramfs != initrd {
                let _ = std::fs::remove_file(&initramfs);
            }
        }
        Err(e) => return Err(format!("installer VM failed: {e}")),
    }

    // 5. Did an installation actually happen? (MVP-1008)
    let root = diskfs::find_installed_root(&args.disk).map_err(|e| {
        format!(
            "the installer exited but {} does not look installed: {e}",
            args.disk.display()
        )
    })?;
    tracing::info!(partition = root.partition, uuid = %root.uuid, "installation detected");

    // 6. Write the runnable profile (MVP-1009).
    let profile = VmConfig {
        name: vm_name.clone(),
        memory_mib: 2048,
        vcpus: 2,
        boot: BootSection {
            mode: BootMode::DirectLinux,
            kernel: PathBuf::from("artifacts/bootstrap/vmlinuz"),
            initramfs: Some(PathBuf::from("artifacts/bootstrap/initrd.img")),
            cmdline: format!("console=ttyS0 root=UUID={} rw", root.uuid),
        },
        disks: vec![DiskSection {
            path: args.disk.clone(),
            writable: true,
        }],
        network: Some(NetworkSection {
            backend: NetworkBackend::Tap,
            interface: args.interface.clone(),
            mac: None,
        }),
        display: DisplaySection::default(),
    };
    let profile_path = args
        .disk
        .with_file_name(format!("{vm_name}.toml"));
    let text = toml::to_string_pretty(&profile).map_err(|e| e.to_string())?;
    std::fs::write(&profile_path, text)
        .map_err(|e| format!("cannot write {}: {e}", profile_path.display()))?;

    println!(
        "installed: /dev/vda{} (ext4 UUID {})\nprofile:   {}\nrun it:    entangled run {}",
        root.partition,
        root.uuid,
        profile_path.display(),
        profile_path.display()
    );
    Ok(())
}

fn artifact_path(report: &FetchReport, kind: MediaKind) -> Result<PathBuf, String> {
    report
        .artifacts
        .iter()
        .find(|a| a.kind == kind)
        .map(|a| a.path.clone())
        .ok_or_else(|| format!("fetch report is missing the {kind:?} artifact"))
}

fn stem_of(disk: &Path, fallback: &str) -> String {
    disk.file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| fallback.to_string())
}

/// Debian ships virtio_mmio as a module, and nothing autoloads it on x86 —
/// there is no discoverable bus, devices are announced on the command line.
/// Without this, d-i's hardware detection finds no NIC and no disk. It must
/// live in a preseed file: d-i's /proc/cmdline parser cannot carry values
/// with spaces, quoted or not. Appended to custom preseeds that lack their
/// own early_command (a custom early_command must include the modprobe).
const EARLY_MODPROBE_PRESEED: &str =
    "\n# added by entangled install: virtio_mmio never autoloads on x86\n\
     d-i preseed/early_command string modprobe virtio_mmio\n";

/// d-i automation command line: priority critical + static netcfg (the host
/// TAP has no DHCP), with initrd preseeding picking up /preseed.cfg.
fn auto_cmdline(hostname: &str) -> String {
    format!(
        "console=ttyS0 panic=1 reboot=k auto=true priority=critical \
         netcfg/disable_autoconfig=true netcfg/get_ipaddress={GUEST_IP} \
         netcfg/get_netmask={GUEST_NETMASK} netcfg/get_gateway={GUEST_GATEWAY} \
         netcfg/get_nameservers={GUEST_DNS} netcfg/confirm_static=true \
         netcfg/get_hostname={hostname} netcfg/get_domain=local"
    )
}

/// Appends `/preseed.cfg` to the installer initrd as a gzip-compressed cpio
/// archive; the kernel concatenates initramfs segments, and d-i loads
/// /preseed.cfg automatically (initrd preseeding).
fn preseeded_initrd(
    base_initrd: &Path,
    preseed: &[u8],
    disk: &Path,
) -> Result<PathBuf, String> {
    let mut image = std::fs::read(base_initrd)
        .map_err(|e| format!("cannot read initrd {}: {e}", base_initrd.display()))?;

    let mut archive = Vec::new();
    write_newc_entry(&mut archive, "preseed.cfg", preseed, 0o100_644);
    write_newc_trailer(&mut archive);

    let mut encoder = GzEncoder::new(Vec::new(), Compression::fast());
    encoder
        .write_all(&archive)
        .and_then(|_| encoder.finish())
        .map(|gz| image.extend_from_slice(&gz))
        .map_err(|e| format!("cannot compress preseed archive: {e}"))?;

    let out = disk.with_file_name(format!(
        "{}.install-initrd.img",
        stem_of(disk, "install")
    ));
    std::fs::write(&out, image).map_err(|e| format!("cannot write {}: {e}", out.display()))?;
    Ok(out)
}

/// Writes one `newc` (SVR4 without CRC) cpio entry.
fn write_newc_entry(out: &mut Vec<u8>, name: &str, data: &[u8], mode: u32) {
    let name_z = format!("{name}\0");
    out.extend_from_slice(b"070701");
    // ino, mode, uid, gid, nlink, mtime, filesize,
    // devmajor, devminor, rdevmajor, rdevminor, namesize, check
    for value in [
        1,
        mode,
        0,
        0,
        1,
        0,
        data.len() as u32,
        0,
        0,
        0,
        0,
        name_z.len() as u32,
        0,
    ] {
        out.extend_from_slice(format!("{value:08X}").as_bytes());
    }
    out.extend_from_slice(name_z.as_bytes());
    pad4(out);
    out.extend_from_slice(data);
    pad4(out);
}

fn write_newc_trailer(out: &mut Vec<u8>) {
    write_newc_entry(out, "TRAILER!!!", &[], 0);
}

fn pad4(out: &mut Vec<u8>) {
    while out.len() % 4 != 0 {
        out.push(0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use flate2::read::GzDecoder;
    use std::io::Read;

    #[test]
    fn newc_archive_round_trips() {
        let mut archive = Vec::new();
        write_newc_entry(&mut archive, "preseed.cfg", b"d-i test", 0o100_644);
        write_newc_trailer(&mut archive);

        // Header magic + hex fields are ASCII; verify the essentials by hand.
        assert!(archive.starts_with(b"070701"));
        let text = String::from_utf8_lossy(&archive);
        assert!(text.contains("preseed.cfg"));
        assert!(text.contains("TRAILER!!!"));
        // filesize field (7th) of the first entry says 8.
        let filesize = &archive[6 + 6 * 8..6 + 7 * 8];
        assert_eq!(filesize, b"00000008");
        // Alignment: every entry starts 4-byte aligned.
        assert_eq!(archive.len() % 4, 0);
    }

    #[test]
    fn preseeded_initrd_appends_a_valid_gzip_member() {
        let dir = std::env::temp_dir().join("entangled-install-tests");
        std::fs::create_dir_all(&dir).unwrap();
        let base = dir.join(format!("base-{}.img", std::process::id()));
        let disk = dir.join(format!("target-{}.raw", std::process::id()));
        std::fs::write(&base, b"FAKE-INITRD").unwrap();
        std::fs::write(&disk, b"").unwrap();

        let out = preseeded_initrd(&base, b"d-i preseed", &disk).unwrap();
        let bytes = std::fs::read(&out).unwrap();
        assert!(bytes.starts_with(b"FAKE-INITRD"));

        // The appended member decompresses to a cpio holding preseed.cfg.
        let mut decoder = GzDecoder::new(&bytes[b"FAKE-INITRD".len()..]);
        let mut archive = Vec::new();
        decoder.read_to_end(&mut archive).unwrap();
        let text = String::from_utf8_lossy(&archive);
        assert!(text.contains("preseed.cfg"));
        assert!(text.contains("d-i preseed"));

        for p in [&out, &base, &disk] {
            let _ = std::fs::remove_file(p);
        }
    }

    #[test]
    fn auto_cmdline_preseeds_static_network() {
        let c = auto_cmdline("testvm");
        for needle in [
            "auto=true",
            "priority=critical",
            "netcfg/disable_autoconfig=true",
            "netcfg/get_ipaddress=172.30.0.2",
            "netcfg/get_gateway=172.30.0.1",
            "console=ttyS0",
        ] {
            assert!(c.contains(needle), "missing {needle} in: {c}");
        }
    }
}
