//! Renderer-crash containment: the host 3D renderer in its own process
//! (ADR-0004's GPU-012 amendment).
//!
//! The failure this exists for is concrete: WSLg's mesa D3D12 megadriver
//! dereferences a NULL gallium hook after a few minutes of GNOME compositing
//! and SIGSEGVs *inside* `virgl_renderer_submit_cmd`. In-process that is fatal
//! to the whole VMM — every vCPU, the disk, the network, the guest's unsaved
//! work — and it is not recoverable in place: the only in-process escape is a
//! `siglongjmp` out of a signal handler, which is UB across Rust frames and
//! leaves mesa's own threads holding locks.
//!
//! So the renderer does not share our process. [`RemoteRenderer`] implements
//! [`crate::Renderer3d`] by spawning a helper that owns the real
//! `VirglRenderer` and talking to it over one bidirectional handle installed as
//! the child's stdin — a `socketpair` end on Unix, a duplex named pipe on
//! Windows (VEN-2004), and nothing above the transport knows which; when the
//! helper dies,
//! the client notices, reports [`crate::Renderer3d::is_alive`] `false`, and the
//! device degrades the VM to 2D instead of dying with it.
//!
//! # Layout
//!
//! * [`protocol`] — the wire format. **Portable**: it builds and is tested on
//!   every host OS, because the containment architecture must not be a
//!   Linux-shaped hole in the design (ADR-0002).
//! * [`frame`] / [`read_frame`] — the length-prefixed framing, portable and
//!   tested over an in-memory stream.
//! * `client` — [`RemoteRenderer`]: spawn, supervise, forward.
//! * `server` — [`serve`]: the helper's loop, running in the child.
//! * `pipe_windows` — the Windows channel (VEN-2004): a duplex named pipe
//!   where Unix uses a `socketpair`, installed as the child's stdin either
//!   way, so nothing above the transport is host-specific.
//!
//! # Guest memory never crosses the boundary
//!
//! The helper is handed *bytes*, never guest physical addresses: the client
//! reads guest pages through the checked `vm-memory` API and sends the span,
//! and the helper keeps a host-side shadow of each resource's backing. The
//! process that runs guest-derived GL work therefore has no window onto guest
//! RAM — the security half of the containment story — and the design needs no
//! shared guest memory, so `vmm_core::create_guest_memory` and both
//! hypervisor backends stay untouched.

use std::io::{Read, Write};

pub mod protocol;

#[cfg(any(unix, windows))]
mod client;
#[cfg(windows)]
mod pipe_windows;
#[cfg(any(unix, windows))]
mod server;

#[cfg(any(unix, windows))]
pub use client::{RemoteRenderer, SpawnError};
#[cfg(any(unix, windows))]
pub use server::{serve, serve_stdin};

pub use protocol::{CodecError, Reply, Request, MAX_FRAME_BYTES, REMOTE_XFER_WINDOW, VERSION};

/// Why a message could not be exchanged.
#[derive(Debug)]
pub enum WireError {
    /// The peer is gone (clean EOF or a broken pipe) — for the client, this is
    /// what "the renderer crashed" looks like.
    Closed,
    Io(std::io::Error),
    Codec(CodecError),
}

impl std::fmt::Display for WireError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Closed => write!(f, "the renderer process closed the connection"),
            Self::Io(error) => write!(f, "renderer socket: {error}"),
            Self::Codec(error) => write!(f, "renderer protocol: {error}"),
        }
    }
}

impl std::error::Error for WireError {}

impl From<std::io::Error> for WireError {
    fn from(error: std::io::Error) -> Self {
        match error.kind() {
            std::io::ErrorKind::UnexpectedEof
            | std::io::ErrorKind::BrokenPipe
            | std::io::ErrorKind::ConnectionReset => Self::Closed,
            _ => Self::Io(error),
        }
    }
}

impl From<CodecError> for WireError {
    fn from(error: CodecError) -> Self {
        Self::Codec(error)
    }
}

/// Frames one message: `[tag][u32 payload_len][payload]`, little-endian.
///
/// One `write_all` per message on purpose: a partial write followed by a peer
/// death would desynchronize the stream, and there is no resynchronization in
/// a protocol whose whole failure mode is "give up and degrade".
pub fn frame(tag: u8, payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(5 + payload.len());
    out.push(tag);
    out.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    out.extend_from_slice(payload);
    out
}

/// Reads one frame, reusing `buf` for the payload.
///
/// The length is checked against [`MAX_FRAME_BYTES`] **before** anything is
/// reserved: the peer is untrusted (the helper runs guest-derived GL work, and
/// a corrupted or hostile helper must not be able to make the VMM allocate
/// without limit).
pub fn read_frame<R: Read>(reader: &mut R, buf: &mut Vec<u8>) -> Result<u8, WireError> {
    let (tag, len) = read_frame_header(reader)?;
    read_payload(reader, buf, len)?;
    Ok(tag)
}

/// Reads a frame's five-byte header: the tag and the payload length, the
/// latter already checked against [`MAX_FRAME_BYTES`].
///
/// Split out of [`read_frame`] for the one message big enough to care where
/// its bytes land: a full-screen scanout readback (7.9 MiB at 1080p, once per
/// present).
pub fn read_frame_header<R: Read>(reader: &mut R) -> Result<(u8, usize), WireError> {
    let mut header = [0u8; 5];
    reader.read_exact(&mut header)?;
    let len = u32::from_le_bytes([header[1], header[2], header[3], header[4]]) as usize;
    if len > MAX_FRAME_BYTES {
        return Err(WireError::Codec(CodecError::TooLarge(len)));
    }
    Ok((header[0], len))
}

/// Reads `len` payload bytes into `buf`.
///
/// Reuses what `buf` already holds rather than clearing it first: a caller
/// that reads same-sized frames in a loop — the scanout path reads one 1080p
/// rect per guest present — then pays no `memset` and no reallocation at all.
/// The peer is untrusted, so the growth goes through `try_reserve`.
pub fn read_payload<R: Read>(
    reader: &mut R,
    buf: &mut Vec<u8>,
    len: usize,
) -> Result<(), WireError> {
    if len > MAX_FRAME_BYTES {
        return Err(WireError::Codec(CodecError::TooLarge(len)));
    }
    if buf.len() < len {
        buf.try_reserve(len - buf.len())
            .map_err(|_| WireError::Codec(CodecError::TooLarge(len)))?;
        buf.resize(len, 0);
    } else {
        buf.truncate(len);
    }
    reader.read_exact(buf)?;
    Ok(())
}

/// Writes one framed message.
pub fn write_frame<W: Write>(writer: &mut W, tag: u8, payload: &[u8]) -> Result<(), WireError> {
    writer.write_all(&frame(tag, payload))?;
    writer.flush()?;
    Ok(())
}

/// Writes a [`Reply::Bytes`] frame straight out of `payload`.
///
/// Identical on the wire to `write_frame(w, tag::BYTES, &Reply::Bytes(v).encode())`
/// and exists only to not build that `encode()` copy: the scanout readback is
/// the one reply whose payload is megabytes, and it is sent on every guest
/// present. Two `write_all`s rather than one is safe here — a peer that dies
/// between them ends the session either way, and the protocol has no
/// resynchronization to lose.
pub fn write_bytes_reply<W: Write>(writer: &mut W, payload: &[u8]) -> Result<(), WireError> {
    // The frame carries `Reply::Bytes`'s own u32 length prefix as well as the
    // frame length, which is what `Reply::encode` would have written.
    let inner = u32::try_from(payload.len()).map_err(|_| CodecError::TooLarge(payload.len()))?;
    let framed = payload.len() + 4;
    if framed > MAX_FRAME_BYTES {
        return Err(WireError::Codec(CodecError::TooLarge(framed)));
    }
    let mut prelude = [0u8; 9];
    prelude[0] = protocol::tag::BYTES;
    prelude[1..5].copy_from_slice(&(framed as u32).to_le_bytes());
    prelude[5..9].copy_from_slice(&inner.to_le_bytes());
    writer.write_all(&prelude)?;
    writer.write_all(payload)?;
    writer.flush()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The bulk scanout path must be byte-identical on the wire to the
    /// ordinary one: it is an allocation shortcut, not a second format.
    #[test]
    fn the_bulk_pixel_reply_is_the_same_frame_as_the_ordinary_one() {
        for len in [0usize, 1, 5, 4096, 1920 * 4] {
            let pixels: Vec<u8> = (0..len).map(|i| i as u8).collect();
            let ordinary = frame(
                Reply::Bytes(pixels.clone()).tag(),
                &Reply::Bytes(pixels.clone()).encode(),
            );
            let mut bulk = Vec::new();
            write_bytes_reply(&mut bulk, &pixels).expect("in-memory write");
            assert_eq!(bulk, ordinary, "{len} bytes");

            // …and reads back through the split header/payload pair.
            let mut cursor = std::io::Cursor::new(bulk);
            let (tag, frame_len) = read_frame_header(&mut cursor).expect("header");
            assert_eq!(tag, protocol::tag::BYTES);
            assert_eq!(frame_len, len + 4);
            let mut buf = Vec::new();
            read_payload(&mut cursor, &mut buf, frame_len).expect("payload");
            assert_eq!(Reply::decode(tag, &buf), Ok(Reply::Bytes(pixels)));
        }
    }

    /// `read_payload` reuses the buffer it is given — the property the
    /// scanout path depends on, and the one a `clear()` would quietly undo.
    #[test]
    fn read_payload_keeps_its_buffers_allocation() {
        let mut buf = Vec::new();
        let mut cursor = std::io::Cursor::new(vec![7u8; 64]);
        read_payload(&mut cursor, &mut buf, 64).expect("first read");
        let capacity = buf.capacity();
        let ptr = buf.as_ptr();
        assert_eq!(buf, vec![7u8; 64]);

        let mut cursor = std::io::Cursor::new(vec![9u8; 64]);
        read_payload(&mut cursor, &mut buf, 64).expect("second read");
        assert_eq!(buf, vec![9u8; 64]);
        assert_eq!(buf.capacity(), capacity, "no reallocation");
        assert!(std::ptr::eq(ptr, buf.as_ptr()), "same allocation");

        // A shorter frame truncates rather than reallocating.
        let mut cursor = std::io::Cursor::new(vec![1u8; 8]);
        read_payload(&mut cursor, &mut buf, 8).expect("short read");
        assert_eq!(buf.len(), 8);
        assert_eq!(buf.capacity(), capacity);
    }

    /// Framing round-trips over any stream, including several messages
    /// back-to-back in one buffer (which is what a socket delivers).
    #[test]
    fn frames_round_trip_back_to_back() {
        let messages = [
            Request::PollFences,
            Request::Submit {
                ctx_id: 1,
                stream: vec![7; 4096],
            },
            Request::Reset,
        ];
        let mut stream = Vec::new();
        for message in &messages {
            stream.extend_from_slice(&frame(message.tag(), &message.encode()));
        }

        let mut cursor = std::io::Cursor::new(stream);
        let mut buf = Vec::new();
        for expected in &messages {
            let tag = read_frame(&mut cursor, &mut buf).expect("frame");
            let decoded = Request::decode(tag, &buf).expect("decode");
            assert_eq!(&decoded, expected);
        }
        // The stream is exhausted: the next read is a clean close.
        assert!(matches!(
            read_frame(&mut cursor, &mut buf),
            Err(WireError::Closed)
        ));
    }

    /// A length header past the cap is refused *without* reserving for it.
    #[test]
    fn an_oversized_length_header_is_refused_before_allocating() {
        let mut evil = vec![protocol::tag::SUBMIT];
        evil.extend_from_slice(&u32::MAX.to_le_bytes());
        let mut cursor = std::io::Cursor::new(evil);
        let mut buf = Vec::new();
        match read_frame(&mut cursor, &mut buf) {
            Err(WireError::Codec(CodecError::TooLarge(len))) => assert_eq!(len, u32::MAX as usize),
            other => panic!("expected a size refusal, got {other:?}"),
        }
        assert!(buf.capacity() < MAX_FRAME_BYTES);
    }

    /// A truncated header mid-frame is a closed peer, not a panic.
    #[test]
    fn a_truncated_stream_reports_a_closed_peer() {
        let mut cursor = std::io::Cursor::new(vec![protocol::tag::RESET, 0, 0]);
        let mut buf = Vec::new();
        assert!(matches!(
            read_frame(&mut cursor, &mut buf),
            Err(WireError::Closed)
        ));
    }
}
