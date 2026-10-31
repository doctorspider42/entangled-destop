//! The Hyper-V reference clock, served from userspace (ADR-0008 §2).
//!
//! A Windows guest without a clock it trusts reads the ACPI PM timer for
//! every `QueryPerformanceCounter` — thousands of port exits a second on an
//! idle desktop. Hyper-V's answer is the reference TSC page: a page of guest
//! memory holding a scale and an offset that turn the guest's own `rdtsc`
//! into 100 ns units, so reading the clock never leaves the guest.
//!
//! WHP offers the real thing only where the host grants synthetic processor
//! features to WHP partitions, which a Windows 10 host does not
//! (`WHV_E_UNKNOWN_CAPABILITY`). The clock half of it needs nothing from the
//! hypervisor, though: it is three CPUID leaves and five MSRs, and WHP hands
//! both to the host once CPUID and MSR exits are on. This module is that
//! half, what KVM calls `hv_time`:
//!
//! * `0x4000_0000`–`0x4000_0006` say "Microsoft Hv", `Hv#1`, and grant only
//!   the partition reference counter, the reference TSC page, the frequency
//!   MSRs and the hypercall MSRs (Windows will not take the clock without
//!   the interface);
//! * `HV_X64_MSR_GUEST_OS_ID` and `HV_X64_MSR_HYPERCALL` are kept, and the
//!   hypercall page they enable is filled with `mov rax, 2; ret` — every
//!   hypercall answers `HV_STATUS_INVALID_HYPERCALL_CODE` without an exit,
//!   which is the truth: none are offered;
//! * `HV_X64_MSR_REFERENCE_TSC` writes the page, `HV_X64_MSR_TIME_REF_COUNT`
//!   reads the same clock from the guest TSC, and the frequency MSRs report
//!   the TSC and APIC timer rates WHP gives the partition.
//!
//! The reference time is the guest TSC scaled to 100 ns, with no offset: it
//! starts when the partition's TSC does, and it is the same function of the
//! TSC whether read through the page or the MSR.

use std::sync::atomic::{AtomicU64, Ordering};

use vm_memory::{Bytes, GuestAddress};

use super::cpuid::{CpuidResult, GuestClocks};
use crate::GuestMem;

pub const HV_X64_MSR_GUEST_OS_ID: u32 = 0x4000_0000;
pub const HV_X64_MSR_HYPERCALL: u32 = 0x4000_0001;
pub const HV_X64_MSR_TIME_REF_COUNT: u32 = 0x4000_0020;
pub const HV_X64_MSR_REFERENCE_TSC: u32 = 0x4000_0021;
pub const HV_X64_MSR_TSC_FREQUENCY: u32 = 0x4000_0022;
pub const HV_X64_MSR_APIC_FREQUENCY: u32 = 0x4000_0023;

/// The hypercall page: `mov rax, 2` (`HV_STATUS_INVALID_HYPERCALL_CODE`),
/// `ret`.
const HYPERCALL_PAGE: [u8; 8] = [0x48, 0xc7, 0xc0, 0x02, 0x00, 0x00, 0x00, 0xc3];

/// The enable bit of the hypercall and reference TSC MSRs.
const MSR_ENABLE: u64 = 1;
/// The page-number half of those MSRs.
const PAGE_MASK: u64 = !0xfff;

/// The emulated clock of one partition, shared by its vCPUs.
pub struct HvClock {
    clocks: GuestClocks,
    guest_os_id: AtomicU64,
    hypercall: AtomicU64,
    reference_tsc: AtomicU64,
}

impl HvClock {
    /// `None` when the partition's TSC rate is unknown, which the clock is
    /// nothing without.
    pub fn new(clocks: Option<GuestClocks>) -> Option<Self> {
        let clocks = clocks.filter(|c| c.tsc_hz > 0)?;
        Some(Self {
            clocks,
            guest_os_id: AtomicU64::new(0),
            hypercall: AtomicU64::new(0),
            reference_tsc: AtomicU64::new(0),
        })
    }

    /// Power-on: no guest registered, no page enabled (ADR-0005).
    pub fn reset(&self) {
        for msr in [&self.guest_os_id, &self.hypercall, &self.reference_tsc] {
            msr.store(0, Ordering::Release);
        }
    }

    /// The hypervisor leaves, or `None` for a leaf this interface does not
    /// own.
    pub fn cpuid(&self, leaf: u32) -> Option<CpuidResult> {
        let [eax, ebx, ecx, edx] = crate::hv::hyperv::clock_leaf(leaf)?;
        Some(CpuidResult { eax, ebx, ecx, edx })
    }

    /// `rdmsr`: `Some(value)` for an MSR of this interface. `tsc` reads the
    /// vCPU's TSC, and is only called for the reference counter.
    pub fn read(&self, msr: u32, tsc: impl FnOnce() -> Option<u64>) -> Option<u64> {
        Some(match msr {
            HV_X64_MSR_GUEST_OS_ID => self.guest_os_id.load(Ordering::Acquire),
            HV_X64_MSR_HYPERCALL => self.hypercall.load(Ordering::Acquire),
            HV_X64_MSR_REFERENCE_TSC => self.reference_tsc.load(Ordering::Acquire),
            HV_X64_MSR_TIME_REF_COUNT => reference_time(tsc().unwrap_or(0), self.scale()),
            HV_X64_MSR_TSC_FREQUENCY => self.clocks.tsc_hz,
            HV_X64_MSR_APIC_FREQUENCY => self.clocks.apic_timer_hz,
            _ => return None,
        })
    }

    /// `wrmsr`: `true` when the MSR is this interface's (a write to a
    /// read-only one is dropped, as Hyper-V does).
    pub fn write(&self, msr: u32, value: u64, mem: &GuestMem) -> bool {
        match msr {
            HV_X64_MSR_GUEST_OS_ID => {
                self.guest_os_id.store(value, Ordering::Release);
                // Clearing the guest id disables the hypercall page.
                if value == 0 {
                    self.hypercall.fetch_and(!MSR_ENABLE, Ordering::AcqRel);
                }
            }
            HV_X64_MSR_HYPERCALL => {
                // The page can only be enabled once the guest said who it is.
                let value = if self.guest_os_id.load(Ordering::Acquire) == 0 {
                    value & !MSR_ENABLE
                } else {
                    value
                };
                self.hypercall.store(value, Ordering::Release);
                if value & MSR_ENABLE != 0 {
                    write_page(mem, value & PAGE_MASK, &HYPERCALL_PAGE, "hypercall");
                }
            }
            HV_X64_MSR_REFERENCE_TSC => {
                self.reference_tsc.store(value, Ordering::Release);
                if value & MSR_ENABLE != 0 {
                    write_page(
                        mem,
                        value & PAGE_MASK,
                        &reference_tsc_page(self.scale()),
                        "reference TSC",
                    );
                }
            }
            HV_X64_MSR_TIME_REF_COUNT | HV_X64_MSR_TSC_FREQUENCY | HV_X64_MSR_APIC_FREQUENCY => {}
            _ => return false,
        }
        true
    }

    fn scale(&self) -> u64 {
        tsc_scale(self.clocks.tsc_hz)
    }
}

/// The 64.64 fixed-point factor from TSC ticks to 100 ns units.
fn tsc_scale(tsc_hz: u64) -> u64 {
    u64::try_from((10_000_000u128 << 64) / u128::from(tsc_hz.max(1))).unwrap_or(u64::MAX)
}

/// The reference time for a TSC value: what the guest computes from the page.
fn reference_time(tsc: u64, scale: u64) -> u64 {
    ((u128::from(tsc) * u128::from(scale)) >> 64) as u64
}

/// `HV_REFERENCE_TSC_PAGE`'s first 24 bytes: a valid (non-zero) sequence, the
/// scale, a zero offset.
fn reference_tsc_page(scale: u64) -> [u8; 24] {
    let mut page = [0u8; 24];
    page[0..4].copy_from_slice(&1u32.to_le_bytes());
    page[8..16].copy_from_slice(&scale.to_le_bytes());
    page
}

/// Writes `bytes` at the start of the guest page `gpa`. A page outside RAM is
/// the guest's mistake; it is logged and the enable stays recorded, as
/// Hyper-V would leave it.
fn write_page(mem: &GuestMem, gpa: u64, bytes: &[u8], what: &str) {
    if let Err(error) = mem.write_slice(bytes, GuestAddress(gpa)) {
        tracing::warn!(
            gpa = format_args!("{gpa:#x}"),
            %error,
            "the guest put the {what} page outside its RAM"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CLOCKS: GuestClocks = GuestClocks {
        tsc_hz: 3_000_000_000,
        apic_timer_hz: 100_000_000,
        intel: false,
    };

    fn memory() -> GuestMem {
        GuestMem::from_ranges(&[(GuestAddress(0), 0x10_000)]).unwrap()
    }

    #[test]
    fn the_leaves_say_hyper_v_with_only_the_clock() {
        let clock = HvClock::new(Some(CLOCKS)).unwrap();
        let vendor = clock.cpuid(0x4000_0000).unwrap();
        let name: Vec<u8> = [vendor.ebx, vendor.ecx, vendor.edx]
            .iter()
            .flat_map(|r| r.to_le_bytes())
            .collect();
        assert_eq!(name, b"Microsoft Hv");
        assert_eq!(vendor.eax, crate::hv::hyperv::MAX_LEAF);
        assert_eq!(
            &clock.cpuid(0x4000_0001).unwrap().eax.to_le_bytes(),
            b"Hv#1"
        );
        let features = clock.cpuid(0x4000_0003).unwrap();
        assert_eq!(features.eax, 0xa22);
        assert_eq!(features.ebx, 0, "no partition creation or other privileges");
        assert_eq!(
            clock.cpuid(0x4000_0004).unwrap().eax,
            0,
            "no recommendations"
        );
        assert!(clock.cpuid(0x4000_0007).is_none());
        assert!(clock.cpuid(0x4000_0010).is_none());
    }

    #[test]
    fn the_reference_tsc_page_and_the_counter_tell_the_same_time() {
        let mem = memory();
        let clock = HvClock::new(Some(CLOCKS)).unwrap();
        assert!(clock.write(HV_X64_MSR_REFERENCE_TSC, 0x3000 | 1, &mem));
        let mut page = [0u8; 24];
        mem.read_slice(&mut page, GuestAddress(0x3000)).unwrap();
        assert_eq!(u32::from_le_bytes(page[0..4].try_into().unwrap()), 1);
        let scale = u64::from_le_bytes(page[8..16].try_into().unwrap());
        assert_eq!(u64::from_le_bytes(page[16..24].try_into().unwrap()), 0);
        // One second of TSC is 10^7 units of 100 ns, both ways.
        let tsc = 3_000_000_000u64 * 7;
        let via_page = ((u128::from(tsc) * u128::from(scale)) >> 64) as u64;
        let via_msr = clock.read(HV_X64_MSR_TIME_REF_COUNT, || Some(tsc)).unwrap();
        assert_eq!(via_page, via_msr);
        assert!((69_999_999..=70_000_000).contains(&via_msr), "{via_msr}");
        assert_eq!(clock.read(HV_X64_MSR_REFERENCE_TSC, || None), Some(0x3001));
        assert_eq!(
            clock.read(HV_X64_MSR_TSC_FREQUENCY, || None),
            Some(3_000_000_000)
        );
        assert_eq!(clock.read(0x4000_0002, || None), None);
    }

    #[test]
    fn the_hypercall_page_needs_a_guest_id_and_fails_every_call() {
        let mem = memory();
        let clock = HvClock::new(Some(CLOCKS)).unwrap();
        clock.write(HV_X64_MSR_HYPERCALL, 0x5000 | 1, &mem);
        assert_eq!(clock.read(HV_X64_MSR_HYPERCALL, || None), Some(0x5000));
        let mut code = [0u8; 8];
        mem.read_slice(&mut code, GuestAddress(0x5000)).unwrap();
        assert_eq!(code, [0; 8], "not enabled without a guest id");

        clock.write(HV_X64_MSR_GUEST_OS_ID, 0x8100_0a00_0000_4a61, &mem);
        clock.write(HV_X64_MSR_HYPERCALL, 0x5000 | 1, &mem);
        mem.read_slice(&mut code, GuestAddress(0x5000)).unwrap();
        assert_eq!(code, HYPERCALL_PAGE);
        clock.write(HV_X64_MSR_GUEST_OS_ID, 0, &mem);
        assert_eq!(clock.read(HV_X64_MSR_HYPERCALL, || None), Some(0x5000));

        clock.reset();
        assert_eq!(clock.read(HV_X64_MSR_GUEST_OS_ID, || None), Some(0));
    }

    #[test]
    fn a_page_outside_ram_is_not_a_crash() {
        let mem = memory();
        let clock = HvClock::new(Some(CLOCKS)).unwrap();
        assert!(clock.write(HV_X64_MSR_REFERENCE_TSC, 0xffff_f000 | 1, &mem));
        assert!(!clock.write(0x4000_0002, 1, &mem));
        assert!(HvClock::new(None).is_none());
    }
}
