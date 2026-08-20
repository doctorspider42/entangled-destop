//! Pause, resume and reset of a running VM on KVM
//! ([ADR-0005](../../../docs/adr/0005-vm-lifecycle.md)).
//!
//! The Linux half of the lifecycle acceptance; `crates/vmm-core/tests/whp_lifecycle.rs`
//! is the same three properties on Windows. What is asserted here is behaviour a
//! guest can see, not host bookkeeping:
//!
//! * **pause** — the guest stops making progress. Measured on the serial
//!   console, which the test guest drives from a real timer loop, so a stalled
//!   console means stalled guest code and not merely a stalled device.
//! * **resume** — it picks up where it was, in the same boot: the ready marker
//!   is not printed again.
//! * **reset** — the machine reboots in place. The marker *is* printed again,
//!   the run is still one process, and it happens twice in a row.
//! * **a guest-initiated reboot** — the same, but asked for by the guest.
//!   `reboot=k` makes Linux pulse the keyboard controller's reset line, which is
//!   one rung of the reboot ladder `machine_x86::reset` implements.
//!
//! Self-skips without `/dev/kvm` or the guest artifacts, like every other test
//! in this crate.

#![cfg(target_os = "linux")]

use std::time::{Duration, Instant};

use boot_tests::{boot_artifacts, boot_once_driven, kvm_available, BootSpec, VmHandle};
use linux_boot::GUEST_READY_MARKER;
use vmm_core::RunState;

/// How long a lifecycle operation and the boot that follows it may take. The
/// bootstrap kernel reaches its marker in ~2 s on this machine; a debug-build
/// reset adds the device sweep and one kernel load.
const STEP: Duration = Duration::from_secs(45);

/// How long a paused VM is watched for signs of life. The test guest prints a
/// heartbeat every 100 ms, so this is ~5 heartbeats' worth of silence.
const FREEZE_WATCH: Duration = Duration::from_millis(500);

fn spec() -> Option<BootSpec> {
    if !kvm_available() {
        return None;
    }
    let (kernel, initramfs) = boot_artifacts()?;
    // `entangled.heartbeat` keeps the guest printing after the ready marker,
    // which is what makes "the guest stopped making progress" observable from
    // the host without a debugger.
    Some(
        BootSpec::new(kernel, initramfs)
            .with_extra_cmdline("entangled.heartbeat=100")
            .with_vcpus(1),
    )
}

/// A paused VM stops making progress, and a resumed one continues the *same*
/// boot rather than starting a new one.
#[test]
fn pause_freezes_the_guest_and_resume_continues_the_same_boot() {
    let Some(spec) = spec() else { return };
    let outcome = boot_once_driven(
        &spec,
        Some(Box::new(|vm: VmHandle| {
            // Wait until the guest is definitely executing: the marker, then a
            // few heartbeats after it.
            if vm.wait_for(GUEST_READY_MARKER, 1, STEP) == 0 {
                vm.finish();
                return;
            }
            if vm.wait_for("VMHOST_HEARTBEAT", 3, STEP) < 3 {
                vm.finish();
                return;
            }

            let paused_at = Instant::now();
            vm.lifecycle.pause().expect("pause");
            assert_eq!(vm.lifecycle.state(), RunState::Paused);
            let acknowledged = paused_at.elapsed();

            // Let whatever was already in flight land, then take the reading
            // that matters.
            std::thread::sleep(Duration::from_millis(50));
            let frozen = vm.count("VMHOST_HEARTBEAT");
            let frozen_len = vm.serial().len();
            std::thread::sleep(FREEZE_WATCH);
            assert_eq!(
                vm.count("VMHOST_HEARTBEAT"),
                frozen,
                "the guest kept printing heartbeats while paused"
            );
            assert_eq!(
                vm.serial().len(),
                frozen_len,
                "the guest wrote to the console while paused"
            );

            vm.lifecycle.resume().expect("resume");
            assert_eq!(vm.lifecycle.state(), RunState::Running);
            let resumed = vm.wait_for("VMHOST_HEARTBEAT", frozen + 3, STEP);
            assert!(
                resumed >= frozen + 3,
                "the guest did not resume: {resumed} heartbeats, expected {}",
                frozen + 3
            );
            // The same boot, not a new one.
            assert_eq!(
                vm.count(GUEST_READY_MARKER),
                1,
                "resume restarted the guest instead of continuing it"
            );
            assert_eq!(vm.lifecycle.resets(), 0);
            eprintln!(
                "paused in {acknowledged:?}, {frozen} heartbeats before, {resumed} after resume"
            );
            vm.finish();
        })),
    )
    .expect("boot");
    assert!(
        outcome.reached_ready(),
        "guest never became ready; serial:\n{}",
        outcome.serial
    );
}

/// A host-initiated reset reboots the machine in place: the guest starts over,
/// in the same process, twice in a row.
#[test]
fn a_host_reset_reboots_the_guest_in_place_twice() {
    let Some(spec) = spec() else { return };
    let outcome = boot_once_driven(
        &spec,
        Some(Box::new(|vm: VmHandle| {
            for round in 1..=2u64 {
                if vm.wait_for(GUEST_READY_MARKER, round as usize, STEP) < round as usize {
                    eprintln!("boot {round} never became ready");
                    vm.finish();
                    return;
                }
                let started = Instant::now();
                vm.lifecycle.reset().expect("reset");
                assert_eq!(vm.lifecycle.state(), RunState::Running);
                assert_eq!(vm.lifecycle.resets(), round);
                eprintln!("reset {round} acknowledged in {:?}", started.elapsed());
            }
            let seen = vm.wait_for(GUEST_READY_MARKER, 3, STEP);
            assert_eq!(
                seen, 3,
                "expected three boots (one plus two resets), saw {seen}"
            );
            vm.finish();
        })),
    )
    .expect("boot");
    assert!(
        outcome.serial.matches(GUEST_READY_MARKER).count() >= 3,
        "serial tail:\n{}",
        tail(&outcome.serial, 40)
    );
}

/// The guest asks for it: `reboot=k` pulses the keyboard controller's reset
/// line, which `machine_x86::reset` latches and the supervisor serves. The test
/// guest reboots itself after its probe, so this needs no host action at all —
/// only that the VM comes back rather than ending.
#[test]
fn a_guest_initiated_reboot_comes_back_twice() {
    let Some(spec) = spec() else { return };
    // No heartbeat: this guest is meant to reach the marker and immediately
    // reboot itself, as the default test initramfs does.
    let spec = BootSpec {
        extra_cmdline: String::new(),
        ..spec
    };
    let outcome = boot_once_driven(
        &spec,
        Some(Box::new(|vm: VmHandle| {
            // Three markers: the first boot plus the two reboots the guest asked
            // for. Nothing here touches the lifecycle — the guest drives it.
            let seen = vm.wait_for(GUEST_READY_MARKER, 3, STEP);
            assert!(
                seen >= 3,
                "the guest did not come back from its own reboot: {seen} boots"
            );
            assert!(
                vm.lifecycle.resets() >= 2,
                "resets: {}",
                vm.lifecycle.resets()
            );
            vm.finish();
        })),
    )
    .expect("boot");
    let boots = outcome.serial.matches(GUEST_READY_MARKER).count();
    assert!(
        boots >= 3,
        "only {boots} boots; serial tail:\n{}",
        tail(&outcome.serial, 40)
    );
    // Still one VM: the vCPU never left its run loop for a shutdown.
    assert!(
        !outcome.ended_by_guest(),
        "the guest's reboot ended the VM instead of restarting it: {:?}",
        outcome.vcpu_outcomes
    );
}

fn tail(text: &str, lines: usize) -> String {
    let all: Vec<&str> = text.lines().collect();
    all[all.len().saturating_sub(lines)..].join("\n")
}
