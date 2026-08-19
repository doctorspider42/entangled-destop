//! End-to-end: `entangled install ubuntu` and then `entangled run` of what it
//! installed (backlog UEFI-1804, ADR-0003 phase 4).
//!
//! This is the acceptance criterion as a test. It drives the real CLI, so it
//! covers the parts no unit test can: that the typed GRUB command line actually
//! reaches subiquity, that the installer finds the seed volume, that an ACPI
//! poweroff is how the host learns it finished, that the disk really ends up
//! with a GPT and an ESP, and — the reason the pflash device exists — that the
//! `Boot####` entry `grub-install` wrote into NVRAM survives the VM stopping and
//! boots the *installed* system afterwards.
//!
//! `#[ignore]`d: it needs `/dev/kvm`, a 4 MiB firmware build, a 2.9 GiB verified
//! ISO, ~12 GiB of scratch disk and 10-40 minutes.
//!
//! ```bash
//! bash guest/firmware/build-cloudhv.sh
//! bash scripts/fetch-ubuntu-iso.sh
//! cargo test -p entangled --test ubuntu_install -- --ignored --nocapture
//! ```

#![cfg(target_os = "linux")]

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

/// The CLI under test, as cargo built it.
const BIN: &str = env!("CARGO_BIN_EXE_entangled");

/// A full unattended install took ~7 minutes on the development machine (16
/// threads, KVM in WSL2). The deadline is deliberately far above that: a
/// mirror-less apt still has timeouts, and this must fail rather than hang.
const INSTALL_DEADLINE: Duration = Duration::from_secs(40 * 60);

/// Boot of the installed system: firmware ~3 s, GRUB ~4 s, systemd + cloud-init
/// (first boot generates SSH host keys) ~2-3 min.
const BOOT_DEADLINE: Duration = Duration::from_secs(6 * 60);

/// Target disk size. Enough for `ubuntu-server-minimal` with room to spare, and
/// small enough that the sparse file stays modest.
const DISK_SIZE: &str = "12G";

fn repo_root() -> PathBuf {
    // apps/entangled -> repo root
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("repo root above apps/entangled")
        .to_path_buf()
}

fn kvm_available() -> bool {
    match std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/kvm")
    {
        Ok(_) => true,
        Err(e) => {
            eprintln!("skipping: /dev/kvm is not usable: {e}");
            false
        }
    }
}

/// Scratch directory: never the repository, whose drvfs mount on the
/// development host cannot make sparse files.
fn scratch_dir() -> Option<PathBuf> {
    let dir = match std::env::var_os("ENTANGLED_SCRATCH_DIR") {
        Some(dir) => PathBuf::from(dir),
        None => PathBuf::from(std::env::var_os("HOME")?).join("entangled-vms"),
    };
    std::fs::create_dir_all(&dir).ok()?;
    Some(dir)
}

fn cached_iso_exists() -> bool {
    let cache = match std::env::var_os("ENTANGLED_CACHE") {
        Some(dir) => PathBuf::from(dir),
        None => match std::env::var_os("HOME") {
            Some(home) => PathBuf::from(home).join(".cache/entangled"),
            None => return false,
        },
    };
    std::fs::read_dir(cache.join("ubuntu"))
        .map(|entries| {
            entries.filter_map(Result::ok).any(|release| {
                std::fs::read_dir(release.path())
                    .into_iter()
                    .flatten()
                    .filter_map(Result::ok)
                    .any(|f| f.path().extension().is_some_and(|e| e == "iso"))
            })
        })
        .unwrap_or(false)
}

/// Runs `entangled` to completion with a deadline, returning its combined
/// output. Killed (and reported) rather than left running on timeout.
fn run_cli(args: &[&str], deadline: Duration) -> (bool, String) {
    let log = std::env::temp_dir().join(format!("entangled-e2e-{}.log", std::process::id()));
    let file = std::fs::File::create(&log).expect("open the CLI log");
    let mut child = Command::new(BIN)
        .args(args)
        .current_dir(repo_root())
        .stdout(Stdio::from(file.try_clone().expect("clone the log handle")))
        .stderr(Stdio::from(file))
        .spawn()
        .expect("spawn entangled");

    let started = Instant::now();
    let status = loop {
        match child.try_wait().expect("wait for entangled") {
            Some(status) => break Some(status),
            None if started.elapsed() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                break None;
            }
            None => std::thread::sleep(Duration::from_millis(500)),
        }
    };
    let output = std::fs::read_to_string(&log).unwrap_or_default();
    let _ = std::fs::remove_file(&log);
    (status.is_some_and(|s| s.success()), output)
}

/// Spawns a VM and waits until its output contains `marker`, then stops it.
/// Returns everything it printed.
fn run_until(args: &[&str], marker: &str, deadline: Duration) -> (bool, String) {
    let log = std::env::temp_dir().join(format!("entangled-e2e-boot-{}.log", std::process::id()));
    let file = std::fs::File::create(&log).expect("open the boot log");
    let mut child: Child = Command::new(BIN)
        .args(args)
        .current_dir(repo_root())
        .stdout(Stdio::from(file.try_clone().expect("clone the log handle")))
        .stderr(Stdio::from(file))
        .spawn()
        .expect("spawn entangled run");

    let started = Instant::now();
    let mut seen = false;
    while started.elapsed() < deadline {
        let text = std::fs::read_to_string(&log).unwrap_or_default();
        if text.contains(marker) {
            seen = true;
            break;
        }
        if child.try_wait().expect("wait").is_some() {
            break; // the VM ended on its own
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    let _ = child.kill();
    let _ = child.wait();
    let output = std::fs::read_to_string(&log).unwrap_or_default();
    let _ = std::fs::remove_file(&log);
    (seen, output)
}

/// The lines that make the transcript evidence rather than noise.
fn show(label: &str, log: &str, needles: &[&str]) {
    println!("--- {label} ---");
    for line in log
        .lines()
        .filter(|l| needles.iter().any(|n| l.contains(n)))
    {
        println!("{}", line.trim());
    }
}

#[test]
#[ignore = "installs a real Ubuntu: needs KVM, the firmware, a verified ISO and ~10 minutes"]
fn ubuntu_installs_unattended_and_the_installed_system_boots() {
    if !kvm_available() {
        return;
    }
    let root = repo_root();
    if !root.join("artifacts/firmware/CLOUDHV.fd").is_file() {
        eprintln!(
            "skipping: no artifacts/firmware/CLOUDHV.fd — run guest/firmware/build-cloudhv.sh"
        );
        return;
    }
    if !cached_iso_exists() {
        eprintln!("skipping: no verified Ubuntu ISO — run scripts/fetch-ubuntu-iso.sh");
        return;
    }
    let Some(scratch) = scratch_dir() else {
        eprintln!("skipping: no scratch directory");
        return;
    };

    // A fresh disk, NVRAM and profile every run: this test is about what an
    // install *produces*, so it must not inherit any of it.
    let disk = scratch.join("e2e-ubuntu.raw");
    let profile = scratch.join("e2e-ubuntu.toml");
    let nvram = scratch.join("e2e-ubuntu.nvram");
    let seed = scratch.join("e2e-ubuntu-seed.iso");
    let transcript = scratch.join("e2e-ubuntu-install.log");
    for path in [&disk, &profile, &nvram, &seed, &transcript] {
        let _ = std::fs::remove_file(path);
    }

    // ---- the install ----
    let started = Instant::now();
    let (ok, host_log) = run_cli(
        &[
            "install",
            "ubuntu",
            "--disk",
            disk.to_str().expect("utf-8 path"),
            "--size",
            DISK_SIZE,
            "--auto",
            "--headless",
        ],
        INSTALL_DEADLINE,
    );
    println!("install took {:?}", started.elapsed());
    show(
        "install",
        &host_log,
        &[
            "wrote the cloud-init NoCloud seed",
            "UEFI variable store ready",
            "GRUB menu is up",
            "typing into GRUB",
            "ACPI S5",
            "installation detected",
            "variable store written",
            "installed:",
            "error",
        ],
    );
    assert!(
        ok,
        "`entangled install ubuntu` failed; last lines:\n{}",
        tail(&host_log, 30)
    );

    // The transcript is the installer's own account of what happened.
    let installer_log = std::fs::read_to_string(&transcript).expect("the install transcript");
    for marker in [
        // GRUB took the typed command line, with both of its load-bearing words.
        "autoinstall console=ttyS0",
        // cloud-init found the seed and subiquity read it.
        "subiquity/load_autoinstall_config",
        // curtin partitioned and installed.
        "cmd-install",
        // and the installed system's command line was set by late-commands.
        "GRUB_CMDLINE_LINUX_DEFAULT",
        // ending in an orderly power off, which is the host's completion signal.
        "reboot: Power down",
    ] {
        assert!(
            installer_log.contains(marker),
            "the installer transcript never mentions {marker:?} — see {}",
            transcript.display()
        );
    }
    // Nothing may have stopped for a human.
    assert!(
        !installer_log.contains("Continue with autoinstall?"),
        "the installer asked for confirmation, so `autoinstall` did not reach \
         /proc/cmdline — the GRUB typing is the mechanism at fault"
    );

    // ---- what the install produced ----
    let text = std::fs::read_to_string(&profile).expect("the generated profile");
    println!("--- profile ---\n{text}");
    let cfg = control_api::VmConfig::from_toml(&text).expect("a valid generated profile");
    assert_eq!(cfg.boot.mode, control_api::BootMode::Uefi);
    assert!(cfg.transport.is_pci(), "a UEFI guest needs virtio-pci");
    assert_eq!(
        cfg.boot.nvram.as_deref(),
        Some(nvram.as_path()),
        "the profile must name the NVRAM store, or the installed system's boot \
         entry is gone on the next start"
    );
    assert_eq!(
        cfg.disks.len(),
        1,
        "the installed profile carries the target disk only — no ISO, no seed"
    );
    assert_eq!(cfg.disks[0].path, disk);
    assert_eq!(
        std::fs::metadata(&nvram).expect("the NVRAM file").len(),
        machine_x86::layout::PFLASH_NVRAM_SIZE
    );
    // A UEFI boot entry for the installed system, in the file, named the way
    // grub-install names it.
    let store = std::fs::read(&nvram).expect("read the NVRAM file");
    assert!(
        utf16_names(&store)
            .iter()
            .any(|n| n == "Boot0006" || n.starts_with("Boot00")),
        "no Boot#### variable in the NVRAM store"
    );

    // ---- and now boot what was installed ----
    let (saw_login, boot_log) = run_until(
        &["run", "--headless", profile.to_str().expect("utf-8 path")],
        LOGIN_PROMPT,
        BOOT_DEADLINE,
    );
    show(
        "installed system boot",
        &boot_log,
        &[
            "BdsDxe: starting Boot",
            "shimx64.efi",
            "GNU GRUB",
            "Welcome to Ubuntu",
            "serial-getty",
            LOGIN_PROMPT,
        ],
    );

    // The chain, in order, each line proving the step before it could not have
    // been skipped.
    assert!(
        boot_log.contains("\\EFI\\ubuntu\\shimx64.efi"),
        "the firmware did not load the installed system's bootloader. With no \
         Boot#### entry it would fall back to removable media, so this is the \
         assertion that the persisted NVRAM entry is what booted:\n{}",
        tail(&boot_log, 30)
    );
    assert!(
        boot_log.contains("GNU GRUB"),
        "shim started but GRUB never printed its banner"
    );
    assert!(
        boot_log.contains("Welcome to Ubuntu"),
        "GRUB ran but the installed kernel never reached systemd"
    );
    assert!(
        boot_log.contains("serial-getty@ttyS0"),
        "the installed system booted but no getty was started on ttyS0 — the \
         late-commands console= configuration did not take"
    );
    assert!(
        saw_login,
        "no login prompt within {BOOT_DEADLINE:?}; last lines:\n{}",
        tail(&boot_log, 30)
    );
}

/// The installed system's login prompt. `agetty` prints `<hostname> login:`,
/// and the hostname comes from the autoinstall profile's `identity.hostname`,
/// which `entangled install` sets from the VM name.
const LOGIN_PROMPT: &str = "login:";

/// UTF-16LE variable names in a UEFI variable store, the way a human greps for
/// them.
fn utf16_names(store: &[u8]) -> Vec<String> {
    let mut names = Vec::new();
    let mut current = String::new();
    for pair in store.chunks_exact(2) {
        match char::from_u32(u32::from(u16::from_le_bytes([pair[0], pair[1]]))) {
            Some(c) if c.is_ascii_graphic() => current.push(c),
            _ if current.len() >= 4 => names.push(std::mem::take(&mut current)),
            _ => current.clear(),
        }
    }
    names
}

fn tail(log: &str, lines: usize) -> String {
    let kept: Vec<&str> = log
        .lines()
        .map(str::trim_end)
        .filter(|l| !l.is_empty())
        .collect();
    kept[kept.len().saturating_sub(lines)..].join("\n")
}
