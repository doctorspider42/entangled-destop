//! Repeated-boot endurance test (backlog MVP-1403).
//!
//! Boots the bootstrap kernel with the test initramfs 100 times in a row inside
//! one host process and checks two things:
//!
//! 1. every boot reaches `VMHOST_GUEST_READY` (EPIC 14: "100/100 correct boots
//!    of the prepared image");
//! 2. the host process does not grow — file descriptors, threads and RSS after a
//!    boot must not creep up, which is what catches a leaked vCPU thread, device
//!    worker thread (MVP-307), eventfd, irqfd or guest memory mapping.
//!
//! `#[ignore]`d, as EPIC 14 requires for endurance tiers. Runtime is roughly
//! 4 seconds per boot, so ~7 minutes for the default 100. Invoke with
//!
//! ```text
//! cargo test -p boot-tests --test repeat_boot -- --ignored --nocapture
//! ```
//!
//! `ENTANGLED_BOOT_ITERATIONS=<n>` shortens the run while iterating on it, and
//! `ENTANGLED_BOOT_DISK=1` attaches a scratch virtio-blk disk so the device path
//! is exercised too (image location from `ENTANGLED_SCRATCH_DIR`).

#![cfg(target_os = "linux")]

use std::path::PathBuf;
use std::time::Duration;

use boot_tests::{
    boot_artifacts, boot_once, kvm_available, make_raw_disk, open_fds, rss_kib, thread_count,
    BootSpec,
};

/// EPIC 14's acceptance number.
const DEFAULT_ITERATIONS: usize = 100;

/// Iterations skipped before the "settled" baseline is taken: the first boots
/// warm the page cache, the allocator's arenas and any lazily-initialised host
/// state, so their RSS is not representative.
const WARMUP: usize = 5;

/// RSS is allowed to move by this much between the settled baseline and the end
/// of the run. Generous in absolute terms but far below what one leaked 256 MiB
/// guest mapping or a growing serial capture would produce.
const RSS_SLACK_KIB: u64 = 32 * 1024;

fn iterations() -> usize {
    std::env::var("ENTANGLED_BOOT_ITERATIONS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(DEFAULT_ITERATIONS)
        .max(1)
}

fn scratch_disk() -> Option<PathBuf> {
    if std::env::var("ENTANGLED_BOOT_DISK").is_err() {
        return None;
    }
    let dir =
        std::env::var("ENTANGLED_SCRATCH_DIR").unwrap_or_else(|_| "/tmp/entangled-bench".into());
    let path = PathBuf::from(dir).join("repeat-boot.raw");
    match make_raw_disk(&path, 64) {
        Ok(()) => Some(path),
        Err(error) => {
            eprintln!("no scratch disk ({error}); booting without one");
            None
        }
    }
}

#[test]
#[ignore = "endurance: 100 sequential boots, roughly 7 minutes"]
fn hundred_sequential_boots_leak_nothing() {
    if !kvm_available() {
        return;
    }
    let Some((kernel, initramfs)) = boot_artifacts() else {
        return;
    };
    let total = iterations();
    let mut spec = BootSpec::new(kernel, initramfs);
    spec.deadline = Duration::from_secs(60);
    if let Some(disk) = scratch_disk() {
        spec = spec.with_disk(disk);
    }

    let mut failures: Vec<String> = Vec::new();
    let mut boot_ms: Vec<u64> = Vec::with_capacity(total);
    // (iteration, fds, threads, rss) sampled after teardown, when nothing from
    // the boot should still be alive.
    let mut samples: Vec<(usize, usize, u64, u64)> = Vec::with_capacity(total);

    for iteration in 0..total {
        match boot_once(&spec) {
            Ok(outcome) => match outcome.time_to_ready {
                Some(ready) => boot_ms.push(ready.as_millis() as u64),
                None => failures.push(format!(
                    "iteration {iteration}: no ready marker within {:?}; serial tail:\n{}",
                    spec.deadline,
                    outcome
                        .serial
                        .lines()
                        .rev()
                        .take(15)
                        .collect::<Vec<_>>()
                        .into_iter()
                        .rev()
                        .collect::<Vec<_>>()
                        .join("\n")
                )),
            },
            Err(error) => failures.push(format!("iteration {iteration}: {error}")),
        }
        samples.push((iteration, open_fds(), thread_count(), rss_kib()));
        if iteration % 10 == 9 || iteration + 1 == total {
            let (_, fds, threads, rss) = samples[samples.len() - 1];
            println!(
                "after {:>3} boots: fds={fds} threads={threads} rss={rss} KiB failures={}",
                iteration + 1,
                failures.len()
            );
        }
    }

    // ------------------------------------------------------------- reporting
    boot_ms.sort_unstable();
    let median = boot_ms.get(boot_ms.len() / 2).copied().unwrap_or(0);
    println!(
        "\n{} of {total} boots reached the ready marker; boot time min/median/max = {}/{}/{} ms",
        boot_ms.len(),
        boot_ms.first().copied().unwrap_or(0),
        median,
        boot_ms.last().copied().unwrap_or(0),
    );

    let baseline = samples
        .get(WARMUP.min(samples.len().saturating_sub(1)))
        .copied()
        .unwrap_or((0, 0, 0, 0));
    let last = samples.last().copied().unwrap_or((0, 0, 0, 0));
    println!(
        "settled baseline (after {} boots): fds={} threads={} rss={} KiB",
        baseline.0 + 1,
        baseline.1,
        baseline.2,
        baseline.3
    );
    println!(
        "final           (after {} boots): fds={} threads={} rss={} KiB\n",
        last.0 + 1,
        last.1,
        last.2,
        last.3
    );

    // ------------------------------------------------------------ assertions
    for failure in &failures {
        eprintln!("{failure}");
    }
    assert!(
        failures.is_empty(),
        "{} of {total} boots failed (see the log above)",
        failures.len()
    );

    // An fd or thread that survives teardown is a leak, full stop: both counts
    // are exact, so no slack is warranted.
    assert_eq!(
        last.1, baseline.1,
        "file descriptors grew from {} to {} across {total} boots",
        baseline.1, last.1
    );
    assert_eq!(
        last.2, baseline.2,
        "threads grew from {} to {} across {total} boots — a vCPU or device \
         worker thread was not joined",
        baseline.2, last.2
    );
    assert!(
        last.3 <= baseline.3 + RSS_SLACK_KIB,
        "RSS grew from {} KiB to {} KiB across {total} boots, more than the \
         {RSS_SLACK_KIB} KiB allowance",
        baseline.3,
        last.3
    );
}
