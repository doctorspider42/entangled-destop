//! 9P2000.L on the wire: little-endian integers, `u16`-length strings, and a
//! seven-byte header (`size[4] type[1] tag[2]`) in front of every message.
//!
//! **Everything decoded here is guest data.** The reader never indexes past
//! its buffer (every take is checked and fails the message), strings must be
//! UTF-8 (the host's file APIs want it on Windows, and a name that is not is
//! refused rather than mangled), and lengths are bounded by the buffer they
//! claim to be in before anything is allocated for them.

/// Message type numbers (Linux `include/net/9p/9p.h`).
pub mod t {
    pub const RLERROR: u8 = 7;
    pub const TSTATFS: u8 = 8;
    pub const TLOPEN: u8 = 12;
    pub const TLCREATE: u8 = 14;
    pub const TSYMLINK: u8 = 16;
    pub const TMKNOD: u8 = 18;
    pub const TRENAME: u8 = 20;
    pub const TREADLINK: u8 = 22;
    pub const TGETATTR: u8 = 24;
    pub const TSETATTR: u8 = 26;
    pub const TXATTRWALK: u8 = 30;
    pub const TXATTRCREATE: u8 = 32;
    pub const TREADDIR: u8 = 40;
    pub const TFSYNC: u8 = 50;
    pub const TLOCK: u8 = 52;
    pub const TGETLOCK: u8 = 54;
    pub const TLINK: u8 = 70;
    pub const TMKDIR: u8 = 72;
    pub const TRENAMEAT: u8 = 74;
    pub const TUNLINKAT: u8 = 76;
    pub const TVERSION: u8 = 100;
    pub const TAUTH: u8 = 102;
    pub const TATTACH: u8 = 104;
    pub const TFLUSH: u8 = 108;
    pub const TWALK: u8 = 110;
    pub const TREAD: u8 = 116;
    pub const TWRITE: u8 = 118;
    pub const TCLUNK: u8 = 120;
    pub const TREMOVE: u8 = 122;
}

/// Size of the message header.
pub const HEADER: usize = 7;
/// Size of a qid on the wire.
pub const QID_LEN: usize = 13;

/// Qid types.
pub const QTDIR: u8 = 0x80;
pub const QTSYMLINK: u8 = 0x02;
pub const QTFILE: u8 = 0x00;

/// A server's identity for a file: type, version, and a path unique per file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Qid {
    pub kind: u8,
    pub version: u32,
    pub path: u64,
}

/// Why a message could not be decoded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WireError {
    /// The message ends before a field does.
    Short,
    /// A string is not UTF-8.
    NotUtf8,
}

/// A bounds-checked reader over one message body.
pub struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    pub fn new(buf: &'a [u8]) -> Self {
        Self { buf, pos: 0 }
    }

    fn take(&mut self, n: usize) -> Result<&'a [u8], WireError> {
        let end = self.pos.checked_add(n).ok_or(WireError::Short)?;
        let slice = self.buf.get(self.pos..end).ok_or(WireError::Short)?;
        self.pos = end;
        Ok(slice)
    }

    pub fn u8(&mut self) -> Result<u8, WireError> {
        Ok(self.take(1)?[0])
    }

    pub fn u16(&mut self) -> Result<u16, WireError> {
        let b = self.take(2)?;
        Ok(u16::from_le_bytes([b[0], b[1]]))
    }

    pub fn u32(&mut self) -> Result<u32, WireError> {
        let b = self.take(4)?;
        Ok(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    }

    pub fn u64(&mut self) -> Result<u64, WireError> {
        let b = self.take(8)?;
        let mut a = [0u8; 8];
        a.copy_from_slice(b);
        Ok(u64::from_le_bytes(a))
    }

    pub fn string(&mut self) -> Result<&'a str, WireError> {
        let len = usize::from(self.u16()?);
        std::str::from_utf8(self.take(len)?).map_err(|_| WireError::NotUtf8)
    }

    /// `n` raw bytes (a write's payload).
    pub fn bytes(&mut self, n: usize) -> Result<&'a [u8], WireError> {
        self.take(n)
    }
}

/// A reply under construction; the header is patched in by [`Writer::finish`].
pub struct Writer {
    buf: Vec<u8>,
}

impl Writer {
    pub fn new(kind: u8, tag: u16) -> Self {
        let mut buf = Vec::with_capacity(64);
        buf.extend_from_slice(&[0, 0, 0, 0, kind]);
        buf.extend_from_slice(&tag.to_le_bytes());
        Self { buf }
    }

    pub fn u8(&mut self, v: u8) -> &mut Self {
        self.buf.push(v);
        self
    }

    pub fn u16(&mut self, v: u16) -> &mut Self {
        self.buf.extend_from_slice(&v.to_le_bytes());
        self
    }

    pub fn u32(&mut self, v: u32) -> &mut Self {
        self.buf.extend_from_slice(&v.to_le_bytes());
        self
    }

    pub fn u64(&mut self, v: u64) -> &mut Self {
        self.buf.extend_from_slice(&v.to_le_bytes());
        self
    }

    /// A string, truncated at `u16::MAX` bytes on a character boundary.
    pub fn string(&mut self, s: &str) -> &mut Self {
        let mut end = s.len().min(usize::from(u16::MAX));
        while !s.is_char_boundary(end) {
            end -= 1;
        }
        self.u16(end as u16);
        self.buf.extend_from_slice(&s.as_bytes()[..end]);
        self
    }

    pub fn qid(&mut self, q: Qid) -> &mut Self {
        self.u8(q.kind).u32(q.version).u64(q.path)
    }

    pub fn bytes(&mut self, b: &[u8]) -> &mut Self {
        self.buf.extend_from_slice(b);
        self
    }

    /// Bytes written so far, header included.
    pub fn len(&self) -> usize {
        self.buf.len()
    }

    pub fn is_empty(&self) -> bool {
        self.buf.is_empty()
    }

    /// The finished message, its length patched into the header. Leaves the
    /// writer empty.
    pub fn finish(&mut self) -> Vec<u8> {
        let mut buf = std::mem::take(&mut self.buf);
        let len = u32::try_from(buf.len()).unwrap_or(u32::MAX);
        if let Some(size) = buf.get_mut(..4) {
            size.copy_from_slice(&len.to_le_bytes());
        }
        buf
    }
}

/// `Rlerror` carrying a Linux errno.
pub fn lerror(tag: u16, errno: u32) -> Vec<u8> {
    let mut w = Writer::new(t::RLERROR, tag);
    w.u32(errno);
    w.finish()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_reader_never_reads_past_its_buffer() {
        let mut r = Reader::new(&[1, 0, 2]);
        assert_eq!(r.u16(), Ok(1));
        assert_eq!(r.u16(), Err(WireError::Short));
        // A string that claims more bytes than there are.
        let mut r = Reader::new(&[0xff, 0xff, b'a']);
        assert_eq!(r.string(), Err(WireError::Short));
        let mut r = Reader::new(&[2, 0, 0xff, 0xfe]);
        assert_eq!(r.string(), Err(WireError::NotUtf8));
    }

    #[test]
    fn a_reply_carries_its_own_length() {
        let mut w = Writer::new(101, 0xffff);
        w.u32(8192).string("9P2000.L");
        let out = w.finish();
        assert_eq!(
            u32::from_le_bytes([out[0], out[1], out[2], out[3]]) as usize,
            out.len()
        );
        assert_eq!(out[4], 101);
        assert_eq!(&out[5..7], &[0xff, 0xff]);
        let mut r = Reader::new(&out[HEADER..]);
        assert_eq!(r.u32(), Ok(8192));
        assert_eq!(r.string(), Ok("9P2000.L"));
    }
}
