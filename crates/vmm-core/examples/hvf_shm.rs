//! Actual 4 KiB data sharing on HVF, including mapping retirement and NX.
#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    use vm_memory::{Bytes, GuestAddress};
    use vmm_core::hvf::{Arm64Exit, HvfGic, HvfOptions, HvfVm, Register};
    use vmm_core::shm::{HostRange, HostShmRegion};
    const RAM: u64 = 0x4000_0000;
    const WINDOW: u64 = 0x1_0000_0000;
    let pages = HostShmRegion::new(16384)?;
    pages.write(4096, &41u64.to_le_bytes())?;
    let mem = vmm_core::GuestMem::from_ranges(&[(GuestAddress(RAM), 16384)])?;
    // ldr x0,[x1]; add x0,x0,#1; str x0,[x1]; hvc #0.
    let code: Vec<u8> = [0xf9400020u32, 0x91000400, 0xf9000020, 0xd4000002]
        .into_iter()
        .flat_map(u32::to_le_bytes)
        .collect();
    mem.write_slice(&code, GuestAddress(RAM))?;
    let vm = HvfVm::with_options(
        mem,
        HvfOptions {
            small_page_mappings: true,
        },
    )?;
    let _gic = HvfGic::new(&vm, 0x0800_0000, 0x0a00_0000)?;
    let window = vm.create_shm_window(65536, true)?;
    window.place(WINDOW)?;
    // SAFETY: this is a 4 KiB prefix inside pages' live 16 KiB allocation;
    // the window is explicitly unmapped/dropped before pages can be freed.
    let range = unsafe { HostRange::new(pages.host_addr() + 4096, 4096) };
    window.map_host_range(4096, range)?;
    let other = vm.create_shm_window(65536, true)?;
    for forbidden in [RAM, 0x0800_0000, WINDOW] {
        other.place(forbidden)?;
        if other.map_host_range(4096, range).is_ok() {
            return Err("overlapping protected range accepted".into());
        }
        other.unplace()?;
    }
    let mut cpu = vm.create_vcpu()?;
    cpu.set_register(Register::Pc, RAM)?;
    cpu.set_register(Register::Pstate, 0x3c5)?;
    cpu.set_register(Register::X(1), WINDOW + 4096)?;
    if !matches!(cpu.run()?, Arm64Exit::Exception { syndrome, .. } if syndrome >> 26 == 0x16)
        || cpu.register(Register::X(0))? != 42
    {
        return Err("guest did not read shared host pages".into());
    }
    let mut bytes = [0; 8];
    pages.read(4096, &mut bytes)?;
    if u64::from_le_bytes(bytes) != 42 {
        return Err("guest write was not visible to host".into());
    }
    cpu.set_register(Register::Pc, WINDOW + 4096)?;
    if !matches!(cpu.run()?, Arm64Exit::Exception { syndrome, physical_address, .. }
        if syndrome >> 26 == 0x20 && physical_address == WINDOW + 4096)
        || cpu.register(Register::Pc)? != WINDOW + 4096
    {
        return Err("shared data was executable".into());
    }
    window.unmap_host_range(4096);
    cpu.set_register(Register::Pc, RAM)?;
    if !matches!(cpu.run()?, Arm64Exit::Exception { syndrome, physical_address, .. }
        if syndrome >> 26 == 0x24 && physical_address == WINDOW + 4096)
        || cpu.register(Register::Pc)? != RAM
    {
        return Err("guest retained access after unmap".into());
    }
    drop(cpu);
    drop(other);
    drop(window);
    drop(pages);
    println!("PASS: 4 KiB shared mapping, bidirectional guest access, RAM/GIC/window isolation, NX and unmap");
    Ok(())
}

#[cfg(not(all(target_os = "macos", target_arch = "aarch64")))]
fn main() {
    eprintln!("hvf_shm requires Apple Silicon and macOS 26+");
    std::process::exit(1);
}
