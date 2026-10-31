//! A plain PCI MSI capability (not MSI-X), for the non-virtio functions: the
//! AHCI controller and the HD Audio controller.
//!
//! MSI rather than INTx because nothing else would reach a Windows guest:
//! this machine's DSDT has no `_PRT`, so Windows could not route the pin, and
//! the virtio functions never needed one because they use MSI-X. Every
//! in-box driver uses MSI when it is there; the firmware polls.

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;

use virtio_core::interrupt::{MsiMessage, MsiSink};

use crate::pci::{ConfigSpace, PciError};

/// PCI capability id of MSI.
const CAP_MSI: u8 = 0x05;
/// MSI message control: 64-bit address capable.
const MSI_64BIT: u16 = 1 << 7;
/// MSI message control, enable bit, as a bit of the capability's first dword.
const MSI_ENABLE_DWORD: u32 = 1 << 16;

/// The four dwords of the capability, mirrored out of config space.
#[derive(Default)]
pub struct MsiCapability {
    control: Arc<AtomicU32>,
    address_lo: Arc<AtomicU32>,
    address_hi: Arc<AtomicU32>,
    data: Arc<AtomicU32>,
}

impl MsiCapability {
    /// Appends the capability to `space` and mirrors its registers here.
    pub fn add_to(&self, space: &mut ConfigSpace) -> Result<(), PciError> {
        let mut record = [0u8; 16];
        record[0] = CAP_MSI;
        record[2..4].copy_from_slice(&MSI_64BIT.to_le_bytes());
        let at = space.add_capability_writable(
            &record,
            &[
                // Enable and the multiple-message-enable field.
                MSI_ENABLE_DWORD | (0x7 << 20),
                0xffff_fffc,
                0xffff_ffff,
                0x0000_ffff,
            ],
        )?;
        space.mirror_dword(at, Arc::clone(&self.control));
        space.mirror_dword(at + 4, Arc::clone(&self.address_lo));
        space.mirror_dword(at + 8, Arc::clone(&self.address_hi));
        space.mirror_dword(at + 12, Arc::clone(&self.data));
        Ok(())
    }

    /// Sends the message the driver programmed, when it enabled MSI.
    pub fn signal(&self, sink: &dyn MsiSink, what: &str) {
        if self.control.load(Ordering::Acquire) & MSI_ENABLE_DWORD == 0 {
            return;
        }
        let address = u64::from(self.address_lo.load(Ordering::Acquire))
            | (u64::from(self.address_hi.load(Ordering::Acquire)) << 32);
        let data = self.data.load(Ordering::Acquire) & 0xffff;
        if let Err(error) = sink.send(MsiMessage { address, data }) {
            tracing::warn!(%error, device = what, "could not send an MSI");
        }
    }
}
