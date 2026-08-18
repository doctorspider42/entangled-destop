//! VM assembly: memory registration, in-kernel IRQ chip/PIT and vCPU
//! creation (backlog MVP-102/103/106).

use std::sync::Arc;

use kvm_bindings::{kvm_pit_config, kvm_userspace_memory_region, KVM_PIT_SPEAKER_DUMMY};
use kvm_ioctls::VmFd;
use vm_memory::{Address, GuestMemory, GuestMemoryRegion, MemoryRegionAddress};

use crate::memory::{create_guest_memory, GuestMem};
use crate::{Hypervisor, Vcpu, VmmError};

/// Hardware shape of a VM, independent of what it boots.
#[derive(Debug, Clone, Copy)]
pub struct MachineConfig {
    pub memory_mib: u64,
    pub vcpu_count: u32,
}

/// A configured (not yet running) virtual machine: guest memory registered,
/// in-kernel IRQ chip and PIT created, vCPUs created with CPUID set.
pub struct Vm {
    /// Shared because device plumbing outlives the borrow: an ioeventfd or
    /// irqfd registration must be undone through the same VM fd when the
    /// device shuts down (MVP-307), long after `Vm::new` returned.
    fd: Arc<VmFd>,
    memory: GuestMem,
    vcpus: Vec<Vcpu>,
}

impl Vm {
    pub fn new(hv: &Hypervisor, cfg: &MachineConfig) -> Result<Self, VmmError> {
        let fd = Arc::new(hv.kvm().create_vm()?);
        let memory = create_guest_memory(cfg.memory_mib << 20)?;
        register_memory(&fd, &memory)?;

        // IRQ chip and PIT must exist before vCPUs are created.
        fd.create_irq_chip()?;
        let pit = kvm_pit_config {
            flags: KVM_PIT_SPEAKER_DUMMY,
            ..Default::default()
        };
        fd.create_pit2(pit)?;

        let mut vcpus = Vec::with_capacity(cfg.vcpu_count as usize);
        for index in 0..cfg.vcpu_count {
            vcpus.push(Vcpu::new(&fd, hv.kvm(), index)?);
        }
        Ok(Self { fd, memory, vcpus })
    }

    pub fn memory(&self) -> &GuestMem {
        &self.memory
    }

    pub fn fd(&self) -> &VmFd {
        &self.fd
    }

    /// A shared handle on the VM fd, for host plumbing that must survive
    /// beyond a borrow of the `Vm` (ioeventfd/irqfd teardown).
    pub fn fd_shared(&self) -> Arc<VmFd> {
        Arc::clone(&self.fd)
    }

    /// Moves the vCPUs out for running (each vCPU is owned by exactly one
    /// thread; KVM requires vCPU ioctls to come from that thread).
    pub fn take_vcpus(&mut self) -> Vec<Vcpu> {
        std::mem::take(&mut self.vcpus)
    }
}

fn register_memory(fd: &VmFd, memory: &GuestMem) -> Result<(), VmmError> {
    for (slot, region) in memory.iter().enumerate() {
        let host_addr = region
            .get_host_address(MemoryRegionAddress(0))
            .map_err(|e| VmmError::GuestMemory(e.to_string()))?;
        let mr = kvm_userspace_memory_region {
            slot: slot as u32,
            flags: 0,
            guest_phys_addr: region.start_addr().raw_value(),
            memory_size: region.len(),
            userspace_addr: host_addr as u64,
        };
        // SAFETY: the slot maps host memory owned by `memory`, which lives in
        // the same struct as `fd` and is dropped only after the VM fd; the
        // region is a valid, page-aligned anonymous mmap of exactly
        // `memory_size` bytes.
        unsafe { fd.set_user_memory_region(mr) }?;
    }
    Ok(())
}
