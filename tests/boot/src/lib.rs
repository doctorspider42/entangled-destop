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
//! * `tests/pci_transport.rs` — EPIC 19: the acceptance boot for virtio-pci, with
//!   no `virtio_mmio.device=` clause anywhere on the command line.
//! * `tests/lifecycle.rs` — ADR-0005: pause, resume and reboot-in-place, using
//!   [`boot_once_driven`] to act on the VM while its vCPUs are running.
//! * `tests/gamepad.rs` — GAME-2104: what a real guest kernel makes of the
//!   gamepad descriptor, and hotplug through the production capture pump.
//!
//! [`BootSpec::transport`] selects the virtio transport, so any test built on the
//! harness can be run either way — which is the point: a transport that only the
//! transport's own test exercises is a transport nobody trusts.
//!
//! Everything self-skips when `/dev/kvm` or the guest artifacts are missing, so
//! a machine without KVM still runs the rest of the suite (vm-testing skill,
//! tier 3).

#![cfg(target_os = "linux")]

use std::cell::Cell;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use std::sync::atomic::{AtomicBool, Ordering};

use control_api::VirtioTransport;
use linux_boot::{BootConfig, GUEST_READY_MARKER};
use machine_x86::boot as x86_boot;
use machine_x86::bus::MachineBus;
use machine_x86::notify::QueueNotifyMode;
use machine_x86::serial::SerialConsole;
use machine_x86::virtio::VirtioMmioBus;
use machine_x86::virtio_pci::{PciInterruptMode, VirtioPciBus};
use virtio_core::{Quiesce, VirtioDevice};
use vmm_core::hv::VcpuRegisters;
use vmm_core::{
    spawn_vcpus_with, Hypervisor, Lifecycle, MachineConfig, MachineLifecycle, RunOutcome, Vm,
    VmmError,
};

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
    /// Let the guest end the VM itself instead of stopping it from the host:
    /// wait until every vCPU thread has finished (or the deadline expires) and
    /// report what ended it. This is what distinguishes an ACPI power-off from
    /// "the harness gave up" — see [`BootOutcome::ended_by_guest`].
    pub await_exit: bool,
    pub notify: QueueNotifyMode,
    /// Which virtio transport the devices sit on. `Mmio` is the default, so an
    /// existing test keeps booting the machine it always booted.
    pub transport: VirtioTransport,
    /// Which interrupt mechanisms the pci functions publish. Ignored on mmio.
    ///
    /// Defaults to MSI-X, which is what a real VM does and therefore what most
    /// tests should exercise; the INTx acceptance boot asks for
    /// [`PciInterruptMode::IntxOnly`] explicitly, because a Linux guest offered
    /// MSI-X will never choose INTx and the path would stop being tested.
    pub pci_interrupts: PciInterruptMode,
    /// Whether to attach a virtio-input gamepad (GAME-2104), and what drives
    /// it. Never a *real* controller: see [`GamepadAttach`].
    pub gamepad: GamepadAttach,
    pub deadline: Duration,
}

/// How a boot attaches the virtio-input gamepad (GAME-2104).
///
/// Neither variant ever touches a host controller, and that is deliberate: a
/// machine with a pad plugged into it and a machine without must produce
/// identical results, or the acceptance is measuring the developer's desk.
#[derive(Clone, Default)]
pub enum GamepadAttach {
    /// No pad on the bus at all.
    #[default]
    None,
    /// A pad with no capture behind it. The only thing that can move it is
    /// [`VmHandle::gamepad`], so the test writes the exact event sequence it
    /// then asserts on — which is what the descriptor acceptance needs.
    Injected,
    /// A pad driven by the real [`virtio_input::GamepadCapture`] pump over a
    /// scripted [`virtio_input::GamepadSource`]. Slower and less exact than
    /// [`Self::Injected`], and the only way to exercise the half of the path a
    /// direct push skips: connect/disconnect, the state diff, and therefore
    /// hotplug.
    Captured(virtio_input::SourceFactory),
}

impl std::fmt::Debug for GamepadAttach {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::None => "None",
            Self::Injected => "Injected",
            Self::Captured(_) => "Captured(..)",
        })
    }
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
            await_exit: false,
            // Honours ENTANGLED_QUEUE_NOTIFY like the real binary does, so a
            // whole test run can be flipped to the synchronous path from the
            // environment; benchmarks override it per boot.
            notify: QueueNotifyMode::from_env(),
            transport: VirtioTransport::default(),
            pci_interrupts: PciInterruptMode::default(),
            gamepad: GamepadAttach::None,
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

    /// Requests the guest's discard probe: fill `mib` MiB on `/dev/vda`, free
    /// it and ask the kernel to hand the space back. The caller measures the
    /// image's allocated size before and after — that is the evidence.
    pub fn with_trim_probe(mut self, mib: u64) -> Self {
        self.extra_cmdline = match self.extra_cmdline.trim() {
            "" => format!("entangled.trim={mib}"),
            existing => format!("{existing} entangled.trim={mib}"),
        };
        self.await_marker = Some("trim".into());
        self
    }

    /// Requests the guest's ACPI power-off probe: the init calls
    /// `reboot(LINUX_REBOOT_CMD_POWER_OFF)`, which only ends the VM if the FADT,
    /// the DSDT's `\_S5` and the ACPI PM block all work. The harness then waits
    /// for the VM to end *itself*.
    pub fn with_poweroff_probe(mut self) -> Self {
        self.extra_cmdline = match self.extra_cmdline.trim() {
            "" => "entangled.poweroff=1".into(),
            existing => format!("{existing} entangled.poweroff=1"),
        };
        self.await_exit = true;
        self
    }

    pub fn with_vcpus(mut self, vcpus: u32) -> Self {
        self.vcpus = vcpus;
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

    pub fn with_transport(mut self, transport: VirtioTransport) -> Self {
        self.transport = transport;
        self
    }

    /// Which interrupt mechanisms the pci functions publish; see
    /// [`Self::pci_interrupts`].
    pub fn with_pci_interrupts(mut self, interrupts: PciInterruptMode) -> Self {
        self.pci_interrupts = interrupts;
        self
    }

    /// Attaches a gamepad and asks the guest to report what the kernel made of
    /// it, then to echo up to `events` input events (GAME-2104).
    ///
    /// `events` is a **ceiling**, not a target: the guest also stops after a
    /// short silence, so asking for more than the host injects turns the
    /// probe's `seq=` into evidence that nothing else followed. The driver
    /// injects through [`VmHandle::gamepad`] once the guest's `padinfo` line
    /// says its event node is open.
    pub fn with_gamepad_probe(mut self, events: usize) -> Self {
        self.gamepad = GamepadAttach::Injected;
        self.with_pad_probe_cmdline(events)
    }

    /// The same probe, but with the pad driven by the production capture pump
    /// over `source` instead of by direct pushes — the hotplug acceptance.
    pub fn with_gamepad_capture(
        mut self,
        source: virtio_input::SourceFactory,
        events: usize,
    ) -> Self {
        self.gamepad = GamepadAttach::Captured(source);
        self.with_pad_probe_cmdline(events)
    }

    fn with_pad_probe_cmdline(mut self, events: usize) -> Self {
        self.extra_cmdline = match self.extra_cmdline.trim() {
            "" => format!("entangled.padprobe={events}"),
            existing => format!("{existing} entangled.padprobe={events}"),
        };
        self.await_marker = Some("padprobe".into());
        self
    }

    /// Requests the guest's PCI enumeration report and waits for its result line.
    pub fn with_pci_scan(mut self) -> Self {
        self.extra_cmdline = match self.extra_cmdline.trim() {
            "" => "entangled.pciscan=1".to_string(),
            existing => format!("{existing} entangled.pciscan=1"),
        };
        self.await_marker = Some("pciscan".into());
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

    /// True when every vCPU ended because the *guest* stopped
    /// ([`RunOutcome::Shutdown`]) rather than because the harness asked it to
    /// ([`RunOutcome::Stopped`]). An ACPI power-off, a triple-fault reboot and a
    /// `hlt` all count; the harness giving up does not.
    pub fn ended_by_guest(&self) -> bool {
        !self.vcpu_outcomes.is_empty()
            && self
                .vcpu_outcomes
                .iter()
                .all(|outcome| matches!(outcome, Ok(RunOutcome::Shutdown) | Ok(RunOutcome::Halted)))
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

/// The machine behind the harness's lifecycle seam (ADR-0005).
///
/// A second, much smaller implementation of `MachineLifecycle` than the one in
/// `entangled run` — deliberately: a seam only one caller can implement is not
/// a seam. This one knows the harness only ever boots a direct-Linux guest, so
/// the boot plan is a single `BootConfig`.
struct TestMachine {
    bus: MachineBus,
    mem: Arc<vmm_core::GuestMem>,
    quiesce: Arc<Quiesce>,
    vcpus: u32,
    boot: BootConfig,
    mem_size: u64,
    entry: Mutex<(u64, u64)>,
}

impl MachineLifecycle for TestMachine {
    fn quiesce(&self) {
        self.quiesce.pause();
        self.bus.set_paused(true);
        self.quiesce.wait_until_idle(Duration::from_secs(5));
    }

    fn unquiesce(&self) {
        self.bus.set_paused(false);
        self.quiesce.resume();
    }

    fn reset_machine(&self) -> Result<(), String> {
        self.bus.reset_devices();
        machine_x86::mptable::write(self.mem.as_ref(), self.vcpus).map_err(|e| e.to_string())?;
        machine_x86::acpi::write(self.mem.as_ref(), self.vcpus).map_err(|e| e.to_string())?;
        let loaded = linux_boot::load(self.mem.as_ref(), &self.boot, self.mem_size)
            .map_err(|e| e.to_string())?;
        match self.entry.lock() {
            Ok(mut slot) => *slot = (loaded.entry, loaded.boot_params_addr),
            Err(poisoned) => *poisoned.into_inner() = (loaded.entry, loaded.boot_params_addr),
        }
        Ok(())
    }

    fn reset_vcpu(&self, index: u32, vcpu: &dyn VcpuRegisters) -> Result<(), String> {
        if index != 0 {
            return Ok(());
        }
        let (entry, boot_params) = match self.entry.lock() {
            Ok(slot) => *slot,
            Err(poisoned) => *poisoned.into_inner(),
        };
        x86_boot::setup_long_mode_sregs(self.mem.as_ref(), vcpu).map_err(|e| e.to_string())?;
        x86_boot::setup_boot_regs(vcpu, entry, boot_params).map_err(|e| e.to_string())
    }
}

/// A running VM, as a [`Driver`] sees it: the lifecycle seam plus the serial
/// console, and a way to say "I have seen enough".
pub struct VmHandle {
    pub lifecycle: Arc<Lifecycle>,
    /// The host end of the guest's gamepad, when [`BootSpec::gamepad`] asked
    /// for one. Pushing into it is exactly what `entangled run`'s capture
    /// thread does, minus the controller.
    pub gamepad: Option<virtio_input::InputHandle>,
    capture: Capture,
    done: Arc<AtomicBool>,
}

impl VmHandle {
    /// Everything the guest has printed so far.
    pub fn serial(&self) -> String {
        self.capture.text()
    }

    /// How many times `needle` appears on the console. The reboot tests count
    /// ready markers with it: "the guest came back" is exactly "the marker
    /// appeared again".
    pub fn count(&self, needle: &str) -> usize {
        self.capture.text().matches(needle).count()
    }

    /// Waits until `needle` has appeared at least `times` times, or the timeout
    /// expires. Returns how many were seen.
    pub fn wait_for(&self, needle: &str, times: usize, timeout: Duration) -> usize {
        let deadline = Instant::now() + timeout;
        loop {
            let seen = self.count(needle);
            if seen >= times || Instant::now() >= deadline {
                return seen;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// Ends the boot: the supervisor stops the vCPUs and `boot_once_driven`
    /// returns.
    pub fn finish(&self) {
        self.done.store(true, Ordering::Release);
    }
}

/// What a test does to a running VM. Runs on its own thread while the vCPUs
/// execute, and must call [`VmHandle::finish`] when it is done (or let the
/// deadline expire).
pub type Driver = Box<dyn FnOnce(VmHandle) + Send>;

/// Boots once and returns when the guest reached the marker, shut down on its
/// own, or the deadline expired. All threads are joined and every device fd is
/// released before returning, which is what makes the endurance test's fd/RSS
/// accounting meaningful.
pub fn boot_once(spec: &BootSpec) -> Result<BootOutcome, String> {
    boot_once_driven(spec, None)
}

/// [`boot_once`] with a [`Driver`] pausing, resuming or resetting the VM while
/// it runs (ADR-0005).
///
/// With a driver attached the VM also gains a lifecycle seam, which changes one
/// thing about the guest as well as about the host: a guest reset (`reboot=k`
/// pulses the keyboard controller, an ACPI reboot writes 0xCF9) becomes an
/// in-place reboot instead of the end of the VM. That is what makes the
/// guest-reboot test possible, and why `boot_once` deliberately does *not*
/// attach one — every existing test depends on the test guest's reboot ending
/// the run.
pub fn boot_once_driven(spec: &BootSpec, drive: Option<Driver>) -> Result<BootOutcome, String> {
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
    // ACPI tables (RSDP..DSDT). Published alongside the MP table, not instead of
    // it: Linux prefers the MADT, the MP table stays the `acpi=off` fallback.
    // `linux_boot::load` points `boot_params.acpi_rsdp_addr` at the RSDP.
    machine_x86::acpi::write(vm.memory(), machine.vcpu_count).map_err(|e| e.to_string())?;

    let capture = Capture::default();
    let serial =
        SerialConsole::new(vm.fd(), Box::new(capture.clone())).map_err(|e| e.to_string())?;

    let mut devices: Vec<Box<dyn VirtioDevice>> = Vec::new();
    if let Some(disk) = &spec.disk {
        let device = virtio_block::BlockDevice::open(disk, true)
            .map_err(|e| format!("cannot attach disk {}: {e}", disk.display()))?;
        devices.push(Box::new(device));
    }
    // Last, as `entangled run` attaches it, so the guest sees the same machine.
    let gamepad_sink = match &spec.gamepad {
        GamepadAttach::None => None,
        GamepadAttach::Injected => {
            let device = virtio_input::InputDevice::gamepad();
            let sink = device.handle();
            devices.push(Box::new(device));
            Some(sink)
        }
        GamepadAttach::Captured(source) => {
            let device = virtio_input::InputDevice::gamepad_with_capture(Arc::clone(source));
            let sink = device.handle();
            devices.push(Box::new(device));
            Some(sink)
        }
    };

    let mem = Arc::new(vm.memory().clone());
    let quiesce = Quiesce::new();
    // Exactly one transport, chosen by the spec. On pci there are no cmdline
    // clauses at all — the guest enumerates the bus — which is also what makes
    // the acceptance test meaningful: nothing tells the kernel where to look.
    let (bus, clauses) = match spec.transport {
        VirtioTransport::Mmio => {
            let virtio =
                VirtioMmioBus::attach_with(vm.fd_shared(), Arc::clone(&mem), devices, spec.notify)
                    .map_err(|e| e.to_string())?;
            let clauses = virtio.cmdline_clauses();
            (MachineBus::with_virtio(serial, virtio), clauses)
        }
        VirtioTransport::Pci => {
            let pci = VirtioPciBus::attach_with_interrupts(
                vm.fd_shared(),
                Arc::clone(&mem),
                devices,
                spec.notify,
                spec.pci_interrupts,
            )
            .map_err(|e| e.to_string())?;
            (MachineBus::with_virtio_pci(serial, pci), String::new())
        }
    };

    let mut cmdline = String::from("console=ttyS0 earlyprintk=serial panic=1 reboot=k");
    for extra in [spec.extra_cmdline.trim(), clauses.trim()] {
        if !extra.is_empty() {
            cmdline.push(' ');
            cmdline.push_str(extra);
        }
    }
    debug_assert!(
        !spec.transport.is_pci() || !cmdline.contains("virtio_mmio.device"),
        "a pci boot must not announce mmio slots: {cmdline}"
    );

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
    // A lifecycle seam only when a driver asked for one: attaching a machine is
    // what turns a guest reset into a reboot, and every other test in this
    // harness relies on the test guest's reboot ending the run.
    let lifecycle = Lifecycle::new(machine.vcpu_count);
    let done = Arc::new(AtomicBool::new(false));
    let driver_thread = match drive {
        Some(drive) => {
            bus.set_quiesce(Arc::clone(&quiesce));
            lifecycle.attach_machine(Arc::new(TestMachine {
                bus: bus.clone(),
                mem: Arc::clone(&mem),
                quiesce,
                vcpus: machine.vcpu_count,
                boot,
                mem_size: machine.memory_mib << 20,
                entry: Mutex::new((loaded.entry, loaded.boot_params_addr)),
            }));
            let handle = VmHandle {
                lifecycle: Arc::clone(&lifecycle),
                gamepad: gamepad_sink,
                capture: capture.clone(),
                done: Arc::clone(&done),
            };
            // Runs while the vCPUs do: everything a driver does blocks until
            // the vCPUs acknowledge, so it cannot live in the poll predicate.
            Some(std::thread::spawn(move || drive(handle)))
        }
        None => None,
    };
    // The harness's own lifecycle supervisor, the same shape `entangled run`
    // has: the thread that turns a *guest* reset request (a 0xCF9 write, the
    // keyboard-controller pulse, a triple fault) into the reset itself. Without
    // one, a guest that reboots itself simply waits for ever.
    let supervisor_stop = Arc::new(AtomicBool::new(false));
    let supervisor = driver_thread.is_some().then(|| {
        let lifecycle = Arc::clone(&lifecycle);
        let stop = Arc::clone(&supervisor_stop);
        std::thread::spawn(move || {
            while !stop.load(Ordering::Acquire) {
                if lifecycle.take_guest_reset() {
                    if let Err(error) = lifecycle.reset() {
                        eprintln!("harness: guest-requested reset failed: {error}");
                        return;
                    }
                }
                std::thread::sleep(Duration::from_millis(10));
            }
        })
    });
    let driven = driver_thread.is_some();
    let threads = spawn_vcpus_with(
        vcpus,
        |_| Box::new(bus.clone()),
        driven.then(|| Arc::clone(&lifecycle)),
    )
    .map_err(|e| e.to_string())?;

    // `join_or_stop` returns as soon as every vCPU has ended on its own — the
    // test initramfs reboots itself after the marker, which reaches the host as
    // KVM_EXIT_SHUTDOWN, and an ACPI power-off arrives the same way — or as soon
    // as this predicate says the harness has seen enough.
    let time_to_ready = Cell::new(None);
    let panicked = Cell::new(false);
    let vcpu_outcomes = threads.join_or_stop(
        || {
            let text = capture.text();
            if time_to_ready.get().is_none() && text.contains(GUEST_READY_MARKER) {
                time_to_ready.set(Some(run_started.elapsed()));
                // A driven boot is never ended by the marker: the driver is
                // about to pause, resume or reboot the guest, and every one of
                // those happens *after* it becomes ready (ADR-0005).
                if spec.await_marker.is_none() && !spec.await_exit && !driven {
                    return true;
                }
            }
            if let Some(probe) = &spec.await_marker {
                // Either outcome line ends the wait; the test decides what a
                // FAIL means for it. The whole line must have arrived — stopping
                // the VM mid-line would truncate the very numbers we came for.
                if !spec.await_exit
                    && (complete_line_with(&text, &format!("VMHOST_TEST_OK {probe} "))
                        || complete_line_with(&text, &format!("VMHOST_TEST_FAIL {probe} ")))
                {
                    return true;
                }
            }
            if text.contains(PANIC_MARKER) {
                panicked.set(true);
                return true;
            }
            done.load(Ordering::Acquire) || run_started.elapsed() >= spec.deadline
        },
        Duration::from_millis(2),
    );
    let time_to_ready = time_to_ready.get();
    let panicked = panicked.get();
    supervisor_stop.store(true, Ordering::Release);
    if let Some(handle) = supervisor {
        let _ = handle.join();
    }
    if let Some(handle) = driver_thread {
        // The vCPUs have stopped, so a driver still blocked inside a lifecycle
        // call has been released by `stop()` and this cannot hang.
        if handle.join().is_err() {
            return Err("the lifecycle driver panicked".into());
        }
    }

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
/// status word, whether it is activated, and the pending interrupt bits. A
/// stalled guest with a non-zero interrupt status means the device answered and
/// the interrupt was never acknowledged.
///
/// Covers both transports, because "the guest stopped making progress" is exactly
/// when you need to know which one it was talking to.
fn device_state(bus: &MachineBus) -> String {
    let mut out = String::from("\n--- host device state at stall ---\n");
    for (index, slot) in bus.virtio().slots().iter().enumerate() {
        match slot.transport.lock() {
            Ok(t) => out.push_str(&format!(
                "mmio slot {index}: device={:?} status={:#04x} activated={} \
                 interrupt_status={:#x} offloaded_queues={:?}\n",
                t.device_type(),
                t.status(),
                t.is_activated(),
                t.interrupt_status(),
                slot.notifier().map(|n| n.offloaded_queues()),
            )),
            Err(_) => out.push_str(&format!("mmio slot {index}: transport lock poisoned\n")),
        }
    }
    for (index, slot) in bus
        .pci()
        .map(|p| p.slots())
        .unwrap_or_default()
        .iter()
        .enumerate()
    {
        match slot.transport.lock() {
            Ok(t) => out.push_str(&format!(
                "pci 00:{:02x}.0 (slot {index}): device={:?} bar={:#x} irq={} status={:#04x} \
                 activated={} isr={:#x} offloaded_queues={:?}\n",
                slot.device_number,
                t.device_type(),
                slot.bar_base,
                slot.irq,
                t.status(),
                t.is_activated(),
                t.interrupt_status(),
                slot.notifier().map(|n| n.offloaded_queues()),
            )),
            Err(_) => out.push_str(&format!("pci slot {index}: transport lock poisoned\n")),
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

/// The project's own bootstrap kernel, and *only* that one.
///
/// [`boot_kernel`] falls back to the fetched Debian-installer kernel, which is
/// right for a test whose subject is the machine. It is wrong for a test whose
/// subject is what the **guest kernel's own input core** makes of a
/// descriptor: `CONFIG_INPUT_JOYDEV` is in
/// `guest/bootstrap-kernel/entangled.config` and is a module (so, absent —
/// this initramfs loads none) in a stock installer kernel. On the fallback
/// "the pad is a joystick" would come back false for a reason that has nothing
/// to do with the device, which is worse than not answering.
pub fn bootstrap_kernel() -> Option<PathBuf> {
    artifact("bootstrap/vmlinuz")
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
