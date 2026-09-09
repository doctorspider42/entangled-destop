//! The virtio-snd device (backlog GAME-2102).
//!
//! # Shape
//!
//! Four queues, as the spec mandates: control, event, TX (playback), RX
//! (capture). The control queue carries information queries and the PCM
//! lifecycle; TX carries audio out of the guest and RX carries audio into it.
//! The event queue is drained of nothing — we advertise none of the features
//! that produce events.
//!
//! ```text
//!   vCPU / queue-worker thread              playback pump
//!   ──────────────────────────              ─────────────
//!   notify(CONTROL) ─ lifecycle ──┐      ┌─ take a period from the out ring
//!   notify(TX) ─ stage a period ──┤      │  write it to the AudioSink
//!                                 ├ Inner┤  advance `consumed`
//!                                 │(Mutex)  retire the messages it covered
//!                                 │      │
//!                                 │      │  capture pump
//!                                 │      │  ────────────
//!   notify(RX) ─ stage a buffer ──┘      └─ read a period from the AudioSource
//!                                           push it into the in ring
//!                                           fill and retire the buffers it covers
//! ```
//!
//! # Why the pumps own the completion
//!
//! virtio-snd is paced by *completions*: the driver puts one message per
//! period on a queue and treats the used ring as the hardware pointer.
//! Completing a message as soon as its bytes are copied would tell the guest
//! that a whole period moved in a microsecond, and ALSA would spin. So a
//! playback message is only retired once the host sink has actually consumed
//! its audio, and a **capture buffer only once the audio to fill it exists** —
//! which happens on a host thread, on the host clock, long after the vCPU that
//! kicked the queue went back to the guest.
//!
//! Those threads own the TX and RX queues (an `Arc<Mutex<…>>` shared with the
//! device, exactly as virtio-net's receive worker owns its RX queue) and take
//! the pause gate before they touch guest memory (ADR-0005). No [`HostWaker`]
//! is needed: unlike the GPU's fences, the completion is not something we have
//! to be *told* about from a foreign callback, it is something these threads
//! measured themselves. They are two threads and not one because each blocks
//! for about a period inside its endpoint, and a single thread would make each
//! direction wait on the other's hardware.
//!
//! [`HostWaker`]: virtio_core::HostWaker
//!
//! # Untrusted guest, and the ownership inversion on RX
//!
//! Every message length is checked before use; the chain walk is
//! `virtio_core::chain`'s bounded one; identifiers are checked against what
//! the config space advertises *and* against the direction of the queue they
//! arrived on; formats, rates, channel counts and buffer geometry are checked
//! against [`crate::stream`]'s constants rather than against anything the
//! guest asserts; and the host-memory bounds — each stream's PCM ring and its
//! pending-completion list — are capped by what the *validated* parameters
//! allow. A malformed request is answered with a virtio-snd status code;
//! nothing on this path panics or unwraps.
//!
//! Capture inverts what the TX path established, and that inversion is where
//! the interesting bug would live. On TX the guest hands over bytes and the
//! device reads them; on RX the guest hands over *room* and the device
//! **writes** into guest memory. Two rules make that safe, and they are worth
//! stating because everything else on the path follows from them:
//!
//! 1. **The room is measured once, from the guest's own device-writable
//!    descriptors, and never recomputed.** [`CaptureBuffer::room`] is the sum
//!    of the writable segment lengths minus the status word, validated as a
//!    whole number of frames no larger than one negotiated period, and
//!    recorded. Every later write is bounded by *that* number and, segment by
//!    segment, by the length the chain walk reported — so the device can never
//!    write more bytes than the guest made writable, whatever the ring holds.
//! 2. **A buffer is only retired when it has actually been filled.** The
//!    capture pump advances `consumed` by the bytes the source *produced*, and
//!    a buffer is handed back only once `consumed` has passed the point at
//!    which its own bytes exist. A buffer retired early is a guest reading
//!    stale memory and a clock that runs away — the capture-side twin of the
//!    "retire on copy" mistake the playback path exists to avoid.
//!
//! # Suspend/restore seam
//!
//! Everything the guest programmed lives in one place, [`Stream`], plus the
//! per-queue state the transport owns. [`SoundDevice::save_device`] serialises
//! [`Stream::state`] and [`Stream::params`] for both streams; the rings, the
//! host endpoints and the pump threads are host artefacts and are rebuilt, not
//! restored. Messages the device is still holding are handed back to the guest
//! by rewinding the queue position it reports — see
//! [`SoundDevice::queue_positions`]. [`SoundDevice::reset`] returns the same
//! set to power-on.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use virtio_core::chain::{self, Segment};
use virtio_core::device::{DeviceError, DeviceResources, DeviceType, VirtioDevice};
use virtio_core::interrupt::Interrupt;
use virtio_core::quiesce::Quiesce;
use virtio_core::{GuestMem, MAX_QUEUE_SIZE, VIRTIO_F_VERSION_1};
use virtio_queue::{Queue, QueueT};
use vm_memory::{Bytes, GuestAddress};

use crate::backend::{AudioSink, AudioSource, NullSink, SilentSource, StreamFormat};
use crate::protocol::{self, ItemHdr, QueryInfo, RawSetParams};
use crate::stream::{self, ParamError, PcmParams, StreamState};

/// Builds this device's host sink.
///
/// A factory rather than a sink, because the sink is created *and released* on
/// the pump thread: WASAPI's COM objects and ALSA's handle both belong to one
/// thread, and a device reset has to be able to get its audio back rather than
/// silently falling to silence.
pub type SinkFactory = Arc<dyn Fn() -> Box<dyn AudioSink> + Send + Sync>;

/// Builds this device's host capture source, on the capture pump thread and
/// for exactly the same reasons as [`SinkFactory`].
pub type SourceFactory = Arc<dyn Fn() -> Box<dyn AudioSource> + Send + Sync>;

/// Streams the device carries, as an array index.
const NUM_STREAMS: usize = stream::STREAMS as usize;
/// Index of the playback stream in [`Inner::streams`].
const OUT: usize = stream::OUTPUT_STREAM as usize;
/// Index of the capture stream in [`Inner::streams`].
const IN: usize = stream::INPUT_STREAM as usize;

/// Queues the device exposes, all of them at [`MAX_QUEUE_SIZE`].
pub const NUM_QUEUES: usize = protocol::NUM_QUEUES;
static QUEUE_MAX_SIZES: [u16; NUM_QUEUES] = [MAX_QUEUE_SIZE; NUM_QUEUES];

/// Hard bound on chains processed per notification, so a guest refilling the
/// ring from another vCPU cannot pin this thread forever. Same value and same
/// reasoning as virtio-blk and virtio-net.
pub const CHAINS_PER_NOTIFY: usize = 4 * MAX_QUEUE_SIZE as usize;

/// Bytes gathered for one control message. The largest legitimate request is
/// `virtio_snd_pcm_set_params` at 24 bytes; 4 KiB is room for a future one and
/// a hard stop on a guest that chains 128 descriptors of padding.
pub const MAX_CONTROL_MSG_BYTES: usize = 4096;

/// Bytes gathered for one playback message: the 4-byte transfer header plus at
/// most one period.
pub const MAX_XFER_BYTES: usize = protocol::PCM_XFER_LEN + stream::MAX_PERIOD_BYTES as usize;

/// Bytes gathered for one **capture** message's device-readable half.
///
/// A capture message carries no audio *towards* the host: its readable part is
/// the four-byte `virtio_snd_pcm_xfer` and nothing else. The cap is the
/// control-message one rather than [`MAX_XFER_BYTES`] precisely so a guest
/// cannot make the device gather 64 KiB of padding on a queue whose messages
/// are four bytes long.
pub const MAX_CAPTURE_HEADER_BYTES: usize = MAX_CONTROL_MSG_BYTES;

/// I/O messages the device may hold un-retired, per stream. A well-behaved
/// driver holds at most `buffer_bytes / period_bytes` of them; this is the
/// backstop that bounds the bookkeeping independently of the byte caps.
pub const MAX_PENDING_PERIODS: usize = MAX_QUEUE_SIZE as usize;

/// Largest slice handed to the host sink in one call.
///
/// The pump holds the pause gate across a sink write, so this is also the
/// bound on how long a `pause` can wait for the audio device: 8 KiB is ~43 ms
/// of 48 kHz stereo. A larger period is simply written in several calls.
const PUMP_CHUNK_BYTES: usize = 8192;

/// How long the pump sleeps when no stream is running, before re-checking.
const PUMP_IDLE_TICK: Duration = Duration::from_millis(20);

/// Underruns are logged at most this often, with the count since the last one.
const UNDERRUN_LOG_EVERY: Duration = Duration::from_secs(1);

/// Guest-visible `struct virtio_snd_config`: three counts.
const CONFIG_LEN: usize = 12;

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    // A poisoned device mutex means a host thread panicked while holding it.
    // Refusing to serve the guest from then on would turn a host bug into a
    // dead VM; the state is plain data and is re-validated on every use.
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

// ------------------------------------------------------------------- counters

/// Observable device counters. All monotonic; read them for a before/after.
#[derive(Debug, Default)]
pub struct SoundStats {
    periods_played: AtomicU64,
    bytes_played: AtomicU64,
    periods_captured: AtomicU64,
    bytes_captured: AtomicU64,
    underruns: AtomicU64,
    overruns: AtomicU64,
    rejected: AtomicU64,
    sink_failures: AtomicU64,
}

macro_rules! counters {
    ($($field:ident),* $(,)?) => {
        impl SoundStats {
            $(
                pub fn $field(&self) -> u64 {
                    self.$field.load(Ordering::Acquire)
                }
            )*
        }
    };
}

counters!(
    periods_played,
    bytes_played,
    periods_captured,
    bytes_captured,
    underruns,
    overruns,
    rejected,
    sink_failures,
);

impl SoundStats {
    fn bump(counter: &AtomicU64) {
        counter.fetch_add(1, Ordering::AcqRel);
    }

    fn add(counter: &AtomicU64, value: u64) {
        counter.fetch_add(value, Ordering::AcqRel);
    }
}

// -------------------------------------------------------------- shared state

/// Where one capture message's audio goes.
///
/// The **only** description of guest-writable memory the RX path ever has, and
/// it is built once, from the chain walk, before a single byte is produced.
/// `room` is the sum of `segments`' lengths, so a fill can be bounded twice
/// over — by the total and, segment by segment, by the length the walk
/// reported — and neither bound comes from anything the guest asserted after
/// the fact.
#[derive(Debug, Clone)]
struct CaptureBuffer {
    segments: Vec<Segment>,
    room: usize,
}

/// One I/O message the device is holding until its audio has moved.
#[derive(Debug, Clone)]
struct Pending {
    /// Descriptor chain head, for the used ring.
    head: u16,
    /// Where the `virtio_snd_pcm_status` goes. Guest-supplied, so it is only
    /// ever used through a checked `vm-memory` write.
    status_addr: u64,
    /// Value of [`Stream::consumed`] at which this message is complete: for
    /// playback, once its audio has been handed to the sink; for capture, once
    /// the source has produced enough audio to fill it.
    consume_at: u64,
    /// Capture only: the guest memory to fill. `None` on the playback path,
    /// where the guest's buffer is device-*readable* and was consumed at
    /// staging time.
    fill: Option<CaptureBuffer>,
}

/// Everything the guest programmed about one stream, plus the host ring that
/// carries its audio.
///
/// One struct for both directions, because the accounting really is
/// symmetrical — `queued` is what the guest has committed and `consumed` is
/// what the host endpoint has moved, and a message completes when the second
/// passes the first. Only the *sign* of the data flow differs:
///
/// | field | playback | capture |
/// |---|---|---|
/// | `ring` | audio waiting to be played | audio waiting to be handed over |
/// | `queued` | bytes the guest pushed | bytes the guest made room for |
/// | `consumed` | bytes the sink took | bytes the source produced |
/// | `promised` | unused | bytes of room the pending buffers still hold |
///
/// The suspend/restore seam (see the module docs): `state` and `params` are
/// guest state and are saved; `ring`, `queued`, `consumed`, `promised`,
/// `epoch` and `flowing` describe host progress and are rebuilt rather than
/// restored; `pending` holds guest descriptors, which come back by rewinding
/// the queue position.
#[derive(Debug, Default)]
struct Stream {
    state: StreamState,
    params: Option<PcmParams>,
    /// Bumped whenever the host endpoint must be torn down and rebuilt
    /// (PREPARE, STOP, RELEASE, SET_PARAMS). The pump compares it to the epoch
    /// its endpoint was opened for, which is how a period staged before a STOP
    /// can never be accounted against the stream that came after it.
    epoch: u64,
    /// Audio in flight through the host, bounded by the negotiated buffer size.
    ring: VecDeque<u8>,
    pending: VecDeque<Pending>,
    /// Bytes the guest has committed to this stream.
    queued: u64,
    /// Bytes the host endpoint has moved.
    consumed: u64,
    /// Capture only: room the un-retired buffers still hold. This, not
    /// `ring.len()`, is what admission control bounds — a capture buffer
    /// reserves its space the moment it is posted, long before there is any
    /// audio to put in it.
    promised: usize,
    /// Set once real audio has flowed, so the gap between START and the first
    /// period is not counted as an underrun (or an overrun).
    flowing: bool,
}

impl Stream {
    /// Bytes the ring may hold: the buffer the guest negotiated, plus one
    /// period of slack so a driver that runs slightly ahead is not punished.
    fn ring_capacity(&self) -> usize {
        match self.params {
            Some(params) => params.buffer_bytes as usize + params.period_bytes as usize,
            None => 0,
        }
    }

    /// Drops everything the stream was carrying. The pending list is *not*
    /// touched here — its chains must be handed back to the guest first.
    fn clear_audio(&mut self) {
        self.ring.clear();
        self.ring.shrink_to_fit();
        self.queued = 0;
        self.consumed = 0;
        self.promised = 0;
        self.flowing = false;
    }

    /// Bytes committed but not yet moved — the `latency_bytes` a
    /// `virtio_snd_pcm_status` reports.
    fn latency(&self) -> u32 {
        u32::try_from(self.queued.saturating_sub(self.consumed)).unwrap_or(u32::MAX)
    }
}

/// The half of the device the pump threads share with the queue paths.
#[derive(Default)]
struct Inner {
    mem: Option<Arc<GuestMem>>,
    interrupt: Option<Arc<dyn Interrupt>>,
    /// Owned jointly: `notify(TX)` fills it, the playback pump retires from it.
    tx_queue: Option<Queue>,
    /// Likewise for `notify(RX)` and the capture pump.
    rx_queue: Option<Queue>,
    /// Indexed by stream id: [`OUT`] then [`IN`].
    streams: [Stream; NUM_STREAMS],
}

impl std::fmt::Debug for Inner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Inner")
            .field("activated", &self.tx_queue.is_some())
            .field("output", &self.streams[OUT])
            .field("input", &self.streams[IN])
            .finish_non_exhaustive()
    }
}

#[derive(Default)]
struct Shared {
    inner: Mutex<Inner>,
    /// Signalled when the pump has new work: audio queued, a stream started or
    /// stopped, or a shutdown requested.
    signal: Condvar,
    stop: AtomicBool,
    /// Behind an `Arc` so a caller can keep watching the counters after the
    /// device has been moved into its transport — which is the only way a
    /// test, or `doctor`, can see an underrun happen.
    stats: Arc<SoundStats>,
}

impl Shared {
    fn wake(&self) {
        self.signal.notify_all();
    }
}

/// The pump threads' handles, for a clean shutdown.
struct Pump {
    playback: std::thread::JoinHandle<()>,
    capture: std::thread::JoinHandle<()>,
    quiesce: Arc<Quiesce>,
}

// ------------------------------------------------------------------ the device

/// A virtio-snd device with one stereo output stream and one stereo input
/// stream.
pub struct SoundDevice {
    /// Log label — the host sink's name at construction time.
    name: String,
    /// Log label for the capture endpoint, which need not be the same device.
    source_name: String,
    features: u64,
    acked_features: u64,
    shared: Arc<Shared>,
    factory: SinkFactory,
    source_factory: SourceFactory,
    pump: Option<Pump>,

    // Queues only the device touches. Set on activate(), cleared on reset().
    control_queue: Option<Queue>,
    event_queue: Option<Queue>,
    mem: Option<Arc<GuestMem>>,
    interrupt: Option<Arc<dyn Interrupt>>,

    /// Reusable staging buffer for one gathered message. Bounded by
    /// [`MAX_XFER_BYTES`], which is the larger of the two message caps.
    staging: Vec<u8>,
}

impl std::fmt::Debug for SoundDevice {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SoundDevice")
            .field("sink", &self.name)
            .field("source", &self.source_name)
            .field("activated", &self.control_queue.is_some())
            .finish_non_exhaustive()
    }
}

impl SoundDevice {
    /// Builds a device around a host sink factory. `name` labels it in logs
    /// (the factory's own sinks are only built once the pump runs).
    ///
    /// The capture endpoint defaults to [`SilentSource`] — a microphone that
    /// hears nothing, on the host clock. A guest always gets a working
    /// recording device; giving it the host's real one is
    /// [`Self::with_source`].
    pub fn new(name: impl Into<String>, factory: SinkFactory) -> Self {
        Self {
            name: name.into(),
            source_name: "silent".to_owned(),
            features: VIRTIO_F_VERSION_1,
            acked_features: 0,
            shared: Arc::new(Shared::default()),
            factory,
            source_factory: Arc::new(|| Box::new(SilentSource::new()) as Box<dyn AudioSource>),
            pump: None,
            control_queue: None,
            event_queue: None,
            mem: None,
            interrupt: None,
            staging: Vec::new(),
        }
    }

    /// Gives the device a host capture source. `name` labels it in logs.
    pub fn with_source(mut self, name: impl Into<String>, factory: SourceFactory) -> Self {
        self.source_name = name.into();
        self.source_factory = factory;
        self
    }

    /// A device that keeps time but makes no sound and hears none — the
    /// headless default.
    pub fn null() -> Self {
        Self::new(
            "null",
            Arc::new(|| Box::new(NullSink::new()) as Box<dyn AudioSink>),
        )
    }

    /// A device whose audio is captured in host memory, for tests and for
    /// anyone who wants to know what the guest actually played.
    ///
    /// `paced` false makes the capture run as fast as the guest can queue,
    /// which is only ever right for a test.
    pub fn recording(paced: bool) -> (Self, Arc<crate::backend::Recording>) {
        let recording = Arc::new(crate::backend::Recording::default());
        let shared = Arc::clone(&recording);
        let device = Self::new(
            "record",
            Arc::new(move || {
                Box::new(crate::backend::RecordingSink::sharing(
                    Arc::clone(&shared),
                    paced,
                )) as Box<dyn AudioSink>
            }),
        );
        (device, recording)
    }

    /// A device that plays into a tone-producing microphone: whatever the
    /// guest records is the deterministic `hz` sine [`crate::backend::
    /// ToneSource`] generates, so a test can assert the exact samples that
    /// should have arrived without a microphone in the room.
    ///
    /// `paced` false makes the capture run as fast as the guest can post
    /// buffers, which is only ever right for a test.
    pub fn with_tone_source(self, hz: f64, paced: bool) -> (Self, Arc<crate::backend::SourceLog>) {
        let log = Arc::new(crate::backend::SourceLog::default());
        // Each activation builds a fresh source on the pump thread, so the
        // *shared* log is what a caller keeps hold of.
        let shared = Arc::clone(&log);
        let device = self.with_source(
            "tone",
            Arc::new(move || {
                Box::new(crate::backend::ToneSource::sharing(
                    hz,
                    paced,
                    Arc::clone(&shared),
                )) as Box<dyn AudioSource>
            }),
        );
        (device, log)
    }

    /// Name of the host sink, for logs and `doctor`.
    pub fn sink_name(&self) -> &str {
        &self.name
    }

    /// Name of the host capture source, for logs and `doctor`.
    pub fn source_name(&self) -> &str {
        &self.source_name
    }

    pub fn stats(&self) -> &SoundStats {
        &self.shared.stats
    }

    /// A handle on the counters that outlives the move into a transport.
    pub fn stats_handle(&self) -> Arc<SoundStats> {
        Arc::clone(&self.shared.stats)
    }

    /// The guest-visible `struct virtio_snd_config`.
    fn config_space(&self) -> [u8; CONFIG_LEN] {
        let mut raw = [0u8; CONFIG_LEN];
        raw[0..4].copy_from_slice(&stream::JACKS.to_le_bytes());
        raw[4..8].copy_from_slice(&stream::STREAMS.to_le_bytes());
        raw[8..12].copy_from_slice(&stream::CHMAPS.to_le_bytes());
        raw
    }

    // ------------------------------------------------------------ control queue

    fn drain_control(&mut self) -> Result<(), DeviceError> {
        let mem = self.mem.clone().ok_or(DeviceError::NotActivated)?;
        let interrupt = self.interrupt.clone().ok_or(DeviceError::NotActivated)?;
        let mut queue = self.control_queue.take().ok_or(DeviceError::NotActivated)?;
        let result = self.control_loop(&mut queue, &mem, interrupt.as_ref());
        self.control_queue = Some(queue);
        result
    }

    fn control_loop(
        &mut self,
        queue: &mut Queue,
        mem: &Arc<GuestMem>,
        interrupt: &dyn Interrupt,
    ) -> Result<(), DeviceError> {
        let desc_table = queue.desc_table();
        let queue_size = queue.size();
        let mut served = 0usize;

        while let Some(head) = queue
            .pop_descriptor_chain(Arc::clone(mem))
            .map(|chain| chain.head_index())
        {
            let written = self.handle_control(mem, desc_table, queue_size, head);
            queue
                .add_used(mem.as_ref(), head, written)
                .map_err(|e| DeviceError::Queue(e.to_string()))?;
            served += 1;
            if served >= CHAINS_PER_NOTIFY {
                tracing::warn!(
                    sink = %self.name,
                    served,
                    "virtio-snd control budget exhausted; deferring the rest"
                );
                break;
            }
        }

        if served > 0
            && queue
                .needs_notification(mem.as_ref())
                .map_err(|e| DeviceError::Queue(e.to_string()))?
        {
            interrupt.signal_used_queue(protocol::VQ_CONTROL)?;
        }
        Ok(())
    }

    /// Handles one control chain. Returns the bytes written into
    /// device-writable buffers (what goes into the used ring).
    fn handle_control(
        &mut self,
        mem: &GuestMem,
        desc_table: u64,
        queue_size: u16,
        head: u16,
    ) -> u32 {
        let Some((readable, writable)) = split_chain(mem, desc_table, queue_size, head, &self.name)
        else {
            return 0;
        };
        // Without somewhere to put a status there is no way to answer at all.
        if total_len(&writable) < protocol::HDR_LEN as u64 {
            tracing::warn!(
                sink = %self.name,
                head,
                "virtio-snd control chain has no device-writable status word"
            );
            return 0;
        }

        let request = match gather(mem, &readable, MAX_CONTROL_MSG_BYTES, &mut self.staging) {
            Ok(len) => len,
            Err(reason) => {
                tracing::warn!(sink = %self.name, head, reason, "refusing control message");
                return write_response(mem, &writable, protocol::S_BAD_MSG, &[]);
            }
        };
        // The staging buffer is reused, so only the prefix just gathered is
        // this message; never look past it. Copied out because the handlers
        // below need `&mut self` and a control message is at most
        // `MAX_CONTROL_MSG_BYTES` — this is a few dozen bytes a handful of
        // times per stream, not a hot path.
        let message: Vec<u8> = self.staging.get(..request).unwrap_or(&[]).to_vec();
        let message = message.as_slice();
        if message.len() < protocol::HDR_LEN {
            SoundStats::bump(&self.shared.stats.rejected);
            return write_response(mem, &writable, protocol::S_BAD_MSG, &[]);
        }

        let code = protocol::request_code(message);
        let mut payload: Vec<u8> = Vec::new();
        let status = match code {
            protocol::R_JACK_INFO | protocol::R_PCM_INFO | protocol::R_CHMAP_INFO => {
                self.query_info(code, message, &mut payload)
            }
            protocol::R_JACK_REMAP => {
                // We publish no jack with VIRTIO_SND_JACK_F_REMAP, so this is
                // well formed but not something we do.
                tracing::debug!(sink = %self.name, "refusing JACK_REMAP: no remappable jack");
                protocol::S_NOT_SUPP
            }
            protocol::R_PCM_SET_PARAMS => self.pcm_set_params(message),
            protocol::R_PCM_PREPARE
            | protocol::R_PCM_RELEASE
            | protocol::R_PCM_START
            | protocol::R_PCM_STOP => self.pcm_lifecycle(code, message),
            other => {
                tracing::debug!(sink = %self.name, code = other, "unknown virtio-snd request");
                protocol::S_NOT_SUPP
            }
        };
        if status != protocol::S_OK {
            SoundStats::bump(&self.shared.stats.rejected);
        }
        write_response(mem, &writable, status, &payload)
    }

    /// `*_INFO`: hand back `count` fixed-size records starting at `start_id`.
    fn query_info(&self, code: u32, message: &[u8], payload: &mut Vec<u8>) -> u32 {
        let Some(raw) = take_array::<{ protocol::QUERY_INFO_LEN }>(message) else {
            return protocol::S_BAD_MSG;
        };
        let query = QueryInfo::parse(&raw);
        let (available, item_len) = match code {
            protocol::R_JACK_INFO => (stream::JACKS, protocol::JACK_INFO_LEN),
            protocol::R_PCM_INFO => (stream::STREAMS, protocol::PCM_INFO_LEN),
            _ => (stream::CHMAPS, protocol::CHMAP_INFO_LEN),
        };
        // The driver tells us how big it believes one record is. Writing our
        // struct into a buffer sized for a different one is exactly the bug
        // this field exists to prevent, so a mismatch is refused, never
        // truncated or padded.
        if query.size != item_len as u32 {
            tracing::warn!(
                sink = %self.name,
                request = stream::command_name(code),
                driver_size = query.size,
                device_size = item_len,
                "refusing an info query whose record size disagrees with ours"
            );
            return protocol::S_BAD_MSG;
        }
        let Some(end) = query.start_id.checked_add(query.count) else {
            return protocol::S_BAD_MSG;
        };
        if end > available {
            tracing::warn!(
                sink = %self.name,
                request = stream::command_name(code),
                start_id = query.start_id,
                count = query.count,
                available,
                "refusing an info query outside the advertised range"
            );
            return protocol::S_BAD_MSG;
        }
        // `count` is bounded by `available`, a device constant, so the
        // allocation below is bounded by a constant too.
        payload.reserve(query.count as usize * item_len);
        // Records vary by identifier now that there are two of each: the array
        // is built per id, in order, rather than by repeating one record.
        for id in query.start_id..end {
            match code {
                protocol::R_JACK_INFO => payload.extend_from_slice(&jack_info(id).encode()),
                protocol::R_PCM_INFO => payload.extend_from_slice(&pcm_info(id).encode()),
                _ => payload.extend_from_slice(&chmap_info(id).encode()),
            }
        }
        protocol::S_OK
    }

    fn pcm_set_params(&mut self, message: &[u8]) -> u32 {
        let Some(raw) = take_array::<{ protocol::SET_PARAMS_LEN }>(message) else {
            return protocol::S_BAD_MSG;
        };
        let request = RawSetParams::parse(&raw);
        let params = match stream::validate_params(&request) {
            Ok(params) => params,
            Err(error) => {
                tracing::warn!(sink = %self.name, %error, "refusing SET_PARAMS");
                return error.status();
            }
        };
        // `validate_params` already refused an unadvertised id, so this cannot
        // be out of range — but the index is derived rather than assumed.
        let Some(index) = stream_index(request.stream_id) else {
            return protocol::S_BAD_MSG;
        };

        let mut inner = lock(&self.shared.inner);
        if let Err(error) =
            stream::transition(inner.streams[index].state, protocol::R_PCM_SET_PARAMS)
        {
            tracing::warn!(sink = %self.name, %error, "refusing SET_PARAMS");
            return error.status();
        }
        // Reconfiguring invalidates whatever was in flight, so the guest gets
        // its chains back before anything else changes.
        let retired = retire_all(&mut inner, index);
        let stream = &mut inner.streams[index];
        stream.clear_audio();
        stream.params = Some(params);
        stream.state = StreamState::ParamsSet;
        stream.epoch = stream.epoch.wrapping_add(1);
        let interrupt = inner.interrupt.as_ref().map(Arc::clone);
        drop(inner);
        self.shared.wake();
        signal_stream(retired, index, interrupt.as_deref(), &self.name);
        tracing::info!(
            sink = %self.name,
            stream = request.stream_id,
            direction = if index == IN { "capture" } else { "playback" },
            rate_hz = params.rate_hz,
            channels = params.channels,
            period_bytes = params.period_bytes,
            buffer_bytes = params.buffer_bytes,
            "virtio-snd stream configured"
        );
        protocol::S_OK
    }

    fn pcm_lifecycle(&mut self, code: u32, message: &[u8]) -> u32 {
        let Some(raw) = take_array::<{ protocol::PCM_HDR_LEN }>(message) else {
            return protocol::S_BAD_MSG;
        };
        let hdr = ItemHdr::parse(&raw);
        let Some(index) = stream_index(hdr.id) else {
            let error = ParamError::UnknownStream { id: hdr.id };
            tracing::warn!(
                sink = %self.name,
                command = stream::command_name(code),
                %error,
                "refusing a PCM command"
            );
            return error.status();
        };

        let mut inner = lock(&self.shared.inner);
        let next = match stream::transition(inner.streams[index].state, code) {
            Ok(next) => next,
            Err(error) => {
                tracing::warn!(sink = %self.name, %error, "refusing a PCM command");
                return error.status();
            }
        };
        // PREPARE with no parameters cannot happen (the state machine only
        // allows it from ParamsSet/Prepared, both of which imply params), but
        // the invariant is checked rather than assumed.
        if next != StreamState::Unset && inner.streams[index].params.is_none() {
            return protocol::S_BAD_MSG;
        }

        let mut retired = false;
        match code {
            protocol::R_PCM_PREPARE => {
                retired = retire_all(&mut inner, index);
                let stream = &mut inner.streams[index];
                stream.clear_audio();
                stream.epoch = stream.epoch.wrapping_add(1);
            }
            protocol::R_PCM_START => {
                inner.streams[index].flowing = false;
            }
            protocol::R_PCM_STOP | protocol::R_PCM_RELEASE => {
                // "The device MUST complete all pending I/O messages for the
                // specified stream" — spec 5.14.6.6.5/6.
                retired = retire_all(&mut inner, index);
                let stream = &mut inner.streams[index];
                stream.clear_audio();
                stream.epoch = stream.epoch.wrapping_add(1);
                if code == protocol::R_PCM_RELEASE {
                    stream.params = None;
                }
            }
            _ => {}
        }
        inner.streams[index].state = next;
        let interrupt = inner.interrupt.as_ref().map(Arc::clone);
        drop(inner);
        self.shared.wake();
        // Chains handed back above need an interrupt of their own: they went
        // into that stream's own used ring, not this one.
        signal_stream(retired, index, interrupt.as_deref(), &self.name);
        tracing::debug!(
            sink = %self.name,
            command = stream::command_name(code),
            stream = hdr.id,
            state = ?next,
            "virtio-snd stream lifecycle"
        );
        protocol::S_OK
    }

    // ----------------------------------------------------------------- TX queue

    fn drain_tx(&mut self) -> Result<(), DeviceError> {
        self.drain_io(OUT)
    }

    /// The capture queue: the guest posts *empty* buffers here and the capture
    /// pump fills them. Staging validates the room and records it; not one
    /// byte is written until the audio to fill it exists.
    fn drain_rx(&mut self) -> Result<(), DeviceError> {
        self.drain_io(IN)
    }

    /// Drains one of the two audio queues. Identical bookkeeping in both
    /// directions — the difference is entirely inside [`stage_xfer`], which is
    /// where "the guest gave us bytes" and "the guest gave us room" part ways.
    fn drain_io(&mut self, index: usize) -> Result<(), DeviceError> {
        let queue_index = queue_of(index);
        let mut inner = lock(&self.shared.inner);
        let Inner {
            mem,
            interrupt,
            tx_queue,
            rx_queue,
            streams,
        } = &mut *inner;
        let queue = if index == IN { rx_queue } else { tx_queue };
        let (Some(mem), Some(queue), Some(stream)) =
            (mem.as_ref(), queue.as_mut(), streams.get_mut(index))
        else {
            return Err(DeviceError::NotActivated);
        };
        let desc_table = queue.desc_table();
        let queue_size = queue.size();
        let mut served = 0usize;
        let mut retired = 0usize;

        while let Some(head) = queue
            .pop_descriptor_chain(Arc::clone(mem))
            .map(|chain| chain.head_index())
        {
            let outcome = stage_xfer(
                index,
                mem,
                desc_table,
                queue_size,
                head,
                &mut self.staging,
                stream,
                &self.shared.stats,
                &self.name,
            );
            match outcome {
                Xfer::Queued => {}
                Xfer::Answer {
                    status_addr,
                    status,
                } => {
                    write_pcm_status(mem, status_addr, status, stream.latency());
                    // A refused capture message writes its status and no
                    // audio, so the used length is the status word alone —
                    // never the room the guest offered.
                    queue
                        .add_used(mem.as_ref(), head, protocol::PCM_STATUS_LEN as u32)
                        .map_err(|e| DeviceError::Queue(e.to_string()))?;
                    retired += 1;
                }
                Xfer::Drop => {
                    queue
                        .add_used(mem.as_ref(), head, 0)
                        .map_err(|e| DeviceError::Queue(e.to_string()))?;
                    retired += 1;
                }
            }
            served += 1;
            if served >= CHAINS_PER_NOTIFY {
                tracing::warn!(
                    sink = %self.name,
                    served,
                    queue = queue_index,
                    "virtio-snd I/O budget exhausted; deferring the rest"
                );
                break;
            }
        }

        let signal = retired > 0
            && queue
                .needs_notification(mem.as_ref())
                .map_err(|e| DeviceError::Queue(e.to_string()))?;
        let interrupt = interrupt.as_ref().map(Arc::clone);
        drop(inner);

        if served > 0 {
            self.shared.wake();
        }
        if signal {
            if let Some(interrupt) = interrupt {
                interrupt.signal_used_queue(queue_index)?;
            }
        }
        Ok(())
    }

    // -------------------------------------------------------------- pump thread

    /// Stops and joins both pumps. Idempotent and infallible, so `reset` and
    /// `Drop` can both call it.
    fn stop_pump(&mut self) {
        let Some(pump) = self.pump.take() else {
            return;
        };
        self.shared.stop.store(true, Ordering::Release);
        // Two places each can be waiting: our condvar and the pause gate (a
        // device reset runs on a quiesced VM — ADR-0005). Wake both, or the
        // joins below are a deadlock.
        self.shared.wake();
        pump.quiesce.wake();
        for (what, thread) in [("playback", pump.playback), ("capture", pump.capture)] {
            match thread.join() {
                Ok(()) => tracing::debug!(sink = %self.name, what, "virtio-snd pump stopped"),
                Err(_) => tracing::error!(sink = %self.name, what, "virtio-snd pump panicked"),
            }
        }
        self.shared.stop.store(false, Ordering::Release);
    }
}

impl Drop for SoundDevice {
    fn drop(&mut self) {
        // Guarantees "closing the VM leaves no worker threads behind" even if
        // the driver never reset the device.
        self.stop_pump();
    }
}

// -------------------------------------------------------------- chain helpers

/// Walks a chain and splits it, logging and dropping anything malformed.
fn split_chain(
    mem: &GuestMem,
    desc_table: u64,
    queue_size: u16,
    head: u16,
    name: &str,
) -> Option<(Vec<Segment>, Vec<Segment>)> {
    let segments = match chain::walk(mem, desc_table, queue_size, head) {
        Ok(segments) => segments,
        Err(error) => {
            tracing::warn!(sink = %name, head, %error, "dropping malformed virtio-snd chain");
            return None;
        }
    };
    match chain::split_rw(&segments) {
        Ok((readable, writable)) => Some((readable.to_vec(), writable.to_vec())),
        Err(error) => {
            tracing::warn!(sink = %name, head, %error, "dropping virtio-snd chain");
            None
        }
    }
}

fn total_len(segments: &[Segment]) -> u64 {
    segments.iter().map(|s| u64::from(s.len)).sum()
}

/// Copies every device-readable byte of a chain into `staging`, refusing
/// anything past `cap` *before* reading it.
fn gather(
    mem: &GuestMem,
    readable: &[Segment],
    cap: usize,
    staging: &mut Vec<u8>,
) -> Result<usize, &'static str> {
    let total = total_len(readable);
    if total > cap as u64 {
        return Err("message exceeds the device's message cap");
    }
    let total = usize::try_from(total).map_err(|_| "message length is not representable")?;
    if staging.len() < total {
        staging.resize(total, 0);
    }
    let mut at = 0usize;
    for segment in readable {
        let len = segment.len as usize;
        if len == 0 {
            continue;
        }
        let Some(slot) = staging.get_mut(at..at + len) else {
            // Unreachable: `total` is the sum of these lengths. Refused rather
            // than indexed, because "unreachable" is not a proof.
            return Err("staging buffer is short");
        };
        mem.read_slice(slot, GuestAddress(segment.addr))
            .map_err(|_| "message is not readable guest memory")?;
        at += len;
    }
    Ok(total)
}

/// Copies the first `N` bytes of a gathered message into a fixed array, or
/// `None` when the guest sent a shorter message than the request needs.
fn take_array<const N: usize>(message: &[u8]) -> Option<[u8; N]> {
    let mut raw = [0u8; N];
    raw.copy_from_slice(message.get(..N)?);
    Some(raw)
}

/// Writes `status` (and optionally an info array) across the device-writable
/// segments. Returns the bytes written, for the used ring.
///
/// A response that does not fit is downgraded to a bare `BAD_MSG` rather than
/// truncated: half a record array is worse for the driver than an error.
fn write_response(mem: &GuestMem, writable: &[Segment], status: u32, payload: &[u8]) -> u32 {
    let needed = protocol::HDR_LEN as u64 + payload.len() as u64;
    if total_len(writable) < needed {
        let bare = protocol::encode_status(protocol::S_BAD_MSG);
        return scatter(mem, writable, &bare);
    }
    let mut buf = Vec::with_capacity(needed as usize);
    buf.extend_from_slice(&protocol::encode_status(status));
    buf.extend_from_slice(payload);
    scatter(mem, writable, &buf)
}

/// Writes `data` across `segments` in order, stopping at the first failure.
fn scatter(mem: &GuestMem, segments: &[Segment], data: &[u8]) -> u32 {
    let mut written = 0usize;
    for segment in segments {
        if written >= data.len() {
            break;
        }
        let take = (segment.len as usize).min(data.len() - written);
        if take == 0 {
            continue;
        }
        let Some(slice) = data.get(written..written + take) else {
            break;
        };
        if mem.write_slice(slice, GuestAddress(segment.addr)).is_err() {
            tracing::warn!(
                addr = format_args!("{:#x}", segment.addr),
                len = take,
                "virtio-snd response buffer is not writable guest memory"
            );
            break;
        }
        written += take;
    }
    u32::try_from(written).unwrap_or(u32::MAX)
}

fn write_pcm_status(mem: &GuestMem, addr: u64, status: u32, latency_bytes: u32) {
    let raw = protocol::encode_pcm_status(status, latency_bytes);
    if mem.write_slice(&raw, GuestAddress(addr)).is_err() {
        tracing::warn!(
            addr = format_args!("{addr:#x}"),
            "virtio-snd I/O status buffer is not writable guest memory"
        );
    }
}

/// The virtqueue a stream's I/O messages travel on.
fn queue_of(index: usize) -> u16 {
    if index == IN {
        protocol::VQ_RX
    } else {
        protocol::VQ_TX
    }
}

/// A stream identifier as an index into [`Inner::streams`], or `None` when the
/// guest named one the config space never advertised.
fn stream_index(stream_id: u32) -> Option<usize> {
    usize::try_from(stream_id)
        .ok()
        .filter(|index| *index < NUM_STREAMS)
}

/// Raises a stream's used-buffer interrupt after chains were handed back
/// outside the ordinary drain path (a STOP or RELEASE retiring what was in
/// flight).
fn signal_stream(retired: bool, index: usize, interrupt: Option<&dyn Interrupt>, name: &str) {
    if !retired {
        return;
    }
    let Some(interrupt) = interrupt else {
        return;
    };
    let queue = queue_of(index);
    if let Err(error) = interrupt.signal_used_queue(queue) {
        tracing::warn!(sink = %name, queue, %error, "cannot signal a virtio-snd queue");
    }
}

/// Whether the driver asked to be told about the used buffers just added to
/// one stream's queue.
fn needs_notification(inner: &mut Inner, index: usize) -> bool {
    let Inner {
        mem,
        tx_queue,
        rx_queue,
        ..
    } = inner;
    let queue = if index == IN { rx_queue } else { tx_queue };
    match (queue.as_mut(), mem.as_ref()) {
        (Some(queue), Some(mem)) => queue.needs_notification(mem.as_ref()).unwrap_or(false),
        _ => false,
    }
}

// -------------------------------------------------------------- the TX staging

enum Xfer {
    /// Accepted; the pump will retire it once the audio has played.
    Queued,
    /// Answer now with this status.
    Answer { status_addr: u64, status: u32 },
    /// The chain is too malformed to answer at all.
    Drop,
}

/// Validates and stages one I/O message, in either direction.
///
/// The two directions share every check; what differs is *which* half of the
/// chain carries the audio, and therefore which length is validated:
///
/// | | playback (TX) | capture (RX) |
/// |---|---|---|
/// | device-readable | header + one period of audio | the header, and nothing else |
/// | device-writable | the status word | the audio buffer, then the status word |
/// | gather cap | [`MAX_XFER_BYTES`] | [`MAX_CAPTURE_HEADER_BYTES`] |
/// | length checked | bytes the guest wrote | **room the guest made writable** |
/// | staged | the bytes, copied into the ring | the segments, to fill later |
#[allow(clippy::too_many_arguments)]
fn stage_xfer(
    index: usize,
    mem: &GuestMem,
    desc_table: u64,
    queue_size: u16,
    head: u16,
    staging: &mut Vec<u8>,
    stream: &mut Stream,
    stats: &SoundStats,
    name: &str,
) -> Xfer {
    let capture = index == IN;
    let Some((readable, writable)) = split_chain(mem, desc_table, queue_size, head, name) else {
        return Xfer::Drop;
    };
    // The status is the final device-writable buffer (spec 5.14.6.8). Without
    // it there is nowhere to report anything, so the chain is unusable.
    let Some(status_seg) = writable.last() else {
        tracing::warn!(sink = %name, head, "virtio-snd I/O message has no status buffer");
        return Xfer::Drop;
    };
    if (status_seg.len as usize) < protocol::PCM_STATUS_LEN {
        tracing::warn!(
            sink = %name,
            head,
            len = status_seg.len,
            "virtio-snd I/O status buffer is too small"
        );
        return Xfer::Drop;
    }
    let status_addr = status_seg.addr;
    let answer = |status: u32| Xfer::Answer {
        status_addr,
        status,
    };

    let cap = if capture {
        MAX_CAPTURE_HEADER_BYTES
    } else {
        MAX_XFER_BYTES
    };
    let gathered = match gather(mem, &readable, cap, staging) {
        Ok(len) => len,
        Err(reason) => {
            tracing::warn!(sink = %name, head, reason, "refusing virtio-snd I/O message");
            SoundStats::bump(&stats.rejected);
            return answer(protocol::S_BAD_MSG);
        }
    };
    let message: &[u8] = staging.get(..gathered).unwrap_or(&[]);
    let Some(raw) = take_array::<{ protocol::PCM_XFER_LEN }>(message) else {
        SoundStats::bump(&stats.rejected);
        return answer(protocol::S_BAD_MSG);
    };
    let stream_id = u32::from_le_bytes(raw);

    // The one number the whole RX path is built on: the room the guest made
    // *writable*, everything in the chain but the trailing status word. It is
    // computed here, once, from the bounded chain walk — and every later write
    // is bounded by it.
    let fill_segments: Vec<Segment> = if capture {
        writable
            .get(..writable.len().saturating_sub(1))
            .unwrap_or(&[])
            .to_vec()
    } else {
        Vec::new()
    };
    let granted = if capture {
        match usize::try_from(total_len(&fill_segments)) {
            Ok(room) => room,
            Err(_) => {
                SoundStats::bump(&stats.rejected);
                return answer(protocol::S_BAD_MSG);
            }
        }
    } else {
        message.len().saturating_sub(protocol::PCM_XFER_LEN)
    };

    let queue_direction = if capture {
        protocol::D_INPUT
    } else {
        protocol::D_OUTPUT
    };
    if let Err(error) = stream::validate_xfer(
        queue_direction,
        stream.state,
        stream.params,
        stream_id,
        granted,
    ) {
        tracing::warn!(sink = %name, %error, "refusing a virtio-snd I/O message");
        SoundStats::bump(&stats.rejected);
        return answer(error.status());
    }

    // Admission control. Playback bounds the bytes already in the ring;
    // capture bounds the room the un-retired buffers *reserve*, because a
    // capture buffer holds its space from the moment it is posted — long
    // before there is any audio to put in it.
    let outstanding = if capture {
        stream.promised
    } else {
        stream.ring.len()
    };
    if stream.pending.len() >= MAX_PENDING_PERIODS
        || outstanding.saturating_add(granted) > stream.ring_capacity()
    {
        // The guest is running further ahead than the buffer it negotiated.
        // Refusing this message keeps the host allocation bounded and tells
        // the driver exactly what happened.
        SoundStats::bump(&stats.overruns);
        tracing::debug!(
            sink = %name,
            outstanding,
            capacity = stream.ring_capacity(),
            pending = stream.pending.len(),
            capture,
            "virtio-snd ring is full"
        );
        return answer(protocol::S_IO_ERR);
    }

    let fill = if capture {
        stream.promised = stream.promised.saturating_add(granted);
        Some(CaptureBuffer {
            segments: fill_segments,
            room: granted,
        })
    } else {
        let payload = message.get(protocol::PCM_XFER_LEN..).unwrap_or(&[]);
        stream.ring.extend(payload.iter().copied());
        None
    };
    stream.queued = stream.queued.saturating_add(granted as u64);
    stream.pending.push_back(Pending {
        head,
        status_addr,
        consume_at: stream.queued,
        fill,
    });
    Xfer::Queued
}

// ---------------------------------------------------------------- completion

/// Retires every pending message whose audio has moved. Returns true when at
/// least one went back to the guest.
///
/// For capture this is where guest memory is finally written, and it is
/// deliberately the *only* place: `consume_at` has been reached, so the bytes
/// this buffer promised exist in the ring, and the buffer is filled with
/// exactly the room it was admitted with. A buffer whose audio does not exist
/// yet is not touched at all.
fn retire_consumed(inner: &mut Inner, index: usize) -> bool {
    let consumed = inner
        .streams
        .get(index)
        .map(|stream| stream.consumed)
        .unwrap_or(0);
    retire_while(inner, index, true, |p| p.consume_at <= consumed)
}

/// Hands every pending message back, whatever the pump has moved. Used by
/// STOP / RELEASE / SET_PARAMS, which the spec requires to drain the queue.
///
/// A capture buffer retired this way carries **no audio**: its bytes never
/// existed, so it comes back with a zero-length payload rather than with
/// whatever the ring happens to hold. Handing a guest a partially-filled
/// buffer while telling it the buffer is full is precisely the bug the
/// "retire when the audio exists" rule is there to prevent.
fn retire_all(inner: &mut Inner, index: usize) -> bool {
    retire_while(inner, index, false, |_| true)
}

/// `fill` false retires without writing audio (a STOP draining the queue).
fn retire_while(
    inner: &mut Inner,
    index: usize,
    fill: bool,
    ready: impl Fn(&Pending) -> bool,
) -> bool {
    let Inner {
        mem,
        tx_queue,
        rx_queue,
        streams,
        ..
    } = inner;
    let queue = if index == IN { rx_queue } else { tx_queue };
    let (Some(mem), Some(queue), Some(stream)) =
        (mem.as_ref(), queue.as_mut(), streams.get_mut(index))
    else {
        // Not activated (or already reset): the chains went with the queue.
        if let Some(stream) = streams.get_mut(index) {
            stream.pending.clear();
            stream.promised = 0;
        }
        return false;
    };
    let latency = stream.latency();
    let mut any = false;
    while stream.pending.front().is_some_and(&ready) {
        let Some(pending) = stream.pending.pop_front() else {
            break;
        };
        let mut written = 0usize;
        if let Some(buffer) = pending.fill.as_ref() {
            stream.promised = stream.promised.saturating_sub(buffer.room);
            if fill {
                written = fill_capture_buffer(mem, stream, buffer);
            }
        }
        write_pcm_status(mem, pending.status_addr, protocol::S_OK, latency);
        let used = u32::try_from(written.saturating_add(protocol::PCM_STATUS_LEN))
            .unwrap_or(protocol::PCM_STATUS_LEN as u32);
        // Even a failed status write must return the chain, or the descriptor
        // leaks and the guest's ring drains to a halt.
        if let Err(error) = queue.add_used(mem.as_ref(), pending.head, used) {
            tracing::warn!(%error, "cannot retire a virtio-snd I/O message");
            break;
        }
        any = true;
    }
    any
}

/// Copies `buffer.room` bytes out of the capture ring into the guest memory
/// the driver made writable, and returns how many actually landed.
///
/// Three bounds, each independent of the others, because this is the one place
/// the device writes guest memory on the guest's own say-so:
///
/// 1. never more than `buffer.room`, the total measured at staging time;
/// 2. never more than the ring holds — which, having reached `consume_at`,
///    is at least `room`, but is re-checked rather than assumed;
/// 3. never more into one segment than the length the chain walk reported for
///    it, and every write goes through `vm-memory`'s checked `write_slice`.
fn fill_capture_buffer(mem: &GuestMem, stream: &mut Stream, buffer: &CaptureBuffer) -> usize {
    let take = buffer.room.min(stream.ring.len());
    if take == 0 {
        return 0;
    }
    // The ring is a VecDeque, so the audio may be in two runs. Copying it into
    // one host-owned staging vector first keeps the scatter below a single
    // straight-line bounded loop.
    let mut audio = Vec::with_capacity(take);
    {
        let (front, back) = stream.ring.as_slices();
        for run in [front, back] {
            if audio.len() >= take {
                break;
            }
            let n = run.len().min(take - audio.len());
            if let Some(src) = run.get(..n) {
                audio.extend_from_slice(src);
            }
        }
    }
    let written = scatter(mem, &buffer.segments, &audio) as usize;
    // The bytes leave the ring whether or not the guest's own buffer turned
    // out to be writable: they were this buffer's audio, and re-offering them
    // to the next one would shift every later sample.
    stream.ring.drain(..take.min(stream.ring.len()));
    written.min(buffer.room)
}

// -------------------------------------------------------------------- the pump

struct PumpContext {
    shared: Arc<Shared>,
    quiesce: Arc<Quiesce>,
    factory: SinkFactory,
    name: String,
}

struct CaptureContext {
    shared: Arc<Shared>,
    quiesce: Arc<Quiesce>,
    factory: SourceFactory,
    name: String,
}

/// Tracks how many underruns happened since the last log line.
struct UnderrunLog {
    last: Instant,
    since: u64,
}

impl UnderrunLog {
    fn new() -> Self {
        Self {
            last: Instant::now(),
            since: 0,
        }
    }

    /// Records an underrun; returns the count to log, or `None` to stay quiet.
    fn record(&mut self) -> Option<u64> {
        self.since += 1;
        if self.last.elapsed() < UNDERRUN_LOG_EVERY {
            return None;
        }
        self.last = Instant::now();
        Some(std::mem::take(&mut self.since))
    }
}

/// What the pump should do this round, decided under the lock and acted on
/// outside it.
enum Plan {
    Idle,
    Play {
        epoch: u64,
        format: StreamFormat,
        period_bytes: usize,
    },
}

/// What one stream wants from its pump right now.
fn plan_for(inner: &Inner, index: usize) -> Plan {
    let Some(stream) = inner.streams.get(index) else {
        return Plan::Idle;
    };
    match (stream.state, stream.params) {
        (StreamState::Running, Some(params)) => Plan::Play {
            epoch: stream.epoch,
            format: StreamFormat {
                rate_hz: params.rate_hz,
                channels: params.channels,
            },
            period_bytes: (params.period_bytes as usize).min(PUMP_CHUNK_BYTES),
        },
        _ => Plan::Idle,
    }
}

/// Parks a pump until something changes or the idle tick expires.
fn pump_wait(shared: &Shared) {
    let guard = lock(&shared.inner);
    let guard = shared
        .signal
        .wait_timeout(guard, PUMP_IDLE_TICK)
        .map(|(guard, _)| guard)
        .unwrap_or_else(|poisoned| poisoned.into_inner().0);
    drop(guard);
}

fn pump_loop(ctx: PumpContext) {
    // Built here, on this thread, and dropped here too — see [`SinkFactory`].
    let mut sink = (ctx.factory)();
    let mut open: Option<u64> = None;
    let mut scratch: Vec<u8> = Vec::new();
    let mut underruns = UnderrunLog::new();
    // Set when the configured sink could not be opened: the stream keeps
    // running against a null sink so the guest is not wedged by a host that
    // has no speakers.
    let mut fallback: Option<NullSink> = None;

    while !ctx.shared.stop.load(Ordering::Acquire) {
        let plan = {
            let inner = lock(&ctx.shared.inner);
            plan_for(&inner, OUT)
        };

        let Plan::Play {
            epoch,
            format,
            period_bytes,
        } = plan
        else {
            if open.take().is_some() {
                sink.stop();
                if let Some(null) = fallback.as_mut() {
                    null.stop();
                }
                fallback = None;
            }
            pump_wait(&ctx.shared);
            continue;
        };

        // Nothing below this line may touch guest memory while the VM is
        // paused, and the pass is held for the whole period so a pause is not
        // acknowledged mid-completion (ADR-0005).
        let Some(_pass) = ctx
            .quiesce
            .wait_while_paused(|| !ctx.shared.stop.load(Ordering::Acquire))
        else {
            return;
        };

        if open != Some(epoch) {
            sink.stop();
            fallback = None;
            match sink.start(format, period_bytes) {
                Ok(()) => tracing::info!(
                    sink = %ctx.name,
                    %format,
                    period_bytes,
                    "virtio-snd playback started"
                ),
                Err(error) => {
                    SoundStats::bump(&ctx.shared.stats.sink_failures);
                    tracing::warn!(
                        sink = %ctx.name,
                        %format,
                        %error,
                        "host audio unavailable; the guest keeps playing into silence"
                    );
                    let mut null = NullSink::new();
                    // A null sink's start cannot fail, but the result is
                    // checked rather than discarded.
                    if let Err(error) = null.start(format, period_bytes) {
                        tracing::error!(%error, "even the null sink refused to start");
                    }
                    fallback = Some(null);
                }
            }
            open = Some(epoch);
        }

        // Take up to one chunk of real audio; pad the rest with silence.
        scratch.clear();
        scratch.resize(period_bytes, 0);
        let taken = {
            let mut inner = lock(&ctx.shared.inner);
            let stream = &mut inner.streams[OUT];
            if stream.epoch != epoch {
                continue;
            }
            let want = period_bytes.min(stream.ring.len());
            let mut taken = 0usize;
            {
                let (front, back) = stream.ring.as_slices();
                for source in [front, back] {
                    if taken >= want {
                        break;
                    }
                    let n = source.len().min(want - taken);
                    if let (Some(dst), Some(src)) =
                        (scratch.get_mut(taken..taken + n), source.get(..n))
                    {
                        dst.copy_from_slice(src);
                        taken += n;
                    }
                }
            }
            stream.ring.drain(..taken);
            if taken > 0 {
                stream.flowing = true;
            } else if !stream.flowing {
                // Started but the driver has not queued anything yet: not an
                // underrun, just the gap before the first period.
            }
            taken
        };
        if taken < period_bytes {
            SoundStats::bump(&ctx.shared.stats.underruns);
            if let Some(count) = underruns.record() {
                tracing::warn!(
                    sink = %ctx.name,
                    count,
                    short_bytes = period_bytes - taken,
                    "virtio-snd underrun: the guest did not keep the buffer full"
                );
            }
        }

        // Outside the lock: a sink write blocks for about a period.
        let active: &mut dyn AudioSink = match fallback.as_mut() {
            Some(null) => null,
            None => sink.as_mut(),
        };
        let mut at = 0usize;
        while at < scratch.len() {
            match active.write(scratch.get(at..).unwrap_or(&[])) {
                Ok(0) => break,
                Ok(n) => at += n,
                Err(error) => {
                    SoundStats::bump(&ctx.shared.stats.sink_failures);
                    tracing::warn!(sink = %ctx.name, %error, "host audio write failed");
                    // Drop back to silence for the rest of this stream rather
                    // than hammering a broken device once per period.
                    let mut null = NullSink::new();
                    let _ = null.start(format, period_bytes);
                    fallback = Some(null);
                    break;
                }
            }
        }
        SoundStats::bump(&ctx.shared.stats.periods_played);
        SoundStats::add(&ctx.shared.stats.bytes_played, taken as u64);

        // Account only the real bytes: silence must not retire a message.
        let (signal, interrupt) = {
            let mut inner = lock(&ctx.shared.inner);
            if inner.streams[OUT].epoch != epoch {
                // The stream was stopped while we were writing; its pending
                // messages have already gone back to the guest.
                continue;
            }
            let stream = &mut inner.streams[OUT];
            stream.consumed = stream.consumed.saturating_add(taken as u64);
            let any = retire_consumed(&mut inner, OUT);
            let signal = any && needs_notification(&mut inner, OUT);
            (signal, inner.interrupt.as_ref().map(Arc::clone))
        };
        if signal {
            if let Some(interrupt) = interrupt {
                if let Err(error) = interrupt.signal_used_queue(protocol::VQ_TX) {
                    tracing::warn!(sink = %ctx.name, %error, "cannot signal virtio-snd completion");
                }
            }
        }
    }

    sink.stop();
}

// ------------------------------------------------------------- capture pump

/// How long the capture stream may go without a guest buffer to fill before
/// the missing microphone audio is counted as an overrun. One tick of slack on
/// top of a period, so a guest that reaps and re-posts promptly is never
/// blamed for the gap in between.
const CAPTURE_STARVE_GRACE: Duration = Duration::from_millis(50);

/// The mirror image of [`pump_loop`]: it pulls audio out of the host source
/// and hands it to the guest's posted buffers.
///
/// The pacing property that makes the whole thing honest lives in two lines
/// below. The pump asks the source for **only the bytes the guest has already
/// made room for** (`queued - consumed`), and the source blocks for as long as
/// that audio lasts. So a buffer becomes retirable when its audio *exists* on
/// the host clock, never when its request arrived — which is what stops a
/// guest with a deep ring from pulling an hour of "recording" out of a
/// millisecond.
fn capture_loop(ctx: CaptureContext) {
    // Built here, on this thread, and dropped here too — see [`SourceFactory`].
    let mut source = (ctx.factory)();
    let mut open: Option<u64> = None;
    let mut scratch: Vec<u8> = Vec::new();
    let mut overruns = UnderrunLog::new();
    let mut starving_since: Option<Instant> = None;
    // Set when the configured source could not be opened: the stream keeps
    // running against silence so the guest is not wedged by a host that has no
    // microphone.
    let mut fallback: Option<SilentSource> = None;

    while !ctx.shared.stop.load(Ordering::Acquire) {
        let plan = {
            let inner = lock(&ctx.shared.inner);
            plan_for(&inner, IN)
        };

        let Plan::Play {
            epoch,
            format,
            period_bytes,
        } = plan
        else {
            if open.take().is_some() {
                source.stop();
                if let Some(silent) = fallback.as_mut() {
                    silent.stop();
                }
                fallback = None;
                starving_since = None;
            }
            pump_wait(&ctx.shared);
            continue;
        };

        // Nothing below this line may touch guest memory while the VM is
        // paused, and the pass is held for the whole period so a pause is not
        // acknowledged mid-completion (ADR-0005).
        let Some(_pass) = ctx
            .quiesce
            .wait_while_paused(|| !ctx.shared.stop.load(Ordering::Acquire))
        else {
            return;
        };

        if open != Some(epoch) {
            source.stop();
            fallback = None;
            starving_since = None;
            match source.start(format, period_bytes) {
                Ok(()) => tracing::info!(
                    source = %ctx.name,
                    %format,
                    period_bytes,
                    "virtio-snd capture started"
                ),
                Err(error) => {
                    SoundStats::bump(&ctx.shared.stats.sink_failures);
                    tracing::warn!(
                        source = %ctx.name,
                        %format,
                        %error,
                        "host microphone unavailable; the guest records silence"
                    );
                    let mut silent = SilentSource::new();
                    // A silent source's start cannot fail, but the result is
                    // checked rather than discarded.
                    if let Err(error) = silent.start(format, period_bytes) {
                        tracing::error!(%error, "even the silent source refused to start");
                    }
                    fallback = Some(silent);
                }
            }
            open = Some(epoch);
        }

        // Ask for exactly the audio the guest has promised to take, and never
        // more than one chunk. `want == 0` means the guest has posted nothing:
        // the microphone is producing audio nobody asked for, which is an
        // overrun rather than something to buffer without bound.
        let want = {
            let inner = lock(&ctx.shared.inner);
            let stream = &inner.streams[IN];
            if stream.epoch != epoch {
                continue;
            }
            let owed = stream.queued.saturating_sub(stream.consumed);
            let room = stream.ring_capacity().saturating_sub(stream.ring.len());
            usize::try_from(owed)
                .unwrap_or(usize::MAX)
                .min(period_bytes)
                .min(room)
        };
        if want == 0 {
            let since = *starving_since.get_or_insert_with(Instant::now);
            if since.elapsed() >= CAPTURE_STARVE_GRACE {
                SoundStats::bump(&ctx.shared.stats.overruns);
                starving_since = Some(Instant::now());
                if let Some(count) = overruns.record() {
                    tracing::warn!(
                        source = %ctx.name,
                        count,
                        "virtio-snd capture overrun: the guest posted no buffer to record into"
                    );
                }
            }
            drop(_pass);
            pump_wait(&ctx.shared);
            continue;
        }
        starving_since = None;

        // Outside the lock: a source read blocks for about the audio's own
        // duration, which is exactly the pacing the guest's clock depends on.
        scratch.clear();
        scratch.resize(want, 0);
        let active: &mut dyn AudioSource = match fallback.as_mut() {
            Some(silent) => silent,
            None => source.as_mut(),
        };
        let produced = match active.read(&mut scratch) {
            // A source claiming more than the room it was given is a host bug,
            // not a guest one — clamp rather than trust, because the number is
            // about to bound a copy towards guest memory.
            Ok(n) => n.min(want),
            Err(error) => {
                SoundStats::bump(&ctx.shared.stats.sink_failures);
                tracing::warn!(source = %ctx.name, %error, "host audio read failed");
                // Drop back to silence for the rest of this stream rather than
                // hammering a broken device once per period.
                let mut silent = SilentSource::new();
                let _ = silent.start(format, period_bytes);
                fallback = Some(silent);
                0
            }
        };

        let (signal, interrupt) = {
            let mut inner = lock(&ctx.shared.inner);
            if inner.streams[IN].epoch != epoch {
                // The stream was stopped while we were reading; its pending
                // buffers have already gone back to the guest.
                continue;
            }
            let stream = &mut inner.streams[IN];
            if let Some(audio) = scratch.get(..produced) {
                stream.ring.extend(audio.iter().copied());
                stream.consumed = stream.consumed.saturating_add(produced as u64);
                if produced > 0 {
                    stream.flowing = true;
                }
            }
            let any = retire_consumed(&mut inner, IN);
            let signal = any && needs_notification(&mut inner, IN);
            (signal, inner.interrupt.as_ref().map(Arc::clone))
        };
        SoundStats::add(&ctx.shared.stats.bytes_captured, produced as u64);
        if produced > 0 {
            SoundStats::bump(&ctx.shared.stats.periods_captured);
        }
        if signal {
            if let Some(interrupt) = interrupt {
                if let Err(error) = interrupt.signal_used_queue(protocol::VQ_RX) {
                    tracing::warn!(source = %ctx.name, %error, "cannot signal virtio-snd capture");
                }
            }
        }
    }

    source.stop();
}

// ------------------------------------------------------------- info records

/// Jack `id`: 0 is the line-out, 1 the microphone. Both are reported plugged
/// in, because both are as real as the streams behind them.
fn jack_info(id: u32) -> protocol::JackInfo {
    protocol::JackInfo {
        hda_fn_nid: id,
        // VIRTIO_SND_JACK_F_REMAP is not offered: each jack has exactly one
        // stream and nothing to remap it to.
        features: 0,
        hda_reg_defconf: if id == stream::INPUT_STREAM {
            protocol::DEFCONF_MIC_IN
        } else {
            protocol::DEFCONF_LINE_OUT
        },
        hda_reg_caps: 0,
        connected: true,
    }
}

/// PCM stream `id`: the same formats, rates and channel counts either way —
/// the direction is the only field that differs, and widening one side's set
/// would be host code on an untrusted path for no gain (see [`crate::stream`]).
fn pcm_info(id: u32) -> protocol::PcmInfo {
    protocol::PcmInfo {
        hda_fn_nid: id,
        features: stream::ADVERTISED_PCM_FEATURES,
        formats: stream::formats_bitmap(),
        rates: stream::rates_bitmap(),
        direction: stream::direction_of(id).unwrap_or(protocol::D_OUTPUT),
        channels_min: stream::MIN_CHANNELS,
        channels_max: stream::MAX_CHANNELS,
    }
}

/// Channel map `id`, one per stream and in stream order.
fn chmap_info(id: u32) -> protocol::ChmapInfo {
    let mut info = if id == stream::INPUT_STREAM {
        protocol::ChmapInfo::stereo_input()
    } else {
        protocol::ChmapInfo::stereo_output()
    };
    info.hda_fn_nid = id;
    info
}

// ------------------------------------------------------------- VirtioDevice

impl VirtioDevice for SoundDevice {
    fn device_type(&self) -> DeviceType {
        DeviceType::Sound
    }

    fn queue_max_sizes(&self) -> &[u16] {
        &QUEUE_MAX_SIZES
    }

    fn device_features(&self) -> u64 {
        self.features
    }

    fn ack_features(&mut self, negotiated: u64) -> bool {
        if negotiated & VIRTIO_F_VERSION_1 == 0 {
            return false;
        }
        if negotiated & !self.features != 0 {
            tracing::warn!(
                sink = %self.name,
                negotiated = format_args!("{negotiated:#x}"),
                offered = format_args!("{:#x}", self.features),
                "driver accepted features the device never offered"
            );
            return false;
        }
        self.acked_features = negotiated;
        true
    }

    fn read_config(&self, offset: u64, data: &mut [u8]) {
        let config = self.config_space();
        for (i, byte) in data.iter_mut().enumerate() {
            let index = offset.saturating_add(i as u64);
            *byte = usize::try_from(index)
                .ok()
                .and_then(|i| config.get(i))
                .copied()
                .unwrap_or(0);
        }
    }

    fn write_config(&mut self, offset: u64, data: &[u8]) {
        tracing::warn!(
            sink = %self.name,
            offset,
            len = data.len(),
            "ignoring guest write to the read-only virtio-snd config space"
        );
    }

    fn activate(&mut self, resources: DeviceResources) -> Result<(), DeviceError> {
        if resources.queues.len() != NUM_QUEUES {
            return Err(DeviceError::QueueCount {
                expected: NUM_QUEUES,
                actual: resources.queues.len(),
            });
        }
        // Re-activation without a reset in between must not leak a pump.
        self.stop_pump();

        let mut queues = resources.queues.into_iter();
        let (Some(control), Some(event), Some(tx), Some(rx)) =
            (queues.next(), queues.next(), queues.next(), queues.next())
        else {
            // Unreachable: the length was just checked.
            return Err(DeviceError::QueueCount {
                expected: NUM_QUEUES,
                actual: 0,
            });
        };

        {
            let mut inner = lock(&self.shared.inner);
            inner.mem = Some(Arc::clone(&resources.mem));
            inner.interrupt = Some(Arc::clone(&resources.interrupt));
            inner.tx_queue = Some(tx);
            inner.rx_queue = Some(rx);
            inner.streams = Default::default();
        }
        self.control_queue = Some(control);
        self.event_queue = Some(event);
        self.mem = Some(resources.mem);
        self.interrupt = Some(resources.interrupt);

        self.shared.stop.store(false, Ordering::Release);
        let playback_ctx = PumpContext {
            shared: Arc::clone(&self.shared),
            quiesce: Arc::clone(&resources.quiesce),
            factory: Arc::clone(&self.factory),
            name: self.name.clone(),
        };
        let playback = std::thread::Builder::new()
            .name("entangled-snd-pump".to_owned())
            .spawn(move || pump_loop(playback_ctx))
            .map_err(|error| {
                DeviceError::Backend(format!("cannot spawn the virtio-snd pump: {error}"))
            })?;
        let capture_ctx = CaptureContext {
            shared: Arc::clone(&self.shared),
            quiesce: Arc::clone(&resources.quiesce),
            factory: Arc::clone(&self.source_factory),
            name: self.source_name.clone(),
        };
        let capture = match std::thread::Builder::new()
            .name("entangled-snd-capture".to_owned())
            .spawn(move || capture_loop(capture_ctx))
        {
            Ok(thread) => thread,
            Err(error) => {
                // The playback thread is already running; joining it here is
                // what keeps a failed activation from leaking one.
                self.shared.stop.store(true, Ordering::Release);
                self.shared.wake();
                resources.quiesce.wake();
                let _ = playback.join();
                self.shared.stop.store(false, Ordering::Release);
                return Err(DeviceError::Backend(format!(
                    "cannot spawn the virtio-snd capture pump: {error}"
                )));
            }
        };
        self.pump = Some(Pump {
            playback,
            capture,
            quiesce: resources.quiesce,
        });

        tracing::info!(
            sink = %self.name,
            source = %self.source_name,
            streams = stream::STREAMS,
            "virtio-snd ready"
        );
        Ok(())
    }

    fn notify(&mut self, queue_index: u16) -> Result<(), DeviceError> {
        match queue_index {
            protocol::VQ_CONTROL => self.drain_control(),
            protocol::VQ_EVENT => {
                // We advertise no feature that produces an event, so the
                // driver's buffers stay available. Nothing to do — and a
                // spurious kick here is how a host waker would arrive, which
                // must also be a no-op.
                if self.event_queue.is_none() {
                    return Err(DeviceError::NotActivated);
                }
                Ok(())
            }
            protocol::VQ_TX => self.drain_tx(),
            protocol::VQ_RX => self.drain_rx(),
            other => Err(DeviceError::UnknownQueue(other)),
        }
    }

    /// All four queues' positions, in spec order: control, event, TX, RX
    /// (ADR-0006).
    ///
    /// TX and RX are read through the handles the device shares with its
    /// pumps, which is safe here and only here: this runs with the VM paused
    /// and both pumps parked on the quiesce gate, so the lock is free and the
    /// numbers are not moving.
    ///
    /// **The two audio queues are rewound by what the device is still
    /// holding.** A playback message whose audio has not been consumed, and a
    /// capture buffer that has not been filled, have had *no* guest-visible
    /// effect: their bytes live in a host ring the restore throws away. So
    /// rather than lose those descriptors — the guest never sees them in the
    /// used ring, and its driver waits for them forever — the reported
    /// `next_avail` is moved back over them, and the restored device pops them
    /// again. That is exactly equivalent to their never having been taken. It
    /// is the opposite of what virtio-gpu needs from the same field, and for
    /// the opposite reason: a GPU command in flight has already had its side
    /// effect, so re-running it would duplicate work; an unconsumed period has
    /// had none.
    ///
    /// **What a restored virtio-snd cannot bring back is the host endpoint.**
    /// The WASAPI or ALSA device this process opened is gone, and a new one is
    /// opened at whatever position it starts from — so a stream that was
    /// mid-playback resumes with a gap and a capture stream misses the audio
    /// of the suspend itself, exactly as they would across a real machine's
    /// suspend. The guest's *stream state* (its parameters, whether it was
    /// started) is the device's own and comes back from
    /// [`Self::save_device`].
    fn queue_positions(&self) -> Vec<virtio_core::QueuePosition> {
        let position = |queue: &Queue| virtio_core::QueuePosition {
            next_avail: queue.next_avail(),
            next_used: queue.next_used(),
        };
        let rewound = |queue: &Queue, held: usize| virtio_core::QueuePosition {
            next_avail: queue
                .next_avail()
                .wrapping_sub(u16::try_from(held).unwrap_or(u16::MAX)),
            next_used: queue.next_used(),
        };
        let inner = lock(&self.shared.inner);
        match (
            &self.control_queue,
            &self.event_queue,
            &inner.tx_queue,
            &inner.rx_queue,
        ) {
            (Some(control), Some(event), Some(tx), Some(rx)) => vec![
                position(control),
                position(event),
                rewound(tx, inner.streams[OUT].pending.len()),
                rewound(rx, inner.streams[IN].pending.len()),
            ],
            _ => Vec::new(),
        }
    }

    /// The guest's stream state, for both streams (ADR-0006).
    ///
    /// Deliberately small: everything else a running virtio-snd holds is a
    /// host artefact. The rings are audio that has not crossed the boundary
    /// yet, the pending lists are guest descriptors that come back through
    /// [`Self::queue_positions`], and the sink and source belong to a process
    /// this one no longer is.
    fn save_device(&self) -> Vec<u8> {
        let inner = lock(&self.shared.inner);
        save::encode(&inner.streams)
    }

    /// Puts the stream state back, re-validating every field.
    ///
    /// The bytes come out of a file, so they are treated exactly like a
    /// `SET_PARAMS` off the control queue: the parameters go through
    /// [`stream::validate_params`] and a snapshot that names a format, rate,
    /// channel count or geometry this device does not advertise is a refused
    /// restore, not a stream the host would then act on.
    fn load_device(&mut self, bytes: &[u8]) -> Result<(), DeviceError> {
        if bytes.is_empty() {
            return Ok(());
        }
        let restored = save::decode(bytes).map_err(DeviceError::Backend)?;
        let mut inner = lock(&self.shared.inner);
        for (index, (state, params)) in restored.into_iter().enumerate() {
            let Some(stream) = inner.streams.get_mut(index) else {
                break;
            };
            stream.state = state;
            stream.params = params;
            stream.epoch = stream.epoch.wrapping_add(1);
            stream.clear_audio();
        }
        drop(inner);
        // A stream that was Running comes back Running, so the pumps have to
        // be told to look again rather than waiting out their idle tick.
        self.shared.wake();
        Ok(())
    }

    fn reset(&mut self) {
        // Order matters: stop the pumps first so nothing touches the queues or
        // guest memory after they are dropped.
        self.stop_pump();
        {
            let mut inner = lock(&self.shared.inner);
            inner.streams = Default::default();
            inner.tx_queue = None;
            inner.rx_queue = None;
            inner.mem = None;
            inner.interrupt = None;
        }
        self.control_queue = None;
        self.event_queue = None;
        self.mem = None;
        self.interrupt = None;
        self.acked_features = 0;
        self.staging = Vec::new();
        // The host sink and source went with the pump threads that owned them;
        // the factories stay, so a driver that resets and re-initialises gets
        // its audio back rather than silently dropping to silence.
    }
}

// ------------------------------------------------------------- saved state

/// The `save_device` payload: a magic, a version and the two streams'
/// guest-programmed state (ADR-0006).
mod save {
    use super::{Stream, NUM_STREAMS};
    use crate::protocol::RawSetParams;
    use crate::stream::{self, PcmParams, StreamState};

    /// `"SND"` and a format version. Bumping the last byte is what makes an
    /// older snapshot a named refusal rather than a misread struct.
    const MAGIC: [u8; 4] = *b"SND1";
    /// magic + stream count.
    const HEADER_LEN: usize = 5;
    /// state, has_params, then the five parameter fields.
    const STREAM_LEN: usize = 1 + 1 + 4 + 4 + 4 + 1 + 1;

    fn state_code(state: StreamState) -> u8 {
        match state {
            StreamState::Unset => 0,
            StreamState::ParamsSet => 1,
            StreamState::Prepared => 2,
            StreamState::Running => 3,
        }
    }

    fn state_of(code: u8) -> Option<StreamState> {
        Some(match code {
            0 => StreamState::Unset,
            1 => StreamState::ParamsSet,
            2 => StreamState::Prepared,
            3 => StreamState::Running,
            _ => return None,
        })
    }

    pub(super) fn encode(streams: &[Stream; NUM_STREAMS]) -> Vec<u8> {
        let mut raw = Vec::with_capacity(HEADER_LEN + NUM_STREAMS * STREAM_LEN);
        raw.extend_from_slice(&MAGIC);
        raw.push(NUM_STREAMS as u8);
        for stream in streams {
            raw.push(state_code(stream.state));
            match stream.params {
                Some(params) => {
                    raw.push(1);
                    raw.extend_from_slice(&params.buffer_bytes.to_le_bytes());
                    raw.extend_from_slice(&params.period_bytes.to_le_bytes());
                    raw.extend_from_slice(&params.rate_hz.to_le_bytes());
                    raw.push(params.channels);
                    raw.push(params.format);
                }
                None => {
                    raw.push(0);
                    raw.extend_from_slice(&[0u8; STREAM_LEN - 2]);
                }
            }
        }
        raw
    }

    #[allow(clippy::type_complexity)]
    pub(super) fn decode(raw: &[u8]) -> Result<Vec<(StreamState, Option<PcmParams>)>, String> {
        if raw.get(..4) != Some(&MAGIC[..]) {
            return Err("virtio-snd snapshot section has the wrong magic".to_owned());
        }
        let count = usize::from(
            *raw.get(4)
                .ok_or("virtio-snd snapshot section is truncated")?,
        );
        if count != NUM_STREAMS {
            return Err(format!(
                "virtio-snd snapshot has {count} streams, this device has {NUM_STREAMS}"
            ));
        }
        let want = HEADER_LEN + count * STREAM_LEN;
        if raw.len() < want {
            return Err(format!(
                "virtio-snd snapshot section is {} bytes, expected {want}",
                raw.len()
            ));
        }
        let mut out = Vec::with_capacity(count);
        for index in 0..count {
            let at = HEADER_LEN + index * STREAM_LEN;
            let field = |offset: usize| raw.get(at + offset).copied().unwrap_or(0);
            let le32 = |offset: usize| {
                let mut bytes = [0u8; 4];
                if let Some(slice) = raw.get(at + offset..at + offset + 4) {
                    bytes.copy_from_slice(slice);
                }
                u32::from_le_bytes(bytes)
            };
            let state = state_of(field(0))
                .ok_or_else(|| format!("virtio-snd snapshot stream {index} has no such state"))?;
            let params = if field(1) == 0 {
                None
            } else {
                // Layout: state, has_params, buffer_bytes, period_bytes,
                // rate_hz, channels, format — see `encode`.
                let rate_hz = le32(10);
                // Back to a *rate index*, so the same table the guest's own
                // SET_PARAMS is checked against decides whether this is a rate
                // we advertise. A file naming 96 kHz is refused, not resampled.
                let rate = (0..crate::protocol::RATE_COUNT)
                    .find(|r| crate::protocol::rate_hz(*r) == Some(rate_hz))
                    .ok_or_else(|| {
                        format!("virtio-snd snapshot stream {index} names {rate_hz} Hz")
                    })?;
                let request = RawSetParams {
                    stream_id: index as u32,
                    buffer_bytes: le32(2),
                    period_bytes: le32(6),
                    features: 0,
                    channels: field(14),
                    format: field(15),
                    rate,
                };
                let params = stream::validate_params(&request)
                    .map_err(|error| format!("virtio-snd snapshot stream {index}: {error}"))?;
                Some(params)
            };
            // A state past `Unset` with no parameters is a file describing a
            // device that could not exist.
            if state != StreamState::Unset && params.is_none() {
                return Err(format!(
                    "virtio-snd snapshot stream {index} is {state:?} with no parameters"
                ));
            }
            out.push((state, params));
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn device() -> SoundDevice {
        SoundDevice::null()
    }

    #[test]
    fn advertises_exactly_version_1_and_four_queues() {
        let device = device();
        assert_eq!(device.device_type(), DeviceType::Sound);
        assert_eq!(device.device_type().id(), 25);
        assert_eq!(device.device_features(), VIRTIO_F_VERSION_1);
        assert_eq!(device.num_queues(), 4);
        assert_eq!(device.queue_max_sizes(), &[MAX_QUEUE_SIZE; 4]);
    }

    #[test]
    fn feature_negotiation_is_exact() {
        let mut device = device();
        assert!(!device.ack_features(0), "VERSION_1 is mandatory");
        assert!(
            !device.ack_features(VIRTIO_F_VERSION_1 | 1),
            "a feature we never offered must be refused"
        );
        assert!(device.ack_features(VIRTIO_F_VERSION_1));
    }

    #[test]
    fn the_config_space_is_the_three_counts_and_nothing_else() {
        let device = device();
        let mut raw = [0xffu8; CONFIG_LEN];
        device.read_config(0, &mut raw);
        assert_eq!(&raw[0..4], &stream::JACKS.to_le_bytes());
        assert_eq!(&raw[4..8], &stream::STREAMS.to_le_bytes());
        assert_eq!(&raw[8..12], &stream::CHMAPS.to_le_bytes());
        // Reads past the end are zeroes, not panics.
        let mut past = [0xffu8; 8];
        device.read_config(CONFIG_LEN as u64, &mut past);
        assert_eq!(past, [0u8; 8]);
        let mut wrapped = [0xffu8; 4];
        device.read_config(u64::MAX, &mut wrapped);
        assert_eq!(wrapped, [0u8; 4]);
    }

    #[test]
    fn the_published_pcm_info_matches_what_validation_accepts() {
        for id in [stream::OUTPUT_STREAM, stream::INPUT_STREAM] {
            let info = pcm_info(id);
            assert_eq!(info.formats, stream::formats_bitmap());
            assert_eq!(info.rates, stream::rates_bitmap());
            assert_eq!(
                info.direction,
                stream::direction_of(id).expect("advertised")
            );
            assert_eq!(info.channels_max, stream::MAX_CHANNELS);
            assert_eq!(info.features, stream::ADVERTISED_PCM_FEATURES);
        }
        assert_eq!(
            pcm_info(stream::OUTPUT_STREAM).direction,
            protocol::D_OUTPUT
        );
        assert_eq!(pcm_info(stream::INPUT_STREAM).direction, protocol::D_INPUT);
    }

    /// The three per-id record builders must agree with each other: one jack,
    /// one PCM stream and one channel map per direction, all pointing the same
    /// way.
    #[test]
    fn every_advertised_id_has_a_matching_jack_stream_and_chmap() {
        assert_eq!(stream::JACKS, stream::STREAMS);
        assert_eq!(stream::CHMAPS, stream::STREAMS);
        for id in 0..stream::STREAMS {
            let direction = stream::direction_of(id).expect("advertised");
            assert_eq!(pcm_info(id).direction, direction);
            assert_eq!(chmap_info(id).direction, direction);
            assert_eq!(chmap_info(id).channels, stream::MAX_CHANNELS);
            let jack = jack_info(id);
            assert!(jack.connected);
            assert_eq!(jack.features, 0, "no jack is remappable");
        }
        assert_eq!(
            jack_info(stream::INPUT_STREAM).hda_reg_defconf,
            protocol::DEFCONF_MIC_IN
        );
        assert_eq!(
            jack_info(stream::OUTPUT_STREAM).hda_reg_defconf,
            protocol::DEFCONF_LINE_OUT
        );
    }

    /// The saved state is the guest's programming and nothing else, and it
    /// comes back through the same validation a `SET_PARAMS` goes through.
    #[test]
    fn the_saved_stream_state_round_trips_and_refuses_a_hostile_file() {
        let params = PcmParams {
            buffer_bytes: 8192,
            period_bytes: 2048,
            channels: 2,
            rate_hz: 48000,
            format: protocol::FMT_S16,
        };
        let mut streams: [Stream; NUM_STREAMS] = Default::default();
        streams[OUT].state = StreamState::Running;
        streams[OUT].params = Some(params);
        streams[IN].state = StreamState::Prepared;
        streams[IN].params = Some(params);
        let raw = save::encode(&streams);
        let back = save::decode(&raw).expect("our own bytes decode");
        assert_eq!(back[OUT], (StreamState::Running, Some(params)));
        assert_eq!(back[IN], (StreamState::Prepared, Some(params)));

        // An unset stream round-trips as unset with no parameters.
        let empty: [Stream; NUM_STREAMS] = Default::default();
        let back = save::decode(&save::encode(&empty)).expect("decode");
        assert_eq!(back[OUT], (StreamState::Unset, None));

        // And the refusals. The bytes come out of a file: every one of these
        // would otherwise be host state the guest never asked for.
        assert!(save::decode(&[]).is_err(), "no magic");
        assert!(save::decode(b"XXXX").is_err(), "wrong magic");
        assert!(save::decode(&raw[..raw.len() - 1]).is_err(), "truncated");
        let mut wrong_count = raw.clone();
        wrong_count[4] = 7;
        assert!(save::decode(&wrong_count).is_err(), "wrong stream count");
        let mut bad_state = raw.clone();
        bad_state[5] = 9;
        assert!(save::decode(&bad_state).is_err(), "no such state");
        // A geometry no SET_PARAMS would ever have been allowed to set.
        let mut huge = raw.clone();
        huge[5 + 2..5 + 6].copy_from_slice(&u32::MAX.to_le_bytes());
        assert!(save::decode(&huge).is_err(), "unbounded buffer accepted");
        // A rate we never advertised.
        let mut fast = raw.clone();
        fast[5 + 10..5 + 14].copy_from_slice(&192_000u32.to_le_bytes());
        assert!(save::decode(&fast).is_err(), "unadvertised rate accepted");
        // Running with nothing configured describes a device that cannot be.
        let mut orphan = raw.clone();
        orphan[5 + 1] = 0;
        assert!(save::decode(&orphan).is_err(), "running with no params");
    }

    #[test]
    fn a_device_that_was_never_activated_refuses_every_queue() {
        let mut device = device();
        for queue in 0..4u16 {
            assert!(matches!(
                device.notify(queue),
                Err(DeviceError::NotActivated)
            ));
        }
        assert!(matches!(
            device.notify(4),
            Err(DeviceError::UnknownQueue(4))
        ));
        // And reset on a fresh device is a no-op, not a panic.
        device.reset();
    }

    #[test]
    fn the_underrun_log_rate_limits_but_counts_everything() {
        let mut log = UnderrunLog::new();
        log.last = Instant::now() - Duration::from_secs(2);
        assert_eq!(log.record(), Some(1));
        assert_eq!(log.record(), None, "the second one is suppressed");
        assert_eq!(log.record(), None);
        log.last = Instant::now() - Duration::from_secs(2);
        assert_eq!(log.record(), Some(3), "suppressed ones are still counted");
    }

    /// Filling a capture buffer must never write more than the room the guest
    /// granted, however much audio the ring holds — the one rule the whole RX
    /// path rests on. Driven here against a real `GuestMem` so the bound is
    /// checked against bytes on the other side, not against an intention.
    #[test]
    fn a_capture_fill_never_writes_past_the_room_the_guest_granted() {
        let mem = virtio_core::testing::guest_memory(1 << 16);
        let base = 0x1000u64;
        let guard = base + 64;
        // Poison the whole area so any overrun shows up as a changed byte.
        mem.write_slice(&[0xab; 256], GuestAddress(base))
            .expect("test write");

        let mut stream = Stream::default();
        // Far more audio than the buffer can take.
        stream.ring.extend((0..200u8).map(|i| i.wrapping_add(1)));

        // Two segments of 32 bytes: 64 bytes of room, and not a byte more.
        let buffer = CaptureBuffer {
            segments: vec![
                Segment {
                    addr: base,
                    len: 32,
                    writable: true,
                },
                Segment {
                    addr: base + 32,
                    len: 32,
                    writable: true,
                },
            ],
            room: 64,
        };
        let written = fill_capture_buffer(&mem, &mut stream, &buffer);
        assert_eq!(written, 64);
        let mut got = [0u8; 64];
        mem.read_slice(&mut got, GuestAddress(base))
            .expect("test read");
        assert_eq!(got.to_vec(), (1..=64u8).collect::<Vec<_>>());
        let mut past = [0u8; 8];
        mem.read_slice(&mut past, GuestAddress(guard))
            .expect("test read");
        assert_eq!(past, [0xab; 8], "the fill ran past the granted room");
        assert_eq!(stream.ring.len(), 200 - 64, "only its own audio was taken");

        // And a buffer whose audio does not all exist yet takes only what does.
        let mut short = Stream::default();
        short.ring.extend([7u8; 10]);
        let written = fill_capture_buffer(&mem, &mut short, &buffer);
        assert_eq!(written, 10);
        assert!(short.ring.is_empty());

        // An empty ring writes nothing at all rather than a short buffer of
        // stale bytes.
        let mut dry = Stream::default();
        assert_eq!(fill_capture_buffer(&mem, &mut dry, &buffer), 0);
    }

    /// A capture buffer pointing outside guest RAM must be answered, not
    /// dereferenced — and its audio must still leave the ring, or every later
    /// sample would be shifted.
    #[test]
    fn a_capture_buffer_outside_guest_memory_costs_its_audio_and_nothing_else() {
        let mem = virtio_core::testing::guest_memory(1 << 16);
        let mut stream = Stream::default();
        stream.ring.extend([9u8; 64]);
        let buffer = CaptureBuffer {
            segments: vec![Segment {
                addr: 1 << 20,
                len: 64,
                writable: true,
            }],
            room: 64,
        };
        assert_eq!(fill_capture_buffer(&mem, &mut stream, &buffer), 0);
        assert!(stream.ring.is_empty());
    }

    #[test]
    fn the_ring_capacity_follows_the_negotiated_buffer() {
        let mut stream = Stream::default();
        assert_eq!(stream.ring_capacity(), 0, "no params, no ring");
        stream.params = Some(PcmParams {
            buffer_bytes: 8192,
            period_bytes: 2048,
            channels: 2,
            rate_hz: 48000,
            format: protocol::FMT_S16,
        });
        assert_eq!(stream.ring_capacity(), 8192 + 2048);
        // And it can never exceed the validated caps.
        stream.params = Some(PcmParams {
            buffer_bytes: stream::MAX_BUFFER_BYTES,
            period_bytes: stream::MAX_PERIOD_BYTES,
            channels: 2,
            rate_hz: 48000,
            format: protocol::FMT_S16,
        });
        assert!(
            stream.ring_capacity()
                <= (stream::MAX_BUFFER_BYTES + stream::MAX_PERIOD_BYTES) as usize
        );
    }
}
