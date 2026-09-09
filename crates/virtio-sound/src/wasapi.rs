//! The Windows playback sink and capture source: WASAPI in shared mode,
//! through Microsoft's `windows` crate (MIT OR Apache-2.0 — the licence
//! question that makes the Linux side interesting does not arise here).
//!
//! # Shared mode, and why no resampler
//!
//! A shared-mode `IAudioClient` normally insists on the audio engine's mix
//! format — almost always 32-bit float at 48 kHz — which would put a format
//! converter and a resampler on our side of the boundary. Windows 10 and later
//! will insert both itself when the stream is initialised with
//! `AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM | AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY`,
//! so we hand it the guest's own S16 PCM at the guest's own rate and let the
//! engine do the arithmetic. That keeps the guest-facing half of this crate
//! byte-identical on both hosts, which is the whole point of the sink trait.
//!
//! # Threading
//!
//! Everything here happens on the device's pump thread: the COM apartment is
//! entered there, the interfaces are created there, and they are released
//! there when the pump exits. That is why [`crate::SoundDevice`] builds its
//! sink from a factory *inside* the pump rather than handing one over — a COM
//! object created on one thread and released on another is a bug that only
//! shows up on someone else's machine.

use std::time::{Duration, Instant};

use windows::Win32::Media::Audio::{
    eCapture, eConsole, eRender, IAudioCaptureClient, IAudioClient, IAudioRenderClient,
    IMMDeviceEnumerator, MMDeviceEnumerator, AUDCLNT_SHAREMODE_SHARED, WAVEFORMATEX,
};
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoUninitialize, CLSCTX_ALL, COINIT_MULTITHREADED,
};

use crate::backend::{AudioError, AudioSink, AudioSource, StreamFormat};

/// `WAVE_FORMAT_PCM`.
const WAVE_FORMAT_PCM: u16 = 1;
/// `AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM`: let the audio engine convert our
/// format to the endpoint's.
const AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM: u32 = 0x8000_0000;
/// `AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY`: and resample at the default
/// quality while it is at it.
const AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY: u32 = 0x0800_0000;
/// 100-nanosecond units per second, the unit every WASAPI duration uses.
const REFTIMES_PER_SEC: i64 = 10_000_000;
/// Endpoint buffer we ask for: 100 ms, comfortably more than the pump's chunk.
const BUFFER_DURATION: i64 = REFTIMES_PER_SEC / 10;
/// `AUDCLNT_BUFFERFLAGS_SILENT`: the packet's memory is undefined and the
/// caller must treat it as silence.
const AUDCLNT_BUFFERFLAGS_SILENT: u32 = 0x2;

/// Owns this thread's COM apartment for as long as the sink lives.
struct ComGuard {
    /// True when *we* initialised it and therefore owe a `CoUninitialize`.
    owned: bool,
}

impl ComGuard {
    fn enter() -> Self {
        // SAFETY: `CoInitializeEx` with a null reserved pointer is the
        // documented way to join the multi-threaded apartment; it is safe to
        // call on any thread and reports its outcome in the HRESULT.
        let hr = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
        // S_OK and S_FALSE are both successes that must be balanced;
        // RPC_E_CHANGED_MODE means this thread is already in another
        // apartment, which is fine to use and must *not* be uninitialised.
        Self { owned: hr.is_ok() }
    }
}

impl Drop for ComGuard {
    fn drop(&mut self) {
        if self.owned {
            // SAFETY: balances exactly one successful `CoInitializeEx` made by
            // this guard, on this same thread (the sink never leaves it).
            unsafe { CoUninitialize() };
        }
    }
}

/// The `WAVEFORMATEX` for one of our S16LE streams.
fn wave_format(format: StreamFormat) -> WAVEFORMATEX {
    let block_align = u16::from(format.channels).saturating_mul(2);
    WAVEFORMATEX {
        wFormatTag: WAVE_FORMAT_PCM,
        nChannels: u16::from(format.channels),
        nSamplesPerSec: format.rate_hz,
        nAvgBytesPerSec: format.rate_hz.saturating_mul(u32::from(block_align)),
        nBlockAlign: block_align,
        wBitsPerSample: 16,
        cbSize: 0,
    }
}

/// Activates an `IAudioClient` on the default console endpoint for `capture`
/// or render. Shared by the sink, the source and both of their probes, so
/// "which endpoint" is decided in exactly one place.
fn default_endpoint(capture: bool) -> Result<IAudioClient, AudioError> {
    let what = if capture { "capture" } else { "render" };
    // SAFETY: standard COM activation of the MMDevice enumerator; the class id
    // is the one the `windows` crate generated for it and the returned
    // interface is checked by `CoCreateInstance` itself.
    let enumerator: IMMDeviceEnumerator =
        unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL) }
            .map_err(|e| AudioError::Unavailable(format!("MMDeviceEnumerator: {e}")))?;
    let flow = if capture { eCapture } else { eRender };
    // SAFETY: `enumerator` is a live interface pointer just obtained above.
    let device = unsafe { enumerator.GetDefaultAudioEndpoint(flow, eConsole) }
        .map_err(|e| AudioError::Unavailable(format!("no default {what} endpoint: {e}")))?;
    // SAFETY: `device` is live; `Activate` with no activation parameters is the
    // documented way to obtain an IAudioClient from an endpoint.
    unsafe { device.Activate(CLSCTX_ALL, None) }
        .map_err(|e| AudioError::Unavailable(format!("IAudioClient ({what}): {e}")))
}

/// A shared-mode WASAPI render stream on the default console endpoint.
pub struct WasapiSink {
    com: Option<ComGuard>,
    client: Option<IAudioClient>,
    render: Option<IAudioRenderClient>,
    buffer_frames: u32,
    frame_bytes: usize,
    /// How long to sleep when the endpoint buffer is full.
    wait: Duration,
}

// SAFETY: the COM interfaces and the apartment guard are created, used and
// released on one and the same thread — the device's pump thread. `Send` is
// only what allows the *empty* sink to be constructed elsewhere and moved
// there; nothing inside is touched until `start`, which runs on the pump.
unsafe impl Send for WasapiSink {}

impl std::fmt::Debug for WasapiSink {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WasapiSink")
            .field("open", &self.client.is_some())
            .field("buffer_frames", &self.buffer_frames)
            .finish_non_exhaustive()
    }
}

impl Default for WasapiSink {
    fn default() -> Self {
        Self::new()
    }
}

impl WasapiSink {
    /// An unopened sink. Cheap and infallible: the endpoint is only touched in
    /// [`AudioSink::start`], on the pump thread.
    pub fn new() -> Self {
        Self {
            com: None,
            client: None,
            render: None,
            buffer_frames: 0,
            frame_bytes: 4,
            wait: Duration::from_millis(5),
        }
    }

    /// Checks that this host has a usable default render endpoint, so an
    /// explicit `backend = "wasapi"` fails at VM start rather than silently
    /// playing into nothing.
    ///
    /// Enters and leaves a COM apartment on the calling thread; it creates no
    /// stream and holds nothing afterwards.
    pub fn probe() -> Result<String, AudioError> {
        let _com = ComGuard::enter();
        let _client = default_endpoint(false)?;
        Ok("wasapi".to_owned())
    }

    fn release(&mut self) {
        if let Some(client) = self.client.take() {
            // SAFETY: `client` is a live interface this sink initialised and
            // started; stopping and resetting an already-stopped client is
            // documented as harmless, and the results are only advisory here.
            unsafe {
                let _ = client.Stop();
                let _ = client.Reset();
            }
        }
        self.render = None;
        self.buffer_frames = 0;
    }
}

impl Drop for WasapiSink {
    fn drop(&mut self) {
        self.release();
        // The apartment goes last: the interfaces must be released inside it.
        self.com = None;
    }
}

impl AudioSink for WasapiSink {
    fn name(&self) -> &str {
        "wasapi"
    }

    fn start(&mut self, format: StreamFormat, _period_bytes: usize) -> Result<(), AudioError> {
        self.release();
        if self.com.is_none() {
            self.com = Some(ComGuard::enter());
        }
        self.frame_bytes = format.frame_bytes().max(1);

        let client = default_endpoint(false)?;
        let wfx = wave_format(format);
        // SAFETY: `client` is live and uninitialised; `&wfx` points at a
        // `WAVEFORMATEX` that outlives the call (Initialize copies it), and the
        // two conversion flags are what let a shared-mode client name a format
        // other than the engine's mix format.
        unsafe {
            client.Initialize(
                AUDCLNT_SHAREMODE_SHARED,
                AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM | AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY,
                BUFFER_DURATION,
                0,
                &wfx,
                None,
            )
        }
        .map_err(|e| AudioError::Format {
            rate_hz: format.rate_hz,
            channels: format.channels,
            reason: e.to_string(),
        })?;

        // SAFETY: `client` is initialised, which is what these three calls
        // require.
        let buffer_frames = unsafe { client.GetBufferSize() }
            .map_err(|e| AudioError::Io(format!("GetBufferSize: {e}")))?;
        // SAFETY: same.
        let render: IAudioRenderClient = unsafe { client.GetService() }
            .map_err(|e| AudioError::Io(format!("IAudioRenderClient: {e}")))?;
        // SAFETY: same.
        unsafe { client.Start() }.map_err(|e| AudioError::Io(format!("Start: {e}")))?;

        self.buffer_frames = buffer_frames;
        // Sleep for a quarter of the endpoint buffer when it is full: long
        // enough not to spin, short enough never to starve it.
        let quarter = u64::from(buffer_frames) * 250 / u64::from(format.rate_hz.max(1));
        self.wait = Duration::from_millis(quarter.clamp(1, 50));
        self.client = Some(client);
        self.render = Some(render);
        Ok(())
    }

    fn write(&mut self, pcm: &[u8]) -> Result<usize, AudioError> {
        let (Some(client), Some(render)) = (self.client.as_ref(), self.render.as_ref()) else {
            return Err(AudioError::Io("WASAPI stream is not open".into()));
        };
        let frame_bytes = self.frame_bytes.max(1);
        let total_frames = pcm.len() / frame_bytes;
        let mut done_frames = 0usize;
        // A stopped endpoint (an unplugged USB headset) must not wedge the
        // pump: give up after several times the audio's own duration.
        let deadline = Instant::now() + Duration::from_secs(2);

        while done_frames < total_frames {
            // SAFETY: `client` is a live, started client.
            let padding = unsafe { client.GetCurrentPadding() }
                .map_err(|e| AudioError::Io(format!("GetCurrentPadding: {e}")))?;
            let free = self.buffer_frames.saturating_sub(padding);
            if free == 0 {
                if Instant::now() >= deadline {
                    return Err(AudioError::Io(
                        "WASAPI endpoint stopped consuming audio".into(),
                    ));
                }
                std::thread::sleep(self.wait);
                continue;
            }
            let take = (free as usize).min(total_frames - done_frames);
            let Some(source) =
                pcm.get(done_frames * frame_bytes..(done_frames + take) * frame_bytes)
            else {
                break;
            };
            let take_u32 = u32::try_from(take).unwrap_or(u32::MAX);
            // SAFETY: `render` is live and belongs to `client`; `take_u32` is
            // at most the free frame count just read, which is what GetBuffer
            // requires.
            let buffer = unsafe { render.GetBuffer(take_u32) }
                .map_err(|e| AudioError::Io(format!("GetBuffer: {e}")))?;
            if buffer.is_null() {
                return Err(AudioError::Io("GetBuffer returned null".into()));
            }
            // SAFETY: `buffer` points at `take_u32 * nBlockAlign` writable
            // bytes owned by the endpoint until ReleaseBuffer, and `source` is
            // exactly that many bytes of host memory. The two cannot overlap:
            // one is our slice, the other the audio engine's ring.
            unsafe {
                std::ptr::copy_nonoverlapping(source.as_ptr(), buffer, source.len());
            }
            // SAFETY: releases exactly the frames just requested and filled.
            unsafe { render.ReleaseBuffer(take_u32, 0) }
                .map_err(|e| AudioError::Io(format!("ReleaseBuffer: {e}")))?;
            done_frames += take;
        }
        Ok(done_frames.saturating_mul(frame_bytes))
    }

    fn stop(&mut self) {
        self.release();
    }
}

// ------------------------------------------------------------ capture source

/// A shared-mode WASAPI capture stream on the default console microphone.
///
/// # Why there is a leftover buffer
///
/// `IAudioCaptureClient` is packet-oriented in a way the render client is not:
/// `GetBuffer` hands over a whole packet and `ReleaseBuffer` must then be told
/// that *all* of it was consumed (or none of it). The device asks for whatever
/// room the guest's next capture buffer has, which is not a packet size, so a
/// packet that does not fit is copied into [`Self::leftover`] and drained on
/// the following read. Holding a partial packet in the endpoint instead would
/// mean releasing zero frames and reading the same packet forever.
pub struct WasapiSource {
    com: Option<ComGuard>,
    client: Option<IAudioClient>,
    capture: Option<IAudioCaptureClient>,
    frame_bytes: usize,
    /// Bytes taken from the endpoint that did not fit the caller's buffer.
    leftover: Vec<u8>,
    /// How long to sleep when the endpoint has produced nothing yet.
    wait: Duration,
}

// SAFETY: as for `WasapiSink` — the COM interfaces and the apartment guard are
// created, used and released on one and the same thread (the device's capture
// pump). `Send` is only what allows the *empty* source to be constructed
// elsewhere and moved there; nothing inside is touched until `start`.
unsafe impl Send for WasapiSource {}

impl std::fmt::Debug for WasapiSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WasapiSource")
            .field("open", &self.client.is_some())
            .field("leftover", &self.leftover.len())
            .finish_non_exhaustive()
    }
}

impl Default for WasapiSource {
    fn default() -> Self {
        Self::new()
    }
}

impl WasapiSource {
    /// An unopened source. Cheap and infallible: the endpoint is only touched
    /// in [`AudioSource::start`], on the capture pump thread.
    pub fn new() -> Self {
        Self {
            com: None,
            client: None,
            capture: None,
            frame_bytes: 4,
            leftover: Vec::new(),
            wait: Duration::from_millis(5),
        }
    }

    /// Checks that this host has a usable default capture endpoint, so an
    /// explicit backend fails at VM start rather than recording silence.
    pub fn probe() -> Result<String, AudioError> {
        let _com = ComGuard::enter();
        let _client = default_endpoint(true)?;
        Ok("wasapi".to_owned())
    }

    fn release(&mut self) {
        if let Some(client) = self.client.take() {
            // SAFETY: `client` is a live interface this source initialised and
            // started; stopping and resetting an already-stopped client is
            // documented as harmless, and the results are only advisory here.
            unsafe {
                let _ = client.Stop();
                let _ = client.Reset();
            }
        }
        self.capture = None;
        self.leftover.clear();
    }

    /// Moves as much of `leftover` into `out` as fits, returning the bytes
    /// moved. Whole frames only — a partial frame would put the guest's
    /// channels out of step for the rest of the stream.
    fn drain_leftover(&mut self, out: &mut [u8]) -> usize {
        let frame_bytes = self.frame_bytes.max(1);
        let take = out.len().min(self.leftover.len()) / frame_bytes * frame_bytes;
        if take == 0 {
            return 0;
        }
        let (Some(dst), Some(src)) = (out.get_mut(..take), self.leftover.get(..take)) else {
            return 0;
        };
        dst.copy_from_slice(src);
        self.leftover.drain(..take);
        take
    }
}

impl Drop for WasapiSource {
    fn drop(&mut self) {
        self.release();
        // The apartment goes last: the interfaces must be released inside it.
        self.com = None;
    }
}

impl AudioSource for WasapiSource {
    fn name(&self) -> &str {
        "wasapi"
    }

    fn start(&mut self, format: StreamFormat, _period_bytes: usize) -> Result<(), AudioError> {
        self.release();
        if self.com.is_none() {
            self.com = Some(ComGuard::enter());
        }
        self.frame_bytes = format.frame_bytes().max(1);

        let client = default_endpoint(true)?;
        let wfx = wave_format(format);
        // SAFETY: `client` is live and uninitialised; `&wfx` points at a
        // `WAVEFORMATEX` that outlives the call (Initialize copies it), and the
        // two conversion flags are what let a shared-mode client name a format
        // other than the engine's mix format — for capture as for render.
        unsafe {
            client.Initialize(
                AUDCLNT_SHAREMODE_SHARED,
                AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM | AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY,
                BUFFER_DURATION,
                0,
                &wfx,
                None,
            )
        }
        .map_err(|e| AudioError::Format {
            rate_hz: format.rate_hz,
            channels: format.channels,
            reason: e.to_string(),
        })?;

        // SAFETY: `client` is initialised, which is what these three calls
        // require.
        let buffer_frames = unsafe { client.GetBufferSize() }
            .map_err(|e| AudioError::Io(format!("GetBufferSize: {e}")))?;
        // SAFETY: same.
        let capture: IAudioCaptureClient = unsafe { client.GetService() }
            .map_err(|e| AudioError::Io(format!("IAudioCaptureClient: {e}")))?;
        // SAFETY: same.
        unsafe { client.Start() }.map_err(|e| AudioError::Io(format!("Start: {e}")))?;

        // Sleep for a quarter of the endpoint buffer when it has nothing yet:
        // long enough not to spin, short enough never to fall behind.
        let quarter = u64::from(buffer_frames) * 250 / u64::from(format.rate_hz.max(1));
        self.wait = Duration::from_millis(quarter.clamp(1, 50));
        self.client = Some(client);
        self.capture = Some(capture);
        Ok(())
    }

    fn read(&mut self, out: &mut [u8]) -> Result<usize, AudioError> {
        if self.capture.is_none() {
            return Err(AudioError::Io("WASAPI capture stream is not open".into()));
        }
        let frame_bytes = self.frame_bytes.max(1);
        let want = out.len() / frame_bytes * frame_bytes;
        if want == 0 {
            return Ok(0);
        }
        let mut done = self.drain_leftover(out);
        // A stopped endpoint (an unplugged USB microphone) must not wedge the
        // pump: give up after several times the audio's own duration and let
        // the caller fall back to silence.
        let deadline = Instant::now() + Duration::from_secs(2);

        while done < want {
            let Some(capture) = self.capture.as_ref() else {
                break;
            };
            // SAFETY: `capture` is a live client belonging to a started
            // `IAudioClient`.
            let available = unsafe { capture.GetNextPacketSize() }
                .map_err(|e| AudioError::Io(format!("GetNextPacketSize: {e}")))?;
            if available == 0 {
                if Instant::now() >= deadline {
                    return Err(AudioError::Io(
                        "WASAPI endpoint stopped producing audio".into(),
                    ));
                }
                std::thread::sleep(self.wait);
                continue;
            }

            let mut data: *mut u8 = std::ptr::null_mut();
            let mut frames: u32 = 0;
            let mut flags: u32 = 0;
            // SAFETY: `capture` is live; the three out-parameters are live
            // locals the callee writes through. On success `data` points at
            // `frames * nBlockAlign` bytes owned by the endpoint until the
            // matching `ReleaseBuffer`.
            unsafe { capture.GetBuffer(&mut data, &mut frames, &mut flags, None, None) }
                .map_err(|e| AudioError::Io(format!("GetBuffer: {e}")))?;
            let packet_bytes = (frames as usize).saturating_mul(frame_bytes);
            let mut packet = vec![0u8; packet_bytes];
            if flags & AUDCLNT_BUFFERFLAGS_SILENT == 0 && !data.is_null() && packet_bytes > 0 {
                // SAFETY: `data` is non-null and, per the call above, points at
                // `frames * nBlockAlign` = `packet_bytes` readable bytes owned
                // by the endpoint until ReleaseBuffer, which has not run yet.
                // `packet` is a freshly allocated host buffer of exactly that
                // length, so the two cannot overlap.
                unsafe {
                    std::ptr::copy_nonoverlapping(data, packet.as_mut_ptr(), packet_bytes);
                }
            }
            // A silent packet stays zeroed — its memory is documented as
            // undefined, so it is never read.
            // SAFETY: releases exactly the frames just obtained, which is the
            // only value ReleaseBuffer accepts after a successful GetBuffer.
            unsafe { capture.ReleaseBuffer(frames) }
                .map_err(|e| AudioError::Io(format!("ReleaseBuffer: {e}")))?;

            let take = packet_bytes.min(want - done);
            if let (Some(dst), Some(src)) = (out.get_mut(done..done + take), packet.get(..take)) {
                dst.copy_from_slice(src);
                done += take;
            }
            if take < packet_bytes {
                // Keep the tail for the next read rather than dropping it.
                self.leftover.extend_from_slice(&packet[take..]);
            }
        }
        Ok(done)
    }

    fn stop(&mut self) {
        self.release();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Self-skipping: a CI container (and a Windows box with no audio driver)
    /// has no render endpoint, and that must not be a failure.
    #[test]
    fn the_default_render_endpoint_is_reachable_when_the_host_has_one() {
        match WasapiSink::probe() {
            Ok(name) => assert_eq!(name, "wasapi"),
            Err(error) => eprintln!("skipping: {error}"),
        }
    }

    /// The same, for the microphone. A machine with speakers and no
    /// microphone is completely ordinary, so this skips more often than its
    /// render twin.
    #[test]
    fn the_default_capture_endpoint_is_reachable_when_the_host_has_one() {
        match WasapiSource::probe() {
            Ok(name) => assert_eq!(name, "wasapi"),
            Err(error) => eprintln!("skipping: {error}"),
        }
    }

    /// A source that was never started must refuse a read rather than
    /// dereference a null interface.
    #[test]
    fn reading_from_an_unopened_source_is_an_error_not_a_crash() {
        let mut source = WasapiSource::new();
        assert!(source.read(&mut [0u8; 64]).is_err());
        source.stop();
    }

    /// The leftover buffer only ever hands back whole frames, however awkward
    /// the caller's room is.
    #[test]
    fn the_leftover_buffer_hands_back_whole_frames_only() {
        let mut source = WasapiSource::new();
        source.frame_bytes = 4;
        source.leftover = vec![1, 2, 3, 4, 5, 6, 7, 8];
        let mut out = [0u8; 7];
        assert_eq!(source.drain_leftover(&mut out), 4);
        assert_eq!(&out[..4], &[1, 2, 3, 4]);
        assert_eq!(source.leftover, vec![5, 6, 7, 8]);
        // Room for less than one frame moves nothing.
        let mut tiny = [0u8; 3];
        assert_eq!(source.drain_leftover(&mut tiny), 0);
        assert_eq!(source.leftover.len(), 4);
    }

    /// A sink that was never started must refuse a write rather than
    /// dereference a null interface.
    #[test]
    fn writing_to_an_unopened_sink_is_an_error_not_a_crash() {
        let mut sink = WasapiSink::new();
        assert!(sink.write(&[0u8; 64]).is_err());
        sink.stop();
    }
}
