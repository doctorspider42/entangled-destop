//! The machine's port-I/O and MMIO dispatch, implementing
//! `vmm_core::ExitHandler` for the vCPU run loop. Grows a virtio-mmio window
//! with EPIC 3; today it routes the serial console and ignores the rest
//! (reads float high, like unclaimed ISA lines).

use std::sync::{Arc, Mutex};

use vmm_core::ExitHandler;

use crate::serial::SerialConsole;

#[derive(Clone)]
pub struct MachineBus {
    serial: Arc<Mutex<SerialConsole>>,
}

impl MachineBus {
    pub fn new(serial: SerialConsole) -> Self {
        Self {
            serial: Arc::new(Mutex::new(serial)),
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

    fn mmio_write(&mut self, _addr: u64, _data: &[u8]) {}

    fn mmio_read(&mut self, _addr: u64, data: &mut [u8]) {
        data.fill(0);
    }
}
