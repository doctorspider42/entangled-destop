//! End-to-end virtio-net tests over a real split virtqueue (backlog MVP-509,
//! EPIC 3 + EPIC 5 acceptance criteria).
//!
//! Every test brings the device up exactly the way a driver does — through the
//! virtio-mmio registers — lays out descriptor chains by hand in a
//! `GuestMemoryMmap`, and then either kicks `QUEUE_NOTIFY` (transmit) or injects
//! a frame into the host backend and waits for the RX worker to publish it
//! (receive).
//!
//! The backend is an in-process fake, so all of this — including the RX thread's
//! lifecycle — runs on any host OS. The real TAP path is covered by
//! `tap_backend.rs`, which self-skips without CAP_NET_ADMIN.
//!
//! Three groups:
//!
//! * "well-behaved driver": TX and RX round trips, scattered chains, split
//!   headers, batched notifications, interrupt delivery;
//! * "lifecycle": reset stops and joins the RX worker, re-initialisation works,
//!   dropping the device leaves no thread behind;
//! * "malicious guest": looped chains, indices past the ring, buffers outside
//!   guest RAM, zero-length and oversized frames, headers that ask for
//!   offloading, RX buffers too small for the frame. None of them may panic,
//!   and none may take the device down.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use virtio_core::chain::{VIRTQ_DESC_F_NEXT, VIRTQ_DESC_F_WRITE};
use virtio_core::status;
use virtio_core::testing::{guest_memory, SplitRing, TestIrqLine};
use virtio_core::{mmio, GuestMem, MmioTransport, VirtioDevice, VIRTIO_F_VERSION_1};
use virtio_net::{
    MacAddr, NetBackend, NetDevice, NetError, NetStats, Readiness, MAX_FRAME_LEN, RX_QUEUE,
    TX_QUEUE, VIRTIO_NET_F_MAC, VIRTIO_NET_HDR_LEN as HDR,
};
use vm_memory::{Bytes, GuestAddress};

const MEM_SIZE: u64 = 1 << 20; // 1 MiB of guest RAM
const RX_RING_BASE: u64 = 0x1000;
const TX_RING_BASE: u64 = 0x2000;
const RING_SIZE: u16 = 16;
/// Scratch guest buffers, well clear of both rings.
const TX_BUF: u64 = 0x8000;
const RX_BUF: u64 = 0x1_0000;

const MAC: MacAddr = MacAddr([0x52, 0x54, 0x00, 0xab, 0xcd, 0xef]);

/// How long a test waits for the RX worker to do its job.
const RX_DEADLINE: Duration = Duration::from_secs(5);

// ============================================================ fake backend

#[derive(Default)]
struct MockState {
    /// Frames the host is delivering to the guest.
    to_guest: VecDeque<Vec<u8>>,
    /// Frames the guest transmitted.
    sent: Vec<Vec<u8>>,
    woken: bool,
    /// Emulates a full interface queue: `write_frame` returns `Ok(0)`.
    congested: bool,
    /// Emulates a broken backend: `write_frame` returns an error.
    failing: bool,
}

/// An in-process [`NetBackend`] with the same blocking contract as the TAP one.
#[derive(Default)]
struct MockBackend {
    state: Mutex<MockState>,
    ready: Condvar,
    waits: AtomicUsize,
}

impl MockBackend {
    fn lock(&self) -> std::sync::MutexGuard<'_, MockState> {
        self.state.lock().expect("test mutex is never poisoned")
    }

    /// Host → guest: hands a frame to the device's RX worker.
    fn inject(&self, frame: &[u8]) {
        self.lock().to_guest.push_back(frame.to_vec());
        self.ready.notify_all();
    }

    /// Frames the guest transmitted, in order.
    fn sent(&self) -> Vec<Vec<u8>> {
        self.lock().sent.clone()
    }

    fn set_congested(&self, congested: bool) {
        self.lock().congested = congested;
    }

    fn set_failing(&self, failing: bool) {
        self.lock().failing = failing;
    }

    /// Number of completed `wait_readable` calls — proof of life for the worker.
    fn waits(&self) -> usize {
        self.waits.load(Ordering::Acquire)
    }
}

impl NetBackend for MockBackend {
    fn name(&self) -> &str {
        "mock"
    }

    fn write_frame(&self, frame: &[u8]) -> Result<usize, NetError> {
        let mut state = self.lock();
        if state.failing {
            return Err(NetError::Write {
                backend: "mock".into(),
                source: std::io::Error::other("test backend failure"),
            });
        }
        if state.congested {
            return Ok(0);
        }
        state.sent.push(frame.to_vec());
        Ok(frame.len())
    }

    fn read_frame(&self, buf: &mut [u8]) -> Result<Option<usize>, NetError> {
        let mut state = self.lock();
        let Some(frame) = state.to_guest.pop_front() else {
            return Ok(None);
        };
        // Same truncating behaviour as a `read` on a TAP descriptor.
        let len = frame.len().min(buf.len());
        buf[..len].copy_from_slice(&frame[..len]);
        Ok(Some(len))
    }

    fn wait_readable(&self, timeout: Duration) -> Result<Readiness, NetError> {
        let mut state = self.lock();
        let deadline = Instant::now() + timeout;
        loop {
            if std::mem::take(&mut state.woken) {
                self.waits.fetch_add(1, Ordering::AcqRel);
                return Ok(Readiness::WokenUp);
            }
            if !state.to_guest.is_empty() {
                self.waits.fetch_add(1, Ordering::AcqRel);
                return Ok(Readiness::Readable);
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                self.waits.fetch_add(1, Ordering::AcqRel);
                return Ok(Readiness::TimedOut);
            }
            let (guard, _) = self
                .ready
                .wait_timeout(state, left)
                .expect("test mutex is never poisoned");
            state = guard;
        }
    }

    fn wake(&self) -> Result<(), NetError> {
        self.lock().woken = true;
        self.ready.notify_all();
        Ok(())
    }
}

// =================================================================== harness

/// A `NetDevice` behind an mmio transport, plus both guest-side rings.
struct Harness {
    mem: Arc<GuestMem>,
    rx_ring: SplitRing,
    tx_ring: SplitRing,
    transport: MmioTransport,
    irq: Arc<TestIrqLine>,
    backend: Arc<MockBackend>,
    stats: Arc<NetStats>,
}

impl Harness {
    fn new() -> Self {
        let backend = Arc::new(MockBackend::default());
        let device = NetDevice::with_backend(Arc::clone(&backend) as Arc<dyn NetBackend>, MAC);
        let stats = Arc::clone(device.stats());
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
            backend,
            stats,
        };
        harness.bring_up();
        harness
    }

    // ------------------------------------------------------------ registers

    fn read32(&mut self, offset: u64) -> u32 {
        let mut data = [0u8; 4];
        self.transport.read(offset, &mut data);
        u32::from_le_bytes(data)
    }

    fn write32(&mut self, offset: u64, value: u32) {
        self.transport.write(offset, &value.to_le_bytes());
    }

    fn device_features(&mut self) -> u64 {
        self.write32(mmio::DEVICE_FEATURES_SEL, 0);
        let low = u64::from(self.read32(mmio::DEVICE_FEATURES));
        self.write32(mmio::DEVICE_FEATURES_SEL, 1);
        let high = u64::from(self.read32(mmio::DEVICE_FEATURES));
        low | (high << 32)
    }

    /// Full driver bring-up: accept every offered feature, program both rings,
    /// set DRIVER_OK.
    fn bring_up(&mut self) {
        assert_eq!(self.read32(mmio::MAGIC_VALUE), mmio::MAGIC);
        assert_eq!(self.read32(mmio::VERSION_REG), 2);
        assert_eq!(self.read32(mmio::DEVICE_ID), 1, "virtio-net device id");

        let features = self.device_features();
        self.write32(mmio::STATUS, status::ACKNOWLEDGE);
        self.write32(mmio::STATUS, status::ACKNOWLEDGE | status::DRIVER);
        self.write32(mmio::DRIVER_FEATURES_SEL, 0);
        self.write32(mmio::DRIVER_FEATURES, features as u32);
        self.write32(mmio::DRIVER_FEATURES_SEL, 1);
        self.write32(mmio::DRIVER_FEATURES, (features >> 32) as u32);
        self.write32(
            mmio::STATUS,
            status::ACKNOWLEDGE | status::DRIVER | status::FEATURES_OK,
        );
        assert_ne!(
            self.transport.status() & status::FEATURES_OK,
            0,
            "device must accept the features it offered"
        );

        for (index, ring) in [(RX_QUEUE, self.rx_ring), (TX_QUEUE, self.tx_ring)] {
            self.write32(mmio::QUEUE_SEL, u32::from(index));
            assert!(self.read32(mmio::QUEUE_NUM_MAX) >= u32::from(RING_SIZE));
            self.write32(mmio::QUEUE_NUM, u32::from(RING_SIZE));
            self.write32(mmio::QUEUE_DESC_LOW, ring.desc_table() as u32);
            self.write32(mmio::QUEUE_DESC_HIGH, 0);
            self.write32(mmio::QUEUE_DRIVER_LOW, ring.driver_area() as u32);
            self.write32(mmio::QUEUE_DRIVER_HIGH, 0);
            self.write32(mmio::QUEUE_DEVICE_LOW, ring.device_area() as u32);
            self.write32(mmio::QUEUE_DEVICE_HIGH, 0);
            self.write32(mmio::QUEUE_READY, 1);
        }
        self.write32(
            mmio::STATUS,
            status::ACKNOWLEDGE | status::DRIVER | status::FEATURES_OK | status::DRIVER_OK,
        );
        assert!(self.transport.is_activated(), "device must be live");
    }

    fn mac_from_config(&mut self) -> [u8; 6] {
        let mut raw = [0u8; 6];
        self.transport.read(mmio::CONFIG_SPACE, &mut raw);
        raw
    }

    // --------------------------------------------------------- guest memory

    fn write_mem(&self, addr: u64, bytes: &[u8]) {
        self.mem
            .write_slice(bytes, GuestAddress(addr))
            .expect("test write inside guest memory");
    }

    fn read_mem(&self, addr: u64, len: usize) -> Vec<u8> {
        let mut buf = vec![0u8; len];
        self.mem
            .read_slice(&mut buf, GuestAddress(addr))
            .expect("test read inside guest memory");
        buf
    }

    // ---------------------------------------------------------------- rings

    /// Chains `descs` (address, length, extra flags) on `ring`, starting at
    /// descriptor `first`, and publishes the head.
    fn submit(&self, ring: &SplitRing, first: u16, descs: &[(u64, u32, u16)]) {
        let last = descs.len().saturating_sub(1);
        for (i, &(addr, len, flags)) in descs.iter().enumerate() {
            let offset = u16::try_from(i).expect("test chains stay small");
            let index = first + offset;
            let (flags, next) = if i == last {
                (flags, 0)
            } else {
                (flags | VIRTQ_DESC_F_NEXT, index + 1)
            };
            self.ring_write_desc(ring, index, addr, len, flags, next);
        }
        ring.publish(&self.mem, first);
    }

    fn ring_write_desc(
        &self,
        ring: &SplitRing,
        index: u16,
        addr: u64,
        len: u32,
        flags: u16,
        next: u16,
    ) {
        ring.write_desc(&self.mem, index, addr, len, flags, next);
    }

    fn notify_tx(&mut self) {
        self.write32(mmio::QUEUE_NOTIFY, u32::from(TX_QUEUE));
    }

    /// `(head, len)` of used-ring entry `slot`.
    fn used(&self, ring: &SplitRing, slot: u16) -> (u32, u32) {
        ring.used_elem(&self.mem, slot)
    }

    // ------------------------------------------------------------ transmit

    /// Writes a virtio-net header plus `frame` at `TX_BUF` and submits them as
    /// a two-descriptor chain, then kicks the TX queue.
    fn transmit(&mut self, frame: &[u8]) {
        self.write_mem(TX_BUF, &[0u8; HDR]);
        self.write_mem(TX_BUF + 0x100, frame);
        let len = u32::try_from(frame.len()).expect("test frame fits");
        self.submit(
            &self.tx_ring,
            0,
            &[(TX_BUF, HDR as u32, 0), (TX_BUF + 0x100, len, 0)],
        );
        self.notify_tx();
    }

    // ------------------------------------------------------------- receive

    /// Posts one RX chain made of `segments` (length per descriptor) at
    /// consecutive addresses starting from `RX_BUF`, using descriptor indices
    /// from `first`.
    fn post_rx(&self, first: u16, segments: &[u32]) {
        let mut descs = Vec::new();
        let mut addr = RX_BUF + u64::from(first) * 0x2000;
        for &len in segments {
            self.write_mem(addr, &vec![0xffu8; len as usize]);
            descs.push((addr, len, VIRTQ_DESC_F_WRITE));
            addr += 0x800 + u64::from(len);
        }
        self.submit(&self.rx_ring, first, &descs);
    }

    /// Address of the first buffer of the chain posted with `post_rx(first, …)`.
    fn rx_addr(&self, first: u16) -> u64 {
        RX_BUF + u64::from(first) * 0x2000
    }
}

/// Spins until `cond` holds or the deadline passes. Returns false on timeout,
/// so a test fails with a message instead of hanging.
fn wait_until(mut cond: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + RX_DEADLINE;
    while Instant::now() < deadline {
        if cond() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    cond()
}

fn test_frame(len: usize, tag: u8) -> Vec<u8> {
    let mut frame = vec![0u8; len];
    // Destination MAC, source MAC, ethertype 0x88b5 (local experimental).
    frame[..6].copy_from_slice(&MAC.0);
    frame[6..12].copy_from_slice(&[0x02, 0, 0, 0, 0, tag]);
    frame[12] = 0x88;
    frame[13] = 0xb5;
    for (i, byte) in frame.iter_mut().enumerate().skip(14) {
        *byte = (i as u8).wrapping_add(tag);
    }
    frame
}

// ===================================================== well-behaved driver

#[test]
fn advertises_the_mac_and_nothing_but_the_minimal_features() {
    let mut h = Harness::new();
    assert_eq!(h.mac_from_config(), MAC.0);
    assert_eq!(h.device_features(), VIRTIO_F_VERSION_1 | VIRTIO_NET_F_MAC);
    // A guest write to the config space cannot change the MAC.
    h.transport.write(mmio::CONFIG_SPACE, &[0u8; 6]);
    assert_eq!(h.mac_from_config(), MAC.0);
}

#[test]
fn transmitted_frame_reaches_the_host() {
    let mut h = Harness::new();
    let frame = test_frame(64, 1);
    h.transmit(&frame);

    assert_eq!(h.backend.sent(), vec![frame.clone()]);
    assert_eq!(h.stats.tx_frames(), 1);
    assert_eq!(h.stats.tx_bytes(), 64);
    assert_eq!(h.stats.tx_dropped(), 0);

    // The chain came back on the used ring with length 0 (nothing was written
    // into guest memory) and the driver was interrupted.
    assert_eq!(h.tx_ring.used_idx(&h.mem), 1);
    assert_eq!(h.used(&h.tx_ring, 0), (0, 0));
    assert!(h.irq.count() > 0);
    assert_ne!(h.transport.interrupt_status() & mmio::INT_VRING, 0);
    h.write32(mmio::INTERRUPT_ACK, mmio::INT_VRING);
    assert_eq!(h.transport.interrupt_status() & mmio::INT_VRING, 0);
}

#[test]
fn transmit_accepts_a_header_split_across_descriptors() {
    let mut h = Harness::new();
    let frame = test_frame(128, 2);
    h.write_mem(TX_BUF, &[0u8; HDR]);
    h.write_mem(TX_BUF + 0x100, &frame);
    let len = u32::try_from(frame.len()).expect("fits");
    // 4 + 5 + 3 bytes of header, then the frame: the spec lets the driver lay
    // the chain out however it likes.
    h.submit(
        &h.tx_ring,
        0,
        &[
            (TX_BUF, 4, 0),
            (TX_BUF + 4, 5, 0),
            (TX_BUF + 9, 3, 0),
            (TX_BUF + 0x100, len, 0),
        ],
    );
    h.notify_tx();

    assert_eq!(h.backend.sent(), vec![frame]);
    assert_eq!(h.stats.tx_dropped(), 0);
}

#[test]
fn transmit_accepts_header_and_frame_in_one_descriptor() {
    let mut h = Harness::new();
    let frame = test_frame(60, 3);
    let mut buffer = vec![0u8; HDR];
    buffer.extend_from_slice(&frame);
    h.write_mem(TX_BUF, &buffer);
    let len = u32::try_from(buffer.len()).expect("fits");
    h.submit(&h.tx_ring, 0, &[(TX_BUF, len, 0)]);
    h.notify_tx();

    assert_eq!(h.backend.sent(), vec![frame]);
}

#[test]
fn transmit_accepts_a_maximum_size_frame() {
    let mut h = Harness::new();
    let frame = test_frame(MAX_FRAME_LEN, 4);
    h.transmit(&frame);
    assert_eq!(h.backend.sent(), vec![frame]);
    assert_eq!(h.stats.tx_bytes(), MAX_FRAME_LEN as u64);
}

#[test]
fn many_frames_in_one_notification() {
    let mut h = Harness::new();
    let mut expected = Vec::new();
    for i in 0..6u16 {
        let frame = test_frame(64 + usize::from(i), i as u8);
        let header = TX_BUF + u64::from(i) * 0x400;
        let payload = header + 0x100;
        h.write_mem(header, &[0u8; HDR]);
        h.write_mem(payload, &frame);
        let base = i * 2;
        h.ring_write_desc(
            &h.tx_ring,
            base,
            header,
            HDR as u32,
            VIRTQ_DESC_F_NEXT,
            base + 1,
        );
        h.ring_write_desc(
            &h.tx_ring,
            base + 1,
            payload,
            u32::try_from(frame.len()).expect("fits"),
            0,
            0,
        );
        h.tx_ring.publish(&h.mem, base);
        expected.push(frame);
    }
    h.notify_tx();

    assert_eq!(h.backend.sent(), expected);
    assert_eq!(h.tx_ring.used_idx(&h.mem), 6);
    assert_eq!(h.stats.tx_frames(), 6);
}

#[test]
fn received_frame_lands_in_the_rx_ring_with_a_zero_header() {
    let h = Harness::new();
    // One 12-byte header buffer plus one frame buffer, the layout Linux uses.
    h.post_rx(0, &[HDR as u32, MAX_FRAME_LEN as u32]);
    let frame = test_frame(100, 7);
    h.backend.inject(&frame);

    assert!(
        wait_until(|| h.rx_ring.used_idx(&h.mem) == 1),
        "the RX worker must publish the frame"
    );
    let (head, len) = h.used(&h.rx_ring, 0);
    assert_eq!(head, 0);
    assert_eq!(len as usize, HDR + frame.len());

    let header = h.read_mem(h.rx_addr(0), HDR);
    assert_eq!(header, vec![0u8; HDR], "num_buffers and flags must be zero");
    let payload = h.read_mem(h.rx_addr(0) + 0x800 + HDR as u64, frame.len());
    assert_eq!(payload, frame);

    assert_eq!(h.stats.rx_frames(), 1);
    assert_eq!(h.stats.rx_bytes(), 100);
    assert_eq!(h.stats.rx_dropped(), 0);
    assert!(h.irq.count() > 0, "RX must raise the interrupt");
    assert_ne!(h.transport.interrupt_status() & mmio::INT_VRING, 0);
}

#[test]
fn received_frame_is_scattered_across_small_descriptors() {
    let h = Harness::new();
    // Deliberately awkward: the header spans two descriptors and the frame is
    // split again in the middle.
    h.post_rx(0, &[4, 8, 40, 1500]);
    let frame = test_frame(200, 9);
    h.backend.inject(&frame);

    assert!(wait_until(|| h.rx_ring.used_idx(&h.mem) == 1));
    let (_, len) = h.used(&h.rx_ring, 0);
    assert_eq!(len as usize, HDR + frame.len());

    // Reassemble what the guest sees out of the four buffers.
    let mut base = h.rx_addr(0);
    let mut seen = Vec::new();
    for size in [4u32, 8, 40, 1500] {
        seen.extend(h.read_mem(base, size as usize));
        base += 0x800 + u64::from(size);
    }
    let mut expected = vec![0u8; HDR];
    expected.extend_from_slice(&frame);
    assert_eq!(&seen[..expected.len()], &expected[..]);
    assert_eq!(h.stats.rx_frames(), 1);
}

#[test]
fn several_frames_use_several_chains() {
    let h = Harness::new();
    for slot in 0..3u16 {
        h.post_rx(slot * 2, &[HDR as u32, MAX_FRAME_LEN as u32]);
    }
    let frames: Vec<Vec<u8>> = (0..3).map(|i| test_frame(80 + i, i as u8)).collect();
    for frame in &frames {
        h.backend.inject(frame);
    }
    assert!(wait_until(|| h.rx_ring.used_idx(&h.mem) == 3));
    for (slot, frame) in frames.iter().enumerate() {
        let index = u16::try_from(slot).expect("fits");
        let (head, len) = h.used(&h.rx_ring, index);
        assert_eq!(head, u32::from(index * 2));
        assert_eq!(len as usize, HDR + frame.len());
        let payload = h.read_mem(h.rx_addr(index * 2) + 0x800 + HDR as u64, frame.len());
        assert_eq!(&payload, frame, "frame {slot}");
    }
    assert_eq!(h.stats.rx_frames(), 3);
}

#[test]
fn transmit_and_receive_work_at_the_same_time() {
    let mut h = Harness::new();
    h.post_rx(0, &[HDR as u32, MAX_FRAME_LEN as u32]);
    let inbound = test_frame(300, 11);
    h.backend.inject(&inbound);
    let outbound = test_frame(120, 12);
    h.transmit(&outbound);

    assert!(wait_until(|| h.rx_ring.used_idx(&h.mem) == 1));
    assert_eq!(h.backend.sent(), vec![outbound]);
    let payload = h.read_mem(h.rx_addr(0) + 0x800 + HDR as u64, inbound.len());
    assert_eq!(payload, inbound);
}

// ================================================================ lifecycle

#[test]
fn reset_stops_the_rx_worker_and_the_device_comes_back() {
    let mut h = Harness::new();
    let frame = test_frame(64, 21);
    h.transmit(&frame);
    assert_eq!(h.backend.sent().len(), 1);
    // Device + RX worker each hold a backend reference while the worker runs.
    assert_eq!(Arc::strong_count(&h.backend), 3, "worker must be running");

    // Driver-initiated reset: STATUS = 0.
    h.write32(mmio::STATUS, 0);
    assert_eq!(h.transport.status(), 0);
    assert!(!h.transport.is_activated());
    assert_eq!(
        Arc::strong_count(&h.backend),
        2,
        "reset must join the RX worker and release its resources"
    );

    // A frame arriving while the device is down is simply dropped, and a notify
    // while down must be ignored rather than crash.
    h.backend.inject(&test_frame(64, 22));
    h.notify_tx();

    // The driver re-initialises the rings and brings the device back up.
    h.rx_ring.rewind(&h.mem);
    h.tx_ring.rewind(&h.mem);
    h.bring_up();
    assert_eq!(Arc::strong_count(&h.backend), 3);

    let again = test_frame(70, 23);
    h.transmit(&again);
    assert_eq!(h.backend.sent().len(), 2);
    assert_eq!(h.backend.sent()[1], again);

    // RX works again too.
    h.post_rx(0, &[HDR as u32, MAX_FRAME_LEN as u32]);
    let inbound = test_frame(90, 24);
    h.backend.inject(&inbound);
    assert!(wait_until(|| h.rx_ring.used_idx(&h.mem) >= 1));
}

#[test]
fn dropping_the_device_joins_the_rx_worker() {
    let h = Harness::new();
    // Outlives the harness, so the reference count is observable afterwards.
    let backend = Arc::clone(&h.backend);
    assert!(wait_until(|| backend.waits() > 0), "worker must be polling");
    assert_eq!(
        Arc::strong_count(&backend),
        4,
        "test + harness + device + worker"
    );

    // No reset: the VM just goes away.
    drop(h);
    assert_eq!(
        Arc::strong_count(&backend),
        1,
        "dropping the device must leave no RX worker behind"
    );
    let before = backend.waits();
    std::thread::sleep(Duration::from_millis(250));
    assert_eq!(
        backend.waits(),
        before,
        "no thread may keep polling the backend"
    );
}

#[test]
fn a_device_without_a_transport_refuses_notifications() {
    let mut device = NetDevice::new(NullBackend, MAC);
    assert!(!device.is_receiving());
    assert!(device.notify(RX_QUEUE).is_err());
    assert!(device.notify(TX_QUEUE).is_err());
    assert!(device.notify(99).is_err());
    device.reset();
}

/// Minimal backend for the no-transport case above.
struct NullBackend;

impl NetBackend for NullBackend {
    fn name(&self) -> &str {
        "null"
    }

    fn write_frame(&self, frame: &[u8]) -> Result<usize, NetError> {
        Ok(frame.len())
    }

    fn read_frame(&self, _buf: &mut [u8]) -> Result<Option<usize>, NetError> {
        Ok(None)
    }

    fn wait_readable(&self, _timeout: Duration) -> Result<Readiness, NetError> {
        Ok(Readiness::TimedOut)
    }
}

// ========================================================= malicious guest

/// Every dropped-frame test asserts the same invariant afterwards: nothing was
/// sent, the chain was returned, and the device is still healthy.
fn assert_tx_dropped(h: &Harness, used_entries: u16) {
    assert!(h.backend.sent().is_empty(), "no frame may reach the host");
    assert_eq!(h.tx_ring.used_idx(&h.mem), used_entries);
    assert_eq!(h.transport.status() & status::DEVICE_NEEDS_RESET, 0);
}

#[test]
fn tx_chain_without_a_full_header_is_dropped() {
    let mut h = Harness::new();
    h.write_mem(TX_BUF, &[0u8; 64]);
    for len in [0u32, 1, 11] {
        h.submit(&h.tx_ring, 0, &[(TX_BUF, len, 0)]);
        h.notify_tx();
    }
    assert_tx_dropped(&h, 3);
    assert_eq!(h.stats.tx_dropped_invalid(), 3);
}

#[test]
fn tx_header_without_a_frame_is_dropped() {
    let mut h = Harness::new();
    h.write_mem(TX_BUF, &[0u8; HDR]);
    h.submit(&h.tx_ring, 0, &[(TX_BUF, HDR as u32, 0)]);
    h.notify_tx();
    assert_tx_dropped(&h, 1);
    assert_eq!(h.stats.tx_dropped_invalid(), 1);
}

#[test]
fn tx_runt_frames_are_dropped() {
    let mut h = Harness::new();
    h.write_mem(TX_BUF, &[0u8; 64]);
    for frame_len in 1..14u32 {
        h.submit(
            &h.tx_ring,
            0,
            &[(TX_BUF, HDR as u32, 0), (TX_BUF + 0x100, frame_len, 0)],
        );
        h.notify_tx();
    }
    assert_tx_dropped(&h, 13);
    assert_eq!(h.stats.tx_dropped_invalid(), 13);
}

#[test]
fn tx_oversized_frames_are_dropped() {
    let mut h = Harness::new();
    h.write_mem(TX_BUF, &[0u8; HDR]);
    // One byte past the cap, a chain far past it, and a descriptor claiming
    // almost 4 GiB — none may be copied anywhere.
    for frame_len in [MAX_FRAME_LEN as u32 + 1, 64 * 1024, u32::MAX] {
        h.submit(
            &h.tx_ring,
            0,
            &[(TX_BUF, HDR as u32, 0), (TX_BUF + 0x1000, frame_len, 0)],
        );
        h.notify_tx();
    }
    assert_tx_dropped(&h, 3);
    assert_eq!(h.stats.tx_dropped_invalid(), 3);

    // Many descriptors that only together exceed the cap.
    let mut descs = vec![(TX_BUF, HDR as u32, 0u16)];
    for i in 0..4u64 {
        descs.push((TX_BUF + 0x1000 + i * 0x800, 512, 0));
    }
    h.submit(&h.tx_ring, 0, &descs);
    h.notify_tx();
    assert_tx_dropped(&h, 4);
}

#[test]
fn tx_buffers_outside_guest_memory_are_dropped() {
    let mut h = Harness::new();
    h.write_mem(TX_BUF, &[0u8; HDR]);
    // Header in RAM, frame past the end of it.
    h.submit(
        &h.tx_ring,
        0,
        &[(TX_BUF, HDR as u32, 0), (MEM_SIZE + 0x1000, 64, 0)],
    );
    h.notify_tx();
    // Header itself outside guest RAM.
    h.submit(&h.tx_ring, 0, &[(0xdead_0000, HDR as u32, 0)]);
    h.notify_tx();
    // A buffer straddling the end of guest RAM.
    h.submit(
        &h.tx_ring,
        0,
        &[(TX_BUF, HDR as u32, 0), (MEM_SIZE - 8, 64, 0)],
    );
    h.notify_tx();

    assert_tx_dropped(&h, 3);
    assert_eq!(h.stats.tx_dropped_invalid(), 3);
}

#[test]
fn tx_looped_chains_are_dropped_without_hanging() {
    let mut h = Harness::new();
    h.write_mem(TX_BUF, &[0u8; 128]);
    // Descriptor 0 chains to itself.
    h.ring_write_desc(&h.tx_ring, 0, TX_BUF, HDR as u32, VIRTQ_DESC_F_NEXT, 0);
    h.tx_ring.publish(&h.mem, 0);
    h.notify_tx();
    // Two-descriptor loop.
    h.ring_write_desc(&h.tx_ring, 1, TX_BUF, HDR as u32, VIRTQ_DESC_F_NEXT, 2);
    h.ring_write_desc(&h.tx_ring, 2, TX_BUF, HDR as u32, VIRTQ_DESC_F_NEXT, 1);
    h.tx_ring.publish(&h.mem, 1);
    h.notify_tx();
    // `next` past the end of the ring.
    h.ring_write_desc(
        &h.tx_ring,
        3,
        TX_BUF,
        HDR as u32,
        VIRTQ_DESC_F_NEXT,
        RING_SIZE + 9,
    );
    h.tx_ring.publish(&h.mem, 3);
    h.notify_tx();

    assert_tx_dropped(&h, 3);
    assert_eq!(h.stats.tx_dropped_invalid(), 3);

    // The device still transmits afterwards.
    let frame = test_frame(64, 31);
    h.transmit(&frame);
    assert_eq!(h.backend.sent(), vec![frame]);
}

#[test]
fn tx_headers_requesting_offloads_are_dropped() {
    let mut h = Harness::new();
    let frame = test_frame(64, 32);
    // VIRTIO_NET_HDR_F_NEEDS_CSUM, then GSO_TCPV4: neither feature was
    // negotiated, so the device must not pass the frame on as-is.
    for (offset, value) in [(0usize, 1u8), (1, 1)] {
        let mut header = [0u8; HDR];
        header[offset] = value;
        h.write_mem(TX_BUF, &header);
        h.write_mem(TX_BUF + 0x100, &frame);
        h.submit(
            &h.tx_ring,
            0,
            &[
                (TX_BUF, HDR as u32, 0),
                (TX_BUF + 0x100, u32::try_from(frame.len()).expect("fits"), 0),
            ],
        );
        h.notify_tx();
    }
    assert_tx_dropped(&h, 2);
    assert_eq!(h.stats.tx_dropped_invalid(), 2);
}

#[test]
fn tx_device_writable_chains_are_dropped() {
    let mut h = Harness::new();
    let frame = test_frame(64, 33);
    h.write_mem(TX_BUF, &[0u8; HDR]);
    h.write_mem(TX_BUF + 0x100, &frame);
    // A device-writable buffer before the header: the spec forbids it.
    h.submit(
        &h.tx_ring,
        0,
        &[
            (TX_BUF + 0x100, 8, VIRTQ_DESC_F_WRITE),
            (TX_BUF, HDR as u32, 0),
        ],
    );
    h.notify_tx();
    assert_tx_dropped(&h, 1);

    // A fully device-writable chain has no readable bytes at all: no header.
    h.submit(&h.tx_ring, 0, &[(TX_BUF, HDR as u32, VIRTQ_DESC_F_WRITE)]);
    h.notify_tx();
    assert_tx_dropped(&h, 2);
    assert_eq!(h.stats.tx_dropped_invalid(), 2);
}

#[test]
fn tx_zero_length_descriptors_are_skipped_not_fatal() {
    let mut h = Harness::new();
    let frame = test_frame(64, 34);
    h.write_mem(TX_BUF, &[0u8; HDR]);
    h.write_mem(TX_BUF + 0x100, &frame);
    // Zero-length descriptors interleaved with real ones must not shift the
    // frame or break the header.
    h.submit(
        &h.tx_ring,
        0,
        &[
            (TX_BUF + 0x2000, 0, 0),
            (TX_BUF, HDR as u32, 0),
            (TX_BUF + 0x2000, 0, 0),
            (TX_BUF + 0x100, u32::try_from(frame.len()).expect("fits"), 0),
        ],
    );
    h.notify_tx();
    assert_eq!(h.backend.sent(), vec![frame]);
    assert_eq!(h.stats.tx_dropped(), 0);
}

#[test]
fn a_congested_or_broken_backend_only_drops_frames() {
    let mut h = Harness::new();
    h.backend.set_congested(true);
    h.transmit(&test_frame(64, 35));
    assert_eq!(h.stats.tx_dropped_backend(), 1);
    assert_eq!(h.transport.status() & status::DEVICE_NEEDS_RESET, 0);

    h.backend.set_congested(false);
    h.backend.set_failing(true);
    h.transmit(&test_frame(64, 36));
    assert_eq!(h.stats.tx_dropped_backend(), 2);
    assert_eq!(h.transport.status() & status::DEVICE_NEEDS_RESET, 0);

    // Recovery: the device keeps working once the backend does.
    h.backend.set_failing(false);
    let frame = test_frame(64, 37);
    h.transmit(&frame);
    assert_eq!(h.backend.sent(), vec![frame]);
}

#[test]
fn rx_chain_too_small_for_the_frame_drops_it() {
    let h = Harness::new();
    // Room for the header and 88 bytes: not enough for a 1000-byte frame,
    // and mergeable RX buffers are not negotiated.
    h.post_rx(0, &[100]);
    h.backend.inject(&test_frame(1000, 41));

    assert!(wait_until(|| h.stats.rx_dropped_no_space() == 1));
    assert!(wait_until(|| h.rx_ring.used_idx(&h.mem) == 1));
    let (head, len) = h.used(&h.rx_ring, 0);
    assert_eq!((head, len), (0, 0), "an unusable chain returns zero length");
    assert_eq!(h.stats.rx_frames(), 0);
    // The buffer was not partially filled with frame data.
    assert_eq!(h.read_mem(h.rx_addr(0), 100), vec![0xffu8; 100]);

    // A properly sized chain right after works.
    h.post_rx(1, &[HDR as u32, MAX_FRAME_LEN as u32]);
    let frame = test_frame(1000, 42);
    h.backend.inject(&frame);
    assert!(wait_until(|| h.stats.rx_frames() == 1));
    let payload = h.read_mem(h.rx_addr(1) + 0x800 + HDR as u64, frame.len());
    assert_eq!(payload, frame);
}

#[test]
fn rx_without_posted_buffers_drops_and_recovers() {
    let mut h = Harness::new();
    // Nothing on the available ring: the frame has nowhere to go.
    h.backend.inject(&test_frame(64, 43));
    assert!(wait_until(|| h.stats.rx_dropped_no_buffer() == 1));
    assert_eq!(h.rx_ring.used_idx(&h.mem), 0);
    assert_eq!(h.transport.status() & status::DEVICE_NEEDS_RESET, 0);

    // The driver posts a buffer and kicks the RX queue; the next frame arrives.
    h.post_rx(0, &[HDR as u32, MAX_FRAME_LEN as u32]);
    h.write32(mmio::QUEUE_NOTIFY, u32::from(RX_QUEUE));
    let frame = test_frame(64, 44);
    h.backend.inject(&frame);
    assert!(wait_until(|| h.stats.rx_frames() == 1));
    let payload = h.read_mem(h.rx_addr(0) + 0x800 + HDR as u64, frame.len());
    assert_eq!(payload, frame);
}

#[test]
fn rx_malformed_chains_are_dropped() {
    let h = Harness::new();
    // A device-writable descriptor chaining to itself.
    h.ring_write_desc(
        &h.rx_ring,
        0,
        RX_BUF,
        2048,
        VIRTQ_DESC_F_WRITE | VIRTQ_DESC_F_NEXT,
        0,
    );
    h.rx_ring.publish(&h.mem, 0);
    h.backend.inject(&test_frame(64, 45));
    assert!(wait_until(|| h.stats.rx_dropped_bad_chain() == 1));
    assert!(wait_until(|| h.rx_ring.used_idx(&h.mem) == 1));
    assert_eq!(h.used(&h.rx_ring, 0), (0, 0));

    // `next` past the end of the ring.
    h.ring_write_desc(
        &h.rx_ring,
        1,
        RX_BUF,
        2048,
        VIRTQ_DESC_F_WRITE | VIRTQ_DESC_F_NEXT,
        RING_SIZE + 3,
    );
    h.rx_ring.publish(&h.mem, 1);
    h.backend.inject(&test_frame(64, 46));
    assert!(wait_until(|| h.stats.rx_dropped_bad_chain() == 2));
    assert_eq!(h.stats.rx_frames(), 0);
    assert_eq!(h.transport.status() & status::DEVICE_NEEDS_RESET, 0);
}

#[test]
fn rx_buffers_outside_guest_memory_are_dropped() {
    let h = Harness::new();
    // Big enough on paper, but not backed by guest RAM: the checked write must
    // fail and the frame must be dropped with a zero-length used entry.
    h.submit(
        &h.rx_ring,
        0,
        &[(MEM_SIZE + 0x4000, 2048, VIRTQ_DESC_F_WRITE)],
    );
    h.backend.inject(&test_frame(64, 47));
    assert!(wait_until(|| h.stats.rx_dropped_bad_chain() == 1));
    assert!(wait_until(|| h.rx_ring.used_idx(&h.mem) == 1));
    assert_eq!(h.used(&h.rx_ring, 0), (0, 0));

    // A buffer that starts inside guest RAM but runs past its end.
    h.submit(&h.rx_ring, 1, &[(MEM_SIZE - 64, 2048, VIRTQ_DESC_F_WRITE)]);
    h.backend.inject(&test_frame(64, 48));
    assert!(wait_until(|| h.stats.rx_dropped_bad_chain() == 2));
    assert_eq!(h.stats.rx_frames(), 0);
    assert_eq!(h.transport.status() & status::DEVICE_NEEDS_RESET, 0);
}

#[test]
fn an_oversized_host_frame_is_dropped_before_the_ring() {
    let h = Harness::new();
    h.post_rx(0, &[HDR as u32, MAX_FRAME_LEN as u32]);
    // A jumbo frame from a misconfigured host interface: no RX chain may be
    // used for it, because a truncated frame is worse than a lost one.
    h.backend.inject(&vec![0x5au8; MAX_FRAME_LEN + 200]);
    assert!(wait_until(|| h.stats.rx_dropped_oversized() == 1));
    assert_eq!(h.rx_ring.used_idx(&h.mem), 0);
    assert_eq!(h.stats.rx_frames(), 0);

    // The chain is still available for the next, valid frame.
    let frame = test_frame(64, 49);
    h.backend.inject(&frame);
    assert!(wait_until(|| h.stats.rx_frames() == 1));
    let payload = h.read_mem(h.rx_addr(0) + 0x800 + HDR as u64, frame.len());
    assert_eq!(payload, frame);
}

#[test]
fn notify_for_a_queue_the_device_does_not_have_is_ignored() {
    let mut h = Harness::new();
    for queue in [2u32, 7, 0xffff_ffff] {
        h.write32(mmio::QUEUE_NOTIFY, queue);
    }
    assert_eq!(h.transport.status() & status::DEVICE_NEEDS_RESET, 0);
    // And the device still works.
    let frame = test_frame(64, 50);
    h.transmit(&frame);
    assert_eq!(h.backend.sent(), vec![frame]);
}
