//! Transport-independent virtio device state.
//!
//! Everything a virtio transport must do that is *not* register decoding lives
//! here: the offered/negotiated feature words and their 32-bit selector
//! windows, the device-status state machine, one [`QueueConfig`] per virtqueue,
//! activation (building validated queues and handing over [`DeviceResources`]),
//! reset, `DEVICE_NEEDS_RESET`, and the queue-notify offload bookkeeping.
//!
//! The two transports on top of it —
//! [`MmioTransport`](crate::transport::MmioTransport) and
//! [`PciTransport`](crate::pci::PciTransport) — are then only address decoders:
//! they map an offset and an access width onto these operations. That is the
//! point of the split. virtio-mmio and virtio-pci disagree about *where* the
//! registers are, their widths and how interrupts are acknowledged, but they
//! agree completely about what the state machine does, and a second copy of
//! "may the driver set DRIVER_OK here?" would inevitably drift from the first.
//!
//! Nothing in this module is guest-trusted: every value passed in is still a
//! raw register write, validated here (or, for queue geometry, by
//! [`QueueConfig::build`]).

use std::sync::Arc;

use crate::device::{DeviceResources, DeviceType, VirtioDevice};
use crate::interrupt::{Interrupt, IrqLine, LineInterrupt};
use crate::queue::QueueConfig;
use crate::status;
use crate::transport::TransportError;
use crate::{GuestMem, MAX_QUEUE_SIZE, VIRTIO_F_VERSION_1};

/// The shared, transport-independent half of a virtio transport.
pub struct TransportState {
    /// Transport name, only used to label log records ("virtio-mmio" /
    /// "virtio-pci").
    kind: &'static str,
    /// Slot (mmio window index / PCI device number), only used in log records.
    slot: usize,
    device: Box<dyn VirtioDevice>,
    device_type: DeviceType,
    mem: Arc<GuestMem>,
    interrupt: Arc<LineInterrupt>,

    /// Cached because the offered feature set cannot change at runtime.
    device_features: u64,
    device_features_sel: u32,
    driver_features: u64,
    driver_features_sel: u32,

    queues: Vec<QueueConfig>,
    queue_sel: u32,

    /// One flag per queue: true when a host notification primitive (ioeventfd)
    /// owns this queue's notifications, so the register path must not run the
    /// device itself. Host wiring, not guest state — survives `reset`.
    notify_offloaded: Vec<bool>,

    status: u32,
    activated: bool,
}

impl TransportState {
    /// Validates the device against the transport contract and takes ownership
    /// of it.
    ///
    /// Fails when the device violates that contract — these are host bugs found
    /// at VM construction time, never guest input.
    pub fn new(
        kind: &'static str,
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
            kind,
            slot,
            device,
            device_type,
            mem,
            interrupt: Arc::new(LineInterrupt::new(line)),
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

    // ------------------------------------------------------------ accessors

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

    /// The pending-interrupt word (`INTERRUPT_STATUS` / the PCI ISR byte).
    pub fn interrupt_status(&self) -> u32 {
        self.interrupt.status()
    }

    /// The shared interrupt object, for the transport's acknowledge path.
    pub fn interrupt(&self) -> &Arc<LineInterrupt> {
        &self.interrupt
    }

    /// The device behind this slot, for inspection (tests, `entangled doctor`).
    pub fn device(&self) -> &dyn VirtioDevice {
        self.device.as_ref()
    }

    /// Device-specific config-space read, delegated straight to the device.
    pub fn read_config(&self, offset: u64, data: &mut [u8]) {
        self.device.read_config(offset, data);
    }

    /// Device-specific config-space write, delegated straight to the device.
    pub fn write_config(&mut self, offset: u64, data: &[u8]) {
        self.device.write_config(offset, data);
    }

    /// Number of virtqueues this slot exposes. The host uses it to decide how
    /// many notification primitives to create (MVP-307).
    pub fn num_queues(&self) -> usize {
        self.queues.len()
    }

    // ------------------------------------------------- queue-notify offload

    /// Hands ownership of queue `index`'s notifications to a host primitive:
    /// from now on the register path drops those writes and the host is
    /// expected to call [`Self::queue_notify`] from its worker thread instead.
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

    /// Gives queue `index`'s notifications back to the register path, used when
    /// the host tears its notification primitive down again.
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

    // -------------------------------------------------------------- features

    /// The selected 32-bit window of the offered feature word. Only windows 0
    /// and 1 are defined; anything else reads 0.
    pub fn device_features_window(&self) -> u32 {
        match self.device_features_sel {
            0 => self.device_features as u32,
            1 => (self.device_features >> 32) as u32,
            _ => 0,
        }
    }

    pub fn device_features_sel(&self) -> u32 {
        self.device_features_sel
    }

    pub fn set_device_features_sel(&mut self, value: u32) {
        self.device_features_sel = value;
    }

    pub fn driver_features_sel(&self) -> u32 {
        self.driver_features_sel
    }

    pub fn set_driver_features_sel(&mut self, value: u32) {
        self.driver_features_sel = value;
    }

    /// The selected 32-bit window of the word the driver has accepted so far.
    /// Serving it back is what makes the register observable; the spec marks
    /// `driver_feature` read-write in virtio-pci.
    pub fn driver_features_window(&self) -> u32 {
        match self.driver_features_sel {
            0 => self.driver_features as u32,
            1 => (self.driver_features >> 32) as u32,
            _ => 0,
        }
    }

    /// Guest write to the driver-features register, into the window the driver
    /// selected. Refused after FEATURES_OK — negotiation is over by then.
    pub fn write_driver_features(&mut self, value: u32) {
        if self.status & status::FEATURES_OK != 0 {
            tracing::warn!(
                transport = self.kind,
                slot = self.slot,
                device = ?self.device_type,
                "ignoring driver-features write after FEATURES_OK"
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
                transport = self.kind,
                slot = self.slot,
                device = ?self.device_type,
                sel,
                "ignoring driver-features write with an out-of-range selector"
            ),
        }
    }

    // ---------------------------------------------------------------- queues

    pub fn queue_sel(&self) -> u32 {
        self.queue_sel
    }

    pub fn set_queue_sel(&mut self, value: u32) {
        self.queue_sel = value;
    }

    /// The queue the driver has selected, if it exists.
    pub fn selected_queue(&self) -> Option<&QueueConfig> {
        usize::try_from(self.queue_sel)
            .ok()
            .and_then(|index| self.queues.get(index))
    }

    /// Applies `edit` to the selected queue's configuration.
    ///
    /// Two guest-driven conditions are handled here so no transport repeats
    /// them: geometry may not change while the device is live (the queues have
    /// already been handed to the device), and the selector may point at a queue
    /// that does not exist. Both are logged and dropped, never fatal.
    pub fn edit_selected_queue(&mut self, register: &str, edit: impl FnOnce(&mut QueueConfig)) {
        if self.activated {
            tracing::warn!(
                transport = self.kind,
                slot = self.slot,
                device = ?self.device_type,
                register,
                "ignoring queue reconfiguration after DRIVER_OK"
            );
            return;
        }
        let (kind, slot, device_type, queue_sel, count) = (
            self.kind,
            self.slot,
            self.device_type,
            self.queue_sel,
            self.queues.len(),
        );
        match usize::try_from(queue_sel)
            .ok()
            .and_then(|index| self.queues.get_mut(index))
        {
            Some(queue) => edit(queue),
            None => tracing::warn!(
                transport = kind,
                slot,
                device = ?device_type,
                register,
                queue_sel,
                queues = count,
                "ignoring write with an out-of-range queue selector"
            ),
        }
    }

    // -------------------------------------------------------------- notify

    /// Runs the device for the queue named by a raw notify value.
    ///
    /// The single entry point for kicks, whichever way they arrive: the vCPU
    /// exit path calls it for non-offloaded queues, the device's worker thread
    /// calls it when its ioeventfd fires (MVP-307). `value` is the raw register
    /// value, i.e. still guest-controlled and validated here.
    pub fn queue_notify(&mut self, value: u32) {
        if !self.activated {
            tracing::warn!(
                transport = self.kind,
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
                    transport = self.kind,
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
                transport = self.kind,
                slot = self.slot,
                device = ?self.device_type,
                queue = index,
                %error,
                "device failed to process a queue notification"
            );
            self.needs_reset();
        }
    }

    /// A notify that arrived through a register write rather than through the
    /// host primitive that owns it.
    ///
    /// When the queue's kicks are offloaded the write is dropped: KVM normally
    /// completes it in the kernel, so reaching userspace at all means the
    /// registration did not apply to this access (a bogus queue index, or a
    /// width/address KVM does not match on). Running the device here too would
    /// double-process the ring, so the offloaded queue's worker stays the only
    /// caller. Everything else runs inline on the vCPU thread, which is also the
    /// whole synchronous fallback path.
    pub fn queue_notify_from_register(&mut self, value: u32) {
        if let Ok(index) = u16::try_from(value) {
            if self.is_queue_notify_offloaded(index) {
                tracing::debug!(
                    transport = self.kind,
                    slot = self.slot,
                    device = ?self.device_type,
                    queue = index,
                    "dropping queue-notify register write for an offloaded queue"
                );
                return;
            }
        }
        self.queue_notify(value);
    }

    // -------------------------------------------------------------- status

    /// Guest write to the device-status register.
    ///
    /// A write of 0 is a reset request. Reserved bits are dropped before
    /// anything looks at the value, so the register the guest reads back never
    /// contains a bit the spec does not define; illegal transitions are ignored
    /// (see [`status::write_is_valid`]).
    pub fn write_status(&mut self, value: u32) {
        if value == 0 {
            tracing::debug!(
                transport = self.kind,
                slot = self.slot,
                device = ?self.device_type,
                "driver requested device reset"
            );
            self.reset();
            return;
        }
        // A write of *only* reserved bits is not a reset request — it is simply
        // meaningless, so it is ignored rather than masked down to 0.
        let value = match value & status::KNOWN {
            0 => {
                tracing::warn!(
                    transport = self.kind,
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
                        transport = self.kind,
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
                transport = self.kind,
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
                    transport = self.kind,
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
                transport = self.kind,
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
                transport = self.kind,
                slot = self.slot,
                device = ?self.device_type,
                driver_features = format_args!("{:#x}", self.driver_features),
                "driver did not accept VIRTIO_F_VERSION_1; refusing FEATURES_OK"
            );
            return false;
        }
        if !self.device.ack_features(negotiated) {
            tracing::warn!(
                transport = self.kind,
                slot = self.slot,
                device = ?self.device_type,
                negotiated = format_args!("{negotiated:#x}"),
                "device vetoed the negotiated feature set; refusing FEATURES_OK"
            );
            return false;
        }
        tracing::debug!(
            transport = self.kind,
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
                        transport = self.kind,
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
                    transport = self.kind,
                    slot = self.slot,
                    device = ?self.device_type,
                    queues = self.queues.len(),
                    "virtio device activated"
                );
            }
            Err(error) => {
                tracing::error!(
                    transport = self.kind,
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
    pub fn needs_reset(&mut self) {
        self.status |= status::DEVICE_NEEDS_RESET;
        if let Err(error) = self.interrupt.signal_config_change() {
            tracing::error!(
                transport = self.kind,
                slot = self.slot,
                device = ?self.device_type,
                %error,
                "failed to notify the driver about DEVICE_NEEDS_RESET"
            );
        }
    }
}
