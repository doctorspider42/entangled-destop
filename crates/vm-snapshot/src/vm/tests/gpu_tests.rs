use super::*;
use std::io::{Read, Seek, SeekFrom, Write};

struct Gpu {
    slots: Vec<u32>,
    fail: bool,
    calls: usize,
}
impl GpuSave for Gpu {
    fn slots(&self) -> &[u32] {
        &self.slots
    }
    fn save(&mut self, slot: u32, out: &mut dyn Write) -> Result<()> {
        assert_eq!(slot, 0);
        self.calls += 1;
        for value in 1..=3 {
            out.write_all(&vec![value; 70 << 10])
                .map_err(SnapshotError::io("writing test GPU bytes"))?;
            if self.fail {
                return Err(SnapshotError::Restore("GPU readback failed".into()));
            }
        }
        Ok(())
    }
}
fn gpu_shape() -> MachineShape {
    let mut shape = shape();
    shape.devices[0].device_type = 16;
    shape
}
fn write_gpu(path: &Path, gpu: &mut dyn GpuSave) -> Result<SaveReport> {
    let mem = memory(4 << 20);
    mem.write_slice(b"guest bytes", GuestAddress(0x2000))
        .unwrap();
    let mut machine = machine_state();
    machine.virtio[0].state.device_type = 16;
    let mut metadata = metadata(vec![]);
    metadata.shape = gpu_shape();
    save_with_gpu(
        path,
        &mem,
        SaveRequest {
            metadata,
            host: fixture_host(),
            cpus: &[cpu_state(0, 42)],
            clock: None,
            host_irqchip: None,
            machine: &machine,
        },
        gpu,
    )
}
fn load_gpu(path: &Path, target: &GuestMemoryMmap) -> Result<Restored> {
    let reader = SnapshotReader::open(std::fs::File::open(path).unwrap())?;
    restore_contents_with_gpu(reader, target, &gpu_shape(), Instant::now(), Some(&[0]))
}

#[test]
fn whole_vm_streams_gpu_section_and_retains_verified_random_access_for_cold_uploads() {
    let dir = temp_dir("gpu-stream");
    let path = dir.join("vm.esnap");
    let mut gpu = Gpu {
        slots: vec![0],
        fail: false,
        calls: 0,
    };
    write_gpu(&path, &mut gpu).unwrap();
    assert_eq!(gpu.calls, 1);
    let target = memory(4 << 20);
    let mut restored = load_gpu(&path, &target).unwrap();
    assert_eq!(restored.cpus[0].registers.rip, 42);
    let mut bytes = [0; 11];
    target.read_slice(&mut bytes, GuestAddress(0x2000)).unwrap();
    assert_eq!(&bytes, b"guest bytes");
    let archive = restored.gpu.as_mut().unwrap();
    assert_eq!(archive.slots(), [0]);
    assert!(archive.section(1).is_err());
    let mut section = archive.section(0).unwrap();
    let mut back = Vec::new();
    section.read_to_end(&mut back).unwrap();
    assert_eq!(back.len(), 210 << 10);
    for (i, part) in back.chunks_exact(70 << 10).enumerate() {
        assert!(part.iter().all(|value| *value == (i + 1) as u8));
    }
    section.seek(SeekFrom::Start(0)).unwrap();
    let mut first = [0; 16];
    section.read_exact(&mut first).unwrap();
    assert_eq!(first, [1; 16]);
    assert!(section.seek(SeekFrom::End(1)).is_err());
    drop(restored);
    let target = memory(4 << 20);
    target
        .write_slice(b"unchanged", GuestAddress(0x2000))
        .unwrap();
    let err = restore_fixture(&path, &target, &gpu_shape()).unwrap_err();
    assert!(
        err.to_string()
            .contains("cannot apply saved accelerated GPU state"),
        "{err}"
    );
    let mut bytes = [0; 9];
    target.read_slice(&mut bytes, GuestAddress(0x2000)).unwrap();
    assert_eq!(&bytes, b"unchanged");
    let reader = SnapshotReader::open(std::fs::File::open(&path).unwrap()).unwrap();
    let err = restore_contents_with_gpu(reader, &target, &gpu_shape(), Instant::now(), Some(&[]))
        .unwrap_err();
    assert!(
        err.to_string().contains("accelerated GPU checkpoint slots"),
        "{err}"
    );
    target.read_slice(&mut bytes, GuestAddress(0x2000)).unwrap();
    assert_eq!(
        &bytes, b"unchanged",
        "a GPU-aware loader must not omit an archive before RAM changes"
    );
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn corrupt_gpu_section_refuses_before_ram_changes_and_changes_after_restore_are_caught() {
    let dir = temp_dir("gpu-corrupt");
    let path = dir.join("vm.esnap");
    let mut gpu = Gpu {
        slots: vec![0],
        fail: false,
        calls: 0,
    };
    write_gpu(&path, &mut gpu).unwrap();
    let target = memory(4 << 20);
    let mut restored = load_gpu(&path, &target).unwrap();
    let reader = SnapshotReader::open(std::fs::File::open(&path).unwrap()).unwrap();
    let offset = reader
        .sections()
        .iter()
        .find(|section| section.kind == SectionKind::Gpu)
        .unwrap()
        .offset;
    drop(reader);
    let mut file = std::fs::OpenOptions::new().write(true).open(&path).unwrap();
    file.seek(SeekFrom::Start(offset + (100 << 10))).unwrap();
    file.write_all(&[99]).unwrap();
    file.sync_all().unwrap();
    assert!(restored.gpu.as_mut().unwrap().section(0).is_err());
    drop(restored);
    drop(file);
    let target = memory(4 << 20);
    target
        .write_slice(b"unchanged", GuestAddress(0x2000))
        .unwrap();
    assert!(matches!(
        load_gpu(&path, &target),
        Err(SnapshotError::Corrupt { .. })
    ));
    let mut bytes = [0; 9];
    target.read_slice(&mut bytes, GuestAddress(0x2000)).unwrap();
    assert_eq!(&bytes, b"unchanged");
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn failed_gpu_save_preserves_existing_snapshot_and_invalid_slots_do_not_call_renderer() {
    let dir = temp_dir("gpu-fail");
    let path = dir.join("vm.esnap");
    let mut gpu = Gpu {
        slots: vec![0],
        fail: false,
        calls: 0,
    };
    write_gpu(&path, &mut gpu).unwrap();
    let original = std::fs::read(&path).unwrap();
    gpu.fail = true;
    assert!(write_gpu(&path, &mut gpu).is_err());
    assert_eq!(std::fs::read(&path).unwrap(), original);
    assert!(!partial_path(&path).exists());
    for slots in [vec![0, 0], vec![1], vec![1, 0], vec![0; 257]] {
        let mut gpu = Gpu {
            slots,
            fail: false,
            calls: 0,
        };
        assert!(write_gpu(&path, &mut gpu).is_err());
        assert_eq!(gpu.calls, 0);
        assert_eq!(std::fs::read(&path).unwrap(), original);
        assert!(!partial_path(&path).exists());
    }
    std::fs::remove_dir_all(dir).unwrap();
}
