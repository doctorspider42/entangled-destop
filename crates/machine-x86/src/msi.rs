//! Message-signalled interrupt delivery: both hosts' implementations of
//! [`virtio_core::interrupt::MsiSink`], and the portable decoding they share.
//!
//! A device hands over an (address, data) pair — the message its driver
//! programmed into the MSI-X table — and the sink makes the guest's local APICs
//! see it:
//!
//! * [`KvmMsiSink`] (Linux): one `KVM_SIGNAL_MSI` ioctl. The kernel decodes the
//!   message against the in-kernel local APICs exactly as it would decode a
//!   write from a real device.
//! * [`UserspaceMsiSink`] (any host whose hypervisor has no MSI entry point —
//!   WHP): [`decode_msi_message`] does the same architectural decode *here*,
//!   producing the neutral [`InterruptRequest`] the IOAPIC model already emits,
//!   and hands it to [`InterruptDelivery`] — on WHP one `WHvRequestInterrupt`.
//!   The decode is pure bit arithmetic (Intel SDM vol. 3, "Message Signalled
//!   Interrupts"), so it lives in the portable machine layer and is unit-tested
//!   on both hosts; no `WHV_*` type is anywhere near it (ADR-0002).
//!
//! # Why `KVM_SIGNAL_MSI` and not an irqfd with a GSI routing entry
//!
//! KVM offers two ways to deliver an MSI, and this module deliberately takes the
//! simpler one:
//!
//! | | `KVM_SIGNAL_MSI` (this) | irqfd + `KVM_SET_GSI_ROUTING` |
//! |---|---|---|
//! | Per delivery | one ioctl on the VM fd, carrying address+data | one `write(2)` on an eventfd |
//! | Setup | none | allocate a GSI per vector, publish a `kvm_irq_routing_entry` of type `KVM_IRQ_ROUTING_MSI`, register the irqfd |
//! | When the guest rewrites a table entry | nothing to do — the message is read fresh on every signal | the **whole** routing table must be rebuilt (`KVM_SET_GSI_ROUTING` replaces it wholesale), so `virtio-core` would have to call back into the machine on every table write |
//! | Who may signal | anyone holding the `VmFd` | the kernel, from an eventfd — which is what vhost or a VFIO device needs |
//!
//! The second column's only real advantage is that the *kernel* can complete a
//! delivery without userspace. Nothing here needs that: every one of our devices
//! runs in userspace and is already holding the message when it decides to
//! interrupt, so an ioctl and an eventfd write cost the same one syscall. What we
//! would pay for it is a GSI allocator, a routing table to keep coherent, and a
//! seam violation — `virtio_core::msix` would have to tell the machine "vector 3
//! now means this address and data", i.e. exactly the KVM-shaped knowledge
//! ADR-0002 keeps out of that crate. [`MsiSink`] stays an address/data pair.
//!
//! If a kernel-side signaller ever arrives (a vhost-net backend, say), this is
//! the module that grows a routing table, and `virtio-core` still only knows how
//! to produce a message.
//!
//! # Fallback
//!
//! `KVM_CAP_SIGNAL_MSI` has been in Linux since 3.5 and is present on every host
//! this VMM supports, but it is checked rather than assumed
//! ([`KvmMsiSink::is_supported`]): a host without it gets INTx-only virtio
//! functions — no MSI-X capability at all — which still boots, rather than a
//! capability whose interrupts vanish.

use virtio_core::interrupt::{InterruptError, MsiMessage, MsiSink};
use vmm_core::hv::{DestinationMode, InterruptKind, InterruptRequest, TriggerMode};

// ---- the architectural decode (portable) -----------------------------------

/// Bits 31:20 of a valid xAPIC MSI address: the `0xFEE` window.
const MSI_ADDRESS_WINDOW: u32 = 0xfee;

/// What an MSI message means to a local APIC, decoded per the Intel SDM
/// (vol. 3, "Message Address Register Format" / "Message Data Register
/// Format"); the same layout every rust-vmm VMM and QEMU decode.
///
/// Returns `None` for a message no local APIC would accept — an address outside
/// the `0xFEE0_0000` window — or one whose delivery mode this machine
/// deliberately does not forward (SMI/INIT/ExtINT, the same three the IOAPIC
/// model drops). Every field is guest-programmed; nothing here is used as a
/// host address.
pub fn decode_msi_message(message: &MsiMessage) -> Option<InterruptRequest> {
    let address = message.address as u32;
    if (address >> 20) & 0xfff != MSI_ADDRESS_WINDOW {
        tracing::debug!(
            address = format_args!("{:#x}", message.address),
            "MSI address outside the 0xFEE window; dropping the message"
        );
        return None;
    }
    // Address: destination id in 19:12, destination mode in bit 2. (Bit 3, the
    // redirection hint, matters only for choosing *among* logical destinations,
    // which `InterruptKind::LowestPriority` already expresses.)
    let destination = (address >> 12) & 0xff;
    let destination_mode = if address & (1 << 2) != 0 {
        DestinationMode::Logical
    } else {
        DestinationMode::Physical
    };
    // Data: vector in 7:0, delivery mode in 10:8, trigger mode in bit 15.
    let vector = (message.data & 0xff) as u8;
    let kind = match (message.data >> 8) & 0x7 {
        0b000 => InterruptKind::Fixed,
        0b001 => InterruptKind::LowestPriority,
        0b100 => InterruptKind::Nmi,
        mode => {
            // SMI/INIT/ExtINT: a guest driver has no business asking for them,
            // and the IOAPIC model drops the same three (see
            // `crate::irqchip::ioapic`). Dropped, not an error — the message is
            // the guest's own.
            tracing::debug!(mode, "unsupported MSI delivery mode; dropping the message");
            return None;
        }
    };
    let trigger = if message.data & (1 << 15) != 0 {
        TriggerMode::Level
    } else {
        TriggerMode::Edge
    };
    Some(InterruptRequest {
        vector,
        destination,
        kind,
        destination_mode,
        trigger,
    })
}

// ---- the userspace sink (portable, used by the WHP machine) ----------------

/// Delivers MSI messages by decoding them in userspace and handing the result
/// to the hypervisor's [`InterruptDelivery`] — the MSI peer of
/// [`crate::irqchip::ioapic::IoApicLine`], and the reason virtio-pci needs no
/// in-kernel MSI support to work on WHP.
pub struct UserspaceMsiSink {
    delivery: std::sync::Arc<dyn vmm_core::hv::InterruptDelivery>,
}

impl UserspaceMsiSink {
    pub fn new(delivery: std::sync::Arc<dyn vmm_core::hv::InterruptDelivery>) -> Self {
        Self { delivery }
    }
}

impl MsiSink for UserspaceMsiSink {
    fn send(&self, message: MsiMessage) -> Result<(), InterruptError> {
        // A message no APIC would accept is the guest's own doing — dropped and
        // logged in the decode, never a host error, exactly as `KVM_SIGNAL_MSI`
        // answering "0 APICs accepted this" is not one.
        let Some(request) = decode_msi_message(&message) else {
            return Ok(());
        };
        self.delivery
            .request(&request)
            .map_err(|e| InterruptError::Signal(e.to_string()))
    }
}

// ---- the KVM sink (Linux) ---------------------------------------------------

#[cfg(target_os = "linux")]
pub use kvm::KvmMsiSink;

#[cfg(target_os = "linux")]
mod kvm {
    use kvm_ioctls::{Cap, VmFd};
    use std::sync::Arc;
    use virtio_core::interrupt::{InterruptError, MsiMessage, MsiSink};

    /// Delivers MSI messages through `KVM_SIGNAL_MSI` on a VM fd.
    pub struct KvmMsiSink {
        vm: Arc<VmFd>,
    }

    impl KvmMsiSink {
        pub fn new(vm: Arc<VmFd>) -> Self {
            Self { vm }
        }

        /// Whether this host can deliver MSI at all.
        ///
        /// Checked once per bus, at attach time, so the decision "does this function
        /// publish an MSI-X capability" is made before any driver can look.
        pub fn is_supported(vm: &VmFd) -> bool {
            vm.check_extension(Cap::SignalMsi)
        }
    }

    impl MsiSink for KvmMsiSink {
        fn send(&self, message: MsiMessage) -> Result<(), InterruptError> {
            // Every field here is guest-programmed, and that is the whole point: the
            // message is what the driver wrote into its own MSI-X table. KVM decodes
            // it against the *guest's* local APICs exactly as it would decode a write
            // from a real device, so a nonsense address can only fail to deliver — it
            // is never dereferenced by the host.
            //
            // `devid` stays 0 and `KVM_MSI_VALID_DEVID` is not set: that flag exists
            // for interrupt remapping on hosts with an IOMMU-backed routing table,
            // which this machine does not publish.
            let msi = kvm_bindings::kvm_msi {
                address_lo: message.address as u32,
                address_hi: (message.address >> 32) as u32,
                data: message.data,
                flags: 0,
                devid: 0,
                pad: [0; 12],
            };
            match self.vm.signal_msi(msi) {
                // KVM returns the number of local APICs that accepted the message; 0
                // means the guest's own APIC state discarded it (a destination that
                // matches no CPU, or a masked vector). Not a host error — the driver
                // wrote the address — but worth a line, because "the interrupt never
                // arrived" is otherwise indistinguishable from a lost injection.
                Ok(0) => {
                    tracing::debug!(
                        address = format_args!("{:#x}", message.address),
                        data = format_args!("{:#x}", message.data),
                        "the guest accepted no MSI for this message"
                    );
                    Ok(())
                }
                Ok(_) => Ok(()),
                Err(error) => Err(InterruptError::Signal(format!(
                    "KVM_SIGNAL_MSI for address {:#x} data {:#x} failed: {error}",
                    message.address, message.data
                ))),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::sync::{Arc, Mutex};
    use vmm_core::hv::{HvError, InterruptDelivery};

    /// The message this machine's guests actually send: a physical-destination,
    /// fixed-delivery message to APIC 0. Not asserting KVM's behaviour — asserting
    /// that the split into `address_lo`/`address_hi` keeps every bit.
    #[test]
    fn a_message_splits_into_the_two_halves_kvm_wants() {
        for address in [0xfee0_0000u64, 0xfee0_1000, 0x0000_00ff_fee0_2000, u64::MAX] {
            let lo = address as u32;
            let hi = (address >> 32) as u32;
            assert_eq!(u64::from(lo) | (u64::from(hi) << 32), address);
        }
    }

    /// The message a Linux guest programs for queue vector 0x41 on CPU 2 —
    /// physical destination, fixed delivery, edge — decoded field for field.
    #[test]
    fn decodes_the_message_linux_actually_writes() {
        let request = decode_msi_message(&MsiMessage {
            address: 0xfee0_2000,
            data: 0x0000_0041,
        })
        .expect("a fixed physical message decodes");
        assert_eq!(request.vector, 0x41);
        assert_eq!(request.destination, 2);
        assert_eq!(request.kind, InterruptKind::Fixed);
        assert_eq!(request.destination_mode, DestinationMode::Physical);
        assert_eq!(request.trigger, TriggerMode::Edge);
    }

    /// Every documented bit lands where the SDM says: logical destination mode
    /// (bit 2), lowest-priority delivery (data 10:8 = 001), level trigger
    /// (data bit 15), NMI (100 — vector ignored by the APIC but carried).
    #[test]
    fn decodes_the_flag_bits() {
        let logical_lowest = decode_msi_message(&MsiMessage {
            address: 0xfee0_f004,
            data: 0x0000_8143,
        })
        .expect("lowest-priority logical level message decodes");
        assert_eq!(logical_lowest.vector, 0x43);
        assert_eq!(logical_lowest.destination, 0xf);
        assert_eq!(logical_lowest.kind, InterruptKind::LowestPriority);
        assert_eq!(logical_lowest.destination_mode, DestinationMode::Logical);
        assert_eq!(logical_lowest.trigger, TriggerMode::Level);

        let nmi = decode_msi_message(&MsiMessage {
            address: 0xfee0_0000,
            data: 0x0000_0400,
        })
        .expect("an NMI message decodes");
        assert_eq!(nmi.kind, InterruptKind::Nmi);
    }

    /// Guest-programmed garbage must be dropped, not delivered somewhere
    /// surprising: an address outside 0xFEE and the three delivery modes this
    /// machine forwards nowhere.
    #[test]
    fn garbage_messages_are_dropped() {
        // Not the APIC window.
        assert_eq!(
            decode_msi_message(&MsiMessage {
                address: 0xdead_0000,
                data: 0x41
            }),
            None
        );
        // SMI (010), INIT (101), ExtINT (111).
        for mode in [0b010u32, 0b101, 0b111] {
            assert_eq!(
                decode_msi_message(&MsiMessage {
                    address: 0xfee0_0000,
                    data: mode << 8,
                }),
                None,
                "delivery mode {mode:#b} must be dropped"
            );
        }
    }

    /// Records what the "hypervisor" was asked to inject.
    #[derive(Default)]
    struct Recording {
        calls: AtomicU32,
        last: Mutex<Option<InterruptRequest>>,
    }

    impl InterruptDelivery for Recording {
        fn request(&self, interrupt: &InterruptRequest) -> Result<(), HvError> {
            self.calls.fetch_add(1, Ordering::AcqRel);
            if let Ok(mut last) = self.last.lock() {
                *last = Some(*interrupt);
            }
            Ok(())
        }
    }

    /// The whole path a WHP MSI-X interrupt takes below `virtio-core`: message
    /// in, one `InterruptDelivery::request` out, carrying the decoded fields —
    /// and a dropped message reaches no delivery at all.
    #[test]
    fn the_userspace_sink_delivers_decoded_messages_and_swallows_garbage() {
        let recording = Arc::new(Recording::default());
        let sink = UserspaceMsiSink::new(Arc::clone(&recording) as Arc<_>);

        sink.send(MsiMessage {
            address: 0xfee0_1000,
            data: 0x31,
        })
        .expect("a valid message is delivered");
        assert_eq!(recording.calls.load(Ordering::Acquire), 1);
        let last = recording.last.lock().unwrap().expect("a request arrived");
        assert_eq!(last.vector, 0x31);
        assert_eq!(last.destination, 1);

        sink.send(MsiMessage {
            address: 0,
            data: 0x31,
        })
        .expect("garbage is dropped, not an error");
        assert_eq!(recording.calls.load(Ordering::Acquire), 1);
    }
}
