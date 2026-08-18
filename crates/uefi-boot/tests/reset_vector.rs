//! Reset-vector firmware boot, end to end on KVM (backlog UEFI-1801).
//!
//! No real firmware involved: a hand-written blob is placed in the last 16
//! bytes of a small fake ROM, the ROM is mapped so that its last byte is at
//! 4 GiB − 1, and the vCPU is started **without touching a single register** —
//! because `KVM_CREATE_VCPU` already leaves it in the architectural reset
//! state. If the blob's port writes come back, the whole path works: placement
//! arithmetic, the read-only memory slot, and the reset-state boot.
//!
//! Self-skips where /dev/kvm is absent or inaccessible, like the vmm-core smoke
//! tests do.

#![cfg(target_os = "linux")]

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use uefi_boot::rom;
use vmm_core::hv::VcpuRegisters;
use vmm_core::{ExitHandler, Hypervisor, MachineConfig, RunOutcome, Vm, VmmError};

/// The port the blob writes to, and the two values it writes.
const PROBE_PORT: u16 = 0x10;
const FIRST: u8 = 0x42;
const SECOND: u8 = 0x43;

/// 16-bit real-mode code, executed straight out of the ROM at `0xffff_fff0`:
///
/// ```text
///   b0 42    mov al, 0x42
///   e6 10    out 0x10, al
///   b0 43    mov al, 0x43
///   e6 10    out 0x10, al
///   eb fe    jmp $        ; park here; the host stops us after the 2nd write
/// ```
///
/// `jmp $` rather than `ud2` on purpose: an exception would depend on the IDT,
/// and this test must not touch guest state that the reset state defines.
const RESET_BLOB: [u8; 10] = [
    0xb0, FIRST, 0xe6, 0x10, 0xb0, SECOND, 0xe6, 0x10, 0xeb, 0xfe,
];

/// Builds a fake flash image of `len` bytes whose last 16 bytes contain the
/// blob — exactly how a real firmware puts its reset vector at `0xffff_fff0`.
fn fake_rom(len: usize) -> Vec<u8> {
    assert!(len >= 16 && len % 0x1000 == 0);
    let mut image = vec![0xffu8; len]; // erased flash
    let vector = len - 16;
    image[vector..vector + RESET_BLOB.len()].copy_from_slice(&RESET_BLOB);
    image
}

fn hypervisor_or_skip() -> Option<Hypervisor> {
    match Hypervisor::open() {
        Ok(hv) => Some(hv),
        Err(e) => {
            eprintln!("skipping KVM test: {e}");
            None
        }
    }
}

/// Records port writes and asks the run loop to stop once both arrived.
#[derive(Clone)]
struct Probe {
    writes: Arc<Mutex<Vec<u8>>>,
    running: Arc<AtomicBool>,
}

impl ExitHandler for Probe {
    fn io_out(&mut self, port: u16, data: &[u8]) {
        if port != PROBE_PORT {
            return;
        }
        let mut writes = match self.writes.lock() {
            Ok(w) => w,
            Err(_) => return,
        };
        writes.extend_from_slice(data);
        if writes.len() >= 2 {
            self.running.store(false, Ordering::Release);
        }
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
fn firmware_rom_executes_from_the_reset_vector() {
    let Some(hv) = hypervisor_or_skip() else {
        return;
    };
    let mut vm = Vm::new(
        &hv,
        &MachineConfig {
            memory_mib: 16,
            vcpu_count: 1,
        },
    )
    .unwrap();

    // 64 KiB of "flash": enough to prove the mapping, small enough to be free.
    let image = fake_rom(0x1_0000);
    let placement = rom::place_at_top_of_32bit(image.len() as u64).unwrap();
    assert_eq!(placement.end(), 0x1_0000_0000);
    assert!(placement.contains(machine_x86::layout::RESET_VECTOR));

    let mapped = vm.map_rom(placement.guest_addr, &image).unwrap();
    assert_eq!(mapped.guest_addr, placement.guest_addr);
    assert_eq!(mapped.len, image.len() as u64);
    assert_eq!(vm.roms().len(), 1);

    let mut vcpus = vm.take_vcpus();
    let mut vcpu = vcpus.remove(0);

    // The contract this whole boot mode rests on: KVM's fresh vCPU *is* the
    // architectural reset state, so the first fetch is at 0xffff_fff0.
    let sregs = vcpu.get_special_registers().unwrap();
    let regs = vcpu.get_registers().unwrap();
    assert_eq!(sregs.cs.base, 0xffff_0000, "reset CS base");
    assert_eq!(sregs.cs.selector, 0xf000, "reset CS selector");
    assert_eq!(regs.rip, 0xfff0, "reset IP");
    assert_eq!(sregs.cs.base + regs.rip, machine_x86::layout::RESET_VECTOR);
    assert_eq!(sregs.cr0 & 1, 0, "reset CR0 must have PE clear (real mode)");
    assert_eq!(sregs.cr4, 0, "reset CR4 must be zero");
    assert_eq!(sregs.efer, 0, "reset EFER must be zero (no long mode)");

    let probe = Probe {
        writes: Arc::new(Mutex::new(Vec::new())),
        running: Arc::new(AtomicBool::new(true)),
    };
    let mut handler = probe.clone();
    let outcome = vcpu.run_loop(&mut handler, &probe.running).unwrap();

    assert_eq!(outcome, RunOutcome::Stopped);
    assert_eq!(
        *probe.writes.lock().unwrap(),
        vec![FIRST, SECOND],
        "the guest did not execute the blob from the top of the ROM"
    );
}

#[test]
fn rom_mapping_validates_its_input() {
    let Some(hv) = hypervisor_or_skip() else {
        return;
    };
    let mut vm = Vm::new(
        &hv,
        &MachineConfig {
            memory_mib: 16,
            vcpu_count: 1,
        },
    )
    .unwrap();

    // Empty, unaligned size, unaligned address: typed errors, no panic.
    assert!(matches!(
        vm.map_rom(0xffff_0000, &[]),
        Err(VmmError::GuestMemory(_))
    ));
    assert!(matches!(
        vm.map_rom(0xffff_0000, &[0xff; 0x1001]),
        Err(VmmError::GuestMemory(_))
    ));
    assert!(matches!(
        vm.map_rom(0xffff_0800, &[0xff; 0x1000]),
        Err(VmmError::GuestMemory(_))
    ));
    assert!(vm.roms().is_empty(), "no failed attempt may leave a slot");

    // A second ROM overlapping the first is refused rather than silently
    // shadowing it.
    vm.map_rom(0xffff_0000, &[0xff; 0x1000]).unwrap();
    assert!(matches!(
        vm.map_rom(0xffff_0000, &[0xff; 0x1000]),
        Err(VmmError::GuestMemory(_))
    ));
    assert!(matches!(
        vm.map_rom(0xfffe_f000, &[0xff; 0x2000]),
        Err(VmmError::GuestMemory(_))
    ));
    // A disjoint one is fine.
    vm.map_rom(0xfffe_f000, &[0xff; 0x1000]).unwrap();
    assert_eq!(vm.roms().len(), 2);
}
