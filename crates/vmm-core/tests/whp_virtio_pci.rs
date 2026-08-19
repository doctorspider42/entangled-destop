//! virtio-pci on WHP, end to end (backlog EPIC 17 phase 4).
//!
//! The Windows mirror of `tests/boot/tests/pci_transport.rs`: a real Linux
//! kernel on the Windows Hypervisor Platform with a **virtio-blk disk on
//! virtio-pci**, MSI-X included, and the guest's own probes as the evidence:
//!
//! * `entangled.pciscan=1` proves *enumeration* — the kernel walked
//!   configuration space over `0xcf8`/`0xcfc` (port I/O exits), found the
//!   `1af4:1042` function, bound `virtio-pci` to it, and allocated MSI-X
//!   vectors (`msi_irqs/` in sysfs, which exists only when message-signalled
//!   interrupts are actually enabled);
//! * `entangled.blkbench=…` proves the *data path* — BAR MMIO through the
//!   instruction emulator, queue kicks in the notification area served inline
//!   on the vCPU thread, and completion interrupts arriving as MSI-X messages:
//!   the driver-programmed (address, data) pairs decoded by
//!   `machine_x86::msi::UserspaceMsiSink` into `WHvRequestInterrupt` calls.
//!
//! `irqmode=msix` in the blkbench line is the load-bearing assertion: a read
//! can complete without interrupts (the driver notices used buffers whenever
//! something else wakes it), so only a climbing MSI-X counter proves the
//! delivery path.
//!
//! Everything above `vmm_core::hv` is the same code the KVM path runs,
//! including `VirtioPciBus` itself — the userspace constructor differs from the
//! KVM one only in the three wiring primitives (IOAPIC lines for INTx, the
//! userspace MSI sink, synchronous kicks).
//!
//! Self-skips when WHP is off or the artifacts are missing, like every other
//! test in this crate.

#![cfg(windows)]

use std::sync::Arc;
use std::time::Instant;

use linux_boot::{BootConfig, GUEST_READY_MARKER};
use machine_x86::boot as x86_boot;
use machine_x86::bus::MachineBus;
use machine_x86::irqchip::UserspaceIrqChip;
use machine_x86::serial::SerialConsole;
use machine_x86::virtio_pci::{PciInterruptMode, VirtioPciBus};
use virtio_core::VirtioDevice;
use vmm_core::whp::{WhpHypervisor, WhpOptions, WhpPartition, WHP_ENABLE_HINT};
use vmm_core::RunOutcome;

mod whp_common;
use whp_common::{
    artifact, complete_line_with, dump_log, kernel, tail, whp_guard, Capture, BOOT_DEADLINE,
    MACHINE,
};

/// Same read size as the mmio benchmark, so the two transports' numbers are
/// comparable from the two logs.
const BENCH_MIB: u64 = 8;

/// Phase 4 acceptance: the guest enumerates our PCI bus on WHP, binds the
/// modern virtio driver, takes its interrupts over MSI-X, and reads the disk.
#[test]
fn virtio_blk_on_pci_with_msix_serves_the_guest_on_whp() {
    let _guard = whp_guard();
    let hv = match WhpHypervisor::open() {
        Ok(hv) => hv,
        Err(e) => {
            eprintln!("skipping: {e}");
            eprintln!("(to run this test: {WHP_ENABLE_HINT})");
            return;
        }
    };
    let (Some((kernel, which)), Some(initramfs), Some(disk)) = (
        kernel(),
        artifact("tests/test-initramfs.cpio.gz"),
        artifact("tests/test-root.raw"),
    ) else {
        eprintln!(
            "skipping: test artifacts missing — build artifacts/bootstrap/vmlinuz, \
             scripts/build-test-initramfs.sh and a raw disk at artifacts/tests/test-root.raw"
        );
        return;
    };
    eprintln!("booting the {which} kernel with {} on pci", disk.display());

    let mut partition = WhpPartition::with_options(&hv, &MACHINE, WhpOptions::for_guest())
        .expect("WHP partition with a local APIC");
    machine_x86::mptable::write(partition.memory(), MACHINE.vcpu_count).unwrap();
    machine_x86::acpi::write(partition.memory(), MACHINE.vcpu_count).unwrap();

    let irqchip = UserspaceIrqChip::new(partition.interrupt_delivery(), MACHINE.vcpu_count)
        .expect("userspace irqchip");

    let block = virtio_block::BlockDevice::open(&disk, false).expect("open the raw disk image");
    let devices: Vec<Box<dyn VirtioDevice>> = vec![Box::new(block)];

    let mem = Arc::new(partition.memory().clone());
    let pci = VirtioPciBus::attach_userspace(mem, devices, &irqchip, PciInterruptMode::Msix)
        .expect("attach virtio-pci on the userspace irqchip");
    assert_eq!(pci.slots().len(), 1);
    assert_eq!(
        pci.slots()[0].msix_vectors,
        2,
        "a one-queue block device publishes queue + config vectors"
    );

    let capture = Capture::default();
    let serial = SerialConsole::with_trigger(irqchip.serial_line(), Box::new(capture.clone()));
    let bus = MachineBus::with_virtio_pci(serial, pci).with_irqchip(Arc::clone(&irqchip));

    // No `virtio_mmio.device=` clauses: enumeration is the whole point.
    let boot = BootConfig {
        kernel,
        initramfs: Some(initramfs),
        cmdline: format!(
            "console=ttyS0 earlyprintk=serial panic=1 reboot=k \
             entangled.pciscan=1 entangled.blkbench={BENCH_MIB}"
        ),
    };
    let loaded = linux_boot::load(partition.memory(), &boot, MACHINE.memory_mib << 20)
        .expect("bzImage + initramfs load");

    let mut vcpus = partition.take_vcpus();
    {
        let vcpu = &mut vcpus[0];
        x86_boot::setup_long_mode_sregs(partition.memory(), vcpu).unwrap();
        x86_boot::setup_boot_regs(vcpu, loaded.entry, loaded.boot_params_addr).unwrap();
    }
    let threads = vmm_core::whp::spawn_vcpus(vcpus, |_| Box::new(bus.clone())).unwrap();

    let start = Instant::now();
    let mut ready_at = None;
    let mut done = false;
    let mut panicked = false;
    while start.elapsed() < BOOT_DEADLINE {
        let text = capture.text();
        if ready_at.is_none() && text.contains(GUEST_READY_MARKER) {
            ready_at = Some(start.elapsed());
        }
        // The block probe runs after the PCI scan, so its verdict means both
        // lines are complete.
        if complete_line_with(&text, "VMHOST_TEST_OK blkbench ")
            || complete_line_with(&text, "VMHOST_TEST_FAIL blkbench ")
            || complete_line_with(&text, "VMHOST_TEST_FAIL pciscan ")
        {
            done = true;
            break;
        }
        if text.contains("Kernel panic - not syncing") {
            panicked = true;
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    let outcomes = threads.stop();
    let text = capture.text();
    dump_log(&text);

    eprintln!(
        "time to ready: {ready_at:?}, ioapic messages delivered: {}, serial bytes: {}",
        irqchip.ioapic().delivered(),
        text.len()
    );
    for line in text
        .lines()
        .filter(|l| l.contains("entangled-pciscan") || l.contains("entangled-irq"))
    {
        eprintln!("{}", line.trim());
    }
    assert!(
        !panicked,
        "the guest panicked; serial tail:\n{}",
        tail(&text, 40)
    );
    assert!(
        done,
        "no probe verdict within {BOOT_DEADLINE:?}; serial tail:\n{}",
        tail(&text, 60)
    );
    assert!(
        !text.contains("VMHOST_TEST_FAIL"),
        "a guest probe failed; serial tail:\n{}",
        tail(&text, 40)
    );

    // ---- enumeration: the pciscan line ----
    let virtio = capture.probe_value("pciscan", "virtio").unwrap_or(-1);
    let bound = capture.probe_value("pciscan", "bound").unwrap_or(-1);
    let msix = capture.probe_value("pciscan", "msix").unwrap_or(-1);
    assert_eq!(virtio, 1, "one virtio function; tail:\n{}", tail(&text, 40));
    assert_eq!(
        bound,
        1,
        "virtio-pci must bind the function; tail:\n{}",
        tail(&text, 40)
    );
    assert!(
        msix >= 2,
        "the kernel allocated {msix} message vectors; a bound modern function \
         gets at least config + one queue; tail:\n{}",
        tail(&text, 40)
    );

    // ---- data path: the blkbench line ----
    let bytes = capture
        .probe_value("blkbench", "bytes")
        .expect("blkbench reported no byte count");
    let ms = capture.probe_value("blkbench", "ms").unwrap_or(0);
    let kib_per_s = capture.probe_value("blkbench", "kib_per_s").unwrap_or(0);
    let irqs = capture.probe_value("blkbench", "irqs").unwrap_or(-1);
    let mode = capture
        .probe("blkbench")
        .and_then(|fields| fields.into_iter().find(|(k, _)| k == "irqmode"))
        .map(|(_, v)| v)
        .unwrap_or_default();

    eprintln!(
        "WHP virtio-pci (MSI-X, synchronous kicks): {bytes} bytes in {ms} ms = \
         {kib_per_s} KiB/s, {irqs} interrupts, mode {mode}"
    );
    assert_eq!(
        bytes,
        (BENCH_MIB << 20) as i64,
        "the probe did not read the whole {BENCH_MIB} MiB; serial tail:\n{}",
        tail(&text, 20)
    );
    assert!(
        irqs > 0,
        "the guest saw {irqs} interrupts on its virtio lines; serial tail:\n{}",
        tail(&text, 40)
    );
    assert_eq!(
        mode,
        "msix",
        "the completions must arrive as MSI-X messages, not INTx; serial tail:\n{}",
        tail(&text, 40)
    );

    assert_eq!(outcomes.len(), 1);
    let outcome = outcomes.into_iter().next().unwrap().expect("vCPU outcome");
    assert!(
        matches!(outcome, RunOutcome::Stopped | RunOutcome::Shutdown),
        "unexpected outcome {outcome:?}"
    );
    drop(partition);
    drop(irqchip);
}
