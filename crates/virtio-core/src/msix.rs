//! MSI-X for the virtio-pci transport (backlog EPIC 19, MSI-X follow-up).
//!
//! Everything about message-signalled interrupts that is *state* rather than
//! address decoding: the capability record, the table, the pending-bit array, the
//! per-queue and config vector assignments, and the interrupt object that turns a
//! device's "queue 0 has used buffers" into one MSI message. [`crate::pci`] stays
//! what the architecture contract says a transport module is — an address
//! decoder — and calls in here.
//!
//! # Why MSI-X at all
//!
//! INTx works on this machine, and it cost three compromises to make it work
//! (all documented in [`crate::pci`] and `machine_x86::virtio_pci`):
//!
//! * the injection is an **edge** on an ISA-style IOAPIC pin, not a
//!   level-triggered `INTA#`, so pins can never be shared — one device per pin,
//!   bounded by `machine_x86::layout::VIRTIO_IRQS`;
//! * `interrupt_line` had to be made **read-only** because EDK2's `PciBusDxe`
//!   scribbles `0xff` over it and no platform driver here can put the real value
//!   back;
//! * the guest logs `can't find IRQ for PCI INT A; probably buggy MP table`,
//!   because the machine publishes ISA interrupt sources and no `_PRT`.
//!
//! MSI-X removes all three: the message carries its own destination and vector,
//! so there is no pin to share, nothing to route, and nothing for a firmware to
//! clobber. A device is simply told where to write.
//!
//! # Layout
//!
//! One capability record in configuration space (id `0x11`, 12 bytes) plus two
//! page-aligned regions in the device's single memory BAR:
//!
//! | Structure | BAR offset | Length | Contents |
//! |---|---:|---:|---|
//! | MSI-X table | [`crate::pci::MSIX_TABLE_OFFSET`] | 4 KiB | [`MSIX_ENTRY_SIZE`]-byte entries, [`MAX_MSIX_VECTORS`] of them |
//! | MSI-X PBA | [`crate::pci::MSIX_PBA_OFFSET`] | 4 KiB | one pending bit per vector |
//!
//! Table and PBA get a page each, which the PCI 3.0 spec recommends (§6.8.2: a
//! system that maps the table into a driver's address space must be able to do so
//! without exposing the PBA) and which matches the existing convention that every
//! region in this BAR is page-aligned and page-sized.
//!
//! # Table size
//!
//! `queues + 1`: one vector per virtqueue plus one for configuration changes.
//! That is what Linux's `vp_find_vqs_msix` asks for in its "one vector per queue"
//! mode, and asking for fewer would make it fall back to the shared-vector mode
//! (or to INTx).
//!
//! # Untrusted guest
//!
//! The whole table is guest-written, and that is *by design*: an MSI message is
//! how the driver tells the device where its own interrupts should go. Two
//! consequences the code depends on:
//!
//! * a message address is **never** a host address. It is handed to the host
//!   interrupt controller, which decodes it against the guest's local APICs the
//!   same way it decodes a write from a real device. A garbage address makes the
//!   delivery fail (logged), it cannot reach host memory.
//! * every index a guest supplies — a table offset, a `queue_msix_vector`, a
//!   `config_msix_vector` — is bounds-checked here. An out-of-range vector is
//!   stored as [`crate::pci::VIRTIO_MSI_NO_VECTOR`], which is precisely the
//!   spec's mechanism for "the device could not accept this" (virtio 1.2
//!   §4.1.4.3), so a driver reading the register back learns it failed.

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use crate::interrupt::{
    Interrupt, InterruptError, IrqLine, LineInterrupt, MsiMessage, MsiSink, TransportInterrupt,
};
use crate::pci::{
    MSIX_PBA_LEN, MSIX_PBA_OFFSET, MSIX_TABLE_LEN, MSIX_TABLE_OFFSET, VIRTIO_MSI_NO_VECTOR,
    VIRTIO_PCI_BAR_INDEX,
};

// ------------------------------------------------------------- the capability

/// PCI capability id for MSI-X.
pub const PCI_CAP_ID_MSIX: u8 = 0x11;

/// Length of a `struct msix_cap`: id, next, message control, table offset/BIR,
/// PBA offset/BIR.
pub const MSIX_CAP_LEN: u8 = 12;

/// Byte offset of the message-control halfword inside the capability record.
pub const MSIX_CONTROL_OFFSET: u8 = 2;

/// Message control bits 10:0: `table size - 1`. Read-only.
pub const MSIX_CTRL_TABLE_SIZE_MASK: u16 = 0x07ff;

/// Message control bit 14, "function mask": while set, no vector of this
/// function may generate an interrupt; requests are recorded in the PBA instead.
pub const MSIX_CTRL_FUNCTION_MASK: u16 = 1 << 14;

/// Message control bit 15, "MSI-X enable". While clear the function uses INTx.
pub const MSIX_CTRL_ENABLE: u16 = 1 << 15;

/// Bits of the capability's **first dword** a guest may write.
///
/// The message control halfword sits at byte 2 of the record, i.e. in the upper
/// half of the dword, and only its top two bits are writable — the table size
/// below them is read-only, which is what stops a guest from claiming a larger
/// table than the one the host allocated.
pub const MSIX_CONTROL_WRITE_MASK: u32 =
    ((MSIX_CTRL_ENABLE | MSIX_CTRL_FUNCTION_MASK) as u32) << (MSIX_CONTROL_OFFSET as u32 * 8);

/// Bytes per MSI-X table entry: `address_lo`, `address_hi`, `data`,
/// `vector_control`.
pub const MSIX_ENTRY_SIZE: u64 = 16;

/// Vector control bit 0: this vector is masked.
///
/// Set for every entry after reset (PCI 3.0 §6.8.2.9), so a driver that has not
/// programmed an entry cannot be interrupted through it.
pub const MSIX_VECTOR_CTRL_MASKED: u32 = 1 << 0;

/// Bits of `vector_control` that exist. Everything else is reserved and reads 0,
/// so a guest write cannot store a bit the spec does not define.
const MSIX_VECTOR_CTRL_KNOWN: u32 = MSIX_VECTOR_CTRL_MASKED;

/// Most MSI-X vectors one device may have: as many entries as fit in the table
/// region. Bounds the table, the PBA and the per-queue vector map.
pub const MAX_MSIX_VECTORS: u16 = (MSIX_TABLE_LEN / MSIX_ENTRY_SIZE) as u16;

/// Table size for a device with `queues` virtqueues: one vector per queue plus
/// one for configuration changes.
///
/// `None` when that does not fit in [`MAX_MSIX_VECTORS`] — a host-side refusal
/// (the transport rejects such a device at construction), never guest input.
pub fn table_size_for(queues: usize) -> Option<u16> {
    let vectors = u16::try_from(queues.checked_add(1)?).ok()?;
    (vectors <= MAX_MSIX_VECTORS).then_some(vectors)
}

/// Encodes the MSI-X capability record for a table of `table_size` entries.
///
/// `cap_next` is left at 0: the PCI layer owns the capability *list* and patches
/// the link when it places the record, exactly as for the four virtio structure
/// locators ([`crate::pci::capability_records`]).
///
/// Message control starts at 0 apart from the table size — MSI-X disabled,
/// function not masked — which is the reset state of real hardware.
pub fn capability_record(table_size: u16) -> Vec<u8> {
    let table_size = table_size.clamp(1, MAX_MSIX_VECTORS);
    let control = (table_size - 1) & MSIX_CTRL_TABLE_SIZE_MASK;
    // Both offsets are BIR-relative and QWORD-aligned, with the BAR indicator
    // register in the low three bits. Our regions are page-aligned, so the low
    // bits are free for the BIR.
    let bir = u32::from(VIRTIO_PCI_BAR_INDEX) & 0x7;
    let table = (MSIX_TABLE_OFFSET as u32) | bir;
    let pba = (MSIX_PBA_OFFSET as u32) | bir;
    let mut record = Vec::with_capacity(usize::from(MSIX_CAP_LEN));
    record.push(PCI_CAP_ID_MSIX);
    record.push(0); // cap_next, patched by the PCI layer
    record.extend_from_slice(&control.to_le_bytes());
    record.extend_from_slice(&table.to_le_bytes());
    record.extend_from_slice(&pba.to_le_bytes());
    record
}

// ------------------------------------------------------------------ the table

/// One MSI-X table entry, as the guest programmed it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MsixEntry {
    pub address_lo: u32,
    pub address_hi: u32,
    pub data: u32,
    pub vector_control: u32,
}

impl Default for MsixEntry {
    /// The reset state: masked, and pointing nowhere.
    fn default() -> Self {
        Self {
            address_lo: 0,
            address_hi: 0,
            data: 0,
            vector_control: MSIX_VECTOR_CTRL_MASKED,
        }
    }
}

impl MsixEntry {
    pub fn is_masked(&self) -> bool {
        self.vector_control & MSIX_VECTOR_CTRL_MASKED != 0
    }

    fn message(&self) -> MsiMessage {
        MsiMessage {
            address: u64::from(self.address_lo) | (u64::from(self.address_hi) << 32),
            data: self.data,
        }
    }

    fn dword(&self, index: usize) -> u32 {
        match index {
            0 => self.address_lo,
            1 => self.address_hi,
            2 => self.data,
            3 => self.vector_control & MSIX_VECTOR_CTRL_KNOWN,
            _ => 0,
        }
    }

    fn set_dword(&mut self, index: usize, value: u32) {
        match index {
            0 => self.address_lo = value,
            1 => self.address_hi = value,
            2 => self.data = value,
            3 => self.vector_control = value & MSIX_VECTOR_CTRL_KNOWN,
            _ => (),
        }
    }
}

/// Bits per PBA word.
const PBA_BITS_PER_WORD: usize = 64;

/// The guest-visible and guest-programmed MSI-X state of one function.
struct MsixState {
    entries: Vec<MsixEntry>,
    /// Pending-bit array: bit *v* set means vector *v* wanted to interrupt while
    /// it was masked, and must be sent as soon as it is unmasked.
    pending: Vec<u64>,
    /// `config_msix_vector` from the common configuration structure.
    config_vector: u16,
    /// `queue_msix_vector`, one per virtqueue.
    queue_vectors: Vec<u16>,
}

impl MsixState {
    fn new(table_size: u16, queues: usize) -> Self {
        let entries = usize::from(table_size);
        Self {
            entries: vec![MsixEntry::default(); entries],
            pending: vec![0u64; entries.div_ceil(PBA_BITS_PER_WORD)],
            config_vector: VIRTIO_MSI_NO_VECTOR,
            queue_vectors: vec![VIRTIO_MSI_NO_VECTOR; queues],
        }
    }

    fn is_pending(&self, vector: u16) -> bool {
        let (word, bit) = (usize::from(vector) / PBA_BITS_PER_WORD, vector % 64);
        self.pending
            .get(word)
            .is_some_and(|w| w & (1u64 << bit) != 0)
    }

    fn set_pending(&mut self, vector: u16, pending: bool) {
        let (word, bit) = (usize::from(vector) / PBA_BITS_PER_WORD, vector % 64);
        if let Some(slot) = self.pending.get_mut(word) {
            match pending {
                true => *slot |= 1u64 << bit,
                false => *slot &= !(1u64 << bit),
            }
        }
    }

    /// Claims vector `vector`'s pending bit and returns the message to send, if
    /// the vector is pending *and* now deliverable (it exists and is unmasked).
    ///
    /// Claim-and-return in one step under the caller's lock is what makes draining
    /// the PBA race-free: whoever clears the bit owns the delivery.
    fn take_pending(&mut self, vector: u16) -> Option<MsiMessage> {
        let entry = *self.entries.get(usize::from(vector))?;
        if entry.is_masked() || !self.is_pending(vector) {
            return None;
        }
        self.set_pending(vector, false);
        Some(entry.message())
    }

    /// One byte of the PBA as the guest sees it. Out-of-range reads are zero.
    fn pba_byte(&self, offset: u64) -> u8 {
        let word = usize::try_from(offset / 8).unwrap_or(usize::MAX);
        let shift = (offset % 8) * 8;
        self.pending
            .get(word)
            .map(|w| (w >> shift) as u8)
            .unwrap_or(0)
    }

    /// One byte of the table as the guest sees it. Out-of-range reads are zero.
    fn table_byte(&self, offset: u64) -> u8 {
        let entry = usize::try_from(offset / MSIX_ENTRY_SIZE).unwrap_or(usize::MAX);
        let within = offset % MSIX_ENTRY_SIZE;
        let dword = (within / 4) as usize;
        let shift = (within % 4) * 8;
        self.entries
            .get(entry)
            .map(|e| (e.dword(dword) >> shift) as u8)
            .unwrap_or(0)
    }
}

// -------------------------------------------------------------- the interrupt

/// The virtio-pci interrupt object with MSI-X: an MSI per vector when the driver
/// has enabled MSI-X, the INTx line and ISR byte when it has not.
///
/// One object serves both because a driver moves between them — Linux's
/// `virtio_pci_probe` tries MSI-X per-queue vectors, then MSI-X with a shared
/// vector, then INTx, and `pci_free_irq_vectors` on an unbind puts the function
/// back on INTx — and because the device on the other side holds one
/// `Arc<dyn Interrupt>` for its whole life and must not care.
///
/// The decision is made per signal, from the message-control register the guest
/// last wrote ([`Self::control_handle`]), not cached at activation.
pub struct MsixInterrupt {
    /// The INTx half: the ISR pending word, the acknowledge semantics, the
    /// config generation counter and the interrupt line. Used verbatim while
    /// MSI-X is disabled, and its generation counter is used either way.
    line: LineInterrupt,
    sink: Arc<dyn MsiSink>,
    /// The MSI-X capability's first dword, mirrored from configuration space by
    /// the machine's PCI bus. Read on every signal, because a driver may enable,
    /// mask or disable MSI-X at any moment.
    control: Arc<AtomicU32>,
    table_size: u16,
    state: Mutex<MsixState>,
}

impl MsixInterrupt {
    /// `line` is the INTx fallback, `sink` the host MSI mechanism, `table_size`
    /// the number of vectors (see [`table_size_for`]) and `queues` how many
    /// per-queue vector registers to keep.
    pub fn new(
        line: Arc<dyn IrqLine>,
        sink: Arc<dyn MsiSink>,
        table_size: u16,
        queues: usize,
    ) -> Self {
        let table_size = table_size.clamp(1, MAX_MSIX_VECTORS);
        Self {
            line: LineInterrupt::new(line),
            sink,
            control: Arc::new(AtomicU32::new(0)),
            table_size,
            state: Mutex::new(MsixState::new(table_size, queues)),
        }
    }

    /// The handle the PCI configuration space mirrors the capability's first
    /// dword into. Plain `AtomicU32` on purpose: the config space is the
    /// machine's, knows nothing about virtio, and this keeps the coupling to one
    /// register value (ADR-0002).
    pub fn control_handle(&self) -> Arc<AtomicU32> {
        Arc::clone(&self.control)
    }

    /// How many per-queue vector registers this function keeps.
    pub fn queue_vector_count(&self) -> usize {
        self.with_state("queue vector count", |s| s.queue_vectors.len())
            .unwrap_or(0)
    }

    /// Number of vectors in the table.
    pub fn table_size(&self) -> u16 {
        self.table_size
    }

    fn control(&self) -> u16 {
        (self.control.load(Ordering::Acquire) >> (MSIX_CONTROL_OFFSET as u32 * 8)) as u16
    }

    /// Whether the driver has set the MSI-X enable bit. While false every signal
    /// takes the INTx path, which is what a `pci_free_irq_vectors` on an unbind,
    /// or a driver that never got MSI-X working, ends up on.
    pub fn is_enabled(&self) -> bool {
        self.control() & MSIX_CTRL_ENABLE != 0
    }

    /// Whether the driver has masked the whole function.
    pub fn is_function_masked(&self) -> bool {
        self.control() & MSIX_CTRL_FUNCTION_MASK != 0
    }

    /// Takes the state lock, logging rather than panicking if it is poisoned.
    ///
    /// A poisoned lock means a thread panicked while holding it, which on a
    /// guest-driven path is a bug we must not compound: losing an interrupt
    /// stalls a device, propagating a panic takes down the VM.
    fn with_state<R>(&self, what: &str, f: impl FnOnce(&mut MsixState) -> R) -> Option<R> {
        match self.state.lock() {
            Ok(mut state) => Some(f(&mut state)),
            Err(_) => {
                tracing::error!(what, "MSI-X state lock is poisoned; ignoring");
                None
            }
        }
    }

    // ------------------------------------------------------ vector registers

    /// `config_msix_vector`.
    pub fn config_vector(&self) -> u16 {
        self.with_state("config_vector read", |s| s.config_vector)
            .unwrap_or(VIRTIO_MSI_NO_VECTOR)
    }

    /// `queue_msix_vector` for queue `queue`.
    pub fn queue_vector(&self, queue: u16) -> u16 {
        self.with_state("queue_vector read", |s| {
            s.queue_vectors
                .get(usize::from(queue))
                .copied()
                .unwrap_or(VIRTIO_MSI_NO_VECTOR)
        })
        .unwrap_or(VIRTIO_MSI_NO_VECTOR)
    }

    /// Guest write to `config_msix_vector`. Returns what the register will read
    /// back, which is [`VIRTIO_MSI_NO_VECTOR`] when the request was refused.
    pub fn set_config_vector(&self, vector: u16) -> u16 {
        let accepted = self.accept(vector);
        self.with_state("config_vector write", |s| s.config_vector = accepted);
        accepted
    }

    /// Guest write to `queue_msix_vector` for queue `queue`. Returns what the
    /// register will read back; a queue that does not exist changes nothing.
    pub fn set_queue_vector(&self, queue: u16, vector: u16) -> u16 {
        let accepted = self.accept(vector);
        self.with_state("queue_vector write", |s| {
            match s.queue_vectors.get_mut(usize::from(queue)) {
                Some(slot) => {
                    *slot = accepted;
                    accepted
                }
                None => VIRTIO_MSI_NO_VECTOR,
            }
        })
        .unwrap_or(VIRTIO_MSI_NO_VECTOR)
    }

    /// A guest-supplied vector number, or [`VIRTIO_MSI_NO_VECTOR`] when it names
    /// no entry in this device's table.
    ///
    /// Reporting `NO_VECTOR` back is the spec's own failure mechanism (virtio 1.2
    /// §4.1.4.3, "if the device fails to use the vector it MUST return
    /// `VIRTIO_MSI_NO_VECTOR` on read"), so a driver learns the assignment did
    /// not take instead of waiting for interrupts that can never arrive.
    fn accept(&self, vector: u16) -> u16 {
        if vector == VIRTIO_MSI_NO_VECTOR {
            return VIRTIO_MSI_NO_VECTOR;
        }
        if vector < self.table_size {
            return vector;
        }
        tracing::warn!(
            vector,
            table_size = self.table_size,
            "driver asked for an MSI-X vector this device does not have; reporting NO_VECTOR"
        );
        VIRTIO_MSI_NO_VECTOR
    }

    // ------------------------------------------------------- table and PBA

    /// Guest read inside the MSI-X table region. Any offset and width is safe;
    /// bytes past the last entry read as zero.
    pub fn read_table(&self, offset: u64, data: &mut [u8]) {
        data.fill(0);
        self.with_state("table read", |s| {
            for (i, byte) in data.iter_mut().enumerate() {
                *byte = s.table_byte(offset.saturating_add(i as u64));
            }
        });
    }

    /// Guest read inside the MSI-X PBA region.
    pub fn read_pba(&self, offset: u64, data: &mut [u8]) {
        data.fill(0);
        self.with_state("pba read", |s| {
            for (i, byte) in data.iter_mut().enumerate() {
                *byte = s.pba_byte(offset.saturating_add(i as u64));
            }
        });
    }

    /// Guest write inside the MSI-X table region.
    ///
    /// Only naturally aligned 4- and 8-byte writes are accepted, which is every
    /// width a driver uses (Linux writes each dword with `writel`; some firmware
    /// writes the address pair as one `writeq`). Anything else is dropped rather
    /// than partially applied, on the same principle as the common-configuration
    /// decoder: a guest must not be able to reach a field by writing across it.
    ///
    /// Clearing an entry's mask bit is the moment a pending interrupt becomes
    /// deliverable, so this is also where the PBA is drained.
    pub fn write_table(&self, offset: u64, data: &[u8]) {
        let width = data.len() as u64;
        if !matches!(width, 4 | 8) || offset % width != 0 {
            tracing::debug!(
                offset,
                len = data.len(),
                "ignoring an MSI-X table write that is not a naturally aligned dword or qword"
            );
            return;
        }
        if self.is_enabled() {
            // Undefined behaviour per PCI 3.0 §6.8.3.5 (software must mask a
            // vector before changing its entry), but "undefined" is not licence
            // to do something unsafe: the write lands, and the guest can only
            // ever redirect its *own* interrupts.
            tracing::debug!(
                offset,
                "MSI-X table written while MSI-X is enabled; the driver should mask first"
            );
        }
        let entry_index = usize::try_from(offset / MSIX_ENTRY_SIZE).unwrap_or(usize::MAX);
        let first_dword = ((offset % MSIX_ENTRY_SIZE) / 4) as usize;
        let function_masked = self.is_function_masked();
        let mut deliver = None;
        self.with_state("table write", |s| {
            let Some(entry) = s.entries.get_mut(entry_index) else {
                return;
            };
            let was_masked = entry.is_masked();
            for (i, chunk) in data.chunks_exact(4).enumerate() {
                let value = u32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]);
                entry.set_dword(first_dword + i, value);
            }
            // Unmasking a vector with a pending request must send it now (PCI
            // 3.0 §6.8.3.5). The pending bit is taken *here*, under the same lock
            // as the mask bit that released it, so two threads racing to drain the
            // same vector cannot both send it.
            let unmasked = was_masked && !entry.is_masked();
            let vector = u16::try_from(entry_index).unwrap_or(u16::MAX);
            if unmasked && !function_masked {
                deliver = s.take_pending(vector);
            }
        });
        if let Some(message) = deliver {
            self.send(message, "unmasked");
        }
    }

    /// Sends every unmasked vector that has a pending bit set.
    ///
    /// Called by the machine after a configuration write changed the message
    /// control register: enabling MSI-X or clearing the function mask makes
    /// everything the PBA remembers deliverable at once (PCI 3.0 §6.8.3.5). A
    /// no-op while MSI-X is disabled or the function is masked, so the machine
    /// can call it after *any* control write without deciding anything itself.
    pub fn flush_pending(&self) {
        if !self.is_enabled() || self.is_function_masked() {
            return;
        }
        // Collected under one lock, and *claimed* while collecting: a concurrent
        // signal on the same vector then finds the bit clear and sends its own
        // message rather than duplicating this one. Bounded by the table size.
        let messages = self
            .with_state("pending drain", |s| {
                let mut out = Vec::new();
                for index in 0..s.entries.len() {
                    let vector = u16::try_from(index).unwrap_or(u16::MAX);
                    if let Some(message) = s.take_pending(vector) {
                        out.push(message);
                    }
                }
                out
            })
            .unwrap_or_default();
        for message in messages {
            self.send(message, "pending");
        }
    }

    /// The pending bits, for diagnostics and tests.
    pub fn pending_vectors(&self) -> Vec<u16> {
        self.with_state("pending vectors", |s| {
            (0..s.entries.len())
                .map(|i| u16::try_from(i).unwrap_or(u16::MAX))
                .filter(|v| s.is_pending(*v))
                .collect()
        })
        .unwrap_or_default()
    }

    /// One table entry, for diagnostics and tests.
    pub fn entry(&self, vector: u16) -> Option<MsixEntry> {
        self.with_state("entry read", |s| {
            s.entries.get(usize::from(vector)).copied()
        })
        .flatten()
    }

    // -------------------------------------------------------------- delivery

    /// Sends the message for `vector`, or records it in the PBA when it cannot be
    /// sent yet.
    fn deliver(&self, vector: u16, what: &'static str) -> Result<(), InterruptError> {
        if vector == VIRTIO_MSI_NO_VECTOR {
            // The driver assigned no vector to this source. Not an error: it
            // simply does not want to be told (Linux does this for the config
            // vector of a device it has no config-change handler for).
            tracing::trace!(what, "no MSI-X vector assigned; nothing to deliver");
            return Ok(());
        }
        let function_masked = self.is_function_masked();
        // The state lock is held across the send so that "masked ⇒ pending" and
        // "unmasked ⇒ sent" cannot interleave with a table write that flips the
        // mask bit. The sink never calls back into this crate, so it is a leaf.
        let message = self.with_state("deliver", |s| {
            let Some(entry) = s.entries.get(usize::from(vector)) else {
                // Only reachable if a vector register outlived a smaller table,
                // which cannot happen — `accept` bounds every assignment — but
                // an out-of-range index must never index anything.
                tracing::warn!(vector, what, "MSI-X vector outside the table; dropping");
                return None;
            };
            if function_masked || entry.is_masked() {
                s.set_pending(vector, true);
                return None;
            }
            let message = entry.message();
            s.set_pending(vector, false);
            Some(message)
        });
        match message.flatten() {
            Some(message) => self.sink.send(message),
            None => Ok(()),
        }
    }

    /// Sends a message the PBA had remembered, logging a failure rather than
    /// propagating it: the caller is a table write or a config-space write, neither
    /// of which has anywhere to report an interrupt failure to.
    fn send(&self, message: MsiMessage, what: &'static str) {
        if let Err(error) = self.sink.send(message) {
            tracing::error!(what, %error, "failed to deliver a pending MSI-X message");
        }
    }
}

impl Interrupt for MsixInterrupt {
    fn signal_used_queue(&self, queue_index: u16) -> Result<(), InterruptError> {
        if !self.is_enabled() {
            return self.line.signal_used_queue(queue_index);
        }
        // Under MSI-X the ISR byte is unused (virtio 1.2 §4.1.4.5: it exists
        // "when MSI-X capability is not enabled"), and this is where that is
        // honoured — the pending bits stay clear, so a driver that reads the ISR
        // out of habit sees 0 rather than a bit nothing will ever acknowledge.
        self.deliver(self.queue_vector(queue_index), "used buffers")
    }

    fn signal_config_change(&self) -> Result<(), InterruptError> {
        if !self.is_enabled() {
            return self.line.signal_config_change();
        }
        // The generation counter is the config-space *read* protocol, not an
        // interrupt mechanism, so it advances either way.
        self.line.bump_generation();
        self.deliver(self.config_vector(), "config change")
    }
}

impl TransportInterrupt for MsixInterrupt {
    fn status(&self) -> u32 {
        self.line.status()
    }

    fn ack(&self, bits: u32) {
        self.line.ack(bits);
    }

    /// Device reset.
    ///
    /// Clears the ISR, drops every vector assignment back to
    /// [`VIRTIO_MSI_NO_VECTOR`] and empties the PBA — the virtio-level state a
    /// driver re-programs during bring-up. The **table entries and the message
    /// control register are deliberately kept**: they are PCI function state
    /// owned by the MSI-X capability, and a virtio device reset (a write of 0 to
    /// `device_status`) is not a function reset. QEMU's `virtio_pci_reset` draws
    /// the same line.
    fn clear(&self) {
        self.line.clear();
        let queues = self
            .with_state("reset", |s| {
                s.config_vector = VIRTIO_MSI_NO_VECTOR;
                s.queue_vectors.fill(VIRTIO_MSI_NO_VECTOR);
                s.pending.fill(0);
                s.queue_vectors.len()
            })
            .unwrap_or(0);
        tracing::debug!(queues, "MSI-X vector assignments reset");
    }

    /// Machine reset (ADR-0005): what [`Self::clear`] does, plus the PCI
    /// function state it deliberately keeps.
    ///
    /// After a reboot the function must look untouched: MSI-X disabled and
    /// unmasked, every table entry back at its power-on value (address and data
    /// zero, vector masked). Only the two guest-writable bits of the control
    /// register are cleared — the rest of that dword is the capability's
    /// identity (id, next pointer, table size), which the machine's
    /// configuration space re-publishes into the same handle on its own reset.
    fn power_on_reset(&self) {
        TransportInterrupt::power_on_reset(&self.line);
        self.control
            .fetch_and(!MSIX_CONTROL_WRITE_MASK, Ordering::AcqRel);
        self.with_state("power-on reset", |s| {
            s.config_vector = VIRTIO_MSI_NO_VECTOR;
            s.queue_vectors.fill(VIRTIO_MSI_NO_VECTOR);
            s.pending.fill(0);
            s.entries.fill(MsixEntry::default());
        });
    }

    fn take_status(&self) -> u32 {
        self.line.take_status()
    }

    /// The whole guest-programmed MSI-X state, on top of the pending word and
    /// the generation counter every transport has (ADR-0006).
    ///
    /// The table and the message-control register are the interesting half:
    /// they are where the guest recorded which host address each queue's
    /// interrupt goes to, and a restored function that forgot them would raise
    /// nothing at all until the driver happened to re-enumerate.
    fn save_interrupt(&self) -> crate::save::InterruptState {
        let msix = self
            .with_state("save", |s| crate::save::MsixState {
                control: self.control.load(Ordering::Acquire),
                config_vector: s.config_vector,
                queue_vectors: s.queue_vectors.clone(),
                entries: s
                    .entries
                    .iter()
                    .map(|e| crate::save::MsixEntryState {
                        address_lo: e.address_lo,
                        address_hi: e.address_hi,
                        data: e.data,
                        vector_control: e.vector_control,
                    })
                    .collect(),
                pending: s.pending.clone(),
            })
            .unwrap_or_default();
        crate::save::InterruptState {
            isr: self.line.status(),
            generation: self.line.generation(),
            msix: Some(msix),
        }
    }

    /// Puts it back.
    ///
    /// The table size is the *function's* shape, decided by the host when the
    /// bus was built, so a snapshot whose table is a different size is refused
    /// rather than truncated: it describes a function this machine did not
    /// build.
    ///
    /// Nothing pending is delivered here. The PBA is restored as it was, and
    /// the guest unmasking a vector is what sends it — exactly as it would have
    /// been without the suspend.
    fn load_interrupt(
        &self,
        state: &crate::save::InterruptState,
    ) -> Result<(), crate::save::StateError> {
        self.line.restore(state.isr, state.generation);
        let Some(msix) = &state.msix else {
            // A snapshot of a function without MSI-X being loaded into one with
            // it: the machine's shape check should have caught this first, so
            // reaching here means the two disagree.
            return Err(crate::save::StateError::MsixTableSize {
                snapshot: 0,
                current: usize::from(self.table_size),
            });
        };
        if msix.entries.len() != usize::from(self.table_size) {
            return Err(crate::save::StateError::MsixTableSize {
                snapshot: msix.entries.len(),
                current: usize::from(self.table_size),
            });
        }
        self.control.store(msix.control, Ordering::Release);
        self.with_state("load", |s| {
            if msix.queue_vectors.len() == s.queue_vectors.len() {
                s.queue_vectors.copy_from_slice(&msix.queue_vectors);
            }
            s.config_vector = msix.config_vector;
            for (slot, saved) in s.entries.iter_mut().zip(&msix.entries) {
                slot.address_lo = saved.address_lo;
                slot.address_hi = saved.address_hi;
                slot.data = saved.data;
                slot.vector_control = saved.vector_control & MSIX_VECTOR_CTRL_KNOWN;
            }
            for (slot, saved) in s.pending.iter_mut().zip(&msix.pending) {
                *slot = *saved;
            }
        });
        if msix.queue_vectors.len() != usize::from(self.table_size)
            && msix.queue_vectors.len() != self.queue_vector_count()
        {
            return Err(crate::save::StateError::QueueCount {
                snapshot: msix.queue_vectors.len(),
                current: self.queue_vector_count(),
            });
        }
        Ok(())
    }

    fn generation(&self) -> u32 {
        self.line.generation()
    }

    fn as_interrupt(self: Arc<Self>) -> Arc<dyn Interrupt> {
        self
    }
}

/// Compile-time proof that the two regions can hold what the bounds claim.
const _: () = {
    assert!(MSIX_TABLE_LEN >= MAX_MSIX_VECTORS as u64 * MSIX_ENTRY_SIZE);
    assert!(MSIX_PBA_LEN * 8 >= MAX_MSIX_VECTORS as u64);
    assert!(MAX_MSIX_VECTORS <= MSIX_CTRL_TABLE_SIZE_MASK + 1);
    assert!(MAX_MSIX_VECTORS != VIRTIO_MSI_NO_VECTOR);
};

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pci::VIRTIO_PCI_BAR_SIZE;
    use crate::testing::{TestIrqLine, TestMsiSink};

    struct Fixture {
        msix: Arc<MsixInterrupt>,
        sink: Arc<TestMsiSink>,
        line: Arc<TestIrqLine>,
    }

    fn fixture(table_size: u16, queues: usize) -> Fixture {
        let line = Arc::new(TestIrqLine::default());
        let sink = Arc::new(TestMsiSink::default());
        let msix = Arc::new(MsixInterrupt::new(
            line.clone(),
            sink.clone(),
            table_size,
            queues,
        ));
        Fixture { msix, sink, line }
    }

    /// Writes the message-control halfword the way the guest's configuration
    /// write would, i.e. into the upper half of the mirrored dword.
    fn set_control(msix: &MsixInterrupt, control: u16) {
        msix.control_handle().store(
            u32::from(control) << (MSIX_CONTROL_OFFSET as u32 * 8),
            Ordering::Release,
        );
    }

    fn program(msix: &MsixInterrupt, vector: u16, address: u64, data: u32) {
        let base = u64::from(vector) * MSIX_ENTRY_SIZE;
        msix.write_table(base, &(address as u32).to_le_bytes());
        msix.write_table(base + 4, &((address >> 32) as u32).to_le_bytes());
        msix.write_table(base + 8, &data.to_le_bytes());
        msix.write_table(base + 12, &0u32.to_le_bytes()); // unmask
    }

    // -------------------------------------------------------- the capability

    #[test]
    fn the_capability_record_describes_the_real_regions() {
        let record = capability_record(3);
        assert_eq!(record.len(), usize::from(MSIX_CAP_LEN));
        assert_eq!(record[0], PCI_CAP_ID_MSIX);
        assert_eq!(record[1], 0, "cap_next is the PCI layer's to patch");

        let control = u16::from_le_bytes([record[2], record[3]]);
        assert_eq!(control & MSIX_CTRL_TABLE_SIZE_MASK, 2, "table size - 1");
        assert_eq!(control & MSIX_CTRL_ENABLE, 0, "MSI-X starts disabled");
        assert_eq!(control & MSIX_CTRL_FUNCTION_MASK, 0);

        let table = u32::from_le_bytes([record[4], record[5], record[6], record[7]]);
        let pba = u32::from_le_bytes([record[8], record[9], record[10], record[11]]);
        assert_eq!(u32::from(VIRTIO_PCI_BAR_INDEX), table & 0x7, "table BIR");
        assert_eq!(u32::from(VIRTIO_PCI_BAR_INDEX), pba & 0x7, "PBA BIR");
        assert_eq!(u64::from(table & !0x7), MSIX_TABLE_OFFSET);
        assert_eq!(u64::from(pba & !0x7), MSIX_PBA_OFFSET);
        // Both regions must fit in the BAR that claims to hold them.
        const {
            assert!(MSIX_TABLE_OFFSET + MSIX_TABLE_LEN <= VIRTIO_PCI_BAR_SIZE);
            assert!(MSIX_PBA_OFFSET + MSIX_PBA_LEN <= VIRTIO_PCI_BAR_SIZE);
        }
    }

    #[test]
    fn table_size_is_one_vector_per_queue_plus_config() {
        assert_eq!(table_size_for(1), Some(2));
        assert_eq!(table_size_for(2), Some(3));
        assert_eq!(
            table_size_for(usize::from(MAX_MSIX_VECTORS) - 1),
            Some(MAX_MSIX_VECTORS)
        );
        // One queue too many for the table region: a host-side refusal.
        assert_eq!(table_size_for(usize::from(MAX_MSIX_VECTORS)), None);
        assert_eq!(table_size_for(usize::MAX), None);
        // A table size the control register cannot encode is impossible.
        const { assert!(MAX_MSIX_VECTORS <= MSIX_CTRL_TABLE_SIZE_MASK + 1) };
    }

    /// The reset state of every entry is "masked", so a driver that has not
    /// programmed the table cannot be interrupted through it.
    #[test]
    fn every_entry_starts_masked_and_pointing_nowhere() {
        let f = fixture(3, 2);
        for vector in 0..3 {
            let entry = f.msix.entry(vector).expect("entry exists");
            assert!(entry.is_masked(), "vector {vector}");
            assert_eq!(
                entry.message(),
                MsiMessage {
                    address: 0,
                    data: 0
                }
            );
        }
        assert_eq!(f.msix.entry(3), None, "the table has three entries");
        assert!(!f.msix.is_enabled());
        assert!(!f.msix.is_function_masked());
    }

    // ------------------------------------------------------ vector registers

    #[test]
    fn vector_assignment_round_trips_and_refuses_out_of_range() {
        let f = fixture(3, 2);
        assert_eq!(f.msix.config_vector(), VIRTIO_MSI_NO_VECTOR);
        assert_eq!(f.msix.queue_vector(0), VIRTIO_MSI_NO_VECTOR);

        assert_eq!(f.msix.set_config_vector(0), 0);
        assert_eq!(f.msix.set_queue_vector(0, 1), 1);
        assert_eq!(f.msix.set_queue_vector(1, 2), 2);
        assert_eq!(f.msix.config_vector(), 0);
        assert_eq!(f.msix.queue_vector(0), 1);
        assert_eq!(f.msix.queue_vector(1), 2);

        // A vector outside the table reads back NO_VECTOR — the spec's way of
        // telling the driver the assignment did not take.
        assert_eq!(f.msix.set_queue_vector(0, 3), VIRTIO_MSI_NO_VECTOR);
        assert_eq!(f.msix.queue_vector(0), VIRTIO_MSI_NO_VECTOR);
        assert_eq!(f.msix.set_config_vector(0xfffe), VIRTIO_MSI_NO_VECTOR);
        assert_eq!(f.msix.config_vector(), VIRTIO_MSI_NO_VECTOR);
        // NO_VECTOR itself is a legal write: "do not interrupt me for this".
        assert_eq!(f.msix.set_config_vector(VIRTIO_MSI_NO_VECTOR), 0xffff);

        // A queue the device does not have changes nothing.
        assert_eq!(f.msix.set_queue_vector(7, 1), VIRTIO_MSI_NO_VECTOR);
        assert_eq!(f.msix.queue_vector(7), VIRTIO_MSI_NO_VECTOR);
        assert_eq!(f.msix.queue_vector(1), 2, "queue 1 was not disturbed");
    }

    // -------------------------------------------------------------- delivery

    #[test]
    fn an_enabled_unmasked_vector_sends_exactly_the_message_the_guest_programmed() {
        let f = fixture(3, 2);
        program(&f.msix, 1, 0xfee0_1000, 0x4021);
        f.msix.set_queue_vector(0, 1);
        set_control(&f.msix, MSIX_CTRL_ENABLE);

        assert!(f.msix.signal_used_queue(0).is_ok());
        assert_eq!(
            f.sink.sent(),
            vec![MsiMessage {
                address: 0xfee0_1000,
                data: 0x4021
            }]
        );
        // Under MSI-X the INTx line is never raised and the ISR stays clear.
        assert_eq!(f.line.count(), 0);
        assert_eq!(f.msix.status(), 0);
        assert!(f.msix.pending_vectors().is_empty());
    }

    #[test]
    fn a_64_bit_message_address_is_assembled_from_both_halves() {
        let f = fixture(2, 1);
        program(&f.msix, 0, 0x0000_00ff_fee0_2000, 0x1234);
        f.msix.set_queue_vector(0, 0);
        set_control(&f.msix, MSIX_CTRL_ENABLE);
        assert!(f.msix.signal_used_queue(0).is_ok());
        assert_eq!(f.sink.sent()[0].address, 0x0000_00ff_fee0_2000);
    }

    /// The config-change source has its own vector and still advances the
    /// generation counter, which is a config-space read protocol rather than an
    /// interrupt.
    #[test]
    fn a_config_change_uses_the_config_vector_and_bumps_the_generation() {
        let f = fixture(3, 2);
        program(&f.msix, 2, 0xfee0_3000, 0x99);
        f.msix.set_config_vector(2);
        set_control(&f.msix, MSIX_CTRL_ENABLE);

        assert_eq!(f.msix.generation(), 0);
        assert!(f.msix.signal_config_change().is_ok());
        assert_eq!(f.msix.generation(), 1);
        assert_eq!(f.sink.sent().len(), 1);
        assert_eq!(f.sink.sent()[0].data, 0x99);
        assert_eq!(f.msix.status(), 0, "the ISR is unused under MSI-X");
        assert_eq!(f.line.count(), 0);
    }

    #[test]
    fn a_queue_with_no_vector_sends_nothing_and_is_not_an_error() {
        let f = fixture(3, 2);
        set_control(&f.msix, MSIX_CTRL_ENABLE);
        assert!(f.msix.signal_used_queue(0).is_ok());
        assert!(f.msix.signal_config_change().is_ok());
        assert_eq!(f.sink.count(), 0);
        assert_eq!(f.line.count(), 0, "and INTx is not a fallback per-source");
        assert!(f.msix.pending_vectors().is_empty());
    }

    /// A queue index the device does not have resolves to NO_VECTOR, so it is
    /// dropped rather than indexing the table.
    #[test]
    fn a_signal_for_an_unknown_queue_is_dropped() {
        let f = fixture(3, 2);
        program(&f.msix, 0, 0xfee0_1000, 1);
        f.msix.set_queue_vector(0, 0);
        set_control(&f.msix, MSIX_CTRL_ENABLE);
        assert!(f.msix.signal_used_queue(99).is_ok());
        assert_eq!(f.sink.count(), 0);
    }

    // ---------------------------------------------------- masking and the PBA

    #[test]
    fn a_masked_vector_sets_its_pending_bit_and_sends_on_unmask() {
        let f = fixture(3, 2);
        program(&f.msix, 1, 0xfee0_1000, 0x4021);
        f.msix.set_queue_vector(0, 1);
        set_control(&f.msix, MSIX_CTRL_ENABLE);
        // Mask vector 1 the way `pci_msix_mask_irq` does: write vector_control.
        f.msix
            .write_table(MSIX_ENTRY_SIZE + 12, &MSIX_VECTOR_CTRL_MASKED.to_le_bytes());

        assert!(f.msix.signal_used_queue(0).is_ok());
        assert_eq!(f.sink.count(), 0, "masked vectors do not interrupt");
        assert_eq!(f.msix.pending_vectors(), vec![1]);
        // The PBA is guest-readable and says exactly that.
        let mut pba = [0u8; 8];
        f.msix.read_pba(0, &mut pba);
        assert_eq!(u64::from_le_bytes(pba), 1 << 1);

        // A second signal while masked coalesces: MSI-X has one pending bit per
        // vector, not a counter.
        assert!(f.msix.signal_used_queue(0).is_ok());
        assert_eq!(f.msix.pending_vectors(), vec![1]);

        // Unmasking sends it and clears the bit.
        f.msix
            .write_table(MSIX_ENTRY_SIZE + 12, &0u32.to_le_bytes());
        assert_eq!(f.sink.count(), 1);
        assert!(f.msix.pending_vectors().is_empty());
        f.msix.read_pba(0, &mut pba);
        assert_eq!(u64::from_le_bytes(pba), 0);
    }

    #[test]
    fn the_function_mask_holds_every_vector_and_flush_releases_them() {
        let f = fixture(3, 2);
        program(&f.msix, 0, 0xfee0_1000, 0x10);
        program(&f.msix, 1, 0xfee0_1000, 0x11);
        f.msix.set_queue_vector(0, 0);
        f.msix.set_queue_vector(1, 1);
        set_control(&f.msix, MSIX_CTRL_ENABLE | MSIX_CTRL_FUNCTION_MASK);

        assert!(f.msix.signal_used_queue(0).is_ok());
        assert!(f.msix.signal_used_queue(1).is_ok());
        assert_eq!(f.sink.count(), 0);
        assert_eq!(f.msix.pending_vectors(), vec![0, 1]);
        // A flush while still masked must not release anything.
        f.msix.flush_pending();
        assert_eq!(f.sink.count(), 0);

        // Clearing the function mask is a *configuration space* write, so the
        // machine tells the transport to drain the PBA.
        set_control(&f.msix, MSIX_CTRL_ENABLE);
        f.msix.flush_pending();
        assert_eq!(f.sink.count(), 2);
        assert!(f.msix.pending_vectors().is_empty());
        let sent: Vec<u32> = f.sink.sent().iter().map(|m| m.data).collect();
        assert_eq!(sent, vec![0x10, 0x11]);
    }

    /// A pending bit set while MSI-X was disabled is released when it is enabled.
    #[test]
    fn flushing_is_a_no_op_while_msix_is_disabled() {
        let f = fixture(2, 1);
        program(&f.msix, 0, 0xfee0_1000, 7);
        f.msix.set_queue_vector(0, 0);
        set_control(&f.msix, MSIX_CTRL_ENABLE | MSIX_CTRL_FUNCTION_MASK);
        assert!(f.msix.signal_used_queue(0).is_ok());
        assert_eq!(f.msix.pending_vectors(), vec![0]);

        set_control(&f.msix, 0);
        f.msix.flush_pending();
        assert_eq!(f.sink.count(), 0, "nothing is deliverable while disabled");
        set_control(&f.msix, MSIX_CTRL_ENABLE);
        f.msix.flush_pending();
        assert_eq!(f.sink.count(), 1);
    }

    // ------------------------------------------------------ INTx interaction

    /// With MSI-X disabled the object *is* the INTx interrupt, bit for bit — the
    /// state Linux's probe starts in and returns to on `pci_free_irq_vectors`.
    #[test]
    fn with_msix_disabled_every_signal_takes_the_intx_path() {
        let f = fixture(3, 2);
        program(&f.msix, 1, 0xfee0_1000, 0x4021);
        f.msix.set_queue_vector(0, 1);

        assert!(f.msix.signal_used_queue(0).is_ok());
        assert_eq!(f.sink.count(), 0, "no MSI while the capability is disabled");
        assert_eq!(f.line.count(), 1);
        assert_eq!(f.msix.status(), crate::mmio::INT_VRING);
        assert_eq!(f.msix.take_status(), crate::mmio::INT_VRING);
        assert_eq!(f.msix.status(), 0);
    }

    /// The transition Linux makes mid-probe: INTx, then MSI-X, then (on an
    /// unbind) INTx again. Each signal must take the path that is live *at the
    /// time of the signal*, not the one that was live at activation.
    #[test]
    fn toggling_msix_moves_signals_between_the_two_paths() {
        let f = fixture(3, 2);
        program(&f.msix, 1, 0xfee0_1000, 0x4021);
        f.msix.set_queue_vector(0, 1);

        assert!(f.msix.signal_used_queue(0).is_ok());
        set_control(&f.msix, MSIX_CTRL_ENABLE);
        assert!(f.msix.signal_used_queue(0).is_ok());
        set_control(&f.msix, 0);
        assert!(f.msix.signal_used_queue(0).is_ok());

        assert_eq!(f.sink.count(), 1, "only the middle signal was an MSI");
        assert_eq!(f.line.count(), 2, "the other two raised INTx");
    }

    #[test]
    fn a_failing_sink_propagates_but_leaves_the_vector_unpending() {
        let line = Arc::new(TestIrqLine::default());
        let sink = Arc::new(TestMsiSink::failing());
        let msix = MsixInterrupt::new(line, sink, 2, 1);
        program(&msix, 0, 0xfee0_1000, 1);
        msix.set_queue_vector(0, 0);
        set_control(&msix, MSIX_CTRL_ENABLE);
        // The device learns the delivery failed and can set DEVICE_NEEDS_RESET;
        // the PBA is not a retry queue, so the bit stays clear.
        assert!(msix.signal_used_queue(0).is_err());
        assert!(msix.pending_vectors().is_empty());
    }

    // ------------------------------------------------------------- reset

    #[test]
    fn a_device_reset_drops_the_vector_assignments_but_keeps_the_table() {
        let f = fixture(3, 2);
        program(&f.msix, 1, 0xfee0_1000, 0x4021);
        f.msix.set_queue_vector(0, 1);
        f.msix.set_config_vector(2);
        set_control(&f.msix, MSIX_CTRL_ENABLE | MSIX_CTRL_FUNCTION_MASK);
        assert!(f.msix.signal_used_queue(0).is_ok());
        assert_eq!(f.msix.pending_vectors(), vec![1]);

        f.msix.clear();
        assert_eq!(f.msix.queue_vector(0), VIRTIO_MSI_NO_VECTOR);
        assert_eq!(f.msix.config_vector(), VIRTIO_MSI_NO_VECTOR);
        assert!(f.msix.pending_vectors().is_empty(), "the PBA is emptied");
        // The table entry and the enable bit belong to the PCI function, not to
        // the virtio device, and survive.
        assert_eq!(f.msix.entry(1).expect("entry").data, 0x4021);
        assert!(f.msix.is_enabled());
    }

    // -------------------------------------------------- malicious table access

    /// Every offset and width must be safe, and nothing outside the table may be
    /// reachable through it.
    #[test]
    fn table_and_pba_accesses_are_bounded_at_every_width() {
        let f = fixture(2, 1);
        program(&f.msix, 0, 0xfee0_1000, 0x4021);

        // Reads past the last entry are zeroes, not a neighbour's bytes.
        let mut data = [0xffu8; 8];
        f.msix.read_table(2 * MSIX_ENTRY_SIZE, &mut data);
        assert_eq!(data, [0u8; 8]);
        f.msix.read_table(MSIX_TABLE_LEN - 4, &mut data);
        assert_eq!(data, [0u8; 8]);
        f.msix.read_table(u64::MAX - 4, &mut data);
        assert_eq!(data, [0u8; 8]);
        f.msix.read_pba(MSIX_PBA_LEN, &mut data);
        assert_eq!(data, [0u8; 8]);
        f.msix.read_pba(u64::MAX - 4, &mut data);
        assert_eq!(data, [0u8; 8]);

        // Writes past the last entry change nothing.
        f.msix
            .write_table(2 * MSIX_ENTRY_SIZE, &0xdead_beefu32.to_le_bytes());
        f.msix
            .write_table(MSIX_TABLE_LEN, &0xdead_beefu32.to_le_bytes());
        f.msix.write_table(u64::MAX - 7, &0u64.to_le_bytes());
        assert_eq!(f.msix.entry(0).expect("entry").address_lo, 0xfee0_1000);

        // Misaligned and odd-width writes are dropped whole, never applied in
        // part: a guest must not reach `vector_control` by writing across it.
        f.msix.write_table(1, &0xffff_ffffu32.to_le_bytes());
        f.msix.write_table(2, &0xffffu16.to_le_bytes());
        f.msix.write_table(0, &[0xff]);
        f.msix
            .write_table(4, &0xffff_ffff_ffff_ffffu64.to_le_bytes());
        let entry = f.msix.entry(0).expect("entry");
        assert_eq!(entry.address_lo, 0xfee0_1000);
        assert_eq!(entry.address_hi, 0);
        assert!(!entry.is_masked(), "vector_control was not reached");

        // Reserved bits of vector_control are dropped rather than stored.
        f.msix.write_table(12, &0xffff_ffffu32.to_le_bytes());
        assert_eq!(
            f.msix.entry(0).expect("entry").vector_control,
            MSIX_VECTOR_CTRL_MASKED
        );
    }

    /// An aligned qword write programs the address pair in one access, which is
    /// what a firmware that treats the entry as a `struct` does.
    #[test]
    fn an_aligned_qword_write_programs_both_address_halves() {
        let f = fixture(2, 1);
        f.msix
            .write_table(0, &0x0000_0001_fee0_4000u64.to_le_bytes());
        let entry = f.msix.entry(0).expect("entry");
        assert_eq!(entry.address_lo, 0xfee0_4000);
        assert_eq!(entry.address_hi, 1);
    }

    /// The PBA is read-only: a guest cannot forge or clear a pending interrupt.
    #[test]
    fn the_pba_cannot_be_written() {
        let f = fixture(2, 1);
        program(&f.msix, 0, 0xfee0_1000, 1);
        f.msix.set_queue_vector(0, 0);
        set_control(&f.msix, MSIX_CTRL_ENABLE | MSIX_CTRL_FUNCTION_MASK);
        assert!(f.msix.signal_used_queue(0).is_ok());
        assert_eq!(f.msix.pending_vectors(), vec![0]);

        // There is no `write_pba`; the transport drops writes to the region. The
        // bit is therefore still there, and only a delivery clears it.
        let mut pba = [0u8; 8];
        f.msix.read_pba(0, &mut pba);
        assert_eq!(u64::from_le_bytes(pba), 1);
        set_control(&f.msix, MSIX_CTRL_ENABLE);
        f.msix.flush_pending();
        f.msix.read_pba(0, &mut pba);
        assert_eq!(u64::from_le_bytes(pba), 0);
    }
}
