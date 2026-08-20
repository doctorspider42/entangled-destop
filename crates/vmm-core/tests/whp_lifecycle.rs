//! Pause, resume and reset of a running VM on WHP
//! ([ADR-0005](../../../docs/adr/0005-vm-lifecycle.md)).
//!
//! The Windows half of the lifecycle acceptance; `tests/boot/tests/lifecycle.rs`
//! asserts the same three properties on KVM, through the same seam. Everything
//! above `vmm_core::lifecycle` is shared code, so what this file is really for
//! is the two places where the backends genuinely differ:
//!
//! * **the reset itself.** KVM writes the architectural state back by hand;
//!   WHP deletes the virtual processor and creates it again, because a VP that
//!   `WHvCreateVirtualProcessor` just made *is* in the reset state, including
//!   an application processor's wait-for-startup suspension — which ADR-0002
//!   phase 4 says a host must not disturb by any other means.
//! * **how a guest reboot arrives.** On KVM a triple fault reaches the run loop
//!   as `KVM_EXIT_SHUTDOWN`; on WHP, with local APIC emulation on, it is
//!   absorbed and the VP parks. So on this host a reboot has to come through a
//!   *device* — the reset control register or the keyboard-controller pulse —
//!   which is exactly what `machine_x86::reset` implements and what this test
//!   proves reaches the host. The guest is booted with `reboot=k`, which is the
//!   ending every other WHP test has to avoid for that very reason.
//!
//! Needs the same artifacts as `whp_boot.rs` and self-skips without them, or
//! without the Windows Hypervisor Platform feature.

#![cfg(windows)]

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use linux_boot::{BootConfig, GUEST_READY_MARKER};
use machine_x86::boot as x86_boot;
use machine_x86::bus::MachineBus;
use machine_x86::irqchip::UserspaceIrqChip;
use machine_x86::serial::SerialConsole;
use virtio_core::Quiesce;
use vmm_core::hv::VcpuRegisters;
use vmm_core::whp::{spawn_vcpus_with, WhpHypervisor, WhpOptions, WhpPartition, WHP_ENABLE_HINT};
use vmm_core::{Lifecycle, MachineLifecycle, RunState};

mod whp_common;
use whp_common::{artifact, dump_log, kernel, tail, whp_guard, Capture, BOOT_DEADLINE, MACHINE};

/// How long a lifecycle operation and the boot after it may take. Generous for
/// the same reason `BOOT_DEADLINE` is: every exit on this host goes through
/// userspace, in a debug build.
const STEP: Duration = Duration::from_secs(120);

/// How long a paused VM is watched for signs of life.
const FREEZE_WATCH: Duration = Duration::from_millis(750);

/// Everything one of these tests needs to run a VM and drive it.
struct Fixture {
    partition: WhpPartition,
    irqchip: Arc<UserspaceIrqChip>,
    capture: Capture,
    lifecycle: Arc<Lifecycle>,
    threads: vmm_core::whp::WhpVcpuThreads,
}

/// The machine behind the seam: the WHP peer of the KVM harness's
/// `TestMachine`, and a third implementation of `MachineLifecycle` after that
/// one and `entangled run`'s.
struct WhpMachine {
    bus: MachineBus,
    mem: Arc<vmm_core::GuestMem>,
    quiesce: Arc<Quiesce>,
    vcpus: u32,
    boot: BootConfig,
    mem_size: u64,
    entry: Mutex<(u64, u64)>,
}

impl MachineLifecycle for WhpMachine {
    fn quiesce(&self) {
        self.quiesce.pause();
        self.bus.set_paused(true);
        self.quiesce.wait_until_idle(Duration::from_secs(5));
    }

    fn unquiesce(&self) {
        self.bus.set_paused(false);
        self.quiesce.resume();
    }

    fn reset_machine(&self) -> Result<(), String> {
        self.bus.reset_devices();
        machine_x86::mptable::write(self.mem.as_ref(), self.vcpus).map_err(|e| e.to_string())?;
        machine_x86::acpi::write(self.mem.as_ref(), self.vcpus).map_err(|e| e.to_string())?;
        let loaded = linux_boot::load(self.mem.as_ref(), &self.boot, self.mem_size)
            .map_err(|e| e.to_string())?;
        match self.entry.lock() {
            Ok(mut slot) => *slot = (loaded.entry, loaded.boot_params_addr),
            Err(poisoned) => *poisoned.into_inner() = (loaded.entry, loaded.boot_params_addr),
        }
        Ok(())
    }

    /// Boot CPU only — see the module docs, and `ADR-0002` phase 4.
    fn reset_vcpu(&self, index: u32, vcpu: &dyn VcpuRegisters) -> Result<(), String> {
        if index != 0 {
            return Ok(());
        }
        let (entry, boot_params) = match self.entry.lock() {
            Ok(slot) => *slot,
            Err(poisoned) => *poisoned.into_inner(),
        };
        x86_boot::setup_long_mode_sregs(self.mem.as_ref(), vcpu).map_err(|e| e.to_string())?;
        x86_boot::setup_boot_regs(vcpu, entry, boot_params).map_err(|e| e.to_string())
    }
}

/// Builds and starts a VM with the lifecycle seam attached, or `None` when this
/// machine cannot run one (no WHP, no artifacts).
fn start(extra_cmdline: &str) -> Option<Fixture> {
    let hv = match WhpHypervisor::open() {
        Ok(hv) => hv,
        Err(e) => {
            eprintln!("skipping: {e}");
            eprintln!("(to run this test: {WHP_ENABLE_HINT})");
            return None;
        }
    };
    let (Some((kernel, which)), Some(initramfs)) =
        (kernel(), artifact("tests/test-initramfs.cpio.gz"))
    else {
        eprintln!(
            "skipping: test artifacts missing — build artifacts/bootstrap/vmlinuz (or run \
             scripts/fetch-test-kernel.sh) and scripts/build-test-initramfs.sh"
        );
        return None;
    };
    eprintln!("booting the {which} kernel: {}", kernel.display());

    let mut partition = WhpPartition::with_options(&hv, &MACHINE, WhpOptions::for_guest())
        .expect("WHP partition with a local APIC");
    machine_x86::mptable::write(partition.memory(), MACHINE.vcpu_count).unwrap();
    machine_x86::acpi::write(partition.memory(), MACHINE.vcpu_count).unwrap();

    let irqchip = UserspaceIrqChip::new(partition.interrupt_delivery(), MACHINE.vcpu_count)
        .expect("userspace irqchip");
    let capture = Capture::default();
    let serial = SerialConsole::with_trigger(irqchip.serial_line(), Box::new(capture.clone()));
    let bus = MachineBus::new(serial).with_irqchip(Arc::clone(&irqchip));

    let mem = Arc::new(partition.memory().clone());
    let quiesce = Quiesce::new();
    let mem_size = MACHINE.memory_mib << 20;
    // `reboot=k` on purpose: on this host it is the *keyboard controller pulse*
    // that reaches the machine, not the triple fault WHP would absorb.
    let boot = BootConfig {
        kernel,
        initramfs: Some(initramfs),
        cmdline: format!("console=ttyS0 earlyprintk=serial panic=1 reboot=k {extra_cmdline}")
            .trim_end()
            .to_string(),
    };
    let loaded = linux_boot::load(partition.memory(), &boot, mem_size).expect("bzImage load");

    let mut vcpus = partition.take_vcpus();
    {
        let vcpu = &mut vcpus[0];
        x86_boot::setup_long_mode_sregs(partition.memory(), vcpu).unwrap();
        x86_boot::setup_boot_regs(vcpu, loaded.entry, loaded.boot_params_addr).unwrap();
    }

    let lifecycle = Lifecycle::new(MACHINE.vcpu_count);
    bus.set_quiesce(Arc::clone(&quiesce));
    lifecycle.attach_machine(Arc::new(WhpMachine {
        bus: bus.clone(),
        mem,
        quiesce,
        vcpus: MACHINE.vcpu_count,
        boot,
        mem_size,
        entry: Mutex::new((loaded.entry, loaded.boot_params_addr)),
    }));

    let threads = spawn_vcpus_with(
        vcpus,
        |_| Box::new(bus.clone()),
        Some(Arc::clone(&lifecycle)),
    )
    .unwrap();

    Some(Fixture {
        partition,
        irqchip,
        capture,
        lifecycle,
        threads,
    })
}

impl Fixture {
    fn count(&self, needle: &str) -> usize {
        self.capture.text().matches(needle).count()
    }

    fn wait_for(&self, needle: &str, times: usize, timeout: Duration) -> usize {
        let deadline = Instant::now() + timeout;
        loop {
            let seen = self.count(needle);
            if seen >= times || Instant::now() >= deadline {
                return seen;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// Stops the VM and returns the console, dumping it where the other WHP
    /// tests dump theirs.
    fn stop(self) -> String {
        let outcomes = self.threads.stop();
        let text = self.capture.text();
        dump_log(&text);
        eprintln!(
            "pit edges: {}, ioapic messages: {}, resets: {}, outcomes: {outcomes:?}",
            self.irqchip.pit().edges(),
            self.irqchip.ioapic().delivered(),
            self.lifecycle.resets(),
        );
        drop(self.partition);
        text
    }
}

/// A paused VM stops making progress; a resumed one continues the same boot.
#[test]
fn pause_freezes_the_guest_and_resume_continues_the_same_boot() {
    let _guard = whp_guard();
    let Some(vm) = start("entangled.heartbeat=100") else {
        return;
    };
    if vm.wait_for(GUEST_READY_MARKER, 1, BOOT_DEADLINE) == 0 {
        let text = vm.stop();
        panic!("no {GUEST_READY_MARKER}; serial tail:\n{}", tail(&text, 40));
    }
    assert!(
        vm.wait_for("VMHOST_HEARTBEAT", 3, STEP) >= 3,
        "the heartbeat probe never started"
    );

    let paused_at = Instant::now();
    vm.lifecycle.pause().expect("pause");
    assert_eq!(vm.lifecycle.state(), RunState::Paused);
    let acknowledged = paused_at.elapsed();

    std::thread::sleep(Duration::from_millis(100));
    let frozen = vm.count("VMHOST_HEARTBEAT");
    let frozen_len = vm.capture.text().len();
    std::thread::sleep(FREEZE_WATCH);
    assert_eq!(
        vm.count("VMHOST_HEARTBEAT"),
        frozen,
        "the guest kept printing heartbeats while paused"
    );
    assert_eq!(
        vm.capture.text().len(),
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
    assert_eq!(
        vm.count(GUEST_READY_MARKER),
        1,
        "resume restarted the guest instead of continuing it"
    );
    assert_eq!(vm.lifecycle.resets(), 0);
    eprintln!("paused in {acknowledged:?}, {frozen} heartbeats before, {resumed} after resume");
    vm.stop();
}

/// A host-initiated reset reboots the machine in place, twice in a row —
/// which on this host means deleting and re-creating the virtual processor.
#[test]
fn a_host_reset_reboots_the_guest_in_place_twice() {
    let _guard = whp_guard();
    let Some(vm) = start("entangled.heartbeat=100") else {
        return;
    };
    for round in 1..=2u64 {
        let seen = vm.wait_for(GUEST_READY_MARKER, round as usize, BOOT_DEADLINE);
        if seen < round as usize {
            let text = vm.stop();
            panic!(
                "boot {round} never became ready; serial tail:\n{}",
                tail(&text, 40)
            );
        }
        let started = Instant::now();
        vm.lifecycle.reset().expect("reset");
        assert_eq!(vm.lifecycle.state(), RunState::Running);
        assert_eq!(vm.lifecycle.resets(), round);
        eprintln!("reset {round} acknowledged in {:?}", started.elapsed());
    }
    let seen = vm.wait_for(GUEST_READY_MARKER, 3, BOOT_DEADLINE);
    let text = vm.stop();
    assert_eq!(
        seen,
        3,
        "expected three boots (one plus two resets), saw {seen}; serial tail:\n{}",
        tail(&text, 40)
    );
}

/// The guest asks for it. On WHP this is the whole reason the reset registers
/// exist: `reboot=k` pulses port 0x64, the machine latches it, and the VM comes
/// back — where before this the vCPU would simply have parked for ever.
#[test]
fn a_guest_initiated_reboot_comes_back_twice() {
    let _guard = whp_guard();
    // No heartbeat probe: this guest reaches its marker and reboots itself.
    let Some(vm) = start("") else { return };

    // The supervisor `entangled run` has, in miniature: a guest reset is
    // latched by whichever vCPU saw it and served by somebody else.
    let stop = Arc::new(AtomicBool::new(false));
    let supervisor = {
        let (lifecycle, stop) = (Arc::clone(&vm.lifecycle), Arc::clone(&stop));
        std::thread::spawn(move || {
            while !stop.load(Ordering::Acquire) {
                if lifecycle.take_guest_reset() {
                    if let Err(error) = lifecycle.reset() {
                        eprintln!("guest-requested reset failed: {error}");
                        return;
                    }
                }
                std::thread::sleep(Duration::from_millis(10));
            }
        })
    };

    let seen = vm.wait_for(GUEST_READY_MARKER, 3, BOOT_DEADLINE);
    let resets = vm.lifecycle.resets();
    stop.store(true, Ordering::Release);
    let text = vm.stop();
    let _ = supervisor.join();
    assert!(
        seen >= 3,
        "the guest did not come back from its own reboot: {seen} boots, {resets} resets; \
         serial tail:\n{}",
        tail(&text, 40)
    );
    assert!(resets >= 2, "resets: {resets}");
}
