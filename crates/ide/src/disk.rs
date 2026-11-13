//! An ATA disk over a RAW image file.

use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::Path;

/// Bytes in a sector.
pub const SECTOR: usize = 512;

/// One disk.
#[derive(Debug)]
pub struct Disk {
    file: File,
    sectors: u64,
    writable: bool,
    serial: String,
}

/// ATA strings are space-padded and byte-swapped within each word.
fn put_ata_string(words: &mut [u16], text: &str) {
    let mut bytes = vec![b' '; words.len() * 2];
    for (slot, byte) in bytes.iter_mut().zip(text.bytes()) {
        *slot = byte;
    }
    for (word, pair) in words.iter_mut().zip(bytes.chunks_exact(2)) {
        *word = (u16::from(pair[0]) << 8) | u16::from(pair[1]);
    }
}

impl Disk {
    /// Opens `path`; a disk not opened writable refuses writes.
    pub fn open(path: &Path, writable: bool, serial: &str) -> io::Result<Self> {
        let file = File::options().read(true).write(writable).open(path)?;
        let sectors = file.metadata()?.len() / SECTOR as u64;
        Ok(Self {
            file,
            sectors,
            writable,
            serial: serial.to_string(),
        })
    }

    /// Size in sectors.
    pub fn sectors(&self) -> u64 {
        self.sectors
    }

    /// The geometry IDENTIFY reports: 16 heads of 63 sectors, and as many
    /// cylinders as fit in the field.
    pub fn chs(&self) -> (u16, u16, u16) {
        let cylinders = (self.sectors / (16 * 63)).min(16383) as u16;
        (cylinders, 16, 63)
    }

    fn offset(&self, lba: u64, len: usize) -> io::Result<u64> {
        let end = lba
            .checked_mul(SECTOR as u64)
            .and_then(|at| at.checked_add(len as u64));
        match end {
            Some(end) if end <= self.sectors * SECTOR as u64 => Ok(lba * SECTOR as u64),
            _ => Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "past the end of the disk",
            )),
        }
    }

    /// Reads whole sectors at `lba`.
    pub fn read(&mut self, lba: u64, buf: &mut [u8]) -> io::Result<()> {
        let at = self.offset(lba, buf.len())?;
        self.file.seek(SeekFrom::Start(at))?;
        self.file.read_exact(buf)
    }

    /// Writes whole sectors at `lba`.
    pub fn write(&mut self, lba: u64, buf: &[u8]) -> io::Result<()> {
        if !self.writable {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "the disk is read-only",
            ));
        }
        let at = self.offset(lba, buf.len())?;
        self.file.seek(SeekFrom::Start(at))?;
        self.file.write_all(buf)
    }

    /// Asks the host to make what was written durable.
    pub fn flush(&mut self) -> io::Result<()> {
        if self.writable {
            self.file.sync_data()
        } else {
            Ok(())
        }
    }

    /// The 512-byte IDENTIFY DEVICE reply. `multiple` is the current
    /// READ/WRITE MULTIPLE block size, 0 when none is set.
    pub fn identify(&self, multiple: u8) -> Vec<u8> {
        let mut words = [0u16; 256];
        let (cylinders, heads, spt) = self.chs();
        // A fixed, non-removable ATA device.
        words[0] = 0x0040;
        words[1] = cylinders;
        words[3] = heads;
        words[6] = spt;
        put_ata_string(&mut words[10..20], &self.serial);
        put_ata_string(&mut words[23..27], "1.0");
        put_ata_string(&mut words[27..47], "Entangled Virtual Disk");
        // READ/WRITE MULTIPLE of up to 16 sectors.
        words[47] = 0x8010;
        // LBA, and no DMA: this controller has no bus master.
        words[49] = 0x0200;
        // Words 54-58 and 64-70 are valid.
        words[53] = 0x0003;
        words[54] = cylinders;
        words[55] = heads;
        words[56] = spt;
        let chs_capacity = u32::from(cylinders) * u32::from(heads) * u32::from(spt);
        words[57] = chs_capacity as u16;
        words[58] = (chs_capacity >> 16) as u16;
        if multiple != 0 {
            words[59] = 0x0100 | u16::from(multiple);
        }
        let lba28 = self.sectors.min(0x0fff_ffff) as u32;
        words[60] = lba28 as u16;
        words[61] = (lba28 >> 16) as u16;
        // PIO modes 3 and 4.
        words[64] = 0x0003;
        words[65..69].fill(120);
        // ATA-2 through ATA/ATAPI-6.
        words[80] = 0x007c;
        // Command sets: NOP, the 48-bit address feature set, FLUSH CACHE and
        // FLUSH CACHE EXT; supported and enabled.
        words[82] = 0x4000;
        words[83] = 0x7400;
        words[84] = 0x4000;
        words[85] = 0x4000;
        words[86] = 0x3400;
        words[87] = 0x4000;
        for (i, word) in words[100..104].iter_mut().enumerate() {
            *word = (self.sectors >> (16 * i)) as u16;
        }
        words.iter().flat_map(|w| w.to_le_bytes()).collect()
    }
}
