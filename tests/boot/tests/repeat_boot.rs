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
//! `#[ignore]`d, as EPIC 14 requires for endurance tiers. A healthy boot takes
//! about four seconds, so a clean run of 100 is ~7 minutes. Invoke with
//!
//! ```text
//! cargo test -p boot-tests --test repeat_boot -- --ignored --nocapture
//! ```
//!
//! Knobs: `ENTANGLED_BOOT_ITERATIONS=<n>` shortens the run while iterating,
//! `ENTANGLED_BOOT_DEADLINE_SECS=<n>` shortens the per-boot deadline,
//! `ENTANGLED_BOOT_DISK=1` attaches a scratch virtio-blk disk (image location
//! from `ENTANGLED_SCRATCH_DIR`, which must be a native Linux path),
//! `ENTANGLED_BOOT_TRANSPORT=pci` runs the whole endurance loop over virtio-pci
//! instead of virtio-mmio (EPIC 19) — which is the point of making the transport
//! a harness parameter: a leaked irqfd, ioeventfd or worker thread on the newer
//! bus shows up in exactly the same accounting.
//!
//! # The leak half passes; the 100/100 half does not, and that is the finding
//!
//! Measured over 100 boots with no virtio device attached at all: **73 reached
//! the marker**, while file descriptors and thread count stayed exactly flat and
//! RSS moved 4080 → 4124 KiB. Nothing leaks; the failures are lost interrupts.
//!
//! Every stall is identical — the kernel log ends at exactly
//!
//! ```text
//! [    4.081880] Run /init as init process
//! ```
//!
//! and the vCPU is still running (`Ok(Stopped)` when the harness stops it). That
//! is the last `printk` before userspace, and `printk` uses the *polled* 8250
//! path while a userspace write to `/dev/console` uses the *interrupt-driven* tty
//! path: the guest is blocked in a `write` of the marker to fd 1, waiting for a
//! transmitter-empty interrupt on IRQ 4 that never arrives. With a disk attached
//! the same defect shows up on the first disk read, with the device's
//! `INTERRUPT_STATUS` still reading `INT_VRING`.
//!
//! The machine model publishes neither an MP table nor ACPI tables, so Linux
//! reports "ACPI MADT or MP tables are not detected" and switches to virtual-wire
//! mode: every interrupt, serial included, goes through the 8259 as ExtINT
//! instead of through the in-kernel IOAPIC. Giving the guest a real interrupt
//! topology is the fix; see the `IrqFdLine` docs in `machine_x86::virtio`.
//!
//! The assertions are deliberately left strict — 100/100 boots is EPIC 14's
//! acceptance criterion, and this test is the gate for it. The leak assertions run
//! first, so a stalled run still reports its verdict on them.

#![cfg(target_os = "linux")]

use std::path::PathBuf;
use std::time::Duration;

use boot_tests::{
    boot_artifacts, boot_once, kvm_available, make_raw_disk, open_fds, rss_kib, thread_count,
    BootSpec,
};
use control_api::VirtioTransport;

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

/// Per-boot deadline. A healthy boot reaches the marker in about four seconds,
/// so the default is generous without making a run dominated by the stalls
/// described in the test docs.
fn deadline() -> Duration {
    let secs = std::env::var("ENTANGLED_BOOT_DEADLINE_SECS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(30u64)
        .clamp(5, 600);
    Duration::from_secs(secs)
}

/// Which transport to boot. Defaults to mmio, so an unqualified run still
/// measures what this test has always measured.
fn transport() -> VirtioTransport {
    match std::env::var("ENTANGLED_BOOT_TRANSPORT").as_deref() {
        Ok("pci") => VirtioTransport::Pci,
        Ok(other) if !other.is_empty() && other != "mmio" => {
            eprintln!("unrecognised ENTANGLED_BOOT_TRANSPORT '{other}'; using mmio");
            VirtioTransport::Mmio
        }
        _ => VirtioTransport::Mmio,
    }
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
    let mut spec = BootSpec::new(kernel, initramfs).with_transport(transport());
    spec.deadline = deadline();
    println!(
        "booting {total} times over the {} transport",
        spec.transport
    );
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
    //
    // Leak checks first: they are independent of whether every boot reached the
    // marker, and they are the reason this test exists. A stalled boot still
    // creates and tears down a full VM, so the accounting stays meaningful.

    // An fd or thread that survives teardown is a leak, full stop: both counts
    // are exact, so no slack is warranted.
    assert_eq!(
        last.1, baseline.1,
        "file descriptors grew from {} to {} across {total} boots",
        baseline.1, last.1
    );
    assert_eq!(
        last.2, baseline.2,
        "threads grew from {} to {} across {total} boots: a vCPU or device worker thread was not joined",
        baseline.2, last.2
    );
    assert!(
        last.3 <= baseline.3 + RSS_SLACK_KIB,
        "RSS grew from {} KiB to {} KiB across {total} boots, more than the {RSS_SLACK_KIB} KiB allowance",
        baseline.3,
        last.3
    );

    for failure in &failures {
        eprintln!("{failure}");
    }
    assert!(
        failures.is_empty(),
        "{} of {total} boots failed (see the log above and this test's docs)",
        failures.len()
    );
}
