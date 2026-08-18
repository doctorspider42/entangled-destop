//! `vmhost run` — boots a direct-linux VM to the serial console with its
//! virtio-mmio devices attached (backlog MVP-1202..1206; display and the
//! remaining device epics plug in here as they land).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use control_api::VmConfig;
use machine_x86::boot as x86_boot;
use machine_x86::bus::MachineBus;
use machine_x86::serial::SerialConsole;
use machine_x86::virtio::VirtioMmioBus;
use virtio_core::VirtioDevice;
use vmm_core::{spawn_vcpus, Hypervisor, MachineConfig, RunOutcome, Vm, VmState};

/// Set by the SIGINT/SIGTERM handler; the run loop polls it (MVP-1204).
static SHUTDOWN_REQUESTED: AtomicBool = AtomicBool::new(false);

extern "C" fn on_termination_signal(_signum: libc::c_int) {
    // Async-signal-safe: a relaxed atomic store and nothing else.
    SHUTDOWN_REQUESTED.store(true, Ordering::Relaxed);
}

/// Installs SIGINT/SIGTERM handlers that request a clean VM stop instead of
/// killing the process mid-I/O.
fn install_signal_handlers() -> Result<(), String> {
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
                return Ok(Presentation::Windowed(Box::new(
                    host.with_title(format!("VMHost — {}", cfg.name)),
                )))
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

pub fn run(cfg: VmConfig, headless: bool) -> Result<(), String> {
    let span = tracing::info_span!("vm", id = %cfg.name);
    let _guard = span.enter();
    install_signal_handlers()?;

    let hv = Hypervisor::open().map_err(|e| e.to_string())?;
    let machine = MachineConfig {
        memory_mib: cfg.memory_mib,
        vcpu_count: cfg.vcpus,
    };
    let mut vm = Vm::new(&hv, &machine).map_err(|e| e.to_string())?;

    let serial =
        SerialConsole::new(vm.fd(), Box::new(std::io::stdout())).map_err(|e| e.to_string())?;

    // One virtio-blk device per [[disk]] entry, in configuration order: the
    // guest kernel probes virtio-mmio devices in command-line order, so the
    // first disk becomes /dev/vda, the second /dev/vdb (MVP-407).
    let mut devices: Vec<Box<dyn VirtioDevice>> = Vec::with_capacity(cfg.disks.len());
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

    // virtio-net from the [network] section (MVP-505); the TAP interface must
    // already exist (scripts/setup-tap.sh) — vmhost itself never needs
    // CAP_NET_ADMIN, only the one-time setup does.
    if let Some(net) = &cfg.network {
        let backend = virtio_net::TapBackend::open(&net.interface)
            .map_err(|e| format!("cannot open TAP '{}': {e}", net.interface))?;
        let mac = match &net.mac {
            Some(text) => parse_mac(text)?,
            None => virtio_net::MacAddr::derive(&cfg.name),
        };
        tracing::info!(interface = %net.interface, mac = %mac, "attaching virtio-net device");
        devices.push(Box::new(virtio_net::NetDevice::new(backend, mac)));
    }

    // Presentation + virtio-gpu (EPIC 7/8): the device pushes scanout pixels
    // into the display handle; with a window they appear on screen, headless
    // they are still screenshot-able.
    let presentation = open_presentation(&cfg, headless)?;
    let display_handle = match &presentation {
        Presentation::Windowed(host) => host.handle(),
        Presentation::Headless(handle) => handle.clone(),
    };
    devices.push(Box::new(virtio_gpu::GpuDevice::new(display_handle.clone())));

    // virtio-input keyboard + tablet (EPIC 9); handles stay on the host side
    // and are fed from the window's input capture.
    let keyboard = virtio_input::InputDevice::keyboard();
    let tablet = virtio_input::InputDevice::absolute_pointer();
    let keyboard_sink = keyboard.handle();
    let tablet_sink = tablet.handle();
    devices.push(Box::new(keyboard));
    devices.push(Box::new(tablet));

    // Guest memory is shared with the devices; cloning a `GuestMemoryMmap`
    // shares the underlying regions rather than copying them.
    let mem = Arc::new(vm.memory().clone());
    let virtio =
        VirtioMmioBus::attach(vm.fd(), Arc::clone(&mem), devices).map_err(|e| e.to_string())?;
    let cmdline = extend_cmdline(&cfg.boot.cmdline, &virtio.cmdline_clauses());
    let bus = MachineBus::with_virtio(serial, virtio);

    let boot = linux_boot::BootConfig {
        kernel: cfg.boot.kernel.clone(),
        initramfs: cfg.boot.initramfs.clone(),
        cmdline,
    };
    let loaded = linux_boot::load(vm.memory(), &boot, machine.memory_mib << 20)
        .map_err(|e| e.to_string())?;

    let vcpus = vm.take_vcpus();
    for vcpu in &vcpus {
        x86_boot::setup_long_mode_sregs(vm.memory(), vcpu.fd()).map_err(|e| e.to_string())?;
        if vcpu.index == 0 {
            // Only the boot CPU starts at the kernel entry; the others wait
            // for INIT/SIPI from the guest.
            x86_boot::setup_boot_regs(vcpu.fd(), loaded.entry, loaded.boot_params_addr)
                .map_err(|e| e.to_string())?;
        }
    }

    // Lifecycle (MVP-1203): Created -> Running -> Stopping -> Stopped, any
    // vCPU error -> Crashed. Transitions are validated by VmState itself.
    let mut state = VmState::Created;
    tracing::info!(entry = format_args!("{:#x}", loaded.entry), state = ?state, "VM created");

    let threads = spawn_vcpus(vcpus, |_| Box::new(bus.clone())).map_err(|e| e.to_string())?;
    state = state
        .transition(VmState::Running)
        .map_err(|e| e.to_string())?;
    tracing::info!(state = ?state, "VM running");

    let outcomes = match presentation {
        Presentation::Headless(_) => threads.join_or_stop(
            || SHUTDOWN_REQUESTED.load(Ordering::Relaxed),
            Duration::from_millis(50),
        ),
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
                let outcomes = threads.join_or_stop(
                    || SHUTDOWN_REQUESTED.load(Ordering::Relaxed),
                    Duration::from_millis(50),
                );
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

    let mut failure = None;
    for (i, outcome) in outcomes.iter().enumerate() {
        match outcome {
            Ok(RunOutcome::Shutdown) => tracing::info!(vcpu = i, "guest shut down"),
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

    match failure {
        Some(message) => Err(message),
        None => Ok(()),
    }
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
    use super::extend_cmdline;

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
}
