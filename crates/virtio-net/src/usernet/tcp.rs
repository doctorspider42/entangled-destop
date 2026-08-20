//! Outbound TCP through the NAT (backlog WHP-1704).
//!
//! The guest's TCP connections are **terminated in this process** and re-made as
//! host sockets. That is what makes a NAT without privileges possible at all: the
//! host never sees the guest's packets, only ordinary `connect()`s from this
//! process, so nothing needs a raw socket, a TAP device or an administrator.
//!
//! ```text
//!   guest ──TCP/IP over virtio-net──> smoltcp tcp::Socket ──bytes──> std::net::TcpStream ──> host
//! ```
//!
//! # Why smoltcp rather than a hand-written TCP
//!
//! Because the guest side really is TCP: sequence numbers, windows,
//! retransmission, delayed ACKs, FIN handshakes and PAWS. `smoltcp::socket::tcp` is
//! a complete, well-tested implementation of exactly that, `no_std`, 0BSD, with no
//! I/O of its own — it takes packets in and gives packets out, which is precisely
//! the shape this needs.
//!
//! Two smoltcp features carry the design:
//!
//! * **`Medium::Ip`**, not `Medium::Ethernet`. The router above this module already
//!   owns the Ethernet layer, the ARP table and the guest's MAC, so smoltcp is
//!   handed bare IPv4 datagrams and never needs a neighbour cache. One less thing
//!   for the two halves to disagree about.
//! * **`set_any_ip(true)`**, which lets the interface accept a packet addressed to
//!   somebody who is not us. Without it a socket could only be reached at the
//!   gateway's own address; with it, a socket listening on `93.184.216.34:80`
//!   receives the guest's SYN to that address, which is how a transparent proxy
//!   works and how the guest is spared any awareness of the NAT.
//!
//! # The host connect runs on its own thread
//!
//! `std` has no non-blocking `connect`, and a blocking one on the pump thread would
//! stall every other flow for the length of a DNS-less TCP handshake to a dead
//! host. So each new flow spawns one short-lived thread that does
//! `TcpStream::connect_timeout` and sends the result down a channel; the pump picks
//! it up on a later pass. The threads are bounded by [`MAX_FLOWS`], which is what
//! keeps a guest opening connections in a loop from being a thread bomb.
//!
//! # What the guest cannot do
//!
//! * open more than [`MAX_FLOWS`] connections — further SYNs are dropped, which the
//!   guest sees as a lossy network and retries, rather than as unbounded host state;
//! * reach the host itself: a SYN whose destination is on the guest's own segment
//!   ([`UserNetConfig::is_local`]) is refused. Otherwise `192.168.74.1` — or any
//!   other address the host happens to answer on — would be reachable from inside
//!   the guest, and a guest reaching *host-local* services is the whole class of bug
//!   user-mode networking is supposed to avoid;
//! * make this module index a buffer with a guest-supplied number. Every header read
//!   here goes through `smoltcp::wire`'s checked accessors.

use std::io::{ErrorKind, Read, Write};
use std::net::{Ipv4Addr, Shutdown, SocketAddr, SocketAddrV4, TcpStream};
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::time::{Duration, Instant as StdInstant};

use smoltcp::iface::{Config, Interface, SocketHandle, SocketSet};
use smoltcp::phy::{Device, DeviceCapabilities, Medium, RxToken, TxToken};
use smoltcp::socket::tcp;
use smoltcp::time::Instant;
use smoltcp::wire::{
    EthernetAddress, EthernetFrame, EthernetProtocol, EthernetRepr, HardwareAddress, IpCidr,
    Ipv4Repr, TcpPacket,
};

use super::UserNetConfig;
use crate::frame::{ETH_HEADER_LEN, MAX_FRAME_LEN};

/// Concurrent guest TCP connections. Each costs two socket buffers and, briefly,
/// one connect thread.
pub const MAX_FLOWS: usize = 64;

/// Per-direction socket buffer for one flow. 16 KiB is a window big enough that a
/// package download is not round-trip bound, and 64 flows of it is 2 MiB — bounded,
/// and bounded by *us* rather than by the guest.
pub const FLOW_BUFFER: usize = 16 * 1024;

/// How long a host connect is given before the flow is abandoned.
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// IPv4 datagrams waiting in either direction, as smoltcp's `Device`.
///
/// This is the whole of the plumbing between the router's Ethernet layer and
/// smoltcp: `rx` is what the guest sent, `tx` is what smoltcp wants sent back.
/// Queues rather than a callback because smoltcp drives the transfer from inside
/// `Interface::poll`, and the router has to add an Ethernet header to everything
/// coming out.
#[derive(Default)]
struct IpQueues {
    rx: std::collections::VecDeque<Vec<u8>>,
    tx: std::collections::VecDeque<Vec<u8>>,
}

impl Device for IpQueues {
    type RxToken<'a> = QueueRxToken;
    type TxToken<'a> = QueueTxToken<'a>;

    fn receive(&mut self, _timestamp: Instant) -> Option<(Self::RxToken<'_>, Self::TxToken<'_>)> {
        let packet = self.rx.pop_front()?;
        Some((QueueRxToken(packet), QueueTxToken(&mut self.tx)))
    }

    fn transmit(&mut self, _timestamp: Instant) -> Option<Self::TxToken<'_>> {
        Some(QueueTxToken(&mut self.tx))
    }

    fn capabilities(&self) -> DeviceCapabilities {
        let mut caps = DeviceCapabilities::default();
        caps.medium = Medium::Ip;
        // The guest's MTU minus nothing: this device's frames are IP datagrams that
        // the router wraps in 14 bytes of Ethernet, and the guest's cap is on the
        // whole frame.
        caps.max_transmission_unit = MAX_FRAME_LEN - ETH_HEADER_LEN;
        caps
    }
}

struct QueueRxToken(Vec<u8>);

impl RxToken for QueueRxToken {
    fn consume<R, F: FnOnce(&[u8]) -> R>(self, f: F) -> R {
        f(&self.0)
    }
}

struct QueueTxToken<'a>(&'a mut std::collections::VecDeque<Vec<u8>>);

impl TxToken for QueueTxToken<'_> {
    fn consume<R, F: FnOnce(&mut [u8]) -> R>(self, len: usize, f: F) -> R {
        let mut buffer = vec![0u8; len];
        let result = f(&mut buffer);
        self.0.push_back(buffer);
        result
    }
}

/// One guest connection and the host socket it was translated into.
struct Flow {
    handle: SocketHandle,
    /// The guest side of the connection, which is what makes a flow unique.
    guest_port: u16,
    remote: SocketAddrV4,
    /// The host connect, until it finishes.
    connecting: Option<Receiver<std::io::Result<TcpStream>>>,
    stream: Option<TcpStream>,
    started: StdInstant,
    /// The host end has returned end-of-file; the guest gets a FIN once its own
    /// pending data has been handed over.
    host_eof: bool,
    /// The guest has sent its FIN and the host stream's write half has been shut
    /// down in response. One bit, because `shutdown` must happen exactly once:
    /// the socket stays in `CloseWait` until *we* close our half, so the
    /// condition that detects it stays true for as long as the flow lives.
    guest_eof: bool,
}

/// The NAT: one smoltcp interface, one socket per guest connection.
pub struct TcpNat {
    config: UserNetConfig,
    iface: Interface,
    device: IpQueues,
    sockets: SocketSet<'static>,
    flows: Vec<Flow>,
    /// Monotonic base for smoltcp's clock, which wants milliseconds since an
    /// arbitrary origin.
    epoch: StdInstant,
    /// Always false outside this crate's tests, which have no other way to point a
    /// flow at a listener they control — a test cannot rely on the machine having a
    /// routable address, and every address it *can* bind is host-local by
    /// definition. Set only by [`Self::new_allowing_host_local`], which is
    /// `#[cfg(test)]`, so no configuration or guest input can reach it.
    allow_host_local: bool,
}

impl TcpNat {
    pub fn new(config: UserNetConfig) -> Self {
        Self::build(config, false)
    }

    /// [`Self::new`] with the host-local guard off, for the end-to-end test.
    #[cfg(test)]
    fn new_allowing_host_local(config: UserNetConfig) -> Self {
        Self::build(config, true)
    }

    fn build(config: UserNetConfig, allow_host_local: bool) -> Self {
        let mut device = IpQueues::default();
        // `HardwareAddress::Ip` selects `Medium::Ip`: no MAC, no ARP, no neighbour
        // cache — the router above owns all three.
        let iface_config = Config::new(HardwareAddress::Ip);
        let mut iface = Interface::new(iface_config, &mut device, Instant::from_millis(0));
        iface.update_ip_addrs(|addrs| {
            // The gateway's own address, so a socket bound to it works too; the
            // interesting addresses arrive through `set_any_ip`.
            let _ = addrs.push(IpCidr::new(config.gateway.into(), config.prefix_len()));
        });
        // The line that makes a transparent NAT possible: accept datagrams
        // addressed to hosts that are not us.
        iface.set_any_ip(true);
        // …and the line without which `any_ip` does nothing. smoltcp only accepts a
        // foreign destination if a route for it resolves to a router address the
        // interface itself holds (`process_ipv4`: "Rejecting IPv4 packet; no
        // matching routes"), which is how it keeps `any_ip` from turning an
        // interface into a sink for everything on the wire. A default route via our
        // own gateway address satisfies exactly that, and cannot send anything
        // anywhere: this interface's only device is the queue pair above.
        let _ = iface.routes_mut().add_default_ipv4_route(config.gateway);
        Self {
            config,
            iface,
            device,
            sockets: SocketSet::new(Vec::new()),
            flows: Vec::new(),
            epoch: StdInstant::now(),
            allow_host_local,
        }
    }

    fn now(&self) -> Instant {
        Instant::from_micros(i64::try_from(self.epoch.elapsed().as_micros()).unwrap_or(i64::MAX))
    }

    /// True while any flow exists, which is what the pump's sleep length is chosen
    /// from.
    pub fn is_busy(&self) -> bool {
        !self.flows.is_empty()
    }

    pub fn flow_count(&self) -> usize {
        self.flows.len()
    }

    /// Feeds one IPv4 datagram carrying TCP into the stack, opening a flow when it
    /// is a fresh SYN. Reports whether a new flow was opened.
    ///
    /// `datagram` is the whole IPv4 packet, header included: smoltcp's interface
    /// wants what came off the wire, not a re-encoding of it.
    pub fn from_guest(&mut self, ip: &Ipv4Repr, datagram: &[u8], payload: &[u8]) -> bool {
        let opened = self.open_if_new(ip, payload);
        self.device.rx.push_back(datagram.to_vec());
        opened
    }

    /// Opens a flow for a SYN that does not belong to one already.
    ///
    /// A SYN is the only packet that may create state. Anything else for an unknown
    /// flow is handed to smoltcp anyway, which answers it with an RST — the correct
    /// response, and one that costs no host state.
    fn open_if_new(&mut self, ip: &Ipv4Repr, payload: &[u8]) -> bool {
        let Ok(packet) = TcpPacket::new_checked(payload) else {
            return false;
        };
        if !packet.syn() || packet.ack() {
            return false;
        }
        let guest_port = packet.src_port();
        let remote = SocketAddrV4::new(Ipv4Addr::from(ip.dst_addr.octets()), packet.dst_port());
        if self
            .flows
            .iter()
            .any(|flow| flow.guest_port == guest_port && flow.remote == remote)
        {
            // A retransmitted SYN. smoltcp's socket is already listening for it.
            return false;
        }
        // A guest must not be able to reach services bound on the host's own
        // loopback or LAN addresses through the NAT.
        if !self.allow_host_local
            && (self.config.is_local(ip.dst_addr) || ip.dst_addr.is_loopback())
        {
            tracing::debug!(
                remote = %remote,
                "refusing a guest connection to a host-local address"
            );
            return false;
        }
        if self.flows.len() >= MAX_FLOWS {
            tracing::warn!(
                flows = self.flows.len(),
                "refusing a guest connection: the NAT is at its flow limit"
            );
            return false;
        }

        let mut socket = tcp::Socket::new(
            tcp::SocketBuffer::new(vec![0u8; FLOW_BUFFER]),
            tcp::SocketBuffer::new(vec![0u8; FLOW_BUFFER]),
        );
        // Listening on the *destination* the guest chose, which only works because
        // of `set_any_ip`.
        if let Err(error) = socket.listen((ip.dst_addr, packet.dst_port())) {
            tracing::debug!(?error, "cannot listen for a guest connection");
            return false;
        }
        let handle = self.sockets.add(socket);

        // The host connect on its own thread: `std` has no non-blocking connect, and
        // blocking here would stall every other flow.
        let (sender, connecting) = mpsc::channel();
        let target = SocketAddr::V4(remote);
        if let Err(error) = std::thread::Builder::new()
            .name("usernet-connect".into())
            .spawn(move || {
                let result =
                    TcpStream::connect_timeout(&target, CONNECT_TIMEOUT).and_then(|stream| {
                        stream.set_nonblocking(true)?;
                        Ok(stream)
                    });
                // The receiver is gone if the flow was torn down first; that is not
                // an error, it just means nobody is waiting any more.
                let _ = sender.send(result);
            })
        {
            tracing::warn!(%error, "cannot start a host connect thread");
            self.sockets.remove(handle);
            return false;
        }

        tracing::debug!(guest_port, remote = %remote, "opened a NAT flow");
        self.flows.push(Flow {
            handle,
            guest_port,
            remote,
            connecting: Some(connecting),
            stream: None,
            started: StdInstant::now(),
            host_eof: false,
            guest_eof: false,
        });
        true
    }

    /// One pass: move bytes in both directions, run smoltcp, and hand every
    /// datagram it produced to `emit` wrapped in an Ethernet header for `guest_mac`.
    pub fn poll(&mut self, guest_mac: EthernetAddress, emit: &mut dyn FnMut(Vec<u8>)) {
        let now = self.now();
        self.iface.poll(now, &mut self.device, &mut self.sockets);
        self.service_flows();
        // Sockets may have produced data or closed above, so run the stack again
        // rather than leaving those bytes until the next tick.
        self.iface
            .poll(self.now(), &mut self.device, &mut self.sockets);

        while let Some(datagram) = self.device.tx.pop_front() {
            let total = ETH_HEADER_LEN + datagram.len();
            if total > MAX_FRAME_LEN {
                // smoltcp respects the MTU we advertised, so this cannot happen
                // without a host bug; dropping beats handing the device a frame it
                // would reject.
                tracing::error!(len = datagram.len(), "over-long datagram from the NAT");
                continue;
            }
            let mut buf = vec![0u8; total];
            let mut frame = EthernetFrame::new_unchecked(&mut buf[..]);
            EthernetRepr {
                src_addr: UserNetConfig::GATEWAY_MAC,
                dst_addr: guest_mac,
                ethertype: EthernetProtocol::Ipv4,
            }
            .emit(&mut frame);
            frame.payload_mut()[..datagram.len()].copy_from_slice(&datagram);
            emit(buf);
        }
    }

    /// Completes connects, copies bytes both ways, and retires finished flows.
    fn service_flows(&mut self) {
        let mut finished = Vec::new();
        for (index, flow) in self.flows.iter_mut().enumerate() {
            let socket = self.sockets.get_mut::<tcp::Socket>(flow.handle);

            // A connect that has come back, one way or the other.
            if let Some(channel) = &flow.connecting {
                match channel.try_recv() {
                    Ok(Ok(stream)) => {
                        flow.stream = Some(stream);
                        flow.connecting = None;
                    }
                    Ok(Err(error)) => {
                        // The host refused or timed out. `abort` sends the guest an
                        // RST, which is exactly what it would have got from the real
                        // destination.
                        tracing::debug!(remote = %flow.remote, %error, "host connect failed");
                        socket.abort();
                        flow.connecting = None;
                        finished.push(index);
                        continue;
                    }
                    Err(TryRecvError::Empty) => {
                        if flow.started.elapsed() > CONNECT_TIMEOUT + Duration::from_secs(1) {
                            socket.abort();
                            finished.push(index);
                        }
                        continue;
                    }
                    // The connect thread vanished without answering.
                    Err(TryRecvError::Disconnected) => {
                        socket.abort();
                        flow.connecting = None;
                        finished.push(index);
                        continue;
                    }
                }
            }

            let Some(stream) = flow.stream.as_mut() else {
                continue;
            };

            // guest -> host. Only as much as the host accepts: an unwritten byte
            // stays in smoltcp's buffer, which closes the guest's window and is how
            // back-pressure is supposed to work.
            if socket.can_recv() {
                let _ = socket.recv(|data| match stream.write(data) {
                    Ok(written) => (written, ()),
                    Err(error) if error.kind() == ErrorKind::WouldBlock => (0, ()),
                    Err(error) => {
                        tracing::debug!(remote = %flow.remote, %error, "host write failed");
                        (0, ())
                    }
                });
            }

            // host -> guest, bounded by the room in the socket's send buffer.
            if socket.can_send() && !flow.host_eof {
                let room = socket.send_capacity() - socket.send_queue();
                if room > 0 {
                    let mut buf = vec![0u8; room.min(FLOW_BUFFER)];
                    match stream.read(&mut buf) {
                        Ok(0) => flow.host_eof = true,
                        Ok(read) => {
                            let _ = socket.send_slice(&buf[..read]);
                        }
                        Err(error) if error.kind() == ErrorKind::WouldBlock => (),
                        Err(error) => {
                            tracing::debug!(remote = %flow.remote, %error, "host read failed");
                            flow.host_eof = true;
                        }
                    }
                }
            }

            // The guest closed its half (its FIN arrived, so smoltcp is in
            // CloseWait): shut down the host stream's *write* half so the remote
            // sees the end of the request, and keep pumping the other direction
            // until it answers with its own EOF.
            //
            // Without this the flow leaks. Every HTTP client closes first, so the
            // socket sits in CloseWait forever — `is_open()` is still true there
            // — the retirement test below never fires, and the 64 flow slots fill
            // up. Measured: `debian-installer` retrieving its udebs over usernet
            // stalls at "Loading additional components" after exactly 64
            // downloads, with `refusing a guest connection: the NAT is at its
            // flow limit` on the host side. `CloseWait` specifically, not
            // `!may_recv()`: that is also false during the handshake, and
            // shutting the host write half there would truncate the request
            // before it was sent.
            if !flow.guest_eof && socket.state() == tcp::State::CloseWait {
                flow.guest_eof = true;
                if let Err(error) = stream.shutdown(Shutdown::Write) {
                    // Not fatal: an already-dead stream means the flow is about
                    // to be retired anyway, through host_eof or the RST.
                    tracing::debug!(remote = %flow.remote, %error, "host shutdown failed");
                    flow.host_eof = true;
                }
            }
            // The host end is done and everything it sent has been handed to the
            // guest: close our half, which sends the FIN.
            if flow.host_eof && socket.send_queue() == 0 && socket.may_send() {
                socket.close();
            }
            // Both halves are done: smoltcp reports Closed or TimeWait, neither of
            // which is `is_open()`, and the flow's slot goes back.
            if !socket.may_recv() && !socket.is_open() {
                finished.push(index);
            }
        }

        // Back to front, so removing one does not move the next.
        for index in finished.into_iter().rev() {
            if index >= self.flows.len() {
                continue;
            }
            let flow = self.flows.remove(index);
            self.sockets.remove(flow.handle);
            tracing::debug!(remote = %flow.remote, "retired a NAT flow");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use smoltcp::wire::{IpProtocol, Ipv4Address, Ipv4Packet};

    fn config() -> UserNetConfig {
        UserNetConfig::default()
    }

    /// Builds a bare IPv4+TCP datagram with the flags a test needs.
    fn datagram(dst: Ipv4Address, src_port: u16, dst_port: u16, syn: bool, ack: bool) -> Vec<u8> {
        let caps = smoltcp::phy::ChecksumCapabilities::default();
        let tcp_len = 20;
        let ip_repr = Ipv4Repr {
            src_addr: config().guest,
            dst_addr: dst,
            next_header: IpProtocol::Tcp,
            payload_len: tcp_len,
            hop_limit: 64,
        };
        let mut buf = vec![0u8; ip_repr.buffer_len() + tcp_len];
        let mut packet = Ipv4Packet::new_unchecked(&mut buf[..]);
        ip_repr.emit(&mut packet, &caps);
        let mut tcp_packet = TcpPacket::new_unchecked(packet.payload_mut());
        tcp_packet.set_src_port(src_port);
        tcp_packet.set_dst_port(dst_port);
        tcp_packet.set_seq_number(smoltcp::wire::TcpSeqNumber(1));
        tcp_packet.set_ack_number(smoltcp::wire::TcpSeqNumber(0));
        tcp_packet.set_header_len(20);
        tcp_packet.set_syn(syn);
        tcp_packet.set_ack(ack);
        tcp_packet.set_window_len(4096);
        tcp_packet.fill_checksum(&ip_repr.src_addr.into(), &ip_repr.dst_addr.into());
        buf
    }

    fn ip_and_payload(datagram: &[u8]) -> (Ipv4Repr, Vec<u8>) {
        let caps = smoltcp::phy::ChecksumCapabilities::default();
        let packet = Ipv4Packet::new_checked(datagram).expect("a well-formed datagram");
        let ip = Ipv4Repr::parse(&packet, &caps).expect("a parseable header");
        (ip, packet.payload().to_vec())
    }

    /// A SYN to an off-segment address opens exactly one flow, and a retransmission
    /// of it does not open a second.
    #[test]
    fn a_syn_opens_one_flow_and_a_retransmission_does_not() {
        let mut nat = TcpNat::new(config());
        assert!(!nat.is_busy());
        let bytes = datagram(Ipv4Address::new(203, 0, 113, 5), 40000, 80, true, false);
        let (ip, payload) = ip_and_payload(&bytes);

        assert!(nat.from_guest(&ip, &bytes, &payload));
        assert_eq!(nat.flow_count(), 1);
        assert!(nat.is_busy());

        assert!(
            !nat.from_guest(&ip, &bytes, &payload),
            "a retransmitted SYN must not open a second flow"
        );
        assert_eq!(nat.flow_count(), 1);
    }

    /// A packet that is not a SYN never creates state. smoltcp answers it with an
    /// RST, which is the right answer and costs nothing.
    #[test]
    fn a_non_syn_for_an_unknown_flow_opens_nothing() {
        let mut nat = TcpNat::new(config());
        let bytes = datagram(Ipv4Address::new(203, 0, 113, 5), 40001, 80, false, true);
        let (ip, payload) = ip_and_payload(&bytes);
        assert!(!nat.from_guest(&ip, &bytes, &payload));
        assert_eq!(nat.flow_count(), 0);
    }

    /// The guest must not be able to reach the host through the NAT — not the
    /// gateway, not anything else on its own segment, and not loopback.
    #[test]
    fn host_local_destinations_are_refused() {
        let mut nat = TcpNat::new(config());
        for dst in [
            config().gateway,
            Ipv4Address::new(192, 168, 74, 200),
            Ipv4Address::new(127, 0, 0, 1),
        ] {
            let bytes = datagram(dst, 40002, 22, true, false);
            let (ip, payload) = ip_and_payload(&bytes);
            assert!(
                !nat.from_guest(&ip, &bytes, &payload),
                "{dst} must not be reachable from the guest"
            );
        }
        assert_eq!(nat.flow_count(), 0);
    }

    /// A guest opening connections in a loop must not be able to grow host state
    /// past the cap.
    #[test]
    fn the_flow_count_is_capped() {
        let mut nat = TcpNat::new(config());
        for port in 0..(MAX_FLOWS as u16 + 8) {
            let bytes = datagram(
                Ipv4Address::new(203, 0, 113, 5),
                40100 + port,
                80,
                true,
                false,
            );
            let (ip, payload) = ip_and_payload(&bytes);
            nat.from_guest(&ip, &bytes, &payload);
        }
        assert_eq!(nat.flow_count(), MAX_FLOWS);
    }

    /// A minimal guest-side TCP peer: enough to complete a handshake, push a
    /// payload and read one back, using smoltcp's codecs from the *other* end.
    struct GuestPeer {
        nat: TcpNat,
        remote: SocketAddrV4,
        port: u16,
        seq: u32,
        ack: u32,
        mac: EthernetAddress,
    }

    impl GuestPeer {
        fn new(nat: TcpNat, remote: SocketAddrV4, port: u16) -> Self {
            Self {
                nat,
                remote,
                port,
                seq: 1000,
                ack: 0,
                mac: EthernetAddress([0x52, 0x54, 0, 7, 7, 7]),
            }
        }

        /// Sends one segment with the given flags and payload.
        fn send(&mut self, syn: bool, ack_flag: bool, fin: bool, data: &[u8]) {
            let caps = smoltcp::phy::ChecksumCapabilities::default();
            let dst = *self.remote.ip();
            let ip_repr = Ipv4Repr {
                src_addr: self.nat.config.guest,
                dst_addr: dst,
                next_header: IpProtocol::Tcp,
                payload_len: 20 + data.len(),
                hop_limit: 64,
            };
            let mut buf = vec![0u8; ip_repr.buffer_len() + ip_repr.payload_len];
            let mut packet = Ipv4Packet::new_unchecked(&mut buf[..]);
            ip_repr.emit(&mut packet, &caps);
            let mut tcp_packet = TcpPacket::new_unchecked(packet.payload_mut());
            tcp_packet.set_src_port(self.port);
            tcp_packet.set_dst_port(self.remote.port());
            tcp_packet.set_seq_number(smoltcp::wire::TcpSeqNumber(self.seq as i32));
            tcp_packet.set_ack_number(smoltcp::wire::TcpSeqNumber(self.ack as i32));
            tcp_packet.set_header_len(20);
            tcp_packet.set_syn(syn);
            tcp_packet.set_ack(ack_flag);
            tcp_packet.set_fin(fin);
            tcp_packet.set_window_len(16384);
            tcp_packet.payload_mut().copy_from_slice(data);
            tcp_packet.fill_checksum(&ip_repr.src_addr.into(), &dst.into());
            self.seq = self
                .seq
                .wrapping_add(u32::from(syn || fin))
                .wrapping_add(data.len() as u32);

            let (ip, payload) = ip_and_payload(&buf);
            self.nat.from_guest(&ip, &buf, &payload);
        }

        /// Runs the NAT and returns every TCP segment it sent us, updating the
        /// acknowledgement number from the last one.
        fn poll(&mut self) -> Vec<(bool, bool, bool, Vec<u8>)> {
            let mut frames = Vec::new();
            let mac = self.mac;
            self.nat.poll(mac, &mut |frame| frames.push(frame));
            let caps = smoltcp::phy::ChecksumCapabilities::default();
            let mut segments = Vec::new();
            for frame in frames {
                let eth = EthernetFrame::new_checked(&frame[..]).expect("a well-formed frame");
                let packet =
                    Ipv4Packet::new_checked(eth.payload()).expect("a well-formed IPv4 packet");
                let ip = Ipv4Repr::parse(&packet, &caps).expect("a valid IPv4 header");
                let tcp = TcpPacket::new_checked(packet.payload()).expect("a well-formed segment");
                assert!(
                    tcp.verify_checksum(&ip.src_addr.into(), &ip.dst_addr.into()),
                    "the NAT must emit a correct TCP checksum"
                );
                let payload = tcp.payload().to_vec();
                // Acknowledge what we were sent: their SYN counts as one byte.
                self.ack = (tcp.seq_number().0 as u32)
                    .wrapping_add(u32::from(tcp.syn() || tcp.fin()))
                    .wrapping_add(payload.len() as u32);
                segments.push((tcp.syn(), tcp.ack(), tcp.fin(), payload));
            }
            segments
        }
    }

    /// **The whole NAT, end to end**: a guest TCP connection is terminated by
    /// smoltcp, re-made as a host socket, and bytes cross in both directions.
    ///
    /// The listener is on loopback because that is the only address a test can be
    /// sure of binding, which is exactly why the host-local guard has a test-only
    /// escape (see [`TcpNat::allow_host_local`]) — the guard itself is asserted by
    /// `host_local_destinations_are_refused`.
    #[test]
    fn a_guest_connection_reaches_a_host_listener_and_bytes_cross_both_ways() {
        use std::net::TcpListener;

        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("bind a local listener");
        let remote = match listener.local_addr().expect("the listener has an address") {
            SocketAddr::V4(addr) => addr,
            SocketAddr::V6(_) => unreachable!("bound to an IPv4 address"),
        };

        let mut peer = GuestPeer::new(TcpNat::new_allowing_host_local(config()), remote, 41000);

        // SYN -> the listening socket answers SYN-ACK straight away; the host
        // connect is still in flight, deliberately.
        peer.send(true, false, false, &[]);
        let handshake = peer.poll();
        assert!(
            handshake.iter().any(|(syn, ack, _, _)| *syn && *ack),
            "the guest's SYN must be answered with a SYN-ACK: {handshake:?}"
        );
        // ACK completes the guest's half of the handshake.
        peer.send(false, true, false, &[]);
        let _ = peer.poll();

        // The host connect runs on its own thread; `accept` waits for it.
        let (mut stream, _) = listener.accept().expect("the NAT connects to the listener");
        peer.send(false, true, false, b"GET / HTTP/1.0\r\n\r\n");

        // guest -> host. The NAT has to be *polled* for the bytes to move: the copy
        // out of smoltcp's buffer into the host socket happens in `service_flows`,
        // and the flow only has a stream once a poll has collected the connect. So
        // this reads and polls in the same loop instead of blocking on the read —
        // blocking on it means nothing ever copies, which is the shape of a real
        // deadlock and not just a slow test.
        stream
            .set_read_timeout(Some(Duration::from_millis(20)))
            .expect("a read timeout can be set");
        let mut request = Vec::new();
        for _ in 0..200 {
            let _ = peer.poll();
            let mut chunk = [0u8; 64];
            match stream.read(&mut chunk) {
                Ok(0) => break,
                Ok(read) => {
                    request.extend_from_slice(&chunk[..read]);
                    break;
                }
                // A read timeout is `TimedOut` on Windows and `WouldBlock` on unix.
                Err(error)
                    if matches!(error.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {}
                Err(error) => panic!("reading the guest's request failed: {error}"),
            }
            // Keep the guest's side acknowledging, so smoltcp keeps the flow alive.
            peer.send(false, true, false, &[]);
        }
        assert!(
            request.starts_with(b"GET / HTTP/1.0"),
            "the guest's bytes must arrive unaltered: {:?}",
            String::from_utf8_lossy(&request)
        );

        // host -> guest
        stream
            .write_all(b"HTTP/1.0 200 OK\r\n\r\nhi")
            .expect("write a reply");
        stream.flush().expect("flush the reply");
        let mut received = Vec::new();
        for _ in 0..200 {
            for (_, _, _, payload) in peer.poll() {
                received.extend_from_slice(&payload);
            }
            if received.windows(2).any(|w| w == b"hi") {
                break;
            }
            // Keep the guest's side acknowledging, which is what makes smoltcp
            // release the next segment.
            peer.send(false, true, false, &[]);
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(
            received.starts_with(b"HTTP/1.0 200 OK"),
            "the host's reply must reach the guest: {:?}",
            String::from_utf8_lossy(&received)
        );
        assert_eq!(peer.nat.flow_count(), 1, "the flow is still open");
    }

    /// **The flow leak that stalled an installer.** A guest that closes its half
    /// first — which is what every HTTP client does — must get its slot back.
    ///
    /// Before the half-close was propagated, the socket stayed in `CloseWait`
    /// forever: `is_open()` is true there, so the retirement test never fired and
    /// nothing ever shut the host stream's write half, so the host never sent its
    /// own EOF either. The 64 slots then filled up one download at a time.
    /// Measured on a real install: `debian-installer` retrieving its udebs over
    /// usernet stalled at "Loading additional components" with
    /// `refusing a guest connection: the NAT is at its flow limit` on the host.
    #[test]
    fn a_guest_that_closes_first_gets_its_flow_slot_back() {
        use std::net::TcpListener;

        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("bind a local listener");
        let remote = match listener.local_addr().expect("the listener has an address") {
            SocketAddr::V4(addr) => addr,
            SocketAddr::V6(_) => unreachable!("bound to an IPv4 address"),
        };

        let mut peer = GuestPeer::new(TcpNat::new_allowing_host_local(config()), remote, 41100);
        peer.send(true, false, false, &[]);
        let _ = peer.poll();
        peer.send(false, true, false, &[]);
        let _ = peer.poll();
        let (mut stream, _) = listener.accept().expect("the NAT connects to the listener");

        // One request, then the guest closes its half — FIN, the way a client that
        // sent `Connection: close` does.
        peer.send(false, true, false, b"GET / HTTP/1.0\r\n\r\n");
        for _ in 0..50 {
            let _ = peer.poll();
        }
        peer.send(false, true, true, &[]);

        // The host must see EOF on its read side: that is the half-close arriving,
        // and it is what a real server waits for before answering.
        stream
            .set_read_timeout(Some(Duration::from_millis(20)))
            .expect("a read timeout can be set");
        let mut host_saw_eof = false;
        let mut request = Vec::new();
        for _ in 0..400 {
            let _ = peer.poll();
            let mut chunk = [0u8; 128];
            match stream.read(&mut chunk) {
                Ok(0) => {
                    host_saw_eof = true;
                    break;
                }
                Ok(read) => request.extend_from_slice(&chunk[..read]),
                Err(error)
                    if matches!(error.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {}
                Err(error) => panic!("reading the guest's request failed: {error}"),
            }
            std::thread::sleep(Duration::from_millis(2));
        }
        assert!(
            request.starts_with(b"GET / HTTP/1.0"),
            "the request must arrive before the FIN is propagated: {:?}",
            String::from_utf8_lossy(&request)
        );
        assert!(
            host_saw_eof,
            "the guest's FIN must reach the host as end-of-file"
        );

        // The host answers and closes too; the flow must then be retired rather
        // than sitting in CloseWait forever.
        stream
            .write_all(b"HTTP/1.0 200 OK\r\n\r\nbye")
            .expect("reply");
        stream.flush().expect("flush");
        drop(stream);
        for _ in 0..400 {
            let _ = peer.poll();
            if peer.nat.flow_count() == 0 {
                break;
            }
            // The guest keeps acknowledging, which is what lets smoltcp finish the
            // close handshake it started.
            peer.send(false, true, false, &[]);
            std::thread::sleep(Duration::from_millis(2));
        }
        assert_eq!(
            peer.nat.flow_count(),
            0,
            "a closed connection must not hold a flow slot"
        );
    }

    /// Whatever smoltcp emits must come back out as a frame the guest's device
    /// would accept: an Ethernet header naming the gateway and the guest, and
    /// nothing over the MTU.
    #[test]
    fn emitted_frames_are_addressed_to_the_guest() {
        let mut nat = TcpNat::new(config());
        let bytes = datagram(Ipv4Address::new(203, 0, 113, 5), 40200, 80, true, false);
        let (ip, payload) = ip_and_payload(&bytes);
        nat.from_guest(&ip, &bytes, &payload);

        let guest_mac = EthernetAddress([0x52, 0x54, 0, 9, 9, 9]);
        let mut frames = Vec::new();
        nat.poll(guest_mac, &mut |frame| frames.push(frame));
        // The SYN is answered with a SYN-ACK as soon as the socket is listening; the
        // host connect has not finished, which is deliberate — the guest's handshake
        // does not wait for it.
        assert!(
            !frames.is_empty(),
            "the listening socket must answer the SYN"
        );
        for frame in &frames {
            assert!(frame.len() <= MAX_FRAME_LEN);
            let eth = EthernetFrame::new_checked(&frame[..]).expect("a well-formed frame");
            let repr = EthernetRepr::parse(&eth).expect("a parseable header");
            assert_eq!(repr.src_addr, UserNetConfig::GATEWAY_MAC);
            assert_eq!(repr.dst_addr, guest_mac);
            assert_eq!(repr.ethertype, EthernetProtocol::Ipv4);
        }
    }
}
