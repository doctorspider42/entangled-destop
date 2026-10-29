//! Bounded random access for large device payloads. Verify the section first,
//! then verify each fetched block against that exact pass, including rereads.
//! Rechecking only the whole file before seeking would miss concurrent changes.
use super::*;

const BLOCK: usize = 64 << 10;
const MAX_BYTES: u64 = 64 << 30;

/// A Read+Seek view of one verified section, never its neighbors. Storage is one
/// 64 KiB cache plus 32 bytes per block (at most 32 MiB for a 64 GiB section).
/// Every cache refill verifies the bytes before returning any to the caller.
#[derive(Debug)]
pub struct VerifiedSectionReader<'a, R> {
    source: &'a mut R,
    offset: u64,
    len: u64,
    position: u64,
    hashes: Vec<[u8; 32]>,
    cache: Vec<u8>,
    cached_block: Option<u64>,
    cached_len: usize,
    label: String,
}

impl<R: Read + Seek + std::fmt::Debug> SnapshotReader<R> {
    /// Check the complete section digest before exposing a bounded seekable
    /// view. Per-block hashes keep every subsequent read tied to that pass,
    /// even if the source changes after verification or between seeks.
    pub fn verified_section(
        &mut self,
        kind: SectionKind,
        instance: u32,
    ) -> Result<VerifiedSectionReader<'_, R>> {
        let entry = self
            .entry(kind, instance)
            .ok_or(SnapshotError::MissingSection(kind.as_str()))?;
        if entry.len > MAX_BYTES {
            return Err(SnapshotError::TooLarge {
                what: "seekable section bytes",
                value: entry.len,
                max: MAX_BYTES,
            });
        }
        let blocks = entry.len.div_ceil(BLOCK as u64) as usize;
        let mut hashes = Vec::new();
        hashes
            .try_reserve_exact(blocks)
            .map_err(|_| SnapshotError::Io {
                what: "allocating section block hashes",
                source: std::io::ErrorKind::OutOfMemory.into(),
            })?;
        let mut cache = Vec::new();
        cache
            .try_reserve_exact(BLOCK)
            .map_err(|_| SnapshotError::Io {
                what: "allocating a section block",
                source: std::io::ErrorKind::OutOfMemory.into(),
            })?;
        cache.resize(BLOCK, 0);
        self.source
            .seek(SeekFrom::Start(entry.offset))
            .map_err(SnapshotError::io("seeking to a verified section"))?;
        let mut hasher = Sha256::new();
        let mut left = entry.len;
        while left != 0 {
            let take = left.min(BLOCK as u64) as usize;
            self.source
                .read_exact(&mut cache[..take])
                .map_err(SnapshotError::io("verifying a section block"))?;
            hasher.update(&cache[..take]);
            hashes.push(Sha256::digest(&cache[..take]).into());
            left -= take as u64;
        }
        if hasher.finalize().as_slice() != entry.digest {
            return Err(SnapshotError::Corrupt {
                section: entry.label(),
            });
        }
        Ok(VerifiedSectionReader {
            source: &mut self.source,
            offset: entry.offset,
            len: entry.len,
            position: 0,
            hashes,
            cache,
            cached_block: None,
            cached_len: 0,
            label: entry.label(),
        })
    }
}

impl<R: Read + Seek> VerifiedSectionReader<'_, R> {
    pub fn len(&self) -> u64 {
        self.len
    }
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    fn load_block(&mut self, block: u64) -> std::io::Result<()> {
        if self.cached_block == Some(block) {
            return Ok(());
        }
        let relative = block
            .checked_mul(BLOCK as u64)
            .filter(|relative| *relative < self.len)
            .ok_or_else(|| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "section block is out of bounds",
                )
            })?;
        let offset = self.offset.checked_add(relative).ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::InvalidInput, "section offset overflow")
        })?;
        let take = (self.len - relative).min(BLOCK as u64) as usize;
        self.cached_block = None;
        self.source.seek(SeekFrom::Start(offset))?;
        self.source.read_exact(&mut self.cache[..take])?;
        let digest: [u8; 32] = Sha256::digest(&self.cache[..take]).into();
        if self.hashes.get(block as usize) != Some(&digest) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                SnapshotError::Corrupt {
                    section: self.label.clone(),
                },
            ));
        }
        self.cached_block = Some(block);
        self.cached_len = take;
        Ok(())
    }
}

impl<R: Read + Seek> Read for VerifiedSectionReader<'_, R> {
    fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
        let mut copied = 0;
        while copied < out.len() && self.position < self.len {
            self.load_block(self.position / BLOCK as u64)?;
            let at = (self.position % BLOCK as u64) as usize;
            let available = self
                .cached_len
                .checked_sub(at)
                .filter(|len| *len != 0)
                .ok_or_else(|| {
                    std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        "invalid cached section range",
                    )
                })?;
            let take = available.min(out.len() - copied);
            out[copied..copied + take].copy_from_slice(&self.cache[at..at + take]);
            copied += take;
            self.position += take as u64;
        }
        Ok(copied)
    }
}

impl<R: Read + Seek> Seek for VerifiedSectionReader<'_, R> {
    fn seek(&mut self, from: SeekFrom) -> std::io::Result<u64> {
        let next = match from {
            SeekFrom::Start(at) => Some(at),
            SeekFrom::Current(delta) => self.position.checked_add_signed(delta),
            SeekFrom::End(delta) => self.len.checked_add_signed(delta),
        }
        .filter(|next| *next <= self.len)
        .ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "seek exceeds snapshot section",
            )
        })?;
        self.position = next;
        Ok(next)
    }
}

#[cfg(test)]
mod tests;
