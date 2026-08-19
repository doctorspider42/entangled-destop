//! virtio devices on the PCI root bus (EPIC 19), the second transport.
//!
//! The mmio counterpart is [`crate::virtio::VirtioMmioBus`]; this module is the
//! same job for [`virtio_core::PciTransport`], and the differences are all
//! consequences of PCI being *enumerable*:
//!
//! * **No kernel command line.** A virtio-mmio device only exists because a
//!   `virtio_mmio.device=` clause told the guest where to look, which is why
//!   [`crate::virtio::VirtioMmioBus::attach`] must preserve caller order — the
//!   clause order is the probe order and therefore decides `/dev/vda` versus
//!   `/dev/vdb`. Here the guest walks the bus, so device *numbers* (dense from
//!   `00:01.0` upwards) take that role instead. Caller order is still preserved,
//!   for the same reason.
//! * **Two address spaces per device.** Configuration accesses arrive as port
//!   I/O on `0xcf8`/`0xcfc` and are served by [`crate::pci::PciRoot`]; register
//!   accesses arrive as MMIO inside the device's BAR window. Both are dispatched
//!   from [`crate::bus::MachineBus`].
//! * **The driver decides when the device decodes.** Until the guest sets the
//!   memory-space-enable bit in the command register, the BAR window is not
//!   claimed at all — [`crate::pci::PciRoot::locate_mmio`] enforces that, so a
//!   pre-`pci_enable_device` access reads zeroes rather than reaching a device.
//!
//! # Interrupts
//!
//! One IOAPIC pin per device, from [`layout::PCI_FIRST_IRQ`], published to the
//! guest in the `interrupt_line` config register and raised through a KVM irqfd
//! — the same mechanism, and the same known limitation, as the mmio bus: the
//! injection is an **edge** on an ISA-style pin, not a level-triggered PCI
//! `INTA#`. Two consequences, both deliberate:
//!
//! * pins are never shared. One device per pin means the ISR byte does not have
//!   to deassert anything, which an edge injection could not model anyway.
//!   [`crate::pci::MAX_PCI_DEVICES`] and the pin space bound each other.
//! * `INTX_DISABLE` in the command register is honoured: [`IntxLine`] checks the
//!   flag the config space maintains, so `pci_intx(dev, 0)` really does stop the
//!   injections instead of leaving the guest with spurious interrupts.
//!
//! Without ACPI or a `$PIR` table Linux takes a PCI device's IRQ straight from
//! `interrupt_line`, and `crate::mptable` routes those ISA pins to the IOAPIC —
//! that pairing is what makes INTx work here at all.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use kvm_ioctls::VmFd;
use thiserror::Error;
use virtio_core::interrupt::{InterruptError, IrqLine};
use virtio_core::pci as vpci;
use virtio_core::transport::TransportError;
use virtio_core::{GuestMem, PciTransport, VirtioDevice};
use vmm_sys_util::eventfd::{EventFd, EFD_NONBLOCK};

use crate::layout;
use crate::notify::{DeviceNotifier, NotifyAddressing, NotifyError, QueueNotifyMode};
use crate::pci::{ConfigSpace, PciError, PciRoot};

#[derive(Debug, Error)]
pub enum VirtioPciAttachError {
    #[error("failed to create the interrupt eventfd for PCI slot {slot}: {source}")]
    EventFd {
        slot: usize,
        #[source]
        source: std::io::Error,
    },

    #[error("failed to register the irqfd for PCI slot {slot} (GSI {gsi}): {source}")]
    Irqfd {
        slot: usize,
        gsi: u32,
        #[source]
        source: kvm_ioctls::Error,
    },

    #[error("PCI slot {slot}: {source}")]
    Transport {
        slot: usize,
        #[source]
        source: TransportError,
    },

    #[error("PCI slot {slot}: {source}")]
    Bus {
        slot: usize,
        #[source]
        source: PciError,
    },

    #[error(transparent)]
    Notify(#[from] NotifyError),
}

/// A device's INTx line: an `EventFd` registered with KVM as an irqfd, gated on
/// the guest not having set `INTX_DISABLE`.
///
/// Triggering injects the interrupt entirely inside the kernel. See the module
/// docs for why this is an edge on an ISA-style pin rather than a level-triggered
/// `INTA#`, and `crate::virtio::IrqFdLine` for the interrupt-topology defect both
/// buses share.
pub struct IntxLine {
    event: EventFd,
    /// Mirrors the command register's `INTX_DISABLE` bit, maintained by the
    /// config space. Checked on every injection rather than snapshotted, because
    /// a driver may disable INTx at any time — while switching to polling, or on
    /// its way to being unbound.
    enabled: Arc<AtomicBool>,
}

impl IrqLine for IntxLine {
    fn trigger(&self) -> Result<(), InterruptError> {
        if !self.enabled.load(Ordering::Acquire) {
            // Not an error: the driver asked for silence. The ISR bit stays set,
            // so a driver that polls still sees why the device wanted attention.
            tracing::trace!("INTx is disabled by the guest; not raising the line");
            return Ok(());
        }
        self.event
            .write(1)
            .map_err(|e| InterruptError::Signal(e.to_string()))
    }
}

/// One attached virtio-pci device.
pub struct VirtioPciSlot {
    /// PCI device number on bus 0 (`00:<device>.0`).
    pub device_number: u8,
    /// Guest physical base of the device's BAR window, as the host assigned it.
    pub bar_base: u64,
    /// GSI the device's INTx line is wired to.
    pub irq: u32,
    /// The transport, shared with every vCPU thread that may take an exit into
    /// the BAR and with the device's queue worker thread.
    pub transport: Arc<Mutex<PciTransport>>,
    /// Present when this device's queue kicks are served by ioeventfds and a
    /// worker thread; `None` means every kick runs inline on the vCPU.
    notifier: Option<DeviceNotifier<PciTransport>>,
}

impl VirtioPciSlot {
    /// The queue-notify offload for this device, if it has one.
    pub fn notifier(&self) -> Option<&DeviceNotifier<PciTransport>> {
        self.notifier.as_ref()
    }
}

/// The machine's PCI bus: configuration space, BAR address decoding, and the
/// virtio transports behind it.
pub struct VirtioPciBus {
    /// Configuration space for every function, host bridge included. Behind a
    /// `Mutex` because a configuration access latches `CONFIG_ADDRESS`, i.e.
    /// even a read mutates, and any vCPU can make one.
    root: Mutex<PciRoot>,
    slots: Vec<VirtioPciSlot>,
    mode: QueueNotifyMode,
}

impl VirtioPciBus {
    /// A bus with the host bridge and no devices.
    pub fn empty() -> Self {
        Self {
            root: Mutex::new(PciRoot::new()),
            slots: Vec::new(),
            mode: QueueNotifyMode::Synchronous,
        }
    }

    /// Places `devices` on consecutive PCI device numbers starting at `00:01.0`,
    /// registering one irqfd per device and (by default) one queue-notify
    /// ioeventfd per queue.
    pub fn attach(
        vm: Arc<VmFd>,
        mem: Arc<GuestMem>,
        devices: Vec<Box<dyn VirtioDevice>>,
    ) -> Result<Self, VirtioPciAttachError> {
        Self::attach_with(vm, mem, devices, QueueNotifyMode::from_env())
    }

    /// [`Self::attach`] with an explicit queue-notify mode (benchmarks, tests).
    pub fn attach_with(
        vm: Arc<VmFd>,
        mem: Arc<GuestMem>,
        devices: Vec<Box<dyn VirtioDevice>>,
        mode: QueueNotifyMode,
    ) -> Result<Self, VirtioPciAttachError> {
        // Built up as we go so that an error part way through drops the slots
        // already created, which stops their workers and deassigns their fds.
        let mut bus = Self {
            root: Mutex::new(PciRoot::new()),
            slots: Vec::with_capacity(devices.len()),
            mode,
        };
        for (slot, device) in devices.into_iter().enumerate() {
            let bar_base = layout::pci_bar_slot(slot as u64);
            let gsi = layout::PCI_FIRST_IRQ + slot as u32;

            let event = EventFd::new(EFD_NONBLOCK)
                .map_err(|source| VirtioPciAttachError::EventFd { slot, source })?;
            vm.register_irqfd(&event, gsi)
                .map_err(|source| VirtioPciAttachError::Irqfd { slot, gsi, source })?;

            // The config space is built first so its INTx flag can gate the
            // line the transport is about to be handed.
            let config = bus.build_config_space(slot, bar_base, gsi, device.as_ref())?;
            let line = Arc::new(IntxLine {
                event,
                enabled: config.intx_flag(),
            });

            let device_type = device.device_type();
            let transport = PciTransport::new(slot, device, Arc::clone(&mem), line)
                .map_err(|source| VirtioPciAttachError::Transport { slot, source })?;
            let transport = Arc::new(Mutex::new(transport));

            let device_number = match bus.root.lock() {
                Ok(mut root) => root
                    .attach(config, slot)
                    .map_err(|source| VirtioPciAttachError::Bus { slot, source })?,
                // Only reachable if a previous panic poisoned it, which cannot
                // have happened yet: nothing else holds this lock during setup.
                Err(_) => {
                    return Err(VirtioPciAttachError::Bus {
                        slot,
                        source: PciError::BusFull,
                    })
                }
            };

            let notifier = if mode.is_offloaded() {
                // Every queue has its own notification address, so the offload
                // needs no datamatch (see `crate::notify`).
                let addressing = NotifyAddressing::PerQueue {
                    base: bar_base.saturating_add(vpci::NOTIFY_CFG_OFFSET),
                    stride: u64::from(vpci::NOTIFY_OFF_MULTIPLIER),
                };
                DeviceNotifier::attach(Arc::clone(&vm), slot, addressing, &transport)?
            } else {
                None
            };

            tracing::info!(
                slot,
                device = ?device_type,
                address = format_args!("00:{device_number:02x}.0"),
                bar = format_args!("{bar_base:#x}"),
                irq = gsi,
                offloaded_queues = notifier.as_ref().map_or(0, |n| n.offloaded_queues().len()),
                "attached virtio-pci device"
            );
            bus.slots.push(VirtioPciSlot {
                device_number,
                bar_base,
                irq: gsi,
                transport,
                notifier,
            });
        }
        Ok(bus)
    }

    /// Builds one device's PCI configuration space: the modern virtio identity,
    /// the single memory BAR the host has reserved for it, its INTx line, and the
    /// four capability records that tell a driver where everything is.
    ///
    /// The identity comes from the transport module rather than from here — the
    /// bus knows about type-0 headers, not about virtio.
    fn build_config_space(
        &self,
        slot: usize,
        bar_base: u64,
        gsi: u32,
        device: &dyn VirtioDevice,
    ) -> Result<ConfigSpace, VirtioPciAttachError> {
        let bus_error = |source| VirtioPciAttachError::Bus { slot, source };
        // The aperture and the BAR size are both host constants far below 4 GiB,
        // and the GSI is one of a handful of low pins.
        let base = u32::try_from(bar_base).map_err(|_| {
            bus_error(PciError::MisalignedBar {
                base: u32::MAX,
                size: 0,
            })
        })?;
        let size = u32::try_from(vpci::VIRTIO_PCI_BAR_SIZE).unwrap_or(u32::MAX);

        let mut config = ConfigSpace::type0(
            vpci::VIRTIO_PCI_VENDOR_ID,
            vpci::device_id(device.device_type()),
            vpci::class_code(device.device_type()),
            vpci::VIRTIO_PCI_REVISION,
        )
        // Linux reads the virtio *vendor* id out of the PCI subsystem vendor.
        .with_subsystem(vpci::VIRTIO_PCI_SUBSYSTEM_VENDOR_ID, 0)
        .with_memory_bar(vpci::VIRTIO_PCI_BAR_INDEX, base, size)
        .map_err(bus_error)?
        .with_interrupt(
            vpci::VIRTIO_PCI_INTERRUPT_PIN,
            u8::try_from(gsi).unwrap_or(u8::MAX),
        );
        for record in vpci::capability_records() {
            config.add_capability(&record).map_err(bus_error)?;
        }
        Ok(config)
    }

    pub fn slots(&self) -> &[VirtioPciSlot] {
        &self.slots
    }

    pub fn is_empty(&self) -> bool {
        self.slots.is_empty()
    }

    /// How queue kicks reach the devices on this bus.
    pub fn notify_mode(&self) -> QueueNotifyMode {
        self.mode
    }

    /// Stops every queue worker thread and deassigns their ioeventfds.
    ///
    /// Idempotent, and also run from `Drop`, so "closing the VM leaves no device
    /// threads behind" holds even on an error path that never gets here.
    pub fn shutdown(&self) {
        for slot in &self.slots {
            if let Some(notifier) = &slot.notifier {
                notifier.shutdown();
            }
        }
    }

    // ------------------------------------------------------------- dispatch

    /// True when `port` is a configuration-mechanism port.
    pub fn claims_port(port: u16) -> bool {
        PciRoot::contains(port)
    }

    /// Guest read from `0xcf8`/`0xcfc`.
    pub fn io_read(&self, port: u16, data: &mut [u8]) {
        match self.root.lock() {
            Ok(root) => root.io_read(port, data),
            Err(_) => {
                tracing::error!(
                    port = format_args!("{port:#x}"),
                    "PCI root lock is poisoned; reading all-ones"
                );
                data.fill(0xff);
            }
        }
    }

    /// Guest write to `0xcf8`/`0xcfc`.
    pub fn io_write(&self, port: u16, data: &[u8]) {
        match self.root.lock() {
            Ok(mut root) => root.io_write(port, data),
            Err(_) => tracing::error!(
                port = format_args!("{port:#x}"),
                "PCI root lock is poisoned; dropping guest write"
            ),
        }
    }

    /// Decodes `addr` into the slot whose BAR claims it and the offset inside
    /// that BAR. `None` when no *enabled* BAR covers it.
    fn locate(&self, addr: u64) -> Option<(&VirtioPciSlot, u64)> {
        let (owner, bar, offset) = match self.root.lock() {
            Ok(root) => root.locate_mmio(addr)?,
            Err(_) => {
                tracing::error!(
                    addr = format_args!("{addr:#x}"),
                    "PCI root lock is poisoned; not decoding"
                );
                return None;
            }
        };
        // Only BAR 0 exists on a virtio function, but a device could in
        // principle grow another one; refusing rather than assuming keeps the
        // dispatch honest.
        if bar != vpci::VIRTIO_PCI_BAR_INDEX {
            return None;
        }
        Some((self.slots.get(owner)?, offset))
    }

    /// Guest MMIO read inside some device's BAR window.
    pub fn mmio_read(&self, addr: u64, data: &mut [u8]) {
        let Some((slot, offset)) = self.locate(addr) else {
            return;
        };
        match slot.transport.lock() {
            Ok(mut transport) => transport.read_bar(offset, data),
            Err(_) => tracing::error!(
                addr = format_args!("{addr:#x}"),
                "virtio-pci transport lock is poisoned; reading zeroes"
            ),
        }
    }

    /// Guest MMIO write inside some device's BAR window.
    pub fn mmio_write(&self, addr: u64, data: &[u8]) {
        let Some((slot, offset)) = self.locate(addr) else {
            return;
        };
        match slot.transport.lock() {
            Ok(mut transport) => transport.write_bar(offset, data),
            Err(_) => tracing::error!(
                addr = format_args!("{addr:#x}"),
                "virtio-pci transport lock is poisoned; dropping guest write"
            ),
        }
    }
}

impl Drop for VirtioPciBus {
    fn drop(&mut self) {
        self.shutdown();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pci;

    /// The host's aperture slots and the transport's BAR must be the same size,
    /// or a device would decode into its neighbour's window (too large) or leave
    /// part of its own register block unreachable (too small).
    #[test]
    fn the_bar_slot_size_matches_the_transport() {
        assert_eq!(layout::PCI_MMIO_SLOT_SIZE, vpci::VIRTIO_PCI_BAR_SIZE);
        assert_eq!(
            layout::PCI_MMIO_SLOTS as usize,
            pci::MAX_PCI_DEVICES - 1,
            "one aperture slot per device, and the host bridge has no BAR"
        );
    }

    /// The identity a driver matches on comes from `virtio_core::pci`, so the
    /// config space and the transport cannot disagree — this pins the values
    /// that reach `lspci` and Linux's `vp_modern_probe`.
    #[test]
    fn the_identity_registers_are_the_transports_own() {
        for (kind, device_id, class) in [
            (virtio_core::DeviceType::Net, 0x1041u16, 0x0200_0000u32),
            (virtio_core::DeviceType::Block, 0x1042, 0x0180_0000),
            (virtio_core::DeviceType::Gpu, 0x1050, 0x0380_0000),
            (virtio_core::DeviceType::Input, 0x1052, 0x0980_0000),
        ] {
            assert_eq!(vpci::device_id(kind), device_id, "{kind:?} device id");
            assert_eq!(vpci::class_code(kind), class, "{kind:?} class code");
        }
    }

    #[test]
    fn an_empty_bus_still_has_a_host_bridge() {
        let bus = VirtioPciBus::empty();
        assert!(bus.is_empty());
        assert!(bus.locate(layout::PCI_MMIO_BASE).is_none());
        // The host bridge answers, which is what makes Linux believe in the bus.
        let address = 0x8000_0000u32;
        bus.io_write(pci::CONFIG_ADDRESS_PORT, &address.to_le_bytes());
        let mut id = [0u8; 4];
        bus.io_read(pci::CONFIG_DATA_PORT, &mut id);
        assert_eq!(u32::from_le_bytes(id) & 0xffff, 0x8086);
    }

    /// A read of an unclaimed BAR address must not touch a transport at all —
    /// this is the path a guest takes before `pci_enable_device`.
    #[test]
    fn undecoded_mmio_is_dropped() {
        let bus = VirtioPciBus::empty();
        let mut data = [0xffu8; 4];
        bus.mmio_read(layout::pci_bar_slot(0), &mut data);
        assert_eq!(data, [0xff; 4], "mmio_read leaves undecoded reads alone");
        bus.mmio_write(layout::pci_bar_slot(0), &[0; 4]);
    }
}
