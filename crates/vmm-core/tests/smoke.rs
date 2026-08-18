//! EPIC 1 smoke tests (backlog MVP-109 and acceptance criteria): a guest
//! executes real machine code, writes a known value to an I/O port and
//! terminates; a running guest can be stopped; VMs can be created and
//! destroyed repeatedly. All tests self-skip without a usable /dev/kvm.
//!
//! Test guests signal completion with `ud2`: with no IDT installed the
//! exception triple-faults, which KVM reports as KVM_EXIT_SHUTDOWN. (HLT
//! cannot be used — the in-kernel irqchip emulates it by blocking the vCPU.)

#![cfg(target_os = "linux")]

use std::sync::atomic::AtomicBool;

use vm_memory::{Bytes, GuestAddress};
use vmm_core::{spawn_vcpus, ExitHandler, Hypervisor, MachineConfig, RunOutcome, Vm};

const CODE_ADDR: u64 = 0x1000;

fn hypervisor_or_skip() -> Option<Hypervisor> {
    match Hypervisor::open() {
        Ok(hv) => Some(hv),
        Err(e) => {
            eprintln!("skipping KVM test: {e}");
            None
        }
    }
}

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

/// Puts a 16-bit real-mode blob at CODE_ADDR and points vCPU0 at it.
fn load_real_mode(vm: &mut Vm, code: &[u8]) -> vmm_core::Vcpu {
    vm.memory()
        .write_slice(code, GuestAddress(CODE_ADDR))
        .unwrap();
    let mut vcpus = vm.take_vcpus();
    let vcpu = vcpus.remove(0);
    let mut sregs = vcpu.fd().get_sregs().unwrap();
    sregs.cs.base = 0;
    sregs.cs.selector = 0;
    vcpu.fd().set_sregs(&sregs).unwrap();
    let mut regs = vcpu.fd().get_regs().unwrap();
    regs.rip = CODE_ADDR;
    regs.rflags = 2;
    vcpu.fd().set_regs(&regs).unwrap();
    vcpu
}

#[test]
fn guest_writes_io_port_and_halts() {
    let Some(hv) = hypervisor_or_skip() else {
        return;
    };
    let cfg = MachineConfig {
        memory_mib: 16,
        vcpu_count: 1,
    };
    let mut vm = Vm::new(&hv, &cfg).unwrap();
    // mov al, 0x42 ; out 0x10, al ; ud2
    let mut vcpu = load_real_mode(&mut vm, &[0xb0, 0x42, 0xe6, 0x10, 0x0f, 0x0b]);

    let mut rec = Recorder::default();
    let running = AtomicBool::new(true);
    let outcome = vcpu.run_loop(&mut rec, &running).unwrap();

    assert_eq!(outcome, RunOutcome::Shutdown);
    assert_eq!(rec.io_writes, vec![(0x10, vec![0x42])]);
}

#[test]
fn running_guest_can_be_stopped() {
    let Some(hv) = hypervisor_or_skip() else {
        return;
    };
    let cfg = MachineConfig {
        memory_mib: 16,
        vcpu_count: 1,
    };
    let mut vm = Vm::new(&hv, &cfg).unwrap();
    // jmp $ — spins forever without a single VM exit.
    let vcpu = load_real_mode(&mut vm, &[0xeb, 0xfe]);

    let threads = spawn_vcpus(vec![vcpu], |_| Box::new(Recorder::default())).unwrap();
    std::thread::sleep(std::time::Duration::from_millis(50));
    let outcomes = threads.stop();

    assert_eq!(outcomes.len(), 1);
    assert_eq!(*outcomes[0].as_ref().unwrap(), RunOutcome::Stopped);
}

/// EPIC 1 acceptance: the VMM can create and destroy VMs a hundred times
/// in a row (leaks would OOM or exhaust fds long before 100 iterations of
/// a 16 MiB guest).
#[test]
fn hundred_create_destroy_cycles() {
    let Some(hv) = hypervisor_or_skip() else {
        return;
    };
    let cfg = MachineConfig {
        memory_mib: 16,
        vcpu_count: 1,
    };
    for i in 0..100 {
        let vm = Vm::new(&hv, &cfg).unwrap_or_else(|e| panic!("iteration {i}: {e}"));
        drop(vm);
    }
}
