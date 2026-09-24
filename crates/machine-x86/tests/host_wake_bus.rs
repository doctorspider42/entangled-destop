//! Host wakeups on the synchronous-kick buses — the ones WHP attaches
//! (`attach_userspace` on both transports, `machine_x86::host_wake`).
//!
//! A device that holds a `HostWaker` and calls it from a thread of its own
//! must be served exactly as a guest kick of its queue 0 is served: through
//! the transport, with the device driver live, with its interrupt delivered
//! through the userspace irqchip — and never while the VM is paused. These
//! tests drive that end to end with no hypervisor, so they run on both hosts;
//! `vmm-core`'s `whp_virtio_blk` does it again under a real WHP guest.
//!
//! The KVM path (`attach_with`, ioeventfd workers) is untouched by the
//! service and keeps its own tests (`tests/queue_notify.rs`).

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use machine_x86::host_wake::HOST_WAKE_THREAD_PREFIX;
use machine_x86::irqchip::UserspaceIrqChip;
use machine_x86::layout;
use machine_x86::virtio::VirtioMmioBus;
use machine_x86::virtio_pci::{PciInterruptMode, VirtioPciBus};
use virtio_core::interrupt::Interrupt;
use virtio_core::testing::SplitRing;
use virtio_core::{
    mmio, pci as vpci, status, DeviceError, DeviceResources, DeviceType, HostWaker, MmioTransport,
    PciTransport, Quiesce, VirtioDevice, VIRTIO_F_VERSION_1,
};
use vmm_core::hv::{HvError, InterruptDelivery, InterruptRequest};

const RING_BASE: u64 = 0x2_0000;
const RING_SIZE: u16 = 16;
const DEADLINE: Duration = Duration::from_secs(5);

/// What the device saw: every notify, with the queue and the thread it ran on.
type Seen = Arc<Mutex<Vec<(u16, String)>>>;

/// A device that hands its waker out to the test and signals a used buffer on
/// every notify, so the interrupt path is exercised too.
struct WakingDevice {
    waker: Arc<Mutex<Option<Arc<dyn HostWaker>>>>,
    seen: Seen,
    interrupt: Option<Arc<dyn Interrupt>>,
}

impl VirtioDevice for WakingDevice {
    fn device_type(&self) -> DeviceType {
        DeviceType::Gpu
    }
    fn queue_max_sizes(&self) -> &[u16] {
        &[RING_SIZE]
    }
    fn device_features(&self) -> u64 {
        VIRTIO_F_VERSION_1
    }
    fn ack_features(&mut self, _negotiated: u64) -> bool {
        true
    }
    fn read_config(&self, _offset: u64, data: &mut [u8]) {
        data.fill(0);
    }
    fn write_config(&mut self, _offset: u64, _data: &[u8]) {}
    fn activate(&mut self, resources: DeviceResources) -> Result<(), DeviceError> {
        self.interrupt = Some(resources.interrupt);
        Ok(())
    }
    fn notify(&mut self, queue_index: u16) -> Result<(), DeviceError> {
        let thread = std::thread::current()
            .name()
            .unwrap_or_default()
            .to_string();
        self.seen.lock().unwrap().push((queue_index, thread));
        if let Some(interrupt) = &self.interrupt {
            interrupt
                .signal_used_queue(queue_index)
                .map_err(|e| DeviceError::Backend(e.to_string()))?;
        }
        Ok(())
    }
    fn reset(&mut self) {
        self.interrupt = None;
    }
    fn set_host_waker(&mut self, waker: Arc<dyn HostWaker>) {
        *self.waker.lock().unwrap() = Some(waker);
    }
}

#[allow(clippy::type_complexity)]
fn device() -> (
    Box<dyn VirtioDevice>,
    Arc<Mutex<Option<Arc<dyn HostWaker>>>>,
    Seen,
) {
    let waker = Arc::new(Mutex::new(None));
    let seen = Seen::default();
    (
        Box::new(WakingDevice {
            waker: Arc::clone(&waker),
            seen: Arc::clone(&seen),
            interrupt: None,
        }),
        waker,
        seen,
    )
}

/// The local APIC stand-in under the real userspace IOAPIC/MSI decode.
#[derive(Default)]
struct Counting(AtomicU32);

impl InterruptDelivery for Counting {
    fn request(&self, _interrupt: &InterruptRequest) -> Result<(), HvError> {
        self.0.fetch_add(1, Ordering::AcqRel);
        Ok(())
    }
}

fn chip() -> (Arc<UserspaceIrqChip>, Arc<Counting>) {
    let delivery = Arc::new(Counting::default());
    let chip = UserspaceIrqChip::new(Arc::clone(&delivery) as Arc<dyn InterruptDelivery>, 1)
        .expect("userspace irqchip");
    // What Linux does when it requests the device's IRQ: program the pin's
    // redirection entry with a vector, unmasked.
    let pin = u32::from(u8::try_from(layout::VIRTIO_IRQS[0]).unwrap());
    let base = u64::from(layout::IOAPIC_ADDR);
    chip.mmio_write(base, &(0x10 + 2 * pin).to_le_bytes());
    chip.mmio_write(base + 0x10, &0x43u32.to_le_bytes());
    (chip, delivery)
}

fn memory() -> Arc<virtio_core::GuestMem> {
    Arc::new(virtio_core::testing::guest_memory(1 << 20))
}

fn bring_up_mmio(transport: &Arc<Mutex<MmioTransport>>) {
    let ring = SplitRing::layout(RING_BASE, RING_SIZE);
    let mut t = transport.lock().unwrap();
    let write =
        |t: &mut MmioTransport, offset: u64, value: u32| t.write(offset, &value.to_le_bytes());
    write(&mut t, mmio::DRIVER_FEATURES_SEL, 1);
    write(
        &mut t,
        mmio::DRIVER_FEATURES,
        (VIRTIO_F_VERSION_1 >> 32) as u32,
    );
    write(&mut t, mmio::STATUS, status::ACKNOWLEDGE);
    write(&mut t, mmio::STATUS, status::ACKNOWLEDGE | status::DRIVER);
    write(
        &mut t,
        mmio::STATUS,
        status::ACKNOWLEDGE | status::DRIVER | status::FEATURES_OK,
    );
    write(&mut t, mmio::QUEUE_SEL, 0);
    write(&mut t, mmio::QUEUE_NUM, u32::from(ring.size()));
    write(&mut t, mmio::QUEUE_DESC_LOW, ring.desc_table() as u32);
    write(&mut t, mmio::QUEUE_DRIVER_LOW, ring.driver_area() as u32);
    write(&mut t, mmio::QUEUE_DEVICE_LOW, ring.device_area() as u32);
    write(&mut t, mmio::QUEUE_READY, 1);
    write(
        &mut t,
        mmio::STATUS,
        status::ACKNOWLEDGE | status::DRIVER | status::FEATURES_OK | status::DRIVER_OK,
    );
    assert!(t.is_activated());
}

fn bring_up_pci(transport: &Arc<Mutex<PciTransport>>) {
    use vpci::common;
    let ring = SplitRing::layout(RING_BASE, RING_SIZE);
    let mut t = transport.lock().unwrap();
    let write = |t: &mut PciTransport, offset: u64, width: usize, value: u64| {
        t.write_bar(
            vpci::COMMON_CFG_OFFSET + offset,
            &value.to_le_bytes()[..width],
        )
    };
    write(&mut t, common::DEVICE_STATUS, 1, status::ACKNOWLEDGE.into());
    write(
        &mut t,
        common::DEVICE_STATUS,
        1,
        (status::ACKNOWLEDGE | status::DRIVER).into(),
    );
    write(&mut t, common::DRIVER_FEATURE_SELECT, 4, 1);
    write(&mut t, common::DRIVER_FEATURE, 4, VIRTIO_F_VERSION_1 >> 32);
    write(
        &mut t,
        common::DEVICE_STATUS,
        1,
        (status::ACKNOWLEDGE | status::DRIVER | status::FEATURES_OK).into(),
    );
    write(&mut t, common::QUEUE_SELECT, 2, 0);
    write(&mut t, common::QUEUE_SIZE, 2, u64::from(ring.size()));
    write(&mut t, common::QUEUE_DESC, 8, ring.desc_table());
    write(&mut t, common::QUEUE_DRIVER, 8, ring.driver_area());
    write(&mut t, common::QUEUE_DEVICE, 8, ring.device_area());
    write(&mut t, common::QUEUE_ENABLE, 2, 1);
    write(
        &mut t,
        common::DEVICE_STATUS,
        1,
        (status::ACKNOWLEDGE | status::DRIVER | status::FEATURES_OK | status::DRIVER_OK).into(),
    );
    assert!(t.is_activated());
}

fn until(what: &str, done: impl Fn() -> bool) {
    let deadline = Instant::now() + DEADLINE;
    while !done() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(1));
    }
}

fn wake_from_a_foreign_thread(waker: &Arc<Mutex<Option<Arc<dyn HostWaker>>>>) {
    let waker = waker
        .lock()
        .unwrap()
        .clone()
        .expect("the synchronous-kick bus hands every device a waker");
    std::thread::spawn(move || waker.wake()).join().unwrap();
}

/// The whole contract on virtio-mmio: a wake from an unrelated thread is a
/// queue-0 notify on the service thread, and the device's interrupt reaches
/// the (userspace) local APIC.
#[test]
fn an_mmio_device_woken_from_a_foreign_thread_is_notified_on_queue_0_and_interrupts() {
    let (chip, delivered) = chip();
    let (dev, waker, seen) = device();
    let bus = VirtioMmioBus::attach_userspace(memory(), vec![dev], &chip).expect("attach");
    bring_up_mmio(&bus.slots()[0].transport);
    let before = delivered.0.load(Ordering::Acquire);

    wake_from_a_foreign_thread(&waker);
    until("the notify", || seen.lock().unwrap().len() == 1);
    assert_eq!(
        seen.lock().unwrap()[0],
        (0, format!("{HOST_WAKE_THREAD_PREFIX}-mmio"))
    );
    until("the interrupt", || {
        delivered.0.load(Ordering::Acquire) > before
    });

    // A guest kick still runs on the thread that took the exit, as before.
    bus.slots()[0]
        .transport
        .lock()
        .unwrap()
        .write(mmio::QUEUE_NOTIFY, &0u32.to_le_bytes());
    assert_eq!(seen.lock().unwrap().len(), 2);
    assert!(!seen.lock().unwrap()[1]
        .1
        .starts_with(HOST_WAKE_THREAD_PREFIX));
}

#[test]
fn a_pci_device_woken_from_a_foreign_thread_is_notified_on_queue_0_and_interrupts() {
    let (chip, delivered) = chip();
    let (dev, waker, seen) = device();
    let bus =
        VirtioPciBus::attach_userspace(memory(), vec![dev], &chip, PciInterruptMode::IntxOnly)
            .expect("attach");
    bring_up_pci(&bus.slots()[0].transport);
    let before = delivered.0.load(Ordering::Acquire);

    wake_from_a_foreign_thread(&waker);
    until("the notify", || seen.lock().unwrap().len() == 1);
    assert_eq!(
        seen.lock().unwrap()[0],
        (0, format!("{HOST_WAKE_THREAD_PREFIX}-pci"))
    );
    until("the interrupt", || {
        delivered.0.load(Ordering::Acquire) > before
    });
}

/// ADR-0005: a paused VM gets no notify from a host wake; the pause settles
/// with the thread parked; resuming serves the wakes, coalesced.
#[test]
fn a_paused_vm_is_not_notified_until_it_resumes() {
    let (chip, _) = chip();
    let (dev, waker, seen) = device();
    let bus = VirtioMmioBus::attach_userspace(memory(), vec![dev], &chip).expect("attach");
    let gate = Quiesce::new();
    bus.set_quiesce(Arc::clone(&gate));
    bring_up_mmio(&bus.slots()[0].transport);

    gate.pause();
    assert!(gate.wait_until_idle(Duration::from_secs(1)));
    for _ in 0..50 {
        wake_from_a_foreign_thread(&waker);
    }
    std::thread::sleep(Duration::from_millis(100));
    assert!(seen.lock().unwrap().is_empty(), "notified while paused");
    assert!(
        gate.wait_until_idle(Duration::from_millis(200)),
        "the parked service thread holds no pass"
    );
    gate.resume();
    until("the notify after resume", || {
        !seen.lock().unwrap().is_empty()
    });
    std::thread::sleep(Duration::from_millis(50));
    // The thread may have taken the pending bit before it parked, and a later
    // wake set it again: one notify for the wakes it had taken, one for the
    // rest. Never one per wake.
    let served = seen.lock().unwrap().len();
    assert!(served <= 2, "fifty wakes, {served} notifies");
}

/// ADR-0005: a machine reset on a quiesced VM joins the thread, drops the old
/// boot's wakes and starts afresh; the reset device is not notified until a
/// driver brings it up again.
#[test]
fn a_machine_reset_joins_the_thread_and_the_next_boot_is_served_afresh() {
    let (chip, _) = chip();
    let (dev, waker, seen) = device();
    let bus =
        VirtioPciBus::attach_userspace(memory(), vec![dev], &chip, PciInterruptMode::IntxOnly)
            .expect("attach");
    let gate = Quiesce::new();
    bus.set_quiesce(Arc::clone(&gate));
    bring_up_pci(&bus.slots()[0].transport);

    gate.pause();
    wake_from_a_foreign_thread(&waker);
    let started = Instant::now();
    bus.reset();
    assert!(started.elapsed() < Duration::from_secs(1), "reset joined");
    assert!(bus.host_wake().expect("a service").is_running());
    gate.resume();
    wake_from_a_foreign_thread(&waker);
    std::thread::sleep(Duration::from_millis(50));
    assert!(
        seen.lock().unwrap().is_empty(),
        "neither the old boot's wake nor a wake of a device with no driver is served"
    );
    bring_up_pci(&bus.slots()[0].transport);
    wake_from_a_foreign_thread(&waker);
    until("the new boot's notify", || seen.lock().unwrap().len() == 1);
}

/// EPIC 14: stopping the bus leaves no thread behind — the service was the
/// only other owner of the transport.
#[test]
fn shutdown_joins_the_thread_and_lets_go_of_the_transports() {
    let (chip, _) = chip();
    let (dev, waker, _) = device();
    let bus = VirtioMmioBus::attach_userspace(memory(), vec![dev], &chip).expect("attach");
    let transport = Arc::clone(&bus.slots()[0].transport);
    assert!(bus.host_wake().expect("a service").is_running());
    bus.shutdown();
    assert!(!bus.host_wake().expect("a service").is_running());
    assert_eq!(
        Arc::strong_count(&transport),
        2,
        "only the bus and this test hold the transport"
    );
    // A device thread that still holds its waker is harmless.
    wake_from_a_foreign_thread(&waker);
    drop(bus);
    assert_eq!(Arc::strong_count(&transport), 1);
}
