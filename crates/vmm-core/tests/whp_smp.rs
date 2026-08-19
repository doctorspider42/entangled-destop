//! SMP on WHP (backlog WHP-1703): a two-vCPU Linux guest boots natively on
//! Windows and brings its application processor up.
//!
//! The evidence is the kernel's own line, `smpboot: Total of 2 processors
//! activated`, which it only prints once the AP has executed the real-mode
//! trampoline, switched to long mode, joined the scheduler and answered the BSP's
//! synchronisation. A guest whose AP never starts does not fail — it boots
//! happily on one CPU — so this has to be asserted by naming the line.
//!
//! The interesting result is that the WHP backend needs no INIT/SIPI code for
//! this: WHP's own xAPIC models the wait-for-startup state, so every AP blocks
//! inside `WHvRunVirtualProcessor` until the guest wakes it. Turning on
//! `X64ApicInitSipiExitTrap` actively breaks it. `vmm_core::whp::WhpPartition`
//! carries the full account.
//!
//! # Why only the bootstrap processor gets long-mode registers
//!
//! On KVM the harness hands every vCPU `setup_long_mode_sregs`, because the
//! in-kernel APIC resets an AP on INIT and the state is thrown away anyway. On WHP
//! an AP must be left in the reset state WHP created it in, so a `CS` from the
//! SIPI is not paired with long-mode control registers the trampoline is about to
//! set up itself.

#![cfg(windows)]

use std::sync::Arc;
use std::time::{Duration, Instant};

use linux_boot::{BootConfig, GUEST_READY_MARKER};
use machine_x86::boot as x86_boot;
use machine_x86::bus::MachineBus;
use machine_x86::irqchip::UserspaceIrqChip;
use machine_x86::serial::SerialConsole;
use vmm_core::whp::{WhpHypervisor, WhpOptions, WhpPartition, WHP_ENABLE_HINT};
use vmm_core::{MachineConfig, RunOutcome};

mod whp_common;
use whp_common::{artifact, dump_log, kernel, tail, whp_guard, Capture, BOOT_DEADLINE};

const SMP_MACHINE: MachineConfig = MachineConfig {
    memory_mib: 512,
    vcpu_count: 2,
};

/// What the kernel prints once the AP is up and counted.
const SMP_MARKER: &str = "smpboot: Total of 2 processors activated";

/// What it prints instead when the AP never answered, after a ten-second wait.
const AP_TIMEOUT_MARKER: &str = "failed to report alive state";

/// EPIC 17 phase 3 acceptance: a 2-vCPU Windows boot reaches the ready marker
/// with both processors online.
#[test]
fn two_processors_come_up_on_whp() {
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
        eprintln!("skipping: test artifacts missing");
        return;
    };
    eprintln!("booting the {which} kernel with 2 vCPUs");

    let mut partition = WhpPartition::with_options(&hv, &SMP_MACHINE, WhpOptions::for_guest())
        .expect("WHP partition with a local APIC");
    // Both tables describe two CPUs, which is what makes the kernel look for an
    // AP in the first place.
    machine_x86::mptable::write(partition.memory(), SMP_MACHINE.vcpu_count).unwrap();
    machine_x86::acpi::write(partition.memory(), SMP_MACHINE.vcpu_count).unwrap();

    let irqchip = UserspaceIrqChip::new(partition.interrupt_delivery(), SMP_MACHINE.vcpu_count)
        .expect("userspace irqchip");
    let capture = Capture::default();
    let serial = SerialConsole::with_trigger(irqchip.serial_line(), Box::new(capture.clone()));
    let bus = MachineBus::new(serial).with_irqchip(Arc::clone(&irqchip));

    let boot = BootConfig {
        kernel,
        initramfs: Some(initramfs),
        cmdline: "console=ttyS0 earlyprintk=serial panic=1 reboot=k".into(),
    };
    let loaded = linux_boot::load(partition.memory(), &boot, SMP_MACHINE.memory_mib << 20)
        .expect("bzImage + initramfs load");

    let mut vcpus = partition.take_vcpus();
    assert_eq!(vcpus.len(), 2);
    // Only the BSP; see the module docs.
    {
        let bsp = &mut vcpus[0];
        x86_boot::setup_long_mode_sregs(partition.memory(), bsp).unwrap();
        x86_boot::setup_boot_regs(bsp, loaded.entry, loaded.boot_params_addr).unwrap();
    }
    let threads = vmm_core::whp::spawn_vcpus(vcpus, |_| Box::new(bus.clone())).unwrap();

    let start = Instant::now();
    let mut ready = false;
    let mut smp = false;
    let mut panicked = false;
    while start.elapsed() < BOOT_DEADLINE {
        let text = capture.text();
        ready |= text.contains(GUEST_READY_MARKER);
        smp |= text.contains(SMP_MARKER);
        // The kernel gives an AP ten seconds and then carries on with one CPU, so
        // its own complaint ends the wait too — there is nothing more to learn by
        // sitting out the deadline.
        if ready && (smp || text.contains(AP_TIMEOUT_MARKER)) {
            break;
        }
        if text.contains("Kernel panic - not syncing") {
            panicked = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let state: Vec<String> = (0..partition.vcpu_count())
        .map(|vp| {
            partition
                .processor_summary(vp)
                .unwrap_or_else(|e| format!("vp{vp}: unreadable ({e})"))
        })
        .collect();
    let outcomes = threads.stop();
    let text = capture.text();
    dump_log(&text);

    eprintln!(
        "reached ready in {:?}; pit edges: {}, ioapic messages: {}; \
         vcpu outcomes: {outcomes:?}",
        start.elapsed(),
        irqchip.pit().edges(),
        irqchip.ioapic().delivered()
    );
    for line in &state {
        eprintln!("  {line}");
    }
    assert!(
        !panicked,
        "the guest panicked; serial tail:\n{}",
        tail(&text, 40)
    );
    assert!(
        ready,
        "no {GUEST_READY_MARKER} within {BOOT_DEADLINE:?}; serial tail:\n{}",
        tail(&text, 60)
    );
    // The failure this asserts against is not a crash: a guest whose AP never
    // starts boots fine on one CPU. So the line has to be named literally, and
    // the kernel's own complaint quoted back when it is missing.
    assert!(
        smp,
        "the application processor never came up (looking for {SMP_MARKER:?}); \
         processor state was:\n  {}\nserial tail:\n{}",
        state.join("\n  "),
        tail(&text, 60)
    );
    assert!(
        !text.contains("Not responding"),
        "the kernel timed out waiting for a processor; serial tail:\n{}",
        tail(&text, 40)
    );

    // Both vCPU threads must end cleanly — including the AP, whose thread is
    // blocked *inside* `WHvRunVirtualProcessor` until it is cancelled.
    assert_eq!(outcomes.len(), 2);
    for (index, outcome) in outcomes.into_iter().enumerate() {
        let outcome = outcome.unwrap_or_else(|e| panic!("vcpu {index} failed: {e}"));
        assert!(
            matches!(outcome, RunOutcome::Stopped | RunOutcome::Shutdown),
            "vcpu {index}: unexpected outcome {outcome:?}"
        );
    }
    drop(partition);
    drop(irqchip);
}
