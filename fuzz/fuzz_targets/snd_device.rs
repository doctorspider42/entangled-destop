//! Fuzzes the whole virtio-snd device through a real virtio-mmio transport and
//! real split virtqueues (backlog MVP-1402, GAME-2102).
//!
//! `snd_control` drives the parsers directly, which is fast but sees only
//! well-shaped messages. This target is the other half: a brought-up device
//! with a live pump thread, fed **descriptor chains** of arbitrary shape —
//! any number of buffers, any lengths (including past the message caps), any
//! addresses (including outside guest RAM), readable and writable in any
//! order — on any of the four queues, interleaved with device resets.
//!
//! That is the surface a hostile driver actually has, and it is where the two
//! gather paths live: `MAX_CONTROL_MSG_BYTES` for a control message and
//! `MAX_XFER_BYTES` for a playback one, both of which must refuse *before*
//! allocating.
//!
//! Properties checked after every operation:
//!
//! * no panic anywhere — the whole point;
//! * the device never sets `DEVICE_NEEDS_RESET`: guest input is answered in
//!   band with a virtio-snd status, never by failing the device;
//! * the status word never carries a bit outside the spec's set;
//! * no used-ring entry ever claims more bytes than the guest offered as
//!   device-writable;
//! * `reset` returns the transport to its pristine state, from any state, and
//!   the device can be brought straight back up.

#![no_main]

use std::sync::Arc;

use arbitrary::Arbitrary;
use libfuzzer_sys::fuzz_target;
use virtio_core::chain::{VIRTQ_DESC_F_INDIRECT, VIRTQ_DESC_F_NEXT, VIRTQ_DESC_F_WRITE};
use virtio_core::testing::{guest_memory, SplitRing, TestIrqLine};
use virtio_core::{mmio, status, GuestMem, MmioTransport};
use virtio_sound::SoundDevice;
use vm_memory::{Bytes, GuestAddress};

const MEM_SIZE: u64 = 1 << 18;
const RING_SIZE: u16 = 16;
const RING_BASE: [u64; 4] = [0x1000, 0x2000, 0x3000, 0x4000];
/// Scratch buffers. A high slot index lands past the end of guest RAM on
/// purpose: an address the device must refuse rather than dereference.
const BUF_BASE: u64 = 0x8000;
const BUF_STRIDE: u64 = 0x400;

/// Every status bit the spec defines.
const KNOWN_STATUS: u32 = status::ACKNOWLEDGE
    | status::DRIVER
    | status::DRIVER_OK
    | status::FEATURES_OK
    | status::DEVICE_NEEDS_RESET
    | status::FAILED;

#[derive(Debug, Arbitrary)]
struct Desc {
    /// Picks the buffer address; values past guest RAM are the interesting ones.
    slot: u8,
    len: u16,
    writable: bool,
    /// Rarely: claim the buffer is an indirect descriptor table, which we never
    /// offered and which must be refused.
    indirect: bool,
}

#[derive(Debug, Arbitrary)]
enum Op {
    /// Puts arbitrary bytes where a chain can point at them.
    Poke { slot: u8, data: Vec<u8> },
    /// Builds a chain on one of the four queues and kicks it.
    Submit { queue: u8, descs: Vec<Desc> },
    /// A config-space read at an arbitrary offset and width.
    ConfigRead { offset: u16, len: u8 },
    /// A guest-driven device reset, followed by a fresh bring-up.
    Reset,
}

#[derive(Debug, Arbitrary)]
struct Case {
    ops: Vec<Op>,
}

struct Harness {
    mem: Arc<GuestMem>,
    rings: [SplitRing; 4],
    transport: MmioTransport,
    /// Largest device-writable total any chain has offered so far.
    max_writable: u32,
    seen_used: [u16; 4],
}

impl Harness {
    fn write32(&mut self, offset: u64, value: u32) {
        self.transport.write(offset, &value.to_le_bytes());
    }

    fn read32(&mut self, offset: u64) -> u32 {
        let mut data = [0u8; 4];
        self.transport.read(offset, &mut data);
        u32::from_le_bytes(data)
    }

    /// The register sequence a driver walks. Deterministic: the *chains* are
    /// what this target fuzzes, and `mmio_transport` already fuzzes registers.
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
        for q in 0..4u32 {
            let ring = self.rings[q as usize];
            self.write32(mmio::QUEUE_SEL, q);
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
    }

    fn check(&mut self) {
        let word = self.transport.status();
        assert_eq!(word & !KNOWN_STATUS, 0, "status carries undefined bits");
        assert_eq!(
            word & status::DEVICE_NEEDS_RESET,
            0,
            "guest input must be answered in band, never by failing the device"
        );
        for q in 0..4usize {
            let idx = self.rings[q].used_idx(&self.mem);
            while self.seen_used[q] != idx {
                let slot = self.seen_used[q] % RING_SIZE;
                let (_, len) = self.rings[q].used_elem(&self.mem, slot);
                assert!(
                    len <= self.max_writable,
                    "queue {q} reported {len} used bytes; no chain offered more than {}",
                    self.max_writable
                );
                self.seen_used[q] = self.seen_used[q].wrapping_add(1);
            }
        }
    }
}

fuzz_target!(|case: Case| {
    let mem = Arc::new(guest_memory(MEM_SIZE));
    let line = Arc::new(TestIrqLine::default());
    // A null sink that does not pace: the fuzzer must not spend its budget
    // sleeping, and this target is about what the device accepts, not when.
    let factory: virtio_sound::SinkFactory = Arc::new(|| {
        Box::new(virtio_sound::NullSink::unpaced()) as Box<dyn virtio_sound::AudioSink>
    });
    let device = SoundDevice::new("fuzz", factory);
    let Ok(transport) = MmioTransport::new(0, Box::new(device), Arc::clone(&mem), line) else {
        return;
    };
    let mut h = Harness {
        rings: [
            SplitRing::layout(RING_BASE[0], RING_SIZE),
            SplitRing::layout(RING_BASE[1], RING_SIZE),
            SplitRing::layout(RING_BASE[2], RING_SIZE),
            SplitRing::layout(RING_BASE[3], RING_SIZE),
        ],
        mem,
        transport,
        max_writable: 0,
        seen_used: [0; 4],
    };
    h.bring_up();
    h.check();

    for op in case.ops.into_iter().take(24) {
        match op {
            Op::Poke { slot, data } => {
                let addr = BUF_BASE + u64::from(slot) * BUF_STRIDE;
                // Deliberately unchecked as to whether it lands: an address
                // past guest RAM simply leaves the buffer as it was.
                let _ = h.mem.write_slice(&data[..data.len().min(1024)], GuestAddress(addr));
            }
            Op::ConfigRead { offset, len } => {
                let mut buf = vec![0u8; usize::from(len).min(64)];
                h.transport
                    .read(mmio::CONFIG_SPACE + u64::from(offset), &mut buf);
            }
            Op::Reset => {
                h.write32(mmio::STATUS, 0);
                assert_eq!(h.transport.status(), 0, "a reset must be complete");
                assert!(!h.transport.is_activated());
                for q in 0..4usize {
                    h.rings[q].rewind(&h.mem);
                    h.seen_used[q] = 0;
                }
                h.bring_up();
            }
            Op::Submit { queue, descs } => {
                if descs.is_empty() {
                    continue;
                }
                let queue = usize::from(queue % 4);
                let ring = h.rings[queue];
                let count = descs.len().min(usize::from(RING_SIZE));
                let mut writable_total = 0u32;
                for (i, desc) in descs.iter().take(count).enumerate() {
                    let index = i as u16;
                    let last = i + 1 == count;
                    let mut flags = 0u16;
                    if desc.writable {
                        flags |= VIRTQ_DESC_F_WRITE;
                        writable_total = writable_total.saturating_add(u32::from(desc.len));
                    }
                    if desc.indirect {
                        flags |= VIRTQ_DESC_F_INDIRECT;
                    }
                    if !last {
                        flags |= VIRTQ_DESC_F_NEXT;
                    }
                    ring.write_desc(
                        &h.mem,
                        index,
                        BUF_BASE + u64::from(desc.slot) * BUF_STRIDE,
                        u32::from(desc.len),
                        flags,
                        if last { 0 } else { index + 1 },
                    );
                }
                h.max_writable = h.max_writable.max(writable_total);
                ring.publish(&h.mem, 0);
                h.write32(mmio::QUEUE_NOTIFY, queue as u32);
            }
        }
        h.check();
    }

    // And a final reset from wherever the run ended must be clean.
    h.write32(mmio::STATUS, 0);
    assert_eq!(h.transport.status(), 0);
    assert!(!h.transport.is_activated());
});
