//! The HD Audio controller as a PCI function (ADR-0008: sound for Windows
//! guests): Intel ICH6's identity, the registers in BAR 0, and MSI.

use std::sync::Arc;

use hda::Hda;
use virtio_core::interrupt::MsiSink;
use virtio_core::{GuestMem, Quiesce};
use virtio_sound::SinkFactory;

use crate::msi_cap::MsiCapability;
use crate::pci::{ConfigSpace, PciError};

/// The controller, its output engine and its interrupt.
pub struct HdaFunction {
    device: Hda,
    msi: MsiCapability,
}

impl HdaFunction {
    /// A controller playing into sinks from `factory`, with guest memory and
    /// the MSI sink wired.
    pub fn new(
        mem: Arc<GuestMem>,
        sink: Option<Arc<dyn MsiSink>>,
        factory: SinkFactory,
    ) -> std::io::Result<Arc<Self>> {
        let function = Arc::new(Self {
            device: Hda::new(),
            msi: MsiCapability::default(),
        });
        let me = Arc::downgrade(&function);
        function.device.connect(
            mem,
            Arc::new(move || {
                if let (Some(me), Some(sink)) = (me.upgrade(), sink.as_ref()) {
                    me.msi.signal(sink.as_ref(), "HD Audio");
                }
            }),
        );
        function.device.start_pump(factory)?;
        Ok(function)
    }

    /// The function's configuration space: the registers at `base` in BAR 0
    /// and an MSI capability.
    pub fn config_space(&self, base: u32) -> Result<ConfigSpace, PciError> {
        let mut space = ConfigSpace::type0(hda::VENDOR_ID, hda::DEVICE_ID, hda::CLASS_HDA, 1)
            .with_subsystem(0x1af4, 0x1100)
            .with_memory_bar(hda::BAR_INDEX, base, hda::BAR_SIZE)?;
        self.msi.add_to(&mut space)?;
        Ok(space)
    }

    pub fn set_quiesce(&self, quiesce: Arc<Quiesce>) {
        self.device.set_quiesce(quiesce);
    }

    pub fn mmio_read(&self, offset: u64, data: &mut [u8]) {
        self.device.mmio_read(offset, data);
    }

    pub fn mmio_write(&self, offset: u64, data: &[u8]) {
        self.device.mmio_write(offset, data);
    }

    /// Power-on (ADR-0005).
    pub fn reset(&self) {
        self.device.reset();
    }

    /// For a snapshot (ADR-0006).
    pub fn save(&self) -> Vec<u8> {
        self.device.save()
    }

    /// Puts [`Self::save`] back.
    pub fn load(&self, bytes: &[u8]) -> bool {
        self.device.load(bytes)
    }
}
