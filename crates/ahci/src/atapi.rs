//! An ATAPI DVD-ROM drive over an ISO image: the SCSI (MMC) commands a
//! firmware, Windows' `cdrom.sys` and Linux's `sr` actually send.
//!
//! Everything a command returns is built here; the HBA moves the bytes. The
//! packet comes from the guest, so every field is bounds-checked, an unknown
//! command is a CHECK CONDITION with ILLEGAL REQUEST sense rather than an
//! error, and nothing here can panic on a guest value.

use std::fs::File;
use std::path::Path;

/// Logical block size of an optical medium.
pub const BLOCK_SIZE: u64 = 2048;

/// Sense data: what REQUEST SENSE reports after a CHECK CONDITION.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Sense {
    pub key: u8,
    pub asc: u8,
    pub ascq: u8,
}

impl Sense {
    /// ILLEGAL REQUEST / INVALID COMMAND OPERATION CODE.
    pub const INVALID_OPCODE: Self = Self {
        key: 0x05,
        asc: 0x20,
        ascq: 0x00,
    };
    /// ILLEGAL REQUEST / INVALID FIELD IN CDB.
    pub const INVALID_FIELD: Self = Self {
        key: 0x05,
        asc: 0x24,
        ascq: 0x00,
    };
    /// ILLEGAL REQUEST / LOGICAL BLOCK ADDRESS OUT OF RANGE.
    pub const OUT_OF_RANGE: Self = Self {
        key: 0x05,
        asc: 0x21,
        ascq: 0x00,
    };
    /// MEDIUM ERROR / UNRECOVERED READ ERROR: the host file failed.
    pub const READ_ERROR: Self = Self {
        key: 0x03,
        asc: 0x11,
        ascq: 0x00,
    };
}

/// What a packet command asks the HBA to do with the data phase.
#[derive(Debug, PartialEq, Eq)]
pub enum Reply {
    /// No data, GOOD status.
    Done,
    /// These bytes to the host, GOOD status.
    Data(Vec<u8>),
    /// `blocks` blocks from `lba`, read straight from the image by the HBA
    /// into the guest's buffers (no staging of the whole transfer).
    Read { lba: u64, blocks: u64 },
    /// CHECK CONDITION with this sense.
    Check(Sense),
}

/// One drive with a medium in it.
#[derive(Debug)]
pub struct Atapi {
    image: File,
    blocks: u64,
    /// The sense REQUEST SENSE returns next, then clears.
    sense: Sense,
    /// Identification strings, already padded to their ATA/SCSI widths.
    serial: String,
}

fn be16(value: u64) -> [u8; 2] {
    (value.min(u64::from(u16::MAX)) as u16).to_be_bytes()
}

fn be32(value: u64) -> [u8; 4] {
    (value.min(u64::from(u32::MAX)) as u32).to_be_bytes()
}

/// Copies `text` into `out`, space-padded, as SCSI strings are.
fn put_ascii(out: &mut [u8], text: &str) {
    out.fill(b' ');
    for (slot, byte) in out.iter_mut().zip(text.bytes()) {
        *slot = byte;
    }
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

/// Logical block address to MSF (minute, second, frame), for READ TOC with
/// the MSF bit: the 150-frame (2 s) pregap is added, as the MMC spec says.
fn lba_to_msf(lba: u64) -> [u8; 4] {
    let frames = lba + 150;
    let minute = (frames / (60 * 75)).min(255) as u8;
    let second = ((frames / 75) % 60) as u8;
    let frame = (frames % 75) as u8;
    [0, minute, second, frame]
}

impl Atapi {
    /// Opens `path` read-only. Its size is rounded down to whole blocks.
    pub fn open(path: &Path, serial: &str) -> std::io::Result<Self> {
        let image = File::open(path)?;
        let blocks = image.metadata()?.len() / BLOCK_SIZE;
        Ok(Self {
            image,
            blocks,
            sense: Sense::default(),
            serial: serial.to_owned(),
        })
    }

    pub fn blocks(&self) -> u64 {
        self.blocks
    }

    /// Reads `buf.len()` bytes at byte `offset` of the image.
    pub fn read_at(&self, buf: &mut [u8], offset: u64) -> std::io::Result<()> {
        #[cfg(unix)]
        {
            use std::os::unix::fs::FileExt;
            self.image.read_exact_at(buf, offset)
        }
        #[cfg(windows)]
        {
            use std::os::windows::fs::FileExt;
            let mut done = 0usize;
            while done < buf.len() {
                let read = self
                    .image
                    .seek_read(&mut buf[done..], offset + done as u64)?;
                if read == 0 {
                    return Err(std::io::ErrorKind::UnexpectedEof.into());
                }
                done += read;
            }
            Ok(())
        }
    }

    /// Records the sense a failed command leaves for REQUEST SENSE.
    pub fn set_sense(&mut self, sense: Sense) {
        self.sense = sense;
    }

    /// The 512-byte IDENTIFY PACKET DEVICE reply.
    pub fn identify_packet(&self) -> Vec<u8> {
        let mut words = [0u16; 256];
        // ATAPI, CD/DVD device type (5), removable, 12-byte packets.
        words[0] = 0x85c0;
        put_ata_string(&mut words[10..20], &self.serial);
        put_ata_string(&mut words[23..27], "1.0");
        put_ata_string(&mut words[27..47], "Entangled Virtual DVD-ROM");
        // DMA and LBA supported.
        words[49] = 0x0300;
        // Words 64–70 and 88 are valid.
        words[53] = 0x0006;
        // Multiword DMA 0–2 supported.
        words[63] = 0x0007;
        // PIO modes 3 and 4.
        words[64] = 0x0003;
        words[65..69].fill(120);
        // ATA/ATAPI-7.
        words[80] = 0x00f0;
        // PACKET feature set supported and enabled.
        words[82] = 1 << 4;
        words[85] = 1 << 4;
        // Ultra DMA 0–5 supported, 5 selected.
        words[88] = 0x203f;
        words.iter().flat_map(|w| w.to_le_bytes()).collect()
    }

    /// Runs one 12/16-byte packet.
    pub fn packet(&mut self, cdb: &[u8; 16]) -> Reply {
        let reply = self.dispatch(cdb);
        match reply {
            Reply::Check(sense) => self.sense = sense,
            // REQUEST SENSE consumed it; everything else that succeeded
            // leaves no sense behind.
            _ if cdb[0] != 0x03 => self.sense = Sense::default(),
            _ => {}
        }
        reply
    }

    fn dispatch(&mut self, cdb: &[u8; 16]) -> Reply {
        let allocation16 = u64::from(u16::from_be_bytes([cdb[7], cdb[8]]));
        match cdb[0] {
            // TEST UNIT READY: the medium is always there.
            0x00 => Reply::Done,
            0x03 => self.request_sense(u64::from(cdb[4])),
            0x12 => self.inquiry(cdb),
            // START STOP UNIT, PREVENT ALLOW MEDIUM REMOVAL, SET CD SPEED,
            // SEEK(10): nothing to do for a disc that never moves.
            0x1b | 0x1e | 0xbb | 0x2b => Reply::Done,
            0x1a => self.mode_sense(cdb[2] & 0x3f, u64::from(cdb[4]), false),
            0x5a => self.mode_sense(cdb[2] & 0x3f, allocation16, true),
            0x25 => self.read_capacity(),
            0x28 => {
                let lba = u64::from(u32::from_be_bytes([cdb[2], cdb[3], cdb[4], cdb[5]]));
                self.read(lba, u64::from(u16::from_be_bytes([cdb[7], cdb[8]])))
            }
            0xa8 => {
                let lba = u64::from(u32::from_be_bytes([cdb[2], cdb[3], cdb[4], cdb[5]]));
                let count = u64::from(u32::from_be_bytes([cdb[6], cdb[7], cdb[8], cdb[9]]));
                self.read(lba, count)
            }
            0x43 => self.read_toc(cdb, allocation16),
            0x46 => self.get_configuration(u16::from_be_bytes([cdb[2], cdb[3]]), allocation16),
            0x4a => self.event_status(cdb, allocation16),
            0x51 => self.disc_information(allocation16),
            0x52 => self.track_information(allocation16),
            0xad => self.disc_structure(cdb[7], allocation16),
            0xbd => Reply::Data(
                vec![0u8; 8]
                    .into_iter()
                    .take(allocation16 as usize)
                    .collect(),
            ),
            _ => Reply::Check(Sense::INVALID_OPCODE),
        }
    }

    fn request_sense(&mut self, allocation: u64) -> Reply {
        let mut data = vec![0u8; 18];
        data[0] = 0x70;
        data[2] = self.sense.key;
        data[7] = 10;
        data[12] = self.sense.asc;
        data[13] = self.sense.ascq;
        self.sense = Sense::default();
        data.truncate(allocation as usize);
        Reply::Data(data)
    }

    fn inquiry(&self, cdb: &[u8; 16]) -> Reply {
        let allocation = u64::from(u16::from_be_bytes([cdb[3], cdb[4]]));
        if cdb[1] & 1 != 0 {
            // Vital product data: only the list of supported pages.
            if cdb[2] != 0 {
                return Reply::Check(Sense::INVALID_FIELD);
            }
            let mut data = vec![0x05, 0x00, 0x00, 1, 0x00];
            data.truncate(allocation as usize);
            return Reply::Data(data);
        }
        let mut data = vec![0u8; 36];
        data[0] = 0x05; // CD/DVD
        data[1] = 0x80; // removable
        data[2] = 0x05; // SPC-3
        data[3] = 0x32; // response data format 2
        data[4] = 31; // additional length
        put_ascii(&mut data[8..16], "ENTANGLD");
        put_ascii(&mut data[16..32], "Virtual DVD-ROM");
        put_ascii(&mut data[32..36], "1.0");
        data.truncate(allocation as usize);
        Reply::Data(data)
    }

    fn mode_sense(&self, page: u8, allocation: u64, ten: bool) -> Reply {
        // The CD capabilities page (0x2a), for 0x2a or "all pages".
        let mut pages = Vec::new();
        if page == 0x2a || page == 0x3f {
            let mut capabilities = vec![0u8; 22];
            capabilities[0] = 0x2a;
            capabilities[1] = 20;
            capabilities[2] = 0x3b; // reads CD-R, CD-RW, DVD-ROM, DVD-R, DVD-RAM
            capabilities[4] = 0x71; // audio play, composite, digital ports
            capabilities[5] = 0xff;
            capabilities[6] = 0x29; // tray, eject, lock
            pages.extend_from_slice(&capabilities);
        } else if page != 0x01 && page != 0x0d && page != 0x0e && page != 0x1a {
            return Reply::Check(Sense::INVALID_FIELD);
        }
        let mut data = if ten {
            let mut header = vec![0u8; 8];
            header[0..2].copy_from_slice(&be16(6 + pages.len() as u64));
            header
        } else {
            vec![(3 + pages.len()) as u8, 0, 0, 0]
        };
        data.extend_from_slice(&pages);
        data.truncate(allocation as usize);
        Reply::Data(data)
    }

    fn read_capacity(&self) -> Reply {
        let mut data = vec![0u8; 8];
        data[0..4].copy_from_slice(&be32(self.blocks.saturating_sub(1)));
        data[4..8].copy_from_slice(&be32(BLOCK_SIZE));
        Reply::Data(data)
    }

    fn read(&self, lba: u64, blocks: u64) -> Reply {
        match lba.checked_add(blocks) {
            Some(end) if end <= self.blocks => {}
            _ => return Reply::Check(Sense::OUT_OF_RANGE),
        }
        if blocks == 0 {
            return Reply::Done;
        }
        Reply::Read { lba, blocks }
    }

    fn read_toc(&self, cdb: &[u8; 16], allocation: u64) -> Reply {
        let msf = cdb[1] & 0x02 != 0;
        let address = |lba: u64| if msf { lba_to_msf(lba) } else { be32(lba) };
        let mut data = vec![0u8; 4];
        match cdb[2] & 0x0f {
            // The TOC: one data track and the lead-out.
            0 => {
                data[2] = 1;
                data[3] = 1;
                for (track, lba) in [(1u8, 0u64), (0xaa, self.blocks)] {
                    data.extend_from_slice(&[0, 0x14, track, 0]);
                    data.extend_from_slice(&address(lba));
                }
            }
            // Session information: one complete session.
            1 => {
                data[2] = 1;
                data[3] = 1;
                data.extend_from_slice(&[0, 0x14, 1, 0]);
                data.extend_from_slice(&address(0));
            }
            _ => return Reply::Check(Sense::INVALID_FIELD),
        }
        let length = data.len() as u64 - 2;
        data[0..2].copy_from_slice(&be16(length));
        data.truncate(allocation as usize);
        Reply::Data(data)
    }

    fn get_configuration(&self, start: u16, allocation: u64) -> Reply {
        // (feature code, version/persistent/current byte, payload)
        let features: [(u16, u8, Vec<u8>); 5] = [
            // Profile list: DVD-ROM, current.
            (0x0000, 0x03, vec![0x00, 0x10, 0x01, 0x00]),
            // Core: ATAPI.
            (0x0001, 0x0b, vec![0, 0, 0, 2, 0x01, 0, 0, 0]),
            // Removable medium: tray, ejectable, lockable.
            (0x0003, 0x03, vec![0x29, 0, 0, 0]),
            // Random readable: 2048-byte blocks, blocking 1.
            (0x0010, 0x03, {
                let mut payload = be32(BLOCK_SIZE).to_vec();
                payload.extend_from_slice(&[0, 1, 0, 0]);
                payload
            }),
            // DVD read.
            (0x001f, 0x03, vec![0, 0, 0, 0]),
        ];
        let mut data = vec![0u8; 8];
        data[6..8].copy_from_slice(&0x0010u16.to_be_bytes());
        for (code, flags, payload) in features.iter().filter(|(c, _, _)| *c >= start) {
            data.extend_from_slice(&code.to_be_bytes());
            data.push(*flags);
            data.push(payload.len() as u8);
            data.extend_from_slice(payload);
        }
        let length = data.len() as u64 - 4;
        data[0..4].copy_from_slice(&be32(length));
        data.truncate(allocation as usize);
        Reply::Data(data)
    }

    fn event_status(&self, cdb: &[u8; 16], allocation: u64) -> Reply {
        // Only the polled form is defined for ATAPI.
        if cdb[1] & 1 == 0 {
            return Reply::Check(Sense::INVALID_FIELD);
        }
        let media_requested = cdb[4] & 0x10 != 0;
        let mut data = if media_requested {
            // Header: 4 bytes of event data, media class, media supported;
            // then "no change, medium present".
            vec![0, 4, 0x04, 0x10, 0, 0x02, 0, 0]
        } else {
            // No event of a requested class: NEA set.
            vec![0, 2, 0x80, 0x10]
        };
        data.truncate(allocation as usize);
        Reply::Data(data)
    }

    fn disc_information(&self, allocation: u64) -> Reply {
        let mut data = vec![0u8; 34];
        data[0..2].copy_from_slice(&be16(32));
        // Last session complete, disc complete.
        data[2] = 0x0e;
        data[3] = 1; // first track
        data[4] = 1; // sessions
        data[5] = 1; // first track of the last session
        data[6] = 1; // last track of the last session
        data.truncate(allocation as usize);
        Reply::Data(data)
    }

    fn track_information(&self, allocation: u64) -> Reply {
        let mut data = vec![0u8; 36];
        data[0..2].copy_from_slice(&be16(34));
        data[2] = 1; // track
        data[3] = 1; // session
        data[5] = 0x04; // data track
        data[6] = 0x01; // mode 1
        data[24..28].copy_from_slice(&be32(self.blocks));
        data.truncate(allocation as usize);
        Reply::Data(data)
    }

    fn disc_structure(&self, format: u8, allocation: u64) -> Reply {
        let body: Vec<u8> = match format {
            // Physical format: DVD-ROM, one layer, 120 mm.
            0x00 => {
                let mut body = vec![0u8; 2048];
                body[0] = 0x01;
                body[1] = 0x0f;
                body[2] = 0x01;
                body
            }
            // Copyright: no protection.
            0x01 => vec![0, 0, 0, 0],
            _ => return Reply::Check(Sense::INVALID_FIELD),
        };
        let mut data = be16(body.len() as u64 + 2).to_vec();
        data.extend_from_slice(&[0, 0]);
        data.extend_from_slice(&body);
        data.truncate(allocation as usize);
        Reply::Data(data)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn drive(blocks: u64) -> (Atapi, tempfile_path::TempPath) {
        let path = tempfile_path::TempPath::new(blocks * BLOCK_SIZE);
        (Atapi::open(&path.0, "TEST").unwrap(), path)
    }

    /// A temporary image file, removed on drop, without a dev-dependency.
    mod tempfile_path {
        use std::sync::atomic::{AtomicUsize, Ordering};
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        pub struct TempPath(pub std::path::PathBuf);
        impl TempPath {
            pub fn new(len: u64) -> Self {
                let path = std::env::temp_dir().join(format!(
                    "entangled-atapi-{}-{}.iso",
                    std::process::id(),
                    NEXT.fetch_add(1, Ordering::Relaxed)
                ));
                let file = std::fs::File::create(&path).unwrap();
                file.set_len(len).unwrap();
                Self(path)
            }
        }
        impl Drop for TempPath {
            fn drop(&mut self) {
                let _ = std::fs::remove_file(&self.0);
            }
        }
    }

    fn cdb(bytes: &[u8]) -> [u8; 16] {
        let mut cdb = [0u8; 16];
        cdb[..bytes.len()].copy_from_slice(bytes);
        cdb
    }

    #[test]
    fn inquiry_reports_a_removable_dvd() {
        let (mut drive, _path) = drive(16);
        let Reply::Data(data) = drive.packet(&cdb(&[0x12, 0, 0, 0, 36])) else {
            panic!()
        };
        assert_eq!(data[0], 0x05);
        assert_eq!(data[1], 0x80);
        assert_eq!(&data[8..16], b"ENTANGLD");
    }

    #[test]
    fn read_capacity_is_the_last_block_in_2k_blocks() {
        let (mut drive, _path) = drive(100);
        let Reply::Data(data) = drive.packet(&cdb(&[0x25])) else {
            panic!()
        };
        assert_eq!(u32::from_be_bytes(data[0..4].try_into().unwrap()), 99);
        assert_eq!(u32::from_be_bytes(data[4..8].try_into().unwrap()), 2048);
    }

    #[test]
    fn reads_inside_the_medium_and_refuses_past_it() {
        let (mut drive, _path) = drive(100);
        assert_eq!(
            drive.packet(&cdb(&[0x28, 0, 0, 0, 0, 10, 0, 0, 5])),
            Reply::Read { lba: 10, blocks: 5 }
        );
        assert_eq!(
            drive.packet(&cdb(&[0x28, 0, 0, 0, 0, 98, 0, 0, 5])),
            Reply::Check(Sense::OUT_OF_RANGE)
        );
        assert_eq!(
            drive.packet(&cdb(&[
                0xa8, 0, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff
            ])),
            Reply::Check(Sense::OUT_OF_RANGE),
            "no overflow"
        );
    }

    #[test]
    fn an_unknown_command_leaves_sense_for_request_sense() {
        let (mut drive, _path) = drive(1);
        assert_eq!(
            drive.packet(&cdb(&[0xff])),
            Reply::Check(Sense::INVALID_OPCODE)
        );
        let Reply::Data(sense) = drive.packet(&cdb(&[0x03, 0, 0, 0, 18])) else {
            panic!()
        };
        assert_eq!((sense[2], sense[12]), (0x05, 0x20));
        let Reply::Data(sense) = drive.packet(&cdb(&[0x03, 0, 0, 0, 18])) else {
            panic!()
        };
        assert_eq!(sense[2], 0, "read once, then cleared");
    }

    #[test]
    fn toc_has_one_data_track_and_the_lead_out() {
        let (mut drive, _path) = drive(1000);
        let Reply::Data(toc) = drive.packet(&cdb(&[0x43, 0, 0, 0, 0, 0, 0, 0x03, 0x24])) else {
            panic!()
        };
        assert_eq!(&toc[2..4], &[1, 1]);
        assert_eq!(toc[6], 1);
        assert_eq!(toc[14], 0xaa);
        assert_eq!(u32::from_be_bytes(toc[16..20].try_into().unwrap()), 1000);
    }

    #[test]
    fn configuration_says_dvd_rom() {
        let (mut drive, _path) = drive(1);
        let Reply::Data(data) = drive.packet(&cdb(&[0x46, 0, 0, 0, 0, 0, 0, 0x01, 0x00])) else {
            panic!()
        };
        assert_eq!(u16::from_be_bytes([data[6], data[7]]), 0x0010);
    }

    #[test]
    fn allocation_lengths_truncate_and_never_panic() {
        let (mut drive, _path) = drive(1);
        for opcode in [0x12u8, 0x5a, 0x43, 0x46, 0x4a, 0x51, 0x52, 0xad, 0xbd, 0x03] {
            for allocation in [0u8, 1, 3] {
                let _ = drive.packet(&cdb(&[opcode, 1, 0x3f, 0, allocation, 0, 0, 0, allocation]));
            }
        }
    }

    #[test]
    fn identify_packet_is_atapi_with_strings_swapped() {
        let (drive, _path) = drive(1);
        let id = drive.identify_packet();
        assert_eq!(id.len(), 512);
        assert_eq!(u16::from_le_bytes([id[0], id[1]]), 0x85c0);
        // "En" of the model string, byte-swapped inside word 27.
        assert_eq!(&id[54..56], b"nE");
    }

    #[test]
    fn read_at_reads_the_image() {
        let path = tempfile_path::TempPath::new(0);
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .open(&path.0)
            .unwrap();
        file.write_all(&[7u8; 4096]).unwrap();
        drop(file);
        let drive = Atapi::open(&path.0, "T").unwrap();
        let mut buf = [0u8; 16];
        drive.read_at(&mut buf, 2048).unwrap();
        assert_eq!(buf, [7u8; 16]);
    }
}
