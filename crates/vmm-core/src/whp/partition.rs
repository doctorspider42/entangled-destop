//! WHP partition lifecycle and guest memory mapping (backlog WHP-1702).

use std::sync::Arc;

use vm_memory::{Address, GuestMemory, GuestMemoryRegion, MemoryRegionAddress};
use windows::Win32::System::Hypervisor::{
    WHvCapabilityCodeHypervisorPresent, WHvCapabilityCodeProcessorVendor, WHvCreatePartition,
    WHvCreateVirtualProcessor, WHvDeletePartition, WHvGetCapability, WHvMapGpaRange,
    WHvMapGpaRangeFlagExecute, WHvMapGpaRangeFlagRead, WHvMapGpaRangeFlagWrite,
    WHvPartitionPropertyCodeProcessorCount, WHvProcessorVendorAmd, WHvProcessorVendorHygon,
    WHvProcessorVendorIntel, WHvSetPartitionProperty, WHvSetupPartition, WHV_PARTITION_HANDLE,
    WHV_PROCESSOR_VENDOR,
};

use crate::hv::MachineConfig;
use crate::memory::{create_guest_memory, GuestMem};
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

/// Owns a WHP partition handle **and the guest RAM mapped into it**.
///
/// Bundling the two is what makes teardown sound: `Drop::drop` runs before the
/// struct's fields are dropped, so `WHvDeletePartition` (which tears down every
/// GPA mapping) always happens *before* the `VirtualAlloc` backing store is
/// released. Virtual processors hold an `Arc<Partition>`, so the partition
/// cannot be deleted while a vCPU handle still exists either.
pub(super) struct Partition {
    handle: WHV_PARTITION_HANDLE,
    memory: GuestMem,
}

impl Partition {
    pub(super) fn handle(&self) -> WHV_PARTITION_HANDLE {
        self.handle
    }

    pub(super) fn memory(&self) -> &GuestMem {
        &self.memory
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
    /// Creates the partition, sets the processor count, finalises it with
    /// `WHvSetupPartition`, maps guest RAM and creates the vCPUs.
    ///
    /// Order matters: partition properties may only be set *before*
    /// `WHvSetupPartition`, and GPA ranges and virtual processors only *after*.
    pub fn new(_hv: &WhpHypervisor, cfg: &MachineConfig) -> Result<Self, VmmError> {
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
        let partition = Arc::new(Partition { handle, memory });

        set_processor_count(handle, cfg.vcpu_count)?;
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

    /// Moves the vCPUs out for running. Each vCPU is owned by exactly one
    /// thread, because WHP allows only one concurrent
    /// `WHvRunVirtualProcessor` per VP index.
    pub fn take_vcpus(&mut self) -> Vec<WhpVcpu> {
        std::mem::take(&mut self.vcpus)
    }
}

fn set_processor_count(handle: WHV_PARTITION_HANDLE, count: u32) -> Result<(), VmmError> {
    // `WHV_PARTITION_PROPERTY` is a union; for `ProcessorCount` WHP reads only
    // the leading `u32`, and that is the size it expects to be told.
    //
    // SAFETY: `&count` points at a live `u32` of exactly the size passed, and
    // `WHvPartitionPropertyCodeProcessorCount` is the `ProcessorCount` arm of
    // `WHV_PARTITION_PROPERTY`, i.e. a `u32`.
    unsafe {
        WHvSetPartitionProperty(
            handle,
            WHvPartitionPropertyCodeProcessorCount,
            (&raw const count).cast(),
            u32::try_from(size_of::<u32>()).unwrap_or(4),
        )
    }
    .map_err(|e| whp_err("WHvSetPartitionProperty(ProcessorCount)", e))
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
