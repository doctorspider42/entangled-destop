//! A running guest is written to a file and comes back out of it
//! ([ADR-0006](../../../docs/adr/0006-suspend-restore.md)).
//!
//! The acceptance criterion, stated the way a person would: *freeze the VM,
//! close it, open it again, and be where you left off.* What makes that
//! checkable rather than plausible is the bootstrap guest's heartbeat probe
//! (`entangled.heartbeat=<ms>`), which prints a **monotonic counter** from a
//! real timer loop. So:
//!
//! * the counter continuing — 8, then 9 — is proof the *vCPU* came back, not
//!   merely that a guest booted. A restored machine whose registers, MSRs or
//!   local APIC were subtly wrong does not carry on counting; it faults, or it
//!   goes quiet.
//! * the ready marker **not** appearing again is proof it is the same boot.
//! * the guest printing at all is proof of something that took a real
//!   measurement to find: on KVM the IOAPIC lives in the kernel, and a restore
//!   that forgot it comes back with every pin masked. The GPU keeps drawing
//!   (MSI-X bypasses the IOAPIC) and the serial console never speaks again.
//!
//! End to end through the real binary — `entangled run --control-stdin`, the
//! `save` command, then `entangled resume` — because half of what suspend is
//! is the plumbing: the control channel, the metadata, the profile that travels
//! inside the file.
//!
//! The refusal tests share the same snapshot: a file that took four seconds to
//! write is corrupted six ways in memory and offered back to `entangled
//! resume`, which must decline each one by name.
//!
//! Self-skipping without a hypervisor or the guest artifacts, like every other
//! integration test here.
//!
//! ```bash
//! cargo test -p entangled --test suspend_restore -- --nocapture
//! ```

#![cfg(any(target_os = "linux", windows))]

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

mod common;
use common::strip_ansi;

const BIN: &str = env!("CARGO_BIN_EXE_entangled");

/// The bootstrap kernel reaches its marker in ~3 s here; a debug-build restore
/// of a 256 MiB guest adds a couple of seconds of memory copy.
const DEADLINE: Duration = Duration::from_secs(90);

/// Heartbeat period. Fast enough that a handful arrive inside a few seconds,
/// slow enough that the console is readable.
const HEARTBEAT_MS: u64 = 200;

/// How many heartbeats to see before suspending. More than one, so "it
/// continued" is a statement about a *sequence*.
const HEARTBEATS_BEFORE: usize = 4;

const HEARTBEAT: &str = "VMHOST_HEARTBEAT";
const READY: &str = "VMHOST_GUEST_READY";

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("repo root")
        .to_path_buf()
}

/// Kernel and initramfs, or `None` with a note.
fn artifacts() -> Option<(PathBuf, PathBuf)> {
    let root = repo_root();
    let kernel = ["artifacts/bootstrap/vmlinuz", "artifacts/tests/vmlinuz"]
        .iter()
        .map(|p| root.join(p))
        .find(|p| p.is_file());
    let initramfs = root.join("artifacts/tests/test-initramfs.cpio.gz");
    match (kernel, initramfs.is_file()) {
        (Some(kernel), true) => Some((kernel, initramfs)),
        _ => {
            eprintln!(
                "skipping: guest artifacts missing — run scripts/fetch-test-kernel.sh and \
                 scripts/build-test-initramfs.sh"
            );
            None
        }
    }
}

/// True when a VM can actually be created here.
fn hypervisor_available() -> bool {
    #[cfg(target_os = "linux")]
    {
        if !Path::new("/dev/kvm").exists() {
            eprintln!("skipping: /dev/kvm is not present");
            return false;
        }
        true
    }
    #[cfg(windows)]
    {
        // `entangled doctor` is the cheap probe; a WHP-less host fails it.
        match Command::new(BIN).arg("doctor").output() {
            Ok(out) if out.status.success() => true,
            _ => {
                eprintln!("skipping: the Windows Hypervisor Platform is not available");
                false
            }
        }
    }
}

/// A scratch directory that goes away with the test.
struct Scratch(PathBuf);

impl Scratch {
    fn new(tag: &str) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "entangled-suspend-{tag}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("scratch dir");
        Self(dir)
    }

    fn path(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Writes a profile for the heartbeat guest, with an optional disk.
///
/// The user-mode NAT is attached on purpose even though this guest never sends
/// a packet: virtio-net is the device whose *host* side cannot survive a
/// suspend — its flows live in sockets the restoring process will not have — so
/// the acceptance run should carry one across rather than leave that path to
/// the unit tests. It needs no host privileges on either host, and it puts a
/// device with a **worker thread of its own** in the snapshot, which is the
/// interesting case for the pause gate.
fn profile(scratch: &Scratch, kernel: &Path, initramfs: &Path, disk: Option<&Path>) -> PathBuf {
    let mut text = format!(
        "name = \"suspend-probe\"\n\
         memory_mib = 256\n\
         vcpus = 1\n\
         transport = \"pci\"\n\n\
         [boot]\n\
         mode = \"direct-linux\"\n\
         kernel = {kernel:?}\n\
         initramfs = {initramfs:?}\n\
         cmdline = \"console=ttyS0 panic=1 reboot=k entangled.heartbeat={HEARTBEAT_MS}\"\n\n\
         [network]\n\
         backend = \"usernet\"\n\n\
         [display]\n\
         width = 640\n\
         height = 480\n"
    );
    if let Some(disk) = disk {
        text.push_str(&format!("\n[[disk]]\npath = {disk:?}\n"));
    }
    let path = scratch.path("vm.toml");
    std::fs::write(&path, text).expect("write profile");
    path
}

/// A running `entangled`, with its console accumulating in the background.
struct Vm {
    child: std::process::Child,
    console: Arc<Mutex<String>>,
    control: Option<std::process::ChildStdin>,
}

impl Vm {
    fn spawn(args: &[&str]) -> Self {
        let mut child = Command::new(BIN)
            .args(args)
            .current_dir(repo_root())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .expect("spawn entangled");
        let control = child.stdin.take();
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

    /// Waits until `needle` has appeared at least `times` times, the child has
    /// ended, or the deadline expires.
    fn wait_for(&mut self, needle: &str, times: usize) -> bool {
        let deadline = Instant::now() + DEADLINE;
        loop {
            if self.count(needle) >= times {
                return true;
            }
            if self.child.try_wait().ok().flatten().is_some() {
                return self.count(needle) >= times;
            }
            if Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    fn send(&mut self, line: &str) {
        if let Some(control) = self.control.as_mut() {
            let _ = writeln!(control, "{line}");
            let _ = control.flush();
        }
    }

    /// Waits for the process to end, and returns everything it printed.
    fn wait_out(mut self) -> (bool, String) {
        let deadline = Instant::now() + DEADLINE;
        let ended = loop {
            match self.child.try_wait() {
                Ok(Some(status)) => break status.success(),
                _ if Instant::now() >= deadline => {
                    let _ = self.child.kill();
                    break false;
                }
                _ => std::thread::sleep(Duration::from_millis(50)),
            }
        };
        // Let the reader thread drain the pipe's tail.
        std::thread::sleep(Duration::from_millis(200));
        (ended, self.console())
    }

    fn kill(mut self) -> String {
        let _ = self.child.kill();
        let _ = self.child.wait();
        std::thread::sleep(Duration::from_millis(100));
        self.console()
    }
}

/// Every heartbeat number the console carries, in order.
fn heartbeats(console: &str) -> Vec<u64> {
    console
        .lines()
        .filter_map(|line| line.trim().strip_prefix(HEARTBEAT))
        // Only the first token: the line carries the guest's own uptime after
        // the counter (the soak's drift measurement), and a reader of the
        // counter must not care how many fields follow it.
        .filter_map(|rest| rest.split_ascii_whitespace().next()?.parse::<u64>().ok())
        .collect()
}

fn tail(text: &str, lines: usize) -> String {
    let all: Vec<&str> = text.lines().collect();
    all[all.len().saturating_sub(lines)..].join("\n")
}

/// Runs the guest, suspends it, and returns `(snapshot path, last heartbeat)`.
fn suspend_once(scratch: &Scratch, config: &Path, snapshot: &Path) -> u64 {
    let mut vm = Vm::spawn(&[
        "run",
        "--headless",
        "--control-stdin",
        "--snapshot",
        &snapshot.display().to_string(),
        &config.display().to_string(),
    ]);
    let _ = scratch;
    assert!(
        vm.wait_for(HEARTBEAT, HEARTBEATS_BEFORE),
        "the guest never produced {HEARTBEATS_BEFORE} heartbeats:\n{}",
        tail(&vm.console(), 40)
    );
    vm.send("save");
    let (ok, console) = vm.wait_out();
    assert!(
        ok,
        "the suspended VM did not exit cleanly:\n{}",
        tail(&console, 40)
    );
    assert!(
        console.contains("entangled-control: saved"),
        "no save confirmation on the control channel:\n{}",
        tail(&console, 40)
    );
    assert!(snapshot.is_file(), "no snapshot at {}", snapshot.display());

    let before = heartbeats(&console);
    let last = *before.last().expect("at least one heartbeat");
    assert_eq!(
        before,
        (0..=last).collect::<Vec<_>>(),
        "the guest skipped a heartbeat before it was suspended"
    );
    // The control channel's own summary carries the timing and the memory
    // ratio — the numbers ADR-0006 records.
    let summary = console
        .lines()
        .find(|line| line.contains("entangled-control: saved"))
        .unwrap_or("")
        .trim()
        .to_string();
    eprintln!(
        "[suspend] {} bytes on disk, last heartbeat {last} — {summary}",
        std::fs::metadata(snapshot).map(|m| m.len()).unwrap_or(0)
    );
    last
}

/// **The acceptance test.** Suspend a running guest, resume it, and watch the
/// counter carry on.
#[test]
fn a_suspended_guest_resumes_on_the_next_heartbeat() {
    let Some((kernel, initramfs)) = artifacts() else {
        return;
    };
    if !hypervisor_available() {
        return;
    }
    let scratch = Scratch::new("resume");
    let config = profile(&scratch, &kernel, &initramfs, None);
    let snapshot = scratch.path("vm.esnap");
    let last = suspend_once(&scratch, &config, &snapshot);

    let resume_started = Instant::now();
    let mut vm = Vm::spawn(&["resume", "--headless", &snapshot.display().to_string()]);
    assert!(
        vm.wait_for(HEARTBEAT, 1),
        "the resumed guest never printed a heartbeat:\n{}",
        tail(&vm.console(), 40)
    );
    // Process start to the guest's first line: machine assembly, the memory
    // read, and the vCPU running again.
    let to_first_beat = resume_started.elapsed();
    assert!(
        vm.wait_for(HEARTBEAT, 3),
        "the resumed guest stopped after one heartbeat:\n{}",
        tail(&vm.console(), 40)
    );
    let console = vm.kill();
    let after = heartbeats(&console);

    assert_eq!(
        after.first().copied(),
        Some(last + 1),
        "the resumed guest restarted its counter instead of continuing it \
         (saved at {last}, came back at {:?})",
        after.first()
    );
    assert!(
        after.windows(2).all(|w| w[1] == w[0] + 1),
        "the resumed guest's heartbeats are not consecutive: {after:?}"
    );
    assert_eq!(
        console.matches(READY).count(),
        0,
        "the resumed guest booted again instead of continuing:\n{}",
        tail(&console, 40)
    );
    eprintln!(
        "[resume] first guest line {to_first_beat:?} after launch; continued at {} and ran to {}",
        after[0],
        after.last().copied().unwrap_or(0)
    );
}

/// **The second snapshot must carry the guest's *later* memory.**
///
/// The bug this exists for is the one a dirty-page scheme would have and a full
/// save would not: a second suspend that wrote only what some log said had
/// changed, and missed a page. The symptom is not a crash — it is a guest that
/// comes back from the *second* file where it was at the *first*, or worse,
/// half at each.
///
/// So: suspend at heartbeat *a*, resume, let the counter run on, suspend again
/// at *b*, and resume the second file. What comes out must continue from `b`,
/// not from `a`, and the gap must be a real one — the guest has to have run
/// long enough between the two saves that `b` is well past `a`, or "it
/// continued" would be true of the earlier file too.
///
/// The heartbeat is what makes this checkable without knowing anything about
/// the guest's memory: the counter lives in the guest's own RAM, it is written
/// by the guest (so no host write can carry it), and it only ever goes up.
///
/// It also exercises the loop the manager offers — resume, work, suspend
/// again — end to end, which nothing else here did.
#[test]
fn a_second_suspend_carries_the_later_memory() {
    let Some((kernel, initramfs)) = artifacts() else {
        return;
    };
    if !hypervisor_available() {
        return;
    }
    let scratch = Scratch::new("later");
    let config = profile(&scratch, &kernel, &initramfs, None);
    let first = scratch.path("first.esnap");
    let second = scratch.path("second.esnap");

    let a = suspend_once(&scratch, &config, &first);

    // Resume the first file and run on, saving to a *different* path so both
    // snapshots survive to be compared.
    let mut vm = Vm::spawn(&[
        "resume",
        "--headless",
        "--control-stdin",
        "--save-to",
        &second.display().to_string(),
        &first.display().to_string(),
    ]);
    // Well past the first save: enough heartbeats that no ambiguity remains
    // about which file a resumed guest came out of.
    assert!(
        vm.wait_for(HEARTBEAT, HEARTBEATS_BEFORE * 3),
        "the resumed guest did not keep counting:\n{}",
        tail(&vm.console(), 40)
    );
    vm.send("save");
    let (ok, console) = vm.wait_out();
    assert!(
        ok,
        "the second suspend did not exit cleanly:\n{}",
        tail(&console, 40)
    );
    let middle = heartbeats(&console);
    let b = *middle.last().expect("heartbeats between the two saves");
    assert!(
        b > a + 2,
        "the guest barely moved between the two saves ({a} then {b}), so this test \
         would pass on a snapshot that carried the earlier memory"
    );
    assert!(second.is_file(), "no second snapshot");

    // The moment of truth: the second file, restored.
    let mut vm = Vm::spawn(&["resume", "--headless", &second.display().to_string()]);
    assert!(
        vm.wait_for(HEARTBEAT, 2),
        "the twice-suspended guest never printed a heartbeat:\n{}",
        tail(&vm.console(), 40)
    );
    let console = vm.kill();
    let after = heartbeats(&console);
    assert_eq!(
        after.first().copied(),
        Some(b + 1),
        "the second snapshot restored the guest's memory as of the FIRST save \
         (saved at {a}, ran to {b}, came back at {:?})",
        after.first()
    );
    assert_eq!(
        console.matches(READY).count(),
        0,
        "the guest booted again instead of continuing:\n{}",
        tail(&console, 40)
    );

    // And the first file is still the first file: a second save must not have
    // reached back into it.
    let mut vm = Vm::spawn(&["resume", "--headless", &first.display().to_string()]);
    assert!(
        vm.wait_for(HEARTBEAT, 2),
        "the first snapshot stopped working"
    );
    let console = vm.kill();
    assert_eq!(
        heartbeats(&console).first().copied(),
        Some(a + 1),
        "resuming the first snapshot no longer gives the guest as it was then"
    );
    eprintln!(
        "[later memory] saved at {a}, ran to {b}, second file resumed at {}",
        b + 1
    );
}

/// Resuming twice from the same file gives the same guest twice: a snapshot is
/// read-only, and restoring one does not consume it.
#[test]
fn a_snapshot_can_be_resumed_more_than_once() {
    let Some((kernel, initramfs)) = artifacts() else {
        return;
    };
    if !hypervisor_available() {
        return;
    }
    let scratch = Scratch::new("twice");
    let config = profile(&scratch, &kernel, &initramfs, None);
    let snapshot = scratch.path("vm.esnap");
    let last = suspend_once(&scratch, &config, &snapshot);

    for round in 1..=2 {
        let mut vm = Vm::spawn(&["resume", "--headless", &snapshot.display().to_string()]);
        assert!(
            vm.wait_for(HEARTBEAT, 2),
            "round {round}: no heartbeat:\n{}",
            tail(&vm.console(), 30)
        );
        let console = vm.kill();
        assert_eq!(
            heartbeats(&console).first().copied(),
            Some(last + 1),
            "round {round} did not start where the snapshot did"
        );
    }
}

// ------------------------------------------------------------- the refusals

/// Offers `bytes` to `entangled resume` and returns its combined output.
fn refuse(scratch: &Scratch, name: &str, bytes: &[u8]) -> String {
    let path = scratch.path(name);
    std::fs::write(&path, bytes).expect("write the damaged snapshot");
    let out = Command::new(BIN)
        .args(["resume", "--headless", &path.display().to_string()])
        .current_dir(repo_root())
        .output()
        .expect("run entangled resume");
    assert!(
        !out.status.success(),
        "{name}: entangled resume accepted a snapshot it should have refused"
    );
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    eprintln!(
        "[refusal] {name}: {}",
        text.trim().lines().next_back().unwrap_or("")
    );
    text
}

/// Every way a snapshot can be wrong is a refusal that says which way.
///
/// One suspend, six corruptions of its bytes. They share a VM because taking
/// the snapshot is the slow part and none of these care what is inside it.
#[test]
fn a_damaged_or_foreign_snapshot_is_refused_by_name() {
    let Some((kernel, initramfs)) = artifacts() else {
        return;
    };
    if !hypervisor_available() {
        return;
    }
    let scratch = Scratch::new("refuse");
    let config = profile(&scratch, &kernel, &initramfs, None);
    let snapshot = scratch.path("vm.esnap");
    suspend_once(&scratch, &config, &snapshot);
    let whole = std::fs::read(&snapshot).expect("read the snapshot");

    // Not a snapshot at all.
    let text = refuse(&scratch, "garbage.esnap", &vec![0x5au8; 4096]);
    assert!(text.contains("not an Entangled snapshot"), "{text}");

    // Cut short — a full disk, an interrupted copy.
    let text = refuse(&scratch, "short.esnap", &whole[..whole.len() / 2]);
    assert!(
        text.contains("truncated") || text.contains("corrupt"),
        "{text}"
    );

    // A format version this build does not read.
    let mut wrong_version = whole.clone();
    wrong_version[8..12].copy_from_slice(&(vm_snapshot::VERSION + 7).to_le_bytes());
    let text = refuse(&scratch, "version.esnap", &wrong_version);
    assert!(text.contains("format version"), "{text}");

    // A header flag from a newer build.
    let mut flagged = whole.clone();
    flagged[12..16].copy_from_slice(&1u32.to_le_bytes());
    let text = refuse(&scratch, "flags.esnap", &flagged);
    assert!(text.contains("unknown flags"), "{text}");

    // The other hypervisor's snapshot: host code 1 is KVM, 2 is WHP.
    let mut foreign = whole.clone();
    let ours = u32::from_le_bytes(foreign[16..20].try_into().unwrap());
    foreign[16..20].copy_from_slice(&(if ours == 1 { 2u32 } else { 1u32 }).to_le_bytes());
    let text = refuse(&scratch, "foreign.esnap", &foreign);
    assert!(text.contains("taken on"), "{text}");

    // A flipped bit somewhere in the middle of the payload.
    let mut corrupt = whole.clone();
    let at = whole.len() / 3;
    corrupt[at] ^= 0xff;
    let text = refuse(&scratch, "corrupt.esnap", &corrupt);
    assert!(
        text.contains("corrupt") || text.contains("truncated") || text.contains("invalid"),
        "{text}"
    );
}

/// **The refusal that protects a filesystem.** A disk that changed while the VM
/// was suspended must not be handed back to a kernel holding stale metadata for
/// it.
#[test]
fn a_disk_that_changed_since_the_snapshot_is_refused() {
    let Some((kernel, initramfs)) = artifacts() else {
        return;
    };
    if !hypervisor_available() {
        return;
    }
    let scratch = Scratch::new("disk");
    let disk = scratch.path("root.raw");
    std::fs::write(&disk, vec![0u8; 8 << 20]).expect("create the disk");
    let config = profile(&scratch, &kernel, &initramfs, Some(&disk));
    let snapshot = scratch.path("vm.esnap");
    suspend_once(&scratch, &config, &snapshot);

    // The same VM, resumed onto the same disk, is fine — proof that the check
    // is about *change* and not merely about having a disk at all.
    let mut vm = Vm::spawn(&["resume", "--headless", &snapshot.display().to_string()]);
    assert!(
        vm.wait_for(HEARTBEAT, 2),
        "the unchanged disk was refused:\n{}",
        tail(&vm.console(), 30)
    );
    vm.kill();

    // Now something else writes to it — a second VM, a restore from backup, a
    // resize.
    std::fs::write(&disk, vec![0u8; 16 << 20]).expect("grow the disk");
    let out = Command::new(BIN)
        .args(["resume", "--headless", &snapshot.display().to_string()])
        .current_dir(repo_root())
        .output()
        .expect("run entangled resume");
    assert!(
        !out.status.success(),
        "entangled resume accepted a snapshot whose disk had changed"
    );
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        text.contains("root.raw"),
        "the refusal does not name the disk:\n{text}"
    );
    assert!(
        text.contains("has changed since the snapshot"),
        "the refusal does not say what happened:\n{text}"
    );
    eprintln!(
        "[refusal] modified disk: {}",
        text.trim().lines().next_back().unwrap_or("")
    );
}
