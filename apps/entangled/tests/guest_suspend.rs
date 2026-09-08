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
//! * **The same boot.** `/proc/sys/kernel/random/boot_id` is a UUID the kernel
//!   generates once per boot. It cannot coincide, and — unlike `uptime -s` — it
//!   is not affected by the wall clock moving, which across a suspend it does by
//!   design. `/proc/uptime` beside it proves the monotonic clock carried on
//!   rather than restarting.
//! * **The same login session.** The resumed console is at a *shell prompt*, not
//!   a login prompt; the shell answering has the same pid; and `history` still
//!   holds the marker typed before the suspend. A guest that re-ran its getty
//!   would have none of the three.
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

/// Prefix of the guest's identity line.
///
/// Split across a `""` in the command that produces it, so the **echo** of the
/// command reads `SUSPEND""PROBE` and only the command's *output* reads
/// `SUSPENDPROBE`. Without that, a reader looking for the answer finds the
/// question first.
const PROBE_TAG: &str = "SUSPENDPROBE";

/// The command that prints it.
///
/// `boot_id` rather than `uptime -s`: it is a UUID the kernel generates once per
/// boot, so it cannot coincide, and it is not affected by the wall clock moving
/// (which it does across a suspend, by design). `/proc/uptime` beside it proves
/// the monotonic clock carried on rather than restarting.
const PROBE_COMMAND: &str = concat!(
    "echo \"SUSPEND\"\"PROBE",
    " boot=$(cat /proc/sys/kernel/random/boot_id)",
    " up=$(cut -d\" \" -f1 /proc/uptime)",
    " shell=$$\""
);

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

/// Runs `command` in the guest's shell and returns everything the console
/// gained, prompt decorations and all.
///
/// Deliberately *not* "the output": a modern bash wraps every prompt and every
/// command in OSC escape sequences (Ubuntu 26.04 emits `OSC 3008` with a fresh
/// UUID per command), so anything that tried to isolate one line would be
/// comparing shell bookkeeping. The callers look for a tagged marker inside the
/// text instead, which is immune to all of it.
fn run(vm: &mut Vm, command: &str, shell_prompt: &str) -> Result<String, String> {
    let before = vm.console().len();
    let seen = vm.count(shell_prompt);
    vm.type_line(command);
    if !vm.wait_for_more(shell_prompt, seen, PROMPT_DEADLINE) {
        return Err(format!("`{command}` never came back to a prompt"));
    }
    Ok(vm.console()[before..].to_string())
}

/// The guest's identity: its boot id, its monotonic uptime and the pid of the
/// shell that is answering.
#[derive(Debug, Clone, PartialEq)]
struct Probe {
    boot_id: String,
    uptime: f64,
    shell_pid: String,
}

fn probe(vm: &mut Vm, shell_prompt: &str) -> Result<Probe, String> {
    let text = run(vm, PROBE_COMMAND, shell_prompt)?;
    // The *last* untagged occurrence: the echo of the command carries
    // `SUSPEND""PROBE`, which does not match.
    let line = text
        .lines()
        .rfind(|line| line.contains(PROBE_TAG) && !line.contains("\"\""))
        .ok_or_else(|| format!("the guest did not answer the probe; console said:\n{text}"))?;
    let field = |key: &str| -> Option<String> {
        line.split_whitespace()
            .find_map(|word| word.strip_prefix(key))
            .map(|value| value.trim_end_matches(['"', '\r']).to_string())
    };
    Ok(Probe {
        boot_id: field("boot=").ok_or("no boot id in the probe line")?,
        uptime: field("up=")
            .and_then(|value| value.parse().ok())
            .ok_or("no uptime in the probe line")?,
        shell_pid: field("shell=").ok_or("no shell pid in the probe line")?,
    })
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
    eprintln!("suspending {} to {}", profile.display(), snapshot.display());
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
    let before = match probe(&mut vm, &shell_prompt) {
        Ok(before) => before,
        Err(error) => {
            let console = vm.kill();
            panic!(
                "{error}; console: {}",
                save_console("probe", &console).display()
            );
        }
    };
    // A command whose only purpose is to be in `history` afterwards.
    let _ = run(&mut vm, &format!("echo {HISTORY_MARKER}"), &shell_prompt);
    eprintln!(
        "[guest_suspend] boot {} , {:.0}s of uptime, shell pid {}, after {:?}",
        before.boot_id,
        before.uptime,
        before.shell_pid,
        started.elapsed()
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
    let after = match probe(&mut vm, &shell_prompt) {
        Ok(after) => after,
        Err(error) => {
            let console = vm.kill();
            panic!(
                "{error}; console: {}",
                save_console("resumed-probe", &console).display()
            );
        }
    };
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
        "[guest_suspend] boot {} -> {}, shell pid {} -> {}, uptime {:.0}s -> {:.0}s; \
         total {:?}; console: {}",
        before.boot_id,
        after.boot_id,
        before.shell_pid,
        after.shell_pid,
        before.uptime,
        after.uptime,
        started.elapsed(),
        saved.display()
    );

    assert_eq!(
        after.boot_id, before.boot_id,
        "the resumed guest is a different boot: its kernel generated a new boot id"
    );
    assert_eq!(
        after.shell_pid, before.shell_pid,
        "the resumed guest is answering from a different shell, so the session did not survive"
    );
    assert!(
        after.uptime >= before.uptime,
        "the resumed guest's monotonic clock went backwards: {} -> {}",
        before.uptime,
        after.uptime
    );
    assert_eq!(
        logins_after_resume,
        0,
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
