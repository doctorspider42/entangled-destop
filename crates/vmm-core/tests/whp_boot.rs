//! Boot-to-marker on Windows (backlog WHP-1703): the Windows mirror of
//! `linux-boot/tests/boot_marker.rs`.
//!
//! Boots a real Linux kernel on the Windows Hypervisor Platform — long-mode
//! entry, MP table, ACPI tables, the userspace 8259/8254/IOAPIC set, the 16550 on
//! IOAPIC pin 4 — and waits for `VMHOST_GUEST_READY` on the captured serial
//! console. Everything above the hypervisor is the *same code* the KVM path runs;
//! the only difference is which backend is behind `vmm_core::hv`.
//!
//! Needs artifacts produced by (on Linux, or copied in):
//!   scripts/fetch-test-kernel.sh       -> artifacts/tests/vmlinuz
//!   scripts/build-test-initramfs.sh    -> artifacts/tests/test-initramfs.cpio.gz
//!   guest/bootstrap-kernel/build.sh    -> artifacts/bootstrap/vmlinuz
//!
//! Prefers the project's bootstrap kernel and falls back to the Debian netboot
//! kernel, so the test is useful with whichever is present. Self-skips when
//! neither is, or when WHP is off — like every other test in this crate.

#![cfg(windows)]

use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::time::{Duration, Instant};

use linux_boot::{BootConfig, GUEST_READY_MARKER};
use machine_x86::boot as x86_boot;
use machine_x86::bus::MachineBus;
use machine_x86::irqchip::UserspaceIrqChip;
use machine_x86::serial::SerialConsole;
use vmm_core::whp::{WhpHypervisor, WhpOptions, WhpPartition, WHP_ENABLE_HINT};
use vmm_core::{MachineConfig, RunOutcome};

/// How long the guest gets to reach the marker.
///
/// Generous next to the KVM test's 60 s: a WHP guest takes every `hlt` and every
/// MMIO access through userspace, and the run is a debug build.
const BOOT_DEADLINE: Duration = Duration::from_secs(120);

const MACHINE: MachineConfig = MachineConfig {
    memory_mib: 512,
    vcpu_count: 1,
};

/// Serialises against WHP's one-mapped-partition-per-process limit, exactly as
/// `whp_smoke.rs` does (see its module docs). Separate binaries do not share a
/// process, but a future second test in this file would.
fn whp_guard() -> MutexGuard<'static, ()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[derive(Clone, Default)]
struct Capture(Arc<Mutex<Vec<u8>>>);

impl Write for Capture {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        if let Ok(mut inner) = self.0.lock() {
            inner.extend_from_slice(buf);
        }
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl Capture {
    fn text(&self) -> String {
        String::from_utf8_lossy(&self.0.lock().map(|v| v.clone()).unwrap_or_default()).into_owned()
    }
}

fn artifact(relative: &str) -> Option<PathBuf> {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../artifacts")
        .join(relative);
    path.exists().then_some(path)
}

/// The bootstrap kernel if it is built, otherwise the Debian netboot kernel.
fn kernel() -> Option<(PathBuf, &'static str)> {
    artifact("bootstrap/vmlinuz")
        .map(|p| (p, "bootstrap"))
        .or_else(|| artifact("tests/vmlinuz").map(|p| (p, "debian-netboot")))
}

/// The last `lines` lines of the serial log, for a failure message that is
/// readable rather than a wall of boot output.
fn tail(text: &str, lines: usize) -> String {
    let all: Vec<&str> = text.lines().collect();
    all[all.len().saturating_sub(lines)..].join("\n")
}

/// Writes the whole serial log to `$ENTANGLED_WHP_BOOT_LOG` when it is set.
///
/// A passing boot is not the same as a *clean* boot: the interesting lines are
/// the ones about TSC calibration, the APIC timer and `check_timer()`, and they
/// scroll past long before the marker. Keeping the whole log a single environment
/// variable away is what makes the next question about this machine answerable
/// without editing the test.
fn dump_log(text: &str) {
    let Ok(path) = std::env::var("ENTANGLED_WHP_BOOT_LOG") else {
        return;
    };
    match std::fs::write(&path, text) {
        Ok(()) => eprintln!("wrote the serial log to {path}"),
        Err(e) => eprintln!("could not write the serial log to {path}: {e}"),
    }
}

/// EPIC 17 phase 2 acceptance: a Linux guest boots to `VMHOST_GUEST_READY`
/// natively on Windows, and the VM tears down cleanly afterwards.
#[test]
fn linux_boots_to_the_ready_marker_on_whp() {
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
    eprintln!("booting the {which} kernel: {}", kernel.display());

    // Local APIC emulation and the CPUID policy: a real guest needs both, and
    // `hlt` means "idle" rather than "done" from here on.
    let mut partition = WhpPartition::with_options(&hv, &MACHINE, WhpOptions::for_guest())
        .expect("WHP partition with a local APIC");

    // Portable machine model, byte-identical to the KVM path.
    machine_x86::mptable::write(partition.memory(), MACHINE.vcpu_count).unwrap();
    machine_x86::acpi::write(partition.memory(), MACHINE.vcpu_count).unwrap();

    // The piece KVM does in the kernel: PIC, PIT and IOAPIC in userspace,
    // delivering through `WHvRequestInterrupt`.
    let irqchip = UserspaceIrqChip::new(partition.interrupt_delivery(), MACHINE.vcpu_count)
        .expect("userspace irqchip");

    let capture = Capture::default();
    let serial = SerialConsole::with_trigger(irqchip.serial_line(), Box::new(capture.clone()));
    let bus = MachineBus::new(serial).with_irqchip(Arc::clone(&irqchip));

    let boot = BootConfig {
        kernel,
        initramfs: Some(initramfs),
        cmdline: "console=ttyS0 earlyprintk=serial panic=1 reboot=k".into(),
    };
    let loaded = linux_boot::load(partition.memory(), &boot, MACHINE.memory_mib << 20)
        .expect("bzImage + initramfs load");

    let mut vcpus = partition.take_vcpus();
    let vcpu = &mut vcpus[0];
    x86_boot::setup_long_mode_sregs(partition.memory(), vcpu).unwrap();
    x86_boot::setup_boot_regs(vcpu, loaded.entry, loaded.boot_params_addr).unwrap();

    let threads = vmm_core::whp::spawn_vcpus(vcpus, |_| Box::new(bus.clone())).unwrap();

    let start = Instant::now();
    let mut ready = false;
    let mut panicked = false;
    while start.elapsed() < BOOT_DEADLINE {
        let text = capture.text();
        if text.contains(GUEST_READY_MARKER) {
            ready = true;
            break;
        }
        if text.contains("Kernel panic - not syncing") {
            panicked = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    let outcomes = threads.stop();
    let text = capture.text();
    dump_log(&text);

    // Progress counters make a failure diagnosable without a second run: no PIT
    // edges means the timer thread never armed, and no IOAPIC deliveries means the
    // guest never unmasked a pin.
    eprintln!(
        "pit edges: {}, ioapic messages delivered: {}, serial bytes: {}",
        irqchip.pit().edges(),
        irqchip.ioapic().delivered(),
        text.len()
    );
    assert!(
        !panicked,
        "the guest panicked; serial tail:\n{}",
        tail(&text, 40)
    );
    assert!(
        ready,
        "no {GUEST_READY_MARKER} within {BOOT_DEADLINE:?}; pit edges {}, ioapic messages {}; \
         serial tail:\n{}",
        irqchip.pit().edges(),
        irqchip.ioapic().delivered(),
        tail(&text, 60)
    );

    // The tables were *used*, not merely written: with an MADT the kernel never
    // prints the MP-table fallback line. Same two assertions as the KVM test.
    assert!(
        !text.contains("ACPI MADT or MP tables are not detected"),
        "the kernel found neither ACPI nor MP tables; serial tail:\n{}",
        tail(&text, 40)
    );
    assert!(
        text.contains("ACPI: RSDP") || text.contains("ACPI: XSDT"),
        "no sign the kernel parsed our ACPI tables; serial tail:\n{}",
        tail(&text, 40)
    );
    // The userspace irqchip really carried the boot rather than the guest falling
    // back to something else: the PIT ticked and the IOAPIC delivered.
    assert!(irqchip.pit().edges() > 0, "the 8254 never fired IRQ 0");
    assert!(
        irqchip.ioapic().delivered() > 0,
        "the IOAPIC never delivered an interrupt message"
    );

    // Clean teardown: one vCPU, stopped on request, no error.
    assert_eq!(outcomes.len(), 1);
    let outcome = outcomes.into_iter().next().unwrap().expect("vCPU outcome");
    assert!(
        matches!(outcome, RunOutcome::Stopped | RunOutcome::Shutdown),
        "unexpected outcome {outcome:?}"
    );
    drop(partition);
    drop(irqchip);
}

/// With no local APIC, `hlt` still has to mean "the guest is done" — the phase-1
/// contract the real-mode smoke guests depend on. Asserted here rather than in
/// `whp_smoke.rs` because it is [`WhpOptions`] that decides, and getting it
/// backwards would hang those tests instead of failing them.
#[test]
fn halt_without_a_local_apic_still_ends_the_run() {
    let _guard = whp_guard();
    let Ok(hv) = WhpHypervisor::open() else {
        eprintln!("skipping: WHP unavailable — {WHP_ENABLE_HINT}");
        return;
    };
    let cfg = MachineConfig {
        memory_mib: 16,
        vcpu_count: 1,
    };
    let mut partition = WhpPartition::new(&hv, &cfg).unwrap();
    assert_eq!(partition.options(), WhpOptions::default());
    assert!(!partition.options().local_apic);

    use vm_memory::{Bytes, GuestAddress};
    partition
        .memory()
        .write_slice(&[0xf4], GuestAddress(0x1000)) // hlt
        .unwrap();
    let mut vcpus = partition.take_vcpus();
    let vcpu = &mut vcpus[0];
    {
        use vmm_core::hv::VcpuRegisters;
        let mut sregs = vcpu.get_special_registers().unwrap();
        sregs.cs.base = 0;
        sregs.cs.selector = 0;
        vcpu.set_special_registers(&sregs).unwrap();
        let mut regs = vcpu.get_registers().unwrap();
        regs.rip = 0x1000;
        regs.rflags = 2;
        vcpu.set_registers(&regs).unwrap();
    }

    struct Ignore;
    impl vmm_core::ExitHandler for Ignore {
        fn io_out(&mut self, _port: u16, _data: &[u8]) {}
        fn io_in(&mut self, _port: u16, data: &mut [u8]) {
            data.fill(0xff);
        }
        fn mmio_write(&mut self, _addr: u64, _data: &[u8]) {}
        fn mmio_read(&mut self, _addr: u64, data: &mut [u8]) {
            data.fill(0);
        }
    }

    let running = AtomicBool::new(true);
    let outcome = vcpus[0].run_loop(&mut Ignore, &running).unwrap();
    assert_eq!(outcome, RunOutcome::Halted);
}
