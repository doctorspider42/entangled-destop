//! Child-process supervision: one `entangled install` or `entangled run` per
//! task, each watched by a single worker thread.
//!
//! Design notes:
//! - **Output goes to a log file, not a pipe.** If the manager exits while a VM
//!   runs, a pipe would break under the guest's serial console; a file keeps the
//!   detached VM healthy (GUI-1603) and leaves a post-mortem log behind.
//! - **Children are never killed on manager exit.** `std::process::Child` does
//!   not reap on drop, so closing the manager leaves running VMs alone.
//! - **Nothing here touches egui.** Repaints are requested through a `Waker`
//!   callback so this module stays testable with mock commands.

use std::collections::VecDeque;
use std::fs::File;
use std::io::Read as _;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use thiserror::Error;

/// How long a stop request waits for a clean exit before escalating to a kill.
const STOP_GRACE: Duration = Duration::from_secs(20);
/// Watcher poll interval — also the log-tail cadence.
const POLL: Duration = Duration::from_millis(120);
/// Lines kept in memory per task (the full log stays on disk).
const LOG_CAPACITY: usize = 4000;

#[derive(Debug, Error)]
pub enum ProcessError {
    #[error("cannot create the log file {path}: {source}")]
    LogFile {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("cannot start {program}: {source}")]
    Spawn {
        program: String,
        #[source]
        source: std::io::Error,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskKind {
    /// `entangled run <profile>` — a live VM.
    Run,
    /// `entangled install debian …` — a provisioning run.
    Install,
}

impl TaskKind {
    pub fn label(self) -> &'static str {
        match self {
            TaskKind::Run => "run",
            TaskKind::Install => "install",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TaskState {
    Running,
    /// A stop was requested; the child has been signalled.
    Stopping,
    Finished(Outcome),
}

impl TaskState {
    pub fn is_active(&self) -> bool {
        matches!(self, TaskState::Running | TaskState::Stopping)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Outcome {
    pub success: bool,
    /// Human-readable exit description ("exit status 0", "stopped", …).
    pub detail: String,
    /// True when the exit followed a stop request from the UI.
    pub stopped_by_user: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogLine {
    pub text: String,
}

/// Where the ANSI filter is when a chunk boundary splits an escape sequence.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
enum Ansi {
    #[default]
    Text,
    /// Saw ESC, waiting for the sequence introducer.
    Escape,
    /// Inside a CSI sequence (`ESC [ … final-byte`).
    Csi,
    /// Inside an OSC string (`ESC ] … BEL | ESC \`).
    Osc,
}

#[derive(Debug, Default)]
struct LogBuffer {
    lines: VecDeque<LogLine>,
    dropped: usize,
    partial: String,
    ansi: Ansi,
}

impl LogBuffer {
    /// Appends child output, splitting lines and dropping ANSI escapes. The CLI
    /// colours its `tracing` output and the guest kernel adds its own sequences;
    /// egui has no terminal emulator, so the codes would show up as `[2m` litter.
    fn push_chunk(&mut self, chunk: &str) {
        for ch in chunk.chars() {
            match self.ansi {
                Ansi::Escape => {
                    self.ansi = match ch {
                        '[' => Ansi::Csi,
                        ']' => Ansi::Osc,
                        // Two-character sequences (ESC c, ESC =, …) end here.
                        _ => Ansi::Text,
                    };
                    continue;
                }
                Ansi::Csi => {
                    // Parameter and intermediate bytes, then a final byte in 0x40..=0x7E.
                    if ('\x40'..='\x7e').contains(&ch) {
                        self.ansi = Ansi::Text;
                    }
                    continue;
                }
                Ansi::Osc => {
                    if ch == '\x07' || ch == '\x1b' {
                        self.ansi = Ansi::Text;
                    }
                    continue;
                }
                Ansi::Text => {}
            }
            match ch {
                '\x1b' => self.ansi = Ansi::Escape,
                '\n' => self.flush_partial(),
                '\r' => {}
                // Other C0 controls carry no meaning in a plain log view.
                ch if ch.is_control() && ch != '\t' => {}
                ch => self.partial.push(ch),
            }
        }
        // Keep unterminated output visible (progress lines, prompts) without
        // committing it: flushed once the newline arrives.
        if self.partial.len() > 4096 {
            self.flush_partial();
        }
    }

    fn flush_partial(&mut self) {
        let text = std::mem::take(&mut self.partial);
        self.push_line(text);
    }

    fn push_line(&mut self, text: String) {
        if self.lines.len() == LOG_CAPACITY {
            self.lines.pop_front();
            self.dropped += 1;
        }
        self.lines.push_back(LogLine { text });
    }
}

/// Called whenever a task produces output or changes state; the UI wires this
/// to `egui::Context::request_repaint`.
pub type Waker = Arc<dyn Fn() + Send + Sync>;

#[cfg(test)]
pub fn no_waker() -> Waker {
    Arc::new(|| {})
}

pub type TaskId = u64;

#[derive(Debug, Clone)]
pub struct TaskSpec {
    pub kind: TaskKind,
    /// VM this task belongs to; the UI keys status badges off it.
    pub vm: String,
    pub program: PathBuf,
    pub args: Vec<String>,
    pub cwd: PathBuf,
    pub log_path: PathBuf,
}

impl TaskSpec {
    pub fn command_line(&self) -> String {
        let mut out = self.program.display().to_string();
        for arg in &self.args {
            out.push(' ');
            if arg.contains(' ') {
                out.push('"');
                out.push_str(arg);
                out.push('"');
            } else {
                out.push_str(arg);
            }
        }
        out
    }
}

struct Shared {
    state: Mutex<TaskState>,
    log: Mutex<LogBuffer>,
    stop: AtomicBool,
    kill: AtomicBool,
}

/// A supervised child process.
pub struct Task {
    pub id: TaskId,
    pub kind: TaskKind,
    pub vm: String,
    pub log_path: PathBuf,
    pub command_line: String,
    pub started_at: Instant,
    shared: Arc<Shared>,
    /// Set once the UI has reported the outcome (toast) so it does so only once.
    reported: bool,
}

impl Task {
    pub fn state(&self) -> TaskState {
        self.shared.state.lock_or_recover().clone()
    }

    pub fn is_active(&self) -> bool {
        self.state().is_active()
    }

    /// Requests a clean stop: SIGTERM on Unix (`entangled run` turns that into
    /// an orderly VM shutdown), `TerminateProcess` on Windows. Escalates to a
    /// hard kill after [`STOP_GRACE`].
    pub fn request_stop(&self) {
        self.shared.stop.store(true, Ordering::Release);
    }

    pub fn request_kill(&self) {
        self.shared.kill.store(true, Ordering::Release);
    }

    pub fn stop_requested(&self) -> bool {
        self.shared.stop.load(Ordering::Acquire)
    }

    /// Copies the tail of the log for rendering. `partial` output (a line
    /// without its newline yet) is included as the last entry.
    pub fn log_tail(&self, max_lines: usize) -> (Vec<String>, usize) {
        let guard = self.shared.log.lock_or_recover();
        let mut lines: Vec<String> = guard
            .lines
            .iter()
            .rev()
            .take(max_lines)
            .map(|l| l.text.clone())
            .rev()
            .collect();
        if !guard.partial.is_empty() {
            lines.push(guard.partial.clone());
        }
        (lines, guard.dropped)
    }
}

/// A poisoned lock here would mean a watcher thread panicked; the UI recovers
/// the data instead of propagating the panic into the frame loop.
trait LockOrRecover<T> {
    fn lock_or_recover(&self) -> std::sync::MutexGuard<'_, T>;
}

impl<T> LockOrRecover<T> for Mutex<T> {
    fn lock_or_recover(&self) -> std::sync::MutexGuard<'_, T> {
        match self.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        }
    }
}

/// Owns every task the manager has started in this session.
pub struct Supervisor {
    tasks: Vec<Task>,
    next_id: TaskId,
    waker: Waker,
}

impl Supervisor {
    pub fn new(waker: Waker) -> Self {
        Self {
            tasks: Vec::new(),
            next_id: 1,
            waker,
        }
    }

    pub fn tasks(&self) -> &[Task] {
        &self.tasks
    }

    pub fn task(&self, id: TaskId) -> Option<&Task> {
        self.tasks.iter().find(|t| t.id == id)
    }

    /// The active task of a VM, if any. There is at most one: `spawn` refuses
    /// to start a second.
    pub fn active_task(&self, vm: &str) -> Option<&Task> {
        self.tasks.iter().find(|t| t.vm == vm && t.is_active())
    }

    pub fn active_kind(&self, vm: &str) -> Option<TaskKind> {
        self.active_task(vm).map(|t| t.kind)
    }

    pub fn is_busy(&self, vm: &str) -> bool {
        self.active_task(vm).is_some()
    }

    pub fn any_active(&self) -> bool {
        self.tasks.iter().any(Task::is_active)
    }

    /// Starts a child. Returns the task id; the VM must not already be busy.
    pub fn spawn(&mut self, spec: TaskSpec) -> Result<TaskId, ProcessError> {
        let log = File::create(&spec.log_path).map_err(|source| ProcessError::LogFile {
            path: spec.log_path.clone(),
            source,
        })?;
        let log_err = log.try_clone().map_err(|source| ProcessError::LogFile {
            path: spec.log_path.clone(),
            source,
        })?;

        let mut command = Command::new(&spec.program);
        command
            .args(&spec.args)
            .current_dir(&spec.cwd)
            .stdin(Stdio::null())
            .stdout(Stdio::from(log))
            .stderr(Stdio::from(log_err));
        detach_process_group(&mut command);
        let child = command.spawn().map_err(|source| ProcessError::Spawn {
            program: spec.program.display().to_string(),
            source,
        })?;

        let shared = Arc::new(Shared {
            state: Mutex::new(TaskState::Running),
            log: Mutex::new(LogBuffer::default()),
            stop: AtomicBool::new(false),
            kill: AtomicBool::new(false),
        });

        let id = self.next_id;
        self.next_id += 1;
        let task = Task {
            id,
            kind: spec.kind,
            vm: spec.vm.clone(),
            log_path: spec.log_path.clone(),
            command_line: spec.command_line(),
            started_at: Instant::now(),
            shared: Arc::clone(&shared),
            reported: false,
        };

        let waker = Arc::clone(&self.waker);
        let log_path = spec.log_path.clone();
        let vm = spec.vm.clone();
        let kind = spec.kind;
        let builder = std::thread::Builder::new().name(format!("watch-{}-{vm}", kind.label()));
        if let Err(source) = builder.spawn(move || watch(child, shared, log_path, waker)) {
            // No watcher means no state updates; better to fail loudly than to
            // show a task that can never finish. The child is already running,
            // so say so in the error.
            tracing::error!(error = %source, vm = %vm, "cannot start the watcher thread");
            return Err(ProcessError::Spawn {
                program: format!("watcher thread for {vm}"),
                source,
            });
        }

        tracing::info!(task = id, vm = %spec.vm, kind = kind.label(), log = %spec.log_path.display(), "child started");
        self.tasks.push(task);
        Ok(id)
    }

    /// Outcomes not yet shown to the user, marked as reported.
    pub fn drain_finished(&mut self) -> Vec<(TaskKind, String, Outcome)> {
        let mut out = Vec::new();
        for task in &mut self.tasks {
            if task.reported {
                continue;
            }
            if let TaskState::Finished(outcome) = task.state() {
                task.reported = true;
                out.push((task.kind, task.vm.clone(), outcome));
            }
        }
        out
    }

    /// Drops finished tasks, keeping the newest `keep` of them for the log pane.
    pub fn prune(&mut self, keep: usize) {
        let finished: Vec<TaskId> = self
            .tasks
            .iter()
            .filter(|t| !t.is_active() && t.reported)
            .map(|t| t.id)
            .collect();
        if finished.len() <= keep {
            return;
        }
        let drop_before = finished[finished.len() - keep];
        self.tasks
            .retain(|t| t.is_active() || !t.reported || t.id >= drop_before);
    }
}

/// The per-task watcher: tails the log file, honours stop/kill requests and
/// records the final state.
fn watch(mut child: Child, shared: Arc<Shared>, log_path: PathBuf, waker: Waker) {
    let mut reader = File::open(&log_path).ok();
    let mut term_sent: Option<Instant> = None;
    let mut killed = false;

    loop {
        let produced = drain_log(&mut reader, &shared);

        if shared.stop.load(Ordering::Acquire) && term_sent.is_none() {
            set_state(&shared, TaskState::Stopping);
            if let Err(e) = request_stop(&mut child) {
                tracing::warn!(error = %e, "cannot signal the child, killing it");
                let _ = child.kill();
                killed = true;
            }
            term_sent = Some(Instant::now());
            waker();
        }
        let grace_expired = term_sent.is_some_and(|t| t.elapsed() > STOP_GRACE);
        if !killed && (shared.kill.load(Ordering::Acquire) || grace_expired) {
            tracing::warn!(grace_expired, "escalating to kill");
            let _ = child.kill();
            killed = true;
        }

        match child.try_wait() {
            Ok(Some(status)) => {
                drain_log(&mut reader, &shared);
                let stopped_by_user = shared.stop.load(Ordering::Acquire);
                let outcome = Outcome {
                    success: status.success() || stopped_by_user,
                    detail: describe(status, stopped_by_user, killed),
                    stopped_by_user,
                };
                set_state(&shared, TaskState::Finished(outcome));
                waker();
                return;
            }
            Ok(None) => {
                if produced {
                    waker();
                }
                std::thread::sleep(POLL);
            }
            Err(e) => {
                let outcome = Outcome {
                    success: false,
                    detail: format!("cannot wait for the child: {e}"),
                    stopped_by_user: shared.stop.load(Ordering::Acquire),
                };
                set_state(&shared, TaskState::Finished(outcome));
                waker();
                return;
            }
        }
    }
}

fn describe(status: std::process::ExitStatus, stopped_by_user: bool, killed: bool) -> String {
    if stopped_by_user {
        let how = if killed { "killed" } else { "stopped" };
        return format!("{how} on request ({status})");
    }
    match status.code() {
        Some(0) => "finished successfully".to_string(),
        Some(code) => format!("exited with status {code}"),
        None => format!("terminated ({status})"),
    }
}

fn set_state(shared: &Shared, state: TaskState) {
    *shared.state.lock_or_recover() = state;
}

/// Reads whatever the child appended to the log file since the last call.
/// Returns true when something new arrived.
fn drain_log(reader: &mut Option<File>, shared: &Shared) -> bool {
    let Some(file) = reader.as_mut() else {
        return false;
    };
    let mut buf = [0u8; 8192];
    let mut produced = false;
    loop {
        match file.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                produced = true;
                let text = String::from_utf8_lossy(&buf[..n]);
                shared.log.lock_or_recover().push_chunk(&text);
            }
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) => {
                tracing::debug!(error = %e, "log tail read failed");
                break;
            }
        }
    }
    produced
}

/// Puts the child in its own process group so a signal aimed at the manager
/// never reaches a running VM.
///
/// Without this, a child inherits the manager's group and dies with it whenever
/// something signals the whole group — closing the terminal that started the
/// manager (SIGHUP), Ctrl+C, or a supervisor killing the job. Verified the hard
/// way: killing the manager's process group took a running VM down with it.
#[cfg(unix)]
fn detach_process_group(command: &mut Command) {
    use std::os::unix::process::CommandExt as _;
    // 0 = "new group with the child's pid as leader".
    command.process_group(0);
}

#[cfg(windows)]
fn detach_process_group(command: &mut Command) {
    use std::os::windows::process::CommandExt as _;
    // CREATE_NEW_PROCESS_GROUP — the Windows equivalent: console control events
    // sent to the manager's group stop at the child.
    const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
    command.creation_flags(CREATE_NEW_PROCESS_GROUP);
}

#[cfg(not(any(unix, windows)))]
fn detach_process_group(_command: &mut Command) {}

/// Unix gets SIGTERM so `entangled run` can shut the VM down cleanly; Windows
/// has no signals, so `TerminateProcess` via `Child::kill` is the only option.
#[cfg(unix)]
fn request_stop(child: &mut Child) -> std::io::Result<()> {
    let pid = child.id() as libc::pid_t;
    // SAFETY: `kill` with a pid this process owns (the child has not been
    // reaped — `try_wait` has not yet reported an exit) and a valid signal
    // number; no memory is passed across the boundary.
    let rc = unsafe { libc::kill(pid, libc::SIGTERM) };
    if rc == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

#[cfg(not(unix))]
fn request_stop(child: &mut Child) -> std::io::Result<()> {
    child.kill()
}

#[cfg(test)]
mod tests {
    use super::*;
    // Every test that spawns a child is `cfg(unix)` (it needs a POSIX shell and
    // signals), so its helpers are too — otherwise a Windows build sees them as
    // dead, which `-D warnings` refuses.
    #[cfg(unix)]
    use std::path::Path;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "entangled-manager-tests/{tag}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("test temp dir");
        dir
    }

    #[cfg(unix)]
    fn wait_until(mut cond: impl FnMut() -> bool, what: &str) {
        let deadline = Instant::now() + Duration::from_secs(30);
        while Instant::now() < deadline {
            if cond() {
                return;
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        panic!("timed out waiting for {what}");
    }

    #[cfg(unix)]
    fn spec(dir: &Path, vm: &str, program: &str, args: &[&str]) -> TaskSpec {
        TaskSpec {
            kind: TaskKind::Run,
            vm: vm.to_string(),
            program: PathBuf::from(program),
            args: args.iter().map(|a| a.to_string()).collect(),
            cwd: dir.to_path_buf(),
            log_path: dir.join(format!("{vm}.log")),
        }
    }

    #[test]
    fn command_line_quotes_arguments_with_spaces() {
        let spec = TaskSpec {
            kind: TaskKind::Install,
            vm: "demo".into(),
            program: PathBuf::from("/usr/bin/entangled"),
            args: vec!["install".into(), "--name".into(), "two words".into()],
            cwd: PathBuf::from("/tmp"),
            log_path: PathBuf::from("/tmp/demo.log"),
        };
        assert_eq!(
            spec.command_line(),
            "/usr/bin/entangled install --name \"two words\""
        );
    }

    #[test]
    fn log_buffer_splits_lines_and_keeps_partials() {
        let mut log = LogBuffer::default();
        log.push_chunk("first\r\nsec");
        assert_eq!(log.lines.len(), 1);
        assert_eq!(log.lines[0].text, "first");
        assert_eq!(log.partial, "sec");
        log.push_chunk("ond\n");
        assert_eq!(log.lines[1].text, "second");
        assert!(log.partial.is_empty());
    }

    #[test]
    fn log_buffer_strips_ansi_colour_codes() {
        let mut log = LogBuffer::default();
        // Exactly what `tracing` writes for one CLI line.
        log.push_chunk(
            "\u{1b}[2m2026-08-18T22:31:12.670801Z\u{1b}[0m \u{1b}[32m INFO\u{1b}[0m \
             \u{1b}[2mentangled::run_vm\u{1b}[0m\u{1b}[2m:\u{1b}[0m attaching virtio-blk\n",
        );
        assert_eq!(
            log.lines[0].text,
            "2026-08-18T22:31:12.670801Z  INFO entangled::run_vm: attaching virtio-blk"
        );
    }

    #[test]
    fn log_buffer_handles_escapes_split_across_chunks() {
        let mut log = LogBuffer::default();
        log.push_chunk("red \u{1b}");
        log.push_chunk("[31");
        log.push_chunk("merror\u{1b}[0m done\n");
        assert_eq!(log.lines[0].text, "red error done");

        // OSC title sequences (BEL- and ST-terminated) vanish too.
        let mut log = LogBuffer::default();
        log.push_chunk("a\u{1b}]0;title\u{7}b\n");
        assert_eq!(log.lines[0].text, "ab");
    }

    #[test]
    fn log_buffer_is_bounded() {
        let mut log = LogBuffer::default();
        for i in 0..(LOG_CAPACITY + 10) {
            log.push_chunk(&format!("line {i}\n"));
        }
        assert_eq!(log.lines.len(), LOG_CAPACITY);
        assert_eq!(log.dropped, 10);
        assert_eq!(log.lines[0].text, "line 10");
    }

    #[test]
    fn unknown_program_is_a_typed_error() {
        let dir = temp_dir("proc-missing");
        let mut sup = Supervisor::new(no_waker());
        let spec = TaskSpec {
            kind: TaskKind::Run,
            vm: "ghost".into(),
            program: dir.join("definitely-not-here"),
            args: vec![],
            cwd: dir.clone(),
            log_path: dir.join("ghost.log"),
        };
        assert!(matches!(sup.spawn(spec), Err(ProcessError::Spawn { .. })));
        assert!(!sup.is_busy("ghost"));
    }

    #[test]
    fn unwritable_log_path_is_a_typed_error() {
        let dir = temp_dir("proc-log");
        let mut sup = Supervisor::new(no_waker());
        let mut spec = TaskSpec {
            kind: TaskKind::Run,
            vm: "nolog".into(),
            program: PathBuf::from("/bin/true"),
            args: vec![],
            cwd: dir.clone(),
            log_path: dir.join("missing-dir").join("nolog.log"),
        };
        spec.args.clear();
        assert!(matches!(sup.spawn(spec), Err(ProcessError::LogFile { .. })));
    }

    #[cfg(unix)]
    #[test]
    fn tracks_a_short_lived_child_and_captures_its_output() {
        let dir = temp_dir("proc-echo");
        let waker_hits = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let hits = Arc::clone(&waker_hits);
        let mut sup = Supervisor::new(Arc::new(move || {
            hits.fetch_add(1, Ordering::Relaxed);
        }));

        let id = sup
            .spawn(spec(
                &dir,
                "echoer",
                "/bin/sh",
                &["-c", "echo hello world; echo to-stderr 1>&2; exit 3"],
            ))
            .expect("spawn");
        wait_until(
            || {
                sup.task(id)
                    .is_some_and(|t| matches!(t.state(), TaskState::Finished(_)))
            },
            "child exit",
        );

        let task = sup.task(id).expect("task");
        let TaskState::Finished(outcome) = task.state() else {
            panic!("expected a finished task");
        };
        assert!(!outcome.success);
        assert!(!outcome.stopped_by_user);
        assert_eq!(outcome.detail, "exited with status 3");

        let (lines, dropped) = task.log_tail(100);
        assert_eq!(dropped, 0);
        assert!(lines.iter().any(|l| l == "hello world"), "lines: {lines:?}");
        assert!(lines.iter().any(|l| l == "to-stderr"), "lines: {lines:?}");
        assert!(waker_hits.load(Ordering::Relaxed) > 0, "waker was called");

        // The log file survives for post-mortem inspection.
        let on_disk = std::fs::read_to_string(dir.join("echoer.log")).expect("log file");
        assert!(on_disk.contains("hello world"));
    }

    /// The manager must be able to exit without taking VMs with it, so children
    /// live in their own process group (see `detach_process_group`).
    #[cfg(unix)]
    #[test]
    fn children_run_in_their_own_process_group() {
        let dir = temp_dir("proc-group");
        let mut sup = Supervisor::new(no_waker());
        // `sh -c 'echo $$ …'` prints the child's pid; its process group must
        // equal that pid rather than the test runner's group.
        let id = sup
            .spawn(spec(
                &dir,
                "grouped",
                "/bin/sh",
                &["-c", "ps -o pgid= -p $$"],
            ))
            .expect("spawn");
        wait_until(
            || {
                sup.task(id)
                    .is_some_and(|t| matches!(t.state(), TaskState::Finished(_)))
            },
            "child exit",
        );

        let (lines, _) = sup.task(id).expect("task").log_tail(10);
        let reported: i32 = lines
            .iter()
            .filter_map(|l| l.trim().parse::<i32>().ok())
            .next_back()
            .expect("child printed its process group");
        // SAFETY: getpgrp takes no arguments and cannot fail.
        let ours = unsafe { libc::getpgrp() };
        assert_ne!(
            reported, ours,
            "child shares the manager's process group; a group kill would take VMs down"
        );
    }

    #[cfg(unix)]
    #[test]
    fn stop_terminates_a_long_running_child() {
        let dir = temp_dir("proc-stop");
        let mut sup = Supervisor::new(no_waker());
        let id = sup
            .spawn(spec(&dir, "sleeper", "/bin/sleep", &["300"]))
            .expect("spawn");

        wait_until(|| sup.is_busy("sleeper"), "running state");
        assert_eq!(sup.active_kind("sleeper"), Some(TaskKind::Run));

        sup.task(id).expect("task").request_stop();
        wait_until(
            || {
                sup.task(id)
                    .is_some_and(|t| matches!(t.state(), TaskState::Finished(_)))
            },
            "stopped child",
        );

        let TaskState::Finished(outcome) = sup.task(id).expect("task").state() else {
            panic!("expected a finished task");
        };
        assert!(outcome.stopped_by_user);
        // A user-requested stop is not a failure, even though SIGTERM makes the
        // exit status non-zero.
        assert!(outcome.success);
        assert!(outcome.detail.starts_with("stopped on request"));
        assert!(!sup.is_busy("sleeper"));
    }

    #[cfg(unix)]
    #[test]
    fn kill_is_available_when_a_child_ignores_sigterm() {
        let dir = temp_dir("proc-kill");
        let mut sup = Supervisor::new(no_waker());
        let id = sup
            .spawn(spec(
                &dir,
                "stubborn",
                "/bin/sh",
                &["-c", "trap '' TERM; sleep 300"],
            ))
            .expect("spawn");
        wait_until(|| sup.is_busy("stubborn"), "running state");

        let task = sup.task(id).expect("task");
        task.request_stop();
        wait_until(
            || matches!(sup.task(id).map(Task::state), Some(TaskState::Stopping)),
            "stopping state",
        );
        assert!(sup.task(id).expect("task").stop_requested());
        sup.task(id).expect("task").request_kill();

        wait_until(
            || {
                sup.task(id)
                    .is_some_and(|t| matches!(t.state(), TaskState::Finished(_)))
            },
            "killed child",
        );
    }

    #[cfg(unix)]
    #[test]
    fn finished_outcomes_are_reported_once_and_then_pruned() {
        let dir = temp_dir("proc-drain");
        let mut sup = Supervisor::new(no_waker());
        for i in 0..3 {
            sup.spawn(spec(&dir, &format!("quick{i}"), "/bin/true", &[]))
                .expect("spawn");
        }
        wait_until(|| !sup.any_active(), "all children to exit");

        let finished = sup.drain_finished();
        assert_eq!(finished.len(), 3);
        assert!(finished.iter().all(|(_, _, o)| o.success));
        assert!(sup.drain_finished().is_empty(), "reported only once");

        sup.prune(1);
        assert_eq!(sup.tasks().len(), 1);
    }
}
