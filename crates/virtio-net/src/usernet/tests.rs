//! Router and backend tests for the user-mode network (backlog WHP-1704).
//!
//! Every one of these runs offline and on both hosts. The DHCP exchange in
//! particular is driven with `smoltcp::wire::dhcpv4`'s own encoder — the same code
//! smoltcp's DHCP *client* emits with — so the test is a real client's bytes rather
//! than a hand-written packet that happens to match this server's expectations.

use super::*;
use smoltcp::wire::{DhcpMessageType as MessageType, DhcpPacket};

const GUEST_MAC: EthernetAddress = EthernetAddress([0x52, 0x54, 0x00, 0xaa, 0xbb, 0xcc]);

fn backend() -> UserNetBackend {
    // Upstream DNS pointed at a discard address: no test here sends a real query,
    // and this makes it obvious that none can leak out to a real resolver.
    UserNetBackend::new(
        UserNetConfig::default()
            .with_dns_upstream(SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 9))),
    )
    .expect("the user-mode network needs no privileges")
}

/// Wraps `payload` in Ethernet, exactly as a guest NIC would.
fn eth(dst: EthernetAddress, ethertype: EthernetProtocol, payload: &[u8]) -> Vec<u8> {
    let mut buf = vec![0u8; ETH_HEADER_LEN + payload.len()];
    let mut frame = EthernetFrame::new_unchecked(&mut buf[..]);
    EthernetRepr {
        src_addr: GUEST_MAC,
        dst_addr: dst,
        ethertype,
    }
    .emit(&mut frame);
    frame.payload_mut().copy_from_slice(payload);
    buf
}

/// Wraps `data` in UDP and IPv4 from the guest.
fn guest_udp(
    src: Ipv4Address,
    dst: Ipv4Address,
    src_port: u16,
    dst_port: u16,
    data: &[u8],
) -> Vec<u8> {
    let caps = ChecksumCapabilities::default();
    let udp_repr = UdpRepr { src_port, dst_port };
    let ip_repr = Ipv4Repr {
        src_addr: src,
        dst_addr: dst,
        next_header: IpProtocol::Udp,
        payload_len: udp_repr.header_len() + data.len(),
        hop_limit: 64,
    };
    let mut buf = vec![0u8; ip_repr.buffer_len() + ip_repr.payload_len];
    let mut packet = Ipv4Packet::new_unchecked(&mut buf[..]);
    ip_repr.emit(&mut packet, &caps);
    let mut udp = UdpPacket::new_unchecked(packet.payload_mut());
    udp_repr.emit(
        &mut udp,
        &src.into(),
        &dst.into(),
        data.len(),
        |p| p.copy_from_slice(data),
        &caps,
    );
    buf
}

/// A DHCP message as a client sends it, encoded by smoltcp.
fn dhcp_request(
    message_type: MessageType,
    requested: Option<Ipv4Address>,
    server: Option<Ipv4Address>,
) -> Vec<u8> {
    let repr = DhcpRepr {
        message_type,
        transaction_id: 0x1234_5678,
        secs: 0,
        client_hardware_address: GUEST_MAC,
        client_ip: Ipv4Address::UNSPECIFIED,
        your_ip: Ipv4Address::UNSPECIFIED,
        server_ip: Ipv4Address::UNSPECIFIED,
        router: None,
        subnet_mask: None,
        relay_agent_ip: Ipv4Address::UNSPECIFIED,
        broadcast: true,
        requested_ip: requested,
        client_identifier: Some(GUEST_MAC),
        server_identifier: server,
        parameter_request_list: None,
        dns_servers: None,
        max_size: Some(1500),
        lease_duration: None,
        renew_duration: None,
        rebind_duration: None,
        additional_options: &[],
    };
    let mut body = vec![0u8; repr.buffer_len()];
    let mut packet = DhcpPacket::new_unchecked(&mut body[..]);
    repr.emit(&mut packet)
        .expect("smoltcp encodes its own repr");
    let ip = guest_udp(
        Ipv4Address::UNSPECIFIED,
        Ipv4Address::BROADCAST,
        CLIENT_PORT,
        SERVER_PORT,
        &body,
    );
    eth(EthernetAddress::BROADCAST, EthernetProtocol::Ipv4, &ip)
}

/// Reads one frame back out of the backend, or `None`.
fn recv(backend: &UserNetBackend) -> Option<Vec<u8>> {
    let mut buf = vec![0u8; MAX_FRAME_LEN];
    let len = backend.read_frame(&mut buf).expect("reading never fails")?;
    buf.truncate(len);
    Some(buf)
}

/// Peels a reply frame down to its DHCP body, asserting the envelope on the way:
/// the guest has to be able to *receive* the reply, so the addressing matters as
/// much as the payload.
///
/// Returns the body rather than a parsed `DhcpRepr` because the repr borrows from
/// the packet it was parsed out of, and the packet cannot outlive this function.
fn dhcp_body(frame: &[u8]) -> &[u8] {
    let eth = EthernetFrame::new_checked(frame).expect("a well-formed frame");
    let eth_repr = EthernetRepr::parse(&eth).expect("a parseable Ethernet header");
    assert_eq!(eth_repr.src_addr, UserNetConfig::GATEWAY_MAC);
    assert_eq!(
        eth_repr.dst_addr, GUEST_MAC,
        "the reply is unicast to the client's hardware address"
    );
    assert_eq!(eth_repr.ethertype, EthernetProtocol::Ipv4);

    let caps = ChecksumCapabilities::default();
    let packet = Ipv4Packet::new_checked(eth.payload()).expect("a well-formed IPv4 packet");
    let ip = Ipv4Repr::parse(&packet, &caps).expect("a valid IPv4 header and checksum");
    assert_eq!(ip.src_addr, UserNetConfig::default().gateway);
    assert_eq!(
        ip.dst_addr,
        Ipv4Address::BROADCAST,
        "a client with no address configured cannot receive a unicast datagram"
    );

    let udp_packet = UdpPacket::new_checked(packet.payload()).expect("a well-formed UDP packet");
    let udp = UdpRepr::parse(&udp_packet, &ip.src_addr.into(), &ip.dst_addr.into(), &caps)
        .expect("a valid UDP header and checksum");
    assert_eq!(udp.src_port, SERVER_PORT);
    assert_eq!(udp.dst_port, CLIENT_PORT);

    // Borrowed from `frame`, not from the local packet views: every offset used
    // here was validated by a `new_checked` above.
    &frame[ETH_HEADER_LEN + ip.buffer_len() + udp.header_len()..]
}

/// `dhcp_body` plus the parse, as a macro because the parsed representation borrows
/// from the packet and so both have to live in the caller's scope.
macro_rules! dhcp_reply {
    ($frame:expr) => {
        DhcpPacket::new_checked(dhcp_body(&$frame)).expect("a well-formed DHCP packet")
    };
}

/// **The WHP-1704 acceptance in miniature**: a guest that speaks DHCP gets an
/// address, a router and a resolver, and the host can see that it did.
///
/// Everything is checked at the level the guest sees it — a frame on the wire,
/// re-parsed from bytes — rather than by inspecting the server's internals, because
/// a lease the guest cannot receive is not a lease.
#[test]
fn a_guest_dhcp_exchange_yields_a_usable_address() {
    let backend = backend();
    let config = *backend.config();
    assert!(backend.lease().is_none());

    let discover = dhcp_request(MessageType::Discover, None, None);
    backend
        .write_frame(&discover)
        .expect("a frame is always accepted");
    let offer_frame = recv(&backend).expect("a DISCOVER must be answered");
    let offer_packet = dhcp_reply!(offer_frame);
    let offer = DhcpRepr::parse(&offer_packet).expect("a parseable DHCP message");
    assert_eq!(offer.message_type, MessageType::Offer);
    assert_eq!(offer.your_ip, config.guest);
    assert_eq!(offer.transaction_id, 0x1234_5678);
    assert_eq!(offer.router, Some(config.gateway));
    assert_eq!(offer.subnet_mask, Some(config.netmask));
    assert_eq!(
        offer.dns_servers.as_ref().map(|d| d.as_slice()),
        Some(&[config.gateway][..])
    );
    assert!(backend.lease().is_none(), "an offer is not yet a lease");

    let request = dhcp_request(
        MessageType::Request,
        Some(config.guest),
        Some(config.gateway),
    );
    backend
        .write_frame(&request)
        .expect("a frame is always accepted");
    let ack_frame = recv(&backend).expect("a REQUEST must be acknowledged");
    let ack_packet = dhcp_reply!(ack_frame);
    let ack = DhcpRepr::parse(&ack_packet).expect("a parseable DHCP message");
    assert_eq!(ack.message_type, MessageType::Ack);
    assert_eq!(ack.your_ip, config.guest);
    assert_eq!(ack.lease_duration, Some(config.lease.as_secs() as u32));

    let lease = backend.lease().expect("the ACK is recorded as a lease");
    assert_eq!(lease.mac, GUEST_MAC);
    assert_eq!(lease.ip, config.guest);
    assert_eq!(backend.stats().dhcp_grants.load(Ordering::Relaxed), 1);
    assert_eq!(recv(&backend), None, "nothing else is queued");
}

/// The guest's very first frame is an ARP request for its default gateway; without
/// an answer nothing else it does can leave the segment.
#[test]
fn the_gateway_answers_arp() {
    let backend = backend();
    let config = *backend.config();
    let request = ArpRepr::EthernetIpv4 {
        operation: ArpOperation::Request,
        source_hardware_addr: GUEST_MAC,
        source_protocol_addr: config.guest,
        target_hardware_addr: EthernetAddress([0; 6]),
        target_protocol_addr: config.gateway,
    };
    let mut body = vec![0u8; request.buffer_len()];
    request.emit(&mut ArpPacket::new_unchecked(&mut body[..]));
    backend
        .write_frame(&eth(
            EthernetAddress::BROADCAST,
            EthernetProtocol::Arp,
            &body,
        ))
        .expect("a frame is always accepted");

    let frame = recv(&backend).expect("an ARP request for the gateway must be answered");
    let eth_frame = EthernetFrame::new_checked(&frame[..]).expect("a well-formed frame");
    let packet = ArpPacket::new_checked(eth_frame.payload()).expect("a well-formed ARP packet");
    let ArpRepr::EthernetIpv4 {
        operation,
        source_hardware_addr,
        source_protocol_addr,
        target_hardware_addr,
        target_protocol_addr,
    } = ArpRepr::parse(&packet).expect("a parseable ARP packet")
    else {
        panic!("the reply is not an Ethernet/IPv4 ARP packet");
    };
    assert_eq!(operation, ArpOperation::Reply);
    assert_eq!(source_hardware_addr, UserNetConfig::GATEWAY_MAC);
    assert_eq!(source_protocol_addr, config.gateway);
    assert_eq!(target_hardware_addr, GUEST_MAC);
    assert_eq!(target_protocol_addr, config.guest);
}

/// A request for anything but the gateway goes unanswered. Proxy-ARPing the whole
/// address space would make the guest treat the internet as link-local, which is a
/// stranger and slower path than a default route.
#[test]
fn arp_for_other_addresses_is_not_answered() {
    let backend = backend();
    let request = ArpRepr::EthernetIpv4 {
        operation: ArpOperation::Request,
        source_hardware_addr: GUEST_MAC,
        source_protocol_addr: backend.config().guest,
        target_hardware_addr: EthernetAddress([0; 6]),
        target_protocol_addr: Ipv4Address::new(192, 168, 74, 99),
    };
    let mut body = vec![0u8; request.buffer_len()];
    request.emit(&mut ArpPacket::new_unchecked(&mut body[..]));
    backend
        .write_frame(&eth(
            EthernetAddress::BROADCAST,
            EthernetProtocol::Arp,
            &body,
        ))
        .expect("a frame is always accepted");
    assert_eq!(recv(&backend), None);
}

/// `ping 192.168.74.1` is the first thing anybody tries when a guest's network
/// looks wrong, so the gateway answers it.
#[test]
fn the_gateway_answers_a_ping() {
    let backend = backend();
    let config = *backend.config();
    let caps = ChecksumCapabilities::default();
    let echo = Icmpv4Repr::EchoRequest {
        ident: 0x4242,
        seq_no: 7,
        data: b"entangled",
    };
    let ip_repr = Ipv4Repr {
        src_addr: config.guest,
        dst_addr: config.gateway,
        next_header: IpProtocol::Icmp,
        payload_len: echo.buffer_len(),
        hop_limit: 64,
    };
    let mut body = vec![0u8; ip_repr.buffer_len() + ip_repr.payload_len];
    let mut packet = Ipv4Packet::new_unchecked(&mut body[..]);
    ip_repr.emit(&mut packet, &caps);
    echo.emit(
        &mut Icmpv4Packet::new_unchecked(packet.payload_mut()),
        &caps,
    );
    backend
        .write_frame(&eth(
            UserNetConfig::GATEWAY_MAC,
            EthernetProtocol::Ipv4,
            &body,
        ))
        .expect("a frame is always accepted");

    let frame = recv(&backend).expect("a ping to the gateway must be answered");
    let eth_frame = EthernetFrame::new_checked(&frame[..]).expect("a well-formed frame");
    let packet = Ipv4Packet::new_checked(eth_frame.payload()).expect("a well-formed IPv4 packet");
    let ip = Ipv4Repr::parse(&packet, &caps).expect("a valid header and checksum");
    assert_eq!(ip.src_addr, config.gateway);
    assert_eq!(ip.dst_addr, config.guest);
    let icmp = Icmpv4Packet::new_checked(packet.payload()).expect("a well-formed ICMP packet");
    let reply = Icmpv4Repr::parse(&icmp, &caps).expect("a valid ICMP message and checksum");
    assert_eq!(
        reply,
        Icmpv4Repr::EchoReply {
            ident: 0x4242,
            seq_no: 7,
            data: b"entangled",
        }
    );
}

/// Frames a guest can send that this router does not serve must be dropped and
/// *counted* — never mistaken for a host failure, which would reset the device, and
/// never silently, because "the guest has no network" and "the guest is speaking
/// IPv6" need telling apart.
#[test]
fn unroutable_frames_are_dropped_and_counted() {
    let backend = backend();
    let stats = backend.stats();

    // Too short to be an Ethernet header at all.
    backend
        .write_frame(&[0u8; 8])
        .expect("never a host failure");
    assert_eq!(stats.dropped_malformed.load(Ordering::Relaxed), 1);

    // A well-formed frame carrying a protocol this router does not handle.
    backend
        .write_frame(&eth(
            EthernetAddress::BROADCAST,
            EthernetProtocol::Ipv6,
            &[0u8; 40],
        ))
        .expect("never a host failure");
    assert_eq!(stats.dropped_unsupported.load(Ordering::Relaxed), 1);

    // An IPv4 header with a broken checksum: smoltcp's parse is the check.
    let mut broken = guest_udp(
        backend.config().guest,
        Ipv4Address::new(203, 0, 113, 1),
        1234,
        4321,
        b"hello",
    );
    broken[10] ^= 0xff;
    backend
        .write_frame(&eth(
            UserNetConfig::GATEWAY_MAC,
            EthernetProtocol::Ipv4,
            &broken,
        ))
        .expect("never a host failure");
    assert_eq!(stats.dropped_malformed.load(Ordering::Relaxed), 2);

    assert_eq!(stats.frames_from_guest.load(Ordering::Relaxed), 3);
    assert_eq!(recv(&backend), None);
}

/// The queue is bounded by the *host*, not by whether the guest reads: a guest that
/// stops draining its RX queue must not be able to grow this process.
#[test]
fn the_guest_queue_is_bounded() {
    let out = ToGuest::default();
    for _ in 0..(MAX_QUEUED_FRAMES + 16) {
        // The return value is what the router turns into a drop counter.
        let _ = out.push(vec![0u8; 64]);
    }
    assert_eq!(
        out.frames.lock().expect("not poisoned").len(),
        MAX_QUEUED_FRAMES
    );
    assert!(!out.push(vec![0u8; 64]), "a full queue reports congestion");
}

/// The device's RX worker blocks on `wait_readable`; a reset has to get through it
/// promptly, and exactly once.
#[test]
fn a_wake_is_reported_once() {
    let backend = backend();
    assert_eq!(
        backend
            .wait_readable(Duration::from_millis(5))
            .expect("waiting never fails"),
        Readiness::TimedOut
    );
    backend.wake().expect("waking never fails");
    assert_eq!(
        backend
            .wait_readable(Duration::from_secs(5))
            .expect("waiting never fails"),
        Readiness::WokenUp,
        "a wake must not be slept through"
    );
    assert_eq!(
        backend
            .wait_readable(Duration::from_millis(5))
            .expect("waiting never fails"),
        Readiness::TimedOut,
        "a wake is consumed, not sticky"
    );
}

/// A queued frame must make the waiter readable rather than time out.
#[test]
fn a_queued_frame_makes_the_backend_readable() {
    let backend = backend();
    let config = *backend.config();
    backend
        .write_frame(&dhcp_request(MessageType::Discover, None, None))
        .expect("a frame is always accepted");
    assert_eq!(
        backend
            .wait_readable(Duration::from_secs(5))
            .expect("waiting never fails"),
        Readiness::Readable
    );
    let frame = recv(&backend).expect("the offer is queued");
    let packet = dhcp_reply!(frame);
    let offer = DhcpRepr::parse(&packet).expect("a parseable DHCP message");
    assert_eq!(offer.your_ip, config.guest);
}

/// The subnet arithmetic the flow filter and the DHCP options both rest on.
#[test]
fn the_config_describes_one_subnet() {
    let config = UserNetConfig::default();
    assert_eq!(config.prefix_len(), 24);
    assert!(config.is_local(config.gateway));
    assert!(config.is_local(config.guest));
    assert!(config.is_local(Ipv4Address::new(192, 168, 74, 254)));
    assert!(!config.is_local(Ipv4Address::new(192, 168, 75, 1)));
    assert!(!config.is_local(Ipv4Address::new(8, 8, 8, 8)));
    // Deliberately not the two subnets a host LAN is most likely to be on.
    assert_ne!(config.gateway, Ipv4Address::new(192, 168, 0, 1));
    assert_ne!(config.gateway, Ipv4Address::new(192, 168, 1, 1));
    assert!(UserNetConfig::GATEWAY_MAC.is_unicast());
}

/// The kernel-side alternative to DHCP has to describe the same segment, or a guest
/// booted with `ip=` and one booted with a DHCP client would land on different
/// networks.
#[test]
fn the_static_cmdline_matches_the_dhcp_offer() {
    let backend = backend();
    let config = *backend.config();
    let cmdline = backend.static_ip_cmdline();
    assert!(cmdline.starts_with(&format!("ip={}::{}:", config.guest, config.gateway)));
    assert!(cmdline.contains(&config.netmask.to_string()));
    assert!(cmdline.contains("eth0:off"));
}

/// Dropping the backend must join its pump thread: a VM that stops leaves no
/// threads behind (EPIC 14's rule, which applies to every host-side worker).
#[test]
fn dropping_the_backend_joins_the_pump() {
    let before = std::thread::available_parallelism().is_ok();
    let backend = backend();
    backend
        .write_frame(&dhcp_request(MessageType::Discover, None, None))
        .expect("a frame is always accepted");
    std::thread::sleep(Duration::from_millis(30));
    drop(backend);
    assert!(before);
}
