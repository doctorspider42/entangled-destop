use super::*;
use crate::virtio_pci::PciInterruptMode;
use std::io::{Cursor, SeekFrom, Write};
use virtio_core::{
    DeviceError, DeviceResources, DeviceType, StateReader, VirtioDevice, VIRTIO_F_VERSION_1,
};
use vmm_core::hv::{HvError, InterruptDelivery, InterruptRequest};

struct Delivery;
impl InterruptDelivery for Delivery {
    fn request(&self, _: &InterruptRequest) -> Result<(), HvError> {
        Ok(())
    }
}

struct Device {
    kind: DeviceType,
    streamed: bool,
    log: Arc<Mutex<Vec<&'static str>>>,
}
impl VirtioDevice for Device {
    fn device_type(&self) -> DeviceType {
        self.kind
    }
    fn queue_max_sizes(&self) -> &[u16] {
        &[256]
    }
    fn device_features(&self) -> u64 {
        VIRTIO_F_VERSION_1
    }
    fn ack_features(&mut self, _: u64) -> bool {
        true
    }
    fn read_config(&self, _: u64, _: &mut [u8]) {}
    fn write_config(&mut self, _: u64, _: &[u8]) {}
    fn activate(&mut self, _: DeviceResources) -> Result<(), DeviceError> {
        Ok(())
    }
    fn notify(&mut self, _: u16) -> Result<(), DeviceError> {
        panic!("checkpoint must not drain new guest requests")
    }
    fn reset(&mut self) {}
    fn has_streamed_state(&self) -> bool {
        self.streamed
    }
    fn prepare_snapshot(&mut self) -> Result<(), DeviceError> {
        if self.streamed {
            self.log.lock().unwrap().push("prepare");
        }
        Ok(())
    }
    fn save_streamed_state(&mut self, out: &mut dyn Write) -> Result<(), DeviceError> {
        self.log.lock().unwrap().push("save");
        out.write_all(&[0xa7; 65536])
            .map_err(|error| DeviceError::Backend(error.to_string()))?;
        out.write_all(b"tail")
            .map_err(|error| DeviceError::Backend(error.to_string()))
    }
    fn load_streamed_state(&mut self, reader: &mut dyn StateReader) -> Result<(), DeviceError> {
        let mut tail = [0; 4];
        reader
            .seek(SeekFrom::End(-4))
            .map_err(|error| DeviceError::Backend(error.to_string()))?;
        reader
            .read_exact(&mut tail)
            .map_err(|error| DeviceError::Backend(error.to_string()))?;
        if &tail != b"tail" {
            return Err(DeviceError::Backend("bad GPU tail".into()));
        }
        reader
            .seek(SeekFrom::Start(0))
            .map_err(|error| DeviceError::Backend(error.to_string()))?;
        let mut bytes = [0; 65536];
        reader
            .read_exact(&mut bytes)
            .map_err(|error| DeviceError::Backend(error.to_string()))?;
        if bytes != [0xa7; 65536] {
            return Err(DeviceError::Backend("bad GPU data".into()));
        }
        self.log.lock().unwrap().push("load");
        Ok(())
    }
}

#[test]
fn both_machine_transports_route_streams_by_real_slot_and_propagate_failures() {
    for pci in [false, true] {
        let log = Arc::new(Mutex::new(Vec::new()));
        let mem = Arc::new(virtio_core::testing::guest_memory(1 << 20));
        let chip = UserspaceIrqChip::new(Arc::new(Delivery), 1).unwrap();
        let serial = SerialConsole::with_trigger(chip.serial_line(), Box::new(std::io::sink()));
        let devices: Vec<Box<dyn VirtioDevice>> = [
            (DeviceType::Block, false),
            (DeviceType::Gpu, true),
            (DeviceType::Gpu, false),
        ]
        .into_iter()
        .map(|(kind, streamed)| {
            Box::new(Device {
                kind,
                streamed,
                log: log.clone(),
            }) as Box<dyn VirtioDevice>
        })
        .collect();
        let bus = if pci {
            MachineBus::with_virtio_pci(
                serial,
                VirtioPciBus::attach_userspace(mem, devices, &chip, PciInterruptMode::Msix)
                    .unwrap(),
            )
        } else {
            MachineBus::with_virtio(
                serial,
                VirtioMmioBus::attach_userspace(mem, devices, &chip).unwrap(),
            )
        };
        assert_eq!(bus.gpu_checkpoint_slots().unwrap(), [1]);
        bus.prepare_snapshot().unwrap();
        let mut bytes = Vec::new();
        bus.save_streamed_state(1, &mut bytes).unwrap();
        assert_eq!(bytes.len(), 65540);
        bus.load_streamed_state(1, &mut Cursor::new(&bytes))
            .unwrap();
        assert_eq!(*log.lock().unwrap(), ["prepare", "save", "load"]);
        for slot in [0, 2, u32::MAX] {
            assert!(bus.save_streamed_state(slot, &mut Vec::new()).is_err());
            assert!(bus
                .load_streamed_state(slot, &mut Cursor::new(&bytes))
                .is_err());
        }
        assert!(bus
            .load_streamed_state(1, &mut Cursor::new(&bytes[..65536]))
            .is_err());
        struct Broken;
        impl Write for Broken {
            fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
                Err(std::io::Error::other("full disk"))
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let err = bus.save_streamed_state(1, &mut Broken).unwrap_err();
        assert!(err.to_string().contains("full disk"));
        assert_eq!(bus.gpu_checkpoint_slots().unwrap(), [1]);
        // An unavailable GPU cannot disappear from inventory and leave a
        // deceptively successful snapshot with only its transport registers.
        if pci {
            let transport = bus.pci().unwrap().slots()[1].transport.clone();
            assert!(std::thread::spawn(move || {
                let _guard = transport.lock().unwrap();
                panic!("poison fixture");
            })
            .join()
            .is_err());
        } else {
            let transport = bus.virtio().slots()[1].transport.clone();
            assert!(std::thread::spawn(move || {
                let _guard = transport.lock().unwrap();
                panic!("poison fixture");
            })
            .join()
            .is_err());
        }
        assert!(bus.gpu_checkpoint_slots().is_err());
        assert!(bus.prepare_snapshot().is_err());
        assert!(bus.save_streamed_state(1, &mut Vec::new()).is_err());
    }
}
