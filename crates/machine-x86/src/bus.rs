//! The machine's port-I/O and MMIO dispatch, implementing
//! `vmm_core::ExitHandler` for the vCPU run loop. Routes the serial console on
//! the legacy COM1 ports, the virtio-mmio window (EPIC 3) and the PCI bus
//! (EPIC 19) by address; everything else floats high on reads, like unclaimed
//! ISA lines.
//!
//! A VM carries **one** virtio transport, chosen by its configuration — the
//! other bus is empty. They cannot collide in any case: their address ranges are
//! disjoint (`crate::layout`), and only one of them is ever populated.
//!
//! Queue-notify writes for queues whose kicks are offloaded to an ioeventfd
//! (MVP-307) normally never reach this handler — KVM completes them in the
//! kernel. The ones that do (a datamatch miss, i.e. a queue index the device does
//! not have) still take this path, where the transport drops them instead of
//! running the device a second time; see `crate::notify`.

use std::sync::{Arc, Mutex};

use vmm_core::ExitHandler;

use crate::platform::FirmwarePlatform;
use crate::serial::SerialConsole;
use crate::virtio::VirtioMmioBus;
use crate::virtio_pci::VirtioPciBus;

#[derive(Clone)]
pub struct MachineBus {
    serial: Arc<Mutex<SerialConsole>>,
    virtio: Arc<VirtioMmioBus>,
    /// The PCI root bus and the virtio devices on it, present when the VM uses
    /// the pci transport. `None` leaves the machine exactly as it was before
    /// EPIC 19: an mmio-transport guest must not suddenly find a PCI bus.
    pci: Option<Arc<VirtioPciBus>>,
    /// Host-bridge stub + ACPI PM timer + RTC, present only for UEFI boots
    /// (EPIC 18). A direct-Linux guest must keep seeing exactly the machine it
    /// saw before: no extra claimed ports.
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
            pci: None,
            platform: None,
        }
    }

    /// A machine with the serial console plus a PCI bus carrying the virtio
    /// devices (EPIC 19). The virtio-mmio window stays empty.
    pub fn with_virtio_pci(serial: SerialConsole, pci: VirtioPciBus) -> Self {
        Self {
            serial: Arc::new(Mutex::new(serial)),
            virtio: Arc::new(VirtioMmioBus::empty()),
            pci: Some(Arc::new(pci)),
            platform: None,
        }
    }

    /// Adds the firmware-facing platform devices (UEFI-1802). Without these an
    /// EDK2 CloudHv firmware asserts in SEC on the host bridge device ID and,
    /// past that, spins forever in `MicroSecondDelay()`.
    ///
    /// When a real PCI bus is present it owns the configuration ports and this
    /// stub's one-device config space is never consulted — the real bus keeps the
    /// same host bridge identity, so the firmware sees no difference.
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

    /// The PCI bus behind this machine, if the VM uses the pci transport.
    pub fn pci(&self) -> Option<&VirtioPciBus> {
        self.pci.as_deref()
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
        // The real PCI bus takes the configuration ports ahead of the firmware
        // stub, which models the same host bridge but no devices.
        if let Some(pci) = &self.pci {
            if VirtioPciBus::claims_port(port) {
                pci.io_write(port, data);
                return;
            }
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
        if let Some(pci) = &self.pci {
            if VirtioPciBus::claims_port(port) {
                pci.io_read(port, data);
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
        if let Some((slot, offset)) = self.virtio.locate(addr) {
            match slot.transport.lock() {
                Ok(mut transport) => transport.write(offset, data),
                // Poisoning means a host-side panic already happened elsewhere;
                // a guest write must not turn that into a second panic.
                Err(_) => tracing::error!(
                    addr = format_args!("{addr:#x}"),
                    "virtio-mmio transport lock is poisoned; dropping guest write"
                ),
            }
            return;
        }
        if let Some(pci) = &self.pci {
            // Undecoded addresses (a BAR the driver has not enabled, or one it
            // has moved out of the aperture) are dropped inside the bus.
            pci.mmio_write(addr, data);
        }
    }

    fn mmio_read(&mut self, addr: u64, data: &mut [u8]) {
        data.fill(0);
        if let Some((slot, offset)) = self.virtio.locate(addr) {
            match slot.transport.lock() {
                Ok(mut transport) => transport.read(offset, data),
                Err(_) => tracing::error!(
                    addr = format_args!("{addr:#x}"),
                    "virtio-mmio transport lock is poisoned; reading zeroes"
                ),
            }
            return;
        }
        if let Some(pci) = &self.pci {
            pci.mmio_read(addr, data);
        }
    }
}
