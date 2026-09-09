//! End-to-end virtio-snd tests over real split virtqueues (GAME-2102,
//! EPIC 3 + EPIC 21 acceptance criteria).
//!
//! Every test brings the device up exactly the way a driver does — through the
//! virtio-mmio registers — lays out descriptor chains by hand in a
//! `GuestMemoryMmap`, kicks `QUEUE_NOTIFY` and inspects the used ring, the
//! status words and the audio the host sink actually received.
//!
//! Two groups:
//!
//! * **"a real driver plays a tone"**: the information queries, the whole
//!   `SET_PARAMS`/`PREPARE`/`START`/`STOP`/`RELEASE` lifecycle, and a 440 Hz
//!   tone pushed one period at a time — asserting that the samples arrive
//!   byte-for-byte, at the right rate and channel count, with no underruns,
//!   and *paced*: eight 10 ms periods may not complete in a millisecond.
//! * **"a real driver records a tone"**: the same, read backwards. The guest
//!   posts empty buffers on the RX queue and the device fills them from a
//!   synthetic 1 kHz source, and the test asserts the frame count, the rate,
//!   the format, the exact samples, zero overruns, and the same pacing floor.
//! * **"malicious guest"**: looped chains, buffers outside guest RAM, indirect
//!   descriptors, missing status words, oversized messages, unadvertised
//!   formats and rates, out-of-range identifiers, streams named on the wrong
//!   queue, lifecycle commands in the wrong order, payloads that are not whole
//!   frames, capture buffers with no room or too much of it, and a flooded
//!   ring in either direction. None of them may panic, and each must answer
//!   with the documented virtio-snd status.

use std::sync::Arc;
use std::time::{Duration, Instant};

use virtio_core::chain::{VIRTQ_DESC_F_INDIRECT, VIRTQ_DESC_F_NEXT, VIRTQ_DESC_F_WRITE};
use virtio_core::testing::{guest_memory, SplitRing, TestIrqLine};
use virtio_core::{mmio, status, GuestMem, MmioTransport, VIRTIO_F_VERSION_1};
use virtio_sound::backend::{Recording, SourceLog, ToneSource};
use virtio_sound::protocol::{self, ItemHdr, QueryInfo, RawSetParams};
use virtio_sound::{stream, SoundDevice, SoundStats};
use vm_memory::{Bytes, GuestAddress};

const MEM_SIZE: u64 = 4 << 20;
const RING_SIZE: u16 = 64;
/// One 4 KiB slot per queue; a 64-entry ring needs ~1.7 KiB.
const RING_BASE: [u64; 4] = [0x1000, 0x2000, 0x3000, 0x4000];

/// Control request / reply buffers.
const CTL_REQ: u64 = 0x1_0000;
const CTL_REPLY: u64 = 0x1_1000;
/// Per-slot playback buffers.
const TX_HDR: u64 = 0x2_0000;
const TX_DATA: u64 = 0x2_1000;
const TX_STATUS: u64 = 0x3_1000;
/// Scratch for the odd-shaped chains of the malicious tests.
const SCRATCH: u64 = 0x3_2000;
/// Per-slot capture buffers: header, the room the device fills, and a status.
const RX_HDR: u64 = 0x4_0000;
const RX_DATA: u64 = 0x4_1000;
const RX_STATUS: u64 = 0x5_1000;

/// The stream this suite negotiates: 48 kHz stereo, 10 ms periods, a four
/// period hardware buffer — what `speaker-test` asks for, near enough.
const RATE_HZ: u32 = 48000;
const CHANNELS: u8 = 2;
const PERIOD_FRAMES: usize = 480;
const PERIOD_BYTES: usize = PERIOD_FRAMES * 4;
const PERIODS_IN_BUFFER: usize = 4;
const BUFFER_BYTES: usize = PERIOD_BYTES * PERIODS_IN_BUFFER;

/// Playback chain slots: three descriptors and one buffer each.
const SLOTS: usize = 8;
const DESCS_PER_CHAIN: u16 = 3;
/// Room per slot for one period, rounded up.
const SLOT_STRIDE: u64 = 2048;

const TONE_HZ: f64 = 440.0;
/// What the synthetic microphone hears. A different frequency from the
/// playback tone so a test that measured the wrong stream would say so.
const CAPTURE_HZ: f64 = 1000.0;
const TONE_PERIODS: usize = 8;
/// Silence queued behind the tone so the ring never runs dry while the test is
/// still measuring. Without it the pump would legitimately underrun the moment
/// the guest stopped writing, and "no underruns" would be a race.
const TAIL_PERIODS: usize = 4;
const TOTAL_PERIODS: usize = TONE_PERIODS + TAIL_PERIODS;

// ---------------------------------------------------------------- harness

struct Harness {
    mem: Arc<GuestMem>,
    rings: [SplitRing; 4],
    transport: MmioTransport,
    irq: Arc<TestIrqLine>,
    recording: Arc<Recording>,
    /// What the synthetic microphone was asked for, watched the same way.
    source_log: Arc<SourceLog>,
    /// Kept from before the device was moved into the transport — the only way
    /// to watch the counters of a device the transport now owns.
    stats: Arc<SoundStats>,
}

impl Harness {
    /// A device whose audio is captured and whose microphone hears a tone,
    /// both paced like a sound card.
    fn new() -> Self {
        Self::with_pacing(true)
    }

    /// A device whose endpoints move audio as fast as it arrives — for the
    /// malicious-guest tests, which care about statuses, not timing.
    fn unpaced() -> Self {
        Self::with_pacing(false)
    }

    fn with_pacing(paced: bool) -> Self {
        let (device, recording) = SoundDevice::recording(paced);
        let (device, source_log) = device.with_tone_source(CAPTURE_HZ, paced);
        let stats = device.stats_handle();
        let mem = Arc::new(guest_memory(MEM_SIZE));
        let irq = Arc::new(TestIrqLine::default());
        let transport = MmioTransport::new(0, Box::new(device), Arc::clone(&mem), irq.clone())
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
            irq,
            recording,
            source_log,
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

    fn bring_up(&mut self) {
        assert_eq!(self.read32(mmio::MAGIC_VALUE), mmio::MAGIC);
        assert_eq!(self.read32(mmio::VERSION_REG), 2);
        assert_eq!(self.read32(mmio::DEVICE_ID), 25, "virtio-snd device id");

        self.write32(mmio::DEVICE_FEATURES_SEL, 0);
        let low = self.read32(mmio::DEVICE_FEATURES);
        self.write32(mmio::DEVICE_FEATURES_SEL, 1);
        let high = self.read32(mmio::DEVICE_FEATURES);
        assert_eq!(
            u64::from(low) | (u64::from(high) << 32),
            VIRTIO_F_VERSION_1,
            "virtio-snd offers VERSION_1 and nothing else in this phase"
        );

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

        for q in 0..4u32 {
            let ring = self.rings[q as usize];
            self.write32(mmio::QUEUE_SEL, q);
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

    fn kick(&mut self, queue: u16) {
        self.write32(mmio::QUEUE_NOTIFY, u32::from(queue));
    }

    fn config(&mut self) -> (u32, u32, u32) {
        let mut raw = [0u8; 12];
        self.transport.read(mmio::CONFIG_SPACE, &mut raw);
        (
            u32::from_le_bytes([raw[0], raw[1], raw[2], raw[3]]),
            u32::from_le_bytes([raw[4], raw[5], raw[6], raw[7]]),
            u32::from_le_bytes([raw[8], raw[9], raw[10], raw[11]]),
        )
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

    /// Lays a chain out starting at descriptor `start` on queue `q` and
    /// publishes it. Returns the head index.
    fn chain(&self, q: usize, start: u16, descs: &[(u64, u32, u16)]) -> u16 {
        let last = descs.len().saturating_sub(1);
        for (i, &(addr, len, flags)) in descs.iter().enumerate() {
            let index = start + u16::try_from(i).expect("test chains stay small");
            let (flags, next) = if i == last {
                (flags, 0)
            } else {
                (flags | VIRTQ_DESC_F_NEXT, index + 1)
            };
            self.rings[q].write_desc(&self.mem, index, addr, len, flags, next);
        }
        self.rings[q].publish(&self.mem, start);
        start
    }

    fn used_idx(&self, q: usize) -> u16 {
        self.rings[q].used_idx(&self.mem)
    }

    fn used_elem(&self, q: usize, slot: u16) -> (u32, u32) {
        self.rings[q].used_elem(&self.mem, slot % RING_SIZE)
    }

    // -------------------------------------------------------- control queue

    /// Sends one control request and returns `(status, payload, used_len)`.
    fn control(&mut self, request: &[u8], reply_len: usize) -> (u32, Vec<u8>, u32) {
        let before = self.used_idx(0);
        self.write_mem(CTL_REQ, request);
        // Poison the reply so a device that writes nothing is caught.
        self.write_mem(CTL_REPLY, &vec![0xab; reply_len]);
        self.chain(
            0,
            0,
            &[
                (CTL_REQ, request.len() as u32, 0),
                (CTL_REPLY, reply_len as u32, VIRTQ_DESC_F_WRITE),
            ],
        );
        self.kick(protocol::VQ_CONTROL);
        let after = self.used_idx(0);
        assert_eq!(after, before.wrapping_add(1), "control chain was not used");
        let (head, len) = self.used_elem(0, before);
        assert_eq!(head, 0);
        let reply = self.read_mem(CTL_REPLY, reply_len);
        let status = u32::from_le_bytes([reply[0], reply[1], reply[2], reply[3]]);
        (status, reply[4..].to_vec(), len)
    }

    fn query(&mut self, code: u32, start_id: u32, count: u32, size: u32) -> (u32, Vec<u8>) {
        let request = QueryInfo {
            code,
            start_id,
            count,
            size,
        }
        .encode();
        let reply_len = protocol::HDR_LEN + (count as usize) * (size as usize);
        let (status, payload, _) = self.control(&request, reply_len.max(protocol::HDR_LEN));
        (status, payload)
    }

    fn pcm_command(&mut self, code: u32, stream_id: u32) -> u32 {
        let request = ItemHdr {
            code,
            id: stream_id,
        }
        .encode();
        self.control(&request, protocol::HDR_LEN).0
    }

    fn set_params(&mut self, params: RawSetParams) -> u32 {
        self.control(&params.encode(), protocol::HDR_LEN).0
    }

    fn good_params() -> RawSetParams {
        RawSetParams {
            stream_id: 0,
            buffer_bytes: BUFFER_BYTES as u32,
            period_bytes: PERIOD_BYTES as u32,
            features: 0,
            channels: CHANNELS,
            format: protocol::FMT_S16,
            rate: protocol::RATE_48000,
        }
    }

    /// SET_PARAMS + PREPARE, the state every playback test starts from.
    fn prepare_stream(&mut self) {
        assert_eq!(self.set_params(Self::good_params()), protocol::S_OK);
        assert_eq!(self.pcm_command(protocol::R_PCM_PREPARE, 0), protocol::S_OK);
    }

    // ------------------------------------------------------------ playback

    fn slot_hdr(slot: usize) -> u64 {
        TX_HDR + slot as u64 * 16
    }

    fn slot_data(slot: usize) -> u64 {
        TX_DATA + slot as u64 * SLOT_STRIDE
    }

    fn slot_status(slot: usize) -> u64 {
        TX_STATUS + slot as u64 * 16
    }

    // ------------------------------------------------------------- capture

    fn rx_hdr(slot: usize) -> u64 {
        RX_HDR + slot as u64 * 16
    }

    fn rx_data(slot: usize) -> u64 {
        RX_DATA + slot as u64 * SLOT_STRIDE
    }

    fn rx_status(slot: usize) -> u64 {
        RX_STATUS + slot as u64 * 16
    }

    /// Posts one **empty** capture buffer of `room` bytes into `slot`, the
    /// ownership inversion in three descriptors: a device-readable header, a
    /// device-*writable* audio buffer, and a device-writable status. Returns
    /// the chain head.
    fn queue_capture(&mut self, slot: usize, room: usize) -> u16 {
        self.write_mem(Self::rx_hdr(slot), &stream::INPUT_STREAM.to_le_bytes());
        // Poison the room so a device that writes nothing, or writes short, is
        // caught rather than passing on leftover zeroes.
        self.write_mem(Self::rx_data(slot), &vec![0xcd; room]);
        self.write_mem(Self::rx_status(slot), &[0xcd; protocol::PCM_STATUS_LEN]);
        let start = u16::try_from(slot).expect("slot fits") * DESCS_PER_CHAIN;
        self.chain(
            3,
            start,
            &[
                (Self::rx_hdr(slot), protocol::PCM_XFER_LEN as u32, 0),
                (Self::rx_data(slot), room as u32, VIRTQ_DESC_F_WRITE),
                (
                    Self::rx_status(slot),
                    protocol::PCM_STATUS_LEN as u32,
                    VIRTQ_DESC_F_WRITE,
                ),
            ],
        )
    }

    /// SET_PARAMS + PREPARE on the capture stream.
    fn prepare_capture(&mut self) {
        assert_eq!(
            self.set_params(RawSetParams {
                stream_id: stream::INPUT_STREAM,
                ..Self::good_params()
            }),
            protocol::S_OK
        );
        assert_eq!(
            self.pcm_command(protocol::R_PCM_PREPARE, stream::INPUT_STREAM),
            protocol::S_OK
        );
    }

    /// Queues one period into `slot`. Returns the chain head.
    fn queue_period(&mut self, slot: usize, pcm: &[u8]) -> u16 {
        self.write_mem(Self::slot_hdr(slot), &0u32.to_le_bytes());
        self.write_mem(Self::slot_data(slot), pcm);
        self.write_mem(Self::slot_status(slot), &[0xcd; protocol::PCM_STATUS_LEN]);
        let start = u16::try_from(slot).expect("slot fits") * DESCS_PER_CHAIN;
        self.chain(
            2,
            start,
            &[
                (Self::slot_hdr(slot), protocol::PCM_XFER_LEN as u32, 0),
                (Self::slot_data(slot), pcm.len() as u32, 0),
                (
                    Self::slot_status(slot),
                    protocol::PCM_STATUS_LEN as u32,
                    VIRTQ_DESC_F_WRITE,
                ),
            ],
        )
    }
}

/// A 440 Hz sine at `RATE_HZ`, stereo S16LE, starting at absolute frame
/// `from`. Deterministic, so the test can compare the capture byte for byte.
fn tone(from: usize, frames: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(frames * 4);
    for i in 0..frames {
        let t = (from + i) as f64 / f64::from(RATE_HZ);
        let value = ((t * TONE_HZ * std::f64::consts::TAU).sin() * 16000.0) as i16;
        out.extend_from_slice(&value.to_le_bytes());
        out.extend_from_slice(&value.to_le_bytes());
    }
    out
}

fn status_of(harness: &Harness, slot: usize) -> u32 {
    let raw = harness.read_mem(Harness::slot_status(slot), protocol::PCM_STATUS_LEN);
    u32::from_le_bytes([raw[0], raw[1], raw[2], raw[3]])
}

fn rx_status_of(harness: &Harness, slot: usize) -> u32 {
    let raw = harness.read_mem(Harness::rx_status(slot), protocol::PCM_STATUS_LEN);
    u32::from_le_bytes([raw[0], raw[1], raw[2], raw[3]])
}

/// The exact bytes the synthetic microphone produces for `frames` frames
/// starting at absolute frame `from`. Deterministic, so the test can compare
/// the recording byte for byte.
fn capture_tone(from: u64, frames: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(frames * 4);
    for i in 0..frames {
        let value = ToneSource::sample_at(CAPTURE_HZ, RATE_HZ, from + i as u64);
        out.extend_from_slice(&value.to_le_bytes());
        out.extend_from_slice(&value.to_le_bytes());
    }
    out
}

// ============================================================ well-behaved

#[test]
fn the_config_space_advertises_a_jack_a_stream_and_a_channel_map_each_way() {
    let mut harness = Harness::unpaced();
    assert_eq!(
        harness.config(),
        (stream::JACKS, stream::STREAMS, stream::CHMAPS)
    );
    assert_eq!(
        harness.config(),
        (2, 2, 2),
        "one of each, in each direction"
    );
}

#[test]
fn the_information_queries_answer_what_the_config_space_promised() {
    let mut harness = Harness::unpaced();

    let (status, payload) = harness.query(
        protocol::R_JACK_INFO,
        0,
        stream::JACKS,
        protocol::JACK_INFO_LEN as u32,
    );
    assert_eq!(status, protocol::S_OK);
    assert_eq!(
        payload.len(),
        stream::JACKS as usize * protocol::JACK_INFO_LEN
    );
    for id in 0..stream::JACKS as usize {
        let at = id * protocol::JACK_INFO_LEN;
        assert_eq!(payload[at + 16], 1, "jack {id} reports itself connected");
    }

    let (status, payload) = harness.query(
        protocol::R_PCM_INFO,
        0,
        stream::STREAMS,
        protocol::PCM_INFO_LEN as u32,
    );
    assert_eq!(status, protocol::S_OK);
    assert_eq!(
        payload.len(),
        stream::STREAMS as usize * protocol::PCM_INFO_LEN,
        "one record per advertised stream"
    );
    // Both streams, in id order, and — the point of the whole phase — the
    // second one points the other way while offering the identical set.
    for (id, expected_direction) in [
        (stream::OUTPUT_STREAM, protocol::D_OUTPUT),
        (stream::INPUT_STREAM, protocol::D_INPUT),
    ] {
        let at = id as usize * protocol::PCM_INFO_LEN;
        let record = &payload[at..at + protocol::PCM_INFO_LEN];
        let formats = u64::from_le_bytes(record[8..16].try_into().expect("8 bytes"));
        let rates = u64::from_le_bytes(record[16..24].try_into().expect("8 bytes"));
        assert_eq!(formats, stream::formats_bitmap(), "stream {id} formats");
        assert_eq!(rates, stream::rates_bitmap(), "stream {id} rates");
        assert_eq!(record[24], expected_direction, "stream {id} direction");
        assert_eq!(record[25], stream::MIN_CHANNELS);
        assert_eq!(record[26], stream::MAX_CHANNELS);
    }

    let (status, payload) = harness.query(
        protocol::R_CHMAP_INFO,
        0,
        stream::CHMAPS,
        protocol::CHMAP_INFO_LEN as u32,
    );
    assert_eq!(status, protocol::S_OK);
    for (id, expected_direction) in [
        (stream::OUTPUT_STREAM, protocol::D_OUTPUT),
        (stream::INPUT_STREAM, protocol::D_INPUT),
    ] {
        let at = id as usize * protocol::CHMAP_INFO_LEN;
        let record = &payload[at..at + protocol::CHMAP_INFO_LEN];
        assert_eq!(record[4], expected_direction, "chmap {id} direction");
        assert_eq!(record[5], 2, "stereo");
        assert_eq!(record[6], protocol::CHMAP_FL);
        assert_eq!(record[7], protocol::CHMAP_FR);
    }

    // And the jacks: a line-out and a microphone, both plugged in.
    let (status, payload) = harness.query(
        protocol::R_JACK_INFO,
        0,
        stream::JACKS,
        protocol::JACK_INFO_LEN as u32,
    );
    assert_eq!(status, protocol::S_OK);
    let defconf = |id: usize| {
        let at = id * protocol::JACK_INFO_LEN;
        u32::from_le_bytes(payload[at + 8..at + 12].try_into().expect("4 bytes"))
    };
    assert_eq!(defconf(0), protocol::DEFCONF_LINE_OUT);
    assert_eq!(defconf(1), protocol::DEFCONF_MIC_IN);
}

/// The whole point of the crate: a guest plays a known tone and the host sink
/// receives it — at the right rate, in the right format, in order, complete,
/// without underruns, and *paced* rather than swallowed instantly.
#[test]
fn a_guest_playing_a_tone_is_heard_by_the_host_sink() {
    let mut harness = Harness::new();
    harness.prepare_stream();

    let mut free: Vec<usize> = (0..SLOTS).rev().collect();
    let mut in_flight: Vec<(u16, usize)> = Vec::new();
    let mut queued = 0usize;
    let mut completed = 0usize;
    let mut seen = harness.used_idx(2);
    let mut started = false;
    let mut underruns_after_tone: Option<u64> = None;
    let mut tone_done_at: Option<Instant> = None;

    let began = Instant::now();
    let deadline = began + Duration::from_secs(20);
    while completed < TOTAL_PERIODS {
        assert!(Instant::now() < deadline, "playback stalled at {completed}");

        // Keep the hardware buffer full, exactly as ALSA does.
        let mut submitted = false;
        while queued < TOTAL_PERIODS && in_flight.len() < PERIODS_IN_BUFFER {
            let Some(slot) = free.pop() else { break };
            let pcm = if queued < TONE_PERIODS {
                tone(queued * PERIOD_FRAMES, PERIOD_FRAMES)
            } else {
                vec![0u8; PERIOD_BYTES]
            };
            let head = harness.queue_period(slot, &pcm);
            in_flight.push((head, slot));
            queued += 1;
            submitted = true;
        }
        if submitted {
            harness.kick(protocol::VQ_TX);
        }
        // ALSA fills the buffer before it starts the stream.
        if !started && queued >= PERIODS_IN_BUFFER {
            assert_eq!(
                harness.pcm_command(protocol::R_PCM_START, 0),
                protocol::S_OK
            );
            started = true;
        }

        // Reap completions.
        let idx = harness.used_idx(2);
        while seen != idx {
            let (head, len) = harness.used_elem(2, seen);
            assert_eq!(
                len,
                protocol::PCM_STATUS_LEN as u32,
                "a completion writes exactly one virtio_snd_pcm_status"
            );
            let head = u16::try_from(head).expect("head fits");
            let position = in_flight
                .iter()
                .position(|(h, _)| *h == head)
                .expect("completion for a chain we never queued");
            let (_, slot) = in_flight.remove(position);
            assert_eq!(
                status_of(&harness, slot),
                protocol::S_OK,
                "period {completed} was refused"
            );
            free.push(slot);
            completed += 1;
            seen = seen.wrapping_add(1);
            if completed == TONE_PERIODS {
                underruns_after_tone = Some(harness.stats.underruns());
                tone_done_at = Some(Instant::now());
            }
        }
        std::thread::sleep(Duration::from_millis(1));
    }

    assert_eq!(
        underruns_after_tone,
        Some(0),
        "the guest kept the buffer full, so nothing may have underrun"
    );

    // Period-based completion, not "copied and forgotten": eight 10 ms periods
    // cannot all have played inside a millisecond.
    let elapsed = tone_done_at
        .unwrap_or_else(Instant::now)
        .duration_since(began);
    assert!(
        elapsed >= Duration::from_millis(50),
        "{TONE_PERIODS} periods of 10 ms completed in {elapsed:?} — completion is not paced"
    );

    assert_eq!(harness.pcm_command(protocol::R_PCM_STOP, 0), protocol::S_OK);
    assert_eq!(
        harness.pcm_command(protocol::R_PCM_RELEASE, 0),
        protocol::S_OK
    );

    // What the host heard.
    let recording = &harness.recording;
    assert_eq!(
        recording.format(),
        Some(virtio_sound::StreamFormat {
            rate_hz: RATE_HZ,
            channels: CHANNELS,
        }),
        "the sink was opened for the format the guest negotiated"
    );
    let captured = recording.pcm();
    let expected = tone(0, TONE_PERIODS * PERIOD_FRAMES);
    assert!(
        captured.len() >= expected.len(),
        "only {} of {} bytes reached the sink",
        captured.len(),
        expected.len()
    );
    assert_eq!(
        &captured[..expected.len()],
        &expected[..],
        "the samples the host received are not the ones the guest sent"
    );

    // And it really is a 440 Hz tone: count sign changes on the left channel.
    let samples = recording.samples();
    let left: Vec<i16> = samples
        .iter()
        .step_by(2)
        .take(TONE_PERIODS * PERIOD_FRAMES)
        .copied()
        .collect();
    let crossings = left.windows(2).filter(|w| (w[0] < 0) != (w[1] < 0)).count();
    let seconds = (TONE_PERIODS * PERIOD_FRAMES) as f64 / f64::from(RATE_HZ);
    let hz = crossings as f64 / (2.0 * seconds);
    assert!(
        (hz - TONE_HZ).abs() < 20.0,
        "the captured audio measures {hz:.1} Hz, not {TONE_HZ} Hz"
    );

    assert!(harness.irq.count() > 0, "completions must raise interrupts");
}

/// The other half of the crate: a guest *records* a known signal, and the
/// samples that reach it are the ones the host produced — at the right rate,
/// in the right format, in order, the right number of them, without overruns,
/// and *paced* rather than handed over instantly.
///
/// Read it against `a_guest_playing_a_tone_is_heard_by_the_host_sink`: the
/// shape is deliberately identical, because the ownership is the only thing
/// that inverts. There the guest fills buffers and the device drains them;
/// here the guest posts empty ones and the device fills them, and the pacing
/// assertion at the end is the same claim about the same clock.
#[test]
fn a_guest_recording_hears_the_host_source() {
    let mut harness = Harness::new();
    harness.prepare_capture();

    let mut free: Vec<usize> = (0..SLOTS).rev().collect();
    let mut in_flight: Vec<(u16, usize)> = Vec::new();
    let mut posted = 0usize;
    let mut completed = 0usize;
    let mut seen = harness.used_idx(3);
    let mut started = false;
    let mut recorded: Vec<u8> = Vec::new();
    let mut overruns_after_tone: Option<u64> = None;
    let mut tone_done_at: Option<Instant> = None;

    let began = Instant::now();
    let deadline = began + Duration::from_secs(20);
    while completed < TOTAL_PERIODS {
        assert!(Instant::now() < deadline, "capture stalled at {completed}");

        // Keep the hardware buffer full of empty buffers, exactly as ALSA does
        // on a capture stream.
        let mut submitted = false;
        while posted < TOTAL_PERIODS && in_flight.len() < PERIODS_IN_BUFFER {
            let Some(slot) = free.pop() else { break };
            let head = harness.queue_capture(slot, PERIOD_BYTES);
            in_flight.push((head, slot));
            posted += 1;
            submitted = true;
        }
        if submitted {
            harness.kick(protocol::VQ_RX);
        }
        // ALSA posts the whole buffer before it starts the stream.
        if !started && posted >= PERIODS_IN_BUFFER {
            assert_eq!(
                harness.pcm_command(protocol::R_PCM_START, stream::INPUT_STREAM),
                protocol::S_OK
            );
            started = true;
        }

        // Reap completions, in the order the device filled them.
        let idx = harness.used_idx(3);
        while seen != idx {
            let (head, len) = harness.used_elem(3, seen);
            assert_eq!(
                len as usize,
                PERIOD_BYTES + protocol::PCM_STATUS_LEN,
                "a filled capture buffer reports its audio plus one status"
            );
            let head = u16::try_from(head).expect("head fits");
            let position = in_flight
                .iter()
                .position(|(h, _)| *h == head)
                .expect("completion for a buffer we never posted");
            let (_, slot) = in_flight.remove(position);
            assert_eq!(
                rx_status_of(&harness, slot),
                protocol::S_OK,
                "capture buffer {completed} was refused"
            );
            recorded.extend_from_slice(&harness.read_mem(Harness::rx_data(slot), PERIOD_BYTES));
            free.push(slot);
            completed += 1;
            seen = seen.wrapping_add(1);
            if completed == TONE_PERIODS {
                overruns_after_tone = Some(harness.stats.overruns());
                tone_done_at = Some(Instant::now());
            }
        }
        std::thread::sleep(Duration::from_millis(1));
    }

    assert_eq!(
        overruns_after_tone,
        Some(0),
        "the guest kept a buffer posted, so nothing may have overrun"
    );

    // Period-based completion, not "answered on arrival": eight 10 ms periods
    // of audio cannot have existed inside a millisecond.
    let elapsed = tone_done_at
        .unwrap_or_else(Instant::now)
        .duration_since(began);
    assert!(
        elapsed >= Duration::from_millis(50),
        "{TONE_PERIODS} periods of 10 ms were recorded in {elapsed:?} — capture is not paced"
    );

    assert_eq!(
        harness.pcm_command(protocol::R_PCM_STOP, stream::INPUT_STREAM),
        protocol::S_OK
    );
    assert_eq!(
        harness.pcm_command(protocol::R_PCM_RELEASE, stream::INPUT_STREAM),
        protocol::S_OK
    );

    // The source was opened for the format the guest negotiated...
    assert_eq!(
        harness.source_log.format(),
        Some(virtio_sound::StreamFormat {
            rate_hz: RATE_HZ,
            channels: CHANNELS,
        })
    );
    // ...and the frame count is exactly what was asked for, no more.
    assert_eq!(
        recorded.len(),
        TOTAL_PERIODS * PERIOD_BYTES,
        "the guest received {} bytes for {TOTAL_PERIODS} periods",
        recorded.len()
    );
    assert_eq!(recorded.len() / 4, TOTAL_PERIODS * PERIOD_FRAMES, "frames");

    // And they are the exact samples the host produced, in order.
    let expected = capture_tone(0, TOTAL_PERIODS * PERIOD_FRAMES);
    assert_eq!(
        recorded, expected,
        "the samples the guest received are not the ones the host produced"
    );

    // Belt and braces: it really measures as a 1 kHz tone at 48 kHz stereo.
    let left: Vec<i16> = recorded
        .chunks_exact(4)
        .map(|f| i16::from_le_bytes([f[0], f[1]]))
        .collect();
    let crossings = left.windows(2).filter(|w| (w[0] < 0) != (w[1] < 0)).count();
    let seconds = left.len() as f64 / f64::from(RATE_HZ);
    let hz = crossings as f64 / (2.0 * seconds);
    assert!(
        (hz - CAPTURE_HZ).abs() < 20.0,
        "the recording measures {hz:.1} Hz, not {CAPTURE_HZ} Hz"
    );

    assert!(
        harness.stats.bytes_captured() >= recorded.len() as u64,
        "the counters must account for every byte the guest received"
    );
    assert!(harness.irq.count() > 0, "completions must raise interrupts");
}

/// Both directions at once, which is what a voice call is. Neither stream may
/// starve the other, and each must get its own audio rather than the other's.
#[test]
fn playback_and_capture_run_at_the_same_time_without_crossing_over() {
    let mut harness = Harness::new();
    harness.prepare_stream();
    harness.prepare_capture();

    // Fill both hardware buffers, then start both streams.
    for slot in 0..PERIODS_IN_BUFFER {
        harness.queue_period(slot, &tone(slot * PERIOD_FRAMES, PERIOD_FRAMES));
        harness.queue_capture(slot, PERIOD_BYTES);
    }
    harness.kick(protocol::VQ_TX);
    harness.kick(protocol::VQ_RX);
    assert_eq!(
        harness.pcm_command(protocol::R_PCM_START, stream::OUTPUT_STREAM),
        protocol::S_OK
    );
    assert_eq!(
        harness.pcm_command(protocol::R_PCM_START, stream::INPUT_STREAM),
        protocol::S_OK
    );

    let deadline = Instant::now() + Duration::from_secs(20);
    while harness.used_idx(2) == 0 || harness.used_idx(3) == 0 {
        assert!(
            Instant::now() < deadline,
            "one direction never completed: tx {} rx {}",
            harness.used_idx(2),
            harness.used_idx(3)
        );
        std::thread::sleep(Duration::from_millis(2));
    }

    // The played audio is the 440 Hz tone the guest sent...
    let played = harness.recording.pcm();
    assert!(!played.is_empty());
    assert_eq!(
        &played[..PERIOD_BYTES.min(played.len())],
        &tone(0, PERIOD_FRAMES)[..PERIOD_BYTES.min(played.len())],
        "the sink received something other than what the guest played"
    );
    // ...and the recorded audio is the 1 kHz tone the microphone produced.
    let recorded = harness.read_mem(Harness::rx_data(0), PERIOD_BYTES);
    assert_eq!(
        recorded,
        capture_tone(0, PERIOD_FRAMES),
        "the guest recorded the audio it was playing, not the audio it was hearing"
    );

    for id in [stream::OUTPUT_STREAM, stream::INPUT_STREAM] {
        assert_eq!(
            harness.pcm_command(protocol::R_PCM_STOP, id),
            protocol::S_OK
        );
        assert_eq!(
            harness.pcm_command(protocol::R_PCM_RELEASE, id),
            protocol::S_OK
        );
    }
}

/// The lifecycle the driver actually walks, and the states it may not skip.
#[test]
fn the_stream_lifecycle_is_enforced_over_the_wire() {
    let mut harness = Harness::unpaced();

    // Nothing works before SET_PARAMS.
    for code in [
        protocol::R_PCM_PREPARE,
        protocol::R_PCM_START,
        protocol::R_PCM_STOP,
        protocol::R_PCM_RELEASE,
    ] {
        assert_eq!(
            harness.pcm_command(code, 0),
            protocol::S_BAD_MSG,
            "{} before SET_PARAMS",
            stream::command_name(code)
        );
    }

    assert_eq!(harness.set_params(Harness::good_params()), protocol::S_OK);
    // START before PREPARE is still refused.
    assert_eq!(
        harness.pcm_command(protocol::R_PCM_START, 0),
        protocol::S_BAD_MSG
    );
    assert_eq!(
        harness.pcm_command(protocol::R_PCM_PREPARE, 0),
        protocol::S_OK
    );
    // STOP before START is refused.
    assert_eq!(
        harness.pcm_command(protocol::R_PCM_STOP, 0),
        protocol::S_BAD_MSG
    );
    assert_eq!(
        harness.pcm_command(protocol::R_PCM_START, 0),
        protocol::S_OK
    );
    // And a second START is not a no-op, it is a protocol error.
    assert_eq!(
        harness.pcm_command(protocol::R_PCM_START, 0),
        protocol::S_BAD_MSG
    );
    assert_eq!(harness.pcm_command(protocol::R_PCM_STOP, 0), protocol::S_OK);
    assert_eq!(
        harness.pcm_command(protocol::R_PCM_RELEASE, 0),
        protocol::S_OK
    );
    // Released: back to square one.
    assert_eq!(
        harness.pcm_command(protocol::R_PCM_PREPARE, 0),
        protocol::S_BAD_MSG
    );
}

/// "The device MUST complete all pending I/O messages for the specified
/// stream" — a STOP must hand every buffer back, or the guest's ring drains to
/// a halt and playback never recovers.
#[test]
fn stopping_a_stream_returns_every_buffer_it_was_holding() {
    let mut harness = Harness::unpaced();
    harness.prepare_stream();

    let before = harness.used_idx(2);
    for slot in 0..PERIODS_IN_BUFFER {
        harness.queue_period(slot, &vec![1u8; PERIOD_BYTES]);
    }
    harness.kick(protocol::VQ_TX);
    // Nothing has started, so nothing may have been retired yet.
    assert_eq!(
        harness.used_idx(2),
        before,
        "a prepared stream plays nothing"
    );

    assert_eq!(
        harness.pcm_command(protocol::R_PCM_STOP, 0),
        protocol::S_BAD_MSG,
        "the stream was never started"
    );
    assert_eq!(
        harness.pcm_command(protocol::R_PCM_RELEASE, 0),
        protocol::S_OK
    );
    assert_eq!(
        harness.used_idx(2),
        before.wrapping_add(PERIODS_IN_BUFFER as u16),
        "RELEASE must return every pending playback message"
    );
    for slot in 0..PERIODS_IN_BUFFER {
        assert_eq!(status_of(&harness, slot), protocol::S_OK);
    }
}

/// A device reset (a guest reboot) must return everything to power-on and stop
/// the pump thread, and the driver must be able to bring it straight back up.
#[test]
fn a_reset_stops_the_pump_and_the_device_comes_back() {
    let mut harness = Harness::new();
    harness.prepare_stream();
    for slot in 0..2 {
        harness.queue_period(slot, &tone(slot * PERIOD_FRAMES, PERIOD_FRAMES));
    }
    harness.kick(protocol::VQ_TX);
    assert_eq!(
        harness.pcm_command(protocol::R_PCM_START, 0),
        protocol::S_OK
    );

    // Write 0 to STATUS: the driver-facing device reset.
    harness.write32(mmio::STATUS, 0);
    assert!(!harness.transport.is_activated());
    assert_eq!(harness.transport.status(), 0);

    // And back up again, on rewound rings.
    for ring in harness.rings {
        ring.rewind(&harness.mem);
    }
    harness.bring_up();
    harness.prepare_stream();
    assert_eq!(
        harness.pcm_command(protocol::R_PCM_START, 0),
        protocol::S_OK
    );
    assert_eq!(harness.pcm_command(protocol::R_PCM_STOP, 0), protocol::S_OK);
}

// ============================================================= hostile guest

#[test]
fn short_unknown_and_unsupported_control_messages_are_refused_in_band() {
    let mut harness = Harness::unpaced();

    // Shorter than the four-byte header.
    let (status, _, _) = harness.control(&[1, 2, 3], protocol::HDR_LEN);
    assert_eq!(status, protocol::S_BAD_MSG);

    // A code nobody defines.
    let (status, _, _) = harness.control(&0xdead_beefu32.to_le_bytes(), protocol::HDR_LEN);
    assert_eq!(status, protocol::S_NOT_SUPP);

    // JACK_REMAP: well formed, but we publish no remappable jack.
    let mut remap = [0u8; protocol::JACK_REMAP_LEN];
    remap[0..4].copy_from_slice(&protocol::R_JACK_REMAP.to_le_bytes());
    let (status, _, _) = harness.control(&remap, protocol::HDR_LEN);
    assert_eq!(status, protocol::S_NOT_SUPP);

    // A lifecycle command whose message is truncated below its header.
    let (status, _, _) = harness.control(&protocol::R_PCM_START.to_le_bytes(), protocol::HDR_LEN);
    assert_eq!(status, protocol::S_BAD_MSG);
}

/// The `size` field of an info query is the driver telling us how big it
/// believes one record is. Writing our struct into a buffer sized for a
/// different one is exactly what it exists to prevent.
#[test]
fn info_queries_outside_the_advertised_range_or_with_the_wrong_record_size_are_refused() {
    let mut harness = Harness::unpaced();

    for size in [0u32, 1, protocol::PCM_INFO_LEN as u32 - 1, 4096] {
        let (status, _) = harness.query(protocol::R_PCM_INFO, 0, 1, size);
        assert_eq!(status, protocol::S_BAD_MSG, "record size {size}");
    }
    for (start_id, count) in [
        (stream::STREAMS, 1),
        (0, stream::STREAMS + 1),
        (1, u32::MAX),
        (u32::MAX, 1),
        (u32::MAX, u32::MAX),
    ] {
        let request = QueryInfo {
            code: protocol::R_PCM_INFO,
            start_id,
            count,
            size: protocol::PCM_INFO_LEN as u32,
        }
        .encode();
        // Deliberately a small reply buffer: the device must refuse on the
        // range, never try to fill `count` records.
        let (status, _, _) = harness.control(&request, protocol::HDR_LEN);
        assert_eq!(status, protocol::S_BAD_MSG, "{start_id}+{count}");
    }
    // A query the reply buffer cannot hold is refused rather than truncated.
    let request = QueryInfo {
        code: protocol::R_CHMAP_INFO,
        start_id: 0,
        count: 1,
        size: protocol::CHMAP_INFO_LEN as u32,
    }
    .encode();
    let (status, _, len) = harness.control(&request, protocol::HDR_LEN);
    assert_eq!(status, protocol::S_BAD_MSG);
    assert_eq!(len, protocol::HDR_LEN as u32);
}

#[test]
fn set_params_refuses_everything_the_device_never_advertised() {
    let mut harness = Harness::unpaced();
    let good = Harness::good_params();

    // Formats and rates outside the advertised set.
    for format in [protocol::FMT_S32, protocol::FMT_FLOAT, 0, 255] {
        assert_eq!(
            harness.set_params(RawSetParams { format, ..good }),
            protocol::S_NOT_SUPP,
            "format {format}"
        );
    }
    for rate in [protocol::RATE_8000, protocol::RATE_192000, 0, 255] {
        assert_eq!(
            harness.set_params(RawSetParams { rate, ..good }),
            protocol::S_NOT_SUPP,
            "rate {rate}"
        );
    }
    // Channel counts the chmap does not describe.
    for channels in [0u8, 3, 18, 255] {
        assert_eq!(
            harness.set_params(RawSetParams { channels, ..good }),
            protocol::S_NOT_SUPP,
            "channels {channels}"
        );
    }
    // Stream features we do not offer.
    for features in [protocol::PCM_F_SHMEM_HOST, protocol::PCM_F_MSG_POLLING, !0] {
        assert_eq!(
            harness.set_params(RawSetParams { features, ..good }),
            protocol::S_NOT_SUPP
        );
    }
    // Identifiers outside the config space.
    for stream_id in [stream::STREAMS, u32::MAX] {
        assert_eq!(
            harness.set_params(RawSetParams { stream_id, ..good }),
            protocol::S_BAD_MSG
        );
    }
    // Geometry that would make the host allocate without bound, or divide by
    // zero, or desynchronise the channels.
    for (period_bytes, buffer_bytes) in [
        (0, 0),
        (0, BUFFER_BYTES as u32),
        (PERIOD_BYTES as u32, 0),
        (PERIOD_BYTES as u32, PERIOD_BYTES as u32),
        (PERIOD_BYTES as u32, u32::MAX),
        (u32::MAX, u32::MAX),
        (stream::MAX_PERIOD_BYTES + 4, stream::MAX_BUFFER_BYTES),
        (PERIOD_BYTES as u32 + 2, BUFFER_BYTES as u32),
        (32, 4096),
    ] {
        assert_eq!(
            harness.set_params(RawSetParams {
                period_bytes,
                buffer_bytes,
                ..good
            }),
            protocol::S_NOT_SUPP,
            "period {period_bytes} buffer {buffer_bytes}"
        );
    }
    // And the good one still works afterwards: nothing above left state behind.
    assert_eq!(harness.set_params(good), protocol::S_OK);
}

#[test]
fn playback_messages_the_stream_cannot_accept_are_answered_not_played() {
    let mut harness = Harness::unpaced();

    // Before SET_PARAMS the stream takes no I/O at all.
    harness.queue_period(0, &vec![0u8; PERIOD_BYTES]);
    harness.kick(protocol::VQ_TX);
    assert_eq!(status_of(&harness, 0), protocol::S_BAD_MSG);

    harness.prepare_stream();

    // An unknown stream id.
    harness.write_mem(Harness::slot_hdr(1), &7u32.to_le_bytes());
    harness.write_mem(Harness::slot_data(1), &[0u8; PERIOD_BYTES]);
    harness.write_mem(Harness::slot_status(1), &[0xcd; protocol::PCM_STATUS_LEN]);
    harness.chain(
        2,
        DESCS_PER_CHAIN,
        &[
            (Harness::slot_hdr(1), protocol::PCM_XFER_LEN as u32, 0),
            (Harness::slot_data(1), PERIOD_BYTES as u32, 0),
            (
                Harness::slot_status(1),
                protocol::PCM_STATUS_LEN as u32,
                VIRTQ_DESC_F_WRITE,
            ),
        ],
    );
    harness.kick(protocol::VQ_TX);
    assert_eq!(status_of(&harness, 1), protocol::S_BAD_MSG);

    // A payload that is not a whole number of frames, one larger than the
    // negotiated period, and an empty one.
    for (slot, len) in [(2usize, PERIOD_BYTES + 2), (3, PERIOD_BYTES * 2), (4, 0)] {
        harness.write_mem(Harness::slot_hdr(slot), &0u32.to_le_bytes());
        harness.write_mem(
            Harness::slot_status(slot),
            &[0xcd; protocol::PCM_STATUS_LEN],
        );
        let start = u16::try_from(slot).expect("slot fits") * DESCS_PER_CHAIN;
        let mut descs = vec![(Harness::slot_hdr(slot), protocol::PCM_XFER_LEN as u32, 0u16)];
        if len > 0 {
            descs.push((Harness::slot_data(slot), len as u32, 0));
        }
        descs.push((
            Harness::slot_status(slot),
            protocol::PCM_STATUS_LEN as u32,
            VIRTQ_DESC_F_WRITE,
        ));
        harness.chain(2, start, &descs);
        harness.kick(protocol::VQ_TX);
        assert_eq!(
            status_of(&harness, slot),
            protocol::S_BAD_MSG,
            "payload of {len} bytes"
        );
    }
}

/// A guest that queues far more than the buffer it negotiated must be refused,
/// not allowed to grow the host ring.
#[test]
fn flooding_the_ring_beyond_the_negotiated_buffer_is_an_io_error() {
    let mut harness = Harness::unpaced();
    harness.prepare_stream();

    let mut refused = 0usize;
    for slot in 0..SLOTS {
        harness.queue_period(slot, &vec![7u8; PERIOD_BYTES]);
        harness.kick(protocol::VQ_TX);
        if status_of(&harness, slot) == protocol::S_IO_ERR {
            refused += 1;
        }
    }
    assert!(
        refused > 0,
        "{SLOTS} periods fitted into a {PERIODS_IN_BUFFER}-period buffer"
    );
    // The device is still alive and still answers control messages.
    assert_eq!(
        harness.pcm_command(protocol::R_PCM_RELEASE, 0),
        protocol::S_OK
    );
}

/// Chains a real driver could never build. None may panic; none may make the
/// device write outside the buffers the guest offered.
#[test]
fn malformed_descriptor_chains_are_dropped_without_taking_the_device_down() {
    let mut harness = Harness::unpaced();
    harness.prepare_stream();

    // 1. A chain that loops back on itself.
    let before = harness.used_idx(2);
    harness.rings[2].write_desc(&harness.mem, 0, SCRATCH, 16, VIRTQ_DESC_F_NEXT, 1);
    harness.rings[2].write_desc(&harness.mem, 1, SCRATCH, 16, VIRTQ_DESC_F_NEXT, 0);
    harness.rings[2].publish(&harness.mem, 0);
    harness.kick(protocol::VQ_TX);
    assert_eq!(harness.used_idx(2), before.wrapping_add(1));
    assert_eq!(
        harness.used_elem(2, before).1,
        0,
        "a looped chain is dropped"
    );

    // 2. Indirect descriptors, which we never offered.
    let before = harness.used_idx(0);
    harness.rings[0].write_desc(&harness.mem, 0, SCRATCH, 16, VIRTQ_DESC_F_INDIRECT, 0);
    harness.rings[0].publish(&harness.mem, 0);
    harness.kick(protocol::VQ_CONTROL);
    assert_eq!(harness.used_idx(0), before.wrapping_add(1));
    assert_eq!(harness.used_elem(0, before).1, 0);

    // 3. A control chain with nowhere to write a reply.
    let before = harness.used_idx(0);
    harness.write_mem(CTL_REQ, &protocol::R_PCM_INFO.to_le_bytes());
    harness.chain(0, 0, &[(CTL_REQ, protocol::QUERY_INFO_LEN as u32, 0)]);
    harness.kick(protocol::VQ_CONTROL);
    assert_eq!(harness.used_elem(0, before).1, 0);

    // 4. A playback chain with no status word.
    let before = harness.used_idx(2);
    harness.chain(
        2,
        0,
        &[
            (TX_HDR, protocol::PCM_XFER_LEN as u32, 0),
            (TX_DATA, PERIOD_BYTES as u32, 0),
        ],
    );
    harness.kick(protocol::VQ_TX);
    assert_eq!(harness.used_elem(2, before).1, 0);

    // 5. A playback chain whose status word is too small to hold one.
    let before = harness.used_idx(2);
    harness.chain(
        2,
        0,
        &[
            (TX_HDR, protocol::PCM_XFER_LEN as u32, 0),
            (SCRATCH, 4, VIRTQ_DESC_F_WRITE),
        ],
    );
    harness.kick(protocol::VQ_TX);
    assert_eq!(harness.used_elem(2, before).1, 0);

    // 6. Buffers that point outside guest RAM.
    let outside = MEM_SIZE + 0x1000;
    let before = harness.used_idx(0);
    harness.chain(
        0,
        0,
        &[
            (outside, protocol::QUERY_INFO_LEN as u32, 0),
            (CTL_REPLY, protocol::HDR_LEN as u32, VIRTQ_DESC_F_WRITE),
        ],
    );
    harness.kick(protocol::VQ_CONTROL);
    let reply = harness.read_mem(CTL_REPLY, protocol::HDR_LEN);
    assert_eq!(
        u32::from_le_bytes([reply[0], reply[1], reply[2], reply[3]]),
        protocol::S_BAD_MSG
    );
    assert_eq!(harness.used_idx(0), before.wrapping_add(1));

    // 7. A control message far larger than the message cap, built out of many
    //    descriptors so the length only shows up once they are added together.
    let before = harness.used_idx(0);
    let mut descs = Vec::new();
    for i in 0..40u16 {
        descs.push((SCRATCH + u64::from(i) * 256, 256u32, 0u16));
    }
    descs.push((CTL_REPLY, protocol::HDR_LEN as u32, VIRTQ_DESC_F_WRITE));
    harness.chain(0, 0, &descs);
    harness.kick(protocol::VQ_CONTROL);
    let reply = harness.read_mem(CTL_REPLY, protocol::HDR_LEN);
    assert_eq!(
        u32::from_le_bytes([reply[0], reply[1], reply[2], reply[3]]),
        protocol::S_BAD_MSG,
        "a message past MAX_CONTROL_MSG_BYTES must be refused before it is read"
    );
    assert_eq!(harness.used_idx(0), before.wrapping_add(1));

    // The device survived all of it and still serves a good request.
    assert_eq!(
        harness.pcm_command(protocol::R_PCM_RELEASE, 0),
        protocol::S_OK
    );
}

/// Capture messages the stream cannot accept are answered, not filled — and
/// the interesting half is the *direction*: a guest that names the capture
/// stream on the playback queue, or the playback stream on the capture queue,
/// is asking the device to treat readable memory as writable or the reverse.
#[test]
fn capture_messages_the_stream_cannot_accept_are_answered_not_filled() {
    let mut harness = Harness::unpaced();

    // Before SET_PARAMS the capture stream takes no I/O at all.
    harness.queue_capture(0, PERIOD_BYTES);
    harness.kick(protocol::VQ_RX);
    assert_eq!(rx_status_of(&harness, 0), protocol::S_BAD_MSG);
    assert_eq!(
        harness.read_mem(Harness::rx_data(0), 4),
        vec![0xcd; 4],
        "a refused capture buffer must not be written into"
    );

    harness.prepare_capture();

    // The playback stream named on the capture queue.
    harness.write_mem(Harness::rx_hdr(1), &stream::OUTPUT_STREAM.to_le_bytes());
    harness.write_mem(Harness::rx_data(1), &vec![0xcd; PERIOD_BYTES]);
    harness.write_mem(Harness::rx_status(1), &[0xcd; protocol::PCM_STATUS_LEN]);
    harness.chain(
        3,
        DESCS_PER_CHAIN,
        &[
            (Harness::rx_hdr(1), protocol::PCM_XFER_LEN as u32, 0),
            (Harness::rx_data(1), PERIOD_BYTES as u32, VIRTQ_DESC_F_WRITE),
            (
                Harness::rx_status(1),
                protocol::PCM_STATUS_LEN as u32,
                VIRTQ_DESC_F_WRITE,
            ),
        ],
    );
    harness.kick(protocol::VQ_RX);
    assert_eq!(rx_status_of(&harness, 1), protocol::S_BAD_MSG);

    // A stream id nobody advertised.
    harness.write_mem(Harness::rx_hdr(2), &9u32.to_le_bytes());
    harness.write_mem(Harness::rx_status(2), &[0xcd; protocol::PCM_STATUS_LEN]);
    harness.chain(
        3,
        2 * DESCS_PER_CHAIN,
        &[
            (Harness::rx_hdr(2), protocol::PCM_XFER_LEN as u32, 0),
            (Harness::rx_data(2), PERIOD_BYTES as u32, VIRTQ_DESC_F_WRITE),
            (
                Harness::rx_status(2),
                protocol::PCM_STATUS_LEN as u32,
                VIRTQ_DESC_F_WRITE,
            ),
        ],
    );
    harness.kick(protocol::VQ_RX);
    assert_eq!(rx_status_of(&harness, 2), protocol::S_BAD_MSG);

    // Room that is not a whole number of frames, more than one period, and
    // none at all.
    for (slot, room) in [(3usize, PERIOD_BYTES + 2), (4, PERIOD_BYTES * 2), (5, 0)] {
        harness.write_mem(Harness::rx_hdr(slot), &stream::INPUT_STREAM.to_le_bytes());
        harness.write_mem(Harness::rx_status(slot), &[0xcd; protocol::PCM_STATUS_LEN]);
        let start = u16::try_from(slot).expect("slot fits") * DESCS_PER_CHAIN;
        let mut descs = vec![(Harness::rx_hdr(slot), protocol::PCM_XFER_LEN as u32, 0u16)];
        if room > 0 {
            descs.push((Harness::rx_data(slot), room as u32, VIRTQ_DESC_F_WRITE));
        }
        descs.push((
            Harness::rx_status(slot),
            protocol::PCM_STATUS_LEN as u32,
            VIRTQ_DESC_F_WRITE,
        ));
        harness.chain(3, start, &descs);
        harness.kick(protocol::VQ_RX);
        assert_eq!(
            rx_status_of(&harness, slot),
            protocol::S_BAD_MSG,
            "capture buffer offering {room} bytes of room"
        );
    }

    // And the mirror image: the capture stream named on the playback queue.
    harness.prepare_stream();
    harness.write_mem(Harness::slot_hdr(0), &stream::INPUT_STREAM.to_le_bytes());
    harness.write_mem(Harness::slot_data(0), &[0u8; PERIOD_BYTES]);
    harness.write_mem(Harness::slot_status(0), &[0xcd; protocol::PCM_STATUS_LEN]);
    harness.chain(
        2,
        0,
        &[
            (Harness::slot_hdr(0), protocol::PCM_XFER_LEN as u32, 0),
            (Harness::slot_data(0), PERIOD_BYTES as u32, 0),
            (
                Harness::slot_status(0),
                protocol::PCM_STATUS_LEN as u32,
                VIRTQ_DESC_F_WRITE,
            ),
        ],
    );
    harness.kick(protocol::VQ_TX);
    assert_eq!(status_of(&harness, 0), protocol::S_BAD_MSG);
}

/// A capture chain with nowhere to report a status, and one whose readable
/// half is far bigger than a four-byte transfer header.
#[test]
fn malformed_capture_chains_are_dropped_or_refused() {
    let mut harness = Harness::unpaced();
    harness.prepare_capture();

    // A chain with no *separate* status word. On the capture queue the status
    // is by definition the last device-writable buffer (spec 5.14.6.8), so a
    // driver that offers only one writable descriptor has offered a status and
    // no room at all — which is refused, and refused *into that descriptor*,
    // never filled with audio the guest did not ask for.
    let before = harness.used_idx(3);
    harness.write_mem(RX_HDR, &stream::INPUT_STREAM.to_le_bytes());
    harness.write_mem(RX_DATA, &vec![0xcd; PERIOD_BYTES]);
    harness.chain(
        3,
        0,
        &[
            (RX_HDR, protocol::PCM_XFER_LEN as u32, 0),
            (RX_DATA, PERIOD_BYTES as u32, VIRTQ_DESC_F_WRITE),
        ],
    );
    harness.kick(protocol::VQ_RX);
    assert_eq!(harness.used_idx(3), before.wrapping_add(1));
    assert_eq!(
        harness.used_elem(3, before).1,
        protocol::PCM_STATUS_LEN as u32
    );
    let raw = harness.read_mem(RX_DATA, protocol::PCM_STATUS_LEN);
    assert_eq!(
        u32::from_le_bytes([raw[0], raw[1], raw[2], raw[3]]),
        protocol::S_BAD_MSG
    );
    assert_eq!(
        harness.read_mem(RX_DATA + protocol::PCM_STATUS_LEN as u64, 8),
        vec![0xcd; 8],
        "nothing past the status word may be touched"
    );

    // A chain with no device-writable buffer at all is unusable: there is
    // nowhere to put even a status, so it comes back with zero bytes used.
    let before = harness.used_idx(3);
    harness.chain(3, 0, &[(RX_HDR, protocol::PCM_XFER_LEN as u32, 0)]);
    harness.kick(protocol::VQ_RX);
    assert_eq!(harness.used_idx(3), before.wrapping_add(1));
    assert_eq!(harness.used_elem(3, before).1, 0);

    // A readable half past MAX_CAPTURE_HEADER_BYTES, built out of many
    // descriptors so the length only shows up once they are added together.
    let before = harness.used_idx(3);
    let mut descs = Vec::new();
    for i in 0..40u16 {
        descs.push((SCRATCH + u64::from(i) * 256, 256u32, 0u16));
    }
    descs.push((Harness::rx_data(6), PERIOD_BYTES as u32, VIRTQ_DESC_F_WRITE));
    harness.write_mem(Harness::rx_status(6), &[0xcd; protocol::PCM_STATUS_LEN]);
    descs.push((
        Harness::rx_status(6),
        protocol::PCM_STATUS_LEN as u32,
        VIRTQ_DESC_F_WRITE,
    ));
    harness.chain(3, 0, &descs);
    harness.kick(protocol::VQ_RX);
    assert_eq!(
        rx_status_of(&harness, 6),
        protocol::S_BAD_MSG,
        "a capture header past the cap must be refused before it is read"
    );
    assert_eq!(harness.used_idx(3), before.wrapping_add(1));
    assert_eq!(
        harness.used_elem(3, before).1,
        protocol::PCM_STATUS_LEN as u32,
        "a refused capture message reports only its status word as used"
    );

    // Still alive.
    assert_eq!(
        harness.pcm_command(protocol::R_PCM_RELEASE, stream::INPUT_STREAM),
        protocol::S_OK
    );
}

/// A guest that posts far more capture room than the buffer it negotiated must
/// be refused, not allowed to make the host reserve it.
#[test]
fn flooding_the_capture_ring_beyond_the_negotiated_buffer_is_an_io_error() {
    let mut harness = Harness::unpaced();
    harness.prepare_capture();

    let mut refused = 0usize;
    for slot in 0..SLOTS {
        harness.queue_capture(slot, PERIOD_BYTES);
        harness.kick(protocol::VQ_RX);
        if rx_status_of(&harness, slot) == protocol::S_IO_ERR {
            refused += 1;
        }
    }
    assert!(
        refused > 0,
        "{SLOTS} capture buffers fitted into a {PERIODS_IN_BUFFER}-period buffer"
    );
    // A refused buffer is never written into.
    assert_eq!(
        harness.read_mem(Harness::rx_data(SLOTS - 1), 8),
        vec![0xcd; 8]
    );
    assert_eq!(
        harness.pcm_command(protocol::R_PCM_RELEASE, stream::INPUT_STREAM),
        protocol::S_OK
    );
}

/// A RELEASE has to hand every posted capture buffer back — and each one must
/// come back carrying **no audio**, because its bytes never existed. Telling
/// the guest a buffer is full when it is not is the capture-side version of
/// retiring a playback message before it has played.
#[test]
fn releasing_a_capture_stream_returns_its_buffers_carrying_no_audio() {
    let mut harness = Harness::unpaced();
    harness.prepare_capture();

    let before = harness.used_idx(3);
    for slot in 0..PERIODS_IN_BUFFER {
        harness.queue_capture(slot, PERIOD_BYTES);
    }
    harness.kick(protocol::VQ_RX);
    assert_eq!(
        harness.used_idx(3),
        before,
        "a prepared capture stream records nothing"
    );

    assert_eq!(
        harness.pcm_command(protocol::R_PCM_RELEASE, stream::INPUT_STREAM),
        protocol::S_OK
    );
    assert_eq!(
        harness.used_idx(3),
        before.wrapping_add(PERIODS_IN_BUFFER as u16),
        "RELEASE must return every pending capture buffer"
    );
    for slot in 0..PERIODS_IN_BUFFER {
        assert_eq!(rx_status_of(&harness, slot), protocol::S_OK);
        assert_eq!(
            harness.used_elem(3, before.wrapping_add(slot as u16)).1,
            protocol::PCM_STATUS_LEN as u32,
            "a drained capture buffer carries no audio"
        );
        assert_eq!(
            harness.read_mem(Harness::rx_data(slot), 8),
            vec![0xcd; 8],
            "a drained capture buffer must not have been written into"
        );
    }
}

/// The event queue carries device-to-driver messages, and we advertise no
/// feature that produces one. Kicking it must be a no-op, not an error and not
/// a consumed buffer.
#[test]
fn the_event_queue_is_accepted_and_left_alone() {
    let mut harness = Harness::unpaced();
    harness.chain(
        1,
        0,
        &[(SCRATCH, protocol::EVENT_LEN as u32, VIRTQ_DESC_F_WRITE)],
    );
    harness.kick(protocol::VQ_EVENT);
    assert_eq!(
        harness.used_idx(1),
        0,
        "the device must not consume event buffers it has nothing to say into"
    );
}
