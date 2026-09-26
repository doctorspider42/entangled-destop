//! `entangled install ubuntu` — an unattended Ubuntu Server installation
//! (backlog UEFI-1804, [ADR-0003](../../../docs/adr/0003-uefi-firmware.md)).
//!
//! # The machine
//!
//! ```text
//!   firmware   CLOUDHV.fd, entered through PVH (crate::firmware finds it:
//!              --firmware, this installation, the verified cache, this checkout)
//!   NVRAM      <disk>.nvram, a CFI flash device at 0xffc00000 (machine_x86::pflash)
//!   /dev/vda   the install target, writable
//!   /dev/vdb   the verified live-server ISO, read-only
//!   /dev/vdc   a 64 KiB ISO9660 seed labelled CIDATA, read-only
//! ```
//!
//! Every one of those five lines is load-bearing. UEFI rather than a direct
//! kernel load, because subiquity decides between an ESP and a BIOS boot
//! partition by looking at the firmware it booted under — a direct load would
//! produce a disk this VMM cannot boot at all (there is no CSM). NVRAM, because
//! `grub-install` records the installed system as a `Boot####` variable and
//! nothing else points at it. The seed as a third volume, because the
//! alternative is repacking media whose whole value is that its provenance was
//! verified.
//!
//! # The network: none for the installer, one for the machine
//!
//! The installer VM has no NIC. The install is offline by design — every
//! package comes from the verified ISO's own pool, so it is reproducible, it
//! takes the ten minutes it was measured at, and no mirror or `updates:
//! security` download can change what was tested — and the installed machine
//! gets updates the way any Ubuntu does, with `apt` once it runs. What the
//! installer does not need, the machine does: the written profile carries
//! `--network` (usernet unless told otherwise) as a `[network]` section with a
//! MAC of its own, and the guest configures that NIC by DHCP — NetworkManager
//! on the Desktop ISO's install, a netplan file the server profile's
//! late-commands write on the server's (ADR-0002, the installed-network
//! amendment).
//!
//! # Why GRUB gets typed at
//!
//! subiquity finds the seed on its own (cloud-init matches the `CIDATA` label),
//! but it will not *act* on an autoinstall configuration unattended unless the
//! word `autoinstall` appears in `/proc/cmdline`. That is a single
//! unconditional check in the installer — no key in the configuration file
//! changes it, `interactive-sections: []` included — and the command line comes
//! from the ISO's own `grub.cfg`, which is read-only verified media.
//!
//! So this command does what the documentation tells a person to do ("interrupt
//! the booting process, and add the `autoinstall` parameter to the kernel
//! command line"), on the only input channel the boot chain has: the 16550.
//! EDK2's console is ttyS0 here — CloudHv ships no `VirtioGpuDxe`, so the
//! firmware and GRUB never appear on the scanout — and GRUB reads the same
//! console. [`GrubScript`] presses `c` for GRUB's command line and then types
//! four commands, each one only after GRUB has printed a fresh prompt, so the
//! whole exchange is synchronised by (and visible in) the transcript.
//!
//! The same command line also carries `console=ttyS0,115200n8`, which is why
//! anything at all is known about the install afterwards: the ISO's own command
//! line has no `console=` clause, so without this the installer would run blind.

use std::path::PathBuf;

use control_api::{
    BootMode, BootSection, DiskSection, DisplaySection, GamepadBackend, GamepadSection,
    NetworkBackend, NetworkSection, SoundBackend, SoundSection, VirtioTransport, VmConfig,
};

use crate::disk;
use crate::install::{
    installed_refresh_hz_here, installed_vcpus_here, net_plan, target_disk, vm_name,
};
use crate::paths;
use crate::run_vm::{self, Automation};
use crate::seed;
use crate::InstallArgs;
use disk_image as diskfs;

/// Installer VM size. subiquity wants ~2 GiB; 2560 is the generous choice
/// `examples/ubuntu-uefi.toml` documents. (Guests above 3072 MiB are legal
/// since the high-RAM split, but the text installer gains nothing from more.)
const INSTALLER_MEMORY_MIB: u64 = 2560;

/// What the installed system gets when `--memory-mib` was left at its default.
/// Less than the installer needs: nothing is unpacking a 1.2 GiB squashfs any
/// more. An explicit `--memory-mib` above this carries through to the written
/// profile — a desktop install sized at 4096 must not boot into 2048.
const INSTALLED_MEMORY_MIB: u64 = 2048;

/// What a GPU desktop (`--venus`) gets at least: the size its measurements
/// were taken at (ADR-0004's GNOME-on-the-GPU amendments). Zink sizes its
/// mapped-bytes limit from guest RAM, and GNOME with a few GL clients on it
/// is not a 2 GiB machine.
const VENUS_MEMORY_MIB: u64 = 4096;

pub fn run(args: &InstallArgs) -> Result<(), String> {
    // 0. The installed machine's network. Resolved first, like Debian's and
    //    Fedora's, so `--network tap` on Windows costs nothing but the message
    //    — although only the machine gets it: the installer runs offline (see
    //    the module docs).
    let net = net_plan(&args.network, &args.interface)?;
    if net
        .section
        .as_ref()
        .is_some_and(|s| s.backend == NetworkBackend::Tap)
    {
        tracing::warn!(
            interface = %args.interface,
            "an installed Ubuntu configures its NIC by DHCP, and a TAP segment has no DHCP \
             server unless one is run beside it (scripts/setup-tap.sh --dnsmasq); \
             --network usernet needs none"
        );
    }

    // 1. Firmware. Named first because it is the one artifact a fresh host may
    //    not have, and the error has to say how to get it — in terms of what
    //    the person in front of the machine can do (crate::firmware).
    let firmware = crate::firmware::resolve(args.firmware.as_deref()).map_err(|e| {
        format!(
            "{e}\n  An Ubuntu install needs UEFI: subiquity only creates an EFI System \
             Partition when the installer itself booted under firmware."
        )
    })?;
    tracing::info!(
        path = %firmware.path.display(),
        origin = firmware.origin.as_str(),
        "UEFI firmware"
    );
    let firmware = firmware.path;

    // 2. The verified ISO. Never fetched here: scripts/fetch-ubuntu-iso.sh owns
    //    the trust chain (pinned signing key, signed SHA256SUMS), and this
    //    command only ever consumes what that produced.
    let iso = match &args.iso {
        Some(path) => {
            if !path.is_file() {
                return Err(format!("installer ISO {} not found", path.display()));
            }
            path.clone()
        }
        None => cached_iso().ok_or_else(|| {
            "no Ubuntu ISO in the cache — run `bash scripts/fetch-ubuntu-iso.sh` \
             (~2.9 GiB, GPG + SHA-256 verified) or pass --iso"
                .to_string()
        })?,
    };

    // 3. Target disk. `--disk` when given, otherwise the manager's VM
    //    directory, so both surfaces list the same machines.
    let vm_name = vm_name(args);
    let target = target_disk(args)?;
    if !target.exists() {
        let bytes = disk::parse_size(&args.size).map_err(|e| e.to_string())?;
        disk::create_raw(&target, bytes).map_err(|e| e.to_string())?;
        tracing::info!(disk = %target.display(), bytes, "created target disk");
    }

    // 4. The autoinstall seed. Written next to the disk so a failed install can
    //    be inspected — and re-run — without regenerating anything.
    if args.preseed.is_some() {
        return Err(
            "--preseed is a Debian d-i file; for Ubuntu use --autoinstall with a \
                    cloud-config document"
                .to_string(),
        );
    }
    // Unattended only when asked for. Without `--auto`/`--autoinstall` there is
    // no seed and no `autoinstall` on the command line, and subiquity runs
    // interactively — on ttyS0 as well as on the window, because the typed
    // command line still carries `console=ttyS0`. That is a useful mode (it is
    // how a layout the built-in profile does not cover gets installed), and it is
    // not the same claim as "unattended", so it is not the same flag.
    let automated = args.auto || args.autoinstall.is_some();
    // The built-in profile follows the ISO: each names an install source only
    // its own ISO carries (`ubuntu-server-minimal`, `ubuntu-desktop-minimal`).
    let builtin = seed::BuiltinProfile::for_iso(&iso);
    if args.venus {
        venus_preflight(automated, args.autoinstall.is_some(), builtin)?;
        if let Err(why) = crate::doctor::venus_host() {
            tracing::warn!(
                %why,
                "this host cannot serve the Venus renderer; the installed profile will \
                 refuse to start here until it can (`entangled doctor`)"
            );
        }
    }
    let seed = if automated {
        if args.autoinstall.is_none() {
            tracing::info!(profile = ?builtin, iso = %iso.display(), "built-in autoinstall profile");
        }
        let user_data = seed::user_data(&vm_name, args.autoinstall.as_deref(), builtin)
            .and_then(|text| {
                if args.venus {
                    seed::with_venus_guest(&text)
                } else {
                    Ok(text)
                }
            })
            .map_err(|e| format!("cannot build the autoinstall configuration: {e}"))?;
        let seed_path = target.with_file_name(format!("{vm_name}-seed.iso"));
        Some(
            seed::write(&seed_path, &user_data, &format!("entangled-{vm_name}"))
                .map_err(|e| e.to_string())?,
        )
    } else {
        tracing::info!(
            "no --auto and no --autoinstall: subiquity will run interactively on ttyS0 \
             and on the window"
        );
        None
    };

    // 5. NVRAM. Deliberately recreated: a store left over from an earlier
    //    install still holds that install's Boot#### entry, and an installer
    //    that then failed halfway would leave a profile whose first boot lands
    //    on a partition that no longer exists.
    let nvram = target.with_file_name(format!("{vm_name}.nvram"));
    if nvram.exists() {
        std::fs::remove_file(&nvram)
            .map_err(|e| format!("cannot replace {}: {e}", nvram.display()))?;
        tracing::info!(path = %nvram.display(), "replaced the previous UEFI variable store");
    }

    // 6. The installer VM. Disk order is device order and the autoinstall file
    //    names /dev/vda, so the target must be first.
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
        // Offline by design (see the module docs): the machine gets its NIC,
        // the installer does not.
        network: None,
        display: DisplaySection {
            width: 1280,
            height: 800,
            scale: 1.0,
            virgl: false,
            virgl_isolation: control_api::VirglIsolation::default(),
            // The installer itself runs in 2D whatever the machine will be:
            // it draws nothing a GPU would help with, and it must not need a
            // host Vulkan device to install onto a disk.
            venus: false,
            refresh_hz: control_api::DEFAULT_REFRESH_HZ,
            frame_stats: None,
            host_visible_mib: None,
            gpu_memory_mib: None,
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
        seed = seed
            .as_ref()
            .map(|s| s.path.display().to_string())
            .unwrap_or_else(|| "none (interactive)".into()),
        nvram = %nvram.display(),
        transcript = %transcript.display(),
        "starting the Ubuntu installer; it powers off when done (20-40 min)"
    );

    let mut script = GrubScript::new(automated);
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
            // not a machine anybody wants to suspend half-way through.
            ..Default::default()
        },
    )
    .map_err(|e| format!("installer VM failed: {e}"))?;

    // 7. Did the installation finish? subiquity is configured with
    //    `shutdown: poweroff`, so a completed install ends as an ACPI S5 write
    //    that this machine latches — anything else means it stopped early, and
    //    the transcript is the only thing that can say why.
    let log = report.serial.unwrap_or_default();
    if !report.guest_shutdown {
        return Err(format!(
            "the installer did not power off, so the installation did not finish. \
             Last lines of {}:\n{}",
            transcript.display(),
            tail(&log, 25)
        ));
    }
    for marker in install_markers(builtin) {
        if !log.contains(marker) {
            tracing::warn!(marker, "the installer transcript is missing a usual marker");
        }
    }

    // 8. What is actually on the disk (UEFI-1804: GPT, an ESP, a root).
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
        root_uuid = install.root_uuid.as_deref().unwrap_or("not ext4"),
        "installation detected"
    );

    // 9. The profile that boots the *installed* system. No ISO, no seed: the
    //    firmware finds \\EFI\\ubuntu\\shimx64.efi through the Boot#### entry
    //    grub-install wrote into the NVRAM store, which is why `nvram` is the
    //    key that makes this profile work more than once.
    let profile = installed_profile(
        args,
        &vm_name,
        firmware,
        nvram.clone(),
        target.clone(),
        net.machine_section(&vm_name, &target),
        installed_vcpus_here(args),
        installed_refresh_hz_here(args),
    );
    let profile_path = target.with_file_name(format!("{vm_name}.toml"));
    let text = toml::to_string_pretty(&profile).map_err(|e| e.to_string())?;
    std::fs::write(&profile_path, text)
        .map_err(|e| format!("cannot write {}: {e}", profile_path.display()))?;

    println!(
        "installed:  GPT with an ESP on /dev/vda{} ({} MiB) and root on /dev/vda{}{}\n\
         nvram:      {}\n\
         transcript: {}\n\
         profile:    {}\n\
         machine:    {} vCPUs, {} MiB, {}x{} at {} Hz\n\
         network:    {}\n\
         run it:     entangled run {}",
        install.esp.index,
        install.esp.sectors() * diskfs::SECTOR / (1 << 20),
        install.root.index,
        install
            .root_uuid
            .as_deref()
            .map(|u| format!(" (ext4 UUID {u})"))
            .unwrap_or_default(),
        nvram.display(),
        transcript.display(),
        profile_path.display(),
        profile.vcpus,
        profile.memory_mib,
        profile.display.width,
        profile.display.height,
        profile.display.refresh_hz,
        network_summary(profile.network.as_ref()),
        profile_path.display()
    );
    if args.venus {
        println!(
            "gpu:        [display] venus = true — GNOME and every GL client on Zink over the \
             host GPU (the guest's /etc/drirc, and its session environment for snaps), no idle \
             blank"
        );
    }
    Ok(())
}

/// The `network:` line of the install summary.
fn network_summary(network: Option<&NetworkSection>) -> String {
    match network {
        None => "none (--network none)".to_string(),
        Some(section) => {
            let mac = section.mac.as_deref().unwrap_or("derived from the name");
            match section.backend {
                NetworkBackend::Usernet => format!(
                    "usernet — user-mode NAT, the guest takes its address by DHCP; MAC {mac}"
                ),
                NetworkBackend::Tap => format!(
                    "tap on {} — needs a DHCP server on that segment; MAC {mac}",
                    section.interface.as_deref().unwrap_or("?")
                ),
            }
        }
    }
}

/// The profile that boots the *installed* system, with its network, vCPU
/// count and refresh rate already decided
/// ([`crate::install::NetPlan::machine_section`],
/// [`crate::install::installed_vcpus`], [`crate::install::installed_refresh_hz`])
/// so a test can inject them.
#[allow(clippy::too_many_arguments)]
fn installed_profile(
    args: &InstallArgs,
    vm_name: &str,
    firmware: PathBuf,
    nvram: PathBuf,
    target: PathBuf,
    network: Option<NetworkSection>,
    vcpus: u32,
    refresh_hz: u32,
) -> VmConfig {
    let installed_memory = if args.venus {
        VENUS_MEMORY_MIB
    } else {
        INSTALLED_MEMORY_MIB
    };
    VmConfig {
        name: vm_name.to_string(),
        memory_mib: args.memory_mib.max(installed_memory),
        vcpus,
        transport: VirtioTransport::Pci,
        boot: BootSection {
            mode: BootMode::Uefi,
            firmware: Some(firmware),
            nvram: Some(nvram),
            ..BootSection::default()
        },
        disks: vec![DiskSection {
            path: target,
            writable: true,
        }],
        cdrom: None,
        network,
        display: installed_display(args.venus, refresh_hz),
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
    }
}

/// The installed machine's display: 1920x1080 — the size the project targets
/// (CLAUDE.md) and the one GNOME comes up in, since the guest builds its
/// monitor from the EDID's preferred mode — with the GPU desktop the seed's
/// late-commands configured the guest for when `--venus` asked (ADR-0004,
/// "how a user turns it on").
///
/// It used to be 1280x800 for every machine, carried over from the installer
/// VM when this command installed only servers. Nothing chose it, and nothing
/// needs it: the 2D path keeps a 1080p GNOME at its refresh rate on both hosts
/// (ADR-0004, "installed profiles"), and the Debian profile has always been
/// 1080p. The installer VM keeps 1280x800: it is not the machine.
///
/// The refresh follows the host monitor ([`control_api::refresh::default_refresh_hz`]:
/// its rate up to the 144 Hz the GPU desktop keeps up with, a whole fraction
/// of it above that), unless `--refresh-hz` chose one (ADR-0004, the
/// high-refresh amendment): the guest composites at the rate its EDID
/// advertises, and a rate that divides the window's monitor's puts each guest
/// frame on screen for the same number of host refreshes.
fn installed_display(venus: bool, refresh_hz: u32) -> DisplaySection {
    DisplaySection {
        venus,
        refresh_hz,
        ..DisplaySection::default()
    }
}

/// Whether `--venus` can do what it says, before any disk is written to.
///
/// It configures the guest from the seed, so there must be one; and with the
/// built-in profile it needs the desktop one — a server install has no GNOME
/// to put on Zink, and no `glib-compile-schemas` for the late-command that
/// turns the idle blank off, which would fail the install twenty minutes in.
/// A custom `--autoinstall` is the author's to get right. The host's own
/// Vulkan is only *warned* about: the disk is portable, and the host that
/// runs it may not be this one.
fn venus_preflight(
    automated: bool,
    custom: bool,
    builtin: seed::BuiltinProfile,
) -> Result<(), String> {
    if !automated {
        return Err(
            "--venus configures the installed guest from the autoinstall seed, and an \
             interactive install has none: add --auto (with the Desktop ISO) or --autoinstall"
                .into(),
        );
    }
    if !custom && builtin != seed::BuiltinProfile::Desktop {
        return Err(
            "--venus sets up a GPU desktop, and this ISO is not an Ubuntu Desktop ISO (the \
             built-in profile follows the ISO's file name). Fetch one with `bash \
             scripts/fetch-ubuntu-iso.sh desktop` and pass it with --iso, or pass your own \
             --autoinstall"
                .into(),
        );
    }
    Ok(())
}

/// Lines that a healthy unattended run produces. Their absence does not fail the
/// install — the disk is the source of truth — but it is worth saying so, because
/// it usually means the automation took a different path than intended.
///
/// They depend on the ISO. The live-server installer logs subiquity's and
/// curtin's progress on ttyS0; the Desktop ISO's runs the same subiquity as
/// a service of its `ubuntu-desktop-bootstrap` snap and prints neither on the
/// console, so `curtin` never appears there, and a successful desktop install
/// warned that it was missing until the fresh `install ubuntu --venus`
/// acceptance read its transcript (0 hits in 186 KiB). What the desktop
/// transcript does show is systemd starting that service.
fn install_markers(profile: seed::BuiltinProfile) -> &'static [&'static str] {
    match profile {
        seed::BuiltinProfile::Server => &[
            // GRUB accepted the typed boot command.
            "autoinstall",
            // cloud-init found the seed volume.
            "cloud-init",
            // curtin ran.
            "curtin",
        ],
        seed::BuiltinProfile::Desktop => &[
            "autoinstall",
            "cloud-init",
            // The desktop installer's subiquity service started.
            "subiquity-server",
        ],
    }
}

// ---------------------------------------------------------------------------
// Typing at GRUB
// ---------------------------------------------------------------------------

/// GRUB's menu, drawn on ttyS0 by the EFI console. Waiting for the *entry* text
/// rather than for the version banner means the menu is really up and a
/// keystroke will be seen. Without "Server", because the Desktop ISO's entry is
/// "Try or Install Ubuntu" — this prefix matches both variants' menus.
const MENU_MARKER: &str = "Try or Install Ubuntu";

/// GRUB's command-line prompt. One appears after `c`, and one after every
/// command completes — which is exactly the acknowledgement each line needs.
const PROMPT: &str = "grub>";

/// The commands typed at that prompt.
///
/// No `search` for the ISO: GRUB found the menu at `($root)/boot/grub/grub.cfg`
/// on the ISO9660 filesystem, so `$root` already *is* the installer volume and
/// the paths are the ISO's own (`/casper/vmlinuz  ---` in its `grub.cfg`). The
/// first line prints them so that a run which goes wrong says where it was
/// looking, in the transcript, without anyone having to reproduce it.
fn grub_commands(automated: bool) -> Vec<String> {
    let autoinstall = if automated { "autoinstall " } else { "" };
    vec![
        "echo entangled: root=$root prefix=$prefix".to_string(),
        // The ISO menu entry is `linux /casper/vmlinuz  ---`; the paths and the
        // `---` separator are kept verbatim, with our arguments in front of it.
        format!("linux /casper/vmlinuz {autoinstall}console=ttyS0,115200n8 ---"),
        "initrd /casper/initrd".to_string(),
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
    /// `automated` puts `autoinstall` on the typed command line, which is what
    /// makes subiquity act on the seed without stopping for a confirmation.
    pub fn new(automated: bool) -> Self {
        Self {
            commands: grub_commands(automated),
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
            // `c` both stops the 30-second countdown and opens the command line.
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

impl Default for GrubScript {
    fn default() -> Self {
        Self::new(true)
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// The newest ISO `scripts/fetch-ubuntu-iso.sh` has verified into the cache.
///
/// The cache directory is [`crate::paths::ubuntu_cache_dir`], which is the same
/// one `entangled fetch` and the shell script use on this host — on Windows
/// `%LOCALAPPDATA%\entangled\ubuntu`, where a `HOME`-only resolution would have
/// found nothing and said "no ISO in the cache" next to a full cache.
fn cached_iso() -> Option<PathBuf> {
    paths::newest_iso(&paths::ubuntu_cache_dir().ok()?)
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

    /// The script must not type into a menu that is not there — a keystroke sent
    /// during firmware startup is swallowed, and then the 30-second countdown
    /// boots the installer *without* `autoinstall`, which is a 40-minute wait
    /// ending at an interactive confirmation nobody is watching.
    #[test]
    fn nothing_is_typed_before_the_menu_appears() {
        let mut script = GrubScript::new(true);
        assert_eq!(script.step(""), None);
        assert_eq!(script.step("PciBus: Discovered PCI @ [00|01|00]"), None);
        assert_eq!(script.step("GNU GRUB  version 2.14"), None);
        assert_eq!(
            script.step("  *Try or Install Ubuntu Server"),
            Some(b"c".to_vec())
        );
    }

    /// Each command waits for its own prompt. Sending them faster than GRUB
    /// consumes them would work by luck of the UART's 4 KiB receive buffer and
    /// fail invisibly the moment a line was dropped.
    #[test]
    fn one_command_per_prompt_in_order() {
        let mut script = GrubScript::new(true);
        let mut log = String::from("  *Try or Install Ubuntu Server\n");
        assert_eq!(script.step(&log), Some(b"c".to_vec()));

        // Still no prompt: nothing more is typed.
        assert_eq!(script.step(&log), None);

        let commands = grub_commands(true);
        for (i, command) in commands.iter().enumerate() {
            // GRUB prints a prompt; the next command follows.
            log.push_str("grub> ");
            let keys = script.step(&log).expect("a command per prompt");
            assert_eq!(
                String::from_utf8_lossy(&keys),
                format!("{command}\r"),
                "command {i}"
            );
            // And only one per prompt.
            assert_eq!(script.step(&log), None, "after command {i}");
            log.push_str(command);
            log.push('\n');
        }
        // Past the last command there is nothing left to type, however many
        // prompts appear.
        log.push_str("grub> grub> ");
        assert_eq!(script.step(&log), None);
    }

    /// The typed command line is the whole point: `autoinstall` (no
    /// confirmation prompt) and `console=ttyS0` (an installer we can see).
    #[test]
    fn the_typed_command_line_carries_what_it_must() {
        let linux_line = |automated| {
            grub_commands(automated)
                .into_iter()
                .find(|c| c.starts_with("linux "))
                .expect("a linux command")
        };
        let automated = linux_line(true);
        assert!(automated.contains(" autoinstall "), "{automated}");
        assert!(automated.contains("console=ttyS0,115200n8"), "{automated}");
        // The ISO menu entry's own paths and `---` separator, kept verbatim.
        assert!(automated.contains("/casper/vmlinuz"));
        assert!(automated.trim_end().ends_with("---"));
        assert!(grub_commands(true).contains(&"initrd /casper/initrd".to_string()));
        assert_eq!(grub_commands(true).last().map(String::as_str), Some("boot"));

        // Interactive: still on the serial console, but driven by a person, so
        // promising `autoinstall` with no seed would stop at "no autoinstall
        // configuration found" instead of doing anything useful.
        let interactive = linux_line(false);
        assert!(!interactive.contains("autoinstall"), "{interactive}");
        assert!(
            interactive.contains("console=ttyS0,115200n8"),
            "{interactive}"
        );
    }

    /// `--venus` is refused where it could only half-work: no seed to
    /// configure the guest from, or the built-in *server* profile, whose
    /// install would fail at the schema late-command twenty minutes in.
    /// A successful desktop install must not warn: its transcript never
    /// mentions curtin. These are lines of the Desktop ISO's real transcript
    /// (Ubuntu 26.04.1), with the systemd colour codes stripped.
    #[test]
    fn desktop_install_markers_are_ones_its_transcript_prints() {
        let desktop_transcript =
            "grub> linux /casper/vmlinuz autoinstall console=ttyS0,115200n8 ---
[   32.924591] cloud-init[464]:   en_US.UTF-8... done
[  OK  ] Started snap.ubuntu-desktop-bootst…desktop-bootstrap.subiquity-server.
[  632.206442] reboot: Power down
";
        for marker in install_markers(seed::BuiltinProfile::Desktop) {
            assert!(desktop_transcript.contains(marker), "{marker}");
        }
        assert!(!install_markers(seed::BuiltinProfile::Desktop).contains(&"curtin"));
        assert!(install_markers(seed::BuiltinProfile::Server).contains(&"curtin"));
    }

    #[test]
    fn venus_needs_a_seed_and_a_desktop() {
        use seed::BuiltinProfile::{Desktop, Server};
        let interactive = venus_preflight(false, false, Desktop).expect_err("no seed");
        assert!(interactive.contains("--auto"), "{interactive}");
        let server = venus_preflight(true, false, Server).expect_err("server profile");
        assert!(server.contains("Desktop ISO"), "{server}");
        assert!(venus_preflight(true, false, Desktop).is_ok());
        // A custom autoinstall is its author's: the ISO's name says nothing.
        assert!(venus_preflight(true, true, Server).is_ok());
    }

    fn desktop_args(venus: bool) -> InstallArgs {
        InstallArgs {
            distro: "ubuntu".into(),
            disk: Some(PathBuf::from("desktop.raw")),
            variant: "gtk-netboot".into(),
            auto: true,
            preseed: None,
            autoinstall: None,
            kickstart: None,
            iso: Some(PathBuf::from("ubuntu-26.04.1-desktop-amd64.iso")),
            firmware: None,
            size: "40G".into(),
            // clap's default: nobody passed --memory-mib.
            memory_mib: 1536,
            vcpus: None,
            refresh_hz: None,
            interface: "entangled0".into(),
            network: crate::DEFAULT_NETWORK.into(),
            name: None,
            headless: true,
            venus,
        }
    }

    fn profile_for(args: &InstallArgs, host_logical_cpus: usize) -> VmConfig {
        profile_on(args, host_logical_cpus, Some(60.0))
    }

    /// The installed profile on a host with `host_logical_cpus` threads and a
    /// primary monitor at `monitor_hz` (`None`: none could be read).
    fn profile_on(
        args: &InstallArgs,
        host_logical_cpus: usize,
        monitor_hz: Option<f64>,
    ) -> VmConfig {
        installed_profile(
            args,
            "desktop",
            PathBuf::from("CLOUDHV.fd"),
            PathBuf::from("desktop.nvram"),
            PathBuf::from("desktop.raw"),
            net_plan(&args.network, &args.interface)
                .expect("a network this host has")
                .machine_section("desktop", std::path::Path::new("desktop.raw")),
            crate::install::installed_vcpus(args, host_logical_cpus),
            crate::install::installed_refresh_hz(args, monitor_hz),
        )
    }

    /// The installed desktop follows the host monitor's refresh (ADR-0004,
    /// the high-refresh amendment): a 144 Hz panel gets 144, a 120 Hz one
    /// 120; this project's 239.76 Hz panel (which Windows reports as 239)
    /// gets 120, the whole fraction of it the GPU desktop keeps up with, as a
    /// 360 Hz one does; a host with no monitor to read (WSL) gets the 60 every
    /// profile had. `--refresh-hz` wins in both directions, and the profile
    /// reads back as written.
    #[test]
    fn the_installed_desktop_runs_at_the_host_monitors_refresh() {
        for (monitor, want) in [
            (Some(239.0), 120),
            (Some(143.9), 144),
            (Some(119.88), 120),
            (Some(59.94), 60),
            (Some(360.0), 120),
            (Some(165.0), 144),
            (Some(50.0), 60),
            (None, 60),
        ] {
            let cfg = profile_on(&desktop_args(true), 24, monitor);
            assert_eq!(cfg.display.refresh_hz, want, "{monitor:?}");
            let text = toml::to_string_pretty(&cfg).expect("serialises");
            assert!(text.contains(&format!("refresh_hz = {want}")), "{text}");
            let back = VmConfig::from_toml(&text).expect("the written profile is valid");
            assert_eq!(back, cfg, "{text}");
        }
        let mut args = desktop_args(true);
        args.refresh_hz = Some(75);
        assert_eq!(profile_on(&args, 24, Some(239.0)).display.refresh_hz, 75);
        args.refresh_hz = Some(240);
        assert_eq!(profile_on(&args, 24, Some(60.0)).display.refresh_hz, 240);
        args.refresh_hz = Some(30);
        assert_eq!(
            profile_on(&args, 24, None).display.refresh_hz,
            30,
            "below the default's floor, as a profile may"
        );
    }

    /// What `install ubuntu --venus --auto` used to write on this project's
    /// 24-thread host: 1280x800 and 2 vCPUs, for a GNOME desktop on the GPU.
    /// Now 1920x1080 and half the host (8), with the memory floors unchanged,
    /// and the profile reads back as exactly what was written.
    #[test]
    fn the_installed_desktop_is_1080p_with_half_the_host() {
        for venus in [true, false] {
            let args = desktop_args(venus);
            let cfg = profile_for(&args, 24);
            assert_eq!((cfg.display.width, cfg.display.height), (1920, 1080));
            assert_eq!(cfg.display.venus, venus);
            assert_eq!(cfg.vcpus, 8, "24 logical CPUs, venus={venus}");
            assert_eq!(
                cfg.memory_mib,
                if venus {
                    VENUS_MEMORY_MIB
                } else {
                    INSTALLED_MEMORY_MIB
                }
            );

            let text = toml::to_string_pretty(&cfg).expect("serialises");
            let back = VmConfig::from_toml(&text).expect("the written profile is valid");
            assert_eq!(back, cfg, "{text}");
            assert!(text.contains("width = 1920") && text.contains("height = 1080"));
            assert!(text.contains("vcpus = 8"), "{text}");
            // An older engine must still read a 2D profile (ADR-0004).
            assert_eq!(text.contains("venus"), venus, "{text}");
        }
        // A small host keeps the old 2.
        assert_eq!(profile_for(&desktop_args(true), 2).vcpus, 2);
        assert_eq!(profile_for(&desktop_args(true), 6).vcpus, 3);
    }

    /// Explicit flags are the user's: `--vcpus` wins over the host in both
    /// directions, and `--memory-mib` above a floor carries through.
    #[test]
    fn explicit_vcpus_and_memory_win() {
        let mut args = desktop_args(true);
        args.vcpus = Some(3);
        args.memory_mib = 8192;
        let cfg = profile_for(&args, 24);
        assert_eq!(cfg.vcpus, 3);
        assert_eq!(cfg.memory_mib, 8192);
        args.vcpus = Some(12);
        assert_eq!(
            profile_for(&args, 4).vcpus,
            12,
            "past the default's ceiling"
        );
    }

    /// The installed machine has a network by default — the gap the
    /// installed-network amendment closed: usernet with a MAC of its own,
    /// valid as written and read back exactly. `--network none` still means
    /// none; on Linux `--network tap` names its interface.
    #[test]
    fn the_installed_machine_is_networked_by_default() {
        let args = desktop_args(true);
        let cfg = profile_for(&args, 24);
        let network = cfg
            .network
            .as_ref()
            .expect("a [network] section by default");
        assert_eq!(network.backend, NetworkBackend::Usernet);
        assert_eq!(network.interface, None);
        let mac = network.mac.clone().expect("a MAC of its own");
        assert_eq!(
            mac,
            control_api::new_machine_mac("desktop", std::path::Path::new("desktop.raw"))
        );
        let text = toml::to_string_pretty(&cfg).expect("serialises");
        assert!(
            text.contains(&format!(
                "[network]\nbackend = \"usernet\"\nmac = \"{mac}\""
            )),
            "{text}"
        );
        assert_eq!(VmConfig::from_toml(&text).expect("valid"), cfg);
        assert!(network_summary(cfg.network.as_ref()).starts_with("usernet"));

        let mut offline = desktop_args(true);
        offline.network = "none".into();
        let cfg = profile_for(&offline, 24);
        assert_eq!(cfg.network, None);
        assert!(!toml::to_string_pretty(&cfg).unwrap().contains("[network]"));
        assert!(network_summary(None).starts_with("none"));

        if cfg!(target_os = "linux") {
            let mut tap = desktop_args(true);
            tap.network = "tap".into();
            let cfg = profile_for(&tap, 24);
            let network = cfg.network.as_ref().expect("a tap section");
            assert_eq!(network.backend, NetworkBackend::Tap);
            assert_eq!(network.interface.as_deref(), Some("entangled0"));
            assert!(network_summary(Some(network)).contains("DHCP server"));
        }
    }

    #[test]
    fn tail_keeps_the_last_lines_and_drops_blanks() {
        let log = "a\n\n b \n\nc\nd\n";
        assert_eq!(tail(log, 2), "c\nd");
        assert_eq!(tail(log, 99), "a\n b\nc\nd");
        assert_eq!(tail("", 5), "");
    }
}
