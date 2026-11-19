//! The host half of the shared clipboard (ADR-0008): the guest agent's port
//! on one side, the host clipboard on the other.
//!
//! Text only, both ways. The host clipboard is looked at every
//! [`POLL`]; a change is sent to the guest. What the guest sends is put on
//! the host clipboard. Each side remembers the last text it exchanged, so
//! nothing bounces back and forth. The wire format is the agent's own
//! (`guest/windows-agent/src/protocol.rs`, included here so the two cannot
//! drift apart).
//!
//! The same port carries the control channel's `exec` ([`Exec`]): a command
//! line out, its exit code and output back, printed as
//! `entangled-control: exec <id> ...` lines.

use std::collections::HashSet;
use std::io::Write as _;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread::JoinHandle;
use std::time::Duration;

use control_api::control::PREFIX;

#[path = "../../../guest/windows-agent/src/protocol.rs"]
#[allow(dead_code)]
mod protocol;

/// The port the agent opens.
pub(super) const PORT_NAME: &str = protocol::PORT_NAME;

/// How often the host clipboard is looked at.
const POLL: Duration = Duration::from_millis(400);

/// The bridge's thread; stopped and joined on drop.
pub(super) struct Bridge {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Bridge {
    pub(super) fn start(
        port: virtio_console::PortHandle,
        exec: Option<Arc<Exec>>,
    ) -> std::io::Result<Self> {
        let stop = Arc::new(AtomicBool::new(false));
        // What the two sides last agreed on.
        let last = Arc::new(Mutex::new(None::<String>));
        // The guest's messages, applied on the port's thread: setting the
        // host clipboard is quick, and keeping it there keeps the order.
        {
            let last = Arc::clone(&last);
            let decoder = Mutex::new(protocol::Decoder::default());
            port.receive(Arc::new(move |bytes: &[u8]| {
                let messages = decoder
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .push(bytes);
                for (kind, payload) in messages {
                    if kind == protocol::KIND_EXEC_RESULT {
                        if let Some(exec) = &exec {
                            exec.on_result(&payload);
                        }
                        continue;
                    }
                    if kind != protocol::KIND_CLIPBOARD_TEXT {
                        continue;
                    }
                    let Ok(text) = String::from_utf8(payload) else {
                        continue;
                    };
                    *last.lock().unwrap_or_else(PoisonError::into_inner) = Some(text.clone());
                    match arboard::Clipboard::new().and_then(|mut c| c.set_text(text)) {
                        Ok(()) => tracing::debug!("guest clipboard copied to the host"),
                        Err(error) => tracing::warn!(%error, "cannot set the host clipboard"),
                    }
                }
            }));
        }
        let thread = {
            let stop = Arc::clone(&stop);
            std::thread::Builder::new()
                .name("clipboard-bridge".into())
                .spawn(move || watch_host(&port, &last, &stop))?
        };
        Ok(Self {
            stop,
            thread: Some(thread),
        })
    }
}

impl Drop for Bridge {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn watch_host(port: &virtio_console::PortHandle, last: &Mutex<Option<String>>, stop: &AtomicBool) {
    let mut clipboard = match arboard::Clipboard::new() {
        Ok(clipboard) => clipboard,
        Err(error) => {
            tracing::warn!(%error, "no host clipboard; the guest's clipboard stays its own");
            return;
        }
    };
    let mut was_open = false;
    while !stop.load(Ordering::Acquire) {
        std::thread::sleep(POLL);
        let open = port.is_open();
        if open != was_open {
            tracing::info!(connected = open, "guest agent");
            was_open = open;
            if open {
                // A fresh agent knows nothing: offer it the host's text.
                *last.lock().unwrap_or_else(PoisonError::into_inner) = None;
            }
        }
        if !open {
            continue;
        }
        let Ok(text) = clipboard.get_text() else {
            continue;
        };
        if text.len() > protocol::MAX_PAYLOAD {
            continue;
        }
        let mut last = last.lock().unwrap_or_else(PoisonError::into_inner);
        if last.as_deref() == Some(text.as_str()) {
            continue;
        }
        if port.send(&protocol::encode(
            protocol::KIND_CLIPBOARD_TEXT,
            text.as_bytes(),
        )) {
            *last = Some(text);
        }
    }
}

/// Commands that may wait for an answer at once.
const MAX_PENDING: usize = 16;
/// How long past its own timeout a command's answer is waited for: the
/// agent kills it at the timeout, then drains its pipes for a second.
const ANSWER_GRACE: Duration = Duration::from_secs(15);

/// The host half of `exec`: sends command lines to the agent and prints what
/// comes back. Only built for a profile with `guest_exec = true`.
pub(super) struct Exec {
    port: virtio_console::PortHandle,
    pending: Mutex<HashSet<u32>>,
    next: AtomicU32,
}

impl Exec {
    pub(super) fn new(port: virtio_console::PortHandle) -> Arc<Self> {
        Arc::new(Self {
            port,
            pending: Mutex::new(HashSet::new()),
            next: AtomicU32::new(1),
        })
    }

    /// Sends `command`; answers its id. Every id sent is answered by exactly
    /// one final line: the result, or `error` when the agent never answers.
    pub(super) fn run(self: &Arc<Self>, command: &str, timeout_secs: u32) -> Result<u32, String> {
        if command.len() > protocol::MAX_EXEC_COMMAND {
            return Err(format!(
                "the command line is longer than {} bytes",
                protocol::MAX_EXEC_COMMAND
            ));
        }
        if !self.port.is_open() {
            return Err(
                "the guest agent is not connected (nobody logged on yet, or no agent installed)"
                    .into(),
            );
        }
        let timeout_ms = timeout_secs
            .saturating_mul(1000)
            .min(protocol::MAX_EXEC_TIMEOUT_MS);
        let id = {
            let mut pending = self.pending.lock().unwrap_or_else(PoisonError::into_inner);
            if pending.len() >= MAX_PENDING {
                return Err(format!("{MAX_PENDING} commands are already waiting"));
            }
            let id = self.next.fetch_add(1, Ordering::Relaxed);
            pending.insert(id);
            id
        };
        let request = protocol::ExecRequest {
            id,
            timeout_ms,
            command: command.to_string(),
        };
        if !self
            .port
            .send(&protocol::encode(protocol::KIND_EXEC, &request.encode()))
        {
            self.forget(id);
            return Err("the agent's port is full".into());
        }
        let this = Arc::clone(self);
        let wait = Duration::from_millis(u64::from(timeout_ms)) + ANSWER_GRACE;
        let watchdog = std::thread::Builder::new()
            .name("exec-watchdog".into())
            .spawn(move || {
                std::thread::sleep(wait);
                if this.forget(id) {
                    print_lines(&[format!(
                        "exec {id} error no answer from the guest agent (an agent older than \
                         exec, or it stopped)"
                    )]);
                }
            });
        if let Err(error) = watchdog {
            tracing::warn!(%error, "cannot watch an exec for its answer");
        }
        Ok(id)
    }

    fn forget(&self, id: u32) -> bool {
        self.pending
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&id)
    }

    /// A result from the guest: printed if it answers a command still
    /// waiting, dropped otherwise (late, repeated, or made up).
    fn on_result(&self, payload: &[u8]) {
        let Some(result) = protocol::ExecResult::decode(payload) else {
            tracing::warn!("the guest agent sent an exec result that does not parse");
            return;
        };
        if !self.forget(result.id) {
            return;
        }
        print_lines(&result_lines(&result));
    }
}

/// Prints reply lines in one write, so another thread's reply cannot land
/// between them.
pub(super) fn print_lines(lines: &[String]) {
    let mut out = String::new();
    for line in lines {
        out.push_str(PREFIX);
        out.push(' ');
        out.push_str(line);
        out.push('\n');
    }
    let mut stdout = std::io::stdout().lock();
    let _ = stdout.write_all(out.as_bytes());
    let _ = stdout.flush();
}

/// Guest text made safe to print on one line: control characters (a
/// carriage return, an escape sequence) become U+FFFD, tabs stay.
fn printable(text: &str) -> String {
    text.chars()
        .map(|c| {
            if c.is_control() && c != '\t' {
                '\u{fffd}'
            } else {
                c
            }
        })
        .collect()
}

/// The reply lines for one result (without the prefix): one line per output
/// line, so a guest cannot forge a reply of its own or move the terminal's
/// cursor, then the one final line.
fn result_lines(result: &protocol::ExecResult) -> Vec<String> {
    let id = result.id;
    let mut lines = Vec::new();
    let failed = matches!(result.status, protocol::ExecStatus::Failed);
    for (name, bytes) in [("stdout", &result.stdout), ("stderr", &result.stderr)] {
        if failed && name == "stderr" {
            continue;
        }
        let text = String::from_utf8_lossy(bytes);
        let text = text.strip_suffix('\n').unwrap_or(&text);
        if text.is_empty() {
            continue;
        }
        for line in text.split('\n') {
            let line = line.strip_suffix('\r').unwrap_or(line);
            lines.push(format!("exec {id} {name} {}", printable(line)));
        }
    }
    let mut last = match result.status {
        protocol::ExecStatus::Exited(code) => format!("exec {id} exit {code}"),
        protocol::ExecStatus::TimedOut => format!("exec {id} timeout"),
        protocol::ExecStatus::Busy => {
            format!("exec {id} error the agent is already running as many commands as it will")
        }
        protocol::ExecStatus::Failed => {
            let why = String::from_utf8_lossy(&result.stderr).replace(['\r', '\n'], " ");
            format!("exec {id} error {}", printable(why.trim()))
        }
    };
    if result.stdout_truncated {
        last.push_str(" stdout-truncated");
    }
    if result.stderr_truncated {
        last.push_str(" stderr-truncated");
    }
    lines.push(last);
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    fn result(status: protocol::ExecStatus, stdout: &[u8], stderr: &[u8]) -> protocol::ExecResult {
        protocol::ExecResult {
            id: 4,
            status,
            stdout: stdout.to_vec(),
            stderr: stderr.to_vec(),
            stdout_truncated: false,
            stderr_truncated: false,
        }
    }

    #[test]
    fn output_becomes_one_reply_line_per_line_then_the_exit() {
        let lines = result_lines(&result(
            protocol::ExecStatus::Exited(2),
            b"one\r\ntwo\r\n",
            b"bad\r\n",
        ));
        assert_eq!(
            lines,
            [
                "exec 4 stdout one",
                "exec 4 stdout two",
                "exec 4 stderr bad",
                "exec 4 exit 2"
            ]
        );
    }

    /// A guest that prints a carriage return and a forged reply gets
    /// neither: its text stays inside its own `stdout` line.
    #[test]
    fn guest_output_cannot_forge_a_reply_or_steer_the_terminal() {
        let lines = result_lines(&result(
            protocol::ExecStatus::Exited(0),
            b"x\rentangled-control: saved /evil\x1b[2J",
            b"",
        ));
        assert_eq!(lines.len(), 2);
        assert!(
            lines[0].starts_with("exec 4 stdout x\u{fffd}entangled-control"),
            "{}",
            lines[0]
        );
        assert!(!lines[0].contains('\x1b'));
    }

    #[test]
    fn every_ending_has_its_final_line() {
        let mut timed_out = result(protocol::ExecStatus::TimedOut, b"", b"");
        timed_out.stdout_truncated = true;
        assert_eq!(
            result_lines(&timed_out),
            ["exec 4 timeout stdout-truncated"]
        );
        assert_eq!(
            result_lines(&result(
                protocol::ExecStatus::Failed,
                b"",
                b"cannot start\r\n"
            )),
            ["exec 4 error cannot start"]
        );
        assert!(
            result_lines(&result(protocol::ExecStatus::Busy, b"", b""))[0]
                .starts_with("exec 4 error")
        );
    }
}
