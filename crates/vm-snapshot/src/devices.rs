//! Device state on disk: the virtio transports and the machine's own devices.
//!
//! One section per device class, each with its own version, so a change to (say)
//! how the 8254 is written down does not invalidate every snapshot ever taken —
//! it invalidates the ones with an 8254 in them, by number, with a message that
//! says so.
//!
//! The state *shapes* live with the things that own them (`virtio_core::save`,
//! `machine_x86::state`). This module owns only their spelling, which is why a
//! format change is a change to this file and nowhere else.

use machine_x86::state::{
    MachineState, SavedAcpiPm, SavedIoApic, SavedIrqChip, SavedPciFunction, SavedPciRoot,
    SavedPflash, SavedPic, SavedPicChip, SavedPit, SavedPitChannel, SavedPlatform,
    SavedResetControl, SavedRtc, SavedSerial, SavedVirtioSlot,
};
use virtio_core::save::{
    InterruptState, MsixEntryState, MsixState, QueuePosition, QueueState, TransportSaveState,
};

use crate::codec::{Reader, Writer};
use crate::error::Result;
#[cfg(test)]
use crate::error::SnapshotError;

/// Version of the virtio section's encoding.
pub const VIRTIO_VERSION: u32 = 1;
/// Version of the 16550 section's encoding.
pub const SERIAL_VERSION: u32 = 1;
/// Version of the firmware-platform section's encoding.
pub const PLATFORM_VERSION: u32 = 1;
/// Version of the ACPI PM section's encoding.
pub const ACPI_PM_VERSION: u32 = 1;
/// Version of the pflash section's encoding.
pub const PFLASH_VERSION: u32 = 1;
/// Version of the PCI root section's encoding.
pub const PCI_ROOT_VERSION: u32 = 1;
/// Version of the interrupt-chip section's encoding.
pub const IRQCHIP_VERSION: u32 = 1;
/// Version of the reset-control section's encoding.
pub const RESET_VERSION: u32 = 1;

/// Queues one virtio device may have. Far above the two or three real ones.
const MAX_QUEUES: usize = 64;
/// MSI-X vectors one function may publish.
const MAX_VECTORS: usize = 2048;
/// Bytes of device-specific state one device may carry (virtio-gpu's resource
/// table is the largest, and it is kilobytes).
const MAX_DEVICE_BLOB: usize = 4 << 20;
/// Bytes the 16550's receive queue may hold.
const MAX_SERIAL_RX: usize = 64 << 10;
/// CMOS bytes.
const MAX_CMOS: usize = 4096;
/// Dwords in one PCI function's configuration space.
const MAX_CONFIG_DWORDS: usize = 1024;
/// Functions on the PCI root bus.
const MAX_PCI_FUNCTIONS: usize = 64;
/// IOAPIC redirection entries.
const MAX_RTES: usize = 256;
/// 8254 channels.
const MAX_PIT_CHANNELS: usize = 8;

// ------------------------------------------------------------------- virtio

fn put_queue(w: &mut Writer, q: &QueueState) {
    w.u16(q.size)
        .bool(q.ready)
        .u64(q.desc_table)
        .u64(q.driver_area)
        .u64(q.device_area)
        .u16(q.position.next_avail)
        .u16(q.position.next_used);
}

fn get_queue(r: &mut Reader<'_>) -> Result<QueueState> {
    Ok(QueueState {
        size: r.u16("queue size")?,
        ready: r.bool("queue ready")?,
        desc_table: r.u64("queue desc table")?,
        driver_area: r.u64("queue driver area")?,
        device_area: r.u64("queue device area")?,
        position: QueuePosition {
            next_avail: r.u16("queue next_avail")?,
            next_used: r.u16("queue next_used")?,
        },
    })
}

fn put_interrupt(w: &mut Writer, i: &InterruptState) {
    w.u32(i.isr).u32(i.generation);
    match &i.msix {
        Some(msix) => {
            w.bool(true)
                .u32(msix.control)
                .u16(msix.config_vector)
                .count(msix.queue_vectors.len());
            for vector in &msix.queue_vectors {
                w.u16(*vector);
            }
            w.count(msix.entries.len());
            for entry in &msix.entries {
                w.u32(entry.address_lo)
                    .u32(entry.address_hi)
                    .u32(entry.data)
                    .u32(entry.vector_control);
            }
            w.count(msix.pending.len());
            for word in &msix.pending {
                w.u64(*word);
            }
        }
        None => {
            w.bool(false);
        }
    }
}

fn get_interrupt(r: &mut Reader<'_>) -> Result<InterruptState> {
    let isr = r.u32("isr")?;
    let generation = r.u32("config generation")?;
    let msix = if r.bool("msix present")? {
        let control = r.u32("msix control")?;
        let config_vector = r.u16("msix config vector")?;
        let queue_count = r.count("msix queue vectors", MAX_QUEUES, 2)?;
        let mut queue_vectors = Vec::with_capacity(queue_count);
        for _ in 0..queue_count {
            queue_vectors.push(r.u16("msix queue vector")?);
        }
        let entry_count = r.count("msix entries", MAX_VECTORS, 16)?;
        let mut entries = Vec::with_capacity(entry_count);
        for _ in 0..entry_count {
            entries.push(MsixEntryState {
                address_lo: r.u32("msix address lo")?,
                address_hi: r.u32("msix address hi")?,
                data: r.u32("msix data")?,
                vector_control: r.u32("msix vector control")?,
            });
        }
        let pending_count = r.count("msix pending words", MAX_VECTORS, 8)?;
        let mut pending = Vec::with_capacity(pending_count);
        for _ in 0..pending_count {
            pending.push(r.u64("msix pending word")?);
        }
        Some(MsixState {
            control,
            config_vector,
            queue_vectors,
            entries,
            pending,
        })
    } else {
        None
    };
    Ok(InterruptState {
        isr,
        generation,
        msix,
    })
}

/// Encodes one virtio slot.
pub fn encode_virtio(state: &TransportSaveState) -> Vec<u8> {
    let mut w = Writer::with_capacity(512 + state.device.len());
    w.u32(state.device_type)
        .u64(state.device_features)
        .u32(state.device_features_sel)
        .u64(state.driver_features)
        .u32(state.driver_features_sel)
        .u32(state.queue_sel)
        .u32(state.status)
        .bool(state.activated)
        .count(state.queues.len());
    for queue in &state.queues {
        put_queue(&mut w, queue);
    }
    put_interrupt(&mut w, &state.interrupt);
    w.blob(&state.device);
    w.into_bytes()
}

/// Decodes one virtio slot.
pub fn decode_virtio(bytes: &[u8]) -> Result<TransportSaveState> {
    let mut r = Reader::new(bytes);
    let device_type = r.u32("device type")?;
    let device_features = r.u64("device features")?;
    let device_features_sel = r.u32("device features selector")?;
    let driver_features = r.u64("driver features")?;
    let driver_features_sel = r.u32("driver features selector")?;
    let queue_sel = r.u32("queue selector")?;
    let status = r.u32("device status")?;
    let activated = r.bool("activated")?;
    let count = r.count("queues", MAX_QUEUES, 29)?;
    let mut queues = Vec::with_capacity(count);
    for _ in 0..count {
        queues.push(get_queue(&mut r)?);
    }
    let interrupt = get_interrupt(&mut r)?;
    let device = r.blob("device state", MAX_DEVICE_BLOB)?.to_vec();
    r.finish("virtio section")?;
    Ok(TransportSaveState {
        device_type,
        device_features,
        device_features_sel,
        driver_features,
        driver_features_sel,
        queue_sel,
        status,
        activated,
        queues,
        interrupt,
        device,
    })
}

// ------------------------------------------------------------------- serial

/// Encodes the 16550.
pub fn encode_serial(state: &SavedSerial) -> Vec<u8> {
    let mut w = Writer::with_capacity(64 + state.rx.len());
    w.u8(state.ier)
        .u8(state.lcr)
        .u8(state.mcr)
        .u8(state.scr)
        .u8(state.dll)
        .u8(state.dlh)
        .bool(state.thre_pending)
        .blob(&state.rx);
    w.into_bytes()
}

/// Decodes the 16550.
pub fn decode_serial(bytes: &[u8]) -> Result<SavedSerial> {
    let mut r = Reader::new(bytes);
    let state = SavedSerial {
        ier: r.u8("ier")?,
        lcr: r.u8("lcr")?,
        mcr: r.u8("mcr")?,
        scr: r.u8("scr")?,
        dll: r.u8("dll")?,
        dlh: r.u8("dlh")?,
        thre_pending: r.bool("thre pending")?,
        rx: r.blob("serial receive queue", MAX_SERIAL_RX)?.to_vec(),
    };
    r.finish("serial section")?;
    Ok(state)
}

// ----------------------------------------------------------------- platform

/// Encodes the firmware platform stub (RTC + host-bridge latch).
pub fn encode_platform(state: &SavedPlatform) -> Vec<u8> {
    let mut w = Writer::with_capacity(64 + state.rtc.cmos.len());
    w.u32(state.config_address)
        .u8(state.rtc.index)
        .blob(&state.rtc.cmos);
    w.into_bytes()
}

/// Decodes it.
pub fn decode_platform(bytes: &[u8]) -> Result<SavedPlatform> {
    let mut r = Reader::new(bytes);
    let state = SavedPlatform {
        config_address: r.u32("config address")?,
        rtc: SavedRtc {
            index: r.u8("rtc index")?,
            cmos: r.blob("cmos", MAX_CMOS)?.to_vec(),
        },
    };
    r.finish("platform section")?;
    Ok(state)
}

// ------------------------------------------------------------------ acpi pm

/// Encodes the ACPI PM block.
pub fn encode_acpi_pm(state: &SavedAcpiPm) -> Vec<u8> {
    let mut w = Writer::with_capacity(32);
    w.u16(state.pm1a_sts)
        .u16(state.pm1a_en)
        .u16(state.pm1a_cnt)
        .u16(state.gpe0_sts)
        .u16(state.gpe0_en)
        .u8(state.sleep_control)
        .u8(state.sleep_status)
        .u32(state.timer_ticks)
        .bool(state.shutdown_requested);
    w.into_bytes()
}

/// Decodes it.
pub fn decode_acpi_pm(bytes: &[u8]) -> Result<SavedAcpiPm> {
    let mut r = Reader::new(bytes);
    let state = SavedAcpiPm {
        pm1a_sts: r.u16("pm1a_sts")?,
        pm1a_en: r.u16("pm1a_en")?,
        pm1a_cnt: r.u16("pm1a_cnt")?,
        gpe0_sts: r.u16("gpe0_sts")?,
        gpe0_en: r.u16("gpe0_en")?,
        sleep_control: r.u8("sleep control")?,
        sleep_status: r.u8("sleep status")?,
        timer_ticks: r.u32("pm timer ticks")?,
        shutdown_requested: r.bool("shutdown latch")?,
    };
    r.finish("acpi-pm section")?;
    Ok(state)
}

// ------------------------------------------------------------------- pflash

/// Encodes the CFI command state machine.
pub fn encode_pflash(state: &SavedPflash) -> Vec<u8> {
    let mut w = Writer::with_capacity(2);
    w.u8(state.command).u8(state.status);
    w.into_bytes()
}

/// Decodes it.
pub fn decode_pflash(bytes: &[u8]) -> Result<SavedPflash> {
    let mut r = Reader::new(bytes);
    let state = SavedPflash {
        command: r.u8("pflash command")?,
        status: r.u8("pflash status")?,
    };
    r.finish("pflash section")?;
    Ok(state)
}

// ----------------------------------------------------------------- pci root

/// Encodes the PCI root bus.
pub fn encode_pci_root(state: &SavedPciRoot) -> Vec<u8> {
    let mut w = Writer::with_capacity(64 + state.functions.len() * 264);
    w.u32(state.address).count(state.functions.len());
    for function in &state.functions {
        w.u8(function.device).count(function.regs.len());
        for reg in &function.regs {
            w.u32(*reg);
        }
    }
    w.into_bytes()
}

/// Decodes it.
pub fn decode_pci_root(bytes: &[u8]) -> Result<SavedPciRoot> {
    let mut r = Reader::new(bytes);
    let address = r.u32("config address")?;
    let count = r.count("pci functions", MAX_PCI_FUNCTIONS, 9)?;
    let mut functions = Vec::with_capacity(count);
    for _ in 0..count {
        let device = r.u8("pci device number")?;
        let dwords = r.count("pci config dwords", MAX_CONFIG_DWORDS, 4)?;
        let mut regs = Vec::with_capacity(dwords);
        for _ in 0..dwords {
            regs.push(r.u32("pci config dword")?);
        }
        functions.push(SavedPciFunction { device, regs });
    }
    r.finish("pci-root section")?;
    Ok(SavedPciRoot { address, functions })
}

// ------------------------------------------------------------------ irqchip

fn put_pic_chip(w: &mut Writer, chip: &SavedPicChip) {
    w.u8(chip.imr)
        .u8(chip.irr)
        .u8(chip.isr)
        .u8(chip.vector_base)
        .u8(chip.cascade)
        .u8(chip.icw4)
        .u8(chip.step)
        .bool(chip.expect_icw4)
        .bool(chip.expect_icw3)
        .bool(chip.read_isr)
        .u8(chip.elcr);
}

fn get_pic_chip(r: &mut Reader<'_>) -> Result<SavedPicChip> {
    Ok(SavedPicChip {
        imr: r.u8("pic imr")?,
        irr: r.u8("pic irr")?,
        isr: r.u8("pic isr")?,
        vector_base: r.u8("pic vector base")?,
        cascade: r.u8("pic cascade")?,
        icw4: r.u8("pic icw4")?,
        step: r.u8("pic init step")?,
        expect_icw4: r.bool("pic expect icw4")?,
        expect_icw3: r.bool("pic expect icw3")?,
        read_isr: r.bool("pic read isr")?,
        elcr: r.u8("pic elcr")?,
    })
}

/// An `Option<u8>`/`Option<u16>` as a present flag plus the value.
fn put_opt_u8(w: &mut Writer, value: Option<u8>) {
    w.bool(value.is_some()).u8(value.unwrap_or(0));
}

fn get_opt_u8(r: &mut Reader<'_>, what: &'static str) -> Result<Option<u8>> {
    let present = r.bool(what)?;
    let value = r.u8(what)?;
    Ok(present.then_some(value))
}

fn put_opt_u16(w: &mut Writer, value: Option<u16>) {
    w.bool(value.is_some()).u16(value.unwrap_or(0));
}

fn get_opt_u16(r: &mut Reader<'_>, what: &'static str) -> Result<Option<u16>> {
    let present = r.bool(what)?;
    let value = r.u16(what)?;
    Ok(present.then_some(value))
}

fn put_opt_u64(w: &mut Writer, value: Option<u64>) {
    w.bool(value.is_some()).u64(value.unwrap_or(0));
}

fn get_opt_u64(r: &mut Reader<'_>, what: &'static str) -> Result<Option<u64>> {
    let present = r.bool(what)?;
    let value = r.u64(what)?;
    Ok(present.then_some(value))
}

/// Encodes the 8259/8254/IOAPIC set.
pub fn encode_irqchip(state: &SavedIrqChip) -> Vec<u8> {
    let mut w = Writer::with_capacity(512);
    put_pic_chip(&mut w, &state.pic.master);
    put_pic_chip(&mut w, &state.pic.slave);
    w.count(state.pit.channels.len());
    for channel in &state.pit.channels {
        w.u16(channel.reload)
            .u8(channel.mode)
            .u8(channel.access)
            .u64(channel.ticks_since_armed)
            .bool(channel.armed);
        put_opt_u8(&mut w, channel.write_lo);
        put_opt_u16(&mut w, channel.latched);
        w.bool(channel.read_hi_next).bool(channel.gate);
    }
    w.u8(state.pit.nmi_control);
    put_opt_u64(&mut w, state.pit.ticks_to_next_edge);
    w.u32(state.ioapic.select)
        .u32(state.ioapic.pending)
        .count(state.ioapic.redirection.len());
    for entry in &state.ioapic.redirection {
        w.u64(*entry);
    }
    w.into_bytes()
}

/// Decodes it.
pub fn decode_irqchip(bytes: &[u8]) -> Result<SavedIrqChip> {
    let mut r = Reader::new(bytes);
    let pic = SavedPic {
        master: get_pic_chip(&mut r)?,
        slave: get_pic_chip(&mut r)?,
    };
    let channel_count = r.count("8254 channels", MAX_PIT_CHANNELS, 19)?;
    let mut channels = Vec::with_capacity(channel_count);
    for _ in 0..channel_count {
        channels.push(SavedPitChannel {
            reload: r.u16("pit reload")?,
            mode: r.u8("pit mode")?,
            access: r.u8("pit access mode")?,
            ticks_since_armed: r.u64("pit ticks since armed")?,
            armed: r.bool("pit armed")?,
            write_lo: get_opt_u8(&mut r, "pit half-written reload")?,
            latched: get_opt_u16(&mut r, "pit latched count")?,
            read_hi_next: r.bool("pit read hi next")?,
            gate: r.bool("pit gate")?,
        });
    }
    let nmi_control = r.u8("pit nmi control")?;
    let ticks_to_next_edge = get_opt_u64(&mut r, "pit next edge")?;
    let select = r.u32("ioapic select")?;
    let pending = r.u32("ioapic pending")?;
    let rte_count = r.count("ioapic redirection entries", MAX_RTES, 8)?;
    let mut redirection = Vec::with_capacity(rte_count);
    for _ in 0..rte_count {
        redirection.push(r.u64("ioapic redirection entry")?);
    }
    r.finish("irqchip section")?;
    Ok(SavedIrqChip {
        pic,
        pit: SavedPit {
            channels,
            nmi_control,
            ticks_to_next_edge,
        },
        ioapic: SavedIoApic {
            select,
            redirection,
            pending,
        },
    })
}

// ------------------------------------------------------------ reset control

/// Encodes the guest reset latches.
pub fn encode_reset(state: &SavedResetControl) -> Vec<u8> {
    let mut w = Writer::with_capacity(9);
    w.u32(state.rcr).bool(state.requested).u32(state.count);
    w.into_bytes()
}

/// Decodes them.
pub fn decode_reset(bytes: &[u8]) -> Result<SavedResetControl> {
    let mut r = Reader::new(bytes);
    let state = SavedResetControl {
        rcr: r.u32("reset control register")?,
        requested: r.bool("reset requested")?,
        count: r.u32("reset count")?,
    };
    r.finish("reset-control section")?;
    Ok(state)
}

// ------------------------------------------------------------- the whole bus

/// Every section a [`MachineState`] produces, as `(kind, version, instance,
/// payload)` tuples ready for the container.
pub fn encode_machine(state: &MachineState) -> Vec<(crate::SectionKind, u32, u32, Vec<u8>)> {
    use crate::SectionKind as K;
    let mut out = vec![
        (K::Serial, SERIAL_VERSION, 0, encode_serial(&state.serial)),
        (
            K::AcpiPm,
            ACPI_PM_VERSION,
            0,
            encode_acpi_pm(&state.acpi_pm),
        ),
        (
            K::ResetControl,
            RESET_VERSION,
            0,
            encode_reset(&state.reset),
        ),
    ];
    if let Some(platform) = &state.platform {
        out.push((K::Platform, PLATFORM_VERSION, 0, encode_platform(platform)));
    }
    if let Some(pflash) = &state.pflash {
        out.push((K::Pflash, PFLASH_VERSION, 0, encode_pflash(pflash)));
    }
    if let Some(pci) = &state.pci_root {
        out.push((K::PciRoot, PCI_ROOT_VERSION, 0, encode_pci_root(pci)));
    }
    if let Some(chip) = &state.irqchip {
        out.push((K::IrqChip, IRQCHIP_VERSION, 0, encode_irqchip(chip)));
    }
    for slot in &state.virtio {
        out.push((
            K::Virtio,
            VIRTIO_VERSION,
            slot.slot,
            encode_virtio(&slot.state),
        ));
    }
    out
}

/// Reads a whole [`MachineState`] back out of a snapshot.
pub fn decode_machine<R: std::io::Read + std::io::Seek + std::fmt::Debug>(
    reader: &mut crate::SnapshotReader<R>,
) -> Result<MachineState> {
    use crate::SectionKind as K;

    let optional = |reader: &mut crate::SnapshotReader<R>,
                    kind: crate::SectionKind,
                    version: u32|
     -> Result<Option<Vec<u8>>> {
        if reader.section_version(kind, 0).is_none() {
            return Ok(None);
        }
        reader.require_version(kind, 0, version)?;
        reader.read_section(kind, 0).map(Some)
    };

    reader.require_version(K::Serial, 0, SERIAL_VERSION)?;
    let serial = decode_serial(&reader.read_section(K::Serial, 0)?)?;
    reader.require_version(K::AcpiPm, 0, ACPI_PM_VERSION)?;
    let acpi_pm = decode_acpi_pm(&reader.read_section(K::AcpiPm, 0)?)?;
    reader.require_version(K::ResetControl, 0, RESET_VERSION)?;
    let reset = decode_reset(&reader.read_section(K::ResetControl, 0)?)?;

    let platform = optional(reader, K::Platform, PLATFORM_VERSION)?
        .map(|bytes| decode_platform(&bytes))
        .transpose()?;
    let pflash = optional(reader, K::Pflash, PFLASH_VERSION)?
        .map(|bytes| decode_pflash(&bytes))
        .transpose()?;
    let pci_root = optional(reader, K::PciRoot, PCI_ROOT_VERSION)?
        .map(|bytes| decode_pci_root(&bytes))
        .transpose()?;
    let irqchip = optional(reader, K::IrqChip, IRQCHIP_VERSION)?
        .map(|bytes| decode_irqchip(&bytes))
        .transpose()?;

    let mut virtio = Vec::new();
    for slot in reader.instances(K::Virtio) {
        reader.require_version(K::Virtio, slot, VIRTIO_VERSION)?;
        let bytes = reader.read_section(K::Virtio, slot)?;
        virtio.push(SavedVirtioSlot {
            slot,
            state: decode_virtio(&bytes)?,
        });
    }

    Ok(MachineState {
        serial,
        acpi_pm,
        reset,
        platform,
        pflash,
        pci_root,
        irqchip,
        virtio,
    })
}

/// Which virtio device types, in slot order, a saved machine describes.
///
/// The [`crate::meta::MachineShape`] check compares this against the machine
/// being restored into, so a snapshot whose `/dev/vda` was somebody else's disk
/// is refused before anything is loaded.
pub fn device_slots(state: &MachineState) -> Vec<crate::meta::DeviceSlot> {
    state
        .virtio
        .iter()
        .map(|slot| crate::meta::DeviceSlot {
            device_type: slot.state.device_type,
            slot: slot.slot,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_virtio() -> TransportSaveState {
        TransportSaveState {
            device_type: 2,
            device_features: 0x1_0000_0007,
            device_features_sel: 1,
            driver_features: 0x1_0000_0005,
            driver_features_sel: 0,
            queue_sel: 0,
            status: 0x0f,
            activated: true,
            queues: vec![QueueState {
                size: 256,
                ready: true,
                desc_table: 0x1234_0000,
                driver_area: 0x1234_1000,
                device_area: 0x1234_2000,
                position: QueuePosition {
                    next_avail: 4242,
                    next_used: 4242,
                },
            }],
            interrupt: InterruptState {
                isr: 1,
                generation: 3,
                msix: Some(MsixState {
                    control: 0x8000_0011,
                    config_vector: 0,
                    queue_vectors: vec![1],
                    entries: vec![
                        MsixEntryState {
                            address_lo: 0xfee0_0000,
                            address_hi: 0,
                            data: 0x30,
                            vector_control: 0,
                        },
                        MsixEntryState::default(),
                    ],
                    pending: vec![0b10],
                }),
            },
            device: vec![1, 2, 3],
        }
    }

    fn sample_machine() -> MachineState {
        MachineState {
            serial: SavedSerial {
                ier: 1,
                lcr: 3,
                mcr: 8,
                scr: 0x5a,
                dll: 1,
                dlh: 0,
                thre_pending: true,
                rx: b"login: ".to_vec(),
            },
            acpi_pm: SavedAcpiPm {
                pm1a_sts: 1,
                pm1a_en: 0x20,
                pm1a_cnt: 0x1c00,
                gpe0_sts: 0,
                gpe0_en: 0,
                sleep_control: 0,
                sleep_status: 0,
                timer_ticks: 123_456,
                shutdown_requested: false,
            },
            reset: SavedResetControl {
                rcr: 0x0e,
                requested: true,
                count: 2,
            },
            platform: Some(SavedPlatform {
                config_address: 0x8000_0010,
                rtc: SavedRtc {
                    index: 0x0b,
                    cmos: vec![0u8; 128],
                },
            }),
            pflash: Some(SavedPflash {
                command: 2,
                status: 0x80,
            }),
            pci_root: Some(SavedPciRoot {
                address: 0x8000_1010,
                functions: vec![SavedPciFunction {
                    device: 1,
                    regs: vec![0xdead_beef; 64],
                }],
            }),
            irqchip: Some(SavedIrqChip {
                pic: SavedPic {
                    master: SavedPicChip {
                        imr: 0xfb,
                        step: 2,
                        expect_icw4: true,
                        elcr: 0x0c,
                        ..SavedPicChip::default()
                    },
                    slave: SavedPicChip {
                        imr: 0xff,
                        ..SavedPicChip::default()
                    },
                },
                pit: SavedPit {
                    channels: vec![
                        SavedPitChannel {
                            reload: 11932,
                            mode: 2,
                            access: 3,
                            ticks_since_armed: 5000,
                            armed: true,
                            write_lo: Some(0x9c),
                            latched: Some(1234),
                            read_hi_next: true,
                            gate: true,
                        },
                        SavedPitChannel::default(),
                        SavedPitChannel::default(),
                    ],
                    nmi_control: 3,
                    ticks_to_next_edge: Some(6932),
                },
                ioapic: SavedIoApic {
                    select: 0x12,
                    redirection: vec![1 << 16; 24],
                    pending: 0b100,
                },
            }),
            virtio: vec![SavedVirtioSlot {
                slot: 0,
                state: sample_virtio(),
            }],
        }
    }

    #[test]
    fn a_virtio_slot_round_trips() {
        let state = sample_virtio();
        assert_eq!(decode_virtio(&encode_virtio(&state)).unwrap(), state);
    }

    #[test]
    fn a_virtio_slot_without_msix_round_trips() {
        let mut state = sample_virtio();
        state.interrupt.msix = None;
        assert_eq!(decode_virtio(&encode_virtio(&state)).unwrap(), state);
    }

    #[test]
    fn every_machine_section_round_trips() {
        let machine = sample_machine();
        assert_eq!(
            decode_serial(&encode_serial(&machine.serial)).unwrap(),
            machine.serial
        );
        assert_eq!(
            decode_acpi_pm(&encode_acpi_pm(&machine.acpi_pm)).unwrap(),
            machine.acpi_pm
        );
        assert_eq!(
            decode_reset(&encode_reset(&machine.reset)).unwrap(),
            machine.reset
        );
        let platform = machine.platform.clone().unwrap();
        assert_eq!(
            decode_platform(&encode_platform(&platform)).unwrap(),
            platform
        );
        let pflash = machine.pflash.unwrap();
        assert_eq!(decode_pflash(&encode_pflash(&pflash)).unwrap(), pflash);
        let pci = machine.pci_root.clone().unwrap();
        assert_eq!(decode_pci_root(&encode_pci_root(&pci)).unwrap(), pci);
        let chip = machine.irqchip.clone().unwrap();
        assert_eq!(decode_irqchip(&encode_irqchip(&chip)).unwrap(), chip);
    }

    /// The half-written PIT reload and the latched count are `Option`s, and an
    /// `Option` that came back as `Some(0)` instead of `None` is a counter the
    /// guest would read wrong.
    #[test]
    fn the_pit_options_keep_their_absence() {
        let mut chip = sample_machine().irqchip.unwrap();
        chip.pit.channels[0].write_lo = None;
        chip.pit.channels[0].latched = None;
        chip.pit.ticks_to_next_edge = None;
        let back = decode_irqchip(&encode_irqchip(&chip)).unwrap();
        assert_eq!(back, chip);
        assert!(back.pit.channels[0].write_lo.is_none());
        assert!(back.pit.ticks_to_next_edge.is_none());
    }

    /// One encoded section plus the decoder that must refuse every prefix of
    /// it.
    type Probe = (Vec<u8>, fn(&[u8]) -> bool);

    #[test]
    fn every_truncation_of_every_section_is_an_error() {
        let machine = sample_machine();
        let sections: Vec<Probe> = vec![
            (encode_serial(&machine.serial), |b| {
                decode_serial(b).is_err()
            }),
            (encode_acpi_pm(&machine.acpi_pm), |b| {
                decode_acpi_pm(b).is_err()
            }),
            (encode_reset(&machine.reset), |b| decode_reset(b).is_err()),
            (encode_platform(machine.platform.as_ref().unwrap()), |b| {
                decode_platform(b).is_err()
            }),
            (encode_pci_root(machine.pci_root.as_ref().unwrap()), |b| {
                decode_pci_root(b).is_err()
            }),
            (encode_irqchip(machine.irqchip.as_ref().unwrap()), |b| {
                decode_irqchip(b).is_err()
            }),
            (encode_virtio(&sample_virtio()), |b| {
                decode_virtio(b).is_err()
            }),
        ];
        for (bytes, is_err) in sections {
            for cut in 0..bytes.len() {
                assert!(is_err(&bytes[..cut]), "a {cut}-byte prefix decoded");
            }
        }
    }

    #[test]
    fn the_device_slot_list_follows_the_bus() {
        let machine = sample_machine();
        assert_eq!(
            device_slots(&machine),
            vec![crate::meta::DeviceSlot {
                device_type: 2,
                slot: 0
            }]
        );
    }

    #[test]
    fn an_absurd_queue_count_is_refused() {
        let mut w = Writer::new();
        w.u32(2)
            .u64(0)
            .u32(0)
            .u64(0)
            .u32(0)
            .u32(0)
            .u32(0)
            .bool(false)
            .u64(u64::MAX);
        let err = decode_virtio(&w.into_bytes()).unwrap_err();
        assert!(matches!(err, SnapshotError::TooLarge { .. }), "{err}");
    }
}
