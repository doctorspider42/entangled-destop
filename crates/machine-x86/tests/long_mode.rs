//! Long-mode entry test (backlog MVP-105): the host-built GDT, page tables
//! and control registers must let a vCPU execute 64-bit code immediately —
//! the same state a bzImage's 64-bit entry point expects. Self-skips
//! without a usable /dev/kvm.

#![cfg(target_os = "linux")]

use std::sync::atomic::AtomicBool;

use machine_x86::{boot, layout};
use vm_memory::{Bytes, GuestAddress};
use vmm_core::{ExitHandler, Hypervisor, MachineConfig, RunOutcome, Vm};

#[derive(Default)]
struct Recorder {
    io_writes: Vec<(u16, Vec<u8>)>,
}

impl ExitHandler for Recorder {
    fn io_out(&mut self, port: u16, data: &[u8]) {
        self.io_writes.push((port, data.to_vec()));
    }
    fn io_in(&mut self, _port: u16, data: &mut [u8]) {
        data.fill(0xff);
    }
    fn mmio_write(&mut self, _addr: u64, _data: &[u8]) {}
    fn mmio_read(&mut self, _addr: u64, data: &mut [u8]) {
        data.fill(0);
    }
}

#[test]
fn vcpu_enters_long_mode_and_runs_64bit_code() {
    let hv = match Hypervisor::open() {
        Ok(hv) => hv,
        Err(e) => {
            eprintln!("skipping KVM test: {e}");
            return;
        }
    };
    let cfg = MachineConfig {
        memory_mib: 128,
        vcpu_count: 1,
    };
    let mut vm = Vm::new(&hv, &cfg).unwrap();

    // 64-bit code, loaded like a kernel at 1 MiB:
    //   mov eax, esi   (89 f0)      — prove rsi carried the boot_params GPA
    //   out 0x10, al   (e6 10)      — low byte of it to the test port
    //   ud2            (0f 0b)      — triple fault => KVM_EXIT_SHUTDOWN
    let code = [0x89, 0xf0, 0xe6, 0x10, 0x0f, 0x0b];
    vm.memory()
        .write_slice(&code, GuestAddress(layout::HIGH_RAM_START))
        .unwrap();

    let mut vcpu = vm.take_vcpus().remove(0);
    boot::setup_long_mode_sregs(vm.memory(), &vcpu).unwrap();
    // Pass a boot_params address with a recognizable low byte.
    boot::setup_boot_regs(
        &vcpu,
        layout::HIGH_RAM_START,
        layout::ZERO_PAGE_START | 0x5a,
    )
    .unwrap();

    let mut rec = Recorder::default();
    let running = AtomicBool::new(true);
    let outcome = vcpu.run_loop(&mut rec, &running).unwrap();

    assert_eq!(outcome, RunOutcome::Shutdown);
    assert_eq!(rec.io_writes, vec![(0x10, vec![0x5a])]);
}
