//! WHP partition lifecycle and guest memory mapping (backlog WHP-1702).

use std::sync::Arc;

use vm_memory::{Address, GuestMemory, GuestMemoryRegion, MemoryRegionAddress};
use windows::Win32::System::Hypervisor::{
    WHvCapabilityCodeHypervisorPresent, WHvCapabilityCodeProcessorVendor, WHvCreatePartition,
    WHvCreateVirtualProcessor, WHvDeletePartition, WHvGetCapability, WHvMapGpaRange,
    WHvMapGpaRangeFlagExecute, WHvMapGpaRangeFlagRead, WHvMapGpaRangeFlagWrite,
    WHvPartitionPropertyCodeCpuidExitList, WHvPartitionPropertyCodeExtendedVmExits,
    WHvPartitionPropertyCodeLocalApicEmulationMode, WHvPartitionPropertyCodeProcessorCount,
    WHvProcessorVendorAmd, WHvProcessorVendorHygon, WHvProcessorVendorIntel,
    WHvSetPartitionProperty, WHvSetupPartition, WHvX64LocalApicEmulationModeXApic,
    WHV_PARTITION_HANDLE, WHV_PROCESSOR_VENDOR,
};

use crate::hv::MachineConfig;
use crate::memory::{create_guest_memory, GuestMem};
use crate::whp::interrupt::{HaltGate, WhpInterruptDelivery};
use crate::whp::vcpu::WhpVcpu;
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
        });

        set_processor_count(handle, cfg.vcpu_count)?;
        if options.local_apic {
            set_local_apic_emulation(handle)?;
        }
        if options.cpuid_policy {
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

    /// The gate halted vCPUs wait on, for a host-side wake-up that is not an
    /// interrupt (a stop request).
    pub fn halt_gate(&self) -> Arc<HaltGate> {
        Arc::clone(self.partition.halt_gate())
    }

    /// Moves the vCPUs out for running. Each vCPU is owned by exactly one
    /// thread, because WHP allows only one concurrent
    /// `WHvRunVirtualProcessor` per VP index.
    pub fn take_vcpus(&mut self) -> Vec<WhpVcpu> {
        std::mem::take(&mut self.vcpus)
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
/// bit is what makes any of them exit at all. Nothing else in that bitfield is
/// wanted: MSR exits would mean re-implementing WHP's MSR policy, and the APIC
/// EOI exit would only be needed for a level-triggered IOAPIC pin, which this
/// machine does not have (`machine_x86::irqchip::ioapic`).
const EXTENDED_VM_EXITS_X64_CPUID: u64 = 1 << 0;

fn set_cpuid_exit_list(handle: WHV_PARTITION_HANDLE) -> Result<(), VmmError> {
    // SAFETY: `WHvPartitionPropertyCodeExtendedVmExits` is the `ExtendedVmExits`
    // arm, a union of a `u64` bitfield and `AsUINT64`; a `u64` is exactly it.
    unsafe {
        set_property(
            handle,
            WHvPartitionPropertyCodeExtendedVmExits,
            "WHvSetPartitionProperty(ExtendedVmExits)",
            &EXTENDED_VM_EXITS_X64_CPUID,
        )
    }?;

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
