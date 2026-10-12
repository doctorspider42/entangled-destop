//! UDP for the guest beyond DHCP, DNS and NTP (2026-10-13): QUIC, games,
//! voice, anything that is not one of the three the router answers itself.
//!
//! **Mapping.** One host socket per guest source port, bound to an ephemeral
//! port of the host's choosing — endpoint-independent mapping, the kind that
//! lets a game's one socket talk to its matchmaker and its peers through one
//! host port. **Filtering is address-and-port restricted:** a datagram from the
//! host side reaches the guest only if the guest has sent to exactly that
//! address and port on that flow. The internet cannot open a conversation the
//! guest did not start, and only the profile can open a listener
//! ([`UdpNat::add_forward`]).
//!
//! **Untrusted guest, bounded host.** The guest chooses how many source ports
//! it uses, so flows are capped ([`MAX_UDP_FLOWS`]) and each remembers a capped
//! set of peers ([`MAX_PEERS_PER_FLOW`]); an idle flow is closed after
//! [`UDP_IDLE_TIMEOUT`]. Host-local destinations are refused exactly as the TCP
//! NAT refuses them — the gateway, the segment, loopback, `0.0.0.0`, broadcast
//! and multicast — so the guest cannot reach a service bound on the host's own
//! loopback through UDP either. Work per poll is bounded by a datagram budget.
//! A datagram too large for one of the guest's frames is dropped and counted:
//! the segment has no fragmentation, and QUIC (which probes its path MTU)
//! settles below it.
//!
//! **Inbound forwards** (`udp:` rules): a host socket bound where the profile
//! says. Each host peer that writes to it is given a port on the gateway's
//! address, and the guest sees the datagrams arrive from `gateway:port`;
//! whatever the guest sends back to that address goes out of the forward's
//! socket to that peer.

use std::io::ErrorKind;
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4, UdpSocket};
use std::time::{Duration, Instant};

use super::tcp::{HostAccess, PortForward};
use super::UserNetConfig;
use crate::frame::{ETH_HEADER_LEN, MAX_FRAME_LEN};

/// Guest source ports with a host socket at once.
pub const MAX_UDP_FLOWS: usize = 128;
/// Peers one flow remembers (oldest forgotten first).
pub const MAX_PEERS_PER_FLOW: usize = 32;
/// Host peers one inbound forward keeps a gateway port for.
pub const MAX_FORWARD_PEERS: usize = 64;
/// A flow (or a forward's peer) idle this long is closed.
pub const UDP_IDLE_TIMEOUT: Duration = Duration::from_secs(90);
/// Datagrams one poll moves to the guest, over every socket.
pub const MAX_DATAGRAMS_PER_POLL: usize = 64;
/// The largest UDP payload one guest frame carries.
pub const MAX_UDP_PAYLOAD: usize = MAX_FRAME_LEN - ETH_HEADER_LEN - 20 - 8;
/// Gateway ports given to inbound forwards' peers.
const GATEWAY_PORTS: std::ops::RangeInclusive<u16> = 49152..=65535;

struct Flow {
    guest_port: u16,
    socket: UdpSocket,
    peers: Vec<SocketAddrV4>,
    last: Instant,
}

struct ForwardPeer {
    host: SocketAddr,
    gateway_port: u16,
    last: Instant,
}

struct Forward {
    socket: UdpSocket,
    guest_port: u16,
    peers: Vec<ForwardPeer>,
}

/// A datagram for the guest: from `src` to the guest's `dst_port`.
pub(super) struct ToGuestDatagram {
    pub src: SocketAddrV4,
    pub dst_port: u16,
    pub data: Vec<u8>,
}

pub(super) struct UdpNat {
    config: UserNetConfig,
    access: HostAccess,
    flows: Vec<Flow>,
    forwards: Vec<Forward>,
    next_gateway_port: u16,
    buf: Vec<u8>,
    refused_at_limit: u64,
    dropped_oversize: u64,
    dropped_unsolicited: u64,
}

impl UdpNat {
    pub(super) fn new(config: UserNetConfig) -> Self {
        Self::with_access(config, HostAccess::Routed)
    }

    pub(super) fn with_access(config: UserNetConfig, access: HostAccess) -> Self {
        Self {
            config,
            access,
            flows: Vec::new(),
            forwards: Vec::new(),
            next_gateway_port: *GATEWAY_PORTS.start(),
            buf: Vec::new(),
            refused_at_limit: 0,
            dropped_oversize: 0,
            dropped_unsolicited: 0,
        }
    }

    pub(super) fn flow_count(&self) -> usize {
        self.flows.len()
    }

    pub(super) fn refused_at_limit(&self) -> u64 {
        self.refused_at_limit
    }

    pub(super) fn dropped_unsolicited(&self) -> u64 {
        self.dropped_unsolicited
    }

    /// Host datagrams too large for one guest frame.
    pub(super) fn dropped_oversize(&self) -> u64 {
        self.dropped_oversize
    }

    pub(super) fn is_busy(&self) -> bool {
        !self.flows.is_empty() || !self.forwards.is_empty()
    }

    /// Listens for an inbound `udp:` forward; answers the address bound.
    pub(super) fn add_forward(&mut self, forward: PortForward) -> std::io::Result<SocketAddr> {
        let socket = UdpSocket::bind(forward.host)?;
        socket.set_nonblocking(true)?;
        let bound = socket.local_addr()?;
        tracing::info!(host = %bound, guest_port = forward.guest_port, "usernet: forwarding a UDP port");
        self.forwards.push(Forward {
            socket,
            guest_port: forward.guest_port,
            peers: Vec::new(),
        });
        Ok(bound)
    }

    fn refused(&self, dst: Ipv4Addr) -> bool {
        self.access == HostAccess::Routed
            && (self.config.is_local(dst)
                || dst.is_loopback()
                || dst.is_unspecified()
                || dst.is_broadcast()
                || dst.is_multicast())
    }

    /// A datagram from the guest's `src_port` to `dst`. Answers whether the
    /// NAT took it (sent, or knowingly discarded); `false` means "nothing here
    /// serves this", for the caller to count.
    pub(super) fn carry(&mut self, src_port: u16, dst: SocketAddrV4, data: &[u8]) -> bool {
        // A reply to an inbound forward's peer: addressed to the gateway port
        // that peer was given.
        if *dst.ip() == self.config.gateway {
            for forward in &mut self.forwards {
                if forward.guest_port != src_port {
                    continue;
                }
                if let Some(peer) = forward
                    .peers
                    .iter_mut()
                    .find(|p| p.gateway_port == dst.port())
                {
                    peer.last = Instant::now();
                    if self.access != HostAccess::Offline {
                        let _ = forward.socket.send_to(data, peer.host);
                    }
                    return true;
                }
            }
        }
        if self.refused(*dst.ip()) {
            tracing::debug!(remote = %dst, "refusing a guest datagram to a host-local address");
            return false;
        }
        let index = match self.flows.iter().position(|f| f.guest_port == src_port) {
            Some(index) => index,
            None => {
                if self.flows.len() >= MAX_UDP_FLOWS {
                    self.refused_at_limit = self.refused_at_limit.saturating_add(1);
                    tracing::warn!(
                        flows = self.flows.len(),
                        "refusing a guest UDP flow: at the limit"
                    );
                    return true;
                }
                if self.access == HostAccess::Offline {
                    return true;
                }
                let socket = match UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0))
                    .and_then(|s| s.set_nonblocking(true).map(|()| s))
                {
                    Ok(socket) => socket,
                    Err(error) => {
                        tracing::warn!(%error, "cannot open a host socket for a guest UDP flow");
                        return true;
                    }
                };
                self.flows.push(Flow {
                    guest_port: src_port,
                    socket,
                    peers: Vec::new(),
                    last: Instant::now(),
                });
                self.flows.len() - 1
            }
        };
        let flow = &mut self.flows[index];
        if !flow.peers.contains(&dst) {
            if flow.peers.len() >= MAX_PEERS_PER_FLOW {
                flow.peers.remove(0);
            }
            flow.peers.push(dst);
        }
        flow.last = Instant::now();
        if let Err(error) = flow.socket.send_to(data, dst) {
            tracing::debug!(%error, remote = %dst, "a guest datagram could not be sent");
        }
        true
    }

    /// Datagrams waiting on the host side, up to `budget`; idle flows closed.
    pub(super) fn poll(&mut self, budget: usize) -> Vec<ToGuestDatagram> {
        let mut out = Vec::new();
        let now = Instant::now();
        self.flows
            .retain(|f| now.duration_since(f.last) < UDP_IDLE_TIMEOUT);
        for forward in &mut self.forwards {
            forward
                .peers
                .retain(|p| now.duration_since(p.last) < UDP_IDLE_TIMEOUT);
        }
        if self.access == HostAccess::Offline {
            return out;
        }
        if self.buf.len() != 65_536 {
            self.buf = vec![0u8; 65_536];
        }
        let budget = budget.min(MAX_DATAGRAMS_PER_POLL);
        for flow in &mut self.flows {
            while out.len() < budget {
                let (len, from) = match flow.socket.recv_from(&mut self.buf) {
                    Ok(got) => got,
                    Err(error) if error.kind() == ErrorKind::WouldBlock => break,
                    // Windows reports an ICMP port-unreachable for an earlier
                    // send as a failed receive (WSAECONNRESET); it is about
                    // that datagram, not this socket.
                    Err(_) => continue,
                };
                let SocketAddr::V4(from) = from else {
                    continue;
                };
                if !flow.peers.contains(&from) {
                    self.dropped_unsolicited = self.dropped_unsolicited.saturating_add(1);
                    continue;
                }
                if len > MAX_UDP_PAYLOAD {
                    self.dropped_oversize = self.dropped_oversize.saturating_add(1);
                    continue;
                }
                flow.last = now;
                out.push(ToGuestDatagram {
                    src: from,
                    dst_port: flow.guest_port,
                    data: self.buf[..len].to_vec(),
                });
            }
        }
        for forward in &mut self.forwards {
            while out.len() < budget {
                let (len, from) = match forward.socket.recv_from(&mut self.buf) {
                    Ok(got) => got,
                    Err(error) if error.kind() == ErrorKind::WouldBlock => break,
                    Err(_) => continue,
                };
                if len > MAX_UDP_PAYLOAD {
                    self.dropped_oversize = self.dropped_oversize.saturating_add(1);
                    continue;
                }
                let gateway_port = match forward.peers.iter_mut().find(|p| p.host == from) {
                    Some(peer) => {
                        peer.last = now;
                        peer.gateway_port
                    }
                    None => {
                        if forward.peers.len() >= MAX_FORWARD_PEERS {
                            forward.peers.remove(0);
                        }
                        let port = self.next_gateway_port;
                        self.next_gateway_port = if port == *GATEWAY_PORTS.end() {
                            *GATEWAY_PORTS.start()
                        } else {
                            port + 1
                        };
                        forward.peers.push(ForwardPeer {
                            host: from,
                            gateway_port: port,
                            last: now,
                        });
                        port
                    }
                };
                out.push(ToGuestDatagram {
                    src: SocketAddrV4::new(self.config.gateway, gateway_port),
                    dst_port: forward.guest_port,
                    data: self.buf[..len].to_vec(),
                });
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> UserNetConfig {
        UserNetConfig::default()
    }

    fn echo_server() -> (UdpSocket, SocketAddrV4) {
        let socket = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let SocketAddr::V4(addr) = socket.local_addr().unwrap() else {
            panic!()
        };
        (socket, addr)
    }

    fn poll_until(nat: &mut UdpNat, want: usize) -> Vec<ToGuestDatagram> {
        let mut got = Vec::new();
        let deadline = Instant::now() + Duration::from_secs(5);
        while got.len() < want && Instant::now() < deadline {
            got.extend(nat.poll(MAX_DATAGRAMS_PER_POLL));
            std::thread::sleep(Duration::from_millis(2));
        }
        got
    }

    #[test]
    fn a_guest_datagram_goes_out_and_the_answer_comes_back_to_its_port() {
        let mut nat = UdpNat::with_access(config(), HostAccess::HostLocal);
        let (server, addr) = echo_server();
        assert!(nat.carry(40000, addr, b"ping"));
        let mut buf = [0u8; 64];
        let (n, from) = server.recv_from(&mut buf).unwrap();
        assert_eq!(&buf[..n], b"ping");
        server.send_to(b"pong", from).unwrap();
        let got = poll_until(&mut nat, 1);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].src, addr);
        assert_eq!(got[0].dst_port, 40000);
        assert_eq!(got[0].data, b"pong");
        // The same guest port reuses its flow, to any peer.
        let (_other, addr2) = echo_server();
        assert!(nat.carry(40000, addr2, b"x"));
        assert_eq!(nat.flow_count(), 1);
    }

    #[test]
    fn a_stranger_cannot_open_a_conversation() {
        let mut nat = UdpNat::with_access(config(), HostAccess::HostLocal);
        let (server, addr) = echo_server();
        assert!(nat.carry(40001, addr, b"hello"));
        let mut buf = [0u8; 64];
        let (_, flow_addr) = server.recv_from(&mut buf).unwrap();
        // Someone else writes to the flow's host port.
        let (stranger, _) = echo_server();
        stranger.send_to(b"spam", flow_addr).unwrap();
        std::thread::sleep(Duration::from_millis(50));
        assert!(nat.poll(MAX_DATAGRAMS_PER_POLL).is_empty());
        assert_eq!(nat.dropped_unsolicited(), 1);
    }

    #[test]
    fn host_local_destinations_are_refused() {
        let mut nat = UdpNat::new(config());
        for dst in [
            config().gateway,
            Ipv4Addr::new(192, 168, 74, 200),
            Ipv4Addr::LOCALHOST,
            Ipv4Addr::UNSPECIFIED,
            Ipv4Addr::BROADCAST,
            Ipv4Addr::new(224, 0, 0, 251),
        ] {
            assert!(
                !nat.carry(40002, SocketAddrV4::new(dst, 9999), b"x"),
                "{dst} must not be reachable"
            );
        }
        assert_eq!(nat.flow_count(), 0);
    }

    #[test]
    fn the_flow_table_is_bounded() {
        let mut nat = UdpNat::with_access(config(), HostAccess::Offline);
        let dst = SocketAddrV4::new(Ipv4Addr::new(203, 0, 113, 9), 443);
        for port in 0..(MAX_UDP_FLOWS as u16 + 10) {
            nat.carry(10_000 + port, dst, b"x");
        }
        // Offline opens no sockets, so the table stays empty; the limit is
        // asserted with real ones below.
        let mut nat = UdpNat::with_access(config(), HostAccess::HostLocal);
        let (_server, addr) = echo_server();
        for port in 0..(MAX_UDP_FLOWS as u16 + 10) {
            nat.carry(10_000 + port, addr, b"x");
        }
        assert_eq!(nat.flow_count(), MAX_UDP_FLOWS);
        assert_eq!(nat.refused_at_limit(), 10);
    }

    #[test]
    fn an_inbound_forward_reaches_the_guest_and_its_reply_reaches_the_peer() {
        let mut nat = UdpNat::with_access(config(), HostAccess::HostLocal);
        let bound = nat
            .add_forward(PortForward {
                host: SocketAddr::from((Ipv4Addr::LOCALHOST, 0)),
                guest_port: 27015,
            })
            .unwrap();
        let (peer, _) = echo_server();
        peer.send_to(b"query", bound).unwrap();
        let got = poll_until(&mut nat, 1);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].dst_port, 27015);
        assert_eq!(*got[0].src.ip(), config().gateway);
        assert_eq!(got[0].data, b"query");
        // The guest answers the gateway port it saw.
        assert!(nat.carry(27015, got[0].src, b"answer"));
        let mut buf = [0u8; 64];
        peer.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let (n, from) = peer.recv_from(&mut buf).unwrap();
        assert_eq!(&buf[..n], b"answer");
        assert_eq!(from, bound);
    }
}
