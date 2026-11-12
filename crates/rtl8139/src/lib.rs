//! A Realtek RTL8139 network card (ADR-0012).
//!
//! Windows XP has no virtio-net driver, but it has had one for this card in
//! the box since before it shipped, and so has everything else of its era.
//! The card is the simple one of the family: four transmit descriptors that
//! each name a buffer in guest memory, and one receive ring the device
//! copies whole frames into, each behind a four-byte header.
//!
//! The host side is a [`virtio_net::NetBackend`], the same wire virtio-net
//! uses: the user-mode NAT or a TAP interface.
//!
//! # Threads
//!
//! Transmission happens on the vCPU thread that wrote the descriptor.
//! Reception has a worker thread, which blocks in the backend and, for each
//! frame, takes the VM's pause gate (`virtio_core::Quiesce`, ADR-0005)
//! before it takes the device lock and writes the ring — a paused VM's
//! memory is not written.
//!
//! # The guest is untrusted
//!
//! The ring address, the buffer addresses and every length are the guest's.
//! All guest memory goes through `vm-memory`'s checked accessors; a frame
//! that does not fit the ring is dropped and counted (`RXOVW`), a transmit
//! length beyond a frame is refused, and nothing here indexes host memory
//! with a guest value.
//!
//! Not modelled: the C+ descriptor mode, wake-on-LAN, flow control, early
//! receive and transmit, and the statistics counters beyond "missed".

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread::JoinHandle;
use std::time::Duration;

use virtio_core::interrupt::IrqLine;
use virtio_core::{GuestMem, Quiesce};
use virtio_net::{MacAddr, NetBackend, Readiness};
use vm_memory::{Bytes, GuestAddress};

/// PCI identity: Realtek, RTL8139, an Ethernet controller, revision 0x10
/// (the 8139C).
pub const VENDOR_ID: u16 = 0x10ec;
pub const DEVICE_ID: u16 = 0x8139;
pub const CLASS_ETHERNET: u32 = 0x0200_0000;
pub const REVISION: u8 = 0x10;
/// Size of the register file, in I/O space and in memory space alike.
pub const REGISTERS: u16 = 0x100;

mod reg {
    pub const IDR0: u16 = 0x00;
    pub const MAR0: u16 = 0x08;
    pub const TSD0: u16 = 0x10;
    pub const TSAD0: u16 = 0x20;
    pub const RBSTART: u16 = 0x30;
    pub const CR: u16 = 0x37;
    pub const CAPR: u16 = 0x38;
    pub const CBR: u16 = 0x3a;
    pub const IMR: u16 = 0x3c;
    pub const ISR: u16 = 0x3e;
    pub const TCR: u16 = 0x40;
    pub const RCR: u16 = 0x44;
    pub const MPC: u16 = 0x4c;
    pub const CR9346: u16 = 0x50;
    pub const CONFIG1: u16 = 0x52;
    pub const MSR: u16 = 0x58;
    pub const TSAD: u16 = 0x60;
    pub const BMCR: u16 = 0x62;
    pub const BMSR: u16 = 0x64;
    pub const ANAR: u16 = 0x66;
    pub const ANLPAR: u16 = 0x68;
}

const CR_BUFFER_EMPTY: u8 = 1 << 0;
const CR_TX_ENABLE: u8 = 1 << 2;
const CR_RX_ENABLE: u8 = 1 << 3;
const CR_RESET: u8 = 1 << 4;

const INT_RX_OK: u16 = 1 << 0;
const INT_TX_OK: u16 = 1 << 2;
const INT_RX_OVERFLOW: u16 = 1 << 4;

const TSD_OWN: u32 = 1 << 13;
const TSD_TX_OK: u32 = 1 << 15;
const TSD_SIZE: u32 = 0x1fff;

const RCR_ALL_PHYSICAL: u32 = 1 << 0;
const RCR_PHYSICAL_MATCH: u32 = 1 << 1;
const RCR_MULTICAST: u32 = 1 << 2;
const RCR_BROADCAST: u32 = 1 << 3;
const RCR_WRAP: u32 = 1 << 7;

/// Receive header status bits.
const RX_OK: u16 = 1 << 0;
const RX_BROADCAST: u16 = 1 << 13;
const RX_PHYSICAL: u16 = 1 << 14;
const RX_MULTICAST: u16 = 1 << 15;

/// The hardware version the transmit configuration register reports: 8139C.
const TCR_VERSION: u32 = 0x7400_0000;
/// The longest frame the card sends or takes, without its checksum.
const MAX_FRAME: usize = 1792;
/// How long the receive worker blocks before it looks at the stop flag.
const RX_POLL: Duration = Duration::from_millis(100);

/// The 93C46 serial EEPROM behind the 9346CR register: 64 words, read by
/// clocking a start bit, a two-bit opcode and a six-bit address in, and
/// sixteen data bits out. Drivers read the MAC address from it.
#[derive(Debug, Clone)]
struct Eeprom {
    words: [u16; 64],
    /// The last value of the clock line.
    clock: bool,
    /// Bits shifted in since chip select went high.
    shift_in: u32,
    bits_in: u8,
    /// The word being shifted out, and how many of its bits are left.
    shift_out: u16,
    bits_out: u8,
    /// The data-out line.
    output: bool,
}

impl Eeprom {
    fn new(mac: [u8; 6]) -> Self {
        let mut words = [0u16; 64];
        words[0] = 0x8129;
        words[1] = VENDOR_ID;
        words[2] = DEVICE_ID;
        words[3] = VENDOR_ID;
        words[4] = DEVICE_ID;
        for (word, pair) in words[7..10].iter_mut().zip(mac.chunks_exact(2)) {
            *word = u16::from_le_bytes([pair[0], pair[1]]);
        }
        Self {
            words,
            clock: false,
            shift_in: 0,
            bits_in: 0,
            shift_out: 0,
            bits_out: 0,
            output: false,
        }
    }

    /// The chip select, clock and data-in lines changed.
    fn lines(&mut self, select: bool, clock: bool, data_in: bool) {
        if !select {
            self.bits_in = 0;
            self.shift_in = 0;
            self.bits_out = 0;
            self.output = false;
            self.clock = clock;
            return;
        }
        let rising = clock && !self.clock;
        self.clock = clock;
        if !rising {
            return;
        }
        if self.bits_out > 0 {
            self.bits_out -= 1;
            self.output = self.shift_out >> self.bits_out & 1 != 0;
            return;
        }
        // Nothing counts before the start bit.
        if self.bits_in == 0 && !data_in {
            return;
        }
        self.shift_in = self.shift_in << 1 | u32::from(data_in);
        self.bits_in += 1;
        // Start bit, two opcode bits, six address bits.
        if self.bits_in == 9 {
            let opcode = self.shift_in >> 6 & 3;
            let address = (self.shift_in & 0x3f) as usize;
            if opcode == 0b10 {
                self.shift_out = self.words[address];
                self.bits_out = 16;
            }
            // A dummy zero precedes the data.
            self.output = false;
            self.bits_in = 0;
            self.shift_in = 0;
        }
    }
}

/// The registers.
struct State {
    mac: [u8; 6],
    multicast: [u8; 8],
    tsd: [u32; 4],
    tsad: [u32; 4],
    rx_start: u32,
    command: u8,
    /// Where the driver has read up to, minus sixteen (as the register holds
    /// it), and where the device writes next: offsets into the ring.
    capr: u16,
    cbr: u16,
    imr: u16,
    isr: u16,
    tcr: u32,
    rcr: u32,
    missed: u32,
    cr9346: u8,
    config1: u8,
    bmcr: u16,
    eeprom: Eeprom,
}

impl State {
    fn new(mac: [u8; 6]) -> Self {
        Self {
            mac,
            multicast: [0; 8],
            tsd: [TSD_OWN; 4],
            tsad: [0; 4],
            rx_start: 0,
            command: 0,
            capr: 0xfff0,
            cbr: 0,
            imr: 0,
            isr: 0,
            tcr: 0,
            rcr: 0,
            missed: 0,
            cr9346: 0,
            // I/O and memory mapping of the registers, both enabled.
            config1: 0x0c,
            // Auto-negotiation on, 100 Mb/s, full duplex.
            bmcr: 0x3100,
            eeprom: Eeprom::new(mac),
        }
    }

    /// The receive ring's size: 8, 16, 32 or 64 KiB by the RBLEN field.
    fn ring_size(&self) -> u32 {
        8192 << (self.rcr >> 11 & 3)
    }

    /// Whether the driver has read everything the device wrote.
    fn ring_empty(&self) -> bool {
        u32::from(self.capr.wrapping_add(16)) % self.ring_size() == u32::from(self.cbr)
    }

    /// The receive status of a frame the filter accepts, or `None`.
    fn accepts(&self, frame: &[u8]) -> Option<u16> {
        let destination = frame.get(..6)?;
        if destination == [0xff; 6] {
            return (self.rcr & RCR_BROADCAST != 0).then_some(RX_OK | RX_BROADCAST);
        }
        if destination[0] & 1 != 0 {
            // The multicast hash filter is not modelled bit by bit: a
            // driver that set any bit of it gets every multicast frame.
            let any = self.multicast.iter().any(|&b| b != 0);
            return (self.rcr & RCR_MULTICAST != 0 && any).then_some(RX_OK | RX_MULTICAST);
        }
        if destination == self.mac && self.rcr & RCR_PHYSICAL_MATCH != 0 {
            return Some(RX_OK | RX_PHYSICAL);
        }
        (self.rcr & RCR_ALL_PHYSICAL != 0).then_some(RX_OK)
    }
}

struct Shared {
    state: Mutex<State>,
    mem: Arc<GuestMem>,
    backend: Arc<dyn NetBackend>,
    irq: Arc<dyn IrqLine>,
    quiesce: Mutex<Option<Arc<Quiesce>>>,
    stop: AtomicBool,
}

impl Shared {
    fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Raises the interrupt when an enabled cause is pending. The line is
    /// level-triggered on the card; here every moment the level could have
    /// risen sends one edge, which is what the interrupt controllers of
    /// this machine deliver.
    fn update_irq(&self, state: &State) {
        if state.isr & state.imr != 0 {
            if let Err(error) = self.irq.trigger() {
                tracing::warn!(%error, "cannot raise the RTL8139 interrupt");
            }
        }
    }

    /// Sends the frame transmit descriptor `n` names.
    fn transmit(&self, state: &mut State, n: usize) {
        let length = (state.tsd[n] & TSD_SIZE) as usize;
        if state.command & CR_TX_ENABLE != 0 && (14..=MAX_FRAME).contains(&length) {
            let mut frame = vec![0u8; length];
            let read = self
                .mem
                .read_slice(&mut frame, GuestAddress(u64::from(state.tsad[n])));
            match read {
                Ok(()) => {
                    if let Err(error) = self.backend.write_frame(&frame) {
                        tracing::warn!(%error, "RTL8139 transmit failed");
                    }
                }
                Err(_) => tracing::debug!(
                    buffer = format_args!("{:#x}", state.tsad[n]),
                    "RTL8139 transmit buffer is not guest memory"
                ),
            }
        }
        // The descriptor is the host's again and the frame is "sent",
        // whatever became of it: a driver waits for exactly these bits.
        state.tsd[n] |= TSD_OWN | TSD_TX_OK;
        state.isr |= INT_TX_OK;
    }

    /// Copies one received frame into the ring. Returns whether it went in.
    fn receive(&self, state: &mut State, frame: &[u8]) -> bool {
        if state.command & CR_RX_ENABLE == 0 || frame.len() > MAX_FRAME {
            return false;
        }
        let Some(status) = state.accepts(frame) else {
            return false;
        };
        let size = state.ring_size();
        // A header, the frame padded to the minimum, its checksum, and the
        // whole rounded up to four.
        let padded = frame.len().max(60);
        let stored = (4 + padded + 4 + 3) & !3;
        let read = u32::from(state.capr.wrapping_add(16)) % size;
        let write = u32::from(state.cbr) % size;
        let free = (read + size - write - 1) % size;
        if stored as u32 > free {
            state.isr |= INT_RX_OVERFLOW;
            state.missed = state.missed.wrapping_add(1);
            return false;
        }
        let mut packet = vec![0u8; stored];
        packet[0..2].copy_from_slice(&status.to_le_bytes());
        packet[2..4].copy_from_slice(&((padded + 4) as u16).to_le_bytes());
        packet[4..4 + frame.len()].copy_from_slice(frame);
        let base = u64::from(state.rx_start);
        let wrote = if state.rcr & RCR_WRAP != 0 || write as usize + stored <= size as usize {
            // In one piece: either it fits, or the driver set WRAP and gave
            // the ring room past its end for exactly this.
            self.mem
                .write_slice(&packet, GuestAddress(base + u64::from(write)))
                .is_ok()
        } else {
            let first = (size - write) as usize;
            self.mem
                .write_slice(&packet[..first], GuestAddress(base + u64::from(write)))
                .is_ok()
                && self
                    .mem
                    .write_slice(&packet[first..], GuestAddress(base))
                    .is_ok()
        };
        if !wrote {
            return false;
        }
        state.cbr = ((write + stored as u32) % size) as u16;
        state.isr |= INT_RX_OK;
        true
    }

    fn read(&self, state: &mut State, offset: u16, size: usize) -> u32 {
        let value = match offset {
            reg::TSD0..=0x1f => state.tsd[usize::from((offset - reg::TSD0) / 4)],
            reg::TSAD0..=0x2f => state.tsad[usize::from((offset - reg::TSAD0) / 4)],
            reg::RBSTART => state.rx_start,
            reg::CR => {
                let empty = if state.ring_empty() {
                    CR_BUFFER_EMPTY
                } else {
                    0
                };
                u32::from(state.command | empty)
            }
            reg::CAPR => u32::from(state.capr),
            reg::CBR => u32::from(state.cbr),
            reg::IMR => u32::from(state.imr),
            reg::ISR => u32::from(state.isr),
            reg::TCR => state.tcr | TCR_VERSION,
            reg::RCR => state.rcr,
            reg::MPC => state.missed,
            reg::CR9346 => u32::from(state.cr9346 & !1 | u8::from(state.eeprom.output)),
            reg::CONFIG1 => u32::from(state.config1),
            // Media status: link up, 100 Mb/s.
            reg::MSR => 0,
            // The four descriptors' OWN bits low, their TOK bits high.
            reg::TSAD => state.tsd.iter().enumerate().fold(0, |all, (n, tsd)| {
                all | u32::from(tsd & TSD_OWN != 0) << n
                    | u32::from(tsd & TSD_TX_OK != 0) << (12 + n)
            }),
            reg::BMCR => u32::from(state.bmcr),
            // Link up, auto-negotiation complete, every speed supported.
            reg::BMSR => 0x782d,
            reg::ANAR => 0x01e1,
            reg::ANLPAR => 0x45e1,
            reg::IDR0..=0x05 => {
                // The MAC a byte at a time, or as the dword and the word a
                // driver reads it in.
                let at = usize::from(offset);
                let mut bytes = [0u8; 4];
                for (i, byte) in bytes.iter_mut().enumerate().take(size.min(4)) {
                    *byte = state.mac.get(at + i).copied().unwrap_or(0);
                }
                u32::from_le_bytes(bytes)
            }
            reg::MAR0..=0x0f => {
                let at = usize::from(offset - reg::MAR0);
                let mut bytes = [0u8; 4];
                for (i, byte) in bytes.iter_mut().enumerate().take(size.min(4)) {
                    *byte = state.multicast.get(at + i).copied().unwrap_or(0);
                }
                u32::from_le_bytes(bytes)
            }
            _ => 0,
        };
        match size {
            1 => value & 0xff,
            2 => value & 0xffff,
            _ => value,
        }
    }

    fn write(&self, state: &mut State, offset: u16, size: usize, value: u32) {
        match offset {
            reg::TSD0..=0x1f => {
                let n = usize::from((offset - reg::TSD0) / 4);
                // Writing the size with OWN clear hands the descriptor over.
                state.tsd[n] = value & !(TSD_OWN | TSD_TX_OK);
                self.transmit(state, n);
            }
            reg::TSAD0..=0x2f => state.tsad[usize::from((offset - reg::TSAD0) / 4)] = value,
            reg::RBSTART => state.rx_start = value,
            reg::CR => {
                let value = value as u8;
                if value & CR_RESET != 0 {
                    // Reset keeps the MAC and clears itself.
                    *state = State::new(state.mac);
                    return;
                }
                state.command = value & (CR_TX_ENABLE | CR_RX_ENABLE);
                if value & CR_RX_ENABLE != 0 {
                    // The ring starts over when reception is enabled.
                    state.cbr = 0;
                    state.capr = 0xfff0;
                }
            }
            reg::CAPR => state.capr = value as u16,
            reg::IMR => state.imr = value as u16,
            // Write one to clear.
            reg::ISR => state.isr &= !(value as u16),
            reg::TCR => state.tcr = value & !TCR_VERSION,
            reg::RCR => state.rcr = value,
            reg::MPC => state.missed = 0,
            reg::CR9346 => {
                let value = value as u8;
                state.cr9346 = value;
                // EEM = 10: the lines below are the EEPROM's.
                if value >> 6 == 0b10 {
                    state
                        .eeprom
                        .lines(value & 0x08 != 0, value & 0x04 != 0, value & 0x02 != 0);
                } else {
                    state.eeprom.lines(false, false, false);
                }
            }
            reg::CONFIG1 => state.config1 = value as u8,
            // Bit 15 resets the PHY and clears itself; bit 9 restarts
            // negotiation, which is always already done.
            reg::BMCR => state.bmcr = value as u16 & !0x8200,
            reg::IDR0..=0x05 => {
                let at = usize::from(offset);
                for (i, byte) in value.to_le_bytes().iter().enumerate().take(size.min(4)) {
                    if let Some(slot) = state.mac.get_mut(at + i) {
                        *slot = *byte;
                    }
                }
            }
            reg::MAR0..=0x0f => {
                let at = usize::from(offset - reg::MAR0);
                for (i, byte) in value.to_le_bytes().iter().enumerate().take(size.min(4)) {
                    if let Some(slot) = state.multicast.get_mut(at + i) {
                        *slot = *byte;
                    }
                }
            }
            _ => {}
        }
    }
}

/// The card. Dropping it stops the receive worker.
pub struct Rtl8139 {
    shared: Arc<Shared>,
    worker: Mutex<Option<JoinHandle<()>>>,
}

impl Rtl8139 {
    /// A card with address `mac` on `backend`, raising `irq`, over guest
    /// memory `mem`. The receive worker starts with [`Self::start`].
    pub fn new(
        mem: Arc<GuestMem>,
        backend: Arc<dyn NetBackend>,
        mac: MacAddr,
        irq: Arc<dyn IrqLine>,
    ) -> Self {
        backend.set_guest_mac(mac);
        Self {
            shared: Arc::new(Shared {
                state: Mutex::new(State::new(mac.0)),
                mem,
                backend,
                irq,
                quiesce: Mutex::new(None),
                stop: AtomicBool::new(false),
            }),
            worker: Mutex::new(None),
        }
    }

    /// The VM's pause gate, which the receive worker waits at before it
    /// writes guest memory (ADR-0005). Set before the guest runs.
    pub fn set_quiesce(&self, quiesce: Arc<Quiesce>) {
        *self
            .shared
            .quiesce
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some(quiesce);
    }

    /// Starts the receive worker. Idempotent.
    pub fn start(&self) -> std::io::Result<()> {
        let mut worker = self.worker.lock().unwrap_or_else(PoisonError::into_inner);
        if worker.is_some() {
            return Ok(());
        }
        let shared = Arc::clone(&self.shared);
        *worker = Some(
            std::thread::Builder::new()
                .name("rtl8139-rx".into())
                .spawn(move || receive_loop(&shared))?,
        );
        Ok(())
    }

    /// A guest read of `data.len()` bytes at register `offset`.
    pub fn read(&self, offset: u16, data: &mut [u8]) {
        let mut state = self.shared.state();
        let value = self.shared.read(&mut state, offset, data.len());
        for (out, byte) in data.iter_mut().zip(value.to_le_bytes()) {
            *out = byte;
        }
    }

    /// A guest write of `data.len()` bytes at register `offset`.
    pub fn write(&self, offset: u16, data: &[u8]) {
        let mut bytes = [0u8; 4];
        for (slot, byte) in bytes.iter_mut().zip(data) {
            *slot = *byte;
        }
        let mut state = self.shared.state();
        self.shared
            .write(&mut state, offset, data.len(), u32::from_le_bytes(bytes));
        self.shared.update_irq(&state);
    }

    /// The guest finished handling the card's interrupt (its EOI). The line
    /// is level-triggered: with a cause still pending and enabled it is
    /// raised again, as a card holding the line low would be seen again.
    pub fn interrupt_acknowledged(&self) {
        let state = self.shared.state();
        self.shared.update_irq(&state);
    }

    /// Power-on (ADR-0005): the registers of a card that was just reset.
    pub fn reset(&self) {
        let mut state = self.shared.state();
        let mac = Eeprom::mac_of(&state.eeprom);
        *state = State::new(mac);
    }
}

impl Eeprom {
    /// The address the EEPROM holds: what a reset restores, whatever the
    /// guest wrote to the ID registers.
    fn mac_of(eeprom: &Eeprom) -> [u8; 6] {
        let mut mac = [0u8; 6];
        for (pair, word) in mac.chunks_exact_mut(2).zip(&eeprom.words[7..10]) {
            pair.copy_from_slice(&word.to_le_bytes());
        }
        mac
    }
}

impl Drop for Rtl8139 {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::Release);
        let _ = self.shared.backend.wake();
        let quiesce = self
            .shared
            .quiesce
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        if let Some(quiesce) = quiesce {
            quiesce.wake();
        }
        let worker = self
            .worker
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        if let Some(worker) = worker {
            if worker.join().is_err() {
                tracing::error!("the RTL8139 receive worker panicked");
            }
        }
    }
}

fn receive_loop(shared: &Shared) {
    let keep_going = || !shared.stop.load(Ordering::Acquire);
    // One byte over the cap, so an oversized frame is seen as one.
    let mut frame = vec![0u8; MAX_FRAME + 1];
    while keep_going() {
        match shared.backend.wait_readable(RX_POLL) {
            Ok(Readiness::Readable) => {}
            Ok(Readiness::TimedOut | Readiness::WokenUp) => continue,
            Err(error) => {
                tracing::warn!(%error, "RTL8139 backend wait failed");
                std::thread::sleep(RX_POLL);
                continue;
            }
        }
        // The pause gate before the device lock, and never under it.
        let quiesce = shared
            .quiesce
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        let _pass = match &quiesce {
            Some(quiesce) => match quiesce.wait_while_paused(keep_going) {
                Some(pass) => Some(pass),
                None => break,
            },
            None => None,
        };
        while let Ok(Some(length)) = shared.backend.read_frame(&mut frame) {
            let mut state = shared.state();
            if shared.receive(&mut state, &frame[..length.min(frame.len())]) {
                shared.update_irq(&state);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::sync::atomic::AtomicU32;
    use virtio_core::interrupt::InterruptError;
    use virtio_net::NetError;
    use vm_memory::GuestMemoryMmap;

    #[derive(Default)]
    struct Wire {
        sent: Mutex<Vec<Vec<u8>>>,
        incoming: Mutex<VecDeque<Vec<u8>>>,
    }

    impl NetBackend for Wire {
        fn name(&self) -> &str {
            "test"
        }
        fn write_frame(&self, frame: &[u8]) -> Result<usize, NetError> {
            self.sent.lock().unwrap().push(frame.to_vec());
            Ok(frame.len())
        }
        fn read_frame(&self, buf: &mut [u8]) -> Result<Option<usize>, NetError> {
            Ok(self.incoming.lock().unwrap().pop_front().map(|frame| {
                let n = frame.len().min(buf.len());
                buf[..n].copy_from_slice(&frame[..n]);
                n
            }))
        }
        fn wait_readable(&self, timeout: Duration) -> Result<Readiness, NetError> {
            if self.incoming.lock().unwrap().is_empty() {
                std::thread::sleep(timeout.min(Duration::from_millis(5)));
                Ok(Readiness::TimedOut)
            } else {
                Ok(Readiness::Readable)
            }
        }
    }

    #[derive(Default)]
    struct Count(AtomicU32);
    impl IrqLine for Count {
        fn trigger(&self) -> Result<(), InterruptError> {
            self.0.fetch_add(1, Ordering::Relaxed);
            Ok(())
        }
    }

    const MAC: [u8; 6] = [0x52, 0x54, 0x00, 0x12, 0x34, 0x56];
    const RING: u64 = 0x10000;
    const TX_BUFFER: u64 = 0x40000;

    struct Rig {
        card: Rtl8139,
        wire: Arc<Wire>,
        irq: Arc<Count>,
        mem: Arc<GuestMem>,
    }

    fn rig() -> Rig {
        let mem = Arc::new(
            GuestMemoryMmap::<()>::from_ranges(&[(GuestAddress(0), 1 << 20)]).expect("memory"),
        );
        let wire = Arc::new(Wire::default());
        let irq = Arc::new(Count::default());
        let card = Rtl8139::new(Arc::clone(&mem), wire.clone(), MacAddr(MAC), irq.clone());
        Rig {
            card,
            wire,
            irq,
            mem,
        }
    }

    fn read(card: &Rtl8139, offset: u16, size: usize) -> u32 {
        let mut bytes = [0u8; 4];
        card.read(offset, &mut bytes[..size]);
        u32::from_le_bytes(bytes)
    }

    fn write(card: &Rtl8139, offset: u16, size: usize, value: u32) {
        card.write(offset, &value.to_le_bytes()[..size]);
    }

    /// What a driver does before it uses the card: the ring, both
    /// directions on, every frame for this address and broadcasts, and the
    /// receive and transmit interrupts.
    fn bring_up(rig: &Rig) {
        write(&rig.card, reg::RBSTART, 4, RING as u32);
        write(&rig.card, reg::RCR, 4, RCR_PHYSICAL_MATCH | RCR_BROADCAST);
        write(
            &rig.card,
            reg::CR,
            1,
            u32::from(CR_TX_ENABLE | CR_RX_ENABLE),
        );
        write(&rig.card, reg::IMR, 2, u32::from(INT_RX_OK | INT_TX_OK));
    }

    fn frame_to(destination: [u8; 6], length: usize) -> Vec<u8> {
        let mut frame = vec![0xabu8; length];
        frame[..6].copy_from_slice(&destination);
        frame
    }

    #[test]
    fn the_mac_is_in_the_id_registers_and_in_the_eeprom() {
        let rig = rig();
        assert_eq!(
            read(&rig.card, reg::IDR0, 4),
            u32::from_le_bytes([0x52, 0x54, 0x00, 0x12])
        );
        assert_eq!(
            read(&rig.card, 4, 2),
            u32::from(u16::from_le_bytes([0x34, 0x56]))
        );
        assert_eq!(read(&rig.card, reg::TCR, 4) & TCR_VERSION, TCR_VERSION);
        // The EEPROM, bit by bit: select, start + read + address 7, then
        // sixteen clocks.
        let clock = |card: &Rtl8139, data: bool| {
            let lines = 0x80 | 0x08 | u32::from(data) << 1;
            write(card, reg::CR9346, 1, lines);
            write(card, reg::CR9346, 1, lines | 0x04);
        };
        write(&rig.card, reg::CR9346, 1, 0x80);
        for bit in [true, true, false, false, false, false, true, true, true] {
            clock(&rig.card, bit);
        }
        let mut word = 0u16;
        for _ in 0..16 {
            clock(&rig.card, false);
            word = word << 1 | (read(&rig.card, reg::CR9346, 1) & 1) as u16;
        }
        assert_eq!(word, u16::from_le_bytes([0x52, 0x54]));
    }

    #[test]
    fn a_descriptor_write_sends_the_frame_and_reports_it() {
        let rig = rig();
        bring_up(&rig);
        let frame = frame_to([0xff; 6], 64);
        rig.mem
            .write_slice(&frame, GuestAddress(TX_BUFFER))
            .unwrap();
        write(&rig.card, reg::TSAD0, 4, TX_BUFFER as u32);
        write(&rig.card, reg::TSD0, 4, 64);
        assert_eq!(rig.wire.sent.lock().unwrap().as_slice(), &[frame]);
        assert_eq!(
            read(&rig.card, reg::TSD0, 4) & (TSD_OWN | TSD_TX_OK),
            TSD_OWN | TSD_TX_OK
        );
        assert_eq!(read(&rig.card, reg::ISR, 2) as u16 & INT_TX_OK, INT_TX_OK);
        assert_eq!(rig.irq.0.load(Ordering::Relaxed), 1);
        // Acknowledged, the cause is gone.
        write(&rig.card, reg::ISR, 2, u32::from(INT_TX_OK));
        assert_eq!(read(&rig.card, reg::ISR, 2), 0);
        // A buffer outside memory and a length beyond a frame send nothing.
        write(&rig.card, reg::TSAD0 + 4, 4, 0xfff0_0000);
        write(&rig.card, reg::TSD0 + 4, 4, 64);
        write(&rig.card, reg::TSD0 + 8, 4, 0x1fff);
        assert_eq!(rig.wire.sent.lock().unwrap().len(), 1);
    }

    #[test]
    fn a_received_frame_lands_in_the_ring_behind_its_header() {
        let rig = rig();
        bring_up(&rig);
        assert_eq!(
            read(&rig.card, reg::CR, 1) as u8 & CR_BUFFER_EMPTY,
            CR_BUFFER_EMPTY
        );
        let frame = frame_to(MAC, 100);
        {
            let mut state = rig.card.shared.state();
            assert!(rig.card.shared.receive(&mut state, &frame));
        }
        let mut header = [0u8; 4];
        rig.mem.read_slice(&mut header, GuestAddress(RING)).unwrap();
        assert_eq!(
            u16::from_le_bytes([header[0], header[1]]),
            RX_OK | RX_PHYSICAL
        );
        assert_eq!(u16::from_le_bytes([header[2], header[3]]), 104);
        let mut stored = vec![0u8; 100];
        rig.mem
            .read_slice(&mut stored, GuestAddress(RING + 4))
            .unwrap();
        assert_eq!(stored, frame);
        // Header + frame + checksum, rounded up to four.
        assert_eq!(read(&rig.card, reg::CBR, 2), 108);
        assert_eq!(read(&rig.card, reg::CR, 1) as u8 & CR_BUFFER_EMPTY, 0);
        // The driver reads it and moves CAPR: empty again.
        write(&rig.card, reg::CAPR, 2, 108 - 16);
        assert_eq!(
            read(&rig.card, reg::CR, 1) as u8 & CR_BUFFER_EMPTY,
            CR_BUFFER_EMPTY
        );
    }

    #[test]
    fn the_filter_and_a_full_ring_drop_frames() {
        let rig = rig();
        bring_up(&rig);
        let mut state = rig.card.shared.state();
        // Someone else's address, and multicast with no filter set.
        assert!(!rig
            .card
            .shared
            .receive(&mut state, &frame_to([2, 0, 0, 0, 0, 1], 64)));
        assert!(!rig
            .card
            .shared
            .receive(&mut state, &frame_to([1, 0, 0x5e, 0, 0, 1], 64)));
        assert!(rig
            .card
            .shared
            .receive(&mut state, &frame_to([0xff; 6], 64)));
        // Fill the 8 KiB ring without the driver reading: it overflows
        // rather than overwriting what is unread.
        let mut stored = 1;
        while rig.card.shared.receive(&mut state, &frame_to(MAC, 1500)) {
            stored += 1;
            assert!(stored < 16, "the ring never filled");
        }
        assert_eq!(state.isr & INT_RX_OVERFLOW, INT_RX_OVERFLOW);
        assert_eq!(state.missed, 1);
        // A ring the guest pointed outside memory takes nothing.
        state.rx_start = 0xfff0_0000;
        state.capr = state.cbr.wrapping_sub(16);
        assert!(!rig.card.shared.receive(&mut state, &frame_to(MAC, 64)));
    }

    #[test]
    fn the_worker_delivers_frames_and_stops_when_the_card_is_dropped() {
        let rig = rig();
        bring_up(&rig);
        rig.card.set_quiesce(Quiesce::new());
        rig.card.start().expect("worker");
        rig.wire
            .incoming
            .lock()
            .unwrap()
            .push_back(frame_to(MAC, 80));
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while read(&rig.card, reg::ISR, 2) as u16 & INT_RX_OK == 0 {
            assert!(std::time::Instant::now() < deadline, "no frame arrived");
            std::thread::sleep(Duration::from_millis(2));
        }
        assert!(rig.irq.0.load(Ordering::Relaxed) >= 1);
        // A software reset keeps the address; dropping joins the worker.
        write(&rig.card, reg::CR, 1, u32::from(CR_RESET));
        assert_eq!(read(&rig.card, reg::IDR0, 1), 0x52);
        assert_eq!(read(&rig.card, reg::CR, 1) as u8 & CR_RESET, 0);
        drop(rig);
    }
}
