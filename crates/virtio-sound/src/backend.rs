//! The host playback contract.
//!
//! [`SoundDevice`](crate::SoundDevice) never talks to an audio API; it fills a
//! ring and a pump thread hands periods to an [`AudioSink`]. Everything
//! guest-facing — the protocol, the validation, the completion accounting — is
//! identical on every host; only the sink differs.
//!
//! Three sinks ship:
//!
//! * [`NullSink`] — discards the audio but **keeps real time**, so a headless
//!   run, a host with no sound card and every test behave like a sound card
//!   that nobody is listening to. Always available, on every OS.
//! * [`RecordingSink`] — a null sink that also keeps the bytes, which is how
//!   the automated tests prove the guest's tone arrived with the right rate,
//!   format and framing.
//! * the real ones: [`crate::alsa::AlsaSink`] on Linux and
//!   [`crate::wasapi::WasapiSink`] on Windows.
//!
//! # Pacing is the contract
//!
//! [`AudioSink::write`] must take about as long as the audio it accepts lasts.
//! That is what makes the device's period-based completion work: a playback
//! message is returned to the guest when its audio has been *consumed*, and a
//! sink that accepted everything instantly would make ALSA in the guest
//! believe an hour of audio played in a millisecond. Real sinks get this from
//! the hardware clock; [`Pacer`] gives it to the null ones.

use std::time::{Duration, Instant};

use thiserror::Error;

/// Errors a host sink reports. All host-side: a guest mistake never reaches
/// this type, it is refused before the pump ever sees it.
#[derive(Debug, Error)]
pub enum AudioError {
    #[error("no host audio backend is available: {0}")]
    Unavailable(String),

    #[error("host audio device does not accept {rate_hz} Hz / {channels} ch S16: {reason}")]
    Format {
        rate_hz: u32,
        channels: u8,
        reason: String,
    },

    #[error("host audio device failed: {0}")]
    Io(String),
}

/// The PCM shape a sink is asked to play.
///
/// Sample format is always signed 16-bit little-endian, interleaved — the one
/// format the device advertises (see [`crate::stream`]). Widening that set
/// means widening this struct, and every sink with it; the type exists so that
/// change is a compile error rather than a silent misinterpretation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StreamFormat {
    pub rate_hz: u32,
    pub channels: u8,
}

impl StreamFormat {
    /// Bytes in one frame.
    pub fn frame_bytes(&self) -> usize {
        usize::from(self.channels) * 2
    }

    /// How long `bytes` of this format lasts. A degenerate format (no
    /// channels, no rate) cannot reach a sink — validation refuses both — but
    /// it lasts no time at all rather than dividing by zero.
    pub fn duration_of(&self, bytes: usize) -> Duration {
        let frame_bytes = self.frame_bytes();
        if frame_bytes == 0 || self.rate_hz == 0 {
            return Duration::ZERO;
        }
        let frames = (bytes / frame_bytes) as u64;
        Duration::from_nanos(
            frames
                .saturating_mul(1_000_000_000)
                .checked_div(u64::from(self.rate_hz))
                .unwrap_or(0),
        )
    }
}

impl std::fmt::Display for StreamFormat {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "S16_LE {} Hz {} ch", self.rate_hz, self.channels)
    }
}

/// A host playback endpoint.
///
/// Lives on, and is only ever touched from, the device's pump thread — which
/// is why the trait is `Send` but not `Sync`, and why WASAPI's thread-affine
/// COM objects and ALSA's handle are both safe behind it.
pub trait AudioSink: Send {
    /// Short label for log records, e.g. `alsa:default`.
    fn name(&self) -> &str;

    /// Opens the host device for `format`. `period_bytes` is the chunk size
    /// [`Self::write`] will be called with, so a sink can size its own buffer
    /// to match. Called on every stream PREPARE; a sink must tolerate being
    /// started again after [`Self::stop`].
    fn start(&mut self, format: StreamFormat, period_bytes: usize) -> Result<(), AudioError>;

    /// Plays `pcm` (interleaved S16LE), blocking for roughly as long as the
    /// audio lasts. Returns the bytes accepted; a short write is congestion,
    /// not failure, and the caller re-offers the rest.
    fn write(&mut self, pcm: &[u8]) -> Result<usize, AudioError>;

    /// Releases the host device. Idempotent; called on STOP, RELEASE, device
    /// reset and drop.
    fn stop(&mut self);
}

/// Turns "bytes accepted" into elapsed wall-clock time.
///
/// A software sink that returns immediately is not a sound card, it is a
/// `/dev/null` with a latency of zero — and a guest driving one sees its
/// buffer drain infinitely fast. The pacer sleeps until the audio it has been
/// handed *would* have finished, so the null sinks present the same timing
/// contract as hardware.
#[derive(Debug)]
pub struct Pacer {
    format: StreamFormat,
    origin: Instant,
    frames: u64,
}

/// How far behind schedule the pacer tolerates before it gives up catching up
/// and re-bases its clock. Without this, a host that was descheduled for a
/// second would then accept a second of audio at once, and every pending
/// playback message would complete in a burst.
const PACER_MAX_LAG: Duration = Duration::from_millis(500);

impl Pacer {
    pub fn new(format: StreamFormat) -> Self {
        Self {
            format,
            origin: Instant::now(),
            frames: 0,
        }
    }

    /// Accounts for `bytes` of audio and sleeps until they would have played.
    pub fn advance(&mut self, bytes: usize) {
        let frame_bytes = self.format.frame_bytes().max(1);
        self.frames = self.frames.saturating_add((bytes / frame_bytes) as u64);
        let played = Duration::from_nanos(
            self.frames
                .saturating_mul(1_000_000_000)
                .checked_div(u64::from(self.format.rate_hz.max(1)))
                .unwrap_or(0),
        );
        let elapsed = self.origin.elapsed();
        if let Some(ahead) = played.checked_sub(elapsed) {
            std::thread::sleep(ahead);
        } else if elapsed.saturating_sub(played) > PACER_MAX_LAG {
            // Too far behind to catch up honestly: start the clock again from
            // here rather than free-running through the backlog.
            self.origin = Instant::now();
            self.frames = 0;
        }
    }
}

/// A sink that keeps time but produces no sound.
///
/// The default on a host with no audio device, in headless runs and in every
/// test that does not care about the samples. It is deliberately *not* a
/// no-op: see [`Pacer`].
#[derive(Debug)]
pub struct NullSink {
    pacer: Option<Pacer>,
    paced: bool,
}

impl Default for NullSink {
    fn default() -> Self {
        Self {
            pacer: None,
            paced: true,
        }
    }
}

impl NullSink {
    /// A null sink that keeps time, like a sound card.
    pub fn new() -> Self {
        Self::default()
    }

    /// A null sink that accepts audio as fast as it is offered.
    ///
    /// For fuzzing and for tests about *what* happens rather than *when*.
    /// Never give one to a VM: the guest would see its buffer drain at
    /// infinite speed, which is the failure [`Pacer`] exists to prevent.
    pub fn unpaced() -> Self {
        Self {
            pacer: None,
            paced: false,
        }
    }
}

impl AudioSink for NullSink {
    fn name(&self) -> &str {
        "null"
    }

    fn start(&mut self, format: StreamFormat, _period_bytes: usize) -> Result<(), AudioError> {
        self.pacer = self.paced.then(|| Pacer::new(format));
        Ok(())
    }

    fn write(&mut self, pcm: &[u8]) -> Result<usize, AudioError> {
        if let Some(pacer) = self.pacer.as_mut() {
            pacer.advance(pcm.len());
        }
        Ok(pcm.len())
    }

    fn stop(&mut self) {
        self.pacer = None;
    }
}

/// What a [`RecordingSink`] captured, shared with whoever is watching.
#[derive(Debug, Default)]
pub struct Recording {
    inner: std::sync::Mutex<RecordingInner>,
}

#[derive(Debug, Default)]
struct RecordingInner {
    pcm: Vec<u8>,
    format: Option<StreamFormat>,
    starts: usize,
    stops: usize,
}

/// Bytes one recording keeps. 16 MiB is ~90 s of 48 kHz stereo — far more than
/// any test needs, and a hard stop so a recording sink left attached to a
/// long-running VM cannot grow without bound.
pub const MAX_RECORDING_BYTES: usize = 16 * 1024 * 1024;

impl Recording {
    /// Every byte handed to the sink, in order.
    pub fn pcm(&self) -> Vec<u8> {
        self.inner.lock().map(|r| r.pcm.clone()).unwrap_or_default()
    }

    /// Bytes captured so far, without copying them.
    pub fn len(&self) -> usize {
        self.inner.lock().map(|r| r.pcm.len()).unwrap_or(0)
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The format the last `start` asked for.
    pub fn format(&self) -> Option<StreamFormat> {
        self.inner.lock().ok().and_then(|r| r.format)
    }

    /// How many times the stream was started / stopped, which is how a test
    /// asserts the lifecycle reached the host and not only the state machine.
    pub fn starts(&self) -> usize {
        self.inner.lock().map(|r| r.starts).unwrap_or(0)
    }

    pub fn stops(&self) -> usize {
        self.inner.lock().map(|r| r.stops).unwrap_or(0)
    }

    /// Interprets the capture as interleaved S16LE samples.
    pub fn samples(&self) -> Vec<i16> {
        self.pcm()
            .chunks_exact(2)
            .map(|c| i16::from_le_bytes([c[0], c[1]]))
            .collect()
    }
}

/// A [`NullSink`] that also keeps what it was given.
///
/// Not test-only on purpose: "record what the guest played" is a genuinely
/// useful thing for a VMM to be able to do, and having one implementation
/// rather than a test double means the tests exercise the real pump path.
pub struct RecordingSink {
    recording: std::sync::Arc<Recording>,
    pacer: Option<Pacer>,
    paced: bool,
    truncated: bool,
}

impl std::fmt::Debug for RecordingSink {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RecordingSink")
            .field("captured", &self.recording.len())
            .field("paced", &self.paced)
            .finish_non_exhaustive()
    }
}

impl RecordingSink {
    /// A recording sink that keeps real time, like a sound card.
    pub fn new() -> (Self, std::sync::Arc<Recording>) {
        Self::build(true)
    }

    /// A recording sink that accepts audio as fast as it is offered.
    ///
    /// For tests about *what* arrives rather than *when* — with the pacing
    /// gone, a second of audio takes microseconds. Never use it as a VM's
    /// sink: the guest would see its buffer drain at infinite speed.
    pub fn unpaced() -> (Self, std::sync::Arc<Recording>) {
        Self::build(false)
    }

    /// Another sink writing into an existing recording — what a device's
    /// sink factory uses, so audio from before and after a device reset lands
    /// in the same capture.
    pub fn sharing(recording: std::sync::Arc<Recording>, paced: bool) -> Self {
        Self {
            recording,
            pacer: None,
            paced,
            truncated: false,
        }
    }

    fn build(paced: bool) -> (Self, std::sync::Arc<Recording>) {
        let recording = std::sync::Arc::new(Recording::default());
        (
            Self::sharing(std::sync::Arc::clone(&recording), paced),
            recording,
        )
    }
}

impl AudioSink for RecordingSink {
    fn name(&self) -> &str {
        "record"
    }

    fn start(&mut self, format: StreamFormat, _period_bytes: usize) -> Result<(), AudioError> {
        self.pacer = self.paced.then(|| Pacer::new(format));
        if let Ok(mut inner) = self.recording.inner.lock() {
            inner.format = Some(format);
            inner.starts += 1;
        }
        Ok(())
    }

    fn write(&mut self, pcm: &[u8]) -> Result<usize, AudioError> {
        if let Ok(mut inner) = self.recording.inner.lock() {
            let room = MAX_RECORDING_BYTES.saturating_sub(inner.pcm.len());
            let take = room.min(pcm.len());
            inner.pcm.extend_from_slice(&pcm[..take]);
            if take < pcm.len() && !self.truncated {
                self.truncated = true;
                tracing::warn!(
                    cap = MAX_RECORDING_BYTES,
                    "recording sink is full; further audio is played but not kept"
                );
            }
        }
        if let Some(pacer) = self.pacer.as_mut() {
            pacer.advance(pcm.len());
        }
        Ok(pcm.len())
    }

    fn stop(&mut self) {
        self.pacer = None;
        if let Ok(mut inner) = self.recording.inner.lock() {
            inner.stops += 1;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const STEREO_48K: StreamFormat = StreamFormat {
        rate_hz: 48000,
        channels: 2,
    };

    #[test]
    fn format_arithmetic_is_frame_based_and_never_divides_by_zero() {
        assert_eq!(STEREO_48K.frame_bytes(), 4);
        assert_eq!(STEREO_48K.duration_of(4 * 48000), Duration::from_secs(1));
        // Degenerate shapes cannot reach a sink, but must not panic if they do.
        let broken = StreamFormat {
            rate_hz: 0,
            channels: 0,
        };
        assert_eq!(broken.frame_bytes(), 0);
        assert_eq!(broken.duration_of(1024), Duration::ZERO);
    }

    /// The whole reason [`NullSink`] is not `Ok(len)` and nothing else.
    #[test]
    fn the_null_sink_takes_about_as_long_as_the_audio_lasts() {
        let mut sink = NullSink::new();
        sink.start(STEREO_48K, 4096).expect("null sink starts");
        let period = vec![0u8; 4 * 4800]; // 100 ms
        let began = Instant::now();
        for _ in 0..2 {
            assert_eq!(sink.write(&period).expect("write"), period.len());
        }
        let elapsed = began.elapsed();
        assert!(
            elapsed >= Duration::from_millis(150),
            "200 ms of audio played in {elapsed:?} — the sink is not pacing"
        );
    }

    #[test]
    fn a_pacer_that_falls_far_behind_rebases_instead_of_racing_to_catch_up() {
        let mut pacer = Pacer::new(STEREO_48K);
        // Pretend the host was descheduled for a long time.
        pacer.origin = Instant::now() - Duration::from_secs(30);
        let began = Instant::now();
        pacer.advance(4 * 480);
        assert!(
            began.elapsed() < Duration::from_millis(50),
            "the pacer tried to replay the backlog"
        );
        assert_eq!(pacer.frames, 0, "the clock was re-based");
    }

    #[test]
    fn the_recording_sink_keeps_what_it_is_given_and_reports_the_format() {
        let (mut sink, recording) = RecordingSink::unpaced();
        sink.start(STEREO_48K, 1024).expect("start");
        sink.write(&[1, 0, 2, 0]).expect("write");
        sink.write(&[3, 0, 4, 0]).expect("write");
        sink.stop();
        assert_eq!(recording.samples(), vec![1, 2, 3, 4]);
        assert_eq!(recording.format(), Some(STEREO_48K));
        assert_eq!((recording.starts(), recording.stops()), (1, 1));
    }

    /// A recording left running for hours must not become the host's memory
    /// problem.
    #[test]
    fn a_recording_is_capped_rather_than_unbounded() {
        let (mut sink, recording) = RecordingSink::unpaced();
        sink.start(STEREO_48K, 1024).expect("start");
        let chunk = vec![0u8; 1024 * 1024];
        for _ in 0..(MAX_RECORDING_BYTES / chunk.len()) + 4 {
            // Every write still *succeeds* — dropping the audio is a recording
            // concern, never a playback one.
            assert_eq!(sink.write(&chunk).expect("write"), chunk.len());
        }
        assert_eq!(recording.len(), MAX_RECORDING_BYTES);
    }
}
