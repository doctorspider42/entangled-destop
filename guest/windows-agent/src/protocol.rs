//! The agent protocol: what host and guest say over the named port.
//!
//! A message is a little-endian `u32` payload length, a `u8` kind, then the
//! payload:
//!
//! | Kind | Payload | Direction |
//! |---|---|---|
//! | 1 | the clipboard's text, UTF-8 | both |
//! | 2 | [`ExecRequest`]: run a command line | host to guest |
//! | 3 | [`ExecResult`]: its exit code and captured output | guest to host |
//!
//! Host and guest both send the clipboard when it changes and apply what
//! they receive; each remembers what it last applied so it never sends it
//! back. An exec request is answered by exactly one result with the same id
//! (an agent that predates kind 2 ignores it, and the host gives up after
//! the timeout). The host's copy of this module is `apps/entangled`'s
//! `clipboard_bridge`, which includes this file.
//!
//! Each side treats the other as untrusted: every length is checked against
//! the bytes present and against the limits below, and a payload that does
//! not parse is dropped, never acted on in part.

/// The name of the virtio-console port the agent opens.
pub const PORT_NAME: &str = "org.entangled.agent.0";
/// Kind: the clipboard's text.
pub const KIND_CLIPBOARD_TEXT: u8 = 1;
/// Largest payload either side accepts: 4 MiB of text.
pub const MAX_PAYLOAD: usize = 4 << 20;
/// Kind: run a command line in the guest ([`ExecRequest`]).
pub const KIND_EXEC: u8 = 2;
/// Kind: what a command did ([`ExecResult`]).
pub const KIND_EXEC_RESULT: u8 = 3;
/// The longest command line, in bytes: `cmd.exe`'s own limit is 8191
/// characters.
pub const MAX_EXEC_COMMAND: usize = 8 * 1024;
/// The most of each of stdout and stderr a result carries; the rest is
/// read and dropped (so the command never blocks on a full pipe) and the
/// result says it was truncated.
pub const MAX_EXEC_OUTPUT: usize = 64 * 1024;
/// The longest a command may run before the agent kills it.
pub const MAX_EXEC_TIMEOUT_MS: u32 = 600_000;

/// Host to guest: run `command` with `cmd.exe /d /s /c`, at most
/// `timeout_ms`. Payload: id `u32`, timeout `u32`, the command line (UTF-8).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecRequest {
    pub id: u32,
    pub timeout_ms: u32,
    pub command: String,
}

impl ExecRequest {
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(8 + self.command.len());
        out.extend_from_slice(&self.id.to_le_bytes());
        out.extend_from_slice(&self.timeout_ms.to_le_bytes());
        out.extend_from_slice(self.command.as_bytes());
        out
    }

    /// `None` for a payload that is short, too long, not UTF-8, empty, or
    /// asks for no time or more than [`MAX_EXEC_TIMEOUT_MS`].
    pub fn decode(payload: &[u8]) -> Option<Self> {
        let id = u32::from_le_bytes(payload.get(0..4)?.try_into().ok()?);
        let timeout_ms = u32::from_le_bytes(payload.get(4..8)?.try_into().ok()?);
        let command = payload.get(8..)?;
        if command.is_empty() || command.len() > MAX_EXEC_COMMAND {
            return None;
        }
        if timeout_ms == 0 || timeout_ms > MAX_EXEC_TIMEOUT_MS {
            return None;
        }
        Some(Self {
            id,
            timeout_ms,
            command: String::from_utf8(command.to_vec()).ok()?,
        })
    }
}

/// How a command ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExecStatus {
    /// It exited with this code.
    Exited(i32),
    /// It ran past its timeout and was killed.
    TimedOut,
    /// It never started; the reason is in `stderr`.
    Failed,
    /// The agent already runs as many commands as it will.
    Busy,
}

/// Guest to host: one command's outcome. Payload: id `u32`, status `u8`
/// (0 exited, 1 timed out, 2 failed, 3 busy), flags `u8` (bit 0 stdout
/// truncated, bit 1 stderr truncated), exit code `i32`, then stdout and
/// stderr, each a `u32` length and that many bytes (UTF-8 where the agent
/// could make it so).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecResult {
    pub id: u32,
    pub status: ExecStatus,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub stdout_truncated: bool,
    pub stderr_truncated: bool,
}

impl ExecResult {
    /// Encodes, cutting each stream to [`MAX_EXEC_OUTPUT`] (and saying so).
    pub fn encode(&self) -> Vec<u8> {
        let (status, code) = match self.status {
            ExecStatus::Exited(code) => (0u8, code),
            ExecStatus::TimedOut => (1, 0),
            ExecStatus::Failed => (2, 0),
            ExecStatus::Busy => (3, 0),
        };
        let stdout = &self.stdout[..self.stdout.len().min(MAX_EXEC_OUTPUT)];
        let stderr = &self.stderr[..self.stderr.len().min(MAX_EXEC_OUTPUT)];
        let flags = u8::from(self.stdout_truncated || stdout.len() < self.stdout.len())
            | (u8::from(self.stderr_truncated || stderr.len() < self.stderr.len()) << 1);
        let mut out = Vec::with_capacity(18 + stdout.len() + stderr.len());
        out.extend_from_slice(&self.id.to_le_bytes());
        out.push(status);
        out.push(flags);
        out.extend_from_slice(&code.to_le_bytes());
        for stream in [stdout, stderr] {
            out.extend_from_slice(&(stream.len() as u32).to_le_bytes());
            out.extend_from_slice(stream);
        }
        out
    }

    /// `None` for anything malformed: a short payload, an unknown status,
    /// a stream longer than [`MAX_EXEC_OUTPUT`] or than the bytes present,
    /// or bytes left over.
    pub fn decode(payload: &[u8]) -> Option<Self> {
        let id = u32::from_le_bytes(payload.get(0..4)?.try_into().ok()?);
        let status = *payload.get(4)?;
        let flags = *payload.get(5)?;
        let code = i32::from_le_bytes(payload.get(6..10)?.try_into().ok()?);
        let status = match status {
            0 => ExecStatus::Exited(code),
            1 => ExecStatus::TimedOut,
            2 => ExecStatus::Failed,
            3 => ExecStatus::Busy,
            _ => return None,
        };
        let mut at = 10usize;
        let mut stream = || -> Option<Vec<u8>> {
            let start = at.checked_add(4)?;
            let len = u32::from_le_bytes(payload.get(at..start)?.try_into().ok()?);
            let len = usize::try_from(len).ok()?;
            if len > MAX_EXEC_OUTPUT {
                return None;
            }
            let end = start.checked_add(len)?;
            let bytes = payload.get(start..end)?.to_vec();
            at = end;
            Some(bytes)
        };
        let stdout = stream()?;
        let stderr = stream()?;
        if at != payload.len() {
            return None;
        }
        Some(Self {
            id,
            status,
            stdout,
            stderr,
            stdout_truncated: flags & 1 != 0,
            stderr_truncated: flags & 2 != 0,
        })
    }
}

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
    fn exec_messages_round_trip() {
        let request = ExecRequest {
            id: 7,
            timeout_ms: 5_000,
            command: r"dir Z:\ & ver".into(),
        };
        assert_eq!(ExecRequest::decode(&request.encode()), Some(request));
        for status in [
            ExecStatus::Exited(-1073741515),
            ExecStatus::TimedOut,
            ExecStatus::Failed,
            ExecStatus::Busy,
        ] {
            let result = ExecResult {
                id: 9,
                status,
                stdout: b"a\r\nb".to_vec(),
                stderr: Vec::new(),
                stdout_truncated: false,
                stderr_truncated: true,
            };
            assert_eq!(ExecResult::decode(&result.encode()), Some(result));
        }
    }

    #[test]
    fn an_oversized_stream_is_cut_and_marked_truncated() {
        let result = ExecResult {
            id: 1,
            status: ExecStatus::Exited(0),
            stdout: vec![b'x'; MAX_EXEC_OUTPUT + 10],
            stderr: Vec::new(),
            stdout_truncated: false,
            stderr_truncated: false,
        };
        let back = ExecResult::decode(&result.encode()).unwrap();
        assert_eq!(back.stdout.len(), MAX_EXEC_OUTPUT);
        assert!(back.stdout_truncated);
        assert!(!back.stderr_truncated);
    }

    /// The guest is untrusted: every malformed result is dropped whole.
    #[test]
    fn malformed_exec_payloads_are_refused() {
        let good = ExecResult {
            id: 1,
            status: ExecStatus::Exited(3),
            stdout: b"out".to_vec(),
            stderr: b"err".to_vec(),
            stdout_truncated: false,
            stderr_truncated: false,
        }
        .encode();
        for cut in 0..good.len() {
            assert_eq!(ExecResult::decode(&good[..cut]), None, "cut at {cut}");
        }
        let mut extra = good.clone();
        extra.push(0);
        assert_eq!(ExecResult::decode(&extra), None);
        let mut bad_status = good.clone();
        bad_status[4] = 9;
        assert_eq!(ExecResult::decode(&bad_status), None);
        let mut huge = good.clone();
        huge[10..14].copy_from_slice(&u32::MAX.to_le_bytes());
        assert_eq!(ExecResult::decode(&huge), None);

        assert_eq!(
            ExecRequest::decode(&[1, 0, 0, 0, 1, 0, 0, 0]),
            None,
            "empty"
        );
        assert_eq!(ExecRequest::decode(&[1, 0, 0]), None, "short");
        let mut zero_timeout = vec![1, 0, 0, 0, 0, 0, 0, 0];
        zero_timeout.extend_from_slice(b"ver");
        assert_eq!(ExecRequest::decode(&zero_timeout), None);
        let mut long = vec![1, 0, 0, 0, 1, 0, 0, 0];
        long.extend(std::iter::repeat_n(b'a', MAX_EXEC_COMMAND + 1));
        assert_eq!(ExecRequest::decode(&long), None);
        let mut not_utf8 = vec![1, 0, 0, 0, 1, 0, 0, 0];
        not_utf8.push(0xff);
        assert_eq!(ExecRequest::decode(&not_utf8), None);
    }

    #[test]
    fn an_absurd_length_resets_the_stream() {
        let mut d = Decoder::default();
        assert!(d.push(&[0xff, 0xff, 0xff, 0xff, 1, 2, 3]).is_empty());
        assert_eq!(d.push(&encode(1, b"ok")), vec![(1, b"ok".to_vec())]);
    }
}
