//! Queue-notify offload against a real KVM VM (backlog MVP-307).
//!
//! Covers the host wiring rather than the guest side: that the ioeventfds are
//! accepted by the kernel, that the per-device worker thread turns an eventfd
//! signal into `VirtioDevice::notify`, that the synchronous fallback still
//! works, and — the acceptance criterion from EPIC 14 — that stopping the bus
//! leaves no worker thread behind (checked through the `Arc` strong count on the
//! transport, the same way the net tests check their backend handles).
//!
//! Skips with a note when `/dev/kvm` is unavailable, so plain CI stays green.

#![cfg(target_os = "linux")]

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use machine_x86::notify::QueueNotifyMode;
use machine_x86::virtio::VirtioMmioBus;
use virtio_core::testing::SplitRing;
use virtio_core::{
    mmio, status, DeviceError, DeviceResources, DeviceType, MmioTransport, VirtioDevice,
    VIRTIO_F_VERSION_1,
};
use vmm_core::{Hypervisor, MachineConfig, Vm};

const RING_BASE: u64 = 0x20_0000;
const RING_SIZE: u16 = 16;
const DEADLINE: Duration = Duration::from_secs(5);

/// A device that only counts the kicks it is told about.
struct CountingDevice {
    queues: Vec<u16>,
    notifies: Arc<AtomicUsize>,
}

impl CountingDevice {
    fn new(queue_count: usize) -> (Self, Arc<AtomicUsize>) {
        let notifies = Arc::new(AtomicUsize::new(0));
        (
            Self {
                queues: vec![RING_SIZE; queue_count],
                notifies: Arc::clone(&notifies),
            },
            notifies,
        )
    }
}

impl VirtioDevice for CountingDevice {
    fn device_type(&self) -> DeviceType {
        DeviceType::Block
    }
    fn queue_max_sizes(&self) -> &[u16] {
        &self.queues
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
    fn activate(&mut self, _resources: DeviceResources) -> Result<(), DeviceError> {
        Ok(())
    }
    fn notify(&mut self, _queue_index: u16) -> Result<(), DeviceError> {
        self.notifies.fetch_add(1, Ordering::AcqRel);
        Ok(())
    }
    fn reset(&mut self) {}
}

/// Opens a VM with guest memory, or returns `None` when KVM is unusable.
fn vm() -> Option<Vm> {
    let hv = match Hypervisor::open() {
        Ok(hv) => hv,
        Err(e) => {
            eprintln!("skipping: {e}");
            return None;
        }
    };
    let cfg = MachineConfig {
        memory_mib: 64,
        vcpu_count: 1,
    };
    match Vm::new(&hv, &cfg) {
        Ok(vm) => Some(vm),
        Err(e) => {
            eprintln!("skipping: cannot create a VM: {e}");
            None
        }
    }
}

/// Drives the register sequence a Linux driver would, so the device is live and
/// `queue_notify` actually reaches it.
fn bring_up(transport: &Arc<Mutex<MmioTransport>>, queues: u16) {
    let ring = SplitRing::layout(RING_BASE, RING_SIZE);
    let mut t = transport.lock().expect("fresh transport lock");
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
    for queue in 0..queues {
        write(&mut t, mmio::QUEUE_SEL, u32::from(queue));
        write(&mut t, mmio::QUEUE_NUM, u32::from(ring.size()));
        write(&mut t, mmio::QUEUE_DESC_LOW, ring.desc_table() as u32);
        write(&mut t, mmio::QUEUE_DRIVER_LOW, ring.driver_area() as u32);
        write(&mut t, mmio::QUEUE_DEVICE_LOW, ring.device_area() as u32);
        write(&mut t, mmio::QUEUE_READY, 1);
    }
    write(
        &mut t,
        mmio::STATUS,
        status::ACKNOWLEDGE | status::DRIVER | status::FEATURES_OK | status::DRIVER_OK,
    );
    assert!(t.is_activated(), "test device must activate");
}

fn wait_for(counter: &AtomicUsize, at_least: usize) -> usize {
    let start = Instant::now();
    loop {
        let seen = counter.load(Ordering::Acquire);
        if seen >= at_least || start.elapsed() > DEADLINE {
            return seen;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

#[test]
fn ioeventfd_offload_registers_and_the_worker_serves_kicks() {
    let Some(vm) = vm() else { return };
    let mem = Arc::new(vm.memory().clone());
    let (device, notifies) = CountingDevice::new(2);

    let bus = VirtioMmioBus::attach_with(
        vm.fd_shared(),
        Arc::clone(&mem),
        vec![Box::new(device)],
        QueueNotifyMode::Ioeventfd,
    )
    .expect("attaching one device with two queues");

    let slot = &bus.slots()[0];
    let notifier = slot
        .notifier()
        .expect("ioeventfd mode must offload both queues");
    assert_eq!(notifier.offloaded_queues(), vec![0, 1]);
    {
        let t = slot.transport.lock().expect("transport lock");
        assert!(t.is_queue_notify_offloaded(0));
        assert!(t.is_queue_notify_offloaded(1));
    }

    bring_up(&slot.transport, 2);

    // Signal the eventfds exactly as KVM does for a guest kick.
    notifier.kick(0).expect("queue 0 is offloaded");
    notifier.kick(1).expect("queue 1 is offloaded");
    assert!(
        wait_for(&notifies, 2) >= 2,
        "the worker thread must run the device for both queues"
    );

    // A queue the device does not have is not offloaded, so kicking it fails
    // loudly instead of being silently dropped.
    assert!(notifier.kick(7).is_err());

    // Register writes for offloaded queues must not double-process the ring.
    let before = notifies.load(Ordering::Acquire);
    {
        let mut t = slot.transport.lock().expect("transport lock");
        t.write(mmio::QUEUE_NOTIFY, &0u32.to_le_bytes());
        t.write(mmio::QUEUE_NOTIFY, &1u32.to_le_bytes());
    }
    assert_eq!(notifies.load(Ordering::Acquire), before);

    // MVP-307 / EPIC 14: shutting the bus down joins the worker, which is the
    // only other owner of the transport.
    assert_eq!(
        Arc::strong_count(&slot.transport),
        2,
        "bus and worker thread hold the transport while the VM runs"
    );
    bus.shutdown();
    assert_eq!(
        Arc::strong_count(&slot.transport),
        1,
        "the worker thread must be joined and its transport handle dropped"
    );

    // Idempotent: a second shutdown (and the one from Drop) changes nothing.
    bus.shutdown();
    assert_eq!(Arc::strong_count(&slot.transport), 1);
}

#[test]
fn synchronous_mode_keeps_every_kick_on_the_vcpu_path() {
    let Some(vm) = vm() else { return };
    let mem = Arc::new(vm.memory().clone());
    let (device, notifies) = CountingDevice::new(1);

    let bus = VirtioMmioBus::attach_with(
        vm.fd_shared(),
        Arc::clone(&mem),
        vec![Box::new(device)],
        QueueNotifyMode::Synchronous,
    )
    .expect("attaching in synchronous mode");

    let slot = &bus.slots()[0];
    assert!(slot.notifier().is_none());
    assert_eq!(bus.notify_mode(), QueueNotifyMode::Synchronous);
    assert_eq!(
        Arc::strong_count(&slot.transport),
        1,
        "synchronous mode must not spawn a worker thread"
    );

    bring_up(&slot.transport, 1);
    {
        let mut t = slot.transport.lock().expect("transport lock");
        assert!(!t.is_queue_notify_offloaded(0));
        t.write(mmio::QUEUE_NOTIFY, &0u32.to_le_bytes());
    }
    assert_eq!(notifies.load(Ordering::Acquire), 1);
}

/// A guest-driven device reset must not stop the worker: the ioeventfd stays
/// registered, so kicks after re-initialisation have to keep arriving.
#[test]
fn worker_survives_a_device_reset() {
    let Some(vm) = vm() else { return };
    let mem = Arc::new(vm.memory().clone());
    let (device, notifies) = CountingDevice::new(1);

    let bus = VirtioMmioBus::attach_with(
        vm.fd_shared(),
        Arc::clone(&mem),
        vec![Box::new(device)],
        QueueNotifyMode::Ioeventfd,
    )
    .expect("attaching one device");
    let slot = &bus.slots()[0];
    bring_up(&slot.transport, 1);

    slot.transport
        .lock()
        .expect("transport lock")
        .write(mmio::STATUS, &0u32.to_le_bytes());
    // Kicks while the device is down are dropped by the transport, not lost by
    // a dead worker.
    if let Some(notifier) = slot.notifier() {
        notifier.kick(0).expect("eventfd still registered");
    }
    bring_up(&slot.transport, 1);

    let before = notifies.load(Ordering::Acquire);
    if let Some(notifier) = slot.notifier() {
        notifier.kick(0).expect("eventfd still registered");
    }
    assert!(
        wait_for(&notifies, before + 1) > before,
        "the worker must keep serving the device after a reset"
    );
}

/// Two devices get independent workers, and dropping the bus stops all of them.
#[test]
fn every_device_gets_its_own_worker_and_none_leaks() {
    let Some(vm) = vm() else { return };
    let mem = Arc::new(vm.memory().clone());
    let (first, first_count) = CountingDevice::new(1);
    let (second, second_count) = CountingDevice::new(1);

    let transports = {
        let bus = VirtioMmioBus::attach_with(
            vm.fd_shared(),
            Arc::clone(&mem),
            vec![Box::new(first), Box::new(second)],
            QueueNotifyMode::Ioeventfd,
        )
        .expect("attaching two devices");
        assert_eq!(bus.slots().len(), 2);
        for slot in bus.slots() {
            bring_up(&slot.transport, 1);
            slot.notifier()
                .expect("both devices are offloaded")
                .kick(0)
                .expect("queue 0 is offloaded");
        }
        assert!(wait_for(&first_count, 1) >= 1);
        assert!(wait_for(&second_count, 1) >= 1);
        bus.slots()
            .iter()
            .map(|slot| Arc::clone(&slot.transport))
            .collect::<Vec<_>>()
        // `bus` is dropped here without an explicit shutdown.
    };

    for (index, transport) in transports.iter().enumerate() {
        assert_eq!(
            Arc::strong_count(transport),
            1,
            "dropping the bus must join slot {index}'s worker thread"
        );
    }
}
