//! Fuzzes the whole virtio-input device through a real virtio-mmio transport
//! and real split virtqueues (backlog MVP-1402, EPIC 9, GAME-2104 follow-up).
//!
//! There are exactly two things a guest can *do* to this device, and this
//! target does both with arbitrary bytes:
//!
//! * **the status queue** — the only inbound path. The guest writes
//!   `virtio_input_event` records there (`EV_LED`, `EV_REP`, and the `EV_FF`
//!   play/stop request rumble would have used), in chains of any shape: any
//!   number of buffers, any lengths, any addresses including ones outside
//!   guest RAM, either direction, interleaved, looped, or claiming to be an
//!   indirect table we never offered. Every record read out of one is
//!   guest-authored structured data on a write path, which is why it gets a
//!   fuzzer of its own;
//! * **the configuration space** — a `(select, subsel)` state machine the
//!   guest drives byte by byte, whose answers size themselves. A `size` that
//!   did not match the payload, or a payload read that ran off the end of a
//!   `Selection`, would be an information leak out of the device struct.
//!
//! The event queue is fuzzed too, for completeness: it is device-writable, so
//! a hostile chain there can only make the device refuse a buffer, but the
//! refusal path still has to hold up while events are being pushed at it.
//!
//! Properties checked after every operation:
//!
//! * no panic anywhere — the whole point;
//! * the device never sets `DEVICE_NEEDS_RESET`: a broken driver loses its own
//!   input, it cannot take the device down;
//! * no used-ring entry ever claims more bytes than one `virtio_input_event`,
//!   and never more than the chain offered as device-writable;
//! * the status queue never writes anything back (`len == 0`, always);
//! * a config read of any width at any offset answers, and the `size` byte
//!   never exceeds the payload the device can hold;
//! * `reset` returns the transport to its pristine state from any state, and
//!   the device can be brought straight back up.

#![no_main]

use std::sync::Arc;

use arbitrary::Arbitrary;
use libfuzzer_sys::fuzz_target;
use virtio_core::chain::{VIRTQ_DESC_F_INDIRECT, VIRTQ_DESC_F_NEXT, VIRTQ_DESC_F_WRITE};
use virtio_core::testing::{guest_memory, SplitRing, TestIrqLine};
use virtio_core::{mmio, status, GuestMem, MmioTransport, VIRTIO_F_VERSION_1};
use virtio_input::{config, InputDevice, InputEvent, Profile};
use vm_memory::{Bytes, GuestAddress};

const MEM_SIZE: u64 = 1 << 18;
const RING_SIZE: u16 = 16;
const RING_BASE: [u64; 2] = [0x1000, 0x2000];
/// Scratch buffers. A high slot index lands past the end of guest RAM on
/// purpose: an address the device must refuse rather than dereference.
const BUF_BASE: u64 = 0x8000;
const BUF_STRIDE: u64 = 0x100;

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
    /// Puts arbitrary bytes where a chain can point at them — this is what
    /// fills the status queue with `virtio_input_event` records.
    Poke { slot: u8, data: Vec<u8> },
    /// Builds a chain on the event or status queue and kicks it.
    Submit { queue: bool, descs: Vec<Desc> },
    /// Drives the config-space state machine: pick a pair, then read it back.
    Select { select: u8, subsel: u8 },
    /// A config-space read at an arbitrary offset and width.
    ConfigRead { offset: u16, len: u8 },
    /// A guest write to config space, which is almost all read-only.
    ConfigWrite { offset: u16, data: Vec<u8> },
    /// Host-side input arriving while the guest misbehaves.
    Push { count: u8, code: u16, value: u32 },
    /// A guest-driven device reset, followed by a fresh bring-up.
    Reset,
    /// Ask the device for what it would put in a snapshot.
    Snapshot,
}

#[derive(Debug, Arbitrary)]
struct Case {
    /// Which device personality this run drives; each has a different
    /// configuration space and a different set of accepted codes.
    profile: u8,
    ops: Vec<Op>,
}

struct Harness {
    mem: Arc<GuestMem>,
    rings: [SplitRing; 2],
    transport: MmioTransport,
    handle: virtio_input::InputHandle,
    /// Largest device-writable total any chain has offered so far.
    max_writable: u32,
    seen_used: [u16; 2],
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

    /// The register sequence a driver walks. Deterministic: the *chains* and
    /// the config-space pairs are what this target fuzzes, and
    /// `mmio_transport` already fuzzes registers.
    fn bring_up(&mut self) {
        self.write32(mmio::DEVICE_FEATURES_SEL, 0);
        let low = self.read32(mmio::DEVICE_FEATURES);
        self.write32(mmio::DEVICE_FEATURES_SEL, 1);
        let high = self.read32(mmio::DEVICE_FEATURES);
        assert_eq!(low, 0, "virtio-input offers no device-specific features");
        assert_eq!(high, (VIRTIO_F_VERSION_1 >> 32) as u32);

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
        for q in 0..2u32 {
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
            "a broken driver loses its own input; it must not fail the device"
        );
        for q in 0..2usize {
            let idx = self.rings[q].used_idx(&self.mem);
            while self.seen_used[q] != idx {
                let slot = self.seen_used[q] % RING_SIZE;
                let (_, len) = self.rings[q].used_elem(&self.mem, slot);
                if q == 1 {
                    assert_eq!(len, 0, "the status queue is never written back");
                } else {
                    // One event per chain, per the spec — and never more than
                    // some chain actually offered as device-writable.
                    assert!(
                        len as usize <= InputEvent::WIRE_SIZE,
                        "event queue reported {len} used bytes for one event"
                    );
                    assert!(
                        len <= self.max_writable,
                        "event queue reported {len} used bytes; no chain offered more than {}",
                        self.max_writable
                    );
                }
                self.seen_used[q] = self.seen_used[q].wrapping_add(1);
            }
        }
    }
}

fuzz_target!(|case: Case| {
    let profile = match case.profile % 3 {
        0 => Profile::Keyboard,
        1 => Profile::AbsolutePointer,
        _ => Profile::Gamepad,
    };
    // Player 2's pad, so the serial selector has a payload to answer with on
    // at least one of the three personalities.
    let device = match profile {
        Profile::Gamepad => InputDevice::gamepad_for_player(1),
        other => InputDevice::new(other),
    };
    let handle = device.handle();
    let mem = Arc::new(guest_memory(MEM_SIZE));
    let line = Arc::new(TestIrqLine::default());
    let Ok(transport) = MmioTransport::new(0, Box::new(device), Arc::clone(&mem), line) else {
        return;
    };
    let mut h = Harness {
        rings: [
            SplitRing::layout(RING_BASE[0], RING_SIZE),
            SplitRing::layout(RING_BASE[1], RING_SIZE),
        ],
        mem,
        transport,
        handle,
        max_writable: 0,
        seen_used: [0; 2],
    };
    h.bring_up();
    h.check();

    for op in case.ops.into_iter().take(32) {
        match op {
            Op::Poke { slot, data } => {
                let addr = BUF_BASE + u64::from(slot) * BUF_STRIDE;
                // Deliberately unchecked as to whether it lands: an address
                // past guest RAM simply leaves the buffer as it was.
                let _ = h
                    .mem
                    .write_slice(&data[..data.len().min(1024)], GuestAddress(addr));
            }
            Op::Select { select, subsel } => {
                h.transport.write(mmio::CONFIG_SPACE, &[select, subsel]);
                let mut size = [0u8; 1];
                h.transport
                    .read(mmio::CONFIG_SPACE + config::SIZE, &mut size);
                assert!(
                    usize::from(size[0]) <= config::PAYLOAD_MAX,
                    "size {} is larger than the payload union",
                    size[0]
                );
                // Read exactly what `size` promised: this is the driver's own
                // sequence, and the one that would notice a short payload.
                let mut payload = vec![0u8; usize::from(size[0])];
                h.transport
                    .read(mmio::CONFIG_SPACE + config::PAYLOAD, &mut payload);
            }
            Op::ConfigRead { offset, len } => {
                let mut buf = vec![0u8; usize::from(len).min(200)];
                h.transport
                    .read(mmio::CONFIG_SPACE + u64::from(offset), &mut buf);
            }
            Op::ConfigWrite { offset, data } => {
                h.transport.write(
                    mmio::CONFIG_SPACE + u64::from(offset),
                    &data[..data.len().min(200)],
                );
            }
            Op::Push { count, code, value } => {
                // Whatever the guest is doing to the queues, the host keeps
                // handing events over; delivery may fail, it may not panic.
                let events: Vec<InputEvent> = (0..usize::from(count).min(8))
                    .map(|_| InputEvent {
                        event_type: virtio_input::ev::KEY,
                        code,
                        value,
                    })
                    .chain(std::iter::once(InputEvent::SYN_REPORT))
                    .collect();
                let _ = h.handle.push(&events);
            }
            Op::Reset => {
                h.write32(mmio::STATUS, 0);
                assert_eq!(h.transport.status(), 0, "a reset must be complete");
                assert!(!h.transport.is_activated());
                assert_eq!(h.handle.pending(), 0, "a reset drops what was buffered");
                for q in 0..2usize {
                    h.rings[q].rewind(&h.mem);
                    h.seen_used[q] = 0;
                }
                h.bring_up();
            }
            Op::Snapshot => {
                let positions = h.transport.device().queue_positions();
                assert_eq!(positions.len(), 2, "two queues, two positions");
            }
            Op::Submit { queue, descs } => {
                if descs.is_empty() {
                    continue;
                }
                let queue = usize::from(queue);
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
