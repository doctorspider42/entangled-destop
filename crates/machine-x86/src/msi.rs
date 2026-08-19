//! KVM message-signalled interrupt delivery: the Linux implementation of
//! [`virtio_core::interrupt::MsiSink`].
//!
//! The MSI peer of [`crate::irqfd::IrqFdLine`]. A device hands over an
//! (address, data) pair — the message its driver programmed into the MSI-X table
//! — and `KVM_SIGNAL_MSI` walks the in-kernel local APICs and injects it.
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

#[cfg(test)]
mod tests {
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
}
