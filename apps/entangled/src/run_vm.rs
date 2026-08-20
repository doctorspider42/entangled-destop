//! `entangled run` — boots a VM to the serial console with its virtio devices
//! attached (backlog MVP-1202..1206; display and the remaining device epics
//! plug in here as they land).
//!
//! # Two hosts, one run path (EPIC 17 phase 4)
//!
//! Everything user-visible — the config format, the window, the serial console
//! on stdout, the ACPI-S5 supervision, the automation hooks the installer
//! drives — is shared. What differs is machine assembly, and only where the
//! hypervisors genuinely differ, so it lives in one per-OS [`host`] module each
//! exporting the same `start()`:
//!
//! | | Linux (KVM) | Windows (WHP) |
//! |---|---|---|
//! | Interrupt chips | in-kernel (`KVM_CREATE_IRQCHIP`/`PIT2`) | `machine_x86::irqchip` in this process |
//! | Device interrupts | irqfd / `KVM_SIGNAL_MSI` | IOAPIC lines / `UserspaceMsiSink` over `WHvRequestInterrupt` |
//! | Queue kicks | ioeventfd + worker threads | inline on the vCPU thread |
//! | AP register setup | every vCPU (KVM's INIT discards it) | **BSP only** — an AP must stay in the reset state WHP created it in |
//! | Stop signal | SIGINT/SIGTERM via `sigaction` | Ctrl+C via `SetConsoleCtrlHandler` |
//! | VMs per process | any | one — WHP maps guest memory for one partition per process |

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use control_api::{NetworkBackend, VmConfig};
use machine_x86::bus::MachineBus;
use machine_x86::pflash::Pflash;
use virtio_core::VirtioDevice;
use vmm_core::{MachineConfig, RunOutcome, VmState};

/// Set by the SIGINT/SIGTERM (Linux) or console-control (Windows) handler; the
/// run loop polls it (MVP-1204).
static SHUTDOWN_REQUESTED: AtomicBool = AtomicBool::new(false);

/// Installs a host-appropriate handler that requests a clean VM stop instead of
/// killing the process mid-I/O.
#[cfg(target_os = "linux")]
fn install_signal_handlers() -> Result<(), String> {
    extern "C" fn on_termination_signal(_signum: libc::c_int) {
        // Async-signal-safe: a relaxed atomic store and nothing else.
        SHUTDOWN_REQUESTED.store(true, Ordering::Relaxed);
    }
    // SAFETY: sigaction with a handler that only stores an atomic; the
    // zeroed sigaction is a valid "no flags, empty mask" configuration.
    unsafe {
        let mut action: libc::sigaction = std::mem::zeroed();
        // sa_sigaction is declared as usize in libc; cast via a pointer to
        // satisfy both rustc's function_casts_as_integer and clippy.
        action.sa_sigaction = on_termination_signal as *const () as usize;
        for sig in [libc::SIGINT, libc::SIGTERM] {
            if libc::sigaction(sig, &action, std::ptr::null_mut()) != 0 {
                return Err(format!(
                    "failed to install handler for signal {sig}: {}",
                    std::io::Error::last_os_error()
                ));
            }
        }
    }
    Ok(())
}

/// The Windows counterpart: a console control handler for Ctrl+C/Ctrl+Break and
/// the console window closing. Same contract as the Linux signals — request a
/// clean stop, never die mid-I/O.
#[cfg(windows)]
fn install_signal_handlers() -> Result<(), String> {
    use windows::core::BOOL;
    use windows::Win32::System::Console::SetConsoleCtrlHandler;

    unsafe extern "system" fn on_console_control(_ctrl_type: u32) -> BOOL {
        // Runs on a system-injected thread: an atomic store and nothing else.
        // Every control type (Ctrl+C, Ctrl+Break, close, logoff, shutdown)
        // means the same thing here — stop the VM cleanly.
        SHUTDOWN_REQUESTED.store(true, Ordering::Relaxed);
        // "Handled": the process must not be terminated out from under the VM.
        BOOL::from(true)
    }
    // SAFETY: registering a handler that only stores an atomic; the routine
    // stays valid for the life of the process (it is a plain fn item).
    unsafe { SetConsoleCtrlHandler(Some(on_console_control), true) }
        .map_err(|e| format!("failed to install the console control handler: {e}"))
}

/// The VM's presentation surface: a real window when the host has one, the
/// windowless scanout otherwise (headless CI, --headless).
enum Presentation {
    Windowed(Box<display::DisplayHost>),
    Headless(display::DisplayHandle),
}

fn open_presentation(cfg: &VmConfig, headless: bool) -> Result<Presentation, String> {
    let display_cfg = display::DisplayConfig {
        width: cfg.display.width,
        height: cfg.display.height,
        scale: cfg.display.scale,
    };
    if !headless {
        match display::DisplayHost::new(display_cfg) {
            Ok(host) => {
                // Window UX (EPIC 15): input goes to the guest only while the
                // grab is active, so the shortcuts have to be discoverable —
                // the title bar repeats the important half of this.
                tracing::info!(
                    "window controls: click the image to grab input, Ctrl+Alt releases it, \
                     Ctrl+Alt+G toggles, F11 fullscreen, Ctrl+Alt+O 1:1, Ctrl+Alt+Q shuts down"
                );
                return Ok(Presentation::Windowed(Box::new(
                    host.with_title(format!("Entangled Desktop — {}", cfg.name)),
                )));
            }
            Err(e) => {
                tracing::warn!(error = %e, "cannot open a window, falling back to headless");
            }
        }
    }
    display::DisplayHandle::detached(cfg.display.width, cfg.display.height)
        .map(Presentation::Headless)
        .map_err(|e| e.to_string())
}

/// A host-side script: given everything the guest has printed on ttyS0 so far,
/// the bytes to type back at it (or `None` to keep waiting).
pub type SerialScript = Box<dyn FnMut(&str) -> Option<Vec<u8>> + Send>;

/// A host-side script driving the guest's serial console (UEFI-1804).
///
/// The one guest-input channel that both EDK2 and GRUB listen to is the 16550:
/// neither has a virtio-input driver, and the firmware's console *is* ttyS0 on
/// this machine (CloudHv ships no `VirtioGpuDxe`, so there is no GOP either).
/// So an unattended install that has to change the installer's kernel command
/// line — because `autoinstall` must be on it and the command line lives on
/// read-only media — types the boot commands into GRUB exactly as a person
/// would, and watches the echo to know it worked.
pub struct Automation {
    /// Called with everything the guest has written to ttyS0 so far, on every
    /// supervision tick (~50 ms). Returns bytes to type, or `None` to wait.
    /// The closure keeps its own progress state.
    pub script: SerialScript,
    /// Where to write the serial transcript when the VM stops. The install flow
    /// keeps it as the evidence for what the installer did.
    pub transcript: Option<PathBuf>,
}

/// How a run ended, for callers that need to tell "the guest finished" from
/// "we stopped it" — `entangled install` decides whether an installation
/// completed on exactly that difference.
#[derive(Debug, Clone, Default)]
pub struct RunReport {
    /// A vCPU ended with [`RunOutcome::Shutdown`]: the guest asked to power off
    /// (ACPI S5) rather than being stopped from the host.
    pub guest_shutdown: bool,
    /// Everything the guest wrote to ttyS0, when an [`Automation`] was attached
    /// (which is what makes the serial console observable to the host).
    pub serial: Option<String>,
}

/// Serial output that goes to the terminal *and* into a buffer the host can
/// read. Only used when an [`Automation`] is attached: an ordinary run must not
/// grow a copy of the guest's console in memory.
#[derive(Clone)]
struct Tee {
    buffer: Arc<Mutex<Vec<u8>>>,
}

impl std::io::Write for Tee {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        if let Ok(mut inner) = self.buffer.lock() {
            inner.extend_from_slice(buf);
        }
        let _ = std::io::stdout().write_all(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        std::io::stdout().flush()
    }
}

/// The devices a config describes, plus whatever the network backend wants
/// appended to a direct-Linux kernel command line.
struct BuiltDevices {
    devices: Vec<Box<dyn VirtioDevice>>,
    /// An `ip=` clause when the usernet backend is attached to a direct-Linux
    /// guest: the same numbers its DHCP server would hand out, for kernels
    /// carrying `CONFIG_IP_PNP` (harmlessly ignored otherwise). A UEFI guest
    /// gets nothing — the firmware owns the command line there.
    net_cmdline: Option<String>,
}

/// One virtio-blk device per `[[disk]]` entry, the configured network backend,
/// the GPU on the given scanout and the two input devices — in exactly this
/// order on both hosts, because device order is guest-visible naming
/// (`/dev/vda`, `00:01.0`).
fn build_devices(
    cfg: &VmConfig,
    display_handle: display::DisplayHandle,
    keyboard_sink: &mut Option<virtio_input::InputHandle>,
    tablet_sink: &mut Option<virtio_input::InputHandle>,
) -> Result<BuiltDevices, String> {
    let mut devices: Vec<Box<dyn VirtioDevice>> = Vec::with_capacity(cfg.disks.len() + 5);
    for disk in &cfg.disks {
        let device = virtio_block::BlockDevice::open(&disk.path, disk.writable)
            .map_err(|e| format!("cannot attach disk {}: {e}", disk.path.display()))?;
        tracing::info!(
            path = %disk.path.display(),
            writable = disk.writable,
            capacity_sectors = device.capacity_sectors(),
            "attaching virtio-blk device"
        );
        devices.push(Box::new(device));
    }

    // The CD-ROM, always read-only and always *after* the disks, so adding one
    // to an existing profile never renames /dev/vda (config validation already
    // pinned this to uefi + pci, where the firmware enumerates it itself).
    if let Some(cdrom) = &cfg.cdrom {
        let device = virtio_block::BlockDevice::open(&cdrom.path, false)
            .map_err(|e| format!("cannot attach cdrom {}: {e}", cdrom.path.display()))?;
        tracing::info!(
            path = %cdrom.path.display(),
            capacity_sectors = device.capacity_sectors(),
            "attaching virtio-blk cdrom (read-only)"
        );
        devices.push(Box::new(device));
    }

    // virtio-net from the [network] section (MVP-505, WHP-1704).
    let mut net_cmdline = None;
    if let Some(net) = &cfg.network {
        let mac = match &net.mac {
            Some(text) => parse_mac(text)?,
            None => virtio_net::MacAddr::derive(&cfg.name),
        };
        match net.backend {
            // The TAP interface must already exist (scripts/setup-tap.sh) —
            // entangled itself never needs CAP_NET_ADMIN, only the one-time
            // setup does.
            NetworkBackend::Tap => {
                #[cfg(target_os = "linux")]
                {
                    let interface = net.require_interface().map_err(|e| e.to_string())?;
                    let backend = virtio_net::TapBackend::open(interface)
                        .map_err(|e| format!("cannot open TAP '{interface}': {e}"))?;
                    tracing::info!(interface, mac = %mac, "attaching virtio-net device (tap)");
                    devices.push(Box::new(virtio_net::NetDevice::new(backend, mac)));
                }
                #[cfg(not(target_os = "linux"))]
                {
                    return Err(
                        "network backend \"tap\" is Linux-only (Windows has no TAP device, \
                         and the drivers that would provide one are GPL); use \
                         backend = \"usernet\""
                            .into(),
                    );
                }
            }
            NetworkBackend::Usernet => {
                use virtio_net::NetBackend as _;
                let backend =
                    virtio_net::UserNetBackend::with_defaults().map_err(|e| e.to_string())?;
                tracing::info!(
                    segment = backend.name(),
                    mac = %mac,
                    "attaching virtio-net device (user-mode NAT: DHCP, DNS relay, outbound TCP)"
                );
                net_cmdline = Some(backend.static_ip_cmdline());
                devices.push(Box::new(virtio_net::NetDevice::new(backend, mac)));
            }
        }
    }

    // Presentation + virtio-gpu (EPIC 7/8): the device pushes scanout pixels
    // into the display handle; with a window they appear on screen, headless
    // they are still screenshot-able. With `[display] virgl = true`
    // (ADR-0004) the device additionally executes 3D command streams through
    // the host's virglrenderer — or the run fails, loudly: a profile that
    // asked for 3D and silently got llvmpipe is the bug the option exists to
    // fix.
    if cfg.display.virgl {
        #[cfg(target_os = "linux")]
        {
            // GPU-012 (ADR-0004): by default the renderer runs in its own
            // process, so a crash inside the host GL stack degrades this VM to
            // 2D instead of killing it. `virgl_isolation = "in-process"` puts
            // virglrenderer back in the VMM for a host whose GL is trusted.
            let renderer: Box<dyn virtio_gpu::Renderer3d> = match cfg.display.virgl_isolation {
                control_api::VirglIsolation::Process => {
                    let renderer = virtio_gpu::remote::RemoteRenderer::spawn().map_err(|e| {
                        format!("[display] virgl = true with process isolation, but {e}")
                    })?;
                    tracing::info!(
                        pid = renderer.pid(),
                        "attaching virtio-gpu with an isolated virgl 3D renderer"
                    );
                    Box::new(renderer)
                }
                control_api::VirglIsolation::InProcess => {
                    let renderer = virtio_gpu::virgl::VirglRenderer::load()
                        .map_err(|e| format!("[display] virgl = true, but {e}"))?;
                    tracing::warn!(
                        "attaching virtio-gpu with an in-process virgl 3D renderer: a crash                          inside the host GL driver will take this VM down (ADR-0004 GPU-012)"
                    );
                    Box::new(renderer)
                }
            };
            devices.push(Box::new(virtio_gpu::GpuDevice::with_renderer(
                display_handle,
                renderer,
            )));
        }
        #[cfg(not(target_os = "linux"))]
        {
            return Err(
                "[display] virgl = true is Linux-only for now: the host renderer \
                 (virglrenderer) speaks EGL. See docs/adr/0004-virtio-gpu-3d.md \
                 for the Windows plan, or drop the option to run with 2D"
                    .into(),
            );
        }
    } else {
        devices.push(Box::new(virtio_gpu::GpuDevice::new(display_handle)));
    }

    // virtio-input keyboard + tablet (EPIC 9); handles stay on the host side
    // and are fed from the window's input capture.
    let keyboard = virtio_input::InputDevice::keyboard();
    let tablet = virtio_input::InputDevice::absolute_pointer();
    *keyboard_sink = Some(keyboard.handle());
    *tablet_sink = Some(tablet.handle());
    devices.push(Box::new(keyboard));
    devices.push(Box::new(tablet));
    Ok(BuiltDevices {
        devices,
        net_cmdline,
    })
}

/// A debug screenshot: write the scanout as PNG `after` this much VM runtime,
/// then refresh the same file every [`SCREENSHOT_REFRESH`] until the VM stops
/// — so a slow graphical boot can be watched from outside by re-reading one
/// path.
///
/// Best effort by design — the timer thread is detached, so a VM that stops
/// before the first write simply never produces the file. It exists to give an
/// unattended run (`--headless` on a CI box, a GNOME boot someone wants
/// evidence of) an artifact without instrumenting the guest.
#[derive(Debug, Clone)]
pub struct ScreenshotRequest {
    pub after: Duration,
    pub path: PathBuf,
}

/// How often the debug screenshot is refreshed after its first write.
const SCREENSHOT_REFRESH: Duration = Duration::from_secs(20);

pub fn run(
    cfg: VmConfig,
    headless: bool,
    screenshot: Option<ScreenshotRequest>,
) -> Result<(), String> {
    run_with(cfg, headless, None, screenshot).map(|_| ())
}

pub fn run_with(
    cfg: VmConfig,
    headless: bool,
    automation: Option<Automation>,
    screenshot: Option<ScreenshotRequest>,
) -> Result<RunReport, String> {
    let span = tracing::info_span!("vm", id = %cfg.name);
    let _guard = span.enter();
    install_signal_handlers()?;

    let machine = MachineConfig {
        memory_mib: cfg.memory_mib,
        vcpu_count: cfg.vcpus,
    };

    // With an automation script attached the console has to be readable by the
    // host as well as by the person watching, so it is tee'd into a buffer.
    let transcript = automation.as_ref().and_then(|a| a.transcript.clone());
    let captured: Option<Arc<Mutex<Vec<u8>>>> = automation
        .as_ref()
        .map(|_| Arc::new(Mutex::new(Vec::new())));
    let out: Box<dyn std::io::Write + Send> = match &captured {
        Some(buffer) => Box::new(Tee {
            buffer: Arc::clone(buffer),
        }),
        None => Box::new(std::io::stdout()),
    };

    // Presentation before devices: the GPU device is built around the handle.
    let presentation = open_presentation(&cfg, headless)?;
    let display_handle = match &presentation {
        Presentation::Windowed(host) => host.handle(),
        Presentation::Headless(handle) => handle.clone(),
    };
    let mut keyboard_sink = None;
    let mut tablet_sink = None;
    let built = build_devices(
        &cfg,
        display_handle.clone(),
        &mut keyboard_sink,
        &mut tablet_sink,
    )?;
    let (keyboard_sink, tablet_sink) = match (keyboard_sink, tablet_sink) {
        (Some(k), Some(t)) => (k, t),
        _ => return Err("input devices were not built".into()),
    };

    // The debug screenshot timer (--screenshot-after). Detached on purpose:
    // waiting for it would hold a finished VM open for the rest of the timer.
    if let Some(request) = screenshot {
        let handle = display_handle.clone();
        std::thread::Builder::new()
            .name("screenshot-timer".into())
            .spawn(move || {
                std::thread::sleep(request.after);
                loop {
                    match handle.screenshot(&request.path) {
                        Ok(()) => {
                            tracing::info!(path = %request.path.display(), "debug screenshot written")
                        }
                        Err(e) => {
                            tracing::warn!(path = %request.path.display(), error = %e,
                                "debug screenshot failed")
                        }
                    }
                    std::thread::sleep(SCREENSHOT_REFRESH);
                }
            })
            .map_err(|e| format!("cannot spawn the screenshot timer: {e}"))?;
    }

    // The UEFI variable store (UEFI-1804). Opened before the bus so the bus can
    // carry it, and before the firmware is loaded so a bad NVRAM path fails
    // before the VM starts rather than mid-boot.
    let pflash = match (cfg.boot.mode, &cfg.boot.nvram) {
        (control_api::BootMode::Uefi, Some(path)) => {
            Some(Arc::new(Mutex::new(Pflash::open(path).map_err(|e| {
                format!("cannot open the UEFI variable store: {e}")
            })?)))
        }
        (control_api::BootMode::DirectLinux, _) | (_, None) => None,
    };

    // Everything hypervisor-shaped happens in here; see the module docs for
    // exactly what differs between the hosts.
    let started = host::start(host::StartRequest {
        cfg: &cfg,
        machine: &machine,
        out,
        devices: built.devices,
        net_cmdline: built.net_cmdline,
        pflash: pflash.clone(),
    })?;
    let bus = started.bus;
    let threads = started.threads;

    // Lifecycle (MVP-1203): Created -> Running -> Stopping -> Stopped, any
    // vCPU error -> Crashed. Transitions are validated by VmState itself.
    let mut state = VmState::Created;
    tracing::info!(entry = format_args!("{:#x}", started.entry), mode = ?cfg.boot.mode, state = ?state, "VM created");
    state = state
        .transition(VmState::Running)
        .map_err(|e| e.to_string())?;
    tracing::info!(state = ?state, "VM running");

    // One predicate for both presentations: stop when asked, and on every tick
    // give the automation script a chance to look at the console and type.
    // `join_or_stop` wants `Fn`, so the script's state lives behind a mutex —
    // it is also called from the supervisor thread in the windowed case.
    let script = automation.map(|a| Mutex::new(a.script));
    let console = captured.clone();
    let script_bus = bus.clone();
    // The ACPI S5 latch, watched from *here* as well as by the vCPU threads.
    // A vCPU only notices it on its next exit, and a guest that has just powered
    // off has no reason to produce one: measured on an Ubuntu install, 126
    // seconds passed between `reboot: Power down` and the run loop noticing.
    // Nothing was wrong — the guest was in HLT and KVM was handling it in the
    // kernel — but "the installer finished" must not take two minutes to observe.
    let power = Arc::clone(bus.acpi_pm());
    let should_stop = move || {
        if let (Some(script), Some(console)) = (&script, &console) {
            let text = console
                .lock()
                .map(|buf| String::from_utf8_lossy(&buf).into_owned())
                .unwrap_or_default();
            if let Ok(mut script) = script.lock() {
                if let Some(keys) = script(&text) {
                    script_bus.push_serial_input(&keys);
                }
            }
        }
        SHUTDOWN_REQUESTED.load(Ordering::Relaxed) || power.is_shutdown_requested()
    };

    let outcomes = match presentation {
        Presentation::Headless(_) => threads.join_or_stop(&should_stop, Duration::from_millis(50)),
        Presentation::Windowed(host) => {
            // The winit event loop must own the main thread; VM supervision
            // and the input pump move to worker threads. Ctrl+Alt+Q and the
            // window close button request shutdown like SIGINT does.
            let input_queue = host.input_queue();
            let control_queue = host.control_queue();
            let pump = std::thread::spawn(move || {
                while !SHUTDOWN_REQUESTED.load(Ordering::Relaxed) {
                    for batch in input_queue.drain_batches() {
                        let split = virtio_input::split_batch(&batch);
                        if let Err(e) = keyboard_sink.push(&split.keyboard) {
                            tracing::warn!(error = %e, "keyboard event delivery failed");
                        }
                        if let Err(e) = tablet_sink.push(&split.pointer) {
                            tracing::warn!(error = %e, "pointer event delivery failed");
                        }
                    }
                    for event in control_queue.drain() {
                        match event {
                            display::ControlEvent::QuitRequested
                            | display::ControlEvent::WindowCloseRequested => {
                                tracing::info!(?event, "shutdown requested from the window");
                                SHUTDOWN_REQUESTED.store(true, Ordering::Relaxed);
                            }
                            display::ControlEvent::GrabToggled(grabbed) => {
                                tracing::info!(grabbed, "input grab toggled");
                            }
                        }
                    }
                    std::thread::sleep(Duration::from_millis(4));
                }
            });

            let (tx, rx) = std::sync::mpsc::channel();
            let supervisor_handle = display_handle.clone();
            let supervisor = std::thread::spawn(move || {
                let outcomes = threads.join_or_stop(&should_stop, Duration::from_millis(50));
                // The guest ended (or was stopped): close the window so the
                // event loop below returns.
                SHUTDOWN_REQUESTED.store(true, Ordering::Relaxed);
                supervisor_handle.shutdown();
                let _ = tx.send(outcomes);
            });

            if let Err(e) = host.run() {
                tracing::error!(error = %e, "display event loop failed");
                SHUTDOWN_REQUESTED.store(true, Ordering::Relaxed);
            }
            let outcomes = rx
                .recv()
                .map_err(|_| "vCPU supervisor thread disappeared".to_string())?;
            let _ = supervisor.join();
            let _ = pump.join();
            outcomes
        }
    };
    state = state
        .transition(VmState::Stopping)
        .map_err(|e| e.to_string())?;
    if SHUTDOWN_REQUESTED.load(Ordering::Relaxed) {
        tracing::info!("termination signal received, VM stopped");
    }

    if let Some(flash) = &pflash {
        match flash.lock() {
            Ok(flash) => {
                let stats = flash.stats();
                tracing::info!(
                    programmed_bytes = stats.programmed_bytes,
                    erased_blocks = stats.erased_blocks,
                    refused = stats.refused_programs,
                    store_errors = stats.store_errors,
                    "UEFI variable store written by the firmware"
                );
            }
            Err(_) => tracing::error!("pflash lock is poisoned; no variable-store report"),
        }
    }

    let mut failure = None;
    let mut report = RunReport {
        // The guest asked to power off, whether or not a vCPU got as far as
        // reporting it: the latch is the guest's request, `RunOutcome::Shutdown`
        // is one way of hearing about it.
        guest_shutdown: bus.acpi_pm().is_shutdown_requested(),
        ..RunReport::default()
    };
    for (i, outcome) in outcomes.iter().enumerate() {
        match outcome {
            Ok(RunOutcome::Shutdown) => {
                report.guest_shutdown = true;
                tracing::info!(vcpu = i, "guest shut down");
            }
            Ok(o) => tracing::info!(vcpu = i, outcome = ?o, "vCPU finished"),
            Err(e) => {
                tracing::error!(vcpu = i, error = %e, "vCPU failed");
                failure = Some(format!("vCPU {i} failed: {e}"));
            }
        }
    }
    state = state
        .transition(if failure.is_some() {
            VmState::Crashed
        } else {
            VmState::Stopped
        })
        .map_err(|e| e.to_string())?;
    tracing::info!(state = ?state, "VM finished");

    if let Some(console) = &captured {
        let text = console
            .lock()
            .map(|buf| String::from_utf8_lossy(&buf).into_owned())
            .unwrap_or_default();
        if let Some(path) = &transcript {
            // Best effort: a missing transcript must not fail a finished install,
            // but it is worth a warning because it is the run's only evidence.
            if let Err(e) = std::fs::write(path, &text) {
                tracing::warn!(path = %path.display(), error = %e, "cannot write the serial transcript");
            } else {
                tracing::info!(path = %path.display(), bytes = text.len(), "serial transcript written");
            }
        }
        report.serial = Some(text);
    }

    match failure {
        Some(message) => Err(message),
        None => Ok(report),
    }
}

/// What both hosts' `start()` take and return: the config, the serial sink, the
/// finished device list — and back, a machine bus wired to running vCPU threads.
mod host_api {
    use super::*;

    pub(super) struct StartRequest<'a> {
        pub cfg: &'a VmConfig,
        pub machine: &'a MachineConfig,
        pub out: Box<dyn std::io::Write + Send>,
        pub devices: Vec<Box<dyn VirtioDevice>>,
        /// Extra clause for a direct-Linux command line (the usernet `ip=`
        /// clause today); `None` when the network backend wants nothing.
        pub net_cmdline: Option<String>,
        pub pflash: Option<Arc<Mutex<Pflash>>>,
    }

    /// The command line a direct-Linux guest boots with: the profile's own,
    /// plus whatever the network backend asked for, plus the transport clauses.
    pub(super) fn direct_linux_cmdline(
        configured: &str,
        net_cmdline: Option<&str>,
        transport_clauses: &str,
    ) -> String {
        let with_net = extend_cmdline(configured, net_cmdline.unwrap_or(""));
        extend_cmdline(&with_net, transport_clauses)
    }
}

use host_api::direct_linux_cmdline;

/// KVM machine assembly (Linux): in-kernel interrupt chips, irqfd/ioeventfd
/// device wiring, register setup on every vCPU.
#[cfg(target_os = "linux")]
mod host {
    use super::*;
    use control_api::{BootMode, VirtioTransport};
    use machine_x86::boot as x86_boot;
    use machine_x86::serial::SerialConsole;
    use machine_x86::virtio::VirtioMmioBus;
    use machine_x86::virtio_pci::VirtioPciBus;
    use vmm_core::{spawn_vcpus, Hypervisor, Vm};

    pub(super) use super::host_api::StartRequest;
    pub(super) type Threads = vmm_core::VcpuThreads;

    pub(super) struct Started {
        pub bus: MachineBus,
        pub threads: Threads,
        /// Where vCPU0 begins executing, for the log line.
        pub entry: u64,
        /// The VM object owns guest RAM and the firmware ROM mappings; it must
        /// outlive the vCPU threads, so the caller holds it until they join.
        _vm: Vm,
    }

    pub(super) fn start(request: StartRequest<'_>) -> Result<Started, String> {
        let StartRequest {
            cfg,
            machine,
            out,
            devices,
            net_cmdline,
            pflash,
        } = request;
        let hv = Hypervisor::open().map_err(|e| e.to_string())?;
        let mut vm = Vm::new(&hv, machine).map_err(|e| e.to_string())?;

        // Interrupt topology: without an MP table the guest never programs the
        // IOAPIC and irqfd injections are intermittently lost (stalled first
        // virtio-blk read with INT_VRING left pending).
        machine_x86::mptable::write(vm.memory(), cfg.vcpus).map_err(|e| e.to_string())?;
        // ACPI tables alongside the MP table: MADT/FADT/DSDT give the guest SMP
        // topology, the PM block and a real S5 poweroff; Linux prefers them and
        // falls back to the MP table when absent.
        machine_x86::acpi::write(vm.memory(), cfg.vcpus).map_err(|e| e.to_string())?;

        let serial = SerialConsole::new(vm.fd(), out).map_err(|e| e.to_string())?;

        // Guest memory is shared with the devices; cloning a `GuestMemoryMmap`
        // shares the underlying regions rather than copying them.
        let mem = Arc::new(vm.memory().clone());
        // MVP-307: a bus needs a shared VM fd so it can deassign the queue-notify
        // ioeventfds again when it is dropped, after this borrow of `vm` is gone.
        //
        // Exactly one transport is attached (EPIC 19). On mmio the guest is *told*
        // where its devices are, through `virtio_mmio.device=` clauses; on pci it
        // enumerates them itself and the command line stays as configured.
        tracing::info!(transport = %cfg.transport, devices = devices.len(), "attaching virtio devices");
        let (bus, cmdline) = match cfg.transport {
            VirtioTransport::Mmio => {
                let virtio = VirtioMmioBus::attach(vm.fd_shared(), Arc::clone(&mem), devices)
                    .map_err(|e| e.to_string())?;
                let cmdline = direct_linux_cmdline(
                    &cfg.boot.cmdline,
                    net_cmdline.as_deref(),
                    &virtio.cmdline_clauses(),
                );
                (MachineBus::with_virtio(serial, virtio), cmdline)
            }
            VirtioTransport::Pci => {
                let pci = VirtioPciBus::attach(vm.fd_shared(), Arc::clone(&mem), devices)
                    .map_err(|e| e.to_string())?;
                let cmdline = direct_linux_cmdline(&cfg.boot.cmdline, net_cmdline.as_deref(), "");
                (MachineBus::with_virtio_pci(serial, pci), cmdline)
            }
        };
        // A UEFI firmware probes the ACPI PM timer and the RTC before it does
        // anything else (EPIC 18); a direct-Linux guest must not suddenly find them
        // where there were none. The PCI configuration ports are the real bus's when
        // the pci transport is in use, and the firmware stub's otherwise.
        let bus = match cfg.boot.mode {
            BootMode::DirectLinux => bus,
            BootMode::Uefi => bus.with_firmware_platform(),
        };
        let bus = match &pflash {
            Some(flash) => bus.with_pflash(Arc::clone(flash)),
            None => bus,
        };

        // Boot mode dispatch (EPIC 18 / ADR-0003). Everything above this point —
        // memory, IRQ chip, serial, the whole virtio window — is identical for both
        // modes; only how the vCPU starts differs.
        let mem_size = machine.memory_mib << 20;
        let (vcpus, entry) = match cfg.boot.mode {
            BootMode::DirectLinux => {
                let boot = linux_boot::BootConfig {
                    kernel: cfg
                        .boot
                        .require_kernel()
                        .map_err(|e| e.to_string())?
                        .clone(),
                    initramfs: cfg.boot.initramfs.clone(),
                    cmdline,
                };
                let loaded =
                    linux_boot::load(vm.memory(), &boot, mem_size).map_err(|e| e.to_string())?;
                let vcpus = vm.take_vcpus();
                for vcpu in &vcpus {
                    // Every vCPU: KVM's INIT discards this state on the APs, so
                    // handing it to all of them is free and keeps them uniform.
                    x86_boot::setup_long_mode_sregs(vm.memory(), vcpu)
                        .map_err(|e| e.to_string())?;
                    if vcpu.index == 0 {
                        // Only the boot CPU starts at the kernel entry; the others
                        // wait for INIT/SIPI from the guest.
                        x86_boot::setup_boot_regs(vcpu, loaded.entry, loaded.boot_params_addr)
                            .map_err(|e| e.to_string())?;
                    }
                }
                (vcpus, loaded.entry)
            }
            BootMode::Uefi => start_uefi(&mut vm, cfg, mem_size)?,
        };

        let threads = spawn_vcpus(vcpus, |_| Box::new(bus.clone())).map_err(|e| e.to_string())?;
        Ok(Started {
            bus,
            threads,
            entry,
            _vm: vm,
        })
    }

    /// Boots a UEFI firmware (EPIC 18, ADR-0003).
    ///
    /// Two shapes, decided by the image itself:
    ///
    /// * a PVH ELF (EDK2 CloudHv, rust-hypervisor-firmware) is loaded into guest
    ///   RAM and entered in 32-bit protected mode with `%ebx` at the
    ///   `hvm_start_info`;
    /// * anything else is a flash image, mapped as a read-only ROM ending at 4 GiB,
    ///   and the vCPUs are left in the state KVM created them in — which *is* the
    ///   architectural reset state (`CS.base 0xffff_0000`, `IP 0xfff0`), so the
    ///   first instruction fetch lands at `0xffff_fff0` inside the ROM.
    fn start_uefi(
        vm: &mut Vm,
        cfg: &VmConfig,
        mem_size: u64,
    ) -> Result<(Vec<vmm_core::Vcpu>, u64), String> {
        let path = cfg.boot.require_firmware().map_err(|e| e.to_string())?;
        let image = uefi_boot::FirmwareImage::read(path).map_err(|e| e.to_string())?;
        tracing::info!(
            firmware = %path.display(),
            bytes = image.len(),
            kind = ?image.kind(),
            "loading UEFI firmware"
        );

        match image.kind() {
            uefi_boot::FirmwareKind::PvhElf { .. } => {
                let boot = uefi_boot::load_pvh(vm.memory(), &image, mem_size)
                    .map_err(|e| e.to_string())?;
                let vcpus = vm.take_vcpus();
                for vcpu in &vcpus {
                    x86_boot::setup_pvh_sregs(vm.memory(), vcpu).map_err(|e| e.to_string())?;
                    if vcpu.index == 0 {
                        x86_boot::setup_pvh_regs(vcpu, boot.entry, boot.start_info_addr)
                            .map_err(|e| e.to_string())?;
                    }
                }
                Ok((vcpus, boot.entry))
            }
            uefi_boot::FirmwareKind::ResetVector => {
                super::refuse_nvram_with_reset_vector(cfg)?;
                let placement = uefi_boot::rom::place_at_top_of_32bit(image.len())
                    .map_err(|e| e.to_string())?;
                let rom = vm
                    .map_rom(placement.guest_addr, image.bytes())
                    .map_err(|e| e.to_string())?;
                tracing::info!(
                    addr = format_args!("{:#x}", rom.guest_addr),
                    end = format_args!("{:#x}", rom.guest_addr + rom.len),
                    read_only = rom.read_only,
                    "firmware ROM mapped; vCPUs stay in the architectural reset state"
                );
                // Deliberately no register setup: KVM_CREATE_VCPU already leaves
                // the vCPU in the reset state (verified in
                // crates/uefi-boot/tests/reset_vector.rs).
                Ok((vm.take_vcpus(), machine_x86::layout::RESET_VECTOR))
            }
        }
    }
}

/// WHP machine assembly (Windows): the userspace 8259/8254/IOAPIC, IOAPIC and
/// MSI device wiring, synchronous queue kicks, BSP-only register setup.
#[cfg(windows)]
mod host {
    use super::*;
    use control_api::{BootMode, VirtioTransport};
    use machine_x86::boot as x86_boot;
    use machine_x86::irqchip::UserspaceIrqChip;
    use machine_x86::serial::SerialConsole;
    use machine_x86::virtio::VirtioMmioBus;
    use machine_x86::virtio_pci::{PciInterruptMode, VirtioPciBus};
    use vmm_core::whp::{spawn_vcpus, WhpHypervisor, WhpOptions, WhpPartition, WhpVcpu};

    pub(super) use super::host_api::StartRequest;
    pub(super) type Threads = vmm_core::whp::WhpVcpuThreads;

    pub(super) struct Started {
        pub bus: MachineBus,
        pub threads: Threads,
        pub entry: u64,
        /// The partition owns guest RAM and the firmware ROM mappings; it must
        /// outlive the vCPU threads, so the caller holds it until they join.
        _vm: WhpPartition,
    }

    pub(super) fn start(request: StartRequest<'_>) -> Result<Started, String> {
        let StartRequest {
            cfg,
            machine,
            out,
            devices,
            net_cmdline,
            pflash,
        } = request;
        let hv = WhpHypervisor::open().map_err(|e| e.to_string())?;
        // Everything a real guest needs: local APIC emulation (interrupt
        // delivery, the `hlt` idle wait) and this machine's CPUID policy. One
        // partition per process — WHP maps guest memory for a single partition,
        // which is why the manager runs one `entangled` process per VM.
        let mut vm = WhpPartition::with_options(&hv, machine, WhpOptions::for_guest())
            .map_err(|e| e.to_string())?;

        // The same interrupt topology the KVM machine publishes: the guest must
        // not be able to tell the hosts apart from the tables.
        machine_x86::mptable::write(vm.memory(), cfg.vcpus).map_err(|e| e.to_string())?;
        machine_x86::acpi::write(vm.memory(), cfg.vcpus).map_err(|e| e.to_string())?;

        // WHP provides each vCPU's local APIC and nothing above it, so the
        // 8259/8254/IOAPIC live in this process (WHP-1703).
        let irqchip =
            UserspaceIrqChip::new(vm.interrupt_delivery(), cfg.vcpus).map_err(|e| e.to_string())?;
        let serial = SerialConsole::with_trigger(irqchip.serial_line(), out);

        let mem = Arc::new(vm.memory().clone());
        tracing::info!(transport = %cfg.transport, devices = devices.len(), "attaching virtio devices");
        let (bus, cmdline) = match cfg.transport {
            VirtioTransport::Mmio => {
                let virtio = VirtioMmioBus::attach_userspace(Arc::clone(&mem), devices, &irqchip)
                    .map_err(|e| e.to_string())?;
                let cmdline = direct_linux_cmdline(
                    &cfg.boot.cmdline,
                    net_cmdline.as_deref(),
                    &virtio.cmdline_clauses(),
                );
                (MachineBus::with_virtio(serial, virtio), cmdline)
            }
            VirtioTransport::Pci => {
                let pci = VirtioPciBus::attach_userspace(
                    Arc::clone(&mem),
                    devices,
                    &irqchip,
                    PciInterruptMode::from_env(),
                )
                .map_err(|e| e.to_string())?;
                let cmdline = direct_linux_cmdline(&cfg.boot.cmdline, net_cmdline.as_deref(), "");
                (MachineBus::with_virtio_pci(serial, pci), cmdline)
            }
        };
        let bus = match cfg.boot.mode {
            BootMode::DirectLinux => bus,
            BootMode::Uefi => bus.with_firmware_platform(),
        };
        let bus = match &pflash {
            Some(flash) => bus.with_pflash(Arc::clone(flash)),
            None => bus,
        }
        .with_irqchip(Arc::clone(&irqchip));

        let mem_size = machine.memory_mib << 20;
        let (vcpus, entry) = match cfg.boot.mode {
            BootMode::DirectLinux => {
                let boot = linux_boot::BootConfig {
                    kernel: cfg
                        .boot
                        .require_kernel()
                        .map_err(|e| e.to_string())?
                        .clone(),
                    initramfs: cfg.boot.initramfs.clone(),
                    cmdline,
                };
                let loaded =
                    linux_boot::load(vm.memory(), &boot, mem_size).map_err(|e| e.to_string())?;
                let vcpus = vm.take_vcpus();
                // **Boot CPU only.** WHP has no INIT of its own to discard host
                // register writes: an AP must be left in the reset state WHP
                // created it in, or the guest's INIT/SIPI never makes it
                // runnable (see `WhpPartition`'s SMP notes).
                {
                    let vcpu = &vcpus[0];
                    x86_boot::setup_long_mode_sregs(vm.memory(), vcpu)
                        .map_err(|e| e.to_string())?;
                    x86_boot::setup_boot_regs(vcpu, loaded.entry, loaded.boot_params_addr)
                        .map_err(|e| e.to_string())?;
                }
                (vcpus, loaded.entry)
            }
            BootMode::Uefi => start_uefi(&mut vm, cfg, mem_size)?,
        };

        let threads = spawn_vcpus(vcpus, |_| Box::new(bus.clone())).map_err(|e| e.to_string())?;
        Ok(Started {
            bus,
            threads,
            entry,
            _vm: vm,
        })
    }

    /// Boots a UEFI firmware on WHP (EPIC 18 on EPIC 17): the same two shapes
    /// as the KVM path, with the one WHP rule applied — PVH register state goes
    /// to the boot CPU only, and a reset-vector image needs *nothing* written
    /// because `WHvCreateVirtualProcessor` already leaves every VP in the
    /// architectural reset state.
    fn start_uefi(
        vm: &mut WhpPartition,
        cfg: &VmConfig,
        mem_size: u64,
    ) -> Result<(Vec<WhpVcpu>, u64), String> {
        let path = cfg.boot.require_firmware().map_err(|e| e.to_string())?;
        let image = uefi_boot::FirmwareImage::read(path).map_err(|e| e.to_string())?;
        tracing::info!(
            firmware = %path.display(),
            bytes = image.len(),
            kind = ?image.kind(),
            "loading UEFI firmware"
        );

        match image.kind() {
            uefi_boot::FirmwareKind::PvhElf { .. } => {
                let boot = uefi_boot::load_pvh(vm.memory(), &image, mem_size)
                    .map_err(|e| e.to_string())?;
                let vcpus = vm.take_vcpus();
                {
                    // Boot CPU only — the firmware's own INIT/SIPI sweep
                    // (`MpInitLib`) brings the APs up from the reset state WHP
                    // models for them.
                    let vcpu = &vcpus[0];
                    x86_boot::setup_pvh_sregs(vm.memory(), vcpu).map_err(|e| e.to_string())?;
                    x86_boot::setup_pvh_regs(vcpu, boot.entry, boot.start_info_addr)
                        .map_err(|e| e.to_string())?;
                }
                Ok((vcpus, boot.entry))
            }
            uefi_boot::FirmwareKind::ResetVector => {
                super::refuse_nvram_with_reset_vector(cfg)?;
                let placement = uefi_boot::rom::place_at_top_of_32bit(image.len())
                    .map_err(|e| e.to_string())?;
                vm.map_rom(placement.guest_addr, image.bytes())
                    .map_err(|e| e.to_string())?;
                tracing::info!(
                    addr = format_args!("{:#x}", placement.guest_addr),
                    end = format_args!("{:#x}", placement.guest_addr + image.len()),
                    "firmware ROM mapped; vCPUs stay in the architectural reset state"
                );
                Ok((vm.take_vcpus(), machine_x86::layout::RESET_VECTOR))
            }
        }
    }
}

/// A reset-vector image *is* its own flash and is mapped over the same window
/// as the pflash device; carrying both is a configuration contradiction, and
/// the same one on both hosts.
fn refuse_nvram_with_reset_vector(cfg: &VmConfig) -> Result<(), String> {
    if cfg.boot.nvram.is_some() {
        return Err(
            "boot.nvram cannot be used with a reset-vector firmware image: that \
             image *is* its own flash and is mapped over the same window as the \
             pflash device (machine_x86::layout::PFLASH_BASE). Drop boot.nvram, \
             or use a PVH firmware such as CLOUDHV.fd"
                .to_string(),
        );
    }
    Ok(())
}

/// Parses a "52:00:ab:01:02:03"-style MAC from the VM config.
fn parse_mac(text: &str) -> Result<virtio_net::MacAddr, String> {
    let mut bytes = [0u8; 6];
    let mut count = 0;
    for (i, part) in text.split(':').enumerate() {
        if i >= 6 {
            count = 7;
            break;
        }
        bytes[i] =
            u8::from_str_radix(part, 16).map_err(|_| format!("invalid MAC address '{text}'"))?;
        count = i + 1;
    }
    if count != 6 {
        return Err(format!("invalid MAC address '{text}': expected 6 octets"));
    }
    Ok(virtio_net::MacAddr(bytes))
}

/// Appends the `virtio_mmio.device=` clauses to the configured kernel command
/// line. There is no PCI bus to enumerate, so this is the only way the guest
/// learns where its devices are.
fn extend_cmdline(configured: &str, clauses: &str) -> String {
    let configured = configured.trim();
    if clauses.is_empty() {
        return configured.to_string();
    }
    if configured.is_empty() {
        return clauses.to_string();
    }
    format!("{configured} {clauses}")
}

#[cfg(test)]
mod tests {
    use super::{direct_linux_cmdline, extend_cmdline};

    /// `control_api` bounds `memory_mib` but deliberately does not depend on
    /// the machine crate, so this is the place that sees both: the largest
    /// allowed guest must actually build an E820 map, and — since the high-RAM
    /// split — one that puts everything above the 32-bit MMIO hole at 4 GiB
    /// rather than on top of the hole.
    #[test]
    fn the_largest_allowed_guest_builds_a_valid_e820_map() {
        let bytes = control_api::MAX_MEMORY_MIB << 20;
        let map = machine_x86::e820_map(bytes);
        assert_eq!(map.iter().map(|e| e.size).sum::<u64>(), bytes);
        for e in &map {
            let end = e.addr + e.size;
            assert!(
                end <= machine_x86::layout::MMIO_HOLE_START
                    || e.addr >= machine_x86::layout::TOP_OF_32BIT,
                "{:#x}..{end:#x} intrudes into the MMIO hole",
                e.addr
            );
        }
    }

    #[test]
    fn clauses_are_appended_once_and_separated() {
        assert_eq!(
            extend_cmdline(
                "console=ttyS0 root=/dev/vda1",
                "virtio_mmio.device=4K@0xd0000000:5"
            ),
            "console=ttyS0 root=/dev/vda1 virtio_mmio.device=4K@0xd0000000:5"
        );
    }

    #[test]
    fn handles_empty_inputs() {
        assert_eq!(extend_cmdline("  console=ttyS0 ", ""), "console=ttyS0");
        assert_eq!(extend_cmdline("", "a=1"), "a=1");
        assert_eq!(extend_cmdline("", ""), "");
    }

    /// The usernet `ip=` clause lands between the profile's command line and
    /// the transport clauses, and an absent one costs nothing.
    #[test]
    fn the_network_clause_is_spliced_into_the_command_line() {
        assert_eq!(
            direct_linux_cmdline(
                "console=ttyS0",
                Some("ip=192.168.74.15::192.168.74.1:255.255.255.0::eth0:off:192.168.74.1"),
                "virtio_mmio.device=4K@0xd0000000:5"
            ),
            "console=ttyS0 ip=192.168.74.15::192.168.74.1:255.255.255.0::eth0:off:192.168.74.1 \
             virtio_mmio.device=4K@0xd0000000:5"
        );
        assert_eq!(
            direct_linux_cmdline("console=ttyS0", None, ""),
            "console=ttyS0"
        );
    }
}
