//! A guest's OpenGL 1.1 calls, run on the host's OpenGL (ADR-0013).
//!
//! The guest's OpenGL driver does not draw. It writes what the program asked
//! for into a buffer — one record per call — and hands the buffer over; this
//! crate reads the records and makes the same calls on a real OpenGL context
//! of the host's. A context draws into an off-screen framebuffer, and when
//! the program swaps buffers the picture is read back and returned, for the
//! caller to put where the guest's window is.
//!
//! # The stream
//!
//! Little-endian 32-bit words. Each record is a header word, `opcode |
//! words << 16`, and `words` words of arguments; a header whose count is
//! `0xffff` carries the real count in the word after it. Opcodes `0..336`
//! are the OpenGL 1.1 functions in the order of a Windows driver's dispatch
//! table ([`calls::NAMES`]); the ones from [`control::BASE`] up are this
//! protocol's own.
//!
//! # Trust
//!
//! The stream comes from the guest and is hostile. Two rules keep it from
//! reaching host memory:
//!
//! * a function that takes only numbers is forwarded as it is — the host's
//!   OpenGL validates numbers, and reports the bad ones as OpenGL errors;
//! * a function that takes a pointer never sees a guest length. How many
//!   bytes the host's OpenGL will read is computed here ([`sizes`]) from the
//!   same arguments it will be given, the record must carry at least that
//!   many, and where OpenGL writes, it writes into a buffer of this crate's
//!   sized for the worst case. The pixel-store state that would change those
//!   sizes is fixed at context creation and has no opcode.

pub mod api;
pub mod exec;
pub mod sizes;
#[cfg(windows)]
mod thread;
#[cfg(windows)]
mod wgl;

#[cfg(windows)]
pub use thread::{GlThread, StartError};

/// The generated half: names, argument counts and the plain calls.
#[allow(clippy::unreadable_literal, clippy::wildcard_imports)]
#[rustfmt::skip]
pub mod calls {
    include!("calls_gen.rs");
}

/// This protocol's own opcodes.
pub mod control {
    /// Everything from here up is not an OpenGL function.
    pub const BASE: u16 = 0x8000;
    /// `share` → the new context's id (never 0), or 0. `share` is a context
    /// whose display lists and textures the new one shares, or 0.
    pub const CONTEXT_CREATE: u16 = 0x8000;
    /// `id`.
    pub const CONTEXT_DESTROY: u16 = 0x8001;
    /// `id, width, height`: the records after this one are for context `id`,
    /// whose window is `width`×`height` pixels. Every buffer starts with one.
    pub const SELECT: u16 = 0x8002;
    /// `x, y`: show the picture, with its top-left corner at this place on
    /// the guest's screen. Answers one word, so the guest waits for it.
    pub const SWAP: u16 = 0x8003;
    /// `mode, count, arrays…`: `glDrawArrays` with the vertices in the
    /// record. See [`crate::exec`].
    pub const DRAW_ARRAYS: u16 = 0x8004;
    /// `mode, vertices, indices, arrays…, index…`: `glDrawElements`.
    pub const DRAW_ELEMENTS: u16 = 0x8005;
    /// `src, dst, mask`: `wglCopyContext`.
    pub const CONTEXT_COPY: u16 = 0x8006;
}

/// The most contexts all the guest's programs can hold together.
pub const MAX_CONTEXTS: usize = 64;
/// The largest window a context draws for, each way.
pub const MAX_DRAWABLE: u32 = 4096;
/// The most bytes one buffer's answer can be.
pub const MAX_RESPONSE: usize = 1 << 20;

/// A picture to show: `width`×`height` pixels of B, G, R, X, top row first,
/// to land with its corner at (`x`, `y`) on the guest's screen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Present {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
    pub pixels: Vec<u8>,
}

/// What running one buffer produced.
#[derive(Debug, Default)]
pub struct Outcome {
    /// What the guest asked to be told.
    pub response: Vec<u8>,
    /// Pictures to show, in order.
    pub presents: Vec<Present>,
}

/// A record: its opcode and its argument words.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Record<'a> {
    pub opcode: u16,
    pub args: &'a [u32],
}

/// Walks a stream's records. Stops — with an error, once — at the first
/// record that does not fit in the stream.
#[derive(Debug, Clone)]
pub struct Records<'a> {
    words: &'a [u32],
}

/// The stream ended inside a record.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("the command stream ends inside a record")]
pub struct Truncated;

impl<'a> Records<'a> {
    pub fn new(words: &'a [u32]) -> Self {
        Self { words }
    }
}

impl<'a> Iterator for Records<'a> {
    type Item = Result<Record<'a>, Truncated>;

    fn next(&mut self) -> Option<Self::Item> {
        let (&header, rest) = self.words.split_first()?;
        let opcode = (header & 0xffff) as u16;
        let short = (header >> 16) as usize;
        let (count, rest) = if short == 0xffff {
            match rest.split_first() {
                Some((&count, rest)) => (count as usize, rest),
                None => {
                    self.words = &[];
                    return Some(Err(Truncated));
                }
            }
        } else {
            (short, rest)
        };
        if count > rest.len() {
            self.words = &[];
            return Some(Err(Truncated));
        }
        let (args, rest) = rest.split_at(count);
        self.words = rest;
        Some(Ok(Record { opcode, args }))
    }
}

/// The stream's words out of the bytes the guest wrote; a trailing partial
/// word is dropped.
pub fn words_of(bytes: &[u8]) -> Vec<u32> {
    bytes
        .chunks_exact(4)
        .map(|w| u32::from_le_bytes([w[0], w[1], w[2], w[3]]))
        .collect()
}

/// The bytes a run of argument words holds, in stream order.
pub fn bytes_of(words: &[u32]) -> &[u8] {
    // SAFETY: a `u32` slice is `4 * len` initialised bytes with no padding,
    // and `u8` has no alignment to violate. Streams are little-endian and so
    // is every host this builds for (asserted below), so the bytes come out
    // in stream order.
    unsafe { std::slice::from_raw_parts(words.as_ptr().cast::<u8>(), words.len() * 4) }
}

const _: () = assert!(cfg!(target_endian = "little"));

/// The double two argument words hold. The caller has checked the count
/// (generated code: against [`calls::PLAIN_WORDS`]).
pub(crate) fn f64_at(a: &[u32], at: usize) -> f64 {
    f64::from_bits(u64::from(a[at]) | u64::from(a[at + 1]) << 32)
}

/// Builds streams: for tests here, and for anything on the host that wants
/// to speak the protocol.
#[derive(Debug, Default, Clone)]
pub struct Writer {
    words: Vec<u32>,
}

impl Writer {
    pub fn new() -> Self {
        Self::default()
    }

    /// One record.
    pub fn record(&mut self, opcode: u16, args: &[u32]) -> &mut Self {
        if args.len() >= 0xffff {
            self.words.push(u32::from(opcode) | 0xffff << 16);
            self.words
                .push(u32::try_from(args.len()).unwrap_or(u32::MAX));
        } else {
            self.words
                .push(u32::from(opcode) | (args.len() as u32) << 16);
        }
        self.words.extend_from_slice(args);
        self
    }

    pub fn words(&self) -> &[u32] {
        &self.words
    }

    pub fn bytes(&self) -> Vec<u8> {
        bytes_of(&self.words).to_vec()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_come_back_as_written() {
        let mut w = Writer::new();
        w.record(7, &[4])
            .record(1, &[])
            .record(305, &[0, 0, 64, 48]);
        let records: Vec<_> = Records::new(w.words()).collect();
        assert_eq!(
            records,
            vec![
                Ok(Record {
                    opcode: 7,
                    args: &[4]
                }),
                Ok(Record {
                    opcode: 1,
                    args: &[]
                }),
                Ok(Record {
                    opcode: 305,
                    args: &[0, 0, 64, 48]
                }),
            ]
        );
    }

    #[test]
    fn a_long_record_carries_its_count_in_a_word_of_its_own() {
        let payload = vec![0xabcd_ef01; 70_000];
        let mut w = Writer::new();
        w.record(9, &payload).record(1, &[]);
        let mut records = Records::new(w.words());
        let first = records.next().unwrap().unwrap();
        assert_eq!(first.args.len(), 70_000);
        assert_eq!(records.next().unwrap().unwrap().opcode, 1);
        assert!(records.next().is_none());
    }

    #[test]
    fn a_record_longer_than_the_stream_ends_the_walk() {
        // Claims three argument words, has one.
        let words = [7 | 3 << 16, 1];
        let mut records = Records::new(&words);
        assert_eq!(records.next(), Some(Err(Truncated)));
        assert_eq!(records.next(), None);
        // A long header with no count word.
        let words = [7 | 0xffff << 16];
        assert_eq!(Records::new(&words).next(), Some(Err(Truncated)));
        // A long header whose count is absurd.
        let words = [7 | 0xffff << 16, u32::MAX, 0];
        assert_eq!(Records::new(&words).next(), Some(Err(Truncated)));
    }

    #[test]
    fn the_generated_table_is_the_dispatch_table() {
        assert_eq!(calls::NAMES.len(), 336);
        assert_eq!(calls::NAMES[usize::from(calls::op::Begin)], "glBegin");
        assert_eq!(calls::NAMES[335], "glPushClientAttrib");
        // glVertex3f: three words. glGetError answers, so it is not plain.
        assert_eq!(
            calls::PLAIN_WORDS[usize::from(calls::op::Vertex3f)],
            Some(3)
        );
        assert_eq!(
            calls::PLAIN_WORDS[usize::from(calls::op::Vertex3d)],
            Some(6)
        );
        assert_eq!(calls::PLAIN_WORDS[usize::from(calls::op::GetError)], None);
    }

    #[test]
    fn words_and_bytes_are_each_others_inverse() {
        let bytes = [1, 2, 3, 4, 5, 6, 7, 8, 9];
        let words = words_of(&bytes);
        assert_eq!(words, [0x0403_0201, 0x0807_0605]);
        assert_eq!(bytes_of(&words), &bytes[..8]);
    }
}
