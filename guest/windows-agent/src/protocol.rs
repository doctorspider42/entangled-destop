//! The agent protocol: what host and guest say over the named port.
//!
//! A message is a little-endian `u32` payload length, a `u8` kind, then the
//! payload. One kind so far:
//!
//! | Kind | Payload | Direction |
//! |---|---|---|
//! | 1 | the clipboard's text, UTF-8 | both |
//!
//! Host and guest both send the clipboard when it changes and apply what
//! they receive; each remembers what it last applied so it never sends it
//! back. The host's copy of this module is `apps/entangled`'s
//! `clipboard_bridge`, which includes this file.

/// The name of the virtio-console port the agent opens.
pub const PORT_NAME: &str = "org.entangled.agent.0";
/// Kind: the clipboard's text.
pub const KIND_CLIPBOARD_TEXT: u8 = 1;
/// Largest payload either side accepts: 4 MiB of text.
pub const MAX_PAYLOAD: usize = 4 << 20;

/// One message's bytes.
pub fn encode(kind: u8, payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(5 + payload.len());
    out.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    out.push(kind);
    out.extend_from_slice(payload);
    out
}

/// Reassembles messages from a byte stream.
#[derive(Debug, Default)]
pub struct Decoder {
    buffer: Vec<u8>,
}

impl Decoder {
    /// Feeds `bytes`; answers every message they complete. A length over
    /// [`MAX_PAYLOAD`] is a broken peer: the stream is dropped and decoding
    /// starts over.
    pub fn push(&mut self, bytes: &[u8]) -> Vec<(u8, Vec<u8>)> {
        self.buffer.extend_from_slice(bytes);
        let mut out = Vec::new();
        loop {
            if self.buffer.len() < 5 {
                break;
            }
            let len = u32::from_le_bytes([
                self.buffer[0],
                self.buffer[1],
                self.buffer[2],
                self.buffer[3],
            ]) as usize;
            if len > MAX_PAYLOAD {
                self.buffer.clear();
                break;
            }
            if self.buffer.len() < 5 + len {
                break;
            }
            let kind = self.buffer[4];
            let payload = self.buffer[5..5 + len].to_vec();
            self.buffer.drain(..5 + len);
            out.push((kind, payload));
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn messages_survive_any_split() {
        let mut wire = encode(KIND_CLIPBOARD_TEXT, "zażółć".as_bytes());
        wire.extend(encode(KIND_CLIPBOARD_TEXT, b""));
        for split in 0..wire.len() {
            let mut d = Decoder::default();
            let mut got = d.push(&wire[..split]);
            got.extend(d.push(&wire[split..]));
            assert_eq!(got.len(), 2, "split at {split}");
            assert_eq!(got[0].1, "zażółć".as_bytes());
            assert!(got[1].1.is_empty());
        }
    }

    #[test]
    fn an_absurd_length_resets_the_stream() {
        let mut d = Decoder::default();
        assert!(d.push(&[0xff, 0xff, 0xff, 0xff, 1, 2, 3]).is_empty());
        assert_eq!(d.push(&encode(1, b"ok")), vec![(1, b"ok".to_vec())]);
    }
}
