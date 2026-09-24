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

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Instant;

use linux_boot::{BootConfig, GUEST_READY_MARKER};
use machine_x86::boot as x86_boot;
use machine_x86::bus::MachineBus;
use machine_x86::host_wake::HOST_WAKE_THREAD_PREFIX;
use machine_x86::irqchip::UserspaceIrqChip;
use machine_x86::layout;
use machine_x86::serial::SerialConsole;
use machine_x86::virtio::VirtioMmioBus;
use virtio_core::{HostWaker, VirtioDevice};
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
    run_blkbench("synchronous", |block| Box::new(block));
}

/// The host waker on WHP (ADR-0002, 2026-09-24): the same boot, with a disk
/// that serves **none** of its requests on the vCPU thread that kicked it —
/// every kick is turned into a host wake, and the request is served when the
/// machine's host-wake thread calls queue 0 back. The guest reading its whole
/// disk, interrupt-completed, proves that a wake from a foreign thread
/// reaches the device through the transport and that the interrupts it
/// raises from that thread reach the guest through the userspace IOAPIC.
/// Before that thread existed a device on this host never got a waker at all.
#[test]
fn virtio_blk_served_only_from_the_host_wake_thread_serves_the_guest_on_whp() {
    let served = Arc::new(ServedCounts::default());
    let counts = Arc::clone(&served);
    let ran = run_blkbench("host-wake", move |block| {
        Box::new(ServedByHostWake {
            inner: block,
            waker: None,
            counts,
        })
    });
    if !ran {
        return;
    }
    let from_wake = served.from_wake.load(Ordering::Acquire);
    let kicks = served.kicks.load(Ordering::Acquire);
    eprintln!(
        "{kicks} guest kicks turned into host wakes, {from_wake} notifies from the host-wake thread"
    );
    assert!(kicks > 0, "the guest never kicked the disk");
    assert!(
        from_wake > 0,
        "no request was served from the host-wake thread"
    );
    assert_eq!(
        served.unwoken.load(Ordering::Acquire),
        0,
        "the machine handed the device no waker"
    );
}

#[derive(Default)]
struct ServedCounts {
    /// Guest kicks, each answered with a host wake instead of any work.
    kicks: AtomicU64,
    /// Notifies that came from the host-wake thread and did the work.
    from_wake: AtomicU64,
    /// Kicks that arrived with no waker to hand them to.
    unwoken: AtomicU64,
}

/// A virtio-blk that defers every guest kick to the machine's host-wake
/// thread (see the test above).
struct ServedByHostWake {
    inner: virtio_block::BlockDevice,
    waker: Option<Arc<dyn HostWaker>>,
    counts: Arc<ServedCounts>,
}

impl VirtioDevice for ServedByHostWake {
    fn device_type(&self) -> virtio_core::DeviceType {
        self.inner.device_type()
    }
    fn queue_max_sizes(&self) -> &[u16] {
        self.inner.queue_max_sizes()
    }
    fn device_features(&self) -> u64 {
        self.inner.device_features()
    }
    fn ack_features(&mut self, negotiated: u64) -> bool {
        self.inner.ack_features(negotiated)
    }
    fn read_config(&self, offset: u64, data: &mut [u8]) {
        self.inner.read_config(offset, data);
    }
    fn write_config(&mut self, offset: u64, data: &[u8]) {
        self.inner.write_config(offset, data);
    }
    fn activate(
        &mut self,
        resources: virtio_core::DeviceResources,
    ) -> Result<(), virtio_core::DeviceError> {
        self.inner.activate(resources)
    }
    fn notify(&mut self, queue_index: u16) -> Result<(), virtio_core::DeviceError> {
        let on_wake_thread = std::thread::current()
            .name()
            .is_some_and(|name| name.starts_with(HOST_WAKE_THREAD_PREFIX));
        if on_wake_thread {
            self.counts.from_wake.fetch_add(1, Ordering::AcqRel);
            return self.inner.notify(queue_index);
        }
        match &self.waker {
            Some(waker) => {
                self.counts.kicks.fetch_add(1, Ordering::AcqRel);
                waker.wake();
                Ok(())
            }
            None => {
                self.counts.unwoken.fetch_add(1, Ordering::AcqRel);
                self.inner.notify(queue_index)
            }
        }
    }
    fn reset(&mut self) {
        self.inner.reset();
    }
    fn set_host_waker(&mut self, waker: Arc<dyn HostWaker>) {
        self.waker = Some(waker);
    }
}

/// Boots the blkbench guest with the disk `wrap` makes, and asserts the
/// probe's verdict. `false` when it skipped.
fn run_blkbench(
    label: &str,
    wrap: impl FnOnce(virtio_block::BlockDevice) -> Box<dyn VirtioDevice>,
) -> bool {
    let _guard = whp_guard();
    let hv = match WhpHypervisor::open() {
        Ok(hv) => hv,
        Err(e) => {
            eprintln!("skipping: {e}");
            eprintln!("(to run this test: {WHP_ENABLE_HINT})");
            return false;
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
        return false;
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
    let devices: Vec<Box<dyn VirtioDevice>> = vec![wrap(block)];

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
        "WHP {label} virtio-mmio kicks: {bytes} bytes in {ms} ms = {kib_per_s} KiB/s, \
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
    true
}
