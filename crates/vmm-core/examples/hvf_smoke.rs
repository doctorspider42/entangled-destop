#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    use vm_memory::{Bytes, GuestAddress};
    use vmm_core::hvf::{Arm64Exit, HvfError, HvfVm, Register};

    const RAM_BASE: GuestAddress = GuestAddress(0x4000_0000);

    for _ in 0..100 {
        let memory = vmm_core::GuestMem::from_ranges(&[(RAM_BASE, 16 * 1024)])?;
        // mov x0,#41; add x0,x0,#1; hvc #0.
        let words = [0xd2800520_u32, 0x91000400, 0xd4000002];
        let code: Vec<u8> = words.into_iter().flat_map(u32::to_le_bytes).collect();
        memory.write_slice(&code, RAM_BASE)?;
        let vm = HvfVm::new(memory)?;
        let other_memory = vmm_core::GuestMem::from_ranges(&[(RAM_BASE, 16 * 1024)])?;
        if !matches!(HvfVm::new(other_memory), Err(HvfError::VmExists)) {
            return Err("a second process VM was not refused".into());
        }
        let mut cpu = vm.create_vcpu()?;
        cpu.set_register(Register::Pc, 0x4000_0000)?;
        cpu.set_register(Register::Pstate, 0x3c5)?;
        if !matches!(cpu.run()?, Arm64Exit::Exception { syndrome, .. } if syndrome >> 26 == 0x16)
            || cpu.register(Register::X(0))? != 42
        {
            return Err("ARM64 guest did not return 42 through HVC".into());
        }
        // The vCPU's reference must keep the VM and its RAM alive.
        drop(vm);
        if cpu.register(Register::X(0))? != 42 {
            return Err("VM lifetime is not retained by the vCPU".into());
        }
        if cpu.register(Register::X(31)).is_ok() {
            return Err("invalid register was accepted".into());
        }
    }
    println!("PASS: 100 native ARM64 VM create/run/destroy cycles, HVC and retained RAM");

    let memory = vmm_core::GuestMem::from_ranges(&[(RAM_BASE, 16 * 1024)])?;
    // add x0,x0,#1; b -4: a busy guest, ended only by a cross-thread kick.
    let code: Vec<u8> = [0x91000400_u32, 0x17ffffff]
        .into_iter()
        .flat_map(u32::to_le_bytes)
        .collect();
    memory.write_slice(&code, RAM_BASE)?;
    let vm = HvfVm::new(memory)?;
    let mut cpu = vm.create_vcpu()?;
    cpu.set_register(Register::Pc, 0x4000_0000)?;
    cpu.set_register(Register::Pstate, 0x3c5)?;
    cpu.set_register(Register::X(0), 0)?;
    let cancel = cpu.canceller();
    let retired_cancel = cancel.clone();
    let worker = std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(100));
        cancel.cancel()
    });
    let exit = cpu.run()?;
    worker.join().map_err(|_| "canceller thread panicked")??;
    if exit != Arm64Exit::Canceled || cpu.register(Register::X(0))? == 0 {
        return Err("cross-thread kick did not stop a running ARM64 guest".into());
    }
    drop(cpu);
    retired_cancel.cancel()?;
    drop(vm);
    println!("PASS: cross-thread cancellation of a busy guest and harmless kick after destruction");

    let memory = vmm_core::GuestMem::from_ranges(&[(RAM_BASE, 16 * 1024)])?;
    let code: Vec<u8> = [0xd2800520_u32, 0x91000400, 0xd4000002]
        .into_iter()
        .flat_map(u32::to_le_bytes)
        .collect();
    memory.write_slice(&code, RAM_BASE)?;
    let vm = HvfVm::new(memory)?;
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
    let mut workers = Vec::new();
    for _ in 0..2 {
        let vm = vm.clone();
        let barrier = barrier.clone();
        workers.push(std::thread::spawn(move || -> Result<(), String> {
            let created = vm.create_vcpu();
            // Both workers reach the barrier even if one API call fails.
            barrier.wait();
            let mut cpu = created.map_err(|e| e.to_string())?;
            cpu.set_register(Register::Pc, 0x4000_0000)
                .map_err(|e| e.to_string())?;
            cpu.set_register(Register::Pstate, 0x3c5)
                .map_err(|e| e.to_string())?;
            let exit = cpu.run().map_err(|e| e.to_string())?;
            if !matches!(exit, Arm64Exit::Exception { syndrome, .. } if syndrome >> 26 == 0x16)
                || cpu.register(Register::X(0)).map_err(|e| e.to_string())? != 42
            {
                return Err("concurrent vCPU failed".into());
            }
            Ok(())
        }));
    }
    for worker in workers {
        worker.join().map_err(|_| "vCPU thread panicked")??;
    }
    println!("PASS: two vCPUs execute concurrently on their owning threads");
    Ok(())
}

#[cfg(not(all(target_os = "macos", target_arch = "aarch64")))]
fn main() {
    eprintln!("hvf_smoke requires macOS on Apple Silicon");
    std::process::exit(1);
}
