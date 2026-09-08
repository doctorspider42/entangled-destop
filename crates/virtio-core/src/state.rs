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

use crate::device::{DeviceResources, DeviceType, ShmRegion, VirtioDevice};
use crate::interrupt::{IrqLine, LineInterrupt, TransportInterrupt};
use crate::queue::QueueConfig;
use crate::quiesce::Quiesce;
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
    /// How this device tells its driver something happened. A
    /// [`LineInterrupt`] for virtio-mmio and for virtio-pci without MSI-X, a
    /// [`MsixInterrupt`](crate::pci::MsixInterrupt) for virtio-pci with it.
    /// Nothing in this module can tell the difference, which is the point.
    interrupt: Arc<dyn TransportInterrupt>,

    /// Cached because the offered feature set cannot change at runtime.
    device_features: u64,
    device_features_sel: u32,
    driver_features: u64,
    driver_features_sel: u32,

    queues: Vec<QueueConfig>,
    queue_sel: u32,

    /// Shared-memory regions the device exposes, read once at construction
    /// because a device's region list is fixed for its life (VEN-2001). Empty
    /// for every device that has none, which is all of them today except a
    /// virtio-gpu whose renderer owns a host-visible window.
    shm_regions: Vec<ShmRegion>,
    /// `SHM_SEL` on virtio-mmio: which region the next `SHM_LEN`/`SHM_BASE`
    /// read describes. Guest state, so it resets with everything else.
    shm_sel: u32,
    /// Where the host actually placed each region, keyed by `shmid`. Filled in
    /// by the machine layer once it has an address; a region with no base is
    /// reported to the guest as *absent*, because telling a driver about a
    /// window that decodes nothing is worse than telling it there is none.
    shm_bases: Vec<(u8, u64)>,

    /// One flag per queue: true when a host notification primitive (ioeventfd)
    /// owns this queue's notifications, so the register path must not run the
    /// device itself. Host wiring, not guest state — survives `reset`.
    notify_offloaded: Vec<bool>,

    /// The pause gate handed to the device at activation, so a worker of its
    /// own (virtio-net's receive thread) stops touching guest memory while the
    /// VM is paused (ADR-0005). Host wiring, like `notify_offloaded`: it
    /// survives a reset, and a machine that never pauses leaves it open.
    quiesce: Arc<Quiesce>,

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
        Self::new_with_interrupt(kind, slot, device, mem, Arc::new(LineInterrupt::new(line)))
    }

    /// [`Self::new`] with an interrupt object built by the caller.
    ///
    /// virtio-pci uses it to install a [`MsixInterrupt`](crate::pci::MsixInterrupt),
    /// which needs a host MSI sink and a table size this module has no business
    /// knowing about. Everything downstream — negotiation, status, activation,
    /// reset — is identical, and virtio-mmio still goes through [`Self::new`]
    /// unchanged.
    pub fn new_with_interrupt(
        kind: &'static str,
        slot: usize,
        device: Box<dyn VirtioDevice>,
        mem: Arc<GuestMem>,
        interrupt: Arc<dyn TransportInterrupt>,
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
        let device_shm_regions = device.shm_regions();
        for region in &device_shm_regions {
            if region.len == 0 {
                return Err(TransportError::InvalidShmRegion {
                    device_type,
                    id: region.id,
                });
            }
        }

        Ok(Self {
            kind,
            slot,
            device,
            device_type,
            mem,
            interrupt,
            device_features,
            device_features_sel: 0,
            driver_features: 0,
            driver_features_sel: 0,
            queues,
            queue_sel: 0,
            shm_regions: device_shm_regions,
            shm_sel: 0,
            shm_bases: Vec::new(),
            notify_offloaded,
            quiesce: Quiesce::new(),
            status: 0,
            activated: false,
        })
    }

    /// Shares the VM's pause gate with this slot, so the device's own workers
    /// park with everything else (ADR-0005). Called once by the machine while
    /// it wires the bus; a transport that is never told keeps a private, always
    /// open gate and behaves exactly as it did before pause existed.
    pub fn set_quiesce(&mut self, quiesce: Arc<Quiesce>) {
        self.quiesce = quiesce;
    }

    /// The pause gate this slot hands to its device.
    pub fn quiesce(&self) -> &Arc<Quiesce> {
        &self.quiesce
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
    pub fn interrupt(&self) -> &Arc<dyn TransportInterrupt> {
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
            // Not a warning, and not a guest bug: since Linux 6.14 the modern
            // virtio-pci driver carries a 128-bit feature word
            // (`VIRTIO_FEATURES_DWORDS == 4`) and `vp_modern_set_extended_features`
            // walks selectors 0..=3 unconditionally, writing zeroes into the
            // windows it has nothing to put in. Ubuntu 26.04's kernel does this
            // for every device on every boot, so warning about it buries the
            // things worth reading in a boot log. Dropping the write is the
            // right answer — we offer no feature above bit 63, so there is
            // nothing for the driver to accept up there — and a *non-zero* write
            // to a window we do not implement is still worth a line.
            sel if value == 0 => tracing::trace!(
                transport = self.kind,
                slot = self.slot,
                device = ?self.device_type,
                sel,
                "driver cleared an extended feature window this device does not offer"
            ),
            sel => tracing::warn!(
                transport = self.kind,
                slot = self.slot,
                device = ?self.device_type,
                sel,
                value = format_args!("{value:#x}"),
                "ignoring a non-zero driver-features write with an out-of-range selector"
            ),
        }
    }

    // ---------------------------------------------------------------- queues

    pub fn queue_sel(&self) -> u32 {
        self.queue_sel
    }

    // ------------------------------------ shared-memory regions (VEN-2001)

    /// The device's shared-memory regions, in the order it declared them.
    pub fn shm_regions(&self) -> &[ShmRegion] {
        &self.shm_regions
    }

    /// Records where the host mapped region `id`. Called by the machine layer
    /// after it has allocated the window; until then the region reads as
    /// absent to the guest.
    pub fn set_shm_base(&mut self, id: u8, base: u64) {
        match self.shm_bases.iter_mut().find(|(slot, _)| *slot == id) {
            Some(slot) => slot.1 = base,
            None => self.shm_bases.push((id, base)),
        }
    }

    /// Where region `id` was placed, if anywhere.
    pub fn shm_base(&self, id: u8) -> Option<u64> {
        self.shm_bases
            .iter()
            .find(|(slot, _)| *slot == id)
            .map(|(_, base)| *base)
    }

    /// `SHM_SEL` (virtio-mmio only).
    pub fn set_shm_sel(&mut self, value: u32) {
        self.shm_sel = value;
    }

    /// The region `SHM_SEL` currently selects *and* the host has placed, or
    /// `None` — which the transport reports as all-ones, the spec's "no such
    /// region".
    pub fn selected_shm(&self) -> Option<(ShmRegion, u64)> {
        let id = u8::try_from(self.shm_sel).ok()?;
        let region = *self.shm_regions.iter().find(|r| r.id == id)?;
        let base = self.shm_base(id)?;
        Some((region, base))
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
        if let Err(error) = self.activate_at(&[]) {
            tracing::error!(
                transport = self.kind,
                slot = self.slot,
                device = ?self.device_type,
                %error,
                "device did not activate"
            );
            self.needs_reset();
        }
    }

    /// Builds the queues and hands them to the device, optionally putting each
    /// one back at a saved position (ADR-0006).
    ///
    /// `positions` is empty on the ordinary bring-up path, where a freshly
    /// programmed ring starts at zero and the guest's own `avail.idx` is zero
    /// too. On a restore it carries what the device reported at save time, and
    /// the positions go in **before** the device sees the queues — a device
    /// that starts serving on activation must not first serve the chains it had
    /// already consumed.
    fn activate_at(&mut self, positions: &[crate::save::QueuePosition]) -> Result<(), String> {
        use virtio_queue::QueueT as _;

        let mut queues = Vec::with_capacity(self.queues.len());
        for (index, config) in self.queues.iter().enumerate() {
            let mut queue = config
                .build(&self.mem)
                .map_err(|e| format!("queue {index}: {e}"))?;
            if let Some(position) = positions.get(index) {
                queue.set_next_avail(position.next_avail);
                queue.set_next_used(position.next_used);
            }
            queues.push(queue);
        }
        let resources = DeviceResources {
            mem: Arc::clone(&self.mem),
            queues,
            interrupt: Arc::clone(&self.interrupt).as_interrupt(),
            quiesce: Arc::clone(&self.quiesce),
        };
        self.device.activate(resources).map_err(|e| e.to_string())?;
        self.activated = true;
        tracing::info!(
            transport = self.kind,
            slot = self.slot,
            device = ?self.device_type,
            queues = self.queues.len(),
            restored = !positions.is_empty(),
            "virtio device activated"
        );
        Ok(())
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
        self.shm_sel = 0;
        self.status = 0;
        self.activated = false;
        self.interrupt.clear();
    }

    /// **Machine** reset (ADR-0005): [`Self::reset`] plus the interrupt state a
    /// device reset deliberately keeps.
    ///
    /// The difference is [`TransportInterrupt::power_on_reset`]: after a reboot
    /// the function must look untouched to the new guest, down to
    /// `config_generation` and — on virtio-pci — the MSI-X table and control
    /// register. `notify_offloaded` still survives, for the same reason it
    /// survives a device reset: it describes the *host's* wiring, and the same
    /// worker keeps serving the device across the reboot.
    pub fn power_on_reset(&mut self) {
        self.reset();
        self.interrupt.power_on_reset();
    }

    // --------------------------------------------------- suspend and restore

    /// Everything this slot is, for a snapshot (ADR-0006).
    ///
    /// Called with the VM paused, so nothing is mid-request and the queue
    /// positions the device reports are stable. What comes out is exactly what
    /// `power_on_reset` would have thrown away, plus the two things it
    /// deliberately keeps (`config_generation` and the MSI-X table) — a
    /// restore has to put those back too, because the guest never saw them go.
    ///
    /// What is **not** in here: `notify_offloaded`, the pause gate, and the
    /// shared-memory region *list*. All three are host wiring or device shape,
    /// rebuilt around the restored transport by whoever attaches it, exactly as
    /// a reset rebuilds them. The region list's *placement* is recorded, but
    /// only so a machine that placed it elsewhere can be refused — see
    /// [`crate::save::TransportSaveState::shm_bases`].
    pub fn save(&self) -> crate::save::TransportSaveState {
        let positions = self.device.queue_positions();
        crate::save::TransportSaveState {
            device_type: self.device_type.id(),
            device_features: self.device_features,
            device_features_sel: self.device_features_sel,
            driver_features: self.driver_features,
            driver_features_sel: self.driver_features_sel,
            queue_sel: self.queue_sel,
            shm_sel: self.shm_sel,
            shm_bases: self.shm_bases.clone(),
            status: self.status,
            activated: self.activated,
            queues: self
                .queues
                .iter()
                .enumerate()
                .map(|(index, config)| {
                    config.to_state(positions.get(index).copied().unwrap_or_default())
                })
                .collect(),
            interrupt: self.interrupt.save_interrupt(),
            device: self.device.save_device(),
        }
    }

    /// Puts a saved slot back.
    ///
    /// The order is the order the guest did it in, because that is the only
    /// order the device is written to accept:
    ///
    /// 1. **Refuse a slot that is not this one.** A different device type or a
    ///    different offered feature set means the guest negotiated against a
    ///    machine this build did not rebuild, and everything after this point
    ///    would be a plausible-looking lie.
    /// 2. **Features before status.** `ack_features` is how the device learns
    ///    what it may do; a device activated before it knew would serve the
    ///    wrong ring layout.
    /// 3. **Queues, then activation.** The geometry goes back into the
    ///    `QueueConfig`s and is validated by the same `build` the guest's own
    ///    `DRIVER_OK` goes through — a snapshot with an out-of-bounds ring is
    ///    refused here, not trusted because it came from a file.
    /// 4. **The interrupt last**, so a pending bit restored into the ISR is not
    ///    cleared by the activation above it.
    pub fn load(
        &mut self,
        state: &crate::save::TransportSaveState,
    ) -> Result<(), crate::save::StateError> {
        use crate::save::StateError;

        if state.device_type != self.device_type.id() {
            return Err(StateError::DeviceType {
                snapshot: state.device_type,
                current: self.device_type.id(),
            });
        }
        if state.device_features != self.device_features {
            return Err(StateError::DeviceFeatures {
                snapshot: state.device_features,
                current: self.device_features,
            });
        }
        if state.queues.len() != self.queues.len() {
            return Err(StateError::QueueCount {
                snapshot: state.queues.len(),
                current: self.queues.len(),
            });
        }

        // A shared-memory region the host has placed somewhere else is not a
        // detail: the guest read the base out of these registers and handed it
        // to its driver, which has been mapping blobs into it ever since
        // (VEN-2001). A restored window at a different address would leave every
        // one of those mappings pointing at nothing.
        for &(id, base) in &state.shm_bases {
            match self.shm_base(id) {
                Some(current) if current == base => {}
                current => {
                    return Err(StateError::ShmBase {
                        id,
                        snapshot: base,
                        current: match current {
                            Some(at) => format!("{at:#x}"),
                            None => "nowhere".into(),
                        },
                    })
                }
            }
        }

        self.device_features_sel = state.device_features_sel;
        self.driver_features = state.driver_features;
        self.driver_features_sel = state.driver_features_sel;
        self.queue_sel = state.queue_sel;
        self.shm_sel = state.shm_sel;
        self.status = state.status;
        self.activated = false;

        for (config, saved) in self.queues.iter_mut().zip(&state.queues) {
            config.load_state(saved);
        }

        if state.status & status::FEATURES_OK != 0 {
            let negotiated = self.driver_features & self.device_features;
            if !self.device.ack_features(negotiated) {
                return Err(StateError::Device(format!(
                    "the device now vetoes the feature set {negotiated:#x} the guest negotiated"
                )));
            }
        }

        if state.activated {
            let positions: Vec<crate::save::QueuePosition> =
                state.queues.iter().map(|q| q.position).collect();
            self.activate_at(&positions).map_err(StateError::Activate)?;
        }

        self.device
            .load_device(&state.device)
            .map_err(|e| StateError::Device(e.to_string()))?;

        self.interrupt.load_interrupt(&state.interrupt)?;
        tracing::info!(
            transport = self.kind,
            slot = self.slot,
            device = ?self.device_type,
            status = format_args!("{:#x}", self.status),
            activated = self.activated,
            "virtio slot restored from a snapshot"
        );
        Ok(())
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
