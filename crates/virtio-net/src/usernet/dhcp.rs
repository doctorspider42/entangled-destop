//! The built-in DHCP server (backlog WHP-1704).
//!
//! One address, one client. That is not a simplification to revisit later — a
//! user-mode NAT serves exactly one guest NIC, so a pool, a lease database and
//! conflict detection would all be machinery with nothing to manage. What *is*
//! needed is the protocol: a guest configures itself with a stock client
//! (`dhclient`, `udhcpc`, `systemd-networkd`, the kernel's own `ip=dhcp`), and
//! every one of them expects the full DISCOVER/OFFER/REQUEST/ACK exchange with the
//! options that make the address usable — netmask, router and DNS.
//!
//! Wire encoding and decoding are `smoltcp::wire::dhcpv4`, i.e. the same codec
//! smoltcp's own DHCP *client* uses, so the two halves cannot disagree about the
//! format.
//!
//! # What a guest cannot make this do
//!
//! Every field in a request is guest-controlled. The server never echoes one back
//! as an address: the offer is always the one this server was built with, fixed at
//! construction from the host's own subnet. A `requested_ip` that is not it is
//! refused with a NAK rather than granted, and a `server_identifier` naming
//! somebody else is ignored, as RFC 2131 §4.3.2 requires — the guest may be
//! talking to a server it imagines exists.

use std::time::{Duration, Instant};

use smoltcp::wire::{DhcpMessageType as MessageType, DhcpRepr, EthernetAddress, Ipv4Address};

/// UDP ports the exchange uses (RFC 2131 §4.1).
pub const SERVER_PORT: u16 = 67;
pub const CLIENT_PORT: u16 = 68;

/// An address handed to a client, and when.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Lease {
    pub mac: EthernetAddress,
    pub ip: Ipv4Address,
    pub granted: Instant,
    pub duration: Duration,
}

impl Lease {
    pub fn expires_at(&self) -> Instant {
        self.granted + self.duration
    }
}

/// The single-address DHCP server.
#[derive(Debug)]
pub struct DhcpServer {
    /// Our own address: the `server_identifier`, the router and the DNS server the
    /// guest is told to use, all the same host-side interface.
    server: Ipv4Address,
    /// The one address on offer.
    offer: Ipv4Address,
    mask: Ipv4Address,
    duration: Duration,
    /// The lease currently out, if any. Kept for diagnostics and so a repeated
    /// REQUEST from the same client is an obvious renewal rather than a new grant.
    lease: Option<Lease>,
    /// How many leases have been granted, ever. The host-side evidence that a
    /// guest configured itself, which is what the acceptance test looks at.
    grants: u64,
}

impl DhcpServer {
    pub fn new(
        server: Ipv4Address,
        offer: Ipv4Address,
        mask: Ipv4Address,
        duration: Duration,
    ) -> Self {
        Self {
            server,
            offer,
            mask,
            duration,
            lease: None,
            grants: 0,
        }
    }

    pub fn lease(&self) -> Option<Lease> {
        self.lease
    }

    pub fn grants(&self) -> u64 {
        self.grants
    }

    /// Answers one DHCP message. `None` means "not ours": a message type only a
    /// server sends, or a REQUEST addressed to a different server.
    ///
    /// The reply carries no `additional_options`, so its lifetime is unrelated to
    /// the request's and the caller can hold it while building the frame.
    pub fn handle(&mut self, request: &DhcpRepr<'_>) -> Option<DhcpRepr<'static>> {
        match request.message_type {
            MessageType::Discover => Some(self.reply(request, MessageType::Offer, self.offer)),
            MessageType::Request => {
                // A REQUEST in the SELECTING state names the server it accepted.
                // One naming somebody else is not a message to answer at all.
                if let Some(chosen) = request.server_identifier {
                    if chosen != self.server {
                        return None;
                    }
                }
                // Anything other than the address we have is refused. `ciaddr` is
                // what a renewing client puts the address in; `requested_ip` is
                // what a selecting one uses.
                let asked = request
                    .requested_ip
                    .filter(|ip| !ip.is_unspecified())
                    .or_else(|| {
                        Some(request.client_ip).filter(|ip: &Ipv4Address| !ip.is_unspecified())
                    });
                match asked {
                    Some(ip) if ip != self.offer => {
                        Some(self.reply(request, MessageType::Nak, Ipv4Address::UNSPECIFIED))
                    }
                    _ => {
                        self.lease = Some(Lease {
                            mac: request.client_hardware_address,
                            ip: self.offer,
                            granted: Instant::now(),
                            duration: self.duration,
                        });
                        self.grants = self.grants.saturating_add(1);
                        Some(self.reply(request, MessageType::Ack, self.offer))
                    }
                }
            }
            // The client gave the address back, or refused it. Either way there is
            // nothing to send, and the address is free again.
            MessageType::Release | MessageType::Decline => {
                self.lease = None;
                None
            }
            // A client that already has an address and only wants the options.
            // Answered with an ACK carrying no `yiaddr`, per RFC 2131 §4.3.5.
            MessageType::Inform => {
                Some(self.reply(request, MessageType::Ack, Ipv4Address::UNSPECIFIED))
            }
            // Offer, Ack, Nak: messages a server sends. A guest sending one is
            // either confused or probing; neither deserves an answer.
            _ => None,
        }
    }

    /// Builds a reply that mirrors the request's transaction id and hardware
    /// address — the two fields a client matches on — and carries the options that
    /// make the address usable.
    fn reply(
        &self,
        request: &DhcpRepr<'_>,
        message_type: MessageType,
        your_ip: Ipv4Address,
    ) -> DhcpRepr<'static> {
        let is_nak = message_type == MessageType::Nak;
        let mut reply = DhcpRepr {
            message_type,
            transaction_id: request.transaction_id,
            secs: 0,
            client_hardware_address: request.client_hardware_address,
            client_ip: Ipv4Address::UNSPECIFIED,
            your_ip,
            server_ip: self.server,
            router: (!is_nak).then_some(self.server),
            subnet_mask: (!is_nak).then_some(self.mask),
            relay_agent_ip: request.relay_agent_ip,
            // Echoed, not decided: a client that asked for a broadcast reply has a
            // reason (it cannot receive unicast before its address is configured).
            broadcast: request.broadcast,
            requested_ip: None,
            client_identifier: None,
            server_identifier: Some(self.server),
            parameter_request_list: None,
            dns_servers: None,
            max_size: None,
            // A NAK carries no timers: there is no lease to time.
            lease_duration: (!is_nak)
                .then_some(self.duration.as_secs().min(u64::from(u32::MAX)) as u32),
            renew_duration: None,
            rebind_duration: None,
            additional_options: &[],
        };
        if !is_nak {
            // The container is `heapless`, which this crate does not depend on
            // directly, so it is produced from the field's own type rather than
            // named. One entry into a three-slot vector cannot overflow.
            let mut servers = reply.dns_servers.take().unwrap_or_default();
            let _ = servers.push(self.server);
            reply.dns_servers = Some(servers);
        }
        reply
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SERVER: Ipv4Address = Ipv4Address::new(192, 168, 74, 1);
    const OFFER: Ipv4Address = Ipv4Address::new(192, 168, 74, 15);
    const MASK: Ipv4Address = Ipv4Address::new(255, 255, 255, 0);
    const CLIENT: EthernetAddress = EthernetAddress([0x52, 0x54, 0, 1, 2, 3]);

    fn server() -> DhcpServer {
        DhcpServer::new(SERVER, OFFER, MASK, Duration::from_secs(3600))
    }

    /// A request as a client builds it: only the fields a client fills in.
    fn request(message_type: MessageType) -> DhcpRepr<'static> {
        DhcpRepr {
            message_type,
            transaction_id: 0xdead_beef,
            secs: 0,
            client_hardware_address: CLIENT,
            client_ip: Ipv4Address::UNSPECIFIED,
            your_ip: Ipv4Address::UNSPECIFIED,
            server_ip: Ipv4Address::UNSPECIFIED,
            router: None,
            subnet_mask: None,
            relay_agent_ip: Ipv4Address::UNSPECIFIED,
            broadcast: true,
            requested_ip: None,
            client_identifier: Some(CLIENT),
            server_identifier: None,
            parameter_request_list: None,
            dns_servers: None,
            max_size: Some(1500),
            lease_duration: None,
            renew_duration: None,
            rebind_duration: None,
            additional_options: &[],
        }
    }

    /// The whole exchange, in the order a client performs it. What the guest needs
    /// out of it is the address *and* the three options without which the address
    /// is useless.
    #[test]
    fn discover_then_request_grants_the_address_with_usable_options() {
        let mut server = server();
        assert_eq!(server.lease(), None);
        assert_eq!(server.grants(), 0);

        let offer = server
            .handle(&request(MessageType::Discover))
            .expect("a DISCOVER must be offered an address");
        assert_eq!(offer.message_type, MessageType::Offer);
        assert_eq!(offer.your_ip, OFFER);
        assert_eq!(offer.transaction_id, 0xdead_beef);
        assert_eq!(offer.client_hardware_address, CLIENT);
        assert_eq!(offer.server_identifier, Some(SERVER));
        assert_eq!(
            offer.router,
            Some(SERVER),
            "without a router there is no default route"
        );
        assert_eq!(offer.subnet_mask, Some(MASK));
        assert_eq!(
            offer.dns_servers.as_ref().map(|d| d.as_slice()),
            Some(&[SERVER][..]),
            "the gateway is also the resolver the guest is told to use"
        );
        assert_eq!(offer.lease_duration, Some(3600));
        // An OFFER is not a grant: nothing is leased until the client asks for it.
        assert_eq!(server.lease(), None);

        let mut selecting = request(MessageType::Request);
        selecting.requested_ip = Some(OFFER);
        selecting.server_identifier = Some(SERVER);
        let ack = server
            .handle(&selecting)
            .expect("a REQUEST for our offer is acknowledged");
        assert_eq!(ack.message_type, MessageType::Ack);
        assert_eq!(ack.your_ip, OFFER);
        assert_eq!(ack.lease_duration, Some(3600));

        let lease = server.lease().expect("the ACK records a lease");
        assert_eq!(lease.mac, CLIENT);
        assert_eq!(lease.ip, OFFER);
        assert!(lease.expires_at() > Instant::now());
        assert_eq!(server.grants(), 1);
    }

    /// A renewal is a REQUEST with the address in `ciaddr` and no server
    /// identifier — the unicast form, which must be acknowledged rather than
    /// NAKed as "a different address".
    #[test]
    fn a_renewal_is_acknowledged() {
        let mut server = server();
        let mut renew = request(MessageType::Request);
        renew.client_ip = OFFER;
        renew.broadcast = false;
        let ack = server.handle(&renew).expect("a renewal is acknowledged");
        assert_eq!(ack.message_type, MessageType::Ack);
        assert_eq!(ack.your_ip, OFFER);
        assert!(!ack.broadcast, "the broadcast flag is echoed, not invented");
    }

    /// A guest asking for an address we do not have must be told no, not handed
    /// the address it asked for. This is the one place a guest-controlled field
    /// could otherwise become the host's answer.
    #[test]
    fn a_request_for_another_address_is_refused() {
        let mut server = server();
        let mut wrong = request(MessageType::Request);
        wrong.requested_ip = Some(Ipv4Address::new(10, 0, 0, 1));
        let nak = server.handle(&wrong).expect("a wrong address is NAKed");
        assert_eq!(nak.message_type, MessageType::Nak);
        assert_eq!(nak.your_ip, Ipv4Address::UNSPECIFIED);
        assert_eq!(nak.router, None, "a NAK carries no configuration");
        assert_eq!(nak.subnet_mask, None);
        assert_eq!(nak.lease_duration, None);
        assert_eq!(server.lease(), None);
        assert_eq!(server.grants(), 0);
    }

    /// A REQUEST that selected a *different* server is not ours to answer;
    /// answering it would break a network with two servers on it (RFC 2131
    /// §4.3.2).
    #[test]
    fn a_request_naming_another_server_is_ignored() {
        let mut server = server();
        let mut elsewhere = request(MessageType::Request);
        elsewhere.requested_ip = Some(OFFER);
        elsewhere.server_identifier = Some(Ipv4Address::new(192, 168, 74, 9));
        assert!(server.handle(&elsewhere).is_none());
        assert_eq!(server.lease(), None);
    }

    #[test]
    fn release_frees_the_address_and_is_not_answered() {
        let mut server = server();
        let mut selecting = request(MessageType::Request);
        selecting.requested_ip = Some(OFFER);
        assert!(server.handle(&selecting).is_some());
        assert!(server.lease().is_some());

        assert!(server.handle(&request(MessageType::Release)).is_none());
        assert_eq!(server.lease(), None);
        // The grant counter is a total, not a gauge: the guest did configure itself.
        assert_eq!(server.grants(), 1);
    }

    /// Messages only a server sends must not be answered — otherwise two of these
    /// pointed at each other would talk forever.
    #[test]
    fn server_side_message_types_are_ignored() {
        let mut server = server();
        for kind in [MessageType::Offer, MessageType::Ack, MessageType::Nak] {
            assert!(server.handle(&request(kind)).is_none(), "{kind:?}");
        }
    }

    /// An INFORM asks for options only, so the ACK must not hand out an address —
    /// a client that already has one would see it as a conflict.
    #[test]
    fn inform_is_answered_without_an_address() {
        let mut server = server();
        let ack = server
            .handle(&request(MessageType::Inform))
            .expect("an INFORM is answered");
        assert_eq!(ack.message_type, MessageType::Ack);
        assert_eq!(ack.your_ip, Ipv4Address::UNSPECIFIED);
        assert_eq!(ack.router, Some(SERVER));
        assert_eq!(server.lease(), None);
    }
}
