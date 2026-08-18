//! 16550 serial console at the classic COM1 port (backlog MVP-205/206).
//!
//! Own register-level emulation instead of `vm-superio`: the 8250 driver in
//! recent kernels (observed on 6.12) fills the 16-byte FIFO, enables the
//! THRI bit in IER and then *waits for the transmitter-empty interrupt that
//! a real 16550 raises immediately when THRI is enabled while THR is empty*.
//! vm-superio 0.8 only latches IER on that write, so guest TX stalls after
//! 16 bytes. Emulating the UART ourselves (like Firecracker and
//! Cloud Hypervisor ended up doing) fixes this by the spec.
//!
//! Emulated subset: everything Linux's 8250 driver touches — THR/RBR,
//! IER/IIR, LCR (incl. DLAB divisor latch), MCR, LSR, MSR, SCR. FCR writes
//! are accepted and ignored (we report FIFOs enabled via IIR).

use std::collections::VecDeque;
use std::io::Write;

use kvm_ioctls::VmFd;
use thiserror::Error;
use vmm_sys_util::eventfd::{EventFd, EFD_NONBLOCK};

/// COM1.
pub const SERIAL_PORT_BASE: u16 = 0x3f8;
pub const SERIAL_PORT_LAST: u16 = 0x3ff;
pub const SERIAL_IRQ: u32 = 4;

/// Bound on buffered guest input; excess input is dropped (a real UART
/// overruns too — the guest is untrusted and must not grow host memory).
const RX_CAPACITY: usize = 4096;

// Register offsets from the base port.
const THR_RBR_DLL: u16 = 0; // write: THR / read: RBR / DLAB: divisor low
const IER_DLH: u16 = 1; //     IER / DLAB: divisor high
const IIR_FCR: u16 = 2; //     read: IIR / write: FCR
const LCR: u16 = 3;
const MCR: u16 = 4;
const LSR: u16 = 5;
const MSR: u16 = 6;
const SCR: u16 = 7;

// IER bits.
const IER_RDA: u8 = 0x01; // received data available
const IER_THRE: u8 = 0x02; // transmitter holding register empty
const IER_VALID: u8 = 0x0f;

// IIR source values (priority order) and bits.
const IIR_NONE: u8 = 0x01;
const IIR_THRE: u8 = 0x02;
const IIR_RDA: u8 = 0x04;
const IIR_FIFOS_ENABLED: u8 = 0xc0;

// LCR bit.
const LCR_DLAB: u8 = 0x80;

// LSR bits.
const LSR_DR: u8 = 0x01; // data ready
const LSR_THRE: u8 = 0x20; // holding register empty
const LSR_TEMT: u8 = 0x40; // transmitter empty

// MSR bits: report CTS + DSR + DCD asserted.
const MSR_STATIC: u8 = 0xb0;

#[derive(Debug, Error)]
pub enum SerialError {
    #[error("failed to create serial interrupt eventfd: {0}")]
    EventFd(#[source] std::io::Error),

    #[error("failed to register serial irqfd: {0}")]
    Irqfd(#[source] kvm_ioctls::Error),
}

/// The guest-visible UART plus its host output sink.
pub struct SerialConsole {
    interrupt: EventFd,
    out: Box<dyn Write + Send>,
    rx: VecDeque<u8>,
    ier: u8,
    lcr: u8,
    mcr: u8,
    scr: u8,
    dll: u8,
    dlh: u8,
    /// Latched "THR became empty" interrupt condition (cleared by reading
    /// IIR while it is the reported source, or by writing THR).
    thre_pending: bool,
}

impl SerialConsole {
    /// Creates the UART and wires its interrupt line to the VM's IRQ 4.
    pub fn new(vm: &VmFd, out: Box<dyn Write + Send>) -> Result<Self, SerialError> {
        let interrupt = EventFd::new(EFD_NONBLOCK).map_err(SerialError::EventFd)?;
        vm.register_irqfd(&interrupt, SERIAL_IRQ)
            .map_err(SerialError::Irqfd)?;
        Ok(Self::with_trigger(interrupt, out))
    }

    /// Test constructor: no VM wiring, interrupts observable via the eventfd.
    pub fn with_trigger(interrupt: EventFd, out: Box<dyn Write + Send>) -> Self {
        Self {
            interrupt,
            out,
            rx: VecDeque::new(),
            ier: 0,
            lcr: 0,
            mcr: 0x08,
            scr: 0,
            dll: 0x0c, // 9600 baud, matching reset expectations
            dlh: 0,
            thre_pending: false,
        }
    }

    /// True when `port` belongs to this UART.
    pub fn contains(port: u16) -> bool {
        (SERIAL_PORT_BASE..=SERIAL_PORT_LAST).contains(&port)
    }

    /// Queues guest-bound input (host keyboard → guest console).
    pub fn push_input(&mut self, data: &[u8]) {
        for &b in data {
            if self.rx.len() >= RX_CAPACITY {
                break; // overrun: drop, never grow unbounded
            }
            self.rx.push_back(b);
        }
        self.update_interrupt();
    }

    fn dlab(&self) -> bool {
        self.lcr & LCR_DLAB != 0
    }

    /// Highest-priority pending interrupt source, per 16550 priority
    /// (received data above transmitter empty).
    fn current_source(&self) -> u8 {
        if self.ier & IER_RDA != 0 && !self.rx.is_empty() {
            IIR_RDA
        } else if self.ier & IER_THRE != 0 && self.thre_pending {
            IIR_THRE
        } else {
            IIR_NONE
        }
    }

    /// Pulses the irqfd when any enabled source is pending. Edge semantics
    /// are fine for the ISA IRQ: the guest re-reads IIR until it returns
    /// "none", and every state change re-arms the pulse here.
    fn update_interrupt(&mut self) {
        if self.current_source() != IIR_NONE {
            let _ = self.interrupt.write(1);
        }
    }

    /// Guest write to a UART register. Never fails toward the guest.
    pub fn io_write(&mut self, port: u16, value: u8) {
        match port - SERIAL_PORT_BASE {
            THR_RBR_DLL if self.dlab() => self.dll = value,
            THR_RBR_DLL => {
                let _ = self.out.write_all(&[value]);
                let _ = self.out.flush();
                // Transmission is instantaneous: THR is empty again — latch
                // the condition and (re-)raise the interrupt if enabled.
                self.thre_pending = true;
                self.update_interrupt();
            }
            IER_DLH if self.dlab() => self.dlh = value,
            IER_DLH => {
                let was_enabled = self.ier & IER_THRE != 0;
                self.ier = value & IER_VALID;
                // A real 16550 raises THRE immediately when the driver
                // enables THRI while the transmitter is idle. Linux 8250
                // relies on this to restart TX — the exact behavior
                // vm-superio lacked.
                if self.ier & IER_THRE != 0 && !was_enabled {
                    self.thre_pending = true;
                }
                self.update_interrupt();
            }
            IIR_FCR => {} // FCR: FIFO control accepted, nothing to model
            LCR => self.lcr = value,
            MCR => self.mcr = value,
            SCR => self.scr = value,
            _ => {}
        }
    }

    /// Guest read from a UART register.
    pub fn io_read(&mut self, port: u16) -> u8 {
        match port - SERIAL_PORT_BASE {
            THR_RBR_DLL if self.dlab() => self.dll,
            THR_RBR_DLL => {
                let byte = self.rx.pop_front().unwrap_or(0);
                self.update_interrupt(); // more RX data => new edge
                byte
            }
            IER_DLH if self.dlab() => self.dlh,
            IER_DLH => self.ier,
            IIR_FCR => {
                let source = self.current_source();
                if source == IIR_THRE {
                    // Reading IIR acknowledges the THRE condition.
                    self.thre_pending = false;
                }
                source | IIR_FIFOS_ENABLED
            }
            LCR => self.lcr,
            MCR => self.mcr,
            LSR => {
                let mut lsr = LSR_THRE | LSR_TEMT;
                if !self.rx.is_empty() {
                    lsr |= LSR_DR;
                }
                lsr
            }
            MSR => MSR_STATIC,
            SCR => self.scr,
            _ => 0xff,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    #[derive(Clone, Default)]
    struct Sink(Arc<Mutex<Vec<u8>>>);
    impl Write for Sink {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    fn uart() -> (SerialConsole, EventFd, Sink) {
        let evt = EventFd::new(EFD_NONBLOCK).unwrap();
        let sink = Sink::default();
        let uart = SerialConsole::with_trigger(evt.try_clone().unwrap(), Box::new(sink.clone()));
        (uart, evt, sink)
    }

    fn fired(evt: &EventFd) -> bool {
        evt.read().is_ok() // nonblocking: Err when no pulses
    }

    #[test]
    fn tx_reaches_sink() {
        let (mut u, _evt, sink) = uart();
        for b in b"hi" {
            u.io_write(SERIAL_PORT_BASE + THR_RBR_DLL, *b);
        }
        assert_eq!(*sink.0.lock().unwrap(), b"hi");
    }

    /// The regression that motivated this module: enabling THRI while the
    /// transmitter is idle must raise the interrupt immediately.
    #[test]
    fn enabling_thri_on_idle_transmitter_fires_interrupt() {
        let (mut u, evt, _sink) = uart();
        assert!(!fired(&evt));
        u.io_write(SERIAL_PORT_BASE + IER_DLH, IER_THRE);
        assert!(fired(&evt), "THRE interrupt must fire on IER enable");
        // IIR reports THRE, then clears to none.
        let iir = u.io_read(SERIAL_PORT_BASE + IIR_FCR);
        assert_eq!(iir & 0x0f, IIR_THRE);
        let iir = u.io_read(SERIAL_PORT_BASE + IIR_FCR);
        assert_eq!(iir & 0x0f, IIR_NONE);
    }

    #[test]
    fn thr_write_rearms_thre_interrupt() {
        let (mut u, evt, _sink) = uart();
        u.io_write(SERIAL_PORT_BASE + IER_DLH, IER_THRE);
        let _ = u.io_read(SERIAL_PORT_BASE + IIR_FCR); // ack initial
        while fired(&evt) {}
        u.io_write(SERIAL_PORT_BASE + THR_RBR_DLL, b'x');
        assert!(fired(&evt), "THR write must re-raise THRE");
    }

    #[test]
    fn rx_has_priority_and_drains() {
        let (mut u, evt, _sink) = uart();
        u.io_write(SERIAL_PORT_BASE + IER_DLH, IER_RDA | IER_THRE);
        u.push_input(b"ab");
        assert!(fired(&evt));
        assert_eq!(u.io_read(SERIAL_PORT_BASE + IIR_FCR) & 0x0f, IIR_RDA);
        assert_ne!(u.io_read(SERIAL_PORT_BASE + LSR) & LSR_DR, 0);
        assert_eq!(u.io_read(SERIAL_PORT_BASE + THR_RBR_DLL), b'a');
        assert_eq!(u.io_read(SERIAL_PORT_BASE + THR_RBR_DLL), b'b');
        assert_eq!(u.io_read(SERIAL_PORT_BASE + LSR) & LSR_DR, 0);
    }

    #[test]
    fn dlab_maps_divisor_latch() {
        let (mut u, _evt, sink) = uart();
        u.io_write(SERIAL_PORT_BASE + LCR, LCR_DLAB);
        u.io_write(SERIAL_PORT_BASE + THR_RBR_DLL, 0x42); // DLL, not TX
        u.io_write(SERIAL_PORT_BASE + IER_DLH, 0x01); // DLH, not IER
        assert!(sink.0.lock().unwrap().is_empty());
        assert_eq!(u.io_read(SERIAL_PORT_BASE + THR_RBR_DLL), 0x42);
        u.io_write(SERIAL_PORT_BASE + LCR, 0);
        assert_eq!(u.io_read(SERIAL_PORT_BASE + IER_DLH), 0);
    }

    #[test]
    fn rx_overrun_is_bounded() {
        let (mut u, _evt, _sink) = uart();
        u.push_input(&vec![b'x'; RX_CAPACITY * 2]);
        assert_eq!(u.rx.len(), RX_CAPACITY);
    }
}
