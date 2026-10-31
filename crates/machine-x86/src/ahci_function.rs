//! The AHCI controller as a PCI function (ADR-0008 §4): its identity, ABAR
//! in BAR 5, and MSI (`crate::msi_cap` says why MSI).

use std::sync::{Arc, Mutex, PoisonError};

use ahci::{Ahci, Atapi};
use virtio_core::interrupt::MsiSink;
use virtio_core::GuestMem;

use crate::msi_cap::MsiCapability;
use crate::pci::{ConfigSpace, PciError};

/// The controller and its interrupt.
pub struct AhciFunction {
    device: Mutex<Ahci>,
    msi: MsiCapability,
}

impl AhciFunction {
    /// The HBA with one port per drive.
    pub fn new(drives: Vec<Atapi>) -> Arc<Self> {
        Arc::new(Self {
            device: Mutex::new(Ahci::new(drives)),
            msi: MsiCapability::default(),
        })
    }

    /// Wires guest memory and the MSI sink. Called once, before the guest
    /// runs.
    pub fn connect(self: &Arc<Self>, mem: Arc<GuestMem>, sink: Option<Arc<dyn MsiSink>>) {
        let me = Arc::downgrade(self);
        let raise: ahci::Raise = Arc::new(move || {
            if let (Some(me), Some(sink)) = (me.upgrade(), sink.as_ref()) {
                me.msi.signal(sink.as_ref(), "AHCI");
            }
        });
        self.lock().connect(mem, raise);
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Ahci> {
        self.device.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The function's configuration space: ABAR at `abar` in BAR 5 and an
    /// MSI capability whose registers this function mirrors.
    pub fn config_space(&self, abar: u32) -> Result<ConfigSpace, PciError> {
        let mut space = ConfigSpace::type0(ahci::VENDOR_ID, ahci::DEVICE_ID, ahci::CLASS_AHCI, 2)
            .with_subsystem(ahci::VENDOR_ID, ahci::DEVICE_ID)
            .with_memory_bar(ahci::ABAR_INDEX, abar, ahci::ABAR_SIZE)?;
        self.msi.add_to(&mut space)?;
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
