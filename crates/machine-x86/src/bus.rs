//! The machine's port-I/O and MMIO dispatch, implementing
//! `vmm_core::ExitHandler` for the vCPU run loop. Routes the serial console on
//! the legacy COM1 ports and the virtio-mmio window (EPIC 3) by address;
//! everything else floats high on reads, like unclaimed ISA lines.

use std::sync::{Arc, Mutex};

use vmm_core::ExitHandler;

use crate::serial::SerialConsole;
use crate::virtio::VirtioMmioBus;

#[derive(Clone)]
pub struct MachineBus {
    serial: Arc<Mutex<SerialConsole>>,
    virtio: Arc<VirtioMmioBus>,
}

impl MachineBus {
    /// A machine with only the serial console.
    pub fn new(serial: SerialConsole) -> Self {
        Self::with_virtio(serial, VirtioMmioBus::empty())
    }

    /// A machine with the serial console plus an attached virtio-mmio window.
    pub fn with_virtio(serial: SerialConsole, virtio: VirtioMmioBus) -> Self {
        Self {
            serial: Arc::new(Mutex::new(serial)),
            virtio: Arc::new(virtio),
        }
    }
}

impl ExitHandler for MachineBus {
    fn io_out(&mut self, port: u16, data: &[u8]) {
        if SerialConsole::contains(port) {
            if let Ok(mut serial) = self.serial.lock() {
                for &byte in data {
                    serial.io_write(port, byte);
                }
            }
        }
    }

    fn io_in(&mut self, port: u16, data: &mut [u8]) {
        if SerialConsole::contains(port) {
            if let Ok(mut serial) = self.serial.lock() {
                for byte in data.iter_mut() {
                    *byte = serial.io_read(port);
                }
                return;
            }
        }
        data.fill(0xff);
    }

    fn mmio_write(&mut self, addr: u64, data: &[u8]) {
        let Some((slot, offset)) = self.virtio.locate(addr) else {
            return;
        };
        match slot.transport.lock() {
            Ok(mut transport) => transport.write(offset, data),
            // Poisoning means a host-side panic already happened elsewhere; a
            // guest write must not turn that into a second panic.
            Err(_) => tracing::error!(
                addr = format_args!("{addr:#x}"),
                "virtio-mmio transport lock is poisoned; dropping guest write"
            ),
        }
    }

    fn mmio_read(&mut self, addr: u64, data: &mut [u8]) {
        data.fill(0);
        let Some((slot, offset)) = self.virtio.locate(addr) else {
            return;
        };
        match slot.transport.lock() {
            Ok(mut transport) => transport.read(offset, data),
            Err(_) => tracing::error!(
                addr = format_args!("{addr:#x}"),
                "virtio-mmio transport lock is poisoned; reading zeroes"
            ),
        }
    }
}
