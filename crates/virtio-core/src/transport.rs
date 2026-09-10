//! The `virtio-mmio` transport (backlog MVP-301/302/303/306/307).
//!
//! Implements the modern (version 2) MMIO device register layout from VirtIO
//! spec 1.2 section 4.2.2 for exactly one device in one 4 KiB slot. Legacy
//! (version 1) guests are not supported: `VIRTIO_F_VERSION_1` is mandatory.
//!
//! The transport is the only component that knows about registers. It owns
//! feature negotiation, the status state machine, the guest-programmed queue
//! geometry and the interrupt status word; the device behind it sees queues,
//! negotiated features and its config space.
//!
//! # Threading
//!
//! One `MmioTransport` is shared (behind a `Mutex`) by every vCPU thread,
//! because any vCPU can take an MMIO exit into the slot, **and** by the
//! device's queue worker thread (see below).
//!
//! # QUEUE_NOTIFY offload (MVP-307)
//!
//! A guest kick is a write of the queue index to `slot_base + QUEUE_NOTIFY`.
//! Handled from the MMIO exit path it costs a full `KVM_EXIT_MMIO` round-trip
//! and runs the whole device — file I/O, TAP writes, pixel blits — on the vCPU
//! thread, stalling guest execution for the duration.
//!
//! The host can therefore *offload* a queue's notification: the machine layer
//! registers a host notification primitive (on Linux an `EventFd` bound to KVM
//! as an **ioeventfd** with a 4-byte datamatch on the queue index) and runs
//! [`MmioTransport::queue_notify`] from a per-device worker thread instead.
//! KVM then completes the guest write inside the kernel and the vCPU never
//! leaves the guest.
//!
//! This module stays host-agnostic (ADR-0002): it only records *which* queues
//! are offloaded, via [`MmioTransport::offload_queue_notify`], so that
//! [`MmioTransport::write`] can drop a `QUEUE_NOTIFY` write that the host
//! primitive already owns instead of running the device twice. All eventfd,
//! epoll and KVM plumbing lives in `machine_x86::notify`.
//!
//! Offloading is a property of the host wiring, not of the guest, so it
//! survives [`MmioTransport::reset`] — a driver that resets and re-initialises
//! the device keeps being served by the same worker thread.

use std::sync::Arc;

use thiserror::Error;

use crate::device::{DeviceType, VirtioDevice};
use crate::interrupt::IrqLine;
use crate::mmio;
use crate::queue::QueueConfig;
use crate::state::TransportState;
use crate::GuestMem;
use crate::MAX_QUEUE_SIZE;

/// Ways a device can fail a transport's contract.
///
/// Shared by both transports: every variant but
/// [`Self::TooManyQueuesForNotify`] and [`Self::TooManyQueuesForMsix`] applies
/// to virtio-mmio and virtio-pci alike, and all of them are raised at VM
/// construction time, never by guest input.
#[derive(Debug, Error)]
pub enum TransportError {
    #[error(
        "device type {device_type:?} does not offer VIRTIO_F_VERSION_1; \
         Entangled Desktop implements the modern interface only"
    )]
    MissingVersion1 { device_type: DeviceType },

    #[error("device type {device_type:?} exposes no virtqueues")]
    NoQueues { device_type: DeviceType },

    #[error(
        "device type {device_type:?} declares shared-memory region {id} with zero length; \
         a region that exists must have a length (VEN-2001)"
    )]
    InvalidShmRegion { device_type: DeviceType, id: u8 },

    #[error(
        "device type {device_type:?} declares queue {index} with max size {max_size}; \
         must be a non-zero power of two, at most {MAX_QUEUE_SIZE}"
    )]
    InvalidQueueMaxSize {
        device_type: DeviceType,
        index: usize,
        max_size: u16,
    },

    #[error(
        "device type {device_type:?} exposes {queues} virtqueues; the virtio-pci \
         notification area addresses at most {max}"
    )]
    TooManyQueuesForNotify {
        device_type: DeviceType,
        queues: usize,
        max: usize,
    },

    #[error(
        "device type {device_type:?} exposes {queues} virtqueues, which needs \
         {queues} + 1 MSI-X vectors; the MSI-X table region holds at most {max}"
    )]
    TooManyQueuesForMsix {
        device_type: DeviceType,
        queues: usize,
        max: u16,
    },
}

/// One virtio-mmio device slot: registers plus the device behind them.
///
/// Only the register layout lives here; the feature/status/queue state machine
/// is [`TransportState`], shared with virtio-pci.
pub struct MmioTransport {
    state: TransportState,
}

impl MmioTransport {
    /// Wires `device` into an mmio slot.
    ///
    /// Fails when the device violates the transport's contract — these are
    /// host bugs found at VM construction time, never guest input.
    pub fn new(
        slot: usize,
        device: Box<dyn VirtioDevice>,
        mem: Arc<GuestMem>,
        line: Arc<dyn IrqLine>,
    ) -> Result<Self, TransportError> {
        Ok(Self {
            state: TransportState::new("virtio-mmio", slot, device, mem, line)?,
        })
    }

    pub fn device_type(&self) -> DeviceType {
        self.state.device_type()
    }

    pub fn slot(&self) -> usize {
        self.state.slot()
    }

    pub fn status(&self) -> u32 {
        self.state.status()
    }

    pub fn is_activated(&self) -> bool {
        self.state.is_activated()
    }

    pub fn interrupt_status(&self) -> u32 {
        self.state.interrupt_status()
    }

    /// The device behind this slot, for inspection (tests, `entangled doctor`).
    pub fn device(&self) -> &dyn VirtioDevice {
        self.state.device()
    }

    /// Number of virtqueues this slot exposes. The host uses it to decide how
    /// many notification primitives to create (MVP-307).
    pub fn num_queues(&self) -> usize {
        self.state.num_queues()
    }

    /// Hands ownership of queue `index`'s `QUEUE_NOTIFY` writes to a host
    /// notification primitive: from now on the register path drops those writes
    /// (KVM normally swallows them anyway) and the host is expected to call
    /// [`Self::queue_notify`] from its worker thread instead.
    ///
    /// Returns false when this device has no such queue, so the host can fall
    /// back to the synchronous path instead of silently losing kicks.
    pub fn offload_queue_notify(&mut self, index: u16) -> bool {
        self.state.offload_queue_notify(index)
    }

    /// Gives queue `index`'s `QUEUE_NOTIFY` writes back to the register path,
    /// used when the host tears its notification primitive down again.
    pub fn restore_queue_notify(&mut self, index: u16) {
        self.state.restore_queue_notify(index);
    }

    /// Whether queue `index`'s kicks arrive out-of-band (ioeventfd) rather than
    /// through an MMIO exit.
    pub fn is_queue_notify_offloaded(&self, index: u16) -> bool {
        self.state.is_queue_notify_offloaded(index)
    }

    /// Shared-memory regions the device behind this slot declares (VEN-2001).
    pub fn shm_regions(&self) -> &[crate::ShmRegion] {
        self.state.shm_regions()
    }

    /// Tells the transport where the host placed shared-memory region `id`, as
    /// a guest-physical address.
    ///
    /// Until this is called the region reads as absent — which is the correct,
    /// and the *safe*, answer: a driver told about a window that decodes
    /// nothing would fault on its first access to it.
    pub fn set_shm_base(&mut self, id: u8, base: u64) {
        self.state.set_shm_base(id, base);
    }

    /// Forgets a placement (see [`TransportState::clear_shm_base`]).
    ///
    /// [`TransportState`]: crate::state::TransportState
    pub fn clear_shm_base(&mut self, id: u8) {
        self.state.clear_shm_base(id);
    }

    // ---------------------------------------------------------------- reads

    /// Guest read at `offset` inside the slot.
    pub fn read(&mut self, offset: u64, data: &mut [u8]) {
        if offset >= mmio::CONFIG_SPACE {
            self.state.read_config(offset - mmio::CONFIG_SPACE, data);
            return;
        }
        data.fill(0);
        if data.len() != 4 || offset % 4 != 0 {
            tracing::warn!(
                slot = self.state.slot(),
                device = ?self.state.device_type(),
                offset,
                len = data.len(),
                "ignoring non 32-bit-aligned virtio-mmio register read"
            );
            return;
        }
        let value = self.read_register(offset);
        // Width checked above, so the slice is exactly 4 bytes long.
        data.copy_from_slice(&value.to_le_bytes());
    }

    fn read_register(&self, offset: u64) -> u32 {
        let selected = || self.state.selected_queue();
        match offset {
            mmio::MAGIC_VALUE => mmio::MAGIC,
            mmio::VERSION_REG => mmio::VERSION,
            mmio::DEVICE_ID => self.state.device_type().id(),
            mmio::VENDOR_ID => mmio::VMHOST_VENDOR_ID,
            mmio::DEVICE_FEATURES => self.state.device_features_window(),
            mmio::QUEUE_NUM_MAX => selected().map_or(0, |q| u32::from(q.max_size())),
            mmio::QUEUE_READY => u32::from(selected().is_some_and(QueueConfig::is_ready)),
            // Spec marks the ring address registers write-only; serving reads
            // back is harmless and keeps the register state observable.
            mmio::QUEUE_DESC_LOW => selected().map_or(0, |q| q.desc_table() as u32),
            mmio::QUEUE_DESC_HIGH => selected().map_or(0, |q| (q.desc_table() >> 32) as u32),
            mmio::QUEUE_DRIVER_LOW => selected().map_or(0, |q| q.driver_area() as u32),
            mmio::QUEUE_DRIVER_HIGH => selected().map_or(0, |q| (q.driver_area() >> 32) as u32),
            mmio::QUEUE_DEVICE_LOW => selected().map_or(0, |q| q.device_area() as u32),
            mmio::QUEUE_DEVICE_HIGH => selected().map_or(0, |q| (q.device_area() >> 32) as u32),
            mmio::INTERRUPT_STATUS => self.state.interrupt_status(),
            mmio::STATUS => self.state.status(),
            mmio::CONFIG_GENERATION => self.state.interrupt().generation(),
            // Shared-memory regions (VEN-2001). A region the device declared
            // *and* the host placed answers with its real length and base;
            // everything else keeps the pre-existing behaviour — all-ones per
            // spec, so drivers recognize "no such region". That distinction is
            // not cosmetic: a zero here looks to Linux' virtio_gpu like a real
            // zero-length region at address 0, which it then tries to reserve
            // and fails its probe on.
            mmio::SHM_LEN_LOW | mmio::SHM_LEN_HIGH | mmio::SHM_BASE_LOW | mmio::SHM_BASE_HIGH => {
                match self.state.selected_shm() {
                    Some((region, base)) => match offset {
                        mmio::SHM_LEN_LOW => region.len as u32,
                        mmio::SHM_LEN_HIGH => (region.len >> 32) as u32,
                        mmio::SHM_BASE_LOW => base as u32,
                        _ => (base >> 32) as u32,
                    },
                    None => u32::MAX,
                }
            }
            _ => {
                tracing::debug!(
                    slot = self.state.slot(),
                    device = ?self.state.device_type(),
                    offset,
                    "read from unimplemented or write-only virtio-mmio register"
                );
                0
            }
        }
    }

    // --------------------------------------------------------------- writes

    /// Guest write at `offset` inside the slot.
    pub fn write(&mut self, offset: u64, data: &[u8]) {
        if offset >= mmio::CONFIG_SPACE {
            self.state.write_config(offset - mmio::CONFIG_SPACE, data);
            return;
        }
        if data.len() != 4 || offset % 4 != 0 {
            tracing::warn!(
                slot = self.state.slot(),
                device = ?self.state.device_type(),
                offset,
                len = data.len(),
                "ignoring non 32-bit-aligned virtio-mmio register write"
            );
            return;
        }
        let mut bytes = [0u8; 4];
        // Width checked above.
        bytes.copy_from_slice(data);
        self.write_register(offset, u32::from_le_bytes(bytes));
    }

    fn write_register(&mut self, offset: u64, value: u32) {
        match offset {
            mmio::DEVICE_FEATURES_SEL => self.state.set_device_features_sel(value),
            mmio::DRIVER_FEATURES => self.state.write_driver_features(value),
            mmio::DRIVER_FEATURES_SEL => self.state.set_driver_features_sel(value),
            mmio::QUEUE_SEL => self.state.set_queue_sel(value),
            mmio::SHM_SEL => self.state.set_shm_sel(value),
            mmio::QUEUE_NUM => self.write_queue_num(value),
            mmio::QUEUE_READY => self
                .state
                .edit_selected_queue("QUEUE_READY", |q| q.set_ready(value == 1)),
            mmio::QUEUE_NOTIFY => self.state.queue_notify_from_register(value),
            mmio::INTERRUPT_ACK => self.state.interrupt().ack(value),
            mmio::STATUS => self.state.write_status(value),
            mmio::QUEUE_DESC_LOW
            | mmio::QUEUE_DESC_HIGH
            | mmio::QUEUE_DRIVER_LOW
            | mmio::QUEUE_DRIVER_HIGH
            | mmio::QUEUE_DEVICE_LOW
            | mmio::QUEUE_DEVICE_HIGH => self.write_queue_address(offset, value),
            _ => tracing::warn!(
                slot = self.state.slot(),
                device = ?self.state.device_type(),
                offset,
                value,
                "guest wrote a read-only or unimplemented virtio-mmio register"
            ),
        }
    }

    fn write_queue_num(&mut self, value: u32) {
        // Sizes beyond u16 cannot be valid; store 0 so `build()` rejects the
        // queue with a typed error instead of silently truncating.
        let size = u16::try_from(value).unwrap_or(0);
        self.state
            .edit_selected_queue("QUEUE_NUM", |q| q.set_size(size));
    }

    fn write_queue_address(&mut self, offset: u64, value: u32) {
        self.state
            .edit_selected_queue("queue address", |queue| match offset {
                mmio::QUEUE_DESC_LOW => queue.set_desc_table_low(value),
                mmio::QUEUE_DESC_HIGH => queue.set_desc_table_high(value),
                mmio::QUEUE_DRIVER_LOW => queue.set_driver_area_low(value),
                mmio::QUEUE_DRIVER_HIGH => queue.set_driver_area_high(value),
                mmio::QUEUE_DEVICE_LOW => queue.set_device_area_low(value),
                mmio::QUEUE_DEVICE_HIGH => queue.set_device_area_high(value),
                // Unreachable: the caller only routes the six offsets above.
                _ => (),
            });
    }

    /// Runs the device for the queue named by a `QUEUE_NOTIFY` value.
    ///
    /// The single entry point for kicks, whichever way they arrive: the vCPU
    /// exit path calls it for non-offloaded queues, the device's worker thread
    /// calls it when its ioeventfd fires (MVP-307). `value` is the raw register
    /// value, i.e. still guest-controlled and validated by the shared state.
    pub fn queue_notify(&mut self, value: u32) {
        self.state.queue_notify(value);
    }

    /// Full device reset (MVP-303): everything returns to the state a freshly
    /// constructed transport is in, so a driver can start bring-up again.
    pub fn reset(&mut self) {
        self.state.reset();
    }

    /// Machine reset (ADR-0005): [`Self::reset`] plus the interrupt state a
    /// device reset deliberately keeps. See
    /// [`TransportState::power_on_reset`](crate::state::TransportState::power_on_reset).
    pub fn power_on_reset(&mut self) {
        self.state.power_on_reset();
    }

    /// Everything this slot is, for a snapshot (ADR-0006).
    pub fn save(&self) -> crate::save::TransportSaveState {
        self.state.save()
    }

    /// Puts a saved slot back. See [`TransportState::load`] for the order and
    /// for what a refusal means.
    pub fn load(
        &mut self,
        state: &crate::save::TransportSaveState,
    ) -> Result<(), crate::save::StateError> {
        self.state.load(state)
    }

    /// Shares the VM's pause gate with this slot's device (ADR-0005).
    pub fn set_quiesce(&mut self, quiesce: std::sync::Arc<crate::quiesce::Quiesce>) {
        self.state.set_quiesce(quiesce);
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;
    use crate::device::{DeviceError, DeviceResources, ShmRegion};
    use crate::testing::{self, SplitRing, TestIrqLine};
    use crate::{status, VIRTIO_F_VERSION_1};

    const FEATURE_A: u64 = 1 << 3;
    const FEATURE_HIGH: u64 = 1 << 40;

    /// Queue indices the device was notified about, observable from outside the
    /// boxed `dyn VirtioDevice` the transport owns.
    type NotifyLog = Arc<Mutex<Vec<u16>>>;

    /// A device that records what the transport did to it.
    struct TestDevice {
        queue_sizes: Vec<u16>,
        shm: Vec<ShmRegion>,
        features: u64,
        veto_features: bool,
        fail_activate: bool,
        fail_notify: bool,
        config: Vec<u8>,
        acked: Option<u64>,
        activations: usize,
        resets: usize,
        notifies: NotifyLog,
        activated_queues: usize,
    }

    impl Default for TestDevice {
        fn default() -> Self {
            Self {
                queue_sizes: vec![16],
                shm: Vec::new(),
                features: VIRTIO_F_VERSION_1 | FEATURE_A | FEATURE_HIGH,
                veto_features: false,
                fail_activate: false,
                fail_notify: false,
                config: vec![0xaa, 0xbb, 0xcc, 0xdd],
                acked: None,
                activations: 0,
                resets: 0,
                notifies: NotifyLog::default(),
                activated_queues: 0,
            }
        }
    }

    impl VirtioDevice for TestDevice {
        fn device_type(&self) -> DeviceType {
            DeviceType::Block
        }

        fn queue_max_sizes(&self) -> &[u16] {
            &self.queue_sizes
        }

        fn device_features(&self) -> u64 {
            self.features
        }

        fn ack_features(&mut self, negotiated: u64) -> bool {
            if self.veto_features {
                return false;
            }
            self.acked = Some(negotiated);
            true
        }

        fn shm_regions(&self) -> Vec<ShmRegion> {
            self.shm.clone()
        }

        fn read_config(&self, offset: u64, data: &mut [u8]) {
            for (i, byte) in data.iter_mut().enumerate() {
                let index = offset.saturating_add(i as u64);
                *byte = usize::try_from(index)
                    .ok()
                    .and_then(|i| self.config.get(i))
                    .copied()
                    .unwrap_or(0);
            }
        }

        fn write_config(&mut self, offset: u64, data: &[u8]) {
            for (i, byte) in data.iter().enumerate() {
                let index = offset.saturating_add(i as u64);
                if let Some(slot) = usize::try_from(index)
                    .ok()
                    .and_then(|i| self.config.get_mut(i))
                {
                    *slot = *byte;
                }
            }
        }

        fn activate(&mut self, resources: DeviceResources) -> Result<(), DeviceError> {
            if self.fail_activate {
                return Err(DeviceError::Backend("test failure".into()));
            }
            self.activated_queues = resources.queues.len();
            self.activations += 1;
            Ok(())
        }

        fn notify(&mut self, queue_index: u16) -> Result<(), DeviceError> {
            if let Ok(mut log) = self.notifies.lock() {
                log.push(queue_index);
            }
            if self.fail_notify {
                return Err(DeviceError::Backend("test notify failure".into()));
            }
            Ok(())
        }

        fn reset(&mut self) {
            self.resets += 1;
            self.acked = None;
            self.activated_queues = 0;
        }
    }

    fn transport_with(device: TestDevice) -> (MmioTransport, Arc<TestIrqLine>) {
        let line = Arc::new(TestIrqLine::default());
        let mem = Arc::new(testing::guest_memory(0x2_0000));
        let transport = MmioTransport::new(0, Box::new(device), mem, line.clone())
            .expect("test device satisfies the transport contract");
        (transport, line)
    }

    fn transport() -> (MmioTransport, Arc<TestIrqLine>) {
        transport_with(TestDevice::default())
    }

    /// Reads a 32-bit register.
    fn reg(transport: &mut MmioTransport, offset: u64) -> u32 {
        let mut raw = [0u8; 4];
        transport.read(offset, &mut raw);
        u32::from_le_bytes(raw)
    }

    fn write_reg(transport: &mut MmioTransport, offset: u64, value: u32) {
        transport.write(offset, &value.to_le_bytes());
    }

    /// The behaviour that was a real bug fix once and must not regress: a
    /// device with **no** shared-memory regions answers all-ones for every
    /// selector, so Linux' virtio_gpu sees "no such region" instead of a
    /// zero-length region at address 0 (VEN-2001 preserves this exactly).
    #[test]
    fn a_device_without_shm_regions_still_reads_all_ones() {
        let (mut transport, _) = transport();
        assert!(transport.shm_regions().is_empty());
        for sel in [0u32, 1, 2, 255, u32::MAX] {
            write_reg(&mut transport, mmio::SHM_SEL, sel);
            for offset in [
                mmio::SHM_LEN_LOW,
                mmio::SHM_LEN_HIGH,
                mmio::SHM_BASE_LOW,
                mmio::SHM_BASE_HIGH,
            ] {
                assert_eq!(reg(&mut transport, offset), u32::MAX, "sel {sel}");
            }
        }
    }

    /// A declared region answers with its real length and base — but only once
    /// the host has actually placed it. Before that it is still absent, because
    /// a driver told about a window that decodes nothing would fault on it.
    #[test]
    fn a_declared_shm_region_answers_only_after_the_host_places_it() {
        let device = TestDevice {
            shm: vec![ShmRegion {
                id: 1,
                len: 0x1_0000_0000,
                host_mapped: false,
            }],
            ..Default::default()
        };
        let (mut transport, _) = transport_with(device);
        assert_eq!(transport.shm_regions().len(), 1);

        write_reg(&mut transport, mmio::SHM_SEL, 1);
        assert_eq!(
            reg(&mut transport, mmio::SHM_LEN_LOW),
            u32::MAX,
            "unplaced regions stay absent"
        );

        transport.set_shm_base(1, 0x8_0000_0000);
        assert_eq!(reg(&mut transport, mmio::SHM_LEN_LOW), 0);
        assert_eq!(reg(&mut transport, mmio::SHM_LEN_HIGH), 1);
        assert_eq!(reg(&mut transport, mmio::SHM_BASE_LOW), 0);
        assert_eq!(reg(&mut transport, mmio::SHM_BASE_HIGH), 8);

        // Every other selector is still absent, including ones a hostile
        // driver picks to probe past the end of our list.
        for sel in [0u32, 2, 255, 256, 0x1_0000, u32::MAX] {
            write_reg(&mut transport, mmio::SHM_SEL, sel);
            assert_eq!(
                reg(&mut transport, mmio::SHM_LEN_LOW),
                u32::MAX,
                "sel {sel}"
            );
            assert_eq!(
                reg(&mut transport, mmio::SHM_BASE_HIGH),
                u32::MAX,
                "sel {sel}"
            );
        }
    }

    /// `SHM_SEL` is guest state, so a device reset puts it back to 0 with
    /// everything else — while the *placement* is host wiring and survives,
    /// exactly like `notify_offloaded`.
    #[test]
    fn a_reset_clears_the_shm_selector_but_not_the_placement() {
        let device = TestDevice {
            shm: vec![ShmRegion {
                id: 0,
                len: 4096,
                host_mapped: false,
            }],
            ..Default::default()
        };
        let (mut transport, _) = transport_with(device);
        transport.set_shm_base(0, 0x1000_0000);
        write_reg(&mut transport, mmio::SHM_SEL, 7);
        assert_eq!(reg(&mut transport, mmio::SHM_LEN_LOW), u32::MAX);
        transport.reset();
        // Selector back to 0, which is region 0 — placed, so it answers.
        assert_eq!(reg(&mut transport, mmio::SHM_LEN_LOW), 4096);
        assert_eq!(reg(&mut transport, mmio::SHM_BASE_LOW), 0x1000_0000);
    }

    /// A zero-length region is a host bug the transport refuses at
    /// construction, because "present with length 0" is exactly the state the
    /// all-ones convention exists to avoid.
    #[test]
    fn a_zero_length_shm_region_is_refused() {
        let line = Arc::new(TestIrqLine::default());
        let mem = Arc::new(testing::guest_memory(0x2_0000));
        let device = TestDevice {
            shm: vec![ShmRegion {
                id: 3,
                len: 0,
                host_mapped: false,
            }],
            ..Default::default()
        };
        assert!(matches!(
            MmioTransport::new(0, Box::new(device), mem, line),
            Err(TransportError::InvalidShmRegion { id: 3, .. })
        ));
    }

    /// A transport plus a handle on the queue indices its device is notified
    /// about — the offload tests need to see through the `Box<dyn VirtioDevice>`.
    fn transport_with_notify_log() -> (MmioTransport, NotifyLog) {
        let log = NotifyLog::default();
        let device = TestDevice {
            notifies: Arc::clone(&log),
            ..Default::default()
        };
        let (transport, _) = transport_with(device);
        (transport, log)
    }

    fn notified(log: &NotifyLog) -> Vec<u16> {
        log.lock().map(|l| l.clone()).unwrap_or_default()
    }

    fn read32(t: &mut MmioTransport, offset: u64) -> u32 {
        let mut data = [0u8; 4];
        t.read(offset, &mut data);
        u32::from_le_bytes(data)
    }

    fn write32(t: &mut MmioTransport, offset: u64, value: u32) {
        t.write(offset, &value.to_le_bytes());
    }

    // ------------------------------------------------------ identification

    #[test]
    fn identification_registers() {
        let (mut t, _) = transport();
        assert_eq!(read32(&mut t, mmio::MAGIC_VALUE), mmio::MAGIC);
        assert_eq!(read32(&mut t, mmio::VERSION_REG), 2);
        assert_eq!(read32(&mut t, mmio::DEVICE_ID), DeviceType::Block.id());
        assert_eq!(read32(&mut t, mmio::VENDOR_ID), mmio::VMHOST_VENDOR_ID);
        assert_eq!(read32(&mut t, mmio::CONFIG_GENERATION), 0);
    }

    #[test]
    fn rejects_devices_that_break_the_contract() {
        let mem = Arc::new(testing::guest_memory(0x1_0000));
        let line = Arc::new(TestIrqLine::default());

        let legacy = TestDevice {
            features: FEATURE_A,
            ..Default::default()
        };
        assert!(matches!(
            MmioTransport::new(0, Box::new(legacy), mem.clone(), line.clone()),
            Err(TransportError::MissingVersion1 { .. })
        ));

        let queueless = TestDevice {
            queue_sizes: vec![],
            ..Default::default()
        };
        assert!(matches!(
            MmioTransport::new(0, Box::new(queueless), mem.clone(), line.clone()),
            Err(TransportError::NoQueues { .. })
        ));

        for bad in [0u16, 3, MAX_QUEUE_SIZE * 2] {
            let device = TestDevice {
                queue_sizes: vec![bad],
                ..Default::default()
            };
            assert!(
                matches!(
                    MmioTransport::new(0, Box::new(device), mem.clone(), line.clone()),
                    Err(TransportError::InvalidQueueMaxSize { .. })
                ),
                "queue max size {bad} must be rejected"
            );
        }
    }

    // ----------------------------------------------------------- features

    #[test]
    fn device_features_are_served_in_two_32_bit_windows() {
        let (mut t, _) = transport();
        let expected = VIRTIO_F_VERSION_1 | FEATURE_A | FEATURE_HIGH;

        write32(&mut t, mmio::DEVICE_FEATURES_SEL, 0);
        assert_eq!(read32(&mut t, mmio::DEVICE_FEATURES), expected as u32);

        write32(&mut t, mmio::DEVICE_FEATURES_SEL, 1);
        assert_eq!(
            read32(&mut t, mmio::DEVICE_FEATURES),
            (expected >> 32) as u32
        );

        // Windows beyond 1 are undefined and read as zero.
        write32(&mut t, mmio::DEVICE_FEATURES_SEL, 2);
        assert_eq!(read32(&mut t, mmio::DEVICE_FEATURES), 0);
    }

    #[test]
    fn driver_features_compose_from_two_windows_and_are_masked() {
        let (mut t, _) = transport();
        // Ask for one bit the device offers plus one it does not.
        write32(&mut t, mmio::DRIVER_FEATURES_SEL, 0);
        write32(&mut t, mmio::DRIVER_FEATURES, (FEATURE_A | (1 << 7)) as u32);
        write32(&mut t, mmio::DRIVER_FEATURES_SEL, 1);
        write32(
            &mut t,
            mmio::DRIVER_FEATURES,
            ((VIRTIO_F_VERSION_1 | FEATURE_HIGH) >> 32) as u32,
        );

        bring_up_features(&mut t);
        assert_eq!(t.status() & status::FEATURES_OK, status::FEATURES_OK);
    }

    #[test]
    fn out_of_range_driver_features_sel_is_ignored() {
        let (mut t, _) = transport();
        write32(&mut t, mmio::DRIVER_FEATURES_SEL, 7);
        write32(&mut t, mmio::DRIVER_FEATURES, 0xffff_ffff);
        // Nothing was recorded, so VERSION_1 is still missing.
        write32(&mut t, mmio::STATUS, status::ACKNOWLEDGE);
        write32(&mut t, mmio::STATUS, status::ACKNOWLEDGE | status::DRIVER);
        write32(
            &mut t,
            mmio::STATUS,
            status::ACKNOWLEDGE | status::DRIVER | status::FEATURES_OK,
        );
        assert_eq!(t.status() & status::FEATURES_OK, 0);
    }

    #[test]
    fn driver_features_writes_after_features_ok_are_ignored() {
        let (mut t, _) = transport();
        bring_up_features(&mut t);
        write32(&mut t, mmio::DRIVER_FEATURES_SEL, 0);
        write32(&mut t, mmio::DRIVER_FEATURES, 0);
        // FEATURES_OK stays set: the late write did not disturb negotiation.
        assert_eq!(t.status() & status::FEATURES_OK, status::FEATURES_OK);
    }

    #[test]
    fn features_ok_refused_without_version_1() {
        let (mut t, _) = transport();
        write32(&mut t, mmio::DRIVER_FEATURES_SEL, 0);
        write32(&mut t, mmio::DRIVER_FEATURES, FEATURE_A as u32);
        write32(&mut t, mmio::STATUS, status::ACKNOWLEDGE);
        write32(&mut t, mmio::STATUS, status::ACKNOWLEDGE | status::DRIVER);
        write32(
            &mut t,
            mmio::STATUS,
            status::ACKNOWLEDGE | status::DRIVER | status::FEATURES_OK,
        );
        assert_eq!(t.status(), status::ACKNOWLEDGE | status::DRIVER);
    }

    #[test]
    fn features_ok_refused_when_the_device_vetoes() {
        let (mut t, _) = transport_with(TestDevice {
            veto_features: true,
            ..Default::default()
        });
        bring_up_features(&mut t);
        assert_eq!(t.status() & status::FEATURES_OK, 0);
    }

    // ------------------------------------------------------------- status

    #[test]
    fn illegal_status_transitions_are_ignored() {
        let (mut t, _) = transport();
        write32(&mut t, mmio::STATUS, status::DRIVER_OK);
        assert_eq!(t.status(), 0);

        write32(&mut t, mmio::STATUS, status::ACKNOWLEDGE);
        write32(
            &mut t,
            mmio::STATUS,
            status::ACKNOWLEDGE | status::DRIVER_OK,
        );
        assert_eq!(t.status(), status::ACKNOWLEDGE);

        // Clearing a bit without a reset is illegal.
        write32(&mut t, mmio::STATUS, status::ACKNOWLEDGE | status::DRIVER);
        write32(&mut t, mmio::STATUS, status::DRIVER);
        assert_eq!(t.status(), status::ACKNOWLEDGE | status::DRIVER);
    }

    /// Regression for the MVP-1402 transport fuzz finding: a status write with
    /// reserved bits set used to store them verbatim, so `STATUS` read back
    /// values the spec does not define (e.g. `0x101`).
    #[test]
    fn reserved_status_bits_are_dropped_not_stored() {
        let (mut t, _) = transport();
        write32(&mut t, mmio::STATUS, status::ACKNOWLEDGE | 0x100);
        assert_eq!(t.status(), status::ACKNOWLEDGE);
        assert_eq!(t.status() & !status::KNOWN, 0);

        // A write of only reserved bits changes nothing — in particular it is
        // not treated as the "write 0 = reset" case.
        write32(&mut t, mmio::STATUS, status::ACKNOWLEDGE | status::DRIVER);
        write32(&mut t, mmio::STATUS, 0xffff_ff00);
        assert_eq!(t.status(), status::ACKNOWLEDGE | status::DRIVER);

        // Bring-up still completes with reserved bits riding along.
        let ring = SplitRing::layout(0x1000, 16);
        write32(&mut t, mmio::DRIVER_FEATURES_SEL, 1);
        write32(
            &mut t,
            mmio::DRIVER_FEATURES,
            (VIRTIO_F_VERSION_1 >> 32) as u32,
        );
        write32(
            &mut t,
            mmio::STATUS,
            status::ACKNOWLEDGE | status::DRIVER | status::FEATURES_OK | 0x200,
        );
        program_ring(&mut t, &ring);
        write32(
            &mut t,
            mmio::STATUS,
            status::ACKNOWLEDGE | status::DRIVER | status::FEATURES_OK | status::DRIVER_OK | 0x400,
        );
        assert!(t.is_activated());
        assert_eq!(t.status() & !status::KNOWN, 0);
    }

    #[test]
    fn driver_ok_without_features_ok_does_not_activate() {
        let (mut t, _) = transport();
        write32(&mut t, mmio::STATUS, status::ACKNOWLEDGE);
        write32(&mut t, mmio::STATUS, status::ACKNOWLEDGE | status::DRIVER);
        // Force DRIVER_OK without FEATURES_OK — write_is_valid allows adding
        // DRIVER_OK only after all earlier stages, so this is rejected first.
        write32(
            &mut t,
            mmio::STATUS,
            status::ACKNOWLEDGE | status::DRIVER | status::DRIVER_OK,
        );
        assert!(!t.is_activated());
    }

    // -------------------------------------------------------------- queues

    #[test]
    fn queue_registers_round_trip_per_selected_queue() {
        let (mut t, _) = transport_with(TestDevice {
            queue_sizes: vec![16, 64],
            ..Default::default()
        });

        write32(&mut t, mmio::QUEUE_SEL, 0);
        assert_eq!(read32(&mut t, mmio::QUEUE_NUM_MAX), 16);
        write32(&mut t, mmio::QUEUE_SEL, 1);
        assert_eq!(read32(&mut t, mmio::QUEUE_NUM_MAX), 64);

        write32(&mut t, mmio::QUEUE_NUM, 32);
        write32(&mut t, mmio::QUEUE_DESC_LOW, 0x1000);
        write32(&mut t, mmio::QUEUE_DESC_HIGH, 0);
        write32(&mut t, mmio::QUEUE_DRIVER_LOW, 0x2000);
        write32(&mut t, mmio::QUEUE_DEVICE_LOW, 0x3000);
        write32(&mut t, mmio::QUEUE_READY, 1);

        assert_eq!(read32(&mut t, mmio::QUEUE_DESC_LOW), 0x1000);
        assert_eq!(read32(&mut t, mmio::QUEUE_DRIVER_LOW), 0x2000);
        assert_eq!(read32(&mut t, mmio::QUEUE_DEVICE_LOW), 0x3000);
        assert_eq!(read32(&mut t, mmio::QUEUE_READY), 1);

        // Queue 0 was untouched.
        write32(&mut t, mmio::QUEUE_SEL, 0);
        assert_eq!(read32(&mut t, mmio::QUEUE_DESC_LOW), 0);
        assert_eq!(read32(&mut t, mmio::QUEUE_READY), 0);
    }

    #[test]
    fn out_of_range_queue_sel_reads_zero_and_swallows_writes() {
        let (mut t, _) = transport();
        write32(&mut t, mmio::QUEUE_SEL, 99);
        assert_eq!(read32(&mut t, mmio::QUEUE_NUM_MAX), 0);
        assert_eq!(read32(&mut t, mmio::QUEUE_READY), 0);
        write32(&mut t, mmio::QUEUE_NUM, 8);
        write32(&mut t, mmio::QUEUE_DESC_LOW, 0xdead);
        write32(&mut t, mmio::QUEUE_READY, 1);

        // Queue 0 is untouched by all of the above.
        write32(&mut t, mmio::QUEUE_SEL, 0);
        assert_eq!(read32(&mut t, mmio::QUEUE_NUM_MAX), 16);
        assert_eq!(read32(&mut t, mmio::QUEUE_DESC_LOW), 0);
        assert_eq!(read32(&mut t, mmio::QUEUE_READY), 0);
    }

    #[test]
    fn oversized_queue_num_becomes_an_invalid_size() {
        let (mut t, _) = transport();
        let ring = SplitRing::layout(0x1000, 16);
        bring_up_features(&mut t);
        program_ring(&mut t, &ring);
        // A size that cannot fit in the 16-bit field is stored as 0, so
        // activation must fail rather than silently truncate to 1.
        write32(&mut t, mmio::QUEUE_NUM, 0x1_0001);
        finish_bring_up(&mut t);
        assert!(!t.is_activated());
        assert_ne!(t.status() & status::DEVICE_NEEDS_RESET, 0);
    }

    // ---------------------------------------------------------- activation

    #[test]
    fn full_bring_up_activates_the_device_with_its_queues() {
        let (mut t, _) = transport();
        let ring = SplitRing::layout(0x1000, 16);
        bring_up(&mut t, &ring);
        assert!(t.is_activated());
        assert_eq!(
            t.status(),
            status::ACKNOWLEDGE | status::DRIVER | status::FEATURES_OK | status::DRIVER_OK
        );
    }

    #[test]
    fn activation_with_rings_outside_guest_memory_needs_reset() {
        let (mut t, _) = transport();
        // Guest memory is 0x2_0000 bytes; put the used ring far beyond it.
        let ring = SplitRing::layout(0x1000, 16);
        bring_up_features(&mut t);
        write32(&mut t, mmio::QUEUE_SEL, 0);
        write32(&mut t, mmio::QUEUE_NUM, 16);
        write32(&mut t, mmio::QUEUE_DESC_LOW, ring.desc_table() as u32);
        write32(&mut t, mmio::QUEUE_DRIVER_LOW, ring.driver_area() as u32);
        write32(&mut t, mmio::QUEUE_DEVICE_LOW, 0xffff_0000);
        write32(&mut t, mmio::QUEUE_READY, 1);
        finish_bring_up(&mut t);

        assert!(!t.is_activated());
        assert_ne!(t.status() & status::DEVICE_NEEDS_RESET, 0);
    }

    #[test]
    fn activation_without_ready_queues_needs_reset() {
        let (mut t, _) = transport();
        bring_up_features(&mut t);
        finish_bring_up(&mut t);
        assert!(!t.is_activated());
        assert_ne!(t.status() & status::DEVICE_NEEDS_RESET, 0);
    }

    #[test]
    fn device_refusing_activation_needs_reset() {
        let (mut t, irq) = transport_with(TestDevice {
            fail_activate: true,
            ..Default::default()
        });
        let ring = SplitRing::layout(0x1000, 16);
        bring_up(&mut t, &ring);
        assert!(!t.is_activated());
        assert_ne!(t.status() & status::DEVICE_NEEDS_RESET, 0);
        // The driver is told to look at the device.
        assert_eq!(t.interrupt_status() & mmio::INT_CONFIG, mmio::INT_CONFIG);
        assert!(irq.count() >= 1);
    }

    #[test]
    fn queue_reconfiguration_after_driver_ok_is_ignored() {
        let (mut t, _) = transport();
        let ring = SplitRing::layout(0x1000, 16);
        bring_up(&mut t, &ring);
        write32(&mut t, mmio::QUEUE_DESC_LOW, 0xbadd);
        write32(&mut t, mmio::QUEUE_NUM, 4);
        write32(&mut t, mmio::QUEUE_READY, 0);
        assert_eq!(
            read32(&mut t, mmio::QUEUE_DESC_LOW),
            ring.desc_table() as u32
        );
        assert_eq!(read32(&mut t, mmio::QUEUE_READY), 1);
    }

    // -------------------------------------------------------------- notify

    #[test]
    fn queue_notify_reaches_the_device_only_when_live() {
        let (mut t, _) = transport();
        // Before activation the notify is dropped.
        write32(&mut t, mmio::QUEUE_NOTIFY, 0);
        assert!(!t.is_activated());

        let ring = SplitRing::layout(0x1000, 16);
        bring_up(&mut t, &ring);
        write32(&mut t, mmio::QUEUE_NOTIFY, 0);
        // Out-of-range queue indices are dropped without touching the device.
        write32(&mut t, mmio::QUEUE_NOTIFY, 1);
        write32(&mut t, mmio::QUEUE_NOTIFY, 0xffff_ffff);
        assert_eq!(t.status() & status::DEVICE_NEEDS_RESET, 0);
    }

    // ----------------------------------------------- notify offload (MVP-307)

    #[test]
    fn offloaded_queue_ignores_register_writes_but_serves_worker_calls() {
        let (mut t, log) = transport_with_notify_log();
        let ring = SplitRing::layout(0x1000, 16);
        bring_up(&mut t, &ring);

        assert!(t.offload_queue_notify(0));
        assert!(t.is_queue_notify_offloaded(0));

        // The register path must not run the device any more…
        write32(&mut t, mmio::QUEUE_NOTIFY, 0);
        assert_eq!(notified(&log), Vec::<u16>::new());

        // …but the worker's direct call still does.
        t.queue_notify(0);
        assert_eq!(notified(&log), vec![0]);

        // Handing the queue back restores the synchronous path.
        t.restore_queue_notify(0);
        assert!(!t.is_queue_notify_offloaded(0));
        write32(&mut t, mmio::QUEUE_NOTIFY, 0);
        assert_eq!(notified(&log), vec![0, 0]);
    }

    #[test]
    fn offloading_a_queue_the_device_lacks_is_refused() {
        let (mut t, _) = transport();
        assert_eq!(t.num_queues(), 1);
        assert!(!t.offload_queue_notify(1));
        assert!(!t.offload_queue_notify(u16::MAX));
        assert!(!t.is_queue_notify_offloaded(1));
    }

    #[test]
    fn offload_survives_a_device_reset() {
        let (mut t, log) = transport_with_notify_log();
        let ring = SplitRing::layout(0x1000, 16);
        bring_up(&mut t, &ring);
        assert!(t.offload_queue_notify(0));

        write32(&mut t, mmio::STATUS, 0);
        assert_eq!(t.status(), 0);
        // Host wiring is untouched by the guest-driven reset, so the worker
        // keeps being the only thing that may run the device.
        assert!(t.is_queue_notify_offloaded(0));

        bring_up(&mut t, &ring);
        write32(&mut t, mmio::QUEUE_NOTIFY, 0);
        assert_eq!(notified(&log), Vec::<u16>::new());
        t.queue_notify(0);
        assert_eq!(notified(&log), vec![0]);
    }

    /// Out-of-range indices reach the inline path even for an offloaded device
    /// (KVM's datamatch only swallows the exact queue index), and are dropped
    /// there without touching the device.
    #[test]
    fn bogus_notify_values_stay_harmless_when_offloaded() {
        let (mut t, log) = transport_with_notify_log();
        let ring = SplitRing::layout(0x1000, 16);
        bring_up(&mut t, &ring);
        assert!(t.offload_queue_notify(0));

        for value in [1u32, 0xffff, 0xffff_ffff, 0x1_0000] {
            write32(&mut t, mmio::QUEUE_NOTIFY, value);
        }
        assert_eq!(notified(&log), Vec::<u16>::new());
        assert_eq!(t.status() & status::DEVICE_NEEDS_RESET, 0);
    }

    #[test]
    fn notify_failure_sets_device_needs_reset() {
        let (mut t, _) = transport_with(TestDevice {
            fail_notify: true,
            ..Default::default()
        });
        let ring = SplitRing::layout(0x1000, 16);
        bring_up(&mut t, &ring);
        write32(&mut t, mmio::QUEUE_NOTIFY, 0);
        assert_ne!(t.status() & status::DEVICE_NEEDS_RESET, 0);
    }

    // --------------------------------------------------------- interrupts

    #[test]
    fn interrupt_status_and_ack() {
        let (mut t, _) = transport();
        assert_eq!(read32(&mut t, mmio::INTERRUPT_STATUS), 0);
        t.state.needs_reset();
        assert_eq!(read32(&mut t, mmio::INTERRUPT_STATUS), mmio::INT_CONFIG);
        assert_eq!(read32(&mut t, mmio::CONFIG_GENERATION), 1);
        write32(&mut t, mmio::INTERRUPT_ACK, mmio::INT_CONFIG);
        assert_eq!(read32(&mut t, mmio::INTERRUPT_STATUS), 0);
    }

    // --------------------------------------------------------------- reset

    #[test]
    fn reset_restores_pristine_register_state() {
        let (mut t, _) = transport();
        let ring = SplitRing::layout(0x1000, 16);
        bring_up(&mut t, &ring);
        t.state.needs_reset();
        assert!(t.is_activated());

        write32(&mut t, mmio::STATUS, 0);

        assert_eq!(t.status(), 0);
        assert!(!t.is_activated());
        assert_eq!(t.interrupt_status(), 0);
        write32(&mut t, mmio::QUEUE_SEL, 0);
        assert_eq!(read32(&mut t, mmio::QUEUE_NUM_MAX), 16);
        assert_eq!(read32(&mut t, mmio::QUEUE_READY), 0);
        assert_eq!(read32(&mut t, mmio::QUEUE_DESC_LOW), 0);
        assert_eq!(read32(&mut t, mmio::QUEUE_DRIVER_LOW), 0);
        assert_eq!(read32(&mut t, mmio::QUEUE_DEVICE_LOW), 0);
        // Selectors are pristine too, so DEVICE_FEATURES serves window 0.
        assert_eq!(
            read32(&mut t, mmio::DEVICE_FEATURES),
            (VIRTIO_F_VERSION_1 | FEATURE_A | FEATURE_HIGH) as u32
        );
        // The whole bring-up works again after a reset.
        bring_up(&mut t, &ring);
        assert!(t.is_activated());
    }

    // -------------------------------------------------------- config space

    #[test]
    fn config_space_is_delegated_to_the_device() {
        let (mut t, _) = transport();
        let mut data = [0u8; 4];
        t.read(mmio::CONFIG_SPACE, &mut data);
        assert_eq!(data, [0xaa, 0xbb, 0xcc, 0xdd]);

        t.write(mmio::CONFIG_SPACE + 1, &[0x11]);
        t.read(mmio::CONFIG_SPACE, &mut data);
        assert_eq!(data, [0xaa, 0x11, 0xcc, 0xdd]);

        // Reads past the end of the config space are zeroes, not a panic.
        let mut far = [0xffu8; 8];
        t.read(mmio::CONFIG_SPACE + 0x800, &mut far);
        assert_eq!(far, [0u8; 8]);
        t.write(mmio::CONFIG_SPACE + 0x800, &[1, 2, 3, 4, 5, 6, 7, 8]);
    }

    #[test]
    fn config_space_accepts_1_2_and_8_byte_accesses() {
        let (mut t, _) = transport();
        for width in [1usize, 2, 4, 8] {
            let mut data = vec![0xffu8; width];
            t.read(mmio::CONFIG_SPACE, &mut data);
            assert_eq!(data[0], 0xaa, "width {width}");
        }
    }

    // ----------------------------------------------- malformed accesses

    #[test]
    fn misaligned_and_odd_width_register_accesses_are_refused() {
        let (mut t, _) = transport();
        // Unaligned read returns zeroes instead of a shifted register.
        let mut data = [0xffu8; 4];
        t.read(mmio::MAGIC_VALUE + 1, &mut data);
        assert_eq!(data, [0, 0, 0, 0]);

        // 1- and 8-byte register accesses are refused.
        let mut byte = [0xffu8; 1];
        t.read(mmio::MAGIC_VALUE, &mut byte);
        assert_eq!(byte, [0]);
        let mut wide = [0xffu8; 8];
        t.read(mmio::MAGIC_VALUE, &mut wide);
        assert_eq!(wide, [0u8; 8]);

        // A misaligned STATUS write must not change the state machine.
        t.write(mmio::STATUS + 2, &1u32.to_le_bytes());
        t.write(mmio::STATUS, &[1u8]);
        assert_eq!(t.status(), 0);
    }

    #[test]
    fn writing_read_only_registers_is_ignored() {
        let (mut t, _) = transport();
        write32(&mut t, mmio::MAGIC_VALUE, 0);
        write32(&mut t, mmio::VERSION_REG, 1);
        write32(&mut t, mmio::DEVICE_ID, 0);
        write32(&mut t, mmio::QUEUE_NUM_MAX, 4);
        assert_eq!(read32(&mut t, mmio::MAGIC_VALUE), mmio::MAGIC);
        assert_eq!(read32(&mut t, mmio::VERSION_REG), 2);
        assert_eq!(read32(&mut t, mmio::DEVICE_ID), DeviceType::Block.id());
        assert_eq!(read32(&mut t, mmio::QUEUE_NUM_MAX), 16);
    }

    #[test]
    fn unknown_offsets_read_zero_and_ignore_writes() {
        let (mut t, _) = transport();
        assert_eq!(read32(&mut t, 0x0c0), 0);
        write32(&mut t, 0x0c0, 0xdead_beef);
        assert_eq!(read32(&mut t, 0x0c0), 0);
    }

    // ------------------------------------------------------------ helpers

    fn bring_up_features(t: &mut MmioTransport) {
        write32(t, mmio::DRIVER_FEATURES_SEL, 0);
        write32(t, mmio::DRIVER_FEATURES, FEATURE_A as u32);
        write32(t, mmio::DRIVER_FEATURES_SEL, 1);
        write32(t, mmio::DRIVER_FEATURES, (VIRTIO_F_VERSION_1 >> 32) as u32);
        write32(t, mmio::STATUS, status::ACKNOWLEDGE);
        write32(t, mmio::STATUS, status::ACKNOWLEDGE | status::DRIVER);
        write32(
            t,
            mmio::STATUS,
            status::ACKNOWLEDGE | status::DRIVER | status::FEATURES_OK,
        );
    }

    fn program_ring(t: &mut MmioTransport, ring: &SplitRing) {
        write32(t, mmio::QUEUE_SEL, 0);
        write32(t, mmio::QUEUE_NUM, u32::from(ring.size()));
        write32(t, mmio::QUEUE_DESC_LOW, ring.desc_table() as u32);
        write32(t, mmio::QUEUE_DESC_HIGH, (ring.desc_table() >> 32) as u32);
        write32(t, mmio::QUEUE_DRIVER_LOW, ring.driver_area() as u32);
        write32(
            t,
            mmio::QUEUE_DRIVER_HIGH,
            (ring.driver_area() >> 32) as u32,
        );
        write32(t, mmio::QUEUE_DEVICE_LOW, ring.device_area() as u32);
        write32(
            t,
            mmio::QUEUE_DEVICE_HIGH,
            (ring.device_area() >> 32) as u32,
        );
        write32(t, mmio::QUEUE_READY, 1);
    }

    fn finish_bring_up(t: &mut MmioTransport) {
        write32(
            t,
            mmio::STATUS,
            status::ACKNOWLEDGE | status::DRIVER | status::FEATURES_OK | status::DRIVER_OK,
        );
    }

    fn bring_up(t: &mut MmioTransport, ring: &SplitRing) {
        bring_up_features(t);
        program_ring(t, ring);
        finish_bring_up(t);
    }
}
