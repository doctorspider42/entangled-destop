//! A userspace 82093AA I/O APIC (backlog WHP-1703).
//!
//! KVM hands us an in-kernel IOAPIC (`KVM_CREATE_IRQCHIP`) and an irqfd that
//! reaches it without ever entering userspace. WHP has no such thing: it emulates
//! the **local** APIC of each vCPU and nothing else, so the redirection table has
//! to live here and each pin assertion has to be turned into an explicit
//! interrupt message through [`InterruptDelivery`] (`WHvRequestInterrupt`).
//!
//! # Guest-visible model
//!
//! One IOAPIC with [`REDIRECTION_ENTRIES`] pins at
//! [`crate::layout::IOAPIC_ADDR`], GSI base 0 — exactly the topology
//! [`crate::mptable`] and [`crate::acpi`] already publish, so the guest sees one
//! machine whichever table it reads. Two 32-bit registers in a 32-byte window:
//!
//! | Offset | Register | Access |
//! |---|---|---|
//! | `+0x00` | `IOREGSEL` — index of the register `IOWIN` addresses | read/write |
//! | `+0x10` | `IOWIN` — the selected register | read/write |
//!
//! Indices: `0x00` ID (bits 27:24), `0x01` VERSION (version `0x11`, max
//! redirection entry in bits 23:16), `0x02` arbitration ID, and
//! `0x10 + 2n`/`0x11 + 2n` the low and high halves of redirection entry *n*.
//!
//! # Redirection table entry
//!
//! | Bits | Field | Modelled |
//! |---|---|---|
//! | 7:0 | vector | yes |
//! | 10:8 | delivery mode | Fixed / LowestPriority / NMI; SMI, INIT and ExtINT are dropped |
//! | 11 | destination mode | yes (physical/logical) |
//! | 12 | delivery status | always reads 0 — delivery here is synchronous |
//! | 13 | interrupt input polarity | accepted, not modelled (every line we own is active-high) |
//! | 14 | remote IRR | always reads 0 — see "no EOI tracking" below |
//! | 15 | trigger mode | passed through to the local APIC |
//! | 16 | mask | yes |
//! | 63:56 | destination field | yes |
//!
//! # Deliberate simplifications
//!
//! * **No EOI tracking / remote IRR.** A real IOAPIC latches a level-triggered
//!   interrupt until the local APIC broadcasts the EOI, which WHP can report
//!   (`WHvRunVpExitReasonX64ApicEoi`). Every line this machine owns is an
//!   edge-triggered ISA-style pin — the 8254 on pin 2, the UART on pin 4, virtio
//!   on 5.. — so nothing needs re-assertion, and turning the EOI exit on for all
//!   of them would only cost exits. This is the same call the KVM path already
//!   made for its irqfds (`crate::irqfd`).
//! * **A masked pin latches one pending edge.** Hardware loses an edge that
//!   arrives while the pin is masked. We remember it and deliver on unmask,
//!   because losing one is not symmetric in consequence: Linux masks and unmasks
//!   IRQ 0 repeatedly in `check_timer()`, and a UART THRE edge lost during a mask
//!   window stalls the console for good. One bit per pin, so a stuck device
//!   cannot make the host queue grow.
//! * **Polarity is accepted and ignored.** Nothing on this machine drives an
//!   active-low ISA line.

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use virtio_core::interrupt::{InterruptError, IrqLine};
use vmm_core::hv::{
    DestinationMode, InterruptDelivery, InterruptKind, InterruptRequest, TriggerMode,
};

use crate::layout;

/// Pins on the IOAPIC — the 82093AA's 24, which is also the bound
/// `crate::virtio::MAX_VIRTIO_SLOTS` was chosen against.
pub const REDIRECTION_ENTRIES: usize = 24;

/// Size of the register window we decode (`IOREGSEL` + `IOWIN` + padding).
pub const WINDOW_SIZE: u64 = 0x20;

const IOREGSEL: u64 = 0x00;
const IOWIN: u64 = 0x10;

const REG_ID: u32 = 0x00;
const REG_VERSION: u32 = 0x01;
const REG_ARBITRATION: u32 = 0x02;
const REG_REDIRECTION_BASE: u32 = 0x10;

/// Version byte of the 82093AA, as every other VMM reports it.
const IOAPIC_VERSION: u32 = 0x11;

// Redirection entry bits (low dword).
const RTE_VECTOR: u64 = 0xff;
const RTE_DELIVERY_MODE_SHIFT: u64 = 8;
const RTE_DESTINATION_MODE: u64 = 1 << 11;
const RTE_TRIGGER_LEVEL: u64 = 1 << 15;
const RTE_MASK: u64 = 1 << 16;
/// Delivery status (12) and remote IRR (14) are read-only zero, so a guest
/// write must not be able to store them.
const RTE_READ_ONLY: u64 = (1 << 12) | (1 << 14);
const RTE_DESTINATION_SHIFT: u64 = 56;

// Delivery modes, bits 10:8.
const DELIVERY_FIXED: u64 = 0b000;
const DELIVERY_LOWEST_PRIORITY: u64 = 0b001;
const DELIVERY_NMI: u64 = 0b100;

/// Redirection entry at reset: masked, everything else zero, per the datasheet.
const RTE_RESET: u64 = RTE_MASK;

struct IoApicState {
    /// `IOREGSEL`. Guest-controlled, so every use is bounds-checked rather than
    /// trusted as an index.
    select: u32,
    id: u8,
    redirection: [u64; REDIRECTION_ENTRIES],
    /// One bit per pin: an edge arrived while the pin was masked.
    pending: u32,
}

/// The machine's I/O APIC.
///
/// Shared (`Arc`) between the bus — which serves the guest's MMIO — and every
/// [`IoApicLine`] handed to a device.
pub struct IoApic {
    state: Mutex<IoApicState>,
    delivery: Arc<dyn InterruptDelivery>,
    /// Messages actually handed to the local APIC. Diagnostics only, but the
    /// cheapest way for a test or `entangled doctor` to tell "the device never
    /// raised its line" from "the guest never unmasked the pin".
    delivered: AtomicU32,
}

impl IoApic {
    /// Creates the IOAPIC with every pin masked, reporting `id` in its ID
    /// register. `id` must be the one the MP table and MADT publish
    /// (`vcpu_count`, above every LAPIC id).
    pub fn new(delivery: Arc<dyn InterruptDelivery>, id: u8) -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(IoApicState {
                select: 0,
                id,
                redirection: [RTE_RESET; REDIRECTION_ENTRIES],
                pending: 0,
            }),
            delivery,
            delivered: AtomicU32::new(0),
        })
    }

    /// True when `addr` falls in the IOAPIC's register window.
    pub fn contains(addr: u64) -> bool {
        let base = u64::from(layout::IOAPIC_ADDR);
        (base..base + WINDOW_SIZE).contains(&addr)
    }

    /// An [`IrqLine`] for `pin`, for a device (or the serial console) to raise.
    ///
    /// Pins are GSIs: the IOAPIC's GSI base is 0 in both published tables, so
    /// pin *n* is GSI *n*. Out-of-range pins are rejected here rather than at
    /// trigger time, so a mis-wired machine fails at construction.
    pub fn line(self: &Arc<Self>, pin: u8) -> Result<Arc<IoApicLine>, IoApicError> {
        if usize::from(pin) >= REDIRECTION_ENTRIES {
            return Err(IoApicError::NoSuchPin(pin));
        }
        Ok(Arc::new(IoApicLine {
            ioapic: Arc::clone(self),
            pin,
        }))
    }

    /// How many interrupt messages this IOAPIC has handed to the local APIC.
    pub fn delivered(&self) -> u32 {
        self.delivered.load(Ordering::Acquire)
    }

    /// Asserts `pin` as an edge: deliver now, or latch if the pin is masked.
    ///
    /// Never fails towards the caller for a masked or unroutable pin — that is
    /// the guest's configuration, not an error. Only a hypervisor injection
    /// failure surfaces.
    pub fn pulse(&self, pin: u8) -> Result<(), InterruptError> {
        let request = {
            let mut state = self
                .state
                .lock()
                .map_err(|_| InterruptError::Signal("IOAPIC lock is poisoned".into()))?;
            let Some(&entry) = state.redirection.get(usize::from(pin)) else {
                return Ok(());
            };
            if entry & RTE_MASK != 0 {
                state.pending |= 1 << u32::from(pin);
                return Ok(());
            }
            decode(entry)
        };
        self.deliver(request)
    }

    fn deliver(&self, request: Option<InterruptRequest>) -> Result<(), InterruptError> {
        let Some(request) = request else {
            return Ok(());
        };
        self.delivery
            .request(&request)
            .map_err(|e| InterruptError::Signal(e.to_string()))?;
        self.delivered.fetch_add(1, Ordering::AcqRel);
        Ok(())
    }

    /// Guest read from the register window. Only aligned 32-bit accesses are
    /// meaningful on an IOAPIC; anything else reads zero, as an unclaimed MMIO
    /// address would.
    pub fn mmio_read(&self, addr: u64, data: &mut [u8]) {
        data.fill(0);
        if data.len() != 4 {
            return;
        }
        let Some(offset) = addr.checked_sub(u64::from(layout::IOAPIC_ADDR)) else {
            return;
        };
        let Ok(state) = self.state.lock() else {
            tracing::error!("IOAPIC lock is poisoned; reading zeroes");
            return;
        };
        let value = match offset {
            IOREGSEL => state.select,
            IOWIN => read_register(&state),
            _ => 0,
        };
        data.copy_from_slice(&value.to_le_bytes());
    }

    /// Guest write to the register window.
    pub fn mmio_write(&self, addr: u64, data: &[u8]) {
        if data.len() != 4 {
            return;
        }
        let Some(offset) = addr.checked_sub(u64::from(layout::IOAPIC_ADDR)) else {
            return;
        };
        let Ok(bytes) = <[u8; 4]>::try_from(data) else {
            return;
        };
        let value = u32::from_le_bytes(bytes);

        // An unmask may release a latched edge, which must be delivered *after*
        // the lock is dropped: `InterruptDelivery::request` can re-enter host
        // code and must never run under the IOAPIC lock.
        let released = {
            let Ok(mut state) = self.state.lock() else {
                tracing::error!("IOAPIC lock is poisoned; dropping guest write");
                return;
            };
            match offset {
                IOREGSEL => {
                    state.select = value;
                    None
                }
                IOWIN => write_register(&mut state, value),
                _ => None,
            }
        };
        if let Some(pin) = released {
            let request = match self.state.lock() {
                Ok(state) => state
                    .redirection
                    .get(usize::from(pin))
                    .copied()
                    .and_then(decode),
                Err(_) => None,
            };
            if let Err(e) = self.deliver(request) {
                tracing::warn!(pin, error = %e, "delivering a latched IOAPIC edge failed");
            }
        }
    }
}

/// Serves a read of the register named by `IOREGSEL`.
fn read_register(state: &IoApicState) -> u32 {
    match state.select {
        REG_ID => u32::from(state.id) << 24,
        REG_VERSION => IOAPIC_VERSION | ((REDIRECTION_ENTRIES as u32 - 1) << 16),
        REG_ARBITRATION => u32::from(state.id) << 24,
        index => {
            let Some((pin, half)) = redirection_index(index) else {
                return 0;
            };
            let entry = state.redirection[pin];
            if half == 0 {
                entry as u32
            } else {
                (entry >> 32) as u32
            }
        }
    }
}

/// Serves a write of the register named by `IOREGSEL`. Returns the pin whose
/// latched edge was just released, if any.
fn write_register(state: &mut IoApicState, value: u32) -> Option<u8> {
    match state.select {
        REG_ID => {
            state.id = ((value >> 24) & 0x0f) as u8;
            None
        }
        // Version and arbitration are read-only.
        REG_VERSION | REG_ARBITRATION => None,
        index => {
            let (pin, half) = redirection_index(index)?;
            let was_masked = state.redirection[pin] & RTE_MASK != 0;
            let entry = &mut state.redirection[pin];
            if half == 0 {
                *entry = (*entry & 0xffff_ffff_0000_0000) | u64::from(value);
            } else {
                *entry = (*entry & 0x0000_0000_ffff_ffff) | (u64::from(value) << 32);
            }
            *entry &= !RTE_READ_ONLY;
            let now_masked = *entry & RTE_MASK != 0;
            let pin = u8::try_from(pin).ok()?;
            let latched = state.pending & (1 << u32::from(pin)) != 0;
            if was_masked && !now_masked && latched {
                state.pending &= !(1 << u32::from(pin));
                return Some(pin);
            }
            None
        }
    }
}

/// Splits an `IOREGSEL` value into (pin, half) for the redirection table, or
/// `None` when it names something else. The index is guest-controlled, so this
/// is the only place it becomes a slice index.
fn redirection_index(index: u32) -> Option<(usize, u32)> {
    let offset = index.checked_sub(REG_REDIRECTION_BASE)?;
    let pin = usize::try_from(offset / 2).ok()?;
    (pin < REDIRECTION_ENTRIES).then_some((pin, offset % 2))
}

/// Turns a redirection entry into the interrupt message it describes, or `None`
/// for a delivery mode this machine drops.
fn decode(entry: u64) -> Option<InterruptRequest> {
    let kind = match (entry >> RTE_DELIVERY_MODE_SHIFT) & 0b111 {
        DELIVERY_FIXED => InterruptKind::Fixed,
        DELIVERY_LOWEST_PRIORITY => InterruptKind::LowestPriority,
        DELIVERY_NMI => InterruptKind::Nmi,
        // SMI (0b010), INIT (0b101) and ExtINT (0b111) are not deliverable
        // through `InterruptDelivery`. ExtINT is the one a guest really does
        // program — on pin 0, for 8259 virtual-wire mode — and dropping it is
        // correct for this machine: nothing is wired to the 8259's INTR
        // (`crate::irqchip::pic`), so there is no ExtINT to vector.
        other => {
            tracing::debug!(
                delivery_mode = other,
                "IOAPIC entry uses a delivery mode this machine does not route; dropped"
            );
            return None;
        }
    };
    Some(InterruptRequest {
        vector: (entry & RTE_VECTOR) as u8,
        destination: ((entry >> RTE_DESTINATION_SHIFT) & 0xff) as u32,
        kind,
        destination_mode: if entry & RTE_DESTINATION_MODE != 0 {
            DestinationMode::Logical
        } else {
            DestinationMode::Physical
        },
        trigger: if entry & RTE_TRIGGER_LEVEL != 0 {
            TriggerMode::Level
        } else {
            TriggerMode::Edge
        },
    })
}

/// One device's interrupt line: pin `pin` of the machine's IOAPIC.
///
/// The Windows counterpart of [`crate::irqfd::IrqFdLine`], and the reason the
/// serial console and both virtio transports need no per-host code: they hold an
/// `Arc<dyn IrqLine>` and never learn which one they got.
pub struct IoApicLine {
    ioapic: Arc<IoApic>,
    pin: u8,
}

impl IoApicLine {
    pub fn pin(&self) -> u8 {
        self.pin
    }
}

impl IrqLine for IoApicLine {
    fn trigger(&self) -> Result<(), InterruptError> {
        self.ioapic.pulse(self.pin)
    }
}

#[derive(Debug, thiserror::Error)]
pub enum IoApicError {
    #[error("IOAPIC pin {0} does not exist (only {REDIRECTION_ENTRIES} pins)")]
    NoSuchPin(u8),
}

#[cfg(test)]
mod tests {
    use super::*;
    use vmm_core::hv::HvError;

    #[derive(Default)]
    struct Recorder {
        requests: Mutex<Vec<InterruptRequest>>,
        fail: bool,
    }

    impl InterruptDelivery for Recorder {
        fn request(&self, interrupt: &InterruptRequest) -> Result<(), HvError> {
            if self.fail {
                return Err(HvError::Interrupt("no".into()));
            }
            self.requests.lock().unwrap().push(*interrupt);
            Ok(())
        }
    }

    fn chip() -> (Arc<IoApic>, Arc<Recorder>) {
        let rec = Arc::new(Recorder::default());
        (IoApic::new(rec.clone(), 1), rec)
    }

    fn base() -> u64 {
        u64::from(layout::IOAPIC_ADDR)
    }

    fn write_reg(apic: &IoApic, index: u32, value: u32) {
        apic.mmio_write(base() + IOREGSEL, &index.to_le_bytes());
        apic.mmio_write(base() + IOWIN, &value.to_le_bytes());
    }

    fn read_reg(apic: &IoApic, index: u32) -> u32 {
        apic.mmio_write(base() + IOREGSEL, &index.to_le_bytes());
        let mut data = [0u8; 4];
        apic.mmio_read(base() + IOWIN, &mut data);
        u32::from_le_bytes(data)
    }

    /// Program pin `pin` as an unmasked edge-triggered fixed interrupt on
    /// `vector`, destination APIC id 0 — what Linux writes for an ISA IRQ.
    fn route(apic: &IoApic, pin: u32, vector: u8) {
        write_reg(apic, REG_REDIRECTION_BASE + 2 * pin, u32::from(vector));
        write_reg(apic, REG_REDIRECTION_BASE + 2 * pin + 1, 0);
    }

    /// The identity registers are what a guest uses to decide the IOAPIC exists
    /// and how many pins it has; both must match the published tables.
    #[test]
    fn identity_registers_match_the_published_topology() {
        let (apic, _) = chip();
        assert_eq!(read_reg(&apic, REG_ID) >> 24, 1);
        let version = read_reg(&apic, REG_VERSION);
        assert_eq!(version & 0xff, IOAPIC_VERSION);
        assert_eq!(
            (version >> 16) & 0xff,
            REDIRECTION_ENTRIES as u32 - 1,
            "max redirection entry must be pins - 1"
        );
    }

    #[test]
    fn every_pin_starts_masked() {
        let (apic, rec) = chip();
        for pin in 0..REDIRECTION_ENTRIES as u32 {
            assert_ne!(
                read_reg(&apic, REG_REDIRECTION_BASE + 2 * pin) & RTE_MASK as u32,
                0,
                "pin {pin} must reset masked"
            );
        }
        assert!(apic.pulse(4).is_ok());
        assert!(rec.requests.lock().unwrap().is_empty());
    }

    #[test]
    fn a_routed_pin_delivers_its_vector_to_its_destination() {
        let (apic, rec) = chip();
        route(&apic, 4, 0x31);
        // Destination APIC id 3, logical mode, level triggered.
        write_reg(
            &apic,
            REG_REDIRECTION_BASE + 2 * 4,
            0x31 | RTE_DESTINATION_MODE as u32 | RTE_TRIGGER_LEVEL as u32,
        );
        write_reg(&apic, REG_REDIRECTION_BASE + 2 * 4 + 1, 3 << 24);

        apic.pulse(4).unwrap();
        let requests = rec.requests.lock().unwrap();
        assert_eq!(requests.len(), 1);
        assert_eq!(
            requests[0],
            InterruptRequest {
                vector: 0x31,
                destination: 3,
                kind: InterruptKind::Fixed,
                destination_mode: DestinationMode::Logical,
                trigger: TriggerMode::Level,
            }
        );
        assert_eq!(apic.delivered(), 1);
    }

    /// The documented deviation from hardware: an edge that arrives masked is
    /// remembered and delivered on unmask, once.
    #[test]
    fn an_edge_arriving_masked_is_delivered_on_unmask() {
        let (apic, rec) = chip();
        // Program the vector but leave the pin masked.
        write_reg(&apic, REG_REDIRECTION_BASE + 2 * 2, 0x30 | RTE_MASK as u32);
        apic.pulse(2).unwrap();
        assert!(rec.requests.lock().unwrap().is_empty());

        route(&apic, 2, 0x30); // unmask
        assert_eq!(rec.requests.lock().unwrap().len(), 1);
        assert_eq!(rec.requests.lock().unwrap()[0].vector, 0x30);

        // Exactly one: the latch holds a single edge per pin, so a second
        // mask/unmask cycle with no new edge delivers nothing.
        write_reg(&apic, REG_REDIRECTION_BASE + 2 * 2, 0x30 | RTE_MASK as u32);
        route(&apic, 2, 0x30);
        assert_eq!(rec.requests.lock().unwrap().len(), 1);
    }

    #[test]
    fn nmi_delivery_mode_is_routed_and_smi_is_dropped() {
        let (apic, rec) = chip();
        write_reg(
            &apic,
            REG_REDIRECTION_BASE + 2,
            ((DELIVERY_NMI as u32) << RTE_DELIVERY_MODE_SHIFT) | 2,
        );
        apic.pulse(1).unwrap();
        assert_eq!(rec.requests.lock().unwrap()[0].kind, InterruptKind::Nmi);

        // ExtINT (0b111) is what a guest programs on pin 0 for virtual-wire
        // mode; it must be dropped, not mis-delivered as a fixed vector.
        write_reg(
            &apic,
            REG_REDIRECTION_BASE,
            0b111 << RTE_DELIVERY_MODE_SHIFT,
        );
        apic.pulse(0).unwrap();
        assert_eq!(rec.requests.lock().unwrap().len(), 1);
    }

    /// Delivery status (12) and remote IRR (14) are read-only: a guest that
    /// writes them must not see them stored, or Linux's `mask/unmask` read-modify
    /// -write would eventually claim an interrupt is permanently in flight.
    #[test]
    fn read_only_entry_bits_never_latch() {
        let (apic, _) = chip();
        write_reg(&apic, REG_REDIRECTION_BASE + 2 * 5, 0xffff_ffff);
        let back = read_reg(&apic, REG_REDIRECTION_BASE + 2 * 5);
        assert_eq!(back & (1 << 12), 0, "delivery status must read 0");
        assert_eq!(back & (1 << 14), 0, "remote IRR must read 0");
    }

    /// Guest-controlled `IOREGSEL` values must never index the table: an
    /// out-of-range register reads zero and swallows writes.
    #[test]
    fn out_of_range_register_indices_are_inert() {
        let (apic, _) = chip();
        for index in [
            0x03,
            0x0f,
            REG_REDIRECTION_BASE + 2 * REDIRECTION_ENTRIES as u32,
            0xffff_ffff,
        ] {
            assert_eq!(read_reg(&apic, index), 0, "index {index:#x} must read 0");
            write_reg(&apic, index, 0xdead_beef);
        }
        // The table is untouched: every pin still masked at reset value.
        assert_eq!(
            read_reg(&apic, REG_REDIRECTION_BASE),
            RTE_RESET as u32,
            "a stray write must not have reached the table"
        );
    }

    #[test]
    fn non_dword_accesses_are_ignored() {
        let (apic, _) = chip();
        route(&apic, 4, 0x31);
        let mut byte = [0u8; 1];
        apic.mmio_read(base() + IOWIN, &mut byte);
        assert_eq!(byte, [0]);
        apic.mmio_write(base() + IOWIN, &[0xff]);
        assert_eq!(read_reg(&apic, REG_REDIRECTION_BASE + 8), 0x31);
    }

    #[test]
    fn window_decoding_is_exact() {
        let base = u64::from(layout::IOAPIC_ADDR);
        assert!(IoApic::contains(base));
        assert!(IoApic::contains(base + WINDOW_SIZE - 1));
        assert!(!IoApic::contains(base - 1));
        assert!(!IoApic::contains(base + WINDOW_SIZE));
        // The window must not collide with the local APIC page.
        assert!(base + WINDOW_SIZE <= u64::from(layout::LAPIC_ADDR));
    }

    #[test]
    fn a_line_is_a_pin_and_out_of_range_pins_are_refused() {
        let (apic, rec) = chip();
        route(&apic, 7, 0x39);
        let line = apic.line(7).unwrap();
        assert_eq!(line.pin(), 7);
        line.trigger().unwrap();
        assert_eq!(rec.requests.lock().unwrap()[0].vector, 0x39);
        assert!(matches!(
            apic.line(REDIRECTION_ENTRIES as u8),
            Err(IoApicError::NoSuchPin(_))
        ));
    }

    /// An injection failure must reach the device as an error rather than being
    /// silently counted as delivered.
    #[test]
    fn injection_failure_propagates() {
        let rec = Arc::new(Recorder {
            requests: Mutex::new(Vec::new()),
            fail: true,
        });
        let apic = IoApic::new(rec, 1);
        route(&apic, 4, 0x31);
        assert!(apic.pulse(4).is_err());
        assert_eq!(apic.delivered(), 0);
    }
}
