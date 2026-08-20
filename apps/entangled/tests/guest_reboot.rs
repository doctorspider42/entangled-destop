//! An installed Ubuntu reboots itself and comes back — twice, in one process
//! ([ADR-0005](../../../docs/adr/0005-vm-lifecycle.md)).
//!
//! This is the acceptance criterion the lifecycle work exists for, stated the
//! way a person would: *install updates, click Restart, and get your desktop
//! back*. Before it, the VM died instead.
//!
//! It is the only test that exercises the whole reboot path end to end, because
//! it is the only one whose guest is a real distribution behind real firmware:
//!
//! * the guest's `reboot` goes through systemd, the kernel and — because this
//!   is a UEFI machine — `efi_reboot()` into the firmware's `ResetSystem`,
//!   which for this host bridge is `IoWrite8 (0xCF9, BIT2|BIT1)`. Nothing in
//!   the bootstrap-kernel tests reaches that rung of the ladder.
//! * coming back means the *firmware* comes back: the PVH image is reloaded and
//!   re-entered, EDK2 re-reads its variable store out of the pflash device
//!   (which the machine reset deliberately does not clear), and its boot manager
//!   starts the same `Boot####` entry again. A reset that wiped NVRAM would
//!   boot to the EFI shell instead, which is exactly the failure this asserts
//!   against by counting firmware banners.
//!
//! Driven through the control channel (`--control-stdin`): the test logs in
//! over the serial console and types `sudo reboot`, as a person would.
//!
//! `#[ignore]`d and self-skipping: it needs a hypervisor, the CloudHv firmware
//! and an **already installed** Ubuntu profile with a serial getty — which is
//! what `entangled install ubuntu` produces (`tests/ubuntu_install.rs`).
//!
//! ```bash
//! # Linux/KVM, using ~/entangled-vms/ubuntu.toml
//! cargo test -p entangled --test guest_reboot -- --ignored --nocapture
//! ```
//! ```powershell
//! # Windows/WHP, using %USERPROFILE%\entangled-vms\e2e-ubuntu.toml
//! cargo test -p entangled --test guest_reboot -- --ignored --nocapture
//! ```
//!
//! `$ENTANGLED_REBOOT_PROFILE` overrides the search, and
//! `$ENTANGLED_REBOOT_LOGIN` / `$ENTANGLED_REBOOT_PASSWORD` the credentials
//! (they default to the ones `assets/autoinstall/ubuntu-server.yaml` seeds).

#![cfg(any(target_os = "linux", windows))]

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

mod common;
use common::strip_ansi;

const BIN: &str = env!("CARGO_BIN_EXE_entangled");

/// Firmware (~3 s) + GRUB (~4 s) + systemd to a login prompt. Generous: the
/// `[network]`-less profile an install writes makes `systemd-networkd-wait-online`
/// and `cloud-init-network` each wait out a two-minute timeout before the
/// prompt appears (ADR-0002 phase 5 records the same cost).
const BOOT_DEADLINE: Duration = Duration::from_secs(8 * 60);

/// How long a shell prompt, a password prompt or an echo may take.
const PROMPT_DEADLINE: Duration = Duration::from_secs(90);

/// `agetty` prints `<hostname> login:`.
const LOGIN_PROMPT: &str = "login:";

/// The one word both the getty's and `sudo`'s password prompts contain. Matched
/// rather than either full prompt because `sudo`'s wording is not stable across
/// releases (`[sudo] password for x:` became `[sudo: authenticate] Password:`).
const PASSWORD_PROMPT: &str = "assword:";

/// How long `sudo` gets to decide whether to ask. Short: if it has not asked by
/// now it is not going to, and the reboot is already under way.
const SUDO_DEADLINE: Duration = Duration::from_secs(20);

/// EDK2's boot manager announcing the entry it is starting. Counting these is
/// how the test knows the *firmware* ran again rather than the kernel merely
/// having been re-entered.
const FIRMWARE_MARKER: &str = "BdsDxe: starting Boot";

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("repo root")
        .to_path_buf()
}

/// The installed-Ubuntu profile to reboot, or `None` with a note on stderr.
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

fn credential(var: &str, default: &str) -> String {
    std::env::var(var).unwrap_or_else(|_| default.to_string())
}

/// One line per step, so a run that fails after twelve minutes says *where*.
fn step(boot: usize, what: &str) {
    eprintln!("[guest_reboot] boot {boot}: {what}");
}

/// Writes the whole console somewhere a failure message can point at. A tail is
/// never enough for a boot log — the interesting line is usually the one before
/// the guest went quiet, thousands of lines up.
fn save_console(console: &str) -> PathBuf {
    let path =
        std::env::temp_dir().join(format!("entangled-guest-reboot-{}.log", std::process::id()));
    let _ = std::fs::write(&path, console);
    path
}

/// A running `entangled run --control-stdin`, with its console accumulating in
/// the background and its control channel open.
struct Vm {
    child: std::process::Child,
    console: Arc<Mutex<String>>,
    control: std::process::ChildStdin,
}

impl Vm {
    fn start(profile: &Path) -> Option<Self> {
        let firmware = repo_root().join("artifacts/firmware/CLOUDHV.fd");
        if !firmware.is_file() {
            eprintln!(
                "skipping: {} is missing — run guest/firmware/build-cloudhv.sh",
                firmware.display()
            );
            return None;
        }
        let mut child = Command::new(BIN)
            .args([
                "run",
                "--headless",
                "--control-stdin",
                &profile.display().to_string(),
            ])
            .current_dir(repo_root())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            // stderr is `tracing`, which carries the host's own account of the
            // reset; kept out of the console buffer so marker counts stay
            // guest-only.
            .stderr(Stdio::inherit())
            .spawn()
            .expect("spawn entangled run");

        let control = child.stdin.take().expect("control channel");
        let stdout = child.stdout.take().expect("console");
        let console = Arc::new(Mutex::new(String::new()));
        {
            // A byte stream, not text, and coloured — see `common::strip_ansi`
            // for the two ways that has cost this project an acceptance run.
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
        Some(Self {
            child,
            console,
            control,
        })
    }

    fn console(&self) -> String {
        self.console.lock().map(|c| c.clone()).unwrap_or_default()
    }

    fn count(&self, needle: &str) -> usize {
        self.console().matches(needle).count()
    }

    /// Waits until `needle` has appeared at least `times` times.
    fn wait_for(&mut self, needle: &str, times: usize, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        loop {
            if self.count(needle) >= times {
                return true;
            }
            if self.child.try_wait().ok().flatten().is_some() {
                return false; // the VM ended — never what this test wants
            }
            if Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(Duration::from_millis(250));
        }
    }

    /// Types a line on the guest's serial console.
    fn type_line(&mut self, text: &str) {
        let _ = writeln!(self.control, "type {text}");
        let _ = self.control.flush();
    }

    fn stop(mut self) -> String {
        let _ = self.child.kill();
        let _ = self.child.wait();
        std::thread::sleep(Duration::from_millis(100));
        self.console()
    }
}

fn tail(text: &str, lines: usize) -> String {
    let all: Vec<&str> = text.lines().collect();
    all[all.len().saturating_sub(lines)..].join("\n")
}

/// Log in on ttyS0 and ask the system to restart.
///
/// Typed rather than scripted into the image: the point is that the *guest*
/// initiates the reboot through its own software stack, which is the path a
/// person clicking Restart takes.
fn log_in_and_reboot(vm: &mut Vm, boot: usize, user: &str, password: &str) -> Result<(), String> {
    step(boot, "waiting for a login prompt");
    if !vm.wait_for(LOGIN_PROMPT, boot, BOOT_DEADLINE) {
        return Err(format!(
            "boot {boot}: no login prompt within {BOOT_DEADLINE:?}"
        ));
    }
    step(boot, "logging in");
    // A getty that has just started can drop the first characters; a moment's
    // pause costs nothing next to an eight-minute boot.
    std::thread::sleep(Duration::from_secs(2));
    vm.type_line(user);
    // Each boot contributes two password prompts: the getty's, and `sudo`'s
    // below. Counting them is how the test tells this boot's prompts from the
    // previous boot's, which are still in the same console buffer.
    if !vm.wait_for(PASSWORD_PROMPT, 2 * boot - 1, PROMPT_DEADLINE) {
        return Err(format!("boot {boot}: no login password prompt"));
    }
    vm.type_line(password);
    // The shell's own prompt: `user@host:~$`. Matching on the username avoids
    // the many `$` that appear in a systemd boot.
    let shell_prompt = format!("{user}@");
    if !vm.wait_for(&shell_prompt, boot, PROMPT_DEADLINE) {
        return Err(format!("boot {boot}: never reached a shell"));
    }
    step(boot, "asking the guest to reboot");
    vm.type_line("sudo reboot");
    // `sudo` asks unless it has been used very recently, and it does not always
    // spell the prompt the same way — Ubuntu 26.04's is
    // `[sudo: authenticate] Password:`, older ones `[sudo] password for …`. Both
    // end in the one word every such prompt contains, which is what is matched.
    // Answering a prompt that never came would type the password at a shell, so
    // this waits for the count to move rather than sending it blind.
    if vm.wait_for(PASSWORD_PROMPT, 2 * boot, SUDO_DEADLINE) {
        vm.type_line(password);
    }
    Ok(())
}

/// The acceptance: `reboot` inside an installed Ubuntu brings it back to a
/// login prompt, in the same process, twice in a row.
#[test]
#[ignore = "needs a hypervisor, the CloudHv firmware and an installed Ubuntu profile"]
fn an_installed_ubuntu_reboots_itself_and_comes_back_twice() {
    let Some(profile) = profile() else { return };
    eprintln!("rebooting {}", profile.display());
    let user = credential("ENTANGLED_REBOOT_LOGIN", "entangled");
    let password = credential("ENTANGLED_REBOOT_PASSWORD", "entangled");

    let Some(mut vm) = Vm::start(&profile) else {
        return;
    };
    let started = Instant::now();
    let mut errors = Vec::new();
    for boot in 1..=2usize {
        if let Err(error) = log_in_and_reboot(&mut vm, boot, &user, &password) {
            errors.push(error);
            break;
        }
        eprintln!(
            "boot {boot}: asked the guest to reboot at {:?}",
            started.elapsed()
        );
    }
    // The third login prompt is the evidence: it can only exist if both reboots
    // came all the way back.
    let came_back = errors.is_empty() && vm.wait_for(LOGIN_PROMPT, 3, BOOT_DEADLINE);
    let firmware_runs = vm.count(FIRMWARE_MARKER);
    let grub_runs = vm.count("GNU GRUB");
    let logins = vm.count(LOGIN_PROMPT);
    let console = vm.stop();

    let saved = save_console(&console);
    eprintln!(
        "[guest_reboot] {logins} login prompts, {firmware_runs} firmware boot-manager runs, \
         {grub_runs} GRUB banners, in {:?}; full console: {}",
        started.elapsed(),
        saved.display()
    );
    assert!(
        errors.is_empty(),
        "{}; console at {}; tail:\n{}",
        errors.join("; "),
        saved.display(),
        tail(&console, 40)
    );
    assert!(
        came_back,
        "the guest did not come back to a login prompt 3 times: {logins} seen; \
         console at {}; tail:\n{}",
        saved.display(),
        tail(&console, 60)
    );
    // Through the firmware, not merely a kernel restart: EDK2's boot manager
    // ran three times, off a variable store the reset did not clear.
    assert!(
        firmware_runs >= 3,
        "the firmware's boot manager ran {firmware_runs}× for 3 boots — a reboot that skipped \
         the firmware, or an NVRAM store the reset wiped; console tail:\n{}",
        tail(&console, 60)
    );
    assert!(
        grub_runs >= 3,
        "GRUB ran {grub_runs}× for 3 boots; console tail:\n{}",
        tail(&console, 60)
    );
}
