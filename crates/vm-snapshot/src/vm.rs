//! Saving a whole VM, and putting one back.
//!
//! This is the only public entry point most callers need: [`save`] writes a
//! machine that is already stopped at a lifecycle checkpoint, [`restore`] reads
//! one into a machine that has been assembled but not started, and [`inspect`]
//! answers "what is in this file?" without touching a VM at all — which is what
//! a manager listing snapshots wants.
//!
//! # The order, and why it is that order
//!
//! **Saving** writes metadata first, because the fingerprints are what a
//! restore refuses on and a reader should not have to walk a four-gigabyte
//! memory section to find out the disks have changed. Then the CPUs and the
//! clock, which are small; then guest memory, which is not.
//!
//! **Restoring** checks before it touches anything: the file's own integrity,
//! then the host, then the machine's shape, then every disk fingerprint. Only
//! after all of that does a byte of guest memory get written. A restore that
//! refuses has changed nothing, which is what lets `entangled resume` fail and
//! leave the snapshot usable.

use std::path::Path;
use std::time::{Duration, Instant};

use machine_x86::state::MachineState;
use vm_memory::GuestMemory;
use vmm_core::hv::{HostIrqChipState, VmClockState, X86CpuState};

use crate::cpu;
use crate::devices;
use crate::error::{Result, SnapshotError};
use crate::format::{HostKind, SectionKind, SnapshotReader, SnapshotWriter};
use crate::memory::{self, MemoryStats};
use crate::meta::{MachineShape, Metadata};

/// Everything [`save`] needs besides guest memory and a path.
pub struct SaveRequest<'a> {
    pub metadata: Metadata,
    pub host: HostKind,
    /// One per vCPU, in index order.
    pub cpus: &'a [X86CpuState],
    /// The VM-wide paravirtual clock, where the hypervisor has one.
    pub clock: Option<VmClockState>,
    /// The hypervisor's own interrupt controllers, where it has them.
    pub host_irqchip: Option<HostIrqChipState>,
    pub machine: &'a MachineState,
}

/// What one save cost.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SaveReport {
    /// Length of the file.
    pub bytes: u64,
    /// What the disk actually gave up for it, where the host can say.
    pub allocated_bytes: Option<u64>,
    pub memory: MemoryStats,
    pub elapsed: Duration,
    pub vcpus: usize,
}

impl SaveReport {
    /// The one-line summary the control channel and the log print.
    pub fn summary(&self) -> String {
        format!(
            "{} written in {:.2?} ({} of {} guest RAM in {} runs, {} vCPUs)",
            human(self.bytes),
            self.elapsed,
            human(self.memory.saved_bytes),
            human(self.memory.total_bytes),
            self.memory.runs,
            self.vcpus
        )
    }
}

fn human(bytes: u64) -> String {
    disk_image::format_bytes(bytes)
}

/// Writes a stopped VM to `path`.
///
/// The VM **must** be at a lifecycle stop point: every vCPU parked, every host
/// device worker quiesced. Nothing here checks that, because nothing here can —
/// it is the caller's contract, and `vmm_core::Lifecycle::save` is what
/// establishes it.
pub fn save<M: GuestMemory>(path: &Path, mem: &M, request: SaveRequest<'_>) -> Result<SaveReport> {
    let partial = partial_path(path);
    let outcome = save_partial(path, &partial, mem, request);
    if outcome.is_err() {
        // A failed suspend must not leave a `.part` beside the snapshot it did
        // not replace: the next one would overwrite it anyway, but a stray file
        // named after a VM invites somebody to try to restore it.
        let _ = std::fs::remove_file(&partial);
    }
    outcome
}

fn save_partial<M: GuestMemory>(
    path: &Path,
    partial: &Path,
    mem: &M,
    request: SaveRequest<'_>,
) -> Result<SaveReport> {
    let started = Instant::now();
    // Written beside the target and renamed at the end. A suspend that is
    // interrupted — a full disk, a killed process, a host that loses power —
    // must not leave a half-written file where a snapshot is supposed to be:
    // the digests would catch it on the way back in, but only after the user
    // had already lost the VM the file was replacing.
    let file =
        std::fs::File::create(partial).map_err(SnapshotError::io("creating the snapshot"))?;
    // Best effort, and only ever a space optimisation: the memory section skips
    // zero pages outright, so the file is small either way. Marking it sparse
    // is what keeps NTFS from committing clusters for the header the writer
    // seeks back over. (`disk_image::ops` owns every hole-aware file operation
    // in this workspace; this crate adds none of its own.)
    let _ = disk_image::ops::mark_sparse(&file);

    let mut writer = SnapshotWriter::create(file, request.host)?;
    writer.put(
        SectionKind::Metadata,
        crate::meta::METADATA_VERSION,
        0,
        &request.metadata.encode(),
    )?;
    for state in request.cpus {
        writer.put(
            SectionKind::Cpu,
            cpu::CPU_VERSION,
            state.index,
            &cpu::encode(state),
        )?;
    }
    if let Some(clock) = &request.clock {
        writer.put(
            SectionKind::Clock,
            cpu::CLOCK_VERSION,
            0,
            &cpu::encode_clock(clock),
        )?;
    }
    if let Some(chip) = &request.host_irqchip {
        writer.put(
            SectionKind::HostIrqChip,
            cpu::HOST_IRQCHIP_VERSION,
            0,
            &cpu::encode_host_irqchip(chip),
        )?;
    }
    for (kind, version, instance, payload) in devices::encode_machine(request.machine) {
        writer.put(kind, version, instance, &payload)?;
    }
    let stats = memory::save(mem, &mut writer)?;
    let (bytes, file) = writer.finish()?;
    file.sync_all()
        .map_err(SnapshotError::io("making the snapshot durable"))?;
    drop(file);
    std::fs::rename(partial, path).map_err(SnapshotError::io("publishing the snapshot"))?;

    Ok(SaveReport {
        bytes,
        allocated_bytes: disk_image::allocated_bytes(path),
        memory: stats,
        elapsed: started.elapsed(),
        vcpus: request.cpus.len(),
    })
}

/// Where [`save`] writes before it renames.
fn partial_path(path: &Path) -> std::path::PathBuf {
    let mut name = path.as_os_str().to_os_string();
    name.push(".part");
    std::path::PathBuf::from(name)
}

/// Everything [`restore`] hands back.
#[derive(Debug)]
pub struct Restored {
    pub metadata: Metadata,
    /// One per vCPU, in index order.
    pub cpus: Vec<X86CpuState>,
    pub clock: Option<VmClockState>,
    pub host_irqchip: Option<HostIrqChipState>,
    pub machine: MachineState,
    pub memory: MemoryStats,
    /// Advisory differences that did not justify a refusal (a firmware image
    /// that changed, say). Worth printing.
    pub notes: Vec<String>,
    pub elapsed: Duration,
}

impl Restored {
    pub fn summary(&self) -> String {
        format!(
            "{} of guest RAM in {} runs restored in {:.2?} ({} vCPUs)",
            human(self.memory.saved_bytes),
            self.memory.runs,
            self.elapsed,
            self.cpus.len()
        )
    }
}

/// Reads `path` into `mem` and returns everything the caller still has to
/// apply.
///
/// **Guest memory is written; nothing else is.** The device state and the CPU
/// state come back as values, because applying them needs objects this crate
/// does not have (the machine bus, the vCPUs) and because the order in which
/// they are applied is the caller's business.
///
/// `shape` is the machine that has just been assembled. Every way it can
/// disagree with the snapshot is checked *before* the first page is written, so
/// a refused restore leaves both the file and the fresh VM untouched.
pub fn restore<M: GuestMemory>(path: &Path, mem: &M, shape: &MachineShape) -> Result<Restored> {
    let started = Instant::now();
    let file = std::fs::File::open(path).map_err(SnapshotError::io("opening the snapshot"))?;
    let mut reader = SnapshotReader::open(file)?;
    reader.require_native()?;

    reader.require_version(SectionKind::Metadata, 0, crate::meta::METADATA_VERSION)?;
    let metadata = Metadata::decode(&reader.read_section(SectionKind::Metadata, 0)?)?;
    metadata.shape.check(shape)?;
    let notes = metadata.check_files()?;

    let mut cpus = Vec::new();
    for index in reader.instances(SectionKind::Cpu) {
        reader.require_version(SectionKind::Cpu, index, cpu::CPU_VERSION)?;
        let state = cpu::decode(&reader.read_section(SectionKind::Cpu, index)?)?;
        if state.index != index {
            return Err(SnapshotError::Mismatch {
                field: "vcpu index".into(),
                snapshot: state.index.to_string(),
                current: index.to_string(),
            });
        }
        cpus.push(state);
    }
    if cpus.len() as u32 != shape.vcpus {
        return Err(SnapshotError::Mismatch {
            field: "saved vCPUs".into(),
            snapshot: cpus.len().to_string(),
            current: shape.vcpus.to_string(),
        });
    }

    let clock = match reader.section_version(SectionKind::Clock, 0) {
        Some(_) => {
            reader.require_version(SectionKind::Clock, 0, cpu::CLOCK_VERSION)?;
            Some(cpu::decode_clock(
                &reader.read_section(SectionKind::Clock, 0)?,
            )?)
        }
        None => None,
    };

    let host_irqchip = match reader.section_version(SectionKind::HostIrqChip, 0) {
        Some(_) => {
            reader.require_version(SectionKind::HostIrqChip, 0, cpu::HOST_IRQCHIP_VERSION)?;
            Some(cpu::decode_host_irqchip(
                &reader.read_section(SectionKind::HostIrqChip, 0)?,
            )?)
        }
        None => None,
    };

    let machine = devices::decode_machine(&mut reader)?;
    // The device list is checked against the machine as well as against the
    // metadata: the metadata says what the *profile* described, this says what
    // was actually on the bus.
    MachineShape {
        devices: devices::device_slots(&machine),
        ..shape.clone()
    }
    .check(shape)?;

    let memory = memory::restore(mem, &mut reader)?;

    Ok(Restored {
        metadata,
        cpus,
        clock,
        host_irqchip,
        machine,
        memory,
        notes,
        elapsed: started.elapsed(),
    })
}

/// What [`inspect`] reports: everything readable without a VM.
#[derive(Debug, Clone)]
pub struct SnapshotInfo {
    pub metadata: Metadata,
    pub host: HostKind,
    pub file_bytes: u64,
    /// One entry per section: what it is, which instance, how big.
    pub sections: Vec<SnapshotSummary>,
    /// True when this build could restore it here (same host, same arch).
    pub restorable_here: bool,
    /// Why not, when it is not.
    pub refusal: Option<String>,
}

/// One line of a snapshot's contents.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SnapshotSummary {
    pub kind: SectionKind,
    pub instance: u32,
    pub bytes: u64,
}

/// Reads a snapshot's header, index and metadata without touching a VM.
///
/// The read-only view a GUI wants: name, when it was taken, how big, what it
/// contains, and whether this machine could restore it. Never opens a disk,
/// never allocates guest memory, and answers for a snapshot from the other host
/// too — it says so instead of failing.
pub fn inspect(path: &Path) -> Result<SnapshotInfo> {
    let file = std::fs::File::open(path).map_err(SnapshotError::io("opening the snapshot"))?;
    let mut reader = SnapshotReader::open(file)?;
    let host = reader.host();
    let file_bytes = reader.file_len();
    let sections: Vec<SnapshotSummary> = reader
        .sections()
        .iter()
        .map(|entry| SnapshotSummary {
            kind: entry.kind,
            instance: entry.instance,
            bytes: entry.len,
        })
        .collect();
    let refusal = reader.require_native().err().map(|e| e.to_string());
    reader.require_version(SectionKind::Metadata, 0, crate::meta::METADATA_VERSION)?;
    let metadata = Metadata::decode(&reader.read_section(SectionKind::Metadata, 0)?)?;
    Ok(SnapshotInfo {
        metadata,
        host,
        file_bytes,
        sections,
        restorable_here: refusal.is_none(),
        refusal,
    })
}

#[cfg(test)]
mod tests {
    use machine_x86::state::{SavedAcpiPm, SavedResetControl, SavedSerial};
    use vm_memory::{Bytes, GuestAddress, GuestMemoryMmap};
    use vmm_core::hv::{
        BlobFormat, MpState, X86DebugRegisters, X86Msr, X86OpaqueState, X86PendingEvents,
        X86Registers, X86SpecialRegisters,
    };

    use super::*;
    use crate::meta::{DeviceSlot, FileFingerprint, FileRole};

    fn temp_dir(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "entangled-snap-{tag}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn shape() -> MachineShape {
        MachineShape {
            vcpus: 1,
            memory_bytes: 4 << 20,
            transport: "pci".into(),
            boot_mode: "direct-linux".into(),
            devices: Vec::new(),
        }
    }

    fn metadata(files: Vec<FileFingerprint>) -> Metadata {
        Metadata {
            vm_name: "demo".into(),
            created_unix: crate::meta::now_unix(),
            writer: crate::writer_id(),
            config_toml: "name = \"demo\"\n".into(),
            shape: shape(),
            files,
        }
    }

    fn cpu_state(index: u32, rip: u64) -> X86CpuState {
        X86CpuState {
            index,
            registers: X86Registers {
                rip,
                ..X86Registers::default()
            },
            special_registers: X86SpecialRegisters::default(),
            msrs: vec![X86Msr {
                index: 0xc000_0080,
                data: 0xd01,
            }],
            xcr0: 7,
            xsave: X86OpaqueState::new(BlobFormat::XsaveArea, vec![0x11; 4096]),
            lapic: X86OpaqueState::new(BlobFormat::KvmLapicPage, vec![0x22; 1024]),
            mp_state: MpState::Runnable,
            events: X86PendingEvents::default(),
            debug_registers: X86DebugRegisters::default(),
        }
    }

    fn machine_state() -> MachineState {
        MachineState {
            serial: SavedSerial {
                ier: 1,
                rx: b"hi".to_vec(),
                ..SavedSerial::default()
            },
            acpi_pm: SavedAcpiPm {
                timer_ticks: 99,
                ..SavedAcpiPm::default()
            },
            reset: SavedResetControl::default(),
            ..MachineState::default()
        }
    }

    fn write_sample(path: &Path, mem: &GuestMemoryMmap, files: Vec<FileFingerprint>) -> SaveReport {
        let machine = machine_state();
        let cpus = [cpu_state(0, 0xffff_ffff_8100_0000)];
        save(
            path,
            mem,
            SaveRequest {
                metadata: metadata(files),
                host: HostKind::current().unwrap_or(HostKind::KvmLinux),
                cpus: &cpus,
                clock: Some(VmClockState {
                    clock_ns: 1_234_567,
                    ..VmClockState::default()
                }),
                host_irqchip: Some(HostIrqChipState {
                    pic_master: vec![1; 512],
                    pic_slave: vec![2; 512],
                    ioapic: vec![3; 512],
                    pit: vec![4; 49],
                }),
                machine: &machine,
            },
        )
        .expect("save")
    }

    fn memory(bytes: usize) -> GuestMemoryMmap {
        GuestMemoryMmap::from_ranges(&[(GuestAddress(0), bytes)]).unwrap()
    }

    #[test]
    fn a_whole_vm_round_trips() {
        let dir = temp_dir("roundtrip");
        let path = dir.join("vm.esnap");
        let source = memory(4 << 20);
        source
            .write_slice(b"guest bytes", GuestAddress(0x2000))
            .unwrap();

        let report = write_sample(&path, &source, Vec::new());
        assert_eq!(report.vcpus, 1);
        assert_eq!(report.memory.total_bytes, 4 << 20);
        assert!(report.memory.saved_bytes <= 4096, "{:?}", report.memory);

        let target = memory(4 << 20);
        let restored = restore(&path, &target, &shape()).expect("restore");
        assert_eq!(restored.cpus.len(), 1);
        assert_eq!(restored.cpus[0].registers.rip, 0xffff_ffff_8100_0000);
        assert_eq!(restored.cpus[0].msr(0xc000_0080), Some(0xd01));
        assert_eq!(restored.clock.unwrap().clock_ns, 1_234_567);
        let chip = restored.host_irqchip.expect("the in-kernel chips");
        assert_eq!(chip.ioapic, vec![3; 512]);
        assert_eq!(chip.pit.len(), 49);
        assert_eq!(restored.machine.serial.rx, b"hi".to_vec());
        assert_eq!(restored.machine.acpi_pm.timer_ticks, 99);
        let mut back = [0u8; 11];
        target.read_slice(&mut back, GuestAddress(0x2000)).unwrap();
        assert_eq!(&back, b"guest bytes");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The refusal that protects a filesystem: the disk moved, so the restore
    /// stops before it writes a page — and says which disk and what about it.
    #[test]
    fn a_modified_disk_is_refused_and_named() {
        let dir = temp_dir("disk");
        let path = dir.join("vm.esnap");
        let disk = dir.join("root.raw");
        std::fs::write(&disk, vec![0u8; 4096]).unwrap();

        let source = memory(4 << 20);
        source.write_slice(b"x", GuestAddress(0x1000)).unwrap();
        write_sample(
            &path,
            &source,
            vec![FileFingerprint::measure(FileRole::Disk, &disk)],
        );

        // Same VM, same everything — except that something wrote to the disk.
        std::fs::write(&disk, vec![0u8; 8192]).unwrap();
        let target = memory(4 << 20);
        let err = restore(&path, &target, &shape()).unwrap_err();
        assert!(
            matches!(&err, SnapshotError::DiskChanged { what: "size", .. }),
            "{err}"
        );
        assert!(err.to_string().contains("root.raw"), "{err}");
        // And nothing was written: the refusal came first.
        let mut byte = [0xffu8; 1];
        target.read_slice(&mut byte, GuestAddress(0x1000)).unwrap();
        assert_eq!(byte, [0]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_changed_machine_shape_is_refused_and_named() {
        let dir = temp_dir("shape");
        let path = dir.join("vm.esnap");
        let source = memory(4 << 20);
        write_sample(&path, &source, Vec::new());

        let target = memory(4 << 20);
        let mut grown = shape();
        grown.vcpus = 2;
        let err = restore(&path, &target, &grown).unwrap_err();
        assert!(
            matches!(&err, SnapshotError::Mismatch { field, .. } if field == "vcpus"),
            "{err}"
        );

        let mut moved = shape();
        moved.devices = vec![DeviceSlot {
            device_type: 2,
            slot: 0,
        }];
        let err = restore(&path, &target, &moved).unwrap_err();
        assert!(matches!(&err, SnapshotError::Mismatch { .. }), "{err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_truncated_snapshot_is_refused_at_every_length() {
        let dir = temp_dir("trunc");
        let path = dir.join("vm.esnap");
        let source = memory(4 << 20);
        source
            .write_slice(&[7u8; 8192], GuestAddress(0x3000))
            .unwrap();
        write_sample(&path, &source, Vec::new());
        let whole = std::fs::read(&path).unwrap();

        let target = memory(4 << 20);
        for cut in [0usize, 1, 71, 100, whole.len() / 2, whole.len() - 1] {
            let cut_path = dir.join(format!("cut-{cut}.esnap"));
            std::fs::write(&cut_path, &whole[..cut.min(whole.len())]).unwrap();
            let err = restore(&cut_path, &target, &shape()).unwrap_err();
            let _ = err.to_string();
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `inspect` answers without a VM, and says so when the snapshot could not
    /// be restored here.
    #[test]
    fn inspect_reads_a_snapshot_without_a_vm() {
        let dir = temp_dir("inspect");
        let path = dir.join("vm.esnap");
        let source = memory(4 << 20);
        write_sample(&path, &source, Vec::new());

        let info = inspect(&path).expect("inspect");
        assert_eq!(info.metadata.vm_name, "demo");
        assert_eq!(info.metadata.shape.vcpus, 1);
        assert!(info.file_bytes > 0);
        assert!(info
            .sections
            .iter()
            .any(|s| s.kind == SectionKind::Cpu && s.instance == 0));
        assert!(info.sections.iter().any(|s| s.kind == SectionKind::Memory));
        assert!(info.restorable_here, "{:?}", info.refusal);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_snapshot_from_the_other_host_is_refused_by_name() {
        let dir = temp_dir("host");
        let path = dir.join("vm.esnap");
        let source = memory(4 << 20);
        let other = match HostKind::current() {
            Some(HostKind::KvmLinux) => HostKind::WhpWindows,
            _ => HostKind::KvmLinux,
        };
        let machine = machine_state();
        let cpus = [cpu_state(0, 0x1000)];
        save(
            &path,
            &source,
            SaveRequest {
                metadata: metadata(Vec::new()),
                host: other,
                cpus: &cpus,
                clock: None,
                host_irqchip: None,
                machine: &machine,
            },
        )
        .expect("save");

        let target = memory(4 << 20);
        let err = restore(&path, &target, &shape()).unwrap_err();
        assert!(
            matches!(
                err,
                SnapshotError::ForeignHost { .. } | SnapshotError::ForeignArch { .. }
            ),
            "{err}"
        );
        // But `inspect` still reads it, and says why it cannot be restored.
        let info = inspect(&path).expect("inspect");
        assert!(!info.restorable_here);
        assert!(info.refusal.is_some());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The ratio the whole memory section exists for.
    #[test]
    fn an_idle_guest_produces_a_small_file() {
        let dir = temp_dir("ratio");
        let path = dir.join("vm.esnap");
        // 64 MiB of guest RAM with 1 MiB touched.
        let source = GuestMemoryMmap::from_ranges(&[(GuestAddress(0), 64 << 20)]).unwrap();
        let touched = vec![0xa5u8; 1 << 20];
        source
            .write_slice(&touched, GuestAddress(0x10_0000))
            .unwrap();

        let report = write_sample(&path, &source, Vec::new());
        assert_eq!(report.memory.total_bytes, 64 << 20);
        assert_eq!(report.memory.saved_bytes, 1 << 20);
        assert!(
            report.bytes < 2 << 20,
            "a 64 MiB guest with 1 MiB touched produced {} bytes",
            report.bytes
        );
        eprintln!("{}", report.summary());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
