//! 16550 serial console at the classic COM1 port (backlog MVP-205/206),
//! backed by the `vm-superio` UART emulation, interrupts via irqfd.

use std::io::Write;

use kvm_ioctls::VmFd;
use thiserror::Error;
use vm_superio::serial::NoEvents;
use vm_superio::{Serial, Trigger};
use vmm_sys_util::eventfd::{EventFd, EFD_NONBLOCK};

/// COM1.
pub const SERIAL_PORT_BASE: u16 = 0x3f8;
pub const SERIAL_PORT_LAST: u16 = 0x3ff;
pub const SERIAL_IRQ: u32 = 4;

#[derive(Debug, Error)]
pub enum SerialError {
    #[error("failed to create serial interrupt eventfd: {0}")]
    EventFd(#[source] std::io::Error),

    #[error("failed to register serial irqfd: {0}")]
    Irqfd(#[source] kvm_ioctls::Error),
}

/// Adapts an `EventFd` to vm-superio's `Trigger`; the eventfd is registered
/// with KVM as an irqfd for [`SERIAL_IRQ`], so triggering it injects the
/// interrupt without a userspace round-trip.
pub struct EventFdTrigger(EventFd);

impl Trigger for EventFdTrigger {
    type E = std::io::Error;

    fn trigger(&self) -> Result<(), Self::E> {
        self.0.write(1)
    }
}

/// The guest-visible UART plus its host output sink.
pub struct SerialConsole {
    serial: Serial<EventFdTrigger, NoEvents, Box<dyn Write + Send>>,
}

impl SerialConsole {
    /// Creates the UART and wires its interrupt line to the VM's IRQ 4.
    pub fn new(vm: &VmFd, out: Box<dyn Write + Send>) -> Result<Self, SerialError> {
        let evt = EventFd::new(EFD_NONBLOCK).map_err(SerialError::EventFd)?;
        vm.register_irqfd(&evt, SERIAL_IRQ)
            .map_err(SerialError::Irqfd)?;
        Ok(Self {
            serial: Serial::new(EventFdTrigger(evt), out),
        })
    }

    /// True when `port` belongs to this UART.
    pub fn contains(port: u16) -> bool {
        (SERIAL_PORT_BASE..=SERIAL_PORT_LAST).contains(&port)
    }

    /// Guest write to a UART register. Errors (e.g. a full FIFO) are the
    /// guest's problem per 16550 semantics — dropped, never propagated.
    pub fn io_write(&mut self, port: u16, value: u8) {
        let _ = self.serial.write((port - SERIAL_PORT_BASE) as u8, value);
    }

    /// Guest read from a UART register.
    pub fn io_read(&mut self, port: u16) -> u8 {
        self.serial.read((port - SERIAL_PORT_BASE) as u8)
    }
}
