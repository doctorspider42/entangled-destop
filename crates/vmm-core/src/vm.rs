//! VM assembly: memory registration, in-kernel IRQ chip/PIT and vCPU
//! creation (backlog MVP-102/103/106).

use std::sync::Arc;

use kvm_bindings::{
    kvm_pit_config, kvm_userspace_memory_region, KVM_MEM_READONLY, KVM_PIT_SPEAKER_DUMMY,
};
use kvm_ioctls::{Cap, VmFd};
use vm_memory::{Address, GuestMemory, GuestMemoryRegion, MemoryRegionAddress, MmapRegion};

use crate::hv::{GuestClock, HostIrqChip, HostIrqChipState, HvError, MachineConfig, VmClockState};
use crate::memory::{create_guest_memory, GuestMem};
use crate::shm::SharedWindow;
use crate::{Hypervisor, Vcpu, VmmError};

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
    /// Next free `KVM_SET_USER_MEMORY_REGION` slot number.
    ///
    /// Guest RAM takes the first `num_regions()`; firmware ROMs and
    /// shared-memory windows take the rest. A single counter rather than two
    /// formulas because the two used to be derived independently, and a ROM
    /// mapped after a window would silently have reused the window's slot —
    /// which KVM implements as "replace that mapping", not as an error.
    next_slot: u32,
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
        let next_slot = memory.num_regions() as u32;
        Ok(Self {
            fd,
            memory,
            vcpus,
            roms: Vec::new(),
            readonly_mem,
            next_slot,
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

        let slot = self.next_slot;
        self.next_slot += 1;
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

    /// Allocates a shared-memory window of `len` bytes and reserves the KVM
    /// memory slot it will live in (EPIC 20, VEN-2001).
    ///
    /// The window comes back **unplaced**: a virtio-pci shared-memory region
    /// lives in a BAR, and a BAR decodes nothing until the guest's driver says
    /// so. The machine layer maps it, moves it when a firmware reassigns the
    /// BAR, and unmaps it when memory decoding goes away
    /// (`machine_x86::shm`).
    ///
    /// One slot per window, reserved for the life of the VM even while the
    /// window is unmapped, because KVM identifies a mapping by its slot number
    /// and reusing one would silently replace somebody else's.
    pub fn create_shm_window(&mut self, len: u64) -> Result<Arc<SharedWindow>, VmmError> {
        let slot = self.next_slot;
        self.next_slot += 1;
        let mapper = Arc::new(KvmGpaMapper {
            fd: Arc::clone(&self.fd),
            slot,
        });
        let window = SharedWindow::new(len, mapper)?;
        tracing::info!(slot, len, "reserved a KVM memory slot for a shared-memory window");
        Ok(Arc::new(window))
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

    /// A handle on this VM's paravirtual clock (ADR-0006).
    ///
    /// Handed out as a neutral `GuestClock` rather than as the VM fd, so the
    /// machine layer can hold it without holding a `kvm_ioctls` type
    /// (ADR-0002). Shared, because it outlives every borrow of the `Vm`: a
    /// suspend reads it long after assembly is done.
    pub fn clock(&self) -> Arc<dyn GuestClock> {
        Arc::new(KvmGuestClock {
            fd: Arc::clone(&self.fd),
        })
    }

    /// A handle on this VM's **in-kernel** interrupt controllers (ADR-0006).
    ///
    /// The half of the machine `machine_x86` never sees on this host, and the
    /// one whose absence from a snapshot is invisible until a restored guest
    /// stops receiving interrupts. See [`HostIrqChipState`].
    pub fn irqchip(&self) -> Arc<dyn HostIrqChip> {
        Arc::new(KvmIrqChip {
            fd: Arc::clone(&self.fd),
        })
    }
}

/// KVM's half of a shared-memory window: one memory slot, moved by
/// re-registering it (EPIC 20, VEN-2001).
///
/// KVM has no "move a slot" ioctl and needs none — `KVM_SET_USER_MEMORY_REGION`
/// on a slot number that already exists replaces it. Deleting is the same call
/// with `memory_size = 0`, which is the kernel's documented spelling of "this
/// slot is gone" and the only way to stop the guest reaching the pages.
///
/// Note the one asymmetry with WHP, stated where it is true rather than in a
/// comment somewhere else: a KVM memory slot has no execute permission of its
/// own. `KVM_MEM_READONLY` exists, execute-disable does not; whether the guest
/// may fetch instructions from the window is decided by the guest's own page
/// tables, exactly as it is for guest RAM. WHP is told `Read | Write` and
/// really does fault an instruction fetch. Neither is a security difference
/// that matters — the pages are the guest's own writable memory either way —
/// but a reader comparing the two backends deserves to be told.
struct KvmGpaMapper {
    fd: Arc<VmFd>,
    slot: u32,
}

impl crate::shm::GpaMapper for KvmGpaMapper {
    fn map(&self, gpa: u64, region: &crate::shm::HostShmRegion) -> Result<(), HvError> {
        let mr = kvm_userspace_memory_region {
            slot: self.slot,
            flags: 0,
            guest_phys_addr: gpa,
            memory_size: region.len(),
            userspace_addr: region.host_addr(),
        };
        // SAFETY: `region` owns a live, page-aligned host mapping of exactly
        // `region.len()` bytes; the caller (`SharedWindow`) holds it for as
        // long as this slot exists and unmaps the slot before dropping it, so
        // KVM never holds a pointer into freed memory. The slot number is this
        // mapper's own, reserved by `Vm::create_shm_window` and used by nothing
        // else.
        unsafe { self.fd.set_user_memory_region(mr) }
            .map_err(|e| HvError::Registers(format!("KVM_SET_USER_MEMORY_REGION(shm): {e}")))
    }

    fn unmap(&self, _gpa: u64, _len: u64) -> Result<(), HvError> {
        let mr = kvm_userspace_memory_region {
            slot: self.slot,
            flags: 0,
            guest_phys_addr: 0,
            memory_size: 0,
            userspace_addr: 0,
        };
        // SAFETY: a zero `memory_size` deletes the slot; the kernel reads no
        // host pointer out of this structure, so there is nothing here that
        // could dangle.
        unsafe { self.fd.set_user_memory_region(mr) }
            .map_err(|e| HvError::Registers(format!("KVM_SET_USER_MEMORY_REGION(shm delete): {e}")))
    }

    fn backend(&self) -> &'static str {
        "kvm"
    }
}

/// KVM's in-kernel interrupt controllers, behind the neutral trait.
///
/// Three `KVM_GET_IRQCHIP` chips (8259 master, 8259 slave, IOAPIC) and the
/// 8254 through `KVM_GET_PIT2`. The chip blobs are the kernel's 512-byte union
/// carried verbatim; the PIT is written out field by field, which costs a
/// little code and saves a transmute.
struct KvmIrqChip {
    fd: Arc<VmFd>,
}

/// `chip_id` values `KVM_GET_IRQCHIP` accepts on x86.
const CHIP_PIC_MASTER: u32 = 0;
const CHIP_PIC_SLAVE: u32 = 1;
const CHIP_IOAPIC: u32 = 2;

/// The `kvm_irqchip` union is 512 bytes whichever arm the kernel filled.
const CHIP_BYTES: usize = 512;

/// One `kvm_pit_channel_state` as this build writes it: eleven fields, no
/// padding of our own.
const PIT_CHANNEL_BYTES: usize = 4 + 2 + 9;
/// Three channels plus the flags word.
const PIT_BYTES: usize = PIT_CHANNEL_BYTES * 3 + 4;

impl KvmIrqChip {
    fn chip(&self, chip_id: u32) -> Result<Vec<u8>, HvError> {
        let mut chip = kvm_bindings::kvm_irqchip {
            chip_id,
            ..Default::default()
        };
        self.fd.get_irqchip(&mut chip).map_err(|e| {
            HvError::Registers(format!("KVM_GET_IRQCHIP chip {chip_id} failed: {e}"))
        })?;
        // SAFETY: `chip.chip` is a union whose `dummy` arm is
        // `[c_char; 512]` — the full size of the union, with no padding and no
        // invalid bit pattern for a byte array. Reading it is therefore defined
        // whichever arm `KVM_GET_IRQCHIP` actually filled, which is precisely
        // why the kernel's own header declares that arm.
        let bytes = unsafe { chip.chip.dummy };
        Ok(bytes.iter().map(|&b| b as u8).collect())
    }

    fn set_chip(&self, chip_id: u32, bytes: &[u8]) -> Result<(), HvError> {
        if bytes.len() != CHIP_BYTES {
            return Err(HvError::Registers(format!(
                "snapshot irqchip {chip_id} is {} bytes, this host wants {CHIP_BYTES}",
                bytes.len()
            )));
        }
        let mut chip = kvm_bindings::kvm_irqchip {
            chip_id,
            ..Default::default()
        };
        let mut dummy = [0 as std::os::raw::c_char; CHIP_BYTES];
        for (slot, &byte) in dummy.iter_mut().zip(bytes) {
            *slot = byte as std::os::raw::c_char;
        }
        // Writing a union field is safe; only reading one is not.
        chip.chip.dummy = dummy;
        self.fd
            .set_irqchip(&chip)
            .map_err(|e| HvError::Registers(format!("KVM_SET_IRQCHIP chip {chip_id} failed: {e}")))
    }

    fn pit(&self) -> Result<Vec<u8>, HvError> {
        let state = self
            .fd
            .get_pit2()
            .map_err(|e| HvError::Registers(format!("KVM_GET_PIT2 failed: {e}")))?;
        let mut out = Vec::with_capacity(PIT_BYTES);
        for channel in &state.channels {
            out.extend_from_slice(&channel.count.to_le_bytes());
            out.extend_from_slice(&channel.latched_count.to_le_bytes());
            out.push(channel.count_latched);
            out.push(channel.status_latched);
            out.push(channel.status);
            out.push(channel.read_state);
            out.push(channel.write_state);
            out.push(channel.write_latch);
            out.push(channel.rw_mode);
            out.push(channel.mode);
            out.push(channel.bcd);
        }
        out.extend_from_slice(&state.flags.to_le_bytes());
        debug_assert_eq!(out.len(), PIT_BYTES);
        Ok(out)
    }

    fn set_pit(&self, bytes: &[u8]) -> Result<(), HvError> {
        if bytes.len() != PIT_BYTES {
            return Err(HvError::Registers(format!(
                "snapshot 8254 state is {} bytes, this host wants {PIT_BYTES}",
                bytes.len()
            )));
        }
        let mut state = kvm_bindings::kvm_pit_state2::default();
        let mut at = 0usize;
        let u32_at = |bytes: &[u8], at: usize| {
            u32::from_le_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]])
        };
        for channel in state.channels.iter_mut() {
            channel.count = u32_at(bytes, at);
            channel.latched_count = u16::from_le_bytes([bytes[at + 4], bytes[at + 5]]);
            channel.count_latched = bytes[at + 6];
            channel.status_latched = bytes[at + 7];
            channel.status = bytes[at + 8];
            channel.read_state = bytes[at + 9];
            channel.write_state = bytes[at + 10];
            channel.write_latch = bytes[at + 11];
            channel.rw_mode = bytes[at + 12];
            channel.mode = bytes[at + 13];
            channel.bcd = bytes[at + 14];
            at += PIT_CHANNEL_BYTES;
        }
        state.flags = u32_at(bytes, at);
        self.fd
            .set_pit2(&state)
            .map_err(|e| HvError::Registers(format!("KVM_SET_PIT2 failed: {e}")))
    }
}

impl HostIrqChip for KvmIrqChip {
    fn save_irqchip(&self) -> Result<HostIrqChipState, HvError> {
        Ok(HostIrqChipState {
            pic_master: self.chip(CHIP_PIC_MASTER)?,
            pic_slave: self.chip(CHIP_PIC_SLAVE)?,
            ioapic: self.chip(CHIP_IOAPIC)?,
            pit: self.pit()?,
        })
    }

    /// Puts them back, IOAPIC **last**.
    ///
    /// The mirror of the reset order (`machine_x86::bus::reset_devices` masks
    /// the IOAPIC first): a restored IOAPIC is immediately able to deliver, and
    /// the 8259 pair behind it should be back before it can.
    fn load_irqchip(&self, state: &HostIrqChipState) -> Result<(), HvError> {
        self.set_chip(CHIP_PIC_MASTER, &state.pic_master)?;
        self.set_chip(CHIP_PIC_SLAVE, &state.pic_slave)?;
        self.set_pit(&state.pit)?;
        self.set_chip(CHIP_IOAPIC, &state.ioapic)
    }
}

/// `KVM_GET_CLOCK`/`KVM_SET_CLOCK` behind the neutral trait.
///
/// Whether the guest *uses* the paravirtual clock is its own business; the VM
/// has one either way, and putting it back is what keeps a restored guest from
/// seeing time jump by however long the snapshot sat on disk.
struct KvmGuestClock {
    fd: Arc<VmFd>,
}

impl GuestClock for KvmGuestClock {
    fn save_clock(&self) -> Result<VmClockState, HvError> {
        let clock = self
            .fd
            .get_clock()
            .map_err(|e| HvError::Registers(format!("KVM_GET_CLOCK failed: {e}")))?;
        Ok(VmClockState {
            clock_ns: clock.clock,
            flags: clock.flags,
            realtime_ns: clock.realtime,
            host_tsc: clock.host_tsc,
        })
    }

    fn load_clock(&self, state: &VmClockState) -> Result<(), HvError> {
        // Only the clock value goes back. `KVM_CLOCK_REALTIME`/`_HOST_TSC` are
        // *output* flags describing what the read reported, and handing them to
        // `KVM_SET_CLOCK` would ask the kernel to interpret host readings from
        // another moment (on another machine, in the general case) as if they
        // were current.
        let clock = kvm_bindings::kvm_clock_data {
            clock: state.clock_ns,
            ..Default::default()
        };
        self.fd
            .set_clock(&clock)
            .map_err(|e| HvError::Registers(format!("KVM_SET_CLOCK failed: {e}")))
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
