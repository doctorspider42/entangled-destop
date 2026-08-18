//! VM assembly: memory registration, in-kernel IRQ chip/PIT and vCPU
//! creation (backlog MVP-102/103/106).

use std::sync::Arc;

use kvm_bindings::{
    kvm_pit_config, kvm_userspace_memory_region, KVM_MEM_READONLY, KVM_PIT_SPEAKER_DUMMY,
};
use kvm_ioctls::{Cap, VmFd};
use vm_memory::{Address, GuestMemory, GuestMemoryRegion, MemoryRegionAddress, MmapRegion};

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
    /// Firmware ROM slots (EPIC 18). Deliberately *not* part of `memory`: a
    /// ROM is not guest RAM, must never show up in the E820/PVH memory map,
    /// and is handed to devices through neither. Held here so the host mapping
    /// outlives every KVM reference to it.
    roms: Vec<RomRegion>,
    /// Whether this host's KVM can mark a memory slot read-only.
    readonly_mem: bool,
}

/// A firmware image mapped into its own KVM memory slot.
pub struct RomRegion {
    /// The host mapping backing the slot. Never read from Rust again — KVM
    /// holds the only live reference — but dropping it would unmap memory the
    /// guest is executing from, so it stays owned by the `Vm`.
    _mapping: MmapRegion<()>,
    pub guest_addr: u64,
    pub len: u64,
    pub read_only: bool,
}

impl Vm {
    pub fn new(hv: &Hypervisor, cfg: &MachineConfig) -> Result<Self, VmmError> {
        let fd = Arc::new(hv.kvm().create_vm()?);
        let readonly_mem = hv.kvm().check_extension(Cap::ReadonlyMem);
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
        Ok(Self {
            fd,
            memory,
            vcpus,
            roms: Vec::new(),
            readonly_mem,
        })
    }

    pub fn memory(&self) -> &GuestMem {
        &self.memory
    }

    /// Maps a firmware image at `guest_addr` as its own KVM memory slot
    /// (backlog UEFI-1801). The image is copied into a fresh anonymous
    /// mapping, so the caller's buffer is free afterwards.
    ///
    /// The slot is marked read-only where KVM supports it
    /// (`KVM_CAP_READONLY_MEM`, universal on x86): guest writes then leave the
    /// kernel as MMIO exits, which the machine bus drops. That is the phase-1
    /// stand-in for a pflash device — firmware variable writes are discarded
    /// rather than persisted (see ADR-0003, "writable pflash / NVRAM").
    ///
    /// `guest_addr + len` may exceed the RAM size; that is the point — a
    /// reset-vector firmware ROM lives at the top of the 32-bit address space,
    /// far above any guest RAM region.
    pub fn map_rom(&mut self, guest_addr: u64, image: &[u8]) -> Result<&RomRegion, VmmError> {
        if image.is_empty() {
            return Err(VmmError::GuestMemory("firmware image is empty".into()));
        }
        let page = 0x1000usize;
        if guest_addr % page as u64 != 0 || image.len() % page != 0 {
            return Err(VmmError::GuestMemory(format!(
                "firmware ROM must be page aligned in address and size \
                 (got {guest_addr:#x} + {:#x})",
                image.len()
            )));
        }
        if self.roms.iter().any(|r| {
            let new_end = guest_addr.saturating_add(image.len() as u64);
            let end = r.guest_addr.saturating_add(r.len);
            guest_addr < end && r.guest_addr < new_end
        }) {
            return Err(VmmError::GuestMemory(format!(
                "a firmware ROM is already mapped over {guest_addr:#x}"
            )));
        }

        let mapping = MmapRegion::<()>::new(image.len())
            .map_err(|e| VmmError::GuestMemory(format!("cannot mmap firmware ROM: {e}")))?;
        // SAFETY: `MmapRegion::new(len)` returned a private anonymous mapping
        // of exactly `len` bytes that nothing else references yet, and
        // `image.len() == len`. Source and destination cannot overlap: the
        // destination is a brand-new mapping.
        unsafe {
            std::ptr::copy_nonoverlapping(image.as_ptr(), mapping.as_ptr(), image.len());
        }

        let slot = (self.memory.num_regions() + self.roms.len()) as u32;
        let mr = kvm_userspace_memory_region {
            slot,
            flags: if self.readonly_mem {
                KVM_MEM_READONLY
            } else {
                0
            },
            guest_phys_addr: guest_addr,
            memory_size: image.len() as u64,
            userspace_addr: mapping.as_ptr() as u64,
        };
        // SAFETY: the slot describes the mapping created above — page-aligned,
        // exactly `memory_size` bytes, owned by `self.roms` from here on and
        // therefore alive for as long as `self.fd`, which is dropped after it.
        unsafe { self.fd.set_user_memory_region(mr) }?;

        self.roms.push(RomRegion {
            _mapping: mapping,
            guest_addr,
            len: image.len() as u64,
            read_only: self.readonly_mem,
        });
        tracing::info!(
            slot,
            addr = format_args!("{guest_addr:#x}"),
            len = image.len(),
            read_only = self.readonly_mem,
            "mapped firmware ROM"
        );
        // `push` above guarantees the vector is non-empty.
        Ok(self.roms.last().expect("just pushed"))
    }

    /// The firmware ROMs mapped into this VM (empty for direct-Linux boots).
    pub fn roms(&self) -> &[RomRegion] {
        &self.roms
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
