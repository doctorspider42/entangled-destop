//! Forward-only GPU envelope inside a digested VM section. Lengths are computed
//! from quiesced metadata, not by doing a second GPU readback. The VM container
//! supplies integrity; this codec validates framing and limits every part view.
use crate::venus::executor::checkpoint::RecipeError;
use std::collections::HashSet;
use std::io::{Read, Seek, SeekFrom, Write};

const MAGIC: [u8; 8] = *b"ENTGLVNS";
const VERSION: u32 = 1;
const MAX_PARTS: usize = 2 + (1 << 16) + 2 * super::super::MAX_VENUS_CONTEXTS;
pub(super) const MAX_BYTES: u64 = 64 << 30;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u32)]
pub(super) enum Kind {
    Metadata = 1,
    Pages = 2,
    SharedImage = 3,
    Buffers = 4,
    Images = 5,
}

impl Kind {
    fn decode(value: u32) -> Result<Self, RecipeError> {
        match value {
            1 => Ok(Self::Metadata),
            2 => Ok(Self::Pages),
            3 => Ok(Self::SharedImage),
            4 => Ok(Self::Buffers),
            5 => Ok(Self::Images),
            _ => Err(invalid("unknown Venus archive part")),
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub(super) struct Part {
    pub kind: Kind,
    pub id: u32,
    pub len: u64,
}
#[derive(Debug, Clone, Copy)]
pub(super) struct Entry {
    pub part: Part,
    pub offset: u64,
}

fn invalid(why: &'static str) -> RecipeError {
    RecipeError::Invalid(why)
}

impl Part {
    fn validate(self) -> Result<(), RecipeError> {
        let limit = match self.kind {
            Kind::Metadata => 576 << 20,
            Kind::SharedImage => 96 + 4 * crate::MAX_RESOURCE_PIXELS,
            _ => MAX_BYTES,
        };
        if self.len > limit
            || match self.kind {
                Kind::Metadata | Kind::Pages => self.id != 0,
                _ => self.id == 0,
            }
        {
            return Err(invalid("invalid Venus archive part identity or size"));
        }
        Ok(())
    }
}

pub(super) fn write_header(out: &mut impl Write, parts: &[Part]) -> Result<(), RecipeError> {
    if parts.len() < 2 || parts.len() > MAX_PARTS {
        return Err(RecipeError::Budget);
    }
    let mut keys = HashSet::new();
    keys.try_reserve(parts.len())
        .map_err(|_| RecipeError::Budget)?;
    let mut len = 16u64;
    for part in parts {
        part.validate()?;
        if !keys.insert((part.kind, part.id)) {
            return Err(invalid("duplicate Venus archive part"));
        }
        len = part
            .len
            .checked_add(16)
            .and_then(|bytes| len.checked_add(bytes))
            .ok_or(RecipeError::Budget)?;
        if len > MAX_BYTES {
            return Err(RecipeError::Budget);
        }
    }
    if !keys.contains(&(Kind::Metadata, 0)) || !keys.contains(&(Kind::Pages, 0)) {
        return Err(invalid("Venus archive lacks metadata or pages"));
    }
    out.write_all(&MAGIC)?;
    out.write_all(&VERSION.to_le_bytes())?;
    out.write_all(&(parts.len() as u32).to_le_bytes())?;
    Ok(())
}

pub(super) struct LimitedWriter<'a, W> {
    out: &'a mut W,
    remaining: u64,
}
impl<W: Write> Write for LimitedWriter<'_, W> {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if bytes.len() as u64 > self.remaining {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "Venus component exceeded its declared length",
            ));
        }
        let written = self.out.write(bytes)?;
        if written > bytes.len() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "invalid Venus writer count",
            ));
        }
        self.remaining = self.remaining.checked_sub(written as u64).ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "invalid Venus writer count",
            )
        })?;
        Ok(written)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.out.flush()
    }
}

pub(super) fn write_part<W: Write>(
    out: &mut W,
    part: Part,
    save: impl FnOnce(&mut LimitedWriter<'_, W>) -> Result<(), RecipeError>,
) -> Result<(), RecipeError> {
    part.validate()?;
    out.write_all(&(part.kind as u32).to_le_bytes())?;
    out.write_all(&part.id.to_le_bytes())?;
    out.write_all(&part.len.to_le_bytes())?;
    let mut limited = LimitedWriter {
        out,
        remaining: part.len,
    };
    save(&mut limited)?;
    if limited.remaining != 0 {
        return Err(invalid("Venus component did not fill its declared length"));
    }
    Ok(())
}

pub(super) fn read_index(reader: &mut (impl Read + Seek)) -> Result<Vec<Entry>, RecipeError> {
    let start = reader.stream_position()?;
    let end = reader.seek(SeekFrom::End(0))?;
    if end.checked_sub(start).is_none_or(|len| len > MAX_BYTES) {
        return Err(RecipeError::Budget);
    }
    reader.seek(SeekFrom::Start(start))?;
    let mut header = [0; 16];
    reader.read_exact(&mut header)?;
    if header[..8] != MAGIC {
        return Err(invalid("not a Venus checkpoint archive"));
    }
    let mut word = [0; 4];
    word.copy_from_slice(&header[8..12]);
    let version = u32::from_le_bytes(word);
    if version != VERSION {
        return Err(RecipeError::Version(version));
    }
    word.copy_from_slice(&header[12..]);
    let count = u32::from_le_bytes(word) as usize;
    if !(2..=MAX_PARTS).contains(&count) {
        return Err(RecipeError::Budget);
    }
    let mut entries = Vec::new();
    entries
        .try_reserve_exact(count)
        .map_err(|_| RecipeError::Budget)?;
    let mut keys = HashSet::new();
    keys.try_reserve(count).map_err(|_| RecipeError::Budget)?;
    for _ in 0..count {
        reader.read_exact(&mut header)?;
        word.copy_from_slice(&header[..4]);
        let kind = Kind::decode(u32::from_le_bytes(word))?;
        word.copy_from_slice(&header[4..8]);
        let id = u32::from_le_bytes(word);
        let mut length = [0; 8];
        length.copy_from_slice(&header[8..]);
        let part = Part {
            kind,
            id,
            len: u64::from_le_bytes(length),
        };
        part.validate()?;
        if !keys.insert((kind, id)) {
            return Err(invalid("duplicate Venus archive part"));
        }
        let offset = reader.stream_position()?;
        let next = offset
            .checked_add(part.len)
            .filter(|next| *next <= end)
            .ok_or(invalid("Venus part exceeds archive"))?;
        entries.push(Entry { part, offset });
        reader.seek(SeekFrom::Start(next))?;
    }
    if reader.stream_position()? != end {
        return Err(invalid("trailing Venus archive bytes"));
    }
    if !keys.contains(&(Kind::Metadata, 0)) || !keys.contains(&(Kind::Pages, 0)) {
        return Err(invalid("Venus archive lacks metadata or pages"));
    }
    Ok(entries)
}

pub(super) fn entry(entries: &[Entry], kind: Kind, id: u32) -> Result<Entry, RecipeError> {
    entries
        .iter()
        .find(|entry| entry.part.kind == kind && entry.part.id == id)
        .copied()
        .ok_or(invalid("Venus archive part is missing"))
}

pub(super) struct PartReader<'a, R> {
    source: &'a mut R,
    start: u64,
    len: u64,
    position: u64,
}
impl<'a, R> PartReader<'a, R> {
    pub(super) fn new(source: &'a mut R, entry: Entry) -> Self {
        Self {
            source,
            start: entry.offset,
            len: entry.part.len,
            position: 0,
        }
    }
}
impl<R: Read + Seek> Read for PartReader<'_, R> {
    fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
        let take = (self.len - self.position).min(out.len() as u64) as usize;
        if take == 0 {
            return Ok(0);
        }
        let at = self.start.checked_add(self.position).ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "Venus part offset overflow",
            )
        })?;
        self.source.seek(SeekFrom::Start(at))?;
        let read = self.source.read(&mut out[..take])?;
        if read > take {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "invalid Venus reader count",
            ));
        }
        self.position += read as u64;
        Ok(read)
    }
}
impl<R: Read + Seek> Seek for PartReader<'_, R> {
    fn seek(&mut self, from: SeekFrom) -> std::io::Result<u64> {
        let next = match from {
            SeekFrom::Start(at) => Some(at),
            SeekFrom::Current(delta) => self.position.checked_add_signed(delta),
            SeekFrom::End(delta) => self.len.checked_add_signed(delta),
        }
        .filter(|next| *next <= self.len)
        .ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::InvalidInput, "seek exceeds Venus part")
        })?;
        self.position = next;
        Ok(next)
    }
}

pub(super) fn read_bytes(
    reader: &mut (impl Read + Seek),
    entry: Entry,
    max: u64,
) -> Result<Vec<u8>, RecipeError> {
    if entry.part.len > max {
        return Err(RecipeError::Budget);
    }
    let len = usize::try_from(entry.part.len).map_err(|_| RecipeError::Budget)?;
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(len)
        .map_err(|_| RecipeError::Budget)?;
    bytes.resize(len, 0);
    PartReader::new(reader, entry).read_exact(&mut bytes)?;
    Ok(bytes)
}
