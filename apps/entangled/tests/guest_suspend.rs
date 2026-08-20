//! An installed Ubuntu is suspended to a file and comes back as the same
//! session ([ADR-0006](../../../docs/adr/0006-suspend-restore.md)).
//!
//! The bootstrap-guest test (`tests/suspend_restore.rs`) proves the vCPU came
//! back, because a counter that keeps counting cannot be faked. This one proves
//! something the bootstrap guest cannot: that a **whole distribution** — two
//! CPUs, systemd, a real filesystem behind UEFI firmware, a login session with
//! state in the shell — survives being written to disk and read back.
//!
//! Three things are asserted, and each one fails differently if the restore is
//! wrong:
//!
//! * **The same boot.** `uptime -s` is the wall-clock instant the kernel
//!   started, computed from the monotonic clock. If the machine had rebooted, or
//!   if the guest's timekeeping came back wrong, this moves.
//! * **The same login session.** The resumed console is at a *shell prompt*, not
//!   a login prompt, and `history` still holds the marker typed before the
//!   suspend. A guest that re-ran its getty would have neither.
//! * **It survives.** The resumed VM is asked to power off through systemd and
//!   ACPI, and does — which means the kernel is not merely alive but able to run
//!   its whole shutdown path minutes after being restored.
//!
//! `#[ignore]`d and self-skipping: it needs a hypervisor, the CloudHv firmware
//! and an **already installed** Ubuntu profile, which is what
//! `entangled install ubuntu` produces.
//!
//! ```bash
//! cargo test -p entangled --test guest_suspend -- --ignored --nocapture
//! ```
//!
//! `$ENTANGLED_REBOOT_PROFILE`, `$ENTANGLED_REBOOT_LOGIN` and
//! `$ENTANGLED_REBOOT_PASSWORD` override the search and the credentials, exactly
//! as in `tests/guest_reboot.rs`.

#![cfg(any(target_os = "linux", windows))]

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

mod common;
use common::strip_ansi;

const BIN: &str = env!("CARGO_BIN_EXE_entangled");

/// Firmware, GRUB and systemd to a login prompt. As generous as the reboot
/// test's, and for the same reason: a `[network]`-less profile waits out two
/// two-minute `systemd-networkd-wait-online` timeouts first.
const BOOT_DEADLINE: Duration = Duration::from_secs(8 * 60);

/// A restore of a 2 GiB guest, plus systemd noticing time moved.
const RESUME_DEADLINE: Duration = Duration::from_secs(3 * 60);

/// A shell prompt, a password prompt or a command's echo.
const PROMPT_DEADLINE: Duration = Duration::from_secs(90);

/// How long the suspend itself may take, including the memory dump.
const SUSPEND_DEADLINE: Duration = Duration::from_secs(5 * 60);

const LOGIN_PROMPT: &str = "login:";
const PASSWORD_PROMPT: &str = "assword:";

/// Typed into the shell before the suspend, and looked for in `history` after
/// it. The marker is deliberately not a word any boot message contains.
const HISTORY_MARKER: &str = "ENTANGLED_SUSPEND_WITNESS_42";

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("repo root")
        .to_path_buf()
}

fn credential(var: &str, default: &str) -> String {
    std::env::var(var).unwrap_or_else(|_| default.to_string())
}

/// The installed-Ubuntu profile, or `None` with a note on stderr.
fn profile() -> Option<PathBuf> {
    if let Some(explicit) = std::env::var_os("ENTANGLED_REBOOT_PROFILE") {
        let path = PathBuf::from(explicit);
        if path.is_file() {
            return Some(path);
        }
        eprintln!("skipping: $ENTANGLED_REBOOT_PROFILE is not a file");
        return None;
    }
    let home = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)?;
    let dir = home.join("entangled-vms");
    for name in ["e2e-ubuntu.toml", "ubuntu.toml", "desktop.toml"] {
        let candidate = dir.join(name);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    eprintln!(
        "skipping: no installed Ubuntu profile in {} — run `entangled install ubuntu --auto`, \
         or set $ENTANGLED_REBOOT_PROFILE",
        dir.display()
    );
    None
}

fn step(what: &str) {
    eprintln!("[guest_suspend] {what}");
}

fn save_console(tag: &str, console: &str) -> PathBuf {
    let path = std::env::temp_dir().join(format!(
        "entangled-guest-suspend-{tag}-{}.log",
        std::process::id()
    ));
    let _ = std::fs::write(&path, console);
    path
}

/// A running `entangled`, with its console accumulating in the background.
struct Vm {
    child: std::process::Child,
    console: Arc<Mutex<String>>,
    control: std::process::ChildStdin,
}

impl Vm {
    fn spawn(args: &[String]) -> Self {
        let mut child = Command::new(BIN)
            .args(args)
            .current_dir(repo_root())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .expect("spawn entangled");
        let control = child.stdin.take().expect("control channel");
        let stdout = child.stdout.take().expect("console");
        let console = Arc::new(Mutex::new(String::new()));
        {
            let console = Arc::clone(&console);
            std::thread::spawn(move || {
                use std::io::Read as _;
                let mut reader = stdout;
                let mut buffer = [0u8; 4096];
                while let Ok(read) = reader.read(&mut buffer) {
                    if read == 0 {
                        return;
                    }
                    let text = strip_ansi(&String::from_utf8_lossy(&buffer[..read]));
                    if let Ok(mut console) = console.lock() {
                        console.push_str(&text);
                    }
                }
            });
        }
        Self {
            child,
            console,
            control,
        }
    }

    fn console(&self) -> String {
        self.console.lock().map(|c| c.clone()).unwrap_or_default()
    }

    fn count(&self, needle: &str) -> usize {
        self.console().matches(needle).count()
    }

    /// Waits until `needle` has appeared **more** times than `baseline`. Always
    /// a baseline, never a total: the buffer holds the whole run, and a prompt's
    /// count is not a per-step number.
    fn wait_for_more(&mut self, needle: &str, baseline: usize, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        loop {
            if self.count(needle) > baseline {
                return true;
            }
            if self.child.try_wait().ok().flatten().is_some() {
                return self.count(needle) > baseline;
            }
            if Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(Duration::from_millis(250));
        }
    }

    fn type_line(&mut self, text: &str) {
        let _ = writeln!(self.control, "type {text}");
        let _ = self.control.flush();
    }

    fn control_line(&mut self, text: &str) {
        let _ = writeln!(self.control, "{text}");
        let _ = self.control.flush();
    }

    /// Waits for the process to end and returns everything it printed.
    fn wait_out(mut self, timeout: Duration) -> (bool, String) {
        let deadline = Instant::now() + timeout;
        let ended = loop {
            match self.child.try_wait() {
                Ok(Some(status)) => break status.success(),
                _ if Instant::now() >= deadline => {
                    let _ = self.child.kill();
                    break false;
                }
                _ => std::thread::sleep(Duration::from_millis(250)),
            }
        };
        std::thread::sleep(Duration::from_millis(300));
        (ended, self.console())
    }

    fn kill(mut self) -> String {
        let _ = self.child.kill();
        let _ = self.child.wait();
        std::thread::sleep(Duration::from_millis(200));
        self.console()
    }
}

/// Logs in on ttyS0 and returns once a shell prompt is up.
fn log_in(vm: &mut Vm, user: &str, password: &str, shell_prompt: &str) -> Result<(), String> {
    step("waiting for a login prompt");
    let seen = vm.count(LOGIN_PROMPT);
    if !vm.wait_for_more(LOGIN_PROMPT, seen, BOOT_DEADLINE) {
        return Err(format!("no login prompt within {BOOT_DEADLINE:?}"));
    }
    // A getty that has just started drops the first characters it is sent.
    std::thread::sleep(Duration::from_secs(2));
    step("logging in");
    let seen = vm.count(PASSWORD_PROMPT);
    vm.type_line(user);
    if !vm.wait_for_more(PASSWORD_PROMPT, seen, PROMPT_DEADLINE) {
        return Err("no password prompt".into());
    }
    let seen = vm.count(shell_prompt);
    vm.type_line(password);
    if !vm.wait_for_more(shell_prompt, seen, PROMPT_DEADLINE) {
        return Err("never reached a shell".into());
    }
    Ok(())
}

/// Runs `command` in the guest's shell and returns the line that follows the
/// echoed command — which is its output, for the one-line commands used here.
fn run(vm: &mut Vm, command: &str, shell_prompt: &str) -> Result<String, String> {
    let before = vm.console().len();
    let seen = vm.count(shell_prompt);
    vm.type_line(command);
    if !vm.wait_for_more(shell_prompt, seen, PROMPT_DEADLINE) {
        return Err(format!("`{command}` never came back to a prompt"));
    }
    let tail = vm.console()[before..].to_string();
    // The echo of the command comes first; everything between it and the next
    // prompt is the output.
    let after_echo = match tail.find(command) {
        Some(at) => &tail[at + command.len()..],
        None => &tail[..],
    };
    Ok(after_echo
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.contains(shell_prompt))
        .map(str::to_string)
        .collect::<Vec<_>>()
        .join("\n"))
}

/// **The acceptance.** An installed Ubuntu, suspended mid-session, comes back
/// as the same session and shuts down cleanly.
#[test]
#[ignore = "needs a hypervisor, the CloudHv firmware and an installed Ubuntu profile"]
fn an_installed_ubuntu_suspends_and_resumes_as_the_same_session() {
    let Some(profile) = profile() else { return };
    let user = credential("ENTANGLED_REBOOT_LOGIN", "entangled");
    let password = credential("ENTANGLED_REBOOT_PASSWORD", "entangled");
    let shell_prompt = format!("{user}@");
    let snapshot = std::env::temp_dir().join(format!(
        "entangled-guest-suspend-{}.esnap",
        std::process::id()
    ));
    let _ = std::fs::remove_file(&snapshot);
    eprintln!(
        "suspending {} to {}",
        profile.display(),
        snapshot.display()
    );
    let started = Instant::now();

    // ---------------------------------------------------------------- run it
    let mut vm = Vm::spawn(&[
        "run".into(),
        "--headless".into(),
        "--control-stdin".into(),
        "--snapshot".into(),
        snapshot.display().to_string(),
        profile.display().to_string(),
    ]);
    if let Err(error) = log_in(&mut vm, &user, &password, &shell_prompt) {
        let console = vm.kill();
        panic!(
            "{error}; console: {}",
            save_console("boot", &console).display()
        );
    }
    step("recording the session's identity");
    let boot_time = run(&mut vm, "uptime -s", &shell_prompt).expect("uptime -s");
    let uptime_before = guest_uptime(&mut vm, &shell_prompt);
    // A command whose only purpose is to be in `history` afterwards.
    let _ = run(&mut vm, &format!("echo {HISTORY_MARKER}"), &shell_prompt);
    eprintln!(
        "[guest_suspend] booted at {boot_time:?}, {uptime_before:.0}s of uptime, \
         after {:?}",
        started.elapsed()
    );
    assert!(
        !boot_time.is_empty(),
        "the guest did not answer `uptime -s`"
    );

    // -------------------------------------------------------------- suspend
    step("suspending");
    let suspend_started = Instant::now();
    vm.control_line("save");
    let (ended, console) = vm.wait_out(SUSPEND_DEADLINE);
    let suspend_took = suspend_started.elapsed();
    let saved = save_console("suspend", &console);
    assert!(
        ended,
        "the suspended VM did not exit cleanly; console: {}",
        saved.display()
    );
    let confirmation = console
        .lines()
        .find(|line| line.contains("entangled-control: saved"))
        .unwrap_or("")
        .to_string();
    assert!(
        !confirmation.is_empty(),
        "no save confirmation; console: {}",
        saved.display()
    );
    let bytes = std::fs::metadata(&snapshot).map(|m| m.len()).unwrap_or(0);
    eprintln!("[guest_suspend] {confirmation}");
    eprintln!(
        "[guest_suspend] suspended in {suspend_took:?}, {bytes} bytes on disk; console: {}",
        saved.display()
    );

    // --------------------------------------------------------------- resume
    step("resuming");
    let resume_started = Instant::now();
    let mut vm = Vm::spawn(&[
        "resume".into(),
        "--headless".into(),
        "--control-stdin".into(),
        snapshot.display().to_string(),
    ]);
    // A bare Enter: the resumed guest is sitting at a shell prompt it printed
    // before the suspend, so nothing new appears until it is poked.
    std::thread::sleep(Duration::from_secs(5));
    let seen = vm.count(&shell_prompt);
    vm.type_line("");
    let alive = vm.wait_for_more(&shell_prompt, seen, RESUME_DEADLINE);
    let resume_took = resume_started.elapsed();
    if !alive {
        let console = vm.kill();
        panic!(
            "the resumed guest never answered; console: {}",
            save_console("resume", &console).display()
        );
    }
    eprintln!("[guest_suspend] the resumed guest answered in {resume_took:?}");

    // ----------------------------------------------------- the same session
    let boot_time_after = run(&mut vm, "uptime -s", &shell_prompt).expect("uptime -s");
    let uptime_after = guest_uptime(&mut vm, &shell_prompt);
    let history = run(&mut vm, "history 20", &shell_prompt).unwrap_or_default();

    // A login prompt in the *resumed* process would mean the getty ran again.
    let logins_after_resume = vm.count(LOGIN_PROMPT);

    step("shutting down");
    let seen = vm.count(PASSWORD_PROMPT);
    vm.type_line("sudo poweroff");
    if vm.wait_for_more(PASSWORD_PROMPT, seen, Duration::from_secs(20)) {
        vm.type_line(&password);
    }
    let (shut_down, console) = vm.wait_out(Duration::from_secs(4 * 60));
    let saved = save_console("resumed", &console);

    eprintln!(
        "[guest_suspend] boot time before {boot_time:?}, after {boot_time_after:?}; \
         uptime {uptime_before:.0}s -> {uptime_after:.0}s; total {:?}; console: {}",
        started.elapsed(),
        saved.display()
    );

    assert_eq!(
        boot_time_after, boot_time,
        "the resumed guest is a different boot"
    );
    assert!(
        uptime_after >= uptime_before,
        "the resumed guest's uptime went backwards: {uptime_before} -> {uptime_after}"
    );
    assert_eq!(
        logins_after_resume, 0,
        "the resumed guest showed a login prompt, so the session did not survive; console: {}",
        saved.display()
    );
    assert!(
        history.contains(HISTORY_MARKER),
        "the shell history did not survive the suspend; `history 20` said:\n{history}"
    );
    assert!(
        shut_down,
        "the resumed guest could not shut down cleanly; console: {}",
        saved.display()
    );
    let _ = std::fs::remove_file(&snapshot);
}

/// `/proc/uptime`'s first field, in seconds.
fn guest_uptime(vm: &mut Vm, shell_prompt: &str) -> f64 {
    run(vm, "cat /proc/uptime", shell_prompt)
        .ok()
        .and_then(|line| {
            line.split_whitespace()
                .next()
                .and_then(|first| first.parse::<f64>().ok())
        })
        .unwrap_or(0.0)
}
