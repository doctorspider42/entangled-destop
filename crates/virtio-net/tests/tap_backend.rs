#![cfg(target_os = "linux")]
//! TAP integration test (backlog MVP-503/505/507, EPIC 5 acceptance criteria).
//!
//! This is the one test that puts a real host network interface under the
//! device. It creates a TAP interface, attaches an `AF_PACKET` socket to it as
//! the "rest of the host", activates a [`NetDevice`] on a hand-built virtqueue
//! pair and moves one frame each way:
//!
//! ```text
//!   host stack  ──AF_PACKET send──▶ vmhostnetN ──▶ /dev/net/tun fd
//!                                                      │  RX worker
//!                                                      ▼
//!                                              RX virtqueue + interrupt
//!
//!   TX virtqueue ──notify──▶ device ──write──▶ /dev/net/tun fd
//!                                                      │
//!                                                      ▼
//!   host stack  ◀──AF_PACKET recv── vmhostnetN ◀───────┘
//! ```
//!
//! So it proves what no in-process fake can: the `TUNSETIFF` attach, the
//! `IFF_NO_PI` framing (a stray 4-byte packet-info prefix would corrupt every
//! frame), that the RX worker really blocks on and drains the descriptor, that
//! the frames the guest sees are byte-identical to the ones on the wire, and
//! that closing the VM releases the interface.
//!
//! # Privileges
//!
//! Creating a TAP interface and opening a raw packet socket both need
//! `CAP_NET_ADMIN`/`CAP_NET_RAW`. Without them — the normal case in CI and for
//! a plain `cargo test` — every test here prints why it skipped and passes, per
//! the vm-testing skill's rule for privileged tiers. To actually run them:
//!
//! ```text
//! cargo test -p virtio-net --test tap_backend --no-run   # build as your user
//! sudo <target>/debug/deps/tap_backend-<hash> --nocapture
//! ```

use std::ffi::CString;
use std::io;
use std::mem::size_of;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use virtio_core::chain::{VIRTQ_DESC_F_NEXT, VIRTQ_DESC_F_WRITE};
use virtio_core::status;
use virtio_core::testing::{guest_memory, SplitRing, TestIrqLine};
use virtio_core::{mmio, GuestMem, MmioTransport};
use virtio_net::{
    MacAddr, NetBackend, NetDevice, TapBackend, MAX_FRAME_LEN, RX_QUEUE, TX_QUEUE,
    VIRTIO_NET_HDR_LEN as HDR,
};
use vm_memory::{Bytes, GuestAddress};

const MEM_SIZE: u64 = 1 << 20;
const RX_RING_BASE: u64 = 0x1000;
const TX_RING_BASE: u64 = 0x2000;
const RING_SIZE: u16 = 16;
const TX_BUF: u64 = 0x8000;
const RX_BUF: u64 = 0x2_0000;
/// One RX chain per slot, one descriptor each, comfortably above a full frame.
const RX_CHAIN_LEN: u32 = 2048;
const RX_CHAINS: u16 = 12;

/// Ethertype 0x88b5 is reserved for local experimental use, so the host stack
/// ignores our frames instead of answering them.
const TEST_ETHERTYPE: [u8; 2] = [0x88, 0xb5];
const ETH_P_ALL: u16 = 0x0003;
const PACKET_OUTGOING: u8 = 4;
const SIOCGIFFLAGS: libc::c_ulong = 0x8913;
const SIOCSIFFLAGS: libc::c_ulong = 0x8914;

const DEADLINE: Duration = Duration::from_secs(5);

// ================================================================== helpers

fn frame(tag: u8, len: usize) -> Vec<u8> {
    let mut frame = vec![0u8; len.max(60)];
    // A made-up unicast destination nobody on the host claims, and a source in
    // the locally administered range.
    frame[..6].copy_from_slice(&[0x02, 0x00, 0x00, 0x00, 0x00, 0xfe]);
    frame[6..12].copy_from_slice(&[0x02, 0x00, 0x00, 0x00, 0x00, tag]);
    frame[12..14].copy_from_slice(&TEST_ETHERTYPE);
    for (i, byte) in frame.iter_mut().enumerate().skip(14) {
        *byte = (i as u8).wrapping_mul(3).wrapping_add(tag);
    }
    frame
}

fn wait_until(mut cond: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + DEADLINE;
    while Instant::now() < deadline {
        if cond() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    cond()
}

fn interface_exists(ifname: &str) -> bool {
    Path::new(&format!("/sys/class/net/{ifname}")).exists()
}

/// `struct ifreq` with the flags member of the union, for `SIOCSIFFLAGS`.
#[repr(C)]
struct IfReqFlags {
    name: [u8; 16],
    flags: i16,
    _rest: [u8; 22],
}

impl IfReqFlags {
    fn new(ifname: &str) -> Self {
        let mut name = [0u8; 16];
        for (slot, byte) in name.iter_mut().take(15).zip(ifname.as_bytes()) {
            *slot = *byte;
        }
        Self {
            name,
            flags: 0,
            _rest: [0u8; 22],
        }
    }
}

/// Brings `ifname` administratively up, which a TAP interface is not by default
/// and which `AF_PACKET` transmission requires.
fn set_interface_up(ifname: &str) -> io::Result<()> {
    // SAFETY: a plain socket creation with constant arguments.
    let raw = unsafe { libc::socket(libc::AF_INET, libc::SOCK_DGRAM | libc::SOCK_CLOEXEC, 0) };
    if raw < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `raw` is a fresh descriptor owned solely by this function.
    let sock = unsafe { OwnedFd::from_raw_fd(raw) };

    let mut request = IfReqFlags::new(ifname);
    // SAFETY: `request` is a fully initialised `struct ifreq`; SIOCGIFFLAGS
    // reads the name and writes back the flags member, both in bounds.
    let rc = unsafe { libc::ioctl(sock.as_raw_fd(), SIOCGIFFLAGS, &mut request) };
    if rc < 0 {
        return Err(io::Error::last_os_error());
    }
    request.flags |= i16::try_from(libc::IFF_UP | libc::IFF_RUNNING).unwrap_or(1);
    // SAFETY: same struct, now with the flags we want; SIOCSIFFLAGS only reads.
    let rc = unsafe { libc::ioctl(sock.as_raw_fd(), SIOCSIFFLAGS, &request) };
    if rc < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// The "rest of the host": a raw packet socket bound to one interface.
struct PacketSocket {
    fd: OwnedFd,
    ifindex: i32,
}

impl PacketSocket {
    fn open(ifname: &str) -> io::Result<Self> {
        let cname = CString::new(ifname).map_err(io::Error::other)?;
        // SAFETY: `cname` is a live NUL-terminated string; the call only reads it.
        let ifindex = unsafe { libc::if_nametoindex(cname.as_ptr()) };
        if ifindex == 0 {
            return Err(io::Error::last_os_error());
        }
        let ifindex = i32::try_from(ifindex).map_err(io::Error::other)?;

        // SAFETY: constant arguments; returns a descriptor or -1.
        let raw = unsafe {
            libc::socket(
                libc::AF_PACKET,
                libc::SOCK_RAW | libc::SOCK_CLOEXEC,
                i32::from(ETH_P_ALL.to_be()),
            )
        };
        if raw < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: `raw` is a fresh descriptor owned solely by this socket.
        let fd = unsafe { OwnedFd::from_raw_fd(raw) };

        let addr = Self::sockaddr(ifindex);
        // SAFETY: `addr` is an initialised `sockaddr_ll` and the length passed
        // is exactly its size; `bind` only reads it.
        let rc = unsafe {
            libc::bind(
                fd.as_raw_fd(),
                (&raw const addr).cast::<libc::sockaddr>(),
                size_of::<libc::sockaddr_ll>() as libc::socklen_t,
            )
        };
        if rc < 0 {
            return Err(io::Error::last_os_error());
        }
        // Bound receives must not block the test forever.
        let timeout = libc::timeval {
            tv_sec: 0,
            tv_usec: 200_000,
        };
        // SAFETY: `timeout` is an initialised `timeval` matching the length
        // passed for SO_RCVTIMEO.
        unsafe {
            libc::setsockopt(
                fd.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_RCVTIMEO,
                (&raw const timeout).cast::<libc::c_void>(),
                size_of::<libc::timeval>() as libc::socklen_t,
            );
        }
        Ok(Self { fd, ifindex })
    }

    fn sockaddr(ifindex: i32) -> libc::sockaddr_ll {
        // SAFETY: `sockaddr_ll` is a plain C struct with no invalid bit
        // patterns; all-zero is the documented "unset" state, and every field
        // that matters is assigned below.
        let mut addr: libc::sockaddr_ll = unsafe { std::mem::zeroed() };
        addr.sll_family = libc::AF_PACKET as u16;
        addr.sll_protocol = ETH_P_ALL.to_be();
        addr.sll_ifindex = ifindex;
        addr
    }

    /// Host → interface: the frame comes out on the TAP descriptor.
    fn send(&self, frame: &[u8]) -> io::Result<()> {
        let mut addr = Self::sockaddr(self.ifindex);
        addr.sll_halen = 6;
        addr.sll_addr[..6].copy_from_slice(&frame[..6]);
        // SAFETY: `frame` and `addr` are live and the lengths passed match them;
        // `sendto` only reads from both.
        let sent = unsafe {
            libc::sendto(
                self.fd.as_raw_fd(),
                frame.as_ptr().cast::<libc::c_void>(),
                frame.len(),
                0,
                (&raw const addr).cast::<libc::sockaddr>(),
                size_of::<libc::sockaddr_ll>() as libc::socklen_t,
            )
        };
        if sent < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    /// Waits until `expected` shows up on the interface as an *incoming* frame.
    ///
    /// Frames this socket itself sent are looped back marked
    /// `PACKET_OUTGOING`; skipping those is what makes this an assertion about
    /// the guest's transmit path and not about our own injection.
    fn saw_incoming(&self, expected: &[u8]) -> bool {
        let deadline = Instant::now() + DEADLINE;
        let mut buf = vec![0u8; 2048];
        while Instant::now() < deadline {
            let mut addr = Self::sockaddr(self.ifindex);
            let mut addr_len = size_of::<libc::sockaddr_ll>() as libc::socklen_t;
            // SAFETY: `addr`/`addr_len` are live out-parameters of exactly the
            // size the value of `addr_len` claims, and `buf` is a live mutable
            // slice of `buf.len()` bytes; `recvfrom` writes no more than that.
            let read = unsafe {
                libc::recvfrom(
                    self.fd.as_raw_fd(),
                    buf.as_mut_ptr().cast::<libc::c_void>(),
                    buf.len(),
                    0,
                    (&raw mut addr).cast::<libc::sockaddr>(),
                    &raw mut addr_len,
                )
            };
            let Ok(len) = usize::try_from(read) else {
                // Timed out (EAGAIN) or interrupted: keep waiting.
                continue;
            };
            if addr.sll_pkttype == PACKET_OUTGOING {
                continue;
            }
            if len >= expected.len() && &buf[..expected.len()] == expected {
                return true;
            }
        }
        false
    }
}

// ================================================================== harness

/// A `NetDevice` behind an mmio transport with both rings programmed.
struct Harness {
    mem: Arc<GuestMem>,
    rx_ring: SplitRing,
    tx_ring: SplitRing,
    transport: MmioTransport,
    irq: Arc<TestIrqLine>,
}

impl Harness {
    fn around(device: NetDevice) -> Self {
        let mem = Arc::new(guest_memory(MEM_SIZE));
        let irq = Arc::new(TestIrqLine::default());
        let transport = MmioTransport::new(0, Box::new(device), Arc::clone(&mem), irq.clone())
            .expect("transport accepts the device");
        let mut harness = Harness {
            mem,
            rx_ring: SplitRing::layout(RX_RING_BASE, RING_SIZE),
            tx_ring: SplitRing::layout(TX_RING_BASE, RING_SIZE),
            transport,
            irq,
        };
        harness.bring_up();
        harness
    }

    fn read32(&mut self, offset: u64) -> u32 {
        let mut data = [0u8; 4];
        self.transport.read(offset, &mut data);
        u32::from_le_bytes(data)
    }

    fn write32(&mut self, offset: u64, value: u32) {
        self.transport.write(offset, &value.to_le_bytes());
    }

    fn bring_up(&mut self) {
        self.write32(mmio::DEVICE_FEATURES_SEL, 0);
        let low = self.read32(mmio::DEVICE_FEATURES);
        self.write32(mmio::DEVICE_FEATURES_SEL, 1);
        let high = self.read32(mmio::DEVICE_FEATURES);

        self.write32(mmio::STATUS, status::ACKNOWLEDGE);
        self.write32(mmio::STATUS, status::ACKNOWLEDGE | status::DRIVER);
        self.write32(mmio::DRIVER_FEATURES_SEL, 0);
        self.write32(mmio::DRIVER_FEATURES, low);
        self.write32(mmio::DRIVER_FEATURES_SEL, 1);
        self.write32(mmio::DRIVER_FEATURES, high);
        self.write32(
            mmio::STATUS,
            status::ACKNOWLEDGE | status::DRIVER | status::FEATURES_OK,
        );
        for (index, ring) in [(RX_QUEUE, self.rx_ring), (TX_QUEUE, self.tx_ring)] {
            self.write32(mmio::QUEUE_SEL, u32::from(index));
            self.write32(mmio::QUEUE_NUM, u32::from(RING_SIZE));
            self.write32(mmio::QUEUE_DESC_LOW, ring.desc_table() as u32);
            self.write32(mmio::QUEUE_DRIVER_LOW, ring.driver_area() as u32);
            self.write32(mmio::QUEUE_DEVICE_LOW, ring.device_area() as u32);
            self.write32(mmio::QUEUE_READY, 1);
        }
        self.write32(
            mmio::STATUS,
            status::ACKNOWLEDGE | status::DRIVER | status::FEATURES_OK | status::DRIVER_OK,
        );
        assert!(self.transport.is_activated(), "device must be live");
    }

    fn rx_addr(&self, slot: u16) -> u64 {
        RX_BUF + u64::from(slot) * 0x1000
    }

    /// Posts `RX_CHAINS` single-descriptor RX buffers. Several, because a live
    /// host interface also emits its own traffic (IPv6 solicitations, mDNS…)
    /// that consumes chains before our frame arrives.
    fn post_rx_chains(&self) {
        for slot in 0..RX_CHAINS {
            let addr = self.rx_addr(slot);
            self.mem
                .write_slice(&vec![0u8; RX_CHAIN_LEN as usize], GuestAddress(addr))
                .expect("test write inside guest memory");
            self.rx_ring
                .write_desc(&self.mem, slot, addr, RX_CHAIN_LEN, VIRTQ_DESC_F_WRITE, 0);
            self.rx_ring.publish(&self.mem, slot);
        }
    }

    /// Scans the RX used ring for `expected`, checking the virtio-net header in
    /// front of it is the all-zero one the MVP must write.
    fn received(&self, expected: &[u8]) -> bool {
        let used = self.rx_ring.used_idx(&self.mem);
        for slot in 0..used {
            let (head, len) = self.rx_ring.used_elem(&self.mem, slot % RING_SIZE);
            let Ok(head) = u16::try_from(head) else {
                continue;
            };
            if len as usize != HDR + expected.len() {
                continue;
            }
            let mut buffer = vec![0u8; len as usize];
            if self
                .mem
                .read_slice(&mut buffer, GuestAddress(self.rx_addr(head)))
                .is_err()
            {
                continue;
            }
            if &buffer[HDR..] == expected {
                assert_eq!(
                    &buffer[..HDR],
                    &[0u8; HDR],
                    "the RX header must be 12 zero bytes (flags, gso, num_buffers)"
                );
                return true;
            }
        }
        false
    }

    /// Guest transmit: header and frame in a two-descriptor chain, then a kick.
    fn transmit(&mut self, frame: &[u8]) {
        self.mem
            .write_slice(&[0u8; HDR], GuestAddress(TX_BUF))
            .expect("test write inside guest memory");
        self.mem
            .write_slice(frame, GuestAddress(TX_BUF + 0x100))
            .expect("test write inside guest memory");
        let len = u32::try_from(frame.len()).expect("test frame fits");
        self.tx_ring
            .write_desc(&self.mem, 0, TX_BUF, HDR as u32, VIRTQ_DESC_F_NEXT, 1);
        self.tx_ring
            .write_desc(&self.mem, 1, TX_BUF + 0x100, len, 0, 0);
        self.tx_ring.publish(&self.mem, 0);
        self.write32(mmio::QUEUE_NOTIFY, u32::from(TX_QUEUE));
    }
}

/// Sets up a TAP interface plus a packet socket, or explains the skip.
fn tap_and_socket(ifname: &str) -> Option<(TapBackend, PacketSocket)> {
    if !Path::new("/dev/net/tun").exists() {
        eprintln!("skipping: /dev/net/tun is absent (no TUN/TAP support in this kernel)");
        return None;
    }
    let tap = match TapBackend::open(ifname) {
        Ok(tap) => tap,
        Err(error) => {
            eprintln!("skipping: cannot create TAP {ifname} ({error}); needs CAP_NET_ADMIN");
            return None;
        }
    };
    if let Err(error) = set_interface_up(ifname) {
        eprintln!("skipping: cannot bring {ifname} up ({error}); needs CAP_NET_ADMIN");
        return None;
    }
    match PacketSocket::open(ifname) {
        Ok(socket) => Some((tap, socket)),
        Err(error) => {
            eprintln!("skipping: cannot open a raw packet socket ({error}); needs CAP_NET_RAW");
            None
        }
    }
}

// ==================================================================== tests

#[test]
fn frames_travel_both_ways_over_a_real_tap_interface() {
    let ifname = "vmhostnet0";
    let Some((tap, socket)) = tap_and_socket(ifname) else {
        return;
    };
    assert!(interface_exists(ifname));

    // Keep a reference so the RX worker's lifecycle is observable: while it runs
    // the backend is held by the test, the device and the worker.
    let backend = Arc::new(tap);
    let device = NetDevice::with_backend(
        Arc::clone(&backend) as Arc<dyn NetBackend>,
        MacAddr::derive("tap-test"),
    );
    let stats = Arc::clone(device.stats());
    let mut h = Harness::around(device);
    h.post_rx_chains();
    assert_eq!(Arc::strong_count(&backend), 3, "RX worker must be running");

    // ---------------------------------------------- host → TAP → RX ring
    let inbound = frame(0x11, 200);
    socket
        .send(&inbound)
        .expect("the host can transmit on the TAP interface");
    assert!(
        wait_until(|| h.received(&inbound)),
        "the frame the host sent must reach the guest's RX ring \
         (rx_frames={}, dropped={})",
        stats.rx_frames(),
        stats.rx_dropped()
    );
    assert!(h.irq.count() > 0, "RX must raise the device interrupt");
    assert_ne!(h.transport.interrupt_status() & mmio::INT_VRING, 0);

    // ---------------------------------------------- TX ring → TAP → host
    let outbound = frame(0x22, 300);
    h.transmit(&outbound);
    assert_eq!(stats.tx_frames(), 1, "the frame must have left the device");
    assert_eq!(stats.tx_dropped(), 0);
    assert!(
        socket.saw_incoming(&outbound),
        "the frame the guest transmitted must appear on the host interface"
    );

    // A full-size frame survives the round trip unfragmented, which is where an
    // off-by-twelve header mistake would show up.
    let big = frame(0x33, MAX_FRAME_LEN);
    h.transmit(&big);
    assert!(
        socket.saw_incoming(&big),
        "a 1514-byte frame must pass whole"
    );

    // --------------------------------------------------------- shutdown
    h.write32(mmio::STATUS, 0);
    assert!(!h.transport.is_activated());
    assert_eq!(
        Arc::strong_count(&backend),
        2,
        "reset must stop and join the RX worker"
    );
    // The interface is still there: this test, not the device, holds the last
    // descriptor. Its release is the next test's subject.
    assert!(interface_exists(ifname));

    // Printed so a privileged run is visibly different from a skipped one.
    eprintln!(
        "TAP round trip over {ifname} OK: rx_frames={}, rx_dropped={}, tx_frames={}, tx_bytes={}",
        stats.rx_frames(),
        stats.rx_dropped(),
        stats.tx_frames(),
        stats.tx_bytes()
    );
}

#[test]
fn closing_the_vm_frees_the_tap_interface() {
    let ifname = "vmhostnet1";
    let Some((tap, _socket)) = tap_and_socket(ifname) else {
        return;
    };
    assert!(interface_exists(ifname));

    // The device owns the only descriptor this time.
    let device = NetDevice::new(tap, MacAddr::derive("tap-teardown"));
    let h = Harness::around(device);
    assert!(interface_exists(ifname));

    drop(h);
    assert!(
        wait_until(|| !interface_exists(ifname)),
        "closing the VM must release the TAP interface \
         (acceptance criterion: no TAP devices left behind)"
    );
}
