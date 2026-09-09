//! `entangled install fedora` — an unattended Fedora Workstation installation.
//!
//! # The machine
//!
//! ```text
//!   firmware   CLOUDHV.fd, entered through PVH (crate::firmware finds it:
//!              --firmware, this installation, the verified cache, this checkout)
//!   NVRAM      <disk>.nvram, a CFI flash device at 0xffc00000 (machine_x86::pflash)
//!   /dev/vda   the install target, writable
//!   /dev/vdb   the verified Fedora installer ISO, read-only
//!   /dev/vdc   a 64 KiB ISO9660 volume labelled OEMDRV holding ks.cfg, read-only
//!   network    usernet or a host TAP — a netinst downloads the whole system
//! ```
//!
//! The shape is [`crate::install_ubuntu`]'s, because the *host* side of an
//! unattended install is the same problem twice: verified read-only media, a
//! second labelled volume carrying the automation so the media is never
//! repacked, UEFI so the installer produces a disk this VMM can boot afterwards,
//! and a script that types at the bootloader over the 16550 because the one
//! thing a volume cannot carry is a kernel command line.
//!
//! Everything distribution-specific is in the three places it has to be: which
//! image, which automation language, and what gets typed.
//!
//! # Which image, and why not the Live one
//!
//! `scripts/fetch-fedora-iso.sh` verifies two images. The **Workstation Live**
//! ISO is the one that proves the product claim — it boots to a GNOME desktop on
//! virtio-gpu with no changes to this VMM at all, `entangled run --cdrom` and
//! nothing else. It is *not* the one that can be installed unattended:
//!
//! * its initramfs carries no anaconda dracut module — no `parse-kickstart`, no
//!   `fetch-kickstart-disk`, no OEMDRV udev rule — so there is nothing in it
//!   that can *find* a kickstart, by label or by `inst.ks=`;
//! * on Live media `%packages` is ignored anyway, because the install is a copy
//!   of the live filesystem rather than an assembly of packages.
//!
//! The **Everything netinst** image runs the classic Anaconda installer, whose
//! initramfs has all of that, honours `%packages`, and can therefore install
//! `@^workstation-product-environment` — the same environment group the
//! Workstation edition ships. It needs a network, which is why this command
//! configures one and the Ubuntu one does not.
//!
//! # How the kickstart is delivered
//!
//! Both at once, deliberately:
//!
//! 1. the volume is labelled `OEMDRV` and holds `/ks.cfg`, which is the pair
//!    Anaconda auto-detects when no `inst.ks=` is given at all;
//! 2. and `inst.ks=hd:LABEL=OEMDRV:/ks.cfg` is typed onto the kernel command
//!    line regardless.
//!
//! (2) is not redundant. Reading `50-kickstart-genrules.sh` in the installer
//! initramfs: with an explicit `inst.ks=hd:...` the initqueue calls
//! `wait_for_kickstart`, so a kickstart that never arrives **stalls visibly** in
//! the initramfs with `Can't get kickstart from ...` on the console. Without it,
//! the auto-detect branch waits a few seconds and then falls through to an
//! *interactive* Anaconda — which, in an unattended run nobody is watching, is a
//! machine that looks alive for an hour and installs nothing. Explicit is the
//! loud failure; the label is what makes the same ISO+volume pair work for a
//! person who boots it by hand.
//!
//! Typing it costs nothing extra either way: `console=ttyS0,115200n8` has to be
//! typed regardless, because Fedora's own `grub.cfg` sets no `console=` and
//! without one the whole install is invisible.

use std::path::{Path, PathBuf};

use control_api::{
    BootMode, BootSection, DiskSection, DisplaySection, GamepadBackend, GamepadSection,
    SoundBackend, SoundSection, VirtioTransport, VmConfig,
};

use crate::disk;
use crate::install::{net_plan, target_disk, vm_name, NetAddress};
use crate::paths;
use crate::run_vm::{self, Automation};
use crate::seed;
use crate::InstallArgs;
use disk_image as diskfs;

/// The maintained kickstart, compiled in so `--auto` works from any working
/// directory (the same reason the Debian preseed and the Ubuntu autoinstall are).
const KICKSTART: &str = include_str!("../../../assets/kickstart/fedora-workstation.ks");

/// Installer VM size. Anaconda's own documented minimum for Fedora is 2 GiB and
/// its dnf transaction for a full Workstation environment is the part that wants
/// the headroom; 3072 has run one comfortably.
const INSTALLER_MEMORY_MIB: u64 = 3072;

/// What the installed system gets when `--memory-mib` was left at its default.
/// GNOME on a software renderer is not a 2 GiB desktop.
const INSTALLED_MEMORY_MIB: u64 = 4096;

/// Placeholders the kickstart carries, substituted before it is written.
const HOSTNAME_PLACEHOLDER: &str = "@HOSTNAME@";

pub fn run(args: &InstallArgs) -> Result<(), String> {
    // 0. The network, first, because a netinst install without one cannot happen
    //    at all: every package comes over it. Resolved before anything is
    //    downloaded or created so `--network tap` on Windows costs nothing but
    //    the message.
    let net = net_plan(&args.network, &args.interface)?;
    let address = net.address.as_ref().ok_or(
        "the Fedora netinst installer downloads the whole system, so it cannot run with \
         --network none. Use --network usernet (no host setup) or --network tap",
    )?;

    // 1. Firmware. Named early because it is the one artifact a fresh host may
    //    not have, and the error has to say how to get it — in terms of what the
    //    person in front of the machine can do (crate::firmware).
    let firmware = crate::firmware::resolve(args.firmware.as_deref()).map_err(|e| {
        format!(
            "{e}\n  A Fedora install needs UEFI: Anaconda only creates an EFI System \
             Partition when the installer itself booted under firmware."
        )
    })?;
    tracing::info!(
        path = %firmware.path.display(),
        origin = firmware.origin.as_str(),
        "UEFI firmware"
    );
    let firmware = firmware.path;

    // 2. The verified ISO. Never fetched here: scripts/fetch-fedora-iso.sh owns
    //    the trust chain (pinned release key, clearsigned CHECKSUM), and this
    //    command only ever consumes what that produced.
    let iso = match &args.iso {
        Some(path) => {
            if !path.is_file() {
                return Err(format!("installer ISO {} not found", path.display()));
            }
            path.clone()
        }
        None => cached_netinst().ok_or_else(|| {
            "no Fedora netinst ISO in the cache — run \
             `bash scripts/fetch-fedora-iso.sh netinst` (~1.2 GiB, OpenPGP + SHA-256 \
             verified) or pass --iso"
                .to_string()
        })?,
    };
    // Which volume label the ISO carries decides what `inst.stage2=hd:LABEL=`
    // must say, and it changes with every compose. Read it rather than pin it —
    // and refuse the Live image here rather than an hour into a run that cannot
    // work (see the module docs).
    let media = InstallerMedia::read(&iso)?;
    tracing::info!(iso = %iso.display(), label = %media.label, "Fedora installer media");

    // 3. Target disk.
    let vm_name = vm_name(args);
    let target = target_disk(args)?;
    if !target.exists() {
        let bytes = disk::parse_size(&args.size).map_err(|e| e.to_string())?;
        disk::create_raw(&target, bytes).map_err(|e| e.to_string())?;
        tracing::info!(disk = %target.display(), bytes, "created target disk");
    }

    // 4. The kickstart volume. Written next to the disk so a failed install can
    //    be inspected — and re-run — without regenerating anything.
    if args.preseed.is_some() || args.autoinstall.is_some() {
        return Err(
            "--preseed is a Debian d-i file and --autoinstall an Ubuntu cloud-config; \
             for Fedora use --kickstart"
                .to_string(),
        );
    }
    let automated = args.auto || args.kickstart.is_some();
    let seed = if automated {
        let text = kickstart(&vm_name, address, args.kickstart.as_deref())?;
        let seed_path = target.with_file_name(format!("{vm_name}-ks.iso"));
        Some(seed::write_kickstart(&seed_path, &text).map_err(|e| e.to_string())?)
    } else {
        tracing::info!(
            "no --auto and no --kickstart: Anaconda will run interactively on ttyS0 \
             and on the window"
        );
        None
    };

    // 5. NVRAM. Deliberately recreated: a store left over from an earlier install
    //    still holds that install's Boot#### entry, and an installer that then
    //    failed halfway would leave a profile whose first boot lands on a
    //    partition that no longer exists.
    let nvram = target.with_file_name(format!("{vm_name}.nvram"));
    if nvram.exists() {
        std::fs::remove_file(&nvram)
            .map_err(|e| format!("cannot replace {}: {e}", nvram.display()))?;
        tracing::info!(path = %nvram.display(), "replaced the previous UEFI variable store");
    }

    // 6. The installer VM. Disk order is device order and the kickstart names
    //    /dev/vda, so the target must be first.
    let cfg = VmConfig {
        name: format!("{vm_name}-install"),
        memory_mib: args.memory_mib.max(INSTALLER_MEMORY_MIB),
        vcpus: 2,
        transport: VirtioTransport::Pci,
        boot: BootSection {
            mode: BootMode::Uefi,
            firmware: Some(firmware.clone()),
            nvram: Some(nvram.clone()),
            ..BootSection::default()
        },
        disks: [
            Some(DiskSection {
                path: target.clone(),
                writable: true,
            }),
            Some(DiskSection {
                path: iso.clone(),
                writable: false,
            }),
            seed.as_ref().map(|seed| DiskSection {
                path: seed.path.clone(),
                writable: false,
            }),
        ]
        .into_iter()
        .flatten()
        .collect(),
        cdrom: None,
        network: net.section.clone(),
        display: DisplaySection {
            width: 1280,
            height: 800,
            scale: 1.0,
            virgl: false,
            virgl_isolation: control_api::VirglIsolation::default(),
            refresh_hz: control_api::DEFAULT_REFRESH_HZ,
            frame_stats: None,
        },
        // The installer has nothing to say; the *installed* profile below is
        // where the card and the pad belong.
        sound: SoundSection::default(),
        gamepad: GamepadSection::default(),
    };

    let transcript = target.with_file_name(format!("{vm_name}-install.log"));
    tracing::info!(
        vm = %cfg.name,
        iso = %iso.display(),
        kickstart = seed
            .as_ref()
            .map(|s| s.path.display().to_string())
            .unwrap_or_else(|| "none (interactive)".into()),
        nvram = %nvram.display(),
        transcript = %transcript.display(),
        "starting the Fedora installer; it powers off when done (30-60 min — every \
         package comes over the network)"
    );

    let mut script = GrubScript::new(&media, automated);
    let report = run_vm::run_with(
        cfg,
        Some(Automation {
            script: Box::new(move |log| script.step(log)),
            transcript: Some(transcript.clone()),
        }),
        run_vm::RunOptions {
            headless: args.headless,
            // No control channel: the installer *is* the program driving this
            // VM, from inside the same process. And no snapshot: an install is
            // not a machine anybody wants to suspend half-way through
            // (ADR-0006) — least of all this one, whose disk is being written
            // by an Anaconda that would not survive being resumed beside it.
            ..Default::default()
        },
    )
    .map_err(|e| installer_failure(&args.network, &args.interface, &e))?;

    // 7. Did the installation finish? The kickstart says `poweroff`, so a
    //    completed install ends as an ACPI S5 write that this machine latches —
    //    anything else means it stopped early, and the transcript is the only
    //    thing that can say why.
    let log = report.serial.unwrap_or_default();
    if !report.guest_shutdown {
        return Err(format!(
            "the installer did not power off, so the installation did not finish. \
             Last lines of {}:\n{}",
            transcript.display(),
            tail(&log, 25)
        ));
    }
    for marker in INSTALL_MARKERS {
        if !log.contains(marker) {
            tracing::warn!(marker, "the installer transcript is missing a usual marker");
        }
    }

    // 8. What is actually on the disk: GPT, an ESP, a root. Fedora's default
    //    layout roots on btrfs, so there is no ext4 UUID to report — the ESP and
    //    the NVRAM boot entry are what make the disk bootable, not the UUID.
    let install = diskfs::find_uefi_install(&target).map_err(|e| {
        format!(
            "the installer powered off but {} does not look installed: {e}\n\
             Last lines of {}:\n{}",
            target.display(),
            transcript.display(),
            tail(&log, 25)
        )
    })?;
    tracing::info!(
        esp = install.esp.index,
        esp_sectors = install.esp.sectors(),
        root = install.root.index,
        root_uuid = install.root_uuid.as_deref().unwrap_or("not ext4 (btrfs)"),
        "installation detected"
    );

    // 9. The profile that boots the *installed* system. No ISO, no kickstart:
    //    the firmware finds \\EFI\\fedora\\shimx64.efi through the Boot#### entry
    //    the bootloader wrote into the NVRAM store, which is why `nvram` is the
    //    key that makes this profile work more than once.
    let profile = VmConfig {
        name: vm_name.clone(),
        memory_mib: args.memory_mib.max(INSTALLED_MEMORY_MIB),
        vcpus: 2,
        transport: VirtioTransport::Pci,
        boot: BootSection {
            mode: BootMode::Uefi,
            firmware: Some(firmware),
            nvram: Some(nvram.clone()),
            ..BootSection::default()
        },
        disks: vec![DiskSection {
            path: target.clone(),
            writable: true,
        }],
        cdrom: None,
        network: net.section.clone(),
        display: DisplaySection {
            width: 1280,
            height: 800,
            scale: 1.0,
            virgl: false,
            virgl_isolation: control_api::VirglIsolation::default(),
            refresh_hz: control_api::DEFAULT_REFRESH_HZ,
            frame_stats: None,
        },
        // A desktop with no sound is not a desktop (GAME-2102). `auto` never
        // fails a run: a host with no audio device gets a card that plays into
        // silence, and the guest still enumerates one.
        sound: SoundSection {
            enabled: true,
            backend: SoundBackend::Auto,
        },
        // …and neither is a desktop you cannot play on (GAME-2104). Same
        // bargain as the sound card: `auto` costs a virtio slot and nothing
        // else, a host with no controller gets a pad that never moves, and one
        // plugged in later is picked up without restarting the VM.
        gamepad: GamepadSection {
            enabled: true,
            // One pad. A second is a config edit away and costs another virtio
            // slot, which an installed profile with disk + cdrom cannot spare.
            players: 1,
            backend: GamepadBackend::Auto,
        },
    };
    let profile_path = target.with_file_name(format!("{vm_name}.toml"));
    let text = toml::to_string_pretty(&profile).map_err(|e| e.to_string())?;
    std::fs::write(&profile_path, text)
        .map_err(|e| format!("cannot write {}: {e}", profile_path.display()))?;

    println!(
        "installed:  GPT with an ESP on /dev/vda{} ({} MiB) and root on /dev/vda{}{}\n\
         nvram:      {}\n\
         transcript: {}\n\
         profile:    {}\n\
         run it:     entangled run {}",
        install.esp.index,
        install.esp.sectors() * diskfs::SECTOR / (1 << 20),
        install.root.index,
        install
            .root_uuid
            .as_deref()
            .map(|u| format!(" (ext4 UUID {u})"))
            .unwrap_or_else(|| " (btrfs)".to_string()),
        nvram.display(),
        transcript.display(),
        profile_path.display(),
        profile_path.display()
    );
    Ok(())
}

/// Lines that a healthy unattended run produces. Their absence does not fail the
/// install — the disk is the source of truth — but it is worth saying so, because
/// it usually means the automation took a different path than intended.
const INSTALL_MARKERS: &[&str] = &[
    // The dracut module found and parsed the kickstart volume.
    "kickstart",
    // Anaconda started.
    "anaconda",
];

// ---------------------------------------------------------------------------
// The installer media
// ---------------------------------------------------------------------------

/// What has to be known about a Fedora installer ISO before it can be booted
/// unattended: its volume label (which `inst.stage2=hd:LABEL=` must repeat) and
/// where its kernel and initramfs are.
///
/// Read from the image rather than pinned. The label carries the compose number
/// (`Fedora-E-dvd-x86_64-44`), so pinning it would break on every respin, and
/// getting it wrong means an installer that boots its kernel and then cannot
/// find its own second stage.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstallerMedia {
    pub label: String,
}

/// ISO9660 logical sector size; the primary volume descriptor is at sector 16
/// and its volume identifier at offset 40, 32 bytes, space padded (ECMA-119
/// §8.4.6). This is the same field `blkid` reports as `LABEL=`.
const ISO_SECTOR: u64 = 2048;
const PVD_SECTOR: u64 = 16;
const VOLUME_ID_OFFSET: u64 = 40;
const VOLUME_ID_LEN: usize = 32;

impl InstallerMedia {
    pub fn read(iso: &Path) -> Result<Self, String> {
        use std::io::{Read, Seek, SeekFrom};
        let mut file = std::fs::File::open(iso)
            .map_err(|e| format!("cannot read the installer ISO {}: {e}", iso.display()))?;
        let mut descriptor = [0u8; 8];
        file.seek(SeekFrom::Start(PVD_SECTOR * ISO_SECTOR))
            .and_then(|_| file.read_exact(&mut descriptor))
            .map_err(|e| format!("{} is not readable as an ISO9660 image: {e}", iso.display()))?;
        if &descriptor[1..6] != b"CD001" || descriptor[0] != 1 {
            return Err(format!(
                "{} has no ISO9660 primary volume descriptor — it is not installer media",
                iso.display()
            ));
        }
        let mut raw = [0u8; VOLUME_ID_LEN];
        file.seek(SeekFrom::Start(PVD_SECTOR * ISO_SECTOR + VOLUME_ID_OFFSET))
            .and_then(|_| file.read_exact(&mut raw))
            .map_err(|e| format!("cannot read the volume label of {}: {e}", iso.display()))?;
        Self::from_label(&String::from_utf8_lossy(&raw), &iso.display().to_string())
    }

    /// The half that is pure logic, so the refusals below are tested without an
    /// ISO on disk.
    fn from_label(raw: &str, source: &str) -> Result<Self, String> {
        let label = raw.trim().to_string();
        if label.is_empty() {
            return Err(format!(
                "{source} has an empty ISO9660 volume label, so \
                 `inst.stage2=hd:LABEL=` has nothing to name — this is not Fedora \
                 installer media"
            ));
        }
        // The Live image is the one that cannot be kickstarted at all (see the
        // module docs). Catching it here turns a silent hour into one line.
        if label.contains("Live") {
            return Err(format!(
                "{source} is a Fedora Live image ({label}). Its initramfs carries no \
                 anaconda dracut module, so it cannot find a kickstart by any route, and \
                 on Live media the package list is ignored anyway. Use the Everything \
                 netinst image — `bash scripts/fetch-fedora-iso.sh netinst` — or boot the \
                 Live image interactively with `entangled run --cdrom`, which works \
                 exactly as it does on real hardware."
            ));
        }
        if !label.starts_with("Fedora") {
            return Err(format!(
                "{source} is labelled {label:?}, which is not Fedora installer media"
            ));
        }
        Ok(Self { label })
    }
}

// ---------------------------------------------------------------------------
// Typing at GRUB
// ---------------------------------------------------------------------------

/// GRUB's menu, drawn on ttyS0 by the EFI console. Waiting for the *entry* text
/// rather than for the version banner means the menu is really up and a
/// keystroke will be seen.
const MENU_MARKER: &str = "Install Fedora";

/// GRUB's command-line prompt. One appears after `c`, and one after every
/// command completes — which is exactly the acknowledgement each line needs.
const PROMPT: &str = "grub>";

/// Where the netinst image keeps its kernel and initramfs. Verbatim from the
/// ISO's own `/EFI/BOOT/grub.cfg`; `$root` is already the installer volume,
/// because that is where GRUB found the config it drew the menu from.
const KERNEL: &str = "/images/pxeboot/vmlinuz";
const INITRD: &str = "/images/pxeboot/initrd.img";

/// The commands typed at that prompt.
fn grub_commands(media: &InstallerMedia, automated: bool) -> Vec<String> {
    let label = &media.label;
    // `inst.ks` explicit even though the volume is labelled OEMDRV: it makes the
    // initramfs *wait* for the kickstart instead of falling through to an
    // interactive Anaconda nobody is watching. See the module docs.
    let ks = if automated {
        format!("inst.ks=hd:LABEL={}:/ks.cfg ", seed::OEMDRV_LABEL)
    } else {
        String::new()
    };
    vec![
        "echo entangled: root=$root prefix=$prefix".to_string(),
        // `inst.stage2` names the volume by label, exactly as the ISO's own menu
        // entry does; `console=` is ours, because Fedora's grub.cfg sets none
        // and without it the whole install is invisible.
        format!("linux {KERNEL} inst.stage2=hd:LABEL={label} {ks}console=ttyS0,115200n8"),
        format!("initrd {INITRD}"),
        "boot".to_string(),
    ]
}

/// Types the boot commands one prompt at a time.
pub struct GrubScript {
    commands: Vec<String>,
    entered_command_line: bool,
    lines_sent: usize,
}

impl GrubScript {
    /// `automated` puts `inst.ks=` on the typed command line, which is what makes
    /// the installer wait for the kickstart volume rather than run interactively.
    pub fn new(media: &InstallerMedia, automated: bool) -> Self {
        Self {
            commands: grub_commands(media, automated),
            entered_command_line: false,
            lines_sent: 0,
        }
    }

    /// Given everything the guest has printed so far, returns the next keys to
    /// type — or `None` while waiting for GRUB to catch up.
    pub fn step(&mut self, log: &str) -> Option<Vec<u8>> {
        if !self.entered_command_line {
            if !log.contains(MENU_MARKER) {
                return None;
            }
            // `c` both stops the countdown and opens the command line.
            self.entered_command_line = true;
            tracing::info!("GRUB menu is up; opening its command line");
            return Some(b"c".to_vec());
        }
        if self.lines_sent >= self.commands.len() {
            return None;
        }
        // One prompt for the command line itself, one after each completed
        // command: send line n only once prompt n+1 has appeared.
        let prompts = log.matches(PROMPT).count();
        if prompts <= self.lines_sent {
            return None;
        }
        let line = self.commands[self.lines_sent].clone();
        self.lines_sent += 1;
        tracing::info!(%line, "typing into GRUB");
        // Carriage return: what the Enter key sends on a serial terminal.
        Some(format!("{line}\r").into_bytes())
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Builds the kickstart for `hostname` on `address`, from the compiled-in
/// profile or from a caller-supplied file.
fn kickstart(
    hostname: &str,
    address: &NetAddress,
    custom: Option<&Path>,
) -> Result<String, String> {
    let template = match custom {
        Some(path) => std::fs::read_to_string(path)
            .map_err(|e| format!("cannot read the kickstart {}: {e}", path.display()))?,
        None => KICKSTART.to_string(),
    };
    Ok(template
        .replace(HOSTNAME_PLACEHOLDER, hostname)
        .replace("@IP@", &address.ip)
        .replace("@NETMASK@", &address.netmask)
        .replace("@GATEWAY@", &address.gateway)
        .replace("@DNS@", &address.dns))
}

/// The newest netinst ISO `scripts/fetch-fedora-iso.sh` has verified into the
/// cache. Explicitly *not* the newest ISO of any kind: the Workstation Live
/// image sorts after it in the same directory and cannot be installed from.
fn cached_netinst() -> Option<PathBuf> {
    let dir = paths::fedora_cache_dir().ok()?;
    let mut candidates: Vec<PathBuf> = std::fs::read_dir(dir)
        .ok()?
        .filter_map(Result::ok)
        .flat_map(|release| {
            std::fs::read_dir(release.path())
                .into_iter()
                .flatten()
                .filter_map(Result::ok)
                .map(|file| file.path())
                .filter(|path| {
                    paths::has_extension(path, "iso")
                        && path
                            .file_name()
                            .and_then(|n| n.to_str())
                            .is_some_and(|n| n.contains("netinst"))
                })
                .collect::<Vec<_>>()
        })
        .collect();
    candidates.sort();
    candidates.pop()
}

/// Why the installer VM never got going, with the one piece of advice that is
/// almost always the answer on a fresh Linux host.
///
/// `--network` defaults to a host TAP there, and a TAP is state somebody has to
/// create as root (`scripts/setup-tap.sh`). The Ubuntu install never meets this
/// because its installer VM has no network at all; a netinst cannot do that, so
/// the first thing an unprepared host sees is an opaque "Operation not
/// permitted" half a second in. Say what to type instead of leaving the errno
/// to be interpreted.
fn installer_failure(network: &str, interface: &str, error: &str) -> String {
    if network.eq_ignore_ascii_case("tap") {
        format!(
            "installer VM failed: {error}\n\
             A Fedora netinst must have a network, and --network defaults to the host TAP \
             interface '{interface}' on Linux. Either create it (bash scripts/setup-tap.sh) \
             or pass --network usernet, which is user-mode NAT inside this process and needs \
             no host setup at all."
        )
    } else {
        format!("installer VM failed: {error}")
    }
}

/// The last `lines` non-empty lines of a transcript, for an error message.
fn tail(log: &str, lines: usize) -> String {
    let kept: Vec<&str> = log
        .lines()
        .map(str::trim_end)
        .filter(|l| !l.is_empty())
        .collect();
    kept[kept.len().saturating_sub(lines)..].join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn netinst() -> InstallerMedia {
        InstallerMedia {
            label: "Fedora-E-dvd-x86_64-44".to_string(),
        }
    }

    fn address() -> NetAddress {
        NetAddress {
            ip: "10.0.2.15".into(),
            gateway: "10.0.2.2".into(),
            netmask: "255.255.255.0".into(),
            dns: "10.0.2.2".into(),
        }
    }

    /// The label is read out of the image, because it carries the compose number
    /// and `inst.stage2=hd:LABEL=` has to repeat it exactly.
    #[test]
    fn the_volume_label_is_taken_from_the_media() {
        let media =
            InstallerMedia::from_label("Fedora-E-dvd-x86_64-44              ", "x.iso").unwrap();
        assert_eq!(media.label, "Fedora-E-dvd-x86_64-44");
        let linux = grub_commands(&media, true)
            .into_iter()
            .find(|c| c.starts_with("linux "))
            .expect("a linux command");
        assert!(
            linux.contains("inst.stage2=hd:LABEL=Fedora-E-dvd-x86_64-44"),
            "{linux}"
        );
    }

    /// The Live image is refused with the reason and the alternative, not with a
    /// stall an hour later. This is the one distribution-specific trap of the
    /// whole exercise, so it fails in the first second of the command.
    #[test]
    fn the_live_image_is_refused_by_name() {
        let error = InstallerMedia::from_label("Fedora-WS-Live-44", "live.iso")
            .expect_err("Live media cannot be kickstarted");
        assert!(error.contains("Live image"), "{error}");
        assert!(error.contains("netinst"), "{error}");
        assert!(error.contains("--cdrom"), "{error}");

        // And anything that is not Fedora media at all.
        assert!(InstallerMedia::from_label("Ubuntu 26.04 amd64", "u.iso").is_err());
        assert!(InstallerMedia::from_label("      ", "blank.iso").is_err());
    }

    /// The script must not type into a menu that is not there — a keystroke sent
    /// during firmware startup is swallowed, and then the countdown boots the
    /// installer *without* `inst.ks`, which is a long wait ending at an
    /// interactive installer nobody is watching.
    #[test]
    fn nothing_is_typed_before_the_menu_appears() {
        let media = netinst();
        let mut script = GrubScript::new(&media, true);
        assert_eq!(script.step(""), None);
        assert_eq!(script.step("PciBus: Discovered PCI @ [00|01|00]"), None);
        assert_eq!(script.step("GNU GRUB  version 2.14"), None);
        assert_eq!(script.step("  *Install Fedora 44"), Some(b"c".to_vec()));
    }

    /// Each command waits for its own prompt. Sending them faster than GRUB
    /// consumes them would work by luck of the UART's receive buffer and fail
    /// invisibly the moment a line was dropped.
    #[test]
    fn one_command_per_prompt_in_order() {
        let media = netinst();
        let mut script = GrubScript::new(&media, true);
        let mut log = String::from("  *Install Fedora 44\n");
        assert_eq!(script.step(&log), Some(b"c".to_vec()));
        assert_eq!(script.step(&log), None, "still no prompt");

        let commands = grub_commands(&media, true);
        for (i, command) in commands.iter().enumerate() {
            log.push_str("grub> ");
            let keys = script.step(&log).expect("a command per prompt");
            assert_eq!(
                String::from_utf8_lossy(&keys),
                format!("{command}\r"),
                "command {i}"
            );
            assert_eq!(script.step(&log), None, "after command {i}");
            log.push_str(command);
            log.push('\n');
        }
        log.push_str("grub> grub> ");
        assert_eq!(script.step(&log), None);
    }

    /// The typed command line is the whole point: the second stage (or the
    /// installer stops at a prompt asking where its own image is), the kickstart
    /// (or it runs interactively) and the console (or nothing is observable).
    #[test]
    fn the_typed_command_line_carries_what_it_must() {
        let media = netinst();
        let linux_line = |automated| {
            grub_commands(&media, automated)
                .into_iter()
                .find(|c| c.starts_with("linux "))
                .expect("a linux command")
        };
        let automated = linux_line(true);
        assert!(automated.contains(KERNEL), "{automated}");
        assert!(
            automated.contains("inst.stage2=hd:LABEL=Fedora-E-dvd-x86_64-44"),
            "{automated}"
        );
        assert!(
            automated.contains("inst.ks=hd:LABEL=OEMDRV:/ks.cfg"),
            "{automated}"
        );
        assert!(automated.contains("console=ttyS0,115200n8"), "{automated}");
        assert_eq!(
            grub_commands(&media, true).last().map(String::as_str),
            Some("boot")
        );
        assert!(grub_commands(&media, true).contains(&format!("initrd {INITRD}")));

        // Interactive: still on the serial console and still able to find its
        // second stage, but no kickstart — promising one with no volume would
        // stall the initramfs waiting for a file that never arrives.
        let interactive = linux_line(false);
        assert!(!interactive.contains("inst.ks"), "{interactive}");
        assert!(
            interactive.contains("inst.stage2=hd:LABEL="),
            "{interactive}"
        );
        assert!(
            interactive.contains("console=ttyS0,115200n8"),
            "{interactive}"
        );
    }

    /// Every placeholder must go: an unsubstituted `@IP@` is a kickstart
    /// Anaconda rejects, and an unsubstituted `@HOSTNAME@` would become the
    /// installed system's name.
    #[test]
    fn the_builtin_kickstart_is_fully_substituted() {
        let text = kickstart("fedora-demo", &address(), None).unwrap();
        for placeholder in ["@HOSTNAME@", "@IP@", "@NETMASK@", "@GATEWAY@", "@DNS@"] {
            assert!(!text.contains(placeholder), "{placeholder} survived");
        }
        assert!(text.contains("--hostname=fedora-demo"));
        assert!(text.contains("--ip=10.0.2.15"));
        assert!(text.contains("--nameserver=10.0.2.2"));

        // The directives the install depends on, each for a stated reason.
        for needle in [
            "text",                              // a TUI on the serial console
            "poweroff",                          // how the host learns it finished
            "@^workstation-product-environment", // what makes it Workstation
            "ignoredisk --only-use=vda",         // never the ISO or the ks volume
            "clearpart --all --initlabel --drives=vda",
            "autopart --type=btrfs",  // Fedora's own default layout
            "console=ttyS0,115200n8", // the installed system speaks too
            "serial-getty@ttyS0.service",
            // Anaconda's text mode leaves default.target at multi-user even
            // with GNOME installed and gdm enabled; without this line the
            // install succeeds and the desktop never appears.
            "systemctl set-default graphical.target",
            "AutomaticLogin=entangled",
        ] {
            assert!(text.contains(needle), "missing {needle}");
        }
        // A password *hash*, never a plaintext password.
        assert!(text.contains("--iscrypted --password=$6$"));
        assert!(text.contains("rootpw --lock"));
    }

    /// The TAP default is the one failure every unprepared Linux host hits, and
    /// it hits it in the first second — so the message has to carry the fix.
    #[test]
    fn a_tap_installer_failure_names_the_alternative() {
        let tap = installer_failure(
            "tap",
            "entangled0",
            "cannot open TAP 'entangled0': Operation not permitted (os error 1)",
        );
        assert!(tap.contains("Operation not permitted"), "{tap}");
        assert!(tap.contains("--network usernet"), "{tap}");
        assert!(tap.contains("setup-tap.sh"), "{tap}");
        assert!(tap.contains("entangled0"), "{tap}");

        // A user who chose usernet and still failed does not need TAP advice;
        // they need their own error and nothing on top of it.
        let usernet = installer_failure("usernet", "entangled0", "the firmware did not load");
        assert_eq!(usernet, "installer VM failed: the firmware did not load");
    }

    #[test]
    fn tail_keeps_the_last_lines_and_drops_blanks() {
        let log = "a\n\n b \n\nc\nd\n";
        assert_eq!(tail(log, 2), "c\nd");
        assert_eq!(tail(log, 99), "a\n b\nc\nd");
        assert_eq!(tail("", 5), "");
    }
}
