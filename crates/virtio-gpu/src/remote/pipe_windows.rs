//! The Windows half of the renderer-isolation transport (backlog VEN-2004).
//!
//! On Unix the VMM and its renderer helper share **one** bidirectional file
//! descriptor: a `socketpair` end installed as the child's stdin, which the
//! helper reads *and* writes. [`DuplexPipe`] reproduces exactly that shape on
//! Windows, so everything above the transport — the framing, the protocol, the
//! server loop, the crash semantics — is one implementation on both hosts.
//!
//! # Why a named pipe and not two anonymous pipes
//!
//! `CreatePipe` (and `Stdio::piped()`, which is the same thing with the
//! bookkeeping done for you) gives *unidirectional* handles, so a request/reply
//! protocol needs two of them, and the reply direction has to be the child's
//! **stdout**. That is the wrong place for it twice over: stdout is where the
//! helper's own diagnostics go — and where, in the containment test, the child
//! test binary's libtest preamble goes — so the protocol stream would start
//! with someone else's bytes. It would also fork `serve_stdin` into two
//! meanings, one per host, in the one module whose whole job is to be the same
//! everywhere.
//!
//! A duplex named pipe has none of that: one inheritable handle, installed as
//! the child's stdin exactly as the socketpair end is, with stdout and stderr
//! left inherited for logs. It is also what the Rust standard library itself
//! uses to implement "anonymous" pipes on Windows (an instance with a
//! generated name), so it is the ordinary way to do this, not an exotic one.
//!
//! # The name is not a hole
//!
//! A named pipe has a name in a namespace any process can open, so the
//! instance is created with `FILE_FLAG_FIRST_PIPE_INSTANCE` (creation *fails*
//! if the name already exists — nobody can pre-squat it), `nMaxInstances = 1`
//! (once our own client end is connected, no second client can be), and
//! `PIPE_REJECT_REMOTE_CLIENTS` (nothing off-machine, ever). The name itself
//! carries the process id and a monotonic counter mixed with the clock, so two
//! renderers in one VMM — or two VMMs — never collide.

use std::fs::File;
use std::io::{self, Read, Write};
use std::os::windows::io::{FromRawHandle, OwnedHandle, RawHandle};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use windows::core::PCWSTR;
use windows::Win32::Foundation::{ERROR_PIPE_CONNECTED, GENERIC_READ, GENERIC_WRITE, HANDLE};
use windows::Win32::Security::SECURITY_ATTRIBUTES;
use windows::Win32::Storage::FileSystem::{
    CreateFileW, FILE_FLAGS_AND_ATTRIBUTES, FILE_FLAG_FIRST_PIPE_INSTANCE, FILE_SHARE_NONE,
    OPEN_EXISTING, PIPE_ACCESS_DUPLEX,
};
use windows::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, PeekNamedPipe, PIPE_READMODE_BYTE,
    PIPE_REJECT_REMOTE_CLIENTS, PIPE_TYPE_BYTE, PIPE_WAIT,
};

/// Kernel buffer per direction. One [`super::REMOTE_XFER_WINDOW`] worth of
/// payload never has to be split into more round trips than necessary; the
/// pipe blocks rather than losing anything when it fills, so this is a
/// throughput knob and nothing more.
const PIPE_BUFFER_BYTES: u32 = 256 * 1024;

/// How long a timed read sleeps between availability checks. 250 µs is far
/// below any deadline this transport is given (the ten-second handshake) and
/// far above the cost of one `PeekNamedPipe`.
const POLL_INTERVAL: Duration = Duration::from_micros(250);

/// Distinguishes pipe names created by this process.
static NEXT_PIPE: AtomicU64 = AtomicU64::new(0);

/// One end of the VMM↔helper channel: a duplex named pipe, readable and
/// writable, with an optional read timeout.
#[derive(Debug)]
pub struct DuplexPipe {
    file: File,
    /// Read timeout in microseconds; 0 means "block forever". Shared with
    /// every clone, which is how `UnixStream::set_read_timeout` behaves (the
    /// timeout lives on the socket, not on the descriptor).
    timeout_us: Arc<AtomicU64>,
}

impl DuplexPipe {
    /// Creates a connected pair: our end, and the inheritable handle to hand
    /// the child as its stdin.
    pub fn pair() -> io::Result<(Self, OwnedHandle)> {
        let name = unique_name();
        let wide: Vec<u16> = name.encode_utf16().chain(std::iter::once(0)).collect();

        // SAFETY: `wide` is a NUL-terminated UTF-16 buffer that outlives the
        // call, and every other argument is a plain integer. The returned
        // handle is checked below and immediately given an owner.
        let server = unsafe {
            CreateNamedPipeW(
                PCWSTR(wide.as_ptr()),
                FILE_FLAGS_AND_ATTRIBUTES(PIPE_ACCESS_DUPLEX.0 | FILE_FLAG_FIRST_PIPE_INSTANCE.0),
                PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
                1,
                PIPE_BUFFER_BYTES,
                PIPE_BUFFER_BYTES,
                0,
                None,
            )
        };
        if server.is_invalid() {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: `server` is a valid, freshly created handle that nothing
        // else owns; wrapping it here is what gives it exactly one owner, so
        // every early return below closes it.
        let server = File::from(unsafe { OwnedHandle::from_raw_handle(server.0 as RawHandle) });

        let attributes = SECURITY_ATTRIBUTES {
            nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: std::ptr::null_mut(),
            // The whole point: the child inherits this end.
            bInheritHandle: true.into(),
        };
        // SAFETY: `wide` still holds the NUL-terminated name, `attributes` is
        // a fully initialized struct that outlives the call, and the handle is
        // given an owner on the next line.
        let client = unsafe {
            CreateFileW(
                PCWSTR(wide.as_ptr()),
                GENERIC_READ.0 | GENERIC_WRITE.0,
                FILE_SHARE_NONE,
                Some(&attributes),
                OPEN_EXISTING,
                FILE_FLAGS_AND_ATTRIBUTES(0),
                None,
            )
        }
        // `windows::core::Error` is an HRESULT wrapper with no `io::Error`
        // conversion; the thread's last-error is the same failure as a plain
        // Win32 code, which is what an `io::Error` is made of here.
        .map_err(|_| io::Error::last_os_error())?;
        // SAFETY: `client` came back from a successful `CreateFileW`, so it is
        // valid and unowned until this line.
        let client = unsafe { OwnedHandle::from_raw_handle(client.0 as RawHandle) };

        // The client is already attached, so this reports `ERROR_PIPE_CONNECTED`
        // rather than success — which is the documented "a client got here
        // first" answer and exactly what we arranged.
        //
        // SAFETY: `server` is a valid pipe-server handle we own, and passing
        // no OVERLAPPED is correct for a synchronous handle.
        let connected = unsafe { ConnectNamedPipe(HANDLE(handle_of(&server)), None) };
        match connected {
            Ok(()) => (),
            Err(error) if error.code() == ERROR_PIPE_CONNECTED.to_hresult() => (),
            Err(_) => return Err(io::Error::last_os_error()),
        }

        Ok((
            Self {
                file: server,
                timeout_us: Arc::new(AtomicU64::new(0)),
            },
            client,
        ))
    }

    /// A second handle onto the same pipe end, so the client can keep a
    /// buffered reader and a buffered writer.
    pub fn try_clone(&self) -> io::Result<Self> {
        Ok(Self {
            file: self.file.try_clone()?,
            timeout_us: Arc::clone(&self.timeout_us),
        })
    }

    /// Bounds how long a read waits, matching `UnixStream::set_read_timeout`:
    /// past the deadline a read fails with [`io::ErrorKind::TimedOut`].
    ///
    /// A synchronous named pipe has no kernel-side read timeout, so this is a
    /// `PeekNamedPipe` poll before the blocking read. That is only ever armed
    /// for the handshake — a bounded ten seconds during process bring-up, not
    /// the steady state, which blocks in the kernel like every other read.
    pub fn set_read_timeout(&self, timeout: Option<Duration>) -> io::Result<()> {
        let micros = match timeout {
            None => 0,
            Some(timeout) => u64::try_from(timeout.as_micros())
                .unwrap_or(u64::MAX)
                .max(1),
        };
        self.timeout_us.store(micros, Ordering::Release);
        Ok(())
    }

    /// Bytes readable without blocking, or `None` when the peer has gone (a
    /// broken pipe is the caller's business, not this poll's).
    fn available(&self) -> Option<u32> {
        let mut total = 0u32;
        // SAFETY: the handle is valid for as long as `self.file` lives, and
        // `total` is a live `u32` for the duration of the call. Passing no
        // buffer asks only for the counts, which is all this reads.
        let peeked = unsafe {
            PeekNamedPipe(
                HANDLE(handle_of(&self.file)),
                None,
                0,
                None,
                Some(&mut total),
                None,
            )
        };
        peeked.ok().map(|()| total)
    }
}

impl Read for DuplexPipe {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let micros = self.timeout_us.load(Ordering::Acquire);
        if micros > 0 {
            let deadline = Instant::now() + Duration::from_micros(micros);
            loop {
                match self.available() {
                    // The peer is gone, or something is readable: either way
                    // the blocking read below answers immediately and
                    // correctly.
                    None | Some(1..) => break,
                    Some(0) => (),
                }
                if Instant::now() >= deadline {
                    return Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        "the renderer process did not answer in time",
                    ));
                }
                std::thread::sleep(POLL_INTERVAL);
            }
        }
        self.file.read(buf)
    }
}

impl Write for DuplexPipe {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.file.write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.file.flush()
    }
}

/// The raw handle behind a `File`, as the Win32 signatures want it.
fn handle_of(file: &File) -> *mut std::ffi::c_void {
    use std::os::windows::io::AsRawHandle;
    file.as_raw_handle()
}

/// A pipe name no other instance can be holding: our process id, a counter,
/// and the clock, so a re-run after a crash cannot inherit a stale name
/// either.
fn unique_name() -> String {
    let counter = NEXT_PIPE.fetch_add(1, Ordering::Relaxed);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    format!(
        r"\\.\pipe\entangled-gpu-renderer-{}-{counter}-{nanos:08x}",
        std::process::id()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The pair is a real, connected, bidirectional channel: both ends can
    /// write and both ends can read.
    #[test]
    fn a_pair_carries_bytes_both_ways() {
        let (mut ours, theirs) = DuplexPipe::pair().expect("pipe pair");
        let mut theirs = File::from(theirs);

        ours.write_all(b"ping").expect("write from the VMM end");
        ours.flush().expect("flush");
        let mut buf = [0u8; 4];
        theirs.read_exact(&mut buf).expect("read at the helper end");
        assert_eq!(&buf, b"ping");

        theirs
            .write_all(b"pong")
            .expect("write from the helper end");
        let mut buf = [0u8; 4];
        ours.read_exact(&mut buf).expect("read at the VMM end");
        assert_eq!(&buf, b"pong");
    }

    /// A clone is the same pipe end, which is what lets the client hold a
    /// buffered reader and a buffered writer at once.
    #[test]
    fn a_clone_is_the_same_end() {
        let (ours, theirs) = DuplexPipe::pair().expect("pipe pair");
        let mut theirs = File::from(theirs);
        let mut reader = ours.try_clone().expect("clone");
        let mut writer = ours;

        writer.write_all(b"x").expect("write");
        writer.flush().expect("flush");
        let mut byte = [0u8; 1];
        theirs.read_exact(&mut byte).expect("helper reads");
        assert_eq!(&byte, b"x");

        theirs.write_all(b"y").expect("helper writes");
        reader.read_exact(&mut byte).expect("clone reads");
        assert_eq!(&byte, b"y");
    }

    /// A read timeout expires instead of blocking forever, and clearing it
    /// puts the pipe back to blocking — both halves of what the handshake
    /// needs.
    #[test]
    fn a_read_timeout_expires_and_can_be_cleared() {
        let (mut ours, theirs) = DuplexPipe::pair().expect("pipe pair");
        let mut theirs = File::from(theirs);

        ours.set_read_timeout(Some(Duration::from_millis(50)))
            .expect("arm");
        let mut buf = [0u8; 1];
        let error = ours.read(&mut buf).expect_err("nothing was sent");
        assert_eq!(error.kind(), io::ErrorKind::TimedOut, "{error}");

        // Still armed, but now there is data: the read succeeds.
        theirs.write_all(b"z").expect("helper writes");
        assert_eq!(ours.read(&mut buf).expect("read"), 1);
        assert_eq!(&buf, b"z");

        ours.set_read_timeout(None).expect("disarm");
        theirs.write_all(b"w").expect("helper writes");
        assert_eq!(ours.read(&mut buf).expect("blocking read"), 1);
    }

    /// The helper end going away is a clean EOF, which is the whole basis of
    /// the crash detection above this module.
    #[test]
    fn a_closed_helper_end_reads_as_end_of_file() {
        let (mut ours, theirs) = DuplexPipe::pair().expect("pipe pair");
        drop(theirs);
        let mut buf = [0u8; 1];
        match ours.read(&mut buf) {
            Ok(0) => (),
            Ok(n) => panic!("expected EOF, read {n} bytes"),
            // A pipe whose only client closed reports a broken pipe rather
            // than a zero read, and `WireError` maps both to `Closed`.
            Err(error) => assert!(
                matches!(
                    error.kind(),
                    io::ErrorKind::BrokenPipe | io::ErrorKind::UnexpectedEof
                ),
                "{error}"
            ),
        }
    }

    /// Two pairs in one process must not collide on the pipe name.
    #[test]
    fn names_are_unique_within_a_process() {
        let first = unique_name();
        let second = unique_name();
        assert_ne!(first, second);
        let (_a, _a_child) = DuplexPipe::pair().expect("first pair");
        let (_b, _b_child) = DuplexPipe::pair().expect("second pair");
    }
}
