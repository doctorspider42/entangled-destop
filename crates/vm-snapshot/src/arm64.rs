//! ARM64 whole-machine contract, sharing the digested container, memory,
//! virtio codecs and GPU archives with x86, never its CPU/platform decoder.
use crate::codec::{Reader, Writer};
use crate::format::{HostKind, SectionKind, SnapshotReader, SnapshotWriter};
use crate::{devices, memory, MachineShape, Metadata, Result, SnapshotError};
use machine_arm64::smp::{CpuStart, Power};
use machine_x86::state::SavedVirtioSlot;
use std::fs::File;
use std::path::Path;
use std::time::Instant;
use vmm_core::hv::arm64::CpuState;
use vmm_core::GuestMem;

const VERSION: u32 = 1;
pub struct State {
    pub cpus: Vec<CpuState>,
    pub power: Vec<Power>,
    pub counter: u64,
    pub gic: Vec<u8>,
    pub devices: Vec<SavedVirtioSlot>,
}

pub fn encode_cpu(state: &CpuState) -> Vec<u8> {
    let mut w = Writer::new();
    for value in state.registers {
        w.u64(value);
    }
    for value in state.simd {
        w.raw(&value);
    }
    for pairs in [&state.system, &state.icc] {
        w.count(pairs.len());
        for &(reg, value) in pairs {
            w.u16(reg).u64(value);
        }
    }
    w.count(state.features.len());
    for &value in &state.features {
        w.u64(value);
    }
    w.u64(state.timer_offset)
        .bool(state.timer_mask)
        .bool(state.serror);
    w.into_bytes()
}
pub fn decode_cpu(bytes: &[u8]) -> Result<CpuState> {
    let mut r = Reader::new(bytes);
    let mut registers = [0; 35];
    for value in &mut registers {
        *value = r.u64("ARM64 register")?;
    }
    let mut simd = [[0; 16]; 32];
    for value in &mut simd {
        value.copy_from_slice(r.take("SIMD register", 16)?);
    }
    fn pairs(r: &mut Reader<'_>) -> Result<Vec<(u16, u64)>> {
        let count = r.count("ARM64 system register count", 256, 10)?;
        let mut values = Vec::with_capacity(count);
        for _ in 0..count {
            values.push((r.u16("register id")?, r.u64("register value")?));
        }
        if values
            .iter()
            .enumerate()
            .any(|(i, v)| values[..i].iter().any(|old| old.0 == v.0))
        {
            return Err(SnapshotError::Restore("duplicate ARM64 register".into()));
        }
        Ok(values)
    }
    let system = pairs(&mut r)?;
    let icc = pairs(&mut r)?;
    let count = r.count("ARM64 features", 32, 8)?;
    let features = (0..count)
        .map(|_| r.u64("ARM64 feature"))
        .collect::<Result<Vec<_>>>()?;
    let state = CpuState {
        registers,
        simd,
        system,
        icc,
        features,
        timer_offset: r.u64("timer offset")?,
        timer_mask: r.bool("timer mask")?,
        serror: r.bool("SError")?,
    };
    r.finish("ARM64 CPU")?;
    Ok(state)
}

fn encode_power(state: &State) -> Vec<u8> {
    let mut w = Writer::new();
    w.u64(state.counter).count(state.power.len());
    for power in &state.power {
        match power {
            Power::Off => {
                w.u8(0).u64(0).u64(0);
            }
            Power::On => {
                w.u8(1).u64(0).u64(0);
            }
            Power::Pending(start) => {
                w.u8(2).u64(start.entry).u64(start.context);
            }
        }
    }
    w.into_bytes()
}
fn decode_power(bytes: &[u8]) -> Result<(u64, Vec<Power>)> {
    let mut r = Reader::new(bytes);
    let counter = r.u64("saved counter")?;
    let count = r.count("PSCI CPUs", 64, 17)?;
    let mut power = Vec::with_capacity(count);
    for _ in 0..count {
        let tag = r.u8("PSCI power")?;
        let entry = r.u64("PSCI entry")?;
        let context = r.u64("PSCI context")?;
        power.push(match (tag, entry, context) {
            (0, 0, 0) => Power::Off,
            (1, 0, 0) => Power::On,
            (2, entry, context) => Power::Pending(CpuStart { entry, context }),
            _ => return Err(SnapshotError::Restore("invalid PSCI power state".into())),
        });
    }
    r.finish("ARM64 clock/PSCI")?;
    Ok((counter, power))
}

pub fn save(
    path: &Path,
    mem: &GuestMem,
    metadata: Metadata,
    state: &State,
    gpu: &mut dyn crate::vm::GpuSave,
) -> Result<crate::vm::SaveReport> {
    if state.cpus.is_empty()
        || state.cpus.len() > 64
        || state.power.len() != state.cpus.len()
        || metadata.shape.vcpus as usize != state.cpus.len()
        || metadata.shape.devices
            != state
                .devices
                .iter()
                .map(|d| crate::DeviceSlot {
                    device_type: d.state.device_type,
                    slot: d.slot,
                })
                .collect::<Vec<_>>()
    {
        return Err(SnapshotError::Restore(
            "ARM64 saved CPU/device inventory differs from metadata".into(),
        ));
    }
    let started = Instant::now();
    let mut part = path.as_os_str().to_os_string();
    part.push(".part");
    let part = std::path::PathBuf::from(part);
    let result = (|| {
        let file = File::create(&part).map_err(SnapshotError::io("creating ARM64 snapshot"))?;
        let mut writer = SnapshotWriter::create(file, HostKind::HvfMacos)?;
        writer.put(
            SectionKind::Metadata,
            crate::meta::METADATA_VERSION,
            0,
            &metadata.encode(),
        )?;
        for (index, cpu) in state.cpus.iter().enumerate() {
            writer.put(SectionKind::Cpu, VERSION, index as u32, &encode_cpu(cpu))?;
        }
        writer.put(SectionKind::Clock, VERSION, 0, &encode_power(state))?;
        writer.put(SectionKind::HostIrqChip, VERSION, 0, &state.gic)?;
        for device in &state.devices {
            writer.put(
                SectionKind::Virtio,
                devices::VIRTIO_VERSION,
                device.slot,
                &devices::encode_virtio(&device.state),
            )?;
        }
        crate::vm::gpu::save_sections(&mut writer, &state.devices, Some(gpu))?;
        let options = memory::SaveOptions::from_env();
        let stats = memory::save_with(mem, &mut writer, options)?;
        let (bytes, file) = writer.finish()?;
        file.sync_all()
            .map_err(SnapshotError::io("syncing ARM64 snapshot"))?;
        drop(file);
        std::fs::rename(&part, path).map_err(SnapshotError::io("publishing ARM64 snapshot"))?;
        Ok(crate::vm::SaveReport {
            bytes,
            allocated_bytes: disk_image::allocated_bytes(path),
            memory: stats,
            elapsed: started.elapsed(),
            vcpus: state.cpus.len(),
            codec: options.codec,
        })
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(part);
    }
    result
}

pub fn restore(
    path: &Path,
    mem: &GuestMem,
    shape: &MachineShape,
    gpu_slots: &[u32],
) -> Result<(State, crate::vm::GpuArchives)> {
    let file = File::open(path).map_err(SnapshotError::io("opening ARM64 snapshot"))?;
    let mut reader = SnapshotReader::open(file)?;
    reader.require_native()?;
    if reader.host() != HostKind::HvfMacos || reader.arch() != crate::Arch::Aarch64 {
        return Err(SnapshotError::Restore("not an ARM64 HVF snapshot".into()));
    }
    for section in reader.sections().to_vec() {
        let version = match section.kind {
            SectionKind::Metadata => crate::meta::METADATA_VERSION,
            SectionKind::Virtio => devices::VIRTIO_VERSION,
            SectionKind::Memory => memory::MEMORY_VERSION,
            SectionKind::Cpu | SectionKind::Clock | SectionKind::HostIrqChip | SectionKind::Gpu => {
                VERSION
            }
            _ => {
                return Err(SnapshotError::Restore(
                    "unexpected ARM64 platform section".into(),
                ))
            }
        };
        reader.require_version(section.kind, section.instance, version)?;
        // Complete integrity check before the first guest-memory write.
        reader.stream_section(section.kind, section.instance, |_| Ok(()))?;
    }
    for kind in [
        SectionKind::Metadata,
        SectionKind::Clock,
        SectionKind::HostIrqChip,
    ] {
        if reader.instances(kind) != [0] {
            return Err(SnapshotError::Restore(format!(
                "ARM64 {} inventory differs",
                kind.as_str()
            )));
        }
    }
    let metadata = Metadata::decode(&reader.read_section(SectionKind::Metadata, 0)?)?;
    metadata.shape.check(shape)?;
    metadata.check_files()?;
    if reader.instances(SectionKind::Cpu) != (0..shape.vcpus).collect::<Vec<_>>()
        || reader.instances(SectionKind::Virtio)
            != shape.devices.iter().map(|d| d.slot).collect::<Vec<_>>()
    {
        return Err(SnapshotError::Restore(
            "ARM64 CPU/device inventory differs".into(),
        ));
    }
    let cpus = (0..shape.vcpus)
        .map(|index| decode_cpu(&reader.read_section(SectionKind::Cpu, index)?))
        .collect::<Result<Vec<_>>>()?;
    let (counter, power) = decode_power(&reader.read_section(SectionKind::Clock, 0)?)?;
    if power.len() != cpus.len() {
        return Err(SnapshotError::Restore("PSCI CPU inventory differs".into()));
    }
    let gic = reader.read_section(SectionKind::HostIrqChip, 0)?;
    if gic.is_empty() || gic.len() > 16 << 20 {
        return Err(SnapshotError::Restore("invalid GIC state size".into()));
    }
    let devices = shape
        .devices
        .iter()
        .map(|slot| {
            let state =
                devices::decode_virtio(&reader.read_section(SectionKind::Virtio, slot.slot)?)?;
            if state.device_type != slot.device_type {
                return Err(SnapshotError::Restore("virtio device type differs".into()));
            }
            Ok(SavedVirtioSlot {
                slot: slot.slot,
                state,
            })
        })
        .collect::<Result<Vec<_>>>()?;
    let slots = crate::vm::gpu::validate_sections(&mut reader, &devices, true)?;
    if slots != gpu_slots {
        return Err(SnapshotError::Restore(
            "GPU archive inventory differs".into(),
        ));
    }
    memory::restore(mem, &mut reader)?;
    Ok((
        State {
            cpus,
            power,
            counter,
            gic,
            devices,
        },
        crate::vm::GpuArchives::new(reader, slots),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    fn cpu() -> CpuState {
        CpuState {
            registers: [42; 35],
            simd: [[0x5a; 16]; 32],
            system: vec![(0xc080, 0x30d00800)],
            icc: vec![(0xc230, 255)],
            features: vec![1, 2, 3],
            timer_offset: 123456,
            timer_mask: true,
            serror: false,
        }
    }
    #[test]
    fn arm64_cpu_roundtrip_keeps_simd_timers_and_system_state() {
        let state = cpu();
        let bytes = encode_cpu(&state);
        assert_eq!(decode_cpu(&bytes).unwrap(), state);
        for end in 0..bytes.len() {
            assert!(decode_cpu(&bytes[..end]).is_err());
        }
        let mut extra = bytes.clone();
        extra.push(0);
        assert!(decode_cpu(&extra).is_err());
        let mut invalid_bool = bytes;
        let index = invalid_bool.len() - 1;
        invalid_bool[index] = 2;
        assert!(decode_cpu(&invalid_bool).is_err());
    }
    #[test]
    fn duplicate_system_registers_are_refused() {
        let mut state = cpu();
        state.system.push(state.system[0]);
        assert!(decode_cpu(&encode_cpu(&state)).is_err());
    }
    #[test]
    fn psci_roundtrip_preserves_off_on_and_pending_context() {
        let state = State {
            cpus: vec![],
            power: vec![
                Power::On,
                Power::Off,
                Power::Pending(CpuStart {
                    entry: 0x40001000,
                    context: u64::MAX,
                }),
            ],
            counter: 12345,
            gic: vec![],
            devices: vec![],
        };
        let bytes = encode_power(&state);
        let (counter, power) = decode_power(&bytes).unwrap();
        assert_eq!(counter, state.counter);
        assert_eq!(power, state.power);
        for end in 0..bytes.len() {
            assert!(decode_power(&bytes[..end]).is_err());
        }
    }
    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    #[test]
    fn native_container_roundtrip_and_refusals_preserve_target_memory() {
        use vm_memory::{Bytes, GuestAddress};
        struct NoGpu;
        impl crate::vm::GpuSave for NoGpu {
            fn slots(&self) -> &[u32] {
                &[]
            }
            fn save(&mut self, _: u32, _: &mut dyn std::io::Write) -> Result<()> {
                Err(SnapshotError::Restore("unexpected GPU save".into()))
            }
        }
        let root =
            std::env::temp_dir().join(format!("entangled-arm-container-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join("state.esnap");
        let shape = MachineShape {
            vcpus: 2,
            memory_bytes: 65536,
            transport: "mmio".into(),
            boot_mode: "direct-linux".into(),
            devices: vec![],
        };
        let metadata = Metadata {
            vm_name: "arm".into(),
            created_unix: 1,
            writer: "test".into(),
            config_toml: "test".into(),
            shape: shape.clone(),
            files: vec![],
        };
        let state = State {
            cpus: vec![cpu(); 2],
            power: vec![Power::On, Power::Off],
            counter: 54321,
            gic: vec![0x17; 4096],
            devices: vec![],
        };
        let address = GuestAddress(machine_arm64::RAM_BASE + 16384);
        let memory =
            || GuestMem::from_ranges(&[(GuestAddress(machine_arm64::RAM_BASE), 65536)]).unwrap();
        let source = memory();
        source.write_slice(b"live session", address).unwrap();
        save(&path, &source, metadata, &state, &mut NoGpu).unwrap();
        let target = memory();
        let (loaded, _) = restore(&path, &target, &shape, &[]).unwrap();
        assert_eq!(loaded.cpus, state.cpus);
        assert_eq!(loaded.power, state.power);
        assert_eq!(loaded.counter, state.counter);
        assert_eq!(loaded.gic, state.gic);
        let mut bytes = [0; 12];
        target.read_slice(&mut bytes, address).unwrap();
        assert_eq!(&bytes, b"live session");
        let mut wrong = shape.clone();
        wrong.vcpus = 3;
        let untouched = memory();
        untouched.write_slice(b"do not touch", address).unwrap();
        assert!(matches!(
            restore(&path, &untouched, &wrong, &[]),
            Err(SnapshotError::Mismatch { .. })
        ));
        assert!(restore(&path, &untouched, &shape, &[0]).is_err());
        let reader = SnapshotReader::open(File::open(&path).unwrap()).unwrap();
        let section = reader
            .sections()
            .iter()
            .find(|e| e.kind == SectionKind::Memory)
            .unwrap();
        let mut damaged = std::fs::read(&path).unwrap();
        damaged[(section.offset + section.len / 2) as usize] ^= 1;
        std::fs::write(&path, damaged).unwrap();
        assert!(matches!(
            restore(&path, &untouched, &shape, &[]),
            Err(SnapshotError::Corrupt { .. })
        ));
        untouched.read_slice(&mut bytes, address).unwrap();
        assert_eq!(&bytes, b"do not touch");
        std::fs::remove_dir_all(root).unwrap();
    }
}
