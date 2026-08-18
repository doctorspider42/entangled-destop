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

use crate::device::{DeviceResources, DeviceType, VirtioDevice};
use crate::interrupt::{Interrupt, IrqLine, MmioInterrupt};
use crate::mmio;
use crate::queue::QueueConfig;
use crate::status;
use crate::{GuestMem, MAX_QUEUE_SIZE, VIRTIO_F_VERSION_1};

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
        "device type {device_type:?} declares queue {index} with max size {max_size}; \
         must be a non-zero power of two, at most {MAX_QUEUE_SIZE}"
    )]
    InvalidQueueMaxSize {
        device_type: DeviceType,
        index: usize,
        max_size: u16,
    },
}

/// One virtio-mmio device slot: registers plus the device behind them.
pub struct MmioTransport {
    /// Index of the mmio slot, only used to label log records.
    slot: usize,
    device: Box<dyn VirtioDevice>,
    device_type: DeviceType,
    mem: Arc<GuestMem>,
    interrupt: Arc<MmioInterrupt>,

    /// Cached because the offered feature set cannot change at runtime.
    device_features: u64,
    device_features_sel: u32,
    driver_features: u64,
    driver_features_sel: u32,

    queues: Vec<QueueConfig>,
    queue_sel: u32,

    /// One flag per queue: true when a host notification primitive (ioeventfd)
    /// owns this queue's `QUEUE_NOTIFY` writes, so the register path must not
    /// run the device itself. Host wiring, not guest state — survives `reset`.
    notify_offloaded: Vec<bool>,

    status: u32,
    activated: bool,
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
        let device_type = device.device_type();
        let device_features = device.device_features();
        if device_features & VIRTIO_F_VERSION_1 == 0 {
            return Err(TransportError::MissingVersion1 { device_type });
        }
        let max_sizes = device.queue_max_sizes();
        if max_sizes.is_empty() {
            return Err(TransportError::NoQueues { device_type });
        }
        for (index, &max_size) in max_sizes.iter().enumerate() {
            if max_size == 0 || !max_size.is_power_of_two() || max_size > MAX_QUEUE_SIZE {
                return Err(TransportError::InvalidQueueMaxSize {
                    device_type,
                    index,
                    max_size,
                });
            }
        }
        let queues: Vec<QueueConfig> = max_sizes.iter().copied().map(QueueConfig::new).collect();
        let notify_offloaded = vec![false; queues.len()];

        Ok(Self {
            slot,
            device,
            device_type,
            mem,
            interrupt: Arc::new(MmioInterrupt::new(line)),
            device_features,
            device_features_sel: 0,
            driver_features: 0,
            driver_features_sel: 0,
            queues,
            queue_sel: 0,
            notify_offloaded,
            status: 0,
            activated: false,
        })
    }

    pub fn device_type(&self) -> DeviceType {
        self.device_type
    }

    pub fn slot(&self) -> usize {
        self.slot
    }

    pub fn status(&self) -> u32 {
        self.status
    }

    pub fn is_activated(&self) -> bool {
        self.activated
    }

    pub fn interrupt_status(&self) -> u32 {
        self.interrupt.status()
    }

    /// The device behind this slot, for inspection (tests, `entangled doctor`).
    pub fn device(&self) -> &dyn VirtioDevice {
        self.device.as_ref()
    }

    /// Number of virtqueues this slot exposes. The host uses it to decide how
    /// many notification primitives to create (MVP-307).
    pub fn num_queues(&self) -> usize {
        self.queues.len()
    }

    /// Hands ownership of queue `index`'s `QUEUE_NOTIFY` writes to a host
    /// notification primitive: from now on the register path drops those writes
    /// (KVM normally swallows them anyway) and the host is expected to call
    /// [`Self::queue_notify`] from its worker thread instead.
    ///
    /// Returns false when this device has no such queue, so the host can fall
    /// back to the synchronous path instead of silently losing kicks.
    pub fn offload_queue_notify(&mut self, index: u16) -> bool {
        match self.notify_offloaded.get_mut(usize::from(index)) {
            Some(flag) => {
                *flag = true;
                true
            }
            None => false,
        }
    }

    /// Gives queue `index`'s `QUEUE_NOTIFY` writes back to the register path,
    /// used when the host tears its notification primitive down again.
    pub fn restore_queue_notify(&mut self, index: u16) {
        if let Some(flag) = self.notify_offloaded.get_mut(usize::from(index)) {
            *flag = false;
        }
    }

    /// Whether queue `index`'s kicks arrive out-of-band (ioeventfd) rather than
    /// through an MMIO exit.
    pub fn is_queue_notify_offloaded(&self, index: u16) -> bool {
        self.notify_offloaded
            .get(usize::from(index))
            .copied()
            .unwrap_or(false)
    }

    // ---------------------------------------------------------------- reads

    /// Guest read at `offset` inside the slot.
    pub fn read(&mut self, offset: u64, data: &mut [u8]) {
        if offset >= mmio::CONFIG_SPACE {
            self.device.read_config(offset - mmio::CONFIG_SPACE, data);
            return;
        }
        data.fill(0);
        if data.len() != 4 || offset % 4 != 0 {
            tracing::warn!(
                slot = self.slot,
                device = ?self.device_type,
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
        match offset {
            mmio::MAGIC_VALUE => mmio::MAGIC,
            mmio::VERSION_REG => mmio::VERSION,
            mmio::DEVICE_ID => self.device_type.id(),
            mmio::VENDOR_ID => mmio::VMHOST_VENDOR_ID,
            mmio::DEVICE_FEATURES => match self.device_features_sel {
                0 => self.device_features as u32,
                1 => (self.device_features >> 32) as u32,
                // Only two 32-bit windows are defined; anything else reads 0.
                _ => 0,
            },
            mmio::QUEUE_NUM_MAX => self.selected_queue().map_or(0, |q| u32::from(q.max_size())),
            mmio::QUEUE_READY => {
                u32::from(self.selected_queue().is_some_and(QueueConfig::is_ready))
            }
            // Spec marks the ring address registers write-only; serving reads
            // back is harmless and keeps the register state observable.
            mmio::QUEUE_DESC_LOW => self.selected_queue().map_or(0, |q| q.desc_table() as u32),
            mmio::QUEUE_DESC_HIGH => self
                .selected_queue()
                .map_or(0, |q| (q.desc_table() >> 32) as u32),
            mmio::QUEUE_DRIVER_LOW => self.selected_queue().map_or(0, |q| q.driver_area() as u32),
            mmio::QUEUE_DRIVER_HIGH => self
                .selected_queue()
                .map_or(0, |q| (q.driver_area() >> 32) as u32),
            mmio::QUEUE_DEVICE_LOW => self.selected_queue().map_or(0, |q| q.device_area() as u32),
            mmio::QUEUE_DEVICE_HIGH => self
                .selected_queue()
                .map_or(0, |q| (q.device_area() >> 32) as u32),
            mmio::INTERRUPT_STATUS => self.interrupt.status(),
            mmio::STATUS => self.status,
            mmio::CONFIG_GENERATION => self.interrupt.generation(),
            // No shared-memory regions: length reads all-ones per spec so
            // drivers recognize "no such region" (base likewise).
            mmio::SHM_LEN_LOW | mmio::SHM_LEN_HIGH | mmio::SHM_BASE_LOW | mmio::SHM_BASE_HIGH => {
                u32::MAX
            }
            _ => {
                tracing::debug!(
                    slot = self.slot,
                    device = ?self.device_type,
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
            self.device.write_config(offset - mmio::CONFIG_SPACE, data);
            return;
        }
        if data.len() != 4 || offset % 4 != 0 {
            tracing::warn!(
                slot = self.slot,
                device = ?self.device_type,
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
            mmio::DEVICE_FEATURES_SEL => self.device_features_sel = value,
            mmio::DRIVER_FEATURES => self.write_driver_features(value),
            mmio::DRIVER_FEATURES_SEL => self.driver_features_sel = value,
            mmio::QUEUE_SEL => self.queue_sel = value,
            mmio::QUEUE_NUM => self.write_queue_num(value),
            mmio::QUEUE_READY => self.write_queue_ready(value),
            mmio::QUEUE_NOTIFY => self.queue_notify_from_register(value),
            mmio::INTERRUPT_ACK => self.interrupt.ack(value),
            mmio::STATUS => self.write_status(value),
            mmio::QUEUE_DESC_LOW
            | mmio::QUEUE_DESC_HIGH
            | mmio::QUEUE_DRIVER_LOW
            | mmio::QUEUE_DRIVER_HIGH
            | mmio::QUEUE_DEVICE_LOW
            | mmio::QUEUE_DEVICE_HIGH => self.write_queue_address(offset, value),
            _ => tracing::warn!(
                slot = self.slot,
                device = ?self.device_type,
                offset,
                value,
                "guest wrote a read-only or unimplemented virtio-mmio register"
            ),
        }
    }

    fn write_driver_features(&mut self, value: u32) {
        if self.status & status::FEATURES_OK != 0 {
            tracing::warn!(
                slot = self.slot,
                device = ?self.device_type,
                "ignoring DRIVER_FEATURES write after FEATURES_OK"
            );
            return;
        }
        match self.driver_features_sel {
            0 => {
                self.driver_features =
                    (self.driver_features & 0xffff_ffff_0000_0000) | u64::from(value)
            }
            1 => {
                self.driver_features =
                    (self.driver_features & 0x0000_0000_ffff_ffff) | (u64::from(value) << 32)
            }
            sel => tracing::warn!(
                slot = self.slot,
                device = ?self.device_type,
                sel,
                "ignoring DRIVER_FEATURES write with an out-of-range DRIVER_FEATURES_SEL"
            ),
        }
    }

    fn write_queue_num(&mut self, value: u32) {
        if self.reject_queue_reconfiguration("QUEUE_NUM") {
            return;
        }
        // Sizes beyond u16 cannot be valid; store 0 so `build()` rejects the
        // queue with a typed error instead of silently truncating.
        let size = u16::try_from(value).unwrap_or(0);
        if let Some(queue) = self.selected_queue_mut() {
            queue.set_size(size);
        } else {
            self.warn_bad_queue_sel("QUEUE_NUM");
        }
    }

    fn write_queue_ready(&mut self, value: u32) {
        if self.reject_queue_reconfiguration("QUEUE_READY") {
            return;
        }
        if let Some(queue) = self.selected_queue_mut() {
            queue.set_ready(value == 1);
        } else {
            self.warn_bad_queue_sel("QUEUE_READY");
        }
    }

    fn write_queue_address(&mut self, offset: u64, value: u32) {
        if self.reject_queue_reconfiguration("queue address") {
            return;
        }
        let Some(queue) = self.selected_queue_mut() else {
            self.warn_bad_queue_sel("queue address");
            return;
        };
        match offset {
            mmio::QUEUE_DESC_LOW => queue.set_desc_table_low(value),
            mmio::QUEUE_DESC_HIGH => queue.set_desc_table_high(value),
            mmio::QUEUE_DRIVER_LOW => queue.set_driver_area_low(value),
            mmio::QUEUE_DRIVER_HIGH => queue.set_driver_area_high(value),
            mmio::QUEUE_DEVICE_LOW => queue.set_device_area_low(value),
            mmio::QUEUE_DEVICE_HIGH => queue.set_device_area_high(value),
            // Unreachable: the caller only routes the six offsets above.
            _ => (),
        }
    }

    /// `QUEUE_NOTIFY` write that arrived through an MMIO exit.
    ///
    /// When the queue's kicks are offloaded to a host primitive the write is
    /// dropped: KVM normally completes it in the kernel, so reaching userspace
    /// at all means either the datamatch did not apply (a bogus queue index) or
    /// the guest wrote a width KVM does not match on. Running the device here
    /// too would double-process the ring, so the offloaded queue's worker stays
    /// the only caller. Everything else runs inline on the vCPU thread, which
    /// is also the whole synchronous fallback path.
    fn queue_notify_from_register(&mut self, value: u32) {
        if let Ok(index) = u16::try_from(value) {
            if self.is_queue_notify_offloaded(index) {
                tracing::debug!(
                    slot = self.slot,
                    device = ?self.device_type,
                    queue = index,
                    "dropping QUEUE_NOTIFY register write for an offloaded queue"
                );
                return;
            }
        }
        self.queue_notify(value);
    }

    /// Runs the device for the queue named by a `QUEUE_NOTIFY` value.
    ///
    /// The single entry point for kicks, whichever way they arrive: the vCPU
    /// exit path calls it for non-offloaded queues, the device's worker thread
    /// calls it when its ioeventfd fires (MVP-307). `value` is the raw register
    /// value, i.e. still guest-controlled and validated here.
    pub fn queue_notify(&mut self, value: u32) {
        if !self.activated {
            tracing::warn!(
                slot = self.slot,
                device = ?self.device_type,
                value,
                "queue notify before DRIVER_OK, ignoring"
            );
            return;
        }
        let index = match u16::try_from(value) {
            Ok(index) if usize::from(index) < self.queues.len() => index,
            _ => {
                tracing::warn!(
                    slot = self.slot,
                    device = ?self.device_type,
                    value,
                    queues = self.queues.len(),
                    "queue notify for a queue this device does not have, ignoring"
                );
                return;
            }
        };
        if let Err(error) = self.device.notify(index) {
            tracing::error!(
                slot = self.slot,
                device = ?self.device_type,
                queue = index,
                %error,
                "device failed to process a queue notification"
            );
            self.needs_reset();
        }
    }

    fn write_status(&mut self, value: u32) {
        if value == 0 {
            tracing::debug!(
                slot = self.slot,
                device = ?self.device_type,
                "driver requested device reset"
            );
            self.reset();
            return;
        }
        // Reserved bits are dropped before anything looks at the value, so the
        // register the guest reads back never contains a bit the spec does not
        // define. A write of *only* reserved bits is not a reset request — it is
        // simply meaningless, so it is ignored rather than masked down to 0.
        let value = match value & status::KNOWN {
            0 => {
                tracing::warn!(
                    slot = self.slot,
                    device = ?self.device_type,
                    requested = format_args!("{value:#x}"),
                    "ignoring device status write with no known bits"
                );
                return;
            }
            masked => {
                if masked != value {
                    tracing::warn!(
                        slot = self.slot,
                        device = ?self.device_type,
                        requested = format_args!("{value:#x}"),
                        kept = format_args!("{masked:#x}"),
                        "dropping reserved bits from a device status write"
                    );
                }
                masked
            }
        };
        if !status::write_is_valid(self.status, value) {
            tracing::warn!(
                slot = self.slot,
                device = ?self.device_type,
                current = self.status,
                requested = value,
                "ignoring illegal device status transition"
            );
            return;
        }
        let added = value & !self.status;
        let mut accepted = value;

        if added & status::FEATURES_OK != 0 && !self.negotiate_features() {
            // Spec: the device leaves FEATURES_OK unset when it cannot accept
            // the driver's subset. The driver is expected to give up.
            accepted &= !status::FEATURES_OK;
        }
        self.status = accepted;

        if added & status::DRIVER_OK != 0 {
            if self.status & status::FEATURES_OK == 0 {
                tracing::warn!(
                    slot = self.slot,
                    device = ?self.device_type,
                    "DRIVER_OK without accepted features, not activating"
                );
            } else if !self.activated {
                self.activate();
            }
        }
        if added & status::FAILED != 0 {
            tracing::warn!(
                slot = self.slot,
                device = ?self.device_type,
                "driver gave up on this device (FAILED)"
            );
        }
    }

    /// Computes and hands the negotiated feature subset to the device.
    /// Returns false when FEATURES_OK must be refused.
    fn negotiate_features(&mut self) -> bool {
        let negotiated = self.driver_features & self.device_features;
        if negotiated & VIRTIO_F_VERSION_1 == 0 {
            tracing::warn!(
                slot = self.slot,
                device = ?self.device_type,
                driver_features = format_args!("{:#x}", self.driver_features),
                "driver did not accept VIRTIO_F_VERSION_1; refusing FEATURES_OK"
            );
            return false;
        }
        if !self.device.ack_features(negotiated) {
            tracing::warn!(
                slot = self.slot,
                device = ?self.device_type,
                negotiated = format_args!("{negotiated:#x}"),
                "device vetoed the negotiated feature set; refusing FEATURES_OK"
            );
            return false;
        }
        tracing::debug!(
            slot = self.slot,
            device = ?self.device_type,
            negotiated = format_args!("{negotiated:#x}"),
            "feature negotiation complete"
        );
        true
    }

    fn activate(&mut self) {
        let mut queues = Vec::with_capacity(self.queues.len());
        for (index, config) in self.queues.iter().enumerate() {
            match config.build(&self.mem) {
                Ok(queue) => queues.push(queue),
                Err(error) => {
                    tracing::error!(
                        slot = self.slot,
                        device = ?self.device_type,
                        queue = index,
                        %error,
                        "driver programmed an unusable virtqueue; not activating"
                    );
                    self.needs_reset();
                    return;
                }
            }
        }
        let resources = DeviceResources {
            mem: Arc::clone(&self.mem),
            queues,
            interrupt: Arc::clone(&self.interrupt) as Arc<dyn Interrupt>,
        };
        match self.device.activate(resources) {
            Ok(()) => {
                self.activated = true;
                tracing::info!(
                    slot = self.slot,
                    device = ?self.device_type,
                    queues = self.queues.len(),
                    "virtio device activated"
                );
            }
            Err(error) => {
                tracing::error!(
                    slot = self.slot,
                    device = ?self.device_type,
                    %error,
                    "device refused activation"
                );
                self.needs_reset();
            }
        }
    }

    /// Full device reset (MVP-303): everything returns to the state a freshly
    /// constructed transport is in, so a driver can start bring-up again.
    ///
    /// `notify_offloaded` is deliberately *not* cleared: it describes host
    /// wiring (which queue has an ioeventfd behind it), not guest state, and the
    /// same worker thread keeps serving the device across the reset.
    pub fn reset(&mut self) {
        self.device.reset();
        for queue in &mut self.queues {
            queue.reset();
        }
        self.device_features_sel = 0;
        self.driver_features = 0;
        self.driver_features_sel = 0;
        self.queue_sel = 0;
        self.status = 0;
        self.activated = false;
        self.interrupt.clear();
    }

    /// Tells the driver the device is broken and must be reset. The config
    /// change notification is how a spec-conforming driver notices.
    fn needs_reset(&mut self) {
        self.status |= status::DEVICE_NEEDS_RESET;
        if let Err(error) = self.interrupt.signal_config_change() {
            tracing::error!(
                slot = self.slot,
                device = ?self.device_type,
                %error,
                "failed to notify the driver about DEVICE_NEEDS_RESET"
            );
        }
    }

    fn selected_queue(&self) -> Option<&QueueConfig> {
        usize::try_from(self.queue_sel)
            .ok()
            .and_then(|index| self.queues.get(index))
    }

    fn selected_queue_mut(&mut self) -> Option<&mut QueueConfig> {
        usize::try_from(self.queue_sel)
            .ok()
            .and_then(|index| self.queues.get_mut(index))
    }

    /// Queue geometry may only change while the device is not live. Returns
    /// true when the write must be dropped.
    fn reject_queue_reconfiguration(&self, register: &str) -> bool {
        if self.activated {
            tracing::warn!(
                slot = self.slot,
                device = ?self.device_type,
                register,
                "ignoring queue reconfiguration after DRIVER_OK"
            );
            return true;
        }
        false
    }

    fn warn_bad_queue_sel(&self, register: &str) {
        tracing::warn!(
            slot = self.slot,
            device = ?self.device_type,
            register,
            queue_sel = self.queue_sel,
            queues = self.queues.len(),
            "ignoring write with an out-of-range QUEUE_SEL"
        );
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;
    use crate::device::DeviceError;
    use crate::testing::{self, SplitRing, TestIrqLine};

    const FEATURE_A: u64 = 1 << 3;
    const FEATURE_HIGH: u64 = 1 << 40;

    /// Queue indices the device was notified about, observable from outside the
    /// boxed `dyn VirtioDevice` the transport owns.
    type NotifyLog = Arc<Mutex<Vec<u16>>>;

    /// A device that records what the transport did to it.
    struct TestDevice {
        queue_sizes: Vec<u16>,
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
        t.needs_reset();
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
        t.needs_reset();
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
