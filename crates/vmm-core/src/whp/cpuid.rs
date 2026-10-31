//! CPUID policy for a WHP guest (backlog WHP-1703).
//!
//! # Why WHP needs an exit and KVM does not
//!
//! On KVM, `crate::Vcpu::new` reads `KVM_GET_SUPPORTED_CPUID`, edits the entries
//! it cares about and installs the whole table with `KVM_SET_CPUID2`. WHP has
//! `WHvPartitionPropertyCodeCpuidResultList`, which looks like the same thing but
//! is not: it is a list of *complete* results, and there is no API that tells you
//! what WHP would otherwise have reported. Installing one means inventing every
//! bit of a leaf from scratch — including the feature bits WHP masks for its own
//! reasons — which is how a guest ends up being told it has features the
//! hypervisor will not honour.
//!
//! `WHvPartitionPropertyCodeCpuidExitList` plus
//! `WHV_EXTENDED_VM_EXITS.X64CpuidExit` is the right tool: the guest's `cpuid`
//! traps, and the exit context carries `DefaultResultRax..Rdx` — exactly what WHP
//! would have returned. The policy below is then a *diff* on top, which is what
//! the KVM path is too. That is the finding: use `CpuidExitList`, not
//! `CpuidResultList`, for anything that modifies rather than replaces a leaf.
//!
//! # The policy, leaf by leaf
//!
//! It mirrors `crate::Vcpu::new` deliberately: a guest must not be able to tell
//! the two backends apart from CPUID alone.
//!
//! | Leaf | Change | Why |
//! |---|---|---|
//! | `0x1` | `ECX[31] = 1` | "running under a hypervisor" — parity with KVM |
//! | `0x1` | `EBX[31:24] = vp_index` | the *initial APIC id*. Guest code that compares the CPUID identity against the local APIC's own id otherwise decides it is on an unknown processor; EDK2's `GetBspNumber()` asserts on it (UEFI-1802) |
//! | `0xb`, `0x1f`, `0x8000_0026` | `EDX = vp_index` | the 32-bit x2APIC id in the topology leaves, same problem |
//! | `0x8000_001e` | `EAX = vp_index` | AMD's extended APIC id, which with TOPOEXT present is the one Linux ends up trusting (`parse_8000_001e()` overwrites the leaf-1 id with it) |
//! | `0x1`, `0x8000_0008`, `0x8000_001e` | AMD only: one package of `n` single-threaded cores | see "One package, not `n`" |
//! | `0x4000_0000` | signature zeroed; `EAX` = `0x4000_0010` when the clocks are known, else 0 | **not** in the KVM policy, and the one place the two hosts legitimately differ — see below |
//! | `0x4000_0001`–`0x4000_000f` | zeroed | Hyper-V's own interface leaves, which must not show through an empty signature |
//! | `0x4000_0010` | `EAX` = TSC kHz, `EBX` = local APIC timer kHz | the generic timing leaf — see "Telling the guest its clocks" |
//! | `0x15` | Intel only: TSC/crystal ratio with the APIC timer as the crystal | see "Telling the guest its clocks" |
//!
//! # Why leaf `0x4000_0000` is zeroed
//!
//! A WHP partition runs on Hyper-V, and the hypervisor CPUID leaves can carry
//! Hyper-V's `"Microsoft Hv"` signature. A Linux guest that sees it runs
//! `ms_hyperv_init_platform()` and starts using Hyper-V synthetic MSRs and
//! enlightenments that a WHP exo-partition does not implement. Reporting an empty
//! hypervisor interface leaves `detect_hypervisor_vendor()` with no match, so the
//! guest keeps the hypervisor-present bit (parity with KVM) and takes the plain
//! architectural paths: TSC and the PIT for timekeeping, no kvmclock (there is
//! none) and no Hyper-V clocksource.
//!
//! # Telling the guest its clocks
//!
//! WHP knows both frequencies a guest otherwise has to measure: the TSC's
//! (`WHvCapabilityCodeProcessorClockFrequency`, 3 500 047 429 Hz on the Zen 1
//! host this was written on) and the local APIC timer's
//! (`WHvCapabilityCodeInterruptClockFrequency`, 200 MHz). A guest that has to
//! *measure* them does it against the 8254, through port-I/O exits of ~6 µs
//! each whose worst case under load is 20× the best — which is exactly what
//! `pit_calibrate_tsc()`'s sanity check rejects (`tsc: Unable to calibrate
//! against PIT`, ADR-0002's 2026-09-26 amendment). So the policy states them,
//! in the two places a guest looks without a paravirtual clock:
//!
//! - **`0x4000_0010`, the generic timing leaf** (VMware's proposal, read by
//!   FreeBSD and XNU for any hypervisor whose `0x4000_0000.EAX` reaches it):
//!   `EAX` TSC kHz, `EBX` APIC bus kHz. The signature stays empty, so Linux's
//!   `detect_hypervisor_vendor()` still matches nothing — Linux 7.0 reads this
//!   leaf only for VMware and ACRN, and neither is impersonated here.
//! - **`0x15`, on Intel hosts only.** Linux's `native_calibrate_tsc()` trusts it
//!   — `TSC_KNOWN_FREQ`, no PIT calibration at all — but only when the vendor is
//!   Intel, and AMD defines no such leaf. Linux also takes the crystal as the
//!   local APIC timer's clock (`lapic_timer_period = crystal_khz * 1000 / HZ`),
//!   so the crystal reported is the *APIC timer's* frequency, and the ratio is
//!   the TSC's to it — which keeps the APIC timer right rather than trading one
//!   calibration for a wrong constant. [`tsc_crystal_ratio`] picks the ratio so
//!   Linux's 32-bit `crystal_khz * ebx / eax` cannot overflow.
//!
//! On an **AMD** host neither leaf changes what Linux does: it calibrates the
//! TSC against the 8254 and falls back to the ACPI PM timer, which is accurate
//! (within ~0.2 %) and costs a boot-log line, not a panic. The panic was the
//! 8254's delivery, fixed in `machine_x86::irqchip::pit`. KVM needs none of
//! this: `KVM_GET_SUPPORTED_CPUID` carries kvmclock, which tells a Linux guest
//! its TSC frequency through `kvm_get_tsc_khz()`.
//!
//! # One package, not `n`
//!
//! WHP's defaults on an AMD host describe no topology at all: leaf `0x1` has
//! `HTT` clear and a logical-processor count of 0, `0x8000_0008.ECX` says one
//! core (`NC = 0`, `ApicIdSize = 0`), and `0x8000_001e` is zero beyond the APIC
//! id. Linux's `parse_8000_0008()` turns that into a core shift of 0, so every
//! APIC id is a package of its own — `CPU topo: Max. logical packages: 8` for
//! an 8-vCPU guest. On AMD that alone makes `unsynchronized_tsc()` true ("assume
//! multi socket systems are not synchronized"): `tsc: Marking TSC unstable due
//! to TSCs unsynchronized`, clocksource `acpi_pm`, and every `clock_gettime()`
//! in the guest a port-I/O exit to this process.
//!
//! The guest's TSCs *are* synchronized — WHP gives every VP of a partition one
//! offset on the host's TSC, and the guest's own warp check at AP bring-up
//! agrees — so the topology is what is wrong. [`PackageTopology`] describes what
//! the machine really is: `n` cores, one thread each, one package, one node. The
//! APIC ids are already `0..n`, which is exactly the core numbering this
//! encodes. Intel hosts are left alone: Linux never applies the multi-package
//! rule to Intel, and Intel describes topology through leaves `0x4`/`0xb`/`0x1f`,
//! not these.

/// Leaves this machine intercepts. Ordered low to high for readability; WHP does
/// not care about the order.
///
/// Deliberately short: every entry costs a VM exit each time the guest reads it,
/// and a guest reads CPUID a lot during boot. A leaf is here only because the
/// policy above changes something in it.
pub const CPUID_EXIT_LEAVES: [u32; 24] = [
    0x0000_0001,
    0x0000_000b,
    0x0000_0015,
    0x0000_001f,
    0x4000_0000,
    // Hyper-V's interface leaves, which an empty signature must not expose
    // now that `0x4000_0000.EAX` reaches past them. Guests read them only when
    // probing, so an exit each costs nothing in practice.
    0x4000_0001,
    0x4000_0002,
    0x4000_0003,
    0x4000_0004,
    0x4000_0005,
    0x4000_0006,
    0x4000_0007,
    0x4000_0008,
    0x4000_0009,
    0x4000_000a,
    0x4000_000b,
    0x4000_000c,
    0x4000_000d,
    0x4000_000e,
    0x4000_000f,
    0x4000_0010,
    0x8000_0008,
    0x8000_001e,
    0x8000_0026,
];

/// The generic timing leaf (see "Telling the guest its clocks").
pub const TIMING_LEAF: u32 = 0x4000_0010;

/// The clocks WHP runs this partition's TSC and local APIC timer at.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GuestClocks {
    /// TSC frequency in Hz (`WHvCapabilityCodeProcessorClockFrequency`).
    pub tsc_hz: u64,
    /// Local APIC timer input frequency in Hz
    /// (`WHvCapabilityCodeInterruptClockFrequency`).
    pub apic_timer_hz: u64,
    /// The host is an Intel part, so the guest reads leaf `0x15`.
    pub intel: bool,
}

/// `(EAX, EBX, ECX)` for leaf `0x15` — denominator, numerator, crystal Hz — with
/// the local APIC timer as the crystal, or `None` when the clocks do not fit the
/// leaf.
///
/// Linux computes `crystal_khz * EBX / EAX` in 32-bit arithmetic, so the
/// numerator is bounded by `u32::MAX / crystal_khz` (21 474 for a 200 MHz
/// crystal) and the result is truncated. The search below scores every
/// denominator by the value Linux will actually compute and keeps the closest:
/// for the host above that is 21 368 / 1 221, 10 ppm under the real TSC —
/// inside a real crystal's tolerance, and the best the leaf can say.
pub fn tsc_crystal_ratio(clocks: &GuestClocks) -> Option<(u32, u32, u32)> {
    let crystal_hz = u32::try_from(clocks.apic_timer_hz).ok()?;
    let crystal_khz = u64::from(crystal_hz / 1000);
    let tsc_khz = clocks.tsc_hz / 1000;
    if crystal_khz == 0 || tsc_khz == 0 {
        return None;
    }
    let max_numerator = u64::from(u32::MAX) / crystal_khz;
    let mut best: Option<(u64, u64, u64)> = None; // (error, den, num)
                                                  // Bounded: a denominator past 2^16 cannot improve a ratio whose numerator
                                                  // is itself held under 2^32 / crystal_khz.
    for den in 1..=(1u64 << 16) {
        let num = (tsc_khz * den + crystal_khz / 2) / crystal_khz;
        if num == 0 {
            continue;
        }
        if num > max_numerator {
            break;
        }
        let error = (crystal_khz * num / den).abs_diff(tsc_khz);
        if best.is_none_or(|(e, _, _)| error < e) {
            best = Some((error, den, num));
        }
    }
    let (_, den, num) = best?;
    Some((
        u32::try_from(den).ok()?,
        u32::try_from(num).ok()?,
        crystal_hz,
    ))
}

const ZERO: CpuidResult = CpuidResult {
    eax: 0,
    ebx: 0,
    ecx: 0,
    edx: 0,
};

/// The shape [`CpuidPolicy`] describes on an AMD host: one package of `cores`
/// single-threaded cores (see "One package, not `n`").
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PackageTopology {
    /// Virtual processors in the partition, each one a core.
    pub cores: u32,
}

impl PackageTopology {
    /// Bits of the APIC id that number a core within the package —
    /// `ceil(log2(cores))` — so the ids `0..cores` all share package 0.
    pub fn core_id_bits(&self) -> u32 {
        self.cores.max(1).next_power_of_two().trailing_zeros()
    }
}

/// The four output registers of one `cpuid`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CpuidResult {
    pub eax: u32,
    pub ebx: u32,
    pub ecx: u32,
    pub edx: u32,
}

/// This machine's CPUID policy for one vCPU.
#[derive(Debug, Clone, Copy)]
pub struct CpuidPolicy {
    vp_index: u32,
    clocks: Option<GuestClocks>,
    topology: Option<PackageTopology>,
    /// The partition has the Hyper-V enlightenments (`WhpOptions::hyperv`):
    /// leaves `0x4000_0000`–`0x4000_000f` are WHP's, not hidden.
    hyperv: bool,
}

impl CpuidPolicy {
    /// The policy without the timing leaves: what a partition gets when WHP
    /// would not report its clocks.
    pub fn new(vp_index: u32) -> Self {
        Self {
            vp_index,
            clocks: None,
            topology: None,
            hyperv: false,
        }
    }

    /// The policy with the timing leaves filled from `clocks`.
    pub fn with_clocks(vp_index: u32, clocks: Option<GuestClocks>) -> Self {
        Self {
            vp_index,
            clocks,
            topology: None,
            hyperv: false,
        }
    }

    /// The same policy, also describing `topology` in the AMD topology leaves.
    /// `None` leaves WHP's own answers there.
    pub fn with_topology(self, topology: Option<PackageTopology>) -> Self {
        Self { topology, ..self }
    }

    /// The same policy for a partition with the Hyper-V enlightenments: the
    /// hypervisor leaves are left as WHP answers them, because they are how
    /// the guest finds the enlightenments it was given.
    pub fn with_hyperv(self, hyperv: bool) -> Self {
        Self { hyperv, ..self }
    }

    /// Applies the policy to what WHP would have reported for `leaf`/`subleaf`.
    ///
    /// Pure, so it is unit-tested without a partition — which matters, because the
    /// alternative is discovering a wrong APIC id from a guest's
    /// `[Firmware Bug]: APIC ID mismatch` line.
    pub fn apply(&self, leaf: u32, subleaf: u32, default: CpuidResult) -> CpuidResult {
        let mut result = default;
        match leaf {
            0x0000_0001 if subleaf == 0 => {
                result.ecx |= 1 << 31;
                result.ebx = (result.ebx & 0x00ff_ffff) | (self.vp_index << 24);
                if let Some(topology) = self.topology {
                    // EBX[23:16]: addressable logical processor ids in the
                    // package, valid when EDX[28] (HTT) is set. AMD's
                    // CmpLegacy (0x8000_0001.ECX[1]) already says they are
                    // cores, not threads.
                    let ids = 1u32 << topology.core_id_bits();
                    result.ebx = (result.ebx & 0xff00_ffff) | (ids.min(0xff) << 16);
                    if topology.cores > 1 {
                        result.edx |= 1 << 28;
                    } else {
                        result.edx &= !(1 << 28);
                    }
                }
            }
            0x0000_000b | 0x0000_001f | 0x8000_0026 => result.edx = self.vp_index,
            0x8000_0008 => {
                if let Some(topology) = self.topology {
                    // ECX[7:0] NC = cores - 1, ECX[15:12] ApicIdSize; the rest
                    // of ECX (PerfTscSize) stays WHP's.
                    let nc = topology.cores.clamp(1, 256) - 1;
                    result.ecx = (result.ecx & !0xf0ff) | (topology.core_id_bits() << 12) | nc;
                }
            }
            0x8000_001e => {
                result.eax = self.vp_index;
                if self.topology.is_some() {
                    // EBX[7:0] core id, EBX[15:8] threads per core - 1 = 0;
                    // ECX[7:0] node id 0, ECX[10:8] nodes per package - 1 = 0.
                    result.ebx = (result.ebx & !0xffff) | (self.vp_index & 0xff);
                    result.ecx &= !0x7ff;
                }
            }
            0x4000_0000..=0x4000_000f if self.hyperv => {}
            0x4000_0000 => {
                result = CpuidResult {
                    eax: if self.clocks.is_some() {
                        TIMING_LEAF
                    } else {
                        0
                    },
                    ebx: 0,
                    ecx: 0,
                    edx: 0,
                }
            }
            0x4000_0001..=0x4000_000f => result = ZERO,
            TIMING_LEAF => {
                result = match self.clocks {
                    Some(clocks) => CpuidResult {
                        eax: u32::try_from(clocks.tsc_hz / 1000).unwrap_or(0),
                        ebx: u32::try_from(clocks.apic_timer_hz / 1000).unwrap_or(0),
                        ecx: 0,
                        edx: 0,
                    },
                    None => ZERO,
                }
            }
            0x0000_0015 => {
                if let Some((eax, ebx, ecx)) = self
                    .clocks
                    .filter(|clocks| clocks.intel)
                    .as_ref()
                    .and_then(tsc_crystal_ratio)
                {
                    result = CpuidResult {
                        eax,
                        ebx,
                        ecx,
                        edx: 0,
                    };
                }
            }
            _ => {}
        }
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DEFAULT: CpuidResult = CpuidResult {
        eax: 0x0001_1111,
        ebx: 0x0700_2222,
        ecx: 0x3333_3333,
        edx: 0x4444_4444,
    };

    /// Every intercepted leaf must actually be in the exit list, or the policy is
    /// dead code the guest never reaches.
    #[test]
    fn every_leaf_the_policy_changes_is_intercepted() {
        let policy = CpuidPolicy::new(1).with_topology(Some(PackageTopology { cores: 4 }));
        for leaf in [
            0x0000_0001u32,
            0x0000_000b,
            0x0000_001f,
            0x4000_0000,
            0x8000_0008,
            0x8000_001e,
            0x8000_0026,
        ] {
            assert!(
                CPUID_EXIT_LEAVES.contains(&leaf),
                "leaf {leaf:#x} is changed but not intercepted"
            );
            assert_ne!(
                policy.apply(leaf, 0, DEFAULT),
                DEFAULT,
                "leaf {leaf:#x} is intercepted but the policy changes nothing"
            );
        }
    }

    /// Leaf 1: the hypervisor bit is set and the initial APIC id is the vCPU
    /// index, with the rest of EBX (CLFLUSH size, max logical processors)
    /// untouched.
    #[test]
    fn leaf_1_carries_the_hypervisor_bit_and_the_initial_apic_id() {
        let result = CpuidPolicy::new(3).apply(1, 0, DEFAULT);
        assert_ne!(result.ecx & (1 << 31), 0, "hypervisor-present bit");
        assert_eq!(result.ebx >> 24, 3, "initial APIC id");
        assert_eq!(
            result.ebx & 0x00ff_ffff,
            DEFAULT.ebx & 0x00ff_ffff,
            "the rest of EBX must survive"
        );
        assert_eq!(result.eax, DEFAULT.eax);
        assert_eq!(result.edx, DEFAULT.edx);
        assert_eq!(
            result.ecx & !(1 << 31),
            DEFAULT.ecx & !(1 << 31),
            "no other feature bit may be touched"
        );
    }

    /// A subleaf other than 0 of leaf 1 is not the feature leaf and must be
    /// passed through — WHP reports the exit for every subleaf.
    #[test]
    fn leaf_1_subleaf_1_is_untouched() {
        assert_eq!(CpuidPolicy::new(3).apply(1, 1, DEFAULT), DEFAULT);
    }

    #[test]
    fn with_hyperv_the_hypervisor_leaves_are_whps() {
        let policy = CpuidPolicy::new(0).with_hyperv(true);
        for leaf in 0x4000_0000..=0x4000_000f {
            assert_eq!(policy.apply(leaf, 0, DEFAULT), DEFAULT, "{leaf:#x}");
        }
        // Without it they are hidden, as before.
        assert_ne!(CpuidPolicy::new(0).apply(0x4000_0001, 0, DEFAULT), DEFAULT);
    }

    #[test]
    fn topology_leaves_report_the_x2apic_id_in_edx() {
        for leaf in [0x0000_000bu32, 0x0000_001f, 0x8000_0026] {
            let result = CpuidPolicy::new(2).apply(leaf, 0, DEFAULT);
            assert_eq!(result.edx, 2, "leaf {leaf:#x}");
            assert_eq!(result.eax, DEFAULT.eax, "leaf {leaf:#x} EAX must survive");
            assert_eq!(result.ebx, DEFAULT.ebx, "leaf {leaf:#x} EBX must survive");
        }
    }

    /// Linux's `parse_8000_0008()`: the core shift is `ApicIdSize`, or
    /// `get_count_order(NC + 1)` when that is 0, and the package id is the APIC
    /// id shifted right by it. Every vCPU must land in package 0, for every
    /// count a profile can ask for.
    #[test]
    fn amd_topology_puts_every_vcpu_in_one_package() {
        for cores in 1u32..=64 {
            let topology = Some(PackageTopology { cores });
            for vp in 0..cores {
                let policy = CpuidPolicy::new(vp).with_topology(topology);
                let ext = policy.apply(0x8000_0008, 0, ZERO);
                let nc = ext.ecx & 0xff;
                assert_eq!(nc + 1, cores, "NC for {cores} cores");
                let mut shift = (ext.ecx >> 12) & 0xf;
                if shift == 0 {
                    shift = (nc + 1).next_power_of_two().trailing_zeros();
                }
                let topo = policy.apply(0x8000_001e, 0, ZERO);
                assert_eq!(topo.eax, vp, "extended APIC id");
                assert_eq!(topo.eax >> shift, 0, "vp {vp} of {cores} left package 0");
                assert_eq!(topo.ebx & 0xff, vp, "core id");
                assert_eq!((topo.ebx >> 8) & 0xff, 0, "one thread per core");
                assert_eq!(topo.ecx & 0x7ff, 0, "one node, node 0");

                let leaf1 = policy.apply(1, 0, ZERO);
                assert!((leaf1.ebx >> 16) & 0xff >= cores, "logical ids for {cores}");
                assert_eq!(leaf1.edx & (1 << 28) != 0, cores > 1, "HTT");
                assert_eq!(leaf1.ebx >> 24, vp, "the initial APIC id survives");
            }
        }
    }

    /// The topology rewrite leaves every bit it does not own to WHP.
    #[test]
    fn amd_topology_keeps_whps_other_bits() {
        let all = CpuidResult {
            eax: u32::MAX,
            ebx: u32::MAX,
            ecx: u32::MAX,
            edx: u32::MAX,
        };
        let policy = CpuidPolicy::new(5).with_topology(Some(PackageTopology { cores: 8 }));
        let ext = policy.apply(0x8000_0008, 0, all);
        assert_eq!(
            ext.ecx, 0xffff_3f07,
            "PerfTscSize and the reserved bits survive"
        );
        assert_eq!((ext.eax, ext.ebx, ext.edx), (all.eax, all.ebx, all.edx));
        let topo = policy.apply(0x8000_001e, 0, all);
        assert_eq!((topo.ebx, topo.ecx), (0xffff_0005, 0xffff_f800));
        let leaf1 = policy.apply(1, 0, all);
        assert_eq!((leaf1.ebx, leaf1.edx), (0x0508_ffff, u32::MAX));
        let single = CpuidPolicy::new(0).with_topology(Some(PackageTopology { cores: 1 }));
        assert_eq!(
            single.apply(1, 0, all).edx & (1 << 28),
            0,
            "one core: no HTT"
        );
    }

    /// Without a topology (Intel, or no CPUID policy) those leaves are WHP's.
    #[test]
    fn without_a_topology_whps_answers_stand() {
        let policy = CpuidPolicy::new(1);
        assert_eq!(policy.apply(0x8000_0008, 0, DEFAULT), DEFAULT);
        let leaf1 = policy.apply(1, 0, DEFAULT);
        assert_eq!(leaf1.ebx & 0x00ff_ffff, DEFAULT.ebx & 0x00ff_ffff);
        assert_eq!(leaf1.edx, DEFAULT.edx);
    }

    /// AMD's extended APIC id lives in EAX; without a topology, core and node
    /// id in EBX/ECX are WHP's.
    #[test]
    fn amd_extended_apic_id_lands_in_eax_only() {
        let result = CpuidPolicy::new(2).apply(0x8000_001e, 0, DEFAULT);
        assert_eq!(result.eax, 2);
        assert_eq!(result.ebx, DEFAULT.ebx);
        assert_eq!(result.ecx, DEFAULT.ecx);
    }

    /// The hypervisor interface leaf must be empty, so the guest does not go
    /// looking for Hyper-V enlightenments a WHP partition has none of.
    #[test]
    fn the_hypervisor_signature_leaf_is_empty() {
        let result = CpuidPolicy::new(0).apply(0x4000_0000, 0, DEFAULT);
        assert_eq!(
            (result.eax, result.ebx, result.ecx, result.edx),
            (0, 0, 0, 0)
        );
    }

    /// vCPU 0 is the case that hides bugs: every id it writes is 0, which is
    /// also the default. The bits that are *not* index-derived must still change.
    #[test]
    fn vcpu_zero_still_gets_the_hypervisor_bit() {
        let result = CpuidPolicy::new(0).apply(1, 0, DEFAULT);
        assert_ne!(result.ecx & (1 << 31), 0);
        assert_eq!(result.ebx >> 24, 0);
    }

    /// An unintercepted leaf must pass through untouched even if it somehow
    /// arrives — WHP is free to report exits for more than we asked for.
    #[test]
    fn unknown_leaves_pass_through() {
        let policy = CpuidPolicy::new(1);
        for leaf in [0u32, 2, 7, 0x8000_0007, 0x4000_0011, 0xffff_ffff] {
            assert_eq!(policy.apply(leaf, 0, DEFAULT), DEFAULT, "leaf {leaf:#x}");
        }
    }

    /// What WHP reported on the Zen 1 host this policy was written on.
    const ZEN1: GuestClocks = GuestClocks {
        tsc_hz: 3_500_047_429,
        apic_timer_hz: 200_000_000,
        intel: false,
    };

    const INTEL: GuestClocks = GuestClocks {
        intel: true,
        ..ZEN1
    };

    /// With the clocks known, `0x4000_0000` reaches the timing leaf and that
    /// leaf carries WHP's two frequencies in kHz, under an empty signature.
    #[test]
    fn the_timing_leaf_reports_the_clocks_whp_reports() {
        let policy = CpuidPolicy::with_clocks(0, Some(ZEN1));
        let base = policy.apply(0x4000_0000, 0, DEFAULT);
        assert_eq!(base.eax, TIMING_LEAF);
        assert_eq!((base.ebx, base.ecx, base.edx), (0, 0, 0), "no signature");
        let timing = policy.apply(TIMING_LEAF, 0, DEFAULT);
        assert_eq!(timing.eax, 3_500_047, "TSC kHz");
        assert_eq!(timing.ebx, 200_000, "APIC timer kHz");
        assert_eq!((timing.ecx, timing.edx), (0, 0));
    }

    /// Without clocks the hypervisor range stays as empty as before.
    #[test]
    fn without_clocks_the_hypervisor_range_is_empty() {
        let policy = CpuidPolicy::new(0);
        assert_eq!(policy.apply(0x4000_0000, 0, DEFAULT), ZERO);
        assert_eq!(policy.apply(TIMING_LEAF, 0, DEFAULT), ZERO);
    }

    /// Hyper-V's own interface leaves must not show through the empty
    /// signature now that the range's maximum reaches past them.
    #[test]
    fn hyper_v_interface_leaves_are_hidden() {
        let policy = CpuidPolicy::with_clocks(0, Some(ZEN1));
        for leaf in 0x4000_0001u32..=0x4000_000f {
            assert!(CPUID_EXIT_LEAVES.contains(&leaf), "leaf {leaf:#x}");
            assert_eq!(policy.apply(leaf, 0, DEFAULT), ZERO, "leaf {leaf:#x}");
        }
        assert!(CPUID_EXIT_LEAVES.contains(&TIMING_LEAF));
    }

    /// Leaf 0x15 is Intel's: an AMD guest gets WHP's own answer untouched.
    #[test]
    fn leaf_0x15_is_rewritten_on_intel_only() {
        assert!(CPUID_EXIT_LEAVES.contains(&0x15));
        assert_eq!(
            CpuidPolicy::with_clocks(0, Some(ZEN1)).apply(0x15, 0, DEFAULT),
            DEFAULT
        );
        assert_eq!(CpuidPolicy::new(0).apply(0x15, 0, DEFAULT), DEFAULT);
        let leaf = CpuidPolicy::with_clocks(0, Some(INTEL)).apply(0x15, 0, DEFAULT);
        assert_eq!(leaf.ecx, 200_000_000, "the crystal is the APIC timer");
        assert_eq!(leaf.edx, 0);
    }

    /// The leaf read back the way Linux's `native_calibrate_tsc()` reads it —
    /// in `unsigned int` — gives the TSC within a crystal's tolerance, never
    /// overflows, and sets the APIC timer period WHP's timer really has.
    #[test]
    fn leaf_0x15_gives_linux_the_tsc_in_its_own_arithmetic() {
        for (tsc_hz, apic_timer_hz) in [
            (3_500_047_429u64, 200_000_000u64),
            (1_896_389_000, 200_000_000),
            (2_111_999_000, 100_000_000),
            (4_999_999_999, 25_000_000),
            (3_000_000_000, 24_000_000),
        ] {
            let clocks = GuestClocks {
                tsc_hz,
                apic_timer_hz,
                intel: true,
            };
            let leaf = CpuidPolicy::with_clocks(0, Some(clocks)).apply(0x15, 0, DEFAULT);
            let (denominator, numerator, crystal_hz) = (leaf.eax, leaf.ebx, leaf.ecx);
            assert!(denominator != 0 && numerator != 0);
            let crystal_khz = crystal_hz / 1000;
            let product = crystal_khz
                .checked_mul(numerator)
                .expect("Linux's 32-bit crystal_khz * ebx must not overflow");
            let tsc_khz = u64::from(product / denominator);
            let want = tsc_hz / 1000;
            let error_ppm = (tsc_khz.abs_diff(want) as f64) * 1e6 / want as f64;
            assert!(
                error_ppm < 20.0,
                "{tsc_hz} Hz TSC over a {apic_timer_hz} Hz crystal: {tsc_khz} kHz, {error_ppm:.2} ppm off"
            );
            // HZ=1000: lapic_timer_period = crystal_khz * 1000 / HZ.
            assert_eq!(u64::from(crystal_khz), apic_timer_hz / 1000);
        }
    }

    /// Clocks that cannot be expressed in the leaf leave it alone.
    #[test]
    fn unrepresentable_clocks_leave_leaf_0x15_alone() {
        let too_fast = GuestClocks {
            tsc_hz: 3_000_000_000,
            apic_timer_hz: 5_000_000_000,
            intel: true,
        };
        assert_eq!(tsc_crystal_ratio(&too_fast), None);
        assert_eq!(
            CpuidPolicy::with_clocks(0, Some(too_fast)).apply(0x15, 0, DEFAULT),
            DEFAULT
        );
        let zero = GuestClocks { tsc_hz: 0, ..INTEL };
        assert_eq!(tsc_crystal_ratio(&zero), None);
    }
}
