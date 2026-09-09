//! Long-run endurance soak (backlog MVP-1404).
//!
//! One VM, booted once, left running for hours. Where `repeat_boot.rs`
//! (MVP-1403) looks for what a *teardown* forgets to release, this looks for
//! what a *running* VM accumulates — and those are different defects: a leaked
//! irqfd shows up in the first, a serial IRQ that stops being delivered after
//! the seventy-thousandth line shows up only in the second.
//!
//! ```text
//! ENTANGLED_SOAK_LOG=$HOME/soak.tsv \
//!   cargo test -p boot-tests --test soak -- --ignored --nocapture
//! ```
//!
//! `#[ignore]`d, like every endurance tier. Knobs, all optional:
//!
//! | Variable | Default | Meaning |
//! |---|---|---|
//! | `ENTANGLED_SOAK_SECS` | 7200 (2 h) | how long the guest runs after warm-up |
//! | `ENTANGLED_SOAK_SAMPLE_SECS` | 60 | how often the host records a sample |
//! | `ENTANGLED_SOAK_HEARTBEAT_MS` | 1000 | the guest's heartbeat period |
//! | `ENTANGLED_SOAK_MEMORY_MIB` | 256 | guest RAM |
//! | `ENTANGLED_SOAK_TRANSPORT` | mmio | `pci` runs the same soak over virtio-pci |
//! | `ENTANGLED_SOAK_LOG` | — | write every sample to this TSV file as it is taken |
//!
//! **Use the log file.** A soak is exactly the kind of run that a reboot, a
//! full disk or an overnight power cut takes with it, and a run whose numbers
//! only exist in the test's final `println!` leaves *nothing* behind when that
//! happens — it happened here on 2026-09-08, fifty minutes into a four-hour
//! run. Each sample is written and flushed as it is taken, so the file is a
//! complete account of everything up to the moment the machine died: it carries
//! the run's parameters, then per-sample RSS, descriptors, threads, the newest
//! heartbeat with the guest and host clocks beside each other, and finally the
//! same summary block the test prints. Every assertion below except the two
//! whole-transcript ones (gaps, unexpected lines) can be re-derived from it.
//!
//! # What is being watched, and why each one needs *time*
//!
//! The guest is the test initramfs with `entangled.heartbeat=<ms>`: it prints
//! `VMHOST_HEARTBEAT <n> uptime_ms=<t>` for ever and does nothing else. That one
//! line carries most of the measurement.
//!
//! * **Host memory growth** — RSS of the VMM process, sampled periodically. The
//!   guest touches its RAM once during boot and then stops, so a settled VM's
//!   RSS is flat; anything that climbs is host-side accumulation.
//! * **File descriptors and threads** — a device worker or an eventfd created
//!   per interrupt, per queue kick or per timer expiry would show here and
//!   nowhere else. Both counts must be *identical* to the settled baseline.
//! * **Timer drift** — the guest's own `CLOCK_MONOTONIC` against the host's,
//!   compared between the first and last heartbeat whose arrival the host timed.
//!   Reported in ppm; a TSC or kvm-clock that is offered to the guest at the
//!   wrong frequency drifts linearly and is invisible in a four-second boot.
//! * **Interrupt and queue stalls** — the heartbeat is written by userspace to
//!   the tty, i.e. through the *interrupt-driven* 8250 path (the one that lost
//!   edges before the machine had an IOAPIC; see `repeat_boot.rs`). Every
//!   sampling interval must carry its share of heartbeats, and the tick numbers
//!   must form an unbroken sequence — a gap is a line that was produced and
//!   never delivered.
//! * **Log spam** — every console line after the ready marker that is not a
//!   heartbeat is counted and printed. A guest that starts logging RCU stalls or
//!   hung tasks at hour three says so here.

#![cfg(target_os = "linux")]

use std::io::Write as _;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use boot_tests::{
    boot_artifacts, boot_once_driven, kvm_available, open_fds, rss_kib, thread_count, BootSpec,
    VmHandle,
};
use control_api::VirtioTransport;
use linux_boot::GUEST_READY_MARKER;

/// Two hours: long enough that a per-interrupt or per-kick leak has had seven
/// thousand heartbeats to show itself, and short enough that the run actually
/// finishes on a shared development machine — which the four-hour default this
/// replaces did not, twice. Longer runs are `ENTANGLED_SOAK_SECS`, and the
/// eight-hour one MVP-1404 asks for is a nightly job, not a desk job.
const DEFAULT_SECS: u64 = 2 * 3600;

/// Time given to the boot itself before the soak is declared a failure.
const BOOT_DEADLINE: Duration = Duration::from_secs(120);

/// Settling time before the baseline sample: the first seconds after the marker
/// still contain the guest faulting in its pages and the allocator growing its
/// arenas, and none of that is a leak.
const WARMUP: Duration = Duration::from_secs(60);

/// How often the driver looks at the console. Bounds how stale the "last
/// heartbeat" reading can be, which is the error term in the drift number.
const OBSERVE_INTERVAL: Duration = Duration::from_millis(250);

/// RSS allowance between the settled baseline and the end of the run — the same
/// number `repeat_boot.rs` uses, so the two endurance tests agree on what
/// "did not grow" means.
const RSS_SLACK_KIB: u64 = 32 * 1024;

/// A sampling interval must carry at least this fraction of the heartbeats its
/// length implies. Well below 1.0 on purpose: this machine runs other VMs and
/// builds, and a guest descheduled for a second is not a stalled guest. A lost
/// interrupt does not produce 0.6 of the heartbeats, it produces none.
const MIN_HEARTBEAT_RATIO: f64 = 0.5;

/// Drift allowance, guest monotonic clock against host monotonic clock. A
/// correct kvm-clock is orders of magnitude better than this; a guest given the
/// wrong TSC frequency is orders of magnitude worse.
const MAX_DRIFT_PPM: f64 = 10_000.0;

/// Console lines after the ready marker that are neither heartbeats nor blank.
/// Not zero: the kernel is entitled to a few late lines (a device probing after
/// userspace started). Sustained spam blows straight past it.
const MAX_UNEXPECTED_LINES: usize = 16;

fn env_u64(name: &str, default: u64, min: u64, max: u64) -> u64 {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
        .clamp(min, max)
}

fn transport() -> VirtioTransport {
    match std::env::var("ENTANGLED_SOAK_TRANSPORT").as_deref() {
        Ok("pci") => VirtioTransport::Pci,
        Ok(other) if !other.is_empty() && other != "mmio" => {
            eprintln!("unrecognised ENTANGLED_SOAK_TRANSPORT '{other}'; using mmio");
            VirtioTransport::Mmio
        }
        _ => VirtioTransport::Mmio,
    }
}

/// One periodic reading of the host process and of the guest's progress.
#[derive(Debug, Clone, Copy)]
struct Sample {
    /// Seconds since the baseline was taken.
    at_s: f64,
    rss_kib: u64,
    fds: usize,
    threads: u64,
    /// Heartbeats seen in total, from the tick number rather than by counting
    /// lines: a *dropped* line must not silently shrink the denominator.
    ticks: u64,
    /// Guest and host milliseconds between the baseline heartbeat and the
    /// newest one seen at this sample. Recorded per sample and not only at the
    /// end so that the drift is a *series* in the log file — a clock that runs
    /// away linearly and one that jumps once look identical in a single
    /// end-to-end number.
    guest_ms: f64,
    host_ms: f64,
    serial_bytes: usize,
}

impl Sample {
    /// Guest clock against host clock since the baseline, in parts per million.
    fn drift_ppm(&self) -> f64 {
        if self.host_ms > 0.0 {
            (self.guest_ms - self.host_ms) / self.host_ms * 1e6
        } else {
            0.0
        }
    }
}

/// What the driver hands back when the run ends.
#[derive(Debug, Default, Clone)]
struct Collected {
    samples: Vec<Sample>,
    /// The heartbeat the baseline was taken at, and the newest one seen since;
    /// the pair is the clock comparison.
    first_beat: Option<Beat>,
    last_beat: Option<Beat>,
    /// Set when the guest never got far enough to soak at all.
    boot_failed: Option<String>,
    ran_for: Option<Duration>,
}

/// The last heartbeat the host has actually seen, and when it saw it.
#[derive(Debug, Clone, Copy)]
struct Beat {
    tick: u64,
    guest_uptime_ms: u64,
    /// Host clock at the first observation of this tick — never the tick's own
    /// print time, but within one [`OBSERVE_INTERVAL`] plus one harness poll of
    /// it. Only ever set where the *transition* to this tick was watched, so
    /// that the same latency sits on both ends of an interval and cancels.
    seen: Instant,
}

/// Parses the newest `VMHOST_HEARTBEAT <n> uptime_ms=<t>` line in `tail`.
///
/// Only complete lines count: a half-written one has no terminator yet, and its
/// uptime field may be truncated mid-number.
fn last_beat(tail: &str) -> Option<(u64, u64)> {
    tail.lines()
        .rev()
        // The final element of `lines()` may be a partial line; a trailing
        // newline makes the last element empty, which this skips anyway.
        .skip(usize::from(!tail.ends_with('\n')))
        .find_map(|line| {
            let rest = line.trim().strip_prefix("VMHOST_HEARTBEAT ")?;
            let mut fields = rest.split_ascii_whitespace();
            let tick: u64 = fields.next()?.parse().ok()?;
            let uptime: u64 = fields.next()?.strip_prefix("uptime_ms=")?.parse().ok()?;
            Some((tick, uptime))
        })
}

/// Waits until a heartbeat has been observed, returning it, or `None` if
/// `timeout` expires first.
fn observe_beat(vm: &VmHandle, timeout: Duration) -> Option<Beat> {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some((tick, guest_uptime_ms)) = last_beat(&vm.serial_tail(4096)) {
            return Some(Beat {
                tick,
                guest_uptime_ms,
                seen: Instant::now(),
            });
        }
        if Instant::now() >= deadline {
            return None;
        }
        std::thread::sleep(OBSERVE_INTERVAL);
    }
}

/// Waits for the *next* heartbeat after the one already on the console.
///
/// The baseline of the clock comparison must be a beat the host watched
/// **arrive**, never one it merely found lying in the transcript. A beat that
/// is already there was printed up to one harness poll plus one observe
/// interval ago, and dating it "now" shortens the host side of every later
/// comparison by that much — a fixed offset that the ppm figure then divides by
/// the run length, so it reads as enormous drift in a short run and quietly
/// decays in a long one. Measured with `observe_beat` here: +77 356 ppm at 10 s
/// falling to +10 031 ppm at 60 s, all of it one ~0.6 s bias and none of it the
/// guest's clock. Detecting the transition puts the same small latency on both
/// ends of the interval, where it cancels.
fn observe_next_beat(vm: &VmHandle, timeout: Duration) -> Option<Beat> {
    let deadline = Instant::now() + timeout;
    let already = last_beat(&vm.serial_tail(4096)).map(|(tick, _)| tick);
    loop {
        if let Some((tick, guest_uptime_ms)) = last_beat(&vm.serial_tail(4096)) {
            if Some(tick) != already {
                return Some(Beat {
                    tick,
                    guest_uptime_ms,
                    seen: Instant::now(),
                });
            }
        }
        if Instant::now() >= deadline {
            return None;
        }
        std::thread::sleep(OBSERVE_INTERVAL);
    }
}

#[test]
#[ignore = "endurance: runs one VM for hours (ENTANGLED_SOAK_SECS, default 2 h)"]
fn a_long_running_guest_neither_grows_nor_stalls() {
    if !kvm_available() {
        return;
    }
    let Some((kernel, initramfs)) = boot_artifacts() else {
        return;
    };

    let soak_secs = env_u64("ENTANGLED_SOAK_SECS", DEFAULT_SECS, 30, 24 * 3600);
    let sample_secs = env_u64("ENTANGLED_SOAK_SAMPLE_SECS", 60, 5, 3600).min(soak_secs);
    let heartbeat_ms = env_u64("ENTANGLED_SOAK_HEARTBEAT_MS", 1000, 100, 10_000);
    let memory_mib = env_u64("ENTANGLED_SOAK_MEMORY_MIB", 256, 128, 4096);
    let warmup = WARMUP.min(Duration::from_secs(soak_secs / 4));
    let soak = Duration::from_secs(soak_secs);
    let sample_every = Duration::from_secs(sample_secs);

    let mut spec = BootSpec::new(kernel, initramfs)
        .with_extra_cmdline(format!("entangled.heartbeat={heartbeat_ms}"))
        .with_transport(transport())
        .with_vcpus(1)
        // At the default 2 ms the harness would copy and scan the whole
        // transcript five hundred times a second for four hours.
        .with_poll_interval(Duration::from_millis(500));
    spec.memory_mib = memory_mib;
    // The driver ends the run; the deadline is only the backstop that keeps a
    // wedged driver from running for ever.
    spec.deadline = soak + warmup + BOOT_DEADLINE + Duration::from_secs(120);

    println!(
        "soak: {soak_secs} s over {} with {memory_mib} MiB, heartbeat {heartbeat_ms} ms, \
         sampling every {sample_secs} s",
        spec.transport
    );

    // The log file is the run's black box: written and flushed sample by
    // sample, so a machine that dies mid-soak still leaves the numbers up to
    // the moment it died. The summary block is appended to the same file at the
    // end, which is why the path outlives the handle the driver owns.
    let log_path = std::env::var("ENTANGLED_SOAK_LOG")
        .ok()
        .filter(|path| !path.is_empty())
        .map(std::path::PathBuf::from);
    let mut log = log_path
        .as_ref()
        .and_then(|path| std::fs::File::create(path).ok());
    if let Some(file) = log.as_mut() {
        let _ = writeln!(
            file,
            "# entangled soak: {soak_secs} s, {} transport, {memory_mib} MiB, \
             heartbeat {heartbeat_ms} ms, sample {sample_secs} s, warm-up {warmup:?}\n\
             at_s\trss_kib\tfds\tthreads\tticks\tguest_ms\thost_ms\tdrift_ppm\tserial_bytes",
            spec.transport
        );
        let _ = file.flush();
    }

    // Everything the driver learns has to come back out through this: a driver
    // is `'static` and owns nothing of the test's stack. Assertions live after
    // the boot returns, so a failure reports the whole run rather than poisoning
    // a thread mid-soak.
    let collected = Arc::new(Mutex::new(Collected::default()));
    let driver_state = Arc::clone(&collected);

    let outcome = boot_once_driven(
        &spec,
        Some(Box::new(move |vm: VmHandle| {
            let mut state = Collected::default();
            let finish = |state: Collected| {
                match driver_state.lock() {
                    Ok(mut slot) => *slot = state,
                    Err(poisoned) => *poisoned.into_inner() = state,
                }
                vm.finish();
            };
            if vm.wait_for(GUEST_READY_MARKER, 1, BOOT_DEADLINE) == 0 {
                state.boot_failed = Some(format!(
                    "the guest never reached {GUEST_READY_MARKER} within {BOOT_DEADLINE:?}"
                ));
                finish(state);
                return;
            }
            if observe_beat(&vm, BOOT_DEADLINE).is_none() {
                state.boot_failed =
                    Some("the guest booted but produced no heartbeat at all".to_string());
                finish(state);
                return;
            }
            println!("soak: guest ready, settling for {warmup:?}");
            std::thread::sleep(warmup);

            // Baseline. Taken after the warm-up so that page faults and lazy
            // host initialisation are behind us; everything after this point is
            // what a *settled* VM does.
            let Some(base_beat) = observe_next_beat(&vm, Duration::from_secs(30)) else {
                state.boot_failed = Some("no heartbeat at the baseline sample".to_string());
                finish(state);
                return;
            };
            // The clock starts at the baseline *beat*, not before the wait for
            // it: otherwise the first interval is a second short of the ticks
            // its `at_s` span implies, and the delivery ratio opens the run with
            // a dip that means nothing.
            let started = base_beat.seen;
            state.first_beat = Some(base_beat);
            state.last_beat = Some(base_beat);
            let take = |at_s: f64, vm: &VmHandle, beat: Beat| Sample {
                at_s,
                rss_kib: rss_kib(),
                fds: open_fds(),
                threads: thread_count(),
                ticks: beat.tick,
                guest_ms: beat
                    .guest_uptime_ms
                    .saturating_sub(base_beat.guest_uptime_ms) as f64,
                host_ms: beat.seen.duration_since(base_beat.seen).as_secs_f64() * 1000.0,
                serial_bytes: vm.serial_bytes(),
            };
            state.samples.push(take(0.0, &vm, base_beat));

            let mut next_sample = started + sample_every;
            while started.elapsed() < soak {
                std::thread::sleep(OBSERVE_INTERVAL);
                // Track the newest heartbeat continuously rather than only at
                // sample time: the drift measurement's error is how stale this
                // reading is, so it is kept to one observe interval.
                if let Some((tick, guest_uptime_ms)) = last_beat(&vm.serial_tail(4096)) {
                    if state.last_beat.map(|b| b.tick) != Some(tick) {
                        state.last_beat = Some(Beat {
                            tick,
                            guest_uptime_ms,
                            seen: Instant::now(),
                        });
                    }
                }
                if Instant::now() < next_sample {
                    continue;
                }
                next_sample += sample_every;
                let beat = state.last_beat.unwrap_or(base_beat);
                let sample = take(started.elapsed().as_secs_f64(), &vm, beat);
                println!(
                    "soak {:>7.0}s: rss={} KiB fds={} threads={} ticks={} drift={:+.0} ppm \
                     console={} B",
                    sample.at_s,
                    sample.rss_kib,
                    sample.fds,
                    sample.threads,
                    sample.ticks,
                    sample.drift_ppm(),
                    sample.serial_bytes
                );
                if let Some(file) = log.as_mut() {
                    let _ = writeln!(
                        file,
                        "{:.0}\t{}\t{}\t{}\t{}\t{:.0}\t{:.0}\t{:+.0}\t{}",
                        sample.at_s,
                        sample.rss_kib,
                        sample.fds,
                        sample.threads,
                        sample.ticks,
                        sample.guest_ms,
                        sample.host_ms,
                        sample.drift_ppm(),
                        sample.serial_bytes
                    );
                    let _ = file.flush();
                }
                state.samples.push(sample);
            }
            state.ran_for = Some(started.elapsed());
            finish(state);
        })),
    )
    .expect("the soak boot failed outright");

    let collected = match Arc::try_unwrap(collected) {
        Ok(mutex) => mutex.into_inner().unwrap_or_else(|e| e.into_inner()),
        Err(shared) => match shared.lock() {
            Ok(slot) => slot.clone(),
            Err(poisoned) => poisoned.into_inner().clone(),
        },
    };
    let samples = collected.samples;
    let (first_beat, last_beat_seen) = (collected.first_beat, collected.last_beat);

    if let Some(why) = collected.boot_failed {
        panic!("{why}\nserial tail:\n{}", tail(&outcome.serial, 40));
    }
    assert!(
        outcome.reached_ready(),
        "the harness saw no ready marker:\n{}",
        tail(&outcome.serial, 40)
    );

    // ------------------------------------------------------------- reporting
    let baseline = *samples.first().expect("a baseline sample");
    let last = *samples.last().expect("a final sample");
    let (Some(first_beat), Some(final_beat)) = (first_beat, last_beat_seen) else {
        panic!("no heartbeats were timed; nothing to report");
    };

    let host_ms = final_beat
        .seen
        .duration_since(first_beat.seen)
        .as_secs_f64()
        * 1000.0;
    let guest_ms = final_beat
        .guest_uptime_ms
        .saturating_sub(first_beat.guest_uptime_ms) as f64;
    let drift_ppm = if host_ms > 0.0 {
        (guest_ms - host_ms) / host_ms * 1e6
    } else {
        0.0
    };
    // What the drift figure is worth. Each end of the interval is a beat the
    // host noticed within one harness poll plus one observe interval of its
    // being printed, so the whole measurement carries that much timing error —
    // constant in milliseconds, and therefore smaller in ppm the longer the run
    // is. Printed beside the number so nobody reads a short soak's drift as a
    // clock defect (see `observe_next_beat`).
    let drift_err_ppm = if host_ms > 0.0 {
        (spec.poll_interval + OBSERVE_INTERVAL).as_secs_f64() * 1000.0 / host_ms * 1e6
    } else {
        0.0
    };

    // Heartbeat accounting. `beats` is every tick number the console carries, so
    // a gap in it is a line the guest produced and the host never received.
    let beats = tick_numbers(&outcome.serial);
    let gaps = gaps_in(&beats);
    let unexpected = unexpected_lines(&outcome.serial);

    // Per-interval delivery: the stall check. Ticks are monotonic, so the
    // difference between consecutive samples is exactly what arrived in between.
    let expected_per_sample = sample_secs as f64 * 1000.0 / heartbeat_ms as f64;
    let mut worst_ratio = f64::INFINITY;
    let mut worst_at = 0.0;
    for pair in samples.windows(2) {
        let (a, b) = (pair[0], pair[1]);
        let seconds = b.at_s - a.at_s;
        if seconds <= 0.0 {
            continue;
        }
        let expected = seconds * 1000.0 / heartbeat_ms as f64;
        let ratio = (b.ticks - a.ticks) as f64 / expected.max(1.0);
        if ratio < worst_ratio {
            worst_ratio = ratio;
            worst_at = b.at_s;
        }
    }
    if !worst_ratio.is_finite() {
        worst_ratio = 1.0;
    }

    let hours = (last.at_s / 3600.0).max(f64::EPSILON);
    if let Some(ran_for) = collected.ran_for {
        println!("\nsoak: the driver ran for {ran_for:?}");
    }
    let report = format!(
        "---- soak result ----------------------------------------------------\n\
         duration          {:.0} s ({:.2} h) after a {warmup:?} warm-up, {} samples\n\
         RSS               {} -> {} KiB ({:+} KiB, {:+.0} KiB/h)\n\
         file descriptors  {} -> {}\n\
         threads           {} -> {}\n\
         heartbeats        {} ticks, {} lines on the console, {} gaps\n\
         delivery          worst interval {:.2} of expected ({} expected per {sample_secs} s), \
         at {:.0} s\n\
         guest clock       {:.0} ms guest vs {:.0} ms host, drift {:+.0} ppm \
         (+-{:.0} ppm observation error)\n\
         console           {} bytes total, {:.1} B per heartbeat, {} unexpected lines\n\
         ---------------------------------------------------------------------",
        last.at_s,
        last.at_s / 3600.0,
        samples.len(),
        baseline.rss_kib,
        last.rss_kib,
        last.rss_kib as i64 - baseline.rss_kib as i64,
        (last.rss_kib as f64 - baseline.rss_kib as f64) / hours,
        baseline.fds,
        last.fds,
        baseline.threads,
        last.threads,
        last.ticks - baseline.ticks,
        beats.len(),
        gaps.len(),
        worst_ratio,
        expected_per_sample as u64,
        worst_at,
        guest_ms,
        host_ms,
        drift_ppm,
        drift_err_ppm,
        last.serial_bytes,
        last.serial_bytes as f64 / (last.ticks.max(1) as f64),
        unexpected.len(),
    );
    println!("\n{report}");
    for line in unexpected.iter().take(MAX_UNEXPECTED_LINES + 4) {
        println!("unexpected console line: {line}");
    }
    for gap in gaps.iter().take(20) {
        println!("heartbeat gap: {} .. {}", gap.0, gap.1);
    }
    // The same verdict into the black box, so the file is the whole account of
    // the run and not just of its samples.
    if let Some(path) = log_path.as_ref() {
        if let Ok(mut file) = std::fs::OpenOptions::new().append(true).open(path) {
            for line in report.lines() {
                let _ = writeln!(file, "# {line}");
            }
            for gap in gaps.iter().take(20) {
                let _ = writeln!(file, "# heartbeat gap: {} .. {}", gap.0, gap.1);
            }
            for line in unexpected.iter().take(MAX_UNEXPECTED_LINES + 4) {
                let _ = writeln!(file, "# unexpected console line: {line}");
            }
            let _ = file.flush();
        }
    }

    // ------------------------------------------------------------ assertions
    //
    // Growth first, as in `repeat_boot.rs`: those verdicts stand even if the
    // guest also stalled, and they are the reason the run took four hours.
    assert_eq!(
        last.fds, baseline.fds,
        "file descriptors grew from {} to {} over {:.0} s of running VM",
        baseline.fds, last.fds, last.at_s
    );
    assert_eq!(
        last.threads, baseline.threads,
        "threads grew from {} to {} over {:.0} s of running VM",
        baseline.threads, last.threads, last.at_s
    );
    assert!(
        last.rss_kib <= baseline.rss_kib + RSS_SLACK_KIB,
        "RSS grew from {} KiB to {} KiB over {:.0} s, more than the {RSS_SLACK_KIB} KiB allowance",
        baseline.rss_kib,
        last.rss_kib,
        last.at_s
    );

    assert!(
        gaps.is_empty(),
        "{} gaps in the heartbeat sequence: lines the guest printed never reached the host",
        gaps.len()
    );
    assert!(
        worst_ratio >= MIN_HEARTBEAT_RATIO,
        "the guest delivered only {worst_ratio:.2} of the expected heartbeats in the interval \
         ending at {worst_at:.0} s — a stall, not slowness"
    );
    // The gate is widened by the measurement's own error rather than by a
    // fudge factor, so that a 60-second smoke run of this test is judged as
    // loosely as its evidence deserves and a two-hour run as tightly.
    assert!(
        drift_ppm.abs() <= MAX_DRIFT_PPM + drift_err_ppm,
        "guest clock drifted {drift_ppm:+.0} ppm against the host over {:.0} s, beyond the \
         {MAX_DRIFT_PPM:.0} ppm allowance and the {drift_err_ppm:.0} ppm this run could not see",
        host_ms / 1000.0
    );
    assert!(
        unexpected.len() <= MAX_UNEXPECTED_LINES,
        "{} console lines that are not heartbeats: the guest is logging something",
        unexpected.len()
    );
}

/// Every heartbeat tick number on the console, in order.
fn tick_numbers(console: &str) -> Vec<u64> {
    console
        .lines()
        .filter_map(|line| line.trim().strip_prefix("VMHOST_HEARTBEAT "))
        .filter_map(|rest| rest.split_ascii_whitespace().next()?.parse().ok())
        .collect()
}

/// Breaks in an otherwise consecutive sequence, as `(before, after)` pairs.
fn gaps_in(ticks: &[u64]) -> Vec<(u64, u64)> {
    ticks
        .windows(2)
        .filter(|pair| pair[1] != pair[0] + 1)
        .map(|pair| (pair[0], pair[1]))
        .collect()
}

/// Console lines after the ready marker that are neither heartbeats nor blank.
fn unexpected_lines(console: &str) -> Vec<String> {
    let after = match console.find(GUEST_READY_MARKER) {
        Some(at) => &console[at..],
        None => console,
    };
    after
        .lines()
        .skip(1)
        .map(|line| line.trim().to_string())
        .filter(|line| !line.is_empty() && !line.starts_with("VMHOST_HEARTBEAT"))
        .collect()
}

fn tail(text: &str, lines: usize) -> String {
    let all: Vec<&str> = text.lines().collect();
    all[all.len().saturating_sub(lines)..].join("\n")
}
