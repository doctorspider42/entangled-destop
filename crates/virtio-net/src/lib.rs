//! virtio-net device (backlog EPIC 5).
//!
//! Current state: MAC address model (MVP-504). TX/RX queues and the TAP
//! backend land with the mmio transport. Offloads and multiqueue stay off in
//! the MVP — correct first, fast later.

/// A guest MAC address.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MacAddr(pub [u8; 6]);

impl MacAddr {
    /// Derives a stable MAC from a VM name: locally administered, unicast,
    /// same input → same address, so DHCP leases survive VM restarts without
    /// the user pinning a MAC in the config.
    pub fn derive(vm_name: &str) -> Self {
        // FNV-1a over the name; no cryptographic requirement here.
        let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
        for b in vm_name.bytes() {
            hash ^= u64::from(b);
            hash = hash.wrapping_mul(0x100_0000_01b3);
        }
        let h = hash.to_le_bytes();
        // 0x52 = locally administered (bit 1) + unicast (bit 0 clear).
        Self([0x52, h[0], h[1], h[2], h[3], h[4]])
    }

    pub fn is_unicast(&self) -> bool {
        self.0[0] & 0x01 == 0
    }

    pub fn is_locally_administered(&self) -> bool {
        self.0[0] & 0x02 != 0
    }
}

impl std::fmt::Display for MacAddr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let m = self.0;
        write!(
            f,
            "{:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
            m[0], m[1], m[2], m[3], m[4], m[5]
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn derived_mac_is_stable_and_valid() {
        let a = MacAddr::derive("debian-demo");
        let b = MacAddr::derive("debian-demo");
        assert_eq!(a, b);
        assert!(a.is_unicast());
        assert!(a.is_locally_administered());
    }

    #[test]
    fn different_names_differ() {
        assert_ne!(MacAddr::derive("vm-a"), MacAddr::derive("vm-b"));
    }

    #[test]
    fn display_format() {
        let s = MacAddr([0x52, 0, 0xab, 1, 2, 3]).to_string();
        assert_eq!(s, "52:00:ab:01:02:03");
    }
}
