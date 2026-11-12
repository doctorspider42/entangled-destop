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
    /// The thread that makes the RTC's interrupts happen, once the RTC has a
    /// line ([`Self::with_rtc_interrupt`]). Shared by every clone of the bus
    /// and joined when the last one goes.
    rtc_timer: Option<Arc<crate::rtc::RtcTimer>>,
    /// The 8042 keyboard controller (ADR-0008 §5), once the machine has wired
    /// its interrupt ([`Self::with_keyboard`]). `None` leaves `0x60`/`0x64` as
    /// they were: reads float, and only the reset pulse is decoded.
    keyboard: Option<Arc<Mutex<crate::i8042::I8042>>>,
    /// The BIOS behind the interrupt stubs of a `mode = "bios"` machine
    /// (ADR-0012). `None` leaves its two ports undecoded, as they were.
    bios: Option<Arc<Mutex<pc_bios::Bios>>>,
    /// The IDE controller of a BIOS-era machine (ADR-0012), at the fixed
    /// ports `0x1f0`/`0x3f6` and `0x170`/`0x376`. `None` leaves them
    /// undecoded.
    ide: Option<Arc<Mutex<ide::Controller>>>,
    /// The VGA's registers on a BIOS-era machine (ADR-0012). `None` leaves
    /// `0x3c0..0x3df` undecoded, as on every other machine.
    vga: Option<Arc<Mutex<crate::vga::Vga>>>,
    /// How the VGA's memory window is taken from the guest while a mode
    /// needs the planes, on a host that can; and whether it is now.
    vga_trap: Option<Arc<dyn vmm_core::hv::RamTrap>>,
    vga_trapped: Arc<std::sync::atomic::AtomicBool>,
    /// IRQ 12 for a PS/2 mouse on the keyboard controller's second port
    /// (ADR-0012); given to the controller when both exist.
    mouse_line: Option<Arc<dyn virtio_core::interrupt::IrqLine>>,
}

#[cfg(test)]
mod stream_tests;

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
            rtc_timer: None,
            keyboard: None,
            bios: None,
            ide: None,
            vga: None,
            vga_trap: None,
            vga_trapped: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            mouse_line: None,
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

    /// Gives the machine a BIOS (ADR-0012): the two ports its interrupt stubs
    /// write are answered by `bios`.
    pub fn with_bios(mut self, bios: Arc<Mutex<pc_bios::Bios>>) -> Self {
        self.bios = Some(bios);
        self
    }

    /// Gives the machine an IDE controller (ADR-0012), already wired to its
    /// two interrupts.
    pub fn with_ide(mut self, ide: ide::Controller) -> Self {
        self.ide = Some(Arc::new(Mutex::new(ide)));
        self
    }

    /// Gives the machine a VGA (ADR-0012). `trap` is the host's way of
    /// taking `0xa0000..0xbffff` from the guest while a mode needs the
    /// planes; without one the machine has text mode only.
    pub fn with_vga(mut self, trap: Option<Arc<dyn vmm_core::hv::RamTrap>>) -> Self {
        self.vga = Some(Arc::new(Mutex::new(crate::vga::Vga::new())));
        self.vga_trap = trap;
        self
    }

    /// Makes the hypervisor's mapping of the VGA window agree with what the
    /// registers now ask for.
    fn sync_vga_trap(&self, wanted: bool) {
        use std::sync::atomic::Ordering;
        let Some(trap) = &self.vga_trap else {
            return;
        };
        if self.vga_trapped.load(Ordering::Acquire) == wanted {
            return;
        }
        match trap.set_trapped(wanted) {
            Ok(()) => self.vga_trapped.store(wanted, Ordering::Release),
            Err(error) => tracing::warn!(%error, wanted, "cannot switch the VGA memory window"),
        }
    }

    /// A guest access to `0xa0000..0xbffff` that exited: the window is
    /// trapped, so it belongs to the VGA's planes.
    fn vga_memory(&self, addr: u64) -> Option<&Arc<Mutex<crate::vga::Vga>>> {
        (0x000a_0000..0x000c_0000)
            .contains(&addr)
            .then_some(self.vga.as_ref())
            .flatten()
    }

    /// The machine's VGA, when it has one: what a presenter draws text mode
    /// from.
    pub fn vga(&self) -> Option<&Arc<Mutex<crate::vga::Vga>>> {
        self.vga.as_ref()
    }

    /// The machine's BIOS, when it has one.
    pub fn bios(&self) -> Option<&Arc<Mutex<pc_bios::Bios>>> {
        self.bios.as_ref()
    }

    /// One BIOS port write. Before a call runs, the bytes waiting in the
    /// keyboard controller go to the BIOS: it takes no keyboard interrupt, so this is
    /// how `int 16h` gets its keys.
    fn bios_write(
        &self,
        bios: &Mutex<pc_bios::Bios>,
        port: u16,
        data: &[u8],
        paging: Option<pc_bios::Paging>,
    ) {
        let mut codes = [0u8; 16];
        let mut count = 0;
        if port == pc_bios::rom::PORT_CALL {
            if let Some(Ok(mut keyboard)) = self.keyboard.as_ref().map(|k| k.lock()) {
                while count < codes.len() && keyboard.io_read(crate::i8042::PORT_COMMAND) & 1 != 0 {
                    codes[count] = keyboard.io_read(crate::i8042::PORT_DATA);
                    count += 1;
                }
            }
        }
        match bios.lock() {
            Ok(mut bios) => {
                for &code in &codes[..count] {
                    bios.scancode(code);
                }
                bios.io_out(port, data, paging);
            }
            Err(_) => tracing::error!("the BIOS lock is poisoned; dropping a BIOS call"),
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
        // The power button's SCI is an IOAPIC pin like any other here.
        self.acpi_pm.connect_sci(irqchip.sci_line());
        // So is the RTC's IRQ 8, when this machine has an RTC.
        let rtc = irqchip.rtc_line();
        self.irqchip = Some(irqchip);
        self.with_rtc_interrupt(rtc)
    }

    /// Wires the RTC to `line` (IRQ 8) and starts the thread that drives it
    /// (ADR-0008 §3). Nothing without a firmware platform: a direct-Linux
    /// machine has no RTC.
    /// Puts the 8042 keyboard controller on `0x60`/`0x64`, raising `line`
    /// (IRQ 1).
    pub fn with_keyboard(mut self, line: Arc<dyn virtio_core::interrupt::IrqLine>) -> Self {
        let mut controller = crate::i8042::I8042::new(line);
        if let Some(mouse) = &self.mouse_line {
            controller.connect_mouse(Arc::clone(mouse));
        }
        self.keyboard = Some(Arc::new(Mutex::new(controller)));
        self
    }

    /// Plugs a PS/2 mouse into the keyboard controller's second port,
    /// raising `line` (IRQ 12). In either order with
    /// [`Self::with_keyboard`].
    pub fn with_mouse(mut self, line: Arc<dyn virtio_core::interrupt::IrqLine>) -> Self {
        if let Some(Ok(mut controller)) = self.keyboard.as_ref().map(|k| k.lock()) {
            controller.connect_mouse(Arc::clone(&line));
        }
        self.mouse_line = Some(line);
        self
    }

    /// The keyboard controller, for the host's keys while the guest drives no
    /// virtio keyboard.
    pub fn keyboard(&self) -> Option<Arc<Mutex<crate::i8042::I8042>>> {
        self.keyboard.clone()
    }

    pub fn with_rtc_interrupt(mut self, line: Arc<dyn virtio_core::interrupt::IrqLine>) -> Self {
        let Some(platform) = self.platform.clone() else {
            return self;
        };
        let rtc = platform
            .lock()
            .map(|platform| (Arc::clone(platform.rtc.interrupts()), platform.rtc.clock()));
        let Ok((interrupts, clock)) = rtc else {
            tracing::error!("platform lock is poisoned; the RTC will not interrupt");
            return self;
        };
        interrupts.connect(line);
        match crate::rtc::RtcTimer::start(interrupts, clock) {
            Ok(timer) => self.rtc_timer = Some(Arc::new(timer)),
            Err(error) => {
                tracing::error!(%error, "could not start the RTC timer; the RTC will not interrupt")
            }
        }
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

    /// The PCI bus, shared: for something that outlives a borrow of the
    /// machine bus (the BIOS's VBE display).
    pub fn pci_shared(&self) -> Option<Arc<VirtioPciBus>> {
        self.pci.clone()
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

    /// Every reason a snapshot taken now would lose state a resumed guest
    /// would notice (ADR-0006), empty when there is none. Asked before
    /// [`Self::save_state`], so a refused snapshot writes nothing.
    pub fn snapshot_refusals(&self) -> Vec<String> {
        let mut refusals = self.virtio.snapshot_refusals();
        if let Some(pci) = &self.pci {
            refusals.extend(pci.snapshot_refusals());
        }
        refusals
    }

    pub fn gpu_checkpoint_slots(&self) -> Result<Vec<u32>, crate::state::StateError> {
        match &self.pci {
            Some(pci) => pci.gpu_checkpoint_slots(),
            None => self.virtio.gpu_checkpoint_slots(),
        }
    }

    pub fn prepare_snapshot(&self) -> Result<(), crate::state::StateError> {
        match &self.pci {
            Some(pci) => pci.prepare_snapshot(),
            None => self.virtio.prepare_snapshot(),
        }
    }

    pub fn save_streamed_state(
        &self,
        slot: u32,
        out: &mut dyn std::io::Write,
    ) -> Result<(), crate::state::StateError> {
        match &self.pci {
            Some(pci) => pci.save_streamed_state(slot, out),
            None => self.virtio.save_streamed_state(slot, out),
        }
    }

    pub fn load_streamed_state(
        &self,
        slot: u32,
        reader: &mut dyn virtio_core::StateReader,
    ) -> Result<(), crate::state::StateError> {
        match &self.pci {
            Some(pci) => pci.load_streamed_state(slot, reader),
            None => self.virtio.load_streamed_state(slot, reader),
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
        let mut framebuffer = None;
        let mut hda = None;
        if let Some(pci) = &self.pci {
            virtio = pci.save_state();
            pci_root = pci.save_config();
            framebuffer = pci.save_framebuffer();
            hda = pci.save_hda();
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
            framebuffer,
            hda,
            keyboard: self
                .keyboard
                .as_ref()
                .and_then(|keyboard| keyboard.lock().ok().map(|k| k.save())),
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
        match (&self.keyboard, &state.keyboard) {
            (Some(keyboard), Some(saved)) => keyboard
                .lock()
                .map_err(|_| crate::state::StateError::Poisoned("the keyboard controller"))?
                .load(saved)
                .map_err(|_| crate::state::StateError::BadValue {
                    what: "the keyboard controller's state",
                    value: saved.len() as u64,
                })?,
            _ => crate::state::require_same_presence(
                "keyboard controller",
                state.keyboard.as_ref(),
                self.keyboard.is_some(),
            )?,
        }

        match (&self.pci, &state.pci_root) {
            (Some(pci), Some(config)) => {
                pci.load_state(config, &state.virtio)?;
                pci.load_framebuffer(state.framebuffer.as_ref())?;
                pci.load_hda(state.hda.as_ref())?;
            }
            _ => {
                crate::state::require_same_presence(
                    "boot framebuffer",
                    state.framebuffer.as_ref(),
                    false,
                )?;
                self.virtio.load_state(&state.virtio)?;
            }
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
        if let Some(Ok(mut vga)) = self.vga.as_ref().map(|vga| vga.lock()) {
            vga.reset();
        }
        // Text mode again: the window is RAM.
        self.sync_vga_trap(false);
        if let Some(ide) = &self.ide {
            match ide.lock() {
                Ok(mut ide) => ide.reset(),
                Err(_) => tracing::error!("the IDE lock is poisoned; the controller is not reset"),
            }
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
        if let Some(keyboard) = &self.keyboard {
            match keyboard.lock() {
                Ok(mut keyboard) => keyboard.reset(),
                Err(_) => tracing::error!("keyboard lock is poisoned; the 8042 is not reset"),
            }
        }
        self.acpi_pm.reset();
        self.reset.clear();
    }
}

impl ExitHandler for MachineBus {
    fn io_out(&mut self, port: u16, data: &[u8]) {
        if let Some(vga) = &self.vga {
            if crate::vga::contains(port) {
                let wanted = match vga.lock() {
                    Ok(mut vga) => {
                        // `out dx, ax` on an index port writes the data
                        // port too.
                        for (n, &byte) in data.iter().enumerate() {
                            vga.write(port.wrapping_add(n as u16), byte);
                        }
                        Some(vga.wants_trap())
                    }
                    Err(_) => None,
                };
                if let Some(wanted) = wanted {
                    self.sync_vga_trap(wanted);
                }
                return;
            }
        }
        if let Some(ide) = &self.ide {
            if ide::Controller::contains(port) {
                match ide.lock() {
                    Ok(mut ide) => ide.io_write(port, data),
                    Err(_) => tracing::error!("the IDE lock is poisoned; dropping guest write"),
                }
                return;
            }
        }
        if let Some(bios) = &self.bios {
            if pc_bios::Bios::contains(port) {
                self.bios_write(bios, port, data, None);
                return;
            }
        }
        if let Some(keyboard) = &self.keyboard {
            if crate::i8042::I8042::contains(port) {
                let pulse = match keyboard.lock() {
                    Ok(mut keyboard) => data.iter().any(|&b| keyboard.io_write(port, b)),
                    Err(_) => false,
                };
                if pulse {
                    self.reset.io_write(
                        crate::reset::PORT_KBD_COMMAND,
                        &[crate::reset::KBD_PULSE_RESET],
                    );
                }
                return;
            }
        }
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

    /// A BIOS call needs to know how its caller translates addresses: one
    /// made from virtual 8086 mode (Windows calling the video BIOS) names
    /// its buffers by linear addresses under the guest's page tables.
    fn io_out_from(&mut self, port: u16, data: &[u8], cpu: &dyn vmm_core::hv::GuestCpu) {
        if let Some(bios) = &self.bios {
            if port == pc_bios::rom::PORT_CALL {
                const CR0_PG: u64 = 1 << 31;
                const CR4_PAE: u64 = 1 << 5;
                let paging =
                    cpu.paging()
                        .filter(|p| p.cr0 & CR0_PG != 0)
                        .map(|p| pc_bios::Paging {
                            cr3: p.cr3,
                            pae: p.cr4 & CR4_PAE != 0,
                        });
                self.bios_write(bios, port, data, paging);
                return;
            }
        }
        self.io_out(port, data);
    }

    fn io_in(&mut self, port: u16, data: &mut [u8]) {
        if let Some(vga) = &self.vga {
            if crate::vga::contains(port) {
                match vga.lock() {
                    Ok(mut vga) => {
                        for (n, byte) in data.iter_mut().enumerate() {
                            *byte = vga.read(port.wrapping_add(n as u16));
                        }
                    }
                    Err(_) => data.fill(0xff),
                }
                return;
            }
        }
        if let Some(ide) = &self.ide {
            if ide::Controller::contains(port) {
                match ide.lock() {
                    Ok(mut ide) => ide.io_read(port, data),
                    Err(_) => data.fill(0xff),
                }
                return;
            }
        }
        if let Some(bios) = &self.bios {
            if pc_bios::Bios::contains(port) {
                match bios.lock() {
                    Ok(mut bios) => bios.io_in(port, data),
                    Err(_) => data.fill(0xff),
                }
                return;
            }
        }
        if let Some(keyboard) = &self.keyboard {
            if crate::i8042::I8042::contains(port) {
                if let Ok(mut keyboard) = keyboard.lock() {
                    for byte in data.iter_mut() {
                        *byte = keyboard.io_read(port);
                    }
                    return;
                }
            }
        }
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
        if let Some(vga) = self.vga_memory(addr) {
            if let Ok(mut vga) = vga.lock() {
                for (n, &byte) in data.iter().enumerate() {
                    vga.mem_write(addr.wrapping_add(n as u64), byte);
                }
            }
            return;
        }
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
        if let Some(vga) = self.vga_memory(addr) {
            match vga.lock() {
                Ok(mut vga) => {
                    for (n, byte) in data.iter_mut().enumerate() {
                        *byte = vga.mem_read(addr.wrapping_add(n as u64));
                    }
                }
                Err(_) => data.fill(0xff),
            }
            return;
        }
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

    /// The userspace IOAPIC's EOI listeners — the 8254's lost-tick
    /// re-injection. Only WHP reports EOIs; on KVM the in-kernel irqchip keeps
    /// them and this is never called.
    fn apic_eoi(&mut self, vector: u8) {
        if let Some(irqchip) = &self.irqchip {
            irqchip.eoi(vector);
        }
    }

    /// The userspace 8259 pair's `INTR` line (ADR-0012); never set on a
    /// machine that did not enable legacy interrupts.
    fn ext_interrupt_pending(&self) -> bool {
        self.irqchip
            .as_ref()
            .is_some_and(|irqchip| irqchip.ext_interrupt_pending())
    }

    fn ext_interrupt_ack(&mut self) -> Option<u8> {
        self.irqchip.as_ref()?.ext_interrupt_ack()
    }
}
