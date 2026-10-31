//! Optical-disc images: reading the files on an installer ISO and writing a
//! small one (ADR-0008 §4).
//!
//! `entangled install windows` needs both halves without a host tool or a
//! mount: it reads the edition list out of the Windows ISO's `install.esd`
//! and the drivers out of the virtio-win ISO, and writes an answer disc with
//! `Autounattend.xml` and `$WinPEDriver$` for Windows Setup to find.
//!
//! * [`iso9660`] — the ISO 9660 primary tree, or its Joliet twin when there
//!   is one, and [`iso9660::IsoBuilder`] for writing an image with Joliet
//!   names.
//! * [`udf`] — UDF, read only. Microsoft's ISOs are UDF bridge discs whose
//!   ISO 9660 side holds nothing but a README, so their files are reachable
//!   through UDF alone.
//!
//! **Everything an image says is untrusted.** An ISO is a file the user
//! downloaded; sizes, extents, record lengths and directory depths are
//! checked before they are used, every read is bounded, and a malformed image
//! is an error, never a panic.

use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use thiserror::Error;

pub mod iso9660;
pub mod udf;

/// The logical sector size of both formats, as every image here uses it.
pub const SECTOR: u64 = 2048;

/// Largest directory read into memory. Installer discs have directories of a
/// few kilobytes.
const MAX_DIR_BYTES: u64 = 4 << 20;

/// Deepest path [`OpticalImage::find`] and [`walk`] follow.
const MAX_DEPTH: usize = 16;

/// Most entries [`walk`] returns, whatever the image claims.
const MAX_WALK_ENTRIES: usize = 100_000;

/// Most directories [`walk`] lists: a directory record pointing back at its
/// ancestor would otherwise make the walk exponential before the depth limit
/// ends it.
const MAX_WALK_DIRS: usize = 10_000;

#[derive(Debug, Error)]
pub enum MediaError {
    #[error("cannot read {path}: {source}")]
    Open {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("I/O error reading the image: {0}")]
    Io(#[from] std::io::Error),

    #[error("malformed image: {0}")]
    Malformed(String),

    #[error("{what} is {size} bytes, more than the {max} this reader accepts")]
    TooLarge { what: String, size: u64, max: u64 },

    #[error("{0} holds neither an ISO 9660 nor a UDF file system")]
    NotAnImage(PathBuf),

    #[error("cannot put {path} on the image: {reason}")]
    BadPath { path: String, reason: &'static str },
}

pub(crate) fn malformed(what: impl Into<String>) -> MediaError {
    MediaError::Malformed(what.into())
}

/// A contiguous run of a file's bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Extent {
    /// Byte offset in the image, or `None` for a run that reads as zeroes
    /// (UDF's allocated-but-unrecorded extents).
    pub start: Option<u64>,
    pub len: u64,
}

/// Where a file's bytes are on the image.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MediaFile {
    /// The file's length.
    pub size: u64,
    pub(crate) extents: Vec<Extent>,
}

/// One name in a directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirEntry {
    pub name: String,
    pub is_dir: bool,
    pub file: MediaFile,
}

/// A file system on an optical-disc image.
pub trait OpticalImage {
    /// The root directory.
    fn root(&self) -> &DirEntry;

    /// The entries of `dir`, without `.` and `..`.
    fn list(&mut self, dir: &DirEntry) -> Result<Vec<DirEntry>, MediaError>;

    /// Fills `buf` from `file` at `offset`; reading past the file's end is an
    /// error.
    fn read_at(&mut self, file: &MediaFile, offset: u64, buf: &mut [u8]) -> Result<(), MediaError>;

    /// The entry at `path` (`/`-separated, ASCII case-insensitive, as both
    /// Windows and these discs treat names), or `None`.
    fn find(&mut self, path: &str) -> Result<Option<DirEntry>, MediaError> {
        let mut entry = self.root().clone();
        for (depth, part) in path.split('/').filter(|p| !p.is_empty()).enumerate() {
            if depth >= MAX_DEPTH || !entry.is_dir {
                return Ok(None);
            }
            match self
                .list(&entry)?
                .into_iter()
                .find(|e| e.name.eq_ignore_ascii_case(part))
            {
                Some(next) => entry = next,
                None => return Ok(None),
            }
        }
        Ok(Some(entry))
    }

    /// The whole of `file`, refused when it is larger than `max`.
    fn read_file(&mut self, file: &MediaFile, max: u64) -> Result<Vec<u8>, MediaError> {
        if file.size > max {
            return Err(MediaError::TooLarge {
                what: "a file".into(),
                size: file.size,
                max,
            });
        }
        let len = usize::try_from(file.size).map_err(|_| malformed("file size"))?;
        let mut buf = vec![0; len];
        self.read_at(file, 0, &mut buf)?;
        Ok(buf)
    }
}

/// Every file under `dir`, with its path relative to `dir` (`/`-separated).
pub fn walk(
    image: &mut dyn OpticalImage,
    dir: &DirEntry,
) -> Result<Vec<(String, DirEntry)>, MediaError> {
    let mut out = Vec::new();
    let mut pending = vec![(String::new(), dir.clone(), 0usize)];
    let mut listed = 0usize;
    while let Some((prefix, dir, depth)) = pending.pop() {
        if depth >= MAX_DEPTH {
            return Err(malformed("directories nested too deeply"));
        }
        listed += 1;
        if listed > MAX_WALK_DIRS {
            return Err(malformed("too many directories"));
        }
        for entry in image.list(&dir)? {
            let path = if prefix.is_empty() {
                entry.name.clone()
            } else {
                format!("{prefix}/{}", entry.name)
            };
            if entry.is_dir {
                pending.push((path, entry, depth + 1));
            } else {
                if out.len() >= MAX_WALK_ENTRIES {
                    return Err(malformed("too many files"));
                }
                out.push((path, entry));
            }
        }
    }
    out.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(out)
}

/// Opens `path` as whichever file system it carries: UDF first, because a
/// bridge disc's ISO 9660 side may be a placeholder (Microsoft's is), then
/// ISO 9660.
pub fn open(path: &Path) -> Result<Box<dyn OpticalImage>, MediaError> {
    let open = || {
        std::fs::File::open(path).map_err(|source| MediaError::Open {
            path: path.to_path_buf(),
            source,
        })
    };
    let mut file = open()?;
    if udf::is_udf(&mut file)? {
        return Ok(Box::new(udf::UdfImage::new(std::io::BufReader::new(file))?));
    }
    match iso9660::IsoImage::new(std::io::BufReader::new(open()?)) {
        Ok(image) => Ok(Box::new(image)),
        Err(MediaError::NotAnImage(_)) => Err(MediaError::NotAnImage(path.to_path_buf())),
        Err(other) => Err(other),
    }
}

/// Reads `buf.len()` bytes of `file` at `offset` from `r`.
pub(crate) fn read_extents<R: Read + Seek>(
    r: &mut R,
    file: &MediaFile,
    mut offset: u64,
    buf: &mut [u8],
) -> Result<(), MediaError> {
    offset
        .checked_add(buf.len() as u64)
        .filter(|&end| end <= file.size)
        .ok_or_else(|| malformed("read past the end of a file"))?;
    let mut done = 0usize;
    for extent in &file.extents {
        if done == buf.len() {
            break;
        }
        if offset >= extent.len {
            offset -= extent.len;
            continue;
        }
        let take = usize::try_from((extent.len - offset).min((buf.len() - done) as u64))
            .map_err(|_| malformed("extent length"))?;
        let out = &mut buf[done..done + take];
        match extent.start {
            Some(start) => {
                let at = start
                    .checked_add(offset)
                    .ok_or_else(|| malformed("extent offset"))?;
                r.seek(SeekFrom::Start(at))?;
                r.read_exact(out)?;
            }
            None => out.fill(0),
        }
        done += take;
        offset = 0;
    }
    if done < buf.len() {
        return Err(malformed("a file is shorter than its recorded size"));
    }
    Ok(())
}

/// Reads one whole sector.
pub(crate) fn read_sector<R: Read + Seek>(
    r: &mut R,
    sector: u64,
) -> Result<[u8; SECTOR as usize], MediaError> {
    let mut buf = [0u8; SECTOR as usize];
    let at = sector
        .checked_mul(SECTOR)
        .ok_or_else(|| malformed("sector number"))?;
    r.seek(SeekFrom::Start(at))?;
    r.read_exact(&mut buf)?;
    Ok(buf)
}

pub(crate) fn u16_le(b: &[u8], at: usize) -> u16 {
    b.get(at..at + 2)
        .map_or(0, |s| u16::from_le_bytes([s[0], s[1]]))
}

pub(crate) fn u32_le(b: &[u8], at: usize) -> u32 {
    b.get(at..at + 4)
        .map_or(0, |s| u32::from_le_bytes([s[0], s[1], s[2], s[3]]))
}

pub(crate) fn u64_le(b: &[u8], at: usize) -> u64 {
    b.get(at..at + 8).map_or(0, |s| {
        u64::from_le_bytes([s[0], s[1], s[2], s[3], s[4], s[5], s[6], s[7]])
    })
}
