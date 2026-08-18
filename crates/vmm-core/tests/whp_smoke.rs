//! WHP smoke tests (backlog WHP-1702), the Windows mirror of `smoke.rs`: a
//! guest executes real machine code, writes a known value to an I/O port and
//! terminates; a running guest can be stopped; partitions can be created and
//! destroyed repeatedly. All tests self-skip when the Windows Hypervisor
//! Platform optional feature is off, the same way the KVM tests skip without
//! `/dev/kvm`.
//!
//! # How the test guest signals completion
//!
//! `hlt`. WHP exposes no in-kernel interrupt controller, and with local APIC
//! emulation off (the default until WHP-1703) `hlt` leaves the guest with
//! nothing to wake it, so WHP reports `WHvRunVpExitReasonX64Halt` — mapped to
//! [`RunOutcome::Halted`]. That is the opposite of the KVM path, where the
//! in-kernel irqchip swallows `hlt` and the test guest has to triple-fault
//! instead. `assert_terminal` accepts either so neither host needs a special
//! case, and so the test still passes if a later phase turns APIC emulation on
//! and `hlt` starts arriving as a shutdown.
//!
//! # Why every test takes a lock
//!
//! WHP allows only **one partition per host process to have guest memory
//! mapped at a time**: creating a second partition succeeds, but its first
//! `WHvMapGpaRange` fails with `0xC0370008`
//! ("another partition with the same name already exists" — the VID names its
//! partition after the process). Sequential create/map/destroy cycles are fine
//! (see `hundred_create_destroy_cycles`), concurrent ones are not, and cargo
//! runs tests in threads of one process. [`whp_guard`] serialises them.

#![cfg(windows)]

use std::sync::atomic::AtomicBool;
use std::sync::{Mutex, MutexGuard, OnceLock};

use vm_memory::{Bytes, GuestAddress};
use vmm_core::hv::VcpuRegisters;
use vmm_core::whp::{spawn_vcpus, WhpHypervisor, WhpPartition, WHP_ENABLE_HINT};
use vmm_core::{ExitHandler, MachineConfig, RunOutcome};

const CODE_ADDR: u64 = 0x1000;

const SMOKE_CONFIG: MachineConfig = MachineConfig {
    memory_mib: 16,
    vcpu_count: 1,
};

/// Serialises the tests against WHP's one-mapped-partition-per-process limit
/// (see the module docs). A poisoned lock is recovered rather than propagated:
/// one failing test must not turn the rest into misleading poison panics.
fn whp_guard() -> MutexGuard<'static, ()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn hypervisor_or_skip() -> Option<WhpHypervisor> {
    match WhpHypervisor::open() {
        Ok(hv) => Some(hv),
        Err(e) => {
            eprintln!("skipping WHP test: {e}");
            eprintln!("(to run these tests: {WHP_ENABLE_HINT})");
            None
        }
    }
}

/// Both terminal outcomes a test guest can legitimately reach; see the module
/// docs.
fn assert_terminal(outcome: RunOutcome) {
    assert!(
        matches!(outcome, RunOutcome::Halted | RunOutcome::Shutdown),
        "expected the guest to terminate, got {outcome:?}"
    );
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

/// Puts a 16-bit real-mode blob at `CODE_ADDR` and points vCPU0 at it, using
/// only the hypervisor-neutral [`VcpuRegisters`] seam — the same setup the KVM
/// smoke test does through KVM's ioctls.
fn load_real_mode(partition: &mut WhpPartition, code: &[u8]) -> vmm_core::whp::WhpVcpu {
    partition
        .memory()
        .write_slice(code, GuestAddress(CODE_ADDR))
        .expect("guest RAM must accept the code blob");

    let mut vcpus = partition.take_vcpus();
    let vcpu = vcpus.remove(0);

    let mut sregs = vcpu.get_special_registers().unwrap();
    // Real mode at segment 0: WHP's reset state points CS at the top of the
    // 4 GiB space (base 0xffff0000), so both base and selector must be cleared.
    sregs.cs.base = 0;
    sregs.cs.selector = 0;
    // IDT limit 0 so any exception escalates straight to a triple fault, which
    // WHP surfaces as `UnrecoverableException` => `RunOutcome::Shutdown`. Same
    // reasoning as the KVM test: with a real-mode limit of 0xffff the handler
    // would be read out of zeroed guest RAM and the guest would slide through
    // zeros forever.
    sregs.idt.base = 0;
    sregs.idt.limit = 0;
    vcpu.set_special_registers(&sregs).unwrap();

    let mut regs = vcpu.get_registers().unwrap();
    regs.rip = CODE_ADDR;
    regs.rflags = 2; // reserved bit 1 must be set
    vcpu.set_registers(&regs).unwrap();
    vcpu
}

/// The register seam must survive a round trip through
/// `WHvSet`/`WHvGetVirtualProcessorRegisters` on a real vCPU — the bit packing
/// unit tests cannot prove that WHP agrees with our layout.
#[test]
fn registers_round_trip_through_whp() {
    let _guard = whp_guard();
    let Some(hv) = hypervisor_or_skip() else {
        return;
    };
    let mut partition = WhpPartition::new(&hv, &SMOKE_CONFIG).unwrap();
    let mut vcpus = partition.take_vcpus();
    let vcpu = vcpus.remove(0);

    let mut regs = vcpu.get_registers().unwrap();
    regs.rax = 0x0123_4567_89ab_cdef;
    regs.r15 = 0xfeed_face;
    regs.rip = CODE_ADDR;
    vcpu.set_registers(&regs).unwrap();
    let back = vcpu.get_registers().unwrap();
    assert_eq!(back.rax, 0x0123_4567_89ab_cdef);
    assert_eq!(back.r15, 0xfeed_face);
    assert_eq!(back.rip, CODE_ADDR);

    let mut sregs = vcpu.get_special_registers().unwrap();
    sregs.gdt.base = 0x500;
    sregs.gdt.limit = 31;
    sregs.idt.base = 0;
    sregs.idt.limit = 0;
    sregs.cs.base = 0;
    sregs.cs.selector = 0;
    vcpu.set_special_registers(&sregs).unwrap();
    let back = vcpu.get_special_registers().unwrap();
    assert_eq!(back.gdt.base, 0x500);
    assert_eq!(back.gdt.limit, 31);
    assert_eq!(back.idt.limit, 0);
    assert_eq!(back.cs.base, 0);
    assert_eq!(back.cs.selector, 0);
    // WHP resets a vCPU into real mode, so CR0.PE must be clear and the code
    // segment must still describe a usable 16-bit segment.
    assert_eq!(back.cr0 & 1, 0, "expected real mode, cr0 = {:#x}", back.cr0);
    assert_eq!(back.cs.present, 1);
}

/// EPIC 1 acceptance, WHP edition: a guest runs real machine code, the port
/// write reaches the exit handler with the right port and payload, and the
/// guest then terminates.
#[test]
fn guest_writes_io_port_and_halts() {
    let _guard = whp_guard();
    let Some(hv) = hypervisor_or_skip() else {
        return;
    };
    let mut partition = WhpPartition::new(&hv, &SMOKE_CONFIG).unwrap();
    // mov al, 0x42 ; out 0x10, al ; hlt
    let mut vcpu = load_real_mode(&mut partition, &[0xb0, 0x42, 0xe6, 0x10, 0xf4]);

    let mut rec = Recorder::default();
    let running = AtomicBool::new(true);
    let outcome = vcpu.run_loop(&mut rec, &running).unwrap();

    assert_eq!(rec.io_writes, vec![(0x10, vec![0x42])]);
    assert_terminal(outcome);
}

/// A triple fault must be reported as [`RunOutcome::Shutdown`], not as an
/// error: that is the arm `UnrecoverableException` maps to, and the KVM path's
/// only way of signalling completion.
#[test]
fn triple_fault_is_a_shutdown() {
    let _guard = whp_guard();
    let Some(hv) = hypervisor_or_skip() else {
        return;
    };
    let mut partition = WhpPartition::new(&hv, &SMOKE_CONFIG).unwrap();
    // ud2, with IDT limit 0 => #UD escalates to a triple fault.
    let mut vcpu = load_real_mode(&mut partition, &[0x0f, 0x0b]);

    let mut rec = Recorder::default();
    let running = AtomicBool::new(true);
    let outcome = vcpu.run_loop(&mut rec, &running).unwrap();

    assert_eq!(outcome, RunOutcome::Shutdown);
    assert!(rec.io_writes.is_empty());
}

/// RIP advance and the `IN` write-back: without both, this guest would either
/// loop forever on the same instruction or read a stale RAX.
#[test]
fn port_read_lands_in_al_and_rip_advances() {
    let _guard = whp_guard();
    let Some(hv) = hypervisor_or_skip() else {
        return;
    };
    let mut partition = WhpPartition::new(&hv, &SMOKE_CONFIG).unwrap();
    // in al, 0x10 ; out 0x11, al ; hlt
    // The handler returns 0xff for reads, so the echo proves the value made it
    // into AL, and reaching `hlt` at all proves RIP was advanced past both
    // I/O instructions.
    let mut vcpu = load_real_mode(&mut partition, &[0xe4, 0x10, 0xe6, 0x11, 0xf4]);

    let mut rec = Recorder::default();
    let running = AtomicBool::new(true);
    let outcome = vcpu.run_loop(&mut rec, &running).unwrap();

    assert_eq!(rec.io_writes, vec![(0x11, vec![0xff])]);
    assert_terminal(outcome);
}

/// A 16-bit `OUT DX, AX` must be reported as a two-byte write, proving the
/// `AccessSize` field of `WHV_X64_IO_PORT_ACCESS_INFO` is decoded correctly and
/// not assumed to be 1.
#[test]
fn word_wide_port_write_reports_two_bytes() {
    let _guard = whp_guard();
    let Some(hv) = hypervisor_or_skip() else {
        return;
    };
    let mut partition = WhpPartition::new(&hv, &SMOKE_CONFIG).unwrap();
    // mov ax, 0xbeef ; mov dx, 0x20 ; out dx, ax ; hlt
    let mut vcpu = load_real_mode(
        &mut partition,
        &[0xb8, 0xef, 0xbe, 0xba, 0x20, 0x00, 0xef, 0xf4],
    );

    let mut rec = Recorder::default();
    let running = AtomicBool::new(true);
    let outcome = vcpu.run_loop(&mut rec, &running).unwrap();

    assert_eq!(rec.io_writes, vec![(0x20, vec![0xef, 0xbe])]);
    assert_terminal(outcome);
}

/// A guest spinning without a single VM exit must still be stoppable — the WHP
/// path does it with `WHvCancelRunVirtualProcessor` instead of KVM's kick
/// signal.
#[test]
fn running_guest_can_be_stopped() {
    let _guard = whp_guard();
    let Some(hv) = hypervisor_or_skip() else {
        return;
    };
    let mut partition = WhpPartition::new(&hv, &SMOKE_CONFIG).unwrap();
    // jmp $ — spins forever without a single VM exit.
    let vcpu = load_real_mode(&mut partition, &[0xeb, 0xfe]);

    let threads = spawn_vcpus(vec![vcpu], |_| Box::new(Recorder::default())).unwrap();
    std::thread::sleep(std::time::Duration::from_millis(50));
    let outcomes = threads.stop();

    assert_eq!(outcomes.len(), 1);
    assert_eq!(*outcomes[0].as_ref().unwrap(), RunOutcome::Stopped);
}

/// EPIC 1 acceptance: create and destroy 100 VMs in a row. On WHP this
/// exercises `WHvDeletePartition`, `WHvDeleteVirtualProcessor` and the
/// `VirtualFree` of 16 MiB of guest RAM — a leak in any of the three would
/// exhaust the address space or the hypervisor's partition budget long before
/// the loop ends.
#[test]
fn hundred_create_destroy_cycles() {
    let _guard = whp_guard();
    let Some(hv) = hypervisor_or_skip() else {
        return;
    };
    for i in 0..100 {
        let partition =
            WhpPartition::new(&hv, &SMOKE_CONFIG).unwrap_or_else(|e| panic!("iteration {i}: {e}"));
        drop(partition);
    }
}
