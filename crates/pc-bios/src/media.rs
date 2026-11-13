//! The drives `int 13h` serves, read straight from their image files.
//!
//! The BIOS does not go through a disk controller: it has the image, and a
//! sector read is a file read into guest memory. A guest that later loads
//! its own driver talks to the controller and never comes back here.

use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::Path;

/// What kind of drive, which decides its sector size and its BIOS number.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DriveKind {
    /// A hard disk: 512-byte sectors, numbered from `0x80`.
    Disk,
    /// A CD-ROM: 2048-byte sectors, numbered from `0xE0`.
    Optical,
    /// A floppy disk: 512-byte sectors, numbered from `0x00`. The BIOS reads
    /// it for a loader (Windows Setup looks there for its answer file) and
    /// never boots from it; the machine has no floppy controller, so an
    /// operating system does not see it at all.
    Floppy,
}

/// The first BIOS drive number of each kind.
pub const FIRST_DISK: u8 = 0x80;
pub const FIRST_OPTICAL: u8 = 0xe0;

/// A cylinder/head/sector geometry for a disk addressed by LBA.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Geometry {
    pub cylinders: u32,
    pub heads: u32,
    pub sectors: u32,
}

impl Geometry {
    /// The translation BIOSes call LBA-assisted: 63 sectors, the fewest
    /// heads that keep the cylinders within 1024, and 255 heads beyond that.
    /// A floppy's geometry: two heads and 80 tracks, the sectors a track
    /// the size implies (18 for 1.44 MB, 9 for 720 KB).
    pub fn floppy(total: u64) -> Self {
        let sectors = (total / 160).clamp(1, 63);
        Self {
            cylinders: 80,
            heads: 2,
            sectors: sectors as u32,
        }
    }

    pub fn for_sectors(total: u64) -> Self {
        let sectors = 63u64;
        let mut heads = 16u64;
        while heads < 128 && total / (heads * sectors) > 1024 {
            heads *= 2;
        }
        if total / (heads * sectors) > 1024 {
            heads = 255;
        }
        let cylinders = (total / (heads * sectors)).clamp(1, 1024);
        Self {
            cylinders: cylinders as u32,
            heads: heads as u32,
            sectors: sectors as u32,
        }
    }

    /// The LBA of a CHS address, or `None` when it is outside the geometry.
    pub fn lba(&self, cylinder: u32, head: u32, sector: u32) -> Option<u64> {
        if sector == 0 || sector > self.sectors || head >= self.heads {
            return None;
        }
        Some(
            (u64::from(cylinder) * u64::from(self.heads) + u64::from(head))
                * u64::from(self.sectors)
                + u64::from(sector - 1),
        )
    }
}

/// What an El Torito boot catalog says to load.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ElTorito {
    /// The segment to load at; `0x7c0` when the catalog says 0.
    pub load_segment: u16,
    /// How many 512-byte sectors to load.
    pub sector_count: u16,
    /// The 2048-byte sector the image starts at.
    pub lba: u32,
}

/// One drive.
pub struct Drive {
    file: File,
    pub kind: DriveKind,
    pub writable: bool,
    /// Size in sectors of [`Self::sector_size`] bytes.
    pub sectors: u64,
    pub geometry: Geometry,
}

impl Drive {
    /// Opens `path`. A disk opened writable is written by `int 13h`.
    pub fn open(path: &Path, kind: DriveKind, writable: bool) -> io::Result<Self> {
        let writable = writable && kind != DriveKind::Optical;
        let file = File::options().read(true).write(writable).open(path)?;
        let bytes = file.metadata()?.len();
        Ok(Self::from_file(file, bytes, kind, writable))
    }

    fn from_file(file: File, bytes: u64, kind: DriveKind, writable: bool) -> Self {
        let sector = match kind {
            DriveKind::Disk | DriveKind::Floppy => 512,
            DriveKind::Optical => 2048,
        };
        let sectors = bytes / sector;
        Self {
            file,
            kind,
            writable,
            sectors,
            geometry: match kind {
                DriveKind::Floppy => Geometry::floppy(sectors),
                _ => Geometry::for_sectors(sectors),
            },
        }
    }

    pub fn sector_size(&self) -> u32 {
        match self.kind {
            DriveKind::Disk | DriveKind::Floppy => 512,
            DriveKind::Optical => 2048,
        }
    }

    fn range(&self, lba: u64, len: usize) -> io::Result<u64> {
        let size = u64::from(self.sector_size());
        let end = lba
            .checked_mul(size)
            .and_then(|at| at.checked_add(len as u64));
        match end {
            Some(end) if end <= self.sectors * size => Ok(lba * size),
            _ => Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "read past the end of the drive",
            )),
        }
    }

    /// Reads whole sectors starting at `lba`; `buf` is a multiple of the
    /// sector size.
    pub fn read(&mut self, lba: u64, buf: &mut [u8]) -> io::Result<()> {
        let at = self.range(lba, buf.len())?;
        self.file.seek(SeekFrom::Start(at))?;
        self.file.read_exact(buf)
    }

    /// Writes whole sectors starting at `lba`.
    pub fn write(&mut self, lba: u64, buf: &[u8]) -> io::Result<()> {
        if !self.writable {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "the drive is read-only",
            ));
        }
        let at = self.range(lba, buf.len())?;
        self.file.seek(SeekFrom::Start(at))?;
        self.file.write_all(buf)
    }

    /// The default boot entry of an El Torito disc, if this is a bootable
    /// one whose entry is a no-emulation image for a PC.
    pub fn el_torito(&mut self) -> Option<ElTorito> {
        if self.kind != DriveKind::Optical {
            return None;
        }
        let mut sector = [0u8; 2048];
        // The boot record volume descriptor is always sector 17.
        self.read(17, &mut sector).ok()?;
        if sector[0] != 0
            || &sector[1..6] != b"CD001"
            || &sector[7..30] != b"EL TORITO SPECIFICATION"
        {
            return None;
        }
        let catalog = u32::from_le_bytes([sector[0x47], sector[0x48], sector[0x49], sector[0x4a]]);
        self.read(u64::from(catalog), &mut sector).ok()?;
        // The validation entry: header 1, platform 0 (x86), key 55 AA.
        if sector[0] != 1 || sector[1] != 0 || sector[30] != 0x55 || sector[31] != 0xaa {
            return None;
        }
        let entry = &sector[32..64];
        // Bootable, and media type 0: no emulation.
        if entry[0] != 0x88 || entry[1] & 0x0f != 0 {
            return None;
        }
        let load_segment = match u16::from_le_bytes([entry[2], entry[3]]) {
            0 => 0x7c0,
            segment => segment,
        };
        Some(ElTorito {
            load_segment,
            sector_count: u16::from_le_bytes([entry[6], entry[7]]).max(1),
            lba: u32::from_le_bytes([entry[8], entry[9], entry[10], entry[11]]),
        })
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A drive over an anonymous temporary file holding `bytes`.
    pub(crate) fn drive(bytes: &[u8], kind: DriveKind) -> Drive {
        static NEXT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let path = std::env::temp_dir().join(format!(
            "pc-bios-test-{}-{}.img",
            std::process::id(),
            NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        std::fs::write(&path, bytes).expect("write the test image");
        let drive = Drive::open(&path, kind, true).expect("open the test image");
        // The open handle keeps the data on Unix; on Windows the file stays
        // until the test process ends, which a temp directory tolerates.
        let _ = std::fs::remove_file(&path);
        drive
    }

    /// A 40-sector disc with a no-emulation boot entry at sector 30.
    pub(crate) fn bootable_disc() -> Vec<u8> {
        let mut disc = vec![0u8; 40 * 2048];
        let record = &mut disc[17 * 2048..];
        record[1..6].copy_from_slice(b"CD001");
        record[6] = 1;
        record[7..30].copy_from_slice(b"EL TORITO SPECIFICATION");
        record[0x47..0x4b].copy_from_slice(&19u32.to_le_bytes());
        let catalog = &mut disc[19 * 2048..];
        catalog[0] = 1;
        catalog[30] = 0x55;
        catalog[31] = 0xaa;
        catalog[32] = 0x88;
        catalog[38..40].copy_from_slice(&4u16.to_le_bytes());
        catalog[40..44].copy_from_slice(&30u32.to_le_bytes());
        let image = &mut disc[30 * 2048..];
        image[..4].copy_from_slice(&[0xfa, 0xf4, 0xeb, 0xfd]);
        disc
    }

    #[test]
    fn geometry_keeps_cylinders_within_1024() {
        // 8 GiB: 255 heads.
        let big = Geometry::for_sectors(8 << 21);
        assert_eq!((big.heads, big.sectors), (255, 63));
        assert!(big.cylinders <= 1024);
        // 100 MiB fits with 16 heads.
        let small = Geometry::for_sectors(100 << 11);
        assert_eq!(small.heads, 16);
        assert_eq!(small.lba(0, 0, 1), Some(0));
        assert_eq!(small.lba(0, 1, 1), Some(63));
        assert_eq!(small.lba(0, 0, 0), None);
        assert_eq!(small.lba(0, 16, 1), None);
    }

    #[test]
    fn the_boot_catalog_is_read() {
        let mut disc = drive(&bootable_disc(), DriveKind::Optical);
        assert_eq!(
            disc.el_torito(),
            Some(ElTorito {
                load_segment: 0x7c0,
                sector_count: 4,
                lba: 30
            })
        );
    }

    #[test]
    fn a_read_past_the_end_fails_and_an_optical_drive_refuses_writes() {
        let mut disc = drive(&vec![0u8; 4 * 2048], DriveKind::Optical);
        let mut sector = [0u8; 2048];
        assert!(disc.read(3, &mut sector).is_ok());
        assert!(disc.read(4, &mut sector).is_err());
        assert!(disc.read(u64::MAX, &mut sector).is_err());
        assert!(disc.write(0, &sector).is_err());
    }
}
