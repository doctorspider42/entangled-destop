//! The AHCI controller as a PCI function (ADR-0008 §4): its identity, ABAR
//! in BAR 5, and MSI.
//!
//! MSI rather than INTx because nothing else would reach a Windows guest:
//! this machine's DSDT has no `_PRT`, so Windows could not route the pin, and
//! the virtio functions never needed one because they use MSI-X. `storahci`,
//! Linux's `ahci` and every other driver use MSI when it is there; the
//! firmware polls.

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use ahci::{Ahci, Atapi};
use virtio_core::interrupt::{MsiMessage, MsiSink};
use virtio_core::GuestMem;

use crate::pci::{ConfigSpace, PciError};

/// PCI capability id of MSI.
const CAP_MSI: u8 = 0x05;
/// MSI message control: 64-bit address capable.
const MSI_64BIT: u16 = 1 << 7;
/// MSI message control, enable bit, as a bit of the capability's first dword.
const MSI_ENABLE_DWORD: u32 = 1 << 16;

/// The four dwords of the MSI capability, mirrored out of config space.
#[derive(Default)]
struct MsiRegisters {
    control: Arc<AtomicU32>,
    address_lo: Arc<AtomicU32>,
    address_hi: Arc<AtomicU32>,
    data: Arc<AtomicU32>,
}

/// The controller and its interrupt.
pub struct AhciFunction {
    device: Mutex<Ahci>,
    msi: MsiRegisters,
}

impl AhciFunction {
    /// The HBA with one port per drive.
    pub fn new(drives: Vec<Atapi>) -> Arc<Self> {
        Arc::new(Self {
            device: Mutex::new(Ahci::new(drives)),
            msi: MsiRegisters::default(),
        })
    }

    /// Wires guest memory and the MSI sink. Called once, before the guest
    /// runs.
    pub fn connect(self: &Arc<Self>, mem: Arc<GuestMem>, sink: Option<Arc<dyn MsiSink>>) {
        let me = Arc::downgrade(self);
        let raise: ahci::Raise = Arc::new(move || {
            if let (Some(me), Some(sink)) = (me.upgrade(), sink.as_ref()) {
                me.signal(sink.as_ref());
            }
        });
        self.lock().connect(mem, raise);
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Ahci> {
        self.device.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Sends the message the driver programmed, when it enabled MSI.
    fn signal(&self, sink: &dyn MsiSink) {
        if self.msi.control.load(Ordering::Acquire) & MSI_ENABLE_DWORD == 0 {
            return;
        }
        let address = u64::from(self.msi.address_lo.load(Ordering::Acquire))
            | (u64::from(self.msi.address_hi.load(Ordering::Acquire)) << 32);
        let data = self.msi.data.load(Ordering::Acquire) & 0xffff;
        if let Err(error) = sink.send(MsiMessage { address, data }) {
            tracing::warn!(%error, "could not send the AHCI MSI");
        }
    }

    /// The function's configuration space: ABAR at `abar` in BAR 5 and an
    /// MSI capability whose registers this function mirrors.
    pub fn config_space(&self, abar: u32) -> Result<ConfigSpace, PciError> {
        let mut space = ConfigSpace::type0(ahci::VENDOR_ID, ahci::DEVICE_ID, ahci::CLASS_AHCI, 2)
            .with_subsystem(ahci::VENDOR_ID, ahci::DEVICE_ID)
            .with_memory_bar(ahci::ABAR_INDEX, abar, ahci::ABAR_SIZE)?;
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
        space.mirror_dword(at, Arc::clone(&self.msi.control));
        space.mirror_dword(at + 4, Arc::clone(&self.msi.address_lo));
        space.mirror_dword(at + 8, Arc::clone(&self.msi.address_hi));
        space.mirror_dword(at + 12, Arc::clone(&self.msi.data));
        Ok(space)
    }

    pub fn mmio_read(&self, offset: u64, data: &mut [u8]) {
        self.lock().mmio_read(offset, data);
    }

    pub fn mmio_write(&self, offset: u64, data: &[u8]) {
        self.lock().mmio_write(offset, data);
    }

    /// Power-on (ADR-0005).
    pub fn reset(&self) {
        self.lock().reset();
    }
}
