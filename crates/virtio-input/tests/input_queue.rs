//! End-to-end virtio-input tests over real split virtqueues (backlog MVP-901,
//! 903, 904, 905; EPIC 3 + EPIC 9 acceptance criteria).
//!
//! Each test brings a device up exactly the way a Linux driver does — through
//! the virtio-mmio registers, probing the configuration space and programming
//! both queues — then lays out descriptor chains by hand in a
//! `GuestMemoryMmap`, pushes host events through an [`InputHandle`] and
//! inspects the guest-visible bytes, the used rings and the interrupt line.
//!
//! Groups:
//!
//! * "well-behaved driver": config-space probe, event delivery byte for byte,
//!   batch ordering across many buffers, kick-driven drain, status queue,
//!   reset, two coexisting devices;
//! * "starved guest": buffering, bounded drop-oldest, delivery after refill;
//! * "malicious guest": buffers too small for one event, device-readable
//!   buffers on the event queue, interleaved directions, looped chains,
//!   indices past the ring, buffers outside guest RAM, indirect descriptors.
//!   None of them may panic, lose device health, or lose the pending event.

use std::sync::Arc;

use virtio_core::chain::{VIRTQ_DESC_F_INDIRECT, VIRTQ_DESC_F_NEXT, VIRTQ_DESC_F_WRITE};
use virtio_core::status;
use virtio_core::testing::{guest_memory, SplitRing, TestIrqLine};
use virtio_core::{mmio, GuestMem, MmioTransport, VirtioDevice, VIRTIO_F_VERSION_1};
use virtio_input::config::{
    self, VIRTIO_INPUT_CFG_ABS_INFO, VIRTIO_INPUT_CFG_EV_BITS, VIRTIO_INPUT_CFG_ID_DEVIDS,
    VIRTIO_INPUT_CFG_ID_NAME, VIRTIO_INPUT_CFG_ID_SERIAL, VIRTIO_INPUT_CFG_PROP_BITS,
};
use virtio_input::device::{EVENT_QUEUE, STATUS_QUEUE};
use virtio_input::{
    abs, btn, ev, rel, InputDevice, InputEvent, InputHandle, Profile, MAX_PENDING_EVENTS,
};
use vm_memory::{Bytes, GuestAddress};

const MEM_SIZE: u64 = 1 << 20; // 1 MiB of guest RAM
const EVENT_RING_BASE: u64 = 0x1000;
const STATUS_RING_BASE: u64 = 0x3000;
const RING_SIZE: u16 = 16;
/// Scratch guest buffers, well clear of both rings.
const BUF_BASE: u64 = 0x8000;
/// One event buffer per descriptor index.
const BUF_STRIDE: u64 = 0x10;
/// Poison byte written into buffers before a test, so "the device wrote here"
/// is always distinguishable from "the buffer was already zero".
const POISON: u8 = 0xa5;

/// A device behind an mmio transport, its host-side handle and both rings.
struct Harness {
    mem: Arc<GuestMem>,
    eventq: SplitRing,
    statusq: SplitRing,
    transport: MmioTransport,
    irq: Arc<TestIrqLine>,
    handle: InputHandle,
    /// Next unused descriptor index on the event queue.
    next_desc: u16,
}

impl Harness {
    fn keyboard() -> Self {
        Self::around(InputDevice::keyboard())
    }

    fn tablet() -> Self {
        Self::around(InputDevice::absolute_pointer())
    }

    fn gamepad() -> Self {
        Self::around(InputDevice::gamepad())
    }

    fn around(device: InputDevice) -> Self {
        let handle = device.handle();
        let mem = Arc::new(guest_memory(MEM_SIZE));
        let irq = Arc::new(TestIrqLine::default());
        let transport = MmioTransport::new(0, Box::new(device), Arc::clone(&mem), irq.clone())
            .expect("transport accepts the device");
        let mut harness = Harness {
            mem,
            eventq: SplitRing::layout(EVENT_RING_BASE, RING_SIZE),
            statusq: SplitRing::layout(STATUS_RING_BASE, RING_SIZE),
            transport,
            irq,
            handle,
            next_desc: 0,
        };
        harness.bring_up();
        harness
    }

    // ---------------------------------------------------------- registers

    fn read32(&mut self, offset: u64) -> u32 {
        let mut data = [0u8; 4];
        self.transport.read(offset, &mut data);
        u32::from_le_bytes(data)
    }

    fn write32(&mut self, offset: u64, value: u32) {
        self.transport.write(offset, &value.to_le_bytes());
    }

    /// Full driver bring-up: identify, negotiate features, program both
    /// queues, set DRIVER_OK.
    fn bring_up(&mut self) {
        assert_eq!(self.read32(mmio::MAGIC_VALUE), mmio::MAGIC);
        assert_eq!(self.read32(mmio::VERSION_REG), 2);
        assert_eq!(self.read32(mmio::DEVICE_ID), 18, "virtio-input device id");

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
        assert_ne!(self.transport.status() & status::FEATURES_OK, 0);

        for (index, ring) in [(EVENT_QUEUE, self.eventq), (STATUS_QUEUE, self.statusq)] {
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
        assert!(self.handle.is_active());
    }

    /// One `(select, subsel)` probe, as `virtinput_cfg_select()` does it.
    fn probe(&mut self, select: u8, subsel: u8) -> Vec<u8> {
        self.transport
            .write(mmio::CONFIG_SPACE + config::SELECT, &[select, subsel]);
        let mut size = [0u8; 1];
        self.transport
            .read(mmio::CONFIG_SPACE + config::SIZE, &mut size);
        let mut payload = vec![POISON; usize::from(size[0])];
        self.transport
            .read(mmio::CONFIG_SPACE + config::PAYLOAD, &mut payload);
        payload
    }

    // ------------------------------------------------------- guest memory

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

    /// Address of the buffer belonging to descriptor `index`.
    fn buf_addr(index: u16) -> u64 {
        BUF_BASE + u64::from(index) * BUF_STRIDE
    }

    /// Offers one device-writable event buffer to the event queue, the way
    /// `virtinput_fill_evt()` does. Returns its guest address.
    fn offer_buffer(&mut self) -> u64 {
        let index = self.next_desc;
        self.next_desc += 1;
        let addr = Self::buf_addr(index);
        self.write_mem(addr, &[POISON; InputEvent::WIRE_SIZE]);
        self.eventq.write_desc(
            &self.mem,
            index,
            addr,
            InputEvent::WIRE_SIZE as u32,
            VIRTQ_DESC_F_WRITE,
            0,
        );
        self.eventq.publish(&self.mem, index);
        addr
    }

    /// Offers `count` buffers and returns their addresses in ring order.
    fn offer_buffers(&mut self, count: usize) -> Vec<u64> {
        (0..count).map(|_| self.offer_buffer()).collect()
    }

    /// Offers one deliberately malformed event buffer: the caller supplies the
    /// descriptor fields verbatim.
    fn offer_raw(&mut self, addr: u64, len: u32, flags: u16, next: u16) -> u16 {
        let index = self.next_desc;
        self.next_desc += 1;
        self.eventq
            .write_desc(&self.mem, index, addr, len, flags, next);
        self.eventq.publish(&self.mem, index);
        index
    }

    /// Reserves a descriptor index without publishing it, for chains built by
    /// hand.
    fn reserve_desc(&mut self) -> u16 {
        let index = self.next_desc;
        self.next_desc += 1;
        index
    }

    // -------------------------------------------------------------- events

    fn push(&self, events: &[InputEvent]) -> usize {
        self.handle.push(events).expect("host push must not fail")
    }

    fn notify(&mut self, queue: u16) {
        self.write32(mmio::QUEUE_NOTIFY, u32::from(queue));
    }

    /// The event the device wrote into `addr`, or `None` if the buffer is still
    /// poisoned.
    fn event_at(&self, addr: u64) -> Option<InputEvent> {
        let raw = self.read_mem(addr, InputEvent::WIRE_SIZE);
        if raw == [POISON; InputEvent::WIRE_SIZE] {
            return None;
        }
        let mut bytes = [0u8; InputEvent::WIRE_SIZE];
        bytes.copy_from_slice(&raw);
        Some(InputEvent::from_le_bytes(bytes))
    }

    fn event_used_idx(&self) -> u16 {
        self.eventq.used_idx(&self.mem)
    }

    /// `(head, len)` of event-queue used entry `slot`.
    fn event_used(&self, slot: u16) -> (u32, u32) {
        self.eventq.used_elem(&self.mem, slot % RING_SIZE)
    }
}

fn key(code: u16, value: u32) -> InputEvent {
    InputEvent {
        event_type: ev::KEY,
        code,
        value,
    }
}

/// A key press as `display` produces it: the event plus its SYN_REPORT.
fn key_batch(code: u16, value: u32) -> Vec<InputEvent> {
    vec![key(code, value), InputEvent::SYN_REPORT]
}

// ===================================================== well-behaved driver

#[test]
fn keyboard_config_space_probes_as_a_keyboard() {
    let mut h = Harness::keyboard();
    assert_eq!(h.probe(VIRTIO_INPUT_CFG_ID_NAME, 0), b"Entangled Keyboard");
    assert_eq!(
        h.probe(VIRTIO_INPUT_CFG_ID_DEVIDS, 0),
        vec![0x06, 0x00, 0x4d, 0x56, 0x01, 0x00, 0x01, 0x00]
    );

    let keys = h.probe(VIRTIO_INPUT_CFG_EV_BITS, ev::KEY as u8);
    assert_eq!(keys.len(), 45);
    // KEY_A = 30: byte 3, bit 6.
    assert_eq!(keys[3] & (1 << 6), 1 << 6);
    // KEY_ESC = 1, KEY_ENTER = 28, KEY_SPACE = 57, KEY_F12 = 88, KEY_UP = 103.
    for code in [1u16, 28, 57, 88, 103, 255] {
        let byte = usize::from(code / 8);
        assert_ne!(keys[byte] & (1 << (code % 8)), 0, "KEY {code}");
    }
    // KEY_SELECT = 353: byte 44, bit 1. Mouse buttons: absent.
    assert_eq!(keys[44], 0x02);
    assert_eq!(&keys[32..44], &[0u8; 12]);

    // EV_REP present (non-zero size is what turns auto-repeat on in Linux).
    assert_eq!(h.probe(VIRTIO_INPUT_CFG_EV_BITS, ev::REP as u8), vec![0x03]);

    // Nothing pointer-ish, no serial, no props, nothing for unknown selectors.
    for (select, subsel) in [
        (VIRTIO_INPUT_CFG_EV_BITS, ev::ABS as u8),
        (VIRTIO_INPUT_CFG_EV_BITS, ev::REL as u8),
        (VIRTIO_INPUT_CFG_EV_BITS, 0xff),
        (VIRTIO_INPUT_CFG_ABS_INFO, abs::X as u8),
        (VIRTIO_INPUT_CFG_ID_SERIAL, 0),
        (VIRTIO_INPUT_CFG_PROP_BITS, 0),
        (0x00, 0),
        (0x42, 0),
        (0xff, 0xff),
    ] {
        assert!(
            h.probe(select, subsel).is_empty(),
            "select {select:#x}/{subsel:#x} must report size 0"
        );
    }
}

#[test]
fn tablet_config_space_probes_as_an_absolute_pointer() {
    let mut h = Harness::tablet();
    assert_eq!(h.probe(VIRTIO_INPUT_CFG_ID_NAME, 0), b"Entangled Tablet");
    assert_eq!(
        h.probe(VIRTIO_INPUT_CFG_ID_DEVIDS, 0),
        vec![0x06, 0x00, 0x4d, 0x56, 0x02, 0x00, 0x01, 0x00]
    );

    // BTN_LEFT = 0x110: byte 34, bit 0, through BTN_MIDDLE at bit 2 — and
    // nothing above it, which is what keeps `joydev` off the tablet
    // (GAME-2104 follow-up; see `config::joydev_sees_an_absolute_mouse`).
    let buttons = h.probe(VIRTIO_INPUT_CFG_EV_BITS, ev::KEY as u8);
    assert_eq!(buttons.len(), 35);
    assert_eq!(buttons[34], 0x07);
    assert!(buttons[..34].iter().all(|&b| b == 0));

    assert_eq!(h.probe(VIRTIO_INPUT_CFG_EV_BITS, ev::ABS as u8), vec![0x03]);
    // REL_HWHEEL = 6 (byte 0 bit 6) and REL_WHEEL = 8 (byte 1 bit 0).
    assert_eq!(
        h.probe(VIRTIO_INPUT_CFG_EV_BITS, ev::REL as u8),
        vec![0x40, 0x01]
    );
    // MSC_SCAN = 4: the bit that lets the wheel above coexist with joydev's
    // absolute-mouse rule. A non-zero size is the whole payload of the claim.
    assert_eq!(h.probe(VIRTIO_INPUT_CFG_EV_BITS, ev::MSC as u8), vec![0x10]);

    for axis in [abs::X, abs::Y] {
        let info = h.probe(VIRTIO_INPUT_CFG_ABS_INFO, axis as u8);
        assert_eq!(info.len(), 20);
        assert_eq!(&info[0..4], &[0, 0, 0, 0], "min = 0");
        assert_eq!(&info[4..8], &[0xff, 0x7f, 0, 0], "max = ABS_AXIS_MAX");
        assert_eq!(&info[8..20], &[0u8; 12], "fuzz, flat and res are 0");
    }
    // Any other axis, and auto-repeat, are absent.
    assert!(h.probe(VIRTIO_INPUT_CFG_ABS_INFO, 0x02).is_empty());
    assert!(h.probe(VIRTIO_INPUT_CFG_EV_BITS, ev::REP as u8).is_empty());
}

/// The pad over a real transport, not only over `config::selection`: what a
/// guest driver reads out of the register window during probe is what decides
/// whether `joydev` claims it, and that path runs through the transport's
/// `select`/`subsel`/`size` handshake rather than through the profile table.
///
/// `tests/boot/tests/gamepad.rs` proves a real kernel agrees; this proves the
/// bytes it reads are the ones the profile meant, without needing a kernel.
#[test]
fn gamepad_config_space_probes_as_an_xbox_shaped_joystick() {
    let mut h = Harness::gamepad();
    assert_eq!(h.probe(VIRTIO_INPUT_CFG_ID_NAME, 0), b"Entangled Gamepad");
    assert_eq!(
        h.probe(VIRTIO_INPUT_CFG_ID_DEVIDS, 0),
        vec![0x06, 0x00, 0x4d, 0x56, 0x03, 0x00, 0x01, 0x00],
        "BUS_VIRTUAL, our own vendor, product 3 — never Microsoft's 045e"
    );

    // The eleven `BTN_GAMEPAD` codes and nothing else. BTN_SOUTH = 0x130 is
    // byte 38, BTN_THUMBR = 0x13e is byte 39 bit 6.
    let buttons = h.probe(VIRTIO_INPUT_CFG_EV_BITS, ev::KEY as u8);
    assert_eq!(buttons.len(), 40);
    assert_eq!(buttons[38], 0b1101_1011, "SOUTH EAST _ NORTH WEST _ TL TR");
    assert_eq!(
        buttons[39], 0b0111_1100,
        "_ _ SELECT START MODE THUMBL THUMBR"
    );
    assert!(
        buttons[..38].iter().all(|&b| b == 0),
        "no BTN_MOUSE, no BTN_TOUCH, no BTN_DIGI"
    );
    assert_eq!(
        buttons.iter().map(|b| b.count_ones()).sum::<u32>(),
        btn::GAMEPAD.len() as u32
    );

    // ABS_X/Y/Z/RX/RY/RZ in byte 0, ABS_HAT0X/Y in byte 2.
    assert_eq!(
        h.probe(VIRTIO_INPUT_CFG_EV_BITS, ev::ABS as u8),
        vec![0x3f, 0x00, 0x03]
    );

    // The three axis shapes, on the wire. Sticks are signed: a driver reading
    // `min` as unsigned would see 0xffff8000 and give the guest a stick that
    // only travels one way.
    for axis in [abs::X, abs::Y, abs::RX, abs::RY] {
        let info = h.probe(VIRTIO_INPUT_CFG_ABS_INFO, axis as u8);
        assert_eq!(info.len(), 20);
        assert_eq!(&info[0..4], &[0x00, 0x80, 0xff, 0xff], "min = -32768");
        assert_eq!(&info[4..8], &[0xff, 0x7f, 0x00, 0x00], "max = 32767");
        assert_eq!(&info[8..12], &[16, 0, 0, 0], "fuzz = 16");
        assert_eq!(&info[12..16], &[128, 0, 0, 0], "flat = 128");
        assert_eq!(&info[16..20], &[0, 0, 0, 0], "res = 0");
    }
    for axis in [abs::Z, abs::RZ] {
        let info = h.probe(VIRTIO_INPUT_CFG_ABS_INFO, axis as u8);
        assert_eq!(&info[0..8], &[0, 0, 0, 0, 0xff, 0, 0, 0], "0..=255");
        assert_eq!(&info[8..20], &[0u8; 12], "no fuzz or flat on a trigger");
    }
    for axis in [abs::HAT0X, abs::HAT0Y] {
        let info = h.probe(VIRTIO_INPUT_CFG_ABS_INFO, axis as u8);
        assert_eq!(&info[0..4], &[0xff, 0xff, 0xff, 0xff], "min = -1");
        assert_eq!(&info[4..8], &[1, 0, 0, 0], "max = 1");
    }

    // Not a keyboard, not a pointer, and no rumble: every one of these would
    // change what SDL and `joydev` make of the device.
    assert!(h.probe(VIRTIO_INPUT_CFG_EV_BITS, ev::REL as u8).is_empty());
    assert!(h.probe(VIRTIO_INPUT_CFG_EV_BITS, ev::REP as u8).is_empty());
    assert!(
        h.probe(VIRTIO_INPUT_CFG_EV_BITS, 0x15).is_empty(),
        "no EV_FF"
    );
    assert!(h.probe(VIRTIO_INPUT_CFG_PROP_BITS, 0).is_empty());
    // The serial *is* implemented on a gamepad, and it is the only thing that
    // tells two players' pads apart: same name, same input_id, different
    // `U: Uniq=` (GAME-2104 follow-up).
    assert_eq!(h.probe(VIRTIO_INPUT_CFG_ID_SERIAL, 0), b"player-1");
    // An axis the pad does not have answers `size = 0`, which is how a driver
    // learns the axis is absent rather than zero-ranged.
    assert!(h.probe(VIRTIO_INPUT_CFG_ABS_INFO, 0x12).is_empty());
}

/// One report of a pad in motion, byte for byte on the ring — including the
/// negative stick value, which is the one thing a `u32` field could mangle.
#[test]
fn a_gamepad_report_lands_on_the_ring_with_its_negative_axis_intact() {
    let mut h = Harness::gamepad();
    let buffers = h.offer_buffers(4);
    let batch = vec![
        InputEvent {
            event_type: ev::KEY,
            code: btn::SOUTH,
            value: 1,
        },
        InputEvent {
            event_type: ev::ABS,
            code: abs::X,
            value: (-32768i32) as u32,
        },
        InputEvent::SYN_REPORT,
    ];
    assert_eq!(h.push(&batch), 3);
    assert_eq!(
        h.read_mem(buffers[0], InputEvent::WIRE_SIZE),
        vec![0x01, 0x00, 0x30, 0x01, 0x01, 0x00, 0x00, 0x00],
        "EV_KEY, BTN_SOUTH, pressed"
    );
    assert_eq!(
        h.read_mem(buffers[1], InputEvent::WIRE_SIZE),
        vec![0x03, 0x00, 0x00, 0x00, 0x00, 0x80, 0xff, 0xff],
        "EV_ABS, ABS_X, -32768 as two's complement"
    );
    assert_eq!(h.read_mem(buffers[2], InputEvent::WIRE_SIZE), vec![0u8; 8]);
    assert_eq!(h.event_at(buffers[3]), None);
}

#[test]
fn a_key_press_batch_lands_in_the_event_queue_byte_for_byte() {
    let mut h = Harness::keyboard();
    let buffers = h.offer_buffers(4);

    // KEY_A down, then the SYN_REPORT that closes the report.
    assert_eq!(h.push(&key_batch(30, 1)), 2);

    // One event per descriptor chain, in order.
    assert_eq!(
        h.read_mem(buffers[0], InputEvent::WIRE_SIZE),
        vec![0x01, 0x00, 0x1e, 0x00, 0x01, 0x00, 0x00, 0x00],
        "EV_KEY, KEY_A, value 1, all little-endian"
    );
    assert_eq!(
        h.read_mem(buffers[1], InputEvent::WIRE_SIZE),
        vec![0u8; 8],
        "SYN_REPORT is all zeroes"
    );
    // Untouched buffers keep their poison.
    assert_eq!(h.event_at(buffers[2]), None);
    assert_eq!(h.event_at(buffers[3]), None);

    // Used ring: two entries, chain heads 0 and 1, 8 bytes each.
    assert_eq!(h.event_used_idx(), 2);
    assert_eq!(h.event_used(0), (0, InputEvent::WIRE_SIZE as u32));
    assert_eq!(h.event_used(1), (1, InputEvent::WIRE_SIZE as u32));

    // The driver was interrupted, with the vring bit set.
    assert!(h.irq.count() > 0, "used buffers must raise the interrupt");
    assert_ne!(h.transport.interrupt_status() & mmio::INT_VRING, 0);
    h.write32(mmio::INTERRUPT_ACK, mmio::INT_VRING);
    assert_eq!(h.transport.interrupt_status() & mmio::INT_VRING, 0);

    let stats = h.handle.stats();
    assert_eq!((stats.queued, stats.delivered), (2, 2));
    assert_eq!(stats.dropped_overflow, 0);
    assert_eq!(stats.rejected_buffers, 0);
    assert_eq!(h.handle.pending(), 0);

    // The release batch reuses the remaining buffers.
    assert_eq!(h.push(&key_batch(30, 0)), 2);
    assert_eq!(h.event_at(buffers[2]), Some(key(30, 0)));
    assert_eq!(h.event_at(buffers[3]), Some(InputEvent::SYN_REPORT));
    assert_eq!(h.event_used_idx(), 4);
}

#[test]
fn pointer_batches_carry_abs_and_wheel_events() {
    let mut h = Harness::tablet();
    let buffers = h.offer_buffers(8);

    let batch = vec![
        InputEvent::abs_from_window(abs::X, 960.0, 1920.0),
        InputEvent::abs_from_window(abs::Y, 0.0, 1080.0),
        InputEvent::SYN_REPORT,
    ];
    assert_eq!(h.push(&batch), 3);
    assert_eq!(h.event_at(buffers[0]), Some(batch[0]));
    assert_eq!(
        h.read_mem(buffers[0], InputEvent::WIRE_SIZE),
        vec![0x03, 0x00, 0x00, 0x00, 0x00, 0x40, 0x00, 0x00],
        "EV_ABS, ABS_X, 16384"
    );
    assert_eq!(h.event_at(buffers[1]), Some(batch[1]));
    assert_eq!(h.event_at(buffers[2]), Some(InputEvent::SYN_REPORT));

    // A button press and a backwards wheel notch.
    let batch = vec![
        key(btn::LEFT, 1),
        InputEvent {
            event_type: ev::REL,
            code: rel::WHEEL,
            value: (-1i32) as u32,
        },
        InputEvent::SYN_REPORT,
    ];
    assert_eq!(h.push(&batch), 3);
    assert_eq!(h.event_at(buffers[3]), Some(key(btn::LEFT, 1)));
    assert_eq!(
        h.read_mem(buffers[4], InputEvent::WIRE_SIZE),
        vec![0x02, 0x00, 0x08, 0x00, 0xff, 0xff, 0xff, 0xff],
        "REL_WHEEL = -1 in two's complement"
    );
    assert_eq!(h.event_used_idx(), 6);
}

#[test]
fn events_queued_before_a_refill_are_delivered_on_the_next_kick() {
    let mut h = Harness::keyboard();
    // No buffers at all yet: everything is buffered on the host side.
    assert_eq!(h.push(&key_batch(30, 1)), 0);
    assert_eq!(h.push(&key_batch(48, 1)), 0);
    assert_eq!(h.handle.pending(), 4);
    assert_eq!(h.event_used_idx(), 0);
    assert_eq!(h.irq.count(), 0, "nothing was delivered, nothing to signal");

    // The driver posts buffers and kicks, exactly as `virtinput_fill_evt` does.
    let buffers = h.offer_buffers(3);
    h.notify(EVENT_QUEUE);

    assert_eq!(h.event_at(buffers[0]), Some(key(30, 1)));
    assert_eq!(h.event_at(buffers[1]), Some(InputEvent::SYN_REPORT));
    assert_eq!(h.event_at(buffers[2]), Some(key(48, 1)));
    assert_eq!(h.handle.pending(), 1, "the last SYN_REPORT still waits");
    assert!(h.irq.count() > 0);

    // One more buffer finishes the second report, in order.
    let last = h.offer_buffer();
    h.notify(EVENT_QUEUE);
    assert_eq!(h.event_at(last), Some(InputEvent::SYN_REPORT));
    assert_eq!(h.handle.pending(), 0);
    assert_eq!(h.event_used_idx(), 4);
}

#[test]
fn a_kick_with_nothing_pending_consumes_no_buffers() {
    let mut h = Harness::keyboard();
    let buffers = h.offer_buffers(2);
    h.notify(EVENT_QUEUE);
    assert_eq!(h.event_used_idx(), 0, "buffers must not be returned unused");
    assert_eq!(h.event_at(buffers[0]), None);
    assert_eq!(h.irq.count(), 0);

    // They are still there for the first real event.
    assert_eq!(h.push(&[key(30, 1)]), 1);
    assert_eq!(h.event_at(buffers[0]), Some(key(30, 1)));
}

#[test]
fn status_queue_chains_are_drained_and_acknowledged() {
    let mut h = Harness::keyboard();
    // The guest reports an LED change: one device-readable event buffer.
    let addr = BUF_BASE + 0x400;
    let led = InputEvent {
        event_type: ev::LED,
        code: 1, // LED_CAPSL
        value: 1,
    };
    h.write_mem(addr, &led.to_le_bytes());
    h.statusq.write_desc(
        &h.mem,
        0,
        addr,
        InputEvent::WIRE_SIZE as u32,
        0, // device-readable
        0,
    );
    h.statusq.publish(&h.mem, 0);
    h.notify(STATUS_QUEUE);

    assert_eq!(h.statusq.used_idx(&h.mem), 1);
    assert_eq!(
        h.statusq.used_elem(&h.mem, 0),
        (0, 0),
        "the device writes nothing back on the status queue"
    );
    assert_eq!(h.handle.stats().status_chains, 1);
    assert_eq!(h.transport.status() & status::DEVICE_NEEDS_RESET, 0);

    // A multi-event chain, and a chain the guest malformed, are both acked.
    h.write_mem(addr + 0x100, &[0u8; 64]);
    h.statusq
        .write_desc(&h.mem, 1, addr + 0x100, 64, VIRTQ_DESC_F_NEXT, 2);
    h.statusq.write_desc(&h.mem, 2, 0xdead_0000, 8, 0, 0);
    h.statusq.publish(&h.mem, 1);
    h.statusq
        .write_desc(&h.mem, 3, 0x1234, 8, VIRTQ_DESC_F_NEXT, 3);
    h.statusq.publish(&h.mem, 3);
    h.notify(STATUS_QUEUE);

    assert_eq!(h.statusq.used_idx(&h.mem), 3);
    assert_eq!(h.statusq.used_elem(&h.mem, 1), (1, 0));
    assert_eq!(h.statusq.used_elem(&h.mem, 2), (3, 0));
    assert_eq!(h.handle.stats().status_chains, 3);
    assert_eq!(h.transport.status() & status::DEVICE_NEEDS_RESET, 0);

    // Event delivery still works afterwards.
    let buffer = h.offer_buffer();
    assert_eq!(h.push(&[key(30, 1)]), 1);
    assert_eq!(h.event_at(buffer), Some(key(30, 1)));
}

#[test]
fn many_events_drain_in_one_go() {
    let mut h = Harness::keyboard();
    // Fill the whole ring with buffers, then push exactly that many events.
    let buffers = h.offer_buffers(usize::from(RING_SIZE));
    let events: Vec<InputEvent> = (0..RING_SIZE).map(|i| key(30 + i, 1)).collect();
    assert_eq!(h.push(&events), usize::from(RING_SIZE));

    for (i, addr) in buffers.iter().enumerate() {
        assert_eq!(h.event_at(*addr), Some(events[i]), "buffer {i}");
    }
    assert_eq!(h.event_used_idx(), RING_SIZE);
    assert_eq!(h.handle.pending(), 0);
    assert_eq!(h.handle.stats().delivered, u64::from(RING_SIZE));
}

#[test]
fn reset_drops_buffered_events_and_deactivates() {
    let mut h = Harness::keyboard();
    let buffer = h.offer_buffer();
    assert_eq!(h.push(&[key(30, 1)]), 1);
    assert_eq!(h.event_at(buffer), Some(key(30, 1)));

    // Buffer some events with no guest buffers available.
    assert_eq!(h.push(&key_batch(48, 1)), 0);
    assert_eq!(h.handle.pending(), 2);

    // Driver-initiated reset: STATUS = 0.
    h.write32(mmio::STATUS, 0);
    assert_eq!(h.transport.status(), 0);
    assert!(!h.transport.is_activated());
    assert!(!h.handle.is_active());
    assert_eq!(h.handle.pending(), 0, "buffered events are dropped");
    assert_eq!(h.handle.stats().dropped_reset, 2);
    assert_eq!(h.transport.interrupt_status(), 0);

    // Pushes while down are dropped, and a notify while down is ignored.
    assert_eq!(h.push(&key_batch(30, 1)), 0);
    assert_eq!(h.handle.pending(), 0);
    h.notify(EVENT_QUEUE);
    h.notify(STATUS_QUEUE);
    assert_eq!(h.transport.status() & status::DEVICE_NEEDS_RESET, 0);

    // The config space is pristine again: select back to UNSET.
    let mut header = [0xffu8; 3];
    h.transport.read(mmio::CONFIG_SPACE, &mut header);
    assert_eq!(header, [0, 0, 0]);

    // The driver re-initialises the rings and brings the device back up.
    h.eventq.rewind(&h.mem);
    h.statusq.rewind(&h.mem);
    h.next_desc = 0;
    h.bring_up();
    let buffer = h.offer_buffer();
    assert_eq!(h.push(&[key(30, 1)]), 1);
    assert_eq!(h.event_at(buffer), Some(key(30, 1)));
}

#[test]
fn keyboard_and_tablet_coexist_independently() {
    let mut keyboard = Harness::keyboard();
    let mut tablet = Harness::tablet();

    assert_eq!(keyboard.handle.profile(), Profile::Keyboard);
    assert_eq!(tablet.handle.profile(), Profile::AbsolutePointer);
    assert_ne!(
        keyboard.probe(VIRTIO_INPUT_CFG_ID_NAME, 0),
        tablet.probe(VIRTIO_INPUT_CFG_ID_NAME, 0)
    );

    let key_buffer = keyboard.offer_buffer();
    let pointer_buffer = tablet.offer_buffer();
    assert_eq!(keyboard.push(&[key(30, 1)]), 1);
    let motion = InputEvent::abs_from_window(abs::X, 1.0, 4.0);
    assert_eq!(tablet.push(&[motion]), 1);

    assert_eq!(keyboard.event_at(key_buffer), Some(key(30, 1)));
    assert_eq!(tablet.event_at(pointer_buffer), Some(motion));
    // Neither device saw the other's event.
    assert_eq!(keyboard.handle.stats().delivered, 1);
    assert_eq!(tablet.handle.stats().delivered, 1);
}

// ============================================================ starved guest

#[test]
fn a_starved_queue_buffers_events_up_to_the_bound_and_drops_the_oldest() {
    let h = Harness::keyboard();
    // Push far more events than the ring can hold, with no guest buffers.
    let overflow = 10u16;
    let total = MAX_PENDING_EVENTS + usize::from(overflow);
    let events: Vec<InputEvent> = (0..total)
        .map(|i| key(1, u32::try_from(i).expect("fits")))
        .collect();
    assert_eq!(h.push(&events), 0);

    assert_eq!(h.handle.pending(), MAX_PENDING_EVENTS);
    let stats = h.handle.stats();
    assert_eq!(stats.queued, total as u64);
    assert_eq!(stats.dropped_overflow, u64::from(overflow));
    assert_eq!(stats.delivered, 0);
    assert_eq!(h.event_used_idx(), 0);
}

#[test]
fn the_newest_events_are_the_ones_that_survive_starvation() {
    let mut h = Harness::keyboard();
    let total = MAX_PENDING_EVENTS + 3;
    let events: Vec<InputEvent> = (0..total)
        .map(|i| key(1, u32::try_from(i).expect("fits")))
        .collect();
    assert_eq!(h.push(&events), 0);

    // The first surviving event must be number 3: 0, 1 and 2 were dropped.
    let buffer = h.offer_buffer();
    h.notify(EVENT_QUEUE);
    assert_eq!(h.event_at(buffer), Some(key(1, 3)));
    assert_eq!(h.handle.pending(), MAX_PENDING_EVENTS - 1);
}

#[test]
fn pushes_and_kicks_interleave_without_reordering() {
    let mut h = Harness::keyboard();
    let mut expected = Vec::new();
    let mut buffers = Vec::new();
    for round in 0..4u32 {
        // One buffer per two events: the queue is permanently half starved.
        buffers.push(h.offer_buffer());
        let batch = vec![key(30, round), InputEvent::SYN_REPORT];
        h.push(&batch);
        expected.extend(batch);
        h.notify(EVENT_QUEUE);
    }
    // Drain the rest.
    for _ in 0..4 {
        buffers.push(h.offer_buffer());
    }
    h.notify(EVENT_QUEUE);

    assert_eq!(h.handle.pending(), 0);
    let delivered: Vec<InputEvent> = buffers
        .iter()
        .filter_map(|addr| h.event_at(*addr))
        .collect();
    assert_eq!(delivered, expected, "events must keep their order");
}

// ========================================================= malicious guest

#[test]
fn buffers_too_small_for_one_event_are_returned_unused() {
    let mut h = Harness::keyboard();
    assert_eq!(h.push(&[key(30, 1)]), 0);

    // Seven bytes is one short of a virtio_input_event, and zero is worse.
    let short = Harness::buf_addr(64);
    h.write_mem(short, &[POISON; 16]);
    let head_a = h.offer_raw(short, 7, VIRTQ_DESC_F_WRITE, 0);
    let head_b = h.offer_raw(short + 8, 0, VIRTQ_DESC_F_WRITE, 0);
    h.notify(EVENT_QUEUE);

    assert_eq!(h.event_used_idx(), 2);
    assert_eq!(h.event_used(0), (u32::from(head_a), 0));
    assert_eq!(h.event_used(1), (u32::from(head_b), 0));
    assert_eq!(
        h.read_mem(short, 16),
        vec![POISON; 16],
        "nothing was written"
    );
    assert_eq!(h.handle.stats().rejected_buffers, 2);
    assert_eq!(h.handle.pending(), 1, "the event waits for a usable buffer");
    assert_eq!(h.transport.status() & status::DEVICE_NEEDS_RESET, 0);

    // A proper buffer still gets the event.
    let good = h.offer_buffer();
    h.notify(EVENT_QUEUE);
    assert_eq!(h.event_at(good), Some(key(30, 1)));
}

#[test]
fn device_readable_buffers_on_the_event_queue_are_refused() {
    let mut h = Harness::keyboard();
    assert_eq!(h.push(&[key(30, 1)]), 0);

    // The event queue is device-writable only; a driver-readable buffer there
    // is a protocol violation, and writing to it would corrupt guest data.
    let addr = Harness::buf_addr(70);
    h.write_mem(addr, &[POISON; 8]);
    let head = h.offer_raw(addr, InputEvent::WIRE_SIZE as u32, 0, 0);
    h.notify(EVENT_QUEUE);

    assert_eq!(h.event_used(0), (u32::from(head), 0));
    assert_eq!(h.read_mem(addr, 8), vec![POISON; 8]);
    assert_eq!(h.handle.pending(), 1);
    assert_eq!(h.handle.stats().rejected_buffers, 1);
    assert_eq!(h.transport.status() & status::DEVICE_NEEDS_RESET, 0);
}

#[test]
fn chains_that_interleave_directions_are_refused() {
    let mut h = Harness::keyboard();
    assert_eq!(h.push(&[key(30, 1)]), 0);

    // Device-writable first, device-readable second: the spec forbids it.
    let first = h.reserve_desc();
    let second = h.reserve_desc();
    let addr = Harness::buf_addr(72);
    h.write_mem(addr, &[POISON; 16]);
    h.eventq.write_desc(
        &h.mem,
        first,
        addr,
        InputEvent::WIRE_SIZE as u32,
        VIRTQ_DESC_F_WRITE | VIRTQ_DESC_F_NEXT,
        second,
    );
    h.eventq.write_desc(&h.mem, second, addr + 8, 8, 0, 0);
    h.eventq.publish(&h.mem, first);
    h.notify(EVENT_QUEUE);

    assert_eq!(h.event_used(0), (u32::from(first), 0));
    assert_eq!(h.read_mem(addr, 16), vec![POISON; 16]);
    assert_eq!(h.handle.pending(), 1);
    assert_eq!(h.transport.status() & status::DEVICE_NEEDS_RESET, 0);
}

#[test]
fn looped_chains_are_dropped_without_hanging() {
    let mut h = Harness::keyboard();
    assert_eq!(h.push(&[key(30, 1)]), 0);

    // A descriptor chaining to itself: an infinite chain.
    let head = h.reserve_desc();
    let addr = Harness::buf_addr(74);
    h.write_mem(addr, &[POISON; 8]);
    h.eventq.write_desc(
        &h.mem,
        head,
        addr,
        InputEvent::WIRE_SIZE as u32,
        VIRTQ_DESC_F_WRITE | VIRTQ_DESC_F_NEXT,
        head,
    );
    h.eventq.publish(&h.mem, head);

    // A two-descriptor loop as well.
    let a = h.reserve_desc();
    let b = h.reserve_desc();
    h.eventq.write_desc(
        &h.mem,
        a,
        addr,
        8,
        VIRTQ_DESC_F_WRITE | VIRTQ_DESC_F_NEXT,
        b,
    );
    h.eventq.write_desc(
        &h.mem,
        b,
        addr,
        8,
        VIRTQ_DESC_F_WRITE | VIRTQ_DESC_F_NEXT,
        a,
    );
    h.eventq.publish(&h.mem, a);
    h.notify(EVENT_QUEUE);

    assert_eq!(h.event_used_idx(), 2);
    assert_eq!(h.event_used(0), (u32::from(head), 0));
    assert_eq!(h.event_used(1), (u32::from(a), 0));
    assert_eq!(h.read_mem(addr, 8), vec![POISON; 8]);
    assert_eq!(h.handle.pending(), 1);
    assert_eq!(h.transport.status() & status::DEVICE_NEEDS_RESET, 0);
}

#[test]
fn descriptor_indices_past_the_ring_are_dropped() {
    let mut h = Harness::keyboard();
    assert_eq!(h.push(&[key(30, 1)]), 0);
    let head = h.offer_raw(
        Harness::buf_addr(76),
        InputEvent::WIRE_SIZE as u32,
        VIRTQ_DESC_F_WRITE | VIRTQ_DESC_F_NEXT,
        RING_SIZE + 40,
    );
    h.notify(EVENT_QUEUE);
    assert_eq!(h.event_used(0), (u32::from(head), 0));
    assert_eq!(h.handle.pending(), 1);
    assert_eq!(h.transport.status() & status::DEVICE_NEEDS_RESET, 0);
}

#[test]
fn buffers_outside_guest_memory_are_refused() {
    let mut h = Harness::keyboard();
    assert_eq!(h.push(&[key(30, 1)]), 0);

    for addr in [MEM_SIZE, MEM_SIZE + 0x1000, 0xdead_0000, u64::MAX - 4] {
        let head = h.offer_raw(addr, InputEvent::WIRE_SIZE as u32, VIRTQ_DESC_F_WRITE, 0);
        h.notify(EVENT_QUEUE);
        let idx = h.event_used_idx();
        assert_eq!(
            h.event_used(idx - 1),
            (u32::from(head), 0),
            "buffer at {addr:#x} must be returned unused"
        );
    }
    assert_eq!(h.handle.pending(), 1, "the event survived every bad buffer");
    assert_eq!(h.handle.stats().rejected_buffers, 4);
    assert_eq!(h.transport.status() & status::DEVICE_NEEDS_RESET, 0);

    // A buffer that starts inside RAM but runs off the end is refused too.
    let head = h.offer_raw(
        MEM_SIZE - 4,
        InputEvent::WIRE_SIZE as u32,
        VIRTQ_DESC_F_WRITE,
        0,
    );
    h.notify(EVENT_QUEUE);
    let idx = h.event_used_idx();
    assert_eq!(h.event_used(idx - 1), (u32::from(head), 0));
    assert_eq!(h.handle.pending(), 1);
}

#[test]
fn indirect_descriptors_are_refused() {
    let mut h = Harness::keyboard();
    assert_eq!(h.push(&[key(30, 1)]), 0);
    // VIRTIO_F_INDIRECT_DESC is never offered.
    let head = h.offer_raw(
        Harness::buf_addr(78),
        64,
        VIRTQ_DESC_F_WRITE | VIRTQ_DESC_F_INDIRECT,
        0,
    );
    h.notify(EVENT_QUEUE);
    assert_eq!(h.event_used(0), (u32::from(head), 0));
    assert_eq!(h.handle.pending(), 1);
    assert_eq!(h.transport.status() & status::DEVICE_NEEDS_RESET, 0);
}

#[test]
fn an_available_head_past_the_ring_does_not_panic() {
    let mut h = Harness::keyboard();
    assert_eq!(h.push(&[key(30, 1)]), 0);
    // Publish a head index the ring cannot contain. The used ring cannot
    // reference it either, so the device reports a host-visible failure and the
    // transport asks the driver to reset — but nothing panics.
    h.eventq.set_avail_entry(&h.mem, 0, RING_SIZE + 7);
    h.eventq.set_avail_idx(&h.mem, 1);
    h.notify(EVENT_QUEUE);
    assert_ne!(h.transport.status() & status::DEVICE_NEEDS_RESET, 0);
}

#[test]
fn a_notify_for_a_queue_the_device_does_not_have_is_ignored() {
    let mut h = Harness::keyboard();
    // The transport filters these before the device sees them.
    for queue in [2u32, 3, 0xffff, 0xffff_ffff] {
        h.write32(mmio::QUEUE_NOTIFY, queue);
    }
    assert_eq!(h.transport.status() & status::DEVICE_NEEDS_RESET, 0);

    // Straight at the device, the same index is a typed error.
    let mut device = InputDevice::keyboard();
    assert!(device.notify(2).is_err());
}

#[test]
fn config_space_writes_cannot_forge_a_payload() {
    let mut h = Harness::keyboard();
    // Select the name, then try to overwrite size and payload.
    assert_eq!(h.probe(VIRTIO_INPUT_CFG_ID_NAME, 0), b"Entangled Keyboard");
    h.transport
        .write(mmio::CONFIG_SPACE + config::SIZE, &[0x7f]);
    h.transport
        .write(mmio::CONFIG_SPACE + config::PAYLOAD, &[0xde; 64]);
    h.transport
        .write(mmio::CONFIG_SPACE + config::CONFIG_LEN, &[0xff; 8]);

    let mut size = [0u8; 1];
    h.transport
        .read(mmio::CONFIG_SPACE + config::SIZE, &mut size);
    assert_eq!(size[0], 18);
    let mut payload = [0u8; 18];
    h.transport
        .read(mmio::CONFIG_SPACE + config::PAYLOAD, &mut payload);
    assert_eq!(&payload, b"Entangled Keyboard");

    // Reads far past the config space are zeroes, not a panic.
    let mut far = [0xffu8; 8];
    h.transport.read(mmio::CONFIG_SPACE + 0x800, &mut far);
    assert_eq!(far, [0u8; 8]);
}
