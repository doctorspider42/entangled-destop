//! UDF (ECMA-167 / OSTA UDF 1.02–2.01), read only.
//!
//! Enough of it to walk a pressed or mastered disc: the anchor at sector 256,
//! the main volume descriptor sequence for the partition and the logical
//! volume, the file set descriptor, and then file entries (plain or extended)
//! whose allocation descriptors are short, long or embedded. A type-1
//! partition map only — the metadata partition of UDF 2.50 is refused, and so
//! are allocation-descriptor continuations, neither of which an installer ISO
//! uses. Every descriptor read is checked for its tag and tag checksum.

use std::io::{Read, Seek};

use super::{
    malformed, read_extents, read_sector, u16_le, u32_le, u64_le, DirEntry, Extent, MediaError,
    MediaFile, OpticalImage, MAX_DIR_BYTES, SECTOR,
};

/// Where the anchor volume descriptor pointer lives.
const ANCHOR_SECTOR: u64 = 256;
/// Most descriptors read from the main volume descriptor sequence.
const MAX_VDS_SECTORS: u64 = 64;
/// Most extents one file may have.
const MAX_EXTENTS: usize = 4096;

const TAG_PRIMARY_VD: u16 = 1;
const TAG_ANCHOR: u16 = 2;
const TAG_PARTITION: u16 = 5;
const TAG_LOGICAL_VOLUME: u16 = 6;
const TAG_TERMINATOR: u16 = 8;
const TAG_FILE_SET: u16 = 256;
const TAG_FILE_ID: u16 = 257;
const TAG_FILE_ENTRY: u16 = 261;
const TAG_EXTENDED_FILE_ENTRY: u16 = 266;

/// File characteristics of a file identifier descriptor.
const FID_DIRECTORY: u8 = 0x02;
const FID_DELETED: u8 = 0x04;
const FID_PARENT: u8 = 0x08;

/// ICB file type of a directory.
const ICB_DIRECTORY: u8 = 4;

/// Whether `r` carries a UDF volume recognition sequence (`NSR02`/`NSR03`
/// after `BEA01`, in the sectors after the 32 KiB system area).
pub fn is_udf<R: Read + Seek>(r: &mut R) -> Result<bool, MediaError> {
    for n in 16..16 + 32 {
        let Ok(sector) = read_sector(r, n) else {
            return Ok(false);
        };
        match &sector[1..6] {
            b"NSR02" | b"NSR03" => return Ok(true),
            b"BEA01" | b"CD001" | b"BOOT2" | b"CDW02" => {}
            _ => return Ok(false),
        }
    }
    Ok(false)
}

/// A UDF image being read.
pub struct UdfImage<R> {
    r: R,
    /// First sector of the (only) partition.
    partition: u64,
    root: DirEntry,
}

impl<R: Read + Seek> UdfImage<R> {
    pub fn new(mut r: R) -> Result<Self, MediaError> {
        let anchor = read_sector(&mut r, ANCHOR_SECTOR)?;
        check_tag(&anchor, TAG_ANCHOR, "anchor volume descriptor pointer")?;
        let vds_len = u64::from(u32_le(&anchor, 16));
        let vds_at = u64::from(u32_le(&anchor, 20));
        let mut partition = None;
        let mut file_set = None;
        for n in 0..(vds_len / SECTOR).min(MAX_VDS_SECTORS) {
            let d = read_sector(&mut r, vds_at + n)?;
            match u16_le(&d, 0) {
                TAG_PARTITION => {
                    check_tag(&d, TAG_PARTITION, "partition descriptor")?;
                    partition.get_or_insert(u64::from(u32_le(&d, 188)));
                }
                TAG_LOGICAL_VOLUME => {
                    check_tag(&d, TAG_LOGICAL_VOLUME, "logical volume descriptor")?;
                    let block = u32_le(&d, 212);
                    if u64::from(block) != SECTOR {
                        return Err(malformed(format!("UDF logical block size {block}")));
                    }
                    // The first partition map must be type 1 (a plain
                    // physical partition).
                    if u32_le(&d, 268) < 1 || d.get(440) != Some(&1) {
                        return Err(malformed(
                            "a UDF partition map other than type 1 (UDF 2.50 metadata?)",
                        ));
                    }
                    file_set.get_or_insert(u64::from(u32_le(&d, 252)));
                }
                TAG_TERMINATOR => break,
                TAG_PRIMARY_VD | 3 | 4 | 7 | 9 => {}
                _ => break,
            }
        }
        let partition = partition.ok_or_else(|| malformed("no UDF partition descriptor"))?;
        let file_set = file_set.ok_or_else(|| malformed("no UDF logical volume descriptor"))?;
        let mut image = Self {
            r,
            partition,
            root: DirEntry {
                name: String::new(),
                is_dir: true,
                file: MediaFile {
                    size: 0,
                    extents: Vec::new(),
                },
            },
        };
        let fsd = image.block(file_set)?;
        check_tag(&fsd, TAG_FILE_SET, "file set descriptor")?;
        let root_icb = u64::from(u32_le(&fsd, 404));
        let (is_dir, file) = image.file_entry(root_icb)?;
        if !is_dir {
            return Err(malformed("the UDF root is not a directory"));
        }
        image.root.file = file;
        Ok(image)
    }

    /// Logical block `lbn` of the partition.
    fn block(&mut self, lbn: u64) -> Result<[u8; SECTOR as usize], MediaError> {
        let sector = self
            .partition
            .checked_add(lbn)
            .ok_or_else(|| malformed("block number"))?;
        read_sector(&mut self.r, sector)
    }

    /// The file entry at `lbn`: whether it is a directory, and its data.
    fn file_entry(&mut self, lbn: u64) -> Result<(bool, MediaFile), MediaError> {
        let fe = self.block(lbn)?;
        let (ea_at, ad_base) = match u16_le(&fe, 0) {
            TAG_FILE_ENTRY => (168, 176),
            TAG_EXTENDED_FILE_ENTRY => (208, 216),
            other => return Err(malformed(format!("tag {other} where a file entry belongs"))),
        };
        check_tag(&fe, u16_le(&fe, 0), "file entry")?;
        let is_dir = fe[16 + 11] == ICB_DIRECTORY;
        let ad_type = u16_le(&fe, 16 + 18) & 7;
        let size = u64_le(&fe, 56);
        let ea_len = u32_le(&fe, ea_at) as usize;
        let ad_len = u32_le(&fe, ea_at + 4) as usize;
        let ads_at = ad_base + ea_len;
        let ads = fe
            .get(ads_at..ads_at.saturating_add(ad_len))
            .ok_or_else(|| malformed("allocation descriptors past their file entry"))?;
        let mut extents = Vec::new();
        let mut push = |kind: u32, len: u64, lbn: u64| -> Result<(), MediaError> {
            if extents.len() >= MAX_EXTENTS {
                return Err(malformed("too many extents"));
            }
            let start = match kind {
                0 => Some(
                    self.partition
                        .checked_add(lbn)
                        .and_then(|s| s.checked_mul(SECTOR))
                        .ok_or_else(|| malformed("extent location"))?,
                ),
                1 | 2 => None,
                _ => return Err(malformed("UDF allocation descriptor continuations")),
            };
            extents.push(Extent { start, len });
            Ok(())
        };
        match ad_type {
            // Short: length (2 bits of type), position.
            0 => {
                for ad in ads.chunks_exact(8) {
                    let raw = u32_le(ad, 0);
                    if raw & 0x3fff_ffff == 0 {
                        break;
                    }
                    push(
                        raw >> 30,
                        u64::from(raw & 0x3fff_ffff),
                        u64::from(u32_le(ad, 4)),
                    )?;
                }
            }
            // Long: length, logical block, partition reference.
            1 => {
                for ad in ads.chunks_exact(16) {
                    let raw = u32_le(ad, 0);
                    if raw & 0x3fff_ffff == 0 {
                        break;
                    }
                    if u16_le(ad, 8) != 0 {
                        return Err(malformed("a UDF extent in another partition"));
                    }
                    push(
                        raw >> 30,
                        u64::from(raw & 0x3fff_ffff),
                        u64::from(u32_le(ad, 4)),
                    )?;
                }
            }
            // Embedded: the data is the descriptors' space.
            3 => {
                let start = (self.partition + lbn) * SECTOR + ads_at as u64;
                extents.push(Extent {
                    start: Some(start),
                    len: ad_len as u64,
                });
            }
            other => return Err(malformed(format!("UDF allocation type {other}"))),
        }
        let recorded: u64 = extents.iter().map(|e| e.len).sum();
        if recorded < size {
            return Err(malformed("a UDF file longer than its extents"));
        }
        Ok((is_dir, MediaFile { size, extents }))
    }
}

/// Checks a descriptor tag: its identifier and its checksum.
fn check_tag(d: &[u8], id: u16, what: &str) -> Result<(), MediaError> {
    if u16_le(d, 0) != id {
        return Err(malformed(format!("no {what}")));
    }
    let sum = d[..16]
        .iter()
        .enumerate()
        .filter(|&(i, _)| i != 4)
        .fold(0u8, |acc, (_, &b)| acc.wrapping_add(b));
    if sum != d[4] {
        return Err(malformed(format!("bad tag checksum on the {what}")));
    }
    Ok(())
}

/// A UDF d-string or file identifier: compression id 8 (Latin-1) or 16
/// (UTF-16 big-endian).
fn decode_name(raw: &[u8]) -> Option<String> {
    let (&id, rest) = raw.split_first()?;
    match id {
        8 => Some(rest.iter().map(|&b| char::from(b)).collect()),
        16 => {
            let units: Vec<u16> = rest
                .chunks_exact(2)
                .map(|c| u16::from_be_bytes([c[0], c[1]]))
                .collect();
            Some(String::from_utf16_lossy(&units))
        }
        _ => None,
    }
}

impl<R: Read + Seek> OpticalImage for UdfImage<R> {
    fn root(&self) -> &DirEntry {
        &self.root
    }

    fn list(&mut self, dir: &DirEntry) -> Result<Vec<DirEntry>, MediaError> {
        if !dir.is_dir {
            return Err(malformed("listing a file"));
        }
        let data = self.read_file(&dir.file, MAX_DIR_BYTES)?;
        let mut out = Vec::new();
        let mut at = 0usize;
        while at + 38 <= data.len() {
            let fid = &data[at..];
            if u16_le(fid, 0) != TAG_FILE_ID {
                break;
            }
            check_tag(fid, TAG_FILE_ID, "file identifier")?;
            let flags = fid[18];
            let name_len = usize::from(fid[19]);
            let icb = u64::from(u32_le(fid, 24));
            let iu_len = usize::from(u16_le(fid, 36));
            let len = (38 + iu_len + name_len).next_multiple_of(4);
            let raw = fid
                .get(38 + iu_len..38 + iu_len + name_len)
                .ok_or_else(|| malformed("file identifier past its directory"))?;
            at += len;
            if flags & (FID_PARENT | FID_DELETED) != 0 {
                continue;
            }
            let Some(name) = decode_name(raw) else {
                continue;
            };
            let (is_dir, file) = self.file_entry(icb)?;
            out.push(DirEntry {
                name,
                is_dir: is_dir || flags & FID_DIRECTORY != 0,
                file,
            });
        }
        Ok(out)
    }

    fn read_at(&mut self, file: &MediaFile, offset: u64, buf: &mut [u8]) -> Result<(), MediaError> {
        read_extents(&mut self.r, file, offset, buf)
    }
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use super::*;
    use crate::optical::walk;

    const PARTITION: usize = 300;

    fn tag(d: &mut [u8], id: u16) {
        d[0..2].copy_from_slice(&id.to_le_bytes());
        d[2..4].copy_from_slice(&2u16.to_le_bytes());
        let sum = d[..16]
            .iter()
            .enumerate()
            .filter(|&(i, _)| i != 4)
            .fold(0u8, |acc, (_, &b)| acc.wrapping_add(b));
        d[4] = sum;
    }

    fn sector(image: &mut [u8], n: usize) -> &mut [u8] {
        &mut image[n * 2048..(n + 1) * 2048]
    }

    /// A file entry at partition block `lbn`; `data` is either embedded or a
    /// short extent at block `at`.
    fn file_entry(image: &mut [u8], lbn: usize, dir: bool, data: &[u8], at: Option<usize>) {
        let fe = sector(image, PARTITION + lbn);
        fe[16 + 11] = if dir { ICB_DIRECTORY } else { 5 };
        fe[56..64].copy_from_slice(&(data.len() as u64).to_le_bytes());
        match at {
            None => {
                fe[16 + 18] = 3;
                fe[172..176].copy_from_slice(&(data.len() as u32).to_le_bytes());
                fe[176..176 + data.len()].copy_from_slice(data);
            }
            Some(at) => {
                fe[172..176].copy_from_slice(&8u32.to_le_bytes());
                fe[176..180].copy_from_slice(&(data.len() as u32).to_le_bytes());
                fe[180..184].copy_from_slice(&(at as u32).to_le_bytes());
            }
        }
        tag(fe, TAG_FILE_ENTRY);
        if let Some(at) = at {
            let start = (PARTITION + at) * 2048;
            image[start..start + data.len()].copy_from_slice(data);
        }
    }

    fn fid(name: Option<&str>, flags: u8, icb: u32) -> Vec<u8> {
        let raw: Vec<u8> = match name {
            Some(n) => std::iter::once(8).chain(n.bytes()).collect(),
            None => Vec::new(),
        };
        let len = (38 + raw.len()).next_multiple_of(4);
        let mut d = vec![0u8; len];
        d[18] = flags;
        d[19] = raw.len() as u8;
        d[24..28].copy_from_slice(&icb.to_le_bytes());
        d[38..38 + raw.len()].copy_from_slice(&raw);
        tag(&mut d, TAG_FILE_ID);
        d
    }

    /// `/sources/install.esd` (embedded) and `/setup.exe` (one extent).
    fn sample() -> Vec<u8> {
        let mut image = vec![0u8; (PARTITION + 16) * 2048];
        for (n, id) in [(16, b"BEA01"), (17, b"NSR02"), (18, b"TEA01")] {
            sector(&mut image, n)[1..6].copy_from_slice(id);
        }
        let anchor = sector(&mut image, 256);
        anchor[16..20].copy_from_slice(&(4u32 * 2048).to_le_bytes());
        anchor[20..24].copy_from_slice(&32u32.to_le_bytes());
        tag(anchor, TAG_ANCHOR);
        let pd = sector(&mut image, 32);
        pd[188..192].copy_from_slice(&(PARTITION as u32).to_le_bytes());
        tag(pd, TAG_PARTITION);
        let lvd = sector(&mut image, 33);
        lvd[212..216].copy_from_slice(&2048u32.to_le_bytes());
        lvd[252..256].copy_from_slice(&0u32.to_le_bytes());
        lvd[268..272].copy_from_slice(&1u32.to_le_bytes());
        lvd[440] = 1;
        lvd[441] = 6;
        tag(lvd, TAG_LOGICAL_VOLUME);
        tag(sector(&mut image, 34), TAG_TERMINATOR);
        let fsd = sector(&mut image, PARTITION);
        fsd[404..408].copy_from_slice(&1u32.to_le_bytes());
        tag(fsd, TAG_FILE_SET);
        let root: Vec<u8> = [
            fid(None, FID_DIRECTORY | FID_PARENT, 1),
            fid(Some("sources"), FID_DIRECTORY, 3),
            fid(Some("setup.exe"), 0, 5),
            fid(Some("gone.txt"), FID_DELETED, 5),
        ]
        .concat();
        file_entry(&mut image, 1, true, &root, Some(2));
        let sources = [
            fid(None, FID_DIRECTORY | FID_PARENT, 1),
            fid(Some("install.esd"), 0, 6),
        ]
        .concat();
        file_entry(&mut image, 3, true, &sources, Some(4));
        file_entry(&mut image, 5, false, &[0x4d; 3000], Some(7));
        file_entry(&mut image, 6, false, b"MSWIM\0\0\0", None);
        image
    }

    #[test]
    fn a_udf_tree_reads_with_embedded_and_extent_files() {
        let mut bytes = sample();
        assert!(is_udf(&mut Cursor::new(&mut bytes)).unwrap());
        let mut image = UdfImage::new(Cursor::new(bytes)).unwrap();
        let root = image.root().clone();
        let files: Vec<String> = walk(&mut image, &root)
            .unwrap()
            .into_iter()
            .map(|(p, _)| p)
            .collect();
        assert_eq!(files, ["setup.exe", "sources/install.esd"]);
        let esd = image.find("SOURCES/Install.esd").unwrap().unwrap();
        assert_eq!(image.read_file(&esd.file, 64).unwrap(), b"MSWIM\0\0\0");
        let setup = image.find("setup.exe").unwrap().unwrap();
        let mut tail = [0u8; 8];
        image.read_at(&setup.file, 2992, &mut tail).unwrap();
        assert_eq!(tail, [0x4d; 8]);
        assert!(image.read_at(&setup.file, 2993, &mut tail).is_err());
    }

    #[test]
    fn an_iso_without_udf_is_not_udf() {
        let mut iso = crate::optical::iso9660::IsoBuilder::new("X").build();
        assert!(!is_udf(&mut Cursor::new(&mut iso)).unwrap());
    }

    #[test]
    fn hostile_udf_is_an_error_not_a_panic() {
        let good = sample();
        let interesting = [256usize, 32, 33, PARTITION, PARTITION + 1, PARTITION + 2]
            .into_iter()
            .chain([PARTITION + 3, PARTITION + 4, PARTITION + 5, PARTITION + 6]);
        for s in interesting {
            for offset in (0..400).step_by(3) {
                let mut bad = good.clone();
                bad[s * 2048 + offset] ^= 0x5a;
                if let Ok(mut image) = UdfImage::new(Cursor::new(bad)) {
                    let root = image.root().clone();
                    if let Ok(files) = walk(&mut image, &root) {
                        for (_, entry) in files {
                            let _ = image.read_file(&entry.file, 1 << 20);
                        }
                    }
                }
            }
        }
    }

    /// The real thing, when one is at hand: `ENTANGLED_WINDOWS_ISO=<path>`.
    #[test]
    fn a_windows_iso_lists_its_install_image() {
        let Some(path) = std::env::var_os("ENTANGLED_WINDOWS_ISO") else {
            eprintln!("skipped: set ENTANGLED_WINDOWS_ISO to a Windows installation ISO");
            return;
        };
        let mut image = crate::optical::open(std::path::Path::new(&path)).unwrap();
        let wim = image
            .find("sources/install.wim")
            .unwrap()
            .or(image.find("sources/install.esd").unwrap())
            .expect("an install image");
        let mut magic = [0u8; 8];
        image.read_at(&wim.file, 0, &mut magic).unwrap();
        assert_eq!(&magic, b"MSWIM\0\0\0");
    }
}
