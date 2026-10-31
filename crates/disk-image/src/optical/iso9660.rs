//! ISO 9660 (ECMA-119) with Joliet names: a reader and a writer.
//!
//! The reader follows the Joliet tree when the image has one and the primary
//! tree otherwise; both are directory records of the same shape, Joliet's
//! names are UCS-2 big-endian. Multi-extent files (flag 0x80, how an image
//! stores a file over 4 GiB) are joined into one [`MediaFile`].
//!
//! The writer, [`IsoBuilder`], lays out a small image with directories: a
//! primary tree with upper-case level-2 names and a Joliet tree with the real
//! ones, sharing the file data. Windows reads the Joliet tree, which is what
//! lets `$WinPEDriver$` keep its `$` signs; Linux's `isofs` does too.

use std::collections::BTreeMap;
use std::io::{Read, Seek};

use super::{
    malformed, read_extents, read_sector, u32_le, DirEntry, Extent, MediaError, MediaFile,
    OpticalImage, MAX_DIR_BYTES, SECTOR,
};

/// The volume descriptors start after the 32 KiB system area.
const FIRST_DESCRIPTOR: u64 = 16;
/// How many descriptors are looked at before giving up on a terminator.
const MAX_DESCRIPTORS: u64 = 32;
/// Directory flag of a directory record.
const FLAG_DIR: u8 = 0x02;
/// "More extents of this file follow" flag of a directory record.
const FLAG_MULTI_EXTENT: u8 = 0x80;
/// Joliet's escape sequences (UCS-2 levels 1, 2 and 3).
const JOLIET_ESCAPES: [&[u8; 3]; 3] = [b"%/@", b"%/C", b"%/E"];

/// An ISO 9660 image being read.
pub struct IsoImage<R> {
    r: R,
    root: DirEntry,
    joliet: bool,
}

impl<R: Read + Seek> IsoImage<R> {
    /// Reads the volume descriptors; [`MediaError::NotAnImage`] when there
    /// are none.
    pub fn new(mut r: R) -> Result<Self, MediaError> {
        let mut primary = None;
        let mut joliet = None;
        for n in FIRST_DESCRIPTOR..FIRST_DESCRIPTOR + MAX_DESCRIPTORS {
            let Ok(sector) = read_sector(&mut r, n) else {
                break;
            };
            if &sector[1..6] != b"CD001" {
                break;
            }
            match sector[0] {
                1 if primary.is_none() => primary = Some(root_of(&sector, false)?),
                2 if joliet.is_none() && JOLIET_ESCAPES.iter().any(|e| &sector[88..91] == *e) => {
                    joliet = Some(root_of(&sector, true)?);
                }
                255 => break,
                _ => {}
            }
        }
        let (root, joliet) = match (joliet, primary) {
            (Some(root), _) => (root, true),
            (None, Some(root)) => (root, false),
            (None, None) => return Err(MediaError::NotAnImage(Default::default())),
        };
        Ok(Self { r, root, joliet })
    }

    /// Whether names come from the Joliet tree.
    pub fn is_joliet(&self) -> bool {
        self.joliet
    }
}

/// The root directory of a primary or supplementary volume descriptor.
fn root_of(descriptor: &[u8], joliet: bool) -> Result<DirEntry, MediaError> {
    let block_size = super::u16_le(descriptor, 128);
    if u64::from(block_size) != SECTOR {
        return Err(malformed(format!("logical block size {block_size}")));
    }
    let (_, entry, _) =
        parse_record(&descriptor[156..190], joliet)?.ok_or_else(|| malformed("root record"))?;
    Ok(DirEntry {
        name: String::new(),
        is_dir: true,
        ..entry
    })
}

/// One directory record: its raw name, the entry, and its flags.
fn parse_record(rec: &[u8], joliet: bool) -> Result<Option<(Vec<u8>, DirEntry, u8)>, MediaError> {
    let len = usize::from(*rec.first().unwrap_or(&0));
    if len < 34 || len > rec.len() {
        return Err(malformed("directory record length"));
    }
    let name_len = usize::from(rec[32]);
    if 33 + name_len > len {
        return Err(malformed("directory record name"));
    }
    let raw = rec[33..33 + name_len].to_vec();
    let extent = u64::from(u32_le(rec, 2));
    let size = u64::from(u32_le(rec, 10));
    let flags = rec[25];
    let name = if raw == [0] || raw == [1] {
        String::new()
    } else if joliet {
        let units: Vec<u16> = raw
            .chunks_exact(2)
            .map(|c| u16::from_be_bytes([c[0], c[1]]))
            .collect();
        clean_name(&String::from_utf16_lossy(&units))
    } else {
        clean_name(&raw.iter().map(|&b| char::from(b)).collect::<String>())
    };
    let start = extent
        .checked_mul(SECTOR)
        .ok_or_else(|| malformed("extent location"))?;
    Ok(Some((
        raw,
        DirEntry {
            name,
            is_dir: flags & FLAG_DIR != 0,
            file: MediaFile {
                size,
                extents: vec![Extent {
                    start: Some(start),
                    len: size,
                }],
            },
        },
        flags,
    )))
}

/// A file identifier without its `;1` version and a trailing dot.
fn clean_name(name: &str) -> String {
    let name = name.split(';').next().unwrap_or("");
    name.strip_suffix('.').unwrap_or(name).to_string()
}

impl<R: Read + Seek> OpticalImage for IsoImage<R> {
    fn root(&self) -> &DirEntry {
        &self.root
    }

    fn list(&mut self, dir: &DirEntry) -> Result<Vec<DirEntry>, MediaError> {
        if !dir.is_dir {
            return Err(malformed("listing a file"));
        }
        if dir.file.size > MAX_DIR_BYTES {
            return Err(MediaError::TooLarge {
                what: "a directory".into(),
                size: dir.file.size,
                max: MAX_DIR_BYTES,
            });
        }
        let data = self.read_file(&dir.file, MAX_DIR_BYTES)?;
        let mut out: Vec<DirEntry> = Vec::new();
        // A multi-extent file is several records of one name in a row.
        let mut continuing = false;
        let mut at = 0usize;
        while at < data.len() {
            let len = usize::from(data[at]);
            if len == 0 {
                // Records never cross a sector; the rest of this one is
                // padding.
                at = (at / SECTOR as usize + 1) * SECTOR as usize;
                continue;
            }
            let end = at
                .checked_add(len)
                .filter(|&e| e <= data.len())
                .ok_or_else(|| malformed("directory record past its directory"))?;
            let Some((raw, entry, flags)) = parse_record(&data[at..end], self.joliet)? else {
                at = end;
                continue;
            };
            at = end;
            if raw == [0] || raw == [1] {
                continue;
            }
            match out.last_mut() {
                Some(last) if continuing && last.name == entry.name => {
                    last.file.size = last
                        .file
                        .size
                        .checked_add(entry.file.size)
                        .ok_or_else(|| malformed("file size"))?;
                    last.file.extents.extend(entry.file.extents);
                }
                _ => out.push(entry),
            }
            continuing = flags & FLAG_MULTI_EXTENT != 0;
        }
        Ok(out)
    }

    fn read_at(&mut self, file: &MediaFile, offset: u64, buf: &mut [u8]) -> Result<(), MediaError> {
        read_extents(&mut self.r, file, offset, buf)
    }
}

/// Deepest directory nesting ISO 9660 allows (eight levels, root included).
const MAX_BUILD_DEPTH: usize = 7;
/// Joliet's name limit, in UCS-2 units, before the `;1` of a file.
const MAX_JOLIET_NAME: usize = 62;
/// Level-2 file identifier limit, before the `;1`.
const MAX_PRIMARY_NAME: usize = 30;
/// A record's date: 2026-01-01 00:00 UTC. Fixed, so a build is reproducible.
const RECORD_DATE: [u8; 7] = [126, 1, 1, 0, 0, 0, 0];
/// A volume descriptor's date, in the same instant.
const VOLUME_DATE: &[u8; 17] = b"2026010100000000\0";
/// A volume descriptor's "not specified" date.
const NO_DATE: &[u8; 17] = b"0000000000000000\0";

/// A small ISO 9660 + Joliet image under construction.
pub struct IsoBuilder {
    label: String,
    root: BTreeMap<String, Node>,
}

enum Node {
    Dir(BTreeMap<String, Node>),
    File(Vec<u8>),
}

/// A directory being laid out: its place in both trees and its children.
struct DirPlan<'a> {
    parent: usize,
    name: &'a str,
    children: &'a BTreeMap<String, Node>,
    /// Sector and size of the directory in the primary and the Joliet tree.
    primary: (u32, u32),
    joliet: (u32, u32),
}

impl IsoBuilder {
    /// An empty image with volume label `label` (upper-cased, at most 16
    /// characters — Joliet's limit).
    pub fn new(label: &str) -> Self {
        Self {
            label: label.chars().take(16).collect::<String>().to_uppercase(),
            root: BTreeMap::new(),
        }
    }

    /// Puts `data` at `path` (`/`-separated), creating its directories.
    pub fn add_file(&mut self, path: &str, data: Vec<u8>) -> Result<(), MediaError> {
        let bad = |reason| MediaError::BadPath {
            path: path.to_string(),
            reason,
        };
        let parts: Vec<&str> = path.split('/').collect();
        if parts.len() > MAX_BUILD_DEPTH + 1 {
            return Err(bad("nested deeper than ISO 9660 allows"));
        }
        for part in &parts {
            if part.is_empty() || *part == "." || *part == ".." {
                return Err(bad("empty or relative component"));
            }
            if part.encode_utf16().count() > MAX_JOLIET_NAME {
                return Err(bad("a name longer than Joliet allows"));
            }
            if part
                .chars()
                .any(|c| matches!(c, '*' | '/' | ':' | ';' | '?' | '\\') || c.is_control())
            {
                return Err(bad("a character Joliet forbids"));
            }
        }
        let (file, dirs) = parts.split_last().ok_or_else(|| bad("empty path"))?;
        let mut dir = &mut self.root;
        for part in dirs {
            let node = dir
                .entry((*part).to_string())
                .or_insert_with(|| Node::Dir(BTreeMap::new()));
            dir = match node {
                Node::Dir(children) => children,
                Node::File(_) => return Err(bad("a file is in the way")),
            };
        }
        if dir.contains_key(*file) {
            return Err(bad("already on the image"));
        }
        dir.insert((*file).to_string(), Node::File(data));
        Ok(())
    }

    /// The finished image.
    pub fn build(&self) -> Vec<u8> {
        // Directories in breadth-first order, which is the path tables' order
        // (by level, then by parent): the root is number 1, index 0.
        let mut dirs = vec![DirPlan {
            parent: 0,
            name: "",
            children: &self.root,
            primary: (0, 0),
            joliet: (0, 0),
        }];
        let mut next = 0;
        while next < dirs.len() {
            let children = dirs[next].children;
            for (name, node) in children {
                if let Node::Dir(grandchildren) = node {
                    dirs.push(DirPlan {
                        parent: next,
                        name,
                        children: grandchildren,
                        primary: (0, 0),
                        joliet: (0, 0),
                    });
                }
            }
            next += 1;
        }
        let primary_names: Vec<BTreeMap<&str, Vec<u8>>> =
            dirs.iter().map(|d| primary_names(d.children)).collect();
        let joliet_names: Vec<BTreeMap<&str, Vec<u8>>> =
            dirs.iter().map(|d| joliet_names(d.children)).collect();

        // Path tables: little- and big-endian for each tree.
        let path_table_len = |names: &dyn Fn(usize) -> usize| -> usize {
            (0..dirs.len())
                .map(|i| {
                    let n = if i == 0 { 1 } else { names(i) };
                    8 + n + n % 2
                })
                .sum()
        };
        let primary_pt = path_table_len(&|i| primary_names[dirs[i].parent][dirs[i].name].len());
        let joliet_pt = path_table_len(&|i| joliet_names[dirs[i].parent][dirs[i].name].len());
        let sectors = |bytes: usize| (bytes as u64).div_ceil(SECTOR) as u32;
        let mut sector = FIRST_DESCRIPTOR as u32 + 3;
        let mut take = |n: u32| {
            let at = sector;
            sector += n;
            at
        };
        let primary_l = take(sectors(primary_pt));
        let primary_m = take(sectors(primary_pt));
        let joliet_l = take(sectors(joliet_pt));
        let joliet_m = take(sectors(joliet_pt));
        for i in 0..dirs.len() {
            let size = dir_size(&primary_names[i]);
            dirs[i].primary = (take(sectors(size)), size as u32);
        }
        for i in 0..dirs.len() {
            let size = dir_size(&joliet_names[i]);
            dirs[i].joliet = (take(sectors(size)), size as u32);
        }
        // File data, shared by both trees, in directory order.
        let mut files: BTreeMap<*const Vec<u8>, (u32, u32)> = BTreeMap::new();
        for dir in &dirs {
            for node in dir.children.values() {
                if let Node::File(data) = node {
                    let at = if data.is_empty() {
                        0
                    } else {
                        take(sectors(data.len()))
                    };
                    files.insert(data as *const _, (at, data.len() as u32));
                }
            }
        }
        let total = sector;

        let mut image = vec![0u8; total as usize * SECTOR as usize];
        let at = |s: u32| s as usize * SECTOR as usize;
        // The root is the only directory without a name.
        let child_dir = |parent: usize, name: &str| {
            dirs.iter()
                .position(|d| d.parent == parent && d.name == name && !name.is_empty())
                .unwrap_or(0)
        };

        for (tree, (names, l, m, pt_len)) in [
            (&primary_names, primary_l, primary_m, primary_pt),
            (&joliet_names, joliet_l, joliet_m, joliet_pt),
        ]
        .into_iter()
        .enumerate()
        {
            let joliet = tree == 1;
            let place = |d: &DirPlan| if joliet { d.joliet } else { d.primary };
            // Volume descriptor.
            let vd = &mut image[at(FIRST_DESCRIPTOR as u32 + tree as u32)..][..SECTOR as usize];
            vd[0] = if joliet { 2 } else { 1 };
            vd[1..6].copy_from_slice(b"CD001");
            vd[6] = 1;
            put_text(&mut vd[8..40], "", joliet);
            put_text(&mut vd[40..72], &self.label, joliet);
            put_both32(&mut vd[80..88], total);
            if joliet {
                vd[88..91].copy_from_slice(JOLIET_ESCAPES[2]);
            }
            put_both16(&mut vd[120..124], 1);
            put_both16(&mut vd[124..128], 1);
            put_both16(&mut vd[128..132], SECTOR as u16);
            put_both32(&mut vd[132..140], pt_len as u32);
            vd[140..144].copy_from_slice(&l.to_le_bytes());
            vd[148..152].copy_from_slice(&m.to_be_bytes());
            let (root_at, root_size) = place(&dirs[0]);
            write_record(&mut vd[156..190], &[0], root_at, root_size, FLAG_DIR);
            for field in [190..318, 318..446, 446..574, 574..702] {
                put_text(&mut vd[field], "", joliet);
            }
            vd[702..813].fill(b' ');
            vd[813..830].copy_from_slice(VOLUME_DATE);
            vd[830..847].copy_from_slice(VOLUME_DATE);
            vd[847..864].copy_from_slice(NO_DATE);
            vd[864..881].copy_from_slice(NO_DATE);
            vd[881] = 1;

            // Path tables.
            let mut little = Vec::with_capacity(pt_len);
            let mut big = Vec::with_capacity(pt_len);
            for (i, dir) in dirs.iter().enumerate() {
                let name: &[u8] = if i == 0 {
                    &[0]
                } else {
                    &names[dir.parent][dir.name]
                };
                let (sector, _) = place(dir);
                let parent = dir.parent as u16 + 1;
                for (table, le) in [(&mut little, true), (&mut big, false)] {
                    table.push(name.len() as u8);
                    table.push(0);
                    if le {
                        table.extend_from_slice(&sector.to_le_bytes());
                        table.extend_from_slice(&parent.to_le_bytes());
                    } else {
                        table.extend_from_slice(&sector.to_be_bytes());
                        table.extend_from_slice(&parent.to_be_bytes());
                    }
                    table.extend_from_slice(name);
                    if name.len() % 2 == 1 {
                        table.push(0);
                    }
                }
            }
            image[at(l)..][..little.len()].copy_from_slice(&little);
            image[at(m)..][..big.len()].copy_from_slice(&big);

            // Directories.
            for (i, dir) in dirs.iter().enumerate() {
                let (sector, size) = place(dir);
                let parent = place(&dirs[dir.parent]);
                let mut records: Vec<(Vec<u8>, u32, u32, u8)> = vec![
                    (vec![0], sector, size, FLAG_DIR),
                    (vec![1], parent.0, parent.1, FLAG_DIR),
                ];
                let mut children: Vec<(&Vec<u8>, &str, &Node)> = dir
                    .children
                    .iter()
                    .map(|(name, node)| (&names[i][name.as_str()], name.as_str(), node))
                    .collect();
                children.sort_by(|a, b| a.0.cmp(b.0));
                for (id, name, node) in children {
                    match node {
                        Node::Dir(_) => {
                            let (s, n) = place(&dirs[child_dir(i, name)]);
                            records.push((id.clone(), s, n, FLAG_DIR));
                        }
                        Node::File(data) => {
                            let (s, n) = files[&(data as *const _)];
                            records.push((id.clone(), s, n, 0));
                        }
                    }
                }
                let out = &mut image[at(sector)..][..size as usize];
                let mut offset = 0usize;
                for (id, s, n, flags) in records {
                    let len = record_len(&id);
                    if offset % SECTOR as usize + len > SECTOR as usize {
                        offset = offset.next_multiple_of(SECTOR as usize);
                    }
                    write_record(&mut out[offset..offset + len], &id, s, n, flags);
                    offset += len;
                }
            }
        }
        // Terminator.
        let end = &mut image[at(FIRST_DESCRIPTOR as u32 + 2)..][..7];
        end[0] = 255;
        end[1..6].copy_from_slice(b"CD001");
        end[6] = 1;
        // File data.
        for dir in &dirs {
            for node in dir.children.values() {
                if let Node::File(data) = node {
                    let (s, _) = files[&(data as *const _)];
                    if !data.is_empty() {
                        image[at(s)..][..data.len()].copy_from_slice(data);
                    }
                }
            }
        }
        image
    }
}

/// Level-2 identifiers: upper case, `[A-Z0-9_.]`, a file's `;1`, unique in
/// their directory.
fn primary_names(children: &BTreeMap<String, Node>) -> BTreeMap<&str, Vec<u8>> {
    let mut used = std::collections::BTreeSet::new();
    let mut out = BTreeMap::new();
    for (name, node) in children {
        let is_file = matches!(node, Node::File(_));
        let mut base: String = name
            .chars()
            .map(|c| match c.to_ascii_uppercase() {
                c @ ('A'..='Z' | '0'..='9' | '_') => c,
                '.' if is_file => '.',
                _ => '_',
            })
            .collect();
        // One dot at most in a file identifier: keep the last.
        if let Some(dot) = base.rfind('.') {
            let (stem, ext) = base.split_at(dot);
            base = format!("{}{ext}", stem.replace('.', "_"));
        }
        base.truncate(MAX_PRIMARY_NAME);
        let mut candidate = base.clone();
        let mut n = 1;
        while used.contains(&candidate) {
            let suffix = format!("~{n}");
            candidate = format!(
                "{}{suffix}",
                &base[..base.len().min(MAX_PRIMARY_NAME - suffix.len())]
            );
            n += 1;
        }
        used.insert(candidate.clone());
        let mut id = candidate.into_bytes();
        if is_file {
            if !id.contains(&b'.') {
                id.push(b'.');
            }
            id.extend_from_slice(b";1");
        }
        out.insert(name.as_str(), id);
    }
    out
}

/// Joliet identifiers: the name in UCS-2 big-endian, a file's `;1`.
fn joliet_names(children: &BTreeMap<String, Node>) -> BTreeMap<&str, Vec<u8>> {
    children
        .iter()
        .map(|(name, node)| {
            let mut text = name.clone();
            if matches!(node, Node::File(_)) {
                text.push_str(";1");
            }
            let id = text.encode_utf16().flat_map(u16::to_be_bytes).collect();
            (name.as_str(), id)
        })
        .collect()
}

/// A directory record's length for identifier `id`, padded to even.
fn record_len(id: &[u8]) -> usize {
    33 + id.len() + (id.len() + 1) % 2
}

/// The bytes a directory with these child identifiers takes, `.` and `..`
/// included, with no record crossing a sector.
fn dir_size(names: &BTreeMap<&str, Vec<u8>>) -> usize {
    let mut offset = 0usize;
    for len in [34, 34]
        .into_iter()
        .chain(names.values().map(|id| record_len(id)))
    {
        if offset % SECTOR as usize + len > SECTOR as usize {
            offset = offset.next_multiple_of(SECTOR as usize);
        }
        offset += len;
    }
    offset.next_multiple_of(SECTOR as usize)
}

fn write_record(out: &mut [u8], id: &[u8], sector: u32, size: u32, flags: u8) {
    out[0] = record_len(id) as u8;
    put_both32(&mut out[2..10], sector);
    put_both32(&mut out[10..18], size);
    out[18..25].copy_from_slice(&RECORD_DATE);
    out[25] = flags;
    put_both16(&mut out[28..32], 1);
    out[32] = id.len() as u8;
    out[33..33 + id.len()].copy_from_slice(id);
}

fn put_both16(out: &mut [u8], v: u16) {
    out[..2].copy_from_slice(&v.to_le_bytes());
    out[2..4].copy_from_slice(&v.to_be_bytes());
}

fn put_both32(out: &mut [u8], v: u32) {
    out[..4].copy_from_slice(&v.to_le_bytes());
    out[4..8].copy_from_slice(&v.to_be_bytes());
}

/// A space-padded text field, in UCS-2 big-endian on the Joliet descriptor.
fn put_text(out: &mut [u8], text: &str, joliet: bool) {
    if joliet {
        for pair in out.chunks_mut(2) {
            pair.copy_from_slice(&[0, b' '][..pair.len()]);
        }
        for (pair, unit) in out.chunks_exact_mut(2).zip(text.encode_utf16()) {
            pair.copy_from_slice(&unit.to_be_bytes());
        }
    } else {
        out.fill(b' ');
        for (b, c) in out.iter_mut().zip(text.bytes()) {
            *b = c;
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use super::*;
    use crate::optical::walk;

    fn sample() -> IsoBuilder {
        let mut iso = IsoBuilder::new("Entangled");
        iso.add_file("Autounattend.xml", b"<unattend/>".to_vec())
            .unwrap();
        iso.add_file(
            "$WinPEDriver$/viostor/viostor.inf",
            b"[Version]".repeat(500),
        )
        .unwrap();
        iso.add_file("$WinPEDriver$/viostor/viostor.sys", vec![0xaa; 70_000])
            .unwrap();
        iso.add_file("$WinPEDriver$/NetKVM/netkvm.inf", b"net".to_vec())
            .unwrap();
        iso.add_file("empty.txt", Vec::new()).unwrap();
        iso
    }

    #[test]
    fn a_built_image_reads_back_through_joliet_with_its_real_names() {
        let bytes = sample().build();
        assert_eq!(bytes.len() % SECTOR as usize, 0);
        let mut image = IsoImage::new(Cursor::new(bytes)).unwrap();
        assert!(image.is_joliet());
        let root = image.root().clone();
        let files = walk(&mut image, &root).unwrap();
        let names: Vec<&str> = files.iter().map(|(p, _)| p.as_str()).collect();
        assert_eq!(
            names,
            [
                "$WinPEDriver$/NetKVM/netkvm.inf",
                "$WinPEDriver$/viostor/viostor.inf",
                "$WinPEDriver$/viostor/viostor.sys",
                "Autounattend.xml",
                "empty.txt",
            ]
        );
        let sys = image
            .find("$winpedriver$/VIOSTOR/viostor.sys")
            .unwrap()
            .unwrap();
        assert_eq!(
            image.read_file(&sys.file, 1 << 20).unwrap(),
            vec![0xaa; 70_000]
        );
        let xml = image.find("autounattend.xml").unwrap().unwrap();
        assert_eq!(image.read_file(&xml.file, 64).unwrap(), b"<unattend/>");
        assert!(image.find("nope/at/all").unwrap().is_none());
    }

    #[test]
    fn the_primary_tree_carries_upper_case_level_2_names() {
        let bytes = sample().build();
        // Hide the Joliet descriptor: the reader falls back to the primary.
        let mut primary_only = bytes.clone();
        primary_only[17 * SECTOR as usize] = 0;
        let mut image = IsoImage::new(Cursor::new(primary_only)).unwrap();
        assert!(!image.is_joliet());
        let root = image.root().clone();
        let names: Vec<String> = walk(&mut image, &root)
            .unwrap()
            .into_iter()
            .map(|(p, _)| p)
            .collect();
        assert_eq!(
            names,
            [
                "AUTOUNATTEND.XML",
                "EMPTY.TXT",
                "_WINPEDRIVER_/NETKVM/NETKVM.INF",
                "_WINPEDRIVER_/VIOSTOR/VIOSTOR.INF",
                "_WINPEDRIVER_/VIOSTOR/VIOSTOR.SYS",
            ]
        );
    }

    #[test]
    fn many_entries_spill_over_sector_boundaries() {
        let mut iso = IsoBuilder::new("MANY");
        for n in 0..200 {
            iso.add_file(
                &format!("dir/a-rather-long-file-name-{n:04}.dat"),
                vec![n as u8],
            )
            .unwrap();
        }
        let mut image = IsoImage::new(Cursor::new(iso.build())).unwrap();
        let root = image.root().clone();
        let files = walk(&mut image, &root).unwrap();
        assert_eq!(files.len(), 200);
        for (n, (path, entry)) in files.iter().enumerate() {
            assert_eq!(path, &format!("dir/a-rather-long-file-name-{n:04}.dat"));
            assert_eq!(image.read_file(&entry.file, 1).unwrap(), [n as u8]);
        }
    }

    #[test]
    fn bad_paths_are_refused() {
        let mut iso = IsoBuilder::new("X");
        iso.add_file("a/b", vec![1]).unwrap();
        assert!(iso.add_file("a/b", vec![2]).is_err());
        assert!(iso.add_file("a/b/c", vec![2]).is_err());
        assert!(iso.add_file("a//c", vec![2]).is_err());
        assert!(iso.add_file("a/../c", vec![2]).is_err());
        assert!(iso.add_file("a/c;1", vec![2]).is_err());
        assert!(iso.add_file(&"n".repeat(63), vec![2]).is_err());
        assert!(iso.add_file("1/2/3/4/5/6/7/8/9", vec![2]).is_err());
    }

    #[test]
    fn hostile_images_are_errors_not_panics() {
        let good = sample().build();
        // Not an image at all.
        assert!(matches!(
            IsoImage::new(Cursor::new(vec![0u8; 40 * 2048])),
            Err(MediaError::NotAnImage(_))
        ));
        // Truncated after the primary descriptor: it opens, and its root
        // cannot be read.
        let mut cut = IsoImage::new(Cursor::new(good[..17 * 2048].to_vec())).unwrap();
        let root = cut.root().clone();
        assert!(cut.list(&root).is_err());
        // Every byte of the descriptors and directories flipped in turn.
        let dirs_end = 40 * SECTOR as usize;
        for at in (16 * SECTOR as usize..dirs_end.min(good.len())).step_by(7) {
            let mut bad = good.clone();
            bad[at] ^= 0xff;
            if let Ok(mut image) = IsoImage::new(Cursor::new(bad)) {
                let root = image.root().clone();
                if let Ok(files) = walk(&mut image, &root) {
                    for (_, entry) in files {
                        let _ = image.read_file(&entry.file, 1 << 20);
                    }
                }
            }
        }
    }

    #[test]
    fn multi_extent_records_join_into_one_file() {
        // Two records of one name, the first flagged "more extents follow".
        let mut dir = vec![0u8; SECTOR as usize];
        let mut offset = 0;
        for (id, sector, size, flags) in [
            (vec![0u8], 20u32, 2048u32, FLAG_DIR),
            (vec![1], 20, 2048, FLAG_DIR),
            (b"BIG.BIN;1".to_vec(), 21, 2048, FLAG_MULTI_EXTENT),
            (b"BIG.BIN;1".to_vec(), 23, 5, 0),
        ] {
            let len = record_len(&id);
            write_record(&mut dir[offset..offset + len], &id, sector, size, flags);
            offset += len;
        }
        let mut image = vec![0u8; 24 * SECTOR as usize];
        let vd = &mut image[16 * 2048..17 * 2048];
        vd[0] = 1;
        vd[1..6].copy_from_slice(b"CD001");
        put_both16(&mut vd[128..132], 2048);
        write_record(&mut vd[156..190], &[0], 20, 2048, FLAG_DIR);
        image[17 * 2048] = 255;
        image[17 * 2048 + 1..17 * 2048 + 6].copy_from_slice(b"CD001");
        image[20 * 2048..21 * 2048].copy_from_slice(&dir);
        image[21 * 2048..22 * 2048].fill(1);
        image[23 * 2048..23 * 2048 + 5].fill(2);
        let mut iso = IsoImage::new(Cursor::new(image)).unwrap();
        let big = iso.find("big.bin").unwrap().unwrap();
        assert_eq!(big.file.size, 2053);
        let data = iso.read_file(&big.file, 4096).unwrap();
        assert_eq!(&data[..2048], &[1; 2048][..]);
        assert_eq!(&data[2048..], &[2; 5]);
    }
}
