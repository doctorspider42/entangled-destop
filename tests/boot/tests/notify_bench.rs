//! Queue-notify offload measurement (backlog MVP-307).
//!
//! Boots the same guest twice — once with every kick handled inline on the vCPU
//! thread (`QueueNotifyMode::Synchronous`, the pre-MVP-307 behaviour) and once
//! with kicks offloaded to ioeventfds and per-device worker threads — and prints
//! boot wall-time plus the in-guest sequential-read rate for both.
//!
//! `#[ignore]`d: it is a measurement, not an assertion about a number, and it
//! needs guest artifacts plus a scratch disk image. Run it with
//!
//! ```text
//! cargo test -p boot-tests --test notify_bench -- --ignored --nocapture
//! ```
//!
//! On the Windows development host the scratch image must live on a native
//! Linux filesystem (`ENTANGLED_SCRATCH_DIR`, default `/tmp/entangled-bench`):
//! the drvfs mount holding the repository cannot create sparse files.
//!
//! # Measured (WSL2 on Hyper-V, 1 vCPU, 256 MiB, 64 MiB raw disk on ext4)
//!
//! ```text
//!                              boot to marker     48 MiB sequential read
//! synchronous (vCPU inline)     3951 / 4292 ms    238601 / 209157 KiB/s
//! ioeventfd + worker thread     3964 / 4791 ms    290840 / 242128 KiB/s
//! ```
//!
//! Two runs, medians of five boots each. Boot time is unchanged: a boot spends
//! its time in kernel init and issues only a handful of kicks, and the spread
//! between the two runs (3.9 s vs 4.3 s for the *same* mode) is larger than the
//! difference between the modes. Guest I/O throughput is where the offload pays
//! off: **+22% and +16%** on the two runs, from not stalling the vCPU for the
//! duration of every disk request.

#![cfg(target_os = "linux")]

use std::path::PathBuf;
use std::time::Duration;

use boot_tests::{
    boot_artifacts, boot_once, kvm_available, make_raw_disk, open_fds, BootOutcome, BootSpec,
};
use machine_x86::notify::QueueNotifyMode;

/// Disk size and how much of it the guest reads. 64 MiB of 64 KiB reads is
/// ~1000 virtio-blk requests, enough for the notify path to matter without
/// making the test take minutes.
const DISK_MIB: u64 = 64;
const READ_MIB: u64 = 48;

/// Repeats per mode; the median is reported so a single scheduling hiccup does
/// not decide the comparison.
const ROUNDS: usize = 5;

fn scratch_disk() -> PathBuf {
    let dir =
        std::env::var("ENTANGLED_SCRATCH_DIR").unwrap_or_else(|_| "/tmp/entangled-bench".into());
    PathBuf::from(dir).join("notify-bench.raw")
}

fn median(mut values: Vec<u64>) -> u64 {
    values.sort_unstable();
    values.get(values.len() / 2).copied().unwrap_or(0)
}

#[derive(Default)]
struct Run {
    boot_ms: Vec<u64>,
    read_kib_per_s: Vec<u64>,
    /// Boots that never printed the ready marker inside the deadline. Reported
    /// rather than asserted on: this measurement runs under nested
    /// virtualisation (WSL2 on Hyper-V), where an occasional stall is not
    /// something the VMM can be held to.
    stalled: usize,
}

/// Boots `ROUNDS` times, tolerating stalls so one bad boot cannot hide the
/// numbers for the rest.
fn measure(label: &str, spec: &BootSpec) -> Run {
    let mut run = Run::default();
    for round in 0..ROUNDS {
        let outcome: BootOutcome = match boot_once(spec) {
            Ok(outcome) => outcome,
            Err(error) => {
                eprintln!("{label} round {round}: boot failed: {error}");
                run.stalled += 1;
                continue;
            }
        };
        match outcome.time_to_ready {
            Some(ready) => run.boot_ms.push(ready.as_millis() as u64),
            None => {
                eprintln!(
                    "{label} round {round}: no ready marker; serial tail:\n{}",
                    tail(&outcome.serial, 12)
                );
                run.stalled += 1;
                continue;
            }
        }
        match outcome.probe_value("blkbench", "kib_per_s") {
            Some(rate) => run.read_kib_per_s.push(rate),
            None => eprintln!(
                "{label} round {round}: no blkbench probe line; serial tail:\n{}",
                tail(&outcome.serial, 8)
            ),
        }
    }
    run
}

fn tail(serial: &str, lines: usize) -> String {
    let all: Vec<&str> = serial.lines().collect();
    all[all.len().saturating_sub(lines)..].join("\n")
}

#[test]
#[ignore = "measurement: needs guest artifacts, a scratch disk and ~1 minute"]
fn ioeventfd_versus_synchronous_queue_notify() {
    if !kvm_available() {
        return;
    }
    let Some((kernel, initramfs)) = boot_artifacts() else {
        return;
    };
    let disk = scratch_disk();
    make_raw_disk(&disk, DISK_MIB).expect("scratch disk image");

    let base = BootSpec::new(kernel, initramfs)
        .with_disk(disk.clone())
        .with_blk_bench(READ_MIB);
    let base = BootSpec {
        deadline: Duration::from_secs(120),
        ..base
    };

    let fds_before = open_fds();

    let sync = measure(
        "sync",
        &base.clone().with_notify(QueueNotifyMode::Synchronous),
    );
    let fast = measure(
        "ioeventfd",
        &base.clone().with_notify(QueueNotifyMode::Ioeventfd),
    );

    let sync_boot = median(sync.boot_ms.clone());
    let fast_boot = median(fast.boot_ms.clone());
    let sync_read = median(sync.read_kib_per_s.clone());
    let fast_read = median(fast.read_kib_per_s.clone());

    println!("\n--- MVP-307 queue notify: {ROUNDS} boots per mode ---");
    println!("boot to VMHOST_GUEST_READY (median ms)");
    println!(
        "  synchronous (vCPU inline) : {sync_boot} ms  {:?} stalled={}",
        sync.boot_ms, sync.stalled
    );
    println!(
        "  ioeventfd + worker thread : {fast_boot} ms  {:?} stalled={}",
        fast.boot_ms, fast.stalled
    );
    println!("in-guest sequential read of {READ_MIB} MiB from /dev/vda (median KiB/s)");
    println!(
        "  synchronous (vCPU inline) : {sync_read} KiB/s  {:?}",
        sync.read_kib_per_s
    );
    println!(
        "  ioeventfd + worker thread : {fast_read} KiB/s  {:?}",
        fast.read_kib_per_s
    );
    if sync_read > 0 && fast_read > 0 {
        println!(
            "  read throughput change    : {:+.1}%",
            (fast_read as f64 / sync_read as f64 - 1.0) * 100.0
        );
    }
    println!("---\n");

    // The only hard assertion: neither mode may leak file descriptors across
    // the whole run (EPIC 14 acceptance, "closing the VM leaves nothing
    // behind"). A couple of fds of slack covers lazily opened /proc handles.
    let fds_after = open_fds();
    assert!(
        fds_after <= fds_before + 2,
        "fd leak across {} boots: {fds_before} -> {fds_after}",
        ROUNDS * 2
    );
    assert!(
        !fast.boot_ms.is_empty(),
        "no ioeventfd boot reached the ready marker"
    );

    let _ = std::fs::remove_file(&disk);
}
