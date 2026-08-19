//! virtio-mmio on WHP, end to end (backlog WHP-1703).
//!
//! The Windows mirror of the Linux virtio acceptance boots: a real Linux kernel
//! on the Windows Hypervisor Platform with a **virtio-blk disk on virtio-mmio**,
//! and the guest's own `entangled.blkbench` probe as the evidence — it opens
//! `/dev/vda`, reads it, and reports bytes, milliseconds and the *interrupt count
//! on the virtio line*. All three have to be right for the line to appear:
//!
//! * the `virtio_mmio.device=` clause has to name a window the host decodes, or
//!   the kernel never probes a device and `/dev/vda` does not exist;
//! * a queue kick has to reach the device, which on WHP means the `QUEUE_NOTIFY`
//!   write comes out of the instruction emulator and runs `virtio-blk` inline on
//!   the vCPU thread — there is no ioeventfd;
//! * the completion interrupt has to reach the guest, which means the IOAPIC
//!   redirection entry the guest programmed for pin `layout::VIRTIO_IRQS[0]`
//!   decoded to a `WHvRequestInterrupt` that landed. `irqs=0` with a non-zero
//!   byte count would mean the reads were completed by polling, not by
//!   interrupts.
//!
//! Everything above `vmm_core::hv` is the same code the KVM path runs, including
//! `VirtioMmioBus` itself — only the two wiring primitives differ (see
//! `machine_x86::virtio::VirtioMmioBus::attach_userspace`).
//!
//! Self-skips when WHP is off or the artifacts are missing, like every other test
//! in this crate.

#![cfg(windows)]

use std::sync::Arc;
use std::time::Instant;

use linux_boot::{BootConfig, GUEST_READY_MARKER};
use machine_x86::boot as x86_boot;
use machine_x86::bus::MachineBus;
use machine_x86::irqchip::UserspaceIrqChip;
use machine_x86::layout;
use machine_x86::serial::SerialConsole;
use machine_x86::virtio::VirtioMmioBus;
use virtio_core::VirtioDevice;
use vmm_core::whp::{WhpHypervisor, WhpOptions, WhpPartition, WHP_ENABLE_HINT};
use vmm_core::RunOutcome;

mod whp_common;
use whp_common::{
    artifact, complete_line_with, dump_log, kernel, tail, whp_guard, Capture, BOOT_DEADLINE,
    MACHINE,
};

/// How much of the disk the probe reads. Enough to make many requests (the probe
/// reads in 64 KiB chunks, so 8 MiB is 128 of them) without making a debug-build
/// synchronous-kick run take minutes.
const BENCH_MIB: u64 = 8;

/// EPIC 17 phase 3 acceptance: a virtio-blk device on virtio-mmio works on WHP —
/// the guest finds `/dev/vda`, reads from it, and the reads are completed by
/// interrupts delivered through the userspace IOAPIC.
#[test]
fn virtio_blk_on_mmio_serves_the_guest_on_whp() {
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
    eprintln!("booting the {which} kernel with {} on mmio", disk.display());

    let mut partition = WhpPartition::with_options(&hv, &MACHINE, WhpOptions::for_guest())
        .expect("WHP partition with a local APIC");
    machine_x86::mptable::write(partition.memory(), MACHINE.vcpu_count).unwrap();
    machine_x86::acpi::write(partition.memory(), MACHINE.vcpu_count).unwrap();

    let irqchip = UserspaceIrqChip::new(partition.interrupt_delivery(), MACHINE.vcpu_count)
        .expect("userspace irqchip");

    // Read-only on purpose: the probe only reads, and the image is a shared
    // artifact that a test must not modify.
    let block = virtio_block::BlockDevice::open(&disk, false).expect("open the raw disk image");
    let devices: Vec<Box<dyn VirtioDevice>> = vec![Box::new(block)];

    let mem = Arc::new(partition.memory().clone());
    let virtio = VirtioMmioBus::attach_userspace(mem, devices, &irqchip)
        .expect("attach virtio-mmio on the userspace irqchip");
    let clauses = virtio.cmdline_clauses();
    assert_eq!(virtio.slots().len(), 1);
    assert_eq!(virtio.slots()[0].irq, layout::VIRTIO_IRQS[0]);
    eprintln!("virtio-mmio clauses: {clauses}");

    let capture = Capture::default();
    let serial = SerialConsole::with_trigger(irqchip.serial_line(), Box::new(capture.clone()));
    let bus = MachineBus::with_virtio(serial, virtio).with_irqchip(Arc::clone(&irqchip));

    let boot = BootConfig {
        kernel,
        initramfs: Some(initramfs),
        cmdline: format!(
            "console=ttyS0 earlyprintk=serial panic=1 reboot=k \
             entangled.blkbench={BENCH_MIB} {clauses}"
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
        // Either verdict ends the wait; the assertions below decide what a FAIL
        // means. The whole line must have arrived first.
        if complete_line_with(&text, "VMHOST_TEST_OK blkbench ")
            || complete_line_with(&text, "VMHOST_TEST_FAIL blkbench ")
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
        "time to ready: {:?}, pit edges: {}, ioapic messages delivered: {}, serial bytes: {}",
        ready_at,
        irqchip.pit().edges(),
        irqchip.ioapic().delivered(),
        text.len()
    );
    // Host-side device state makes a stall diagnosable in one run: a non-zero
    // interrupt status with the guest stuck means the device answered and the
    // interrupt was lost, which is a different bug from a kick never arriving.
    if !done {
        for (index, slot) in bus.virtio().slots().iter().enumerate() {
            match slot.transport.lock() {
                Ok(t) => eprintln!(
                    "mmio slot {index}: device={:?} status={:#04x} activated={} \
                     interrupt_status={:#x}",
                    t.device_type(),
                    t.status(),
                    t.is_activated(),
                    t.interrupt_status(),
                ),
                Err(_) => eprintln!("mmio slot {index}: transport lock poisoned"),
            }
        }
    }
    assert!(
        !panicked,
        "the guest panicked; serial tail:\n{}",
        tail(&text, 40)
    );
    assert!(
        done,
        "no blkbench verdict within {BOOT_DEADLINE:?}; serial tail:\n{}",
        tail(&text, 60)
    );
    assert!(
        !text.contains("VMHOST_TEST_FAIL blkbench"),
        "the guest could not read /dev/vda; serial tail:\n{}",
        tail(&text, 40)
    );

    let bytes = capture
        .probe_value("blkbench", "bytes")
        .expect("blkbench reported no byte count");
    let ms = capture.probe_value("blkbench", "ms").unwrap_or(0);
    let irqs = capture.probe_value("blkbench", "irqs").unwrap_or(-1);
    let kib_per_s = capture.probe_value("blkbench", "kib_per_s").unwrap_or(0);

    // The kick-latency measurement this test exists to produce: on WHP every
    // `QUEUE_NOTIFY` write is a full VM exit *plus* instruction emulation, and the
    // device then runs on the vCPU thread. Printed rather than asserted — a
    // threshold would only encode this host's speed.
    eprintln!(
        "WHP synchronous virtio-mmio kicks: {bytes} bytes in {ms} ms = {kib_per_s} KiB/s, \
         {irqs} interrupts on the virtio line"
    );

    assert_eq!(
        bytes,
        (BENCH_MIB << 20) as i64,
        "the probe did not read the whole {BENCH_MIB} MiB; serial tail:\n{}",
        tail(&text, 20)
    );
    assert!(
        irqs > 0,
        "the guest saw {irqs} interrupts on the virtio line: with -1 there is no such line \
         (the device never probed), with 0 the reads completed without an interrupt ever being \
         delivered through the userspace IOAPIC; serial tail:\n{}",
        tail(&text, 40)
    );
    // The IOAPIC carried both the console and the disk, so its delivery count is
    // strictly greater than the disk's own share.
    assert!(irqchip.ioapic().delivered() > irqs as u32);

    assert_eq!(outcomes.len(), 1);
    let outcome = outcomes.into_iter().next().unwrap().expect("vCPU outcome");
    assert!(
        matches!(outcome, RunOutcome::Stopped | RunOutcome::Shutdown),
        "unexpected outcome {outcome:?}"
    );
    drop(partition);
    drop(irqchip);
}
