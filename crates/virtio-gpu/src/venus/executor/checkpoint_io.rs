//! Seekable component framing. Metadata is validated first, then bounded native
//! upload chunks read only the ranges whose identities/geometry were checked.
use super::checkpoint::RecipeError;
use std::io::{Read, Seek, SeekFrom};

pub(super) struct ComponentReader<'a, R> {
    source: &'a mut R,
    start: u64,
    end: u64,
}

impl<'a, R: Read + Seek> ComponentReader<'a, R> {
    pub(super) fn new(source: &'a mut R) -> Result<Self, RecipeError> {
        let start = source.stream_position()?;
        let end = source.seek(SeekFrom::End(0))?;
        if end < start {
            return Err(RecipeError::Invalid(
                "checkpoint component starts past its end",
            ));
        }
        source.seek(SeekFrom::Start(start))?;
        Ok(Self { source, start, end })
    }

    pub(super) fn u32(&mut self) -> Result<u32, RecipeError> {
        let mut bytes = [0; 4];
        self.source.read_exact(&mut bytes)?;
        Ok(u32::from_le_bytes(bytes))
    }

    pub(super) fn u64(&mut self) -> Result<u64, RecipeError> {
        let mut bytes = [0; 8];
        self.source.read_exact(&mut bytes)?;
        Ok(u64::from_le_bytes(bytes))
    }

    pub(super) fn blob_offset(&mut self, len: u64) -> Result<u64, RecipeError> {
        let at = self.source.stream_position()?;
        let next = len
            .checked_next_multiple_of(4)
            .and_then(|padded| at.checked_add(padded))
            .filter(|next| *next <= self.end)
            .ok_or(RecipeError::Invalid(
                "checkpoint payload exceeds its component",
            ))?;
        self.source.seek(SeekFrom::Start(next))?;
        Ok(at)
    }

    pub(super) fn finish_metadata(&mut self) -> Result<(), RecipeError> {
        if self.source.stream_position()? != self.end {
            return Err(RecipeError::Invalid("trailing checkpoint component bytes"));
        }
        Ok(())
    }

    pub(super) fn read_at(&mut self, offset: u64, bytes: &mut [u8]) -> Result<(), RecipeError> {
        if offset < self.start
            || offset
                .checked_add(bytes.len() as u64)
                .is_none_or(|end| end > self.end)
        {
            return Err(RecipeError::Invalid(
                "checkpoint read exceeds its validated component",
            ));
        }
        self.source.seek(SeekFrom::Start(offset))?;
        self.source.read_exact(bytes)?;
        Ok(())
    }

    pub(super) fn finish(&mut self) -> Result<(), RecipeError> {
        self.source.seek(SeekFrom::Start(self.end))?;
        Ok(())
    }
}
