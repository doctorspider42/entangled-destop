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
//!
//! # What the two hosts put on this bus
//!
//! On KVM the interrupt controllers (PIC, IOAPIC, PIT) live in the kernel and
//! never appear here. WHP has none of them, so a WHP machine attaches
//! [`crate::irqchip::UserspaceIrqChip`] with [`MachineBus::with_irqchip`], which
//! claims the 8259 and 8254 ports and the IOAPIC's MMIO page. A machine without
//! one behaves exactly as this bus always did — the ports stay unclaimed and
//! float high.
//!
//! The virtio buses are the mirror image: their wiring is irqfds and ioeventfds,
//! so they are Linux-only until EPIC 17 phase 3 attaches the same devices through
//! the userspace irqchip.

use std::sync::{Arc, Mutex};

use vmm_core::ExitHandler;

use crate::acpi::AcpiPmBlock;
use crate::irqchip::UserspaceIrqChip;
use crate::platform::FirmwarePlatform;
use crate::serial::SerialConsole;
#[cfg(target_os = "linux")]
use crate::virtio::VirtioMmioBus;
#[cfg(target_os = "linux")]
use crate::virtio_pci::VirtioPciBus;

#[derive(Clone)]
pub struct MachineBus {
    serial: Arc<Mutex<SerialConsole>>,
    #[cfg(target_os = "linux")]
    virtio: Arc<VirtioMmioBus>,
    /// The PCI root bus and the virtio devices on it, present when the VM uses
    /// the pci transport. `None` leaves the machine exactly as it was before
    /// EPIC 19: an mmio-transport guest must not suddenly find a PCI bus.
    #[cfg(target_os = "linux")]
    pci: Option<Arc<VirtioPciBus>>,
    /// The ACPI fixed-feature registers (0x600..0x610), in **both** boot modes:
    /// the FADT `machine_x86::acpi` publishes names these ports for a
    /// direct-Linux guest exactly as it does for a firmware. An S5 write here is
    /// what `ExitHandler::shutdown_requested` reports.
    ///
    /// Behind an `Arc` with interior mutability rather than a `Mutex<…>` field,
    /// so the shutdown check on the hot exit path takes no lock.
    acpi_pm: Arc<AcpiPmBlock>,
    /// The userspace 8259/8254/IOAPIC set, present only on a host whose
    /// hypervisor has no in-kernel interrupt controllers (WHP). `None` on KVM,
    /// where claiming those ports would fight the in-kernel chips.
    irqchip: Option<Arc<UserspaceIrqChip>>,
    /// Host-bridge stub + RTC, present only for UEFI boots (EPIC 18). A
    /// direct-Linux guest must keep seeing exactly the machine it saw before:
    /// no extra claimed ports. (When the real PCI bus is present it owns the
    /// configuration ports; the ACPI PM registers live in `acpi_pm` for both.)
    platform: Option<Arc<Mutex<FirmwarePlatform>>>,
}

impl MachineBus {
    /// A machine with only the serial console.
    pub fn new(serial: SerialConsole) -> Self {
        Self {
            serial: Arc::new(Mutex::new(serial)),
            #[cfg(target_os = "linux")]
            virtio: Arc::new(VirtioMmioBus::empty()),
            #[cfg(target_os = "linux")]
            pci: None,
            acpi_pm: Arc::new(AcpiPmBlock::new()),
            irqchip: None,
            platform: None,
        }
    }

    /// A machine with the serial console plus an attached virtio-mmio window.
    #[cfg(target_os = "linux")]
    pub fn with_virtio(serial: SerialConsole, virtio: VirtioMmioBus) -> Self {
        Self {
            virtio: Arc::new(virtio),
            ..Self::new(serial)
        }
    }

    /// A machine with the serial console plus a PCI bus carrying the virtio
    /// devices (EPIC 19). The virtio-mmio window stays empty.
    #[cfg(target_os = "linux")]
    pub fn with_virtio_pci(serial: SerialConsole, pci: VirtioPciBus) -> Self {
        Self {
            pci: Some(Arc::new(pci)),
            ..Self::new(serial)
        }
    }

    /// The ACPI PM register block behind this bus, for `entangled doctor` and
    /// for a host-initiated shutdown path that wants to observe the same latch.
    pub fn acpi_pm(&self) -> &Arc<AcpiPmBlock> {
        &self.acpi_pm
    }

    /// Attaches the userspace interrupt controllers (WHP-1703). Only a host
    /// without in-kernel ones may do this: on KVM the 8259/8254 ports and the
    /// IOAPIC page are served in the kernel and must not be claimed here.
    pub fn with_irqchip(mut self, irqchip: Arc<UserspaceIrqChip>) -> Self {
        self.irqchip = Some(irqchip);
        self
    }

    /// The userspace interrupt controllers behind this bus, if any.
    pub fn irqchip(&self) -> Option<&Arc<UserspaceIrqChip>> {
        self.irqchip.as_ref()
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
    #[cfg(target_os = "linux")]
    pub fn virtio(&self) -> &VirtioMmioBus {
        &self.virtio
    }

    /// The PCI bus behind this machine, if the VM uses the pci transport.
    #[cfg(target_os = "linux")]
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
        if AcpiPmBlock::contains(port) {
            self.acpi_pm.io_write(port, data);
            return;
        }
        if let Some(irqchip) = &self.irqchip {
            if UserspaceIrqChip::claims_port(port) {
                irqchip.io_write(port, data);
                return;
            }
        }
        // The real PCI bus takes the configuration ports ahead of the firmware
        // stub, which models the same host bridge but no devices.
        #[cfg(target_os = "linux")]
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
        if AcpiPmBlock::contains(port) {
            self.acpi_pm.io_read(port, data);
            return;
        }
        if let Some(irqchip) = &self.irqchip {
            if UserspaceIrqChip::claims_port(port) {
                irqchip.io_read(port, data);
                return;
            }
        }
        #[cfg(target_os = "linux")]
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
        if let Some(irqchip) = &self.irqchip {
            if UserspaceIrqChip::claims_mmio(addr) {
                irqchip.mmio_write(addr, data);
                return;
            }
        }
        #[cfg(target_os = "linux")]
        {
            if let Some((slot, offset)) = self.virtio.locate(addr) {
                match slot.transport.lock() {
                    Ok(mut transport) => transport.write(offset, data),
                    // Poisoning means a host-side panic already happened
                    // elsewhere; a guest write must not turn that into a second
                    // panic.
                    Err(_) => tracing::error!(
                        addr = format_args!("{addr:#x}"),
                        "virtio-mmio transport lock is poisoned; dropping guest write"
                    ),
                }
                return;
            }
            if let Some(pci) = &self.pci {
                // Undecoded addresses (a BAR the driver has not enabled, or one
                // it has moved out of the aperture) are dropped inside the bus.
                pci.mmio_write(addr, data);
            }
        }
        #[cfg(not(target_os = "linux"))]
        tracing::debug!(
            addr = format_args!("{addr:#x}"),
            bytes = data.len(),
            "MMIO write to an unclaimed address; dropped"
        );
    }

    fn mmio_read(&mut self, addr: u64, data: &mut [u8]) {
        data.fill(0);
        if let Some(irqchip) = &self.irqchip {
            if UserspaceIrqChip::claims_mmio(addr) {
                irqchip.mmio_read(addr, data);
                return;
            }
        }
        #[cfg(target_os = "linux")]
        {
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
        #[cfg(not(target_os = "linux"))]
        tracing::debug!(
            addr = format_args!("{addr:#x}"),
            bytes = data.len(),
            "MMIO read from an unclaimed address; returning zeroes"
        );
    }

    /// An ACPI S5 write on the PM block ends the VM. Every vCPU's handler is a
    /// clone of this bus and shares the same latch, so whichever vCPU exits
    /// next stops too.
    fn shutdown_requested(&self) -> bool {
        self.acpi_pm.is_shutdown_requested()
    }
}
