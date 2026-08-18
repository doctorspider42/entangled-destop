//! The machine's port-I/O and MMIO dispatch, implementing
//! `vmm_core::ExitHandler` for the vCPU run loop. Routes the serial console on
//! the legacy COM1 ports and the virtio-mmio window (EPIC 3) by address;
//! everything else floats high on reads, like unclaimed ISA lines.
//!
//! `QUEUE_NOTIFY` writes for queues whose kicks are offloaded to an ioeventfd
//! (MVP-307) normally never reach this handler — KVM completes them in the
//! kernel. The ones that do (a datamatch miss, i.e. a queue index the device
//! does not have) still take this path, where `MmioTransport::write` drops them
//! instead of running the device a second time; see `crate::notify`.

use std::sync::{Arc, Mutex};

use vmm_core::ExitHandler;

use crate::platform::FirmwarePlatform;
use crate::serial::SerialConsole;
use crate::virtio::VirtioMmioBus;

#[derive(Clone)]
pub struct MachineBus {
    serial: Arc<Mutex<SerialConsole>>,
    virtio: Arc<VirtioMmioBus>,
    /// PCI configuration space + ACPI PM timer, present only for UEFI boots
    /// (EPIC 18). A direct-Linux guest must keep seeing exactly the machine it
    /// saw before: no host bridge to enumerate, no extra claimed ports.
    platform: Option<Arc<Mutex<FirmwarePlatform>>>,
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
            platform: None,
        }
    }

    /// Adds the firmware-facing platform devices (UEFI-1802). Without these an
    /// EDK2 CloudHv firmware asserts in SEC on the host bridge device ID and,
    /// past that, spins forever in `MicroSecondDelay()`.
    pub fn with_firmware_platform(mut self) -> Self {
        self.platform = Some(Arc::new(Mutex::new(FirmwarePlatform::new())));
        self
    }

    /// The virtio-mmio window behind this bus, for inspection: `entangled
    /// doctor`, and test harnesses that want to report device state when a guest
    /// stops making progress.
    pub fn virtio(&self) -> &VirtioMmioBus {
        &self.virtio
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
            return;
        }
        if let Some(platform) = &self.platform {
            if FirmwarePlatform::contains(port) {
                match platform.lock() {
                    Ok(mut platform) => {
                        platform.io_write(port, data);
                    }
                    Err(_) => tracing::error!(
                        port = format_args!("{port:#x}"),
                        "platform lock is poisoned; dropping guest write"
                    ),
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
        if let Some(platform) = &self.platform {
            if FirmwarePlatform::contains(port) {
                if let Ok(mut platform) = platform.lock() {
                    if platform.io_read(port, data) {
                        return;
                    }
                }
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
