//! virtio-net device with a TAP backend (backlog EPIC 5).
//!
//! Four layers, mirroring `virtio-block`:
//!
//! * [`frame`] — the virtio-net header and every pure frame check (portable,
//!   where most of the unit tests live),
//! * [`backend`] — the [`NetBackend`] contract the device drives, plus its
//!   typed errors,
//! * [`tap`] — the Linux TAP backend (`/dev/net/tun` + `TUNSETIFF`),
//! * [`device`] — [`NetDevice`], the `virtio_core::VirtioDevice`
//!   implementation, its RX worker thread and its counters.
//!
//! Nothing in this crate knows about virtio-mmio: the device sees queues,
//! features and config space only, so the post-MVP virtio-pci transport can
//! drive it unchanged.
//!
//! # Using it from a VM builder
//!
//! ```no_run
//! # #[cfg(target_os = "linux")]
//! # fn main() -> Result<(), Box<dyn std::error::Error>> {
//! use virtio_net::{MacAddr, NetDevice, TapBackend};
//!
//! // The TAP interface is set up once by the host administrator; see
//! // `scripts/setup-tap.sh` and the CAP_NET_ADMIN notes in `virtio_net::tap`.
//! let backend = TapBackend::open("entangled0")?;
//! let mac = MacAddr::derive("debian-demo"); // stable per VM name
//! let device = NetDevice::new(backend, mac);
//!
//! // …then hand `Box::new(device)` to `virtio_core::MmioTransport::new`.
//! # let _ = device;
//! # Ok(()) }
//! # #[cfg(not(target_os = "linux"))] fn main() {}
//! ```
//!
//! Queue order is fixed by the spec and by [`device::RX_QUEUE`] /
//! [`device::TX_QUEUE`]: queue 0 receives, queue 1 transmits. The device starts
//! its RX worker thread on activation and joins it on reset or drop, so a VM
//! that shuts down leaves no thread and no open TAP descriptor behind.
//! [`NetDevice::stats`] exposes per-direction counters, including one counter
//! per drop reason, for `entangled doctor` and for tests.

pub mod backend;
pub mod device;
pub mod frame;

#[cfg(target_os = "linux")]
pub mod tap;

pub use backend::{NetBackend, NetError, Readiness};
pub use device::{
    NetDevice, NetStats, CHAINS_PER_NOTIFY, FEATURES, NUM_QUEUES, RX_QUEUE, TX_QUEUE,
    VIRTIO_NET_F_MAC,
};
pub use frame::{
    validate_rx_frame, validate_tx_buffer, FrameError, NetHeader, ETH_HEADER_LEN, MAX_BUFFER_LEN,
    MAX_FRAME_LEN, MTU, VIRTIO_NET_HDR_LEN,
};

#[cfg(target_os = "linux")]
pub use tap::TapBackend;

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
