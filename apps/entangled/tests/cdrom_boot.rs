//! `entangled run --cdrom <iso>`: the generic "boot this ISO" path, end to end
//! through the real CLI (the wave-G cdrom deliverable).
//!
//! A UEFI profile with *no disks at all* plus `--cdrom` must carry the firmware
//! to the ISO's bootloader: EDK2 CloudHv finds the ESP on the read-only
//! virtio-blk the flag attached, loads `\EFI\BOOT\BOOTX64.EFI`, and GRUB draws
//! its menu on ttyS0. That is exactly what `tests/boot/tests/uefi_iso.rs`
//! asserts for the hand-assembled machine — this test asserts the *CLI + config
//! plumbing* delivers the same machine, because the flag re-validates the
//! profile, appends the device after the disks and enforces read-only.
//!
//! `#[ignore]`d: needs a hypervisor (`/dev/kvm` or WHP), the firmware build and
//! a verified Ubuntu ISO
//! (either variant — the test takes the newest in the cache), ~1 minute.
//!
//! ```bash
//! bash guest/firmware/build-cloudhv.sh
//! bash scripts/fetch-ubuntu-iso.sh
//! cargo test -p entangled --test cdrom_boot -- --ignored --nocapture
//! ```

#![cfg(any(target_os = "linux", windows))]

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

mod common;
use common::read_transcript;

/// The CLI under test, as cargo built it.
const BIN: &str = env!("CARGO_BIN_EXE_entangled");

/// Firmware ~3 s, then GPT + FAT reads off a multi-GiB image; a healthy run
/// shows GRUB's menu in ~25 s.
const DEADLINE: Duration = Duration::from_secs(150);

/// GRUB painted its banner on the EFI console (ttyS0 — CloudHv has no GOP).
const GRUB_BANNER: &str = "GNU GRUB  version";

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("repo root above apps/entangled")
        .to_path_buf()
}

/// `/dev/kvm` on Linux, the Windows Hypervisor Platform on Windows: the CLI
/// runs a UEFI guest on either host (EPIC 17 phase 4), so this test does too,
/// and it skips rather than fails where neither is available.
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

/// The newest verified ISO in the fetch script's cache, or
/// `$ENTANGLED_UBUNTU_ISO`.
fn cached_iso() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os("ENTANGLED_UBUNTU_ISO").map(PathBuf::from) {
        return path.is_file().then_some(path);
    }
    // The project-wide cache resolution, so this finds the same ISO
    // `entangled install ubuntu` would on either host.
    let cache = match std::env::var_os("ENTANGLED_CACHE") {
        Some(dir) => PathBuf::from(dir),
        None => debian_media::cache_root()?,
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
    candidates.sort();
    candidates.pop()
}

#[test]
#[ignore = "boots a real ISO: needs KVM, the firmware and a verified Ubuntu ISO"]
fn an_arbitrary_iso_boots_to_its_bootloader_via_cdrom() {
    if !hypervisor_available() {
        return;
    }
    let root = repo_root();
    if !root.join("artifacts/firmware/CLOUDHV.fd").is_file() {
        eprintln!(
            "skipping: no artifacts/firmware/CLOUDHV.fd — run guest/firmware/build-cloudhv.sh"
        );
        return;
    }
    let Some(iso) = cached_iso() else {
        eprintln!("skipping: no verified Ubuntu ISO — run scripts/fetch-ubuntu-iso.sh");
        return;
    };
    eprintln!("booting {} via --cdrom", iso.display());

    // A diskless UEFI profile: the CD-ROM is the whole machine's media. High-RAM
    // sized on purpose — this is also the split's standing regression boot.
    let profile_path = std::env::temp_dir().join(format!("cdrom-boot-{}.toml", std::process::id()));
    std::fs::write(
        &profile_path,
        r#"name = "cdrom-boot-test"
memory_mib = 4096
vcpus = 2
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

    let log = std::env::temp_dir().join(format!("cdrom-boot-{}.log", std::process::id()));
    let file = std::fs::File::create(&log).expect("open the boot log");
    let mut child = Command::new(BIN)
        .args([
            "run",
            "--headless",
            "--cdrom",
            &iso.display().to_string(),
            &profile_path.display().to_string(),
        ])
        .current_dir(&root)
        .stdout(Stdio::from(file.try_clone().expect("clone the log handle")))
        .stderr(Stdio::from(file))
        .spawn()
        .expect("spawn entangled run");

    let started = Instant::now();
    let mut seen = false;
    while started.elapsed() < DEADLINE {
        let text = read_transcript(&log);
        if text.contains(GRUB_BANNER) {
            seen = true;
            break;
        }
        if child.try_wait().expect("wait").is_some() {
            break; // the VM ended on its own — the asserts below will say why
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    let _ = child.kill();
    let _ = child.wait();
    let text = read_transcript(&log);
    let _ = std::fs::remove_file(&log);
    let _ = std::fs::remove_file(&profile_path);

    println!("--- boot chain ---");
    for line in text.lines().filter(|l| {
        [
            "attaching virtio-blk cdrom",
            "PciBus: Discovered",
            "VirtioBlkInit",
            "FSOpen",
            "BdsDxe: starting",
            GRUB_BANNER,
            "ASSERT",
        ]
        .iter()
        .any(|n| l.contains(n))
    }) {
        println!("{}", line.trim());
    }

    // The host attached the flag's ISO read-only as a virtio-blk device.
    assert!(
        text.contains("attaching virtio-blk cdrom (read-only)"),
        "the --cdrom device was never attached; full log:\n{text}"
    );
    // A firmware ASSERT is a missing machine feature, never a timeout.
    let asserts: Vec<&str> = text.lines().filter(|l| l.contains("ASSERT")).collect();
    assert!(asserts.is_empty(), "firmware asserted: {asserts:?}");
    // The firmware opened the ISO's removable-media boot loader and BDS ran it.
    assert!(
        text.contains(r"FSOpen: Open '\EFI\BOOT\BOOTX64.EFI' Success"),
        "the firmware never opened \\EFI\\BOOT\\BOOTX64.EFI on the cdrom; full log:\n{text}"
    );
    assert!(
        text.contains("BdsDxe: starting Boot"),
        "BDS opened BOOTX64.EFI but never transferred control to it"
    );
    // And the loader it chained to is the ISO's GRUB.
    assert!(
        seen && text.contains(GRUB_BANNER),
        "GRUB never printed its banner within {DEADLINE:?}; full log:\n{text}"
    );
}
