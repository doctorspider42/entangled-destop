//! The 8254 under WHP, booted many times (ADR-0002, 2026-09-26 amendment).
//!
//! Linux's `check_timer()` needs five timer interrupts inside `40e9 / HZ` TSC
//! cycles — 11.4 ms at HZ=1000 on a 3.5 GHz part. On WHP every one of them is
//! raised by the userspace 8254's host thread and injected through the
//! userspace IOAPIC, so a host that is slow to wake that thread, or that
//! deschedules the vCPU, made an installed 8-vCPU Ubuntu panic with
//! `IO-APIC + timer doesn't work!` in 2 of 8 boots, and a loaded host far more
//! often. One boot proves nothing about that; this boots the same machine
//! over and over, optionally with every host core kept busy, and counts.
//!
//! What it asserts, per boot: no `..MP-BIOS bug: 8254 timer not connected to
//! IO-APIC` (the first `timer_irq_works()` failing), no panic, the ready
//! marker, and every processor up. What it *reports*: how the guest calibrated
//! its TSC. On an Intel host CPUID leaf `0x15` tells Linux the frequency and
//! no PIT calibration may happen at all, so that is asserted too; on AMD Linux
//! always calibrates (`native_calibrate_tsc()` reads `0x15` on Intel only), the
//! PIT half of it can fail on exit latency alone, and the PM-timer fallback it
//! then uses is accurate — so the counts are printed, not asserted.
//!
//! `#[ignore]`d: it takes minutes. Knobs, all optional:
//!
//! | Variable | Default | Meaning |
//! |---|---|---|
//! | `ENTANGLED_TIMER_BOOTS` | 10 | boots |
//! | `ENTANGLED_TIMER_VCPUS` | 8 | processors per boot |
//! | `ENTANGLED_TIMER_BUSY` | 0 | host threads spinning for the whole run (`all` = one per logical CPU) |
//! | `ENTANGLED_TIMER_KERNEL` | the test kernel | a different bzImage, e.g. Ubuntu's HZ=1000 `casper/vmlinuz` |
//! | `ENTANGLED_TIMER_LOG_DIR` | none | writes every boot's serial log there as `boot-N.log` |
//!
//! ```powershell
//! $env:ENTANGLED_TIMER_BUSY = "all"; $env:ENTANGLED_TIMER_KERNEL = "F:\ubuntu-vmlinuz"
//! cargo test --release -p vmm-core --test whp_timer -- --ignored --nocapture
//! ```
//!
//! Measured with Ubuntu 26.04's 7.0 kernel, 8 vCPUs and 24 busy threads on a
//! 24-thread host: before the fix 9 of 20 boots failed `check_timer()` and 8
//! panicked; after it, none of 20.

#![cfg(windows)]

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use linux_boot::{BootConfig, GUEST_READY_MARKER};
use machine_x86::boot as x86_boot;
use machine_x86::bus::MachineBus;
use machine_x86::irqchip::UserspaceIrqChip;
use machine_x86::serial::SerialConsole;
use vmm_core::whp::{WhpHypervisor, WhpOptions, WhpPartition, WHP_ENABLE_HINT};
use vmm_core::MachineConfig;

mod whp_common;
use whp_common::{artifact, kernel, tail, whp_guard, Capture};

/// `check_timer()`'s first `timer_irq_works()` failed. Linux may still rescue
/// the boot on its second attempt (the same IOAPIC pin), which is exactly how
/// the bug hid in six of eight boots — so this counts as a failure on its own.
const CHECK_TIMER_FAILED: &str = "8254 timer not connected to IO-APIC";
const TIMER_PANIC: &str = "IO-APIC + timer doesn't work";
const ANY_PANIC: &str = "Kernel panic - not syncing";
const PIT_CALIBRATION_FAILED: &str = "tsc: Unable to calibrate against PIT";
const PIT_CALIBRATION_OK: &str = "tsc: PIT calibration matches";
const FAST_CALIBRATION_OK: &str = "tsc: Fast TSC calibration using PIT";
/// The PM-timer reference failed instead, and Linux kept the PIT's figure.
const REFERENCE_FAILED: &str = "tsc: HPET/PMTIMER calibration failed";

/// One boot's deadline. The guest reaches the marker in ~20 s with every host
/// core busy; a boot that has not after three minutes is stuck.
const DEADLINE: Duration = Duration::from_secs(180);

fn env_u32(name: &str, default: u32) -> u32 {
    std::env::var(name)
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(default)
}

fn busy_threads() -> usize {
    match std::env::var("ENTANGLED_TIMER_BUSY").as_deref() {
        Ok("all") => std::thread::available_parallelism().map_or(4, |n| n.get()),
        Ok(value) => value.parse().unwrap_or(0),
        Err(_) => 0,
    }
}

/// Spinning host threads, stopped and joined on drop.
struct HostLoad {
    stop: Arc<AtomicBool>,
    threads: Vec<std::thread::JoinHandle<()>>,
}

impl HostLoad {
    fn start(count: usize) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let threads = (0..count)
            .map(|_| {
                let stop = Arc::clone(&stop);
                std::thread::spawn(move || {
                    let mut x = 0u64;
                    while !stop.load(Ordering::Relaxed) {
                        x = std::hint::black_box(x.wrapping_add(1));
                    }
                })
            })
            .collect();
        Self { stop, threads }
    }
}

impl Drop for HostLoad {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        for thread in self.threads.drain(..) {
            let _ = thread.join();
        }
    }
}

#[derive(Debug, Default)]
struct Tally {
    boots: u32,
    check_timer_failed: u32,
    panicked: u32,
    not_ready: u32,
    not_smp: u32,
    pit_calibration_failed: u32,
    pit_calibration_ok: u32,
    fast_calibration_ok: u32,
    pm_timer_reference_failed: u32,
    no_calibration: u32,
}

#[test]
#[ignore = "boots the guest ENTANGLED_TIMER_BOOTS times; run with --ignored"]
fn smp_boots_never_lose_the_timer() {
    let _guard = whp_guard();
    let hv = match WhpHypervisor::open() {
        Ok(hv) => hv,
        Err(e) => {
            eprintln!("skipping: {e}");
            eprintln!("(to run this test: {WHP_ENABLE_HINT})");
            return;
        }
    };
    let kernel = match std::env::var_os("ENTANGLED_TIMER_KERNEL") {
        Some(path) => Some((PathBuf::from(path), "ENTANGLED_TIMER_KERNEL")),
        None => kernel(),
    };
    let (Some((kernel, which)), Some(initramfs)) =
        (kernel, artifact("tests/test-initramfs.cpio.gz"))
    else {
        eprintln!("skipping: test artifacts missing");
        return;
    };
    let boots = env_u32("ENTANGLED_TIMER_BOOTS", 10);
    let vcpus = env_u32("ENTANGLED_TIMER_VCPUS", 8);
    let busy = busy_threads();
    let intel = hv.capabilities().processor_vendor == Some("Intel");
    eprintln!(
        "{boots} boots of the {which} kernel, {vcpus} vCPUs, {busy} busy host threads, \
         {} host",
        hv.capabilities().processor_vendor.unwrap_or("unknown")
    );

    let _load = HostLoad::start(busy);
    let mut tally = Tally::default();
    let mut failures = Vec::new();
    let log_dir = std::env::var_os("ENTANGLED_TIMER_LOG_DIR").map(PathBuf::from);
    for boot in 1..=boots {
        let text = boot_once(&hv, &kernel, &initramfs, vcpus);
        if let Some(dir) = &log_dir {
            let path = dir.join(format!("boot-{boot}.log"));
            if let Err(e) = std::fs::create_dir_all(dir).and_then(|()| std::fs::write(&path, &text))
            {
                eprintln!("could not write {}: {e}", path.display());
            }
        }
        tally.boots += 1;
        let failed_check = text.contains(CHECK_TIMER_FAILED);
        let panicked = text.contains(ANY_PANIC);
        let ready = text.contains(GUEST_READY_MARKER);
        let smp =
            text.contains(&format!("smpboot: Total of {vcpus} processors activated")) || vcpus == 1;
        tally.check_timer_failed += u32::from(failed_check);
        tally.panicked += u32::from(panicked);
        tally.not_ready += u32::from(!ready);
        tally.not_smp += u32::from(!smp);
        let pit_failed = text.contains(PIT_CALIBRATION_FAILED);
        let pit_ok = text.contains(PIT_CALIBRATION_OK);
        let fast_ok = text.contains(FAST_CALIBRATION_OK);
        let reference_failed = text.contains(REFERENCE_FAILED);
        let calibrated = pit_failed || pit_ok || fast_ok || reference_failed;
        tally.pit_calibration_failed += u32::from(pit_failed);
        tally.pit_calibration_ok += u32::from(pit_ok);
        tally.fast_calibration_ok += u32::from(fast_ok);
        tally.pm_timer_reference_failed += u32::from(reference_failed);
        tally.no_calibration += u32::from(!calibrated);
        eprintln!(
            "boot {boot}: check_timer {} panic {} ready {} smp {} calibration {}",
            if failed_check { "FAILED" } else { "ok" },
            panicked,
            ready,
            smp,
            if pit_failed {
                "PIT failed, PM timer used"
            } else if pit_ok {
                "PIT matches PM timer"
            } else if fast_ok {
                "fast PIT"
            } else if reference_failed {
                "PM timer reference failed, PIT used"
            } else {
                "none (frequency known)"
            }
        );
        if failed_check || panicked || !ready || !smp {
            let text = text.trim_end();
            failures.push(format!(
                "boot {boot} ({} bytes of serial log):\n{}",
                text.len(),
                tail(text, 30)
            ));
        }
    }
    eprintln!("{tally:#?}");
    assert!(
        failures.is_empty(),
        "{} of {boots} boots lost the timer or failed to boot ({tally:?}):\n{}",
        failures.len(),
        failures.join("\n---\n")
    );
    if intel {
        assert_eq!(
            tally.no_calibration, tally.boots,
            "an Intel host tells the guest its TSC frequency in leaf 0x15; \
             no boot should have calibrated against the PIT ({tally:?})"
        );
    }
}

/// Boots the machine once and returns its serial log, ending the VM as soon
/// as the outcome is known.
fn boot_once(
    hv: &WhpHypervisor,
    kernel: &std::path::Path,
    initramfs: &std::path::Path,
    vcpus: u32,
) -> String {
    let machine = MachineConfig {
        memory_mib: 512,
        vcpu_count: vcpus,
    };
    let mut partition = WhpPartition::with_options(hv, &machine, WhpOptions::for_guest())
        .expect("WHP partition with a local APIC");
    machine_x86::mptable::write(partition.memory(), vcpus).unwrap();
    machine_x86::acpi::write(partition.memory(), vcpus).unwrap();
    let irqchip =
        UserspaceIrqChip::new(partition.interrupt_delivery(), vcpus).expect("userspace irqchip");
    let capture = Capture::default();
    let serial = SerialConsole::with_trigger(irqchip.serial_line(), Box::new(capture.clone()));
    let bus = MachineBus::new(serial).with_irqchip(Arc::clone(&irqchip));
    let boot = BootConfig {
        kernel: kernel.to_path_buf(),
        initramfs: Some(initramfs.to_path_buf()),
        // `panic=0`: a panicking guest stays down, so it is seen and counted
        // rather than rebooting into a second, different boot.
        cmdline: "console=ttyS0 earlyprintk=serial panic=0 reboot=k apic=verbose".into(),
    };
    let loaded = linux_boot::load(partition.memory(), &boot, machine.memory_mib << 20)
        .expect("bzImage + initramfs load");
    let mut vcpu_handles = partition.take_vcpus();
    {
        let bsp = &mut vcpu_handles[0];
        x86_boot::setup_long_mode_sregs(partition.memory(), bsp).unwrap();
        x86_boot::setup_boot_regs(bsp, loaded.entry, loaded.boot_params_addr).unwrap();
    }
    let threads = vmm_core::whp::spawn_vcpus(vcpu_handles, |_| Box::new(bus.clone())).unwrap();
    let start = Instant::now();
    while start.elapsed() < DEADLINE {
        let text = capture.text();
        if text.contains(GUEST_READY_MARKER)
            || text.contains(ANY_PANIC)
            || text.contains(TIMER_PANIC)
        {
            // Give a panic's backtrace a moment to reach the log.
            std::thread::sleep(Duration::from_millis(200));
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let _ = threads.stop();
    let text = capture.text();
    drop(partition);
    drop(irqchip);
    text
}
