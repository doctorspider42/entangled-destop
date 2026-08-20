//! virtio-snd over the **virtio-pci** transport (EPIC 19 + GAME-2102).
//!
//! `snd_queue.rs` drives the same, untouched device over virtio-mmio. The point
//! of having both is the claim the workspace makes about every device: the
//! transport is the only thing that changes. Nothing in `virtio-sound` knows
//! which of the two is underneath, and this file is the evidence — the identity
//! a PCI guest sees, the four queues, an MSI-X vector for each of them, and one
//! whole playback round trip, all through the BAR.

use std::sync::Arc;
use std::time::{Duration, Instant};

use virtio_core::chain::VIRTQ_DESC_F_WRITE;
use virtio_core::pci::{self, common};
use virtio_core::testing::{guest_memory, SplitRing, TestIrqLine, TestMsiSink};
use virtio_core::{msix, status, GuestMem, PciTransport, VirtioDevice};
use virtio_sound::backend::Recording;
use virtio_sound::protocol::{self, ItemHdr, RawSetParams};
use virtio_sound::{stream, SoundDevice};
use vm_memory::{Bytes, GuestAddress};

const MEM_SIZE: u64 = 1 << 20;
const RING_SIZE: u16 = 16;
const RING_BASE: [u64; 4] = [0x1000, 0x2000, 0x3000, 0x4000];
const CTL_REQ: u64 = 0x8000;
const CTL_REPLY: u64 = 0x8100;
const TX_HDR: u64 = 0x8200;
const TX_STATUS: u64 = 0x8300;
const TX_DATA: u64 = 0x9000;

const PERIOD_BYTES: usize = 512;
const BUFFER_BYTES: usize = PERIOD_BYTES * 4;

struct Harness {
    mem: Arc<GuestMem>,
    rings: [SplitRing; 4],
    transport: PciTransport,
    sink: Arc<TestMsiSink>,
    recording: Arc<Recording>,
}

impl Harness {
    fn new() -> Self {
        // Paced, like a sound card: this file is about the transport, but a
        // free-running pump would fill the capture with silence while the test
        // is still setting up.
        let (device, recording) = SoundDevice::recording(true);
        assert_eq!(device.num_queues(), 4);
        let mem = Arc::new(guest_memory(MEM_SIZE));
        let irq = Arc::new(TestIrqLine::default());
        let sink = Arc::new(TestMsiSink::default());
        let transport =
            PciTransport::with_msix(0, Box::new(device), Arc::clone(&mem), irq, sink.clone())
                .expect("transport accepts the device");
        let rings = [
            SplitRing::layout(RING_BASE[0], RING_SIZE),
            SplitRing::layout(RING_BASE[1], RING_SIZE),
            SplitRing::layout(RING_BASE[2], RING_SIZE),
            SplitRing::layout(RING_BASE[3], RING_SIZE),
        ];
        let mut harness = Harness {
            mem,
            rings,
            transport,
            sink,
            recording,
        };
        harness.bring_up();
        harness
    }

    fn read(&mut self, offset: u64, len: usize) -> u64 {
        let mut data = vec![0u8; len];
        self.transport.read_bar(offset, &mut data);
        let mut bytes = [0u8; 8];
        bytes[..len].copy_from_slice(&data);
        u64::from_le_bytes(bytes)
    }

    fn write(&mut self, offset: u64, len: usize, value: u64) {
        self.transport
            .write_bar(offset, &value.to_le_bytes()[..len]);
    }

    fn set_status(&mut self, value: u32) {
        self.write(common::DEVICE_STATUS, 1, u64::from(value));
    }

    fn bring_up(&mut self) {
        self.set_status(0);
        self.set_status(status::ACKNOWLEDGE);
        self.set_status(status::ACKNOWLEDGE | status::DRIVER);

        self.write(common::DEVICE_FEATURE_SELECT, 4, 0);
        let low = self.read(common::DEVICE_FEATURE, 4);
        self.write(common::DEVICE_FEATURE_SELECT, 4, 1);
        let high = self.read(common::DEVICE_FEATURE, 4);
        self.write(common::DRIVER_FEATURE_SELECT, 4, 0);
        self.write(common::DRIVER_FEATURE, 4, low);
        self.write(common::DRIVER_FEATURE_SELECT, 4, 1);
        self.write(common::DRIVER_FEATURE, 4, high);
        self.set_status(status::ACKNOWLEDGE | status::DRIVER | status::FEATURES_OK);

        assert_eq!(
            self.read(common::NUM_QUEUES, 2),
            4,
            "virtio-snd publishes control, event, TX and RX"
        );
        for q in 0..4u64 {
            let ring = self.rings[q as usize];
            self.write(common::QUEUE_SELECT, 2, q);
            assert!(self.read(common::QUEUE_SIZE, 2) >= u64::from(RING_SIZE));
            self.write(common::QUEUE_SIZE, 2, u64::from(RING_SIZE));
            for (field, addr) in [
                (common::QUEUE_DESC, ring.desc_table()),
                (common::QUEUE_DRIVER, ring.driver_area()),
                (common::QUEUE_DEVICE, ring.device_area()),
            ] {
                self.write(field, 4, addr & 0xffff_ffff);
                self.write(field + 4, 4, addr >> 32);
            }
            self.write(common::QUEUE_ENABLE, 2, 1);
        }
        self.set_status(
            status::ACKNOWLEDGE | status::DRIVER | status::FEATURES_OK | status::DRIVER_OK,
        );
        assert!(self.transport.is_activated());
    }

    /// One MSI-X vector per queue plus one for configuration, programmed the
    /// way `vp_find_vqs_msix` does.
    fn enable_msix(&mut self) {
        assert_eq!(
            self.transport.msix_table_size(),
            5,
            "four queues plus configuration"
        );
        for vector in 0..5u64 {
            let base = pci::MSIX_TABLE_OFFSET + vector * msix::MSIX_ENTRY_SIZE;
            self.write(base, 4, 0xfee0_0000);
            self.write(base + 4, 4, 0);
            self.write(base + 8, 4, 0x4030 + vector);
            self.write(base + 12, 4, 0);
        }
        self.write(common::CONFIG_MSIX_VECTOR, 2, 0);
        for q in 0..4u64 {
            self.write(common::QUEUE_SELECT, 2, q);
            self.write(common::QUEUE_MSIX_VECTOR, 2, q + 1);
            assert_eq!(self.read(common::QUEUE_MSIX_VECTOR, 2), q + 1);
        }
        let handle = self
            .transport
            .msix_control_handle()
            .expect("this function publishes MSI-X");
        handle.store(
            u32::from(msix::MSIX_CTRL_ENABLE) << (msix::MSIX_CONTROL_OFFSET as u32 * 8),
            std::sync::atomic::Ordering::Release,
        );
        self.transport.msix_control_changed();
        assert!(self.transport.msix_enabled());
    }

    fn kick(&mut self, queue: u16) {
        self.transport
            .write_bar(pci::queue_notify_offset(queue), &queue.to_le_bytes());
    }

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

    fn chain(&self, q: usize, start: u16, descs: &[(u64, u32, u16)]) {
        let last = descs.len().saturating_sub(1);
        for (i, &(addr, len, flags)) in descs.iter().enumerate() {
            let index = start + u16::try_from(i).expect("small chains");
            let (flags, next) = if i == last {
                (flags, 0)
            } else {
                (flags | virtio_core::chain::VIRTQ_DESC_F_NEXT, index + 1)
            };
            self.rings[q].write_desc(&self.mem, index, addr, len, flags, next);
        }
        self.rings[q].publish(&self.mem, start);
    }

    fn control(&mut self, request: &[u8]) -> u32 {
        self.write_mem(CTL_REQ, request);
        self.write_mem(CTL_REPLY, &[0xab; protocol::HDR_LEN]);
        self.chain(
            0,
            0,
            &[
                (CTL_REQ, request.len() as u32, 0),
                (CTL_REPLY, protocol::HDR_LEN as u32, VIRTQ_DESC_F_WRITE),
            ],
        );
        self.kick(protocol::VQ_CONTROL);
        let raw = self.read_mem(CTL_REPLY, protocol::HDR_LEN);
        u32::from_le_bytes([raw[0], raw[1], raw[2], raw[3]])
    }
}

/// The identity a PCI guest enumerates: `lspci` must call this a multimedia
/// audio device, and the driver must match on `0x1af4:0x1039`.
#[test]
fn the_function_identifies_itself_as_a_virtio_sound_card() {
    let harness = Harness::new();
    assert_eq!(
        harness.transport.device_id(),
        0x1040 + 25,
        "virtio device id 25 maps to PCI device id 0x1039"
    );
    assert_eq!(
        harness.transport.class_code() >> 24,
        0x04,
        "PCI base class 04 = multimedia controller"
    );
    assert_eq!(
        (harness.transport.class_code() >> 16) & 0xff,
        0x01,
        "sub-class 01 = audio device"
    );
}

/// One whole playback round trip over PCI, on the device `snd_queue.rs` drives
/// over mmio without a line of difference.
#[test]
fn a_full_playback_round_trip_works_the_same_over_pci() {
    let mut harness = Harness::new();
    harness.enable_msix();

    assert_eq!(
        harness.control(
            &RawSetParams {
                stream_id: 0,
                buffer_bytes: BUFFER_BYTES as u32,
                period_bytes: PERIOD_BYTES as u32,
                features: 0,
                channels: 2,
                format: protocol::FMT_S16,
                rate: protocol::RATE_48000,
            }
            .encode()
        ),
        protocol::S_OK
    );
    assert_eq!(
        harness.control(
            &ItemHdr {
                code: protocol::R_PCM_PREPARE,
                id: 0
            }
            .encode()
        ),
        protocol::S_OK,
        "{}",
        stream::command_name(protocol::R_PCM_PREPARE)
    );

    // One period of a square wave, so the capture is trivially recognisable.
    let mut pcm = vec![0u8; PERIOD_BYTES];
    for (i, chunk) in pcm.chunks_mut(2).enumerate() {
        let value: i16 = if (i / 8) % 2 == 0 { 12000 } else { -12000 };
        chunk.copy_from_slice(&value.to_le_bytes());
    }
    harness.write_mem(TX_HDR, &0u32.to_le_bytes());
    harness.write_mem(TX_DATA, &pcm);
    harness.write_mem(TX_STATUS, &[0xcd; protocol::PCM_STATUS_LEN]);
    harness.chain(
        2,
        0,
        &[
            (TX_HDR, protocol::PCM_XFER_LEN as u32, 0),
            (TX_DATA, PERIOD_BYTES as u32, 0),
            (
                TX_STATUS,
                protocol::PCM_STATUS_LEN as u32,
                VIRTQ_DESC_F_WRITE,
            ),
        ],
    );
    harness.kick(protocol::VQ_TX);
    // Started only once the buffer holds something, exactly as ALSA does — a
    // stream started empty would play (and capture) silence first.
    assert_eq!(
        harness.control(
            &ItemHdr {
                code: protocol::R_PCM_START,
                id: 0
            }
            .encode()
        ),
        protocol::S_OK
    );

    // The completion comes from the pump thread, so wait for it.
    let deadline = Instant::now() + Duration::from_secs(10);
    while harness.rings[2].used_idx(&harness.mem) == 0 {
        assert!(Instant::now() < deadline, "the period never completed");
        std::thread::sleep(Duration::from_millis(1));
    }
    let (head, len) = harness.rings[2].used_elem(&harness.mem, 0);
    assert_eq!(head, 0);
    assert_eq!(len, protocol::PCM_STATUS_LEN as u32);
    let raw = harness.read_mem(TX_STATUS, protocol::PCM_STATUS_LEN);
    assert_eq!(
        u32::from_le_bytes([raw[0], raw[1], raw[2], raw[3]]),
        protocol::S_OK
    );
    // The capture starts with the period; a running stream that the guest has
    // stopped feeding keeps playing silence behind it, exactly as a sound card
    // does, so only the prefix is asserted on.
    let captured = harness.recording.pcm();
    assert!(captured.len() >= pcm.len());
    assert_eq!(
        &captured[..pcm.len()],
        &pcm[..],
        "the host heard the period"
    );

    // And it was announced with the TX queue's own MSI-X vector (3 = queue 2
    // plus the configuration vector), not the shared line.
    let deadline = Instant::now() + Duration::from_secs(5);
    while !harness.sink.data().contains(&(0x4030 + 3)) {
        assert!(
            Instant::now() < deadline,
            "no MSI-X message for the TX queue; saw {:?}",
            harness.sink.data()
        );
        std::thread::sleep(Duration::from_millis(1));
    }
}
