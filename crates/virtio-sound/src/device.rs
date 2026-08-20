//! The virtio-snd device (backlog GAME-2102).
//!
//! # Shape
//!
//! Four queues, as the spec mandates: control, event, TX (playback), RX
//! (capture). The control queue carries information queries and the PCM
//! lifecycle; TX carries the audio. The event queue is drained of nothing —
//! we advertise none of the features that produce events — and RX is answered
//! `VIRTIO_SND_S_NOT_SUPP`, because no capture stream is advertised and a
//! conforming driver therefore never posts to it.
//!
//! ```text
//!   vCPU / queue-worker thread                pump thread
//!   ──────────────────────────                ───────────
//!   notify(CONTROL) ─ lifecycle ──┐        ┌─ take a period from the ring
//!   notify(TX) ─ stage a period ──┼─ Inner ┤  write it to the AudioSink
//!                                 │ (Mutex)│  advance `consumed`
//!                                 └────────┴─ retire the messages it covered
//! ```
//!
//! # Why a pump thread owns the completion
//!
//! virtio-snd is paced by *completions*: the driver puts one message per
//! period on TX and treats the used ring as the hardware pointer. Completing a
//! message as soon as its bytes are copied would tell the guest that a whole
//! period played in a microsecond, and ALSA would spin. So a message is only
//! retired once the host sink has actually consumed its audio — which happens
//! on a host thread, on the host clock, long after the vCPU that kicked the
//! queue went back to the guest.
//!
//! That thread owns the TX queue (an `Arc<Mutex<…>>` shared with the device,
//! exactly as virtio-net's receive worker owns the RX queue) and takes the
//! pause gate before it touches guest memory (ADR-0005). No [`HostWaker`] is
//! needed: unlike the GPU's fences, the completion is not something we have to
//! be *told* about from a foreign callback, it is something this thread
//! measured itself.
//!
//! [`HostWaker`]: virtio_core::HostWaker
//!
//! # Untrusted guest
//!
//! Every message length is checked before use; the chain walk is
//! `virtio_core::chain`'s bounded one; identifiers are checked against what
//! the config space advertises; formats, rates, channel counts and buffer
//! geometry are checked against [`crate::stream`]'s constants rather than
//! against anything the guest asserts; and the two host-memory bounds — the
//! PCM ring and the pending-completion list — are both capped by what the
//! *validated* parameters allow. A malformed request is answered with a
//! virtio-snd status code; nothing on this path panics or unwraps.
//!
//! # Suspend/restore seam
//!
//! Everything the guest programmed lives in one place, [`Stream`], plus the
//! per-queue state the transport owns. A future `save`/`load` (ADR-0005's next
//! step) serialises [`Stream::state`], [`Stream::params`] and the pending
//! list; the ring, the sink and the pump thread are host artefacts and are
//! rebuilt, not restored. [`SoundDevice::reset`] already returns exactly that
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

use crate::backend::{AudioSink, NullSink, StreamFormat};
use crate::protocol::{self, ItemHdr, QueryInfo, RawSetParams};
use crate::stream::{self, ParamError, PcmParams, StreamState};

/// Builds this device's host sink.
///
/// A factory rather than a sink, because the sink is created *and released* on
/// the pump thread: WASAPI's COM objects and ALSA's handle both belong to one
/// thread, and a device reset has to be able to get its audio back rather than
/// silently falling to silence.
pub type SinkFactory = Arc<dyn Fn() -> Box<dyn AudioSink> + Send + Sync>;

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

/// Playback messages the device may hold un-retired. A well-behaved driver
/// holds at most `buffer_bytes / period_bytes` of them; this is the backstop
/// that bounds the bookkeeping independently of the byte caps.
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

/// One playback message the device is holding until its audio has been played.
#[derive(Debug, Clone, Copy)]
struct Pending {
    /// Descriptor chain head, for the used ring.
    head: u16,
    /// Where the `virtio_snd_pcm_status` goes. Guest-supplied, so it is only
    /// ever used through a checked `vm-memory` write.
    status_addr: u64,
    /// Value of [`Stream::queued`] once this message's audio is consumed.
    consume_at: u64,
}

/// Everything the guest programmed about the one output stream, plus the host
/// ring that carries its audio.
///
/// The suspend/restore seam (see the module docs): `state`, `params` and
/// `pending` are guest state; `ring`, `queued`, `consumed`, `epoch` and
/// `flowing` describe host progress and are rebuilt rather than restored.
#[derive(Debug, Default)]
struct Stream {
    state: StreamState,
    params: Option<PcmParams>,
    /// Bumped whenever the host sink must be torn down and rebuilt (PREPARE,
    /// STOP, RELEASE, SET_PARAMS). The pump compares it to the epoch its sink
    /// was opened for, which is how a period staged before a STOP can never be
    /// accounted against the stream that came after it.
    epoch: u64,
    /// Audio waiting to be played, bounded by the negotiated buffer size.
    ring: VecDeque<u8>,
    pending: VecDeque<Pending>,
    /// Bytes ever pushed into `ring` for this stream.
    queued: u64,
    /// Bytes ever handed to the sink.
    consumed: u64,
    /// Set once real audio has flowed, so the gap between START and the first
    /// period is not counted as an underrun.
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
        self.flowing = false;
    }
}

/// The half of the device the pump thread shares with the queue paths.
#[derive(Default)]
struct Inner {
    mem: Option<Arc<GuestMem>>,
    interrupt: Option<Arc<dyn Interrupt>>,
    /// Owned jointly: `notify(TX)` fills it, the pump retires from it.
    tx_queue: Option<Queue>,
    stream: Stream,
}

impl std::fmt::Debug for Inner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Inner")
            .field("activated", &self.tx_queue.is_some())
            .field("stream", &self.stream)
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

/// The pump thread's handle, for a clean shutdown.
struct Pump {
    thread: std::thread::JoinHandle<()>,
    quiesce: Arc<Quiesce>,
}

// ------------------------------------------------------------------ the device

/// A virtio-snd device with one stereo output stream.
pub struct SoundDevice {
    /// Log label — the host sink's name at construction time.
    name: String,
    features: u64,
    acked_features: u64,
    shared: Arc<Shared>,
    factory: SinkFactory,
    pump: Option<Pump>,

    // Queues only the device touches. Set on activate(), cleared on reset().
    control_queue: Option<Queue>,
    event_queue: Option<Queue>,
    rx_queue: Option<Queue>,
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
            .field("activated", &self.control_queue.is_some())
            .finish_non_exhaustive()
    }
}

impl SoundDevice {
    /// Builds a device around a host sink factory. `name` labels it in logs
    /// (the factory's own sinks are only built once the pump runs).
    pub fn new(name: impl Into<String>, factory: SinkFactory) -> Self {
        Self {
            name: name.into(),
            features: VIRTIO_F_VERSION_1,
            acked_features: 0,
            shared: Arc::new(Shared::default()),
            factory,
            pump: None,
            control_queue: None,
            event_queue: None,
            rx_queue: None,
            mem: None,
            interrupt: None,
            staging: Vec::new(),
        }
    }

    /// A device that keeps time but makes no sound — the headless default.
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

    /// Name of the host sink, for logs and `doctor`.
    pub fn sink_name(&self) -> &str {
        &self.name
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
        for _ in query.start_id..end {
            match code {
                protocol::R_JACK_INFO => payload.extend_from_slice(&jack_info().encode()),
                protocol::R_PCM_INFO => payload.extend_from_slice(&pcm_info().encode()),
                _ => payload.extend_from_slice(&protocol::ChmapInfo::stereo_output().encode()),
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

        let mut inner = lock(&self.shared.inner);
        if let Err(error) = stream::transition(inner.stream.state, protocol::R_PCM_SET_PARAMS) {
            tracing::warn!(sink = %self.name, %error, "refusing SET_PARAMS");
            return error.status();
        }
        // Reconfiguring invalidates whatever was in flight, so the guest gets
        // its chains back before anything else changes.
        let retired = retire_all(&mut inner);
        inner.stream.clear_audio();
        inner.stream.params = Some(params);
        inner.stream.state = StreamState::ParamsSet;
        inner.stream.epoch = inner.stream.epoch.wrapping_add(1);
        let interrupt = inner.interrupt.as_ref().map(Arc::clone);
        drop(inner);
        self.shared.wake();
        signal_tx(retired, interrupt.as_deref(), &self.name);
        tracing::info!(
            sink = %self.name,
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
        if hdr.id >= stream::STREAMS {
            let error = ParamError::UnknownStream { id: hdr.id };
            tracing::warn!(
                sink = %self.name,
                command = stream::command_name(code),
                %error,
                "refusing a PCM command"
            );
            return error.status();
        }

        let mut inner = lock(&self.shared.inner);
        let next = match stream::transition(inner.stream.state, code) {
            Ok(next) => next,
            Err(error) => {
                tracing::warn!(sink = %self.name, %error, "refusing a PCM command");
                return error.status();
            }
        };
        // PREPARE with no parameters cannot happen (the state machine only
        // allows it from ParamsSet/Prepared, both of which imply params), but
        // the invariant is checked rather than assumed.
        if next != StreamState::Unset && inner.stream.params.is_none() {
            return protocol::S_BAD_MSG;
        }

        let mut retired = false;
        match code {
            protocol::R_PCM_PREPARE => {
                retired = retire_all(&mut inner);
                inner.stream.clear_audio();
                inner.stream.epoch = inner.stream.epoch.wrapping_add(1);
            }
            protocol::R_PCM_START => {
                inner.stream.flowing = false;
            }
            protocol::R_PCM_STOP | protocol::R_PCM_RELEASE => {
                // "The device MUST complete all pending I/O messages for the
                // specified stream" — spec 5.14.6.6.5/6.
                retired = retire_all(&mut inner);
                inner.stream.clear_audio();
                inner.stream.epoch = inner.stream.epoch.wrapping_add(1);
                if code == protocol::R_PCM_RELEASE {
                    inner.stream.params = None;
                }
            }
            _ => {}
        }
        inner.stream.state = next;
        let interrupt = inner.interrupt.as_ref().map(Arc::clone);
        drop(inner);
        self.shared.wake();
        // Chains handed back above need an interrupt of their own: they went
        // into the TX used ring, not this one.
        signal_tx(retired, interrupt.as_deref(), &self.name);
        tracing::debug!(
            sink = %self.name,
            command = stream::command_name(code),
            state = ?next,
            "virtio-snd stream lifecycle"
        );
        protocol::S_OK
    }

    // ----------------------------------------------------------------- TX queue

    fn drain_tx(&mut self) -> Result<(), DeviceError> {
        let mut inner = lock(&self.shared.inner);
        let Inner {
            mem,
            interrupt,
            tx_queue,
            stream,
        } = &mut *inner;
        let (Some(mem), Some(queue)) = (mem.as_ref(), tx_queue.as_mut()) else {
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
                    write_pcm_status(mem, status_addr, status, pending_bytes(stream));
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
                    "virtio-snd playback budget exhausted; deferring the rest"
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
                interrupt.signal_used_queue(protocol::VQ_TX)?;
            }
        }
        Ok(())
    }

    /// The capture queue. No capture stream is advertised, so a conforming
    /// driver never gets here; a non-conforming one is answered in band.
    fn drain_rx(&mut self) -> Result<(), DeviceError> {
        let mem = self.mem.clone().ok_or(DeviceError::NotActivated)?;
        let interrupt = self.interrupt.clone().ok_or(DeviceError::NotActivated)?;
        let mut queue = self.rx_queue.take().ok_or(DeviceError::NotActivated)?;
        let result = self.rx_loop(&mut queue, &mem, interrupt.as_ref());
        self.rx_queue = Some(queue);
        result
    }

    fn rx_loop(
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
            let mut written = 0u32;
            if let Some((_, writable)) = split_chain(mem, desc_table, queue_size, head, &self.name)
            {
                if let Some(last) = writable.last() {
                    if last.len as usize >= protocol::PCM_STATUS_LEN {
                        write_pcm_status(mem, last.addr, protocol::S_NOT_SUPP, 0);
                        written = protocol::PCM_STATUS_LEN as u32;
                    }
                }
            }
            SoundStats::bump(&self.shared.stats.rejected);
            queue
                .add_used(mem.as_ref(), head, written)
                .map_err(|e| DeviceError::Queue(e.to_string()))?;
            served += 1;
            if served >= CHAINS_PER_NOTIFY {
                break;
            }
        }
        if served > 0 {
            tracing::warn!(
                sink = %self.name,
                served,
                "guest posted capture buffers to a device that advertises no capture stream"
            );
            if queue
                .needs_notification(mem.as_ref())
                .map_err(|e| DeviceError::Queue(e.to_string()))?
            {
                interrupt.signal_used_queue(protocol::VQ_RX)?;
            }
        }
        Ok(())
    }

    // -------------------------------------------------------------- pump thread

    /// Stops and joins the pump. Idempotent and infallible, so `reset` and
    /// `Drop` can both call it.
    fn stop_pump(&mut self) {
        let Some(pump) = self.pump.take() else {
            return;
        };
        self.shared.stop.store(true, Ordering::Release);
        // Two places it can be waiting: our condvar and the pause gate (a
        // device reset runs on a quiesced VM — ADR-0005). Wake both, or the
        // join below is a deadlock.
        self.shared.wake();
        pump.quiesce.wake();
        match pump.thread.join() {
            Ok(()) => tracing::debug!(sink = %self.name, "virtio-snd pump stopped"),
            Err(_) => tracing::error!(sink = %self.name, "virtio-snd pump panicked"),
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

/// Raises the TX used-buffer interrupt after chains were handed back outside
/// the ordinary drain path (a STOP or RELEASE retiring what was in flight).
fn signal_tx(retired: bool, interrupt: Option<&dyn Interrupt>, name: &str) {
    if !retired {
        return;
    }
    let Some(interrupt) = interrupt else {
        return;
    };
    if let Err(error) = interrupt.signal_used_queue(protocol::VQ_TX) {
        tracing::warn!(sink = %name, %error, "cannot signal the virtio-snd TX queue");
    }
}

/// Whether the driver asked to be told about the used buffers just added.
fn needs_notification(inner: &mut Inner) -> bool {
    let Inner { mem, tx_queue, .. } = inner;
    match (tx_queue.as_mut(), mem.as_ref()) {
        (Some(queue), Some(mem)) => queue.needs_notification(mem.as_ref()).unwrap_or(false),
        _ => false,
    }
}

fn pending_bytes(stream: &Stream) -> u32 {
    u32::try_from(stream.queued.saturating_sub(stream.consumed)).unwrap_or(u32::MAX)
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

/// Validates and stages one playback message.
#[allow(clippy::too_many_arguments)]
fn stage_xfer(
    mem: &GuestMem,
    desc_table: u64,
    queue_size: u16,
    head: u16,
    staging: &mut Vec<u8>,
    stream: &mut Stream,
    stats: &SoundStats,
    name: &str,
) -> Xfer {
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

    let gathered = match gather(mem, &readable, MAX_XFER_BYTES, staging) {
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
    let payload = message.get(protocol::PCM_XFER_LEN..).unwrap_or(&[]);
    if let Err(error) = stream::validate_xfer(stream.state, stream.params, stream_id, payload.len())
    {
        tracing::warn!(sink = %name, %error, "refusing a virtio-snd I/O message");
        SoundStats::bump(&stats.rejected);
        return answer(error.status());
    }
    if stream.pending.len() >= MAX_PENDING_PERIODS
        || stream.ring.len().saturating_add(payload.len()) > stream.ring_capacity()
    {
        // The guest is running further ahead than the buffer it negotiated.
        // Refusing this message keeps the host allocation bounded and tells
        // the driver exactly what happened.
        SoundStats::bump(&stats.overruns);
        tracing::debug!(
            sink = %name,
            queued = stream.ring.len(),
            capacity = stream.ring_capacity(),
            pending = stream.pending.len(),
            "virtio-snd playback ring is full"
        );
        return answer(protocol::S_IO_ERR);
    }

    stream.ring.extend(payload.iter().copied());
    stream.queued = stream.queued.saturating_add(payload.len() as u64);
    stream.pending.push_back(Pending {
        head,
        status_addr,
        consume_at: stream.queued,
    });
    Xfer::Queued
}

// ---------------------------------------------------------------- completion

/// Retires every pending message whose audio has been consumed. Returns true
/// when at least one went back to the guest.
fn retire_consumed(inner: &mut Inner) -> bool {
    let consumed = inner.stream.consumed;
    retire_while(inner, |p| p.consume_at <= consumed)
}

/// Hands every pending message back, whatever the pump has played. Used by
/// STOP / RELEASE / SET_PARAMS, which the spec requires to drain the queue.
fn retire_all(inner: &mut Inner) -> bool {
    retire_while(inner, |_| true)
}

fn retire_while(inner: &mut Inner, ready: impl Fn(&Pending) -> bool) -> bool {
    let Inner {
        mem,
        tx_queue,
        stream,
        ..
    } = inner;
    let (Some(mem), Some(queue)) = (mem.as_ref(), tx_queue.as_mut()) else {
        // Not activated (or already reset): the chains went with the queue.
        stream.pending.clear();
        return false;
    };
    let latency = u32::try_from(stream.queued.saturating_sub(stream.consumed)).unwrap_or(u32::MAX);
    let mut any = false;
    while stream.pending.front().is_some_and(&ready) {
        let Some(pending) = stream.pending.pop_front() else {
            break;
        };
        write_pcm_status(mem, pending.status_addr, protocol::S_OK, latency);
        // Even a failed status write must return the chain, or the descriptor
        // leaks and the guest's ring drains to a halt.
        if let Err(error) =
            queue.add_used(mem.as_ref(), pending.head, protocol::PCM_STATUS_LEN as u32)
        {
            tracing::warn!(%error, "cannot retire a virtio-snd playback message");
            break;
        }
        any = true;
    }
    any
}

// -------------------------------------------------------------------- the pump

struct PumpContext {
    shared: Arc<Shared>,
    quiesce: Arc<Quiesce>,
    factory: SinkFactory,
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
            match (inner.stream.state, inner.stream.params) {
                (StreamState::Running, Some(params)) => Plan::Play {
                    epoch: inner.stream.epoch,
                    format: StreamFormat {
                        rate_hz: params.rate_hz,
                        channels: params.channels,
                    },
                    period_bytes: (params.period_bytes as usize).min(PUMP_CHUNK_BYTES),
                },
                _ => Plan::Idle,
            }
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
            let guard = lock(&ctx.shared.inner);
            let guard = ctx
                .shared
                .signal
                .wait_timeout(guard, PUMP_IDLE_TICK)
                .map(|(guard, _)| guard)
                .unwrap_or_else(|poisoned| poisoned.into_inner().0);
            drop(guard);
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
            if inner.stream.epoch != epoch {
                continue;
            }
            let want = period_bytes.min(inner.stream.ring.len());
            let mut taken = 0usize;
            {
                let (front, back) = inner.stream.ring.as_slices();
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
            inner.stream.ring.drain(..taken);
            if taken > 0 {
                inner.stream.flowing = true;
            } else if !inner.stream.flowing {
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
            if inner.stream.epoch != epoch {
                // The stream was stopped while we were writing; its pending
                // messages have already gone back to the guest.
                continue;
            }
            inner.stream.consumed = inner.stream.consumed.saturating_add(taken as u64);
            let any = retire_consumed(&mut inner);
            let signal = any && needs_notification(&mut inner);
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

// ------------------------------------------------------------- info records

fn jack_info() -> protocol::JackInfo {
    protocol::JackInfo {
        hda_fn_nid: 0,
        // VIRTIO_SND_JACK_F_REMAP is not offered: there is one jack and
        // nothing to remap it to.
        features: 0,
        // An HDA pin default configuration of zero reads as a line-out on a
        // complex (physical) port, which is what the guest should call it.
        hda_reg_defconf: 0,
        hda_reg_caps: 0,
        connected: true,
    }
}

fn pcm_info() -> protocol::PcmInfo {
    protocol::PcmInfo {
        hda_fn_nid: 0,
        features: stream::ADVERTISED_PCM_FEATURES,
        formats: stream::formats_bitmap(),
        rates: stream::rates_bitmap(),
        direction: protocol::D_OUTPUT,
        channels_min: stream::MIN_CHANNELS,
        channels_max: stream::MAX_CHANNELS,
    }
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
            inner.stream = Stream::default();
        }
        self.control_queue = Some(control);
        self.event_queue = Some(event);
        self.rx_queue = Some(rx);
        self.mem = Some(resources.mem);
        self.interrupt = Some(resources.interrupt);

        self.shared.stop.store(false, Ordering::Release);
        let context = PumpContext {
            shared: Arc::clone(&self.shared),
            quiesce: Arc::clone(&resources.quiesce),
            factory: Arc::clone(&self.factory),
            name: self.name.clone(),
        };
        let thread = std::thread::Builder::new()
            .name("entangled-snd-pump".to_owned())
            .spawn(move || pump_loop(context))
            .map_err(|error| {
                DeviceError::Backend(format!("cannot spawn the virtio-snd pump: {error}"))
            })?;
        self.pump = Some(Pump {
            thread,
            quiesce: resources.quiesce,
        });

        tracing::info!(
            sink = %self.name,
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
    /// TX is read through the handle the device keeps beside the pump's, which
    /// is safe here and only here: this runs with the VM paused and the pump
    /// parked on the quiesce gate, so the lock is free and the number is not
    /// moving.
    ///
    /// **What a restored virtio-snd cannot bring back is the host sink.** The
    /// WASAPI or ALSA device this process opened is gone, and a new one is
    /// opened at whatever position the sink starts from — so a stream that was
    /// mid-playback resumes with a gap, exactly as it would across a real
    /// machine's suspend. The guest's *stream state* (its parameters, whether
    /// it was started) is the device's own and comes back with the queues.
    fn queue_positions(&self) -> Vec<virtio_core::QueuePosition> {
        let position = |queue: &Queue| virtio_core::QueuePosition {
            next_avail: queue.next_avail(),
            next_used: queue.next_used(),
        };
        let inner = lock(&self.shared.inner);
        match (
            &self.control_queue,
            &self.event_queue,
            &inner.tx_queue,
            &self.rx_queue,
        ) {
            (Some(control), Some(event), Some(tx), Some(rx)) => vec![
                position(control),
                position(event),
                position(tx),
                position(rx),
            ],
            _ => Vec::new(),
        }
    }

    fn reset(&mut self) {
        // Order matters: stop the pump first so nothing touches the queues or
        // guest memory after they are dropped.
        self.stop_pump();
        {
            let mut inner = lock(&self.shared.inner);
            inner.stream = Stream::default();
            inner.tx_queue = None;
            inner.mem = None;
            inner.interrupt = None;
        }
        self.control_queue = None;
        self.event_queue = None;
        self.rx_queue = None;
        self.mem = None;
        self.interrupt = None;
        self.acked_features = 0;
        self.staging = Vec::new();
        // The host sink went with the pump thread that owned it; the factory
        // stays, so a driver that resets and re-initialises gets its audio
        // back rather than silently dropping to silence.
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
        let info = pcm_info();
        assert_eq!(info.formats, stream::formats_bitmap());
        assert_eq!(info.rates, stream::rates_bitmap());
        assert_eq!(info.direction, protocol::D_OUTPUT);
        assert_eq!(info.channels_max, stream::MAX_CHANNELS);
        assert_eq!(info.features, stream::ADVERTISED_PCM_FEATURES);
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
