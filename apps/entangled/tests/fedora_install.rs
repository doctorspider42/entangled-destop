//! End-to-end Fedora: the Workstation Live image booted interactively through
//! `--cdrom`, and `entangled install fedora --auto` followed by `entangled run`
//! of what it installed.
//!
//! These are the two halves of the Fedora claim, and they are deliberately
//! separate tests because they prove different things.
//!
//! * [`fedora_live_reaches_a_desktop_via_cdrom`] is the *architecture* claim:
//!   a distribution this project has never seen, on stock media, with no guest
//!   additions and no VMM changes, reaches a GNOME desktop drawn on virtio-gpu.
//!   Nothing is typed and nothing is automated — the ISO is attached and the
//!   firmware is left to it. The evidence is a screenshot, because the Live
//!   image's own command line has no `console=` and the serial console goes
//!   quiet the moment GRUB hands over.
//! * [`fedora_installs_unattended_and_the_installed_system_boots`] is the
//!   *installer* claim, and it uses the Everything netinst image instead: only
//!   that one's initramfs carries the anaconda dracut module, so only that one
//!   can find a kickstart at all (see `install_fedora`'s module docs).
//!
//! Both `#[ignore]`d: they need a hypervisor, the firmware build, verified media
//! and — for the install — a network and 30-60 minutes, because a netinst
//! downloads the whole system.
//!
//! ```bash
//! bash guest/firmware/build-cloudhv.sh
//! bash scripts/fetch-fedora-iso.sh              # Workstation Live, ~2.7 GiB
//! bash scripts/fetch-fedora-iso.sh netinst      # Everything netinst, ~1.2 GiB
//! cargo test -p entangled --test fedora_install -- --ignored --nocapture
//! ```

#![cfg(any(target_os = "linux", windows))]

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

mod common;
use common::read_transcript;

/// The CLI under test, as cargo built it.
const BIN: &str = env!("CARGO_BIN_EXE_entangled");

/// How long the Live image gets to reach a desktop. On the development machine
/// (16 threads, KVM in WSL2, 2D scanout, GNOME on llvmpipe) it took ~3.5 minutes
/// from power-on — including the ISO's default menu entry, which is the one that
/// checksums the whole 2.7 GiB medium first.
const LIVE_DEADLINE: Duration = Duration::from_secs(8 * 60);

/// When the Live screenshot is taken. Late enough that the session is up,
/// early enough to be inside the deadline.
const LIVE_SCREENSHOT_AFTER: u64 = 240;

/// A full unattended netinst install: every package comes over the network, so
/// this is dominated by the mirror rather than by the VMM. Deliberately far
/// above the observed time — it must fail rather than hang.
const INSTALL_DEADLINE: Duration = Duration::from_secs(90 * 60);

/// Boot of the installed system: firmware ~3 s, GRUB ~4 s, then systemd bringing
/// up a full Workstation.
const BOOT_DEADLINE: Duration = Duration::from_secs(8 * 60);

/// Target disk size. A Workstation environment is ~7 GiB installed; the file is
/// sparse, so the headroom costs nothing until it is used.
const DISK_SIZE: &str = "24G";

/// The installed system's login prompt. `agetty` prints `<hostname> login:`, and
/// the hostname comes from the kickstart, which the CLI sets from the VM name.
const LOGIN_PROMPT: &str = "login:";

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("repo root above apps/entangled")
        .to_path_buf()
}

/// `/dev/kvm` on Linux, the Windows Hypervisor Platform on Windows; skip rather
/// than fail where neither is available.
#[cfg(target_os = "linux")]
fn hypervisor_available() -> bool {
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

#[cfg(windows)]
fn hypervisor_available() -> bool {
    match vmm_core::whp::WhpHypervisor::probe() {
        Ok(caps) if caps.is_runnable() => true,
        Ok(_) => {
            eprintln!("skipping: {}", vmm_core::whp::WHP_ENABLE_HINT);
            false
        }
        Err(e) => {
            eprintln!("skipping: cannot query the Windows Hypervisor Platform: {e}");
            false
        }
    }
}

fn firmware_present() -> bool {
    if repo_root().join("artifacts/firmware/CLOUDHV.fd").is_file() {
        return true;
    }
    eprintln!("skipping: no artifacts/firmware/CLOUDHV.fd — run guest/firmware/build-cloudhv.sh");
    false
}

/// Scratch directory: never the repository, whose drvfs mount on the development
/// host cannot make sparse files.
fn scratch_dir() -> Option<PathBuf> {
    let dir = match std::env::var_os("ENTANGLED_SCRATCH_DIR") {
        Some(dir) => PathBuf::from(dir),
        None => disk_image::refs::manager_vm_dir()?,
    };
    std::fs::create_dir_all(&dir).ok()?;
    Some(dir)
}

/// A verified Fedora ISO whose file name contains `kind`, from the cache
/// `scripts/fetch-fedora-iso.sh` writes into — the same resolution
/// `entangled install fedora` uses, so a host where the CLI *would* find media
/// does not skip here.
fn cached_iso(kind: &str) -> Option<PathBuf> {
    let cache = match std::env::var_os("ENTANGLED_CACHE") {
        Some(dir) => PathBuf::from(dir),
        None => debian_media::cache_root()?,
    };
    let mut found: Vec<PathBuf> = std::fs::read_dir(cache.join("fedora"))
        .ok()?
        .filter_map(Result::ok)
        .flat_map(|release| {
            std::fs::read_dir(release.path())
                .into_iter()
                .flatten()
                .filter_map(Result::ok)
                .map(|f| f.path())
                .filter(|p| {
                    p.extension().is_some_and(|e| e == "iso")
                        && p.file_name()
                            .and_then(|n| n.to_str())
                            .is_some_and(|n| n.contains(kind))
                })
                .collect::<Vec<_>>()
        })
        .collect();
    found.sort();
    found.pop()
}

/// Runs `entangled` to completion with a deadline, returning its combined
/// output. Killed (and reported) rather than left running on timeout.
fn run_cli(args: &[&str], deadline: Duration) -> (bool, String) {
    let log = std::env::temp_dir().join(format!("entangled-fedora-{}.log", std::process::id()));
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
    let output = read_transcript(&log);
    let _ = std::fs::remove_file(&log);
    (status.is_some_and(|s| s.success()), output)
}

/// Spawns a VM and waits until its output contains `marker` (or `until` says so),
/// then stops it. Returns everything it printed.
fn run_until(
    args: &[&str],
    done: impl Fn(&str) -> bool,
    deadline: Duration,
    tag: &str,
) -> (bool, String) {
    let log =
        std::env::temp_dir().join(format!("entangled-fedora-{tag}-{}.log", std::process::id()));
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
        if done(&read_transcript(&log)) {
            seen = true;
            break;
        }
        if child.try_wait().expect("wait").is_some() {
            break; // the VM ended on its own — the asserts below say why
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    let _ = child.kill();
    let _ = child.wait();
    let output = read_transcript(&log);
    let _ = std::fs::remove_file(&log);
    (seen, output)
}

/// The lines that make a transcript evidence rather than noise.
fn show(label: &str, log: &str, needles: &[&str]) {
    println!("--- {label} ---");
    for line in log
        .lines()
        .filter(|l| needles.iter().any(|n| l.contains(n)))
    {
        println!("{}", line.trim());
    }
}

fn tail(log: &str, lines: usize) -> String {
    let kept: Vec<&str> = log
        .lines()
        .map(str::trim_end)
        .filter(|l| !l.is_empty())
        .collect();
    kept[kept.len().saturating_sub(lines)..].join("\n")
}

// ---------------------------------------------------------------------------
// 1. The Live image, interactively
// ---------------------------------------------------------------------------

#[test]
#[ignore = "boots a real Fedora Live image: needs a hypervisor, the firmware, a verified ISO and ~5 minutes"]
fn fedora_live_reaches_a_desktop_via_cdrom() {
    if !hypervisor_available() || !firmware_present() {
        return;
    }
    let Some(iso) = cached_iso("Live") else {
        eprintln!("skipping: no verified Fedora Live ISO — run scripts/fetch-fedora-iso.sh");
        return;
    };
    let Some(scratch) = scratch_dir() else {
        eprintln!("skipping: no scratch directory");
        return;
    };
    eprintln!("booting {} via --cdrom", iso.display());

    // A diskless UEFI profile: the ISO is the whole machine's media, and this
    // test changes nothing else about the machine — that is the claim.
    let profile = scratch.join("e2e-fedora-live.toml");
    let shot = scratch.join("e2e-fedora-live.png");
    let _ = std::fs::remove_file(&shot);
    std::fs::write(
        &profile,
        r#"name = "e2e-fedora-live"
memory_mib = 4096
vcpus = 4
transport = "pci"

[boot]
mode = "uefi"
firmware = "artifacts/firmware/CLOUDHV.fd"

[display]
width = 1280
height = 800
"#,
    )
    .expect("write the test profile");

    let after = LIVE_SCREENSHOT_AFTER.to_string();
    let (_, log) = run_until(
        &[
            "run",
            "--headless",
            "--cdrom",
            iso.to_str().expect("utf-8 path"),
            "--screenshot-after",
            &after,
            "--screenshot",
            shot.to_str().expect("utf-8 path"),
            profile.to_str().expect("utf-8 path"),
        ],
        |_| shot.is_file(),
        LIVE_DEADLINE,
        "live",
    );
    show(
        "fedora live boot",
        &log,
        &[
            "attaching virtio-blk cdrom",
            "BdsDxe: starting Boot",
            "FSOpen: Open '\\EFI\\BOOT\\",
            "virtio-gpu ready",
            "virtio-gpu scanout set",
            "ASSERT",
        ],
    );

    // A firmware ASSERT is a missing machine feature, never a timeout.
    let asserts: Vec<&str> = log.lines().filter(|l| l.contains("ASSERT")).collect();
    assert!(asserts.is_empty(), "firmware asserted: {asserts:?}");
    assert!(
        log.contains(r"FSOpen: Open '\EFI\BOOT\BOOTX64.EFI' Success"),
        "the firmware never opened the ISO's removable-media loader:\n{}",
        tail(&log, 30)
    );
    // The *guest's* graphics stack drove our device — not the firmware, which
    // has no GOP on CloudHv and never touches virtio-gpu.
    assert!(
        log.contains("virtio-gpu scanout set"),
        "the guest never set a scanout, so nothing was ever drawn:\n{}",
        tail(&log, 30)
    );
    assert!(
        shot.is_file(),
        "no screenshot at {} within {LIVE_DEADLINE:?}; last lines:\n{}",
        shot.display(),
        tail(&log, 30)
    );

    // And what was drawn is a desktop, not a console or a splash. A GNOME
    // session fills 1280x800 with thousands of distinct colours (wallpaper
    // gradient, antialiased text, icons); a text console or a blank scanout has
    // a handful.
    let colours = distinct_colours(&shot);
    println!(
        "screenshot {} has {colours} distinct colours",
        shot.display()
    );
    assert!(
        colours > 2000,
        "the scanout at {LIVE_SCREENSHOT_AFTER}s has only {colours} distinct colours, \
         which is a text console or a splash screen rather than a desktop — see {}",
        shot.display()
    );
}

/// How many distinct RGB values a PNG contains, capped so a full-colour image
/// does not build a huge set.
fn distinct_colours(path: &Path) -> usize {
    let file = std::fs::File::open(path).expect("open the screenshot");
    let decoder = png::Decoder::new(std::io::BufReader::new(file));
    let mut reader = decoder.read_info().expect("a readable PNG");
    let mut buffer = vec![0u8; reader.output_buffer_size().expect("a bounded PNG")];
    let info = reader.next_frame(&mut buffer).expect("the first frame");
    let bytes_per_pixel = (info.color_type.samples() * usize::from(info.bit_depth as u8)) / 8;
    assert!(bytes_per_pixel >= 3, "expected an RGB(A) screenshot");
    let mut seen = std::collections::HashSet::new();
    for pixel in buffer[..info.buffer_size()].chunks_exact(bytes_per_pixel) {
        seen.insert([pixel[0], pixel[1], pixel[2]]);
        if seen.len() > 100_000 {
            break;
        }
    }
    seen.len()
}

// ---------------------------------------------------------------------------
// 2. The unattended install, and booting what it produced
// ---------------------------------------------------------------------------

#[test]
#[ignore = "installs a real Fedora Workstation: needs a hypervisor, the firmware, a verified netinst ISO, a network and 30-60 minutes"]
fn fedora_installs_unattended_and_the_installed_system_boots() {
    if !hypervisor_available() || !firmware_present() {
        return;
    }
    if cached_iso("netinst").is_none() {
        eprintln!(
            "skipping: no verified Fedora netinst ISO — run \
             `bash scripts/fetch-fedora-iso.sh netinst`"
        );
        return;
    }
    let Some(scratch) = scratch_dir() else {
        eprintln!("skipping: no scratch directory");
        return;
    };

    // A fresh disk, NVRAM, kickstart volume and profile every run: this test is
    // about what an install *produces*, so it must not inherit any of it.
    let disk = scratch.join("e2e-fedora.raw");
    let profile = scratch.join("e2e-fedora.toml");
    let nvram = scratch.join("e2e-fedora.nvram");
    let ks = scratch.join("e2e-fedora-ks.iso");
    let transcript = scratch.join("e2e-fedora-install.log");
    for path in [&disk, &profile, &nvram, &ks, &transcript] {
        let _ = std::fs::remove_file(path);
    }

    // ---- the install ----
    let started = Instant::now();
    let (ok, host_log) = run_cli(
        &[
            "install",
            "fedora",
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
            "Fedora installer media",
            "installer configuration volume",
            "GRUB menu is up",
            "typing into GRUB",
            "granted the guest a DHCP lease",
            "ACPI S5",
            "installation detected",
            "installed:",
            "error",
        ],
    );
    assert!(
        ok,
        "`entangled install fedora` failed; last lines:\n{}",
        tail(&host_log, 30)
    );

    // The transcript is Anaconda's own account of what happened.
    assert!(
        transcript.is_file(),
        "no install transcript at {}",
        transcript.display()
    );
    let installer_log = read_transcript(&transcript);
    for marker in [
        // GRUB took the typed command line, with all three load-bearing clauses.
        "inst.stage2=hd:LABEL=Fedora",
        "inst.ks=hd:LABEL=OEMDRV:/ks.cfg",
        "console=ttyS0,115200n8",
        // Anaconda read the kickstart and knew it was automated.
        "automated install",
        // ...and ended in an orderly power off, the host's completion signal.
        "Power down",
    ] {
        assert!(
            installer_log.contains(marker),
            "the installer transcript never mentions {marker:?} — see {}",
            transcript.display()
        );
    }
    // The loud failure mode this delivery mechanism exists to produce.
    assert!(
        !installer_log.contains("Can't get kickstart from"),
        "the initramfs could not read the OEMDRV volume — see {}",
        transcript.display()
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
        "the installed profile carries the target disk only — no ISO, no kickstart"
    );
    assert_eq!(cfg.disks[0].path, disk);
    // An installed desktop gets a sound card (GAME-2102).
    assert!(cfg.sound.enabled, "the installed profile has no sound card");
    assert_eq!(cfg.sound.backend, control_api::SoundBackend::Auto);
    assert_eq!(
        std::fs::metadata(&nvram).expect("the NVRAM file").len(),
        machine_x86::layout::PFLASH_NVRAM_SIZE
    );

    // ---- and now boot what was installed ----
    let (saw_login, boot_log) = run_until(
        &["run", "--headless", profile.to_str().expect("utf-8 path")],
        |text| text.contains(LOGIN_PROMPT),
        BOOT_DEADLINE,
        "boot",
    );
    show(
        "installed system boot",
        &boot_log,
        &[
            "BdsDxe: starting Boot",
            "shimx64.efi",
            "GNU GRUB",
            "Fedora Linux",
            "serial-getty",
            LOGIN_PROMPT,
        ],
    );

    // The chain, in order, each line proving the step before it could not have
    // been skipped.
    assert!(
        boot_log.contains(r"\EFI\fedora\shimx64.efi")
            || boot_log.contains(r"\EFI\fedora\grubx64.efi"),
        "the firmware did not load the installed system's bootloader. With no \
         Boot#### entry it would fall back to removable media, so this is the \
         assertion that the persisted NVRAM entry is what booted:\n{}",
        tail(&boot_log, 30)
    );
    assert!(
        boot_log.contains("GNU GRUB"),
        "the bootloader started but GRUB never printed its banner"
    );
    assert!(
        boot_log.contains("Fedora Linux"),
        "GRUB ran but the installed kernel never reached systemd's greeting"
    );
    assert!(
        boot_log.contains("serial-getty@ttyS0"),
        "the installed system booted but no getty was started on ttyS0 — the \
         kickstart's %post console configuration did not take"
    );
    assert!(
        saw_login,
        "no login prompt within {BOOT_DEADLINE:?}; last lines:\n{}",
        tail(&boot_log, 30)
    );
}
