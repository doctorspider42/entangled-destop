//! The Hyper-V CPUID leaves a Windows guest is shown (ADR-0008 §2): the
//! hypervisor interface `Hv#1`, granting the reference clock and nothing else.
//!
//! Host-neutral because both backends present the same leaves: WHP's
//! userspace clock (`whp::hv_clock`) answers them from the `cpuid` exit, and
//! KVM gets them through `KVM_SET_CPUID2` and serves the MSRs they grant in
//! the kernel.

/// Highest hypervisor leaf this interface answers.
pub const MAX_LEAF: u32 = 0x4000_0006;
/// "Microsoft Hv" as `0x4000_0000` EBX, ECX, EDX.
const VENDOR: [u32; 3] = [0x7263_694d, 0x666f_736f, 0x7648_2074];
/// "Hv#1" as `0x4000_0001` EAX.
const INTERFACE_HV1: u32 = 0x3123_7648;

/// `0x4000_0003` EAX: the partition privileges granted.
const PRIVILEGES: u32 = (1 << 1) // AccessPartitionReferenceCounter
    | (1 << 5) // AccessHypercallMsrs
    | (1 << 9) // AccessPartitionReferenceTsc
    | (1 << 11); // AccessFrequencyMsrs
/// `0x4000_0003` EDX bit 8: the frequency MSRs are there to read.
const FEATURE_FREQUENCY_MSRS: u32 = 1 << 8;

/// `[eax, ebx, ecx, edx]` of hypervisor leaf `leaf`, or `None` for a leaf
/// outside the interface.
pub fn clock_leaf(leaf: u32) -> Option<[u32; 4]> {
    Some(match leaf {
        0x4000_0000 => [MAX_LEAF, VENDOR[0], VENDOR[1], VENDOR[2]],
        0x4000_0001 => [INTERFACE_HV1, 0, 0, 0],
        0x4000_0003 => [PRIVILEGES, 0, 0, FEATURE_FREQUENCY_MSRS],
        // Recommendations: none, and never notify on spinlocks.
        0x4000_0004 => [0, u32::MAX, 0, 0],
        0x4000_0002 | 0x4000_0005..=MAX_LEAF => [0; 4],
        _ => return None,
    })
}

/// Every leaf of the interface, in order.
pub fn clock_leaves() -> impl Iterator<Item = (u32, [u32; 4])> {
    (0x4000_0000..=MAX_LEAF).filter_map(|leaf| clock_leaf(leaf).map(|regs| (leaf, regs)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_interface_says_microsoft_hv1_with_the_clock() {
        let [max, b, c, d] = clock_leaf(0x4000_0000).unwrap();
        assert_eq!(max, MAX_LEAF);
        let vendor: Vec<u8> = [b, c, d].iter().flat_map(|r| r.to_le_bytes()).collect();
        assert_eq!(&vendor, b"Microsoft Hv");
        assert_eq!(&clock_leaf(0x4000_0001).unwrap()[0].to_le_bytes(), b"Hv#1");
        assert_eq!(clock_leaves().count(), 7);
        assert!(clock_leaf(0x4000_0100).is_none());
    }
}
