//! The `virtio-pci` transport, modern (virtio 1.0+) only.
//!
//! VirtIO spec 1.2 section 4.1, "Virtio Over PCI Bus". This is the second
//! transport; [`crate::transport::MmioTransport`] was the first, and both are
//! register decoders over the same [`TransportState`], so nothing about feature
//! negotiation, the status state machine, queue validation or activation is
//! duplicated between them.
//!
//! It exists because a UEFI firmware needs it: EDK2's `CloudHvX64` build ships
//! `VirtioPciDeviceDxe` + `Virtio10Dxe` and **no** virtio-MMIO driver at all
//! (ADR-0003 gap map), so an ISO boot is impossible without PCI. Linux is the
//! second consumer, and the one this module is verified against.
//!
//! # Shape of a modern virtio PCI device
//!
//! * PCI vendor `0x1af4`, device `0x1040 + virtio device type`, revision 1.
//!   Linux's `vp_modern_probe` derives the virtio device id as
//!   `pci_dev->device - 0x1040` for ids ≥ `0x1040` and takes the virtio *vendor*
//!   id from the PCI subsystem vendor, which is why both are set.
//! * **No legacy interface.** There is no I/O BAR, no legacy register block and
//!   no transitional device id, so a legacy-only driver simply does not bind.
//!   `VIRTIO_F_VERSION_1` is mandatory (enforced by [`TransportState::new`]).
//! * Everything the driver touches lives in **one 32-bit memory BAR**
//!   ([`VIRTIO_PCI_BAR_INDEX`], [`VIRTIO_PCI_BAR_SIZE`]), split into four
//!   page-aligned structures found through the PCI capability list:
//!
//!   | Structure | `cfg_type` | BAR offset | Length | Notes |
//!   |---|---:|---:|---:|---|
//!   | common configuration | 1 | `0x0000` | `0x1000` | 60 bytes used |
//!   | ISR status | 3 | `0x1000` | `0x1000` | 1 byte, read-to-clear |
//!   | notification area | 2 | `0x2000` | `0x1000` | multiplier 4 → one dword per queue |
//!   | device configuration | 4 | `0x3000` | `0x1000` | passed to the device |
//!
//!   Page-aligned and page-sized so each region could later be given its own
//!   KVM memory slot or ioeventfd granularity without moving anything.
//!
//! # Interrupts: INTx, not MSI-X
//!
//! The device advertises **no MSI-X capability**, so the driver uses the legacy
//! INTx path: the ISR byte tells it *why* the line was raised and reading the
//! byte acknowledges it. [`LineInterrupt`](crate::LineInterrupt) already
//! implements exactly that word — bit 0 "a queue has used buffers", bit 1
//! "config space changed" — and the ordering it guarantees is the one INTx
//! needs: the bit is set *before* the line is raised, so a driver that takes the
//! interrupt and reads the ISR can never see 0.
//!
//! On this machine the line itself is an edge injected through a KVM irqfd on an
//! ISA-style IOAPIC pin (`machine_x86::pci`), not a real level-triggered PCI
//! `INTA#`: the ISR byte is therefore *not* what deasserts the line, and a
//! shared pin would lose interrupts. One device per pin keeps that honest; the
//! machine enforces it. MSI-X would remove the whole question and is the obvious
//! follow-up — but a half-built MSI-X capability is worse than none, so there is
//! none.
//!
//! # Untrusted guest
//!
//! Every access arrives as `(offset, width)` from the guest. Reads are served
//! from a snapshot of the register block, so any offset/width combination inside
//! it is safe and out-of-range reads are zeroes. Writes are matched against an
//! explicit `(offset, width)` table and anything else is logged and dropped —
//! a guest cannot reach a field by writing across it at an unexpected width.
//! Queue geometry is validated by [`crate::QueueConfig::build`] at activation,
//! exactly as on mmio.

use std::sync::Arc;

use crate::device::{DeviceType, VirtioDevice};
use crate::interrupt::IrqLine;
use crate::state::TransportState;
use crate::transport::TransportError;
use crate::GuestMem;

// ---------------------------------------------------------------- PCI identity

/// PCI vendor id for all virtio devices (Red Hat, Inc.).
pub const VIRTIO_PCI_VENDOR_ID: u16 = 0x1af4;

/// Modern virtio PCI device ids start here: `0x1040 + virtio device type`.
pub const VIRTIO_PCI_DEVICE_ID_BASE: u16 = 0x1040;

/// PCI revision id. Must be ≥ 1 for a non-transitional (modern-only) device;
/// revision 0 is what marks a legacy/transitional one.
pub const VIRTIO_PCI_REVISION: u8 = 1;

/// PCI subsystem vendor id. Linux reads the virtio *vendor* id from this field.
pub const VIRTIO_PCI_SUBSYSTEM_VENDOR_ID: u16 = 0x1af4;

/// Written into the PCI `interrupt_pin` register: INTA#. Nonzero is what tells a
/// driver the device has a legacy interrupt at all.
pub const VIRTIO_PCI_INTERRUPT_PIN: u8 = 1;

// ------------------------------------------------------------------ BAR layout

/// Index of the single memory BAR every structure lives in.
pub const VIRTIO_PCI_BAR_INDEX: u8 = 0;

/// Size of that BAR: four 4 KiB regions. A power of two, as the BAR sizing
/// protocol requires.
pub const VIRTIO_PCI_BAR_SIZE: u64 = 0x4000;

/// Offset and length of the common configuration structure inside the BAR.
pub const COMMON_CFG_OFFSET: u64 = 0x0000;
pub const COMMON_CFG_LEN: u64 = 0x1000;

/// Offset and length of the ISR status byte inside the BAR.
pub const ISR_CFG_OFFSET: u64 = 0x1000;
pub const ISR_CFG_LEN: u64 = 0x1000;

/// Offset and length of the notification area inside the BAR.
pub const NOTIFY_CFG_OFFSET: u64 = 0x2000;
pub const NOTIFY_CFG_LEN: u64 = 0x1000;

/// `notify_off_multiplier`: queue *n* is kicked by writing to
/// `NOTIFY_CFG_OFFSET + n * 4`.
///
/// A per-queue address (rather than a single shared one with the queue index in
/// the data) is what lets the host register one ioeventfd per queue **without a
/// datamatch**, so a kick of any width is completed inside the kernel.
pub const NOTIFY_OFF_MULTIPLIER: u32 = 4;

/// Offset and length of the device-specific configuration structure.
pub const DEVICE_CFG_OFFSET: u64 = 0x3000;
pub const DEVICE_CFG_LEN: u64 = 0x1000;

/// Most virtqueues a device may expose on this transport: the notification area
/// must hold one [`NOTIFY_OFF_MULTIPLIER`]-sized slot per queue.
pub const MAX_NOTIFY_QUEUES: usize = (NOTIFY_CFG_LEN / NOTIFY_OFF_MULTIPLIER as u64) as usize;

/// BAR offset of the notification slot for queue `index`.
pub const fn queue_notify_offset(index: u16) -> u64 {
    NOTIFY_CFG_OFFSET + index as u64 * NOTIFY_OFF_MULTIPLIER as u64
}

// ------------------------------------------------------------- capability list

/// PCI capability id for a vendor-specific capability, which is what every
/// virtio structure locator is.
pub const PCI_CAP_ID_VNDR: u8 = 0x09;

/// `cfg_type` values from the spec.
pub const VIRTIO_PCI_CAP_COMMON_CFG: u8 = 1;
pub const VIRTIO_PCI_CAP_NOTIFY_CFG: u8 = 2;
pub const VIRTIO_PCI_CAP_ISR_CFG: u8 = 3;
pub const VIRTIO_PCI_CAP_DEVICE_CFG: u8 = 4;

/// Length of a `struct virtio_pci_cap`.
pub const VIRTIO_PCI_CAP_LEN: u8 = 16;
/// Length of a `struct virtio_pci_notify_cap` (the above plus the multiplier).
pub const VIRTIO_PCI_NOTIFY_CAP_LEN: u8 = 20;

/// Encodes one `struct virtio_pci_cap` (spec 4.1.4).
///
/// `cap_next` is left at 0: the PCI layer owns the capability *list*, so it
/// patches the link when it places the record in config space. Everything else
/// — which BAR, which offset, how long, and the notify multiplier — is
/// transport knowledge and belongs here.
fn encode_cap(cfg_type: u8, offset: u64, length: u64, multiplier: Option<u32>) -> Vec<u8> {
    let len = match multiplier {
        Some(_) => VIRTIO_PCI_NOTIFY_CAP_LEN,
        None => VIRTIO_PCI_CAP_LEN,
    };
    let mut record = Vec::with_capacity(usize::from(len));
    record.push(PCI_CAP_ID_VNDR);
    record.push(0); // cap_next, patched by the PCI layer
    record.push(len);
    record.push(cfg_type);
    record.push(VIRTIO_PCI_BAR_INDEX);
    record.push(0); // id: only one structure of each type
    record.extend_from_slice(&[0, 0]); // padding
    record.extend_from_slice(&(offset as u32).to_le_bytes());
    record.extend_from_slice(&(length as u32).to_le_bytes());
    if let Some(multiplier) = multiplier {
        record.extend_from_slice(&multiplier.to_le_bytes());
    }
    record
}

/// The four capability records a modern virtio PCI device publishes, in the
/// order they are linked.
///
/// COMMON_CFG comes first deliberately: Linux's `vp_modern_probe` walks the list
/// looking for it and falls back to "leaving for legacy driver" when it is
/// absent, so it is the record that decides whether the modern driver binds.
pub fn capability_records() -> Vec<Vec<u8>> {
    vec![
        encode_cap(
            VIRTIO_PCI_CAP_COMMON_CFG,
            COMMON_CFG_OFFSET,
            COMMON_CFG_LEN,
            None,
        ),
        encode_cap(
            VIRTIO_PCI_CAP_NOTIFY_CFG,
            NOTIFY_CFG_OFFSET,
            NOTIFY_CFG_LEN,
            Some(NOTIFY_OFF_MULTIPLIER),
        ),
        encode_cap(VIRTIO_PCI_CAP_ISR_CFG, ISR_CFG_OFFSET, ISR_CFG_LEN, None),
        encode_cap(
            VIRTIO_PCI_CAP_DEVICE_CFG,
            DEVICE_CFG_OFFSET,
            DEVICE_CFG_LEN,
            None,
        ),
    ]
}

// ------------------------------------------------------ common configuration

/// Field offsets inside `struct virtio_pci_common_cfg` (spec 4.1.4.3).
pub mod common {
    pub const DEVICE_FEATURE_SELECT: u64 = 0x00;
    pub const DEVICE_FEATURE: u64 = 0x04;
    pub const DRIVER_FEATURE_SELECT: u64 = 0x08;
    pub const DRIVER_FEATURE: u64 = 0x0c;
    pub const CONFIG_MSIX_VECTOR: u64 = 0x10;
    pub const NUM_QUEUES: u64 = 0x12;
    pub const DEVICE_STATUS: u64 = 0x14;
    pub const CONFIG_GENERATION: u64 = 0x15;
    pub const QUEUE_SELECT: u64 = 0x16;
    pub const QUEUE_SIZE: u64 = 0x18;
    pub const QUEUE_MSIX_VECTOR: u64 = 0x1a;
    pub const QUEUE_ENABLE: u64 = 0x1c;
    pub const QUEUE_NOTIFY_OFF: u64 = 0x1e;
    pub const QUEUE_DESC: u64 = 0x20;
    pub const QUEUE_DRIVER: u64 = 0x28;
    pub const QUEUE_DEVICE: u64 = 0x30;
    pub const QUEUE_NOTIFY_DATA: u64 = 0x38;
    pub const QUEUE_RESET: u64 = 0x3a;

    /// Upper halves of the three 64-bit ring-address fields. The spec declares
    /// them as `le64`, but Linux's `vp_iowrite64_twopart` writes each half
    /// separately, so both widths must be accepted.
    pub const QUEUE_DESC_HIGH: u64 = QUEUE_DESC + 4;
    pub const QUEUE_DRIVER_HIGH: u64 = QUEUE_DRIVER + 4;
    pub const QUEUE_DEVICE_HIGH: u64 = QUEUE_DEVICE + 4;

    /// Size of the whole structure.
    pub const SIZE: usize = 0x3c;
}

/// "No MSI-X vector" — what both vector fields always read, because this device
/// publishes no MSI-X capability.
pub const VIRTIO_MSI_NO_VECTOR: u16 = 0xffff;

// ------------------------------------------------------------- the ISR status

/// ISR bit: one or more virtqueues have used buffers.
pub const ISR_QUEUE: u8 = 1 << 0;
/// ISR bit: the device configuration changed.
pub const ISR_CONFIG: u8 = 1 << 1;

/// PCI device id for a virtio device type: `0x1040 + type`.
///
/// A free function because the machine builds a device's configuration space
/// *before* its transport exists, and both must agree.
pub fn device_id(device_type: DeviceType) -> u16 {
    // Every `DeviceType` id is far below 0x1000, so this cannot overflow;
    // saturating rather than wrapping keeps that true if one ever grows.
    VIRTIO_PCI_DEVICE_ID_BASE.saturating_add(device_type.id() as u16)
}

/// PCI class code register value for a virtio device type: base class in bits
/// 31:24, sub class in 23:16, programming interface in 15:8. The revision
/// occupies bits 7:0 of the same dword and is added by the PCI layer.
///
/// Chosen so `lspci` names the device something recognisable; no driver binds on
/// it — Linux and EDK2 both match on vendor/device id.
pub fn class_code(device_type: DeviceType) -> u32 {
    let (base, sub): (u32, u32) = match device_type {
        DeviceType::Net => (0x02, 0x00),   // network / ethernet
        DeviceType::Block => (0x01, 0x80), // mass storage / other
        DeviceType::Gpu => (0x03, 0x80),   // display / other
        DeviceType::Input => (0x09, 0x80), // input device / other
    };
    (base << 24) | (sub << 16)
}

// ----------------------------------------------------------------- the device

/// One modern virtio PCI function: the BAR register file plus the device behind
/// it.
///
/// PCI *configuration space* is not here — that is the machine's PCI bus
/// ([`machine_x86::pci`](../../machine_x86/pci/index.html)), which asks this
/// type for its identity ([`Self::device_id`], [`Self::class_code`]) and for
/// [`capability_records`]. This type owns only what lives inside the BAR.
pub struct PciTransport {
    state: TransportState,
}

impl PciTransport {
    /// Wires `device` onto the PCI bus as device number `slot`.
    ///
    /// Fails when the device violates the transport contract (no
    /// `VIRTIO_F_VERSION_1`, no queues, a bad advertised queue size) or exposes
    /// more queues than the notification area can address. All of these are host
    /// bugs found at VM construction time, never guest input.
    pub fn new(
        slot: usize,
        device: Box<dyn VirtioDevice>,
        mem: Arc<GuestMem>,
        line: Arc<dyn IrqLine>,
    ) -> Result<Self, TransportError> {
        let state = TransportState::new("virtio-pci", slot, device, mem, line)?;
        if state.num_queues() > MAX_NOTIFY_QUEUES {
            return Err(TransportError::TooManyQueuesForNotify {
                device_type: state.device_type(),
                queues: state.num_queues(),
                max: MAX_NOTIFY_QUEUES,
            });
        }
        Ok(Self { state })
    }

    // ------------------------------------------------------- PCI identity

    /// PCI device id: `0x1040 + virtio device type`.
    pub fn device_id(&self) -> u16 {
        device_id(self.state.device_type())
    }

    /// PCI class code register value for this device; see [`class_code`].
    pub fn class_code(&self) -> u32 {
        class_code(self.state.device_type())
    }

    // ---------------------------------------------------------- accessors

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

    /// The pending ISR bits *without* clearing them — for diagnostics only. A
    /// guest read goes through [`Self::read_bar`], which clears.
    pub fn interrupt_status(&self) -> u32 {
        self.state.interrupt_status()
    }

    /// The device behind this function, for inspection (tests, `doctor`).
    pub fn device(&self) -> &dyn VirtioDevice {
        self.state.device()
    }

    pub fn num_queues(&self) -> usize {
        self.state.num_queues()
    }

    /// See [`crate::MmioTransport::offload_queue_notify`]; identical contract.
    pub fn offload_queue_notify(&mut self, index: u16) -> bool {
        self.state.offload_queue_notify(index)
    }

    pub fn restore_queue_notify(&mut self, index: u16) {
        self.state.restore_queue_notify(index);
    }

    pub fn is_queue_notify_offloaded(&self, index: u16) -> bool {
        self.state.is_queue_notify_offloaded(index)
    }

    /// Runs the device for queue `value`. The single entry point for kicks,
    /// whichever way they arrive (register write or ioeventfd worker).
    pub fn queue_notify(&mut self, value: u32) {
        self.state.queue_notify(value);
    }

    /// Full device reset, as if the driver had written 0 to `device_status`.
    pub fn reset(&mut self) {
        self.state.reset();
    }

    // --------------------------------------------------------- BAR access

    /// Guest read at `offset` inside the device's BAR.
    ///
    /// Any offset and width is safe: unclaimed bytes read as zero.
    pub fn read_bar(&mut self, offset: u64, data: &mut [u8]) {
        data.fill(0);
        if let Some(within) = region_offset(offset, COMMON_CFG_OFFSET, COMMON_CFG_LEN) {
            let snapshot = self.common_cfg_bytes();
            copy_from_snapshot(&snapshot, within, data);
            return;
        }
        if let Some(within) = region_offset(offset, ISR_CFG_OFFSET, ISR_CFG_LEN) {
            // Reading the ISR clears it (spec 4.1.4.5). Only the byte at offset
            // 0 exists; a read anywhere else in the region is not an
            // acknowledgement and must not clear anything — nor is a zero-width
            // read, which would otherwise acknowledge without reporting.
            if let (0, Some(first)) = (within, data.first_mut()) {
                let pending = self.state.interrupt().take_status();
                *first = (pending & u32::from(ISR_QUEUE | ISR_CONFIG)) as u8;
            }
            return;
        }
        if let Some(within) = region_offset(offset, DEVICE_CFG_OFFSET, DEVICE_CFG_LEN) {
            self.state.read_config(within, data);
            return;
        }
        // The notification area is write-only, and so is everything else in the
        // BAR that no capability points at.
        if region_offset(offset, NOTIFY_CFG_OFFSET, NOTIFY_CFG_LEN).is_none() {
            tracing::debug!(
                slot = self.state.slot(),
                device = ?self.state.device_type(),
                offset,
                "read from an unclaimed virtio-pci BAR offset"
            );
        }
    }

    /// Guest write at `offset` inside the device's BAR.
    pub fn write_bar(&mut self, offset: u64, data: &[u8]) {
        if let Some(within) = region_offset(offset, COMMON_CFG_OFFSET, COMMON_CFG_LEN) {
            self.write_common_cfg(within, data);
            return;
        }
        if let Some(within) = region_offset(offset, NOTIFY_CFG_OFFSET, NOTIFY_CFG_LEN) {
            self.write_notify(within, data);
            return;
        }
        if let Some(within) = region_offset(offset, DEVICE_CFG_OFFSET, DEVICE_CFG_LEN) {
            self.state.write_config(within, data);
            return;
        }
        // The ISR is read-to-clear; a write to it means nothing.
        tracing::debug!(
            slot = self.state.slot(),
            device = ?self.state.device_type(),
            offset,
            len = data.len(),
            "ignoring write to a read-only or unclaimed virtio-pci BAR offset"
        );
    }

    /// A kick: the queue is identified by *which* notification slot was written,
    /// because that is what the host's per-queue ioeventfd is registered on.
    ///
    /// The value the driver writes is the queue index too, and a well-behaved
    /// driver makes the two agree; when they do not, the address wins (a
    /// register write that reached userspace despite an ioeventfd would
    /// otherwise be able to kick a *different* queue than the one KVM would
    /// have).
    fn write_notify(&mut self, within: u64, data: &[u8]) {
        if within % u64::from(NOTIFY_OFF_MULTIPLIER) != 0 {
            tracing::warn!(
                slot = self.state.slot(),
                device = ?self.state.device_type(),
                within,
                "ignoring misaligned virtio-pci queue notification"
            );
            return;
        }
        let queue = within / u64::from(NOTIFY_OFF_MULTIPLIER);
        let written = le_value(data);
        if written != queue {
            tracing::debug!(
                slot = self.state.slot(),
                device = ?self.state.device_type(),
                queue,
                written,
                "queue notification value disagrees with its address; using the address"
            );
        }
        // Truncation is impossible for an in-range queue and harmless
        // otherwise: `queue_notify_from_register` rejects unknown indices.
        let value = u32::try_from(queue).unwrap_or(u32::MAX);
        self.state.queue_notify_from_register(value);
    }

    // ------------------------------------------------- common configuration

    /// The whole common-configuration structure as bytes, so a read of any
    /// offset and width can be served by slicing it.
    fn common_cfg_bytes(&self) -> [u8; common::SIZE] {
        let mut out = [0u8; common::SIZE];
        let mut put = |offset: u64, bytes: &[u8]| {
            let at = offset as usize;
            // Every offset below is a compile-time constant inside the struct;
            // the guard keeps that a fact rather than an assumption.
            if let Some(slot) = out.get_mut(at..at + bytes.len()) {
                slot.copy_from_slice(bytes);
            }
        };
        let queue = self.state.selected_queue();
        put(
            common::DEVICE_FEATURE_SELECT,
            &self.state.device_features_sel().to_le_bytes(),
        );
        put(
            common::DEVICE_FEATURE,
            &self.state.device_features_window().to_le_bytes(),
        );
        put(
            common::DRIVER_FEATURE_SELECT,
            &self.state.driver_features_sel().to_le_bytes(),
        );
        put(
            common::DRIVER_FEATURE,
            &self.state.driver_features_window().to_le_bytes(),
        );
        put(
            common::CONFIG_MSIX_VECTOR,
            &VIRTIO_MSI_NO_VECTOR.to_le_bytes(),
        );
        let num_queues = u16::try_from(self.state.num_queues()).unwrap_or(u16::MAX);
        put(common::NUM_QUEUES, &num_queues.to_le_bytes());
        // `status::KNOWN` is 8 bits wide, so the device status always fits.
        put(common::DEVICE_STATUS, &[self.state.status() as u8]);
        put(
            common::CONFIG_GENERATION,
            &[self.state.interrupt().generation() as u8],
        );
        let queue_sel = u16::try_from(self.state.queue_sel()).unwrap_or(u16::MAX);
        put(common::QUEUE_SELECT, &queue_sel.to_le_bytes());
        // A selector that names no queue reads back a size of 0, which is how a
        // driver detects "this queue does not exist" (Linux: `-ENOENT`).
        put(
            common::QUEUE_SIZE,
            &queue.map_or(0u16, |q| q.size()).to_le_bytes(),
        );
        put(
            common::QUEUE_MSIX_VECTOR,
            &VIRTIO_MSI_NO_VECTOR.to_le_bytes(),
        );
        put(
            common::QUEUE_ENABLE,
            &u16::from(queue.is_some_and(|q| q.is_ready())).to_le_bytes(),
        );
        // Queue *n* is kicked at `NOTIFY_CFG_OFFSET + n * NOTIFY_OFF_MULTIPLIER`,
        // i.e. the notify offset is simply the queue index.
        put(common::QUEUE_NOTIFY_OFF, &queue_sel.to_le_bytes());
        put(
            common::QUEUE_DESC,
            &queue.map_or(0u64, |q| q.desc_table()).to_le_bytes(),
        );
        put(
            common::QUEUE_DRIVER,
            &queue.map_or(0u64, |q| q.driver_area()).to_le_bytes(),
        );
        put(
            common::QUEUE_DEVICE,
            &queue.map_or(0u64, |q| q.device_area()).to_le_bytes(),
        );
        // queue_notify_data is only meaningful with VIRTIO_F_NOTIF_CONFIG_DATA
        // and queue_reset only with VIRTIO_F_RING_RESET; neither is offered, so
        // both stay 0.
        out
    }

    fn write_common_cfg(&mut self, offset: u64, data: &[u8]) {
        let value = le_value(data);
        // Access widths are matched exactly (with the documented exception of
        // the 64-bit ring addresses, which Linux writes as two halves): a guest
        // must not be able to reach a field by writing across it.
        match (offset, data.len()) {
            (common::DEVICE_FEATURE_SELECT, 4) => self.state.set_device_features_sel(value as u32),
            (common::DRIVER_FEATURE_SELECT, 4) => self.state.set_driver_features_sel(value as u32),
            (common::DRIVER_FEATURE, 4) => self.state.write_driver_features(value as u32),
            (common::DEVICE_STATUS, 1) => self.state.write_status(value as u32),
            (common::QUEUE_SELECT, 2) => self.state.set_queue_sel(value as u32),
            (common::QUEUE_SIZE, 2) => {
                let size = value as u16;
                self.state
                    .edit_selected_queue("queue_size", |q| q.set_size(size));
            }
            (common::QUEUE_ENABLE, 2) => {
                let ready = value == 1;
                self.state
                    .edit_selected_queue("queue_enable", |q| q.set_ready(ready));
            }
            (common::QUEUE_DESC, 8) | (common::QUEUE_DRIVER, 8) | (common::QUEUE_DEVICE, 8) => {
                self.write_queue_address(offset, value as u32, Some((value >> 32) as u32));
            }
            (common::QUEUE_DESC, 4) | (common::QUEUE_DRIVER, 4) | (common::QUEUE_DEVICE, 4) => {
                self.write_queue_address(offset, value as u32, None);
            }
            (common::QUEUE_DESC_HIGH, 4) => {
                self.write_queue_address_high(common::QUEUE_DESC, value as u32)
            }
            (common::QUEUE_DRIVER_HIGH, 4) => {
                self.write_queue_address_high(common::QUEUE_DRIVER, value as u32)
            }
            (common::QUEUE_DEVICE_HIGH, 4) => {
                self.write_queue_address_high(common::QUEUE_DEVICE, value as u32)
            }
            // Both vector fields are meaningless without an MSI-X capability,
            // and the spec forbids writing them in that case. They read back
            // VIRTIO_MSI_NO_VECTOR whatever happens here.
            (common::CONFIG_MSIX_VECTOR, 2) | (common::QUEUE_MSIX_VECTOR, 2) => {
                tracing::warn!(
                    slot = self.state.slot(),
                    device = ?self.state.device_type(),
                    offset,
                    value,
                    "ignoring MSI-X vector write: this device publishes no MSI-X capability"
                );
            }
            (common::QUEUE_RESET, 2) => tracing::warn!(
                slot = self.state.slot(),
                device = ?self.state.device_type(),
                value,
                "ignoring queue_reset write: VIRTIO_F_RING_RESET is not offered"
            ),
            _ => tracing::warn!(
                slot = self.state.slot(),
                device = ?self.state.device_type(),
                offset,
                len = data.len(),
                value,
                "ignoring write to a read-only virtio-pci common-config field, \
                 or at an unsupported width"
            ),
        }
    }

    fn write_queue_address(&mut self, field: u64, low: u32, high: Option<u32>) {
        self.state.edit_selected_queue("queue address", |queue| {
            match field {
                common::QUEUE_DESC => queue.set_desc_table_low(low),
                common::QUEUE_DRIVER => queue.set_driver_area_low(low),
                common::QUEUE_DEVICE => queue.set_device_area_low(low),
                // Unreachable: the caller only routes the three offsets above.
                _ => return,
            }
            if let Some(high) = high {
                match field {
                    common::QUEUE_DESC => queue.set_desc_table_high(high),
                    common::QUEUE_DRIVER => queue.set_driver_area_high(high),
                    common::QUEUE_DEVICE => queue.set_device_area_high(high),
                    _ => (),
                }
            }
        });
    }

    fn write_queue_address_high(&mut self, field: u64, high: u32) {
        self.state
            .edit_selected_queue("queue address", |queue| match field {
                common::QUEUE_DESC => queue.set_desc_table_high(high),
                common::QUEUE_DRIVER => queue.set_driver_area_high(high),
                common::QUEUE_DEVICE => queue.set_device_area_high(high),
                _ => (),
            });
    }
}

// ------------------------------------------------------------------- helpers

/// Offset of `addr` inside the region `[base, base + len)`, if it is in it.
fn region_offset(addr: u64, base: u64, len: u64) -> Option<u64> {
    let within = addr.checked_sub(base)?;
    (within < len).then_some(within)
}

/// A guest-written little-endian value of 1..=8 bytes. Longer writes keep only
/// their first 8 bytes, which no field cares about.
fn le_value(data: &[u8]) -> u64 {
    let mut bytes = [0u8; 8];
    let take = data.len().min(8);
    bytes[..take].copy_from_slice(&data[..take]);
    u64::from_le_bytes(bytes)
}

/// Copies the slice of `snapshot` starting at `within` into `data`, zero-filling
/// anything past the end of the snapshot.
fn copy_from_snapshot(snapshot: &[u8], within: u64, data: &mut [u8]) {
    for (i, byte) in data.iter_mut().enumerate() {
        let at = within.saturating_add(i as u64);
        *byte = usize::try_from(at)
            .ok()
            .and_then(|at| snapshot.get(at))
            .copied()
            .unwrap_or(0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::device::{DeviceError, DeviceResources};
    use crate::interrupt::Interrupt;
    use crate::testing::{self, SplitRing, TestIrqLine};
    use crate::{mmio, status, VIRTIO_F_VERSION_1};
    use std::sync::Mutex;

    const FEATURE_A: u64 = 1 << 3;

    type NotifyLog = Arc<Mutex<Vec<u16>>>;

    /// The same shape of stub the mmio transport tests use: it records what the
    /// transport did to it.
    struct TestDevice {
        queue_sizes: Vec<u16>,
        features: u64,
        config: Vec<u8>,
        notifies: NotifyLog,
        device_type: DeviceType,
    }

    impl Default for TestDevice {
        fn default() -> Self {
            Self {
                queue_sizes: vec![16],
                features: VIRTIO_F_VERSION_1 | FEATURE_A,
                config: vec![0xaa, 0xbb, 0xcc, 0xdd],
                notifies: NotifyLog::default(),
                device_type: DeviceType::Block,
            }
        }
    }

    impl VirtioDevice for TestDevice {
        fn device_type(&self) -> DeviceType {
            self.device_type
        }
        fn queue_max_sizes(&self) -> &[u16] {
            &self.queue_sizes
        }
        fn device_features(&self) -> u64 {
            self.features
        }
        fn ack_features(&mut self, _negotiated: u64) -> bool {
            true
        }
        fn read_config(&self, offset: u64, data: &mut [u8]) {
            copy_from_snapshot(&self.config, offset, data);
        }
        fn write_config(&mut self, offset: u64, data: &[u8]) {
            for (i, byte) in data.iter().enumerate() {
                let at = offset.saturating_add(i as u64);
                if let Some(slot) = usize::try_from(at)
                    .ok()
                    .and_then(|i| self.config.get_mut(i))
                {
                    *slot = *byte;
                }
            }
        }
        fn activate(&mut self, _resources: DeviceResources) -> Result<(), DeviceError> {
            Ok(())
        }
        fn notify(&mut self, queue_index: u16) -> Result<(), DeviceError> {
            if let Ok(mut log) = self.notifies.lock() {
                log.push(queue_index);
            }
            Ok(())
        }
        fn reset(&mut self) {}
    }

    fn transport_with(device: TestDevice) -> (PciTransport, Arc<TestIrqLine>, NotifyLog) {
        let log = Arc::clone(&device.notifies);
        let line = Arc::new(TestIrqLine::default());
        let mem = Arc::new(testing::guest_memory(0x2_0000));
        let transport = PciTransport::new(0, Box::new(device), mem, line.clone())
            .expect("test device satisfies the transport contract");
        (transport, line, log)
    }

    fn transport() -> (PciTransport, Arc<TestIrqLine>, NotifyLog) {
        transport_with(TestDevice::default())
    }

    fn read(t: &mut PciTransport, offset: u64, len: usize) -> u64 {
        let mut data = vec![0u8; len];
        t.read_bar(offset, &mut data);
        le_value(&data)
    }

    fn write(t: &mut PciTransport, offset: u64, len: usize, value: u64) {
        t.write_bar(offset, &value.to_le_bytes()[..len]);
    }

    fn notified(log: &NotifyLog) -> Vec<u16> {
        log.lock().map(|l| l.clone()).unwrap_or_default()
    }

    // --------------------------------------------------------- identity

    #[test]
    fn device_ids_follow_the_modern_numbering() {
        for (kind, expected) in [
            (DeviceType::Net, 0x1041u16),
            (DeviceType::Block, 0x1042),
            (DeviceType::Gpu, 0x1050),
            (DeviceType::Input, 0x1052),
        ] {
            let (t, _, _) = transport_with(TestDevice {
                device_type: kind,
                ..Default::default()
            });
            assert_eq!(t.device_id(), expected, "{kind:?}");
            // Linux only claims 0x1000..=0x107f.
            assert!((0x1000..=0x107f).contains(&t.device_id()));
        }
    }

    #[test]
    fn class_codes_are_plausible_and_never_zero() {
        for kind in [
            DeviceType::Net,
            DeviceType::Block,
            DeviceType::Gpu,
            DeviceType::Input,
        ] {
            let (t, _, _) = transport_with(TestDevice {
                device_type: kind,
                ..Default::default()
            });
            let class = t.class_code();
            assert_ne!(class >> 24, 0, "{kind:?} must have a base class");
            // The revision byte lives in the low 8 bits of the same dword and
            // is the PCI layer's business, so it must be clear here.
            assert_eq!(class & 0xff, 0);
        }
    }

    // ------------------------------------------------------ capabilities

    /// The capability list is how the driver finds anything at all: the offsets,
    /// lengths, BAR index and the notify multiplier must be exactly what this
    /// transport then decodes.
    #[test]
    fn capability_records_describe_the_real_bar_layout() {
        let records = capability_records();
        assert_eq!(records.len(), 4);
        let mut seen = Vec::new();
        for record in &records {
            assert_eq!(record[0], PCI_CAP_ID_VNDR);
            assert_eq!(record[1], 0, "cap_next is the PCI layer's to patch");
            assert_eq!(usize::from(record[2]), record.len(), "cap_len");
            assert_eq!(record[4], VIRTIO_PCI_BAR_INDEX);
            let offset = u32::from_le_bytes([record[8], record[9], record[10], record[11]]);
            let length = u32::from_le_bytes([record[12], record[13], record[14], record[15]]);
            // Every structure must fit inside the BAR it claims to live in.
            assert!(u64::from(offset) + u64::from(length) <= VIRTIO_PCI_BAR_SIZE);
            seen.push((record[3], u64::from(offset), u64::from(length)));
        }
        assert_eq!(
            seen,
            vec![
                (VIRTIO_PCI_CAP_COMMON_CFG, COMMON_CFG_OFFSET, COMMON_CFG_LEN),
                (VIRTIO_PCI_CAP_NOTIFY_CFG, NOTIFY_CFG_OFFSET, NOTIFY_CFG_LEN),
                (VIRTIO_PCI_CAP_ISR_CFG, ISR_CFG_OFFSET, ISR_CFG_LEN),
                (VIRTIO_PCI_CAP_DEVICE_CFG, DEVICE_CFG_OFFSET, DEVICE_CFG_LEN),
            ]
        );
        // The notify capability carries the multiplier as a fifth dword.
        let notify = &records[1];
        assert_eq!(notify.len(), usize::from(VIRTIO_PCI_NOTIFY_CAP_LEN));
        assert_eq!(
            u32::from_le_bytes([notify[16], notify[17], notify[18], notify[19]]),
            NOTIFY_OFF_MULTIPLIER
        );
    }

    #[test]
    fn regions_do_not_overlap_and_the_common_struct_fits() {
        let regions = [
            (COMMON_CFG_OFFSET, COMMON_CFG_LEN),
            (ISR_CFG_OFFSET, ISR_CFG_LEN),
            (NOTIFY_CFG_OFFSET, NOTIFY_CFG_LEN),
            (DEVICE_CFG_OFFSET, DEVICE_CFG_LEN),
        ];
        for (i, (base, len)) in regions.iter().enumerate() {
            for (other_base, other_len) in regions.iter().skip(i + 1) {
                assert!(
                    base + len <= *other_base || other_base + other_len <= *base,
                    "regions overlap"
                );
            }
        }
        assert!(common::SIZE as u64 <= COMMON_CFG_LEN);
        assert!(VIRTIO_PCI_BAR_SIZE.is_power_of_two(), "BAR sizing protocol");
        assert_eq!(queue_notify_offset(0), NOTIFY_CFG_OFFSET);
        assert_eq!(queue_notify_offset(3), NOTIFY_CFG_OFFSET + 12);
    }

    /// The ISR byte and the mmio `INTERRUPT_STATUS` word must agree bit for bit,
    /// because one [`crate::LineInterrupt`] serves both transports.
    #[test]
    fn isr_bits_match_the_mmio_interrupt_word() {
        assert_eq!(u32::from(ISR_QUEUE), mmio::INT_VRING);
        assert_eq!(u32::from(ISR_CONFIG), mmio::INT_CONFIG);
    }

    // --------------------------------------------------- common config

    #[test]
    fn read_only_identity_fields() {
        let (mut t, _, _) = transport();
        assert_eq!(read(&mut t, common::NUM_QUEUES, 2), 1);
        assert_eq!(
            read(&mut t, common::CONFIG_MSIX_VECTOR, 2),
            u64::from(VIRTIO_MSI_NO_VECTOR)
        );
        assert_eq!(
            read(&mut t, common::QUEUE_MSIX_VECTOR, 2),
            u64::from(VIRTIO_MSI_NO_VECTOR)
        );
        assert_eq!(read(&mut t, common::DEVICE_STATUS, 1), 0);
        assert_eq!(read(&mut t, common::QUEUE_NOTIFY_DATA, 2), 0);
        assert_eq!(read(&mut t, common::QUEUE_RESET, 2), 0);
    }

    #[test]
    fn feature_windows_are_selected_and_read_back() {
        let (mut t, _, _) = transport();
        let offered = VIRTIO_F_VERSION_1 | FEATURE_A;

        write(&mut t, common::DEVICE_FEATURE_SELECT, 4, 0);
        assert_eq!(
            read(&mut t, common::DEVICE_FEATURE, 4),
            offered & 0xffff_ffff
        );
        write(&mut t, common::DEVICE_FEATURE_SELECT, 4, 1);
        assert_eq!(read(&mut t, common::DEVICE_FEATURE, 4), offered >> 32);
        // Windows beyond 1 are undefined and read as zero.
        write(&mut t, common::DEVICE_FEATURE_SELECT, 4, 7);
        assert_eq!(read(&mut t, common::DEVICE_FEATURE, 4), 0);

        // The driver's own word reads back per window, which is what makes the
        // register observable.
        write(&mut t, common::DRIVER_FEATURE_SELECT, 4, 1);
        write(&mut t, common::DRIVER_FEATURE, 4, VIRTIO_F_VERSION_1 >> 32);
        assert_eq!(
            read(&mut t, common::DRIVER_FEATURE, 4),
            VIRTIO_F_VERSION_1 >> 32
        );
        write(&mut t, common::DRIVER_FEATURE_SELECT, 4, 0);
        assert_eq!(read(&mut t, common::DRIVER_FEATURE, 4), 0);
    }

    /// Sub-dword and unaligned *reads* are legal and must slice the register
    /// block rather than returning garbage.
    #[test]
    fn reads_of_any_width_slice_the_register_block() {
        let (mut t, _, _) = transport();
        write(&mut t, common::QUEUE_SELECT, 2, 0);
        write(&mut t, common::QUEUE_SIZE, 2, 0x10);
        // queue_size is at 0x18: read its two bytes separately, then the dword
        // that also covers queue_msix_vector.
        assert_eq!(read(&mut t, 0x18, 1), 0x10);
        assert_eq!(read(&mut t, 0x19, 1), 0x00);
        assert_eq!(read(&mut t, 0x18, 4), 0xffff_0010);
        // A read straddling the end of the structure zero-fills.
        assert_eq!(read(&mut t, 0x3a, 4), 0);
    }

    #[test]
    fn queue_registers_round_trip_per_selected_queue() {
        let (mut t, _, _) = transport_with(TestDevice {
            queue_sizes: vec![16, 64],
            ..Default::default()
        });
        assert_eq!(read(&mut t, common::NUM_QUEUES, 2), 2);

        // On reset queue_size reports the device's maximum.
        write(&mut t, common::QUEUE_SELECT, 2, 0);
        assert_eq!(read(&mut t, common::QUEUE_SIZE, 2), 16);
        assert_eq!(read(&mut t, common::QUEUE_NOTIFY_OFF, 2), 0);
        write(&mut t, common::QUEUE_SELECT, 2, 1);
        assert_eq!(read(&mut t, common::QUEUE_SIZE, 2), 64);
        assert_eq!(read(&mut t, common::QUEUE_NOTIFY_OFF, 2), 1);

        write(&mut t, common::QUEUE_SIZE, 2, 32);
        write(&mut t, common::QUEUE_DESC, 8, 0x1234_5678_9abc_d000);
        write(&mut t, common::QUEUE_DRIVER, 4, 0x2000);
        write(&mut t, common::QUEUE_DRIVER + 4, 4, 1);
        write(&mut t, common::QUEUE_DEVICE, 8, 0x3000);
        write(&mut t, common::QUEUE_ENABLE, 2, 1);

        assert_eq!(read(&mut t, common::QUEUE_SIZE, 2), 32);
        assert_eq!(read(&mut t, common::QUEUE_DESC, 8), 0x1234_5678_9abc_d000);
        assert_eq!(read(&mut t, common::QUEUE_DRIVER, 8), 0x1_0000_2000);
        assert_eq!(read(&mut t, common::QUEUE_DEVICE, 8), 0x3000);
        assert_eq!(read(&mut t, common::QUEUE_ENABLE, 2), 1);

        // Queue 0 was untouched by all of that.
        write(&mut t, common::QUEUE_SELECT, 2, 0);
        assert_eq!(read(&mut t, common::QUEUE_SIZE, 2), 16);
        assert_eq!(read(&mut t, common::QUEUE_DESC, 8), 0);
        assert_eq!(read(&mut t, common::QUEUE_ENABLE, 2), 0);
    }

    /// A queue_select the device cannot satisfy must read back size 0 — that is
    /// how a driver learns the queue does not exist — and swallow every write.
    #[test]
    fn out_of_range_queue_select_reads_zero_and_swallows_writes() {
        let (mut t, _, _) = transport();
        write(&mut t, common::QUEUE_SELECT, 2, 99);
        assert_eq!(read(&mut t, common::QUEUE_SIZE, 2), 0);
        assert_eq!(read(&mut t, common::QUEUE_ENABLE, 2), 0);
        assert_eq!(read(&mut t, common::QUEUE_DESC, 8), 0);
        write(&mut t, common::QUEUE_SIZE, 2, 8);
        write(&mut t, common::QUEUE_DESC, 8, 0xdead_beef);
        write(&mut t, common::QUEUE_ENABLE, 2, 1);

        write(&mut t, common::QUEUE_SELECT, 2, 0);
        assert_eq!(read(&mut t, common::QUEUE_SIZE, 2), 16);
        assert_eq!(read(&mut t, common::QUEUE_DESC, 8), 0);
        assert_eq!(read(&mut t, common::QUEUE_ENABLE, 2), 0);
    }

    #[test]
    fn writes_at_the_wrong_width_or_to_read_only_fields_are_dropped() {
        let (mut t, _, _) = transport();
        // Read-only fields.
        write(&mut t, common::DEVICE_FEATURE, 4, 0xffff_ffff);
        write(&mut t, common::NUM_QUEUES, 2, 7);
        write(&mut t, common::CONFIG_GENERATION, 1, 9);
        write(&mut t, common::QUEUE_NOTIFY_OFF, 2, 5);
        assert_eq!(read(&mut t, common::NUM_QUEUES, 2), 1);
        assert_eq!(read(&mut t, common::CONFIG_GENERATION, 1), 0);
        assert_eq!(read(&mut t, common::QUEUE_NOTIFY_OFF, 2), 0);

        // A 1-byte write into a 2-byte field, and a 2-byte write into a 4-byte
        // one, must not partially update anything.
        write(&mut t, common::QUEUE_SELECT, 1, 1);
        assert_eq!(read(&mut t, common::QUEUE_SELECT, 2), 0);
        write(&mut t, common::DEVICE_FEATURE_SELECT, 2, 1);
        assert_eq!(read(&mut t, common::DEVICE_FEATURE_SELECT, 4), 0);
        // An unaligned write inside the structure is not a field at all.
        write(
            &mut t,
            common::DEVICE_STATUS + 1,
            1,
            status::ACKNOWLEDGE.into(),
        );
        assert_eq!(read(&mut t, common::DEVICE_STATUS, 1), 0);
        // MSI-X vectors always read back "no vector", whatever is written.
        write(&mut t, common::QUEUE_MSIX_VECTOR, 2, 0);
        assert_eq!(
            read(&mut t, common::QUEUE_MSIX_VECTOR, 2),
            u64::from(VIRTIO_MSI_NO_VECTOR)
        );
        // queue_reset is inert without VIRTIO_F_RING_RESET.
        write(&mut t, common::QUEUE_RESET, 2, 1);
        assert_eq!(read(&mut t, common::QUEUE_RESET, 2), 0);
    }

    // ------------------------------------------------------- bring-up

    fn bring_up(t: &mut PciTransport, ring: &SplitRing) {
        write(t, common::DEVICE_STATUS, 1, status::ACKNOWLEDGE.into());
        write(
            t,
            common::DEVICE_STATUS,
            1,
            (status::ACKNOWLEDGE | status::DRIVER).into(),
        );
        write(t, common::DRIVER_FEATURE_SELECT, 4, 0);
        write(t, common::DRIVER_FEATURE, 4, FEATURE_A);
        write(t, common::DRIVER_FEATURE_SELECT, 4, 1);
        write(t, common::DRIVER_FEATURE, 4, VIRTIO_F_VERSION_1 >> 32);
        write(
            t,
            common::DEVICE_STATUS,
            1,
            (status::ACKNOWLEDGE | status::DRIVER | status::FEATURES_OK).into(),
        );
        write(t, common::QUEUE_SELECT, 2, 0);
        write(t, common::QUEUE_SIZE, 2, u64::from(ring.size()));
        write(t, common::QUEUE_DESC, 8, ring.desc_table());
        write(t, common::QUEUE_DRIVER, 8, ring.driver_area());
        write(t, common::QUEUE_DEVICE, 8, ring.device_area());
        write(t, common::QUEUE_ENABLE, 2, 1);
        write(
            t,
            common::DEVICE_STATUS,
            1,
            (status::ACKNOWLEDGE | status::DRIVER | status::FEATURES_OK | status::DRIVER_OK).into(),
        );
    }

    #[test]
    fn full_bring_up_activates_the_device() {
        let (mut t, _, _) = transport();
        let ring = SplitRing::layout(0x1000, 16);
        bring_up(&mut t, &ring);
        assert!(t.is_activated());
        assert_eq!(
            read(&mut t, common::DEVICE_STATUS, 1),
            u64::from(
                status::ACKNOWLEDGE | status::DRIVER | status::FEATURES_OK | status::DRIVER_OK
            )
        );
    }

    #[test]
    fn a_status_write_of_zero_resets_everything() {
        let (mut t, _, _) = transport();
        let ring = SplitRing::layout(0x1000, 16);
        bring_up(&mut t, &ring);
        write(&mut t, common::DEVICE_STATUS, 1, 0);

        assert_eq!(t.status(), 0);
        assert!(!t.is_activated());
        write(&mut t, common::QUEUE_SELECT, 2, 0);
        assert_eq!(read(&mut t, common::QUEUE_ENABLE, 2), 0);
        assert_eq!(read(&mut t, common::QUEUE_DESC, 8), 0);
        assert_eq!(
            read(&mut t, common::QUEUE_SIZE, 2),
            16,
            "back to the maximum"
        );
        // And the whole bring-up works again from scratch.
        bring_up(&mut t, &ring);
        assert!(t.is_activated());
    }

    #[test]
    fn illegal_status_transitions_and_reserved_bits_are_refused() {
        let (mut t, _, _) = transport();
        write(&mut t, common::DEVICE_STATUS, 1, status::DRIVER_OK.into());
        assert_eq!(t.status(), 0);
        write(&mut t, common::DEVICE_STATUS, 1, status::ACKNOWLEDGE.into());
        // Reserved bit 5 (0x20) is not in `status::KNOWN`.
        write(
            &mut t,
            common::DEVICE_STATUS,
            1,
            u64::from(status::ACKNOWLEDGE | status::DRIVER) | 0x20,
        );
        assert_eq!(t.status(), status::ACKNOWLEDGE | status::DRIVER);
        assert_eq!(t.status() & !status::KNOWN, 0);
    }

    #[test]
    fn activation_with_garbage_queue_addresses_needs_reset() {
        let (mut t, _, _) = transport();
        // Guest memory is 0x2_0000 bytes; put the used ring far beyond it.
        let ring = SplitRing::layout(0x1000, 16);
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
        write(&mut t, common::QUEUE_DESC, 8, ring.desc_table());
        write(&mut t, common::QUEUE_DRIVER, 8, ring.driver_area());
        write(&mut t, common::QUEUE_DEVICE, 8, 0xffff_ffff_0000);
        write(&mut t, common::QUEUE_ENABLE, 2, 1);
        write(
            &mut t,
            common::DEVICE_STATUS,
            1,
            (status::ACKNOWLEDGE | status::DRIVER | status::FEATURES_OK | status::DRIVER_OK).into(),
        );
        assert!(!t.is_activated());
        assert_ne!(t.status() & status::DEVICE_NEEDS_RESET, 0);
    }

    // -------------------------------------------------------- notify

    #[test]
    fn a_notification_runs_the_queue_its_address_names() {
        let (mut t, _, log) = transport();
        let ring = SplitRing::layout(0x1000, 16);
        bring_up(&mut t, &ring);
        assert!(t.is_activated());

        // Linux writes the queue index as a 16-bit value to the queue's own slot.
        t.write_bar(queue_notify_offset(0), &0u16.to_le_bytes());
        assert_eq!(notified(&log), vec![0]);
        // A 32-bit kick works too, as does a single byte: the address is what
        // identifies the queue, so KVM can match on it without a datamatch.
        write(&mut t, queue_notify_offset(0), 4, 0);
        write(&mut t, queue_notify_offset(0), 1, 0);
        assert_eq!(notified(&log), vec![0, 0, 0]);
    }

    #[test]
    fn notifications_for_queues_the_device_lacks_are_dropped() {
        let (mut t, _, log) = transport();
        let ring = SplitRing::layout(0x1000, 16);
        bring_up(&mut t, &ring);
        // The device has one queue; every other slot in the region is inert.
        for queue in [1u16, 2, 17, 1023] {
            write(&mut t, queue_notify_offset(queue), 2, u64::from(queue));
        }
        // …as is a misaligned write inside the notify region.
        write(&mut t, NOTIFY_CFG_OFFSET + 1, 2, 0);
        write(&mut t, NOTIFY_CFG_OFFSET + 3, 4, 0);
        assert_eq!(notified(&log), Vec::<u16>::new());
        assert_eq!(t.status() & status::DEVICE_NEEDS_RESET, 0);
    }

    #[test]
    fn a_notification_before_driver_ok_is_dropped() {
        let (mut t, _, log) = transport();
        write(&mut t, queue_notify_offset(0), 2, 0);
        assert_eq!(notified(&log), Vec::<u16>::new());
    }

    /// The value written names a queue other than the slot's: the address wins,
    /// because that is what the host's ioeventfd is keyed on.
    #[test]
    fn the_notify_address_wins_over_a_disagreeing_value() {
        let (mut t, _, log) = transport_with(TestDevice {
            queue_sizes: vec![16, 16],
            ..Default::default()
        });
        let ring = SplitRing::layout(0x1000, 16);
        // Bring both queues up: queue 1 gets its own ring further along memory.
        let ring1 = SplitRing::layout(0x4000, 16);
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
        for (index, r) in [(0u64, &ring), (1, &ring1)] {
            write(&mut t, common::QUEUE_SELECT, 2, index);
            write(&mut t, common::QUEUE_DESC, 8, r.desc_table());
            write(&mut t, common::QUEUE_DRIVER, 8, r.driver_area());
            write(&mut t, common::QUEUE_DEVICE, 8, r.device_area());
            write(&mut t, common::QUEUE_ENABLE, 2, 1);
        }
        write(
            &mut t,
            common::DEVICE_STATUS,
            1,
            (status::ACKNOWLEDGE | status::DRIVER | status::FEATURES_OK | status::DRIVER_OK).into(),
        );
        assert!(t.is_activated());

        // Slot for queue 1, value claiming queue 0.
        write(&mut t, queue_notify_offset(1), 2, 0);
        assert_eq!(notified(&log), vec![1]);
    }

    #[test]
    fn offloaded_queues_ignore_register_notifications() {
        let (mut t, _, log) = transport();
        let ring = SplitRing::layout(0x1000, 16);
        bring_up(&mut t, &ring);
        assert!(t.offload_queue_notify(0));
        assert!(t.is_queue_notify_offloaded(0));

        write(&mut t, queue_notify_offset(0), 2, 0);
        assert_eq!(notified(&log), Vec::<u16>::new());
        // The worker's direct call still runs the device.
        t.queue_notify(0);
        assert_eq!(notified(&log), vec![0]);
        // Handing it back restores the register path.
        t.restore_queue_notify(0);
        write(&mut t, queue_notify_offset(0), 2, 0);
        assert_eq!(notified(&log), vec![0, 0]);
    }

    #[test]
    fn a_device_with_more_queues_than_notify_slots_is_refused() {
        // Not reachable with today's devices, but the bound must be enforced
        // rather than assumed: a queue with no notify slot could never be kicked.
        let device = TestDevice {
            queue_sizes: vec![16; MAX_NOTIFY_QUEUES + 1],
            ..Default::default()
        };
        let line = Arc::new(TestIrqLine::default());
        let mem = Arc::new(testing::guest_memory(0x1000));
        assert!(matches!(
            PciTransport::new(0, Box::new(device), mem, line),
            Err(TransportError::TooManyQueuesForNotify { .. })
        ));
    }

    // ----------------------------------------------------------- ISR

    #[test]
    fn the_isr_is_read_to_clear() {
        let (mut t, line, _) = transport();
        assert_eq!(read(&mut t, ISR_CFG_OFFSET, 1), 0);

        // A device-side used-buffer signal sets bit 0 and raises the line.
        assert!(t.state.interrupt().signal_used_queue(0).is_ok());
        assert_eq!(line.count(), 1);
        assert_eq!(t.interrupt_status(), u32::from(ISR_QUEUE));
        // The bit is set *before* the line is raised, so a driver that reads
        // the ISR in its handler always sees why it was interrupted.
        assert_eq!(read(&mut t, ISR_CFG_OFFSET, 1), u64::from(ISR_QUEUE));
        // …and the read cleared it.
        assert_eq!(read(&mut t, ISR_CFG_OFFSET, 1), 0);
        assert_eq!(t.interrupt_status(), 0);

        // Config-change sets bit 1 and bumps the generation.
        assert!(t.state.interrupt().signal_config_change().is_ok());
        assert_eq!(read(&mut t, common::CONFIG_GENERATION, 1), 1);
        assert_eq!(read(&mut t, ISR_CFG_OFFSET, 1), u64::from(ISR_CONFIG));
        assert_eq!(read(&mut t, ISR_CFG_OFFSET, 1), 0);
    }

    /// Only the byte at offset 0 is the ISR; a read elsewhere in the region must
    /// not acknowledge an interrupt the driver has not looked at.
    #[test]
    fn reads_elsewhere_in_the_isr_region_do_not_acknowledge() {
        let (mut t, _, _) = transport();
        assert!(t.state.interrupt().signal_used_queue(0).is_ok());
        assert_eq!(read(&mut t, ISR_CFG_OFFSET + 1, 1), 0);
        assert_eq!(read(&mut t, ISR_CFG_OFFSET + 0x800, 4), 0);
        assert_eq!(t.interrupt_status(), u32::from(ISR_QUEUE));
        // Writes to the ISR are meaningless and must not clear it either.
        write(&mut t, ISR_CFG_OFFSET, 1, 0xff);
        assert_eq!(t.interrupt_status(), u32::from(ISR_QUEUE));
    }

    // -------------------------------------------------- device config

    #[test]
    fn the_device_config_region_is_delegated_to_the_device() {
        let (mut t, _, _) = transport();
        let mut data = [0u8; 4];
        t.read_bar(DEVICE_CFG_OFFSET, &mut data);
        assert_eq!(data, [0xaa, 0xbb, 0xcc, 0xdd]);

        t.write_bar(DEVICE_CFG_OFFSET + 1, &[0x11]);
        t.read_bar(DEVICE_CFG_OFFSET, &mut data);
        assert_eq!(data, [0xaa, 0x11, 0xcc, 0xdd]);

        // Reads past the end of the device's config space are zeroes.
        let mut far = [0xffu8; 8];
        t.read_bar(DEVICE_CFG_OFFSET + 0x800, &mut far);
        assert_eq!(far, [0u8; 8]);
        // As are writes: nothing panics, nothing is corrupted.
        t.write_bar(DEVICE_CFG_OFFSET + 0x800, &[1, 2, 3, 4, 5, 6, 7, 8]);
        t.read_bar(DEVICE_CFG_OFFSET, &mut data);
        assert_eq!(data, [0xaa, 0x11, 0xcc, 0xdd]);
    }

    // ------------------------------------------------ malicious guest

    /// Accesses outside every capability's region, and ones that run off the end
    /// of the BAR, must be inert.
    #[test]
    fn accesses_outside_the_described_regions_are_inert() {
        let (mut t, _, _) = transport();
        for offset in [
            common::SIZE as u64 + 4, // inside the common page, past the struct
            0x0fff,
            ISR_CFG_OFFSET - 1,
            VIRTIO_PCI_BAR_SIZE,
            VIRTIO_PCI_BAR_SIZE + 0x1000,
            u64::MAX - 8,
        ] {
            let mut data = [0xffu8; 8];
            t.read_bar(offset, &mut data);
            assert_eq!(data, [0u8; 8], "read at {offset:#x} must be zero");
            t.write_bar(offset, &[0xff; 8]);
        }
        // Nothing above disturbed the state machine.
        assert_eq!(t.status(), 0);
        assert!(!t.is_activated());
    }

    /// A read whose width runs off the end of a region must not pull bytes out
    /// of the next one.
    #[test]
    fn a_read_straddling_two_regions_does_not_leak_the_second() {
        let (mut t, _, _) = transport();
        assert!(t.state.interrupt().signal_used_queue(0).is_ok());
        // Last byte of the common page plus the first byte of the ISR page.
        let mut data = [0xffu8; 2];
        t.read_bar(COMMON_CFG_OFFSET + COMMON_CFG_LEN - 1, &mut data);
        assert_eq!(data, [0, 0], "the ISR must not appear here");
        // …and the ISR is still pending, i.e. that read did not acknowledge it.
        assert_eq!(t.interrupt_status(), u32::from(ISR_QUEUE));
    }
}
