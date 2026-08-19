//! `entangled run` — boots a direct-linux VM to the serial console with its
//! virtio-mmio devices attached (backlog MVP-1202..1206; display and the
//! remaining device epics plug in here as they land).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use control_api::{BootMode, VirtioTransport, VmConfig};
use machine_x86::boot as x86_boot;
use machine_x86::bus::MachineBus;
use machine_x86::serial::SerialConsole;
use machine_x86::virtio::VirtioMmioBus;
use machine_x86::virtio_pci::VirtioPciBus;
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

    // Interrupt topology: without an MP table the guest never programs the
    // IOAPIC and irqfd injections are intermittently lost (stalled first
    // virtio-blk read with INT_VRING left pending).
    machine_x86::mptable::write(vm.memory(), cfg.vcpus).map_err(|e| e.to_string())?;
    // ACPI tables alongside the MP table: MADT/FADT/DSDT give the guest SMP
    // topology, the PM block and a real S5 poweroff; Linux prefers them and
    // falls back to the MP table when absent.
    machine_x86::acpi::write(vm.memory(), cfg.vcpus).map_err(|e| e.to_string())?;

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
    // already exist (scripts/setup-tap.sh) — entangled itself never needs
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

    // The UEFI variable store (UEFI-1804). Opened before the bus so the bus can
    // carry it, and before the firmware is loaded so a bad NVRAM path fails
    // before the VM starts rather than mid-boot.
    let pflash = match (cfg.boot.mode, &cfg.boot.nvram) {
        (BootMode::Uefi, Some(path)) => Some(Arc::new(std::sync::Mutex::new(
            machine_x86::pflash::Pflash::open(path)
                .map_err(|e| format!("cannot open the UEFI variable store: {e}"))?,
        ))),
        (BootMode::DirectLinux, _) | (_, None) => None,
    };

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
            let cmdline = extend_cmdline(&cfg.boot.cmdline, &virtio.cmdline_clauses());
            (MachineBus::with_virtio(serial, virtio), cmdline)
        }
        VirtioTransport::Pci => {
            let pci = VirtioPciBus::attach(vm.fd_shared(), Arc::clone(&mem), devices)
                .map_err(|e| e.to_string())?;
            let cmdline = cfg.boot.cmdline.trim().to_string();
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
    let entry = match cfg.boot.mode {
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
                x86_boot::setup_long_mode_sregs(vm.memory(), vcpu).map_err(|e| e.to_string())?;
                if vcpu.index == 0 {
                    // Only the boot CPU starts at the kernel entry; the others
                    // wait for INIT/SIPI from the guest.
                    x86_boot::setup_boot_regs(vcpu, loaded.entry, loaded.boot_params_addr)
                        .map_err(|e| e.to_string())?;
                }
            }
            BootEntry {
                vcpus,
                entry: loaded.entry,
            }
        }
        BootMode::Uefi => start_uefi(&mut vm, &cfg, mem_size)?,
    };
    let (vcpus, entry_addr) = (entry.vcpus, entry.entry);

    // Lifecycle (MVP-1203): Created -> Running -> Stopping -> Stopped, any
    // vCPU error -> Crashed. Transitions are validated by VmState itself.
    let mut state = VmState::Created;
    tracing::info!(entry = format_args!("{entry_addr:#x}"), mode = ?cfg.boot.mode, state = ?state, "VM created");

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

/// The vCPUs, taken out of the VM and started, plus where vCPU0 begins
/// executing (for the log line).
struct BootEntry {
    vcpus: Vec<vmm_core::Vcpu>,
    entry: u64,
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
fn start_uefi(vm: &mut vmm_core::Vm, cfg: &VmConfig, mem_size: u64) -> Result<BootEntry, String> {
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
            let boot =
                uefi_boot::load_pvh(vm.memory(), &image, mem_size).map_err(|e| e.to_string())?;
            let vcpus = vm.take_vcpus();
            for vcpu in &vcpus {
                x86_boot::setup_pvh_sregs(vm.memory(), vcpu).map_err(|e| e.to_string())?;
                if vcpu.index == 0 {
                    x86_boot::setup_pvh_regs(vcpu, boot.entry, boot.start_info_addr)
                        .map_err(|e| e.to_string())?;
                }
            }
            Ok(BootEntry {
                vcpus,
                entry: boot.entry,
            })
        }
        uefi_boot::FirmwareKind::ResetVector => {
            if cfg.boot.nvram.is_some() {
                return Err(
                    "boot.nvram cannot be used with a reset-vector firmware image: that                      image *is* its own flash and is mapped over the same window as the                      pflash device (machine_x86::layout::PFLASH_BASE). Drop boot.nvram,                      or use a PVH firmware such as CLOUDHV.fd"
                        .to_string(),
                );
            }
            let placement =
                uefi_boot::rom::place_at_top_of_32bit(image.len()).map_err(|e| e.to_string())?;
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
            Ok(BootEntry {
                vcpus: vm.take_vcpus(),
                entry: machine_x86::layout::RESET_VECTOR,
            })
        }
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

    /// `control_api` bounds `memory_mib` so a too-large guest is a typed config
    /// error, but it deliberately does not depend on the machine crate — so the
    /// number it uses is a copy, and this is the only place that sees both.
    /// Without it, a change to the memory layout would silently turn a rejected
    /// config back into a panic inside `machine_x86::e820_map`.
    #[test]
    fn the_config_memory_ceiling_is_the_machines_mmio_hole() {
        assert_eq!(
            control_api::MAX_MEMORY_MIB << 20,
            machine_x86::layout::MMIO_HOLE_START,
            "control_api::MAX_MEMORY_MIB and machine_x86::layout::MMIO_HOLE_START disagree"
        );
        // And the largest allowed guest really does build an E820 map.
        let map = machine_x86::e820_map(control_api::MAX_MEMORY_MIB << 20);
        assert_eq!(
            map.iter().map(|e| e.size).sum::<u64>(),
            control_api::MAX_MEMORY_MIB << 20
        );
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
}
