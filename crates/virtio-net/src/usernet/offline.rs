//! The router with the host unplugged: what the fuzz target drives.
//!
//! [`UserNetBackend`](super::UserNetBackend) is the real receive path, but it is
//! also a pump thread and, for every SYN, a `connect()` to whatever address the
//! frame named. A fuzzer generating arbitrary IPv4 headers would therefore dial
//! arbitrary hosts on the internet, at libFuzzer speed, which is neither polite
//! nor reproducible.
//!
//! So this is the same router with [`HostAccess::Offline`]: every parser, the
//! ARP responder, the DHCP server, the DNS relay's own bookkeeping, the flow
//! table and the whole smoltcp TCP state machine, and **no host sockets** except
//! the one the DNS relay binds — pointed at the discard port on loopback, so a
//! forwarded query leaves the machine no more than a `/dev/null` write does.
//!
//! Available to this crate's tests and, through the `fuzzing` feature, to
//! `fuzz/fuzz_targets/usernet_frames.rs`. Nothing a VM builds can reach it.

use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};
use std::time::Duration;

use super::tcp::HostAccess;
use super::{Router, ToGuest, UserNetConfig, UserNetStats};
use crate::frame::MAX_FRAME_LEN;

/// A router, its outbound queue and its counters, driven by hand.
pub struct OfflineNet {
    router: Router,
    out: ToGuest,
    stats: UserNetStats,
}

impl Default for OfflineNet {
    fn default() -> Self {
        Self::new()
    }
}

impl OfflineNet {
    /// The default segment, with DNS pointed at the discard port on loopback.
    pub fn new() -> Self {
        Self::with_config(
            UserNetConfig::default()
                .with_dns_upstream(SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 9))),
        )
    }

    pub fn with_config(config: UserNetConfig) -> Self {
        let mut router = Router::new(config);
        router.tcp = super::TcpNat::with_access(config, HostAccess::Offline);
        Self {
            router,
            out: ToGuest::default(),
            stats: UserNetStats::default(),
        }
    }

    /// Shortens the flow keep-alive pair, so a test need not wait a minute for an
    /// abandoned flow to be retired.
    pub fn set_keepalive(&mut self, keepalive: Duration, idle_timeout: Duration) {
        self.router.tcp.set_keepalive(keepalive, idle_timeout);
    }

    /// One frame from the guest, exactly as `write_frame` delivers it.
    pub fn feed(&mut self, frame: &[u8]) {
        UserNetStats::bump(&self.stats.frames_from_guest);
        self.router
            .handle_guest_frame(frame, &self.out, &self.stats);
    }

    /// One pass of the host side, then everything queued for the guest.
    pub fn poll(&mut self) -> Vec<Vec<u8>> {
        self.router.poll_host(&self.out, &self.stats);
        self.drain()
    }

    /// Everything queued for the guest so far, without running the host side.
    pub fn drain(&mut self) -> Vec<Vec<u8>> {
        let mut frames = Vec::new();
        let mut buf = vec![0u8; MAX_FRAME_LEN];
        while let Some(len) = self.out.pop(&mut buf) {
            frames.push(buf[..len].to_vec());
        }
        frames
    }

    pub fn flow_count(&self) -> usize {
        self.router.tcp.flow_count()
    }

    pub fn refused_at_limit(&self) -> u64 {
        self.router.tcp.refused_at_limit()
    }

    pub fn stats(&self) -> &UserNetStats {
        &self.stats
    }

    /// The guest MAC the router learned, if any.
    pub fn guest_mac(&self) -> Option<smoltcp::wire::EthernetAddress> {
        self.router.guest_mac
    }
}
