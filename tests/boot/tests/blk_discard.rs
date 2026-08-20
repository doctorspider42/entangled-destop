//! Thin-provisioning reclaim, end to end, from inside a real guest
//! (`VIRTIO_BLK_F_DISCARD`).
//!
//! The unit and transport tests in `virtio-block` prove the device answers a
//! discard correctly and that the host punches the hole. What they cannot prove
//! is the thing the feature exists for: that a **guest kernel**, told about the
//! feature through config space, decides to use it, and that the host image
//! shrinks as a result. That chain has five links and every one has to agree —
//! the device's feature bits, the config fields the guest turns into queue
//! limits, the guest's decision to trim, the request's segment array, and
//! `fallocate(PUNCH_HOLE)` on the image.
//!
//! So this test boots the same guest **twice over the same image**:
//!
//! 1. with `ENTANGLED_BLK_DISCARD=off`, which is the world before this feature:
//!    the guest fills a few hundred MiB, frees them, asks for the space back and
//!    is refused, and the host image stays fat;
//! 2. with reclaim on: the same fill, the same `fstrim`, and the image goes back
//!    to roughly its empty size.
//!
//! The two allocated-size numbers either side are the whole point, and they are
//! printed so a reader of the test log gets the same before/after the report
//! quotes.
//!
//! Self-skips without `/dev/kvm`, without the guest artifacts, or where the
//! filesystem holding the image cannot do sparse files at all (drvfs, i.e.
//! `/mnt/*`) — there the reclaim is a legal no-op and the assertion would be
//! about the host's filesystem rather than about us. Keep the image on a
//! WSL-native path; `~/entangled-vms` is the default.

#![cfg(target_os = "linux")]

use std::path::{Path, PathBuf};

use boot_tests::{boot_once, BootOutcome, BootSpec};
use disk_image::{allocated_bytes, create_raw, format_bytes};

/// Image size: room for a filesystem plus the fill, small enough to write fast.
const IMAGE_BYTES: u64 = 1 << 30;
/// How much the guest fills and then frees, in MiB.
const FILL_MIB: u64 = 512;
/// The reclaim we insist on seeing. Less than the fill because ext4 keeps its
/// own metadata allocated and `FITRIM` may skip extents below the granularity.
const MIN_RECLAIM: u64 = 256 << 20;
/// How much the fill must have allocated for the measurement to mean anything.
const MIN_GROWTH: u64 = 384 << 20;

/// The kernel and initramfs to boot.
///
/// The **bootstrap** kernel first, deliberately: it is built from
/// `guest/bootstrap-kernel/entangled.config`, which has `CONFIG_EXT4_FS=y`, so
/// the guest can mount a real filesystem and the probe can take its `FITRIM`
/// route — a genuine `fstrim`, which is the acceptance that matters. The
/// Debian-installer test kernel has ext4 as a *module*, so there the probe falls
/// back to `BLKDISCARD`: the same `blkdev_issue_discard` path one layer down,
/// and still a real guest asking a real device.
fn artifacts() -> Option<(PathBuf, PathBuf)> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let initramfs = root.join("artifacts/tests/test-initramfs.cpio.gz");
    if !initramfs.is_file() {
        return None;
    }
    for kernel in ["artifacts/bootstrap/vmlinuz", "artifacts/tests/vmlinuz"] {
        let kernel = root.join(kernel);
        if kernel.is_file() {
            return Some((kernel, initramfs));
        }
    }
    None
}

/// Where the test image lives: WSL-native by default, because drvfs cannot do
/// sparse files and the whole measurement would read as "no reclaim".
fn image_path() -> PathBuf {
    if let Ok(dir) = std::env::var("ENTANGLED_VM_DIR") {
        return PathBuf::from(dir).join("discard-test.raw");
    }
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
    PathBuf::from(home).join("entangled-vms/discard-test.raw")
}

/// A fresh sparse image with an ext4 filesystem on it. `mkfs.ext4` is a host
/// tool, not a guest one: the test initramfs has no mkfs, and formatting here
/// keeps the guest side to the one thing being measured.
///
/// `-E nodiscard` matters — otherwise mkfs discards the whole device on
/// creation, which would work and would also hide whether the *guest* trims.
fn fresh_ext4_image(path: &Path) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let _ = std::fs::remove_file(path);
    create_raw(path, IMAGE_BYTES).map_err(|e| e.to_string())?;
    let output = std::process::Command::new("mkfs.ext4")
        .args(["-q", "-F", "-E", "nodiscard", "-O", "^has_journal"])
        .arg(path)
        .output()
        .map_err(|e| format!("mkfs.ext4 not runnable: {e}"))?;
    if !output.status.success() {
        return Err(format!(
            "mkfs.ext4 failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(())
}

/// One field of the guest's `trim` probe line, or "" when the line (or field) is
/// missing — a refused trim prints `VMHOST_TEST_FAIL` and has no fields at all,
/// which is exactly what phase one expects.
fn probe_field(outcome: &BootOutcome, key: &str) -> String {
    outcome
        .probe("trim")
        .unwrap_or_default()
        .into_iter()
        .find(|(k, _)| k == key)
        .map(|(_, v)| v)
        .unwrap_or_default()
}

fn tail(serial: &str) -> String {
    let lines: Vec<&str> = serial.lines().collect();
    lines[lines.len().saturating_sub(25)..].join("\n")
}

#[test]
fn a_guest_fstrim_gives_the_host_its_disk_space_back() {
    if !Path::new("/dev/kvm").exists() {
        eprintln!("skipping: /dev/kvm is not available");
        return;
    }
    let Some((kernel, initramfs)) = artifacts() else {
        eprintln!(
            "skipping: build the guest artifacts first \
             (scripts/fetch-test-kernel.sh, scripts/build-test-initramfs.sh)"
        );
        return;
    };
    let image = image_path();
    if let Err(why) = fresh_ext4_image(&image) {
        eprintln!("skipping: cannot prepare the test image: {why}");
        return;
    }
    let empty = allocated_bytes(&image).expect("host reports allocated size");
    if empty > IMAGE_BYTES / 4 {
        eprintln!(
            "skipping: an empty {} image already allocates {} on this filesystem, \
             so it cannot do sparse files (drvfs?) — put the image on a WSL-native path",
            format_bytes(IMAGE_BYTES),
            format_bytes(empty)
        );
        let _ = std::fs::remove_file(&image);
        return;
    }

    let spec = BootSpec::new(kernel, initramfs)
        .with_disk(image.clone())
        .with_trim_probe(FILL_MIB);

    // ---- phase 1: the world before this feature -----------------------------
    // Single-threaded on purpose: this test binary has one test, and the device
    // reads the knob when it is constructed inside `boot_once`.
    std::env::set_var("ENTANGLED_BLK_DISCARD", "off");
    let without = boot_once(&spec).expect("boot with reclaim withheld");
    std::env::remove_var("ENTANGLED_BLK_DISCARD");
    assert!(
        without.reached_ready(),
        "guest never booted; serial tail:\n{}",
        tail(&without.serial)
    );
    let grown = allocated_bytes(&image).expect("host reports allocated size");
    assert_eq!(
        probe_field(&without, "granularity"),
        "",
        "with the features withheld the probe must not have succeeded at all; \
         serial tail:\n{}",
        tail(&without.serial)
    );
    assert!(
        grown >= empty + MIN_GROWTH,
        "the guest's {} MiB fill only grew the image from {} to {} — nothing to reclaim;          serial tail:
{}",
        FILL_MIB,
        format_bytes(empty),
        format_bytes(grown),
        tail(&without.serial)
    );

    // ---- phase 2: reclaim on ------------------------------------------------
    let with = boot_once(&spec).expect("boot with reclaim on");
    assert!(
        with.reached_ready(),
        "guest never booted; serial tail:\n{}",
        tail(&with.serial)
    );
    let after = allocated_bytes(&image).expect("host reports allocated size");

    let route = probe_field(&with, "path");
    let filled: u64 = probe_field(&with, "filled").parse().unwrap_or(0);
    let trimmed: u64 = probe_field(&with, "trimmed").parse().unwrap_or(0);
    let granularity = probe_field(&with, "granularity");
    let max_discard = probe_field(&with, "max_discard");
    let max_write_zeroes = probe_field(&with, "max_write_zeroes");

    println!(
        "reclaim via {route}: empty {} -> filled {} -> after fstrim {}\n\
         guest filled {}, trimmed {}; guest-side limits: granularity={granularity} \
         discard_max_bytes={max_discard} write_zeroes_max_bytes={max_write_zeroes}",
        format_bytes(empty),
        format_bytes(grown),
        format_bytes(after),
        format_bytes(filled),
        format_bytes(trimmed),
    );

    assert!(
        !route.is_empty(),
        "no successful trim probe line; serial tail:\n{}",
        tail(&with.serial)
    );
    assert_eq!(filled, FILL_MIB << 20, "the guest filled the wrong amount");
    // The config fields we publish have to have become queue limits, or the
    // guest would never have issued a discard in the first place.
    assert_ne!(
        granularity, "0",
        "the guest kernel reported discard_granularity=0, so the config fields \
         never landed; serial tail:\n{}",
        tail(&with.serial)
    );
    assert_ne!(max_discard, "0", "discard_max_bytes must be non-zero");
    assert!(
        trimmed >= MIN_RECLAIM,
        "the guest only trimmed {} of the {} it filled",
        format_bytes(trimmed),
        format_bytes(filled)
    );
    // The measurement that matters.
    assert!(
        after + MIN_RECLAIM <= grown,
        "the host kept {} allocated after the guest trimmed {} (it held {} before, \
         and an empty image is {})",
        format_bytes(after),
        format_bytes(trimmed),
        format_bytes(grown),
        format_bytes(empty)
    );
    assert_eq!(
        std::fs::metadata(&image).expect("image").len(),
        IMAGE_BYTES,
        "reclaim must never change the image's apparent size"
    );

    let _ = std::fs::remove_file(&image);
}
