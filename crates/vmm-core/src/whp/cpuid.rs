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
//! | `0x4000_0000` | all four registers zeroed | **not** in the KVM policy, and the one place the two hosts legitimately differ — see below |
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

/// Leaves this machine intercepts. Ordered low to high for readability; WHP does
/// not care about the order.
///
/// Deliberately short: every entry costs a VM exit each time the guest reads it,
/// and a guest reads CPUID a lot during boot. A leaf is here only because the
/// policy above changes something in it.
pub const CPUID_EXIT_LEAVES: [u32; 6] = [
    0x0000_0001,
    0x0000_000b,
    0x0000_001f,
    0x4000_0000,
    0x8000_001e,
    0x8000_0026,
];

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
}

impl CpuidPolicy {
    pub fn new(vp_index: u32) -> Self {
        Self { vp_index }
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
            }
            0x0000_000b | 0x0000_001f | 0x8000_0026 => result.edx = self.vp_index,
            0x8000_001e => result.eax = self.vp_index,
            0x4000_0000 => {
                result = CpuidResult {
                    eax: 0,
                    ebx: 0,
                    ecx: 0,
                    edx: 0,
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
        let policy = CpuidPolicy::new(1);
        for leaf in [
            0x0000_0001u32,
            0x0000_000b,
            0x0000_001f,
            0x4000_0000,
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
    fn topology_leaves_report_the_x2apic_id_in_edx() {
        for leaf in [0x0000_000bu32, 0x0000_001f, 0x8000_0026] {
            let result = CpuidPolicy::new(2).apply(leaf, 0, DEFAULT);
            assert_eq!(result.edx, 2, "leaf {leaf:#x}");
            assert_eq!(result.eax, DEFAULT.eax, "leaf {leaf:#x} EAX must survive");
            assert_eq!(result.ebx, DEFAULT.ebx, "leaf {leaf:#x} EBX must survive");
        }
    }

    /// AMD's extended APIC id lives in EAX, and core/node id in EBX/ECX must be
    /// left alone: this machine has no topology to describe beyond "n independent
    /// CPUs".
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
        for leaf in [0u32, 2, 7, 0x8000_0008, 0x4000_0001, 0xffff_ffff] {
            assert_eq!(policy.apply(leaf, 0, DEFAULT), DEFAULT, "leaf {leaf:#x}");
        }
    }
}
