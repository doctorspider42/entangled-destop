//! Guest-visible CPUID identity.
//!
//! `KVM_GET_SUPPORTED_CPUID` returns a snapshot taken on whichever host CPU
//! serviced the ioctl, so the APIC-ID fields in it belong to the *host*.
//! A VMM has to overwrite them per vCPU; forgetting to is invisible until some
//! guest cross-checks the CPUID identity against the local APIC and concludes
//! it is running on a processor that does not exist (EDK2's `GetBspNumber()`
//! asserts on precisely this — see UEFI-1802).
//!
//! Self-skips without a usable /dev/kvm.

#![cfg(target_os = "linux")]

use vmm_core::{Hypervisor, MachineConfig, Vm};

const VCPUS: u32 = 4;

#[test]
fn every_vcpu_reports_its_own_apic_id() {
    let Ok(hv) = Hypervisor::open() else {
        eprintln!("skipping KVM test: /dev/kvm unavailable");
        return;
    };
    let mut vm = Vm::new(
        &hv,
        &MachineConfig {
            memory_mib: 16,
            vcpu_count: VCPUS,
        },
    )
    .unwrap();

    let mut seen = Vec::new();
    for vcpu in vm.take_vcpus() {
        let cpuid = vcpu.fd().get_cpuid2(256).unwrap();
        let mut leaf1_apic_id = None;
        for entry in cpuid.as_slice() {
            match entry.function {
                1 if entry.index == 0 => {
                    leaf1_apic_id = Some(entry.ebx >> 24);
                    assert_ne!(
                        entry.ecx & (1 << 31),
                        0,
                        "vcpu {}: hypervisor bit must be set",
                        vcpu.index
                    );
                    assert_eq!(
                        (entry.ebx >> 8) & 0xff,
                        8,
                        "vcpu {}: CLFLUSH line size must survive the APIC-ID fixup",
                        vcpu.index
                    );
                }
                // x2APIC ID in EDX of the topology leaves.
                0xb | 0x1f => assert_eq!(
                    entry.edx, vcpu.index,
                    "vcpu {}: leaf {:#x} subleaf {} reports x2APIC id {:#x}",
                    vcpu.index, entry.function, entry.index, entry.edx
                ),
                _ => {}
            }
        }
        let apic_id = leaf1_apic_id.expect("CPUID leaf 1 must be present");
        assert_eq!(
            apic_id, vcpu.index,
            "vcpu {} reports initial APIC id {apic_id:#x}",
            vcpu.index
        );
        seen.push(apic_id);
    }

    assert_eq!(seen.len(), VCPUS as usize);
    let mut unique = seen.clone();
    unique.sort_unstable();
    unique.dedup();
    assert_eq!(
        unique, seen,
        "APIC ids must be unique across vCPUs: {seen:?}"
    );
}
