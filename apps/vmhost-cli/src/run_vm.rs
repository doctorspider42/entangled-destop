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
        // sa_sigaction is declared as usize in libc; the cast is the API.
        #[allow(clippy::fn_to_numeric_cast_any)]
        {
            action.sa_sigaction = on_termination_signal as usize;
        }
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

pub fn run(cfg: VmConfig) -> Result<(), String> {
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

    let outcomes = threads.join_or_stop(
        || SHUTDOWN_REQUESTED.load(Ordering::Relaxed),
        Duration::from_millis(50),
    );
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
