//! WHP partition lifecycle and guest memory mapping (backlog WHP-1702).

use std::sync::{Arc, Mutex};

use vm_memory::{Address, GuestMemory, GuestMemoryRegion, MemoryRegionAddress, MmapRegion};
use windows::Win32::System::Hypervisor::{
    WHvCapabilityCodeHypervisorPresent, WHvCapabilityCodeProcessorVendor, WHvCreatePartition,
    WHvCreateVirtualProcessor, WHvDeletePartition, WHvGetCapability, WHvMapGpaRange,
    WHvMapGpaRangeFlagExecute, WHvMapGpaRangeFlagRead, WHvMapGpaRangeFlagWrite,
    WHvPartitionPropertyCodeCpuidExitList, WHvPartitionPropertyCodeExtendedVmExits,
    WHvPartitionPropertyCodeLocalApicEmulationMode, WHvPartitionPropertyCodeProcessorCount,
    WHvProcessorVendorAmd, WHvProcessorVendorHygon, WHvProcessorVendorIntel,
    WHvSetPartitionProperty, WHvSetupPartition, WHvUnmapGpaRange,
    WHvX64LocalApicEmulationModeXApic, WHV_PARTITION_HANDLE, WHV_PROCESSOR_VENDOR,
};

use crate::hv::{HvError, MachineConfig};
use crate::memory::{create_guest_memory, GuestMem};
use crate::shm::SharedWindow;
use crate::whp::interrupt::{HaltGate, WhpInterruptDelivery};
use crate::whp::regs::{seg_from_whp, zeroed_value, Aligned16};
use crate::whp::vcpu::{get_regs_raw, WhpVcpu};
use crate::VmmError;

/// Turns a WHP `HRESULT` failure into a typed error naming the call, so a WHP
/// failure is never an anonymous hex code (EPIC 1 acceptance, kept for WHP).
pub(super) fn whp_err(call: &'static str, e: windows::core::Error) -> VmmError {
    VmmError::Whp {
        call,
        message: format!("{e} ({:#010x})", e.code().0 as u32),
    }
}

/// What the host's WHP offers, for `entangled doctor`. Mirrors
/// [`crate::HostCapabilities`].
#[derive(Debug, Clone)]
pub struct WhpCapabilities {
    /// `WHvCapabilityCodeHypervisorPresent`: false when the "Windows
    /// Hypervisor Platform" optional feature is off or Hyper-V is not running.
    pub hypervisor_present: bool,
    /// CPU vendor as WHP reports it, `None` when the query failed.
    pub processor_vendor: Option<&'static str>,
}

impl WhpCapabilities {
    pub fn is_runnable(&self) -> bool {
        self.hypervisor_present
    }
}

/// What to tell the user when WHP is not available.
pub const WHP_ENABLE_HINT: &str = "enable the \"Windows Hypervisor Platform\" optional feature \
     (Windows Features, or `dism /Online /Enable-Feature /FeatureName:HypervisorPlatform`) \
     and reboot";

// The WHP constants are `WHV_*_CODE(i32)` newtypes whose names follow Win32
// casing, so matching on them trips `non_upper_case_globals`. Renaming them is
// not an option (they come from the `windows` crate) and matching on `.0`
// integers would throw away the only readable names we have.
#[allow(non_upper_case_globals)]
fn vendor_name(vendor: WHV_PROCESSOR_VENDOR) -> Option<&'static str> {
    match vendor {
        WHvProcessorVendorAmd => Some("AMD"),
        WHvProcessorVendorIntel => Some("Intel"),
        WHvProcessorVendorHygon => Some("Hygon"),
        _ => None,
    }
}

/// Reads one fixed-size WHP capability into `T`.
///
/// # Safety
///
/// `T` must be the type WHP documents for `code` (an arm of `WHV_CAPABILITY`),
/// and all-zero must be a valid bit pattern for it.
unsafe fn get_capability<T: Copy>(
    code: windows::Win32::System::Hypervisor::WHV_CAPABILITY_CODE,
) -> Result<T, VmmError> {
    // SAFETY: guaranteed by this function's contract.
    let mut value: T = unsafe { core::mem::zeroed() };
    let size = u32::try_from(size_of::<T>()).unwrap_or(u32::MAX);
    let mut written = 0u32;
    // SAFETY: `value` is a live, writable `T` of exactly `size` bytes and the
    // caller promised `T` matches `code`, so WHP writes at most `size` bytes
    // into it. `written` is a live `u32`.
    unsafe { WHvGetCapability(code, (&raw mut value).cast(), size, Some(&raw mut written)) }
        .map_err(|e| whp_err("WHvGetCapability", e))?;
    if written as usize != size_of::<T>() {
        return Err(VmmError::WhpUnavailable(format!(
            "WHvGetCapability({}) wrote {written} bytes, expected {}",
            code.0,
            size_of::<T>()
        )));
    }
    Ok(value)
}

/// A validated handle on the Windows Hypervisor Platform, the WHP counterpart
/// of [`crate::Hypervisor`].
///
/// WHP has no device node to open: "opening" it means confirming that the
/// optional feature is enabled and the hypervisor is running.
#[derive(Debug, Clone)]
pub struct WhpHypervisor {
    capabilities: WhpCapabilities,
}

impl WhpHypervisor {
    /// Confirms WHP is usable. Fails with [`VmmError::WhpUnavailable`] — which
    /// carries [`WHP_ENABLE_HINT`] — when the optional feature is off.
    pub fn open() -> Result<Self, VmmError> {
        let capabilities = Self::probe()?;
        if !capabilities.hypervisor_present {
            return Err(VmmError::WhpUnavailable(format!(
                "WHvCapabilityCodeHypervisorPresent is false — {WHP_ENABLE_HINT}"
            )));
        }
        Ok(Self { capabilities })
    }

    /// Probes capabilities without failing on a disabled feature; used by
    /// `entangled doctor` to print a diagnosis instead of an error.
    pub fn probe() -> Result<WhpCapabilities, VmmError> {
        // SAFETY: `WHvCapabilityCodeHypervisorPresent` is documented to write
        // a `BOOL` (4 bytes), for which all-zero (FALSE) is valid.
        let present: windows::core::BOOL =
            unsafe { get_capability(WHvCapabilityCodeHypervisorPresent)? };
        let hypervisor_present = present.as_bool();
        let processor_vendor = if hypervisor_present {
            // SAFETY: `WHvCapabilityCodeProcessorVendor` is documented to
            // write a `WHV_PROCESSOR_VENDOR`, a 4-byte enum over `i32`.
            unsafe { get_capability::<WHV_PROCESSOR_VENDOR>(WHvCapabilityCodeProcessorVendor) }
                .ok()
                .and_then(vendor_name)
        } else {
            None
        };
        Ok(WhpCapabilities {
            hypervisor_present,
            processor_vendor,
        })
    }

    pub fn capabilities(&self) -> &WhpCapabilities {
        &self.capabilities
    }
}

/// What a partition needs switched on beyond the phase-1 minimum.
///
/// Additive on purpose: [`WhpPartition::new`] keeps the phase-1 behaviour exactly
/// (no APIC, no CPUID interception), so the real-mode smoke guests that use `hlt`
/// to terminate still terminate. A real guest needs both, and asks for them with
/// [`WhpPartition::with_options`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct WhpOptions {
    /// Turn on WHP's in-hypervisor **local** APIC (xAPIC) emulation.
    ///
    /// Required by anything that delivers an interrupt: `WHvRequestInterrupt`
    /// fails without an APIC to request of, `WHvX64RegisterApicBase` is not even
    /// readable, and the guest gets no LAPIC timer. It also changes what `hlt`
    /// means — with an APIC there is something that can wake the CPU, so the run
    /// loop waits on [`crate::whp::HaltGate`] instead of reporting
    /// [`crate::RunOutcome::Halted`].
    pub local_apic: bool,

    /// Intercept the CPUID leaves this machine has a policy for, so the WHP
    /// guest sees the same CPUID as the KVM guest (see
    /// [`crate::whp::CPUID_EXIT_LEAVES`]).
    pub cpuid_policy: bool,
}

impl WhpOptions {
    /// Everything a real guest needs. The shape a boot path asks for.
    pub fn for_guest() -> Self {
        Self {
            local_apic: true,
            cpuid_policy: true,
        }
    }
}

/// Owns a WHP partition handle **and the guest RAM mapped into it**.
///
/// Bundling the two is what makes teardown sound: `Drop::drop` runs before the
/// struct's fields are dropped, so `WHvDeletePartition` (which tears down every
/// GPA mapping) always happens *before* the `VirtualAlloc` backing store is
/// released. Virtual processors hold an `Arc<Partition>`, so the partition
/// cannot be deleted while a vCPU handle still exists either.
pub struct Partition {
    handle: WHV_PARTITION_HANDLE,
    memory: GuestMem,
    options: WhpOptions,
    /// Shared by every vCPU's run loop and by [`WhpInterruptDelivery`]: the run
    /// loops wait on it while halted, the delivery bumps it.
    halt_gate: Arc<HaltGate>,
    /// Number of virtual processors, fixed before `WHvSetupPartition`.
    vcpu_count: u32,
    /// Firmware ROM mappings (EPIC 18): the WHP peer of [`crate::Vm`]'s ROM
    /// slots. Deliberately *not* part of `memory` — a ROM is not guest RAM and
    /// must never show up in a memory map. Held here so the host mapping
    /// outlives every hypervisor reference: `Drop::drop` runs
    /// `WHvDeletePartition` before the struct's fields are dropped, so the
    /// backing store is released only after the GPA mappings are gone. Behind a
    /// `Mutex` because mapping happens through the `Arc` every vCPU shares.
    roms: Mutex<Vec<WhpRom>>,
}

/// A firmware image mapped into the partition outside guest RAM.
struct WhpRom {
    /// The host mapping backing the range. Never read from Rust again — WHP
    /// holds the only live reference — but dropping it would unmap memory the
    /// guest is executing from, so it stays owned by the `Partition`.
    _mapping: MmapRegion<()>,
    guest_addr: u64,
    len: u64,
}

impl Partition {
    pub(super) fn handle(&self) -> WHV_PARTITION_HANDLE {
        self.handle
    }

    pub(super) fn memory(&self) -> &GuestMem {
        &self.memory
    }

    pub(super) fn options(&self) -> WhpOptions {
        self.options
    }

    pub(super) fn halt_gate(&self) -> &Arc<HaltGate> {
        &self.halt_gate
    }
}

impl Drop for Partition {
    fn drop(&mut self) {
        // SAFETY: `handle` came from a successful `WHvCreatePartition`, has not
        // been deleted before (`Drop::drop` runs exactly once) and no virtual
        // processor outlives this value — vCPUs hold an `Arc` on it.
        if let Err(e) = unsafe { WHvDeletePartition(self.handle) } {
            tracing::warn!(error = %e, "WHvDeletePartition failed");
        }
    }
}

// `Partition` holds a `WHV_PARTITION_HANDLE` (a plain `isize`) and guest
// memory, both of which are `Send`/`Sync`; the auto impls are therefore
// correct and no `unsafe impl` is needed. WHP itself is thread-safe: the only
// per-thread rule is that `WHvRunVirtualProcessor` for a given VP index must
// come from one thread at a time, which `WhpVcpu` ownership enforces.

/// A configured (not yet running) WHP virtual machine: partition created and
/// set up, guest RAM mapped, virtual processors created.
///
/// The WHP counterpart of [`crate::Vm`].
///
/// # One mapped partition per process
///
/// WHP permits only **one partition per host process to have guest memory
/// mapped at a time**. Creating a second [`WhpPartition`] while the first is
/// alive succeeds up to `WHvMapGpaRange`, which then fails with `0xC0370008`
/// ("another partition with the same name already exists" — the virtualization
/// infrastructure driver names its partition after the process). Sequential
/// create/destroy cycles are unaffected.
///
/// This is a platform constraint, not something this backend can lift, so on
/// Windows a multi-VM `entangled` must run one VM per process. KVM has no
/// equivalent limit, which makes it a genuine behavioural difference between
/// the two backends rather than an implementation detail.
///
/// # SMP needs nothing from this backend (WHP-1703)
///
/// A `vcpu_count` above one works with no INIT/SIPI code at all, and the
/// measured reason is worth recording, because the obvious implementation is a
/// trap that makes it *worse*.
///
/// WHP's xAPIC emulation models an application processor's wait-for-startup
/// state itself. Create *n* VPs, run every one of them, and each AP simply blocks
/// inside `WHvRunVirtualProcessor` until the guest's SIPI arrives — the same shape
/// as a KVM AP blocking inside `KVM_RUN` in `KVM_MP_STATE_UNINITIALIZED`. WHP then
/// applies the INIT and the SIPI, puts the VP at `vector << 12` in real mode, and
/// the run call returns with the AP executing the kernel's trampoline.
///
/// `WHV_EXTENDED_VM_EXITS.X64ApicInitSipiExitTrap` (bit 6) **replaces** that
/// handling rather than observing it. With it on, the backend gets a
/// `WHvRunVpExitReasonX64ApicInitSipiTrap` carrying the raw ICR — and the target
/// VP stays in its wait-for-startup state, so writing its `CS` and `RIP` by hand
/// does not make it runnable. Its `WHvRunVirtualProcessor` keeps blocking, and the
/// symptom is a guest that boots fine on one CPU and prints
/// `CPU1 failed to report alive state` ten seconds later. Diagnosed with
/// [`Self::processor_summary`], which showed VP 1 with exactly the `CS` the host
/// had written, `RIP` still 0, and `$ENTANGLED_WHP_TRACE_EXITS` reporting one
/// `Canceled` exit for the whole boot — an AP that never executed an instruction.
///
/// The one thing a caller must get right is that an AP has to be left in the reset
/// state WHP created it in: `machine_x86::boot::setup_long_mode_sregs` belongs to
/// the bootstrap processor only. The KVM path may hand it to every vCPU because
/// KVM's INIT throws that state away.
pub struct WhpPartition {
    partition: Arc<Partition>,
    vcpus: Vec<WhpVcpu>,
}

impl WhpPartition {
    /// Creates a partition with the phase-1 feature set: processor count only, no
    /// local APIC and no CPUID interception. What the real-mode smoke guests
    /// want.
    pub fn new(hv: &WhpHypervisor, cfg: &MachineConfig) -> Result<Self, VmmError> {
        Self::with_options(hv, cfg, WhpOptions::default())
    }

    /// Creates the partition, sets its properties, finalises it with
    /// `WHvSetupPartition`, maps guest RAM and creates the vCPUs.
    ///
    /// Order matters: partition properties may only be set *before*
    /// `WHvSetupPartition`, and GPA ranges and virtual processors only *after*.
    /// Local APIC emulation and the CPUID exit list are both properties, so a
    /// guest's shape is fixed here and cannot be changed later.
    pub fn with_options(
        _hv: &WhpHypervisor,
        cfg: &MachineConfig,
        options: WhpOptions,
    ) -> Result<Self, VmmError> {
        if cfg.vcpu_count == 0 {
            return Err(VmmError::WhpUnavailable("vcpu_count is zero".into()));
        }
        // SAFETY: no arguments; returns an owned handle or an error.
        let handle =
            unsafe { WHvCreatePartition() }.map_err(|e| whp_err("WHvCreatePartition", e))?;

        // From here on every early return must delete the partition, so wrap
        // it in `Partition` immediately — with guest memory allocated first so
        // the struct is complete.
        let memory = match create_guest_memory(cfg.memory_mib << 20) {
            Ok(memory) => memory,
            Err(e) => {
                // SAFETY: `handle` is a freshly created, not-yet-deleted
                // partition and nothing references it.
                let _ = unsafe { WHvDeletePartition(handle) };
                return Err(e);
            }
        };
        let partition = Arc::new(Partition {
            handle,
            memory,
            options,
            halt_gate: Arc::new(HaltGate::default()),
            vcpu_count: cfg.vcpu_count,
            roms: Mutex::new(Vec::new()),
        });

        set_processor_count(handle, cfg.vcpu_count)?;
        if options.local_apic {
            set_local_apic_emulation(handle)?;
        }
        // One property write, so every wanted bit goes in together; a second
        // `WHvSetPartitionProperty(ExtendedVmExits)` would clear the first.
        if options.cpuid_policy {
            set_extended_vm_exits(handle, EXTENDED_VM_EXITS_X64_CPUID)?;
            set_cpuid_exit_list(handle)?;
        }
        // SAFETY: `handle` is a live partition with all properties set.
        unsafe { WHvSetupPartition(handle) }.map_err(|e| whp_err("WHvSetupPartition", e))?;

        map_guest_memory(handle, partition.memory())?;

        let mut vcpus = Vec::with_capacity(cfg.vcpu_count as usize);
        for index in 0..cfg.vcpu_count {
            vcpus.push(WhpVcpu::new(Arc::clone(&partition), index)?);
        }
        Ok(Self { partition, vcpus })
    }

    pub fn memory(&self) -> &GuestMem {
        self.partition.memory()
    }

    pub fn options(&self) -> WhpOptions {
        self.partition.options()
    }

    /// The [`crate::hv::InterruptDelivery`] implementation for this partition —
    /// what `machine_x86::irqchip::UserspaceIrqChip` is built on.
    ///
    /// Only meaningful with [`WhpOptions::local_apic`]; without it every
    /// `WHvRequestInterrupt` fails, so the machine's IOAPIC would decode
    /// correctly and then have nowhere to deliver.
    pub fn interrupt_delivery(&self) -> Arc<WhpInterruptDelivery> {
        Arc::new(WhpInterruptDelivery::new(
            Arc::clone(&self.partition),
            Arc::clone(self.partition.halt_gate()),
        ))
    }

    /// How many virtual processors this partition has.
    pub fn vcpu_count(&self) -> u32 {
        self.partition.vcpu_count
    }

    /// A one-line dump of VP `index`'s execution state, for the failure message of
    /// a guest that stopped making progress.
    ///
    /// The WHP peer of the boot harness's "host device state at stall": an
    /// application processor that never reports alive looks identical in a kernel
    /// log whether it is spinning in the real-mode trampoline, sitting in long mode
    /// with a wedged APIC, or has never executed an instruction at all. RIP
    /// together with `CS` and the mode says which — this is the call that settled
    /// the SMP question (see the type's SMP notes).
    pub fn processor_summary(&self, index: u32) -> Result<String, VmmError> {
        use windows::Win32::System::Hypervisor::{
            WHvX64RegisterCr0, WHvX64RegisterCr3, WHvX64RegisterCr4, WHvX64RegisterCs,
            WHvX64RegisterEfer, WHvX64RegisterRip, WHvX64RegisterRsp,
        };

        let names = [
            WHvX64RegisterRip,
            WHvX64RegisterRsp,
            WHvX64RegisterCs,
            WHvX64RegisterCr0,
            WHvX64RegisterCr3,
            WHvX64RegisterCr4,
            WHvX64RegisterEfer,
        ];
        let mut values = Aligned16([zeroed_value(); 7]);
        let count = u32::try_from(names.len()).unwrap_or(u32::MAX);
        get_regs_raw(self.partition.handle(), index, &names, &mut values.0, count)?;
        // SAFETY: the names above select, in order, six 64-bit registers (`Reg64`
        // arm) and one segment register (`Segment` arm) — which is which is fixed
        // by the array, not by anything the guest controls.
        let (rip, rsp, cs, cr0, cr3, cr4, efer) = unsafe {
            (
                values.0[0].Reg64,
                values.0[1].Reg64,
                seg_from_whp(&values.0[2].Segment),
                values.0[3].Reg64,
                values.0[4].Reg64,
                values.0[5].Reg64,
                values.0[6].Reg64,
            )
        };
        Ok(format!(
            "vp{index}: rip={rip:#x} rsp={rsp:#x} cs={{sel={:#x} base={:#x} l={} db={}}} \
             cr0={cr0:#x} cr3={cr3:#x} cr4={cr4:#x} efer={efer:#x} mode={}",
            cs.selector,
            cs.base,
            cs.l,
            cs.db,
            if efer & (1 << 10) != 0 {
                "long"
            } else if cr0 & 1 != 0 {
                "protected"
            } else {
                "real"
            }
        ))
    }

    /// The gate halted vCPUs wait on, for a host-side wake-up that is not an
    /// interrupt (a stop request).
    pub fn halt_gate(&self) -> Arc<HaltGate> {
        Arc::clone(self.partition.halt_gate())
    }

    /// Maps a firmware image at `guest_addr` as its own GPA range, outside guest
    /// RAM (backlog UEFI-1801, the WHP peer of [`crate::Vm::map_rom`]). The image
    /// is copied into a fresh anonymous mapping, so the caller's buffer is free
    /// afterwards.
    ///
    /// Mapped **read + execute, no write**: a guest write faults out as a
    /// `MemoryAccess` exit, goes through the instruction emulator to
    /// `ExitHandler::mmio_write`, and the machine bus drops it — the same
    /// "firmware variable writes are discarded" semantics the KVM path gets from
    /// `KVM_MEM_READONLY`.
    ///
    /// `guest_addr + len` may exceed the RAM size; that is the point — a
    /// reset-vector firmware ROM lives at the top of the 32-bit address space,
    /// far above any guest RAM region.
    pub fn map_rom(&mut self, guest_addr: u64, image: &[u8]) -> Result<(), VmmError> {
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
        let mut roms = self
            .partition
            .roms
            .lock()
            .map_err(|_| VmmError::GuestMemory("the ROM table lock is poisoned".into()))?;
        if roms.iter().any(|r| {
            let new_end = guest_addr.saturating_add(image.len() as u64);
            let end = r.guest_addr.saturating_add(r.len);
            guest_addr < end && r.guest_addr < new_end
        }) {
            return Err(VmmError::GuestMemory(format!(
                "a firmware ROM is already mapped over {guest_addr:#x}"
            )));
        }

        let mapping = MmapRegion::<()>::new(image.len())
            .map_err(|e| VmmError::GuestMemory(format!("cannot map firmware ROM: {e}")))?;
        // SAFETY: `MmapRegion::new(len)` returned a private anonymous mapping of
        // exactly `len` bytes that nothing else references yet, and
        // `image.len() == len`. Source and destination cannot overlap: the
        // destination is a brand-new mapping.
        unsafe {
            std::ptr::copy_nonoverlapping(image.as_ptr(), mapping.as_ptr(), image.len());
        }

        // SAFETY: `mapping` is a live, page-aligned host allocation of exactly
        // `image.len()` bytes, owned by the partition's ROM table from the push
        // below onward and therefore alive until after `WHvDeletePartition`
        // (`Partition::drop` deletes the partition before its fields drop). The
        // GPA range does not overlap another ROM (checked above); overlapping
        // guest RAM is the caller's contract, as it is for the KVM slots.
        unsafe {
            WHvMapGpaRange(
                self.partition.handle,
                mapping.as_ptr().cast::<core::ffi::c_void>(),
                guest_addr,
                image.len() as u64,
                WHvMapGpaRangeFlagRead | WHvMapGpaRangeFlagExecute,
            )
        }
        .map_err(|e| whp_err("WHvMapGpaRange(rom)", e))?;

        roms.push(WhpRom {
            _mapping: mapping,
            guest_addr,
            len: image.len() as u64,
        });
        tracing::info!(
            addr = format_args!("{guest_addr:#x}"),
            len = image.len(),
            "mapped firmware ROM (read+execute)"
        );
        Ok(())
    }

    /// Allocates a shared-memory window of `len` bytes (EPIC 20, VEN-2001).
    ///
    /// The WHP twin of [`crate::Vm::create_shm_window`], and simpler: WHP
    /// addresses a mapping by its GPA range rather than by a slot number, so
    /// there is nothing to reserve up front and nothing to leak if the window
    /// is never placed.
    ///
    /// The window comes back **unplaced** — a BAR decodes nothing until the
    /// driver enables memory space — and is mapped read/write with **no
    /// execute**, which is the strongest statement either host can make about
    /// host memory that is not guest RAM.
    pub fn create_shm_window(&self, len: u64) -> Result<Arc<SharedWindow>, VmmError> {
        let mapper = Arc::new(WhpGpaMapper {
            partition: Arc::clone(&self.partition),
        });
        let window = SharedWindow::new(len, mapper)?;
        tracing::info!(len, "allocated a shared-memory window for this partition");
        Ok(Arc::new(window))
    }

    /// Moves the vCPUs out for running. Each vCPU is owned by exactly one
    /// thread, because WHP allows only one concurrent
    /// `WHvRunVirtualProcessor` per VP index.
    pub fn take_vcpus(&mut self) -> Vec<WhpVcpu> {
        std::mem::take(&mut self.vcpus)
    }
}

/// WHP's half of a shared-memory window (EPIC 20, VEN-2001).
///
/// Holds an `Arc` on the partition rather than the bare handle, so the window
/// cannot outlive the partition it maps into: `WHvUnmapGpaRange` on a deleted
/// partition is a use-after-free of a kernel object, and a `SharedWindow` is
/// dropped by whoever holds it, not by the partition.
struct WhpGpaMapper {
    partition: Arc<Partition>,
}

impl crate::shm::GpaMapper for WhpGpaMapper {
    fn map(&self, gpa: u64, region: &crate::shm::HostShmRegion) -> Result<(), HvError> {
        // SAFETY: `region` owns a live, page-aligned `VirtualAlloc` allocation
        // of exactly `region.len()` bytes. The `SharedWindow` that owns it
        // unmaps this range before dropping the pages, and this mapper holds an
        // `Arc` on the partition, so the handle is live for the whole call and
        // WHP never keeps a pointer into freed memory. Read+write and *not*
        // execute: this is data the guest maps, never code the host offers it.
        unsafe {
            WHvMapGpaRange(
                self.partition.handle,
                region.host_addr() as *mut core::ffi::c_void,
                gpa,
                region.len(),
                WHvMapGpaRangeFlagRead | WHvMapGpaRangeFlagWrite,
            )
        }
        .map_err(|e| {
            HvError::Registers(format!(
                "WHvMapGpaRange(shm) at {gpa:#x}: {e} ({:#010x})",
                e.code().0 as u32
            ))
        })
    }

    fn unmap(&self, gpa: u64, len: u64) -> Result<(), HvError> {
        // SAFETY: `gpa`/`len` name a range this mapper mapped and has not
        // unmapped since (`SharedWindow` tracks exactly one live placement),
        // and the partition handle is kept alive by the `Arc`.
        unsafe { WHvUnmapGpaRange(self.partition.handle, gpa, len) }.map_err(|e| {
            HvError::Registers(format!(
                "WHvUnmapGpaRange(shm) at {gpa:#x}: {e} ({:#010x})",
                e.code().0 as u32
            ))
        })
    }

    fn backend(&self) -> &'static str {
        "whp"
    }
}

/// Sets one fixed-size partition property.
///
/// # Safety
///
/// `T` must be the arm of `WHV_PARTITION_PROPERTY` that WHP documents for
/// `code`, and `handle` must name a partition that has not been through
/// `WHvSetupPartition` yet.
unsafe fn set_property<T: Copy>(
    handle: WHV_PARTITION_HANDLE,
    code: windows::Win32::System::Hypervisor::WHV_PARTITION_PROPERTY_CODE,
    call: &'static str,
    value: &T,
) -> Result<(), VmmError> {
    let size = u32::try_from(size_of::<T>()).unwrap_or(u32::MAX);
    // SAFETY: `value` points at a live `T` of exactly `size` bytes which WHP only
    // reads, and the caller promised `T` matches `code`.
    unsafe { WHvSetPartitionProperty(handle, code, (value as *const T).cast(), size) }
        .map_err(|e| whp_err(call, e))
}

fn set_processor_count(handle: WHV_PARTITION_HANDLE, count: u32) -> Result<(), VmmError> {
    // SAFETY: `WHvPartitionPropertyCodeProcessorCount` is the `ProcessorCount`
    // arm of `WHV_PARTITION_PROPERTY`, i.e. a `u32`, and this runs before
    // `WHvSetupPartition`.
    unsafe {
        set_property(
            handle,
            WHvPartitionPropertyCodeProcessorCount,
            "WHvSetPartitionProperty(ProcessorCount)",
            &count,
        )
    }
}

/// Turns on WHP's in-hypervisor local APIC.
///
/// xAPIC rather than x2APIC: the machine publishes an MP table and an MADT with
/// 8-bit LAPIC ids and a memory-mapped LAPIC at
/// `machine_x86::layout::LAPIC_ADDR`, which is the xAPIC contract. x2APIC would
/// need the MADT to carry x2APIC entries and is only worth it above 255 CPUs.
fn set_local_apic_emulation(handle: WHV_PARTITION_HANDLE) -> Result<(), VmmError> {
    // SAFETY: `WHvPartitionPropertyCodeLocalApicEmulationMode` is the
    // `LocalApicEmulationMode` arm, a 4-byte enum over `i32`, and this runs
    // before `WHvSetupPartition`.
    unsafe {
        set_property(
            handle,
            WHvPartitionPropertyCodeLocalApicEmulationMode,
            "WHvSetPartitionProperty(LocalApicEmulationMode)",
            &WHvX64LocalApicEmulationModeXApic,
        )
    }
}

/// `WHV_EXTENDED_VM_EXITS` bit 0 (`X64CpuidExit`), from WinHvPlatformDefs.h.
///
/// The list in [`crate::whp::CPUID_EXIT_LEAVES`] says *which* leaves exit; this
/// bit is what makes any of them exit at all.
const EXTENDED_VM_EXITS_X64_CPUID: u64 = 1 << 0;

fn set_extended_vm_exits(handle: WHV_PARTITION_HANDLE, bits: u64) -> Result<(), VmmError> {
    // SAFETY: `WHvPartitionPropertyCodeExtendedVmExits` is the `ExtendedVmExits`
    // arm, a union of a `u64` bitfield and `AsUINT64`; a `u64` is exactly it, and
    // this runs before `WHvSetupPartition`.
    unsafe {
        set_property(
            handle,
            WHvPartitionPropertyCodeExtendedVmExits,
            "WHvSetPartitionProperty(ExtendedVmExits)",
            &bits,
        )
    }
}

fn set_cpuid_exit_list(handle: WHV_PARTITION_HANDLE) -> Result<(), VmmError> {
    // The CpuidExitList arm is a *variable-length* array of leaf numbers, so the
    // size is the whole array rather than `size_of` one element — the one
    // property here that cannot go through `set_property`.
    let leaves = crate::whp::CPUID_EXIT_LEAVES;
    let size = u32::try_from(size_of_val(&leaves)).unwrap_or(u32::MAX);
    // SAFETY: `leaves` is a live `[u32; N]` of exactly `size` bytes which WHP
    // only reads, `WHvPartitionPropertyCodeCpuidExitList` is documented to take
    // an array of leaf numbers, and this runs before `WHvSetupPartition`.
    unsafe {
        WHvSetPartitionProperty(
            handle,
            WHvPartitionPropertyCodeCpuidExitList,
            leaves.as_ptr().cast(),
            size,
        )
    }
    .map_err(|e| whp_err("WHvSetPartitionProperty(CpuidExitList)", e))
}

/// Maps every guest memory region read/write/execute. One region today (see
/// [`create_guest_memory`]), but the loop mirrors the KVM memslot loop so
/// adding the MMIO hole later is symmetric.
fn map_guest_memory(handle: WHV_PARTITION_HANDLE, memory: &GuestMem) -> Result<(), VmmError> {
    for region in memory.iter() {
        let host_addr = region
            .get_host_address(MemoryRegionAddress(0))
            .map_err(|e| VmmError::GuestMemory(e.to_string()))?;
        let flags = WHvMapGpaRangeFlagRead | WHvMapGpaRangeFlagWrite | WHvMapGpaRangeFlagExecute;
        // SAFETY: `host_addr` is the page-aligned base of a live
        // `VirtualAlloc` region of exactly `region.len()` bytes owned by
        // `memory`, which is dropped only after `WHvDeletePartition` (see
        // `Partition`'s docs). The GPA range is the region's own, so no two
        // regions overlap.
        unsafe {
            WHvMapGpaRange(
                handle,
                host_addr.cast::<core::ffi::c_void>(),
                region.start_addr().raw_value(),
                region.len(),
                flags,
            )
        }
        .map_err(|e| whp_err("WHvMapGpaRange", e))?;
    }
    Ok(())
}

/// Creates one virtual processor. Kept here so partition-scoped WHP calls stay
/// in one file.
pub(super) fn create_virtual_processor(
    handle: WHV_PARTITION_HANDLE,
    index: u32,
) -> Result<(), VmmError> {
    // SAFETY: `handle` is a live, set-up partition and `index` is below the
    // processor count set before `WHvSetupPartition`. Flags must be 0.
    unsafe { WHvCreateVirtualProcessor(handle, index, 0) }
        .map_err(|e| whp_err("WHvCreateVirtualProcessor", e))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The hint must name the feature so `entangled doctor` output is
    /// actionable without consulting docs.
    #[test]
    fn enable_hint_names_the_feature() {
        assert!(WHP_ENABLE_HINT.contains("Windows Hypervisor Platform"));
        assert!(WHP_ENABLE_HINT.contains("HypervisorPlatform"));
    }

    /// `probe` must never panic and must not error just because the feature is
    /// off — that is the whole point of it existing next to `open`.
    #[test]
    fn probe_reports_rather_than_fails() {
        match WhpHypervisor::probe() {
            Ok(caps) => {
                if caps.hypervisor_present {
                    assert!(caps.is_runnable());
                } else {
                    eprintln!("WHP not present on this host — {WHP_ENABLE_HINT}");
                    assert_eq!(caps.processor_vendor, None);
                }
            }
            Err(e) => eprintln!("skipping: WHvGetCapability unavailable: {e}"),
        }
    }
}
