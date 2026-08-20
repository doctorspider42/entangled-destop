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
//! Both virtio transports are on this bus on both hosts: the **virtio-mmio**
//! window since EPIC 17 phase 3 (`VirtioMmioBus::attach_userspace`), the **PCI**
//! bus since phase 4 (`VirtioPciBus::attach_userspace` — IOAPIC INTx lines, the
//! userspace MSI sink, synchronous kicks that follow a BAR move by construction
//! because every access is decoded against the BAR's current base).

use std::sync::{Arc, Mutex};

use vmm_core::ExitHandler;

use crate::acpi::AcpiPmBlock;
use crate::irqchip::UserspaceIrqChip;
use crate::pflash::Pflash;
use crate::platform::FirmwarePlatform;
use crate::reset::ResetControl;
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
    /// The CFI flash device backing the UEFI variable store (UEFI-1804),
    /// present only for a UEFI boot with an NVRAM file. `None` leaves the
    /// window at 4 GiB − 4 MiB undecoded, which is what every boot before this
    /// existed saw — and what a direct-Linux guest must keep seeing.
    pflash: Option<Arc<Mutex<Pflash>>>,
    /// The three ways a guest asks to be rebooted (ADR-0005), on both hosts and
    /// in both boot modes — a UEFI firmware's `ResetSystem` and a Linux
    /// `reboot(2)` land on the same register. Behind an `Arc` with interior
    /// mutability for the same reason [`Self::acpi_pm`] is: the run loop reads
    /// its latch after every exit and must take no lock to do it.
    reset: Arc<ResetControl>,
}

impl MachineBus {
    /// A machine with only the serial console.
    pub fn new(serial: SerialConsole) -> Self {
        Self {
            serial: Arc::new(Mutex::new(serial)),
            virtio: Arc::new(VirtioMmioBus::empty()),
            pci: None,
            acpi_pm: Arc::new(AcpiPmBlock::new()),
            irqchip: None,
            platform: None,
            pflash: None,
            reset: Arc::new(ResetControl::new()),
        }
    }

    /// A machine with the serial console plus an attached virtio-mmio window.
    pub fn with_virtio(serial: SerialConsole, virtio: VirtioMmioBus) -> Self {
        Self {
            virtio: Arc::new(virtio),
            ..Self::new(serial)
        }
    }

    /// A machine with the serial console plus a PCI bus carrying the virtio
    /// devices (EPIC 19). The virtio-mmio window stays empty.
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

    /// Attaches the UEFI variable store's flash device (UEFI-1804).
    ///
    /// Only meaningful together with [`Self::with_firmware_platform`]: it is the
    /// firmware that speaks the CFI command set, and a direct-Linux guest never
    /// touches the window.
    pub fn with_pflash(mut self, pflash: Arc<Mutex<Pflash>>) -> Self {
        self.pflash = Some(pflash);
        self
    }

    /// The flash device behind this bus, if any — for reporting what the
    /// firmware actually wrote (`Pflash::stats`).
    pub fn pflash(&self) -> Option<&Arc<Mutex<Pflash>>> {
        self.pflash.as_ref()
    }

    /// Queues bytes on the serial console's receive path, as if they had been
    /// typed on it (UEFI-1804: this is how the installer's kernel command line
    /// is edited in GRUB, and the only guest-input channel both EDK2 and GRUB
    /// listen to — neither has a virtio-input driver).
    pub fn push_serial_input(&self, bytes: &[u8]) {
        match self.serial.lock() {
            Ok(mut serial) => serial.push_input(bytes),
            Err(_) => tracing::error!("serial lock is poisoned; dropping host input"),
        }
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

    /// The machine's reset controls (ADR-0005), for the supervisor that turns a
    /// latched guest request into an in-place reboot.
    pub fn reset_control(&self) -> &Arc<ResetControl> {
        &self.reset
    }

    /// Shares the VM's pause gate with every device that has a worker of its
    /// own (ADR-0005). Called once, while the machine is being wired.
    pub fn set_quiesce(&self, quiesce: Arc<virtio_core::Quiesce>) {
        self.virtio.set_quiesce(Arc::clone(&quiesce));
        if let Some(pci) = &self.pci {
            pci.set_quiesce(quiesce);
        }
    }

    /// Stops or restarts the machine's own sources of activity for a pause
    /// (ADR-0005).
    ///
    /// The device workers are handled by the gate [`Self::set_quiesce`]
    /// installed; what is left is the two things that run off *host* time and
    /// would otherwise hand the resumed guest a jump: the 8254's timer thread
    /// (on a host with a userspace irqchip) and the ACPI PM timer.
    pub fn set_paused(&self, paused: bool) {
        if let Some(irqchip) = &self.irqchip {
            irqchip.set_paused(paused);
        }
        if paused {
            self.acpi_pm.pause();
        } else {
            self.acpi_pm.resume();
        }
    }

    /// Everything on this bus, for a snapshot (ADR-0006).
    ///
    /// The mirror image of [`Self::reset_devices`], device for device. Called
    /// with every vCPU parked and the host workers quiesced, so it may take any
    /// lock and what it reads is a single consistent instant.
    ///
    /// What is deliberately **not** in it is the same list `reset_devices`
    /// deliberately does not touch: the pflash contents (the NVRAM file is the
    /// store), the serial console's output sink and interrupt line, the IOAPIC
    /// id, and the host-side diagnostic counters. All of them are the host's,
    /// rebuilt by whoever assembles the machine the snapshot is loaded into.
    pub fn save_state(&self) -> crate::state::MachineState {
        let mut virtio = self.virtio.save_state();
        let mut pci_root = None;
        if let Some(pci) = &self.pci {
            virtio = pci.save_state();
            pci_root = pci.save_config();
        }
        crate::state::MachineState {
            serial: match self.serial.lock() {
                Ok(serial) => serial.save_state(),
                Err(_) => {
                    tracing::error!("serial lock is poisoned; saving a power-on UART");
                    crate::state::SavedSerial::default()
                }
            },
            acpi_pm: self.acpi_pm.save_state(),
            reset: self.reset.save_state(),
            platform: self
                .platform
                .as_ref()
                .map(|platform| match platform.lock() {
                    Ok(platform) => platform.save_state(),
                    Err(_) => {
                        tracing::error!("platform lock is poisoned; saving a power-on RTC");
                        crate::state::SavedPlatform::default()
                    }
                }),
            pflash: self.pflash.as_ref().map(|pflash| match pflash.lock() {
                Ok(pflash) => pflash.save_state(),
                Err(_) => {
                    tracing::error!("pflash lock is poisoned; saving a read-array flash");
                    crate::state::SavedPflash::default()
                }
            }),
            pci_root,
            irqchip: self.irqchip.as_ref().map(|chip| chip.save_state()),
            virtio,
        }
    }

    /// Puts it all back.
    ///
    /// The order is the reverse of the reset order, and for the mirror-image
    /// reason. A reset masks the interrupt sources **first**, so nothing can
    /// deliver into a CPU with no IDT; a restore programs them **last**, so
    /// nothing can deliver while the devices behind them are still half the
    /// power-on machine. In between, the devices go back in bus order, and the
    /// virtio transports go last of those because restoring one re-activates
    /// it — which means it may start serving its queues immediately.
    ///
    /// A machine whose *shape* differs from the snapshot's is refused rather
    /// than partially loaded: a missing pflash, a different number of virtio
    /// slots, a PCI bus that is not there. The caller has already compared the
    /// configuration fingerprints, so reaching one of these is a bug in this
    /// build rather than a user error — but the check is cheap and the failure
    /// it prevents is a silently wrong guest.
    pub fn load_state(
        &self,
        state: &crate::state::MachineState,
    ) -> Result<(), crate::state::StateError> {
        use crate::state::require_same_presence;

        require_same_presence(
            "firmware platform",
            state.platform.as_ref(),
            self.platform.is_some(),
        )?;
        require_same_presence("pflash", state.pflash.as_ref(), self.pflash.is_some())?;
        require_same_presence("PCI bus", state.pci_root.as_ref(), self.pci.is_some())?;
        require_same_presence(
            "userspace interrupt chip",
            state.irqchip.as_ref(),
            self.irqchip.is_some(),
        )?;

        match self.serial.lock() {
            Ok(mut serial) => serial.load_state(&state.serial),
            Err(_) => return Err(crate::state::StateError::Poisoned("the 16550")),
        }
        if let (Some(platform), Some(saved)) = (&self.platform, &state.platform) {
            match platform.lock() {
                Ok(mut platform) => platform.load_state(saved)?,
                Err(_) => return Err(crate::state::StateError::Poisoned("the firmware platform")),
            }
        }
        if let (Some(pflash), Some(saved)) = (&self.pflash, &state.pflash) {
            match pflash.lock() {
                Ok(mut pflash) => pflash.load_state(saved)?,
                Err(_) => return Err(crate::state::StateError::Poisoned("the pflash device")),
            }
        }
        self.acpi_pm.load_state(&state.acpi_pm);
        self.reset.load_state(&state.reset);

        match (&self.pci, &state.pci_root) {
            (Some(pci), Some(config)) => pci.load_state(config, &state.virtio)?,
            _ => self.virtio.load_state(&state.virtio)?,
        }

        if let (Some(chip), Some(saved)) = (&self.irqchip, &state.irqchip) {
            chip.load_state(saved)?;
        }
        Ok(())
    }

    /// Every device on this bus back to its power-on state (ADR-0005).
    ///
    /// Called with every vCPU parked and the host workers quiesced, so it may
    /// take any device lock. The order is the one a real machine's reset line
    /// implies: **interrupt sources first** (the chips that could deliver into a
    /// CPU that has no IDT yet), then the devices, then the latches that say a
    /// reset was asked for.
    ///
    /// What is deliberately *not* reset: the pflash **contents** (that is the
    /// non-volatile variable store, and a UEFI VM boots the entry it holds), the
    /// serial console's output sink and interrupt line, the IOAPIC id, and the
    /// host-side diagnostic counters. Everything else in the table in ADR-0005.
    pub fn reset_devices(&self) {
        if let Some(irqchip) = &self.irqchip {
            irqchip.reset();
        }
        self.virtio.reset();
        if let Some(pci) = &self.pci {
            pci.reset();
        }
        match self.serial.lock() {
            Ok(mut serial) => serial.reset(),
            Err(_) => tracing::error!("serial lock is poisoned; the UART is not reset"),
        }
        if let Some(platform) = &self.platform {
            match platform.lock() {
                Ok(mut platform) => platform.reset(),
                Err(_) => {
                    tracing::error!(
                        "platform lock is poisoned; the RTC and host bridge stay as they were"
                    )
                }
            }
        }
        if let Some(pflash) = &self.pflash {
            match pflash.lock() {
                Ok(mut flash) => flash.reset(),
                Err(_) => tracing::error!(
                    "pflash lock is poisoned; the flash command state machine is not reset"
                ),
            }
        }
        self.acpi_pm.reset();
        self.reset.clear();
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
        // Before the PCI configuration ports: 0xCF9 sits inside the 0xCF8..0xD0
        // range the legacy configuration mechanism nominally covers, and it is a
        // reset register on every real chipset that has both.
        if ResetControl::claims_write(port) {
            self.reset.io_write(port, data);
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
        // Reads of the keyboard command port are deliberately *not* claimed —
        // see `crate::reset` — so this is 0xCF9 only.
        if ResetControl::claims_port(port) {
            self.reset.io_read(port, data);
            return;
        }
        if let Some(irqchip) = &self.irqchip {
            if UserspaceIrqChip::claims_port(port) {
                irqchip.io_read(port, data);
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
        if let Some(pflash) = &self.pflash {
            match pflash.lock() {
                Ok(mut flash) if flash.contains(addr) => {
                    flash.mmio_write(addr, data);
                    return;
                }
                Ok(_) => {}
                Err(_) => tracing::error!(
                    addr = format_args!("{addr:#x}"),
                    "pflash lock is poisoned; dropping guest write"
                ),
            }
        }
        if let Some(irqchip) = &self.irqchip {
            if UserspaceIrqChip::claims_mmio(addr) {
                irqchip.mmio_write(addr, data);
                return;
            }
        }
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
            // Undecoded addresses (a BAR the driver has not enabled, or one it
            // has moved out of the aperture) are dropped inside the bus.
            pci.mmio_write(addr, data);
            return;
        }
        tracing::debug!(
            addr = format_args!("{addr:#x}"),
            bytes = data.len(),
            "MMIO write to an unclaimed address; dropped"
        );
    }

    fn mmio_read(&mut self, addr: u64, data: &mut [u8]) {
        data.fill(0);
        if let Some(pflash) = &self.pflash {
            match pflash.lock() {
                Ok(mut flash) if flash.contains(addr) => {
                    flash.mmio_read(addr, data);
                    return;
                }
                Ok(_) => {}
                Err(_) => tracing::error!(
                    addr = format_args!("{addr:#x}"),
                    "pflash lock is poisoned; reading zeroes"
                ),
            }
        }
        if let Some(irqchip) = &self.irqchip {
            if UserspaceIrqChip::claims_mmio(addr) {
                irqchip.mmio_read(addr, data);
                return;
            }
        }
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
            return;
        }
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

    /// A write to one of the reset controls (`crate::reset`) reboots the VM in
    /// place. Same shape as the shutdown latch above, and shared the same way.
    fn reset_requested(&self) -> bool {
        self.reset.is_reset_requested()
    }
}
