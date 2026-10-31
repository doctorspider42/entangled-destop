//! Native ARM64 runtime. The window owns the main thread; HVF vCPU state
//! remains on its worker thread, with the existing virtio devices and Venus.
pub use crate::run_options::{RunOptions, ScreenshotRequest};
use control_api::control::{CMD_PASTE, CMD_SHUTDOWN, CMD_STATUS, CMD_THUMBNAIL, PREFIX};
use control_api::{BootMode, GuestArchitecture, NetworkBackend, VirtioTransport, VmConfig};
use display::{ControlEvent, DisplayHandle, DisplayHost};
use machine_arm64::virtio::{AttachedDevice, MmioBus};
use machine_arm64::{boot, device_tree, TreeConfig, RAM_BASE};
use std::io::{BufRead, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant};
use virtio_core::VirtioDevice;
use vm_memory::{Bytes, GuestAddress};
use vmm_core::hvf::{HvfGic, HvfOptions, HvfVm};
#[path = "macos_cpus.rs"]
mod cpus;

static STOP: AtomicBool = AtomicBool::new(false);
extern "C" fn stop_signal(_: i32) {
    STOP.store(true, Ordering::Relaxed);
}
struct Signals([(i32, libc::sighandler_t); 2]);
impl Signals {
    fn install() -> Result<Self, String> {
        STOP.store(false, Ordering::Relaxed);
        let mut installed = Self([
            (libc::SIGINT, libc::SIG_DFL),
            (libc::SIGTERM, libc::SIG_DFL),
        ]);
        for (signal, previous) in &mut installed.0 {
            // SAFETY: the handler only stores to a lock-free AtomicBool and
            // remains valid for this process; the old handlers are restored.
            *previous =
                unsafe { libc::signal(*signal, stop_signal as *const () as libc::sighandler_t) };
            if *previous == libc::SIG_ERR {
                return Err("cannot install VM stop handler".into());
            }
        }
        Ok(installed)
    }
}
impl Drop for Signals {
    fn drop(&mut self) {
        for &(signal, previous) in &self.0 {
            if previous != libc::SIG_ERR {
                // SAFETY: restores the handler returned by signal for this signal.
                unsafe { libc::signal(signal, previous) };
            }
        }
    }
}

fn validate(cfg: &VmConfig, options: &RunOptions) -> Result<(), String> {
    if cfg.architecture != GuestArchitecture::Aarch64 {
        return Err("Apple Silicon requires architecture = \"aarch64\"; x86 guests cannot be hardware-virtualized here".into());
    }
    if cfg.boot.mode != BootMode::DirectLinux || cfg.transport != VirtioTransport::Mmio {
        return Err(
            "the ARM64 backend currently requires direct-linux boot and virtio-mmio".into(),
        );
    }
    if cfg.vcpus == 0 || cfg.vcpus > machine_arm64::MAX_CPUS {
        return Err("ARM64 vCPU count must be 1..64".into());
    }
    if cfg.display.virgl {
        return Err("macOS uses display.venus = true for 3D".into());
    }
    if cfg.sound.enabled || cfg.gamepad.enabled || cfg.reclaim_memory || cfg.cdrom.is_some() {
        return Err(
            "ARM64 audio, gamepad, memory reclaim and ISO boot are not connected yet".into(),
        );
    }
    if options.restore.is_some() {
        return Err("ARM64 saved CPU state is not supported by the x86 snapshot format".into());
    }
    if cfg
        .network
        .as_ref()
        .is_some_and(|n| n.backend != NetworkBackend::Usernet)
    {
        return Err("macOS networking requires usernet".into());
    }
    Ok(())
}

type Ready = (
    Arc<cpus::CpuGroup>,
    virtio_input::InputHandle,
    virtio_input::InputHandle,
    Arc<virtio_core::Quiesce>,
);

/// Running guest CPUs must keep progressing when their window is occluded.
/// The activity prevents App Nap, while still allowing the host to sleep.
struct VmActivity {
    process: objc2::rc::Retained<objc2_foundation::NSProcessInfo>,
    token:
        objc2::rc::Retained<objc2::runtime::ProtocolObject<dyn objc2_foundation::NSObjectProtocol>>,
}

impl VmActivity {
    fn begin() -> Self {
        use objc2_foundation::{NSActivityOptions, NSProcessInfo, NSString};
        let process = NSProcessInfo::processInfo();
        let token = process.beginActivityWithOptions_reason(
            NSActivityOptions::UserInitiatedAllowingIdleSystemSleep,
            &NSString::from_str("Running an Entangled virtual machine"),
        );
        Self { process, token }
    }
}

impl Drop for VmActivity {
    fn drop(&mut self) {
        // SAFETY: token is the retained activity returned by this process's
        // beginActivity call, and this guard ends it exactly once.
        unsafe { self.process.endActivity(&self.token) };
    }
}

pub fn run(cfg: VmConfig, options: RunOptions) -> Result<(), String> {
    run_inner(cfg, options, None)
}

pub fn run_with_success_marker(
    cfg: VmConfig,
    options: RunOptions,
    expected: String,
) -> Result<(), String> {
    if expected.is_empty() || expected.len() > MARKER_LINE_BYTES || expected.contains(['\r', '\n'])
    {
        return Err("invalid provisioning success marker".into());
    }
    let marker = SerialMarker {
        expected,
        seen: Arc::new(AtomicBool::new(false)),
        powered_off: Arc::new(AtomicBool::new(false)),
    };
    run_inner(cfg, options, Some(marker.clone()))?;
    if !marker.seen.load(Ordering::Acquire) || !marker.powered_off.load(Ordering::Acquire) {
        return Err("Ubuntu provisioning stopped without its success marker and orderly guest power-off; inspect the installation log".into());
    }
    Ok(())
}

const MARKER_LINE_BYTES: usize = 256;
#[derive(Clone)]
struct SerialMarker {
    expected: String,
    seen: Arc<AtomicBool>,
    powered_off: Arc<AtomicBool>,
}
struct MarkerWriter<W> {
    output: W,
    marker: SerialMarker,
    line: Vec<u8>,
    discarded: bool,
}
impl<W: Write> Write for MarkerWriter<W> {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        let count = self.output.write(bytes)?;
        for byte in &bytes[..count] {
            if *byte == b'\n' {
                if !self.discarded
                    && self.line.strip_suffix(b"\r").unwrap_or(&self.line)
                        == self.marker.expected.as_bytes()
                {
                    self.marker.seen.store(true, Ordering::Release);
                }
                self.line.clear();
                self.discarded = false;
            } else if self.line.len() == MARKER_LINE_BYTES {
                self.discarded = true;
            } else if !self.discarded {
                self.line.push(*byte);
            }
        }
        Ok(count)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.output.flush()
    }
}

fn run_inner(
    cfg: VmConfig,
    options: RunOptions,
    marker: Option<SerialMarker>,
) -> Result<(), String> {
    validate(&cfg, &options)?;
    let _activity = VmActivity::begin();
    let _signals = Signals::install()?;
    tracing::debug!(snapshot = ?options.snapshot, "ARM64 snapshots remain unavailable");
    let thumbnail_default = options.snapshot.as_ref().and_then(|path| {
        Some(control_api::control::thumbnail_path(
            path.parent().unwrap_or(std::path::Path::new(".")),
            path.file_stem()?.to_str()?,
        ))
    });
    let typist = virtio_input::Typist::with_layout(virtio_input::Layout::from_name(
        cfg.display.typing_layout.as_deref(),
    ));
    let host = if options.headless {
        None
    } else {
        Some(
            DisplayHost::new(display::DisplayConfig {
                width: cfg.display.width,
                height: cfg.display.height,
                scale: cfg.display.scale,
            })
            .map_err(|e| e.to_string())?
            .with_title(format!("Entangled — {} (ARM64)", cfg.name))
            .with_typist(typist.clone()),
        )
    };
    let display = match &host {
        Some(host) => host.handle(),
        None => DisplayHandle::detached(cfg.display.width, cfg.display.height)
            .map_err(|e| e.to_string())?,
    };
    let input = host.as_ref().map(DisplayHost::input_queue);
    let control = host.as_ref().map(DisplayHost::control_queue);
    let done = Arc::new(AtomicBool::new(false));
    let (ready_tx, ready_rx) = mpsc::channel();
    let vm_display = display.clone();
    let finished = done.clone();
    let worker = std::thread::spawn(move || {
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            machine(cfg, vm_display.clone(), ready_tx, marker)
        }))
        .map_err(|_| "ARM64 VM worker panicked".to_string())
        .and_then(|result| result.map_err(|e| e.to_string()));
        finished.store(true, Ordering::Release);
        vm_display.shutdown();
        result
    });
    let (command_tx, command_rx) = mpsc::channel();
    if options.control_stdin {
        std::thread::spawn(move || {
            for line in std::io::stdin().lock().lines() {
                let Ok(line) = line else {
                    break;
                };
                if command_tx.send(line).is_err() {
                    break;
                }
            }
        });
    }
    let screenshots = options.screenshot.map(|shot| {
        let screen = display.clone();
        let finished = done.clone();
        std::thread::spawn(move || {
            let start = Instant::now();
            let mut at = shot.after;
            let mut count = 0u32;
            while !finished.load(Ordering::Acquire) {
                if start.elapsed() >= at {
                    let path = if shot.burst.is_some() {
                        let Some(next) = count.checked_add(1) else {
                            break;
                        };
                        count = next;
                        let stem = shot
                            .path
                            .file_stem()
                            .map_or_else(|| "shot".into(), |s| s.to_string_lossy());
                        shot.path.with_file_name(format!("{stem}-{count:04}.png"))
                    } else {
                        shot.path.clone()
                    };
                    if let Err(e) = screen.screenshot(path) {
                        tracing::warn!(%e, "screenshot failed");
                    }
                    let Some(interval) = shot.burst else {
                        break;
                    };
                    at = start.elapsed() + interval.max(Duration::from_millis(1));
                }
                std::thread::sleep(Duration::from_millis(20));
            }
        })
    });
    let monitor_done = done.clone();
    let monitor = std::thread::spawn(move || {
        let mut ready: Option<Ready> = None;
        let mut stop_at = None;
        let mut power_requested = false;
        while !monitor_done.load(Ordering::Acquire) {
            if ready.is_none() {
                ready = ready_rx.try_recv().ok();
            }
            for command in command_rx.try_iter() {
                if let Some(text) = command.strip_prefix(&format!("{CMD_PASTE} ")) {
                    let outcome = typist.type_text(&text.replace("\\n", "\n"));
                    println!(
                        "{PREFIX} ok {CMD_PASTE} typed={} skipped={} truncated={}",
                        outcome.typed, outcome.skipped, outcome.truncated
                    );
                } else if let Some(text) = command.strip_prefix("type ") {
                    typist.type_text(text);
                } else if let Some(chord) = command.strip_prefix("chord ") {
                    match virtio_input::typing::parse_chord(chord) {
                        Some(keys) => {
                            typist.chord(&keys);
                        }
                        None => tracing::warn!(%chord, "unsupported keyboard chord"),
                    }
                } else if command == "stop" {
                    STOP.store(true, Ordering::Relaxed);
                } else if command == CMD_SHUTDOWN {
                    power_requested = true;
                } else if command == CMD_STATUS {
                    println!("{PREFIX} state=Running");
                } else if let Some(argument) =
                    command.strip_prefix(CMD_THUMBNAIL).filter(|argument| {
                        argument.is_empty() || argument.starts_with(char::is_whitespace)
                    })
                {
                    let argument = argument.trim();
                    let path = if argument.is_empty() {
                        thumbnail_default.as_deref()
                    } else {
                        Some(std::path::Path::new(argument))
                    };
                    if let Some(path) = path {
                        if let Some(parent) = path.parent() {
                            if let Err(error) = std::fs::create_dir_all(parent) {
                                println!("{PREFIX} error thumbnail {error}");
                                continue;
                            }
                        }
                        if let Err(error) =
                            display.write_thumbnail(path, display::thumbnail::MAX_WIDTH)
                        {
                            println!("{PREFIX} error thumbnail {error}");
                        }
                    } else {
                        println!("{PREFIX} error thumbnail needs a path");
                    }
                } else {
                    println!("{PREFIX} error {command} unsupported ARM64 control");
                }
            }
            if let Some(control) = &control {
                for event in control.drain() {
                    match event {
                        ControlEvent::QuitRequested | ControlEvent::WindowCloseRequested => {
                            STOP.store(true, Ordering::Relaxed)
                        }
                        ControlEvent::PowerButtonRequested => power_requested = true,
                        ControlEvent::ResetRequested | ControlEvent::SaveRequested => {
                            tracing::warn!(
                                ?event,
                                "ARM64 lifecycle operation is not implemented yet"
                            )
                        }
                        _ => {}
                    }
                }
            }
            if let Some((group, keyboard, pointer, quiesce)) = &ready {
                if group.stopping() {
                    group.kick_all();
                    std::thread::sleep(Duration::from_millis(4));
                    continue;
                }
                let Some(_pass) = quiesce.try_enter() else {
                    if let Some(input) = &input {
                        input.drain_batches();
                    }
                    std::thread::sleep(Duration::from_millis(4));
                    continue;
                };
                if let Some(input) = &input {
                    for batch in virtio_input::coalesce_motion(input.drain_batches()) {
                        let split = virtio_input::split_batch(&batch);
                        if let Err(e) = keyboard.push(&split.keyboard) {
                            tracing::warn!(%e, "keyboard delivery failed");
                        }
                        if let Err(e) = pointer.push(&split.pointer) {
                            tracing::warn!(%e, "pointer delivery failed");
                        }
                    }
                }
                let keys = typist.take(2);
                if !keys.is_empty() {
                    let _ = keyboard.push(&keys);
                }
                if power_requested {
                    match press_power_button(keyboard) {
                        Ok(()) => println!("{PREFIX} ok {CMD_SHUTDOWN}"),
                        Err(e) => println!("{PREFIX} error {CMD_SHUTDOWN} {e}"),
                    }
                    power_requested = false;
                }
                if STOP.load(Ordering::Relaxed) {
                    if stop_at.is_none() {
                        let _ = press_power_button(keyboard);
                        stop_at = Some(Instant::now());
                    }
                    if stop_at.is_some_and(|at| at.elapsed() >= Duration::from_secs(20)) {
                        tracing::warn!(
                            "guest did not power off within the grace period; VM stopped"
                        );
                        group.stop();
                    }
                }
            }
            std::thread::sleep(Duration::from_millis(4));
        }
    });
    let window_result = host
        .map(|host| host.run().map_err(|e| e.to_string()))
        .transpose();
    if !options.headless {
        STOP.store(true, Ordering::Relaxed);
    }
    let result = worker
        .join()
        .map_err(|_| "ARM64 vCPU worker panicked".to_string())?;
    monitor
        .join()
        .map_err(|_| "ARM64 input monitor panicked".to_string())?;
    if let Some(screenshots) = screenshots {
        screenshots
            .join()
            .map_err(|_| "ARM64 screenshot worker panicked".to_string())?;
    }
    window_result?;
    result
}

fn press_power_button(
    keyboard: &virtio_input::InputHandle,
) -> Result<(), virtio_core::DeviceError> {
    keyboard
        .push(&[
            virtio_input::InputEvent {
                event_type: virtio_input::ev::KEY,
                code: 116,
                value: 1,
            },
            virtio_input::InputEvent::SYN_REPORT,
            virtio_input::InputEvent {
                event_type: virtio_input::ev::KEY,
                code: 116,
                value: 0,
            },
            virtio_input::InputEvent::SYN_REPORT,
        ])
        .map(|_| ())
}

struct GicLine {
    gic: HvfGic,
    irq: u32,
    pulse: Mutex<()>,
}
impl virtio_core::IrqLine for GicLine {
    fn trigger(&self) -> Result<(), virtio_core::InterruptError> {
        let _guard = self.pulse.lock().unwrap_or_else(|e| e.into_inner());
        self.gic
            .set_spi(self.irq, true)
            .and_then(|()| self.gic.set_spi(self.irq, false))
            .map_err(|e| virtio_core::InterruptError::Signal(e.to_string()))
    }
}

fn machine(
    cfg: VmConfig,
    display: DisplayHandle,
    ready: mpsc::Sender<Ready>,
    marker: Option<SerialMarker>,
) -> Result<(), Box<dyn std::error::Error>> {
    let maximum = HvfVm::max_vcpu_count()?.min(machine_arm64::MAX_CPUS);
    if cfg.vcpus > maximum {
        return Err(format!("this hypervisor supports at most {maximum} ARM64 vCPUs").into());
    }
    let ram = cfg
        .memory_mib
        .checked_mul(1 << 20)
        .ok_or("RAM size overflow")?;
    let mem = vmm_core::GuestMem::from_ranges(&[(GuestAddress(RAM_BASE), usize::try_from(ram)?)])?;
    let entry = boot::load_kernel(&mem, &boot::read_image(cfg.boot.require_kernel()?)?)?;
    let initrd = cfg
        .boot
        .initramfs
        .as_ref()
        .map(|path| boot::read_image(path).and_then(|bytes| boot::write_initrd(&mem, &bytes, ram)))
        .transpose()?;
    let vm = HvfVm::with_options(
        mem,
        HvfOptions {
            small_page_mappings: cfg.display.venus,
        },
    )?;
    let gic = HvfGic::new(
        &vm,
        machine_arm64::GIC_DISTRIBUTOR,
        machine_arm64::GIC_REDISTRIBUTOR,
    )?;
    let mut devices = Vec::new();
    let mut attach = |device: Box<dyn VirtioDevice>, shm| {
        let slot = devices.len();
        devices.push(AttachedDevice {
            device,
            shm,
            line: Arc::new(GicLine {
                gic: gic.clone(),
                irq: machine_arm64::VIRTIO_IRQ + slot as u32,
                pulse: Mutex::new(()),
            }),
        });
    };
    for disk in &cfg.disks {
        attach(
            Box::new(virtio_block::BlockDevice::open(&disk.path, disk.writable)?),
            None,
        );
    }
    if let Some(network) = &cfg.network {
        let backend = virtio_net::UserNetBackend::new(virtio_net::UserNetConfig::for_host())?;
        for forward in &network.forward {
            let f = control_api::PortForwardRule::parse(forward)?;
            let forward = virtio_net::PortForward {
                host: std::net::SocketAddr::new(f.bind, f.host_port),
                guest_port: f.guest_port,
            };
            if f.udp {
                backend.forward_udp(forward)?;
            } else {
                backend.forward(forward)?;
            }
        }
        let mac = network
            .mac
            .as_ref()
            .map(|s| control_api::parse_mac(s).map(virtio_net::MacAddr))
            .transpose()?
            .unwrap_or_else(|| virtio_net::MacAddr::derive(&cfg.name));
        attach(Box::new(virtio_net::NetDevice::new(backend, mac)), None);
    }
    let mut gpu = if cfg.display.venus {
        let host = virtio_gpu::host_vulkan::AshVulkan::load()?;
        let admitted = host.usable_devices()?;
        if admitted.is_empty() {
            return Err("no hardware Vulkan device admitted for Venus".into());
        }
        for d in admitted {
            tracing::info!(device=%d.name(), "Venus host GPU");
        }
        let factory = virtio_gpu::ExecutorFactory::new(Arc::new(host))
            .with_gpu_memory(cfg.display.gpu_memory_mib.map(|n| u64::from(n) << 20));
        let renderer = virtio_gpu::VenusRenderer::new(factory).with_host_visible_bytes(
            u64::from(
                cfg.display
                    .host_visible_mib
                    .unwrap_or(control_api::DEFAULT_HOST_VISIBLE_MIB),
            ) << 20,
        );
        virtio_gpu::GpuDevice::with_renderer(display, Box::new(renderer))
    } else {
        virtio_gpu::GpuDevice::new(display)
    };
    gpu.set_refresh_hz(cfg.display.refresh_hz);
    gpu.set_frame_stats(cfg.display.frame_stats.clone());
    let shm = if let Some(region) = gpu.shm_region() {
        let base = (RAM_BASE + ram)
            .checked_add(region.len - 1)
            .ok_or("GPU window placement overflows")?
            & !(region.len - 1);
        let window = vm.create_shm_window(region.len, region.host_mapped)?;
        window.place(base)?;
        gpu.set_shm_backing(
            region.id,
            Arc::new(machine_arm64::shm::WindowBacking(window)),
        );
        Some((region.id, base))
    } else {
        None
    };
    attach(Box::new(gpu), shm);
    let keyboard = virtio_input::InputDevice::keyboard();
    let keyboard_handle = keyboard.handle();
    let pointer = virtio_input::InputDevice::absolute_pointer();
    let pointer_handle = pointer.handle();
    attach(Box::new(keyboard), None);
    attach(Box::new(pointer), None);
    for share in &cfg.shares {
        attach(
            Box::new(virtio_9p::NinePDevice::new(
                &share.tag,
                virtio_9p::Server::new(&share.path, share.read_only)?,
            )?),
            None,
        );
    }
    let bus = Arc::new(MmioBus::new(Arc::new(vm.memory().clone()), devices)?);
    let tree = device_tree(&TreeConfig {
        memory_bytes: ram,
        cpus: cfg.vcpus,
        gic: gic.layout(),
        initrd,
        virtio_slots: bus.len() as u32,
        cmdline: &cfg.boot.cmdline,
    })?;
    let dtb = RAM_BASE + ram - boot::DTB_RESERVE;
    vm.memory().write_slice(&tree, GuestAddress(dtb))?;
    let guest_poweroff = marker.as_ref().map(|marker| marker.powered_off.clone());
    let output: Box<dyn Write + Send> = match marker {
        Some(marker) => Box::new(MarkerWriter {
            output: std::io::stdout(),
            marker,
            line: Vec::with_capacity(MARKER_LINE_BYTES),
            discarded: false,
        }),
        None => Box::new(std::io::stdout()),
    };
    let machine = cpus::Machine::new(cfg.vcpus, ram, bus.clone(), output, guest_poweroff)?;
    tracing::info!(name=%cfg.name, vcpus=cfg.vcpus, "Ubuntu ARM64 CPUs starting on Hypervisor.framework");
    let result = cpus::run(
        &vm,
        cfg.vcpus,
        machine_arm64::smp::CpuStart {
            entry,
            context: dtb,
        },
        machine,
        |group| {
            ready
                .send((group, keyboard_handle, pointer_handle, bus.quiesce()))
                .map_err(|_| "VM monitor stopped before CPU initialization".to_string())
        },
    );
    bus.shutdown();
    std::io::stdout().flush()?;
    tracing::info!(name=%cfg.name, "ARM64 guest stopped");
    result.map_err(Into::into)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn arm_profile() -> VmConfig {
        VmConfig::from_toml(include_str!("../../../examples/macos-arm64.toml")).unwrap()
    }

    #[test]
    fn serial_marker_is_exact_bounded_and_survives_overlong_lines() {
        let marker = SerialMarker {
            expected: "READY".into(),
            seen: Arc::new(AtomicBool::new(false)),
            powered_off: Arc::new(AtomicBool::new(false)),
        };
        let mut writer = MarkerWriter {
            output: Vec::new(),
            marker: marker.clone(),
            line: Vec::new(),
            discarded: false,
        };
        writer
            .write_all(b"command line READY\nREADY trailing\n")
            .unwrap();
        assert!(!marker.seen.load(Ordering::Acquire));
        writer
            .write_all(&vec![b'x'; MARKER_LINE_BYTES * 8])
            .unwrap();
        assert!(writer.line.len() <= MARKER_LINE_BYTES);
        writer.write_all(b"READY\nREADY\r\n").unwrap();
        assert!(marker.seen.load(Ordering::Acquire));
        assert!(!marker.powered_off.load(Ordering::Acquire));
    }

    #[test]
    fn unsupported_architecture_and_cpu_state_are_refused_before_host_setup() {
        let mut cfg = arm_profile();
        let options = RunOptions::default();
        assert!(validate(&cfg, &options).is_ok());
        cfg.architecture = GuestArchitecture::X86_64;
        assert!(validate(&cfg, &options).unwrap_err().contains("aarch64"));
        cfg = arm_profile();
        for count in [1, 2, 4, 6, 12, 64] {
            cfg.vcpus = count;
            assert!(validate(&cfg, &options).is_ok());
        }
        for count in [0, 65] {
            cfg.vcpus = count;
            assert!(validate(&cfg, &options).unwrap_err().contains("vCPU"));
        }
        cfg = arm_profile();
        let restore = RunOptions {
            restore: Some("x86.esnap".into()),
            ..Default::default()
        };
        assert!(validate(&cfg, &restore).unwrap_err().contains("snapshot"));
        cfg.boot.mode = BootMode::Uefi;
        assert!(validate(&cfg, &options).is_err());
    }
}
