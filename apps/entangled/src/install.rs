//! `entangled install` — the installer mode (backlog EPIC 10, UEFI-1804).
//!
//! # Two hosts, one installer (EPIC 17 phase 5)
//!
//! Same split as `run_vm`: one shared body, and per-host choices named in one
//! place instead of scattered through it. The body is the interesting half and
//! it is *entirely* portable — media verification, the preseed cpio, the
//! autoinstall seed, the GRUB typing, disk inspection, profile writing — because
//! all of it is logic over files. What differs is one thing:
//!
//! | | Linux (KVM) | Windows (WHP) |
//! |---|---|---|
//! | Default network | `tap` (`scripts/setup-tap.sh`), static netcfg | `usernet` (user-mode NAT in-process), static netcfg from its own config |
//! | `--network tap` | the host interface | refused: [`NETWORK_TAP_UNAVAILABLE`] |
//! | Bootstrap kernel (Debian) | `guest/bootstrap-kernel/build.sh` | copied in from a Linux checkout — there is no cross build |
//! | Ubuntu | offline install off the verified ISO — identical on both | ditto |
//!
//! Nothing else is host-specific, and in particular nothing here mounts
//! anything: the installer writes the disk from *inside* the guest, and the host
//! only ever reads partition tables (`crates/disk-image`, portable).
//!
//! # Two distributions, two entirely different mechanisms, one command:
//!
//! * **Debian** boots the verified d-i installer directly (`mode =
//!   "direct-linux"`) against a target RAW disk with the full device set. In
//!   `--auto` mode a preseed file is appended to the installer initrd
//!   (gzip-concatenated cpio, which the kernel treats as one initramfs) and the
//!   network is configured statically on the kernel command line — the host TAP
//!   has no DHCP server. Afterwards the disk is inspected (MBR + ext4 UUID) and
//!   a ready-to-run profile is written next to it (MVP-1008/1009).
//! * **Fedora** boots the verified Everything netinst ISO through UEFI firmware,
//!   automated with a **kickstart** on an ISO9660 volume labelled `OEMDRV` —
//!   Anaconda's own convention — and named explicitly as
//!   `inst.ks=hd:LABEL=OEMDRV:/ks.cfg` on a command line typed into GRUB, so a
//!   kickstart that fails to arrive stalls loudly instead of falling through to
//!   an interactive installer. Unlike Ubuntu's it is an *online* install: every
//!   package comes over the network. See [`crate::install_fedora`].
//! * **Ubuntu** boots the verified live-server ISO through UEFI firmware
//!   (`mode = "uefi"`, virtio-pci), which is the only way to end up with a
//!   GPT + ESP the firmware can boot afterwards. subiquity is automated with an
//!   autoinstall configuration on a cloud-init NoCloud seed volume, and the one
//!   thing a seed cannot carry — the word `autoinstall` on the kernel command
//!   line, without which the installer stops for a confirmation — is typed into
//!   GRUB over the serial console. The install ends with an ACPI poweroff, the
//!   disk is inspected (GPT + ESP + root) and the profile that boots the
//!   *installed* system through its persisted NVRAM boot entry is written.

use std::io::Write as _;
use std::path::{Path, PathBuf};

use control_api::{
    BootMode, BootSection, DiskSection, DisplaySection, NetworkBackend, NetworkSection,
    SoundBackend, SoundSection, VirtioTransport, VmConfig,
};
use debian_media::{FetchOptions, FetchReport, MediaKind};
use flate2::write::GzEncoder;
use flate2::Compression;

use crate::disk;
use crate::paths::stem_of;
use crate::InstallArgs;
use disk_image as diskfs;

/// Static guest address matching scripts/setup-tap.sh's defaults
/// (host side 192.168.73.1/24).
const TAP_GUEST_IP: &str = "192.168.73.2";
const TAP_GUEST_GATEWAY: &str = "192.168.73.1";
const TAP_GUEST_NETMASK: &str = "255.255.255.0";
/// A public resolver, because a TAP segment has nothing on it that resolves
/// names. (The usernet segment does: its gateway relays DNS.)
const TAP_GUEST_DNS: &str = "1.1.1.1";

/// The maintained automated profile (EPIC 13). Compiled in so `--auto` works
/// from any working directory.
const AUTO_PRESEED: &str = include_str!("../../../assets/preseed/auto-weston.cfg");

/// The project kernel that boots both installer and installed system.
const BOOTSTRAP_KERNEL: &str = "artifacts/bootstrap/vmlinuz";

// ---------------------------------------------------------------------------
// The one per-host choice: which network the installer gets
// ---------------------------------------------------------------------------

/// What `--network tap` gets told on a host that has no TAP. The same fact
/// `run_vm` reports for a profile, worded for the flag that asked for it.
pub const NETWORK_TAP_UNAVAILABLE: &str =
    "--network tap is Linux-only: Windows has no TAP device, and the drivers that \
     would provide one are GPL (see ADR-0002). Use --network usernet (user-mode NAT \
     inside the entangled process: DHCP, DNS relay and outbound TCP, no \
     administrator), or --network none for an offline install";

/// The installer VM's network, and the numbers d-i must be preseeded with for
/// it. One value, so a command line and a `[network]` section cannot disagree
/// about which segment the guest is on — the failure that produces is a d-i that
/// downloads nothing and says only "the network is not configured".
#[derive(Debug)]
pub struct NetPlan {
    pub section: Option<NetworkSection>,
    /// `None` for an offline install: netcfg is then told to configure nothing.
    pub address: Option<NetAddress>,
}

/// A static guest address, in netcfg's terms.
#[derive(Debug)]
pub struct NetAddress {
    pub ip: String,
    pub gateway: String,
    pub netmask: String,
    pub dns: String,
}

/// Resolves `--network <choice>` for this host.
///
/// Static addressing for both backends, rather than DHCP for the one that has a
/// DHCP server: it is the same shape on both hosts, it removes a boot-time
/// negotiation from the middle of an unattended install, and for usernet the
/// numbers come from [`virtio_net::UserNetConfig`] itself, so they cannot drift
/// away from what the NAT would hand out.
pub fn net_plan(choice: &str, interface: &str) -> Result<NetPlan, String> {
    match choice {
        "tap" => {
            if !cfg!(target_os = "linux") {
                return Err(NETWORK_TAP_UNAVAILABLE.to_string());
            }
            Ok(NetPlan {
                section: Some(NetworkSection {
                    backend: NetworkBackend::Tap,
                    interface: Some(interface.to_string()),
                    mac: None,
                }),
                address: Some(NetAddress {
                    ip: TAP_GUEST_IP.to_string(),
                    gateway: TAP_GUEST_GATEWAY.to_string(),
                    netmask: TAP_GUEST_NETMASK.to_string(),
                    dns: TAP_GUEST_DNS.to_string(),
                }),
            })
        }
        "usernet" => {
            let net = virtio_net::UserNetConfig::default();
            Ok(NetPlan {
                section: Some(NetworkSection {
                    backend: NetworkBackend::Usernet,
                    // Refused by config validation for this backend: the segment
                    // is inside this process and touches no host interface.
                    interface: None,
                    mac: None,
                }),
                address: Some(NetAddress {
                    ip: net.guest.to_string(),
                    gateway: net.gateway.to_string(),
                    netmask: net.netmask.to_string(),
                    // The gateway *is* the resolver: usernet relays DNS from the
                    // host. Naming a public resolver instead would work too, but
                    // only while the host can reach it directly.
                    dns: net.gateway.to_string(),
                }),
            })
        }
        "none" => Ok(NetPlan {
            section: None,
            address: None,
        }),
        other => Err(format!(
            "unknown --network '{other}': expected tap, usernet or none"
        )),
    }
}

// ---------------------------------------------------------------------------
// Where the machine goes
// ---------------------------------------------------------------------------

/// The VM's name: `--name`, else the disk file's stem, else the distribution.
///
/// In that order because the name is what every derived file is called, and a
/// person who passed `--disk "D:\vms\work laptop.raw"` means that machine to be
/// called `work laptop` — not `ubuntu`.
pub fn vm_name(args: &InstallArgs) -> String {
    if let Some(name) = args.name.clone().filter(|n| !n.trim().is_empty()) {
        return name;
    }
    match &args.disk {
        Some(disk) => stem_of(disk, &args.distro.to_lowercase()),
        None => args.distro.to_lowercase(),
    }
}

/// The target disk: `--disk` when given, otherwise `<vm dir>/<name>.raw`.
///
/// The VM directory is the manager's (`disk_image::refs::manager_vm_dir` reads
/// its `manager.toml`, defaulting to `~/entangled-vms` —
/// `%USERPROFILE%\entangled-vms` on Windows), so a machine installed from the
/// command line appears in the manager's list and a machine created there is
/// found by `entangled disk rm`. Two defaults would mean two halves of one VM
/// collection.
pub fn target_disk(args: &InstallArgs) -> Result<PathBuf, String> {
    if let Some(disk) = &args.disk {
        return Ok(disk.clone());
    }
    let dir = diskfs::refs::manager_vm_dir().ok_or(
        "no --disk given and no home directory to default into: pass --disk with an \
         explicit path (on Windows, USERPROFILE is what names the default \
         %USERPROFILE%\\entangled-vms)",
    )?;
    std::fs::create_dir_all(&dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    Ok(dir.join(format!("{}.raw", vm_name(args))))
}

pub fn run(args: &InstallArgs) -> Result<(), String> {
    if args.distro.eq_ignore_ascii_case("ubuntu") {
        return crate::install_ubuntu::run(args);
    }
    if args.distro.eq_ignore_ascii_case("fedora") {
        return crate::install_fedora::run(args);
    }
    if !args.distro.eq_ignore_ascii_case("debian") {
        return Err(format!(
            "unknown distro '{}': expected debian, ubuntu or fedora",
            args.distro
        ));
    }

    // 0. The network, first, because a d-i install without one cannot happen at
    //    all: the netboot installer carries no packages. Resolved before
    //    anything is downloaded or created so `--network tap` on Windows costs
    //    nothing but the message.
    let net = net_plan(&args.network, &args.interface)?;
    let address = net.address.as_ref().ok_or(
        "the Debian netboot installer downloads the whole system, so it cannot run with \
         --network none. Use --network usernet (no host setup) or --network tap",
    )?;

    // 1. Verified installer media (cache-first; EPIC 6 owns the trust chain).
    let report = debian_media::fetch_debian(
        &args.distro,
        "stable",
        "amd64",
        &args.variant,
        FetchOptions::default(),
    )
    .map_err(|e| format!("cannot obtain installer media: {e}"))?;
    let initrd = artifact_path(&report, MediaKind::Initrd)?;
    // The installer RUNS ON THE BOOTSTRAP KERNEL, not the fetched d-i kernel:
    // Debian builds virtio_mmio without cmdline-device support, so their
    // kernel cannot see our devices. The fetched kernel stays verified in the
    // cache (useful for ISO flows post-MVP).
    let kernel = PathBuf::from(BOOTSTRAP_KERNEL);
    if !kernel.exists() {
        return Err(format!(
            "bootstrap kernel {BOOTSTRAP_KERNEL} not found — build it with \
             `bash guest/bootstrap-kernel/build.sh` (the Debian installer kernel \
             cannot drive virtio-mmio devices).{}",
            if cfg!(target_os = "linux") {
                ""
            } else {
                " That script is a Linux kernel build and does not cross-build: copy \
                 artifacts/bootstrap/ in from a Linux checkout, or install Ubuntu \
                 instead — `entangled install ubuntu` boots verified media through \
                 UEFI and needs no project kernel at all"
            }
        ));
    }
    tracing::info!(version = %report.version, variant = %args.variant, "installer media ready");

    // 2. Target disk (MVP-1001/1006).
    let vm_name = vm_name(args);
    let target = target_disk(args)?;
    if !target.exists() {
        let bytes = disk::parse_size(&args.size).map_err(|e| e.to_string())?;
        disk::create_raw(&target, bytes).map_err(|e| e.to_string())?;
        tracing::info!(disk = %target.display(), bytes, "created target disk");
    }

    // 3. Preseed (MVP-1010) and command line (MVP-1003).
    // A preseed is ALWAYS appended to the initrd: at minimum it carries the
    // virtio_mmio modprobe (see EARLY_MODPROBE_PRESEED — the d-i kernel
    // command line parser cannot carry values with spaces, so early_command
    // must ride in a preseed file). Interactive installs get only that line.
    let (preseed, automated) = match (&args.preseed, args.auto) {
        (Some(path), _) => {
            let content = std::fs::read(path)
                .map_err(|e| format!("cannot read preseed {}: {e}", path.display()))?;
            (with_required_keys(content), true)
        }
        (None, true) => (with_required_keys(AUTO_PRESEED.as_bytes().to_vec()), true),
        (None, false) => (with_required_keys(Vec::new()), false),
    };
    let initramfs = preseeded_initrd(&initrd, &preseed, &target)?;
    let cmdline = if automated {
        auto_cmdline(&vm_name, address)
    } else {
        "console=ttyS0 panic=1 reboot=k".to_string()
    };

    // 4. The installer VM: target disk as /dev/vda, network, window unless
    //    --headless (MVP-1004/1005/1006).
    let cfg = VmConfig {
        name: format!("{vm_name}-install"),
        memory_mib: args.memory_mib,
        vcpus: 2,
        // The installer boots the same direct-Linux path the MVP has always
        // used, so it keeps the transport that path was built around.
        transport: VirtioTransport::default(),
        boot: BootSection {
            mode: BootMode::DirectLinux,
            kernel: Some(kernel),
            initramfs: Some(initramfs.clone()),
            firmware: None,
            nvram: None,
            cmdline,
        },
        disks: vec![DiskSection {
            path: target.clone(),
            writable: true,
        }],
        cdrom: None,
        network: net.section.clone(),
        display: DisplaySection::default(),
        // The installer has nothing to say; the *installed* profile below is
        // where the card belongs.
        sound: SoundSection::default(),
    };

    tracing::info!(
        vm = %cfg.name,
        auto = automated,
        "starting the installer; it powers off when done (Ctrl+Alt+Q / Ctrl+C aborts)"
    );
    let run_result = crate::run_vm::run(
        cfg,
        crate::run_vm::RunOptions {
            headless: args.headless,
            ..Default::default()
        },
    );
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
    let root = diskfs::find_installed_root(&target).map_err(|e| {
        format!(
            "the installer exited but {} does not look installed: {e}",
            target.display()
        )
    })?;
    tracing::info!(partition = root.partition, uuid = %root.uuid, "installation detected");

    // 6. Write the runnable profile (MVP-1009).
    let profile = VmConfig {
        name: vm_name.clone(),
        memory_mib: 2048,
        vcpus: 2,
        transport: VirtioTransport::default(),
        boot: BootSection {
            mode: BootMode::DirectLinux,
            kernel: Some(PathBuf::from("artifacts/bootstrap/vmlinuz")),
            initramfs: Some(PathBuf::from("artifacts/bootstrap/initrd.img")),
            firmware: None,
            nvram: None,
            cmdline: format!("console=ttyS0 root=UUID={} rw", root.uuid),
        },
        disks: vec![DiskSection {
            path: target.clone(),
            writable: true,
        }],
        cdrom: None,
        network: net.section.clone(),
        display: DisplaySection::default(),
        // A desktop with no sound is not a desktop (GAME-2102). `auto` never
        // fails a run: a host with no audio device gets a card that plays into
        // silence, and the guest still enumerates one.
        sound: SoundSection {
            enabled: true,
            backend: SoundBackend::Auto,
        },
    };
    let profile_path = target.with_file_name(format!("{vm_name}.toml"));
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

/// Compatibility preseed for running d-i on the bootstrap kernel: Debian's
/// own installer kernel builds virtio_mmio without cmdline-device support
/// (verified: `modinfo -p` on the d-i module lists no parameters), so the
/// installer must run on our kernel with virtio built in. The d-i initrd
/// then carries no modules for the running kernel version, and the target
/// kernel cannot be derived from `uname -r` — both preseeded away here.
/// Appended to every initrd (interactive installs included) and to custom
/// preseeds that do not set the keys themselves.
const KERNEL_COMPAT_PRESEED: &str =
    "\n# added by entangled install: d-i runs on the Entangled bootstrap kernel\n\
     d-i anna/no_kernel_modules boolean true\n\
     d-i base-installer/kernel/image string linux-image-amd64\n";

/// The other key every install needs, whoever wrote the preseed: **end by
/// powering off, not by rebooting**.
///
/// `entangled install` learns that an installation finished from the guest
/// stopping, and only one of the two endings means that on both hosts. d-i's
/// default reboot ends in the kernel's restart chain — with `reboot=k` a triple
/// fault — which KVM reports as a shutdown exit but which WHP's local APIC
/// emulation *absorbs*: the vCPU parks inside `WHvRunVirtualProcessor` and the
/// installer VM hangs forever after a perfectly good install. An ACPI S5 write
/// is latched by `machine_x86::acpi::pm` and reported as a clean stop by both
/// backends, which is the same contract the Ubuntu path already relies on
/// (`shutdown: poweroff` in the autoinstall profile).
const POWEROFF_PRESEED: &str =
    "\n# added by entangled install: stop the VM the way both hosts can observe\n\
     d-i debian-installer/exit/poweroff boolean true\n";

/// Appends the keys an Entangled install cannot do without to a preseed —
/// whichever of them the author did not already set. Never overrides: a preseed
/// that names a key means it, and the check is on the key's *name*, so a
/// deliberate `false` is left alone.
fn with_required_keys(mut preseed: Vec<u8>) -> Vec<u8> {
    for (key, block) in [
        ("anna/no_kernel_modules", KERNEL_COMPAT_PRESEED),
        ("debian-installer/exit/poweroff", POWEROFF_PRESEED),
    ] {
        if !contains(&preseed, key.as_bytes()) {
            preseed.extend_from_slice(block.as_bytes());
        }
    }
    preseed
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|w| w == needle)
}

/// d-i automation command line: priority critical + static netcfg, with initrd
/// preseeding picking up /preseed.cfg.
///
/// Static on both hosts, for two different reasons that happen to agree: a host
/// TAP has no DHCP server at all, and while usernet does have one, an unattended
/// install is better off not negotiating for the address this host already knows
/// it would be given (see [`net_plan`]).
fn auto_cmdline(hostname: &str, address: &NetAddress) -> String {
    let NetAddress {
        ip,
        gateway,
        netmask,
        dns,
    } = address;
    format!(
        "console=ttyS0 panic=1 reboot=k auto=true priority=critical \
         netcfg/disable_autoconfig=true netcfg/get_ipaddress={ip} \
         netcfg/get_netmask={netmask} netcfg/get_gateway={gateway} \
         netcfg/get_nameservers={dns} netcfg/confirm_static=true \
         netcfg/get_hostname={hostname} netcfg/get_domain=local"
    )
}

/// Appends `/preseed.cfg` to the installer initrd as a gzip-compressed cpio
/// archive; the kernel concatenates initramfs segments, and d-i loads
/// /preseed.cfg automatically (initrd preseeding).
fn preseeded_initrd(base_initrd: &Path, preseed: &[u8], disk: &Path) -> Result<PathBuf, String> {
    let mut image = std::fs::read(base_initrd)
        .map_err(|e| format!("cannot read initrd {}: {e}", base_initrd.display()))?;

    let mut archive = Vec::new();
    write_newc_entry(&mut archive, "preseed.cfg", preseed, 0o100_644);
    // Payload files for late_command: shipped as-is so no shell escaping ever
    // crosses the debconf boundary (see assets/preseed/auto-weston.cfg).
    write_newc_dir(&mut archive, "entangled");
    write_newc_entry(
        &mut archive,
        "entangled/autologin.conf",
        include_bytes!("../../../assets/preseed/autologin.conf"),
        0o100_644,
    );
    write_newc_entry(
        &mut archive,
        "entangled/weston-profile.sh",
        include_bytes!("../../../assets/preseed/weston-profile.sh"),
        0o100_644,
    );
    write_newc_trailer(&mut archive);

    let mut encoder = GzEncoder::new(Vec::new(), Compression::fast());
    let compressed = encoder
        .write_all(&archive)
        .and_then(|()| encoder.finish())
        .map_err(|e| format!("cannot compress preseed archive: {e}"))?;
    image.extend_from_slice(&compressed);

    let out = disk.with_file_name(format!("{}.install-initrd.img", stem_of(disk, "install")));
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

fn write_newc_dir(out: &mut Vec<u8>, name: &str) {
    write_newc_entry(out, name, &[], 0o040_755);
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

    /// Both keys an Entangled install cannot do without, added to whatever the
    /// author wrote — and *not* added twice, and not overriding a deliberate
    /// setting. The poweroff key is the one that makes the Windows host able to
    /// tell a finished install from a hung one at all.
    #[test]
    fn required_preseed_keys_are_added_once_and_never_override() {
        let text = |bytes: Vec<u8>| String::from_utf8(bytes).expect("ascii preseed");

        // An empty preseed (an interactive install) gets both blocks.
        let bare = text(with_required_keys(Vec::new()));
        assert!(
            bare.contains("d-i anna/no_kernel_modules boolean true"),
            "{bare}"
        );
        assert!(
            bare.contains("d-i debian-installer/exit/poweroff boolean true"),
            "{bare}"
        );

        // The compiled-in automated profile already sets both, so nothing is
        // appended to it. This is also the assertion that keeps the asset and
        // this module from disagreeing about the ending.
        let auto = text(with_required_keys(AUTO_PRESEED.as_bytes().to_vec()));
        assert_eq!(
            auto, AUTO_PRESEED,
            "the built-in profile needs no additions"
        );
        assert_eq!(
            auto.matches("debian-installer/exit/poweroff").count(),
            1,
            "the poweroff key must appear exactly once"
        );

        // A custom preseed that sets one key keeps its own and gains the other.
        let custom = text(with_required_keys(
            b"d-i debian-installer/exit/poweroff boolean false
"
            .to_vec(),
        ));
        assert!(custom.contains("exit/poweroff boolean false"), "{custom}");
        assert_eq!(custom.matches("exit/poweroff").count(), 1, "{custom}");
        assert!(custom.contains("anna/no_kernel_modules"), "{custom}");
    }

    #[test]
    fn auto_cmdline_preseeds_static_network() {
        let plan = net_plan("tap", "entangled0");
        let c = if cfg!(target_os = "linux") {
            auto_cmdline("testvm", plan.unwrap().address.as_ref().unwrap())
        } else {
            // TAP is refused here, so this host asserts on the numbers through
            // the same builder with the TAP address spelled out.
            assert!(plan.is_err());
            auto_cmdline(
                "testvm",
                &NetAddress {
                    ip: TAP_GUEST_IP.into(),
                    gateway: TAP_GUEST_GATEWAY.into(),
                    netmask: TAP_GUEST_NETMASK.into(),
                    dns: TAP_GUEST_DNS.into(),
                },
            )
        };
        for needle in [
            "auto=true",
            "priority=critical",
            "netcfg/disable_autoconfig=true",
            "netcfg/get_ipaddress=192.168.73.2",
            "netcfg/get_gateway=192.168.73.1",
            "netcfg/get_nameservers=1.1.1.1",
            "console=ttyS0",
        ] {
            assert!(c.contains(needle), "missing {needle} in: {c}");
        }
    }

    /// The usernet plan's numbers come from the backend's own config, and the
    /// command line must carry *those* — a d-i preseeded onto the wrong segment
    /// looks exactly like a broken NAT.
    #[test]
    fn the_usernet_plan_matches_the_backend_it_configures() {
        let plan = net_plan("usernet", "ignored").expect("usernet exists on every host");
        let section = plan.section.expect("a [network] section");
        assert_eq!(section.backend, NetworkBackend::Usernet);
        // Validation refuses an interface for this backend, so the plan must not
        // invent one out of --interface's default.
        assert_eq!(section.interface, None);

        let expected = virtio_net::UserNetConfig::default();
        let address = plan.address.expect("a static address");
        assert_eq!(address.ip, expected.guest.to_string());
        assert_eq!(address.gateway, expected.gateway.to_string());
        // The relay, not a public resolver: the guest's only route out is the
        // gateway inside this process.
        assert_eq!(address.dns, expected.gateway.to_string());

        let c = auto_cmdline("testvm", &address);
        assert!(
            c.contains(&format!("netcfg/get_ipaddress={}", expected.guest)),
            "{c}"
        );
        assert!(
            c.contains(&format!("netcfg/get_nameservers={}", expected.gateway)),
            "{c}"
        );
    }

    /// The host that cannot do TAP says so, by name, and says what to use
    /// instead. Asserted on both hosts (one gets the refusal, the other the
    /// interface) so the message cannot rot on the host that never sees it.
    #[test]
    fn tap_is_refused_where_there_is_no_tap() {
        let plan = net_plan("tap", "entangled0");
        if cfg!(target_os = "linux") {
            let section = plan
                .expect("TAP resolves on Linux")
                .section
                .expect("a [network] section");
            assert_eq!(section.backend, NetworkBackend::Tap);
            assert_eq!(section.interface.as_deref(), Some("entangled0"));
            assert_eq!(crate::DEFAULT_NETWORK, "tap");
        } else {
            let message = plan.expect_err("TAP cannot resolve on a host with no TAP");
            assert_eq!(message, NETWORK_TAP_UNAVAILABLE);
            assert!(message.contains("--network usernet"), "{message}");
            assert_eq!(crate::DEFAULT_NETWORK, "usernet");
        }
    }

    /// An offline install has no address to preseed, and an unknown name is a
    /// typo rather than a silent default.
    #[test]
    fn none_is_offline_and_nonsense_is_refused() {
        let plan = net_plan("none", "entangled0").unwrap();
        assert!(plan.section.is_none() && plan.address.is_none());
        let e = net_plan("bridge", "entangled0").unwrap_err();
        assert!(e.contains("expected tap, usernet or none"), "{e}");
    }

    /// Where a machine goes when `--disk` was not given: the manager's VM
    /// directory, under the name every other file is derived from.
    #[test]
    fn names_and_default_paths_are_derived_the_way_the_manager_expects() {
        let args = |disk: Option<&str>, name: Option<&str>| InstallArgs {
            distro: "Ubuntu".into(),
            disk: disk.map(PathBuf::from),
            variant: "gtk-netboot".into(),
            auto: true,
            preseed: None,
            autoinstall: None,
            kickstart: None,
            iso: None,
            firmware: None,
            size: "20G".into(),
            memory_mib: 2048,
            interface: "entangled0".into(),
            network: crate::DEFAULT_NETWORK.into(),
            name: name.map(str::to_string),
            headless: true,
        };

        // A disk path in this host's spelling, with a space in it — the shape
        // that breaks a codebase that treats paths as strings.
        let disk = if cfg!(windows) {
            r"D:\my vms\my vm.raw"
        } else {
            "/srv/my vms/my vm.raw"
        };

        // --name wins; then the disk's stem, spaces and all; then the distro,
        // lower-cased (it is a file name, and `Ubuntu.raw` next to `ubuntu.toml`
        // is one machine on Windows and two on Linux).
        assert_eq!(vm_name(&args(None, Some("work"))), "work");
        assert_eq!(vm_name(&args(Some(disk), None)), "my vm");
        assert_eq!(vm_name(&args(None, None)), "ubuntu");

        // An explicit --disk is used verbatim, whatever it looks like.
        assert_eq!(
            target_disk(&args(Some(disk), None)).unwrap(),
            PathBuf::from(disk)
        );

        // The default lands in the manager's VM directory under the VM name.
        if let Some(dir) = diskfs::refs::manager_vm_dir() {
            assert_eq!(
                target_disk(&args(None, Some("demo"))).unwrap(),
                dir.join("demo.raw")
            );
        }
    }
}
