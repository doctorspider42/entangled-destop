//! Router lifecycle metadata, not host sockets. A resumed guest still has its
//! MAC, DHCP lease and old TCP tuples; forgetting these causes inbound stalls.
use super::*;

const MAGIC: &[u8; 8] = b"ENTGLNET";
const VERSION: u32 = 1;
const BYTES: usize = 24;

fn encode(mac: Option<EthernetAddress>, port: u16) -> Vec<u8> {
    let mut bytes = [0; BYTES];
    bytes[..8].copy_from_slice(MAGIC);
    bytes[8..12].copy_from_slice(&VERSION.to_le_bytes());
    bytes[12..14].copy_from_slice(&port.to_le_bytes());
    if let Some(mac) = mac {
        bytes[14] = 1;
        bytes[16..22].copy_from_slice(&mac.0);
    }
    bytes.to_vec()
}

fn decode(bytes: &[u8]) -> Result<(Option<EthernetAddress>, u16), NetError> {
    let invalid = || NetError::Checkpoint("router metadata framing, MAC or port differs".into());
    if bytes.len() != BYTES
        || &bytes[..8] != MAGIC
        || bytes[8..12] != VERSION.to_le_bytes()
        || bytes[14] > 1
        || bytes[15] != 0
        || bytes[22..24] != [0; 2]
    {
        return Err(invalid());
    }
    let port = u16::from_le_bytes([bytes[12], bytes[13]]);
    if port < 49152 {
        return Err(invalid());
    }
    let mac = EthernetAddress([
        bytes[16], bytes[17], bytes[18], bytes[19], bytes[20], bytes[21],
    ]);
    let mac = if bytes[14] == 0 {
        if mac.0 != [0; 6] {
            return Err(invalid());
        }
        None
    } else {
        if !mac.is_unicast() {
            return Err(invalid());
        }
        Some(mac)
    };
    Ok((mac, port))
}

impl UserNetBackend {
    pub(super) fn save_checkpoint(&self) -> Vec<u8> {
        let router = self
            .router
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        encode(router.guest_mac, router.tcp.checkpoint_inbound_port())
    }

    pub(super) fn load_checkpoint(&self, bytes: &[u8]) -> Result<(), NetError> {
        if bytes.is_empty() {
            return Ok(());
        }
        let (mac, port) = decode(bytes)?;
        let mut router = self
            .router
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        router.tcp.restore_inbound_port(port)?;
        // An absent learned MAC preserves the device's configured initial MAC.
        if mac.is_some() {
            router.guest_mac = mac;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{MacAddr, NetDevice};

    #[test]
    fn the_device_supplies_its_mac_before_any_guest_frame_or_dhcp_renewal() {
        let backend = Arc::new(UserNetBackend::new(UserNetConfig::default()).unwrap());
        assert!(backend.router.lock().unwrap().guest_mac.is_none());
        let mac = MacAddr([0x52, 0x54, 0, 0x12, 0x34, 0x56]);
        let device = NetDevice::with_backend(backend.clone(), mac);
        use virtio_core::VirtioDevice;
        assert_eq!(
            decode(&device.save_device()).unwrap().0,
            Some(EthernetAddress(mac.0))
        );
        // No DHCP, ARP or other guest TX: an incoming host connection must
        // already produce a correctly addressed SYN for the resumed guest.
        let bound = backend
            .forward(PortForward {
                host: std::net::SocketAddr::from((std::net::Ipv4Addr::LOCALHOST, 0)),
                guest_port: 22,
            })
            .unwrap();
        let _client = std::net::TcpStream::connect(bound).unwrap();
        assert_eq!(
            backend.wait_readable(Duration::from_secs(2)).unwrap(),
            Readiness::Readable
        );
        let mut frame = vec![0; MAX_FRAME_LEN + 1];
        let len = backend.read_frame(&mut frame).unwrap().unwrap();
        let ethernet = EthernetFrame::new_checked(&frame[..len]).unwrap();
        assert_eq!(ethernet.dst_addr(), EthernetAddress(mac.0));
        let ip = Ipv4Packet::new_checked(ethernet.payload()).unwrap();
        let tcp = smoltcp::wire::TcpPacket::new_checked(ip.payload()).unwrap();
        assert!(tcp.syn());
        assert_eq!(tcp.dst_port(), 22);
        // Never rewind the allocator of a backend that has accepted traffic.
        assert!(backend.load_state(&device.save_device()).is_err());
    }

    #[test]
    fn malformed_router_metadata_does_not_change_the_live_router() {
        let backend = UserNetBackend::new(UserNetConfig::default()).unwrap();
        backend.set_guest_mac(MacAddr([0x52, 0x54, 0, 0x12, 0x34, 0x56]));
        let before = backend.save_state();
        let mut invalid: Vec<_> = (1..BYTES).map(|len| before[..len].to_vec()).collect();
        for offset in [0, 8, 14, 15, 16, 22] {
            let mut bad = before.clone();
            bad[offset] = 0xff;
            invalid.push(bad);
        }
        let mut bad_port = before.clone();
        bad_port[12..14].fill(0);
        invalid.push(bad_port);
        let mut trailing = before.clone();
        trailing.push(0);
        invalid.push(trailing);
        for bad in invalid {
            assert!(backend.load_state(&bad).is_err(), "{bad:?}");
            assert_eq!(backend.save_state(), before);
        }
        assert!(backend.load_state(&[]).is_ok());
    }

    #[test]
    fn router_metadata_carries_the_learned_mac_and_next_unused_forward_port() {
        let mac = EthernetAddress([0x52, 0x54, 0, 0x12, 0x34, 0x56]);
        let source = UserNetBackend::new(UserNetConfig::default()).unwrap();
        {
            let mut router = source.router.lock().unwrap();
            router.guest_mac = Some(mac);
            router.tcp.restore_inbound_port(54321).unwrap();
        }
        let bytes = source.save_state();
        drop(source);
        let target = UserNetBackend::new(UserNetConfig::default()).unwrap();
        target.load_state(&bytes).unwrap();
        assert_eq!(decode(&target.save_state()).unwrap(), (Some(mac), 54321));
    }
}
