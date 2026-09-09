//! End-to-end: `entangled install debian --auto` and then `entangled run` of
//! what it installed (backlog EPIC 10, MVP-1008/1009/1010).
//!
//! The Debian acceptance criterion as a test, and the third of its shape after
//! `ubuntu_install` and `fedora_install`. What only this one covers:
//!
//! * that the **bootstrap kernel** is resolved from wherever this host keeps it
//!   — a checkout that built it, `ENTANGLED_BOOTSTRAP_DIR`, or the verified
//!   cache `entangled fetch bootstrap-kernel` writes into. That resolution is
//!   the whole reason `install debian` works on Windows at all: the kernel is a
//!   Linux kernel build with no cross-compile, so a Windows host can only ever
//!   download it;
//! * that the preseed reaches d-i through the **cpio appended to the installer
//!   initrd**, which is a different automation mechanism from subiquity's seed
//!   volume and Anaconda's kickstart;
//! * that the install ends in an **ACPI power-off** rather than d-i's default
//!   reboot — the only ending both hypervisors report as a stop (a `reboot=k`
//!   triple fault is absorbed by WHP's local APIC and hangs the installer VM
//!   forever after a perfectly good install);
//! * that the profile it writes names kernel and initramfs paths that **exist
//!   from wherever the profile is read**, which a relative
//!   `artifacts/bootstrap/...` does not on a host that downloaded them.
//!
//! Runs on either host: KVM on Linux, WHP on Windows. It self-skips when the
//! hypervisor or the bootstrap artifacts are missing, like every other test of
//! this kind here.
//!
//! `#[ignore]`d: it needs a hypervisor, the two guest artifacts, **a working
//! network** (the netboot installer downloads the entire system from a Debian
//! mirror — there is no offline Debian install) and 15-45 minutes.
//!
//! ```bash
//! entangled fetch bootstrap-kernel      # or guest/bootstrap-kernel/build.sh
//! cargo test -p entangled --test debian_install -- --ignored --nocapture
//! ```
//!
//! ```powershell
//! # on Windows, where `entangled fetch bootstrap-kernel` is the only way in
//! cargo test -p entangled --test debian_install -- --ignored --nocapture
//! ```

#![cfg(any(target_os = "linux", windows))]

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

mod common;
use common::read_transcript;

const BIN: &str = env!("CARGO_BIN_EXE_entangled");

/// A full unattended d-i install measured **6 min 34 s** and **7 min 36 s** on
/// two runs on the development machine (Windows/WHP, 1536 MiB, usernet NAT,
/// deb.debian.org over a domestic line). The minute between them is the mirror,
/// not the VMM, which is why the deadline is far above both: this must fail
/// rather than hang, but it must not fail because somebody's link is slow.
const INSTALL_DEADLINE: Duration = Duration::from_secs(75 * 60);

/// Boot of the installed system: no firmware and no bootloader on this path —
/// the bootstrap kernel is started directly and its initramfs `switch_root`s
/// into `/dev/vda1` — so this is systemd's own start-up and nothing else. It
/// took **7.3 s** to the login prompt on the development machine, which is why
/// six minutes is generous rather than tight. If it ever approaches the
/// deadline, something is wrong with the disk, not with the deadline.
const BOOT_DEADLINE: Duration = Duration::from_secs(6 * 60);

/// `agetty` prints `<hostname> login:`, and the hostname is the VM name, which
/// `install debian` preseeds through `netcfg/get_hostname`.
const LOGIN_PROMPT: &str = "login:";

/// Enough for a Debian base system plus the automated profile's packages, and
/// small enough that the sparse file stays modest.
const DISK_SIZE: &str = "12G";

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("repo root above apps/entangled")
        .to_path_buf()
}

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

/// Whether a bootstrap kernel + initramfs pair is reachable, the same three
/// places `entangled` looks (`apps/entangled/src/bootstrap.rs`) and in the same
/// order. Duplicated rather than imported because `entangled` is a binary crate
/// with no library face; the CLI is the authority and this only decides whether
/// to skip.
fn bootstrap_artifacts() -> Option<PathBuf> {
    let pair = |dir: PathBuf| {
        (dir.join("vmlinuz").is_file() && dir.join("initrd.img").is_file()).then_some(dir)
    };
    if let Some(dir) = std::env::var_os("ENTANGLED_BOOTSTRAP_DIR").map(PathBuf::from) {
        if let Some(found) = pair(dir) {
            return Some(found);
        }
    }
    if let Some(found) = pair(repo_root().join("artifacts/bootstrap")) {
        return Some(found);
    }
    let cache = match std::env::var_os("ENTANGLED_CACHE") {
        Some(dir) => PathBuf::from(dir),
        None => debian_media::cache_root()?,
    };
    std::fs::read_dir(cache.join("bootstrap"))
        .ok()?
        .filter_map(Result::ok)
        .find_map(|entry| pair(entry.path()))
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

/// Runs `entangled` to completion with a deadline, returning its combined
/// output. Killed (and reported) rather than left running on timeout.
fn run_cli(args: &[&str], deadline: Duration) -> (bool, String) {
    let log = std::env::temp_dir().join(format!("entangled-debian-{}.log", std::process::id()));
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

/// Spawns a VM and waits until its output contains `marker`, then stops it.
fn run_until(args: &[&str], marker: &str, deadline: Duration) -> (bool, String) {
    let log =
        std::env::temp_dir().join(format!("entangled-debian-boot-{}.log", std::process::id()));
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
        if read_transcript(&log).contains(marker) {
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
    let output = read_transcript(&log);
    let _ = std::fs::remove_file(&log);
    (seen, output)
}

fn tail(log: &str, lines: usize) -> String {
    let all: Vec<&str> = log.lines().collect();
    all[all.len().saturating_sub(lines)..].join("\n")
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

#[test]
#[ignore = "installs a real Debian: needs a hypervisor, the bootstrap artifacts, a mirror and ~25 minutes"]
fn debian_installs_unattended_and_the_installed_system_boots() {
    if !hypervisor_available() {
        return;
    }
    let Some(artifacts) = bootstrap_artifacts() else {
        eprintln!(
            "skipping: no bootstrap kernel + initramfs — run `entangled fetch \
             bootstrap-kernel` (either host) or `bash guest/bootstrap-kernel/build.sh` \
             followed by `bash scripts/build-bootstrap-initramfs.sh` (Linux)"
        );
        return;
    };
    println!("bootstrap artifacts: {}", artifacts.display());
    let Some(scratch) = scratch_dir() else {
        eprintln!("skipping: no scratch directory");
        return;
    };

    // A fresh disk, profile and derived initrd every run: this test is about
    // what an install *produces*, so it must inherit none of it.
    let disk = scratch.join("e2e-debian.raw");
    let profile = scratch.join("e2e-debian.toml");
    let derived = scratch.join("e2e-debian.install-initrd.img");
    for path in [&disk, &profile, &derived] {
        let _ = std::fs::remove_file(path);
    }

    // ---- the install ----
    // `--network` is left to the per-host default: TAP on Linux, the in-process
    // user-mode NAT on Windows. Pinning it here would be wrong on one host, and
    // on Windows would name a backend that does not exist.
    let started = Instant::now();
    let (ok, host_log) = run_cli(
        &[
            "install",
            "debian",
            "--disk",
            disk.to_str().expect("utf-8 path"),
            "--size",
            DISK_SIZE,
            "--name",
            "e2e-debian",
            "--auto",
            "--headless",
        ],
        INSTALL_DEADLINE,
    );
    let install_took = started.elapsed();
    println!("install took {install_took:?}");
    show(
        "install",
        &host_log,
        &[
            "bootstrap artifacts resolved",
            "installer media ready",
            "created target disk",
            "ACPI S5",
            "installation detected",
            "installed:",
            "error",
        ],
    );
    assert!(
        ok,
        "`entangled install debian` failed after {install_took:?}; last lines:\n{}",
        tail(&host_log, 40)
    );

    // The installer's own account, on the serial console the host captured.
    for marker in [
        // d-i got through netcfg with the static address the CLI preseeded, and
        // reached a mirror: no mirror, no Debian.
        "Installing the base system",
        // …and finished writing one.
        "Finishing the installation",
        // The ending that matters. d-i's default is a reboot, which WHP absorbs
        // into a parked vCPU; the preseeded ACPI power-off is the only ending
        // both hypervisors report as a stop.
        "Power down",
    ] {
        assert!(
            host_log.contains(marker),
            "the installer transcript never mentions {marker:?}; last lines:\n{}",
            tail(&host_log, 40)
        );
    }

    // ---- what the install produced ----
    let text = std::fs::read_to_string(&profile).expect("the generated profile");
    println!("--- profile ---\n{text}");
    let cfg = control_api::VmConfig::from_toml(&text).expect("a valid generated profile");
    assert_eq!(cfg.boot.mode, control_api::BootMode::DirectLinux);
    assert_eq!(cfg.disks.len(), 1);
    assert_eq!(cfg.disks[0].path, disk);
    assert!(
        cfg.boot.cmdline.contains("root=UUID="),
        "the installed root must be named by UUID, not by device order: {:?}",
        cfg.boot.cmdline
    );

    // The two boot files the profile names must exist *as the profile spells
    // them*. A checkout that built its own keeps the historical relative path;
    // a host that downloaded them gets absolute paths, and a relative one there
    // would name nothing at all — which is the bug this assertion exists for.
    let root = repo_root();
    for (label, path) in [
        ("kernel", cfg.boot.kernel.as_deref()),
        ("initramfs", cfg.boot.initramfs.as_deref()),
    ] {
        let path = path.unwrap_or_else(|| panic!("the profile names no {label}"));
        let resolved = if path.is_absolute() {
            path.to_path_buf()
        } else {
            root.join(path)
        };
        assert!(
            resolved.is_file(),
            "the profile's {label} {} does not exist (resolved to {})",
            path.display(),
            resolved.display()
        );
    }

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
            "switch_root",
            "Welcome to",
            "Reached target",
            "getty",
            LOGIN_PROMPT,
        ],
    );
    assert!(
        saw_login,
        "no login prompt within {BOOT_DEADLINE:?}; last lines:\n{}",
        tail(&boot_log, 40)
    );
    assert!(
        !boot_log.contains("Kernel panic"),
        "the installed system panicked:\n{}",
        tail(&boot_log, 40)
    );
}
