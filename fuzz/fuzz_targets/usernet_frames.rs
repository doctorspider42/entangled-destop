//! Fuzzes the user-mode NAT's receive path with arbitrary guest frames
//! (backlog MVP-1402, WHP-1704).
//!
//! `usernet` is the whole network on Windows and the rootless one on Linux, and
//! every byte it parses comes from the guest: Ethernet, ARP, IPv4, ICMPv4, UDP,
//! DHCP and — through `smoltcp`'s interface — a complete TCP state machine with
//! sequence numbers, windows and reassembly. That is by some distance the widest
//! untrusted-input surface in the tree, and it is the one surface that had no
//! fuzz target.
//!
//! The harness is [`OfflineNet`]: the real router, the real parsers, the real
//! flow table and the real smoltcp interface, with **no host sockets** — a
//! fuzzer that could open connections would dial arbitrary internet addresses at
//! libFuzzer speed. The DNS relay's own socket is pointed at the discard port on
//! loopback for the same reason.
//!
//! Three input shapes, because a fuzzer that only ever sees raw bytes never gets
//! past the first length check:
//!
//! * `Raw` — bytes straight at `write_frame`'s parser;
//! * `Eth` — a well-formed Ethernet header with an arbitrary ethertype and body,
//!   which reaches the ARP and IPv4 decoders;
//! * `Ipv4` — a well-formed IPv4 header (checksum fixed) with an arbitrary
//!   protocol and body, optionally with the transport checksum fixed up too,
//!   which is what reaches ICMP, DHCP, the DNS relay and the TCP stack.
//!
//! Properties checked after every operation:
//!
//! * no panic anywhere — the whole point, and the untrusted-guest rule;
//! * the flow table never exceeds `MAX_FLOWS`, however many SYNs arrive;
//! * the queue of frames for the guest never exceeds `MAX_QUEUED_FRAMES`, so a
//!   guest that stops reading cannot grow this process;
//! * every frame the router emits is a well-formed Ethernet frame within the
//!   MTU, addressed *from* the gateway — a guest must never be handed a frame
//!   its own device would drop, and never one claiming to come from itself.

#![no_main]

use arbitrary::Arbitrary;
use libfuzzer_sys::fuzz_target;
use virtio_net::usernet::offline::OfflineNet;
use virtio_net::usernet::{UserNetConfig, MAX_FLOWS, MAX_QUEUED_FRAMES};
use virtio_net::{ETH_HEADER_LEN, MAX_FRAME_LEN};

/// The guest's own MAC, so the router learns a unicast address and can address
/// replies — without one it silently drops everything it would have answered.
const GUEST_MAC: [u8; 6] = [0x52, 0x54, 0x00, 0xaa, 0xbb, 0xcc];

#[derive(Arbitrary, Debug)]
enum Op {
    /// Raw bytes, exactly as `NetBackend::write_frame` receives them.
    Raw(Vec<u8>),
    /// An Ethernet frame with a chosen ethertype.
    Eth { ethertype: u16, payload: Vec<u8> },
    /// An IPv4 datagram, header checksum always correct so the fuzzer's effort
    /// goes into the payload rather than into rediscovering a checksum.
    Ipv4 {
        protocol: u8,
        /// Low byte of the destination; the rest is fixed so the fuzzer chooses
        /// between the gateway, the guest's own segment and an off-segment host
        /// without wasting bits on 2^32 addresses.
        dst: Dst,
        /// Fix the TCP/UDP checksum too. Without this almost nothing reaches the
        /// transport layer; with it, always, the parsers below never see a
        /// corrupt one.
        fix_transport_checksum: bool,
        payload: Vec<u8>,
    },
    /// Run the host side and take whatever came back.
    Poll,
}

#[derive(Arbitrary, Debug)]
enum Dst {
    Gateway,
    Guest,
    OnSegment(u8),
    OffSegment(u8),
}

fuzz_target!(|ops: Vec<Op>| {
    let config = UserNetConfig::default();
    let mut net = OfflineNet::new();
    for op in ops.iter().take(64) {
        let emitted = match op {
            Op::Raw(bytes) => {
                net.feed(bytes);
                net.drain()
            }
            Op::Eth { ethertype, payload } => {
                net.feed(&ethernet(*ethertype, payload));
                net.drain()
            }
            Op::Ipv4 {
                protocol,
                dst,
                fix_transport_checksum,
                payload,
            } => {
                let src = config.guest.octets();
                let dst = match dst {
                    Dst::Gateway => config.gateway.octets(),
                    Dst::Guest => config.guest.octets(),
                    Dst::OnSegment(host) => [192, 168, 74, *host],
                    Dst::OffSegment(host) => [203, 0, 113, *host],
                };
                let datagram = ipv4(*protocol, src, dst, payload, *fix_transport_checksum);
                net.feed(&ethernet(0x0800, &datagram));
                net.drain()
            }
            Op::Poll => net.poll(),
        };

        assert!(
            net.flow_count() <= MAX_FLOWS,
            "the flow table grew past its bound: {}",
            net.flow_count()
        );
        assert!(
            emitted.len() <= MAX_QUEUED_FRAMES,
            "more frames were queued for the guest than the bound allows: {}",
            emitted.len()
        );
        for frame in &emitted {
            assert!(
                frame.len() >= ETH_HEADER_LEN && frame.len() <= MAX_FRAME_LEN,
                "the router emitted a frame of {} bytes",
                frame.len()
            );
            assert_eq!(
                &frame[6..12],
                &UserNetConfig::GATEWAY_MAC.0[..],
                "every frame the router emits comes from the gateway"
            );
        }
    }
});

/// An Ethernet frame from the guest to the gateway.
fn ethernet(ethertype: u16, payload: &[u8]) -> Vec<u8> {
    let mut frame = Vec::with_capacity(ETH_HEADER_LEN + payload.len());
    frame.extend_from_slice(&UserNetConfig::GATEWAY_MAC.0);
    frame.extend_from_slice(&GUEST_MAC);
    frame.extend_from_slice(&ethertype.to_be_bytes());
    frame.extend_from_slice(payload);
    frame
}

/// An IPv4 datagram with a correct header checksum, and optionally a correct
/// transport checksum for TCP (protocol 6) or UDP (17).
///
/// Written out here rather than built with `smoltcp::wire` on purpose: a header
/// this target hands to smoltcp should not have been produced by smoltcp.
fn ipv4(protocol: u8, src: [u8; 4], dst: [u8; 4], payload: &[u8], fix_transport: bool) -> Vec<u8> {
    // The header's total-length field is 16 bits; anything longer is not a
    // datagram this function can describe, and the frame cap is far below it.
    let payload = &payload[..payload.len().min(MAX_FRAME_LEN)];
    let total = 20 + payload.len();
    let mut datagram = vec![0u8; total];
    datagram[0] = 0x45;
    datagram[2..4].copy_from_slice(&(total as u16).to_be_bytes());
    datagram[8] = 64;
    datagram[9] = protocol;
    datagram[12..16].copy_from_slice(&src);
    datagram[16..20].copy_from_slice(&dst);
    let header_checksum = checksum(&[&datagram[..20]]);
    datagram[10..12].copy_from_slice(&header_checksum.to_be_bytes());
    datagram[20..].copy_from_slice(payload);

    // The transport checksum covers a pseudo-header of the two addresses, the
    // protocol and the transport length, plus the transport bytes themselves.
    let offset = match (fix_transport, protocol) {
        (true, 6) if payload.len() >= 20 => Some(16),
        (true, 17) if payload.len() >= 8 => Some(6),
        _ => None,
    };
    if let Some(offset) = offset {
        let length = (payload.len() as u16).to_be_bytes();
        let pseudo = [
            src[0], src[1], src[2], src[3], dst[0], dst[1], dst[2], dst[3], 0, protocol, length[0],
            length[1],
        ];
        datagram[20 + offset..22 + offset].copy_from_slice(&[0, 0]);
        let sum = checksum(&[&pseudo, &datagram[20..]]);
        datagram[20 + offset..22 + offset].copy_from_slice(&sum.to_be_bytes());
    }
    datagram
}

/// The internet checksum (RFC 1071) over a sequence of byte runs.
fn checksum(parts: &[&[u8]]) -> u16 {
    let mut sum: u32 = 0;
    // The runs are concatenated logically, so an odd-length run pairs its last
    // byte with the first of the next one — which is why this walks a joined
    // iterator rather than each run on its own.
    let mut pending: Option<u8> = None;
    for part in parts {
        for &byte in *part {
            match pending.take() {
                None => pending = Some(byte),
                Some(high) => sum += u32::from(u16::from_be_bytes([high, byte])),
            }
        }
    }
    if let Some(high) = pending {
        sum += u32::from(u16::from_be_bytes([high, 0]));
    }
    while sum >> 16 != 0 {
        sum = (sum & 0xffff) + (sum >> 16);
    }
    !(sum as u16)
}
