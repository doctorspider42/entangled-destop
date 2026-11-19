//! Running a command line the host asked for ([`protocol::KIND_EXEC`]).
//!
//! `cmd.exe /d /s /c <line>` with no window, in the agent's own session (the
//! logged-on user, unelevated), stdin empty, stdout and stderr captured up to
//! [`protocol::MAX_EXEC_OUTPUT`] each. At most [`MAX_RUNNING`] at once; one
//! more is answered `Busy` rather than queued.
//!
//! Past its timeout the command's `cmd.exe` is killed. Programs it started
//! and did not wait for (`start notepad`) are left alone — that is what
//! `start` is for — and a pipe such a program inherited does not hold the
//! answer back: once `cmd.exe` has exited, what was captured within
//! [`DRAIN`] is sent.

use std::io::Read;
use std::os::windows::process::CommandExt;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{mpsc, Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use crate::protocol::{ExecRequest, ExecResult, ExecStatus, MAX_EXEC_OUTPUT};

/// Commands that may run at once.
pub const MAX_RUNNING: usize = 4;
/// `CREATE_NO_WINDOW`: the agent has no console, and a command must not
/// flash one on the user's desktop.
const CREATE_NO_WINDOW: u32 = 0x0800_0000;
/// How long the output pipes may stay open after `cmd.exe` exits.
const DRAIN: Duration = Duration::from_secs(1);
/// How often a running command is looked at.
const POLL: Duration = Duration::from_millis(20);

static RUNNING: AtomicUsize = AtomicUsize::new(0);

/// Runs `request` on a thread of its own and hands the result to `reply`.
pub fn spawn(request: ExecRequest, reply: impl FnOnce(ExecResult) + Send + 'static) {
    if RUNNING.fetch_add(1, Ordering::AcqRel) >= MAX_RUNNING {
        RUNNING.fetch_sub(1, Ordering::AcqRel);
        reply(ExecResult {
            id: request.id,
            status: ExecStatus::Busy,
            stdout: Vec::new(),
            stderr: Vec::new(),
            stdout_truncated: false,
            stderr_truncated: false,
        });
        return;
    }
    let spawned = std::thread::Builder::new()
        .name("exec".into())
        .spawn(move || {
            let result = run(&request);
            RUNNING.fetch_sub(1, Ordering::AcqRel);
            reply(result);
        });
    if spawned.is_err() {
        RUNNING.fetch_sub(1, Ordering::AcqRel);
    }
}

/// One stream's bytes, capped; what is past the cap is read and dropped.
#[derive(Default)]
struct Capture {
    bytes: Vec<u8>,
    truncated: bool,
}

fn drain(mut from: impl Read + Send + 'static, done: mpsc::Sender<()>) -> Arc<Mutex<Capture>> {
    let capture = Arc::new(Mutex::new(Capture::default()));
    let into = Arc::clone(&capture);
    let _ = std::thread::Builder::new()
        .name("exec-pipe".into())
        .spawn(move || {
            let mut buffer = [0u8; 8192];
            while let Ok(n) = from.read(&mut buffer) {
                if n == 0 {
                    break;
                }
                let mut capture = into.lock().unwrap_or_else(PoisonError::into_inner);
                let room = MAX_EXEC_OUTPUT.saturating_sub(capture.bytes.len());
                let take = n.min(room);
                capture.bytes.extend_from_slice(&buffer[..take]);
                capture.truncated |= take < n;
            }
            let _ = done.send(());
        });
    capture
}

fn run(request: &ExecRequest) -> ExecResult {
    let failed = |message: String| ExecResult {
        id: request.id,
        status: ExecStatus::Failed,
        stdout: Vec::new(),
        stderr: message.into_bytes(),
        stdout_truncated: false,
        stderr_truncated: false,
    };
    let comspec = std::env::var_os("ComSpec").unwrap_or_else(|| "cmd.exe".into());
    // `/s /c "<line>"`: cmd strips exactly the outer quotes and runs the
    // line as typed, inner quotes included.
    let mut child = match Command::new(comspec)
        .raw_arg(format!("/d /s /c \"{}\"", request.command))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .creation_flags(CREATE_NO_WINDOW)
        .spawn()
    {
        Ok(child) => child,
        Err(error) => return failed(format!("cannot start cmd.exe: {error}")),
    };
    let (done, finished) = mpsc::channel();
    let stdout = child.stdout.take().map(|s| drain(s, done.clone()));
    let stderr = child.stderr.take().map(|s| drain(s, done.clone()));
    drop(done);
    let deadline = Instant::now() + Duration::from_millis(u64::from(request.timeout_ms));
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break ExecStatus::Exited(status.code().unwrap_or(-1)),
            Ok(None) if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                break ExecStatus::TimedOut;
            }
            Ok(None) => std::thread::sleep(POLL),
            Err(error) => {
                let _ = child.kill();
                return failed(format!("cannot wait for the command: {error}"));
            }
        }
    };
    // Both pipes at end of file, or what they hold by the drain deadline.
    let until = Instant::now() + DRAIN;
    for _ in 0..2 {
        let left = until.saturating_duration_since(Instant::now());
        if finished.recv_timeout(left).is_err() {
            break;
        }
    }
    let take = |capture: Option<Arc<Mutex<Capture>>>| match capture {
        Some(capture) => {
            let capture = capture.lock().unwrap_or_else(PoisonError::into_inner);
            (text(&capture.bytes), capture.truncated)
        }
        None => (Vec::new(), false),
    };
    let (stdout, stdout_truncated) = take(stdout);
    let (stderr, stderr_truncated) = take(stderr);
    ExecResult {
        id: request.id,
        status,
        stdout,
        stderr,
        stdout_truncated,
        stderr_truncated,
    }
}

/// Console programs write the OEM code page, not UTF-8: anything that is not
/// already UTF-8 is converted from it, so the host can print the result.
fn text(bytes: &[u8]) -> Vec<u8> {
    if std::str::from_utf8(bytes).is_ok() {
        return bytes.to_vec();
    }
    use windows::Win32::Globalization::{
        MultiByteToWideChar, CP_OEMCP, MULTI_BYTE_TO_WIDE_CHAR_FLAGS,
    };
    // SAFETY: a size query; `bytes` is a valid slice for the call.
    let units =
        unsafe { MultiByteToWideChar(CP_OEMCP, MULTI_BYTE_TO_WIDE_CHAR_FLAGS(0), bytes, None) };
    let Ok(len) = usize::try_from(units) else {
        return String::from_utf8_lossy(bytes).into_owned().into_bytes();
    };
    if len == 0 {
        return String::from_utf8_lossy(bytes).into_owned().into_bytes();
    }
    let mut wide = vec![0u16; len];
    // SAFETY: `wide` has exactly the length the size query asked for.
    let written = unsafe {
        MultiByteToWideChar(
            CP_OEMCP,
            MULTI_BYTE_TO_WIDE_CHAR_FLAGS(0),
            bytes,
            Some(&mut wide),
        )
    };
    let written = usize::try_from(written).unwrap_or(0).min(len);
    String::from_utf16_lossy(&wide[..written]).into_bytes()
}
