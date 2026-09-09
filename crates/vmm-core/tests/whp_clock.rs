//! Does a WHP guest keep time? (the Windows half of the 2026-09-09 clock
//! finding).
//!
//! `tests/boot/tests/soak.rs` measures guest clock drift on KVM and is
//! `#[cfg(target_os = "linux")]`, so for a day the honest answer to "is the
//! guest's time base sound?" existed for exactly one of the two supported
//! hosts. It mattered, because the KVM measurement had come back reading
//! −32 000 ppm and the first plausible cause — a wrong TSC frequency handed to
//! the guest — is a *hypervisor-specific* defect that WHP could not share. A
//! second host that shows the same number implicates the machine model; one
//! that does not, exonerates it. (It did not: the cause was the WSL2 host's own
//! `CLOCK_MONOTONIC`, which gains 3.3 %. See `HOST_CLOCK_SANITY_PPM` in the
//! soak.)
//!
//! What this measures is deliberately the same shape as the soak, minus the
//! leak accounting that needs hours:
//!
//! * the guest's `CLOCK_MONOTONIC` against the host's, between two heartbeats
//!   whose *arrival* was watched, so the same observation latency sits on both
//!   ends of the interval and cancels;
//! * the guest's raw TSC over the same interval, which is what would catch a
//!   frequency we advertise and do not honour;
//! * the host's clock as the **guest** reads it out of our emulated ACPI PM
//!   timer, which takes the harness's own timing out of the comparison;
//! * and the host's monotonic clock against the host's wall clock, because a
//!   measurement is worth no more than its reference — the whole point of the
//!   finding this test exists for.
//!
//! Windows is a much better instrument than WSL2 here: `Instant` is QPC, which
//! Hyper-V keeps honest, and it agreed with `SystemTime` to ~10 ppm in every
//! run of this test.
//!
//! Sixty seconds by default (`ENTANGLED_WHP_CLOCK_SECS`), which is short enough
//! to leave in the normal suite and long enough that the ±0.35 s of observation
//! error is under 6 000 ppm — well inside the 33 000 ppm a wrong TSC frequency
//! would produce, which is the defect class this is here to catch. Self-skips
//! without WHP or without the artifacts, like every test in this crate.

#![cfg(windows)]

use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use linux_boot::{BootConfig, GUEST_READY_MARKER};
use machine_x86::boot as x86_boot;
use machine_x86::bus::MachineBus;
use machine_x86::irqchip::UserspaceIrqChip;
use machine_x86::serial::SerialConsole;
use vmm_core::whp::{WhpHypervisor, WhpOptions, WhpPartition, WHP_ENABLE_HINT};
use vmm_core::RunOutcome;

mod whp_common;
use whp_common::{artifact, dump_log, kernel, tail, whp_guard, Capture, BOOT_DEADLINE, MACHINE};

/// How long the guest is watched after its first heartbeat.
const DEFAULT_SECS: u64 = 60;

/// How often the console is re-read. The whole observation error, and it sits
/// on both ends of the interval.
const OBSERVE_INTERVAL: Duration = Duration::from_millis(100);

/// Drift allowance in ppm, widened per run by the observation error below.
/// Deliberately the same number as the soak's `MAX_DRIFT_PPM`: the two hosts
/// answer to one standard.
const MAX_DRIFT_PPM: f64 = 10_000.0;

/// How far the host's own two clocks may disagree before this test refuses to
/// judge the guest by them. The soak's `HOST_CLOCK_SANITY_PPM`, for the same
/// reason and with the same story behind it.
const HOST_CLOCK_SANITY_PPM: f64 = 1_000.0;

/// One heartbeat, and when the host saw it arrive.
#[derive(Debug, Clone, Copy)]
struct Beat {
    tick: u64,
    guest_uptime_ms: u64,
    tsc: Option<u64>,
    pm_us: Option<u64>,
    seen: Instant,
    seen_real: SystemTime,
}

/// `a` against `b` in parts per million.
fn ppm(a: f64, b: f64) -> f64 {
    if b > 0.0 {
        (a - b) / b * 1e6
    } else {
        0.0
    }
}

/// The newest complete `VMHOST_HEARTBEAT <n> uptime_ms=<t> [tsc=..] [pm_us=..]`
/// line. Partial lines are skipped: a truncated number is worse than no number.
fn last_beat(text: &str) -> Option<(u64, u64, Option<u64>, Option<u64>)> {
    text.lines()
        .rev()
        .skip(usize::from(!text.ends_with('\n')))
        .find_map(|line| {
            let rest = line.trim().strip_prefix("VMHOST_HEARTBEAT ")?;
            let mut fields = rest.split_ascii_whitespace();
            let tick: u64 = fields.next()?.parse().ok()?;
            let uptime: u64 = fields.next()?.strip_prefix("uptime_ms=")?.parse().ok()?;
            let mut tsc = None;
            let mut pm_us = None;
            for field in fields {
                if let Some(v) = field.strip_prefix("tsc=") {
                    tsc = v.parse().ok();
                } else if let Some(v) = field.strip_prefix("pm_us=") {
                    pm_us = v.parse().ok();
                }
            }
            Some((tick, uptime, tsc, pm_us))
        })
}

/// Waits for the *next* heartbeat after whatever is already on the console, so
/// that the beat is dated by an arrival the host watched rather than by one it
/// found lying there. Dating a found beat "now" biases the host side of the
/// interval by up to a full observe period, which a short run then reports as
/// enormous drift; the soak learned that the expensive way.
fn next_beat(capture: &Capture, timeout: Duration) -> Option<Beat> {
    let deadline = Instant::now() + timeout;
    let already = last_beat(&capture.text()).map(|(tick, ..)| tick);
    loop {
        if let Some((tick, guest_uptime_ms, tsc, pm_us)) = last_beat(&capture.text()) {
            if Some(tick) != already {
                return Some(Beat {
                    tick,
                    guest_uptime_ms,
                    tsc,
                    pm_us,
                    seen: Instant::now(),
                    seen_real: SystemTime::now(),
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
fn a_whp_guest_keeps_the_hosts_time() {
    let _guard = whp_guard();
    let hv = match WhpHypervisor::open() {
        Ok(hv) => hv,
        Err(e) => {
            eprintln!("skipping: {e}");
            eprintln!("(to run this test: {WHP_ENABLE_HINT})");
            return;
        }
    };
    let (Some((kernel, which)), Some(initramfs)) =
        (kernel(), artifact("tests/test-initramfs.cpio.gz"))
    else {
        eprintln!(
            "skipping: test artifacts missing — build artifacts/bootstrap/vmlinuz (or run \
             scripts/fetch-test-kernel.sh) and scripts/build-test-initramfs.sh"
        );
        return;
    };
    let secs = std::env::var("ENTANGLED_WHP_CLOCK_SECS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(DEFAULT_SECS)
        .clamp(10, 24 * 3600);
    eprintln!("booting the {which} kernel and watching its clock for {secs} s");

    let mut partition = WhpPartition::with_options(&hv, &MACHINE, WhpOptions::for_guest())
        .expect("WHP partition with a local APIC");
    machine_x86::mptable::write(partition.memory(), MACHINE.vcpu_count).unwrap();
    machine_x86::acpi::write(partition.memory(), MACHINE.vcpu_count).unwrap();
    let irqchip = UserspaceIrqChip::new(partition.interrupt_delivery(), MACHINE.vcpu_count)
        .expect("userspace irqchip");

    let capture = Capture::default();
    let serial = SerialConsole::with_trigger(irqchip.serial_line(), Box::new(capture.clone()));
    let bus = MachineBus::new(serial).with_irqchip(Arc::clone(&irqchip));

    let boot = BootConfig {
        kernel,
        initramfs: Some(initramfs),
        // One heartbeat a second, for ever: the probe never returns, so the
        // host decides when the VM ends.
        cmdline: "console=ttyS0 earlyprintk=serial panic=1 reboot=k entangled.heartbeat=1000"
            .into(),
    };
    let loaded = linux_boot::load(partition.memory(), &boot, MACHINE.memory_mib << 20)
        .expect("bzImage + initramfs load");

    let mut vcpus = partition.take_vcpus();
    let vcpu = &mut vcpus[0];
    x86_boot::setup_long_mode_sregs(partition.memory(), vcpu).unwrap();
    x86_boot::setup_boot_regs(vcpu, loaded.entry, loaded.boot_params_addr).unwrap();
    let threads = vmm_core::whp::spawn_vcpus(vcpus, |_| Box::new(bus.clone())).unwrap();

    // ------------------------------------------------------------ the run
    let start = Instant::now();
    let mut ready = false;
    while start.elapsed() < BOOT_DEADLINE {
        let text = capture.text();
        if text.contains(GUEST_READY_MARKER) {
            ready = true;
            break;
        }
        if text.contains("Kernel panic - not syncing") {
            break;
        }
        std::thread::sleep(OBSERVE_INTERVAL);
    }
    // Both ends of the measured interval are transitions the host watched, and
    // the first is taken after the guest has settled rather than at the marker.
    let first = ready
        .then(|| next_beat(&capture, Duration::from_secs(30)))
        .flatten();
    if first.is_some() {
        std::thread::sleep(Duration::from_secs(secs));
    }
    let last = first
        .and_then(|_| next_beat(&capture, Duration::from_secs(30)))
        .filter(|beat| Some(beat.tick) != first.map(|b| b.tick));

    let outcomes = threads.stop();
    let text = capture.text();
    dump_log(&text);

    assert!(
        ready,
        "the guest never reached {GUEST_READY_MARKER}:\n{}",
        tail(&text, 40)
    );
    let (Some(first), Some(last)) = (first, last) else {
        panic!(
            "the guest produced no pair of heartbeats to compare:\n{}",
            tail(&text, 40)
        );
    };

    // ------------------------------------------------------------- verdicts
    let host_ms = last.seen.duration_since(first.seen).as_secs_f64() * 1000.0;
    let guest_ms = last.guest_uptime_ms.saturating_sub(first.guest_uptime_ms) as f64;
    let drift_ppm = ppm(guest_ms, host_ms);
    // Two observe intervals: one on each end of the interval, and nothing else
    // between the guest's print and the host's reading of it.
    let drift_err_ppm = ppm(
        host_ms + 2.0 * OBSERVE_INTERVAL.as_secs_f64() * 1000.0,
        host_ms,
    );
    let host_real_ms = last
        .seen_real
        .duration_since(first.seen_real)
        .ok()
        .map(|d| d.as_secs_f64() * 1000.0)
        .filter(|ms| *ms > 0.0);
    let host_ref_ppm = host_real_ms.map(|real| ppm(host_ms, real));
    let host_clock_trustworthy = host_ref_ppm.is_none_or(|p| p.abs() <= HOST_CLOCK_SANITY_PPM);
    let drift_real_ppm = host_real_ms.map(|real| ppm(guest_ms, real));
    let pm_ms = last
        .pm_us
        .zip(first.pm_us)
        .map(|(now, base)| now.saturating_sub(base) as f64 / 1000.0);
    let tsc_mhz = last
        .tsc
        .zip(first.tsc)
        .map(|(now, base)| now.wrapping_sub(base) as f64 / host_ms.max(f64::EPSILON) / 1000.0);
    let show = |v: Option<f64>, unit: &str| match v {
        Some(v) => format!("{v:+.0} {unit}"),
        None => "n/a".to_string(),
    };

    eprintln!(
        "\n---- whp guest clock ------------------------------------------------\n\
         interval          {:.0} ms host, {:.0} ms guest, ticks {} -> {}\n\
         drift             {drift_ppm:+.0} ppm (+-{drift_err_ppm:.0} ppm observation error)\n\
         cross-check       guest vs the host clock it read itself {}, that reading vs the \
         host's own {}\n\
         host reference    monotonic vs wall clock {}, guest vs the host's wall clock {}\n\
         guest TSC         {}\n\
         clocksource       {}\n\
         ---------------------------------------------------------------------",
        host_ms,
        guest_ms,
        first.tick,
        last.tick,
        show(pm_ms.map(|pm| ppm(guest_ms, pm)), "ppm"),
        show(pm_ms.map(|pm| ppm(pm, host_ms)), "ppm"),
        show(host_ref_ppm, "ppm"),
        show(drift_real_ppm, "ppm"),
        match tsc_mhz {
            Some(v) => format!("{v:.3} MHz measured against host time"),
            None => "n/a (this initramfs predates the three-clock probe)".to_string(),
        },
        clocksource(&text).unwrap_or_else(|| "unknown".to_string()),
    );

    // A host whose own two clocks disagree cannot judge the guest; say so and
    // measure against the wall clock instead of blaming the VM. Windows has
    // never needed this — the branch exists because WSL2 did, and a test that
    // silently reports someone else's broken clock as our defect is the exact
    // failure this whole investigation was.
    let (judged_ppm, reference) = match (host_clock_trustworthy, drift_real_ppm) {
        (true, _) | (false, None) => (drift_ppm, "the host's monotonic clock"),
        (false, Some(real)) => (real, "the host's wall clock"),
    };
    if !host_clock_trustworthy {
        eprintln!(
            "note: this host's CLOCK_MONOTONIC disagrees with its own wall clock by {}; the \
             guest was judged against the wall clock",
            show(host_ref_ppm, "ppm")
        );
    }
    assert!(
        judged_ppm.abs() <= MAX_DRIFT_PPM + drift_err_ppm,
        "the guest's clock drifted {judged_ppm:+.0} ppm against {reference} over {:.0} s, beyond \
         the {MAX_DRIFT_PPM:.0} ppm allowance and the {drift_err_ppm:.0} ppm this run could not \
         see",
        host_ms / 1000.0
    );
    // A stopped clock is not slow, and the drift ratio would not catch it: the
    // guest has to have actually counted the beats.
    assert!(
        last.tick > first.tick,
        "the guest's heartbeat did not advance at all"
    );
    for outcome in &outcomes {
        assert!(
            matches!(outcome, Ok(RunOutcome::Stopped) | Ok(RunOutcome::Halted)),
            "a vCPU ended the run with {outcome:?}"
        );
    }
}

/// The clocksource the guest kernel settled on, from its own boot log. On WHP
/// there is no kvmclock to leave, so this is the whole story of what it read.
fn clocksource(console: &str) -> Option<String> {
    console
        .lines()
        .filter_map(|line| line.split_once("Switched to clocksource "))
        .map(|(_, name)| name.trim().to_string())
        .next_back()
}
