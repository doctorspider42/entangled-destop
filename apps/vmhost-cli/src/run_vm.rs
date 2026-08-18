//! `vmhost run` — boots a direct-linux VM to the serial console
//! (backlog MVP-1202; display and virtio devices attach here as their
//! epics land).

use control_api::VmConfig;
use machine_x86::boot as x86_boot;
use machine_x86::bus::MachineBus;
use machine_x86::serial::SerialConsole;
use vmm_core::{spawn_vcpus, Hypervisor, MachineConfig, RunOutcome, Vm};

pub fn run(cfg: VmConfig) -> Result<(), String> {
    let hv = Hypervisor::open().map_err(|e| e.to_string())?;
    let machine = MachineConfig {
        memory_mib: cfg.memory_mib,
        vcpu_count: cfg.vcpus,
    };
    let mut vm = Vm::new(&hv, &machine).map_err(|e| e.to_string())?;

    let serial =
        SerialConsole::new(vm.fd(), Box::new(std::io::stdout())).map_err(|e| e.to_string())?;
    let bus = MachineBus::new(serial);

    let boot = linux_boot::BootConfig {
        kernel: cfg.boot.kernel.clone(),
        initramfs: cfg.boot.initramfs.clone(),
        cmdline: cfg.boot.cmdline.clone(),
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

    tracing::info!(vm = %cfg.name, entry = format_args!("{:#x}", loaded.entry), "starting VM");
    let threads = spawn_vcpus(vcpus, |_| Box::new(bus.clone())).map_err(|e| e.to_string())?;
    let outcomes = threads.join();

    for (i, outcome) in outcomes.iter().enumerate() {
        match outcome {
            Ok(RunOutcome::Shutdown) => tracing::info!(vcpu = i, "guest shut down"),
            Ok(o) => tracing::info!(vcpu = i, outcome = ?o, "vCPU finished"),
            Err(e) => return Err(format!("vCPU {i} failed: {e}")),
        }
    }
    Ok(())
}
