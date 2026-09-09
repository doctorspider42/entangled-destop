//! User-mode networking: a NAT router that lives in this process (backlog
//! WHP-1704).
//!
//! The guest gets a normal Ethernet segment with one other host on it — us — and
//! everything it sends off that segment is translated into ordinary host sockets.
//! No TAP device, no bridge, no `CAP_NET_ADMIN`, no administrator.
//!
//! # Why this exists on both hosts
//!
//! On **Windows** it is the only option. There is no TAP, and the two drivers that
//! provide one — wintun and tap-windows6 — are GPL, which `cargo deny` blocks for
//! host code (ADR-0001). On **Linux** it is the rootless option: `TapBackend` needs
//! an interface an administrator created, which is the single most common reason a
//! first run of a VMM has no network. So this backend is portable on purpose, and
//! its tests run on both hosts.
//!
//! # What is in the segment
//!
//! ```text
//!   guest  192.168.74.15/24  ─┬─ 192.168.74.1  this process
//!                             │                  ├── DHCP server        (port 67)
//!                             │                  ├── DNS relay          (port 53)
//!                             │                  ├── ICMP echo responder
//!                             │                  └── TCP NAT ──> std::net::TcpStream
//! ```
//!
//! The gateway address is also the DNS server and the DHCP server: one host-side
//! interface wearing three hats, which is what every user-mode NAT does and what
//! keeps the guest's routing table to one default route.
//!
//! # The wire codecs are smoltcp's
//!
//! Every header this module reads or writes goes through `smoltcp::wire`
//! (0BSD) — Ethernet, ARP, IPv4, UDP, ICMPv4 and DHCP. That matters for the
//! **untrusted-guest** rule as much as for effort: `*Packet::new_checked` and
//! `*Repr::parse` are the bounds and checksum checks, they are the layer a
//! malformed frame is rejected at, and they are far better tested than anything
//! written here would be. A frame that fails them is counted and dropped; nothing
//! in this module indexes a buffer with a guest-supplied length.
//!
//! # Threads
//!
//! Two directions, two owners:
//!
//! * **guest → host** runs on the vCPU thread that took the queue-notify exit,
//!   inside [`NetBackend::write_frame`]. Replies the router generates itself (an
//!   ARP reply, a DHCP ACK, an ICMP echo reply) are queued for the guest there and
//!   then, so a DHCP exchange never waits for a poll tick.
//! * **host → guest** runs on one pump thread per backend, because a host socket
//!   becoming readable is not an event the guest caused. It polls the DNS relay and
//!   the TCP NAT and queues frames; [`NetBackend::wait_readable`] is what the
//!   device's RX worker blocks on, woken by the pump.
//!
//! The pump polls rather than selects, and sleeps [`PUMP_IDLE`] when there is
//! nothing in flight and [`PUMP_BUSY`] when there is. That is the price of using
//! blocking `std::net` sockets on two platforms with nothing in the standard
//! library to wait on many of them at once; it costs latency, not correctness, and
//! replacing it with epoll/IOCP is a contained change behind this module.

mod dhcp;
#[cfg(any(test, feature = "fuzzing"))]
pub mod offline;
mod tcp;

use std::collections::VecDeque;
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4, UdpSocket};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use smoltcp::phy::ChecksumCapabilities;
use smoltcp::wire::{
    ArpOperation, ArpPacket, ArpRepr, DhcpPacket, DhcpRepr, EthernetAddress, EthernetFrame,
    EthernetProtocol, EthernetRepr, Icmpv4Packet, Icmpv4Repr, IpProtocol, Ipv4Address, Ipv4Packet,
    Ipv4Repr, UdpPacket, UdpRepr,
};

use crate::backend::{NetBackend, NetError, Readiness};
use crate::frame::{ETH_HEADER_LEN, MAX_FRAME_LEN};

pub use dhcp::{Lease, CLIENT_PORT, SERVER_PORT};
pub use tcp::TcpNat;

/// Frames queued for the guest before the backend starts dropping them.
///
/// A cap rather than an unbounded queue because the producer is the *host* network
/// and the consumer is a guest that may have stopped reading — a guest must never
/// be able to make this process grow without bound by ignoring its own RX queue.
/// A full queue is congestion, which is what a real NIC reports by dropping.
pub const MAX_QUEUED_FRAMES: usize = 256;

/// How long the pump sleeps with nothing in flight, and with something.
pub const PUMP_IDLE: Duration = Duration::from_millis(20);
pub const PUMP_BUSY: Duration = Duration::from_millis(1);

/// How long an unanswered DNS query is remembered.
pub const DNS_QUERY_TTL: Duration = Duration::from_secs(5);

/// Outstanding DNS queries kept at once. Bounded because the guest picks how many
/// it sends.
pub const MAX_DNS_QUERIES: usize = 64;

/// Everything about the segment the guest is placed on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UserNetConfig {
    /// This process's address on the segment: gateway, DNS server and DHCP server.
    pub gateway: Ipv4Address,
    /// The single address handed to the guest by DHCP.
    pub guest: Ipv4Address,
    pub netmask: Ipv4Address,
    /// Where DNS queries are forwarded. The host's own resolver when it can be
    /// determined, otherwise a public one — see [`Self::default`].
    pub dns_upstream: SocketAddr,
    pub lease: Duration,
}

impl Default for UserNetConfig {
    /// `192.168.74.0/24`, gateway `.1`, guest `.15`.
    ///
    /// The subnet is deliberately obscure. A user-mode NAT is invisible to the
    /// host's routing table, so it cannot *conflict* with a host network, but a
    /// guest that ends up on the same subnet as its host's LAN cannot reach that
    /// LAN — so `192.168.0.0/24` and `192.168.1.0/24` are exactly the two to
    /// avoid.
    ///
    /// Upstream DNS is `1.1.1.1:53` rather than the host's resolver because there
    /// is no portable way to read the host's resolver configuration
    /// (`/etc/resolv.conf` is Linux-only, and on Windows it takes
    /// `GetAdaptersAddresses`). Overriding it is one field, and the relay does not
    /// care what it points at — a host-local resolver at `127.0.0.53:53` works
    /// just as well.
    fn default() -> Self {
        Self {
            gateway: Ipv4Address::new(192, 168, 74, 1),
            guest: Ipv4Address::new(192, 168, 74, 15),
            netmask: Ipv4Address::new(255, 255, 255, 0),
            dns_upstream: SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::new(1, 1, 1, 1), 53)),
            lease: Duration::from_secs(3600),
        }
    }
}

impl UserNetConfig {
    /// The MAC the guest sees for the gateway.
    ///
    /// Fixed, locally administered and unicast. It never leaves this segment — no
    /// frame the guest sends is forwarded as a frame — so it cannot collide with
    /// anything on the host's real network.
    pub const GATEWAY_MAC: EthernetAddress = EthernetAddress([0x52, 0x54, 0x00, 0x12, 0x34, 0x01]);

    pub fn prefix_len(&self) -> u8 {
        u32::from_bits(self.netmask).count_ones() as u8
    }

    /// True when `addr` is on the guest's segment.
    pub fn is_local(&self, addr: Ipv4Address) -> bool {
        let mask = u32::from_bits(self.netmask);
        u32::from_bits(addr) & mask == u32::from_bits(self.gateway) & mask
    }

    pub fn with_dns_upstream(mut self, upstream: SocketAddr) -> Self {
        self.dns_upstream = upstream;
        self
    }
}

/// `Ipv4Address` is `core::net::Ipv4Addr`, whose `u32` conversion is `From`, so
/// this is only here to keep the mask arithmetic above readable.
trait FromBits {
    fn from_bits(addr: Ipv4Address) -> u32;
}

impl FromBits for u32 {
    fn from_bits(addr: Ipv4Address) -> u32 {
        u32::from_be_bytes(addr.octets())
    }
}

/// Counters, for `entangled doctor` and for tests that need to know *why* nothing
/// arrived.
#[derive(Debug, Default)]
pub struct UserNetStats {
    /// Frames the guest sent that were not a well-formed Ethernet/IPv4 frame this
    /// router serves.
    pub dropped_malformed: AtomicU64,
    /// Frames the guest sent whose protocol this router does not handle (IPv6,
    /// anything but TCP/UDP/ICMP over IPv4).
    pub dropped_unsupported: AtomicU64,
    /// Frames for the guest dropped because its queue was full.
    pub dropped_full: AtomicU64,
    pub frames_from_guest: AtomicU64,
    pub frames_to_guest: AtomicU64,
    pub dhcp_grants: AtomicU64,
    pub dns_queries: AtomicU64,
    pub dns_replies: AtomicU64,
    pub tcp_flows: AtomicU64,
}

impl UserNetStats {
    fn bump(counter: &AtomicU64) {
        counter.fetch_add(1, Ordering::Relaxed);
    }
}

/// Frames waiting to go to the guest, plus the wake-up the device's RX worker
/// blocks on.
#[derive(Default)]
struct ToGuest {
    frames: Mutex<VecDeque<Vec<u8>>>,
    ready: Condvar,
    /// Set by [`NetBackend::wake`]; consumed by the waiter, so a reset is noticed
    /// once rather than turning every later wait into a spin.
    woken: AtomicBool,
}

impl ToGuest {
    /// Queues a frame, or reports that the guest is not keeping up.
    fn push(&self, frame: Vec<u8>) -> bool {
        let Ok(mut frames) = self.frames.lock() else {
            return false;
        };
        if frames.len() >= MAX_QUEUED_FRAMES {
            return false;
        }
        frames.push_back(frame);
        drop(frames);
        self.ready.notify_all();
        true
    }

    fn pop(&self, buf: &mut [u8]) -> Option<usize> {
        let mut frames = self.frames.lock().ok()?;
        let frame = frames.pop_front()?;
        let len = frame.len().min(buf.len());
        buf[..len].copy_from_slice(&frame[..len]);
        Some(len)
    }

    fn wait(&self, timeout: Duration) -> Readiness {
        if self.woken.swap(false, Ordering::AcqRel) {
            return Readiness::WokenUp;
        }
        let Ok(frames) = self.frames.lock() else {
            std::thread::sleep(timeout);
            return Readiness::TimedOut;
        };
        if !frames.is_empty() {
            return Readiness::Readable;
        }
        let (frames, _) = self
            .ready
            .wait_timeout(frames, timeout)
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if self.woken.swap(false, Ordering::AcqRel) {
            Readiness::WokenUp
        } else if frames.is_empty() {
            Readiness::TimedOut
        } else {
            Readiness::Readable
        }
    }

    fn wake(&self) {
        self.woken.store(true, Ordering::Release);
        let _guard = self.frames.lock();
        self.ready.notify_all();
    }
}

/// The DNS relay: guest queries out of one host socket, replies matched back by
/// transaction id.
///
/// One socket rather than one per query, because a guest decides how many queries
/// to make and a socket per query is a file-descriptor exhaustion the guest
/// controls. The transaction id is a 16-bit field the *guest* chooses, so it is
/// never trusted as a key on its own: a reply is delivered to the flow whose id it
/// matches, and if the guest reuses an id the worst case is its own two answers
/// arriving in the wrong order.
struct DnsRelay {
    socket: Option<UdpSocket>,
    upstream: SocketAddr,
    queries: VecDeque<DnsQuery>,
}

#[derive(Debug, Clone, Copy)]
struct DnsQuery {
    id: u16,
    guest_port: u16,
    sent: Instant,
}

impl DnsRelay {
    fn new(upstream: SocketAddr) -> Self {
        Self {
            socket: None,
            upstream,
            queries: VecDeque::new(),
        }
    }

    /// The host socket, bound on first use so a VM that never resolves anything
    /// never opens one.
    fn socket(&mut self) -> Option<&UdpSocket> {
        if self.socket.is_none() {
            match UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)) {
                Ok(socket) => {
                    if let Err(error) = socket.set_nonblocking(true) {
                        tracing::warn!(%error, "cannot make the DNS relay socket non-blocking");
                        return None;
                    }
                    self.socket = Some(socket);
                }
                Err(error) => {
                    tracing::warn!(%error, "cannot open the DNS relay socket");
                    return None;
                }
            }
        }
        self.socket.as_ref()
    }

    /// Forwards one query. `payload` is the whole DNS message; its first two bytes
    /// are the transaction id, and a message too short to have one is not a query.
    fn forward(&mut self, guest_port: u16, payload: &[u8]) -> bool {
        let Some(&[hi, lo, ..]) = payload.get(..2) else {
            return false;
        };
        let id = u16::from_be_bytes([hi, lo]);
        let upstream = self.upstream;
        let Some(socket) = self.socket() else {
            return false;
        };
        if let Err(error) = socket.send_to(payload, upstream) {
            tracing::debug!(%error, "forwarding a DNS query failed");
            return false;
        }
        // Oldest first, so the bound evicts the query least likely to still matter.
        while self.queries.len() >= MAX_DNS_QUERIES {
            self.queries.pop_front();
        }
        self.queries.push_back(DnsQuery {
            id,
            guest_port,
            sent: Instant::now(),
        });
        true
    }

    /// One pending reply, if the upstream resolver has answered: the guest port to
    /// deliver it to and the message.
    fn poll(&mut self) -> Option<(u16, Vec<u8>)> {
        self.expire();
        let socket = self.socket.as_ref()?;
        // A DNS reply over UDP is capped at 512 bytes without EDNS0, and this relay
        // does not advertise a larger receive size on the guest's behalf; anything
        // longer is truncated by the host stack, which is exactly what a resolver
        // handles by retrying over TCP.
        let mut buf = vec![0u8; 512];
        let (len, from) = socket.recv_from(&mut buf).ok()?;
        if from != self.upstream {
            // Not from the resolver we asked. Dropped rather than delivered: this
            // socket is only ever used for one destination.
            return None;
        }
        buf.truncate(len);
        let &[hi, lo, ..] = buf.get(..2)? else {
            return None;
        };
        let id = u16::from_be_bytes([hi, lo]);
        let position = self.queries.iter().position(|q| q.id == id)?;
        let query = self.queries.remove(position)?;
        Some((query.guest_port, buf))
    }

    fn expire(&mut self) {
        let now = Instant::now();
        self.queries
            .retain(|query| now.duration_since(query.sent) < DNS_QUERY_TTL);
    }

    fn is_busy(&self) -> bool {
        !self.queries.is_empty()
    }
}

/// The router: everything that decides what a frame means.
///
/// Behind one mutex because the two directions run on different threads and every
/// piece of state here is small and touched briefly.
struct Router {
    config: UserNetConfig,
    dhcp: dhcp::DhcpServer,
    dns: DnsRelay,
    tcp: TcpNat,
    /// The guest's MAC, learned from the first frame it sends. Needed to address
    /// anything *to* it, and learned rather than configured because the device's
    /// MAC is chosen by whoever built the VM.
    guest_mac: Option<EthernetAddress>,
}

impl Router {
    fn new(config: UserNetConfig) -> Self {
        Self {
            dhcp: dhcp::DhcpServer::new(config.gateway, config.guest, config.netmask, config.lease),
            dns: DnsRelay::new(config.dns_upstream),
            tcp: TcpNat::new(config),
            guest_mac: None,
            config,
        }
    }

    /// Handles one frame from the guest, queueing whatever it deserves in reply.
    fn handle_guest_frame(&mut self, frame: &[u8], out: &ToGuest, stats: &UserNetStats) {
        let Ok(eth) = EthernetFrame::new_checked(frame) else {
            UserNetStats::bump(&stats.dropped_malformed);
            return;
        };
        let Ok(eth_repr) = EthernetRepr::parse(&eth) else {
            UserNetStats::bump(&stats.dropped_malformed);
            return;
        };
        if eth_repr.src_addr.is_unicast() {
            self.guest_mac = Some(eth_repr.src_addr);
        }
        match eth_repr.ethertype {
            EthernetProtocol::Arp => self.handle_arp(eth.payload(), out, stats),
            EthernetProtocol::Ipv4 => self.handle_ipv4(eth.payload(), out, stats),
            // IPv6 and everything else: this router is IPv4-only, which is a scope
            // decision, not a bug. Counted so "the guest has no network" can be
            // told apart from "the guest is trying IPv6".
            _ => UserNetStats::bump(&stats.dropped_unsupported),
        }
    }

    /// Answers an ARP request for the gateway.
    ///
    /// Only for the gateway: a proxy-ARP answer for every address would make the
    /// guest believe the whole internet is on its local segment and send frames
    /// with off-segment destination MACs, which is a slower and stranger path than
    /// routing through a default gateway. Requests for anything else go unanswered,
    /// which is what a real segment does.
    fn handle_arp(&mut self, payload: &[u8], out: &ToGuest, stats: &UserNetStats) {
        let Ok(packet) = ArpPacket::new_checked(payload) else {
            UserNetStats::bump(&stats.dropped_malformed);
            return;
        };
        let Ok(ArpRepr::EthernetIpv4 {
            operation,
            source_hardware_addr,
            source_protocol_addr,
            target_protocol_addr,
            ..
        }) = ArpRepr::parse(&packet)
        else {
            UserNetStats::bump(&stats.dropped_malformed);
            return;
        };
        if operation != ArpOperation::Request || target_protocol_addr != self.config.gateway {
            return;
        }
        let reply = ArpRepr::EthernetIpv4 {
            operation: ArpOperation::Reply,
            source_hardware_addr: UserNetConfig::GATEWAY_MAC,
            source_protocol_addr: self.config.gateway,
            target_hardware_addr: source_hardware_addr,
            target_protocol_addr: source_protocol_addr,
        };
        let mut buf = vec![0u8; ETH_HEADER_LEN + reply.buffer_len()];
        let mut frame = EthernetFrame::new_unchecked(&mut buf[..]);
        EthernetRepr {
            src_addr: UserNetConfig::GATEWAY_MAC,
            dst_addr: source_hardware_addr,
            ethertype: EthernetProtocol::Arp,
        }
        .emit(&mut frame);
        let mut packet = ArpPacket::new_unchecked(frame.payload_mut());
        reply.emit(&mut packet);
        queue(out, stats, buf);
    }

    fn handle_ipv4(&mut self, payload: &[u8], out: &ToGuest, stats: &UserNetStats) {
        let caps = ChecksumCapabilities::default();
        let Ok(packet) = Ipv4Packet::new_checked(payload) else {
            UserNetStats::bump(&stats.dropped_malformed);
            return;
        };
        let Ok(ip) = Ipv4Repr::parse(&packet, &caps) else {
            UserNetStats::bump(&stats.dropped_malformed);
            return;
        };
        // `payload_len` came from the header; take the payload through the packet's
        // own accessor so the slice is the one smoltcp bounds-checked.
        let inner = packet.payload();
        match ip.next_header {
            IpProtocol::Udp => self.handle_udp(&ip, inner, out, stats),
            IpProtocol::Icmp => self.handle_icmp(&ip, inner, out, stats),
            IpProtocol::Tcp => {
                // smoltcp's interface wants the datagram as it came off the wire,
                // header included, and `total_len` is the bound its own parse
                // already validated against the buffer.
                let total = ip.buffer_len() + ip.payload_len;
                match payload.get(..total) {
                    Some(datagram) if self.tcp.from_guest(&ip, datagram, inner) => {
                        UserNetStats::bump(&stats.tcp_flows);
                    }
                    Some(_) => (),
                    None => UserNetStats::bump(&stats.dropped_malformed),
                }
            }
            _ => UserNetStats::bump(&stats.dropped_unsupported),
        }
    }

    fn handle_udp(&mut self, ip: &Ipv4Repr, payload: &[u8], out: &ToGuest, stats: &UserNetStats) {
        let caps = ChecksumCapabilities::default();
        let Ok(packet) = UdpPacket::new_checked(payload) else {
            UserNetStats::bump(&stats.dropped_malformed);
            return;
        };
        let Ok(udp) = UdpRepr::parse(&packet, &ip.src_addr.into(), &ip.dst_addr.into(), &caps)
        else {
            UserNetStats::bump(&stats.dropped_malformed);
            return;
        };
        let data = packet.payload();
        if udp.dst_port == SERVER_PORT && udp.src_port == CLIENT_PORT {
            self.handle_dhcp(data, out, stats);
            return;
        }
        // Anything addressed to port 53 is a DNS query, wherever the guest thinks
        // the resolver is: it was told to use the gateway, but a guest with a
        // hard-coded resolver should still work, and forwarding it to our upstream
        // is both what it wants and the only thing this NAT can do with it.
        if udp.dst_port == 53 {
            UserNetStats::bump(&stats.dns_queries);
            self.dns.forward(udp.src_port, data);
            return;
        }
        UserNetStats::bump(&stats.dropped_unsupported);
    }

    fn handle_dhcp(&mut self, data: &[u8], out: &ToGuest, stats: &UserNetStats) {
        let Ok(packet) = DhcpPacket::new_checked(data) else {
            UserNetStats::bump(&stats.dropped_malformed);
            return;
        };
        let Ok(request) = DhcpRepr::parse(&packet) else {
            UserNetStats::bump(&stats.dropped_malformed);
            return;
        };
        let before = self.dhcp.grants();
        let Some(reply) = self.dhcp.handle(&request) else {
            return;
        };
        if self.dhcp.grants() > before {
            UserNetStats::bump(&stats.dhcp_grants);
            tracing::info!(
                mac = %request.client_hardware_address,
                ip = %reply.your_ip,
                "granted the guest a DHCP lease"
            );
        }
        // Unicast to the client's hardware address but broadcast at the IP layer: a
        // client that has not configured its address yet will not accept a unicast
        // datagram, and addressing the frame to its MAC keeps the reply off any
        // other interface's path.
        let dst_mac = request.client_hardware_address;
        let mut body = vec![0u8; reply.buffer_len()];
        {
            let mut packet = DhcpPacket::new_unchecked(&mut body[..]);
            if reply.emit(&mut packet).is_err() {
                tracing::error!("could not encode the DHCP reply");
                return;
            }
        }
        self.send_udp(
            out,
            stats,
            dst_mac,
            (self.config.gateway, SERVER_PORT),
            (Ipv4Address::BROADCAST, CLIENT_PORT),
            &body,
        );
    }

    /// Replies to a ping addressed to the gateway. Not a toy: `ping 192.168.74.1`
    /// is the first thing anybody does when a guest's network looks wrong, and an
    /// unanswered one sends them looking in the wrong place.
    fn handle_icmp(&mut self, ip: &Ipv4Repr, payload: &[u8], out: &ToGuest, stats: &UserNetStats) {
        if ip.dst_addr != self.config.gateway {
            // Pinging through the NAT would need a host raw socket, which needs
            // privileges on both hosts. Out of scope, and counted so it is visible.
            UserNetStats::bump(&stats.dropped_unsupported);
            return;
        }
        let caps = ChecksumCapabilities::default();
        let Ok(packet) = Icmpv4Packet::new_checked(payload) else {
            UserNetStats::bump(&stats.dropped_malformed);
            return;
        };
        let Ok(Icmpv4Repr::EchoRequest {
            ident,
            seq_no,
            data,
        }) = Icmpv4Repr::parse(&packet, &caps)
        else {
            UserNetStats::bump(&stats.dropped_unsupported);
            return;
        };
        let Some(dst_mac) = self.guest_mac else {
            return;
        };
        let reply = Icmpv4Repr::EchoReply {
            ident,
            seq_no,
            data,
        };
        let ip_repr = Ipv4Repr {
            src_addr: self.config.gateway,
            dst_addr: ip.src_addr,
            next_header: IpProtocol::Icmp,
            payload_len: reply.buffer_len(),
            hop_limit: 64,
        };
        let mut buf = vec![0u8; ETH_HEADER_LEN + ip_repr.buffer_len() + ip_repr.payload_len];
        let mut frame = EthernetFrame::new_unchecked(&mut buf[..]);
        EthernetRepr {
            src_addr: UserNetConfig::GATEWAY_MAC,
            dst_addr: dst_mac,
            ethertype: EthernetProtocol::Ipv4,
        }
        .emit(&mut frame);
        let mut ipv4 = Ipv4Packet::new_unchecked(frame.payload_mut());
        ip_repr.emit(&mut ipv4, &caps);
        let mut icmp = Icmpv4Packet::new_unchecked(ipv4.payload_mut());
        reply.emit(&mut icmp, &caps);
        queue(out, stats, buf);
    }

    /// Wraps `data` in UDP/IPv4/Ethernet and queues it for the guest.
    fn send_udp(
        &self,
        out: &ToGuest,
        stats: &UserNetStats,
        dst_mac: EthernetAddress,
        src: (Ipv4Address, u16),
        dst: (Ipv4Address, u16),
        data: &[u8],
    ) {
        let caps = ChecksumCapabilities::default();
        let udp_repr = UdpRepr {
            src_port: src.1,
            dst_port: dst.1,
        };
        let ip_repr = Ipv4Repr {
            src_addr: src.0,
            dst_addr: dst.0,
            next_header: IpProtocol::Udp,
            payload_len: udp_repr.header_len() + data.len(),
            hop_limit: 64,
        };
        let total = ETH_HEADER_LEN + ip_repr.buffer_len() + ip_repr.payload_len;
        if total > MAX_FRAME_LEN {
            // The guest offers no mergeable buffers, so a frame over the MTU could
            // not be delivered anyway. Dropping beats emitting something the device
            // would reject.
            UserNetStats::bump(&stats.dropped_full);
            return;
        }
        let mut buf = vec![0u8; total];
        let mut frame = EthernetFrame::new_unchecked(&mut buf[..]);
        EthernetRepr {
            src_addr: UserNetConfig::GATEWAY_MAC,
            dst_addr: dst_mac,
            ethertype: EthernetProtocol::Ipv4,
        }
        .emit(&mut frame);
        let mut ipv4 = Ipv4Packet::new_unchecked(frame.payload_mut());
        ip_repr.emit(&mut ipv4, &caps);
        let mut udp = UdpPacket::new_unchecked(ipv4.payload_mut());
        udp_repr.emit(
            &mut udp,
            &src.0.into(),
            &dst.0.into(),
            data.len(),
            |payload| payload.copy_from_slice(data),
            &caps,
        );
        queue(out, stats, buf);
    }

    /// One pass over the host side: DNS replies and TCP. Reports whether anything
    /// is in flight, which is what the pump's sleep length is chosen from.
    fn poll_host(&mut self, out: &ToGuest, stats: &UserNetStats) -> bool {
        while let Some((guest_port, reply)) = self.dns.poll() {
            UserNetStats::bump(&stats.dns_replies);
            let Some(dst_mac) = self.guest_mac else {
                break;
            };
            self.send_udp(
                out,
                stats,
                dst_mac,
                (self.config.gateway, 53),
                (self.config.guest, guest_port),
                &reply,
            );
        }
        if let Some(dst_mac) = self.guest_mac {
            self.tcp
                .poll(dst_mac, &mut |frame| queue(out, stats, frame));
        }
        self.dns.is_busy() || self.tcp.is_busy()
    }
}

/// Queues a frame for the guest, counting a full queue as a drop.
fn queue(out: &ToGuest, stats: &UserNetStats, frame: Vec<u8>) {
    if out.push(frame) {
        UserNetStats::bump(&stats.frames_to_guest);
    } else {
        UserNetStats::bump(&stats.dropped_full);
    }
}

/// A [`NetBackend`] that routes the guest's traffic through host sockets.
pub struct UserNetBackend {
    name: String,
    config: UserNetConfig,
    router: Arc<Mutex<Router>>,
    out: Arc<ToGuest>,
    stats: Arc<UserNetStats>,
    stop: Arc<AtomicBool>,
    /// `None` once joined by `Drop`.
    pump: Option<JoinHandle<()>>,
}

impl UserNetBackend {
    /// Builds the segment and starts the host-side pump thread.
    pub fn new(config: UserNetConfig) -> Result<Self, NetError> {
        let router = Arc::new(Mutex::new(Router::new(config)));
        let out = Arc::new(ToGuest::default());
        let stats = Arc::new(UserNetStats::default());
        let stop = Arc::new(AtomicBool::new(false));

        let pump = {
            let router = Arc::clone(&router);
            let out = Arc::clone(&out);
            let stats = Arc::clone(&stats);
            let stop = Arc::clone(&stop);
            std::thread::Builder::new()
                .name("usernet".into())
                .spawn(move || pump_loop(&router, &out, &stats, &stop))
                .map_err(|source| NetError::Wakeup { source })?
        };
        Ok(Self {
            name: format!("usernet:{}/{}", config.gateway, config.prefix_len()),
            config,
            router,
            out,
            stats,
            stop,
            pump: Some(pump),
        })
    }

    /// The default segment: `192.168.74.0/24`.
    pub fn with_defaults() -> Result<Self, NetError> {
        Self::new(UserNetConfig::default())
    }

    pub fn config(&self) -> &UserNetConfig {
        &self.config
    }

    pub fn stats(&self) -> &Arc<UserNetStats> {
        &self.stats
    }

    /// The lease currently out, if the guest has configured itself. The host-side
    /// evidence a boot test asserts on.
    pub fn lease(&self) -> Option<Lease> {
        self.router.lock().ok().and_then(|r| r.dhcp.lease())
    }

    /// Live NAT flows, out of [`tcp::MAX_FLOWS`].
    ///
    /// The number the flow-leak bug was invisible without: a workload that closes
    /// thousands of connections has to leave this at zero, and nothing could see it
    /// from outside the crate until it did not. Also what `entangled doctor` and a
    /// soak run should watch.
    pub fn flow_count(&self) -> usize {
        self.router.lock().map(|r| r.tcp.flow_count()).unwrap_or(0)
    }

    /// Flows retired since the backend was built, and SYNs refused because the
    /// table was full. Together with [`Self::flow_count`] they say whether a full
    /// table is a workload or a leak.
    pub fn flows_retired(&self) -> u64 {
        self.router.lock().map(|r| r.tcp.retired()).unwrap_or(0)
    }

    pub fn flows_refused_at_limit(&self) -> u64 {
        self.router
            .lock()
            .map(|r| r.tcp.refused_at_limit())
            .unwrap_or(0)
    }

    /// The `ip=` clause a kernel with `CONFIG_IP_PNP` can use to configure itself
    /// without a DHCP client in the initramfs, as an alternative to DHCP.
    ///
    /// Same numbers the DHCP server would hand out, from the same config, so the
    /// two cannot drift apart.
    pub fn static_ip_cmdline(&self) -> String {
        format!(
            "ip={}::{}:{}::eth0:off:{}",
            self.config.guest, self.config.gateway, self.config.netmask, self.config.gateway
        )
    }
}

fn pump_loop(router: &Mutex<Router>, out: &ToGuest, stats: &UserNetStats, stop: &AtomicBool) {
    while !stop.load(Ordering::Acquire) {
        let busy = match router.lock() {
            Ok(mut router) => router.poll_host(out, stats),
            // A poisoned router means a host-side panic already happened; the pump
            // stopping is the correct response, and the guest simply sees a NIC
            // that stops receiving rather than a second panic.
            Err(_) => {
                tracing::error!("the usernet router lock is poisoned; stopping the pump");
                return;
            }
        };
        std::thread::sleep(if busy { PUMP_BUSY } else { PUMP_IDLE });
    }
}

impl Drop for UserNetBackend {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(pump) = self.pump.take() {
            if pump.join().is_err() {
                tracing::error!("the usernet pump thread panicked");
            }
        }
    }
}

impl NetBackend for UserNetBackend {
    fn name(&self) -> &str {
        &self.name
    }

    /// A frame from the guest. Always reported as accepted: this backend is the
    /// network, so a frame it cannot route is dropped and counted exactly as a
    /// router drops a packet — never reported as a host failure, which would take
    /// the device down.
    fn write_frame(&self, frame: &[u8]) -> Result<usize, NetError> {
        UserNetStats::bump(&self.stats.frames_from_guest);
        match self.router.lock() {
            Ok(mut router) => router.handle_guest_frame(frame, &self.out, &self.stats),
            Err(_) => tracing::error!("the usernet router lock is poisoned; dropping a frame"),
        }
        Ok(frame.len())
    }

    fn read_frame(&self, buf: &mut [u8]) -> Result<Option<usize>, NetError> {
        Ok(self.out.pop(buf))
    }

    fn wait_readable(&self, timeout: Duration) -> Result<Readiness, NetError> {
        Ok(self.out.wait(timeout))
    }

    fn wake(&self) -> Result<(), NetError> {
        self.out.wake();
        Ok(())
    }
}

#[cfg(test)]
mod tests;
