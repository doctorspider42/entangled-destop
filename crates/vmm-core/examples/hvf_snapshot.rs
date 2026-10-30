//! Opt-in native state acceptance. Run as a separately signed executable.
#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    use vm_memory::{Bytes, GuestAddress};
    use vmm_core::hvf::{Arm64Exit, HvfGic, HvfVm, Register};
    const BASE: u64 = 0x4000_0000;
    let memory = vmm_core::GuestMem::from_ranges(&[(GuestAddress(BASE), 16384)])?;
    // Increment x0, then exit. Restore continues with another increment, not reset.
    let code: Vec<u8> = [0x91000400_u32, 0xd4000002, 0x91000400, 0xd4000002]
        .into_iter()
        .flat_map(u32::to_le_bytes)
        .collect();
    memory.write_slice(&code, GuestAddress(BASE))?;
    let (state, gic_state) = {
        let vm = HvfVm::new(memory.clone())?;
        let gic = HvfGic::new(&vm, 0x0800_0000, 0x080a_0000)?;
        let mut cpu = vm.create_vcpu()?;
        cpu.set_register(Register::Pc, BASE)?;
        cpu.set_register(Register::Pstate, 0x3c5)?;
        cpu.set_register(Register::X(0), 41)?;
        assert!(matches!(cpu.run()?, Arm64Exit::Exception { .. }));
        assert_eq!(cpu.register(Register::X(0))?, 42);
        let mut saved = cpu.save_state()?;
        for (reg, value) in saved.simd.iter_mut().enumerate() {
            for (byte, value) in value.iter_mut().enumerate() {
                *value = (reg * 16 + byte) as u8;
            }
        }
        cpu.load_state(&saved)?;
        assert_eq!(cpu.save_state()?.simd, saved.simd);
        gic.set_spi(48, true)?;
        assert_eq!(saved.registers[0], 42);
        (saved, gic.save_state()?)
    };
    let vm = HvfVm::new(memory)?;
    let gic = HvfGic::new(&vm, 0x0800_0000, 0x080a_0000)?;
    let mut cpu = vm.create_vcpu()?;
    cpu.load_state(&state)?;
    gic.load_state(&gic_state)?;
    assert_eq!(cpu.save_state()?.simd, state.simd);
    #[link(name = "Hypervisor", kind = "framework")]
    extern "C" {
        fn hv_gic_get_distributor_reg(reg: u16, value: *mut u64) -> i32;
    }
    let mut pending = 0;
    assert_eq!(
        // SAFETY: live GIC, SDK ISPENDR1 offset and a writable u64 output local.
        unsafe { hv_gic_get_distributor_reg(0x204, &mut pending) },
        0
    );
    assert_ne!(pending & (1 << 16), 0, "pending SPI was lost");
    assert!(matches!(cpu.run()?, Arm64Exit::Exception { .. }));
    assert_eq!(cpu.register(Register::X(0))?, 43);
    println!("PASS: native CPU and GIC survive VM destruction and reconstruction; execution continues at 43");
    Ok(())
}
#[cfg(not(all(target_os = "macos", target_arch = "aarch64")))]
fn main() {
    panic!("requires macOS ARM64");
}
