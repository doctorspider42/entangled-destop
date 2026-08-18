//! Shared harness for boot integration tests (backlog EPIC 14).
//!
//! Boots a real VM headlessly — kernel + initramfs, optional virtio-blk disk —
//! captures the serial console and reports how long it took to reach
//! [`linux_boot::GUEST_READY_MARKER`]. Two tests are built on it:
//!
//! * `tests/repeat_boot.rs` — MVP-1403: 100 sequential boots with fd and RSS
//!   accounting, so a leak in VM teardown shows up as growth across iterations.
//! * `tests/notify_bench.rs` — MVP-307: the same boot with queue kicks handled
//!   synchronously on the vCPU thread versus offloaded to ioeventfds plus
//!   per-device worker threads.
//!
//! Everything self-skips when `/dev/kvm` or the guest artifacts are missing, so
//! a machine without KVM still runs the rest of the suite (vm-testing skill,
//! tier 3).

#![cfg(target_os = "linux")]

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use linux_boot::{BootConfig, GUEST_READY_MARKER};
use machine_x86::boot as x86_boot;
use machine_x86::bus::MachineBus;
use machine_x86::notify::QueueNotifyMode;
use machine_x86::serial::SerialConsole;
use machine_x86::virtio::VirtioMmioBus;
use virtio_core::VirtioDevice;
use vmm_core::{spawn_vcpus, Hypervisor, MachineConfig, RunOutcome, Vm, VmmError};

/// Kernel panic banner: seeing it means the boot failed, no point in waiting.
const PANIC_MARKER: &str = "Kernel panic - not syncing";

/// Default per-boot deadline. Generous because a cold page cache on the first
/// iteration of a 100-boot run is much slower than the steady state.
pub const DEFAULT_DEADLINE: Duration = Duration::from_secs(60);

/// What to boot and how.
#[derive(Debug, Clone)]
pub struct BootSpec {
    pub kernel: PathBuf,
    pub initramfs: PathBuf,
    /// Attached as `/dev/vda` when present.
    pub disk: Option<PathBuf>,
    pub memory_mib: u64,
    pub vcpus: u32,
    /// Appended to the base command line (before the virtio clauses).
    pub extra_cmdline: String,
    /// Keep the guest running past the ready marker until this string appears
    /// (a probe's `VMHOST_TEST_OK`/`FAIL` line) or the deadline expires.
    pub await_marker: Option<String>,
    pub notify: QueueNotifyMode,
    pub deadline: Duration,
}

impl BootSpec {
    /// A minimal single-vCPU boot of `kernel` + `initramfs`.
    pub fn new(kernel: PathBuf, initramfs: PathBuf) -> Self {
        Self {
            kernel,
            initramfs,
            disk: None,
            memory_mib: 256,
            vcpus: 1,
            extra_cmdline: String::new(),
            await_marker: None,
            // Honours ENTANGLED_QUEUE_NOTIFY like the real binary does, so a
            // whole test run can be flipped to the synchronous path from the
            // environment; benchmarks override it per boot.
            notify: QueueNotifyMode::from_env(),
            deadline: DEFAULT_DEADLINE,
        }
    }

    /// Requests the guest's block-read probe and waits for its result line.
    pub fn with_blk_bench(mut self, mib: u64) -> Self {
        self.extra_cmdline = match self.extra_cmdline.trim() {
            "" => format!("entangled.blkbench={mib}"),
            existing => format!("{existing} entangled.blkbench={mib}"),
        };
        self.await_marker = Some("blkbench".into());
        self
    }

    pub fn with_disk(mut self, disk: PathBuf) -> Self {
        self.disk = Some(disk);
        self
    }

    pub fn with_notify(mut self, notify: QueueNotifyMode) -> Self {
        self.notify = notify;
        self
    }

    pub fn with_extra_cmdline(mut self, extra: impl Into<String>) -> Self {
        self.extra_cmdline = extra.into();
        self
    }
}

/// Result of one boot.
#[derive(Debug)]
pub struct BootOutcome {
    /// Time from `spawn_vcpus` to the ready marker appearing on the console.
    pub time_to_ready: Option<Duration>,
    /// Time spent in the whole call, including VM construction and teardown.
    pub total: Duration,
    pub serial: String,
    pub vcpu_outcomes: Vec<Result<RunOutcome, VmmError>>,
}

impl BootOutcome {
    pub fn reached_ready(&self) -> bool {
        self.time_to_ready.is_some()
    }

    /// Extracts a `VMHOST_TEST_OK <name> key=value …` probe line's fields.
    pub fn probe(&self, name: &str) -> Option<Vec<(String, String)>> {
        let prefix = format!("VMHOST_TEST_OK {name} ");
        self.serial
            .lines()
            .find(|line| line.trim_start().starts_with(&prefix))
            .map(|line| {
                line.trim()
                    .split_ascii_whitespace()
                    .skip(2)
                    .filter_map(|field| field.split_once('='))
                    .map(|(k, v)| (k.to_string(), v.to_string()))
                    .collect()
            })
    }

    /// One numeric field of a probe line.
    pub fn probe_value(&self, name: &str, key: &str) -> Option<u64> {
        self.probe(name)?
            .into_iter()
            .find(|(k, _)| k == key)
            .and_then(|(_, v)| v.parse().ok())
    }
}

/// True once `needle` has appeared *and* the line carrying it is terminated, so
/// a reader is guaranteed to see all of its fields.
fn complete_line_with(text: &str, needle: &str) -> bool {
    match text.find(needle) {
        Some(at) => text[at..].contains('\n'),
        None => false,
    }
}

/// Serial sink that keeps everything the guest printed.
#[derive(Clone, Default)]
struct Capture(Arc<Mutex<Vec<u8>>>);

impl Write for Capture {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        if let Ok(mut inner) = self.0.lock() {
            inner.extend_from_slice(buf);
        }
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl Capture {
    fn text(&self) -> String {
        String::from_utf8_lossy(&self.0.lock().map(|v| v.clone()).unwrap_or_default()).into_owned()
    }
}

/// Boots once and returns when the guest reached the marker, shut down on its
/// own, or the deadline expired. All threads are joined and every device fd is
/// released before returning, which is what makes the endurance test's fd/RSS
/// accounting meaningful.
pub fn boot_once(spec: &BootSpec) -> Result<BootOutcome, String> {
    let started = Instant::now();
    let hv = Hypervisor::open().map_err(|e| e.to_string())?;
    let machine = MachineConfig {
        memory_mib: spec.memory_mib,
        vcpu_count: spec.vcpus,
    };
    let mut vm = Vm::new(&hv, &machine).map_err(|e| e.to_string())?;
    // Interrupt topology (the fix this harness's 100-boot test exists to
    // verify): route device IRQs through the IOAPIC instead of the 8259
    // virtual-wire fallback, where irqfd edges were lost ~1 boot in 3.
    machine_x86::mptable::write(vm.memory(), machine.vcpu_count).map_err(|e| e.to_string())?;

    let capture = Capture::default();
    let serial =
        SerialConsole::new(vm.fd(), Box::new(capture.clone())).map_err(|e| e.to_string())?;

    let mut devices: Vec<Box<dyn VirtioDevice>> = Vec::new();
    if let Some(disk) = &spec.disk {
        let device = virtio_block::BlockDevice::open(disk, true)
            .map_err(|e| format!("cannot attach disk {}: {e}", disk.display()))?;
        devices.push(Box::new(device));
    }

    let mem = Arc::new(vm.memory().clone());
    let virtio = VirtioMmioBus::attach_with(vm.fd_shared(), mem, devices, spec.notify)
        .map_err(|e| e.to_string())?;
    let clauses = virtio.cmdline_clauses();

    let mut cmdline = String::from("console=ttyS0 earlyprintk=serial panic=1 reboot=k");
    for extra in [spec.extra_cmdline.trim(), clauses.trim()] {
        if !extra.is_empty() {
            cmdline.push(' ');
            cmdline.push_str(extra);
        }
    }
    let bus = MachineBus::with_virtio(serial, virtio);

    let boot = BootConfig {
        kernel: spec.kernel.clone(),
        initramfs: Some(spec.initramfs.clone()),
        cmdline,
    };
    let loaded = linux_boot::load(vm.memory(), &boot, machine.memory_mib << 20)
        .map_err(|e| e.to_string())?;

    let vcpus = vm.take_vcpus();
    for vcpu in &vcpus {
        x86_boot::setup_long_mode_sregs(vm.memory(), vcpu).map_err(|e| e.to_string())?;
        if vcpu.index == 0 {
            x86_boot::setup_boot_regs(vcpu, loaded.entry, loaded.boot_params_addr)
                .map_err(|e| e.to_string())?;
        }
    }

    let run_started = Instant::now();
    let threads = spawn_vcpus(vcpus, |_| Box::new(bus.clone())).map_err(|e| e.to_string())?;

    let mut time_to_ready = None;
    let mut panicked = false;
    while run_started.elapsed() < spec.deadline {
        let text = capture.text();
        if time_to_ready.is_none() && text.contains(GUEST_READY_MARKER) {
            time_to_ready = Some(run_started.elapsed());
            if spec.await_marker.is_none() {
                break;
            }
        }
        if let Some(probe) = &spec.await_marker {
            // Either outcome line ends the wait; the test decides what a FAIL
            // means for it. The whole line must have arrived — stopping the VM
            // mid-line would truncate the very numbers we came for.
            if complete_line_with(&text, &format!("VMHOST_TEST_OK {probe} "))
                || complete_line_with(&text, &format!("VMHOST_TEST_FAIL {probe} "))
            {
                break;
            }
        }
        if text.contains(PANIC_MARKER) {
            panicked = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    // The test initramfs reboots itself after the marker, which reaches the host
    // as KVM_EXIT_SHUTDOWN; `stop` joins whether or not that already happened.
    let vcpu_outcomes = threads.stop();

    let mut serial = capture.text();
    if time_to_ready.is_none() {
        serial.push_str(&format!("\nvcpu outcomes: {vcpu_outcomes:?}\n"));
        // A guest that stopped making progress is almost always waiting for a
        // device; say what the devices think their state is, so the log
        // distinguishes "the kick never arrived" from "the interrupt was lost".
        serial.push_str(&device_state(&bus));
    }
    if panicked {
        return Err(format!("guest kernel panicked; serial log:\n{serial}"));
    }
    Ok(BootOutcome {
        time_to_ready,
        total: started.elapsed(),
        serial,
        vcpu_outcomes,
    })
}

/// One line per virtio slot describing what the host side believes: device
/// status word, whether it is activated, and the pending `INTERRUPT_STATUS`
/// bits. A stalled guest with `INTERRUPT_STATUS` still non-zero means the device
/// answered and the interrupt was never acknowledged.
fn device_state(bus: &MachineBus) -> String {
    let mut out = String::from("\n--- host device state at stall ---\n");
    for (index, slot) in bus.virtio().slots().iter().enumerate() {
        match slot.transport.lock() {
            Ok(t) => out.push_str(&format!(
                "slot {index}: device={:?} status={:#04x} activated={} interrupt_status={:#x} \
                 offloaded_queues={:?}\n",
                t.device_type(),
                t.status(),
                t.is_activated(),
                t.interrupt_status(),
                slot.notifier().map(|n| n.offloaded_queues()),
            )),
            Err(_) => out.push_str(&format!("slot {index}: transport lock poisoned\n")),
        }
    }
    out.push_str("---\n");
    out
}

// ------------------------------------------------------------------ artifacts

/// Repository root, derived from this crate's manifest directory.
pub fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

/// An existing file under `artifacts/`, or `None`.
pub fn artifact(relative: &str) -> Option<PathBuf> {
    let path = repo_root().join("artifacts").join(relative);
    path.exists().then_some(path)
}

/// The kernel the boot tests use: the project's own bootstrap kernel when it has
/// been built (EPIC 11), otherwise the fetched Debian test kernel.
pub fn boot_kernel() -> Option<PathBuf> {
    artifact("bootstrap/vmlinuz").or_else(|| artifact("tests/vmlinuz"))
}

pub fn test_initramfs() -> Option<PathBuf> {
    artifact("tests/test-initramfs.cpio.gz")
}

/// Kernel + initramfs, or `None` with an explanatory note on stderr.
pub fn boot_artifacts() -> Option<(PathBuf, PathBuf)> {
    match (boot_kernel(), test_initramfs()) {
        (Some(kernel), Some(initramfs)) => Some((kernel, initramfs)),
        _ => {
            eprintln!(
                "skipping: guest artifacts missing — run scripts/fetch-test-kernel.sh (or \
                 guest/bootstrap-kernel/build.sh) and scripts/build-test-initramfs.sh"
            );
            None
        }
    }
}

/// True when a VM can actually be created here.
pub fn kvm_available() -> bool {
    match Hypervisor::open() {
        Ok(_) => true,
        Err(e) => {
            eprintln!("skipping: {e}");
            false
        }
    }
}

// -------------------------------------------------------------- process stats

/// Open file descriptors of this process, for leak accounting.
pub fn open_fds() -> usize {
    std::fs::read_dir("/proc/self/fd")
        .map(|entries| entries.count())
        .unwrap_or(0)
}

/// Resident set size in KiB, from `/proc/self/status`.
pub fn rss_kib() -> u64 {
    field_from_status("VmRSS:").unwrap_or(0)
}

/// Threads currently in this process, so a leaked device or vCPU thread shows up
/// even when it holds no fd of its own.
pub fn thread_count() -> u64 {
    field_from_status("Threads:").unwrap_or(0)
}

fn field_from_status(prefix: &str) -> Option<u64> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    status
        .lines()
        .find(|line| line.starts_with(prefix))
        .and_then(|line| line.split_ascii_whitespace().nth(1))
        .and_then(|value| value.parse().ok())
}

/// Creates (or truncates) a sparse raw disk image of `size_mib` MiB.
///
/// Deliberately takes an absolute path: on this project's Windows development
/// host the repository lives on a drvfs mount that cannot create sparse files,
/// so callers put scratch images somewhere on a native Linux filesystem.
pub fn make_raw_disk(path: &Path, size_mib: u64) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("{}: {e}", parent.display()))?;
    }
    let file = std::fs::File::create(path).map_err(|e| format!("{}: {e}", path.display()))?;
    file.set_len(size_mib << 20)
        .map_err(|e| format!("{}: {e}", path.display()))?;
    Ok(())
}
