//! The little-endian primitive codec every snapshot section is built from.
//!
//! Deliberately hand-written rather than `serde` on whatever structs happen to
//! exist ([ADR-0005](../../../docs/adr/0005-vm-lifecycle.md) asked for exactly
//! that): a snapshot outlives the build that wrote it, so the field order and
//! the width of every value are part of the format and have to be visible in
//! one place. Deriving them from Rust structs would make an innocuous field
//! reorder a silent format change.
//!
//! # The untrusted-input rules
//!
//! Everything a [`Reader`] returns has been bounds-checked against the bytes
//! actually present, and **no allocation is ever sized by a length that has not
//! been checked first**. That is what [`Reader::count`] exists for: a count is
//! only accepted when `count * minimum_element_size` still fits in the
//! remaining input, so a four-byte file claiming 2^60 entries costs four bytes
//! of work and produces a typed error.

use crate::error::{Result, SnapshotError};

/// Reads primitives out of one section's payload.
pub struct Reader<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    pub fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, pos: 0 }
    }

    pub fn remaining(&self) -> usize {
        self.bytes.len().saturating_sub(self.pos)
    }

    pub fn is_empty(&self) -> bool {
        self.remaining() == 0
    }

    /// Consumes exactly `n` bytes, or reports how far short the input fell.
    pub fn take(&mut self, what: &'static str, n: usize) -> Result<&'a [u8]> {
        let end = self.pos.checked_add(n).ok_or(SnapshotError::Truncated {
            what,
            need: n as u64,
            have: self.remaining() as u64,
        })?;
        let slice = self
            .bytes
            .get(self.pos..end)
            .ok_or(SnapshotError::Truncated {
                what,
                need: n as u64,
                have: self.remaining() as u64,
            })?;
        self.pos = end;
        Ok(slice)
    }

    pub fn u8(&mut self, what: &'static str) -> Result<u8> {
        Ok(self.take(what, 1)?[0])
    }

    pub fn u16(&mut self, what: &'static str) -> Result<u16> {
        let b = self.take(what, 2)?;
        Ok(u16::from_le_bytes([b[0], b[1]]))
    }

    pub fn u32(&mut self, what: &'static str) -> Result<u32> {
        let b = self.take(what, 4)?;
        Ok(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    }

    pub fn u64(&mut self, what: &'static str) -> Result<u64> {
        let b = self.take(what, 8)?;
        Ok(u64::from_le_bytes([
            b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7],
        ]))
    }

    pub fn i64(&mut self, what: &'static str) -> Result<i64> {
        Ok(self.u64(what)? as i64)
    }

    /// A boolean stored as one byte. Anything but 0 or 1 is a refusal rather
    /// than a truthiness test: it means the writer and the reader disagree.
    pub fn bool(&mut self, what: &'static str) -> Result<bool> {
        match self.u8(what)? {
            0 => Ok(false),
            1 => Ok(true),
            other => Err(SnapshotError::BadValue {
                what,
                value: u64::from(other),
            }),
        }
    }

    /// A length-prefixed byte string, refused above `max` **before** anything
    /// is copied.
    pub fn blob(&mut self, what: &'static str, max: usize) -> Result<&'a [u8]> {
        let len = self.u64(what)?;
        if len > max as u64 {
            return Err(SnapshotError::TooLarge {
                what,
                value: len,
                max: max as u64,
            });
        }
        // `len <= max <= usize::MAX`, so the cast cannot truncate.
        self.take(what, len as usize)
    }

    /// A length-prefixed UTF-8 string.
    pub fn string(&mut self, what: &'static str, max: usize) -> Result<String> {
        let bytes = self.blob(what, max)?;
        std::str::from_utf8(bytes)
            .map(str::to_owned)
            .map_err(|_| SnapshotError::NotUtf8 { what })
    }

    /// An element count for a repeated field.
    ///
    /// `min_element` is the smallest number of bytes one element can occupy;
    /// a count whose elements could not possibly fit in what is left is refused
    /// here, before a `Vec::with_capacity` could act on it. `max` is the
    /// format's own bound for this field, so a valid-looking but absurd count
    /// inside a large file is refused too.
    pub fn count(&mut self, what: &'static str, max: usize, min_element: usize) -> Result<usize> {
        let count = self.u64(what)?;
        if count > max as u64 {
            return Err(SnapshotError::TooLarge {
                what,
                value: count,
                max: max as u64,
            });
        }
        let needed = count.saturating_mul(min_element.max(1) as u64);
        if needed > self.remaining() as u64 {
            return Err(SnapshotError::Truncated {
                what,
                need: needed,
                have: self.remaining() as u64,
            });
        }
        Ok(count as usize)
    }

    /// Every byte of the section must have been consumed.
    ///
    /// A section with bytes left over was written by a build that put more in
    /// it than this one knows how to read; carrying on would restore a guest
    /// that is missing whatever those bytes described.
    pub fn finish(self, what: &'static str) -> Result<()> {
        match self.remaining() {
            0 => Ok(()),
            left => Err(SnapshotError::TrailingBytes { what, left }),
        }
    }
}

/// Writes primitives into one section's payload.
#[derive(Default)]
pub struct Writer {
    out: Vec<u8>,
}

impl Writer {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_capacity(bytes: usize) -> Self {
        Self {
            out: Vec::with_capacity(bytes),
        }
    }

    pub fn into_bytes(self) -> Vec<u8> {
        self.out
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.out
    }

    pub fn len(&self) -> usize {
        self.out.len()
    }

    pub fn is_empty(&self) -> bool {
        self.out.is_empty()
    }

    pub fn u8(&mut self, value: u8) -> &mut Self {
        self.out.push(value);
        self
    }

    pub fn u16(&mut self, value: u16) -> &mut Self {
        self.out.extend_from_slice(&value.to_le_bytes());
        self
    }

    pub fn u32(&mut self, value: u32) -> &mut Self {
        self.out.extend_from_slice(&value.to_le_bytes());
        self
    }

    pub fn u64(&mut self, value: u64) -> &mut Self {
        self.out.extend_from_slice(&value.to_le_bytes());
        self
    }

    pub fn i64(&mut self, value: i64) -> &mut Self {
        self.u64(value as u64)
    }

    pub fn bool(&mut self, value: bool) -> &mut Self {
        self.u8(u8::from(value))
    }

    pub fn blob(&mut self, value: &[u8]) -> &mut Self {
        self.u64(value.len() as u64);
        self.out.extend_from_slice(value);
        self
    }

    /// Bytes with no length prefix, for fields whose width the format fixes
    /// (the magic, a 32-byte digest, a page of register file).
    pub fn raw(&mut self, value: &[u8]) -> &mut Self {
        self.out.extend_from_slice(value);
        self
    }

    pub fn string(&mut self, value: &str) -> &mut Self {
        self.blob(value.as_bytes())
    }

    pub fn count(&mut self, value: usize) -> &mut Self {
        self.u64(value as u64)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn primitives_round_trip_in_order() {
        let mut w = Writer::new();
        w.u8(0x12)
            .u16(0x3456)
            .u32(0x789a_bcde)
            .u64(0x0123_4567_89ab_cdef)
            .bool(true)
            .bool(false)
            .string("entangled")
            .blob(&[1, 2, 3]);
        let bytes = w.into_bytes();

        let mut r = Reader::new(&bytes);
        assert_eq!(r.u8("a").unwrap(), 0x12);
        assert_eq!(r.u16("b").unwrap(), 0x3456);
        assert_eq!(r.u32("c").unwrap(), 0x789a_bcde);
        assert_eq!(r.u64("d").unwrap(), 0x0123_4567_89ab_cdef);
        assert!(r.bool("e").unwrap());
        assert!(!r.bool("f").unwrap());
        assert_eq!(r.string("g", 64).unwrap(), "entangled");
        assert_eq!(r.blob("h", 64).unwrap(), &[1, 2, 3]);
        r.finish("all").unwrap();
    }

    /// The rule the whole codec exists for: a huge length is refused by
    /// arithmetic, never by allocating.
    #[test]
    fn an_absurd_length_is_refused_without_allocating() {
        let mut w = Writer::new();
        w.u64(u64::MAX);
        let bytes = w.into_bytes();
        let err = Reader::new(&bytes).blob("payload", 4096).unwrap_err();
        assert!(matches!(err, SnapshotError::TooLarge { .. }), "{err}");

        // And one that is under the format bound but over what is present.
        let mut w = Writer::new();
        w.u64(4000);
        let bytes = w.into_bytes();
        let err = Reader::new(&bytes).blob("payload", 4096).unwrap_err();
        assert!(matches!(err, SnapshotError::Truncated { .. }), "{err}");
    }

    #[test]
    fn a_count_is_checked_against_the_bytes_that_are_left() {
        let mut w = Writer::new();
        w.count(1_000_000).u32(7);
        let bytes = w.into_bytes();
        let err = Reader::new(&bytes)
            .count("entries", 1 << 20, 4)
            .unwrap_err();
        assert!(matches!(err, SnapshotError::Truncated { .. }), "{err}");
    }

    #[test]
    fn a_zero_length_input_reports_truncation_for_every_primitive() {
        for probe in [0usize, 1, 3, 7] {
            let bytes = vec![0u8; probe];
            let err = Reader::new(&bytes).u64("x").unwrap_err();
            assert!(matches!(err, SnapshotError::Truncated { .. }), "{err}");
        }
    }

    #[test]
    fn trailing_bytes_are_a_refusal() {
        let bytes = [1u8, 2, 3];
        let mut r = Reader::new(&bytes);
        assert_eq!(r.u8("x").unwrap(), 1);
        let err = r.finish("section").unwrap_err();
        assert!(
            matches!(err, SnapshotError::TrailingBytes { left: 2, .. }),
            "{err}"
        );
    }

    #[test]
    fn a_non_boolean_byte_is_a_refusal() {
        let bytes = [0x02u8];
        let err = Reader::new(&bytes).bool("flag").unwrap_err();
        assert!(
            matches!(err, SnapshotError::BadValue { value: 2, .. }),
            "{err}"
        );
    }

    #[test]
    fn invalid_utf8_is_a_refusal() {
        let mut w = Writer::new();
        w.blob(&[0xff, 0xfe]);
        let bytes = w.into_bytes();
        let err = Reader::new(&bytes).string("name", 64).unwrap_err();
        assert!(matches!(err, SnapshotError::NotUtf8 { .. }), "{err}");
    }
}
