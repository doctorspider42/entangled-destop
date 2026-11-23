//! A dual-channel IDE controller (ADR-0012).
//!
//! Windows XP has no driver for AHCI or for anything virtio, but every PC it
//! was written for had this: two channels at the fixed ports `0x1f0`/`0x3f6`
//! and `0x170`/`0x376`, on IRQ 14 and 15, each with up to two drives. This is
//! that controller with ATA disks ([`Disk`]) and ATAPI drives
//! ([`ahci::Atapi`], the same packet command set the AHCI controller serves).
//!
//! **PIO only.** Every sector goes through the data port, a word at a time,
//! and the transfer happens on the vCPU thread that did the port access; the
//! controller has no thread of its own and no bus master. The identify data
//! says so (no DMA modes), and a command that asks for DMA is aborted.
//!
//! The guest is untrusted: register values are bytes, every LBA and count is
//! checked against the drive, and nothing here indexes with a value the
//! guest chose.

mod disk;

use std::sync::Arc;

use ahci::{Atapi, Reply};
use virtio_core::interrupt::IrqLine;

pub use disk::{Disk, SECTOR};

/// The command block and the control port of each channel.
pub const PRIMARY_BASE: u16 = 0x1f0;
pub const PRIMARY_CONTROL: u16 = 0x3f6;
pub const SECONDARY_BASE: u16 = 0x170;
pub const SECONDARY_CONTROL: u16 = 0x376;
/// The ISA interrupts the channels raise.
pub const PRIMARY_IRQ: u8 = 14;
pub const SECONDARY_IRQ: u8 = 15;

const STATUS_ERR: u8 = 0x01;
const STATUS_DRQ: u8 = 0x08;
const STATUS_DSC: u8 = 0x10;
const STATUS_DRDY: u8 = 0x40;
const STATUS_READY: u8 = STATUS_DRDY | STATUS_DSC;

const ERROR_ABRT: u8 = 0x04;
const ERROR_IDNF: u8 = 0x10;
const ERROR_UNC: u8 = 0x40;

const CONTROL_NIEN: u8 = 0x02;
const CONTROL_SRST: u8 = 0x04;
const CONTROL_HOB: u8 = 0x80;

const DEVICE_LBA: u8 = 0x40;

/// ATAPI interrupt reason, in the sector count register.
const REASON_COMMAND: u8 = 0x01;
const REASON_TO_HOST: u8 = 0x02;

/// The longest READ/WRITE MULTIPLE block, in sectors.
const MAX_MULTIPLE: u8 = 16;
/// An ATAPI data chunk when the host named no limit.
const ATAPI_DEFAULT_LIMIT: usize = 0xfffe;
const ATAPI_BLOCK: usize = ahci::BLOCK_SIZE as usize;

/// What sits on one end of a cable.
#[derive(Debug)]
pub enum Drive {
    Disk(Disk),
    Atapi(Atapi),
}

/// What happens when the guest has read the whole buffer.
#[derive(Debug)]
enum AfterIn {
    /// The command is over.
    Nothing,
    /// More sectors of a disk read: the next block is `block` sectors.
    DiskRead {
        lba: u64,
        remaining: u64,
        block: u64,
    },
    /// More bytes of a packet command's reply.
    AtapiBytes { data: Vec<u8>, at: usize },
    /// More of a packet command's read from the image.
    AtapiImage { offset: u64, remaining: u64 },
}

/// What happens when the guest has filled the whole buffer.
#[derive(Debug)]
enum AfterOut {
    /// The buffer is `block` sectors of a disk write.
    DiskWrite {
        lba: u64,
        remaining: u64,
        block: u64,
    },
    /// The buffer is a packet command.
    Packet,
}

#[derive(Debug)]
enum Transfer {
    Idle,
    In(AfterIn),
    Out(AfterOut),
}

/// One channel: two drives and the registers they share.
pub struct Channel {
    drives: [Option<Drive>; 2],
    irq: Option<Arc<dyn IrqLine>>,
    selected: usize,
    features: u8,
    error: u8,
    count: u8,
    lba_low: u8,
    lba_mid: u8,
    lba_high: u8,
    device: u8,
    status: u8,
    /// The values `count` and the LBA registers held before their last
    /// write: the high bytes of a 48-bit command.
    previous: [u8; 4],
    control: u8,
    /// READ/WRITE MULTIPLE block size per drive; 0 when not set.
    multiple: [u8; 2],
    /// The ATAPI byte count limit of the packet command in flight.
    atapi_limit: usize,
    buffer: Vec<u8>,
    position: usize,
    transfer: Transfer,
}

impl Channel {
    fn new(drives: [Option<Drive>; 2]) -> Self {
        let mut channel = Self {
            drives,
            irq: None,
            selected: 0,
            features: 0,
            error: 0,
            count: 0,
            lba_low: 0,
            lba_mid: 0,
            lba_high: 0,
            device: 0,
            status: 0,
            previous: [0; 4],
            control: 0,
            multiple: [0; 2],
            atapi_limit: ATAPI_DEFAULT_LIMIT,
            buffer: Vec::new(),
            position: 0,
            transfer: Transfer::Idle,
        };
        channel.reset();
        channel
    }

    /// Power-on, and what SRST does.
    fn reset(&mut self) {
        self.selected = 0;
        self.device = 0;
        self.features = 0;
        self.previous = [0; 4];
        self.multiple = [0; 2];
        self.buffer.clear();
        self.position = 0;
        self.transfer = Transfer::Idle;
        self.signature();
    }

    fn has_drives(&self) -> bool {
        self.drives.iter().any(Option::is_some)
    }

    fn drive(&mut self) -> Option<&mut Drive> {
        self.drives.get_mut(self.selected).and_then(Option::as_mut)
    }

    fn is_atapi(&self) -> bool {
        matches!(self.drives.get(self.selected), Some(Some(Drive::Atapi(_))))
    }

    /// The reset signature of the selected drive: what tells a host an
    /// ATAPI drive from a disk before it has identified either.
    fn signature(&mut self) {
        self.count = 1;
        self.lba_low = 1;
        self.error = 1;
        (self.lba_mid, self.lba_high) = if self.is_atapi() {
            (0x14, 0xeb)
        } else {
            (0, 0)
        };
        self.status = STATUS_READY;
    }

    fn raise(&self) {
        if self.control & CONTROL_NIEN != 0 {
            return;
        }
        if let Some(irq) = &self.irq {
            if let Err(error) = irq.trigger() {
                tracing::warn!(%error, "cannot raise an IDE interrupt");
            }
        }
    }

    fn done(&mut self) {
        self.transfer = Transfer::Idle;
        self.status = STATUS_READY;
        self.raise();
    }

    fn fail(&mut self, error: u8) {
        self.transfer = Transfer::Idle;
        self.error = error;
        self.status = STATUS_READY | STATUS_ERR;
        self.raise();
    }

    fn abort(&mut self) {
        self.fail(ERROR_ABRT);
    }

    /// Offers `bytes` to the guest through the data port.
    fn data_in(&mut self, bytes: Vec<u8>, after: AfterIn) {
        self.buffer = bytes;
        self.position = 0;
        self.transfer = Transfer::In(after);
        self.status = STATUS_READY | STATUS_DRQ;
        self.raise();
    }

    /// Asks the guest for `len` bytes through the data port.
    fn data_out(&mut self, len: usize, after: AfterOut) {
        self.buffer = vec![0; len];
        self.position = 0;
        self.transfer = Transfer::Out(after);
        self.status = STATUS_READY | STATUS_DRQ;
    }

    // ---- registers ---------------------------------------------------------

    fn read_register(&mut self, offset: u16) -> u8 {
        if !self.has_drives() {
            return 0;
        }
        let high_bytes = self.control & CONTROL_HOB != 0;
        match offset {
            1 => self.error,
            2 if high_bytes => self.previous[0],
            3 if high_bytes => self.previous[1],
            4 if high_bytes => self.previous[2],
            5 if high_bytes => self.previous[3],
            2 => self.count,
            3 => self.lba_low,
            4 => self.lba_mid,
            5 => self.lba_high,
            6 => self.device,
            // A drive that is not there drives nothing.
            7 if self.drives[self.selected].is_none() => 0,
            7 => self.status,
            _ => 0xff,
        }
    }

    fn write_register(&mut self, offset: u16, value: u8) {
        // Any write to the command block ends "read the high bytes".
        self.control &= !CONTROL_HOB;
        match offset {
            1 => self.features = value,
            2 => {
                self.previous[0] = self.count;
                self.count = value;
            }
            3 => {
                self.previous[1] = self.lba_low;
                self.lba_low = value;
            }
            4 => {
                self.previous[2] = self.lba_mid;
                self.lba_mid = value;
            }
            5 => {
                self.previous[3] = self.lba_high;
                self.lba_high = value;
            }
            6 => {
                self.device = value;
                self.selected = usize::from(value >> 4 & 1);
            }
            7 => self.command(value),
            _ => {}
        }
    }

    fn write_control(&mut self, value: u8) {
        let was_reset = self.control & CONTROL_SRST != 0;
        self.control = value;
        if value & CONTROL_SRST != 0 && !was_reset {
            self.reset();
        }
    }

    fn read_data(&mut self, data: &mut [u8]) {
        data.fill(0);
        if !matches!(self.transfer, Transfer::In(_)) {
            return;
        }
        for byte in data.iter_mut() {
            let Some(&value) = self.buffer.get(self.position) else {
                break;
            };
            *byte = value;
            self.position += 1;
        }
        if self.position >= self.buffer.len() {
            self.after_in();
        }
    }

    fn write_data(&mut self, data: &[u8]) {
        if !matches!(self.transfer, Transfer::Out(_)) {
            return;
        }
        for &byte in data {
            let Some(slot) = self.buffer.get_mut(self.position) else {
                break;
            };
            *slot = byte;
            self.position += 1;
        }
        if self.position >= self.buffer.len() {
            self.after_out();
        }
    }

    // ---- data phases -------------------------------------------------------

    fn after_in(&mut self) {
        match std::mem::replace(&mut self.transfer, Transfer::Idle) {
            Transfer::In(AfterIn::DiskRead {
                lba,
                remaining,
                block,
            }) if remaining > 0 => self.disk_read(lba, remaining, block),
            Transfer::In(AfterIn::AtapiBytes { data, at }) if at < data.len() => {
                self.atapi_bytes(data, at);
            }
            Transfer::In(AfterIn::AtapiImage { offset, remaining }) if remaining > 0 => {
                self.atapi_image(offset, remaining);
            }
            Transfer::In(AfterIn::AtapiBytes { .. } | AfterIn::AtapiImage { .. }) => {
                self.packet_done();
            }
            // The last block of a PIO read ends without an interrupt.
            _ => self.status = STATUS_READY,
        }
    }

    fn after_out(&mut self) {
        match std::mem::replace(&mut self.transfer, Transfer::Idle) {
            Transfer::Out(AfterOut::DiskWrite {
                lba,
                remaining,
                block,
            }) => {
                let buffer = std::mem::take(&mut self.buffer);
                let written = match self.drive() {
                    Some(Drive::Disk(disk)) => disk.write(lba, &buffer).is_ok(),
                    _ => false,
                };
                if !written {
                    return self.fail(ERROR_IDNF | ERROR_ABRT);
                }
                let remaining = remaining - block;
                if remaining == 0 {
                    return self.done();
                }
                let next = block.min(remaining);
                self.data_out(
                    next as usize * SECTOR,
                    AfterOut::DiskWrite {
                        lba: lba + block,
                        remaining,
                        block: next,
                    },
                );
                self.raise();
            }
            Transfer::Out(AfterOut::Packet) => self.packet(),
            _ => self.status = STATUS_READY,
        }
    }

    /// Loads the next block of a disk read and offers it.
    fn disk_read(&mut self, lba: u64, remaining: u64, block: u64) {
        let sectors = block.min(remaining);
        let mut bytes = vec![0u8; sectors as usize * SECTOR];
        let read = match self.drive() {
            Some(Drive::Disk(disk)) => disk.read(lba, &mut bytes).is_ok(),
            _ => false,
        };
        if !read {
            return self.fail(ERROR_IDNF | ERROR_ABRT);
        }
        self.data_in(
            bytes,
            AfterIn::DiskRead {
                lba: lba + sectors,
                remaining: remaining - sectors,
                block,
            },
        );
    }

    // ---- ATA commands ------------------------------------------------------

    /// The 28-bit address in the registers, by LBA or by CHS.
    fn address28(&self, chs: (u16, u16, u16)) -> Option<u64> {
        if self.device & DEVICE_LBA != 0 {
            return Some(
                u64::from(self.device & 0x0f) << 24
                    | u64::from(self.lba_high) << 16
                    | u64::from(self.lba_mid) << 8
                    | u64::from(self.lba_low),
            );
        }
        let (_, heads, sectors) = chs;
        let cylinder = u64::from(self.lba_high) << 8 | u64::from(self.lba_mid);
        let head = u64::from(self.device & 0x0f);
        let sector = u64::from(self.lba_low);
        if sector == 0 || sector > u64::from(sectors) || head >= u64::from(heads) {
            return None;
        }
        Some((cylinder * u64::from(heads) + head) * u64::from(sectors) + sector - 1)
    }

    fn address48(&self) -> u64 {
        u64::from(self.previous[3]) << 40
            | u64::from(self.previous[2]) << 32
            | u64::from(self.previous[1]) << 24
            | u64::from(self.lba_high) << 16
            | u64::from(self.lba_mid) << 8
            | u64::from(self.lba_low)
    }

    /// The sector count of a 28-bit command (0 means 256) or a 48-bit one
    /// (0 means 65536).
    fn sector_count(&self, wide: bool) -> u64 {
        if wide {
            match u64::from(self.previous[0]) << 8 | u64::from(self.count) {
                0 => 65536,
                n => n,
            }
        } else {
            match u64::from(self.count) {
                0 => 256,
                n => n,
            }
        }
    }

    fn command(&mut self, command: u8) {
        // EXECUTE DEVICE DIAGNOSTIC is for both drives, present or not.
        if command == 0x90 {
            if self.has_drives() {
                self.selected = 0;
                self.device &= !0x10;
                self.signature();
                self.raise();
            }
            return;
        }
        if self.drives[self.selected].is_none() {
            return;
        }
        self.transfer = Transfer::Idle;
        self.error = 0;
        tracing::trace!(
            command = format_args!("{command:#04x}"),
            drive = self.selected,
            "IDE command"
        );
        if self.is_atapi() {
            self.atapi_command(command);
        } else {
            self.disk_command(command);
        }
    }

    fn disk_command(&mut self, command: u8) {
        let multiple = self.multiple[self.selected];
        let (sectors, chs) = match self.drive() {
            Some(Drive::Disk(disk)) => (disk.sectors(), disk.chs()),
            _ => return self.abort(),
        };
        // (wide address, block size in sectors) of the read and write
        // commands; a MULTIPLE command with no block size set is an error.
        let shape = match command {
            0x20 | 0x21 | 0x30 | 0x31 => Some((false, 1)),
            0x24 | 0x34 => Some((true, 1)),
            0xc4 | 0xc5 if multiple != 0 => Some((false, u64::from(multiple))),
            0x29 | 0x39 if multiple != 0 => Some((true, u64::from(multiple))),
            _ => None,
        };
        match command {
            0x20 | 0x21 | 0x24 | 0xc4 | 0x29 | 0x30 | 0x31 | 0x34 | 0xc5 | 0x39 | 0x40 | 0x41
            | 0x42 => {
                let (wide, block) = match (shape, command) {
                    (Some(shape), _) => shape,
                    (None, 0x40 | 0x41) => (false, 1),
                    (None, 0x42) => (true, 1),
                    (None, _) => return self.abort(),
                };
                let lba = if wide {
                    Some(self.address48())
                } else {
                    self.address28(chs)
                };
                let count = self.sector_count(wide);
                let Some(lba) = lba.filter(|lba| lba.saturating_add(count) <= sectors) else {
                    return self.fail(ERROR_IDNF | ERROR_ABRT);
                };
                match command {
                    0x40..=0x42 => self.done(),
                    0x30 | 0x31 | 0x34 | 0xc5 | 0x39 => {
                        let first = block.min(count);
                        self.data_out(
                            first as usize * SECTOR,
                            AfterOut::DiskWrite {
                                lba,
                                remaining: count,
                                block: first,
                            },
                        );
                    }
                    _ => self.disk_read(lba, count, block),
                }
            }
            0xec => {
                let identify = match self.drive() {
                    Some(Drive::Disk(disk)) => disk.identify(multiple),
                    _ => return self.abort(),
                };
                self.data_in(identify, AfterIn::Nothing);
            }
            0xc6 => {
                // SET MULTIPLE MODE: a power of two up to the maximum, or 0.
                let size = self.count;
                if size != 0 && (size > MAX_MULTIPLE || !size.is_power_of_two()) {
                    return self.abort();
                }
                self.multiple[self.selected] = size;
                self.done();
            }
            // READ NATIVE MAX ADDRESS, 28-bit and 48-bit.
            0xf8 => {
                let last = sectors.saturating_sub(1).min(0x0fff_ffff);
                self.lba_low = last as u8;
                self.lba_mid = (last >> 8) as u8;
                self.lba_high = (last >> 16) as u8;
                self.device = (self.device & 0xf0) | (last >> 24) as u8;
                self.done();
            }
            0x27 => {
                let last = sectors.saturating_sub(1);
                self.previous[1] = (last >> 24) as u8;
                self.previous[2] = (last >> 32) as u8;
                self.previous[3] = (last >> 40) as u8;
                self.lba_low = last as u8;
                self.lba_mid = (last >> 8) as u8;
                self.lba_high = (last >> 16) as u8;
                self.done();
            }
            0xe7 | 0xea => {
                let flushed = match self.drive() {
                    Some(Drive::Disk(disk)) => disk.flush().is_ok(),
                    _ => false,
                };
                if flushed {
                    self.done();
                } else {
                    self.fail(ERROR_UNC);
                }
            }
            // CHECK POWER MODE: active.
            0xe5 => {
                self.count = 0xff;
                self.done();
            }
            // RECALIBRATE, SEEK, INITIALIZE DEVICE PARAMETERS, the standby
            // and idle commands, SET FEATURES: nothing to do for a file.
            0x10..=0x1f | 0x70..=0x7f | 0x91 | 0xe0..=0xe3 | 0xe6 | 0xef => self.done(),
            _ => self.abort(),
        }
    }

    // ---- ATAPI -------------------------------------------------------------

    fn atapi_command(&mut self, command: u8) {
        match command {
            0xa1 => {
                let mut identify = match self.drive() {
                    Some(Drive::Atapi(drive)) => drive.identify_packet(),
                    _ => return self.abort(),
                };
                // The drive's own identify data offers DMA, which is true of
                // it behind AHCI and not here: no DMA (word 49), no
                // multiword modes (63), no Ultra DMA (88).
                for (word, mask) in [(49usize, 0xfeffu16), (63, 0), (88, 0)] {
                    if let Some(bytes) = identify.get_mut(word * 2..word * 2 + 2) {
                        let value = u16::from_le_bytes([bytes[0], bytes[1]]) & mask;
                        bytes.copy_from_slice(&value.to_le_bytes());
                    }
                }
                self.data_in(identify, AfterIn::Nothing);
            }
            0xa0 => {
                // Bit 0 of the features register asks for DMA.
                if self.features & 1 != 0 {
                    return self.abort();
                }
                let limit = usize::from(self.lba_high) << 8 | usize::from(self.lba_mid);
                self.atapi_limit = match limit {
                    0 | 0xffff => ATAPI_DEFAULT_LIMIT,
                    n => n & !1,
                }
                .max(2);
                self.count = REASON_COMMAND;
                self.data_out(12, AfterOut::Packet);
            }
            // DEVICE RESET.
            0x08 => self.signature(),
            // IDENTIFY DEVICE is refused, leaving the signature that says
            // which identify command to use instead.
            0xec => {
                self.signature();
                self.abort();
            }
            0xe5 => {
                self.count = 0xff;
                self.done();
            }
            0xe0..=0xe3 | 0xe6 | 0xe7 | 0xef => self.done(),
            _ => self.abort(),
        }
    }

    /// Runs the packet command the guest just wrote.
    fn packet(&mut self) {
        let mut cdb = [0u8; 16];
        for (slot, byte) in cdb.iter_mut().zip(&self.buffer) {
            *slot = *byte;
        }
        let reply = match self.drive() {
            Some(Drive::Atapi(drive)) => drive.packet(&cdb),
            _ => return self.abort(),
        };
        match reply {
            Reply::Done => self.packet_done(),
            Reply::Data(data) if data.is_empty() => self.packet_done(),
            Reply::Data(data) => self.atapi_bytes(data, 0),
            Reply::Read { blocks: 0, .. } => self.packet_done(),
            Reply::Read { lba, blocks } => self.atapi_image(
                lba.saturating_mul(ahci::BLOCK_SIZE),
                blocks.saturating_mul(ahci::BLOCK_SIZE),
            ),
            Reply::Check(sense) => {
                self.count = REASON_COMMAND | REASON_TO_HOST;
                self.fail(sense.key << 4);
            }
        }
    }

    fn packet_done(&mut self) {
        self.count = REASON_COMMAND | REASON_TO_HOST;
        self.done();
    }

    /// Offers one chunk of a packet reply, with its length in the byte count
    /// registers.
    fn atapi_chunk(&mut self, bytes: Vec<u8>, after: AfterIn) {
        self.lba_mid = bytes.len() as u8;
        self.lba_high = (bytes.len() >> 8) as u8;
        self.count = REASON_TO_HOST;
        self.data_in(bytes, after);
    }

    fn atapi_bytes(&mut self, data: Vec<u8>, at: usize) {
        let end = data.len().min(at + self.atapi_limit);
        let chunk = data.get(at..end).unwrap_or_default().to_vec();
        self.atapi_chunk(chunk, AfterIn::AtapiBytes { data, at: end });
    }

    fn atapi_image(&mut self, offset: u64, remaining: u64) {
        // Whole blocks per chunk when the limit allows a block at all.
        let mut len = (self.atapi_limit as u64).min(remaining) as usize;
        if len > ATAPI_BLOCK {
            len -= len % ATAPI_BLOCK;
        }
        let mut bytes = vec![0u8; len];
        let read = match self.drive() {
            Some(Drive::Atapi(drive)) => {
                let read = drive.read_at(&mut bytes, offset).is_ok();
                if !read {
                    drive.set_sense(ahci::Sense::READ_ERROR);
                }
                read
            }
            _ => false,
        };
        if !read {
            self.count = REASON_COMMAND | REASON_TO_HOST;
            return self.fail(ahci::Sense::READ_ERROR.key << 4);
        }
        self.atapi_chunk(
            bytes,
            AfterIn::AtapiImage {
                offset: offset + len as u64,
                remaining: remaining - len as u64,
            },
        );
    }
}

/// The controller: the primary and the secondary channel.
pub struct Controller {
    channels: [Channel; 2],
}

impl Controller {
    /// A controller with these drives as master and slave of each channel.
    pub fn new(primary: [Option<Drive>; 2], secondary: [Option<Drive>; 2]) -> Self {
        Self {
            channels: [Channel::new(primary), Channel::new(secondary)],
        }
    }

    /// Wires the two interrupts: IRQ 14 and IRQ 15.
    pub fn connect(&mut self, primary: Arc<dyn IrqLine>, secondary: Arc<dyn IrqLine>) {
        self.channels[0].irq = Some(primary);
        self.channels[1].irq = Some(secondary);
    }

    /// Whether `port` is one of the controller's.
    pub fn contains(port: u16) -> bool {
        Self::decode(port).is_some()
    }

    /// (channel, register): 0–7 in the command block, 8 for the control
    /// port.
    fn decode(port: u16) -> Option<(usize, u16)> {
        match port {
            PRIMARY_BASE..=0x1f7 => Some((0, port - PRIMARY_BASE)),
            SECONDARY_BASE..=0x177 => Some((1, port - SECONDARY_BASE)),
            PRIMARY_CONTROL => Some((0, 8)),
            SECONDARY_CONTROL => Some((1, 8)),
            _ => None,
        }
    }

    /// A guest port read.
    pub fn io_read(&mut self, port: u16, data: &mut [u8]) {
        data.fill(0xff);
        let Some((channel, register)) = Self::decode(port) else {
            return;
        };
        let channel = &mut self.channels[channel];
        match register {
            0 => channel.read_data(data),
            // The alternate status: the status, without acknowledging.
            8 => {
                if let Some(byte) = data.first_mut() {
                    *byte = channel.read_register(7);
                }
            }
            _ => {
                if let Some(byte) = data.first_mut() {
                    *byte = channel.read_register(register);
                }
            }
        }
    }

    /// A guest port write.
    pub fn io_write(&mut self, port: u16, data: &[u8]) {
        let Some((channel, register)) = Self::decode(port) else {
            return;
        };
        let channel = &mut self.channels[channel];
        match (register, data.first()) {
            (0, _) => channel.write_data(data),
            (8, Some(&value)) => channel.write_control(value),
            (_, Some(&value)) => channel.write_register(register, value),
            _ => {}
        }
    }

    /// Power-on (ADR-0005).
    pub fn reset(&mut self) {
        for channel in &mut self.channels {
            channel.control = 0;
            channel.reset();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU32, Ordering};
    use virtio_core::interrupt::InterruptError;

    #[derive(Default)]
    struct Counter(AtomicU32);
    impl IrqLine for Counter {
        fn trigger(&self) -> Result<(), InterruptError> {
            self.0.fetch_add(1, Ordering::Relaxed);
            Ok(())
        }
    }

    fn temp_image(bytes: &[u8]) -> PathBuf {
        static NEXT: AtomicU32 = AtomicU32::new(0);
        let path = std::env::temp_dir().join(format!(
            "ide-test-{}-{}.img",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::write(&path, bytes).expect("write the test image");
        path
    }

    struct Machine {
        ide: Controller,
        irq14: Arc<Counter>,
        irq15: Arc<Counter>,
        files: Vec<PathBuf>,
    }

    impl Drop for Machine {
        fn drop(&mut self) {
            for file in &self.files {
                let _ = std::fs::remove_file(file);
            }
        }
    }

    /// A 64-sector disk whose sector `n` starts with the byte `n`, as the
    /// primary master, and a 16-block disc whose block `n` starts with
    /// `0x80 + n`, as the secondary master.
    fn machine() -> Machine {
        let mut disk = vec![0u8; 64 * SECTOR];
        for n in 0..64 {
            disk[n * SECTOR] = n as u8;
        }
        let mut disc = vec![0u8; 16 * ATAPI_BLOCK];
        for n in 0..16 {
            disc[n * ATAPI_BLOCK] = 0x80 + n as u8;
        }
        let files = vec![temp_image(&disk), temp_image(&disc)];
        let mut ide = Controller::new(
            [
                Some(Drive::Disk(
                    Disk::open(&files[0], true, "TEST-DISK").expect("disk"),
                )),
                None,
            ],
            [
                Some(Drive::Atapi(
                    Atapi::open(&files[1], "TEST-DISC").expect("disc"),
                )),
                None,
            ],
        );
        let (irq14, irq15) = (Arc::new(Counter::default()), Arc::new(Counter::default()));
        ide.connect(irq14.clone(), irq15.clone());
        Machine {
            ide,
            irq14,
            irq15,
            files,
        }
    }

    fn inb(ide: &mut Controller, port: u16) -> u8 {
        let mut byte = [0u8];
        ide.io_read(port, &mut byte);
        byte[0]
    }

    fn outb(ide: &mut Controller, port: u16, value: u8) {
        ide.io_write(port, &[value]);
    }

    fn read_words(ide: &mut Controller, base: u16, bytes: usize) -> Vec<u8> {
        let mut out = vec![0u8; bytes];
        for word in out.chunks_mut(2) {
            ide.io_read(base, word);
        }
        out
    }

    #[test]
    fn the_signatures_tell_a_disk_from_an_atapi_drive() {
        let mut m = machine();
        assert_eq!(inb(&mut m.ide, 0x1f4), 0);
        assert_eq!(inb(&mut m.ide, 0x1f5), 0);
        assert_eq!(inb(&mut m.ide, 0x174), 0x14);
        assert_eq!(inb(&mut m.ide, 0x175), 0xeb);
        assert_eq!(inb(&mut m.ide, 0x1f7), STATUS_READY);
        // The slave that is not there reads as nothing.
        outb(&mut m.ide, 0x1f6, 0xb0);
        assert_eq!(inb(&mut m.ide, 0x1f7), 0);
    }

    #[test]
    fn identify_describes_the_disk_and_offers_no_dma() {
        let mut m = machine();
        outb(&mut m.ide, 0x1f6, 0xa0);
        outb(&mut m.ide, 0x1f7, 0xec);
        assert_eq!(inb(&mut m.ide, 0x1f7), STATUS_READY | STATUS_DRQ);
        assert_eq!(m.irq14.0.load(Ordering::Relaxed), 1);
        let id = read_words(&mut m.ide, 0x1f0, 512);
        let word = |n: usize| u16::from_le_bytes([id[n * 2], id[n * 2 + 1]]);
        assert_eq!(word(60), 64, "LBA28 sectors");
        assert_eq!(word(49) & 0x0100, 0, "no DMA");
        assert_eq!(word(49) & 0x0200, 0x0200, "LBA");
        assert_eq!(inb(&mut m.ide, 0x1f7), STATUS_READY);
    }

    #[test]
    fn sectors_are_read_one_interrupt_per_block() {
        let mut m = machine();
        // Two sectors from LBA 5.
        outb(&mut m.ide, 0x1f2, 2);
        outb(&mut m.ide, 0x1f3, 5);
        outb(&mut m.ide, 0x1f4, 0);
        outb(&mut m.ide, 0x1f5, 0);
        outb(&mut m.ide, 0x1f6, 0xe0);
        outb(&mut m.ide, 0x1f7, 0x20);
        let first = read_words(&mut m.ide, 0x1f0, SECTOR);
        assert_eq!(first[0], 5);
        assert_eq!(inb(&mut m.ide, 0x1f7), STATUS_READY | STATUS_DRQ);
        let second = read_words(&mut m.ide, 0x1f0, SECTOR);
        assert_eq!(second[0], 6);
        assert_eq!(inb(&mut m.ide, 0x1f7), STATUS_READY);
        assert_eq!(m.irq14.0.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn a_write_reaches_the_disk_and_a_chs_read_finds_it() {
        let mut m = machine();
        outb(&mut m.ide, 0x1f2, 1);
        outb(&mut m.ide, 0x1f3, 63);
        outb(&mut m.ide, 0x1f4, 0);
        outb(&mut m.ide, 0x1f5, 0);
        outb(&mut m.ide, 0x1f6, 0xe0);
        outb(&mut m.ide, 0x1f7, 0x30);
        assert_eq!(inb(&mut m.ide, 0x1f7), STATUS_READY | STATUS_DRQ);
        assert_eq!(m.irq14.0.load(Ordering::Relaxed), 0, "no interrupt yet");
        for _ in 0..SECTOR / 2 {
            m.ide.io_write(0x1f0, &[0xab, 0xcd]);
        }
        assert_eq!(inb(&mut m.ide, 0x1f7), STATUS_READY);
        assert_eq!(m.irq14.0.load(Ordering::Relaxed), 1);
        // LBA 63 is cylinder 0, head 1, sector 1.
        outb(&mut m.ide, 0x1f2, 1);
        outb(&mut m.ide, 0x1f3, 1);
        outb(&mut m.ide, 0x1f4, 0);
        outb(&mut m.ide, 0x1f5, 0);
        outb(&mut m.ide, 0x1f6, 0xa1);
        outb(&mut m.ide, 0x1f7, 0x20);
        let sector = read_words(&mut m.ide, 0x1f0, SECTOR);
        assert_eq!(&sector[..2], &[0xab, 0xcd]);
    }

    #[test]
    fn a_read_past_the_end_and_an_unknown_command_fail() {
        let mut m = machine();
        outb(&mut m.ide, 0x1f2, 2);
        outb(&mut m.ide, 0x1f3, 63);
        outb(&mut m.ide, 0x1f4, 0);
        outb(&mut m.ide, 0x1f5, 0);
        outb(&mut m.ide, 0x1f6, 0xe0);
        outb(&mut m.ide, 0x1f7, 0x20);
        assert_eq!(inb(&mut m.ide, 0x1f7), STATUS_READY | STATUS_ERR);
        assert_eq!(inb(&mut m.ide, 0x1f1) & ERROR_IDNF, ERROR_IDNF);
        outb(&mut m.ide, 0x1f7, 0xc8); // READ DMA
        assert_eq!(inb(&mut m.ide, 0x1f1), ERROR_ABRT);
        // READ MULTIPLE before SET MULTIPLE MODE.
        outb(&mut m.ide, 0x1f7, 0xc4);
        assert_eq!(inb(&mut m.ide, 0x1f1), ERROR_ABRT);
        // Data ports with nothing to transfer do nothing.
        m.ide.io_write(0x1f0, &[1, 2]);
        assert_eq!(read_words(&mut m.ide, 0x1f0, 2), vec![0, 0]);
    }

    #[test]
    fn a_packet_read_comes_back_in_chunks_of_the_byte_count() {
        let mut m = machine();
        // IDENTIFY DEVICE is refused with the ATAPI signature.
        outb(&mut m.ide, 0x177, 0xec);
        assert_eq!(inb(&mut m.ide, 0x177) & STATUS_ERR, STATUS_ERR);
        assert_eq!(inb(&mut m.ide, 0x175), 0xeb);
        // PACKET, at most 2048 bytes a chunk.
        outb(&mut m.ide, 0x171, 0);
        outb(&mut m.ide, 0x174, 0x00);
        outb(&mut m.ide, 0x175, 0x08);
        outb(&mut m.ide, 0x177, 0xa0);
        assert_eq!(inb(&mut m.ide, 0x177), STATUS_READY | STATUS_DRQ);
        assert_eq!(inb(&mut m.ide, 0x172), REASON_COMMAND);
        // READ(10), 2 blocks from LBA 3.
        let cdb = [0x28, 0, 0, 0, 0, 3, 0, 0, 2, 0, 0, 0];
        for pair in cdb.chunks(2) {
            m.ide.io_write(0x170, pair);
        }
        for block in 3..5u8 {
            assert_eq!(inb(&mut m.ide, 0x177), STATUS_READY | STATUS_DRQ);
            assert_eq!(inb(&mut m.ide, 0x172), REASON_TO_HOST);
            let len =
                usize::from(inb(&mut m.ide, 0x175)) << 8 | usize::from(inb(&mut m.ide, 0x174));
            assert_eq!(len, ATAPI_BLOCK);
            let data = read_words(&mut m.ide, 0x170, len);
            assert_eq!(data[0], 0x80 + block);
        }
        assert_eq!(inb(&mut m.ide, 0x177), STATUS_READY);
        assert_eq!(inb(&mut m.ide, 0x172), REASON_COMMAND | REASON_TO_HOST);
        // One interrupt per chunk and one at the end.
        assert_eq!(m.irq15.0.load(Ordering::Relaxed), 3 + 1);
    }

    #[test]
    fn a_failed_packet_is_a_check_condition_and_dma_is_refused() {
        let mut m = machine();
        outb(&mut m.ide, 0x171, 0);
        outb(&mut m.ide, 0x177, 0xa0);
        // READ(10) far past the disc.
        let cdb = [0x28, 0, 0, 0, 0xff, 0xff, 0, 0, 1, 0, 0, 0];
        for pair in cdb.chunks(2) {
            m.ide.io_write(0x170, pair);
        }
        assert_eq!(inb(&mut m.ide, 0x177) & STATUS_ERR, STATUS_ERR);
        assert_ne!(inb(&mut m.ide, 0x171) >> 4, 0, "a sense key");
        // A packet that asks for DMA.
        outb(&mut m.ide, 0x171, 1);
        outb(&mut m.ide, 0x177, 0xa0);
        assert_eq!(inb(&mut m.ide, 0x171), ERROR_ABRT);
    }

    #[test]
    fn a_soft_reset_ends_a_transfer_and_nien_silences_the_interrupt() {
        let mut m = machine();
        outb(&mut m.ide, 0x3f6, CONTROL_NIEN);
        outb(&mut m.ide, 0x1f6, 0xa0);
        outb(&mut m.ide, 0x1f7, 0xec);
        assert_eq!(m.irq14.0.load(Ordering::Relaxed), 0);
        assert_eq!(inb(&mut m.ide, 0x3f6), STATUS_READY | STATUS_DRQ);
        outb(&mut m.ide, 0x3f6, CONTROL_SRST);
        outb(&mut m.ide, 0x3f6, 0);
        assert_eq!(inb(&mut m.ide, 0x1f7), STATUS_READY);
        assert_eq!(inb(&mut m.ide, 0x1f2), 1);
        // Ports that are not the controller's are not answered.
        assert!(!Controller::contains(0x1f8));
        assert!(Controller::contains(0x376));
    }
}
