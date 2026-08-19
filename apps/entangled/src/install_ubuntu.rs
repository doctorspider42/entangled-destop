//! `entangled install ubuntu` — an unattended Ubuntu Server installation
//! (backlog UEFI-1804, [ADR-0003](../../../docs/adr/0003-uefi-firmware.md)).
//!
//! # The machine
//!
//! ```text
//!   firmware   artifacts/firmware/CLOUDHV.fd, entered through PVH
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

use std::path::{Path, PathBuf};

use control_api::{BootMode, BootSection, DiskSection, DisplaySection, VirtioTransport, VmConfig};

use crate::disk;
use crate::diskfs;
use crate::run_vm::{self, Automation};
use crate::seed;
use crate::InstallArgs;

/// The firmware built by `guest/firmware/build-cloudhv.sh`.
const FIRMWARE: &str = "artifacts/firmware/CLOUDHV.fd";

/// Installer VM size. subiquity wants ~2 GiB; 2560 is the generous choice
/// `examples/ubuntu-uefi.toml` documents. (Guests above 3072 MiB are legal
/// since the high-RAM split, but the text installer gains nothing from more.)
const INSTALLER_MEMORY_MIB: u64 = 2560;

/// What the installed system gets when `--memory-mib` was left at its default.
/// Less than the installer needs: nothing is unpacking a 1.2 GiB squashfs any
/// more. An explicit `--memory-mib` above this carries through to the written
/// profile — a desktop install sized at 4096 must not boot into 2048.
const INSTALLED_MEMORY_MIB: u64 = 2048;

pub fn run(args: &InstallArgs) -> Result<(), String> {
    // 1. Firmware. Named first because it is the one artifact a fresh checkout
    //    does not have, and the error has to say how to get it.
    let firmware = args
        .firmware
        .clone()
        .unwrap_or_else(|| PathBuf::from(FIRMWARE));
    if !firmware.exists() {
        return Err(format!(
            "firmware {} not found — build it with `bash guest/firmware/build-cloudhv.sh` \
             (~2.5 min). An Ubuntu install needs UEFI: subiquity only creates an EFI \
             System Partition when the installer itself booted under firmware",
            firmware.display()
        ));
    }

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

    // 3. Target disk.
    let vm_name = args
        .name
        .clone()
        .unwrap_or_else(|| stem_of(&args.disk, "ubuntu"));
    if !args.disk.exists() {
        let bytes = disk::parse_size(&args.size).map_err(|e| e.to_string())?;
        disk::create_raw(&args.disk, bytes).map_err(|e| e.to_string())?;
        tracing::info!(disk = %args.disk.display(), bytes, "created target disk");
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
    let seed = if automated {
        let user_data = seed::user_data(&vm_name, args.autoinstall.as_deref())
            .map_err(|e| format!("cannot build the autoinstall configuration: {e}"))?;
        let seed_path = args.disk.with_file_name(format!("{vm_name}-seed.iso"));
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
    let nvram = args.disk.with_file_name(format!("{vm_name}.nvram"));
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
                path: args.disk.clone(),
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
        network: None,
        display: DisplaySection {
            width: 1280,
            height: 800,
            scale: 1.0,
        },
    };

    let transcript = args.disk.with_file_name(format!("{vm_name}-install.log"));
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
        args.headless,
        Some(Automation {
            script: Box::new(move |log| script.step(log)),
            transcript: Some(transcript.clone()),
        }),
        None,
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
    for marker in INSTALL_MARKERS {
        if !log.contains(marker) {
            tracing::warn!(marker, "the installer transcript is missing a usual marker");
        }
    }

    // 8. What is actually on the disk (UEFI-1804: GPT, an ESP, a root).
    let install = diskfs::find_uefi_install(&args.disk).map_err(|e| {
        format!(
            "the installer powered off but {} does not look installed: {e}\n\
             Last lines of {}:\n{}",
            args.disk.display(),
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
            path: args.disk.clone(),
            writable: true,
        }],
        cdrom: None,
        network: None,
        display: DisplaySection {
            width: 1280,
            height: 800,
            scale: 1.0,
        },
    };
    let profile_path = args.disk.with_file_name(format!("{vm_name}.toml"));
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
            .unwrap_or_default(),
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
    // GRUB accepted the typed boot command.
    "autoinstall",
    // cloud-init found the seed volume.
    "cloud-init",
    // curtin ran.
    "curtin",
];

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
fn cached_iso() -> Option<PathBuf> {
    let cache = match std::env::var_os("ENTANGLED_CACHE") {
        Some(dir) => PathBuf::from(dir),
        None => PathBuf::from(std::env::var_os("HOME")?).join(".cache/entangled"),
    };
    let mut candidates: Vec<PathBuf> = std::fs::read_dir(cache.join("ubuntu"))
        .ok()?
        .filter_map(Result::ok)
        .flat_map(|release| {
            std::fs::read_dir(release.path())
                .into_iter()
                .flatten()
                .filter_map(Result::ok)
                .map(|f| f.path())
                .filter(|p| p.extension().is_some_and(|e| e == "iso"))
                .collect::<Vec<_>>()
        })
        .collect();
    // Sorted, so a machine holding several releases picks the newest name.
    candidates.sort();
    candidates.pop()
}

fn stem_of(disk: &Path, fallback: &str) -> String {
    disk.file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| fallback.to_string())
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

    #[test]
    fn tail_keeps_the_last_lines_and_drops_blanks() {
        let log = "a\n\n b \n\nc\nd\n";
        assert_eq!(tail(log, 2), "c\nd");
        assert_eq!(tail(log, 99), "a\n b\nc\nd");
        assert_eq!(tail("", 5), "");
    }
}
