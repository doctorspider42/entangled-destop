//! virtio-mmio device slots on the x86-64 machine (backlog MVP-301/306).
//!
//! Each device gets one 4 KiB MMIO page from
//! [`layout::virtio_mmio_slot`] and one IOAPIC pin starting at
//! [`layout::VIRTIO_MMIO_FIRST_IRQ`]. The interrupt line is an `EventFd`
//! registered with KVM as an **irqfd**, so a device raising its interrupt never
//! round-trips through userspace.
//!
//! Slots are announced to the guest kernel through `virtio_mmio.device=`
//! clauses on the command line (there is no PCI bus to enumerate). The order of
//! the clauses is the order the kernel probes the devices in, which is what
//! makes the first `[[disk]]` show up as `/dev/vda`, the second as `/dev/vdb`
//! and so on — so [`VirtioMmioBus::attach`] must preserve caller order.
//!
//! Queue kicks are offloaded to ioeventfds and per-device worker threads by
//! default; see [`crate::notify`] (backlog MVP-307).
//!
//! # Two hosts, one bus
//!
//! Everything above is host-neutral: the addresses come from [`crate::layout`],
//! the transport from `virtio_core`, and a device's interrupt is an
//! `Arc<dyn IrqLine>` that cannot tell an irqfd from an IOAPIC redirection-table
//! lookup. Only the two *wiring* primitives differ, so there are two
//! constructors:
//!
//! * [`VirtioMmioBus::attach`] / [`VirtioMmioBus::attach_with`] — KVM: an irqfd
//!   per device and (by default) an ioeventfd per queue.
//! * [`VirtioMmioBus::attach_userspace`] — a host with no in-kernel irqchip
//!   (WHP): lines come from [`crate::irqchip::UserspaceIrqChip`]'s IOAPIC and
//!   every kick runs inline on the vCPU thread.

use std::sync::{Arc, Mutex};

#[cfg(target_os = "linux")]
use kvm_ioctls::VmFd;
use thiserror::Error;
use virtio_core::device::DeviceType;
use virtio_core::interrupt::IrqLine;
use virtio_core::transport::TransportError;
use virtio_core::{mmio, GuestMem, MmioTransport, VirtioDevice};

use crate::irqchip::{IrqChipError, UserspaceIrqChip};
#[cfg(target_os = "linux")]
use crate::irqfd::{IrqFdError, IrqFdLine};
use crate::layout;
#[cfg(target_os = "linux")]
use crate::notify::{DeviceNotifier, NotifyAddressing, NotifyError, QueueNotifyMode};

/// Maximum number of virtio-mmio devices.
///
/// Bounded by the IOAPIC's 24 pins: pins below
/// [`layout::VIRTIO_MMIO_FIRST_IRQ`] belong to legacy ISA devices (the serial
/// console uses 4). The MVP device set needs six at most (two disks, net, gpu,
/// keyboard, pointer), so eight leaves head-room without crowding the pin
/// space.
pub const MAX_VIRTIO_SLOTS: usize = 8;

#[derive(Debug, Error)]
pub enum VirtioAttachError {
    #[error(
        "too many virtio-mmio devices: {count} requested, only {MAX_VIRTIO_SLOTS} slots exist"
    )]
    TooManySlots { count: usize },

    #[cfg(target_os = "linux")]
    #[error("failed to wire the interrupt line for virtio slot {slot}: {source}")]
    Irq {
        slot: usize,
        #[source]
        source: IrqFdError,
    },

    #[error("failed to wire the IOAPIC line for virtio slot {slot}: {source}")]
    IrqChip {
        slot: usize,
        #[source]
        source: IrqChipError,
    },

    #[error("virtio slot {slot}: {source}")]
    Transport {
        slot: usize,
        #[source]
        source: TransportError,
    },

    #[cfg(target_os = "linux")]
    #[error(transparent)]
    Notify(#[from] NotifyError),
}

/// One attached virtio-mmio device.
pub struct VirtioMmioSlot {
    /// Guest physical base address of the device's 4 KiB register window.
    pub base: u64,
    /// GSI the device's interrupt is wired to.
    pub irq: u32,
    /// The transport, shared with every vCPU thread that may take an exit here
    /// and with the device's queue worker thread.
    pub transport: Arc<Mutex<MmioTransport>>,
    /// Present when this device's queue kicks are served by ioeventfds and a
    /// worker thread (MVP-307); `None` means every kick runs inline on the vCPU.
    #[cfg(target_os = "linux")]
    notifier: Option<DeviceNotifier<MmioTransport>>,
}

impl VirtioMmioSlot {
    /// The queue-notify offload for this device, if it has one.
    #[cfg(target_os = "linux")]
    pub fn notifier(&self) -> Option<&DeviceNotifier<MmioTransport>> {
        self.notifier.as_ref()
    }
}

/// The machine's virtio-mmio window: address decoding plus the guest cmdline
/// clauses that announce it.
pub struct VirtioMmioBus {
    slots: Vec<VirtioMmioSlot>,
    #[cfg(target_os = "linux")]
    mode: QueueNotifyMode,
}

/// Slot placement: the mmio window base and the IOAPIC pin for slot `index`.
///
/// Pure arithmetic over [`crate::layout`], shared by both constructors so a
/// device lands in the same place — and on the same pin — whichever host wired
/// it. The pin table is shorter than the slot count is bounded by, so an
/// out-of-range slot is reported here rather than producing a device on a pin
/// nothing routes.
fn placement(index: usize) -> Result<(u64, u32), VirtioAttachError> {
    let base = layout::virtio_mmio_slot(index as u64);
    // Not `first + index`: the pins that skips are ones this machine's own
    // legacy devices own (see `layout::VIRTIO_IRQS`).
    let gsi =
        layout::virtio_irq(index).ok_or(VirtioAttachError::TooManySlots { count: index + 1 })?;
    Ok((base, gsi))
}

/// Wraps `device` in a transport bound to `line`, reporting its type for the log
/// record the callers write.
fn transport_for(
    index: usize,
    device: Box<dyn VirtioDevice>,
    mem: Arc<GuestMem>,
    line: Arc<dyn IrqLine>,
) -> Result<(DeviceType, Arc<Mutex<MmioTransport>>), VirtioAttachError> {
    let device_type = device.device_type();
    let transport = MmioTransport::new(index, device, mem, line).map_err(|source| {
        VirtioAttachError::Transport {
            slot: index,
            source,
        }
    })?;
    Ok((device_type, Arc::new(Mutex::new(transport))))
}

impl VirtioMmioBus {
    /// A machine with no virtio devices.
    pub fn empty() -> Self {
        Self {
            slots: Vec::new(),
            #[cfg(target_os = "linux")]
            mode: QueueNotifyMode::Synchronous,
        }
    }

    /// Places `devices` in consecutive mmio slots on a host whose hypervisor has
    /// **no in-kernel interrupt controllers** — WHP (backlog WHP-1703).
    ///
    /// The difference from [`Self::attach_with`] is only the two host primitives:
    ///
    /// * the interrupt line is [`crate::irqchip::UserspaceIrqChip::virtio_line`],
    ///   an IOAPIC redirection-table lookup followed by one `WHvRequestInterrupt`,
    ///   instead of an irqfd. Both are an `Arc<dyn IrqLine>`, so the transport and
    ///   the device cannot tell which they got;
    /// * there is no ioeventfd, so every `QUEUE_NOTIFY` write stays a full VM exit
    ///   and the device runs **inline on the vCPU thread** that took it. That is
    ///   [`QueueNotifyMode::Synchronous`], which the KVM path has always supported
    ///   as a fallback — correct, just serialised against guest execution.
    ///
    /// Portable on purpose: it compiles and is exercised on Linux too, which is
    /// what keeps the unit tests for it running in CI on both hosts.
    pub fn attach_userspace(
        mem: Arc<GuestMem>,
        devices: Vec<Box<dyn VirtioDevice>>,
        irqchip: &UserspaceIrqChip,
    ) -> Result<Self, VirtioAttachError> {
        if devices.len() > MAX_VIRTIO_SLOTS {
            return Err(VirtioAttachError::TooManySlots {
                count: devices.len(),
            });
        }
        let mut bus = Self::empty();
        bus.slots.reserve(devices.len());
        for (slot, device) in devices.into_iter().enumerate() {
            let (base, gsi) = placement(slot)?;
            let line = irqchip
                .virtio_line(slot)
                .map_err(|source| VirtioAttachError::IrqChip { slot, source })?;
            let (device_type, transport) = transport_for(slot, device, Arc::clone(&mem), line)?;
            tracing::info!(
                slot,
                device = ?device_type,
                base = format_args!("{base:#x}"),
                irq = gsi,
                "attached virtio-mmio device on the userspace irqchip (synchronous kicks)"
            );
            bus.slots.push(VirtioMmioSlot {
                base,
                irq: gsi,
                transport,
                #[cfg(target_os = "linux")]
                notifier: None,
            });
        }
        Ok(bus)
    }

    /// Places `devices` in consecutive mmio slots, registering one irqfd per
    /// device and (by default) one queue-notify ioeventfd per queue. Slot *n*
    /// keeps the position `devices[n]` had, because that is what determines the
    /// guest's device naming.
    ///
    /// The notification mode comes from [`QueueNotifyMode::from_env`], so a host
    /// where the offload misbehaves can be put back on the synchronous path
    /// without a rebuild; [`Self::attach_with`] pins it explicitly.
    #[cfg(target_os = "linux")]
    pub fn attach(
        vm: Arc<VmFd>,
        mem: Arc<GuestMem>,
        devices: Vec<Box<dyn VirtioDevice>>,
    ) -> Result<Self, VirtioAttachError> {
        Self::attach_with(vm, mem, devices, QueueNotifyMode::from_env())
    }

    /// [`Self::attach`] with an explicit queue-notify mode (benchmarks, tests).
    #[cfg(target_os = "linux")]
    pub fn attach_with(
        vm: Arc<VmFd>,
        mem: Arc<GuestMem>,
        devices: Vec<Box<dyn VirtioDevice>>,
        mode: QueueNotifyMode,
    ) -> Result<Self, VirtioAttachError> {
        if devices.len() > MAX_VIRTIO_SLOTS {
            return Err(VirtioAttachError::TooManySlots {
                count: devices.len(),
            });
        }
        // Built up as we go so that an error part way through drops the slots
        // already created, which stops their workers and deassigns their fds.
        let mut bus = Self {
            slots: Vec::with_capacity(devices.len()),
            mode,
        };
        for (slot, mut device) in devices.into_iter().enumerate() {
            let (base, gsi) = placement(slot)?;
            let line = IrqFdLine::new(&vm, gsi)
                .map_err(|source| VirtioAttachError::Irq { slot, source })?;
            // Handed over before the device disappears into its transport; the
            // real waker (the worker's queue-0 eventfd) does not exist yet, so
            // it is filled in below (see `virtio_core::DeferredWaker`).
            let waker = virtio_core::DeferredWaker::new();
            device.set_host_waker(Arc::clone(&waker) as Arc<dyn virtio_core::HostWaker>);
            let (device_type, transport) =
                transport_for(slot, device, Arc::clone(&mem), Arc::new(line))?;

            let notifier = if mode.is_offloaded() {
                // All of a device's queues share one QUEUE_NOTIFY register, so
                // KVM tells them apart by a datamatch on the queue index.
                let addressing = NotifyAddressing::SharedWithDatamatch {
                    addr: base.saturating_add(mmio::QUEUE_NOTIFY),
                };
                DeviceNotifier::attach(Arc::clone(&vm), slot, addressing, &transport)?
            } else {
                None
            };
            if let Some(host_waker) = notifier.as_ref().and_then(|n| n.waker()) {
                waker.install(host_waker);
            }

            tracing::info!(
                slot,
                device = ?device_type,
                base = format_args!("{base:#x}"),
                irq = gsi,
                offloaded_queues = notifier.as_ref().map_or(0, |n| n.offloaded_queues().len()),
                "attached virtio-mmio device"
            );
            bus.slots.push(VirtioMmioSlot {
                base,
                irq: gsi,
                transport,
                notifier,
            });
        }
        Ok(bus)
    }

    pub fn slots(&self) -> &[VirtioMmioSlot] {
        &self.slots
    }

    /// Shares the VM's pause gate with every device in the window (ADR-0005).
    pub fn set_quiesce(&self, quiesce: Arc<virtio_core::Quiesce>) {
        for slot in &self.slots {
            match slot.transport.lock() {
                Ok(mut transport) => transport.set_quiesce(Arc::clone(&quiesce)),
                Err(_) => tracing::error!(
                    base = format_args!("{:#x}", slot.base),
                    "virtio-mmio transport lock is poisoned; this device will not pause"
                ),
            }
        }
        #[cfg(target_os = "linux")]
        for slot in &self.slots {
            if let Some(notifier) = &slot.notifier {
                notifier.set_quiesce(Arc::clone(&quiesce));
            }
        }
    }

    /// Machine reset (ADR-0005): every transport and device back to power-on.
    ///
    /// Simpler than the PCI bus's, and for a structural reason worth recording:
    /// a virtio-mmio slot's window is at a *fixed* address the host chose and
    /// announced on the kernel command line, so there is no guest-movable BAR,
    /// no configuration space and nothing for the queue-notify registrations to
    /// follow. The whole of a slot's guest-visible state is its
    /// `TransportState`.
    pub fn reset(&self) {
        for slot in &self.slots {
            match slot.transport.lock() {
                Ok(mut transport) => transport.power_on_reset(),
                Err(_) => tracing::error!(
                    base = format_args!("{:#x}", slot.base),
                    "virtio-mmio transport lock is poisoned; this device is not reset"
                ),
            }
        }
    }

    pub fn is_empty(&self) -> bool {
        self.slots.is_empty()
    }

    /// How queue kicks reach the devices on this bus.
    #[cfg(target_os = "linux")]
    pub fn notify_mode(&self) -> QueueNotifyMode {
        self.mode
    }

    /// Stops every queue worker thread and deassigns their ioeventfds.
    ///
    /// Idempotent, and also run from `Drop`, so "closing the VM leaves no
    /// device threads behind" holds even on an error path that never gets here
    /// (EPIC 14 acceptance criterion). A bus whose kicks are synchronous owns no
    /// threads and no registrations, so there is nothing to undo.
    pub fn shutdown(&self) {
        #[cfg(target_os = "linux")]
        for slot in &self.slots {
            if let Some(notifier) = &slot.notifier {
                notifier.shutdown();
            }
        }
    }

    /// The `virtio_mmio.device=` clauses announcing every slot, in probe order.
    pub fn cmdline_clauses(&self) -> String {
        self.slots
            .iter()
            .map(|slot| mmio::cmdline_clause(slot.base, slot.irq))
            .collect::<Vec<_>>()
            .join(" ")
    }

    /// Decodes a guest physical address into the slot it belongs to and the
    /// register offset inside that slot's window.
    pub fn locate(&self, addr: u64) -> Option<(&VirtioMmioSlot, u64)> {
        let offset_in_window = addr.checked_sub(layout::VIRTIO_MMIO_BASE)?;
        let index = usize::try_from(offset_in_window / layout::VIRTIO_MMIO_SLOT_SIZE).ok()?;
        let slot = self.slots.get(index)?;
        Some((slot, offset_in_window % layout::VIRTIO_MMIO_SLOT_SIZE))
    }
}

impl Drop for VirtioMmioBus {
    fn drop(&mut self) {
        self.shutdown();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::irqchip::UserspaceIrqChip;
    use std::sync::atomic::{AtomicU32, Ordering};
    use virtio_core::{DeviceType, VIRTIO_F_VERSION_1};
    use vmm_core::hv::{HvError, InterruptDelivery, InterruptRequest};

    /// Counts `InterruptDelivery::request` calls and remembers the last vector,
    /// standing in for WHP's local APIC. The IOAPIC above it is the real thing.
    #[derive(Default)]
    struct Counting {
        calls: AtomicU32,
        last_vector: AtomicU32,
    }

    impl InterruptDelivery for Counting {
        fn request(&self, interrupt: &InterruptRequest) -> Result<(), HvError> {
            self.calls.fetch_add(1, Ordering::AcqRel);
            self.last_vector
                .store(u32::from(interrupt.vector), Ordering::Release);
            Ok(())
        }
    }

    /// A device that exists only to be placed in a slot and asked what it is.
    struct IdentityOnly(DeviceType);

    impl VirtioDevice for IdentityOnly {
        fn device_type(&self) -> DeviceType {
            self.0
        }
        fn queue_max_sizes(&self) -> &[u16] {
            &[256]
        }
        fn device_features(&self) -> u64 {
            VIRTIO_F_VERSION_1
        }
        fn ack_features(&mut self, _negotiated: u64) -> bool {
            true
        }
        fn read_config(&self, _offset: u64, _data: &mut [u8]) {}
        fn write_config(&mut self, _offset: u64, _data: &[u8]) {}
        fn activate(
            &mut self,
            _resources: virtio_core::DeviceResources,
        ) -> Result<(), virtio_core::DeviceError> {
            Ok(())
        }
        fn notify(&mut self, _queue_index: u16) -> Result<(), virtio_core::DeviceError> {
            Ok(())
        }
        fn reset(&mut self) {}
    }

    fn chip(delivery: Arc<Counting>) -> Arc<UserspaceIrqChip> {
        UserspaceIrqChip::new(delivery, 1).expect("userspace irqchip")
    }

    fn devices(count: usize) -> Vec<Box<dyn VirtioDevice>> {
        (0..count)
            .map(|_| Box::new(IdentityOnly(DeviceType::Block)) as Box<dyn VirtioDevice>)
            .collect()
    }

    fn memory() -> Arc<GuestMem> {
        Arc::new(virtio_core::testing::guest_memory(1 << 20))
    }

    /// The userspace-irqchip attach path must place devices exactly where the KVM
    /// one does — same window, same pin, same clause order — or a WHP guest and a
    /// KVM guest see two different machines from the same configuration.
    #[test]
    fn attach_userspace_places_devices_where_the_kvm_path_does() {
        let chip = chip(Arc::new(Counting::default()));
        let bus = VirtioMmioBus::attach_userspace(memory(), devices(3), &chip)
            .expect("attach on the userspace irqchip");

        assert_eq!(bus.slots().len(), 3);
        for (index, slot) in bus.slots().iter().enumerate() {
            assert_eq!(slot.base, layout::virtio_mmio_slot(index as u64));
            assert_eq!(Some(slot.irq), layout::virtio_irq(index));
            // The pins the table skips are the RTC's and the SCI's; a device must
            // never land on one.
            assert_ne!(slot.irq, 8);
            assert_ne!(slot.irq, layout::ACPI_SCI_GSI);
        }
        assert_eq!(
            bus.cmdline_clauses(),
            format!(
                "{} {} {}",
                mmio::cmdline_clause(layout::virtio_mmio_slot(0), layout::VIRTIO_IRQS[0]),
                mmio::cmdline_clause(layout::virtio_mmio_slot(1), layout::VIRTIO_IRQS[1]),
                mmio::cmdline_clause(layout::virtio_mmio_slot(2), layout::VIRTIO_IRQS[2]),
            )
        );
    }

    /// The whole dispatch path a WHP MMIO exit takes: the instruction emulator's
    /// memory callback lands on `ExitHandler::mmio_read`/`mmio_write`, which is
    /// `MachineBus`, which decodes the address to a slot and a register offset.
    /// Asserted here rather than in the WHP backend because everything from the
    /// callback inwards is portable — and so is testable on both hosts.
    #[test]
    fn the_bus_dispatches_the_virtio_window_to_the_right_slot() {
        use crate::bus::MachineBus;
        use crate::serial::SerialConsole;
        use vmm_core::ExitHandler;

        let chip = chip(Arc::new(Counting::default()));
        let bus = VirtioMmioBus::attach_userspace(memory(), devices(2), &chip)
            .expect("attach on the userspace irqchip");
        let serial = SerialConsole::with_trigger(chip.serial_line(), Box::new(std::io::sink()));
        let mut bus = MachineBus::with_virtio(serial, bus).with_irqchip(Arc::clone(&chip));

        let mut word = [0u8; 4];
        for slot in 0..2u64 {
            let base = layout::virtio_mmio_slot(slot);
            bus.mmio_read(base + mmio::MAGIC_VALUE, &mut word);
            assert_eq!(u32::from_le_bytes(word), mmio::MAGIC, "slot {slot} magic");
            bus.mmio_read(base + mmio::VERSION_REG, &mut word);
            assert_eq!(u32::from_le_bytes(word), mmio::VERSION);
            bus.mmio_read(base + mmio::DEVICE_ID, &mut word);
            assert_eq!(u32::from_le_bytes(word), DeviceType::Block.id());
        }

        // A slot the bus does not have reads as zeroes rather than reaching a
        // neighbour's registers.
        bus.mmio_read(layout::virtio_mmio_slot(4) + mmio::MAGIC_VALUE, &mut word);
        assert_eq!(u32::from_le_bytes(word), 0);
        // The IOAPIC's own page must still win over the virtio window: the two
        // are far apart, but the ordering inside the bus is what enforces it.
        bus.mmio_read(u64::from(layout::IOAPIC_ADDR), &mut word);
        assert_ne!(u32::from_le_bytes(word), mmio::MAGIC);
    }

    /// A device's interrupt line on this host is an IOAPIC pin, and raising it
    /// must produce exactly one delivered message carrying the vector the *guest*
    /// programmed into the redirection entry for that pin.
    ///
    /// This is the WHP peer of "an irqfd write injects the GSI", and the reason
    /// the transport can take either without knowing which.
    #[test]
    fn a_device_line_delivers_the_vector_the_guest_programmed() {
        let delivery = Arc::new(Counting::default());
        let chip = chip(Arc::clone(&delivery));
        let _bus = VirtioMmioBus::attach_userspace(memory(), devices(1), &chip)
            .expect("attach on the userspace irqchip");

        // What Linux does when it requests the IRQ: select the low half of the
        // redirection entry for the pin and write vector, unmasked.
        let pin = u32::from(u8::try_from(layout::VIRTIO_IRQS[0]).unwrap());
        let base = u64::from(layout::IOAPIC_ADDR);
        chip.mmio_write(base, &(0x10 + 2 * pin).to_le_bytes());
        chip.mmio_write(base + 0x10, &0x43u32.to_le_bytes());

        let before = chip.ioapic().delivered();
        chip.virtio_line(0)
            .expect("slot 0 has a pin")
            .trigger()
            .expect("delivery succeeds");
        assert_eq!(chip.ioapic().delivered(), before + 1);
        assert_eq!(delivery.last_vector.load(Ordering::Acquire), 0x43);
    }

    /// More devices than there are pins must be refused rather than silently
    /// dropped or stacked onto one pin.
    #[test]
    fn more_devices_than_slots_is_refused() {
        let chip = chip(Arc::new(Counting::default()));
        let attached =
            VirtioMmioBus::attach_userspace(memory(), devices(MAX_VIRTIO_SLOTS + 1), &chip);
        let Err(error) = attached else {
            panic!("nine devices must not fit in {MAX_VIRTIO_SLOTS} slots");
        };
        assert!(matches!(error, VirtioAttachError::TooManySlots { count } if count == 9));
    }

    #[test]
    fn empty_bus_decodes_nothing() {
        let bus = VirtioMmioBus::empty();
        assert!(bus.is_empty());
        assert!(bus.locate(layout::VIRTIO_MMIO_BASE).is_none());
        assert_eq!(bus.cmdline_clauses(), "");
    }

    /// Address decoding is pure arithmetic, so it can be tested without KVM by
    /// building the slot list directly.
    fn fake_bus(count: usize) -> Vec<(u64, u32)> {
        (0..count)
            .map(|n| {
                (
                    layout::virtio_mmio_slot(n as u64),
                    layout::virtio_irq(n).expect("fewer slots than the pin table holds"),
                )
            })
            .collect()
    }

    #[test]
    fn slots_are_page_sized_and_consecutive() {
        let slots = fake_bus(3);
        assert_eq!(slots[0].0, layout::VIRTIO_MMIO_BASE);
        assert_eq!(
            slots[1].0,
            layout::VIRTIO_MMIO_BASE + layout::VIRTIO_MMIO_SLOT_SIZE
        );
        assert_eq!(slots[2].1, layout::VIRTIO_IRQS[2]);
        // Every slot stays inside the 32-bit MMIO hole and clear of RAM.
        for (base, _) in &slots {
            assert!(*base >= layout::MMIO_HOLE_START);
        }
    }

    #[test]
    fn cmdline_clause_order_matches_slot_order() {
        let clauses: Vec<String> = fake_bus(2)
            .into_iter()
            .map(|(base, irq)| mmio::cmdline_clause(base, irq))
            .collect();
        assert_eq!(
            clauses.join(" "),
            format!(
                "virtio_mmio.device=4K@{:#x}:{} virtio_mmio.device=4K@{:#x}:{}",
                layout::VIRTIO_MMIO_BASE,
                layout::VIRTIO_IRQS[0],
                layout::VIRTIO_MMIO_BASE + layout::VIRTIO_MMIO_SLOT_SIZE,
                layout::VIRTIO_IRQS[1]
            )
        );
    }
}
