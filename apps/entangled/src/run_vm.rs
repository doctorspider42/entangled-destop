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
//! | vCPU reset | architectural state written back by hand | the VP deleted and re-created |
//! | Guest reboot arrives as | a reset register write, or a triple fault | a reset register write only — WHP absorbs the fault |
//! | VMs per process | any | one — WHP maps guest memory for one partition per process |
//!
//! # Lifecycle (ADR-0005)
//!
//! A VM here can be frozen and rebooted in place. Everything that decides *when*
//! is shared: [`LifecycleSupervisor`] is the one thread that turns a request —
//! from the guest, the window's `Ctrl+Alt+P`/`Ctrl+Alt+R`, or the
//! `--control-stdin` channel — into the operation, and [`host_api::VmMachine`]
//! is what a pause freezes and a reset puts back. Only the two rows above
//! differ per host.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use control_api::{NetworkBackend, VmConfig};
use machine_x86::bus::MachineBus;
use machine_x86::pflash::Pflash;
use virtio_core::VirtioDevice;
use vmm_core::hv::{GuestClock, HostIrqChip, X86CpuState};
use vmm_core::{Lifecycle, MachineConfig, RunOutcome, VmState};

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
                     Ctrl+Alt+G toggles, F11 fullscreen, Ctrl+Alt+O 1:1, \
                     Ctrl+Alt+P pauses, Ctrl+Alt+R reboots, Ctrl+Alt+Q shuts down"
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
/// the GPU on the given scanout, the keyboard and tablet, and then the two
/// opt-in devices — the sound card and the gamepad — in exactly this order on
/// both hosts, because device order is guest-visible naming (`/dev/vda`,
/// `00:01.0`). Anything new goes on the *end*, for the same reason.
///
/// The full set is eight devices with one disk and no CD-ROM, which is exactly
/// `machine_x86::virtio::MAX_VIRTIO_SLOTS`; the bus refuses a ninth by name and
/// count when it is built.
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
            let mut gpu = virtio_gpu::GpuDevice::with_renderer(display_handle, renderer);
            // Phase 1's synchronous fences on request, which is how the
            // before/after measurement is taken (ADR-0004 phase 2).
            let fences = virtio_gpu::FenceMode::from_env();
            if !fences.is_deferred() {
                tracing::warn!(
                    var = virtio_gpu::FENCE_MODE_ENV,
                    "virtio-gpu fences forced synchronous: the guest gets no host/guest                      pipelining (this is the phase-1 baseline)"
                );
            }
            gpu.set_fence_mode(fences);
            gpu.set_refresh_hz(cfg.display.refresh_hz);
            gpu.set_frame_stats(cfg.display.frame_stats.clone());
            devices.push(Box::new(gpu));
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
        let mut gpu = virtio_gpu::GpuDevice::new(display_handle);
        gpu.set_refresh_hz(cfg.display.refresh_hz);
        gpu.set_frame_stats(cfg.display.frame_stats.clone());
        devices.push(Box::new(gpu));
    }

    // virtio-input keyboard + tablet (EPIC 9); handles stay on the host side
    // and are fed from the window's input capture.
    let keyboard = virtio_input::InputDevice::keyboard();
    let tablet = virtio_input::InputDevice::absolute_pointer();
    *keyboard_sink = Some(keyboard.handle());
    *tablet_sink = Some(tablet.handle());
    devices.push(Box::new(keyboard));
    devices.push(Box::new(tablet));

    // virtio-snd (GAME-2102), deliberately last: device order is guest-visible
    // naming, so a card added after the input devices never renames /dev/vda
    // nor shifts a PCI device number in a profile that already existed.
    if cfg.sound.enabled {
        let choice = match cfg.sound.backend {
            control_api::SoundBackend::Auto => virtio_sound::SinkChoice::Auto,
            control_api::SoundBackend::Null => virtio_sound::SinkChoice::Null,
            control_api::SoundBackend::Alsa => virtio_sound::SinkChoice::Alsa,
            control_api::SoundBackend::Wasapi => virtio_sound::SinkChoice::Wasapi,
        };
        // `auto` never fails — a machine with no speakers still boots — but an
        // explicit backend that is not there fails the run rather than
        // silently playing into nothing, exactly as `[display] virgl` does.
        let (sink, factory) = virtio_sound::open_sink(choice)
            .map_err(|e| format!("[sound] backend = \"{}\": {e}", cfg.sound.backend))?;
        // The capture half of the same backend. It never fails the run: a
        // machine with speakers and no microphone is completely ordinary, so
        // an absent one degrades to a capture device that records silence —
        // see `virtio_sound::open_source`.
        let (source, source_factory) = virtio_sound::open_source(choice)
            .map_err(|e| format!("[sound] backend = \"{}\": {e}", cfg.sound.backend))?;
        tracing::info!(
            %sink,
            %source,
            requested = %cfg.sound.backend,
            "attaching virtio-snd device"
        );
        devices.push(Box::new(
            virtio_sound::SoundDevice::new(sink, factory).with_source(source, source_factory),
        ));
    }

    // virtio-input gamepad (GAME-2104), last for the same reason the sound
    // card is second-to-last: appending never renames a disk nor moves a PCI
    // function that an existing profile already depends on.
    if cfg.gamepad.enabled {
        let choice = match cfg.gamepad.backend {
            control_api::GamepadBackend::Auto => virtio_input::SourceChoice::Auto,
            control_api::GamepadBackend::Null => virtio_input::SourceChoice::Null,
            control_api::GamepadBackend::Evdev => virtio_input::SourceChoice::Evdev,
            control_api::GamepadBackend::XInput => virtio_input::SourceChoice::XInput,
        };
        // `auto` never fails — a machine with no controller still boots, and a
        // pad plugged in later is picked up — but an explicitly named
        // mechanism that this host does not have fails the run, exactly as
        // `[sound] backend` and `[display] virgl` do.
        let (mechanism, factory) = virtio_input::open_source(choice)
            .map_err(|e| format!("[gamepad] backend = \"{}\": {e}", cfg.gamepad.backend))?;
        tracing::info!(
            mechanism,
            requested = %cfg.gamepad.backend,
            "attaching virtio-input gamepad"
        );
        devices.push(Box::new(virtio_input::InputDevice::gamepad_with_capture(
            factory,
        )));
    }

    Ok(BuiltDevices {
        devices,
        net_cmdline,
    })
}

/// Lifecycle requests waiting to be served (ADR-0005).
///
/// Flags rather than a queue: pausing twice in a row is pausing once, and
/// resetting twice in a row is resetting once. Whoever notices the request —
/// the window's input pump, the control channel — sets a flag and carries on;
/// the supervisor thread is the only one that blocks.
#[derive(Debug, Default)]
struct LifecycleRequests {
    /// The window's `Ctrl+Alt+P`: freeze, or continue if already frozen.
    pause_toggle: AtomicBool,
    /// The control channel's explicit `pause` / `resume`, which a program
    /// driving a VM wants instead of a toggle it would have to track.
    pause: AtomicBool,
    resume: AtomicBool,
    reset: AtomicBool,
    /// Where to write the VM (ADR-0006). Not a flag, because a `save` names a
    /// path and `Ctrl+Alt+S` uses the profile's default one; taking the last
    /// request wins is right for the same reason the flags coalesce.
    save: Mutex<Option<PathBuf>>,
}

impl LifecycleRequests {
    fn request_save(&self, path: PathBuf) {
        match self.save.lock() {
            Ok(mut slot) => *slot = Some(path),
            Err(poisoned) => *poisoned.into_inner() = Some(path),
        }
    }

    fn take_save(&self) -> Option<PathBuf> {
        match self.save.lock() {
            Ok(mut slot) => slot.take(),
            Err(poisoned) => poisoned.into_inner().take(),
        }
    }
}

/// Records a window control event: shutdown requests are acted on immediately
/// (a store to the same flag SIGINT sets), lifecycle requests are queued.
fn note_control_event(
    event: display::ControlEvent,
    requests: &LifecycleRequests,
    snapshot: Option<&Path>,
) {
    match event {
        display::ControlEvent::QuitRequested | display::ControlEvent::WindowCloseRequested => {
            tracing::info!(?event, "shutdown requested from the window");
            SHUTDOWN_REQUESTED.store(true, Ordering::Relaxed);
        }
        display::ControlEvent::GrabToggled(grabbed) => {
            tracing::info!(grabbed, "input grab toggled");
        }
        display::ControlEvent::PauseToggleRequested => {
            requests.pause_toggle.store(true, Ordering::Relaxed);
        }
        display::ControlEvent::ResetRequested => {
            requests.reset.store(true, Ordering::Relaxed);
        }
        display::ControlEvent::SaveRequested => match snapshot {
            Some(path) => {
                tracing::info!(path = %path.display(), "suspend requested from the window");
                requests.request_save(path.to_path_buf());
            }
            // Reachable only from a VM started without a profile path, which
            // `entangled run` always has. Saying so beats freezing the guest
            // and then having nowhere to put it.
            None => tracing::error!(
                "Ctrl+Alt+S: this VM has no snapshot path; start it with --snapshot <file>"
            ),
        },
    }
}

/// Prefix every line the control channel prints, so a program driving a VM can
/// tell its own answers apart from the guest's console — they share stdout,
/// because the guest console *is* what `entangled run` prints.
///
/// The prefix and the reply shapes are `control_api::control`'s, not this
/// module's: the other end of the pipe is `entangled-manager`, a separate
/// crate that has to recognise the same words. A literal on each side would
/// drift, and the failure mode is silent — a Suspend button that never learns
/// its file was written.
pub const CONTROL_PREFIX: &str = control_api::control::PREFIX;

/// Reads lifecycle commands from stdin, one per line (ADR-0005).
///
/// The VM's control surface for a program rather than a person: the window's
/// key bindings need someone at a keyboard, and `entangled-manager` drives the
/// CLI as a child process whose stdin it already owns. A pipe rather than a
/// socket because it is the one channel that exists identically on both hosts,
/// needs no path, no permissions and no cleanup, and dies with the process it
/// controls — which for "pause this VM" is exactly the lifetime wanted.
///
/// | Command | Effect |
/// |---|---|
/// | `pause` | freeze the VM (idempotent) |
/// | `resume` | let it continue (idempotent) |
/// | `reset` | reboot it in place |
/// | `save [path]` | suspend to `path` (or the profile's default) and exit |
/// | `type <text>` | type `<text>` and Enter on the guest's serial console |
/// | `status` | print the current [`RunState`](vmm_core::RunState) |
///
/// `type` is the same host-to-guest path the unattended installer uses to drive
/// GRUB (`MachineBus::push_serial_input`), exposed rather than kept private:
/// a headless VM whose console can only be watched and never answered is half a
/// console. It is also what lets the reboot acceptance log into a guest and ask
/// it to restart, which is the thing being tested.
///
/// Unknown commands are reported and ignored — never fatal. The thread is
/// detached: it ends when stdin closes, which for a child process is when its
/// parent goes away.
fn spawn_control_channel(
    bus: MachineBus,
    requests: Arc<LifecycleRequests>,
    lifecycle: Arc<Lifecycle>,
    snapshot: Option<PathBuf>,
) {
    let spawned = std::thread::Builder::new()
        .name("control".into())
        .spawn(move || {
            use std::io::BufRead as _;
            let stdin = std::io::stdin();
            for line in stdin.lock().lines() {
                let Ok(line) = line else { break };
                let line = line.trim();
                let (command, argument) = match line.split_once(char::is_whitespace) {
                    Some((command, rest)) => (command, rest),
                    None => (line, ""),
                };
                // The command words are `control_api::control`'s constants, so
                // the writer at the other end of the pipe and the reader here
                // cannot drift apart.
                use control_api::control::{
                    CMD_PAUSE, CMD_RESET, CMD_RESUME, CMD_SAVE, CMD_STATUS, CMD_TYPE,
                };
                match command {
                    "" => {}
                    CMD_PAUSE => requests.pause.store(true, Ordering::Relaxed),
                    CMD_RESUME => requests.resume.store(true, Ordering::Relaxed),
                    CMD_RESET => requests.reset.store(true, Ordering::Relaxed),
                    CMD_SAVE => {
                        let path = match argument.trim() {
                            "" => snapshot.clone(),
                            given => Some(PathBuf::from(given)),
                        };
                        match path {
                            Some(path) => requests.request_save(path),
                            None => {
                                println!(
                                    "{CONTROL_PREFIX} error save needs a path (this VM has no \
                                     default one)"
                                );
                                continue;
                            }
                        }
                    }
                    CMD_TYPE => {
                        // Carriage return, not newline: the guest's terminal
                        // discipline is what turns it into one, and a bare `\n`
                        // is not what a serial keyboard sends.
                        let mut bytes = argument.as_bytes().to_vec();
                        bytes.push(b'\r');
                        bus.push_serial_input(&bytes);
                    }
                    CMD_STATUS => {
                        println!("{CONTROL_PREFIX} state={:?}", lifecycle.state());
                    }
                    other => {
                        println!("{CONTROL_PREFIX} unknown command {other:?}");
                        continue;
                    }
                }
                if !command.is_empty() {
                    println!("{CONTROL_PREFIX} ok {command}");
                }
                let _ = std::io::Write::flush(&mut std::io::stdout());
            }
            tracing::debug!("the control channel closed");
        });
    if let Err(error) = spawned {
        tracing::error!(%error, "cannot start the control channel; --control-stdin is inert");
    }
}

/// How long a pause waits for the host's device workers to finish the work
/// they already had in hand (ADR-0005).
///
/// A worker that is stuck — a host disk that has stopped answering — must not
/// be able to wedge a pause, so this is bounded and the pause proceeds with a
/// warning. Generous next to the milliseconds a queue drain takes.
const QUIESCE_SETTLE: Duration = Duration::from_secs(5);

/// How often the supervisor looks for a lifecycle request.
///
/// The latency a person notices between pressing Ctrl+Alt+P and the VM
/// freezing, and — more importantly — the delay between a guest writing its
/// reset register and the host restarting it. Small enough that a reboot looks
/// instant, large enough that an idle VM costs nothing.
const LIFECYCLE_POLL: Duration = Duration::from_millis(20);

/// The thread that serves lifecycle requests, and the only place `pause`,
/// `resume` and `reset` are called from (ADR-0005).
struct LifecycleSupervisor {
    stop: Arc<AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl LifecycleSupervisor {
    fn start(
        lifecycle: Arc<Lifecycle>,
        requests: Arc<LifecycleRequests>,
        vm_state: Arc<Mutex<VmState>>,
    ) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&stop);
        let handle = std::thread::Builder::new()
            .name("lifecycle".into())
            .spawn(move || {
                while !flag.load(Ordering::Relaxed) && !SHUTDOWN_REQUESTED.load(Ordering::Relaxed) {
                    // Suspend wins over everything, and ends the VM (ADR-0006).
                    // Nothing else is worth doing to a machine that is about to
                    // stop existing in this process, and a reset served first
                    // would put a *rebooted* guest in the file.
                    if let Some(path) = requests.take_save() {
                        Self::suspend(&vm_state, &lifecycle, &path);
                        // Whether it worked or not: a failed suspend leaves the
                        // VM paused and the user without the file they asked
                        // for, and running on as if nothing happened would be
                        // the one outcome nobody can act on.
                        SHUTDOWN_REQUESTED.store(true, Ordering::Relaxed);
                        return;
                    }
                    // Reset wins over pause: a guest that asked to reboot while
                    // someone was holding the pause key wants to reboot, and
                    // `Lifecycle::reset` works from either state.
                    let guest_asked = lifecycle.take_guest_reset();
                    if guest_asked || requests.reset.swap(false, Ordering::Relaxed) {
                        Self::serve(
                            &vm_state,
                            VmState::Resetting,
                            || lifecycle.reset(),
                            || Some(VmState::Running),
                        );
                    } else {
                        // The window sends a toggle (it cannot know the state);
                        // the control channel sends what it means. Both end up
                        // here, and the toggle is resolved against the truth.
                        let toggled = requests.pause_toggle.swap(false, Ordering::Relaxed);
                        let paused = lifecycle.is_paused();
                        let pause =
                            requests.pause.swap(false, Ordering::Relaxed) || (toggled && !paused);
                        let resume =
                            requests.resume.swap(false, Ordering::Relaxed) || (toggled && paused);
                        if pause {
                            Self::serve(&vm_state, VmState::Paused, || lifecycle.pause(), || None);
                        } else if resume {
                            Self::serve(
                                &vm_state,
                                VmState::Running,
                                || lifecycle.resume(),
                                || None,
                            );
                        }
                    }
                    std::thread::sleep(LIFECYCLE_POLL);
                }
            });
        // Not fatal — the VM runs perfectly well without one — but it is the
        // difference between a guest's Restart rebooting and the VM stopping
        // 30 seconds later with "no supervisor served the reset request", so it
        // must not be silent.
        let handle = match handle {
            Ok(handle) => Some(handle),
            Err(error) => {
                tracing::error!(
                    %error,
                    "cannot start the lifecycle supervisor: this VM cannot be paused or \
                     rebooted, and a guest that asks to restart will stop instead"
                );
                None
            }
        };
        Self { stop, handle }
    }

    /// Suspends the VM to `path` and reports what happened on stdout as well
    /// as in the log (ADR-0006).
    ///
    /// On stdout because the control channel's caller is a program that asked
    /// for this and has no other way to learn the file is complete — and
    /// because a suspend is the one lifecycle operation whose *result* is a
    /// thing on disk rather than a state the VM is in.
    fn suspend(vm_state: &Mutex<VmState>, lifecycle: &Lifecycle, path: &Path) {
        Self::advance(vm_state, VmState::Suspending);
        match lifecycle.save(path) {
            Ok(summary) => {
                Self::advance(vm_state, VmState::Suspended);
                tracing::info!(path = %path.display(), %summary, "VM suspended");
                println!("{CONTROL_PREFIX} saved {} {summary}", path.display());
            }
            Err(error) => {
                tracing::error!(path = %path.display(), %error, "suspend failed");
                println!("{CONTROL_PREFIX} error save {error}");
                // The seam left the VM paused; the state machine has to agree,
                // and `Suspending -> Paused` is exactly that transition.
                Self::advance(vm_state, VmState::Paused);
            }
        }
        let _ = std::io::Write::flush(&mut std::io::stdout());
    }

    /// Runs one lifecycle operation and keeps [`VmState`] honest about it.
    ///
    /// `during` is the state the VM is in while the operation runs, `after` the
    /// one it settles into (`None` means "stay in `during`" — a pause holds).
    /// A refused transition is reported rather than papered over: it would mean
    /// the state machine and the seam disagree, which is a bug in one of them.
    fn serve(
        vm_state: &Mutex<VmState>,
        during: VmState,
        operation: impl FnOnce() -> Result<(), vmm_core::LifecycleError>,
        after: impl FnOnce() -> Option<VmState>,
    ) {
        let before = Self::read(vm_state);
        Self::advance(vm_state, during);
        match operation() {
            Ok(()) => {
                if let Some(next) = after() {
                    Self::advance(vm_state, next);
                }
            }
            Err(error) => {
                tracing::error!(%error, "lifecycle request failed");
                // Put the state back, or the machine would go on claiming to be
                // `Paused` while its vCPUs run — a failed pause is not a pause.
                // A failed *reset* has left the seam in `Stopping` and the VM
                // about to die; restoring `Running` here is still right, because
                // `Running -> Stopping` is exactly the transition the shutdown
                // path below is about to make. Where the way back is not legal,
                // `advance` refuses and says so rather than inventing one.
                Self::advance(vm_state, before);
            }
        }
    }

    fn read(vm_state: &Mutex<VmState>) -> VmState {
        match vm_state.lock() {
            Ok(guard) => *guard,
            Err(poisoned) => *poisoned.into_inner(),
        }
    }

    fn advance(vm_state: &Mutex<VmState>, to: VmState) {
        let mut guard = match vm_state.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        if *guard == to {
            return;
        }
        match guard.transition(to) {
            Ok(next) => {
                *guard = next;
                tracing::info!(state = ?next, "VM state");
            }
            Err(error) => tracing::error!(%error, "refusing an invalid VM state transition"),
        }
    }
}

impl Drop for LifecycleSupervisor {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
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

/// Everything about *how* to run a VM that is not the VM itself.
///
/// A struct rather than five more parameters: the run path has grown a
/// lifecycle control channel, a debug screenshot and now a snapshot on either
/// end of it, and a call site that reads `run_with(cfg, false, None, None,
/// false, None, None)` tells the reader nothing.
#[derive(Debug, Default)]
pub struct RunOptions {
    /// No window; the VM still runs with an off-screen scanout.
    pub headless: bool,
    /// Read lifecycle commands from this process's stdin (ADR-0005).
    pub control_stdin: bool,
    pub screenshot: Option<ScreenshotRequest>,
    /// Where `Ctrl+Alt+S` and a bare `save` write this VM (ADR-0006).
    pub snapshot: Option<PathBuf>,
    /// Start this VM **from** a snapshot instead of from its boot images.
    ///
    /// The machine is assembled exactly as it would be for a cold boot — same
    /// memory size, same devices in the same order — and then, instead of
    /// loading a kernel or a firmware, the snapshot is loaded over it.
    pub restore: Option<PathBuf>,
}

pub fn run(cfg: VmConfig, options: RunOptions) -> Result<(), String> {
    run_with(cfg, None, options).map(|_| ())
}

/// [`run`] with a host-side console script attached.
pub fn run_with(
    cfg: VmConfig,
    automation: Option<Automation>,
    options: RunOptions,
) -> Result<RunReport, String> {
    let RunOptions {
        headless,
        control_stdin,
        screenshot,
        snapshot,
        restore,
    } = options;
    let span = tracing::info_span!("vm", id = %cfg.name);
    let _guard = span.enter();
    install_signal_handlers()?;

    // The pause/reset seam (ADR-0005), created before the machine because the
    // machine is attached to it and the vCPU threads are spawned with it.
    let lifecycle = Lifecycle::new(cfg.vcpus);

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
        lifecycle: Arc::clone(&lifecycle),
        restore: restore.as_deref(),
    })?;
    let bus = started.bus;
    let threads = started.threads;
    let quiesce = started.quiesce;

    // Lifecycle (MVP-1203, ADR-0005): Created -> Running -> Stopping -> Stopped,
    // with Running <-> Paused and Running -> Resetting -> Running in between,
    // and any vCPU error -> Crashed. Transitions are validated by VmState
    // itself; the shared cell is what lets the lifecycle supervisor below drive
    // the middle two while this thread waits for the VM to end.
    let mut state = VmState::Created;
    tracing::info!(
        entry = format_args!("{:#x}", started.entry),
        mode = ?cfg.boot.mode,
        restored = restore.is_some(),
        state = ?state,
        "VM created"
    );
    state = state
        .transition(VmState::Running)
        .map_err(|e| e.to_string())?;
    tracing::info!(state = ?state, "VM running");
    let vm_state = Arc::new(Mutex::new(state));

    // The lifecycle supervisor: the one thread that turns a *request* to pause,
    // resume or reset into the thing itself. Requests reach it from three
    // places — the guest (a write to a reset control, or a triple fault), the
    // window (Ctrl+Alt+P / Ctrl+Alt+R) and, on a reset that the machine cannot
    // serve, nowhere at all.
    //
    // A thread of its own rather than a branch in `should_stop` because both
    // operations block until every vCPU has acknowledged, and the predicate
    // `join_or_stop` polls must stay cheap: a pause that took a second would
    // otherwise be a second in which the console script stopped being typed.
    let requests = Arc::new(LifecycleRequests::default());
    let supervisor = LifecycleSupervisor::start(
        Arc::clone(&lifecycle),
        Arc::clone(&requests),
        Arc::clone(&vm_state),
    );
    if control_stdin {
        spawn_control_channel(
            bus.clone(),
            Arc::clone(&requests),
            Arc::clone(&lifecycle),
            snapshot.clone(),
        );
    }

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
            let pump_requests = Arc::clone(&requests);
            let pump_quiesce = Arc::clone(&quiesce);
            let pump_snapshot = snapshot.clone();
            let pump = std::thread::spawn(move || {
                while !SHUTDOWN_REQUESTED.load(Ordering::Relaxed) {
                    // A paused VM must not accumulate a burst of input to
                    // deliver on resume, and pushing an event writes the guest's
                    // event ring — which is precisely what a pause forbids
                    // (ADR-0005). The events are dropped rather than queued: the
                    // window is not grabbed while frozen anyway.
                    //
                    // A pass rather than a flag check, and held across the
                    // pushes: a pause that landed between the check and the push
                    // would otherwise be told the machine was quiet while this
                    // thread was writing to it. Control events are still read —
                    // they are how the VM gets un-paused.
                    let Some(pass) = pump_quiesce.try_enter() else {
                        input_queue.drain_batches();
                        for event in control_queue.drain() {
                            note_control_event(event, &pump_requests, pump_snapshot.as_deref());
                        }
                        std::thread::sleep(Duration::from_millis(20));
                        continue;
                    };
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
                        note_control_event(event, &pump_requests, pump_snapshot.as_deref());
                    }
                    drop(pass);
                    std::thread::sleep(Duration::from_millis(4));
                }
            });

            let (tx, rx) = std::sync::mpsc::channel();
            let supervisor_handle = display_handle.clone();
            let vcpu_supervisor = std::thread::spawn(move || {
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
            let _ = vcpu_supervisor.join();
            let _ = pump.join();
            outcomes
        }
    };
    // The lifecycle supervisor is joined before the state machine moves on, so
    // it cannot transition underneath the shutdown path.
    drop(supervisor);
    state = match vm_state.lock() {
        Ok(guard) => *guard,
        Err(poisoned) => *poisoned.into_inner(),
    };
    let resets = lifecycle.resets();
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
    tracing::info!(state = ?state, resets, "VM finished");

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
        /// The pause/reset seam (ADR-0005). Each host attaches the assembled
        /// machine to it and hands it to its `spawn_vcpus`.
        pub lifecycle: Arc<Lifecycle>,
        /// Start this VM from a snapshot instead of from its boot images
        /// (ADR-0006).
        pub restore: Option<&'a Path>,
    }

    /// What has to go back into guest memory to start this VM — at boot, and
    /// again at every reset.
    ///
    /// The point of naming it is that **reset re-runs exactly the boot path**.
    /// A reboot that loaded the kernel through a second, "reset-only" code path
    /// would be a second thing to keep correct, and the first thing to rot.
    pub(super) enum BootPlan {
        DirectLinux {
            boot: linux_boot::BootConfig,
            mem_size: u64,
        },
        /// A PVH ELF firmware (EDK2 CloudHv). Reloading it is what makes a UEFI
        /// reboot land back in the firmware: it re-reads its variable store out
        /// of pflash — which a reset deliberately does *not* clear — and runs
        /// its boot manager again.
        Pvh {
            image: uefi_boot::FirmwareImage,
            mem_size: u64,
        },
        /// A flash image mapped as a read-only ROM at the top of the 32-bit
        /// address space. Nothing to reload: the ROM is a host mapping, the
        /// guest cannot have changed it, and the architectural reset state the
        /// vCPU is put back into already points its first fetch at it.
        ResetVector,
    }

    /// Where a vCPU starts, and in which of the three start-of-day states.
    #[derive(Debug, Clone, Copy)]
    pub(super) struct BootEntry {
        pub entry: u64,
        /// `rsi` (boot_params) for direct Linux, `rbx` (hvm_start_info) for
        /// PVH, unused for a reset-vector ROM.
        pub argument: u64,
        pub kind: BootEntryKind,
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub(super) enum BootEntryKind {
        LongMode,
        Pvh,
        ResetVector,
    }

    /// Puts the boot images into guest memory and reports where execution
    /// starts. Called once at boot and once per reset.
    pub(super) fn load_boot(
        mem: &vmm_core::GuestMem,
        plan: &BootPlan,
    ) -> Result<BootEntry, String> {
        match plan {
            BootPlan::DirectLinux { boot, mem_size } => {
                let loaded = linux_boot::load(mem, boot, *mem_size).map_err(|e| e.to_string())?;
                Ok(BootEntry {
                    entry: loaded.entry,
                    argument: loaded.boot_params_addr,
                    kind: BootEntryKind::LongMode,
                })
            }
            BootPlan::Pvh { image, mem_size } => {
                let boot = uefi_boot::load_pvh(mem, image, *mem_size).map_err(|e| e.to_string())?;
                Ok(BootEntry {
                    entry: boot.entry,
                    argument: boot.start_info_addr,
                    kind: BootEntryKind::Pvh,
                })
            }
            BootPlan::ResetVector => Ok(BootEntry {
                entry: machine_x86::layout::RESET_VECTOR,
                argument: 0,
                kind: BootEntryKind::ResetVector,
            }),
        }
    }

    /// Puts one vCPU into the start-of-day state `entry` describes.
    ///
    /// `is_boot_cpu` decides how much: the boot CPU gets its segments *and* its
    /// general-purpose registers, an application processor only the segments —
    /// it is waiting for the guest's own INIT/SIPI either way.
    pub(super) fn apply_boot_state(
        mem: &vmm_core::GuestMem,
        entry: &BootEntry,
        vcpu: &dyn vmm_core::hv::VcpuRegisters,
        is_boot_cpu: bool,
    ) -> Result<(), String> {
        use machine_x86::boot as x86_boot;
        match entry.kind {
            BootEntryKind::LongMode => {
                x86_boot::setup_long_mode_sregs(mem, vcpu).map_err(|e| e.to_string())?;
                if is_boot_cpu {
                    x86_boot::setup_boot_regs(vcpu, entry.entry, entry.argument)
                        .map_err(|e| e.to_string())?;
                }
            }
            BootEntryKind::Pvh => {
                x86_boot::setup_pvh_sregs(mem, vcpu).map_err(|e| e.to_string())?;
                if is_boot_cpu {
                    x86_boot::setup_pvh_regs(vcpu, entry.entry, entry.argument)
                        .map_err(|e| e.to_string())?;
                }
            }
            // Nothing to write: `KVM_CREATE_VCPU` / `WHvCreateVirtualProcessor`
            // — and, after a reboot, `ResettableVcpu::reset_arch_state` — leave
            // the vCPU in the architectural reset state, whose first fetch is
            // already at 0xffff_fff0 inside the ROM.
            BootEntryKind::ResetVector => {}
        }
        Ok(())
    }

    /// The [`BootEntry`] a *restored* VM records.
    ///
    /// Nothing is applied from it: the vCPUs are already exactly where the
    /// snapshot left them, which is the whole point. It exists because the
    /// machine behind the lifecycle seam keeps one, and because a later
    /// in-place reset recomputes it from the boot plan *before* it is used —
    /// `reset_machine` runs `load_boot` and overwrites this. `ResetVector` is
    /// the kind that applies nothing, which is the honest description of a
    /// vCPU that needs nothing applied.
    pub(super) fn restored_entry(cpus: &[X86CpuState]) -> BootEntry {
        BootEntry {
            entry: cpus.first().map(|c| c.registers.rip).unwrap_or(0),
            argument: 0,
            kind: BootEntryKind::ResetVector,
        }
    }

    /// The machine behind the lifecycle seam (ADR-0005): what a pause has to
    /// freeze, and what a reset has to put back.
    pub(super) struct VmMachine {
        pub bus: MachineBus,
        pub mem: Arc<vmm_core::GuestMem>,
        pub quiesce: Arc<virtio_core::Quiesce>,
        pub vcpus: u32,
        pub plan: BootPlan,
        /// Recomputed by every reset: a reloaded kernel does not have to land
        /// on the same entry point, and a firmware certainly does not.
        pub entry: Mutex<BootEntry>,
        /// The profile this VM was started from, for a snapshot's metadata and
        /// its fingerprints (ADR-0006).
        pub cfg: VmConfig,
        /// The VM-wide paravirtual clock, where the hypervisor has one.
        pub clock: Option<Arc<dyn GuestClock>>,
        /// The hypervisor's **own** interrupt controllers, where it has them:
        /// KVM's in-kernel 8259 pair, IOAPIC and 8254. `None` on a host whose
        /// chips are in this process, where they are saved with the rest of the
        /// machine.
        pub host_irqchip: Option<Arc<dyn HostIrqChip>>,
    }

    impl VmMachine {
        fn current_entry(&self) -> BootEntry {
            match self.entry.lock() {
                Ok(entry) => *entry,
                Err(poisoned) => *poisoned.into_inner(),
            }
        }
    }

    impl vmm_core::MachineLifecycle for VmMachine {
        /// Everything that touches guest memory without a vCPU behind it stops:
        /// the queue workers and virtio-net's receive thread through the gate,
        /// the 8254's timer thread and the ACPI PM timer through the bus.
        fn quiesce(&self) {
            self.quiesce.pause();
            self.bus.set_paused(true);
            // Closing the gate stops work that has not started; this waits for
            // the work already in hand, which is what makes "paused" true of
            // guest memory and not only of the guest.
            self.quiesce.wait_until_idle(QUIESCE_SETTLE);
        }

        fn unquiesce(&self) {
            self.bus.set_paused(false);
            self.quiesce.resume();
        }

        /// Runs on the requesting thread with every vCPU parked, so it may take
        /// any device lock and write guest memory freely.
        ///
        /// The order is the machine's dependency order: devices first (they are
        /// what could still be pointing into guest RAM), then the firmware
        /// tables, then the boot images — because `linux_boot::load` writes its
        /// `boot_params` over the same low memory the previous guest was using.
        fn reset_machine(&self) -> Result<(), String> {
            self.bus.reset_devices();
            machine_x86::mptable::write(self.mem.as_ref(), self.vcpus)
                .map_err(|e| format!("cannot rewrite the MP table: {e}"))?;
            machine_x86::acpi::write(self.mem.as_ref(), self.vcpus)
                .map_err(|e| format!("cannot rewrite the ACPI tables: {e}"))?;
            let entry = load_boot(self.mem.as_ref(), &self.plan)?;
            match self.entry.lock() {
                Ok(mut slot) => *slot = entry,
                Err(poisoned) => *poisoned.into_inner() = entry,
            }
            tracing::info!(
                entry = format_args!("{:#x}", entry.entry),
                kind = ?entry.kind,
                "machine reset: devices at power-on, tables and boot images reloaded"
            );
            Ok(())
        }

        /// Boot-CPU only, on **both** hosts.
        ///
        /// On WHP because an application processor must be left exactly as
        /// `WHvCreateVirtualProcessor` made it or the guest's INIT/SIPI never
        /// makes it runnable (ADR-0002 phase 4) — and `reset_arch_state` has
        /// just re-created it precisely to get back there. On KVM because the
        /// segment state a host writes to an AP is discarded by the INIT that
        /// starts it anyway; the AP is back at `KVM_MP_STATE_UNINITIALIZED`,
        /// which is where `KVM_CREATE_VCPU` left it on the first boot.
        fn reset_vcpu(
            &self,
            index: u32,
            vcpu: &dyn vmm_core::hv::VcpuRegisters,
        ) -> Result<(), String> {
            if index != 0 {
                return Ok(());
            }
            apply_boot_state(self.mem.as_ref(), &self.current_entry(), vcpu, true)
        }

        /// Writes the whole machine to `path` (ADR-0006).
        ///
        /// Runs on the requesting thread with every vCPU parked and every host
        /// worker quiesced — the same contract as `reset_machine`, and the
        /// reason the fingerprints taken here are stable: nothing is writing
        /// the disks any more, so their size and mtime will still be what this
        /// records when the process exits.
        ///
        /// The clock is read last of the small state, as close as possible to
        /// the memory dump it will be restored alongside.
        fn save_machine(&self, cpus: &[X86CpuState], path: &Path) -> Result<String, String> {
            let machine = self.bus.save_state();
            let devices = vm_snapshot::devices::device_slots(&machine);
            let clock = match &self.clock {
                Some(clock) => match clock.save_clock() {
                    Ok(clock) => Some(clock),
                    // Not fatal: a guest that does not use the paravirtual
                    // clock is unaffected, and one that does gets a time jump
                    // rather than no snapshot at all. Worth saying so.
                    Err(error) => {
                        tracing::warn!(%error, "cannot read the guest clock; not saving it");
                        None
                    }
                },
                None => None,
            };
            // The one piece of state whose absence is invisible until the
            // restored guest stops receiving interrupts, so it is a hard error
            // rather than a warning: an IOAPIC that came back masked is a VM
            // whose serial console, disk and network never interrupt again.
            let host_irqchip = match &self.host_irqchip {
                Some(chip) => Some(
                    chip.save_irqchip()
                        .map_err(|e| format!("cannot read the in-kernel interrupt chips: {e}"))?,
                ),
                None => None,
            };
            let report = vm_snapshot::vm::save(
                path,
                self.mem.as_ref(),
                vm_snapshot::vm::SaveRequest {
                    metadata: crate::snapshot::metadata(&self.cfg, devices),
                    host: vm_snapshot::HostKind::current()
                        .ok_or("this build has no hypervisor backend to snapshot")?,
                    cpus,
                    clock,
                    host_irqchip,
                    machine: &machine,
                },
            )
            .map_err(|e| e.to_string())?;
            tracing::info!(
                path = %path.display(),
                bytes = report.bytes,
                allocated = ?report.allocated_bytes,
                saved_ram = report.memory.saved_bytes,
                total_ram = report.memory.total_bytes,
                runs = report.memory.runs,
                percent = format_args!("{:.1}", report.memory.percent()),
                elapsed = ?report.elapsed,
                "snapshot written"
            );
            Ok(report.summary())
        }
    }

    /// Loads a snapshot over a machine that has been assembled but not started.
    ///
    /// The order is the one the machine was built in, reversed where it has to
    /// be:
    ///
    /// 1. **Guest memory first**, because restoring a virtio transport
    ///    *activates* it, and activation validates the driver's rings against
    ///    the memory they point into. Rings that are still zeroes would be
    ///    rejected as unusable.
    /// 2. **The devices**, which is where the activation happens.
    /// 3. **The clock**, so the guest's paravirtual time is back before any
    ///    vCPU can read it.
    ///
    /// The vCPUs are the caller's job: the two hosts hold them differently, and
    /// only one of them may write to an application processor.
    pub(super) fn restore_machine(
        path: &Path,
        cfg: &VmConfig,
        mem: &vmm_core::GuestMem,
        bus: &MachineBus,
        clock: Option<&dyn GuestClock>,
        host_irqchip: Option<&dyn HostIrqChip>,
    ) -> Result<Vec<X86CpuState>, String> {
        let devices = bus_device_slots(bus);
        let shape = crate::snapshot::shape(cfg, devices);
        let restored = vm_snapshot::vm::restore(path, mem, &shape).map_err(|e| e.to_string())?;
        for note in &restored.notes {
            tracing::warn!("{note}");
        }
        bus.load_state(&restored.machine)
            .map_err(|e| format!("cannot restore the machine's devices: {e}"))?;
        // The hypervisor's own chips go back *after* the devices, so a
        // redirection entry that is about to become live points at a device
        // that is already there.
        match (host_irqchip, &restored.host_irqchip) {
            (Some(chip), Some(saved)) => chip
                .load_irqchip(saved)
                .map_err(|e| format!("cannot restore the in-kernel interrupt chips: {e}"))?,
            (Some(_), None) => {
                return Err(
                    "this host runs its interrupt controllers in the kernel, and the \
                            snapshot has no state for them: the restored guest would find \
                            every IOAPIC pin masked"
                        .into(),
                )
            }
            (None, Some(_)) => {
                return Err(
                    "the snapshot carries in-kernel interrupt-controller state and this \
                            host has none to load it into"
                        .into(),
                )
            }
            (None, None) => {}
        }
        if let (Some(clock), Some(saved)) = (clock, restored.clock) {
            if let Err(error) = clock.load_clock(&saved) {
                // The guest gets a time jump; everything else is intact. That
                // is worth a warning and not worth throwing the restore away.
                tracing::warn!(%error, "cannot restore the guest clock");
            }
        }
        tracing::info!(
            path = %path.display(),
            summary = %restored.summary(),
            taken = restored.metadata.created_unix,
            "VM restored from a snapshot"
        );
        Ok(restored.cpus)
    }

    /// The virtio devices on `bus`, in slot order.
    pub(super) fn bus_device_slots(bus: &MachineBus) -> Vec<vm_snapshot::DeviceSlot> {
        let mmio = bus.virtio().slots().iter().enumerate().map(|(i, s)| {
            (
                i as u32,
                s.transport.lock().ok().map(|t| t.device_type().id()),
            )
        });
        let pci = bus
            .pci()
            .map(|bus| bus.slots())
            .unwrap_or_default()
            .iter()
            .enumerate()
            .map(|(i, s)| {
                (
                    i as u32,
                    s.transport.lock().ok().map(|t| t.device_type().id()),
                )
            });
        mmio.chain(pci)
            .filter_map(|(slot, device_type)| {
                device_type.map(|device_type| vm_snapshot::DeviceSlot { device_type, slot })
            })
            .collect()
    }

    /// The command line a direct-Linux guest boots with: the profile's own,
    /// plus whatever the network backend asked for, plus the transport clauses.
    ///
    /// A profile that spells out its own `ip=` wins. The backend's clause is a
    /// convenience for a guest that was told nothing, and the kernel takes the
    /// *last* `ip=` it is given — so appending ours would silently override the
    /// author's, and `ip=dhcp` (the one way to exercise the usernet DHCP server
    /// from a real kernel) could never be asked for.
    pub(super) fn direct_linux_cmdline(
        configured: &str,
        net_cmdline: Option<&str>,
        transport_clauses: &str,
    ) -> String {
        let net = match net_cmdline {
            Some(_) if names_clause(configured, "ip=") => {
                tracing::info!(
                    "the profile's command line configures the network itself; leaving the \
                     backend's ip= clause off"
                );
                None
            }
            other => other,
        };
        let with_net = extend_cmdline(configured, net.unwrap_or(""));
        extend_cmdline(&with_net, transport_clauses)
    }

    /// Whether `cmdline` already carries a clause with this prefix. Split on
    /// whitespace rather than searched as a substring: `panic=1 nfsrootdebug`
    /// must not look like it carries `ip=`.
    fn names_clause(cmdline: &str, prefix: &str) -> bool {
        cmdline.split_whitespace().any(|c| c.starts_with(prefix))
    }
}

use host_api::{
    apply_boot_state, direct_linux_cmdline, load_boot, restored_entry, BootPlan, VmMachine,
};

/// KVM machine assembly (Linux): in-kernel interrupt chips, irqfd/ioeventfd
/// device wiring, register setup on every vCPU.
#[cfg(target_os = "linux")]
mod host {
    use super::*;
    use control_api::{BootMode, VirtioTransport};
    use machine_x86::serial::SerialConsole;
    use machine_x86::virtio::VirtioMmioBus;
    use machine_x86::virtio_pci::VirtioPciBus;
    use vmm_core::{spawn_vcpus_with, Hypervisor, Vm};

    use super::host_api::restore_machine;
    pub(super) use super::host_api::StartRequest;
    use vmm_core::lifecycle::ResettableVcpu as _;
    pub(super) type Threads = vmm_core::VcpuThreads;

    pub(super) struct Started {
        pub bus: MachineBus,
        pub threads: Threads,
        /// The VM's pause gate (ADR-0005), so the host's input pump can decline
        /// to write the guest's event ring while the VM is frozen.
        pub quiesce: Arc<virtio_core::Quiesce>,
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
            lifecycle,
            restore,
        } = request;
        // The VM's pause gate (ADR-0005), created before the devices so every
        // worker thread that is about to be spawned can be handed it.
        let quiesce = virtio_core::Quiesce::new();
        let hv = Hypervisor::open().map_err(|e| e.to_string())?;
        let mut vm = Vm::new(&hv, machine).map_err(|e| e.to_string())?;

        // A restored VM writes **none** of the firmware tables. They are
        // already in the snapshot's guest memory, byte for byte as the running
        // guest last saw them — and the guest may well have reused the pages
        // around them since. Rewriting them would be the host putting its own
        // idea of the machine on top of the guest's (ADR-0006).
        if restore.is_none() {
            // Interrupt topology: without an MP table the guest never programs
            // the IOAPIC and irqfd injections are intermittently lost (a
            // stalled first virtio-blk read with INT_VRING left pending).
            machine_x86::mptable::write(vm.memory(), cfg.vcpus).map_err(|e| e.to_string())?;
            // ACPI tables alongside the MP table: MADT/FADT/DSDT give the guest
            // SMP topology, the PM block and a real S5 poweroff; Linux prefers
            // them and falls back to the MP table when absent.
            machine_x86::acpi::write(vm.memory(), cfg.vcpus).map_err(|e| e.to_string())?;
        }

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
        let plan = match cfg.boot.mode {
            BootMode::DirectLinux => BootPlan::DirectLinux {
                boot: linux_boot::BootConfig {
                    kernel: cfg
                        .boot
                        .require_kernel()
                        .map_err(|e| e.to_string())?
                        .clone(),
                    initramfs: cfg.boot.initramfs.clone(),
                    cmdline,
                },
                mem_size,
            },
            BootMode::Uefi => uefi_plan(&mut vm, cfg, mem_size)?,
        };
        // The pause gate goes in before anything can activate a device: a
        // restore re-activates every virtio slot, and a device that started a
        // worker without the gate would be one a pause could not stop.
        bus.set_quiesce(Arc::clone(&quiesce));

        let clock = vm.clock();
        let host_irqchip = vm.irqchip();
        let (entry, vcpus) = match restore {
            None => {
                let entry = load_boot(vm.memory(), &plan)?;
                let vcpus = vm.take_vcpus();
                for vcpu in &vcpus {
                    // Every vCPU gets the segment state: KVM's INIT discards it
                    // on the APs, so handing it to all of them is free and keeps
                    // them uniform. Only the boot CPU starts at the entry point;
                    // the others wait for INIT/SIPI from the guest.
                    apply_boot_state(vm.memory(), &entry, vcpu, vcpu.index == 0)?;
                }
                (entry, vcpus)
            }
            Some(path) => {
                let cpus = restore_machine(
                    path,
                    cfg,
                    vm.memory(),
                    &bus,
                    Some(clock.as_ref()),
                    Some(host_irqchip.as_ref()),
                )?;
                // Every vCPU, boot CPU or not: a restored application processor
                // has to come back exactly where it was, which for one the
                // guest had brought up means running, and for one still waiting
                // for its INIT/SIPI means `mp_state` back at uninitialised.
                // `load_cpu_state` carries that distinction; nothing here has to
                // know which is which.
                let mut vcpus = vm.take_vcpus();
                for vcpu in vcpus.iter_mut() {
                    let state = cpus
                        .get(vcpu.index as usize)
                        .ok_or_else(|| format!("the snapshot has no vCPU {}", vcpu.index))?;
                    vcpu.load_cpu_state(state)
                        .map_err(|e| format!("cannot restore vCPU {}: {e}", vcpu.index))?;
                }
                (restored_entry(&cpus), vcpus)
            }
        };

        // The machine goes behind the lifecycle seam *before* the vCPUs start,
        // so a guest that faults in its first microseconds is rebooted rather
        // than reported as a shutdown (ADR-0005).
        lifecycle.attach_machine(Arc::new(VmMachine {
            bus: bus.clone(),
            mem: Arc::clone(&mem),
            quiesce: Arc::clone(&quiesce),
            vcpus: cfg.vcpus,
            plan,
            entry: Mutex::new(entry),
            cfg: cfg.clone(),
            clock: Some(clock),
            host_irqchip: Some(host_irqchip),
        }));

        let threads = spawn_vcpus_with(vcpus, |_| Box::new(bus.clone()), Some(lifecycle))
            .map_err(|e| e.to_string())?;
        Ok(Started {
            bus,
            threads,
            quiesce,
            entry: entry.entry,
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
    fn uefi_plan(vm: &mut Vm, cfg: &VmConfig, mem_size: u64) -> Result<BootPlan, String> {
        let path = cfg.boot.require_firmware().map_err(|e| e.to_string())?;
        let image = uefi_boot::FirmwareImage::read(path).map_err(|e| e.to_string())?;
        tracing::info!(
            firmware = %path.display(),
            bytes = image.len(),
            kind = ?image.kind(),
            "loading UEFI firmware"
        );

        match image.kind() {
            uefi_boot::FirmwareKind::PvhElf { .. } => Ok(BootPlan::Pvh { image, mem_size }),
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
                // crates/uefi-boot/tests/reset_vector.rs). The mapping is a host
                // slot the guest cannot change, so a reset reloads nothing.
                Ok(BootPlan::ResetVector)
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
    use machine_x86::irqchip::UserspaceIrqChip;
    use machine_x86::serial::SerialConsole;
    use machine_x86::virtio::VirtioMmioBus;
    use machine_x86::virtio_pci::{PciInterruptMode, VirtioPciBus};
    use vmm_core::whp::{spawn_vcpus_with, WhpHypervisor, WhpOptions, WhpPartition};

    use super::host_api::restore_machine;
    pub(super) use super::host_api::StartRequest;
    use vmm_core::lifecycle::ResettableVcpu as _;
    pub(super) type Threads = vmm_core::whp::WhpVcpuThreads;

    pub(super) struct Started {
        pub bus: MachineBus,
        pub threads: Threads,
        /// The VM's pause gate (ADR-0005); see the KVM module's `Started`.
        pub quiesce: Arc<virtio_core::Quiesce>,
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
            lifecycle,
            restore,
        } = request;
        let quiesce = virtio_core::Quiesce::new();
        let hv = WhpHypervisor::open().map_err(|e| e.to_string())?;
        // Everything a real guest needs: local APIC emulation (interrupt
        // delivery, the `hlt` idle wait) and this machine's CPUID policy. One
        // partition per process — WHP maps guest memory for a single partition,
        // which is why the manager runs one `entangled` process per VM.
        let mut vm = WhpPartition::with_options(&hv, machine, WhpOptions::for_guest())
            .map_err(|e| e.to_string())?;

        // The same interrupt topology the KVM machine publishes: the guest must
        // not be able to tell the hosts apart from the tables. A restored VM
        // skips them for the same reason the KVM path does — the snapshot's
        // guest memory already holds them (ADR-0006).
        if restore.is_none() {
            machine_x86::mptable::write(vm.memory(), cfg.vcpus).map_err(|e| e.to_string())?;
            machine_x86::acpi::write(vm.memory(), cfg.vcpus).map_err(|e| e.to_string())?;
        }

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
        let plan = match cfg.boot.mode {
            BootMode::DirectLinux => BootPlan::DirectLinux {
                boot: linux_boot::BootConfig {
                    kernel: cfg
                        .boot
                        .require_kernel()
                        .map_err(|e| e.to_string())?
                        .clone(),
                    initramfs: cfg.boot.initramfs.clone(),
                    cmdline,
                },
                mem_size,
            },
            BootMode::Uefi => uefi_plan(&mut vm, cfg, mem_size)?,
        };
        // Before anything can activate a device; see the KVM path.
        bus.set_quiesce(Arc::clone(&quiesce));

        let (entry, vcpus) = match restore {
            None => {
                let entry = load_boot(vm.memory(), &plan)?;
                let vcpus = vm.take_vcpus();
                // **Boot CPU only.** WHP has no INIT of its own to discard host
                // register writes: an AP must be left in the reset state WHP
                // created it in, or the guest's INIT/SIPI never makes it
                // runnable (see `WhpPartition`'s SMP notes). The same rule
                // governs `reset_vcpu`.
                if let Some(vcpu) = vcpus.first() {
                    apply_boot_state(vm.memory(), &entry, vcpu, true)?;
                }
                (entry, vcpus)
            }
            Some(path) => {
                // No clock: WHP has no `KVM_SET_CLOCK` equivalent, and a
                // partition with local APIC emulation has no paravirtual clock
                // for the guest to have been using.
                let cpus = restore_machine(path, cfg, vm.memory(), &bus, None, None)?;
                // **Every** VP here, unlike the boot path. The rule that keeps
                // an AP untouched exists so the guest's own INIT/SIPI can bring
                // it up from WHP's reset state — but a restored AP is not being
                // brought up, it is being put back exactly where it already
                // was, INIT/SIPI included or long past.
                let mut vcpus = vm.take_vcpus();
                for vcpu in vcpus.iter_mut() {
                    let state = cpus
                        .get(vcpu.index as usize)
                        .ok_or_else(|| format!("the snapshot has no vCPU {}", vcpu.index))?;
                    vcpu.load_cpu_state(state)
                        .map_err(|e| format!("cannot restore vCPU {}: {e}", vcpu.index))?;
                }
                (restored_entry(&cpus), vcpus)
            }
        };

        lifecycle.attach_machine(Arc::new(VmMachine {
            bus: bus.clone(),
            mem: Arc::clone(&mem),
            quiesce: Arc::clone(&quiesce),
            vcpus: cfg.vcpus,
            plan,
            entry: Mutex::new(entry),
            cfg: cfg.clone(),
            clock: None,
            // WHP's interrupt controllers are `machine_x86::irqchip`, in this
            // process, and are saved with every other device.
            host_irqchip: None,
        }));

        let threads = spawn_vcpus_with(vcpus, |_| Box::new(bus.clone()), Some(lifecycle))
            .map_err(|e| e.to_string())?;
        Ok(Started {
            bus,
            threads,
            quiesce,
            entry: entry.entry,
            _vm: vm,
        })
    }

    /// Boots a UEFI firmware on WHP (EPIC 18 on EPIC 17): the same two shapes
    /// as the KVM path, with the one WHP rule applied — PVH register state goes
    /// to the boot CPU only, and a reset-vector image needs *nothing* written
    /// because `WHvCreateVirtualProcessor` already leaves every VP in the
    /// architectural reset state.
    fn uefi_plan(vm: &mut WhpPartition, cfg: &VmConfig, mem_size: u64) -> Result<BootPlan, String> {
        let path = cfg.boot.require_firmware().map_err(|e| e.to_string())?;
        let image = uefi_boot::FirmwareImage::read(path).map_err(|e| e.to_string())?;
        tracing::info!(
            firmware = %path.display(),
            bytes = image.len(),
            kind = ?image.kind(),
            "loading UEFI firmware"
        );

        match image.kind() {
            // The firmware's own INIT/SIPI sweep (`MpInitLib`) brings the APs up
            // from the reset state WHP models for them, so only the boot CPU is
            // ever touched — see the caller.
            uefi_boot::FirmwareKind::PvhElf { .. } => Ok(BootPlan::Pvh { image, mem_size }),
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
                Ok(BootPlan::ResetVector)
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

    /// A profile that configures the network itself keeps its own clause: the
    /// kernel honours the last `ip=` it is handed, so appending the backend's
    /// would quietly overrule the author — and `ip=dhcp`, the only way to make a
    /// real kernel talk to the usernet DHCP server, could never be asked for.
    #[test]
    fn the_profiles_own_ip_clause_wins() {
        let backend = Some("ip=192.168.74.15::192.168.74.1:255.255.255.0::eth0:off:192.168.74.1");
        assert_eq!(
            direct_linux_cmdline(
                "console=ttyS0 ip=dhcp",
                backend,
                "virtio_mmio.device=4K@0xd0:5"
            ),
            "console=ttyS0 ip=dhcp virtio_mmio.device=4K@0xd0:5"
        );
        // A clause that merely *contains* "ip=" is not one: `nfsrootdebug` and
        // `noip=` style words must not suppress the backend's configuration.
        let with_lookalike = direct_linux_cmdline("console=ttyS0 nfsrootdebug", backend, "");
        assert!(
            with_lookalike.contains("ip=192.168.74.15"),
            "{with_lookalike}"
        );
    }
}
