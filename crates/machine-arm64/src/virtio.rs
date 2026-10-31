//! Synchronous guest MMIO kicks and a shared worker for host GPU completion.
use crate::{MachineError, MAX_VIRTIO_SLOTS, VIRTIO_BASE};
use std::sync::{Arc, Mutex};
use virtio_core::host_wake::HostWakeService;
use virtio_core::{IrqLine, MmioTransport, Quiesce, VirtioDevice};
use vmm_core::GuestMem;

pub struct AttachedDevice {
    pub device: Box<dyn VirtioDevice>,
    pub line: Arc<dyn IrqLine>,
    /// The machine has already supplied backing to this region. MMIO pins its
    /// placement; the guest can query but cannot relocate it like a PCI BAR.
    pub shm: Option<(u8, u64)>,
}

pub struct MmioBus {
    transports: Vec<Arc<Mutex<MmioTransport>>>,
    wake: HostWakeService<MmioTransport>,
    quiesce: Arc<Quiesce>,
}

impl MmioBus {
    pub fn new(mem: Arc<GuestMem>, devices: Vec<AttachedDevice>) -> Result<Self, MachineError> {
        if devices.len() > MAX_VIRTIO_SLOTS as usize {
            return Err(MachineError::Invalid("too many virtio devices".into()));
        }
        let quiesce = Quiesce::new();
        let wake = HostWakeService::new(devices.len());
        wake.set_quiesce(quiesce.clone());
        let mut transports = Vec::new();
        for (slot, mut attached) in devices.into_iter().enumerate() {
            attached.device.set_host_waker(wake.waker(slot));
            let mut transport =
                MmioTransport::new(slot, attached.device, mem.clone(), attached.line)?;
            transport.set_quiesce(quiesce.clone());
            if let Some((id, base)) = attached.shm {
                transport.set_shm_base(id, base);
            }
            transports.push(Arc::new(Mutex::new(transport)));
        }
        wake.start(transports.clone());
        Ok(Self {
            transports,
            wake,
            quiesce,
        })
    }

    fn locked(
        &self,
        slot: usize,
    ) -> Result<std::sync::MutexGuard<'_, MmioTransport>, MachineError> {
        self.transports
            .get(slot)
            .ok_or_else(|| MachineError::Invalid("absent virtio slot".into()))?
            .lock()
            .map_err(|_| MachineError::Invalid("poisoned virtio transport".into()))
    }

    pub fn prepare_snapshot(&self) -> Result<(), MachineError> {
        for slot in 0..self.len() {
            let mut transport = self.locked(slot)?;
            transport
                .prepare_snapshot()
                .map_err(|e| MachineError::Invalid(e.to_string()))?;
            if let Some(reason) = transport.snapshot_refusal() {
                return Err(MachineError::Invalid(reason));
            }
        }
        Ok(())
    }

    pub fn save(&self) -> Result<Vec<virtio_core::TransportSaveState>, MachineError> {
        (0..self.len())
            .map(|slot| Ok(self.locked(slot)?.save()))
            .collect()
    }

    pub fn load(&self, saved: &[virtio_core::TransportSaveState]) -> Result<(), MachineError> {
        if saved.len() != self.len() {
            return Err(MachineError::Invalid("virtio inventory differs".into()));
        }
        for (slot, state) in saved.iter().enumerate() {
            self.locked(slot)?
                .load(state)
                .map_err(|e| MachineError::Invalid(e.to_string()))?;
        }
        Ok(())
    }

    pub fn gpu_checkpoint_slots(&self) -> Result<Vec<u32>, MachineError> {
        let mut slots = Vec::new();
        for slot in 0..self.len() {
            let transport = self.locked(slot)?;
            if transport.has_streamed_state() {
                slots.push(slot as u32);
            }
        }
        Ok(slots)
    }

    pub fn save_streamed_state(
        &self,
        slot: u32,
        out: &mut dyn std::io::Write,
    ) -> Result<(), MachineError> {
        self.locked(slot as usize)?
            .save_streamed_state(out)
            .map_err(|e| MachineError::Invalid(e.to_string()))
    }

    pub fn load_streamed_state(
        &self,
        slot: u32,
        input: &mut dyn virtio_core::StateReader,
    ) -> Result<(), MachineError> {
        self.locked(slot as usize)?
            .load_streamed_state(input)
            .map_err(|e| MachineError::Invalid(e.to_string()))
    }

    pub fn len(&self) -> usize {
        self.transports.len()
    }
    pub fn is_empty(&self) -> bool {
        self.transports.is_empty()
    }

    pub fn read(&self, address: u64, data: &mut [u8]) -> Result<(), MachineError> {
        let (slot, offset) = self.locate(address, data.len())?;
        self.transports[slot]
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .read(offset, data);
        Ok(())
    }

    pub fn write(&self, address: u64, data: &[u8]) -> Result<(), MachineError> {
        let (slot, offset) = self.locate(address, data.len())?;
        self.transports[slot]
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .write(offset, data);
        Ok(())
    }

    fn locate(&self, address: u64, len: usize) -> Result<(usize, u64), MachineError> {
        let offset = address
            .checked_sub(VIRTIO_BASE)
            .ok_or_else(|| MachineError::Invalid("MMIO address is below the virtio bus".into()))?;
        let slot = usize::try_from(offset / 0x1000)
            .map_err(|_| MachineError::Invalid("MMIO slot overflows".into()))?;
        let within = offset % 0x1000;
        if slot >= self.len() || len == 0 || len > 8 || within + len as u64 > 0x1000 {
            return Err(MachineError::Invalid(
                "MMIO access leaves a device slot".into(),
            ));
        }
        Ok((slot, within))
    }

    /// Called once vCPUs have stopped. Stop host completion work before
    /// resetting devices, which joins their workers and retires GPU mappings.
    pub fn shutdown(&self) {
        self.wake.shutdown();
        for transport in &self.transports {
            transport
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .power_on_reset();
        }
    }

    pub fn quiesce(&self) -> Arc<Quiesce> {
        self.quiesce.clone()
    }
}

impl Drop for MmioBus {
    fn drop(&mut self) {
        self.shutdown();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    struct Line;
    impl IrqLine for Line {
        fn trigger(&self) -> Result<(), virtio_core::InterruptError> {
            Ok(())
        }
    }

    #[test]
    fn a_guest_cannot_cross_or_address_an_absent_device_slot() {
        let path =
            std::env::temp_dir().join(format!("entangled-arm64-bus-{}.raw", std::process::id()));
        let file = std::fs::File::create(&path).unwrap();
        file.set_len(8192).unwrap();
        drop(file);
        let device = virtio_block::BlockDevice::open(&path, false).unwrap();
        let mem = Arc::new(
            GuestMem::from_ranges(&[(vm_memory::GuestAddress(crate::RAM_BASE), 65536)]).unwrap(),
        );
        let bus = MmioBus::new(
            mem,
            vec![AttachedDevice {
                device: Box::new(device),
                line: Arc::new(Line),
                shm: None,
            }],
        )
        .unwrap();
        let mut word = [0; 4];
        bus.read(VIRTIO_BASE, &mut word).unwrap();
        assert_eq!(u32::from_le_bytes(word), 0x74726976);
        for address in [
            VIRTIO_BASE - 1,
            VIRTIO_BASE + 0xffd,
            VIRTIO_BASE + 0x1000,
            u64::MAX,
        ] {
            assert!(bus.read(address, &mut word).is_err());
            assert!(bus.write(address, &word).is_err());
        }
        assert!(bus.write(VIRTIO_BASE, &[0; 9]).is_err());
        assert!(bus.read(VIRTIO_BASE, &mut []).is_err());
        bus.shutdown();
        drop(bus);
        std::fs::remove_file(path).unwrap();
    }
}
