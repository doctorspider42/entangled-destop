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

use std::sync::{Arc, Mutex};

use kvm_ioctls::VmFd;
use thiserror::Error;
use virtio_core::transport::TransportError;
use virtio_core::{mmio, GuestMem, MmioTransport, VirtioDevice};

use crate::irqfd::{IrqFdError, IrqFdLine};
use crate::layout;
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

    #[error("failed to wire the interrupt line for virtio slot {slot}: {source}")]
    Irq {
        slot: usize,
        #[source]
        source: IrqFdError,
    },

    #[error("virtio slot {slot}: {source}")]
    Transport {
        slot: usize,
        #[source]
        source: TransportError,
    },

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
    notifier: Option<DeviceNotifier<MmioTransport>>,
}

impl VirtioMmioSlot {
    /// The queue-notify offload for this device, if it has one.
    pub fn notifier(&self) -> Option<&DeviceNotifier<MmioTransport>> {
        self.notifier.as_ref()
    }
}

/// The machine's virtio-mmio window: address decoding plus the guest cmdline
/// clauses that announce it.
pub struct VirtioMmioBus {
    slots: Vec<VirtioMmioSlot>,
    mode: QueueNotifyMode,
}

impl VirtioMmioBus {
    /// A machine with no virtio devices.
    pub fn empty() -> Self {
        Self {
            slots: Vec::new(),
            mode: QueueNotifyMode::Synchronous,
        }
    }

    /// Places `devices` in consecutive mmio slots, registering one irqfd per
    /// device and (by default) one queue-notify ioeventfd per queue. Slot *n*
    /// keeps the position `devices[n]` had, because that is what determines the
    /// guest's device naming.
    ///
    /// The notification mode comes from [`QueueNotifyMode::from_env`], so a host
    /// where the offload misbehaves can be put back on the synchronous path
    /// without a rebuild; [`Self::attach_with`] pins it explicitly.
    pub fn attach(
        vm: Arc<VmFd>,
        mem: Arc<GuestMem>,
        devices: Vec<Box<dyn VirtioDevice>>,
    ) -> Result<Self, VirtioAttachError> {
        Self::attach_with(vm, mem, devices, QueueNotifyMode::from_env())
    }

    /// [`Self::attach`] with an explicit queue-notify mode (benchmarks, tests).
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
        for (slot, device) in devices.into_iter().enumerate() {
            let base = layout::virtio_mmio_slot(slot as u64);
            let gsi = layout::VIRTIO_MMIO_FIRST_IRQ + slot as u32;

            let line = IrqFdLine::new(&vm, gsi)
                .map_err(|source| VirtioAttachError::Irq { slot, source })?;

            let device_type = device.device_type();
            let transport = MmioTransport::new(slot, device, Arc::clone(&mem), Arc::new(line))
                .map_err(|source| VirtioAttachError::Transport { slot, source })?;
            let transport = Arc::new(Mutex::new(transport));

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

    pub fn is_empty(&self) -> bool {
        self.slots.is_empty()
    }

    /// How queue kicks reach the devices on this bus.
    pub fn notify_mode(&self) -> QueueNotifyMode {
        self.mode
    }

    /// Stops every queue worker thread and deassigns their ioeventfds.
    ///
    /// Idempotent, and also run from `Drop`, so "closing the VM leaves no
    /// device threads behind" holds even on an error path that never gets here
    /// (EPIC 14 acceptance criterion).
    pub fn shutdown(&self) {
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
                    layout::VIRTIO_MMIO_FIRST_IRQ + n as u32,
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
        assert_eq!(slots[2].1, layout::VIRTIO_MMIO_FIRST_IRQ + 2);
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
                layout::VIRTIO_MMIO_FIRST_IRQ,
                layout::VIRTIO_MMIO_BASE + layout::VIRTIO_MMIO_SLOT_SIZE,
                layout::VIRTIO_MMIO_FIRST_IRQ + 1
            )
        );
    }
}
